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
    let labels = ClientSettingsSection::ALL
        .iter()
        .map(|section| section.label())
        .collect::<Vec<_>>();
    let nav_rows = super::super::page::navigation_rows(inner.width, &labels);
    let stack = super::super::page::PageLayout::new(inner, nav_rows, false, true);

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
    let mut tab_y = stack.navigation.y;
    let mut tab_hits = Vec::new();
    for section in ClientSettingsSection::ALL {
        let badge = *section == ClientSettingsSection::Integrations && integration_badge;
        let label = if badge {
            format!(" ● {} ", section.label())
        } else {
            format!(" {} ", section.label())
        };
        let width = display_width(&label).min(inner.width);
        if tab_x > inner.x && tab_x.saturating_add(width) > inner.right() {
            tab_x = inner.x;
            tab_y += 1;
        }
        let rect = Rect::new(tab_x, tab_y, width, 1);
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
    }
    put_text(
        buffer,
        inner.x,
        stack.navigation.bottom(),
        inner.width,
        &"─".repeat(inner.width as usize),
        Style::default().fg(palette.surface0).bg(palette.panel_bg),
    );

    let content = stack.content;
    let mut choice_hits = Vec::new();
    let mut scroll = 0;
    match settings.section {
        ClientSettingsSection::Language => {
            render_choice_section(
                buffer,
                content,
                t.language,
                t.language_hint,
                &[t.lang_zh, t.lang_en],
                settings.selected,
                settings.current,
                palette,
                &mut choice_hits,
            );
        }
        ClientSettingsSection::Theme => {
            let visible = usize::from(content.height);
            scroll = super::super::page::list_start(
                settings.scroll,
                settings.selected,
                crate::config::THEME_NAMES.len(),
                visible,
                settings.reveal,
            );
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
                settings.current,
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
                settings.current,
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
                settings.current,
                palette,
                &mut choice_hits,
            );
        }
        ClientSettingsSection::Integrations => {
            scroll = render_integrations(buffer, content, settings, cx);
        }
    }

    // 主按钮的作用域是「安装选中」，可点性就取选中行的判据（TOOL-04）：
    // 别的行待装不该让一个按下去注定无效的主按钮维持可点状态。
    let selected_installable = settings
        .integrations
        .get(settings.selected)
        .is_some_and(super::super::settings::integration_needs_install);
    let show_primary = settings.section != ClientSettingsSection::Integrations
        || settings.installing_integrations
        || selected_installable;
    let primary_label = if settings.installing_integrations {
        t.installing
    } else if settings.section == ClientSettingsSection::Integrations {
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
    let buttons = modal_button_row(stack.actions, &labels, 2);
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
    {
        let footer = stack.footer;
        // `a` 是无确认的批量安装键，只在 Integrations 分区生效，也只在那里
        // 进键位提示。
        let line = if settings.section == ClientSettingsSection::Integrations {
            format!("{}{}", t.footer, t.footer_install_all)
        } else {
            t.footer.to_owned()
        };
        put_text(
            buffer,
            footer.x,
            footer.y,
            footer.width,
            &line,
            Style::default().fg(palette.overlay1).bg(palette.panel_bg),
        );
    }

    Some(OverlayRender {
        area: popup,
        primary,
        cancel: close,
        settings_popup: popup,
        settings_scroll: scroll,
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
    current: usize,
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
    let header_rows = if area.height >= choices.len() as u16 + 3 {
        3
    } else {
        0
    };
    let row_gap = u16::from(area.height >= header_rows + choices.len() as u16 * 2);
    for (index, choice) in choices.iter().enumerate() {
        let y = area.y + header_rows + index as u16 * (1 + row_gap);
        if y >= area.bottom() {
            break;
        }
        let rect = Rect::new(area.x, y, area.width, 1);
        draw_choice(
            buffer,
            rect,
            choice,
            index == selected,
            index == current,
            palette,
        );
        hits.push((rect, index));
    }
}

fn installed_count(settings: &ClientSettingsOverlay) -> usize {
    settings
        .integrations
        .iter()
        .filter(|integration| {
            matches!(
                integration.state,
                crate::api::schema::IntegrationState::Current
                    | crate::api::schema::IntegrationState::Outdated
            )
        })
        .count()
}

fn render_integrations(
    buffer: &mut Buffer,
    area: Rect,
    settings: &ClientSettingsOverlay,
    cx: &super::feedback::ChromeContext<'_>,
) -> usize {
    let p = cx.palette;
    let t = &crate::i18n::texts().settings;
    if settings.loading_integrations || settings.integrations.is_empty() {
        put_text(
            buffer,
            area.x,
            area.y,
            area.width,
            if settings.loading_integrations {
                t.loading
            } else {
                t.no_targets
            },
            Style::default().fg(p.overlay0),
        );
        return 0;
    }
    let area = if area.height >= 4 {
        // 一次性提示（「选中的集成无需安装」）占用说明行，不挤掉下方承载
        // 服务端安装结果的消息区。
        let (line, style) = match settings.integration_notice.as_deref() {
            Some(notice) => (notice.to_owned(), Style::default().fg(p.yellow)),
            None => (
                format!(
                    "{} · ✓ {}/{} · {}",
                    t.integrations,
                    installed_count(settings),
                    settings.integrations.len(),
                    t.integrations_hint
                ),
                Style::default().fg(p.overlay0),
            ),
        };
        put_text(buffer, area.x, area.y, area.width, &line, style);
        Rect::new(area.x, area.y + 1, area.width, area.height - 1)
    } else {
        area
    };
    let count = settings.integrations.len() + settings.integration_messages.len();
    // Fixed status column: labels longer than the old hard-coded 12 columns
    // (antigravity-cli) otherwise pushed their status out of alignment.
    let label_width = settings
        .integrations
        .iter()
        .map(|integration| UnicodeWidthStr::width(integration.label.as_str()))
        .max()
        .unwrap_or(12)
        .clamp(10, 18);
    let scroll = super::super::page::list_start(
        settings.scroll,
        settings.selected,
        count,
        usize::from(area.height),
        settings.reveal,
    );
    for (offset, index) in (scroll..count).take(usize::from(area.height)).enumerate() {
        let rect = Rect::new(area.x, area.y + offset as u16, area.width, 1);
        if let Some(integration) = settings.integrations.get(index) {
            let (marker, color, status) = match integration.state {
                crate::api::schema::IntegrationState::Current => ("✓", p.green, t.state_installed),
                crate::api::schema::IntegrationState::Outdated => {
                    ("↻", p.yellow, t.state_update_available)
                }
                crate::api::schema::IntegrationState::NotInstalled if integration.available => {
                    ("+", p.accent, t.state_available)
                }
                _ => ("–", p.overlay0, t.state_not_found),
            };
            let style = if index == settings.selected {
                choice_style(true, p)
            } else {
                Style::default().fg(color).bg(p.panel_bg)
            };
            buffer.set_style(rect, style);
            put_text(
                buffer,
                rect.x,
                rect.y,
                rect.width,
                &format!(
                    " {marker} {:<width$}  {status}",
                    integration.label,
                    width = label_width
                ),
                style,
            );
        } else if let Some(message) = settings
            .integration_messages
            .get(index - settings.integrations.len())
        {
            put_text(
                buffer,
                rect.x,
                rect.y,
                rect.width,
                message,
                Style::default().fg(p.subtext0),
            );
        }
    }
    scroll
}
