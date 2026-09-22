//! 监控偏好页：分区卡片（kit `card`）+ 开关 / 分段控件 / 步进器（kit `tabs`），
//! 取代整行点击循环；写回仍走 `PreferenceKey`。整页先画进离屏缓冲，再按
//! `settings_scroll` 搬进可见区——卡片可以被裁在视口边缘，不必拆帧画。

use std::borrow::Cow;

use super::*;
use crate::ui::kit::card::{render_card, CardSpec};
use crate::ui::kit::tabs::{render_segmented, render_stepper, render_toggle};

/// 步进器固定宽度：`[-]` + 值槽 10 列 + `[+]`，值变长变短按钮不跳位。
const STEPPER_WIDTH: u16 = 18;

/// 显示项开关的卡片 id（与系统页的卡片顺序一致）。
const CARD_IDS: [&str; 8] = [
    "cpu",
    "cores",
    "memory",
    "gpu",
    "disks",
    "network",
    "sensors",
    "processes",
];

/// 设置页的一个控件行。文案在构建时格式化：整页离屏绘制，没有「只格式化视口
/// 内」的必要。
enum Row<'a> {
    /// 开关：整行可点（点标签也能切换）。
    Toggle {
        label: Cow<'a, str>,
        on: bool,
        action: Action,
    },
    /// 分段控件：每段一个动作，直接设值。
    Segmented {
        label: &'a str,
        options: Vec<&'a str>,
        active: usize,
        actions: Vec<Action>,
    },
    /// 步进器：`-` / `+` 各一个动作。
    Stepper {
        label: Cow<'a, str>,
        value: String,
        down: Action,
        up: Action,
    },
    /// 按钮 + 右侧说明；`action` 为 `None` 时是禁用态。
    Button {
        label: &'a str,
        note: &'a str,
        action: Option<Action>,
    },
    /// 只读说明行。
    Note(String),
}

struct Card<'a> {
    title: &'a str,
    rows: Vec<Row<'a>>,
}

fn stepper<'a>(label: impl Into<Cow<'a, str>>, value: String, down: Action, up: Action) -> Row<'a> {
    Row::Stepper {
        label: label.into(),
        value,
        down,
        up,
    }
}

