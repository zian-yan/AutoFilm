//! 业务配置：`config.yaml`。
//!
//! 本模块只负责「业务数据」——AList 客户端、媒体服务器、同步 / 追番 / 海报任务。
//! Web 管理面板自身的设置（监听地址、认证、文件监听开关）已拆分到
//! [`crate::web_config`]，两者使用独立的文件、独立的存储与独立的版本号，
//! 任何一个的读写都不会碰到另一个。
//!
//! 业务配置允许为空：文件不存在、内容为空、只有注释，都会被解析成
//! [`Config::default`]（各分区均为空集合），程序照常启动，便于在初始化阶段
//! 先准备目录、之后再通过管理面板或手工编辑逐步补齐。

use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::warn;

use crate::{alist, alist2strm, ani2alist, library_poster, media_server, mediawarp};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("读取配置文件失败: {0}")]
    Read(#[source] std::io::Error),

    #[error("解析配置文件失败: {0}")]
    Parse(#[from] serde_yaml::Error),

    #[error("序列化配置失败: {0}")]
    Serialize(#[source] serde_yaml::Error),

    #[error("写入配置文件失败: {0}")]
    Write(#[source] std::io::Error),
}

/// 业务配置缺失时写入的骨架内容。
///
/// 只有注释，因此解析结果等价于空配置；既能保证配置文件在磁盘上存在
/// （便于容器挂载与文件监听），又不会凭空生成一堆示例条目误导用户。
const EMPTY_CONFIG_SKELETON: &str = "\
# AutoFilm 业务配置（config.yaml）
#
# 本文件允许为空：alist / media_servers / alist2strm_tasks / ani2alist_tasks /
# library_poster_tasks 全部可以省略，程序会按空配置启动，之后通过 Web 管理面板
# 或直接编辑本文件逐步补充。
#
# Web 管理面板自身的设置（监听地址、认证、文件监听）已拆分到同目录的 web.yaml，
# 与本文件互不影响：保存面板设置不会重写本文件，反之亦然。
";

/// 单条配置校验问题。
///
/// `field` 使用 `段落[下标].字段` 形式（例如 `alist2strm_tasks[0].source_dir`），
/// 管理面板据此把错误定位到具体输入框。
#[derive(Debug, Clone, Serialize)]
pub struct ValidationIssue {
    pub field: String,
    pub message: String,
}

impl ValidationIssue {
    pub fn new(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            message: message.into(),
        }
    }
}

/// 导出 / 回传配置时替换敏感字段的占位符。
///
/// 面板拿到的是这个占位符，原样提交回来时由 [`Config::unmask`] 换回真实值：
/// 既能让「复制备份」与接口响应不含明文密钥，又不会把用户没碰过的密码冲掉。
pub const SECRET_MASK: &str = "__autofilm_masked__";

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Config {
    // Rust 版统一使用 snake_case 根字段；旧 Python 平铺配置不再兼容。
    #[serde(default)]
    pub alist: Vec<alist::AlistConfig>,
    /// Alist2Strm 全局设置：并发、伴生文件下载、同步删除策略。
    ///
    /// 这些选项对所有同步任务共用一份，任务里不再重复配置。
    #[serde(default)]
    pub alist2strm: alist2strm::GlobalConfig,
    #[serde(default)]
    pub alist2strm_tasks: Vec<alist2strm::Config>,
    #[serde(default)]
    pub ani2alist_tasks: Vec<ani2alist::Config>,
    #[serde(default)]
    pub media_servers: Vec<media_server::Config>,
    #[serde(default)]
    pub library_poster_tasks: Vec<library_poster::Config>,
    /// MediaWarp 反向代理的面板配置。
    ///
    /// 这是生成 `config/mediawarp.yaml` 的唯一数据源；保存本文件时由热重载内核
    /// 渲染成 MediaWarp 原生配置并自动重启代理进程（详见 `mediawarp` 模块）。
    #[serde(default)]
    pub mediawarp: mediawarp::MediaWarpConfig,
}

impl Config {
    /// 返回脱敏副本：AList 密码 / OTP / 令牌与媒体服务器 API Key 全部替换为
    /// [`SECRET_MASK`]，用于面板回传与「复制备份」导出。
    pub fn masked(&self) -> Self {
        let mut masked = self.clone();
        for item in &mut masked.alist {
            mask_optional(&mut item.password);
            mask_optional(&mut item.otp_code);
            mask_optional(&mut item.token);
        }
        for server in &mut masked.media_servers {
            if !server.api_key.is_empty() {
                server.api_key = SECRET_MASK.to_string();
            }
        }
        masked
    }

    /// 把提交里仍是占位符的敏感字段还原成当前生效值。
    ///
    /// 按 `id` 匹配（而不是下标），这样在面板里插删条目也不会把密码串到别的
    /// AList 上；找不到同 id 的历史配置时保持原样，交给校验阶段报错。
    pub fn unmask(&mut self, current: &Self) {
        for item in &mut self.alist {
            let Some(old) = current.alist.iter().find(|old| old.id == item.id) else {
                continue;
            };
            restore_optional(&mut item.password, &old.password);
            restore_optional(&mut item.otp_code, &old.otp_code);
            restore_optional(&mut item.token, &old.token);
        }
        for server in &mut self.media_servers {
            if server.api_key == SECRET_MASK
                && let Some(old) = current.media_servers.iter().find(|old| old.id == server.id)
            {
                server.api_key = old.api_key.clone();
            }
        }
    }

    /// 读取业务配置。
    ///
    /// 文件缺失不算错误：会按空配置启动，并尽力补一份仅含注释的骨架文件。
    /// 只有「文件存在但读不出来」或「内容不是合法 YAML」才会返回错误——
    /// 后者通常是用户改坏了配置，静默忽略会让问题更难发现。
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let content = match fs::read_to_string(path) {
            Ok(content) => content,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                if let Err(err) = write_skeleton(path, EMPTY_CONFIG_SKELETON) {
                    warn!(
                        path = %path.display(),
                        error = %err,
                        "业务配置不存在且无法创建骨架文件，将按空配置启动"
                    );
                }
                return Ok(Self::default());
            }
            Err(err) => return Err(Error::Read(err)),
        };

        Self::from_yaml(&content)
    }

    /// 从 YAML 文本解析业务配置。
    ///
    /// 空文档、只有注释、显式 `null` 都会得到空配置（各分区为空集合），
    /// 不会因为缺少分区而报错。
    pub fn from_yaml(text: &str) -> Result<Self> {
        if is_blank_yaml(text) {
            return Ok(Self::default());
        }

        let value: serde_yaml::Value = serde_yaml::from_str(text)?;
        warn_legacy_web_section(&value);
        warn_legacy_alist2strm_fields(&value);
        let parsed: Option<Config> = serde_yaml::from_value(value)?;
        Ok(parsed.unwrap_or_default())
    }

    /// 是否为空配置（所有业务分区都没有条目）。
    pub fn is_empty(&self) -> bool {
        // 启用 MediaWarp 托管时即使没有任何调度任务也不能算「空配置」：
        // 入口会据此在没有 Web 面板时提前退出，那样会连带停掉代理子进程。
        self.alist.is_empty()
            && self.alist2strm_tasks.is_empty()
            && self.ani2alist_tasks.is_empty()
            && self.media_servers.is_empty()
            && self.library_poster_tasks.is_empty()
            && !self.mediawarp.enable
    }

    /// 将配置原子写回磁盘，并保留一份 `.bak` 备份。
    ///
    /// 只会覆盖业务配置文件本身，不会触碰 `web.yaml`。先写同目录临时文件再
    /// `rename`，避免面板保存过程中断电导致配置被截断；已存在的配置文件会被
    /// 复制为 `<文件名>.bak`，便于回滚。配置中包含 AList 密码/令牌与媒体服务器
    /// API Key，因此写入时会尽量限制为仅属主可读。
    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        save_atomic(path.as_ref(), self).map_err(Error::Write)
    }

    /// 校验配置的语义正确性，返回全部问题而不是遇到第一个就返回。
    ///
    /// 静态校验覆盖引用完整性、唯一性、数值范围与路径形态；Cron 表达式的语法
    /// 错误会在真正构建调度器时二次校验。
    pub fn validate(&self) -> Vec<ValidationIssue> {
        let mut issues = Vec::new();

        let alist_ids = validate_alist(&self.alist, &mut issues);
        let media_server_ids = validate_media_servers(&self.media_servers, &mut issues);
        validate_alist2strm_global(&self.alist2strm, &mut issues);
        validate_alist2strm(&self.alist2strm_tasks, &alist_ids, &mut issues);
        validate_ani2alist(&self.ani2alist_tasks, &alist_ids, &mut issues);
        validate_library_poster(&self.library_poster_tasks, &media_server_ids, &mut issues);
        issues.extend(self.mediawarp.validate());

        issues
    }
}

