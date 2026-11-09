//! 任务级运行日志：按「模块:任务 ID」独立持久化，最多保留最新 1024 行。
//!
//! - 采集：[`TaskLogLayer`] 挂在全局 tracing subscriber 上，只把发生在
//!   [`task_span`] 作用域内的事件写入对应任务的日志文件；全局日志仍走原来的
//!   控制台 / 按天文件输出，互不干扰。
//! - 存储：每个任务一个文件，以行为单位追加；超过 1024 行时删掉最早一行并
//!   回写文件，因此内存与磁盘都始终只保留最近 1024 行。
//! - 读取：[`TaskLogStore::read`] 首次访问时从磁盘恢复，服务重启后仍可查看。

use std::{
    collections::{HashMap, VecDeque},
    fmt::Debug,
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{Duration, SystemTime},
};

use chrono::Utc;
use chrono_tz::Tz;
use tracing::{
    Event, Id, Subscriber,
    field::{Field, Visit},
    span::Attributes,
};
use tracing_subscriber::{
    layer::{Context, Layer},
    registry::LookupSpan,
};

/// 单个任务最多保留的日志行数。
pub const MAX_LINES: usize = 1024;
/// 孤儿任务日志文件的保留天数，与全局按天日志的清理策略一致。
const LOG_KEEP_DAYS: u64 = 14;
/// tracing span 上携带任务键的字段名。
const KEY_FIELD: &str = "task_log_key";

static STORE: OnceLock<TaskLogStore> = OnceLock::new();

/// 初始化全局任务日志存储；重复调用只有第一次生效。
pub fn init(root: impl Into<PathBuf>, tz: Tz) -> io::Result<()> {
    let root = root.into();
    fs::create_dir_all(&root)?;
    let _ = STORE.set(TaskLogStore::new(root, tz));
    Ok(())
}

/// 取得全局存储；未初始化（例如只跑单元测试）时返回 `None`。
pub fn get() -> Option<&'static TaskLogStore> {
    STORE.get()
}

/// 热重载后清理陈旧缓存；未初始化时为空操作。
pub fn prune(active_keys: &[String]) {
    if let Some(store) = get() {
        store.prune(active_keys);
    }
}

/// 创建带任务键的 span，任务执行体需要 `.instrument()` 到它上面。
///
/// 手动触发与 cron 触发共用同一个 [`crate::schedule::RunFn`]，因此只要把执行
/// 体挂到这个 span 上，两类触发产生的日志都会归到同一个任务。
pub fn task_span(key: &str) -> tracing::Span {
    tracing::info_span!("task_run", task_log_key = key)
}

/// 拼接「模块:任务 ID」，与调度闸门、面板接口使用同一个键。
pub fn task_key(module: &str, id: &str) -> String {
    format!("{module}:{id}")
}

/* ---------------- 存储 ---------------- */

pub struct TaskLogStore {
    root: PathBuf,
    tz: Tz,
    buffers: Mutex<HashMap<String, VecDeque<String>>>,
}

impl TaskLogStore {
    pub fn new(root: PathBuf, tz: Tz) -> Self {
        Self {
            root,
            tz,
            buffers: Mutex::new(HashMap::new()),
        }
    }

    /// 与面板其它日志一致，使用应用时区；样式与控制台 / 按天文件保持统一
    /// （`11-07 12:21:02`）。
    fn timestamp(&self) -> String {
        Utc::now()
            .with_timezone(&self.tz)
            .format("%m-%d %H:%M:%S")
            .to_string()
    }

    fn path_for(root: &Path, key: &str) -> PathBuf {
        root.join(format!("{}-{:016x}.log", safe_prefix(key), fnv1a64(key)))
    }

    /// 从磁盘恢复；历史文件若曾超过上限，加载时立即裁到 1024 行。
    fn load_from_disk(root: &Path, key: &str) -> VecDeque<String> {
        let path = Self::path_for(root, key);
        let Ok(content) = fs::read_to_string(&path) else {
            return VecDeque::new();
        };
        let mut lines: VecDeque<String> = content.lines().map(str::to_string).collect();
        if lines.len() > MAX_LINES {
            while lines.len() > MAX_LINES {
                lines.pop_front();
            }
            let _ = write_lines(&path, &lines);
        }
        lines
    }

