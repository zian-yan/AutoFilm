use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, atomic::Ordering},
    time::Duration,
};

use alist::{
    Client,
    models::fs::{FsListReq, ObjResp},
};
use chrono::{DateTime, Utc};
use filetime::FileTime;
use futures_util::stream::StreamExt;
use regex::Regex;
use reqwest::StatusCode;
use tokio::{fs, io::AsyncWriteExt, sync::Semaphore};
use tracing::{debug, info, warn};

use crate::{
    alist2strm::{
        config::{Config, GlobalConfig, SmartProtectionConfig},
        errors::{Error, Result},
        path::{self, bdmv_root, download_url, is_bdmv_file, join_alist_path, suffix_of},
        protection::ProtectionManager,
        retry::RetryDirs,
        summary::{RunStats, RunSummary},
        utils::{collect_local_files, companion_file_is_stale, remove_empty_parents},
    },
    extensions::{IMAGE_EXTS, NFO_EXTS, SUBTITLE_EXTS, VIDEO_EXTS},
};

/// 单个伴生文件的下载体积上限。
///
/// 字幕、图片、nfo 远小于该值；设置上限可以防止异常或恶意的 AList 响应把
/// 本地磁盘写满。
const MAX_ATTACHMENT_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// 连接 AList 的超时时间，避免站点无响应时任务永久挂起。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone)]
pub struct Alist2Strm {
    config: Config,
    /// 全局并发 / 下载 / 同步策略，对所有同步任务共用。
    settings: GlobalConfig,
    client: Arc<Client>,
    server_url: String,
}

#[derive(Debug)]
struct RunContext {
    client: Arc<Client>,
    http: reqwest::Client,
    server_url: String,
    base_path: String,
    download_semaphore: Arc<Semaphore>,
    download_exts: HashSet<String>,
    process_exts: HashSet<String>,
    sync_ignore_pattern: Option<Regex>,
    stats: Arc<RunStats>,
    /// 上次任务失败、本轮必须无条件扫描的远端目录。
    retry_dirs: Arc<RetryDirs>,
    /// 本轮出现失败的远端目录（扫描失败 / 文件处理失败），任务结束时落盘。
    failed_dirs: Mutex<HashSet<String>>,
    /// 本轮已经确认存在的本地目录。
    ///
    /// 同一目录下往往有几十上百个文件，逐个 `create_dir_all` 会一路 stat 到根；
    /// 这里按目录去重，每个目录每轮只真正创建一次。
    created_dirs: Mutex<HashSet<PathBuf>>,
}

impl RunContext {
    /// 确保目标目录存在；同一目录在一轮任务里只真正创建一次。
    async fn ensure_dir(&self, dir: &Path) -> Result<()> {
        {
            let created = self
                .created_dirs
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            if created.contains(dir) {
                return Ok(());
            }
        }

        fs::create_dir_all(dir).await?;
        self.created_dirs
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .insert(dir.to_path_buf());
        Ok(())
    }

    /// 记录一个本轮失败的远端目录，供下次任务强制重扫。
    fn note_failed_dir(&self, dir: &str) {
        let mut failed = self
            .failed_dirs
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        failed.insert(dir.to_string());
    }

    /// 本轮失败目录的快照。
    fn failed_dir_snapshot(&self) -> HashSet<String> {
        self.failed_dirs
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clone()
    }
}

/// 扫描阶段对单个远端文件的处理决策。
///
/// BDMV 主片必须等同目录所有 m2ts 收集完成后才能按大小选片，因此单独分出
/// `Bdmv` 分支，避免提前进入并发处理队列。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathDecision {
    /// 加入当前目录批次，与同目录的其它文件一起并发处理。
    Process,
    /// 属于 BDMV 主片候选，先收集，扫描结束后统一选片。
    Bdmv,
    /// 直接跳过该文件。
    Skip,
}

impl Alist2Strm {
    /// 创建一个 Alist2Strm 任务执行器。
    ///
    /// 执行器保存任务配置、全局设置和共享的 AList 客户端；真正的扫描和本地
    /// 文件处理发生在 `run` 中。
    pub fn new(
        config: Config,
        settings: GlobalConfig,
        client: Arc<Client>,
        server_url: String,
    ) -> Self {
        Self {
            config,
            settings,
            client,
            server_url,
        }
    }

    /// 执行一次完整的 Alist2Strm 同步任务。
    ///
    /// 流程包括创建 AList/HTTP 上下文、按目录分批扫描并处理普通文件、
    /// 收集并处理 BDMV 主片、按需清理本地过期文件。单个远端目录或文件失败会
    /// 记录日志并跳过，初始化和本地清理等关键错误仍会返回给调度器。
    pub async fn run(&self) -> Result<RunSummary> {
        // 每次 run 都重新创建轻量上下文，确保 base_path 和配置派生项是当前值。
        let stats = Arc::new(RunStats::new());
        self.log_start();
        // 上次没处理完的目录本轮无条件重扫，避免被「目录时间检查」永久跳过。
        let retry = Arc::new(RetryDirs::load(&self.config.target_dir, &self.config.id).await);
        if !retry.is_empty() {
            info!(
                "上次任务有 {} 个目录未完成，本轮强制重扫，不受目录时间检查影响",
                retry.len()
            );
        }
        let context = Arc::new(self.create_context(stats.clone(), retry.clone()).await?);
        info!(">>> 开始生成");
        let mut processed_local_paths = HashSet::new();
        let mut bdmv_collections: HashMap<String, Vec<(String, ObjResp)>> = HashMap::new();
        // 记录扫描失败的远端目录，用于把对应本地子树排除出删除集合。
        let mut unscanned_dirs: Vec<String> = Vec::new();
        // 因目录时间未变化而整棵跳过的目录，同样要排除在清理之外。
        let mut time_skipped_dirs: Vec<String> = Vec::new();
        // 远端目录 → (本地目录, 远端时间)：目录时间要等全部子项落盘后再统一设置。
        let mut dir_times: Vec<(PathBuf, DateTime<Utc>)> = Vec::new();

        // 第一阶段边遍历远端文件边处理普通文件；BDMV 文件先收集，扫描结束后再选主片处理。
        self.collect_and_process_paths(
            context.clone(),
            &mut processed_local_paths,
            &mut bdmv_collections,
            &mut unscanned_dirs,
            &mut time_skipped_dirs,
            &mut dir_times,
        )
        .await?;

        if self.config.dir_time_check {
            info!(
                "目录时间检查：跳过 {} 个未变化的目录",
                time_skipped_dirs.len()
            );
        }

        // 第二阶段为每个 BDMV 目录挑选最大的 m2ts，生成单个电影标题 .strm。
        let bdmv_paths = self.finalize_bdmv_paths(bdmv_collections);
        context
            .stats
            .bdmv_selected_count
            .store(bdmv_paths.len(), Ordering::Relaxed);
        if !bdmv_paths.is_empty() {
            info!(">>> 开始处理 BDMV 主片，共 {} 个", bdmv_paths.len());
        }
        for (full_path, object) in &bdmv_paths {
            let local_path = self.local_path(full_path, &object.name);
            processed_local_paths.insert(local_path);
        }
        self.process_paths(context.clone(), bdmv_paths).await?;

        if self.sync_enabled() {
            info!(">>> 开始清理");
            self.cleanup_after_scan(
                &context,
                processed_local_paths,
                &unscanned_dirs,
                &time_skipped_dirs,
            )
            .await?;
        }

        // 目录时间放在最后统一校对：新建/删除子项都会改写父目录 mtime。
        self.apply_dir_times(dir_times).await;

        // 用本轮结果覆盖待重扫记录：本轮全部成功就删掉文件，避免上轮的记录继续生效。
        let mut failed_dirs = context.failed_dir_snapshot();
        failed_dirs.extend(unscanned_dirs.iter().cloned());
        if let Err(err) = retry.replace(&failed_dirs).await {
            warn!("写入待重扫记录失败: {err}");
        }
        if failed_dirs.is_empty() {
            if !retry.is_empty() {
                info!("待重扫目录已全部处理成功，记录已清除");
            }
        } else {
            info!(
                "本轮有 {} 个目录未完成（扫描失败或文件失败），已记录待下次重扫",
                failed_dirs.len()
            );
        }

        let summary = stats.summarize(&self.config);
        self.log_summary(&summary);
        Ok(summary)
    }

