use super::*;
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

const SELECTION_REPAINT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(16);

/// 侧栏 / 标签条把「按住不放的移动」升级成拖拽所需的最小位移（单位：cell）。
/// 阈值只有 1 cell 时轻微手抖就会建立一次指向自身槽位的拖拽，抬起时既不重排
/// 也不切换，用户看到的是「什么都没发生」（HERDR-UX-004）；与工作台面板拖动
/// （`workbench::interaction` 的 `> 1`）取同一档。
const CHROME_DRAG_THRESHOLD_CELLS: u16 = 2;

impl ClientShellState {
    fn selection_autoscroll_interval(&self) -> std::time::Duration {
        self.config.selection_autoscroll_interval
    }

    fn selection_edge_scroll_lines(&self, distance: u16) -> usize {
        let min = self.config.selection_autoscroll_min_lines.max(1);
        let max = self.config.selection_autoscroll_max_lines.max(min);
        usize::from(distance).saturating_mul(min).clamp(min, max)
    }
    fn set_sidebar_width_from_column(&mut self, column: u16, outcome: &mut ClientShellInput) {
        let (min, max) = crate::config::validated_sidebar_bounds(
            self.config.sidebar_min_width,
            self.config.sidebar_max_width,
        )
        .unwrap_or((18, 36));
        let width = column.saturating_add(1).clamp(min, max);
        if self.sidebar_width != width {
            self.sidebar_width = width;
            self.sidebar_width_manual = true;
            self.invalidate_pane_surface();
            outcome.repaint = true;
            outcome.resize = true;
        }
    }

    fn set_sidebar_section_from_row(&mut self, row: u16, outcome: &mut ClientShellInput) {
        let divider = self.hits.sidebar_divider;
        if divider.height == 0 {
            return;
        }
        let ratio = row.saturating_sub(divider.y) as f32 / divider.height as f32;
        let ratio = ratio.clamp(0.1, 0.9);
        if (self.sidebar_section_split - ratio).abs() > f32::EPSILON {
            self.sidebar_section_split = ratio;
            self.sidebar_section_split_manual = true;
            outcome.repaint = true;
        }
    }

    fn pane_scrollbar_offset(
        hit: &PaneHit,
        row: u16,
        grab_row_offset: Option<u16>,
    ) -> Option<usize> {
        let track = hit.scrollbar_rect?;
        let metrics = hit.scroll?;
        (metrics.max_offset_from_bottom > 0).then(|| match grab_row_offset {
            Some(grab_row_offset) => {
                crate::ui::scrollbar_offset_from_drag_row(metrics, track, row, grab_row_offset)
            }
            None => crate::ui::scrollbar_offset_from_row(metrics, track, row),
        })
    }

    pub(super) fn push_pane_scroll_offset(
        &mut self,
        pane_id: String,
        offset_from_bottom: usize,
        outcome: &mut ClientShellInput,
    ) {
        self.pane_scroll_targets
            .insert(pane_id.clone(), offset_from_bottom);
        if self.pane_scroll_in_flight.contains_key(&pane_id) {
            self.pane_scroll_queued.insert(pane_id, offset_from_bottom);
            return;
        }
        self.dispatch_pane_scroll_offset(pane_id, offset_from_bottom, outcome);
    }

    fn dispatch_pane_scroll_offset(
        &mut self,
        pane_id: String,
        offset_from_bottom: usize,
        outcome: &mut ClientShellInput,
    ) {
        if self.snapshot.is_none() {
            return;
        }
        self.next_scroll_serial = self.next_scroll_serial.saturating_add(1);
        let serial = self.next_scroll_serial;
        self.pane_scroll_targets
            .insert(pane_id.clone(), offset_from_bottom);
        self.pane_scroll_in_flight.insert(pane_id.clone(), serial);
        if !self.push_endpoint_method_with_kind(
            crate::api::schema::Method::PaneScroll(crate::api::schema::PaneScrollParams {
                pane_id: pane_id.clone(),
                offset_from_bottom: offset_from_bottom as u64,
            }),
            PendingEndpointKind::PaneScroll {
                pane_id: pane_id.clone(),
                serial,
            },
            outcome,
        ) {
            self.pane_scroll_targets.remove(&pane_id);
            self.pane_scroll_in_flight.remove(&pane_id);
        }
    }

    pub(super) fn complete_pane_scroll(
        &mut self,
        pane_id: String,
        serial: u64,
        result: Result<crate::api::schema::ResponseResult, ClientShellEndpointError>,
        outcome: &mut ClientShellInput,
    ) -> bool {
        if self.pane_scroll_in_flight.get(&pane_id).copied() != Some(serial) {
            return false;
        }
        self.pane_scroll_in_flight.remove(&pane_id);
        let repaint = match result {
            Ok(crate::api::schema::ResponseResult::PaneInfo { pane })
                if pane.pane_id == pane_id =>
            {
                if let Some(scroll) = pane.scroll {
                    if self.pane_scroll_targets.contains_key(&pane_id) {
                        self.pane_scroll_targets.insert(
                            pane_id.clone(),
                            usize::try_from(scroll.offset_from_bottom).unwrap_or(usize::MAX),
                        );
                    }
                }
                false
            }
            Ok(_) => {
                self.pane_scroll_queued.remove(&pane_id);
                self.pane_scroll_targets.remove(&pane_id);
                self.set_endpoint_error("endpoint returned an unexpected pane-scroll result");
                true
            }
            Err(_) => {
                self.pane_scroll_queued.remove(&pane_id);
                self.pane_scroll_targets.remove(&pane_id);
                true
            }
        };
        if let Some(offset) = self.pane_scroll_queued.remove(&pane_id) {
            self.dispatch_pane_scroll_offset(pane_id, offset, outcome);
        }
        repaint
    }

    pub(super) fn stop_selection_autoscroll(&mut self) {
        self.selection_autoscroll = None;
        self.selection_autoscroll_deadline = None;
    }

    fn selection_scroll_metrics(&self, hit: &PaneHit) -> Option<crate::pane::ScrollMetrics> {
        let metrics = hit.scroll?;
        Some(
            self.selection_autoscroll
                .as_ref()
                .filter(|autoscroll| autoscroll.pane_id == hit.pane_id)
                .map_or(metrics, |autoscroll| crate::pane::ScrollMetrics {
                    offset_from_bottom: autoscroll.offset_from_bottom,
                    max_offset_from_bottom: autoscroll.max_offset_from_bottom,
                    viewport_rows: metrics.viewport_rows,
                }),
        )
    }

    fn active_selection_pane(&self) -> Option<PaneHit> {
        let pane_id = if let Some(gesture) = self.word_selection_gesture.as_ref() {
            if gesture.released {
                return None;
            }
            &gesture.pane_id
        } else {
            &self
                .selection
                .as_ref()
                .filter(|selection| selection.is_in_progress())?
                .pane_id
        };
        self.hits
            .panes
            .iter()
            .find(|hit| &hit.pane_id == pane_id)
            .cloned()
    }

    fn update_selection_cursor_with_metrics(
        &mut self,
        hit: &PaneHit,
        column: u16,
        row: u16,
        metrics: Option<crate::pane::ScrollMetrics>,
        outcome: &mut ClientShellInput,
    ) {
        if self.word_selection_gesture.is_some() {
            let viewport_row = row
                .saturating_sub(hit.inner_rect.y)
                .min(hit.inner_rect.height.saturating_sub(1));
            let col = column
                .saturating_sub(hit.inner_rect.x)
                .min(hit.inner_rect.width.saturating_sub(1));
            let absolute_row = crate::selection::absolute_row_for_viewport(viewport_row, metrics);
            self.drag_word_selection((absolute_row, col), outcome);
        } else if let Some(selection) = self.selection.as_mut() {
            selection.drag(column, row, hit.inner_rect, metrics);
        }
    }

    fn update_selection_drag(
        &mut self,
        hit: &PaneHit,
        column: u16,
        row: u16,
        outcome: &mut ClientShellInput,
    ) {
        let metrics = self.selection_scroll_metrics(hit);
        let was_dragging = self
            .selection
            .as_ref()
            .is_some_and(crate::selection::Selection::is_dragging);
        let moved_from_anchor = self.selection.as_ref().is_some_and(|selection| {
            let (anchor_row, anchor_col) = selection.anchor_screen_pos(hit.inner_rect, metrics);
            anchor_row != row || anchor_col != column
        });
        self.update_selection_cursor_with_metrics(hit, column, row, metrics, outcome);
        let is_dragging = self
            .word_selection_gesture
            .as_ref()
            .map_or(was_dragging || moved_from_anchor, |gesture| gesture.dragged);
        if is_dragging {
            if let Some(selection) = self.selection.as_mut() {
                if selection.is_just_click() {
                    selection.force_dragging();
                }
            }
            self.last_pane_click = None;
        }
        if !is_dragging {
            self.stop_selection_autoscroll();
            return;
        }

        let Some(metrics) = metrics else {
            self.stop_selection_autoscroll();
            return;
        };
        let top = hit.inner_rect.y;
        let bottom = hit.inner_rect.y + hit.inner_rect.height.saturating_sub(1);
        let (direction, immediate_lines) = if row < top {
            (
                ClientSelectionAutoscrollDirection::Up,
                self.selection_edge_scroll_lines(top - row),
            )
        } else if row > bottom {
            (
                ClientSelectionAutoscrollDirection::Down,
                self.selection_edge_scroll_lines(row - bottom),
            )
        } else if row == top {
            (ClientSelectionAutoscrollDirection::Up, 0)
        } else if row == bottom {
            (ClientSelectionAutoscrollDirection::Down, 0)
        } else {
            self.stop_selection_autoscroll();
            return;
        };

        let offset_from_bottom = match direction {
            ClientSelectionAutoscrollDirection::Up => metrics
                .offset_from_bottom
                .saturating_add(immediate_lines)
                .min(metrics.max_offset_from_bottom),
            ClientSelectionAutoscrollDirection::Down => {
                metrics.offset_from_bottom.saturating_sub(immediate_lines)
            }
        };
        if offset_from_bottom != metrics.offset_from_bottom {
            let projected = crate::pane::ScrollMetrics {
                offset_from_bottom,
                ..metrics
            };
            self.update_selection_cursor_with_metrics(hit, column, row, Some(projected), outcome);
            self.push_pane_scroll_offset(hit.pane_id.clone(), offset_from_bottom, outcome);
        }
        self.selection_autoscroll = Some(ClientSelectionAutoscroll {
            pane_id: hit.pane_id.clone(),
            direction,
            last_mouse_column: column,
            last_mouse_row: row,
            inner_rect: hit.inner_rect,
            offset_from_bottom,
            max_offset_from_bottom: metrics.max_offset_from_bottom,
        });
        self.selection_autoscroll_deadline =
            Some(std::time::Instant::now() + self.selection_autoscroll_interval());
    }

    fn scroll_in_progress_selection(
        &mut self,
        mouse: MouseEvent,
        outcome: &mut ClientShellInput,
    ) -> bool {
        if !matches!(
            mouse.kind,
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
        ) {
            return false;
        }
        let Some(hit) = self.active_selection_pane() else {
            return false;
        };
        let Some(metrics) = self.selection_scroll_metrics(&hit) else {
            return false;
        };
        let offset_from_bottom = match mouse.kind {
            MouseEventKind::ScrollUp => metrics
                .offset_from_bottom
                .saturating_add(self.config.mouse_scroll_lines)
                .min(metrics.max_offset_from_bottom),
            MouseEventKind::ScrollDown => metrics
                .offset_from_bottom
                .saturating_sub(self.config.mouse_scroll_lines),
            _ => unreachable!(),
        };
        if offset_from_bottom != metrics.offset_from_bottom {
            let projected = crate::pane::ScrollMetrics {
                offset_from_bottom,
                ..metrics
            };
            self.update_selection_cursor_with_metrics(
                &hit,
                mouse.column,
                mouse.row,
                Some(projected),
                outcome,
            );
            self.push_pane_scroll_offset(hit.pane_id, offset_from_bottom, outcome);
            outcome.repaint = true;
        }
        true
    }

    pub(super) fn request_selection_drag_repaint(&mut self, now: std::time::Instant) -> bool {
        let deadline = self
            .last_composed_at
            .map(|last| last + SELECTION_REPAINT_INTERVAL);
        self.selection_repaint_deadline = deadline.filter(|deadline| now < *deadline);
        self.selection_repaint_deadline.is_none()
    }

