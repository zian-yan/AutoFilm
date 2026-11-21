use std::{
    collections::HashMap,
    fmt::Write as _,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use base64::Engine;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::config::{temp_path, write_private_file};

/// 会话 Cookie 名称。
pub const SESSION_COOKIE: &str = "autofilm_session";
/// 会话有效期：登录一次保持 30 天，期间无需再次登录。
///
/// 期限从登录时刻固定计算（不随访问顺延），因此行为可预期：浏览器里的
/// Cookie 与服务端会话在同一时刻失效。
pub const SESSION_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);
/// 同时有效的会话数量上限，超出后淘汰最早过期的一条。
const MAX_SESSIONS: usize = 128;
/// 会话文件的格式版本；结构变化时递增，旧文件会被直接忽略。
const SESSION_FILE_VERSION: u32 = 1;

/// 连续失败多少次后短暂锁定来源 IP。
const MAX_FAILURES: u32 = 8;
/// 触发锁定后的冷却时长。
const LOCKOUT: Duration = Duration::from_secs(60);
/// 失败计数窗口，窗口外的失败重新计数，避免历史失败长期累积。
const FAILURE_WINDOW: Duration = Duration::from_secs(300);
/// 清理过期记录的间隔。
const SWEEP_INTERVAL: Duration = Duration::from_secs(60);
/// 同时跟踪的来源数量上限，超过后不再登记新来源，防止内存无界增长。
const MAX_TRACKED_SOURCES: usize = 4096;

/// 面板访问凭据。
///
/// 登录页提交明文凭据后换算成随机会话；同时保留 HTTP Basic 校验，方便
/// curl 或自动化脚本直接带凭据访问。密码为空视为关闭认证，此时只允许
/// 监听回环地址（由 `server::start` 硬性拦截）。
pub struct AuthConfig {
    username: String,
    password: String,
}

impl AuthConfig {
    pub fn new(username: String, password: String) -> Self {
        Self { username, password }
    }

    pub fn enabled(&self) -> bool {
        !self.password.is_empty()
    }

    /// 校验 `Authorization` 请求头。
    ///
    /// 用户名与密码都比较失败时不做提前返回，减少通过响应时间推测凭据的可能。
    pub fn verify(&self, header: Option<&str>) -> bool {
        let Some(value) = header else {
            return false;
        };

        let mut parts = value.splitn(2, ' ');
        let scheme = parts.next().unwrap_or_default();
        let Some(payload) = parts.next() else {
            return false;
        };
        if !scheme.eq_ignore_ascii_case("Basic") {
            return false;
        }

        let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(payload.trim()) else {
            return false;
        };
        let Ok(text) = String::from_utf8(decoded) else {
            return false;
        };
        let Some((username, password)) = text.split_once(':') else {
            return false;
        };

        self.verify_credentials(username, password)
    }

    /// 校验明文的用户名与密码（登录页提交的凭据）。
    ///
    /// 两个比较都要执行完，不做提前返回，减少通过响应时间推测凭据的可能。
    pub fn verify_credentials(&self, username: &str, password: &str) -> bool {
        constant_time_eq(username.as_bytes(), self.username.as_bytes())
            & constant_time_eq(password.as_bytes(), self.password.as_bytes())
    }
}

/// 登录会话表：登录成功后签发随机 token，之后凭 Cookie 里的 token 通过校验。
///
/// 会话落盘在 `web.yaml` 同目录的会话文件里，进程重启（版本升级、容器重建）
/// 后依旧有效——否则「登录一次管 30 天」会被一次重启打断。文件里同时记录
/// 凭据指纹：用户名或密码变了，旧会话一律作废，避免改密码后残留的 Cookie
/// 还能进来；文件损坏或版本不符则按「没有会话」处理，面板照常启动。
pub struct SessionStore {
    /// 会话文件路径；`None` 表示只在内存里（测试与无持久化场景）。
    path: Option<PathBuf>,
    inner: Mutex<SessionState>,
}

struct SessionState {
    /// 凭据指纹，与当前用户名/密码不符时整份会话作废。
    fingerprint: u64,
    /// token -> 过期时刻（Unix 秒）。
    sessions: HashMap<String, u64>,
}

