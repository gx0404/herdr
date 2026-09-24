use std::borrow::Cow;

use super::*;

#[cfg(unix)]
const MAX_RETIRED_DIRECT_GRAPHICS: usize = 64;

#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RetiredDirectGraphicsMatch {
    None,
    Exact,
    Saturated,
}

#[cfg(unix)]
pub(super) struct RetiredDirectGraphics {
    generation: u64,
    transfers: Vec<(u64, u32)>,
    saturated: bool,
}

#[cfg(unix)]
impl RetiredDirectGraphics {
    fn new(generation: u64) -> Self {
        Self {
            generation,
            transfers: Vec::new(),
            saturated: false,
        }
    }
}

/// State tracking for the thin client.
pub(super) struct ClientState {
    /// Stateful semantic-frame encoder used when the server sends FrameData.
    pub(super) blit_encoder: render_ansi::BlitEncoder,
    pub(super) image_files: image_files::FileTransport,
    pub(super) mouse_capture_active: bool,
    pub(super) endpoint_mouse_capture_requested: bool,
    pub(super) endpoint_sgr_pixels_requested: bool,
    /// Latest physical host theme observations, retained so an endpoint selected after the
    /// observation receives the same client-owned baseline.
    pub(super) host_theme_updates: Vec<crate::protocol::ClientHostThemeUpdate>,
    pub(super) direct_mouse_capture_preference: bool,
    pub(super) shell_mouse_capture_preference: bool,
    pub(super) direct_keyboard_protocol: crate::terminal_modes::DirectHostKeyboardState,
    pub(super) pane_keyboard_report_all: bool,
    pub(super) keyboard_report_all_active: bool,
    pub(super) reported_size: (u16, u16),
    pub(super) reported_cell_size: (u32, u32),
    pub(super) sound_config: crate::config::SoundConfig,
    pub(super) kitty_graphics_enabled: bool,
    pub(super) pixel_geometry_enabled: bool,
    pub(super) pixel_geometry_exact: bool,
    #[cfg(unix)]
    pub(super) direct_graphics_response: Arc<Mutex<direct_graphics::ResponseMatcher>>,
    #[cfg(unix)]
    pub(super) retired_direct_graphics: HashMap<endpoint::ClientEndpointId, RetiredDirectGraphics>,
    #[cfg(unix)]
    pub(super) disabled_native_graphics: HashMap<endpoint::ClientEndpointId, u64>,
    pub(super) pending_native_cleanup: Vec<u8>,
    #[cfg(unix)]
    pub(super) pending_surface_graphics: HashMap<
        (endpoint::ClientEndpointId, u64, u64, u32),
        crate::protocol::SurfaceGraphicsAssetKey,
    >,
    pub(super) attach_escape: Option<AttachEscapeState>,
    #[cfg(unix)]
    pub(super) mouse_scroll_lines: usize,
    pub(super) remote_image_paste_key:
        Option<(crossterm::event::KeyCode, crossterm::event::KeyModifiers)>,
    pub(super) redraw_on_focus_gained: bool,
    pub(super) repaint_pending: bool,
    /// During a source-off-first handoff the currently blitted frame remains authoritative until
    /// an acknowledged target snapshot/surface pair commits.
    pub(super) presentation_frozen: bool,
    /// Latest explicit Local selection awaiting this client's replacement Local connection.
    pub(super) deferred_local_activation: Option<endpoint::EndpointActivationIntent>,
    pub(super) draw_host_cursor: bool,
    pub(super) detached_process_children: Vec<std::process::Child>,
    pub(super) shell: Option<shell::ClientShellState>,
    /// kitty 图形清理不再单独写宿主：先暂存，随下一次呈现的同步块一起送达，
    /// 让图片删除与帧内容同一批次到达（否则宿主会先呈现「无图」的中间态）。
    ///
    /// 不变量：每个 `present_graphics` 调用点紧跟 `present_frame` /
    /// `present_surface_patch` / `flush_pending_graphics`；`unfreeze_presentation`
    /// 必排空；客户端事件循环每轮开头兜底排空并在 debug 构建下断言为空。
    pub(super) pending_graphics: Vec<u8>,
}

impl Drop for ClientState {
    fn drop(&mut self) {
        // 退出路径统一收口：`run_client_loop` 还有 `ServerShutdown` /
        // `ConnectionLost` / `?` 传播等不经过显式 flush 的返回，延迟落盘的最后
        // 一次布局变更不能在那里静默丢掉。已 flush 过时脏标记为空，这里是空操作。
        if let Some(shell) = self.shell.as_mut() {
            shell.flush_chrome_preferences(&mut shell::ClientShellInput::default());
        }
        if self.attach_escape.is_some() {
            let _ = crate::terminal_modes::set_direct_host_keyboard_protocol(
                &mut io::stdout(),
                &mut self.direct_keyboard_protocol,
                0,
                0,
            );
        }
    }
}

