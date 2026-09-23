//! 账号卡片的绘制：kit `card` 外框 + `meter_row` / `gauge` 额度行 + 数值项行，
//! 按行滚动并裁剪到视口；卡片比视口高时只画前 N 行、在下边框写「+N」。概览
//! （全部厂商）每厂商一张紧凑卡，≥96 列双栏。只读状态，命中区由调用方收集。

use std::borrow::Cow;

use super::cards::{
    build_card, family, headline, vendor_groups, Card, Family, Line, Meter, Stat, Tone, VendorGroup,
};
use super::*;
use crate::ui::kit::card::{render_card, CardSpec};
use crate::ui::kit::empty_state::{render_empty_state, EmptyState};
use crate::ui::kit::gauge::GaugeSpec;
use crate::ui::kit::meter_row::{render_meter_row, MeterRow};

/// 概览紧凑卡的高度：上下边框 + 首行（最紧张的额度）+ 汇总行。
pub(super) const OVERVIEW_CARD_HEIGHT: usize = 4;

/// 账号卡片的卡头行数（上边框标题行 + 状态行），即「选中该账号」的命中区高度。
const CARD_HEADER_ROWS: u16 = 2;

/// 概览的列数：≥96 列双栏。
pub(super) fn overview_columns(width: u16) -> usize {
    if width >= 96 {
        2
    } else {
        1
    }
}

/// 绘制期共用的只读输入。
#[derive(Clone, Copy)]
struct Paint<'p> {
    palette: &'p Palette,
    glyphs: crate::ui::BorderGlyphs,
    ascii: bool,
    now_ms: u64,
}

/// 在视口里画一个高 `height` 的块：`top` 是块顶相对视口顶的行偏移（可为负）。
/// 完全在视口内时直接画；部分可见时先画进同尺寸的暂存缓冲，再把可见行拷进来
/// ——kit 卡片需要完整的矩形，不能直接画到视口外。
fn draw_clipped(
    buffer: &mut Buffer,
    viewport: Rect,
    x: u16,
    top: i32,
    width: u16,
    height: u16,
    paint: impl FnOnce(&mut Buffer, Rect),
) {
    if width == 0 || height == 0 {
        return;
    }
    if top >= 0 && top + i32::from(height) <= i32::from(viewport.height) {
        paint(buffer, Rect::new(x, viewport.y + top as u16, width, height));
        return;
    }
    let mut scratch = Buffer::empty(Rect::new(0, 0, width, height));
    let area = scratch.area;
    paint(&mut scratch, area);
    for row in 0..height {
        let y = top + i32::from(row);
        if y < 0 || y >= i32::from(viewport.height) {
            continue;
        }
        let y = viewport.y + y as u16;
        for column in 0..width {
            if let (Some(source), Some(target)) = (
                scratch.cell((column, row)).cloned(),
                buffer.cell_mut((x + column, y)),
            ) {
                *target = source;
            }
        }
    }
}

/// 块在视口里可见的那部分矩形（命中区）。
fn visible_rect(viewport: Rect, x: u16, top: i32, width: u16, height: u16) -> Rect {
    let start = top.max(0);
    let end = (top + i32::from(height)).min(i32::from(viewport.height));
    if end <= start {
        return Rect::default();
    }
    Rect::new(x, viewport.y + start as u16, width, (end - start) as u16)
}

/// 从 `x` 起写一段文本，返回写下的列数（放不下带省略号截断）。
fn put(buffer: &mut Buffer, x: u16, y: u16, max_width: u16, value: &str, style: Style) -> u16 {
    if max_width == 0 {
        return 0;
    }
    let value = crate::ui::truncate_end(value, usize::from(max_width));
    let (end, _) = buffer.set_stringn(x, y, &value, usize::from(max_width), style);
    end.saturating_sub(x)
}

