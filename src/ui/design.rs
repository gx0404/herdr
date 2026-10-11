//! Graphite 设计语言的唯一真源：状态字形与颜色、转圈帧、文字层级、键帽与相对时间。
//!
//! 规则（详见 `docs/AGENT_RULES/app-render.md` 的「设计语言」）：
//! - 颜色只用于表达状态；结构面用中性色阶（`sidebar_bg` < `panel_bg` < `surface0` <
//!   `surface1`），选中靠底色与粗细，不靠彩色。
//! - 状态一律经 [`AgentBucket`] 取字形与颜色，所有界面同一套映射。
//! - 文字四级：标题（粗体 `text`）、正文（`text`）、说明（`subtext0`）、元信息
//!   （`overlay1`）。
//! - 纯函数、无分配（相对时间除外），可在行循环里调用。

use ratatui::style::{Color, Modifier, Style};

use crate::app::state::{ComponentStyles, Palette};
use crate::config::StatusIndicatorStyle;
use crate::harness::AgentBucket;

/// 运行中转圈帧（盲文点阵，单列宽）。所有转圈共用全局时钟，同相位。
pub const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// 关闭动画时运行中的静态字形。
pub const WORKING_STATIC: &str = "◌";

/// 状态桶的字形。`frame` 是全局转圈帧序号；`animate = false` 时运行中画静态字形。
pub fn status_glyph(
    bucket: AgentBucket,
    style: StatusIndicatorStyle,
    frame: usize,
    animate: bool,
) -> &'static str {
    match style {
        StatusIndicatorStyle::Graphite => match bucket {
            AgentBucket::NeedsInput => "◆",
            AgentBucket::Failed => "✕",
            AgentBucket::Working if animate => SPINNER_FRAMES[frame % SPINNER_FRAMES.len()],
            AgentBucket::Working => WORKING_STATIC,
            AgentBucket::Review => "●",
            AgentBucket::Idle => "○",
            AgentBucket::None => "·",
        },
        StatusIndicatorStyle::Dots => match bucket {
            AgentBucket::NeedsInput | AgentBucket::Working | AgentBucket::Review => "●",
            AgentBucket::Failed => "✕",
            AgentBucket::Idle => "○",
            AgentBucket::None => "·",
        },
        StatusIndicatorStyle::Symbols => match bucket {
            AgentBucket::NeedsInput => "×",
            AgentBucket::Failed => "✕",
            AgentBucket::Working => "◐",
            AgentBucket::Review => "✓",
            AgentBucket::Idle => "○",
            AgentBucket::None => "·",
        },
    }
}

/// 状态桶的颜色（来自 `ComponentStyles` 的状态 token，主题可覆盖）。
pub fn status_color(bucket: AgentBucket, palette: &Palette, components: &ComponentStyles) -> Color {
    match bucket {
        AgentBucket::NeedsInput => components.status_input,
        AgentBucket::Failed => components.status_failed,
        AgentBucket::Working => components.status_running,
        AgentBucket::Review => components.status_review,
        AgentBucket::Idle => components.status_idle,
        AgentBucket::None => palette.overlay0,
    }
}

/// 状态字形的完整样式：需要输入与失败加粗，空闲与无状态不加粗。
pub fn status_style(bucket: AgentBucket, palette: &Palette, components: &ComponentStyles) -> Style {
    let style = Style::default().fg(status_color(bucket, palette, components));
    match bucket {
        AgentBucket::NeedsInput | AgentBucket::Failed => style.add_modifier(Modifier::BOLD),
        _ => style,
    }
}

/// 文字层级：标题。
pub fn title(palette: &Palette) -> Style {
    Style::default()
        .fg(palette.text)
        .add_modifier(Modifier::BOLD)
}

/// 文字层级：正文。
pub fn body(palette: &Palette) -> Style {
    Style::default().fg(palette.text)
}

/// 文字层级：说明（次要信息、摘要）。
pub fn muted(palette: &Palette) -> Style {
    Style::default().fg(palette.subtext0)
}

