//! 待重扫目录记录：本轮扫描 / 处理失败的目录，下次运行无条件重扫。
//!
//! 「目录时间检查」会让没变化的目录整棵跳过，但如果上一轮在某个目录里失败了
//! （列目录失败、或目录里某个文件没生成成功），跳过就等于永久漏文件。因此这里
//! 把失败目录持久化到目标目录下的状态文件，下次运行对它们（及其子树）强制扫描。
//!
//! 记录的生命周期是「以本轮结果为准」：任务结束时用本轮失败集合覆盖写盘，
//! 本轮全部成功则直接删除状态文件，避免旧记录一直生效。

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use tokio::fs;
use tracing::warn;

use crate::alist2strm::{errors::Result, protection::sanitize_state_id};

/// 状态文件里最多记录的目录数量。
///
/// 这是本地有界缓存：异常场景（例如 AList 整站不可用）下宁可丢一部分记录、
/// 下次多扫一遍，也不要让状态文件无上限膨胀。
const MAX_ENTRIES: usize = 5_000;

#[derive(Debug, Default, Deserialize, Serialize)]
struct RetryState {
    /// 记录写入时间（Unix 秒），仅用于排查。
    updated: String,
    /// 远端目录路径；按字符串排序，保证文件内容稳定可 diff。
    dirs: Vec<String>,
}

/// 上次任务留下的失败目录集合。
#[derive(Debug)]
pub(super) struct RetryDirs {
    state_file: PathBuf,
    dirs: HashSet<String>,
}

