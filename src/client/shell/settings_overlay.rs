use super::*;

/// 单个字符的显示宽度，栈上编码、不分配（与 `kit::char_width` 同口径）。
fn char_width(ch: char) -> usize {
    let mut bytes = [0u8; 4];
    usize::from(display_width(ch.encode_utf8(&mut bytes)))
}

/// 按显示宽度截断，放不下时以 `…` 收尾（冒烟 M13：集成页头部说明改用这个
/// 而不是 `put_text` 的硬截断）。
fn truncate_with_ellipsis(text: &str, max_width: u16) -> String {
    if usize::from(display_width(text)) <= usize::from(max_width) {
        return text.to_owned();
    }
    if max_width == 0 {
        return String::new();
    }
    if max_width == 1 {
        return "…".to_owned();
    }
    let budget = usize::from(max_width - 1);
    let mut used = 0usize;
    let mut out = String::new();
    for ch in text.chars() {
        let width = char_width(ch);
        if used + width > budget {
            break;
        }
        out.push(ch);
        used += width;
    }
    out.push('…');
    out
}

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
    // 冒烟 M9：当前值标记放在行首 marker 位，与标签里可能出现的样例字符
    // （指示器页 indicator_symbols 的样例串本身就带一个 "✓"）分开，避免
    // 两个 ✓ 挨在一起分不清哪个是"当前值"。
    let current_marker = if current { "✓" } else { " " };
    put_text(
        buffer,
        rect.x,
        rect.y,
        rect.width,
        &format!(" {marker}{current_marker} {label}"),
        style,
    );
}

/// 设置浮层的正文几何与（列表型分区的）行数：视图计算阶段与渲染阶段共用
/// （STATE-04）。非列表分区返回 `None`，滚动位置保持不动。
pub(in crate::client::shell) fn settings_list_window(
    area: Rect,
    page_bounds: Option<Rect>,
    settings: &ClientSettingsOverlay,
) -> Option<(Rect, usize)> {
    let integration_height = 14u16
        .saturating_add(settings.integrations.len().max(1) as u16)
        .saturating_add(settings.integration_messages.len().min(6) as u16);
    let height = if settings.section == ClientSettingsSection::Integrations {
        integration_height.max(22)
    } else {
        22
    };
    let outer = page_bounds
        .map(|rect| rect.intersection(area))
        .or_else(|| crate::ui::modal_rect(area, crate::ui::ModalSize::Large.with_height(height)))?;
    let inner = super::render::panel_inner(outer)?;
    if inner.width < 20 || inner.height < 8 {
        return None;
    }
    let labels = ClientSettingsSection::ALL
        .iter()
        .map(|section| section.label())
        .collect::<Vec<_>>();
    let nav_rows = super::super::page::navigation_rows(inner.width, &labels);
    let layout = super::super::page::PageLayout::new(inner, nav_rows, false, true);
    let count = match settings.section {
        ClientSettingsSection::Theme => crate::config::THEME_NAMES.len(),
        ClientSettingsSection::Integrations => {
            settings.integrations.len() + settings.integration_messages.len()
        }
        _ => return None,
    };
    Some((layout.content, count))
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
    // 焦点指示（TOOL-17）：`focus` 此前只写不读，Tab 在导航/正文/按钮之间移动
    // 时画面没有任何变化。
    let focus = settings.focus;
    let nav_focused = focus == super::super::page::PageFocus::Navigation;
    let content_focused = focus == super::super::page::PageFocus::Content;

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
            let style = Style::default()
                .fg(panel_contrast_fg(palette))
                .bg(palette.accent)
                .add_modifier(Modifier::BOLD);
            if nav_focused {
                style.add_modifier(Modifier::UNDERLINED)
            } else {
                style
            }
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
                content_focused,
                palette,
                &mut choice_hits,
            );
        }
        ClientSettingsSection::Theme => {
            let visible = usize::from(content.height);
            // 窗口起点由视图计算阶段写好的 `scroll` 决定（STATE-04）。
            let scroll = super::super::page::list_start(
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
                    index == settings.selected && content_focused,
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
                content_focused,
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
                content_focused,
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
                content_focused,
                palette,
                &mut choice_hits,
            );
        }
        ClientSettingsSection::Integrations => {
            render_integrations(buffer, content, settings, cx);
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
        // 进键位提示；这个分区的主按钮是「安装选中」而不是「应用」，页脚
        // 用专属的 `footer_integrations` 措辞与头部 `integrations_hint`
        // 对齐，不重用通用 `footer`（冒烟 L7）。
        let line = if settings.section == ClientSettingsSection::Integrations {
            format!("{}{}", t.footer_integrations, t.footer_install_all)
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
        settings_tabs: tab_hits,
        settings_choices: choice_hits,
        ..OverlayRender::default()
    })
}

