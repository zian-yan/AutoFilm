use ab_glyph::{Font, FontArc, PxScale, ScaleFont};
use image::{DynamicImage, Rgba, RgbaImage, imageops::overlay};
use imageproc::geometric_transformations::{Border, Interpolation, rotate_about_center_no_crop};

use super::{
    Fonts, Result,
    utils::{
        adjust_brightness, apply_rounded_corners, cover, dominant_color, draw_text_left,
        optimized_blur, soft_shadow, spaced_text_width, text_line_height, tint, wrap_text,
    },
};
use crate::library_poster::RenderConfig;

// 以下比例取自 1920×1080 参考画布，运行时按实际分辨率等比缩放。
/// 单张海报宽度占画布宽度的比例（420 / 1920）。
const CARD_WIDTH_RATIO: f32 = 420.0 / 1920.0;
/// 海报间距与海报宽度的比值（30 / 420）。
const CARD_GAP_RATIO: f32 = 30.0 / 420.0;
/// 圆角半径与海报宽度的比值（95.238 / 1000）。
const CARD_RADIUS_RATIO: f32 = 95.238 / 1000.0;
/// 九宫格中心在画布上的相对位置。
const WALL_CENTER_X_RATIO: f32 = 1465.0 / 1920.0;
const WALL_CENTER_Y_RATIO: f32 = 650.0 / 1080.0;
/// 整面海报墙的倾斜角度（度）。
const WALL_ANGLE_DEGREES: f32 = 15.0;
/// 标题与副标题共用的左边距比例。
const TITLE_LEFT_RATIO: f32 = 176.0 / 1920.0;
/// 标题墨迹顶边在画布上的位置（1080p 下 356px）：文字块贴顶排布、副标题向下生长，
/// 不再纵向居中，避免与左下角斜向铺开的海报重叠。
const TITLE_TOP_RATIO: f32 = 0.33;
/// 标题墨迹高度占画布高度的比例，字号在此基础上按可用宽度收缩。
const TITLE_INK_RATIO: f32 = 0.175;
/// 副标题基准墨迹高度占比。
const SUBTITLE_INK_RATIO: f32 = 0.062;
/// 标题块最大宽度占比；实际宽度还会被同高度处的海报墙左缘进一步收窄。
const TITLE_MAX_WIDTH_RATIO: f32 = 0.34;
/// 文字块与海报墙之间保留的横向间距（1080p 下 46px），用于避开投影外溢。
const TITLE_WALL_GAP_RATIO: f32 = 46.0 / 1920.0;
/// 副标题最多折行数，超出后改为缩字号。
const SUBTITLE_MAX_LINES: usize = 2;
/// 判定海报墙「已铺到此处」的 alpha 阈值，低于此值视为旋转留白。
const WALL_ALPHA_THRESHOLD: u8 = 24;
/// 首张素材作为视觉重心放在墙面中心，其余素材依次铺满。
const SLOT_SOURCES: [usize; 9] = [1, 2, 3, 4, 0, 5, 6, 7, 8];

/// 生成左侧标题、右侧九张圆角海报倾斜排列的海报墙风格。
pub fn render(
    images: &[DynamicImage],
    title: &str,
    subtitle: &str,
    fonts: &Fonts,
    config: &RenderConfig,
    dimensions: (u32, u32),
) -> Result<RgbaImage> {
    let first = images.first().ok_or(super::Error::MissingImage)?;
    let (width, height) = dimensions;
    let width_f = width as f32;
    let height_f = height as f32;
    let theme = dominant_color(first);

    let mut canvas = cover(first, width, height);
    canvas = optimized_blur(&canvas, config.blur_radius.max(8.0) * height_f / 1080.0);
    tint(
        &mut canvas,
        adjust_brightness(theme, 0.55),
        config.color_strength.clamp(0.0, 1.0) * 0.35,
    );
    tint(&mut canvas, Rgba([0, 0, 0, 255]), 0.34);

    let wall = build_wall(images, width_f, height_f);
    // 先铺所有投影再压上海报，避免相邻海报的投影落在别的海报上。
    let shadow_blur = width_f * 0.0083;
    let shadow_offset = (width_f * 0.00625) as i64;
    for layer in &wall {
        let (shadow, padding) = soft_shadow(&layer.image, shadow_blur, 51);
        overlay(
            &mut canvas,
            &shadow,
            layer.left - padding,
            layer.top + shadow_offset - padding,
        );
    }
    for layer in &wall {
        overlay(&mut canvas, &layer.image, layer.left, layer.top);
    }

    draw_title_block(
        &mut canvas,
        title,
        subtitle,
        fonts,
        &wall,
        width_f,
        height_f,
    );
    Ok(canvas)
}

