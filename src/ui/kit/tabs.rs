//! 标签页与三个「一行内的开关类控件」：分段选择、开关、步进器。都只画一行、
//! 返回命中矩形；放不下的项整体丢弃（不画半个标签），对应矩形为空。

#![allow(dead_code)] // seam-stub(monitor)：波 2 监控车道接入监控页签 / 设置页后删除

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
};

use super::{ellipsis_width, fill_row, put_str, put_str_ellipsis, selected_style};
use crate::app::state::Palette;
use crate::ui::display_width_u16;

/// 一个标签页：`badge` 是紧跟标签的小字（计数、状态），`enabled` 为假时置灰。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TabItem<'a> {
    pub label: &'a str,
    pub badge: Option<&'a str>,
    pub enabled: bool,
}

impl<'a> TabItem<'a> {
    pub(crate) const fn new(label: &'a str) -> Self {
        Self {
            label,
            badge: None,
            enabled: true,
        }
    }
}

/// 标签页行：` label badge ` 依次排开、项间 1 列。活动项 accent 反色加粗，悬浮项
/// 弱底色，禁用项灰字。返回与 `items` 等长的命中矩形（放不下的为空）。
pub(crate) fn render_tabs(
    buffer: &mut Buffer,
    area: Rect,
    items: &[TabItem<'_>],
    active: usize,
    hovered: Option<usize>,
    palette: &Palette,
) -> Vec<Rect> {
    let mut rects = vec![Rect::default(); items.len()];
    if area.is_empty() {
        return rects;
    }
    let hover_bg = palette.hover_row_bg();
    let mut x = area.x;
    for (index, item) in items.iter().enumerate() {
        let label_w = display_width_u16(item.label);
        let badge_w = item
            .badge
            .map_or(0, |badge| display_width_u16(badge).saturating_add(1));
        let width = label_w.saturating_add(badge_w).saturating_add(2);
        if x.saturating_add(width) > area.right() {
            break;
        }
        let style = if index == active {
            selected_style(palette)
        } else if !item.enabled {
            Style::default().fg(palette.overlay0)
        } else if hovered == Some(index) {
            Style::default().fg(palette.text).bg(hover_bg)
        } else {
            Style::default().fg(palette.subtext0)
        };
        let rect = Rect::new(x, area.y, width, 1);
        fill_row(buffer, x, area.y, width, " ", style);
        put_str(buffer, x + 1, area.y, label_w, item.label, style);
        if let Some(badge) = item.badge {
            let badge_style = if index == active || !item.enabled {
                style
            } else {
                style.fg(palette.overlay1)
            };
            put_str(
                buffer,
                x + 2 + label_w,
                area.y,
                badge_w - 1,
                badge,
                badge_style,
            );
        }
        rects[index] = rect;
        x = x.saturating_add(width).saturating_add(1);
    }
    rects
}

/// 分段选择：选项紧贴排开、无间隔，以 surface0 底色连成一整块；活动段 accent
/// 反色。`enabled` 为假整块置灰、活动段只换 surface1 底。返回与 `options`
/// 等长的命中矩形。
pub(crate) fn render_segmented(
    buffer: &mut Buffer,
    area: Rect,
    options: &[&str],
    active: usize,
    hovered: Option<usize>,
    enabled: bool,
    palette: &Palette,
) -> Vec<Rect> {
    let mut rects = vec![Rect::default(); options.len()];
    if area.is_empty() {
        return rects;
    }
    let hover_bg = palette.hover_row_bg();
    let mut x = area.x;
    for (index, option) in options.iter().enumerate() {
        let width = display_width_u16(option).saturating_add(2);
        if x.saturating_add(width) > area.right() {
            break;
        }
        let style = match (enabled, index == active, hovered == Some(index)) {
            (false, true, _) => Style::default().fg(palette.overlay0).bg(palette.surface1),
            (false, false, _) => Style::default().fg(palette.overlay0).bg(palette.surface0),
            (true, true, _) => selected_style(palette),
            (true, false, true) => Style::default().fg(palette.text).bg(hover_bg),
            (true, false, false) => Style::default().fg(palette.subtext0).bg(palette.surface0),
        };
        fill_row(buffer, x, area.y, width, " ", style);
        put_str(buffer, x + 1, area.y, width - 2, option, style);
        rects[index] = Rect::new(x, area.y, width, 1);
        x = x.saturating_add(width);
    }
    rects
}

/// 开关：3 列的滑块，开 `━━●`（accent）、关 `●━━`（overlay0）；`focused` 加弱
/// 底色作焦点环，`enabled` 为假整体灰。`ascii` 用 `--o` / `o--`。返回命中矩形。
pub(crate) fn render_toggle(
    buffer: &mut Buffer,
    area: Rect,
    on: bool,
    focused: bool,
    enabled: bool,
    ascii: bool,
    palette: &Palette,
) -> Rect {
    const WIDTH: u16 = 3;
    if area.is_empty() {
        return Rect::default();
    }
    let width = WIDTH.min(area.width);
    let rect = Rect::new(area.x, area.y, width, 1);
    let color = match (enabled, on) {
        (false, _) => palette.overlay0,
        (true, true) => palette.accent,
        (true, false) => palette.overlay0,
    };
    let mut style = Style::default().fg(color);
    if focused && enabled {
        style = style
            .bg(palette.hover_row_bg())
            .add_modifier(Modifier::BOLD);
    }
    let glyphs = match (ascii, on) {
        (false, true) => "━━●",
        (false, false) => "●━━",
        (true, true) => "--o",
        (true, false) => "o--",
    };
    put_str(buffer, area.x, area.y, width, glyphs, style);
    rect
}

/// 步进器：`[-] value [+]`，两个 3 列按钮天然 ASCII 且好点。步进器占满 `area`
/// 的宽度：减号钉左端、加号钉右端、值居中——调用方按最长取值定宽，值变化时按钮
/// 不跳位（连点不会点空）。值放不下时截断；连按钮都放不下（< 9 列）时不画、返回
/// 两个空矩形。返回 `(减, 加)` 的命中矩形。
pub(crate) fn render_stepper(
    buffer: &mut Buffer,
    area: Rect,
    value: &str,
    focused: bool,
    enabled: bool,
    palette: &Palette,
) -> (Rect, Rect) {
    const BUTTON: u16 = 3;
    // 两个按钮 + 两侧各 1 列间隔 + 至少 1 列值。
    if area.height == 0 || area.width < BUTTON * 2 + 3 {
        return (Rect::default(), Rect::default());
    }
    let slot = area.width - BUTTON * 2 - 2;
    let value_w = ellipsis_width(value, slot);
    let bracket = Style::default().fg(palette.overlay0);
    let sign = Style::default().fg(match (enabled, focused) {
        (false, _) => palette.overlay0,
        (true, true) => palette.accent,
        (true, false) => palette.overlay1,
    });
    let value_style = if enabled {
        let style = Style::default().fg(palette.text);
        if focused {
            style.add_modifier(Modifier::BOLD)
        } else {
            style
        }
    } else {
        Style::default().fg(palette.overlay0)
    };
    let y = area.y;
    let minus = Rect::new(area.x, y, BUTTON, 1);
    let plus = Rect::new(area.right() - BUTTON, y, BUTTON, 1);
    for (rect, glyph) in [(minus, "-"), (plus, "+")] {
        put_str(buffer, rect.x, y, 1, "[", bracket);
        put_str(buffer, rect.x + 1, y, 1, glyph, sign);
        put_str(buffer, rect.x + 2, y, 1, "]", bracket);
    }
    let value_x = minus.right() + 1 + (slot - value_w) / 2;
    put_str_ellipsis(buffer, value_x, y, slot, value, value_style);
    (minus, plus)
}

#[cfg(test)]
mod tests {
    use super::super::row_text;
    use super::*;
    use ratatui::style::Color;

    #[test]
    fn tabs_lay_out_with_badges_and_return_hit_rects() {
        let palette = Palette::catppuccin();
        let area = Rect::new(0, 0, 30, 1);
        let mut buffer = Buffer::empty(area);
        let items = [
            TabItem::new("Monitor"),
            TabItem {
                label: "Accounts",
                badge: Some("3"),
                enabled: true,
            },
            TabItem {
                label: "Prefs",
                badge: None,
                enabled: false,
            },
        ];
        let rects = render_tabs(&mut buffer, area, &items, 1, Some(2), &palette);
        assert_eq!(row_text(&buffer, 0), " Monitor   Accounts 3   Prefs ");
        assert_eq!(
            rects,
            vec![
                Rect::new(0, 0, 9, 1),
                Rect::new(10, 0, 12, 1),
                Rect::new(23, 0, 7, 1),
            ]
        );
        let active = buffer[(11, 0)].style();
        assert_eq!(active.bg, Some(palette.accent));
        assert!(active.add_modifier.contains(Modifier::BOLD));
        assert_eq!(buffer[(1, 0)].style().fg, Some(palette.subtext0));
        assert_eq!(
            buffer[(24, 0)].style().fg,
            Some(palette.overlay0),
            "禁用项灰字，悬浮不改"
        );
    }

    #[test]
    fn tabs_that_do_not_fit_are_dropped_whole() {
        let palette = Palette::catppuccin();
        let area = Rect::new(0, 0, 13, 1);
        let mut buffer = Buffer::empty(area);
        let items = [
            TabItem::new("监控"),
            TabItem::new("账号"),
            TabItem::new("偏好"),
        ];
        let rects = render_tabs(&mut buffer, area, &items, 0, None, &palette);
        assert_eq!(row_text(&buffer, 0), " 监控   账号 ");
        assert_eq!(rects[1], Rect::new(7, 0, 6, 1));
        assert_eq!(rects[2], Rect::default(), "第三个放不下，不画半个");
        let mut buffer = Buffer::empty(area);
        let rects = render_tabs(&mut buffer, Rect::default(), &items, 0, None, &palette);
        assert_eq!(rects.len(), 3);
        assert!(rects.iter().all(|rect| rect.is_empty()));
    }

    #[test]
    fn segmented_control_is_contiguous_and_greys_out_when_disabled() {
        let palette = Palette::catppuccin();
        let area = Rect::new(0, 0, 20, 1);
        let mut buffer = Buffer::empty(area);
        let rects = render_segmented(
            &mut buffer,
            area,
            &["Left", "Right"],
            1,
            Some(0),
            true,
            &palette,
        );
        assert_eq!(row_text(&buffer, 0), " Left  Right        ");
        assert_eq!(rects, vec![Rect::new(0, 0, 6, 1), Rect::new(6, 0, 7, 1)]);
        assert_eq!(buffer[(1, 0)].style().bg, Some(palette.hover_row_bg()));
        assert_eq!(buffer[(7, 0)].style().bg, Some(palette.accent));

        let mut buffer = Buffer::empty(area);
        render_segmented(
            &mut buffer,
            area,
            &["Left", "Right"],
            1,
            None,
            false,
            &palette,
        );
        assert_eq!(buffer[(1, 0)].style().fg, Some(palette.overlay0));
        assert_eq!(
            buffer[(7, 0)].style().bg,
            Some(palette.surface1),
            "禁用时活动段不再 accent"
        );
    }

    #[test]
    fn toggle_draws_the_knob_side_and_degrades_to_ascii() {
        let palette = Palette::catppuccin();
        let area = Rect::new(0, 0, 5, 1);
        let mut buffer = Buffer::empty(area);
        let rect = render_toggle(&mut buffer, area, true, true, true, false, &palette);
        assert_eq!(row_text(&buffer, 0), "━━●  ");
        assert_eq!(rect, Rect::new(0, 0, 3, 1));
        let style = buffer[(0, 0)].style();
        assert_eq!(style.fg, Some(palette.accent));
        assert_eq!(style.bg, Some(palette.hover_row_bg()), "焦点环");
        let mut buffer = Buffer::empty(area);
        render_toggle(&mut buffer, area, false, false, true, false, &palette);
        assert_eq!(row_text(&buffer, 0), "●━━  ");
        assert_eq!(buffer[(0, 0)].style().fg, Some(palette.overlay0));
        assert_eq!(buffer[(0, 0)].style().bg, Some(Color::Reset));
        let mut buffer = Buffer::empty(area);
        render_toggle(&mut buffer, area, true, true, false, true, &palette);
        assert_eq!(row_text(&buffer, 0), "--o  ");
        assert_eq!(
            buffer[(0, 0)].style().fg,
            Some(palette.overlay0),
            "禁用整体灰"
        );
        assert_eq!(
            buffer[(0, 0)].style().bg,
            Some(Color::Reset),
            "禁用不画焦点环"
        );
        let mut buffer = Buffer::empty(Rect::new(0, 0, 2, 1));
        let rect = render_toggle(
            &mut buffer,
            Rect::new(0, 0, 2, 1),
            true,
            false,
            true,
            false,
            &palette,
        );
        assert_eq!(row_text(&buffer, 0), "━━", "窄区域裁到可用宽度");
        assert_eq!(rect, Rect::new(0, 0, 2, 1));
    }

    #[test]
    fn stepper_pins_the_buttons_to_both_ends_and_clips_the_value() {
        let palette = Palette::catppuccin();
        let area = Rect::new(2, 0, 14, 1);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 16, 1));
        let (minus, plus) = render_stepper(&mut buffer, area, "1200", true, true, &palette);
        assert_eq!(
            row_text(&buffer, 0),
            "  [-]  1200  [+]",
            "值在两按钮之间居中"
        );
        assert_eq!(minus, Rect::new(2, 0, 3, 1));
        assert_eq!(plus, Rect::new(13, 0, 3, 1), "加号钉在区域右端");
        assert_eq!(buffer[(3, 0)].style().fg, Some(palette.accent));
        assert_eq!(buffer[(2, 0)].style().fg, Some(palette.overlay0));
        assert!(buffer[(7, 0)].style().add_modifier.contains(Modifier::BOLD));
        // 值变短，按钮不跳位。
        let mut buffer = Buffer::empty(Rect::new(0, 0, 16, 1));
        let (_, plus_short) = render_stepper(&mut buffer, area, "5", true, true, &palette);
        assert_eq!(plus_short, plus);
        assert_eq!(row_text(&buffer, 0), "  [-]   5    [+]");

        let area = Rect::new(0, 0, 12, 1);
        let mut buffer = Buffer::empty(area);
        let (minus, plus) = render_stepper(&mut buffer, area, "很长的值", false, false, &palette);
        assert_eq!(row_text(&buffer, 0), "[-] 很…  [+]", "CJK 值按显示宽度截断");
        assert_eq!(minus, Rect::new(0, 0, 3, 1));
        assert_eq!(plus, Rect::new(9, 0, 3, 1));
        assert_eq!(
            buffer[(1, 0)].style().fg,
            Some(palette.overlay0),
            "禁用按钮灰"
        );
        assert_eq!(buffer[(4, 0)].style().fg, Some(palette.overlay0));

        let area = Rect::new(0, 0, 8, 1);
        let mut buffer = Buffer::empty(area);
        let (minus, plus) = render_stepper(&mut buffer, area, "1", false, true, &palette);
        assert_eq!((minus, plus), (Rect::default(), Rect::default()));
        assert_eq!(row_text(&buffer, 0), "        ", "放不下就不画");
    }
}
