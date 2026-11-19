use std::{net::IpAddr, sync::mpsc as std_mpsc, time::Duration};

use alist::models::fs::FsListReq;
use serde_json::{Value, json};
use tracing::info;

use crate::{
    admin::{
        APPLY_TIMEOUT, AdminCommand, AdminContext, ApplyOutcome, ReloadSource, assets, auth,
        http::{Request, Response},
    },
    config::Config,
    mediawarp,
    schedule::RegisteredTask,
    web_config::WebConfig,
};

/// 内容安全策略：只允许加载同源资源，禁止内联脚本与内联样式。
const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; script-src 'self'; style-src 'self'; \
     img-src 'self' data:; connect-src 'self'; base-uri 'none'; form-action 'none'; \
     frame-ancestors 'none'";

/// 浏览远端目录的超时时间：AList 或其上游网盘卡住时不能拖死面板连接。
const BROWSE_TIMEOUT: Duration = Duration::from_secs(10);

/// 路由分发入口：先做访问控制，再匹配路由。
pub fn dispatch(ctx: &AdminContext, request: &Request) -> Response {
    let page = |body: &'static str| {
        Response::html(body).with_header("Content-Security-Policy", CONTENT_SECURITY_POLICY)
    };
    // 每请求只读一次面板配置：后续认证判断直接复用，避免反复加锁与克隆。
    let auth = ctx.auth();

    // 登录页与它的静态资源必须始终可达，否则未登录用户会被重定向到自己。
    if request.method == "GET" && is_login_asset(&request.path) {
        if !auth.enabled() || is_authenticated(ctx, request, &auth) {
            return Response::redirect("/");
        }
        return match request.path.as_str() {
            "/login" => page(assets::LOGIN_HTML),
            "/assets/login.css" => Response::new(200, "text/css; charset=utf-8", assets::LOGIN_CSS),
            _ => Response::new(
                200,
                "application/javascript; charset=utf-8",
                assets::LOGIN_JS,
            ),
        };
    }

    // 登录接口本身不能要求已登录，但同样带 CSRF 头校验与失败限流。
    if request.method == "POST" && request.path == "/api/login" {
        return login_handler(ctx, request);
    }

    // 站点图标对登录页与主面板都公开，需放在认证判断之前。
    if request.method == "GET" && request.path == "/favicon.ico" {
        return Response::new(200, "image/x-icon", assets::FAVICON_ICO);
    }

    // 主题预判定脚本要进 `<head>` 且先于样式表执行，登录页与主面板都得能取到；
    // 它不含任何配置数据，因此放在认证判断之前无条件放行（否则未启用认证时会被
    // 登录页分支重定向回首页，主面板拿不到脚本，首帧闪烁依旧存在）。
    if request.method == "GET" && request.path == "/assets/theme.js" {
        return Response::new(
            200,
            "application/javascript; charset=utf-8",
            assets::THEME_JS,
        );
    }

    if auth.enabled() {
        let client = client_key(request);
        if let Some(remaining) = ctx.guard.locked_remaining(&client) {
            let message = format!("认证失败次数过多，请在 {remaining} 秒后重试");
            return if request.path.starts_with("/api/") {
                Response::json(429, &json!({ "message": message }))
            } else {
                Response::text(429, message)
            };
        }

        if !is_authenticated(ctx, request, &auth) {
            // 带了 Authorization 才算一次主动尝试，否则每次刷新页面都会累积失败次数。
            if request.header("authorization").is_some() {
                ctx.guard.register_failure(&client);
            }
            return if request.path.starts_with("/api/") {
                // 不带 WWW-Authenticate，避免浏览器弹出原生登录框盖住页面登录。
                Response::json(401, &json!({ "message": "登录状态已失效，请重新登录" }))
            } else {
                Response::redirect("/login")
            };
        }

        ctx.guard.reset(&client);
    }

    // 变更类请求必须携带自定义请求头：浏览器跨站表单无法伪造该头，
    // 配合会话 Cookie（SameSite=Strict）即可阻断 CSRF 写入。
    if matches!(request.method.as_str(), "PUT" | "POST")
        && request.header("x-autofilm-request") != Some("1")
    {
        return Response::text(403, "缺少 X-AutoFilm-Request 请求头，请通过管理面板操作");
    }

    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/") | ("GET", "/index.html") => page(assets::INDEX_HTML),
        ("GET", "/assets/app.css") => {
            Response::new(200, "text/css; charset=utf-8", assets::APP_CSS)
        }
        ("GET", "/assets/app.js") => {
            Response::new(200, "application/javascript; charset=utf-8", assets::APP_JS)
        }
        ("GET", "/api/state") => state_handler(ctx),
        ("GET", "/api/runtime") => runtime_handler(ctx),
        ("GET", "/api/config.yaml") => config_yaml_handler(ctx),
        ("GET", "/api/web.yaml") => web_yaml_handler(ctx),
        ("PUT", "/api/config") => save_config_handler(ctx, request),
        ("PUT", "/api/web") => save_web_handler(ctx, request),
        ("POST", "/api/run") => run_handler(ctx, request),
        ("POST", "/api/run-all") => run_all_handler(ctx, request),
        ("POST", "/api/task-log") => task_log_handler(request),
        ("POST", "/api/browse") => browse_handler(ctx, request),
        ("POST", "/api/logout") => logout_handler(ctx, request),
        ("POST", "/api/reload") => reload_handler(ctx),
        ("GET", "/api/mediawarp/preview") => mediawarp_preview_handler(ctx),
        ("POST", "/api/mediawarp/log") => mediawarp_log_handler(ctx, request),
        ("POST", "/api/mediawarp/restart") => mediawarp_restart_handler(ctx),
        ("POST", "/api/mediawarp/import") => mediawarp_import_handler(ctx),
        (_, path) if path.starts_with("/api/") => {
            Response::json(404, &json!({ "message": "未知接口" }))
        }
        _ => Response::text(404, "未找到该资源"),
    }
}