struct PosterLayer {
    image: RgbaImage,
    left: i64,
    top: i64,
}

fn build_wall(images: &[DynamicImage], width_f: f32, height_f: f32) -> Vec<PosterLayer> {
    let card_width = (width_f * CARD_WIDTH_RATIO).round().max(4.0);
    let card_height = (card_width * 1.5).round();
    let gap = card_width * CARD_GAP_RATIO;
    let radius = (card_width * CARD_RADIUS_RATIO).round() as u32;
    let (card_width, card_height) = (card_width as u32, card_height as u32);
    let angle = WALL_ANGLE_DEGREES.to_radians();
    let centers = wall_centers(
        width_f,
        height_f,
        card_width as f32,
        card_height as f32,
        gap,
    );

    centers
        .into_iter()
        .enumerate()
        .map(|(index, (center_x, center_y))| {
            let source = &images[SLOT_SOURCES[index] % images.len()];
            let mut card = cover(source, card_width, card_height);
            apply_rounded_corners(&mut card, radius);
            // 必须用 no_crop 版本：rotate_about_center 会保持原尺寸并裁掉旋转后的四角，
            // 卡片会变成多边形而不是长方形。
            let image = rotate_about_center_no_crop(
                &card,
                angle,
                Interpolation::Bicubic,
                Border::Constant(Rgba([0, 0, 0, 0])),
            );
            PosterLayer {
                left: (center_x - image.width() as f32 / 2.0).round() as i64,
                top: (center_y - image.height() as f32 / 2.0).round() as i64,
                image,
            }
        })
        .collect()
}

/// 计算倾斜后每张海报的中心坐标，顺序为「列优先」，第 5 张位于墙面正中。
fn wall_centers(
    width_f: f32,
    height_f: f32,
    card_width: f32,
    card_height: f32,
    gap: f32,
) -> [(f32, f32); 9] {
    let (sin, cos) = WALL_ANGLE_DEGREES.to_radians().sin_cos();
    let center_x = width_f * WALL_CENTER_X_RATIO;
    let center_y = height_f * WALL_CENTER_Y_RATIO;
    let step_x = card_width + gap;
    let step_y = card_height + gap;

    std::array::from_fn(|index| {
        let column = (index / 3) as f32 - 1.0;
        let row = (index % 3) as f32 - 1.0;
        let offset_x = column * step_x;
        let offset_y = row * step_y;
        (
            center_x + offset_x * cos - offset_y * sin,
            center_y + offset_x * sin + offset_y * cos,
        )
    })
}

fn draw_title_block(
    canvas: &mut RgbaImage,
    title: &str,
    subtitle: &str,
    fonts: &Fonts,
    wall: &[PosterLayer],
    width_f: f32,
    height_f: f32,
) {
    let (left, layout) = resolve_title_layout(title, subtitle, fonts, wall, width_f, height_f);
    let mut y = layout.block_top(height_f * TITLE_TOP_RATIO);

    if !title.is_empty() {
        draw_text_left(
            canvas,
            title,
            &fonts.title,
            left,
            y,
            layout.title_size,
            Rgba([255, 255, 255, 255]),
            0.0,
        );
        y += layout.title_height + layout.block_gap;
    }
    for line in &layout.subtitle_lines {
        draw_text_left(
            canvas,
            line,
            &fonts.subtitle,
            left,
            y,
            layout.subtitle_size,
            Rgba([255, 255, 255, 235]),
            layout.subtitle_spacing,
        );
        y += layout.subtitle_line_height + layout.subtitle_line_gap;
    }
}

