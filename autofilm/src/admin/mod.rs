//! Web 管理面板与配置热重载。
//!
//! 该模块提供三件事：
//! 1. 一个零外部依赖的 HTTP 服务（基于 `std::net`），对外提供配置读写 API；
//! 2. 配置热重载内核：校验 → 写盘 → 重建调度器 → 切换，失败时保持旧配置继续运行；
//! 3. 配置文件监听线程，手工编辑 YAML 后同样可以即时生效。
//!
//! 两份配置都**实时生效**，没有任何字段需要重启进程：
//! - 业务配置（`config.yaml`）：重建调度器，运行中任务持有旧快照继续跑完；
//! - 面板配置（`web.yaml`）：认证信息动态读取，监听地址热重绑，`enable` 与
//!   `watch_file` 动态启停，全部在保存的同一事务里完成。
//!
//! 两条通道各有独立的 [`ConfigStore`] / [`WebStore`]、独立的版本号与独立的写盘
//! 路径，因此面板保存业务配置不会重写 `web.yaml`，反之亦然。
//!
//! 为避免额外依赖，HTTP 层只实现管理面板所需的最小子集：`GET`/`PUT`/`POST`、
//! `Content-Length` 定长请求体、短连接（`Connection: close`）。

mod assets;
pub mod auth;
mod http;
mod lock;
mod router;
mod schema;
mod server;
mod store;

use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc as std_mpsc,
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

pub use auth::AuthConfig;
use chrono::Local;
pub use server::WebHandle;
pub use store::{ConfigStore, Persist, Snapshot, Store, WebStore};
use tokio::sync::{mpsc, oneshot};
use tokio_cron_scheduler::JobScheduler;
use tracing::{error, info, warn};

use crate::{
    config::{Config, ValidationIssue},
    mediawarp::Supervisor,
    schedule,
    web_config::WebConfig,
};

/// 配置文件外部改动检测间隔。
const WATCH_INTERVAL: Duration = Duration::from_secs(2);

/// 等待一次热重载完成的最长时间，超时后连接线程放弃等待并返回 503。
///
/// 业务配置保存现在可能连带一次 MediaWarp 重启（优雅停止 5s + 启动健康检查
/// 10s，均可在面板里调整），因此留出 45s 余量，避免慢一点就误报 503。
pub(crate) const APPLY_TIMEOUT: Duration = Duration::from_secs(45);

/// 热重载的触发来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReloadSource {
    /// 管理面板保存。
    Panel,
    /// 配置文件被外部修改。
    File,
    /// 通过 `POST /api/reload` 手工重读文件（供自动化脚本使用）。
    Manual,
}

impl ReloadSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Panel => "panel",
            Self::File => "file",
            Self::Manual => "manual",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Panel => "管理面板保存",
            Self::File => "配置文件变更",
            Self::Manual => "手工重新加载",
        }
    }
}

/// 热重载结果。
#[derive(Debug)]
pub enum ApplyOutcome {
    Applied(ApplyReport),
    Rejected(Vec<ValidationIssue>),
    /// 本次提交改动了正在运行的任务配置，整次拒绝（任务键列表）。
    Locked(Vec<String>),
    Failed(String),
}

/// 一次成功保存的摘要，用于回传前端展示。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ApplyReport {
    pub version: u64,
    pub scheduled_count: usize,
    pub duration_ms: u64,
    pub source: String,
    /// 本次保存作用于哪份配置：`business` 或 `web`。
    pub scope: &'static str,
    pub warnings: Vec<String>,
    /// 成功但值得告知用户的信息（例如「已自动重启 MediaWarp」），前端按成功提示展示。
    pub notes: Vec<String>,
    /// 因正在运行而被保护（保留旧配置）的任务键。
    pub locked_tasks: Vec<String>,
}

/// 面板展示的运行时状态。
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct RuntimeStatus {
    pub scheduled_count: usize,
    pub reload_count: u64,
    pub last_reload_at: Option<String>,
    pub last_reload_source: Option<String>,
    pub last_error: Option<String>,
}

