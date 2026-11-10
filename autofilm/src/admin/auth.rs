use std::{
    collections::HashMap,
    fmt::Write as _,
    sync::Mutex,
    time::{Duration, Instant},
};

use base64::Engine;

/// 会话 Cookie 名称。
pub const SESSION_COOKIE: &str = "autofilm_session";
/// 会话有效期：滑动过期，超过该时长没有请求就需要重新登录。
pub const SESSION_TTL: Duration = Duration::from_secs(12 * 60 * 60);
/// 会话表清理间隔。
const SESSION_SWEEP_INTERVAL: Duration = Duration::from_secs(300);
/// 同时有效的会话数量上限，超出后淘汰最早过期的一条。
const MAX_SESSIONS: usize = 128;

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
/// 采用 HTTP Basic 认证：浏览器会弹出原生登录框，无需在页面里实现登录逻辑；
/// 密码为空视为关闭认证，此时只允许监听回环地址（由 `server::start` 硬性拦截）。
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
/// 会话只存在内存里：重启进程后所有人都需要重新登录，这既省去了持久化，
/// 也保证「改了密码但忘了踢人」不会因为旧文件残留而出现。
pub struct SessionStore {
    inner: Mutex<SessionState>,
}

struct SessionState {
    /// token -> 过期时刻。
    sessions: HashMap<String, Instant>,
    last_sweep: Instant,
}

impl Default for SessionStore {
    fn default() -> Self {
        Self {
            inner: Mutex::new(SessionState {
                sessions: HashMap::new(),
                last_sweep: Instant::now(),
            }),
        }
    }
}

impl SessionStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// 清空全部会话。
    ///
    /// 面板用户名或密码实时变更后调用：旧凭据签发的 Cookie 不应继续有效。
    pub fn clear(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.sessions.clear();
        }
    }

    /// 签发一个新会话，返回写进 Cookie 的 token。
    pub fn issue(&self) -> Option<String> {
        let mut inner = self.inner.lock().ok()?;
        let now = Instant::now();
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
        inner.sessions.insert(token.clone(), now + SESSION_TTL);
        Some(token)
    }

    /// 校验 token；命中时顺延过期时间（滑动过期）。
    pub fn verify(&self, token: &str) -> bool {
        let Ok(mut inner) = self.inner.lock() else {
            return false;
        };
        let now = Instant::now();
        let Some(expires) = inner.sessions.get_mut(token) else {
            return false;
        };
        if *expires <= now {
            inner.sessions.remove(token);
            return false;
        }
        *expires = now + SESSION_TTL;
        true
    }

    /// 注销单个会话。
    pub fn revoke(&self, token: &str) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.sessions.remove(token);
        }
    }

    /// 注销全部会话，可保留一个（改密码时让当前管理员不被踢下线）。
    pub fn revoke_all_except(&self, keep: Option<&str>) {
        if let Ok(mut inner) = self.inner.lock() {
            inner
                .sessions
                .retain(|token, _| Some(token.as_str()) == keep);
        }
    }
}

impl SessionState {
    /// 清掉已过期的会话。
    fn sweep(&mut self, now: Instant) {
        if now.saturating_duration_since(self.last_sweep) < SESSION_SWEEP_INTERVAL {
            return;
        }
        self.last_sweep = now;
        self.sessions.retain(|_, expires| *expires > now);
    }
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
            // 手动把过期时间拨到过去，模拟长时间没操作。
            let mut inner = sessions.inner.lock().unwrap();
            inner.sessions.insert(token.clone(), Instant::now());
        }
        assert!(!sessions.verify(&token));
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