impl ClientState {
    #[cfg(test)]
    pub(super) fn test_new() -> Self {
        Self {
            blit_encoder: render_ansi::BlitEncoder::new(),
            image_files: image_files::FileTransport::default(),
            mouse_capture_active: false,
            endpoint_mouse_capture_requested: false,
            endpoint_sgr_pixels_requested: false,
            host_theme_updates: Vec::new(),
            direct_mouse_capture_preference: false,
            shell_mouse_capture_preference: false,
            direct_keyboard_protocol: Default::default(),
            pane_keyboard_report_all: false,
            keyboard_report_all_active: false,
            reported_size: (100, 30),
            reported_cell_size: (0, 0),
            sound_config: Default::default(),
            kitty_graphics_enabled: false,
            pixel_geometry_enabled: false,
            pixel_geometry_exact: false,
            #[cfg(unix)]
            direct_graphics_response: Default::default(),
            #[cfg(unix)]
            retired_direct_graphics: HashMap::new(),
            #[cfg(unix)]
            disabled_native_graphics: Default::default(),
            pending_native_cleanup: Vec::new(),
            #[cfg(unix)]
            pending_surface_graphics: HashMap::new(),
            attach_escape: None,
            #[cfg(unix)]
            mouse_scroll_lines: 3,
            remote_image_paste_key: None,
            redraw_on_focus_gained: false,
            repaint_pending: false,
            presentation_frozen: false,
            deferred_local_activation: None,
            draw_host_cursor: false,
            detached_process_children: Vec::new(),
            shell: Some(shell::ClientShellState::new(
                shell::ClientShellConfig::from_config(&crate::config::Config::default()),
            )),
            pending_graphics: Vec::new(),
        }
    }

    pub(super) fn request_repaint(&mut self) {
        self.repaint_pending = true;
    }

    /// 宿主终端尺寸变化：记下新尺寸，作废主机侧 blit 基线与 pane 命中几何。
    pub(super) fn apply_terminal_resize(
        &mut self,
        cols: u16,
        rows: u16,
        cell_width_px: u32,
        cell_height_px: u32,
        pixel_geometry_exact: bool,
    ) {
        self.reported_size = (cols, rows);
        self.reported_cell_size = (cell_width_px, cell_height_px);
        self.pixel_geometry_exact = pixel_geometry_exact;
        // Resizing invalidates both the host-side blit baseline and pane hit geometry.
        self.request_repaint();
        if let Some(shell) = self.shell.as_mut() {
            shell.set_graphics_cell_size(cell_width_px, cell_height_px);
            shell.invalidate_pane_surface();
        }
    }

    /// 宿主 resize 之后，停靠工作台立即按新尺寸组合并呈现一帧（冒烟 M12）。
    ///
    /// 经典布局等服务端按新尺寸推来配对的 surface（此刻组合只会画「不可用」
    /// 占位），这里不动。工作台的几何只取决于终端尺寸：服务端为这次 resize
    /// 没有东西可推（views 只随 `client.views.set` 变），而 `tick_workbench` 按
    /// 上一次组合的尺寸算 views——不在这里出帧，画面与 views 都停在旧尺寸，
    /// 直到下一次输入或服务端碰巧推帧（紧凑视图只剩 Agents 面板时什么都不来）。
    pub(super) fn present_after_resize(&mut self) {
        if self
            .shell
            .as_ref()
            .is_some_and(|shell| shell.composes_on_host_resize())
        {
            self.compose_and_present();
        }
    }

    /// compose 并呈现一帧；`compose` 返回 `None`（等待配对的快照 / surface
    /// 分代）时记下「还欠一帧」：否则这次 repaint 请求被静默吞掉，后续补丁会
    /// 走快路径一直不补画这次变化（CFP-12）。
    pub(super) fn compose_and_present(&mut self) {
        let Some(shell) = self.shell.as_mut() else {
            return;
        };
        let (cols, rows) = self.reported_size;
        match shell.compose(cols, rows) {
            Some(frame) => self.present_frame(frame),
            None => self.request_repaint(),
        }
    }

    pub(super) fn freeze_presentation(&mut self) {
        self.presentation_frozen = true;
    }

    pub(super) fn record_host_theme_update(
        &mut self,
        update: &crate::protocol::ClientHostThemeUpdate,
    ) {
        use crate::protocol::ClientHostThemeUpdate;

        match update {
            ClientHostThemeUpdate::DefaultColor { kind, .. } => {
                self.host_theme_updates.retain(|current| {
                    !matches!(
                        current,
                        ClientHostThemeUpdate::DefaultColor {
                            kind: current_kind,
                            ..
                        } if current_kind == kind
                    )
                });
            }
            ClientHostThemeUpdate::PaletteColors(_) => self
                .host_theme_updates
                .retain(|current| !matches!(current, ClientHostThemeUpdate::PaletteColors(_))),
            ClientHostThemeUpdate::Appearance(_) => self
                .host_theme_updates
                .retain(|current| !matches!(current, ClientHostThemeUpdate::Appearance(_))),
        }
        self.host_theme_updates.push(update.clone());
    }

