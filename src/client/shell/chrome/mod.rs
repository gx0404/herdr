//! 外壳 chrome（Graphite 设计语言）：标题栏、状态栏与面板头的版式与绘制。
//!
//! 约定：版式是纯函数（只量宽度、不碰缓冲区），可单测；绘制只读入参、按值画，
//! 命中区作为返回值交给调用方登记。上游文件（`workbench/render.rs`、`render.rs`）
//! 只留薄调用点，fork 自有的视觉代码集中在这里。

pub(super) mod panel_header;
pub(super) mod status_bar;
pub(super) mod title_bar;

use std::borrow::Cow;

use ratatui::buffer::Buffer;
use ratatui::style::{Color, Style};
use unicode_segmentation::UnicodeSegmentation;

use crate::app::state::Palette;
use crate::config::{ActionKeybinds, LiveKeybindConfig};
use crate::ui::display_width_u16;

/// 标题栏与状态栏的底色：侧栏面——窗口 chrome 用最深的结构色阶，与桌面端一致。
/// 主题把侧栏留成终端底色（`Reset`）时退到面板底色。
pub(super) fn bar_surface(palette: &Palette) -> Color {
    if palette.sidebar_bg == Color::Reset {
        palette.panel_bg
    } else {
        palette.sidebar_bg
    }
}

/// 叠在 `bg` 上、读得清的文字色：`candidates` 按由弱到强排，取第一个对比度
/// ≥ 4.5:1 的；都不够取对比度最大的。`bg` 换算不出 sRGB（`Reset`、16 色）时拿
/// 面板底色近似，仍换算不出就用第一个候选。纯函数、不分配。
pub(super) fn readable_on(palette: &Palette, bg: Color, candidates: &[Color]) -> Color {
    const READABLE: f32 = 4.5;
    let first = candidates.first().copied().unwrap_or(palette.text);
    let bg = if crate::ui::color::color_to_rgb(bg).is_some() {
        bg
    } else {
        palette.panel_bg
    };
    let mut best: Option<(Color, f32)> = None;
    for &candidate in candidates {
        let Some(ratio) = crate::ui::color::contrast_ratio(candidate, bg) else {
            continue;
        };
        if ratio >= READABLE {
            return candidate;
        }
        if best.is_none_or(|(_, best_ratio)| ratio > best_ratio) {
            best = Some((candidate, ratio));
        }
    }
    best.map_or(first, |(color, _)| color)
}

/// 栏内键帽（标题栏的命令提示、状态栏的键位提示）：比页面页脚的键帽低一档——说明色
/// 字叠在最浅一档的底上，常驻在屏幕边缘也不抢眼。`surface0` 与栏面分不开时退到
/// `surface1`，字色按对比度挑。
pub(super) fn bar_key_cap(palette: &Palette, bar: Color) -> Style {
    let cap = if Palette::row_bg_is_distinct(bar, palette.surface0) {
        palette.surface0
    } else {
        palette.surface1
    };
    Style::default()
        .fg(readable_on(palette, cap, &[palette.subtext0, palette.text]))
        .bg(cap)
}

/// 栏内键位提示的标签：元信息色，栏面上读不清时退到说明色。
pub(super) fn bar_hint_label(palette: &Palette, bar: Color) -> Style {
    Style::default().fg(readable_on(
        palette,
        bar,
        &[palette.overlay1, palette.subtext0, palette.text],
    ))
}

/// 一段带样式的文字。
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Span<'a> {
    pub(super) text: Cow<'a, str>,
    pub(super) style: Style,
}

impl<'a> Span<'a> {
    pub(super) fn new(text: impl Into<Cow<'a, str>>, style: Style) -> Self {
        Self {
            text: text.into(),
            style,
        }
    }

    pub(super) fn width(&self) -> u16 {
        display_width_u16(&self.text)
    }
}

