use super::super::observability::tr;
use super::interaction::Action;
use super::*;
use ratatui::widgets::{Block, Borders, Widget};

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

/// 无边框窗格的拖动把手落点（冒烟 L2）：窗格首行就是终端内容，把手改放到正上方
/// 那一行的空位——面板顶部的窗格是面板头（标签条并在里面），其余是窗格间的空隙行。
/// 在窗格列宽内从左往右找 2 列把手 + 左侧 1 列间隔都满足的位置：不压标签 / 按钮等
/// 命中区与已放的把手，不落进任何窗格矩形（无空隙堆叠时正上方是另一个窗格的内容），
/// 画面上是空白格（避开标题、省略号）。找不到返回 `None`。
fn borderless_pane_handle(
    composed: &Buffer,
    pane: Rect,
    taken: &[(Rect, Action)],
    panes: &[PaneHit],
) -> Option<Rect> {
    let y = pane.y.checked_sub(1)?;
    let last = pane.right().checked_sub(2)?;
    (pane.x.saturating_add(1)..=last)
        .map(|x| Rect::new(x, y, 2, 1))
        .find(|slot| {
            let padded = Rect::new(slot.x - 1, y, slot.width + 1, 1);
            padded.intersection(composed.area) == padded
                // 面板头整行是开始停靠拖动的命中区：把手画在它上面（后登记的在上层）。
                && !taken
                    .iter()
                    .any(|(rect, action)| !matches!(action, Action::Header(_)) && rect.intersects(padded))
                && !panes.iter().any(|hit| hit.rect.intersects(padded))
                && (padded.x..padded.right()).all(|x| composed[(x, y)].symbol() == " ")
        })
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
    ) -> Option<crate::client::frame_output::ComposedFrame> {
        let snapshot = self.snapshot.as_deref()?;
        #[cfg(test)]
        {
            self.workbench.compose_count += 1;
        }
        let now = self
            .last_composed_at
            .unwrap_or_else(std::time::Instant::now);
        // 视图计算：记下第一次看到这个快照修订号的时刻，算出沿用旧帧的视图里最早
        // 到期的宽限（到期由 `tick_workbench` 请求重绘）。渲染只读这些结果。
        if self
            .workbench
            .snapshot_seen
            .is_none_or(|(revision, _)| revision != snapshot.revision)
        {
            self.workbench.snapshot_seen = Some((snapshot.revision, now));
        }
        self.workbench.stale_until = self
            .workbench
            .views
            .iter()
            .filter_map(|(id, view)| {
                match super::presentation(
                    view,
                    self.workbench.stamps.get(id).copied(),
                    self.workbench.snapshot_seen,
                    snapshot,
                    now,
                ) {
                    super::Presentation::Stale { until } => Some(until),
                    _ => None,
                }
            })
            .min();
        let spinner = self.spinner_glyph();
        let status_glyphs = self.status_glyphs();
        let visual_bell = self.visual_bell_active();
        let palette = self.config.palette.clone();
        let palette = &palette;
        let full = Rect::new(0, 0, cols, rows);
        // 单 Buffer 管线（批 12b）：与经典 compose 共用同一个保留 Buffer。
        let mut canvas = super::super::compose_canvas::ComposeCanvas::reuse_or_new(
            self.compose_buffer.take(),
            cols,
            rows,
        );
        canvas
            .buffer()
            .set_style(full, Style::default().fg(palette.text).bg(palette.panel_bg));
        self.hits = ShellHitMap::default();
        self.workbench.hits.clear();
        self.workbench.footer_hits.clear();
        if self.workbench.footer_arranging != self.workbench.arranging {
            // 进出调整布局时状态栏换了一套提示：旧下标不再指向同一条，不留悬浮态。
            self.workbench.footer_arranging = self.workbench.arranging;
            self.workbench.footer_hover = None;
        }
        // 标题栏（第 0 行）：主菜单入口、面包屑、注意力计数、命令提示与 `⋯` 布局菜单。
        let title = self.paint_workbench_title_bar(
            canvas.buffer(),
            Rect::new(0, 0, cols, u16::from(rows > 0)),
            snapshot,
        );
        self.hits.global_launcher = title.launcher;
        self.hits.title_command = title.command;
        self.hits.title_menu = title.menu;
        self.hits.title_arrange = title.arrange;
        for (rect, action) in [
            (title.launcher, Action::Menu),
            (title.arrange, Action::Arrange),
            (title.command, Action::CommandSearch),
            (title.menu, Action::LayoutMenu),
        ] {
            if !rect.is_empty() {
                self.workbench.hits.push((rect, action));
            }
        }
        for (rect, _) in &title.chips {
            self.workbench.hits.push((*rect, Action::Attention));
        }
        self.hits.title_chips = title.chips;
        if let Some(cache) = self.session_rows.as_ref() {
            super::super::chrome::sessions::paint_sessions(
                canvas.buffer(),
                &super::super::chrome::sessions::SessionPaint {
                    cache,
                    view: &self.session_view,
                    grouping: self.session_grouping,
                    config: &self.config,
                    glyphs: status_glyphs,
                    hover: self.hover.as_ref(),
                    spinner,
                    endpoints: &self.endpoints,
                    active_endpoint_id: &self.active_endpoint_id,
                },
                &mut self.hits,
            );
        }
        self.paint_agent_keyboard_focus(canvas.buffer());
        self.hits.sidebar_divider = Rect::default();
        self.hits.sidebar_section_divider = Rect::default();
        self.hits.sidebar_toggle = Rect::default();
        // 面板头：画在各面板自己的底色上，终端组把标签条并进同一行。
        let controls = self.workbench.control_geometry(cols);
        let compact_switcher_shown = super::super::chrome::panel_header::paint_workbench_headers(
            canvas.buffer(),
            &super::super::chrome::panel_header::HeaderFrame {
                dock: &self.workbench.dock,
                geometry: &self.workbench.geometry,
                controls: &controls,
                snapshot,
                config: &self.config,
                hover: self.hover.as_ref(),
                arranging: self.workbench.arranging,
                visual_bell,
                glyphs: status_glyphs,
                stale: self.active_endpoint_stale(),
            },
            super::super::chrome::panel_header::HeaderOutputs {
                tab_scroll: &mut self.workbench.tab_scroll,
                tab_focus: &mut self.workbench.tab_focus,
                actions: &mut self.workbench.hits,
                hits: &mut self.hits,
            },
        );
        for divider in &self.workbench.geometry.dividers {
            for y in divider.handle.y..divider.handle.bottom() {
                for x in divider.handle.x..divider.handle.right() {
                    canvas.buffer()[(x, y)]
                        .set_symbol(if divider.axis == dock::Axis::Horizontal {
                            "│"
                        } else {
                            "─"
                        })
                        .set_style(crate::ui::design::rule(palette));
                }
            }
        }
        // 状态栏（最后一行）：模式标签、上下文键位提示（调整布局模式的「Esc 完成」与
        // 终端模式的导航 / 命令 / 快捷键提示可点）、右侧 `tab_bar_right`；端点错误也画
        // 在这里。悬浮下标由鼠标移动按上一帧的 `footer_hits` 算出，只落在可点项上
        // （复审轻级 B2）。
        let footer = Rect::new(0, rows.saturating_sub(1), cols, u16::from(rows > 0));
        for (rect, index, action) in
            self.paint_workbench_status_bar(canvas.buffer(), footer, compact_switcher_shown)
        {
            self.workbench.hits.push((rect, action));
            self.workbench.footer_hits.push((rect, index));
        }
        let mut occlusion = crate::kitty_graphics::surface::Occlusion::default();
        self.observability.begin_paint();
        let mut stale_panel_areas = Vec::new();
        for (panel, rect) in &self.workbench.geometry.panels {
            let area = body(*rect, panel, self.workbench.geometry.compact);
            if let PanelId::Terminal(id) = panel {
                let view_id = id.to_string();
                let Some(view) = self.workbench.views.get(&view_id).filter(|view| {
                    super::presentation(
                        view,
                        self.workbench.stamps.get(&view_id).copied(),
                        self.workbench.snapshot_seen,
                        snapshot,
                        now,
                    ) != super::Presentation::Unavailable
                }) else {
                    // C-12 (a)：占位不再逐面板整帧往返，先收集区域，与把手/预览合并成一次。
                    stale_panel_areas.push(area);
                    continue;
                };
                let cursor = canvas.cursor();
                canvas.blit_frame(&view.surface.frame, area);
                if panel
                    != self
                        .workbench
                        .dock
                        .projected_focus(&self.workbench.geometry)
                {
                    canvas.set_cursor(cursor);
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
            } else if *panel == PanelId::Agents {
                canvas.clear_links_in(area);
                super::super::chrome::inspector::paint(
                    canvas.buffer(),
                    &self.inspector,
                    &self.config,
                    status_glyphs,
                    self.hover.as_ref(),
                    &mut self.hits,
                );
                occlusion.cover(area);
            } else if *panel == PanelId::Monitor {
                // The monitor panel hosts all observation pages and always
                // paints the tab the user last selected, whether or not the
                // panel is focused; rendering never rewrites `page`.
                let tab = self.observability.monitor_tab;
                let cx = super::super::feedback::ChromeContext {
                    page_bounds: None,
                    palette,
                    components: &self.config.components,
                    glyphs: self.config.border_glyphs,
                    hover: None,
                    spinner,
                    now: self
                        .last_composed_at
                        .unwrap_or_else(std::time::Instant::now),
                };
                if let Some(painted) =
                    self.observability
                        .paint(&mut canvas, area, &cx, Some(tab), false)
                {
                    occlusion.cover(painted.covered);
                    self.observability.commit_paint(painted);
                }
            }
        }
        // 窗格的 `⠿` 把手只在拖得动时画：锁定布局拒绝拖动（冒烟 L1），不接鼠标时
        // 工作台不收鼠标事件（N20），都不画、也不登记点不到的命中区。
        let drag_handles = !self.workbench.dock.locked && self.config.mouse_capture;
        // C-12 (b)：把手/预览/占位的合并往返只在确有内容可画时才做；守卫谓词与
        // 下方各绘制分支完全一致（端点错误画在状态栏里，不再在这里补画）。
        let has_handles = drag_handles
            && self.hits.panes.iter().any(|hit| {
                hit.rect.width > 4 && (hit.inner_rect.y > hit.rect.y || self.workbench.arranging)
            });
        let drop_preview = self
            .workbench
            .drag
            .as_ref()
            .and_then(|drag| drag.drop_target(&self.workbench.geometry));
        if !stale_panel_areas.is_empty() || has_handles || drop_preview.is_some() {
            {
                let composed = canvas.buffer();
                for area in &stale_panel_areas {
                    put(
                        composed,
                        *area,
                        tr("Waiting for terminal…", "正在同步终端…"),
                        Style::default().fg(palette.overlay0),
                    );
                }
                for hit in &self.hits.panes {
                    if hit.rect.width > 4
                        && drag_handles
                        && (hit.inner_rect.y > hit.rect.y || self.workbench.arranging)
                    {
                        // 有上边框：把手压在边框行上。无边框（只在调整布局模式画）：
                        // 首行是终端内容，把手挪到正上方那一行的空位（冒烟 L2）；
                        // 上方没有 chrome 行可用（无空隙堆叠在另一个窗格下面、或
                        // 标签占满窗格列宽）才退回原位，保住拖动能力。
                        let on_border = Rect::new(hit.rect.x + 1, hit.rect.y, 2, 1);
                        let handle = if hit.inner_rect.y > hit.rect.y {
                            on_border
                        } else {
                            borderless_pane_handle(
                                composed,
                                hit.rect,
                                &self.workbench.hits,
                                &self.hits.panes,
                            )
                            .unwrap_or(on_border)
                        };
                        put(composed, handle, "⠿", Style::default().fg(palette.accent));
                        self.workbench
                            .hits
                            .push((handle, Action::Pane(hit.pane_id.clone())));
                        occlusion.cover(handle);
                    }
                }
                if let Some((target, edge)) = drop_preview {
                    if let Some((_, area)) = self
                        .workbench
                        .geometry
                        .panels
                        .iter()
                        .find(|(panel, _)| panel == &target)
                    {
                        let area = super::interaction::preview(*area, edge);
                        // 拖放预览只画轮廓、不填底：盖住底下的终端内容就没法判断
                        // 放到哪一侧。边框字形与颜色跟面板边框同源（THEME-01/02）。
                        Block::default()
                            .borders(Borders::ALL)
                            .border_set(self.config.border_glyphs.border_set())
                            .border_style(
                                Style::default().fg(self.config.components.pane_border_focused),
                            )
                            .title(tr(" Drop here ", " 放到这里 "))
                            .title_style(Style::default().fg(palette.accent))
                            .render(area, composed);
                        occlusion.cover(area);
                    }
                }
            }
        }
        if visual_bell {
            if let Some(hit) = snapshot
                .focused_pane_id
                .as_ref()
                .and_then(|id| self.hits.panes.iter().find(|hit| &hit.pane_id == id))
            {
                // 聚焦边框强调只改样式不改符号，链接登记不受影响。
                super::super::composition::emphasize_pane_border(
                    canvas.buffer(),
                    hit,
                    palette.yellow,
                );
            }
        }
        self.paint_frozen_selection(&mut canvas, &mut occlusion);
        self.paint_shell_copy(&mut canvas, &mut occlusion)?;
        self.hits.popup = None;
        if let Some((area, popup)) = self
            .workbench
            .geometry
            .panels
            .iter()
            .find(|(panel, _)| {
                panel
                    == self
                        .workbench
                        .dock
                        .projected_focus(&self.workbench.geometry)
            })
            .and_then(|(panel, area)| {
                self.workbench
                    .focused_view()?
                    .surface
                    .popup
                    .as_deref()
                    .map(|popup| (body(*area, panel, self.workbench.geometry.compact), popup))
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
                ratatui::widgets::Clear.render(geometry.outer, canvas.buffer());
                // 弹窗边框走共享的带标题面板（字形跟随 ui.border_style，颜色取
                // 组件 token），随后 blit 的弹窗帧盖住面板底（THEME-01/02）。
                super::super::render::titled_panel(
                    canvas.buffer(),
                    geometry.outer,
                    &popup.title,
                    self.config.components.pane_border_focused,
                    palette.panel_bg,
                    self.config.border_glyphs,
                );
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
        if self.observability.process_dialog.is_some() {
            // 全局浮层在终端内容、把手与选择高亮之后绘制，保持视觉与输入层级一致。
            let cx = super::super::feedback::ChromeContext {
                page_bounds: None,
                palette,
                components: &self.config.components,
                glyphs: self.config.border_glyphs,
                hover: None,
                spinner,
                now: self
                    .last_composed_at
                    .unwrap_or_else(std::time::Instant::now),
            };
            if let Some(painted) = self.observability.paint(&mut canvas, full, &cx, None, true) {
                occlusion.cover(painted.covered);
                self.observability.commit_paint(painted);
            }
        }
        self.paint_shell_feedback(
            &mut canvas,
            ClientShellLayout {
                sidebar: Rect::default(),
                tab_bar: Rect::default(),
                mobile_header: Rect::default(),
                pane_surface: full,
            },
            &mut occlusion,
        )?;
        self.paint_shell_overlays(&mut canvas, &mut occlusion)?;
        // 图形与帧分开交出（上游 #4561）：由 `frame_output` 按输出通道写出。
        let mut graphics = crate::kitty_graphics::GraphicsOutput::from_bytes(std::mem::take(
            &mut self.workbench.cleanup,
        ));
        // 组合期间快照不会变；上面的 `&mut self` 绘制段之后重新借用。
        let snapshot = self.snapshot.as_deref();
        let stamps = &self.workbench.stamps;
        let snapshot_seen = self.workbench.snapshot_seen;
        for (id, view) in &mut self.workbench.views {
            let area = self.workbench.geometry.panels.iter().find(|(panel, _)| matches!(panel, PanelId::Terminal(group) if group.to_string() == *id)).map(|(panel, area)| body(*area, panel, self.workbench.geometry.compact));
            // 图形与画面同一口径：沿用旧帧期间图片照常显示，不跟着闪。
            let visible = area.is_some()
                && snapshot.is_some_and(|snapshot| {
                    super::presentation(view, stamps.get(id).copied(), snapshot_seen, snapshot, now)
                        != super::Presentation::Unavailable
                })
                && self.endpoint_error.is_none();
            let area = area.unwrap_or_default();
            let popup = self
                .hits
                .popup
                .as_ref()
                .filter(|_| {
                    self.workbench
                        .dock
                        .projected_focus(&self.workbench.geometry)
                        == &PanelId::Terminal(id.parse().unwrap_or(0))
                })
                .map(|hit| (hit.inner_rect.x, hit.inner_rect.y));
            let visibility = if !visible {
                crate::kitty_graphics::surface::Visibility::Hidden
            } else if popup.is_some() {
                crate::kitty_graphics::surface::Visibility::Popup
            } else {
                crate::kitty_graphics::surface::Visibility::Main
            };
            graphics.extend(view.graphics.encode_output(
                visibility,
                (area.x, area.y),
                popup,
                self.graphics_cell_size,
                &occlusion,
            ));
        }
        let (frame, buffer) = canvas.finish(Vec::new());
        self.compose_buffer = Some(buffer);
        self.hits.rebuild_chrome_bounds();
        Some(crate::client::frame_output::ComposedFrame { frame, graphics })
    }
}
