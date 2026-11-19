//! MediaWarp 子进程托管与「保存即重启」事务。
//!
//! 设计要点：
//! - **单一数据源**：面板配置 → [`super::render`] → `config/mediawarp.yaml` →
//!   `--config` 传给子进程。进程永远读取刚写好的文件，不会出现内存与磁盘不一致。
//! - **串行**：所有启停/重启都走同一把异步互斥锁，面板连点保存、文件监听与外部
//!   重载都不会并发拉起两个进程、争抢同一个端口。
//! - **只在真的变化时重启**：配置指纹与磁盘内容都未变时直接跳过，避免改一个无关
//!   任务就把正在播放的请求打断。
//! - **完整回滚**：写盘 → 优雅停旧 → 拉起新进程 → 端口健康检查，任一步失败都把
//!   生成文件还原成旧内容并用旧配置重新拉起；`apply()` 后续步骤（写 `config.yaml`）
//!   失败时也可以调用 [`Supervisor::undo`] 回滚整次事务。

use std::{
    fs,
    net::{Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    process::Stdio,
    sync::Mutex,
    time::{Duration, Instant},
};

use chrono::Local;
use serde::Serialize;
use serde_json::Value;
use tokio::{
    process::{Child, Command},
    sync::Mutex as AsyncMutex,
    time::{sleep, timeout},
};
use tracing::{info, warn};

use super::{MediaWarpConfig, render};
use crate::config::save_atomic_bytes;

/// 服务当前所处阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// 未启用托管（配置仍会生成）。
    Disabled,
    /// 已启用但当前没有运行中的进程。
    Stopped,
    Starting,
    Running,
    Failed,
}

/// 面板轮询展示的状态快照。
#[derive(Debug, Clone, Serialize)]
pub struct Status {
    pub phase: Phase,
    pub enabled: bool,
    pub pid: Option<u32>,
    pub port: u16,
    pub binary: String,
    /// 解析后的生成文件绝对路径（相对 CWD 时按 `config.yaml` 所在目录解析）。
    pub config_path: String,
    pub healthy: bool,
    /// 磁盘上的生成文件是否与当前面板配置一致。
    pub file_in_sync: bool,
    pub restart_count: u64,
    pub last_restart_at: Option<String>,
    pub last_error: Option<String>,
    /// 上一次成功应用的配置指纹，用于判定是否需要重启；不暴露给前端。
    #[serde(skip)]
    pub applied: Option<String>,
}

impl Default for Status {
    fn default() -> Self {
        Self {
            phase: Phase::Disabled,
            enabled: false,
            pid: None,
            port: 9000,
            binary: "mediawarp".to_string(),
            config_path: String::new(),
            healthy: false,
            file_in_sync: false,
            restart_count: 0,
            last_restart_at: None,
            last_error: None,
            applied: None,
        }
    }
}

/// 一次保存事务的结果摘要。
#[derive(Debug, Clone, Serialize)]
pub struct CommitReport {
    pub changed: bool,
    pub note: String,
}

struct Inner {
    child: Option<Child>,
    /// 上一次成功应用的完整配置，供回滚时按旧参数重新拉起。
    current: Option<MediaWarpConfig>,
    /// 尚未被 `finish()` 确认的回滚现场。
    undo: Option<Undo>,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            child: None,
            current: None,
            undo: None,
        }
    }
}

struct Undo {
    path: PathBuf,
    /// `None` 表示事务开始前文件不存在，回滚时应删除。
    bytes: Option<Vec<u8>>,
    previous: Option<MediaWarpConfig>,
    previous_applied: Option<String>,
}

/// 托管 MediaWarp 子进程的句柄。
pub struct Supervisor {
    /// 生成相对路径的基准：`config.yaml` 所在目录。
    base_dir: PathBuf,
    inner: AsyncMutex<Inner>,
    /// 同步锁：HTTP 连接线程（同步）也能读取状态，不必进入异步运行时。
    status: Mutex<Status>,
}

