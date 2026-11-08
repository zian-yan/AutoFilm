use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};

use alist::Client;
use tracing::{debug, error, warn};

use crate::{
    alist::{AlistConfig, build_client},
    media_server,
};

/// 任务级并发闸门。
///
/// 定时任务的执行时长可能超过 cron 间隔（例如媒体库很大时一次同步要跑几十分）。
/// 没有互斥时同一任务会被重复触发，两轮任务会同时读写同一批 `.strm` 与保护
/// 状态文件，既可能互相覆盖也可能造成误删。
#[derive(Debug, Default)]
pub struct TaskRunGuard {
    running: Arc<AtomicBool>,
}

impl TaskRunGuard {
    /// 尝试占用执行权；已有实例在运行时返回 `None`。
    pub fn try_acquire(&self) -> Option<TaskRunSlot> {
        self.running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| TaskRunSlot {
                running: self.running.clone(),
            })
    }

    /// 当前是否有实例正在运行（面板据此展示「运行中」）。
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }
}

/// 执行权凭据，离开作用域时自动释放。
pub struct TaskRunSlot {
    running: Arc<AtomicBool>,
}

impl Drop for TaskRunSlot {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
    }
}

/// 全进程共享的任务闸门表。
///
/// 热重载会重建调度器；如果闸门只挂在单次调度器实例上，旧调度器里仍在运行的
/// 任务会和新调度器的同类任务并发执行。这里按「模块:任务 ID」在进程内共享闸门，
/// 让热重载前后的同一任务依然互斥。
static RUN_GUARDS: OnceLock<Mutex<HashMap<String, Arc<TaskRunGuard>>>> = OnceLock::new();

/// 取得（或创建）指定任务的共享闸门。
pub fn shared_guard(key: &str) -> Arc<TaskRunGuard> {
    let registry = RUN_GUARDS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut registry = registry.lock().unwrap_or_else(|err| err.into_inner());
    registry.entry(key.to_string()).or_default().clone()
}

/// 列出当前正在运行的任务键（`模块:任务 ID`），供面板展示运行状态。
pub fn running_keys() -> Vec<String> {
    let registry = RUN_GUARDS.get_or_init(|| Mutex::new(HashMap::new()));
    let registry = registry.lock().unwrap_or_else(|err| err.into_inner());
    let mut keys: Vec<String> = registry
        .iter()
        .filter(|(_, guard)| guard.is_running())
        .map(|(key, _)| key.clone())
        .collect();
    keys.sort();
    keys
}

pub async fn create_alist_clients(
    alist_configs: &Vec<AlistConfig>,
) -> HashMap<String, (Arc<Client>, String)> {
    let mut alist_clients = HashMap::new();

    for alist_config in alist_configs {
        if alist_clients.contains_key(&alist_config.id) {
            warn!(
                alist = %alist_config.id,
                "AList 客户端 ID 重复，已跳过后续重复配置"
            );
            continue;
        }

        let server_url = alist_config
            .public_url
            .clone()
            .unwrap_or_else(|| alist_config.base_url.clone());
        match build_client(&alist_config).await {
            Ok(client) => {
                debug!(
                    id = %alist_config.id,
                    base_url = %alist_config.base_url,
                    public_url = ?alist_config.public_url,
                    server_url = %server_url,
                    "成功创建 AList 客户端",
                );
                alist_clients.insert(alist_config.id.clone(), (Arc::new(client), server_url));
            }
            Err(err) => {
                error!(
                    id = %alist_config.id,
                    error = %err,
                    "创建 AList 客户端失败，引用该客户端的任务将被跳过"
                );
            }
        }
    }

    alist_clients
}

/// 创建可复用的媒体服务器客户端，调用方直接获得不可变映射。
pub fn create_media_server_clients(
    configs: &[media_server::Config],
) -> HashMap<String, Arc<media_server::Client>> {
    configs.iter().fold(HashMap::new(), |mut clients, config| {
        if clients.contains_key(&config.id) {
            warn!(
                server = %config.id,
                "媒体服务器 ID 重复，已跳过后续重复配置"
            );
            return clients;
        }

        match media_server::Client::new(config.clone()) {
            Ok(client) => {
                debug!(
                    id = %client.id(),
                    kind = ?client.kind(),
                    base_url = %config.base_url,
                    "成功创建媒体服务器客户端"
                );
                clients.insert(config.id.clone(), Arc::new(client));
            }
            Err(err) => {
                error!(
                    id = %config.id,
                    error = %err,
                    "创建媒体服务器客户端失败，引用该服务器的任务将被跳过"
                );
            }
        }
        clients
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media_server::Kind;

    #[test]
    fn guards_are_mutually_exclusive_and_shared_by_key() {
        let guard = shared_guard("test:任务");
        let other_reference = shared_guard("test:任务");
        assert!(Arc::ptr_eq(&guard, &other_reference));

        let slot = guard.try_acquire().expect("首次应能占用执行权");
        assert!(
            other_reference.try_acquire().is_none(),
            "运行中的任务不应被重复调度"
        );
        drop(slot);
        assert!(guard.try_acquire().is_some(), "任务结束后应重新获得执行权");

        // 不同任务互不影响。
        let other = shared_guard("test:另一个任务");
        assert!(other.try_acquire().is_some());
    }

    #[test]
    fn creates_unique_media_server_clients() {
        let configs = vec![
            media_server::Config {
                id: "server".to_string(),
                kind: Kind::Jellyfin,
                base_url: "http://localhost:8096".to_string(),
                api_key: "secret".to_string(),
                user_id: None,
                timeout: 30,
            },
            media_server::Config {
                id: "server".to_string(),
                kind: Kind::Emby,
                base_url: "http://localhost:8097".to_string(),
                api_key: "secret".to_string(),
                user_id: None,
                timeout: 30,
            },
        ];

        let clients = create_media_server_clients(&configs);

        assert_eq!(clients.len(), 1);
        assert_eq!(clients["server"].kind(), Kind::Jellyfin);
    }
}
