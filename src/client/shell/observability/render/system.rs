//! 系统页：主机摘要条、资源卡片（kit 原语：card / meter_row / gauge /
//! braille_chart / table）与进程详情对话框。布局与命中计算全在这里完成，
//! 状态只读；历史迷你图的桶与网络样本都拷进栈上缓冲，不在渲染期分配（被页面
//! 底边裁到的卡片例外：借一块一卡大小的暂存缓冲，见 `cut_system_card`）。

use std::borrow::Cow;

use super::*;
use crate::ui::kit::braille_chart::{render_area_chart, render_sparkline, ChartGlyphs};
use crate::ui::kit::card::{render_card, CardSpec};
use crate::ui::kit::gauge::{gauge_color, GaugeSpec, GaugeThresholds};
use crate::ui::kit::meter_row::{
    render_meter_row, render_meter_row_aligned, MeterColumns, MeterRow,
};
use crate::ui::kit::table::{
    render_table, Column, ColumnWidth, SortState, TableCell, TableRender, TableState,
};

/// 历史迷你图最多分多少个采样槽（盲文 256 列）；再宽的图右对齐留白。
const HISTORY_SLOTS: usize = 512;

/// 卡片双列布局的最小正文宽度（面板宽 96 列、去掉两侧边框后 94）；更窄时单列。
const TWO_COLUMN_MIN_WIDTH: u16 = 94;

/// 逐核迷你条每格的宽度（标签 + 最窄条形 + 数字）。
const CORE_SLOT_WIDTH: u16 = 16;

/// 系统页里内容可滚动的卡片；`CardScrollLimits` 按这个顺序存各卡的滚动上界。
const SCROLLABLE_CARDS: [&str; 6] = ["cores", "gpu", "disks", "network", "sensors", "processes"];

/// 系统页各可滚动卡片本次绘制的滚动上界：卡片内首个可见条目（逐核卡是首个
/// 可见行）下标的最大值，由卡片内高与每个条目占的行数算出——内容放得下时为
/// 0，滚到上界时最后一个条目正好完整露出。卡片自己按它钳 offset，
/// `State::scroll_card` 也按它钳，两处是同一个值；没画出内容的卡片为 `None`。
/// 定长数组、`Copy`，渲染期不分配。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(in crate::client::shell) struct CardScrollLimits([Option<usize>; SCROLLABLE_CARDS.len()]);

impl CardScrollLimits {
    fn slot(card: &str) -> Option<usize> {
        SCROLLABLE_CARDS.iter().position(|id| *id == card)
    }

    fn set(&mut self, card: &str, max: usize) {
        if let Some(slot) = Self::slot(card) {
            self.0[slot] = Some(max);
        }
    }

    /// `card` 本次绘制的滚动上界；没画出来（或不可滚动）时为 `None`。
    pub(in crate::client::shell) fn get(&self, card: &str) -> Option<usize> {
        Self::slot(card).and_then(|slot| self.0[slot])
    }

    /// 合并同一帧里另一次绘制的上界（多个停靠面板依次绘制）：只覆盖对方画出
    /// 来的卡片。
    pub(in crate::client::shell::observability) fn merge(&mut self, other: &Self) {
        for (mine, theirs) in self.0.iter_mut().zip(other.0) {
            if theirs.is_some() {
                *mine = theirs;
            }
        }
    }
}

/// kit 表格的滚动上界：与 `render_table` 内部的钳位同一个值（行数 − 表体行数）；
/// 没有表体时不可滚动。
fn table_scroll_max(rows: usize, render: &TableRender) -> usize {
    if render.body.is_empty() {
        0
    } else {
        rows.saturating_sub(usize::from(render.body.height))
    }
}

/// 迷你图一列容纳的样本数：盲文每格两列、方块与 ASCII 一格一列（与 kit 的
/// 字形定义一致，见 `braille_chart` 模块说明）。
fn samples_per_column(glyphs: ChartGlyphs) -> usize {
    match glyphs {
        ChartGlyphs::Braille => 2,
        ChartGlyphs::Blocks | ChartGlyphs::Ascii => 1,
    }
}

/// 把历史点按时间窗口均分到 `slots` 个桶写进栈上的 `out`（同桶取平均，空桶
/// NaN → 迷你图留白），返回实际槽数。
fn history_into(
    state: &State,
    slots: usize,
    value: impl Fn(&HistoryPoint) -> Option<f32>,
    out: &mut [f32; HISTORY_SLOTS],
) -> usize {
    let slots = slots.min(HISTORY_SLOTS);
    if slots == 0 {
        return 0;
    }
    let mut counts = [0u16; HISTORY_SLOTS];
    out[..slots].fill(0.0);
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
        let Some(value) = value(point).filter(|value| value.is_finite()) else {
            continue;
        };
        let index = ((point.at - start) as u128 * slots as u128 / u128::from(duration)) as usize;
        if index >= slots {
            continue;
        }
        out[index] += value.clamp(0.0, 100.0);
        counts[index] = counts[index].saturating_add(1);
    }
    for (sum, count) in out.iter_mut().zip(counts.iter()).take(slots) {
        if *count > 0 {
            *sum /= f32::from(*count);
        } else {
            *sum = f32::NAN;
        }
    }
    slots
}

/// 在 `area` 画一条 0..=100 定标的历史迷你图。
fn history_chart(
    buffer: &mut Buffer,
    area: Rect,
    state: &State,
    value: impl Fn(&HistoryPoint) -> Option<f32>,
    color: Color,
) {
    if area.is_empty() {
        return;
    }
    let glyphs = state.chart_glyphs.chart();
    let mut samples = [0f32; HISTORY_SLOTS];
    let slots = history_into(
        state,
        usize::from(area.width) * samples_per_column(glyphs),
        value,
        &mut samples,
    );
    render_sparkline(
        buffer,
        area,
        &samples[..slots],
        Some(100.0),
        glyphs,
        Style::default().fg(color),
    );
}

/// 一行「左标签 + 右值」：值靠右且优先保留，标签按剩余宽度截断。
fn label_value_row(
    buffer: &mut Buffer,
    rect: Rect,
    label: &str,
    label_style: Style,
    value: &str,
    value_style: Style,
) {
    if rect.is_empty() {
        return;
    }
    let value_width = crate::ui::display_width_u16(value).min(rect.width);
    let value_x = rect.right() - value_width;
    text(
        buffer,
        Rect::new(value_x, rect.y, value_width, 1),
        0,
        value,
        value_style,
    );
    let label_width = value_x.saturating_sub(rect.x).saturating_sub(1);
    if label_width > 0 {
        text(
            buffer,
            Rect::new(rect.x, rect.y, label_width, 1),
            0,
            label,
            label_style,
        );
    }
}

/// 十进制整数的位数（即其文本的显示宽度），不经格式化、不分配。
fn decimal_width(mut value: usize) -> u16 {
    let mut width = 1;
    while value >= 10 {
        value /= 10;
        width += 1;
    }
    width
}

/// 阈值着色：≥90% 红、≥75% 黄，其余不改色。
fn load_color(ratio: f32, palette: &Palette) -> Option<Color> {
    if ratio >= 0.9 {
        Some(palette.red)
    } else if ratio >= 0.75 {
        Some(palette.yellow)
    } else {
        None
    }
}

/// 主机摘要条：`主机名 · 系统 · 已运行 · 更新时间 · 采样间隔`，右端是
/// 「编辑布局 / 完成」入口。
fn summary_bar(
    buffer: &mut Buffer,
    area: Rect,
    state: &State,
    sample: &SystemMetricsSnapshot,
    palette: &Palette,
    hits: &mut Vec<(Rect, Action)>,
) {
    if area.is_empty() {
        return;
    }
    let texts = &crate::i18n::texts().monitor;
    let label = if state.layout_editing {
        texts.edit_layout_done
    } else {
        texts.edit_layout
    };
    let button_width = crate::ui::modal_button_width(&format!(" {label} ")).min(area.width);
    let button_x = area.right() - button_width;
    secondary_button(
        buffer,
        Rect::new(button_x, area.y, button_width, 1),
        label,
        Action::EditLayout,
        palette,
        hits,
    );
    let mut summary = sample.hostname.clone();
    let os = if sample.operating_system.is_empty() {
        sample.environment.as_str()
    } else {
        sample.operating_system.as_str()
    };
    if !os.is_empty() {
        summary.push_str(" · ");
        summary.push_str(os);
    }
    if sample.uptime_seconds > 0 {
        summary.push_str(" · ");
        summary.push_str(&crate::i18n::fill(
            texts.uptime_fmt,
            &[("span", &span_text(sample.uptime_seconds))],
        ));
    }
    summary.push_str(" · ");
    summary.push_str(&age_text(state.now_ms, sample.sampled_at_ms, sample.status));
    if sample.status != ObservationStatus::Ready {
        summary.push_str(" · ");
        summary.push_str(status(sample.status));
    }
    summary.push_str(" · ");
    summary.push_str(&crate::i18n::fill(
        texts.interval_fmt,
        &[("ms", &sample.interval_ms.to_string())],
    ));
    text(
        buffer,
        Rect::new(
            area.x,
            area.y,
            button_x.saturating_sub(area.x).saturating_sub(1),
            1,
        ),
        0,
        &summary,
        Style::default().fg(palette.overlay1),
    );
}

/// 卡片网格的页面滚动度量：`cards` 张卡按 `columns` 列排成行，每行占
/// `card_height` 行、行与行之间隔 1 行（最后一行不需要间隔），卡片区可用高度
/// `height`。上界是让最后一行卡片完整露出的最小起始行；卡片比可用高度还高时
/// 一屏按一行算。
fn card_grid_scroll(cards: usize, columns: u16, height: u16, card_height: u16) -> PageScroll {
    let rows = cards.div_ceil(usize::from(columns.max(1)));
    let visible = usize::from(height.saturating_add(1) / card_height.saturating_add(1)).max(1);
    PageScroll {
        max: rows.saturating_sub(visible),
        screen: visible,
    }
}