/// 会话文件的磁盘格式。
#[derive(Debug, Serialize, Deserialize)]
struct SessionFile {
    version: u32,
    credentials: u64,
    sessions: Vec<SessionEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
struct SessionEntry {
    token: String,
    expires_at: u64,
}

impl Default for SessionStore {
    fn default() -> Self {
        Self {
            path: None,
            inner: Mutex::new(SessionState {
                fingerprint: 0,
                sessions: HashMap::new(),
            }),
        }
    }
}

impl SessionStore {
    /// 仅内存的会话表（测试用）。
    pub fn new() -> Self {
        Self::default()
    }

    /// 会话文件的默认位置：与 `web.yaml` 同目录，便于随配置一起挂载 / 备份。
    ///
    /// 文件名带前导点，避免在 `ls` 里被误当成用户配置。
    pub fn path_for(web_config_path: &Path) -> PathBuf {
        web_config_path.with_file_name(".autofilm_sessions.json")
    }

    /// 从磁盘加载会话表。
    ///
    /// 加载时会丢弃已过期会话；认证信息在停机期间被改过（指纹不符）时整份
    /// 会话作废。文件不存在、读不出或格式损坏都不会报错——面板不能因为一个
    /// 会话文件坏掉就起不来，代价只是重新登录一次。
    pub fn load(path: Option<PathBuf>, auth: &AuthConfig) -> Self {
        let fingerprint = auth.fingerprint();
        let sessions = path
            .as_deref()
            .map(|path| read_sessions(path, fingerprint))
            .unwrap_or_default();
        Self {
            path,
            inner: Mutex::new(SessionState {
                fingerprint,
                sessions,
            }),
        }
    }

    /// 签发一个新会话，返回写进 Cookie 的 token。
    pub fn issue(&self) -> Option<String> {
        let mut inner = self.inner.lock().ok()?;
        let now = epoch_now();
        inner.sweep(now);

        // 表满时淘汰最早过期的一条，避免长期不关的浏览器把名额占满。
        if inner.sessions.len() >= MAX_SESSIONS
            && let Some(oldest) = inner
                .sessions
                .iter()
                .min_by_key(|(_, expires)| **expires)
                .map(|(token, _)| token.clone())
        {
            inner.sessions.remove(&oldest);
        }

        let token = new_token();
        inner
            .sessions
            .insert(token.clone(), now + SESSION_TTL.as_secs());
        self.persist(&inner);
        Some(token)
    }

    /// 校验 token。
    ///
    /// 有效期从签发时刻固定计算，不随请求顺延：会话文件因此不需要在每次
    /// 访问时重写，也避免「一直开着页面就永不过期」。
    pub fn verify(&self, token: &str) -> bool {
        let Ok(mut inner) = self.inner.lock() else {
            return false;
        };
        let now = epoch_now();
        let Some(expires) = inner.sessions.get(token).copied() else {
            return false;
        };
        if expires <= now {
            inner.sessions.remove(token);
            self.persist(&inner);
            return false;
        }
        true
    }

    /// 注销单个会话。
    pub fn revoke(&self, token: &str) {
        if let Ok(mut inner) = self.inner.lock()
            && inner.sessions.remove(token).is_some()
        {
            self.persist(&inner);
        }
    }

    /// 注销全部会话，可保留一个（改密码时让当前管理员不被踢下线）。
    pub fn revoke_all_except(&self, keep: Option<&str>) {
        if let Ok(mut inner) = self.inner.lock() {
            inner
                .sessions
                .retain(|token, _| Some(token.as_str()) == keep);
            self.persist(&inner);
        }
    }

    /// 认证信息变更：换指纹并清空全部会话。
    ///
    /// 旧凭据签发的 Cookie 不应继续有效，也不应在下次启动时被重新加载。
    pub fn reset_for_credentials(&self, auth: &AuthConfig) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.fingerprint = auth.fingerprint();
            inner.sessions.clear();
            self.persist(&inner);
        }
    }

    /// 把当前会话表写回磁盘；失败只告警，不影响本次登录。
    fn persist(&self, state: &SessionState) {
        let Some(path) = self.path.as_deref() else {
            return;
        };
        if let Err(err) = write_sessions(path, state) {
            warn!(
                path = %path.display(),
                error = %err,
                "写入会话文件失败，重启后需要重新登录"
            );
        }
    }
}

impl SessionState {
    /// 清掉已过期的会话。
    fn sweep(&mut self, now: u64) {
        self.sessions.retain(|_, expires| *expires > now);
    }
}

impl AuthConfig {
    /// 凭据指纹：写入会话文件，加载时与当前用户名/密码比对。
    ///
    /// 存的是不可逆的 64 位摘要而不是明文，避免会话文件里多一份可读凭据。
    /// 它不是密码学哈希，只用于回答「凭据变没变」——同目录的 `web.yaml`
    /// 本身就是明文。
    fn fingerprint(&self) -> u64 {
        fnv1a(&self.username, &self.password)
    }
}

