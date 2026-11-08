[prs]: https://github.com/AkimioJR/AutoFilm
[prs-badge]: https://img.shields.io/badge/PRs-welcome-brightgreen.svg?style=flat-square
[issues]: https://github.com/AkimioJR/AutoFilm/issues/new
[issues-badge]: https://img.shields.io/badge/Issues-welcome-brightgreen.svg?style=flat-square
[release]: https://github.com/zian-yan/AutoFilm/releases/latest
[release-badge]: https://img.shields.io/github/v/release/zian-yan/AutoFilm?style=flat-square
[docker]: https://github.com/zian-yan/AutoFilm/pkgs/container/autofilm
[docker-badge]: https://img.shields.io/badge/GHCR-autofilm-2496ED?style=flat-square&logo=docker&logoColor=white

<div align="center">

# AutoFilm

**一个为Emby、Jellyfin提供生成strm、动漫追番、绘制媒体库海报的小工具** 

[![prs][prs-badge]][prs]
[![issues][issues-badge]][issues]
[![release][release-badge]][release]
[![docker][docker-badge]][docker]


[说明文档](#说明文档) •
[部署方式](#部署方式) •
[Web 管理面板](#web-管理面板) •
[Strm文件优点](#Strm文件优点) •
[TODO LIST](#todo-list) •
[更新日志](CHANGELOG.md) •
[贡献者](#贡献者) •
[Star History](#star-history)

</div>

# 说明文档
详情见 [AutoFilm 说明文档](https://blog.akimio.top/posts/1031/)

# 运行方式
1. Docker 运行（**推荐**）
   见[docker-compose.yaml](docker/compose.yaml)

   镜像地址 `ghcr.io/zian-yan/autofilm:latest`，`latest` 始终跟随最新正式版；需要固定版本时改用 `ghcr.io/zian-yan/autofilm:1.0.0` 这类语义化标签。

2. 二进制运行
    ```bash
    ./autofilm --help # 可选，查看具体使用说明
    ./autofilm --config /path/to/config.yaml --web-config /path/to/web.yaml
    ```

## 两份互相独立的配置文件

| 文件 | 默认路径 | 职责 | 生效方式 |
| --- | --- | --- | --- |
| 业务配置 | `config/config.yaml` | AList 客户端、媒体服务器、同步 / 追番 / 海报任务 | 保存后热重载调度器 |
| 面板配置 | `config/web.yaml` | 面板监听地址、HTTP Basic 认证、文件监听开关 | 保存后需重启进程 |

两份文件各自读写、各自持有版本号：面板保存业务配置**不会**重写 `web.yaml`，反之亦然，不存在谁覆盖谁的问题。结构参考 [`config/config.example.yaml`](./config/config.example.yaml) 与 [`config/web.example.yaml`](./config/web.example.yaml)。

**两份配置都允许为空**（文件不存在、只有注释、或显式 `null` 均可）：程序按各自的默认值启动，不会报错，方便在初始化阶段先建目录、之后再通过面板或手工编辑逐步补齐。业务配置缺失时会补一份仅含注释的骨架文件，面板配置缺失时会写入默认值；解析失败（例如 YAML 语法错误）仍然会报错，避免静默忽略改坏的配置。

# Web 管理面板

内置一个轻量管理面板（不依赖外部前端资源，静态文件直接编译进二进制），可以在线增删改配置，业务配置保存后**自动热重载，无需重启进程**。

面板自身的设置保存在独立的 `web.yaml`（不再是 `config.yaml` 里的 `web:` 段）：

```yaml
enable: true                      # 启用面板
listen: 127.0.0.1:8080            # 必须为 ip:port；非回环地址必须设置密码
username: admin                   # HTTP Basic 认证用户名
password: "your-password"         # 留空表示不认证（此时仅允许监听回环地址）
watch_file: true                  # 监听 config.yaml / web.yaml 外部改动并自动重载
```

启动后访问 `http://127.0.0.1:8080`，浏览器会弹出 HTTP Basic 登录框。

> **注意**：监听非回环地址却没有设置 `password` 时，进程会在启动阶段直接报错退出，不会以「无认证」状态对外提供服务。面板接口会返回 AList 令牌与媒体服务器 API Key，请勿直接暴露到公网；如需远程访问，建议前置反向代理并启用 HTTPS。

## 配置数据结构与校验规则

面板按 `alist`、`media_servers`、`alist2strm_tasks`、`ani2alist_tasks`、`library_poster_tasks` 五个业务分区渲染表单，另有独立的「Web 面板」分区用于编辑 `web.yaml`。字段结构由 `/api/state` 下发的 schema（`sections` + `web_section`）描述，与 Rust 结构体（`src/config.rs`、`src/web_config.rs` 及各子模块 `config.rs`）保持同一套命名。

保存前会做完整静态校验，**任何一处不通过都不会写盘**：

| 规则 | 说明 |
| --- | --- |
| 唯一性 | 各分区内 `id` 必填且不可重复 |
| 引用完整性 | 任务的 `alist` / `server` 必须指向已存在的条目 |
| Cron | 需为 6 段（秒 分 时 日 月 周）或 7 段；留空表示只手工触发 |
| 数值范围 | 并发数 1-1024、下载并发 1-256、超时与阈值 > 0、主题色强度 0.0-1.0、模糊半径 ≥ 0 |
| 路径形态 | AList 扫描目录与挂载路径必须以 `/` 开头；海报任务至少配置一个媒体库 |
| 面板安全 | 监听非回环地址时必须设置密码；设置密码时必须填写用户名 |

Cron 表达式的语法细节由调度器二次校验：构建调度任务失败时整次保存回滚，原配置继续运行。

## 热重载机制与生效范围

触发方式有三种：面板保存（业务配置 `PUT /api/config`、面板配置 `PUT /api/web`）、手工点击「重新加载文件」（`POST /api/reload`，两份一起重读）、配置文件被外部修改（默认 2 秒轮询 `watch_file`，两份文件分别检测）。

业务配置的应用顺序（任一步失败即回滚，旧配置继续运行）：

1. 静态校验 → 2. 构建并启动**新**调度器（重建 AList / 媒体服务器客户端、重新注册全部 cron 任务）→ 3. 原子写盘（原文件备份为 `config.yaml.bak`）→ 4. 切换内存配置并停用旧调度器。

面板配置只做「校验 → 原子写盘（备份为 `web.yaml.bak`）」，不触碰调度器。

生效范围：

- 新增 / 修改 / 删除的任务立即按新配置与 cron 生效；
- **正在执行的任务不会被打断**，其运行上下文仍是启动时的配置快照，下一次触发才使用新配置；
- 面板自身的监听地址与认证信息属于服务级设置，保存后会提示「重启生效」；
- 直接编辑 `config.yaml` / `web.yaml` 触发的重载**只读取、不回写**，注释与排版保持原样；只有面板保存才会原子重写对应文件，并把上一版备份为 `.bak`；

## 权限控制与反馈

- HTTP Basic 认证，凭据比较为定长比较；连续 8 次失败后按来源 IP 锁定 60 秒；
- 变更类请求必须携带 `X-AutoFilm-Request: 1` 自定义头，配合 `Content-Security-Policy` 同源限制阻断 CSRF；
- 密码不会回传到前端（`/api/state`、`/api/config.yaml`、`/api/web.yaml` 均已脱敏），留空提交表示保持原密码；
- 保存成功：右上角提示版本号、调度任务数与耗时；校验失败：按 `section[index].field` 精确定位到输入框并列出全部问题；版本冲突（其他会话已修改）返回 409 并提示重新加载。


# Strm文件优点
- [x] 轻量化 Emby 服务器，降低 Emby 服务器的性能需求以及硬盘需求
- [x] 运行稳定
- [x] 相比直接访问 Webdav，Emby、Jellyfin 服务器可以提供更好的视频搜索功能以及自带刮削器，以及多设备同步播放进度
- [x] 提高访问速度，播放速度不受 Emby / Jellyfin 服务器带宽限制（需要使用 [MediaWarp](https://github.com/AkimioJR/MediaWarp)）

# TODO LIST
- [x] 从 config 文件中读取配置
- [x] 优化程序运行效率（异步处理）
- [x] 增加 Docker 镜像
- [x] 本地同步网盘
- [x] Alist 永久令牌
- [x] LibraryPoster（媒体库海报，感谢[HappyQuQu/jellyfin-library-poster](https://github.com/HappyQuQu/jellyfin-library-poster)）
- [ ] 使用 API 触发任务
- [ ] 通知功能
- [ ] ~~对接 TMDB 实现分类、重命名、刮削等功能~~
    > 已经向 [MoviePilot](https://github.com/jxxghp/MoviePilot) 提交支持对 Alist 服务器文件的操作功能的 PR，目前已经合并进入主线分支，可以直接使用 MoviePilot 直接刮削

# 功能演示
## LibraryPoster

视觉设计参考了[HappyQuQu/jellyfin-library-poster](https://github.com/HappyQuQu/jellyfin-library-poster)和[MoviePilot-Plugins/mediacovergenerator](https://github.com/justzerock/MoviePilot-Plugins/tree/main/plugins.v2/mediacovergenerator)，
图像处理流程由 AutoFilm 使用 Rust 重新实现。

<table width="100%">
  <tr>
    <td align="center" width="50%">
      <strong>blur</strong>
      <br>
      <img src="./img/library-poster/blur.png" alt="LibraryPoster blur style" width="100%">
    </td>
    <td align="center" width="50%">
      <strong>card</strong>
      <br>
      <img src="./img/library-poster/card.png" alt="LibraryPoster card style" width="100%">
    </td>
  </tr>
  <tr>
    <td align="center" width="50%">
      <strong>collage</strong>
      <br>
      <img src="./img/library-poster/collage.png" alt="LibraryPoster collage style" width="100%">
    </td>
    <td align="center" width="50%">
      <strong>split</strong>
      <br>
      <img src="./img/library-poster/split.png" alt="LibraryPoster split style" width="100%">
    </td>
  </tr>
</table>


# 贡献者
<a href="https://github.com/AkimioJR/AutoFilm/graphs/contributors">
  <img src="https://contrib.rocks/image?repo=AkimioJR/AutoFilm" />
</a>

# Star History
<a href="https://github.com/AkimioJR/AutoFilm/stargazers">
    <img width="500" alt="Star History Chart" src="https://api.star-history.com/svg?repos=AkimioJR/AutoFilm&type=Date">
</a> 
