//! 宽屏机器工作台：左侧选择，右侧详情与动作；窄屏沿用分步导航。

use super::*;
use crate::client::shell::page::{action_grid, action_row_count, list_start, PageLayout};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget, Wrap};

// 参数与现有机器页渲染入口一致，集中在一次投影中避免从全局重新查状态。
#[allow(clippy::too_many_arguments)]
pub(super) fn render_dashboard(
    b: &mut Buffer,
    overlay: &ClientMachinesOverlay,
    endpoints: &[ClientShellEndpoint],
    profiles: &[SavedSshEndpoint],
    errors: &HashMap<ClientEndpointId, crate::remote::ConnectionErrorKind>,
    forwards: &HashMap<ClientEndpointId, Vec<crate::remote::PortForwardStatus>>,
    log_dropped: &HashMap<ProfileId, u64>,
    cx: &super::super::feedback::ChromeContext<'_>,
) -> Option<OverlayRender> {
    let p = cx.palette;
    let t = &crate::i18n::texts().machines;
    let (popup, inner) = modal_panel(
        b,
        crate::ui::ModalSize::Content {
            width: 116,
            height: 34,
        },
        p.accent,
        cx,
    )?;
    if inner.height < 8 || inner.width < 32 {
        return Some(OverlayRender {
            area: popup,
            machines_popup: popup,
            ..OverlayRender::default()
        });
    }
    // 页脚键表要先算出来：它的行数决定 `PageLayout` 给 footer 留几行
    // （HERDR-MACH-007 的键表在单行里必然被尾部截断）。
    let rows = machine_list_rows(profiles, endpoints, overlay.query.as_str());
    let selected = overlay.selected.min(rows.len().saturating_sub(1));
    let has_review = rows.get(selected).is_some_and(|row| {
        errors
            .get(&ClientEndpointId::Ssh(row.id.clone()))
            .is_some_and(super::super::machine_auth_overlay::failure_kind_has_review)
    });
    let hints = super::machine_list_hints(!rows.is_empty(), has_review);
    let layout = PageLayout::with_footer_rows(
        inner,
        0,
        true,
        1,
        super::machine_footer_rows(&hints, inner.width),
    );
    put_text(
        b,
        layout.header.x,
        layout.header.y,
        layout.header.width,
        t.title,
        Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
    );
    let cursor = render_search_bar(
        b,
        layout.search,
        &SearchBar {
            focused: overlay.search_focused,
            query: &overlay.query,
            hint: t.search_hint,
            status: None,
            echo_query: true,
            count: Some(rows.len().to_string()),
        },
        p,
    );
    let left_width = (layout.content.width / 3).clamp(24, 36);
    let left = Rect::new(
        layout.content.x,
        layout.content.y,
        left_width,
        layout.content.height,
    );
    let right = Rect::new(
        left.right().saturating_add(2),
        left.y,
        layout.content.width.saturating_sub(left_width + 2),
        left.height,
    );
    for y in left.y..left.bottom() {
        put_text(b, left.right(), y, 1, "│", Style::default().fg(p.surface1));
    }
    let count = usize::from(left.height / 2);
    let scroll = list_start(overlay.scroll, selected, rows.len(), count, overlay.reveal);
    let mut row_hits = Vec::new();
    for (index, row) in rows.iter().enumerate().skip(scroll).take(count) {
        let rect = Rect::new(
            left.x,
            left.y + ((index - scroll) * 2) as u16,
            left.width.saturating_sub(1),
            2,
        );
        let style = if index == selected {
            Style::default().fg(panel_contrast_fg(p)).bg(p.accent)
        } else {
            Style::default().fg(p.text).bg(p.panel_bg)
        };
        b.set_style(rect, style);
        let (glyph, state, color) = endpoint_status_presentation(row.status, p, cx.spinner);
        let signal = format!("{glyph} {state}");
        let signal_width = display_width(&signal).min(rect.width / 2);
        put_text(
            b,
            rect.x,
            rect.y,
            rect.width.saturating_sub(signal_width + 1),
            &format!(" {}", row.label),
            style.add_modifier(Modifier::BOLD),
        );
        put_text(
            b,
            rect.right().saturating_sub(signal_width),
            rect.y,
            signal_width,
            &signal,
            if index == selected {
                style
            } else {
                style.fg(color)
            },
        );
        let version = row
            .server_version
            .as_ref()
            .map(|version| format!("v{version}"))
            .unwrap_or_default();
        let version_width = display_width(&version).min(rect.width / 3);
        put_text(
            b,
            rect.x,
            rect.y + 1,
            rect.width
                .saturating_sub(version_width + u16::from(version_width > 0)),
            &row.target,
            if index == selected {
                style
            } else {
                style.fg(p.overlay0)
            },
        );
        if version_width > 0 {
            put_text(
                b,
                rect.right() - version_width,
                rect.y + 1,
                version_width,
                &version,
                if index == selected {
                    style
                } else {
                    style.fg(p.overlay0)
                },
            );
        }
        row_hits.push((rect, row.id.clone()));
    }
    let mut action_hits = Vec::new();
    let mut detail_max_scroll = 0;
    if let Some(profile) = rows
        .get(selected)
        .and_then(|row| profiles.iter().find(|profile| profile.id == row.id))
    {
        let endpoint = endpoint_for(endpoints, &profile.id);
        let endpoint_id = ClientEndpointId::Ssh(profile.id.clone());
        let mut buttons = vec![
            (t.reconnect_button, MachineOverlayButton::Reconnect),
            (t.edit_button, MachineOverlayButton::Edit),
            (t.browse_files_button, MachineOverlayButton::BrowseFiles),
            (t.forwards_button, MachineOverlayButton::Forwards),
            (
                if profile.enabled {
                    t.disable_button
                } else {
                    t.enable_button
                },
                MachineOverlayButton::ToggleEnabled,
            ),
            (t.remove_button, MachineOverlayButton::Remove),
        ];
        if has_review {
            buttons.insert(
                0,
                (
                    crate::i18n::texts().machine_auth.review_button,
                    MachineOverlayButton::ReviewIssue,
                ),
            );
        }
        let labels = buttons.iter().map(|(label, _)| *label).collect::<Vec<_>>();
        let details =
            PageLayout::with_action_rows(right, 0, false, action_row_count(right.width, &labels));
        put_text(
            b,
            details.header.x,
            details.header.y,
            details.header.width,
            &profile.label,
            Style::default().fg(p.text).add_modifier(Modifier::BOLD),
        );
        let mut lines = detail_lines(
            profile,
            endpoint,
            forwards.get(&endpoint_id).map(Vec::as_slice),
            log_dropped.get(&profile.id).copied(),
        )
        .into_iter()
        .filter(|(label, _)| {
            label != t.detail_id && (endpoint.is_none() || label != t.detail_enabled)
        })
        .map(|(label, value)| {
            let line = Line::from(vec![
                Span::styled(format!("{label}  "), Style::default().fg(p.overlay0)),
                Span::styled(value, Style::default().fg(p.text)),
            ]);
            (line.width(), line)
        })
        .collect::<Vec<_>>();
        if let Some(error) = errors.get(&endpoint_id) {
            let (label, next) =
                super::super::machine_auth_overlay::failure_kind_presentation(error);
            for (index, (text, color)) in [(label, p.red), (next, p.yellow)].into_iter().enumerate()
            {
                let line = Line::styled(text.to_owned(), Style::default().fg(color));
                lines.insert(index, (line.width(), line));
            }
        }
        let metrics = crate::ui::display_lines_scroll_metrics(
            &lines,
            overlay.detail_scroll.min(u16::MAX as usize) as u16,
            details.content,
        );
        detail_max_scroll = metrics.max_offset_from_bottom;
        Paragraph::new(lines.into_iter().map(|(_, line)| line).collect::<Vec<_>>())
            .wrap(Wrap { trim: false })
            .scroll((overlay.detail_scroll.min(detail_max_scroll) as u16, 0))
            .render(details.content, b);
        for (rect, (label, action)) in action_grid(details.actions, &labels)
            .into_iter()
            .zip(buttons)
        {
            let disabled = action == MachineOverlayButton::Reconnect && !profile.enabled;
            modal_button(
                b,
                rect,
                label,
                if action == MachineOverlayButton::Remove {
                    crate::ui::ModalButtonTone::Danger
                } else {
                    crate::ui::ModalButtonTone::Secondary
                },
                cx.button_state(
                    &super::super::feedback::ChromeHover::MachineButton(action),
                    if disabled {
                        crate::ui::ModalButtonState::Disabled
                    } else {
                        crate::ui::ModalButtonState::Normal
                    },
                ),
                p,
            );
            if !disabled {
                action_hits.push((rect, action));
            }
        }
    } else {
        put_text(
            b,
            right.x,
            right.y,
            right.width,
            t.empty,
            Style::default().fg(p.text),
        );
        put_text(
            b,
            right.x,
            right.y.saturating_add(1),
            right.width,
            t.empty_hint,
            Style::default().fg(p.overlay0),
        );
    }
    let labels = [
        t.add_button,
        t.import_button,
        crate::ui::modal_close_button_text(),
    ];
    for (rect, (label, action)) in
        action_grid(layout.actions, &labels)
            .into_iter()
            .zip(labels.into_iter().zip([
                MachineOverlayButton::Add,
                MachineOverlayButton::Import,
                MachineOverlayButton::Close,
            ]))
    {
        modal_button(
            b,
            rect,
            label,
            if action == MachineOverlayButton::Add {
                crate::ui::ModalButtonTone::Primary
            } else {
                crate::ui::ModalButtonTone::Secondary
            },
            crate::ui::ModalButtonState::Normal,
            p,
        );
        action_hits.push((rect, action));
    }
    // 页脚与动作网格同一套键：网格里有的按钮，页脚就有它的键
    // （HERDR-MACH-007）。
    render_key_hints(b, layout.footer, &hints, p, cx.components);
    Some(OverlayRender {
        area: popup,
        machines_popup: popup,
        machines_search: layout.search,
        machines_rows: row_hits,
        machines_actions: action_hits,
        machines_detail_area: right,
        machines_scroll: scroll,
        machines_max_scroll: detail_max_scroll,
        machines_toast: layout.footer,
        cursor,
        ..OverlayRender::default()
    })
}
