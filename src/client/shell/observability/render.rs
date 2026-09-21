use super::*;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::Color;
use ratatui::widgets::{Cell, Row, Sparkline, Table, Widget};

use super::super::feedback::ChromeContext;

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

/// Page tab with an unambiguous active state: the selected page inverts into
/// the accent color so it can never be confused with idle tabs. 反色的前景取
/// 组件表（`panel_contrast_fg`），terminal 主题下不再是「终端默认前景压在
/// accent 上」（ds-08）。
fn page_tab(
    buffer: &mut Buffer,
    rect: Rect,
    label: &str,
    active: bool,
    action: Action,
    palette: &Palette,
    hits: &mut Vec<(Rect, Action)>,
) {
    let label = format!(" {label} ");
    let width = crate::ui::modal_button_width(&label).min(rect.width);
    let rect = Rect::new(rect.x, rect.y, width, rect.height.min(1));
    let style = crate::ui::modal_button_style(
        palette,
        if active {
            crate::ui::ModalButtonTone::Primary
        } else {
            crate::ui::ModalButtonTone::Secondary
        },
        crate::ui::ModalButtonState::Normal,
    );
    buffer.set_style(rect, style);
    text(buffer, rect, 0, &label, style);
    if !rect.is_empty() {
        hits.push((rect, action));
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

fn history(
    state: &State,
    width: u16,
    value: impl Fn(&HistoryPoint) -> Option<f32>,
) -> Vec<Option<u64>> {
    let width = usize::from(width);
    let mut buckets = vec![(0.0_f64, 0_u32); width];
    if width == 0 {
        return Vec::new();
    }
    let end = state
        .history
        .back()
        .map_or(state.now_ms, |point| point.at)
        .saturating_add(1);
    let duration = u64::from(state.monitor.history_minutes.clamp(1, 60)) * 60_000;
    let start = end.saturating_sub(duration);
    for point in &state.history {
        if point.at < start || point.at >= end {
            continue;
        }
        if let Some(value) = value(point).filter(|value| value.is_finite()) {
            let index =
                ((point.at - start) as u128 * width as u128 / u128::from(duration)) as usize;
            if let Some((sum, count)) = buckets.get_mut(index) {
                *sum += f64::from(value.clamp(0.0, 100.0));
                *count += 1;
            }
        }
    }
    buckets
        .into_iter()
        .map(|(sum, count)| (count > 0).then(|| (sum / f64::from(count)).round() as u64))
        .collect()
}

fn bar(buffer: &mut Buffer, area: Rect, value: Option<f32>, palette: &Palette) {
    if area.is_empty() {
        return;
    }
    let Some(value) = value else {
        text(
            buffer,
            area,
            0,
            tr("Waiting for a sample", "等待采样"),
            Style::default().fg(palette.overlay0),
        );
        return;
    };
    let fill = (area.width as f32 * value.clamp(0.0, 100.0) / 100.0).round() as u16;
    let color = if value >= 90.0 {
        palette.red
    } else if value >= 75.0 {
        palette.yellow
    } else {
        palette.teal
    };
    for x in 0..area.width {
        if let Some(cell) = buffer.cell_mut((area.x + x, area.y)) {
            cell.set_symbol(if x < fill { "━" } else { "─" })
                .set_style(Style::default().fg(if x < fill { color } else { palette.surface1 }));
        }
    }
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

/// 两种显示模式统一的已用百分比：官方 `used_percent` 优先，否则由 `used/limit`
/// 推算；统一夹取到 0..=100（ACC-20）。
pub(super) fn metric_percent(metric: &UsageMetric) -> Option<f32> {
    metric
        .used_percent
        .or_else(|| match (metric.used, metric.limit) {
            (Some(used), Some(limit)) if limit > 0.0 => Some(used / limit * 100.0),
            _ => None,
        })
        .filter(|value| value.is_finite())
        .map(|value| value.clamp(0.0, 100.0) as f32)
}

/// 数值去掉无意义的小数位：`100` / `42.5` / `0.33`。
fn trim_number(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        format!("{value:.2}")
    }
}

/// 数量列：文本值 / 金额 / `used/limit` / 单独的 used / remaining；都没有时 `—`
/// （ACC-14：`used/limit` 不再被互斥链丢弃）。
fn metric_quantity(metric: &UsageMetric) -> String {
    if let Some(text) = &metric.text_value {
        return text.clone();
    }
    if let Some(amount) = &metric.amount_decimal {
        return format!("{amount} {}", metric.unit);
    }
    // 百分比型指标的单位就是 `%`，数量列不再重复它。
    let unit = if metric.unit == "%" {
        ""
    } else {
        metric.unit.as_str()
    };
    let with_unit = |value: String| {
        if unit.is_empty() {
            value
        } else {
            format!("{value} {unit}")
        }
    };
    match (metric.used, metric.limit, metric.remaining) {
        (Some(used), Some(limit), _) => {
            with_unit(format!("{}/{}", trim_number(used), trim_number(limit)))
        }
        (Some(used), None, _) => with_unit(trim_number(used)),
        (None, _, Some(remaining)) => format!(
            "{} {}",
            with_unit(trim_number(remaining)),
            tr("remaining", "剩余")
        ),
        _ => "—".into(),
    }
}

/// 距重置的秒数（已过期为 0）。
fn reset_secs(metric: &UsageMetric, now_ms: u64) -> Option<u64> {
    metric
        .resets_at
        .map(|reset| reset.saturating_sub(now_ms / 1000))
}

/// 「距重置 6d21h」（ACC-13：不再写成「重置于 167h」）。
fn reset_text(metric: &UsageMetric, now_ms: u64) -> Option<String> {
    reset_secs(metric, now_ms).map(|seconds| {
        crate::i18n::fill(
            crate::i18n::texts().monitor.resets_in_fmt,
            &[("span", &span_text(seconds))],
        )
    })
}

/// 额度窗口已过比例（0..=1）：只有同时知道 `window_seconds` 与 `resets_at` 才有意义。
fn window_elapsed(metric: &UsageMetric, now_ms: u64) -> Option<f32> {
    let window = metric.window_seconds.filter(|window| *window > 0)?;
    let remaining = reset_secs(metric, now_ms)?;
    Some((1.0 - remaining as f32 / window as f32).clamp(0.0, 1.0))
}

/// 额度条颜色：知道窗口已过比例时按「超前于时间」判色（用得比时间快 25 个点以上
/// 为红、10 个点以上为黄），否则回退 90 / 75 阈值。
pub(super) fn quota_color(percent: f32, elapsed: Option<f32>, palette: &Palette) -> Color {
    let expected = elapsed.map(|elapsed| elapsed * 100.0);
    if percent >= 90.0 || expected.is_some_and(|expected| percent >= expected + 25.0) {
        palette.red
    } else if percent >= 75.0 || expected.is_some_and(|expected| percent > expected + 10.0) {
        palette.yellow
    } else {
        palette.teal
    }
}

/// 定宽额度条：已用 `━`、未用 `░`（surface_dim，永远与边框线区分开）；非 Ready /
/// Warming 状态只画虚化占位，不给失效数据画确定的基线（F-2）。不改 Monitor 的满宽 `bar`。
pub(super) fn quota_bar(
    buffer: &mut Buffer,
    rect: Rect,
    percent: Option<f32>,
    elapsed: Option<f32>,
    status: ObservationStatus,
    palette: &Palette,
) {
    if rect.is_empty() {
        return;
    }
    let live = matches!(
        status,
        ObservationStatus::Ready | ObservationStatus::Warming
    );
    let fill = percent.filter(|_| live).map_or(0, |percent| {
        (rect.width as f32 * percent / 100.0).round() as u16
    });
    let color = quota_color(percent.unwrap_or(0.0), elapsed, palette);
    for x in 0..rect.width {
        if let Some(cell) = buffer.cell_mut((rect.x + x, rect.y)) {
            let filled = x < fill;
            cell.set_symbol(if filled { "━" } else { "░" })
                .set_style(Style::default().fg(if filled { color } else { palette.surface_dim }));
        }
    }
}

/// 流式排布：项从左到右摆放（项间 1 列间距），放不下换行，最多 `max_rows` 行；末行
/// 右侧保留 `reserve` 列给折叠标记。返回每项的 (x, row)（折叠掉的项为 `None`，一旦
/// 开始折叠后面全部折叠、保持顺序）与实际占用行数。单项比行宽还宽时原地放下、由绘制方
/// 截断，不会把整个项吞掉。
fn flow_positions(
    widths: &[u16],
    width: u16,
    max_rows: u16,
    reserve: u16,
) -> (Vec<Option<(u16, u16)>>, u16) {
    let max_rows = max_rows.max(1);
    let line_width = |row: u16| {
        if row + 1 >= max_rows {
            width.saturating_sub(reserve)
        } else {
            width
        }
    };
    let mut positions = Vec::with_capacity(widths.len());
    let (mut x, mut row) = (0_u16, 0_u16);
    let mut folded = false;
    for &item in widths {
        if folded {
            positions.push(None);
            continue;
        }
        if x > 0 && x.saturating_add(item) > line_width(row) {
            if row + 1 >= max_rows {
                folded = true;
                positions.push(None);
                continue;
            }
            row += 1;
            x = 0;
        }
        positions.push(Some((x, row)));
        x = x.saturating_add(item).saturating_add(1);
    }
    let rows = if widths.is_empty() { 0 } else { row + 1 };
    (positions, rows)
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

/// 卡片左缘的状态槽 `▌`：一眼分出每张卡的健康度。
fn status_slot(buffer: &mut Buffer, x: u16, y: u16, color: Color) {
    if let Some(cell) = buffer.cell_mut((x, y)) {
        cell.set_symbol("▌").set_style(Style::default().fg(color));
    }
}

/// 指标行之间的竖线分隔 ` │ `，返回占用的列数。
fn column_separator(
    buffer: &mut Buffer,
    x: u16,
    y: u16,
    glyphs: crate::ui::BorderGlyphs,
    palette: &Palette,
) -> u16 {
    if let Some(cell) = buffer.cell_mut((x + 1, y)) {
        cell.set_symbol(glyphs.vertical)
            .set_style(Style::default().fg(palette.surface_dim));
    }
    3
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
}

/// 渲染纯函数：`page` 是本次要画的页面（停靠面板由调用方决定画哪个 tab），
/// 状态只读；`draw_hover` 为真时画可见的悬浮层（agent 行悬浮只在没有页面时，
/// 总览浮层不受页面影响），进程对话框总是最后覆盖。调色板、组件 token 与
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
    if let Some(page) = page {
        page_rect = area;
        buffer.set_style(area, Style::default().fg(palette.text).bg(palette.panel_bg));
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                buffer[(x, y)].set_symbol(" ");
            }
        }
        let inner = block(buffer, area, tr(" MONITOR ", " 监控 "), cx);
        // Single navigation level: page tabs only. Refresh/pause/close live
        // on keyboard shortcuts (see footer) so the row never mixes
        // navigation with actions.
        let mut x = inner.x;
        for (label, tab) in [
            (tr("System", "系统"), Page::Monitor),
            (tr("Accounts", "账号"), Page::Accounts),
            (tr("Settings", "设置"), Page::Settings),
        ] {
            if x >= inner.right() {
                break;
            }
            page_tab(
                buffer,
                Rect::new(x, inner.y, inner.right() - x, 1),
                label,
                page == tab,
                Action::Page(tab),
                palette,
                &mut hits,
            );
            x = x.saturating_add(UnicodeWidthStr::width(label) as u16 + 3);
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
            Page::Monitor => monitor(buffer, body, state, cx, &mut hits),
            Page::Accounts => accounts(buffer, body, state, palette, &mut hits),
            Page::Settings => settings(buffer, body, state, palette, &mut hits),
        }
        let footer = state.message.as_deref().unwrap_or(tr(
            "1/2/3 pages · r refresh · Space pause · Esc close",
            "1/2/3 切页 · r 刷新 · 空格 暂停 · Esc 关闭",
        ));
        text(
            buffer,
            Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1),
            0,
            footer,
            Style::default().fg(palette.overlay0),
        );
    }
    let mut hover_rect = Rect::default();
    let mut hover_hits = Vec::new();
    // agent 行悬浮只在没有页面时画（停靠面板的全局 pass / 经典布局无页面）；
    // 总览浮层锚在侧栏按钮上，经典布局页面打开时同样放行。
    let hover = state
        .hover
        .as_ref()
        .filter(|hover| draw_hover && hover.visible && (page.is_none() || hover.is_overview()));
    if let Some(hover) = hover {
        match &hover.target {
            HoverTarget::Agent { agent, .. } => {
                let width = buffer.area.width.saturating_sub(2).min(68);
                let height = buffer.area.height.saturating_sub(2).min(17);
                let x = hover
                    .anchor
                    .right()
                    .saturating_add(1)
                    .min(buffer.area.right().saturating_sub(width));
                let y = hover
                    .anchor
                    .y
                    .min(buffer.area.bottom().saturating_sub(height));
                hover_rect = Rect::new(x, y, width, height);
                clear(buffer, hover_rect);
                let inner = block(
                    buffer,
                    hover_rect,
                    &format!(" {} · {} ", agent, tr("Account usage", "账号用量")),
                    cx,
                );
                accounts_body(
                    buffer,
                    Rect::new(
                        inner.x,
                        inner.y,
                        inner.width,
                        inner.height.saturating_sub(2),
                    ),
                    state,
                    &hover_scope(state),
                    palette,
                    &mut hover_hits,
                );
                // 底行只留「打开页面」：绑定 / 刷新 / 回调等动作都在正文自带的动作行里，
                // 不再出现两个「绑定账号」（ACC-02）。
                let y = inner.bottom().saturating_sub(1);
                secondary_button(
                    buffer,
                    Rect::new(inner.x, y, inner.width.min(24), 1),
                    tr("Open page", "打开页面"),
                    Action::Page(Page::Accounts),
                    palette,
                    &mut hover_hits,
                );
            }
            HoverTarget::UsageOverview => {
                let scope = hover_scope(state);
                let (width, height) = usage_hover_size(state, &scope, buffer.area);
                // 按钮下方左对齐；下方放不下就上翻到按钮上方。两侧都放不下时取
                // 空间更大的一侧并收缩高度：浮层永远不盖住按钮本身（否则按钮上的
                // 点击会落进浮层独占分支、变成「打开账号页」而不是取消钉住）。
                let x = hover
                    .anchor
                    .x
                    .min(buffer.area.right().saturating_sub(width));
                let (y, height) = usage_hover_placement(hover.anchor, height, buffer.area);
                hover_rect = Rect::new(x, y, width, height);
                clear(buffer, hover_rect);
                let inner = block(
                    buffer,
                    hover_rect,
                    &format!(" {} ", tr("Usage overview", "用量总览")),
                    cx,
                );
                usage_hover(
                    buffer,
                    inner,
                    state,
                    &scope,
                    hover.pinned,
                    palette,
                    &mut hover_hits,
                );
            }
        }
    }
    let mut dialog_rect = Rect::default();
    if let Some(dialog) = &state.process_dialog {
        hover_rect = Rect::default();
        hover_hits.clear();
        hits.clear();
        let rect = crate::ui::centered_popup_rect(buffer.area, 66, 12).unwrap_or(buffer.area);
        dialog_rect = rect;
        for y in rect.y..rect.bottom() {
            for x in rect.x..rect.right() {
                buffer[(x, y)].set_symbol(" ");
            }
        }
        let inner = block(buffer, rect, tr(" PROCESS DETAILS ", " 进程详情 "), cx);
        let host = state
            .metrics
            .as_ref()
            .map(|value| value.hostname.as_str())
            .unwrap_or("—");
        text(
            buffer,
            inner,
            0,
            &format!(
                "{host} · PID {} · {}",
                dialog.process.identity.pid, dialog.process.name
            ),
            Style::default()
                .fg(palette.text)
                .add_modifier(Modifier::BOLD),
        );
        text(
            buffer,
            inner,
            2,
            &format!(
                "CPU {}   RAM {}   {}",
                percent(dialog.process.cpu_percent),
                bytes(dialog.process.memory_bytes),
                dialog.process.status
            ),
            Style::default().fg(palette.overlay1),
        );
        text(
            buffer,
            inner,
            3,
            dialog.process.executable.as_deref().unwrap_or("—"),
            Style::default().fg(palette.overlay0),
        );
        hits.clear();
        let y = inner.bottom().saturating_sub(1);
        secondary_button(
            buffer,
            Rect::new(inner.x, y, 16.min(inner.width), 1),
            tr("Cancel", "取消"),
            Action::CancelProcess,
            palette,
            &mut hits,
        );
        if dialog.process.protected || dialog.process.action_token.is_none() {
            text(
                buffer,
                inner,
                5,
                tr(
                    "This process cannot be safely terminated here.",
                    "此进程受保护，或系统无法取得安全终止句柄。",
                ),
                Style::default().fg(palette.yellow),
            );
        } else if dialog.pending {
            text(
                buffer,
                inner,
                5,
                tr("Sending termination…", "正在发送结束请求…"),
                Style::default().fg(palette.yellow),
            );
        } else if dialog.confirm {
            text(
                buffer,
                inner,
                5,
                if dialog.force {
                    tr(
                        "Confirm force termination. Unsaved work may be lost.",
                        "确认强制结束此进程，未保存的数据可能丢失。",
                    )
                } else {
                    tr("Confirm ending this process.", "确认结束此进程。")
                },
                Style::default().fg(palette.red),
            );
            // 结束进程是破坏性操作：与浮层的 Danger 按钮同色，不再和「取消」
            // 长相一样（C-29）。
            button(
                buffer,
                Rect::new(
                    inner.x.saturating_add(18),
                    y,
                    inner.width.saturating_sub(18),
                    1,
                ),
                tr("Confirm", "确认执行"),
                crate::ui::ModalButtonTone::Danger,
                crate::ui::ModalButtonState::Normal,
                Action::ConfirmProcess,
                palette,
                &mut hits,
            );
        } else {
            button(
                buffer,
                Rect::new(
                    inner.x.saturating_add(18),
                    y,
                    inner.width.saturating_sub(18).min(18),
                    1,
                ),
                tr("Terminate", "正常结束"),
                crate::ui::ModalButtonTone::Danger,
                crate::ui::ModalButtonState::Normal,
                Action::Terminate(false),
                palette,
                &mut hits,
            );
            button(
                buffer,
                Rect::new(
                    inner.x.saturating_add(38),
                    y,
                    inner.width.saturating_sub(38),
                    1,
                ),
                tr("Force", "强制结束"),
                crate::ui::ModalButtonTone::Danger,
                crate::ui::ModalButtonState::Normal,
                Action::Terminate(true),
                palette,
                &mut hits,
            );
        }
    }
    PaintOutput {
        hits,
        hover_hits,
        page_rect,
        hover_rect,
        dialog_rect,
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

fn monitor(
    buffer: &mut Buffer,
    area: Rect,
    state: &State,
    cx: &ChromeContext<'_>,
    hits: &mut Vec<(Rect, Action)>,
) {
    let palette = cx.palette;
    let Some(sample) = state.metrics.as_ref() else {
        text(
            buffer,
            area,
            1,
            tr("Connecting to the host sampler…", "正在连接主机采样器…"),
            Style::default().fg(palette.overlay0),
        );
        return;
    };
    if sample.sampled_at_ms == 0 {
        text(
            buffer,
            area,
            1,
            tr("Waiting for the first sample…", "等待第一份有效采样…"),
            Style::default().fg(palette.overlay0),
        );
        return;
    }
    text(
        buffer,
        area,
        0,
        &format!(
            "{} · {} · {} · {} ms",
            sample.hostname,
            sample.environment,
            status(sample.status),
            sample.interval_ms
        ),
        Style::default().fg(palette.overlay1),
    );
    let visible = &state.monitor.visible;
    let sections = visible
        .iter()
        .filter(|id| {
            [
                "cpu",
                "cores",
                "memory",
                "gpu",
                "disks",
                "network",
                "sensors",
                "processes",
            ]
            .contains(&id.as_str())
        })
        .collect::<Vec<_>>();
    let columns = if area.width >= 70 { 2 } else { 1 };
    let gap = 1_u16;
    let width = area.width.saturating_sub(gap * (columns - 1)) / columns;
    let card_height = state
        .monitor
        .card_height
        .clamp(7, 24)
        .min(area.height.saturating_sub(2).max(7));
    let start = state.scroll.min(sections.len().saturating_sub(1));
    for (index, section) in sections.iter().enumerate().skip(start) {
        let position = (index - start) as u16;
        let x = area.x + (position % columns) * (width + gap);
        let y = area.y + 2 + (position / columns) * (card_height + 1);
        if y >= area.bottom() {
            break;
        }
        let rect = Rect::new(x, y, width, card_height.min(area.bottom() - y));
        let title = section_title(section);
        let inner = block(buffer, rect, title, cx);
        hits.push((rect, Action::Card((*section).clone())));
        if rect.width > 18 {
            secondary_button(
                buffer,
                Rect::new(rect.right() - 8, rect.y, 3, 1),
                "↑",
                Action::CardMove((*section).clone(), -1),
                palette,
                hits,
            );
            secondary_button(
                buffer,
                Rect::new(rect.right() - 4, rect.y, 3, 1),
                "↓",
                Action::CardMove((*section).clone(), 1),
                palette,
                hits,
            );
        }
        let offset = state
            .card_scroll
            .get(section.as_str())
            .copied()
            .unwrap_or(0);
        if let Some(status_value) = sample
            .group_status
            .get(section.as_str())
            .filter(|value| **value != ObservationStatus::Ready)
        {
            text(
                buffer,
                inner,
                inner.height.saturating_sub(1),
                status(*status_value),
                Style::default().fg(palette.yellow),
            );
        }
        if inner.is_empty() {
            continue;
        }
        match section.as_str() {
            "cpu" => {
                text(
                    buffer,
                    inner,
                    0,
                    &format!(
                        "{}   {} {}",
                        percent(sample.cpu_percent),
                        sample.cores.len(),
                        tr("logical CPUs", "逻辑核")
                    ),
                    Style::default()
                        .fg(palette.text)
                        .add_modifier(Modifier::BOLD),
                );
                text(
                    buffer,
                    inner,
                    1,
                    &sample.cpu_brand,
                    Style::default().fg(palette.overlay0),
                );
                let data = history(state, inner.width, |point| {
                    state
                        .selected_core
                        .map_or(point.cpu, |core| point.cores.get(core).copied().flatten())
                });
                if let Some(core) = state.selected_core {
                    text(
                        buffer,
                        inner,
                        1,
                        &format!("CPU {core} · {} min", state.monitor.history_minutes),
                        Style::default().fg(palette.accent),
                    );
                }
                if inner.height > 2 {
                    Sparkline::default()
                        .data(&data)
                        .max(100)
                        .style(Style::default().fg(palette.teal))
                        .render(
                            Rect::new(inner.x, inner.y + 2, inner.width, inner.height - 2),
                            buffer,
                        );
                }
            }
            "cores" => {
                let slots = (inner.width / 13).max(1);
                for (index, core) in sample
                    .cores
                    .iter()
                    .skip(offset)
                    .take(usize::from(inner.height * slots))
                    .enumerate()
                {
                    let row = index as u16 / slots;
                    let x = inner.x + (index as u16 % slots) * 13;
                    text(
                        buffer,
                        Rect::new(x, inner.y + row, 12.min(inner.right().saturating_sub(x)), 1),
                        0,
                        &format!("{:>3} {:>6}", core.id, percent(core.usage_percent)),
                        Style::default().fg(if core.usage_percent.is_some_and(|v| v >= 90.0) {
                            palette.red
                        } else {
                            palette.teal
                        }),
                    );
                    hits.push((
                        Rect::new(x, inner.y + row, 12.min(inner.right().saturating_sub(x)), 1),
                        Action::Core(core.id),
                    ));
                }
            }
            "memory" => {
                let memory = &sample.memory;
                let usage = (memory.total_bytes > 0)
                    .then(|| memory.used_bytes as f32 / memory.total_bytes as f32 * 100.0);
                text(
                    buffer,
                    inner,
                    0,
                    &format!(
                        "{} / {}",
                        bytes(memory.used_bytes),
                        bytes(memory.total_bytes)
                    ),
                    Style::default()
                        .fg(palette.text)
                        .add_modifier(Modifier::BOLD),
                );
                if inner.height > 1 {
                    bar(
                        buffer,
                        Rect::new(inner.x, inner.y + 1, inner.width, 1),
                        usage,
                        palette,
                    );
                }
                text(
                    buffer,
                    inner,
                    3,
                    &format!(
                        "Swap {} / {}",
                        bytes(memory.swap_used_bytes),
                        bytes(memory.swap_total_bytes)
                    ),
                    Style::default().fg(palette.overlay1),
                );
                if inner.height > 4 {
                    bar(
                        buffer,
                        Rect::new(inner.x, inner.y + 4, inner.width, 1),
                        (memory.swap_total_bytes > 0).then(|| {
                            memory.swap_used_bytes as f32 / memory.swap_total_bytes as f32 * 100.0
                        }),
                        palette,
                    );
                }
                if inner.height > 6 {
                    Sparkline::default()
                        .data(history(state, inner.width, |point| point.memory))
                        .max(100)
                        .style(Style::default().fg(palette.mauve))
                        .render(
                            Rect::new(inner.x, inner.y + 6, inner.width, inner.height - 6),
                            buffer,
                        );
                }
            }
            "gpu" => {
                if sample.gpus.is_empty() {
                    text(
                        buffer,
                        inner,
                        0,
                        tr("No GPU data available", "无可用 GPU 数据"),
                        Style::default().fg(palette.overlay0),
                    );
                }
                for (index, gpu) in sample
                    .gpus
                    .iter()
                    .filter(|gpu| !state.monitor.hidden_devices.contains(&gpu.id))
                    .skip(offset)
                    .take((inner.height as usize).div_ceil(3))
                    .enumerate()
                {
                    let row = index as u16 * 3;
                    text(
                        buffer,
                        inner,
                        row,
                        &format!("{}  {}", gpu.name, percent(gpu.usage_percent)),
                        Style::default().fg(if gpu.usage_percent.is_some_and(|v| v >= 90.0) {
                            palette.red
                        } else if gpu.usage_percent.is_some_and(|v| v >= 75.0) {
                            palette.yellow
                        } else {
                            palette.teal
                        }),
                    );
                    text(
                        buffer,
                        inner,
                        row + 1,
                        &format!(
                            "VRAM {} / {}  {}",
                            gpu.memory_used_bytes
                                .map(bytes)
                                .unwrap_or_else(|| "—".into()),
                            gpu.memory_total_bytes
                                .map(bytes)
                                .unwrap_or_else(|| "—".into()),
                            gpu.temperature_celsius
                                .map(|v| format!("{v:.0}°C"))
                                .unwrap_or_else(|| "—".into())
                        ),
                        Style::default().fg(palette.overlay1),
                    );
                    if let Some(message) = &gpu.message {
                        text(
                            buffer,
                            inner,
                            row + 2,
                            message,
                            Style::default().fg(palette.overlay0),
                        );
                    }
                }
            }
            "disks" => {
                for (index, disk) in sample
                    .disks
                    .iter()
                    .filter(|disk| !state.monitor.hidden_devices.contains(&disk.id))
                    .skip(offset)
                    .take(inner.height as usize)
                    .enumerate()
                {
                    let used = disk.total_bytes.saturating_sub(disk.available_bytes);
                    let usage_pct = (disk.total_bytes > 0)
                        .then(|| used as f32 / disk.total_bytes as f32 * 100.0);
                    text(
                        buffer,
                        inner,
                        index as u16,
                        &format!(
                            "{}  {} / {}  {}",
                            disk.mount_point,
                            bytes(used),
                            bytes(disk.total_bytes),
                            percent(usage_pct)
                        ),
                        Style::default().fg(if usage_pct.is_some_and(|value| value >= 90.0) {
                            palette.red
                        } else if usage_pct.is_some_and(|value| value >= 75.0) {
                            palette.yellow
                        } else {
                            palette.text
                        }),
                    );
                }
            }
            "network" => {
                for (index, net) in sample
                    .networks
                    .iter()
                    .filter(|net| !state.monitor.hidden_devices.contains(&net.id))
                    .skip(offset)
                    .take(inner.height as usize)
                    .enumerate()
                {
                    text(
                        buffer,
                        inner,
                        index as u16,
                        &format!(
                            "{}  ↓{}/s  ↑{}/s",
                            net.id,
                            net.received_bytes_per_second
                                .map(|v| bytes(v.max(0.0) as u64))
                                .unwrap_or_else(|| "—".into()),
                            net.transmitted_bytes_per_second
                                .map(|v| bytes(v.max(0.0) as u64))
                                .unwrap_or_else(|| "—".into())
                        ),
                        Style::default().fg(palette.teal),
                    );
                }
            }
            "sensors" => {
                if sample.sensors.is_empty() {
                    text(
                        buffer,
                        inner,
                        0,
                        tr(
                            "Sensors unavailable in this environment",
                            "当前环境未提供温度传感器",
                        ),
                        Style::default().fg(palette.overlay0),
                    );
                }
                for (index, sensor) in sample
                    .sensors
                    .iter()
                    .filter(|sensor| {
                        !state
                            .monitor
                            .hidden_devices
                            .contains(&format!("sensor:{}", sensor.name))
                    })
                    .skip(offset)
                    .take(inner.height as usize)
                    .enumerate()
                {
                    text(
                        buffer,
                        inner,
                        index as u16,
                        &format!(
                            "{}  {}",
                            sensor.name,
                            sensor
                                .temperature_celsius
                                .map(|v| format!("{v:.1}°C"))
                                .unwrap_or_else(|| "—".into())
                        ),
                        Style::default().fg(palette.text),
                    );
                }
            }
            _ => {
                let filter = state.process_filter.to_lowercase();
                let mut processes = sample
                    .processes
                    .iter()
                    .filter(|process| process.name.to_lowercase().contains(&filter))
                    .collect::<Vec<_>>();
                processes.sort_by(|a, b| match state.process_sort {
                    ProcessSort::Memory => b.memory_bytes.cmp(&a.memory_bytes),
                    ProcessSort::Name => a.name.cmp(&b.name),
                    _ => b
                        .cpu_percent
                        .unwrap_or(-1.0)
                        .total_cmp(&a.cpu_percent.unwrap_or(-1.0)),
                });
                secondary_button(
                    buffer,
                    Rect::new(inner.x, inner.y, inner.width.min(15), 1),
                    tr("Sort", "排序"),
                    Action::SortProcesses,
                    palette,
                    hits,
                );
                secondary_button(
                    buffer,
                    Rect::new(inner.x + 16, inner.y, inner.width.saturating_sub(16), 1),
                    tr("Filter /", "筛选 /"),
                    Action::FilterProcesses,
                    palette,
                    hits,
                );
                for (index, process) in processes
                    .into_iter()
                    .skip(offset)
                    .take(inner.height.saturating_sub(1) as usize)
                    .enumerate()
                {
                    let row = Rect::new(inner.x, inner.y + index as u16 + 1, inner.width, 1);
                    text(
                        buffer,
                        row,
                        0,
                        &format!(
                            "{:>6} {:>6} {:>9} {}",
                            process.identity.pid,
                            percent(process.cpu_percent),
                            bytes(process.memory_bytes),
                            process.name
                        ),
                        Style::default().fg(palette.text),
                    );
                    hits.push((row, Action::Process(process.identity.clone())));
                }
            }
        }
    }
}

/// 厂商 chip 的外观。
struct ChipStyle {
    /// 已选中：反色成 accent（与 `page_tab` 同一套语言）。
    active: bool,
    /// 官方 CLI 未安装（仅因显式配置账号而列出）：降色。
    dimmed: bool,
    /// 状态圆点颜色；`None` 不画圆点（「全部厂商」）。
    dot: Option<Color>,
}

/// 工具栏 chip：` ● 标签 `，选中反色、未安装降色、圆点按状态着色。
fn chip(
    buffer: &mut Buffer,
    rect: Rect,
    label: &str,
    style: ChipStyle,
    action: Action,
    palette: &Palette,
    hits: &mut Vec<(Rect, Action)>,
) {
    let text_value = if style.dot.is_some() {
        format!(" ● {label} ")
    } else {
        format!(" {label} ")
    };
    let width = (UnicodeWidthStr::width(text_value.as_str()) as u16).min(rect.width);
    let rect = Rect::new(rect.x, rect.y, width, rect.height.min(1));
    if rect.is_empty() {
        return;
    }
    let base = if style.active {
        Style::default()
            .fg(palette.panel_bg)
            .bg(palette.accent)
            .add_modifier(Modifier::BOLD)
    } else if style.dimmed {
        Style::default().fg(palette.overlay0).bg(palette.surface0)
    } else {
        Style::default().fg(palette.text).bg(palette.surface0)
    };
    buffer.set_style(rect, base);
    text(buffer, rect, 0, &text_value, base);
    if let Some(dot) = style.dot {
        if let Some(cell) = buffer.cell_mut((rect.x + 1, rect.y)) {
            cell.set_style(base.fg(dot));
        }
    }
    hits.push((rect, action));
}

/// 状态的「严重度」：chip 圆点取该厂商已加载账号里最严重的一档。
fn status_severity(status: ObservationStatus) -> u8 {
    match status {
        ObservationStatus::Unavailable
        | ObservationStatus::Unsupported
        | ObservationStatus::PermissionDenied
        | ObservationStatus::Error => 4,
        ObservationStatus::NotAuthenticated | ObservationStatus::NeedsBinding => 3,
        ObservationStatus::Warming => 2,
        ObservationStatus::Ready | ObservationStatus::Stale | ObservationStatus::Unknown => 1,
    }
}

/// 厂商 chip 的状态圆点：按该厂商已加载账号里最需要注意的状态着色；还没有加载任何
/// 账号（未选中它、或数据未到）时用中性色。
fn provider_dot(accounts: &[AccountUsageSnapshot], agent: &str, palette: &Palette) -> Color {
    accounts
        .iter()
        .filter(|account| account.agent == agent)
        .map(|account| account.status)
        .max_by_key(|status| status_severity(*status))
        .map_or(palette.overlay0, |status| status_color(status, palette))
}

/// 工具栏里一个动作：`None` 动作是禁用 / 状态提示（灰色、不回填命中区）。
struct ToolbarItem {
    label: String,
    action: Option<Action>,
}

impl ToolbarItem {
    fn width(&self) -> u16 {
        (UnicodeWidthStr::width(self.label.as_str()) as u16).saturating_add(2)
    }
}

/// 「刷新」项：本地强意图刷新在途时原位变成「刷新中…」，显式刷新被防抖 / 厂商退避时
/// 变成「N 秒后可刷新」；两者都是状态提示，不回填命中区。服务端探测在途（订阅期间
/// 可能长期为真）只在标题行 / 状态列表达，不锁按钮。
fn refresh_item(scope: &AccountsScope<'_>, now_ms: u64) -> ToolbarItem {
    if scope.refreshing {
        return ToolbarItem {
            label: tr("Refreshing…", "刷新中…").to_owned(),
            action: None,
        };
    }
    match scope.refresh_wait_secs(now_ms) {
        Some(secs) => ToolbarItem {
            label: if zh() {
                format!("{secs} 秒后可刷新")
            } else {
                format!("Refresh in {secs}s")
            },
            action: None,
        },
        None => ToolbarItem {
            label: tr("Refresh", "刷新").to_owned(),
            action: Some(Action::Refresh),
        },
    }
}

/// 所选厂商的官方回调开关状态：厂商不支持回调开关（服务端未宣告
/// `supports_callback`，含旧 server）或未选厂商时 `None`。支持时给出作用对象账号
/// （作用域内显式选中或唯一的账号，与 `Action::UsageIntegration` 取的目标同源）及其
/// 服务端判定的接入态（`UsageRefreshState.callback_enabled`，缺失 = 未知）：显示态与
/// 动作作用于同一个账号，悬浮在远端 pane 上时也取远端返回的状态。能力只信服务端宣告，
/// 客户端不维护厂商名单。
fn callback_state<'a>(
    state: &State,
    scope: &AccountsScope<'a>,
) -> Option<(Option<&'a str>, Option<bool>)> {
    let agent = scope.provider?;
    state
        .providers
        .iter()
        .any(|info| info.agent == agent && info.supports_callback)
        .then(|| {
            let account = scope.scoped_account();
            let enabled = account
                .and_then(|id| scope.refresh_of(id))
                .and_then(|refresh| refresh.callback_enabled);
            (account, enabled)
        })
}

