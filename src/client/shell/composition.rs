use super::*;

fn restore_mode_bar(
    frame: &mut FrameData,
    bar: Option<Rect>,
    cells: Option<&[crate::protocol::CellData]>,
) {
    let (Some(bar), Some(cells)) = (bar, cells) else {
        return;
    };
    let start = usize::from(bar.y) * usize::from(frame.width) + usize::from(bar.x);
    frame.cells[start..start + usize::from(bar.width)].clone_from_slice(cells);
    // 只有模式条真正覆盖了光标所在的列区间才抹掉光标；同一行其它列仍归终端。
    if frame
        .cursor
        .as_ref()
        .is_some_and(|cursor| contains(bar, (cursor.x, cursor.y)))
    {
        frame.cursor = None;
    }
}

impl ClientShellState {
    fn prepare_chrome_feedback(&mut self, now: std::time::Instant) {
        // Entrance-fade clocks: overlay kind transitions and toast arrivals
        // start a one-frame dim; the settle repaint comes from the timer.
        let overlay_kind = self.overlay.as_ref().map(ClientShellOverlay::kind);
        if overlay_kind != self.last_overlay_kind {
            if overlay_kind.is_some() && self.config.feedback.animations {
                self.overlay_since = Some(now);
            }
            self.last_overlay_kind = overlay_kind;
        }
        let has_toast =
            self.visible_notification.is_some() || self.visible_endpoint_notice.is_some();
        if has_toast && !self.had_toast && self.config.feedback.animations {
            self.toast_since = Some(now);
        }
        self.had_toast = has_toast;
    }

    fn compose_unavailable(&mut self, cols: u16, rows: u16) -> FrameData {
        let layout = self.layout(cols, rows);
        let mut buffer = Buffer::empty(Rect::new(0, 0, cols, rows));
        buffer.set_style(
            buffer.area,
            Style::default()
                .fg(self.config.palette.text)
                .bg(self.config.palette.panel_bg),
        );
        self.hits = ShellHitMap::default();
        let sidebar = if layout.sidebar.width > 0 {
            layout.sidebar
        } else {
            Rect::new(0, 1, cols, rows.saturating_sub(2))
        };
        let valid_navigation_target = self.mode == ClientShellMode::Navigate
            && self
                .navigate_workspace_id
                .as_ref()
                .is_some_and(|target| self.navigation_target_valid(target));
        // A resize invalidates pane geometry, not the healthy Local workspace chrome.
        let local_snapshot = self.snapshot.as_deref().filter(|_| {
            self.endpoints.len() == 1
                && !self.sidebar_collapsed
                && layout.sidebar.width > 0
                && self.endpoint_status(&self.active_endpoint_id)
                    == Some(ClientEndpointStatus::Online)
        });
        let spinner = self.spinner_glyph();
        let mut render_state = render::ShellRenderState {
            endpoints: &self.endpoints,
            machine_chrome: &self.machine_chrome,
            active_endpoint_id: &self.active_endpoint_id,
            collapsed_endpoints: &self.collapsed_endpoints,
            collapsed_groups: &self.collapsed_groups,
            remote_collapsed_groups: &self.remote_collapsed_groups,
            workspace_scroll: &mut self.workspace_scroll,
            agent_scroll: &mut self.agent_scroll,
            tab_scroll: &mut self.tab_scroll,
            reveal_focused_workspace: &mut self.reveal_focused_workspace,
            reveal_focused_tab: &mut self.reveal_focused_tab,
            sidebar_collapsed: false,
            sidebar_section_split: self.sidebar_section_split,
            tab_drag_insert_index: None,
            selected_workspace_id: self
                .navigate_workspace_id
                .as_ref()
                .filter(|_| valid_navigation_target),
            reveal_navigation_workspace: &mut self.reveal_navigation_workspace,
            dragged_workspace_id: None,
            workspace_drop_indicator_row: None,
            chrome_hover: self.hover.as_ref(),
            spinner,
        };
        if let Some(snapshot) = local_snapshot {
            render::render_sidebar(
                &mut buffer,
                sidebar,
                snapshot,
                &self.config,
                &mut render_state,
                &mut self.hits,
            );
        } else {
            super::endpoint_sidebar::render_expanded(
                &mut buffer,
                sidebar,
                self.snapshot.as_deref(),
                &self.config,
                &mut render_state,
                &mut self.hits,
            );
        }
        if !self.config.mouse_capture {
            self.hits = ShellHitMap::default();
        }
        let message = self.endpoint_error.clone().unwrap_or_else(|| {
            let status = self
                .endpoint_status(&self.active_endpoint_id)
                .unwrap_or(ClientEndpointStatus::Connecting);
            let (_, status_label, _) =
                endpoint_status_presentation(status, &self.config.palette, self.spinner_glyph());
            crate::i18n::fill(
                crate::i18n::texts().endpoint.offline_hint_fmt,
                &[
                    ("label", self.active_endpoint_label()),
                    ("status", status_label),
                ],
            )
        });
        let message_area = if layout.sidebar.width > 0 {
            layout.pane_surface
        } else {
            Rect::new(0, 0, cols, 1)
        };
        if local_snapshot.is_none() || self.endpoint_error.is_some() {
            render::put_text(
                &mut buffer,
                message_area.x,
                message_area.y,
                message_area.width,
                &message,
                Style::default().fg(self.config.palette.overlay0),
            );
        }
        render::render_mode_bar(
            &mut buffer,
            Rect::new(0, 0, cols, rows),
            self.mode,
            None,
            self.endpoint_error.as_deref(),
            false,
            self.broadcast_indicator_count(),
            &self.config.keybinds,
            &self.config.palette,
            &self.config.components,
        );
        FrameData::from_ratatui_buffer_with_hyperlinks(&buffer, None, &[])
    }