/// 系统页：摘要条 + 卡片网格（`TWO_COLUMN_MIN_WIDTH` 起双列）。每张卡片走 kit
/// `card`；编辑布局模式下卡片顶边带 ↑↓，未选中的卡片转灰。可滚动卡片的滚动
/// 上界写进 `limits`（见 `CardScrollLimits`）；返回页面级滚动度量（见
/// `card_grid_scroll`），还没有采样时不画卡片、返回 `None`。
pub(super) fn monitor(
    buffer: &mut Buffer,
    area: Rect,
    state: &State,
    cx: &ChromeContext<'_>,
    hits: &mut Vec<(Rect, Action)>,
    limits: &mut CardScrollLimits,
) -> Option<PageScroll> {
    let palette = cx.palette;
    let Some(sample) = state.metrics.as_ref() else {
        text(
            buffer,
            area,
            1,
            tr("Connecting to the host sampler…", "正在连接主机采样器…"),
            Style::default().fg(palette.overlay0),
        );
        return None;
    };
    if sample.sampled_at_ms == 0 {
        text(
            buffer,
            area,
            1,
            tr("Waiting for the first sample…", "等待第一份有效采样…"),
            Style::default().fg(palette.overlay0),
        );
        return None;
    }
    summary_bar(
        buffer,
        Rect::new(area.x, area.y, area.width, area.height.min(1)),
        state,
        sample,
        palette,
        hits,
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
    let columns = if area.width >= TWO_COLUMN_MIN_WIDTH {
        2
    } else {
        1
    };
    let gap = 1_u16;
    let width = area.width.saturating_sub(gap * (columns - 1)) / columns;
    let card_height = state
        .monitor
        .card_height
        .clamp(7, 24)
        .min(area.height.saturating_sub(2).max(7));
    // 页面按整行卡片滚动（`state.scroll` 是首个可见行）：双列时一次换一整行，
    // 卡片不会在两列之间来回换位；钳到让最后一行完整露出的起始行，不留空屏。
    let scroll = card_grid_scroll(
        sections.len(),
        columns,
        area.height.saturating_sub(2),
        card_height,
    );
    let start = state
        .scroll
        .min(scroll.max)
        .saturating_mul(usize::from(columns));
    for (index, section) in sections.iter().enumerate().skip(start) {
        let position = (index - start) as u16;
        let x = area.x + (position % columns) * (width + gap);
        let y = area.y + 2 + (position / columns) * (card_height + 1);
        if y >= area.bottom() {
            break;
        }
        let rect = Rect::new(x, y, width, card_height);
        let visible = Rect::new(x, y, width, card_height.min(area.bottom() - y));
        if visible == rect {
            system_card(buffer, rect, section, state, sample, cx, hits, limits);
        } else {
            cut_system_card(
                buffer, rect, visible, section, state, sample, cx, hits, limits,
            );
        }
    }
    Some(scroll)
}

/// 页面底边只露出一截的卡片（最后一行卡片）：整张卡按完整卡高画进同位置的
/// 暂存缓冲，再只拷回可见的几行——与账号页、偏好页一样裁在视口边缘，露出的是
/// 卡片的标题与真实的首几行、没有下边框。把卡压扁画进剩下的几行会画出一张
/// 「完整的矮卡」：只剩两行时是一张只有上下边框的空卡，高一点时正文按矮卡重排、
/// 像是卡里没有更多数据，卡内滚动上界也按压扁后的高度算。命中区裁到可见部分；
/// 暂存缓冲每帧至多一行卡片、每张一卡大小。
fn cut_system_card(
    buffer: &mut Buffer,
    rect: Rect,
    visible: Rect,
    id: &str,
    state: &State,
    sample: &SystemMetricsSnapshot,
    cx: &ChromeContext<'_>,
    hits: &mut Vec<(Rect, Action)>,
    limits: &mut CardScrollLimits,
) {
    let mut scratch = Buffer::empty(rect);
    // 可见部分先照搬页面上已有的单元格：卡片没写到的样式与直接画时一致。
    copy_cells(buffer, &mut scratch, visible);
    let first = hits.len();
    system_card(&mut scratch, rect, id, state, sample, cx, hits, limits);
    copy_cells(&scratch, buffer, visible);
    let mut index = first;
    while index < hits.len() {
        let clipped = hits[index].0.intersection(visible);
        if clipped.is_empty() {
            hits.remove(index);
        } else {
            hits[index].0 = clipped;
            index += 1;
        }
    }
}

/// 把 `area` 内的单元格从 `from` 拷到 `to`（两边都有的格才拷）。
fn copy_cells(from: &Buffer, to: &mut Buffer, area: Rect) {
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            if let (Some(source), Some(target)) = (from.cell((x, y)), to.cell_mut((x, y))) {
                *target = source.clone();
            }
        }
    }
}

/// 画一张系统页卡片：kit `card` 外框（编辑布局时顶边带 ↑↓）与卡片正文；卡片与
/// 按钮的命中区写进 `hits`，可滚动卡片的滚动上界写进 `limits`。
fn system_card(
    buffer: &mut Buffer,
    rect: Rect,
    id: &str,
    state: &State,
    sample: &SystemMetricsSnapshot,
    cx: &ChromeContext<'_>,
    hits: &mut Vec<(Rect, Action)>,
    limits: &mut CardScrollLimits,
) {
    let palette = cx.palette;
    let selected = state.selected_card.as_deref() == Some(id);
    let group_status = sample
        .group_status
        .get(id)
        .copied()
        .unwrap_or(ObservationStatus::Ready);
    // 非 Ready 的分组在顶边右侧挂状态徽标；编辑布局时那里放 ↑↓，不挂徽标。
    let badge = (group_status != ObservationStatus::Ready && !state.layout_editing)
        .then(|| (status(group_status), status_color(group_status, palette)));
    let spec = CardSpec {
        title: section_title(id),
        badge,
        focused: selected,
        hovered: false,
        dimmed: state.layout_editing && state.selected_card.is_some() && !selected,
    };
    let inner = render_card(buffer, rect, &spec, cx.glyphs, palette);
    hits.push((rect, Action::Card(id.to_owned())));
    if state.layout_editing && rect.width > 18 {
        secondary_button(
            buffer,
            Rect::new(rect.right() - 8, rect.y, 3, 1),
            "↑",
            Action::CardMove(id.to_owned(), -1),
            palette,
            hits,
        );
        secondary_button(
            buffer,
            Rect::new(rect.right() - 4, rect.y, 3, 1),
            "↓",
            Action::CardMove(id.to_owned(), 1),
            palette,
            hits,
        );
    }
    if inner.is_empty() {
        return;
    }
    let offset = state.card_scroll.get(id).copied().unwrap_or(0);
    let max = match id {
        "cpu" => {
            cpu_card(buffer, inner, state, sample, palette);
            return;
        }
        "memory" => {
            memory_card(buffer, inner, state, sample, palette);
            return;
        }
        "cores" => cores_card(buffer, inner, state, sample, palette, offset, hits),
        "gpu" => gpu_card(buffer, inner, state, sample, palette, offset),
        "disks" => disks_card(buffer, inner, state, sample, palette, offset),
        "network" => network_card(buffer, inner, state, sample, palette, offset),
        "sensors" => sensors_card(buffer, inner, state, sample, palette, offset),
        _ => processes_card(buffer, inner, state, sample, palette, offset, hits),
    };
    limits.set(id, max);
}

/// CPU 卡：总体 gauge（数字永不裁）、型号 / 选中核说明、历史迷你图。
fn cpu_card(
    buffer: &mut Buffer,
    inner: Rect,
    state: &State,
    sample: &SystemMetricsSnapshot,
    palette: &Palette,
) {
    let texts = &crate::i18n::texts().monitor;
    let value = percent(sample.cpu_percent);
    let detail = crate::i18n::fill(
        texts.logical_cpus_fmt,
        &[("n", &sample.cores.len().to_string())],
    );
    render_meter_row(
        buffer,
        Rect::new(inner.x, inner.y, inner.width, 1),
        &MeterRow {
            label: "CPU",
            value: &value,
            detail: Some(&detail),
            gauge: GaugeSpec {
                ratio: sample.cpu_percent.map(|value| value / 100.0),
                ascii: state.chart_glyphs.ascii(),
                ..GaugeSpec::default()
            },
            stale: sample.status == ObservationStatus::Stale,
        },
        palette,
    );
    match state.selected_core {
        Some(core) => text(
            buffer,
            inner,
            1,
            &format!("CPU {core} · {} min", state.monitor.history_minutes),
            Style::default().fg(palette.accent),
        ),
        None => text(
            buffer,
            inner,
            1,
            &sample.cpu_brand,
            Style::default().fg(palette.overlay0),
        ),
    }
    if inner.height > 2 {
        history_chart(
            buffer,
            Rect::new(inner.x, inner.y + 2, inner.width, inner.height - 2),
            state,
            |point| {
                state
                    .selected_core
                    .map_or(point.cpu, |core| point.cores.get(core).copied().flatten())
            },
            palette.teal,
        );
    }
}

/// 逐核卡：每核一条迷你 meter row，按列铺开；点击选中该核的历史曲线。滚动
/// 按整行步进（`offset` 是首个可见行，逐核步进会让多列时所有核整体错一列），
/// 返回滚动上界。
fn cores_card(
    buffer: &mut Buffer,
    inner: Rect,
    state: &State,
    sample: &SystemMetricsSnapshot,
    palette: &Palette,
    offset: usize,
    hits: &mut Vec<(Rect, Action)>,
) -> usize {
    // 每格至少 `CORE_SLOT_WIDTH` 列，放得下几格就排几格，再把富余均分给各格：
    // 条形铺满卡片宽度，不在右侧留一大片空白（整除后的零头 < 格数，留在最右）。
    let slots = (inner.width.saturating_add(1) / (CORE_SLOT_WIDTH + 1)).max(1);
    let stride = inner.width.saturating_add(1) / slots;
    let slot_width = if slots == 1 {
        inner.width
    } else {
        stride.saturating_sub(1)
    };
    let per_row = usize::from(slots);
    let max = sample
        .cores
        .len()
        .div_ceil(per_row)
        .saturating_sub(usize::from(inner.height));
    let offset = offset.min(max);
    let ascii = state.chart_glyphs.ascii();
    // 全卡统一列宽：标签按最大核号的位数、数字按「100%」预留，每格条形等长且
    // 起点对齐，滚动换屏时也不跳。
    let columns = MeterColumns {
        label: decimal_width(sample.cores.iter().map(|core| core.id).max().unwrap_or(0)),
        value: crate::ui::display_width_u16("100%"),
        detail: 0,
    };
    for (index, core) in sample
        .cores
        .iter()
        .skip(offset.saturating_mul(per_row))
        .take(usize::from(inner.height) * per_row)
        .enumerate()
    {
        let row = index as u16 / slots;
        let x = inner.x + (index as u16 % slots) * stride;
        let rect = Rect::new(
            x,
            inner.y + row,
            slot_width.min(inner.right().saturating_sub(x)),
            1,
        );
        let label = core.id.to_string();
        let value = core
            .usage_percent
            .map_or_else(|| "—".to_owned(), |value| format!("{value:.0}%"));
        render_meter_row_aligned(
            buffer,
            rect,
            &MeterRow {
                label: &label,
                value: &value,
                detail: None,
                gauge: GaugeSpec {
                    ratio: core.usage_percent.map(|value| value / 100.0),
                    ascii,
                    ..GaugeSpec::default()
                },
                stale: false,
            },
            columns,
            palette,
        );
        if state.selected_core == Some(core.id) {
            buffer.set_style(rect, Style::default().bg(palette.hover_row_bg()));
        }
        hits.push((rect, Action::Core(core.id)));
    }
    max
}

