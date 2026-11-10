use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Jellyfin,
    Emby,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    // 服务器 ID 用于 LibraryPoster 任务引用并复用 HTTP 客户端。
    pub id: String,
    pub kind: Kind,
    pub base_url: String,
    pub api_key: String,
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default = "default_timeout")]
    pub timeout: u64,
}

fn default_timeout() -> u64 {
    30
}

/// Jellyfin 的 `Authorization: MediaBrowser Token="..."` 用双引号包裹密钥，
/// 因此密钥不能含引号、逗号、反斜杠、空白或非 ASCII 字符。
/// Jellyfin/Emby 生成的 API Key 均为十六进制或 GUID，天然满足条件。
pub fn is_safe_api_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_media_server_config() {
        let config: Config = serde_yaml::from_str(
            r#"
id: 我的Jellyfin
kind: jellyfin
base_url: http://jellyfin:8096
api_key: secret
"#,
        )
        .unwrap();

        assert_eq!(config.kind, Kind::Jellyfin);
        assert_eq!(config.timeout, 30);
        assert!(config.user_id.is_none());
    }
}