/// 面板共享状态。
pub struct AdminContext {
    /// 业务配置存储（`config.yaml`）。
    pub store: Arc<ConfigStore>,
    /// 面板配置存储（`web.yaml`）。
    pub web_store: Arc<WebStore>,
    /// 当前面板监听句柄；保存 `web.yaml` 时按新配置热重绑。
    pub web: Mutex<Option<WebHandle>>,
    /// MediaWarp 反向代理子进程的托管句柄；业务配置保存时按新配置热重启。
    pub mediawarp: Arc<Supervisor>,
    pub guard: auth::LoginGuard,
    /// 登录页签发/校验的会话表（落盘在 web.yaml 同目录，重启后仍有效；
    /// 用户名或密码变更时立即作废）。
    pub sessions: auth::SessionStore,
    pub schema: serde_json::Value,
    pub commands: mpsc::Sender<AdminCommand>,
    pub status: Mutex<RuntimeStatus>,
    /// 当前调度器构建出的任务执行入口，供面板「立即运行」使用。
    pub runners: Mutex<schedule::RunRegistry>,
    /// 当前已登录的 AList 客户端，供面板在编辑配置时浏览远端目录。
    pub alist_clients: Mutex<std::collections::HashMap<String, (Arc<alist::Client>, String)>>,
    /// 调度时区，用于计算面板展示的「下次运行」时间。
    pub tz: chrono_tz::Tz,
    /// 运行时句柄：手动触发的任务交给 tokio 运行时执行，不占用 HTTP 连接线程。
    pub handle: tokio::runtime::Handle,
    pub config_path: PathBuf,
    pub web_config_path: PathBuf,
    pub timezone: String,
    pub started_at: String,
}

impl AdminContext {
    /// 当前生效的面板配置。所有服务级设置都从这里实时读取，不再缓存启动快照。
    pub fn web_config(&self) -> WebConfig {
        self.web_store.snapshot().config
    }

    /// 当前生效的认证凭据。用户名/密码改动后立即生效。
    pub fn auth(&self) -> AuthConfig {
        let web = self.web_config();
        AuthConfig::new(web.username, web.password)
    }

    /// 面板当前是否正在提供 HTTP 服务。
    pub fn web_enabled(&self) -> bool {
        self.web
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .is_some()
    }

