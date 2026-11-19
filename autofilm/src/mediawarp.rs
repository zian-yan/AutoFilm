//! MediaWarp 集成：把外部反向代理服务交给 AutoFilm 托管。
//!
//! MediaWarp（<https://github.com/zian-yan/MediaWarp>）是前置于 Emby/Jellyfin
//! 的 Go 反向代理，用于让 HTTPStrm 走 302 直链播放。它**只认一份配置文件**
//! （默认 `config/config.yaml`），且不提供 Web 配置页与热重载，改动必须重启进程。
//!
//! 本模块把这件事收拢到 AutoFilm 一侧：
//! - [`config`]：面板可编辑的配置结构体与静态校验，保存在 AutoFilm 的 `config.yaml`
//!   的 `mediawarp` 段（唯一数据源）；
//! - [`render`]：把面板配置渲染成 MediaWarp 原生 `config.yaml`，并支持反向解析导入；
//! - [`supervisor`]：子进程生命周期与「写盘 → 重启 → 健康检查 → 失败回滚」事务。
//! - [`logs`]：读 MediaWarp 自己落盘的 `service.log` / `access.log`，供面板的
//!   运行日志弹窗展示。
//!
//! 生成文件与面板配置的分离是刻意设计：MediaWarp 有自己的字段命名与校验，
//! 直接把 AutoFilm 的托管参数塞进它的配置会耦合两个项目；而面板配置落在
//! `config.yaml` 里，则复用了既有的版本控制、乐观锁与文件监听通道。

mod config;
pub mod logs;
mod render;
mod supervisor;

pub use config::MediaWarpConfig;
pub use logs::Source as LogSource;
pub use render::{parse as parse_yaml, parse_into, render as render_yaml};
pub use supervisor::{CommitReport, Phase, Status, Supervisor};
