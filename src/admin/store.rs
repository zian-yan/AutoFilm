//! 配置的共享存储。
//!
//! 同一份代码服务两份互不相干的配置：业务配置（`config.yaml`）与面板配置
//! （`web.yaml`）。二者各自持有独立的 [`Store`] 实例，因此版本号、修改时间
//! 与写盘路径都是分开的——保存其中一份不会推进另一份的版本号，也不会因为
//! 版本冲突把另一份的改动顶掉。

use std::{
    fs,
    path::{Path, PathBuf},
    sync::RwLock,
    time::SystemTime,
};

use crate::{
    config::{Config, Result},
    web_config::WebConfig,
};

/// 可被 [`Store`] 持久化的配置类型。
pub trait Persist: Clone {
    /// 把自身写到指定路径，实现方需保证原子写与权限收紧。
    fn persist(&self, path: &Path) -> Result<()>;
}

impl Persist for Config {
    fn persist(&self, path: &Path) -> Result<()> {
        self.save(path)
    }
}

impl Persist for WebConfig {
    fn persist(&self, path: &Path) -> Result<()> {
        self.save(path)
    }
}

/// 配置快照：一份可安全克隆的配置副本与其版本号。
#[derive(Debug, Clone)]
pub struct Snapshot<T> {
    pub config: T,
    pub version: u64,
}

/// 一份配置的共享存储。
///
/// 配置本体保存在 `RwLock` 中，写操作由热重载内核串行执行；每次成功应用都
/// 会把版本号加一，前端据此实现乐观并发控制（版本不一致时返回 409）。
pub struct Store<T> {
    path: PathBuf,
    state: RwLock<State<T>>,
}

#[derive(Debug)]
struct State<T> {
    config: T,
    version: u64,
    /// 最近一次「已应用」的配置文件修改时间，用于判断磁盘上是否出现了新改动。
    applied_mtime: Option<SystemTime>,
}

impl<T: Persist> Store<T> {
    pub fn new(path: PathBuf, config: T) -> Self {
        let applied_mtime = file_mtime(&path);
        Self {
            path,
            state: RwLock::new(State {
                config,
                version: 1,
                applied_mtime,
            }),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 读取当前配置快照。
    pub fn snapshot(&self) -> Snapshot<T> {
        let state = self.state.read().unwrap_or_else(|err| err.into_inner());
        Snapshot {
            config: state.config.clone(),
            version: state.version,
        }
    }

    /// 读取当前配置版本号。
    pub fn version(&self) -> u64 {
        self.state
            .read()
            .unwrap_or_else(|err| err.into_inner())
            .version
    }

    /// 持久化并切换到新配置，成功后返回新版本号。
    ///
    /// 写盘失败时不会修改内存中的配置，调用方可以继续使用旧配置运行。
    pub fn apply(&self, config: T) -> Result<u64> {
        config.persist(&self.path)?;
        let mtime = file_mtime(&self.path);

        let mut state = self.state.write().unwrap_or_else(|err| err.into_inner());
        state.config = config;
        state.version += 1;
        state.applied_mtime = mtime;
        Ok(state.version)
    }

    /// 采用一份来自磁盘的配置：只更新内存与版本号，不回写文件。
    ///
    /// 用于「配置文件被外部编辑」与「手工重新加载」两种来源——文件内容已经是
    /// 用户想要的形态，回写只会把注释与排版抹掉，还会顺带改动文件时间。
    pub fn adopt(&self, config: T) -> u64 {
        let mtime = file_mtime(&self.path);

        let mut state = self.state.write().unwrap_or_else(|err| err.into_inner());
        state.config = config;
        state.version += 1;
        state.applied_mtime = mtime;
        state.version
    }

    /// 判断磁盘上的配置文件是否出现了尚未应用的改动。
    pub fn config_changed_on_disk(&self) -> bool {
        let current = file_mtime(&self.path);
        let applied = self
            .state
            .read()
            .unwrap_or_else(|err| err.into_inner())
            .applied_mtime;

        match (current, applied) {
            (Some(current), Some(applied)) => current > applied,
            (Some(_), None) => true,
            _ => false,
        }
    }

    /// 把当前文件修改时间标记为已处理，避免同一份内容被反复重试。
    pub fn mark_seen(&self) {
        let mtime = file_mtime(&self.path);
        let mut state = self.state.write().unwrap_or_else(|err| err.into_inner());
        state.applied_mtime = mtime;
    }
}

/// 业务配置存储（`config.yaml`）。
pub type ConfigStore = Store<Config>;

/// 面板配置存储（`web.yaml`）。
pub type WebStore = Store<WebConfig>;

fn file_mtime(path: &Path) -> Option<SystemTime> {
    fs::metadata(path)
        .ok()
        .and_then(|meta| meta.modified().ok())
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;

    /// 每个用例一个独立目录，避免并行执行时互相覆盖。
    fn temp_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "autofilm-store-{name}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time should be after unix epoch")
                .as_nanos()
        ))
    }

    #[test]
    fn stores_are_isolated_from_each_other() {
        let dir = temp_dir("isolated");
        let config_path = dir.join("config.yaml");
        let web_path = dir.join("web.yaml");

        let business = ConfigStore::new(config_path.clone(), Config::default());
        let web = WebStore::new(web_path.clone(), WebConfig::default());

        // 保存业务配置：只应写出 config.yaml，且版本号只推进自己。
        let business_version = business.apply(Config::default()).expect("business save");
        assert_eq!(business_version, 2);
        assert_eq!(web.version(), 1, "保存业务配置不应刷新面板配置版本");
        assert!(config_path.exists());
        assert!(!web_path.exists(), "保存业务配置不应创建 web.yaml");

        // 保存面板配置：只应写出 web.yaml。
        let web_version = web
            .apply(WebConfig {
                enable: true,
                ..WebConfig::default()
            })
            .expect("web save");
        assert_eq!(web_version, 2);
        assert_eq!(business.version(), 2, "保存面板配置不应刷新业务配置版本");
        assert!(web_path.exists());

        let web_text = fs::read_to_string(&web_path).expect("web.yaml should exist");
        assert!(web_text.contains("enable: true"));
        assert!(
            !web_text.contains("alist"),
            "web.yaml 不应包含任何业务字段:\n{web_text}"
        );

        let config_text = fs::read_to_string(&config_path).expect("config.yaml should exist");
        assert!(
            !config_text.contains("listen"),
            "config.yaml 不应包含面板字段:\n{config_text}"
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn adopt_updates_memory_without_writing() {
        let dir = temp_dir("adopt");
        let web_path = dir.join("web.yaml");
        let store = WebStore::new(web_path.clone(), WebConfig::default());

        let version = store.adopt(WebConfig {
            listen: "127.0.0.1:9999".to_string(),
            ..WebConfig::default()
        });

        assert_eq!(version, 2);
        assert_eq!(store.snapshot().config.listen, "127.0.0.1:9999");
        assert!(!web_path.exists(), "adopt 不应回写文件");

        fs::remove_dir_all(&dir).ok();
    }
}