fn toggle<'a>(label: impl Into<Cow<'a, str>>, on: bool, action: Action) -> Row<'a> {
    Row::Toggle {
        label: label.into(),
        on,
        action,
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

/// 分区：监控 / 账号用量 / 显示项 / 告警（有规则时）/ 厂商（有列出时）/ 设备
/// （有采样时）。
fn build_cards(state: &State) -> Vec<Card<'_>> {
    let texts = &crate::i18n::texts().monitor;
    let monitor = &state.monitor;
    let usage = &state.usage;
    let mut cards = Vec::new();

    cards.push(Card {
        title: texts.section_monitor,
        rows: vec![
            stepper(
                texts.sampling_interval,
                format!("{} ms", monitor.interval_ms),
                Action::Interval(-1),
                Action::Interval(1),
            ),
            stepper(
                texts.card_height,
                monitor.card_height.to_string(),
                Action::CardSize(-1),
                Action::CardSize(1),
            ),
            stepper(
                texts.history_range,
                format!("{} min", monitor.history_minutes),
                Action::HistoryRange(-1),
                Action::HistoryRange(1),
            ),
            toggle(
                texts.alerts_enabled,
                monitor.alerts_enabled,
                Action::ToggleAlerts,
            ),
        ],
    });

    let mut usage_rows = vec![
        toggle(texts.usage_enabled, usage.enabled, Action::UsageEnabled),
        Row::Segmented {
            label: texts.usage_format,
            options: vec![
                usage_format_label(UsageDisplayFormat::Dashboard),
                usage_format_label(UsageDisplayFormat::Table),
            ],
            active: match usage.format {
                UsageDisplayFormat::Dashboard => 0,
                UsageDisplayFormat::Table => 1,
            },
            actions: vec![
                Action::UsageFormat(UsageDisplayFormat::Dashboard),
                Action::UsageFormat(UsageDisplayFormat::Table),
            ],
        },
        Row::Segmented {
            label: texts.usage_position,
            options: vec![
                usage_position_label(UsageDisplayPosition::Hover),
                usage_position_label(UsageDisplayPosition::Page),
                usage_position_label(UsageDisplayPosition::Both),
            ],
            active: match usage.position {
                UsageDisplayPosition::Hover => 0,
                UsageDisplayPosition::Page => 1,
                UsageDisplayPosition::Both => 2,
            },
            actions: vec![
                Action::UsagePosition(UsageDisplayPosition::Hover),
                Action::UsagePosition(UsageDisplayPosition::Page),
                Action::UsagePosition(UsageDisplayPosition::Both),
            ],
        },
    ];
    if usage.position == UsageDisplayPosition::Page {
        usage_rows.push(Row::Note(format!("↳ {}", texts.hover_closed_hint)));
    }
    usage_rows.push(stepper(
        texts.hover_delay,
        format!("{} ms", usage.hover_delay_ms),
        Action::HoverDelay(-1),
        Action::HoverDelay(1),
    ));
    usage_rows.push(Row::Button {
        label: texts.restore_config,
        note: if state.usage_overridden {
            texts.clear_overrides
        } else {
            texts.no_overrides
        },
        action: state
            .usage_overridden
            .then_some(Action::RestoreUsagePreferences),
    });
    usage_rows.push(Row::Note(format!(
        "{}: {} s ({})",
        texts.api_refresh, usage.api_refresh_seconds, texts.config_value_hint
    )));
    usage_rows.push(Row::Note(format!(
        "{}: {} s ({})",
        texts.cli_refresh, usage.cli_refresh_seconds, texts.config_value_hint
    )));
    usage_rows.push(Row::Note(format!(
        "{}: {} ({})",
        texts.interactive_probe,
        if usage.interactive_probe {
            texts.on
        } else {
            texts.off
        },
        texts.config_value_hint
    )));
    cards.push(Card {
        title: texts.section_usage,
        rows: usage_rows,
    });

    cards.push(Card {
        title: texts.section_cards,
        rows: CARD_IDS
            .iter()
            .map(|id| {
                toggle(
                    section_title(id),
                    monitor.visible.iter().any(|item| item == id),
                    Action::Metric((*id).into()),
                )
            })
            .collect(),
    });

    if !monitor.alerts.is_empty() {
        let mut rows = Vec::with_capacity(monitor.alerts.len() * 3);
        for (index, rule) in monitor.alerts.iter().enumerate() {
            rows.push(stepper(
                crate::i18n::fill(texts.alert_rule_fmt, &[("metric", &rule.metric)]),
                format!("{:.0}%", rule.threshold),
                Action::AlertThreshold(index, -1),
                Action::AlertThreshold(index, 1),
            ));
            rows.push(stepper(
                format!("  {}", texts.alert_duration),
                format!("{} s", rule.duration_seconds),
                Action::AlertDuration(index, -1),
                Action::AlertDuration(index, 1),
            ));
            rows.push(stepper(
                format!("  {}", texts.alert_cooldown),
                format!("{} s", rule.cooldown_seconds),
                Action::AlertCooldown(index, -1),
                Action::AlertCooldown(index, 1),
            ));
        }
        cards.push(Card {
            title: texts.section_alerts,
            rows,
        });
    }

    let providers = state
        .providers
        .iter()
        .filter(|provider| provider_listed(provider))
        .map(|provider| {
            toggle(
                provider.label.as_str(),
                !usage.disabled_providers.contains(&provider.agent),
                Action::ProviderEnabled(provider.agent.clone()),
            )
        })
        .collect::<Vec<_>>();
    if !providers.is_empty() {
        cards.push(Card {
            title: texts.section_providers,
            rows: providers,
        });
    }

    if let Some(sample) = &state.metrics {
        let shown = |id: &str| !monitor.hidden_devices.iter().any(|hidden| hidden == id);
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
            .map(|(id, label)| toggle(label, shown(id), Action::Device(id.into())))
            .chain(sample.sensors.iter().map(|sensor| {
                let id = format!("sensor:{}", sensor.name);
                let on = shown(&id);
                toggle(sensor.name.as_str(), on, Action::Device(id))
            }))
            .collect::<Vec<_>>();
        if !devices.is_empty() {
            cards.push(Card {
                title: texts.section_devices,
                rows: devices,
            });
        }
    }
    cards
}

/// 行右端 `width` 列的控件槽（放不下时裁到行宽）。
fn control_slot(rect: Rect, width: u16) -> Rect {
    let width = width.min(rect.width);
    Rect::new(rect.right() - width, rect.y, width, 1)
}