/// 判断一段 YAML 文本是否「空」：没有内容，或只有注释。
///
/// 这类文件在语义上等价于空配置。这里显式短路，而不是依赖 YAML 解析器把空文档
/// 解析成 `null` 的行为，保证「留空也照常启动」不因解析器版本而变。
pub(crate) fn is_blank_yaml(text: &str) -> bool {
    text.lines()
        .map(str::trim)
        .all(|line| line.is_empty() || line.starts_with('#'))
}

/// 提示旧版把 `web:` 写在业务配置里的用户迁移到 `web.yaml`。
///
/// 该分区会被直接忽略（业务结构体里没有 web 字段），如果不提示，用户会以为
/// 修改生效了而实际没有。
fn warn_legacy_web_section(value: &serde_yaml::Value) {
    let Some(map) = value.as_mapping() else {
        return;
    };
    if map.contains_key(&serde_yaml::Value::String("web".to_string())) {
        warn!(
            "业务配置中仍存在 `web:` 分区，该分区已拆分到独立的 web.yaml，本次将被忽略；\
             请把该段内容迁移到 web.yaml 后从业务配置中删除"
        );
    }
}

/// 提示旧版把并发 / 下载 / 同步 / 生成后缀写在单个任务里的用户迁移到全局配置。
///
/// 这些字段已经从任务结构体上移到根级 `alist2strm`，任务里的同名键会被 serde
/// 直接忽略；不提示的话用户会以为改动生效了（实际用的是全局默认值）。
fn warn_legacy_alist2strm_fields(value: &serde_yaml::Value) {
    let Some(tasks) = value
        .get("alist2strm_tasks")
        .and_then(|tasks| tasks.as_sequence())
    else {
        return;
    };

    for (index, task) in tasks.iter().enumerate() {
        let Some(map) = task.as_mapping() else {
            continue;
        };
        for key in ["concurrency", "download", "sync", "strm_ext"] {
            if map.contains_key(serde_yaml::Value::String(key.to_string())) {
                warn!(
                    index,
                    field = key,
                    "alist2strm_tasks 中仍存在任务级 `{key}` 配置，该选项只支持配置在全局 \
                     `alist2strm` 段，本次将被忽略；请把该值移到全局配置后从任务中删除"
                );
            }
        }
    }
}