/// 官方回调单开关（F11）：显示作用对象账号的当前态，点击切到相反态；状态未知时提供
/// 「启用」；多账号且未选时没有作用对象，禁用并标注「先选账号」。
fn callback_item(state: &State, scope: &AccountsScope<'_>) -> Option<ToolbarItem> {
    let (account, enabled) = callback_state(state, scope)?;
    let texts = &crate::i18n::texts().monitor;
    if account.is_none() {
        return Some(ToolbarItem {
            label: format!("{} · {}", texts.callback_toggle, texts.select_account_first),
            action: None,
        });
    }
    let (glyph, next) = match enabled {
        Some(true) => ("✓", false),
        Some(false) => ("○", true),
        None => ("?", true),
    };
    Some(ToolbarItem {
        label: format!("{glyph} {}", texts.callback_toggle),
        action: Some(Action::UsageIntegration(next)),
    })
}

/// 「切换账号」：少于两个候选账号时禁用态。只探第二个候选是否存在，渲染期不分配。
fn cycle_item(state: &State, provider: Option<&str>) -> ToolbarItem {
    ToolbarItem {
        label: tr("Switch account", "切换账号").to_owned(),
        action: state
            .cycle_candidates(provider)
            .nth(1)
            .map(|_| Action::CycleAccount),
    }
}

