//! MediaWarp 落盘日志的读取（面板「运行日志」弹窗的数据源）。
//!
//! MediaWarp 自己按天分目录写 `<工作目录>/logs/mediawarp/<YYYY-MM-DD>/`，里面
//! 只放 `service.log`（启动、配置、错误）与 `access.log`（请求级访问日志）两个
//! 文件。面板不另存一份副本，直接读这些文件，因此弹窗里看到的内容与
//! `tail -f` 完全一致，也不存在「面板日志」与「真实日志」对不上的问题。
//!
//! 读取只做三件事：定位最近一天的分目录、从文件尾部读一段、按行切分。访问日志
//! 在图片/字幕请求密集时增长很快，所以既能按字节上限截断，也能按行数截断。

use std::{
    fs::{self, File},
    io::{self, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

use chrono::NaiveDate;
use serde::Serialize;

/// 单次最多返回的行数：足够覆盖一次排障窗口，又不至于让每轮轮询都搬运整份文件。
pub const MAX_LINES: usize = 2000;

/// 单次最多读取的字节数。几百 MB 的访问日志不能整份读进内存，只从尾部读这么多。
const MAX_BYTES: u64 = 4 * 1024 * 1024;

/// 以天为单位写出、且能明确解析成日期的最小宽度：`2026-01-02`。
const DAY_LAYOUT: &str = "%Y-%m-%d";

/// 面板可切换的日志来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// `service.log`：启动、配置、错误等运行日志。
    Service,
    /// `access.log`：请求级访问日志，量很大。
    Access,
}

impl Source {
    /// 解析请求体里的 `source` 字段；缺省为服务日志。
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "" | "service" => Some(Self::Service),
            "access" => Some(Self::Access),
            _ => None,
        }
    }

    pub fn file_name(self) -> &'static str {
        match self {
            Self::Service => "service.log",
            Self::Access => "access.log",
        }
    }
}

/// 一次读取的结果快照。
#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub source: Source,
    /// 实际读到的文件绝对路径；当天还没有日志（或文件被删）时为 `None`。
    pub path: Option<String>,
    /// 日志所在的分目录名（`YYYY-MM-DD`），用于在弹窗上标注是哪一天的日志。
    pub day: Option<String>,
    pub lines: Vec<String>,
    /// 是否因为上限丢掉了更早的内容。
    pub truncated: bool,
}

/// 读取 `dir` 下最近一天里指定来源的日志。
///
/// 找不到日期分目录、或分目录里没有该文件时返回空行集而不是错误：这两种情况在
/// 首次启动、切换 `log.*.file` 开关后都属于正常状态，应由面板显示空态提示。
pub fn read(dir: &Path, source: Source) -> Snapshot {
    let Some((day_dir, day)) = latest_day_dir(dir) else {
        return Snapshot {
            source,
            path: None,
            day: None,
            lines: Vec::new(),
            truncated: false,
        };
    };

    let path = day_dir.join(source.file_name());
    let Ok((lines, truncated)) = tail_lines(&path, MAX_LINES, MAX_BYTES) else {
        return Snapshot {
            source,
            path: None,
            day: Some(day),
            lines: Vec::new(),
            truncated: false,
        };
    };

    Snapshot {
        source,
        path: Some(path.display().to_string()),
        day: Some(day),
        lines,
        truncated,
    }
}

/// 找出 `dir` 下日期最新的分目录，返回目录路径与其名字。
///
/// 按解析出的日期比较而不是按名字排序：旧版本 MediaWarp 用 `2006-1-2` 写目录名，
/// 字符串序会把 `2026-1-9` 排到 `2026-1-10` 之后，日期比较才正确。
fn latest_day_dir(dir: &Path) -> Option<(PathBuf, String)> {
    let entries = fs::read_dir(dir).ok()?;
    let mut best: Option<(NaiveDate, PathBuf, String)> = None;

    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let Some(date) = NaiveDate::parse_from_str(&name, DAY_LAYOUT).ok() else {
            continue;
        };
        if best.as_ref().is_none_or(|(current, _, _)| date > *current) {
            best = Some((date, entry.path(), name));
        }
    }

    best.map(|(_, path, name)| (path, name))
}

