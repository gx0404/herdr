use super::*;

fn rect_fits_frame(rect: protocol::SurfaceRect, frame: &FrameData) -> bool {
    rect.x.saturating_add(rect.width) <= frame.width
        && rect.y.saturating_add(rect.height) <= frame.height
}

/// RS-01 根治：把收集期局部链接表索引翻译为接收者基线帧的绝对索引——
/// 已有 URI 复用基线索引，新 URI 追加到本补丁的增量表
/// （索引 = 基线表长 + 增量表内偏移）。
fn frame_hyperlink_index(
    frame: &FrameData,
    new_hyperlink_uris: &mut Vec<String>,
    uri: &str,
) -> u32 {
    let as_index = |len: usize| u32::try_from(len).unwrap_or(u32::MAX);
    if let Some(index) = frame.hyperlinks.iter().position(|known| known == uri) {
        return as_index(index);
    }
    let base = as_index(frame.hyperlinks.len());
    if let Some(offset) = new_hyperlink_uris.iter().position(|known| known == uri) {
        return base.saturating_add(as_index(offset));
    }
    let index = base.saturating_add(as_index(new_hyperlink_uris.len()));
    new_hyperlink_uris.push(uri.to_owned());
    index
}

/// 把收集补丁行的超链接索引翻译到接收者基线帧；无链接或整行无需改写时零拷贝
/// 借用，只有真正改索引的格子才复制（符号是 String，逐格克隆要避开热路径）。
fn translate_row_hyperlinks<'a>(
    cells: &'a [protocol::CellData],
    patch: &crate::pane::TerminalDirtyPatch,
    frame: &FrameData,
    new_hyperlink_uris: &mut Vec<String>,
) -> std::borrow::Cow<'a, [protocol::CellData]> {
    if patch.hyperlinks.is_empty() {
        return std::borrow::Cow::Borrowed(cells);
    }
    let mut translated: Option<Vec<protocol::CellData>> = None;
    for (index, cell) in cells.iter().enumerate() {
        let Some(local) = cell.hyperlink else {
            continue;
        };
        // 收集期索引与链接表同步构造，越组不会发生；防御性丢链优于写出错误
        // 索引（ctrl-hover 会开错 URL）。
        let absolute = patch
            .hyperlinks
            .get(local as usize)
            .map(|uri| frame_hyperlink_index(frame, new_hyperlink_uris, uri));
        if absolute == cell.hyperlink {
            continue;
        }
        let owned = translated.get_or_insert_with(|| cells.to_vec());
        if let Some(cell) = owned.get_mut(index) {
            cell.hyperlink = absolute;
        }
    }
    match translated {
        Some(cells) => std::borrow::Cow::Owned(cells),
        None => std::borrow::Cow::Borrowed(cells),
    }
}

fn patch_row_changed(frame: &FrameData, row: &protocol::PaneSurfacePatchRow) -> Option<bool> {
    if row.y >= frame.height
        || row.x.saturating_add(u16::try_from(row.cells.len()).ok()?) > frame.width
    {
        return None;
    }
    let start = usize::from(row.y) * usize::from(frame.width) + usize::from(row.x);
    let end = start + row.cells.len();
    if end > frame.cells.len() {
        return None;
    }
    Some(frame.cells[start..end] != row.cells)
}

