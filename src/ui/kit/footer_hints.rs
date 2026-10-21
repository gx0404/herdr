//! 页脚键位提示：` key ` 键帽 + 标签，一行或多行排开、可点。单行放不下时从尾部
//! 丢弃，`primary` 项最后才丢；多行时整项换行，行用完后余下整项丢弃。键帽的宽度与
//! 绘制取 [`super::key_cap`]（按键文字统一规范成小写口径），页面页脚的配色取
//! `ui::design`；状态栏这类常驻的栏用 [`render_footer_hints_styled`] 换配色与间隔。

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
};

use super::key_cap::{self, HINT_GAP};
use super::{fill_row, put_str};
use crate::app::state::Palette;

/// 一条提示。`primary` = 主动作（键帽 accent 反色、最后丢弃）；`enabled` 为假
/// 整条灰。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FooterHint<'a> {
    pub key: &'a str,
    pub label: &'a str,
    pub enabled: bool,
    pub primary: bool,
}

impl<'a> FooterHint<'a> {
    /// 普通的可用提示（非主动作）。
    pub(crate) const fn new(key: &'a str, label: &'a str) -> Self {
        Self {
            key,
            label,
            enabled: true,
            primary: false,
        }
    }
}

/// 最多参与排布的项数（位掩码上限；再多的项直接不画）。
const MAX_HINTS: usize = 64;

/// 行用完、还有提示没画出来时的截断标记。
const OVERFLOW: &str = "…";

fn hint_width(hint: &FooterHint<'_>) -> u16 {
    key_cap::hint_width(hint.key, hint.label)
}

