use super::*;

#[path = "../shell/overlays.rs"]
mod overlays;
#[path = "../shell/sidebar.rs"]
pub(in crate::client::shell) mod sidebar;
#[path = "../shell/tabs.rs"]
mod tabs;

pub(super) use super::agent_sidebar::{ordered_agent_pane_ids, render_agent_panel};
pub(super) use super::aggregate_navigation::navigator_rows as client_navigator_rows;
pub(in crate::client::shell) use overlays::{
    modal_button, modal_button_row, modal_panel, panel, panel_inner, render_search_bar,
    scrollback_overlay_layout, titled_panel, OverlayRender, SearchBar,
};
pub(super) use overlays::{render_client_overlay, render_context_menu, render_minimum_overlay};
pub(super) use sidebar::{render_collapsed_sidebar, render_sidebar, workspace_entries};
pub(super) use tabs::{render_tab_bar, render_tab_strip, tab_bar_status_width, TabStripContext};

pub(in crate::client::shell) fn render_sidebar_background(
    buffer: &mut Buffer,
    area: Rect,
    palette: &Palette,
    divider_hovered: bool,
) {
    buffer.set_style(area, Style::default().bg(palette.sidebar_bg));
    let separator_x = area.right().saturating_sub(1);
    let divider_color = if divider_hovered {
        palette.overlay1
    } else {
        palette.surface_dim
    };
    for y in area.y..area.bottom() {
        if let Some(cell) = buffer.cell_mut((separator_x, y)) {
            cell.set_symbol("│");
            cell.set_style(Style::default().fg(divider_color));
        }
    }
}

/// 一段键提示在 `render_key_hints` 里占用的列宽：键帽（左右各一个空格）+
/// 一个间隔 + 文案，非末项再加两列分隔。页脚要在绘制前判断能否放下时必须
/// 用这个函数，避免两处各写一份宽度公式。
fn key_hint_segment_width(key: &str, label: &str, last: bool) -> u16 {
    display_width(key)
        .saturating_add(2)
        .saturating_add(1)
        .saturating_add(display_width(label))
        .saturating_add(if last { 0 } else { 2 })
}

/// 一组键提示按 `width` 列排版需要的行数（0 行表示放不下任何一项）。
pub(super) fn key_hints_rows(hints: &[(String, String)], width: u16) -> u16 {
    if width == 0 || hints.is_empty() {
        return 0;
    }
    let mut rows = 1u16;
    let mut used = 0u16;
    for (index, (key, label)) in hints.iter().enumerate() {
        let last = index + 1 == hints.len();
        let segment = key_hint_segment_width(key, label, last);
        if used.saturating_add(segment) > width {
            if used == 0 {
                // 单项就超宽：再换行也放不下，按当前行计。
                return rows;
            }
            rows = rows.saturating_add(1);
            used = 0;
        }
        used = used.saturating_add(segment);
    }
    rows
}

