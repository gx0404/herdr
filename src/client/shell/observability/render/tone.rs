//! 系统页的分类色：每类数据（CPU、内存、GPU、磁盘、网络收 / 发、温度）一种颜色，
//! 只用来区分数据类型；严重程度仍由黄 / 红表达（`Severity`），过期数据仍转灰。
//! 数据元素（曲线、速率、主读数、条形常态色）一律经 [`Tone::data`] /
//! [`Tone::data_soft`] 取色，过期时退回改动前的中性灰阶（[`Neutral`]）；卡片标题是
//! 数据类型标识，过期时仍用 [`Tone::color`]。
//!
//! 按面板底色 `panel_bg` 选组，不看主题名（用户可以自定义 `panel_bg`）：取不到 sRGB
//! （终端 16 色主题，`panel_bg` 为 `Reset`）用 ANSI 组，相对亮度 < 0.4 用深色组，
//! 否则用浅色组。深色 / 浅色组在所有 RGB 内置主题的面板底上对比度都不低于 3:1，
//! 且与各主题的黄 / 红不同色（见单测）。纯函数，不分配。

use ratatui::style::Color;

use crate::app::state::Palette;
use crate::ui::color::{color_to_rgb, mix, relative_luminance};

/// 一类数据的分类色。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Tone {
    Cpu,
    Memory,
    Gpu,
    Disk,
    NetRx,
    NetTx,
    Temperature,
}

/// 面板底色决定的色组。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Group {
    Dark,
    Light,
    Ansi,
}

/// 深色组面板底的相对亮度上界（不含）。
const DARK_LUMINANCE: f32 = 0.4;

/// 曲线填充从分类色朝底色混合的比例。
const SOFT_MIX: f32 = 0.55;

/// 内存缓存段从分类色朝底色混合的比例：比曲线填充再淡一档，与紧挨着的已用段
/// （主色）拉开差距。
const FAINT_MIX: f32 = 0.68;

/// 过期数据退回的中性灰阶档位，即改用分类色之前的配色：`Line` 是曲线描边与网络
/// 「收」曲线（说明色），`Fill` 是曲线下方的填充与网络「发」曲线（`overlay0`），
/// `Text` 是速率文字（正文色），`Reading` 是主读数（与过期条形同一档 `overlay0`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Neutral {
    Line,
    Fill,
    Text,
    Reading,
}

impl Neutral {
    pub(super) fn color(self, palette: &Palette) -> Color {
        match self {
            Self::Line => palette.subtext0,
            Self::Fill | Self::Reading => palette.overlay0,
            Self::Text => palette.text,
        }
    }
}

fn group(palette: &Palette) -> Group {
    match color_to_rgb(palette.panel_bg) {
        None => Group::Ansi,
        Some(rgb) if relative_luminance(rgb) < DARK_LUMINANCE => Group::Dark,
        Some(_) => Group::Light,
    }
}

impl Tone {
    #[cfg(test)]
    pub(super) const ALL: [Self; 7] = [
        Self::Cpu,
        Self::Memory,
        Self::Gpu,
        Self::Disk,
        Self::NetRx,
        Self::NetTx,
        Self::Temperature,
    ];

    /// 主色：卡片标题、常态条形、曲线描边、常态主读数。
    pub(super) fn color(self, palette: &Palette) -> Color {
        match (group(palette), self) {
            (Group::Dark, Self::Cpu) => Color::Rgb(0x60, 0xA5, 0xFA),
            (Group::Dark, Self::Memory) => Color::Rgb(0xA7, 0x8B, 0xFA),
            (Group::Dark, Self::Gpu) => Color::Rgb(0x4A, 0xDE, 0x80),
            (Group::Dark, Self::Disk) => Color::Rgb(0x2D, 0xD4, 0xBF),
            (Group::Dark, Self::NetRx) => Color::Rgb(0x22, 0xD3, 0xEE),
            (Group::Dark, Self::NetTx) => Color::Rgb(0xE8, 0x79, 0xF9),
            (Group::Dark, Self::Temperature) => Color::Rgb(0x7D, 0xD3, 0xFC),
            (Group::Light, Self::Cpu) => Color::Rgb(0x25, 0x63, 0xEB),
            (Group::Light, Self::Memory) => Color::Rgb(0x7C, 0x3A, 0xED),
            (Group::Light, Self::Gpu) => Color::Rgb(0x15, 0x80, 0x3D),
            (Group::Light, Self::Disk) => Color::Rgb(0x0F, 0x76, 0x6E),
            (Group::Light, Self::NetRx) => Color::Rgb(0x0E, 0x74, 0x90),
            (Group::Light, Self::NetTx) => Color::Rgb(0xC0, 0x26, 0xD3),
            (Group::Light, Self::Temperature) => Color::Rgb(0x03, 0x69, 0xA1),
            (Group::Ansi, Self::Cpu) => Color::Blue,
            (Group::Ansi, Self::Memory) => Color::Magenta,
            (Group::Ansi, Self::Gpu) => Color::Green,
            (Group::Ansi, Self::Disk) => Color::Cyan,
            (Group::Ansi, Self::NetRx) => Color::LightCyan,
            (Group::Ansi, Self::NetTx) => Color::LightMagenta,
            (Group::Ansi, Self::Temperature) => Color::LightBlue,
        }
    }

    /// 柔和色：曲线下方的填充。RGB 主题是主色朝面板底混合 `SOFT_MIX`；终端 16 色
    /// 主题无从混合，用 `overlay0`。
    pub(super) fn soft(self, palette: &Palette) -> Color {
        self.toward_panel(SOFT_MIX, palette)
    }

