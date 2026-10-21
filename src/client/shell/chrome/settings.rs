//! 设置浮层的版式与绘制（Graphite 设计语言）：宽屏是「左侧分区导航 + 竖线 +
//! 右侧标题 / 灰色说明 / 选项行」，窄屏把导航收成会换行的标签行，所有分区始终
//! 可达。版式是纯函数（[`settings_layout`]、[`nav_items`]、[`list_body`]），视图
//! 计算、绘制与鼠标命中共用同一份几何；绘制只读入参，命中区作为返回值交回。
//!
//! 选中行用设计真源的中性底、粗体与首列竖条；键盘焦点所在的一侧（导航或
//! 正文）选中行用这一档，另一侧的当前分区退一档，焦点在哪一眼可见。颜色只表达
//! 语义：集成有更新时导航上的 `●` 用「需要处理」色。

use std::borrow::Cow;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

use super::super::ClientSettingsSection;
use super::readable_on;
use crate::app::state::{ComponentStyles, Palette};
use crate::ui::display_width_u16;

/// 设置浮层的目标尺寸（再被终端尺寸按模态边距夹紧）。
pub(in crate::client::shell) const SETTINGS_MODAL_SIZE: crate::ui::ModalSize =
    crate::ui::ModalSize::Content {
        width: 100,
        height: 28,
    };

pub(in crate::client::shell) fn modal_size(area: Rect) -> crate::ui::ModalSize {
    if area.width >= 140 {
        SETTINGS_MODAL_SIZE
    } else {
        crate::ui::ModalSize::Content {
            width: 76,
            height: 28,
        }
    }
}

/// 宽屏导航的最小内框宽度：导航列 + 竖线 + 至少 40 列正文。
const WIDE_MIN_WIDTH: u16 = 64;
/// 宽屏导航列宽的上下限（含左右留白）。
const NAV_MIN_WIDTH: u16 = 14;
const NAV_MAX_WIDTH: u16 = 22;
/// 正文与竖线之间、正文与右边框之间的留白。
const CONTENT_GAP_LEFT: u16 = 3;
const CONTENT_GAP_RIGHT: u16 = 2;

/// 导航的一项：分区、显示文字、是否带「有更新」标记。
#[derive(Debug, Clone, Copy)]
pub(in crate::client::shell) struct NavItem<'a> {
    pub section: ClientSettingsSection,
    pub label: &'a str,
    pub badge: bool,
}

/// 导航的两种形态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::client::shell) enum SettingsNav {
    /// 宽屏：左侧竖排，一行一个分区。
    Column(Rect),
    /// 窄屏：顶部标签行，放不下时换行。
    Tabs(Rect),
}

/// 设置浮层内框的版式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::client::shell) struct SettingsLayout {
    /// 标题行（已离边框留白）。
    pub header: Rect,
    pub nav: SettingsNav,
    /// 横向细分隔线所在的行（标题下方；宽屏还有按钮行上方一条）。
    pub top_rule: Rect,
    pub bottom_rule: Option<Rect>,
    /// 宽屏导航与正文之间的竖线。
    pub divider: Option<Rect>,
    pub content: Rect,
    /// 与操作按钮同一行的键位提示，空间不足时优先保留主动作。
    pub footer: Rect,
    /// 按钮行：按钮在这一行里靠右排。
    pub actions: Rect,
}

impl SettingsLayout {
    pub(in crate::client::shell) fn is_wide(&self) -> bool {
        matches!(self.nav, SettingsNav::Column(_))
    }
}

/// 导航项的显示文字：带更新标记时窄屏标签前加 `● `（宽屏标记画在行尾）。
fn tab_text(item: &NavItem<'_>) -> String {
    if item.badge {
        format!(" ● {} ", item.label)
    } else {
        format!(" {} ", item.label)
    }
}

