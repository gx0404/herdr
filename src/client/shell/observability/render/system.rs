//! 系统页：主机摘要条、资源卡片（kit 原语：card / meter_row / gauge /
//! braille_chart / table）与进程详情对话框。布局与命中计算全在这里完成，
//! 状态只读；历史曲线读每份样本算好的预分桶、进程表读每份样本排好的行序（见
//! `HistoryBuckets` / `ProcessRows`），只按图宽合并桶、拷进栈上缓冲，不逐帧遍历
//! 历史、不逐帧排序（被页面底边裁到的卡片例外：借一块一卡大小的暂存缓冲，见
//! `cut_system_card`）。
//!
//! Graphite 外观：圆角卡片（字形随 `ui.border_style`）、`surface1` 细边框、说明色的
//! 卡片标题；卡片的主读数（CPU / 内存 / GPU / 磁盘占用、最高温度、进程数）粗体压在
//! 上边框右侧。颜色只表达严重程度：常态是中性的 accent 与灰阶，越过告警规则的
//! 警告线转黄、越过规则阈值转红（`thresholds`）。

use std::borrow::Cow;

use super::*;
use crate::ui::kit::braille_chart::{render_area_chart, render_shaded_sparkline, ChartGlyphs};
use crate::ui::kit::card::{render_card, CardSpec};
use crate::ui::kit::gauge::{render_gauge, GaugeSpec, GaugeThresholds, Severity};
use crate::ui::kit::meter_row::{
    render_meter_row, render_meter_row_aligned, MeterColumns, MeterRow,
};
use crate::ui::kit::table::{
    render_table, Column, ColumnWidth, SortState, TableCell, TableRender, TableState,
};

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

/// 把预分桶（`HISTORY_SLOTS` 个）按图宽合并成 `slots` 个槽写进 `out`：每槽取它覆盖
/// 的桶里非空桶的平均，全空为 NaN（迷你图留白）。返回实际槽数。
fn resample_buckets(buckets: &[f32], slots: usize, out: &mut [f32; HISTORY_SLOTS]) -> usize {
    let slots = slots.min(HISTORY_SLOTS).min(buckets.len());
    if slots == 0 {
        return 0;
    }
    let total = buckets.len();
    for (slot, value) in out.iter_mut().enumerate().take(slots) {
        let from = slot * total / slots;
        let to = ((slot + 1) * total / slots).max(from + 1).min(total);
        let (sum, count) = buckets[from..to]
            .iter()
            .filter(|value| value.is_finite())
            .fold((0.0f32, 0u16), |(sum, count), value| {
                (sum + value, count.saturating_add(1))
            });
        *value = if count > 0 {
            sum / f32::from(count)
        } else {
            f32::NAN
        };
    }
    slots
}