/// 登录页相关的公开路径。
fn is_login_asset(path: &str) -> bool {
    matches!(path, "/login" | "/assets/login.css" | "/assets/login.js")
}

/// 判断请求是否已通过认证：先看会话 Cookie，再兼容 HTTP Basic。
fn is_authenticated(ctx: &AdminContext, request: &Request, auth: &auth::AuthConfig) -> bool {
    if let Some(token) = auth::session_from_cookie(request.header("cookie"))
        && ctx.sessions.verify(token)
    {
        return true;
    }

    // 保留 Basic：curl 或自动化脚本仍可直接带凭据访问，不必先走登录页。
    auth.verify(request.header("authorization"))
}

/// 判断当前请求是否经由 HTTPS。
///
/// 面板本身不终结 TLS，实际部署多是 Nginx / Caddy 反代并透传
/// `X-Forwarded-Proto`。据此给会话 Cookie 打 `Secure`，避免明文回传。
fn is_https(request: &Request) -> bool {
    request
        .header("x-forwarded-proto")
        .and_then(|value| value.split(',').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("https"))
}

/// 限流与锁定使用的来源标识。
///
/// 面板常被放在反代后面，此时所有请求的对端地址都是反代本身：按 `remote_addr`
/// 计数会让任意一个攻击者把所有人一起锁死，限流也失去意义。只有在「对端是本机
/// 或内网」时才采信 `X-Forwarded-For` 的第一跳——直连公网的客户端可以随意伪造
/// 该请求头，采信它等于关闭限流。
fn client_key(request: &Request) -> String {
    let peer = request.remote_addr.as_str();
    if !peer.parse::<IpAddr>().is_ok_and(is_internal_ip) {
        return peer.to_string();
    }

    request
        .header("x-forwarded-for")
        .and_then(|value| value.split(',').next())
        .map(str::trim)
        .filter(|value| value.len() <= 64 && value.parse::<IpAddr>().is_ok())
        .unwrap_or(peer)
        .to_string()
}

