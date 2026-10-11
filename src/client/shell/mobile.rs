use super::chrome::sessions::{paint_sessions, SessionHit, SessionPaint, SessionRowsCache};
use super::render::{display_width, put_text};
use super::*;
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::Style,
    widgets::{Clear, Widget},
};

const MOBILE_BUTTON_WIDTH: u16 = 10;

pub(super) fn render_mobile_header(
    buffer: &mut Buffer,
    area: Rect,
    snapshot: &ClientShellSnapshot,
    config: &ClientShellConfig,
    cache: Option<&SessionRowsCache>,
    hits: &mut ShellHitMap,
) {
    if area.is_empty() {
        return;
    }
    let palette = &config.palette;
    buffer.set_style(area, Style::default().bg(palette.panel_bg));
    let button_width = MOBILE_BUTTON_WIDTH.min(area.width);
    let button = Rect::new(
        area.right().saturating_sub(button_width),
        area.y,
        button_width,
        area.height,
    );
    hits.mobile_switch = button;
    let width = button.x.saturating_sub(area.x + 1);
    if let Some(workspace) = snapshot.workspaces.iter().find(|workspace| {
        Some(workspace.workspace_id.as_str()) == snapshot.focused_workspace_id.as_deref()
    }) {
        let bucket = cache
            .and_then(|cache| {
                cache
                    .rail
                    .iter()
                    .find(|item| item.workspace_id == workspace.workspace_id && item.focused)
            })
            .map_or(crate::harness::AgentBucket::None, |item| item.bucket);
        let glyph = crate::ui::design::status_glyph(bucket, config.status_indicators, 0, false);
        put_text(
            buffer,
            area.x,
            area.y,
            3.min(width),
            glyph,
            crate::ui::design::status_style(bucket, palette, &config.components),
        );
        let tabs = snapshot
            .tabs
            .iter()
            .filter(|tab| tab.workspace_id == workspace.workspace_id)
            .collect::<Vec<_>>();
        let position = tabs
            .iter()
            .position(|tab| tab.tab_id == workspace.active_tab_id)
            .unwrap_or(0);
        let label = tabs.get(position).map_or("1", |tab| tab.label.as_str());
        let tab_status = if tabs.len() <= 1 {
            crate::i18n::fill(
                crate::i18n::texts().mobile.tab_label_fmt,
                &[("label", label)],
            )
        } else {
            crate::i18n::fill(
                crate::i18n::texts().mobile.tab_label_pos_fmt,
                &[
                    ("label", label),
                    ("active", &(position + 1).to_string()),
                    ("total", &tabs.len().to_string()),
                ],
            )
        };
        let tab_width = display_width(&tab_status).min(width.saturating_sub(3));
        put_text(
            buffer,
            area.x + 3,
            area.y,
            width.saturating_sub(tab_width + 4),
            &crate::ui::truncate_end(
                &workspace.label,
                usize::from(width.saturating_sub(tab_width + 4)),
            ),
            crate::ui::design::title(palette),
        );
        put_text(
            buffer,
            area.x + width.saturating_sub(tab_width),
            area.y,
            tab_width,
            &tab_status,
            crate::ui::design::meta(palette),
        );
    } else {
        put_text(
            buffer,
            area.x,
            area.y,
            width,
            crate::i18n::texts().mobile.no_workspace,
            crate::ui::design::body(palette),
        );
    }
    if area.height > 1 {
        let counts = cache.map_or_else(Default::default, |cache| cache.counts);
        let mut x = area.x;
        let end = area.x + width;
        if crate::harness::AgentBucket::LISTING
            .iter()
            .all(|bucket| counts.get(*bucket) == 0)
        {
            put_text(
                buffer,
                x,
                area.y + 1,
                width,
                crate::i18n::texts().mobile.no_agents,
                crate::ui::design::meta(palette),
            );
        } else if counts.needs_input + counts.failed + counts.review + counts.working == 0 {
            put_text(
                buffer,
                x,
                area.y + 1,
                width,
                crate::i18n::texts().mobile.all_idle,
                crate::ui::design::meta(palette),
            );
        } else {
            for bucket in crate::harness::AgentBucket::LISTING {
                let count = counts.get(bucket);
                if count == 0 {
                    continue;
                }
                let glyph =
                    crate::ui::design::status_glyph(bucket, config.status_indicators, 0, false);
                let full = format!(
                    "{glyph} {count} {} ",
                    super::chrome::sessions::bucket_label(bucket)
                );
                let text = if display_width(&full) <= end.saturating_sub(x) {
                    full
                } else {
                    format!("{glyph} {count} ")
                };
                let needed = display_width(&text);
                if x + needed > end {
                    break;
                }
                put_text(
                    buffer,
                    x,
                    area.y + 1,
                    needed,
                    &text,
                    crate::ui::design::status_style(bucket, palette, &config.components),
                );
                x += needed;
            }
        }
    }
    buffer.set_style(button, Style::default().bg(palette.surface0));
    put_text(
        buffer,
        button.x + 1,
        button.bottom().saturating_sub(1),
        button.width.saturating_sub(1),
        crate::i18n::texts().mobile.switch,
        crate::ui::design::title(palette),
    );
}

