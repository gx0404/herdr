//! Virtual rendering helpers for headless client frame streaming.

use ratatui::backend::{Backend, ClearType, TestBackend, WindowSize};
use ratatui::layout::{Position, Rect, Size};

use crate::app::state::AppState;
use crate::protocol::render_ansi::{BlitEncoder, EncodedBlit};
use crate::protocol::{
    CursorState, FrameData, PaneSurfaceFrame, PaneSurfacePatch, RenderEncoding, ServerMessage,
    SurfaceGraphicsAssetKey, SurfaceGraphicsScene, TerminalFrame,
};
use crate::terminal::TerminalRuntimeRegistry;

/// Per-client render baseline for the negotiated render encoding.
pub(crate) enum ClientRenderState {
    /// Semantic clients compare full frame data and skip identical frames.
    Semantic {
        last_surface: Option<Box<PaneSurfaceFrame>>,
        surface_revision: u64,
        surface_reuse: bool,
        surface_delta: bool,
        recompute_pending: bool,
    },
    /// Terminal-ANSI clients keep a terminal diff encoder and sequence number.
    TerminalAnsi {
        blit_encoder: BlitEncoder,
        seq: u64,
        repaint_pending: bool,
    },
}

impl ClientRenderState {
    pub(crate) fn new(render_encoding: RenderEncoding) -> Self {
        match render_encoding {
            RenderEncoding::SemanticFrame => Self::Semantic {
                last_surface: None,
                surface_revision: 0,
                surface_reuse: false,
                surface_delta: false,
                recompute_pending: false,
            },
            RenderEncoding::TerminalAnsi => Self::TerminalAnsi {
                blit_encoder: BlitEncoder::new(),
                seq: 0,
                repaint_pending: false,
            },
        }
    }

    pub(crate) fn enable_surface_reuse(&mut self, enabled: bool) {
        if let Self::Semantic { surface_reuse, .. } = self {
            *surface_reuse = enabled;
        }
    }

    pub(crate) fn enable_surface_delta(&mut self, enabled: bool) {
        if let Self::Semantic { surface_delta, .. } = self {
            *surface_delta = enabled;
        }
    }

    pub(crate) fn request_recompute(&mut self) {
        if let Self::Semantic {
            surface_delta: true,
            recompute_pending,
            ..
        } = self
        {
            *recompute_pending = true;
        } else {
            self.request_repaint();
        }
    }

    pub(crate) fn requires_recompute(&self) -> bool {
        matches!(
            self,
            Self::Semantic {
                recompute_pending: true,
                ..
            }
        )
    }

    pub(crate) fn reset_baseline(&mut self) {
        match self {
            Self::Semantic { last_surface, .. } => *last_surface = None,
            Self::TerminalAnsi {
                blit_encoder,
                repaint_pending,
                ..
            } => {
                *blit_encoder = BlitEncoder::new();
                *repaint_pending = false;
            }
        }
    }

    pub(crate) fn request_repaint(&mut self) {
        match self {
            Self::Semantic { last_surface, .. } => *last_surface = None,
            Self::TerminalAnsi {
                repaint_pending, ..
            } => *repaint_pending = true,
        }
    }

    pub(crate) fn prepare_frame(&mut self, frame: FrameData) -> Option<PreparedRender> {
        match self {
            Self::Semantic { .. } => None,
            Self::TerminalAnsi {
                blit_encoder,
                seq,
                repaint_pending,
            } => {
                if !*repaint_pending && blit_encoder.is_current(&frame) {
                    crate::render_prof::event("prepare_frame.ansi.skip_current");
                    return None;
                }
                let mut encoded = blit_encoder.encode(&frame, *repaint_pending);
                crate::render_prof::event("prepare_frame.ansi.changed");
                crate::render_prof::counter("prepare_frame.ansi.bytes", encoded.bytes.len() as u64);
                if encoded.full {
                    crate::render_prof::event("prepare_frame.ansi.full");
                } else {
                    crate::render_prof::event("prepare_frame.ansi.partial");
                }
                insert_graphics_before_sync_end(&mut encoded.bytes, &frame.graphics);
                crate::render_prof::counter(
                    "prepare_frame.graphics.bytes",
                    frame.graphics.len() as u64,
                );
                Some(PreparedRender::TerminalAnsi {
                    message: ServerMessage::Terminal(TerminalFrame {
                        seq: *seq + 1,
                        width: frame.width,
                        height: frame.height,
                        full: encoded.full,
                        bytes: encoded.bytes.clone(),
                    }),
                    frame,
                    encoded: Some(encoded),
                })
            }
        }
    }

    pub(crate) fn last_pane_surface(&self) -> Option<&PaneSurfaceFrame> {
        match self {
            Self::Semantic { last_surface, .. } => last_surface.as_deref(),
            Self::TerminalAnsi { .. } => None,
        }
    }

