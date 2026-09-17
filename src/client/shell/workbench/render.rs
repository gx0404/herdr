use super::super::observability::{tr, Page};
use super::interaction::Action;
use super::*;
use ratatui::widgets::{Block, BorderType, Borders, Widget};

fn put(buffer: &mut Buffer, area: Rect, text: &str, style: Style) {
    if area.is_empty() {
        return;
    }
    buffer.set_stringn(area.x, area.y, text, usize::from(area.width), style);
}

fn translated(rect: crate::protocol::SurfaceRect, area: Rect) -> Rect {
    Rect::new(
        area.x.saturating_add(rect.x),
        area.y.saturating_add(rect.y),
        rect.width,
        rect.height,
    )
    .intersection(area)
}

pub(super) fn pane_hit(pane: &crate::protocol::PaneSurfacePane, area: Rect) -> PaneHit {
    PaneHit {
        rect: translated(pane.rect, area),
        inner_rect: translated(pane.inner_rect, area),
        scrollbar_rect: pane.scrollbar_rect.map(|rect| translated(rect, area)),
        scroll: pane.scroll.map(|value| crate::pane::ScrollMetrics {
            offset_from_bottom: value.offset_from_bottom as usize,
            max_offset_from_bottom: value.max_offset_from_bottom as usize,
            viewport_rows: value.viewport_rows as usize,
        }),
        pane_id: pane.pane_id.clone(),
        popup: false,
        mouse_reporting: pane.mouse_reporting,
        sgr_pixel_mouse: pane.sgr_pixel_mouse,
        pixel_width: pane.pixel_width,
        pixel_height: pane.pixel_height,
    }
}

