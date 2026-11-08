use std::path::{Component, Path, PathBuf};

use crate::extensions::VIDEO_EXTS;

/// 返回文件名的小写扩展名。
///
/// 结果包含前导 `.`；文件名没有扩展名时返回空字符串。该函数用于统一处理
/// AList 返回的文件名和配置中的扩展名匹配。
pub fn suffix_of(name: &str) -> String {
    match name.rsplit_once('.') {
        Some((_, ext)) => format!(".{}", ext.to_ascii_lowercase()),
        None => String::new(),
    }
}

/// 判断远端路径是否是 BDMV 主片候选文件。
///
/// 只有 `BDMV/STREAM` 目录下的 `.m2ts` 会作为候选参与后续按大小选择；
/// BDMV 目录里的其它文件会被跳过，避免产生大量无意义 `.strm`。
pub fn is_bdmv_file(full_path: &str, name: &str) -> bool {
    // 只处理 BDMV/STREAM 下的 m2ts；其它 BDMV 内部文件全部跳过。
    full_path.contains("/BDMV/STREAM/") && suffix_of(name) == ".m2ts"
}

/// 提取 BDMV 目录的根路径。
///
/// 返回 `/BDMV/` 之前的远端目录，用作同一蓝光原盘内 m2ts 文件的分组键。
/// 该分组会在扫描结束后选出最大 m2ts 作为主片。
pub fn bdmv_root(full_path: &str) -> Option<String> {
    full_path
        .find("/BDMV/")
        .map(|index| full_path[..index].to_string())
        .filter(|root| !root.is_empty())
}

/// 从 BDMV 根路径中提取电影标题。
///
/// 标题来自根路径的最后一级目录；如果路径为空或无法提取，则回退为 `BDMV`。
/// 结果用于生成 BDMV 主片对应的本地 `.strm` 文件名。
pub fn movie_title_from_bdmv_root(root: &str) -> String {
    root.trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|value| !value.is_empty())
        .unwrap_or("BDMV")
        .to_string()
}

/// 拼接 AList 父目录和子项名称。
///
/// 函数会避免父目录末尾的 `/` 造成双斜杠，并确保根目录下的子项仍以
/// `/name` 形式表示。
pub fn join_alist_path(parent: &str, name: &str) -> String {
    let parent = parent.trim_end_matches('/');
    if parent.is_empty() {
        format!("/{name}")
    } else {
        format!("{parent}/{name}")
    }
}

/// 生成 AList `/d/...` 下载链接。
///
/// 该链接遵循 Python 版本的规则：站点地址 + `/d` + 用户 `base_path` +
/// 远端完整路径，并在存在签名时附加 `sign` 查询参数。路径组件会做 URL
/// 编码，确保中文和空格等字符可用于 `.strm` 内容或伴生文件下载。签名值
/// 不做编码，直接追加，避免 `=` 和 `:` 等字符被百分编码后导致 AList
/// 服务端验签失败。
pub fn download_url(server_url: &str, base_path: &str, full_path: &str, sign: &str) -> String {
    // Python 版本的下载链接规则是：server + "/d" + base_path + full_path + sign。
    let abs_path = format!(
        "{}{}",
        base_path.trim_end_matches('/'),
        ensure_leading_slash(full_path)
    );
    let mut url = format!(
        "{}/d{}",
        server_url.trim_end_matches('/'),
        encode_path(&abs_path)
    );
    if !sign.is_empty() {
        url.push_str("?sign=");
        url.push_str(sign);
    }
    url
}

