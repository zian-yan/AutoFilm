//! 面板前端静态资源。
//!
//! 直接编译进二进制，省去运行时资源目录与打包步骤；页面不引用任何外部
//! CDN，符合 `Content-Security-Policy` 的同源约束。

pub const INDEX_HTML: &str = include_str!("assets/index.html");
pub const APP_CSS: &str = include_str!("assets/app.css");
pub const APP_JS: &str = include_str!("assets/app.js");

/// 站点图标：从品牌标记抠图后生成的多尺寸 ICO（16/32/48/64）。
pub const FAVICON_ICO: &[u8] = include_bytes!("assets/favicon.ico");

/// 登录页：独立于主面板的极简页面，不加载主面板脚本。
pub const LOGIN_HTML: &str = include_str!("assets/login.html");
pub const LOGIN_CSS: &str = include_str!("assets/login.css");
pub const LOGIN_JS: &str = include_str!("assets/login.js");