/// 窄屏标签行的逐项矩形（相对 `area` 左上角起排，标签间空 1 列，放不下换行）。
fn tab_rects(area: Rect, items: &[NavItem<'_>]) -> Vec<Rect> {
    let mut x = area.x;
    let mut y = area.y;
    items
        .iter()
        .map(|item| {
            let width = display_width_u16(&tab_text(item)).min(area.width.max(1));
            if x > area.x && x.saturating_add(width) > area.right() {
                x = area.x;
                y = y.saturating_add(1);
            }
            let rect = Rect::new(x, y, width, 1);
            x = x.saturating_add(width.saturating_add(1));
            rect
        })
        .collect()
}

/// 设置浮层内框的版式；内框太小（< 20×8）时为 `None`。宽度够且高度能竖排下全部
/// 分区时用左侧导航，否则用会换行的标签行——两种形态下每个分区都有命中区。
pub(in crate::client::shell) fn settings_layout(
    inner: Rect,
    items: &[NavItem<'_>],
) -> Option<SettingsLayout> {
    if inner.width < 20 || inner.height < 8 {
        return None;
    }
    let header = Rect::new(inner.x + 1, inner.y, inner.width - 2, 1);
    let actions_y = inner.bottom() - 1;
    let item_count = u16::try_from(items.len()).unwrap_or(u16::MAX);
    if inner.width >= WIDE_MIN_WIDTH && inner.height >= item_count.saturating_add(5) {
        let label_width = items
            .iter()
            .map(|item| display_width_u16(item.label))
            .max()
            .unwrap_or(0);
        // 左 2 列留白、标签、行尾更新标记 2 列、右 1 列留白。
        let nav_width = label_width
            .saturating_add(5)
            .clamp(NAV_MIN_WIDTH, NAV_MAX_WIDTH);
        let body_y = inner.y + 2;
        let bottom_rule_y = actions_y - 1;
        let body_height = bottom_rule_y - body_y;
        let divider = Rect::new(inner.x + nav_width, body_y, 1, body_height);
        let content_x = divider.x + CONTENT_GAP_LEFT;
        let content = Rect::new(
            content_x,
            body_y + 1,
            inner
                .right()
                .saturating_sub(content_x.saturating_add(CONTENT_GAP_RIGHT)),
            body_height.saturating_sub(1),
        );
        let footer = Rect::new(inner.x + 1, actions_y, inner.width - 2, 1);
        return Some(SettingsLayout {
            header,
            nav: SettingsNav::Column(Rect::new(
                inner.x,
                body_y + 1,
                nav_width,
                body_height.saturating_sub(1),
            )),
            top_rule: Rect::new(inner.x, inner.y + 1, inner.width, 1),
            bottom_rule: Some(Rect::new(inner.x, bottom_rule_y, inner.width, 1)),
            divider: Some(divider),
            content,
            footer,
            actions: Rect::new(inner.x + 1, actions_y, inner.width - 2, 1),
        });
    }
    let tabs_area = Rect::new(inner.x + 1, inner.y + 1, inner.width - 2, inner.height);
    let tab_rows = tab_rects(tabs_area, items)
        .iter()
        .map(|rect| rect.bottom())
        .max()
        .map_or(1, |bottom| bottom - tabs_area.y);
    let nav = Rect::new(tabs_area.x, tabs_area.y, tabs_area.width, tab_rows);
    let top_rule = Rect::new(inner.x, nav.bottom(), inner.width, 1);
    let content_y = top_rule.bottom();
    let footer = Rect::new(inner.x + 1, actions_y, inner.width - 2, 1);
    let content = Rect::new(
        inner.x + 1,
        content_y,
        inner.width - 2,
        footer.y.saturating_sub(content_y),
    );
    Some(SettingsLayout {
        header,
        nav: SettingsNav::Tabs(nav),
        top_rule,
        bottom_rule: None,
        divider: None,
        content,
        footer,
        actions: Rect::new(inner.x + 1, actions_y, inner.width - 2, 1),
    })
}

/// 各导航项的命中矩形（与绘制同一口径）。宽屏一行一项；窄屏按标签行换行。
pub(in crate::client::shell) fn nav_items(
    layout: &SettingsLayout,
    items: &[NavItem<'_>],
) -> Vec<(Rect, ClientSettingsSection)> {
    match layout.nav {
        SettingsNav::Column(area) => items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| {
                let y = area.y.saturating_add(u16::try_from(index).ok()?);
                (y < area.bottom()).then(|| {
                    (
                        Rect::new(area.x, y, area.width.saturating_sub(1), 1),
                        item.section,
                    )
                })
            })
            .collect(),
        SettingsNav::Tabs(area) => tab_rects(area, items)
            .into_iter()
            .zip(items)
            .map(|(rect, item)| (rect, item.section))
            .collect(),
    }
}

/// 正文顶部「标题 + 灰色说明 + 空行」占的行数：矮区域先保标题与说明。
pub(in crate::client::shell) fn header_rows(content: Rect) -> u16 {
    if content.height >= 6 {
        3
    } else {
        content.height.min(2)
    }
}

/// 列表型分区（主题、快捷键、监控与各选项分区）在正文里的列表区。
pub(in crate::client::shell) fn list_body(content: Rect) -> Rect {
    let rows = header_rows(content);
    Rect::new(
        content.x,
        content.y.saturating_add(rows),
        content.width,
        content.height.saturating_sub(rows),
    )
}

/// 按显示宽度截断，放不下时以 `…` 收尾；宽字符不劈开。
pub(in crate::client::shell) fn truncate_with_ellipsis(text: &str, max_width: u16) -> String {
    if display_width_u16(text) <= max_width {
        return text.to_owned();
    }
    if max_width == 0 {
        return String::new();
    }
    let budget = max_width - 1;
    let mut used = 0u16;
    let mut out = String::new();
    for grapheme in unicode_segmentation::UnicodeSegmentation::graphemes(text, true) {
        let width = display_width_u16(grapheme);
        if used.saturating_add(width) > budget {
            break;
        }
        out.push_str(grapheme);
        used += width;
    }
    out.push('…');
    out
}

fn put(buffer: &mut Buffer, x: u16, y: u16, width: u16, text: &str, style: Style) {
    if width == 0 || y >= buffer.area.bottom() || x >= buffer.area.right() {
        return;
    }
    buffer.set_stringn(x, y, text, usize::from(width), style);
}

/// 选中行底色与设计真源一致。
pub(in crate::client::shell) fn selected_bg(palette: &Palette) -> Color {
    palette.selection_row_bg()
}

/// 焦点不在这一侧时「当前项」的弱底色。
fn resting_bg(palette: &Palette) -> Color {
    if Palette::row_bg_is_distinct(palette.panel_bg, palette.surface0) {
        palette.surface0
    } else {
        palette.panel_bg
    }
}

/// 悬浮行的底色：比常态亮一档、比选中暗一档。
fn hover_bg(palette: &Palette) -> Color {
    for candidate in [palette.surface_dim, palette.surface0] {
        if Palette::row_bg_is_distinct(palette.panel_bg, candidate) {
            return candidate;
        }
    }
    palette.panel_bg
}

/// 标题行、两条横线与竖线。
pub(in crate::client::shell) fn paint_frame(
    buffer: &mut Buffer,
    layout: &SettingsLayout,
    title: &str,
    palette: &Palette,
) {
    let base = Style::default().bg(palette.panel_bg);
    put(
        buffer,
        layout.header.x,
        layout.header.y,
        layout.header.width,
        title.trim(),
        base.fg(palette.text).add_modifier(Modifier::BOLD),
    );
    let rule = base.fg(palette.surface1);
    for line in std::iter::once(layout.top_rule).chain(layout.bottom_rule) {
        for x in line.x..line.right() {
            if x < buffer.area.right() && line.y < buffer.area.bottom() {
                buffer[(x, line.y)].set_symbol("─").set_style(rule);
            }
        }
    }
    if let Some(divider) = layout.divider {
        for y in divider.y..divider.bottom() {
            if divider.x < buffer.area.right() && y < buffer.area.bottom() {
                buffer[(divider.x, y)].set_symbol("│").set_style(rule);
            }
        }
    }
}

/// 导航：返回各项命中区。`hovered` 是指针所在的分区。
// Navigation geometry and presentation inputs remain separate from client state.
#[allow(clippy::too_many_arguments)]
pub(in crate::client::shell) fn paint_nav(
    buffer: &mut Buffer,
    layout: &SettingsLayout,
    items: &[NavItem<'_>],
    active: ClientSettingsSection,
    focused: bool,
    hovered: Option<ClientSettingsSection>,
    palette: &Palette,
    components: &ComponentStyles,
) -> Vec<(Rect, ClientSettingsSection)> {
    let hits = nav_items(layout, items);
    let wide = layout.is_wide();
    for ((rect, section), item) in hits.iter().zip(items) {
        let is_active = *section == active;
        let bg = if is_active {
            if focused {
                selected_bg(palette)
            } else {
                resting_bg(palette)
            }
        } else if hovered == Some(*section) {
            hover_bg(palette)
        } else {
            palette.panel_bg
        };
        let fg = if is_active {
            readable_on(palette, bg, &[palette.text])
        } else {
            readable_on(palette, bg, &[palette.subtext0, palette.text])
        };
        let mut style = Style::default().fg(fg).bg(bg);
        if is_active {
            style = style.add_modifier(Modifier::BOLD);
        }
        buffer.set_style(*rect, style);
        let badge_style = Style::default()
            .fg(components.status_input)
            .bg(bg)
            .add_modifier(Modifier::BOLD);
        if wide {
            let label_room = rect.width.saturating_sub(if item.badge { 5 } else { 3 });
            put(
                buffer,
                rect.x + 2,
                rect.y,
                label_room,
                &truncate_with_ellipsis(item.label, label_room),
                style,
            );
            if item.badge && rect.width >= 4 {
                put(buffer, rect.right() - 2, rect.y, 1, "●", badge_style);
            }
        } else {
            put(buffer, rect.x, rect.y, rect.width, &tab_text(item), style);
            if item.badge && rect.width >= 3 {
                put(buffer, rect.x + 1, rect.y, 1, "●", badge_style);
            }
        }
        if is_active && focused {
            crate::ui::kit::paint_selection_mark(buffer, rect.x, rect.y, palette);
        }
    }
    hits
}

/// 正文顶部的分区标题与灰色说明（放不下以 `…` 收尾）。
pub(in crate::client::shell) fn paint_section_header(
    buffer: &mut Buffer,
    content: Rect,
    title: &str,
    description: &str,
    palette: &Palette,
) {
    let rows = header_rows(content);
    let base = Style::default().bg(palette.panel_bg);
    if rows >= 1 {
        put(
            buffer,
            content.x,
            content.y,
            content.width,
            &truncate_with_ellipsis(title, content.width),
            base.fg(palette.text).add_modifier(Modifier::BOLD),
        );
    }
    if rows >= 2 {
        put(
            buffer,
            content.x,
            content.y + 1,
            content.width,
            &truncate_with_ellipsis(description, content.width),
            base.fg(palette.subtext0),
        );
    }
}

/// 选项 / 列表的一行：标签、灰色说明、可选的右侧键帽、是否当前值。
#[derive(Debug, Clone)]
pub(in crate::client::shell) struct OptionRow<'a> {
    pub label: Cow<'a, str>,
    pub detail: Cow<'a, str>,
    pub key: Option<Cow<'a, str>>,
    pub current: bool,
}

impl<'a> OptionRow<'a> {
    pub(in crate::client::shell) fn new(label: impl Into<Cow<'a, str>>) -> Self {
        Self {
            label: label.into(),
            detail: Cow::Borrowed(""),
            key: None,
            current: false,
        }
    }

    pub(in crate::client::shell) fn detail(mut self, detail: impl Into<Cow<'a, str>>) -> Self {
        self.detail = detail.into();
        self
    }

    pub(in crate::client::shell) fn key(mut self, key: impl Into<Cow<'a, str>>) -> Self {
        self.key = Some(key.into());
        self
    }

    pub(in crate::client::shell) fn current(mut self, current: bool) -> Self {
        self.current = current;
        self
    }
}

/// 选项行的绘制参数。
pub(in crate::client::shell) struct OptionList<'a> {
    pub rows: &'a [OptionRow<'a>],
    /// 首个可见行（列表滚动后的起点）。
    pub start: usize,
    pub selected: usize,
    /// 键盘焦点在正文：只有这时才画选中底色。
    pub focused: bool,
    pub hovered: Option<usize>,
    /// 行间空一行（选项少、正文高时更透气）。
    pub gap: bool,
}