fn changed_rows(
    frame: &FrameData,
    area: protocol::SurfaceRect,
    patch: &crate::pane::TerminalDirtyPatch,
    new_hyperlink_uris: &mut Vec<String>,
) -> Option<Vec<protocol::PaneSurfacePatchRow>> {
    if !rect_fits_frame(area, frame) {
        return None;
    }
    let mut rows = Vec::new();
    for (local_y, cells) in &patch.rows {
        if *local_y >= area.height {
            continue;
        }
        let width = usize::from(area.width);
        if cells.len() < width {
            return None;
        }
        let y = area.y + *local_y;
        let frame_start = usize::from(y) * usize::from(frame.width) + usize::from(area.x);
        let frame_end = frame_start.checked_add(width)?;
        let existing = frame.cells.get(frame_start..frame_end)?;
        // 先按接收者基线翻译超链接索引，再做逐格 diff——两表不同源，
        // 直接比 u32 索引是错的。
        let desired = translate_row_hyperlinks(&cells[..width], patch, frame, new_hyperlink_uris);
        let desired: &[protocol::CellData] = &desired;
        let mut offset = 0;
        while offset < width {
            if existing[offset] == desired[offset] {
                offset += 1;
                continue;
            }
            let start = offset;
            offset += 1;
            while offset < width && existing[offset] != desired[offset] {
                offset += 1;
            }
            // Include the following cell so a wide-to-narrow (or
            // narrow-to-wide) transition repaints content covered by the old
            // grapheme width even when that logical neighbor is unchanged.
            let end = offset.saturating_add(1).min(width);
            rows.push(protocol::PaneSurfacePatchRow {
                x: area.x.checked_add(u16::try_from(start).ok()?)?,
                y,
                cells: desired[start..end].to_vec(),
            });
            offset = end;
        }
    }
    Some(rows)
}

fn retained_scrollbar_patch(
    app: &app::App,
    frame: &FrameData,
    pane: &mut protocol::PaneSurfacePane,
    alternate_screen_active: bool,
    metrics: Option<crate::pane::ScrollMetrics>,
) -> Option<Vec<protocol::PaneSurfacePatchRow>> {
    let next_rect = metrics
        .filter(|metrics| metrics.max_offset_from_bottom > 0)
        .filter(|_| app.state.pane_scrollbars && !alternate_screen_active)
        .and_then(|_| {
            let rect = protocol::SurfaceRect {
                x: pane.inner_rect.x.checked_add(pane.inner_rect.width)?,
                y: pane.inner_rect.y,
                width: 1,
                height: pane.inner_rect.height,
            };
            (rect_fits_frame(rect, frame)
                && rect.x >= pane.rect.x
                && rect.x < pane.rect.x.saturating_add(pane.rect.width))
            .then_some(rect)
        });
    let patch_rect = next_rect.or(pane.scrollbar_rect);
    pane.scrollbar_rect = next_rect;
    let Some(rect) = patch_rect else {
        return Some(Vec::new());
    };

    let track = Rect::new(0, 0, 1, rect.height);
    let mut buffer = ratatui::buffer::Buffer::empty(track);
    if let (Some(metrics), Some(_)) = (metrics, next_rect) {
        crate::ui::render_pane_scrollbar_buffer_styled(
            &mut buffer,
            metrics,
            track,
            &app.state.components,
            pane.focused,
        );
    }
    let cells = buffer
        .content
        .iter()
        .map(protocol::CellData::from_ratatui_cell)
        .collect::<Vec<_>>();
    let mut rows = Vec::new();
    for (offset, cell) in cells.into_iter().enumerate() {
        let row = protocol::PaneSurfacePatchRow {
            x: rect.x,
            y: rect.y.checked_add(u16::try_from(offset).ok()?)?,
            cells: vec![cell],
        };
        if patch_row_changed(frame, &row)? {
            rows.push(row);
        }
    }
    Some(rows)
}

fn retained_cursor(
    app: &app::App,
    panes: &[protocol::PaneSurfacePane],
) -> Option<protocol::CursorState> {
    let pane = panes.iter().find(|pane| pane.focused)?;
    let (workspace_index, pane_id) = app.parse_pane_id(&pane.pane_id)?;
    let runtime = app.state.runtime_for_pane_in_workspace(
        &app.terminal_runtimes,
        workspace_index,
        pane_id,
    )?;
    // 与完整渲染器 `ui::tab_surface_cursor` 共用同一个纯函数（含 IME 揭示与
    // DECSET 2026 批次内沿用批次前光标的语义），retained 快路径对同一 pane
    // 输出同一宿主光标，不需要单独的整帧回退。
    crate::ui::pane_host_cursor(
        runtime,
        crate::ui::PaneHostCursorInputs {
            area: Rect::new(
                pane.inner_rect.x,
                pane.inner_rect.y,
                pane.inner_rect.width,
                pane.inner_rect.height,
            ),
            reveal: crate::ui::cjk_ime_reveal(&app.state, workspace_index, pane_id),
            scrolled_back: crate::ui::pane_is_scrolled_back(runtime),
            reveal_shape: app.state.cjk_ime_cursor_shape,
        },
    )
}