    /// Replay the retained physical-host baseline only after an endpoint owns the committed
    /// presentation. The endpoint transport preserves this order ahead of the resync control.
    pub(super) fn replay_host_theme(
        &self,
        endpoints: &mut endpoint::EndpointRegistry,
        endpoint_id: &endpoint::ClientEndpointId,
    ) {
        for update in &self.host_theme_updates {
            let _ = endpoints.send_to(
                endpoint_id,
                &crate::protocol::ClientMessage::ClientShellHostTheme {
                    update: update.clone(),
                },
            );
        }
    }

    pub(super) fn unfreeze_presentation(&mut self) {
        self.presentation_frozen = false;
        // 解冻必排空：冻结前暂存、冻结期间没有帧可搭车的图形清理在这里送出，
        // 不依赖调用方纪律。
        self.flush_pending_graphics();
        // A resize or metadata event may have happened while frozen. Force a full frame rather
        // than attempting to patch the old source frame.
        self.request_repaint();
    }

    /// Present a composed error/chrome frame while retaining the handoff input freeze. The pane
    /// cells are still the last coherent surface; only client chrome (including the error) moves.
    pub(super) fn present_frozen_chrome(
        &mut self,
        frame_data: impl Into<frame_output::ComposedFrame>,
    ) {
        let frozen = self.presentation_frozen;
        // Chrome can repaint the frozen source, but cannot retire its staged images.
        let deferred_cleanup = frozen.then(|| std::mem::take(&mut self.pending_native_cleanup));
        self.presentation_frozen = false;
        self.present_frame(frame_data);
        self.presentation_frozen = frozen;
        if let Some(cleanup) = deferred_cleanup {
            self.pending_native_cleanup = cleanup;
        }
    }

    #[cfg(unix)]
    pub(super) fn queue_native_image_cleanup(&mut self, image_id: u32) {
        crate::kitty_graphics::encode_delete_image(&mut self.pending_native_cleanup, image_id);
    }

    /// Retirement is lifecycle bookkeeping, not a presentation effect. The caller has
    /// already checked the endpoint generation, even for inactive/frozen owners.
    #[cfg(unix)]
    pub(super) fn receive_graphics_retirement(
        &mut self,
        endpoint_id: &endpoint::ClientEndpointId,
        generation: u64,
        transfer_id: u64,
        image_id: u32,
        owner_active: bool,
    ) {
        if transfer_id & crate::kitty_graphics::surface::NATIVE_TRANSFER_BIT != 0 {
            self.disabled_native_graphics
                .insert(endpoint_id.clone(), generation);
        }
        self.record_retired_direct_graphics(endpoint_id.clone(), generation, transfer_id, image_id);
        let upload_pending = self
            .pending_surface_graphics
            .remove(&(endpoint_id.clone(), generation, transfer_id, image_id))
            .is_some();
        // Retirement cleanup is terminal-owned. A native retirement owns the
        // ID only while its exact upload is pending: collision rejection and a
        // late post-ACK retirement must not delete the currently visible bank.
        // API/non-native retirement retains its prior ownership behavior.
        let owns_image = self.queue_retired_graphics_cleanup(
            transfer_id,
            image_id,
            upload_pending,
            owner_active,
        );
        let frame = if owns_image && owner_active {
            let frozen = self.presentation_frozen;
            let size = self.reported_size;
            self.shell.as_mut().and_then(|shell| {
                shell.retire_direct_graphics_image(image_id);
                (!frozen).then(|| shell.compose(size.0, size.1)).flatten()
            })
        } else {
            None
        };
        // A retirement repaint is a normal synchronized frame. In particular,
        // do not extract its graphics into a graphics-only swap.
        if let Some(frame) = frame {
            self.present_frame(frame);
        }
        if let Ok(mut matcher) = self.direct_graphics_response.lock() {
            matcher.retire(transfer_id);
        }
    }

    /// Returns whether a retirement owns an image that may be removed from the terminal/cache.
    /// Native rejections can retire a proposed colliding ID without ever uploading it; only an
    /// exact still-pending native upload gives that retirement ownership of the image.
    #[cfg(unix)]
    pub(super) fn queue_retired_graphics_cleanup(
        &mut self,
        transfer_id: u64,
        image_id: u32,
        upload_pending: bool,
        owner_active: bool,
    ) -> bool {
        let native = transfer_id & crate::kitty_graphics::surface::NATIVE_TRANSFER_BIT != 0;
        if native && !upload_pending {
            return false;
        }
        if native || !owner_active {
            self.queue_native_image_cleanup(image_id);
        }
        true
    }