/// 是否属于内网 / 回环地址（可能是本机反代）。
fn is_internal_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
        // 双栈监听时对端常是 `::ffff:192.168.x.x`，需要先还原成 IPv4 再判断。
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || v6
                    .to_ipv4_mapped()
                    .is_some_and(|v4| is_internal_ip(IpAddr::V4(v4)))
        }
    }
}

/// 构造会话 Cookie：HttpOnly 防止脚本读取，SameSite=Strict 阻断跨站携带，
/// HTTPS 场景再补 Secure 防止降级泄漏。
fn session_cookie(token: &str, max_age: u64, secure: bool) -> String {
    let secure = if secure { "; Secure" } else { "" };
    format!(
        "{}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={max_age}{secure}",
        auth::SESSION_COOKIE
    )
}

/// 清除会话 Cookie。
fn cleared_cookie(secure: bool) -> String {
    let secure = if secure { "; Secure" } else { "" };
    format!(
        "{}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0{secure}",
        auth::SESSION_COOKIE
    )
}

/// 登录：校验凭据后签发会话 Cookie。
fn login_handler(ctx: &AdminContext, request: &Request) -> Response {
    let auth = ctx.auth();
    if !auth.enabled() {
        return Response::json(400, &json!({ "message": "面板未设置访问密码，无需登录" }));
    }

    if request.header("x-autofilm-request") != Some("1") {
        return Response::text(403, "缺少 X-AutoFilm-Request 请求头，请通过登录页操作");
    }

    if let Some(remaining) = ctx.guard.locked_remaining(&client_key(request)) {
        return Response::json(
            429,
            &json!({ "message": format!("失败次数过多，请在 {remaining} 秒后重试") }),
        );
    }

    let body = match request.json_body() {
        Ok(body) => body,
        Err(message) => return Response::json(400, &json!({ "message": message })),
    };
    let username = body
        .get("username")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    let password = body
        .get("password")
        .and_then(Value::as_str)
        .unwrap_or_default();

    let client = client_key(request);
    if !auth.verify_credentials(username, password) {
        ctx.guard.register_failure(&client);
        info!(peer = %request.remote_addr, client = %client, username = %username, "面板登录失败");
        return Response::json(401, &json!({ "message": "用户名或密码错误" }));
    }

    ctx.guard.reset(&client);
    let Some(token) = ctx.sessions.issue() else {
        return Response::json(500, &json!({ "message": "签发会话失败，请重试" }));
    };
    info!(peer = %request.remote_addr, client = %client, username = %username, "面板登录成功");

    Response::json(200, &json!({ "ok": true })).with_header(
        "Set-Cookie",
        session_cookie(&token, auth::SESSION_TTL.as_secs(), is_https(request)),
    )
}

/// 退出登录：注销当前会话并清除 Cookie。
fn logout_handler(ctx: &AdminContext, request: &Request) -> Response {
    if let Some(token) = auth::session_from_cookie(request.header("cookie")) {
        ctx.sessions.revoke(token);
    }
    info!(peer = %request.remote_addr, "面板退出登录");
    Response::json(200, &json!({ "ok": true }))
        .with_header("Set-Cookie", cleared_cookie(is_https(request)))
}

/// 返回面板初始化所需的全部数据：应用信息、两份配置、版本号、运行时状态与表单结构。
///
/// 业务配置与面板配置分别放在 `config` / `web` 两个字段里，各自带自己的版本号，
/// 前端因此可以独立保存、独立处理 409 冲突。
fn state_handler(ctx: &AdminContext) -> Response {
    let business = ctx.store.snapshot();
    let web = ctx.web_store.snapshot();

    let config_value = match serde_json::to_value(business.config.masked()) {
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
                "watch_file": web.config.watch_file,
                "listen": web.config.listen.clone(),
                "auth_enabled": !web.config.password.is_empty(),
                "password_set": !web.config.password.is_empty(),
                "started_at": ctx.started_at.clone(),
            },
            "version": business.version,
            "config": config_value,
            "web_version": web.version,
            "web": web_value,
            "runtime": ctx.runtime_json(),
            "schema": ctx.schema.clone(),
        }),
    )
}