impl Supervisor {
    pub fn new(base_dir: PathBuf) -> Self {
        // 容器里 `config.yaml` 常以相对路径（config/config.yaml）传入，基准目录也会是
        // 相对的 `config`。此时写盘按主进程 cwd(`/`) 解析成 /config/mediawarp.yaml，
        // 而子进程 cwd 是 /config，同一个相对路径会被解析成
        // /config/config/mediawarp.yaml，MediaWarp 随即报 "no such file or directory"。
        // 统一转成绝对路径，让写盘、`--config` 参数与状态展示指向同一处，
        // 且不受子进程 cwd 影响。
        let base_dir = if is_fixed_path(&base_dir) {
            base_dir
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("/"))
                .join(&base_dir)
        };
        Self {
            base_dir,
            inner: AsyncMutex::new(Inner::default()),
            status: Mutex::new(Status::default()),
        }
    }

    pub fn status(&self) -> Status {
        self.status
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clone()
    }

    pub fn status_json(&self) -> Value {
        serde_json::to_value(self.status()).unwrap_or(Value::Null)
    }

    /// 是否启用了托管；二进制方式启动时据此决定「无调度任务也不要退出」。
    pub fn managed(&self) -> bool {
        self.status().enabled
    }

    /// 相对路径按 `config.yaml` 所在目录解析，绝对路径原样使用。
    /// `base_dir` 已在 `new()` 里绝对化，因此返回值一定是绝对路径。
    pub fn resolve_path(&self, raw: &str) -> PathBuf {
        let path = PathBuf::from(raw.trim());
        if is_fixed_path(&path) {
            path
        } else {
            self.base_dir.join(path)
        }
    }

    /// 子进程工作目录：配置目录的父目录（容器里 `/config` → `/`），
    /// 让 MediaWarp 的 `logs/` 等相对路径落到容器根下的挂载点。
    fn process_cwd(&self) -> PathBuf {
        self.base_dir
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(&self.base_dir)
            .to_path_buf()
    }

    /// MediaWarp 落盘日志的目录：`<子进程工作目录>/logs/mediawarp`。
    ///
    /// MediaWarp 的 `logs` 是相对路径，由它自己的工作目录解析（见 [`Self::process_cwd`]），
    /// 面板读日志必须用同一个基准；若按主进程 cwd 解析，容器里会读到一份空目录，
    /// 面板就会一直显示「暂无日志」。
    pub fn log_dir(&self) -> PathBuf {
        self.process_cwd().join("logs").join("mediawarp")
    }

    fn set_status(&self, update: impl FnOnce(&mut Status)) {
        let mut status = self.status.lock().unwrap_or_else(|err| err.into_inner());
        update(&mut status);
    }

    /// 启动时调用：生成配置文件并按配置拉起子进程（幂等）。
    ///
    /// 失败只记录日志、不阻断 AutoFilm 启动——否则用户连面板都打不开，无法修正
    /// 一个写错的二进制路径。状态会停留在 `failed` 并在面板上显示原因。
    pub async fn bootstrap(&self, config: &MediaWarpConfig) -> Result<CommitReport, String> {
        let report = self.commit_inner(config, false).await?;
        self.finish().await;
        Ok(report)
    }

    /// 配置保存事务：只在配置或磁盘内容变化时重启。
    pub async fn commit(&self, config: &MediaWarpConfig) -> Result<CommitReport, String> {
        self.commit_inner(config, false).await
    }

    /// 面板「立即重启」：强制重启，即使配置一字未改。
    pub async fn restart_now(&self, config: &MediaWarpConfig) -> Result<CommitReport, String> {
        if !config.enable {
            return Err("未启用 MediaWarp 托管；请先勾选「启用托管」并保存后再重启".to_string());
        }
        let report = self.commit_inner(config, true).await?;
        self.finish().await;
        Ok(report)
    }

    /// `apply()` 后续步骤失败时回滚整次事务。
    pub async fn undo(&self) {
        let mut inner = self.inner.lock().await;
        if inner.undo.is_none() {
            return;
        }
        self.restore(
            &mut inner,
            "业务配置写入失败，MediaWarp 已回滚到上一次生效的配置",
        )
        .await;
    }

    /// 事务已完整落地，丢弃回滚现场。
    pub async fn finish(&self) {
        self.inner.lock().await.undo = None;
    }

    /// AutoFilm 退出时优雅停止子进程，避免容器里留下孤儿进程。
    pub async fn shutdown(&self) {
        let mut inner = self.inner.lock().await;
        if let Some(mut child) = inner.child.take() {
            stop_child(&mut child, 5).await;
        }
        inner.current = None;
        inner.undo = None;
        let enabled = self.status().enabled;
        self.set_status(|status| {
            status.phase = if enabled {
                Phase::Stopped
            } else {
                Phase::Disabled
            };
            status.pid = None;
            status.healthy = false;
        });
    }

    async fn commit_inner(
        &self,
        config: &MediaWarpConfig,
        force: bool,
    ) -> Result<CommitReport, String> {
        let yaml = render::render(config)?;
        let path = self.resolve_path(&config.config_path);
        let fingerprint = config.fingerprint();

        let existing = match fs::read(&path) {
            Ok(bytes) => Some(bytes),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => return Err(format!("读取 {} 失败: {err}", path.display())),
        };
        let needs_write = existing.as_deref() != Some(yaml.as_bytes());
        let previous_applied = self.status().applied;
        let fingerprint_changed = previous_applied.as_deref() != Some(fingerprint.as_str());

        // 从这里开始串行化：重启期间任何其它保存或手动重启都会排队。
        let mut inner = self.inner.lock().await;

        // 回收已经退出的子进程；端口空出来后自然会被重新拉起。
        let mut exited = false;
        if let Some(child) = inner.child.as_mut() {
            match child.try_wait() {
                Ok(Some(status)) => {
                    exited = true;
                    warn!(exit = %status, "MediaWarp 子进程已退出，本次保存会重新拉起");
                }
                Ok(None) => {}
                Err(err) => warn!(error = %err, "检查 MediaWarp 子进程状态失败"),
            }
        }
        if exited {
            inner.child = None;
        }
        let alive = inner.child.is_some();
        let restart_needed =
            config.enable && (force || needs_write || fingerprint_changed || !alive);
        let stop_needed = !config.enable && alive;

        self.set_status(|status| {
            status.enabled = config.enable;
            status.port = config.port;
            status.binary = config.binary.clone();
            status.config_path = path.display().to_string();
        });

        if !needs_write && !restart_needed && !stop_needed {
            self.set_status(|status| status.file_in_sync = true);
            return Ok(CommitReport {
                changed: false,
                note: "MediaWarp 配置未变化，未触发重启".to_string(),
            });
        }

        inner.undo = Some(Undo {
            path: path.clone(),
            bytes: existing,
            previous: inner.current.clone(),
            previous_applied,
        });

        // 步骤 1：先原子写盘（含 .bak 备份），新进程只会读到完整文件。
        if needs_write {
            if let Err(err) = save_atomic_bytes(&path, yaml.as_bytes()) {
                inner.undo = None;
                let message = format!("写入 {} 失败: {err}", path.display());
                self.set_status(|status| {
                    status.file_in_sync = false;
                    status.last_error = Some(message.clone());
                    status.phase = Phase::Failed;
                });
                return Err(message);
            }
        }

        // 步骤 2：关闭托管时只停进程，不做健康检查。
        if !config.enable {
            if let Some(mut child) = inner.child.take() {
                stop_child(&mut child, config.stop_timeout_secs).await;
            }
            inner.current = Some(config.clone());
            self.set_status(|status| {
                status.phase = Phase::Disabled;
                status.pid = None;
                status.healthy = false;
                status.file_in_sync = true;
                status.applied = Some(fingerprint.clone());
                status.last_error = None;
            });
            return Ok(CommitReport {
                changed: true,
                note: "配置文件已更新；托管未启用，未自动重启代理服务".to_string(),
            });
        }

        // 步骤 3：优雅停旧进程。
        self.set_status(|status| {
            status.phase = Phase::Starting;
            status.pid = None;
            status.healthy = false;
        });
        if let Some(mut child) = inner.child.take() {
            stop_child(&mut child, config.stop_timeout_secs).await;
        }

        // 步骤 4：拉起新进程 + 端口健康检查。
        let cwd = self.process_cwd();
        let mut child = match spawn_process(config, &path, &cwd) {
            Ok(child) => child,
            Err(err) => {
                let message = format!(
                    "启动 MediaWarp 失败（可执行文件 `{}`）：{err}；请确认镜像中已包含 MediaWarp，或修正二进制路径",
                    config.binary
                );
                self.restore(&mut inner, &message).await;
                return Err(message);
            }
        };

        match wait_healthy(&mut child, config).await {
            Ok(()) => {
                let pid = child.id();
                inner.child = Some(child);
                inner.current = Some(config.clone());
                self.set_status(|status| {
                    status.phase = Phase::Running;
                    status.pid = pid;
                    status.healthy = true;
                    status.file_in_sync = true;
                    status.restart_count += 1;
                    status.last_restart_at = Some(now_rfc3339());
                    status.applied = Some(fingerprint.clone());
                    status.last_error = None;
                });
                info!(
                    pid = ?pid,
                    port = config.port,
                    config = %path.display(),
                    "MediaWarp 配置已生效，代理服务重启完成"
                );
                let pid_text = pid
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "未知".to_string());
                Ok(CommitReport {
                    changed: true,
                    note: format!(
                        "配置已写入并自动重启 MediaWarp（端口 {}，PID {}）",
                        config.port, pid_text
                    ),
                })
            }
            Err(message) => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                self.restore(&mut inner, &message).await;
                Err(message)
            }
        }
    }

    /// 还原生成文件、用旧配置尽力重新拉起进程，并把失败原因写进状态。
    async fn restore(&self, inner: &mut Inner, reason: &str) {
        let Some(undo) = inner.undo.take() else {
            return;
        };
        let path = undo.path.clone();

        match undo.bytes {
            Some(bytes) => {
                if let Err(err) = save_atomic_bytes(&path, &bytes) {
                    warn!(path = %path.display(), error = %err, "回滚 MediaWarp 配置文件失败");
                }
            }
            None => {
                let _ = fs::remove_file(&path);
            }
        }

        if let Some(mut child) = inner.child.take() {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }

        let mut recovered = false;
        if let Some(previous) = undo.previous.clone() {
            if previous.enable {
                let cwd = self
                    .base_dir
                    .parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                    .unwrap_or(&self.base_dir)
                    .to_path_buf();
                if let Ok(mut child) = spawn_process(&previous, &path, &cwd) {
                    if wait_healthy(&mut child, &previous).await.is_ok() {
                        let pid = child.id();
                        inner.child = Some(child);
                        inner.current = Some(previous);
                        recovered = true;
                        self.set_status(|status| {
                            status.phase = Phase::Running;
                            status.pid = pid;
                            status.healthy = true;
                        });
                        info!(pid = ?pid, "已按回滚前的配置重新拉起 MediaWarp");
                    } else {
                        let _ = child.start_kill();
                        let _ = child.wait().await;
                    }
                }
            }
        }

        self.set_status(|status| {
            status.applied = undo.previous_applied.clone();
            status.file_in_sync = true;
            status.last_error = Some(reason.to_string());
            if !recovered {
                status.phase = Phase::Failed;
                status.pid = None;
                status.healthy = false;
            }
        });
        warn!(error = %reason, recovered, "MediaWarp 配置事务已回滚");
    }
}