    #[cfg(unix)]
    pub(super) fn flush_native_cleanup(
        &mut self,
        writer: &mut impl std::io::Write,
    ) -> io::Result<()> {
        if self.presentation_frozen || self.pending_native_cleanup.is_empty() {
            return Ok(());
        }
        writer.write_all(&self.pending_native_cleanup)?;
        writer.flush()?;
        self.pending_native_cleanup.clear();
        Ok(())
    }

    /// 暂存图形清理，等本轮的 `present_frame` / `present_surface_patch` 把它并进同步块；
    /// 本轮没有帧时调用方用 [`Self::flush_pending_graphics`] 单独包一个同步块送出。
    pub(super) fn present_graphics(&mut self, graphics: &[u8]) {
        if self.presentation_frozen || graphics.is_empty() || !self.kitty_graphics_enabled {
            return;
        }
        self.pending_graphics.extend_from_slice(graphics);
    }

    /// 没有帧可搭车时，把暂存的图形清理包在自己的同步块里立即送出。
    pub(super) fn flush_pending_graphics(&mut self) {
        if self.presentation_frozen || self.pending_graphics.is_empty() {
            return;
        }
        let graphics = std::mem::take(&mut self.pending_graphics);
        let mut stdout = io::stdout();
        let _ = write_standalone_graphics(&mut stdout, &graphics);
        let _ = stdout.flush();
    }

