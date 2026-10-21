//! 系统页的分类色：CPU、内存、GPU、磁盘、网络收 / 发、温度复用有限的分类色，
//! 只用来区分数据类型；严重程度仍由黄 / 红表达（`Severity`），过期数据仍转灰。
//! 分类色仅用于曲线与条形，取主题的紫 / 青 / 桃及中性色，避开状态 token。
//! 标题和正常读数保持中性；阈值警告仍用黄 / 红，过期数据仍转灰。
//!
//! 分类色来自主题，若与状态色重合或不足 3:1 则退回中性色。曲线填充根据面板底色
//! 混合；终端默认底色无法混合时用 overlay0。纯函数，不分配。

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

    /// 分类主色仅用于图表；旧主题若将分类色映射到状态色，则退回中性色。
    pub(super) fn color(self, palette: &Palette) -> Color {
        let preferred = match self {
            Self::Cpu | Self::NetTx => palette.mauve,
            Self::Memory | Self::NetRx => palette.teal,
            Self::Gpu | Self::Temperature => palette.peach,
            Self::Disk => palette.subtext0,
        };
        [preferred, palette.subtext0, palette.overlay1, palette.text]
            .into_iter()
            .find(|color| {
                ![palette.blue, palette.green, palette.yellow, palette.red].contains(color)
                    && crate::ui::color::contrast_ratio(*color, palette.panel_bg)
                        .is_none_or(|ratio| ratio >= 3.0)
            })
            .unwrap_or(palette.text)
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
        } else if matches!(neutral, Neutral::Text | Neutral::Reading) {
            palette.text
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

    /// 所有内置主题的图表至少 3:1；分类色允许复用，但不能冒充状态。
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
                assert_ne!(*color, palette.blue, "{name} {tone:?}: 与运行色撞色");
                assert_ne!(*color, palette.green, "{name} {tone:?}: 与待查看色撞色");
            }
        }
    }

    /// 柔和色介于主题主色与面板底之间；终端默认底色用中性填充。
    #[test]
    fn group_follows_the_panel_background_and_soft_mixes_toward_it() {
        let mut palette = Palette::catppuccin();
        palette.panel_bg = Color::Rgb(0x1E, 0x1E, 0x2E);
        assert_eq!(Tone::Cpu.color(&palette), palette.mauve);
        assert_eq!(
            Tone::Cpu.soft(&palette),
            mix(Tone::Cpu.color(&palette), palette.panel_bg, SOFT_MIX)
        );
        palette.panel_bg = Color::Rgb(0xFA, 0xFA, 0xFA);
        assert_eq!(group(&palette), Group::Light);
        palette.panel_bg = Color::Reset;
        assert_eq!(Tone::Cpu.color(&palette), palette.mauve);
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