pub(super) fn spans_width(spans: &[Span<'_>]) -> u16 {
    spans
        .iter()
        .fold(0u16, |width, span| width.saturating_add(span.width()))
}

/// 从 `x` 起依次画 `spans`，不越过 `right`（不含）；宽字符放不下时停在它前面。
/// 返回画到的列。
pub(super) fn paint_spans(
    buffer: &mut Buffer,
    x: u16,
    y: u16,
    right: u16,
    spans: &[Span<'_>],
) -> u16 {
    let mut x = x;
    for span in spans {
        if x >= right || y >= buffer.area.bottom() || x >= buffer.area.right() {
            break;
        }
        let (end, _) =
            buffer.set_stringn(x, y, span.text.as_ref(), usize::from(right - x), span.style);
        x = end;
    }
    x
}

/// 把 `spans` 收进 `width` 列：放得下原样返回；放不下从左截断、保留尾部，最前面
/// 补一个 `…`（`ellipsis` 样式）。按字素截，宽字符不劈开；`width` 为 0 时为空。
pub(super) fn truncate_spans_left<'a>(
    spans: &[Span<'a>],
    width: u16,
    ellipsis: Style,
) -> Vec<Span<'a>> {
    if spans_width(spans) <= width {
        return spans.to_vec();
    }
    if width == 0 {
        return Vec::new();
    }
    let mut budget = width - 1;
    let mut tail = Vec::new();
    for span in spans.iter().rev() {
        if budget == 0 {
            break;
        }
        let span_width = span.width();
        if span_width <= budget {
            budget -= span_width;
            tail.push(span.clone());
            continue;
        }
        // 这一段只放得下尾部：从右往左按字素收，直到再收一个就超出。
        let mut start = span.text.len();
        let mut used = 0u16;
        for (index, grapheme) in span.text.grapheme_indices(true).rev() {
            let grapheme_width = display_width_u16(grapheme);
            if used.saturating_add(grapheme_width) > budget {
                break;
            }
            used += grapheme_width;
            start = index;
        }
        if start < span.text.len() {
            tail.push(Span::new(span.text[start..].to_owned(), span.style));
        }
        break;
    }
    tail.push(Span::new("…", ellipsis));
    tail.reverse();
    tail
}

/// 一个动作键位的完整按键序列（键帽上的文字）：前缀键位写成「ctrl+b /」，直接
/// 键位原样；配了多个时取第一个。没配就是 `None`。
pub(super) fn key_sequence(
    keybinds: &LiveKeybindConfig,
    action: &ActionKeybinds,
) -> Option<String> {
    let binding = action.bindings.first()?;
    if binding.trigger.is_prefix() {
        let key = binding
            .label
            .strip_prefix("prefix+")
            .unwrap_or(&binding.label);
        Some(format!("{} {key}", keybinds.primary_prefix_label()))
    } else {
        Some(binding.label.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Modifier;

    fn text(spans: &[Span<'_>]) -> String {
        spans.iter().map(|span| span.text.as_ref()).collect()
    }

    #[test]
    fn left_truncation_keeps_the_tail_and_marks_the_cut() {
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let spans = [
            Span::new("Local", Style::default()),
            Span::new(" / ", Style::default()),
            Span::new("herdr", Style::default()),
            Span::new(" · ", Style::default()),
            Span::new("claude", bold),
        ];
        assert_eq!(
            text(&truncate_spans_left(&spans, 22, bold)),
            "Local / herdr · claude"
        );
        let cut = truncate_spans_left(&spans, 12, Style::default());
        assert_eq!(text(&cut), "…dr · claude");
        assert_eq!(spans_width(&cut), 12);
        assert_eq!(
            cut.last().map(|span| span.style),
            Some(bold),
            "尾段样式保留"
        );
        assert!(truncate_spans_left(&spans, 0, Style::default()).is_empty());
        assert_eq!(text(&truncate_spans_left(&spans, 1, Style::default())), "…");
    }

    #[test]
    fn left_truncation_never_splits_a_wide_character() {
        let spans = [Span::new("工作区 / 修复焦点", Style::default())];
        let cut = truncate_spans_left(&spans, 6, Style::default());
        // 预算 5 列：「焦点」占 4 列，再收「复」就是 6 列。
        assert_eq!(text(&cut), "…焦点");
        assert!(spans_width(&cut) <= 6);
    }

    #[test]
    fn readable_text_prefers_the_weakest_candidate_that_passes() {
        let palette = Palette::graphite_dark();
        let bg = palette.sidebar_bg;
        assert_eq!(
            readable_on(&palette, bg, &[palette.subtext0, palette.text]),
            palette.subtext0
        );
        // overlay0 叠在侧栏面上不足 4.5:1，退到更强的候选。
        assert_ne!(
            readable_on(&palette, bg, &[palette.overlay0, palette.subtext0]),
            palette.overlay0
        );
    }

    #[test]
    fn key_sequences_spell_out_the_prefix() {
        let keybinds = crate::config::Config::default()
            .live_keybinds_with_diagnostics()
            .map(|(keybinds, _)| keybinds)
            .expect("default keybinds");
        assert_eq!(
            key_sequence(&keybinds, &keybinds.keybinds.command_search).as_deref(),
            Some("ctrl+b /")
        );
        assert_eq!(
            key_sequence(&keybinds, &ActionKeybinds::direct("ctrl+k")).as_deref(),
            Some("ctrl+k")
        );
        assert_eq!(key_sequence(&keybinds, &ActionKeybinds::default()), None);
    }
}
