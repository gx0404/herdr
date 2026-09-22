//! 卡片：带标题与徽标的边框容器，是监控系统页 / 账号页 / 设置页的分区单元。
//! 边框字形来自调用方的 `BorderGlyphs`（随 `ui.border_style` 解析一次）。

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
};

use super::{draw_frame, ellipsis_width, fill_row, put_str, put_str_ellipsis};
use crate::app::state::Palette;
use crate::ui::BorderGlyphs;

/// 卡片外观。`focused` 边框取 accent，`hovered` 取 overlay1，常态取 surface1；
/// `dimmed` 整体转灰并加 DIM（编辑布局模式下未选中的卡、失效数据的卡）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct CardSpec<'a> {
    pub title: &'a str,
    pub badge: Option<(&'a str, Color)>,
    pub focused: bool,
    pub hovered: bool,
    pub dimmed: bool,
}

fn border_color(spec: &CardSpec<'_>, palette: &Palette) -> Color {
    if spec.dimmed {
        palette.overlay0
    } else if spec.focused {
        palette.accent
    } else if spec.hovered {
        palette.overlay1
    } else if palette.surface1 != Color::Reset {
        palette.surface1
    } else {
        palette.overlay0
    }
}

/// 画卡片，返回内容区（边框内一圈）。`area` 不足 2×2 时什么都不画、返回空矩形。
/// 标题放在上边框左侧、徽标放在上边框右侧，两者都按剩余宽度截断（徽标优先）。
pub(crate) fn render_card(
    buffer: &mut Buffer,
    area: Rect,
    spec: &CardSpec<'_>,
    glyphs: BorderGlyphs,
    palette: &Palette,
) -> Rect {
    if area.width < 2 || area.height < 2 {
        return Rect::default();
    }
    let dim = if spec.dimmed {
        Modifier::DIM
    } else {
        Modifier::empty()
    };
    let background = Style::default()
        .bg(palette.panel_bg)
        .remove_modifier(Modifier::DIM)
        .add_modifier(dim);
    let border = background.fg(border_color(spec, palette));

    for y in area.y..area.bottom() {
        fill_row(buffer, area.x, y, area.width, " ", background);
    }
    draw_frame(buffer, area, glyphs, border);
    let top = area.y;
    let right = area.right() - 1;

    // 上边框：` 徽标 ` 靠右先占位（优先），标题拿剩下的；两者都在时中间至少
    // 留一格边框线，不让标题贴着徽标。
    let mut title_budget = area.width - 2;
    if let Some((badge, color)) = spec.badge {
        let style = background.fg(if spec.dimmed { palette.overlay0 } else { color });
        let used = framed_label_width(badge, title_budget);
        if used > 0 {
            let x = right - used;
            put_framed_label(buffer, x, top, used, badge, style);
            title_budget = title_budget.saturating_sub(used + 1);
        }
    }
    if !spec.title.is_empty() {
        let style = background
            .fg(if spec.dimmed {
                palette.overlay0
            } else if spec.focused {
                palette.accent
            } else {
                palette.text
            })
            .add_modifier(Modifier::BOLD);
        let used = framed_label_width(spec.title, title_budget);
        if used > 0 {
            put_framed_label(buffer, area.x + 1, top, used, spec.title, style);
        }
    }

    Rect::new(area.x + 1, area.y + 1, area.width - 2, area.height - 2)
}

/// ` text ` 在 `budget` 列内实际占多少列：两侧各留一格空白，正文至少 1 列，
/// 放不下返回 0。按截断后的真实宽度算（宽字符放不下时会少一列），尾部空白
/// 紧跟正文，不留双空格。
fn framed_label_width(text: &str, budget: u16) -> u16 {
    if budget < 3 {
        return 0;
    }
    ellipsis_width(text, budget - 2) + 2
}

/// 在 `(x, y)` 写 ` text `，`width` 取自 [`framed_label_width`]。
fn put_framed_label(buffer: &mut Buffer, x: u16, y: u16, width: u16, text: &str, style: Style) {
    put_str(buffer, x, y, 1, " ", style);
    put_str_ellipsis(buffer, x + 1, y, width - 2, text, style);
    put_str(buffer, x + width - 1, y, 1, " ", style);
}

#[cfg(test)]
mod tests {
    use super::super::row_text;
    use super::*;

    fn render(area: Rect, spec: CardSpec<'_>, glyphs: BorderGlyphs) -> (Buffer, Rect, Palette) {
        let palette = Palette::catppuccin();
        let mut buffer = Buffer::empty(area);
        let inner = render_card(&mut buffer, area, &spec, glyphs, &palette);
        (buffer, inner, palette)
    }