/// 在 `area` 画一条 0..=100 定标的历史迷你图：读每份样本算好的预分桶，按图宽合并；
/// 预分桶与历史对不上时（测试直接改了历史）退回现算。
fn history_chart(
    buffer: &mut Buffer,
    area: Rect,
    state: &State,
    series: HistorySeries,
    palette: &Palette,
) {
    if area.is_empty() {
        return;
    }
    let glyphs = state.chart_glyphs.chart();
    let wanted = usize::from(area.width) * samples_per_column(glyphs);
    let mut samples = [0f32; HISTORY_SLOTS];
    let slots = match state.history_buckets.get(state, series) {
        Some(buckets) => resample_buckets(buckets, wanted, &mut samples),
        None => bucket_history(
            &state.history,
            state.now_ms,
            state.monitor.history_minutes,
            wanted,
            |point| series.value(point, state.selected_core),
            &mut samples,
        ),
    };
    render_shaded_sparkline(
        buffer,
        area,
        &samples[..slots],
        Some(100.0),
        glyphs,
        Style::default().fg(chart_color(palette)),
        Style::default().fg(palette.overlay0),
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

/// 某个指标的着色阈值：与该指标的告警规则同一条线——规则阈值起算严重（红），
/// 提前 15 个百分点算警告（黄）；没有规则的指标用 75 % / 90 %（`GaugeThresholds`
/// 的默认值，正是默认 CPU / 内存 / 磁盘规则的那两条线）。
fn thresholds(state: &State, metric: &str) -> GaugeThresholds {
    state
        .monitor
        .alerts
        .iter()
        .find(|rule| rule.metric == metric)
        .filter(|rule| rule.threshold.is_finite() && rule.threshold > 0.0)
        .map_or_else(GaugeThresholds::default, |rule| {
            let crit = (rule.threshold / 100.0) as f32;
            GaugeThresholds {
                warn: (crit - 0.15).max(0.0),
                crit,
            }
        })
}

/// 比例的语义色：警告黄、严重红，常态 `None`（调用方用自己的中性色）。
fn severity_color(ratio: f32, thresholds: GaugeThresholds, palette: &Palette) -> Option<Color> {
    Severity::of(ratio, None, thresholds).color(palette)
}

/// 历史曲线（轮廓）与网络曲线的中性色：说明色一档，比条形的 accent 安静；曲线
/// 下方的填充再低一档（`overlay0`）。
fn chart_color(palette: &Palette) -> Color {
    palette.subtext0
}

/// 主机摘要条：粗体主机名，其后是说明色的 `系统 · 已运行 · 更新时间 · 采样间隔`，
/// 右端是「编辑布局 / 完成」入口。非 Ready 的采样状态用状态色插在更新时间之后。
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
    let mut facts = String::new();
    let os = if sample.operating_system.is_empty() {
        sample.environment.as_str()
    } else {
        sample.operating_system.as_str()
    };
    if !os.is_empty() {
        facts.push_str(" · ");
        facts.push_str(os);
    }
    if sample.uptime_seconds > 0 {
        facts.push_str(" · ");
        facts.push_str(&crate::i18n::fill(
            texts.uptime_fmt,
            &[("span", &span_text(sample.uptime_seconds))],
        ));
    }
    facts.push_str(" · ");
    facts.push_str(&age_text(state.now_ms, sample.sampled_at_ms, sample.status));
    let status_text = (sample.status != ObservationStatus::Ready)
        .then(|| format!(" · {}", status(sample.status)));
    let interval = format!(
        " · {}",
        crate::i18n::fill(
            texts.interval_fmt,
            &[("ms", &sample.interval_ms.to_string())]
        )
    );
    let right = button_x.saturating_sub(1);
    let mut x = area.x;
    let spans: [(&str, Style); 4] = [
        (sample.hostname.as_str(), crate::ui::design::title(palette)),
        (facts.as_str(), crate::ui::design::meta(palette)),
        (
            status_text.as_deref().unwrap_or(""),
            Style::default().fg(status_color(sample.status, palette)),
        ),
        (interval.as_str(), crate::ui::design::meta(palette)),
    ];
    for (value, style) in spans {
        if value.is_empty() {
            continue;
        }
        if x >= right {
            break;
        }
        // 放不下的段以省略号收尾，后面的段不再画。
        let width = right - x;
        let fitted = crate::ui::truncate_end(value, usize::from(width));
        let (end, _) = buffer.set_stringn(x, area.y, &fitted, usize::from(width), style);
        if crate::ui::display_width(value) > usize::from(width) {
            break;
        }
        x = end;
    }
}

/// 还没有采样时的占位：页面正中一个安静的空状态——灰色字形、标题与一行说明，
/// 不再是左上角的一行灰字。
fn placeholder(buffer: &mut Buffer, area: Rect, title: &str, palette: &Palette) {
    crate::ui::kit::empty_state::render_empty_state(
        buffer,
        area,
        &crate::ui::kit::empty_state::EmptyState {
            glyph: Some("◌"),
            title,
            body: Some(tr(
                "Metrics appear here after the first sample.",
                "收到第一份采样后在这里显示。",
            )),
            action: None,
        },
        palette,
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
        placeholder(
            buffer,
            area,
            tr("Connecting to the host sampler…", "正在连接主机采样器…"),
            palette,
        );
        return None;
    };
    if sample.sampled_at_ms == 0 {
        placeholder(
            buffer,
            area,
            tr("Waiting for the first sample…", "等待第一份有效采样…"),
            palette,
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
/// 暂存缓冲，再只拷回可见的几行——与偏好页一样裁在视口边缘，露出的是
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

/// 卡片的主读数（压在上边框右侧的粗体徽标）与它的颜色：CPU / 内存 / GPU / 磁盘
/// 的占用、最高温度、进程数。越过阈值时转黄 / 红，常态正文色。
fn headline(
    id: &str,
    state: &State,
    sample: &SystemMetricsSnapshot,
    palette: &Palette,
) -> Option<(String, Color)> {
    let ratio_badge = |ratio: Option<f32>, metric: &str| {
        let ratio = ratio.filter(|ratio| ratio.is_finite())?;
        let color =
            severity_color(ratio, thresholds(state, metric), palette).unwrap_or(palette.text);
        Some((format!("{:.1}%", ratio * 100.0), color))
    };
    match id {
        "cpu" => ratio_badge(sample.cpu_percent.map(|value| value / 100.0), "cpu"),
        "memory" => ratio_badge(
            (sample.memory.total_bytes > 0)
                .then(|| sample.memory.used_bytes as f32 / sample.memory.total_bytes as f32),
            "memory",
        ),
        "gpu" => ratio_badge(
            listed_gpus(state, sample)
                .iter()
                .filter_map(|gpu| gpu.usage_percent)
                .reduce(f32::max)
                .map(|value| value / 100.0),
            "gpu",
        ),
        "disks" => ratio_badge(
            listed_disks(state, sample)
                .iter()
                .map(|disk| {
                    disk.total_bytes.saturating_sub(disk.available_bytes) as f32
                        / disk.total_bytes as f32
                })
                .reduce(f32::max),
            "disk",
        ),
        "sensors" => {
            let chips = sensor_chips(state, sample);
            let hottest = chips
                .iter()
                .filter(|chip| chip.count > 0)
                .max_by(|a, b| a.max.total_cmp(&b.max))?;
            let ratio = hottest.max / hottest.critical.unwrap_or(100.0);
            let color =
                severity_color(ratio, GaugeThresholds::default(), palette).unwrap_or(palette.text);
            Some((format!("{:.0}°C", hottest.max), color))
        }
        "processes" => (!sample.processes.is_empty())
            .then(|| (sample.processes.len().to_string(), palette.subtext0)),
        _ => None,
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
    // 顶边右侧：非 Ready 的分组挂状态徽标，否则是卡片的主读数；编辑布局时那里放
    // ↑↓，两者都不挂。
    let reading = headline(id, state, sample, palette);
    let badge = if state.layout_editing {
        None
    } else if group_status != ObservationStatus::Ready {
        Some((status(group_status), status_color(group_status, palette)))
    } else {
        reading
            .as_ref()
            .map(|(value, color)| (value.as_str(), *color))
    };
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

/// 一条铺满整行的占用条（主读数在卡片上边框，这里不再重复数字）。
fn full_gauge(buffer: &mut Buffer, row: Rect, spec: GaugeSpec<'_>, stale: bool, palette: &Palette) {
    if row.is_empty() {
        return;
    }
    if stale {
        // 过期数据：保留量值，但用灰色单段代替阈值色，不给旧数据画确定的语义色。
        let share = spec.ratio.map_or_else(
            || {
                spec.segments.map_or(0.0, |segments| {
                    segments.iter().map(|(share, _)| share).sum()
                })
            },
            |ratio| ratio.clamp(0.0, 1.0),
        );
        let muted = [(share, palette.overlay0)];
        render_gauge(
            buffer,
            row,
            &GaugeSpec {
                segments: Some(&muted),
                ..spec
            },
            palette,
        );
    } else {
        render_gauge(buffer, row, &spec, palette);
    }
}

/// 卡片里的历史区：正文第 `from` 行起到底；比 3 行高时在说明与曲线之间空一行，
/// 卡片读起来不挤。
fn chart_area(inner: Rect, from: u16) -> Rect {
    let top = inner.y.saturating_add(from);
    if top >= inner.bottom() {
        return Rect::default();
    }
    let height = inner.bottom() - top;
    let gap = u16::from(height > 3);
    Rect::new(inner.x, top + gap, inner.width, height - gap)
}

/// CPU 卡：整行占用条（数字在上边框）、型号与逻辑核数的说明（点选某个核后换成该核
/// 的说明）、历史迷你图。
fn cpu_card(
    buffer: &mut Buffer,
    inner: Rect,
    state: &State,
    sample: &SystemMetricsSnapshot,
    palette: &Palette,
) {
    let texts = &crate::i18n::texts().monitor;
    full_gauge(
        buffer,
        Rect::new(inner.x, inner.y, inner.width, 1),
        GaugeSpec {
            ratio: sample.cpu_percent.map(|value| value / 100.0),
            thresholds: thresholds(state, "cpu"),
            ascii: state.chart_glyphs.ascii(),
            ..GaugeSpec::default()
        },
        sample.status == ObservationStatus::Stale,
        palette,
    );
    let cores = crate::i18n::fill(
        texts.logical_cpus_fmt,
        &[("n", &sample.cores.len().to_string())],
    );
    match state.selected_core {
        Some(core) => text(
            buffer,
            inner,
            1,
            &format!("CPU {core} · {} min", state.monitor.history_minutes),
            crate::ui::design::body(palette).add_modifier(Modifier::BOLD),
        ),
        None => {
            let caption = if sample.cpu_brand.is_empty() {
                cores
            } else {
                format!("{} · {cores}", sample.cpu_brand)
            };
            text(
                buffer,
                inner,
                1,
                &caption,
                crate::ui::design::muted(palette),
            );
        }
    }
    history_chart(
        buffer,
        chart_area(inner, 2),
        state,
        HistorySeries::Cpu,
        palette,
    );
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
    let cpu_thresholds = thresholds(state, "cpu");
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
                    thresholds: cpu_thresholds,
                    ascii,
                    ..GaugeSpec::default()
                },
                stale: false,
            },
            columns,
            palette,
        );
        if state.selected_core == Some(core.id) {
            buffer.set_style(
                rect,
                Style::default()
                    .bg(palette.selection_row_bg())
                    .add_modifier(Modifier::BOLD),
            );
        }
        hits.push((rect, Action::Core(core.id)));
    }
    max
}

/// 内存卡：整行的已用 / 缓存分段占用条（占用比例在上边框）、已用量与缓存量的说明、
/// Swap 条、历史迷你图。
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
    let memory_thresholds = thresholds(state, "memory");
    let used_color = severity_color(used_ratio.unwrap_or(0.0), memory_thresholds, palette)
        .unwrap_or(palette.accent);
    // 已用段跟随严重程度（常态 accent），缓存段是低一档的中性灰。
    let segments = [
        (used_ratio.unwrap_or(0.0), used_color),
        (cache_ratio.unwrap_or(0.0), palette.overlay1),
    ];
    full_gauge(
        buffer,
        Rect::new(inner.x, inner.y, inner.width, 1),
        GaugeSpec {
            ratio: used_ratio,
            segments: Some(&segments),
            thresholds: memory_thresholds,
            ascii,
            ..GaugeSpec::default()
        },
        stale,
        palette,
    );
    let mut caption = format!(
        "{} {} / {}",
        texts.mem_used,
        bytes(memory.used_bytes),
        bytes(total)
    );
    if cache > 0 {
        caption.push_str(" · ");
        caption.push_str(&crate::i18n::fill(
            texts.cache_fmt,
            &[("size", &bytes(cache))],
        ));
    }
    text(
        buffer,
        inner,
        1,
        &caption,
        crate::ui::design::muted(palette),
    );
    if inner.height > 2 {
        let swap = format!(
            "{} / {}",
            bytes(memory.swap_used_bytes),
            bytes(memory.swap_total_bytes)
        );
        render_meter_row(
            buffer,
            Rect::new(inner.x, inner.y + 2, inner.width, 1),
            &MeterRow {
                label: texts.swap,
                value: &swap,
                detail: None,
                gauge: GaugeSpec {
                    ratio: (memory.swap_total_bytes > 0)
                        .then(|| memory.swap_used_bytes as f32 / memory.swap_total_bytes as f32),
                    thresholds: memory_thresholds,
                    ascii,
                    ..GaugeSpec::default()
                },
                stale,
            },
            palette,
        );
    }
    history_chart(
        buffer,
        chart_area(inner, 3),
        state,
        HistorySeries::Memory,
        palette,
    );
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
            crate::ui::design::meta(palette),
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
    let gpu_thresholds = thresholds(state, "gpu");
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
            crate::ui::design::body(palette),
            &temperature,
            crate::ui::design::meta(palette),
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
                        thresholds: gpu_thresholds,
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
                                thresholds: gpu_thresholds,
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
                        crate::ui::design::muted(palette),
                        note,
                        crate::ui::design::meta(palette),
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
            crate::ui::design::meta(palette),
        );
        return 0;
    }
    let disk_thresholds = thresholds(state, "disk");
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
                1 => TableCell {
                    fg: Some(palette.subtext0),
                    ..TableCell::from(disk.name.as_str())
                },
                2 => TableCell::quantity(bytes(used)),
                3 => TableCell::quantity(bytes(disk.total_bytes)),
                _ => TableCell {
                    text: Cow::Owned(format!("{:.0}%", ratio * 100.0)),
                    fg: severity_color(ratio, disk_thresholds, palette),
                    dim_tail: 0,
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
            crate::ui::design::muted(palette),
            &net_rates_text(net),
            crate::ui::design::body(palette),
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
                    // 收 / 发两档中性灰：颜色只表达状态，方向靠速率文字的 ↓↑。
                    &[chart_color(palette), palette.overlay0],
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
            crate::ui::design::meta(palette),
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
            crate::ui::design::meta(palette),
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
            crate::ui::design::muted(palette),
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
            HistorySeries::Temperature,
            palette,
        );
    }
    max
}

