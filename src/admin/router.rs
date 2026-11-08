use std::sync::mpsc as std_mpsc;

use serde_json::{Value, json};

use crate::{
    admin::{
        APPLY_TIMEOUT, AdminCommand, AdminContext, ApplyOutcome, ReloadSource,
        assets,
        http::{Request, Response},
    },
    config::Config,
    web_config::WebConfig,
};

/// 内容安全策略：只允许加载同源资源，禁止内联脚本与内联样式。
const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; script-src 'self'; style-src 'self'; \
     img-src 'self' data:; connect-src 'self'; base-uri 'none'; form-action 'none'; \
     frame-ancestors 'none'";

/// 路由分发入口：先做访问控制，再匹配路由。
pub fn dispatch(ctx: &AdminContext, request: &Request) -> Response {
    if ctx.auth.enabled() {
        if let Some(remaining) = ctx.guard.locked_remaining(&request.remote_addr) {
            return Response::text(
                429,
                format!("认证失败次数过多，请在 {remaining} 秒后重试"),
            );
        }

        if !ctx.auth.verify(request.header("authorization")) {
            ctx.guard.register_failure(&request.remote_addr);
            return Response::text(401, "需要身份验证").with_header(
                "WWW-Authenticate",
                "Basic realm=\"AutoFilm\", charset=\"UTF-8\"",
            );
        }

        ctx.guard.reset(&request.remote_addr);
    }

    // 变更类请求必须携带自定义请求头：浏览器跨站表单无法伪造该头，
    // 配合 Basic 认证即可阻断 CSRF 写入。
    if matches!(request.method.as_str(), "PUT" | "POST")
        && request.header("x-autofilm-request") != Some("1")
    {
        return Response::text(403, "缺少 X-AutoFilm-Request 请求头，请通过管理面板操作");
    }

    let page = |body: &'static str| {
        Response::html(body).with_header("Content-Security-Policy", CONTENT_SECURITY_POLICY)
    };

    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/") | ("GET", "/index.html") => page(assets::INDEX_HTML),
        ("GET", "/assets/app.css") => Response::new(200, "text/css; charset=utf-8", assets::APP_CSS),
        ("GET", "/assets/app.js") => Response::new(
            200,
            "application/javascript; charset=utf-8",
            assets::APP_JS,
        ),
        ("GET", "/favicon.ico") => Response::empty(204),
        ("GET", "/api/state") => state_handler(ctx),
        ("GET", "/api/config.yaml") => config_yaml_handler(ctx),
        ("GET", "/api/web.yaml") => web_yaml_handler(ctx),
        ("PUT", "/api/config") => save_config_handler(ctx, request),
        ("PUT", "/api/web") => save_web_handler(ctx, request),
        ("POST", "/api/reload") => reload_handler(ctx),
        (_, path) if path.starts_with("/api/") => {
            Response::json(404, &json!({ "message": "未知接口" }))
        }
        _ => Response::text(404, "未找到该资源"),
    }
}

/// 返回面板初始化所需的全部数据：应用信息、两份配置、版本号、运行时状态与表单结构。
///
/// 业务配置与面板配置分别放在 `config` / `web` 两个字段里，各自带自己的版本号，
/// 前端因此可以独立保存、独立处理 409 冲突。
fn state_handler(ctx: &AdminContext) -> Response {
    let business = ctx.store.snapshot();
    let web = ctx.web_store.snapshot();

    let config_value = match serde_json::to_value(&business.config) {
        Ok(value) => value,
        Err(err) => return Response::text(500, format!("序列化业务配置失败: {err}")),
    };
    let web_value = match serde_json::to_value(web.config.masked()) {
        Ok(value) => value,
        Err(err) => return Response::text(500, format!("序列化面板配置失败: {err}")),
    };

    Response::json(
        200,
        &json!({
            "app": {
                "name": env!("CARGO_PKG_NAME"),
                "version": env!("CARGO_PKG_VERSION"),
                "config_path": ctx.config_path.display().to_string(),
                "web_config_path": ctx.web_config_path.display().to_string(),
                "timezone": ctx.timezone.clone(),
                "watch_file": ctx.watch_file,
                "listen": ctx.listen.clone(),
                "auth_enabled": ctx.auth.enabled(),
                "password_set": !web.config.password.is_empty(),
                "started_at": ctx.started_at.clone(),
            },
            "version": business.version,
            "config": config_value,
            "web_version": web.version,
            "web": web_value,
            "runtime": ctx.status_json(),
            "schema": ctx.schema.clone(),
        }),
    )
}