#[derive(Clone)]
struct ViewIdentity {
    index: usize,
    revision: u64,
    id: String,
    tab: String,
}

struct RetainedRecipient<'a> {
    client_id: u64,
    view: Option<ViewIdentity>,
    surface: &'a protocol::PaneSurfaceFrame,
}

struct CollectedPanePatch {
    pane_id: String,
    patch: crate::pane::TerminalDirtyPatch,
    content_revision: u64,
    scroll_metrics: Option<crate::pane::ScrollMetrics>,
    mouse_reporting: bool,
    sgr_pixel_mouse: bool,
    alternate_screen_active: bool,
    graphics_may_have_placements: bool,
}

struct RetainedRecipientUpdate {
    client_id: u64,
    view: Option<ViewIdentity>,
    patch: protocol::PaneSurfacePatch,
    graphics: Option<(
        protocol::PaneSurfaceFrame,
        crate::kitty_graphics::surface::DeliveryCache,
    )>,
}

impl HeadlessServer {
    /// Applies terminal dirty rows to the committed origin-relative pane surface.
    /// Any presentation or geometry uncertainty falls back to the complete renderer.
    pub(super) fn render_retained_pane_surface_and_stream(
        &mut self,
        pty_sources: &HashSet<crate::layout::PaneId>,
    ) -> bool {
        crate::render_prof::event("retained_surface.attempt");
        let started = crate::render_prof::timer();
        // RS-01/RS-05 过渡层：回退拆成两类。全局不安全（unsafe_state /
        // non_shell_target）保持整 tick 回退；接收者级与 pane 级问题只剔除
        // 受影响者，观看它的客户端延期到一次全量渲染恢复基线（defer_full_render
        // + 显式 render kick），其余 pane/接收者照常走补丁。
        macro_rules! fallback {
            ($reason:literal) => {{
                crate::render_prof::event(concat!("retained_surface.fallback.", $reason));
                crate::render_prof::duration_since("retained_surface.total", started);
                return false;
            }};
        }
        macro_rules! success {
            ($reason:literal) => {{
                crate::render_prof::event("retained_surface.success");
                crate::render_prof::event(concat!("retained_surface.success.", $reason));
                crate::render_prof::duration_since("retained_surface.total", started);
                return true;
            }};
        }

        if pty_sources.is_empty()
            || self.app.full_redraw_pending
            || self.app.state.popup_pane.is_some()
        {
            fallback!("unsafe_state");
        }
        let mut targets = render_targets(&self.clients, self.foreground_client_id);
        targets.retain(|(client_id, _, _, _, mode)| {
            !matches!(mode, ClientConnectionMode::ClientShell)
                || self
                    .clients
                    .get(client_id)
                    .is_some_and(|client| client.shell_surface_active)
        });
        if targets.is_empty() {
            success!("no_active_surface");
        }
        if targets
            .iter()
            .any(|target| !matches!(target.4, ClientConnectionMode::ClientShell))
        {
            fallback!("non_shell_target");
        }

        let mut deferred_clients: HashSet<u64> = HashSet::new();
        let mut recipients = Vec::with_capacity(targets.len());
        for (client_id, (cols, rows), _, _, _) in &targets {
            let Some(client) = self.clients.get(client_id) else {
                // client_missing：收集期间客户端已消失，剔除即可（延期无对象）。
                crate::render_prof::event("retained_surface.skip.client_missing");
                continue;
            };
            if client.deferred_render() != DeferredRender::None {
                crate::render_prof::event("retained_surface.recipient_deferred");
                continue;
            }
            let mut surfaces = Vec::new();
            if let Some(views) = &client.views {
                for (index, view) in views.views.iter().enumerate() {
                    let Some(surface) = view.render_state.last_pane_surface() else {
                        // no_view_baseline：剔除该 view 并延期客户端全量重绘。
                        crate::render_prof::event("retained_surface.defer.no_view_baseline");
                        deferred_clients.insert(*client_id);
                        continue;
                    };
                    surfaces.push((
                        Some(ViewIdentity {
                            index,
                            revision: views.revision,
                            id: view.spec.view_id.clone(),
                            tab: view.spec.tab_id.clone(),
                        }),
                        surface,
                        view.spec.cols,
                        view.spec.rows,
                    ));
                }
            } else if let Some(surface) = client.render_state.last_pane_surface() {
                surfaces.push((None, surface, *cols, *rows));
            } else {
                // no_baseline：客户端还没有基线（首个全量渲染前），延期恢复。
                crate::render_prof::event("retained_surface.defer.no_baseline");
                deferred_clients.insert(*client_id);
            }
            for (view, surface, cols, rows) in surfaces {
                if surface.boot_id != self.client_shell_boot_id
                    || surface.projection_revision != client.shell_projection_revision
                    || surface.frame.width != cols
                    || surface.frame.height != rows
                    || surface.popup.is_some()
                    || !surface.graphics.assets.is_empty()
                    || !surface.frame.graphics.is_empty()
                {
                    // baseline_mismatch：基线陈旧/不适用，剔除并延期全量重绘。
                    crate::render_prof::event("retained_surface.defer.baseline_mismatch");
                    deferred_clients.insert(*client_id);
                    continue;
                }
                recipients.push(RetainedRecipient {
                    client_id: *client_id,
                    view,
                    surface,
                });
            }
        }
        if recipients.is_empty() {
            success!("all_recipients_deferred");
        }

        let mut collected = Vec::with_capacity(pty_sources.len());
        for source in pty_sources {
            let mut public_pane_id = None;
            let mut width = 0u16;
            let mut height = 0u16;
            let mut watching_clients = HashSet::new();
            for recipient in &recipients {
                let Some(pane) = recipient.surface.panes.iter().find(|pane| {
                    self.app
                        .parse_pane_id(&pane.pane_id)
                        .is_some_and(|(_, pane_id)| pane_id == *source)
                }) else {
                    continue;
                };
                watching_clients.insert(recipient.client_id);
                public_pane_id.get_or_insert_with(|| pane.pane_id.clone());
                width = width.max(pane.inner_rect.width);
                height = height.max(pane.inner_rect.height);
            }
            let Some(public_pane_id) = public_pane_id else {
                continue;
            };
            let Some((workspace_index, pane_id)) = self.app.parse_pane_id(&public_pane_id) else {
                // pane_missing：基线里的 pane 已从状态消失，剔除并让观看者全量重绘。
                crate::render_prof::event("retained_surface.defer.pane_missing");
                deferred_clients.extend(watching_clients);
                continue;
            };
            let Some(runtime) = self.app.state.runtime_for_pane_in_workspace(
                &self.app.terminal_runtimes,
                workspace_index,
                pane_id,
            ) else {
                // runtime_missing：pane 生命周期切换中，剔除并让观看者全量重绘。
                crate::render_prof::event("retained_surface.defer.runtime_missing");
                deferred_clients.extend(watching_clients);
                continue;
            };
            let snapshot = match runtime.collect_dirty_patch_snapshot(width, height) {
                Ok(snapshot) => snapshot,
                Err(crate::pane::DirtyPatchSnapshotUnavailable::Contended) => {
                    // terminal_snapshot 竞态：revision 未配对完成，武装下一 tick
                    // 重试补丁，不延期、不整 tick 回退。
                    crate::render_prof::event("retained_surface.retry.terminal_snapshot");
                    self.app.render_dirty.request_pty(pane_id);
                    self.app.render_notify.notify_one();
                    continue;
                }
                Err(crate::pane::DirtyPatchSnapshotUnavailable::PatchFallback) => {
                    // terminal_patch（如超链接单元格）：剔除该 pane，观看者延期
                    // 全量重绘重建基线。
                    crate::render_prof::event("retained_surface.defer.terminal_patch");
                    deferred_clients.extend(watching_clients);
                    continue;
                }
            };
            let patch = match snapshot.patch {
                crate::pane::TerminalDirtyPatchOutcome::Clean => {
                    crate::render_prof::event("retained_surface.pane_clean");
                    crate::pane::TerminalDirtyPatch::default()
                }
                crate::pane::TerminalDirtyPatchOutcome::Patch(patch) => patch,
                crate::pane::TerminalDirtyPatchOutcome::Fallback => {
                    // collect_dirty_patch_snapshot 已把 Fallback 映射为
                    // Err(PatchFallback)；防御分支同等处理。
                    crate::render_prof::event("retained_surface.defer.terminal_patch");
                    deferred_clients.extend(watching_clients);
                    continue;
                }
            };
            collected.push(CollectedPanePatch {
                pane_id: public_pane_id,
                patch,
                content_revision: snapshot.content_revision,
                scroll_metrics: snapshot.scroll_metrics,
                mouse_reporting: snapshot.mouse_reporting,
                sgr_pixel_mouse: snapshot.sgr_pixel_mouse,
                alternate_screen_active: snapshot.alternate_screen_active,
                graphics_may_have_placements: snapshot.graphics_may_have_placements,
            });
        }

        let mut updates = Vec::with_capacity(recipients.len());
        'recipient: for recipient in recipients {
            let client_id = recipient.client_id;
            let surface = recipient.surface;
            let mut panes = surface.panes.clone();
            let projection_revision = surface.projection_revision;
            let base_surface_revision = surface.surface_revision;
            let mut changed_panes = Vec::with_capacity(collected.len());
            let mut patch_rows = Vec::new();
            let mut metadata_changed = false;
            let mut new_hyperlink_uris: Vec<String> = Vec::new();
            let mut refresh_graphics = !surface.graphics.placements.is_empty()
                || !surface.graphics.retained_assets.is_empty();
            for collected_pane in &collected {
                let Some(pane) = panes
                    .iter_mut()
                    .find(|pane| pane.pane_id == collected_pane.pane_id)
                else {
                    continue;
                };
                // Alternate-screen transitions change whether the pane reserves
                // a scrollbar gutter. 该 pane 剔除出本 tick 补丁，观看客户端延期到
                // 全量渲染重算布局并 resize runtime。
                if pane.alternate_screen_active != collected_pane.alternate_screen_active {
                    crate::render_prof::event("retained_surface.defer.alternate_screen_geometry");
                    deferred_clients.insert(client_id);
                    continue;
                }
                refresh_graphics |= collected_pane.graphics_may_have_placements;
                let previous_pane = pane.clone();
                // 该 pane 可能中途延期：把行与链接增量一起回滚到本 pane 之前，
                // 「剔除该 pane」才名副其实——否则会留下无人引用的链接条目，
                // 或让被剔除的行引用已回滚的索引。
                let rows_checkpoint = patch_rows.len();
                let hyperlinks_checkpoint = new_hyperlink_uris.len();
                let Some(rows) = changed_rows(
                    &surface.frame,
                    pane.inner_rect,
                    &collected_pane.patch,
                    &mut new_hyperlink_uris,
                ) else {
                    crate::render_prof::event("retained_surface.defer.invalid_patch");
                    new_hyperlink_uris.truncate(hyperlinks_checkpoint);
                    deferred_clients.insert(client_id);
                    continue;
                };
                patch_rows.extend(rows);
                let Some(scrollbar_rows) = retained_scrollbar_patch(
                    &self.app,
                    &surface.frame,
                    pane,
                    collected_pane.alternate_screen_active,
                    collected_pane.scroll_metrics,
                ) else {
                    crate::render_prof::event("retained_surface.defer.scrollbar_patch");
                    patch_rows.truncate(rows_checkpoint);
                    new_hyperlink_uris.truncate(hyperlinks_checkpoint);
                    deferred_clients.insert(client_id);
                    continue;
                };
                patch_rows.extend(scrollbar_rows);
                pane.content_revision = collected_pane.content_revision;
                pane.mouse_reporting = collected_pane.mouse_reporting;
                pane.sgr_pixel_mouse = collected_pane.sgr_pixel_mouse;
                pane.alternate_screen_active = collected_pane.alternate_screen_active;
                pane.scroll = collected_pane.scroll_metrics.map(|metrics| {
                    protocol::PaneSurfaceScrollMetrics {
                        offset_from_bottom: metrics.offset_from_bottom as u64,
                        max_offset_from_bottom: metrics.max_offset_from_bottom as u64,
                        viewport_rows: metrics.viewport_rows as u64,
                    }
                });
                metadata_changed |= *pane != previous_pane;
                changed_panes.push(pane.clone());
            }

            let cursor = retained_cursor(&self.app, &panes);
            let cursor_changed = cursor != surface.frame.cursor;
            let patch = protocol::PaneSurfacePatch {
                boot_id: self.client_shell_boot_id.clone(),
                projection_revision,
                base_surface_revision,
                surface_revision: 0,
                rows: patch_rows,
                panes: changed_panes,
                cursor,
                hyperlink_uris: new_hyperlink_uris,
            };
            let mut graphics_changed = false;
            let graphics = if refresh_graphics {
                let target = recipient
                    .view
                    .as_ref()
                    .and_then(|view| self.app.parse_tab_id(&view.tab))
                    .map(|(workspace_index, tab_index)| crate::ui::TabSurfaceTarget {
                        workspace_index,
                        tab_index,
                    })
                    .or_else(|| self.shell_target_for_client(client_id));
                let Some(target) = target else {
                    crate::render_prof::event("retained_surface.defer.graphics_target");
                    deferred_clients.insert(client_id);
                    continue 'recipient;
                };
                let client = &self.clients[&client_id];
                let mut next_surface = surface.clone();
                crate::server::render_stream::apply_pane_surface_patch(&mut next_surface, &patch);
                let Some((graphics, delivery)) =
                    crate::server::client_shell_graphics::collect_retained(
                        &self.app,
                        &next_surface,
                        target,
                        client.cell_size,
                        recipient
                            .view
                            .as_ref()
                            .and_then(|identity| client.views.as_ref()?.views.get(identity.index))
                            .map(|view| &view.graphics_delivery)
                            .unwrap_or(&client.shell_graphics_delivery),
                        client_id,
                    )
                else {
                    crate::render_prof::event("retained_surface.defer.graphics_geometry");
                    deferred_clients.insert(client_id);
                    continue 'recipient;
                };
                graphics_changed = graphics != surface.graphics;
                next_surface.graphics = graphics;
                Some((next_surface, delivery))
            } else {
                None
            };
            if patch.rows.is_empty() && !cursor_changed && !metadata_changed && !graphics_changed {
                continue;
            }
            updates.push(RetainedRecipientUpdate {
                client_id,
                view: recipient.view,
                patch,
                graphics,
            });
        }
        if !deferred_clients.is_empty() {
            let mut armed = false;
            for client_id in &deferred_clients {
                if let Some(client) = self.clients.get_mut(client_id) {
                    client.defer_full_render();
                    armed = true;
                }
            }
            if armed {
                // 延期必须可靠唤醒：客户端写入侧没有待发帧时不会有
                // ClientWriterDrained 事件，显式安排一次全量渲染恢复基线。
                self.app.render_dirty.request_generic();
                self.app.render_notify.notify_one();
            }
        }
        if updates.is_empty() {
            success!("unchanged");
        }

