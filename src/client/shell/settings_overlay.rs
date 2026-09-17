use super::*;

fn choice_style(selected: bool, palette: &Palette) -> Style {
    if selected {
        Style::default()
            .fg(panel_contrast_fg(palette))
            .bg(palette.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(palette.text).bg(palette.panel_bg)
    }
}

fn draw_choice(
    buffer: &mut Buffer,
    rect: Rect,
    label: &str,
    selected: bool,
    current: bool,
    palette: &Palette,
) {
    let style = choice_style(selected, palette);
    buffer.set_style(rect, style);
    let marker = if selected { "▸" } else { " " };
    let current = if current { " ✓" } else { "" };
    put_text(
        buffer,
        rect.x,
        rect.y,
        rect.width,
        &format!(" {marker} {label}{current}"),
        style,
    );
}

pub(super) fn render_settings_overlay(
    buffer: &mut Buffer,
    settings: &ClientSettingsOverlay,
    integration_updates_available: bool,
    cx: &super::feedback::ChromeContext<'_>,
) -> Option<OverlayRender> {
    let palette = cx.palette;
    let integration_height = 14u16
        .saturating_add(settings.integrations.len().max(1) as u16)
        .saturating_add(settings.integration_messages.len().min(6) as u16);
    let height = if settings.section == ClientSettingsSection::Integrations {
        integration_height.max(22)
    } else {
        22
    };
    let t = &crate::i18n::texts().settings;
    let (popup, inner) = modal_panel(
        buffer,
        crate::ui::ModalSize::Large.with_height(height),
        palette.accent,
        cx,
    )?;
    if inner.width < 20 || inner.height < 8 {
        return None;
    }
    let stack = crate::ui::modal_stack_areas(inner, 2, 1, 1, 1);

    put_text(
        buffer,
        stack.header.x,
        stack.header.y,
        stack.header.width,
        t.title,
        Style::default()
            .fg(palette.text)
            .bg(palette.panel_bg)
            .add_modifier(Modifier::BOLD),
    );

    let integration_badge = integration_updates_available
        || settings
            .integrations
            .iter()
            .any(|integration| integration.state == crate::api::schema::IntegrationState::Outdated);
    let mut tab_x = inner.x;
    let mut tab_hits = Vec::new();
    for section in ClientSettingsSection::ALL {
        let badge = *section == ClientSettingsSection::Integrations && integration_badge;
        let label = if badge {
            format!(" ● {} ", section.label())
        } else {
            format!(" {} ", section.label())
        };
        let width = display_width(&label).min(inner.right().saturating_sub(tab_x));
        let rect = Rect::new(tab_x, stack.header.y + 1, width, 1);
        let active = *section == settings.section;
        let style = if active {
            Style::default()
                .fg(panel_contrast_fg(palette))
                .bg(palette.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(palette.overlay1).bg(palette.panel_bg)
        };
        buffer.set_style(rect, style);
        put_text(buffer, rect.x, rect.y, rect.width, &label, style);
        if badge && !active {
            put_text(
                buffer,
                rect.x.saturating_add(1),
                rect.y,
                rect.width.saturating_sub(1).min(2),
                "● ",
                Style::default()
                    .fg(palette.accent)
                    .bg(palette.panel_bg)
                    .add_modifier(Modifier::BOLD),
            );
        }
        tab_hits.push((rect, *section));
        tab_x = tab_x.saturating_add(width.saturating_add(1));
        if tab_x >= inner.right() {
            break;
        }
    }
    put_text(
        buffer,
        inner.x,
        stack.header.bottom(),
        inner.width,
        &"─".repeat(inner.width as usize),
        Style::default().fg(palette.surface0).bg(palette.panel_bg),
    );

    let content = stack.content;
    let mut choice_hits = Vec::new();
    match settings.section {
        ClientSettingsSection::Language => {
            render_choice_section(
                buffer,
                content,
                t.language,
                t.language_hint,
                &[t.lang_zh, t.lang_en],
                settings.selected,
                palette,
                &mut choice_hits,
            );
        }
        ClientSettingsSection::Theme => {
            let visible = usize::from(content.height);
            let scroll = settings.selected.saturating_sub(visible.saturating_sub(1));
            for (visible_index, (index, name)) in crate::config::THEME_NAMES
                .iter()
                .enumerate()
                .skip(scroll)
                .take(visible)
                .enumerate()
            {
                let rect = Rect::new(
                    content.x,
                    content.y + visible_index as u16,
                    content.width,
                    1,
                );
                draw_choice(
                    buffer,
                    rect,
                    crate::i18n::theme_display_name(name),
                    index == settings.selected,
                    super::super::settings::normalized_theme_name(name)
                        == super::super::settings::normalized_theme_name(
                            &settings.original_theme_name,
                        ),
                    palette,
                );
                choice_hits.push((rect, index));
            }
        }
        ClientSettingsSection::Indicators => {
            render_choice_section(
                buffer,
                content,
                t.indicators,
                t.indicators_hint,
                &[t.indicator_dots, t.indicator_symbols],
                settings.selected,
                palette,
                &mut choice_hits,
            );
        }
        ClientSettingsSection::Sound => {
            render_choice_section(
                buffer,
                content,
                t.sound,
                t.sound_hint,
                &[t.sound_on, t.sound_off],
                settings.selected,
                palette,
                &mut choice_hits,
            );
        }
        ClientSettingsSection::Toast => {
            render_choice_section(
                buffer,
                content,
                t.toasts,
                t.toasts_hint,
                &[t.toast_off, t.toast_herdr, t.toast_terminal, t.toast_system],
                settings.selected,
                palette,
                &mut choice_hits,
            );
        }
        ClientSettingsSection::Integrations => {
            render_integrations(buffer, content, settings, cx);
        }
    }

    let installable = settings
        .integrations
        .iter()
        .any(super::super::settings::integration_needs_install);
    let show_primary = settings.section != ClientSettingsSection::Integrations || installable;
    let primary_label = if settings.section == ClientSettingsSection::Integrations {
        t.install_button
    } else {
        t.apply_button
    };
    let close_label = crate::ui::modal_close_button_text();
    let labels: Vec<&str> = if show_primary {
        vec![primary_label, close_label]
    } else {
        vec![close_label]
    };
    let buttons = modal_button_row(stack.actions.unwrap_or_default(), &labels, 2);
    let (primary, close) = match buttons.as_slice() {
        [primary, close] => (*primary, *close),
        [close] => (Rect::default(), *close),
        _ => return None,
    };
    if show_primary {
        modal_button(
            buffer,
            primary,
            primary_label,
            crate::ui::ModalButtonTone::Primary,
            cx.button_state(
                &super::feedback::ChromeHover::OverlayPrimary,
                crate::ui::ModalButtonState::Focused,
            ),
            palette,
        );
    }
    modal_button(
        buffer,
        close,
        close_label,
        crate::ui::ModalButtonTone::Secondary,
        cx.button_state(
            &super::feedback::ChromeHover::OverlayCancel,
            crate::ui::ModalButtonState::Normal,
        ),
        palette,
    );
    if let Some(footer) = stack.footer {
        put_text(
            buffer,
            footer.x,
            footer.y,
            footer.width,
            t.footer,
            Style::default().fg(palette.overlay1).bg(palette.panel_bg),
        );
    }

    Some(OverlayRender {
        area: popup,
        primary,
        cancel: close,
        settings_popup: popup,
        settings_tabs: tab_hits,
        settings_choices: choice_hits,
        ..OverlayRender::default()
    })
}

fn render_choice_section(
    buffer: &mut Buffer,
    area: Rect,
    title: &str,
    description: &str,
    choices: &[&str],
    selected: usize,
    palette: &Palette,
    hits: &mut Vec<(Rect, usize)>,
) {
    put_text(
        buffer,
        area.x,
        area.y,
        area.width,
        title,
        Style::default()
            .fg(palette.text)
            .bg(palette.panel_bg)
            .add_modifier(Modifier::BOLD),
    );
    put_text(
        buffer,
        area.x,
        area.y + 1,
        area.width,
        description,
        Style::default().fg(palette.overlay1).bg(palette.panel_bg),
    );
    let row_gap = u16::from(choices.len() > 2);
    for (index, choice) in choices.iter().enumerate() {
        let y = area.y + 3 + index as u16 * (1 + row_gap);
        if y >= area.bottom() {
            break;
        }
        let rect = Rect::new(area.x, y, area.width, 1);
        draw_choice(buffer, rect, choice, index == selected, false, palette);
        hits.push((rect, index));
    }
}

fn render_integrations(
    buffer: &mut Buffer,
    area: Rect,
    settings: &ClientSettingsOverlay,
    cx: &super::feedback::ChromeContext<'_>,
) {
    let palette = cx.palette;
    let t = &crate::i18n::texts().settings;
    put_text(
        buffer,
        area.x,
        area.y,
        area.width,
        t.integrations,
        Style::default()
            .fg(palette.text)
            .bg(palette.panel_bg)
            .add_modifier(Modifier::BOLD),
    );
    put_text(
        buffer,
        area.x,
        area.y + 1,
        area.width,
        t.integrations_hint,
        Style::default().fg(palette.overlay1).bg(palette.panel_bg),
    );
    if settings.loading_integrations {
        put_text(
            buffer,
            area.x,
            area.y + 3,
            area.width,
            &format!("{} {}", cx.spinner, t.loading),
            Style::default().fg(palette.overlay1).bg(palette.panel_bg),
        );
        return;
    }
    if settings.integrations.is_empty() {
        put_text(
            buffer,
            area.x,
            area.y + 3,
            area.width,
            t.no_targets,
            Style::default().fg(palette.overlay1).bg(palette.panel_bg),
        );
        return;
    }
    for (index, integration) in settings.integrations.iter().enumerate() {
        let y = area.y + 3 + index as u16;
        if y >= area.bottom() {
            break;
        }
        let (marker, color, status) = match integration.state {
            crate::api::schema::IntegrationState::Current => {
                ("✓", palette.green, t.state_installed)
            }
            crate::api::schema::IntegrationState::Outdated => {
                ("↻", palette.yellow, t.state_update_available)
            }
            crate::api::schema::IntegrationState::NotInstalled if integration.available => {
                ("+", palette.accent, t.state_available)
            }
            crate::api::schema::IntegrationState::NotInstalled => {
                ("–", palette.overlay0, t.state_not_found)
            }
        };
        put_text(
            buffer,
            area.x,
            y,
            3,
            &format!(" {marker}"),
            Style::default().fg(color).bg(palette.panel_bg),
        );
        put_text(
            buffer,
            area.x + 3,
            y,
            11.min(area.width.saturating_sub(3)),
            &format!("{:<9}", integration.label),
            Style::default().fg(palette.subtext0).bg(palette.panel_bg),
        );
        put_text(
            buffer,
            area.x + 14,
            y,
            area.width.saturating_sub(14),
            status,
            Style::default().fg(palette.overlay1).bg(palette.panel_bg),
        );
    }
    let message_y = area
        .y
        .saturating_add(4)
        .saturating_add(settings.integrations.len() as u16);
    for (offset, message) in settings.integration_messages.iter().take(6).enumerate() {
        let y = message_y.saturating_add(offset as u16);
        if y >= area.bottom() {
            break;
        }
        put_text(
            buffer,
            area.x,
            y,
            area.width,
            &format!(" {message}"),
            Style::default().fg(palette.overlay1).bg(palette.panel_bg),
        );
    }
    if settings.installing_integrations && message_y < area.bottom() {
        put_text(
            buffer,
            area.x,
            message_y,
            area.width,
            &format!("{} {}", cx.spinner, t.installing),
            Style::default().fg(palette.overlay1).bg(palette.panel_bg),
        );
    }
}