/// 已度量好的标题块：字号、行高与副标题分行结果，纵向数值均为像素。
struct TitleLayout {
    title_size: f32,
    title_height: i32,
    /// 文字原点（行框顶部）到字形墨迹顶边的距离。
    ink_offset: f32,
    block_gap: i32,
    subtitle_size: f32,
    subtitle_spacing: f32,
    subtitle_line_height: i32,
    subtitle_line_gap: i32,
    subtitle_lines: Vec<String>,
}

impl TitleLayout {
    fn new(
        title: &str,
        subtitle: &str,
        fonts: &Fonts,
        width_f: f32,
        height_f: f32,
        max_width: i32,
    ) -> Self {
        // 按字形墨迹高度对齐参考模板，不受字体 em 差异影响。
        let title_size = fit_font_size(
            &fonts.title,
            title,
            ink_fitted_size(&fonts.title, title, height_f * TITLE_INK_RATIO),
            max_width,
            0.0,
        );
        // 副标题宽度跟随标题，避免宽字面字体把字幕拉得比标题还长。
        let title_width = spaced_text_width(&fonts.title, title_size, title, 0.0) as f32;
        let subtitle_limit = if title.is_empty() {
            max_width
        } else {
            (title_width * 1.05)
                .min(width_f * 0.32)
                .max(1.0)
                .min(max_width as f32) as i32
        };
        let (subtitle_size, subtitle_lines) = fit_subtitle(
            &fonts.subtitle,
            subtitle,
            height_f * SUBTITLE_INK_RATIO,
            subtitle_limit,
            0.10,
        );
        let title_height = if title.is_empty() {
            0
        } else {
            text_line_height(&fonts.title, title_size)
        };
        Self {
            title_size,
            title_height,
            ink_offset: if title.is_empty() {
                0.0
            } else {
                ink_top_offset(&fonts.title, title_size, title)
            },
            block_gap: if title.is_empty() || subtitle_lines.is_empty() {
                0
            } else {
                (title_size * 0.10) as i32
            },
            subtitle_size,
            subtitle_spacing: subtitle_size * 0.10,
            subtitle_line_height: if subtitle_lines.is_empty() {
                0
            } else {
                text_line_height(&fonts.subtitle, subtitle_size)
            },
            subtitle_line_gap: if subtitle_lines.len() > 1 {
                (subtitle_size * 0.22) as i32
            } else {
                0
            },
            subtitle_lines,
        }
    }

    fn subtitle_height(&self) -> i32 {
        if self.subtitle_lines.is_empty() {
            return 0;
        }
        self.subtitle_line_height * self.subtitle_lines.len() as i32
            + self.subtitle_line_gap * (self.subtitle_lines.len() - 1) as i32
    }

    fn block_height(&self) -> i32 {
        self.title_height + self.block_gap + self.subtitle_height()
    }

    /// 让标题墨迹顶边落在 `ink_top` 上时，文字原点（行框顶部）应有的 y。
    fn block_top(&self, ink_top: f32) -> i32 {
        (ink_top - self.ink_offset).round() as i32
    }

    /// 文字块占用的画布纵向区间，用于判断同高度处海报墙让出了多少横向空间。
    fn band(&self, ink_top: f32) -> (i64, i64) {
        let top = i64::from(self.block_top(ink_top));
        (top, top + i64::from(self.block_height()))
    }
}