        let mut sent = 0u64;
        let mut deferred = 0u64;
        let mut disconnected = Vec::new();
        let mut grouped = HashMap::<u64, Vec<RetainedRecipientUpdate>>::new();
        for update in updates {
            grouped.entry(update.client_id).or_default().push(update);
        }
        for (client_id, updates) in grouped {
            let Some(client) = self.clients.get_mut(&client_id) else {
                continue;
            };
            let Some(writer) = client.writer.as_ref().cloned() else {
                client.defer_full_render();
                deferred += 1;
                continue;
            };
            let mut batch = Vec::new();
            let mut commits = Vec::new();
            let mut needs_retry = false;
            for update in updates {
                let state = if let Some(identity) = &update.view {
                    client
                        .views
                        .as_mut()
                        .filter(|views| views.revision == identity.revision)
                        .and_then(|views| views.views.get_mut(identity.index))
                        .map(|view| &mut view.render_state)
                } else {
                    Some(&mut client.render_state)
                };
                let Some(state) = state else {
                    needs_retry = true;
                    continue;
                };
                let (prepared, delivery) = if let Some((surface, delivery)) = update.graphics {
                    (state.prepare_pane_surface(surface), Some(delivery))
                } else {
                    (state.prepare_pane_surface_patch(update.patch), None)
                };
                let Some(prepared) = prepared else {
                    needs_retry = true;
                    continue;
                };
                let serialized = if let Some(identity) = &update.view {
                    protocol::views::message(
                        &self.client_shell_boot_id,
                        identity.revision,
                        &identity.id,
                        &identity.tab,
                        prepared.message(),
                    )
                    .map_err(io::Error::other)
                    .and_then(|message| {
                        Self::frame_server_message_with_max(&message, MAX_GRAPHICS_FRAME_SIZE)
                            .map_err(io::Error::other)
                    })
                } else {
                    Self::frame_server_message_with_max(
                        prepared.message(),
                        if delivery.is_some() {
                            MAX_GRAPHICS_FRAME_SIZE
                        } else {
                            protocol::MAX_FRAME_SIZE
                        },
                    )
                    .map_err(io::Error::other)
                };
                let Ok(serialized) = serialized else {
                    needs_retry = true;
                    continue;
                };
                if batch.len().saturating_add(serialized.len()) > MAX_GRAPHICS_FRAME_SIZE {
                    needs_retry = true;
                    continue;
                }
                batch.extend_from_slice(&serialized);
                commits.push((update.view, prepared, delivery));
            }
            crate::render_prof::counter("retained_surface.bytes", batch.len() as u64);
            if batch.is_empty() {
                if needs_retry {
                    client.defer_full_render();
                    deferred += 1;
                }
                continue;
            }
            match writer.render.try_send(batch) {
                Ok(()) => {
                    for (identity, prepared, delivery) in commits {
                        needs_retry |= delivery.as_ref().is_some_and(
                            crate::kitty_graphics::surface::DeliveryCache::has_pending,
                        );
                        if let Some(identity) = identity {
                            if let Some(view) = client
                                .views
                                .as_mut()
                                .and_then(|views| views.views.get_mut(identity.index))
                            {
                                view.render_state.commit_sent_frame(prepared);
                                if let Some(delivery) = delivery {
                                    view.graphics_delivery = delivery;
                                }
                            }
                        } else {
                            client.render_state.commit_sent_frame(prepared);
                            if let Some(delivery) = delivery {
                                client.shell_graphics_delivery = delivery;
                            }
                        }
                        sent += 1;
                    }
                    if needs_retry {
                        client.defer_full_render();
                    } else if !deferred_clients.contains(&client_id) {
                        // 本 tick 被剔除 pane 而延期的客户端不在此清除：
                        // 补丁成功不代表基线已恢复，全量渲染后才清。
                        client.clear_deferred_render();
                    }
                }
                Err(std::sync::mpsc::TrySendError::Full(_)) => {
                    client.defer_full_render();
                    deferred += 1;
                }
                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => disconnected.push(client_id),
            }
        }
        for client_id in disconnected {
            self.remove_client_and_resize_if_needed(client_id);
        }
        crate::render_prof::counter("retained_surface.recipients.sent", sent);
        crate::render_prof::counter("retained_surface.recipients.deferred", deferred);
        if sent > 0 {
            success!("sent");
        }
        success!("recovery_queued");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(symbol: &str) -> protocol::CellData {
        protocol::CellData {
            symbol: symbol.into(),
            fg: 0,
            bg: 0,
            modifier: 0,
            skip: false,
            hyperlink: None,
        }
    }

