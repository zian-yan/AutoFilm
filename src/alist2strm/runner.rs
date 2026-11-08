use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

use alist::{
    Client,
    models::fs::{FsListReq, ObjResp},
};
use futures_util::{
    future::FutureExt,
    stream::{FuturesUnordered, StreamExt},
};
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
}

/// 扫描阶段对单个远端文件的处理决策。
///
/// BDMV 主片必须等同目录所有 m2ts 收集完成后才能按大小选片，因此单独分出
/// `Bdmv` 分支，避免提前进入并发处理队列。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathDecision {
    /// 需要立即进入处理队列。
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
    /// 流程包括创建 AList/HTTP 上下文、扫描远端目录、边扫描边处理普通文件、
    /// 收集并处理 BDMV 主片、按需清理本地过期文件。单个远端目录或文件失败会
    /// 记录日志并跳过，初始化和本地清理等关键错误仍会返回给调度器。
    pub async fn run(&self) -> Result<RunSummary> {
        // 每次 run 都重新创建轻量上下文，确保 base_path 和配置派生项是当前值。
        let stats = Arc::new(RunStats::new());
        let context = Arc::new(self.create_context(stats.clone()).await?);
        info!(task_id = %self.config.id, source_dir = %self.config.source_dir, "开始扫描 AList 目录");
        let mut processed_local_paths = HashSet::new();
        let mut bdmv_collections: HashMap<String, Vec<(String, ObjResp)>> = HashMap::new();
        // 记录扫描失败的远端目录，用于把对应本地子树排除出删除集合。
        let mut unscanned_dirs: Vec<String> = Vec::new();

        // 第一阶段边遍历远端文件边处理普通文件；BDMV 文件先收集，扫描结束后再选主片处理。
        self.collect_and_process_paths(
            context.clone(),
            &mut processed_local_paths,
            &mut bdmv_collections,
            &mut unscanned_dirs,
        )
        .await?;

        // 第二阶段为每个 BDMV 目录挑选最大的 m2ts，生成单个电影标题 .strm。
        let bdmv_paths = self.finalize_bdmv_paths(bdmv_collections);
        context
            .stats
            .bdmv_selected_count
            .store(bdmv_paths.len(), Ordering::Relaxed);
        if !bdmv_paths.is_empty() {
            info!(count = bdmv_paths.len(), "开始处理 BDMV 主片文件");
        }
        for (full_path, object) in &bdmv_paths {
            let local_path = self.local_path(full_path, &object.name);
            processed_local_paths.insert(local_path);
        }
        self.process_paths(context.clone(), bdmv_paths).await?;

        if self.sync_enabled() {
            self.cleanup_after_scan(&context, processed_local_paths, &unscanned_dirs)
                .await?;
        }

        let summary = stats.summarize(&self.config);
        Ok(summary)
    }

    /// 在扫描结果基础上执行本地清理。
    ///
    /// 扫描失败的目录不能当成「远端已删除」，否则一次网络抖动就会清空本地
    /// 媒体库。这里把失败目录映射成本地子树排除掉；平铺模式无法按目录区分，
    /// 只能整轮跳过清理。
    async fn cleanup_after_scan(
        &self,
        context: &RunContext,
        processed_local_paths: HashSet<PathBuf>,
        unscanned_dirs: &[String],
    ) -> Result<()> {
        let mut excluded_prefixes = Vec::new();

        if !unscanned_dirs.is_empty() {
            if self.config.flatten_mode {
                warn!(
                    task_id = %self.config.id,
                    unscanned_dir_count = unscanned_dirs.len(),
                    "存在未扫描成功的目录且启用了平铺模式，本轮跳过本地清理以避免误删"
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
                task_id = %self.config.id,
                unscanned_dir_count = unscanned_dirs.len(),
                excluded_prefix_count = excluded_prefixes.len(),
                "存在未扫描成功的目录，已排除对应本地子树后执行清理"
            );
        }

        self.cleanup_local_files(context, processed_local_paths, &excluded_prefixes)
            .await
    }

    /// 创建本次运行共享的上下文。
    ///
    /// 该函数会复用 AList 客户端、读取当前用户 `base_path`、准备 HTTP 下载
    /// 客户端、计算需要处理和下载的扩展名集合，并编译同步忽略规则。
    async fn create_context(&self, stats: Arc<RunStats>) -> Result<RunContext> {
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
        })
    }

    /// 扫描远端目录并立即处理普通文件。
    ///
    /// 函数使用显式栈遍历 `source_dir` 下的 AList 目录；扫描到普通文件后会
    /// 立刻提交到有上限的并发处理队列，避免等全量扫描完成才开始写入本地。
    /// BDMV 候选文件会先收集到分组中，等待扫描结束后统一选择主片。
    async fn collect_and_process_paths(
        &self,
        context: Arc<RunContext>,
        processed_local_paths: &mut HashSet<PathBuf>,
        bdmv_collections: &mut HashMap<String, Vec<(String, ObjResp)>>,
        unscanned_dirs: &mut Vec<String>,
    ) -> Result<()> {
        // 使用显式栈递归遍历，避免深层目录导致函数递归过深。
        let mut stack = vec![self.config.source_dir.clone()];
        let concurrency = self.settings.concurrency.max(1);
        let mut pending = FuturesUnordered::new();

        while let Some(dir_path) = stack.pop() {
            debug!(
                task_id = %self.config.id,
                dir_path = %dir_path,
                "正在扫描 AList 目录"
            );
            let resp = match context.client.fs_list(FsListReq::all(&dir_path)).await {
                Ok(resp) => resp,
                Err(err) => {
                    RunStats::inc(&context.stats.skipped_dir_count);
                    // 该目录下的文件状态未知，不能参与后续删除判定。
                    unscanned_dirs.push(dir_path.clone());
                    warn!(
                        task_id = %self.config.id,
                        dir_path = %dir_path,
                        error = %err,
                        "获取 AList 目录文件列表失败，已跳过该目录"
                    );
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
            for item in resp.content {
                let object = item.object;
                let full_path = join_alist_path(&dir_path, &object.name);

                if object.is_dir {
                    debug!(
                        task_id = %self.config.id,
                        dir_path = %full_path,
                        "发现 AList 子目录，加入扫描队列"
                    );
                    stack.push(full_path);
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
                            "AList 路径加入处理队列"
                        );
                        pending.push(
                            self.process_path_logged(context.clone(), full_path, object)
                                .boxed(),
                        );
                        while pending.len() >= concurrency {
                            pending.next().await;
                        }
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
                        warn!(
                            task_id = %self.config.id,
                            path = %full_path,
                            error = %err,
                            "判断 AList 路径是否需要处理失败，已跳过该路径"
                        );
                    }
                }
            }
        }

        while pending.next().await.is_some() {}

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

        if !self.config.overwrite && fs::try_exists(&local_path).await? {
            if context.download_exts.contains(&suffix)
                && companion_file_is_stale(
                    &local_path,
                    object.size.max(0) as u64,
                    object.modified.timestamp(),
                )
                .await?
            {
                debug!(path = %full_path, local_path = %local_path.display(), "伴生文件已过期或大小不一致，重新处理");
                return Ok(PathDecision::Process);
            }
            debug!(path = %full_path, local_path = %local_path.display(), "本地文件已存在，跳过处理");
            if local_path.extension().and_then(|ext| ext.to_str()) == Some("strm") {
                RunStats::inc(&context.stats.strm_skipped_count);
            } else {
                RunStats::inc(&context.stats.attachment_skipped_count);
            }
            return Ok(PathDecision::Skip);
        }

        Ok(PathDecision::Process)
    }

    /// 并发处理一组已经确定需要处理的路径。
    ///
    /// 该函数主要用于扫描结束后的 BDMV 主片批处理；普通文件在扫描阶段已经
    /// 通过 `collect_and_process_paths` 边扫描边提交。每个路径的失败会被记录并
    /// 跳过，不会中断同批次其它路径。
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
            warn!(
                task_id = %self.config.id,
                path = %full_path,
                error = %err,
                "处理 AList 路径失败，已跳过该路径"
            );
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
            fs::create_dir_all(parent).await?;
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
            } else {
                RunStats::inc(&context.stats.strm_created_count);
            }
            info!(
                task_id = %self.config.id,
                remote_path = %full_path,
                local_path = %local_path.display(),
                "strm 文件创建成功"
            );
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
            } else {
                RunStats::inc(&context.stats.attachment_downloaded_count);
            }
            info!(
                task_id = %self.config.id,
                remote_path = %full_path,
                local_path = %local_path.display(),
                "伴生文件下载成功"
            );
        }

        Ok(())
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

        for file_path in files_to_delete {
            match fs::try_exists(&file_path).await {
                Ok(true) => {}
                Ok(false) => continue,
                Err(err) => {
                    warn!(path = %file_path.display(), error = %err, "检查本地文件失败，已跳过");
                    continue;
                }
            }

            match fs::remove_file(&file_path).await {
                Ok(()) => {
                    RunStats::inc(&context.stats.local_deleted_count);
                    info!(path = %file_path.display(), "删除本地过期文件");
                    if let Err(err) =
                        remove_empty_parents(file_path.parent(), &self.config.target_dir).await
                    {
                        warn!(path = %file_path.display(), error = %err, "清理空目录失败");
                    }
                }
                Err(err) => {
                    warn!(path = %file_path.display(), error = %err, "删除本地过期文件失败，已跳过");
                }
            }
        }

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
                    info!(path = %full_path, size = object.size, "选择 BDMV 最大 m2ts 作为主片");
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

    #[test]
    fn temp_file_path_keeps_original_name() {
        let local = Path::new("/media/ani/movie.zh.ass");
        assert_eq!(
            temp_file_path(local),
            PathBuf::from("/media/ani/movie.zh.ass.part")
        );
    }
}