/// 轻量轮询接口：只返回运行时数据，不携带整份配置。
///
/// 面板在任务运行期间用它刷新「运行中」与「下次运行」，避免为了几行状态而
/// 重新下发配置、把用户正在编辑的表单冲掉。
fn runtime_handler(ctx: &AdminContext) -> Response {
    Response::json(
        200,
        &json!({
            "runtime": ctx.runtime_json(),
            "version": ctx.store.version(),
            "web_version": ctx.web_store.version(),
        }),
    )
}

/// 立即运行单个任务。
///
/// 与定时触发共用同一个执行入口（含任务级互斥闸门），因此重复点击不会跑出
/// 两个实例；真正的执行结果写日志，面板只负责告诉用户「已触发」。
fn run_handler(ctx: &AdminContext, request: &Request) -> Response {
    let body = match request.json_body() {
        Ok(body) => body,
        Err(message) => return Response::text(400, message),
    };

    let module = body
        .get("module")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    let id = body
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if module.is_empty() || id.is_empty() {
        return Response::json(400, &json!({ "message": "请求体需要 module 与 id" }));
    }

    let task = {
        let registry = ctx.runners.lock().unwrap_or_else(|err| err.into_inner());
        registry.get(&module, &id)
    };

    let Some(task) = task else {
        return Response::json(
            404,
            &json!({ "message": format!("任务「{id}」不存在或尚未就绪，请先保存配置") }),
        );
    };

    spawn_task(ctx, task);
    Response::json(
        202,
        &json!({ "message": format!("已触发任务「{id}」，执行结果见日志") }),
    )
}

/// 立即运行某个模块下的全部任务。
fn run_all_handler(ctx: &AdminContext, request: &Request) -> Response {
    let body = match request.json_body() {
        Ok(body) => body,
        Err(message) => return Response::text(400, message),
    };

    let module = body
        .get("module")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if module.is_empty() {
        return Response::json(400, &json!({ "message": "请求体需要 module" }));
    }

    let tasks = {
        let registry = ctx.runners.lock().unwrap_or_else(|err| err.into_inner());
        registry.by_module(&module)
    };

    if tasks.is_empty() {
        return Response::json(
            404,
            &json!({ "message": "该分区下还没有可运行的任务，请先添加并保存配置" }),
        );
    }

    let count = tasks.len();
    for task in tasks {
        spawn_task(ctx, task);
    }

    Response::json(
        202,
        &json!({ "message": format!("已触发 {count} 个任务，执行结果见日志"), "count": count }),
    )
}

/// 读取单个任务的运行日志。
///
/// 请求体 `{"module": "...", "id": "..."}`；只返回该任务自己的日志行（按时间
/// 顺序），并附带是否正在运行，供面板在执行期间持续刷新。
fn task_log_handler(request: &Request) -> Response {
    let body = match request.json_body() {
        Ok(body) => body,
        Err(message) => return Response::text(400, message),
    };
    let module = body
        .get("module")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    let id = body
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if module.is_empty() || id.is_empty() {
        return Response::json(400, &json!({ "message": "请求体需要 module 与 id" }));
    }
    if module.len() > 64 || id.len() > 256 {
        return Response::json(400, &json!({ "message": "任务标识过长" }));
    }

    let key = crate::task_log::task_key(module, id);
    let lines = crate::task_log::get()
        .map(|store| store.read(&key))
        .unwrap_or_default();
    let running = crate::schedule::running_tasks()
        .iter()
        .any(|item| item == &key);

    Response::json(
        200,
        &json!({
            "key": key,
            "lines": lines,
            "running": running,
            "limit": crate::task_log::MAX_LINES,
        }),
    )
}

/// 把一次手动触发交给 tokio 运行时执行，不占用 HTTP 连接线程。
fn spawn_task(ctx: &AdminContext, task: RegisteredTask) {
    info!(
        module = task.module,
        task_id = %task.id,
        source = "panel",
        "面板触发任务立即运行"
    );
    ctx.handle.spawn((task.run)());
}

