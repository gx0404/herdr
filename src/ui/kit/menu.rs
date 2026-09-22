//! 菜单：右键菜单、命令面板目录视图与子菜单共用的列表原语。分隔线、分组标题、
//! 右对齐快捷键、子菜单箭头、禁用置灰、勾选态、危险项、注意徽标、悬浮 / 键盘
//! 高亮分离；行数放不下时按滚动窗口画（键盘高亮永远在窗口内，上下边框画
//! `▲` / `▼` 提示还有项）。键盘导航（跳过分隔与禁用项、回绕）、首字母跳转与
//! 滚动窗口起点都是纯函数。
//!
//! 行版式（边框内）：`␠[✓␠]标签…[␠●][␠␠快捷键][␠▸]␠`。勾选列只在有项带
//! `checked` 时出现，徽标列只在有项带 `badge` 时出现，箭头列只在有子菜单时出现，
//! 快捷键列按最宽的快捷键右对齐。

#![allow(dead_code)] // seam-stub(menu)：波 2 菜单车道接入右键菜单 / 命令面板后删除

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
};

use super::{draw_frame, ellipsis_width, fill_row, put_str, put_str_ellipsis, selected_style};
use crate::app::state::Palette;
use crate::ui::color::contrast_fg;
use crate::ui::{display_width_u16, BorderGlyphs};

/// 菜单项种类。只有启用的 `Action` / `Submenu` 可激活（可高亮、可点、进命中表）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MenuItemKind {
    Action,
    Submenu,
    Separator,
    Header,
}

/// 一个菜单项。`checked`：`None` = 不是勾选项，`Some(false)` = 可勾选但未勾；
/// `danger` = 破坏性动作（关闭、删除），标签转红；`badge` = 需要注意（如分类里
/// 有可用更新，对应命令面板的 `ClientPaletteItem.badge`），在快捷键列前画一个
/// accent 圆点（`ascii` 为 `o`），只在动作 / 子菜单行画。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MenuItem<'a> {
    pub kind: MenuItemKind,
    pub label: &'a str,
    pub shortcut: Option<&'a str>,
    pub enabled: bool,
    pub checked: Option<bool>,
    pub danger: bool,
    pub badge: bool,
}

impl<'a> MenuItem<'a> {
    const fn of(kind: MenuItemKind, label: &'a str) -> Self {
        Self {
            kind,
            label,
            shortcut: None,
            enabled: true,
            checked: None,
            danger: false,
            badge: false,
        }
    }

    /// 启用的普通动作项。
    pub(crate) const fn action(label: &'a str) -> Self {
        Self::of(MenuItemKind::Action, label)
    }

    /// 展开子菜单的项（行尾画箭头）。
    pub(crate) const fn submenu(label: &'a str) -> Self {
        Self::of(MenuItemKind::Submenu, label)
    }

    /// 分组标题：不可激活，灰色加粗。
    pub(crate) const fn header(label: &'a str) -> Self {
        Self::of(MenuItemKind::Header, label)
    }

    /// 分隔线：与边框相接的一整行横线。
    pub(crate) const fn separator() -> Self {
        Self::of(MenuItemKind::Separator, "")
    }

    /// 能否高亮 / 激活：启用的动作或子菜单项。
    pub(crate) const fn is_activatable(&self) -> bool {
        self.enabled && matches!(self.kind, MenuItemKind::Action | MenuItemKind::Submenu)
    }
}

/// 交互状态：`highlighted` 是键盘选中项（回车激活的就是它），`hovered` 是指针
/// 悬浮项，两者分离（鼠标路过不劫持键盘选择）。指向不可激活项时不画高亮。
/// `hover_bg` 为 `None` 时取 `Palette::hover_row_bg()`；右键菜单接入时传
/// `ComponentStyles::hover_bg`，保住用户主题里的 `hover_bg` 覆盖。`scroll` 是
/// 期望的首个可见项下标（默认 0 = 从头画），渲染时按 [`menu_scroll`] 折算。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct MenuState {
    pub highlighted: usize,
    pub hovered: Option<usize>,
    pub hover_bg: Option<Color>,
    pub scroll: usize,
}

/// 渲染结果：菜单实际占的矩形（含边框）、画出来的可激活项 `(行矩形, 项下标)`
/// 与实际生效的 `scroll`（首个画出的项下标）。调用方把 `scroll` 记回自己的状态，
/// 滚轮从这里继续；本函数不改输入状态。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct MenuRender {
    pub area: Rect,
    pub rows: Vec<(Rect, usize)>,
    pub scroll: usize,
}