    pub(crate) fn compose(&mut self, cols: u16, rows: u16) -> Option<FrameData> {
        let compose_now = std::time::Instant::now();
        self.last_composed_at = Some(compose_now);
        if self
            .last_composed_size
            .is_some_and(|size| size != (cols, rows))
        {
            self.cancel_frozen_selection();
            self.page_drag = None;
            match self.overlay.as_mut() {
                Some(ClientShellOverlay::CommandPalette(page)) => page.reveal = true,
                Some(ClientShellOverlay::Settings(page)) => page.reveal = true,
                Some(ClientShellOverlay::Machines(page)) => page.reveal = true,
                _ => {}
            }
        }
        self.selection_repaint_deadline = None;
        if self.last_composed_size != Some((cols, rows)) && self.mode == ClientShellMode::Navigate {
            self.reveal_navigation_workspace = true;
            self.reveal_mobile_workspace = true;
        }
        self.last_composed_size = Some((cols, rows));
        self.prepare_chrome_feedback(compose_now);
        if self.workbench.enabled {
            return self.compose_workbench(cols, rows);
        }
        let valid_navigation_target = self.mode == ClientShellMode::Navigate
            && self
                .navigate_workspace_id
                .as_ref()
                .is_some_and(|target| self.navigation_target_valid(target));
        if self.snapshot.is_none() || self.pane_surface.is_none() {
            let mut frame = self.compose_unavailable(cols, rows);
            let area = self.layout(cols, rows).pane_surface;
            self.paint_observability(&mut frame, area);
            let mut occlusion = crate::kitty_graphics::surface::Occlusion::default();
            self.paint_shell_feedback(&mut frame, self.layout(cols, rows), &mut occlusion)?;
            self.paint_shell_overlays(&mut frame, &mut occlusion)?;
            return Some(frame);
        }
        let snapshot = self.snapshot.as_deref()?;
        // Do not compose a retained surface while waiting for its matching snapshot or
        // connection generation.
        if self.pending_pane_surface.is_some()
            || self.pane_surface_generation != self.active_snapshot_generation
        {
            return None;
        }
        let surface = self.pane_surface.as_ref()?;
        if snapshot.revision != surface.projection_revision {
            return None;
        }
        let layout = self.layout(cols, rows);
        if self.last_tab_bar_width != Some(layout.tab_bar.width) {
            self.last_tab_bar_width = Some(layout.tab_bar.width);
            self.reveal_focused_tab = true;
        }
        let tab_drag_insert_index = match &self.chrome_drag {
            Some(ClientChromeDrag::Tab { insert_index, .. }) => *insert_index,
            _ => None,
        };
        let (dragged_workspace_id, workspace_drop_indicator_row) = match &self.chrome_drag {
            Some(ClientChromeDrag::Workspace {
                source_workspace_id,
                target,
            }) => (
                Some(source_workspace_id.as_str()),
                target.as_ref().map(|(_, row)| *row),
            ),
            _ => (None, None),
        };
        let visual_bell = self.visual_bell_active();
        let spinner = self.spinner_glyph();
        let mut buffer = Buffer::empty(Rect::new(0, 0, cols, rows));
        self.hits = render::render_shell(
            &mut buffer,
            layout,
            snapshot,
            &self.config,
            render::ShellRenderState {
                endpoints: &self.endpoints,
                machine_chrome: &self.machine_chrome,
                active_endpoint_id: &self.active_endpoint_id,
                collapsed_endpoints: &self.collapsed_endpoints,
                collapsed_groups: &self.collapsed_groups,
                remote_collapsed_groups: &self.remote_collapsed_groups,
                workspace_scroll: &mut self.workspace_scroll,
                agent_scroll: &mut self.agent_scroll,
                tab_scroll: &mut self.tab_scroll,
                reveal_focused_workspace: &mut self.reveal_focused_workspace,
                reveal_focused_tab: &mut self.reveal_focused_tab,
                sidebar_collapsed: self.sidebar_collapsed,
                sidebar_section_split: self.sidebar_section_split,
                tab_drag_insert_index,
                selected_workspace_id: self
                    .navigate_workspace_id
                    .as_ref()
                    .filter(|_| valid_navigation_target),
                reveal_navigation_workspace: &mut self.reveal_navigation_workspace,
                dragged_workspace_id,
                workspace_drop_indicator_row,
                chrome_hover: self.hover.as_ref(),
                spinner,
            },
            visual_bell,
        );
        self.hits.panes = surface
            .panes
            .iter()
            .map(|pane| PaneHit {
                rect: Rect::new(
                    layout.pane_surface.x.saturating_add(pane.rect.x),
                    layout.pane_surface.y.saturating_add(pane.rect.y),
                    pane.rect.width,
                    pane.rect.height,
                ),
                inner_rect: Rect::new(
                    layout.pane_surface.x.saturating_add(pane.inner_rect.x),
                    layout.pane_surface.y.saturating_add(pane.inner_rect.y),
                    pane.inner_rect.width,
                    pane.inner_rect.height,
                ),
                scrollbar_rect: pane.scrollbar_rect.map(|rect| {
                    Rect::new(
                        layout.pane_surface.x.saturating_add(rect.x),
                        layout.pane_surface.y.saturating_add(rect.y),
                        rect.width,
                        rect.height,
                    )
                }),
                scroll: pane.scroll.map(|metrics| crate::pane::ScrollMetrics {
                    offset_from_bottom: usize::try_from(metrics.offset_from_bottom)
                        .unwrap_or(usize::MAX),
                    max_offset_from_bottom: usize::try_from(metrics.max_offset_from_bottom)
                        .unwrap_or(usize::MAX),
                    viewport_rows: usize::try_from(metrics.viewport_rows).unwrap_or(usize::MAX),
                }),
                pane_id: pane.pane_id.clone(),
                popup: false,
                mouse_reporting: pane.mouse_reporting,
                sgr_pixel_mouse: pane.sgr_pixel_mouse,
                pixel_width: pane.pixel_width,
                pixel_height: pane.pixel_height,
            })
            .collect();
        let topology_signature = pane_surface_topology_signature(surface);
        self.hits.pane_splits = surface
            .splits
            .iter()
            .map(|split| PaneSplitHit {
                tab_id: snapshot.focused_tab_id.clone(),
                direction: split.direction,
                pos: match split.direction {
                    crate::protocol::PaneSurfaceSplitDirection::Horizontal => {
                        layout.pane_surface.x.saturating_add(split.pos)
                    }
                    crate::protocol::PaneSurfaceSplitDirection::Vertical => {
                        layout.pane_surface.y.saturating_add(split.pos)
                    }
                },
                area: Rect::new(
                    layout.pane_surface.x.saturating_add(split.area.x),
                    layout.pane_surface.y.saturating_add(split.area.y),
                    split.area.width,
                    split.area.height,
                ),
                hit_rect: Rect::new(
                    layout.pane_surface.x.saturating_add(split.hit_rect.x),
                    layout.pane_surface.y.saturating_add(split.hit_rect.y),
                    split.hit_rect.width,
                    split.hit_rect.height,
                ),
                path: split.path.clone(),
                topology_signature,
            })
            .collect();
        if !self.config.mouse_capture {
            self.hits.pane_splits.clear();
        }
        let mode_bar_area = if layout.mobile_header.is_empty()
            && self.config.tab_bar_position == TabBarPositionConfig::Bottom
            && !layout.tab_bar.is_empty()
        {
            layout.tab_bar
        } else {
            layout.pane_surface
        };
        let mobile_navigate_panel = !layout.mobile_header.is_empty()
            && self.mode == ClientShellMode::Navigate
            && self.endpoint_error.is_none();
        let mode_bar = if mobile_navigate_panel || self.overlay.is_some() {
            None
        } else {
            render::render_mode_bar(
                &mut buffer,
                mode_bar_area,
                self.mode,
                self.copy_mode.as_ref(),
                self.endpoint_error.as_deref(),
                snapshot.update_available.is_some(),
                self.broadcast_indicator_count(),
                &self.config.keybinds,
                &self.config.palette,
                &self.config.components,
            )
        };
        if mode_bar == Some(layout.tab_bar) {
            self.hits.tabs.clear();
            self.hits.new_tab = Rect::default();
            self.hits.tab_scroll_left = Rect::default();
            self.hits.tab_scroll_right = Rect::default();
        }
        let mut frame = FrameData::from_ratatui_buffer_with_hyperlinks(&buffer, None, &[]);
        let mode_bar_cells = mode_bar.map(|bar| {
            let start = usize::from(bar.y) * usize::from(frame.width) + usize::from(bar.x);
            frame.cells[start..start + usize::from(bar.width)].to_vec()
        });
        blit_pane_surface(&mut frame, &surface.frame, layout.pane_surface);
        if visual_bell {
            if let Some(focused_pane_id) = snapshot.focused_pane_id.as_deref() {
                if let Some(hit) = self
                    .hits
                    .panes
                    .iter()
                    .find(|hit| hit.pane_id == focused_pane_id)
                    .cloned()
                {
                    let cursor = frame.cursor.clone();
                    let mut composed = frame.to_ratatui_buffer()?;
                    emphasize_pane_border(&mut composed, &hit, self.config.palette.yellow);
                    frame.replace_from_ratatui_buffer_preserving_effects(&composed, cursor);
                }
            }
        }
        restore_mode_bar(&mut frame, mode_bar, mode_bar_cells.as_deref());
        let mut occlusion = crate::kitty_graphics::surface::Occlusion::default();
        self.paint_frozen_selection(&mut frame, &mut occlusion);
        self.paint_shell_copy(&mut frame, &mut occlusion)?;
        restore_mode_bar(&mut frame, mode_bar, mode_bar_cells.as_deref());
        self.paint_shell_feedback(&mut frame, layout, &mut occlusion)?;
        if let Some(covered) = self.paint_observability(&mut frame, layout.pane_surface) {
            for rect in covered {
                occlusion.cover(rect);
            }
        }
        let snapshot = self.snapshot.as_deref()?;
        let surface = self.pane_surface.as_ref()?;
        self.hits.popup = None;
        if let Some(popup) = surface.popup.as_deref() {
            let width = popup.width.map(client_popup_size);
            let height = popup.height.map(client_popup_size);
            if let Some(geometry) =
                crate::popup_size::resolve_popup_geometry(width, height, layout.pane_surface)
            {
                occlusion.start_popup(geometry.outer);
                let mut composed = frame.to_ratatui_buffer()?;
                let block = ratatui::widgets::Block::default()
                    .borders(ratatui::widgets::Borders::ALL)
                    .border_set(self.config.border_glyphs.border_set())
                    .border_style(ratatui::style::Style::default().fg(self.config.palette.accent))
                    .title(popup.title.clone())
                    .style(ratatui::style::Style::default().bg(self.config.palette.panel_bg));
                ratatui::widgets::Widget::render(
                    ratatui::widgets::Clear,
                    geometry.outer,
                    &mut composed,
                );
                ratatui::widgets::Widget::render(block, geometry.outer, &mut composed);
                frame.replace_from_ratatui_buffer_preserving_effects(&composed, None);
                blit_pane_surface(&mut frame, &popup.frame, geometry.inner);
                self.hits.popup = Some(PaneHit {
                    rect: geometry.outer,
                    inner_rect: geometry.inner,
                    scrollbar_rect: None,
                    scroll: None,
                    pane_id: popup.terminal_id.clone(),
                    popup: true,
                    mouse_reporting: popup.mouse_reporting,
                    sgr_pixel_mouse: popup.sgr_pixel_mouse,
                    pixel_width: popup.pixel_width,
                    pixel_height: popup.pixel_height,
                });
            }
        }
        let cx = super::feedback::ChromeContext {
            page_bounds: None,
            palette: &self.config.palette,
            components: &self.config.components,
            glyphs: self.config.border_glyphs,
            hover: self.hover.as_ref(),
            spinner,
            now: compose_now,
        };
        let active_lifecycle = self
            .endpoints
            .iter()
            .find(|endpoint| endpoint.endpoint_id == self.active_endpoint_id)
            .filter(|endpoint| endpoint.status != ClientEndpointStatus::Online)
            .map(|endpoint| {
                let progress =
                    self.endpoint_reconnect_progress(&endpoint.endpoint_id)
                        .map(|progress| {
                            (
                                progress.attempts,
                                progress
                                    .next_attempt_at
                                    .saturating_duration_since(compose_now)
                                    .as_secs(),
                            )
                        });
                (
                    endpoint.endpoint_id.clone(),
                    endpoint.label.clone(),
                    endpoint.status,
                    progress,
                )
            });
        if !layout.mobile_header.is_empty()
            && self.mode == ClientShellMode::Navigate
            && self.overlay.is_none()
        {
            let mut composed = frame.to_ratatui_buffer()?;
            occlusion.cover(composed.area);
            super::mobile::render_mobile_switcher(
                &mut composed,
                Rect::new(0, 0, cols, rows),
                snapshot,
                &self.endpoints,
                &self.active_endpoint_id,
                &self.config,
                self.navigate_workspace_id
                    .as_ref()
                    .filter(|_| valid_navigation_target),
                &mut self.mobile_switcher_scroll,
                &mut self.reveal_mobile_workspace,
                &mut self.hits,
            );
            if let Some((_, label, status, progress)) = active_lifecycle.as_ref() {
                let _ = endpoint_notices::render_lifecycle_banner(
                    &mut composed,
                    Rect::new(0, 0, cols, rows),
                    label,
                    *status,
                    *progress,
                    false,
                    2,
                    &cx,
                );
            }
            if let Some(notice) = self.visible_endpoint_notice.as_ref() {
                self.hits.notification_toast = endpoint_notices::render_mobile_banner(
                    &mut composed,
                    Rect::new(0, 0, cols, rows),
                    notice,
                    active_lifecycle.is_some(),
                    &self.config.palette,
                    &self.config.components,
                );
            }
            frame.replace_from_ratatui_buffer_preserving_effects(&composed, None);
            self.hits.panes.clear();
            self.hits.pane_splits.clear();
            self.hits.popup = None;
        }
        restore_mode_bar(&mut frame, mode_bar, mode_bar_cells.as_deref());
        if let Some(bar) = mode_bar {
            occlusion.cover(bar);
        }
        self.paint_shell_overlays(&mut frame, &mut occlusion)?;
        if self.endpoint_status(&self.active_endpoint_id) != Some(ClientEndpointStatus::Online) {
            frame.cursor = None;
            self.hits.panes.clear();
            self.hits.pane_splits.clear();
            self.hits.popup = None;
        }
        self.compose_graphics(&mut frame, layout, &occlusion);
        self.hits.composed = true;
        Some(frame)
    }
    pub(super) fn paint_shell_feedback(
        &mut self,
        frame: &mut FrameData,
        layout: ClientShellLayout,
        occlusion: &mut crate::kitty_graphics::surface::Occlusion,
    ) -> Option<()> {
        let (cols, rows) = (frame.width, frame.height);
        let compose_now = self
            .last_composed_at
            .unwrap_or_else(std::time::Instant::now);
        let cx = super::feedback::ChromeContext {
            page_bounds: None,
            palette: &self.config.palette,
            components: &self.config.components,
            glyphs: self.config.border_glyphs,
            hover: self.hover.as_ref(),
            spinner: self.spinner_glyph(),
            now: compose_now,
        };
        self.hits.notification_toast = Rect::default();
        self.hits.machine_auth_actions = Vec::new();
        self.hits.lifecycle_banner_retry = Rect::default();
        self.hits.lifecycle_banner_give_up = Rect::default();
        let has_config_diagnostic = self.config_diagnostic.is_some();
        let active_lifecycle = self
            .endpoints
            .iter()
            .find(|endpoint| endpoint.endpoint_id == self.active_endpoint_id)
            .filter(|endpoint| endpoint.status != ClientEndpointStatus::Online)
            .map(|endpoint| {
                let progress =
                    self.endpoint_reconnect_progress(&endpoint.endpoint_id)
                        .map(|progress| {
                            (
                                progress.attempts,
                                progress
                                    .next_attempt_at
                                    .saturating_duration_since(compose_now)
                                    .as_secs(),
                            )
                        });
                (
                    endpoint.endpoint_id.clone(),
                    endpoint.label.clone(),
                    endpoint.status,
                    progress,
                )
            });
        if has_config_diagnostic
            || active_lifecycle.is_some()
            || self.visible_endpoint_notice.is_some()
            || self.visible_notification.is_some()
        {
            let cursor = frame.cursor.clone();
            let mut composed = frame.to_ratatui_buffer()?;
            if let Some(diagnostic) = self.config_diagnostic.as_deref() {
                let diagnostic_area = if layout.mobile_header.is_empty() {
                    Rect::new(0, 0, cols, rows)
                } else {
                    layout.pane_surface
                };
                crate::ui::render_config_diagnostic_buffer(
                    &mut composed,
                    diagnostic_area,
                    diagnostic,
                    &self.config.palette,
                    |rect| occlusion.cover(rect),
                );
            }
            let (lifecycle_offset, lifecycle_banner) = active_lifecycle.as_ref().map_or(
                (0, None),
                |(endpoint_id, label, status, progress)| {
                    let banner = endpoint_notices::render_lifecycle_banner(
                        &mut composed,
                        Rect::new(0, 0, cols, rows),
                        label,
                        *status,
                        *progress,
                        !endpoint_id.is_local(),
                        u16::from(has_config_diagnostic) + layout.mobile_header.height,
                        &cx,
                    );
                    occlusion.cover(banner.rect);
                    (1, Some(banner))
                },
            );
            if let Some(banner) = lifecycle_banner {
                self.hits.lifecycle_banner_retry = banner.retry;
                self.hits.lifecycle_banner_give_up = banner.give_up;
            }
            if let Some(notice) = self.visible_endpoint_notice.as_ref() {
                self.hits.notification_toast = if layout.mobile_header.is_empty() {
                    endpoint_notices::render_notice(
                        &mut composed,
                        Rect::new(0, 0, cols, rows),
                        notice,
                        u16::from(has_config_diagnostic) + lifecycle_offset,
                        &cx,
                    )
                } else {
                    endpoint_notices::render_mobile_banner(
                        &mut composed,
                        Rect::new(0, 0, cols, rows),
                        notice,
                        has_config_diagnostic || lifecycle_offset > 0,
                        &self.config.palette,
                        &self.config.components,
                    )
                };
            } else if let Some(notification) = self.visible_notification.as_ref() {
                self.hits.notification_toast = if layout.mobile_header.is_empty() {
                    notifications::render_visible_notification(
                        &mut composed,
                        Rect::new(0, 0, cols, rows),
                        notification,
                        self.config.toast_position,
                        u16::from(has_config_diagnostic) + lifecycle_offset,
                        &cx,
                    )
                } else {
                    notifications::render_mobile_notification_banner(
                        &mut composed,
                        Rect::new(0, 0, cols, rows),
                        notification,
                        has_config_diagnostic || lifecycle_offset > 0,
                        &self.config.palette,
                        &self.config.components,
                    )
                };
            }
            occlusion.cover(self.hits.notification_toast);
            if self.toast_since.is_some_and(|since| {
                compose_now.duration_since(since) < super::feedback::ENTRANCE_DURATION
            }) && !self.hits.notification_toast.is_empty()
            {
                composed.set_style(
                    self.hits.notification_toast,
                    Style::default().add_modifier(Modifier::DIM),
                );
            }
            frame.replace_from_ratatui_buffer_preserving_effects(&composed, cursor);
        }
        if let Some(feedback) = self.copy_feedback.as_ref() {
            let cursor = frame.cursor.clone();
            let mut composed = frame.to_ratatui_buffer()?;
            let base_offset = u16::from(has_config_diagnostic);
            let feedback_area = if layout.mobile_header.is_empty() {
                layout.pane_surface
            } else {
                Rect::new(0, 0, cols, rows)
            };
            let offset = crate::ui::copy_feedback_offset_for_toast(
                feedback_area,
                feedback,
                base_offset,
                self.config.clipboard_toast_position,
                self.hits.notification_toast,
            );
            occlusion.cover(crate::ui::render_copy_feedback_buffer_styled(
                &mut composed,
                feedback_area,
                feedback,
                offset,
                self.config.clipboard_toast_position,
                &self.config.palette,
                &self.config.components,
            ));
            frame.replace_from_ratatui_buffer_preserving_effects(&composed, cursor);
        }
        Some(())
    }