/// FNV-1a 64：短输入上足够分散，实现只有几行，不引入新依赖。
fn fnv1a(username: &str, password: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in username.bytes().chain([0]).chain(password.bytes()) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// 当前 Unix 时间（秒）。系统时钟早于 1970 时按 0 处理，不 panic。
fn epoch_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// 读取并过滤会话文件；任何异常都退化为「没有会话」。
fn read_sessions(path: &Path, fingerprint: u64) -> HashMap<String, u64> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return HashMap::new(),
        Err(err) => {
            warn!(
                path = %path.display(),
                error = %err,
                "读取会话文件失败，本次需要重新登录"
            );
            return HashMap::new();
        }
    };

    let Ok(file) = serde_json::from_str::<SessionFile>(&content) else {
        warn!(path = %path.display(), "会话文件格式无效，已忽略并需要重新登录");
        return HashMap::new();
    };

    // 版本变化或凭据在停机期间被改过：旧 Cookie 一律作废，且顺手覆盖掉旧文件。
    if file.version != SESSION_FILE_VERSION || file.credentials != fingerprint {
        return HashMap::new();
    }

    let now = epoch_now();
    file.sessions
        .into_iter()
        .filter(|entry| entry.expires_at > now && !entry.token.is_empty())
        .map(|entry| (entry.token, entry.expires_at))
        .collect()
}

/// 原子写入会话文件。
///
/// token 等同于登录凭据，因此与配置文件一样按仅属主可读落盘；先写临时文件
/// 再 rename，避免进程在中途退出时留下半截 JSON 把所有人踢下线。
fn write_sessions(path: &Path, state: &SessionState) -> std::io::Result<()> {
    let file = SessionFile {
        version: SESSION_FILE_VERSION,
        credentials: state.fingerprint,
        sessions: state
            .sessions
            .iter()
            .map(|(token, expires_at)| SessionEntry {
                token: token.clone(),
                expires_at: *expires_at,
            })
            .collect(),
    };
    let json = serde_json::to_vec(&file).map_err(std::io::Error::other)?;

    let temp = temp_path(path);
    write_private_file(&temp, &json)?;
    std::fs::rename(&temp, path)
}

/// 生成 32 字节随机 token 的十六进制表示。
fn new_token() -> String {
    let bytes: [u8; 32] = rand::random();
    let mut token = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(token, "{byte:02x}");
    }
    token
}

/// 从 `Cookie` 请求头里取出会话 token。
pub fn session_from_cookie(header: Option<&str>) -> Option<&str> {
    let header = header?;
    for pair in header.split(';') {
        let Some((name, value)) = pair.split_once('=') else {
            continue;
        };
        if name.trim() == SESSION_COOKIE {
            let value = value.trim();
            if !value.is_empty() {
                return Some(value);
            }
        }
    }
    None
}

/// 登录失败限流器，按来源 IP 计数。
///
/// 记录会按 [`SWEEP_INTERVAL`] 定期清理：既清掉冷却结束的锁定，也清掉长期
/// 不再出现的来源，避免被大量一次性 IP 扫描时无限占用内存。
pub struct LoginGuard {
    inner: Mutex<GuardState>,
}

#[derive(Debug)]
struct GuardState {
    attempts: HashMap<String, Attempt>,
    last_sweep: Instant,
}

#[derive(Debug, Default)]
struct Attempt {
    failures: u32,
    first_failure: Option<Instant>,
    locked_until: Option<Instant>,
}

impl Default for LoginGuard {
    fn default() -> Self {
        Self {
            inner: Mutex::new(GuardState {
                attempts: HashMap::new(),
                last_sweep: Instant::now(),
            }),
        }
    }
}

impl LoginGuard {
    /// 若来源被锁定，返回剩余秒数。
    pub fn locked_remaining(&self, key: &str) -> Option<u64> {
        let mut inner = self.inner.lock().ok()?;
        let locked_until = inner.attempts.get(key).and_then(|entry| entry.locked_until);
        let Some(until) = locked_until else {
            return None;
        };

        let now = Instant::now();
        if until > now {
            return Some(until.saturating_duration_since(now).as_secs().max(1));
        }

        // 冷却结束，清除记录让该来源重新获得尝试机会。
        inner.attempts.remove(key);
        None
    }