    pub(crate) fn tick_selection_autoscroll(
        &mut self,
        now: std::time::Instant,
    ) -> ClientShellInput {
        let mut outcome = ClientShellInput::default();
        self.tick_frozen_selection(now, &mut outcome);
        if self
            .selection_repaint_deadline
            .is_some_and(|deadline| now >= deadline)
        {
            self.selection_repaint_deadline = None;
            outcome.repaint = true;
        }
        if self
            .selection_autoscroll_deadline
            .is_none_or(|deadline| now < deadline)
        {
            return outcome;
        }
        let Some(mut autoscroll) = self.selection_autoscroll.clone() else {
            self.selection_autoscroll_deadline = None;
            return outcome;
        };
        let dragging = self.word_selection_gesture.as_ref().map_or_else(
            || {
                self.selection.as_ref().is_some_and(|selection| {
                    selection.pane_id == autoscroll.pane_id && selection.is_dragging()
                })
            },
            |gesture| gesture.pane_id == autoscroll.pane_id && gesture.dragged && !gesture.released,
        );
        if !dragging {
            self.stop_selection_autoscroll();
            return outcome;
        }
        let Some(hit) = self
            .hits
            .panes
            .iter()
            .find(|hit| hit.pane_id == autoscroll.pane_id)
            .cloned()
        else {
            self.stop_selection_autoscroll();
            return outcome;
        };
        if hit.inner_rect != autoscroll.inner_rect {
            self.stop_selection_autoscroll();
            return outcome;
        }
        let next_offset = match autoscroll.direction {
            ClientSelectionAutoscrollDirection::Up => autoscroll
                .offset_from_bottom
                .saturating_add(1)
                .min(autoscroll.max_offset_from_bottom),
            ClientSelectionAutoscrollDirection::Down => {
                autoscroll.offset_from_bottom.saturating_sub(1)
            }
        };
        if next_offset == autoscroll.offset_from_bottom {
            self.stop_selection_autoscroll();
            return outcome;
        }
        autoscroll.offset_from_bottom = next_offset;
        let metrics = crate::pane::ScrollMetrics {
            offset_from_bottom: next_offset,
            max_offset_from_bottom: autoscroll.max_offset_from_bottom,
            viewport_rows: hit.scroll.map_or(0, |metrics| metrics.viewport_rows),
        };
        self.update_selection_cursor_with_metrics(
            &hit,
            autoscroll.last_mouse_column,
            autoscroll.last_mouse_row,
            Some(metrics),
            &mut outcome,
        );
        self.push_pane_scroll_offset(autoscroll.pane_id.clone(), next_offset, &mut outcome);
        self.selection_autoscroll = Some(autoscroll);
        self.selection_autoscroll_deadline = Some(now + self.selection_autoscroll_interval());
        outcome.repaint = true;
        outcome
    }

    fn pane_split_target_is_current(&self, hit: &PaneSplitHit, tab_id: &str) -> Option<bool> {
        let snapshot = self.snapshot.as_deref()?;
        if self.workbench.enabled {
            let view = self
                .workbench
                .views
                .values()
                .find(|view| view.tab == tab_id)?;
            if view.surface.projection_revision != snapshot.revision {
                return None;
            }
            return Some(
                hit.tab_id.as_deref() == Some(tab_id)
                    && pane_surface_topology_signature(&view.surface) == hit.topology_signature,
            );
        }
        let surface = self.pane_surface.as_ref()?;
        if snapshot.revision != surface.projection_revision {
            return None;
        }
        Some(
            snapshot.focused_tab_id.as_deref() == Some(tab_id)
                && pane_surface_topology_signature(surface) == hit.topology_signature,
        )
    }

    fn pane_split_ratio(hit: &PaneSplitHit, grab_offset: i32, point: (u16, u16)) -> f32 {
        let (pointer, origin, length) = match hit.direction {
            crate::protocol::PaneSurfaceSplitDirection::Horizontal => {
                (i32::from(point.0), i32::from(hit.area.x), hit.area.width)
            }
            crate::protocol::PaneSurfaceSplitDirection::Vertical => {
                (i32::from(point.1), i32::from(hit.area.y), hit.area.height)
            }
        };
        ((pointer + grab_offset - origin) as f32 / f32::from(length.max(1))).clamp(0.1, 0.9)
    }

    fn tab_drop_index_at(&self, point: (u16, u16)) -> Option<usize> {
        let snapshot = self.snapshot.as_deref()?;
        let workspace_id = snapshot.focused_workspace_id.as_deref()?;
        let tabs = snapshot
            .tabs
            .iter()
            .filter(|tab| tab.workspace_id == workspace_id)
            .collect::<Vec<_>>();
        let visible = self
            .hits
            .tabs
            .iter()
            .filter_map(|(rect, tab_id)| {
                tabs.iter()
                    .position(|tab| tab.tab_id == *tab_id)
                    .map(|index| (index, *rect))
            })
            .collect::<Vec<_>>();
        let (first_index, first_rect) = *visible.first()?;
        let (last_index, last_rect) = *visible.last()?;
        let on_tab_row = point.1 == first_rect.y;
        if !on_tab_row {
            return None;
        }
        if super::contains(self.hits.tab_scroll_left, point) {
            return Some(0);
        }
        if super::contains(self.hits.tab_scroll_right, point) {
            return Some(tabs.len());
        }
        let left_edge = if first_index == 0 {
            first_rect.x
        } else {
            self.hits.tab_scroll_left.right()
        };
        let right_edge = if last_index + 1 >= tabs.len() {
            last_rect.right()
        } else {
            self.hits.tab_scroll_right.x.saturating_sub(1)
        };
        if point.0 <= left_edge {
            return Some(first_index);
        }
        if point.0 >= right_edge {
            return Some(last_index + 1);
        }
        for (index, rect) in visible {
            let midpoint = rect.x + rect.width / 2;
            if point.0 < midpoint {
                return Some(index);
            }
            if point.0 < rect.right() {
                return Some(index + 1);
            }
        }
        Some(last_index + 1)
    }

    fn workspace_drop_target_at(&self, point: (u16, u16)) -> Option<(Option<String>, u16)> {
        if self.hits.workspace_body.height == 0
            || point.1 < self.hits.workspace_body.y.saturating_sub(1)
            || point.1 >= self.hits.new_workspace.y
            || self.hits.workspaces.iter().any(|hit| {
                hit.endpoint_id != self.active_endpoint_id && super::contains(hit.rect, point)
            })
        {
            return None;
        }
        let mut slots = self
            .hits
            .workspaces
            .iter()
            .filter(|hit| hit.endpoint_id == self.active_endpoint_id && !hit.indented)
            .map(|hit| (Some(hit.workspace_id.clone()), hit.rect.y.saturating_sub(1)))
            .collect::<Vec<_>>();
        let snapshot = self.snapshot.as_deref()?;
        let empty_collapsed_groups = HashSet::new();
        let collapsed_groups = self
            .collapsed_groups_for_endpoint(&self.active_endpoint_id)
            .unwrap_or(&empty_collapsed_groups);
        let entries = render::workspace_entries(snapshot, collapsed_groups);
        let last_hit = self
            .hits
            .workspaces
            .iter()
            .rev()
            .find(|hit| hit.endpoint_id == self.active_endpoint_id)?;
        let last_position = entries.iter().position(|entry| {
            snapshot
                .workspaces
                .get(entry.index)
                .is_some_and(|workspace| workspace.workspace_id == last_hit.workspace_id)
        })?;
        let next = entries.get(last_position + 1);
        if !next.is_some_and(|entry| entry.indented) {
            let before = next.and_then(|entry| {
                snapshot
                    .workspaces
                    .get(entry.index)
                    .map(|workspace| workspace.workspace_id.clone())
            });
            let row = last_hit.rect.bottom();
            if row < self.hits.new_workspace.y {
                slots.push((before, row));
            }
        }
        slots
            .into_iter()
            .enumerate()
            .min_by_key(|(index, (_, row))| (point.1.abs_diff(*row), *index))
            .map(|(_, target)| target)
    }

    /// 把 `source` 拖到 `before_workspace_id` 之前是否真的改变顺序。落点槽位
    /// 就是源槽位（`before` 是自己，或插入位置等于当前位置）时返回 false：
    /// 这类拖拽建立了也只会在抬起时被丢弃，反而吃掉原本的点击（HERDR-UX-004）。
    /// 纯判定、无分配，供指针移动的每个事件调用。
    fn workspace_drop_reorders(
        &self,
        source_workspace_id: &str,
        before_workspace_id: Option<&str>,
    ) -> bool {
        let Some(snapshot) = self.snapshot.as_deref() else {
            return false;
        };
        if before_workspace_id == Some(source_workspace_id) {
            return false;
        }
        let roots = || {
            snapshot.workspaces.iter().filter(|workspace| {
                !workspace
                    .worktree
                    .as_ref()
                    .is_some_and(|worktree| worktree.is_linked_worktree)
            })
        };
        let Some(source_position) =
            roots().position(|workspace| workspace.workspace_id == source_workspace_id)
        else {
            return false;
        };
        let remaining =
            || roots().filter(|workspace| workspace.workspace_id != source_workspace_id);
        let insert_position = match before_workspace_id {
            Some(target) => {
                match remaining().position(|workspace| workspace.workspace_id == target) {
                    Some(position) => position,
                    None => return false,
                }
            }
            None => remaining().count(),
        };
        insert_position != source_position
    }

    /// 把 `tab_id` 插到 `insert_index` 是否真的改变顺序。判据不在这里重写，
    /// 直接调服务端 `Workspace::move_tab` 用的同一个
    /// `crate::workspace::reorder_target_index`——客户端自己抄一份的话，日后
    /// 谁改了插入语义都不会变红，症状却是静默的（客户端判定能排、服务端丢弃，
    /// 或反过来把重排手势变成切换）。
    pub(super) fn tab_drop_reorders(&self, tab_id: &str, insert_index: usize) -> bool {
        let Some(snapshot) = self.snapshot.as_deref() else {
            return false;
        };
        let Some(workspace_id) = snapshot.focused_workspace_id.as_deref() else {
            return false;
        };
        let mut count = 0usize;
        let mut position = None;
        for tab in snapshot
            .tabs
            .iter()
            .filter(|tab| tab.workspace_id == workspace_id)
        {
            if tab.tab_id == tab_id {
                position = Some(count);
            }
            count += 1;
        }
        let Some(position) = position else {
            return false;
        };
        crate::workspace::reorder_target_index(position, insert_index, count).is_some()
    }

    fn workspace_move_method(
        &self,
        source_workspace_id: &str,
        before_workspace_id: Option<&str>,
    ) -> Option<crate::api::schema::Method> {
        let snapshot = self.snapshot.as_deref()?;
        let source = snapshot
            .workspaces
            .iter()
            .find(|workspace| workspace.workspace_id == source_workspace_id)?;
        if source
            .worktree
            .as_ref()
            .is_some_and(|worktree| worktree.is_linked_worktree)
        {
            return None;
        }
        if !self.workspace_drop_reorders(source_workspace_id, before_workspace_id) {
            return None;
        }

        if let Some(worktree) = source.worktree.as_ref() {
            let workspace_ids = std::iter::once(source.workspace_id.clone())
                .chain(
                    snapshot
                        .workspaces
                        .iter()
                        .filter(|workspace| workspace.workspace_id != source.workspace_id)
                        .filter(|workspace| {
                            workspace
                                .worktree
                                .as_ref()
                                .is_some_and(|candidate| candidate.key == worktree.key)
                        })
                        .map(|workspace| workspace.workspace_id.clone()),
                )
                .collect();
            Some(crate::api::schema::Method::WorkspaceMoveBlock(
                crate::api::schema::WorkspaceMoveBlockParams {
                    workspace_ids,
                    before_workspace_id: before_workspace_id.map(str::to_owned),
                },
            ))
        } else {
            let insert_index = before_workspace_id
                .and_then(|target| {
                    snapshot
                        .workspaces
                        .iter()
                        .position(|workspace| workspace.workspace_id == target)
                })
                .unwrap_or(snapshot.workspaces.len());
            Some(crate::api::schema::Method::WorkspaceMove(
                crate::api::schema::WorkspaceMoveParams {
                    workspace_id: source.workspace_id.clone(),
                    insert_index,
                },
            ))
        }
    }