/// 通用原子写盘：写临时文件 → 备份旧文件 → rename 覆盖 → 收紧权限。
pub(crate) fn save_atomic<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    let yaml = serde_yaml::to_string(value)
        .map_err(|err| std::io::Error::other(format!("序列化配置失败: {err}")))?;
    save_atomic_bytes(path, yaml.as_bytes())
}

/// 原子写入一段已生成的字节内容。
///
/// 与 [`save_atomic`] 共用同一套「备份 → 临时文件 → rename」流程，供 MediaWarp
/// 生成文件与失败回滚使用；生成文件同样包含媒体服务器 API Key，因此权限一致收紧。
pub(crate) fn save_atomic_bytes(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }

    if path.exists() {
        let backup = backup_path(path);
        fs::copy(path, &backup)?;
        // 备份同样包含明文密钥，权限一并收紧。
        restrict_permissions(&backup);
    }

    let temp = temp_path(path);
    write_private_file(&temp, contents)?;
    fs::rename(&temp, path)?;
    // 覆盖旧文件时 rename 会沿用临时文件权限，这里再兜底收紧一次。
    restrict_permissions(path);
    Ok(())
}

/// 只有文件不存在时才写入骨架内容；已存在则什么都不做。
pub(crate) fn write_skeleton(path: &Path, contents: &str) -> std::io::Result<()> {
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    write_private_file(path, contents.as_bytes())
}

/// 返回配置文件的备份路径。
pub fn backup_path(path: &Path) -> PathBuf {
    sibling_path(path, ".bak")
}

/// 非空的敏感字段替换为占位符；空值与 `None` 原样保留（表示本来就没配置）。
fn mask_optional(value: &mut Option<String>) {
    if value.as_deref().is_some_and(|text| !text.is_empty()) {
        *value = Some(SECRET_MASK.to_string());
    }
}

/// 提交值是占位符时还原为旧值；真实输入（包括空字符串，代表用户主动清空）
/// 一律保持用户提交的内容。
fn restore_optional(value: &mut Option<String>, old: &Option<String>) {
    if value.as_deref() == Some(SECRET_MASK) {
        *value = old.clone();
    }
}

/// 返回写入配置时使用的临时文件路径。
pub(crate) fn temp_path(path: &Path) -> PathBuf {
    sibling_path(path, ".tmp")
}