/// Keycap-style shortcut footer: each hint renders as a padded key cap
/// (accent on surface0) followed by its description in the muted base color.
/// 放不下的提示整项换行；`area` 的行用完后剩余项整项丢弃，并在末尾画省略号
/// 标记截断。Callers pass hints resolved from the live keybind config so
/// the footer stays documentation generated from bindings.
pub(super) fn render_key_hints(
    buffer: &mut Buffer,
    area: Rect,
    hints: &[(String, String)],
    palette: &Palette,
    components: &crate::app::state::ComponentStyles,
) {
    if area.is_empty() {
        return;
    }
    let base = Style::default().fg(palette.overlay0).bg(palette.panel_bg);
    let cap = Style::default()
        .fg(components.mode_bar_accent)
        .bg(palette.surface0)
        .add_modifier(Modifier::BOLD);
    let mut x = area.x;
    let mut y = area.y;
    let end = area.right();
    let bottom = area.bottom();
    let mut truncated = false;
    for (index, (key, label)) in hints.iter().enumerate() {
        let cap_width = display_width(key).saturating_add(2);
        let label_width = display_width(label);
        let separator = if index + 1 == hints.len() { 0 } else { 2 };
        let segment = key_hint_segment_width(key, label, index + 1 == hints.len());
        if x.saturating_add(segment) > end {
            // 多行页脚（`area.height > 1`）先换行再放弃；单行页脚行为不变。
            if x > area.x && y.saturating_add(1) < bottom {
                y = y.saturating_add(1);
                x = area.x;
            } else {
                truncated = true;
                break;
            }
        }
        if x.saturating_add(segment) > end {
            truncated = true;
            break;
        }
        let cap_text = format!(" {key} ");
        put_text(buffer, x, y, cap_width.min(end - x), &cap_text, cap);
        x = x.saturating_add(cap_width).saturating_add(1);
        put_text(buffer, x, y, label_width.min(end - x), label, base);
        x = x.saturating_add(label_width).saturating_add(separator);
    }
    if truncated && x < end {
        put_text(buffer, x, y, end - x, "…", base);
    }
}