fn source_item() -> ToolbarItem {
    ToolbarItem {
        label: tr("Official source", "官方查询说明").to_owned(),
        action: Some(Action::Source),
    }
}

/// 视图切换项：标签写的是目标视图。
fn format_item(state: &State) -> ToolbarItem {
    let texts = &crate::i18n::texts().monitor;
    let target = match state.usage.format {
        UsageDisplayFormat::Dashboard => texts.format_table,
        UsageDisplayFormat::Table => texts.format_dashboard,
    };
    ToolbarItem {
        label: format!("⇄ {target}"),
        action: Some(Action::UsageFormat),
    }
}

/// 把一个工具栏项画在 `x` 处（禁用态灰色），返回占用宽度。
fn toolbar_item(
    buffer: &mut Buffer,
    rect: Rect,
    item: &ToolbarItem,
    palette: &Palette,
    hits: &mut Vec<(Rect, Action)>,
) -> u16 {
    match &item.action {
        Some(action) => {
            secondary_button(buffer, rect, &item.label, action.clone(), palette, hits);
            item.width().min(rect.width)
        }
        None => disabled_button(buffer, rect, &item.label, palette),
    }
}

/// 把工具栏项按流式排布画进 `area`（最多 `area.height` 行，放不下折叠成 `⋯`），
/// 返回占用行数。
fn toolbar_rows(
    buffer: &mut Buffer,
    area: Rect,
    items: &[ToolbarItem],
    palette: &Palette,
    hits: &mut Vec<(Rect, Action)>,
) -> u16 {
    if area.is_empty() || items.is_empty() {
        return 0;
    }
    let widths = items.iter().map(ToolbarItem::width).collect::<Vec<_>>();
    let (positions, rows) = flow_positions(&widths, area.width, area.height, 2);
    let mut folded = false;
    for (item, position) in items.iter().zip(positions) {
        let Some((x, row)) = position else {
            folded = true;
            continue;
        };
        let rect = Rect::new(area.x + x, area.y + row, area.width.saturating_sub(x), 1);
        toolbar_item(buffer, rect, item, palette, hits);
    }
    if folded {
        text(
            buffer,
            Rect::new(area.right().saturating_sub(1), area.y + rows - 1, 1, 1),
            0,
            "⋯",
            Style::default().fg(palette.overlay0),
        );
    }
    rows
}