    pub(super) fn paint_shell_copy(
        &self,
        frame: &mut FrameData,
        occlusion: &mut crate::kitty_graphics::surface::Occlusion,
    ) -> Option<()> {
        let has_selection = self
            .selection
            .as_ref()
            .is_some_and(|selection| selection.is_visible());
        let has_search = self
            .copy_mode
            .as_ref()
            .is_some_and(|copy_mode| !copy_mode.search_matches.is_empty());
        let has_link_hints = self.link_hints.is_some();
        // C-12 (e)：选区与 copy 搜索高亮只属于各自的拥有者 pane，不再对每个 pane
        // 调一遍渲染函数。
        let selection_owner = self.selection.as_ref().map(|s| s.pane_id.as_str());
        let copy_owner = self.copy_mode.as_ref().map(|c| c.pane_id.as_str());
        // Copy 模式的单格光标（原第三段往返）：先算出目标格，界外则不画。
        let copy_cursor_cell = if self.mode == ClientShellMode::Copy {
            self.copy_mode.as_ref().and_then(|copy_mode| {
                let hit = self.hits.panes.iter().find(|hit| {
                    hit.pane_id == copy_mode.pane_id
                        && client_copy_surface_coherent(Some(copy_mode), hit)
                })?;
                let viewport_top = copy_mode
                    .max_offset_from_bottom
                    .saturating_sub(copy_mode.offset_from_bottom)
                    .min(u32::MAX as usize) as u32;
                let viewport_row = copy_mode.cursor.row.saturating_sub(viewport_top);
                let x = hit.inner_rect.x.saturating_add(copy_mode.cursor.col);
                let y = hit.inner_rect.y.saturating_add(viewport_row as u16);
                (viewport_row < u32::from(hit.inner_rect.height)
                    && copy_mode.cursor.col < hit.inner_rect.width
                    && x < frame.width
                    && y < frame.height)
                    .then_some((x, y))
            })
        } else {
            None
        };
        // Copy 模式整帧无光标（与原逐段行为一致，不论光标格是否在界内）。
        if self.mode == ClientShellMode::Copy {
            frame.cursor = None;
        }
        // C-12 (d)：选区/搜索高亮、link hints、copy-mode 光标三段互不冲突的整帧往返
        // 合并为一次 Buffer 转换；绘制顺序与原顺序一致。
        if has_selection || has_search || has_link_hints || copy_cursor_cell.is_some() {
            let keep_cursor = !has_link_hints && self.mode != ClientShellMode::Copy;
            let cursor = if keep_cursor {
                frame.cursor.clone()
            } else {
                None
            };
            let mut composed = frame.to_ratatui_buffer()?;
            if has_selection || has_search {
                for hit in self.hits.panes.iter().filter(|hit| {
                    selection_owner == Some(hit.pane_id.as_str())
                        || copy_owner == Some(hit.pane_id.as_str())
                }) {
                    let copy_surface_coherent =
                        client_copy_surface_coherent(self.copy_mode.as_ref(), hit);
                    if copy_surface_coherent {
                        render_client_copy_search_highlights(
                            &mut composed,
                            self.copy_mode.as_ref(),
                            hit,
                            &self.config.palette,
                            false,
                            occlusion,
                        );
                    }
                    let selection_is_stale_copy_projection = !copy_surface_coherent
                        && self.copy_mode.as_ref().is_some_and(|copy_mode| {
                            copy_mode.pane_id == hit.pane_id
                                && self
                                    .selection
                                    .as_ref()
                                    .is_some_and(|selection| selection.pane_id == hit.pane_id)
                        });
                    if !selection_is_stale_copy_projection {
                        if let Some(selection) =
                            self.selection.as_ref().filter(|s| s.pane_id == hit.pane_id)
                        {
                            for rect in selection.visible_rects(hit.inner_rect, hit.scroll) {
                                occlusion.cover(rect);
                            }
                        }
                        crate::ui::render_selection_highlight_styled(
                            self.selection.as_ref(),
                            &mut composed,
                            &hit.pane_id,
                            hit.inner_rect,
                            hit.scroll,
                            &self.config.palette,
                            &self.config.components,
                            crate::terminal_theme::TerminalTheme {
                                background: self.host_background,
                                ..Default::default()
                            },
                        );
                    }
                    if copy_surface_coherent {
                        render_client_copy_search_highlights(
                            &mut composed,
                            self.copy_mode.as_ref(),
                            hit,
                            &self.config.palette,
                            true,
                            occlusion,
                        );
                    }
                }
            }
            if has_link_hints {
                self.render_link_hints(&mut composed, occlusion);
            }
            if let Some((x, y)) = copy_cursor_cell {
                occlusion.cover(Rect::new(x, y, 1, 1));
                composed[(x, y)].set_style(
                    Style::default()
                        .fg(match self.config.palette.panel_bg {
                            ratatui::style::Color::Reset => self.config.palette.surface_dim,
                            color => color,
                        })
                        .bg(self.config.palette.accent)
                        .add_modifier(Modifier::BOLD),
                );
            }
            frame.replace_from_ratatui_buffer_preserving_effects(&composed, cursor);
        }
        self.render_link_hover(frame, occlusion);
        Some(())
    }

