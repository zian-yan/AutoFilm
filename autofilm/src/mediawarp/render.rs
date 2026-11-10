//! 面板配置 ↔ MediaWarp 原生 `config.yaml` 的转换。
//!
//! 生成文件是**派生产物**，不是数据源：面板保存在 `config.yaml` 的 `mediawarp`
//! 段，保存时由 [`render`] 渲染成 MediaWarp 能直接读取的 YAML。文件头带注释说明
//! 来源，避免用户手工改完被下一次保存覆盖却不知道原因。

use serde::{Deserialize, Serialize};

use super::config::{Cache, ClientFilter, HttpStrm, Log, MediaWarpConfig, Server};

/// 生成文件头。Go 的 `yaml.Unmarshal` 会忽略注释，不影响解析。
const HEADER: &str = "\
# 本文件由 AutoFilm 自动生成，请勿手工编辑。
# 请在 Web 管理面板「MediaWarp」页修改配置并保存；AutoFilm 会重写本文件并重启代理服务。
# 如需把手工改动带回面板，请使用该页的「从文件导入」按钮。
# MediaWarp: https://github.com/zian-yan/MediaWarp
";

/// MediaWarp `internal/config.Setting` 的等价结构。
///
/// 字段名与 Go 的 `yaml` 标签完全一致；托管参数（enable / binary / ...）不在
/// 这里，因此生成文件是一份干净、可被独立部署的 MediaWarp 配置。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Doc {
    pub port: u16,
    pub server: Server,
    pub log: Log,
    pub cache: Cache,
    pub client: ClientFilter,
    pub http_strm: HttpStrm,
}

impl Default for Doc {
    fn default() -> Self {
        Self::from_config(&MediaWarpConfig::default())
    }
}

impl Doc {
    pub fn from_config(config: &MediaWarpConfig) -> Self {
        Self {
            port: config.port,
            server: config.server.clone(),
            log: config.log.clone(),
            cache: config.cache.clone(),
            client: config.client.clone(),
            http_strm: config.http_strm.clone(),
        }
    }

    /// 把原生字段合并回面板配置，保留 AutoFilm 的托管参数。
    pub fn into_config(self, base: &MediaWarpConfig) -> MediaWarpConfig {
        MediaWarpConfig {
            enable: base.enable,
            config_path: base.config_path.clone(),
            binary: base.binary.clone(),
            extra_args: base.extra_args.clone(),
            startup_timeout_secs: base.startup_timeout_secs,
            stop_timeout_secs: base.stop_timeout_secs,
            port: self.port,
            server: self.server,
            log: self.log,
            cache: self.cache,
            client: self.client,
            http_strm: self.http_strm,
        }
    }
}

/// 渲染一份可直接交给 `mediawarp --config <path>` 的 YAML。
pub fn render(config: &MediaWarpConfig) -> Result<String, String> {
    let body = serde_yaml::to_string(&Doc::from_config(config))
        .map_err(|err| format!("序列化 MediaWarp 配置失败: {err}"))?;
    Ok(format!("{HEADER}{body}"))
}

/// 从文件内容解析原生配置，并保留 `base` 中的托管参数。
///
/// 用于「从文件导入」：外部部署或手工修改过的 `config.yaml` 可以一次性带回面板。
pub fn parse_into(text: &str, base: &MediaWarpConfig) -> Result<MediaWarpConfig, String> {
    if crate::config::is_blank_yaml(text) {
        return Err("配置文件为空或只有注释，没有可导入的内容".to_string());
    }
    let doc: Doc =
        serde_yaml::from_str(text).map_err(|err| format!("解析 MediaWarp 配置失败: {err}"))?;
    let config = doc.into_config(base);
    let issues = config.validate();
    if !issues.is_empty() {
        return Err(format!(
            "导入的配置未通过校验：{}",
            issues
                .iter()
                .map(|item| format!("{} {}", item.field, item.message))
                .collect::<Vec<_>>()
                .join("；")
        ));
    }
    Ok(config)
}