    /// 追加一行日志；达到上限时先删最早一行，再把当前 1024 行回写文件。
    pub fn append(&self, key: &str, line: &str) {
        let clean = sanitize_line(line);
        let path = Self::path_for(&self.root, key);
        let mut buffers = self.buffers.lock().unwrap_or_else(|err| err.into_inner());
        let lines = buffers
            .entry(key.to_string())
            .or_insert_with(|| Self::load_from_disk(&self.root, key));

        let trimmed = lines.len() >= MAX_LINES;
        if trimmed {
            lines.pop_front();
        }
        lines.push_back(clean.clone());

        // 这里刻意不用 tracing 宏：写盘失败时若再发事件，会在 Layer 里递归。
        if let Err(err) = append_line(&path, &clean) {
            eprintln!("写入任务日志失败 ({}): {err}", path.display());
        }
        if trimmed {
            if let Err(err) = write_lines(&path, lines) {
                eprintln!("回写任务日志失败 ({}): {err}", path.display());
            }
        }
    }

    /// 按时间顺序返回该任务的全部日志行；任务从未运行过时返回空数组。
    ///
    /// 只读查询**绝不**为未知 key 创建缓存条目：`/api/task-log` 的 key 来自用户
    /// 输入，若为每个查询过的 key 都插入一条（哪怕是空 VecDeque），HashMap 会
    /// 随请求无限增长。未命中时在锁外查磁盘，只有确实存在日志文件的任务才进入
    /// 缓存；不存在的文件直接返回空，既不建条目也不反复读盘。
    pub fn read(&self, key: &str) -> Vec<String> {
        let cached = {
            let buffers = self.buffers.lock().unwrap_or_else(|err| err.into_inner());
            buffers
                .get(key)
                .map(|lines| lines.iter().cloned().collect::<Vec<_>>())
        };
        if let Some(lines) = cached {
            return lines;
        }

        if !Self::path_for(&self.root, key).is_file() {
            return Vec::new();
        }
        let lines = Self::load_from_disk(&self.root, key);
        let result: Vec<String> = lines.iter().cloned().collect();
        if !result.is_empty() {
            let mut buffers = self.buffers.lock().unwrap_or_else(|err| err.into_inner());
            buffers.entry(key.to_string()).or_insert(lines);
        }
        result
    }

    /// 热重载后按当前配置的任务键清理内存缓存，并删除超过保留天数的孤儿日志。
    ///
    /// 磁盘文件是持久化权威，这里只清内存条目与「很久没更新」的文件：刚从配置
    /// 里删掉的任务日志仍保留一段时间，需要时 `read()` 会从磁盘恢复。
    pub fn prune(&self, active_keys: &[String]) {
        {
            let mut buffers = self.buffers.lock().unwrap_or_else(|err| err.into_inner());
            buffers.retain(|key, _| active_keys.iter().any(|active| active == key));
        }

        let Some(cutoff) =
            SystemTime::now().checked_sub(Duration::from_secs(LOG_KEEP_DAYS * 86400))
        else {
            return;
        };
        let Ok(entries) = fs::read_dir(&self.root) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("log") {
                continue;
            }
            let Ok(modified) = entry.metadata().and_then(|meta| meta.modified()) else {
                continue;
            };
            if modified < cutoff && fs::remove_file(&path).is_err() {
                // 刻意不用 tracing 宏：避免与日志采集链路相互触发。
                eprintln!("清理过期任务日志失败 ({})", path.display());
            }
        }
    }
}

fn append_line(path: &Path, line: &str) -> io::Result<()> {
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(file, "{line}")
}

fn write_lines(path: &Path, lines: &VecDeque<String>) -> io::Result<()> {
    let mut file = fs::File::create(path)?;
    for line in lines {
        writeln!(file, "{line}")?;
    }
    Ok(())
}

/// 一条日志必须只占一行，避免多行消息破坏「按行裁剪」规则。
fn sanitize_line(line: &str) -> String {
    line.replace('\r', " ").replace('\n', " ")
}

/// 任务 ID 可能含中文、冒号、斜杠；保留可读前缀，再用稳定哈希保证唯一且
/// 不会出现路径分隔符，重启后仍能定位同一个文件。
fn safe_prefix(key: &str) -> String {
    let mut out = String::new();
    for ch in key.chars() {
        if ch.is_ascii_alphanumeric()
            || ch == '-'
            || ch == '_'
            || (!ch.is_ascii() && !ch.is_control())
        {
            out.push(ch);
        } else {
            out.push('_');
        }
        if out.chars().count() >= 32 {
            break;
        }
    }
    let trimmed = out.trim_matches('_').to_string();
    if trimmed.is_empty() {
        "task".to_string()
    } else {
        trimmed
    }
}