/// 依海报墙实际左缘收敛标题块宽度，返回左边界与最终布局。
fn resolve_title_layout(
    title: &str,
    subtitle: &str,
    fonts: &Fonts,
    wall: &[PosterLayer],
    width_f: f32,
    height_f: f32,
) -> (i32, TitleLayout) {
    let left = (width_f * TITLE_LEFT_RATIO) as i32;
    let ink_top = height_f * TITLE_TOP_RATIO;
    let gap = width_f * TITLE_WALL_GAP_RATIO;
    let mut max_width = (width_f * TITLE_MAX_WIDTH_RATIO) as i32;
    let mut layout = TitleLayout::new(title, subtitle, fonts, width_f, height_f, max_width);

    // 海报墙左缘是斜的、越往下越靠左：先按占满最大宽度估块高，再按该块高让出宽度重排。
    // 只有两轮的原因是块高随字号单调收缩，第二轮已收敛，多轮不会进一步变宽。
    for _ in 0..2 {
        let (band_top, band_bottom) = layout.band(ink_top);
        let Some(edge) = wall_left_edge(wall, band_top, band_bottom) else {
            break;
        };
        let available = (edge as f32 - gap - left as f32).floor() as i32;
        if available >= max_width {
            break;
        }
        max_width = available.max(1);
        layout = TitleLayout::new(title, subtitle, fonts, width_f, height_f, max_width);
    }
    (left, layout)
}

/// 海报墙在画布纵向区间 `[top, bottom]` 内最靠左的不透明像素横坐标。
fn wall_left_edge(wall: &[PosterLayer], top: i64, bottom: i64) -> Option<i64> {
    let mut edge: Option<i64> = None;
    for layer in wall {
        let last_row = layer.image.height() as i64 - 1;
        let start = (top - layer.top).clamp(0, last_row);
        let end = (bottom - layer.top).clamp(-1, last_row);
        if end < start {
            continue;
        }
        let stride = layer.image.width() as usize * 4;
        let raw = layer.image.as_raw();
        for row in start..=end {
            let offset = row as usize * stride;
            let scanned = &raw[offset..offset + stride];
            let first_opaque = scanned
                .chunks_exact(4)
                .position(|pixel| pixel[3] > WALL_ALPHA_THRESHOLD);
            if let Some(x) = first_opaque {
                let canvas_x = layer.left + x as i64;
                edge = Some(edge.map_or(canvas_x, |current| current.min(canvas_x)));
            }
        }
    }
    edge
}

/// 副标题优先折行（最多两行），仍放不下时再逐级缩字号。
fn fit_subtitle(
    font: &FontArc,
    text: &str,
    size: f32,
    max_width: i32,
    spacing_ratio: f32,
) -> (f32, Vec<String>) {
    if text.trim().is_empty() {
        return (size, Vec::new());
    }
    let mut size = size;
    loop {
        let lines = wrap_text(font, PxScale::from(size), text, max_width);
        let too_wide = lines
            .iter()
            .any(|line| spaced_text_width(font, size, line, size * spacing_ratio) > max_width);
        if size <= 12.0 || (lines.len() <= SUBTITLE_MAX_LINES && !too_wide) {
            return (size, lines);
        }
        size *= 0.94;
    }
}

/// 以 100px 为基准测量字形墨迹高度，换算出目标墨迹高度对应的字号。
fn ink_fitted_size(font: &FontArc, text: &str, target_height: f32) -> f32 {
    const PROBE: f32 = 100.0;
    let ink = ink_height(font, PROBE, text);
    if ink <= f32::EPSILON {
        return PROBE;
    }
    PROBE * target_height / ink
}

/// 字形墨迹顶边相对绘制原点的偏移：imageproc 以「原点 + ascent」为基线出字。
fn ink_top_offset(font: &FontArc, size: f32, text: &str) -> f32 {
    let scaled = font.as_scaled(PxScale::from(size));
    let ink_top = text
        .chars()
        .filter(|character| !character.is_whitespace())
        .filter_map(|character| scaled.outline_glyph(scaled.scaled_glyph(character)))
        .map(|glyph| glyph.px_bounds().min.y)
        .fold(f32::INFINITY, f32::min);
    if ink_top.is_finite() {
        scaled.ascent() + ink_top
    } else {
        0.0
    }
}
fn ink_height(font: &FontArc, size: f32, text: &str) -> f32 {
    let scaled = font.as_scaled(PxScale::from(size));
    text.chars()
        .filter(|character| !character.is_whitespace())
        .filter_map(|character| scaled.outline_glyph(scaled.scaled_glyph(character)))
        .map(|glyph| glyph.px_bounds().height())
        .fold(0.0_f32, f32::max)
}

