use std::{
    io,
    net::{SocketAddr, TcpListener, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread::JoinHandle,
    time::Duration,
};

use tracing::{error, info, warn};

use crate::admin::{AdminContext, http, router};

/// 同时处理的连接上限，超出直接返回 503，避免面板被慢连接拖垮。
const MAX_CONNECTIONS: usize = 32;
/// 非阻塞 accept 的轮询间隔，同时决定停止响应的最大延迟。
const ACCEPT_INTERVAL: Duration = Duration::from_millis(80);

/// Web 面板线程句柄。
pub struct WebHandle {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl WebHandle {
    /// 通知监听线程退出并等待其结束。
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// 绑定端口并启动监听线程。
pub fn start(ctx: Arc<AdminContext>) -> Result<WebHandle, String> {
    let listen = ctx.listen.clone();
    let addr: SocketAddr = listen
        .parse()
        .map_err(|_| format!("Web 面板监听地址无效: {listen}（需为 ip:port）"))?;

    // 安全兜底：未设置密码的面板只能监听回环地址。
    //
    // 配置校验只会在启动时给出警告，不能作为唯一防线；这里做硬性拦截，避免
    // 把包含 AList 令牌与媒体服务器 API Key 的配置接口暴露到网络上。
    if !ctx.auth.enabled() && !addr.ip().is_loopback() {
        return Err(format!(
            "Web 面板监听 {addr} 但未设置访问密码，已拒绝启动；请设置 web.password，或改为回环地址（例如 127.0.0.1:8080）"
        ));
    }

    let listener =
        TcpListener::bind(addr).map_err(|err| format!("Web 面板监听 {addr} 失败: {err}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|err| format!("设置 Web 面板非阻塞监听失败: {err}"))?;

    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let thread = std::thread::Builder::new()
        .name("autofilm-web".to_string())
        .spawn(move || accept_loop(listener, addr, ctx, thread_stop))
        .map_err(|err| format!("启动 Web 面板线程失败: {err}"))?;

    Ok(WebHandle {
        stop,
        thread: Some(thread),
    })
}

fn accept_loop(
    listener: TcpListener,
    addr: SocketAddr,
    ctx: Arc<AdminContext>,
    stop: Arc<AtomicBool>,
) {
    let active = Arc::new(AtomicUsize::new(0));
    info!(listen = %addr, "Web 管理面板开始监听");

    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, peer)) => {
                if !try_reserve(&active) {
                    let mut stream = stream;
                    // 写超时防止慢客户端把 accept 线程卡住。
                    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
                    let _ = http::Response::text(503, "服务繁忙，请稍后重试").write_to(&mut stream);
                    continue;
                }

                let ctx = ctx.clone();
                let counter = active.clone();
                let spawned = std::thread::Builder::new()
                    .name("autofilm-web-conn".to_string())
                    .spawn(move || {
                        if let Err(err) = handle_connection(stream, peer, &ctx) {
                            warn!(peer = %peer, error = %err, "Web 请求处理失败");
                        }
                        counter.fetch_sub(1, Ordering::AcqRel);
                    });

                if let Err(err) = spawned {
                    active.fetch_sub(1, Ordering::AcqRel);
                    error!(error = %err, "创建 Web 连接处理线程失败");
                }
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(ACCEPT_INTERVAL);
            }
            Err(err) => {
                warn!(error = %err, "Web 面板接受连接失败");
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }

    info!("Web 管理面板已停止监听");
}

/// 原子地占用一个连接名额，避免「先检查后累加」导致并发超限。
fn try_reserve(active: &AtomicUsize) -> bool {
    let mut current = active.load(Ordering::Acquire);
    loop {
        if current >= MAX_CONNECTIONS {
            return false;
        }
        match active.compare_exchange_weak(
            current,
            current + 1,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return true,
            Err(actual) => current = actual,
        }
    }
}

fn handle_connection(
    mut stream: TcpStream,
    peer: SocketAddr,
    ctx: &AdminContext,
) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(30)))?;

    let Some(request) = http::read_request(&mut stream, &peer.ip().to_string())? else {
        return Ok(());
    };
    let response = router::dispatch(ctx, &request);
    response.write_to(&mut stream)
}