    pub(super) fn paint_shell_overlays(
        &mut self,
        frame: &mut FrameData,
        occlusion: &mut crate::kitty_graphics::surface::Occlusion,
    ) -> Option<()> {
        let compose_now = self
            .last_composed_at
            .unwrap_or_else(std::time::Instant::now);
        let spinner = self.spinner_glyph();
        let cx = super::feedback::ChromeContext {
            page_bounds: self.floating_page_rect(frame.width, frame.height),
            palette: &self.config.palette,
            components: &self.config.components,
            glyphs: self.config.border_glyphs,
            hover: self.hover.as_ref(),
            spinner,
            now: compose_now,
        };
        let layout = self.layout(frame.width, frame.height);
        if self.mode == ClientShellMode::Prefix && self.config.which_key && self.overlay.is_none() {
            let cursor = frame.cursor.clone();
            let mut composed = frame.to_ratatui_buffer()?;
            super::which_key::render_which_key(
                &mut composed,
                Rect::new(
                    layout.pane_surface.x,
                    layout.pane_surface.y,
                    layout.pane_surface.width,
                    layout.pane_surface.height.saturating_sub(1),
                ),
                &self.config.keybinds,
                &cx,
                occlusion,
            );
            frame.replace_from_ratatui_buffer_preserving_effects(&composed, cursor);
        }
        self.hits.overlay_bounds = Rect::default();
        self.hits.overlay_kind = None;
        let snapshot = self.snapshot.as_deref();
        if let Some(overlay) = self.overlay.as_ref() {
            let mut composed = frame.to_ratatui_buffer()?;
            let entrance_area: Rect;
            let cursor = if let ClientShellOverlay::ContextMenu(menu) = overlay {
                let rendered = render::render_context_menu(&mut composed, menu, &cx)
                    .unwrap_or_else(|| render::render_minimum_overlay(&mut composed, &cx));
                entrance_area = rendered.area;
                occlusion.cover(rendered.area);
                self.hits.context_menu_rows = rendered.menu_rows;
                None
            } else {
                let rendered = render::render_client_overlay(
                    &mut composed,
                    overlay,
                    snapshot,
                    &self.endpoints,
                    &self.saved_profiles,
                    &self.broadcast,
                    &self.endpoint_connection_errors,
                    &self.endpoint_port_forwards,
                    &self.session_log_dropped,
                    &self.active_endpoint_id,
                    &self.config.keybinds,
                    &self.notification_history,
                    &self.observability,
                    &cx,
                )
                .unwrap_or_else(|| render::render_minimum_overlay(&mut composed, &cx));
                entrance_area = rendered.area;
                occlusion.cover(rendered.area);
                self.hits.overlay_primary = rendered.primary;
                self.hits.overlay_clear = rendered.clear;
                self.hits.overlay_cancel = rendered.cancel;
                self.hits.usage_dashboard_actions = rendered.usage_dashboard_actions;
                self.hits.menu_popup = rendered.menu_popup;
                self.hits.menu_search = rendered.menu_search;
                self.hits.menu_scroll = rendered.menu_scroll;
                self.hits.global_menu_rows = rendered.menu_rows;
                self.hits.navigator_popup = rendered.navigator_popup;
                self.hits.navigator_search = rendered.navigator_search;
                self.hits.navigator_rows = rendered.navigator_rows;
                self.hits.worktree_search = rendered.worktree_search;
                self.hits.worktree_rows = rendered.worktree_rows;
                self.hits.help_popup = rendered.help_popup;
                self.hits.help_scrollbar = rendered.help_scrollbar;
                self.hits.help_scroll_metrics = rendered.help_scroll_metrics;
                self.hits.help_max_scroll = rendered.help_max_scroll;
                self.hits.settings_popup = rendered.settings_popup;
                self.hits.settings_scroll = rendered.settings_scroll;
                self.hits.settings_tabs = rendered.settings_tabs;
                self.hits.settings_choices = rendered.settings_choices;
                self.hits.machines_popup = rendered.machines_popup;
                self.hits.machines_detail_area = rendered.machines_detail_area;
                self.hits.machines_scroll = rendered.machines_scroll;
                self.hits.machines_scroll_valid = rendered.machines_scroll_valid;
                self.hits.machines_search = rendered.machines_search;
                self.hits.machines_footer = rendered.machines_toast;
                self.hits.machines_rows = rendered.machines_rows;
                self.hits.machines_actions = rendered.machines_actions;
                self.hits.machines_fields = rendered.machines_fields;
                self.hits.machines_wizard_rows = rendered.machines_wizard_rows;
                self.hits.machines_wizard_fields = rendered.machines_wizard_fields;
                self.hits.machines_max_scroll = rendered.machines_max_scroll;
                self.hits.machine_auth_max_scroll = rendered.machine_auth_max_scroll;
                self.hits.machine_auth_actions = rendered.machine_auth_actions;
                self.hits.broadcast_popup = rendered.broadcast_popup;
                self.hits.broadcast_rows = rendered.broadcast_rows;
                self.hits.broadcast_actions = rendered.broadcast_actions;
                self.hits.machine_files_popup = rendered.machine_files_popup;
                self.hits.machine_files_search = rendered.machine_files_search;
                self.hits.machine_files_rows = rendered.machine_files_rows;
                self.hits.machine_files_actions = rendered.machine_files_actions;
                self.hits.snippet_popup = rendered.snippet_popup;
                self.hits.snippet_search = rendered.snippet_search;
                self.hits.snippet_rows = rendered.snippet_rows;
                self.hits.snippet_fields = rendered.snippet_fields;
                self.hits.snippet_actions = rendered.snippet_actions;
                self.hits.scenes_popup = rendered.scenes_popup;
                self.hits.scenes_rows = rendered.scenes_rows;
                self.hits.scenes_fields = rendered.scenes_fields;
                self.hits.scenes_actions = rendered.scenes_actions;
                self.hits.notification_history_rows = rendered.notification_history_rows;
                self.hits.product_announcement_scrollbar = rendered.product_announcement_scrollbar;
                self.hits.product_announcement_scroll_metrics =
                    rendered.product_announcement_scroll_metrics;
                self.hits.product_announcement_max_scroll =
                    rendered.product_announcement_max_scroll;
                self.hits.release_notes_scrollbar = rendered.release_notes_scrollbar;
                self.hits.release_notes_scroll_metrics = rendered.release_notes_scroll_metrics;
                self.hits.release_notes_max_scroll = rendered.release_notes_max_scroll;
                self.hits.overlay_kind = Some(overlay.kind());
                // 浮层自带光标（文本输入）时归浮层；否则只有浮层矩形真正盖住
                // 终端光标才把它抹掉，未覆盖的终端插入点保留（用量仪表盘等）。
                rendered.cursor.or_else(|| {
                    frame
                        .cursor
                        .clone()
                        .filter(|cursor| !contains(rendered.area, (cursor.x, cursor.y)))
                })
            };
            self.hits.overlay_bounds = entrance_area;
            if self.overlay_since.is_some_and(|since| {
                compose_now.duration_since(since) < super::feedback::ENTRANCE_DURATION
            }) && !entrance_area.is_empty()
            {
                composed.set_style(entrance_area, Style::default().add_modifier(Modifier::DIM));
            }
            frame.replace_from_ratatui_buffer_preserving_effects(&composed, cursor);
        }
        if let Some(ClientShellOverlay::CommandPalette(palette)) = self.overlay.as_mut() {
            palette.scroll = self.hits.menu_scroll;
            palette.reveal = false;
        }
        if let Some(ClientShellOverlay::Settings(settings)) = self.overlay.as_mut() {
            settings.scroll = self.hits.settings_scroll;
            settings.reveal = false;
        }
        if let Some(ClientShellOverlay::Machines(page)) = self.overlay.as_mut() {
            // 列表 / 导入向导 / 转发编辑器共用「compose 期回写 scroll」：渲染是
            // 唯一知道可见行数的地方，reveal 是一次性请求，画完即清。本帧没
            // 真的画列表（窗口太小、discover 步骤）时跳过，别把滚动位置清零。
            if self.hits.machines_scroll_valid {
                match &mut page.view {
                    super::machines_overlay::ClientMachinesView::List => {
                        page.scroll = self.hits.machines_scroll;
                        page.reveal = false;
                    }
                    super::machines_overlay::ClientMachinesView::Import(view) => {
                        view.scroll = self.hits.machines_scroll;
                        view.reveal = false;
                    }
                    super::machines_overlay::ClientMachinesView::Forwards(view) => {
                        view.scroll = self.hits.machines_scroll;
                        view.reveal = false;
                    }
                    _ => {}
                }
            }
            if matches!(
                page.view,
                super::machines_overlay::ClientMachinesView::Detail(_)
            ) || !self.hits.machines_detail_area.is_empty()
            {
                page.detail_scroll = page.detail_scroll.min(self.hits.machines_max_scroll);
            }
        }
        if let Some(ClientShellOverlay::Help(help)) = self.overlay.as_mut() {
            help.scroll = help.scroll.min(self.hits.help_max_scroll);
        }
        if let Some(ClientShellOverlay::ProductAnnouncement(announcement)) = self.overlay.as_mut() {
            announcement.scroll = announcement
                .scroll
                .min(u16::try_from(self.hits.product_announcement_max_scroll).unwrap_or(u16::MAX));
        }
        if let Some(ClientShellOverlay::ReleaseNotes(notes)) = self.overlay.as_mut() {
            notes.scroll = notes
                .scroll
                .min(u16::try_from(self.hits.release_notes_max_scroll).unwrap_or(u16::MAX));
        }
        Some(())
    }
}

