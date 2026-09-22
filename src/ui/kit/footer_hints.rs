//! 页脚键位提示：` key ` 键帽 + 标签，一行排开、可点。放不下时从尾部丢弃，
//! `primary` 项最后才丢。

#![allow(dead_code)] // seam-stub(machines)：波 2 机器车道接入机器浮层页脚后删除

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
};

use super::{fill_row, put_str, selected_style};
use crate::app::state::Palette;
use crate::ui::display_width_u16;

/// 一条提示。`primary` = 主动作（键帽 accent 反色、最后丢弃）；`enabled` 为假
/// 整条灰。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FooterHint<'a> {
    pub key: &'a str,
    pub label: &'a str,
    pub enabled: bool,
    pub primary: bool,
}

/// 项间间隔与最多参与排布的项数（位掩码上限；再多的项直接不画）。
const GAP: u16 = 2;
const MAX_HINTS: usize = 64;

fn hint_width(hint: &FooterHint<'_>) -> u16 {
    display_width_u16(hint.key)
        .saturating_add(2)
        .saturating_add(1)
        .saturating_add(display_width_u16(hint.label))
}

fn total_width(hints: &[FooterHint<'_>], shown: u64) -> u16 {
    let mut total = 0u16;
    let mut count = 0u16;
    for (index, hint) in hints.iter().enumerate().take(MAX_HINTS) {
        if shown & (1u64 << index) == 0 {
            continue;
        }
        total = total.saturating_add(hint_width(hint));
        count += 1;
    }
    total.saturating_add(GAP.saturating_mul(count.saturating_sub(1)))
}

/// 决定哪些项画出来：先丢最靠后的非 primary，都丢完了再从后往前丢 primary。
fn shown_mask(hints: &[FooterHint<'_>], width: u16) -> u64 {
    let count = hints.len().min(MAX_HINTS);
    let mut shown = if count >= 64 {
        u64::MAX
    } else {
        (1u64 << count) - 1
    };
    while shown != 0 && total_width(hints, shown) > width {
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

/// 画页脚提示，返回画出来的项的 `(命中矩形, 下标)`。
pub(crate) fn render_footer_hints(
    buffer: &mut Buffer,
    area: Rect,
    hints: &[FooterHint<'_>],
    hovered: Option<usize>,
    palette: &Palette,
) -> Vec<(Rect, usize)> {
    let mut hits = Vec::new();
    if area.is_empty() || hints.is_empty() {
        return hits;
    }
    let shown = shown_mask(hints, area.width);
    let hover_bg = palette.hover_row_bg();
    let mut x = area.x;
    let y = area.y;
    for (index, hint) in hints.iter().enumerate().take(MAX_HINTS) {
        if shown & (1u64 << index) == 0 {
            continue;
        }
        let key_w = display_width_u16(hint.key).saturating_add(2);
        let label_w = display_width_u16(hint.label);
        let width = hint_width(hint);
        let (key_style, label_style) = if !hint.enabled {
            (
                Style::default()
                    .fg(palette.overlay0)
                    .bg(palette.surface_dim),
                Style::default().fg(palette.overlay0),
            )
        } else {
            let key = if hint.primary {
                selected_style(palette)
            } else {
                Style::default()
                    .fg(palette.text)
                    .bg(palette.surface0)
                    .add_modifier(Modifier::BOLD)
            };
            let label = Style::default().fg(palette.subtext0);
            let label = if hovered == Some(index) {
                label.bg(hover_bg).fg(palette.text)
            } else {
                label
            };
            (key, label)
        };
        fill_row(buffer, x, y, key_w, " ", key_style);
        put_str(buffer, x + 1, y, key_w - 2, hint.key, key_style);
        fill_row(buffer, x + key_w, y, 1 + label_w, " ", label_style);
        put_str(buffer, x + key_w + 1, y, label_w, hint.label, label_style);
        hits.push((Rect::new(x, y, width, 1), index));
        x = x.saturating_add(width).saturating_add(GAP);
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::super::row_text;
    use super::*;

    fn hint<'a>(key: &'a str, label: &'a str) -> FooterHint<'a> {
        FooterHint {
            key,
            label,
            enabled: true,
            primary: false,
        }
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
        // 8 + 2 + 11 + 2 + 10 = 33 列正好放下三项。
        let area = Rect::new(0, 0, 33, 1);
        let mut buffer = Buffer::empty(area);
        let hits = render_footer_hints(&mut buffer, area, &hints, Some(1), &palette);
        assert_eq!(row_text(&buffer, 0), " ↵  save   esc  close   d  delete");
        assert_eq!(
            hits,
            vec![
                (Rect::new(0, 0, 8, 1), 0),
                (Rect::new(10, 0, 11, 1), 1),
                (Rect::new(23, 0, 10, 1), 2),
            ]
        );
        let primary = buffer[(1, 0)].style();
        assert_eq!(primary.bg, Some(palette.accent), "primary 键帽 accent 反色");
        assert!(primary.add_modifier.contains(Modifier::BOLD));
        assert_eq!(buffer[(5, 0)].style().fg, Some(palette.subtext0));
        let keycap = buffer[(11, 0)].style();
        assert_eq!(keycap.bg, Some(palette.surface0), "普通键帽");
        assert!(keycap.add_modifier.contains(Modifier::BOLD));
        let hovered = buffer[(16, 0)].style();
        assert_eq!(hovered.bg, Some(palette.hover_row_bg()), "悬浮标签换底");
        assert_eq!(hovered.fg, Some(palette.text));
        assert_eq!(buffer[(24, 0)].style().fg, Some(palette.overlay0), "禁用灰");
        assert_eq!(buffer[(27, 0)].style().fg, Some(palette.overlay0));

        // 少 3 列：尾部的非 primary 项整项丢弃，不画半个。
        let area = Rect::new(0, 0, 30, 1);
        let mut buffer = Buffer::empty(area);
        let hits = render_footer_hints(&mut buffer, area, &hints, None, &palette);
        assert_eq!(row_text(&buffer, 0), " ↵  save   esc  close         ");
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn overflow_drops_from_the_tail_but_keeps_primary_hints_longest() {
        let palette = Palette::catppuccin();
        let hints = [
            hint("a", "first"),
            FooterHint {
                primary: true,
                ..hint("↵", "go")
            },
            hint("c", "third"),
        ];
        // 全部需要 9 + 2 + 6 + 2 + 9 = 28 列。
        let area = Rect::new(0, 0, 20, 1);
        let mut buffer = Buffer::empty(area);
        let hits = render_footer_hints(&mut buffer, area, &hints, None, &palette);
        assert_eq!(row_text(&buffer, 0), " a  first   ↵  go   ");
        assert_eq!(
            hits.iter().map(|(_, index)| *index).collect::<Vec<_>>(),
            vec![0, 1]
        );
        let area = Rect::new(0, 0, 8, 1);
        let mut buffer = Buffer::empty(area);
        let hits = render_footer_hints(&mut buffer, area, &hints, None, &palette);
        assert_eq!(row_text(&buffer, 0), " ↵  go  ", "只剩 primary");
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
        let hits = render_footer_hints(&mut buffer, area, &[hint("空格", "勾选")], None, &palette);
        assert_eq!(row_text(&buffer, 0), " 空格  勾选 ");
        assert_eq!(hits, vec![(Rect::new(0, 0, 11, 1), 0)]);
        let hits = render_footer_hints(&mut buffer, area, &[], None, &palette);
        assert!(hits.is_empty());
    }
}