/// 文字层级：元信息（时间、计数、分支、快捷键说明）。
pub fn meta(palette: &Palette) -> Style {
    Style::default().fg(palette.overlay1)
}

/// 分区标题（侧栏「工作区」、Inspector 小节名）：元信息色加粗。
pub fn section(palette: &Palette) -> Style {
    Style::default()
        .fg(palette.overlay1)
        .add_modifier(Modifier::BOLD)
}

/// 细分隔线。
pub fn rule(palette: &Palette) -> Style {
    Style::default().fg(palette.surface1)
}

/// 键帽：浅一档的底色上的正文色，左右各留一格（调用方负责在文字两侧补空格）。
pub fn key_cap(palette: &Palette) -> Style {
    Style::default().fg(palette.text).bg(palette.surface1)
}

/// 主要键帽（每个提示条最多一个）：反色。
pub fn key_cap_primary(palette: &Palette) -> Style {
    Style::default()
        .fg(crate::ui::color::contrast_fg(palette, palette.accent))
        .bg(palette.accent)
        .add_modifier(Modifier::BOLD)
}

/// 选中行（键盘光标）：浅一档的底色加粗，不用彩色。
pub fn selected_row(palette: &Palette) -> Style {
    Style::default()
        .bg(palette.selection_row_bg())
        .add_modifier(Modifier::BOLD)
}

/// 紧凑相对时间：`now` / `12s` / `3m` / `2h` / `3d` / `5w`。
pub fn compact_age(elapsed_ms: u64) -> String {
    let seconds = elapsed_ms / 1_000;
    match seconds {
        0..=4 => "now".to_owned(),
        5..=59 => format!("{seconds}s"),
        60..=3_599 => format!("{}m", seconds / 60),
        3_600..=86_399 => format!("{}h", seconds / 3_600),
        86_400..=604_799 => format!("{}d", seconds / 86_400),
        _ => format!("{}w", seconds / 604_800),
    }
}

/// 计时（运行时长）：`42s` / `3m 05s` / `1h 12m`。
pub fn duration_label(elapsed_ms: u64) -> String {
    let seconds = elapsed_ms / 1_000;
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3_600 {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h {:02}m", seconds / 3_600, (seconds % 3_600) / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graphite_glyphs_are_single_width_and_distinct() {
        let glyphs = AgentBucket::LISTING
            .map(|bucket| status_glyph(bucket, StatusIndicatorStyle::Graphite, 0, false));
        for glyph in glyphs {
            assert_eq!(crate::ui::display_width(glyph), 1, "{glyph}");
        }
        let unique = glyphs.iter().collect::<std::collections::HashSet<_>>();
        assert_eq!(unique.len(), glyphs.len());
        for frame in SPINNER_FRAMES {
            assert_eq!(crate::ui::display_width(frame), 1, "{frame}");
        }
    }

    #[test]
    fn working_spinner_follows_the_global_frame() {
        assert_eq!(
            status_glyph(
                AgentBucket::Working,
                StatusIndicatorStyle::Graphite,
                3,
                true
            ),
            SPINNER_FRAMES[3]
        );
        assert_eq!(
            status_glyph(
                AgentBucket::Working,
                StatusIndicatorStyle::Graphite,
                13,
                true
            ),
            SPINNER_FRAMES[3]
        );
        assert_eq!(
            status_glyph(
                AgentBucket::Working,
                StatusIndicatorStyle::Graphite,
                3,
                false
            ),
            WORKING_STATIC
        );
    }

    #[test]
    fn ages_and_durations_are_compact() {
        assert_eq!(compact_age(1_000), "now");
        assert_eq!(compact_age(42_000), "42s");
        assert_eq!(compact_age(3 * 60_000), "3m");
        assert_eq!(compact_age(2 * 3_600_000), "2h");
        assert_eq!(compact_age(3 * 86_400_000), "3d");
        assert_eq!(compact_age(15 * 86_400_000), "2w");
        assert_eq!(duration_label(42_000), "42s");
        assert_eq!(duration_label(185_000), "3m 05s");
        assert_eq!(duration_label(4_320_000), "1h 12m");
    }
}