/// 计算远端路径对应的本地目标路径。
///
/// 非平铺模式会保留 `source_dir` 之下的目录结构；平铺模式只使用文件名。
/// 视频文件会转换为 `.strm` 后缀。BDMV 主片会以 BDMV 根目录名生成单个
/// 电影标题 `.strm`，避免把整盘结构里的多个 m2ts 都暴露给媒体库。
///
/// 文件名与目录名来自远端接口，属于外部输入，因此所有片段都会经过
/// [`sanitize_components`] 过滤，确保结果永远落在 `target_dir` 之内。
pub fn local_path(
    full_path: &str,
    name: &str,
    source_dir: &str,
    target_dir: &Path,
    flatten_mode: bool,
    bdmv_root: Option<&str>,
) -> PathBuf {
    // BDMV 只为最大 m2ts 生成一个电影标题 .strm，避免整盘结构被扫成多个视频。
    if let Some(bdmv_root) = bdmv_root {
        let movie_title = movie_title_from_bdmv_root(bdmv_root);
        let file_name = format!("{}.strm", safe_file_name(&movie_title, "BDMV"));
        return if flatten_mode {
            target_dir.join(file_name)
        } else {
            target_dir
                .join(sanitize_components(&relative_from_source(
                    bdmv_root, source_dir,
                )))
                .join(file_name)
        };
    }

    let mut local_path = if flatten_mode {
        target_dir.join(safe_file_name(name, "unnamed"))
    } else {
        let mut relative = sanitize_components(&relative_from_source(full_path, source_dir));
        if relative.file_name().is_none() {
            relative.push(safe_file_name(name, "unnamed"));
        }
        target_dir.join(relative)
    };

    if VIDEO_EXTS.contains(&suffix_of(name).as_str()) {
        local_path.set_extension("strm");
    }
    local_path
}

/// 计算某个远端目录在本地对应的目录路径。
///
/// 用于把「远端扫描失败的目录」映射成本地排除前缀，避免同步清理把没能
/// 扫描到的文件误判为已删除。平铺模式下所有文件都落在 `target_dir` 第一
/// 层，无法按目录区分，因此返回 `None`。
pub fn local_dir(
    source_dir: &str,
    target_dir: &Path,
    remote_dir: &str,
    flatten_mode: bool,
) -> Option<PathBuf> {
    if flatten_mode {
        return None;
    }
    Some(target_dir.join(sanitize_components(&relative_from_source(
        remote_dir, source_dir,
    ))))
}

/// 过滤掉可能逃逸目标目录的路径片段。
///
/// 远端文件名/目录名可能包含 `..`、根目录或盘符前缀，直接 `join` 会让结果
/// 落到 `target_dir` 之外（`Path::join` 遇到绝对路径会整体替换，遇到 `..`
/// 会上跳）。这里只保留普通名称片段。
fn sanitize_components(path: &Path) -> PathBuf {
    let mut sanitized = PathBuf::new();
    for component in path.components() {
        if let Component::Normal(part) = component {
            sanitized.push(part);
        }
    }
    sanitized
}

/// 把远端文件名收敛为单个安全的文件名。
///
/// 只保留最后一级名称并过滤掉特殊片段；无法得到有效名称时使用 `fallback`。
fn safe_file_name(name: &str, fallback: &str) -> String {
    let path = Path::new(name);
    let candidate = path.file_name().map(Path::new).unwrap_or(Path::new(""));
    let sanitized = sanitize_components(candidate);
    match sanitized.to_str() {
        Some(value) if !value.is_empty() => value.to_string(),
        _ => fallback.to_string(),
    }
}

/// 计算远端路径相对 `source_dir` 的本地相对路径。
///
/// 非平铺模式下用这个结果保持云端目录结构；如果远端路径不在 `source_dir`
/// 之下，则使用原路径去掉开头 `/` 后的形式，避免生成绝对本地路径。
fn relative_from_source(path: &str, source_dir: &str) -> PathBuf {
    // 非平铺模式下，本地目录结构与 source_dir 下的云端结构保持一致。
    let source = source_dir.trim_end_matches('/');
    let relative = path
        .strip_prefix(source)
        .unwrap_or(path)
        .trim_start_matches('/');
    PathBuf::from(relative)
}

/// 确保路径字符串以 `/` 开头。
///
/// 生成 AList 下载链接时需要绝对路径形态；该函数把缺失前导斜杠的路径补齐。
fn ensure_leading_slash(path: &str) -> String {
    if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    }
}