/// FNV-1a 64 位：确定性哈希，跨进程 / 跨重启结果一致。
fn fnv1a64(text: &str) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/* ---------------- tracing 采集 ---------------- */

#[derive(Clone)]
struct SpanKey(String);

#[derive(Default)]
struct EventVisitor {
    key: Option<String>,
    message: String,
    fields: Vec<String>,
}

fn unquote(text: &str) -> String {
    if text.len() >= 2 && text.starts_with('"') && text.ends_with('"') {
        text[1..text.len() - 1].to_string()
    } else {
        text.to_string()
    }
}

impl Visit for EventVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
        let text = unquote(&format!("{value:?}"));
        match field.name() {
            KEY_FIELD => self.key = Some(text),
            "message" => self.message = text,
            name => self.fields.push(format!("{name}={text}")),
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            KEY_FIELD => self.key = Some(value.to_string()),
            "message" => self.message = value.to_string(),
            name => self.fields.push(format!("{name}={value}")),
        }
    }
}

/// 只把带任务键的 span 内的事件写入对应任务日志；其它事件由原有 layer 处理。
pub struct TaskLogLayer;

impl<S> Layer<S> for TaskLogLayer
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let mut visitor = EventVisitor::default();
        attrs.record(&mut visitor);
        let Some(key) = visitor.key else { return };
        if let Some(span) = ctx.span(id) {
            span.extensions_mut().insert(SpanKey(key));
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let Some(store) = get() else { return };
        let Some(scope) = ctx.event_scope(event) else {
            return;
        };

        // Scope 从最内层 span 向外迭代，取第一个带任务键的祖先。
        let mut key = None;
        for span in scope {
            let extensions = span.extensions();
            if let Some(found) = extensions.get::<SpanKey>() {
                key = Some(found.0.clone());
                break;
            }
        }
        let Some(key) = key else { return };

        let mut visitor = EventVisitor::default();
        event.record(&mut visitor);
        let meta = event.metadata();
        let timestamp = store.timestamp();
        // 与全局日志同款样式：`11-07 12:21:02 消息 字段=值`，非 INFO 才带级别标记。
        let mut line = timestamp;
        line.push(' ');
        if let Some(tag) = crate::logging::level_tag(meta.level()) {
            line.push_str(tag);
            line.push(' ');
        }
        line.push_str(&visitor.message);
        if !visitor.fields.is_empty() {
            line.push(' ');
            line.push_str(&visitor.fields.join(" "));
        }
        store.append(&key, &line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("autofilm-task-log-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn line_text(index: usize) -> String {
        format!("line-{index:04}")
    }

    #[test]
    fn empty_task_has_no_lines() {
        let root = temp_root("empty");
        let store = TaskLogStore::new(root.clone(), chrono_tz::UTC);
        assert!(store.read("alist2strm:从未运行").is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn continuous_append_keeps_chronological_order() {
        let root = temp_root("order");
        let store = TaskLogStore::new(root.clone(), chrono_tz::UTC);
        let key = "alist2strm:国产剧";
        for index in 0..12 {
            store.append(key, &line_text(index));
            // 运行途中也能读到已追加的全部内容，顺序不乱。
            let visible = store.read(key);
            assert_eq!(visible.len(), index + 1);
            assert_eq!(visible[index], line_text(index));
        }
        assert_eq!(store.read(key)[0], line_text(0));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn running_task_log_grows_and_stays_readable() {
        let root = temp_root("running");
        let store = TaskLogStore::new(root.clone(), chrono_tz::UTC);
        let key = "alist2strm:运行中任务";
        // 任务执行期间持续追加，面板每次轮询都应看到按序增长的全部行。
        let mut snapshots = Vec::new();
        for index in 0..5 {
            store.append(key, &format!("运行中-{index}"));
            snapshots.push(store.read(key).len());
        }
        assert_eq!(snapshots, vec![1, 2, 3, 4, 5]);
        let lines = store.read(key);
        assert_eq!(lines[0], "运行中-0");
        assert_eq!(lines[4], "运行中-4");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn caps_at_1024_lines_and_keeps_newest() {
        let root = temp_root("cap");
        let store = TaskLogStore::new(root.clone(), chrono_tz::UTC);
        let key = "library_poster:媒体库海报";
        let total = MAX_LINES + 37;
        for index in 0..total {
            store.append(key, &line_text(index));
        }

        let lines = store.read(key);
        assert_eq!(lines.len(), MAX_LINES, "始终只保留最近 1024 行");
        assert_eq!(lines[0], line_text(total - MAX_LINES), "最早的行已被删除");
        assert_eq!(lines[MAX_LINES - 1], line_text(total - 1));
        for (offset, line) in lines.iter().enumerate() {
            assert_eq!(*line, line_text(total - MAX_LINES + offset), "顺序必须连续");
        }

        // 磁盘文件同样不超过上限，重启后不会长出多余的行。
        let path = TaskLogStore::path_for(&root, key);
        let disk_lines = fs::read_to_string(path).unwrap().lines().count();
        assert_eq!(disk_lines, MAX_LINES);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn lines_survive_store_reopen() {
        let root = temp_root("restart");
        let key = "ani2alist:新番追更";
        {
            let store = TaskLogStore::new(root.clone(), chrono_tz::UTC);
            for index in 0..MAX_LINES + 5 {
                store.append(key, &line_text(index));
            }
        }
        // 模拟服务重启：内存缓冲全部丢失，只从磁盘恢复。
        let reopened = TaskLogStore::new(root.clone(), chrono_tz::UTC);
        let lines = reopened.read(key);
        assert_eq!(lines.len(), MAX_LINES);
        assert_eq!(lines[0], line_text(5));
        assert_eq!(lines[MAX_LINES - 1], line_text(MAX_LINES + 4));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn tasks_are_isolated() {
        let root = temp_root("isolated");
        let store = TaskLogStore::new(root.clone(), chrono_tz::UTC);
        store.append("alist2strm:任务A", "A-1");
        store.append("alist2strm:任务B", "B-1");
        store.append("alist2strm:任务A", "A-2");
        assert_eq!(store.read("alist2strm:任务A"), vec!["A-1", "A-2"]);
        assert_eq!(store.read("alist2strm:任务B"), vec!["B-1"]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn special_characters_in_key_are_safe() {
        let root = temp_root("special");
        let store = TaskLogStore::new(root.clone(), chrono_tz::UTC);
        let key = "alist2strm:../../国产剧/第一季:abc";
        store.append(key, "第一行\n第二行\r第三行");
        let lines = store.read(key);
        assert_eq!(lines.len(), 1, "多行消息必须压成一行");
        assert_eq!(lines[0], "第一行 第二行 第三行");
        let path = TaskLogStore::path_for(&root, key);
        assert_eq!(path.parent(), Some(root.as_path()), "文件名不能越出目录");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn read_never_creates_entries_for_unknown_keys() {
        let root = temp_root("read-no-insert");
        let store = TaskLogStore::new(root.clone(), chrono_tz::UTC);
        // 模拟 /api/task-log 收到大量各不相同的任务 ID。
        for index in 0..200 {
            assert!(
                store
                    .read(&format!("alist2strm:从未运行-{index}"))
                    .is_empty()
            );
        }
        assert!(
            store.buffers.lock().unwrap().is_empty(),
            "只读查询不得往 buffers 插入任何条目"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn read_caches_only_existing_logs_and_prune_drops_stale_entries() {
        let root = temp_root("prune");
        {
            let store = TaskLogStore::new(root.clone(), chrono_tz::UTC);
            store.append("alist2strm:保留", "line-1");
            store.append("alist2strm:已删除", "line-1");
        }
        // 模拟重启：内存缓冲为空，只有磁盘上真有日志的 key 才会被重新缓存。
        let reopened = TaskLogStore::new(root.clone(), chrono_tz::UTC);
        assert_eq!(reopened.read("alist2strm:保留").len(), 1);
        assert_eq!(reopened.read("alist2strm:已删除").len(), 1);
        assert!(reopened.read("alist2strm:从未运行").is_empty());
        assert_eq!(reopened.buffers.lock().unwrap().len(), 2);

        reopened.prune(&["alist2strm:保留".to_string()]);
        {
            let buffers = reopened.buffers.lock().unwrap();
            assert_eq!(buffers.len(), 1);
            assert!(buffers.contains_key("alist2strm:保留"));
        }
        // 内存条目被淘汰，但磁盘历史不丢，下次读取仍能从磁盘恢复。
        assert_eq!(reopened.read("alist2strm:已删除"), vec!["line-1"]);
        fs::remove_dir_all(root).unwrap();
    }
}
