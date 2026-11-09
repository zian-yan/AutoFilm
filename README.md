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

   镜像地址 `ghcr.io/zian-yan/autofilm:latest`，`latest` 始终跟随最新正式版；需要固定版本时改用 `ghcr.io/zian-yan/autofilm:2.2.0` 这类语义化标签。

2. 二进制运行
    ```bash
    ./autofilm --help # 可选，查看具体使用说明
    ./autofilm --config /path/to/config.yaml --web-config /path/to/web.yaml
    ```


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
