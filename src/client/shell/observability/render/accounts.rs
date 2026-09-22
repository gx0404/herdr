//! 账号页：厂商 chip / 选择器、工具栏、仪表盘与表格、右栏详情、绑定行，
//! 以及账号正文的作用域视图（页面与悬浮层共用）。

use super::*;
use ratatui::layout::{Constraint, Layout};
use ratatui::widgets::{Cell, Row, Sparkline, Table, Widget};

/// 两种显示模式统一的已用百分比：官方 `used_percent` 优先，否则由 `used/limit`
/// 推算；统一夹取到 0..=100（ACC-20）。
pub(in crate::client::shell::observability) fn metric_percent(metric: &UsageMetric) -> Option<f32> {
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
pub(super) fn flow_positions(
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
pub(super) struct ToolbarItem {
    pub(super) label: String,
    pub(super) action: Option<Action>,
}

impl ToolbarItem {
    pub(super) fn width(&self) -> u16 {
        (UnicodeWidthStr::width(self.label.as_str()) as u16).saturating_add(2)
    }
}

/// 账号页此刻能否接受显式刷新：与工具栏「刷新」项同一口径（强意图刷新在途或
/// 被防抖 / 厂商退避时不行），页脚的刷新提示据此置灰。
pub(super) fn refresh_available(state: &State) -> bool {
    let scope = page_scope(state);
    !scope.refreshing && scope.refresh_wait_secs(state.now_ms).is_none()
}

/// 「刷新」项：本地强意图刷新在途时原位变成「刷新中…」，显式刷新被防抖 / 厂商退避时
/// 变成「N 秒后可刷新」；两者都是状态提示，不回填命中区。服务端探测在途（订阅期间
/// 可能长期为真）只在标题行 / 状态列表达，不锁按钮。
pub(super) fn refresh_item(scope: &AccountsScope<'_>, now_ms: u64) -> ToolbarItem {
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
pub(super) fn callback_item(state: &State, scope: &AccountsScope<'_>) -> Option<ToolbarItem> {
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
pub(super) fn cycle_item(state: &State, provider: Option<&str>) -> ToolbarItem {
    ToolbarItem {
        label: tr("Switch account", "切换账号").to_owned(),
        action: state
            .cycle_candidates(provider)
            .nth(1)
            .map(|_| Action::CycleAccount),
    }
}

pub(super) fn source_item() -> ToolbarItem {
    ToolbarItem {
        label: tr("Official source", "官方查询说明").to_owned(),
        action: Some(Action::Source),
    }
}

/// 视图切换项：标签写的是目标视图。
fn format_item(state: &State) -> ToolbarItem {
    let texts = &crate::i18n::texts().monitor;
    let (target, label) = match state.usage.format {
        UsageDisplayFormat::Dashboard => (UsageDisplayFormat::Table, texts.format_table),
        UsageDisplayFormat::Table => (UsageDisplayFormat::Dashboard, texts.format_dashboard),
    };
    ToolbarItem {
        label: format!("⇄ {label}"),
        action: Some(Action::UsageFormat(target)),
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
pub(super) fn toolbar_rows(
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
pub(super) fn accounts(
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
pub(super) fn accounts_content(
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

/// 账号页使用的页面作用域。
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

/// 禁用态按钮：走组件的 `Disabled` 档（灰字 + 弱底色），不回填命中区；
/// 返回实际占用宽度。
pub(super) fn disabled_button(
    buffer: &mut Buffer,
    rect: Rect,
    label: &str,
    palette: &Palette,
) -> u16 {
    let label = format!(" {label} ");
    let width = crate::ui::modal_button_width(&label).min(rect.width);
    let rect = Rect::new(rect.x, rect.y, width, rect.height.min(1));
    crate::client::shell::render::modal_button(
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

/// 仪表盘模式下每个账号占用的行数：头行 + 说明行（message / 服务端说明 / 套餐·身份）
/// + 每指标 1 行 + 卡间分隔线。这是滚动的真源：改行数必须同步 `usage_dashboard`。
pub(in crate::client::shell::observability) fn account_rows(
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
