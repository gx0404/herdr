use super::*;
use ratatui::widgets::{Block, BorderType, Borders, Row, Sparkline, Table, Widget};
use unicode_segmentation::UnicodeSegmentation;

fn text(buffer: &mut Buffer, rect: Rect, row: u16, value: &str, style: Style) {
    if row >= rect.height || rect.width == 0 {
        return;
    }
    let value = value
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>();
    // Clip with an ellipsis instead of a hard cut so panel edges never
    // swallow the tail of a long status message.
    let value = if UnicodeWidthStr::width(value.as_str()) > usize::from(rect.width) {
        let mut cut = String::new();
        let mut used = 0_usize;
        for grapheme in value.graphemes(true) {
            let width = grapheme.width();
            if used + width > usize::from(rect.width).saturating_sub(1) {
                break;
            }
            cut.push_str(grapheme);
            used += width;
        }
        cut.push('…');
        cut
    } else {
        value
    };
    buffer.set_stringn(rect.x, rect.y + row, value, rect.width as usize, style);
}

fn block(buffer: &mut Buffer, rect: Rect, title: &str, palette: &Palette) -> Rect {
    let panel = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(palette.surface1))
        .title(title.to_owned())
        .title_style(
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD),
        )
        .style(Style::default().fg(palette.text).bg(palette.panel_bg));
    let inner = panel.inner(rect);
    panel.render(rect, buffer);
    inner
}

fn button(
    buffer: &mut Buffer,
    rect: Rect,
    label: &str,
    action: Action,
    palette: &Palette,
    hits: &mut Vec<(Rect, Action)>,
) {
    let width = (UnicodeWidthStr::width(label) as u16)
        .saturating_add(2)
        .min(rect.width);
    let rect = Rect::new(rect.x, rect.y, width, rect.height.min(1));
    buffer.set_style(rect, Style::default().bg(palette.surface0));
    text(
        buffer,
        rect,
        0,
        &format!(" {label} "),
        Style::default()
            .fg(palette.text)
            .add_modifier(Modifier::BOLD),
    );
    if !rect.is_empty() {
        hits.push((rect, action));
    }
}

