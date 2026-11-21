use std::{collections::HashSet, path::PathBuf};

use serde::{Deserialize, Serialize};

use crate::extensions::VIDEO_EXTS;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DownloadOption {
    #[serde(default)]
    pub enable: bool,
    #[serde(default)]
    pub subtitle: bool,
    #[serde(default)]
    pub image: bool,
    #[serde(default)]
    pub nfo: bool,
    #[serde(default, deserialize_with = "deserialize_exts")]
    pub other_ext: Vec<String>,
    #[serde(default = "default_download_concurrency")]
    pub concurrency: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SmartProtectionConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_protection_threshold")]
    pub threshold: usize,
    #[serde(default = "default_grace_scans")]
    pub grace_scans: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SyncConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub ignore: Option<String>,
    #[serde(default)]
    pub smart_protection: Option<SmartProtectionConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    // 任务 ID 用于日志、保护状态文件名等场景。
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub cron: Option<String>,
    pub alist: String,
    pub source_dir: String,
    pub target_dir: PathBuf,
    #[serde(default)]
    pub flatten_mode: bool,
    #[serde(default)]
    pub overwrite: bool,
    /// 目录时间检查：远端目录的修改时间不晚于本地目录时，跳过该子树的扫描。
    ///
    /// 适合大规模媒体库的增量扫描；但部分网盘驱动的目录时间不随内容更新，
    /// 会出现「文件已更新却仍被跳过」，此时应关闭该项。
    #[serde(default)]
    pub dir_time_check: bool,
}

/// Alist2Strm 全局设置。
///
/// 并发、生成后缀、伴生文件下载与同步删除策略对所有同步任务共用一份：这些选项
/// 通常是「这台机器 / 这个 AList 能承受多少、要产出哪些文件」的公共约定，而不是
/// 每个任务各不相同的属性，放在任务里只会被反复抄写。任务本身因此只描述
/// 「扫哪里、写到哪里」。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GlobalConfig {
    /// 需要生成 `.strm` 的源文件后缀列表（白名单）。
    ///
    /// 留空、缺失或只写了无效条目时回退到 [`VIDEO_EXTS`] 默认视频后缀。
    /// 后缀大小写不敏感，可带或不带 `.` 前缀，重复项会被去重。
    #[serde(default, deserialize_with = "deserialize_exts")]
    pub strm_ext: Vec<String>,
    #[serde(default = "default_concurrency")]
    pub concurrency: usize,
    #[serde(default)]
    pub download: DownloadOption,
    #[serde(default)]
    pub sync: Option<SyncConfig>,
}

impl GlobalConfig {
    /// 返回实际生效的 `.strm` 源文件后缀集合。
    ///
    /// 配置为「空」（缺失、空列表或全是空条目）时回退到 [`VIDEO_EXTS`]：
    /// 若把「一个后缀都没有」当成有效配置，整轮扫描不会产出任何文件，开启
    /// 同步清理时还会把库里已有的 `.strm` 全部判成过期文件删掉。
    pub fn strm_exts(&self) -> HashSet<String> {
        self.normalized_strm_ext().unwrap_or_else(|| {
            VIDEO_EXTS
                .iter()
                .map(|ext| (*ext).to_string())
                .collect::<HashSet<_>>()
        })
    }

    /// 配置中是否给出了有效的 `.strm` 源文件后缀（用于日志区分默认值）。
    pub fn has_custom_strm_ext(&self) -> bool {
        self.normalized_strm_ext().is_some()
    }

    /// 规范化并去重配置里的后缀；没有任何有效条目时返回 `None`。
    ///
    /// 反序列化已经规范化过一次，这里再算一遍是给「代码直接构造 GlobalConfig」
    /// 的场景兜底，保证两条入口的规则完全一致。
    fn normalized_strm_ext(&self) -> Option<HashSet<String>> {
        let exts = normalize_exts(self.strm_ext.iter().map(String::as_str));
        (!exts.is_empty()).then(|| exts.into_iter().collect())
    }
}

impl Default for GlobalConfig {
    fn default() -> Self {
        Self {
            strm_ext: Vec::new(),
            concurrency: default_concurrency(),
            download: DownloadOption::default(),
            sync: None,
        }
    }
}

/// 返回默认的普通文件处理并发数。
///
/// 该值限制「单个远端目录内」同时生成 `.strm` / 下载伴生文件的任务数量。
/// 目录之间严格串行：一批全部完成后才会继续扫描下一个目录。
fn default_concurrency() -> usize {
    50
}

/// 返回默认的伴生文件下载并发数。
///
/// 下载字幕、图片、nfo 等伴生文件时会使用独立限流，避免下载连接占满
/// AList 或上游存储资源。
fn default_download_concurrency() -> usize {
    5
}

/// 返回智能删除保护的默认触发阈值。
///
/// 当一次同步准备删除的 `.strm` 数量达到该阈值时，会进入宽限确认流程，
/// 用来降低 AList 故障或扫描不完整时的大量误删风险。
fn default_protection_threshold() -> usize {
    100
}

/// 返回智能删除保护的默认宽限扫描次数。
///
/// 超过删除阈值的 `.strm` 需要连续多次仍被判定为可删除，才会真正移除。
fn default_grace_scans() -> usize {
    3
}