impl RetryDirs {
    /// 读取上次任务留下的待重扫目录。
    ///
    /// 文件不存在属于正常情况（上次全部成功）；解析失败只告警并按空集合处理，
    /// 不能因为一个坏状态文件阻止同步任务运行。
    pub(super) async fn load(target_dir: &Path, task_id: &str) -> Self {
        let state_file = state_file_path(target_dir, task_id);
        let dirs = match fs::read(&state_file).await {
            Ok(content) => match serde_json::from_slice::<RetryState>(&content) {
                Ok(state) => state.dirs.into_iter().collect(),
                Err(err) => {
                    warn!(
                        "待重扫记录解析失败，本轮按无记录处理（{}）: {err}",
                        state_file.display()
                    );
                    HashSet::new()
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => HashSet::new(),
            Err(err) => {
                warn!("读取待重扫记录失败（{}）: {err}", state_file.display());
                HashSet::new()
            }
        };
        Self { state_file, dirs }
    }

    /// 空集合，供测试构造 `RunContext` 使用。
    #[cfg(test)]
    pub(super) fn empty() -> Self {
        Self {
            state_file: PathBuf::new(),
            dirs: HashSet::new(),
        }
    }

    /// 该目录（或整棵子树）是否需要无条件扫描。
    pub(super) fn contains(&self, dir: &str) -> bool {
        self.dirs.contains(dir)
    }

    /// 该目录的子树里是否还有待重扫的目录。
    ///
    /// 用于避免「父目录被目录时间检查跳过 → 失败过的子目录根本没被发现」：
    /// 只要子孙里有待重扫记录，父目录这一轮就必须老老实实列一遍。
    pub(super) fn has_pending_under(&self, dir: &str) -> bool {
        let prefix = format!("{}/", dir.trim_end_matches('/'));
        self.dirs.iter().any(|pending| pending.starts_with(&prefix))
    }

    pub(super) fn is_empty(&self) -> bool {
        self.dirs.is_empty()
    }

    pub(super) fn len(&self) -> usize {
        self.dirs.len()
    }

    /// 用本轮失败集合覆盖记录。
    ///
    /// 集合为空表示本轮没有失败目录，直接删除状态文件，避免上一轮的记录继续
    /// 影响后续运行。写入同样走「临时文件 + rename」，防止中断留下半截 JSON。
    pub(super) async fn replace(&self, failed: &HashSet<String>) -> Result<()> {
        if failed.is_empty() {
            match fs::remove_file(&self.state_file).await {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => warn!("删除待重扫记录失败（{}）: {err}", self.state_file.display()),
            }
            return Ok(());
        }

        let mut dirs: Vec<String> = failed.iter().cloned().collect();
        dirs.sort();
        if dirs.len() > MAX_ENTRIES {
            warn!(
                kept = MAX_ENTRIES,
                total = dirs.len(),
                "待重扫目录过多，已截断"
            );
            dirs.truncate(MAX_ENTRIES);
        }

        let state = RetryState {
            updated: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_secs().to_string())
                .unwrap_or_else(|_| "0".to_string()),
            dirs,
        };

        if let Some(parent) = self.state_file.parent() {
            fs::create_dir_all(parent).await?;
        }
        let temp_file = self.state_file.with_extension("tmp");
        fs::write(&temp_file, serde_json::to_vec_pretty(&state)?).await?;
        fs::rename(&temp_file, &self.state_file).await?;
        Ok(())
    }
}

/// 状态文件路径：放在目标目录下，文件名带任务 ID，多任务互不干扰。
///
/// 前缀 `.autofilm_` 与保护状态文件一致，`collect_local_files` 会把它当成程序
/// 内部文件跳过，不会被同步清理误删。
fn state_file_path(target_dir: &Path, task_id: &str) -> PathBuf {
    target_dir.join(format!(
        ".autofilm_retry_{}.json",
        sanitize_state_id(task_id)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "autofilm-retry-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[tokio::test]
    async fn missing_state_file_means_nothing_to_rescan() {
        let root = temp_dir("missing");
        let retry = RetryDirs::load(&root, "电影").await;
        assert!(retry.is_empty());
        assert!(!retry.contains("/媒体库/电影/某目录"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn records_survive_reload_and_clear_on_success() {
        let root = temp_dir("roundtrip");
        let retry = RetryDirs::load(&root, "电影").await;

        let failed: HashSet<String> = ["/媒体库/电影/甲", "/媒体库/电影/乙"]
            .into_iter()
            .map(str::to_string)
            .collect();
        retry.replace(&failed).await.unwrap();

        let reloaded = RetryDirs::load(&root, "电影").await;
        assert_eq!(reloaded.len(), 2);
        assert!(reloaded.contains("/媒体库/电影/甲"));
        assert!(reloaded.contains("/媒体库/电影/乙"));

        // 本轮全部成功：记录连同文件一起清除。
        reloaded.replace(&HashSet::new()).await.unwrap();
        let cleared = RetryDirs::load(&root, "电影").await;
        assert!(cleared.is_empty());
        assert!(!state_file_path(&root, "电影").exists());
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn tasks_do_not_share_state_files() {
        let root = temp_dir("isolated");
        let failed: HashSet<String> = ["/a".to_string()].into_iter().collect();
        RetryDirs::load(&root, "任务A")
            .await
            .replace(&failed)
            .await
            .unwrap();

        assert!(RetryDirs::load(&root, "任务B").await.is_empty());
        assert_eq!(RetryDirs::load(&root, "任务A").await.len(), 1);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn detects_pending_descendants_without_prefix_false_positive() {
        let retry = RetryDirs {
            state_file: PathBuf::new(),
            dirs: ["/a/b/c".to_string()].into_iter().collect(),
        };
        assert!(retry.contains("/a/b/c"));
        assert!(retry.has_pending_under("/a"));
        assert!(retry.has_pending_under("/a/b"));
        // 末尾斜杠不应影响判断。
        assert!(retry.has_pending_under("/a/b/"));
        // 前缀必须按目录边界匹配：`/a/bc` 不是 `/a/b/c` 的祖先。
        assert!(!retry.has_pending_under("/a/bc"));
        assert!(!retry.has_pending_under("/x"));
    }

    #[tokio::test]
    async fn corrupted_state_is_ignored() {
        let root = temp_dir("corrupted");
        let path = state_file_path(&root, "../../坏任务");
        std::fs::write(&path, b"{ not json").unwrap();

        let retry = RetryDirs::load(&root, "../../坏任务").await;
        assert!(retry.is_empty(), "坏状态文件不应阻塞任务");
        // 文件名必须落在目标目录内，不能因任务 ID 里的斜杠逃出去。
        assert_eq!(path.parent(), Some(root.as_path()));
        std::fs::remove_dir_all(&root).ok();
    }
}