    /// 记录一次认证失败，达到阈值后锁定来源。
    pub fn register_failure(&self, key: &str) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        let now = Instant::now();
        inner.sweep(now);

        // 跟踪表已满时放弃登记新来源，宁可少限流也不让内存无界增长。
        if inner.attempts.len() >= MAX_TRACKED_SOURCES && !inner.attempts.contains_key(key) {
            return;
        }

        let entry = inner.attempts.entry(key.to_string()).or_default();

        let expired = entry
            .first_failure
            .is_none_or(|first| now.saturating_duration_since(first) > FAILURE_WINDOW);
        if expired {
            entry.first_failure = Some(now);
            entry.failures = 0;
        }

        entry.failures += 1;
        if entry.failures >= MAX_FAILURES {
            entry.failures = 0;
            entry.first_failure = Some(now);
            entry.locked_until = Some(now + LOCKOUT);
        }
    }

    /// 认证成功后清除失败记录。
    pub fn reset(&self, key: &str) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.attempts.remove(key);
        }
    }
}

impl GuardState {
    /// 定期清理过期记录，把表大小控制在上限内。
    fn sweep(&mut self, now: Instant) {
        let due = now.saturating_duration_since(self.last_sweep) >= SWEEP_INTERVAL;
        let full = self.attempts.len() >= MAX_TRACKED_SOURCES;
        if !due && !full {
            return;
        }

        self.last_sweep = now;
        self.attempts.retain(|_, entry| match entry.locked_until {
            // 仍在锁定中：保留，保证限流状态不被清掉。
            Some(until) => until > now,
            // 未锁定：只保留仍在统计窗口内的记录。
            None => entry
                .first_failure
                .is_some_and(|first| now.saturating_duration_since(first) <= FAILURE_WINDOW),
        });
    }
}