/// 浏览指定 AList 客户端上的目录，供面板的「扫描目录」选择器使用。
///
/// 复用调度器里已经登录好的客户端，因此不需要再传账号密码；只返回目录项，
/// 文件不参与选择。请求体：`{"alist": "客户端 ID", "path": "/某个目录"}`。
fn browse_handler(ctx: &AdminContext, request: &Request) -> Response {
    let body = match request.json_body() {
        Ok(body) => body,
        Err(message) => return Response::text(400, message),
    };

    let alist_id = body
        .get("alist")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if alist_id.is_empty() {
        return Response::json(
            400,
            &json!({ "message": "请求体需要 alist（AList 客户端 ID）" }),
        );
    }

    let path = normalize_alist_path(body.get("path").and_then(Value::as_str).unwrap_or("/"));

    let client = {
        let clients = ctx
            .alist_clients
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        clients.get(&alist_id).map(|(client, _)| client.clone())
    };

    let Some(client) = client else {
        return Response::json(
            404,
            &json!({
                "message": format!("AList 客户端「{alist_id}」尚未就绪，请先保存配置"),
            }),
        );
    };

    let request_path = path.clone();
    let listing = ctx.handle.block_on(async move {
        match tokio::time::timeout(BROWSE_TIMEOUT, client.fs_list(FsListReq::all(request_path)))
            .await
        {
            Ok(result) => result.map_err(|err| err.to_string()),
            Err(_) => Err("请求 AList 目录超时，请稍后重试".to_string()),
        }
    });

    let listing = match listing {
        Ok(listing) => listing,
        Err(message) => {
            return Response::json(
                502,
                &json!({ "message": format!("读取目录失败: {message}") }),
            );
        }
    };

    let mut entries: Vec<(String, String)> = listing
        .content
        .into_iter()
        .map(|item| item.object)
        .filter(|item| item.is_dir)
        .map(|item| {
            let child = join_alist_path(&path, &item.name);
            (item.name, child)
        })
        .collect();
    entries.sort_by(|left, right| left.0.cmp(&right.0));

    let entries: Vec<Value> = entries
        .into_iter()
        .map(|(name, child)| json!({ "name": name, "path": child }))
        .collect();

    Response::json(
        200,
        &json!({
            "alist": alist_id,
            "path": path,
            "parent": parent_alist_path(&path),
            "entries": entries,
        }),
    )
}

/// 规范化 AList 路径：补前导 `/`、去掉多余尾部 `/`。
fn normalize_alist_path(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return "/".to_string();
    }

    let mut path = if trimmed.starts_with('/') {
        trimmed.to_string()
    } else {
        format!("/{trimmed}")
    };
    while path.len() > 1 && path.ends_with('/') {
        path.pop();
    }
    path
}

/// 拼接子目录路径。
fn join_alist_path(parent: &str, name: &str) -> String {
    if parent == "/" {
        format!("/{name}")
    } else {
        format!("{parent}/{name}")
    }
}