/// 从文件尾部读出最近的若干行，返回 `(行, 是否截断)`。
///
/// 先按字节上限定位起点，再按行数裁剪；因为起点很可能落在半行中间（甚至在多字节
/// 字符中间），所以第一行整行丢弃——那半行本来就不完整，显示出来只会误导。
fn tail_lines(path: &Path, max_lines: usize, max_bytes: u64) -> io::Result<(Vec<String>, bool)> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    let skip_bytes = len > max_bytes;
    if skip_bytes {
        file.seek(SeekFrom::Start(len - max_bytes))?;
    }

    let mut bytes = Vec::new();
    file.take(max_bytes).read_to_end(&mut bytes)?;
    // 非法字节用替换字符兜底：日志里混进半截多字节字符时不能让整个弹窗报错。
    let text = String::from_utf8_lossy(&bytes);
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    if skip_bytes && !lines.is_empty() {
        lines.remove(0);
    }

    let dropped = lines.len().saturating_sub(max_lines);
    if dropped > 0 {
        lines.drain(..dropped);
    }

    Ok((lines, skip_bytes || dropped > 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "autofilm-mw-logs-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn write_day(root: &Path, day: &str, file: &str, body: &str) -> PathBuf {
        let dir = root.join(day);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(file);
        fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn reads_service_log_from_the_latest_day() {
        let root = temp_dir("latest");
        write_day(&root, "2026-11-18", "service.log", "old\n");
        write_day(&root, "2026-11-19", "service.log", "line-1\nline-2\n");

        let snapshot = read(&root, Source::Service);
        assert_eq!(snapshot.day.as_deref(), Some("2026-11-19"));
        assert_eq!(snapshot.lines, vec!["line-1", "line-2"]);
        assert!(!snapshot.truncated);
        assert!(snapshot.path.unwrap().ends_with("service.log"));

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn picks_the_newest_day_even_with_unpadded_folder_names() {
        let root = temp_dir("unpadded");
        // 旧版本 MediaWarp 的目录名不带前导零，字符串排序会把它误判成最新的。
        write_day(&root, "2026-1-9", "service.log", "jan-9\n");
        write_day(&root, "2026-1-10", "service.log", "jan-10\n");

        let snapshot = read(&root, Source::Service);
        assert_eq!(snapshot.day.as_deref(), Some("2026-1-10"));
        assert_eq!(snapshot.lines, vec!["jan-10"]);

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn missing_directory_or_file_is_an_empty_snapshot() {
        let root = temp_dir("empty");
        assert!(read(&root, Source::Service).lines.is_empty());

        write_day(&root, "2026-11-19", "service.log", "only-service\n");
        let access = read(&root, Source::Access);
        assert!(access.lines.is_empty());
        assert!(access.path.is_none(), "文件不存在时不应给出路径");
        assert_eq!(access.day.as_deref(), Some("2026-11-19"));

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn keeps_only_the_newest_lines_when_over_the_line_limit() {
        let root = temp_dir("lines");
        let body: String = (0..MAX_LINES + 10).map(|i| format!("line-{i}\n")).collect();
        write_day(&root, "2026-11-19", "access.log", &body);

        let snapshot = read(&root, Source::Access);
        assert_eq!(snapshot.lines.len(), MAX_LINES);
        assert!(snapshot.truncated);
        assert_eq!(snapshot.lines[0], "line-10");
        assert_eq!(
            snapshot.lines[MAX_LINES - 1],
            format!("line-{}", MAX_LINES + 9)
        );

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn drops_the_partial_first_line_when_truncating_by_bytes() {
        let path =
            std::env::temp_dir().join(format!("autofilm-mw-logs-tail-{}.log", std::process::id()));
        // 每行 11 字节（含换行）：`line-0000\n`。
        let body: String = (0..20).map(|i| format!("line-{i:04}\n")).collect();
        fs::write(&path, body).unwrap();

        let (lines, truncated) = tail_lines(&path, 100, 45).unwrap();

        assert!(truncated);
        // 45 字节从半行处开始，那半行必须被丢掉，其余各行保持完整。
        assert!(lines.iter().all(|line| line.starts_with("line-")));
        assert_eq!(lines.last().unwrap(), "line-0019");
        assert!(lines.len() <= 4);

        fs::remove_file(&path).ok();
    }

    #[test]
    fn source_parsing_defaults_to_service() {
        assert_eq!(Source::parse(""), Some(Source::Service));
        assert_eq!(Source::parse(" Service "), Some(Source::Service));
        assert_eq!(Source::parse("ACCESS"), Some(Source::Access));
        assert_eq!(Source::parse("stdout"), None);
    }
}