    /// 打印任务抬头，便于在长日志里一眼看清这一段跑的是哪个任务、什么配置。
    fn log_start(&self) {
        info!("{}", "=".repeat(50));
        info!("开始任务: {}", self.config.id);
        info!("任务类型: 生成 STRM");
        info!("远端路径: {}", self.config.source_dir);
        info!("保存目录: {}", self.config.target_dir.display());
        info!(
            "平铺模式: {}",
            if self.config.flatten_mode {
                "开启"
            } else {
                "关闭"
            }
        );
        info!(
            "覆盖写入: {}",
            if self.config.overwrite {
                "开启"
            } else {
                "关闭"
            }
        );
        info!(
            "目录时间检查: {}",
            if self.config.dir_time_check {
                "开启"
            } else {
                "关闭"
            }
        );
        info!(
            "同步清理: {}",
            if self.sync_enabled() {
                "开启"
            } else {
                "关闭"
            }
        );
        info!("并发数: {}", self.settings.concurrency.max(1));
        info!("{}", "=".repeat(50));
    }

    /// 打印本轮汇总：沿用「生成 / 跳过 / 清理」三段式，数字一眼可比。
    ///
    /// 「跳过目录」只统计因目录时间未变化而整棵跳过的目录；列目录失败单独
    /// 记成「扫描失败目录」，两类混在一起会让人误以为同步是正常的。
    fn log_summary(&self, summary: &RunSummary) {
        info!(
            "【生成文件完成】 生成 {} 个，更新 {} 个，跳过 {} 个，跳过目录 {} 个，扫描失败目录 {} 个，文件失败 {} 个",
            summary.strm_created_count,
            summary.strm_updated_count,
            summary.strm_skipped_count + summary.attachment_skipped_count,
            summary.skipped_dir_count,
            summary.failed_dir_count,
            summary.failed_path_count
        );
        if self.sync_enabled() {
            info!(
                "【清理文件完成】 删除 {} 个，忽略 {} 个",
                summary.local_deleted_count, summary.local_delete_ignored_count
            );
        }
        info!("任务完成，用时 {}", format_duration(summary.duration));
    }

    /// 在扫描结果基础上执行本地清理。
    ///
    /// 扫描失败或被「目录时间检查」跳过的目录不能当成「远端已删除」，否则一次
    /// 网络抖动、或一次目录时间比对就会清空本地媒体库。这里把这些目录映射成
    /// 本地子树排除掉；平铺模式无法按目录区分，只能整轮跳过清理。
    async fn cleanup_after_scan(
        &self,
        context: &RunContext,
        processed_local_paths: HashSet<PathBuf>,
        unscanned_dirs: &[String],
        time_skipped_dirs: &[String],
    ) -> Result<()> {
        let mut excluded_prefixes = Vec::new();

        if !unscanned_dirs.is_empty() {
            if self.config.flatten_mode {
                warn!(
                    failed_dir_count = unscanned_dirs.len(),
                    "存在扫描失败的目录且启用了平铺模式，本轮跳过本地清理以避免误删"
                );
                return Ok(());
            }

            excluded_prefixes = unscanned_dirs
                .iter()
                .filter_map(|dir| {
                    path::local_dir(
                        &self.config.source_dir,
                        &self.config.target_dir,
                        dir,
                        self.config.flatten_mode,
                    )
                })
                .collect();
            warn!(
                failed_dir_count = unscanned_dirs.len(),
                excluded_prefix_count = excluded_prefixes.len(),
                "存在扫描失败的目录，已排除对应本地子树后执行清理"
            );
        }

        // 因目录时间未变化而跳过的子树同样不在扫描结果里，必须一并排除。
        if !time_skipped_dirs.is_empty() {
            let extra: Vec<PathBuf> = time_skipped_dirs
                .iter()
                .filter_map(|dir| {
                    path::local_dir(
                        &self.config.source_dir,
                        &self.config.target_dir,
                        dir,
                        self.config.flatten_mode,
                    )
                })
                .collect();
            debug!(
                skipped_dir_count = time_skipped_dirs.len(),
                excluded_prefix_count = extra.len(),
                "目录时间未变化的子树已排除，不参与本轮清理"
            );
            excluded_prefixes.extend(extra);
        }

        self.cleanup_local_files(context, processed_local_paths, &excluded_prefixes)
            .await
    }

    /// 创建本次运行共享的上下文。
    ///
    /// 该函数会复用 AList 客户端、读取当前用户 `base_path`、准备 HTTP 下载
    /// 客户端、计算需要处理和下载的扩展名集合，并编译同步忽略规则。
    async fn create_context(
        &self,
        stats: Arc<RunStats>,
        retry: Arc<RetryDirs>,
    ) -> Result<RunContext> {
        let client = self.client.clone();
        let me = client.me().await?;
        let download_exts = self.download_exts();
        let mut process_exts = VIDEO_EXTS
            .iter()
            .map(|ext| ext.to_string())
            .collect::<HashSet<_>>();
        process_exts.extend(download_exts.iter().cloned());
        let sync_ignore_pattern = self
            .settings
            .sync
            .as_ref()
            .and_then(|sync| sync.ignore.as_deref())
            .filter(|pattern| !pattern.trim().is_empty())
            .map(Regex::new)
            .transpose()?;

        Ok(RunContext {
            client,
            http: reqwest::Client::builder()
                .user_agent(format!("AutoFilm/{}", env!("CARGO_PKG_VERSION")))
                .connect_timeout(CONNECT_TIMEOUT)
                .build()?,
            server_url: self.server_url.clone(),
            base_path: me.base_path,
            download_semaphore: Arc::new(Semaphore::new(self.settings.download.concurrency.max(1))),
            download_exts,
            process_exts,
            sync_ignore_pattern,
            stats,
            retry_dirs: retry,
            failed_dirs: Mutex::new(HashSet::new()),
            created_dirs: Mutex::new(HashSet::new()),
        })
    }

