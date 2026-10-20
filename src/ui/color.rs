//! 颜色换算与对比度：把 ratatui 的 `Color` 换成 sRGB，按 WCAG 相对亮度算对比度，
//! 再据此给「叠在某个底色上的前景」挑颜色，或把两色按比例混合。纯函数，不分配。

use ratatui::style::Color;

use crate::app::state::Palette;

pub(crate) type Rgb = (u8, u8, u8);

/// WCAG 相对亮度（0.0 黑 – 1.0 白）。
pub(crate) fn relative_luminance(color: Rgb) -> f32 {
    fn channel(value: u8) -> f32 {
        let value = f32::from(value) / 255.0;
        if value <= 0.03928 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    }
    0.2126 * channel(color.0) + 0.7152 * channel(color.1) + 0.0722 * channel(color.2)
}

/// 把 `Color` 换成 sRGB。`Reset` 是终端默认色，取不到具体值，返回 `None`；
/// ANSI 16 色按 xterm 的惯用值近似；`Indexed` 按 xterm 256 色表换算
/// （0–15 同 ANSI，16–231 是 6×6×6 立方体，232–255 是灰阶）。
pub(crate) fn color_to_rgb(color: Color) -> Option<Rgb> {
    match color {
        Color::Reset => None,
        Color::Black => Some((0, 0, 0)),
        Color::Red => Some((128, 0, 0)),
        Color::Green => Some((0, 128, 0)),
        Color::Yellow => Some((128, 128, 0)),
        Color::Blue => Some((0, 0, 128)),
        Color::Magenta => Some((128, 0, 128)),
        Color::Cyan => Some((0, 128, 128)),
        Color::Gray => Some((192, 192, 192)),
        Color::DarkGray => Some((128, 128, 128)),
        Color::LightRed => Some((255, 0, 0)),
        Color::LightGreen => Some((0, 255, 0)),
        Color::LightYellow => Some((255, 255, 0)),
        Color::LightBlue => Some((0, 0, 255)),
        Color::LightMagenta => Some((255, 0, 255)),
        Color::LightCyan => Some((0, 255, 255)),
        Color::White => Some((255, 255, 255)),
        Color::Rgb(r, g, b) => Some((r, g, b)),
        Color::Indexed(index) => Some(indexed_to_rgb(index)),
    }
}

fn indexed_to_rgb(index: u8) -> Rgb {
    const ANSI: [Rgb; 16] = [
        (0, 0, 0),
        (128, 0, 0),
        (0, 128, 0),
        (128, 128, 0),
        (0, 0, 128),
        (128, 0, 128),
        (0, 128, 128),
        (192, 192, 192),
        (128, 128, 128),
        (255, 0, 0),
        (0, 255, 0),
        (255, 255, 0),
        (0, 0, 255),
        (255, 0, 255),
        (0, 255, 255),
        (255, 255, 255),
    ];
    match index {
        0..=15 => ANSI[usize::from(index)],
        16..=231 => {
            // 立方体每个分量的档位：0, 95, 135, 175, 215, 255。
            let level = |step: u8| if step == 0 { 0 } else { 55 + step * 40 };
            let offset = index - 16;
            (
                level(offset / 36),
                level((offset / 6) % 6),
                level(offset % 6),
            )
        }
        232..=255 => {
            let gray = 8 + (index - 232) * 10;
            (gray, gray, gray)
        }
    }
}

/// 两色按 `toward_b`（0.0 = `a`，1.0 = `b`）在 sRGB 各通道上线性插值并四舍五入；
/// 任一颜色取不到 sRGB（`Reset`）时无从混合，返回 `a`。`toward_b` 钳到 0..=1，
/// NaN 视为 0。
pub(crate) fn mix(a: Color, b: Color, toward_b: f32) -> Color {
    let (Some(from), Some(to)) = (color_to_rgb(a), color_to_rgb(b)) else {
        return a;
    };
    let t = if toward_b.is_nan() {
        0.0
    } else {
        toward_b.clamp(0.0, 1.0)
    };
    let channel = |from: u8, to: u8| {
        let value = f32::from(from) + (f32::from(to) - f32::from(from)) * t;
        value.round().clamp(0.0, 255.0) as u8
    };
    Color::Rgb(
        channel(from.0, to.0),
        channel(from.1, to.1),
        channel(from.2, to.2),
    )
}

/// WCAG 对比度（1.0 – 21.0）；任一颜色取不到 sRGB（`Reset`）时为 `None`。
pub(crate) fn contrast_ratio(a: Color, b: Color) -> Option<f32> {
    let a = relative_luminance(color_to_rgb(a)?);
    let b = relative_luminance(color_to_rgb(b)?);
    let (lighter, darker) = if a >= b { (a, b) } else { (b, a) };
    Some((lighter + 0.05) / (darker + 0.05))
}