/// 控件左侧的标签：占到控件前一列，放不下截断。
fn draw_label(buffer: &mut Buffer, rect: Rect, control_x: u16, label: &str, palette: &Palette) {
    let width = control_x.saturating_sub(rect.x).saturating_sub(1);
    if width > 0 {
        text(
            buffer,
            Rect::new(rect.x, rect.y, width, 1),
            0,
            label,
            Style::default().fg(palette.text),
        );
    }
}

/// 画一行控件，命中区按控件语义登记：开关整行、分段每段、步进器两端按钮。
fn draw_row(
    buffer: &mut Buffer,
    rect: Rect,
    row: &Row<'_>,
    state: &State,
    palette: &Palette,
    hits: &mut Vec<(Rect, Action)>,
) {
    if rect.is_empty() {
        return;
    }
    let ascii = state.chart_glyphs.ascii();
    match row {
        Row::Toggle { label, on, action } => {
            let control = control_slot(rect, 3);
            draw_label(buffer, rect, control.x, label, palette);
            render_toggle(buffer, control, *on, false, true, ascii, palette);
            hits.push((rect, action.clone()));
        }
        Row::Segmented {
            label,
            options,
            active,
            actions,
        } => {
            let width = options
                .iter()
                .map(|option| crate::ui::display_width_u16(option).saturating_add(2))
                .fold(0u16, u16::saturating_add);
            let control = control_slot(rect, width);
            draw_label(buffer, rect, control.x, label, palette);
            let rects = render_segmented(buffer, control, options, *active, None, true, palette);
            for (segment, action) in rects.iter().zip(actions) {
                if !segment.is_empty() {
                    hits.push((*segment, action.clone()));
                }
            }
        }
        Row::Stepper {
            label,
            value,
            down,
            up,
        } => {
            let control = control_slot(rect, STEPPER_WIDTH);
            draw_label(buffer, rect, control.x, label, palette);
            let (minus, plus) = render_stepper(buffer, control, value, false, true, palette);
            if !minus.is_empty() {
                hits.push((minus, down.clone()));
                hits.push((plus, up.clone()));
            }
        }
        Row::Button {
            label,
            note,
            action,
        } => {
            match action {
                Some(action) => {
                    secondary_button(buffer, rect, label, action.clone(), palette, hits);
                }
                None => {
                    disabled_button(buffer, rect, label, palette);
                }
            }
            let width = crate::ui::modal_button_width(&format!(" {label} "));
            text(
                buffer,
                Rect::new(
                    rect.x.saturating_add(width + 1),
                    rect.y,
                    rect.width.saturating_sub(width + 1),
                    1,
                ),
                0,
                note,
                Style::default().fg(palette.overlay0),
            );
        }
        Row::Note(note) => text(buffer, rect, 0, note, Style::default().fg(palette.overlay0)),
    }
}