/// 厂商 chip 行（≥60 列）：首项「全部厂商」，之后每个已列出的厂商一个 chip（状态圆点 +
/// 账号数）；最多两行，放不下折叠成「还有 N 个」。返回 (占用行数, 末行结束的 x)，
/// 动作区放得下时右对齐挂在末行。
fn provider_chips(
    buffer: &mut Buffer,
    area: Rect,
    state: &State,
    listed: &[&UsageProviderInfo],
    palette: &Palette,
    hits: &mut Vec<(Rect, Action)>,
) -> (u16, u16) {
    let texts = &crate::i18n::texts().monitor;
    let overview = state.selected_provider.is_none();
    let labels =
        std::iter::once(texts.all_providers.to_owned())
            .chain(listed.iter().map(|provider| {
                format!("{} {}", provider.label, provider.configured_accounts.len())
            }))
            .collect::<Vec<_>>();
    let widths = labels
        .iter()
        .enumerate()
        .map(|(index, label)| {
            (UnicodeWidthStr::width(label.as_str()) as u16).saturating_add(if index == 0 {
                2
            } else {
                4
            })
        })
        .collect::<Vec<_>>();
    let (positions, rows) = flow_positions(&widths, area.width, area.height.min(2), 12);
    let mut end = area.x;
    let mut folded = 0_usize;
    for (index, (label, position)) in labels.iter().zip(&positions).enumerate() {
        let Some((x, row)) = position else {
            folded += 1;
            continue;
        };
        let rect = Rect::new(area.x + x, area.y + row, area.width.saturating_sub(*x), 1);
        let (style, action) = if index == 0 {
            (
                ChipStyle {
                    active: overview,
                    dimmed: false,
                    dot: None,
                },
                Action::Overview,
            )
        } else {
            let provider = listed[index - 1];
            let active = state.selected_provider.as_deref() == Some(provider.agent.as_str());
            (
                ChipStyle {
                    active,
                    dimmed: provider.installed == Some(false),
                    dot: Some(provider_dot(&state.accounts, &provider.agent, palette)),
                },
                // 再点已选厂商回到总览。
                if active {
                    Action::Overview
                } else {
                    Action::Provider(provider.agent.clone())
                },
            )
        };
        chip(buffer, rect, label, style, action, palette, hits);
        let width = widths[index].min(rect.width);
        if *row + 1 == rows {
            end = rect.x.saturating_add(width).saturating_add(1);
        }
    }
    if folded > 0 {
        let note = crate::i18n::fill(texts.more_fmt, &[("n", &folded.to_string())]);
        let rect = Rect::new(
            end,
            area.y + rows.saturating_sub(1),
            area.right().saturating_sub(end),
            1,
        );
        text(
            buffer,
            rect,
            0,
            &note,
            Style::default().fg(palette.overlay0),
        );
        end = end.saturating_add(UnicodeWidthStr::width(note.as_str()) as u16 + 1);
    }
    if listed.is_empty() {
        let rect = Rect::new(end, area.y, area.right().saturating_sub(end), 1);
        let note = tr(
            "No installed agent CLI detected.",
            "未检测到已安装的 agent CLI。",
        );
        text(buffer, rect, 0, note, Style::default().fg(palette.overlay0));
        end = area.right();
    }
    (rows.max(1), end)
}

/// 窄面板（<60 列）的厂商选择器：`‹ ● 厂商 ›`，候选含「全部厂商」。
fn provider_picker(
    buffer: &mut Buffer,
    row: Rect,
    state: &State,
    listed: &[&UsageProviderInfo],
    palette: &Palette,
    hits: &mut Vec<(Rect, Action)>,
) {
    if row.is_empty() {
        return;
    }
    let texts = &crate::i18n::texts().monitor;
    // 候选表：None = 全部厂商。
    let entries = std::iter::once(None)
        .chain(listed.iter().map(|provider| Some(*provider)))
        .collect::<Vec<_>>();
    let index = entries
        .iter()
        .position(|entry| {
            entry.map(|provider| provider.agent.as_str()) == state.selected_provider.as_deref()
        })
        .unwrap_or(0);
    let action_of = |entry: Option<&UsageProviderInfo>| match entry {
        Some(provider) => Action::Provider(provider.agent.clone()),
        None => Action::Overview,
    };
    let previous = entries[(index + entries.len() - 1) % entries.len()];
    let next = entries[(index + 1) % entries.len()];
    secondary_button(
        buffer,
        Rect::new(row.x, row.y, 3.min(row.width), 1),
        "‹",
        action_of(previous),
        palette,
        hits,
    );
    let label_rect = Rect::new(row.x + 4, row.y, row.width.saturating_sub(8), 1);
    match entries[index] {
        Some(provider) => {
            let label = format!("{} {}", provider.label, provider.configured_accounts.len());
            chip(
                buffer,
                label_rect,
                &label,
                ChipStyle {
                    active: true,
                    dimmed: provider.installed == Some(false),
                    dot: Some(provider_dot(&state.accounts, &provider.agent, palette)),
                },
                Action::Overview,
                palette,
                hits,
            );
        }
        None => chip(
            buffer,
            label_rect,
            texts.all_providers,
            ChipStyle {
                active: true,
                dimmed: false,
                dot: None,
            },
            Action::Overview,
            palette,
            hits,
        ),
    }
    secondary_button(
        buffer,
        Rect::new(
            row.right().saturating_sub(3).max(row.x),
            row.y,
            3.min(row.width),
            1,
        ),
        "›",
        action_of(next),
        palette,
        hits,
    );
}

/// 汇总状态行：`3 个账号 · 2 已更新 · 1 需处理 · 13s 前更新`（只列非零项）。
fn summary_line(scope: &AccountsScope<'_>, now_ms: u64) -> String {
    let texts = &crate::i18n::texts().monitor;
    let count = |n: usize| n.to_string();
    let mut parts = vec![if scope.accounts.len() == 1 {
        texts.account_one.to_owned()
    } else {
        crate::i18n::fill(texts.accounts_fmt, &[("n", &count(scope.accounts.len()))])
    }];
    let (mut ready, mut loading, mut attention, mut failed) = (0, 0, 0, 0);
    for account in scope.accounts {
        let in_flight = scope
            .refresh_of(&account.account_id)
            .is_some_and(|state| state.in_flight);
        match account.status {
            _ if in_flight => loading += 1,
            ObservationStatus::Ready | ObservationStatus::Stale => ready += 1,
            ObservationStatus::Warming | ObservationStatus::Unknown => loading += 1,
            ObservationStatus::NotAuthenticated
            | ObservationStatus::NeedsBinding
            | ObservationStatus::PermissionDenied => attention += 1,
            ObservationStatus::Unavailable
            | ObservationStatus::Unsupported
            | ObservationStatus::Error => failed += 1,
        }
    }
    for (n, template) in [
        (ready, texts.summary_ready_fmt),
        (loading, texts.summary_loading_fmt),
        (attention, texts.summary_attention_fmt),
        (failed, texts.summary_failed_fmt),
    ] {
        if n > 0 {
            parts.push(crate::i18n::fill(template, &[("n", &count(n))]));
        }
    }
    if let Some(latest) = scope
        .accounts
        .iter()
        .map(|account| account.observed_at_ms)
        .max()
        .filter(|latest| *latest > 0)
    {
        parts.push(age_text(now_ms, latest, ObservationStatus::Ready));
    }
    parts.join(" · ")
}

/// 账号页（F-1）：工具栏（厂商 chip 行 + 动作区）、分隔线、正文（≥96 列双栏）、
/// 绑定行、汇总状态行；<60 列 chip 行折成选择器，<40 列只留头行 + 正文。行数按
/// 高度逐级让位（先让分隔线，再让绑定行，再让汇总行），正文至少保留 3 行。
fn accounts(
    buffer: &mut Buffer,
    area: Rect,
    state: &State,
    palette: &Palette,
    hits: &mut Vec<(Rect, Action)>,
) {
    if area.is_empty() {
        return;
    }
    if !state.usage.enabled {
        text(
            buffer,
            area,
            0,
            tr(
                "Account usage is disabled in settings.",
                "账号用量已在设置中关闭。",
            ),
            Style::default().fg(palette.overlay0),
        );
        return;
    }
    // 设置里关掉的厂商不进 chip 行 / 选择器：选中它只会得到一个永不发请求的页面。
    let listed = state
        .providers
        .iter()
        .filter(|provider| provider_listed(provider))
        .filter(|provider| !state.usage.disabled_providers.contains(&provider.agent))
        .collect::<Vec<_>>();
    let scope = page_scope(state);
    let narrow = area.width < 40;
    let compact = area.width < 60;

    // ---- 工具栏：厂商 chip 行 ----
    let (chip_rows, chip_end) = if compact {
        provider_picker(
            buffer,
            Rect::new(area.x, area.y, area.width, 1),
            state,
            &listed,
            palette,
            hits,
        );
        (1, area.right())
    } else {
        provider_chips(
            buffer,
            Rect::new(area.x, area.y, area.width, area.height.min(2)),
            state,
            &listed,
            palette,
            hits,
        )
    };
    let mut y = area.y.saturating_add(chip_rows);

    // ---- 工具栏：动作区 ----
    let mut action_rows = 0;
    if !narrow && y < area.bottom() {
        let mut items = vec![refresh_item(&scope, state.now_ms), format_item(state)];
        items.push(cycle_item(state, scope.provider));
        items.push(source_item());
        items.extend(callback_item(state, &scope));
        items.push(ToolbarItem {
            label: crate::i18n::texts().monitor.settings_button.to_owned(),
            action: Some(Action::Configure),
        });
        let total = items
            .iter()
            .map(|item| item.width() + 1)
            .sum::<u16>()
            .saturating_sub(1);
        let spare = area.right().saturating_sub(chip_end);
        if spare > total {
            // 放得下就右对齐挂在 chip 行末行，不另占一行。
            let mut x = area.right().saturating_sub(total);
            let row = Rect::new(x, y - 1, total, 1);
            for item in &items {
                let rect = Rect::new(x, row.y, row.right().saturating_sub(x), 1);
                let width = toolbar_item(buffer, rect, item, palette, hits);
                x = x.saturating_add(width).saturating_add(1);
            }
        } else {
            action_rows = toolbar_rows(
                buffer,
                Rect::new(area.x, y, area.width, (area.bottom() - y).min(2)),
                &items,
                palette,
                hits,
            );
        }
    }
    y = y.saturating_add(action_rows);

    // ---- 分隔线 / 正文 / 绑定行 / 汇总行：按剩余高度逐级让位 ----
    let rest = area.bottom().saturating_sub(y);
    let (mut sep, mut binding, mut status) = (0_u16, 0_u16, 0_u16);
    if !narrow {
        if rest >= 4 {
            status = 1;
        }
        if rest >= 5 {
            binding = 1;
        }
        if rest >= 6 {
            sep = 1;
        }
    }
    let [sep_rect, content, binding_rect, status_rect] = Layout::vertical([
        Constraint::Length(sep),
        Constraint::Min(0),
        Constraint::Length(binding),
        Constraint::Length(status),
    ])
    .areas(Rect::new(area.x, y, area.width, rest));
    if sep == 1 {
        rule(buffer, sep_rect, state.glyphs, palette);
    }
    accounts_content(buffer, content, state, &scope, palette, hits);
    if binding == 1 && scope.chrome == BodyChrome::Page {
        binding_row(buffer, binding_rect, &scope, palette, hits);
    }
    if status == 1 {
        text(
            buffer,
            status_rect,
            0,
            &summary_line(&scope, state.now_ms),
            Style::default().fg(palette.overlay1),
        );
    }
}

/// 账号正文：空态提示，或仪表盘 / 表格（≥96 列时右栏显示所选账号的详情）；
/// 强意图刷新在途时整块变暗。页面与 agent 行悬浮层共用。
fn accounts_content(
    buffer: &mut Buffer,
    area: Rect,
    state: &State,
    scope: &AccountsScope<'_>,
    palette: &Palette,
    hits: &mut Vec<(Rect, Action)>,
) {
    if area.is_empty() {
        return;
    }
    if scope.accounts.is_empty() {
        // 强意图刷新在途时显示「刷新中…」，而不是退回「请选择厂商」。
        text(
            buffer,
            area,
            0,
            if scope.refreshing {
                tr("Refreshing…", "刷新中…")
            } else {
                tr(
                    "Select a provider to inspect its official usage source.",
                    "请选择厂商以查询对应的官方用量。",
                )
            },
            Style::default().fg(palette.overlay0),
        );
        return;
    }
    let (main, detail) = if area.width >= 96 {
        let [main, detail] =
            Layout::horizontal([Constraint::Min(40), Constraint::Length(30)]).areas(area);
        (main, Some(detail))
    } else {
        (area, None)
    };
    if state.usage.format == UsageDisplayFormat::Table {
        usage_table(buffer, main, state, scope, palette, hits);
    } else {
        usage_dashboard(buffer, main, state, scope, palette, hits);
    }
    if let Some(detail) = detail {
        account_detail(buffer, detail, state, scope, palette);
    }
    if scope.refreshing {
        // 切换厂商 / 账号后旧快照保留但变暗，直到新数据到达。
        buffer.set_style(area, Style::default().add_modifier(Modifier::DIM));
    }
}