/// 一行里依次写几段，段间 ` · `；写不下的尾段截断。
fn spans(buffer: &mut Buffer, rect: Rect, parts: &[(Cow<'_, str>, Style)], palette: &Palette) {
    let mut x = rect.x;
    for (index, (value, style)) in parts.iter().enumerate() {
        if index > 0 {
            x += put(
                buffer,
                x,
                rect.y,
                rect.right().saturating_sub(x),
                " · ",
                Style::default().fg(palette.overlay0),
            );
        }
        x += put(
            buffer,
            x,
            rect.y,
            rect.right().saturating_sub(x),
            value,
            *style,
        );
        if x >= rect.right() {
            break;
        }
    }
}

/// 服务端文本（模型名）截断时至少留给值的列数：再少省略号前只剩一两个字，不如
/// 先整段让出标签。
const TEXT_MIN_WIDTH: u16 = 8;

/// 值最少要占的列：数字（`Number` / `Muted`）是整段；服务端文本可以带省略号截到
/// `TEXT_MIN_WIDTH`。
fn stat_value_min(stat: &Stat<'_>) -> u16 {
    let value = crate::ui::display_width_u16(&stat.value);
    match stat.tone {
        Tone::Text => value.min(TEXT_MIN_WIDTH),
        Tone::Number | Tone::Muted => value,
    }
}

/// 「标签 值」整项最少要占的列：标签整段 + 一格空白 + 值的最小宽度。
fn stat_min_width(stat: &Stat<'_>) -> u16 {
    crate::ui::display_width_u16(&stat.label)
        .saturating_add(1)
        .saturating_add(stat_value_min(stat))
}

/// 「标签 值」整项的自然宽度。
fn stat_full_width(stat: &Stat<'_>) -> u16 {
    crate::ui::display_width_u16(&stat.label)
        .saturating_add(1)
        .saturating_add(crate::ui::display_width_u16(&stat.value))
}

/// 一个数值项落在 `rect` 里，标签靠左、值靠右，按 token 整段取舍（真机 L3）：放得下
/// 「标签 值」才画标签，否则整段丢掉标签只画值；数字连自己都放不下就整项不画，
/// 不留半截。服务端文本（`Text`）在标签之后还有 `TEXT_MIN_WIDTH` 列时带省略号截断。
fn stat_cell(buffer: &mut Buffer, rect: Rect, stat: &Stat<'_>, palette: &Palette) {
    if rect.is_empty() {
        return;
    }
    let label_style = Style::default().fg(palette.subtext0);
    let value_style = match stat.tone {
        Tone::Number => Style::default()
            .fg(palette.text)
            .add_modifier(Modifier::BOLD),
        Tone::Muted => Style::default().fg(palette.overlay0),
        Tone::Text => Style::default().fg(palette.text),
    };
    let label_width = crate::ui::display_width_u16(&stat.label);
    let value_width = crate::ui::display_width_u16(&stat.value);
    let (label_w, value_w) = if stat_min_width(stat) <= rect.width {
        (label_width, value_width.min(rect.width - label_width - 1))
    } else if stat_value_min(stat) <= rect.width {
        (0, value_width.min(rect.width))
    } else {
        return;
    };
    if label_w > 0 {
        put(buffer, rect.x, rect.y, label_w, &stat.label, label_style);
    }
    put(
        buffer,
        rect.right() - value_w,
        rect.y,
        value_w,
        &stat.value,
        value_style,
    );
}

/// 数值项网格的列数：卡片里所有数值行共用同一套列，上下对齐。
const STAT_COLUMNS: u16 = 3;
/// 数值列最宽多少：宽卡片上标签与数字不至于隔得太远，网格靠左排。
const STAT_COLUMN_MAX: u16 = 28;
/// 紧凑排布里项与项之间的空白列数。
const STAT_FLOW_GAP: u16 = 2;

/// 一行 1–3 个数值项。每项都放得下「标签 值」时落在固定的三列网格上（列间 ` │ `，
/// 窄时退成一格空白），行末一项可以向右延伸（例如模型名）；网格放不下任何一项时
/// 改为紧凑排布：按顺序整项摆放、项间两格，放不下的整项不画；一项都放不下时只画
/// 第一项的值。标签与数字从不被截成半截（真机 L3：窄面板曾截成「时… 4m…」）。
fn stats_row(buffer: &mut Buffer, rect: Rect, stats: &[Stat<'_>], paint: Paint<'_>) {
    let stats = &stats[..stats.len().min(usize::from(STAT_COLUMNS))];
    if rect.is_empty() || stats.is_empty() {
        return;
    }
    let count = stats.len() as u16;
    let gap = if rect.width >= STAT_COLUMNS * 14 {
        3
    } else {
        1
    };
    let column =
        (rect.width.saturating_sub(gap * (STAT_COLUMNS - 1)) / STAT_COLUMNS).min(STAT_COLUMN_MAX);
    let grid_cell = |index: u16, stat: &Stat<'_>| {
        let x = rect.x + index * (column + gap);
        if column == 0 || x >= rect.right() {
            return None;
        }
        let width = if index + 1 < count {
            column
        } else {
            column.max(stat_full_width(stat)).min(rect.right() - x)
        };
        Some(Rect::new(x, rect.y, width, 1))
    };
    let grid = stats.iter().enumerate().all(|(index, stat)| {
        grid_cell(index as u16, stat).is_some_and(|cell| stat_min_width(stat) <= cell.width)
    });
    if grid {
        for (index, stat) in stats.iter().enumerate() {
            let Some(cell) = grid_cell(index as u16, stat) else {
                continue;
            };
            if index > 0 && gap == 3 {
                if let Some(separator) = buffer.cell_mut((cell.x - 2, rect.y)) {
                    separator
                        .set_symbol(paint.glyphs.vertical)
                        .set_style(Style::default().fg(paint.palette.surface_dim));
                }
            }
            stat_cell(buffer, cell, stat, paint.palette);
        }
        return;
    }
    let mut x = rect.x;
    let mut placed = false;
    for stat in stats {
        let remaining = rect.right().saturating_sub(x);
        if stat_min_width(stat) > remaining {
            continue;
        }
        let width = stat_full_width(stat).min(remaining);
        stat_cell(buffer, Rect::new(x, rect.y, width, 1), stat, paint.palette);
        placed = true;
        x = x.saturating_add(width).saturating_add(STAT_FLOW_GAP);
        if x >= rect.right() {
            break;
        }
    }
    if let Some(first) = stats.first().filter(|_| !placed) {
        let width = crate::ui::display_width_u16(&first.value).min(rect.width);
        stat_cell(
            buffer,
            Rect::new(rect.x, rect.y, width, 1),
            first,
            paint.palette,
        );
    }
}

/// 同一张卡片里各 meter 的列宽：标签补齐、数字右对齐、说明补齐，条形起止对齐。
#[derive(Clone, Copy, Default)]
struct MeterColumns {
    label: u16,
    value: u16,
    detail: u16,
}

/// 标签列最宽多少：超长的服务端标签不把整张卡的条形挤短。
const METER_LABEL_MAX: u16 = 14;

impl MeterColumns {
    fn of<'a>(lines: impl IntoIterator<Item = &'a Line<'a>>) -> Self {
        let mut columns = Self::default();
        for line in lines {
            if let Line::Meter(meter) = line {
                let width = crate::ui::display_width_u16;
                columns.label = columns.label.max(width(&meter.label).min(METER_LABEL_MAX));
                columns.value = columns.value.max(width(&meter.value));
                columns.detail = columns.detail.max(meter.detail.as_deref().map_or(0, width));
            }
        }
        columns
    }
}

/// 按显示宽度在尾部（`end`）或头部补空格到 `width`；已够宽原样返回。
fn pad(text: &str, width: u16, end: bool) -> Cow<'_, str> {
    let missing = usize::from(width.saturating_sub(crate::ui::display_width_u16(text)));
    if missing == 0 {
        Cow::Borrowed(text)
    } else if end {
        Cow::Owned(format!("{text}{}", " ".repeat(missing)))
    } else {
        Cow::Owned(format!("{}{text}", " ".repeat(missing)))
    }
}

