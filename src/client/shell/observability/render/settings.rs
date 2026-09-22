//! 设置页：分节的行列表，只有落入视口的行才格式化文案。

use super::*;

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
                section_title(id)
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
pub(super) fn settings(
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