pub(super) fn render_mobile_switcher(
    buffer: &mut Buffer,
    area: Rect,
    frame: &SessionPaint<'_>,
    hits: &mut ShellHitMap,
) {
    if area.is_empty() {
        return;
    }
    Clear.render(area, buffer);
    buffer.set_style(area, Style::default().bg(frame.config.palette.panel_bg));
    hits.mobile_switch = Rect::default();
    hits.mobile_targets.clear();
    if area.height <= 2 {
        hits.mobile_close = Rect::default();
        hits.mobile_max_scroll = 0;
        for x in area.x..area.right() {
            buffer[(x, area.y)]
                .set_symbol("─")
                .set_style(crate::ui::design::rule(&frame.config.palette));
        }
        return;
    }
    let close = Rect::new(
        area.right()
            .saturating_sub(MOBILE_BUTTON_WIDTH.min(area.width)),
        area.y,
        MOBILE_BUTTON_WIDTH.min(area.width),
        area.height.min(2),
    );
    hits.mobile_close = close;
    put_text(
        buffer,
        area.x,
        area.y,
        close.x.saturating_sub(area.x),
        crate::i18n::texts().mobile.switch_label,
        crate::ui::design::title(&frame.config.palette),
    );
    buffer.set_style(close, Style::default().bg(frame.config.palette.surface0));
    put_text(
        buffer,
        close.x + 1,
        close.y,
        close.width.saturating_sub(1),
        crate::i18n::texts().mobile.close,
        crate::ui::design::meta(&frame.config.palette),
    );
    if close.height > 1 {
        put_text(
            buffer,
            close.x + close.width / 2,
            close.y + 1,
            1,
            "×",
            crate::ui::design::title(&frame.config.palette),
        );
    }
    if area.height > 2 {
        for x in area.x..area.right() {
            buffer[(x, area.y + 2)]
                .set_symbol("─")
                .set_style(crate::ui::design::rule(&frame.config.palette));
        }
    }
    paint_sessions(buffer, frame, hits);
    hits.mobile_max_scroll = frame
        .view
        .metrics
        .map_or(0, |metrics| metrics.max_offset_from_bottom);
}

