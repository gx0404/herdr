//! Graphite 设计语言的唯一真源：状态字形与颜色、转圈帧、文字层级、键帽与相对时间。
//!
//! 规则（详见 `docs/AGENT_RULES/app-render.md` 的「设计语言」）：
//! - 颜色只用于表达状态；结构面用中性色阶（`sidebar_bg` < `panel_bg` < `surface0` <
//!   `surface1`），选中靠底色、粗细与首列 accent 竖条，不靠彩色反色；浮层边框同样走
//!   中性色（[`frame`]），状态色描边只留给吐司。
//! - 状态一律经 [`AgentBucket`] 取字形与颜色，所有界面同一套映射。
//! - 文字四级：标题（粗体 `text`）、正文（`text`）、说明（`subtext0`）、元信息
//!   （`overlay1`）。
//! - 纯函数、无分配（相对时间除外），可在行循环里调用。

use ratatui::style::{Color, Modifier, Style};

use crate::api::schema::AgentActivityStatus;
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

/// 活动树节点状态 → 状态桶：子智能体页面、Inspector 与会话树共用这一个映射，字形与
/// 颜色再经 [`status_glyph`] / [`status_color`] 取，不另建映射。已完成算「待查看」，
/// 等待算空闲，未知不画状态、不参与汇总。
pub fn activity_bucket(status: AgentActivityStatus) -> AgentBucket {
    match status {
        AgentActivityStatus::Running => AgentBucket::Working,
        AgentActivityStatus::Blocked => AgentBucket::NeedsInput,
        AgentActivityStatus::Failed => AgentBucket::Failed,
        AgentActivityStatus::Done => AgentBucket::Review,
        AgentActivityStatus::Pending => AgentBucket::Idle,
        AgentActivityStatus::Unknown => AgentBucket::None,
    }
}

/// 内容强调：转写里的工具名。青色只用来区分「这是一次工具调用」，不表达状态，
/// 也不出现在结构面上（状态仍只用 [`status_color`]）。
pub fn tool_name(palette: &Palette) -> Style {
    Style::default().fg(palette.teal)
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

/// 键帽旁的标签（页脚、提示条、吐司里「键帽 + 说明」的说明）：说明色。
pub fn key_label(palette: &Palette) -> Style {
    muted(palette)
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

/// 选中行整行文字的基础样式：在 [`selected_row`] 上补读得清的正文前景。列表先用它
/// 铺整行，再按需叠状态色 / 说明色；首列另画 [`SELECTION_MARK`]。
pub fn selected_row_text(palette: &Palette) -> Style {
    selected_row(palette).fg(readable_text_on(palette, palette.selection_row_bg()))
}

/// 选中标记：键盘选中行首列的 accent 竖条（侧栏、浮层列表、菜单共用）。悬浮行的
/// 底色与选中行几乎相同但没有这道竖条，选中与悬浮因此始终可辨（MENU-01）。
pub const SELECTION_MARK: &str = "▌";

/// [`SELECTION_MARK`] 的样式：只给 accent 前景，底色沿用所在行。
pub fn selection_mark(palette: &Palette) -> Style {
    Style::default().fg(palette.accent)
}

/// 叠在 `bg` 上读得清的正文色：正文色够 4.5:1 就用正文色，否则按对比度挑
/// （`crate::ui::color::contrast_fg`；`bg` 无从换算时也走它）。
pub fn readable_text_on(palette: &Palette, bg: Color) -> Color {
    if crate::ui::color::contrast_ratio(palette.text, bg).is_some_and(|ratio| ratio >= 4.5) {
        palette.text
    } else {
        crate::ui::color::contrast_fg(palette, bg)
    }
}

/// 浮层、弹窗与菜单的结构边框色：中性色阶，不随主题 accent 变彩色——危险语义放在
/// 正文与按钮上，状态色描边只留给吐司。`surface1` 与面板底分不开（16 色或自定义
/// 主题）时退到 `overlay0`。
pub fn frame(palette: &Palette) -> Color {
    if Palette::row_bg_is_distinct(palette.panel_bg, palette.surface1) {
        palette.surface1
    } else {
        palette.overlay0
    }
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

/// 本地化的「多久以前」：`刚刚` / `3 分钟前` / `2 小时前` / `4 天前`（英文 `just now` /
/// `3m ago` …）。通知历史、通知卡片与片段历史共用；紧凑的列表右侧时间用
/// [`compact_age`]。按分钟取整，不随秒变化。
pub fn ago_label(elapsed_secs: u64) -> String {
    let t = &crate::i18n::texts().history;
    if elapsed_secs < 60 {
        return t.just_now.to_owned();
    }
    let minutes = elapsed_secs / 60;
    if minutes < 60 {
        return crate::i18n::fill(t.minutes_ago_fmt, &[("n", minutes.to_string().as_str())]);
    }
    let hours = minutes / 60;
    if hours < 24 {
        return crate::i18n::fill(t.hours_ago_fmt, &[("n", hours.to_string().as_str())]);
    }
    crate::i18n::fill(t.days_ago_fmt, &[("n", (hours / 24).to_string().as_str())])
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
    fn activity_statuses_map_onto_the_shared_buckets() {
        use AgentActivityStatus::*;
        let mapped = [Running, Blocked, Failed, Done, Pending, Unknown].map(activity_bucket);
        assert_eq!(
            mapped,
            [
                AgentBucket::Working,
                AgentBucket::NeedsInput,
                AgentBucket::Failed,
                AgentBucket::Review,
                AgentBucket::Idle,
                AgentBucket::None,
            ]
        );
    }

    #[test]
    fn selection_and_frame_stay_readable_and_neutral_for_every_theme() {
        for name in crate::config::THEME_NAMES {
            let palette = Palette::from_name(name).expect("built-in theme");
            let selected = selected_row_text(&palette);
            assert_eq!(selected.bg, Some(palette.selection_row_bg()), "{name}");
            assert!(selected.add_modifier.contains(Modifier::BOLD), "{name}");
            let fg = selected.fg.expect("fg");
            if let Some(ratio) = crate::ui::color::contrast_ratio(fg, palette.selection_row_bg()) {
                assert!(ratio >= 4.5, "选中行文字读不清：{name} {ratio:.2}:1");
            }
            let border = frame(&palette);
            assert_ne!(border, palette.panel_bg, "边框与面板底同色：{name}");
            assert_ne!(border, Color::Reset, "边框落回 Reset：{name}");
        }
        let graphite = Palette::graphite_dark();
        assert_eq!(frame(&graphite), graphite.surface1, "Graphite 用中性结构色");
        assert_eq!(selection_mark(&graphite).fg, Some(graphite.accent));
        assert_eq!(selection_mark(&graphite).bg, None, "竖条沿用所在行的底色");
        assert_eq!(crate::ui::display_width(SELECTION_MARK), 1);
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

    #[test]
    fn ago_labels_round_to_minutes_and_localize() {
        let _guard = crate::i18n::lang_guard(crate::i18n::Lang::ZhCn);
        let t = &crate::i18n::texts().history;
        assert_eq!(ago_label(0), t.just_now);
        assert_eq!(ago_label(59), t.just_now);
        assert_eq!(
            ago_label(3 * 60 + 59),
            crate::i18n::fill(t.minutes_ago_fmt, &[("n", "3")])
        );
        assert_eq!(
            ago_label(2 * 3_600),
            crate::i18n::fill(t.hours_ago_fmt, &[("n", "2")])
        );
        assert_eq!(
            ago_label(4 * 86_400),
            crate::i18n::fill(t.days_ago_fmt, &[("n", "4")])
        );
    }
}
