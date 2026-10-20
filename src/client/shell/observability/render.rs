//! 监控面板的渲染纯函数：本文件放页面骨架（边框、页签、页脚、进程对话框
//! 的挂载）与各页共用的小工具；系统页 / 设置页各在 `render/{system,settings}.rs`。

use std::borrow::Cow;

use super::*;
use ratatui::style::Color;

use super::super::feedback::ChromeContext;

mod settings;
mod system;
mod tone;

use settings::*;
use system::*;

pub(super) use system::CardScrollLimits;

/// 页面级滚动的度量：本次绘制算出，`State::commit_paint` 写回，`State::scroll_page`
/// 按它写回式钳位——与渲染时的钳位是同一个值。系统页的单位是「卡片行」（单列
/// 一行一张卡、双列一行两张），偏好页的单位是文本行。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(in crate::client::shell) struct PageScroll {
    /// 滚动位置上界：系统页是让最后一行卡片完整露出的最小起始行，偏好页是
    /// 总行数 − 视口高；内容放得下时为 0。
    pub max: usize,
    /// 一屏的步长（PageUp / PageDown），至少 1。
    pub screen: usize,
}

/// 各页本次绘制的 `PageScroll`；没画出来的页为 `None`。定长、`Copy`，渲染期不分配。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(in crate::client::shell) struct PageScrollLimits {
    monitor: Option<PageScroll>,
    settings: Option<PageScroll>,
}

impl PageScrollLimits {
    /// `page` 本次绘制的滚动度量。
    pub(in crate::client::shell) fn get(&self, page: Page) -> Option<PageScroll> {
        match page {
            Page::Monitor => self.monitor,
            Page::Settings => self.settings,
        }
    }

    fn set(&mut self, page: Page, scroll: Option<PageScroll>) {
        match page {
            Page::Monitor => self.monitor = scroll,
            Page::Settings => self.settings = scroll,
        }
    }

    /// 合并同一帧里另一次绘制的度量（多个停靠面板依次绘制）：只覆盖对方画出
    /// 来的页。
    pub(in crate::client::shell::observability) fn merge(&mut self, other: &Self) {
        if other.monitor.is_some() {
            self.monitor = other.monitor;
        }
        if other.settings.is_some() {
            self.settings = other.settings;
        }
    }
}

fn text(buffer: &mut Buffer, rect: Rect, row: u16, value: &str, style: Style) {
    if row >= rect.height || rect.width == 0 {
        return;
    }
    let value = value
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>();
    // Clip with an ellipsis instead of a hard cut so panel edges never
    // swallow the tail of a long status message；截断走共享实现，CJK 边界
    // 修复不必在每个自建组件里重复一遍（C-29）。
    let value = crate::ui::truncate_end(&value, usize::from(rect.width));
    buffer.set_stringn(rect.x, rect.y + row, value, rect.width as usize, style);
}

/// 带标题的面板：走共享的 `overlays::titled_panel`，边框色取组件 token
/// （`components.pane_border_focused`），与其它浮层同一种边框语言；字形表
/// 尊重 `ui.border_style`。
fn block(buffer: &mut Buffer, rect: Rect, title: &str, cx: &ChromeContext<'_>) -> Rect {
    let palette = cx.palette;
    let Some(inner) = super::super::render::titled_panel(
        buffer,
        rect,
        title,
        cx.components.pane_border_focused,
        palette.panel_bg,
        cx.glyphs,
    ) else {
        return Rect::default();
    };
    // 面板正文底：字色与底色是内容默认值，正文自己的样式优先。
    buffer.set_style(
        inner,
        Style::default()
            .fg(palette.text)
            .bg(palette.panel_bg)
            .remove_modifier(Modifier::DIM),
    );
    inner
}

/// 面板按钮：共用 `overlays::modal_button` 的语义色与状态表；`tone` 区分
/// 主操作与破坏性操作（结束进程 = Danger）。
fn button(
    buffer: &mut Buffer,
    rect: Rect,
    label: &str,
    tone: crate::ui::ModalButtonTone,
    state: crate::ui::ModalButtonState,
    action: Action,
    palette: &Palette,
    hits: &mut Vec<(Rect, Action)>,
) {
    let label = format!(" {label} ");
    let width = crate::ui::modal_button_width(&label).min(rect.width);
    let rect = Rect::new(rect.x, rect.y, width, rect.height.min(1));
    super::super::render::modal_button(buffer, rect, &label, tone, state, palette);
    if !rect.is_empty() {
        hits.push((rect, action));
    }
}

/// 常态（非破坏性）按钮：面板里绝大多数按钮都是这一档。
fn secondary_button(
    buffer: &mut Buffer,
    rect: Rect,
    label: &str,
    action: Action,
    palette: &Palette,
    hits: &mut Vec<(Rect, Action)>,
) {
    button(
        buffer,
        rect,
        label,
        crate::ui::ModalButtonTone::Secondary,
        crate::ui::ModalButtonState::Normal,
        action,
        palette,
        hits,
    );
}

