//! 鼠标阅读独占手势与不可变画面；实时 surface 始终继续接收。

use super::*;
use crate::api::schema::{
    Method, ResponseResult, TextSnapshotCaptureParams, TextSnapshotSelectionParams,
    TextSnapshotTarget,
};
use crate::terminal::text_snapshot::FrozenText;
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use std::time::{Duration, Instant};

pub(super) struct Capture {
    pub epoch: u64,
    pub endpoint: ClientEndpointId,
    pub boot: String,
    pub generation: Option<u64>,
    pub hit: PaneHit,
    pub focus_confirmed: bool,
    anchor: (u16, u16),
    pointer: (u16, u16),
    streak: u8,
    dragged: bool,
    released: bool,
    finalized: Option<((u32, u16), (u32, u16))>,
    want_copy: bool,
    copy_sent: bool,
    started: Instant,
    next_scroll: Instant,
    token: Option<String>,
    text: Option<Box<FrozenText>>,
    preview: FrameData,
    preview_area: Rect,
    source_area: Rect,
    changed_before_release: bool,
    presented: bool,
    top: u32,
    desired_top: u32,
    window_pending: bool,
    queued_browse: i32,
    anchor_bounds: Option<(u16, u16)>,
}

pub(super) struct Release {
    endpoint: ClientEndpointId,
    boot: String,
    token: String,
}

impl ClientShellState {
    pub(super) fn cancel_frozen_selection(&mut self) {
        let Some(capture) = self.selection_capture.take() else {
            return;
        };
        if let Some(token) = capture.token {
            self.selection_releases.push(Release {
                endpoint: capture.endpoint,
                boot: capture.boot,
                token,
            });
        }
        self.selection_epoch = self.selection_epoch.saturating_add(1);
        self.selection = None;
        self.stop_selection_autoscroll();
        self.word_selection_gesture = None;
        self.selection_highlight_clear_deadline = None;
    }