    pub(super) fn handle_mouse(&mut self, mouse: MouseEvent, outcome: &mut ClientShellInput) {
        if self.pending_selection_mouse(mouse, outcome) {
            return;
        }
        if self.frozen_selection_mouse(mouse, outcome) {
            return;
        }
        if self.floating_page_mouse(mouse, outcome) {
            return;
        }
        let point = (mouse.column, mouse.row);
        if self.config.mouse_capture
            && mouse.kind == MouseEventKind::Down(MouseButton::Left)
            && !contains(self.observability.hover_rect, point)
            && self.observability.process_dialog.is_none()
            && contains(self.hits.global_launcher, point)
            && !contains(self.hits.overlay_bounds, point)
            && !matches!(
                self.overlay,
                Some(
                    ClientShellOverlay::MachineAuth(_)
                        | ClientShellOverlay::ConfirmClose(_)
                        | ClientShellOverlay::Onboarding
                        | ClientShellOverlay::ProductAnnouncement(_)
                )
            )
            && !contains(self.hits.notification_toast, point)
            && !contains(self.hits.lifecycle_banner_retry, point)
            && !contains(self.hits.lifecycle_banner_give_up, point)
        {
            self.toggle_global_menu();
            outcome.repaint = true;
            return;
        }
        if self.workbench_mouse(mouse, outcome) {
            return;
        }
        if self.observation_mouse(mouse, outcome) {
            return;
        }
        self.update_link_hover(mouse, outcome);
        // 按下 / 拖拽 / 抬起 / 滚轮这类明确的指针动作结束 link hints 模式（它是
        // 键盘驱动的）；单纯的移动不结束——1003 模式下终端全程上报指针位置，
        // 手指没离开触控板、桌面震一下都会在第二个字母之前取消（HERDR-UX-007）。
        if mouse.kind != MouseEventKind::Moved && self.link_hints.take().is_some() {
            outcome.repaint = true;
        }
        let point = (mouse.column, mouse.row);
        if mouse.kind == MouseEventKind::Moved {
            self.update_chrome_hover(point, outcome);
        }
        if self.mode == ClientShellMode::Navigate
            && self.workspace_preview_action_blocked()
            && self.overlay.is_none()
            && !self.mobile_layout_active()
            && mouse.kind == MouseEventKind::Down(MouseButton::Left)
        {
            self.mode = self.copy_or_terminal_mode();
            self.navigate_workspace_id = None;
            outcome.repaint = true;
        }
        if matches!(self.overlay, Some(ClientShellOverlay::Onboarding)) {
            if mouse.kind == MouseEventKind::Down(MouseButton::Left)
                && super::contains(self.hits.overlay_primary, point)
            {
                self.complete_onboarding(outcome);
            }
            return;
        }
        if matches!(
            self.overlay,
            Some(ClientShellOverlay::ProductAnnouncement(_))
        ) {
            match mouse.kind {
                MouseEventKind::Down(MouseButton::Left)
                    if super::contains(self.hits.overlay_primary, point) =>
                {
                    self.dismiss_product_announcement(outcome);
                }
                MouseEventKind::Down(MouseButton::Left)
                    if super::contains(self.hits.product_announcement_scrollbar, point) =>
                {
                    if let Some(metrics) = self.hits.product_announcement_scroll_metrics {
                        if let Some(grab_row_offset) = crate::ui::scrollbar_thumb_grab_offset(
                            metrics,
                            self.hits.product_announcement_scrollbar,
                            mouse.row,
                        ) {
                            self.chrome_drag =
                                Some(ClientChromeDrag::ProductAnnouncementScrollbar {
                                    grab_row_offset,
                                });
                        } else {
                            let offset = crate::ui::scrollbar_offset_from_row(
                                metrics,
                                self.hits.product_announcement_scrollbar,
                                mouse.row,
                            );
                            self.set_product_announcement_offset_from_bottom(offset);
                            outcome.repaint = true;
                        }
                    }
                }
                MouseEventKind::Drag(MouseButton::Left) => {
                    if let (
                        Some(ClientChromeDrag::ProductAnnouncementScrollbar { grab_row_offset }),
                        Some(metrics),
                    ) = (
                        self.chrome_drag.as_ref(),
                        self.hits.product_announcement_scroll_metrics,
                    ) {
                        let offset = crate::ui::scrollbar_offset_from_drag_row(
                            metrics,
                            self.hits.product_announcement_scrollbar,
                            mouse.row,
                            *grab_row_offset,
                        );
                        self.set_product_announcement_offset_from_bottom(offset);
                        outcome.repaint = true;
                    }
                }
                MouseEventKind::Up(MouseButton::Left) => {
                    self.chrome_drag = None;
                }
                MouseEventKind::ScrollUp => {
                    self.scroll_product_announcement(-(self.config.mouse_scroll_lines as isize));
                    outcome.repaint = true;
                }
                MouseEventKind::ScrollDown => {
                    self.scroll_product_announcement(self.config.mouse_scroll_lines as isize);
                    outcome.repaint = true;
                }
                _ => {}
            }
            return;
        }
        if matches!(self.overlay, Some(ClientShellOverlay::ReleaseNotes(_))) {
            let (close, track, metrics) = self.current_release_notes_input_geometry().unwrap_or((
                Rect::default(),
                None,
                None,
            ));
            match mouse.kind {
                MouseEventKind::Down(MouseButton::Left) if super::contains(close, point) => {
                    self.dismiss_release_notes(outcome);
                }
                MouseEventKind::Down(MouseButton::Left)
                    if track.is_some_and(|track| super::contains(track, point)) =>
                {
                    if let (Some(track), Some(metrics)) = (track, metrics) {
                        if let Some(grab_row_offset) =
                            crate::ui::scrollbar_thumb_grab_offset(metrics, track, mouse.row)
                        {
                            self.chrome_drag =
                                Some(ClientChromeDrag::ReleaseNotesScrollbar { grab_row_offset });
                        } else {
                            let offset =
                                crate::ui::scrollbar_offset_from_row(metrics, track, mouse.row);
                            self.set_release_notes_offset_from_bottom(offset);
                            outcome.repaint = true;
                        }
                    }
                }
                MouseEventKind::Drag(MouseButton::Left) => {
                    if let (
                        Some(ClientChromeDrag::ReleaseNotesScrollbar { grab_row_offset }),
                        Some(track),
                        Some(metrics),
                    ) = (self.chrome_drag.as_ref(), track, metrics)
                    {
                        let offset = crate::ui::scrollbar_offset_from_drag_row(
                            metrics,
                            track,
                            mouse.row,
                            *grab_row_offset,
                        );
                        self.set_release_notes_offset_from_bottom(offset);
                        outcome.repaint = true;
                    }
                }
                MouseEventKind::Up(MouseButton::Left) => {
                    self.chrome_drag = None;
                }
                MouseEventKind::ScrollUp => {
                    self.scroll_release_notes(-(self.config.mouse_scroll_lines as isize));
                    outcome.repaint = true;
                }
                MouseEventKind::ScrollDown => {
                    self.scroll_release_notes(self.config.mouse_scroll_lines as isize);
                    outcome.repaint = true;
                }
                _ => {}
            }
            return;
        }
        if self.url_click_consumes_until_up {
            match mouse.kind {
                MouseEventKind::Drag(MouseButton::Left) => return,
                MouseEventKind::Up(MouseButton::Left) => {
                    self.url_click_consumes_until_up = false;
                    return;
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    self.url_click_consumes_until_up = false;
                }
                _ => {}
            }
        }
        if !self.replaying_url_click
            && matches!(
                mouse.kind,
                MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left)
            )
        {
            if let Some(fallback_events) =
                self.pending_requests
                    .values_mut()
                    .find_map(|pending| match &mut pending.kind {
                        PendingEndpointKind::PaneLinkActivate {
                            fallback_events, ..
                        } if !fallback_events
                            .iter()
                            .any(|event| event.kind == MouseEventKind::Up(MouseButton::Left)) =>
                        {
                            Some(fallback_events)
                        }
                        _ => None,
                    })
            {
                fallback_events.push(mouse);
                return;
            }
        }
        if let Some(gesture) = self.pane_mouse_gesture.as_ref() {
            let gesture_event = matches!(
                mouse.kind,
                MouseEventKind::Drag(button) | MouseEventKind::Up(button)
                    if button == gesture.button
            );
            if gesture_event {
                let button = gesture.button;
                let modifiers = mouse.modifiers.difference(gesture.stripped_modifiers);
                let hit = if gesture.hit.popup {
                    self.hits
                        .popup
                        .as_ref()
                        .filter(|hit| hit.pane_id == gesture.hit.pane_id)
                        .cloned()
                } else {
                    self.hits
                        .panes
                        .iter()
                        .find(|hit| hit.pane_id == gesture.hit.pane_id)
                        .cloned()
                }
                .unwrap_or_else(|| gesture.hit.clone());
                let position = self.pane_mouse_position(&hit, mouse);
                if let Some(gesture) = self.pane_mouse_gesture.as_mut() {
                    gesture.last_event = mouse;
                    gesture.last_position = position;
                }
                self.push_pane_mouse_event(&hit, mouse, modifiers, outcome);
                if mouse.kind == MouseEventKind::Up(button) {
                    self.pane_mouse_gesture = None;
                }
                return;
            }
            if matches!(
                mouse.kind,
                MouseEventKind::Down(_) | MouseEventKind::Drag(_) | MouseEventKind::Up(_)
            ) {
                return;
            }
        }
        if self.popup_pending && self.overlay.is_none() {
            return;
        }
        if let Some(hit) = self.hits.popup.clone().filter(|_| self.overlay.is_none()) {
            if super::contains(hit.inner_rect, point) {
                match mouse.kind {
                    MouseEventKind::Down(button) => {
                        self.push_pane_mouse_event(&hit, mouse, mouse.modifiers, outcome);
                        if hit.mouse_reporting {
                            self.pane_mouse_gesture = Some(ClientPaneMouseGesture {
                                last_position: self.pane_mouse_position(&hit, mouse),
                                hit,
                                button,
                                stripped_modifiers: crossterm::event::KeyModifiers::empty(),
                                last_event: mouse,
                            });
                        }
                    }
                    MouseEventKind::Moved if hit.mouse_reporting => {
                        self.push_pane_mouse_event(&hit, mouse, mouse.modifiers, outcome);
                    }
                    MouseEventKind::ScrollUp
                    | MouseEventKind::ScrollDown
                    | MouseEventKind::ScrollLeft
                    | MouseEventKind::ScrollRight => {
                        self.push_pane_mouse_event(&hit, mouse, mouse.modifiers, outcome);
                    }
                    MouseEventKind::Up(_) | MouseEventKind::Drag(_) | MouseEventKind::Moved => {}
                }
            }
            return;
        }
        if self.popup_terminal_id.is_some() && self.overlay.is_none() {
            return;
        }
        if !self.replaying_url_click
            && self.overlay.is_none()
            && self.mode == ClientShellMode::Terminal
            && mouse.kind == MouseEventKind::Down(MouseButton::Left)
            && mouse
                .modifiers
                .contains(crossterm::event::KeyModifiers::CONTROL)
        {
            if let Some(hit) = self
                .hits
                .panes
                .iter()
                .find(|hit| super::contains(hit.inner_rect, point))
                .cloned()
            {
                let viewport_row = mouse.row.saturating_sub(hit.inner_rect.y);
                let col = mouse.column.saturating_sub(hit.inner_rect.x);
                let content_revision = self
                    .pane_surface
                    .as_ref()
                    .and_then(|surface| {
                        surface
                            .panes
                            .iter()
                            .find(|pane| pane.pane_id == hit.pane_id)
                    })
                    .map(|pane| pane.content_revision);
                self.last_pane_click = None;
                let pane_id = hit.pane_id.clone();
                self.push_endpoint_method_with_kind(
                    crate::api::schema::Method::PaneLinkActivate(
                        crate::api::schema::PaneLinkActivateParams {
                            pane_id: pane_id.clone(),
                            viewport_row,
                            col,
                            content_revision,
                            offset_from_bottom: hit
                                .scroll
                                .map(|metrics| metrics.offset_from_bottom as u64),
                        },
                    ),
                    PendingEndpointKind::PaneLinkActivate {
                        pane_id,
                        inner_rect: hit.inner_rect,
                        fallback_events: vec![mouse],
                    },
                    outcome,
                );
                return;
            }
        }
        if self.overlay.is_none() && mouse.kind == MouseEventKind::Down(MouseButton::Left) {
            if super::contains(self.hits.lifecycle_banner_retry, point) {
                let endpoint_id = self.active_endpoint_id.clone();
                if !endpoint_id.is_local() {
                    outcome
                        .actions
                        .push(ClientShellAction::ReconnectEndpoint { endpoint_id });
                    outcome.repaint = true;
                }
                return;
            }
            if super::contains(self.hits.lifecycle_banner_give_up, point) {
                // Give up on the reconnect loop by disabling the machine; it
                // can be re-enabled from the machines overlay.
                if let ClientEndpointId::Ssh(profile_id) = self.active_endpoint_id.clone() {
                    self.machine_set_enabled(&profile_id, false);
                    outcome.repaint = true;
                }
                return;
            }
        }
        if self.visible_endpoint_notice.is_some()
            && mouse.kind == MouseEventKind::Down(MouseButton::Left)
            && super::contains(self.hits.notification_toast, point)
        {
            self.visible_endpoint_notice = None;
            outcome.repaint = true;
            return;
        }
        if self.overlay.is_none()
            && self.mode == ClientShellMode::Terminal
            && self
                .visible_notification
                .as_ref()
                .is_some_and(|notification| notification.event.pane_id.is_some())
            && mouse.kind == MouseEventKind::Down(MouseButton::Left)
            && super::contains(self.hits.notification_toast, point)
        {
            self.focus_visible_notification(outcome);
            return;
        }
        if self.handle_mobile_mouse(mouse, outcome) {
            return;
        }
        if mouse.kind == MouseEventKind::Drag(MouseButton::Left) {
            match self.chrome_drag.as_ref() {
                Some(ClientChromeDrag::SidebarWidth) => {
                    self.set_sidebar_width_from_column(mouse.column, outcome);
                    return;
                }
                Some(ClientChromeDrag::SidebarSection) => {
                    self.set_sidebar_section_from_row(mouse.row, outcome);
                    return;
                }
                Some(ClientChromeDrag::WorkspaceScrollbar { grab_row_offset }) => {
                    if let Some(metrics) = self.hits.workspace_scroll_metrics {
                        let offset = crate::ui::scrollbar_offset_from_drag_row(
                            metrics,
                            self.hits.workspace_scrollbar,
                            mouse.row,
                            *grab_row_offset,
                        );
                        let next = metrics.max_offset_from_bottom.saturating_sub(offset);
                        if next != self.workspace_scroll {
                            self.workspace_scroll = next;
                            outcome.repaint = true;
                        }
                    }
                    return;
                }
                Some(ClientChromeDrag::AgentScrollbar { grab_row_offset }) => {
                    if let Some(metrics) = self.hits.agent_scroll_metrics {
                        let offset = crate::ui::scrollbar_offset_from_drag_row(
                            metrics,
                            self.hits.agent_scrollbar,
                            mouse.row,
                            *grab_row_offset,
                        );
                        let next = metrics.max_offset_from_bottom.saturating_sub(offset);
                        if next != self.agent_scroll {
                            self.agent_scroll = next;
                            outcome.repaint = true;
                        }
                    }
                    return;
                }
                Some(ClientChromeDrag::NavigatorScrollbar { grab_row_offset }) => {
                    if let Some(metrics) = self.hits.navigator_scroll_metrics {
                        let offset = crate::ui::scrollbar_offset_from_drag_row(
                            metrics,
                            self.hits.navigator_scrollbar,
                            mouse.row,
                            *grab_row_offset,
                        );
                        self.scroll_navigator_to(
                            metrics.max_offset_from_bottom.saturating_sub(offset),
                            metrics.viewport_rows,
                        );
                        outcome.repaint = true;
                    }
                    return;
                }
                Some(ClientChromeDrag::HelpScrollbar { grab_row_offset }) => {
                    if let (Some(metrics), Some(ClientShellOverlay::Help(help))) =
                        (self.hits.help_scroll_metrics, self.overlay.as_mut())
                    {
                        let offset = crate::ui::scrollbar_offset_from_drag_row(
                            metrics,
                            self.hits.help_scrollbar,
                            mouse.row,
                            *grab_row_offset,
                        );
                        let next = metrics.max_offset_from_bottom.saturating_sub(offset);
                        if next != help.scroll {
                            help.scroll = next;
                            outcome.repaint = true;
                        }
                    }
                    return;
                }
                Some(
                    ClientChromeDrag::ProductAnnouncementScrollbar { .. }
                    | ClientChromeDrag::ReleaseNotesScrollbar { .. },
                ) => {
                    self.chrome_drag = None;
                    return;
                }
                Some(ClientChromeDrag::PaneScrollbar {
                    hit,
                    grab_row_offset,
                    last_sent_offset,
                    last_sent_at,
                }) => {
                    let current_hit = self
                        .hits
                        .panes
                        .iter()
                        .find(|current| current.pane_id == hit.pane_id)
                        .cloned()
                        .unwrap_or_else(|| hit.clone());
                    let Some(offset) = Self::pane_scrollbar_offset(
                        &current_hit,
                        mouse.row,
                        Some(*grab_row_offset),
                    ) else {
                        self.chrome_drag = None;
                        return;
                    };
                    let now = std::time::Instant::now();
                    let should_send = *last_sent_offset != Some(offset)
                        && last_sent_at.is_none_or(|last| {
                            now.duration_since(last) >= self.config.drag_throttle
                        });
                    if should_send {
                        if let Some(ClientChromeDrag::PaneScrollbar {
                            last_sent_offset,
                            last_sent_at,
                            ..
                        }) = self.chrome_drag.as_mut()
                        {
                            *last_sent_offset = Some(offset);
                            *last_sent_at = Some(now);
                        }
                        self.push_pane_scroll_offset(current_hit.pane_id, offset, outcome);
                    }
                    return;
                }
                Some(ClientChromeDrag::PaneSplit {
                    hit,
                    tab_id,
                    grab_offset,
                    last_sent_at,
                    ..
                }) => {
                    let hit = hit.clone();
                    let tab_id = tab_id.clone();
                    let grab_offset = *grab_offset;
                    match self.pane_split_target_is_current(&hit, &tab_id) {
                        Some(true) => {}
                        Some(false) => {
                            self.chrome_drag = None;
                            return;
                        }
                        None => return,
                    }
                    let ratio = Self::pane_split_ratio(&hit, grab_offset, point);
                    let now = std::time::Instant::now();
                    let should_send = last_sent_at
                        .is_none_or(|last| now.duration_since(last) >= self.config.drag_throttle);
                    if let Some(ClientChromeDrag::PaneSplit {
                        last_sent_ratio,
                        last_sent_at,
                        ..
                    }) = self.chrome_drag.as_mut()
                    {
                        if should_send {
                            *last_sent_ratio = Some(ratio);
                            *last_sent_at = Some(now);
                        }
                    }
                    if should_send {
                        self.push_endpoint_method(
                            crate::api::schema::Method::LayoutSetSplitRatio(
                                crate::api::schema::LayoutSetSplitRatioParams {
                                    tab_id: Some(tab_id),
                                    pane_id: None,
                                    path: hit.path,
                                    ratio,
                                },
                            ),
                            outcome,
                        );
                    }
                    return;
                }
                Some(ClientChromeDrag::Tab { .. }) => {
                    let insert_index = self.tab_drop_index_at(point);
                    if let Some(ClientChromeDrag::Tab {
                        insert_index: current,
                        ..
                    }) = self.chrome_drag.as_mut()
                    {
                        *current = insert_index;
                    }
                    outcome.repaint = true;
                    return;
                }
                Some(ClientChromeDrag::Workspace { .. }) => {
                    let target = self.workspace_drop_target_at(point);
                    if let Some(ClientChromeDrag::Workspace {
                        target: current, ..
                    }) = self.chrome_drag.as_mut()
                    {
                        *current = target;
                    }
                    outcome.repaint = true;
                    return;
                }
                None => {}
            }
            if let Some(press) = self.workspace_press.as_ref() {
                // 侧栏是纵向列表：只有纵向位移才可能是重排，横漂不算拖拽。
                let delta = mouse.row.abs_diff(press.start_row);
                if delta >= CHROME_DRAG_THRESHOLD_CELLS {
                    let source_workspace_id = press.workspace_id.clone();
                    let draggable = self.endpoint_workspace_is_draggable(press);
                    if draggable {
                        if let Some(target) = self.workspace_drop_target_at(point) {
                            // 落点就是源槽位时不建立拖拽：保留 `workspace_press`，
                            // 抬起时照旧是点击切换。
                            if self
                                .workspace_drop_reorders(&source_workspace_id, target.0.as_deref())
                            {
                                self.chrome_drag = Some(ClientChromeDrag::Workspace {
                                    source_workspace_id,
                                    target: Some(target),
                                });
                                outcome.repaint = true;
                            }
                        }
                    }
                }
                return;
            }
            if let Some(press) = self.tab_press.as_ref() {
                // 标签条是横向列表：只看横向位移。
                let delta = mouse.column.abs_diff(press.start_column);
                if delta >= CHROME_DRAG_THRESHOLD_CELLS {
                    let tab_id = press.tab_id.clone();
                    let workspace_id = press.workspace_id.clone();
                    if let Some(insert_index) = self.tab_drop_index_at(point) {
                        // 插到自己的位置不是重排（服务端同口径），保留点击语义。
                        if self.tab_drop_reorders(&tab_id, insert_index) {
                            self.chrome_drag = Some(ClientChromeDrag::Tab {
                                tab_id,
                                workspace_id,
                                insert_index: Some(insert_index),
                            });
                            outcome.repaint = true;
                        }
                    }
                }
                return;
            }
        }
        if mouse.kind == MouseEventKind::Up(MouseButton::Left) {
            if let Some(drag) = self.chrome_drag.take() {
                // 抬起时拖拽没有产生真实重排，就退化回原本的点击语义
                // （HERDR-UX-004）：一次按下抬起要么重排、要么切换，不能两头
                // 落空。press 在拖拽期间一直保留，这里正是它的用处。
                let workspace_press = self.workspace_press.take();
                let tab_press = self.tab_press.take();
                match drag {
                    ClientChromeDrag::Tab {
                        tab_id,
                        workspace_id,
                        ..
                    } => {
                        let insert_index = self.tab_drop_index_at(point);
                        let valid_drop = self.snapshot.as_deref().is_some_and(|snapshot| {
                            snapshot.focused_workspace_id.as_deref() == Some(workspace_id.as_str())
                                && snapshot.tabs.iter().any(|tab| {
                                    tab.tab_id == tab_id && tab.workspace_id == workspace_id
                                })
                                && insert_index.is_some_and(|index| {
                                    index
                                        <= snapshot
                                            .tabs
                                            .iter()
                                            .filter(|tab| tab.workspace_id == workspace_id)
                                            .count()
                                })
                        });
                        // 落在合法插入位但不改变顺序时退化为点击切换；完全没有
                        // 落点（拖出标签条）仍是「取消拖拽」，不产生任何动作。
                        if valid_drop {
                            let insert_index = insert_index.unwrap_or_default();
                            if self.tab_drop_reorders(&tab_id, insert_index) {
                                self.push_endpoint_method(
                                    crate::api::schema::Method::TabMove(
                                        crate::api::schema::TabMoveParams {
                                            tab_id,
                                            insert_index,
                                        },
                                    ),
                                    outcome,
                                );
                            } else if let Some(press) = tab_press {
                                // 鼠标点击标签是用户手势：连续点击只保留最新目标。
                                self.push_endpoint_method_coalescing(
                                    crate::api::schema::Method::TabFocus(
                                        crate::api::schema::TabTarget {
                                            tab_id: press.tab_id,
                                        },
                                    ),
                                    outcome,
                                );
                            }
                        }
                        outcome.repaint = true;
                    }
                    ClientChromeDrag::Workspace {
                        source_workspace_id,
                        target,
                    } => {
                        // 同上：落在槽位但不改变顺序 → 点击语义；拖出侧栏
                        // （`target` 为 None）→ 取消拖拽。
                        if let Some((before_workspace_id, _)) = target {
                            match self.workspace_move_method(
                                &source_workspace_id,
                                before_workspace_id.as_deref(),
                            ) {
                                Some(method) => self.push_endpoint_method(method, outcome),
                                None => {
                                    if let Some(press) = workspace_press {
                                        self.finish_endpoint_workspace_press(press, outcome);
                                    }
                                }
                            }
                        }
                        outcome.repaint = true;
                    }
                    ClientChromeDrag::PaneScrollbar {
                        hit,
                        grab_row_offset,
                        last_sent_offset,
                        ..
                    } => {
                        let current_hit = self
                            .hits
                            .panes
                            .iter()
                            .find(|current| current.pane_id == hit.pane_id)
                            .cloned()
                            .unwrap_or(hit);
                        if let Some(offset) = Self::pane_scrollbar_offset(
                            &current_hit,
                            mouse.row,
                            Some(grab_row_offset),
                        ) {
                            if last_sent_offset != Some(offset) {
                                self.push_pane_scroll_offset(current_hit.pane_id, offset, outcome);
                            }
                        }
                    }
                    ClientChromeDrag::PaneSplit {
                        hit,
                        tab_id,
                        grab_offset,
                        last_sent_ratio,
                        ..
                    } => {
                        let target_is_current =
                            self.pane_split_target_is_current(&hit, &tab_id) == Some(true);
                        let ratio = Self::pane_split_ratio(&hit, grab_offset, point);
                        if target_is_current
                            && last_sent_ratio
                                .is_none_or(|sent| (sent - ratio).abs() > f32::EPSILON)
                        {
                            self.push_endpoint_method(
                                crate::api::schema::Method::LayoutSetSplitRatio(
                                    crate::api::schema::LayoutSetSplitRatioParams {
                                        tab_id: Some(tab_id),
                                        pane_id: None,
                                        path: hit.path,
                                        ratio,
                                    },
                                ),
                                outcome,
                            );
                        }
                    }
                    ClientChromeDrag::SidebarWidth | ClientChromeDrag::SidebarSection => {
                        self.persist_chrome_preferences(outcome);
                    }
                    ClientChromeDrag::WorkspaceScrollbar { .. }
                    | ClientChromeDrag::AgentScrollbar { .. }
                    | ClientChromeDrag::HelpScrollbar { .. }
                    | ClientChromeDrag::NavigatorScrollbar { .. }
                    | ClientChromeDrag::ProductAnnouncementScrollbar { .. }
                    | ClientChromeDrag::ReleaseNotesScrollbar { .. } => {}
                }
                return;
            }
            if let Some(press) = self.workspace_press.take() {
                self.finish_endpoint_workspace_press(press, outcome);
                return;
            }
            if let Some(press) = self.tab_press.take() {
                // 鼠标点击标签是用户手势：连续点击只保留最新目标。
                self.push_endpoint_method_coalescing(
                    crate::api::schema::Method::TabFocus(crate::api::schema::TabTarget {
                        tab_id: press.tab_id,
                    }),
                    outcome,
                );
                return;
            }
        }
        if self.handle_command_palette_mouse(mouse, point, outcome) {
            return;
        }
        if self.handle_context_menu_mouse(mouse, point, outcome) {
            return;
        }
        if matches!(
            self.overlay,
            Some(
                ClientShellOverlay::WorktreeCreate(_)
                    | ClientShellOverlay::WorktreeOpen(_)
                    | ClientShellOverlay::WorktreeRemove(_)
            )
        ) {
            match mouse.kind {
                MouseEventKind::ScrollUp
                    if matches!(self.overlay, Some(ClientShellOverlay::WorktreeOpen(_))) =>
                {
                    self.move_worktree_open_selection(-1);
                    outcome.repaint = true;
                }
                MouseEventKind::ScrollDown
                    if matches!(self.overlay, Some(ClientShellOverlay::WorktreeOpen(_))) =>
                {
                    self.move_worktree_open_selection(1);
                    outcome.repaint = true;
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    if super::contains(self.hits.overlay_cancel, point) {
                        let busy =
                            matches!(
                                self.overlay,
                                Some(
                                    ClientShellOverlay::WorktreeCreate(
                                        ClientWorktreeCreateOverlay { creating: true, .. }
                                    ) | ClientShellOverlay::WorktreeOpen(
                                        ClientWorktreeOpenOverlay { opening: true, .. }
                                    ) | ClientShellOverlay::WorktreeRemove(
                                        ClientWorktreeRemoveOverlay { removing: true, .. }
                                    )
                                )
                            );
                        if !busy {
                            self.overlay = None;
                            outcome.repaint = true;
                        }
                    } else if super::contains(self.hits.worktree_search, point) {
                        if let Some(ClientShellOverlay::WorktreeOpen(open)) = self.overlay.as_mut()
                        {
                            open.search_focused = true;
                            outcome.repaint = true;
                        }
                    } else if let Some((_, index)) = self
                        .hits
                        .worktree_rows
                        .iter()
                        .find(|(rect, _)| super::contains(*rect, point))
                        .copied()
                    {
                        if let Some(ClientShellOverlay::WorktreeOpen(open)) = self.overlay.as_mut()
                        {
                            open.selected = index;
                        }
                        self.submit_worktree_open(outcome);
                    } else if super::contains(self.hits.overlay_primary, point) {
                        match self.overlay.as_ref() {
                            Some(ClientShellOverlay::WorktreeCreate(_)) => {
                                self.submit_worktree_create(outcome)
                            }
                            Some(ClientShellOverlay::WorktreeOpen(_)) => {
                                self.submit_worktree_open(outcome)
                            }
                            Some(ClientShellOverlay::WorktreeRemove(_)) => {
                                self.submit_worktree_remove(outcome)
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
            return;
        }
        if matches!(self.overlay, Some(ClientShellOverlay::Settings(_))) {
            if matches!(
                mouse.kind,
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
            ) {
                self.scroll_settings(if mouse.kind == MouseEventKind::ScrollUp {
                    -3
                } else {
                    3
                });
                outcome.repaint = true;
                return;
            }
            if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
                if let Some((_, section)) = self
                    .hits
                    .settings_tabs
                    .iter()
                    .find(|(rect, _)| super::contains(*rect, point))
                    .copied()
                {
                    self.select_settings_section(section, outcome);
                } else if let Some((_, index)) = self
                    .hits
                    .settings_choices
                    .iter()
                    .find(|(rect, _)| super::contains(*rect, point))
                    .copied()
                {
                    self.select_settings_choice(index);
                    outcome.repaint = true;
                } else if super::contains(self.hits.overlay_primary, point) {
                    self.apply_settings_choice(outcome);
                } else if super::contains(self.hits.overlay_cancel, point)
                    || !super::contains(self.hits.settings_popup, point)
                {
                    let installing = matches!(
                        self.overlay,
                        Some(ClientShellOverlay::Settings(ClientSettingsOverlay {
                            installing_integrations: true,
                            ..
                        }))
                    );
                    if !installing {
                        self.cancel_settings_overlay();
                        outcome.repaint = true;
                    }
                }
            }
            return;
        }
        if matches!(self.overlay, Some(ClientShellOverlay::Help(_))) {
            match mouse.kind {
                MouseEventKind::ScrollUp => {
                    if let Some(ClientShellOverlay::Help(help)) = self.overlay.as_mut() {
                        let next = help.scroll.saturating_sub(self.config.mouse_scroll_lines);
                        if next != help.scroll {
                            help.scroll = next;
                            outcome.repaint = true;
                        }
                    }
                }
                MouseEventKind::ScrollDown => {
                    if let Some(ClientShellOverlay::Help(help)) = self.overlay.as_mut() {
                        let next = help
                            .scroll
                            .saturating_add(self.config.mouse_scroll_lines)
                            .min(help.max_scroll);
                        if next != help.scroll {
                            help.scroll = next;
                            outcome.repaint = true;
                        }
                    }
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    if super::contains(self.hits.help_scrollbar, point) {
                        if let Some(metrics) = self.hits.help_scroll_metrics {
                            if let Some(grab_row_offset) = crate::ui::scrollbar_thumb_grab_offset(
                                metrics,
                                self.hits.help_scrollbar,
                                mouse.row,
                            ) {
                                self.chrome_drag =
                                    Some(ClientChromeDrag::HelpScrollbar { grab_row_offset });
                            } else {
                                let offset = crate::ui::scrollbar_offset_from_row(
                                    metrics,
                                    self.hits.help_scrollbar,
                                    mouse.row,
                                );
                                if let Some(ClientShellOverlay::Help(help)) = self.overlay.as_mut()
                                {
                                    help.scroll =
                                        metrics.max_offset_from_bottom.saturating_sub(offset);
                                    outcome.repaint = true;
                                }
                            }
                        }
                    } else if super::contains(self.hits.overlay_cancel, point) {
                        let search_focused = matches!(
                            self.overlay,
                            Some(ClientShellOverlay::Help(ClientHelpOverlay {
                                search_focused: true,
                                ..
                            }))
                        );
                        if search_focused {
                            if let Some(ClientShellOverlay::Help(help)) = self.overlay.as_mut() {
                                help.search_focused = false;
                                help.query.clear();
                                help.scroll = 0;
                            }
                        } else {
                            self.overlay = None;
                        }
                        outcome.repaint = true;
                    } else if !super::contains(self.hits.help_popup, point) {
                        self.overlay = None;
                        outcome.repaint = true;
                    }
                }
                _ => {}
            }
            return;
        }
        if matches!(self.overlay, Some(ClientShellOverlay::Navigator(_))) {
            let row_hit = self
                .hits
                .navigator_rows
                .iter()
                .find(|(rect, _)| super::contains(*rect, point))
                .cloned();
            match mouse.kind {
                MouseEventKind::Moved => {
                    // 指针只写 hover：Enter 会真的切走（跨端点还会激活端点
                    // 投影），键盘选中不能被「鼠标路过」改写；出界也要写 None
                    // 才不会留下残影（MENU-01 / UX-04）。
                    let hovered = row_hit.map(|(_, target)| target);
                    if let Some(ClientShellOverlay::Navigator(navigator)) = self.overlay.as_mut() {
                        if navigator.hovered != hovered {
                            navigator.hovered = hovered;
                            outcome.repaint = true;
                        }
                    }
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    if super::contains(self.hits.navigator_scrollbar, point) {
                        if let Some(metrics) = self.hits.navigator_scroll_metrics {
                            if let Some(grab_row_offset) = crate::ui::scrollbar_thumb_grab_offset(
                                metrics,
                                self.hits.navigator_scrollbar,
                                mouse.row,
                            ) {
                                self.chrome_drag =
                                    Some(ClientChromeDrag::NavigatorScrollbar { grab_row_offset });
                            } else {
                                let offset = crate::ui::scrollbar_offset_from_row(
                                    metrics,
                                    self.hits.navigator_scrollbar,
                                    mouse.row,
                                );
                                self.scroll_navigator_to(
                                    metrics.max_offset_from_bottom.saturating_sub(offset),
                                    metrics.viewport_rows,
                                );
                                outcome.repaint = true;
                            }
                        }
                    } else if super::contains(self.hits.navigator_search, point) {
                        if let Some(ClientShellOverlay::Navigator(navigator)) =
                            self.overlay.as_mut()
                        {
                            navigator.search_focused = true;
                            navigator.filter = None;
                        }
                        outcome.repaint = true;
                    } else if let Some((_, target)) = row_hit {
                        if let Some(ClientShellOverlay::Navigator(navigator)) =
                            self.overlay.as_mut()
                        {
                            navigator.selected = Some(target);
                        }
                        self.accept_navigator_selection(outcome);
                    } else if !super::contains(self.hits.navigator_popup, point) {
                        self.overlay = None;
                        outcome.repaint = true;
                    }
                }
                MouseEventKind::ScrollUp => {
                    self.move_navigator_selection(-(self.config.mouse_scroll_lines as isize));
                    outcome.repaint = true;
                }
                MouseEventKind::ScrollDown => {
                    self.move_navigator_selection(self.config.mouse_scroll_lines as isize);
                    outcome.repaint = true;
                }
                _ => {}
            }
            return;
        }
        if matches!(self.overlay, Some(ClientShellOverlay::MachineAuth(_))) {
            if matches!(
                mouse.kind,
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
            ) {
                self.scroll_machine_auth(if mouse.kind == MouseEventKind::ScrollUp {
                    -3
                } else {
                    3
                });
                outcome.repaint = true;
                return;
            }
            if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
                if let Some((_, button)) = self
                    .hits
                    .machine_auth_actions
                    .iter()
                    .find(|(rect, _)| super::contains(*rect, point))
                    .copied()
                {
                    self.activate_machine_auth_button(button, outcome);
                }
                // Clicks outside the dialog are intentionally ignored:
                // auth recovery stays modal until an explicit choice.
            }
            return;
        }
        if self.handle_machines_mouse(mouse, point, outcome) {
            return;
        }
        if matches!(self.overlay, Some(ClientShellOverlay::Snippets(_))) {
            match mouse.kind {
                MouseEventKind::Moved => {
                    // 悬浮只写 hover，不改键盘选中（MENU-01）。
                    let hovered = self
                        .hits
                        .snippet_rows
                        .iter()
                        .find(|(rect, _)| super::contains(*rect, point))
                        .map(|(_, index)| *index);
                    outcome.repaint |= self.hover_snippet_row(hovered);
                }
                MouseEventKind::ScrollUp if super::contains(self.hits.snippet_popup, point) => {
                    // List/History 视图下滚轮走的是 `move_snippet_selection`：
                    // 视口由 `selected` 反推，选中被移出窗口时视口跟着滚，指针
                    // 下面的行随之改变，旧的 hover 行号立刻失效。
                    self.scroll_snippets_overlay(-(self.config.mouse_scroll_lines as isize));
                    self.hover_snippet_row(None);
                    outcome.repaint = true;
                }
                MouseEventKind::ScrollDown if super::contains(self.hits.snippet_popup, point) => {
                    self.scroll_snippets_overlay(self.config.mouse_scroll_lines as isize);
                    self.hover_snippet_row(None);
                    outcome.repaint = true;
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    if super::contains(self.hits.snippet_search, point) {
                        if let Some(ClientShellOverlay::Snippets(overlay)) = self.overlay.as_mut() {
                            overlay.search_focused = true;
                            overlay.message = None;
                        }
                        outcome.repaint = true;
                    } else if let Some((_, button)) = self
                        .hits
                        .snippet_actions
                        .iter()
                        .find(|(rect, _)| super::contains(*rect, point))
                        .copied()
                    {
                        self.activate_snippet_button(button, outcome);
                    } else if let Some((_, field)) = self
                        .hits
                        .snippet_fields
                        .iter()
                        .find(|(rect, _)| super::contains(*rect, point))
                        .copied()
                    {
                        if let Some(ClientShellOverlay::Snippets(overlay)) = self.overlay.as_mut() {
                            match &mut overlay.view {
                                super::snippets_overlay::ClientSnippetsView::Form(form) => {
                                    form.focused = field;
                                }
                                super::snippets_overlay::ClientSnippetsView::RunVariables(
                                    draft,
                                ) => {
                                    draft.variable_focused = field;
                                }
                                _ => {}
                            }
                        }
                        outcome.repaint = true;
                    } else if let Some((_, row)) = self
                        .hits
                        .snippet_rows
                        .iter()
                        .find(|(rect, _)| super::contains(*rect, point))
                        .copied()
                    {
                        // The confirm view's single row is the press-enter toggle.
                        let is_confirm_toggle = matches!(
                            self.overlay,
                            Some(ClientShellOverlay::Snippets(
                                super::snippets_overlay::ClientSnippetsOverlay {
                                    view: super::snippets_overlay::ClientSnippetsView::RunConfirm(
                                        _
                                    ),
                                    ..
                                }
                            ))
                        );
                        if is_confirm_toggle {
                            if let Some(ClientShellOverlay::Snippets(overlay)) =
                                self.overlay.as_mut()
                            {
                                if let super::snippets_overlay::ClientSnippetsView::RunConfirm(
                                    draft,
                                ) = &mut overlay.view
                                {
                                    draft.press_enter = !draft.press_enter;
                                }
                            }
                            outcome.repaint = true;
                        } else {
                            self.click_snippet_row(row, outcome);
                        }
                    } else if !super::contains(self.hits.snippet_popup, point) {
                        self.overlay = None;
                        outcome.repaint = true;
                    }
                }
                _ => {}
            }
            return;
        }
        if matches!(self.overlay, Some(ClientShellOverlay::Scenes(_))) {
            let list_view = matches!(
                self.overlay,
                Some(ClientShellOverlay::Scenes(
                    super::scenes_overlay::ClientScenesOverlay {
                        view: super::scenes_overlay::ClientScenesView::List,
                        ..
                    }
                ))
            );
            match mouse.kind {
                MouseEventKind::Moved => {
                    // 悬浮只写 hover：恢复现场会写端点目录，键盘选中必须只由
                    // 键盘与点击决定（MENU-01 / TOOL-02）。
                    let hovered = self
                        .hits
                        .scenes_rows
                        .iter()
                        .find(|(rect, _)| super::contains(*rect, point))
                        .map(|(_, index)| *index);
                    outcome.repaint |= self.set_scenes_hover(hovered);
                }
                // 滚轮只滚视口，不改键盘选中（C-20 残留面）：滚完之后 Enter
                // 作用在原来的选中行上，而不是「滚到的那一行」。视口滚动会换
                // 行，指针下面的行随之改变，所以顺带清 hover。
                MouseEventKind::ScrollUp if super::contains(self.hits.scenes_popup, point) => {
                    self.scroll_scenes_list(-(self.config.mouse_scroll_lines as isize));
                    self.set_scenes_hover(None);
                    outcome.repaint = true;
                }
                MouseEventKind::ScrollDown if super::contains(self.hits.scenes_popup, point) => {
                    self.scroll_scenes_list(self.config.mouse_scroll_lines as isize);
                    self.set_scenes_hover(None);
                    outcome.repaint = true;
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    if super::contains(self.hits.overlay_primary, point) && !list_view {
                        self.activate_scene_primary(outcome);
                    } else if super::contains(self.hits.overlay_cancel, point) && !list_view {
                        self.scenes_back();
                        outcome.repaint = true;
                    } else if list_view {
                        if let Some((_, button)) = self
                            .hits
                            .scenes_actions
                            .iter()
                            .find(|(rect, _)| super::contains(*rect, point))
                            .copied()
                        {
                            self.activate_scene_button(button, outcome);
                        } else if let Some((_, index)) = self
                            .hits
                            .scenes_rows
                            .iter()
                            .find(|(rect, _)| super::contains(*rect, point))
                            .copied()
                        {
                            // 单击只选中（TOOL-02）：恢复会写端点目录并断开
                            // 现场之外的 live SSH 连接，必须走「恢复」按钮 /
                            // Enter / 同一条现场的二次点击。
                            self.set_scenes_selection(index);
                            if self.scenes_row_click_is_second() {
                                self.restore_selected_scene(outcome);
                            }
                            outcome.repaint = true;
                        } else if !super::contains(self.hits.scenes_popup, point) {
                            self.overlay = None;
                            outcome.repaint = true;
                        }
                    } else if let Some((_, field)) = self
                        .hits
                        .scenes_fields
                        .iter()
                        .find(|(rect, _)| super::contains(*rect, point))
                        .copied()
                    {
                        self.focus_scene_form_field(field);
                        outcome.repaint = true;
                    } else if !super::contains(self.hits.scenes_popup, point) {
                        // Form views treat an outside click like Esc: back to
                        // the list rather than dropping the overlay.
                        self.scenes_back();
                        outcome.repaint = true;
                    }
                }
                _ => {}
            }
            return;
        }
        if matches!(self.overlay, Some(ClientShellOverlay::MachineFiles(_))) {
            match mouse.kind {
                MouseEventKind::ScrollUp
                    if super::contains(self.hits.machine_files_popup, point) =>
                {
                    self.scroll_machine_files_overlay(-(self.config.mouse_scroll_lines as isize));
                    outcome.repaint = true;
                }
                MouseEventKind::ScrollDown
                    if super::contains(self.hits.machine_files_popup, point) =>
                {
                    self.scroll_machine_files_overlay(self.config.mouse_scroll_lines as isize);
                    outcome.repaint = true;
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    if super::contains(self.hits.machine_files_search, point) {
                        if let Some(ClientShellOverlay::MachineFiles(overlay)) =
                            self.overlay.as_mut()
                        {
                            overlay.search_focused = true;
                            overlay.message = None;
                        }
                        outcome.repaint = true;
                    } else if let Some((_, button)) = self
                        .hits
                        .machine_files_actions
                        .iter()
                        .find(|(rect, _)| super::contains(*rect, point))
                        .copied()
                    {
                        self.activate_machine_files_button(button, outcome);
                    } else if let Some((_, row)) = self
                        .hits
                        .machine_files_rows
                        .iter()
                        .find(|(rect, _)| super::contains(*rect, point))
                        .copied()
                    {
                        self.click_machine_files_row(row, outcome);
                    } else if !super::contains(self.hits.machine_files_popup, point) {
                        self.overlay = None;
                        outcome.repaint = true;
                    }
                }
                _ => {}
            }
            return;
        }
        if self.handle_agent_activity_mouse(mouse, point, outcome) {
            return;
        }
        if matches!(self.overlay, Some(ClientShellOverlay::Broadcast(_))) {
            match mouse.kind {
                MouseEventKind::ScrollUp if super::contains(self.hits.broadcast_popup, point) => {
                    self.scroll_broadcast_overlay(-(self.config.mouse_scroll_lines as isize));
                    outcome.repaint = true;
                }
                MouseEventKind::ScrollDown if super::contains(self.hits.broadcast_popup, point) => {
                    self.scroll_broadcast_overlay(self.config.mouse_scroll_lines as isize);
                    outcome.repaint = true;
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    if let Some((_, button)) = self
                        .hits
                        .broadcast_actions
                        .iter()
                        .find(|(rect, _)| super::contains(*rect, point))
                        .copied()
                    {
                        self.activate_broadcast_button(button, outcome);
                    } else if let Some((_, row)) = self
                        .hits
                        .broadcast_rows
                        .iter()
                        .find(|(rect, _)| super::contains(*rect, point))
                        .copied()
                    {
                        self.click_broadcast_row(row, outcome);
                    } else if !super::contains(self.hits.broadcast_popup, point) {
                        self.overlay = None;
                        outcome.repaint = true;
                    }
                }
                _ => {}
            }
            return;
        }
        if matches!(
            self.overlay,
            Some(ClientShellOverlay::NotificationHistory(_))
        ) {
            let row_hit = self
                .hits
                .notification_history_rows
                .iter()
                .find(|(rect, _)| super::contains(*rect, point))
                .copied();
            match mouse.kind {
                MouseEventKind::Moved => {
                    // 指针只写 hover：Enter 会跳到源 pane，键盘选中不能被
                    // 「鼠标路过」改写；出界也要写 None（MENU-01 / UX-04）。
                    outcome.repaint |=
                        self.set_notification_history_hover(row_hit.map(|(_, index)| index));
                }
                MouseEventKind::ScrollUp => {
                    self.move_notification_history_selection(-3);
                    outcome.repaint = true;
                }
                MouseEventKind::ScrollDown => {
                    self.move_notification_history_selection(3);
                    outcome.repaint = true;
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    if let Some((_, index)) = row_hit {
                        self.set_notification_history_selection(index);
                        self.focus_notification_history_target(outcome);
                    } else {
                        self.overlay = None;
                        outcome.repaint = true;
                    }
                }
                _ => {}
            }
            return;
        }
        if self.overlay.is_some() {
            if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
                return;
            }
            if super::contains(self.hits.overlay_primary, point) {
                match self.overlay.as_ref() {
                    Some(ClientShellOverlay::Rename(_)) => self.save_rename_overlay(outcome),
                    Some(ClientShellOverlay::ConfirmClose(_)) => {
                        self.accept_close_confirmation(outcome);
                    }
                    _ => {}
                }
            } else if super::contains(self.hits.overlay_clear, point) {
                if let Some(ClientShellOverlay::Rename(rename)) = self.overlay.as_mut() {
                    rename.input.clear();
                    outcome.repaint = true;
                }
            } else {
                self.overlay = None;
                outcome.repaint = true;
            }
            return;
        }

        if mouse.kind == MouseEventKind::Drag(MouseButton::Left) {
            let selection_hit = self.active_selection_pane();
            if let Some(hit) = selection_hit {
                self.update_selection_drag(&hit, mouse.column, mouse.row, outcome);
                // Consume every motion, but do not rebuild a frame for every intermediate position.
                outcome.repaint |= !outcome.actions.is_empty()
                    || self.request_selection_drag_repaint(std::time::Instant::now());
                return;
            }
        }
        if mouse.kind == MouseEventKind::Up(MouseButton::Left)
            && self.word_selection_gesture.is_some()
        {
            self.finish_word_selection(outcome);
            outcome.repaint = true;
            return;
        }
        if mouse.kind == MouseEventKind::Up(MouseButton::Left) && self.selection.is_some() {
            self.stop_selection_autoscroll();
            let copied = self
                .selection
                .as_mut()
                .is_some_and(crate::selection::Selection::finish);
            if copied && self.config.copy_on_select {
                self.request_selection_copy(outcome, true);
                self.selection = None;
            } else if self
                .selection
                .as_ref()
                .is_some_and(crate::selection::Selection::is_just_click)
            {
                self.selection = None;
            }
            if copied {
                self.last_pane_click = None;
            }
            outcome.repaint = true;
            return;
        }
        if self.scroll_in_progress_selection(mouse, outcome) {
            return;
        }

        match mouse.kind {
            MouseEventKind::Down(MouseButton::Right) => {
                let pane_hit = self
                    .hits
                    .panes
                    .iter()
                    .find(|hit| super::contains(hit.inner_rect, point))
                    .cloned();
                if let Some(hit) = pane_hit {
                    if let Some(stripped_modifiers) = self.pane_right_click_modifiers(&hit, mouse) {
                        self.push_pane_mouse_event(
                            &hit,
                            mouse,
                            mouse.modifiers.difference(stripped_modifiers),
                            outcome,
                        );
                        self.push_endpoint_method(
                            crate::api::schema::Method::PaneFocus(crate::api::schema::PaneTarget {
                                pane_id: hit.pane_id.clone(),
                            }),
                            outcome,
                        );
                        self.pane_mouse_gesture = Some(ClientPaneMouseGesture {
                            last_position: self.pane_mouse_position(&hit, mouse),
                            hit,
                            button: MouseButton::Right,
                            stripped_modifiers,
                            last_event: mouse,
                        });
                        return;
                    }
                }
                if !self.config.mouse_capture {
                    return;
                }
                if self.open_agent_context_menu_at(point) {
                    outcome.repaint = true;
                    return;
                }
                let workspace_id = (!self.sidebar_collapsed)
                    .then(|| self.active_endpoint_workspace_at(point))
                    .flatten();
                if let Some(workspace_id) = workspace_id {
                    self.open_workspace_context_menu(workspace_id, mouse.column, mouse.row);
                    outcome.repaint = true;
                    return;
                }
                let machine_endpoint = self
                    .hits
                    .machines
                    .iter()
                    .find(|hit| super::contains(hit.rect, point))
                    .map(|hit| hit.endpoint_id.clone());
                if let Some(endpoint_id) = machine_endpoint {
                    self.open_machine_context_menu(&endpoint_id, mouse.column, mouse.row);
                    outcome.repaint = true;
                    return;
                }
                let tab_id = self
                    .hits
                    .tabs
                    .iter()
                    .find(|(rect, _)| super::contains(*rect, point))
                    .map(|(_, tab_id)| tab_id.clone());
                if let Some(tab_id) = tab_id {
                    self.open_tab_context_menu(tab_id, mouse.column, mouse.row);
                    outcome.repaint = true;
                    return;
                }
                let pane_id = self
                    .hits
                    .panes
                    .iter()
                    .find(|hit| super::contains(hit.rect, point))
                    .map(|hit| hit.pane_id.clone());
                if let Some(pane_id) = pane_id {
                    self.open_pane_context_menu(pane_id, mouse.column, mouse.row);
                    outcome.repaint = true;
                }
            }
            MouseEventKind::ScrollUp
                if self
                    .hits
                    .tabs
                    .iter()
                    .any(|(rect, _)| super::contains(*rect, point))
                    || super::contains(self.hits.tab_scroll_left, point)
                    || super::contains(self.hits.tab_scroll_right, point)
                    || super::contains(self.hits.new_tab, point) =>
            {
                self.record_binding(
                    crate::input::KeybindMatch::Action(crate::input::KeybindAction::PreviousTab),
                    outcome,
                );
            }
            MouseEventKind::ScrollDown
                if self
                    .hits
                    .tabs
                    .iter()
                    .any(|(rect, _)| super::contains(*rect, point))
                    || super::contains(self.hits.tab_scroll_left, point)
                    || super::contains(self.hits.tab_scroll_right, point)
                    || super::contains(self.hits.new_tab, point) =>
            {
                self.record_binding(
                    crate::input::KeybindMatch::Action(crate::input::KeybindAction::NextTab),
                    outcome,
                );
            }
            MouseEventKind::ScrollUp if super::contains(self.hits.agent_body, point) => {
                // 钳位写回（D10）：多机折叠侧栏的渲染只读、不回写越界的滚动位置
                // （例如从展开视图带过来的），先按上一帧的上界夹住再滚，反向第一格
                // 画面就动。
                let next = self
                    .agent_scroll
                    .min(self.hits.agent_max_scroll)
                    .saturating_sub(self.config.mouse_scroll_lines);
                if next != self.agent_scroll {
                    self.agent_scroll = next;
                    outcome.repaint = true;
                }
            }
            MouseEventKind::ScrollDown if super::contains(self.hits.agent_body, point) => {
                let next = self
                    .agent_scroll
                    .saturating_add(self.config.mouse_scroll_lines)
                    .min(self.hits.agent_max_scroll);
                if next != self.agent_scroll {
                    self.agent_scroll = next;
                    outcome.repaint = true;
                }
            }
            MouseEventKind::ScrollUp if super::contains(self.hits.workspace_body, point) => {
                let next = self
                    .workspace_scroll
                    .saturating_sub(self.config.mouse_scroll_lines);
                if next != self.workspace_scroll {
                    self.workspace_scroll = next;
                    outcome.repaint = true;
                }
            }
            MouseEventKind::ScrollDown if super::contains(self.hits.workspace_body, point) => {
                let next = self
                    .workspace_scroll
                    .saturating_add(self.config.mouse_scroll_lines)
                    .min(self.hits.workspace_max_scroll);
                if next != self.workspace_scroll {
                    self.workspace_scroll = next;
                    outcome.repaint = true;
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if self.selection.take().is_some() {
                    outcome.repaint = true;
                }
                self.stop_selection_autoscroll();
                self.selection_highlight_clear_deadline = None;
                self.word_selection_gesture = None;
                let previous_pane_click = self.last_pane_click.take();
                self.workspace_press = None;
                self.tab_press = None;
                self.chrome_drag = None;
                if super::contains(self.hits.sidebar_divider, point)
                    && !super::contains(self.hits.sidebar_toggle, point)
                {
                    let now = std::time::Instant::now();
                    let double_click = self.last_sidebar_divider_click.is_some_and(|last| {
                        now.duration_since(last) <= self.config.double_click_window
                    });
                    self.last_sidebar_divider_click = Some(now);
                    if double_click {
                        self.sidebar_width = self.config.sidebar_width;
                        self.sidebar_width_manual = false;
                        self.invalidate_pane_surface();
                        outcome.repaint = true;
                        outcome.resize = true;
                        self.persist_chrome_preferences(outcome);
                    } else {
                        self.chrome_drag = Some(ClientChromeDrag::SidebarWidth);
                        self.set_sidebar_width_from_column(mouse.column, outcome);
                    }
                    return;
                }
                if super::contains(self.hits.sidebar_section_divider, point) {
                    self.chrome_drag = Some(ClientChromeDrag::SidebarSection);
                    self.set_sidebar_section_from_row(mouse.row, outcome);
                    return;
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
                            let next = metrics.max_offset_from_bottom.saturating_sub(offset);
                            if next != self.workspace_scroll {
                                self.workspace_scroll = next;
                                outcome.repaint = true;
                            }
                        }
                    }
                    return;
                }
                if super::contains(self.hits.agent_scrollbar, point) {
                    if let Some(metrics) = self.hits.agent_scroll_metrics {
                        if let Some(grab_row_offset) = crate::ui::scrollbar_thumb_grab_offset(
                            metrics,
                            self.hits.agent_scrollbar,
                            mouse.row,
                        ) {
                            self.chrome_drag =
                                Some(ClientChromeDrag::AgentScrollbar { grab_row_offset });
                        } else {
                            let offset = crate::ui::scrollbar_offset_from_row(
                                metrics,
                                self.hits.agent_scrollbar,
                                mouse.row,
                            );
                            let next = metrics.max_offset_from_bottom.saturating_sub(offset);
                            if next != self.agent_scroll {
                                self.agent_scroll = next;
                                outcome.repaint = true;
                            }
                        }
                    }
                    return;
                }
                if super::contains(self.hits.agent_sort_toggle, point) {
                    self.config.agent_panel_sort = self.config.agent_panel_sort.next();
                    self.agent_panel_sort_manual = true;
                    self.agent_scroll = 0;
                    self.persist_chrome_preferences(outcome);
                    outcome.repaint = true;
                    return;
                }
                if self.handle_endpoint_machine_click(point, outcome) {
                    return;
                }
                if super::contains(self.hits.global_launcher, point) {
                    self.toggle_global_menu();
                    outcome.repaint = true;
                    return;
                }
                if super::contains(self.hits.new_workspace, point) {
                    self.record_binding(
                        crate::input::KeybindMatch::Action(
                            crate::input::KeybindAction::NewWorkspace,
                        ),
                        outcome,
                    );
                    return;
                }
                if super::contains(self.hits.new_tab, point) {
                    self.record_binding(
                        crate::input::KeybindMatch::Action(crate::input::KeybindAction::NewTab),
                        outcome,
                    );
                    return;
                }
                if super::contains(self.hits.tab_scroll_left, point) {
                    self.tab_scroll = self.tab_scroll.saturating_sub(1);
                    outcome.repaint = true;
                    return;
                }
                if super::contains(self.hits.tab_scroll_right, point) {
                    let tab_count = self
                        .snapshot
                        .as_deref()
                        .and_then(|snapshot| {
                            snapshot.focused_workspace_id.as_deref().map(|id| {
                                snapshot
                                    .tabs
                                    .iter()
                                    .filter(|tab| tab.workspace_id == id)
                                    .count()
                            })
                        })
                        .unwrap_or(0);
                    self.tab_scroll = self
                        .tab_scroll
                        .saturating_add(1)
                        .min(tab_count.saturating_sub(1));
                    outcome.repaint = true;
                    return;
                }
                if super::contains(self.hits.sidebar_toggle, point) {
                    self.sidebar_collapsed = !self.sidebar_collapsed;
                    self.sidebar_collapsed_manual = true;
                    // 与键盘 `ToggleSidebar` 一致：折叠态与展开态的 `workspace_scroll`
                    // 上限不同，切换后按聚焦行重新定位，而不是沿用被钳过的下标。
                    self.reveal_focused_workspace = true;
                    self.invalidate_pane_surface();
                    outcome.repaint = true;
                    outcome.resize = true;
                    self.persist_chrome_preferences(outcome);
                    return;
                }
                let group_toggle = self.hits.workspaces.iter().find_map(|hit| {
                    let (rect, key) = hit.group_toggle.as_ref()?;
                    super::contains(*rect, point).then(|| (hit.endpoint_id.clone(), key.clone()))
                });
                if let Some((endpoint_id, key)) = group_toggle {
                    self.toggle_collapsed_group(&endpoint_id, key);
                    outcome.repaint = true;
                    self.persist_chrome_preferences(outcome);
                    return;
                }
                let workspace_press = self
                    .hits
                    .workspaces
                    .iter()
                    .find(|hit| super::contains(hit.rect, point))
                    .map(|hit| ClientWorkspacePress {
                        endpoint_id: hit.endpoint_id.clone(),
                        workspace_id: hit.workspace_id.clone(),
                        start_row: mouse.row,
                    });
                if let Some(workspace_press) = workspace_press {
                    self.workspace_press = Some(workspace_press);
                    return;
                }
                let tab_press = self
                    .config
                    .mouse_capture
                    .then(|| {
                        self.hits
                            .tabs
                            .iter()
                            .find(|(rect, _)| super::contains(*rect, point))
                            .and_then(|(_, tab_id)| {
                                let tab = self
                                    .snapshot
                                    .as_deref()?
                                    .tabs
                                    .iter()
                                    .find(|tab| tab.tab_id == *tab_id)?;
                                Some(ClientTabPress {
                                    tab_id: tab.tab_id.clone(),
                                    workspace_id: tab.workspace_id.clone(),
                                    start_column: mouse.column,
                                })
                            })
                    })
                    .flatten();
                if let Some(tab_press) = tab_press {
                    self.tab_press = Some(tab_press);
                    return;
                }
                // 树节点的折叠开关落在行矩形之内：必须先于行点击。
                if self.handle_agent_tree_click(point, outcome) {
                    return;
                }
                if self.handle_endpoint_agent_click(point, outcome) {
                    return;
                }
                if let Some((_, key)) = self
                    .hits
                    .agent_group_toggles
                    .iter()
                    .find(|(rect, _)| super::contains(*rect, point))
                {
                    let endpoint_id = self.active_endpoint_id.clone();
                    let key = key.clone();
                    self.toggle_collapsed_group(&endpoint_id, key);
                    self.agent_scroll = 0;
                    outcome.repaint = true;
                    self.persist_chrome_preferences(outcome);
                    return;
                }
                let agent_pane_id = self
                    .hits
                    .agents
                    .iter()
                    .find(|(rect, _)| super::contains(*rect, point))
                    .map(|(_, pane_id)| pane_id.clone());
                if let Some(pane_id) = agent_pane_id {
                    self.push_endpoint_method(
                        crate::api::schema::Method::PaneFocus(crate::api::schema::PaneTarget {
                            pane_id,
                        }),
                        outcome,
                    );
                    return;
                }
                let scrollbar_hit = self
                    .hits
                    .panes
                    .iter()
                    .find(|hit| {
                        hit.scrollbar_rect
                            .is_some_and(|rect| super::contains(rect, point))
                            && hit
                                .scroll
                                .is_some_and(|metrics| metrics.max_offset_from_bottom > 0)
                    })
                    .cloned();
                if let Some(hit) = scrollbar_hit {
                    self.mode = ClientShellMode::Terminal;
                    self.push_endpoint_method(
                        crate::api::schema::Method::PaneFocus(crate::api::schema::PaneTarget {
                            pane_id: hit.pane_id.clone(),
                        }),
                        outcome,
                    );
                    let (Some(track), Some(metrics)) = (hit.scrollbar_rect, hit.scroll) else {
                        return;
                    };
                    if let Some(grab_row_offset) =
                        crate::ui::scrollbar_thumb_grab_offset(metrics, track, mouse.row)
                    {
                        self.chrome_drag = Some(ClientChromeDrag::PaneScrollbar {
                            hit,
                            grab_row_offset,
                            last_sent_offset: None,
                            last_sent_at: None,
                        });
                    } else if let Some(offset) = Self::pane_scrollbar_offset(&hit, mouse.row, None)
                    {
                        self.push_pane_scroll_offset(hit.pane_id, offset, outcome);
                    }
                    return;
                }
                let split_hit = self
                    .hits
                    .pane_splits
                    .iter()
                    .find(|hit| super::contains(hit.hit_rect, point))
                    .cloned();
                if let Some(hit) = split_hit {
                    let Some(tab_id) = hit.tab_id.clone() else {
                        return;
                    };
                    let pointer = match hit.direction {
                        crate::protocol::PaneSurfaceSplitDirection::Horizontal => mouse.column,
                        crate::protocol::PaneSurfaceSplitDirection::Vertical => mouse.row,
                    };
                    self.chrome_drag = Some(ClientChromeDrag::PaneSplit {
                        grab_offset: i32::from(hit.pos) - i32::from(pointer),
                        last_sent_ratio: None,
                        last_sent_at: None,
                        hit,
                        tab_id,
                    });
                    return;
                }
                let pane_hit = self
                    .hits
                    .panes
                    .iter()
                    .find(|hit| super::contains(hit.rect, point))
                    .cloned();
                if let Some(hit) = pane_hit {
                    if hit.mouse_reporting && super::contains(hit.inner_rect, point) {
                        if !self.begin_codex_selection_press(&hit, mouse) {
                            self.push_pane_mouse_event(&hit, mouse, mouse.modifiers, outcome);
                            self.pane_mouse_gesture = Some(ClientPaneMouseGesture {
                                last_position: self.pane_mouse_position(&hit, mouse),
                                hit: hit.clone(),
                                button: MouseButton::Left,
                                stripped_modifiers: crossterm::event::KeyModifiers::empty(),
                                last_event: mouse,
                            });
                        }
                    } else if super::contains(hit.inner_rect, point) {
                        let click = ClientPaneClick {
                            pane_id: hit.pane_id.clone(),
                            viewport_row: mouse.row.saturating_sub(hit.inner_rect.y),
                            col: mouse.column.saturating_sub(hit.inner_rect.x),
                            at: std::time::Instant::now(),
                            streak: 1,
                        };
                        let streak = if mouse.modifiers.is_empty() {
                            previous_pane_click
                                .as_ref()
                                .filter(|previous| {
                                    previous
                                        .continues_streak(&click, self.config.double_click_window)
                                })
                                .map_or(1, |previous| previous.streak.saturating_add(1).min(3))
                        } else {
                            1
                        };
                        let click = ClientPaneClick { streak, ..click };
                        if self.begin_frozen_selection(&hit, mouse, streak, outcome) {
                            self.last_pane_click = (streak < 3).then_some(click);
                        } else if streak >= 3 {
                            // Triple-click selects the whole line and restarts
                            // the streak so a fourth click is a fresh anchor.
                            self.last_pane_click = None;
                            self.select_line_at(&hit, click.viewport_row, outcome);
                        } else if streak == 2 {
                            // Keep the streak alive so a third click can escalate.
                            self.last_pane_click = Some(click.clone());
                            self.request_word_selection(
                                &hit,
                                click.viewport_row,
                                click.col,
                                outcome,
                            );
                        } else {
                            if mouse.modifiers.is_empty() {
                                self.last_pane_click = Some(click);
                            }
                            self.selection = Some(crate::selection::Selection::anchor(
                                hit.pane_id.clone(),
                                mouse.row.saturating_sub(hit.inner_rect.y),
                                mouse.column.saturating_sub(hit.inner_rect.x),
                                hit.scroll,
                            ));
                        }
                    }
                    self.push_endpoint_method(
                        crate::api::schema::Method::PaneFocus(crate::api::schema::PaneTarget {
                            pane_id: hit.pane_id,
                        }),
                        outcome,
                    );
                }
            }
            MouseEventKind::Down(MouseButton::Middle) => {
                if let Some(hit) = self
                    .hits
                    .panes
                    .iter()
                    .find(|hit| super::contains(hit.inner_rect, point) && hit.mouse_reporting)
                    .cloned()
                {
                    self.push_pane_mouse_event(&hit, mouse, mouse.modifiers, outcome);
                    self.pane_mouse_gesture = Some(ClientPaneMouseGesture {
                        last_position: self.pane_mouse_position(&hit, mouse),
                        hit,
                        button: MouseButton::Middle,
                        stripped_modifiers: crossterm::event::KeyModifiers::empty(),
                        last_event: mouse,
                    });
                }
            }
            MouseEventKind::Up(MouseButton::Left | MouseButton::Middle)
            | MouseEventKind::Drag(MouseButton::Left | MouseButton::Middle) => {}
            MouseEventKind::Moved => {
                // 借引用即可（`push_pane_mouse_event` 只读 self）：原来每移动一次
                // 白克隆一个 `PaneHit`（HERDR-PERF-009）。
                if let Some(hit) = self
                    .hits
                    .panes
                    .iter()
                    .find(|hit| super::contains(hit.inner_rect, point) && hit.mouse_reporting)
                {
                    // 同一格的重复移动不转发：像素模式下同一格会被反复上报，
                    // 应用只关心格子/像素坐标变化（HERDR-PERF-012）。
                    let position = (
                        hit.pane_id.clone(),
                        mouse.column,
                        mouse.row,
                        mouse.modifiers,
                    );
                    if self.last_pane_move.as_ref() != Some(&position) {
                        self.last_pane_move = Some(position);
                        self.push_pane_mouse_event(hit, mouse, mouse.modifiers, outcome);
                    }
                } else {
                    self.last_pane_move = None;
                }
            }
            MouseEventKind::ScrollUp
            | MouseEventKind::ScrollDown
            | MouseEventKind::ScrollLeft
            | MouseEventKind::ScrollRight => {
                if let Some(hit) = self
                    .hits
                    .panes
                    .iter()
                    .find(|hit| super::contains(hit.inner_rect, point))
                    .cloned()
                {
                    if self.focused_pane_id().as_deref() != Some(hit.pane_id.as_str()) {
                        self.push_endpoint_method(
                            crate::api::schema::Method::PaneFocus(crate::api::schema::PaneTarget {
                                pane_id: hit.pane_id.clone(),
                            }),
                            outcome,
                        );
                    }
                    self.push_pane_mouse_event(&hit, mouse, mouse.modifiers, outcome);
                }
            }
            _ => {}
        }
    }

