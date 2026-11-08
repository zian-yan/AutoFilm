use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use tokio::fs;
use tracing::warn;

use crate::alist2strm::{config::SmartProtectionConfig, errors::Result};

/// 保护状态里最多记录的候选文件数量。
///
/// 状态文件是本地的有界缓存，数量异常时宁可清空重新计数（这会让删除更保守），
/// 也不要让文件无限膨胀。
const MAX_PROTECTED_ENTRIES: usize = 20_000;

#[derive(Debug)]
pub struct ProtectionManager {
    target_dir: PathBuf,
    state_file: PathBuf,
    threshold: usize,
    grace_scans: usize,
    protected: HashMap<String, usize>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct ProtectionState {
    updated: String,
    protected: HashMap<String, usize>,
}

impl ProtectionManager {
    /// 创建 `.strm` 智能删除保护管理器。
    ///
    /// 管理器会在目标目录下使用任务 ID 对应的状态文件保存候选删除计数。
    /// 如果存在上次扫描留下的状态，会尽量加载并延续计数；加载失败则从空状态
    /// 开始，避免损坏的保护文件阻止同步任务继续运行。
    pub async fn new(
        target_dir: PathBuf,
        task_id: &str,
        config: &SmartProtectionConfig,
    ) -> Result<Self> {
        let state_file = target_dir.join(format!(
            ".autofilm_strm_{}.json",
            sanitize_state_id(task_id)
        ));
        let protected = load_state(&state_file).await.unwrap_or_default().protected;
        Ok(Self {
            target_dir,
            state_file,
            threshold: config.threshold,
            grace_scans: config.grace_scans,
            protected,
        })
    }

    /// 根据本次扫描结果决定哪些本地文件可以真正删除。
    ///
    /// 当待删除数量低于阈值时，认为是正常同步差异并立即返回待删除集合；
    /// 当数量达到阈值时，进入宽限计数，只有连续多次扫描都确认缺失的文件才会
    /// 返回给清理流程。仍然存在于远端扫描结果中的文件会从保护状态中移除。
    ///
    /// 传参覆盖全部文件类型（不止 `.strm`），因为 AList 故障时字幕、图片同样
    /// 会被误判为「已删除」，只保护 `.strm` 会留下同样的数据丢失风险。
    pub async fn process(
        &mut self,
        to_delete: HashSet<PathBuf>,
        present: &HashSet<PathBuf>,
    ) -> Result<HashSet<PathBuf>> {
        // 如果文件重新出现在远端扫描结果中，说明不是误删候选，清除保护计数。
        self.protected.retain(|relative_path, _| {
            let absolute_path = self.target_dir.join(relative_path);
            !present.contains(&absolute_path)
        });

        let ready = if to_delete.len() < self.threshold {
            // 删除量低于阈值时认为是正常同步，立即删除。
            for file_path in &to_delete {
                let relative_path = self.to_relative(file_path);
                self.protected.remove(&relative_path);
            }
            to_delete
        } else {
            // 删除量超过阈值时进入宽限计数，连续多次确认后才真正删除。
            for file_path in &to_delete {
                let relative_path = self.to_relative(file_path);
                *self.protected.entry(relative_path).or_insert(0) += 1;
            }

            // 只保留本轮仍是候选的条目，避免历史条目随着时间无限堆积。
            self.protected.retain(|relative_path, _| {
                let absolute_path = self.target_dir.join(relative_path);
                to_delete.contains(&absolute_path)
            });
            if self.protected.len() > MAX_PROTECTED_ENTRIES {
                warn!(
                    entries = self.protected.len(),
                    "删除保护候选数量异常，已重置计数（本轮不会执行大批量删除）"
                );
                self.protected.clear();
            }

            let ready_relative_paths = self
                .protected
                .iter()
                .filter_map(|(path, count)| (*count >= self.grace_scans).then_some(path.clone()))
                .collect::<Vec<_>>();

            let ready = ready_relative_paths
                .iter()
                .map(|path| self.target_dir.join(path))
                .collect();

            for path in ready_relative_paths {
                self.protected.remove(&path);
            }
            ready
        };

        self.save().await?;
        Ok(ready)
    }