/// 内存卡：已用 / 缓存分段 gauge、Swap gauge、历史迷你图。
fn memory_card(
    buffer: &mut Buffer,
    inner: Rect,
    state: &State,
    sample: &SystemMetricsSnapshot,
    palette: &Palette,
) {
    let texts = &crate::i18n::texts().monitor;
    let memory = &sample.memory;
    let ascii = state.chart_glyphs.ascii();
    let stale = sample.status == ObservationStatus::Stale;
    let total = memory.total_bytes;
    let used_ratio = (total > 0).then(|| memory.used_bytes as f32 / total as f32);
    // 缓存 / 缓冲：总量里既不算已用也不可用的部分（平台不区分时为 0）。
    let cache = total
        .saturating_sub(memory.used_bytes)
        .saturating_sub(memory.available_bytes);
    let cache_ratio = (total > 0).then(|| cache as f32 / total as f32);
    let segments = [
        (
            used_ratio.unwrap_or(0.0),
            gauge_color(
                used_ratio.unwrap_or(0.0),
                None,
                GaugeThresholds::default(),
                palette,
            ),
        ),
        (cache_ratio.unwrap_or(0.0), palette.mauve),
    ];
    let value = format!("{} / {}", bytes(memory.used_bytes), bytes(total));
    let detail =
        (cache > 0).then(|| crate::i18n::fill(texts.cache_fmt, &[("size", &bytes(cache))]));
    let swap = format!(
        "{} / {}",
        bytes(memory.swap_used_bytes),
        bytes(memory.swap_total_bytes)
    );
    // 已用与 Swap 两行共用列宽：条形起止对齐（Swap 行也占住缓存说明那一列）。
    let mut columns = MeterColumns::default();
    columns.fit(texts.mem_used, &value, detail.as_deref());
    columns.fit(texts.swap, &swap, None);
    render_meter_row_aligned(
        buffer,
        Rect::new(inner.x, inner.y, inner.width, 1),
        &MeterRow {
            label: texts.mem_used,
            value: &value,
            detail: detail.as_deref(),
            gauge: GaugeSpec {
                ratio: used_ratio,
                segments: Some(&segments),
                ascii,
                ..GaugeSpec::default()
            },
            stale,
        },
        columns,
        palette,
    );
    if inner.height > 1 {
        render_meter_row_aligned(
            buffer,
            Rect::new(inner.x, inner.y + 1, inner.width, 1),
            &MeterRow {
                label: texts.swap,
                value: &swap,
                detail: None,
                gauge: GaugeSpec {
                    ratio: (memory.swap_total_bytes > 0)
                        .then(|| memory.swap_used_bytes as f32 / memory.swap_total_bytes as f32),
                    ascii,
                    ..GaugeSpec::default()
                },
                stale,
            },
            columns,
            palette,
        );
    }
    if inner.height > 2 {
        history_chart(
            buffer,
            Rect::new(inner.x, inner.y + 2, inner.width, inner.height - 2),
            state,
            |point| point.memory,
            palette.mauve,
        );
    }
}

/// GPU 卡实际会列出的设备：隐藏的不进卡。
fn listed_gpus<'a>(state: &State, sample: &'a SystemMetricsSnapshot) -> Vec<&'a GpuMetric> {
    sample
        .gpus
        .iter()
        .filter(|gpu| !state.monitor.hidden_devices.contains(&gpu.id))
        .collect()
}

/// GPU 卡：每块 GPU 三行——名称 + 温度、利用率 gauge、显存 gauge（无显存数据时
/// 换成驱动说明）。返回滚动上界。
fn gpu_card(
    buffer: &mut Buffer,
    inner: Rect,
    state: &State,
    sample: &SystemMetricsSnapshot,
    palette: &Palette,
    offset: usize,
) -> usize {
    let texts = &crate::i18n::texts().monitor;
    if sample.gpus.is_empty() {
        text(
            buffer,
            inner,
            0,
            tr("No GPU data available", "无可用 GPU 数据"),
            Style::default().fg(palette.overlay0),
        );
        return 0;
    }
    let gpus = listed_gpus(state, sample);
    // 上界按完整放得下的块数算（每块 3 行）：滚到底时最后一块的三行全露出，
    // 放得下时不可滚；隐藏设备或快照变动留下的越界存量 offset 也钳回来。
    let max = gpus
        .len()
        .saturating_sub(usize::from(inner.height / 3).max(1));
    let offset = offset.min(max);
    let ascii = state.chart_glyphs.ascii();
    // 利用率与显存两种行、所有 GPU 共用列宽：条形起止对齐，滚动换设备时也不跳。
    let vram_text = |gpu: &GpuMetric| match (gpu.memory_used_bytes, gpu.memory_total_bytes) {
        (Some(used), Some(total)) if total > 0 => {
            Some((format!("{} / {}", bytes(used), bytes(total)), used, total))
        }
        _ => None,
    };
    let mut columns = MeterColumns::default();
    for gpu in &gpus {
        columns.fit(texts.gpu_util, &percent(gpu.usage_percent), None);
        if let Some((value, ..)) = vram_text(gpu) {
            columns.fit(texts.vram, &value, None);
        }
    }
    for (index, gpu) in gpus
        .iter()
        .skip(offset)
        .take((inner.height as usize).div_ceil(3))
        .enumerate()
    {
        let row = index as u16 * 3;
        let stale = gpu.status == ObservationStatus::Stale;
        let temperature = gpu
            .temperature_celsius
            .map_or_else(|| "—".to_owned(), |value| format!("{value:.0}°C"));
        label_value_row(
            buffer,
            Rect::new(inner.x, inner.y + row, inner.width, 1),
            &gpu.name,
            Style::default()
                .fg(palette.text)
                .add_modifier(Modifier::BOLD),
            &temperature,
            Style::default().fg(palette.overlay1),
        );
        if row + 1 < inner.height {
            let value = percent(gpu.usage_percent);
            render_meter_row_aligned(
                buffer,
                Rect::new(inner.x, inner.y + row + 1, inner.width, 1),
                &MeterRow {
                    label: texts.gpu_util,
                    value: &value,
                    detail: None,
                    gauge: GaugeSpec {
                        ratio: gpu.usage_percent.map(|value| value / 100.0),
                        ascii,
                        ..GaugeSpec::default()
                    },
                    stale,
                },
                columns,
                palette,
            );
        }
        if row + 2 < inner.height {
            let rect = Rect::new(inner.x, inner.y + row + 2, inner.width, 1);
            match vram_text(gpu) {
                Some((value, used, total)) => {
                    render_meter_row_aligned(
                        buffer,
                        rect,
                        &MeterRow {
                            label: texts.vram,
                            value: &value,
                            detail: None,
                            gauge: GaugeSpec {
                                ratio: Some(used as f32 / total as f32),
                                ascii,
                                ..GaugeSpec::default()
                            },
                            stale,
                        },
                        columns,
                        palette,
                    );
                }
                _ => {
                    let note = gpu.message.as_deref().unwrap_or("—");
                    label_value_row(
                        buffer,
                        rect,
                        texts.vram,
                        Style::default().fg(palette.subtext0),
                        note,
                        Style::default().fg(palette.overlay0),
                    );
                }
            }
        }
    }
    max
}

/// 磁盘卡实际会列出的盘：没有容量的伪文件系统不进表，同一设备挂在多处只列
/// 一次（服务端已按容量去重，这里再按设备名兜底），隐藏的设备不进表。
fn listed_disks<'a>(state: &State, sample: &'a SystemMetricsSnapshot) -> Vec<&'a DiskMetric> {
    let mut disks: Vec<&DiskMetric> = Vec::new();
    for disk in &sample.disks {
        if disk.total_bytes == 0 || state.monitor.hidden_devices.contains(&disk.id) {
            continue;
        }
        if !disk.name.is_empty() && disks.iter().any(|seen| seen.name == disk.name) {
            continue;
        }
        disks.push(disk);
    }
    disks
}

/// 磁盘卡：表格列出真实数据盘——没有容量的伪文件系统不进表，同一设备挂在多处
/// 只列一次（服务端已按容量去重，这里再按设备名兜底）。返回滚动上界。
fn disks_card(
    buffer: &mut Buffer,
    inner: Rect,
    state: &State,
    sample: &SystemMetricsSnapshot,
    palette: &Palette,
    offset: usize,
) -> usize {
    let texts = &crate::i18n::texts().monitor;
    let disks = listed_disks(state, sample);
    if disks.is_empty() {
        text(
            buffer,
            inner,
            0,
            texts.no_disks,
            Style::default().fg(palette.overlay0),
        );
        return 0;
    }
    let columns = [
        Column {
            title: texts.col_mount,
            width: ColumnWidth::Fill(2),
            align_right: false,
            sortable: false,
            priority: 0,
        },
        Column {
            title: texts.col_device,
            width: ColumnWidth::Fill(1),
            align_right: false,
            sortable: false,
            priority: 3,
        },
        Column {
            title: texts.col_used,
            width: ColumnWidth::Fixed(9),
            align_right: true,
            sortable: false,
            priority: 2,
        },
        Column {
            title: texts.col_total,
            width: ColumnWidth::Fixed(9),
            align_right: true,
            sortable: false,
            priority: 2,
        },
        Column {
            title: texts.col_percent,
            width: ColumnWidth::Fixed(6),
            align_right: true,
            sortable: false,
            priority: 1,
        },
    ];
    let table = TableState {
        sort: None,
        row_count: disks.len(),
        scroll: offset,
        selected: None,
        hovered: None,
        ascii: state.chart_glyphs.ascii(),
    };
    let render = render_table(
        buffer,
        inner,
        &columns,
        &table,
        |row, column| {
            let Some(disk) = disks.get(row) else {
                return TableCell::from("");
            };
            let used = disk.total_bytes.saturating_sub(disk.available_bytes);
            let ratio = used as f32 / disk.total_bytes as f32;
            match column {
                0 => TableCell::from(disk.mount_point.as_str()),
                1 => TableCell::from(disk.name.as_str()),
                2 => TableCell::from(bytes(used)),
                3 => TableCell::from(bytes(disk.total_bytes)),
                _ => TableCell {
                    text: Cow::Owned(format!("{:.0}%", ratio * 100.0)),
                    fg: load_color(ratio, palette),
                },
            }
        },
        palette,
    );
    table_scroll_max(disks.len(), &render)
}

/// 网络接口当前的 (收, 发) 速率；两个方向都未知（首次差分）时为 `None`。
fn net_rates(net: &NetworkMetric) -> Option<(f32, f32)> {
    match (
        net.received_bytes_per_second,
        net.transmitted_bytes_per_second,
    ) {
        (None, None) => None,
        (rx, tx) => Some((
            rx.unwrap_or(0.0).max(0.0) as f32,
            tx.unwrap_or(0.0).max(0.0) as f32,
        )),
    }
}