    /// 同一策略供右键转发和冻结选区的菜单保留判定使用。
    pub(super) fn pane_right_click_modifiers(
        &self,
        hit: &PaneHit,
        mouse: MouseEvent,
    ) -> Option<crossterm::event::KeyModifiers> {
        if !hit.mouse_reporting {
            return None;
        }
        let configured = self
            .config
            .right_click_passthrough_modifiers
            .filter(|modifiers| *modifiers == mouse.modifiers);
        if configured.is_some() {
            return configured;
        }
        self.snapshot
            .as_ref()
            .is_some_and(|snapshot| {
                snapshot
                    .panes
                    .iter()
                    .any(|pane| pane.pane_id == hit.pane_id && pane.right_click_passthrough)
            })
            .then_some(crossterm::event::KeyModifiers::empty())
            .filter(|_| mouse.modifiers.is_empty())
    }

    fn begin_codex_selection_press(&mut self, hit: &PaneHit, mouse: MouseEvent) -> bool {
        if !self.config.mouse_capture
            || !mouse.modifiers.is_empty()
            || !self.supports_frozen_selection()
        {
            return false;
        }
        let Some(snapshot) = self.snapshot.as_ref().filter(|snapshot| {
            snapshot.agents.iter().any(|agent| {
                agent.pane_id == hit.pane_id && agent.agent.as_deref() == Some("codex")
            })
        }) else {
            return false;
        };
        self.pane_selection_press = Some(ClientPaneSelectionPress {
            endpoint: self.active_endpoint_id.clone(),
            boot: snapshot.boot_id.clone(),
            generation: self.active_snapshot_generation,
            hit: hit.clone(),
            down: mouse,
            pixels: self.host_mouse_pixels,
            focus_confirmed: snapshot.focused_pane_id.as_deref() == Some(&hit.pane_id),
        });
        true
    }