    pub(super) fn begin_frozen_selection(
        &mut self,
        hit: &PaneHit,
        mouse: MouseEvent,
        streak: u8,
        outcome: &mut ClientShellInput,
    ) -> bool {
        let advertised = self
            .endpoints
            .iter()
            .find(|endpoint| endpoint.endpoint_id == self.active_endpoint_id)
            .and_then(|endpoint| endpoint.methods.as_ref())
            .is_some_and(|methods| {
                ["capture", "read", "selection", "release", "retain"]
                    .iter()
                    .all(|name| methods.contains(&format!("pane.text_snapshot.{name}")))
            });
        if !advertised || hit.popup {
            return false;
        }
        self.cancel_frozen_selection();
        let surface = self.visible_surface_for_pane(&hit.pane_id);
        let Some((frame, area)) = surface.and_then(|surface| {
            surface
                .panes
                .iter()
                .find(|pane| pane.pane_id == hit.pane_id)
                .map(|pane| {
                    let rect = pane.inner_rect;
                    let mut cells =
                        Vec::with_capacity(usize::from(rect.width) * usize::from(rect.height));
                    for row in rect.y..rect.y.saturating_add(rect.height) {
                        let start = usize::from(row) * usize::from(surface.frame.width)
                            + usize::from(rect.x);
                        if let Some(slice) = surface
                            .frame
                            .cells
                            .get(start..start + usize::from(rect.width))
                        {
                            cells.extend_from_slice(slice);
                        }
                    }
                    (
                        FrameData {
                            width: rect.width,
                            height: rect.height,
                            cells,
                            cursor: None,
                            hyperlinks: surface.frame.hyperlinks.clone(),
                            graphics: Vec::new(),
                        },
                        Rect::new(rect.x, rect.y, rect.width, rect.height),
                    )
                })
        }) else {
            return false;
        };
        self.selection_epoch = self.selection_epoch.saturating_add(1);
        let epoch = self.selection_epoch;
        let now = Instant::now();
        self.selection_capture = Some(Capture {
            epoch,
            endpoint: self.active_endpoint_id.clone(),
            boot: self
                .snapshot
                .as_ref()
                .map(|snapshot| snapshot.boot_id.clone())
                .unwrap_or_default(),
            generation: self.active_snapshot_generation,
            hit: hit.clone(),
            focus_confirmed: self
                .snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.focused_pane_id.as_deref() == Some(&hit.pane_id)),
            anchor: (
                mouse.row.saturating_sub(hit.inner_rect.y),
                mouse.column.saturating_sub(hit.inner_rect.x),
            ),
            pointer: (mouse.column, mouse.row),
            streak,
            dragged: false,
            released: false,
            finalized: None,
            want_copy: false,
            copy_sent: false,
            started: now,
            next_scroll: now,
            token: None,
            text: None,
            preview: frame,
            preview_area: Rect::new(0, 0, area.width, area.height),
            source_area: area,
            changed_before_release: false,
            presented: false,
            top: 0,
            desired_top: 0,
            window_pending: false,
            queued_browse: 0,
            anchor_bounds: None,
        });
        self.selection = None;
        self.word_selection_gesture = None;
        let sent = self.push_endpoint_method_with_kind(
            Method::PaneTextSnapshotCapture(TextSnapshotCaptureParams {
                pane_id: hit.pane_id.clone(),
            }),
            PendingEndpointKind::TextCapture {
                epoch,
                endpoint: self.active_endpoint_id.clone(),
            },
            outcome,
        );
        if !sent {
            self.cancel_frozen_selection();
        }
        outcome.repaint = true;
        true
    }

    pub(super) fn visible_surface_for_pane(&self, pane: &str) -> Option<&PaneSurfaceFrame> {
        if self.workbench.enabled {
            self.workbench
                .views
                .values()
                .map(|view| &view.surface)
                .find(|surface| surface.panes.iter().any(|entry| entry.pane_id == pane))
        } else {
            self.pane_surface
                .as_ref()
                .filter(|surface| surface.panes.iter().any(|entry| entry.pane_id == pane))
        }
    }

    fn rebuild_frozen_selection(&mut self) {
        let Some(capture) = self.selection_capture.as_ref() else {
            return;
        };
        let Some(text) = capture.text.as_ref() else {
            return;
        };
        let row = capture
            .pointer
            .1
            .saturating_sub(capture.hit.inner_rect.y)
            .min(capture.hit.inner_rect.height.saturating_sub(1));
        let col = capture
            .pointer
            .0
            .saturating_sub(capture.hit.inner_rect.x)
            .min(text.cols.saturating_sub(1));
        let anchor = (
            text.viewport_start + u32::from(capture.anchor.0),
            capture.anchor.1,
        );
        let cursor = (capture.top + u32::from(row), col);
        let (anchor, cursor) = capture.finalized.unwrap_or_else(|| match capture.streak {
            2 => {
                let bounds = |point: (u32, u16)| {
                    let row = text
                        .row(point.0)
                        .map(|row| {
                            row.cells
                                .iter()
                                .filter(|cell| cell.width != 0 && cell.width != 3)
                                .map(|cell| cell.text.as_str())
                                .collect::<String>()
                        })
                        .unwrap_or_default();
                    crate::app::actions::word_bounds_at_column(&row, point.1)
                        .unwrap_or((point.1, point.1))
                };
                let a = capture.anchor_bounds.unwrap_or_else(|| bounds(anchor));
                let c = bounds(cursor);
                if anchor <= cursor {
                    ((anchor.0, a.0), (cursor.0, c.1))
                } else {
                    ((anchor.0, a.1), (cursor.0, c.0))
                }
            }
            3.. => {
                if anchor <= cursor {
                    ((anchor.0, 0), (cursor.0, text.cols.saturating_sub(1)))
                } else {
                    ((anchor.0, text.cols.saturating_sub(1)), (cursor.0, 0))
                }
            }
            _ => (anchor, cursor),
        });
        let mut selection = if capture.dragged || capture.streak > 1 {
            crate::selection::Selection::absolute_range(capture.hit.pane_id.clone(), anchor, cursor)
        } else {
            crate::selection::Selection::absolute_anchor(capture.hit.pane_id.clone(), anchor)
        };
        if capture.released {
            selection.finish();
        }
        self.selection = Some(selection);
        if let Some(capture) = self.selection_capture.as_mut().filter(|capture| {
            capture.released && !capture.window_pending && capture.top == capture.desired_top
        }) {
            capture.finalized = Some((anchor, cursor));
        }
    }

    pub(super) fn frozen_selection_mouse(
        &mut self,
        mouse: MouseEvent,
        outcome: &mut ClientShellInput,
    ) -> bool {
        if self.overlay.is_some() {
            self.cancel_frozen_selection();
            return false;
        }
        let Some(capture) = self.selection_capture.as_mut() else {
            return false;
        };
        match mouse.kind {
            MouseEventKind::Down(_) => {
                self.cancel_frozen_selection();
                return false;
            }
            MouseEventKind::Drag(MouseButton::Left) if !capture.released => {
                capture.pointer = (mouse.column, mouse.row);
                if capture.presented {
                    capture.changed_before_release = false;
                }
                capture.dragged |= (
                    mouse.row.saturating_sub(capture.hit.inner_rect.y),
                    mouse.column.saturating_sub(capture.hit.inner_rect.x),
                ) != capture.anchor;
            }
            MouseEventKind::Up(MouseButton::Left) if !capture.released => {
                capture.pointer = (mouse.column, mouse.row);
                capture.dragged |= (
                    mouse.row.saturating_sub(capture.hit.inner_rect.y),
                    mouse.column.saturating_sub(capture.hit.inner_rect.x),
                ) != capture.anchor;
                capture.released = true;
                capture.want_copy =
                    self.config.copy_on_select && (capture.dragged || capture.streak > 1);
                if !capture.dragged && capture.streak == 1 {
                    self.cancel_frozen_selection();
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                if capture.released && capture.finalized.is_none() {
                    capture.queued_browse = capture.queued_browse.saturating_add(
                        if mouse.kind == MouseEventKind::ScrollUp {
                            -3
                        } else {
                            3
                        },
                    );
                    return true;
                }
                if contains(capture.hit.rect, (mouse.column, mouse.row)) || !capture.released {
                    self.scroll_frozen_selection(if mouse.kind == MouseEventKind::ScrollUp {
                        -3
                    } else {
                        3
                    });
                } else {
                    return false;
                }
            }
            MouseEventKind::Moved if !capture.released => return true,
            _ => return false,
        }
        self.request_frozen_window(outcome);
        self.rebuild_frozen_selection();
        self.maybe_copy_frozen_selection(outcome);
        outcome.repaint |= if matches!(mouse.kind, MouseEventKind::Drag(_)) {
            self.request_selection_drag_repaint(Instant::now())
        } else {
            true
        };
        true
    }

    fn scroll_frozen_selection(&mut self, delta: i32) {
        let Some(capture) = self.selection_capture.as_mut() else {
            return;
        };
        let Some(text) = capture.text.as_ref() else {
            return;
        };
        let max = text.range_end.saturating_sub(u32::from(text.viewport_rows));
        capture.desired_top = capture
            .desired_top
            .saturating_add_signed(delta)
            .clamp(text.range_start, max.max(text.range_start));
        if text.row(capture.desired_top).is_some()
            && text
                .row(capture.desired_top + u32::from(text.viewport_rows.saturating_sub(1)))
                .is_some()
        {
            capture.top = capture.desired_top;
        }
    }

    fn request_frozen_window(&mut self, outcome: &mut ClientShellInput) {
        let Some(capture) = self.selection_capture.as_mut() else {
            return;
        };
        if capture.window_pending || capture.top == capture.desired_top {
            return;
        }
        let (Some(token), Some(text)) = (capture.token.clone(), capture.text.as_ref()) else {
            return;
        };
        capture.window_pending = true;
        let epoch = capture.epoch;
        let params = crate::api::schema::TextSnapshotReadParams {
            snapshot_id: token,
            start_row: capture.desired_top.saturating_sub(16).max(text.range_start),
            rows: text.viewport_rows.saturating_add(32),
        };
        if !self.push_endpoint_method_with_kind(
            Method::PaneTextSnapshotRead(params),
            PendingEndpointKind::TextWindow { epoch },
            outcome,
        ) {
            self.cancel_frozen_selection();
        }
    }

    pub(super) fn receive_text_window(
        &mut self,
        epoch: u64,
        result: Result<ResponseResult, ClientShellEndpointError>,
    ) -> (bool, Vec<ClientShellAction>) {
        let Some(capture) = self
            .selection_capture
            .as_mut()
            .filter(|capture| capture.epoch == epoch && capture.window_pending)
        else {
            return (false, Vec::new());
        };
        let valid = match &result {
            Ok(ResponseResult::PaneTextSnapshot {
                snapshot_id,
                pane_id,
                boot_id,
                text,
            }) => {
                text.valid_window()
                    && Some(snapshot_id.as_str()) == capture.token.as_deref()
                    && *pane_id == capture.hit.pane_id
                    && *boot_id == capture.boot
                    && capture.text.as_ref().is_some_and(|old| {
                        old.cols == text.cols
                            && old.viewport_rows == text.viewport_rows
                            && old.viewport_start == text.viewport_start
                            && old.range_start == text.range_start
                            && old.range_end == text.range_end
                            && old.total_rows == text.total_rows
                            && old.content_revision == text.content_revision
                    })
            }
            _ => false,
        };
        if !valid {
            self.cancel_frozen_selection();
            self.set_endpoint_error("无法读取冻结的历史行，请重新选择");
            return (true, Vec::new());
        }
        if let Ok(ResponseResult::PaneTextSnapshot { text, .. }) = result {
            capture.window_pending = false;
            if text.row(capture.desired_top).is_some()
                && text
                    .row(capture.desired_top + u32::from(text.viewport_rows.saturating_sub(1)))
                    .is_some()
            {
                capture.top = capture.desired_top;
                capture.text = Some(text);
            }
        }
        let mut outcome = ClientShellInput::default();
        self.request_frozen_window(&mut outcome);
        self.rebuild_frozen_selection();
        self.flush_frozen_browse(&mut outcome);
        self.maybe_copy_frozen_selection(&mut outcome);
        (true, outcome.actions)
    }

    fn flush_frozen_browse(&mut self, outcome: &mut ClientShellInput) {
        let delta = self
            .selection_capture
            .as_mut()
            .filter(|capture| capture.finalized.is_some() && !capture.want_copy)
            .map(|capture| std::mem::take(&mut capture.queued_browse))
            .unwrap_or_default();
        if delta != 0 {
            self.scroll_frozen_selection(delta);
            self.request_frozen_window(outcome);
        }
    }

    pub(super) fn copy_frozen_selection(&mut self, outcome: &mut ClientShellInput) -> bool {
        let Some(capture) = self.selection_capture.as_mut() else {
            return false;
        };
        capture.want_copy = true;
        capture.changed_before_release = false;
        capture.released = true;
        self.maybe_copy_frozen_selection(outcome);
        true
    }

    fn maybe_copy_frozen_selection(&mut self, outcome: &mut ClientShellInput) {
        let geometry_changed = self.selection_capture.as_ref().is_some_and(|capture| {
            self.visible_surface_for_pane(&capture.hit.pane_id)
                .and_then(|surface| {
                    surface
                        .panes
                        .iter()
                        .find(|pane| pane.pane_id == capture.hit.pane_id)
                })
                .is_none_or(|pane| {
                    Rect::new(
                        pane.inner_rect.x,
                        pane.inner_rect.y,
                        pane.inner_rect.width,
                        pane.inner_rect.height,
                    ) != capture.source_area
                })
        });
        if geometry_changed {
            self.cancel_frozen_selection();
            return;
        }
        let Some(capture) = self.selection_capture.as_mut() else {
            return;
        };
        if capture.want_copy && capture.changed_before_release {
            capture.want_copy = false;
            self.set_endpoint_error("画面在捕获完成前已更新；请确认固定选区后按 Ctrl+C 复制");
            outcome.repaint = true;
            return;
        }
        if !capture.want_copy
            || capture.copy_sent
            || capture.window_pending
            || capture.top != capture.desired_top
        {
            return;
        }
        let Some(token) = capture.token.clone() else {
            return;
        };
        let Some(selection) = self.selection.as_ref() else {
            return;
        };
        let (anchor, cursor) = selection.ordered_cells();
        capture.copy_sent = true;
        let epoch = capture.epoch;
        let sent = self.push_endpoint_method_with_kind(
            Method::PaneTextSnapshotSelection(TextSnapshotSelectionParams {
                snapshot_id: token,
                anchor: crate::api::schema::PaneTextPoint {
                    row: anchor.0,
                    col: anchor.1,
                },
                cursor: crate::api::schema::PaneTextPoint {
                    row: cursor.0,
                    col: cursor.1,
                },
            }),
            PendingEndpointKind::TextCopy { epoch },
            outcome,
        );
        if !sent {
            self.cancel_frozen_selection();
        }
    }

    pub(super) fn receive_text_capture(
        &mut self,
        epoch: u64,
        endpoint: ClientEndpointId,
        boot: &str,
        result: Result<ResponseResult, ClientShellEndpointError>,
    ) -> (bool, Vec<ClientShellAction>) {
        let valid = self.selection_capture.as_ref().is_some_and(|capture| {
            capture.epoch == epoch
                && capture.endpoint == endpoint
                && capture.boot == boot
                && endpoint == self.active_endpoint_id
                && capture.generation == self.active_snapshot_generation
        });
        match result {
            Ok(ResponseResult::PaneTextSnapshot {
                snapshot_id,
                pane_id,
                boot_id,
                text,
            }) => {
                if !valid {
                    self.selection_releases.push(Release {
                        endpoint,
                        boot: boot.to_owned(),
                        token: snapshot_id,
                    });
                    return (false, Vec::new());
                }
                let Some(capture) = self.selection_capture.as_mut() else {
                    return (false, Vec::new());
                };
                capture.token = Some(snapshot_id);
                if !text.valid_capture()
                    || capture.hit.pane_id != pane_id
                    || boot_id != boot
                    || text.cols != capture.hit.inner_rect.width
                    || text.viewport_rows != capture.hit.inner_rect.height
                    || text.row(text.viewport_start).is_none()
                    || text
                        .row(
                            text.viewport_start
                                .saturating_add(u32::from(text.viewport_rows.saturating_sub(1))),
                        )
                        .is_none()
                {
                    self.cancel_frozen_selection();
                    self.set_endpoint_error("画面尺寸已变化，请重新选择");
                    return (true, Vec::new());
                }
                capture.changed_before_release = (0..text.viewport_rows).any(|row| {
                    text.row(text.viewport_start + u32::from(row))
                        .is_none_or(|line| {
                            line.cells.iter().enumerate().any(|(col, cell)| {
                                capture
                                    .preview
                                    .cells
                                    .get(usize::from(row) * usize::from(text.cols) + col)
                                    .is_none_or(|previous| {
                                        previous.symbol != cell.text && cell.width != 0
                                    })
                            })
                        })
                });
                capture.top = text.viewport_start;
                capture.desired_top = text.viewport_start;
                let anchor_row = text
                    .row(text.viewport_start + u32::from(capture.anchor.0))
                    .map(|row| {
                        row.cells
                            .iter()
                            .filter(|cell| cell.width != 0 && cell.width != 3)
                            .map(|cell| cell.text.as_str())
                            .collect::<String>()
                    })
                    .unwrap_or_default();
                capture.anchor_bounds =
                    crate::app::actions::word_bounds_at_column(&anchor_row, capture.anchor.1);
                capture.text = Some(text);
                self.rebuild_frozen_selection();
                let token = self
                    .selection_capture
                    .as_ref()
                    .and_then(|capture| capture.token.clone());
                let mut outcome = ClientShellInput::default();
                if let Some(snapshot_id) = token {
                    self.push_endpoint_method_with_kind(
                        Method::PaneTextSnapshotRetain(TextSnapshotTarget { snapshot_id }),
                        PendingEndpointKind::TextRelease,
                        &mut outcome,
                    );
                }
                self.flush_frozen_browse(&mut outcome);
                self.maybe_copy_frozen_selection(&mut outcome);
                (true, outcome.actions)
            }
            error if valid => {
                self.cancel_frozen_selection();
                self.set_endpoint_error(
                    error
                        .err()
                        .map(|error| error.message)
                        .unwrap_or_else(|| "无法读取固定画面".into()),
                );
                (true, Vec::new())
            }
            _ => (false, Vec::new()),
        }
    }

    pub(super) fn receive_text_copy(
        &mut self,
        epoch: u64,
        result: Result<ResponseResult, ClientShellEndpointError>,
    ) -> (bool, Vec<ClientShellAction>) {
        let valid = self.selection_capture.as_ref().is_some_and(|capture| {
            capture.epoch == epoch
                && capture.endpoint == self.active_endpoint_id
                && capture.generation == self.active_snapshot_generation
                && capture.copy_sent
        });
        if !valid {
            return (false, Vec::new());
        }
        let token = self
            .selection_capture
            .as_ref()
            .and_then(|capture| capture.token.clone());
        self.cancel_frozen_selection();
        match result {
            Ok(ResponseResult::PaneTextSnapshotSelection { snapshot_id, text })
                if token.as_deref() == Some(snapshot_id.as_str()) =>
            {
                if text.is_empty() {
                    return (true, Vec::new());
                }
                self.show_copy_feedback(Instant::now());
                (
                    true,
                    vec![ClientShellAction::ClipboardWrite(text.into_bytes())],
                )
            }
            other => {
                self.set_endpoint_error(
                    other
                        .err()
                        .map(|error| error.message)
                        .unwrap_or_else(|| "复制快照响应不匹配".into()),
                );
                (true, Vec::new())
            }
        }
    }

    pub(super) fn tick_frozen_selection(&mut self, now: Instant, outcome: &mut ClientShellInput) {
        if let Some(capture) = self.selection_capture.as_mut() {
            let invalid = !self.config.mouse_capture
                || capture.endpoint != self.active_endpoint_id
                || capture.generation != self.active_snapshot_generation
                || self.snapshot.as_ref().is_none_or(|snapshot| {
                    snapshot.boot_id != capture.boot
                        || !snapshot
                            .panes
                            .iter()
                            .any(|pane| pane.pane_id == capture.hit.pane_id)
                })
                || self.overlay.is_some()
                || self
                    .copy_mode
                    .as_ref()
                    .is_some_and(|copy_mode| copy_mode.pane_id == capture.hit.pane_id);
            let timed_out = capture.text.is_none()
                && now.duration_since(capture.started) > Duration::from_secs(15);
            if invalid || timed_out {
                self.cancel_frozen_selection();
                if timed_out {
                    self.set_endpoint_error("阅读快照已超时，请重新选择");
                }
                outcome.repaint = true;
            } else if !capture.released && capture.dragged && now >= capture.next_scroll {
                let step = if capture.pointer.1 < capture.hit.inner_rect.y {
                    -1
                } else if capture.pointer.1 >= capture.hit.inner_rect.bottom() {
                    1
                } else {
                    0
                };
                capture.next_scroll = now + self.config.selection_autoscroll_interval;
                if step != 0 {
                    self.scroll_frozen_selection(step);
                    self.request_frozen_window(outcome);
                    self.rebuild_frozen_selection();
                    outcome.repaint = true;
                }
            }
        }
        for release in std::mem::take(&mut self.selection_releases) {
            if self.endpoint_boot_id(&release.endpoint) == Some(release.boot.as_str()) {
                self.push_endpoint_method_for(
                    &release.endpoint,
                    Method::PaneTextSnapshotRelease(TextSnapshotTarget {
                        snapshot_id: release.token,
                    }),
                    PendingEndpointKind::TextRelease,
                    outcome,
                );
            }
        }
    }

    pub(super) fn paint_frozen_selection(
        &mut self,
        frame: &mut FrameData,
        occlusion: &mut crate::kitty_graphics::surface::Occlusion,
    ) {
        let Some(capture) = self.selection_capture.as_ref() else {
            return;
        };
        let Some(hit) = self
            .hits
            .panes
            .iter_mut()
            .find(|hit| hit.pane_id == capture.hit.pane_id)
        else {
            return;
        };
        if hit.inner_rect != capture.hit.inner_rect {
            self.cancel_frozen_selection();
            return;
        }
        let area = hit
            .inner_rect
            .intersection(Rect::new(0, 0, frame.width, frame.height));
        let link_offset = frame.hyperlinks.len() as u32;
        if capture.text.is_none() {
            frame.hyperlinks.extend(capture.preview.hyperlinks.clone());
        }
        for y in 0..area.height {
            for x in 0..area.width {
                let cell = if let Some(text) = capture.text.as_ref() {
                    text.row(capture.top + u32::from(y))
                        .and_then(|row| row.cells.get(usize::from(x)))
                        .map(|cell| {
                            let hyperlink = cell.hyperlink.as_ref().map(|uri| {
                                frame.hyperlinks.push(uri.clone());
                                frame.hyperlinks.len() as u32 - 1
                            });
                            crate::protocol::CellData {
                                symbol: if cell.width == 0 {
                                    " ".into()
                                } else {
                                    cell.text.clone()
                                },
                                fg: cell.fg,
                                bg: cell.bg,
                                modifier: cell.modifier,
                                skip: cell.width == 0,
                                hyperlink,
                            }
                        })
                } else {
                    capture
                        .preview
                        .cells
                        .get(
                            usize::from(capture.preview_area.y + y)
                                * usize::from(capture.preview.width)
                                + usize::from(capture.preview_area.x + x),
                        )
                        .cloned()
                        .map(|mut cell| {
                            cell.hyperlink = cell.hyperlink.map(|id| id + link_offset);
                            cell
                        })
                };
                if let Some(cell) = cell {
                    frame.cells[usize::from(area.y + y) * usize::from(frame.width)
                        + usize::from(area.x + x)] = cell;
                }
            }
        }
        if let Some(text) = &capture.text {
            hit.scroll = Some(crate::pane::ScrollMetrics {
                viewport_rows: usize::from(text.viewport_rows),
                max_offset_from_bottom: text
                    .total_rows
                    .saturating_sub(u32::from(text.viewport_rows))
                    as usize,
                offset_from_bottom: text
                    .total_rows
                    .saturating_sub(u32::from(text.viewport_rows))
                    .saturating_sub(capture.top) as usize,
            });
        }
        if frame
            .cursor
            .as_ref()
            .is_some_and(|cursor| contains(area, (cursor.x, cursor.y)))
        {
            frame.cursor = None;
        }
        if let Some(capture) = self
            .selection_capture
            .as_mut()
            .filter(|capture| capture.text.is_some())
        {
            capture.presented = true;
        }
        occlusion.cover(area);
    }
}
