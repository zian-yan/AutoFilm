use std::path::PathBuf;

use chrono_tz::Tz;
use clap::Parser;

#[derive(Debug, Parser)]
#[command(
    name = env!("CARGO_PKG_NAME"),
    disable_version_flag = true
)]
pub struct CliArgs {
    /// 是否启用调试日志
    #[arg(long, default_value_t = false)]
    pub debug: bool,

    /// 指定业务配置文件路径（允许为空：可只保留注释，或完全不存在）
    #[arg(
        long = "config",
        value_name = "PATH",
        default_value = "config/config.yaml"
    )]
    pub config_path: PathBuf,

    /// 指定 Web 面板配置文件路径（与业务配置完全独立，允许为空）
    #[arg(
        long = "web-config",
        value_name = "PATH",
        default_value = "config/web.yaml"
    )]
    pub web_config_path: PathBuf,

    /// 指定日志文件目录路径（为空表示禁用日志文件写入）
    #[arg(long = "log", value_name = "PATH", default_value = "logs")]
    pub log_path: String,

    /// 指定时区（默认依次读取 TZ 环境变量、系统本地时区，最后回退 UTC）
    /// 支持解析 IANA 时区字符串，如 "Asia/Shanghai"、"America/New_York"、 "UTC" 等
    #[arg(long, value_name = "TZ")]
    pub timezone: Option<Tz>,

    /// 是否启用彩色日志输出
    /// `true`（默认）表示启用彩色日志
    /// `false` 表示禁用彩色日志输出
    #[arg(long, default_value_t = true)]
    pub colorful_log: bool,

    /// 显示版本、Git 与编译信息
    #[arg(short = 'v', long = "version", default_value_t = false)]
    pub show_version: bool,
}

impl CliArgs {
    pub fn log_level(&self) -> tracing::Level {
        if self.debug {
            tracing::Level::DEBUG
        } else {
            tracing::Level::INFO
        }
    }

    /// 解析应用时区，优先级：`--timezone` > `TZ` 环境变量 > 系统时区 > UTC。
    ///
    /// 容器里镜像通常不写 `/etc/localtime`，靠系统时区探测只能拿到 UTC，
    /// 因此显式支持 `TZ=Asia/Shanghai` 这类 Docker 常见写法；`TZ` 允许带
    /// POSIX 风格的 `:` 前缀，空值视为未设置。
    pub fn app_timezone(&self) -> Tz {
        self.timezone
            .or_else(timezone_from_env)
            .or_else(|| iana_time_zone::get_timezone().ok()?.parse().ok())
            .unwrap_or(chrono_tz::UTC)
    }
}

/// 从 `TZ` 环境变量解析时区；无效值返回 `None`，交由系统时区兜底。
fn timezone_from_env() -> Option<Tz> {
    let raw = std::env::var("TZ").ok()?;
    parse_tz_name(&raw)
}

/// 解析 `TZ` 变量内容：允许空值与 POSIX 风格的 `:` 前缀；非法值返回 `None`，
/// 不会覆盖系统时区。
fn parse_tz_name(raw: &str) -> Option<Tz> {
    let name = raw.trim().trim_start_matches(':').trim();
    if name.is_empty() {
        return None;
    }
    name.parse().ok()
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::CliArgs;

    #[test]
    fn parses_debug_flag_and_config_path() {
        let args = CliArgs::parse_from(["autofilm", "--debug", "--config", "config/demo.yaml"]);
        assert!(args.debug);
        assert_eq!(
            args.config_path,
            std::path::PathBuf::from("config/demo.yaml")
        );
    }

    #[test]
    fn rejects_positional_config_path() {
        let result = CliArgs::try_parse_from(["autofilm", "config/demo.yaml"]);
        assert!(result.is_err());
    }

    #[test]
    fn defaults_config_path() {
        let args = CliArgs::parse_from(["autofilm"]);
        assert_eq!(
            args.config_path,
            std::path::PathBuf::from("config/config.yaml")
        );
    }

    #[test]
    fn defaults_log_path() {
        let args = CliArgs::parse_from(["autofilm"]);
        assert_eq!(args.log_path, std::path::PathBuf::from("logs"));
    }

    #[test]
    fn parses_custom_log_path() {
        let args = CliArgs::parse_from(["autofilm", "--log", "/tmp/autofilm-logs"]);
        assert_eq!(
            args.log_path,
            std::path::PathBuf::from("/tmp/autofilm-logs")
        );
    }

    #[test]
    fn defaults_timezone_to_none() {
        let args = CliArgs::parse_from(["autofilm"]);
        assert_eq!(args.timezone, None);
    }

    #[test]
    fn parses_cli_timezone_shanghai() {
        let args = CliArgs::parse_from(["autofilm", "--timezone", "Asia/Shanghai"]);
        assert_eq!(args.timezone, Some(chrono_tz::Asia::Shanghai));
    }

    #[test]
    fn parses_cli_timezone_new_york() {
        let args = CliArgs::parse_from(["autofilm", "--timezone", "America/New_York"]);
        assert_eq!(args.timezone, Some(chrono_tz::America::New_York));
    }

    #[test]
    fn parses_cli_timezone_utc() {
        let args = CliArgs::parse_from(["autofilm", "--timezone", "UTC"]);
        assert_eq!(args.timezone, Some(chrono_tz::UTC));
    }

    #[test]
    fn rejects_invalid_cli_timezone() {
        let result = CliArgs::try_parse_from(["autofilm", "--timezone", "Not/AZone"]);
        assert!(result.is_err());
    }

    #[test]
    fn parses_version_flag() {
        let args = CliArgs::parse_from(["autofilm", "--version"]);
        assert!(args.show_version);
    }

    #[test]
    fn parses_tz_env_values() {
        let shanghai = Some(chrono_tz::Asia::Shanghai);
        assert_eq!(super::parse_tz_name("Asia/Shanghai"), shanghai);
        assert_eq!(super::parse_tz_name(" Asia/Shanghai "), shanghai);
        // POSIX 写法允许 `:` 前缀，Docker 里偶尔会这么配。
        assert_eq!(super::parse_tz_name(":Asia/Shanghai"), shanghai);
        assert_eq!(super::parse_tz_name("UTC"), Some(chrono_tz::UTC));
        // 带斜杠的 IANA 名称必须原样解析成功：Git Bash 会丢弃这类值，
        // 但 Docker / Linux / PowerShell 下子进程收到的就是原字符串。
        assert_eq!(
            super::parse_tz_name("Europe/London"),
            Some(chrono_tz::Europe::London)
        );
        assert_eq!(
            super::parse_tz_name("America/New_York"),
            Some(chrono_tz::America::New_York)
        );
        assert_eq!(
            super::parse_tz_name(":Asia/Tokyo"),
            Some(chrono_tz::Asia::Tokyo)
        );
    }

    #[test]
    fn ignores_empty_or_invalid_tz_env_values() {
        assert_eq!(super::parse_tz_name(""), None);
        assert_eq!(super::parse_tz_name("   "), None);
        assert_eq!(super::parse_tz_name(":"), None);
        assert_eq!(super::parse_tz_name("Not/AZone"), None);
    }
}