    /// 本次呈现要带上的图形字节：先前暂存的清理在前，本帧图形在后。暂存为空（常态）
    /// 时零拷贝借用本帧切片，热路径不新增分配。
    fn take_graphics_for_presentation<'frame>(
        &mut self,
        frame_graphics: &'frame [u8],
    ) -> Cow<'frame, [u8]> {
        merge_presentation_graphics(&mut self.pending_graphics, frame_graphics)
    }

    pub(super) fn present_surface_patch(
        &mut self,
        patch: shell::ClientComposedSurfacePatch,
    ) -> io::Result<bool> {
        if self.presentation_frozen
            || self.repaint_pending
            || (self.kitty_graphics_enabled && !self.pending_native_cleanup.is_empty())
        {
            crate::render_prof::event("client_surface_patch.fallback.repaint");
            return Ok(false);
        }
        let rows = if self.draw_host_cursor {
            let Some(rows) = self
                .blit_encoder
                .patch_rows_with_drawn_cursor(&patch.rows, patch.cursor.as_ref())
            else {
                crate::render_prof::event("client_surface_patch.fallback.drawn_cursor");
                return Ok(false);
            };
            rows
        } else {
            patch.rows
        };
        let encode_started = crate::render_prof::timer();
        let Some(encoded) =
            self.blit_encoder
                .encode_patch(&rows, patch.cursor.clone(), self.draw_host_cursor)
        else {
            crate::render_prof::event("client_surface_patch.fallback.encode");
            return Ok(false);
        };
        crate::render_prof::duration_since("client_surface_patch.encode", encode_started);
        let write_started = crate::render_prof::timer();
        let graphics = self.take_graphics_for_presentation(&[]);
        if !encoded.bytes.is_empty() {
            let mut stdout = io::stdout();
            write_encoded_frame_with_graphics(&mut stdout, &encoded.bytes, &graphics)?;
            stdout.flush()?;
        } else if !graphics.is_empty() {
            // 补丁本身为空时，暂存的图形清理仍包在自己的同步块里送出。
            let mut stdout = io::stdout();
            write_standalone_graphics(&mut stdout, &graphics)?;
            stdout.flush()?;
        }
        crate::render_prof::duration_since("client_surface_patch.write", write_started);
        let committed = self.blit_encoder.commit_patch(&rows, patch.cursor, encoded);
        crate::render_prof::event(if committed {
            "client_surface_patch.success"
        } else {
            "client_surface_patch.fallback.commit"
        });
        Ok(committed)
    }

    #[cfg(unix)]
    fn retire_pending_endpoint_graphics(
        &mut self,
        endpoint_id: &endpoint::ClientEndpointId,
        generation: Option<u64>,
    ) {
        let retired = self
            .pending_surface_graphics
            .keys()
            .filter(|(owner, pending_generation, _, _)| {
                owner == endpoint_id
                    && generation.is_none_or(|generation| *pending_generation == generation)
            })
            .cloned()
            .collect::<Vec<_>>();
        let mut transfer_ids = Vec::with_capacity(retired.len());
        for key in retired {
            let (_, _, transfer_id, image_id) = &key;
            if transfer_id & crate::kitty_graphics::surface::NATIVE_TRANSFER_BIT != 0 {
                self.queue_native_image_cleanup(*image_id);
            }
            transfer_ids.push(*transfer_id);
            self.pending_surface_graphics.remove(&key);
        }
        if let Ok(mut matcher) = self.direct_graphics_response.lock() {
            for transfer_id in transfer_ids {
                matcher.retire(transfer_id);
            }
        }
    }

    #[cfg(unix)]
    pub(super) fn record_retired_direct_graphics(
        &mut self,
        endpoint_id: endpoint::ClientEndpointId,
        generation: u64,
        transfer_id: u64,
        image_id: u32,
    ) {
        let retired = self
            .retired_direct_graphics
            .entry(endpoint_id)
            .or_insert_with(|| RetiredDirectGraphics::new(generation));
        if retired.generation != generation {
            *retired = RetiredDirectGraphics::new(generation);
        }
        if retired.saturated || retired.transfers.contains(&(transfer_id, image_id)) {
            return;
        }
        if retired.transfers.len() == MAX_RETIRED_DIRECT_GRAPHICS {
            // Never evict an older tombstone: a delayed file for an evicted tuple could otherwise
            // overwrite a visible image. Reject every unrecognized file until generation reset.
            retired.saturated = true;
            return;
        }
        retired.transfers.push((transfer_id, image_id));
    }

    #[cfg(unix)]
    pub(super) fn match_retired_direct_graphics(
        &mut self,
        endpoint_id: &endpoint::ClientEndpointId,
        generation: u64,
        transfer_id: u64,
        image_id: u32,
    ) -> RetiredDirectGraphicsMatch {
        let Some(retired) = self.retired_direct_graphics.get_mut(endpoint_id) else {
            return RetiredDirectGraphicsMatch::None;
        };
        if retired.generation != generation {
            return RetiredDirectGraphicsMatch::None;
        }
        if let Some(index) = retired
            .transfers
            .iter()
            .position(|transfer| *transfer == (transfer_id, image_id))
        {
            retired.transfers.swap_remove(index);
            if retired.transfers.is_empty() && !retired.saturated {
                self.retired_direct_graphics.remove(endpoint_id);
            }
            return RetiredDirectGraphicsMatch::Exact;
        }
        if retired.saturated {
            RetiredDirectGraphicsMatch::Saturated
        } else {
            RetiredDirectGraphicsMatch::None
        }
    }

    /// Forget graphics guards only for the connection generation which actually disconnected.
    /// A delayed disconnect from an older generation must not clear its replacement's guards.
    #[cfg(unix)]
    pub(super) fn retire_endpoint_graphics(
        &mut self,
        endpoint_id: &endpoint::ClientEndpointId,
        generation: u64,
    ) {
        self.retire_pending_endpoint_graphics(endpoint_id, Some(generation));
        if self.disabled_native_graphics.get(endpoint_id) == Some(&generation) {
            self.disabled_native_graphics.remove(endpoint_id);
        }
        if self
            .retired_direct_graphics
            .get(endpoint_id)
            .is_some_and(|retired| retired.generation == generation)
        {
            self.retired_direct_graphics.remove(endpoint_id);
        }
    }

    /// Reset bounded per-endpoint state when a replacement connection becomes authoritative.
    #[cfg(unix)]
    pub(super) fn start_endpoint_graphics_generation(
        &mut self,
        endpoint_id: &endpoint::ClientEndpointId,
        generation: u64,
    ) {
        self.retire_pending_endpoint_graphics(endpoint_id, None);
        if self.disabled_native_graphics.get(endpoint_id) != Some(&generation) {
            self.disabled_native_graphics.remove(endpoint_id);
        }
        if self
            .retired_direct_graphics
            .get(endpoint_id)
            .is_some_and(|retired| retired.generation != generation)
        {
            self.retired_direct_graphics.remove(endpoint_id);
        }
    }

    #[cfg(unix)]
    pub(super) fn forget_endpoint_graphics(&mut self, endpoint_id: &endpoint::ClientEndpointId) {
        self.retire_pending_endpoint_graphics(endpoint_id, None);
        self.disabled_native_graphics.remove(endpoint_id);
        self.retired_direct_graphics.remove(endpoint_id);
    }

    pub(super) fn present_frame(&mut self, frame_data: impl Into<frame_output::ComposedFrame>) {
        let _ = self.try_present_frame(frame_data);
    }

    fn write_composed_output(
        &mut self,
        writer: &mut impl io::Write,
        encoded: &[u8],
        mut graphics: crate::kitty_graphics::GraphicsOutput,
    ) -> io::Result<()> {
        if self.kitty_graphics_enabled {
            if !self.pending_native_cleanup.is_empty() {
                graphics.operations.insert(
                    0,
                    crate::kitty_graphics::GraphicsOperation::Bytes(
                        self.pending_native_cleanup.clone(),
                    ),
                );
            }
            // fork：`present_graphics` 暂存的图形清理排在最前，与本帧同一同步块到达。
            if !self.pending_graphics.is_empty() {
                graphics.operations.insert(
                    0,
                    crate::kitty_graphics::GraphicsOperation::Bytes(self.pending_graphics.clone()),
                );
            }
            frame_output::write_composed_frame(
                writer.by_ref(),
                encoded,
                &graphics,
                &mut self.image_files,
            )?;
        } else {
            writer.write_all(encoded)?;
        }
        writer.flush()?;
        if self.kitty_graphics_enabled {
            self.pending_native_cleanup.clear();
            self.pending_graphics.clear();
        }
        Ok(())
    }

    /// Presents and commits a frame only after all terminal output has been written successfully.
    /// Callers which acknowledge presentation-sensitive work use the return value rather than
    /// treating composition as presentation.
    pub(super) fn try_present_frame(
        &mut self,
        frame_data: impl Into<frame_output::ComposedFrame>,
    ) -> bool {
        if self.presentation_frozen {
            return false;
        }
        let frame_output::ComposedFrame {
            frame: frame_data,
            graphics,
        } = frame_data.into();
        // 帧与上次提交的完全相同、且没有待重绘标记与待送出的图形清理时直接跳过：
        // server 端 `render_stream` 一直有这条短路，客户端此前每次都要整帧 diff
        // 加两次 syscall（CFP-10）。画主机光标时不短路——光标位置可能单独变化。
        if !self.repaint_pending
            && !self.draw_host_cursor
            && graphics.is_empty()
            && self.pending_graphics.is_empty()
            && (!self.kitty_graphics_enabled || self.pending_native_cleanup.is_empty())
            && self.blit_encoder.is_current(&frame_data)
        {
            return true;
        }
        let frame_data = if self.draw_host_cursor {
            render_ansi::frame_with_drawn_cursor(frame_data)
        } else {
            frame_data
        };
        let encoded = if self.draw_host_cursor {
            self.blit_encoder
                .encode_with_suppressed_visible_cursor(&frame_data, self.repaint_pending)
        } else {
            self.blit_encoder.encode(&frame_data, self.repaint_pending)
        };
        let mut stdout = io::stdout();
        if let Err(error) = self.write_composed_output(&mut stdout, &encoded.bytes, graphics) {
            tracing::warn!(%error, "failed to present client frame");
            self.repaint_pending = true;
            return false;
        }
        self.blit_encoder.commit(frame_data, encoded);
        self.repaint_pending = false;
        true
    }
}