/// 接口是否空闲：当前与最近的样本里都没有超过 1 B/s 的流量。
fn net_idle(state: &State, net: &NetworkMetric) -> bool {
    let quiet = |(rx, tx): (f32, f32)| rx < 1.0 && tx < 1.0;
    net_rates(net).is_none_or(quiet)
        && state
            .net_history
            .get(&net.id)
            .is_none_or(|samples| samples.iter().all(|sample| quiet(*sample)))
}

/// 速率文本 `↓1.2 MiB/s ↑24.0 KiB/s`。
fn net_rates_text(net: &NetworkMetric) -> String {
    let rate = |value: Option<f64>| {
        value.map_or_else(|| "—".to_owned(), |value| bytes(value.max(0.0) as u64))
    };
    format!(
        "↓{}/s ↑{}/s",
        rate(net.received_bytes_per_second),
        rate(net.transmitted_bytes_per_second)
    )
}

/// 网络卡实际会画出的接口与被折叠的空闲接口数：隐藏的不进卡，空闲的折成末行
/// 的一句说明。
fn listed_networks<'a>(
    state: &State,
    sample: &'a SystemMetricsSnapshot,
) -> (Vec<&'a NetworkMetric>, usize) {
    let mut active = Vec::new();
    let mut idle = 0;
    for net in &sample.networks {
        if state.monitor.hidden_devices.contains(&net.id) {
            continue;
        }
        if net_idle(state, net) {
            idle += 1;
        } else {
            active.push(net);
        }
    }
    (active, idle)
}

/// 网络卡：每个活动接口两行——名称 + 速率、收 / 发堆叠迷你图；空闲接口折成
/// 末行的一句说明。返回滚动上界。
fn network_card(
    buffer: &mut Buffer,
    inner: Rect,
    state: &State,
    sample: &SystemMetricsSnapshot,
    palette: &Palette,
    offset: usize,
) -> usize {
    let texts = &crate::i18n::texts().monitor;
    let (active, idle) = listed_networks(state, sample);
    let bottom = inner.bottom() - u16::from(idle > 0 && inner.height > 1);
    // 末行留给空闲接口说明，其余每个活动接口占 2 行：上界按完整放得下的接口数
    // 算，滚到底时最后一个接口的两行都露出，放得下时不可滚。
    let capacity = usize::from(bottom.saturating_sub(inner.y) / 2).max(1);
    let max = active.len().saturating_sub(capacity);
    let offset = offset.min(max);
    let glyphs = state.chart_glyphs.chart();
    let mut y = inner.y;
    for net in active.iter().skip(offset) {
        if y >= bottom {
            break;
        }
        label_value_row(
            buffer,
            Rect::new(inner.x, y, inner.width, 1),
            &net.id,
            Style::default().fg(palette.subtext0),
            &net_rates_text(net),
            Style::default()
                .fg(palette.text)
                .add_modifier(Modifier::BOLD),
        );
        y += 1;
        if y < bottom {
            if let Some(samples) = state.net_history.get(&net.id) {
                let mut rx = [0f32; NET_HISTORY_SAMPLES];
                let mut tx = [0f32; NET_HISTORY_SAMPLES];
                let mut count = 0;
                for (rx_sample, tx_sample) in samples.iter().rev().take(NET_HISTORY_SAMPLES) {
                    count += 1;
                    rx[NET_HISTORY_SAMPLES - count] = *rx_sample;
                    tx[NET_HISTORY_SAMPLES - count] = *tx_sample;
                }
                let start = NET_HISTORY_SAMPLES - count;
                render_area_chart(
                    buffer,
                    Rect::new(inner.x, y, inner.width, 1),
                    &[&rx[start..], &tx[start..]],
                    None,
                    glyphs,
                    &[palette.teal, palette.blue],
                );
            }
            y += 1;
        }
    }
    if idle > 0 && inner.height > 0 {
        let note = if idle == 1 {
            texts.net_idle_one.to_owned()
        } else {
            crate::i18n::fill(texts.net_idle_folded_fmt, &[("n", &idle.to_string())])
        };
        text(
            buffer,
            Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
            0,
            &note,
            Style::default().fg(palette.overlay0),
        );
    }
    max
}

/// 一颗芯片（传感器名的首个词）的温度汇总。
struct Chip<'a> {
    name: &'a str,
    max: f32,
    sum: f32,
    count: u32,
    critical: Option<f32>,
}

/// 温度卡实际会画出的行：传感器名的首个词相同的归为一颗芯片，隐藏的传感器
/// 不参与汇总。
fn sensor_chips<'a>(state: &State, sample: &'a SystemMetricsSnapshot) -> Vec<Chip<'a>> {
    let hidden = |name: &str| {
        state
            .monitor
            .hidden_devices
            .iter()
            .any(|hidden| hidden.strip_prefix("sensor:") == Some(name))
    };
    let mut chips: Vec<Chip<'_>> = Vec::new();
    for sensor in sample.sensors.iter().filter(|sensor| !hidden(&sensor.name)) {
        let name = sensor
            .name
            .split_whitespace()
            .next()
            .unwrap_or(sensor.name.as_str());
        let temperature = sensor.temperature_celsius.filter(|value| value.is_finite());
        let critical = sensor.critical_celsius.filter(|value| *value > 0.0);
        match chips.iter_mut().find(|chip| chip.name == name) {
            Some(chip) => {
                if let Some(temperature) = temperature {
                    chip.max = chip.max.max(temperature);
                    chip.sum += temperature;
                    chip.count += 1;
                }
                chip.critical = match (chip.critical, critical) {
                    (Some(a), Some(b)) => Some(a.max(b)),
                    (a, b) => a.or(b),
                };
            }
            None => chips.push(Chip {
                name,
                max: temperature.unwrap_or(0.0),
                sum: temperature.unwrap_or(0.0),
                count: u32::from(temperature.is_some()),
                critical,
            }),
        }
    }
    chips
}

/// 温度卡：传感器按芯片汇总成一行（最高 / 平均），gauge 以临界温度（未知时
/// 100 °C）定标。返回滚动上界。
fn sensors_card(
    buffer: &mut Buffer,
    inner: Rect,
    state: &State,
    sample: &SystemMetricsSnapshot,
    palette: &Palette,
    offset: usize,
) -> usize {
    let texts = &crate::i18n::texts().monitor;
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
        return 0;
    }
    let chips = sensor_chips(state, sample);
    // 每颗芯片一行：上界是芯片数 − 内高，放得下时不可滚，滚到底时最后一颗
    // 落在末行（汇总后的行数远少于传感器数，不能按传感器数算）。
    let max = chips.len().saturating_sub(usize::from(inner.height));
    let offset = offset.min(max);
    let ascii = state.chart_glyphs.ascii();
    // 数字是芯片里最热的传感器（条形也按它定标），多个传感器时平均温度作说明，
    // 窄时说明先让位、条形与数字保住。全部芯片（含滚出视口的）先格式化并量出
    // 统一列宽：各行条形起止对齐，滚动时也不跳。芯片数很少，这里分配不在 pane
    // 规模路径上。
    let values = chips
        .iter()
        .map(|chip| {
            let value = if chip.count == 0 {
                "—".to_owned()
            } else {
                format!("{:.0}°C", chip.max)
            };
            let detail = (chip.count > 1).then(|| {
                crate::i18n::fill(
                    texts.temp_avg_fmt,
                    &[("avg", &format!("{:.0}°C", chip.sum / chip.count as f32))],
                )
            });
            (value, detail)
        })
        .collect::<Vec<_>>();
    let mut columns = MeterColumns::default();
    for (chip, (value, detail)) in chips.iter().zip(&values) {
        columns.fit(chip.name, value, detail.as_deref());
    }
    for (index, (chip, (value, detail))) in chips
        .iter()
        .zip(&values)
        .skip(offset)
        .take(usize::from(inner.height))
        .enumerate()
    {
        let limit = chip.critical.unwrap_or(100.0);
        render_meter_row_aligned(
            buffer,
            Rect::new(inner.x, inner.y + index as u16, inner.width, 1),
            &MeterRow {
                label: chip.name,
                value,
                detail: detail.as_deref(),
                gauge: GaugeSpec {
                    ratio: (chip.count > 0).then(|| chip.max / limit),
                    ascii,
                    ..GaugeSpec::default()
                },
                stale: false,
            },
            columns,
            palette,
        );
    }
    // 芯片都放得下、卡片还空着至少两行时，与 CPU / 内存卡同一口径画历史：一行
    // 说明 + 其余行画最高温度的迷你图（0–100 °C 定标），卡片不再留一大片空白。
    let used = u16::try_from(chips.len()).unwrap_or(u16::MAX);
    if max == 0 && inner.height >= used.saturating_add(2) {
        let caption = inner.y + used;
        text(
            buffer,
            Rect::new(inner.x, caption, inner.width, 1),
            0,
            &format!(
                "{} · {} min",
                tr("Hottest sensor", "最高温度"),
                state.monitor.history_minutes
            ),
            Style::default().fg(palette.overlay0),
        );
        history_chart(
            buffer,
            Rect::new(
                inner.x,
                caption + 1,
                inner.width,
                inner.bottom() - caption - 1,
            ),
            state,
            |point| point.temperature,
            palette.peach,
        );
    }
    max
}

/// 进程表的列顺序与 `ProcessSort` 的对应关系。
const PROCESS_COLUMNS: [ProcessSort; 4] = [
    ProcessSort::Pid,
    ProcessSort::Name,
    ProcessSort::Cpu,
    ProcessSort::Memory,
];

/// 进程表实际会列出的进程：按筛选词过滤（排序不改变条数，排序在卡片里做）。
fn listed_processes<'a>(
    state: &State,
    sample: &'a SystemMetricsSnapshot,
) -> Vec<&'a ProcessMetric> {
    let filter = state.process_filter.to_lowercase();
    sample
        .processes
        .iter()
        .filter(|process| process.name.to_lowercase().contains(&filter))
        .collect()
}