pub(super) fn render_mode_bar(
    buffer: &mut Buffer,
    pane_area: Rect,
    mode: ClientShellMode,
    copy_mode: Option<&ClientCopyModeState>,
    endpoint_error: Option<&str>,
    update_available: bool,
    broadcast_count: Option<usize>,
    keybinds: &LiveKeybindConfig,
    palette: &Palette,
    components: &crate::app::state::ComponentStyles,
) -> Option<Rect> {
    if (mode == ClientShellMode::Terminal && endpoint_error.is_none() && broadcast_count.is_none())
        || pane_area.is_empty()
    {
        return None;
    }

    let bar = Rect::new(
        pane_area.x,
        pane_area.y + pane_area.height.saturating_sub(1),
        pane_area.width,
        1,
    );
    let base = Style::default().fg(palette.overlay0).bg(palette.panel_bg);
    for x in bar.x..bar.x + bar.width {
        buffer[(x, bar.y)].set_symbol(" ").set_style(base);
    }

    let key = Style::default()
        .fg(components.mode_bar_accent)
        .bg(palette.panel_bg)
        .add_modifier(Modifier::BOLD);
    let mode_style = Style::default()
        .fg(match palette.panel_bg {
            ratatui::style::Color::Reset => palette.surface_dim,
            color => color,
        })
        .bg(components.mode_bar_accent)
        .add_modifier(Modifier::BOLD);
    // The broadcast badge is the loudest element on the bar: input fans out
    // to other machines while it shows, so it takes the warning color and
    // renders even in plain Terminal mode.
    let broadcast_style = Style::default()
        .fg(match palette.panel_bg {
            ratatui::style::Color::Reset => palette.surface_dim,
            color => color,
        })
        .bg(palette.peach)
        .add_modifier(Modifier::BOLD);
    let broadcast_badge = broadcast_count.map(|count| {
        crate::i18n::fill(
            crate::i18n::texts().mode_bar.broadcast_fmt,
            &[("count", &count.to_string())],
        )
    });
    let broadcast_width = broadcast_badge.as_deref().map(display_width).unwrap_or(0);
    if let Some(badge) = broadcast_badge.as_deref() {
        buffer.set_stringn(bar.x, bar.y, badge, usize::from(bar.width), broadcast_style);
    }
    let prefix = crate::config::format_key_combo(keybinds.prefix);
    let prefix_rhs = |bindings: &crate::config::ActionKeybinds| {
        bindings
            .prefix_rhs_label()
            .unwrap_or_else(|| crate::i18n::texts().keybinds.unset.to_owned())
    };
    let navigate_label = |bindings: &crate::config::ActionKeybinds| {
        bindings
            .label()
            .unwrap_or_else(|| crate::i18n::texts().keybinds.unset.to_owned())
    };

    let mode_bar = &crate::i18n::texts().mode_bar;
    if let Some(error) = endpoint_error {
        let segments = [
            (mode_bar.error.to_owned(), mode_style),
            (format!(" {error}"), base),
        ];
        let mut x = bar.x.saturating_add(broadcast_width);
        let end = bar.right();
        for (text, style) in segments {
            if x >= end {
                break;
            }
            let remaining = end - x;
            buffer.set_stringn(x, bar.y, &text, usize::from(remaining), style);
            x = x.saturating_add(
                u16::try_from(UnicodeWidthStr::width(text.as_str()))
                    .unwrap_or(u16::MAX)
                    .min(remaining),
            );
        }
        return Some(bar);
    }

    // Terminal mode with an active broadcast: the badge alone is the bar.
    if mode == ClientShellMode::Terminal {
        return Some(bar);
    }

    if let (ClientShellMode::Copy, Some(copy_mode)) = (mode, copy_mode) {
        if let Some(prompt) = copy_mode.search_prompt.as_ref() {
            let marker = match prompt.direction {
                crate::api::schema::PaneCopySearchDirection::Forward => "/",
                crate::api::schema::PaneCopySearchDirection::Backward => "?",
            };
            let content_x = bar.x.saturating_add(broadcast_width);
            buffer.set_stringn(
                content_x,
                bar.y,
                mode_bar.copy,
                usize::from(bar.width.saturating_sub(broadcast_width)),
                mode_style,
            );
            let prefix = 8.min(bar.width.saturating_sub(broadcast_width));
            if prefix >= 8 {
                buffer.set_string(content_x + 7, bar.y, marker, key);
            }
            let footer = mode_bar.copy_footer;
            // 页脚宽度按显示宽度算：中文页脚（"  enter 搜索  esc 取消"）比字节
            // 长度短 4 格，用 `len()` 会把搜索输入区挤窄并让页脚左移压住查询尾部
            // （ds-05）。
            let footer_width = if bar.width.saturating_sub(broadcast_width) >= 50 {
                display_width(footer)
            } else {
                0
            };
            let field = Rect::new(
                content_x + prefix,
                bar.y,
                bar.width
                    .saturating_sub(broadcast_width + prefix + footer_width),
                1,
            );
            if let Some(cursor) = text_editor::render(
                buffer,
                field,
                &prompt.query,
                Style::default().fg(palette.text).bg(palette.panel_bg),
            ) {
                buffer[(cursor.x, cursor.y)]
                    .set_style(Style::default().fg(palette.panel_bg).bg(palette.text));
            }
            if footer_width > 0 {
                buffer.set_string(bar.right() - footer_width, bar.y, footer, base);
            }
            return Some(bar);
        }
    }

    let (badge, hints): (String, Vec<(String, String)>) = match mode {
        ClientShellMode::Prefix => (
            mode_bar.prefix.to_owned(),
            vec![
                ("esc".to_owned(), mode_bar.prefix_cancel.to_owned()),
                (prefix, mode_bar.prefix_send.to_owned()),
                (
                    prefix_rhs(&keybinds.keybinds.workspace_picker),
                    mode_bar.prefix_nav.to_owned(),
                ),
                (
                    prefix_rhs(&keybinds.keybinds.help),
                    mode_bar.prefix_keybinds.to_owned(),
                ),
            ],
        ),
        ClientShellMode::Navigate => (
            mode_bar.navigate.to_owned(),
            vec![
                ("esc".to_owned(), mode_bar.nav_back.to_owned()),
                (
                    format!(
                        "{} / {}",
                        navigate_label(&keybinds.keybinds.navigate.workspace_up),
                        navigate_label(&keybinds.keybinds.navigate.workspace_down)
                    ),
                    mode_bar.nav_workspace.to_owned(),
                ),
                ("tab".to_owned(), mode_bar.nav_pane.to_owned()),
                (
                    prefix_rhs(&keybinds.keybinds.help),
                    mode_bar.prefix_keybinds.to_owned(),
                ),
            ],
        ),
        ClientShellMode::Resize => (
            mode_bar.resize.to_owned(),
            vec![
                ("h/l".to_owned(), mode_bar.resize_width.to_owned()),
                ("j/k".to_owned(), mode_bar.resize_height.to_owned()),
                ("esc".to_owned(), mode_bar.resize_done.to_owned()),
            ],
        ),
        ClientShellMode::Copy => {
            let copy_mode = copy_mode?;
            let select = if copy_mode.selection.is_some() {
                "selecting"
            } else {
                "select"
            };
            let match_status = copy_mode
                .search_current_global
                .map(|current| format!(" {}/{}", current + 1, copy_mode.search_total))
                .or_else(|| (!copy_mode.search_query.is_empty()).then(|| " 0/0".to_owned()))
                .unwrap_or_default();
            let (exit_keys, exit_label) =
                if copy_mode.search_query.is_empty() && copy_mode.selection.is_none() {
                    ("q/esc", "exit")
                } else {
                    ("esc", "clear · q exit")
                };
            (
                mode_bar.copy.to_owned(),
                vec![
                    ("h/j/k/l w/b/e { }".to_owned(), "move".to_owned()),
                    ("/ ?".to_owned(), "search".to_owned()),
                    ("n/N".to_owned(), format!("repeat{match_status}")),
                    ("v/space".to_owned(), select.to_owned()),
                    ("y/enter".to_owned(), "copy".to_owned()),
                    (exit_keys.to_owned(), exit_label.to_owned()),
                ],
            )
        }
        // 终端的错误 / 广播两条路径已在上面各自 return；这里再保一层：渲染
        // 路径不 panic，最多只画一个没有内容的模式条（CFP-17）。
        ClientShellMode::Terminal => return Some(bar),
    };

    let badge_width = display_width(&badge);
    let content_x = bar.x.saturating_add(broadcast_width);
    buffer.set_stringn(
        content_x,
        bar.y,
        &badge,
        usize::from(bar.width.saturating_sub(broadcast_width)),
        mode_style,
    );
    let hints_area = Rect::new(
        content_x.saturating_add(badge_width).saturating_add(1),
        bar.y,
        bar.width
            .saturating_sub(broadcast_width + badge_width.saturating_add(1)),
        1,
    );
    render_key_hints(buffer, hints_area, &hints, palette, components);
    if update_available && mode == ClientShellMode::Navigate {
        let width = 13.min(bar.width);
        let area = Rect::new(bar.right().saturating_sub(width), bar.y, width, 1);
        buffer.set_style(area, Style::default().bg(palette.panel_bg));
        put_right_text(
            buffer,
            area,
            area.y,
            crate::i18n::texts().overlays.release_preview_title,
            Style::default()
                .fg(components.mode_bar_accent)
                .bg(palette.panel_bg)
                .add_modifier(Modifier::BOLD),
        );
    }
    Some(bar)
}