/// 标题超出可用宽度时逐步缩字号，避免与海报墙重叠。
fn fit_font_size(font: &FontArc, text: &str, size: f32, max_width: i32, spacing_ratio: f32) -> f32 {
    if text.is_empty() {
        return size;
    }
    let mut size = size;
    while size > 12.0 && spaced_text_width(font, size, text, size * spacing_ratio) > max_width {
        size *= 0.94;
    }
    size
}

#[cfg(test)]
mod tests {
    use image::{DynamicImage, Rgb, RgbImage};

    use super::*;

    fn centers_at_1080p() -> [(f32, f32); 9] {
        wall_centers(1920.0, 1080.0, 420.0, 630.0, 30.0)
    }

    #[test]
    fn wall_matches_reference_grid() {
        let centers = centers_at_1080p();

        // 中心海报与参考模板的 (1465, 650) 一致。
        assert!((centers[4].0 - 1465.0).abs() < 0.5);
        assert!((centers[4].1 - 650.0).abs() < 0.5);
        // 左上角海报落在画布外，形成斜向出血。
        assert!((centers[0].0 - 1201.15).abs() < 0.5);
        assert!((centers[0].1 + 103.98).abs() < 0.5);
    }

    #[test]
    fn wall_stays_aligned_at_other_resolutions() {
        let centers = wall_centers(3840.0, 2160.0, 840.0, 1260.0, 60.0);

        assert!((centers[4].0 - 2930.0).abs() < 1.0);
        assert!((centers[4].1 - 1300.0).abs() < 1.0);
        // 相邻海报中心间距随分辨率线性放大（列距 900、行距 1320，倾斜 15°）。
        assert!((centers[7].0 - centers[4].0 - 869.33).abs() < 1.0);
        assert!((centers[4].1 - centers[3].1 - 1275.02).abs() < 1.0);
    }

    #[test]
    fn title_shrinks_to_fit_available_width() {
        let font =
            ab_glyph::FontArc::try_from_slice(include_bytes!("../../../fonts/ch.ttf")).unwrap();
        let long_title = "非常非常长的媒体库标题";
        let fitted = fit_font_size(&font, long_title, 200.0, 400, 0.0);

        assert!(fitted < 200.0);
        assert!(spaced_text_width(&font, fitted, long_title, 0.0) <= 400);
    }

    #[test]
    fn renders_partial_image_sets() {
        for count in [1, 3, 9] {
            let images = (0..count)
                .map(|index| {
                    DynamicImage::ImageRgb8(RgbImage::from_pixel(
                        400,
                        600,
                        Rgb([40 + index as u8 * 12, 90, 180]),
                    ))
                })
                .collect::<Vec<_>>();
            let layer_count = build_wall(&images, 640.0, 360.0).len();

            assert_eq!(layer_count, 9);
        }
    }

    #[test]
    fn rotated_card_keeps_full_rectangle() {
        let card = RgbaImage::from_pixel(420, 630, Rgba([190, 120, 60, 255]));
        let rotated = rotate_about_center_no_crop(
            &card,
            WALL_ANGLE_DEGREES.to_radians(),
            Interpolation::Bicubic,
            Border::Constant(Rgba([0, 0, 0, 0])),
        );

        // 输出应等于旋转外接框；被裁切成多边形时这里是 420×630。
        assert_eq!(rotated.dimensions(), (569, 718));
        // 旋转不改变面积，被裁掉四角会少 13% 以上。
        let opaque = rotated.pixels().filter(|pixel| pixel[3] > 128).count() as i64;
        assert!((opaque - 420 * 630).abs() < 420 * 630 / 30);
    }