/// 进程卡：首行是筛选入口 + 当前筛选词 + 可见范围，其下是带表头的可排序表格
/// （点击表头按列排序，点击行打开详情）。返回滚动上界。
fn processes_card(
    buffer: &mut Buffer,
    inner: Rect,
    state: &State,
    sample: &SystemMetricsSnapshot,
    palette: &Palette,
    offset: usize,
    hits: &mut Vec<(Rect, Action)>,
) -> usize {
    let texts = &crate::i18n::texts().monitor;
    let mut processes = listed_processes(state, sample);
    processes.sort_by(|a, b| match state.process_sort {
        ProcessSort::Memory => b.memory_bytes.cmp(&a.memory_bytes),
        ProcessSort::Name => a.name.cmp(&b.name),
        ProcessSort::Pid => a.identity.pid.cmp(&b.identity.pid),
        ProcessSort::Cpu => b
            .cpu_percent
            .unwrap_or(-1.0)
            .total_cmp(&a.cpu_percent.unwrap_or(-1.0)),
    });
    let filter_label = tr("Filter /", "筛选 /");
    secondary_button(
        buffer,
        Rect::new(inner.x, inner.y, inner.width, 1),
        filter_label,
        Action::FilterProcesses,
        palette,
        hits,
    );
    let filter_width = crate::ui::modal_button_width(&format!(" {filter_label} "));
    if inner.height < 2 {
        return 0;
    }
    // PID 列按快照里最长的 PID 定宽（至少 6 列，放得下排序标记 + 表头）：Linux
    // 的 PID 可达 7 位，截成「40905…」后几行同名进程无法区分；按全表而不是
    // 可见行取宽，滚动时列宽不跳。
    let pid_width = processes
        .iter()
        .map(|process| decimal_width(usize::try_from(process.identity.pid).unwrap_or(usize::MAX)))
        .max()
        .unwrap_or(0)
        .max(6);
    let columns = [
        Column {
            title: texts.proc_col_pid,
            width: ColumnWidth::Min(pid_width),
            align_right: true,
            sortable: true,
            priority: 2,
        },
        Column {
            title: texts.proc_col_name,
            width: ColumnWidth::Fill(1),
            align_right: false,
            sortable: true,
            priority: 0,
        },
        Column {
            title: texts.proc_col_cpu,
            width: ColumnWidth::Fixed(6),
            align_right: true,
            sortable: true,
            priority: 1,
        },
        Column {
            title: texts.proc_col_mem,
            width: ColumnWidth::Fixed(9),
            align_right: true,
            sortable: true,
            priority: 1,
        },
    ];
    let sort = SortState {
        column: PROCESS_COLUMNS
            .iter()
            .position(|sort| *sort == state.process_sort)
            .unwrap_or(2),
        descending: matches!(state.process_sort, ProcessSort::Cpu | ProcessSort::Memory),
    };
    let table = TableState {
        sort: Some(sort),
        row_count: processes.len(),
        scroll: offset,
        selected: None,
        hovered: None,
        ascii: state.chart_glyphs.ascii(),
    };
    let render = render_table(
        buffer,
        Rect::new(inner.x, inner.y + 1, inner.width, inner.height - 1),
        &columns,
        &table,
        |row, column| {
            let Some(process) = processes.get(row) else {
                return TableCell::from("");
            };
            match column {
                0 => TableCell::from(process.identity.pid.to_string()),
                1 => TableCell::from(process.name.as_str()),
                2 => TableCell {
                    text: Cow::Owned(percent(process.cpu_percent)),
                    fg: process
                        .cpu_percent
                        .and_then(|value| load_color(value / 100.0, palette)),
                },
                _ => TableCell::from(bytes(process.memory_bytes)),
            }
        },
        palette,
    );
    for (rect, column) in &render.header_cells {
        if let Some(sort) = PROCESS_COLUMNS.get(*column) {
            hits.push((*rect, Action::SortProcesses(*sort)));
        }
    }
    for (rect, row) in &render.rows {
        if let Some(process) = processes.get(*row) {
            hits.push((*rect, Action::Process(process.identity.clone())));
        }
    }
    // 首行右侧：筛选词（输入中带光标）与可见范围 `起–止 / 总数`。
    let shown = render.rows.len();
    let range = if shown == 0 {
        format!("0 / {}", processes.len())
    } else {
        format!(
            "{}–{} / {}",
            render.scroll + 1,
            render.scroll + shown,
            processes.len()
        )
    };
    let filter_text = match (state.process_filter.is_empty(), state.filtering_processes) {
        (true, false) => String::new(),
        (_, typing) => format!("{}{}", state.process_filter, if typing { "▏" } else { "" }),
    };
    let rest = Rect::new(
        inner.x + filter_width + 1,
        inner.y,
        inner.width.saturating_sub(filter_width + 1),
        1,
    );
    label_value_row(
        buffer,
        rest,
        &filter_text,
        Style::default().fg(palette.text),
        &range,
        Style::default().fg(palette.overlay0),
    );
    if render.rows.is_empty() && !render.body.is_empty() {
        text(
            buffer,
            render.body,
            0,
            texts.no_matching_processes,
            Style::default().fg(palette.overlay0),
        );
    }
    table_scroll_max(processes.len(), &render)
}

/// 进程详情对话框：居中覆盖在页面与悬浮层之上，返回其矩形；`hits` 只含
/// 对话框自己的按钮（调用方已清空页面与悬浮层的命中区）。
pub(super) fn process_dialog(
    buffer: &mut Buffer,
    dialog: &ProcessDialog,
    state: &State,
    cx: &ChromeContext<'_>,
    hits: &mut Vec<(Rect, Action)>,
) -> Rect {
    let palette = cx.palette;
    let rect = crate::ui::centered_popup_rect(buffer.area, 66, 12).unwrap_or(buffer.area);
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
    let y = inner.bottom().saturating_sub(1);
    secondary_button(
        buffer,
        Rect::new(inner.x, y, 16.min(inner.width), 1),
        tr("Cancel", "取消"),
        Action::CancelProcess,
        palette,
        hits,
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
            hits,
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
            hits,
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
            hits,
        );
    }
    rect
}