    fn pending_selection_mouse(
        &mut self,
        mouse: MouseEvent,
        outcome: &mut ClientShellInput,
    ) -> bool {
        let Some(press) = self.pane_selection_press.take() else {
            return false;
        };
        let is_release = mouse.kind == MouseEventKind::Up(MouseButton::Left);
        let follows_press = is_release || mouse.kind == MouseEventKind::Drag(MouseButton::Left);
        let valid = self.config.mouse_capture
            && self.overlay.is_none()
            && !self.popup_pending
            && self.popup_terminal_id.is_none()
            && press.endpoint == self.active_endpoint_id
            && press.generation == self.active_snapshot_generation
            && self.snapshot.as_ref().is_some_and(|snapshot| {
                snapshot.boot_id == press.boot
                    && snapshot
                        .panes
                        .iter()
                        .any(|pane| pane.pane_id == press.hit.pane_id)
            })
            && self.hits.panes.iter().any(|hit| {
                hit.pane_id == press.hit.pane_id
                    && hit.inner_rect == press.hit.inner_rect
                    && hit.mouse_reporting
            });
        if !valid {
            return follows_press;
        }
        if mouse.kind == MouseEventKind::Moved {
            self.pane_selection_press = Some(press);
            return true;
        }
        if !follows_press {
            return false;
        }
        let moved = (mouse.column, mouse.row) != (press.down.column, press.down.row);
        if !moved && !is_release {
            self.pane_selection_press = Some(press);
            return true;
        }
        if moved
            && mouse.modifiers.is_empty()
            && self.begin_frozen_selection(&press.hit, press.down, 1, outcome)
            && self.selection_capture.is_some()
        {
            self.frozen_selection_mouse(mouse, outcome);
            return true;
        }
        // 单击或无法取得冻结能力：按原顺序交给应用。像素Down必须使用按下时坐标。
        let current_pixels = self.host_mouse_pixels;
        self.host_mouse_pixels = press.pixels;
        self.push_pane_mouse_event(&press.hit, press.down, press.down.modifiers, outcome);
        self.host_mouse_pixels = current_pixels;
        self.push_pane_mouse_event(&press.hit, mouse, mouse.modifiers, outcome);
        if !is_release {
            self.pane_mouse_gesture = Some(ClientPaneMouseGesture {
                last_position: self.pane_mouse_position(&press.hit, mouse),
                hit: press.hit,
                button: MouseButton::Left,
                stripped_modifiers: crossterm::event::KeyModifiers::empty(),
                last_event: mouse,
            });
        }
        true
    }

