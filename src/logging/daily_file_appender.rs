use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

use chrono::{Datelike, Days, NaiveDate, Utc};
use chrono_tz::Tz;

/// 日志文件保留天数，超期文件会在切换日志文件时清理。
///
/// 没有清理策略时长跑服务会被日志写满磁盘，因此这里给出一个保守的默认值。
const KEEP_DAYS: u64 = 14;

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
        let directory = std::env::temp_dir().join(format!(
            "autofilm-log-prune-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();

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
}
