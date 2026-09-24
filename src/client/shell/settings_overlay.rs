use super::*;

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

/// 设置浮层的尺寸：视图计算与渲染共用（STATE-04）。集成页按集成数与安装消息
/// 折行后的行数加高（消息最多加 6 行，再多就在列表里滚动），其余分区固定 22 行。
/// 浮层宽度与高度无关，先按默认高度取宽度，再按这个宽度算消息折成几行（N13）。
fn settings_modal_size(area: Rect, settings: &ClientSettingsOverlay) -> crate::ui::ModalSize {
    let base = crate::ui::ModalSize::Large.with_height(22);
    if settings.section != ClientSettingsSection::Integrations {
        return base;
    }
    let message_rows = crate::ui::modal_rect(area, base)
        .and_then(super::render::panel_inner)
        .map_or(settings.integration_messages.len(), |inner| {
            integration_lines(settings, integrations_layout(inner).1)
                .iter()
                .filter(|line| line.entry >= settings.integrations.len())
                .count()
        });
    let height = 14u16
        .saturating_add(settings.integrations.len().max(1) as u16)
        .saturating_add(message_rows.min(6) as u16);
    crate::ui::ModalSize::Large.with_height(height.max(22))
}

/// 列表型分区（主题、集成）的滚动口径：列表区矩形、总行数、选中条目占的
/// 行区间（首行下标, 行数）。集成页的安装消息折行后一条可能占多行（N13），
/// 滚动按行计，键盘选中仍按条目计。
#[derive(Debug, Clone, Copy)]
pub(in crate::client::shell) struct SettingsListWindow {
    body: Rect,
    rows: usize,
    selected: (usize, usize),
}

impl SettingsListWindow {
    /// 列表首个可见行：先夹到最后一屏；`reveal` 时再把选中条目整条滚进视野，
    /// 条目比视野还高时保证它的首行可见。
    pub(in crate::client::shell) fn start(&self, requested: usize, reveal: bool) -> usize {
        let height = usize::from(self.body.height).max(1);
        let start = requested.min(self.rows.saturating_sub(height));
        if !reveal {
            return start;
        }
        let (first, span) = self.selected;
        start
            .max(first.saturating_add(span.max(1)).saturating_sub(height))
            .min(first)
    }
}

/// 设置浮层列表型分区的滚动窗口：视图计算阶段与渲染阶段共用同一份几何与
/// 行展开（STATE-04）。非列表分区返回 `None`，滚动位置保持不动。
pub(in crate::client::shell) fn settings_list_window(
    area: Rect,
    page_bounds: Option<Rect>,
    settings: &ClientSettingsOverlay,
) -> Option<SettingsListWindow> {
    let outer = page_bounds
        .map(|rect| rect.intersection(area))
        .or_else(|| crate::ui::modal_rect(area, settings_modal_size(area, settings)))?;
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
    match settings.section {
        ClientSettingsSection::Theme => Some(SettingsListWindow {
            body: layout.content,
            rows: crate::config::THEME_NAMES.len(),
            selected: (settings.selected, 1),
        }),
        ClientSettingsSection::Integrations => {
            let (_, list) = integrations_layout(layout.content);
            let lines = integration_lines(settings, list);
            Some(SettingsListWindow {
                body: list,
                rows: lines.len(),
                selected: entry_rows(&lines, settings.selected),
            })
        }
        _ => None,
    }
}