    /// 以 JSON 形式返回运行时状态。
    pub fn status_json(&self) -> serde_json::Value {
        // 锁中毒时继续使用内部数据，避免面板因为一次 panic 永久不可用。
        let status = match self.status.lock() {
            Ok(status) => status.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        serde_json::to_value(status).unwrap_or(serde_json::Value::Null)
    }

    /// 每个任务的下次运行时间，键为 `模块:任务 ID`。
    ///
    /// 解析规则与调度器一致（秒必填、日与周同时生效、使用配置时区），因此面板
    /// 展示的时间就是调度器真正触发的时间；cron 缺失或非法时不返回该键。
    pub fn next_runs_json(&self) -> serde_json::Value {
        // 只读锁内直接遍历任务列表，不克隆整份业务配置。
        let entries = self.store.with_config(|config| {
            let mut entries: Vec<(String, Option<String>)> = Vec::new();
            for task in &config.alist2strm_tasks {
                entries.push((
                    format!("{}:{}", schedule::MODULE_ALIST2STRM, task.id),
                    task.cron.clone(),
                ));
            }
            for task in &config.ani2alist_tasks {
                entries.push((
                    format!("{}:{}", schedule::MODULE_ANI2ALIST, task.id),
                    task.cron.clone(),
                ));
            }
            for task in &config.library_poster_tasks {
                entries.push((
                    format!("{}:{}", schedule::MODULE_LIBRARY_POSTER, task.id),
                    task.cron.clone(),
                ));
            }
            entries
        });

        let mut map = serde_json::Map::new();
        for (key, cron) in entries {
            let Some(cron) = cron else { continue };
            let Some(next) = schedule::next_run_at(&cron, self.tz) else {
                continue;
            };
            map.insert(
                key,
                serde_json::Value::String(next.format("%Y-%m-%d %H:%M:%S").to_string()),
            );
        }
        serde_json::Value::Object(map)
    }

    /// 正在运行的任务键集合，供面板展示「运行中」并禁用重复触发。
    pub fn running_json(&self) -> serde_json::Value {
        serde_json::to_value(schedule::running_tasks()).unwrap_or(serde_json::Value::Null)
    }

    /// 面板需要的运行时数据（状态 + 下次运行 + 运行中），供轮询接口与首屏复用。
    pub fn runtime_json(&self) -> serde_json::Value {
        let mut runtime = self.status_json();
        if let serde_json::Value::Object(map) = &mut runtime {
            map.insert("next_runs".to_string(), self.next_runs_json());
            map.insert("running".to_string(), self.running_json());
            // 服务状态随 3 秒轮询一起下发，MediaWarp 页的状态胶囊就地刷新，无需单独接口。
            map.insert("mediawarp".to_string(), self.mediawarp.status_json());
        }
        runtime
    }
}

/// 发送给热重载内核的命令。
pub enum AdminCommand {
    Apply {
        config: Config,
        source: ReloadSource,
        /// 用标准库通道回传结果，方便连接线程带超时等待，避免面板被卡死。
        reply: std_mpsc::SyncSender<ApplyOutcome>,
    },
    /// 只更新面板配置：只写 `web.yaml`，不重建调度器。
    ApplyWeb {
        web: WebConfig,
        source: ReloadSource,
        reply: std_mpsc::SyncSender<ApplyOutcome>,
    },
    /// 面板「立即重启」：按当前已生效的配置强制重启 MediaWarp。
    ///
    /// 与 Apply 共用同一条串行命令通道，因此不会与配置保存同时抢端口。
    MediaWarpRestart {
        reply: std_mpsc::SyncSender<Result<String, String>>,
    },
    Shutdown {
        reply: oneshot::Sender<()>,
    },
}

/// 管理面板与热重载内核的生命周期句柄。
pub struct Runtime {
    pub ctx: Arc<AdminContext>,
    watcher: Option<WatcherHandle>,
    supervisor: Option<tokio::task::JoinHandle<()>>,
}

impl Runtime {
    /// 启动调度内核、配置监听与（可选的）Web 面板。
    ///
    /// 返回值中的计数是首次调度注册成功的任务数量，供调用方决定是否需要
    /// 在无任务且未开启面板时提前退出。
    pub async fn start(
        config_path: PathBuf,
        config: Config,
        web_config_path: PathBuf,
        web_config: WebConfig,
        tz: chrono_tz::Tz,
    ) -> Result<(Self, usize), String> {
        let scheduled = schedule::create_scheduler(config.clone(), tz)
            .await
            .map_err(|err| format!("初始化调度器失败: {err}"))?;
        let (mut scheduler, scheduled_count) = (scheduled.scheduler, scheduled.scheduled_count);
        let (runners, alist_clients) = (scheduled.runners, scheduled.alist_clients);
        scheduler
            .start()
            .await
            .map_err(|err| format!("启动调度器失败: {err}"))?;

        let store = Arc::new(ConfigStore::new(config_path.clone(), config));
        let web_store = Arc::new(WebStore::new(web_config_path.clone(), web_config.clone()));
        let (command_tx, command_rx) = mpsc::channel::<AdminCommand>(16);

        // MediaWarp 生成文件与 config.yaml 同目录：本地是 config/mediawarp.yaml，
        // 容器里 config.yaml 位于 /config，因此自然落到 /config/mediawarp.yaml。
        let mediawarp_base = config_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(std::path::Path::new("."))
            .to_path_buf();
        let mediawarp = Arc::new(Supervisor::new(mediawarp_base));

        // 会话表与会话文件都要在启动时按当前凭据初始化：文件里的旧会话只有在
        // 用户名/密码未变时才继续有效，这样「改密码 = 全部踢下线」在停机期间
        // 改配置的场景下同样成立。
        let sessions = auth::SessionStore::load(
            Some(auth::SessionStore::path_for(web_config_path.as_path())),
            &auth::AuthConfig::new(web_config.username.clone(), web_config.password.clone()),
        );

        let ctx = Arc::new(AdminContext {
            store,
            web_store,
            web: Mutex::new(None),
            mediawarp: mediawarp.clone(),
            guard: auth::LoginGuard::default(),
            sessions,
            schema: schema::schema(),
            commands: command_tx,
            status: Mutex::new(RuntimeStatus {
                scheduled_count,
                ..RuntimeStatus::default()
            }),
            runners: Mutex::new(runners),
            alist_clients: Mutex::new(alist_clients),
            tz,
            handle: tokio::runtime::Handle::current(),
            config_path,
            web_config_path,
            timezone: tz.to_string(),
            started_at: now_rfc3339(),
        });

        // 面板先绑定端口：监听失败时直接终止启动，避免留下无人接管的调度器。
        if web_config.enable {
            match server::start(ctx.clone()) {
                Ok(handle) => {
                    info!(
                        listen = %web_config.listen,
                        auth = !web_config.password.is_empty(),
                        "Web 管理面板已启动"
                    );
                    *ctx.web.lock().unwrap_or_else(|err| err.into_inner()) = Some(handle);
                }
                Err(err) => {
                    let _ = scheduler.shutdown().await;
                    return Err(err);
                }
            }
        } else {
            info!("Web 管理面板未启用（web.enable = false）");
        }

        // 按 config.yaml 的 mediawarp 段生成配置文件并拉起代理子进程。
        // 这里失败只告警、不终止启动：否则一个写错的二进制路径会让用户连面板
        // 都打不开，反而没法修。状态会留在 failed 并在面板上给出原因。
        let initial_mediawarp = ctx.store.snapshot().config.mediawarp;
        match mediawarp.bootstrap(&initial_mediawarp).await {
            Ok(report) if report.changed => info!(note = %report.note, "MediaWarp 托管已初始化"),
            Ok(_) => info!("MediaWarp 配置未变化，跳过初始化重启"),
            Err(err) => warn!(error = %err, "MediaWarp 初始化失败，可在面板修正后保存重试"),
        }

        let supervisor = tokio::spawn(supervise(ctx.clone(), command_rx, Some(scheduler), tz));

        // 监听线程常驻，由 watch_loop 每轮读取实时 watch_file 决定是否检测，
        // 这样开关本身也能即时生效，不必重启线程。
        let watcher_ctx = ctx.clone();
        let runtime = Self {
            ctx,
            watcher: Some(spawn_watcher(watcher_ctx)),
            supervisor: Some(supervisor),
        };

        Ok((runtime, scheduled_count))
    }