    /// 判断某个远端目录是否可以整棵跳过：远端修改时间不晚于本地目录即可。
    ///
    /// 根目录拿不到自身时间（父列表里没有它），始终扫描；平铺模式没有对应的
    /// 本地子目录，无从比较，也一律扫描。
    async fn dir_unchanged(
        &self,
        remote_dir: &str,
        remote_modified: Option<DateTime<Utc>>,
    ) -> bool {
        if !self.config.dir_time_check {
            return false;
        }
        let Some(remote_modified) = remote_modified else {
            return false;
        };
        let Some(local_dir) = path::local_dir(
            &self.config.source_dir,
            &self.config.target_dir,
            remote_dir,
            self.config.flatten_mode,
        ) else {
            return false;
        };
        local_dir_covers(&local_dir, remote_modified).await
    }

    /// 按目录分批扫描远端目录并处理普通文件。
    ///
    /// 使用显式栈做 DFS；每读完一个目录，把该目录下的 `.strm` 与伴生文件收集成
    /// 一批，批内并发处理，**整批完成后才继续下一个目录**，保证目录之间不交错。
    /// BDMV 候选文件仍先收集到分组中，等待扫描结束后统一选择主片。
    async fn collect_and_process_paths(
        &self,
        context: Arc<RunContext>,
        processed_local_paths: &mut HashSet<PathBuf>,
        bdmv_collections: &mut HashMap<String, Vec<(String, ObjResp)>>,
        unscanned_dirs: &mut Vec<String>,
        time_skipped_dirs: &mut Vec<String>,
        dir_times: &mut Vec<(PathBuf, DateTime<Utc>)>,
    ) -> Result<()> {
        // 使用显式栈递归遍历，避免深层目录导致函数递归过深。
        // 栈元素：目录路径、目录自身的远端修改时间（根目录没有父列表，只能拿到
        // None）、是否强制扫描（上次失败的目录要连整棵子树一起重扫）。
        let root_force = context.retry_dirs.contains(&self.config.source_dir);
        let mut stack: Vec<(String, Option<DateTime<Utc>>, bool)> =
            vec![(self.config.source_dir.clone(), None, root_force)];

        while let Some((dir_path, remote_modified, force)) = stack.pop() {
            // 目录时间检查：远端目录没变过就整棵子树跳过，省掉逐层列目录的开销。
            // 上次失败过的目录无条件扫描，否则漏掉的文件永远补不回来。
            if force {
                debug!("上次失败目录，强制重扫: {dir_path}");
            } else if context.retry_dirs.has_pending_under(&dir_path) {
                debug!("子树内有待重扫目录，本轮照常扫描: {dir_path}");
            } else if self.dir_unchanged(&dir_path, remote_modified).await {
                RunStats::inc(&context.stats.skipped_dir_count);
                // 跳过的目录同样没参与扫描，清理时必须排除，否则本地文件会被当成已删除。
                time_skipped_dirs.push(dir_path.clone());
                // 顺手把目录时间校正到远端：跳过 ≠ 时间一定对得上（例如上一轮被写入子项刷过）。
                if let Some(remote_modified) = remote_modified {
                    if let Some(local_dir) = path::local_dir(
                        &self.config.source_dir,
                        &self.config.target_dir,
                        &dir_path,
                        self.config.flatten_mode,
                    ) {
                        dir_times.push((local_dir, remote_modified));
                    }
                }
                debug!("目录时间未变化，跳过: {dir_path}");
                continue;
            }
            info!("读取目录: {dir_path}");
            let resp = match context.client.fs_list(FsListReq::all(&dir_path)).await {
                Ok(resp) => resp,
                Err(err) => {
                    RunStats::inc(&context.stats.failed_dir_count);
                    // 该目录下的文件状态未知，不能参与后续删除判定。
                    unscanned_dirs.push(dir_path.clone());
                    context.note_failed_dir(&dir_path);
                    warn!("扫描失败目录: {dir_path}（{err}），已记入待重扫");
                    continue;
                }
            };
            RunStats::inc(&context.stats.scanned_dir_count);
            debug!(
                task_id = %self.config.id,
                dir_path = %dir_path,
                total = resp.total,
                item_count = resp.content.len(),
                "AList 目录扫描完成"
            );
            // 本目录的一批文件：视频的 .strm 与字幕 / 图片 / NFO 等伴生文件
            // 混在同一批里，保证二者「同时生成」。
            let mut batch: Vec<(String, ObjResp)> = Vec::new();
            for item in resp.content {
                let object = item.object;
                let full_path = join_alist_path(&dir_path, &object.name);

                if object.is_dir {
                    debug!(
                        task_id = %self.config.id,
                        dir_path = %full_path,
                        "发现 AList 子目录，加入扫描队列"
                    );
                    // 平铺模式下没有本地子目录，local_dir 返回 None，自然跳过。
                    if let Some(time) = remote_dir_time(&object) {
                        if let Some(local_dir) = path::local_dir(
                            &self.config.source_dir,
                            &self.config.target_dir,
                            &full_path,
                            self.config.flatten_mode,
                        ) {
                            dir_times.push((local_dir, time));
                        }
                    }
                    let child_force = force || context.retry_dirs.contains(&full_path);
                    stack.push((full_path, Some(object.modified), child_force));
                    continue;
                }
                RunStats::inc(&context.stats.discovered_file_count);

                match self
                    .classify_path(&context, &full_path, &object, processed_local_paths)
                    .await
                {
                    Ok(PathDecision::Process) => {
                        debug!(
                            task_id = %self.config.id,
                            path = %full_path,
                            "AList 路径加入当前目录批次"
                        );
                        batch.push((full_path, object));
                    }
                    Ok(PathDecision::Bdmv) => {
                        // BDMV 需要等同目录所有 m2ts 都收集完，才能按大小选主片。
                        if let Some(root) = bdmv_root(&full_path) {
                            debug!(
                                task_id = %self.config.id,
                                path = %full_path,
                                "收集 BDMV 主片候选文件"
                            );
                            bdmv_collections
                                .entry(root)
                                .or_default()
                                .push((full_path, object));
                            RunStats::inc(&context.stats.bdmv_collection_count);
                        }
                    }
                    Ok(PathDecision::Skip) => {}
                    Err(err) => {
                        RunStats::inc(&context.stats.failed_path_count);
                        context.note_failed_dir(remote_parent(&full_path));
                        warn!("判断是否需要处理失败，已跳过 {full_path}（{err}）");
                    }
                }
            }

            // 目录之间的屏障：等这一批全部落盘，才 pop 下一个目录。
            if !batch.is_empty() {
                let count = batch.len();
                self.process_paths(context.clone(), batch).await?;
                info!("目录处理完成: {dir_path}（{count} 个文件）");
            }
        }

        Ok(())
    }

