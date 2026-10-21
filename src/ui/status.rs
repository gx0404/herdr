//! 持续状态横幅（配置诊断）。复制反馈等会消失的吐司在
//! `client::shell::chrome::toast`。

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    text::Span,
    widgets::{Clear, Paragraph, Widget},
};

use crate::app::state::Palette;

pub(crate) fn render_config_diagnostic_buffer(
    buffer: &mut Buffer,
    area: Rect,
    message: &str,
    palette: &Palette,
    mut covered: impl FnMut(Rect),
) -> u16 {
    // 底色是 yellow，前景按 yellow 挑对比色；`panel_contrast_fg` 是为 accent 底
    // 挑的，terminal 主题下会给出白字叠黄底（4.2:1）。
    let style = Style::default()
        .fg(super::color::contrast_fg(palette, palette.yellow))
        .bg(palette.yellow)
        .add_modifier(Modifier::BOLD);
    let mut rendered_rows = 0u16;

    for (row, line) in message
        .lines()
        .filter(|line| !line.trim().is_empty())
        .take(area.height as usize)
        .enumerate()
    {
        let text = format!(" {line} ");
        let width = u16::try_from(unicode_width::UnicodeWidthStr::width(text.as_str()))
            .unwrap_or(u16::MAX)
            .min(area.width);
        let diagnostic_area = Rect::new(
            area.x + area.width.saturating_sub(width),
            area.y + row as u16,
            width,
            1,
        );

        covered(diagnostic_area);
        Clear.render(diagnostic_area, buffer);
        Paragraph::new(Span::styled(text, style)).render(diagnostic_area, buffer);
        rendered_rows = rendered_rows.saturating_add(1);
    }

    rendered_rows
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 诊断条的底色是 yellow，前景必须按 yellow 挑：terminal 主题下按 accent
    /// 挑出来的是白字，叠在黄底上只有 4.2:1；按黄底挑得到黑字（5.0:1）。
    #[test]
    fn config_diagnostic_picks_its_foreground_against_the_yellow_background() {
        use crate::ui::color::contrast_ratio;
        use ratatui::style::Color;

        for name in crate::config::THEME_NAMES {
            let palette = Palette::from_name(name).expect("built-in theme");
            let area = Rect::new(0, 0, 30, 2);
            let mut buffer = Buffer::empty(area);
            let rows =
                render_config_diagnostic_buffer(&mut buffer, area, "bad key", &palette, |_| {});
            assert_eq!(rows, 1);
            let text: String = (area.width - 9..area.width)
                .map(|x| buffer[(x, 0)].symbol())
                .collect();
            assert_eq!(text, " bad key ", "{name}");
            let style = buffer[(area.width - 2, 0)].style();
            assert_eq!(style.bg, Some(palette.yellow), "{name}");
            let ratio = contrast_ratio(style.fg.expect("fg"), palette.yellow).expect("可比较");
            assert!(ratio >= 4.5, "{name}: {ratio}");
        }
        let terminal = Palette::terminal();
        let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 1));
        render_config_diagnostic_buffer(
            &mut buffer,
            Rect::new(0, 0, 12, 1),
            "x",
            &terminal,
            |_| {},
        );
        assert_eq!(buffer[(11, 0)].style().fg, Some(Color::Black));
    }
}