    /// 是否启用了 Web 面板。
    pub fn web_enabled(&self) -> bool {
        self.ctx.web_enabled()
    }

    /// 是否启用了 MediaWarp 托管（决定「没有调度任务」时能否提前退出）。
    pub fn mediawarp_enabled(&self) -> bool {
        self.ctx.mediawarp.managed()
    }

    /// 停止面板、MediaWarp 子进程、监听线程与调度器。
    pub async fn shutdown(&mut self) {
        if let Some(mut watcher) = self.watcher.take() {
            watcher.stop();
        }
        let web = self
            .ctx
            .web
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .take();
        if let Some(mut handle) = web {
            handle.stop();
        }
        // 先带走子进程再退出，避免容器里留下占用 9000 端口的孤儿进程。
        self.ctx.mediawarp.shutdown().await;

        let (reply, receiver) = oneshot::channel();
        if self
            .ctx
            .commands
            .send(AdminCommand::Shutdown { reply })
            .await
            .is_ok()
        {
            let _ = receiver.await;
        }
        if let Some(handle) = self.supervisor.take() {
            let _ = handle.await;
        }
    }
}

/// 热重载内核：串行处理配置应用与关闭命令。
async fn supervise(
    ctx: Arc<AdminContext>,
    mut commands: mpsc::Receiver<AdminCommand>,
    mut current: Option<JobScheduler>,
    tz: chrono_tz::Tz,
) {
    while let Some(command) = commands.recv().await {
        match command {
            AdminCommand::Apply {
                config,
                source,
                reply,
            } => {
                let outcome = apply(&ctx, &mut current, tz, config, source).await;
                let _ = reply.send(outcome);
            }
            AdminCommand::ApplyWeb { web, source, reply } => {
                let outcome = apply_web(&ctx, web, source);
                let _ = reply.send(outcome);
            }
            AdminCommand::MediaWarpRestart { reply } => {
                let config = ctx.store.snapshot().config.mediawarp;
                let outcome = ctx.mediawarp.restart_now(&config).await;
                let _ = reply.send(outcome.map(|report| report.note));
            }
            AdminCommand::Shutdown { reply } => {
                if let Some(mut scheduler) = current.take() {
                    if let Err(err) = scheduler.shutdown().await {
                        warn!(error = %err, "关闭调度器失败");
                    }
                }
                let _ = reply.send(());
                return;
            }
        }
    }

    // 命令通道关闭（进程退出路径）时同样需要释放调度器。
    if let Some(mut scheduler) = current.take() {
        let _ = scheduler.shutdown().await;
    }
}

/// 应用一份新的业务配置。
///
/// 顺序为：静态校验 → 构建并启动新调度器 → 写盘 → 切换 → 停旧调度器。
/// 任何一步失败都会保留原调度器与原配置继续运行，并把错误回传前端。
async fn apply(
    ctx: &Arc<AdminContext>,
    current: &mut Option<JobScheduler>,
    tz: chrono_tz::Tz,
    config: Config,
    source: ReloadSource,
) -> ApplyOutcome {
    let started = Instant::now();

    // 运行中任务的配置保护：面板提交整次拒绝，外部编辑只回滚被锁的那条任务，
    // 其余改动照常生效。只比较任务条目，全局策略与客户端配置不受限制。
    let running = schedule::running_tasks();
    let current_value = serde_json::to_value(&ctx.store.snapshot().config);
    let next_value = serde_json::to_value(&config);
    let mut locked_tasks = Vec::new();
    let config = if running.is_empty() {
        config
    } else {
        match (current_value, next_value) {
            (Ok(current), Ok(next)) if source == ReloadSource::Panel => {
                let changed = lock::changed_locked(&current, &next, &running);
                if !changed.is_empty() {
                    warn!(tasks = ?changed, "提交改动了运行中任务的配置，已拒绝");
                    return ApplyOutcome::Locked(changed);
                }
                config
            }
            (Ok(current), Ok(mut next)) => {
                locked_tasks = lock::preserve_locked(&current, &mut next, &running);
                match serde_json::from_value::<Config>(next) {
                    Ok(restored) => restored,
                    Err(err) => {
                        let message = format!("恢复运行中任务配置失败: {err}");
                        record_failure(ctx, message.clone(), source);
                        return ApplyOutcome::Failed(message);
                    }
                }
            }
            _ => {
                warn!("配置序列化失败，本次跳过运行中任务保护");
                config
            }
        }
    };

    let issues = config.validate();
    if !issues.is_empty() {
        let message = format!("配置校验未通过（{} 处问题）", issues.len());
        record_failure(ctx, message, source);
        return ApplyOutcome::Rejected(issues);
    }

    let scheduled = match schedule::create_scheduler(config.clone(), tz).await {
        Ok(result) => result,
        Err(err) => {
            let message = format!("构建调度任务失败: {err}");
            record_failure(ctx, message.clone(), source);
            return ApplyOutcome::Failed(message);
        }
    };
    let (mut scheduler, scheduled_count) = (scheduled.scheduler, scheduled.scheduled_count);
    let (runners_next, alist_clients_next) = (scheduled.runners, scheduled.alist_clients);

    if let Err(err) = scheduler.start().await {
        let message = format!("启动新调度器失败: {err}");
        record_failure(ctx, message.clone(), source);
        return ApplyOutcome::Failed(message);
    }

    // MediaWarp 事务放在写 config.yaml 之前：代理服务先完成「原子写生成文件 →
    // 优雅停旧 → 拉起新进程 → 健康检查」，失败时内部已自行回滚，这里只需丢弃
    // 尚未启用的新调度器；两份配置都不会留下半成品。
    let mut mediawarp_note: Option<String> = None;
    match ctx.mediawarp.commit(&config.mediawarp).await {
        Ok(report) if report.changed => mediawarp_note = Some(report.note),
        Ok(_) => {}
        Err(message) => {
            let _ = scheduler.shutdown().await;
            record_failure(ctx, message.clone(), source);
            return ApplyOutcome::Failed(message);
        }
    }

    // 只有面板保存才写盘；外部编辑与手工重新加载只采用磁盘内容，
    // 避免把用户写的注释与排版抹掉。
    let version = if source == ReloadSource::Panel {
        match ctx.store.apply(config) {
            Ok(version) => version,
            Err(err) => {
                let message = format!("写入业务配置文件失败: {err}");
                let _ = scheduler.shutdown().await;
                // 生成文件与代理进程已经切换，这里必须一并回滚。
                ctx.mediawarp.undo().await;
                record_failure(ctx, message.clone(), source);
                return ApplyOutcome::Failed(message);
            }
        }
    } else {
        ctx.store.adopt(config)
    };

    // config.yaml 已落盘，整次事务完成，丢弃 MediaWarp 的回滚现场。
    ctx.mediawarp.finish().await;

    // 新调度器已经就绪后再停掉旧的；正在执行的任务不会被中断，
    // 它们持有的是启动时的配置快照，下一次触发才会使用新配置。
    if let Some(mut previous_scheduler) = current.replace(scheduler) {
        if let Err(err) = previous_scheduler.shutdown().await {
            warn!(error = %err, "关闭旧调度器时出错");
        }
    }

    // 手动运行入口与 AList 客户端同步替换，避免面板触发到已经下线的旧任务，
    // 或按旧凭据浏览目录。
    {
        let mut registry = ctx.runners.lock().unwrap_or_else(|err| err.into_inner());
        *registry = runners_next;
    }
    {
        let mut clients = ctx
            .alist_clients
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        *clients = alist_clients_next;
    }

    let warnings = if locked_tasks.is_empty() {
        Vec::new()
    } else {
        vec![format!(
            "任务 {} 正在运行，其配置已保留为运行开始时的值；请等任务结束后再修改",
            locked_tasks.join("、")
        )]
    };
    let notes = mediawarp_note.into_iter().collect::<Vec<_>>();
    let report = ApplyReport {
        version,
        scheduled_count,
        duration_ms: started.elapsed().as_millis() as u64,
        source: source.as_str().to_string(),
        scope: "business",
        warnings,
        notes,
        locked_tasks,
    };

    {
        let mut status = ctx.status.lock().unwrap_or_else(|err| err.into_inner());
        status.scheduled_count = scheduled_count;
        status.reload_count += 1;
        status.last_reload_at = Some(now_rfc3339());
        status.last_reload_source = Some(source.label().to_string());
        status.last_error = None;
    }

    info!(
        version,
        scheduled_count,
        source = source.as_str(),
        duration_ms = report.duration_ms,
        "业务配置已热重载"
    );

    ApplyOutcome::Applied(report)
}

/// 面板监听的切换计划。
enum ServicePlan {
    /// 地址未变，服务保持不动（认证等字段由请求链路动态读取）。
    Keep,
    /// 关闭面板。
    Stop,
    /// 用已预绑定的监听器重绑到新地址。
    Switch {
        listener: std::net::TcpListener,
        listen: String,
    },
}

/// 应用一份新的面板配置：校验 → 预绑定 → 切换监听 → 写盘 → 收尾。
///
/// 所有字段都实时生效，无需重启进程。预绑定放在写盘之前，新地址不可用就整次
/// 回滚、旧监听保持可用；写盘失败同样把监听现场恢复回去。认证信息由请求链路
/// 动态读取，改密码时顺带清空旧会话。
fn apply_web(ctx: &Arc<AdminContext>, web: WebConfig, source: ReloadSource) -> ApplyOutcome {
    let started = Instant::now();

    let issues = web.validate();
    if !issues.is_empty() {
        let message = format!("面板配置校验未通过（{} 处问题）", issues.len());
        record_failure(ctx, message, source);
        return ApplyOutcome::Rejected(issues);
    }

    let previous = ctx.web_config();
    let notes = previous.realtime_notes(&web);

    let plan = if web.enable {
        let current = ctx
            .web
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .as_ref()
            .map(|handle| handle.listen.clone());
        if current.as_deref() == Some(web.listen.as_str()) {
            ServicePlan::Keep
        } else {
            match server::bind(&web.listen, !web.password.is_empty()) {
                Ok(listener) => ServicePlan::Switch {
                    listener,
                    listen: web.listen.clone(),
                },
                Err(err) => {
                    let message = format!("切换面板监听失败: {err}");
                    record_failure(ctx, message.clone(), source);
                    return ApplyOutcome::Failed(message);
                }
            }
        }
    } else if ctx.web_enabled() {
        ServicePlan::Stop
    } else {
        ServicePlan::Keep
    };

    let service_changed = !matches!(plan, ServicePlan::Keep);
    // 新监听就绪后才撤掉旧的，面板不会出现空窗；失败的分支不写盘。
    let previous_handle = {
        let mut slot = ctx.web.lock().unwrap_or_else(|err| err.into_inner());
        match plan {
            ServicePlan::Keep => None,
            ServicePlan::Stop => slot.take(),
            ServicePlan::Switch { listener, listen } => {
                match server::serve(ctx.clone(), listener, listen.clone()) {
                    Ok(handle) => {
                        let old = slot.replace(handle);
                        info!(listen = %listen, "Web 管理面板监听地址已热切换");
                        old
                    }
                    Err(err) => {
                        let message = format!("启动新的面板监听失败: {err}");
                        record_failure(ctx, message.clone(), source);
                        return ApplyOutcome::Failed(message);
                    }
                }
            }
        }
    };

    let saved = if source == ReloadSource::Panel {
        ctx.web_store
            .apply(web.clone())
            .map_err(|err| format!("写入面板配置文件失败: {err}"))
    } else {
        // 外部编辑 / 手工重载：文件已是最新内容，不再回写。
        Ok(ctx.web_store.adopt(web.clone()))
    };

    let version = match saved {
        Ok(version) => version,
        Err(message) => {
            // 写盘失败：把监听恢复成切换前的状态，避免磁盘与运行态不一致。
            if service_changed {
                let abandoned = {
                    let mut slot = ctx.web.lock().unwrap_or_else(|err| err.into_inner());
                    let current = slot.take();
                    *slot = previous_handle;
                    current
                };
                if let Some(mut handle) = abandoned {
                    handle.stop();
                }
            }
            record_failure(ctx, message.clone(), source);
            return ApplyOutcome::Failed(message);
        }
    };

    // 旧监听的连接线程不受影响：当前保存请求仍能正常把响应写回浏览器。
    if let Some(mut handle) = previous_handle {
        handle.stop();
    }

    if previous.username != web.username || previous.password != web.password {
        // 同时刷新会话文件里的凭据指纹：否则下次启动会把旧凭据的会话重新
        // 加载回来（文件里存的是变更前的指纹）。
        ctx.sessions.reset_for_credentials(&auth::AuthConfig::new(
            web.username.clone(),
            web.password.clone(),
        ));
        info!("面板认证信息已实时更新，旧登录会话全部失效");
    }

    let report = ApplyReport {
        version,
        scheduled_count: ctx.status.lock().map(|s| s.scheduled_count).unwrap_or(0),
        duration_ms: started.elapsed().as_millis() as u64,
        source: source.as_str().to_string(),
        scope: "web",
        warnings: notes,
        notes: Vec::new(),
        locked_tasks: Vec::new(),
    };

    info!(
        version,
        source = source.as_str(),
        duration_ms = report.duration_ms,
        "面板配置已保存并实时生效"
    );
    for note in &report.warnings {
        info!("{note}");
    }

    ApplyOutcome::Applied(report)
}

/// 记录一次失败的热重载，供面板展示最近错误。
fn record_failure(ctx: &AdminContext, message: String, source: ReloadSource) {
    error!(source = source.as_str(), error = %message, "配置热重载失败");
    let mut status = ctx.status.lock().unwrap_or_else(|err| err.into_inner());
    status.last_error = Some(message);
    status.last_reload_at = Some(now_rfc3339());
    status.last_reload_source = Some(source.label().to_string());
}

/// 配置文件监听线程句柄。
struct WatcherHandle {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl WatcherHandle {
    fn stop(&mut self) {
        // 只置标志、不 join：监听线程可能正在等待热重载内核回包，join 会与内核
        // 互相等待直到超时。线程最多一个轮询周期后自行退出。
        self.stop.store(true, Ordering::Relaxed);
        self.thread.take();
    }
}

fn spawn_watcher(ctx: Arc<AdminContext>) -> WatcherHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let thread = std::thread::Builder::new()
        .name("autofilm-config-watch".to_string())
        .spawn(move || watch_loop(ctx, thread_stop))
        .ok();