    #[test]
    fn retained_rows_send_only_changed_cell_spans() {
        let frame = FrameData {
            width: 6,
            height: 2,
            cells: vec![cell(" "); 12],
            cursor: None,
            hyperlinks: Vec::new(),
            graphics: Vec::new(),
        };
        let patch = crate::pane::TerminalDirtyPatch {
            rows: vec![(0, vec![cell(" "), cell("x"), cell("y"), cell(" ")])],
            hyperlinks: Vec::new(),
        };

        let mut new_hyperlink_uris = Vec::new();
        let rows = changed_rows(
            &frame,
            protocol::SurfaceRect {
                x: 1,
                y: 1,
                width: 4,
                height: 1,
            },
            &patch,
            &mut new_hyperlink_uris,
        )
        .expect("valid patch");

        assert_eq!(
            rows,
            vec![protocol::PaneSurfacePatchRow {
                x: 2,
                y: 1,
                cells: vec![cell("x"), cell("y"), cell(" ")],
            }]
        );
        assert_eq!(frame.cells, vec![cell(" "); 12], "planning must not commit");
    }

    #[test]
    fn retained_rows_include_the_cell_after_a_width_transition() {
        let frame = FrameData {
            width: 3,
            height: 1,
            cells: vec![cell("界"), cell("z"), cell("q")],
            cursor: None,
            hyperlinks: Vec::new(),
            graphics: Vec::new(),
        };
        let patch = crate::pane::TerminalDirtyPatch {
            rows: vec![(0, vec![cell("x"), cell("z"), cell("q")])],
            hyperlinks: Vec::new(),
        };

        let mut new_hyperlink_uris = Vec::new();
        let rows = changed_rows(
            &frame,
            protocol::SurfaceRect {
                x: 0,
                y: 0,
                width: 3,
                height: 1,
            },
            &patch,
            &mut new_hyperlink_uris,
        )
        .expect("valid patch");

        assert_eq!(
            rows,
            vec![protocol::PaneSurfacePatchRow {
                x: 0,
                y: 0,
                cells: vec![cell("x"), cell("z")],
            }]
        );
    }

    #[test]
    fn retained_rows_omit_unchanged_full_dirty_rows() {
        let frame = FrameData {
            width: 4,
            height: 2,
            cells: vec![cell(" "); 8],
            cursor: None,
            hyperlinks: Vec::new(),
            graphics: Vec::new(),
        };
        let patch = crate::pane::TerminalDirtyPatch {
            rows: vec![(0, vec![cell(" "); 4]), (1, vec![cell(" "); 4])],
            hyperlinks: Vec::new(),
        };

        let mut new_hyperlink_uris = Vec::new();
        let rows = changed_rows(
            &frame,
            protocol::SurfaceRect {
                x: 0,
                y: 0,
                width: 4,
                height: 2,
            },
            &patch,
            &mut new_hyperlink_uris,
        )
        .expect("valid patch");

        assert!(rows.is_empty());
    }
}