#[cfg(all(test, unix))]
mod native_cleanup_tests {
    use super::*;

    #[test]
    fn retired_graphics_tombstones_survive_handoff_and_only_exact_files_consume_them() {
        let mut state = ClientState::test_new();
        let endpoint = endpoint::ClientEndpointId::Local;
        state.record_retired_direct_graphics(endpoint.clone(), 7, 11, 1234);
        state.record_retired_direct_graphics(endpoint.clone(), 7, 12, 5678);
        state.disabled_native_graphics.insert(endpoint.clone(), 7);

        // Switching away and back does not reset guards, and an unrelated queued file cannot
        // consume either of two sequential API retirement tombstones.
        assert_eq!(
            state.match_retired_direct_graphics(&endpoint, 7, 13, 9999),
            RetiredDirectGraphicsMatch::None
        );
        assert_eq!(
            state
                .retired_direct_graphics
                .get(&endpoint)
                .unwrap()
                .transfers
                .len(),
            2
        );
        assert_eq!(state.disabled_native_graphics.get(&endpoint), Some(&7));
        assert_eq!(
            state.match_retired_direct_graphics(&endpoint, 7, 11, 1234),
            RetiredDirectGraphicsMatch::Exact
        );
        assert!(state
            .retired_direct_graphics
            .get(&endpoint)
            .unwrap()
            .transfers
            .contains(&(12, 5678)));
        assert_eq!(
            state.match_retired_direct_graphics(&endpoint, 7, 12, 5678),
            RetiredDirectGraphicsMatch::Exact
        );
        assert!(!state.retired_direct_graphics.contains_key(&endpoint));
    }

    #[test]
    fn replacement_generation_resets_bounded_graphics_guards() {
        let mut state = ClientState::test_new();
        let endpoint = endpoint::ClientEndpointId::Local;
        let native = crate::kitty_graphics::surface::NATIVE_TRANSFER_BIT | 11;
        state.record_retired_direct_graphics(endpoint.clone(), 7, native, 1234);
        state.disabled_native_graphics.insert(endpoint.clone(), 7);

        state.start_endpoint_graphics_generation(&endpoint, 8);

        assert_eq!(
            state.match_retired_direct_graphics(&endpoint, 7, native, 1234),
            RetiredDirectGraphicsMatch::None
        );
        assert!(!state.disabled_native_graphics.contains_key(&endpoint));
    }

