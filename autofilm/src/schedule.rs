pub(crate) mod utils;

use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
};

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use croner::parser::{CronParser, Seconds};
use tokio_cron_scheduler::{Job, JobScheduler, JobSchedulerError};
use tracing::{Instrument, debug, error, info, warn};

use crate::{
    alist2strm::Alist2Strm, ani2alist::Ani2Alist, config::Config, library_poster::LibraryPoster,
    task_log,
};

/// Alist2Strm 任务的模块名，同时用作面板接口的 `module` 参数。
pub const MODULE_ALIST2STRM: &str = "alist2strm";
/// Ani2Alist 任务的模块名。
pub const MODULE_ANI2ALIST: &str = "ani2alist";
/// LibraryPoster 任务的模块名。
pub const MODULE_LIBRARY_POSTER: &str = "library_poster";

/// 一次任务运行的未来类型。
///
/// 定时触发（调度器回调）与手动触发（面板按钮）共用同一个入口，因此这里把
/// 签名单一化成「无返回值的可发送 future」，具体结果全部写进日志。
pub type RunFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

/// 任务执行入口。
pub type RunFn = Arc<dyn Fn() -> RunFuture + Send + Sync>;

/// 一个可在面板上手动触发的任务。
#[derive(Clone)]
pub struct RegisteredTask {
    pub module: &'static str,
    pub id: String,
    pub run: RunFn,
}

/// 本次调度器构建出的全部可执行任务，键为 `模块:任务 ID`。
///
/// 每个任务的执行入口与调度器回调共享同一份闭包，因此「立即运行」与 cron
/// 触发走的是完全相同的代码路径——包括任务级互斥闸门与日志。
#[derive(Clone, Default)]
pub struct RunRegistry {
    tasks: Arc<Mutex<HashMap<String, RegisteredTask>>>,
}

impl RunRegistry {
    fn key(module: &str, id: &str) -> String {
        format!("{module}:{id}")
    }

    fn insert(&self, task: RegisteredTask) {
        let mut tasks = self.tasks.lock().unwrap_or_else(|err| err.into_inner());
        tasks.insert(Self::key(task.module, &task.id), task);
    }

    /// 取出指定任务的执行入口。
    pub fn get(&self, module: &str, id: &str) -> Option<RegisteredTask> {
        let tasks = self.tasks.lock().unwrap_or_else(|err| err.into_inner());
        tasks.get(&Self::key(module, id)).cloned()
    }

    /// 取出某个模块下的全部任务。
    pub fn by_module(&self, module: &str) -> Vec<RegisteredTask> {
        let tasks = self.tasks.lock().unwrap_or_else(|err| err.into_inner());
        tasks
            .values()
            .filter(|task| task.module == module)
            .cloned()
            .collect()
    }

    /// 当前注册的全部任务键（`模块:任务 ID`），供热重载后清理陈旧缓存与闸门。
    pub fn keys(&self) -> Vec<String> {
        let tasks = self.tasks.lock().unwrap_or_else(|err| err.into_inner());
        tasks.keys().cloned().collect()
    }
}

/// 计算某个 cron 表达式在给定时区下的下次运行时间。
///
/// 解析参数与 `tokio-cron-scheduler` 内部保持一致（秒必填、日与周同时生效），
/// 面板展示的时间因此与调度器实际触发时间一致；表达式非法时返回 `None`，
/// 由校验阶段提示用户修正。
pub fn next_run_at(cron: &str, tz: Tz) -> Option<DateTime<Tz>> {
    let schedule = CronParser::builder()
        .seconds(Seconds::Required)
        .dom_and_dow(true)
        .build()
        .parse(cron)
        .ok()?;
    let now = Utc::now().with_timezone(&tz);
    schedule.find_next_occurrence(&now, false).ok()
}

/// 当前正在运行的任务键集合（`模块:任务 ID`）。
pub fn running_tasks() -> Vec<String> {
    utils::running_keys()
}

/// 给任务执行入口挂上任务日志 span。
///
/// 包装后的 `run` 同时交给 Registry（面板「立即运行」）和 cron Job（定时触发），
/// 因此两种触发方式产生的 tracing 事件都会归到同一个任务的日志文件里。
fn with_task_log(run: RunFn, key: String) -> RunFn {
    Arc::new(move || -> RunFuture {
        let future = run();
        Box::pin(future.instrument(task_log::task_span(&key)))
    })
}