fn sibling_path(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

/// 写入敏感文件内容。
///
/// 配置与会话文件都含有等同于凭据的内容。Unix 下按 0600 创建，避免同机其他
/// 用户直接读到明文；其他平台沿用默认行为。
pub(crate) fn write_private_file(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::{io::Write, os::unix::fs::OpenOptionsExt};

        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(contents)?;
        file.sync_all()?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        fs::write(path, contents)
    }
}

/// 尽力把已经存在的配置文件权限收紧到 0600。
fn restrict_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        if let Ok(metadata) = fs::metadata(path) {
            let mut permissions = metadata.permissions();
            if (permissions.mode() & 0o777) != 0o600 {
                permissions.set_mode(0o600);
                let _ = fs::set_permissions(path, permissions);
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

/// 判断字符串是否是可用的 http(s) 地址。
fn is_http_url(value: &str) -> bool {
    reqwest::Url::parse(value).is_ok_and(|url| matches!(url.scheme(), "http" | "https"))
}

fn validate_alist(
    items: &[alist::AlistConfig],
    issues: &mut Vec<ValidationIssue>,
) -> HashSet<String> {
    let mut ids = HashSet::new();
    for (index, item) in items.iter().enumerate() {
        let base = format!("alist[{index}]");
        if item.id.trim().is_empty() {
            issues.push(ValidationIssue::new(
                format!("{base}.id"),
                "AList 客户端标识 ID 不能为空",
            ));
        } else if !ids.insert(item.id.trim().to_string()) {
            issues.push(ValidationIssue::new(
                format!("{base}.id"),
                format!("AList 客户端标识 ID `{}` 重复", item.id),
            ));
        }

        if item.base_url.trim().is_empty() {
            issues.push(ValidationIssue::new(
                format!("{base}.base_url"),
                "服务器地址不能为空",
            ));
        } else if !is_http_url(item.base_url.trim()) {
            issues.push(ValidationIssue::new(
                format!("{base}.base_url"),
                "服务器地址需为 http:// 或 https:// 开头的完整地址",
            ));
        }

        let has_username = item
            .username
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty());
        let has_password = item
            .password
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty());
        if has_username != has_password {
            issues.push(ValidationIssue::new(
                format!("{base}.username"),
                "用户名与密码需同时填写，或都留空改用令牌 / 匿名访问",
            ));
        }

        if item.wait_time < 0.0 {
            issues.push(ValidationIssue::new(
                format!("{base}.wait_time"),
                "请求间隔不能为负数",
            ));
        }
    }
    ids
}

fn validate_media_servers(
    items: &[media_server::Config],
    issues: &mut Vec<ValidationIssue>,
) -> HashSet<String> {
    let mut ids = HashSet::new();
    for (index, item) in items.iter().enumerate() {
        let base = format!("media_servers[{index}]");
        if item.id.trim().is_empty() {
            issues.push(ValidationIssue::new(
                format!("{base}.id"),
                "媒体服务器标识 ID 不能为空",
            ));
        } else if !ids.insert(item.id.trim().to_string()) {
            issues.push(ValidationIssue::new(
                format!("{base}.id"),
                format!("媒体服务器标识 ID `{}` 重复", item.id),
            ));
        }
        if item.base_url.trim().is_empty() {
            issues.push(ValidationIssue::new(
                format!("{base}.base_url"),
                "服务器地址不能为空",
            ));
        } else if !is_http_url(item.base_url.trim()) {
            issues.push(ValidationIssue::new(
                format!("{base}.base_url"),
                "服务器地址需为 http:// 或 https:// 开头的完整地址",
            ));
        }
        if item.api_key.trim().is_empty() {
            issues.push(ValidationIssue::new(
                format!("{base}.api_key"),
                "API Key 不能为空",
            ));
        } else if item.kind == media_server::Kind::Jellyfin
            && !media_server::is_safe_api_key(&item.api_key)
        {
            issues.push(ValidationIssue::new(
                format!("{base}.api_key"),
                "Jellyfin API Key 只能包含字母、数字、-、_、.（认证头不支持引号、逗号或空格）",
            ));
        }
        if item.timeout == 0 {
            issues.push(ValidationIssue::new(
                format!("{base}.timeout"),
                "超时时间必须大于 0 秒",
            ));
        }
    }
    ids
}