impl ClientShellState {
    pub(in crate::client::shell) fn compose_workbench(
        &mut self,
        cols: u16,
        rows: u16,
    ) -> Option<FrameData> {
        let snapshot = self.snapshot.as_deref()?;
        let projection_revision = snapshot.revision;
        let spinner = self.spinner_glyph();
        let visual_bell = self.visual_bell_active();
        let broadcast_count = self.broadcast_indicator_count();
        let palette = self.config.palette.clone();
        let palette = &palette;
        let full = Rect::new(0, 0, cols, rows);
        let mut buffer = Buffer::empty(full);
        buffer.set_style(full, Style::default().fg(palette.text).bg(palette.panel_bg));
        self.hits = ShellHitMap::default();
        self.workbench.hits.clear();
        self.workbench.geometry =
            self.workbench
                .dock
                .geometry(Rect::new(0, 1.min(rows), cols, rows.saturating_sub(2)));
        let mut x = 0;
        for (label, action) in [
            (" herdr ≡ ", Action::Menu),
            (tr(" Monitor ", " 监控 "), Action::Open(PanelId::Monitor)),
            (tr(" Accounts ", " 用量 "), Action::Open(PanelId::Accounts)),
            (tr(" Layout ", " 布局 "), Action::Arrange),
            (
                if self.workbench.dock.locked {
                    tr(" Unlock ", " 解锁 ")
                } else {
                    tr(" Lock ", " 锁定 ")
                },
                Action::Lock,
            ),
            (tr(" Reset ", " 复位 "), Action::Reset),
        ]
        .into_iter()
        .filter(|(_, action)| self.workbench.arranging || !matches!(action, Action::Reset))
        {
            let width = (label.width() as u16).min(cols.saturating_sub(x));
            let rect = Rect::new(x, 0, width, u16::from(rows > 0));
            put(
                &mut buffer,
                rect,
                label,
                Style::default()
                    .fg(palette.accent)
                    .bg(palette.surface0)
                    .add_modifier(Modifier::BOLD),
            );
            if matches!(action, Action::Menu) {
                self.hits.global_launcher = rect;
            }
            self.workbench.hits.push((rect, action));
            x = x.saturating_add(width).saturating_add(1);
        }
        let workspace = self
            .workbench
            .geometry
            .panels
            .iter()
            .find(|(id, _)| *id == PanelId::Workspaces)
            .map(|(id, area)| body(*area, id))
            .unwrap_or_default();
        let agents = self
            .workbench
            .geometry
            .panels
            .iter()
            .find(|(id, _)| *id == PanelId::Agents)
            .map(|(id, area)| body(*area, id))
            .unwrap_or_default();
        let mut sidebar_state = super::super::render::ShellRenderState {
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
            sidebar_section_split: 0.5,
            tab_drag_insert_index: None,
            selected_workspace_id: self.navigate_workspace_id.as_ref(),
            reveal_navigation_workspace: &mut self.reveal_navigation_workspace,
            dragged_workspace_id: None,
            workspace_drop_indicator_row: None,
            chrome_hover: self.hover.as_ref(),
            spinner,
        };
        if !workspace.is_empty() || !agents.is_empty() {
            super::super::endpoint_sidebar::render_expanded_regions(
                &mut buffer,
                workspace,
                Some(snapshot),
                &self.config,
                &mut sidebar_state,
                &mut self.hits,
                Some((workspace, agents)),
            );
        }
        self.hits.sidebar_divider = Rect::default();
        self.hits.sidebar_section_divider = Rect::default();
        self.hits.sidebar_toggle = Rect::default();
        for (panel, area) in &self.workbench.geometry.panels {
            if area.is_empty() {
                continue;
            }
            let focused = panel == &self.workbench.dock.focused;
            let color = if focused {
                palette.accent
            } else {
                palette.overlay0
            };
            let label = match panel {
                PanelId::Workspaces => tr("⠿ WORKSPACES", "⠿ 工作区").to_string(),
                PanelId::Agents => "⠿ Agents".into(),
                PanelId::Monitor => tr("⠿ SYSTEM", "⠿ 系统监控").to_string(),
                PanelId::Accounts => tr("⠿ ACCOUNTS", "⠿ 账号用量").to_string(),
                PanelId::Terminal(id) => format!("⠿ {} {id}", tr("TERMINALS", "终端组")),
            };
            let header = Rect::new(area.x, area.y, area.width, 1);
            buffer.set_style(header, Style::default().bg(palette.surface0));
            put(
                &mut buffer,
                Rect::new(area.x, area.y, area.width.saturating_sub(3), 1),
                &label,
                Style::default()
                    .fg(color)
                    .bg(palette.surface0)
                    .add_modifier(Modifier::BOLD),
            );
            self.workbench
                .hits
                .push((header, Action::Header(panel.clone())));
            let toggle = Rect::new(
                area.right().saturating_sub(3).max(area.x),
                area.y,
                3.min(area.width),
                1,
            );
            put(
                &mut buffer,
                toggle,
                if self.workbench.dock.maximized.is_some() {
                    " ◫ "
                } else {
                    " □ "
                },
                Style::default().fg(color),
            );
            self.workbench
                .hits
                .push((toggle, Action::Maximize(panel.clone())));
            if let PanelId::Terminal(id) = panel {
                let Some(group) = self
                    .workbench
                    .dock
                    .groups
                    .iter()
                    .find(|group| group.id == *id)
                else {
                    continue;
                };
                let tabs = group
                    .tabs
                    .iter()
                    .filter_map(|id| snapshot.tabs.iter().find(|tab| &tab.tab_id == id))
                    .collect::<Vec<_>>();
                let scroll = self.workbench.tab_scroll.entry(*id).or_default();
                let stamp = (group.active.clone(), area.width);
                let mut reveal = self.workbench.tab_focus.get(id) != Some(&stamp);
                self.workbench.tab_focus.insert(*id, stamp);
                let mut strip_hits = ShellHitMap::default();
                super::super::render::render_tab_strip(
                    &mut buffer,
                    Rect::new(area.x, area.y + 1, area.width, u16::from(area.height > 1)),
                    &self.config,
                    scroll,
                    &mut reveal,
                    super::super::render::TabStripContext {
                        tabs: &tabs,
                        focused: group.active.as_deref(),
                        status: focused.then_some(snapshot),
                        hover: self.hover.as_ref(),
                        visual_bell: visual_bell && focused,
                        insert_index: None,
                    },
                    &mut strip_hits,
                );
                for (rect, tab) in &strip_hits.tabs {
                    self.workbench.hits.push((
                        *rect,
                        Action::Tab {
                            group: *id,
                            tab: tab.clone(),
                        },
                    ));
                }
                for (rect, action) in [
                    (strip_hits.tab_scroll_left, Action::ScrollTabs(*id, -1)),
                    (strip_hits.tab_scroll_right, Action::ScrollTabs(*id, 1)),
                    (strip_hits.new_tab, Action::NewTab(*id)),
                ] {
                    if !rect.is_empty() {
                        self.workbench.hits.push((rect, action));
                    }
                }
                self.hits.tabs.extend(strip_hits.tabs);
            }
        }
        for divider in &self.workbench.geometry.dividers {
            for y in divider.handle.y..divider.handle.bottom() {
                for x in divider.handle.x..divider.handle.right() {
                    buffer[(x, y)]
                        .set_symbol(if divider.axis == dock::Axis::Horizontal {
                            "│"
                        } else {
                            "─"
                        })
                        .set_style(Style::default().fg(palette.surface1));
                }
            }
        }
        let footer = Rect::new(0, rows.saturating_sub(1), cols, u16::from(rows > 0));
        let hint = if self.workbench.arranging {
            tr("LAYOUT · Tab focus · arrows resize · Shift+arrows move · Enter maximize · Esc done", "布局 · Tab 切换 · 方向键调尺寸 · Shift+方向键移动 · Enter 最大化 · Esc 完成")
        } else if self.workbench.geometry.compact {
            tr(
                "Compact view · use Layout / Tab to switch panels",
                "紧凑视图 · 在「布局」模式用 Tab 切换面板",
            )
        } else {
            tr(
                "Drag ⠿ to dock · drag borders to resize · drag tabs to split or regroup",
                "拖动 ⠿ 停靠 · 拖动分隔线调尺寸 · 拖动标签拆分或归组",
            )
        };
        put(
            &mut buffer,
            footer,
            hint,
            Style::default().fg(palette.overlay0),
        );
        let mut frame = FrameData::from_ratatui_buffer_with_hyperlinks(&buffer, None, &[]);
        let mut occlusion = crate::kitty_graphics::surface::Occlusion::default();
        self.observability.hits.clear();
        self.observability.page_rect = Rect::default();
        let saved_page = self.observability.page;
        for (panel, rect) in &self.workbench.geometry.panels {
            let area = body(*rect, panel);
            if let PanelId::Terminal(id) = panel {
                let Some(view) = self
                    .workbench
                    .views
                    .get(&id.to_string())
                    .filter(|view| view.surface.projection_revision == projection_revision)
                else {
                    if let Some(mut buffer) = frame.to_ratatui_buffer() {
                        put(
                            &mut buffer,
                            area,
                            tr("Waiting for terminal…", "正在同步终端…"),
                            Style::default().fg(palette.overlay0),
                        );
                        let cursor = frame.cursor.clone();
                        frame.replace_from_ratatui_buffer_preserving_effects(&buffer, cursor);
                    }
                    continue;
                };
                let cursor = frame.cursor.clone();
                blit_pane_surface(&mut frame, &view.surface.frame, area);
                if panel != &self.workbench.dock.focused {
                    frame.cursor = cursor;
                }
                self.hits
                    .panes
                    .extend(view.surface.panes.iter().map(|pane| pane_hit(pane, area)));
                let signature = pane_surface_topology_signature(&view.surface);
                self.hits
                    .pane_splits
                    .extend(view.surface.splits.iter().map(|split| PaneSplitHit {
                        tab_id: Some(view.tab.clone()),
                        direction: split.direction,
                        pos: split.pos.saturating_add(
                            if split.direction
                                == crate::protocol::PaneSurfaceSplitDirection::Horizontal
                            {
                                area.x
                            } else {
                                area.y
                            },
                        ),
                        area: translated(split.area, area),
                        hit_rect: translated(split.hit_rect, area),
                        path: split.path.clone(),
                        topology_signature: signature,
                    }));
            } else if matches!(panel, PanelId::Monitor | PanelId::Accounts) {
                self.observability.page = Some(if *panel == PanelId::Accounts {
                    Page::Accounts
                } else if saved_page == Some(Page::Settings) {
                    Page::Settings
                } else {
                    Page::Monitor
                });
                let previous_hits = std::mem::take(&mut self.observability.hits);
                if let Some(covered) = self.observability.paint(&mut frame, area, palette) {
                    occlusion.cover(covered);
                }
                self.observability.hits.splice(0..0, previous_hits);
            }
        }
        self.observability.page = match self.workbench.dock.focused {
            PanelId::Monitor => Some(if saved_page == Some(Page::Settings) {
                Page::Settings
            } else {
                Page::Monitor
            }),
            PanelId::Accounts => Some(Page::Accounts),
            _ => None,
        };
        if let Some(mut composed) = frame.to_ratatui_buffer() {
            for hit in &self.hits.panes {
                if hit.rect.width > 4
                    && !self.workbench.dock.locked
                    && (hit.inner_rect.y > hit.rect.y || self.workbench.arranging)
                {
                    let handle = Rect::new(hit.rect.x + 1, hit.rect.y, 2, 1);
                    put(
                        &mut composed,
                        handle,
                        "⠿",
                        Style::default().fg(palette.accent),
                    );
                    self.workbench
                        .hits
                        .push((handle, Action::Pane(hit.pane_id.clone())));
                    occlusion.cover(handle);
                }
            }
            if let Some((target, edge)) = self
                .workbench
                .drag
                .as_ref()
                .and_then(|drag| drag.drop_target(&self.workbench.geometry))
            {
                if let Some((_, area)) = self
                    .workbench
                    .geometry
                    .panels
                    .iter()
                    .find(|(panel, _)| panel == &target)
                {
                    let area = super::interaction::preview(*area, edge);
                    Block::default()
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(palette.accent))
                        .title(tr(" Drop here ", " 放到这里 "))
                        .render(area, &mut composed);
                    occlusion.cover(area);
                }
            }
            if let Some(error) = self.endpoint_error.as_deref() {
                put(
                    &mut composed,
                    footer,
                    error,
                    Style::default().fg(palette.red),
                );
            }
            let cursor = frame.cursor.clone();
            frame.replace_from_ratatui_buffer_preserving_effects(&composed, cursor);
        }
        if visual_bell {
            if let Some(hit) = snapshot
                .focused_pane_id
                .as_ref()
                .and_then(|id| self.hits.panes.iter().find(|hit| &hit.pane_id == id))
            {
                let cursor = frame.cursor.clone();
                if let Some(mut composed) = frame.to_ratatui_buffer() {
                    super::super::composition::emphasize_pane_border(
                        &mut composed,
                        hit,
                        palette.yellow,
                    );
                    frame.replace_from_ratatui_buffer_preserving_effects(&composed, cursor);
                }
            }
        }
        self.paint_frozen_selection(&mut frame, &mut occlusion);
        self.paint_shell_copy(&mut frame, &mut occlusion)?;
        if let Some(mut composed) = frame.to_ratatui_buffer() {
            if self.overlay.is_none() {
                if let Some(bar) = super::super::render::render_mode_bar(
                    &mut composed,
                    full,
                    self.mode,
                    self.copy_mode.as_ref(),
                    self.endpoint_error.as_deref(),
                    false,
                    broadcast_count,
                    &self.config.keybinds,
                    palette,
                    &self.config.components,
                ) {
                    occlusion.cover(bar);
                }
                let cursor = frame.cursor.clone();
                frame.replace_from_ratatui_buffer_preserving_effects(&composed, cursor);
            }
        }
        self.hits.popup = None;
        if let Some((area, popup)) = self
            .workbench
            .geometry
            .panels
            .iter()
            .find(|(panel, _)| panel == &self.workbench.dock.focused)
            .and_then(|(panel, area)| {
                self.workbench
                    .focused_view()?
                    .surface
                    .popup
                    .as_deref()
                    .map(|popup| (body(*area, panel), popup))
            })
        {
            if let Some(geometry) = crate::popup_size::resolve_popup_geometry(
                popup
                    .width
                    .map(super::super::composition::client_popup_size),
                popup
                    .height
                    .map(super::super::composition::client_popup_size),
                area,
            ) {
                occlusion.start_popup(geometry.outer);
                let mut composed = frame.to_ratatui_buffer()?;
                ratatui::widgets::Clear.render(geometry.outer, &mut composed);
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .title(popup.title.clone())
                    .border_style(Style::default().fg(palette.accent))
                    .render(geometry.outer, &mut composed);
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
        if self.observability.page.is_none()
            || self.observability.process_dialog.is_some()
            || self
                .observability
                .hover
                .as_ref()
                .is_some_and(|hover| hover.visible)
        {
            // 全局浮层在终端内容、把手与选择高亮之后绘制，保持视觉与输入层级一致。
            let page = self.observability.page.take();
            let page_rect = self.observability.page_rect;
            let hits = std::mem::take(&mut self.observability.hits);
            if let Some(covered) = self.observability.paint(&mut frame, full, palette) {
                occlusion.cover(covered);
            }
            self.observability.page = page;
            self.observability.page_rect = page_rect;
            if self.observability.process_dialog.is_none() {
                self.observability.hits.splice(0..0, hits);
            }
        }
        self.paint_shell_feedback(
            &mut frame,
            ClientShellLayout {
                sidebar: Rect::default(),
                tab_bar: Rect::default(),
                mobile_header: Rect::default(),
                pane_surface: full,
            },
            &mut occlusion,
        )?;
        self.paint_shell_overlays(&mut frame, &mut occlusion)?;
        let mut graphics = std::mem::take(&mut self.workbench.cleanup);
        for (id, view) in &mut self.workbench.views {
            let area = self.workbench.geometry.panels.iter().find(|(panel, _)| matches!(panel, PanelId::Terminal(group) if group.to_string() == *id)).map(|(panel, area)| body(*area, panel));
            let visible = area.is_some()
                && view.surface.projection_revision == projection_revision
                && self.endpoint_error.is_none();
            let area = area.unwrap_or_default();
            let popup = self
                .hits
                .popup
                .as_ref()
                .filter(|_| {
                    self.workbench.dock.focused == PanelId::Terminal(id.parse().unwrap_or(0))
                })
                .map(|hit| (hit.inner_rect.x, hit.inner_rect.y));
            let visibility = if !visible {
                crate::kitty_graphics::surface::Visibility::Hidden
            } else if popup.is_some() {
                crate::kitty_graphics::surface::Visibility::Popup
            } else {
                crate::kitty_graphics::surface::Visibility::Main
            };
            graphics.extend(view.graphics.encode(
                visibility,
                (area.x, area.y),
                popup,
                self.graphics_cell_size,
                &occlusion,
            ));
        }
        frame.graphics = graphics;
        Some(frame)
    }
}