    #[test]
    fn title_stays_clear_of_poster_wall() {
        let fonts = Fonts {
            title: ab_glyph::FontArc::try_from_slice(include_bytes!("../../../fonts/ch.ttf"))
                .unwrap(),
            subtitle: ab_glyph::FontArc::try_from_slice(include_bytes!("../../../fonts/en.otf"))
                .unwrap(),
        };
        let images = (0..9)
            .map(|index| {
                DynamicImage::ImageRgb8(RgbImage::from_pixel(
                    400,
                    600,
                    Rgb([30 + index as u8 * 15, 90, 170]),
                ))
            })
            .collect::<Vec<_>>();

        // 海报墙左缘随分辨率等比左移，标题块必须在各档分辨率下都留出同比例间距。
        for (width, height) in [(1280.0, 720.0), (1920.0, 1080.0), (3840.0, 2160.0)] {
            for subtitle in ["", "JAPANESE · KOREAN · AMERICAN SERIES"] {
                let wall = build_wall(&images, width, height);
                let (left, layout) =
                    resolve_title_layout("日韩美剧", subtitle, &fonts, &wall, width, height);
                let (band_top, band_bottom) = layout.band(height * TITLE_TOP_RATIO);
                let edge =
                    wall_left_edge(&wall, band_top, band_bottom).expect("海报墙应至少有可见像素");

                let title_right = i64::from(left)
                    + i64::from(spaced_text_width(
                        &fonts.title,
                        layout.title_size,
                        "日韩美剧",
                        0.0,
                    ));
                let subtitle_right = layout
                    .subtitle_lines
                    .iter()
                    .map(|line| {
                        i64::from(left)
                            + i64::from(spaced_text_width(
                                &fonts.subtitle,
                                layout.subtitle_size,
                                line,
                                layout.subtitle_spacing,
                            ))
                    })
                    .max()
                    .unwrap_or(0);

                assert!(title_right <= edge, "{width}×{height} 标题压到海报上");
                assert!(subtitle_right <= edge, "{width}×{height} 副标题压到海报上");
                // 文字块整体不能越出画布底部（副标题折行后块高变大）。
                assert!(
                    i64::from(layout.block_height())
                        + i64::from(layout.block_top(height * TITLE_TOP_RATIO))
                        < height as i64
                );
            }
        }
    }

    #[test]
    fn long_subtitle_wraps_before_shrinking() {
        let subtitle =
            ab_glyph::FontArc::try_from_slice(include_bytes!("../../../fonts/en.otf")).unwrap();
        let base = 60.0;
        let text = "JAPANESE · KOREAN · AMERICAN SERIES";

        // 单行需要 2800px 以上，1600px 只能靠折行消化，字号不该被压小。
        let (size, lines) = fit_subtitle(&subtitle, text, base, 1600, 0.10);

        assert_eq!(size, base);
        assert!(lines.len() <= SUBTITLE_MAX_LINES);
        assert!(
            lines
                .iter()
                .all(|line| spaced_text_width(&subtitle, size, line, size * 0.10) <= 1600)
        );
    }

    #[test]
    fn unbreakable_subtitle_shrinks_to_fit() {
        let subtitle =
            ab_glyph::FontArc::try_from_slice(include_bytes!("../../../fonts/en.otf")).unwrap();

        // 没有空格的副标题无法折行，只能缩字号；CJK 标题同理。
        let (size, lines) = fit_subtitle(&subtitle, "COLLECTION", 80.0, 300, 0.10);

        assert_eq!(lines.len(), 1);
        assert!(size < 80.0);
        assert!(spaced_text_width(&subtitle, size, "COLLECTION", size * 0.10) <= 300);
    }

    #[test]
    fn block_height_follows_subtitle_presence() {
        let fonts = Fonts {
            title: ab_glyph::FontArc::try_from_slice(include_bytes!("../../../fonts/ch.ttf"))
                .unwrap(),
            subtitle: ab_glyph::FontArc::try_from_slice(include_bytes!("../../../fonts/en.otf"))
                .unwrap(),
        };

        let bare = TitleLayout::new("电影", "", &fonts, 1920.0, 1080.0, 640);
        let with_subtitle = TitleLayout::new("电影", "MOVIE", &fonts, 1920.0, 1080.0, 640);

        assert_eq!(bare.block_height(), bare.title_height);
        assert!(bare.subtitle_lines.is_empty());
        assert!(with_subtitle.block_height() > bare.block_height());
    }
}