/// 返回当前业务配置的 YAML 文本，便于复制备份。
fn config_yaml_handler(ctx: &AdminContext) -> Response {
    match serde_yaml::to_string(&ctx.store.snapshot().config) {
        Ok(yaml) => Response::new(200, "text/yaml; charset=utf-8", yaml),
        Err(err) => Response::text(500, format!("序列化业务配置失败: {err}")),
    }
}

/// 返回当前面板配置的 YAML 文本（密码已脱敏）。
fn web_yaml_handler(ctx: &AdminContext) -> Response {
    match serde_yaml::to_string(&ctx.web_store.snapshot().config.masked()) {
        Ok(yaml) => Response::new(200, "text/yaml; charset=utf-8", yaml),
        Err(err) => Response::text(500, format!("序列化面板配置失败: {err}")),
    }
}

/// 应用面板提交的业务配置；只写 `config.yaml`。
fn save_config_handler(ctx: &AdminContext, request: &Request) -> Response {
    let body = match request.json_body() {
        Ok(body) => body,
        Err(message) => return Response::text(400, message),
    };

    let current_version = ctx.store.version();
    if let Some(response) = conflict("业务配置", &body, current_version) {
        return response;
    }

    let Some(config_value) = body.get("config").cloned() else {
        return Response::text(400, "请求体缺少 config 字段");
    };

    let config: Config = match serde_json::from_value(config_value) {
        Ok(config) => config,
        Err(err) => {
            return Response::json(
                400,
                &json!({
                    "message": format!("配置结构不正确: {err}"),
                    "errors": [ { "field": "", "message": err.to_string() } ],
                }),
            );
        }
    };

    submit(ctx, config, ReloadSource::Panel)
}

/// 应用面板提交的面板配置；只写 `web.yaml`。
fn save_web_handler(ctx: &AdminContext, request: &Request) -> Response {
    let body = match request.json_body() {
        Ok(body) => body,
        Err(message) => return Response::text(400, message),
    };

    let current_version = ctx.web_store.version();
    if let Some(response) = conflict("面板配置", &body, current_version) {
        return response;
    }

    let Some(web_value) = body.get("web").cloned() else {
        return Response::text(400, "请求体缺少 web 字段");
    };

    let mut web: WebConfig = match serde_json::from_value(web_value) {
        Ok(web) => web,
        Err(err) => {
            return Response::json(
                400,
                &json!({
                    "message": format!("面板配置结构不正确: {err}"),
                    "errors": [ { "field": "", "message": err.to_string() } ],
                }),
            );
        }
    };

    // 前端不会拿到明文密码，因此留空提交视为沿用当前密码。
    if web.password.is_empty() {
        web.password = ctx.web_store.snapshot().config.password;
    }

    submit_web(ctx, web, ReloadSource::Panel)
}

/// 乐观并发控制：提交的 `base_version` 与当前版本不一致时返回 409。
fn conflict(label: &str, body: &Value, current_version: u64) -> Option<Response> {
    let base_version = body
        .get("base_version")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    if base_version == current_version {
        return None;
    }

    Some(Response::json(
        409,
        &json!({
            "message": format!(
                "{label}已被其他会话修改（当前版本 {current_version}），请重新加载后再保存"
            ),
            "version": current_version,
        }),
    ))
}

