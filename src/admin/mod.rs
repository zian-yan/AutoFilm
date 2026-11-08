//! Web 管理面板与配置热重载。
//!
//! 该模块提供三件事：
//! 1. 一个零外部依赖的 HTTP 服务（基于 `std::net`），对外提供配置读写 API；
//! 2. 配置热重载内核：校验 → 写盘 → 重建调度器 → 切换，失败时保持旧配置继续运行；
//! 3. 配置文件监听线程，手工编辑 YAML 后同样可以即时生效。
//!
//! 配置分为两条完全独立的通道：
//! - 业务配置（`config.yaml`）：参与调度器热重载，保存后立即重建全部 cron 任务；
//! - 面板配置（`web.yaml`）：服务级设置，保存后只提示「重启生效」，不触碰调度器。
//!
//! 两条通道各有独立的 [`ConfigStore`] / [`WebStore`]、独立的版本号与独立的写盘
//! 路径，因此面板保存业务配置不会重写 `web.yaml`，反之亦然。
//!
//! 为避免额外依赖，HTTP 层只实现管理面板所需的最小子集：`GET`/`PUT`/`POST`、
//! `Content-Length` 定长请求体、短连接（`Connection: close`）。

mod assets;
pub mod auth;
mod http;
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

use chrono::Local;
use tokio::sync::{mpsc, oneshot};
use tokio_cron_scheduler::JobScheduler;
use tracing::{error, info, warn};

use crate::{
    config::{Config, ValidationIssue},
    schedule,
    web_config::WebConfig,
};

pub use auth::AuthConfig;
pub use server::WebHandle;
pub use store::{ConfigStore, Persist, Snapshot, Store, WebStore};

/// 配置文件外部改动检测间隔。
const WATCH_INTERVAL: Duration = Duration::from_secs(2);

/// 等待一次热重载完成的最长时间，超时后连接线程放弃等待并返回 503。
pub(crate) const APPLY_TIMEOUT: Duration = Duration::from_secs(20);

/// 热重载的触发来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReloadSource {
    /// 管理面板保存。
    Panel,
    /// 配置文件被外部修改。
    File,
    /// 手工点击「重新加载文件」。
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
    pub auth: AuthConfig,
    pub guard: auth::LoginGuard,
    pub schema: serde_json::Value,
    pub commands: mpsc::Sender<AdminCommand>,
    pub status: Mutex<RuntimeStatus>,
    pub config_path: PathBuf,
    pub web_config_path: PathBuf,
    /// 启动时实际生效的监听地址（面板配置改动后需重启才会变化）。
    pub listen: String,
    /// 启动时实际生效的文件监听开关。
    pub watch_file: bool,
    pub timezone: String,
    pub started_at: String,
}