/// 对远端路径的每个片段做 URL 编码。
///
/// 函数按 `/` 拆分路径，只编码各个路径片段本身，从而保留目录分隔符并兼容
/// 中文、空格、特殊符号等文件名。
fn encode_path(path: &str) -> String {
    path.split('/')
        .map(urlencoding::encode)
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_alist_paths_without_double_slashes() {
        assert_eq!(join_alist_path("/", "movie.mkv"), "/movie.mkv");
        assert_eq!(join_alist_path("/media", "movie.mkv"), "/media/movie.mkv");
    }

    #[test]
    fn extracts_bdmv_root_and_title() {
        let full_path = "/电影/Example/BDMV/STREAM/00001.m2ts";
        assert!(is_bdmv_file(full_path, "00001.m2ts"));
        assert_eq!(bdmv_root(full_path).as_deref(), Some("/电影/Example"));
        assert_eq!(movie_title_from_bdmv_root("/电影/Example"), "Example");
    }

    #[test]
    fn download_url_does_not_encode_sign() {
        let url = download_url(
            "http://192.168.10.15:5255",
            "",
            "/media/movie.mkv",
            "SGzogi30rKyG5MVNVuwREmRYhE4iDlKcIGIGqpR1m9c=:0",
        );
        assert_eq!(
            url,
            "http://192.168.10.15:5255/d/media/movie.mkv?sign=SGzogi30rKyG5MVNVuwREmRYhE4iDlKcIGIGqpR1m9c=:0"
        );
    }

    #[test]
    fn download_url_without_sign_has_no_query() {
        let url = download_url("http://192.168.10.15:5255", "", "/media/movie.mkv", "");
        assert_eq!(url, "http://192.168.10.15:5255/d/media/movie.mkv");
    }

    #[test]
    fn local_path_converts_video_ext_to_strm() {
        let target_dir = Path::new("/media/ani");
        assert_eq!(
            local_path("/ani/2026/01.mkv", "01.mkv", "/ani", target_dir, false, None),
            PathBuf::from("/media/ani/2026/01.strm")
        );
        assert_eq!(
            local_path("/ani/2026/01.mkv", "01.mkv", "/ani", target_dir, true, None),
            PathBuf::from("/media/ani/01.strm")
        );
    }

    #[test]
    fn local_path_rejects_parent_directory_escapes() {
        let target_dir = Path::new("/media/ani");

        // `..` 片段会被丢弃，不会上跳出 target_dir。
        assert_eq!(
            local_path(
                "/ani/../../etc/cron.d/evil.mkv",
                "evil.mkv",
                "/ani",
                target_dir,
                false,
                None
            ),
            PathBuf::from("/media/ani/etc/cron.d/evil.strm")
        );

        // 绝对路径形式的文件名不会替换掉 target_dir。
        assert_eq!(
            local_path("/ani/x.mkv", "/etc/passwd", "/ani", target_dir, true, None),
            PathBuf::from("/media/ani/passwd")
        );

        // 纯 `..` 名称回退为 unnamed，仍然留在 target_dir 内。
        let escaped = local_path("/ani/..", "..", "/ani", target_dir, true, None);
        assert_eq!(escaped, PathBuf::from("/media/ani/unnamed"));
    }

    #[test]
    fn local_path_stays_inside_target_dir_for_bdmv() {
        let target_dir = Path::new("/media/ani");
        let path = local_path(
            "/ani/../../etc/BDMV/STREAM/00001.m2ts",
            "00001.m2ts",
            "/ani",
            target_dir,
            false,
            Some("/ani/../../etc"),
        );

        assert!(path.starts_with(target_dir), "path escaped: {}", path.display());
    }

    #[test]
    fn local_dir_maps_remote_dir_to_target_subtree() {
        let target_dir = Path::new("/media/ani");
        assert_eq!(
            local_dir("/ani", target_dir, "/ani/2026/01", false),
            Some(PathBuf::from("/media/ani/2026/01"))
        );
        assert_eq!(local_dir("/ani", target_dir, "/ani/2026", true), None);
    }
}