    #[test]
    fn draws_the_frame_with_title_and_badge_and_returns_the_inner_rect() {
        let (buffer, inner, palette) = render(
            Rect::new(0, 0, 20, 4),
            CardSpec {
                title: "CPU",
                badge: Some(("stale", Color::Rgb(1, 2, 3))),
                ..CardSpec::default()
            },
            BorderGlyphs::ROUNDED,
        );
        assert_eq!(row_text(&buffer, 0), "╭ CPU ────── stale ╮");
        assert_eq!(row_text(&buffer, 1), "│                  │");
        assert_eq!(row_text(&buffer, 3), "╰──────────────────╯");
        assert_eq!(inner, Rect::new(1, 1, 18, 2));
        let title = buffer[(2, 0)].style();
        assert_eq!(title.fg, Some(palette.text));
        assert!(title.add_modifier.contains(Modifier::BOLD));
        assert_eq!(buffer[(14, 0)].style().fg, Some(Color::Rgb(1, 2, 3)));
        assert_eq!(buffer[(0, 0)].style().fg, Some(palette.surface1));
        assert_eq!(buffer[(5, 1)].style().bg, Some(palette.panel_bg));
    }

    #[test]
    fn focus_hover_and_dim_change_the_border_and_title_colors() {
        let (buffer, _, palette) = render(
            Rect::new(0, 0, 10, 3),
            CardSpec {
                title: "A",
                focused: true,
                ..CardSpec::default()
            },
            BorderGlyphs::SINGLE,
        );
        assert_eq!(buffer[(0, 0)].style().fg, Some(palette.accent));
        assert_eq!(buffer[(2, 0)].style().fg, Some(palette.accent));
        let (buffer, _, palette) = render(
            Rect::new(0, 0, 10, 3),
            CardSpec {
                title: "A",
                hovered: true,
                ..CardSpec::default()
            },
            BorderGlyphs::SINGLE,
        );
        assert_eq!(buffer[(0, 0)].style().fg, Some(palette.overlay1));
        let (buffer, _, palette) = render(
            Rect::new(0, 0, 10, 3),
            CardSpec {
                title: "A",
                badge: Some(("x", palette.red)),
                focused: true,
                dimmed: true,
                ..CardSpec::default()
            },
            BorderGlyphs::SINGLE,
        );
        let border = buffer[(0, 0)].style();
        assert_eq!(border.fg, Some(palette.overlay0), "dimmed 压过 focused");
        assert!(border.add_modifier.contains(Modifier::DIM));
        assert_eq!(
            buffer[(7, 0)].style().fg,
            Some(palette.overlay0),
            "徽标也转灰"
        );
    }

    #[test]
    fn narrow_cards_truncate_the_cjk_title_and_keep_the_badge() {
        let (buffer, _, _) = render(
            Rect::new(0, 0, 12, 3),
            CardSpec {
                title: "内存与交换",
                badge: Some(("!", Color::Red)),
                ..CardSpec::default()
            },
            BorderGlyphs::SINGLE,
        );
        // 12 列：角 2 + 徽标 ` ! ` 3 + 间隔 1 → 标题预算 6 → 正文 4 列；「存」放不下，
        // 写「内…」3 列，尾部空白紧跟正文，余下的仍是边框线。
        assert_eq!(row_text(&buffer, 0), "┌ 内… ── ! ┐");
        let (buffer, _, _) = render(
            Rect::new(0, 0, 6, 3),
            CardSpec {
                title: "Title",
                ..CardSpec::default()
            },
            BorderGlyphs::SINGLE,
        );
        assert_eq!(row_text(&buffer, 0), "┌ T… ┐");
    }

    #[test]
    fn too_small_areas_draw_nothing_and_return_an_empty_rect() {
        let area = Rect::new(0, 0, 1, 1);
        let (buffer, inner, _) = render(
            area,
            CardSpec {
                title: "x",
                ..CardSpec::default()
            },
            BorderGlyphs::SINGLE,
        );
        assert_eq!(inner, Rect::default());
        assert_eq!(row_text(&buffer, 0), " ");
        let (buffer, inner, _) = render(
            Rect::new(0, 0, 2, 2),
            CardSpec {
                title: "x",
                ..CardSpec::default()
            },
            BorderGlyphs::DOUBLE,
        );
        assert_eq!(row_text(&buffer, 0), "╔╗", "2×2 只有四个角，标题放不下");
        assert_eq!(row_text(&buffer, 1), "╚╝");
        assert!(inner.is_empty());
    }
}