/// 一条额度 meter：kit `meter_row`（数字永不裁，窄时先砍条形）；过期窗口的条形
/// 再叠 DIM。按卡片的列宽补齐，同卡的条形起止对齐。
fn meter_line(
    buffer: &mut Buffer,
    rect: Rect,
    meter: &Meter<'_>,
    columns: MeterColumns,
    paint: Paint<'_>,
) {
    let label = pad(&meter.label, columns.label, true);
    let value = pad(&meter.value, columns.value, false);
    // 同卡有别的 meter 带说明时，没说明的这条也占住说明列，数字才上下对齐。
    let detail = match meter.detail.as_deref() {
        Some(detail) => Some(pad(detail, columns.detail, true)),
        None if columns.detail > 0 => Some(pad("", columns.detail, true)),
        None => None,
    };
    let row = MeterRow {
        label: &label,
        value: &value,
        detail: detail.as_deref(),
        gauge: GaugeSpec {
            ratio: meter.ratio,
            window_progress: meter.window,
            ascii: paint.ascii,
            ..GaugeSpec::default()
        },
        stale: meter.stale,
    };
    render_meter_row(buffer, rect, &row, paint.palette);
    if meter.expired {
        dim_gauge_cells(buffer, rect, &row, paint.palette);
    }
}

/// 过期窗口只弱化一次：文字已按 stale 转成 overlay0，整行再叠 DIM 会让沿用的
/// 数字与「已过重置 · 沿用上次值」在把 DIM 渲染成半亮的终端上几乎看不清，所以
/// DIM 只加在条形格上（与仅是缓存的 stale 行区分开）。条形的起止由 kit 的宽度
/// 预算决定：用同宽的空白文字在暂存行里画两遍（空条 / 满条），两遍不同的格子
/// 就是条形——不依赖字形与主题色，标签被截断时的省略号也不会被误认。只在过期
/// 窗口上走，不在 pane 规模路径上。
fn dim_gauge_cells(buffer: &mut Buffer, rect: Rect, row: &MeterRow<'_>, palette: &Palette) {
    let blank = |text: &str| " ".repeat(usize::from(crate::ui::display_width_u16(text)));
    let (label, value) = (blank(row.label), blank(row.value));
    let detail = row.detail.map(blank);
    let area = Rect::new(0, 0, rect.width, 1);
    let scratch = |ratio: Option<f32>| {
        let mut scratch = Buffer::empty(area);
        render_meter_row(
            &mut scratch,
            area,
            &MeterRow {
                label: &label,
                value: &value,
                detail: detail.as_deref(),
                gauge: GaugeSpec {
                    ratio,
                    ascii: row.gauge.ascii,
                    ..GaugeSpec::default()
                },
                stale: row.stale,
            },
            palette,
        );
        scratch
    };
    let (empty, full) = (scratch(None), scratch(Some(1.0)));
    for offset in 0..rect.width {
        if empty[(offset, 0)] == full[(offset, 0)] {
            continue;
        }
        if let Some(cell) = buffer.cell_mut((rect.x + offset, rect.y)) {
            cell.set_style(Style::default().add_modifier(Modifier::DIM));
        }
    }
}

/// 小节标题：`标题 ────`（线段字形尊重 `ui.border_style`）。
fn section_line(buffer: &mut Buffer, rect: Rect, title: &str, paint: Paint<'_>) {
    let used = put(
        buffer,
        rect.x,
        rect.y,
        rect.width,
        title,
        Style::default()
            .fg(paint.palette.overlay1)
            .add_modifier(Modifier::BOLD),
    );
    for x in rect.x.saturating_add(used + 1)..rect.right() {
        if let Some(cell) = buffer.cell_mut((x, rect.y)) {
            cell.set_symbol(paint.glyphs.horizontal)
                .set_style(Style::default().fg(paint.palette.surface_dim));
        }
    }
}

