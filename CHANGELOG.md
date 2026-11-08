# 更新日志

本文件记录本项目的重要变更。

格式基于 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，
版本号遵循 [语义化版本](https://semver.org/lang/zh-CN/)。

> 本仓库从 `v1.0.0` 起重置版本号，作为独立发布线的起点；此前上游（AkimioJR/AutoFilm）的历史版本请查阅 Git 提交记录与上游 Release。

## [1.1.0] - 2026-11-08

### ⚠️ 破坏性变更

- **[Alist2Strm]**: 并发数、伴生文件下载与同步删除策略从「每个任务」上移为全局配置段 `alist2strm`（`concurrency` / `download.*` / `sync.*`）。`alist2strm_tasks` 中的同名键会被忽略，启动时打印迁移警告。
  > **迁移说明**：把任务里原先的 `concurrency`、`download`、`sync` 三段整体移到根级 `alist2strm` 下，再从各任务中删除。

### 新增

- **面板**: 任务列表改为表格（任务名 / 存储 / 路径 / 定时 / 下次运行 / 操作），点「编辑」才展开完整表单；新增「立即运行」与「运行所有」，与 cron 触发共用同一执行入口并带任务级互斥；新增 `POST /api/run`、`POST /api/run-all`、`GET /api/runtime`。
- **面板**: 配色对齐 Jellyfin（`#00A4DC` / `#AA5CC3`），提供深色与浅色双主题（跟随系统 + 手动切换 + `?theme=` 强制），图标全部自绘；导航与地址栏 hash 同步。
- **面板**: 新增「同步全局设置」分区，集中编辑 `alist2strm` 全局段。
- **面板**: 编辑任务的「扫描目录」/「挂载路径」时点「浏览」即可从 AList 已有目录中直接选择（新增 `POST /api/browse`，复用已登录的 AList 客户端，10 秒超时）。
- **面板**: 新增独立登录页 `/login`（替代浏览器原生 Basic 弹窗）：未登录页面 302 跳转、接口 401，新增 `POST /api/login` / `POST /api/logout`；会话为内存中的随机 token（12 小时滑动过期、上限 128），Cookie 设置 `HttpOnly` + `SameSite=Strict`，右上角提供退出登录，脚本仍可用 HTTP Basic 访问接口。

### 变更

- **[LibraryPoster]**: 媒体服务器认证按 `kind` 分流——Jellyfin 改用 `Authorization: MediaBrowser Token="..."`，兼容 12.0 起默认禁用 `X-Emby-Token` / `?api_key=` 的行为，且向下兼容 10.8+；Emby 保持原有认证方式。Jellyfin 的 API Key 新增字符校验（面板保存与客户端创建时均会拦截含引号、空格等值）。
- **面板**: 移除侧栏的「业务 YAML / 面板 YAML」直链（接口仍保留，只是不再在页面上展示）。

## [1.0.0] - 2026-11-08

首个独立发布版本。相较上游基线，本版本补齐了 Web 管理面板、配置热重载与一整套安全加固，并把面板配置从业务配置中彻底拆分出来。

### ⚠️ 破坏性变更

- **[Config]**: Web 面板配置从业务配置中拆分，改由独立的 `config/web.yaml` 承载（可用 `--web-config` 指定路径）。`config.yaml` 不再接受 `web:` 分区，旧文件中的该段会被忽略并打印一条迁移警告。
  > **迁移说明**：把原 `config.yaml` 中的 `web:` 段内容平移到 `config/web.yaml`（去掉外层 `web:` 键），再从 `config.yaml` 中删除该段。
- **[Config]**: 业务配置与面板配置均允许为空——文件缺失、空文件、仅含注释或 `null` 都会按内置默认值启动，不再报错退出；只有 YAML 语法错误仍然快速失败。
- **[Alist2Strm]**: 移除 `mode` 配置项与 `RawURL` / `AlistPath` 两种 `.strm` 内容模式，`.strm` 统一写入 AList `/d` 直链；旧配置中的 `mode` 字段会被忽略。

### ✨ 新增

- **[Admin]**: 内置 Web 管理面板（静态资源内嵌进二进制，零外部前端依赖）：schema 驱动表单、增删改条目、按 `section[index].field` 定位校验错误、乐观并发控制（版本冲突返回 409）。
- **[Admin]**: 配置热重载内核——校验 → 构建并启动新调度器 → 原子写盘 → 切换 → 停用旧调度器，任一步失败都保留旧配置继续运行。触发来源：面板保存、手工「重新加载文件」、配置文件外部改动（2 秒轮询）。
- **[Admin]**: 业务配置与面板配置各自独立的存储、版本号与接口（`PUT /api/config`、`PUT /api/web`），面板保存互不覆盖；外部编辑与手工重载只读取、不回写，保留注释与排版。
- **[Alist2Strm]**: 智能删除保护：远端目录扫描失败时不参与清理，大批删除先进入宽限确认。
- **[LibraryPoster]**: 支持 Collage / Card / Split / Blur 四种海报渲染风格与自定义分辨率。
- **[Core]**: 任务互斥（同一任务上一轮未结束则跳过本轮）、`.part` 临时文件原子写入、SIGTERM 优雅退出。

### 🔒 安全

- **[Admin]**: HTTP Basic 认证使用定长比较，连续 8 次失败按来源 IP 锁定 60 秒（限流表定期清理并设有容量上限）。
- **[Admin]**: 变更类请求必须携带 `X-AutoFilm-Request: 1`，配合 CSP 与同源限制阻断 CSRF；未设置密码时只允许监听回环地址，否则进程拒绝启动。
- **[Admin]**: 密码与密钥不回传前端（`/api/state`、`/api/config.yaml`、`/api/web.yaml` 均脱敏）。
- **[Config]**: 配置文件（含明文密钥）在 Unix 下按 `0600` 创建，保存时同步收紧权限，`.bak` 备份同样处理。
- **[Alist2Strm]**: 修复远端文件名/路径未规范化导致的目录穿越，并校验最终路径仍位于 `target_dir` 内。

### 🐛 修复

- **[Alist2Strm]**: 修复 AList 目录扫描失败被当作「远端已删除」而清空本地文件的问题。
- **[Alist2Strm]**: 修复智能删除保护的状态文件被同步清理反复删除、导致宽限计数永远无法累积的问题。
- **[Admin]**: 面板保存/重载改为带超时的等待，避免热重载卡住时占满连接名额导致面板假死；连接计数改为原子占用。
- **[Core]**: 修复互斥锁中毒后面板请求永久 panic 的问题。
- **[Ani2Alist]**: UrlTree 写入前清理名称中的冒号与换行，解析/写入/序列化增加深度上限。

### ⚡ 优化

- **[Core]**: AList 客户端补充连接与请求超时，并发 `401` 时只登录一次，避免站点挂起导致任务永久等待。
- **[Core]**: 日志按天切分并保留 14 天，结构化字段贯穿任务与请求上下文。
- **[Admin]**: 面板支持深色/浅色自适应与移动端断点布局。
- **[Release]**: 发布流程收敛为只由 `v*.*.*` 版本 tag 触发，不再在每次 main 分支提交时产出滚动 `pre-release`；Releases 列表只保留正式版本。
- **[Release]**: Docker 镜像标签固定为 `latest` + 语义化版本（`1.0.0` / `1.0` / `1`），不再产出 `main` 分支标签；`docker/compose.yaml` 与文档改为指向本仓库镜像 `ghcr.io/zian-yan/autofilm`。
