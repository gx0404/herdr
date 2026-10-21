use super::super::super::chrome::settings as chrome;
use super::*;
use chrome::{NavItem, OptionList, OptionRow};

fn navigation(settings: &ClientSettingsOverlay, updates: bool) -> Vec<NavItem<'static>> {
    let badge = updates
        || settings
            .integrations
            .iter()
            .any(|row| row.state == crate::api::schema::IntegrationState::Outdated);
    ClientSettingsSection::ALL
        .iter()
        .map(|section| NavItem {
            section: *section,
            label: section.label(),
            badge: *section == ClientSettingsSection::Integrations && badge,
        })
        .collect()
}

fn section_header(section: ClientSettingsSection) -> (&'static str, &'static str) {
    let t = &crate::i18n::texts().settings;
    match section {
        ClientSettingsSection::Language => (t.language, t.language_hint),
        ClientSettingsSection::Theme => (t.theme, t.theme_hint),
        ClientSettingsSection::Indicators => (t.indicators, t.indicators_hint),
        ClientSettingsSection::Toast => (t.toasts, t.toasts_hint),
        ClientSettingsSection::Sound => (t.sound, t.sound_hint),
        ClientSettingsSection::Integrations => (t.integrations, t.integrations_hint),
        ClientSettingsSection::Keyboard => (t.keyboard, t.keyboard_hint),
        ClientSettingsSection::Monitor => (t.monitor, t.monitor_hint),
    }
}

fn option_rows(settings: &ClientSettingsOverlay) -> Vec<OptionRow<'static>> {
    let t = &crate::i18n::texts().settings;
    let options: Vec<(&str, &str)> = match settings.section {
        ClientSettingsSection::Language => vec![(t.lang_zh, "中文"), (t.lang_en, "English")],
        ClientSettingsSection::Theme => crate::config::THEME_NAMES
            .iter()
            .map(|name| {
                (
                    crate::i18n::theme_display_name(name),
                    if *name == "graphite" {
                        t.theme_adaptive
                    } else {
                        ""
                    },
                )
            })
            .collect(),
        ClientSettingsSection::Indicators => vec![
            (t.indicator_graphite, t.indicator_graphite_detail),
            (t.indicator_dots, t.indicator_dots_detail),
            (t.indicator_symbols, t.indicator_symbols_detail),
        ],
        ClientSettingsSection::Toast => vec![
            (t.toast_off, t.toast_off_detail),
            (t.toast_herdr, t.toast_herdr_detail),
            (t.toast_terminal, t.toast_terminal_detail),
            (t.toast_system, t.toast_system_detail),
        ],
        ClientSettingsSection::Sound => vec![
            (t.sound_on, t.sound_on_detail),
            (t.sound_off, t.sound_off_detail),
        ],
        ClientSettingsSection::Keyboard => vec![(t.keyboard, t.keyboard_hint)],
        ClientSettingsSection::Monitor => vec![
            (t.monitor_preferences, t.monitor_preferences_detail),
            (t.monitor_system, t.monitor_system_detail),
        ],
        ClientSettingsSection::Integrations => vec![],
    };
    options
        .into_iter()
        .enumerate()
        .map(|(index, (label, detail))| {
            let current = if settings.section == ClientSettingsSection::Theme {
                crate::config::THEME_NAMES.get(index).is_some_and(|name| {
                    super::super::settings::normalized_theme_name(name)
                        == super::super::settings::normalized_theme_name(
                            &settings.original_theme_name,
                        )
                })
            } else {
                !matches!(
                    settings.section,
                    ClientSettingsSection::Keyboard | ClientSettingsSection::Monitor
                ) && index == settings.current
            };
            let row = OptionRow::new(label).detail(detail).current(current);
            if matches!(
                settings.section,
                ClientSettingsSection::Keyboard | ClientSettingsSection::Monitor
            ) {
                row.key("enter")
            } else {
                row
            }
        })
        .collect()
}

#[derive(Debug, Clone, Copy)]
pub(in crate::client::shell) struct SettingsListWindow {
    body: Rect,
    rows: usize,
    selected: (usize, usize),
}