fn validate_alist2strm(
    items: &[alist2strm::Config],
    alist_ids: &HashSet<String>,
    issues: &mut Vec<ValidationIssue>,
) {
    let mut ids = HashSet::new();
    for (index, item) in items.iter().enumerate() {
        let base = format!("alist2strm_tasks[{index}]");
        check_task_id(&base, &item.id, &mut ids, "Alist2Strm", issues);
        validate_cron(&base, item.cron.as_deref(), issues);

        if item.alist.trim().is_empty() {
            issues.push(ValidationIssue::new(
                format!("{base}.alist"),
                "必须选择一个 AList 客户端",
            ));
        } else if !alist_ids.contains(item.alist.trim()) {
            issues.push(ValidationIssue::new(
                format!("{base}.alist"),
                format!("引用的 AList 客户端 `{}` 不存在", item.alist),
            ));
        }

        if !item.source_dir.starts_with('/') {
            issues.push(ValidationIssue::new(
                format!("{base}.source_dir"),
                "扫描目录必须是 AList 上的绝对路径，以 `/` 开头",
            ));
        }
        if item.target_dir.as_os_str().is_empty() {
            issues.push(ValidationIssue::new(
                format!("{base}.target_dir"),
                "输出目录不能为空",
            ));
        }
    }
}

/// 校验 Alist2Strm 的全局设置（并发与同步删除阈值）。
fn validate_alist2strm_global(
    global: &alist2strm::GlobalConfig,
    issues: &mut Vec<ValidationIssue>,
) {
    const BASE: &str = "alist2strm";

    if !(1..=1024).contains(&global.concurrency) {
        issues.push(ValidationIssue::new(
            format!("{BASE}.concurrency"),
            "并发数需在 1 - 1024 之间",
        ));
    }
    if !(1..=256).contains(&global.download.concurrency) {
        issues.push(ValidationIssue::new(
            format!("{BASE}.download.concurrency"),
            "下载并发数需在 1 - 256 之间",
        ));
    }
    if let Some(protection) = global
        .sync
        .as_ref()
        .and_then(|sync| sync.smart_protection.as_ref())
    {
        if protection.enabled && protection.threshold == 0 {
            issues.push(ValidationIssue::new(
                format!("{BASE}.sync.smart_protection.threshold"),
                "删除阈值必须大于 0",
            ));
        }
        if protection.enabled && protection.grace_scans == 0 {
            issues.push(ValidationIssue::new(
                format!("{BASE}.sync.smart_protection.grace_scans"),
                "宽限扫描次数必须大于 0",
            ));
        }
    }
}

fn validate_ani2alist(
    items: &[ani2alist::Config],
    alist_ids: &HashSet<String>,
    issues: &mut Vec<ValidationIssue>,
) {
    let mut ids = HashSet::new();
    for (index, item) in items.iter().enumerate() {
        let base = format!("ani2alist_tasks[{index}]");
        check_task_id(&base, &item.id, &mut ids, "Ani2Alist", issues);
        validate_cron(&base, item.cron.as_deref(), issues);

        if item.alist.trim().is_empty() {
            issues.push(ValidationIssue::new(
                format!("{base}.alist"),
                "必须选择一个 AList 客户端",
            ));
        } else if !alist_ids.contains(item.alist.trim()) {
            issues.push(ValidationIssue::new(
                format!("{base}.alist"),
                format!("引用的 AList 客户端 `{}` 不存在", item.alist),
            ));
        }

        if !item.target_dir.starts_with('/') {
            issues.push(ValidationIssue::new(
                format!("{base}.target_dir"),
                "挂载路径必须是 AList 上的绝对路径，以 `/` 开头",
            ));
        }
        if item.source.source_url.trim().is_empty() {
            issues.push(ValidationIssue::new(
                format!("{base}.source.source_url"),
                "目录数据源地址不能为空",
            ));
        } else if !is_http_url(item.source.source_url.trim()) {
            issues.push(ValidationIssue::new(
                format!("{base}.source.source_url"),
                "目录数据源地址需为 http:// 或 https:// 开头的完整地址",
            ));
        }
        if item.source.rss_url.trim().is_empty() {
            issues.push(ValidationIssue::new(
                format!("{base}.source.rss_url"),
                "RSS 地址不能为空",
            ));
        } else if !is_http_url(item.source.rss_url.trim()) {
            issues.push(ValidationIssue::new(
                format!("{base}.source.rss_url"),
                "RSS 地址需为 http:// 或 https:// 开头的完整地址",
            ));
        }
        if let ani2alist::UpdateConfig::Keyword { keyword } = &item.update {
            if keyword.trim().is_empty() {
                issues.push(ValidationIssue::new(
                    format!("{base}.update.keyword"),
                    "关键字模式下关键字不能为空",
                ));
            }
        }
    }
}

