//! 统一的日志行格式：`11-07 12:21:02 消息 字段=值`。
//!
//! 目标是「一眼能扫」：INFO 行只有时间和消息，不带 target、行号和级别，
//! 避免真实内容被 `autofilm::alist2strm::runner:186` 这类前缀淹没；
//! WARN / ERROR 额外带级别标记（终端里再上色），异常行依然一眼可见。

use std::fmt;

use tracing::{Event, Level, Subscriber};
use tracing_subscriber::{
    fmt::{FmtContext, FormatEvent, FormatFields, format::Writer, time::FormatTime},
    registry::LookupSpan,
};

use super::ChronoTzTimer;

const WARN_COLOR: &str = "\x1b[33m";
const ERROR_COLOR: &str = "\x1b[31m";
const RESET: &str = "\x1b[0m";

/// 控制台 / 按天文件共用的格式化器。
pub(super) struct ReadableFormat {
    timer: ChronoTzTimer,
    /// 终端才上色；写文件时保持纯文本，避免日志里出现转义序列。
    ansi: bool,
}

impl ReadableFormat {
    pub(super) fn new(timer: ChronoTzTimer, ansi: bool) -> Self {
        Self { timer, ansi }
    }
}

impl<S, N> FormatEvent<S, N> for ReadableFormat
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
    N: for<'writer> FormatFields<'writer> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let level = *event.metadata().level();
        self.timer.format_time(&mut writer)?;
        writer.write_char(' ')?;

        if let Some(tag) = super::level_tag(&level) {
            if self.ansi {
                let color = match level {
                    Level::ERROR => ERROR_COLOR,
                    Level::WARN => WARN_COLOR,
                    _ => "",
                };
                write!(writer, "{color}{tag}{RESET} ")?;
            } else {
                write!(writer, "{tag} ")?;
            }
        }

        // 字段沿用 tracing 自带的渲染：消息在前，其余按声明顺序以 `name=value`
        // 追加，信息量不丢，只是把 target / 行号这类噪音拿掉了。
        ctx.field_format().format_fields(writer.by_ref(), event)?;
        writeln!(writer)
    }
}