/// 卡片状态行：（徽标没写状态时）状态 · 探测在途 · 新鲜度 · 套餐与身份。
fn meta_line(buffer: &mut Buffer, rect: Rect, card: &Card<'_>, in_flight: bool, paint: Paint<'_>) {
    let account = card.account;
    let palette = paint.palette;
    let mut parts: Vec<(Cow<'_, str>, Style)> = Vec::with_capacity(4);
    if card.family != Family::Quota {
        parts.push((
            Cow::Borrowed(status(account.status)),
            Style::default()
                .fg(status_color(account.status, palette))
                .add_modifier(Modifier::BOLD),
        ));
    }
    if in_flight {
        parts.push((
            Cow::Borrowed(tr("refreshing…", "刷新中…")),
            Style::default().fg(palette.overlay1),
        ));
    }
    parts.push((
        Cow::Owned(age_text(
            paint.now_ms,
            account.observed_at_ms,
            account.status,
        )),
        Style::default().fg(age_color(
            paint.now_ms,
            account.observed_at_ms,
            account.status,
            palette,
        )),
    ));
    let identity = match (account.plan.as_deref(), account.account_identity.as_deref()) {
        (Some(plan), Some(identity)) => Some(Cow::Owned(format!("{plan} · {identity}"))),
        (Some(value), None) | (None, Some(value)) => Some(Cow::Borrowed(value)),
        (None, None) => None,
    };
    if let Some(identity) = identity {
        parts.push((identity, Style::default().fg(palette.overlay1)));
    }
    spans(buffer, rect, &parts, palette);
}

/// 卡片里除状态行以外的一行。
fn body_line(
    buffer: &mut Buffer,
    rect: Rect,
    line: &Line<'_>,
    columns: MeterColumns,
    paint: Paint<'_>,
) {
    match line {
        // 状态行由调用方按宿主画（需要刷新状态）。
        Line::Meta => {}
        Line::Note(note) => {
            put(
                buffer,
                rect.x,
                rect.y,
                rect.width,
                note,
                Style::default().fg(paint.palette.yellow),
            );
        }
        Line::Section(title) => section_line(buffer, rect, title, paint),
        Line::Meter(meter) => meter_line(buffer, rect, meter, columns, paint),
        Line::Stats(stats) => stats_row(buffer, rect, stats, paint),
        Line::Empty(title) => {
            render_empty_state(
                buffer,
                rect,
                &EmptyState {
                    title,
                    ..EmptyState::default()
                },
                paint.palette,
            );
        }
    }
}

/// 卡片右上角的徽标：额度厂商写状态；本地 / 会话统计写声明（状态移到状态行）。
fn badge(
    family: Family,
    status_value: ObservationStatus,
    palette: &Palette,
) -> (&'static str, Color) {
    let texts = &crate::i18n::texts().monitor;
    match family {
        Family::Quota => (status(status_value), status_color(status_value, palette)),
        Family::Local => (texts.local_stats_badge, palette.blue),
        Family::Session => (texts.session_stats_badge, palette.blue),
    }
}

/// 画在下边框右侧的「+N」：卡片高度不够，还有 N 行没画。
fn hidden_marker(buffer: &mut Buffer, rect: Rect, hidden: usize, palette: &Palette) {
    let label = format!(" +{hidden} ");
    let width = crate::ui::display_width_u16(&label);
    if rect.width < width + 4 || rect.height < 2 {
        return;
    }
    put(
        buffer,
        rect.right() - 2 - width,
        rect.bottom() - 1,
        width,
        &label,
        Style::default().fg(palette.overlay1).bg(palette.panel_bg),
    );
}

/// 画一张账号卡片：标题是账号标签、徽标是状态或统计声明，选中的卡片边框取
/// accent；`rect` 比全部行矮时只画前 N 行，下边框写「+N」。
fn paint_card(
    buffer: &mut Buffer,
    rect: Rect,
    card: &Card<'_>,
    selected: bool,
    in_flight: bool,
    paint: Paint<'_>,
) {
    let account = card.account;
    let (badge_text, badge_color) = badge(card.family, account.status, paint.palette);
    let inner = render_card(
        buffer,
        rect,
        &CardSpec {
            title: &account.account_label,
            badge: Some((badge_text, badge_color)),
            focused: selected,
            hovered: false,
            dimmed: false,
        },
        paint.glyphs,
        paint.palette,
    );
    let visible = usize::from(inner.height);
    let columns = MeterColumns::of(card.lines.iter().take(visible));
    for (index, line) in card.lines.iter().take(visible).enumerate() {
        let row = Rect::new(inner.x, inner.y + index as u16, inner.width, 1);
        match line {
            Line::Meta => meta_line(buffer, row, card, in_flight, paint),
            other => body_line(buffer, row, other, columns, paint),
        }
    }
    let hidden = card.lines.len().saturating_sub(visible);
    if hidden > 0 {
        hidden_marker(buffer, rect, hidden, paint.palette);
    }
}

/// 卡片画多高。页面上是自然高度（按行滚动能看到每一条额度）；悬浮层（≤68×17，
/// 另有「打开页面」）里一张卡不超过视口高度（至少 3 行，放得下边框 + 状态行），
/// 多出的行折成下边框上的「+N」。
fn card_height(card: &Card<'_>, viewport: Rect, chrome: BodyChrome) -> usize {
    let natural = card.natural_height().min(usize::from(u16::MAX));
    match chrome {
        BodyChrome::Page => natural,
        BodyChrome::Hover => natural.min(usize::from(viewport.height).max(3)),
    }
}

/// 概览是否画成每厂商一张紧凑卡：只有一个厂商时紧凑卡没有信息增量，直接画该
/// 厂商的账号卡片。不分配。
pub(super) fn multi_vendor(accounts: &[AccountUsageSnapshot]) -> bool {
    accounts
        .first()
        .is_some_and(|first| accounts.iter().any(|account| account.agent != first.agent))
}

/// 账号卡片列表：每账号一张厂商专属卡（`cards::build_card`），按行滚动并裁剪到
/// 视口；点卡头选中该账号。返回全部卡片的总行数，页面上与 `account_rows` 同口径
/// （悬浮层里卡片按视口封顶，实际行数可能更少，滚动在渲染期再钳位）。
pub(super) fn account_cards(
    buffer: &mut Buffer,
    area: Rect,
    state: &State,
    scope: &AccountsScope<'_>,
    palette: &Palette,
    hits: &mut Vec<(Rect, Action)>,
) -> usize {
    if area.is_empty() {
        return 0;
    }
    let paint = Paint {
        palette,
        glyphs: state.glyphs,
        ascii: state.chart_glyphs.ascii(),
        now_ms: state.now_ms,
    };
    let cards = scope
        .accounts
        .iter()
        .map(|account| build_card(account, scope.refresh_of(&account.account_id), state.now_ms))
        .collect::<Vec<_>>();
    let heights = cards
        .iter()
        .map(|card| card_height(card, area, scope.chrome))
        .collect::<Vec<_>>();
    let total = heights.iter().sum::<usize>();
    let start = scope
        .scroll
        .min(total.saturating_sub(usize::from(area.height)));
    let mut y = 0_usize;
    for (card, height) in cards.iter().zip(heights) {
        let top = y as i32 - start as i32;
        y += height;
        if top + height as i32 <= 0 {
            continue;
        }
        if top >= i32::from(area.height) {
            break;
        }
        let account_id = card.account.account_id.as_str();
        let selected = scope.account == Some(account_id);
        let in_flight = scope
            .refresh_of(account_id)
            .is_some_and(|refresh| refresh.in_flight);
        let height = height as u16;
        draw_clipped(
            buffer,
            area,
            area.x,
            top,
            area.width,
            height,
            |buffer, rect| {
                paint_card(buffer, rect, card, selected, in_flight, paint);
            },
        );
        // 命中区是卡头（上边框标题行 + 状态行）：正文行留给内容，悬浮层里的空白
        // 也不被选中动作占满。
        let visible = visible_rect(area, area.x, top, area.width, height.min(CARD_HEADER_ROWS));
        if !visible.is_empty() {
            hits.push((visible, Action::Account(account_id.to_owned())));
        }
    }
    total
}

/// 概览紧凑卡的汇总行：（徽标没写状态时）状态 · 账号数 · 最近更新。
fn overview_summary(
    buffer: &mut Buffer,
    rect: Rect,
    group: &VendorGroup<'_>,
    worst: ObservationStatus,
    paint: Paint<'_>,
) {
    let texts = &crate::i18n::texts().monitor;
    let palette = paint.palette;
    let mut parts: Vec<(Cow<'_, str>, Style)> = Vec::with_capacity(3);
    if family(group.agent) != Family::Quota {
        parts.push((
            Cow::Borrowed(status(worst)),
            Style::default()
                .fg(status_color(worst, palette))
                .add_modifier(Modifier::BOLD),
        ));
    }
    let count = group.accounts.len();
    parts.push((
        if count == 1 {
            Cow::Borrowed(texts.account_one)
        } else {
            Cow::Owned(crate::i18n::fill(
                texts.accounts_fmt,
                &[("n", &count.to_string())],
            ))
        },
        Style::default().fg(palette.overlay1),
    ));
    let latest = group.latest_ms();
    parts.push((
        Cow::Owned(age_text(paint.now_ms, latest, worst)),
        Style::default().fg(age_color(paint.now_ms, latest, worst, palette)),
    ));
    spans(buffer, rect, &parts, palette);
}

/// 概览（全部厂商、仪表盘格式）：每厂商一张紧凑卡——首行是最紧张的一条额度
/// meter（本地 / 会话统计是关键数值），次行是状态与更新时间；≥96 列双栏。点卡片
/// 进入该厂商。返回总行数，与 `account_rows` 同口径。
pub(super) fn overview_cards(
    buffer: &mut Buffer,
    area: Rect,
    state: &State,
    scope: &AccountsScope<'_>,
    palette: &Palette,
    hits: &mut Vec<(Rect, Action)>,
) -> usize {
    if area.is_empty() {
        return 0;
    }
    let paint = Paint {
        palette,
        glyphs: state.glyphs,
        ascii: state.chart_glyphs.ascii(),
        now_ms: state.now_ms,
    };
    let groups = vendor_groups(scope.accounts);
    let columns = overview_columns(area.width);
    let grid_rows = groups.len().div_ceil(columns);
    let total = grid_rows * OVERVIEW_CARD_HEIGHT;
    let start = scope
        .scroll
        .min(total.saturating_sub(usize::from(area.height)));
    let gap = u16::from(columns > 1);
    let column_width = (area.width - gap) / columns as u16;
    for (index, group) in groups.iter().enumerate() {
        let top = (index / columns * OVERVIEW_CARD_HEIGHT) as i32 - start as i32;
        if top + OVERVIEW_CARD_HEIGHT as i32 <= 0 || top >= i32::from(area.height) {
            continue;
        }
        let column = (index % columns) as u16;
        let x = area.x + column * (column_width + gap);
        // 末列吃掉除不尽的余数，双栏右缘与页面对齐。
        let width = if column + 1 == columns as u16 {
            area.right() - x
        } else {
            column_width
        };
        let label = state
            .providers
            .iter()
            .find(|provider| provider.agent == group.agent)
            .map_or(group.agent, |provider| provider.label.as_str());
        let worst = group.worst_status();
        let (badge_text, badge_color) = badge(family(group.agent), worst, palette);
        let line = headline(group, state.now_ms);
        draw_clipped(
            buffer,
            area,
            x,
            top,
            width,
            OVERVIEW_CARD_HEIGHT as u16,
            |buffer, rect| {
                let inner = render_card(
                    buffer,
                    rect,
                    &CardSpec {
                        title: label,
                        badge: Some((badge_text, badge_color)),
                        ..CardSpec::default()
                    },
                    paint.glyphs,
                    paint.palette,
                );
                if inner.height >= 1 {
                    body_line(
                        buffer,
                        Rect::new(inner.x, inner.y, inner.width, 1),
                        &line,
                        MeterColumns::default(),
                        paint,
                    );
                }
                if inner.height >= 2 {
                    overview_summary(
                        buffer,
                        Rect::new(inner.x, inner.y + 1, inner.width, 1),
                        group,
                        worst,
                        paint,
                    );
                }
            },
        );
        let visible = visible_rect(area, x, top, width, OVERVIEW_CARD_HEIGHT as u16);
        if !visible.is_empty() {
            hits.push((visible, Action::Provider(group.agent.to_owned())));
        }
    }
    total
}

/// 仪表盘格式下的滚动真源（行数）：概览按紧凑卡网格（列数取上一帧页面宽度），
/// 否则逐账号的卡片自然高度。与 `account_cards` / `overview_cards` 同口径；卡片被
/// 视口封顶时实际行数更少，渲染期再按实际行数钳位。
pub(super) fn dashboard_rows(
    accounts: &[AccountUsageSnapshot],
    refresh_states: &[UsageRefreshState],
    now_ms: u64,
    overview_width: Option<u16>,
) -> usize {
    if let Some(width) = overview_width {
        return vendor_groups(accounts)
            .len()
            .div_ceil(overview_columns(width))
            * OVERVIEW_CARD_HEIGHT;
    }
    accounts
        .iter()
        .map(|account| {
            let refresh = refresh_states
                .iter()
                .find(|refresh| refresh.account_id == account.account_id);
            build_card(account, refresh, now_ms).natural_height()
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::super::super::test_support::*;
    use super::*;

    fn metric(id: &str, percent: Option<f64>) -> UsageMetric {
        UsageMetric {
            id: id.into(),
            label: id.into(),
            unit: "%".into(),
            scope: "account".into(),
            used_percent: percent,
            ..Default::default()
        }
    }

    fn account(agent: &str, id: &str, status: ObservationStatus) -> AccountUsageSnapshot {
        AccountUsageSnapshot {
            account_id: id.into(),
            account_label: id.into(),
            agent: agent.into(),
            provider: agent.into(),
            status,
            observed_at_ms: 1_000,
            metrics: vec![
                metric("five_hour", Some(42.0)),
                metric("seven_day", Some(3.0)),
            ],
            ..Default::default()
        }
    }

    /// 已选 claude 的页面：说明行、套餐 / 身份、目录信任说明与无指标账号都在。
    fn cards_state() -> State {
        let mut state = State::new(&config());
        state.now_ms = 14_000;
        state.selected_provider = Some("claude".into());
        let mut first = account("claude", "claude:default", ObservationStatus::Ready);
        first.message = Some("hello".into());
        first.plan = Some("Max".into());
        let mut second = account("claude", "claude:work", ObservationStatus::NotAuthenticated);
        second.account_identity = Some("me@example.invalid".into());
        let mut empty = account("claude", "claude:empty", ObservationStatus::Warming);
        empty.metrics.clear();
        state.accounts = vec![first, second, empty];
        state.refresh_states = vec![UsageRefreshState {
            account_id: "claude:work".into(),
            trust_required: true,
            ..Default::default()
        }];
        state
    }

    /// 在比 `area` 大一圈的缓冲里画卡片列表，便于断言视口外的行没被写。
    fn draw_cards(state: &State, area: Rect) -> (Buffer, usize, Vec<(Rect, Action)>) {
        let scope = page_scope(state);
        let mut buffer = Buffer::empty(Rect::new(0, 0, area.right() + 2, area.bottom() + 2));
        let mut hits = Vec::new();
        let rows = account_cards(
            &mut buffer,
            area,
            state,
            &scope,
            &config().palette,
            &mut hits,
        );
        (buffer, rows, hits)
    }

    /// 卡片实际画出的总行数与滚动真源 `account_rows` 一致（说明行 / 目录信任 /
    /// 空态各占一行，套餐与身份并进状态行）；每账号一个卡头命中区。
    #[test]
    fn card_rows_match_the_scroll_source_of_truth() {
        let state = cards_state();
        let (_, rows, hits) = draw_cards(&state, Rect::new(0, 0, 100, 200));
        assert_eq!(
            rows,
            account_rows(&state, &state.accounts, &state.refresh_states)
        );
        // 5 + 1（message）/ 5 + 1（目录信任）/ 空态卡 4。
        assert_eq!(rows, 6 + 6 + 4);
        assert_eq!(hits.len(), 3, "每账号一个卡头命中区");
        assert!(hits.iter().all(|(rect, _)| rect.height == CARD_HEADER_ROWS));
    }

    /// 视口比内容矮：按行滚动，部分可见的卡片裁剪到视口内，命中区只含可见的卡头。
    #[test]
    fn scrolled_cards_are_clipped_to_the_viewport() {
        let mut state = cards_state();
        state.account_scroll = 3;
        let area = Rect::new(2, 5, 60, 8);
        let (buffer, _, hits) = draw_cards(&state, area);
        // 第一张卡的上边框与状态行滚出视口：它没有命中区，视口首行是它的正文。
        assert!(!hits
            .iter()
            .any(|(_, action)| matches!(action, Action::Account(id) if id == "claude:default")));
        assert!(
            row_text(&buffer, area.y).contains("42%"),
            "{}",
            buffer_text(&buffer)
        );
        // 第二张卡的卡头完整可见，命中区在视口内。
        let (rect, _) = hits
            .iter()
            .find(|(_, action)| matches!(action, Action::Account(id) if id == "claude:work"))
            .expect("第二张卡的卡头");
        assert!(contains_rect(area, *rect));
        assert!(row_has(&buffer, rect.y, "claude:work"));
        // 视口外一行不写。
        assert!(row_text(&buffer, area.bottom()).trim().is_empty());
        assert!(row_text(&buffer, area.y - 1).trim().is_empty());
    }

    /// 多厂商概览：紧凑卡网格的行数与滚动真源一致（列数取上一帧页面宽度）；
    /// 单厂商时不画紧凑卡。
    #[test]
    fn overview_rows_follow_the_grid_and_single_vendor_uses_cards() {
        let mut state = State::new(&config());
        state.now_ms = 14_000;
        state.accounts = vec![
            account("claude", "claude:default", ObservationStatus::Ready),
            account("codex", "codex:default", ObservationStatus::Ready),
            account("kimi", "kimi:default", ObservationStatus::Error),
        ];
        for (page_width, columns) in [(122_u16, 2_usize), (80, 1)] {
            state.page_rect = Rect::new(0, 0, page_width, 40);
            let area = Rect::new(0, 0, page_width - 2, 30);
            let scope = page_scope(&state);
            let mut buffer = Buffer::empty(area);
            let mut hits = Vec::new();
            let rows = overview_cards(
                &mut buffer,
                area,
                &state,
                &scope,
                &config().palette,
                &mut hits,
            );
            assert_eq!(rows, 3_usize.div_ceil(columns) * OVERVIEW_CARD_HEIGHT);
            assert_eq!(
                rows,
                account_rows(&state, &state.accounts, &state.refresh_states)
            );
            assert_eq!(hits.len(), 3, "每厂商一张可点的紧凑卡");
        }
        assert!(multi_vendor(&state.accounts));
        state.accounts.truncate(1);
        assert!(!multi_vendor(&state.accounts));
        assert_eq!(
            account_rows(&state, &state.accounts, &state.refresh_states),
            5,
            "单厂商按账号卡片计行"
        );
        // 悬浮层的账号不按概览算（切片不是页面的 `accounts`）。
        let hover = state.accounts.clone();
        state.accounts = vec![
            account("claude", "claude:default", ObservationStatus::Ready),
            account("codex", "codex:default", ObservationStatus::Ready),
        ];
        assert_eq!(account_rows(&state, &hover, &[]), 5);
    }

    /// 过期窗口只弱化一次：文字随 stale 转灰，DIM 只落在条形格上（ascii 字形同理，
    /// 数字里的 `.` 不会被当成空条）；窄到条形被砍掉时整行都不带 DIM。
    #[test]
    fn expired_meter_dims_only_the_bar() {
        let palette = config().palette;
        let meter = Meter {
            label: Cow::Borrowed("Weekly"),
            value: "91.5%".into(),
            detail: Some("past reset".into()),
            ratio: Some(0.915),
            window: None,
            stale: true,
            expired: true,
        };
        let columns = MeterColumns {
            label: 6,
            value: 5,
            detail: 10,
        };
        let dim = |buffer: &Buffer, x: u16| buffer[(x, 0)].modifier.contains(Modifier::DIM);
        for ascii in [false, true] {
            let paint = Paint {
                palette: &palette,
                glyphs: crate::ui::BorderGlyphs::SINGLE,
                ascii,
                now_ms: 0,
            };
            let area = Rect::new(0, 0, 40, 1);
            let mut buffer = Buffer::empty(area);
            meter_line(&mut buffer, area, &meter, columns, paint);
            let text = row_text(&buffer, 0);
            let column = |needle: &str| {
                let byte = text.find(needle).expect("文字可见");
                text[..byte].chars().count() as u16
            };
            for needle in ["Weekly", "91.5%", "past reset"] {
                let start = column(needle);
                for x in start..start + needle.len() as u16 {
                    assert!(!dim(&buffer, x), "ascii={ascii}: {needle} 不叠 DIM: {text}");
                    assert_eq!(buffer[(x, 0)].fg, palette.overlay0, "文字按 stale 转灰");
                }
            }
            let (filled, empty) = if ascii { ("#", ".") } else { ("━", "░") };
            let bar = (0..area.width)
                .filter(|x| matches!(buffer[(*x, 0)].symbol(), s if s == filled || s == empty))
                .filter(|x| *x < column("91.5%"))
                .collect::<Vec<_>>();
            assert!(!bar.is_empty(), "ascii={ascii}: 有条形: {text}");
            assert!(bar.iter().all(|x| dim(&buffer, *x)), "条形格 DIM: {text}");
            let dimmed = (0..area.width).filter(|x| dim(&buffer, *x)).count();
            assert_eq!(dimmed, bar.len(), "只有条形格带 DIM: {text}");
        }
        let area = Rect::new(0, 0, 10, 1);
        let mut buffer = Buffer::empty(area);
        let paint = Paint {
            palette: &palette,
            glyphs: crate::ui::BorderGlyphs::SINGLE,
            ascii: false,
            now_ms: 0,
        };
        meter_line(&mut buffer, area, &meter, columns, paint);
        assert!(row_text(&buffer, 0).contains("91.5%"));
        assert!(
            (0..area.width).all(|x| !dim(&buffer, x)),
            "条形被砍掉时没有 DIM: {}",
            row_text(&buffer, 0)
        );
    }

    /// 带本会话费用 / 时长 / API 时长的 claude 账号（与 `parse::claude_session` 同形：
    /// 时长是服务端格式化好的 `text_value`）。
    fn session_account() -> AccountUsageSnapshot {
        let session = |id: &str, unit: &str| UsageMetric {
            id: id.into(),
            label: id.into(),
            unit: unit.into(),
            scope: "session".into(),
            ..Default::default()
        };
        AccountUsageSnapshot {
            account_id: "claude:default".into(),
            account_label: "Claude Code".into(),
            agent: "claude".into(),
            provider: "claude".into(),
            status: ObservationStatus::Ready,
            observed_at_ms: 1_000,
            metrics: vec![
                UsageMetric {
                    amount_decimal: Some("0.1082".into()),
                    ..session("cost/total_cost_usd", "USD")
                },
                UsageMetric {
                    used: Some(259_000.0),
                    text_value: Some("4m19s".into()),
                    ..session("cost/total_duration_ms", "ms")
                },
                UsageMetric {
                    used: Some(14_000.0),
                    text_value: Some("14s".into()),
                    ..session("cost/total_api_duration_ms", "ms")
                },
            ],
            ..Default::default()
        }
    }

    /// 真机 L3：窄卡片（claude-24 的内宽 24）一行放不下三个「标签 数值」时按 token
    /// 整段取舍——标签与数字要么完整出现、要么整项不画，不再截成「时… 4m… API…」；
    /// 宽卡片仍是三列网格。
    #[test]
    fn narrow_stats_rows_keep_or_drop_whole_tokens() {
        let _guard = crate::i18n::lang_guard(crate::i18n::Lang::ZhCn);
        let mut state = State::new(&config());
        state.now_ms = 14_000;
        state.selected_provider = Some("claude".into());
        state.accounts = vec![session_account()];
        // 卡片：上边框、状态行，第 3 行是数值行。
        let stats_row = |width: u16| {
            let (buffer, _, _) = draw_cards(&state, Rect::new(0, 0, width, 6));
            row_text(&buffer, 2).replace(' ', "")
        };
        // 卡宽 26（内宽 24）：费用与时长整项放得下，API 时长整项放不下就不画。
        assert_eq!(stats_row(26), "│费用$0.1082时长4m19s│");
        // 内宽 18：只放得下第一项；内宽 12 恰好放下「费用 $0.1082」。
        assert_eq!(stats_row(20), "│费用$0.1082│");
        assert_eq!(stats_row(14), "│费用$0.1082│");
        // 内宽 10：费用整项放不下就整项让位，放得下的时长整项照画。
        assert_eq!(stats_row(12), "│时长4m19s│");
        // 内宽 7：哪一项的「标签 值」都放不下，只画第一项的值。
        assert_eq!(stats_row(9), "│$0.1082│");
        for width in [26_u16, 24, 22, 20, 18, 16, 14, 12, 10, 9] {
            let row = stats_row(width);
            assert!(!row.contains('…'), "宽 {width}: 不截成半截：{row}");
            if row.contains("4m") {
                assert!(row.contains("4m19s"), "宽 {width}: 时长整段：{row}");
            }
            if row.contains("API") {
                assert!(
                    row.contains("API时长14s"),
                    "宽 {width}: API 时长整项：{row}"
                );
            }
        }
        // 宽卡片：三列网格，列间竖线，时长与数字同样加粗。
        let (buffer, _, _) = draw_cards(&state, Rect::new(0, 0, 100, 6));
        let y = (0..buffer.area.height)
            .find(|y| row_has(&buffer, *y, "API时长"))
            .expect("宽卡片的数值行");
        let row = row_text(&buffer, y).replace(' ', "");
        assert!(row.contains("费用$0.1082│时长4m19s│API时长14s"), "{row}");
        let x = (0..buffer.area.width)
            .find(|x| buffer[(*x, y)].symbol() == "4")
            .expect("时长数字");
        assert!(
            buffer[(x, y)].modifier.contains(Modifier::BOLD),
            "时长与数字同一语气"
        );
    }

    /// 数值项：数字永不截断，列宽不够先整段丢标签，连数值都放不下就不画。
    #[test]
    fn stat_cells_keep_numbers_and_drop_labels_first() {
        let palette = config().palette;
        let stat = Stat {
            label: Cow::Borrowed("Sessions"),
            value: "12345".into(),
            tone: Tone::Number,
        };
        for (width, expected) in [
            (20, "Sessions       12345"),
            (14, "Sessions 12345"),
            // 放不下「标签 数值」时标签整段丢掉，不截成「Se…」（真机 L3）。
            (13, "        12345"),
            (9, "    12345"),
            (5, "12345"),
            // 连数值都放不下：不画半截数字。
            (4, ""),
        ] {
            let area = Rect::new(0, 0, width, 1);
            let mut buffer = Buffer::empty(area);
            stat_cell(&mut buffer, area, &stat, &palette);
            assert_eq!(
                row_text(&buffer, 0).trim_end(),
                expected.trim_end(),
                "宽 {width}"
            );
        }
    }
}