pub(super) struct ShellRenderState<'a> {
    pub(super) endpoints: &'a [ClientShellEndpoint],
    pub(super) machine_chrome: &'a HashMap<crate::client::endpoint::ProfileId, MachineChrome>,
    pub(super) active_endpoint_id: &'a ClientEndpointId,
    pub(super) collapsed_endpoints: &'a HashSet<ClientEndpointId>,
    pub(super) collapsed_groups: &'a HashSet<String>,
    pub(super) remote_collapsed_groups: &'a HashMap<ClientEndpointId, HashSet<String>>,
    pub(super) workspace_scroll: &'a mut usize,
    pub(super) agent_scroll: &'a mut usize,
    pub(super) tab_scroll: &'a mut usize,
    pub(super) reveal_focused_workspace: &'a mut bool,
    pub(super) reveal_focused_tab: &'a mut bool,
    pub(super) sidebar_collapsed: bool,
    pub(super) sidebar_section_split: f32,
    pub(super) tab_drag_insert_index: Option<usize>,
    pub(super) selected_workspace_id: Option<&'a WorkspaceNavigationTarget>,
    pub(super) reveal_navigation_workspace: &'a mut bool,
    pub(super) dragged_workspace_id: Option<&'a str>,
    pub(super) workspace_drop_indicator_row: Option<u16>,
    /// Current chrome hover identity for row/thumb highlight lookups.
    pub(super) chrome_hover: Option<&'a super::feedback::ChromeHover>,
    /// Current spinner frame for connecting/reconnecting endpoint rows.
    pub(super) spinner: &'a str,
}