/// 右栏详情（≥96 列）：所选账号（否则第一个）的认证方式 / 厂商 / 来源 / 窗口 /
/// 更新时间 / 官方文档链接。左缘一条竖线与正文分开。
fn account_detail(
    buffer: &mut Buffer,
    area: Rect,
    state: &State,
    scope: &AccountsScope<'_>,
    palette: &Palette,
) {
    let texts = &crate::i18n::texts().monitor;
    for y in area.y..area.bottom() {
        if let Some(cell) = buffer.cell_mut((area.x, y)) {
            cell.set_symbol(state.glyphs.vertical)
                .set_style(Style::default().fg(palette.surface_dim));
        }
    }
    let inner = Rect::new(
        area.x.saturating_add(2),
        area.y,
        area.width.saturating_sub(2),
        area.height,
    );
    text(
        buffer,
        inner,
        0,
        texts.detail_title,
        Style::default()
            .fg(palette.accent)
            .add_modifier(Modifier::BOLD),
    );
    let account = scope
        .account
        .and_then(|id| {
            scope
                .accounts
                .iter()
                .find(|account| account.account_id == id)
        })
        .or_else(|| scope.accounts.first());
    let Some(account) = account else {
        text(
            buffer,
            inner,
            1,
            texts.detail_none,
            Style::default().fg(palette.overlay0),
        );
        return;
    };
    let windows = account
        .metrics
        .iter()
        .filter_map(|metric| metric.window_seconds.filter(|window| *window > 0))
        .map(span_text)
        .collect::<Vec<_>>()
        .join(" · ");
    let dash = |value: &str| {
        if value.is_empty() {
            "—".to_owned()
        } else {
            value.to_owned()
        }
    };
    let rows = [
        (texts.detail_auth, dash(&account.auth_mode)),
        (texts.detail_provider, dash(&account.provider)),
        (texts.detail_source, dash(&account.source)),
        (texts.detail_window, dash(&windows)),
        (
            texts.detail_updated,
            age_text(state.now_ms, account.observed_at_ms, account.status),
        ),
        (texts.detail_docs, dash(&account.source_url)),
    ];
    let label_width = rows
        .iter()
        .map(|(label, _)| UnicodeWidthStr::width(*label) as u16)
        .max()
        .unwrap_or(0)
        .saturating_add(1);
    for (index, (label, value)) in rows.iter().enumerate() {
        let row = index as u16 + 1;
        text(
            buffer,
            Rect::new(inner.x, inner.y, label_width, inner.height),
            row,
            label,
            Style::default().fg(palette.overlay0),
        );
        let color = if *label == texts.detail_updated {
            age_color(
                state.now_ms,
                account.observed_at_ms,
                account.status,
                palette,
            )
        } else {
            palette.text
        };
        text(
            buffer,
            Rect::new(
                inner.x.saturating_add(label_width),
                inner.y,
                inner.width.saturating_sub(label_width),
                inner.height,
            ),
            row,
            value,
            Style::default().fg(color),
        );
    }
    // 用量历史 sparkline：数据来自客户端记录的采样（`State::usage_history`），
    // 每次轮询响应 / 订阅事件按 `observed_at_ms` 前进追加一条。
    let history_row = rows.len() as u16 + 1;
    if inner.height <= history_row {
        return;
    }
    let samples = state
        .usage_history
        .get(&account.account_id)
        .map(VecDeque::as_slices)
        .map(|(front, back)| front.iter().chain(back.iter()).copied().collect::<Vec<_>>())
        .unwrap_or_default();
    let label = if samples.len() < 2 {
        format!(
            "{} · {}",
            texts.detail_history, texts.detail_history_waiting
        )
    } else {
        crate::i18n::fill(
            texts.detail_history_fmt,
            &[("count", &samples.len().to_string())],
        )
    };
    text(
        buffer,
        inner,
        history_row,
        &label,
        Style::default().fg(palette.overlay0),
    );
    if samples.len() >= 2 && inner.height > history_row + 1 {
        // 不足采样数的列由 `Sparkline` 自己留白；`max(100)` 与柱高口径一致。
        let data = samples
            .iter()
            .map(|sample| u64::from(sample.percent.clamp(0.0, 100.0).round() as u8))
            .collect::<Vec<_>>();
        Sparkline::default()
            .data(&data)
            .max(100)
            .style(Style::default().fg(quota_color(
                samples.last().map_or(0.0, |sample| sample.percent),
                None,
                palette,
            )))
            .render(
                Rect::new(
                    inner.x,
                    inner.y.saturating_add(history_row + 1),
                    inner.width,
                    inner.height.saturating_sub(history_row + 1),
                ),
                buffer,
            );
    }
}

fn metric_scope(scope: &str) -> &str {
    match scope {
        "session" | "local" => tr("session", "会话统计"),
        "api_key" => crate::i18n::texts().monitor.scope_api_key,
        "account" => tr("account", "账号"),
        other => other,
    }
}

/// 紧凑的单行用量（总览浮层 / 表格用量列）：`数量或百分比 ↻距重置`。
fn metric_value(metric: &UsageMetric, now_ms: u64) -> String {
    let quantity = metric_quantity(metric);
    let value = match (metric_percent(metric), quantity.as_str()) {
        (Some(percent), "—") => format!("{percent:.1}% {}", tr("used", "已用")),
        (Some(percent), quantity) => format!("{percent:.1}% · {quantity}"),
        (None, quantity) => quantity.to_owned(),
    };
    match reset_secs(metric, now_ms) {
        Some(seconds) => format!("{value} ↻{}", span_text(seconds)),
        None => value,
    }
}

/// 账号正文的宿主。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum BodyChrome {
    Page,
    Hover,
}

/// 账号正文的作用域视图：页面与悬浮层各自传入自己的数据，正文本身不区分来源。
pub(super) struct AccountsScope<'a> {
    pub accounts: &'a [AccountUsageSnapshot],
    /// 与 `accounts` 按 `account_id` 对齐的服务端刷新状态（旧 server 为空）。
    pub refresh_states: &'a [UsageRefreshState],
    pub provider: Option<&'a str>,
    pub account: Option<&'a str>,
    /// 本作用域可绑定的 pane；有值时才画「确认账号绑定」按钮。
    pub pane: Option<&'a str>,
    /// 页面作用域里 `pane` 的显示名（绑定行）。
    pub pane_label: Option<&'a str>,
    /// 正文的宿主：页面（动作在工具栏，正文下方是绑定行 + 汇总行）或 agent 行悬浮层
    /// （正文底部自带动作行；hover 的 pane 就是绑定目标）。
    pub chrome: BodyChrome,
    /// 本作用域有强意图刷新排队或在途：旧数据变暗，空态显示「刷新中…」。
    pub refreshing: bool,
    /// 本作用域的账号列表滚动位置。
    pub scroll: usize,
}

impl<'a> AccountsScope<'a> {
    fn refresh_of(&self, account_id: &str) -> Option<&UsageRefreshState> {
        self.refresh_states
            .iter()
            .find(|state| state.account_id == account_id)
    }

    /// 本作用域可操作的账号：显式选中的，否则作用域内唯一的账号；与
    /// `State::scoped_account` 同一口径，渲染的开关状态与动作目标才是同一个账号。
    fn scoped_account(&self) -> Option<&'a str> {
        self.account
            .or_else(|| (self.accounts.len() == 1).then(|| self.accounts[0].account_id.as_str()))
    }

    /// 显式刷新最早会被接受的时刻距现在的秒数：选中账号时只看它；否则取作用域内
    /// 的最小值——任一账号可刷新就允许点击（`account.usage.refresh` 按账号过闸门），
    /// 不让一个被重度防抖 / 退避的账号锁住整页。已到期或超过可信上限（厂商长退避、
    /// 端点时钟偏差）时为 `None`，超长退避在账号自己的行里说明（`refresh_note`）。
    fn refresh_wait_secs(&self, now_ms: u64) -> Option<u64> {
        let wait_of = |account_id: &str| {
            self.refresh_of(account_id)
                .and_then(|state| state.next_allowed_at_ms)
                .map_or(0, |at| at.saturating_sub(now_ms).div_ceil(1000))
        };
        let secs = match self.account {
            Some(account_id) => wait_of(account_id),
            None => self
                .accounts
                .iter()
                .map(|account| wait_of(&account.account_id))
                .min()
                .unwrap_or(0),
        };
        (secs > 0 && secs <= MAX_REFRESH_WAIT_SECS).then_some(secs)
    }

    /// 首个带候选账号的待办绑定（同一 pane 会挂在同 agent 的每个账号上，取第一个）。
    fn pending_binding(&self) -> Option<&UsagePendingBinding> {
        self.refresh_states
            .iter()
            .filter_map(|state| state.pending_binding.as_ref())
            .find(|pending| !pending.candidates.is_empty())
    }
}

/// 该账号的显式刷新等待是否超过「刷新」按钮的可信上限：厂商长退避（HTTP 429）
/// 时按钮不锁、但点了必被拒，改在账号自己的行里说明。
fn long_backoff_secs(state: &UsageRefreshState, now_ms: u64) -> Option<u64> {
    state
        .next_allowed_at_ms
        .map(|at| at.saturating_sub(now_ms).div_ceil(1000))
        .filter(|secs| *secs > MAX_REFRESH_WAIT_SECS)
}

/// 账号自己一行的服务端刷新状态说明：需确认目录信任 / 推断绑定 / 厂商长退避
/// （探测在途另在账号标题行与表格状态列体现）。`None` 表示不需要这一行。
fn refresh_note(state: Option<&UsageRefreshState>, now_ms: u64) -> Option<String> {
    let state = state?;
    let mut notes = Vec::new();
    if state.trust_required {
        notes.push(tr("Confirm folder trust in the CLI", "需在 CLI 中确认目录信任").to_owned());
    }
    if state.binding_inferred {
        notes.push(tr("binding inferred", "按唯一账号推断绑定").to_owned());
    }
    if let Some(secs) = long_backoff_secs(state, now_ms) {
        let minutes = secs.div_ceil(60);
        notes.push(if zh() {
            format!("厂商退避中，约 {minutes} 分钟后可刷新")
        } else {
            format!("vendor backoff, refresh in about {minutes} min")
        });
    }
    (!notes.is_empty()).then(|| notes.join(" · "))
}

/// 账号在表格模式下的状态列：探测在途 / 需确认目录信任优先于快照状态。
fn table_status(account: &AccountUsageSnapshot, state: Option<&UsageRefreshState>) -> String {
    match state {
        Some(state) if state.in_flight => tr("Refreshing…", "刷新中…").to_owned(),
        Some(state) if state.trust_required => tr("Confirm trust", "需确认目录信任").to_owned(),
        _ => status(account.status).to_owned(),
    }
}

/// 账号页 / 浮动仪表盘使用的页面作用域。
pub(super) fn page_scope(state: &State) -> AccountsScope<'_> {
    AccountsScope {
        accounts: &state.accounts,
        refresh_states: &state.refresh_states,
        provider: state.selected_provider.as_deref(),
        account: state.selected_account.as_deref(),
        pane: state.selected_pane.as_deref(),
        pane_label: state.selected_pane_label.as_deref(),
        chrome: BodyChrome::Page,
        refreshing: state.refreshing(),
        scroll: state.account_scroll,
    }
}

/// 悬浮层作用域：账号与 pane 都来自 `hover_scope`，选中账号沿用页面的高亮
/// （仅当它在悬浮层的账号里）。
pub(super) fn hover_scope(state: &State) -> AccountsScope<'_> {
    let account = state.selected_account.as_deref().filter(|selected| {
        state
            .hover_scope
            .accounts
            .iter()
            .any(|account| account.account_id == *selected)
    });
    AccountsScope {
        accounts: &state.hover_scope.accounts,
        refresh_states: &state.hover_scope.refresh_states,
        provider: state.hover_scope.provider.as_deref(),
        account,
        pane: state.hover_scope.pane.as_deref(),
        pane_label: None,
        chrome: BodyChrome::Hover,
        refreshing: state.hover_scope.refreshing(),
        scroll: state.hover_scope.scroll,
    }
}

/// 禁用态按钮：走组件的 `Disabled` 档（灰字 + 弱底色），不回填命中区；
/// 返回实际占用宽度。
fn disabled_button(buffer: &mut Buffer, rect: Rect, label: &str, palette: &Palette) -> u16 {
    let label = format!(" {label} ");
    let width = crate::ui::modal_button_width(&label).min(rect.width);
    let rect = Rect::new(rect.x, rect.y, width, rect.height.min(1));
    super::super::render::modal_button(
        buffer,
        rect,
        &label,
        crate::ui::ModalButtonTone::Secondary,
        crate::ui::ModalButtonState::Disabled,
        palette,
    );
    width
}

/// 一行内从左到右依次摆放的按钮：放下后把 `x` 推进到下一个起点（含 1 列间距），
/// 窄面板上后面的按钮自然截断而不是整个消失。
fn flow_button(
    buffer: &mut Buffer,
    x: &mut u16,
    row: Rect,
    label: &str,
    action: Action,
    palette: &Palette,
    hits: &mut Vec<(Rect, Action)>,
) {
    let rect = Rect::new(*x, row.y, row.right().saturating_sub(*x), 1);
    let width = (UnicodeWidthStr::width(label) as u16)
        .saturating_add(2)
        .min(rect.width);
    secondary_button(buffer, rect, label, action, palette, hits);
    *x = x.saturating_add(width).saturating_add(1);
}

