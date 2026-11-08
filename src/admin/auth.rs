use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

use base64::Engine;

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

        constant_time_eq(username.as_bytes(), self.username.as_bytes())
            & constant_time_eq(password.as_bytes(), self.password.as_bytes())
    }
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
    use base64::Engine as _;

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
}