/// 系统页卡片标题；设置页的显示项复用同一份文案（ACC-12）。
pub(super) fn section_title(id: &str) -> &'static str {
    match id {
        "cpu" => "CPU",
        "cores" => tr("CPU CORES", "CPU 逐核"),
        "memory" => tr("MEMORY", "内存"),
        "gpu" => "GPU",
        "disks" => tr("DISKS", "磁盘"),
        "network" => tr("NETWORK", "网络"),
        "sensors" => tr("TEMPERATURE", "温度"),
        _ => tr("PROCESSES", "进程"),
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;
    use crate::i18n::{lang_guard, Lang};

    const GIB: u64 = 1024 * 1024 * 1024;

    fn core(id: usize, usage: f32) -> CpuCoreMetric {
        CpuCoreMetric {
            id,
            name: format!("cpu{id}"),
            usage_percent: Some(usage),
            frequency_mhz: None,
        }
    }

    fn disk(name: &str, mount_point: &str, total_bytes: u64, available_bytes: u64) -> DiskMetric {
        DiskMetric {
            id: format!("{name}:{mount_point}"),
            name: name.into(),
            mount_point: mount_point.into(),
            total_bytes,
            available_bytes,
            ..Default::default()
        }
    }

    fn network(id: &str, rx: Option<f64>, tx: Option<f64>) -> NetworkMetric {
        NetworkMetric {
            id: id.into(),
            received_bytes_per_second: rx,
            transmitted_bytes_per_second: tx,
            ..Default::default()
        }
    }

    fn sensor(name: &str, temperature: f32) -> SensorMetric {
        SensorMetric {
            name: name.into(),
            temperature_celsius: Some(temperature),
            critical_celsius: None,
        }
    }

    fn process(pid: u32, name: &str, cpu: f32, memory_bytes: u64) -> ProcessMetric {
        ProcessMetric {
            identity: ProcessIdentity {
                pid,
                ..Default::default()
            },
            name: name.into(),
            cpu_percent: Some(cpu),
            memory_bytes,
            ..Default::default()
        }
    }

    /// 一台四核、一块 GPU、两块真实数据盘（外加一处重复挂载与一个 tmpfs）、
    /// 三个接口（两个空闲）、两颗温度芯片、三个进程的主机快照。
    fn sample() -> SystemMetricsSnapshot {
        SystemMetricsSnapshot {
            boot_id: "boot".into(),
            sequence: 1,
            sampled_at_ms: 10_000,
            interval_ms: 1000,
            hostname: "devbox".into(),
            operating_system: "Ubuntu 22.04".into(),
            environment: "native".into(),
            uptime_seconds: 3_700,
            status: ObservationStatus::Ready,
            cpu_percent: Some(42.0),
            cpu_brand: "Test CPU".into(),
            physical_core_count: Some(2),
            cores: vec![core(0, 10.0), core(1, 95.0), core(2, 50.0), core(3, 0.0)],
            memory: MemoryMetric {
                total_bytes: 16 * GIB,
                used_bytes: 6 * GIB,
                available_bytes: 8 * GIB,
                swap_total_bytes: 2 * GIB,
                swap_used_bytes: 0,
            },
            gpus: vec![GpuMetric {
                id: "gpu0".into(),
                name: "Test GPU".into(),
                status: ObservationStatus::Ready,
                usage_percent: Some(30.0),
                memory_used_bytes: Some(2 * GIB),
                memory_total_bytes: Some(8 * GIB),
                temperature_celsius: Some(65.0),
                ..Default::default()
            }],
            disks: vec![
                disk("/dev/sda1", "/", 500 * GIB, 100 * GIB),
                disk("/dev/sda1", "/home", 500 * GIB, 100 * GIB),
                disk("tmpfs", "/run", 0, 0),
                disk("/dev/nvme0n1p1", "/data", 1024 * GIB, 900 * GIB),
            ],
            networks: vec![
                network("eth0", Some(1_048_576.0), Some(2048.0)),
                network("lo", Some(0.0), Some(0.0)),
                network("wlan0", None, None),
            ],
            sensors: vec![
                sensor("coretemp Core 0", 60.0),
                sensor("coretemp Core 1", 70.0),
                sensor("nvme Composite", 40.0),
            ],
            processes: vec![
                process(10, "herdr", 5.0, 100 * 1024 * 1024),
                process(2, "zsh", 0.5, 8 * 1024 * 1024),
                process(7, "cargo", 92.0, 900 * 1024 * 1024),
            ],
            ..Default::default()
        }
    }

    fn monitored() -> State {
        let mut state = State::new(&config());
        state.now_ms = 14_000;
        state.metrics = Some(Box::new(sample()));
        state.monitor.card_height = 10;
        state
    }

    fn card_rect(output: &PaintOutput, id: &str) -> Rect {
        hit_rect(
            output,
            |action| matches!(action, Action::Card(card) if card == id),
        )
        .unwrap_or_else(|| panic!("{id} 卡片有命中区"))
    }

    fn is_braille(ch: char) -> bool {
        ('\u{2800}'..='\u{28FF}').contains(&ch)
    }

    fn is_block(ch: char) -> bool {
        "▁▂▃▄▅▆▇█".contains(ch)
    }

    /// 硬性版式约束：额度 / 指标数字在任意宽度可见——窄时先砍条形，数字不裁。
    #[test]
    fn system_cards_keep_numbers_visible_at_every_width() {
        let _guard = lang_guard(Lang::ZhCn);
        let state = monitored();
        for width in [120_u16, 60, 30, 14] {
            let (buffer, output) = paint_page(&state, Page::Monitor, width, 100);
            let text = buffer_text(&buffer);
            assert!(buffer_has(&buffer, "42.0%"), "宽 {width}: CPU 数字\n{text}");
            // 极窄时摘要条让位给「编辑布局」入口，主机名可以省略。
            assert!(
                width < 30 || buffer_has(&buffer, "devbox"),
                "宽 {width}: 主机名\n{text}"
            );
            for id in [
                "cpu",
                "cores",
                "memory",
                "gpu",
                "disks",
                "network",
                "sensors",
                "processes",
            ] {
                assert!(
                    has(
                        &output,
                        |action| matches!(action, Action::Card(card) if card == id)
                    ),
                    "宽 {width}: {id} 卡片有命中区"
                );
            }
            for (rect, action) in &output.hits {
                assert!(
                    contains_rect(Rect::new(0, 0, width, 100), *rect),
                    "宽 {width}: 命中区 {rect:?}（{action:?}）越界"
                );
            }
        }
        // 宽：条形与数字都在。
        let (wide, output) = paint_page(&state, Page::Monitor, 120, 100);
        let cpu = card_rect(&output, "cpu");
        assert!(
            row_has(&wide, cpu.y + 1, "━"),
            "{}",
            row_text(&wide, cpu.y + 1)
        );
        assert!(
            row_has(&wide, cpu.y + 1, "4 逻辑核"),
            "{}",
            row_text(&wide, cpu.y + 1)
        );
        assert!(buffer_has(&wide, "6.0 GiB / 16.0 GiB"));
        // 极窄：CPU 行只剩标签与数字，条形整段让位。
        let (tiny, output) = paint_page(&state, Page::Monitor, 14, 100);
        let cpu = card_rect(&output, "cpu");
        let row = row_text(&tiny, cpu.y + 1);
        assert!(row.contains("42.0%"), "{row}");
        assert!(!row.contains('━') && !row.contains('░'), "{row}");
    }

    #[test]
    fn cards_use_two_columns_only_from_96_columns() {
        let state = monitored();
        let columns = |width: u16| {
            let (_, output) = paint_page(&state, Page::Monitor, width, 100);
            output
                .hits
                .iter()
                .filter(|(_, action)| matches!(action, Action::Card(_)))
                .map(|(rect, _)| rect.x)
                .collect::<std::collections::BTreeSet<_>>()
                .len()
        };
        assert_eq!(columns(60), 1);
        assert_eq!(columns(95), 1, "< 96 列单列");
        assert_eq!(columns(96), 2, "≥ 96 列双列");
    }

    /// 双列时页面按整行卡片滚动：卡片不在两列之间换位；越界的存量滚动位置按
    /// 「最后一行完整露出」的上界钳回，不留空屏。
    #[test]
    fn two_column_grid_scrolls_by_whole_card_rows() {
        let mut state = monitored();
        let order = state.monitor.visible.clone();
        let cards = |output: &PaintOutput| {
            let mut cards = output
                .hits
                .iter()
                .filter_map(|(rect, action)| match action {
                    Action::Card(id) => Some((rect.y, rect.x, id.clone())),
                    _ => None,
                })
                .collect::<Vec<_>>();
            cards.sort();
            cards
        };
        // 120×40：卡片区 33 行放得下 3 行卡片，4 行卡片的上界是 1。
        let (_, output) = paint_page(&state, Page::Monitor, 120, 40);
        assert_eq!(
            output.page_scroll_limits.get(Page::Monitor),
            Some(PageScroll { max: 1, screen: 3 })
        );
        let top = cards(&output);
        state.scroll = 1;
        let (_, output) = paint_page(&state, Page::Monitor, 120, 40);
        let scrolled = cards(&output);
        assert_eq!(
            scrolled.iter().map(|(.., id)| id).collect::<Vec<_>>(),
            order[2..].iter().collect::<Vec<_>>(),
            "滚一格换一整行"
        );
        // 内存卡原本在第二行左列，滚一行后到第一行左列：列不变。
        let column_of = |cards: &[(u16, u16, String)], id: &str| {
            cards
                .iter()
                .find(|(.., card)| card == id)
                .map(|(_, x, _)| *x)
        };
        for id in &order[2..6] {
            let before = column_of(&top, id);
            assert!(before.is_some(), "{id} 滚动前可见");
            assert_eq!(before, column_of(&scrolled, id), "{id} 不换列");
        }
        state.scroll = 9;
        let (_, output) = paint_page(&state, Page::Monitor, 120, 40);
        assert_eq!(cards(&output), scrolled, "越界存量钳到上界");
    }

    /// 页面底边只露出一截的卡片按完整卡高排版、裁在视口边缘：露出部分没有下
    /// 边框，卡内滚动上界与它完整露出时相同（三个接口放得下，上界 0，滚轮交给
    /// 页面），命中区不越过可见部分。压扁成 5 行的矮卡只放得下一个接口，上界会
    /// 变成 2、滚轮被一张看不全的卡吃掉。
    #[test]
    fn a_card_cut_by_the_page_bottom_is_laid_out_at_full_height() {
        let _guard = lang_guard(Lang::ZhCn);
        let mut state = monitored();
        assert_eq!(
            state.monitor.visible[5], "network",
            "用例前提：默认卡片顺序"
        );
        if let Some(sample) = state.metrics.as_mut() {
            sample.networks = (0..3)
                .map(|id| network(&format!("eth{id}"), Some(1_048_576.0), Some(2048.0)))
                .collect();
        }
        // 60×32：单列、卡高 10，两行卡片之下露出第三张卡的 5 行；滚 3 行后
        // GPU / 磁盘完整，网络卡露出一截。
        state.scroll = 3;
        let (buffer, output) = paint_page(&state, Page::Monitor, 60, 32);
        let text = buffer_text(&buffer);
        let network = card_rect(&output, "network");
        assert_eq!(network.height, 5, "{text}");
        assert_eq!(network.bottom(), 31, "露出部分贴着页脚\n{text}");
        assert_eq!(buffer[(network.x, network.y)].symbol(), "┌", "{text}");
        for y in network.y + 1..network.bottom() {
            assert_eq!(
                buffer[(network.x, y)].symbol(),
                "│",
                "第 {y} 行：裁在视口边缘、没有下边框\n{text}"
            );
        }
        assert!(row_has(&buffer, network.y + 1, "eth0"), "{text}");
        assert!(row_has(&buffer, network.y + 3, "eth1"), "{text}");
        assert_eq!(
            output.card_scroll_limits.get("network"),
            Some(0),
            "按完整卡高算：三个接口放得下"
        );
        // 页脚自己的命中区从页脚行开始；页面正文里的命中区都不越过页脚。
        for (rect, action) in output.hits.iter().filter(|(rect, _)| rect.y < 31) {
            assert!(
                rect.bottom() <= network.bottom(),
                "命中区 {rect:?}（{action:?}）越过可见部分"
            );
        }
        // 滚一行后完整露出：同一个上界。
        state.scroll = 4;
        let (_, output) = paint_page(&state, Page::Monitor, 60, 32);
        assert_eq!(card_rect(&output, "network").height, 10);
        assert_eq!(output.card_scroll_limits.get("network"), Some(0));
    }

    #[test]
    fn summary_bar_lists_host_facts_and_the_sampler_status() {
        let _guard = lang_guard(Lang::ZhCn);
        let mut state = monitored();
        let (buffer, output) = paint_page(&state, Page::Monitor, 120, 40);
        assert!(
            buffer_has(
                &buffer,
                "devbox · Ubuntu 22.04 · 已运行 1h01m · 4s 前更新 · 每 1000 ms"
            ),
            "{}",
            buffer_text(&buffer)
        );
        assert!(has(&output, |action| matches!(action, Action::EditLayout)));
        assert!(buffer_has(&buffer, "编辑布局"));
        if let Some(metrics) = state.metrics.as_mut() {
            metrics.status = ObservationStatus::Warming;
        }
        let (buffer, _) = paint_page(&state, Page::Monitor, 120, 40);
        assert!(
            buffer_has(&buffer, "· 采集中 ·"),
            "{}",
            buffer_text(&buffer)
        );
    }

    #[test]
    fn cpu_memory_and_gpu_cards_show_gauges_with_their_numbers() {
        let _guard = lang_guard(Lang::ZhCn);
        let mut state = monitored();
        state.selected_core = Some(1);
        let (buffer, output) = paint_page(&state, Page::Monitor, 120, 100);
        let text = buffer_text(&buffer);
        // 逐核：每核一条迷你条，选中核有命中区与高亮底。
        assert!(has(&output, |action| matches!(action, Action::Core(1))));
        assert!(buffer_has(&buffer, "95%"), "{text}");
        let core = hit_rect(&output, |action| matches!(action, Action::Core(1))).expect("核 1");
        assert_eq!(
            buffer[(core.x, core.y)].style().bg,
            Some(config().palette.hover_row_bg()),
            "选中核高亮"
        );
        let cpu = card_rect(&output, "cpu");
        assert!(
            row_has(&buffer, cpu.y + 2, "CPU 1 · 15 min"),
            "{}",
            row_text(&buffer, cpu.y + 2)
        );
        // 内存：已用 / 缓存分段，Swap 单独一行。
        assert!(buffer_has(&buffer, "缓存 2.0 GiB"), "{text}");
        assert!(buffer_has(&buffer, "Swap"), "{text}");
        assert!(buffer_has(&buffer, "0 B / 2.0 GiB"), "{text}");
        let memory = card_rect(&output, "memory");
        let palette = config().palette;
        let row = memory.y + 1;
        let used = (memory.x..memory.right())
            .find(|x| buffer[(*x, row)].symbol() == "━")
            .expect("已用段");
        assert_eq!(buffer[(used, row)].style().fg, Some(palette.teal));
        let cache = (used..memory.right())
            .find(|x| buffer[(*x, row)].style().fg == Some(palette.mauve))
            .expect("缓存段");
        assert_eq!(buffer[(cache, row)].symbol(), "━");
        // GPU：名称 + 温度、利用率、显存。
        assert!(buffer_has(&buffer, "Test GPU"), "{text}");
        assert!(buffer_has(&buffer, "65°C"), "{text}");
        assert!(buffer_has(&buffer, "GPU 利用率"), "{text}");
        assert!(buffer_has(&buffer, "30.0%"), "{text}");
        assert!(buffer_has(&buffer, "显存"), "{text}");
        assert!(buffer_has(&buffer, "2.0 GiB / 8.0 GiB"), "{text}");
    }

    #[test]
    fn disks_table_filters_pseudo_filesystems_and_duplicate_devices() {
        let _guard = lang_guard(Lang::ZhCn);
        let state = monitored();
        let (buffer, output) = paint_page(&state, Page::Monitor, 120, 100);
        let disks = card_rect(&output, "disks");
        let rows = (disks.y..disks.bottom())
            .map(|y| row_text(&buffer, y))
            .collect::<Vec<_>>();
        let joined = rows.join("\n");
        assert!(row_has(&buffer, disks.y + 1, "挂载点"), "{joined}");
        assert!(row_has(&buffer, disks.y + 1, "使用率"), "{joined}");
        assert_eq!(
            rows.iter().filter(|row| row.contains("/dev/sda1")).count(),
            1,
            "同一设备只列一次\n{joined}"
        );
        assert!(!joined.contains("/home"), "{joined}");
        assert!(!joined.contains("/run"), "tmpfs 不进表\n{joined}");
        assert!(joined.contains("/data"), "{joined}");
        let y = (disks.y..disks.bottom())
            .find(|y| row_has(&buffer, *y, "/dev/sda1"))
            .expect("根分区行");
        assert!(row_has(&buffer, y, "80%"), "{}", row_text(&buffer, y));
        assert_eq!(color_at(&buffer, y, "80%"), Some(config().palette.yellow));
    }

    #[test]
    fn network_card_folds_idle_interfaces_and_stacks_rx_tx() {
        let _guard = lang_guard(Lang::ZhCn);
        let mut state = monitored();
        state.net_history.insert(
            "eth0".into(),
            std::iter::repeat_n((1000.0, 500.0), 8).collect(),
        );
        let (buffer, output) = paint_page(&state, Page::Monitor, 120, 100);
        let network = card_rect(&output, "network");
        let text = (network.y..network.bottom())
            .map(|y| row_text(&buffer, y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(row_has(&buffer, network.y + 1, "eth0"), "{text}");
        assert!(
            row_has(&buffer, network.y + 1, "↓1.0 MiB/s ↑2.0 KiB/s"),
            "{text}"
        );
        assert!(
            row_text(&buffer, network.y + 2).chars().any(is_braille),
            "堆叠迷你图行\n{text}"
        );
        assert!(!text.contains("wlan0") && !text.contains("lo "), "{text}");
        assert!(
            (network.y..network.bottom()).any(|y| row_has(&buffer, y, "2 个空闲接口")),
            "{text}"
        );
    }

    #[test]
    fn sensors_are_summarised_per_chip() {
        let _guard = lang_guard(Lang::ZhCn);
        let state = monitored();
        let (buffer, output) = paint_page(&state, Page::Monitor, 120, 100);
        let sensors = card_rect(&output, "sensors");
        let text = (sensors.y..sensors.bottom())
            .map(|y| row_text(&buffer, y))
            .collect::<Vec<_>>()
            .join("\n");
        let card_has =
            |needle: &str| (sensors.y..sensors.bottom()).any(|y| row_has(&buffer, y, needle));
        assert!(card_has("coretemp"), "{text}");
        // 数字是最热的传感器，多传感器芯片的平均温度作说明跟在后面。
        assert!(card_has("70°C 平均 65°C"), "{text}");
        assert!(card_has("nvme"), "{text}");
        assert!(card_has("40°C"), "{text}");
        assert!(!card_has("Core 0"), "逐传感器行已汇总\n{text}");
        // 窄卡片：说明先让位，条形与最高温度保住。
        let (buffer, output) = paint_page(&state, Page::Monitor, 30, 100);
        let sensors = card_rect(&output, "sensors");
        let row = (sensors.y..sensors.bottom())
            .map(|y| row_text(&buffer, y))
            .find(|row| row.contains("coretemp"))
            .expect("coretemp 行");
        assert!(row.contains("70°C") && row.contains('━'), "{row}");
        assert!(!row.contains("平均"), "{row}");
    }

    /// 画一帧系统页，并像 `State::commit_paint` 那样把各卡的滚动上界写回状态。
    fn paint_committed(state: &mut State, width: u16, height: u16) -> (Buffer, PaintOutput) {
        let (buffer, output) = paint_page(state, Page::Monitor, width, height);
        state.card_scroll_limits = output.card_scroll_limits;
        (buffer, output)
    }

    fn card_text(buffer: &Buffer, rect: Rect) -> String {
        (rect.y..rect.bottom())
            .map(|y| row_text(buffer, y))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 卡片内容放得下时滚动上界为 0：滚轮 / ↑↓ 不改变 `card_scroll`，画面也不
    /// 动；快照变动留下的越界存量 offset 由卡片按同一个上界钳回来。
    #[test]
    fn scrolling_leaves_cards_whose_content_fits_untouched() {
        let _guard = lang_guard(Lang::ZhCn);
        let mut state = monitored();
        let (baseline, output) = paint_committed(&mut state, 120, 100);
        // 4 核 / 1 块 GPU / 2 块盘 / 1 个活动接口 / 2 颗芯片 / 3 个进程，都放得下。
        for card in SCROLLABLE_CARDS {
            assert_eq!(state.card_scroll_limits.get(card), Some(0), "{card}");
            for _ in 0..3 {
                state.scroll_card(card, 1);
            }
            assert_eq!(state.card_scroll.get(card).copied(), Some(0), "{card}");
        }
        let (buffer, _) = paint_committed(&mut state, 120, 100);
        for card in SCROLLABLE_CARDS {
            let rect = card_rect(&output, card);
            assert_eq!(
                card_text(&buffer, rect),
                card_text(&baseline, rect),
                "{card} 画面不动"
            );
        }
        let sensors = card_text(&buffer, card_rect(&output, "sensors")).replace(' ', "");
        assert!(
            sensors.contains("coretemp") && sensors.contains("nvme"),
            "{sensors}"
        );
        for card in SCROLLABLE_CARDS {
            state.card_scroll.insert(card.into(), 9);
        }
        let (buffer, _) = paint_committed(&mut state, 120, 100);
        for card in SCROLLABLE_CARDS {
            let rect = card_rect(&output, card);
            assert_eq!(
                card_text(&buffer, rect),
                card_text(&baseline, rect),
                "{card} 越界存量钳回 0"
            );
        }
    }

    /// 进程表按表体行数定上界（与 kit::table 的内部钳位同一个值）：滚到底停在
    /// 最后一屏，反向滚第一格画面就变，没有死区；越界的存量值也一样。
    #[test]
    fn process_table_scrolls_to_the_last_screen_and_back_without_a_dead_zone() {
        let _guard = lang_guard(Lang::ZhCn);
        let mut state = monitored();
        state.process_sort = ProcessSort::Pid;
        if let Some(sample) = state.metrics.as_mut() {
            sample.processes = (1..=30)
                .map(|pid| process(pid, &format!("proc{pid:02}"), 1.0, 1024))
                .collect();
        }
        paint_committed(&mut state, 120, 100);
        // 卡高 10：内高 8，减去筛选行与表头，表体 6 行 → 上界 30 − 6。
        assert_eq!(state.card_scroll_limits.get("processes"), Some(24));
        for _ in 0..40 {
            state.scroll_card("processes", 1);
        }
        assert_eq!(state.card_scroll.get("processes").copied(), Some(24));
        let (buffer, output) = paint_committed(&mut state, 120, 100);
        let card = card_text(&buffer, card_rect(&output, "processes")).replace(' ', "");
        assert!(
            card.contains("25–30/30") && card.contains("proc30"),
            "{card}"
        );
        assert!(!card.contains("proc24"), "{card}");
        state.scroll_card("processes", -1);
        assert_eq!(state.card_scroll.get("processes").copied(), Some(23));
        let (buffer, _) = paint_committed(&mut state, 120, 100);
        let card = card_text(&buffer, card_rect(&output, "processes")).replace(' ', "");
        assert!(
            card.contains("24–29/30") && card.contains("proc24"),
            "{card}"
        );
        assert!(!card.contains("proc30"), "反向第一格就有变化\n{card}");
        // 越界的存量值（进程刚退出、卡片刚变高）：反向第一格同样立刻生效。
        state.card_scroll.insert("processes".into(), 29);
        state.scroll_card("processes", -1);
        assert_eq!(state.card_scroll.get("processes").copied(), Some(23));
    }

    /// 放不下的温度 / 网络 / GPU 卡：上界按「条目数 − 放得下的完整条目数」算，
    /// 滚到底时最后一个条目完整露出、卡片不留空白。
    #[test]
    fn overflowing_cards_scroll_until_the_last_item_is_fully_shown() {
        let _guard = lang_guard(Lang::ZhCn);
        let mut state = monitored();
        if let Some(sample) = state.metrics.as_mut() {
            sample.sensors = (0..12)
                .map(|chip| sensor(&format!("chip{chip:02} Core"), 50.0))
                .collect();
            sample.networks = (0..6)
                .map(|net| network(&format!("eth{net}"), Some(4096.0), Some(1024.0)))
                .chain([network("lo", Some(0.0), Some(0.0))])
                .collect();
            sample.gpus = ["GPU-A", "GPU-B", "GPU-C", "GPU-D"]
                .iter()
                .map(|name| GpuMetric {
                    id: (*name).into(),
                    name: (*name).into(),
                    status: ObservationStatus::Ready,
                    usage_percent: Some(30.0),
                    memory_used_bytes: Some(GIB),
                    memory_total_bytes: Some(8 * GIB),
                    ..Default::default()
                })
                .collect();
        }
        paint_committed(&mut state, 120, 100);
        // 内高 8：温度 12 颗芯片 − 8 行；网络末行留给空闲说明，7 行放 3 个完整
        // 接口；GPU 每块 3 行，放 2 块完整的。
        assert_eq!(state.card_scroll_limits.get("sensors"), Some(4));
        assert_eq!(state.card_scroll_limits.get("network"), Some(3));
        assert_eq!(state.card_scroll_limits.get("gpu"), Some(2));
        for _ in 0..20 {
            for card in ["sensors", "network", "gpu"] {
                state.scroll_card(card, 1);
            }
        }
        assert_eq!(state.card_scroll.get("sensors").copied(), Some(4));
        assert_eq!(state.card_scroll.get("network").copied(), Some(3));
        assert_eq!(state.card_scroll.get("gpu").copied(), Some(2));
        let (buffer, output) = paint_committed(&mut state, 120, 100);
        let sensors = card_rect(&output, "sensors");
        let inner_rows = sensors.y + 1..sensors.bottom() - 1;
        assert!(
            row_has(&buffer, sensors.y + 1, "chip04"),
            "{}",
            card_text(&buffer, sensors)
        );
        assert!(
            row_has(&buffer, sensors.bottom() - 2, "chip11"),
            "最后一颗落在末行\n{}",
            card_text(&buffer, sensors)
        );
        assert!(
            inner_rows.clone().all(|y| row_has(&buffer, y, "chip")),
            "滚到底不留空白\n{}",
            card_text(&buffer, sensors)
        );
        let network = card_text(&buffer, card_rect(&output, "network")).replace(' ', "");
        assert!(
            network.contains("eth3") && network.contains("eth5"),
            "{network}"
        );
        assert!(
            !network.contains("eth2") && network.contains("1个空闲接口"),
            "{network}"
        );
        let gpu = card_rect(&output, "gpu");
        let gpu_text = card_text(&buffer, gpu);
        assert!(row_has(&buffer, gpu.y + 1, "GPU-C"), "{gpu_text}");
        assert!(row_has(&buffer, gpu.y + 4, "GPU-D"), "{gpu_text}");
        assert!(
            row_has(&buffer, gpu.y + 6, "显存"),
            "最后一块三行全露出\n{gpu_text}"
        );
        assert!(!gpu_text.contains("GPU-B"), "{gpu_text}");
    }

    /// 逐核卡按整行滚动：多列时滚一格所有核换一整行，不会整体错一列；滚到底
    /// 最后一个核可见。
    #[test]
    fn core_card_scrolls_by_whole_rows() {
        let _guard = lang_guard(Lang::ZhCn);
        let mut state = monitored();
        if let Some(sample) = state.metrics.as_mut() {
            sample.cores = (0..40).map(|id| core(id, 20.0)).collect();
        }
        let core_hits = |output: &PaintOutput| {
            output
                .hits
                .iter()
                .filter_map(|(rect, action)| match action {
                    Action::Core(id) => Some((rect.y, *id)),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        let (_, output) = paint_committed(&mut state, 120, 100);
        let hits = core_hits(&output);
        let first_row = hits[0].0;
        let slots = hits.iter().filter(|(y, _)| *y == first_row).count();
        assert!(slots > 1, "双列布局下逐核卡一行多格");
        let rows = 40_usize.div_ceil(slots);
        assert_eq!(state.card_scroll_limits.get("cores"), Some(rows - 8));
        state.scroll_card("cores", 1);
        let (_, output) = paint_committed(&mut state, 120, 100);
        let hits = core_hits(&output);
        assert_eq!(hits[0].1, slots, "滚一格换一整行");
        assert!(
            hits.iter()
                .all(|(y, id)| usize::from(*y - first_row) == (id - slots) / slots),
            "每个核仍在自己的列上：{hits:?}"
        );
        for _ in 0..100 {
            state.scroll_card("cores", 1);
        }
        let (_, output) = paint_committed(&mut state, 120, 100);
        let hits = core_hits(&output);
        assert_eq!(hits[0].1, (rows - 8) * slots);
        assert_eq!(hits.last().map(|(_, id)| *id), Some(39), "最后一个核可见");
    }

    #[test]
    fn process_table_sorts_by_header_and_filters_by_name() {
        let _guard = lang_guard(Lang::ZhCn);
        let mut state = monitored();
        let row_of = |buffer: &Buffer, needle: &str| {
            (0..buffer.area.height)
                .find(|y| row_has(buffer, *y, needle))
                .unwrap_or_else(|| panic!("{needle} 在画面上"))
        };
        let (buffer, output) = paint_page(&state, Page::Monitor, 120, 100);
        for sort in PROCESS_COLUMNS {
            assert!(
                has(
                    &output,
                    |action| matches!(action, Action::SortProcesses(s) if *s == sort)
                ),
                "表头 {sort:?} 可点"
            );
        }
        assert!(has(
            &output,
            |action| matches!(action, Action::Process(id) if id.pid == 7)
        ));
        assert!(
            row_of(&buffer, "cargo") < row_of(&buffer, "herdr"),
            "CPU 降序"
        );
        assert!(row_of(&buffer, "herdr") < row_of(&buffer, "zsh"));
        let y = row_of(&buffer, "cargo");
        assert_eq!(color_at(&buffer, y, "92.0%"), Some(config().palette.red));
        state.process_sort = ProcessSort::Pid;
        let (buffer, _) = paint_page(&state, Page::Monitor, 120, 100);
        assert!(
            row_of(&buffer, "zsh") < row_of(&buffer, "cargo"),
            "PID 升序"
        );
        assert!(row_of(&buffer, "cargo") < row_of(&buffer, "herdr"));
        state.process_filter = "her".into();
        let (buffer, output) = paint_page(&state, Page::Monitor, 120, 100);
        assert!(buffer_has(&buffer, "herdr"));
        assert!(!buffer_has(&buffer, "cargo"), "筛选掉不匹配的进程");
        assert!(buffer_has(&buffer, "1–1 / 1"), "{}", buffer_text(&buffer));
        assert!(has(&output, |action| matches!(
            action,
            Action::FilterProcesses
        )));
        state.process_filter = "nothing".into();
        let (buffer, _) = paint_page(&state, Page::Monitor, 120, 100);
        assert!(buffer_has(&buffer, "没有匹配的进程"));
    }

    #[test]
    fn edit_layout_mode_adds_move_buttons_and_dims_the_other_cards() {
        let _guard = lang_guard(Lang::ZhCn);
        let mut state = monitored();
        let (_, output) = paint_page(&state, Page::Monitor, 120, 100);
        assert!(!has(&output, |action| matches!(
            action,
            Action::CardMove(..)
        )));
        assert!(has(&output, |action| matches!(action, Action::Close)));
        state.layout_editing = true;
        state.selected_card = Some("memory".into());
        let (buffer, output) = paint_page(&state, Page::Monitor, 120, 100);
        assert!(has(
            &output,
            |action| matches!(action, Action::CardMove(id, -1) if id == "memory")
        ));
        assert!(has(
            &output,
            |action| matches!(action, Action::CardMove(id, 1) if id == "cpu")
        ));
        assert!(buffer_has(&buffer, "完成"));
        assert!(buffer_has(&buffer, "移动卡片"));
        assert!(
            !has(&output, |action| matches!(action, Action::Close)),
            "编辑布局时页脚换成完成"
        );
        let palette = config().palette;
        let memory = card_rect(&output, "memory");
        assert_eq!(
            buffer[(memory.x, memory.y)].style().fg,
            Some(palette.accent)
        );
        let cpu = card_rect(&output, "cpu");
        let border = buffer[(cpu.x, cpu.y)].style();
        assert_eq!(border.fg, Some(palette.overlay0));
        assert!(border.add_modifier.contains(Modifier::DIM));
    }

    #[test]
    fn chart_glyph_tiers_change_the_sparkline_alphabet_and_the_bars() {
        let mut state = monitored();
        state.history = (0..30)
            .map(|index| HistoryPoint {
                at: 5_000 + index * 200,
                cpu: Some(50.0 + index as f32),
                memory: Some(40.0),
                cores: Vec::new(),
                temperature: None,
            })
            .collect();
        for glyphs in [
            ChartGlyphsPreference::Braille,
            ChartGlyphsPreference::Blocks,
            ChartGlyphsPreference::Ascii,
        ] {
            state.chart_glyphs = glyphs;
            let (buffer, output) = paint_page(&state, Page::Monitor, 120, 100);
            let cpu = card_rect(&output, "cpu");
            let gauge = row_text(&buffer, cpu.y + 1);
            let chart = row_text(&buffer, cpu.bottom() - 2);
            match glyphs {
                ChartGlyphsPreference::Braille => {
                    assert!(chart.chars().any(is_braille), "{glyphs:?}: {chart}");
                    assert!(gauge.contains('━'), "{glyphs:?}: {gauge}");
                }
                ChartGlyphsPreference::Blocks => {
                    assert!(chart.chars().any(is_block), "{glyphs:?}: {chart}");
                    assert!(!chart.chars().any(is_braille), "{glyphs:?}: {chart}");
                    assert!(gauge.contains('━'), "{glyphs:?}: {gauge}");
                }
                ChartGlyphsPreference::Ascii => {
                    assert!(chart.contains('#'), "{glyphs:?}: {chart}");
                    assert!(
                        !chart.chars().any(|ch| is_braille(ch) || is_block(ch)),
                        "{glyphs:?}: {chart}"
                    );
                    assert!(
                        gauge.contains('#') && !gauge.contains('━'),
                        "{glyphs:?}: {gauge}"
                    );
                }
            }
        }
    }

    #[test]
    fn footer_hints_are_clickable_and_yield_to_messages() {
        let _guard = lang_guard(Lang::ZhCn);
        let mut state = monitored();
        let (buffer, output) = paint_page(&state, Page::Monitor, 120, 40);
        assert!(has(&output, |action| matches!(action, Action::Refresh)));
        assert!(has(&output, |action| matches!(action, Action::Pause)));
        assert!(has(&output, |action| matches!(action, Action::Close)));
        assert!(buffer_has(&buffer, "暂停"));
        state.paused = true;
        let (buffer, _) = paint_page(&state, Page::Monitor, 120, 40);
        assert!(buffer_has(&buffer, "继续"));
        assert!(buffer_has(&buffer, "已暂停"));
        state.message = Some("hello footer".into());
        let (buffer, output) = paint_page(&state, Page::Monitor, 120, 40);
        assert!(buffer_has(&buffer, "hello footer"));
        assert!(!has(&output, |action| matches!(action, Action::Pause)));
        // 账号页没有「暂停」。
        let (_, output) = paint_page(&State::new(&config()), Page::Accounts, 120, 40);
        assert!(!has(&output, |action| matches!(action, Action::Pause)));
        assert!(has(&output, |action| matches!(action, Action::Refresh)));
    }

    #[test]
    fn page_tabs_name_the_third_page_monitor_preferences() {
        let _guard = lang_guard(Lang::ZhCn);
        let (buffer, output) = paint_page(&monitored(), Page::Monitor, 120, 40);
        assert!(buffer_has(&buffer, "监控偏好"));
        assert!(!buffer_has(&buffer, "设置"));
        assert!(has(&output, |action| matches!(
            action,
            Action::Page(Page::Settings)
        )));
        assert!(has(&output, |action| matches!(
            action,
            Action::Page(Page::Accounts)
        )));
    }

    /// 窄面板下三个页签都得留着：英文标签比中文长得多，放不下时截短而不是
    /// 整项丢弃，否则鼠标再也进不去那一页，活动页也不再高亮。
    #[test]
    fn narrow_panel_truncates_page_tabs_instead_of_dropping_them() {
        let _guard = lang_guard(Lang::En);
        let state = monitored();
        for width in [120_u16, 60, 40, 30, 24] {
            let (buffer, output) = paint_page(&state, Page::Settings, width, 40);
            let text = buffer_text(&buffer);
            for page in [Page::Monitor, Page::Accounts, Page::Settings] {
                assert!(
                    has(
                        &output,
                        |action| matches!(action, Action::Page(p) if *p == page)
                    ),
                    "宽 {width}: {page:?} 页签可点\n{text}"
                );
            }
            // 活动页签反色：被丢弃时 rect 为空，这一条就红。
            let active = hit_rect(&output, |action| {
                matches!(action, Action::Page(Page::Settings))
            })
            .expect("监控偏好页签命中区");
            assert_eq!(
                buffer[(active.x, active.y)].style().bg,
                Some(config().palette.accent),
                "宽 {width}: 活动页签反色\n{text}"
            );
        }
        // 宽面板仍是完整标签，截短只在放不下时发生。
        let (buffer, _) = paint_page(&state, Page::Settings, 120, 40);
        assert!(
            buffer_has(&buffer, "Monitor preferences"),
            "{}",
            buffer_text(&buffer)
        );
    }
}