/// 监控偏好页：分区卡片竖排，整页离屏绘制后按 `settings_scroll`（行）搬进
/// 视口，越界的滚动收回到最后一屏；命中区随之平移，视口外的丢弃。
pub(super) fn settings(
    buffer: &mut Buffer,
    area: Rect,
    state: &State,
    palette: &Palette,
    hits: &mut Vec<(Rect, Action)>,
) {
    if area.is_empty() {
        return;
    }
    let cards = build_cards(state);
    let card_height = |card: &Card<'_>| {
        u16::try_from(card.rows.len())
            .unwrap_or(u16::MAX)
            .saturating_add(2)
    };
    let total = cards
        .iter()
        .map(card_height)
        .fold(0u16, u16::saturating_add)
        .saturating_add(u16::try_from(cards.len().saturating_sub(1)).unwrap_or(u16::MAX));
    let mut page = Buffer::empty(Rect::new(0, 0, area.width, total));
    page.set_style(
        page.area,
        Style::default().fg(palette.text).bg(palette.panel_bg),
    );
    let mut page_hits = Vec::new();
    let mut y = 0u16;
    for card in &cards {
        let height = card_height(card);
        let inner = render_card(
            &mut page,
            Rect::new(0, y, area.width, height),
            &CardSpec {
                title: card.title,
                ..CardSpec::default()
            },
            state.glyphs,
            palette,
        );
        for (index, row) in card.rows.iter().enumerate() {
            let row_rect = Rect::new(
                inner.x,
                inner
                    .y
                    .saturating_add(u16::try_from(index).unwrap_or(u16::MAX)),
                inner.width,
                1,
            );
            draw_row(&mut page, row_rect, row, state, palette, &mut page_hits);
        }
        y = y.saturating_add(height).saturating_add(1);
    }
    let visible = area.height.min(total);
    let scroll = u16::try_from(state.settings_scroll)
        .unwrap_or(u16::MAX)
        .min(total.saturating_sub(area.height));
    for row in 0..visible {
        for x in 0..area.width {
            buffer[(area.x + x, area.y + row)] = page[(x, scroll + row)].clone();
        }
    }
    for (rect, action) in page_hits {
        if rect.y >= scroll && rect.y < scroll + visible {
            hits.push((
                Rect::new(
                    rect.x + area.x,
                    rect.y - scroll + area.y,
                    rect.width,
                    rect.height,
                ),
                action,
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;
    use crate::i18n::{lang_guard, Lang};

    fn preferences_state() -> State {
        let mut state = State::new(&config());
        state.now_ms = 14_000;
        state
    }

    /// 带一条告警规则、一个厂商与几个设备的状态。
    fn populated_state() -> State {
        let mut state = preferences_state();
        state.monitor.alerts = crate::config::MonitorConfig::default().alerts;
        state.providers = vec![UsageProviderInfo {
            agent: "claude".into(),
            label: "Claude Code".into(),
            source_url: "https://example.invalid/docs".into(),
            method: "cli".into(),
            account_scope: "account".into(),
            minimum_interval_seconds: 300,
            configured_accounts: vec!["claude:default".into()],
            installed: Some(true),
            supports_callback: true,
        }];
        state.metrics = Some(Box::new(SystemMetricsSnapshot {
            sampled_at_ms: 10_000,
            gpus: vec![GpuMetric {
                id: "gpu0".into(),
                name: "Test GPU".into(),
                ..Default::default()
            }],
            disks: vec![DiskMetric {
                id: "/dev/sda1:/".into(),
                mount_point: "/".into(),
                total_bytes: 1,
                ..Default::default()
            }],
            sensors: vec![SensorMetric {
                name: "coretemp Core 0".into(),
                ..Default::default()
            }],
            ..Default::default()
        }));
        state
    }

    #[test]
    fn preferences_page_lays_out_cards_with_kit_controls() {
        let _guard = lang_guard(Lang::ZhCn);
        let mut state = preferences_state();
        let (buffer, output) = paint_page(&state, Page::Settings, 120, 60);
        let text = buffer_text(&buffer);
        for needle in [
            "监控",
            "采样间隔",
            "1000 ms",
            "卡片高度",
            "历史范围",
            "15 min",
            "账号用量",
            "仪表盘",
            "表格",
            "悬浮延时",
            "400 ms",
            "显示项",
            "CPU 逐核",
            "恢复配置文件值",
            "无本机覆盖",
            "本机配置文件值",
        ] {
            assert!(buffer_has(&buffer, needle), "{needle}\n{text}");
        }
        assert!(
            buffer_has(&buffer, "[-]") && buffer_has(&buffer, "[+]"),
            "{text}"
        );
        assert!(
            buffer_has(&buffer, "━━●") || buffer_has(&buffer, "●━━"),
            "开关字形\n{text}"
        );
        for wanted in [
            |a: &Action| matches!(a, Action::Interval(-1)),
            |a: &Action| matches!(a, Action::Interval(1)),
            |a: &Action| matches!(a, Action::CardSize(1)),
            |a: &Action| matches!(a, Action::HistoryRange(-1)),
            |a: &Action| matches!(a, Action::ToggleAlerts),
            |a: &Action| matches!(a, Action::UsageEnabled),
            |a: &Action| matches!(a, Action::UsageFormat(UsageDisplayFormat::Table)),
            |a: &Action| matches!(a, Action::UsagePosition(UsageDisplayPosition::Page)),
            |a: &Action| matches!(a, Action::HoverDelay(1)),
            |a: &Action| matches!(a, Action::Metric(id) if id == "processes"),
        ] {
            assert!(has(&output, wanted), "缺少控件命中区\n{text}");
        }
        assert!(
            !has(&output, |a| matches!(a, Action::RestoreUsagePreferences)),
            "无本机覆盖时不可点"
        );
        for (rect, action) in &output.hits {
            assert!(
                contains_rect(Rect::new(0, 0, 120, 60), *rect),
                "命中区 {rect:?}（{action:?}）越界"
            );
        }
        // 有本机覆盖：按钮可点；位置为「页面」：常驻提示行。
        state.usage_overridden = true;
        state.usage.position = UsageDisplayPosition::Page;
        let (buffer, output) = paint_page(&state, Page::Settings, 120, 60);
        assert!(has(&output, |a| matches!(
            a,
            Action::RestoreUsagePreferences
        )));
        assert!(buffer_has(&buffer, "清除本机覆盖"));
        assert!(buffer_has(&buffer, "悬浮浮层已关闭"));
        // 活动段反色：分段控件的活动项底色是 accent。
        let page = hit_rect(&output, |a| {
            matches!(a, Action::UsagePosition(UsageDisplayPosition::Page))
        })
        .expect("页面段");
        assert_eq!(
            buffer[(page.x, page.y)].style().bg,
            Some(config().palette.accent)
        );
    }

    #[test]
    fn alerts_providers_and_devices_get_their_own_cards() {
        let _guard = lang_guard(Lang::ZhCn);
        let state = populated_state();
        let (buffer, output) = paint_page(&state, Page::Settings, 120, 80);
        let text = buffer_text(&buffer);
        for needle in [
            "告警",
            "告警 cpu ≥",
            "90%",
            "持续",
            "30 s",
            "冷却",
            "60 s",
            "厂商",
            "Claude Code",
            "设备",
            "Test GPU",
            "coretemp Core 0",
        ] {
            assert!(buffer_has(&buffer, needle), "{needle}\n{text}");
        }
        assert!(has(&output, |a| matches!(a, Action::AlertThreshold(0, 1))));
        assert!(has(&output, |a| matches!(a, Action::AlertDuration(0, -1))));
        assert!(has(&output, |a| matches!(a, Action::AlertCooldown(0, 1))));
        assert!(has(
            &output,
            |a| matches!(a, Action::ProviderEnabled(agent) if agent == "claude")
        ));
        assert!(has(
            &output,
            |a| matches!(a, Action::Device(id) if id == "gpu0")
        ));
        assert!(has(
            &output,
            |a| matches!(a, Action::Device(id) if id == "sensor:coretemp Core 0")
        ));
    }

    #[test]
    fn preferences_page_scrolls_by_rows_and_clips_hits_to_the_viewport() {
        let _guard = lang_guard(Lang::ZhCn);
        let mut state = populated_state();
        let (buffer, output) = paint_page(&state, Page::Settings, 100, 14);
        assert!(has(&output, |a| matches!(a, Action::Interval(1))));
        assert!(
            !has(&output, |a| matches!(a, Action::Device(_))),
            "设备卡在视口之外"
        );
        assert!(buffer_has(&buffer, "采样间隔"));
        state.settings_scroll = 8;
        let (buffer, output) = paint_page(&state, Page::Settings, 100, 14);
        assert!(!buffer_has(&buffer, "采样间隔"), "滚过监控卡的首行");
        assert!(buffer_has(&buffer, "账号用量") || buffer_has(&buffer, "厂商账号用量"));
        for (rect, action) in &output.hits {
            assert!(
                contains_rect(Rect::new(0, 0, 100, 14), *rect),
                "命中区 {rect:?}（{action:?}）越界"
            );
        }
        // 越界的滚动收回到最后一屏：设备卡可见。
        state.settings_scroll = 10_000;
        let (buffer, output) = paint_page(&state, Page::Settings, 100, 14);
        assert!(
            buffer_has(&buffer, "coretemp Core 0"),
            "{}",
            buffer_text(&buffer)
        );
        assert!(has(&output, |a| matches!(a, Action::Device(_))));
        assert!(!has(&output, |a| matches!(a, Action::Interval(_))));
    }

    #[test]
    fn narrow_preferences_page_keeps_every_control_inside_the_panel() {
        let _guard = lang_guard(Lang::ZhCn);
        let state = populated_state();
        for width in [60_u16, 28] {
            let (buffer, output) = paint_page(&state, Page::Settings, width, 40);
            assert!(
                has(&output, |a| matches!(a, Action::Interval(1))),
                "宽 {width}"
            );
            assert!(
                has(&output, |a| matches!(a, Action::ToggleAlerts)),
                "宽 {width}"
            );
            assert!(buffer_has(&buffer, "1000 ms"), "宽 {width}: 步进器数值");
            for (rect, action) in &output.hits {
                assert!(
                    contains_rect(Rect::new(0, 0, width, 40), *rect),
                    "宽 {width}: 命中区 {rect:?}（{action:?}）越界"
                );
            }
        }
        paint_page(&state, Page::Settings, 3, 3);
        paint_page(&state, Page::Settings, 0, 0);
    }
}
