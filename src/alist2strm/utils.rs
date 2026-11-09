use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use tokio::fs;
use tracing::{debug, info};

use crate::alist2strm::errors::Result;

/// 程序自己写在目标目录里的内部文件前缀。
///
/// 智能删除保护的状态文件（`.autofilm_strm_<任务>.json`）与它的临时文件都
/// 位于 `target_dir` 下。它们不是媒体文件，必须在同步清理时被排除，
/// 否则每轮清理都会把状态文件删掉，导致宽限计数永远无法累积。
const INTERNAL_FILE_PREFIX: &str = ".autofilm_";

/// 判断文件名是否是程序自己维护的内部文件。
pub(super) fn is_internal_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with(INTERNAL_FILE_PREFIX))
}

/// 判断本地伴生文件是否已经过期或疑似损坏。
///
/// 本地大小小于远端大小、或本地修改时间早于远端时间时，认为需要重新下载。
/// `remote_timestamps` 同时给出远端的创建与修改时间：任务会把本地文件时间
/// 对齐到远端时间，若本地 mtime 恰好等于其中之一，说明这一份就是最新内容，
/// 不能当成过期——否则每轮同步都会把伴生文件重下一遍。
pub(super) async fn companion_file_is_stale(
    local_path: &Path,
    remote_size: u64,
    remote_timestamps: &[i64],
) -> Result<bool> {
    let metadata = fs::metadata(local_path).await?;
    if metadata.len() < remote_size {
        return Ok(true);
    }
    if let Ok(modified) = metadata.modified() {
        if let Ok(duration) = modified.duration_since(std::time::UNIX_EPOCH) {
            let local_timestamp = duration.as_secs() as i64;
            if remote_timestamps.contains(&local_timestamp) {
                return Ok(false);
            }
            // 与最“新”的远端时间比较：任何一个都更晚才算过期。
            let newest = remote_timestamps.iter().copied().max().unwrap_or(i64::MIN);
            return Ok(local_timestamp < newest);
        }
    }
    Ok(false)
}

/// 收集目标目录中的本地文件集合。
///
/// 平铺模式只扫描目标目录第一层文件；非平铺模式会递归扫描所有子目录。
/// 返回结果用于和远端扫描得到的 `processed_local_paths` 比较，找出需要清理的
/// 过期本地文件。程序自身的状态文件、临时文件不会进入结果集。
pub(super) async fn collect_local_files(
    target_dir: &Path,
    flatten_mode: bool,
) -> Result<HashSet<PathBuf>> {
    let mut files = HashSet::new();
    if !fs::try_exists(target_dir).await? {
        return Ok(files);
    }

    if flatten_mode {
        let mut entries = fs::read_dir(target_dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if entry.file_type().await?.is_file() && !is_internal_file(&path) {
                files.insert(path);
            }
        }
        return Ok(files);
    }

    let mut dirs = vec![target_dir.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let mut entries = fs::read_dir(dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let file_type = entry.file_type().await?;
            let path = entry.path();
            if file_type.is_dir() {
                dirs.push(path);
            } else if file_type.is_file() && !is_internal_file(&path) {
                files.insert(path);
            }
        }
    }
    Ok(files)
}

/// 从指定父目录开始向上删除空目录。
///
/// 删除会在遇到非空目录、缺失目录或到达任务目标目录时停止。该函数用于移除
/// 过期文件后清理遗留的空目录层级，不会删除 `target_dir` 本身，也不会越出
/// `target_dir` 向上删除。
pub(super) async fn remove_empty_parents(parent: Option<&Path>, target_dir: &Path) -> Result<()> {
    let mut current = parent.map(Path::to_path_buf);
    while let Some(dir) = current {
        // 只在目标目录内部清理；越界（历史遗留的异常路径）直接停止。
        if dir == target_dir || !dir.starts_with(target_dir) {
            break;
        }

        let mut entries = match fs::read_dir(&dir).await {
            Ok(entries) => entries,
            Err(err) => {
                debug!(path = %dir.display(), error = %err, "读取目录失败，停止清理空目录");
                break;
            }
        };
        match entries.next_entry().await {
            Ok(None) => {}
            Ok(Some(_)) => break,
            Err(err) => {
                debug!(path = %dir.display(), error = %err, "读取目录项失败，停止清理空目录");
                break;
            }
        }

        if let Err(err) = fs::remove_dir(&dir).await {
            debug!(path = %dir.display(), error = %err, "删除空目录失败，停止清理");
            break;
        }
        info!("删除空目录: {}", dir.display());
        current = dir.parent().map(Path::to_path_buf);
    }
    Ok(())
}