/// 一行内从左到右摆放的说明文字：写下后把 `x` 推进到下一个起点（含 1 列间距）。
fn flow_text(buffer: &mut Buffer, x: &mut u16, row: Rect, value: &str, style: Style) {
    let rect = Rect::new(*x, row.y, row.right().saturating_sub(*x), 1);
    text(buffer, rect, 0, value, style);
    let width = (UnicodeWidthStr::width(value) as u16).min(rect.width);
    *x = x.saturating_add(width).saturating_add(1);
}

/// 账号页的绑定行：有待办绑定时给出一键绑定（服务端被拒的回调 pane → 候选账号），
/// 否则是 pane 选择器 +「绑定到聚焦 pane」。
fn binding_row(
    buffer: &mut Buffer,
    row: Rect,
    scope: &AccountsScope<'_>,
    palette: &Palette,
    hits: &mut Vec<(Rect, Action)>,
) {
    let mut x = row.x;
    if let Some(pending) = scope.pending_binding() {
        let label = if zh() {
            format!("待绑定 {} · {} →", pending.pane_id, pending.agent)
        } else {
            format!("Pending {} · {} →", pending.pane_id, pending.agent)
        };
        flow_text(
            buffer,
            &mut x,
            row,
            &label,
            Style::default().fg(palette.yellow),
        );
        for candidate in pending.candidates.iter().take(3) {
            flow_button(
                buffer,
                &mut x,
                row,
                candidate,
                Action::BindTo(pending.pane_id.clone(), candidate.clone()),
                palette,
                hits,
            );
        }
        return;
    }
    flow_text(
        buffer,
        &mut x,
        row,
        tr("Pane", "绑定 pane"),
        Style::default().fg(palette.overlay1),
    );
    flow_button(
        buffer,
        &mut x,
        row,
        "‹",
        Action::CyclePane(-1),
        palette,
        hits,
    );
    let (label, style) = match scope.pane_label.or(scope.pane) {
        Some(label) => (label, Style::default().fg(palette.text)),
        None => (
            tr("none selected", "未选择"),
            Style::default().fg(palette.overlay0),
        ),
    };
    let width = (UnicodeWidthStr::width(label) as u16).min(24);
    let rect = Rect::new(x, row.y, width.min(row.right().saturating_sub(x)), 1);
    text(buffer, rect, 0, label, style);
    x = x.saturating_add(rect.width).saturating_add(1);
    flow_button(
        buffer,
        &mut x,
        row,
        "›",
        Action::CyclePane(1),
        palette,
        hits,
    );
    // 选中 pane 后「确认账号绑定」是主动作，排在前面（窄面板先保它）；没有目标时
    // 不给一个必然失败的按钮。
    if scope.pane.is_some() {
        flow_button(
            buffer,
            &mut x,
            row,
            tr("Bind account", "确认账号绑定"),
            Action::Bind,
            palette,
            hits,
        );
    }
    flow_button(
        buffer,
        &mut x,
        row,
        tr("Bind focused pane", "绑定到聚焦 pane"),
        Action::BindFocused,
        palette,
        hits,
    );
}

pub(super) fn usage_table(
    buffer: &mut Buffer,
    area: Rect,
    state: &State,
    scope: &AccountsScope<'_>,
    palette: &Palette,
    hits: &mut Vec<(Rect, Action)>,
) {
    let texts = &crate::i18n::texts().monitor;
    // 每行：账号 / 指标 / 用量 / 重置 / 新鲜度（着色）/ 状态。
    let mut entries = Vec::new();
    for account in scope.accounts {
        let status_cell = table_status(account, scope.refresh_of(&account.account_id));
        let freshness = (
            age_text(state.now_ms, account.observed_at_ms, account.status),
            age_color(
                state.now_ms,
                account.observed_at_ms,
                account.status,
                palette,
            ),
        );
        if account.metrics.is_empty() {
            // 无指标的账号：指标 / 用量 / 重置三列留空，状态落在真正的「状态」列，
            // 说明文字并入状态列尾（`状态 · 说明`），不再错位到指标列。
            let status_cell = match account.message.as_deref().filter(|m| !m.is_empty()) {
                Some(message) => format!("{status_cell} · {message}"),
                None => status_cell,
            };
            entries.push((
                account,
                [
                    account.account_label.clone(),
                    "—".into(),
                    "—".into(),
                    "—".into(),
                ],
                freshness,
                status_cell,
            ));
        } else {
            for metric in &account.metrics {
                let quantity = metric_quantity(metric);
                let usage = match (metric_percent(metric), quantity.as_str()) {
                    (Some(percent), "—") => format!("{percent:.1}%"),
                    (Some(percent), quantity) => format!("{percent:.1}% {quantity}"),
                    (None, quantity) => quantity.to_owned(),
                };
                entries.push((
                    account,
                    [
                        account.account_label.clone(),
                        format!("{} [{}]", metric.label, metric_scope(&metric.scope)),
                        usage,
                        reset_text(metric, state.now_ms).unwrap_or_else(|| "—".into()),
                    ],
                    freshness.clone(),
                    status_cell.clone(),
                ));
            }
        }
    }
    let start = scope.scroll.min(entries.len().saturating_sub(1));
    let rows = entries
        .iter()
        .skip(start)
        .take(area.height.saturating_sub(1) as usize)
        .enumerate()
        .map(
            |(index, (account, columns, (age, age_color), status_cell))| {
                hits.push((
                    Rect::new(area.x, area.y + index as u16 + 1, area.width, 1),
                    Action::Account(account.account_id.clone()),
                ));
                let fg = if scope.account == Some(account.account_id.as_str()) {
                    palette.accent
                } else {
                    palette.text
                };
                let cells = columns
                    .iter()
                    .map(|column| Cell::from(column.as_str()))
                    .chain([
                        Cell::from(age.as_str()).style(Style::default().fg(*age_color)),
                        Cell::from(status_cell.as_str())
                            .style(Style::default().fg(status_color(account.status, palette))),
                    ])
                    .collect::<Vec<_>>();
                Row::new(cells).style(Style::default().fg(fg).bg(if index % 2 == 0 {
                    palette.surface0
                } else {
                    palette.panel_bg
                }))
            },
        )
        .collect::<Vec<_>>();
    Table::new(
        rows,
        [
            Constraint::Percentage(16),
            Constraint::Percentage(22),
            Constraint::Percentage(18),
            Constraint::Percentage(14),
            Constraint::Percentage(14),
            Constraint::Percentage(16),
        ],
    )
    .header(
        Row::new([
            tr("Account", "账号"),
            tr("Metric / scope", "指标 / 范围"),
            texts.col_usage,
            texts.col_reset,
            texts.col_freshness,
            tr("Status", "状态"),
        ])
        .style(
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD),
        ),
    )
    .column_spacing(1)
    .render(area, buffer);
}

/// 总览浮层（紧凑版）每账号的行数：1 头行 + ≤3 指标行 + 需要绑定时 1 行提示。
/// 与页面 / 仪表盘的 `account_rows` 口径不同：浮层只做一眼总览。
pub(super) fn usage_hover_rows(accounts: &[AccountUsageSnapshot]) -> usize {
    accounts
        .iter()
        .map(|account| {
            1 + account.metrics.len().min(USAGE_HOVER_METRICS)
                + usize::from(account.status == ObservationStatus::NeedsBinding)
        })
        .sum()
}

/// 总览浮层每账号最多显示的指标行数。
const USAGE_HOVER_METRICS: usize = 3;
/// 指标行尾部进度条的列数（含 1 列间距）。
const USAGE_HOVER_BAR: usize = 13;

/// 总览浮层的头行：`账号 · 状态[ · 刷新中…]`。
fn usage_hover_header(account: &AccountUsageSnapshot, scope: &AccountsScope<'_>) -> String {
    let mut line = format!("{} · {}", account.account_label, status(account.status));
    if scope
        .refresh_of(&account.account_id)
        .is_some_and(|state| state.in_flight)
    {
        line.push_str(" · ");
        line.push_str(tr("refreshing…", "刷新中…"));
    }
    line
}

/// 总览浮层的指标行：`指标 用量`（重置时间用紧凑格式）。
fn usage_hover_metric(metric: &UsageMetric, now_ms: u64) -> String {
    format!("{} {}", metric.label, metric_value(metric, now_ms))
}

/// 需要绑定的账号在浮层里的一行提示。
fn usage_hover_binding_note() -> &'static str {
    tr(
        "→ needs a pane binding · open the accounts page",
        "→ 需要账号绑定 · 打开账号页绑定 pane",
    )
}

/// 总览浮层的纵向落点 `(y, height)`：优先按钮下方，放不下则上翻到按钮上方
/// （`bottom() == anchor.y`）；两侧都不够时取空间更大的一侧并把高度收缩到该
/// 空间，永远不与按钮行相交。
pub(super) fn usage_hover_placement(anchor: Rect, height: u16, area: Rect) -> (u16, u16) {
    let below = anchor.bottom().min(area.bottom());
    let space_below = area.bottom().saturating_sub(below);
    let space_above = anchor.y.saturating_sub(area.y);
    if height <= space_below {
        (below, height)
    } else if height <= space_above {
        (anchor.y - height, height)
    } else if space_below >= space_above {
        (below, space_below)
    } else {
        (anchor.y - space_above, space_above)
    }
}

/// 总览浮层的尺寸（含边框）：高 `(2 + Σ行 + 1 页脚).clamp(5, 14)`、宽按内容
/// `clamp(34, 56)`，再按整帧夹取；loading 态占 1 行占位。
pub(super) fn usage_hover_size(state: &State, scope: &AccountsScope<'_>, area: Rect) -> (u16, u16) {
    let rows = usage_hover_rows(scope.accounts).max(1);
    let height = (2 + rows + 1).clamp(5, 14);
    let content = scope
        .accounts
        .iter()
        .map(|account| {
            let header = UnicodeWidthStr::width(usage_hover_header(account, scope).as_str());
            let metrics = account
                .metrics
                .iter()
                .take(USAGE_HOVER_METRICS)
                .map(|metric| {
                    UnicodeWidthStr::width(usage_hover_metric(metric, state.now_ms).as_str())
                        + usize::from(metric.used_percent.is_some()) * USAGE_HOVER_BAR
                })
                .max()
                .unwrap_or(0);
            let note = if account.status == ObservationStatus::NeedsBinding {
                UnicodeWidthStr::width(usage_hover_binding_note())
            } else {
                0
            };
            header.max(metrics).max(note)
        })
        .max()
        .unwrap_or(0);
    // 2 列边框 + 2 列内边距。
    let width = (content + 4).clamp(34, 56);
    (
        (width as u16).min(area.width.saturating_sub(2)),
        (height as u16).min(area.height.saturating_sub(2)),
    )
}

/// 总览浮层正文（紧凑版）：每账号 1 头行 + ≤3 指标行（百分比指标带进度条），
/// 无按钮条，整块命中区打开账号页；页脚提示钉住 / 关闭方式。数据未到时显示
/// 「查询中…」占位而不是空框。
pub(super) fn usage_hover(
    buffer: &mut Buffer,
    area: Rect,
    state: &State,
    scope: &AccountsScope<'_>,
    pinned: bool,
    palette: &Palette,
    hits: &mut Vec<(Rect, Action)>,
) {
    if area.is_empty() {
        return;
    }
    // 1 列内边距。
    let inner = Rect::new(
        area.x.saturating_add(1),
        area.y,
        area.width.saturating_sub(2),
        area.height,
    );
    let content = Rect::new(
        inner.x,
        inner.y,
        inner.width,
        inner.height.saturating_sub(1),
    );
    if !state.usage.enabled {
        // 只有显式点击「用量」按钮（`pin_usage_overview`）会走到这里：扫过按钮
        // 在 `usage.enabled` 关闭时不弹浮层，点击则用这一行说明去哪里开启。
        text(
            buffer,
            content,
            0,
            tr(
                "Account usage is disabled · Monitor → Settings turns it on.",
                "账号用量已在设置中关闭 · 监控 → 设置 可开启。",
            ),
            Style::default().fg(palette.overlay0),
        );
    } else if scope.accounts.is_empty() {
        // 在途判定含逐厂商的 `hover_usage:<agent>` 键，不只看整体请求的键。
        let loading = scope.refreshing || state.usage_in_flight(true);
        text(
            buffer,
            content,
            0,
            if loading {
                tr("Loading…", "查询中…")
            } else {
                tr("No account usage yet.", "暂无账号用量数据。")
            },
            Style::default().fg(palette.overlay0),
        );
    } else {
        let rows = usage_hover_rows(scope.accounts);
        let start = scope
            .scroll
            .min(rows.saturating_sub(content.height.max(1) as usize));
        let viewport_row = |row: usize| {
            row.checked_sub(start)
                .filter(|row| *row < content.height as usize)
                .map(|row| Rect::new(content.x, content.y + row as u16, content.width, 1))
        };
        let mut row = 0;
        for account in scope.accounts {
            if row >= start + content.height as usize {
                break;
            }
            if let Some(rect) = viewport_row(row) {
                let header = usage_hover_header(account, scope);
                text(
                    buffer,
                    rect,
                    0,
                    &header,
                    Style::default()
                        .fg(status_color(account.status, palette))
                        .add_modifier(Modifier::BOLD),
                );
            }
            row += 1;
            for metric in account.metrics.iter().take(USAGE_HOVER_METRICS) {
                if let Some(rect) = viewport_row(row) {
                    let line = usage_hover_metric(metric, state.now_ms);
                    text(buffer, rect, 0, &line, Style::default().fg(palette.text));
                    if let Some(percent) = metric.used_percent {
                        let used = UnicodeWidthStr::width(line.as_str()) as u16;
                        let bar_rect = Rect::new(
                            rect.x.saturating_add(used).saturating_add(1),
                            rect.y,
                            rect.width.saturating_sub(used.saturating_add(1)),
                            1,
                        );
                        if bar_rect.width >= 6 {
                            bar(buffer, bar_rect, Some(percent as f32), palette);
                        }
                    }
                }
                row += 1;
            }
            if account.status == ObservationStatus::NeedsBinding {
                if let Some(rect) = viewport_row(row) {
                    text(
                        buffer,
                        rect,
                        0,
                        usage_hover_binding_note(),
                        Style::default().fg(palette.yellow),
                    );
                }
                row += 1;
            }
        }
        if scope.refreshing {
            buffer.set_style(content, Style::default().add_modifier(Modifier::DIM));
        }
    }
    if inner.height > 1 {
        let footer = Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1);
        text(
            buffer,
            footer,
            0,
            if pinned {
                tr(
                    "click opens accounts · esc closes",
                    "点击打开账号页 · Esc 关闭",
                )
            } else {
                tr(
                    "click opens accounts · usage button pins",
                    "点击打开账号页 · 点「用量」钉住",
                )
            },
            Style::default().fg(palette.overlay0),
        );
    }
    hits.push((area, Action::Page(Page::Accounts)));
}

