use super::*;

/// 接收者基线链接表的条目上界（RS-01 根治的配套界）。表只追加：只有全量帧会
/// 重建它，所以长会话里被引用过的 URI 会一直占位，同时把每格的线性扫描拉长。
/// 达到上界就剔除该接收者并延期一次全量渲染，让表回到「只含当前可见链接」。
/// 单个全量帧的可见链接本身就超过上界时（极端 pane）退化为每 tick 全量渲染，
/// 与 B9 之前的行为一致，不会更差。
pub(super) const MAX_RECIPIENT_HYPERLINKS: usize = 4096;

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

/// 把收集补丁行的超链接索引翻译到接收者基线帧。默认零拷贝借用原行，只有真正改
/// 索引（或防御性丢链）的格子才复制一份——符号是 String，逐格克隆要避开热路径。
struct RowHyperlinks<'a> {
    cells: &'a [protocol::CellData],
    /// 按格偏移升序的覆盖项：`(offset, 翻译后的格子)`。
    overrides: Vec<(usize, protocol::CellData)>,
}

impl<'a> RowHyperlinks<'a> {
    fn new(
        cells: &'a [protocol::CellData],
        local_uris: &[String],
        frame: &FrameData,
        new_hyperlink_uris: &mut Vec<String>,
    ) -> Self {
        let mut overrides: Vec<(usize, protocol::CellData)> = Vec::new();
        if local_uris.is_empty() {
            return Self { cells, overrides };
        }
        // 同一 URI 的连续游程（一行里最常见的形态）只查表一次，避免每格线性扫描。
        let mut last: Option<(&str, u32)> = None;
        for (offset, cell) in cells.iter().enumerate() {
            let Some(local) = cell.hyperlink else {
                continue;
            };
            // 收集期索引与局部表同步构造，越界不会发生；防御性丢链优于写出错误
            // 索引（ctrl-hover 会开错 URL）。
            let Some(uri) = local_uris.get(local as usize) else {
                let mut dropped = cell.clone();
                dropped.hyperlink = None;
                overrides.push((offset, dropped));
                continue;
            };
            let absolute = match last {
                Some((known, index)) if known == uri.as_str() => index,
                _ => {
                    let index = frame_hyperlink_index(frame, new_hyperlink_uris, uri);
                    last = Some((uri.as_str(), index));
                    index
                }
            };
            if Some(absolute) == cell.hyperlink {
                continue;
            }
            let mut translated = cell.clone();
            translated.hyperlink = Some(absolute);
            overrides.push((offset, translated));
        }
        Self { cells, overrides }
    }

    fn cell(&self, offset: usize) -> Option<&protocol::CellData> {
        match self
            .overrides
            .binary_search_by_key(&offset, |(offset, _)| *offset)
        {
            Ok(index) => self.overrides.get(index).map(|(_, cell)| cell),
            Err(_) => self.cells.get(offset),
        }
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
        // 直接比 u32 索引是错的。未改索引的格子保持借用，不逐格克隆。
        let translation = RowHyperlinks::new(
            &cells[..width],
            &patch.hyperlinks,
            frame,
            new_hyperlink_uris,
        );
        let mut offset = 0;
        while offset < width {
            if existing[offset] == *translation.cell(offset)? {
                offset += 1;
                continue;
            }
            let start = offset;
            offset += 1;
            while offset < width && existing[offset] != *translation.cell(offset)? {
                offset += 1;
            }
            // Include the following cell so a wide-to-narrow (or
            // narrow-to-wide) transition repaints content covered by the old
            // grapheme width even when that logical neighbor is unchanged.
            let end = offset.saturating_add(1).min(width);
            let mut emitted = Vec::with_capacity(end - start);
            for index in start..end {
                emitted.push(translation.cell(index)?.clone());
            }
            rows.push(protocol::PaneSurfacePatchRow {
                x: area.x.checked_add(u16::try_from(start).ok()?)?,
                y,
                cells: emitted,
            });
            offset = end;
        }
    }
    Some(rows)
}