    /// 判断一个远端文件应该如何进入处理流程。
    ///
    /// 函数会过滤系统文件、非目标扩展名、BDMV 内部非主片候选文件，并维护
    /// `processed_local_paths` 用于后续同步清理。对于本地已存在的文件，
    /// `overwrite=false` 时会跳过；伴生文件如果过期或大小偏小则仍会重新处理。
    async fn classify_path(
        &self,
        context: &RunContext,
        full_path: &str,
        object: &ObjResp,
        processed_local_paths: &mut HashSet<PathBuf>,
    ) -> Result<PathDecision> {
        if full_path.contains("@eaDir")
            || full_path.contains("Thumbs.db")
            || full_path.contains(".DS_Store")
        {
            debug!(path = %full_path, "跳过系统文件");
            RunStats::inc(&context.stats.filtered_file_count);
            return Ok(PathDecision::Skip);
        }

        if full_path.contains("/BDMV/") && !is_bdmv_file(full_path, &object.name) {
            debug!(path = %full_path, "跳过 BDMV 内部非主片候选文件");
            RunStats::inc(&context.stats.filtered_file_count);
            return Ok(PathDecision::Skip);
        }

        let suffix = suffix_of(&object.name);
        if !context.process_exts.contains(&suffix) {
            debug!(path = %full_path, suffix = %suffix, "文件后缀不在处理列表中");
            RunStats::inc(&context.stats.filtered_file_count);
            return Ok(PathDecision::Skip);
        }
        RunStats::inc(&context.stats.matched_file_count);

        if is_bdmv_file(full_path, &object.name) {
            return Ok(PathDecision::Bdmv);
        }

        let local_path = self.local_path(full_path, &object.name);
        processed_local_paths.insert(local_path.clone());

        // 一次 metadata 同时回答「本地是否存在」和「伴生文件是否过期」，
        // 避免每个文件多付一次 stat。
        let existing = if self.config.overwrite {
            None
        } else {
            match fs::metadata(&local_path).await {
                Ok(metadata) => Some(metadata),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
                Err(err) => return Err(err.into()),
            }
        };

        if let Some(metadata) = existing {
            if context.download_exts.contains(&suffix)
                && companion_file_is_stale(
                    &metadata,
                    object.size.max(0) as u64,
                    &remote_timestamps(object),
                )
            {
                debug!(path = %full_path, local_path = %local_path.display(), "伴生文件已过期或大小不一致，重新处理");
                return Ok(PathDecision::Process);
            }
            debug!(path = %full_path, local_path = %local_path.display(), "本地文件已存在，跳过处理");
            // 内容不动，但时间仍然对齐：老库第一次跑也要把时间补齐。
            self.follow_remote_time(&local_path, object).await;
            if local_path.extension().and_then(|ext| ext.to_str()) == Some("strm") {
                RunStats::inc(&context.stats.strm_skipped_count);
            } else {
                RunStats::inc(&context.stats.attachment_skipped_count);
            }
            return Ok(PathDecision::Skip);
        }

        Ok(PathDecision::Process)
    }

    /// 并发处理一批已经确定需要处理的路径，并等待整批完成才返回。
    ///
    /// 扫描阶段每个远端目录调用一次（批内并发、目录间串行）；扫描结束后的
    /// BDMV 主片批次也走这里。单个路径失败会记录并跳过，不中断同批次其它路径。
    async fn process_paths(
        &self,
        context: Arc<RunContext>,
        files: Vec<(String, ObjResp)>,
    ) -> Result<()> {
        // 用 bounded concurrency 限制并发处理数量，避免压垮 AList 或本地 IO。
        futures_util::stream::iter(files)
            .for_each_concurrent(self.settings.concurrency.max(1), |(full_path, object)| {
                let context = context.clone();
                self.process_path_logged(context, full_path, object)
            })
            .await;

        Ok(())
    }

    /// 包装单路径处理并记录错误。
    ///
    /// 该函数把 `process_path` 的错误降级为警告日志，使单个文件的 `.strm` 写入
    /// 或伴生文件下载失败不会影响后续路径继续处理。
    async fn process_path_logged(
        &self,
        context: Arc<RunContext>,
        full_path: String,
        object: ObjResp,
    ) {
        if let Err(err) = self.process_path(&context, &full_path, &object).await {
            RunStats::inc(&context.stats.failed_path_count);
            // 把失败文件所在的目录记下来，下次强制重扫这个目录。
            context.note_failed_dir(remote_parent(&full_path));
            warn!("处理失败，已跳过 {full_path}（{err}）");
        }
    }

    /// 处理单个远端文件。
    ///
    /// 视频文件写入本地 `.strm`，字幕、图片、nfo 等伴生文件则从 AList 下载
    /// 链接保存到本地。
    async fn process_path(
        &self,
        context: &RunContext,
        full_path: &str,
        object: &ObjResp,
    ) -> Result<()> {
        debug!(
            task_id = %self.config.id,
            path = %full_path,
            "正在处理 AList 路径"
        );

        let local_path = self.local_path(full_path, &object.name);
        // 双保险：路径规则已经过滤了 `..` 与绝对路径，这里再确认结果没有越界。
        if !local_path.starts_with(&self.config.target_dir) {
            return Err(Error::UnsafeLocalPath { path: local_path });
        }
        debug!(
            task_id = %self.config.id,
            remote_path = %full_path,
            local_path = %local_path.display(),
            "已计算本地目标路径"
        );
        if let Some(parent) = local_path.parent() {
            context.ensure_dir(parent).await?;
        }

        let url = download_url(
            &context.server_url,
            &context.base_path,
            full_path,
            &object.sign,
        );
        if local_path.extension().and_then(|ext| ext.to_str()) == Some("strm") {
            let existed_before = fs::try_exists(&local_path).await?;
            // 视频文件写入 .strm 内容；伴生文件则直接下载到本地。
            debug!(
                task_id = %self.config.id,
                remote_path = %full_path,
                local_path = %local_path.display(),
                "正在写入 strm 文件"
            );
            // 同样先写临时文件再改名，避免进程中断留下截断的 .strm。
            let temp_path = temp_file_path(&local_path);
            if let Err(err) = fs::write(&temp_path, url).await {
                let _ = fs::remove_file(&temp_path).await;
                return Err(err.into());
            }
            if let Err(err) = fs::rename(&temp_path, &local_path).await {
                let _ = fs::remove_file(&temp_path).await;
                return Err(err.into());
            }
            if existed_before {
                RunStats::inc(&context.stats.strm_updated_count);
                info!("更新: {}", local_path.display());
            } else {
                RunStats::inc(&context.stats.strm_created_count);
                info!("生成: {}", local_path.display());
            }
            self.follow_remote_time(&local_path, object).await;
        } else {
            let existed_before = fs::try_exists(&local_path).await?;
            debug!(
                task_id = %self.config.id,
                remote_path = %full_path,
                local_path = %local_path.display(),
                "正在下载 AList 伴生文件"
            );
            self.download_file(context, &url, &local_path).await?;
            if existed_before {
                RunStats::inc(&context.stats.attachment_updated_count);
                info!("更新: {}", local_path.display());
            } else {
                RunStats::inc(&context.stats.attachment_downloaded_count);
                info!("下载: {}", local_path.display());
            }
            self.follow_remote_time(&local_path, object).await;
        }

        Ok(())
    }