/// 仪表盘模式下每个账号占用的行数：头行 + 说明行（message / 服务端说明 / 套餐·身份）
/// + 每指标 1 行 + 卡间分隔线。这是滚动的真源：改行数必须同步 `usage_dashboard`。
pub(super) fn account_rows(
    state: &State,
    accounts: &[AccountUsageSnapshot],
    refresh_states: &[UsageRefreshState],
) -> usize {
    accounts
        .iter()
        .map(|account| {
            if state.usage.format == UsageDisplayFormat::Table {
                return account.metrics.len().max(1);
            }
            // 与 `usage_dashboard` 里 `refresh_note` 出现的条件一致，滚动夹取不分配。
            let noted = refresh_states
                .iter()
                .find(|refresh| refresh.account_id == account.account_id)
                .is_some_and(|refresh| {
                    refresh.trust_required
                        || refresh.binding_inferred
                        || long_backoff_secs(refresh, state.now_ms).is_some()
                });
            2 + usize::from(account.message.is_some())
                + usize::from(noted)
                + usize::from(account.plan.is_some() || account.account_identity.is_some())
                + account.metrics.len()
        })
        .sum()
}

/// 指标一行：`label [scope] │ 定宽额度条 │ 42.0% │ used/limit │ 距重置 6d21h`；从左到右
/// 按剩余宽度依次放下，放不下的尾段省略（窄面板先保标签与百分比，额度条 8-24 列）。
fn metric_row(
    buffer: &mut Buffer,
    rect: Rect,
    metric: &UsageMetric,
    status: ObservationStatus,
    now_ms: u64,
    glyphs: crate::ui::BorderGlyphs,
    palette: &Palette,
) {
    if rect.is_empty() {
        return;
    }
    let name = format!("{} [{}]", metric.label, metric_scope(&metric.scope));
    let percent = metric_percent(metric);
    let elapsed = window_elapsed(metric, now_ms);
    let quantity = metric_quantity(metric);
    let reset = reset_text(metric, now_ms);
    let right = rect.right();
    let mut x = rect.x;
    let name_width = (UnicodeWidthStr::width(name.as_str()) as u16)
        .min(18)
        .min(right.saturating_sub(x));
    text(
        buffer,
        Rect::new(x, rect.y, name_width, 1),
        0,
        &name,
        Style::default().fg(palette.text),
    );
    x = x.saturating_add(name_width);
    const SEP: u16 = 3;
    const PCT: u16 = 6;
    if percent.is_some() {
        // 额度条只在放得下「分隔 + 条 + 分隔 + 百分比」时出现。
        let available = right.saturating_sub(x);
        if available >= SEP * 2 + 8 + PCT {
            let bar_width = ((available - SEP * 2 - PCT) / 2).clamp(8, 24);
            x += column_separator(buffer, x, rect.y, glyphs, palette);
            quota_bar(
                buffer,
                Rect::new(x, rect.y, bar_width, 1),
                percent,
                elapsed,
                status,
                palette,
            );
            x = x.saturating_add(bar_width);
        }
    }
    if let Some(percent) = percent {
        if right.saturating_sub(x) >= SEP + PCT {
            x += column_separator(buffer, x, rect.y, glyphs, palette);
            let live = matches!(
                status,
                ObservationStatus::Ready | ObservationStatus::Warming
            );
            text(
                buffer,
                Rect::new(x, rect.y, PCT, 1),
                0,
                &format!("{percent:>5.1}%"),
                Style::default().fg(if live {
                    quota_color(percent, elapsed, palette)
                } else {
                    palette.overlay0
                }),
            );
            x = x.saturating_add(PCT);
        }
    }
    for (value, color) in [
        ((quantity != "—").then_some(quantity.as_str()), palette.teal),
        (reset.as_deref(), palette.overlay0),
    ] {
        let Some(value) = value else {
            continue;
        };
        let width = UnicodeWidthStr::width(value) as u16;
        if right.saturating_sub(x) < SEP + width {
            break;
        }
        x += column_separator(buffer, x, rect.y, glyphs, palette);
        text(
            buffer,
            Rect::new(x, rect.y, width, 1),
            0,
            value,
            Style::default().fg(color),
        );
        x = x.saturating_add(width);
    }
}

/// 仪表盘：每账号一张卡——左缘状态槽 `▌`、头行「› 账号 · 状态」右对齐新鲜度、说明行、
/// 每指标一行（定宽额度条）、卡间 `surface_dim` 分隔线。返回走过的总行数（视口内
/// 提前结束时为部分值），与 `account_rows` 同口径。
pub(super) fn usage_dashboard(
    buffer: &mut Buffer,
    area: Rect,
    state: &State,
    scope: &AccountsScope<'_>,
    palette: &Palette,
    hits: &mut Vec<(Rect, Action)>,
) -> usize {
    let glyphs = state.glyphs;
    let start = scope.scroll.min(
        account_rows(state, scope.accounts, scope.refresh_states)
            .saturating_sub(area.height.max(1) as usize),
    );
    let viewport_row = |row: usize| {
        row.checked_sub(start)
            .filter(|row| *row < area.height as usize)
            .map(|row| Rect::new(area.x, area.y + row as u16, area.width, 1))
    };
    // 卡片内容缩进状态槽之后。
    let indented = |rect: Rect| {
        Rect::new(
            rect.x.saturating_add(2),
            rect.y,
            rect.width.saturating_sub(2),
            1,
        )
    };
    let mut row = 0;
    for account in scope.accounts {
        if row >= start + area.height as usize {
            break;
        }
        let color = status_color(account.status, palette);
        if let Some(header) = viewport_row(row) {
            status_slot(buffer, header.x, header.y, color);
            let selected = scope.account == Some(account.account_id.as_str());
            let label = format!(
                "{}{}",
                if selected { "› " } else { "  " },
                account.account_label
            );
            let body = Rect::new(
                header.x.saturating_add(1),
                header.y,
                header.width.saturating_sub(1),
                1,
            );
            text(
                buffer,
                body,
                0,
                &label,
                Style::default()
                    .fg(if selected {
                        palette.accent
                    } else {
                        palette.text
                    })
                    .add_modifier(Modifier::BOLD),
            );
            let mut offset = UnicodeWidthStr::width(label.as_str()) as u16;
            let status_text = format!(" · {}", status(account.status));
            text(
                buffer,
                Rect::new(
                    body.x.saturating_add(offset),
                    body.y,
                    body.width.saturating_sub(offset),
                    1,
                ),
                0,
                &status_text,
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            );
            offset = offset.saturating_add(UnicodeWidthStr::width(status_text.as_str()) as u16);
            // 服务端报告探测在途：标题行尾追加「刷新中…」。
            if scope
                .refresh_of(&account.account_id)
                .is_some_and(|state| state.in_flight)
            {
                let note = format!(" · {}", tr("refreshing…", "刷新中…"));
                text(
                    buffer,
                    Rect::new(
                        body.x.saturating_add(offset),
                        body.y,
                        body.width.saturating_sub(offset),
                        1,
                    ),
                    0,
                    &note,
                    Style::default().fg(palette.overlay1),
                );
                offset = offset.saturating_add(UnicodeWidthStr::width(note.as_str()) as u16);
            }
            // 右对齐新鲜度（放得下才画）。
            let age = age_text(state.now_ms, account.observed_at_ms, account.status);
            let age_width = UnicodeWidthStr::width(age.as_str()) as u16;
            if body.width > offset.saturating_add(1).saturating_add(age_width) {
                text(
                    buffer,
                    Rect::new(body.right().saturating_sub(age_width), body.y, age_width, 1),
                    0,
                    &age,
                    Style::default().fg(age_color(
                        state.now_ms,
                        account.observed_at_ms,
                        account.status,
                        palette,
                    )),
                );
            }
            hits.push((header, Action::Account(account.account_id.clone())));
        }
        row += 1;
        let note = refresh_note(scope.refresh_of(&account.account_id), state.now_ms);
        let identity = match (account.plan.as_deref(), account.account_identity.as_deref()) {
            (Some(plan), Some(identity)) => Some(format!("{plan} · {identity}")),
            (Some(plan), None) => Some(plan.to_owned()),
            (None, Some(identity)) => Some(identity.to_owned()),
            (None, None) => None,
        };
        for (value, note_color) in [
            (account.message.as_deref(), palette.yellow),
            (note.as_deref(), palette.yellow),
            (identity.as_deref(), palette.overlay1),
        ] {
            if let Some(value) = value {
                if let Some(rect) = viewport_row(row) {
                    status_slot(buffer, rect.x, rect.y, color);
                    text(
                        buffer,
                        indented(rect),
                        0,
                        value,
                        Style::default().fg(note_color),
                    );
                }
                row += 1;
            }
        }
        for metric in &account.metrics {
            if let Some(rect) = viewport_row(row) {
                status_slot(buffer, rect.x, rect.y, color);
                metric_row(
                    buffer,
                    indented(rect),
                    metric,
                    account.status,
                    state.now_ms,
                    glyphs,
                    palette,
                );
            }
            row += 1;
        }
        if let Some(rect) = viewport_row(row) {
            rule(buffer, rect, glyphs, palette);
        }
        row += 1;
    }
    row
}

/// agent 行悬浮层的账号正文：正文 + 底部流式动作行（刷新 · 切换账号 · 官方查询说明 ·
/// 官方回调 · 绑定账号，最多两行）。页面的动作在工具栏里，不走这里。
fn accounts_body(
    buffer: &mut Buffer,
    area: Rect,
    state: &State,
    scope: &AccountsScope<'_>,
    palette: &Palette,
    hits: &mut Vec<(Rect, Action)>,
) {
    if area.is_empty() {
        return;
    }
    let mut items = vec![
        refresh_item(scope, state.now_ms),
        cycle_item(state, scope.provider),
        source_item(),
    ];
    items.extend(callback_item(state, scope));
    // 悬浮层的绑定目标就是 hover 的 pane；页面的绑定入口在绑定行里，不在这里重复。
    if scope.chrome == BodyChrome::Hover && scope.pane.is_some() {
        items.push(ToolbarItem {
            label: tr("Bind account", "绑定账号").to_owned(),
            action: Some(Action::Bind),
        });
    }
    let widths = items.iter().map(ToolbarItem::width).collect::<Vec<_>>();
    let (_, rows) = flow_positions(&widths, area.width, 2, 2);
    // 正文至少留 1 行。
    let rows = rows.min(area.height.saturating_sub(1));
    let content = Rect::new(area.x, area.y, area.width, area.height.saturating_sub(rows));
    accounts_content(buffer, content, state, scope, palette, hits);
    if rows > 0 {
        toolbar_rows(
            buffer,
            Rect::new(area.x, content.bottom(), area.width, rows),
            &items,
            palette,
            hits,
        );
    }
}

/// 系统页卡片标题（含两侧空格）；设置页的显示项复用同一份文案（ACC-12）。
fn section_title(id: &str) -> &'static str {
    match id {
        "cpu" => " CPU ",
        "cores" => tr(" CPU CORES ", " CPU 逐核 "),
        "memory" => tr(" MEMORY ", " 内存 "),
        "gpu" => " GPU ",
        "disks" => tr(" DISKS ", " 磁盘 "),
        "network" => tr(" NETWORK ", " 网络 "),
        "sensors" => tr(" TEMPERATURE ", " 温度 "),
        _ => tr(" PROCESSES ", " 进程 "),
    }
}

