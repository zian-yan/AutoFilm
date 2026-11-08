//! Web 管理面板配置：`web.yaml`。
//!
//! 与业务配置（`config.yaml`）完全独立：独立文件、独立的存储与版本号、独立的
//! 保存路径。面板保存业务配置不会重写本文件，反之亦然，因此不存在「谁覆盖谁」
//! 的问题。
//!
//! 该配置属于服务级设置（监听地址、认证、是否监听文件改动），几乎都需要重启
//! 进程才能生效，所以保存后只提示「重启生效」，不参与调度器热重载。
//!
//! 与业务配置一样，本文件允许为空：文件不存在或内容为空时使用内置默认值
//! （不启用面板、监听 `127.0.0.1:8080`、无认证、启用文件监听）。

use std::{fs, net::SocketAddr, path::Path};

use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::config::{Result, ValidationIssue, save_atomic, write_skeleton};

/// Web 配置缺失时写入的骨架内容，等价于 [`WebConfig::default`]。
const DEFAULT_WEB_SKELETON: &str = "\
# AutoFilm Web 管理面板配置（web.yaml）
#
# 与业务配置 config.yaml 完全独立：本文件只管面板自身的监听地址与认证，
# 面板保存业务配置时不会重写本文件，反之亦然。
#
# 本文件允许为空，缺省值等价于下面这份内容。

# 是否启用面板；启用后可在浏览器里增删改业务配置并热重载。
enable: false
# 监听地址，必须是 ip:port；监听非回环地址时必须设置密码，否则进程拒绝启动。
listen: 127.0.0.1:8080
# HTTP Basic 认证用户名（设置密码时必填）。
username: \"\"
# HTTP Basic 认证密码；留空表示不认证，此时只允许监听回环地址。
password: \"\"
# 是否监听配置文件的外部改动并自动热重载（2 秒轮询，同时监听 config.yaml 与 web.yaml）。
watch_file: true
";

/// Web 管理面板配置。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WebConfig {
    // 是否启用 Web 管理面板。
    #[serde(default)]
    pub enable: bool,
    // 面板监听地址，必须是 ip:port 形式。
    #[serde(default = "default_web_listen")]
    pub listen: String,
    // HTTP Basic 认证用户名。
    #[serde(default)]
    pub username: String,
    // HTTP Basic 认证密码；留空表示不做认证（仅允许监听回环地址）。
    #[serde(default)]
    pub password: String,
    // 是否监听配置文件的外部改动并自动热重载。
    #[serde(default = "default_watch_file")]
    pub watch_file: bool,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            enable: false,
            listen: default_web_listen(),
            username: String::new(),
            password: String::new(),
            watch_file: default_watch_file(),
        }
    }
}

impl WebConfig {
    /// 读取面板配置。
    ///
    /// 文件缺失不算错误：按默认值（不启用面板）启动，并尽力补一份骨架文件。
    /// 只有「文件存在但读不出来」或「内容不是合法 YAML」才返回错误。
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let content = match fs::read_to_string(path) {
            Ok(content) => content,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                if let Err(err) = write_skeleton(path, DEFAULT_WEB_SKELETON) {
                    warn!(
                        path = %path.display(),
                        error = %err,
                        "面板配置不存在且无法创建骨架文件，将使用内置默认值"
                    );
                }
                return Ok(Self::default());
            }
            Err(err) => return Err(crate::config::Error::Read(err)),
        };

        Self::from_yaml(&content)
    }

    /// 从 YAML 文本解析面板配置；空文档 / `null` / 只有注释都得到默认值。
    pub fn from_yaml(text: &str) -> Result<Self> {
        if crate::config::is_blank_yaml(text) {
            return Ok(Self::default());
        }

        let value: serde_yaml::Value = serde_yaml::from_str(text)?;
        let parsed: Option<WebConfig> = serde_yaml::from_value(value)?;
        Ok(parsed.unwrap_or_default())
    }

    /// 原子写回 `web.yaml`，不会触碰业务配置文件。
    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        save_atomic(path, self).map_err(crate::config::Error::Write)
    }

    /// 校验面板配置的语义正确性。
    ///
    /// 未启用面板时不做任何校验——此时监听地址与密码都不会被使用，
    /// 提前报错只会妨碍「先留空、以后再开面板」的初始化流程。
    pub fn validate(&self) -> Vec<ValidationIssue> {
        let mut issues = Vec::new();
        if !self.enable {
            return issues;
        }

        match self.listen.parse::<SocketAddr>() {
            Ok(addr) => {
                // 未设置密码时只允许监听回环地址，避免面板裸奔暴露到公网。
                if self.password.is_empty() && !addr.ip().is_loopback() {
                    issues.push(ValidationIssue::new(
                        "web.password",
                        "监听非回环地址时必须设置访问密码",
                    ));
                }
            }
            Err(_) => issues.push(ValidationIssue::new(
                "web.listen",
                "监听地址需为 ip:port 形式，例如 127.0.0.1:8080",
            )),
        }

        if !self.password.is_empty() && self.username.trim().is_empty() {
            issues.push(ValidationIssue::new(
                "web.username",
                "设置密码时必须填写用户名",
            ));
        }

        issues
    }

    /// 列出本次改动中需要重启才能生效的项。
    ///
    /// 面板配置全部属于服务级设置，没有可以热更新的字段，因此这里只负责把
    /// 「改了哪几项」说清楚，让前端给出明确提示。
    pub fn restart_reasons(&self, next: &WebConfig) -> Vec<String> {
        let mut reasons = Vec::new();
        if self.enable != next.enable {
            reasons.push(format!(
                "web.enable 已改为 {}，重启后生效",
                if next.enable { "true" } else { "false" }
            ));
        }
        if self.listen != next.listen {
            reasons.push(format!("面板监听地址已改为 {}，重启后生效", next.listen));
        }
        if self.username != next.username || self.password != next.password {
            reasons.push("面板认证信息已保存，重启后生效".to_string());
        }
        if self.watch_file != next.watch_file {
            reasons.push("配置文件监听开关已保存，重启后生效".to_string());
        }
        reasons
    }

    /// 返回脱敏后的副本，用于回传前端与导出 YAML。
    pub fn masked(&self) -> Self {
        let mut masked = self.clone();
        masked.password = String::new();
        masked
    }
}