    /// 无原生文件上传的整面准备（fork 多视图 view 与测试使用）。
    pub(crate) fn prepare_pane_surface(
        &mut self,
        surface: PaneSurfaceFrame,
    ) -> Option<PreparedRender> {
        self.prepare_pane_surface_with_file(surface, false)
    }

    pub(crate) fn prepare_pane_surface_with_file(
        &mut self,
        mut surface: PaneSurfaceFrame,
        has_file_upload: bool,
    ) -> Option<PreparedRender> {
        let Self::Semantic {
            last_surface,
            surface_revision,
            surface_reuse,
            surface_delta,
            recompute_pending,
        } = self
        else {
            return None;
        };
        if !has_file_upload
            && !*recompute_pending
            && surface.graphics.assets.is_empty()
            && last_surface.as_deref().is_some_and(|last| {
                last.projection_revision == surface.projection_revision
                    && last.frame == surface.frame
                    && last.panes == surface.panes
                    && last.splits == surface.splits
                    && last.popup == surface.popup
                    && last.graphics.placements == surface.graphics.placements
                    && last.graphics.retained_assets == surface.graphics.retained_assets
            })
        {
            return None;
        }
        surface.surface_revision = surface_revision.saturating_add(1);
        let assets = std::mem::take(&mut surface.graphics.assets);
        let queued_graphics_assets = assets.iter().map(|asset| asset.key.clone()).collect();
        let committed_surface = surface.clone();
        surface.graphics.assets = assets;
        let mut message = ServerMessage::PaneSurface(surface);
        let delta = (*surface_delta)
            .then_some(last_surface.as_deref())
            .flatten()
            .and_then(|last| {
                crate::protocol::surface_delta::message(last, &mut message)
                    .map_err(|error| tracing::warn!(%error, "failed to encode surface delta"))
                    .ok()
                    .flatten()
            });
        let reused = if let ServerMessage::PaneSurface(surface) = &mut message {
            (delta.is_none() && *surface_reuse)
                .then_some(last_surface.as_deref())
                .flatten()
                .filter(|last| {
                    last.boot_id == surface.boot_id
                        && last.frame == surface.frame
                        // Popup cells are not part of the reusable grid; keep their compact codec.
                        && surface.popup.is_none()
                        && surface.graphics.assets.is_empty()
                })
                .and_then(|last| {
                    crate::protocol::surface_reuse::message(last.surface_revision, surface)
                        .map_err(|error| tracing::warn!(%error, "failed to encode surface reuse"))
                        .ok()
                        .flatten()
                })
        } else {
            None
        };
        Some(PreparedRender::Semantic {
            message: delta.or(reused).unwrap_or(message),
            committed_surface: Box::new(committed_surface),
            queued_graphics_assets,
        })
    }

    /// 投影修订号前进而画面未变时，把已提交的 surface 改戳到新修订号重发，不重新
    /// 渲染 pane。协商了复用编码的连接只发非单元格部分（单元格沿用客户端基线）。
    /// 没有已提交基线时返回 `None`，由调用方回退整帧渲染。
    pub(crate) fn prepare_projection_restamp(
        &self,
        projection_revision: u64,
    ) -> Option<PreparedRender> {
        let Self::Semantic {
            last_surface,
            surface_revision,
            surface_reuse,
            recompute_pending,
            ..
        } = self
        else {
            return None;
        };
        // 上游 #4508：上一次完整渲染被推迟（同步输出未完成）时基线不再可信，
        // 回退整帧渲染由它继续推迟。
        if *recompute_pending {
            return None;
        }
        let last = last_surface.as_deref()?;
        let mut surface = last.clone();
        surface.projection_revision = projection_revision;
        surface.surface_revision = surface_revision.saturating_add(1);
        // 与 `prepare_pane_surface` 的复用条件一致：popup 单元格不在可复用网格里。
        let reused = (*surface_reuse && surface.popup.is_none())
            .then(|| {
                crate::protocol::surface_reuse::message(last.surface_revision, &mut surface)
                    .map_err(|error| tracing::warn!(%error, "failed to encode surface reuse"))
                    .ok()
                    .flatten()
            })
            .flatten();
        let message = match reused {
            Some(message) => message,
            None => ServerMessage::PaneSurface(surface.clone()),
        };
        Some(PreparedRender::Semantic {
            message,
            committed_surface: Box::new(surface),
            // 改戳只重发已提交基线，不携带新的内联像素载荷。
            queued_graphics_assets: Vec::new(),
        })
    }