    /// 更淡一档：内存缓存段，紧挨在主色的已用段后面。RGB 主题朝面板底混合
    /// `FAINT_MIX`；终端 16 色主题同 [`Tone::soft`]。
    pub(super) fn faint(self, palette: &Palette) -> Color {
        self.toward_panel(FAINT_MIX, palette)
    }

    fn toward_panel(self, amount: f32, palette: &Palette) -> Color {
        match group(palette) {
            Group::Ansi => palette.overlay0,
            Group::Dark | Group::Light => mix(self.color(palette), palette.panel_bg, amount),
        }
    }

    /// 数据元素主色的唯一入口：常态取分类主色，`stale`（数据过期）时退回 `neutral`
    /// 灰阶——过期数据不画确定的分类色。
    pub(super) fn data(self, stale: bool, neutral: Neutral, palette: &Palette) -> Color {
        if stale {
            neutral.color(palette)
        } else {
            self.color(palette)
        }
    }

    /// 同 [`Tone::data`]，常态取柔和色（曲线填充）。
    pub(super) fn data_soft(self, stale: bool, neutral: Neutral, palette: &Palette) -> Color {
        if stale {
            neutral.color(palette)
        } else {
            self.soft(palette)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::color::contrast_ratio;

    /// 每个内置主题：7 个分类色在面板底上对比度 ≥ 3:1（终端默认底色无从比较，
    /// 改为断言用 ANSI 组）、两两不同、不与该主题的黄 / 红撞色。
    #[test]
    fn tones_are_distinct_readable_and_never_alias_severity_colors() {
        for name in crate::config::THEME_NAMES {
            let palette = Palette::from_name(name).expect("内置主题");
            let colors = Tone::ALL.map(|tone| tone.color(&palette));
            for (index, color) in colors.iter().enumerate() {
                let tone = Tone::ALL[index];
                match contrast_ratio(*color, palette.panel_bg) {
                    Some(ratio) => assert!(ratio >= 3.0, "{name} {tone:?}: 对比度 {ratio:.2}:1"),
                    None => {
                        assert_eq!(group(&palette), Group::Ansi, "{name}");
                        assert!(
                            !matches!(color, Color::Rgb(..)),
                            "{name} {tone:?}: 终端底色用 ANSI 组"
                        );
                        assert_eq!(tone.soft(&palette), palette.overlay0, "{name}");
                    }
                }
                assert_ne!(*color, palette.yellow, "{name} {tone:?}: 与警告色撞色");
                assert_ne!(*color, palette.red, "{name} {tone:?}: 与严重色撞色");
                for other in &colors[index + 1..] {
                    assert_ne!(color, other, "{name} {tone:?}: 分类色重复");
                }
            }
        }
    }

    /// 选组只看 `panel_bg` 的亮度：同一主题换成浅底就用浅色组；柔和色介于主色与
    /// 底色之间。
    #[test]
    fn group_follows_the_panel_background_and_soft_mixes_toward_it() {
        let mut palette = Palette::catppuccin();
        palette.panel_bg = Color::Rgb(0x1E, 0x1E, 0x2E);
        assert_eq!(Tone::Cpu.color(&palette), Color::Rgb(0x60, 0xA5, 0xFA));
        assert_eq!(
            Tone::Cpu.soft(&palette),
            mix(Color::Rgb(0x60, 0xA5, 0xFA), palette.panel_bg, SOFT_MIX)
        );
        palette.panel_bg = Color::Rgb(0xFA, 0xFA, 0xFA);
        assert_eq!(Tone::Cpu.color(&palette), Color::Rgb(0x25, 0x63, 0xEB));
        palette.panel_bg = Color::Reset;
        assert_eq!(Tone::Cpu.color(&palette), Color::Blue);
        assert_eq!(Tone::Cpu.soft(&palette), palette.overlay0);
        assert_eq!(Tone::Cpu.faint(&palette), palette.overlay0);
    }

    /// 缓存段比曲线填充更淡，但每个 RGB 内置主题上仍与面板底分得开、与主色不同。
    #[test]
    fn faint_sits_between_the_tone_and_the_panel() {
        for name in crate::config::THEME_NAMES {
            let palette = Palette::from_name(name).expect("内置主题");
            if group(&palette) == Group::Ansi {
                continue;
            }
            for tone in Tone::ALL {
                let faint = tone.faint(&palette);
                assert_ne!(faint, tone.soft(&palette), "{name} {tone:?}");
                assert_ne!(faint, tone.color(&palette), "{name} {tone:?}");
                let ratio = contrast_ratio(faint, palette.panel_bg).expect("RGB 主题");
                assert!(ratio >= 1.25, "{name} {tone:?}: 缓存段对比度 {ratio:.2}:1");
            }
        }
    }

    /// 过期数据退回改动前的中性灰阶，常态取分类色。
    #[test]
    fn data_colors_fall_back_to_the_neutral_greys_when_stale() {
        let palette = Palette::catppuccin();
        assert_eq!(
            Tone::Cpu.data(false, Neutral::Line, &palette),
            Tone::Cpu.color(&palette)
        );
        assert_eq!(
            Tone::Cpu.data_soft(false, Neutral::Fill, &palette),
            Tone::Cpu.soft(&palette)
        );
        assert_eq!(
            Tone::Cpu.data(true, Neutral::Line, &palette),
            palette.subtext0
        );
        assert_eq!(
            Tone::Cpu.data_soft(true, Neutral::Fill, &palette),
            palette.overlay0
        );
        assert_eq!(
            Tone::NetRx.data(true, Neutral::Text, &palette),
            palette.text
        );
        assert_eq!(
            Tone::Gpu.data(true, Neutral::Reading, &palette),
            palette.overlay0
        );
    }
}