    /// 让本地文件的时间跟随远端：优先远端创建时间，缺失时退回修改时间。
    ///
    /// 已经对齐过的文件直接跳过，避免每轮同步都写一次时间戳；失败只告警——
    /// 文件内容已经写好，时间戳属于锦上添花，不该让整个路径被判失败。
    async fn follow_remote_time(&self, local_path: &Path, object: &ObjResp) {
        let Some(time) = remote_time(object) else {
            debug!(path = %local_path.display(), "远端时间无效，跳过设置文件时间");
            return;
        };
        if local_time_matches(local_path, time).await {
            return;
        }
        if let Err(err) = set_local_time(local_path, time).await {
            warn!("设置文件时间失败 {}（{err}）", local_path.display());
        }
    }

    /// 把本地目录的时间校对成远端时间。
    ///
    /// 必须在文件写入与清理都结束之后调用：新建、删除子项都会改写父目录的
    /// mtime，只有全部落盘后自深向浅设置，最终值才稳定。清理阶段删掉的空目录
    /// 直接跳过。
    async fn apply_dir_times(&self, mut dir_times: Vec<(PathBuf, DateTime<Utc>)>) {
        if dir_times.is_empty() {
            return;
        }
        dir_times.sort_by_key(|(path, _)| std::cmp::Reverse(path.components().count()));

        let total = dir_times.len();
        let mut applied = 0usize;
        let mut failed = 0usize;
        for (path, time) in dir_times {
            if !fs::try_exists(&path).await.unwrap_or(false) {
                continue;
            }
            // 已对齐就不写盘：目录可能很多，没必要每轮都刷新一遍 ctime。
            if local_time_matches(&path, time).await {
                continue;
            }
            match set_local_time(&path, time).await {
                Ok(()) => applied += 1,
                Err(err) => {
                    failed += 1;
                    debug!(path = %path.display(), "设置目录时间失败: {err}");
                }
            }
        }
        info!("目录时间已跟随远端：设置 {applied} 个，失败 {failed} 个，共 {total} 个");
    }

    /// 下载远端伴生文件到本地路径。
    ///
    /// 下载会使用独立信号量限流，避免字幕、图片、nfo 等伴生文件批量同步时
    /// 占满连接；只有 HTTP 200 会被视为成功，其它状态会作为下载错误返回。
    /// 内容先写入同目录的 `.part` 临时文件，完整下载成功后才改名，避免网络
    /// 中断留下半截文件被当成有效字幕。
    async fn download_file(
        &self,
        context: &RunContext,
        url: &str,
        local_path: &Path,
    ) -> Result<()> {
        // 下载伴生文件有独立限流，避免字幕/图片批量同步时占满连接。
        let _permit = context
            .download_semaphore
            .acquire()
            .await
            .expect("semaphore open");
        let response = context.http.get(url).send().await?;
        let status = response.status();
        if status != StatusCode::OK {
            return Err(Error::DownloadStatus {
                status,
                url: url.to_string(),
            });
        }
        if let Some(length) = response.content_length() {
            if length > MAX_ATTACHMENT_BYTES {
                return Err(Error::DownloadTooLarge {
                    size: length,
                    limit: MAX_ATTACHMENT_BYTES,
                    url: url.to_string(),
                });
            }
        }

        let temp_path = temp_file_path(local_path);
        if let Err(err) = stream_to_file(response, &temp_path, url).await {
            let _ = fs::remove_file(&temp_path).await;
            return Err(err);
        }
        if let Err(err) = fs::rename(&temp_path, local_path).await {
            let _ = fs::remove_file(&temp_path).await;
            return Err(err.into());
        }
        Ok(())
    }

    /// 清理本地已经不在远端扫描结果中的文件。
    ///
    /// 函数会收集目标目录里的本地文件，与本次扫描确认存在的路径做差集；
    /// `excluded_prefixes` 内的子树（远端扫描失败的目录）不参与删除。启用
    /// 智能保护时，大批量删除会先进入宽限确认。删除文件后会尝试清理空父目录，
    /// 单个文件失败不会中断整轮清理。
    async fn cleanup_local_files(
        &self,
        context: &RunContext,
        processed_local_paths: HashSet<PathBuf>,
        excluded_prefixes: &[PathBuf],
    ) -> Result<()> {
        // sync.enabled 开启时，本地多出来的文件会被清理；可配合保护防止大规模误删。
        info!("检查目录: {}", self.config.target_dir.display());
        let all_local_files =
            collect_local_files(&self.config.target_dir, self.config.flatten_mode).await?;
        let mut files_to_delete = all_local_files
            .difference(&processed_local_paths)
            .filter(|path| {
                !excluded_prefixes
                    .iter()
                    .any(|prefix| path.starts_with(prefix))
            })
            .cloned()
            .collect::<HashSet<_>>();

        // 忽略规则先生效，避免被忽略的文件占用保护计数。
        files_to_delete.retain(|path| {
            let ignored = context.sync_ignore_pattern.as_ref().is_some_and(|regex| {
                regex.is_match(
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or(""),
                )
            });
            if ignored {
                RunStats::inc(&context.stats.local_delete_ignored_count);
            }
            !ignored
        });

        if let Some(protection_config) = self.sync_smart_protection() {
            let mut protection = ProtectionManager::new(
                self.config.target_dir.clone(),
                &self.config.id,
                protection_config,
            )
            .await?;
            files_to_delete = protection
                .process(files_to_delete, &processed_local_paths)
                .await?;
        }

        let mut deleted_parents: HashSet<PathBuf> = HashSet::new();
        for file_path in files_to_delete {
            match fs::try_exists(&file_path).await {
                Ok(true) => {}
                Ok(false) => continue,
                Err(err) => {
                    warn!("检查文件失败，已跳过 {}（{err}）", file_path.display());
                    continue;
                }
            }

            match fs::remove_file(&file_path).await {
                Ok(()) => {
                    RunStats::inc(&context.stats.local_deleted_count);
                    info!("删除: {}", file_path.display());
                    if let Some(parent) = file_path.parent() {
                        deleted_parents.insert(parent.to_path_buf());
                    }
                }
                Err(err) => {
                    warn!("删除失败，已跳过 {}（{err}）", file_path.display());
                }
            }
        }

        // 全部删完再统一清理空目录：按深度自深向浅处理，同一目录只扫一次。
        let mut deleted_parents: Vec<PathBuf> = deleted_parents.into_iter().collect();
        deleted_parents.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
        let mut visited: HashSet<PathBuf> = HashSet::new();
        for parent in deleted_parents {
            if let Err(err) =
                remove_empty_parents(Some(&parent), &self.config.target_dir, &mut visited).await
            {
                warn!("清理空目录失败 {}（{err}）", parent.display());
            }
        }

        // 清理阶段可能删掉了整棵空目录，创建的缓存不再可信。
        // 宁可让后续写入多一次 create_dir_all，也不要写进已消失的目录。
        context
            .created_dirs
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clear();

        Ok(())
    }