fn spawn_process(config: &MediaWarpConfig, path: &Path, cwd: &Path) -> std::io::Result<Child> {
    // 子进程 cwd 与主进程不同，`--config` 必须用绝对路径；`resolve_path()` 已保证这一点，
    // 这里再兜底一次，避免任何相对路径在子进程里被二次拼接。
    let config_path = if is_fixed_path(path) {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut command = Command::new(&config.binary);
    command
        .arg("--config")
        .arg(config_path)
        .args(&config.extra_args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        // 进程句柄意外丢弃时也要带走子进程，避免端口被孤儿进程占住。
        .kill_on_drop(true);
    command.spawn()
}

/// 先发 SIGTERM 让 Go 侧完成 `http.Server.Shutdown`，超时再强制结束。
async fn stop_child(child: &mut Child, grace_secs: u64) {
    let grace = Duration::from_secs(grace_secs.clamp(1, 60));

    #[cfg(unix)]
    {
        if let Some(pid) = child.id() {
            // SAFETY: 只对本进程刚派生的子进程发送 SIGTERM。
            let sent = unsafe { libc::kill(pid as i32, libc::SIGTERM) };
            if sent != 0 {
                let _ = child.start_kill();
            }
        } else {
            let _ = child.start_kill();
        }
    }
    #[cfg(not(unix))]
    {
        let _ = child.start_kill();
    }

    if timeout(grace, child.wait()).await.is_err() {
        warn!("MediaWarp 未在宽限期内退出，已强制结束");
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
}

/// 健康检查：进程未提前退出且 `127.0.0.1:<port>` 可连接即视为就绪。
///
/// 只探测 TCP 而不是发业务请求——上游 Emby/Jellyfin 可能恰好不可用，此时
/// MediaWarp 返回 502 也仍属于「代理已正常监听」，不应误判成启动失败。
async fn wait_healthy(child: &mut Child, config: &MediaWarpConfig) -> Result<(), String> {
    let budget = Duration::from_secs(config.startup_timeout_secs.clamp(1, 60));
    let started = Instant::now();
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, config.port));

    loop {
        if let Ok(Some(exit)) = child.try_wait() {
            return Err(format!(
                "MediaWarp 启动后立即退出（{exit}）；通常是配置有误或端口 {} 被占用，请查看容器日志",
                config.port
            ));
        }
        if timeout(
            Duration::from_millis(400),
            tokio::net::TcpStream::connect(addr),
        )
        .await
        .is_ok_and(|result| result.is_ok())
        {
            return Ok(());
        }
        if started.elapsed() >= budget {
            return Err(format!(
                "MediaWarp 在 {} 秒内未监听 127.0.0.1:{}，已回滚到上一次生效的配置",
                budget.as_secs(),
                config.port
            ));
        }
        sleep(Duration::from_millis(200)).await;
    }
}

fn now_rfc3339() -> String {
    Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// 路径是否已不依赖当前工作目录：Unix 绝对路径，或 Windows 上以根分隔符
/// 开头（如 `/config`，不带盘符但也不会随 cwd 变化）的路径。
fn is_fixed_path(path: &Path) -> bool {
    path.is_absolute() || path.has_root()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_relative_paths_against_config_dir() {
        let supervisor = Supervisor::new(PathBuf::from("/config"));
        assert_eq!(
            supervisor.resolve_path("mediawarp.yaml"),
            PathBuf::from("/config/mediawarp.yaml")
        );
        assert_eq!(
            supervisor.resolve_path("/etc/mediawarp.yaml"),
            PathBuf::from("/etc/mediawarp.yaml")
        );
    }

    #[test]
    fn relative_base_dir_is_absolute_for_both_write_and_spawn() {
        // 复现容器内的启动方式：--config config/config.yaml（相对路径）。
        let supervisor = Supervisor::new(PathBuf::from("config"));
        let path = supervisor.resolve_path("mediawarp.yaml");
        assert!(path.is_absolute(), "生成路径必须绝对化: {}", path.display());
        assert!(path.ends_with("config/mediawarp.yaml"));

        let cwd = supervisor.process_cwd();
        assert!(
            cwd.is_absolute(),
            "子进程 cwd 必须绝对化: {}",
            cwd.display()
        );
        assert_eq!(cwd, supervisor.base_dir.parent().unwrap());
        assert_ne!(
            cwd, supervisor.base_dir,
            "cwd 不能等于配置目录，否则相对路径会被二次拼接"
        );

        // 绝对路径配置仍然原样使用。
        assert_eq!(
            supervisor.resolve_path("/etc/mediawarp.yaml"),
            PathBuf::from("/etc/mediawarp.yaml")
        );
    }

    #[test]
    fn initial_status_is_disabled_and_serializable() {
        let supervisor = Supervisor::new(PathBuf::from("config"));
        let json = supervisor.status_json();
        assert_eq!(json["phase"], "disabled");
        assert_eq!(json["enabled"], false);
        // applied 指纹属于内部状态，不应下发到面板。
        assert!(json.get("applied").is_none());
    }

    #[tokio::test]
    async fn disabled_commit_only_writes_the_generated_file() {
        let dir = std::env::temp_dir().join(format!(
            "autofilm-mw-disabled-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let supervisor = Supervisor::new(dir.clone());
        let mut config = MediaWarpConfig::default();
        config.config_path = "mediawarp.yaml".to_string();

        let report = supervisor.commit(&config).await.expect("commit");
        assert!(report.changed);
        let path = supervisor.resolve_path(&config.config_path);
        let written = fs::read_to_string(&path).expect("generated file");
        assert!(written.starts_with("# 本文件由 AutoFilm 自动生成"));
        assert_eq!(supervisor.status().phase, Phase::Disabled);
        assert!(supervisor.status().file_in_sync);

        // 配置与文件都没变：第二次提交不应有任何动作。
        let second = supervisor.commit(&config).await.expect("commit");
        assert!(!second.changed);
        assert_eq!(supervisor.status().restart_count, 0);

        supervisor.finish().await;
        fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn undo_restores_the_previous_file_content() {
        let dir = std::env::temp_dir().join(format!(
            "autofilm-mw-undo-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mediawarp.yaml");
        let old = "# 旧的手工配置\nport: 9000\n";
        fs::write(&path, old).unwrap();

        let supervisor = Supervisor::new(dir.clone());
        let config = MediaWarpConfig::default();
        // 直接提交会写新内容；随后模拟 config.yaml 写盘失败触发 undo。
        supervisor.commit(&config).await.expect("commit");
        assert_ne!(fs::read_to_string(&path).unwrap(), old);
        supervisor.undo().await;
        assert_eq!(fs::read_to_string(&path).unwrap(), old);

        supervisor.shutdown().await;
        fs::remove_dir_all(&dir).ok();
    }
}