impl AdminContext {
    /// 以 JSON 形式返回运行时状态。
    pub fn status_json(&self) -> serde_json::Value {
        // 锁中毒时继续使用内部数据，避免面板因为一次 panic 永久不可用。
        let status = match self.status.lock() {
            Ok(status) => status.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        serde_json::to_value(status).unwrap_or(serde_json::Value::Null)
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
    Shutdown {
        reply: oneshot::Sender<()>,
    },
}

/// 管理面板与热重载内核的生命周期句柄。
pub struct Runtime {
    pub ctx: Arc<AdminContext>,
    web: Option<WebHandle>,
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
        let (mut scheduler, scheduled_count) = schedule::create_scheduler(config.clone(), tz)
            .await
            .map_err(|err| format!("初始化调度器失败: {err}"))?;
        scheduler
            .start()
            .await
            .map_err(|err| format!("启动调度器失败: {err}"))?;

        let store = Arc::new(ConfigStore::new(config_path.clone(), config));
        let web_store = Arc::new(WebStore::new(web_config_path.clone(), web_config.clone()));
        let (command_tx, command_rx) = mpsc::channel::<AdminCommand>(16);

        let ctx = Arc::new(AdminContext {
            store,
            web_store,
            auth: AuthConfig::new(web_config.username.clone(), web_config.password.clone()),
            guard: auth::LoginGuard::default(),
            schema: schema::schema(),
            commands: command_tx,
            status: Mutex::new(RuntimeStatus {
                scheduled_count,
                ..RuntimeStatus::default()
            }),
            config_path,
            web_config_path,
            listen: web_config.listen.clone(),
            watch_file: web_config.watch_file,
            timezone: tz.to_string(),
            started_at: now_rfc3339(),
        });

        // 面板先绑定端口：监听失败时直接终止启动，避免留下无人接管的调度器。
        let web = if web_config.enable {
            match server::start(ctx.clone()) {
                Ok(handle) => {
                    info!(
                        listen = %web_config.listen,
                        auth = !web_config.password.is_empty(),
                        "Web 管理面板已启动"
                    );
                    Some(handle)
                }
                Err(err) => {
                    let _ = scheduler.shutdown().await;
                    return Err(err);
                }
            }
        } else {
            info!("Web 管理面板未启用（web.enable = false）");
            None
        };

        let supervisor = tokio::spawn(supervise(ctx.clone(), command_rx, Some(scheduler), tz));

        let mut runtime = Self {
            ctx,
            web,
            watcher: None,
            supervisor: Some(supervisor),
        };

        if web_config.watch_file {
            runtime.watcher = Some(spawn_watcher(runtime.ctx.clone()));
        }

        Ok((runtime, scheduled_count))
    }

    /// 是否启用了 Web 面板。
    pub fn web_enabled(&self) -> bool {
        self.web.is_some()
    }

    /// 停止面板、监听线程与调度器。
    pub async fn shutdown(&mut self) {
        if let Some(mut watcher) = self.watcher.take() {
            watcher.stop();
        }
        if let Some(mut web) = self.web.take() {
            web.stop();
        }

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

    let issues = config.validate();
    if !issues.is_empty() {
        let message = format!("配置校验未通过（{} 处问题）", issues.len());
        record_failure(ctx, message, source);
        return ApplyOutcome::Rejected(issues);
    }

    let (mut scheduler, scheduled_count) =
        match schedule::create_scheduler(config.clone(), tz).await {
            Ok(result) => result,
            Err(err) => {
                let message = format!("构建调度任务失败: {err}");
                record_failure(ctx, message.clone(), source);
                return ApplyOutcome::Failed(message);
            }
        };

    if let Err(err) = scheduler.start().await {
        let message = format!("启动新调度器失败: {err}");
        record_failure(ctx, message.clone(), source);
        return ApplyOutcome::Failed(message);
    }

    // 只有面板保存才写盘；外部编辑与手工重新加载只采用磁盘内容，
    // 避免把用户写的注释与排版抹掉。
    let version = if source == ReloadSource::Panel {
        match ctx.store.apply(config) {
            Ok(version) => version,
            Err(err) => {
                let message = format!("写入业务配置文件失败: {err}");
                let _ = scheduler.shutdown().await;
                record_failure(ctx, message.clone(), source);
                return ApplyOutcome::Failed(message);
            }
        }
    } else {
        ctx.store.adopt(config)
    };

    // 新调度器已经就绪后再停掉旧的；正在执行的任务不会被中断，
    // 它们持有的是启动时的配置快照，下一次触发才会使用新配置。
    if let Some(mut previous_scheduler) = current.replace(scheduler) {
        if let Err(err) = previous_scheduler.shutdown().await {
            warn!(error = %err, "关闭旧调度器时出错");
        }
    }

    let report = ApplyReport {
        version,
        scheduled_count,
        duration_ms: started.elapsed().as_millis() as u64,
        source: source.as_str().to_string(),
        scope: "business",
        warnings: Vec::new(),
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

/// 应用一份新的面板配置。
///
/// 只写 `web.yaml`：不重建调度器、不推进业务配置的版本号。面板配置全部是
/// 服务级设置，保存后仍需重启进程才会生效，因此把改动项作为警告回传前端。
fn apply_web(ctx: &AdminContext, web: WebConfig, source: ReloadSource) -> ApplyOutcome {
    let started = Instant::now();

    let issues = web.validate();
    if !issues.is_empty() {
        let message = format!("面板配置校验未通过（{} 处问题）", issues.len());
        record_failure(ctx, message, source);
        return ApplyOutcome::Rejected(issues);
    }

    let previous = ctx.web_store.snapshot();
    let warnings = previous.config.restart_reasons(&web);

    let version = if source == ReloadSource::Panel {
        match ctx.web_store.apply(web) {
            Ok(version) => version,
            Err(err) => {
                let message = format!("写入面板配置文件失败: {err}");
                record_failure(ctx, message.clone(), source);
                return ApplyOutcome::Failed(message);
            }
        }
    } else {
        // 外部编辑 / 手工重载：文件已是最新内容，不再回写。
        ctx.web_store.adopt(web)
    };

    let report = ApplyReport {
        version,
        scheduled_count: ctx.status.lock().map(|s| s.scheduled_count).unwrap_or(0),
        duration_ms: started.elapsed().as_millis() as u64,
        source: source.as_str().to_string(),
        scope: "web",
        warnings,
    };

    info!(
        version,
        source = source.as_str(),
        duration_ms = report.duration_ms,
        "面板配置已保存（重启后生效）"
    );
    for warning in &report.warnings {
        warn!("{warning}");
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
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
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
        "已启用配置文件变更监听"
    );

    'watch: while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(WATCH_INTERVAL);
        if stop.load(Ordering::Relaxed) {
            break;
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
                            "面板配置文件改动已载入，重启后生效"
                        ),
                        Ok(ApplyOutcome::Rejected(issues)) => warn!(
                            count = issues.len(),
                            "面板配置文件改动未通过校验，已保留当前运行配置"
                        ),
                        Ok(ApplyOutcome::Failed(message)) => {
                            warn!(error = %message, "面板配置保存失败，已保留当前运行配置");
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
