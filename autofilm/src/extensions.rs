// 默认会生成 .strm 的视频文件后缀；全局 `alist2strm.strm_ext` 可覆盖该列表。
pub const VIDEO_EXTS: &[&str] = &[
    ".mp4", ".mkv", ".flv", ".avi", ".wmv", ".ts", ".rmvb", ".webm", ".mpg", ".m2ts",
];

// 需要保留源文件后缀再追加 `.strm` 的镜像后缀。
//
// 蓝光原盘镜像（`xxx.iso`）必须有 `.iso.strm` 这样的完整名字，媒体服务器才能
// 识别出它指向的是 ISO 而不是普通视频；其它后缀仍按 `xxx.strm` 输出。
pub const ISO_EXT: &str = ".iso";

// 可选下载的伴生文件类型，行为与 Python 版本保持一致。
pub const SUBTITLE_EXTS: &[&str] = &[".ass", ".srt", ".ssa", ".sub"];

pub const IMAGE_EXTS: &[&str] = &[".png", ".jpg", ".jpeg", ".webp"];

pub const NFO_EXTS: &[&str] = &[".nfo"];