/// 重新读取磁盘上的两份配置文件并应用。
///
/// 先校验两份配置再依次应用，避免出现「业务配置已生效、面板配置却报错」的半成品
/// 状态；任一份有问题都不会写盘。
fn reload_handler(ctx: &AdminContext) -> Response {
    let config = match Config::load(&ctx.config_path) {
        Ok(config) => config,
        Err(err) => {
            return Response::json(
                400,
                &json!({
                    "message": format!("读取业务配置文件失败: {err}"),
                    "errors": [],
                }),
            );
        }
    };
    let web = match WebConfig::load(&ctx.web_config_path) {
        Ok(web) => web,
        Err(err) => {
            return Response::json(
                400,
                &json!({
                    "message": format!("读取面板配置文件失败: {err}"),
                    "errors": [],
                }),
            );
        }
    };

    let mut issues = config.validate();
    issues.extend(web.validate());
    if !issues.is_empty() {
        let message = format!("配置文件校验未通过（{} 处问题）", issues.len());
        return Response::json(422, &json!({ "message": message, "errors": issues }));
    }

    let business = submit(ctx, config, ReloadSource::Manual);
    if business.status != 200 {
        return business;
    }
    let web = submit_web(ctx, web, ReloadSource::Manual);
    if web.status != 200 {
        return web;
    }

    Response::json(
        200,
        &json!({ "message": "已重新读取磁盘上的 config.yaml 与 web.yaml", "errors": [] }),
    )
}

/// 把业务配置交给热重载内核并等待结果。
///
/// 等待带超时：热重载一旦卡住，连接线程不会永久占用连接名额，而是返回 503。
fn submit(ctx: &AdminContext, config: Config, source: ReloadSource) -> Response {
    let (reply, receiver) = std_mpsc::sync_channel(1);
    if ctx
        .commands
        .blocking_send(AdminCommand::Apply {
            config,
            source,
            reply,
        })
        .is_err()
    {
        return Response::text(503, "配置内核已停止，无法应用配置");
    }

    finish(receiver, "业务配置")
}

/// 把面板配置交给热重载内核并等待结果。
fn submit_web(ctx: &AdminContext, web: WebConfig, source: ReloadSource) -> Response {
    let (reply, receiver) = std_mpsc::sync_channel(1);
    if ctx
        .commands
        .blocking_send(AdminCommand::ApplyWeb { web, source, reply })
        .is_err()
    {
        return Response::text(503, "配置内核已停止，无法应用配置");
    }

    finish(receiver, "面板配置")
}

/// 等待内核回传结果并转成 HTTP 响应。
fn finish(receiver: std_mpsc::Receiver<ApplyOutcome>, label: &str) -> Response {
    let outcome = match receiver.recv_timeout(APPLY_TIMEOUT) {
        Ok(outcome) => outcome,
        Err(std_mpsc::RecvTimeoutError::Timeout) => {
            return Response::text(
                503,
                format!("等待{label}生效超时，请稍后刷新面板确认结果"),
            );
        }
        Err(std_mpsc::RecvTimeoutError::Disconnected) => {
            return Response::text(500, format!("等待{label}应用结果失败"));
        }
    };

    match outcome {
        ApplyOutcome::Applied(report) => {
            let message = if report.scope == "web" {
                "面板配置已保存，重启进程后生效".to_string()
            } else {
                format!(
                    "配置已保存并生效，共 {} 个调度任务",
                    report.scheduled_count
                )
            };
            Response::json(
                200,
                &json!({
                    "message": message,
                    "version": report.version,
                    "scope": report.scope,
                    "report": report,
                }),
            )
        }
        ApplyOutcome::Rejected(errors) => {
            let message = format!("{label}校验未通过（{} 处问题）", errors.len());
            Response::json(422, &json!({ "message": message, "errors": errors }))
        }
        ApplyOutcome::Failed(message) => {
            Response::json(500, &json!({ "message": message, "errors": [] }))
        }
    }
}