/// 设置页的一行：只描述「是什么」，文案在真正落入视口时才格式化（ACC-18）。
enum SettingsRow<'a> {
    Section(&'static str),
    Interval,
    CardHeight,
    History,
    Alerts,
    UsageEnabled,
    UsageFormat,
    UsagePosition,
    /// 用量位置为「页面」时的常驻提示行（无动作）。
    PositionHint,
    HoverDelay,
    /// 「恢复配置文件值」：有本机覆盖（usage_* 偏好键任一为 Some）时可点，否则只是
    /// 说明行。
    RestoreUsage {
        overridden: bool,
    },
    ApiRefresh,
    CliRefresh,
    InteractiveProbe,
    Card(&'static str),
    AlertThreshold(usize),
    AlertDuration(usize),
    AlertCooldown(usize),
    Provider(&'a UsageProviderInfo),
    Device {
        id: &'a str,
        label: &'a str,
    },
    Sensor(&'a str),
}

impl SettingsRow<'_> {
    fn action(&self) -> Option<Action> {
        Some(match self {
            Self::Section(_)
            | Self::PositionHint
            | Self::ApiRefresh
            | Self::CliRefresh
            | Self::InteractiveProbe
            | Self::RestoreUsage { overridden: false } => return None,
            Self::RestoreUsage { overridden: true } => Action::RestoreUsagePreferences,
            Self::Interval => Action::Interval,
            Self::CardHeight => Action::CardSize,
            Self::History => Action::HistoryRange,
            Self::Alerts => Action::ToggleAlerts,
            Self::UsageEnabled => Action::UsageEnabled,
            Self::UsageFormat => Action::UsageFormat,
            Self::UsagePosition => Action::UsagePosition,
            Self::HoverDelay => Action::HoverDelay,
            Self::Card(id) => Action::Metric((*id).into()),
            Self::AlertThreshold(index) => Action::AlertThreshold(*index),
            Self::AlertDuration(index) => Action::AlertDuration(*index),
            Self::AlertCooldown(index) => Action::AlertCooldown(*index),
            Self::Provider(provider) => Action::ProviderEnabled(provider.agent.clone()),
            Self::Device { id, .. } => Action::Device((*id).into()),
            Self::Sensor(name) => Action::Device(format!("sensor:{name}")),
        })
    }

    fn label(&self, state: &State) -> String {
        let texts = &crate::i18n::texts().monitor;
        let check = |on: bool| if on { "[x]" } else { "[ ]" };
        match self {
            Self::Section(title) => (*title).to_owned(),
            Self::Interval => format!(
                "{}: {} ms",
                tr("Sampling interval", "采样间隔"),
                state.monitor.interval_ms
            ),
            Self::CardHeight => format!(
                "{}: {}",
                tr("Card height", "卡片高度"),
                state.monitor.card_height
            ),
            Self::History => format!(
                "{}: {} min",
                tr("History", "历史范围"),
                state.monitor.history_minutes
            ),
            Self::Alerts => format!(
                "{} {}",
                check(state.monitor.alerts_enabled),
                tr("Resource alerts while connected", "连接期间资源告警")
            ),
            Self::UsageEnabled => format!(
                "{} {}",
                check(state.usage.enabled),
                tr("Provider account usage", "厂商账号用量")
            ),
            Self::UsageFormat => format!(
                "{}: {}",
                tr("Usage format", "用量样式"),
                usage_format_label(state.usage.format)
            ),
            Self::UsagePosition => format!(
                "{}: {}",
                tr("Usage placement", "用量位置"),
                usage_position_label(state.usage.position)
            ),
            Self::PositionHint => format!("    ↳ {}", texts.hover_closed_hint),
            Self::HoverDelay => format!("{}: {} ms", texts.hover_delay, state.usage.hover_delay_ms),
            Self::RestoreUsage { overridden } => format!(
                "{} ({})",
                tr("Restore config file values", "恢复配置文件值"),
                if *overridden {
                    tr("clear local overrides", "清除本机覆盖")
                } else {
                    tr("no local overrides", "无本机覆盖")
                }
            ),
            Self::ApiRefresh => format!(
                "{}: {} s ({})",
                texts.api_refresh, state.usage.api_refresh_seconds, texts.config_value_hint
            ),
            Self::CliRefresh => format!(
                "{}: {} s ({})",
                texts.cli_refresh, state.usage.cli_refresh_seconds, texts.config_value_hint
            ),
            Self::InteractiveProbe => format!(
                "{}: {} ({})",
                texts.interactive_probe,
                if state.usage.interactive_probe {
                    texts.on
                } else {
                    texts.off
                },
                texts.config_value_hint
            ),
            Self::Card(id) => format!(
                "{} {}",
                check(state.monitor.visible.iter().any(|item| item == id)),
                section_title(id).trim()
            ),
            Self::AlertThreshold(index) => {
                state
                    .monitor
                    .alerts
                    .get(*index)
                    .map_or_else(String::new, |rule| {
                        format!(
                            "{} {} ≥ {:.0}%",
                            tr("Alert", "告警"),
                            rule.metric,
                            rule.threshold
                        )
                    })
            }
            Self::AlertDuration(index) => state
                .monitor
                .alerts
                .get(*index)
                .map_or_else(String::new, |rule| {
                    format!("  {} {}s", tr("Duration", "持续"), rule.duration_seconds)
                }),
            Self::AlertCooldown(index) => state
                .monitor
                .alerts
                .get(*index)
                .map_or_else(String::new, |rule| {
                    format!("  {} {}s", tr("Cooldown", "冷却"), rule.cooldown_seconds)
                }),
            Self::Provider(provider) => format!(
                "{} {}",
                check(!state.usage.disabled_providers.contains(&provider.agent)),
                provider.label
            ),
            Self::Device { id, label } => format!(
                "{} {label}",
                check(
                    !state
                        .monitor
                        .hidden_devices
                        .iter()
                        .any(|hidden| hidden == id)
                )
            ),
            Self::Sensor(name) => {
                let id = format!("sensor:{name}");
                format!(
                    "{} {name}",
                    check(!state.monitor.hidden_devices.contains(&id))
                )
            }
        }
    }
}

/// 用量样式的显示名（走文案表，不再 `{:?}`）。
pub(super) fn usage_format_label(format: UsageDisplayFormat) -> &'static str {
    let texts = &crate::i18n::texts().monitor;
    match format {
        UsageDisplayFormat::Dashboard => texts.format_dashboard,
        UsageDisplayFormat::Table => texts.format_table,
    }
}

/// 用量位置的显示名（走文案表，不再 `{:?}`）。
pub(super) fn usage_position_label(position: UsageDisplayPosition) -> &'static str {
    let texts = &crate::i18n::texts().monitor;
    match position {
        UsageDisplayPosition::Hover => texts.position_hover,
        UsageDisplayPosition::Page => texts.position_page,
        UsageDisplayPosition::Both => texts.position_both,
    }
}

/// 设置页：分节（监控 / 账号用量 / 显示项 / 告警 / 厂商 / 设备）的行列表，节标题画成
/// 带标题的分隔线；只有落入视口的行才格式化文案，滚动位置用 `settings_scroll`。
fn settings(
    buffer: &mut Buffer,
    area: Rect,
    state: &State,
    palette: &Palette,
    hits: &mut Vec<(Rect, Action)>,
) {
    let texts = &crate::i18n::texts().monitor;
    let mut rows = vec![
        SettingsRow::Section(texts.section_monitor),
        SettingsRow::Interval,
        SettingsRow::CardHeight,
        SettingsRow::History,
        SettingsRow::Alerts,
        SettingsRow::Section(texts.section_usage),
        SettingsRow::UsageEnabled,
        SettingsRow::UsageFormat,
        SettingsRow::UsagePosition,
    ];
    if state.usage.position == UsageDisplayPosition::Page {
        rows.push(SettingsRow::PositionHint);
    }
    rows.extend([
        SettingsRow::HoverDelay,
        SettingsRow::RestoreUsage {
            overridden: state.usage_overridden,
        },
        SettingsRow::ApiRefresh,
        SettingsRow::CliRefresh,
        SettingsRow::InteractiveProbe,
        SettingsRow::Section(texts.section_cards),
    ]);
    rows.extend(
        [
            "cpu",
            "cores",
            "memory",
            "gpu",
            "disks",
            "network",
            "sensors",
            "processes",
        ]
        .into_iter()
        .map(SettingsRow::Card),
    );
    if !state.monitor.alerts.is_empty() {
        rows.push(SettingsRow::Section(texts.section_alerts));
        for index in 0..state.monitor.alerts.len() {
            rows.push(SettingsRow::AlertThreshold(index));
            rows.push(SettingsRow::AlertDuration(index));
            rows.push(SettingsRow::AlertCooldown(index));
        }
    }
    let providers = state
        .providers
        .iter()
        .filter(|provider| provider_listed(provider))
        .collect::<Vec<_>>();
    if !providers.is_empty() {
        rows.push(SettingsRow::Section(texts.section_providers));
        rows.extend(providers.into_iter().map(SettingsRow::Provider));
    }
    if let Some(sample) = &state.metrics {
        let devices = sample
            .gpus
            .iter()
            .map(|item| (item.id.as_str(), item.name.as_str()))
            .chain(
                sample
                    .disks
                    .iter()
                    .map(|item| (item.id.as_str(), item.mount_point.as_str())),
            )
            .chain(
                sample
                    .networks
                    .iter()
                    .map(|item| (item.id.as_str(), item.id.as_str())),
            )
            .map(|(id, label)| SettingsRow::Device { id, label })
            .chain(
                sample
                    .sensors
                    .iter()
                    .map(|sensor| SettingsRow::Sensor(sensor.name.as_str())),
            )
            .collect::<Vec<_>>();
        if !devices.is_empty() {
            rows.push(SettingsRow::Section(texts.section_devices));
            rows.extend(devices);
        }
    }
    let max_scroll = rows.len().saturating_sub(1);
    for (index, row) in rows
        .iter()
        .skip(state.settings_scroll.min(max_scroll))
        .take(area.height as usize)
        .enumerate()
    {
        let rect = Rect::new(area.x, area.y + index as u16, area.width, 1);
        let label = row.label(state);
        if let SettingsRow::Section(_) = row {
            rule(buffer, rect, state.glyphs, palette);
            text(
                buffer,
                Rect::new(
                    rect.x.saturating_add(1),
                    rect.y,
                    rect.width.saturating_sub(1),
                    1,
                ),
                0,
                &format!(" {label} "),
                Style::default()
                    .fg(palette.accent)
                    .add_modifier(Modifier::BOLD),
            );
            continue;
        }
        let action = row.action();
        text(
            buffer,
            rect,
            0,
            &label,
            Style::default().fg(if action.is_some() {
                palette.text
            } else {
                palette.overlay0
            }),
        );
        if let Some(action) = action {
            hits.push((rect, action));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::shell::ClientShellConfig;
    use crate::config::Config;
    use crate::i18n::{lang_guard, Lang};

    fn config() -> ClientShellConfig {
        ClientShellConfig::from_config(&Config::default())
    }

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

    /// 测试用的组件上下文：只需要调色板、组件 token 与字形。
    fn chrome_context(config: &ClientShellConfig) -> ChromeContext<'_> {
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

    fn paint_page(state: &State, page: Page, width: u16, height: u16) -> (Buffer, PaintOutput) {
        let area = Rect::new(0, 0, width, height);
        let mut buffer = Buffer::empty(area);
        let config = config();
        let cx = chrome_context(&config);
        let output = paint(&mut buffer, area, state, &cx, Some(page), false);
        (buffer, output)
    }

    fn row_text(buffer: &Buffer, y: u16) -> String {
        (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol().to_owned())
            .collect::<String>()
    }

    fn buffer_text(buffer: &Buffer) -> String {
        (0..buffer.area.height)
            .map(|y| row_text(buffer, y))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 某行是否含 `needle`：宽字符占两个单元格、续格是空格，两边都去掉空格再比。
    fn row_has(buffer: &Buffer, y: u16, needle: &str) -> bool {
        row_text(buffer, y)
            .replace(' ', "")
            .contains(&needle.replace(' ', ""))
    }

    fn buffer_has(buffer: &Buffer, needle: &str) -> bool {
        (0..buffer.area.height).any(|y| row_has(buffer, y, needle))
    }

    /// 某行里 `needle` 首字符所在单元格的前景色（宽字符占两个单元格，按符号逐格找）。
    fn color_at(buffer: &Buffer, y: u16, needle: &str) -> Option<Color> {
        let first = needle.chars().next()?.to_string();
        let x = (0..buffer.area.width).find(|x| {
            buffer[(*x, y)].symbol() == first
                && row_text(buffer, y)
                    .replace(' ', "")
                    .contains(&needle.replace(' ', ""))
        })?;
        buffer[(x, y)].style().fg
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
            let mut buffer = Buffer::empty(area);
            paint(
                &mut buffer,
                area,
                &populated(),
                &cx,
                Some(Page::Monitor),
                false,
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

    /// ds-08：terminal 主题的 `panel_bg` 是 `Reset`，页签「反色」于是退化成
    /// 「终端默认前景压在 accent 上」。反色前景改取组件表（与按钮同源），
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
        assert_eq!(
            cell.style().fg,
            Some(config.palette.surface_dim),
            "面板底为 Reset 时反色前景取 surface_dim（组件表口径）"
        );
        assert_ne!(cell.style().fg, Some(Color::Reset));
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

    fn contains_rect(outer: Rect, inner: Rect) -> bool {
        inner.x >= outer.x
            && inner.y >= outer.y
            && inner.right() <= outer.right()
            && inner.bottom() <= outer.bottom()
    }

    fn has(output: &PaintOutput, wanted: impl Fn(&Action) -> bool) -> bool {
        output.hits.iter().any(|(_, action)| wanted(action))
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
        assert!(has(&output, |action| matches!(action, Action::HoverDelay)));
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
        assert!(has(&output, |a| matches!(a, Action::UsageFormat)));
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
}