fn default_web_listen() -> String {
    "127.0.0.1:8080".to_string()
}

fn default_watch_file() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use std::{
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;
    use crate::config::{backup_path, temp_path};

    const EXAMPLE: &str = include_str!("../config/web.example.yaml");

    fn temp_web_path(name: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!(
                "autofilm-web-{name}-{}",
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .expect("system time should be after unix epoch")
                    .as_nanos()
            ))
            .join("web.yaml")
    }

    #[test]
    fn defaults_when_absent() {
        for text in ["", "null", "# 注释\n"] {
            let web = WebConfig::from_yaml(text).unwrap();
            assert!(!web.enable);
            assert_eq!(web.listen, "127.0.0.1:8080");
            assert!(web.password.is_empty());
            assert!(web.watch_file);
        }
    }

    #[test]
    fn missing_file_yields_defaults() {
        let path = temp_web_path("missing");
        let web = WebConfig::load(&path).expect("missing web config should not fail");
        assert!(!web.enable);
        assert!(path.exists(), "skeleton should be written");
        fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn example_file_matches_defaults() {
        let web = WebConfig::from_yaml(EXAMPLE).expect("example should parse");
        let default = WebConfig::default();
        assert_eq!(web.enable, default.enable);
        assert_eq!(web.listen, default.listen);
        assert_eq!(web.password, default.password);
        assert_eq!(web.watch_file, default.watch_file);
        assert_eq!(web.username, "admin");
    }

    #[test]
    fn skeleton_matches_defaults() {
        let seeded = WebConfig::from_yaml(DEFAULT_WEB_SKELETON).expect("skeleton should parse");
        let default = WebConfig::default();

        assert_eq!(seeded.enable, default.enable);
        assert_eq!(seeded.listen, default.listen);
        assert_eq!(seeded.username, default.username);
        assert_eq!(seeded.password, default.password);
        assert_eq!(seeded.watch_file, default.watch_file);
    }

    #[test]
    fn rejects_public_listen_without_password() {
        let web = WebConfig {
            enable: true,
            listen: "0.0.0.0:8080".to_string(),
            ..WebConfig::default()
        };
        let issues = web.validate();
        assert!(
            issues.iter().any(|issue| issue.field == "web.password"),
            "expected password guard: {issues:?}"
        );
    }

    #[test]
    fn skips_validation_when_disabled() {
        let web = WebConfig {
            enable: false,
            listen: "not-an-addr".to_string(),
            ..WebConfig::default()
        };
        assert!(web.validate().is_empty());
    }

    #[test]
    fn saves_and_reloads() {
        let path = temp_web_path("save");
        let web = WebConfig {
            enable: true,
            listen: "127.0.0.1:9000".to_string(),
            username: "admin".to_string(),
            password: "secret".to_string(),
            watch_file: false,
        };
        web.save(&path).unwrap();
        web.save(&path).unwrap();
        assert!(backup_path(&path).exists());

        let reloaded = WebConfig::load(&path).unwrap();
        assert!(reloaded.enable);
        assert_eq!(reloaded.listen, "127.0.0.1:9000");
        assert_eq!(reloaded.password, "secret");
        assert!(!reloaded.watch_file);

        fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn restart_reasons_list_changed_fields() {
        let before = WebConfig::default();
        let after = WebConfig {
            enable: true,
            listen: "0.0.0.0:8080".to_string(),
            ..before.clone()
        };
        let reasons = before.restart_reasons(&after);
        assert_eq!(reasons.len(), 2, "应当只报告改动过的两项: {reasons:?}");
        assert!(before.restart_reasons(&before).is_empty());
    }

    #[test]
    fn temp_and_backup_paths_are_siblings() {
        let path = PathBuf::from("/tmp/config/web.yaml");
        assert_eq!(temp_path(&path), PathBuf::from("/tmp/config/web.yaml.tmp"));
        assert_eq!(
            backup_path(&path),
            PathBuf::from("/tmp/config/web.yaml.bak")
        );
    }
}