/// 计算上级目录；根目录没有上级。
fn parent_alist_path(path: &str) -> Option<String> {
    if path == "/" {
        return None;
    }

    match path.rfind('/') {
        Some(0) | None => Some("/".to_string()),
        Some(index) => Some(path[..index].to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_and_joins_alist_paths() {
        assert_eq!(normalize_alist_path(""), "/");
        assert_eq!(
            normalize_alist_path("  /媒体库/国产剧/  "),
            "/媒体库/国产剧"
        );
        assert_eq!(normalize_alist_path("媒体库"), "/媒体库");
        assert_eq!(join_alist_path("/", "动漫"), "/动漫");
        assert_eq!(join_alist_path("/媒体库", "动漫"), "/媒体库/动漫");
    }

    #[test]
    fn computes_parent_alist_paths() {
        assert_eq!(parent_alist_path("/"), None);
        assert_eq!(parent_alist_path("/动漫"), Some("/".to_string()));
        assert_eq!(
            parent_alist_path("/媒体库/动漫"),
            Some("/媒体库".to_string())
        );
    }

    fn request(remote: &str, headers: &[(&str, &str)]) -> Request {
        Request {
            method: "GET".to_string(),
            path: "/api/state".to_string(),
            headers: headers
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect(),
            body: Vec::new(),
            remote_addr: remote.to_string(),
        }
    }

    #[test]
    fn client_key_only_trusts_forwarded_for_behind_internal_proxy() {
        // 内网反代：采信第一跳，避免所有请求挤在同一个 key 上互相锁死。
        let behind_proxy = request(
            "172.18.0.2",
            &[("x-forwarded-for", "203.0.113.9, 10.0.0.1")],
        );
        assert_eq!(client_key(&behind_proxy), "203.0.113.9");

        // 直连公网：请求头可以随意伪造，必须忽略。
        let direct = request("203.0.113.50", &[("x-forwarded-for", "1.2.3.4")]);
        assert_eq!(client_key(&direct), "203.0.113.50");

        // 非法值不会进入限流表，回退到真实对端地址。
        let garbage = request("127.0.0.1", &[("x-forwarded-for", "not-an-ip")]);
        assert_eq!(client_key(&garbage), "127.0.0.1");
    }

    #[test]
    fn session_cookie_is_secure_only_over_https() {
        assert!(session_cookie("token", 60, true).contains("; Secure"));
        assert!(!session_cookie("token", 60, false).contains("Secure"));
        assert!(cleared_cookie(true).contains("Max-Age=0; Secure"));

        let https = request("127.0.0.1", &[("x-forwarded-proto", "https")]);
        assert!(is_https(&https));
        let http = request("127.0.0.1", &[("x-forwarded-proto", "http")]);
        assert!(!is_https(&http));
        assert!(!is_https(&request("127.0.0.1", &[])));
    }
}

/// 返回当前业务配置的 YAML 文本，便于复制备份。
///
/// 敏感字段已脱敏：这份文本常被贴到 issue 或聊天里，不应带出明文密钥。
fn config_yaml_handler(ctx: &AdminContext) -> Response {
    match serde_yaml::to_string(&ctx.store.snapshot().config.masked()) {
        Ok(yaml) => {
            // 明确告知脱敏：避免用户把它直接覆盖回 config.yaml 后发现登录失败。
            let mut text = String::from(
                "# 敏感字段已在导出时替换为占位符（密码 / 令牌 / API Key），\n\
                 # 本文件不能直接用于启动；需要完整配置请直接查看磁盘上的 config.yaml。\n\n",
            );
            text.push_str(&yaml);
            Response::new(200, "text/yaml; charset=utf-8", text)
        }
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

    let current = ctx.store.snapshot();
    if let Some(response) = conflict("业务配置", &body, current.version) {
        return response;
    }

    let Some(config_value) = body.get("config").cloned() else {
        return Response::text(400, "请求体缺少 config 字段");
    };

    let mut config: Config = match serde_json::from_value(config_value) {
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

    // 前端拿到的是脱敏占位符，提交回来时换回真实密钥；用户真正改过的字段不受影响。
    config.unmask(&current.config);

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
                "{label}已被其他会话或外部文件修改（当前版本 {current_version}），请刷新页面获取最新配置后再保存"
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
            return Response::text(503, format!("等待{label}生效超时，请稍后刷新面板确认结果"));
        }
        Err(std_mpsc::RecvTimeoutError::Disconnected) => {
            return Response::text(500, format!("等待{label}应用结果失败"));
        }
    };

    match outcome {
        ApplyOutcome::Applied(report) => {
            let message = if report.scope == "web" {
                "面板配置已保存并实时生效".to_string()
            } else {
                format!("配置已保存并生效，共 {} 个调度任务", report.scheduled_count)
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
        ApplyOutcome::Locked(tasks) => {
            let message = format!(
                "任务 {} 正在运行，其配置已锁定；请等任务结束后再修改",
                tasks.join("、")
            );
            Response::json(
                423,
                &json!({ "message": message, "locked_tasks": tasks, "errors": [] }),
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

/// 预览当前已生效配置会生成的 MediaWarp YAML（不写盘）。
fn mediawarp_preview_handler(ctx: &AdminContext) -> Response {
    let config = ctx.store.snapshot().config.mediawarp;
    let path = ctx.mediawarp.resolve_path(&config.config_path);
    match mediawarp::render_yaml(&config) {
        Ok(yaml) => Response::json(
            200,
            &json!({ "yaml": yaml, "path": path.display().to_string() }),
        ),
        Err(message) => Response::json(500, &json!({ "message": message })),
    }
}

/// 读取 MediaWarp 的落盘日志（面板 MediaWarp 页的「运行日志」弹窗）。
///
/// 请求体 `{"source": "service" | "access"}`，缺省为服务日志。日志不在内存里另存
/// 一份，直接读 MediaWarp 自己按天写出的文件，所以面板看到的与 `tail -f` 一致。
fn mediawarp_log_handler(ctx: &AdminContext, request: &Request) -> Response {
    let body = match request.json_body() {
        Ok(body) => body,
        Err(message) => return Response::text(400, message),
    };
    let raw = body
        .get("source")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let Some(source) = mediawarp::LogSource::parse(raw) else {
        return Response::json(
            400,
            &json!({ "message": "source 只能是 service 或 access" }),
        );
    };

    let config = ctx.store.snapshot().config.mediawarp;
    // 关掉文件输出时 MediaWarp 根本不会创建日志文件，提前告诉面板，
    // 否则用户只会看到「暂无日志」，误以为服务没在跑。
    let file_enabled = match source {
        mediawarp::LogSource::Service => config.log.service.file,
        mediawarp::LogSource::Access => config.log.access.file,
    };

    let dir = ctx.mediawarp.log_dir();
    let snapshot = mediawarp::logs::read(&dir, source);

    Response::json(
        200,
        &json!({
            "source": snapshot.source,
            "dir": dir.display().to_string(),
            "path": snapshot.path,
            "day": snapshot.day,
            "lines": snapshot.lines,
            "truncated": snapshot.truncated,
            "file_enabled": file_enabled,
            "managed": config.enable,
            "limit": mediawarp::logs::MAX_LINES,
        }),
    )
}

/// 面板「立即重启」：与配置保存共用热重载内核的串行命令通道，避免并发抢端口。
fn mediawarp_restart_handler(ctx: &AdminContext) -> Response {
    let (reply, receiver) = std_mpsc::sync_channel(1);
    if ctx
        .commands
        .blocking_send(AdminCommand::MediaWarpRestart { reply })
        .is_err()
    {
        return Response::text(503, "配置内核已停止，无法重启 MediaWarp");
    }

    let status = || ctx.mediawarp.status_json();
    match receiver.recv_timeout(APPLY_TIMEOUT) {
        Ok(Ok(note)) => Response::json(200, &json!({ "message": note, "status": status() })),
        Ok(Err(message)) => Response::json(500, &json!({ "message": message, "status": status() })),
        Err(std_mpsc::RecvTimeoutError::Timeout) => {
            Response::text(503, "等待 MediaWarp 重启超时，请稍后刷新面板确认服务状态")
        }
        Err(std_mpsc::RecvTimeoutError::Disconnected) => {
            Response::text(500, "等待 MediaWarp 重启结果失败")
        }
    }
}

/// 把生成文件（可能是外部部署或手工编辑过的）导回面板表单。
///
/// 只返回解析后的配置对象，由前端合并进草稿；用户点「保存」才真正写盘并重启，
/// 因此导入本身不会中断正在播放的请求。
fn mediawarp_import_handler(ctx: &AdminContext) -> Response {
    let current = ctx.store.snapshot().config.mediawarp;
    let path = ctx.mediawarp.resolve_path(&current.config_path);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) => {
            return Response::json(
                404,
                &json!({ "message": format!("读取 {} 失败: {err}", path.display()) }),
            );
        }
    };

    match mediawarp::parse_into(&text, &current) {
        Ok(config) => Response::json(
            200,
            &json!({ "config": config, "path": path.display().to_string() }),
        ),
        Err(message) => Response::json(422, &json!({ "message": message })),
    }
}
