//! 空状态：区域中央的「字形 / 标题 / 说明 / 动作按钮」竖排。高度不足时从最不
//! 重要的开始省（先字形，再说明，最后动作），标题总在。

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
};

use super::{ellipsis_width, fill_row, put_str_ellipsis};
use crate::app::state::Palette;
use crate::ui::{modal_button_style, ModalButtonState, ModalButtonTone};

/// 空状态内容。`body` 可含 `\n` 分行；`action` 画成主按钮并返回命中矩形。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct EmptyState<'a> {
    pub glyph: Option<&'a str>,
    pub title: &'a str,
    pub body: Option<&'a str>,
    pub action: Option<&'a str>,
}

/// 居中写一行（按截断后的真实宽度居中），返回其矩形。
fn centered(buffer: &mut Buffer, area: Rect, y: u16, text: &str, style: Style) -> Rect {
    let width = ellipsis_width(text, area.width);
    let x = area.x + (area.width - width) / 2;
    put_str_ellipsis(buffer, x, y, width, text, style);
    Rect::new(x, y, width, 1)
}

/// 画空状态，返回动作按钮的命中矩形（没画按钮时为 `None`）。
pub(crate) fn render_empty_state(
    buffer: &mut Buffer,
    area: Rect,
    spec: &EmptyState<'_>,
    palette: &Palette,
) -> Option<Rect> {
    if area.is_empty() {
        return None;
    }
    let body_lines = spec.body.map_or(0, |body| {
        u16::try_from(body.lines().count()).unwrap_or(u16::MAX)
    });
    let has_action = spec.action.is_some();
    let needed = |glyph: bool, body: bool| -> u16 {
        u16::from(glyph) * 2 + 1 + if body { body_lines } else { 0 } + u16::from(has_action) * 2
    };
    let mut show_glyph = spec.glyph.is_some();
    let mut show_body = body_lines > 0;
    if needed(show_glyph, show_body) > area.height {
        show_glyph = false;
    }
    if needed(show_glyph, show_body) > area.height {
        show_body = false;
    }
    let height = needed(show_glyph, show_body).min(area.height);
    let mut y = area.y + (area.height - height) / 2;

    if show_glyph {
        if let Some(glyph) = spec.glyph {
            centered(
                buffer,
                area,
                y,
                glyph,
                Style::default().fg(palette.overlay0),
            );
        }
        y += 2;
    }
    centered(
        buffer,
        area,
        y,
        spec.title,
        Style::default()
            .fg(palette.text)
            .add_modifier(Modifier::BOLD),
    );
    y += 1;
    if show_body {
        if let Some(body) = spec.body {
            for line in body.lines() {
                if y >= area.bottom() {
                    break;
                }
                centered(buffer, area, y, line, Style::default().fg(palette.subtext0));
                y += 1;
            }
        }
    }
    let action = spec.action?;
    y += 1;
    if y >= area.bottom() {
        return None;
    }
    let style = modal_button_style(palette, ModalButtonTone::Primary, ModalButtonState::Normal);
    // 按钮是 ` 文案 `：两侧各 1 列内边距；区域不足 3 列时只画截断后的文案。
    let width = if area.width >= 3 {
        ellipsis_width(action, area.width - 2) + 2
    } else {
        ellipsis_width(action, area.width)
    };
    let x = area.x + (area.width - width) / 2;
    fill_row(buffer, x, y, width, " ", style);
    let pad = u16::from(area.width >= 3);
    put_str_ellipsis(buffer, x + pad, y, width - pad * 2, action, style);
    Some(Rect::new(x, y, width, 1))
}

#[cfg(test)]
mod tests {
    use super::super::row_text;
    use super::*;

    fn render(area: Rect, spec: EmptyState<'_>) -> (Buffer, Option<Rect>, Palette) {
        let palette = Palette::catppuccin();
        let mut buffer = Buffer::empty(area);
        let action = render_empty_state(&mut buffer, area, &spec, &palette);
        (buffer, action, palette)
    }

    #[test]
    fn full_layout_is_centered_and_returns_the_action_rect() {
        let (buffer, action, palette) = render(
            Rect::new(0, 0, 20, 9),
            EmptyState {
                glyph: Some("◌"),
                title: "No machines",
                body: Some("Add one to\nget started"),
                action: Some("Add"),
            },
        );
        // 需要 2 + 1 + 2 + 2 = 7 行，9 行里上下各留 1。
        assert_eq!(row_text(&buffer, 0), "                    ");
        assert_eq!(row_text(&buffer, 1), "         ◌          ");
        assert_eq!(row_text(&buffer, 3), "    No machines     ");
        assert_eq!(row_text(&buffer, 4), "     Add one to     ");
        assert_eq!(row_text(&buffer, 5), "    get started     ");
        assert_eq!(
            row_text(&buffer, 7),
            "        Add         ",
            "按钮 ` Add ` 居中"
        );
        assert_eq!(action, Some(Rect::new(7, 7, 5, 1)));
        assert!(buffer[(4, 3)].style().add_modifier.contains(Modifier::BOLD));
        assert_eq!(buffer[(5, 4)].style().fg, Some(palette.subtext0));
        assert_eq!(buffer[(8, 7)].style().bg, Some(palette.accent));
    }

    #[test]
    fn short_areas_drop_glyph_then_body_then_action_but_keep_the_title() {
        let spec = EmptyState {
            glyph: Some("◌"),
            title: "Empty",
            body: Some("body"),
            action: Some("Go"),
        };
        let (buffer, action, _) = render(Rect::new(0, 0, 10, 4), spec);
        assert_eq!(row_text(&buffer, 0), "  Empty   ", "字形先丢");
        assert_eq!(row_text(&buffer, 1), "   body   ");
        assert_eq!(row_text(&buffer, 3), "    Go    ");
        assert!(action.is_some());
        let (buffer, action, _) = render(Rect::new(0, 0, 10, 3), spec);
        assert_eq!(row_text(&buffer, 0), "  Empty   ", "说明再丢");
        assert_eq!(row_text(&buffer, 2), "    Go    ");
        assert!(action.is_some());
        let (buffer, action, _) = render(Rect::new(0, 0, 10, 1), spec);
        assert_eq!(row_text(&buffer, 0), "  Empty   ", "只剩标题");
        assert_eq!(action, None);
    }

    #[test]
    fn cjk_text_is_centered_by_display_width_and_clipped() {
        let (buffer, action, _) = render(
            Rect::new(0, 0, 10, 3),
            EmptyState {
                title: "还没有机器",
                action: Some("添加机器"),
                ..EmptyState::default()
            },
        );
        assert_eq!(row_text(&buffer, 0), "还没有机器");
        assert_eq!(row_text(&buffer, 2), " 添加机器 ");
        assert_eq!(action, Some(Rect::new(0, 2, 10, 1)));
        let (buffer, _, _) = render(
            Rect::new(0, 0, 6, 1),
            EmptyState {
                title: "还没有机器",
                ..EmptyState::default()
            },
        );
        assert_eq!(row_text(&buffer, 0), "还没… ", "宽字符放不下时少写一列");
        let (_, action, _) = render(Rect::default(), EmptyState::default());
        assert_eq!(action, None);
    }
}
