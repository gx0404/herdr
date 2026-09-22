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

use ratatui::{
    buffer::Buffer,
    layout::{Position, Rect},
    style::{Modifier, Style},
};

use super::{display_width, display_width_u16, BorderGlyphs};
use crate::app::state::Palette;

pub(crate) mod braille_chart;
pub(crate) mod card;
pub(crate) mod empty_state;
pub(crate) mod footer_hints;
pub(crate) mod form_field;
pub(crate) mod gauge;
pub(crate) mod hover_card;
pub(crate) mod menu;
pub(crate) mod meter_row;
pub(crate) mod table;
pub(crate) mod tabs;
pub(crate) mod tree;

/// 单个字符的显示宽度：走 `display_width` 唯一真源，栈上编码、不分配。
fn char_width(ch: char) -> usize {
    let mut bytes = [0u8; 4];
    display_width(ch.encode_utf8(&mut bytes))
}

/// 键盘选中 / 活动项：accent 反色 + 加粗，与浮层列表 `list_row_style` 同口径。
fn selected_style(palette: &Palette) -> Style {
    Style::default()
        .bg(palette.accent)
        .fg(super::panel_contrast_fg(palette))
        .add_modifier(Modifier::BOLD)
}

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

/// [`put_str_ellipsis`] 在 `max_width` 列内会写多少列，不写缓冲区——供右对齐
/// 或「按真实宽度收口」的布局先量后画。零宽字符不占列（与 ratatui 写入一致）。
fn ellipsis_width(text: &str, max_width: u16) -> u16 {
    let full = display_width_u16(text);
    if full <= max_width {
        return full;
    }
    if max_width <= 1 {
        return max_width;
    }
    let budget = usize::from(max_width - 1);
    let mut used = 0usize;
    for ch in text.chars() {
        let width = char_width(ch);
        if used + width > budget {
            break;
        }
        used += width;
    }
    // used ≤ budget < u16::MAX，转换不会截断。
    u16::try_from(used).unwrap_or(max_width - 1) + 1
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

/// 在 `area` 上画一圈边框（四角 + 四边，内部不动），`area` 不足 2×2 时不画。
/// 卡片与菜单共用；分隔线等与边框相接的 T 形接头由调用方另画。
fn draw_frame(buffer: &mut Buffer, area: Rect, glyphs: BorderGlyphs, style: Style) {
    if area.width < 2 || area.height < 2 {
        return;
    }
    let right = area.right() - 1;
    let bottom = area.bottom() - 1;
    fill_row(
        buffer,
        area.x + 1,
        area.y,
        area.width - 2,
        glyphs.horizontal,
        style,
    );
    fill_row(
        buffer,
        area.x + 1,
        bottom,
        area.width - 2,
        glyphs.horizontal,
        style,
    );
    for y in area.y + 1..bottom {
        put_str(buffer, area.x, y, 1, glyphs.vertical, style);
        put_str(buffer, right, y, 1, glyphs.vertical, style);
    }
    put_str(buffer, area.x, area.y, 1, glyphs.top_left, style);
    put_str(buffer, right, area.y, 1, glyphs.top_right, style);
    put_str(buffer, area.x, bottom, 1, glyphs.bottom_left, style);
    put_str(buffer, right, bottom, 1, glyphs.bottom_right, style);
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

    #[test]
    fn put_str_clips_wide_characters_instead_of_splitting_them() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 5, 1));
        let used = put_str(&mut buffer, 0, 0, 3, "中文字", Style::default());
        assert_eq!(used, 2, "第二个宽字符放不下时停在它前面");
        assert_eq!(row_text(&buffer, 0), "中   ");
        assert_eq!(buffer[(1, 0)].symbol(), " ", "宽字符的第二格被 reset");
        assert_eq!(char_width('中'), 2);
        assert_eq!(char_width('a'), 1);
    }

    #[test]
    fn selected_style_inverts_on_accent_with_a_readable_foreground() {
        let palette = Palette::catppuccin();
        let style = selected_style(&palette);
        assert_eq!(style.bg, Some(palette.accent));
        assert!(style.add_modifier.contains(Modifier::BOLD));
        let fg = style.fg.expect("fg");
        assert!(crate::ui::color::contrast_ratio(fg, palette.accent).expect("可比较") >= 4.5);
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
    fn ellipsis_width_measures_exactly_what_put_str_ellipsis_writes() {
        for (text, max) in [
            ("提交反馈", 5),
            ("提交反馈", 4),
            ("提交反馈", 8),
            ("abc", 2),
            ("abc", 1),
            ("abc", 0),
            ("中a", 2),
        ] {
            let mut buffer = Buffer::empty(Rect::new(0, 0, 10, 1));
            let written = put_str_ellipsis(&mut buffer, 0, 0, max, text, Style::default());
            assert_eq!(ellipsis_width(text, max), written, "{text:?} @ {max}");
        }
        // 第二个宽字符放不下：只写「提」+「…」共 3 列，不是 4 列。
        assert_eq!(ellipsis_width("提交反馈", 4), 3);
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
    #[test]
    fn draw_frame_draws_only_the_outline() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 5, 4));
        fill_row(&mut buffer, 0, 1, 5, "x", Style::default());
        draw_frame(
            &mut buffer,
            Rect::new(0, 0, 4, 3),
            BorderGlyphs::ROUNDED,
            Style::default(),
        );
        assert_eq!(row_text(&buffer, 0), "╭──╮ ");
        assert_eq!(row_text(&buffer, 1), "│xx│x", "内部与区域外不动");
        assert_eq!(row_text(&buffer, 2), "╰──╯ ");
        let mut buffer = Buffer::empty(Rect::new(0, 0, 2, 1));
        draw_frame(
            &mut buffer,
            Rect::new(0, 0, 2, 1),
            BorderGlyphs::SINGLE,
            Style::default(),
        );
        assert_eq!(row_text(&buffer, 0), "  ", "不足 2×2 不画");
    }
}