/// 选项列表的行间距：每项占两行放得下时空一行。
pub(in crate::client::shell) fn option_gap(body: Rect, rows: usize) -> bool {
    usize::from(body.height) >= rows.saturating_mul(2)
}

/// 画选项列表，返回各可见行的命中区（行号是 `rows` 里的下标）。行首 2 列放当前值
/// 标记 `✓`；说明列在最长标签之后对齐，放不下就不画；键帽靠右。
pub(in crate::client::shell) fn paint_options(
    buffer: &mut Buffer,
    body: Rect,
    list: &OptionList<'_>,
    palette: &Palette,
) -> Vec<(Rect, usize)> {
    paint_options_with_previews(buffer, body, list, palette, &[])
}

pub(in crate::client::shell) fn paint_options_with_previews(
    buffer: &mut Buffer,
    body: Rect,
    list: &OptionList<'_>,
    palette: &Palette,
    previews: &[Vec<(&'static str, Color)>],
) -> Vec<(Rect, usize)> {
    let mut hits = Vec::new();
    if body.is_empty() {
        return hits;
    }
    let label_width = list
        .rows
        .iter()
        .map(|row| display_width_u16(&row.label))
        .max()
        .unwrap_or(0);
    let step = if list.gap { 2 } else { 1 };
    let visible = usize::from(body.height).div_ceil(step);
    for (offset, (index, row)) in list
        .rows
        .iter()
        .enumerate()
        .skip(list.start)
        .take(visible)
        .enumerate()
    {
        let y = body.y + u16::try_from(offset * step).unwrap_or(u16::MAX);
        if y >= body.bottom() {
            break;
        }
        let rect = Rect::new(body.x, y, body.width, 1);
        let selected = list.focused && index == list.selected;
        let bg = if selected {
            selected_bg(palette)
        } else if list.hovered == Some(index) {
            hover_bg(palette)
        } else {
            palette.panel_bg
        };
        let base = Style::default().bg(bg);
        buffer.set_style(rect, base);
        if selected {
            crate::ui::kit::paint_selection_mark(buffer, rect.x, y, palette);
        }
        let mut label_style = base.fg(readable_on(palette, bg, &[palette.text]));
        if selected || row.current {
            label_style = label_style.add_modifier(Modifier::BOLD);
        }
        if row.current {
            put(
                buffer,
                rect.x + 1,
                y,
                1,
                "✓",
                base.fg(palette.accent).add_modifier(Modifier::BOLD),
            );
        }
        let key_width = row
            .key
            .as_deref()
            .map_or(0, crate::ui::kit::key_cap::cap_width);
        let right = rect.right().saturating_sub(if key_width > 0 {
            key_width.saturating_add(1)
        } else {
            1
        });
        let label_x = rect.x + 3;
        let preview = previews.get(index).map_or(&[][..], Vec::as_slice);
        let preview_width = preview
            .iter()
            .map(|(text, _)| display_width_u16(text) + 1)
            .sum::<u16>();
        let preview_x = right.saturating_sub(preview_width);
        let show_preview = preview_width > 0 && preview_x >= label_x.saturating_add(8);
        let label_right = if show_preview {
            preview_x.saturating_sub(1)
        } else {
            right
        };
        let label_room = label_right.saturating_sub(label_x);
        put(
            buffer,
            label_x,
            y,
            label_room,
            &truncate_with_ellipsis(&row.label, label_room),
            label_style,
        );
        let detail_x = label_x.saturating_add(label_width).saturating_add(3);
        if !row.detail.is_empty() && detail_x.saturating_add(8) <= label_right {
            let room = label_right - detail_x;
            put(
                buffer,
                detail_x,
                y,
                room,
                &truncate_with_ellipsis(&row.detail, room),
                base.fg(readable_on(
                    palette,
                    bg,
                    &[palette.subtext0, palette.overlay1, palette.text],
                )),
            );
        }
        if show_preview {
            let mut x = preview_x;
            for (glyph, color) in preview {
                let width = display_width_u16(glyph);
                put(buffer, x, y, width, glyph, base.fg(*color));
                x += width + 1;
            }
        }
        if let Some(key) = row.key.as_deref() {
            if key_width > 0 && key_width < rect.width {
                crate::ui::kit::key_cap::paint_cap(
                    buffer,
                    rect.right() - key_width - 1,
                    y,
                    rect.right() - 1,
                    key,
                    crate::ui::design::key_cap(palette),
                );
            }
        }
        hits.push((rect, index));
    }
    hits
}

/// 页脚：左侧键位提示、右侧按钮。`buttons` 是（文字, 色调, 状态）；返回各按钮矩形
/// （与 `buttons` 同序）。按钮放不下时从左往右丢键位提示，按钮总在。
pub(in crate::client::shell) fn footer_button_rects(
    layout: &SettingsLayout,
    labels: &[&str],
) -> Vec<Rect> {
    let widths = labels
        .iter()
        .map(|label| crate::ui::modal_button_width(label))
        .collect::<Vec<_>>();
    let total =
        widths.iter().sum::<u16>() + u16::try_from(widths.len().saturating_sub(1)).unwrap_or(0);
    let row = layout.actions;
    let mut x = row.right().saturating_sub(total).max(row.x);
    widths
        .iter()
        .map(|width| {
            let rect = Rect::new(x, row.y, (*width).min(row.right().saturating_sub(x)), 1);
            x = x.saturating_add(width.saturating_add(1));
            rect
        })
        .collect()
}

/// 键位提示可用的区域：与按钮同一行时让出按钮及 2 列间隔。
pub(in crate::client::shell) fn footer_hint_area(
    layout: &SettingsLayout,
    buttons: &[Rect],
) -> Rect {
    let footer = layout.footer;
    if footer.height == 0 {
        return footer;
    }
    match buttons.first() {
        Some(first) if first.y == footer.y => Rect::new(
            footer.x,
            footer.y,
            first.x.saturating_sub(footer.x).saturating_sub(2),
            1,
        ),
        _ => footer,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn navigation_focus_marks_only_the_selected_section() {
        let palette = Palette::graphite_dark();
        let components = ComponentStyles::from_palette(&palette);
        for cols in [60, 80, 160] {
            let area = Rect::new(0, 0, cols, 24);
            let layout = settings_layout(area, &items()).expect("layout");
            let mut buffer = Buffer::empty(area);
            let hits = paint_nav(
                &mut buffer,
                &layout,
                &items(),
                ClientSettingsSection::Theme,
                true,
                Some(ClientSettingsSection::Sound),
                &palette,
                &components,
            );
            for (rect, section) in hits {
                if section == ClientSettingsSection::Theme {
                    assert_eq!(
                        buffer[(rect.x, rect.y)].symbol(),
                        crate::ui::design::SELECTION_MARK
                    );
                    assert_eq!(buffer[(rect.x, rect.y)].bg, palette.selection_row_bg());
                } else {
                    assert_ne!(
                        buffer[(rect.x, rect.y)].symbol(),
                        crate::ui::design::SELECTION_MARK
                    );
                }
            }
            assert_eq!(layout.footer.height, 1);
            assert_eq!(layout.footer.y, layout.actions.y);
        }
    }

    fn items() -> Vec<NavItem<'static>> {
        ClientSettingsSection::ALL
            .iter()
            .map(|section| NavItem {
                section: *section,
                label: match section {
                    ClientSettingsSection::Language => "通用",
                    ClientSettingsSection::Theme => "外观",
                    ClientSettingsSection::Indicators => "指示器",
                    ClientSettingsSection::Toast => "通知",
                    ClientSettingsSection::Sound => "声音",
                    ClientSettingsSection::Integrations => "集成",
                    ClientSettingsSection::Keyboard => "快捷键",
                    ClientSettingsSection::Monitor => "监控",
                },
                badge: *section == ClientSettingsSection::Integrations,
            })
            .collect()
    }

    fn inner_for(cols: u16, rows: u16) -> Rect {
        let outer =
            crate::ui::modal_rect(Rect::new(0, 0, cols, rows), SETTINGS_MODAL_SIZE).expect("modal");
        Rect::new(outer.x + 1, outer.y + 1, outer.width - 2, outer.height - 2)
    }

    /// 宽屏是左侧竖排导航；80 列仍然放得下竖排，60 列切到标签行。
    #[test]
    fn navigation_switches_to_tabs_at_narrow_widths() {
        let items = items();
        for (cols, rows, wide) in [(160, 45, true), (80, 24, true), (60, 20, false)] {
            let layout = settings_layout(inner_for(cols, rows), &items).expect("layout");
            assert_eq!(layout.is_wide(), wide, "{cols}x{rows}");
        }
        // 宽但矮：竖排放不下全部分区时也退到标签行。
        let short = settings_layout(Rect::new(0, 0, 90, 10), &items).expect("layout");
        assert!(!short.is_wide());
    }

    /// 每种尺寸下每个分区都有命中区，命中区互不重叠、都在内框里，且都在正文上方或
    /// 左侧（不压正文）。
    #[test]
    fn every_section_stays_reachable_down_to_sixty_columns() {
        let items = items();
        for (cols, rows) in [(160, 45), (100, 30), (80, 24), (60, 20), (60, 16)] {
            let inner = inner_for(cols, rows);
            let layout = settings_layout(inner, &items).expect("layout");
            let hits = nav_items(&layout, &items);
            assert_eq!(
                hits.len(),
                ClientSettingsSection::ALL.len(),
                "{cols}x{rows}"
            );
            for (index, (rect, section)) in hits.iter().enumerate() {
                assert_eq!(*section, ClientSettingsSection::ALL[index]);
                assert!(rect.width > 0 && inner.contains((rect.x, rect.y).into()));
                assert!(rect.right() <= inner.right(), "{cols}x{rows} {section:?}");
                assert!(
                    !rect.intersects(layout.content),
                    "{cols}x{rows} {section:?}"
                );
                for (other, _) in hits.iter().skip(index + 1) {
                    assert!(!rect.intersects(*other), "{cols}x{rows} {section:?}");
                }
            }
            assert!(layout.content.height >= 4, "{cols}x{rows} 正文至少 4 行");
            assert!(layout.actions.bottom() <= inner.bottom());
        }
    }

    #[test]
    fn ellipsis_truncation_never_splits_wide_characters() {
        assert_eq!(truncate_with_ellipsis("hello", 5), "hello");
        assert_eq!(truncate_with_ellipsis("hello world", 6), "hello…");
        assert_eq!(truncate_with_ellipsis("界面主题说明", 6), "界面…");
        assert_eq!(truncate_with_ellipsis("abc", 0), "");
    }

    #[test]
    fn ellipsis_truncation_preserves_combining_and_joined_graphemes() {
        assert_eq!(truncate_with_ellipsis("e\u{301}clair", 2), "e\u{301}…");
        assert_eq!(truncate_with_ellipsis("👩‍💻abcd", 3), "👩‍💻…");
        assert_eq!(truncate_with_ellipsis("👩‍💻abcd", 2), "…");
        assert_eq!(truncate_with_ellipsis("🇨🇳abcd", 3), "🇨🇳…");
    }

    /// 选中行用设计底色、粗体和竖条，当前值带 `✓`，说明列是灰色。
    #[test]
    fn option_rows_mark_selection_current_value_and_grey_details() {
        let palette = Palette::graphite_dark();
        let area = Rect::new(0, 0, 40, 4);
        let mut buffer = Buffer::empty(area);
        let rows = [
            OptionRow::new("on").detail("chime").current(true),
            OptionRow::new("off").detail("quiet"),
        ];
        let hits = paint_options(
            &mut buffer,
            area,
            &OptionList {
                rows: &rows,
                start: 0,
                selected: 1,
                focused: true,
                hovered: None,
                gap: false,
            },
            &palette,
        );
        assert_eq!(hits.len(), 2);
        assert_eq!(buffer[(1, 0)].symbol(), "✓");
        assert_eq!(buffer[(3, 1)].bg, palette.selection_row_bg());
        assert_eq!(buffer[(0, 1)].symbol(), crate::ui::design::SELECTION_MARK);
        assert!(buffer[(3, 1)].modifier.contains(Modifier::BOLD));
        assert_eq!(buffer[(9, 0)].symbol(), "c");
        assert_eq!(buffer[(9, 0)].fg, palette.subtext0);
    }
}