    #[test]
    fn retired_graphics_tombstone_overflow_fails_closed_until_generation_reset() {
        let mut state = ClientState::test_new();
        let endpoint = endpoint::ClientEndpointId::Local;
        for transfer_id in 0..=MAX_RETIRED_DIRECT_GRAPHICS as u64 {
            state.record_retired_direct_graphics(
                endpoint.clone(),
                7,
                transfer_id,
                transfer_id as u32,
            );
        }
        let retired = state.retired_direct_graphics.get(&endpoint).unwrap();
        assert_eq!(retired.transfers.len(), MAX_RETIRED_DIRECT_GRAPHICS);
        assert!(retired.saturated);
        assert_eq!(
            state.match_retired_direct_graphics(&endpoint, 7, 1, 1),
            RetiredDirectGraphicsMatch::Exact
        );
        assert_eq!(
            state.match_retired_direct_graphics(&endpoint, 7, 10_000, 10_000),
            RetiredDirectGraphicsMatch::Saturated
        );
        assert!(
            state
                .retired_direct_graphics
                .get(&endpoint)
                .unwrap()
                .saturated
        );

        state.start_endpoint_graphics_generation(&endpoint, 8);
        assert!(!state.retired_direct_graphics.contains_key(&endpoint));
        state.record_retired_direct_graphics(endpoint.clone(), 8, 1, 2);
        assert_eq!(
            state.match_retired_direct_graphics(&endpoint, 8, 9, 9),
            RetiredDirectGraphicsMatch::None
        );
        assert_eq!(
            state.match_retired_direct_graphics(&endpoint, 8, 1, 2),
            RetiredDirectGraphicsMatch::Exact
        );
    }

    #[test]
    fn pending_none_native_retirement_does_not_touch_cleanup_or_shell_cache() {
        let mut state = ClientState::test_new();
        state.pending_native_cleanup.extend_from_slice(b"existing");
        let transfer = crate::kitty_graphics::surface::NATIVE_TRANSFER_BIT | 7;

        assert!(!state.queue_retired_graphics_cleanup(transfer, 1234, false, true));
        assert_eq!(state.pending_native_cleanup, b"existing");
        assert!(state
            .shell
            .as_mut()
            .expect("test shell")
            .take_pending_graphics_cleanup()
            .is_empty());
    }

