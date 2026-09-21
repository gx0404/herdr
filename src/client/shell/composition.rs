use super::*;

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

    fn compose_unavailable(
        &mut self,
        cols: u16,
        rows: u16,
    ) -> super::compose_canvas::ComposeCanvas {
        let layout = self.layout(cols, rows);
        let mut canvas = super::compose_canvas::ComposeCanvas::reuse_or_new(
            self.compose_buffer.take(),
            cols,
            rows,
        );
        canvas.buffer().set_style(
            Rect::new(0, 0, cols, rows),
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
                canvas.buffer(),
                sidebar,
                snapshot,
                &self.config,
                &mut render_state,
                &mut self.hits,
            );
        } else {
            super::endpoint_sidebar::render_expanded(
                canvas.buffer(),
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
                canvas.buffer(),
                message_area.x,
                message_area.y,
                message_area.width,
                &message,
                Style::default().fg(self.config.palette.overlay0),
            );
        }
        render::render_mode_bar(
            canvas.buffer(),
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
        canvas
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
        // 渲染前的显式视图计算：滚动窗口与 reveal 在这里更新，渲染只读
        // （STATE-04 / ARCH-02 / TOOL-13）。
        self.compute_overlay_view(cols, rows);
        if self.workbench.enabled {
            return self.compose_workbench(cols, rows);
        }
        let valid_navigation_target = self.mode == ClientShellMode::Navigate
            && self
                .navigate_workspace_id
                .as_ref()
                .is_some_and(|target| self.navigation_target_valid(target));
        if self.snapshot.is_none() || self.pane_surface.is_none() {
            let mut canvas = self.compose_unavailable(cols, rows);
            let area = self.layout(cols, rows).pane_surface;
            self.paint_observability(&mut canvas, area);
            let mut occlusion = crate::kitty_graphics::surface::Occlusion::default();
            self.paint_shell_feedback(&mut canvas, self.layout(cols, rows), &mut occlusion)?;
            self.paint_shell_overlays(&mut canvas, &mut occlusion)?;
            let (frame, buffer) = canvas.finish(Vec::new());
            self.compose_buffer = Some(buffer);
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
        // 单 Buffer 管线（批 12b）：chrome 与 pane 内容画进同一个保留 Buffer，
        // 各装饰段直写，收尾只做一次 Buffer→FrameData 转换。
        let mut canvas = super::compose_canvas::ComposeCanvas::reuse_or_new(
            self.compose_buffer.take(),
            cols,
            rows,
        );
        self.hits = render::render_shell(
            canvas.buffer(),
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
                canvas.buffer(),
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
        let mode_bar_cells = mode_bar.map(|bar| canvas.save_cells(bar));
        canvas.blit_frame(&surface.frame, layout.pane_surface);
        if visual_bell {
            if let Some(focused_pane_id) = snapshot.focused_pane_id.as_deref() {
                if let Some(hit) = self
                    .hits
                    .panes
                    .iter()
                    .find(|hit| hit.pane_id == focused_pane_id)
                    .cloned()
                {
                    // 聚焦边框强调只改样式不改符号，链接登记不受影响。
                    emphasize_pane_border(canvas.buffer(), &hit, self.config.palette.yellow);
                }
            }
        }
        if let Some(bar) = mode_bar {
            if let Some(cells) = mode_bar_cells.as_deref() {
                canvas.restore_cells(bar, cells);
            }
        }
        let mut occlusion = crate::kitty_graphics::surface::Occlusion::default();
        self.paint_frozen_selection(&mut canvas, &mut occlusion);
        self.paint_shell_copy(&mut canvas, &mut occlusion)?;
        if let Some(bar) = mode_bar {
            if let Some(cells) = mode_bar_cells.as_deref() {
                canvas.restore_cells(bar, cells);
            }
        }
        self.paint_shell_feedback(&mut canvas, layout, &mut occlusion)?;
        if let Some(covered) = self.paint_observability(&mut canvas, layout.pane_surface) {
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
                let composed = canvas.buffer();
                let block = ratatui::widgets::Block::default()
                    .borders(ratatui::widgets::Borders::ALL)
                    .border_set(self.config.border_glyphs.border_set())
                    .border_style(ratatui::style::Style::default().fg(self.config.palette.accent))
                    .title(popup.title.clone())
                    .style(ratatui::style::Style::default().bg(self.config.palette.panel_bg));
                ratatui::widgets::Widget::render(ratatui::widgets::Clear, geometry.outer, composed);
                ratatui::widgets::Widget::render(block, geometry.outer, composed);
                canvas.set_cursor(None);
                canvas.blit_frame(&popup.frame, geometry.inner);
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
            let composed = canvas.buffer();
            occlusion.cover(composed.area);
            super::mobile::render_mobile_switcher(
                composed,
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
                    composed,
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
                    composed,
                    Rect::new(0, 0, cols, rows),
                    notice,
                    active_lifecycle.is_some(),
                    &self.config.palette,
                    &self.config.components,
                );
            }
            canvas.set_cursor(None);
            self.hits.panes.clear();
            self.hits.pane_splits.clear();
            self.hits.popup = None;
        }
        if let Some(bar) = mode_bar {
            if let Some(cells) = mode_bar_cells.as_deref() {
                canvas.restore_cells(bar, cells);
            }
            occlusion.cover(bar);
        }
        self.paint_shell_overlays(&mut canvas, &mut occlusion)?;
        if self.endpoint_status(&self.active_endpoint_id) != Some(ClientEndpointStatus::Online) {
            canvas.set_cursor(None);
            self.hits.panes.clear();
            self.hits.pane_splits.clear();
            self.hits.popup = None;
        }
        let (mut frame, buffer) = canvas.finish(Vec::new());
        self.compose_buffer = Some(buffer);
        self.compose_graphics(&mut frame, layout, &occlusion);
        self.hits.composed = true;
        Some(frame)
    }
    pub(super) fn paint_shell_feedback(
        &mut self,
        canvas: &mut super::compose_canvas::ComposeCanvas,
        layout: ClientShellLayout,
        occlusion: &mut crate::kitty_graphics::surface::Occlusion,
    ) -> Option<()> {
        let (cols, rows) = canvas.size();
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
            let composed = canvas.buffer();
            if let Some(diagnostic) = self.config_diagnostic.as_deref() {
                let diagnostic_area = if layout.mobile_header.is_empty() {
                    Rect::new(0, 0, cols, rows)
                } else {
                    layout.pane_surface
                };
                crate::ui::render_config_diagnostic_buffer(
                    composed,
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
                        composed,
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
                        composed,
                        Rect::new(0, 0, cols, rows),
                        notice,
                        u16::from(has_config_diagnostic) + lifecycle_offset,
                        &cx,
                    )
                } else {
                    endpoint_notices::render_mobile_banner(
                        composed,
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
                        composed,
                        Rect::new(0, 0, cols, rows),
                        notification,
                        self.config.toast_position,
                        u16::from(has_config_diagnostic) + lifecycle_offset,
                        &cx,
                    )
                } else {
                    notifications::render_mobile_notification_banner(
                        composed,
                        Rect::new(0, 0, cols, rows),
                        notification,
                        notifications::notification_agent_label(&self.endpoints, notification),
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
        }
        if let Some(feedback) = self.copy_feedback.as_ref() {
            let composed = canvas.buffer();
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
                composed,
                feedback_area,
                feedback,
                offset,
                self.config.clipboard_toast_position,
                &self.config.palette,
                &self.config.components,
            ));
        }
        Some(())
    }

    pub(super) fn paint_shell_copy(
        &self,
        canvas: &mut super::compose_canvas::ComposeCanvas,
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
        let (frame_width, frame_height) = canvas.size();
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
                    && x < frame_width
                    && y < frame_height)
                    .then_some((x, y))
            })
        } else {
            None
        };
        // Copy 模式整帧无光标（与原逐段行为一致，不论光标格是否在界内）。
        if self.mode == ClientShellMode::Copy {
            canvas.set_cursor(None);
        }
        // 选区/搜索高亮、link hints、copy-mode 光标合并为同一次 Buffer 直写
        //（C-12 第二步后半：单 Buffer 管线），绘制顺序与原逐段顺序一致。
        if has_selection || has_search || has_link_hints || copy_cursor_cell.is_some() {
            // link hints 把标签字符写进格内（改写符号），hints/Copy 模式整帧无光标。
            if has_link_hints || self.mode == ClientShellMode::Copy {
                canvas.set_cursor(None);
            }
            if has_selection || has_search {
                for hit in self.hits.panes.iter().filter(|hit| {
                    selection_owner == Some(hit.pane_id.as_str())
                        || copy_owner == Some(hit.pane_id.as_str())
                }) {
                    let copy_surface_coherent =
                        client_copy_surface_coherent(self.copy_mode.as_ref(), hit);
                    if copy_surface_coherent {
                        render_client_copy_search_highlights(
                            canvas.buffer(),
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
                            canvas.buffer(),
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
                            canvas.buffer(),
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
                self.render_link_hints(canvas.buffer(), occlusion);
            }
            if let Some((x, y)) = copy_cursor_cell {
                occlusion.cover(Rect::new(x, y, 1, 1));
                canvas.buffer()[(x, y)].set_style(
                    Style::default()
                        .fg(match self.config.palette.panel_bg {
                            ratatui::style::Color::Reset => self.config.palette.surface_dim,
                            color => color,
                        })
                        .bg(self.config.palette.accent)
                        .add_modifier(Modifier::BOLD),
                );
            }
        }
        self.render_link_hover(canvas, occlusion);
        Some(())
    }

    pub(super) fn paint_shell_overlays(
        &mut self,
        canvas: &mut super::compose_canvas::ComposeCanvas,
        occlusion: &mut crate::kitty_graphics::surface::Occlusion,
    ) -> Option<()> {
        let compose_now = self
            .last_composed_at
            .unwrap_or_else(std::time::Instant::now);
        let spinner = self.spinner_glyph();
        let (frame_width, frame_height) = canvas.size();
        let cx = super::feedback::ChromeContext {
            page_bounds: self.floating_page_rect(frame_width, frame_height),
            palette: &self.config.palette,
            components: &self.config.components,
            glyphs: self.config.border_glyphs,
            hover: self.hover.as_ref(),
            spinner,
            now: compose_now,
        };
        let (frame_width, frame_height) = canvas.size();
        let layout = self.layout(frame_width, frame_height);
        if self.mode == ClientShellMode::Prefix && self.config.which_key && self.overlay.is_none() {
            super::which_key::render_which_key(
                canvas.buffer(),
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
        }
        self.hits.overlay_bounds = Rect::default();
        self.hits.overlay_kind = None;
        let snapshot = self.snapshot.as_deref();
        if let Some(overlay) = self.overlay.as_ref() {
            let entrance_area: Rect;
            let cursor = if let ClientShellOverlay::ContextMenu(menu) = overlay {
                let rendered = render::render_context_menu(canvas.buffer(), menu, &cx)
                    .unwrap_or_else(|| render::render_minimum_overlay(canvas.buffer(), &cx));
                entrance_area = rendered.area;
                occlusion.cover(rendered.area);
                self.hits.context_menu_rows = rendered.menu_rows;
                None
            } else {
                let rendered = render::render_client_overlay(
                    canvas.buffer(),
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
                .unwrap_or_else(|| render::render_minimum_overlay(canvas.buffer(), &cx));
                entrance_area = rendered.area;
                occlusion.cover(rendered.area);
                self.hits.overlay_primary = rendered.primary;
                self.hits.overlay_clear = rendered.clear;
                self.hits.overlay_cancel = rendered.cancel;
                self.hits.usage_dashboard_actions = rendered.usage_dashboard_actions;
                self.hits.menu_popup = rendered.menu_popup;
                self.hits.menu_search = rendered.menu_search;
                self.hits.global_menu_rows = rendered.menu_rows;
                self.hits.navigator_popup = rendered.navigator_popup;
                self.hits.navigator_search = rendered.navigator_search;
                self.hits.navigator_rows = rendered.navigator_rows;
                self.hits.worktree_search = rendered.worktree_search;
                self.hits.worktree_rows = rendered.worktree_rows;
                self.hits.help_popup = rendered.help_popup;
                self.hits.help_scrollbar = rendered.help_scrollbar;
                self.hits.help_scroll_metrics = rendered.help_scroll_metrics;
                self.hits.settings_popup = rendered.settings_popup;
                self.hits.settings_tabs = rendered.settings_tabs;
                self.hits.settings_choices = rendered.settings_choices;
                self.hits.machines_popup = rendered.machines_popup;
                self.hits.machines_detail_area = rendered.machines_detail_area;
                self.hits.machines_search = rendered.machines_search;
                self.hits.machines_footer = rendered.machines_toast;
                self.hits.machines_rows = rendered.machines_rows;
                self.hits.machines_actions = rendered.machines_actions;
                self.hits.machines_fields = rendered.machines_fields;
                self.hits.machines_wizard_rows = rendered.machines_wizard_rows;
                self.hits.machines_wizard_fields = rendered.machines_wizard_fields;
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
                self.hits.release_notes_scrollbar = rendered.release_notes_scrollbar;
                self.hits.release_notes_scroll_metrics = rendered.release_notes_scroll_metrics;
                self.hits.overlay_kind = Some(overlay.kind());
                // 浮层自带光标（文本输入）时归浮层；否则只有浮层矩形真正盖住
                // 终端光标才把它抹掉，未覆盖的终端插入点保留（用量仪表盘等）。
                rendered.cursor.or_else(|| {
                    canvas
                        .cursor()
                        .filter(|cursor| !contains(rendered.area, (cursor.x, cursor.y)))
                })
            };
            self.hits.overlay_bounds = entrance_area;
            if self.overlay_since.is_some_and(|since| {
                compose_now.duration_since(since) < super::feedback::ENTRANCE_DURATION
            }) && !entrance_area.is_empty()
            {
                canvas
                    .buffer()
                    .set_style(entrance_area, Style::default().add_modifier(Modifier::DIM));
            }
            canvas.set_cursor(cursor);
            // CFP-15：浮层矩形内的格归浮层所有，即使符号巧合未变也不再保留 OSC 8 链接。
            canvas.clear_links_in(entrance_area);
        }
        // 滚动窗口与 `reveal` 已在 `compute_overlay_view`（绘制前）写回状态，这里
        // 不再从渲染输出反写（STATE-04 / ARCH-02 / TOOL-13）。
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

    fn canvas_with_cursor(x: u16, y: u16) -> super::compose_canvas::ComposeCanvas {
        let mut canvas = super::compose_canvas::ComposeCanvas::reuse_or_new(None, 20, 4);
        canvas.set_cursor(Some(crate::protocol::CursorState {
            x,
            y,
            visible: true,
            shape: 0,
        }));
        canvas
    }

    #[test]
    fn restore_cells_only_hides_a_cursor_inside_the_bar_columns() {
        let bar = Rect::new(10, 3, 5, 1);
        let cells = {
            let canvas = canvas_with_cursor(0, 0);
            canvas.save_cells(Rect::new(0, 0, 5, 1))
        };
        let mut covered = canvas_with_cursor(12, 3);
        covered.restore_cells(bar, &cells);
        assert!(covered.cursor().is_none(), "模式条列区间内的光标被抹掉");
        let mut beside = canvas_with_cursor(2, 3);
        beside.restore_cells(bar, &cells);
        assert!(
            beside.cursor().is_some(),
            "同一行但不在模式条列区间内的光标保留"
        );
    }
}