fn validate_library_poster(
    items: &[library_poster::Config],
    media_server_ids: &HashSet<String>,
    issues: &mut Vec<ValidationIssue>,
) {
    let mut ids = HashSet::new();
    for (index, item) in items.iter().enumerate() {
        let base = format!("library_poster_tasks[{index}]");
        check_task_id(&base, &item.id, &mut ids, "LibraryPoster", issues);
        validate_cron(&base, item.cron.as_deref(), issues);

        if item.server.trim().is_empty() {
            issues.push(ValidationIssue::new(
                format!("{base}.server"),
                "必须选择一个媒体服务器",
            ));
        } else if !media_server_ids.contains(item.server.trim()) {
            issues.push(ValidationIssue::new(
                format!("{base}.server"),
                format!("引用的媒体服务器 `{}` 不存在", item.server),
            ));
        }

        let has_output = item
            .output_dir
            .as_ref()
            .is_some_and(|dir| !dir.as_os_str().is_empty());
        if !item.upload && !has_output {
            issues.push(ValidationIssue::new(
                format!("{base}.output_dir"),
                "未开启上传时，必须配置本地预览输出目录",
            ));
        }
        if item.title_font.as_os_str().is_empty() {
            issues.push(ValidationIssue::new(
                format!("{base}.title_font"),
                "主标题字体路径不能为空",
            ));
        }
        if item.subtitle_font.as_os_str().is_empty() {
            issues.push(ValidationIssue::new(
                format!("{base}.subtitle_font"),
                "副标题字体路径不能为空",
            ));
        }
        if item.libraries.is_empty() {
            issues.push(ValidationIssue::new(
                format!("{base}.libraries"),
                "至少需要配置一个媒体库",
            ));
        }
        if item.render.blur_radius < 0.0 {
            issues.push(ValidationIssue::new(
                format!("{base}.render.blur_radius"),
                "模糊半径不能为负数",
            ));
        }
        if !(0.0..=1.0).contains(&item.render.color_strength) {
            issues.push(ValidationIssue::new(
                format!("{base}.render.color_strength"),
                "主题色强度需在 0.0 - 1.0 之间",
            ));
        }
    }
}

fn check_task_id(
    base: &str,
    id: &str,
    ids: &mut HashSet<String>,
    module: &str,
    issues: &mut Vec<ValidationIssue>,
) {
    if id.trim().is_empty() {
        issues.push(ValidationIssue::new(
            format!("{base}.id"),
            format!("{module} 任务 ID 不能为空，将用于日志与保护状态文件"),
        ));
    } else if !ids.insert(id.trim().to_string()) {
        issues.push(ValidationIssue::new(
            format!("{base}.id"),
            format!("{module} 任务 ID `{id}` 重复"),
        ));
    }
}

