//! 投影改戳：纯 chrome 刷新让快照修订号前进时，把已提交的 pane surface 改戳到
//! 新修订号重发，不重新渲染 pane。
//!
//! 客户端只画与快照修订号精确配对的 surface（workbench 下不配对就画「正在同步
//! 终端…」），所以修订号前进后必须补一帧；但 chrome 变化不改 pane 画面，补的
//! 这一帧沿用已提交基线即可——协商了复用编码的连接只发非单元格部分。只有基线
//! 不适用（缺基线、boot / 几何不一致、popup、图形）时才由调用方回退整帧渲染。

use super::*;

/// 单个 surface 基线能否直接改戳。
enum RestampBaseline {
    /// 基线已是目标修订号，不必重发。
    Current,
    /// 可以改戳重发。
    Restamp(Box<crate::server::render_stream::PreparedRender>),
    /// 基线不适用，需要整帧渲染重建。
    Unusable,
}

/// 单个客户端的改戳结局。
enum RestampOutcome {
    /// 已发出，或无需 / 暂不能发（已有写端排空后的整帧恢复会带上新修订号）。
    Handled,
    /// 有基线不适用，需要本 tick 补整帧渲染。
    NeedsFullRender,
    /// 写端已断开。
    Disconnected,
}

fn restamp_baseline(
    state: &crate::server::render_stream::ClientRenderState,
    boot_id: &str,
    projection_revision: u64,
    cols: u16,
    rows: u16,
) -> RestampBaseline {
    let Some(surface) = state.last_pane_surface() else {
        crate::render_prof::event("projection_restamp.fallback.no_baseline");
        return RestampBaseline::Unusable;
    };
    if surface.boot_id != boot_id
        || surface.frame.width != cols
        || surface.frame.height != rows
        || surface.popup.is_some()
        || !surface.graphics.assets.is_empty()
        || !surface.graphics.placements.is_empty()
        || !surface.graphics.retained_assets.is_empty()
        || !surface.frame.graphics.is_empty()
    {
        crate::render_prof::event("projection_restamp.fallback.baseline_mismatch");
        return RestampBaseline::Unusable;
    }
    if surface.projection_revision == projection_revision {
        return RestampBaseline::Current;
    }
    match state.prepare_projection_restamp(projection_revision) {
        Some(prepared) => RestampBaseline::Restamp(Box::new(prepared)),
        None => RestampBaseline::Unusable,
    }
}

impl HeadlessServer {
    /// 把快照修订号前进的客户端（`(client_id, 新修订号)`）已提交的 surface 改戳
    /// 重发。返回 `false` 表示至少一个客户端的基线不适用，调用方需补整帧渲染。
    pub(super) fn restamp_client_shell_surfaces(&mut self, advanced: &[(u64, u64)]) -> bool {
        if self.app.full_redraw_pending || self.app.state.popup_pane.is_some() {
            crate::render_prof::event("projection_restamp.fallback.unsafe_state");
            return false;
        }
        let mut complete = true;
        let mut broken_clients = Vec::new();
        for &(client_id, projection_revision) in advanced {
            match self.restamp_client_shell_surface(client_id, projection_revision) {
                RestampOutcome::Handled => {}
                RestampOutcome::NeedsFullRender => complete = false,
                RestampOutcome::Disconnected => broken_clients.push(client_id),
            }
        }
        for client_id in broken_clients {
            self.remove_client_and_resize_if_needed(client_id);
        }
        complete
    }

    fn restamp_client_shell_surface(
        &mut self,
        client_id: u64,
        projection_revision: u64,
    ) -> RestampOutcome {
        let boot_id = self.client_shell_boot_id.as_str();
        let Some(client) = self.clients.get_mut(&client_id) else {
            return RestampOutcome::Handled;
        };
        if client.deferred_render() != DeferredRender::None {
            // 写端排空后的整帧恢复已经排着，那一帧会带上新修订号。
            crate::render_prof::event("projection_restamp.skip.deferred");
            return RestampOutcome::Handled;
        }
        let Some(writer) = client.writer.clone() else {
            return RestampOutcome::Handled;
        };
        // 多视图连接的各 view 合成一次写入（与 `render_client_views` 一致）。
        let mut batch = Vec::new();
        let mut prepared_views = Vec::new();
        let mut prepared_shell = None;
        if let Some(views) = client.views.as_ref() {
            for (index, view) in views.views.iter().enumerate() {
                // 标签已不存在的 view 整帧渲染同样跳过，这里也不重发它的旧基线。
                if self.app.parse_tab_id(&view.spec.tab_id).is_none() {
                    continue;
                }
                let prepared = match restamp_baseline(
                    &view.render_state,
                    boot_id,
                    projection_revision,
                    view.spec.cols,
                    view.spec.rows,
                ) {
                    RestampBaseline::Current => continue,
                    RestampBaseline::Restamp(prepared) => *prepared,
                    RestampBaseline::Unusable => return RestampOutcome::NeedsFullRender,
                };
                let Ok(framed) = protocol::views::message(
                    boot_id,
                    views.revision,
                    &view.spec.view_id,
                    &view.spec.tab_id,
                    prepared.message(),
                )
                .map_err(io::Error::other)
                .and_then(|message| {
                    Self::frame_server_message_with_max(&message, MAX_GRAPHICS_FRAME_SIZE)
                        .map_err(io::Error::other)
                }) else {
                    return RestampOutcome::NeedsFullRender;
                };
                if batch.len().saturating_add(framed.len()) > MAX_GRAPHICS_FRAME_SIZE {
                    return RestampOutcome::NeedsFullRender;
                }
                batch.extend_from_slice(&framed);
                prepared_views.push((index, prepared));
            }
        } else {
            let (cols, rows) = client.terminal_size;
            let prepared = match restamp_baseline(
                &client.render_state,
                boot_id,
                projection_revision,
                cols,
                rows,
            ) {
                RestampBaseline::Current => return RestampOutcome::Handled,
                RestampBaseline::Restamp(prepared) => *prepared,
                RestampBaseline::Unusable => return RestampOutcome::NeedsFullRender,
            };
            let Ok(framed) = Self::frame_server_message(prepared.message()) else {
                return RestampOutcome::NeedsFullRender;
            };
            batch = framed;
            prepared_shell = Some(prepared);
        }
        if batch.is_empty() {
            return RestampOutcome::Handled;
        }
        crate::render_prof::counter("projection_restamp.bytes", batch.len() as u64);
        match writer.render.try_send(batch) {
            Ok(()) => {
                if let Some(prepared) = prepared_shell {
                    client.render_state.commit_sent_frame(prepared);
                }
                if let Some(views) = client.views.as_mut() {
                    for (index, prepared) in prepared_views {
                        if let Some(view) = views.views.get_mut(index) {
                            view.render_state.commit_sent_frame(prepared);
                        }
                    }
                }
                crate::render_prof::event("projection_restamp.sent");
                RestampOutcome::Handled
            }
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                // 与整帧路径一致：写端排空事件触发整帧恢复。
                client.defer_full_render();
                crate::render_prof::event("projection_restamp.deferred");
                RestampOutcome::Handled
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => RestampOutcome::Disconnected,
        }
    }
}
