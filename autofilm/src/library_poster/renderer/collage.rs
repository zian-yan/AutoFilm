use ab_glyph::{Font, FontArc, PxScale, ScaleFont};
use image::{DynamicImage, Rgba, RgbaImage, imageops::overlay};
use imageproc::geometric_transformations::{Border, Interpolation, rotate_about_center_no_crop};

use super::{
    Fonts, Result,
    utils::{
        adjust_brightness, apply_rounded_corners, cover, dominant_color, draw_text_left,
        optimized_blur, soft_shadow, spaced_text_width, text_line_height, tint,
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
/// 标题左边距与副标题色标的位置比例。
const TITLE_LEFT_RATIO: f32 = 176.0 / 1920.0;
const ACCENT_LEFT_RATIO: f32 = 132.0 / 1920.0;
const ACCENT_WIDTH_RATIO: f32 = 14.0 / 1920.0;
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
        width_f,
        height_f,
        theme,
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
    width_f: f32,
    height_f: f32,
    theme: Rgba<u8>,
) {
    let max_width = (width_f * 0.34) as i32;
    // 按字形墨迹高度对齐参考模板（标题约占画布高度 17.5%），不受字体 em 差异影响。
    let title_size = fit_font_size(
        &fonts.title,
        title,
        ink_fitted_size(&fonts.title, title, height_f * 0.175),
        max_width,
        0.0,
    );
    // 副标题宽度跟随标题，避免宽字面字体把字幕拉得比标题还长。
    let title_width = spaced_text_width(&fonts.title, title_size, title, 0.0) as f32;
    let subtitle_limit = (title_width * 1.05).min(width_f * 0.32).max(1.0) as i32;
    let subtitle_size = fit_font_size(
        &fonts.subtitle,
        subtitle,
        height_f * 0.062,
        subtitle_limit,
        0.10,
    );
    let subtitle_spacing = subtitle_size * 0.10;
    let title_height = if title.is_empty() {
        0
    } else {
        text_line_height(&fonts.title, title_size)
    };
    let subtitle_height = if subtitle.is_empty() {
        0
    } else {
        text_line_height(&fonts.subtitle, subtitle_size)
    };
    let block_gap = if title.is_empty() || subtitle.is_empty() {
        0
    } else {
        (title_size * 0.10) as i32
    };
    let block_height = title_height + block_gap + subtitle_height;
    let mut y = (height_f * 0.52) as i32 - block_height / 2;

    let left = (width_f * TITLE_LEFT_RATIO) as i32;
    draw_text_left(
        canvas,
        title,
        &fonts.title,
        left,
        y,
        title_size,
        Rgba([255, 255, 255, 255]),
        0.0,
    );
    y += title_height + block_gap;

    if !subtitle.is_empty() {
        // 色标按字号取固定倍数，与参考模板的 88×14 / 80px 字幕比例一致，
        // 不受字体自身 ascent/descent 差异影响。
        let accent_width = (width_f * ACCENT_WIDTH_RATIO).round().max(2.0) as u32;
        let accent_height = (subtitle_size * 1.10).round() as u32;
        let accent_left = (width_f * ACCENT_LEFT_RATIO) as i64;
        let accent_top = i64::from(y) + (subtitle_size * 0.10) as i64;
        let accent = RgbaImage::from_pixel(accent_width, accent_height, accent_color(theme));
        overlay(canvas, &accent, accent_left, accent_top);
        draw_text_left(
            canvas,
            subtitle,
            &fonts.subtitle,
            left,
            y,
            subtitle_size,
            Rgba([255, 255, 255, 235]),
            subtitle_spacing,
        );
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

/// 由主题色推导高饱和强调色，保证在压暗背景上足够醒目。
fn accent_color(theme: Rgba<u8>) -> Rgba<u8> {
    let [red, green, blue] =
        [theme[0], theme[1], theme[2]].map(|channel| f32::from(channel) / 255.0);
    let maximum = red.max(green).max(blue);
    let minimum = red.min(green).min(blue);
    let delta = maximum - minimum;
    let hue = if delta <= f32::EPSILON {
        0.0
    } else if maximum == red {
        60.0 * ((green - blue) / delta).rem_euclid(6.0)
    } else if maximum == green {
        60.0 * ((blue - red) / delta + 2.0)
    } else {
        60.0 * ((red - green) / delta + 4.0)
    };
    // 只保留色相，饱和度与明度抬到醒目区间，避免灰调素材产生脏色。
    hsv_to_rgb(hue, 0.78, 0.96)
}

fn hsv_to_rgb(hue: f32, saturation: f32, value: f32) -> Rgba<u8> {
    let channel = |offset: f32| {
        let sector = (offset + hue / 60.0).rem_euclid(6.0);
        let factor = sector.min(4.0 - sector).clamp(0.0, 1.0);
        ((value - value * saturation * factor) * 255.0).round() as u8
    };
    Rgba([channel(5.0), channel(3.0), channel(1.0), 255])
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
    fn accent_color_brightens_dull_theme() {
        let accent = accent_color(Rgba([30, 60, 90, 255]));

        assert!(accent[2] > 90);
        assert!(accent[0] > 30);
    }

    #[test]
    fn accent_color_keeps_hue_but_raises_saturation() {
        // 暖褐色主题应得到醒目的橙色色标，而不是灰白。
        let accent = accent_color(Rgba([120, 60, 40, 255]));

        assert!(accent[0] >= 220);
        assert!(accent[1] < accent[0]);
        assert!(accent[2] < accent[1]);
    }
}