fn validate_cron(base: &str, cron: Option<&str>, issues: &mut Vec<ValidationIssue>) {
    let Some(cron) = cron.map(str::trim).filter(|value| !value.is_empty()) else {
        // 留空表示只手工触发，不参与调度。
        return;
    };

    let fields = cron.split_whitespace().count();
    if fields != 6 && fields != 7 {
        issues.push(ValidationIssue::new(
            format!("{base}.cron"),
            "Cron 表达式需为 6 段（秒 分 时 日 月 周）或 7 段（含年）",
        ));
    }
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    const EXAMPLE: &str = include_str!("../config/config.example.yaml");

    #[test]
    fn masked_hides_secrets_and_unmask_restores_them() {
        let alist = alist::AlistConfig {
            id: "main".to_string(),
            base_url: "http://alist:5244".to_string(),
            public_url: None,
            username: Some("admin".to_string()),
            password: Some("p@ss".to_string()),
            otp_code: Some("123456".to_string()),
            token: Some("tok".to_string()),
            wait_time: 0.0,
        };
        let current = Config {
            alist: vec![alist],
            media_servers: vec![media_server::Config {
                id: "emby".to_string(),
                kind: media_server::Kind::Emby,
                base_url: "http://emby:8096".to_string(),
                api_key: "sk-live-abc".to_string(),
                user_id: None,
                timeout: 30,
            }],
            ..Config::default()
        };

        let masked = current.masked();
        // 导出的副本里不能出现任何明文凭据。
        let yaml = serde_yaml::to_string(&masked).expect("masked config should serialize");
        for secret in ["p@ss", "123456", "sk-live-abc"] {
            assert!(!yaml.contains(secret), "掩码后不应残留 {secret}:\n{yaml}");
        }
        assert!(yaml.contains(SECRET_MASK));

        // 原样提交回来（用户没动敏感字段）时必须还原成真实值。
        let mut submitted = masked.clone();
        submitted.unmask(&current);
        assert_eq!(submitted.alist[0].password.as_deref(), Some("p@ss"));
        assert_eq!(submitted.alist[0].token.as_deref(), Some("tok"));
        assert_eq!(submitted.alist[0].otp_code.as_deref(), Some("123456"));
        assert_eq!(submitted.media_servers[0].api_key, "sk-live-abc");

        // 用户主动改写（包括清空）时以其提交内容为准。
        let mut edited = masked.clone();
        edited.alist[0].password = Some("new".to_string());
        edited.alist[0].token = None;
        edited.unmask(&current);
        assert_eq!(edited.alist[0].password.as_deref(), Some("new"));
        assert_eq!(edited.alist[0].token, None);
    }

    fn parse_example() -> Config {
        Config::from_yaml(EXAMPLE).expect("example config should parse")
    }

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!(
                "autofilm-config-{name}-{}",
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .expect("system time should be after unix epoch")
                    .as_nanos()
            ))
            .join("config.yaml")
    }

    #[test]
    fn parses_rust_nested_example_config() {
        let config = parse_example();

        assert_eq!(config.alist2strm_tasks.len(), 2);
        assert_eq!(config.ani2alist_tasks.len(), 3);
        assert_eq!(config.library_poster_tasks.len(), 1);
        assert_eq!(config.media_servers.len(), 1);
        assert_eq!(config.alist.len(), 3);
        assert_eq!(config.alist[0].id, "我的Alist");
        assert_eq!(config.alist[0].base_url, "http://alist:5244");
        assert_eq!(config.alist[0].wait_time, 0.0);
        assert_eq!(config.alist2strm_tasks[0].alist, "我的Alist");
        assert_eq!(config.alist2strm_tasks[0].source_dir, "/ani");
        // 并发 / 下载 / 同步已上移到全局设置，任务里不再有这些字段。
        assert_eq!(config.alist2strm.concurrency, 50);
        assert_eq!(
            config.alist2strm.strm_ext,
            crate::extensions::VIDEO_EXTS
                .iter()
                .map(|ext| (*ext).to_string())
                .collect::<Vec<_>>(),
            "示例配置的生成后缀应与默认视频后缀保持一致"
        );
        assert!(!config.alist2strm.download.enable);
        assert_eq!(config.alist2strm.download.concurrency, 5);
        assert!(
            config
                .alist2strm
                .sync
                .as_ref()
                .expect("sync config should exist")
                .enabled
        );
        assert!(
            config
                .alist2strm
                .sync
                .as_ref()
                .and_then(|sync| sync.smart_protection.as_ref())
                .expect("sync smart protection should exist")
                .enabled
        );
        assert_eq!(config.ani2alist_tasks[0].alist, "OpenList");
        assert_eq!(
            config.ani2alist_tasks[0].source.rss_url,
            "https://api.ani.rip/ani-download.xml"
        );
        assert_eq!(config.library_poster_tasks[0].server, "我的Jellyfin");
        assert_eq!(
            config.library_poster_tasks[0].render.style,
            library_poster::Style::Collage
        );
    }

    #[test]
    fn legacy_task_level_alist2strm_fields_are_ignored() {
        // 旧配置把并发 / 下载 / 同步写在任务里：应能正常解析（不报错），
        // 但取值来自全局段，任务级字段被忽略。
        let yaml = "\
alist2strm_tasks:
  - id: 旧任务
    alist: 我的Alist
    source_dir: /ani
    target_dir: /media/ani
    concurrency: 20
    download:
      enable: true
      subtitle: true
    sync:
      enabled: true
    strm_ext: [.iso]
";
        let config = Config::from_yaml(yaml).expect("legacy config should parse");
        assert_eq!(config.alist2strm_tasks.len(), 1);
        assert_eq!(config.alist2strm.concurrency, 50, "任务级并发应被忽略");
        assert!(!config.alist2strm.download.enable, "任务级下载应被忽略");
        assert!(config.alist2strm.sync.is_none(), "任务级同步应被忽略");
        assert!(
            config.alist2strm.strm_ext.is_empty(),
            "任务级 strm_ext 应被忽略"
        );
    }

    #[test]
    fn validates_global_alist2strm_ranges() {
        let yaml = "\
alist2strm:
  concurrency: 0
  download:
    concurrency: 0
  sync:
    enabled: true
    smart_protection:
      enabled: true
      threshold: 0
      grace_scans: 0
";
        let config = Config::from_yaml(yaml).expect("config should parse");
        let fields: Vec<String> = config.validate().into_iter().map(|it| it.field).collect();
        assert!(fields.contains(&"alist2strm.concurrency".to_string()));
        assert!(fields.contains(&"alist2strm.download.concurrency".to_string()));
        assert!(fields.contains(&"alist2strm.sync.smart_protection.threshold".to_string()));
        assert!(fields.contains(&"alist2strm.sync.smart_protection.grace_scans".to_string()));
    }

    #[test]
    fn empty_documents_yield_empty_config() {
        for text in ["", "   \n\n", "# 只有注释\n", "null", "~", "{}"] {
            let config = Config::from_yaml(text).expect("empty config should parse");
            assert!(config.is_empty(), "`{text}` 应解析为空配置");
            assert!(config.validate().is_empty(), "空配置不应产生校验问题");
        }
    }

    #[test]
    fn missing_file_yields_empty_config_and_skeleton() {
        let path = temp_path("missing");
        let config = Config::load(&path).expect("missing file should not fail");
        assert!(config.is_empty());

        let written = fs::read_to_string(&path).expect("skeleton should be written");
        assert!(written.contains("本文件允许为空"));
        assert_eq!(
            Config::from_yaml(&written)
                .expect("skeleton should parse")
                .alist
                .len(),
            0
        );

        fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn blank_existing_file_is_accepted() {
        let path = temp_path("blank");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "").unwrap();

        let config = Config::load(&path).expect("blank file should not fail");
        assert!(config.is_empty());

        fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn legacy_web_section_is_ignored() {
        let config = Config::from_yaml("web:\n  enable: true\n  listen: 0.0.0.0:1\nalist: []\n")
            .expect("legacy section should be ignored, not rejected");
        assert!(config.is_empty());
    }

    #[test]
    fn example_config_passes_validation() {
        let issues = parse_example().validate();
        assert!(
            issues.is_empty(),
            "example config should be valid: {issues:?}"
        );
    }

    #[test]
    fn rejects_duplicate_and_dangling_references() {
        let yaml = r#"
alist:
  - id: 重复
    base_url: http://a
  - id: 重复
    base_url: http://b
alist2strm_tasks:
  - id: 任务
    alist: 不存在
    source_dir: /ani
    target_dir: /media/ani
    cron: bad cron
"#;
        let config = Config::from_yaml(yaml).unwrap();
        let issues = config.validate();
        let fields = issues
            .iter()
            .map(|issue| issue.field.as_str())
            .collect::<Vec<_>>();

        assert!(fields.contains(&"alist[1].id"));
        assert!(fields.contains(&"alist2strm_tasks[0].alist"));
        assert!(fields.contains(&"alist2strm_tasks[0].cron"));
    }

    #[test]
    fn validates_library_poster_requires_output_or_upload() {
        let config = Config::from_yaml(
            r#"
media_servers:
  - id: 我的Jellyfin
    kind: jellyfin
    base_url: http://jellyfin:8096
    api_key: key
library_poster_tasks:
  - id: 海报
    server: 我的Jellyfin
    upload: false
    title_font: /fonts/ch.ttf
    subtitle_font: /fonts/en.otf
    libraries:
      - name: 动漫
"#,
        )
        .unwrap();
        let issues = config.validate();

        assert!(
            issues
                .iter()
                .any(|issue| issue.field == "library_poster_tasks[0].output_dir"),
            "expected output_dir issue: {issues:?}"
        );
    }

    #[test]
    fn saves_config_and_keeps_backup() {
        let config_path = temp_path("save");
        let config = parse_example();

        config.save(&config_path).expect("first save should work");
        assert!(config_path.exists());
        config.save(&config_path).expect("second save should work");

        let backup = backup_path(&config_path);
        assert!(backup.exists(), "backup should be created");
        let reloaded = Config::load(&config_path).expect("saved config should parse");
        assert_eq!(reloaded.alist.len(), config.alist.len());

        fs::remove_dir_all(config_path.parent().unwrap()).ok();
    }

    #[test]
    fn saved_yaml_has_no_web_section() {
        let config_path = temp_path("no-web");
        parse_example().save(&config_path).unwrap();
        let written = fs::read_to_string(&config_path).unwrap();

        assert!(
            !written.contains("web:"),
            "业务配置不应包含 web 分区:\n{written}"
        );

        fs::remove_dir_all(config_path.parent().unwrap()).ok();
    }
}