/// 叠在 `bg` 上的前景色。候选按序为 `panel_bg`、`text`、黑、白（去掉 `Reset`）：
/// 取第一个对比度 ≥ 4.5 的，保住主题观感；都不够就取对比度最大的。`bg` 取不到
/// sRGB 时无从比较，回退 `palette.text`。
pub(crate) fn contrast_fg(palette: &Palette, bg: Color) -> Color {
    const READABLE: f32 = 4.5;
    if color_to_rgb(bg).is_none() {
        return palette.text;
    }
    let mut best: Option<(Color, f32)> = None;
    for candidate in [palette.panel_bg, palette.text, Color::Black, Color::White] {
        let Some(ratio) = contrast_ratio(candidate, bg) else {
            continue;
        };
        if ratio >= READABLE {
            return candidate;
        }
        if best.is_none_or(|(_, best_ratio)| ratio > best_ratio) {
            best = Some((candidate, ratio));
        }
    }
    best.map_or(palette.text, |(color, _)| color)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexed_colors_follow_the_xterm_256_table() {
        assert_eq!(color_to_rgb(Color::Indexed(0)), Some((0, 0, 0)));
        assert_eq!(color_to_rgb(Color::Indexed(15)), Some((255, 255, 255)));
        assert_eq!(color_to_rgb(Color::Indexed(16)), Some((0, 0, 0)));
        assert_eq!(color_to_rgb(Color::Indexed(21)), Some((0, 0, 255)));
        assert_eq!(color_to_rgb(Color::Indexed(196)), Some((255, 0, 0)));
        assert_eq!(color_to_rgb(Color::Indexed(231)), Some((255, 255, 255)));
        assert_eq!(color_to_rgb(Color::Indexed(232)), Some((8, 8, 8)));
        assert_eq!(color_to_rgb(Color::Indexed(255)), Some((238, 238, 238)));
        assert_eq!(color_to_rgb(Color::Reset), None);
    }

    #[test]
    fn contrast_ratio_matches_the_wcag_extremes() {
        let ratio = contrast_ratio(Color::Black, Color::White).expect("黑白可比较");
        assert!((ratio - 21.0).abs() < 0.01, "{ratio}");
        let same = contrast_ratio(Color::Rgb(40, 40, 40), Color::Rgb(40, 40, 40)).expect("同色");
        assert!((same - 1.0).abs() < 0.001, "{same}");
        assert_eq!(contrast_ratio(Color::Reset, Color::White), None);
    }

    #[test]
    fn mix_interpolates_srgb_channels_and_falls_back_on_reset() {
        let a = Color::Rgb(0, 100, 200);
        let b = Color::Rgb(200, 0, 100);
        assert_eq!(mix(a, b, 0.0), a);
        assert_eq!(mix(a, b, 1.0), b);
        assert_eq!(mix(a, b, 0.5), Color::Rgb(100, 50, 150));
        assert_eq!(mix(a, b, 0.55), Color::Rgb(110, 45, 145), "四舍五入");
        assert_eq!(mix(a, b, 2.0), b, "比例钳到 1");
        assert_eq!(mix(a, b, f32::NAN), a);
        // ANSI 色按 xterm 近似值参与混合。
        assert_eq!(
            mix(Color::Black, Color::White, 0.5),
            Color::Rgb(128, 128, 128)
        );
        // 取不到 sRGB：原样返回 `a`。
        assert_eq!(mix(a, Color::Reset, 0.5), a);
        assert_eq!(mix(Color::Reset, b, 0.5), Color::Reset);
    }

    /// 回归：`panel_bg` 与 `accent` 亮度接近（或降到 16 色后量化成同色）时，旧实现
    /// 直接拿 `panel_bg` 当前景，字叠在 accent 底上就看不见。
    #[test]
    fn contrast_fg_never_returns_a_color_that_vanishes_on_the_background() {
        let mut palette = Palette::catppuccin();
        palette.panel_bg = Color::Blue;
        palette.accent = Color::Blue;
        let fg = contrast_fg(&palette, palette.accent);
        assert_ne!(fg, palette.accent, "前景不能与底色同色");
        assert!(contrast_ratio(fg, palette.accent).expect("可比较") >= 4.5);

        // `panel_bg` 本身够用时保住主题观感。
        let palette = Palette::catppuccin();
        if contrast_ratio(palette.panel_bg, palette.accent).is_some_and(|ratio| ratio >= 4.5) {
            assert_eq!(contrast_fg(&palette, palette.accent), palette.panel_bg);
        }

        // 底色取不到 sRGB：回退正文色。
        assert_eq!(contrast_fg(&palette, Color::Reset), palette.text);
    }
}
