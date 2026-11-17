[release]: https://github.com/zian-yan/AutoFilm/releases/latest
[release-badge]: https://img.shields.io/github/v/release/zian-yan/AutoFilm?style=flat-square
[docker]: https://github.com/zian-yan/AutoFilm/pkgs/container/autofilm
[docker-badge]: https://img.shields.io/badge/GHCR-autofilm-2496ED?style=flat-square&logo=docker&logoColor=white
[license]: ./LICENSE
[license-badge]: https://img.shields.io/badge/license-AGPL--3.0%20(modified)-blue?style=flat-square

<div align="center">

# AutoFilm

**一个为 Emby、Jellyfin 提供生成 strm、动漫追番、绘制媒体库海报的小工具**

本仓库是 [AkimioJR/AutoFilm](https://github.com/AkimioJR/AutoFilm) 的衍生分支，
配套 [AkimioJR/MediaWarp](https://github.com/AkimioJR/MediaWarp) 使用。

[![release][release-badge]][release]
[![docker][docker-badge]][docker]
[![license][license-badge]][license]

[项目简介](#项目简介) •
[仓库结构](#仓库结构) •
[部署方式](#部署方式) •
[与上游的差异](#与上游的差异) •
[致谢](#致谢) •
[许可证](#许可证) •
[更新日志](autofilm/CHANGELOG.md)

</div>

# 项目简介

AutoFilm 把网盘 / AList 上的影视资源整理成 Strm 文件，交给 Emby、Jellyfin 刮削播放，
并额外提供动漫追番与媒体库海报（LibraryPoster）绘制能力。

Strm 方案的好处：

- 轻量化媒体服务器：不需要为影视文件本身准备硬盘，降低 Emby / Jellyfin 的存储与性能需求
- 相比直连 WebDAV，媒体服务器仍能提供刮削器、搜索与多设备播放进度同步
- 播放流量不经过媒体服务器（需配合 [MediaWarp](https://github.com/AkimioJR/MediaWarp) 做 302 直链）

上游说明文档见 [AutoFilm 说明文档](https://blog.akimio.top/posts/1031/)。

# 仓库结构

这是一个聚合仓库，根目录下两个源码目录并列：

| 目录 | 内容 |
| --- | --- |
| `autofilm/` | AutoFilm（Rust）源码，所有 `cargo` 命令都在此目录执行 |
| `mediawarp/` | [MediaWarp](https://github.com/zian-yan/MediaWarp)（Go）子模块，镜像构建时与本项目一起编译 |
| `.github/` | 仓库级 CI：GitHub Actions 只识别**仓库根**的 `.github/workflows`，因此留在根 |

克隆需要带上子模块：

```bash
git clone --recursive https://github.com/zian-yan/AutoFilm.git
# 已克隆过的仓库
git submodule update --init --recursive
```

Docker 构建的上下文必须是**仓库根目录**（需要同时拿到两个源码目录）：

```bash
docker build -f autofilm/docker/Dockerfile -t autofilm:local .
```

# 部署方式

1. **Docker 运行（推荐）**

   见 [`autofilm/docker/compose.yaml`](autofilm/docker/compose.yaml)。
   镜像地址 `ghcr.io/zian-yan/autofilm:latest`，`latest` 跟随最新正式版，需要固定版本时改用
   `ghcr.io/zian-yan/autofilm:3.3.1` 这类语义化标签。

2. **从源码构建**

   ```bash
   cd autofilm
   cargo build --release
   ./target/release/autofilm --help      # 可选，查看具体使用说明
   ./target/release/autofilm --config /path/to/config.yaml --web-config /path/to/web.yaml
   ```

完整配置结构见 [`autofilm/config/config.example.yaml`](autofilm/config/config.example.yaml)。

# 与上游的差异

除跟随上游功能外，本分支主要增加了 **Web 管理面板** 与 **MediaWarp 托管**：

- 面板覆盖 Strm 任务、Ani 任务、媒体服务器、AList 存储与任务日志，配置落盘前有静态校验与乐观锁
- MediaWarp 由 AutoFilm 作为子进程托管：面板里改完配置点「保存并重启」，AutoFilm 渲染出
  MediaWarp 原生 YAML、原子写入、优雅重启并做健康检查，失败自动回滚
- 具体行为、配置字段与回滚顺序见 [`autofilm/CHANGELOG.md`](autofilm/CHANGELOG.md)

# 致谢

本项目的绝大部分工作来自 **AkimioJR**，没有这两个上游项目就没有本仓库：

- [**AkimioJR/AutoFilm**](https://github.com/AkimioJR/AutoFilm) — AutoFilm 全部核心功能
  （Strm 生成、AList 对接、动漫追番、LibraryPoster 海报）的原作者与主要实现者。
  本仓库的 `autofilm/` 即在其基础上派生，`autofilm/LICENSE` 保留原始许可与署名。
- [**AkimioJR/MediaWarp**](https://github.com/AkimioJR/MediaWarp) — 前置于 Emby / Jellyfin
  的反向代理，Strm 302 直链播放的实现方。`mediawarp/` 子模块源码来自该项目
  （经 [zian-yan/MediaWarp](https://github.com/zian-yan/MediaWarp) 镜像），许可见 `mediawarp/LICENSE`。

同时感谢上游 README 致谢过的项目：[HappyQuQu/jellyfin-library-poster](https://github.com/HappyQuQu/jellyfin-library-poster)
与 [justzerock/MoviePilot-Plugins](https://github.com/justzerock/MoviePilot-Plugins) 的 mediacovergenerator
（LibraryPoster 的视觉设计参考），以及 [MoviePilot](https://github.com/jxxghp/MoviePilot)。

据上游许可第 3 条要求声明：**本项目中与 MediaWarp、AutoFilm 相关的代码与实现思路均来自上述两个项目**。

# 许可证

本仓库采用 **修改版 GNU AGPL-3.0**，与上游 [AkimioJR/AutoFilm](https://github.com/AkimioJR/AutoFilm)、
[AkimioJR/MediaWarp](https://github.com/AkimioJR/MediaWarp) 完全一致。完整文本见
[`LICENSE`](./LICENSE)（仓库根，覆盖整个仓库），`autofilm/LICENSE` 为同一份文件（随二进制发布包分发），
`mediawarp/LICENSE` 随子模块保留。

在 AGPL-3.0 之上附加的三条条件：

1. 归属权属于原作者 AkimioJR（akimio.jr@gmail.com）；
2. 相关代码不得用于商业活动；
3. 使用或参考本项目代码时，必须开源你的修改，并明确注明代码 / 思路来源于本项目并附上项目链接。
