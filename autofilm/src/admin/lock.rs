//! 运行中任务的配置保护。
//!
//! 任务一旦开始执行，其 runner 持有的是启动时的配置快照；允许同时改掉这条
//! 任务的 YAML 会让「页面上看到的」与「实际在跑的」不一致，也容易在任务结束后
//! 出现一次语义完全不同的执行。因此以 `模块:任务 ID` 为粒度做保护：
//!
//! - 面板提交：若改动了运行中任务的条目，整次保存拒绝，提示等任务停止；
//! - 外部编辑 / 手工重载：把运行中任务的条目恢复成当前生效值，其余改动照常
//!   应用，避免因为一处锁定让整份配置失效。
//!
//! 只锁任务条目本身，AList 客户端、媒体服务器与全局策略仍可随时修改——它们
//! 不会影响已经持有快照的运行中任务，下一次触发才会用到新值。

use serde_json::Value;

/// 任务分区与对应模块名的映射。
const TASK_SECTIONS: &[(&str, &str)] = &[
    ("alist2strm_tasks", crate::schedule::MODULE_ALIST2STRM),
    ("ani2alist_tasks", crate::schedule::MODULE_ANI2ALIST),
    (
        "library_poster_tasks",
        crate::schedule::MODULE_LIBRARY_POSTER,
    ),
];

fn items<'a>(config: &'a Value, section: &str) -> &'a [Value] {
    config
        .get(section)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn task_id(item: &Value) -> Option<&str> {
    item.get("id").and_then(Value::as_str)
}

fn key(module: &str, id: &str) -> String {
    format!("{module}:{id}")
}

/// 找出「正在运行、但本次提交改到了」的任务键。
///
/// 条目内容变化、任务被删除、任务 ID 被改名（旧 ID 消失）都算改动。
pub fn changed_locked(current: &Value, next: &Value, running: &[String]) -> Vec<String> {
    if running.is_empty() {
        return Vec::new();
    }

    let mut changed = Vec::new();
    for &(section, module) in TASK_SECTIONS {
        let new_items = items(next, section);
        for old in items(current, section) {
            let Some(id) = task_id(old) else { continue };
            let task_key = key(module, id);
            if !running.contains(&task_key) {
                continue;
            }

            match new_items.iter().find(|new| task_id(new) == Some(id)) {
                None => changed.push(task_key),
                Some(new) if new != old => changed.push(task_key),
                Some(_) => {}
            }
        }
    }

    changed.sort();
    changed.dedup();
    changed
}

/// 把运行中任务的条目恢复成当前生效值，返回被恢复的任务键。
///
/// 任务被删除或改名时，旧条目会按其在旧配置中的位置重新插回：用户新增的条目
/// 仍然保留，因此「运行中任务不可删」不会连带丢掉本次其它改动。
pub fn preserve_locked(current: &Value, next: &mut Value, running: &[String]) -> Vec<String> {
    if running.is_empty() {
        return Vec::new();
    }

    let mut protected = Vec::new();
    for &(section, module) in TASK_SECTIONS {
        let old_items = items(current, section).to_vec();
        let Some(new_items) = next.get_mut(section).and_then(Value::as_array_mut) else {
            continue;
        };

        for (index, old) in old_items.iter().enumerate() {
            let Some(id) = task_id(old) else { continue };
            let task_key = key(module, id);
            if !running.contains(&task_key) {
                continue;
            }

            if let Some(position) = new_items.iter().position(|new| task_id(new) == Some(id)) {
                if new_items[position] != *old {
                    new_items[position] = old.clone();
                    protected.push(task_key);
                }
            } else {
                let insert_at = index.min(new_items.len());
                new_items.insert(insert_at, old.clone());
                protected.push(task_key);
            }
        }
    }

    protected.sort();
    protected.dedup();
    protected
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    const YAML: &str = "\
alist2strm_tasks:
  - id: 动漫
    cron: 0 0 20 * * *
    alist: a
    source_dir: /ani
    target_dir: /media/ani
  - id: 电影
    cron: 0 0 21 * * *
    alist: a
    source_dir: /movie
    target_dir: /media/movie
";

    fn value() -> Value {
        serde_json::to_value(Config::from_yaml(YAML).unwrap()).unwrap()
    }

    #[test]
    fn detects_edit_delete_and_rename_of_running_task() {
        let current = value();
        let running = vec!["alist2strm:动漫".to_string()];

        let mut edited = current.clone();
        edited["alist2strm_tasks"][0]["source_dir"] = Value::String("/new".into());
        assert_eq!(changed_locked(&current, &edited, &running), running);

        let mut deleted = current.clone();
        deleted["alist2strm_tasks"]
            .as_array_mut()
            .unwrap()
            .remove(0);
        assert_eq!(changed_locked(&current, &deleted, &running), running);

        let mut renamed = current.clone();
        renamed["alist2strm_tasks"][0]["id"] = Value::String("新番".into());
        assert_eq!(changed_locked(&current, &renamed, &running), running);
    }

    #[test]
    fn unrelated_changes_are_not_locked() {
        let current = value();
        let running = vec!["alist2strm:动漫".to_string()];

        let mut next = current.clone();
        next["alist2strm_tasks"][1]["source_dir"] = Value::String("/other".into());
        assert!(changed_locked(&current, &next, &running).is_empty());
        assert!(changed_locked(&current, &current, &running).is_empty());
        assert!(changed_locked(&current, &next, &[]).is_empty());
    }

    #[test]
    fn preserve_restores_only_the_running_entry() {
        let current = value();
        let running = vec!["alist2strm:动漫".to_string()];

        let mut next = current.clone();
        next["alist2strm_tasks"][0]["source_dir"] = Value::String("/hacked".into());
        next["alist2strm_tasks"][1]["source_dir"] = Value::String("/kept".into());

        assert_eq!(preserve_locked(&current, &mut next, &running), running);
        let restored: Config = serde_json::from_value(next).unwrap();
        assert_eq!(restored.alist2strm_tasks[0].source_dir, "/ani");
        assert_eq!(restored.alist2strm_tasks[1].source_dir, "/kept");
    }

    #[test]
    fn preserve_reinserts_deleted_running_task() {
        let current = value();
        let running = vec!["alist2strm:动漫".to_string()];

        let mut next = current.clone();
        next["alist2strm_tasks"].as_array_mut().unwrap().remove(0);

        assert_eq!(preserve_locked(&current, &mut next, &running), running);
        let restored: Config = serde_json::from_value(next).unwrap();
        assert_eq!(restored.alist2strm_tasks.len(), 2);
        assert_eq!(restored.alist2strm_tasks[0].id, "动漫");
    }
}
