//! 卡片：带标题与徽标的边框容器，是监控系统页 / 设置页的分区单元。
//! 边框字形来自调用方的 `BorderGlyphs`（随 `ui.border_style` 解析一次）。
//!
//! Graphite 外观：常态边框 `surface1`、标题是小字（不加粗；默认说明色，调用方可给
//! 一个分类色 `tone` 标出卡片的数据类型），徽标（卡片的主读数或状态）粗体靠右压在
//! 上边框上；返回的内容区在边框内再留左右各 1 列内边距。边框始终是中性色阶。

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
/// `tone` 是标题的分类色：常态标题用它（`None` 时说明色），选中时用它加粗
/// （`None` 时正文色加粗），`dimmed` 时仍转灰；不影响边框。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct CardSpec<'a> {
    pub title: &'a str,
    pub badge: Option<(&'a str, Color)>,
    pub focused: bool,
    pub hovered: bool,
    pub dimmed: bool,
    pub tone: Option<Color>,
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

/// 内容区左右各留的内边距（列）。
pub(crate) const CARD_PADDING: u16 = 1;

/// 画卡片，返回内容区（边框内一圈，再去掉左右各 [`CARD_PADDING`] 列；边框内不足
/// 3 列时不留内边距）。`area` 不足 2×2 时什么都不画、返回空矩形。标题放在上边框
/// 左侧、徽标放在上边框右侧，两者都按剩余宽度截断（徽标优先）。
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
        let style = background
            .fg(if spec.dimmed { palette.overlay0 } else { color })
            .add_modifier(Modifier::BOLD);
        let used = framed_label_width(badge, title_budget);
        if used > 0 {
            let x = right - used;
            put_framed_label(buffer, x, top, used, badge, style);
            title_budget = title_budget.saturating_sub(used + 1);
        }
    }
    if !spec.title.is_empty() {
        // 标题是说明层级的小字：常态用分类色（未给时说明色）、不加粗；选中的卡片
        // 标题加粗（未给分类色时换正文色），与 accent 边框一起标出焦点。
        let style = if spec.dimmed {
            background.fg(palette.overlay0)
        } else if spec.focused {
            background
                .fg(spec.tone.unwrap_or(palette.text))
                .add_modifier(Modifier::BOLD)
        } else {
            background.fg(spec.tone.unwrap_or(palette.subtext0))
        };
        let used = framed_label_width(spec.title, title_budget);
        if used > 0 {
            put_framed_label(buffer, area.x + 1, top, used, spec.title, style);
        }
    }

    let inner_width = area.width - 2;
    let padding = if inner_width > CARD_PADDING * 2 {
        CARD_PADDING
    } else {
        0
    };
    Rect::new(
        area.x + 1 + padding,
        area.y + 1,
        inner_width - padding * 2,
        area.height - 2,
    )
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
        assert_eq!(inner, Rect::new(2, 1, 16, 2), "边框内左右各留 1 列内边距");
        let title = buffer[(2, 0)].style();
        assert_eq!(title.fg, Some(palette.subtext0), "标题是说明层级的小字");
        assert!(!title.add_modifier.contains(Modifier::BOLD));
        let badge = buffer[(14, 0)].style();
        assert_eq!(badge.fg, Some(Color::Rgb(1, 2, 3)));
        assert!(badge.add_modifier.contains(Modifier::BOLD), "徽标是主读数");
        assert_eq!(buffer[(0, 0)].style().fg, Some(palette.surface1));
        assert_eq!(buffer[(5, 1)].style().bg, Some(palette.panel_bg));
        // 太窄放不下内边距时退回边框内一圈。
        let (_, inner, _) = render(
            Rect::new(0, 0, 4, 3),
            CardSpec::default(),
            BorderGlyphs::ROUNDED,
        );
        assert_eq!(inner, Rect::new(1, 1, 2, 1));
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
        let title = buffer[(2, 0)].style();
        assert_eq!(title.fg, Some(palette.text), "选中卡片的标题换正文色");
        assert!(title.add_modifier.contains(Modifier::BOLD));
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

    /// 分类色只染标题：常态不加粗、选中加粗、dimmed 仍转灰；边框保持中性。
    #[test]
    fn tone_colors_the_title_but_never_the_border() {
        let tone = Color::Rgb(167, 139, 250);
        for (focused, dimmed) in [(false, false), (true, false), (true, true)] {
            let (buffer, _, palette) = render(
                Rect::new(0, 0, 12, 3),
                CardSpec {
                    title: "MEM",
                    focused,
                    dimmed,
                    tone: Some(tone),
                    ..CardSpec::default()
                },
                BorderGlyphs::SINGLE,
            );
            let title = buffer[(2, 0)].style();
            let border = buffer[(0, 0)].style();
            if dimmed {
                assert_eq!(title.fg, Some(palette.overlay0));
                assert_eq!(border.fg, Some(palette.overlay0));
            } else if focused {
                assert_eq!(title.fg, Some(tone));
                assert!(title.add_modifier.contains(Modifier::BOLD));
                assert_eq!(border.fg, Some(palette.accent), "焦点边框不变");
            } else {
                assert_eq!(title.fg, Some(tone));
                assert!(!title.add_modifier.contains(Modifier::BOLD));
                assert_eq!(border.fg, Some(palette.surface1), "常态边框不变");
            }
        }
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