impl Default for DownloadOption {
    /// 构造默认下载选项。
    ///
    /// 默认只生成视频对应的 `.strm`，不额外下载字幕、图片、nfo 或自定义
    /// 扩展名文件。
    fn default() -> Self {
        Self {
            enable: false,
            subtitle: false,
            image: false,
            nfo: false,
            other_ext: Vec::new(),
            concurrency: default_download_concurrency(),
        }
    }
}

impl Default for SmartProtectionConfig {
    /// 构造默认智能保护配置。
    ///
    /// 默认不开启保护，但保留阈值和宽限次数的默认值，便于配置中只显式打开
    /// `enabled` 时获得稳定行为。
    fn default() -> Self {
        Self {
            enabled: false,
            threshold: default_protection_threshold(),
            grace_scans: default_grace_scans(),
        }
    }
}

/// 反序列化自定义扩展名配置。
///
/// 配置可以为空、逗号分隔字符串，或字符串列表；函数会统一清理空白、规范成
/// 小写且带 `.` 前缀的扩展名并去重，方便后续直接和文件后缀集合匹配。
fn deserialize_exts<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Exts {
        Empty(Option<String>),
        String(String),
        List(Vec<String>),
    }

    let exts = match Exts::deserialize(deserializer)? {
        Exts::Empty(None) => Vec::new(),
        Exts::Empty(Some(value)) | Exts::String(value) => normalize_exts(value.split(',')),
        Exts::List(values) => normalize_exts(values.iter().map(String::as_str)),
    };
    Ok(exts)
}

/// 批量规范化扩展名：丢弃空条目、统一小写加点并按首次出现的顺序去重。
fn normalize_exts<'a>(values: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut exts = Vec::new();
    for value in values {
        let Some(ext) = normalize_ext(value) else {
            continue;
        };
        if seen.insert(ext.clone()) {
            exts.push(ext);
        }
    }
    exts
}

/// 规范化单个文件扩展名；空值或只有空白 / 点的条目返回 `None`。
///
/// 该函数会去除首尾空白、转成小写，并补齐缺失的 `.` 前缀，使配置中的
/// `srt`、`.SRT` 与 ` .Srt ` 都能匹配同一种后缀。对外的伴生文件后缀也走
/// 这里，避免两套后缀写法各走一套规则。
pub(super) fn normalize_ext(value: &str) -> Option<String> {
    let value = value.trim().trim_start_matches('.').trim();
    if value.is_empty() {
        return None;
    }
    Some(format!(".{}", value.to_ascii_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 解析根级 `alist2strm` 段。
    fn global(yaml: &str) -> GlobalConfig {
        #[derive(Deserialize)]
        struct Root {
            alist2strm: GlobalConfig,
        }
        let root: Root = serde_yaml::from_str(yaml).expect("global config should parse");
        root.alist2strm
    }

    #[test]
    fn missing_strm_ext_falls_back_to_default_video_exts() {
        let config = global("alist2strm:\n  concurrency: 10\n");
        assert!(!config.has_custom_strm_ext());
        let exts = config.strm_exts();
        assert_eq!(exts.len(), VIDEO_EXTS.len());
        for ext in VIDEO_EXTS {
            assert!(exts.contains(*ext), "默认后缀缺少 {ext}");
        }
        assert!(!exts.contains(".iso"), "ISO 需要显式配置才会生成 strm");
    }

    #[test]
    fn empty_strm_ext_falls_back_instead_of_disabling_output() {
        // 空列表若被当成「有效配置」，整轮不会产出任何 .strm，同步清理还会误删。
        for yaml in [
            "alist2strm:\n  strm_ext: []\n",
            "alist2strm:\n  strm_ext:\n",
            "alist2strm:\n  strm_ext: \"\"\n",
            "alist2strm:\n  strm_ext: [\"\", '  ', \".\"]\n",
        ] {
            let config = global(yaml);
            assert!(!config.has_custom_strm_ext(), "`{yaml}` 不应算作自定义后缀");
            assert_eq!(config.strm_exts().len(), VIDEO_EXTS.len());
        }
    }

    #[test]
    fn strm_ext_normalizes_case_prefix_and_duplicates() {
        let config = global("alist2strm:\n  strm_ext: [ISO, '.iso', ' .Mkv ', 'mkv', '']\n");
        assert!(config.has_custom_strm_ext());
        // 大小写与点前缀归一，重复项只保留第一次出现的形态，空条目被丢弃。
        assert_eq!(
            config.strm_ext,
            vec![".iso".to_string(), ".mkv".to_string()]
        );
        let exts = config.strm_exts();
        assert_eq!(exts.len(), 2);
        assert!(exts.contains(".iso") && exts.contains(".mkv"));
    }

    #[test]
    fn strm_ext_accepts_comma_separated_string() {
        let config = global("alist2strm:\n  strm_ext: iso, .TS, iso\n");
        assert_eq!(config.strm_ext, vec![".iso".to_string(), ".ts".to_string()]);
    }

    #[test]
    fn download_other_ext_is_normalized_and_deduped() {
        let download: DownloadOption =
            serde_yaml::from_str("other_ext: ['.ZIP', 'zip', '']\n").expect("download option");
        assert_eq!(download.other_ext, vec![".zip".to_string()]);
    }
}