/// 页签标签按可用宽度逐级截短。kit 的 `render_tabs` 遇到放不下的项整体丢弃：
/// 被丢弃的页签既不画也不登记命中区，鼠标从此没有路径进入那一页，当前页正好
/// 被丢弃时三个页签还会一个都不高亮。herdr 是 mouse-first TUI，所以窄面板下
/// 先把最长的标签截短（英文「Monitor preferences」比中文长得多），保证每个
/// 页签都在。返回值与 `labels` 一一对应，放得下时原样借用、不分配。
fn fit_tab_labels<'a, const N: usize>(labels: [&'a str; N], width: u16) -> [Cow<'a, str>; N] {
    let full: [u16; N] = labels.map(crate::ui::display_width_u16);
    // 与 `render_tabs` 的排版一致：每项左右各一列内边距，项与项之间一列间隔。
    let count = N as u16;
    let chrome = count
        .saturating_mul(2)
        .saturating_add(count.saturating_sub(1));
    let budget = u32::from(width.saturating_sub(chrome));
    let mut fitted = full;
    let mut total: u32 = full.iter().map(|width| u32::from(*width)).sum();
    while total > budget {
        // 每次削最长的那个，短标签先保住；谁都削不动（都只剩 1 列）就收手，
        // 这种宽度下整行本来也画不出。
        let Some(index) = fitted
            .iter()
            .enumerate()
            .filter(|(_, width)| **width > 1)
            .max_by_key(|(_, width)| **width)
            .map(|(index, _)| index)
        else {
            break;
        };
        fitted[index] -= 1;
        total -= 1;
    }
    std::array::from_fn(|index| {
        if fitted[index] >= full[index] {
            Cow::Borrowed(labels[index])
        } else {
            Cow::Owned(crate::ui::truncate_end(
                labels[index],
                usize::from(fitted[index]),
            ))
        }
    })
}

/// 页脚键位提示（kit `footer_hints`，可点）：系统页有「刷新」与「暂停 / 继续」；
/// 编辑布局模式下换成移动卡片的说明与「完成」。键帽与标签和状态栏同一套样式
/// （`chrome::bar_key_cap` / `chrome::bar_hint_label`），按面板底色挑对比度。
fn footer_hints(
    buffer: &mut Buffer,
    area: Rect,
    state: &State,
    page: Page,
    palette: &Palette,
    hits: &mut Vec<(Rect, Action)>,
) {
    use crate::ui::kit::footer_hints::{render_footer_hints_styled, FooterHint, FooterHintStyles};
    let texts = &crate::i18n::texts().monitor;
    let hint = |key, label, enabled, primary| FooterHint {
        key,
        label,
        enabled,
        primary,
    };
    let (hints, actions): (Vec<FooterHint<'_>>, Vec<Option<Action>>) =
        if page == Page::Monitor && state.layout_editing {
            (
                vec![
                    hint(
                        "↑↓",
                        texts.edit_layout_hint,
                        state.selected_card.is_some(),
                        false,
                    ),
                    hint("Esc", texts.edit_layout_done, true, true),
                ],
                vec![None, Some(Action::EditLayout)],
            )
        } else {
            let mut hints = Vec::with_capacity(3);
            let mut actions = Vec::with_capacity(3);
            // 监控偏好页只有本机偏好控件，没有可刷新的内容，不提示「r 刷新」（冒烟
            // L7）。
            if page == Page::Monitor {
                hints.push(hint("r", texts.hint_refresh, true, false));
                actions.push(Some(Action::Refresh));
                let label = if state.paused {
                    texts.hint_resume
                } else {
                    texts.hint_pause
                };
                hints.push(hint("Space", label, true, false));
                actions.push(Some(Action::Pause));
            }
            hints.push(hint("Esc", texts.hint_close, true, false));
            actions.push(Some(Action::Close));
            (hints, actions)
        };
    let styles = FooterHintStyles {
        key: super::super::chrome::bar_key_cap(palette, palette.panel_bg),
        label: super::super::chrome::bar_hint_label(palette, palette.panel_bg),
        hover_bg: palette.hover_row_bg(),
    };
    for (rect, index) in render_footer_hints_styled(buffer, area, &hints, None, palette, styles) {
        if let Some(action) = actions.get(index).cloned().flatten() {
            hits.push((rect, action));
        }
    }
}

fn bytes(value: u64) -> String {
    let value = value as f64;
    for (size, suffix) in [
        (1_099_511_627_776.0, "TiB"),
        (1_073_741_824.0, "GiB"),
        (1_048_576.0, "MiB"),
        (1024.0, "KiB"),
    ] {
        if value >= size {
            return format!("{:.1} {suffix}", value / size);
        }
    }
    format!("{value:.0} B")
}

fn percent(value: Option<f32>) -> String {
    value
        .map(|v| format!("{v:.1}%"))
        .unwrap_or_else(|| "—".into())
}

pub(super) fn status(status: ObservationStatus) -> &'static str {
    match status {
        ObservationStatus::Ready => tr("Current", "已更新"),
        ObservationStatus::Warming => tr("Loading", "采集中"),
        ObservationStatus::Unsupported => tr("Not supported", "暂不支持"),
        ObservationStatus::PermissionDenied => tr("Permission required", "缺少权限"),
        ObservationStatus::Unavailable => tr("Unavailable", "不可用"),
        ObservationStatus::Stale => tr("Cached", "缓存数据"),
        ObservationStatus::Error => tr("Query failed", "查询失败"),
        ObservationStatus::Unknown => tr("Unknown", "未知"),
    }
}