    fn pane_mouse_position(&self, hit: &PaneHit, mouse: MouseEvent) -> ClientMousePosition {
        let cell = ClientMousePosition::Cell {
            column: mouse.column.saturating_sub(hit.inner_rect.x),
            row: mouse.row.saturating_sub(hit.inner_rect.y),
        };
        if hit.sgr_pixel_mouse && hit.pixel_width > 0 && hit.pixel_height > 0 {
            self.host_mouse_pixels
                .and_then(|pixels| {
                    pixels
                        .pane_position(hit.inner_rect, hit.pixel_width, hit.pixel_height)
                        .and_then(|position| match position {
                            crate::input::mouse::Position::Pixels { x, y } => {
                                Some(ClientMousePosition::Pixels {
                                    x,
                                    y,
                                    column: mouse.column.saturating_sub(hit.inner_rect.x),
                                    row: mouse.row.saturating_sub(hit.inner_rect.y),
                                })
                            }
                            crate::input::mouse::Position::Cell { .. } => None,
                        })
                })
                .unwrap_or(cell)
        } else {
            cell
        }
    }

    pub(super) fn push_pane_mouse_event(
        &self,
        hit: &PaneHit,
        mouse: MouseEvent,
        modifiers: crossterm::event::KeyModifiers,
        outcome: &mut ClientShellInput,
    ) {
        let Some(kind) = crate::protocol::ClientMouseKind::from_crossterm(mouse.kind) else {
            return;
        };
        let position = self.pane_mouse_position(hit, mouse);
        let geometry = matches!(position, ClientMousePosition::Pixels { .. }).then_some(
            crate::protocol::ClientMouseGeometry {
                cols: hit.inner_rect.width,
                rows: hit.inner_rect.height,
                width_px: hit.pixel_width,
                height_px: hit.pixel_height,
            },
        );
        let target = if hit.popup {
            ClientInputTarget::Popup(hit.pane_id.clone())
        } else {
            ClientInputTarget::Pane(hit.pane_id.clone())
        };
        push_target_event(
            target,
            ClientPaneInputEvent::Mouse {
                kind,
                position,
                geometry,
                modifiers: modifiers.bits(),
                lines: self.config.mouse_scroll_lines.min(u16::MAX as usize) as u16,
            },
            outcome,
        );
    }
}