impl SettingsListWindow {
    pub(in crate::client::shell) fn page_rows(&self) -> usize {
        usize::from(self.body.height).max(1)
    }

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

pub(in crate::client::shell) fn settings_list_window(
    area: Rect,
    page_bounds: Option<Rect>,
    settings: &ClientSettingsOverlay,
    integration_updates_available: bool,
) -> Option<SettingsListWindow> {
    let outer = page_bounds
        .map(|rect| rect.intersection(area))
        .or_else(|| crate::ui::modal_rect(area, chrome::modal_size(area)))?;
    let layout = chrome::settings_layout(
        super::render::panel_inner(outer)?,
        &navigation(settings, integration_updates_available),
    )?;
    let body = chrome::list_body(layout.content);
    if settings.section == ClientSettingsSection::Integrations {
        let (_, list) = integrations_layout(body);
        let lines = integration_lines(settings, list);
        return Some(SettingsListWindow {
            body: list,
            rows: lines.len(),
            selected: entry_rows(&lines, settings.selected),
        });
    }
    let rows = option_rows(settings).len();
    let gap = chrome::option_gap(body, rows);
    let step = if gap { 2 } else { 1 };
    Some(SettingsListWindow {
        body,
        rows: rows * step,
        selected: (settings.selected * step, 1),
    })
}

pub(super) fn render_settings_overlay(
    buffer: &mut Buffer,
    settings: &ClientSettingsOverlay,
    integration_updates_available: bool,
    cx: &super::feedback::ChromeContext<'_>,
) -> Option<OverlayRender> {
    let palette = cx.palette;
    let t = &crate::i18n::texts().settings;
    let (popup, inner) = modal_panel(buffer, chrome::modal_size(buffer.area), cx)?;
    let nav = navigation(settings, integration_updates_available);
    let layout = chrome::settings_layout(inner, &nav)?;
    let focus = settings.focus;
    let nav_focused = focus == super::super::page::PageFocus::Navigation;
    let content_focused = focus == super::super::page::PageFocus::Content;
    chrome::paint_frame(buffer, &layout, t.title, palette);
    let hovered_section = match cx.hover {
        Some(ChromeHover::SettingsSection(section)) => Some(*section),
        _ => None,
    };
    let tab_hits = chrome::paint_nav(
        buffer,
        &layout,
        &nav,
        settings.section,
        nav_focused,
        hovered_section,
        palette,
        cx.components,
    );
    let (title, description) = section_header(settings.section);
    chrome::paint_section_header(buffer, layout.content, title, description, palette);
    let body = chrome::list_body(layout.content);
    let choice_hits = if settings.section == ClientSettingsSection::Integrations {
        let mut hits = Vec::new();
        render_integrations(buffer, body, settings, cx, &mut hits);
        hits
    } else {
        let rows = option_rows(settings);
        let gap = chrome::option_gap(body, rows.len());
        let hovered = match cx.hover {
            Some(ChromeHover::SettingsChoice(index)) => Some(*index),
            _ => None,
        };
        chrome::paint_options(
            buffer,
            body,
            &OptionList {
                rows: &rows,
                start: settings.scroll / if gap { 2 } else { 1 },
                selected: settings.selected,
                focused: content_focused,
                hovered,
                gap,
            },
            palette,
        )
    };
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
    } else if matches!(
        settings.section,
        ClientSettingsSection::Keyboard | ClientSettingsSection::Monitor
    ) {
        t.open_button
    } else {
        t.apply_button
    };
    let close_label = crate::ui::modal_close_button_text();
    let labels = if show_primary {
        vec![primary_label, close_label]
    } else {
        vec![close_label]
    };
    let buttons = chrome::footer_button_rects(&layout, &labels);
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
                &ChromeHover::OverlayPrimary,
                if focus == super::super::page::PageFocus::Actions {
                    crate::ui::ModalButtonState::Focused
                } else {
                    crate::ui::ModalButtonState::Normal
                },
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
            &ChromeHover::OverlayCancel,
            crate::ui::ModalButtonState::Normal,
        ),
        palette,
    );
    let action_hint = match settings.section {
        ClientSettingsSection::Integrations => t.hint_install,
        ClientSettingsSection::Keyboard | ClientSettingsSection::Monitor => t.hint_open,
        _ => t.hint_apply,
    };
    let mut hints = vec![
        ("←→".into(), t.hint_section.into()),
        ("↑↓".into(), t.hint_select.into()),
        ("enter".into(), action_hint.into()),
    ];
    if settings.section == ClientSettingsSection::Integrations {
        hints.push(("a".into(), t.hint_install_all.into()));
    }
    hints.push((
        "esc".into(),
        if settings.section == ClientSettingsSection::Theme {
            t.hint_cancel_preview
        } else {
            t.hint_close
        }
        .into(),
    ));
    super::super::render_key_hints(
        buffer,
        chrome::footer_hint_area(&layout, &buttons),
        &hints,
        palette,
    );

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
            Style::default().fg(p.subtext0),
        );
        return;
    }
    let (header, list) = integrations_layout(area);
    if let Some(header) = header.map(super::text_inset) {
        // 一次性提示（「选中的集成无需安装」）占用说明行，不挤掉下方承载
        // 服务端安装结果的消息区。
        let (line, color) = match settings.integration_notice.as_deref() {
            Some(notice) => (notice.to_owned(), p.yellow),
            None => (
                format!(
                    "{} · ✓ {}/{} · {}",
                    t.integrations,
                    installed_count(settings),
                    settings.integrations.len(),
                    t.integrations_hint
                ),
                p.subtext0,
            ),
        };
        put_text(
            buffer,
            header.x,
            header.y,
            header.width,
            &chrome::truncate_with_ellipsis(&line, header.width),
            Style::default().fg(color),
        );
    }
    let lines = integration_lines(settings, list);
    let label_width = settings
        .integrations
        .iter()
        .map(|row| display_width(&row.label))
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
                _ => ("–", p.overlay1, t.state_not_found),
            };
            let selected = line.entry == settings.selected
                && settings.focus == super::super::page::PageFocus::Content;
            let hovered = cx.hover == Some(&ChromeHover::SettingsChoice(line.entry));
            chrome::paint_options(
                buffer,
                rect,
                &OptionList {
                    rows: &[OptionRow::new(integration.label.as_str())],
                    start: 0,
                    selected: 0,
                    focused: selected,
                    hovered: hovered.then_some(0),
                    gap: false,
                },
                p,
            );
            let bg = buffer[(rect.x, rect.y)].bg;
            put_text(
                buffer,
                rect.x + 1,
                rect.y,
                1,
                marker,
                Style::default().fg(color).bg(bg),
            );
            let status_x = rect.x + 3 + label_width + 3;
            if status_x < rect.right() {
                put_text(
                    buffer,
                    status_x,
                    rect.y,
                    rect.right() - status_x,
                    status,
                    Style::default().fg(color).bg(bg),
                );
            }
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
        assert_eq!(t.hint_apply, "应用");
        assert!(
            !t.hint_apply.contains("保存"),
            "不能再说保存：{}",
            t.hint_apply
        );
        assert_eq!(t.hint_install, "安装选中");
        assert!(!t.hint_install.contains("应用"));
    }
}