/// 定长比较，避免因为提前返回泄漏匹配前缀长度。
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }

    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn basic(username: &str, password: &str) -> String {
        let encoded =
            base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"));
        format!("Basic {encoded}")
    }

    #[test]
    fn accepts_matching_credentials() {
        let auth = AuthConfig::new("admin".to_string(), "secret".to_string());
        assert!(auth.enabled());
        assert!(auth.verify(Some(&basic("admin", "secret"))));
    }

    #[test]
    fn rejects_wrong_scheme_and_credentials() {
        let auth = AuthConfig::new("admin".to_string(), "secret".to_string());
        assert!(!auth.verify(Some(&basic("admin", "wrong"))));
        assert!(!auth.verify(Some(&basic("root", "secret"))));
        assert!(!auth.verify(Some("Bearer token")));
        assert!(!auth.verify(Some("Basic not-base64!!")));
        assert!(!auth.verify(None));
    }

    #[test]
    fn locks_source_after_repeated_failures() {
        let guard = LoginGuard::default();
        for _ in 0..MAX_FAILURES {
            guard.register_failure("127.0.0.1");
        }

        assert!(guard.locked_remaining("127.0.0.1").is_some());
        guard.reset("127.0.0.1");
        assert!(guard.locked_remaining("127.0.0.1").is_none());
    }

    #[test]
    fn sweep_drops_stale_sources_but_keeps_active_locks() {
        let now = Instant::now();
        let mut state = GuardState {
            attempts: HashMap::from([
                (
                    "stale".to_string(),
                    Attempt {
                        failures: 1,
                        first_failure: Some(now),
                        locked_until: None,
                    },
                ),
                (
                    "locked".to_string(),
                    Attempt {
                        failures: 8,
                        first_failure: Some(now),
                        locked_until: Some(now + LOCKOUT + SWEEP_INTERVAL + FAILURE_WINDOW),
                    },
                ),
            ]),
            last_sweep: now,
        };

        // 模拟很久以后的一次清理。
        let later = now + SWEEP_INTERVAL + FAILURE_WINDOW + Duration::from_secs(1);
        state.sweep(later);

        assert!(
            !state.attempts.contains_key("stale"),
            "过期的失败记录应当被清理"
        );
        // 仍然处于锁定状态的来源必须保留，否则限流会被绕过。
        assert!(state.attempts.contains_key("locked"));
    }

    #[test]
    fn accepts_matching_plain_credentials() {
        let auth = AuthConfig::new("admin".to_string(), "secret".to_string());
        assert!(auth.verify_credentials("admin", "secret"));
        assert!(!auth.verify_credentials("admin", "Secret"));
        assert!(!auth.verify_credentials("", "secret"));
    }

    #[test]
    fn session_lifecycle_and_revocation() {
        let sessions = SessionStore::new();
        let token = sessions.issue().expect("应能签发会话");
        assert_eq!(token.len(), 64, "token 应当是 32 字节的十六进制");
        assert!(sessions.verify(&token));

        // 两个会话互相独立，注销一个不影响另一个。
        let other = sessions.issue().expect("应能签发第二个会话");
        assert_ne!(token, other);
        sessions.revoke(&other);
        assert!(!sessions.verify(&other));
        assert!(sessions.verify(&token));

        // 保留当前会话地全量注销（改密码时用）。
        let spare = sessions.issue().expect("应能签发第三个会话");
        sessions.revoke_all_except(Some(&token));
        assert!(sessions.verify(&token));
        assert!(!sessions.verify(&spare));

        sessions.revoke(&token);
        assert!(!sessions.verify(&token));
        assert!(!sessions.verify("不存在的 token"));
    }

    #[test]
    fn expired_sessions_are_rejected() {
        let sessions = SessionStore::new();
        let token = sessions.issue().expect("应能签发会话");
        {
            // 手动把过期时间拨到过去，模拟超过 30 天。
            let mut inner = sessions.inner.lock().unwrap();
            inner.sessions.insert(token.clone(), 1);
        }
        assert!(!sessions.verify(&token));
    }

    /// 会话文件用随机后缀的临时目录，避免并行测试互相踩。
    fn temp_session_path(name: &str) -> std::path::PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("系统时间应晚于 Unix 纪元")
            .as_nanos();
        std::env::temp_dir()
            .join(format!("autofilm-sessions-{name}-{unique}"))
            .join(".autofilm_sessions.json")
    }

    #[test]
    fn session_survives_restart_and_expires_with_credentials_change() {
        let path = temp_session_path("restart");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let auth = AuthConfig::new("admin".to_string(), "secret".to_string());

        let token = {
            let store = SessionStore::load(Some(path.clone()), &auth);
            let token = store.issue().expect("应能签发会话");
            assert!(store.verify(&token));
            token
        };

        // 进程重启后重新加载：30 天内的会话必须继续有效，否则升级一次就被踢下线。
        let reopened = SessionStore::load(Some(path.clone()), &auth);
        assert!(reopened.verify(&token), "重启后会话应保持有效");

        // 停机期间改了密码：指纹对不上，整份会话作废。
        let changed = SessionStore::load(
            Some(path.clone()),
            &AuthConfig::new("admin".to_string(), "new-secret".to_string()),
        );
        assert!(!changed.verify(&token), "改密码后旧会话必须失效");

        // 运行时改密码走 reset：文件里的旧 token 也应被清掉。
        let store = SessionStore::load(Some(path.clone()), &auth);
        store.reset_for_credentials(&AuthConfig::new("admin".to_string(), "next".to_string()));
        let reloaded = SessionStore::load(
            Some(path.clone()),
            &AuthConfig::new("admin".to_string(), "next".to_string()),
        );
        assert!(!reloaded.verify(&token));

        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn corrupted_or_expired_session_files_are_ignored() {
        let path = temp_session_path("broken");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let auth = AuthConfig::new("admin".to_string(), "secret".to_string());

        std::fs::write(&path, "不是 JSON").unwrap();
        assert!(
            SessionStore::load(Some(path.clone()), &auth)
                .issue()
                .is_some(),
            "文件损坏时面板仍应能签发新会话"
        );

        // 过期条目在加载时就被丢掉，不会因为文件里留着旧记录而复活。
        let expired = SessionFile {
            version: SESSION_FILE_VERSION,
            credentials: auth.fingerprint(),
            sessions: vec![SessionEntry {
                token: "deadbeef".to_string(),
                expires_at: 1,
            }],
        };
        std::fs::write(&path, serde_json::to_vec(&expired).unwrap()).unwrap();
        assert!(!SessionStore::load(Some(path.clone()), &auth).verify("deadbeef"));

        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn parses_session_cookie_from_header() {
        assert_eq!(
            session_from_cookie(Some("other=1; autofilm_session=abc; extra=2")),
            Some("abc")
        );
        assert_eq!(
            session_from_cookie(Some("autofilm_session=abc")),
            Some("abc")
        );
        assert_eq!(session_from_cookie(Some("autofilm_session=")), None);
        assert_eq!(session_from_cookie(Some("other=1")), None);
        assert_eq!(session_from_cookie(None), None);
    }
}