/// 采集状态的颜色，与状态语义（`ui::design`）同一套：已更新 = 待查看绿、采集中 =
/// 运行中蓝、缺少权限 = 需要你处理的琥珀、查询失败 = 失败红；暂不支持、不可用、
/// 缓存与未知只是事实陈述，用元信息灰，不报警。
pub(super) fn status_color(status: ObservationStatus, palette: &Palette) -> ratatui::style::Color {
    match status {
        ObservationStatus::Ready => palette.green,
        ObservationStatus::Warming => palette.blue,
        ObservationStatus::PermissionDenied => palette.yellow,
        ObservationStatus::Error => palette.red,
        ObservationStatus::Unavailable
        | ObservationStatus::Unsupported
        | ObservationStatus::Stale
        | ObservationStatus::Unknown => palette.overlay1,
    }
}

/// 时长文本：`6d21h` / `3h02m` / `13m` / `45s`。
fn span_text(seconds: u64) -> String {
    if seconds >= 86_400 {
        format!("{}d{:02}h", seconds / 86_400, seconds % 86_400 / 3600)
    } else if seconds >= 3600 {
        format!("{}h{:02}m", seconds / 3600, seconds % 3600 / 60)
    } else if seconds >= 60 {
        format!("{}m", seconds / 60)
    } else {
        format!("{seconds}s")
    }
}

/// 新鲜度文案「13m 前更新」；≥30 min 或 `Stale` 追加 ⚠，从未观测过时说明「尚未更新」。
fn age_text(now_ms: u64, observed_at_ms: u64, status: ObservationStatus) -> String {
    let texts = &crate::i18n::texts().monitor;
    if observed_at_ms == 0 {
        return texts.never_updated.to_owned();
    }
    let seconds = now_ms.saturating_sub(observed_at_ms) / 1000;
    let mut line = crate::i18n::fill(texts.updated_ago_fmt, &[("age", &span_text(seconds))]);
    if seconds >= 1800 || status == ObservationStatus::Stale {
        line.push_str(" ⚠");
    }
    line
}

/// 一次绘制产生的矩形与命中区；未绘制的部分保持 `Rect::default()`。
pub(super) struct PaintOutput {
    /// 页面（或进程对话框）的命中区。
    pub hits: Vec<(Rect, Action)>,
    /// 页面铺满的矩形（本次传入 `page` 时等于 `area`）。
    pub page_rect: Rect,
    pub dialog_rect: Rect,
    /// 系统页各可滚动卡片的滚动上界（本次没画系统页时全为 `None`）。
    pub card_scroll_limits: CardScrollLimits,
    /// 本次画出的页面的页面级滚动度量（没画页面时全为 `None`）。
    pub page_scroll_limits: PageScrollLimits,
}

/// 渲染纯函数：`page` 是本次要画的页面（停靠面板由调用方决定画哪个 tab），
/// 状态只读；`framed` 为真是经典布局 pass（页面自带外框与标题），进程对话框总是
/// 最后覆盖。调色板、组件 token 与边框字形都来自 `ChromeContext`（与浮层同源，C-29）。
pub(super) fn paint(
    buffer: &mut Buffer,
    area: Rect,
    state: &State,
    cx: &ChromeContext<'_>,
    page: Option<Page>,
    framed: bool,
) -> PaintOutput {
    let palette = cx.palette;
    let mut hits = Vec::new();
    let mut page_rect = Rect::default();
    let mut card_scroll_limits = CardScrollLimits::default();
    let mut page_scroll_limits = PageScrollLimits::default();
    if let Some(page) = page {
        page_rect = area;
        buffer.set_style(area, Style::default().fg(palette.text).bg(palette.panel_bg));
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                buffer[(x, y)].set_symbol(" ");
            }
        }
        // 经典布局（页面铺满 pane 区，即 `framed` 为真）没有别的 chrome，页面自带
        // 外框并在边框上写一次标题。停靠面板已有表头与分隔线：页面不再自带外框
        // （否则分隔线、页面框、卡片框挤成「│││」），左右各留一列内边距，原先
        // 边框占的顶边与底边两行让给正文。
        let inner = if framed {
            block(buffer, area, tr(" MONITOR ", " 监控 "), cx)
        } else {
            buffer.set_style(area, Style::default().remove_modifier(Modifier::DIM));
            Rect::new(
                area.x.saturating_add(1),
                area.y,
                area.width.saturating_sub(2),
                area.height,
            )
        };
        // Single navigation level: page tabs only. Refresh/pause/close live
        // on keyboard shortcuts (see footer) so the row never mixes
        // navigation with actions. 页签走 kit `tabs` 的分段式标签条：活动页是
        // 浅一档的结构面上的粗体正文，其余说明色字（Graphite，不反色）。
        let texts = &crate::i18n::texts().monitor;
        let tabs = [
            (texts.tab_system, Page::Monitor),
            (texts.tab_preferences, Page::Settings),
        ];
        let labels = fit_tab_labels(tabs.map(|(label, _)| label), inner.width);
        let items = labels
            .each_ref()
            .map(|label| crate::ui::kit::tabs::TabItem::new(label));
        let active = tabs.iter().position(|(_, tab)| *tab == page).unwrap_or(0);
        let rects = crate::ui::kit::tabs::render_tabs(
            buffer,
            Rect::new(inner.x, inner.y, inner.width, inner.height.min(1)),
            &items,
            active,
            None,
            palette,
        );
        let mut x = inner.x;
        for (rect, (_, tab)) in rects.iter().zip(tabs) {
            if !rect.is_empty() {
                hits.push((*rect, Action::Page(tab)));
                x = rect.right().saturating_add(1);
            }
        }
        // 暂停是需要留意的状态（历史停止记录），用「需要你处理」的琥珀色小标签。
        if state.paused {
            let label = tr("‖ paused", "‖ 已暂停");
            let width = UnicodeWidthStr::width(label) as u16;
            let room = inner.right().saturating_sub(x.saturating_add(1));
            if room >= width {
                let rect = Rect::new(inner.right() - width, inner.y, width, 1);
                text(
                    buffer,
                    rect,
                    0,
                    label,
                    Style::default()
                        .fg(palette.yellow)
                        .add_modifier(Modifier::BOLD),
                );
            }
        }
        let body = Rect::new(
            inner.x,
            inner.y.saturating_add(2),
            inner.width,
            inner.height.saturating_sub(3),
        );
        let scroll = match page {
            Page::Monitor => monitor(buffer, body, state, cx, &mut hits, &mut card_scroll_limits),
            Page::Settings => settings(buffer, body, state, palette, &mut hits),
        };
        page_scroll_limits.set(page, scroll);
        // 页脚：一次性说明（`message`）优先，否则是可点的键位提示。
        let footer = Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1);
        match state.message.as_deref() {
            Some(message) => text(
                buffer,
                footer,
                0,
                message,
                crate::ui::design::muted(palette),
            ),
            None => footer_hints(buffer, footer, state, page, palette, &mut hits),
        }
    }
    let mut dialog_rect = Rect::default();
    if let Some(dialog) = &state.process_dialog {
        hits.clear();
        dialog_rect = process_dialog(buffer, dialog, state, cx, &mut hits);
    }
    if let Some(hovered) = state.hovered_action.as_ref() {
        if let Some((rect, action)) = hits.iter().rev().find(|(_, action)| action == hovered) {
            let style = if matches!(action, Action::Terminate(_) | Action::ConfirmProcess) {
                Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
            } else {
                Style::default().bg(cx.components.hover_bg)
            };
            buffer.set_style(*rect, style);
        }
    }
    PaintOutput {
        hits,
        page_rect,
        dialog_rect,
        card_scroll_limits,
        page_scroll_limits,
    }
}

