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

/// 分区：监控 / 显示项 / 告警（有规则时）/ 设备（有采样时）。
fn build_cards(state: &State) -> Vec<Card<'_>> {
    let texts = &crate::i18n::texts().monitor;
    let monitor = &state.monitor;
    let mut cards = Vec::new();

    let glyph_active = match state.chart_glyphs {
        ChartGlyphsPreference::Braille => 0,
        ChartGlyphsPreference::Blocks => 1,
        ChartGlyphsPreference::Ascii => 2,
    };
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
            Row::Segmented {
                label: texts.chart_glyphs,
                options: vec![texts.glyph_braille, texts.glyph_blocks, texts.glyph_ascii],
                active: glyph_active,
                actions: vec![
                    Action::ChartGlyphs(ChartGlyphsPreference::Braille),
                    Action::ChartGlyphs(ChartGlyphsPreference::Blocks),
                    Action::ChartGlyphs(ChartGlyphsPreference::Ascii),
                ],
            },
        ],
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
                crate::i18n::fill(
                    texts.alert_rule_fmt,
                    &[("metric", alert_metric_label(&rule.metric))],
                ),
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
    }
}

/// 监控偏好页：分区卡片竖排，整页离屏绘制后按 `settings_scroll`（行）搬进
/// 视口，越界的滚动收回到最后一屏；命中区随之平移，视口外的丢弃。返回页面级
/// 滚动度量：上界是总行数 − 视口高，一屏留一行上下文。
pub(super) fn settings(
    buffer: &mut Buffer,
    area: Rect,
    state: &State,
    palette: &Palette,
    hits: &mut Vec<(Rect, Action)>,
) -> Option<PageScroll> {
    if area.is_empty() {
        return None;
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
    let max = total.saturating_sub(area.height);
    let scroll = u16::try_from(state.settings_scroll)
        .unwrap_or(u16::MAX)
        .min(max);
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
    Some(PageScroll {
        max: usize::from(max),
        screen: usize::from(area.height.saturating_sub(1)).max(1),
    })
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

    /// 带一条告警规则与几个设备的状态。
    fn populated_state() -> State {
        let mut state = preferences_state();
        state.monitor.alerts = crate::config::MonitorConfig::default().alerts;
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
        let state = preferences_state();
        let (buffer, output) = paint_page(&state, Page::Settings, 120, 60);
        let text = buffer_text(&buffer);
        for needle in [
            "监控",
            "采样间隔",
            "1000 ms",
            "卡片高度",
            "历史范围",
            "15 min",
            "图表字形",
            "盲文",
            "方块",
            "ASCII",
            "显示项",
            "CPU 逐核",
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
            |a: &Action| matches!(a, Action::ChartGlyphs(ChartGlyphsPreference::Blocks)),
            |a: &Action| matches!(a, Action::ChartGlyphs(ChartGlyphsPreference::Ascii)),
            |a: &Action| matches!(a, Action::Metric(id) if id == "processes"),
        ] {
            assert!(has(&output, wanted), "缺少控件命中区\n{text}");
        }
        for (rect, action) in &output.hits {
            assert!(
                contains_rect(Rect::new(0, 0, 120, 60), *rect),
                "命中区 {rect:?}（{action:?}）越界"
            );
        }
        // 活动段反色：分段控件的活动项底色是 accent。
        let braille = hit_rect(&output, |a| {
            matches!(a, Action::ChartGlyphs(ChartGlyphsPreference::Braille))
        })
        .expect("盲文段");
        assert_eq!(
            buffer[(braille.x, braille.y)].style().bg,
            Some(config().palette.accent)
        );
    }

    #[test]
    fn alerts_and_devices_get_their_own_cards() {
        let _guard = lang_guard(Lang::ZhCn);
        let state = populated_state();
        let (buffer, output) = paint_page(&state, Page::Settings, 120, 80);
        let text = buffer_text(&buffer);
        for needle in [
            "告警",
            "告警 CPU ≥",
            "告警 内存 ≥",
            "告警 GPU ≥",
            "告警 磁盘 ≥",
            "90%",
            "持续",
            "30 s",
            "冷却",
            "60 s",
            "设备",
            "Test GPU",
            "coretemp Core 0",
        ] {
            assert!(buffer_has(&buffer, needle), "{needle}\n{text}");
        }
        // 冒烟 L11：告警行写界面语言的指标名，不把配置里的原始 id 拼进标签。
        for raw in ["告警 cpu", "告警 memory", "告警 gpu", "告警 disk"] {
            assert!(!buffer_has(&buffer, raw), "{raw}\n{text}");
        }
        assert!(has(&output, |a| matches!(a, Action::AlertThreshold(0, 1))));
        assert!(has(&output, |a| matches!(a, Action::AlertDuration(0, -1))));
        assert!(has(&output, |a| matches!(a, Action::AlertCooldown(0, 1))));
        assert!(has(
            &output,
            |a| matches!(a, Action::Device(id) if id == "gpu0")
        ));
        assert!(has(
            &output,
            |a| matches!(a, Action::Device(id) if id == "sensor:coretemp Core 0")
        ));
    }

    /// 冒烟 L11（英文界面）：告警行同样写指标的显示名。
    #[test]
    fn alert_rows_name_metrics_in_english_too() {
        let _guard = lang_guard(Lang::En);
        let state = populated_state();
        let (buffer, _) = paint_page(&state, Page::Settings, 120, 80);
        let text = buffer_text(&buffer);
        for needle in [
            "Alert CPU ≥",
            "Alert Memory ≥",
            "Alert GPU ≥",
            "Alert Disk ≥",
        ] {
            assert!(buffer_has(&buffer, needle), "{needle}\n{text}");
        }
        assert!(!buffer_has(&buffer, "Alert memory"), "{text}");
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
        assert!(buffer_has(&buffer, "显示项"), "{}", buffer_text(&buffer));
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