    pub(crate) fn prepare_pane_surface_patch(
        &mut self,
        mut patch: PaneSurfacePatch,
    ) -> Option<PreparedRender> {
        if self.requires_recompute() {
            return None;
        }
        let Self::Semantic {
            last_surface,
            surface_revision,
            ..
        } = self
        else {
            return None;
        };
        let last = last_surface.as_deref()?;
        if last.boot_id != patch.boot_id
            || last.projection_revision != patch.projection_revision
            || last.surface_revision != patch.base_surface_revision
        {
            return None;
        }
        let next_revision = surface_revision.saturating_add(1);
        patch.surface_revision = next_revision;
        if !patch.hyperlink_uris.is_empty() {
            // 冻结的 v1 补丁不能追加链接表。沿用 retained 文本/布局，无须重绘
            // pane；协商了 delta 时只发送增量，否则发送兼容的完整 surface。
            let mut surface = last.clone();
            apply_pane_surface_patch(&mut surface, &patch);
            return self.prepare_pane_surface(surface);
        }
        Some(PreparedRender::SemanticPatch {
            message: ServerMessage::PaneSurfacePatch(patch),
        })
    }

    pub(crate) fn commit_sent_frame(&mut self, prepared: PreparedRender) {
        match (self, prepared) {
            (
                Self::Semantic {
                    last_surface,
                    surface_revision,
                    recompute_pending,
                    ..
                },
                PreparedRender::Semantic {
                    committed_surface, ..
                },
            ) => {
                *surface_revision = committed_surface.surface_revision;
                *last_surface = Some(committed_surface);
                *recompute_pending = false;
            }
            (
                Self::Semantic {
                    last_surface,
                    surface_revision,
                    ..
                },
                PreparedRender::SemanticPatch {
                    message: ServerMessage::PaneSurfacePatch(patch),
                },
            ) => {
                // RS-15：补丁提交只在基线存在时发生（规划阶段已保证）；缺失时
                // 不 panic，也不推进修订号，等待下一次全量帧重建基线。
                let Some(surface) = last_surface.as_deref_mut() else {
                    return;
                };
                apply_pane_surface_patch(surface, &patch);
                *surface_revision = patch.surface_revision;
            }
            (
                Self::TerminalAnsi {
                    blit_encoder,
                    seq,
                    repaint_pending,
                },
                PreparedRender::TerminalAnsi {
                    frame,
                    encoded: Some(encoded),
                    ..
                },
            ) => {
                blit_encoder.commit(frame, encoded);
                *seq += 1;
                *repaint_pending = false;
            }
            _ => {}
        }
    }
}

// Planning validates all rows and pane IDs before any send. The server does not yield
// between planning and commit, so applying the accepted patch cannot fail partway through.
pub(super) fn apply_pane_surface_patch(surface: &mut PaneSurfaceFrame, patch: &PaneSurfacePatch) {
    debug_assert_eq!(surface.boot_id, patch.boot_id);
    debug_assert_eq!(surface.projection_revision, patch.projection_revision);
    debug_assert_eq!(surface.surface_revision, patch.base_surface_revision);
    // RS-01 根治：先把补丁新增的超链接 URI 按序并进基线表，再写行——
    // 行内索引以「基线表长 + 增量偏移」绝对编码。
    surface
        .frame
        .hyperlinks
        .extend(patch.hyperlink_uris.iter().cloned());
    for row in &patch.rows {
        let start = usize::from(row.y) * usize::from(surface.frame.width) + usize::from(row.x);
        // RS-15：规划阶段已校验行落在帧内；这里防御性跳过越界行而不是切片 panic。
        let Some(target) = surface
            .frame
            .cells
            .get_mut(start..start.saturating_add(row.cells.len()))
        else {
            continue;
        };
        target.clone_from_slice(&row.cells);
    }
    for updated in &patch.panes {
        // RS-15：客户端同样按 pane_id 归并，缺条目时跳过该条（不 panic）。
        if let Some(pane) = surface
            .panes
            .iter_mut()
            .find(|pane| pane.pane_id == updated.pane_id)
        {
            pane.clone_from(updated);
        }
    }
    surface.frame.cursor.clone_from(&patch.cursor);
    surface.surface_revision = patch.surface_revision;
}

fn insert_graphics_before_sync_end(encoded: &mut Vec<u8>, graphics: &[u8]) {
    if graphics.is_empty() {
        return;
    }

    if let Some(sync_end) = crate::protocol::render_ansi::final_sync_output_end(encoded) {
        encoded.splice(sync_end..sync_end, graphics.iter().copied());
    } else {
        encoded.extend_from_slice(graphics);
    }
}

/// A prepared client render message plus any baseline state needed after send.
pub(crate) enum PreparedRender {
    Semantic {
        message: ServerMessage,
        committed_surface: Box<PaneSurfaceFrame>,
        queued_graphics_assets: Vec<SurfaceGraphicsAssetKey>,
    },
    SemanticPatch {
        message: ServerMessage,
    },
    TerminalAnsi {
        message: ServerMessage,
        frame: FrameData,
        encoded: Option<EncodedBlit>,
    },
}