/// 一次调度器构建的产物。
///
/// 除调度器本身外，还把「可手动触发的任务入口」与「已登录的 AList 客户端」
/// 一并交给面板：前者用于立即运行，后者用于在编辑配置时浏览远端目录。
pub struct Scheduled {
    pub scheduler: JobScheduler,
    pub scheduled_count: usize,
    pub runners: RunRegistry,
    pub alist_clients: HashMap<String, (Arc<alist::Client>, String)>,
}

pub async fn create_scheduler(config: Config, tz: Tz) -> Result<Scheduled, JobSchedulerError> {
    let scheduler = JobScheduler::new().await?;
    let registry = RunRegistry::default();
    let mut scheduled_count = 0usize;

    // key是AList客户端ID，value是(Client, 服务器URL)二元组
    let alist_clients = utils::create_alist_clients(&config.alist).await;
    let media_server_clients = utils::create_media_server_clients(&config.media_servers);

    let mut alist2strm_scheduled_count = 0usize;
    // 全局并发 / 下载 / 同步策略对所有同步任务共用，先克隆一份供每个 runner 持有。
    let alist2strm_settings = config.alist2strm.clone();
    for task in config.alist2strm_tasks {
        let task_id = task.id.clone();
        let cron = task.cron.clone();

        let Some((client, server_url)) = alist_clients.get(&task.alist) else {
            error!(
                task_id = %task_id,
                alist = %task.alist,
                "Alist2Strm 任务引用的 AList 客户端不存在，已跳过任务"
            );
            continue;
        };

        if cron.is_none() {
            warn!(task_id = %task_id, "Alist2Strm 任务未设置 cron，只能手动运行");
        }

        debug!(
            task_id = %task_id,
            alist = %task.alist,
            source_dir = %task.source_dir,
            target_dir = %task.target_dir.display(),
            "成功解析 Alist2Strm 任务配置"
        );

        let runner = Arc::new(Alist2Strm::new(
            task,
            alist2strm_settings.clone(),
            client.clone(),
            server_url.clone(),
        ));
        let guard = utils::shared_guard(&format!("{MODULE_ALIST2STRM}:{task_id}"));
        let run: RunFn = {
            let runner = runner.clone();
            let guard = guard.clone();
            let task_id = task_id.clone();
            Arc::new(move || {
                let runner = runner.clone();
                let guard = guard.clone();
                let task_id = task_id.clone();
                Box::pin(async move {
                    let Some(_slot) = guard.try_acquire() else {
                        warn!("上一次任务仍在运行，跳过本次执行: {task_id}");
                        return;
                    };
                    match runner.run().await {
                        Err(err) => {
                            error!("任务失败: {task_id}（{err}）");
                        }
                        Ok(summary) => {
                            // 面向人的汇总由 runner 负责（三段式中文行）；这里只保留
                            // 排障需要的明细，默认 INFO 下不刷屏。
                            debug!(
                               task_id = %summary.task_id,
                               source_dir = %summary.source_dir,
                               target_dir = %summary.target_dir.display(),
                               start_time = %&summary.start_time.with_timezone(&tz),
                               end_time = %&summary.end_time.with_timezone(&tz),
                               duration = ?summary.duration,
                               scanned_dir_count = summary.scanned_dir_count,
                               skipped_dir_count = summary.skipped_dir_count,
                               failed_dir_count = summary.failed_dir_count,
                               discovered_file_count = summary.discovered_file_count,
                               matched_file_count = summary.matched_file_count,
                               filtered_file_count = summary.filtered_file_count,
                               bdmv_collection_count = summary.bdmv_collection_count,
                               bdmv_selected_count = summary.bdmv_selected_count,
                               strm_created_count = summary.strm_created_count,
                               strm_updated_count = summary.strm_updated_count,
                               strm_skipped_count = summary.strm_skipped_count,
                               attachment_downloaded_count = summary.attachment_downloaded_count,
                               attachment_updated_count = summary.attachment_updated_count,
                               attachment_skipped_count = summary.attachment_skipped_count,
                               local_deleted_count = summary.local_deleted_count,
                               local_delete_ignored_count = summary.local_delete_ignored_count,
                               failed_path_count = summary.failed_path_count,
                               "Alist2Strm 任务统计明细"
                            );
                        }
                    }
                })
            })
        };
        let run = with_task_log(run, task_log::task_key(MODULE_ALIST2STRM, &task_id));

        registry.insert(RegisteredTask {
            module: MODULE_ALIST2STRM,
            id: task_id.clone(),
            run: run.clone(),
        });

        if let Some(cron) = cron {
            info!(task_id = %task_id, cron = %cron, "添加 Alist2Strm 定时任务");
            scheduler
                .add(Job::new_async_tz(cron, tz, move |_uuid, _lock| run())?)
                .await?;
            alist2strm_scheduled_count += 1;
            scheduled_count += 1;
        }
    }

    info!(
        module = "Alist2Strm",
        scheduled_count = alist2strm_scheduled_count,
        "子任务调度完成"
    );

    let mut ani2alist_scheduled_count = 0usize;
    for task in config.ani2alist_tasks {
        let task_id = task.id.clone();
        let cron = task.cron.clone();

        let Some((client, _server_url)) = alist_clients.get(&task.alist) else {
            error!(
                task_id = %task_id,
                alist = %task.alist,
                "Ani2Alist 任务引用的 AList 客户端不存在，已跳过任务"
            );
            continue;
        };

        if cron.is_none() {
            warn!(task_id = %task_id, "Ani2Alist 任务未设置 cron，只能手动运行");
        }

        debug!(
            task_id = %task_id,
            alist = %task.alist,
            target_dir = %task.target_dir,
            "成功解析 Ani2Alist 任务配置"
        );

        let runner = Arc::new(Ani2Alist::new(task, client.clone(), tz));
        let guard = utils::shared_guard(&format!("{MODULE_ANI2ALIST}:{task_id}"));
        let run: RunFn = {
            let runner = runner.clone();
            let guard = guard.clone();
            let task_id = task_id.clone();
            Arc::new(move || {
                let runner = runner.clone();
                let guard = guard.clone();
                let task_id = task_id.clone();
                Box::pin(async move {
                    let Some(_slot) = guard.try_acquire() else {
                        warn!(
                            task_id = %task_id,
                            "上一次 Ani2Alist 任务仍在运行，跳过本次执行"
                        );
                        return;
                    };
                    info!(task_id = %task_id, "开始执行 Ani2Alist 任务");
                    match runner.run().await {
                        Err(err) => {
                            error!(task_id = %task_id, error = %err, "Ani2Alist 任务失败");
                        }
                        Ok(()) => {
                            info!(task_id = %task_id, "Ani2Alist 任务完成");
                        }
                    }
                })
            })
        };
        let run = with_task_log(run, task_log::task_key(MODULE_ANI2ALIST, &task_id));

        registry.insert(RegisteredTask {
            module: MODULE_ANI2ALIST,
            id: task_id.clone(),
            run: run.clone(),
        });

        if let Some(cron) = cron {
            info!(task_id = %task_id, cron = %cron, "添加 Ani2Alist 定时任务");
            scheduler
                .add(Job::new_async_tz(cron, tz, move |_uuid, _lock| run())?)
                .await?;
            ani2alist_scheduled_count += 1;
            scheduled_count += 1;
        }
    }

    info!(
        module = "Ani2Alist",
        scheduled_count = ani2alist_scheduled_count,
        "子任务调度完成"
    );

    let mut library_poster_scheduled_count = 0usize;
    for task in config.library_poster_tasks {
        let task_id = task.id.clone();
        let cron = task.cron.clone();

        let Some(client) = media_server_clients.get(&task.server) else {
            error!(
                task_id = %task_id,
                server = %task.server,
                "LibraryPoster 任务引用的媒体服务器不存在，已跳过任务"
            );
            continue;
        };
        let runner = match LibraryPoster::new(task, client.clone()) {
            Ok(runner) => Arc::new(runner),
            Err(err) => {
                error!(
                    task_id = %task_id,
                    error = %err,
                    "初始化 LibraryPoster 任务失败，已跳过任务"
                );
                continue;
            }
        };

        if cron.is_none() {
            warn!(task_id = %task_id, "LibraryPoster 任务未设置 cron，只能手动运行");
        }

        let guard = utils::shared_guard(&format!("{MODULE_LIBRARY_POSTER}:{task_id}"));
        let run: RunFn = {
            let runner = runner.clone();
            let guard = guard.clone();
            let task_id = task_id.clone();
            Arc::new(move || {
                let runner = runner.clone();
                let guard = guard.clone();
                let task_id = task_id.clone();
                Box::pin(async move {
                    let Some(_slot) = guard.try_acquire() else {
                        warn!(
                            task_id = %task_id,
                            "上一次 LibraryPoster 任务仍在运行，跳过本次执行"
                        );
                        return;
                    };
                    info!(task_id = %task_id, "开始执行 LibraryPoster 任务");
                    match runner.run().await {
                        Ok(summary) => {
                            info!(
                                task_id = %summary.task_id,
                                library_count = summary.library_count,
                                succeeded_count = summary.succeeded_count,
                                failed_count = summary.failed_count,
                                downloaded_image_count = summary.downloaded_image_count,
                                generated_count = summary.generated_count,
                                saved_count = summary.saved_count,
                                uploaded_count = summary.uploaded_count,
                                "LibraryPoster 任务完成"
                            );
                        }
                        Err(err) => {
                            error!(
                                task_id = %task_id,
                                error = %err,
                                "LibraryPoster 任务失败"
                            );
                        }
                    }
                })
            })
        };
        let run = with_task_log(run, task_log::task_key(MODULE_LIBRARY_POSTER, &task_id));

        registry.insert(RegisteredTask {
            module: MODULE_LIBRARY_POSTER,
            id: task_id.clone(),
            run: run.clone(),
        });

        if let Some(cron) = cron {
            info!(task_id = %task_id, cron = %cron, "添加 LibraryPoster 定时任务");
            scheduler
                .add(Job::new_async_tz(cron, tz, move |_uuid, _lock| run())?)
                .await?;
            library_poster_scheduled_count += 1;
            scheduled_count += 1;
        }
    }

    info!(
        module = "LibraryPoster",
        scheduled_count = library_poster_scheduled_count,
        "子任务调度完成"
    );
    info!(scheduled_count, "全部子任务调度完成");

    // 新注册表就绪后，清掉历史配置遗留的任务闸门与任务日志内存缓存。运行中
    // 的旧任务闸门会保留，磁盘日志按保留天数清理，刚删掉的任务历史仍可回查。
    let active_keys = registry.keys();
    utils::prune_guards(&active_keys);
    task_log::prune(&active_keys);

    Ok(Scheduled {
        scheduler,
        scheduled_count,
        runners: registry,
        alist_clients,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noop_task(module: &'static str, id: &str) -> RegisteredTask {
        RegisteredTask {
            module,
            id: id.to_string(),
            run: Arc::new(|| Box::pin(async {})),
        }
    }

    #[test]
    fn registry_isolates_modules_with_same_id() {
        let registry = RunRegistry::default();
        registry.insert(noop_task(MODULE_ALIST2STRM, "国产剧"));
        registry.insert(noop_task(MODULE_ANI2ALIST, "国产剧"));

        assert!(registry.get(MODULE_ALIST2STRM, "国产剧").is_some());
        assert!(registry.get(MODULE_ANI2ALIST, "国产剧").is_some());
        assert!(registry.get(MODULE_LIBRARY_POSTER, "国产剧").is_none());
        assert_eq!(registry.by_module(MODULE_ALIST2STRM).len(), 1);
    }

    #[test]
    fn next_run_matches_six_field_cron() {
        let tz = chrono_tz::Asia::Shanghai;
        let next = next_run_at("0 0 20 * * *", tz).expect("合法的 6 段 cron 应能算出下次运行");
        assert_eq!(next.timezone(), tz);
        assert_eq!(next.format("%M:%S").to_string(), "00:00");
    }

    #[test]
    fn next_run_returns_none_for_invalid_cron() {
        assert!(next_run_at("这不是 cron", chrono_tz::Asia::Shanghai).is_none());
    }
}
