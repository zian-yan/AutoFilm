use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

use chrono::{Datelike, Days, NaiveDate, Utc};
use chrono_tz::Tz;

/// 日志文件保留天数，超期文件会在切换日志文件时清理。
///
/// 没有清理策略时长跑服务会被日志写满磁盘；7 天足够覆盖一次「上周同样的操作
/// 是不是也报过这个错」的排查，同时把磁盘占用压在一个可预期的小范围内。
const KEEP_DAYS: u64 = 7;

pub(super) struct DailyFileAppender {
    directory: PathBuf,
    timezone: Tz,
    current_date: Option<NaiveDate>,
    file: Option<File>,
}

impl DailyFileAppender {
    pub(super) fn new(directory: PathBuf, timezone: Tz) -> io::Result<Self> {
        fs::create_dir_all(&directory)?;
        prune_old_logs(&directory, KEEP_DAYS, timezone);
        // 旧版本把按天日志直接放在日志根目录（如 `logs/2026-11-01.log`）。升级后
        // 这些文件不再被写入，也不在新子目录的清理范围内；启动时对上一级补一次
        // 同样的清理，否则它们会永久留在磁盘上。只认 `YYYY-MM-DD.log`，天然不会
        // 碰到 MediaWarp 的日期子目录（无 `.log` 后缀）或其它文件。
        if let Some(parent) = directory.parent() {
            prune_old_logs(parent, KEEP_DAYS, timezone);
        }

        Ok(Self {
            directory,
            timezone,
            current_date: None,
            file: None,
        })
    }

    fn current_date(&self) -> NaiveDate {
        Utc::now().with_timezone(&self.timezone).date_naive()
    }

    fn ensure_file(&mut self) -> io::Result<&mut File> {
        let date = self.current_date();

        if self.current_date != Some(date) {
            let path = log_file_path(&self.directory, date);
            self.file = Some(OpenOptions::new().create(true).append(true).open(path)?);
            self.current_date = Some(date);
            // 每天切换文件时顺手清理过期日志。
            prune_old_logs(&self.directory, KEEP_DAYS, self.timezone);
        }

        self.file
            .as_mut()
            .ok_or_else(|| io::Error::other("daily log file was not opened"))
    }
}

impl Write for DailyFileAppender {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.ensure_file()?.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.file.as_mut() {
            Some(file) => file.flush(),
            None => Ok(()),
        }
    }
}

fn log_file_path(directory: &Path, date: NaiveDate) -> PathBuf {
    directory.join(format!(
        "{:04}-{:02}-{:02}.log",
        date.year(),
        date.month(),
        date.day()
    ))
}

/// 删除超过保留天数的日志文件。
///
/// 只处理文件名形如 `YYYY-MM-DD.log` 的文件，其它文件一律不动。错误会被
/// 静默忽略：这里运行在日志写入线程里，不能因为清理失败影响日志本身。
fn prune_old_logs(directory: &Path, keep_days: u64, timezone: Tz) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    let Some(cutoff) = Utc::now()
        .with_timezone(&timezone)
        .date_naive()
        .checked_sub_days(Days::new(keep_days))
    else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(date) = name
            .strip_suffix(".log")
            .and_then(|stem| NaiveDate::parse_from_str(stem, "%Y-%m-%d").ok())
        else {
            continue;
        };
        if date < cutoff {
            let _ = fs::remove_file(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "autofilm-log-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        directory
    }

    #[test]
    fn daily_log_file_path_uses_date_suffix() {
        let path = log_file_path(
            Path::new("/tmp/autofilm-logs"),
            NaiveDate::from_ymd_opt(2026, 6, 5).unwrap(),
        );

        assert_eq!(path, PathBuf::from("/tmp/autofilm-logs/2026-06-05.log"));
    }

    #[test]
    fn prunes_only_expired_log_files() {
        let directory = temp_dir("prune");

        let today = Utc::now().date_naive();
        let old = today - chrono::Duration::days(KEEP_DAYS as i64 + 5);
        let recent = today - chrono::Duration::days(1);
        let old_file = log_file_path(&directory, old);
        let recent_file = log_file_path(&directory, recent);
        let unrelated = directory.join("keep-me.txt");
        fs::write(&old_file, b"old").unwrap();
        fs::write(&recent_file, b"recent").unwrap();
        fs::write(&unrelated, b"other").unwrap();

        prune_old_logs(&directory, KEEP_DAYS, chrono_tz::UTC);

        assert!(!old_file.exists(), "expired log file should be removed");
        assert!(recent_file.exists(), "recent log file should be kept");
        assert!(unrelated.exists(), "unrelated file should be kept");

        fs::remove_dir_all(&directory).ok();
    }

    #[test]
    fn keeps_files_exactly_on_the_retention_boundary() {
        let directory = temp_dir("boundary");
        // 边界日（今天 - 7）仍然算「保留期内」，越界一天才删。
        let on_boundary = log_file_path(
            &directory,
            Utc::now().date_naive() - chrono::Duration::days(KEEP_DAYS as i64),
        );
        fs::write(&on_boundary, b"boundary").unwrap();

        prune_old_logs(&directory, KEEP_DAYS, chrono_tz::UTC);

        assert!(on_boundary.exists(), "第 {} 天的日志不应被删除", KEEP_DAYS);
        fs::remove_dir_all(&directory).ok();
    }

    #[test]
    fn startup_sweeps_expired_files_left_in_the_flat_layout() {
        let root = temp_dir("legacy");
        let today = Utc::now().date_naive();
        let old = log_file_path(&root, today - chrono::Duration::days(KEEP_DAYS as i64 + 3));
        let recent = log_file_path(&root, today - chrono::Duration::days(2));
        fs::write(&old, b"old").unwrap();
        fs::write(&recent, b"recent").unwrap();

        let _appender = DailyFileAppender::new(root.join("autofilm"), chrono_tz::UTC).unwrap();

        assert!(!old.exists(), "旧扁平布局里的过期日志应被清理");
        assert!(recent.exists(), "旧扁平布局里未过期的日志必须保留");
        assert!(root.join("autofilm").is_dir(), "新的按天日志子目录应被创建");

        fs::remove_dir_all(&root).ok();
    }
}
