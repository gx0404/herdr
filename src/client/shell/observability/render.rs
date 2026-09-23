//! 监控面板的渲染纯函数：本文件放页面骨架（边框、页签、页脚、进程对话框
//! 的挂载）与各页共用的小工具；系统页 / 账号页 / 设置页 / 悬浮层各在
//! `render/{system,accounts,settings,hover}.rs`。

use std::borrow::Cow;

use super::*;
use ratatui::style::Color;

use super::super::feedback::ChromeContext;

mod accounts;
mod hover;
mod settings;
mod system;

use accounts::*;
use hover::*;
use settings::*;
use system::*;

pub(super) use accounts::{account_rows, metric_percent};
pub(super) use system::CardScrollLimits;

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

/// 页脚键位提示（kit `footer_hints`，可点）：系统页多一个「暂停 / 继续」；
/// 编辑布局模式下换成移动卡片的说明与「完成」。
fn footer_hints(
    buffer: &mut Buffer,
    area: Rect,
    state: &State,
    page: Page,
    palette: &Palette,
    hits: &mut Vec<(Rect, Action)>,
) {
    use crate::ui::kit::footer_hints::{render_footer_hints, FooterHint};
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
            // 账号页的刷新与工具栏同一口径：刷新中 / 防抖期间置灰、不回填命中区。
            let refresh = page != Page::Accounts || refresh_available(state);
            let mut hints = vec![hint("r", texts.hint_refresh, refresh, false)];
            let mut actions = vec![refresh.then_some(Action::Refresh)];
            if page == Page::Monitor {
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
    for (rect, index) in render_footer_hints(buffer, area, &hints, None, palette) {
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
        ObservationStatus::NotAuthenticated => tr("Sign in required", "需要登录"),
        ObservationStatus::NeedsBinding => tr("Select an account", "需要账号绑定"),
        ObservationStatus::Unavailable => tr("Unavailable", "不可用"),
        ObservationStatus::Stale => tr("Cached", "缓存数据"),
        ObservationStatus::Error => tr("Query failed", "查询失败"),
        ObservationStatus::Unknown => tr("Unknown", "未知"),
    }
}

/// Health grading so a glance separates fine / busy / action-needed rows.
pub(super) fn status_color(status: ObservationStatus, palette: &Palette) -> ratatui::style::Color {
    match status {
        ObservationStatus::Ready => palette.green,
        ObservationStatus::Warming => palette.blue,
        ObservationStatus::NotAuthenticated | ObservationStatus::NeedsBinding => palette.yellow,
        ObservationStatus::Unavailable
        | ObservationStatus::Unsupported
        | ObservationStatus::PermissionDenied
        | ObservationStatus::Error => palette.red,
        ObservationStatus::Stale | ObservationStatus::Unknown => palette.overlay1,
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

/// 新鲜度着色：<60 s green / <5 min overlay1 / <30 min yellow / ≥30 min peach；
/// `Stale`（缓存数据）独立映射 peach，不看观测时间。
pub(super) fn age_color(
    now_ms: u64,
    observed_at_ms: u64,
    status: ObservationStatus,
    palette: &Palette,
) -> Color {
    if status == ObservationStatus::Stale {
        return palette.peach;
    }
    let seconds = now_ms.saturating_sub(observed_at_ms) / 1000;
    if seconds < 60 {
        palette.green
    } else if seconds < 300 {
        palette.overlay1
    } else if seconds < 1800 {
        palette.yellow
    } else {
        palette.peach
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

/// 一整行分隔线（`surface_dim`，字形尊重 `ui.border_style`）。
fn rule(buffer: &mut Buffer, rect: Rect, glyphs: crate::ui::BorderGlyphs, palette: &Palette) {
    for x in rect.x..rect.right() {
        if let Some(cell) = buffer.cell_mut((x, rect.y)) {
            cell.set_symbol(glyphs.horizontal)
                .set_style(Style::default().fg(palette.surface_dim));
        }
    }
}

/// 一次绘制产生的矩形与命中区；未绘制的部分保持 `Rect::default()`。
pub(super) struct PaintOutput {
    /// 页面（或进程对话框）的命中区。
    pub hits: Vec<(Rect, Action)>,
    /// 悬浮层自己的命中区。
    pub hover_hits: Vec<(Rect, Action)>,
    /// 页面铺满的矩形（本次传入 `page` 时等于 `area`）。
    pub page_rect: Rect,
    pub hover_rect: Rect,
    pub dialog_rect: Rect,
    /// 系统页各可滚动卡片的滚动上界（本次没画系统页时全为 `None`）。
    pub card_scroll_limits: CardScrollLimits,
}

/// 渲染纯函数：`page` 是本次要画的页面（停靠面板由调用方决定画哪个 tab），
/// 状态只读；`draw_hover` 为真时画可见的悬浮层（agent 行悬浮只在没有页面时），
/// 进程对话框总是最后覆盖。调色板、组件 token 与
/// 边框字形都来自 `ChromeContext`（与浮层同源，C-29）。
pub(super) fn paint(
    buffer: &mut Buffer,
    area: Rect,
    state: &State,
    cx: &ChromeContext<'_>,
    page: Option<Page>,
    draw_hover: bool,
) -> PaintOutput {
    let palette = cx.palette;
    let mut hits = Vec::new();
    let mut page_rect = Rect::default();
    let mut card_scroll_limits = CardScrollLimits::default();
    if let Some(page) = page {
        page_rect = area;
        buffer.set_style(area, Style::default().fg(palette.text).bg(palette.panel_bg));
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                buffer[(x, y)].set_symbol(" ");
            }
        }
        // 停靠面板的表头已经写了「监控」，边框上不再重复；只有经典布局（页面
        // 铺满 pane 区、与悬浮层同一 pass 绘制，即 `draw_hover` 为真）没有别的
        // 标题，才在边框上写一次。
        let title = if draw_hover {
            tr(" MONITOR ", " 监控 ")
        } else {
            ""
        };
        let inner = block(buffer, area, title, cx);
        // Single navigation level: page tabs only. Refresh/pause/close live
        // on keyboard shortcuts (see footer) so the row never mixes
        // navigation with actions. 页签走 kit `tabs`：活动页 accent 反色，
        // 前景按对比度挑选（ds-08）。
        let texts = &crate::i18n::texts().monitor;
        let tabs = [
            (texts.tab_system, Page::Monitor),
            (texts.tab_accounts, Page::Accounts),
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
        if state.paused {
            let label = tr(" ‖ paused ", " ‖ 已暂停 ");
            let width = UnicodeWidthStr::width(label) as u16;
            let rect = Rect::new(
                inner.right().saturating_sub(width).max(x),
                inner.y,
                width.min(inner.right().saturating_sub(x.max(inner.x))),
                1,
            );
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
        let body = Rect::new(
            inner.x,
            inner.y.saturating_add(2),
            inner.width,
            inner.height.saturating_sub(3),
        );
        match page {
            Page::Monitor => monitor(buffer, body, state, cx, &mut hits, &mut card_scroll_limits),
            Page::Accounts => accounts(buffer, body, state, palette, &mut hits),
            Page::Settings => settings(buffer, body, state, palette, &mut hits),
        }
        // 页脚：一次性说明（`message`）优先，否则是可点的键位提示。
        let footer = Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1);
        match state.message.as_deref() {
            Some(message) => text(
                buffer,
                footer,
                0,
                message,
                Style::default().fg(palette.overlay0),
            ),
            None => footer_hints(buffer, footer, state, page, palette, &mut hits),
        }
    }
    let mut hover_hits = Vec::new();
    // agent 行悬浮只在没有页面时画（停靠面板的全局 pass / 经典布局无页面）；
    // 钉住的卡是用户显式打开的（右键「用量」），经典布局页面之上也画。
    let pinned = state.hover.as_ref().is_some_and(|hover| hover.pinned);
    let mut hover_rect = if draw_hover && (page.is_none() || pinned) {
        hover_layer(buffer, state, cx, &mut hover_hits)
    } else {
        Rect::default()
    };
    let mut dialog_rect = Rect::default();
    if let Some(dialog) = &state.process_dialog {
        hover_rect = Rect::default();
        hover_hits.clear();
        hits.clear();
        dialog_rect = process_dialog(buffer, dialog, state, cx, &mut hits);
    }
    PaintOutput {
        hits,
        hover_hits,
        page_rect,
        hover_rect,
        dialog_rect,
        card_scroll_limits,
    }
}

/// 把矩形填成空格，供浮层在终端内容之上重新绘制。
fn clear(buffer: &mut Buffer, rect: Rect) {
    for row in rect.y..rect.bottom() {
        for col in rect.x..rect.right() {
            if let Some(cell) = buffer.cell_mut((col, row)) {
                cell.set_symbol(" ");
            }
        }
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

    fn provider(agent: &str, label: &str, accounts: &[&str]) -> UsageProviderInfo {
        UsageProviderInfo {
            agent: agent.into(),
            label: label.into(),
            source_url: "https://example.invalid/docs".into(),
            method: "cli".into(),
            account_scope: "account".into(),
            minimum_interval_seconds: 300,
            configured_accounts: accounts.iter().map(|id| (*id).to_string()).collect(),
            installed: Some(true),
            // 测试里 claude 视为服务端宣告支持回调开关（生产由 server 宣告）。
            supports_callback: agent == "claude",
        }
    }

    fn metric(label: &str, percent: Option<f64>) -> UsageMetric {
        UsageMetric {
            id: label.to_lowercase(),
            label: label.into(),
            unit: "%".into(),
            scope: "account".into(),
            used_percent: percent,
            used: Some(42.0),
            limit: Some(100.0),
            resets_at: Some(7 * 24 * 3600 + 21 * 3600),
            window_seconds: Some(7 * 24 * 3600),
            ..Default::default()
        }
    }

    fn account(id: &str, status: ObservationStatus) -> AccountUsageSnapshot {
        AccountUsageSnapshot {
            account_id: id.into(),
            account_label: id.into(),
            agent: "claude".into(),
            provider: "claude".into(),
            auth_mode: "cli".into(),
            status,
            source: "official CLI".into(),
            source_url: "https://example.invalid/docs".into(),
            observed_at_ms: 1_000,
            metrics: vec![metric("5h", Some(42.0)), metric("7d", Some(3.0))],
            ..Default::default()
        }
    }

    /// 已选 claude、两个账号（一个已更新、一个需登录）的页面状态。
    fn populated() -> State {
        let mut state = State::new(&config());
        state.now_ms = 14_000;
        state.providers = vec![
            provider("claude", "Claude Code", &["claude:default", "claude:work"]),
            provider("codex", "Codex", &["codex:default"]),
        ];
        state.selected_provider = Some("claude".into());
        state.accounts = vec![
            account("claude:default", ObservationStatus::Ready),
            account("claude:work", ObservationStatus::NotAuthenticated),
        ];
        state
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
            // 经典布局 pass（`draw_hover` 为真）：没有停靠表头，边框上写标题。
            let mut buffer = Buffer::empty(area);
            paint(
                &mut buffer,
                area,
                &populated(),
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
            // 停靠面板 pass：表头已写「监控」，边框上不再重复标题。
            let (docked, _) = paint_page(&populated(), Page::Monitor, 100, 30);
            assert!(
                !row_text(&docked, 0).contains(first),
                "停靠面板的边框不重复标题（{style:?}）：{:?}",
                row_text(&docked, 0)
            );
        }
    }

    /// 右栏（≥96 列）在账号详情下方画出用量历史 sparkline：数据来自客户端记录
    /// 的采样，少于两个点时只显示「正在采样…」。
    #[test]
    fn account_detail_draws_the_usage_history_sparkline() {
        let _guard = lang_guard(Lang::ZhCn);
        let mut state = populated();
        state.accounts = vec![account("claude:default", ObservationStatus::Ready)];
        state.selected_provider = Some("claude".into());
        let right_column = |buffer: &Buffer| -> String {
            (0..buffer.area.height)
                .flat_map(|y| {
                    (90..buffer.area.width).map(move |x| buffer[(x, y)].symbol().to_owned())
                })
                .collect()
        };

        // 一个采样点：只提示还在采样，不画 sparkline。
        state.usage_history.insert(
            "claude:default".into(),
            std::collections::VecDeque::from([UsageSample {
                at_ms: 1_000,
                percent: 12.0,
            }]),
        );
        let (buffer, _) = paint_page(&state, Page::Accounts, 120, 30);
        let text = right_column(&buffer);
        let stripped = text.split_whitespace().collect::<String>();
        assert!(stripped.contains("正在采样"), "右栏: {text}");

        // 多个采样点：标签带计数，并出现 sparkline 的方块字。
        let samples = (0..8)
            .map(|index| UsageSample {
                at_ms: 1_000 + index * 1_000,
                percent: 10.0 + index as f32 * 5.0,
            })
            .collect::<std::collections::VecDeque<_>>();
        state.usage_history.insert("claude:default".into(), samples);
        let (buffer, _) = paint_page(&state, Page::Accounts, 120, 30);
        let text = right_column(&buffer);
        let stripped = text.split_whitespace().collect::<String>();
        assert!(stripped.contains("用量历史"), "右栏: {text}");
        assert!(
            text.chars().any(|ch| "▁▂▃▄▅▆▇█".contains(ch)),
            "sparkline 方块字应出现在右栏: {text}"
        );
    }

    /// ds-08：terminal 主题的 `panel_bg` 是 `Reset`，页签「反色」曾退化成
    /// 「终端默认前景压在 accent 上」。反色前景取组件表（与按钮同源）；组件表
    /// 现在按对比度挑颜色（`crate::ui::color::contrast_fg`），`Reset` 候选被跳过，
    /// 16 色主题下也有明确对比。
    #[test]
    fn active_page_tab_inverts_with_the_component_contrast_color() {
        let mut raw = Config::default();
        raw.theme.name = Some("terminal".into());
        let config = ClientShellConfig::from_config(&raw);
        let cx = chrome_context(&config);
        let area = Rect::new(0, 0, 100, 30);
        let mut buffer = Buffer::empty(area);
        let output = paint(
            &mut buffer,
            area,
            &populated(),
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
        let cell = buffer[(active.x, active.y)].clone();
        assert_eq!(cell.style().bg, Some(config.palette.accent), "活动页签底色");
        let fg = cell.style().fg.expect("活动页签有前景");
        assert_ne!(fg, Color::Reset, "面板底为 Reset 时不能退回终端默认前景");
        assert_eq!(
            fg,
            crate::ui::panel_contrast_fg(&config.palette),
            "反色前景与组件表同源"
        );
        let ratio = crate::ui::color::contrast_ratio(fg, config.palette.accent)
            .expect("ANSI accent 可换算");
        assert!(ratio >= 4.5, "对比度只有 {ratio:.2}");
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

    /// ds-15：对话框按钮用固定 cell 起点，中文标签必须完整落在命中区内
    /// （不是被截断或与相邻按钮重叠）。
    #[test]
    fn process_dialog_fixed_offsets_fit_the_chinese_labels() {
        let _guard = lang_guard(Lang::ZhCn);
        let state = process_dialog_state();
        let (buffer, output) = paint_page(&state, Page::Monitor, 120, 40);
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
        for (rect, label) in [
            (cancel, "取消"),
            (terminate, "正常结束"),
            (force, "强制结束"),
        ] {
            let width = UnicodeWidthStr::width(label) as u16;
            assert!(
                rect.width >= width,
                "{label} 的命中区只有 {} 列，放不下 {width} 列",
                rect.width
            );
            assert!(
                row_has(&buffer, rect.y, label),
                "{label} 应完整画在命中区所在行"
            );
        }
        assert!(
            terminate.right() < force.x || force.right() <= terminate.x,
            "固定列偏移下两个结束按钮不能重叠: {terminate:?} / {force:?}"
        );
    }

    #[test]
    fn settings_page_shows_enum_values_in_the_ui_language() {
        let _guard = lang_guard(Lang::ZhCn);
        let mut state = State::new(&config());
        state.usage.position = UsageDisplayPosition::Page;
        let (buffer, output) = paint_page(&state, Page::Settings, 100, 40);
        let text = buffer_text(&buffer);
        for token in ["Dashboard", "Hover", "Page", "Table", "Both"] {
            assert!(
                !text.contains(token),
                "中文设置页不能出现枚举英文 {token}: {text}"
            );
        }
        assert!(buffer_has(&buffer, "仪表盘"), "{text}");
        assert!(
            buffer_has(&buffer, "悬浮浮层已关闭"),
            "位置为页面时常驻提示: {text}"
        );
        assert!(
            buffer_has(&buffer, "CPU 逐核"),
            "显示项复用系统页中文标题: {text}"
        );
        assert!(buffer_has(&buffer, "悬浮延时"), "{text}");
        assert!(has(&output, |action| matches!(
            action,
            Action::HoverDelay(_)
        )));
        assert!(!text.contains("Interval"), "只读行也走中文: {text}");
    }

    /// 某个工具栏项所在的行（按其文案定位，避免全屏找字形被别处的同名字符误判）。
    fn toolbar_row(buffer: &Buffer, needle: &str) -> Option<u16> {
        (0..buffer.area.height).find(|y| row_has(buffer, *y, needle))
    }

    /// 给 `account_id` 附上服务端判定的官方回调态。
    fn with_callback(state: &mut State, account_id: &str, enabled: Option<bool>) {
        state.refresh_states.push(UsageRefreshState {
            account_id: account_id.into(),
            callback_enabled: enabled,
            ..Default::default()
        });
    }

    #[test]
    fn accounts_page_toolbar_offers_settings_and_format_actions() {
        let mut state = populated();
        let (buffer, output) = paint_page(&state, Page::Accounts, 120, 40);
        assert!(has(&output, |a| matches!(a, Action::Configure)));
        assert!(has(&output, |a| matches!(a, Action::UsageFormat(_))));
        assert!(has(&output, |a| matches!(a, Action::Refresh)));
        assert!(has(&output, |a| matches!(a, Action::Source)));
        assert!(has(&output, |a| matches!(a, Action::CycleAccount)));
        assert!(has(&output, |a| matches!(a, Action::CyclePane(1))));
        assert!(has(&output, |a| matches!(a, Action::BindFocused)));
        // 两个账号且未选：开关没有作用对象，禁用并标注「先选账号」，不发任何动作。
        let texts = &crate::i18n::texts().monitor;
        assert!(!has(&output, |a| matches!(a, Action::UsageIntegration(_))));
        let row = toolbar_row(&buffer, texts.callback_toggle).expect("开关仍在工具栏");
        assert!(row_has(&buffer, row, texts.select_account_first));
        // 选中账号后开关有了作用对象：状态未知时提供「启用」，且没有独立的「移除」按钮。
        state.selected_account = Some("claude:work".into());
        let (buffer, output) = paint_page(&state, Page::Accounts, 120, 40);
        assert!(has(&output, |a| matches!(
            a,
            Action::UsageIntegration(true)
        )));
        assert!(!has(&output, |a| matches!(
            a,
            Action::UsageIntegration(false)
        )));
        let row = toolbar_row(&buffer, texts.callback_toggle).expect("开关在工具栏");
        assert!(row_has(&buffer, row, "?"), "未知态字形");
        assert!(!row_has(&buffer, row, "○") && !row_has(&buffer, row, "✓"));
    }

    #[test]
    fn callback_toggle_reflects_the_server_reported_state() {
        let texts = &crate::i18n::texts().monitor;
        // 已接入：✓ + 下一步是移除。
        let mut state = populated();
        state.selected_account = Some("claude:default".into());
        with_callback(&mut state, "claude:default", Some(true));
        with_callback(&mut state, "claude:work", Some(false));
        let (buffer, output) = paint_page(&state, Page::Accounts, 120, 40);
        assert!(has(&output, |a| matches!(
            a,
            Action::UsageIntegration(false)
        )));
        assert!(!has(&output, |a| matches!(
            a,
            Action::UsageIntegration(true)
        )));
        let row = toolbar_row(&buffer, texts.callback_toggle).expect("开关在工具栏");
        assert!(row_has(&buffer, row, "✓"), "已启用态带 ✓");
        assert!(!row_has(&buffer, row, "○") && !row_has(&buffer, row, "?"));
        // 同厂商另一个账号未接入：显示态跟着作用对象走，不是厂商级聚合。
        state.selected_account = Some("claude:work".into());
        let (buffer, output) = paint_page(&state, Page::Accounts, 120, 40);
        assert!(has(&output, |a| matches!(
            a,
            Action::UsageIntegration(true)
        )));
        assert!(!has(&output, |a| matches!(
            a,
            Action::UsageIntegration(false)
        )));
        let row = toolbar_row(&buffer, texts.callback_toggle).expect("开关在工具栏");
        assert!(row_has(&buffer, row, "○"), "未接入态带 ○");
        assert!(!row_has(&buffer, row, "?") && !row_has(&buffer, row, "✓"));
        // 唯一账号时不必显式选中。
        state.selected_account = None;
        state.accounts.truncate(1);
        let (buffer, output) = paint_page(&state, Page::Accounts, 120, 40);
        assert!(has(&output, |a| matches!(
            a,
            Action::UsageIntegration(false)
        )));
        let row = toolbar_row(&buffer, texts.callback_toggle).expect("开关在工具栏");
        assert!(row_has(&buffer, row, "✓"));
        // 不支持回调的厂商没有开关。
        state.selected_provider = Some("codex".into());
        let (_, output) = paint_page(&state, Page::Accounts, 120, 40);
        assert!(!has(&output, |a| matches!(a, Action::UsageIntegration(_))));
        // 服务端未宣告能力（旧 server）：即使是 claude 也不画开关，客户端不维护厂商名单。
        state.selected_provider = Some("claude".into());
        state.providers[0].supports_callback = false;
        let (buffer, output) = paint_page(&state, Page::Accounts, 120, 40);
        assert!(!has(&output, |a| matches!(a, Action::UsageIntegration(_))));
        assert!(toolbar_row(&buffer, texts.callback_toggle).is_none());
    }

    #[test]
    fn usage_table_keeps_status_in_the_status_column_for_accounts_without_metrics() {
        let mut state = populated();
        state.usage.format = UsageDisplayFormat::Table;
        state.accounts[1].metrics.clear();
        // 说明文字很短：状态列按百分比分宽，长说明在窄表里会被截断（状态在前仍可见）。
        state.accounts[1].message = Some("later".into());
        let (buffer, _) = paint_page(&state, Page::Accounts, 160, 40);
        let row = (0..buffer.area.height)
            .find(|y| row_has(&buffer, *y, "claude:work"))
            .expect("无指标账号有一行");
        let text = row_text(&buffer, row);
        let compact = text.replace(' ', "");
        assert!(
            compact.contains("———"),
            "指标 / 用量 / 重置三列留空: {text}"
        );
        let status_at = compact
            .find(&status(ObservationStatus::NotAuthenticated).replace(' ', ""))
            .expect("状态文案在行内");
        let message_at = compact.find("·later").expect("说明并入状态列尾");
        let freshness_at = compact.find("13s").expect("新鲜度列");
        assert!(
            freshness_at < status_at && status_at < message_at,
            "状态与说明落在新鲜度之后的「状态」列: {text}"
        );
    }

    #[test]
    fn provider_chips_return_to_overview_and_skip_disabled_providers() {
        let mut state = populated();
        state.usage.disabled_providers = vec!["codex".into()];
        let (_, output) = paint_page(&state, Page::Accounts, 120, 40);
        // 已选中的厂商 chip 与「全部厂商」都回到总览；未选中的厂商 chip 选它。
        assert!(has(&output, |a| matches!(a, Action::Overview)));
        assert!(!has(
            &output,
            |a| matches!(a, Action::Provider(agent) if agent == "claude")
        ));
        assert!(!has(
            &output,
            |a| matches!(a, Action::Provider(agent) if agent == "codex")
        ));
        state.selected_provider = None;
        state.usage.disabled_providers.clear();
        let (_, output) = paint_page(&state, Page::Accounts, 120, 40);
        assert!(has(
            &output,
            |a| matches!(a, Action::Provider(agent) if agent == "claude")
        ));
        assert!(has(
            &output,
            |a| matches!(a, Action::Provider(agent) if agent == "codex")
        ));
    }

    #[test]
    fn accounts_page_hits_stay_inside_the_area_at_every_width() {
        let state = populated();
        for width in [40_u16, 80, 120] {
            let area = Rect::new(0, 0, width, 40);
            let (_, output) = paint_page(&state, Page::Accounts, width, 40);
            for (rect, action) in &output.hits {
                assert!(
                    contains_rect(area, *rect),
                    "宽 {width}: 命中区 {rect:?}（{action:?}）越界"
                );
            }
            // 每档都能选厂商、选账号。
            assert!(has(&output, |a| matches!(
                a,
                Action::Provider(_) | Action::Overview
            )));
            assert!(has(&output, |a| matches!(a, Action::Account(_))));
        }
    }

    #[test]
    fn narrow_panel_keeps_only_the_picker_row_and_content() {
        let state = populated();
        let (_, wide) = paint_page(&state, Page::Accounts, 80, 40);
        assert!(has(&wide, |a| matches!(a, Action::Configure)));
        assert!(has(&wide, |a| matches!(a, Action::BindFocused)));
        let (_, narrow) = paint_page(&state, Page::Accounts, 39, 40);
        assert!(
            !has(&narrow, |a| matches!(a, Action::Configure)),
            "<40 列只留头行"
        );
        assert!(!has(&narrow, |a| matches!(a, Action::BindFocused)));
        assert!(has(&narrow, |a| matches!(a, Action::Account(_))));
        // 最小停靠面板（28×8 → 正文 26×3）也画得出账号。
        let (_, tiny) = paint_page(&state, Page::Accounts, 28, 8);
        assert!(
            has(&tiny, |a| matches!(a, Action::Account(_))),
            "{:?}",
            tiny.hits
        );
    }

    #[test]
    fn dashboard_rows_match_the_scroll_source_of_truth() {
        let mut state = populated();
        state.accounts[0].message = Some("hello".into());
        state.accounts[0].plan = Some("Max".into());
        state.accounts[1].account_identity = Some("me@example.invalid".into());
        state.accounts.push(AccountUsageSnapshot {
            metrics: Vec::new(),
            ..account("claude:empty", ObservationStatus::Warming)
        });
        state.refresh_states = vec![UsageRefreshState {
            account_id: "claude:work".into(),
            trust_required: true,
            ..Default::default()
        }];
        let scope = page_scope(&state);
        let area = Rect::new(0, 0, 100, 200);
        let mut buffer = Buffer::empty(area);
        let mut hits = Vec::new();
        let drawn = usage_dashboard(
            &mut buffer,
            area,
            &state,
            &scope,
            &config().palette,
            &mut hits,
        );
        assert_eq!(
            drawn,
            account_rows(&state, &state.accounts, &state.refresh_states),
            "仪表盘实际行数与滚动真源一致"
        );
        assert_eq!(hits.len(), 3, "每账号一个标题行命中区");
    }

    #[test]
    fn ready_and_not_authenticated_accounts_differ_in_text_and_color() {
        let _guard = lang_guard(Lang::ZhCn);
        let state = populated();
        let (buffer, output) = paint_page(&state, Page::Accounts, 120, 40);
        let header_of = |id: &str| {
            output
                .hits
                .iter()
                .find(|(_, action)| matches!(action, Action::Account(account) if account == id))
                .map(|(rect, _)| *rect)
                .unwrap_or_else(|| panic!("账号 {id} 的标题行命中区"))
        };
        let ready = header_of("claude:default");
        let blocked = header_of("claude:work");
        assert!(
            row_has(&buffer, ready.y, "已更新"),
            "{}",
            row_text(&buffer, ready.y)
        );
        assert!(
            row_has(&buffer, blocked.y, "需要登录"),
            "{}",
            row_text(&buffer, blocked.y)
        );
        let palette = config().palette;
        assert_eq!(color_at(&buffer, ready.y, "已更新"), Some(palette.green));
        assert_eq!(
            color_at(&buffer, blocked.y, "需要登录"),
            Some(palette.yellow)
        );
        // 头行右侧是新鲜度：13 秒前 → 绿色。
        assert!(
            row_has(&buffer, ready.y, "13s 前更新"),
            "{}",
            row_text(&buffer, ready.y)
        );
        assert_eq!(color_at(&buffer, ready.y, "13s"), Some(palette.green));
        // 指标行：定宽额度条只在 Ready 账号上着色，需登录的账号是虚化占位。
        let ready_metric = row_text(&buffer, ready.y + 1);
        assert!(ready_metric.contains("━"), "{ready_metric}");
        assert!(
            row_has(&buffer, ready.y + 1, "距重置 7d20h"),
            "{ready_metric}"
        );
        assert!(ready_metric.contains("42/100"), "{ready_metric}");
        let blocked_metric = row_text(&buffer, blocked.y + 1);
        assert!(!blocked_metric.contains("━"), "{blocked_metric}");
        assert!(blocked_metric.contains("░"), "{blocked_metric}");
        // ≥96 列右栏显示详情。
        let text = buffer_text(&buffer);
        assert!(buffer_has(&buffer, "认证方式"), "{text}");
        assert!(buffer_has(&buffer, "official CLI"), "{text}");
        // 底部汇总行。
        assert!(buffer_has(&buffer, "2 个账号"), "{text}");
        assert!(buffer_has(&buffer, "1 需处理"), "{text}");
    }

    #[test]
    fn table_mode_adds_a_freshness_column_and_unified_percent() {
        let _guard = lang_guard(Lang::ZhCn);
        let mut state = populated();
        state.usage.format = UsageDisplayFormat::Table;
        state.accounts[0].metrics[0].used_percent = None;
        state.accounts[0].metrics[0].used = Some(150.0);
        let (buffer, output) = paint_page(&state, Page::Accounts, 120, 40);
        let text = buffer_text(&buffer);
        assert!(buffer_has(&buffer, "新鲜度"), "{text}");
        assert!(
            buffer_has(&buffer, "100.0%"),
            "used/limit 推算并夹取到 100: {text}"
        );
        assert!(has(&output, |a| matches!(a, Action::Account(_))));
    }

    #[test]
    fn freshness_colors_follow_the_age_bands() {
        let palette = config().palette;
        let now = 3_600_000;
        let ready = ObservationStatus::Ready;
        assert_eq!(age_color(now, now - 30_000, ready, &palette), palette.green);
        assert_eq!(
            age_color(now, now - 120_000, ready, &palette),
            palette.overlay1
        );
        assert_eq!(
            age_color(now, now - 600_000, ready, &palette),
            palette.yellow
        );
        assert_eq!(
            age_color(now, now - 1_900_000, ready, &palette),
            palette.peach
        );
        assert_eq!(
            age_color(now, now - 1_000, ObservationStatus::Stale, &palette),
            palette.peach
        );
        assert!(age_text(now, now - 1_900_000, ready).ends_with('⚠'));
        assert!(!age_text(now, now - 1_000, ready).contains('⚠'));
        assert_eq!(span_text(7 * 86_400 + 21 * 3600 + 5), "7d21h");
        assert_eq!(span_text(3 * 3600 + 120), "3h02m");
        assert_eq!(span_text(780), "13m");
    }

    #[test]
    fn quota_bar_colors_by_window_pace_and_placeholders_dead_data() {
        let palette = config().palette;
        // 无窗口：90 / 75 阈值。
        assert_eq!(quota_color(50.0, None, &palette), palette.teal);
        assert_eq!(quota_color(80.0, None, &palette), palette.yellow);
        assert_eq!(quota_color(95.0, None, &palette), palette.red);
        // 有窗口：用得比时间快才变色。
        assert_eq!(quota_color(50.0, Some(0.6), &palette), palette.teal);
        assert_eq!(quota_color(50.0, Some(0.3), &palette), palette.yellow);
        assert_eq!(quota_color(50.0, Some(0.1), &palette), palette.red);
        let area = Rect::new(0, 0, 10, 1);
        let mut buffer = Buffer::empty(area);
        quota_bar(
            &mut buffer,
            area,
            Some(50.0),
            None,
            ObservationStatus::Ready,
            &palette,
        );
        assert_eq!(row_text(&buffer, 0), "━━━━━░░░░░");
        let mut buffer = Buffer::empty(area);
        quota_bar(
            &mut buffer,
            area,
            Some(50.0),
            None,
            ObservationStatus::Error,
            &palette,
        );
        assert_eq!(row_text(&buffer, 0), "░░░░░░░░░░", "失效数据不画确定基线");
    }

    #[test]
    fn flow_layout_wraps_and_folds_in_order() {
        let (positions, rows) = flow_positions(&[5, 5, 5], 20, 2, 0);
        assert_eq!(positions, vec![Some((0, 0)), Some((6, 0)), Some((12, 0))]);
        assert_eq!(rows, 1);
        let (positions, rows) = flow_positions(&[8, 8, 8, 8, 8], 20, 2, 2);
        assert_eq!(
            positions,
            vec![Some((0, 0)), Some((9, 0)), Some((0, 1)), Some((9, 1)), None]
        );
        assert_eq!(rows, 2);
        // 单项比行宽还宽：原地放下，不吞掉。
        let (positions, rows) = flow_positions(&[30, 4], 20, 2, 0);
        assert_eq!(positions, vec![Some((0, 0)), Some((0, 1))]);
        assert_eq!(rows, 2);
        assert_eq!(flow_positions(&[], 20, 2, 0), (Vec::new(), 0));
    }

    #[test]
    fn empty_accounts_page_never_panics() {
        let mut state = State::new(&config());
        for width in [40_u16, 80, 120] {
            paint_page(&state, Page::Accounts, width, 40);
            paint_page(&state, Page::Accounts, width, 3);
        }
        state.providers = vec![provider("codex", "Codex", &[])];
        state.selected_provider = Some("codex".into());
        for width in [28_u16, 40, 80, 120] {
            paint_page(&state, Page::Accounts, width, 8);
        }
        state.providers = (0..30)
            .map(|index| provider(&format!("p{index}"), &format!("Provider {index}"), &[]))
            .collect();
        let (buffer, _) = paint_page(&state, Page::Accounts, 100, 40);
        assert!(buffer_has(&buffer, "还有") || buffer_has(&buffer, "more"));
        paint_page(&state, Page::Accounts, 0, 0);
        paint_page(&state, Page::Settings, 0, 0);
    }

    /// 选中页签反色到 accent 底上；前景由 `panel_contrast_fg` 按对比度挑选，
    /// 每个内置主题在真彩与 256 色降级下都要读得出字：各格字符逐一相等，且
    /// 前景 / 底色对比度 ≥ 3.0（曾有主题的 `panel_bg` 与 `accent` 亮度接近，
    /// 字叠上去就看不见）。
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
        let label = "Accounts";
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
                    assert_eq!(bg, palette.accent, "{name}/{depth:?}");
                    match crate::ui::color::contrast_ratio(fg, bg) {
                        Some(ratio) => assert!(
                            ratio >= 3.0,
                            "{name}/{depth:?}: fg {fg:?} 叠在 accent {bg:?} 上对比度只有 {ratio:.2}"
                        ),
                        // terminal 主题的 accent 是终端默认色，无从换算。
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