impl ClientShellState {
    pub(super) fn handle_mobile_mouse(
        &mut self,
        mouse: crossterm::event::MouseEvent,
        outcome: &mut ClientShellInput,
    ) -> bool {
        let mobile = self
            .last_composed_size
            .is_some_and(|(cols, rows)| !self.layout(cols, rows).mobile_header.is_empty());
        if !mobile || self.overlay.is_some() {
            return false;
        }
        use crossterm::event::{MouseButton, MouseEventKind};
        let point = (mouse.column, mouse.row);
        if self.mode != ClientShellMode::Navigate {
            if matches!(
                self.mode,
                ClientShellMode::Terminal | ClientShellMode::Resize
            ) && mouse.kind == MouseEventKind::Down(MouseButton::Left)
                && super::contains(self.hits.mobile_switch, point)
            {
                self.mobile_switcher_scroll = 0;
                self.reveal_mobile_workspace = false;
                self.pending_workspace_highlight = None;
                self.mode = ClientShellMode::Navigate;
                self.navigate_workspace_id = self.focused_navigation_target();
                outcome.repaint = true;
                return true;
            }
            return false;
        }
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.mobile_switcher_scroll = self.mobile_switcher_scroll.saturating_sub(2);
                outcome.repaint = true;
                return true;
            }
            MouseEventKind::ScrollDown => {
                self.mobile_switcher_scroll = self
                    .mobile_switcher_scroll
                    .saturating_add(2)
                    .min(self.hits.mobile_max_scroll);
                outcome.repaint = true;
                return true;
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if let (
                    Some(ClientChromeDrag::WorkspaceScrollbar { grab_row_offset }),
                    Some(metrics),
                ) = (&self.chrome_drag, self.hits.workspace_scroll_metrics)
                {
                    let offset = crate::ui::scrollbar_offset_from_drag_row(
                        metrics,
                        self.hits.workspace_scrollbar,
                        mouse.row,
                        *grab_row_offset,
                    );
                    self.mobile_switcher_scroll =
                        metrics.max_offset_from_bottom.saturating_sub(offset);
                    outcome.repaint = true;
                }
                return true;
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if matches!(
                    self.chrome_drag,
                    Some(ClientChromeDrag::WorkspaceScrollbar { .. })
                ) {
                    self.chrome_drag = None;
                }
                return true;
            }
            MouseEventKind::Down(MouseButton::Left) => {}
            _ => return true,
        }
        if super::contains(self.hits.workspace_scrollbar, point) {
            if let Some(metrics) = self.hits.workspace_scroll_metrics {
                if let Some(grab_row_offset) = crate::ui::scrollbar_thumb_grab_offset(
                    metrics,
                    self.hits.workspace_scrollbar,
                    mouse.row,
                ) {
                    self.chrome_drag =
                        Some(ClientChromeDrag::WorkspaceScrollbar { grab_row_offset });
                } else {
                    let offset = crate::ui::scrollbar_offset_from_row(
                        metrics,
                        self.hits.workspace_scrollbar,
                        mouse.row,
                    );
                    self.mobile_switcher_scroll =
                        metrics.max_offset_from_bottom.saturating_sub(offset);
                    outcome.repaint = true;
                }
            }
            return true;
        }
        if super::contains(self.hits.mobile_close, point) {
            self.mode = ClientShellMode::Terminal;
            self.navigate_workspace_id = None;
            outcome.repaint = true;
            return true;
        }
        if self.hits.sessions.iter().any(|(rect, hit)| {
            super::contains(*rect, point)
                && !matches!(hit, SessionHit::Terminal(..) | SessionHit::Rail(..))
        }) && self.handle_session_click(point, outcome)
        {
            return true;
        }
        if self
            .hits
            .machines
            .iter()
            .any(|hit| super::contains(hit.collapse_toggle, point))
            && self.handle_endpoint_machine_click(point, outcome)
        {
            return true;
        }
        if self.handle_agent_tree_click(point, outcome) {
            return true;
        }
        let target = self
            .hits
            .mobile_targets
            .iter()
            .find(|(rect, _)| super::contains(*rect, point))
            .map(|(_, target)| target.clone());
        match target {
            Some(ClientMobileTarget::Machine(endpoint_id)) => {
                if endpoint_id == self.active_endpoint_id {
                    self.mode = ClientShellMode::Terminal;
                    self.navigate_workspace_id = None;
                    if endpoint_id.is_local() {
                        self.activate_endpoint(endpoint_id, outcome);
                    }
                } else if self.activate_endpoint(endpoint_id, outcome) {
                    self.mode = ClientShellMode::Terminal;
                    self.navigate_workspace_id = None;
                }
            }
            Some(ClientMobileTarget::NewWorkspace) => {
                self.mobile_switcher_suspended = true;
                self.record_binding(
                    crate::input::KeybindMatch::Action(crate::input::KeybindAction::NewWorkspace),
                    outcome,
                );
            }
            Some(ClientMobileTarget::Workspace {
                endpoint_id,
                workspace_id,
            }) => {
                if self.focus_or_activate(
                    endpoint_id,
                    ClientEndpointFocusTarget::Workspace(workspace_id),
                    outcome,
                ) {
                    self.mode = ClientShellMode::Terminal;
                    self.navigate_workspace_id = None;
                }
            }
            Some(ClientMobileTarget::Agent {
                endpoint_id,
                pane_id,
            }) => {
                if self.focus_or_activate(
                    endpoint_id,
                    ClientEndpointFocusTarget::Pane(pane_id),
                    outcome,
                ) {
                    self.mode = ClientShellMode::Terminal;
                    self.navigate_workspace_id = None;
                }
            }
            Some(ClientMobileTarget::Tab {
                endpoint_id,
                tab_id,
            }) => {
                if self.focus_or_activate(
                    endpoint_id,
                    ClientEndpointFocusTarget::Tab(tab_id),
                    outcome,
                ) {
                    self.mode = ClientShellMode::Terminal;
                    self.navigate_workspace_id = None;
                }
            }
            Some(ClientMobileTarget::NewTab) => {
                self.mobile_switcher_suspended = true;
                self.record_binding(
                    crate::input::KeybindMatch::Action(crate::input::KeybindAction::NewTab),
                    outcome,
                );
            }
            Some(ClientMobileTarget::Menu(index)) => {
                let actionable = self.snapshot.as_deref().is_some_and(|snapshot| {
                    super::global_menu::global_menu_items(snapshot)
                        .get(index)
                        .is_some_and(|entry| entry.enabled)
                });
                if actionable {
                    self.mobile_switcher_suspended = true;
                    self.activate_global_menu_item(index, outcome);
                }
            }
            None => {}
        }
        outcome.repaint = true;
        true
    }
}