pub(super) fn render_shell(
    buffer: &mut Buffer,
    layout: ClientShellLayout,
    snapshot: &ClientShellSnapshot,
    config: &ClientShellConfig,
    mut state: ShellRenderState<'_>,
    visual_bell: bool,
) -> ShellHitMap {
    let mut hits = ShellHitMap::default();
    if layout.mobile_header.height > 0 {
        super::mobile::render_mobile_header(
            buffer,
            layout.mobile_header,
            snapshot,
            config,
            &mut hits,
        );
    }
    if layout.sidebar.width > 0 {
        if state.endpoints.len() > 1 {
            if state.sidebar_collapsed {
                super::endpoint_sidebar::render_collapsed(
                    buffer,
                    layout.sidebar,
                    config,
                    &mut state,
                    &mut hits,
                );
            } else {
                super::endpoint_sidebar::render_expanded(
                    buffer,
                    layout.sidebar,
                    Some(snapshot),
                    config,
                    &mut state,
                    &mut hits,
                );
            }
        } else if state.sidebar_collapsed {
            render_collapsed_sidebar(
                buffer,
                layout.sidebar,
                snapshot,
                config,
                &mut state,
                &mut hits,
            );
        } else {
            render_sidebar(
                buffer,
                layout.sidebar,
                snapshot,
                config,
                &mut state,
                &mut hits,
            );
        }
    }
    if layout.tab_bar.height > 0 {
        render_tab_bar(
            buffer,
            layout.tab_bar,
            snapshot,
            config,
            state.tab_scroll,
            state.reveal_focused_tab,
            state.tab_drag_insert_index,
            state.chrome_hover,
            visual_bell,
            &mut hits,
        );
    }
    if !config.mouse_capture {
        hits.sidebar_divider = Rect::default();
        hits.sidebar_section_divider = Rect::default();
        hits.workspace_scrollbar = Rect::default();
        hits.agent_scrollbar = Rect::default();
        hits.agent_sort_toggle = Rect::default();
        hits.new_workspace = Rect::default();
        hits.machines.clear();
        hits.workspaces.clear();
        hits.agents.clear();
        hits.endpoint_agents.clear();
        hits.tab_scroll_left = Rect::default();
        hits.tab_scroll_right = Rect::default();
        hits.new_tab = Rect::default();
        hits.pane_splits.clear();
    }
    hits
}

pub(super) fn put_right_text(buffer: &mut Buffer, area: Rect, y: u16, text: &str, style: Style) {
    let width = display_width(text).min(area.width);
    put_text(
        buffer,
        area.right().saturating_sub(width),
        y,
        width,
        text,
        style,
    );
}

pub(super) fn put_segment(
    buffer: &mut Buffer,
    x: u16,
    y: u16,
    right: u16,
    text: &str,
    style: Style,
) -> u16 {
    let width = display_width(text).min(right.saturating_sub(x));
    put_text(buffer, x, y, width, text, style);
    x.saturating_add(width)
}

pub(super) fn put_text(buffer: &mut Buffer, x: u16, y: u16, width: u16, text: &str, style: Style) {
    if width == 0 || y >= buffer.area.bottom() || x >= buffer.area.right() {
        return;
    }
    buffer.set_stringn(x, y, text, width as usize, style);
}

pub(super) fn display_width(text: &str) -> u16 {
    UnicodeWidthStr::width(text).min(u16::MAX as usize) as u16
}
