//! 键帽：两侧各留一格的按键文字（` k `），标签跟在键帽后空一格。页面页脚、栏内
//! 提示、吐司、帮助与 which-key 共用这一份宽度与绘制；按键文字一律先经
//! [`crate::ui::key_labels::display_key`] 规范成小写口径，量宽与绘制用同一份文字。

use std::borrow::Cow;

use ratatui::{buffer::Buffer, style::Style};

use super::{fill_row, put_str};
use crate::ui::display_width_u16;
use crate::ui::key_labels::display_key;

/// 页面页脚与浮层提示里「键帽 + 标签」项之间的空格数（浮层窄，排得紧一些；键帽
/// 自带底色，两格足以分开）。
pub(crate) const HINT_GAP: u16 = 2;

/// 常驻的栏（状态栏）与吐司里提示项之间的空格数：多留一格更松。
pub(crate) const BAR_HINT_GAP: u16 = 3;

/// 键帽上实际显示的按键文字（规范写法；已规范时借用）。
pub(crate) fn cap_text(key: &str) -> Cow<'_, str> {
    display_key(key)
}

/// 键帽占的列数：按键文字两侧各一格。
pub(crate) fn cap_width(key: &str) -> u16 {
    display_width_u16(&cap_text(key)).saturating_add(2)
}

/// 「键帽 + 空一格 + 标签」占的列数；标签为空时只有键帽。
pub(crate) fn hint_width(key: &str, label: &str) -> u16 {
    let cap = cap_width(key);
    if label.is_empty() {
        cap
    } else {
        cap.saturating_add(1)
            .saturating_add(display_width_u16(label))
    }
}

/// 从 `(x, y)` 起画一个键帽，不越过 `right`（不含）；放不下时裁到 `right`，宽字符
/// 不劈开。返回画到的列。
pub(crate) fn paint_cap(
    buffer: &mut Buffer,
    x: u16,
    y: u16,
    right: u16,
    key: &str,
    style: Style,
) -> u16 {
    let text = cap_text(key);
    let full = display_width_u16(&text).saturating_add(2);
    let width = full.min(right.saturating_sub(x));
    if width == 0 {
        return x;
    }
    fill_row(buffer, x, y, width, " ", style);
    if width > 1 {
        let room = if width == full { width - 2 } else { width - 1 };
        put_str(buffer, x + 1, y, room, &text, style);
    }
    x.saturating_add(width)
}

/// 画「键帽 + 空一格 + 标签」，不越过 `right`；间隔那一格随标签的样式（悬浮换底时
/// 连成一片）。返回画到的列。
// 入参就是一项提示的全部要素（位置、边界、两段文字与两种样式），拆结构体反而啰嗦。
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_hint(
    buffer: &mut Buffer,
    x: u16,
    y: u16,
    right: u16,
    key: &str,
    label: &str,
    cap_style: Style,
    label_style: Style,
) -> u16 {
    let end = paint_cap(buffer, x, y, right, key, cap_style);
    if label.is_empty() || end >= right {
        return end;
    }
    let label_width = display_width_u16(label);
    let span = label_width.saturating_add(1).min(right - end);
    fill_row(buffer, end, y, span, " ", label_style);
    if span > 1 {
        put_str(buffer, end + 1, y, span - 1, label, label_style);
    }
    end.saturating_add(span)
}

#[cfg(test)]
mod tests {
    use super::super::row_text;
    use super::*;
    use ratatui::layout::Rect;

    #[test]
    fn caps_measure_and_paint_the_normalized_key() {
        assert_eq!(cap_width("esc"), 5);
        assert_eq!(cap_width("Esc"), 5, "规范成 esc 后量宽");
        assert_eq!(cap_width("↵"), 7, "↵ 写成 enter");
        assert_eq!(hint_width("g", "会话"), 3 + 1 + 4);
        assert_eq!(hint_width("g", ""), 3);
        let area = Rect::new(0, 0, 16, 1);
        let mut buffer = Buffer::empty(area);
        let cap = Style::default().bg(ratatui::style::Color::Blue);
        let end = paint_hint(&mut buffer, 0, 0, 16, "Esc", "关闭", cap, Style::default());
        assert_eq!(row_text(&buffer, 0), " esc  关闭      ");
        assert_eq!(end, 10);
        assert_eq!(buffer[(0, 0)].style().bg, Some(ratatui::style::Color::Blue));
        assert_eq!(buffer[(4, 0)].style().bg, Some(ratatui::style::Color::Blue));
        assert_ne!(buffer[(5, 0)].style().bg, Some(ratatui::style::Color::Blue));
    }

    #[test]
    fn painting_clips_at_the_right_edge_without_splitting_wide_glyphs() {
        let area = Rect::new(0, 0, 8, 1);
        let mut buffer = Buffer::empty(area);
        let end = paint_hint(
            &mut buffer,
            0,
            0,
            8,
            "空格",
            "勾选",
            Style::default(),
            Style::default(),
        );
        // 「空格」规范成 space：键帽 7 列，标签只剩 1 列（间隔），不画半个字。
        assert_eq!(row_text(&buffer, 0), " space  ");
        assert_eq!(end, 8);
        let mut buffer = Buffer::empty(area);
        assert_eq!(
            paint_cap(&mut buffer, 6, 0, 8, "ctrl+b", Style::default()),
            8
        );
        assert_eq!(paint_cap(&mut buffer, 8, 0, 8, "g", Style::default()), 8);
    }
}