/// RS-13：完整渲染只在 `pane_inner.width > 4` 时留出滚动条留白
/// （`ui::terminal_inner_rect`）。窄 pane 没有留白，补丁照旧按
/// `inner_rect.x + inner_rect.width` 画会落到 pane 右边框，与完整渲染不一致
/// （滚动时闪烁）。留白后 `inner_rect.width = pane_inner.width - 1`，因此
/// `inner_rect.width >= 5` 一定留了留白（pane_inner ≤ 4 不可能留）；`== 4` 是
/// 两种情形唯一重叠的一档，只有基线本来就画着滚动条（说明上一帧完整渲染留了
/// 留白）时才补。
fn scrollbar_gutter_available(pane: &protocol::PaneSurfacePane) -> bool {
    pane.inner_rect.width >= 5 || (pane.inner_rect.width == 4 && pane.scrollbar_rect.is_some())
}

fn retained_scrollbar_patch(
    app: &app::App,
    frame: &FrameData,
    pane: &mut protocol::PaneSurfacePane,
    alternate_screen_active: bool,
    metrics: Option<crate::pane::ScrollMetrics>,
) -> Option<Vec<protocol::PaneSurfacePatchRow>> {
    let gutter_available = scrollbar_gutter_available(pane);
    let next_rect = metrics
        .filter(|metrics| metrics.max_offset_from_bottom > 0)
        .filter(|_| app.state.pane_scrollbars && !alternate_screen_active)
        .filter(|_| gutter_available)
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

/// RS-10：本 tick 被补丁覆盖的脏 pane，及其在接收者基线里的公开 id、观看者与
/// 最大内部几何。
struct WatchedSource {
    public_pane_id: String,
    watching_clients: HashSet<u64>,
    width: u16,
    height: u16,
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
        crate::kitty_graphics::surface::SourceFiles,
    )>,
}

fn has_synchronized_pane(app: &app::App, surface: &protocol::PaneSurfaceFrame) -> bool {
    surface.panes.iter().any(|pane| {
        app.parse_pane_id(&pane.pane_id)
            .and_then(|(workspace_index, pane_id)| {
                app.state.runtime_for_pane_in_workspace(
                    &app.terminal_runtimes,
                    workspace_index,
                    pane_id,
                )
            })
            .is_some_and(|runtime| runtime.synchronized_output_active())
    })
}