    #[test]
    fn queued_cleanup_is_synchronized_and_respects_graphics_capability() {
        let mut state = ClientState::test_new();
        state.kitty_graphics_enabled = true;
        state.queue_native_image_cleanup(42);
        let encoded = b"\x1b[?2026htext\x1b[?2026l";
        let mut output = Vec::new();
        state
            .write_composed_output(&mut output, encoded, Default::default())
            .unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.find("\x1b[?2026h").unwrap() < text.find("a=d,d=I,i=42").unwrap());
        assert!(text.find("a=d,d=I,i=42").unwrap() < text.find("\x1b[?2026l").unwrap());
        assert!(state.pending_native_cleanup.is_empty());
        state.kitty_graphics_enabled = false;
        state.queue_native_image_cleanup(43);
        let mut output = Vec::new();
        state
            .write_composed_output(&mut output, encoded, Default::default())
            .unwrap();
        assert_eq!(output, encoded);
        assert!(!state.pending_native_cleanup.is_empty());
        state.kitty_graphics_enabled = true;
        let mut no_capacity = &mut [][..];
        assert!(state
            .write_composed_output(&mut no_capacity, encoded, Default::default())
            .is_err());
        assert!(!state.pending_native_cleanup.is_empty());
    }

    #[test]
    fn active_api_retirement_leaves_cleanup_to_composition() {
        let mut state = ClientState::test_new();
        assert!(state.queue_retired_graphics_cleanup(7, 1234, true, true));
        assert!(state.pending_native_cleanup.is_empty());
        assert!(state.queue_retired_graphics_cleanup(8, 5678, true, false));
        assert!(!state.pending_native_cleanup.is_empty());
    }

    #[test]
    fn unacknowledged_native_upload_cleanup_survives_frozen_disconnect() {
        let mut state = ClientState::test_new();
        let endpoint = endpoint::ClientEndpointId::Local;
        let key = crate::protocol::SurfaceGraphicsAssetKey {
            source: crate::protocol::SurfaceGraphicsSource::Terminal {
                target: crate::protocol::SurfaceGraphicsTarget::Pane {
                    pane_id: "p".into(),
                },
                image_id: 1,
            },
            image_width: 1,
            image_height: 1,
            format: crate::protocol::SurfaceGraphicsFormat::Rgba,
            data_len: 4,
            data_fingerprint: 1,
        };
        let transfer = crate::kitty_graphics::surface::NATIVE_TRANSFER_BIT | 2;
        state
            .pending_surface_graphics
            .insert((endpoint.clone(), 9, transfer, 1234), key);
        state.presentation_frozen = true;
        state.retire_endpoint_graphics(&endpoint, 9);
        assert!(state.pending_surface_graphics.is_empty());
        let mut output = Vec::new();
        state.flush_native_cleanup(&mut output).unwrap();
        assert!(output.is_empty());
        assert!(!state.pending_native_cleanup.is_empty());
        state.unfreeze_presentation();
        state.flush_native_cleanup(&mut output).unwrap();
        assert!(String::from_utf8(output.clone())
            .unwrap()
            .contains("a=d,d=I,i=1234"));
        state.flush_native_cleanup(&mut output).unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap().matches("a=d,").count(),
            1
        );
    }

    #[test]
    fn unacknowledged_native_upload_retirement_survives_frozen_handoff() {
        let mut state = ClientState::test_new();
        let endpoint = endpoint::ClientEndpointId::Local;
        let key = crate::protocol::SurfaceGraphicsAssetKey {
            source: crate::protocol::SurfaceGraphicsSource::Terminal {
                target: crate::protocol::SurfaceGraphicsTarget::Pane {
                    pane_id: "p".into(),
                },
                image_id: 1,
            },
            image_width: 1,
            image_height: 1,
            format: crate::protocol::SurfaceGraphicsFormat::Rgba,
            data_len: 4,
            data_fingerprint: 1,
        };
        let transfer = crate::kitty_graphics::surface::NATIVE_TRANSFER_BIT | 2;
        state
            .pending_surface_graphics
            .insert((endpoint.clone(), 9, transfer, 1234), key);
        state.presentation_frozen = true;
        // The source connection is still live, but no longer owns the active shell.
        state.receive_graphics_retirement(&endpoint, 8, transfer, 1234, false);
        assert_eq!(state.pending_surface_graphics.len(), 1);
        assert!(state.pending_native_cleanup.is_empty());
        state.receive_graphics_retirement(&endpoint, 9, transfer, 1234, false);
        assert_eq!(
            state.match_retired_direct_graphics(&endpoint, 9, transfer, 1234),
            RetiredDirectGraphicsMatch::Exact
        );
        assert!(state
            .shell
            .as_mut()
            .unwrap()
            .take_pending_graphics_cleanup()
            .is_empty());
        assert!(state.pending_surface_graphics.is_empty());
        let mut output = Vec::new();
        state.flush_native_cleanup(&mut output).unwrap();
        assert!(output.is_empty());
        assert!(!state.pending_native_cleanup.is_empty());
        let cleanup = state.pending_native_cleanup.clone();
        let frame = crate::protocol::FrameData::from_ratatui_buffer(
            &ratatui::buffer::Buffer::empty(ratatui::layout::Rect::new(0, 0, 1, 1)),
            None,
        );
        state.kitty_graphics_enabled = true;
        assert!(!state.try_present_frame(frame.clone()));
        assert_eq!(state.pending_native_cleanup, cleanup);
        state.present_frozen_chrome(frame);
        assert!(state.presentation_frozen);
        assert_eq!(state.pending_native_cleanup, cleanup);
        state.unfreeze_presentation();
        state.flush_native_cleanup(&mut output).unwrap();
        assert!(String::from_utf8(output.clone())
            .unwrap()
            .contains("a=d,d=I,i=1234"));
        state.flush_native_cleanup(&mut output).unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap().matches("a=d,").count(),
            1
        );
    }
}

/// 把暂存的图形清理与本帧图形合成一次写出的字节；暂存区被清空。暂存为空时借用
/// 本帧切片（零拷贝），只有暂存非空的罕见路径才付一次拼接。
fn merge_presentation_graphics<'frame>(
    pending: &mut Vec<u8>,
    frame_graphics: &'frame [u8],
) -> Cow<'frame, [u8]> {
    if pending.is_empty() {
        return Cow::Borrowed(frame_graphics);
    }
    let mut graphics = std::mem::take(pending);
    graphics.extend_from_slice(frame_graphics);
    Cow::Owned(graphics)
}

#[cfg(test)]
pub(super) fn test_client_state() -> ClientState {
    ClientState::test_new()
}

#[cfg(test)]
mod tests {
    use super::merge_presentation_graphics;
    use std::borrow::Cow;

    #[test]
    fn pending_cleanup_precedes_frame_graphics_and_drains_the_buffer() {
        let mut pending = b"<cleanup>".to_vec();
        let merged = merge_presentation_graphics(&mut pending, b"<frame>");
        assert!(matches!(merged, Cow::Owned(_)), "暂存非空才拼接");
        assert_eq!(merged.as_ref(), b"<cleanup><frame>");
        assert!(pending.is_empty());

        // 常态（暂存为空）：零拷贝借用本帧图形，不分配。
        let merged = merge_presentation_graphics(&mut pending, b"<frame>");
        assert!(matches!(merged, Cow::Borrowed(_)), "暂存为空时零拷贝");
        assert_eq!(merged.as_ref(), b"<frame>");
        assert!(merge_presentation_graphics(&mut pending, b"").is_empty());
    }
}