    /// 从 BDMV 候选集合中选出每个原盘目录的主片。
    ///
    /// 同一个 BDMV 根目录下会选择体积最大的 `.m2ts` 作为主片，最终只生成一个
    /// 电影标题 `.strm`，避免媒体库识别到多个分段文件。
    fn finalize_bdmv_paths(
        &self,
        collections: HashMap<String, Vec<(String, ObjResp)>>,
    ) -> Vec<(String, ObjResp)> {
        collections
            .into_values()
            .filter_map(|files| {
                let selected = files.into_iter().max_by_key(|(_, object)| object.size);
                if let Some((full_path, object)) = selected.as_ref() {
                    info!(
                        "选择 BDMV 主片: {full_path}（{}）",
                        format_size(object.size)
                    );
                }
                selected
            })
            .collect()
    }

    /// 计算远端文件对应的本地输出路径。
    ///
    /// 该函数把任务级别的 `source_dir`、`target_dir`、`flatten_mode` 和 BDMV
    /// 特殊规则传给路径规则，得到最终要写入或下载的本地文件位置。
    fn local_path(&self, full_path: &str, name: &str) -> PathBuf {
        let bdmv_root = is_bdmv_file(full_path, name)
            .then(|| bdmv_root(full_path))
            .flatten();
        path::local_path(
            full_path,
            name,
            &self.config.source_dir,
            &self.config.target_dir,
            self.config.flatten_mode,
            bdmv_root.as_deref(),
        )
    }

    /// 根据下载配置计算需要作为伴生文件保存的扩展名集合。
    ///
    /// 未启用下载或平铺模式下不会下载伴生文件；否则会根据 subtitle/image/nfo
    /// 开关和 `other_ext` 合并出需要额外下载的扩展名。
    fn download_exts(&self) -> HashSet<String> {
        if !self.settings.download.enable || self.config.flatten_mode {
            return HashSet::new();
        }

        let mut exts = HashSet::new();
        if self.settings.download.subtitle {
            exts.extend(SUBTITLE_EXTS.iter().map(|ext| ext.to_string()));
        }
        if self.settings.download.image {
            exts.extend(IMAGE_EXTS.iter().map(|ext| ext.to_string()));
        }
        if self.settings.download.nfo {
            exts.extend(NFO_EXTS.iter().map(|ext| ext.to_string()));
        }
        exts.extend(
            self.settings
                .download
                .other_ext
                .iter()
                .map(|ext| ext.to_ascii_lowercase()),
        );
        exts
    }

    /// 判断是否启用了本地同步清理。
    ///
    /// 只有配置中存在 `sync` 且 `enabled=true` 时，任务结束后才会删除本地过期
    /// 文件。
    fn sync_enabled(&self) -> bool {
        self.settings.sync.as_ref().is_some_and(|sync| sync.enabled)
    }

    /// 返回启用状态下的智能删除保护配置。
    ///
    /// 该配置用于防止远端扫描失败导致本地媒体库被误清空；保护范围覆盖全部
    /// 同步产生的文件，而不只是 `.strm`。
    fn sync_smart_protection(&self) -> Option<&SmartProtectionConfig> {
        self.settings
            .sync
            .as_ref()
            .and_then(|sync| sync.smart_protection.as_ref())
            .filter(|config| config.enabled)
    }
}

/// 取远端路径的父目录：`/a/b.mkv` → `/a`，`/b.mkv` → `/`。
fn remote_parent(path: &str) -> &str {
    match path.rfind('/') {
        Some(0) | None => "/",
        Some(index) => &path[..index],
    }
}

/// 本地目录的修改时间是否不早于远端目录。
///
/// 本地目录不存在（首轮同步、远端新建目录）时返回 `false`，必须扫描；远端时间
/// 无效（零值或早于 1970）时同样返回 `false`，避免某些不维护目录时间的存储
/// 被永久跳过。
async fn local_dir_covers(local_dir: &Path, remote_modified: DateTime<Utc>) -> bool {
    if remote_modified.timestamp() <= 0 {
        return false;
    }
    let Ok(metadata) = fs::metadata(local_dir).await else {
        return false;
    };
    let Ok(modified) = metadata.modified() else {
        return false;
    };
    let Ok(elapsed) = modified.duration_since(std::time::UNIX_EPOCH) else {
        return false;
    };
    elapsed.as_secs() as i64 >= remote_modified.timestamp()
}

/// 取远端文件的时间：优先创建时间，缺失或为 Go 零值（`0001-01-01`）时退回修改时间。
///
/// AList 背后的存储不一定提供创建时间，部分驱动会把 `created` 填成零值，
/// 因此这里用「时间戳大于 0」作为有效性判断。
fn remote_time(object: &ObjResp) -> Option<DateTime<Utc>> {
    [object.created, object.modified]
        .into_iter()
        .find(|time| time.timestamp() > 0)
}

/// 取远端目录的时间：优先修改时间。
///
/// 目录必须用 `modified` 而不是 `created`：文件系统里目录 mtime 的语义就是
/// 「内容最后变化时间」，且「目录时间检查」正是拿本地 mtime 与远端 `modified`
/// 比较，两者必须同一个基准，否则每轮都会被判成有变化而白跑一遍。
fn remote_dir_time(object: &ObjResp) -> Option<DateTime<Utc>> {
    [object.modified, object.created]
        .into_iter()
        .find(|time| time.timestamp() > 0)
}

