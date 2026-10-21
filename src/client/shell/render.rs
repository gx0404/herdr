use super::*;

#[path = "../shell/overlays.rs"]
mod overlays;
#[path = "../shell/sidebar.rs"]
pub(in crate::client::shell) mod sidebar;
#[path = "../shell/tabs.rs"]
mod tabs;

pub(super) use super::aggregate_navigation::navigator_rows as client_navigator_rows;
pub(super) use super::context_menu::render_context_menu;
pub(in crate::client::shell) use overlays::settings_overlay::settings_list_window;
pub(in crate::client::shell) use overlays::{
    help_geometry, help_max_scroll, modal_button, modal_button_row, modal_panel, panel,
    panel_inner, render_search_bar, scrollback_overlay_body, scrollback_overlay_layout, text_inset,
    titled_panel, OverlayRender, SearchBar,
};
pub(super) use overlays::{render_client_overlay, render_minimum_overlay};
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

/// 叠在 `bg` 上的次要文字色（冒烟 L3）：按「弱 → 强」依次试 overlay0、overlay1、
/// subtext0、text，取第一个与 `bg` 对比度 ≥ 4.5:1 的，保住层级又读得清；都不够
/// 就取对比度最大的。`bg` 是 `Reset`（跟随终端底色）时拿 `panel_bg` 近似，仍换算
/// 不出 sRGB 就维持主题原本的 overlay0。纯函数、不分配。
pub(in crate::client::shell) fn readable_muted_fg(
    palette: &Palette,
    bg: ratatui::style::Color,
) -> ratatui::style::Color {
    const READABLE: f32 = 4.5;
    let bg = if crate::ui::color::color_to_rgb(bg).is_some() {
        bg
    } else {
        palette.panel_bg
    };
    let mut best: Option<(ratatui::style::Color, f32)> = None;
    for candidate in [
        palette.overlay0,
        palette.overlay1,
        palette.subtext0,
        palette.text,
    ] {
        let Some(ratio) = crate::ui::color::contrast_ratio(candidate, bg) else {
            continue;
        };
        if ratio >= READABLE {
            return candidate;
        }
        if best.is_none_or(|(_, best_ratio)| ratio > best_ratio) {
            best = Some((candidate, ratio));
        }
    }
    best.map_or(palette.overlay0, |(color, _)| color)
}

/// 键帽式页脚（旧调用面的适配层）：与 `kit::footer_hints` 同一套键帽、说明色标签与
/// 项间隔，按键文字统一规范成小写口径。放不下的提示整项换行；`area` 的行用完后剩余
/// 项整项丢弃，并在末尾画 `…` 标记截断。调用方传入按实时键位配置解析出的按键，
/// 页脚因此始终与绑定一致。新代码直接用 `kit::footer_hints`（可点、带命中区）。
pub(super) fn render_key_hints(
    buffer: &mut Buffer,
    area: Rect,
    hints: &[(String, String)],
    palette: &Palette,
) {
    use crate::ui::kit::footer_hints::{render_footer_hints_wrapped, FooterHint, FooterHintStyles};
    if area.is_empty() || hints.is_empty() {
        return;
    }
    let footer = hints
        .iter()
        .map(|(key, label)| FooterHint::new(key, label))
        .collect::<Vec<_>>();
    render_footer_hints_wrapped(
        buffer,
        area,
        &footer,
        None,
        palette,
        FooterHintStyles::page(palette),
        true,
    );
}

/// 经典布局的模式条：与工作台状态栏同一个组件（`chrome::status_bar`），只在非终端
/// 模式、端点错误或广播进行中时画，位置仍是终端区（或底部标签栏）的最后一行。
/// 返回画出的矩形，调用方据此遮挡与回填。
// 入参是模式条要的全部状态，散在 `ClientShellState` 各处，调用方各自取用。
#[allow(clippy::too_many_arguments)]
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
    use super::chrome::status_bar::{self, Middle, StatusBarView};
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
    let content = status_bar::mode_content(mode, copy_mode, endpoint_error, keybinds);
    let right = if update_available && mode == ClientShellMode::Navigate {
        vec![super::chrome::Span::new(
            crate::i18n::texts().overlays.release_preview_title,
            Style::default()
                .fg(components.mode_bar_accent)
                .add_modifier(Modifier::BOLD),
        )]
    } else {
        Vec::new()
    };
    let (chip, middle) = match content {
        Some((chip, middle)) => (Some(chip), middle),
        None => (None, Middle::Empty),
    };
    let view = StatusBarView {
        mode: chip,
        broadcast: broadcast_count.map(status_bar::broadcast_badge),
        middle,
        hovered: None,
        right,
    };
    status_bar::paint(
        buffer,
        bar,
        &view,
        super::chrome::bar_surface(palette),
        palette,
        components,
    );
    Some(bar)
}

pub(super) struct ShellRenderState<'a> {
    pub(super) endpoints: &'a [ClientShellEndpoint],
    pub(super) active_endpoint_id: &'a ClientEndpointId,
    pub(super) session_rows: Option<&'a super::chrome::sessions::SessionRowsCache>,
    pub(super) session_view: &'a super::chrome::sessions::SessionView,
    pub(super) session_grouping: super::chrome::sessions::SessionGrouping,
    pub(super) tab_scroll: &'a mut usize,
    pub(super) reveal_focused_tab: &'a mut bool,
    pub(super) sidebar_collapsed: bool,
    pub(super) tab_drag_insert_index: Option<usize>,
    pub(super) chrome_hover: Option<&'a super::feedback::ChromeHover>,
    pub(super) spinner: &'a str,
    pub(super) status_glyphs: super::StatusGlyphs,
}

pub(super) fn render_shell(
    buffer: &mut Buffer,
    layout: ClientShellLayout,
    snapshot: &ClientShellSnapshot,
    config: &ClientShellConfig,
    state: ShellRenderState<'_>,
    visual_bell: bool,
) -> ShellHitMap {
    let mut hits = ShellHitMap::default();
    if layout.mobile_header.height > 0 {
        super::mobile::render_mobile_header(
            buffer,
            layout.mobile_header,
            snapshot,
            config,
            state.session_rows,
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
                    &state,
                    &mut hits,
                );
            } else {
                super::endpoint_sidebar::render_expanded(
                    buffer,
                    layout.sidebar,
                    Some(snapshot),
                    config,
                    &state,
                    &mut hits,
                );
            }
        } else if state.sidebar_collapsed {
            render_collapsed_sidebar(buffer, layout.sidebar, snapshot, config, &state, &mut hits);
        } else {
            render_sidebar(buffer, layout.sidebar, snapshot, config, &state, &mut hits);
        }
    }
    if layout.tab_bar.height > 0 {
        let stale = state.endpoints.iter().any(|endpoint| {
            &endpoint.endpoint_id == state.active_endpoint_id
                && endpoint.status != ClientEndpointStatus::Online
        });
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
            state.status_glyphs,
            stale,
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

pub(super) use crate::ui::display_width_u16 as display_width;