pub(super) fn render_settings_overlay(
    buffer: &mut Buffer,
    settings: &ClientSettingsOverlay,
    integration_updates_available: bool,
    cx: &super::feedback::ChromeContext<'_>,
) -> Option<OverlayRender> {
    let palette = cx.palette;
    let t = &crate::i18n::texts().settings;
    let size = settings_modal_size(buffer.area, settings);
    let (popup, inner) = modal_panel(buffer, size, palette.accent, cx)?;
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
            render_integrations(buffer, content, settings, cx, &mut choice_hits);
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
    // 标题与说明离边框留 1 列（冒烟 L4）；选项行整行高亮，文字自带前导空格。
    let text = super::text_inset(area);
    put_text(
        buffer,
        text.x,
        text.y,
        text.width,
        title,
        Style::default()
            .fg(palette.text)
            .bg(palette.panel_bg)
            .add_modifier(Modifier::BOLD),
    );
    if area.height >= 2 {
        put_text(
            buffer,
            text.x,
            text.y + 1,
            text.width,
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

/// 集成页正文的几何：说明行（正文至少 4 行时才有）与其下的列表区。视图计算、
/// 渲染与命中区共用这一份（STATE-04）。
fn integrations_layout(content: Rect) -> (Option<Rect>, Rect) {
    if content.height >= 4 {
        (
            Some(Rect::new(content.x, content.y, content.width, 1)),
            Rect::new(content.x, content.y + 1, content.width, content.height - 1),
        )
    } else {
        (None, content)
    }
}

/// 集成页列表的一行（N13）：集成各占一行；服务端返回的安装消息按列表宽度折行，
/// 一条消息可能占多行。`entry` 与键盘选中同一编号（集成在前、消息在后），
/// `start..end` 是这一行在消息里的字节区间（集成行为空区间）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct IntegrationLine {
    entry: usize,
    start: usize,
    end: usize,
    continuation: bool,
}

/// 消息续行比首行多缩进的列数：一眼看出它接着上一行，不是新的一条消息。
const MESSAGE_CONTINUATION_INDENT: u16 = 2;

/// 集成页列表区 `list` 展开成行：消息按离边框留白后的文字宽度折行（冒烟 L4，
/// 与渲染同一口径）。加载中或没有集成时列表区只画一行提示，不列行，滚动归零。
fn integration_lines(settings: &ClientSettingsOverlay, list: Rect) -> Vec<IntegrationLine> {
    if settings.loading_integrations || settings.integrations.is_empty() {
        return Vec::new();
    }
    let width = super::text_inset(list).width;
    let mut lines = (0..settings.integrations.len())
        .map(|entry| IntegrationLine {
            entry,
            start: 0,
            end: 0,
            continuation: false,
        })
        .collect::<Vec<_>>();
    let first = usize::from(width);
    let rest = usize::from(width.saturating_sub(MESSAGE_CONTINUATION_INDENT));
    for (index, message) in settings.integration_messages.iter().enumerate() {
        let entry = settings.integrations.len() + index;
        // 首尾空白不画：首行与其它消息同列起笔。区间换算回原消息里的字节。
        let lead = message.len() - message.trim_start().len();
        wrap_text(message.trim(), first, rest, |start, end| {
            lines.push(IntegrationLine {
                entry,
                start: lead + start,
                end: lead + end,
                continuation: start > 0,
            });
        });
    }
    lines
}

/// 条目 `entry` 占的行区间（首行下标, 行数）；不在列表里（越界或列表为空）时
/// 取最后一行。
fn entry_rows(lines: &[IntegrationLine], entry: usize) -> (usize, usize) {
    match lines.iter().position(|line| line.entry == entry) {
        Some(first) => (
            first,
            lines[first..]
                .iter()
                .take_while(|line| line.entry == entry)
                .count(),
        ),
        None => (lines.len().saturating_sub(1), 1),
    }
}

fn render_integrations(
    buffer: &mut Buffer,
    area: Rect,
    settings: &ClientSettingsOverlay,
    cx: &super::feedback::ChromeContext<'_>,
    hits: &mut Vec<(Rect, usize)>,
) {
    let p = cx.palette;
    let t = &crate::i18n::texts().settings;
    if settings.loading_integrations || settings.integrations.is_empty() {
        // 这两句文案自带前导空格，已与其余正文同列。
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
        return;
    }
    let (header, list) = integrations_layout(area);
    if let Some(header) = header.map(super::text_inset) {
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
            header.x,
            header.y,
            header.width,
            &truncate_with_ellipsis(&line, header.width),
            style,
        );
    }
    let lines = integration_lines(settings, list);
    // Fixed status column: labels longer than the old hard-coded 12 columns
    // (antigravity-cli) otherwise pushed their status out of alignment.
    let label_width = settings
        .integrations
        .iter()
        .map(|integration| UnicodeWidthStr::width(integration.label.as_str()))
        .max()
        .unwrap_or(12)
        .clamp(10, 18);
    // 起点由视图计算阶段写好的 `scroll` 决定（STATE-04），这里只夹到最后一屏。
    let window = SettingsListWindow {
        body: list,
        rows: lines.len(),
        selected: entry_rows(&lines, settings.selected),
    };
    let start = window.start(settings.scroll, false);
    for (offset, line) in lines
        .iter()
        .skip(start)
        .take(usize::from(list.height))
        .enumerate()
    {
        let rect = Rect::new(list.x, list.y + offset as u16, list.width, 1);
        if let Some(integration) = settings.integrations.get(line.entry) {
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
            let style = if line.entry == settings.selected {
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
            // 命中区就是画出来的这一行：滚出视野的集成行不留命中区（N13）。
            hits.push((rect, line.entry));
        } else if let Some(text) = settings
            .integration_messages
            .get(line.entry - settings.integrations.len())
            .and_then(|message| message.get(line.start..line.end))
        {
            // 消息与集成行的标记同列起笔（离边框 1 列），续行再缩进。
            let area = super::text_inset(rect);
            let indent = if line.continuation {
                MESSAGE_CONTINUATION_INDENT.min(area.width)
            } else {
                0
            };
            put_text(
                buffer,
                area.x + indent,
                area.y,
                area.width - indent,
                text,
                Style::default().fg(p.subtext0),
            );
        }
    }
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
        render_integrations(&mut buffer, area, &settings, &cx, &mut Vec::new());
        let header: String = (area.x..area.right())
            .map(|x| buffer[(x, area.y)].symbol().to_string())
            .collect();
        assert!(
            header.trim_end().ends_with('…'),
            "窄面板下头部说明要带省略号收尾：{header:?}"
        );
    }

    fn wrapped(text: &str, first: usize, rest: usize) -> Vec<&str> {
        let mut lines = Vec::new();
        wrap_text(text, first, rest, |start, end| {
            lines.push(&text[start..end])
        });
        lines
    }

    /// N13：安装消息折行优先断在空格后，长路径按列硬断，续行按续行宽度折；
    /// CJK 按显示宽度计，换行符强制断行，空消息也占一行。
    #[test]
    fn wrap_text_prefers_spaces_and_hard_breaks_long_paths() {
        assert_eq!(
            wrapped("start opencode2 once, then reinstall", 16, 14),
            vec!["start opencode2 ", "once, then ", "reinstall"]
        );
        assert_eq!(
            wrapped("to /home/user/.config/opencode/x.js", 12, 10),
            vec!["to ", "/home/user", "/.config/o", "pencode/x.", "js"]
        );
        assert_eq!(
            wrapped("安装完成请重启", 6, 4),
            vec!["安装完", "成请", "重启"]
        );
        assert_eq!(wrapped("first\nsecond", 20, 18), vec!["first", "second"]);
        assert_eq!(wrapped("", 10, 8), vec![""]);
        assert_eq!(wrapped("fits", 10, 8), vec!["fits"]);
    }

    /// 审查（轻）：溢出的恰好是空格时就在它前面断、空格不带到下一行（此前续行多
    /// 缩进 1 列，后接超宽长路径时还会单出一行空白）；整词恰好填满一行时，其后
    /// 的空格不再让这一行提前断开；连续空格在断行处一并跳过。
    #[test]
    fn wrap_text_breaks_before_an_overflowing_space() {
        assert_eq!(
            wrapped("abcdefghij klmnopqrstu", 10, 8),
            vec!["abcdefghij", "klmnopqr", "stu"]
        );
        assert_eq!(
            wrapped("start opencode2 once", 15, 13),
            vec!["start opencode2", "once"]
        );
        assert_eq!(wrapped("abcde   fgh", 5, 5), vec!["abcde", "fgh"]);
    }

    /// 审查（轻）：以空白开头或结尾的安装消息画出来与其它消息同列起笔——折行前
    /// 先去掉首尾空白，行区间仍指向原消息里的字节。
    #[test]
    fn integration_lines_trim_messages_before_wrapping() {
        let palette = Palette::catppuccin();
        let mut settings = settings_overlay_with_integrations(&palette);
        settings.integration_messages = vec!["  indented message  ".to_owned()];
        let lines = integration_lines(&settings, Rect::new(0, 0, 40, 10));
        let texts = lines
            .iter()
            .filter(|line| line.entry >= settings.integrations.len())
            .map(|line| &settings.integration_messages[0][line.start..line.end])
            .collect::<Vec<_>>();
        assert_eq!(texts, vec!["indented message"]);
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