fn client_copy_surface_coherent(copy_mode: Option<&ClientCopyModeState>, hit: &PaneHit) -> bool {
    copy_mode
        .filter(|copy_mode| copy_mode.pane_id == hit.pane_id)
        .is_none_or(|copy_mode| {
            copy_mode.geometry == (hit.inner_rect.width, hit.inner_rect.height)
                && hit.scroll.is_some_and(|scroll| {
                    scroll.offset_from_bottom == copy_mode.offset_from_bottom
                        && scroll.max_offset_from_bottom == copy_mode.max_offset_from_bottom
                })
        })
}

fn render_client_copy_search_highlights(
    buffer: &mut Buffer,
    copy_mode: Option<&ClientCopyModeState>,
    hit: &PaneHit,
    palette: &Palette,
    current_only: bool,
    occlusion: &mut crate::kitty_graphics::surface::Occlusion,
) {
    let Some(copy_mode) = copy_mode.filter(|copy_mode| copy_mode.pane_id == hit.pane_id) else {
        return;
    };
    if hit.inner_rect.is_empty() {
        return;
    }
    let top = copy_mode
        .max_offset_from_bottom
        .saturating_sub(copy_mode.offset_from_bottom)
        .min(u32::MAX as usize) as u32;
    let bottom = top.saturating_add(u32::from(hit.inner_rect.height.saturating_sub(1)));
    let style = if current_only {
        Style::default()
            .fg(panel_contrast_fg(palette))
            .bg(palette.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(palette.text).bg(palette.surface1)
    };
    for (index, text_match) in copy_mode.search_matches.iter().enumerate() {
        if (copy_mode.search_current == Some(index)) != current_only
            || text_match.end.row < top
            || text_match.start.row > bottom
        {
            continue;
        }
        let start_row = text_match.start.row.max(top);
        let end_row = text_match.end.row.min(bottom);
        for absolute_row in start_row..=end_row {
            let viewport_row = absolute_row.saturating_sub(top) as u16;
            let start_col = if absolute_row == text_match.start.row {
                text_match.start.col
            } else {
                0
            };
            let end_col = if absolute_row == text_match.end.row {
                text_match.end.col
            } else {
                hit.inner_rect.width.saturating_sub(1)
            };
            let end_col = end_col.min(hit.inner_rect.width.saturating_sub(1));
            occlusion.cover(Rect::new(
                hit.inner_rect.x.saturating_add(start_col),
                hit.inner_rect.y.saturating_add(viewport_row),
                end_col.saturating_add(1).saturating_sub(start_col),
                1,
            ));
            for col in start_col..=end_col {
                buffer[(
                    hit.inner_rect.x.saturating_add(col),
                    hit.inner_rect.y.saturating_add(viewport_row),
                )]
                    .set_style(style);
            }
        }
    }
}

pub(super) fn client_popup_size(
    size: crate::protocol::ClientShellPopupSize,
) -> crate::popup_size::PopupSize {
    match size {
        crate::protocol::ClientShellPopupSize::Cells(cells) => {
            crate::popup_size::PopupSize::Cells(cells)
        }
        crate::protocol::ClientShellPopupSize::Percent(percent) => {
            crate::popup_size::PopupSize::Percent(percent)
        }
    }
}

/// Visual bell: repaint the focused pane's border frame with a brief
/// emphasis color. Only cells in `rect` outside `inner_rect` are touched,
/// so terminal content is preserved byte-for-byte.
pub(super) fn emphasize_pane_border(
    buffer: &mut Buffer,
    hit: &PaneHit,
    color: ratatui::style::Color,
) {
    let inner = hit.inner_rect;
    let area = hit.rect.intersection(buffer.area);
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            if x >= inner.x && x < inner.right() && y >= inner.y && y < inner.bottom() {
                continue;
            }
            let cell = &mut buffer[(x, y)];
            cell.set_style(cell.style().fg(color).add_modifier(Modifier::BOLD));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame_with_cursor(x: u16, y: u16) -> FrameData {
        let buffer = Buffer::empty(Rect::new(0, 0, 20, 4));
        FrameData::from_ratatui_buffer_with_hyperlinks(
            &buffer,
            Some(crate::protocol::CursorState {
                x,
                y,
                visible: true,
                shape: 0,
            }),
            &[],
        )
    }

    #[test]
    fn restore_mode_bar_only_hides_a_cursor_inside_the_bar_columns() {
        let bar = Rect::new(10, 3, 5, 1);
        let cells = frame_with_cursor(0, 0).cells[..5].to_vec();
        let mut covered = frame_with_cursor(12, 3);
        restore_mode_bar(&mut covered, Some(bar), Some(&cells));
        assert!(covered.cursor.is_none(), "模式条列区间内的光标被抹掉");
        let mut beside = frame_with_cursor(2, 3);
        restore_mode_bar(&mut beside, Some(bar), Some(&cells));
        assert!(
            beside.cursor.is_some(),
            "同一行但不在模式条列区间内的光标保留"
        );
    }
}