    /// 将当前保护状态写入磁盘。
    ///
    /// 状态记录每个相对路径已连续缺失的次数。写入时先生成临时文件再 rename，
    /// 尽量避免程序中断或崩溃时留下半截 JSON。
    async fn save(&self) -> Result<()> {
        // 使用临时文件 + rename，尽量避免程序中断时写出半截 JSON。
        if let Some(parent) = self.state_file.parent() {
            fs::create_dir_all(parent).await?;
        }

        let temp_file = self.state_file.with_extension("tmp");
        let state = ProtectionState {
            updated: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_secs().to_string())
                .unwrap_or_else(|_| "0".to_string()),
            protected: self.protected.clone(),
        };
        fs::write(&temp_file, serde_json::to_vec_pretty(&state)?).await?;
        fs::rename(temp_file, &self.state_file).await?;
        Ok(())
    }

    /// 将本地绝对路径转换为目标目录下的相对路径字符串。
    ///
    /// 保护状态使用相对路径持久化，避免目标目录移动后状态文件中记录的绝对路径
    /// 失效；Windows 路径分隔符也会统一转换为 `/`。
    fn to_relative(&self, path: &Path) -> String {
        path.strip_prefix(&self.target_dir)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    }
}

/// 把任务 ID 收敛成可用作文件名的形式。
///
/// 任务 ID 来自配置，可能包含路径分隔符；直接拼进状态文件名会写到目标目录
/// 之外，因此只保留字母、数字、`-`、`_` 与常见 Unicode 字符。
fn sanitize_state_id(task_id: &str) -> String {
    let sanitized = task_id
        .chars()
        .map(|character| match character {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            character => character,
        })
        .collect::<String>();
    let sanitized = sanitized.trim().trim_matches('.').trim();
    if sanitized.is_empty() {
        "task".to_string()
    } else {
        sanitized.to_string()
    }
}

/// 从保护状态文件读取上次扫描留下的计数。
///
/// 返回值包含每个候选文件的连续缺失次数。调用方会在读取失败时回退为空
/// 状态，因此该函数只负责严格解析文件内容。
async fn load_state(path: &Path) -> Result<ProtectionState> {
    let content = fs::read(path).await?;
    Ok(serde_json::from_slice(&content)?)
}

#[cfg(test)]
mod tests {
    use std::{
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;

    #[tokio::test]
    async fn delays_large_deletions_until_grace_count() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let target_dir = std::env::temp_dir().join(format!("autofilm-protection-{unique}"));
        fs::create_dir_all(&target_dir).await.unwrap();

        let config = SmartProtectionConfig {
            enabled: true,
            threshold: 1,
            grace_scans: 2,
        };
        let mut manager = ProtectionManager::new(target_dir.clone(), "test", &config)
            .await
            .unwrap();
        let file = target_dir.join("movie.strm");

        let first = manager
            .process(HashSet::from([file.clone()]), &HashSet::new())
            .await
            .unwrap();
        assert!(first.is_empty());

        let second = manager
            .process(HashSet::from([file.clone()]), &HashSet::new())
            .await
            .unwrap();
        assert_eq!(second, HashSet::from([file]));

        fs::remove_dir_all(&target_dir).await.ok();
    }

    #[tokio::test]
    async fn state_is_persisted_between_runs() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let target_dir = std::env::temp_dir().join(format!("autofilm-protection-state-{unique}"));
        fs::create_dir_all(&target_dir).await.unwrap();

        let config = SmartProtectionConfig {
            enabled: true,
            threshold: 1,
            grace_scans: 3,
        };
        let file = target_dir.join("movie.strm");

        let mut first = ProtectionManager::new(target_dir.clone(), "task", &config)
            .await
            .unwrap();
        let ready = first
            .process(HashSet::from([file.clone()]), &HashSet::new())
            .await
            .unwrap();
        assert!(ready.is_empty());

        // 新一轮扫描必须读到上一轮写入的计数，否则宽限机制形同虚设。
        let mut second = ProtectionManager::new(target_dir.clone(), "task", &config)
            .await
            .unwrap();
        let ready = second
            .process(HashSet::from([file.clone()]), &HashSet::new())
            .await
            .unwrap();
        assert!(ready.is_empty(), "grace count should require 3 scans");

        let ready = second
            .process(HashSet::from([file.clone()]), &HashSet::new())
            .await
            .unwrap();
        assert_eq!(ready, HashSet::from([file]));

        fs::remove_dir_all(&target_dir).await.ok();
    }

    #[test]
    fn state_file_name_does_not_escape_target_dir() {
        let id = sanitize_state_id("../../etc");
        assert!(!id.contains('/') && !id.contains('\\'), "id: {id}");
        assert!(!id.starts_with('.'), "id: {id}");

        let path = PathBuf::from("/media").join(format!(".autofilm_strm_{id}.json"));
        assert!(path.starts_with("/media"), "path: {}", path.display());
    }
}