impl PreparedRender {
    pub(crate) fn message(&self) -> &ServerMessage {
        match self {
            Self::Semantic { message, .. }
            | Self::SemanticPatch { message }
            | Self::TerminalAnsi { message, .. } => message,
        }
    }

    /// Graphics metadata represented by this semantic update plus only the
    /// asset keys whose pixel payloads were queued. This is independent of the
    /// selected wire codec and avoids cloning asset byte vectors.
    pub(crate) fn queued_surface_graphics(
        &self,
    ) -> Option<(&SurfaceGraphicsScene, &[SurfaceGraphicsAssetKey])> {
        match self {
            Self::Semantic {
                committed_surface,
                queued_graphics_assets,
                ..
            } => Some((&committed_surface.graphics, queued_graphics_assets)),
            Self::SemanticPatch { .. } | Self::TerminalAnsi { .. } => None,
        }
    }

    pub(crate) fn has_queued_surface_assets(&self) -> bool {
        matches!(self, Self::Semantic { queued_graphics_assets, .. } if !queued_graphics_assets.is_empty())
    }

    /// Removes the largest inline payload from a full semantic surface while
    /// preserving placement metadata. Largest-first guarantees that a fitting
    /// smaller asset is not discarded behind an oversized one. Equal sizes use
    /// deterministic scene order. Encoded delta/reuse messages return `None`; callers
    /// can invalidate that baseline and retry as a full surface.
    pub(crate) fn pop_pane_surface_asset(&mut self) -> Option<SurfaceGraphicsAssetKey> {
        let Self::Semantic {
            message: ServerMessage::PaneSurface(surface),
            queued_graphics_assets,
            ..
        } = self
        else {
            return None;
        };
        let index = surface
            .graphics
            .assets
            .iter()
            .enumerate()
            .max_by_key(|(index, asset)| (asset.data.len(), *index))?
            .0;
        let asset = surface.graphics.assets.remove(index);
        let key = asset.key;
        if let Some(index) = queued_graphics_assets
            .iter()
            .position(|queued| *queued == key)
        {
            queued_graphics_assets.remove(index);
        }
        Some(key)
    }
}

struct CursorTrackingBackend {
    inner: TestBackend,
    rendered_cursor: Option<Position>,
}

impl CursorTrackingBackend {
    fn new(width: u16, height: u16) -> Self {
        Self {
            inner: TestBackend::new(width, height),
            rendered_cursor: None,
        }
    }

    fn buffer(&self) -> &ratatui::buffer::Buffer {
        self.inner.buffer()
    }

    fn rendered_cursor(&self) -> Option<CursorState> {
        self.rendered_cursor.map(|pos| CursorState {
            x: pos.x,
            y: pos.y,
            visible: true,
            shape: 0,
        })
    }
}

impl Backend for CursorTrackingBackend {
    type Error = std::convert::Infallible;

    fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
    where
        I: Iterator<Item = (u16, u16, &'a ratatui::buffer::Cell)>,
    {
        self.inner.draw(content)
    }

    fn append_lines(&mut self, n: u16) -> Result<(), Self::Error> {
        self.inner.append_lines(n)
    }

    fn hide_cursor(&mut self) -> Result<(), Self::Error> {
        self.inner.hide_cursor()?;
        self.rendered_cursor = None;
        Ok(())
    }

    fn show_cursor(&mut self) -> Result<(), Self::Error> {
        self.inner.show_cursor()
    }

    fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
        self.inner.get_cursor_position()
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> Result<(), Self::Error> {
        let position = position.into();
        self.inner.set_cursor_position(position)?;
        self.rendered_cursor = Some(position);
        Ok(())
    }

    fn clear(&mut self) -> Result<(), Self::Error> {
        self.inner.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> Result<(), Self::Error> {
        self.inner.clear_region(clear_type)
    }

    fn size(&self) -> Result<Size, Self::Error> {
        self.inner.size()
    }

    fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
        self.inner.window_size()
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        self.inner.flush()
    }
}

pub(crate) type RenderedTabSurface = (
    ratatui::buffer::Buffer,
    Option<CursorState>,
    Vec<((u16, u16), String, String)>,
    crate::ui::TabSurfaceLayout,
);

/// Renders only the active tab's pane surface at an origin-relative client viewport.
pub(crate) fn render_tab_surface_virtual(
    app_state: &AppState,
    terminal_runtimes: &TerminalRuntimeRegistry,
    layout: crate::ui::TabSurfaceLayout,
    area: Rect,
) -> RenderedTabSurface {
    let surface = crate::ui::TabSurfaceView {
        target: layout.target,
        pane_infos: &layout.pane_infos,
        split_borders: &layout.split_borders,
    };
    let cursor = crate::ui::tab_surface_cursor(app_state, terminal_runtimes, surface);
    let hyperlinks = crate::ui::tab_surface_hyperlinks(app_state, terminal_runtimes, surface);

    let backend = CursorTrackingBackend::new(area.width, area.height);
    // RS-15：`Terminal::new` 对本地后端不会失败（错误类型不可构造），直接绑定 Ok
    // 分支，不做 expect。
    let Ok(mut terminal) = ratatui::Terminal::new(backend);
    // 绘制失败保留后端里已经画出的部分（与 `Terminal::draw` 的语义一致）。
    let _ = terminal.draw(|frame| {
        crate::ui::render_tab_surface(app_state, terminal_runtimes, surface, frame);
    });

    (
        terminal.backend().buffer().clone(),
        cursor,
        hyperlinks,
        layout,
    )
}

/// Renders one server-owned terminal directly for `terminal attach` clients.
pub(crate) fn render_terminal_virtual(
    runtime: &crate::terminal::TerminalRuntime,
    area: Rect,
) -> (ratatui::buffer::Buffer, Option<CursorState>) {
    let backend = CursorTrackingBackend::new(area.width, area.height);
    // RS-15：同上；`Terminal::new` 不失败，直接绑定 Ok 分支。
    let Ok(mut terminal) = ratatui::Terminal::new(backend);

    let _ = terminal.draw(|frame| {
        runtime.render(frame, area, true);
    });

    let buffer = terminal.backend().buffer().clone();
    // DECSET 2026 批次进行中不再整帧抑制光标：`runtime.cursor_state` 沿用批次前
    // 快照的光标（pane 层看门狗，超时自动失效），attach 客户端不会收到一次
    // `?25l` 闪断。与 `ui::tab_surface_cursor` / headless `retained_cursor`
    // （两者共用 `ui::pane_host_cursor`）是同一判据；本路径无 IME 揭示。
    let cursor = runtime
        .cursor_state(area, true)
        .map(|cursor| CursorState {
            x: cursor.x,
            y: cursor.y,
            visible: cursor.visible && !crate::ui::pane_is_scrolled_back(runtime),
            shape: cursor.shape,
        })
        .or_else(|| terminal.backend().rendered_cursor());

    (buffer, cursor)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ClientShellPopupSurface;

    fn popup_surface(content: &str) -> PaneSurfaceFrame {
        let pane = ratatui::buffer::Buffer::with_lines(["pane"]);
        let popup = ratatui::buffer::Buffer::with_lines([content]);
        PaneSurfaceFrame {
            boot_id: "boot-1".into(),
            projection_revision: 1,
            surface_revision: 1,
            frame: FrameData::from_ratatui_buffer_with_hyperlinks(&pane, None, &[]),
            panes: Vec::new(),
            splits: Vec::new(),
            popup: Some(Box::new(ClientShellPopupSurface {
                terminal_id: "popup-terminal".into(),
                title: "popup".into(),
                width: None,
                height: None,
                frame: FrameData::from_ratatui_buffer_with_hyperlinks(&popup, None, &[]),
                mouse_reporting: false,
                sgr_pixel_mouse: false,
                pixel_width: 0,
                pixel_height: 0,
            })),
            graphics: crate::protocol::SurfaceGraphicsScene::default(),
        }
    }

    /// attach 直渲路径：DECSET 2026 批次进行中沿用批次前光标而不是整帧关掉宿主
    /// 光标，批次结束后恢复当前光标。
    #[tokio::test]
    async fn render_terminal_virtual_keeps_the_cursor_during_synchronized_output() {
        let runtime = crate::terminal::TerminalRuntime::test_with_screen_bytes(20, 8, b"READY");
        let area = Rect::new(0, 0, 20, 8);
        let (_, before) = render_terminal_virtual(&runtime, area);
        let before = before.expect("批次前有光标");
        assert!(before.visible);
        assert_eq!((before.x, before.y), (5, 0));

        runtime.test_process_pty_bytes(b"\x1b[?2026h\x1b[?25l\x1b[3;3H");
        assert!(runtime.synchronized_output_active());
        let (_, during) = render_terminal_virtual(&runtime, area);
        assert_eq!(during, Some(before), "批次内沿用批次前光标，不发 ?25l");

        runtime.test_process_pty_bytes(b"\x1b[?2026l\x1b[?25h");
        let (_, after) = render_terminal_virtual(&runtime, area);
        let after = after.expect("批次结束后有光标");
        assert!(after.visible);
        assert_eq!((after.x, after.y), (2, 2));
    }

    #[test]
    fn hyperlink_patch_uses_negotiated_delta_or_frozen_full_surface() {
        for enabled in [false, true] {
            let mut state = ClientRenderState::new(RenderEncoding::SemanticFrame);
            state.enable_surface_delta(enabled);
            let mut decoder = crate::protocol::surface_reuse::Decoder::new(enabled);
            let mut surface = popup_surface("popup");
            surface.popup = None;
            surface.frame = FrameData::from_ratatui_buffer(
                &ratatui::buffer::Buffer::empty(Rect::new(0, 0, 120, 40)),
                None,
            );
            let initial = state.prepare_pane_surface(surface.clone()).unwrap();
            decoder.decode(initial.message().clone()).unwrap();
            state.commit_sent_frame(initial);
            let mut cell = surface.frame.cells[0].clone();
            cell.symbol = "link".into();
            cell.hyperlink = Some(0);
            let prepared = state
                .prepare_pane_surface_patch(PaneSurfacePatch {
                    boot_id: surface.boot_id.clone(),
                    projection_revision: surface.projection_revision,
                    base_surface_revision: 1,
                    surface_revision: 0,
                    rows: vec![crate::protocol::PaneSurfacePatchRow {
                        x: 0,
                        y: 0,
                        cells: vec![cell.clone()],
                    }],
                    panes: Vec::new(),
                    cursor: None,
                    hyperlink_uris: vec!["https://example.test/new".into()],
                })
                .unwrap();
            assert_eq!(state.last_pane_surface().unwrap().surface_revision, 1);
            assert!(state
                .last_pane_surface()
                .unwrap()
                .frame
                .hyperlinks
                .is_empty());
            assert_eq!(
                matches!(prepared.message(), ServerMessage::EndpointControl { kind, .. }
                if kind == crate::protocol::surface_delta::MESSAGE_KIND),
                enabled
            );
            let mut bytes = Vec::new();
            crate::protocol::write_message(&mut bytes, prepared.message()).unwrap();
            let message = crate::protocol::read_message(
                &mut bytes.as_slice(),
                crate::protocol::MAX_FRAME_SIZE,
            )
            .unwrap();
            let ServerMessage::PaneSurface(decoded) = decoder.decode(message).unwrap() else {
                panic!("new hyperlinks require a complete table, never an extended legacy patch");
            };
            assert_eq!(decoded.frame.cells[0], cell);
            assert_eq!(decoded.frame.hyperlinks, ["https://example.test/new"]);
            assert_eq!(decoded.surface_revision, 2);
            state.commit_sent_frame(prepared);
            assert_eq!(state.last_pane_surface().unwrap(), &decoded);
        }
    }

    #[test]
    fn surface_delta_recompute_preserves_wire_baseline_but_epoch_reset_drops_it() {
        for enabled in [false, true] {
            let mut state = ClientRenderState::new(RenderEncoding::SemanticFrame);
            state.enable_surface_delta(enabled);
            let mut surface = popup_surface("popup");
            surface.popup = None;
            surface.frame = FrameData::from_ratatui_buffer(
                &ratatui::buffer::Buffer::empty(Rect::new(0, 0, 120, 40)),
                None,
            );
            let initial = state.prepare_pane_surface(surface.clone()).unwrap();
            state.commit_sent_frame(initial);
            state.request_recompute();
            assert_eq!(state.last_pane_surface().is_some(), enabled);
            assert_eq!(state.requires_recompute(), enabled);
            // A freshness request still emits a new revision when every cell is equal.
            let fresh = state.prepare_pane_surface(surface.clone()).unwrap();
            assert_eq!(
                matches!(fresh.message(), ServerMessage::EndpointControl { kind, .. }
                if kind == crate::protocol::surface_delta::MESSAGE_KIND),
                enabled
            );
            assert_eq!(
                state.requires_recompute(),
                enabled,
                "prepare must not commit"
            );
            state.commit_sent_frame(fresh);
            assert!(!state.requires_recompute());
            assert_eq!(state.last_pane_surface().unwrap().surface_revision, 2);
            state.request_repaint();
            assert!(state.last_pane_surface().is_none());
            let recovery = state.prepare_pane_surface(surface).unwrap();
            assert!(
                matches!(recovery.message(), ServerMessage::PaneSurface(frame) if frame.surface_revision == 3)
            );
        }
    }

    #[test]
    fn surface_reuse_preserves_projection_and_patch_baselines_without_resending_cells() {
        for enabled in [false, true] {
            let mut state = ClientRenderState::new(RenderEncoding::SemanticFrame);
            state.enable_surface_reuse(enabled);
            let mut decoder = crate::protocol::surface_reuse::Decoder::default();
            let mut surface = popup_surface("popup");
            surface.popup = None;
            let buffer = ratatui::buffer::Buffer::empty(Rect::new(0, 0, 240, 100));
            surface.frame = FrameData::from_ratatui_buffer(&buffer, None);
            let initial = state.prepare_pane_surface(surface.clone()).unwrap();
            decoder.decode(initial.message().clone()).unwrap();
            state.commit_sent_frame(initial);

            surface.projection_revision += 1;
            let update = state.prepare_pane_surface(surface.clone()).unwrap();
            let mut bytes = Vec::new();
            crate::protocol::write_message(&mut bytes, update.message()).unwrap();
            if enabled {
                assert!(
                    matches!(update.message(), ServerMessage::EndpointControl { kind, .. }
                    if kind == crate::protocol::surface_reuse::MESSAGE_KIND)
                );
                assert!(
                    bytes.len() < 2000,
                    "metadata update was {} bytes",
                    bytes.len()
                );
            } else {
                assert!(matches!(update.message(), ServerMessage::PaneSurface(_)));
                assert!(bytes.len() > 100_000);
            }
            let ServerMessage::PaneSurface(decoded) =
                decoder.decode(update.message().clone()).unwrap()
            else {
                panic!("decoded full surface");
            };
            assert_eq!(decoded.frame, surface.frame);
            assert_eq!(decoded.projection_revision, surface.projection_revision);
            assert_eq!(decoded.surface_revision, 2);
            state.commit_sent_frame(update);

            let mut changed_cell = surface.frame.cells[0].clone();
            changed_cell.symbol = "x".into();
            let patch = state
                .prepare_pane_surface_patch(PaneSurfacePatch {
                    boot_id: surface.boot_id.clone(),
                    projection_revision: surface.projection_revision,
                    base_surface_revision: 2,
                    surface_revision: 0,
                    rows: vec![crate::protocol::PaneSurfacePatchRow {
                        x: 0,
                        y: 0,
                        cells: vec![changed_cell.clone()],
                    }],
                    panes: Vec::new(),
                    cursor: None,
                    hyperlink_uris: Vec::new(),
                })
                .unwrap();
            decoder.decode(patch.message().clone()).unwrap();
            state.commit_sent_frame(patch);
            surface.frame.cells[0] = changed_cell;
            surface.projection_revision += 1;
            let update = state.prepare_pane_surface(surface.clone()).unwrap();
            let ServerMessage::PaneSurface(decoded) =
                decoder.decode(update.message().clone()).unwrap()
            else {
                panic!("decoded surface after patch");
            };
            assert_eq!(decoded.frame, surface.frame);
            assert_eq!(decoded.surface_revision, 4);
            state.commit_sent_frame(update);

            // A changed border or terminal cell must still reach the client.
            surface.frame.cells[0].symbol = "y".into();
            let changed = state.prepare_pane_surface(surface.clone()).unwrap();
            assert!(matches!(changed.message(), ServerMessage::PaneSurface(_)));
            let ServerMessage::PaneSurface(decoded) =
                decoder.decode(changed.message().clone()).unwrap()
            else {
                panic!("changed full surface");
            };
            assert_eq!(decoded.frame, surface.frame);
            state.commit_sent_frame(changed);

            state.request_repaint();
            assert!(matches!(
                state.prepare_pane_surface(surface).unwrap().message(),
                ServerMessage::PaneSurface(_)
            ));
        }
    }

    /// 投影改戳：不重渲染，只把已提交基线的投影修订号前移。复用连接的线上消息
    /// 不带单元格，解码后与基线逐格一致；此后的补丁以改戳后的基线为准。
    #[test]
    fn projection_restamp_resends_the_committed_surface_under_the_new_revision() {
        for enabled in [false, true] {
            let mut state = ClientRenderState::new(RenderEncoding::SemanticFrame);
            state.enable_surface_reuse(enabled);
            assert!(
                state.prepare_projection_restamp(2).is_none(),
                "无基线不改戳"
            );

            let mut decoder = crate::protocol::surface_reuse::Decoder::default();
            let mut surface = popup_surface("popup");
            surface.popup = None;
            let buffer = ratatui::buffer::Buffer::empty(Rect::new(0, 0, 240, 100));
            surface.frame = FrameData::from_ratatui_buffer(&buffer, None);
            let initial = state.prepare_pane_surface(surface.clone()).unwrap();
            decoder.decode(initial.message().clone()).unwrap();
            state.commit_sent_frame(initial);

            let next_revision = surface.projection_revision + 1;
            let restamp = state.prepare_projection_restamp(next_revision).unwrap();
            let mut bytes = Vec::new();
            crate::protocol::write_message(&mut bytes, restamp.message()).unwrap();
            if enabled {
                assert!(
                    matches!(restamp.message(), ServerMessage::EndpointControl { kind, .. }
                    if kind == crate::protocol::surface_reuse::MESSAGE_KIND)
                );
                assert!(bytes.len() < 2000, "改戳消息 {} 字节", bytes.len());
            } else {
                assert!(matches!(restamp.message(), ServerMessage::PaneSurface(_)));
            }
            let ServerMessage::PaneSurface(decoded) =
                decoder.decode(restamp.message().clone()).unwrap()
            else {
                panic!("decoded restamped surface");
            };
            assert_eq!(decoded.frame, surface.frame);
            assert_eq!(decoded.projection_revision, next_revision);
            assert_eq!(decoded.surface_revision, 2);
            state.commit_sent_frame(restamp);
            let committed = state.last_pane_surface().unwrap();
            assert_eq!(committed.projection_revision, next_revision);
            assert_eq!(committed.surface_revision, 2);

            let mut changed_cell = surface.frame.cells[0].clone();
            changed_cell.symbol = "x".into();
            let patch = state
                .prepare_pane_surface_patch(PaneSurfacePatch {
                    boot_id: surface.boot_id.clone(),
                    projection_revision: next_revision,
                    base_surface_revision: 2,
                    surface_revision: 0,
                    rows: vec![crate::protocol::PaneSurfacePatchRow {
                        x: 0,
                        y: 0,
                        cells: vec![changed_cell],
                    }],
                    panes: Vec::new(),
                    cursor: None,
                    hyperlink_uris: Vec::new(),
                })
                .expect("补丁以改戳后的基线为准");
            decoder.decode(patch.message().clone()).unwrap();
        }
    }

    #[test]
    fn surface_reuse_keeps_popup_cells_on_the_binary_codec() {
        let mut state = ClientRenderState::new(RenderEncoding::SemanticFrame);
        state.enable_surface_reuse(true);
        let mut surface = popup_surface("popup");
        let buffer = ratatui::buffer::Buffer::empty(Rect::new(0, 0, 400, 100));
        surface.popup.as_mut().unwrap().frame = FrameData::from_ratatui_buffer(&buffer, None);
        let initial = state.prepare_pane_surface(surface.clone()).unwrap();
        state.commit_sent_frame(initial);
        surface.projection_revision += 1;
        let update = state.prepare_pane_surface(surface).unwrap();
        assert!(matches!(update.message(), ServerMessage::PaneSurface(_)));
        let mut bytes = Vec::new();
        crate::protocol::write_message(&mut bytes, update.message()).unwrap();
        assert!(bytes.len() < crate::protocol::MAX_FRAME_SIZE);
    }

    #[test]
    fn surface_reuse_falls_back_when_json_metadata_exceeds_the_frame_limit() {
        let mut state = ClientRenderState::new(RenderEncoding::SemanticFrame);
        state.enable_surface_reuse(true);
        let mut surface = popup_surface("popup");
        surface.popup = None;
        surface.frame.hyperlinks = vec!["\"".repeat(crate::protocol::MAX_FRAME_SIZE / 2)];
        let initial = state.prepare_pane_surface(surface.clone()).unwrap();
        state.commit_sent_frame(initial);
        surface.projection_revision += 1;
        let update = state.prepare_pane_surface(surface).unwrap();
        assert!(matches!(update.message(), ServerMessage::PaneSurface(_)));
        let mut bytes = Vec::new();
        crate::protocol::write_message(&mut bytes, update.message()).unwrap();
        assert!(bytes.len() < crate::protocol::MAX_FRAME_SIZE);
    }

    #[test]
    fn deferred_file_upload_keeps_identical_metadata_and_retries_without_committing() {
        for reuse in [false, true] {
            let mut state = ClientRenderState::new(RenderEncoding::SemanticFrame);
            state.enable_surface_reuse(reuse);
            let surface = popup_surface("native");
            let first = state.prepare_pane_surface(surface.clone()).unwrap();
            state.commit_sent_frame(first);
            assert!(state.prepare_pane_surface(surface.clone()).is_none());
            let file = state
                .prepare_pane_surface_with_file(surface.clone(), true)
                .unwrap();
            let retry = state
                .prepare_pane_surface_with_file(surface.clone(), true)
                .unwrap();
            let config = bincode::config::standard();
            assert_eq!(
                bincode::serde::encode_to_vec(file.message(), config).unwrap(),
                bincode::serde::encode_to_vec(retry.message(), config).unwrap()
            );
            state.commit_sent_frame(retry);
            assert!(state.prepare_pane_surface(surface).is_none());
        }
    }

    #[test]
    fn popup_only_surface_changes_are_not_deduplicated() {
        let mut state = ClientRenderState::new(RenderEncoding::SemanticFrame);
        let prepared = state
            .prepare_pane_surface(popup_surface("first"))
            .expect("initial surface");
        state.commit_sent_frame(prepared);

        assert!(state
            .prepare_pane_surface(popup_surface("second"))
            .is_some());
    }

    #[test]
    fn forced_full_surface_keeps_the_connection_revision_monotonic() {
        let mut state = ClientRenderState::new(RenderEncoding::SemanticFrame);
        let prepared = state
            .prepare_pane_surface(popup_surface("first"))
            .expect("initial surface");
        state.commit_sent_frame(prepared);
        state.request_repaint();

        let prepared = state
            .prepare_pane_surface(popup_surface("replacement"))
            .expect("forced replacement surface");
        assert!(matches!(
            prepared.message(),
            ServerMessage::PaneSurface(surface) if surface.surface_revision == 2
        ));
        state.commit_sent_frame(prepared);
        assert_eq!(state.last_pane_surface().unwrap().surface_revision, 2);
    }
}