fn total_width(hints: &[FooterHint<'_>], shown: u64, gap: u16) -> u16 {
    let mut total = 0u16;
    let mut count = 0u16;
    for (index, hint) in hints.iter().enumerate().take(MAX_HINTS) {
        if shown & (1u64 << index) == 0 {
            continue;
        }
        total = total.saturating_add(hint_width(hint));
        count += 1;
    }
    total.saturating_add(gap.saturating_mul(count.saturating_sub(1)))
}

/// 决定哪些项画出来：先丢最靠后的非 primary，都丢完了再从后往前丢 primary。
fn shown_mask(hints: &[FooterHint<'_>], width: u16, gap: u16) -> u64 {
    let count = hints.len().min(MAX_HINTS);
    let mut shown = if count >= 64 {
        u64::MAX
    } else {
        (1u64 << count) - 1
    };
    while shown != 0 && total_width(hints, shown, gap) > width {
        let victim = (0..count)
            .rev()
            .find(|index| shown & (1u64 << index) != 0 && !hints[*index].primary)
            .or_else(|| (0..count).rev().find(|index| shown & (1u64 << index) != 0));
        match victim {
            Some(index) => shown &= !(1u64 << index),
            None => break,
        }
    }
    shown
}

/// 提示的配色与间隔：普通键帽、标签、可点项悬浮时标签的底色与项间空格数。主键帽
/// （`primary`，accent 反色）与禁用项（整条灰）不随它变。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FooterHintStyles {
    pub key: Style,
    pub label: Style,
    pub hover_bg: Color,
    pub gap: u16,
}

impl FooterHintStyles {
    /// 页面页脚的配色：设计语言的键帽、说明色标签、主题的悬浮行底色，项间
    /// [`HINT_GAP`] 格。
    pub(crate) fn page(palette: &Palette) -> Self {
        Self {
            key: crate::ui::design::key_cap(palette),
            label: crate::ui::design::key_label(palette),
            hover_bg: palette.hover_row_bg(),
            gap: HINT_GAP,
        }
    }
}

/// 一项提示在给定状态下的键帽与标签样式。
fn hint_styles(
    hint: &FooterHint<'_>,
    hovered: bool,
    palette: &Palette,
    styles: FooterHintStyles,
) -> (Style, Style) {
    if !hint.enabled {
        return (
            Style::default()
                .fg(palette.overlay0)
                .bg(palette.surface_dim),
            Style::default().fg(palette.overlay0),
        );
    }
    let key = if hint.primary {
        crate::ui::design::key_cap_primary(palette)
    } else {
        styles.key
    };
    let label = if hovered {
        styles.label.bg(styles.hover_bg).fg(palette.text)
    } else {
        styles.label
    };
    (key, label)
}

/// 在 `(x, y)` 画一项，返回它的命中矩形。
fn paint_one(
    buffer: &mut Buffer,
    x: u16,
    y: u16,
    hint: &FooterHint<'_>,
    hovered: bool,
    palette: &Palette,
    styles: FooterHintStyles,
) -> Rect {
    let width = hint_width(hint);
    let (key_style, label_style) = hint_styles(hint, hovered, palette, styles);
    key_cap::paint_hint(
        buffer,
        x,
        y,
        x.saturating_add(width),
        hint.key,
        hint.label,
        key_style,
        label_style,
    );
    Rect::new(x, y, width, 1)
}

/// 画页脚提示（页面页脚的配色），返回画出来的项的 `(命中矩形, 下标)`。
pub(crate) fn render_footer_hints(
    buffer: &mut Buffer,
    area: Rect,
    hints: &[FooterHint<'_>],
    hovered: Option<usize>,
    palette: &Palette,
) -> Vec<(Rect, usize)> {
    render_footer_hints_styled(
        buffer,
        area,
        hints,
        hovered,
        palette,
        FooterHintStyles::page(palette),
    )
}

/// 同 [`render_footer_hints`]，普通键帽、标签的配色与项间隔由调用方给。只用
/// `area` 的第一行。
pub(crate) fn render_footer_hints_styled(
    buffer: &mut Buffer,
    area: Rect,
    hints: &[FooterHint<'_>],
    hovered: Option<usize>,
    palette: &Palette,
    styles: FooterHintStyles,
) -> Vec<(Rect, usize)> {
    let mut hits = Vec::new();
    if area.is_empty() || hints.is_empty() {
        return hits;
    }
    let shown = shown_mask(hints, area.width, styles.gap);
    let mut x = area.x;
    for (index, hint) in hints.iter().enumerate().take(MAX_HINTS) {
        if shown & (1u64 << index) == 0 {
            continue;
        }
        let rect = paint_one(
            buffer,
            x,
            area.y,
            hint,
            hovered == Some(index),
            palette,
            styles,
        );
        hits.push((rect, index));
        x = rect.right().saturating_add(styles.gap);
    }
    hits
}

/// 多行页脚：按顺序排，放不下的项整项换到下一行；`area` 的行用完后余下整项丢弃，
/// `overflow_marker` 为真时在最后一项后面画 `…` 标记截断。`area` 只有一行时等同
/// [`render_footer_hints_styled`]（尾部先丢、primary 最后丢），同样按需补 `…`。
// 入参是页脚排布的全部要素，与单行版本一致外加截断标记开关。
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_footer_hints_wrapped(
    buffer: &mut Buffer,
    area: Rect,
    hints: &[FooterHint<'_>],
    hovered: Option<usize>,
    palette: &Palette,
    styles: FooterHintStyles,
    overflow_marker: bool,
) -> Vec<(Rect, usize)> {
    if area.is_empty() || hints.is_empty() {
        return Vec::new();
    }
    let marker_style = styles.label;
    if area.height <= 1 {
        let hits = render_footer_hints_styled(buffer, area, hints, hovered, palette, styles);
        if overflow_marker && hits.len() < hints.len().min(MAX_HINTS) {
            let x = hits
                .last()
                .map_or(area.x, |(rect, _)| rect.right().saturating_add(1));
            if x < area.right() {
                put_str(buffer, x, area.y, 1, OVERFLOW, marker_style);
            }
        }
        return hits;
    }
    let mut hits = Vec::new();
    let mut x = area.x;
    let mut y = area.y;
    let mut truncated = false;
    for (index, hint) in hints.iter().enumerate().take(MAX_HINTS) {
        let width = hint_width(hint);
        if x.saturating_add(width) > area.right() {
            if x > area.x && y.saturating_add(1) < area.bottom() {
                y += 1;
                x = area.x;
            } else {
                truncated = true;
                break;
            }
        }
        if x.saturating_add(width) > area.right() {
            truncated = true;
            break;
        }
        let rect = paint_one(buffer, x, y, hint, hovered == Some(index), palette, styles);
        hits.push((rect, index));
        x = rect.right().saturating_add(styles.gap);
    }
    if truncated && overflow_marker {
        let x = hits
            .last()
            .filter(|(rect, _)| rect.y == y)
            .map_or(x, |(rect, _)| rect.right().saturating_add(1));
        if x < area.right() {
            fill_row(buffer, x, y, 1, " ", marker_style);
            put_str(buffer, x, y, 1, OVERFLOW, marker_style);
        }
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::super::row_text;
    use super::*;
    use ratatui::style::Modifier;

    fn hint<'a>(key: &'a str, label: &'a str) -> FooterHint<'a> {
        FooterHint::new(key, label)
    }

    #[test]
    fn hints_lay_out_with_keycaps_and_return_hit_rects() {
        let palette = Palette::catppuccin();
        let hints = [
            FooterHint {
                primary: true,
                ..hint("↵", "save")
            },
            hint("esc", "close"),
            FooterHint {
                enabled: false,
                ..hint("d", "delete")
            },
        ];
        // ↵ 规范成 enter：12 + 2 + 11 + 2 + 10 = 37 列正好放下三项。
        let area = Rect::new(0, 0, 37, 1);
        let mut buffer = Buffer::empty(area);
        let hits = render_footer_hints(&mut buffer, area, &hints, Some(1), &palette);
        assert_eq!(
            row_text(&buffer, 0),
            " enter  save   esc  close   d  delete"
        );
        assert_eq!(
            hits,
            vec![
                (Rect::new(0, 0, 12, 1), 0),
                (Rect::new(14, 0, 11, 1), 1),
                (Rect::new(27, 0, 10, 1), 2),
            ]
        );
        let primary = buffer[(1, 0)].style();
        assert_eq!(primary.bg, Some(palette.accent), "primary 键帽 accent 反色");
        assert!(primary.add_modifier.contains(Modifier::BOLD));
        assert_eq!(buffer[(9, 0)].style().fg, Some(palette.subtext0));
        let keycap = buffer[(15, 0)].style();
        let expected = crate::ui::design::key_cap(&palette);
        assert_eq!(keycap.bg, expected.bg, "普通键帽");
        assert_eq!(keycap.fg, expected.fg);
        let hovered = buffer[(20, 0)].style();
        assert_eq!(hovered.bg, Some(palette.hover_row_bg()), "悬浮标签换底");
        assert_eq!(hovered.fg, Some(palette.text));
        assert_eq!(buffer[(28, 0)].style().fg, Some(palette.overlay0), "禁用灰");
        assert_eq!(buffer[(31, 0)].style().fg, Some(palette.overlay0));

        // 少 3 列：尾部的非 primary 项整项丢弃，不画半个。
        let area = Rect::new(0, 0, 34, 1);
        let mut buffer = Buffer::empty(area);
        let hits = render_footer_hints(&mut buffer, area, &hints, None, &palette);
        assert_eq!(row_text(&buffer, 0), " enter  save   esc  close         ");
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn overflow_drops_from_the_tail_but_keeps_primary_hints_longest() {
        let palette = Palette::catppuccin();
        let hints = [
            hint("a", "first"),
            FooterHint {
                primary: true,
                ..hint("g", "go")
            },
            hint("c", "third"),
        ];
        // 全部需要 9 + 2 + 6 + 2 + 9 = 28 列。
        let area = Rect::new(0, 0, 20, 1);
        let mut buffer = Buffer::empty(area);
        let hits = render_footer_hints(&mut buffer, area, &hints, None, &palette);
        assert_eq!(row_text(&buffer, 0), " a  first   g  go   ");
        assert_eq!(
            hits.iter().map(|(_, index)| *index).collect::<Vec<_>>(),
            vec![0, 1]
        );
        let area = Rect::new(0, 0, 8, 1);
        let mut buffer = Buffer::empty(area);
        let hits = render_footer_hints(&mut buffer, area, &hints, None, &palette);
        assert_eq!(row_text(&buffer, 0), " g  go  ", "只剩 primary");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].1, 1);
        let area = Rect::new(0, 0, 3, 1);
        let mut buffer = Buffer::empty(area);
        let hits = render_footer_hints(&mut buffer, area, &hints, None, &palette);
        assert!(hits.is_empty(), "连 primary 都放不下就不画");
        assert_eq!(row_text(&buffer, 0), "   ");
    }

    #[test]
    fn cjk_labels_use_display_width_and_empty_input_is_a_no_op() {
        let palette = Palette::catppuccin();
        let area = Rect::new(0, 0, 12, 1);
        let mut buffer = Buffer::empty(area);
        let hits = render_footer_hints(&mut buffer, area, &[hint("x", "勾选")], None, &palette);
        assert_eq!(row_text(&buffer, 0), " x  勾选    ");
        assert_eq!(hits, vec![(Rect::new(0, 0, 8, 1), 0)]);
        let hits = render_footer_hints(&mut buffer, area, &[], None, &palette);
        assert!(hits.is_empty());
    }

    #[test]
    fn keys_are_normalized_before_measuring_and_painting() {
        let palette = Palette::catppuccin();
        let hints = [hint("Esc", "back"), hint("^c", "quit")];
        // 10 + 2 + 13 = 25 列：按规范写法量宽、画字。
        let area = Rect::new(0, 0, 25, 1);
        let mut buffer = Buffer::empty(area);
        let hits = render_footer_hints(&mut buffer, area, &hints, None, &palette);
        assert_eq!(row_text(&buffer, 0), " esc  back   ctrl+c  quit");
        assert_eq!(hits.len(), 2);
        let area = Rect::new(0, 0, 24, 1);
        let mut buffer = Buffer::empty(area);
        let hits = render_footer_hints(&mut buffer, area, &hints, None, &palette);
        assert_eq!(row_text(&buffer, 0), " esc  back              ");
        assert_eq!(hits.len(), 1, "第二项按 ctrl+c 的宽度量，放不下就整项丢弃");
    }

    #[test]
    fn wrapped_footers_break_lines_and_mark_truncation() {
        let palette = Palette::catppuccin();
        let hints = [
            hint("a", "alpha"),
            hint("b", "bravo"),
            hint("c", "charlie"),
            hint("d", "delta"),
        ];
        let styles = FooterHintStyles::page(&palette);
        // 每行 20 列：第一行放 a、b（9 + 2 + 9 = 20），第二行放 c，d 放不下。
        let area = Rect::new(0, 0, 20, 2);
        let mut buffer = Buffer::empty(area);
        let hits =
            render_footer_hints_wrapped(&mut buffer, area, &hints, None, &palette, styles, true);
        assert_eq!(row_text(&buffer, 0), " a  alpha   b  bravo");
        assert_eq!(row_text(&buffer, 1), " c  charlie …       ");
        assert_eq!(
            hits.iter()
                .map(|(rect, index)| (rect.y, *index))
                .collect::<Vec<_>>(),
            vec![(0, 0), (0, 1), (1, 2)]
        );
        // 单行：同 render_footer_hints_styled 的尾部丢弃，再隔一格补截断标记。
        let area = Rect::new(0, 0, 22, 1);
        let mut buffer = Buffer::empty(area);
        let hits =
            render_footer_hints_wrapped(&mut buffer, area, &hints, None, &palette, styles, true);
        assert_eq!(row_text(&buffer, 0), " a  alpha   b  bravo …");
        assert_eq!(hits.len(), 2);
    }
}