impl HeadlessServer {
    /// 把本 tick 被剔除的接收者标为延期全量渲染，并显式唤醒一次渲染。
    fn arm_deferred_full_render(&mut self, deferred_clients: &HashSet<u64>) {
        let mut armed = false;
        for client_id in deferred_clients {
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
        // RS-17：目标列表只装 Copy 判别值（不再克隆含 String 的连接形态）。
        let targets: Vec<_> = render_targets(&self.clients, self.foreground_client_id)
            .into_iter()
            .filter(|(client_id, _, _, _, mode)| {
                !matches!(mode, RenderTargetMode::ClientShell)
                    || self
                        .clients
                        .get(client_id)
                        .is_some_and(|client| client.shell_surface_active)
            })
            .collect();
        if targets.is_empty() {
            success!("no_active_surface");
        }
        if targets
            .iter()
            .any(|target| !matches!(target.4, RenderTargetMode::ClientShell))
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
            // 上游 #4508：上一次完整渲染因同步输出 / 渲染期内容变化而推迟时，
            // 基线不能再打补丁——整 tick 回退到完整渲染（它会继续推迟直到帧完整）。
            if client.render_state.requires_recompute()
                || client.views.as_ref().is_some_and(|views| {
                    views
                        .views
                        .iter()
                        .any(|view| view.render_state.requires_recompute())
                })
            {
                fallback!("recompute_pending");
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
                // 上游 #4508：可见 pane 处于同步输出批次中，补丁会发布半帧；整
                // tick 回退，完整渲染路径负责推迟到批次结束。
                if has_synchronized_pane(&self.app, surface) {
                    fallback!("synchronized_visible");
                }
                recipients.push(RetainedRecipient {
                    client_id: *client_id,
                    view,
                    surface,
                });
            }
        }
        if recipients.is_empty() {
            // 全部接收者都被剔除时同样要武装延期：否则陈旧基线（例如纯投影 tick
            // 让快照修订号前进之后）永远没人修复，此后每个输出 tick 都在这里早退，
            // 客户端一直停在旧帧 /「正在同步终端…」。
            self.arm_deferred_full_render(&deferred_clients);
            success!("all_recipients_deferred");
        }

        // RS-10：先按接收者基线扫一遍，一次聚合「脏 pane → 观看者 / 公开 id /
        // 最大几何」。原先每个源 × 每个接收者 × 每个 pane 都要解析一次公开 id
        // 并线性扫描该接收者的 pane 列表（O(源×接收者×pane²) 的字符串解析）。
        let mut watching: HashMap<crate::layout::PaneId, WatchedSource> = HashMap::new();
        for recipient in &recipients {
            for pane in &recipient.surface.panes {
                let Some((_, pane_id)) = self.app.parse_pane_id(&pane.pane_id) else {
                    continue;
                };
                if !pty_sources.contains(&pane_id) {
                    continue;
                }
                let entry = watching.entry(pane_id).or_insert_with(|| WatchedSource {
                    public_pane_id: pane.pane_id.clone(),
                    watching_clients: HashSet::new(),
                    width: 0,
                    height: 0,
                });
                entry.watching_clients.insert(recipient.client_id);
                entry.width = entry.width.max(pane.inner_rect.width);
                entry.height = entry.height.max(pane.inner_rect.height);
            }
        }

        let mut collected = Vec::with_capacity(watching.len());
        for source in pty_sources {
            let Some(WatchedSource {
                public_pane_id,
                watching_clients,
                width,
                height,
            }) = watching.remove(source)
            else {
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
        'recipient: for recipient in &recipients {
            let client_id = recipient.client_id;
            let surface = recipient.surface;
            let mut panes = surface.panes.clone();
            // RS-10：公开 id → 本接收者基线内的下标，避免逐个收集 pane 做线性
            // 扫描 + 字符串比较。
            let mut pane_positions: HashMap<&str, usize> =
                HashMap::with_capacity(surface.panes.len());
            for (index, pane) in surface.panes.iter().enumerate() {
                pane_positions.entry(pane.pane_id.as_str()).or_insert(index);
            }
            let projection_revision = surface.projection_revision;
            let base_surface_revision = surface.surface_revision;
            let mut changed_panes = Vec::with_capacity(collected.len());
            let mut patch_rows = Vec::new();
            let mut metadata_changed = false;
            let mut new_hyperlink_uris: Vec<String> = Vec::new();
            let mut refresh_graphics = !surface.graphics.placements.is_empty()
                || !surface.graphics.retained_assets.is_empty();
            for collected_pane in &collected {
                let Some(index) = pane_positions.get(collected_pane.pane_id.as_str()).copied()
                else {
                    continue;
                };
                let Some(pane) = panes.get_mut(index) else {
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
                let mut rows_present = !rows.is_empty();
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
                rows_present |= !scrollbar_rows.is_empty();
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
                let pane_metadata_changed = *pane != previous_pane;
                metadata_changed |= pane_metadata_changed;
                // RS-18：只回传真正变化的 pane 元数据——但带行的 pane 必须保留，
                // 客户端按 patch.panes 校验行归属（行必须落在某个条目的
                // inner_rect / scrollbar_rect 内），缺条目会让整补丁被拒。
                if pane_metadata_changed || rows_present {
                    changed_panes.push(pane.clone());
                }
            }

            // 链接表上界（见常量说明）：超限剔除该接收者，延期一次全量渲染重建表。
            if surface
                .frame
                .hyperlinks
                .len()
                .saturating_add(new_hyperlink_uris.len())
                > MAX_RECIPIENT_HYPERLINKS
            {
                crate::render_prof::event("retained_surface.defer.hyperlink_table_full");
                deferred_clients.insert(client_id);
                continue 'recipient;
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
                let Some((graphics, delivery, sources)) =
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
                Some((next_surface, delivery, sources))
            } else {
                None
            };
            if patch.rows.is_empty() && !cursor_changed && !metadata_changed && !graphics_changed {
                continue;
            }
            updates.push(RetainedRecipientUpdate {
                client_id,
                view: recipient.view.clone(),
                patch,
                graphics,
            });
        }
        // 上游 #4508：收集补丁期间有接收者的可见 pane 进入同步输出批次——整 tick
        // 回退，由完整渲染推迟到批次结束。先求值再释放对接收者基线的借用。
        let synchronized_during_patch = recipients
            .iter()
            .any(|recipient| has_synchronized_pane(&self.app, recipient.surface));
        drop(recipients);
        self.arm_deferred_full_render(&deferred_clients);
        if updates.is_empty() {
            success!("unchanged");
        }
        if synchronized_during_patch {
            fallback!("synchronized_during_patch");
        }

        let mut sent = 0u64;
        let mut deferred = 0u64;
        let mut disconnected = Vec::new();
        let mut grouped = HashMap::<u64, Vec<RetainedRecipientUpdate>>::new();
        for update in updates {
            grouped.entry(update.client_id).or_default().push(update);
        }
        for (client_id, mut updates) in grouped {
            // 上游 #4561：原生 kitty 图形状态按客户端单槽，只服务主 surface；多视图
            // 客户端的 view 仍走内联资产，源文件就地物化。
            let mut native_upload = None;
            let mut native_geometry_deferred = false;
            for update in &mut updates {
                let Some((surface, delivery, sources)) = update.graphics.as_mut() else {
                    continue;
                };
                if update.view.is_some() {
                    self.materialize_native_sources(
                        client_id,
                        &mut surface.graphics,
                        delivery,
                        sources,
                    );
                    continue;
                }
                if self.defer_changed_native_geometry(client_id, &surface.graphics) {
                    native_geometry_deferred = true;
                    break;
                }
                native_upload =
                    self.prepare_native_scene(client_id, &mut surface.graphics, delivery, sources);
            }
            if native_geometry_deferred {
                deferred += 1;
                continue;
            }
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
            let mut main_committed = false;
            for update in updates {
                let is_main = update.view.is_none();
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
                // The published row patch cannot carry images. Reuse the retained
                // text/layout in a graphics-capable surface message rather than
                // invoking the full renderer.
                let (prepared, delivery) = if let Some((surface, delivery, _)) = update.graphics {
                    let prepared = if is_main {
                        state.prepare_pane_surface_with_file(surface, native_upload.is_some())
                    } else {
                        state.prepare_pane_surface(surface)
                    };
                    (prepared, Some(delivery))
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
                    warn!(client_id, "failed to serialize retained pane surface patch");
                    // A delta may own an encoded graphics payload that cannot be
                    // trimmed in place. Force the bounded full-surface recovery path.
                    state.request_repaint();
                    needs_retry = true;
                    continue;
                };
                if batch.len().saturating_add(serialized.len()) > MAX_GRAPHICS_FRAME_SIZE {
                    needs_retry = true;
                    continue;
                }
                batch.extend_from_slice(&serialized);
                main_committed |= is_main;
                commits.push((update.view, prepared, delivery));
            }
            // 主 surface 未进本批时不发原生上传（上传必须紧跟它引用的场景）。
            let native_upload = native_upload.filter(|_| main_committed);
            if let Some((_, message)) = &native_upload {
                let Ok(file_frame) =
                    Self::frame_server_message_with_max(message, MAX_GRAPHICS_FRAME_SIZE)
                else {
                    client.defer_full_render();
                    deferred += 1;
                    continue;
                };
                batch.extend_from_slice(&file_frame);
            }
            crate::render_prof::counter("retained_surface.bytes", batch.len() as u64);
            if batch.is_empty() {
                if needs_retry {
                    client.defer_full_render();
                    deferred += 1;
                }
                continue;
            }
            let send = if native_upload.is_some() || self.native_graphics.is_pending(client_id) {
                writer.render.send_ordered(batch)
            } else {
                writer.render.try_send(batch)
            };
            match send {
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
                            if let Some((graphics, inline_assets)) =
                                prepared.queued_surface_graphics()
                            {
                                self.native_graphics.commit_scene(
                                    client_id,
                                    graphics,
                                    inline_assets,
                                );
                            }
                            client.render_state.commit_sent_frame(prepared);
                            if let Some(delivery) = delivery {
                                client.shell_graphics_delivery = delivery;
                            }
                        }
                        sent += 1;
                    }
                    if let Some((pending, _)) = native_upload {
                        self.native_graphics.commit(client_id, pending);
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

    /// RS-13：窄 pane（完整渲染不会留出滚动条留白）不补滚动条；正常宽度补。
    /// RS-13（复审补测）：5 列宽 pane 的留白可用——基线没有滚动条时，开始滚动
    /// （`max_offset_from_bottom > 0`）后补丁里出现留白列的滚动条；4 列宽（完整渲染
    /// 不留留白）即使滚动也不出现。
    #[test]
    fn narrow_pane_scrollbar_appears_only_where_the_full_render_reserves_a_gutter() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let app = crate::app::App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        let frame = FrameData {
            width: 12,
            height: 8,
            cells: vec![cell(" "); 12 * 8],
            cursor: None,
            hyperlinks: Vec::new(),
            graphics: Vec::new(),
        };
        let pane = |inner_width: u16| protocol::PaneSurfacePane {
            pane_id: "w1:p1".into(),
            content_revision: 0,
            rect: protocol::SurfaceRect {
                x: 0,
                y: 0,
                width: inner_width + 2,
                height: 8,
            },
            inner_rect: protocol::SurfaceRect {
                x: 1,
                y: 0,
                width: inner_width,
                height: 8,
            },
            scrollbar_rect: None,
            scroll: None,
            focused: true,
            mouse_reporting: false,
            sgr_pixel_mouse: false,
            alternate_screen_active: false,
            pixel_width: 0,
            pixel_height: 0,
        };
        let metrics = crate::pane::ScrollMetrics {
            offset_from_bottom: 0,
            max_offset_from_bottom: 4,
            viewport_rows: 8,
        };

        let mut wide = pane(5);
        let rows = retained_scrollbar_patch(&app, &frame, &mut wide, false, Some(metrics))
            .expect("scrollbar patch");
        assert!(
            !rows.is_empty(),
            "a 5-column pane must show the gutter once it scrolls"
        );
        assert!(wide.scrollbar_rect.is_some());

        let mut narrow = pane(4);
        let rows = retained_scrollbar_patch(&app, &frame, &mut narrow, false, Some(metrics))
            .expect("narrow pane patch");
        assert!(rows.is_empty(), "a 4-column pane has no reserved gutter");
        assert!(narrow.scrollbar_rect.is_none());
    }

    #[test]
    fn scrollbar_gutter_follows_the_full_render_layout_rule() {
        let pane = |inner_width: u16, baseline_scrollbar: bool| protocol::PaneSurfacePane {
            pane_id: "w1:p1".into(),
            content_revision: 0,
            rect: protocol::SurfaceRect {
                x: 0,
                y: 0,
                width: inner_width + 2,
                height: 10,
            },
            inner_rect: protocol::SurfaceRect {
                x: 1,
                y: 1,
                width: inner_width,
                height: 8,
            },
            scrollbar_rect: baseline_scrollbar.then_some(protocol::SurfaceRect {
                x: 1 + inner_width,
                y: 1,
                width: 1,
                height: 8,
            }),
            scroll: None,
            focused: true,
            mouse_reporting: false,
            sgr_pixel_mouse: false,
            alternate_screen_active: false,
            pixel_width: 0,
            pixel_height: 0,
        };

        assert!(!scrollbar_gutter_available(&pane(3, false)));
        assert!(!scrollbar_gutter_available(&pane(4, false)));
        assert!(scrollbar_gutter_available(&pane(4, true)));
        assert!(scrollbar_gutter_available(&pane(5, false)));
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

    /// 非门禁扩展剖析（`just bench-render-scale` 的 `render_scale_profile` 过滤命中）：
    /// 带链接脏行的补丁翻译 + 逐格 diff 在 1 / 15 pane 下的成本，并给出同形状
    /// 无链接行作对照。修复前该形状每 tick 直接回落整帧，所以这里是净新增工作量
    /// 的口径（不是与被删代码的等价替换）。
    #[test]
    #[ignore = "non-gating hyperlink patch translation profile"]
    fn render_scale_profile_hyperlink_patch_translation() {
        const WIDTH: u16 = 120;
        const PANE_HEIGHT: u16 = 40;
        const LINKED_CELLS: usize = 40;
        const SAMPLES: usize = 200;
        for count in [1usize, 15] {
            let height = PANE_HEIGHT * count as u16;
            let baseline_uris: Vec<String> = (0..64)
                .map(|index| format!("https://known.example/{index}"))
                .collect();
            let frame = FrameData {
                width: WIDTH,
                height,
                cells: vec![cell("x"); usize::from(WIDTH) * usize::from(height)],
                cursor: None,
                hyperlinks: baseline_uris.clone(),
                graphics: Vec::new(),
            };
            // 局部链接表：0 号是本轮新 URI，1..=8 复用基线已有 URI；行内 40 格
            // 按 8 格一段重复引用，覆盖「连续同 URI 游程」这一常见形态。
            let local_uris: Vec<String> = std::iter::once("https://new.example/0".to_owned())
                .chain(baseline_uris.iter().take(8).cloned())
                .collect();
            let linked_row = |plain: bool| {
                let mut row = vec![cell("x"); usize::from(WIDTH)];
                if !plain {
                    for (index, cell) in row.iter_mut().take(LINKED_CELLS).enumerate() {
                        cell.hyperlink = Some(u32::try_from(index / 8).unwrap_or(0));
                    }
                }
                row
            };
            for plain in [true, false] {
                let patch = crate::pane::TerminalDirtyPatch {
                    rows: vec![(0, linked_row(plain))],
                    hyperlinks: if plain {
                        Vec::new()
                    } else {
                        local_uris.clone()
                    },
                };
                let run = || {
                    let started = Instant::now();
                    let mut new_hyperlink_uris = Vec::new();
                    for pane_index in 0..count {
                        let area = protocol::SurfaceRect {
                            x: 0,
                            y: PANE_HEIGHT * pane_index as u16,
                            width: WIDTH,
                            height: PANE_HEIGHT,
                        };
                        let rows = changed_rows(&frame, area, &patch, &mut new_hyperlink_uris)
                            .expect("valid patch");
                        std::hint::black_box(rows);
                    }
                    started.elapsed().as_micros()
                };
                for _ in 0..20 {
                    std::hint::black_box(run());
                }
                let mut samples: Vec<u128> = (0..SAMPLES).map(|_| run()).collect();
                samples.sort_unstable();
                println!(
                    "hyperlink translation panes={count} plain={plain} median_us={} p95_us={} max_us={}",
                    samples[SAMPLES / 2],
                    samples[SAMPLES * 95 / 100],
                    samples[SAMPLES - 1],
                );
            }
        }
    }
}