/// 进程表的列顺序与 `ProcessSort` 的对应关系（用户列不可排序）。
const PROCESS_COLUMNS: [Option<ProcessSort>; 5] = [
    Some(ProcessSort::Pid),
    Some(ProcessSort::Name),
    None,
    Some(ProcessSort::Cpu),
    Some(ProcessSort::Memory),
];

/// 进程表的行序：读每份样本排好的 `ProcessRows`；对不上（测试直接改了样本、排序或
/// 筛选）时现算一次。
fn listed_processes<'a>(state: &'a State, sample: &SystemMetricsSnapshot) -> Cow<'a, [usize]> {
    match state.process_rows.get(state, sample) {
        Some(rows) => Cow::Borrowed(rows),
        None => Cow::Owned(sorted_process_rows(
            sample,
            state.process_sort,
            &state.process_filter,
        )),
    }
}

/// 进程表的列：窄时按 `priority` 从大到小丢——先丢用户列，再丢内存，再丢 PID，
/// 名称（至少 12 列）与 CPU 最后才让位；最窄时剩下 `top` 式的 PID · 名称 · CPU。
fn process_columns(texts: &crate::i18n::MonitorTexts, pid_width: u16) -> [Column<'_>; 5] {
    [
        Column {
            title: texts.proc_col_pid,
            width: ColumnWidth::Min(pid_width),
            align_right: true,
            sortable: true,
            priority: 2,
        },
        Column {
            title: texts.proc_col_name,
            width: ColumnWidth::FillAtLeast(12, 1),
            align_right: false,
            sortable: true,
            priority: 0,
        },
        Column {
            title: tr("User", "用户"),
            width: ColumnWidth::Fixed(10),
            align_right: false,
            sortable: false,
            priority: 4,
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
            width: ColumnWidth::Fixed(10),
            align_right: true,
            sortable: true,
            priority: 3,
        },
    ]
}