/// 远端时间戳集合（创建 / 修改），供伴生文件的增量判断使用。
fn remote_timestamps(object: &ObjResp) -> [i64; 2] {
    [object.created.timestamp(), object.modified.timestamp()]
}

/// 本地文件的修改时间是否已经等于目标时间。
///
/// 按秒比较即可：远端时间戳本身就是秒级，不同文件系统的精度差异（纳秒 / 100ns）
/// 不该让判断反复落空。
async fn local_time_matches(path: &Path, time: DateTime<Utc>) -> bool {
    let Ok(metadata) = fs::metadata(path).await else {
        return false;
    };
    metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
        .is_some_and(|elapsed| elapsed.as_secs() as i64 == time.timestamp())
}

/// 设置本地文件或目录的时间（atime + mtime）。
///
/// 用 `filetime` 而不是 `std::fs::File::set_times`：后者在 Windows 上打不开
/// 目录，而目录时间同样要跟随远端。文件系统层面的「创建时间」（btime）在
/// Linux 上不可写，Emby / Jellyfin 判断入库时间用的正是 mtime。
async fn set_local_time(path: &Path, time: DateTime<Utc>) -> Result<()> {
    let stamp = FileTime::from_unix_time(time.timestamp(), time.timestamp_subsec_nanos());
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || filetime::set_file_times(&path, stamp, stamp))
        .await
        .map_err(std::io::Error::other)??;
    Ok(())
}

/// 把耗时格式化成 `12.3s` / `1m23s`，避免日志里出现 `Duration { .. }` 的调试输出。
fn format_duration(duration: Duration) -> String {
    let secs = duration.as_secs_f64();
    if secs < 60.0 {
        format!("{secs:.1}s")
    } else {
        let total = duration.as_secs();
        format!("{}m{:02}s", total / 60, total % 60)
    }
}

