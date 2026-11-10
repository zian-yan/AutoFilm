use autofilm::{admin, app_info, args::CliArgs, config, logging, web_config};
use clap::Parser;
use tracing::{debug, info, warn};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let args = CliArgs::parse();
    app_info::print_banner();

    if args.show_version {
        let version_info = serde_json::to_string_pretty(&*app_info::APPLICATION_INFO)?;
        println!("{version_info}");
        return Ok(());
    }

    let tz = args.app_timezone();

    let _logging_guard = logging::init(args.log_level(), &args.log_path, tz, args.colorful_log)?;
    // 任务日志独立于按天文件（即使 --log 留空也要能用），放在 logs/task-logs/。
    let task_log_root = if args.log_path.is_empty() {
        std::path::PathBuf::from("logs")
    } else {
        std::path::PathBuf::from(&args.log_path)
    };
    match std::path::absolute(task_log_root.join("task-logs")) {
        Ok(root) => {
            if let Err(err) = autofilm::task_log::init(root, tz) {
                warn!(error = %err, "任务日志目录初始化失败，日志按钮可能不可用");
            }
        }
        Err(err) => warn!(error = %err, "无法解析任务日志目录，日志按钮可能不可用"),
    }
    debug!(
        debug = args.debug,
        config_path = %args.config_path.display(),
        web_config_path = %args.web_config_path.display(),
        log_path = %args.log_path,
        timezone = ?args.timezone,
        "启动参数解析完成"
    );

    // 两份配置完全独立加载：任何一份缺失或为空都不算错误，按各自的内置默认值
    // 继续启动，便于在初始化阶段先建目录、之后逐步补齐。
    let config = config::Config::load(&args.config_path)
        .map_err(|err| -> Box<dyn std::error::Error + Send + Sync> { Box::new(err) })?;
    if config.is_empty() {
        info!(
            path = %args.config_path.display(),
            "业务配置为空，将以空配置启动；可通过 Web 面板或编辑该文件逐步添加"
        );
    }

    let web = web_config::WebConfig::load(&args.web_config_path)
        .map_err(|err| -> Box<dyn std::error::Error + Send + Sync> { Box::new(err) })?;

    info!(timezone = %tz, "使用应用时区");

    // 启动时不因校验问题中断（CLI 用户可能依赖部分可用配置），只提示具体位置。
    for issue in config.validate() {
        warn!(field = %issue.field, message = %issue.message, "业务配置校验问题");
    }
    for issue in web.validate() {
        warn!(field = %issue.field, message = %issue.message, "面板配置校验问题");
    }

    // 由热重载内核接管调度器：它同时负责首次注册、后续配置变更与退出清理。
    let (mut runtime, scheduled_count) = admin::Runtime::start(
        args.config_path.clone(),
        config,
        args.web_config_path.clone(),
        web,
        tz,
    )
    .await
    .map_err(|err| -> Box<dyn std::error::Error + Send + Sync> { err.into() })?;

    if scheduled_count == 0 {
        warn!("没有可调度的任务");
        // 面板或 MediaWarp 托管都算「有事可做」：后者是常驻子进程，主进程
        // 一旦退出它也会被一并带走，因此不能按空配置提前结束。
        if !runtime.web_enabled() && !runtime.mediawarp_enabled() {
            runtime.shutdown().await;
            warn!(
                config_path = %args.config_path.display(),
                web_config_path = %args.web_config_path.display(),
                "没有可调度任务，且未启用 Web 管理面板或 MediaWarp 托管，进程退出；\
                 请在业务配置中添加任务、在面板配置中设置 enable=true，或开启 MediaWarp 托管"
            );
            return Ok(());
        }
    }

    info!(
        scheduled_count,
        web_panel = runtime.web_enabled(),
        "AutoFilm 调度器启动完成"
    );

    // 阻塞主任务，直到收到退出信号；调度器与配置热重载都在后台运行。
    shutdown_signal().await;
    info!("AutoFilm 收到退出信号，正在退出中...");
    runtime.shutdown().await;
    info!("AutoFilm 已成功退出");
    Ok(())
}

/// 等待退出信号。
///
/// 除了 Ctrl-C（SIGINT），Unix 下还要处理 SIGTERM：`docker stop` 发送的是
/// SIGTERM，如果不捕获就会直接杀掉进程，导致日志缓冲丢失、正在写入的
/// `.strm` 被截断。
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        match signal(SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = terminate.recv() => {}
                }
            }
            Err(err) => {
                warn!(error = %err, "注册 SIGTERM 处理器失败，仅监听 Ctrl-C");
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }

    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