/// 进程卡：首行是筛选入口 + 当前筛选词 + 可见范围，其下是带表头的可排序表格
/// （点击表头按列排序，点击行打开详情；键盘焦点停在某一行或它的详情开着时，那一行
/// 画在 `selection_bg` 上）。数字列右对齐、单位变暗。返回滚动上界。
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
    let rows = listed_processes(state, sample);
    let process = |row: usize| rows.get(row).and_then(|index| sample.processes.get(*index));
    let filter_label = tr("⌕ Filter", "⌕ 筛选");
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
    let pid_width = sample
        .processes
        .iter()
        .map(|process| decimal_width(usize::try_from(process.identity.pid).unwrap_or(usize::MAX)))
        .max()
        .unwrap_or(0)
        .max(6);
    let columns = process_columns(texts, pid_width);
    let sort = SortState {
        column: PROCESS_COLUMNS
            .iter()
            .position(|sort| *sort == Some(state.process_sort))
            .unwrap_or(3),
        descending: matches!(state.process_sort, ProcessSort::Cpu | ProcessSort::Memory),
    };
    // 选中行：详情对话框正开着的那个进程，或键盘焦点（上一帧的命中区）停着的行。
    let focused = state
        .process_dialog
        .as_ref()
        .map(|dialog| &dialog.process.identity)
        .or_else(|| match state.hits.get(state.selected_hit) {
            Some((_, Action::Process(identity))) if state.page.is_some() => Some(identity),
            _ => None,
        });
    let selected = focused.and_then(|identity| {
        (0..rows.len())
            .find(|row| process(*row).is_some_and(|process| process.identity == *identity))
    });
    let cpu_thresholds = thresholds(state, "cpu");
    let table = TableState {
        sort: Some(sort),
        row_count: rows.len(),
        scroll: offset,
        selected,
        hovered: None,
        ascii: state.chart_glyphs.ascii(),
    };
    let render = render_table(
        buffer,
        Rect::new(inner.x, inner.y + 1, inner.width, inner.height - 1),
        &columns,
        &table,
        |row, column| {
            let Some(process) = process(row) else {
                return TableCell::from("");
            };
            match column {
                0 => TableCell {
                    fg: Some(palette.subtext0),
                    ..TableCell::from(process.identity.pid.to_string())
                },
                1 => TableCell::from(process.name.as_str()),
                2 => TableCell {
                    fg: Some(palette.subtext0),
                    ..TableCell::from(process.user.as_deref().unwrap_or("—"))
                },
                3 => TableCell {
                    text: Cow::Owned(percent(process.cpu_percent)),
                    fg: process
                        .cpu_percent
                        .and_then(|value| severity_color(value / 100.0, cpu_thresholds, palette)),
                    dim_tail: 0,
                },
                _ => TableCell::quantity(bytes(process.memory_bytes)),
            }
        },
        palette,
    );
    for (rect, column) in &render.header_cells {
        if let Some(Some(sort)) = PROCESS_COLUMNS.get(*column) {
            hits.push((*rect, Action::SortProcesses(*sort)));
        }
    }
    for (rect, row) in &render.rows {
        if let Some(process) = process(*row) {
            hits.push((*rect, Action::Process(process.identity.clone())));
        }
    }
    // 首行右侧：筛选词（输入中带光标）与可见范围 `起–止 / 总数`。
    let shown = render.rows.len();
    let range = if shown == 0 {
        format!("0 / {}", rows.len())
    } else {
        format!(
            "{}–{} / {}",
            render.scroll + 1,
            render.scroll + shown,
            rows.len()
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
        crate::ui::design::body(palette),
        &range,
        crate::ui::design::meta(palette),
    );
    if render.rows.is_empty() && !render.body.is_empty() {
        text(
            buffer,
            render.body,
            0,
            texts.no_matching_processes,
            crate::ui::design::meta(palette),
        );
    }
    table_scroll_max(rows.len(), &render)
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
    let rect = crate::ui::modal_rect(
        buffer.area,
        crate::ui::ModalSize::Content {
            width: 66,
            height: 14,
        },
    )
    .unwrap_or(buffer.area);
    for y in rect.y..rect.bottom() {
        for x in rect.x..rect.right() {
            buffer[(x, y)].set_symbol(" ");
        }
    }
    let inner = block(buffer, rect, tr(" Process details ", " 进程详情 "), cx);
    if inner.is_empty() {
        return rect;
    }
    let inner = Rect::new(
        inner.x.saturating_add(1),
        inner.y.saturating_add(1),
        inner.width.saturating_sub(2),
        inner.height.saturating_sub(2),
    );
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
        crate::ui::design::title(palette),
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
        crate::ui::design::muted(palette),
    );
    text(
        buffer,
        inner,
        3,
        dialog.process.executable.as_deref().unwrap_or("—"),
        crate::ui::design::meta(palette),
    );
    let y = inner.bottom().saturating_sub(1);
    let button_area = |x: u16| {
        Rect::new(
            x.min(inner.right()),
            y,
            inner.right().saturating_sub(x),
            u16::from(inner.height > 0),
        )
    };
    let width = |label: &str| crate::ui::display_width_u16(label).saturating_add(2);
    let cancel = tr("Cancel", "取消");
    secondary_button(
        buffer,
        button_area(inner.x),
        cancel,
        Action::CancelProcess,
        palette,
        hits,
    );
    let next_x = inner.x.saturating_add(width(cancel)).saturating_add(2);
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
        button(
            buffer,
            button_area(next_x),
            tr("Confirm", "确认执行"),
            crate::ui::ModalButtonTone::Danger,
            crate::ui::ModalButtonState::Normal,
            Action::ConfirmProcess,
            palette,
            hits,
        );
    } else {
        let terminate = tr("Terminate", "正常结束");
        button(
            buffer,
            button_area(next_x),
            terminate,
            crate::ui::ModalButtonTone::Danger,
            crate::ui::ModalButtonState::Normal,
            Action::Terminate(false),
            palette,
            hits,
        );
        button(
            buffer,
            button_area(next_x.saturating_add(width(terminate)).saturating_add(2)),
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

    /// 硬性版式约束：指标数字在任意宽度可见——窄时先砍条形，数字不裁。
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
        // 宽：主读数压在卡片上边框右侧，首行是整行条形，其下是型号与逻辑核数。
        let (wide, output) = paint_page(&state, Page::Monitor, 120, 100);
        let cpu = card_rect(&output, "cpu");
        assert!(row_has(&wide, cpu.y, "42.0%"), "{}", row_text(&wide, cpu.y));
        assert!(
            row_has(&wide, cpu.y + 1, "━"),
            "{}",
            row_text(&wide, cpu.y + 1)
        );
        assert!(
            row_has(&wide, cpu.y + 2, "Test CPU · 4 逻辑核"),
            "{}",
            row_text(&wide, cpu.y + 2)
        );
        assert!(buffer_has(&wide, "6.0 GiB / 16.0 GiB"));
        // 极窄：数字仍在上边框上（徽标优先、标题让位），条形不与它抢列。
        let (tiny, output) = paint_page(&state, Page::Monitor, 14, 100);
        let cpu = card_rect(&output, "cpu");
        let top = row_text(&tiny, cpu.y);
        assert!(top.contains("42.0%"), "{top}");
        let row = row_text(&tiny, cpu.y + 1);
        assert!(!row.contains("42.0%"), "数字不在条形那一行重复：{row}");
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
        assert_eq!(buffer[(network.x, network.y)].symbol(), "╭", "{text}");
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
            Some(config().palette.selection_row_bg()),
            "选中核画在 selection_bg 上"
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
        assert_eq!(
            buffer[(used, row)].style().fg,
            Some(palette.accent),
            "常态用量是中性的 accent"
        );
        let cache = (used..memory.right())
            .find(|x| buffer[(*x, row)].style().fg == Some(palette.overlay1))
            .expect("缓存段");
        assert_eq!(buffer[(cache, row)].symbol(), "━");
        assert!(row_has(&buffer, memory.y, "37.5%"), "内存占用压在上边框");
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
        for sort in PROCESS_COLUMNS.into_iter().flatten() {
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

    /// 进程表窄时按优先级丢列：先丢用户列，再丢内存，再丢 PID，名称与 CPU 最后
    /// 才让位；宽时五列都在。名称列至少 12 列，不会被低优先级的列挤成几个字符。
    #[test]
    fn process_table_drops_low_priority_columns_first() {
        let _guard = lang_guard(Lang::ZhCn);
        let mut state = monitored();
        state.monitor.visible = vec!["processes".into()];
        if let Some(sample) = state.metrics.as_mut() {
            for process in &mut sample.processes {
                process.user = Some("dev".into());
            }
        }
        let header = |width: u16| {
            let (buffer, output) = paint_page(&state, Page::Monitor, width, 40);
            let card = card_rect(&output, "processes");
            row_text(&buffer, card.y + 2).replace(' ', "")
        };
        let has_all = |row: &str, names: &[&str]| names.iter().all(|name| row.contains(name));
        let wide = header(90);
        assert!(
            has_all(&wide, &["PID", "名称", "用户", "CPU", "内存"]),
            "{wide}"
        );
        // 46 列：卡片内容 40 列，五列要 6+12+10+6+10+4 = 48 → 先丢用户列（38）。
        let medium = header(46);
        assert!(
            has_all(&medium, &["PID", "名称", "CPU", "内存"]),
            "{medium}"
        );
        assert!(!medium.contains("用户"), "先丢用户列：{medium}");
        let narrow = header(34);
        assert!(has_all(&narrow, &["PID", "名称", "CPU"]), "{narrow}");
        assert!(!narrow.contains("内存"), "再丢内存：{narrow}");
        let tiny = header(26);
        assert!(has_all(&tiny, &["名称", "CPU"]), "{tiny}");
        assert!(!tiny.contains("PID"), "再丢 PID：{tiny}");
        // 名称列保住至少 12 列：最长的名称在中等宽度下完整。
        let (buffer, _) = paint_page(&state, Page::Monitor, 46, 40);
        assert!(buffer_has(&buffer, "cargo"), "{}", buffer_text(&buffer));
    }

    /// 60 列（单列）时页面上没有互相压住的控件：页签、暂停标记、摘要条与「编辑
    /// 布局」按钮、页脚提示各占各的列，所有命中区都在页面内、同一行的单行命中区
    /// 互不重叠。
    #[test]
    fn nothing_overlaps_at_sixty_columns() {
        for lang in [Lang::ZhCn, Lang::En] {
            let _guard = lang_guard(lang);
            let mut state = monitored();
            state.paused = true;
            let (buffer, output) = paint_page(&state, Page::Monitor, 60, 40);
            let text = buffer_text(&buffer);
            let columns = output
                .hits
                .iter()
                .filter(|(_, action)| matches!(action, Action::Card(_)))
                .map(|(rect, _)| rect.x)
                .collect::<std::collections::BTreeSet<_>>();
            assert_eq!(columns.len(), 1, "{lang:?}: 60 列卡片单列堆叠\n{text}");
            let leaves = output
                .hits
                .iter()
                .filter(|(rect, action)| {
                    rect.height == 1
                        && !matches!(
                            action,
                            Action::Card(_) | Action::Process(_) | Action::Core(_)
                        )
                })
                .collect::<Vec<_>>();
            for (index, (a, first)) in leaves.iter().enumerate() {
                assert!(
                    contains_rect(Rect::new(0, 0, 60, 40), *a),
                    "{lang:?}: {first:?} {a:?} 越界\n{text}"
                );
                for (b, second) in &leaves[index + 1..] {
                    assert!(
                        a.intersection(*b).is_empty(),
                        "{lang:?}: {first:?} {a:?} 与 {second:?} {b:?} 重叠\n{text}"
                    );
                }
            }
            // 暂停标记与页签同在首行、互不压住。
            assert!(row_has(&buffer, 0, "‖"), "{lang:?}\n{text}");
            // 摘要条的主机名与「编辑布局」按钮都完整。
            let edit = hit_rect(&output, |action| matches!(action, Action::EditLayout))
                .expect("编辑布局按钮");
            assert!(row_has(&buffer, edit.y, "devbox"), "{lang:?}\n{text}");
        }
    }

    /// 还没有采样时页面正中是一个安静的空状态：字形、标题与一行说明居中，没有
    /// 卡片、没有页面滚动度量。
    #[test]
    fn empty_state_is_a_centered_placeholder() {
        let _guard = lang_guard(Lang::ZhCn);
        let state = State::new(&config());
        let (buffer, output) = paint_page(&state, Page::Monitor, 80, 30);
        let text = buffer_text(&buffer);
        let title = (0..30)
            .find(|y| row_has(&buffer, *y, "正在连接主机采样器…"))
            .unwrap_or_else(|| panic!("占位标题\n{text}"));
        assert!(
            title > 8 && title < 22,
            "标题在页面中部：第 {title} 行\n{text}"
        );
        let row = row_text(&buffer, title);
        let start = row.find('正').expect("标题");
        let left = row[..start].chars().count();
        let right = row.trim_end().chars().count();
        assert!(left > 10, "标题水平居中：{row:?}");
        assert!(right < 70, "{row:?}");
        assert!(
            (0..30).any(|y| row_has(&buffer, y, "收到第一份采样后")),
            "说明\n{text}"
        );
        assert!(!has(&output, |action| matches!(action, Action::Card(_))));
        assert_eq!(output.page_scroll_limits.get(Page::Monitor), None);
    }

    /// 颜色只表达严重程度，阈值与告警规则同一条线：GPU 规则是 95 %，所以 85 %
    /// 只到警告（黄），96 % 才严重（红）；CPU 规则 90 %，42 % 保持中性。
    #[test]
    fn severity_colors_follow_the_alert_rule_thresholds() {
        let _guard = lang_guard(Lang::ZhCn);
        let palette = config().palette;
        for (usage, expected) in [
            (60.0, palette.text),
            (85.0, palette.yellow),
            (96.0, palette.red),
        ] {
            let mut state = monitored();
            if let Some(sample) = state.metrics.as_mut() {
                sample.gpus[0].usage_percent = Some(usage);
            }
            let (buffer, output) = paint_page(&state, Page::Monitor, 120, 100);
            let gpu = card_rect(&output, "gpu");
            let headline = format!("{usage:.1}%");
            assert_eq!(
                color_at(&buffer, gpu.y, &headline),
                Some(expected),
                "GPU {usage}%：{}",
                row_text(&buffer, gpu.y)
            );
        }
        // 改规则阈值，着色线跟着走：CPU 规则调到 40 %，42 % 就是严重。
        let mut state = monitored();
        if let Some(rule) = state
            .monitor
            .alerts
            .iter_mut()
            .find(|rule| rule.metric == "cpu")
        {
            rule.threshold = 40.0;
        }
        let (buffer, output) = paint_page(&state, Page::Monitor, 120, 100);
        let cpu = card_rect(&output, "cpu");
        assert_eq!(color_at(&buffer, cpu.y, "42.0%"), Some(palette.red));
        let (buffer, output) = paint_page(&monitored(), Page::Monitor, 120, 100);
        let cpu = card_rect(&output, "cpu");
        assert_eq!(color_at(&buffer, cpu.y, "42.0%"), Some(palette.text));
    }

    /// 热路径：进程表的行序与历史曲线的桶在每份样本时算好，渲染只读。生产路径
    /// （`refresh_*`）算出的进程表与渲染时现算的回退完全一致，预分桶按图宽合并后与
    /// 直接分桶只差取整；样本、筛选或历史一变，旧缓存就不再被采用。
    #[test]
    fn per_sample_caches_match_the_fallback_and_expire_with_their_inputs() {
        let _guard = lang_guard(Lang::ZhCn);
        let mut state = monitored();
        state.history = (0..120u64)
            .map(|index| HistoryPoint {
                at: 200_000 + index * 1_000,
                cpu: Some((index % 50) as f32),
                memory: Some(40.0),
                cores: Vec::new(),
                temperature: Some(55.0),
            })
            .collect();
        let (fallback, output) = paint_page(&state, Page::Monitor, 120, 100);
        let processes = card_rect(&output, "processes");
        state.refresh_history_buckets();
        state.refresh_process_rows();
        let sample = state.metrics.as_deref().expect("样本");
        assert!(
            state.process_rows.get(&state, sample).is_some(),
            "行序已缓存"
        );
        let buckets = state
            .history_buckets
            .get(&state, HistorySeries::Cpu)
            .expect("预分桶已缓存");
        let (cached, _) = paint_page(&state, Page::Monitor, 120, 100);
        assert_eq!(
            card_text(&cached, processes),
            card_text(&fallback, processes),
            "进程表：缓存与现算一致"
        );
        let mut merged = [0f32; HISTORY_SLOTS];
        let mut direct = [0f32; HISTORY_SLOTS];
        let slots = resample_buckets(buckets, 64, &mut merged);
        let direct_slots = bucket_history(
            &state.history,
            state.now_ms,
            state.monitor.history_minutes,
            64,
            |point| point.cpu,
            &mut direct,
        );
        assert_eq!(slots, direct_slots);
        for (slot, (a, b)) in merged.iter().zip(&direct).take(slots).enumerate() {
            assert_eq!(a.is_nan(), b.is_nan(), "第 {slot} 槽空与非空一致");
            if !a.is_nan() {
                assert!((a - b).abs() < 5.0, "第 {slot} 槽：{a} / {b}");
            }
        }
        // 筛选词变了：旧行序作废，渲染退回现算，结果仍然正确。
        state.process_filter = "zsh".into();
        let sample = state.metrics.as_deref().expect("样本");
        assert!(state.process_rows.get(&state, sample).is_none());
        let (buffer, _) = paint_page(&state, Page::Monitor, 120, 100);
        assert!(buffer_has(&buffer, "zsh") && !buffer_has(&buffer, "cargo"));
        // 历史多了一个点：旧桶作废。
        state.history.push_back(HistoryPoint {
            at: 400_000,
            cpu: Some(10.0),
            memory: None,
            cores: Vec::new(),
            temperature: None,
        });
        assert!(state
            .history_buckets
            .get(&state, HistorySeries::Cpu)
            .is_none());
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
        // 偏好页没有「暂停」。
        let (_, output) = paint_page(&State::new(&config()), Page::Settings, 120, 40);
        assert!(!has(&output, |action| matches!(action, Action::Pause)));
    }

    /// 面板里的两页是「系统 | 偏好」：面板头已经写着「监控」，第二页不再叫
    /// 「监控偏好」，也不叫与全局设置浮层撞名的「设置」。
    #[test]
    fn page_tabs_name_the_second_page_preferences() {
        let _guard = lang_guard(Lang::ZhCn);
        let (buffer, output) = paint_page(&monitored(), Page::Monitor, 120, 40);
        assert!(row_has(&buffer, 0, "系统"), "{}", row_text(&buffer, 0));
        assert!(row_has(&buffer, 0, "偏好"), "{}", row_text(&buffer, 0));
        assert!(!buffer_has(&buffer, "监控偏好"));
        assert!(!buffer_has(&buffer, "设置"));
        assert!(has(&output, |action| matches!(
            action,
            Action::Page(Page::Settings)
        )));
    }

    /// 窄面板下两个页签都得留着：英文标签比中文长得多，放不下时截短而不是
    /// 整项丢弃，否则鼠标再也进不去那一页，活动页也不再高亮。
    #[test]
    fn narrow_panel_truncates_page_tabs_instead_of_dropping_them() {
        let _guard = lang_guard(Lang::En);
        let state = monitored();
        for width in [120_u16, 60, 40, 30, 24] {
            let (buffer, output) = paint_page(&state, Page::Settings, width, 40);
            let text = buffer_text(&buffer);
            for page in [Page::Monitor, Page::Settings] {
                assert!(
                    has(
                        &output,
                        |action| matches!(action, Action::Page(p) if *p == page)
                    ),
                    "宽 {width}: {page:?} 页签可点\n{text}"
                );
            }
            // 活动页签画在浅一档的结构面上：被丢弃时 rect 为空，这一条就红。
            let active = hit_rect(&output, |action| {
                matches!(action, Action::Page(Page::Settings))
            })
            .expect("偏好页签命中区");
            assert_eq!(
                buffer[(active.x, active.y)].style().bg,
                Some(config().palette.hover_row_bg()),
                "宽 {width}: 活动页签画在浅一档的结构面上\n{text}"
            );
        }
        // 宽面板仍是完整标签，截短只在放不下时发生。
        let (buffer, _) = paint_page(&state, Page::Settings, 120, 40);
        assert!(
            row_has(&buffer, 0, "Preferences"),
            "{}",
            buffer_text(&buffer)
        );
    }
}