/// 各页测试共用的绘制与缓冲区断言辅助（`render/*.rs` 的测试用
/// `super::super::test_support::*`）。
#[cfg(test)]
pub(super) mod test_support {
    use super::*;
    use crate::client::shell::ClientShellConfig;
    use crate::config::Config;

    pub(super) fn config() -> ClientShellConfig {
        ClientShellConfig::from_config(&Config::default())
    }

    /// 测试用的组件上下文：只需要调色板、组件 token 与字形。
    pub(super) fn chrome_context(config: &ClientShellConfig) -> ChromeContext<'_> {
        ChromeContext {
            page_bounds: None,
            palette: &config.palette,
            components: &config.components,
            glyphs: config.border_glyphs,
            hover: None,
            spinner: "",
            now: std::time::Instant::now(),
        }
    }

    /// 按停靠面板 pass（不画悬浮层、边框无标题）画一页。
    pub(super) fn paint_page(
        state: &State,
        page: Page,
        width: u16,
        height: u16,
    ) -> (Buffer, PaintOutput) {
        let area = Rect::new(0, 0, width, height);
        let mut buffer = Buffer::empty(area);
        let config = config();
        let cx = chrome_context(&config);
        let output = paint(&mut buffer, area, state, &cx, Some(page), false);
        (buffer, output)
    }

    pub(super) fn row_text(buffer: &Buffer, y: u16) -> String {
        (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol().to_owned())
            .collect::<String>()
    }

    pub(super) fn buffer_text(buffer: &Buffer) -> String {
        (0..buffer.area.height)
            .map(|y| row_text(buffer, y))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 某行是否含 `needle`：宽字符占两个单元格、续格是空格，两边都去掉空格再比。
    pub(super) fn row_has(buffer: &Buffer, y: u16, needle: &str) -> bool {
        row_text(buffer, y)
            .replace(' ', "")
            .contains(&needle.replace(' ', ""))
    }

    pub(super) fn buffer_has(buffer: &Buffer, needle: &str) -> bool {
        (0..buffer.area.height).any(|y| row_has(buffer, y, needle))
    }

    /// 某行里 `needle` 首字符所在单元格的前景色（宽字符占两个单元格，按符号逐格找）。
    pub(super) fn color_at(buffer: &Buffer, y: u16, needle: &str) -> Option<Color> {
        let first = needle.chars().next()?.to_string();
        let x = (0..buffer.area.width).find(|x| {
            buffer[(*x, y)].symbol() == first
                && row_text(buffer, y)
                    .replace(' ', "")
                    .contains(&needle.replace(' ', ""))
        })?;
        buffer[(x, y)].style().fg
    }

    pub(super) fn contains_rect(outer: Rect, inner: Rect) -> bool {
        inner.x >= outer.x
            && inner.y >= outer.y
            && inner.right() <= outer.right()
            && inner.bottom() <= outer.bottom()
    }

    pub(super) fn has(output: &PaintOutput, wanted: impl Fn(&Action) -> bool) -> bool {
        output.hits.iter().any(|(_, action)| wanted(action))
    }

    /// `output` 里第一个满足 `wanted` 的命中矩形。
    pub(super) fn hit_rect(output: &PaintOutput, wanted: impl Fn(&Action) -> bool) -> Option<Rect> {
        output
            .hits
            .iter()
            .find(|(_, action)| wanted(action))
            .map(|(rect, _)| *rect)
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use crate::client::shell::ClientShellConfig;
    use crate::config::Config;
    use crate::i18n::{lang_guard, Lang};

    fn plain_state() -> State {
        let mut state = State::new(&config());
        state.now_ms = 14_000;
        state
    }

    #[test]
    fn monitor_hover_uses_control_identity_without_changing_keyboard_selection() {
        let mut state = plain_state();
        let (_, output) = paint_page(&state, Page::Monitor, 100, 30);
        state.hits = output.hits;
        state.selected_hit = 1;
        let rect = state.hits[0].0;
        let point = (rect.x + 1, rect.y);
        assert!(state.update_hover(point, true));
        assert_eq!(state.hovered_action, Some(Action::Page(Page::Monitor)));
        assert_eq!(state.selected_hit, 1);
        assert!(!state.update_hover(point, true));
        let (buffer, _) = paint_page(&state, Page::Monitor, 100, 30);
        assert_eq!(buffer[point].style().bg, Some(config().components.hover_bg));
        assert!(state.update_hover(point, false));
        assert_eq!(state.hovered_action, None);
        assert_eq!(state.selected_hit, 1);
    }

    #[test]
    fn monitor_hover_skips_card_regions_and_prefers_nested_controls() {
        let mut state = plain_state();
        state.hits = vec![
            (Rect::new(0, 0, 40, 12), Action::Card("cpu".into())),
            (Rect::new(2, 3, 6, 1), Action::Core(2)),
        ];
        assert!(!state.update_hover((1, 1), true));
        assert!(state.update_hover((3, 3), true));
        assert_eq!(state.hovered_action, Some(Action::Core(2)));
        assert!(state.update_hover((60, 20), true));
        assert_eq!(state.hovered_action, None);
    }

    /// 未确认的进程对话框：Terminate / Force 都该是破坏性语义。
    fn process_dialog_state() -> State {
        let mut state = State::new(&config());
        state.now_ms = 14_000;
        state.process_dialog = Some(ProcessDialog {
            process: ProcessMetric {
                identity: ProcessIdentity {
                    pid: 4242,
                    started_at: 1_700_000_000,
                    boot_id: "boot-1".into(),
                    instance_token: None,
                },
                parent_pid: Some(1),
                name: "herdr-test".into(),
                cpu_percent: Some(12.5),
                memory_bytes: 64 * 1024 * 1024,
                status: "Running".into(),
                user: Some("tester".into()),
                protected: false,
                action_token: Some("token-1".into()),
                executable: Some("/usr/bin/herdr-test".into()),
            },
            force: false,
            confirm: false,
            pending: false,
        });
        state
    }

    /// 带 `needle` 的那一行里，该矩形起点处的底色。
    fn rect_bg(buffer: &Buffer, rect: Rect, needle: &str) -> Color {
        let y = (rect.y..rect.bottom())
            .find(|y| row_has(buffer, *y, needle))
            .unwrap_or_else(|| panic!("{needle} 未出现在对话框里"));
        buffer[(rect.x, y)].style().bg.unwrap_or(Color::Reset)
    }

    /// C-29：面板边框与其它浮层同源——字形取 `ui.border_style`，颜色取
    /// `components.pane_border_focused`，不再自带一套 `surface1` 边框语言。
    #[test]
    fn page_border_uses_the_component_token_and_the_border_style() {
        for (style, expect_round) in [
            (crate::config::BorderStyleConfig::Rounded, true),
            (crate::config::BorderStyleConfig::Double, false),
        ] {
            let mut raw = Config::default();
            raw.ui.border_style = style;
            let config = ClientShellConfig::from_config(&raw);
            let cx = chrome_context(&config);
            let area = Rect::new(0, 0, 100, 30);
            // 经典布局 pass（`framed` 为真）：没有停靠表头，边框上写标题。
            let mut buffer = Buffer::empty(area);
            paint(
                &mut buffer,
                area,
                &plain_state(),
                &cx,
                Some(Page::Monitor),
                true,
            );
            let corner = buffer[(0, 0)].clone();
            assert_eq!(
                corner.symbol(),
                cx.glyphs.top_left,
                "面板左上角字形应跟随 ui.border_style（{style:?}）"
            );
            assert_eq!(
                corner.style().fg,
                Some(cx.components.pane_border_focused),
                "面板边框色应取组件 token（{style:?}）"
            );
            // 宽字符续格在 ratatui 里会被 reset，取右侧/底边这类纯边框格核对。
            for (x, y) in [(99, 10), (50, 29)] {
                assert_eq!(
                    buffer[(x, y)].style().fg,
                    Some(cx.components.pane_border_focused),
                    "边框格 ({x},{y}) 应取组件 token（{style:?}）"
                );
            }
            // 标题仍然画在顶边上（宽字符续格是空符号，按首字判定）。
            let title = tr(" MONITOR ", " 监控 ");
            let first = title.trim().chars().next().expect("标题首字");
            assert!(
                row_text(&buffer, 0).contains(first),
                "标题仍在边框上（{style:?}）：{:?}",
                row_text(&buffer, 0)
            );
            assert!(!expect_round || cx.glyphs.top_left == "╭");
            // 停靠面板 pass：面板表头已写「监控」、左侧有分隔线，页面不再自带
            // 外框（冒烟 L13），首行就是页签。
            let (docked, _) = paint_page(&plain_state(), Page::Monitor, 100, 30);
            let frame = [
                cx.glyphs.top_left,
                cx.glyphs.top_right,
                cx.glyphs.bottom_left,
                cx.glyphs.bottom_right,
                cx.glyphs.vertical,
            ];
            for (x, y) in [(0, 0), (99, 0), (0, 15), (99, 15), (0, 29), (99, 29)] {
                let symbol = docked[(x, y)].symbol();
                assert!(
                    !frame.contains(&symbol),
                    "停靠面板不画页面外框（{style:?}）：({x},{y}) = {symbol:?}"
                );
            }
            assert!(
                row_has(&docked, 0, crate::i18n::texts().monitor.tab_system),
                "页签在首行（{style:?}）：{:?}",
                row_text(&docked, 0)
            );
        }
    }

    /// ds-08 / Graphite：活动页签是浅一档的结构面上的粗体正文。terminal 主题的
    /// `panel_bg` 与 `text` 都是 `Reset`，前景不能退回终端默认色：按对比度挑。
    #[test]
    fn active_page_tab_sits_on_a_lighter_surface_with_a_readable_label() {
        let mut raw = Config::default();
        raw.theme.name = Some("terminal".into());
        let config = ClientShellConfig::from_config(&raw);
        let cx = chrome_context(&config);
        let area = Rect::new(0, 0, 100, 30);
        let mut buffer = Buffer::empty(area);
        let output = paint(
            &mut buffer,
            area,
            &plain_state(),
            &cx,
            Some(Page::Monitor),
            false,
        );
        assert_eq!(
            config.palette.panel_bg,
            Color::Reset,
            "terminal 主题的面板底是终端默认背景"
        );
        let active = output
            .hits
            .iter()
            .find(|(_, action)| matches!(action, Action::Page(Page::Monitor)))
            .map(|(rect, _)| *rect)
            .expect("系统页签命中区");
        let cell = buffer[(active.x + 1, active.y)].clone();
        let bg = cell.style().bg.expect("活动页签有底色");
        assert_eq!(bg, config.palette.hover_row_bg(), "浅一档的结构面");
        let fg = cell.style().fg.expect("活动页签有前景");
        assert_ne!(fg, Color::Reset, "不能退回终端默认前景");
        let ratio = crate::ui::color::contrast_ratio(fg, bg).expect("ANSI 颜色可换算");
        assert!(ratio >= 4.5, "对比度只有 {ratio:.2}");
        assert!(cell.style().add_modifier.contains(Modifier::BOLD));
        // 其余页签是说明色字、没有底色。
        let other = output
            .hits
            .iter()
            .find(|(_, action)| matches!(action, Action::Page(Page::Settings)))
            .map(|(rect, _)| *rect)
            .expect("偏好页签命中区");
        let cell = buffer[(other.x + 1, other.y)].clone();
        assert_eq!(cell.style().fg, Some(config.palette.subtext0));
    }

    /// 回归：面板聚焦时键盘焦点环落在第一个命中区（系统页签）上，曾把前景改成
    /// accent——活动页签的底色也是 accent，标签文字整个看不见。焦点环只叠下划线，
    /// 字色、底色保持活动页签自己的样式；面板没有键盘时不画焦点环。
    #[test]
    fn focused_active_tab_label_stays_visible_under_the_focus_ring() {
        let config = config();
        let cx = chrome_context(&config);
        let area = Rect::new(0, 0, 100, 30);
        for theme in ["graphite-dark", "graphite-light", "catppuccin", "terminal"] {
            let mut raw = Config::default();
            raw.theme.name = Some(theme.into());
            let config = ClientShellConfig::from_config(&raw);
            let cx = chrome_context(&config);
            let mut state = plain_state();
            state.page = Some(Page::Monitor);
            state.selected_hit = 0;
            let mut canvas =
                crate::client::shell::compose_canvas::ComposeCanvas::reuse_or_new(None, 100, 30);
            let painted = state
                .paint(&mut canvas, area, &cx, Some(Page::Monitor), false)
                .expect("页面已绘制");
            let (tab, action) = painted.hits.first().expect("首个命中区");
            assert!(matches!(action, Action::Page(Page::Monitor)), "{theme}");
            let buffer = canvas.buffer();
            // 宽字符的续格被 ratatui 复位成默认样式，只看画了字的格。
            for x in (tab.x + 1..tab.right() - 1)
                .filter(|x| !buffer[(*x, tab.y)].symbol().trim().is_empty())
            {
                let style = buffer[(x, tab.y)].style();
                assert!(
                    style.add_modifier.contains(Modifier::UNDERLINED),
                    "{theme}：焦点环是下划线"
                );
                let (fg, bg) = (style.fg.expect("前景"), style.bg.expect("底色"));
                assert_ne!(fg, bg, "{theme}：标签字色不能等于底色");
                if let Some(ratio) = crate::ui::color::contrast_ratio(fg, bg) {
                    assert!(ratio >= 4.5, "{theme}：对比度只有 {ratio:.2}");
                }
            }
        }
        // 面板没有键盘（终端聚焦）：不画焦点环。
        let state = plain_state();
        let mut canvas =
            crate::client::shell::compose_canvas::ComposeCanvas::reuse_or_new(None, 100, 30);
        let painted = state
            .paint(&mut canvas, area, &cx, Some(Page::Monitor), false)
            .expect("页面已绘制");
        let (tab, _) = painted.hits.first().expect("首个命中区");
        assert!(!canvas.buffer()[(tab.x + 1, tab.y)]
            .style()
            .add_modifier
            .contains(Modifier::UNDERLINED));
    }

    #[test]
    fn global_process_dialog_keeps_focus_and_clears_only_covered_links() {
        let state = process_dialog_state();
        let config = config();
        let cx = chrome_context(&config);
        let (preview, output) = paint_page(&state, Page::Monitor, 120, 40);
        let cancel = hit_rect(&output, |action| matches!(action, Action::CancelProcess))
            .expect("cancel action");
        let point = (cancel.x + 1, cancel.y);
        let symbol = preview[point].symbol().to_owned();
        let mut canvas =
            crate::client::shell::compose_canvas::ComposeCanvas::reuse_or_new(None, 120, 40);
        canvas.buffer()[point].set_symbol(&symbol);
        canvas.push_link(
            point.0,
            point.1,
            symbol.into(),
            "https://covered.example".into(),
        );
        canvas.buffer()[(0, 0)].set_symbol("x");
        canvas.push_link(0, 0, "x".into(), "https://outside.example".into());
        state
            .paint(&mut canvas, Rect::new(0, 0, 120, 40), &cx, None, false)
            .expect("global process dialog");
        assert!(canvas.buffer()[point]
            .style()
            .add_modifier
            .contains(Modifier::UNDERLINED));
        let (frame, _) = canvas.finish(Vec::new());
        let covered = usize::from(point.1) * usize::from(frame.width) + usize::from(point.0);
        assert_eq!(frame.cells[covered].hyperlink, None);
        assert!(frame.cells[0].hyperlink.is_some());
    }

    /// C-29：结束进程是破坏性操作，用组件的 Danger 语义色；「取消」保持常态，
    /// 两者不再长得一样。
    #[test]
    fn process_dialog_marks_destructive_buttons_with_the_danger_tone() {
        let state = process_dialog_state();
        let (buffer, output) = paint_page(&state, Page::Monitor, 120, 40);
        let palette = &config().palette;
        let rect_of = |wanted: fn(&Action) -> bool| {
            output
                .hits
                .iter()
                .find(|(_, action)| wanted(action))
                .map(|(rect, _)| *rect)
                .expect("按钮命中区")
        };
        let cancel = rect_of(|action| matches!(action, Action::CancelProcess));
        let terminate = rect_of(|action| matches!(action, Action::Terminate(false)));
        let force = rect_of(|action| matches!(action, Action::Terminate(true)));
        assert_eq!(
            rect_bg(&buffer, terminate, tr("Terminate", "正常结束")),
            palette.red,
            "正常结束应带 Danger 底色"
        );
        assert_eq!(
            rect_bg(&buffer, force, tr("Force", "强制结束")),
            palette.red,
            "强制结束应带 Danger 底色"
        );
        assert_eq!(
            rect_bg(&buffer, cancel, tr("Cancel", "取消")),
            palette.surface0,
            "取消保持 Secondary 常态"
        );
    }

    #[test]
    fn process_dialog_buttons_fit_translated_labels_at_narrow_widths() {
        for lang in [Lang::En, Lang::ZhCn] {
            let _guard = lang_guard(lang);
            for columns in [40, 60, 120] {
                let state = process_dialog_state();
                let (buffer, output) = paint_page(&state, Page::Monitor, columns, 30);
                let rect_of = |wanted: fn(&Action) -> bool| {
                    output
                        .hits
                        .iter()
                        .find(|(_, action)| wanted(action))
                        .map(|(rect, _)| *rect)
                        .expect("button hit region")
                };
                let cancel = rect_of(|action| matches!(action, Action::CancelProcess));
                let terminate = rect_of(|action| matches!(action, Action::Terminate(false)));
                let force = rect_of(|action| matches!(action, Action::Terminate(true)));
                for (rect, label) in [
                    (cancel, tr("Cancel", "取消")),
                    (terminate, tr("Terminate", "正常结束")),
                    (force, tr("Force", "强制结束")),
                ] {
                    assert!(rect.width >= UnicodeWidthStr::width(label) as u16);
                    assert!(contains_rect(output.dialog_rect, rect));
                    assert!(row_has(&buffer, rect.y, label), "{columns}: {label}");
                }
                assert!(cancel.right() < terminate.x);
                assert!(terminate.right() < force.x);
            }
        }
    }

    #[test]
    fn process_dialog_hover_preserves_destructive_button_colours() {
        let mut state = process_dialog_state();
        state.hovered_action = Some(Action::Terminate(true));
        let (buffer, output) = paint_page(&state, Page::Monitor, 60, 30);
        let force = hit_rect(&output, |action| matches!(action, Action::Terminate(true)))
            .expect("force action");
        let cell = &buffer[(force.x + 1, force.y)];
        assert_eq!(cell.style().bg, Some(config().palette.red));
        assert!(cell.style().add_modifier.contains(Modifier::UNDERLINED));
        assert_eq!(state.selected_hit, 0);
    }

    #[test]
    fn freshness_text_and_spans_read_naturally() {
        let now = 3_600_000;
        let ready = ObservationStatus::Ready;
        assert!(age_text(now, now - 1_900_000, ready).ends_with('⚠'));
        assert!(!age_text(now, now - 1_000, ready).contains('⚠'));
        assert!(age_text(now, now - 1_000, ObservationStatus::Stale).ends_with('⚠'));
        assert_eq!(span_text(7 * 86_400 + 21 * 3600 + 5), "7d21h");
        assert_eq!(span_text(3 * 3600 + 120), "3h02m");
        assert_eq!(span_text(780), "13m");
    }

    /// 冒烟 L7：监控偏好页没有可刷新的内容，页脚不再提示「r 刷新」（也不登记
    /// 刷新命中区）；系统页照旧提示。
    #[test]
    fn preferences_footer_omits_the_refresh_hint() {
        let _guard = lang_guard(Lang::ZhCn);
        let state = plain_state();
        let (buffer, output) = paint_page(&state, Page::Settings, 133, 32);
        let footer = row_text(&buffer, 31);
        assert!(row_has(&buffer, 31, "Esc关闭"), "{footer}");
        assert!(
            !row_has(&buffer, 31, "刷新"),
            "偏好页页脚不提示刷新：{footer}"
        );
        assert!(
            !has(&output, |a| matches!(a, Action::Refresh)),
            "偏好页没有刷新命中区"
        );
        let (buffer, _) = paint_page(&state, Page::Monitor, 133, 32);
        assert!(
            row_has(&buffer, 31, "r刷新"),
            "系统页页脚仍提示刷新：{}",
            row_text(&buffer, 31)
        );
    }

    /// 面板只有系统与偏好两个页签：命中区一一对应，没有别的页。
    #[test]
    fn monitor_panel_has_exactly_the_system_and_preferences_tabs() {
        let state = plain_state();
        let (_, output) = paint_page(&state, Page::Monitor, 100, 30);
        let pages = output
            .hits
            .iter()
            .filter_map(|(_, action)| match action {
                Action::Page(page) => Some(*page),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(pages, [Page::Monitor, Page::Settings]);
    }

    #[test]
    fn tiny_and_empty_pages_never_panic() {
        let state = plain_state();
        for page in [Page::Monitor, Page::Settings] {
            for (width, height) in [(40_u16, 40_u16), (80, 3), (28, 8), (0, 0)] {
                paint_page(&state, page, width, height);
            }
        }
    }

    /// 选中页签画在浅一档的结构面上（`hover_row_bg`）；前景按对比度挑选，每个
    /// 内置主题在真彩与 256 色降级下都要读得出字：各格字符逐一相等，且前景 /
    /// 底色对比度 ≥ 3.0。
    #[test]
    fn active_page_tab_stays_legible_on_every_builtin_theme_and_color_depth() {
        type ThemeCtor = fn() -> Palette;
        let themes: [(&str, ThemeCtor); 18] = [
            ("catppuccin", Palette::catppuccin),
            ("catppuccin_latte", Palette::catppuccin_latte),
            ("terminal", Palette::terminal),
            ("tokyo_night", Palette::tokyo_night),
            ("tokyo_night_day", Palette::tokyo_night_day),
            ("dracula", Palette::dracula),
            ("nord", Palette::nord),
            ("gruvbox", Palette::gruvbox),
            ("gruvbox_light", Palette::gruvbox_light),
            ("one_dark", Palette::one_dark),
            ("one_light", Palette::one_light),
            ("solarized", Palette::solarized),
            ("solarized_light", Palette::solarized_light),
            ("kanagawa", Palette::kanagawa),
            ("kanagawa_lotus", Palette::kanagawa_lotus),
            ("rose_pine", Palette::rose_pine),
            ("rose_pine_dawn", Palette::rose_pine_dawn),
            ("vesper", Palette::vesper),
        ];
        let label = "Monitor";
        let expected = format!(" {label} ");
        for (name, theme) in themes {
            for depth in [
                crate::config::ColorDepth::Truecolor,
                crate::config::ColorDepth::Color256,
            ] {
                let palette = theme().with_color_depth(depth);
                let area = Rect::new(0, 0, 20, 1);
                let mut buffer = Buffer::empty(area);
                let rects = crate::ui::kit::tabs::render_tabs(
                    &mut buffer,
                    area,
                    &[crate::ui::kit::tabs::TabItem::new(label)],
                    0,
                    None,
                    &palette,
                );
                let rect = rects.first().expect("选中页签登记命中区");
                assert_eq!(
                    rect.width as usize,
                    expected.chars().count(),
                    "{name}/{depth:?}"
                );
                for (index, ch) in expected.chars().enumerate() {
                    let cell = &buffer[(rect.x + index as u16, rect.y)];
                    assert_eq!(
                        cell.symbol(),
                        ch.to_string(),
                        "{name}/{depth:?} 第 {index} 格"
                    );
                    let style = cell.style();
                    let bg = style.bg.expect("选中页签有底色");
                    let fg = style.fg.expect("选中页签有前景");
                    assert_eq!(bg, palette.hover_row_bg(), "{name}/{depth:?}");
                    match crate::ui::color::contrast_ratio(fg, bg) {
                        Some(ratio) => assert!(
                            ratio >= 3.0,
                            "{name}/{depth:?}: fg {fg:?} 叠在 {bg:?} 上对比度只有 {ratio:.2}"
                        ),
                        None => assert!(
                            matches!(bg, Color::Reset) || matches!(fg, Color::Reset),
                            "{name}/{depth:?}: 只有 Reset 才允许算不出对比度"
                        ),
                    }
                }
            }
        }
    }
}
