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
[MediaWarp 集成](#mediawarp-集成) •
[Strm文件优点](#Strm文件优点) •
[TODO LIST](#todo-list) •
[更新日志](CHANGELOG.md) •
[贡献者](#贡献者) •
[Star History](#star-history)

</div>

# 仓库结构

这是一个聚合仓库，根目录下是两个并列的源码目录：

| 目录 | 内容 |
| --- | --- |
| `autofilm/` | AutoFilm（Rust）源码，所有 `cargo` 命令都在此目录执行 |
| `mediawarp/` | MediaWarp（Go）v1.2.3 上游源码，镜像构建时直接编译 |
| `.github/` | 仓库级 CI：GitHub Actions 只识别**仓库根**的 `.github/workflows`，因此留在根 |

Docker 构建的上下文必须是**仓库根目录**（同时需要两个源码目录）：

```bash
docker build -f autofilm/docker/Dockerfile -t autofilm:local .
```

# 说明文档
详情见 [AutoFilm 说明文档](https://blog.akimio.top/posts/1031/)

# 运行方式
1. Docker 运行（**推荐**）
   见[docker-compose.yaml](docker/compose.yaml)

   镜像地址 `ghcr.io/zian-yan/autofilm:latest`，`latest` 始终跟随最新正式版；需要固定版本时改用 `ghcr.io/zian-yan/autofilm:2.2.1` 这类语义化标签。

2. 从源码构建
    ```bash
    cd autofilm
    cargo build --release
    ./target/release/autofilm --help                 # 可选，查看具体使用说明
    ./target/release/autofilm --config /path/to/config.yaml --web-config /path/to/web.yaml
    ```


# MediaWarp 集成

[MediaWarp](https://github.com/zian-yan/MediaWarp) 是前置于 Emby / Jellyfin 的反向代理，
让 HTTPStrm 以 302 直链播放，流量不经过媒体服务器。它本身只认一份 `config/config.yaml`，
**没有 Web 配置页、也不支持热重载**，改配置必须重启进程——这正是 AutoFilm 接管的部分。

## 运行形态

官方镜像（`ghcr.io/zian-yan/autofilm`）在构建时**直接编译仓库内 `mediawarp/` 的源码**
并一起打进镜像：AutoFilm 是容器主进程（PID 1），按面板配置拉起 / 重启
`/usr/local/bin/mediawarp` 子进程。两种二进制在同一容器内，无需暴露 Docker socket，
也无需额外的 `docker restart`。升级 MediaWarp 只需替换 `mediawarp/` 中的源码。

构建不再 clone GitHub，但 Go 依赖仍由 `go mod download` 从模块代理获取；如需完全离线，
先在 `mediawarp/` 下执行 `go mod vendor`，再把两个 Dockerfile 的 `go build` 改成
`go build -mod=vendor`。

二进制方式运行时，把 MediaWarp 可执行文件放进 `PATH`（或在面板里填绝对路径）即可；
未找到二进制时 AutoFilm 只会告警，不会退出，仍可通过面板修正路径后保存重试。

## 面板与配置流

侧边栏 **MediaWarp** 页包含服务状态卡片和按「服务托管 / 监听与上游 / 日志与缓存 /
客户端过滤 / HTTPStrm 302」分组的完整配置项。配置保存在 `config.yaml` 的 `mediawarp` 段
（复用业务配置的版本号与乐观锁），点「保存并重启」后的事务顺序是：

1. 静态校验（端口、上游地址类型、Go 时长格式、白名单非空、路径前缀等），错误直接回传并定位到输入框；
2. 渲染成 MediaWarp 原生 YAML，**原子写入** `config/mediawarp.yaml`（容器内 `/config/mediawarp.yaml`），保留 `.bak` 备份；
3. 对旧进程发 SIGTERM 等待优雅退出（超时才强制结束），再用 `--config <生成文件>` 拉起新进程；
4. 轮询 `127.0.0.1:<port>` 做健康检查；失败则还原生成文件、按旧配置重新拉起，并把原因显示到状态卡片。
5. 全部成功后才写 `config.yaml`；若这一步失败，同样回滚第 2~4 步，不留半成品状态。

所有重启在热重载内核里串行执行，**只有 MediaWarp 配置或生成文件真的变化时才重启**，
改动其它任务不会打断正在播放的请求。状态卡片每 3 秒随 `/api/runtime` 刷新，
另外提供「立即重启」「预览配置」「从文件导入」三个动作；导入只填表单，点保存才生效。

## 开始使用

1. 在面板 **MediaWarp** 页勾选「启用托管（保存后自动重启）」，填好上游地址、API 密钥与路径前缀；
2. 点「保存并重启」，状态卡片应变为「运行中」且端口健康检查「通过」；
3. 把客户端的访问地址改成容器的 `9000` 端口（`mediawarp.port`，见 `docker/compose.yaml`），
   不要再直连 Emby / Jellyfin。

生成文件由面板全权管理，请勿手工编辑；若已有外部部署的配置，用「从文件导入」带回面板。
示例见 [config/mediawarp.example.yaml](config/mediawarp.example.yaml)，
`config.yaml` 中 `mediawarp` 段的示例见 [config/config.example.yaml](config/config.example.yaml)。

> 关闭「启用托管」时 AutoFilm 仍会生成配置文件，但不拉起 / 重启进程，适用于把
> `config/mediawarp.yaml` 挂载给独立部署的 MediaWarp 容器、由外部负责重启的场景。

## 上游源码与许可

`mediawarp/` 目录包含 [MediaWarp](https://github.com/zian-yan/MediaWarp) **v1.2.3**
（tag `v1.2.3`，commit `acbb22d5740a3c57f6410cbe15208b11def195ec`）的完整源码，
仅用于在同一镜像内编译；其中的 `.git` 已移除，源码纳入本仓库版本控制。

其许可证为修改版 GNU AGPL-3.0，见 [`mediawarp/LICENSE`](../mediawarp/LICENSE)，
附加条件包括：归属原作者 AkimioJR、不得用于商业活动、使用或参考其代码须开源修改
并明确注明来源。据此声明：**本项目中 MediaWarp 相关代码与实现思路来自 MediaWarp 项目**，
项目链接 <https://github.com/zian-yan/MediaWarp>（上游 <https://github.com/AkimioJR/MediaWarp>）。
AutoFilm 自身的许可见 [`autofilm/LICENSE`](LICENSE)。

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

> **兼容性**：支持 Jellyfin 10.8+（含 12.x）与 Emby。Jellyfin 12 起服务器默认禁用 `X-Emby-Token` 与 `?api_key=`，本工具改用 `Authorization: MediaBrowser Token="..."` 认证；Jellyfin 的 API Key 请保持为字母、数字、`-`、`_`、`.`，不要包含空格或引号。

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