    WatcherHandle { stop, thread }
}

/// 轮询两份配置文件的修改时间，发现外部改动后分别触发对应的应用流程。
///
/// 两条通道互不影响：编辑 `web.yaml` 不会重建调度器，编辑 `config.yaml` 也不会
/// 改动面板设置。
fn watch_loop(ctx: Arc<AdminContext>, stop: Arc<AtomicBool>) {
    info!(
        business = %ctx.config_path.display(),
        web = %ctx.web_config_path.display(),
        interval_secs = WATCH_INTERVAL.as_secs(),
        "已启动配置文件变更监听（按 web.watch_file 实时开关）"
    );

    'watch: while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(WATCH_INTERVAL);
        if stop.load(Ordering::Relaxed) {
            break;
        }

        // watch_file 本身也是实时配置：每轮重新读取，开关改动即时生效。
        if !ctx.web_config().watch_file {
            continue;
        }

        if ctx.store.config_changed_on_disk() {
            match Config::load(&ctx.config_path) {
                Ok(config) => {
                    let (reply, receiver) = std_mpsc::sync_channel(1);
                    if ctx
                        .commands
                        .blocking_send(AdminCommand::Apply {
                            config,
                            source: ReloadSource::File,
                            reply,
                        })
                        .is_err()
                    {
                        break 'watch;
                    }

                    match receiver.recv_timeout(APPLY_TIMEOUT) {
                        Ok(ApplyOutcome::Applied(report)) => info!(
                            version = report.version,
                            scheduled_count = report.scheduled_count,
                            "业务配置文件改动已热重载"
                        ),
                        Ok(ApplyOutcome::Rejected(issues)) => warn!(
                            count = issues.len(),
                            "业务配置文件改动未通过校验，已保留当前运行配置"
                        ),
                        Ok(ApplyOutcome::Failed(message)) => {
                            warn!(error = %message, "业务配置应用失败，已保留当前运行配置");
                        }
                        Ok(ApplyOutcome::Locked(tasks)) => {
                            warn!(tasks = ?tasks, "业务配置改动了运行中任务，已保留当前运行配置");
                        }
                        Err(std_mpsc::RecvTimeoutError::Timeout) => {
                            warn!("等待业务配置热重载结果超时，已放弃本次等待");
                        }
                        Err(std_mpsc::RecvTimeoutError::Disconnected) => break 'watch,
                    }
                }
                Err(err) => {
                    // 解析失败时保留当前运行配置，只标记已读，避免反复触发。
                    warn!(
                        path = %ctx.config_path.display(),
                        error = %err,
                        "检测到业务配置变化，但解析失败，已保留当前运行配置"
                    );
                }
            }
            // 无论成功与否都记录当前文件时间，避免同一份内容被反复重试。
            ctx.store.mark_seen();
        }

        if ctx.web_store.config_changed_on_disk() {
            match WebConfig::load(&ctx.web_config_path) {
                Ok(web) => {
                    let (reply, receiver) = std_mpsc::sync_channel(1);
                    if ctx
                        .commands
                        .blocking_send(AdminCommand::ApplyWeb {
                            web,
                            source: ReloadSource::File,
                            reply,
                        })
                        .is_err()
                    {
                        break 'watch;
                    }

                    match receiver.recv_timeout(APPLY_TIMEOUT) {
                        Ok(ApplyOutcome::Applied(_)) => info!(
                            path = %ctx.web_config_path.display(),
                            "面板配置文件改动已载入并实时生效"
                        ),
                        Ok(ApplyOutcome::Rejected(issues)) => warn!(
                            count = issues.len(),
                            "面板配置文件改动未通过校验，已保留当前运行配置"
                        ),
                        Ok(ApplyOutcome::Failed(message)) => {
                            warn!(error = %message, "面板配置保存失败，已保留当前运行配置");
                        }
                        Ok(ApplyOutcome::Locked(tasks)) => {
                            warn!(tasks = ?tasks, "面板配置改动被锁定，已保留当前运行配置");
                        }
                        Err(std_mpsc::RecvTimeoutError::Timeout) => {
                            warn!("等待面板配置保存结果超时，已放弃本次等待");
                        }
                        Err(std_mpsc::RecvTimeoutError::Disconnected) => break 'watch,
                    }
                }
                Err(err) => {
                    warn!(
                        path = %ctx.web_config_path.display(),
                        error = %err,
                        "检测到面板配置变化，但解析失败，已保留当前运行配置"
                    );
                }
            }
            ctx.web_store.mark_seen();
        }
    }
}