/// 把 AList 返回的字节数转成可读体积（`1.2 GB`），用于挑选 BDMV 主片时的提示。
fn format_size(bytes: i64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes.max(0) as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// 生成写入过程中使用的临时文件路径（下载与 `.strm` 写入共用）。
fn temp_file_path(local_path: &Path) -> PathBuf {
    match (local_path.parent(), local_path.file_name()) {
        (Some(parent), Some(name)) => {
            let mut name = name.to_os_string();
            name.push(".part");
            parent.join(name)
        }
        _ => local_path.with_extension("part"),
    }
}

/// 把响应体流式写入临时文件，超过体积上限立即中止。
async fn stream_to_file(response: reqwest::Response, path: &Path, url: &str) -> Result<()> {
    let mut file = fs::File::create(path).await?;
    let mut written = 0u64;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        written += chunk.len() as u64;
        if written > MAX_ATTACHMENT_BYTES {
            return Err(Error::DownloadTooLarge {
                size: written,
                limit: MAX_ATTACHMENT_BYTES,
                url: url.to_string(),
            });
        }
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use alist::Authentication::Token;

    use super::*;
    fn test_config(target_dir: PathBuf) -> Config {
        Config {
            id: "test".to_string(),
            cron: None,
            alist: "test-alist".to_string(),
            source_dir: "/source".to_string(),
            target_dir,
            flatten_mode: true,
            overwrite: true,
            dir_time_check: false,
        }
    }

    fn test_settings() -> GlobalConfig {
        GlobalConfig {
            concurrency: 1,
            ..GlobalConfig::default()
        }
    }

    fn test_object(name: &str) -> ObjResp {
        serde_json::from_value(serde_json::json!({
            "name": name,
            "size": 1,
            "is_dir": false,
            "modified": "2024-01-01T00:00:00Z",
            "created": "2024-01-01T00:00:00Z",
            "sign": "",
            "thumb": "",
            "type": 0,
            "hashinfo": "",
        }))
        .expect("test object should deserialize")
    }

    fn test_context() -> RunContext {
        RunContext {
            client: Arc::new(
                Client::new("http://127.0.0.1:5244")
                    .unwrap()
                    .with_authentication(Token("token".to_string())),
            ),
            http: reqwest::Client::new(),
            server_url: "http://127.0.0.1:5244".to_string(),
            base_path: String::new(),
            download_semaphore: Arc::new(Semaphore::new(1)),
            download_exts: HashSet::new(),
            process_exts: HashSet::new(),
            sync_ignore_pattern: None,
            stats: Arc::new(RunStats::new()),
            retry_dirs: Arc::new(RetryDirs::empty()),
            failed_dirs: Mutex::new(HashSet::new()),
            created_dirs: Mutex::new(HashSet::new()),
        }
    }

    #[tokio::test]
    async fn process_paths_logs_item_errors_and_continues() {
        let unique = format!(
            "autofilm-process-error-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let target_dir = std::env::temp_dir().join(unique);
        fs::write(&target_dir, b"not a directory").await.unwrap();

        let runner = Alist2Strm::new(
            test_config(target_dir.clone()),
            test_settings(),
            Arc::new(
                Client::new("http://127.0.0.1:5244")
                    .unwrap()
                    .with_authentication(Token("token".to_string())),
            ),
            "http://127.0.0.1:5244".to_string(),
        );
        let context = Arc::new(test_context());
        let files = vec![("/source/movie.mkv".to_string(), test_object("movie.mkv"))];

        let result = runner.process_paths(context, files).await;
        let _ = fs::remove_file(&target_dir).await;

        assert!(result.is_ok());
    }

    /// 目录屏障依赖「批次返回时整批已落盘」：单测锁定这个契约。
    #[tokio::test]
    async fn process_paths_waits_for_whole_batch() {
        let unique = format!(
            "autofilm-batch-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let target_dir = std::env::temp_dir().join(unique);
        fs::create_dir_all(&target_dir).await.unwrap();

        let client = Arc::new(
            Client::new("http://127.0.0.1:5244")
                .unwrap()
                .with_authentication(Token("token".to_string())),
        );
        let runner = Alist2Strm::new(
            test_config(target_dir.clone()),
            GlobalConfig {
                concurrency: 3,
                ..GlobalConfig::default()
            },
            client,
            "http://127.0.0.1:5244".to_string(),
        );
        let context = Arc::new(test_context());
        let files: Vec<(String, ObjResp)> = (1..=6)
            .map(|index| {
                let name = format!("movie-{index}.mkv");
                (format!("/source/{name}"), test_object(&name))
            })
            .collect();

        runner.process_paths(context.clone(), files).await.unwrap();

        for index in 1..=6 {
            assert!(
                target_dir.join(format!("movie-{index}.strm")).exists(),
                "批次文件 {index} 未落盘"
            );
        }
        assert_eq!(context.stats.strm_created_count.load(Ordering::Relaxed), 6);

        let _ = fs::remove_dir_all(&target_dir).await;
    }

    #[test]
    fn temp_file_path_keeps_original_name() {
        let local = Path::new("/media/ani/movie.zh.ass");
        assert_eq!(
            temp_file_path(local),
            PathBuf::from("/media/ani/movie.zh.ass.part")
        );
    }

    #[test]
    fn remote_parent_returns_containing_directory() {
        assert_eq!(
            remote_parent("/媒体库/电影/甲/movie.mkv"),
            "/媒体库/电影/甲"
        );
        assert_eq!(remote_parent("/movie.mkv"), "/");
        assert_eq!(remote_parent("movie.mkv"), "/");
    }

    #[test]
    fn remote_dir_time_prefers_modified() {
        let mut object = test_object("电影");
        object.created = "2022-05-01T10:00:00Z".parse().unwrap();
        object.modified = "2023-06-02T11:00:00Z".parse().unwrap();
        // 目录必须按修改时间对齐，才能和「目录时间检查」用同一个基准。
        assert_eq!(remote_dir_time(&object), Some(object.modified));

        object.modified = "0001-01-01T00:00:00Z".parse().unwrap();
        assert_eq!(remote_dir_time(&object), Some(object.created));
    }

    #[tokio::test]
    async fn local_dir_covers_compares_timestamps() {
        let root = std::env::temp_dir().join(format!(
            "autofilm-dir-covers-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).await.unwrap();
        // 用文件代替目录做探针：时间比较逻辑与路径类型无关，且文件时间在所有
        // 平台都能设置（部分平台无法改目录时间）。
        let probe = root.join("probe");
        fs::write(&probe, b"x").await.unwrap();

        let stamp: DateTime<Utc> = "2022-01-02T03:04:05Z".parse().unwrap();
        set_local_time(&probe, stamp).await.unwrap();

        assert!(
            local_dir_covers(&probe, stamp).await,
            "两边时间一致说明子树没变化"
        );
        assert!(
            !local_dir_covers(&probe, stamp + chrono::Duration::seconds(1)).await,
            "远端更晚时必须扫描"
        );
        assert!(
            !local_dir_covers(&root.join("missing"), stamp).await,
            "本地目录不存在时必须扫描"
        );
        assert!(
            !local_dir_covers(&probe, "0001-01-01T00:00:00Z".parse().unwrap()).await,
            "远端时间为零值时不能跳过"
        );

        fs::remove_dir_all(&root).await.ok();
    }

    #[test]
    fn remote_time_prefers_created_and_falls_back_to_modified() {
        let mut object = test_object("movie.mkv");
        object.created = "2022-05-01T10:00:00Z".parse().unwrap();
        object.modified = "2023-06-02T11:00:00Z".parse().unwrap();
        assert_eq!(remote_time(&object), Some(object.created));

        // Go 零值（0001-01-01）表示存储没给创建时间，必须退回修改时间。
        object.created = "0001-01-01T00:00:00Z".parse().unwrap();
        assert_eq!(remote_time(&object), Some(object.modified));

        object.modified = "0001-01-01T00:00:00Z".parse().unwrap();
        assert_eq!(remote_time(&object), None, "两个时间都无效时不应设置时间");
    }

    #[tokio::test]
    async fn set_local_time_applies_to_files_and_directories() {
        let root = std::env::temp_dir().join(format!(
            "autofilm-file-time-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let dir = root.join("电影");
        fs::create_dir_all(&dir).await.unwrap();
        let file = dir.join("movie.strm");
        fs::write(&file, "http://example.com/movie.mkv")
            .await
            .unwrap();

        let stamp: DateTime<Utc> = "2022-01-02T03:04:05Z".parse().unwrap();
        set_local_time(&file, stamp).await.unwrap();

        let mtime = fs::metadata(&file).await.unwrap().modified().unwrap();
        let secs = mtime
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        assert_eq!(secs, stamp.timestamp(), "文件 mtime 应等于远端创建时间");

        // 目录时间在 Windows 上依赖打开目录的权限，失败时不影响主流程，这里跳过断言。
        match set_local_time(&dir, stamp).await {
            Ok(()) => {
                let mtime = fs::metadata(&dir).await.unwrap().modified().unwrap();
                let secs = mtime
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs() as i64;
                assert_eq!(secs, stamp.timestamp(), "目录 mtime 应等于远端创建时间");
            }
            Err(err) => eprintln!("当前平台无法设置目录时间，跳过断言: {err}"),
        }

        fs::remove_dir_all(&root).await.ok();
    }

    #[tokio::test]
    async fn local_time_matches_uses_second_precision() {
        let root = std::env::temp_dir().join(format!(
            "autofilm-time-match-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).await.unwrap();
        let file = root.join("movie.strm");
        fs::write(&file, "http://example.com/movie.mkv")
            .await
            .unwrap();

        let stamp: DateTime<Utc> = "2022-01-02T03:04:05Z".parse().unwrap();
        set_local_time(&file, stamp).await.unwrap();
        assert!(
            local_time_matches(&file, stamp).await,
            "同一秒应判定为已对齐"
        );
        assert!(
            !local_time_matches(&file, stamp + chrono::Duration::seconds(1)).await,
            "相差一秒应判定为未对齐"
        );

        fs::remove_dir_all(&root).await.ok();
    }

    #[tokio::test]
    async fn aligned_companion_file_is_not_stale() {
        let root = std::env::temp_dir().join(format!(
            "autofilm-stale-time-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).await.unwrap();
        let file = root.join("movie.zh.ass");
        fs::write(&file, b"subtitle").await.unwrap();
        let size = 8;

        let stamp: DateTime<Utc> = "2022-01-02T03:04:05Z".parse().unwrap();
        set_local_time(&file, stamp).await.unwrap();
        let created = stamp.timestamp();
        let modified = created + 3600;
        let metadata = fs::metadata(&file).await.unwrap();

        // 本地时间已被对齐到远端创建时间：不能再判成过期，否则每轮都会重下。
        assert!(
            !companion_file_is_stale(&metadata, size, &[created, modified]),
            "已对齐的文件不应判定为过期"
        );
        // 远端体积变大，依然要重下。
        assert!(
            companion_file_is_stale(&metadata, size + 1024, &[created, modified]),
            "远端体积变大应重新下载"
        );
        // 远端更新（时间前移），本地时间对不上：过期。
        assert!(
            companion_file_is_stale(&metadata, size, &[created + 7200, modified + 7200]),
            "远端时间前移应重新下载"
        );

        fs::remove_dir_all(&root).await.ok();
    }
}