#[allow(clippy::too_many_arguments)]
fn render_choice_section(
    buffer: &mut Buffer,
    area: Rect,
    title: &str,
    description: &str,
    choices: &[&str],
    selected: usize,
    current: usize,
    focused: bool,
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
    if area.height >= 2 {
        put_text(
            buffer,
            area.x,
            area.y + 1,
            area.width,
            description,
            Style::default().fg(palette.overlay1).bg(palette.panel_bg),
        );
    }
    // 标题与说明无论如何都会画，所以矮区域也得给它们留行；第三行留白只在有余量
    // 时才留。否则第一个选项会跟标题写在同一条线上（TOOL-20）。
    let header_rows = if area.height >= choices.len() as u16 + 3 {
        3
    } else {
        area.height.min(2)
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
            focused && index == selected,
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
        // 冒烟 M13：面板窄、文案长时 `put_text` 硬截断不留提示，改先按显示
        // 宽度截到 `…` 收尾（与其它浮层的省略号风格一致）。
        put_text(
            buffer,
            area.x,
            area.y,
            area.width,
            &truncate_with_ellipsis(&line, area.width),
            style,
        );
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

#[cfg(test)]
mod tests {
    use super::*;

    fn palette_and_components() -> (Palette, crate::app::state::ComponentStyles) {
        let palette = Palette::catppuccin();
        let components = crate::app::state::ComponentStyles::from_palette(&palette);
        (palette, components)
    }

    fn test_cx<'a>(
        palette: &'a Palette,
        components: &'a crate::app::state::ComponentStyles,
    ) -> super::super::feedback::ChromeContext<'a> {
        super::super::feedback::ChromeContext {
            page_bounds: None,
            palette,
            components,
            glyphs: crate::ui::BorderGlyphs::SINGLE,
            hover: None,
            spinner: "◐",
            now: std::time::Instant::now(),
        }
    }

    fn integration(label: &str) -> crate::api::schema::IntegrationInfo {
        crate::api::schema::IntegrationInfo {
            target: crate::api::schema::IntegrationTarget::Codex,
            label: label.to_owned(),
            command: label.to_owned(),
            available: true,
            state: crate::api::schema::IntegrationState::Current,
        }
    }

    fn settings_overlay_with_integrations(palette: &Palette) -> ClientSettingsOverlay {
        ClientSettingsOverlay {
            focus: super::super::page::PageFocus::Content,
            current: 0,
            scroll: 0,
            reveal: true,
            section: ClientSettingsSection::Integrations,
            selected: 0,
            original_theme_name: "catppuccin".to_owned(),
            original_palette: palette.clone(),
            original_components: crate::app::state::ComponentStyles::from_palette(palette),
            integrations: vec![integration("codex"), integration("claude")],
            integration_messages: Vec::new(),
            integration_notice: None,
            loading_integrations: false,
            installing_integrations: false,
        }
    }

    /// 冒烟 M13：面板窄时集成页头部说明要以 `…` 收尾，不能被右边框硬切且
    /// 不留任何截断提示。
    #[test]
    fn render_integrations_header_ends_with_ellipsis_when_too_narrow() {
        let _guard = crate::i18n::lang_guard(crate::i18n::Lang::ZhCn);
        let (palette, components) = palette_and_components();
        let cx = test_cx(&palette, &components);
        let settings = settings_overlay_with_integrations(&palette);
        let area = Rect::new(0, 0, 24, 6);
        let mut buffer = Buffer::empty(area);
        render_integrations(&mut buffer, area, &settings, &cx);
        let header: String = (area.x..area.right())
            .map(|x| buffer[(x, area.y)].symbol().to_string())
            .collect();
        assert!(
            header.trim_end().ends_with('…'),
            "窄面板下头部说明要带省略号收尾：{header:?}"
        );
    }

    /// 冒烟 L7：设置页脚同一个 Enter 键不能一边说「应用」一边说「保存」；
    /// 集成页页脚要跟头部 `integrations_hint` 一致地说「安装」，不是通用
    /// 页脚的「应用」。
    #[test]
    fn footer_wording_is_consistent_between_apply_and_integrations() {
        let _guard = crate::i18n::lang_guard(crate::i18n::Lang::ZhCn);
        let t = &crate::i18n::texts().settings;
        assert!(
            t.footer.contains("应用"),
            "通用页脚应该说应用：{}",
            t.footer
        );
        assert!(!t.footer.contains("保存"), "不能再说保存：{}", t.footer);
        assert!(
            t.footer_integrations.contains("安装"),
            "集成页页脚要跟头部一致说安装：{}",
            t.footer_integrations
        );
        assert!(
            !t.footer_integrations.contains("应用"),
            "集成页页脚不该说应用：{}",
            t.footer_integrations
        );
    }
}
