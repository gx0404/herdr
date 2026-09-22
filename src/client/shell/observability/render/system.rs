//! 系统页：主机摘要、资源卡片与进程详情对话框。

use super::*;
use ratatui::widgets::{Sparkline, Widget};

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

pub(super) fn monitor(
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

/// 系统页卡片标题（含两侧空格）；设置页的显示项复用同一份文案（ACC-12）。
pub(super) fn section_title(id: &str) -> &'static str {
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