/// 菜单最窄宽度（含边框），与既有右键菜单一致。
const MIN_WIDTH: u16 = 14;
/// 空间不够时，快捷键列只在标签列还能保住这么多列时保留。
const MIN_LABEL_WITH_SHORTCUT: u16 = 8;

/// 各列宽度：勾选列、标签列、徽标列（间隔 + 圆点）、快捷键列（不含前面的 2 列
/// 间隔）、箭头列。
#[derive(Debug, Clone, Copy, Default)]
struct Columns {
    check: u16,
    label: u16,
    badge: u16,
    shortcut: u16,
    arrow: u16,
}

impl Columns {
    fn of(items: &[MenuItem<'_>]) -> Self {
        let mut columns = Self::default();
        for item in items {
            if item.kind == MenuItemKind::Separator {
                continue;
            }
            columns.label = columns.label.max(display_width_u16(item.label));
            if let Some(shortcut) = item.shortcut {
                columns.shortcut = columns.shortcut.max(display_width_u16(shortcut));
            }
            if item.checked.is_some() {
                columns.check = 2;
            }
            if item.kind == MenuItemKind::Submenu {
                columns.arrow = 2;
            }
            if item.badge && item.kind != MenuItemKind::Header {
                columns.badge = 2;
            }
        }
        columns
    }

    fn shortcut_span(self) -> u16 {
        if self.shortcut == 0 {
            0
        } else {
            self.shortcut.saturating_add(2)
        }
    }
}

/// 菜单的自然尺寸 `(宽, 高)`，含边框；宽度不小于 14 列。
pub(crate) fn menu_size(items: &[MenuItem<'_>]) -> (u16, u16) {
    let columns = Columns::of(items);
    let inner = columns
        .check
        .saturating_add(columns.label)
        .saturating_add(columns.badge)
        .saturating_add(columns.shortcut_span())
        .saturating_add(columns.arrow)
        .saturating_add(2);
    let width = inner.saturating_add(2).max(MIN_WIDTH);
    let height = u16::try_from(items.len())
        .unwrap_or(u16::MAX)
        .saturating_add(2);
    (width, height)
}

fn row_styles(
    item: &MenuItem<'_>,
    highlighted: bool,
    hovered: bool,
    hover_bg: Color,
    palette: &Palette,
) -> (Style, Style) {
    let base = Style::default().bg(palette.panel_bg);
    if !item.enabled {
        let style = base.fg(palette.overlay0);
        return (style, style);
    }
    if highlighted {
        let style = if item.danger {
            Style::default()
                .bg(palette.red)
                .fg(contrast_fg(palette, palette.red))
                .add_modifier(Modifier::BOLD)
        } else {
            selected_style(palette)
        };
        return (style, style);
    }
    let row = if hovered { base.bg(hover_bg) } else { base };
    let label = row.fg(if item.danger {
        palette.red
    } else {
        palette.text
    });
    (label, row.fg(palette.overlay0))
}

/// 滚动窗口起点（首个画出的项下标）。`visible` 是边框内的行数，即
/// `min(menu_size 的高, bounds 的高) - 2`；[`render_menu`] 用同一口径，调用方
/// 可在视图计算阶段先算好。先把 `state.scroll` 收回到「最后一屏」之内，再移动
/// 最少的行让**可激活**的 `highlighted` 落进窗口——回车激活的永远是画出来的项。
/// 向上揭示时顺带露出高亮项正上方紧挨着的分组标题 / 分隔线（窗口放得下为限），
/// 免得回绕到顶时看不到组名。滚轮滚动而不想被拉回高亮项时，调用方随滚动移动
/// `highlighted`，或把它置为越界值放弃键盘高亮（[`menu_step`] 从越界值出发会
/// 落到首 / 末项）。
pub(crate) fn menu_scroll(items: &[MenuItem<'_>], state: &MenuState, visible: usize) -> usize {
    let visible = visible.max(1);
    let mut scroll = state.scroll.min(items.len().saturating_sub(visible));
    let highlighted = state.highlighted;
    if !items.get(highlighted).is_some_and(MenuItem::is_activatable) {
        return scroll;
    }
    if highlighted < scroll {
        scroll = highlighted;
        while scroll > 0
            && highlighted - (scroll - 1) < visible
            && !items[scroll - 1].is_activatable()
        {
            scroll -= 1;
        }
    } else if highlighted >= scroll + visible {
        scroll = highlighted + 1 - visible;
    }
    scroll
}

/// 画菜单。`anchor` 是左上角的期望位置，放不下时向左 / 向上平移贴住 `bounds`
/// （与既有右键菜单一致；子菜单要「翻到父菜单左侧」由调用方按 [`menu_size`]
/// 算好 anchor）。`bounds` 比自然尺寸小时收窄：先截断标签，标签列不足 8 列再丢
/// 快捷键列（徽标列保留）；行数放不下时只画 [`menu_scroll`] 算出的窗口，上 / 下
/// 还有项时在上 / 下边框正中画 `▲` / `▼`。`ascii` 把 `✓` / `▸` / `●` / `▲` /
/// `▼` 降级为 `*` / `>` / `o` / `^` / `v`。
pub(crate) fn render_menu(
    buffer: &mut Buffer,
    anchor: (u16, u16),
    bounds: Rect,
    items: &[MenuItem<'_>],
    state: &MenuState,
    glyphs: BorderGlyphs,
    ascii: bool,
    palette: &Palette,
) -> MenuRender {
    let mut render = MenuRender::default();
    if items.is_empty() {
        return render;
    }
    let (natural_w, natural_h) = menu_size(items);
    let width = natural_w.min(bounds.width);
    let height = natural_h.min(bounds.height);
    if width < 3 || height < 3 {
        return render;
    }
    let x = anchor.0.clamp(bounds.x, bounds.right() - width);
    let y = anchor.1.clamp(bounds.y, bounds.bottom() - height);
    let area = Rect::new(x, y, width, height);
    render.area = area;

    let base = Style::default().bg(palette.panel_bg).fg(palette.text);
    let border = base.fg(palette.accent);
    for row in area.y..area.bottom() {
        fill_row(buffer, area.x, row, area.width, " ", base);
    }
    draw_frame(buffer, area, glyphs, border);
    let right = area.right() - 1;

    let inner = Rect::new(x + 1, y + 1, width - 2, height - 2);
    let mut columns = Columns::of(items);
    // 内容区 = 两侧各 1 列内边距之间；勾选列与箭头列固定，其余归标签与快捷键。
    let content = inner.width.saturating_sub(2);
    // 极窄时依次让出箭头列、徽标列、勾选列，保证标签至少 1 列、右侧不越过边框。
    if columns.check + columns.badge + columns.arrow + 1 > content {
        columns.arrow = 0;
    }
    if columns.check + columns.badge + 1 > content {
        columns.badge = 0;
    }
    if columns.check + 1 > content {
        columns.check = 0;
    }
    let flexible = content.saturating_sub(columns.check + columns.badge + columns.arrow);
    if columns.shortcut > 0
        && flexible < columns.label.min(MIN_LABEL_WITH_SHORTCUT) + columns.shortcut_span()
    {
        columns.shortcut = 0;
    }
    let label_budget = flexible.saturating_sub(columns.shortcut_span());
    let hover_bg = state.hover_bg.unwrap_or_else(|| palette.hover_row_bg());
    let (check_glyph, arrow_glyph, badge_glyph) = if ascii {
        ("*", ">", "o")
    } else {
        ("✓", "▸", "●")
    };

    let visible = usize::from(inner.height);
    let scroll = menu_scroll(items, state, visible);
    render.scroll = scroll;
    for (offset, (index, item)) in items
        .iter()
        .enumerate()
        .skip(scroll)
        .take(visible)
        .enumerate()
    {
        // offset < inner.height ≤ u16::MAX，转换不会失败。
        let row_y = inner.y + u16::try_from(offset).unwrap_or(0);
        match item.kind {
            MenuItemKind::Separator => {
                put_str(buffer, x, row_y, 1, glyphs.tee_right, border);
                fill_row(
                    buffer,
                    inner.x,
                    row_y,
                    inner.width,
                    glyphs.horizontal,
                    border,
                );
                put_str(buffer, right, row_y, 1, glyphs.tee_left, border);
            }
            MenuItemKind::Header => {
                let style = base.fg(palette.overlay1).add_modifier(Modifier::BOLD);
                put_str_ellipsis(buffer, inner.x + 1, row_y, content, item.label, style);
            }
            MenuItemKind::Action | MenuItemKind::Submenu => {
                let activatable = item.is_activatable();
                let highlighted = activatable && state.highlighted == index;
                let hovered = activatable && !highlighted && state.hovered == Some(index);
                let (label_style, aside_style) =
                    row_styles(item, highlighted, hovered, hover_bg, palette);
                fill_row(buffer, inner.x, row_y, inner.width, " ", label_style);
                let mut cursor = inner.x + 1;
                if columns.check > 0 {
                    if item.checked == Some(true) {
                        put_str(buffer, cursor, row_y, 1, check_glyph, label_style);
                    }
                    cursor += columns.check;
                }
                put_str_ellipsis(buffer, cursor, row_y, label_budget, item.label, label_style);
                // 右侧自右向左：内边距、箭头列、快捷键列、徽标列。
                let mut end = inner.right() - 1;
                if columns.arrow > 0 {
                    if item.kind == MenuItemKind::Submenu {
                        put_str(buffer, end - 1, row_y, 1, arrow_glyph, aside_style);
                    }
                    end -= columns.arrow;
                }
                if columns.shortcut > 0 {
                    if let Some(shortcut) = item.shortcut {
                        let used = ellipsis_width(shortcut, columns.shortcut);
                        put_str_ellipsis(
                            buffer,
                            end - used,
                            row_y,
                            columns.shortcut,
                            shortcut,
                            aside_style,
                        );
                    }
                    end -= columns.shortcut_span();
                }
                if columns.badge > 0 && item.badge {
                    // 高亮行随整行反色、禁用行随整行置灰，其余用 accent 点出来。
                    let style = if highlighted || !item.enabled {
                        aside_style
                    } else {
                        aside_style.fg(palette.accent)
                    };
                    put_str(buffer, end - 1, row_y, 1, badge_glyph, style);
                }
                if activatable {
                    render
                        .rows
                        .push((Rect::new(inner.x, row_y, inner.width, 1), index));
                }
            }
        }
    }

    // 窗口外还有项：上 / 下边框正中画箭头提示。
    let (up_glyph, down_glyph) = if ascii { ("^", "v") } else { ("▲", "▼") };
    let middle = x + width / 2;
    if scroll > 0 {
        put_str(buffer, middle, y, 1, up_glyph, border);
    }
    if scroll + visible < items.len() {
        put_str(buffer, middle, area.bottom() - 1, 1, down_glyph, border);
    }
    render
}

/// 从 `from` 起按 `delta` 的方向走 `|delta|` 个可激活项，跳过分隔、标题与禁用项，
/// 两端回绕。`delta == 0` 时 `from` 可激活就停在原地，否则前进到下一个可激活项。
/// `from` 越界视为「还没有选中」：向下落到第一个可激活项、向上落到最后一个。
/// 没有任何可激活项时返回 `None`。
pub(crate) fn menu_step(items: &[MenuItem<'_>], from: usize, delta: isize) -> Option<usize> {
    let len = items.len();
    if !items.iter().any(MenuItem::is_activatable) {
        return None;
    }
    if delta == 0 && from < len && items[from].is_activatable() {
        return Some(from);
    }
    let forward = delta >= 0;
    let mut index = match (from < len, forward) {
        (true, _) => from,
        (false, true) => len - 1,
        (false, false) => 0,
    };
    for _ in 0..delta.unsigned_abs().max(1) {
        loop {
            index = if forward {
                (index + 1) % len
            } else {
                (index + len - 1) % len
            };
            if items[index].is_activatable() {
                break;
            }
        }
    }
    Some(index)
}

/// 首字母跳转：从 `from` 的下一项起（回绕，最后才轮到 `from` 自己）找第一个标签
/// 首字符与 `ch` 相同（不分大小写）的可激活项。连按同一字母在同首字母项间轮转。
pub(crate) fn menu_first_letter(items: &[MenuItem<'_>], ch: char, from: usize) -> Option<usize> {
    let len = items.len();
    if len == 0 {
        return None;
    }
    let start = if from >= len { 0 } else { from + 1 };
    (0..len)
        .map(|offset| (start + offset) % len)
        .find(|&index| {
            let item = &items[index];
            item.is_activatable()
                && item
                    .label
                    .trim_start()
                    .chars()
                    .next()
                    .is_some_and(|first| first.to_lowercase().eq(ch.to_lowercase()))
        })
}

#[cfg(test)]
mod tests {
    use super::super::row_text;
    use super::*;

    fn sample() -> [MenuItem<'static>; 7] {
        [
            MenuItem::header("Pane"),
            MenuItem {
                shortcut: Some("^F"),
                ..MenuItem::action("Focus")
            },
            MenuItem {
                checked: Some(true),
                ..MenuItem::action("Follow")
            },
            MenuItem::submenu("Move to"),
            MenuItem {
                enabled: false,
                ..MenuItem::action("Rename")
            },
            MenuItem::separator(),
            MenuItem {
                danger: true,
                shortcut: Some("^W"),
                ..MenuItem::action("Close")
            },
        ]
    }

    fn paint(
        width: u16,
        height: u16,
        anchor: (u16, u16),
        items: &[MenuItem<'_>],
        state: MenuState,
        ascii: bool,
    ) -> (Buffer, MenuRender, Palette) {
        let palette = Palette::catppuccin();
        let bounds = Rect::new(0, 0, width, height);
        let mut buffer = Buffer::empty(bounds);
        let render = render_menu(
            &mut buffer,
            anchor,
            bounds,
            items,
            &state,
            BorderGlyphs::SINGLE,
            ascii,
            &palette,
        );
        (buffer, render, palette)
    }

    #[test]
    fn size_accounts_for_check_shortcut_and_arrow_columns() {
        // 内容：勾选 2 + 标签 7 + 间隔 2 + 快捷键 2 + 箭头 2 + 内边距 2 = 17，边框 +2。
        assert_eq!(menu_size(&sample()), (19, 9));
        assert_eq!(menu_size(&[MenuItem::action("OK")]), (MIN_WIDTH, 3));
        assert_eq!(menu_size(&[]), (MIN_WIDTH, 2));
    }

    #[test]
    fn renders_every_item_kind_with_aligned_columns() {
        let (buffer, render, palette) = paint(
            20,
            9,
            (0, 0),
            &sample(),
            MenuState {
                highlighted: 1,
                hovered: Some(3),
                hover_bg: None,
                scroll: 0,
            },
            false,
        );
        let rows: Vec<String> = (0..9).map(|y| row_text(&buffer, y)).collect();
        assert_eq!(
            rows,
            vec![
                "┌─────────────────┐ ",
                "│ Pane            │ ",
                "│   Focus    ^F   │ ",
                "│ ✓ Follow        │ ",
                "│   Move to     ▸ │ ",
                "│   Rename        │ ",
                "├─────────────────┤ ",
                "│   Close    ^W   │ ",
                "└─────────────────┘ ",
            ]
        );
        assert_eq!(render.area, Rect::new(0, 0, 19, 9));
        assert_eq!(
            render
                .rows
                .iter()
                .map(|(_, index)| *index)
                .collect::<Vec<_>>(),
            vec![1, 2, 3, 6],
            "标题、禁用项、分隔线不进命中表"
        );
        assert_eq!(render.rows[0].0, Rect::new(1, 2, 17, 1));

        let header = buffer[(2, 1)].style();
        assert_eq!(header.fg, Some(palette.overlay1));
        assert!(header.add_modifier.contains(Modifier::BOLD));
        let focus = buffer[(4, 2)].style();
        assert_eq!(focus.bg, Some(palette.accent), "键盘高亮 accent 反色");
        assert!(focus.add_modifier.contains(Modifier::BOLD));
        assert_eq!(
            buffer[(13, 2)].style().bg,
            Some(palette.accent),
            "快捷键同行反色"
        );
        assert_eq!(buffer[(4, 3)].style().bg, Some(palette.panel_bg));
        assert_eq!(
            buffer[(4, 4)].style().bg,
            Some(palette.hover_row_bg()),
            "悬浮"
        );
        assert_eq!(buffer[(16, 4)].style().fg, Some(palette.overlay0), "箭头灰");
        assert_eq!(buffer[(4, 5)].style().fg, Some(palette.overlay0), "禁用灰");
        assert_eq!(buffer[(4, 7)].style().fg, Some(palette.red), "危险项红字");
        assert_eq!(
            buffer[(13, 7)].style().fg,
            Some(palette.overlay0),
            "快捷键灰"
        );
        assert_eq!(
            buffer[(0, 6)].style().fg,
            Some(palette.accent),
            "分隔线接边框"
        );
    }

    #[test]
    fn highlighted_danger_items_invert_on_red_and_hover_bg_can_be_overridden() {
        let override_bg = Color::Rgb(1, 2, 3);
        let (buffer, _, palette) = paint(
            20,
            9,
            (0, 0),
            &sample(),
            MenuState {
                highlighted: 6,
                hovered: Some(2),
                hover_bg: Some(override_bg),
                scroll: 0,
            },
            false,
        );
        let close = buffer[(4, 7)].style();
        assert_eq!(close.bg, Some(palette.red));
        assert_eq!(close.fg, Some(contrast_fg(&palette, palette.red)));
        assert_eq!(buffer[(4, 3)].style().bg, Some(override_bg));
        // 高亮 / 悬浮落在禁用项上不画。
        let (buffer, _, palette) = paint(
            20,
            9,
            (0, 0),
            &sample(),
            MenuState {
                highlighted: 4,
                hovered: Some(4),
                hover_bg: None,
                scroll: 0,
            },
            false,
        );
        assert_eq!(buffer[(4, 5)].style().bg, Some(palette.panel_bg));
        assert_eq!(buffer[(4, 5)].style().fg, Some(palette.overlay0));
    }

    #[test]
    fn ascii_degrades_the_check_and_arrow() {
        let (buffer, _, _) = paint(20, 9, (0, 0), &sample(), MenuState::default(), true);
        assert_eq!(row_text(&buffer, 3), "│ * Follow        │ ");
        assert_eq!(row_text(&buffer, 4), "│   Move to     > │ ");
    }

    #[test]
    fn anchor_is_clamped_into_bounds() {
        let items = [MenuItem::action("A"), MenuItem::action("B")];
        let (_, render, _) = paint(30, 10, (25, 9), &items, MenuState::default(), false);
        assert_eq!(
            render.area,
            Rect::new(16, 6, 14, 4),
            "向左、向上平移贴住边界"
        );
        let (_, render, _) = paint(30, 10, (3, 2), &items, MenuState::default(), false);
        assert_eq!(render.area, Rect::new(3, 2, 14, 4));
    }

    #[test]
    fn narrow_bounds_truncate_labels_then_drop_shortcuts() {
        let items = [
            MenuItem {
                shortcut: Some("ctrl+shift+w"),
                ..MenuItem::action("关闭当前窗格")
            },
            MenuItem::action("OK"),
        ];
        // 自然宽度 2 + 12 + 2 + 12 + 2 = 28 → 30。
        assert_eq!(menu_size(&items).0, 30);
        let (buffer, _, _) = paint(26, 4, (0, 0), &items, MenuState::default(), false);
        // 内容 22 列：标签 12 + 间隔 2 + 快捷键 12 放不下，但标签还能保 8 列 → 截断
        // 标签（「前」放不下，写「关闭当…」7 列），快捷键仍在。
        assert_eq!(row_text(&buffer, 1), "│ 关闭当…   ctrl+shift+w │");
        let (buffer, _, _) = paint(20, 4, (0, 0), &items, MenuState::default(), false);
        assert_eq!(
            row_text(&buffer, 1),
            "│ 关闭当前窗格     │",
            "标签保不住 8 列时丢快捷键列"
        );
        let (_, render, _) = paint(20, 3, (0, 0), &items, MenuState::default(), false);
        assert_eq!(render.rows.len(), 1, "窗口外的行不画、不进命中表");
        let (_, render, _) = paint(2, 9, (0, 0), &items, MenuState::default(), false);
        assert_eq!(render, MenuRender::default());
        // 极窄：箭头列与勾选列让位，标签截断，边框不被覆盖。
        let (buffer, render, _) = paint(6, 9, (0, 0), &sample(), MenuState::default(), false);
        assert_eq!(render.area, Rect::new(0, 0, 6, 9));
        assert_eq!(row_text(&buffer, 3), "│ F… │", "勾选列让位");
        assert_eq!(row_text(&buffer, 4), "│ M… │", "箭头列让位");
        let (buffer, _, _) = paint(3, 9, (0, 0), &sample(), MenuState::default(), false);
        assert_eq!(row_text(&buffer, 4), "│ │");
    }

    /// 两组共 8 项：标题 + a1..a3、分隔线 + b1..b3。
    fn scrolling() -> [MenuItem<'static>; 8] {
        [
            MenuItem::header("Group"),
            MenuItem::action("a1"),
            MenuItem::action("a2"),
            MenuItem::action("a3"),
            MenuItem::separator(),
            MenuItem::action("b1"),
            MenuItem::action("b2"),
            MenuItem::action("b3"),
        ]
    }

    fn scrolled(highlighted: usize, scroll: usize) -> MenuState {
        MenuState {
            highlighted,
            scroll,
            ..MenuState::default()
        }
    }

    #[test]
    fn window_follows_a_highlight_past_the_visible_rows() {
        let items = scrolling();
        // 高 5 → 边框内 3 行。高亮 b2（6）越过窗口底：窗口下移到 4..=6。
        let (buffer, render, palette) = paint(14, 5, (0, 0), &items, scrolled(6, 0), false);
        assert_eq!(render.scroll, 4);
        assert_eq!(menu_scroll(&items, &scrolled(6, 0), 3), 4, "与渲染同口径");
        let rows: Vec<String> = (0..5).map(|y| row_text(&buffer, y)).collect();
        assert_eq!(
            rows,
            vec![
                "┌──────▲─────┐",
                "├────────────┤",
                "│ b1         │",
                "│ b2         │",
                "└──────▼─────┘",
            ]
        );
        assert_eq!(
            render
                .rows
                .iter()
                .map(|(rect, index)| (rect.y, *index))
                .collect::<Vec<_>>(),
            vec![(2, 5), (3, 6)],
            "只有窗口内的可激活项进命中表，行矩形按窗口偏移"
        );
        assert_eq!(
            buffer[(2, 3)].style().bg,
            Some(palette.accent),
            "高亮在窗口内"
        );
        assert_eq!(
            buffer[(7, 0)].style().fg,
            Some(palette.accent),
            "滚动提示随边框色"
        );

        // 高亮回到 a1 而 scroll 还停在 4：窗口上移，并带上正上方的分组标题。
        let (buffer, render, _) = paint(14, 5, (0, 0), &items, scrolled(1, 4), false);
        assert_eq!(render.scroll, 0);
        assert_eq!(row_text(&buffer, 0), "┌────────────┐", "上面没有更多项");
        assert_eq!(row_text(&buffer, 1), "│ Group      │");
        assert_eq!(row_text(&buffer, 4), "└──────▼─────┘");

        let (buffer, _, _) = paint(14, 5, (0, 0), &items, scrolled(6, 0), true);
        assert_eq!(row_text(&buffer, 0), "┌──────^─────┐", "ascii 降级");
        assert_eq!(row_text(&buffer, 4), "└──────v─────┘");
    }

    #[test]
    fn scroll_is_clamped_and_only_moved_to_reveal_the_highlight() {
        let items = scrolling();
        assert_eq!(
            menu_scroll(&items, &scrolled(5, 99), 3),
            5,
            "越界收回到最后一屏"
        );
        assert_eq!(
            menu_scroll(&items, &scrolled(3, 2), 3),
            2,
            "高亮已可见时尊重 scroll"
        );
        assert_eq!(
            menu_scroll(&items, &scrolled(usize::MAX, 3), 3),
            3,
            "没有键盘高亮时滚轮说了算"
        );
        assert_eq!(
            menu_scroll(&items, &scrolled(0, 3), 3),
            3,
            "高亮落在标题上不揭示"
        );
        assert_eq!(menu_scroll(&items, &scrolled(0, 0), 20), 0, "放得下时不滚");
        assert_eq!(menu_scroll(&[], &scrolled(0, 5), 3), 0);
        // 向上揭示连带的标题 / 分隔线以窗口放得下为限，高亮项本身始终可见。
        let stacked = [
            MenuItem::header("H"),
            MenuItem::separator(),
            MenuItem::header("H2"),
            MenuItem::action("a"),
            MenuItem::action("b"),
            MenuItem::action("c"),
        ];
        assert_eq!(menu_scroll(&stacked, &scrolled(3, 4), 2), 2);
    }

    #[test]
    fn keyboard_highlight_is_always_drawn_and_hit_testable() {
        let items = scrolling();
        let activatable = (0..items.len()).filter(|&index| items[index].is_activatable());
        for highlighted in activatable {
            for scroll in 0..=items.len() + 1 {
                let (buffer, render, palette) =
                    paint(14, 5, (0, 0), &items, scrolled(highlighted, scroll), false);
                let Some((rect, _)) = render.rows.iter().find(|(_, i)| *i == highlighted) else {
                    panic!("高亮 {highlighted} 在 scroll {scroll} 时没画出来");
                };
                assert_eq!(
                    buffer[(rect.x + 1, rect.y)].style().bg,
                    Some(palette.accent),
                    "高亮 {highlighted} / scroll {scroll}"
                );
            }
        }
    }

    #[test]
    fn badge_draws_an_accent_dot_before_the_shortcut_column() {
        let items = [
            MenuItem {
                shortcut: Some("^U"),
                badge: true,
                ..MenuItem::action("Updates")
            },
            MenuItem::action("Settings"),
            MenuItem {
                badge: true,
                enabled: false,
                ..MenuItem::action("Plugins")
            },
        ];
        // 内容：标签 8 + 徽标 2 + 间隔 2 + 快捷键 2 + 内边距 2 = 16，边框 +2。
        assert_eq!(menu_size(&items), (18, 5));
        let state = MenuState {
            highlighted: 1,
            hovered: Some(0),
            ..MenuState::default()
        };
        let (buffer, _, palette) = paint(18, 5, (0, 0), &items, state, false);
        assert_eq!(row_text(&buffer, 1), "│ Updates  ●  ^U │");
        assert_eq!(row_text(&buffer, 2), "│ Settings       │");
        assert_eq!(row_text(&buffer, 3), "│ Plugins  ●     │");
        let dot = buffer[(11, 1)].style();
        assert_eq!(dot.fg, Some(palette.accent), "徽标 accent");
        assert_eq!(dot.bg, Some(palette.hover_row_bg()), "随悬浮行底色");
        assert_eq!(
            buffer[(11, 3)].style().fg,
            Some(palette.overlay0),
            "禁用项的徽标随整行置灰"
        );

        let (buffer, _, palette) = paint(18, 5, (0, 0), &items, MenuState::default(), false);
        assert_eq!(
            buffer[(11, 1)].style(),
            buffer[(3, 1)].style(),
            "高亮行的徽标随整行反色"
        );
        assert_eq!(buffer[(11, 1)].style().bg, Some(palette.accent));

        let (buffer, _, _) = paint(18, 5, (0, 0), &items, state, true);
        assert_eq!(row_text(&buffer, 1), "│ Updates  o  ^U │", "ascii 降级");
        // 收窄：快捷键列先丢，徽标列保留；极窄时徽标列也让位。
        let (buffer, _, _) = paint(14, 5, (0, 0), &items, state, false);
        assert_eq!(row_text(&buffer, 1), "│ Updates  ● │");
        let (buffer, _, _) = paint(6, 5, (0, 0), &items, state, false);
        assert_eq!(row_text(&buffer, 1), "│ U… │");
        // 标题不画徽标，也不因此多占一列。
        let header = [MenuItem {
            badge: true,
            ..MenuItem::header("Long header text")
        }];
        assert_eq!(menu_size(&header), (20, 3));
    }

    #[test]
    fn step_skips_separators_headers_and_disabled_items_and_wraps() {
        let items = sample();
        assert_eq!(menu_step(&items, 1, 1), Some(2));
        assert_eq!(menu_step(&items, 3, 1), Some(6), "跳过禁用与分隔");
        assert_eq!(menu_step(&items, 6, 1), Some(1), "回绕并跳过标题");
        assert_eq!(menu_step(&items, 1, -1), Some(6));
        assert_eq!(menu_step(&items, 1, 2), Some(3));
        assert_eq!(menu_step(&items, 1, 0), Some(1));
        assert_eq!(menu_step(&items, 0, 0), Some(1), "停在标题上时前进");
        assert_eq!(menu_step(&items, usize::MAX, 1), Some(1));
        assert_eq!(menu_step(&items, usize::MAX, -1), Some(6));
        let inert = [MenuItem::header("x"), MenuItem::separator()];
        assert_eq!(menu_step(&inert, 0, 1), None);
        assert_eq!(menu_step(&[], 0, 1), None);
    }

    #[test]
    fn first_letter_matches_case_insensitively_and_cycles() {
        let items = [
            MenuItem::action("Focus"),
            MenuItem {
                enabled: false,
                ..MenuItem::action("Fork")
            },
            MenuItem::action("follow"),
            MenuItem::action("聚焦"),
        ];
        assert_eq!(menu_first_letter(&items, 'f', 0), Some(2));
        assert_eq!(menu_first_letter(&items, 'F', 2), Some(0), "回绕");
        assert_eq!(menu_first_letter(&items, '聚', 0), Some(3));
        assert_eq!(menu_first_letter(&items, 'x', 0), None);
        assert_eq!(menu_first_letter(&items, 'f', usize::MAX), Some(0));
        assert_eq!(menu_first_letter(&[], 'f', 0), None);
    }
}
