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

/// 一个数值项：数字（`Number` / `Muted`）永不截断，列宽不够先截再丢标签；
/// 文本（`Text`）保留标签、值带省略号截断。值靠右。
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
    let (label_w, value_w) = match stat.tone {
        Tone::Text => {
            let label_w = label_width.min(rect.width.saturating_sub(4));
            let value_w = value_width.min(rect.width.saturating_sub(label_w + 1));
            (label_w, value_w)
        }
        Tone::Number | Tone::Muted => {
            let value_w = value_width.min(rect.width);
            let spare = rect.width - value_w;
            // 标签至少留 2 列才画（1 列只剩省略号，没有信息量）。
            let label_w = if spare >= 3 {
                label_width.min(spare - 1)
            } else {
                0
            };
            (label_w, value_w)
        }
    };
    if label_w > 0 {
        put(buffer, rect.x, rect.y, label_w, &stat.label, label_style);
    }
    put(
        buffer,
        rect.right().saturating_sub(value_w),
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

/// 一行 1–3 个数值项，落在固定的三列网格上（列间 ` │ `，窄时退成一格空白）。
/// 行末那一项可以向右延伸到放得下「标签 值」（例如模型名），再长的文本才截断。
fn stats_row(buffer: &mut Buffer, rect: Rect, stats: &[Stat<'_>], paint: Paint<'_>) {
    let count = stats.len().min(usize::from(STAT_COLUMNS)) as u16;
    if rect.is_empty() || count == 0 {
        return;
    }
    let gap = if rect.width >= STAT_COLUMNS * 14 {
        3
    } else {
        1
    };
    let column =
        (rect.width.saturating_sub(gap * (STAT_COLUMNS - 1)) / STAT_COLUMNS).min(STAT_COLUMN_MAX);
    if column == 0 {
        return;
    }
    for (index, stat) in stats.iter().take(usize::from(count)).enumerate() {
        let index = index as u16;
        let x = rect.x + index * (column + gap);
        if x >= rect.right() {
            break;
        }
        let width = if index + 1 < count {
            column
        } else {
            let needed = crate::ui::display_width_u16(&stat.label)
                .saturating_add(1)
                .saturating_add(crate::ui::display_width_u16(&stat.value));
            column.max(needed).min(rect.right() - x)
        };
        if index > 0 && gap == 3 {
            if let Some(cell) = buffer.cell_mut((x - 2, rect.y)) {
                cell.set_symbol(paint.glyphs.vertical)
                    .set_style(Style::default().fg(paint.palette.surface_dim));
            }
        }
        stat_cell(buffer, Rect::new(x, rect.y, width, 1), stat, paint.palette);
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

/// 一条额度 meter：kit `meter_row`（数字永不裁，窄时先砍条形）+ 过期窗口 DIM。
/// 按卡片的列宽补齐，同卡的条形起止对齐。
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
    render_meter_row(
        buffer,
        rect,
        &MeterRow {
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
        },
        paint.palette,
    );
    if meter.expired {
        buffer.set_style(rect, Style::default().add_modifier(Modifier::DIM));
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

    /// 数值项：数字永不截断，列宽不够先截再丢标签。
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
            (9, "Se… 12345"),
            (6, " 12345"),
            (5, "12345"),
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