/// Page tab with an unambiguous active state: the selected page inverts into
/// the accent color so it can never be confused with idle tabs.
fn page_tab(
    buffer: &mut Buffer,
    rect: Rect,
    label: &str,
    active: bool,
    action: Action,
    palette: &Palette,
    hits: &mut Vec<(Rect, Action)>,
) {
    let width = (UnicodeWidthStr::width(label) as u16)
        .saturating_add(2)
        .min(rect.width);
    let rect = Rect::new(rect.x, rect.y, width, rect.height.min(1));
    let style = if active {
        Style::default()
            .fg(palette.panel_bg)
            .bg(palette.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(palette.text).bg(palette.surface0)
    };
    buffer.set_style(rect, style);
    text(buffer, rect, 0, &format!(" {label} "), style);
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

fn updated_ago(now_ms: u64, observed_at_ms: u64) -> String {
    let seconds = now_ms.saturating_sub(observed_at_ms) / 1000;
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m", seconds / 60)
    } else {
        format!("{}h{}m", seconds / 3600, seconds % 3600 / 60)
    }
}

/// 一次绘制产生的矩形与命中区；未绘制的部分保持 `Rect::default()`。
pub(super) struct PaintOutput {
    pub hits: Vec<(Rect, Action)>,
    /// 页面铺满的矩形（本次传入 `page` 时等于 `area`）。
    pub page_rect: Rect,
    pub hover_rect: Rect,
    pub dialog_rect: Rect,
}

/// 渲染纯函数：`page` 是本次要画的页面（停靠面板由调用方决定画哪个 tab），
/// 状态只读；悬浮层只在没有页面时绘制，进程对话框总是最后覆盖。
pub(super) fn paint(
    buffer: &mut Buffer,
    area: Rect,
    state: &State,
    palette: &Palette,
    page: Option<Page>,
) -> PaintOutput {
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
        let inner = block(buffer, area, tr(" MONITOR ", " 监控 "), palette);
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
            Page::Monitor => monitor(buffer, body, state, palette, &mut hits),
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
    if page.is_none() {
        if let Some(hover) = state.hover.as_ref().filter(|hover| hover.visible) {
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
            for row in hover_rect.y..hover_rect.bottom() {
                for col in hover_rect.x..hover_rect.right() {
                    buffer[(col, row)].set_symbol(" ");
                }
            }
            let inner = block(
                buffer,
                hover_rect,
                &format!(" {} · {} ", hover.agent, tr("Account usage", "账号用量")),
                palette,
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
                &mut hits,
            );
            let y = inner.bottom().saturating_sub(1);
            button(
                buffer,
                Rect::new(inner.x, y, inner.width.min(24), 1),
                tr("Open page", "打开页面"),
                Action::Page(Page::Accounts),
                palette,
                &mut hits,
            );
            button(
                buffer,
                Rect::new(
                    inner.x.saturating_add(25),
                    y,
                    inner.width.saturating_sub(25),
                    1,
                ),
                tr("Bind account", "绑定账号"),
                Action::Bind,
                palette,
                &mut hits,
            );
        }
    }
    let mut dialog_rect = Rect::default();
    if let Some(dialog) = &state.process_dialog {
        hover_rect = Rect::default();
        hits.clear();
        let rect = crate::ui::centered_popup_rect(buffer.area, 66, 12).unwrap_or(buffer.area);
        dialog_rect = rect;
        for y in rect.y..rect.bottom() {
            for x in rect.x..rect.right() {
                buffer[(x, y)].set_symbol(" ");
            }
        }
        let inner = block(buffer, rect, tr(" PROCESS DETAILS ", " 进程详情 "), palette);
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
        button(
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
            button(
                buffer,
                Rect::new(
                    inner.x.saturating_add(18),
                    y,
                    inner.width.saturating_sub(18),
                    1,
                ),
                tr("Confirm", "确认执行"),
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
                Action::Terminate(true),
                palette,
                &mut hits,
            );
        }
    }
    PaintOutput {
        hits,
        page_rect,
        hover_rect,
        dialog_rect,
    }
}

fn monitor(
    buffer: &mut Buffer,
    area: Rect,
    state: &State,
    palette: &Palette,
    hits: &mut Vec<(Rect, Action)>,
) {
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
        let title = match section.as_str() {
            "cpu" => " CPU ",
            "cores" => tr(" CPU CORES ", " CPU 逐核 "),
            "memory" => tr(" MEMORY ", " 内存 "),
            "gpu" => " GPU ",
            "disks" => tr(" DISKS ", " 磁盘 "),
            "network" => tr(" NETWORK ", " 网络 "),
            "sensors" => tr(" TEMPERATURE ", " 温度 "),
            _ => tr(" PROCESSES ", " 进程 "),
        };
        let inner = block(buffer, rect, title, palette);
        hits.push((rect, Action::Card((*section).clone())));
        if rect.width > 18 {
            button(
                buffer,
                Rect::new(rect.right() - 8, rect.y, 3, 1),
                "↑",
                Action::CardMove((*section).clone(), -1),
                palette,
                hits,
            );
            button(
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
                button(
                    buffer,
                    Rect::new(inner.x, inner.y, inner.width.min(15), 1),
                    tr("Sort", "排序"),
                    Action::SortProcesses,
                    palette,
                    hits,
                );
                button(
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

fn accounts(
    buffer: &mut Buffer,
    area: Rect,
    state: &State,
    palette: &Palette,
    hits: &mut Vec<(Rect, Action)>,
) {
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
    // 设置里关掉的厂商不进侧栏 / ‹› 选择器：选中它只会得到一个永不发请求的页面。
    let listed = state
        .providers
        .iter()
        .filter(|provider| provider_listed(provider))
        .filter(|provider| !state.usage.disabled_providers.contains(&provider.agent))
        .collect::<Vec<_>>();
    let sidebar_width = if area.width >= 70 { 22 } else { 0 };
    if sidebar_width > 0 {
        let side = block(
            buffer,
            Rect::new(area.x, area.y, sidebar_width, area.height),
            tr(" PROVIDERS ", " 厂商 "),
            palette,
        );
        if listed.is_empty() {
            text(
                buffer,
                side,
                0,
                tr(
                    "No installed agent CLI detected.",
                    "未检测到已安装的 agent CLI。",
                ),
                Style::default().fg(palette.overlay0),
            );
        }
        for (index, provider) in listed
            .iter()
            .skip(state.scroll.min(listed.len().saturating_sub(1)))
            .take(side.height as usize)
            .enumerate()
        {
            let rect = Rect::new(side.x, side.y + index as u16, side.width, 1);
            let selected = state.selected_provider.as_deref() == Some(provider.agent.as_str());
            text(
                buffer,
                rect,
                0,
                &provider.label,
                Style::default()
                    .fg(if selected {
                        palette.accent
                    } else {
                        palette.text
                    })
                    .add_modifier(if selected {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            );
            hits.push((rect, Action::Provider(provider.agent.clone())));
        }
    }
    let picker_height = u16::from(sidebar_width == 0 && !listed.is_empty()) * 2;
    if picker_height > 0 && !area.is_empty() {
        let index = listed
            .iter()
            .position(|provider| Some(&provider.agent) == state.selected_provider.as_ref())
            .unwrap_or(0);
        let provider = listed[index];
        let previous = listed[(index + listed.len() - 1) % listed.len()];
        let next = listed[(index + 1) % listed.len()];
        button(
            buffer,
            Rect::new(area.x, area.y, 3.min(area.width), 1),
            "‹",
            Action::Provider(previous.agent.clone()),
            palette,
            hits,
        );
        let label = Rect::new(area.x + 4, area.y, area.width.saturating_sub(8), 1);
        text(
            buffer,
            label,
            0,
            &provider.label,
            Style::default().fg(palette.accent),
        );
        hits.push((label, Action::Provider(provider.agent.clone())));
        button(
            buffer,
            Rect::new(
                area.right().saturating_sub(3).max(area.x),
                area.y,
                3.min(area.width),
                1,
            ),
            "›",
            Action::Provider(next.agent.clone()),
            palette,
            hits,
        );
    }
    let body = Rect::new(
        area.x + sidebar_width + u16::from(sidebar_width > 0),
        area.y.saturating_add(picker_height),
        area.width
            .saturating_sub(sidebar_width + u16::from(sidebar_width > 0)),
        area.height.saturating_sub(picker_height),
    );
    accounts_body(buffer, body, state, &page_scope(state), palette, hits);
}

fn metric_scope(scope: &str) -> &str {
    match scope {
        "session" | "local" => tr("session", "会话统计"),
        "api_key" => "API key",
        "account" => tr("account", "账号"),
        other => other,
    }
}

fn metric_value(metric: &UsageMetric, now_ms: u64, compact: bool) -> String {
    let value = if let Some(text) = &metric.text_value {
        text.clone()
    } else if let Some(amount) = &metric.amount_decimal {
        format!("{amount} {}", metric.unit)
    } else if let Some(percent) = metric.used_percent {
        format!("{percent:.1}% {}", tr("used", "已用"))
    } else if let Some(used) = metric.used {
        format!("{used:.2} {}", metric.unit)
    } else if let Some(remaining) = metric.remaining {
        format!("{remaining:.2} {} {}", metric.unit, tr("remaining", "剩余"))
    } else {
        "—".into()
    };
    if let Some(reset) = metric.resets_at {
        let minutes = reset.saturating_sub(now_ms / 1000).div_ceil(60);
        if compact {
            return format!("{value} ↻{}h{:02}m", minutes / 60, minutes % 60);
        }
        format!(
            "{value} · {} {}h {:02}m",
            tr("resets in", "重置于"),
            minutes / 60,
            minutes % 60
        )
    } else {
        value
    }
}

/// 账号正文的作用域视图：页面与悬浮层各自传入自己的数据，正文本身不区分来源。
pub(super) struct AccountsScope<'a> {
    pub accounts: &'a [AccountUsageSnapshot],
    pub provider: Option<&'a str>,
    pub account: Option<&'a str>,
    /// 本作用域可绑定的 pane；有值时才画「绑定账号」按钮（页面作用域在 B-3
    /// 接入 pane 选择器前恒为 `None`，按钮只在悬浮层出现）。
    pub pane: Option<&'a str>,
    /// 本作用域有强意图刷新排队或在途：旧数据变暗，空态显示「刷新中…」。
    pub refreshing: bool,
    /// 本作用域的账号列表滚动位置。
    pub scroll: usize,
}

/// 账号页 / 浮动仪表盘使用的页面作用域。
pub(super) fn page_scope(state: &State) -> AccountsScope<'_> {
    AccountsScope {
        accounts: &state.accounts,
        provider: state.selected_provider.as_deref(),
        account: state.selected_account.as_deref(),
        pane: state.selected_pane.as_deref(),
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
        provider: state.hover_scope.provider.as_deref(),
        account,
        pane: state.hover_scope.pane.as_deref(),
        refreshing: state.hover_scope.refreshing(),
        scroll: state.hover_scope.scroll,
    }
}

/// 禁用态按钮：灰色、不回填命中区；返回实际占用宽度。
fn disabled_button(buffer: &mut Buffer, rect: Rect, label: &str, palette: &Palette) -> u16 {
    let width = (UnicodeWidthStr::width(label) as u16)
        .saturating_add(2)
        .min(rect.width);
    let rect = Rect::new(rect.x, rect.y, width, rect.height.min(1));
    buffer.set_style(rect, Style::default().bg(palette.surface0));
    text(
        buffer,
        rect,
        0,
        &format!(" {label} "),
        Style::default().fg(palette.overlay0),
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
    button(buffer, rect, label, action, palette, hits);
    *x = x.saturating_add(width).saturating_add(1);
}

/// 「刷新」按钮，刷新在途时原位变成「刷新中…」状态提示（不回填命中区）。
fn refresh_button(
    buffer: &mut Buffer,
    x: &mut u16,
    row: Rect,
    refreshing: bool,
    palette: &Palette,
    hits: &mut Vec<(Rect, Action)>,
) {
    if refreshing {
        let rect = Rect::new(*x, row.y, row.right().saturating_sub(*x), 1);
        let width = disabled_button(buffer, rect, tr("Refreshing…", "刷新中…"), palette);
        *x = x.saturating_add(width).saturating_add(1);
    } else {
        flow_button(
            buffer,
            x,
            row,
            tr("Refresh", "刷新"),
            Action::Refresh,
            palette,
            hits,
        );
    }
}

pub(super) fn usage_table(
    buffer: &mut Buffer,
    area: Rect,
    state: &State,
    scope: &AccountsScope<'_>,
    palette: &Palette,
    hits: &mut Vec<(Rect, Action)>,
) {
    use ratatui::layout::Constraint;
    let mut entries = Vec::new();
    for account in scope.accounts {
        if account.metrics.is_empty() {
            entries.push((
                account,
                vec![
                    account.account_label.clone(),
                    status(account.status).into(),
                    "—".into(),
                    account.message.clone().unwrap_or_default(),
                ],
            ));
        } else {
            for metric in &account.metrics {
                entries.push((
                    account,
                    vec![
                        account.account_label.clone(),
                        format!("{} [{}]", metric.label, metric_scope(&metric.scope)),
                        metric_value(metric, state.now_ms, true),
                        status(account.status).into(),
                    ],
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
        .map(|(index, (account, columns))| {
            hits.push((
                Rect::new(area.x, area.y + index as u16 + 1, area.width, 1),
                Action::Account(account.account_id.clone()),
            ));
            Row::new(columns.clone()).style(
                Style::default()
                    .fg(if scope.account == Some(account.account_id.as_str()) {
                        palette.accent
                    } else {
                        palette.text
                    })
                    .bg(if index % 2 == 0 {
                        palette.surface0
                    } else {
                        palette.panel_bg
                    }),
            )
        })
        .collect::<Vec<_>>();
    Table::new(
        rows,
        [
            Constraint::Percentage(20),
            Constraint::Percentage(28),
            Constraint::Percentage(32),
            Constraint::Percentage(20),
        ],
    )
    .header(
        Row::new([
            tr("Account", "账号"),
            tr("Metric / scope", "指标 / 范围"),
            tr("Usage / reset", "用量 / 重置"),
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

pub(super) fn account_rows(state: &State, accounts: &[AccountUsageSnapshot]) -> usize {
    accounts
        .iter()
        .map(|account| {
            if state.usage.format == UsageDisplayFormat::Table {
                return account.metrics.len().max(1);
            }
            2 + usize::from(account.message.is_some())
                + usize::from(account.plan.is_some())
                + usize::from(account.account_identity.is_some())
                + usize::from(account.observed_at_ms > 0)
                + account
                    .metrics
                    .iter()
                    .map(|metric| 2 + usize::from(metric.used_percent.is_some()))
                    .sum::<usize>()
        })
        .sum()
}

fn usage_dashboard(
    buffer: &mut Buffer,
    area: Rect,
    state: &State,
    scope: &AccountsScope<'_>,
    palette: &Palette,
    hits: &mut Vec<(Rect, Action)>,
) {
    let start = scope
        .scroll
        .min(account_rows(state, scope.accounts).saturating_sub(area.height.max(1) as usize));
    let viewport_row = |row: usize| {
        row.checked_sub(start)
            .filter(|row| *row < area.height as usize)
            .map(|row| Rect::new(area.x, area.y + row as u16, area.width, 1))
    };
    let mut row = 0;
    for account in scope.accounts {
        if row >= start + area.height as usize {
            break;
        }
        if let Some(header) = viewport_row(row) {
            let selected = scope.account == Some(account.account_id.as_str());
            let label = format!(
                "{}{}",
                if selected { "› " } else { "  " },
                account.account_label
            );
            text(
                buffer,
                header,
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
            let offset = UnicodeWidthStr::width(label.as_str()) as u16;
            let tail = Rect::new(
                header.x.saturating_add(offset),
                header.y,
                header.width.saturating_sub(offset),
                1,
            );
            text(
                buffer,
                tail,
                0,
                &format!(" · {}", status(account.status)),
                Style::default()
                    .fg(status_color(account.status, palette))
                    .add_modifier(Modifier::BOLD),
            );
            hits.push((header, Action::Account(account.account_id.clone())));
        }
        row += 1;
        for (value, color) in [
            (account.message.as_deref(), palette.yellow),
            (account.plan.as_deref(), palette.overlay1),
            (account.account_identity.as_deref(), palette.overlay1),
        ] {
            if let Some(value) = value {
                if let Some(rect) = viewport_row(row) {
                    text(buffer, rect, 0, value, Style::default().fg(color));
                }
                row += 1;
            }
        }
        for metric in &account.metrics {
            if let Some(rect) = viewport_row(row) {
                let scope = metric_scope(&metric.scope);
                text(
                    buffer,
                    rect,
                    0,
                    &format!("{} [{}]", metric.label, scope),
                    Style::default().fg(palette.text),
                );
            }
            row += 1;
            if let Some(rect) = viewport_row(row) {
                text(
                    buffer,
                    rect,
                    0,
                    &metric_value(metric, state.now_ms, false),
                    Style::default().fg(palette.teal),
                );
            }
            row += 1;
            if let Some(percent) = metric.used_percent {
                if let Some(rect) = viewport_row(row) {
                    bar(buffer, rect, Some(percent as f32), palette);
                }
                row += 1;
            }
        }
        if account.observed_at_ms > 0 {
            if let Some(rect) = viewport_row(row) {
                let mut line = format!(
                    "{} {}",
                    tr("Updated", "更新于"),
                    updated_ago(state.now_ms, account.observed_at_ms)
                );
                if !account.source.is_empty() {
                    line.push_str(&format!(" · {}", account.source));
                }
                text(
                    buffer,
                    rect,
                    0,
                    &line,
                    Style::default().fg(palette.overlay0),
                );
            }
            row += 1;
        }
        row += 1;
    }
}

fn accounts_body(
    buffer: &mut Buffer,
    area: Rect,
    state: &State,
    scope: &AccountsScope<'_>,
    palette: &Palette,
    hits: &mut Vec<(Rect, Action)>,
) {
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
        if area.height > 1 {
            let row = Rect::new(area.x, area.bottom() - 1, area.width, 1);
            let mut x = row.x;
            refresh_button(buffer, &mut x, row, scope.refreshing, palette, hits);
        }
        return;
    }
    let content = Rect::new(area.x, area.y, area.width, area.height.saturating_sub(3));
    if state.usage.format == UsageDisplayFormat::Table {
        usage_table(buffer, content, state, scope, palette, hits);
    } else {
        usage_dashboard(buffer, content, state, scope, palette, hits);
    }
    if scope.refreshing {
        // 切换厂商 / 账号后旧快照保留但变暗，直到新数据到达。
        buffer.set_style(content, Style::default().add_modifier(Modifier::DIM));
    }
    if area.height > 3
        && scope
            .provider
            .is_some_and(|agent| matches!(agent, "claude" | "antigravity"))
    {
        button(
            buffer,
            Rect::new(area.x, area.bottom() - 2, area.width.min(25), 1),
            tr("Enable official callback", "启用官方回调"),
            Action::UsageIntegration(true),
            palette,
            hits,
        );
        button(
            buffer,
            Rect::new(
                area.x + 26,
                area.bottom() - 2,
                area.width.saturating_sub(26),
                1,
            ),
            tr("Remove callback", "移除回调"),
            Action::UsageIntegration(false),
            palette,
            hits,
        );
    }
    if area.height > 3 {
        // 少于两个候选账号时「切换账号」是禁用态：不回填命中区。只探第二个
        // 候选是否存在，渲染期不分配。
        let switch = Rect::new(area.x, area.bottom() - 3, area.width.min(25), 1);
        let switch_label = tr("Switch account", "切换账号");
        if state.cycle_candidates(scope.provider).nth(1).is_some() {
            button(
                buffer,
                switch,
                switch_label,
                Action::CycleAccount,
                palette,
                hits,
            );
        } else {
            disabled_button(buffer, switch, switch_label, palette);
        }
        button(
            buffer,
            Rect::new(
                area.x + 26,
                area.bottom() - 3,
                area.width.saturating_sub(26),
                1,
            ),
            tr("Dashboard / table", "仪表盘 / 表格"),
            Action::UsageFormat,
            palette,
            hits,
        );
    }
    if area.height > 1 {
        // 底行流式排布：刷新（或「刷新中…」）· 官方来源 · 绑定（有 pane 时）。
        let row = Rect::new(area.x, area.bottom() - 1, area.width, 1);
        let mut x = row.x;
        refresh_button(buffer, &mut x, row, scope.refreshing, palette, hits);
        flow_button(
            buffer,
            &mut x,
            row,
            tr("Official source", "官方查询说明"),
            Action::Source,
            palette,
            hits,
        );
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
    }
}

fn settings(
    buffer: &mut Buffer,
    area: Rect,
    state: &State,
    palette: &Palette,
    hits: &mut Vec<(Rect, Action)>,
) {
    let mut rows = vec![
        (
            format!(
                "{}: {} ms",
                tr("Sampling interval", "采样间隔"),
                state.monitor.interval_ms
            ),
            Action::Interval,
        ),
        (
            format!(
                "{}: {}",
                tr("Card height", "卡片高度"),
                state.monitor.card_height
            ),
            Action::CardSize,
        ),
        (
            format!(
                "{}: {} min",
                tr("History", "历史范围"),
                state.monitor.history_minutes
            ),
            Action::HistoryRange,
        ),
        (
            format!(
                "[{}] {}",
                if state.monitor.alerts_enabled {
                    "x"
                } else {
                    " "
                },
                tr("Resource alerts while connected", "连接期间资源告警")
            ),
            Action::ToggleAlerts,
        ),
        (
            format!(
                "[{}] {}",
                if state.usage.enabled { "x" } else { " " },
                tr("Provider account usage", "厂商账号用量")
            ),
            Action::UsageEnabled,
        ),
        (
            format!(
                "{}: {:?}",
                tr("Usage format", "用量样式"),
                state.usage.format
            ),
            Action::UsageFormat,
        ),
        (
            format!(
                "{}: {:?}",
                tr("Usage placement", "用量位置"),
                state.usage.position
            ),
            Action::UsagePosition,
        ),
    ];
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
        rows.push((
            format!(
                "[{}] {id}",
                if state.monitor.visible.iter().any(|item| item == id) {
                    "x"
                } else {
                    " "
                }
            ),
            Action::Metric(id.into()),
        ));
    }
    for (index, rule) in state.monitor.alerts.iter().enumerate() {
        rows.push((
            format!(
                "{} {} ≥ {:.0}%",
                tr("Alert", "告警"),
                rule.metric,
                rule.threshold
            ),
            Action::AlertThreshold(index),
        ));
        rows.push((
            format!("  {} {}s", tr("Duration", "持续"), rule.duration_seconds),
            Action::AlertDuration(index),
        ));
        rows.push((
            format!("  {} {}s", tr("Cooldown", "冷却"), rule.cooldown_seconds),
            Action::AlertCooldown(index),
        ));
    }
    for provider in state
        .providers
        .iter()
        .filter(|provider| provider_listed(provider))
    {
        rows.push((
            format!(
                "[{}] {}",
                if state.usage.disabled_providers.contains(&provider.agent) {
                    " "
                } else {
                    "x"
                },
                provider.label
            ),
            Action::ProviderEnabled(provider.agent.clone()),
        ));
    }
    if let Some(sample) = &state.metrics {
        for (id, label) in sample
            .gpus
            .iter()
            .map(|item| (&item.id, &item.name))
            .chain(
                sample
                    .disks
                    .iter()
                    .map(|item| (&item.id, &item.mount_point)),
            )
            .chain(sample.networks.iter().map(|item| (&item.id, &item.id)))
        {
            rows.push((
                format!(
                    "[{}] {label}",
                    if state.monitor.hidden_devices.contains(id) {
                        " "
                    } else {
                        "x"
                    }
                ),
                Action::Device(id.clone()),
            ));
        }
        for sensor in &sample.sensors {
            let id = format!("sensor:{}", sensor.name);
            rows.push((
                format!(
                    "[{}] {}",
                    if state.monitor.hidden_devices.contains(&id) {
                        " "
                    } else {
                        "x"
                    },
                    sensor.name
                ),
                Action::Device(id),
            ));
        }
    }
    let max_scroll = rows.len().saturating_sub(1);
    for (index, (label, action)) in rows
        .into_iter()
        .skip(state.scroll.min(max_scroll))
        .take(area.height as usize)
        .enumerate()
    {
        let rect = Rect::new(area.x, area.y + index as u16, area.width, 1);
        text(buffer, rect, 0, &label, Style::default().fg(palette.text));
        hits.push((rect, action));
    }
}