/// 返回当前本地时间（秒级 RFC3339），用于面板展示。
fn now_rfc3339() -> String {
    Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;
    use crate::schedule::utils::shared_guard;

    const YAML: &str = "\
alist2strm_tasks:
  - id: 动漫
    cron: 0 0 20 * * *
    alist: a
    source_dir: /ani
    target_dir: /media/ani
  - id: 电影
    cron: 0 0 21 * * *
    alist: a
    source_dir: /movie
    target_dir: /media/movie
";

    fn test_context(dir: &std::path::Path, config: Config) -> Arc<AdminContext> {
        let (tx, _rx) = mpsc::channel(1);
        Arc::new(AdminContext {
            store: Arc::new(ConfigStore::new(dir.join("config.yaml"), config)),
            web_store: Arc::new(WebStore::new(dir.join("web.yaml"), WebConfig::default())),
            web: Mutex::new(None),
            mediawarp: Arc::new(Supervisor::new(dir.to_path_buf())),
            guard: auth::LoginGuard::default(),
            sessions: auth::SessionStore::new(),
            schema: serde_json::Value::Null,
            commands: tx,
            status: Mutex::new(RuntimeStatus::default()),
            runners: Mutex::new(schedule::RunRegistry::default()),
            alist_clients: Mutex::new(HashMap::new()),
            tz: chrono_tz::UTC,
            handle: tokio::runtime::Handle::current(),
            config_path: dir.join("config.yaml"),
            web_config_path: dir.join("web.yaml"),
            timezone: "UTC".to_string(),
            started_at: String::new(),
        })
    }

    #[tokio::test]
    async fn panel_save_is_rejected_when_a_running_task_changed() {
        let dir = std::env::temp_dir().join(format!(
            "autofilm-apply-lock-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let current = Config::from_yaml(YAML).unwrap();
        let ctx = test_context(&dir, current.clone());

        let key = format!("{}:动漫", schedule::MODULE_ALIST2STRM);
        let slot = shared_guard(&key).try_acquire().expect("占用任务执行权");

        let mut next = current.clone();
        next.alist2strm_tasks[0].source_dir = "/changed".to_string();
        let outcome = apply(&ctx, &mut None, chrono_tz::UTC, next, ReloadSource::Panel).await;

        match outcome {
            ApplyOutcome::Locked(tasks) => assert_eq!(tasks, vec![key]),
            other => panic!("改动运行中任务应被拒绝，实际得到: {other:?}"),
        }
        assert_eq!(ctx.store.version(), 1, "被拒绝的保存不能推进版本号");
        assert!(!dir.join("config.yaml").exists(), "被拒绝的保存不能写盘");

        // 任务停止后，同一份改动才允许进入后续流程（校验 / 重建调度器）。
        drop(slot);
        let mut next = current.clone();
        next.alist2strm_tasks[0].source_dir = "/changed".to_string();
        let outcome = apply(&ctx, &mut None, chrono_tz::UTC, next, ReloadSource::Panel).await;
        assert!(
            !matches!(outcome, ApplyOutcome::Locked(_)),
            "任务结束后不应再被锁定: {outcome:?}"
        );
    }
}
