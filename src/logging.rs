mod daily_file_appender;
mod format;

use chrono::Utc;
use chrono_tz::Tz;
use daily_file_appender::DailyFileAppender;
use format::ReadableFormat;
use tracing::{Level, info, level_filters::LevelFilter, warn};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{
    Layer, fmt::time::FormatTime, layer::SubscriberExt, util::SubscriberInitExt,
};

use crate::task_log::TaskLogLayer;

/// 日志时间样式：`11-07 12:21:02`。
///
/// 不带年份是刻意的——按天日志的文件名已经是 `YYYY-MM-DD.log`，行内再重复年份
/// 只会挤占正文宽度；不带毫秒则让同一秒内的批量输出保持对齐。
const TIME_FORMAT: &str = "%m-%d %H:%M:%S";

/// 自定义一个结构体，用来存放 chrono_tz 的时区
#[derive(Debug, Clone)]
struct ChronoTzTimer {
    timezone: chrono_tz::Tz,
}

impl ChronoTzTimer {
    fn new(timezone: chrono_tz::Tz) -> Self {
        Self { timezone }
    }
}

// 为我们的结构体实现 tracing-subscriber 的 FormatTime 特征
impl FormatTime for ChronoTzTimer {
    fn format_time(&self, w: &mut tracing_subscriber::fmt::format::Writer<'_>) -> std::fmt::Result {
        // 1. 获取当前的 UTC 时间
        let utc_now = Utc::now();

        // 2. 将 UTC 时间转换为经由 chrono_tz::Tz 指定的时区时间
        let local_now = utc_now.with_timezone(&self.timezone);

        // 3. 格式化为你想要的字符串样式（例如: 11-07 12:21:02）
        write!(w, "{}", local_now.format(TIME_FORMAT))
    }
}

/// 非 INFO 级别才带标记，INFO 行保持 `11-07 12:21:02 消息` 的简洁样式。
///
/// 任务日志（[`crate::task_log`]）复用同一个规则，保证面板里看到的行与
/// 控制台 / 文件完全一致。
pub(crate) fn level_tag(level: &Level) -> Option<&'static str> {
    match *level {
        Level::ERROR => Some("[ERROR]"),
        Level::WARN => Some("[WARN]"),
        Level::INFO => None,
        Level::DEBUG => Some("[DEBUG]"),
        Level::TRACE => Some("[TRACE]"),
    }
}

/// 级别过滤 + 压掉第三方库的 INFO 噪音。
///
/// cron 调度器会打 `Uninited`、`Job creator created` 这类没有上下文的行，
/// 在「时间 + 消息」的极简格式里只剩一个孤零零的单词，容易让人误以为是
/// 自家日志，所以这里统一把它的 INFO 及以下丢掉。
#[derive(Clone, Copy)]
struct QuietFilter(LevelFilter);

impl<S: tracing::Subscriber> tracing_subscriber::layer::Filter<S> for QuietFilter {
    fn enabled(
        &self,
        meta: &tracing::Metadata<'_>,
        _cx: &tracing_subscriber::layer::Context<'_, S>,
    ) -> bool {
        LevelFilter::from_level(*meta.level()) <= self.0
            && !meta.target().starts_with("tokio_cron_scheduler")
    }
}

pub struct LoggingGuard {
    _file_guard: WorkerGuard,
}

/// 初始化日志系统，支持控制台和文件输出。返回一个 `LoggingGuard` 以保持文件 appender 的生命周期。
/// `level` 决定了日志的详细程度
/// `log_path` 指定了日志文件的目录（为空表示禁用文件输出）
/// `tz` 指定使用时区
pub fn init(
    level: Level,
    log_path: &str,
    tz: Tz,
    colorful_log: bool,
) -> Result<Option<LoggingGuard>, Box<dyn std::error::Error + Send + Sync>> {
    let level_filter = LevelFilter::from_level(level);
    let tz_timer = ChronoTzTimer::new(tz);
    let filter = QuietFilter(level_filter);

    let subscriber = tracing_subscriber::registry();

    let console_layer = tracing_subscriber::fmt::layer()
        .event_format(ReadableFormat::new(tz_timer.clone(), colorful_log))
        .with_ansi(colorful_log)
        .with_filter(filter.clone());

    // 任务级日志与控制台/按天文件并行采集：只有带任务 span 的事件才会落到
    // 对应任务的日志文件里，其余事件不受影响。
    let subscriber = subscriber
        .with(console_layer)
        .with(TaskLogLayer.with_filter(filter.clone()));

    if log_path.is_empty() {
        subscriber.init();
        warn!("日志文件输出已禁用");
        return Ok(None);
    }

    let log_path = std::path::absolute(log_path)?;
    std::fs::create_dir_all(&log_path)?;

    let file_appender = DailyFileAppender::new(log_path.clone(), tz)?;
    let (file_writer, file_guard) = tracing_appender::non_blocking(file_appender);

    let file_layer = tracing_subscriber::fmt::layer()
        .event_format(ReadableFormat::new(tz_timer, false))
        .with_ansi(false)
        .with_writer(file_writer)
        .with_filter(filter.clone());

    subscriber.with(file_layer).init();

    info!(log_path = %log_path.display(), "日志文件输出已启用");

    Ok(Some(LoggingGuard {
        _file_guard: file_guard,
    }))
}
