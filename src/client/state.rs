use std::borrow::Cow;

use super::*;

/// State tracking for the thin client.
pub(super) struct ClientState {
    /// Stateful semantic-frame encoder used when the server sends FrameData.
    pub(super) blit_encoder: render_ansi::BlitEncoder,
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
    pub(super) retired_direct_graphics: Option<(endpoint::ClientEndpointId, u64, u32)>,
    #[cfg(unix)]
    pub(super) pending_surface_graphics:
        HashMap<(endpoint::ClientEndpointId, u64, u32), crate::protocol::SurfaceGraphicsAssetKey>,
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
    pub(super) fn request_repaint(&mut self) {
        self.repaint_pending = true;
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
    pub(super) fn present_frozen_chrome(&mut self, frame_data: FrameData) {
        let frozen = self.presentation_frozen;
        self.presentation_frozen = false;
        self.present_frame(frame_data);
        self.presentation_frozen = frozen;
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
        if self.presentation_frozen || self.repaint_pending {
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
        let mut stdout = io::stdout();
        write_encoded_frame_with_graphics(&mut stdout, &encoded.bytes, &graphics)?;
        stdout.flush()?;
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
    pub(super) fn retire_endpoint_graphics(&mut self, endpoint_id: &endpoint::ClientEndpointId) {
        let transfer_ids = self
            .pending_surface_graphics
            .keys()
            .filter(|(owner, _, _)| owner == endpoint_id)
            .map(|(_, transfer_id, _)| *transfer_id)
            .collect::<Vec<_>>();
        self.pending_surface_graphics
            .retain(|(owner, _, _), _| owner != endpoint_id);
        if self
            .retired_direct_graphics
            .as_ref()
            .is_some_and(|(owner, _, _)| owner == endpoint_id)
        {
            self.retired_direct_graphics = None;
        }
        if let Ok(mut matcher) = self.direct_graphics_response.lock() {
            for transfer_id in transfer_ids {
                matcher.retire(transfer_id);
            }
        }
    }

    pub(super) fn present_frame(&mut self, frame_data: FrameData) {
        if self.presentation_frozen {
            return;
        }
        // 帧与上次提交的完全相同、且没有待重绘标记时直接跳过：server 端
        // `render_stream` 一直有这条短路，客户端此前每次都要整帧 diff 加两次
        // syscall（CFP-10）。画主机光标时不短路——光标位置可能单独变化。
        if !self.repaint_pending
            && !self.draw_host_cursor
            && frame_data.graphics.is_empty()
            && self.blit_encoder.is_current(&frame_data)
        {
            return;
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
        let frame_graphics = if self.kitty_graphics_enabled {
            frame_data.graphics.as_slice()
        } else {
            &[]
        };
        let graphics = self.take_graphics_for_presentation(frame_graphics);
        let _ = write_encoded_frame_with_graphics(&mut stdout, &encoded.bytes, &graphics);
        let _ = stdout.flush();
        self.blit_encoder.commit(frame_data, encoded);
        self.repaint_pending = false;
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
    ClientState {
        blit_encoder: render_ansi::BlitEncoder::new(),
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
        retired_direct_graphics: None,
        #[cfg(unix)]
        pending_surface_graphics: HashMap::new(),
        attach_escape: None,
        #[cfg(unix)]
        mouse_scroll_lines: 3,
        remote_image_paste_key: None,
        redraw_on_focus_gained: false,
        repaint_pending: false,
        presentation_frozen: false,
        draw_host_cursor: false,
        detached_process_children: Vec::new(),
        shell: Some(shell::ClientShellState::new(
            shell::ClientShellConfig::from_config(&crate::config::Config::default()),
        )),
        pending_graphics: Vec::new(),
    }
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
