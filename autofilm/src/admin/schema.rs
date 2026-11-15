use serde_json::{Value, json};

/// 返回管理面板使用的表单结构。
///
/// 字段的 `key` 使用点号路径，直接对应该条目在 JSON 中的嵌套位置（例如
/// `download.concurrency`）；前端据此渲染输入框并回填修改，后端则用 serde
/// 的结构体定义做最终校验，二者共用同一份字段命名。
pub fn schema() -> Value {
    json!({
        "sections": [
            {
                "key": "alist",
                "title": "AList 服务器",
                "subtitle": "所有任务都通过这里的客户端访问 AList / OpenList，标识 ID 供任务引用",
                "item_label": "AList 客户端",
                "addable": true,
                "removable": true,
                // 列表页的摘要列：前端按这些列渲染表格，编辑后才展开完整表单。
                "table": {
                    "columns": [
                        { "key": "id", "label": "存储名" },
                        { "key": "base_url", "label": "地址" },
                        { "key": "public_url", "label": "公共地址" },
                        { "key": "username", "label": "账号" }
                    ]
                },
                "fields": [
                    { "key": "id", "label": "标识 ID", "kind": "text", "required": true, "placeholder": "我的Alist", "help": "任务通过该 ID 引用本客户端，需全局唯一" },
                    { "key": "base_url", "label": "服务器地址", "kind": "text", "required": true, "placeholder": "http://alist:5244", "help": "AList 或 OpenList 的访问地址" },
                    { "key": "public_url", "label": "公共访问地址", "kind": "text", "nullable": true, "placeholder": "https://alist.example.com", "help": "生成 .strm 内容时使用的外部地址；留空则使用服务器地址" },
                    { "key": "username", "label": "用户名", "kind": "text", "nullable": true, "help": "使用令牌时可留空" },
                    { "key": "password", "label": "密码", "kind": "secret", "nullable": true },
                    { "key": "otp_code", "label": "两步验证码", "kind": "text", "nullable": true },
                    { "key": "token", "label": "永久令牌", "kind": "secret", "nullable": true, "help": "填写后无需用户名与密码" },
                    { "key": "wait_time", "label": "请求间隔（秒）", "kind": "number", "min": 0, "step": 0.1, "help": "0 表示不限速" }
                ],
                "template": {
                    "id": "",
                    "base_url": "http://",
                    "public_url": null,
                    "username": null,
                    "password": null,
                    "otp_code": null,
                    "token": null,
                    "wait_time": 0
                }
            },
            {
                "key": "media_servers",
                "title": "媒体服务器",
                "subtitle": "Jellyfin / Emby 连接信息；下方是依赖这些服务器的媒体库海报任务",
                "item_label": "媒体服务器",
                "addable": true,
                "removable": true,
                "table": {
                    "columns": [
                        { "key": "id", "label": "名称" },
                        { "key": "kind", "label": "类型", "kind": "map", "map": { "jellyfin": "Jellyfin", "emby": "Emby" } },
                        { "key": "base_url", "label": "地址" },
                        { "key": "timeout", "label": "超时（秒）" }
                    ]
                },
                "fields": [
                    { "key": "id", "label": "标识 ID", "kind": "text", "required": true, "placeholder": "我的Jellyfin" },
                    { "key": "kind", "label": "服务器类型", "kind": "select", "required": true, "options": [
                        { "value": "jellyfin", "label": "Jellyfin" },
                        { "value": "emby", "label": "Emby" }
                    ] },
                    { "key": "base_url", "label": "服务器地址", "kind": "text", "required": true, "placeholder": "http://jellyfin:8096" },
                    { "key": "api_key", "label": "API Key", "kind": "secret", "required": true },
                    { "key": "user_id", "label": "用户 ID", "kind": "text", "nullable": true, "help": "留空时自动使用首个用户" },
                    { "key": "timeout", "label": "请求超时（秒）", "kind": "number", "min": 1, "step": 1 }
                ],
                "template": {
                    "id": "",
                    "kind": "jellyfin",
                    "base_url": "http://",
                    "api_key": "",
                    "user_id": null,
                    "timeout": 30
                }
            },
            {
                // 挂在 alist2strm_tasks 页面下（parent），不再单独占一个侧边栏入口：
                // 两者是同一件事的「任务」与「公共策略」两面，前端用分栏切换。
                // 注意数据源不同：本分区与任务列表都来自 config.yaml，保存走同一个接口，
                // 因此合并到一页不涉及任何跨文件事务；web_section 的 detached 才是另一回事。
                "key": "alist2strm",
                "parent": "alist2strm_tasks",
                "title": "同步全局设置",
                "tab_label": "全局设置",
                "subtitle": "所有 Alist2Strm 同步任务共用的并发、伴生文件下载与同步删除策略，改一处即对全部任务生效；属于业务配置，保存在 config.yaml",
                // 卡片头标题：分栏已经写着「全局设置」，这里再重复一遍会浪费一行的信息量，
                // 改用它实际承载的内容命名。
                "item_label": "同步策略",
                "single": true,
                "addable": false,
                "removable": false,
                "fields": [
                    { "key": "concurrency", "label": "并发数", "kind": "number", "min": 1, "max": 1024, "step": 1, "help": "同时处理的文件数量上限，可减轻运行设备与 AList 负载" },
                    { "key": "download.enable", "label": "下载伴生文件", "kind": "bool", "help": "字幕、图片、NFO 与自定义后缀；平铺模式下始终不下载" },
                    { "key": "download.subtitle", "label": "下载字幕", "kind": "bool", "help": ".ass / .srt / .ssa / .sub" },
                    { "key": "download.image", "label": "下载图片", "kind": "bool", "help": ".png / .jpg" },
                    { "key": "download.nfo", "label": "下载 NFO", "kind": "bool" },
                    { "key": "download.other_ext", "label": "自定义后缀", "kind": "list", "placeholder": ".zip, .md", "help": "逗号分隔的自定义下载后缀" },
                    { "key": "download.concurrency", "label": "下载并发数", "kind": "number", "min": 1, "max": 256, "step": 1 },
                    { "key": "sync.enabled", "label": "同步删除", "kind": "bool", "help": "远端已不存在的本地文件会被清理；扫描失败的目录自动排除" },
                    { "key": "sync.ignore", "label": "删除忽略正则", "kind": "text", "nullable": true, "placeholder": "\\.(nfo|jpg)$", "help": "仅匹配文件名" },
                    { "key": "sync.smart_protection.enabled", "label": "智能删除保护", "kind": "bool", "help": "大量删除前先进入宽限确认" },
                    { "key": "sync.smart_protection.threshold", "label": "删除阈值", "kind": "number", "min": 1, "step": 1 },
                    { "key": "sync.smart_protection.grace_scans", "label": "宽限扫描次数", "kind": "number", "min": 1, "step": 1 }
                ]
            },
            {
                "key": "alist2strm_tasks",
                "title": "Strm 任务",
                "tab_label": "任务",
                "subtitle": "把 AList 上的视频扫描成本地 .strm 文件；并发、伴生文件下载与同步删除等公共策略在「全局设置」分栏中统一配置",
                "item_label": "同步任务",
                "addable": true,
                "removable": true,
                // 该分区支持「立即运行」，module 与后端任务执行入口的模块名一致。
                "module": "alist2strm",
                "next_run": true,
                "table": {
                    "columns": [
                        { "key": "id", "label": "任务名" },
                        { "key": "alist", "label": "存储", "kind": "ref" },
                        { "key": "source_dir", "label": "路径" },
                        { "key": "cron", "label": "定时", "kind": "cron" }
                    ]
                },
                "fields": [
                    { "key": "id", "label": "任务 ID", "kind": "text", "required": true, "placeholder": "动漫", "help": "用于日志与智能保护状态文件，需唯一" },
                    { "key": "cron", "label": "Cron 表达式", "kind": "cron", "nullable": true, "placeholder": "0 0 20 * * *", "help": "6 段：秒 分 时 日 月 周；可点「随机」生成每天 00:00–23:59 内的随机时刻；留空表示不参与调度" },
                    { "key": "alist", "label": "AList 客户端", "kind": "select", "required": true, "options_from": "alist" },
                    { "key": "source_dir", "label": "扫描目录", "kind": "alist_path", "required": true, "placeholder": "/ani", "alist_ref": "alist", "help": "点「浏览」直接从 AList 已有目录中选择" },
                    { "key": "target_dir", "label": "输出目录", "kind": "text", "required": true, "placeholder": "/media/ani", "help": "本地 .strm 与伴生文件输出位置" },
                    { "key": "flatten_mode", "label": "平铺模式", "kind": "bool", "help": "所有 .strm 输出到同一级目录，且不下载伴生文件" },
                    { "key": "overwrite", "label": "覆盖已存在文件", "kind": "bool" },
                    { "key": "dir_time_check", "label": "目录时间检查", "kind": "bool", "help": "开启后仅当远端目录的修改时间晚于本地目录时才扫描其子目录；若文件已更新但仍被跳过，请取消本选项" }
                ],
                "template": {
                    "id": "",
                    "cron": "0 0 20 * * *",
                    "alist": "",
                    "source_dir": "/",
                    "target_dir": "/media/",
                    "flatten_mode": false,
                    "overwrite": false,
                    "dir_time_check": false
                }
            },
            {
                "key": "ani2alist_tasks",
                "title": "Ani 任务",
                "subtitle": "按 RSS、当前季度或关键字解析番剧目录，并以 UrlTree 形式写入 AList",
                "item_label": "追番任务",
                "addable": true,
                "removable": true,
                "module": "ani2alist",
                "next_run": true,
                "table": {
                    "columns": [
                        { "key": "id", "label": "任务名" },
                        { "key": "alist", "label": "存储", "kind": "ref" },
                        { "key": "target_dir", "label": "挂载路径" },
                        { "key": "update.mode", "label": "更新模式", "kind": "map", "map": { "rss": "RSS 追更", "latest": "当前季度", "keyword": "自定义关键字" } },
                        { "key": "cron", "label": "定时", "kind": "cron" }
                    ]
                },
                "fields": [
                    { "key": "id", "label": "任务 ID", "kind": "text", "required": true, "placeholder": "新番追更" },
                    { "key": "cron", "label": "Cron 表达式", "kind": "cron", "nullable": true, "placeholder": "0 20 12 * * *", "help": "6 段：秒 分 时 日 月 周；可点「随机」生成每天随机时刻" },
                    { "key": "alist", "label": "AList 客户端", "kind": "select", "required": true, "options_from": "alist", "help": "需要管理员权限的 AList 客户端" },
                    { "key": "target_dir", "label": "挂载路径", "kind": "alist_path", "required": true, "placeholder": "/Anime", "alist_ref": "alist", "help": "UrlTree 存储器挂载路径，不存在时自动创建；可点「浏览」参考已有目录" },
                    { "key": "source.source_url", "label": "目录数据源", "kind": "text", "required": true, "placeholder": "https://aniopen.an-i.workers.dev" },
                    { "key": "source.rss_url", "label": "RSS 地址", "kind": "text", "required": true, "placeholder": "https://api.ani.rip/ani-download.xml" },
                    { "key": "update.mode", "label": "更新模式", "kind": "select", "required": true, "options": [
                        { "value": "rss", "label": "RSS 追更" },
                        { "value": "latest", "label": "当前季度" },
                        { "value": "keyword", "label": "自定义关键字" }
                    ] },
                    { "key": "update.template", "label": "子目录模板", "kind": "text", "nullable": true, "placeholder": "{{ year }}年/{{ month }}月", "help": "留空则直接写入挂载路径", "visible_when": { "field": "update.mode", "equals": "latest" } },
                    { "key": "update.keyword", "label": "关键字", "kind": "text", "placeholder": "2026-4", "visible_when": { "field": "update.mode", "equals": "keyword" } }
                ],
                "template": {
                    "id": "",
                    "cron": "0 20 12 * * *",
                    "alist": "",
                    "target_dir": "/Anime",
                    "source": {
                        "source_url": "https://aniopen.an-i.workers.dev",
                        "rss_url": "https://api.ani.rip/ani-download.xml"
                    },
                    "update": { "mode": "rss", "template": null, "keyword": null }
                }
            },
            {
                "key": "library_poster_tasks",
                "title": "媒体库海报",
                "parent": "media_servers",
                "subtitle": "抓取媒体库最新条目，合成海报图并上传到 Jellyfin / Emby",
                "item_label": "海报任务",
                "addable": true,
                "removable": true,
                "module": "library_poster",
                "next_run": true,
                "table": {
                    "columns": [
                        { "key": "id", "label": "任务名" },
                        { "key": "server", "label": "媒体服务器", "kind": "ref" },
                        { "key": "upload", "label": "上传", "kind": "bool" },
                        { "key": "libraries", "label": "媒体库", "kind": "count" },
                        { "key": "cron", "label": "定时", "kind": "cron" }
                    ]
                },
                "fields": [
                    { "key": "id", "label": "任务 ID", "kind": "text", "required": true, "placeholder": "媒体库海报" },
                    { "key": "cron", "label": "Cron 表达式", "kind": "cron", "nullable": true, "placeholder": "0 50 13 * * *", "help": "6 段：秒 分 时 日 月 周；可点「随机」生成每天随机时刻" },
                    { "key": "server", "label": "媒体服务器", "kind": "select", "required": true, "options_from": "media_servers" },
                    { "key": "upload", "label": "上传到服务器", "kind": "bool", "help": "覆盖媒体库 Primary 图片" },
                    { "key": "output_dir", "label": "预览输出目录", "kind": "text", "nullable": true, "placeholder": "/media/posters", "help": "未开启上传时必填" },
                    { "key": "title_font", "label": "主标题字体", "kind": "text", "required": true, "placeholder": "/fonts/ch.ttf" },
                    { "key": "subtitle_font", "label": "副标题字体", "kind": "text", "required": true, "placeholder": "/fonts/en.otf" },
                    { "key": "render.style", "label": "渲染风格", "kind": "select", "options": [
                        { "value": "collage", "label": "海报墙 Collage" },
                        { "value": "card", "label": "卡片 Card" },
                        { "value": "split", "label": "分栏 Split" },
                        { "value": "blur", "label": "模糊 Blur" }
                    ] },
                    { "key": "render.resolution", "label": "分辨率", "kind": "resolution" },
                    { "key": "render.blur_radius", "label": "模糊半径", "kind": "number", "min": 0, "step": 1, "help": "以 1080p 为基准自动缩放" },
                    { "key": "render.color_strength", "label": "主题色强度", "kind": "number", "min": 0, "max": 1, "step": 0.05 },
                    { "key": "libraries", "label": "媒体库列表", "kind": "libraries", "required": true, "help": "只处理这里明确列出的媒体库" }
                ],
                "template": {
                    "id": "",
                    "cron": "0 50 13 * * *",
                    "server": "",
                    "upload": false,
                    "output_dir": "/media/posters",
                    "title_font": "/fonts/ch.ttf",
                    "subtitle_font": "/fonts/en.otf",
                    "render": {
                        "style": "collage",
                        "resolution": "1080p",
                        "blur_radius": 50,
                        "color_strength": 0.8
                    },
                    "libraries": [
                        { "name": "", "title": "", "subtitle": "", "sort": "date_last_content_added" }
                    ]
                }
            },
            {
                // MediaWarp：数据仍在 config.yaml（复用业务配置的保存与版本控制），
                // 但页面由 app.js 按 widget 标记定制渲染——把 30 多个原生字段按
                // groups 拆成模块分栏，避免一屏铺满输入框。
                "key": "mediawarp",
                "title": "MediaWarp",
                "subtitle": "前置于 Emby / Jellyfin 的反向代理（HTTPStrm 302 直链）；本页配置保存后自动生成 config/mediawarp.yaml 并重启代理服务",
                "item_label": "MediaWarp 配置",
                "single": true,
                "addable": false,
                "removable": false,
                "widget": "mediawarp",
                "groups": [
                    {
                        "tab_label": "服务托管",
                        "title": "服务托管",
                        "subtitle": "AutoFilm 负责生成配置并管理子进程；关闭托管时只写配置文件，不自动重启",
                        "fields": [
                            { "key": "enable", "label": "启用托管（保存后自动重启）", "kind": "bool", "help": "开启后 AutoFilm 会拉起 MediaWarp 子进程；每次配置变化自动重启并通过端口健康检查。关闭时仅生成配置文件，供外部部署方式使用" },
                            { "key": "config_path", "label": "生成配置文件路径", "kind": "text", "required": true, "placeholder": "mediawarp.yaml", "help": "相对 config.yaml 所在目录；默认 mediawarp.yaml，即 config/mediawarp.yaml（容器内 /config/mediawarp.yaml），也可填绝对路径" },
                            { "key": "binary", "label": "可执行文件", "kind": "text", "required": true, "placeholder": "mediawarp", "help": "只写名字时按 PATH 查找；集成镜像内置 /usr/local/bin/mediawarp" },
                            { "key": "extra_args", "label": "附加启动参数", "kind": "list", "placeholder": "--debug", "help": "逗号分隔，追加在 --config 之后" },
                            { "key": "startup_timeout_secs", "label": "启动等待（秒）", "kind": "number", "min": 1, "max": 60, "step": 1, "help": "重启后等待端口就绪的最长时间，超时自动回滚" },
                            { "key": "stop_timeout_secs", "label": "停止宽限（秒）", "kind": "number", "min": 1, "max": 60, "step": 1, "help": "先发 SIGTERM 让 Go 侧优雅退出，超时才强制结束" }
                        ]
                    },
                    {
                        "tab_label": "端口与上游",
                        "title": "监听端口与上游媒体服务器",
                        "subtitle": "对应 MediaWarp 的 port / server 段",
                        "fields": [
                            { "key": "port", "label": "监听端口", "kind": "number", "min": 1, "max": 65535, "step": 1, "help": "客户端应访问 MediaWarp 的这个端口，而不是直连 Emby/Jellyfin" },
                            { "key": "server.type", "label": "媒体服务器类型", "kind": "select", "required": true, "options": [
                                { "value": "Emby", "label": "Emby" },
                                { "value": "Jellyfin", "label": "Jellyfin" }
                            ] },
                            { "key": "server.addr", "label": "上游服务器地址", "kind": "text", "required": true, "placeholder": "http://emby:8096", "help": "需为带主机名的 http(s) 完整地址" },
                            { "key": "server.auth", "label": "API 密钥", "kind": "secret", "keep_empty": true, "help": "Emby / Jellyfin 控制台 → 高级 → API 密钥" }
                        ]
                    },
                    {
                        "tab_label": "日志与缓存",
                        "title": "日志与缓存",
                        "subtitle": "对应 MediaWarp 的 log / cache 段；TTL 为 Go 时长格式，0 表示关闭该项缓存",
                        "fields": [
                            { "key": "log.access.console", "label": "访问日志 · 终端", "kind": "bool" },
                            { "key": "log.access.file", "label": "访问日志 · 文件", "kind": "bool" },
                            { "key": "log.service.console", "label": "服务日志 · 终端", "kind": "bool" },
                            { "key": "log.service.file", "label": "服务日志 · 文件", "kind": "bool" },
                            { "key": "cache.enable", "label": "启用缓存", "kind": "bool" },
                            { "key": "cache.http_strm_ttl", "label": "302 结果缓存", "kind": "text", "placeholder": "1m", "help": "仅当 final_url 开启时生效" },
                            { "key": "cache.image_ttl", "label": "图片缓存", "kind": "text", "placeholder": "10m" },
                            { "key": "cache.subtitle_ttl", "label": "字幕缓存", "kind": "text", "placeholder": "2h" }
                        ]
                    },
                    {
                        "tab_label": "客户端过滤",
                        "title": "客户端过滤",
                        "subtitle": "按 User-Agent 关键字包含关系匹配",
                        "fields": [
                            { "key": "client.enable", "label": "启用过滤", "kind": "bool" },
                            { "key": "client.mode", "label": "名单模式", "kind": "select", "options": [
                                { "value": "BlackList", "label": "黑名单（命中则拒绝）" },
                                { "value": "WhiteList", "label": "白名单（仅命中放行）" }
                            ] },
                            { "key": "client.list", "label": "客户端关键字", "kind": "list", "placeholder": "Fileball, Infuse", "help": "逗号分隔；白名单模式下不能为空" }
                        ]
                    },
                    {
                        "tab_label": "HTTPStrm",
                        "title": "HTTPStrm 302 直链",
                        "subtitle": "对应 MediaWarp 的 http_strm 段；prefix_list 是媒体服务器内 Strm 文件的路径前缀",
                        "fields": [
                            { "key": "http_strm.enable", "label": "启用 HTTPStrm 重定向", "kind": "bool" },
                            { "key": "http_strm.proxy", "label": "允许经过媒体服务器", "kind": "bool", "help": "开启后保留串流 / 转码能力，但流量会经过 Emby/Jellyfin" },
                            { "key": "http_strm.final_url", "label": "解析最终地址", "kind": "bool", "help": "重定向前先解析出最终 URL，减少客户端重定向次数" },
                            { "key": "http_strm.allow_loopback", "label": "允许环回目标", "kind": "bool", "help": "SSRF 防护：默认拒绝 127.0.0.1 / ::1；仅同机 Alist 直连且确认无敏感服务时开启" },
                            { "key": "http_strm.compatibility_mode", "label": "兼容模式", "kind": "bool", "help": "用更兼容但效率较低的方式获取最终 URL" },
                            { "key": "http_strm.hide_embedded_subtitles", "label": "隐藏内嵌字幕流", "kind": "bool", "help": "避免媒体服务器为提取字幕而整片下载远程文件" },
                            { "key": "http_strm.hide_embedded_subtitles_clients", "label": "隐藏字幕 · 客户端", "kind": "list", "placeholder": "Mozilla, Polaris", "help": "逗号分隔的 User-Agent 关键字；留空对所有客户端生效" },
                            { "key": "http_strm.prefix_list", "label": "Strm 路径前缀", "kind": "list", "placeholder": "/media/strm/http, /media/strm/https", "help": "逗号分隔，每项需以 / 开头" }
                        ]
                    }
                ]
            }
        ],
        // 面板配置单独下发：它保存在 web.yaml，与业务配置的版本号、保存接口
        // 完全独立，前端用 detached 标记把它当成独立表单渲染与提交。
        // 导航上用「面板自身」分组与业务分区隔开，避免被当成业务配置。
        "web_section": {
            "key": "web",
            "title": "Web 面板",
            "subtitle": "面板自身的监听地址与认证信息（保存在独立的 web.yaml），与业务配置 config.yaml 完全分离；保存后实时生效，无需重启进程",
            "item_label": "Web 面板设置",
            "addable": false,
            "removable": false,
            "realtime": true,
            "single": true,
            "detached": true,
            "fields": [
                { "key": "enable", "label": "启用 Web 面板", "kind": "bool", "help": "保存后立即启停面板；关闭后当前页面会断开连接" },
                { "key": "listen", "label": "监听地址", "kind": "text", "required": true, "placeholder": "127.0.0.1:8080", "help": "必须为 ip:port；非回环地址必须设置密码，保存后立即切换监听" },
                { "key": "username", "label": "用户名", "kind": "text", "help": "登录页的用户名，保存后立即生效" },
                { "key": "password", "label": "密码", "kind": "secret", "help": "登录页的密码，留空表示保持原密码不变；改动后所有登录会话立即失效" },
                { "key": "watch_file", "label": "监听配置文件改动", "kind": "bool", "help": "手工编辑 YAML 后自动触发重载（同时监听 config.yaml 与 web.yaml），开关改动即时生效" }
            ]
        },
        "library_fields": [
            { "key": "name", "label": "媒体库名称", "kind": "text", "required": true, "placeholder": "动漫" },
            { "key": "title", "label": "主标题", "kind": "text", "placeholder": "留空则使用媒体库名称" },
            { "key": "subtitle", "label": "副标题", "kind": "text", "placeholder": "ANIME" },
            { "key": "sort", "label": "排序方式", "kind": "select", "options": [
                { "value": "date_last_content_added", "label": "最近添加内容" },
                { "value": "date_created", "label": "创建时间" },
                { "value": "random", "label": "随机" }
            ] }
        ],
        "resolution_presets": [
            { "value": "480p", "label": "480p (854×480)" },
            { "value": "720p", "label": "720p (1280×720)" },
            { "value": "1080p", "label": "1080p (1920×1080)" }
        ]
    })
}
