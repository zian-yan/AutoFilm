//! MediaWarp 的面板配置与静态校验。
//!
//! 字段分两组：
//! - 「托管参数」只被 AutoFilm 使用（是否托管、二进制路径、生成文件路径、超时）；
//! - 「原生配置」与 MediaWarp `internal/config.Setting` 一一对应，字段名即其
//!   `yaml` 标签，渲染时不做任何改名。
//!
//! 校验规则与 MediaWarp 的 `internal/config.validate()` 保持一致，另外补上
//! TTL 格式、路径前缀与托管参数的范围检查——目的都是**在写盘与重启之前**把
//! 错误挡住，避免生成一份会让代理进程直接 panic 退出的配置。

use serde::{Deserialize, Serialize};

use crate::config::ValidationIssue;

/// 面板中 MediaWarp 分区的字段前缀。
const BASE: &str = "mediawarp";

/// 一份完整的 MediaWarp 托管配置。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MediaWarpConfig {
    // ---------------- AutoFilm 托管参数（不会写进生成文件） ----------------
    /// 是否由 AutoFilm 生成配置并托管子进程。
    ///
    /// 关闭时仍然会生成配置文件（便于外部部署方式使用），但不会拉起 / 重启进程。
    /// 默认关闭，避免未安装 MediaWarp 二进制的部署在启动时反复报错。
    pub enable: bool,
    /// 生成的 MediaWarp `config.yaml` 路径。
    ///
    /// 相对路径按 `config.yaml` 所在目录解析：默认 `mediawarp.yaml` 对应本地
    /// `config/mediawarp.yaml`、容器内 `/config/mediawarp.yaml`。
    pub config_path: String,
    /// MediaWarp 可执行文件；只写名字时按 `PATH` 查找（容器内为
    /// `/usr/local/bin/mediawarp`）。
    pub binary: String,
    /// 附加启动参数，追加在 `--config <path>` 之后。
    pub extra_args: Vec<String>,
    /// 重启后等待端口就绪的最长秒数。
    pub startup_timeout_secs: u64,
    /// 停止旧进程时等待优雅退出的最长秒数，超时后强制结束。
    pub stop_timeout_secs: u64,

    // ---------------- MediaWarp 原生配置 ----------------
    pub port: u16,
    pub server: Server,
    pub log: Log,
    pub cache: Cache,
    pub client: ClientFilter,
    pub http_strm: HttpStrm,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Server {
    /// Go 里的字段名是 `type`，Rust 关键字冲突，只在序列化层改名。
    #[serde(rename = "type")]
    pub kind: String,
    pub addr: String,
    pub auth: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Log {
    pub access: LogTarget,
    pub service: LogTarget,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LogTarget {
    pub console: bool,
    pub file: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Cache {
    pub enable: bool,
    /// Go `time.Duration` 字符串，例如 `1m` / `300ms` / `2h45m` / `0`。
    pub http_strm_ttl: String,
    pub image_ttl: String,
    pub subtitle_ttl: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClientFilter {
    pub enable: bool,
    /// `WhiteList` / `BlackList`，与 Go `constants.FliterMode` 一致。
    pub mode: String,
    pub list: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HttpStrm {
    pub enable: bool,
    pub proxy: bool,
    pub final_url: bool,
    /// v1.2.3 起默认拒绝环回目标（SSRF 防护）。
    pub allow_loopback: bool,
    pub compatibility_mode: bool,
    pub hide_embedded_subtitles: bool,
    pub hide_embedded_subtitles_clients: Vec<String>,
    pub prefix_list: Vec<String>,
}

impl Default for MediaWarpConfig {
    fn default() -> Self {
        Self {
            enable: false,
            config_path: "mediawarp.yaml".to_string(),
            binary: "mediawarp".to_string(),
            extra_args: Vec::new(),
            startup_timeout_secs: 10,
            stop_timeout_secs: 5,
            // 以下默认值与 MediaWarp 仓库 config/config.yaml.example 一致。
            port: 9000,
            server: Server::default(),
            log: Log::default(),
            cache: Cache::default(),
            client: ClientFilter::default(),
            http_strm: HttpStrm::default(),
        }
    }
}

impl Default for Server {
    fn default() -> Self {
        Self {
            kind: "Emby".to_string(),
            addr: "http://localhost:8096".to_string(),
            auth: String::new(),
        }
    }
}

impl Default for Log {
    fn default() -> Self {
        Self {
            access: LogTarget {
                console: true,
                file: false,
            },
            service: LogTarget {
                console: true,
                file: true,
            },
        }
    }
}

impl Default for LogTarget {
    fn default() -> Self {
        Self {
            console: true,
            file: false,
        }
    }
}

impl Default for Cache {
    fn default() -> Self {
        Self {
            enable: true,
            http_strm_ttl: "1m".to_string(),
            image_ttl: "10m".to_string(),
            subtitle_ttl: "2h".to_string(),
        }
    }
}

impl Default for ClientFilter {
    fn default() -> Self {
        Self {
            enable: false,
            mode: "BlackList".to_string(),
            list: vec!["Fileball".to_string(), "Infuse".to_string()],
        }
    }
}

impl Default for HttpStrm {
    fn default() -> Self {
        Self {
            enable: true,
            proxy: false,
            final_url: true,
            allow_loopback: false,
            compatibility_mode: false,
            hide_embedded_subtitles: false,
            hide_embedded_subtitles_clients: Vec::new(),
            prefix_list: vec![
                "/media/strm/http".to_string(),
                "/media/strm/https".to_string(),
            ],
        }
    }
}

impl MediaWarpConfig {
    /// 用于「配置是否真的变了」的比较指纹。
    ///
    /// 包含托管参数，因此改二进制路径 / 超时 / 开关也会触发重启；结构体字段顺序
    /// 固定，序列化结果在同一进程内稳定可比较。
    pub fn fingerprint(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// 静态校验，返回全部问题而不是遇到第一个就返回。
    pub fn validate(&self) -> Vec<ValidationIssue> {
        let mut issues = Vec::new();
        let issue =
            |field: &str, message: &str| ValidationIssue::new(format!("{BASE}.{field}"), message);

        if self.config_path.trim().is_empty() {
            issues.push(issue("config_path", "生成的配置文件路径不能为空"));
        }
        if self.binary.trim().is_empty() {
            issues.push(issue("binary", "MediaWarp 可执行文件名不能为空"));
        }
        if !(1..=60).contains(&self.startup_timeout_secs) {
            issues.push(issue(
                "startup_timeout_secs",
                "启动等待超时需在 1 - 60 秒之间",
            ));
        }
        if !(1..=60).contains(&self.stop_timeout_secs) {
            issues.push(issue("stop_timeout_secs", "停止等待超时需在 1 - 60 秒之间"));
        }

        // MediaWarp: 端口必须是 1-65535。
        if self.port == 0 {
            issues.push(issue("port", "监听端口需在 1 - 65535 之间"));
        }
        // MediaWarp: 类型只能是 Emby / Jellyfin。
        if !matches!(self.server.kind.as_str(), "Emby" | "Jellyfin") {
            issues.push(issue(
                "server.type",
                "媒体服务器类型只能是 Emby 或 Jellyfin（区分大小写）",
            ));
        }
        // MediaWarp: addr 必须是带主机名的 http(s) 地址。
        match parse_http_addr(&self.server.addr) {
            Ok(()) => {}
            Err(message) => issues.push(issue("server.addr", &message)),
        }

        for (field, value) in [
            ("cache.http_strm_ttl", &self.cache.http_strm_ttl),
            ("cache.image_ttl", &self.cache.image_ttl),
            ("cache.subtitle_ttl", &self.cache.subtitle_ttl),
        ] {
            if !is_go_duration(value) {
                issues.push(issue(
                    field,
                    "TTL 需为 Go 时长格式（如 0 / 300ms / 1m / 2h45m），0 表示关闭该缓存",
                ));
            }
        }

        if !matches!(self.client.mode.as_str(), "WhiteList" | "BlackList") {
            issues.push(issue(
                "client.mode",
                "客户端过滤模式只能是 WhiteList 或 BlackList（区分大小写）",
            ));
        }
        // MediaWarp: 白名单为空会让所有请求被拒绝，启动时直接报错。
        if self.client.enable
            && self.client.mode == "WhiteList"
            && self.client.list.iter().all(|item| item.trim().is_empty())
        {
            issues.push(issue(
                "client.list",
                "白名单模式下至少需要一个 User-Agent 关键字，否则所有请求都会被拒绝",
            ));
        }

        for entry in &self.http_strm.prefix_list {
            if !entry.trim().starts_with('/') {
                issues.push(issue(
                    "http_strm.prefix_list",
                    "Strm 路径前缀需以 / 开头（媒体服务器内的绝对路径）",
                ));
                break;
            }
        }

        issues
    }
}

/// 与 Go `url.Parse` + Scheme/Hostname 校验等价的最小检查。
fn parse_http_addr(addr: &str) -> Result<(), String> {
    let trimmed = addr.trim();
    if trimmed.is_empty() {
        return Err("上游媒体服务器地址不能为空".to_string());
    }
    let url = reqwest::Url::parse(trimmed)
        .map_err(|err| format!("地址格式错误（{err}），需为 http:// 或 https:// 开头的完整地址"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("地址需以 http:// 或 https:// 开头".to_string());
    }
    if url.host_str().unwrap_or_default().is_empty() {
        return Err("地址缺少主机名".to_string());
    }
    Ok(())
}

/// 判断字符串是否可被 Go `time.ParseDuration` 解析。
///
/// 支持 `0`、`-1.5h`、`300ms`、`2h45m` 等组合；单位沿用 Go 的
/// `ns` / `us`(或 `µs`) / `ms` / `s` / `m` / `h`。
pub(crate) fn is_go_duration(value: &str) -> bool {
    let text = value.trim();
    if text.is_empty() {
        return false;
    }
    let body = text
        .strip_prefix('+')
        .or_else(|| text.strip_prefix('-'))
        .unwrap_or(text);
    if body == "0" {
        return true;
    }
    if body.is_empty() {
        return false;
    }

    const UNITS: [&str; 8] = ["ns", "us", "µs", "μs", "ms", "s", "m", "h"];
    let mut rest = body;
    while !rest.is_empty() {
        let number: String = rest
            .chars()
            .take_while(|ch| ch.is_ascii_digit() || *ch == '.')
            .collect();
        if number.is_empty()
            || !number.chars().any(|ch| ch.is_ascii_digit())
            || number.matches('.').count() > 1
        {
            return false;
        }
        rest = &rest[number.len()..];

        let Some(unit) = UNITS.iter().find(|unit| rest.starts_with(**unit)) else {
            return false;
        };
        rest = &rest[unit.len()..];
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid_and_match_upstream_example() {
        let config = MediaWarpConfig::default();
        assert!(config.validate().is_empty(), "{:?}", config.validate());
        assert_eq!(config.port, 9000);
        assert_eq!(config.server.kind, "Emby");
        assert_eq!(config.cache.http_strm_ttl, "1m");
        assert!(!config.http_strm.allow_loopback);
        assert!(!config.enable, "默认不托管，避免未安装二进制时报错");
    }

    #[test]
    fn go_duration_parser_accepts_valid_and_rejects_invalid() {
        for value in ["0", "1m", "300ms", "2h45m", "-1.5h", "10s", "500us", "2µs"] {
            assert!(is_go_duration(value), "`{value}` 应被接受");
        }
        for value in ["", "1", "m", "1x", "1..5h", "1h 30m", "1hm"] {
            assert!(!is_go_duration(value), "`{value}` 应被拒绝");
        }
    }

    #[test]
    fn validation_mirrors_mediawarp_startup_checks() {
        let mut config = MediaWarpConfig::default();
        config.port = 0;
        config.server.kind = "Plex".to_string();
        config.server.addr = "ftp://emby".to_string();
        config.client.enable = true;
        config.client.mode = "WhiteList".to_string();
        config.client.list.clear();
        config.cache.image_ttl = "10 minutes".to_string();
        config.http_strm.prefix_list = vec!["media/strm".to_string()];

        let fields: Vec<String> = config
            .validate()
            .into_iter()
            .map(|item| item.field)
            .collect();
        for expected in [
            "mediawarp.port",
            "mediawarp.server.type",
            "mediawarp.server.addr",
            "mediawarp.client.list",
            "mediawarp.cache.image_ttl",
            "mediawarp.http_strm.prefix_list",
        ] {
            assert!(
                fields.contains(&expected.to_string()),
                "缺少 {expected}: {fields:?}"
            );
        }
    }

    #[test]
    fn fingerprint_changes_with_any_field() {
        let base = MediaWarpConfig::default();
        let mut changed = base.clone();
        changed.enable = !changed.enable;
        assert_ne!(base.fingerprint(), changed.fingerprint());
        let mut changed = base.clone();
        changed.http_strm.prefix_list.push("/extra".to_string());
        assert_ne!(base.fingerprint(), changed.fingerprint());
        assert_eq!(base.fingerprint(), MediaWarpConfig::default().fingerprint());
    }
}
