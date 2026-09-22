//! 设计系统原语：`gauge`、`meter_row`、`braille_chart`、`card`、`tabs`、
//! `form_field`、`tree`、`menu`、`hover_card`、`footer_hints`、`empty_state`、
//! `table` 各占一个子模块（`src/ui/kit/<name>.rs`）。
//!
//! 统一约定：自由函数；**只画不改状态**；可交互原语返回命中矩形，由调用方写进
//! 自己的命中表；热路径不分配（栈数组或调用方传入的缓冲）；`ascii` / `glyphs`
//! 开关做字形降级；CJK 宽度一律走 `crate::ui::display_width`；颜色一律取自
//! `Palette` 与 `crate::ui::color`。本目录同时服务 server 直连渲染，不得反向依赖
//! `crate::client`。调用写全路径（`crate::ui::kit::gauge::render_gauge`），不往
//! `crate::ui` 的 re-export 区堆符号。
//!
//! 本文件另放子模块共用的小工具：带裁剪 / 省略号的文本写入、整行填充、选中 /
//! 悬浮行样式。父模块的私有项对子模块可见，不对 `kit` 之外导出。

use ratatui::{buffer::Buffer, layout::Position, style::Style};

use super::display_width;

pub(crate) mod braille_chart;
pub(crate) mod gauge;
pub(crate) mod meter_row;

/// 从 `(x, y)` 起写文本，最多占 `max_width` 列；起点落在缓冲区外或零宽直接
/// 返回。宽字符放不下时停在它前面，不写半个字。返回实际占用的列数。
fn put_str(buffer: &mut Buffer, x: u16, y: u16, max_width: u16, text: &str, style: Style) -> u16 {
    if max_width == 0 || !buffer.area.contains(Position::new(x, y)) {
        return 0;
    }
    let (end, _) = buffer.set_stringn(x, y, text, usize::from(max_width), style);
    end.saturating_sub(x)
}

/// 同 [`put_str`]，放不下时截断并以 `…` 收尾（与 `truncate_end` 同口径）。
fn put_str_ellipsis(
    buffer: &mut Buffer,
    x: u16,
    y: u16,
    max_width: u16,
    text: &str,
    style: Style,
) -> u16 {
    if max_width == 0 || !buffer.area.contains(Position::new(x, y)) {
        return 0;
    }
    if display_width(text) <= usize::from(max_width) {
        return put_str(buffer, x, y, max_width, text, style);
    }
    if max_width == 1 {
        return put_str(buffer, x, y, 1, "…", style);
    }
    let used = put_str(buffer, x, y, max_width - 1, text, style);
    used + put_str(buffer, x.saturating_add(used), y, 1, "…", style)
}

/// 用同一个符号与样式填一行的 `width` 列；越出缓冲区的格跳过。
fn fill_row(buffer: &mut Buffer, x: u16, y: u16, width: u16, symbol: &str, style: Style) {
    for column in 0..width {
        let Some(px) = x.checked_add(column) else {
            break;
        };
        if let Some(cell) = buffer.cell_mut((px, y)) {
            cell.set_symbol(symbol).set_style(style);
        }
    }
}

/// 测试用：把一行拼成字符串。宽字符只取一次，跳过它占用的第二格（ratatui 把
/// 那一格 reset 成空格），这样 CJK 断言写起来与肉眼所见一致。
#[cfg(test)]
fn row_text(buffer: &Buffer, y: u16) -> String {
    let mut text = String::new();
    let mut x = buffer.area.x;
    while x < buffer.area.right() {
        let symbol = buffer[(x, y)].symbol();
        text.push_str(symbol);
        x += display_width(symbol).max(1) as u16;
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;

    #[test]
    fn put_str_clips_wide_characters_instead_of_splitting_them() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 5, 1));
        let used = put_str(&mut buffer, 0, 0, 3, "中文字", Style::default());
        assert_eq!(used, 2, "第二个宽字符放不下时停在它前面");
        assert_eq!(row_text(&buffer, 0), "中   ");
        assert_eq!(buffer[(1, 0)].symbol(), " ", "宽字符的第二格被 reset");
    }

    #[test]
    fn put_str_ellipsis_marks_truncation_and_respects_display_width() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 1));
        let used = put_str_ellipsis(&mut buffer, 0, 0, 5, "提交反馈", Style::default());
        assert_eq!(row_text(&buffer, 0), "提交…   ");
        assert_eq!(used, 5);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 1));
        put_str_ellipsis(&mut buffer, 0, 0, 8, "fits", Style::default());
        assert_eq!(row_text(&buffer, 0), "fits    ");
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 1));
        put_str_ellipsis(&mut buffer, 0, 0, 1, "long", Style::default());
        assert_eq!(row_text(&buffer, 0), "…       ");
    }

    #[test]
    fn writes_outside_the_buffer_are_ignored() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 1));
        assert_eq!(put_str(&mut buffer, 0, 3, 4, "x", Style::default()), 0);
        assert_eq!(put_str(&mut buffer, 9, 0, 4, "x", Style::default()), 0);
        fill_row(&mut buffer, 2, 0, 10, "#", Style::default());
        assert_eq!(row_text(&buffer, 0), "  ##");
        fill_row(&mut buffer, u16::MAX - 1, 0, 4, "#", Style::default());
        assert_eq!(row_text(&buffer, 0), "  ##");
    }
}