/// 与 [`parse_into`] 相同，但使用默认托管参数（供接口层直接调用）。
pub fn parse(text: &str) -> Result<MediaWarpConfig, String> {
    parse_into(text, &MediaWarpConfig::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_all_upstream_keys_and_no_management_fields() {
        let yaml = render(&MediaWarpConfig::default()).expect("render");
        for key in [
            "port:",
            "server:",
            "  type:",
            "  addr:",
            "  auth:",
            "log:",
            "  access:",
            "  service:",
            "cache:",
            "  http_strm_ttl:",
            "  image_ttl:",
            "  subtitle_ttl:",
            "client:",
            "  mode:",
            "  list:",
            "http_strm:",
            "  final_url:",
            "  allow_loopback:",
            "  compatibility_mode:",
            "  hide_embedded_subtitles:",
            "  hide_embedded_subtitles_clients:",
            "  prefix_list:",
        ] {
            assert!(yaml.contains(key), "生成的 YAML 缺少 `{key}`:\n{yaml}");
        }
        // 托管参数不能出现在生成文件里；注意 cache/http_strm 也有各自的 enable，
        // 因此这里只按「顶层键」判断，避免把合法的原生字段误判成泄漏。
        for key in [
            "enable:",
            "binary:",
            "config_path:",
            "extra_args:",
            "startup_timeout_secs:",
            "stop_timeout_secs:",
        ] {
            assert!(
                !yaml.lines().any(|line| line.starts_with(key)),
                "生成文件不应包含顶层托管参数 `{key}`:\n{yaml}"
            );
        }
        assert!(yaml.starts_with("# 本文件由 AutoFilm 自动生成"));
    }

    #[test]
    fn render_parse_round_trip_is_lossless() {
        let mut original = MediaWarpConfig::default();
        original.enable = true;
        original.port = 9100;
        original.server.kind = "Jellyfin".to_string();
        original.server.addr = "http://jellyfin:8096".to_string();
        original.server.auth = "secret-key".to_string();
        original.http_strm.allow_loopback = true;
        original.http_strm.prefix_list = vec!["/mnt/strm".to_string()];
        original.client.list = vec!["Infuse".to_string()];
        original.cache.subtitle_ttl = "2h45m".to_string();

        let yaml = render(&original).expect("render");
        let parsed = parse_into(&yaml, &original).expect("parse");
        assert_eq!(parsed, original, "往返之后配置应完全一致:\n{yaml}");
    }

    #[test]
    fn parses_handwritten_upstream_example() {
        let text = "\
port: 9000
server:
  type: Emby
  addr: http://localhost:8096
  auth: 2eaxxxxxxxxxa8
log:
  access:
    console: true
    file: false
  service:
    console: true
    file: true
cache:
  enable: true
  http_strm_ttl: 1m
  image_ttl: 10m
  subtitle_ttl: 2h
client:
  enable: false
  mode: BlackList
  list:
    - Fileball
    - Infuse
http_strm:
  enable: true
  proxy: false
  final_url: true
  allow_loopback: false
  compatibility_mode: false
  hide_embedded_subtitles: false
  hide_embedded_subtitles_clients: []
  prefix_list:
    - /media/strm/http
    - /media/strm/https
";
        let parsed = parse(text).expect("upstream example should parse");
        let mut expected = MediaWarpConfig::default();
        // 上游示例带了一个占位 API Key，默认值则是空串。
        expected.server.auth = "2eaxxxxxxxxxa8".to_string();
        assert_eq!(parsed, expected);
    }

    #[test]
    fn import_preserves_management_fields() {
        let mut base = MediaWarpConfig::default();
        base.enable = true;
        base.binary = "/opt/bin/mediawarp".to_string();
        base.config_path = "/etc/mediawarp.yaml".to_string();

        let yaml = render(&MediaWarpConfig::default()).unwrap();
        let parsed = parse_into(&yaml, &base).unwrap();
        assert!(parsed.enable);
        assert_eq!(parsed.binary, "/opt/bin/mediawarp");
        assert_eq!(parsed.config_path, "/etc/mediawarp.yaml");
    }

    #[test]
    fn rejects_blank_and_broken_files() {
        assert!(parse("# 只有注释\n").is_err());
        assert!(parse("port: 不是端口\n").is_err());
    }
}
