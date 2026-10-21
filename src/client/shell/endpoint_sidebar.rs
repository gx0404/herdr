use super::chrome::sessions::{paint_sessions, SessionHit, SessionPaint};
use super::render::{put_text, ShellRenderState};
use super::*;

pub(super) fn render_collapsed(
    buffer: &mut Buffer,
    area: Rect,
    config: &ClientShellConfig,
    state: &ShellRenderState<'_>,
    hits: &mut ShellHitMap,
) {
    render_expanded(buffer, area, None, config, state, hits);
}

pub(super) fn render_expanded(
    buffer: &mut Buffer,
    area: Rect,
    snapshot: Option<&ClientShellSnapshot>,
    config: &ClientShellConfig,
    state: &ShellRenderState<'_>,
    hits: &mut ShellHitMap,
) {
    super::render::render_sidebar_background(
        buffer,
        area,
        &config.palette,
        matches!(
            state.chrome_hover,
            Some(super::feedback::ChromeHover::SidebarDivider)
        ),
    );
    let view = state.session_view;
    if let Some(cache) = state.session_rows {
        paint_sessions(
            buffer,
            &SessionPaint {
                cache,
                view,
                grouping: state.session_grouping,
                config,
                glyphs: state.status_glyphs,
                hover: state.chrome_hover,
                spinner: state.spinner,
                endpoints: state.endpoints,
                active_endpoint_id: state.active_endpoint_id,
            },
            hits,
        );
    }
    hits.sidebar_divider = view.sidebar_divider;
    hits.sidebar_section_divider = view.section_divider;
    hits.sidebar_toggle = view.sidebar_toggle;
    if !view.header.is_empty() {
        let header = view.header;
        let attention = snapshot.is_some_and(super::global_menu::global_menu_attention);
        let label = if attention {
            crate::i18n::texts().sidebar.attention_menu
        } else {
            crate::i18n::texts().sidebar.menu
        };
        let menu_width = super::render::display_width(label).min(header.width.saturating_sub(5));
        hits.global_launcher = Rect::new(header.x + 1, header.y, menu_width, 1);
        put_text(
            buffer,
            header.x + 1,
            header.y,
            menu_width,
            label,
            crate::ui::design::meta(&config.palette),
        );
        if attention {
            put_text(
                buffer,
                header.x + 1,
                header.y,
                1,
                "●",
                Style::default().fg(config.palette.accent),
            );
        }
        put_text(
            buffer,
            header.x.saturating_add(menu_width + 2),
            header.y,
            header.width.saturating_sub(menu_width + 7),
            super::observability::tr("Sessions", "会话"),
            crate::ui::design::title(&config.palette),
        );
        for (offset, label, hit) in [
            (4, "✚", SessionHit::NewWorkspace),
            (2, "⌕", SessionHit::Search),
        ] {
            if header.width >= offset {
                let rect = Rect::new(header.right() - offset, header.y, 2, 1);
                put_text(
                    buffer,
                    rect.x,
                    rect.y,
                    rect.width,
                    label,
                    crate::ui::design::meta(&config.palette),
                );
                if hit == SessionHit::NewWorkspace {
                    hits.new_workspace = rect;
                }
                hits.sessions.push((rect, hit));
            }
        }
    }
    if !view.sidebar_toggle.is_empty() {
        put_text(
            buffer,
            view.sidebar_toggle.x,
            view.sidebar_toggle.y,
            view.sidebar_toggle.width,
            if state.sidebar_collapsed { "»" } else { "«" },
            if state.sidebar_collapsed
                && snapshot.is_some_and(super::global_menu::global_menu_attention)
            {
                Style::default()
                    .fg(config.palette.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                crate::ui::design::meta(&config.palette)
            },
        );
    }
}

pub(super) fn sidebar_toggle_rect(area: Rect, workbench: bool) -> Rect {
    if workbench || area.is_empty() {
        return Rect::default();
    }
    Rect::new(
        area.right().saturating_sub(2),
        area.bottom().saturating_sub(1),
        u16::from(area.width > 1),
        1,
    )
}

#[cfg(test)]
pub(super) fn footer_menu_slot(workspace_area: Rect, footer_y: u16, toggle: Rect) -> Rect {
    let right = if !toggle.is_empty() && toggle.y == footer_y {
        toggle.x.saturating_sub(1).max(workspace_area.x)
    } else {
        workspace_area.right()
    };
    Rect::new(
        workspace_area.x,
        footer_y,
        right.min(workspace_area.right()) - workspace_area.x,
        1,
    )
}
