use super::*;
use crate::api::schema::{ClientViewsSetParams, ResponseResult};
use crate::server::clients::{ClientView, MultiViewState};

impl HeadlessServer {
    pub(super) fn set_client_views(
        &mut self,
        client_id: u64,
        params: ClientViewsSetParams,
    ) -> Result<bool, String> {
        let client = self.clients.get(&client_id).ok_or("客户端已断开")?;
        if !client.is_active_shell_client() || params.views.len() > 32 {
            return Err("视图数量无效或客户端未激活".into());
        }
        if client
            .views
            .as_ref()
            .is_some_and(|views| params.revision <= views.revision)
            || params.revision == 0
        {
            return Err("视图版本已经过期".into());
        }
        if params.views.iter().filter(|view| view.focused).count()
            != usize::from(!params.views.is_empty())
        {
            return Err("可见视图必须恰好有一个焦点".into());
        }
        let mut view_ids = HashSet::new();
        let mut tab_ids = HashSet::new();
        let mut cells = 0_u64;
        for view in &params.views {
            if view.view_id.is_empty()
                || view.view_id.len() > 128
                || view.view_id.chars().any(char::is_control)
                || !view_ids.insert(view.view_id.clone())
                || !tab_ids.insert(view.tab_id.clone())
                || view.cols == 0
                || view.rows == 0
                || view.cols > 4096
                || view.rows > 4096
                || self.app.parse_tab_id(&view.tab_id).is_none()
            {
                return Err("视图身份、标签或尺寸无效".into());
            }
            cells = cells.saturating_add(u64::from(view.cols) * u64::from(view.rows));
        }
        if cells > 1_000_000 {
            return Err("视图总面积超过限制".into());
        }
        // 新布局建立新输入代际，旧按键/鼠标租约先释放，尺寸更新同样适用。
        {
            let held = self
                .clients
                .get_mut(&client_id)
                .map(ClientConnection::drain_shell_held_inputs)
                .unwrap_or_default();
            self.release_client_shell_inputs(client_id, held);
        }
        // 此方法只申报几何；用户导航使用既有 tab.focus 或明确的窗格输入。
        // 迟到的布局刷新因此不会撤销另一客户端或公开 API 的焦点选择。
        let views = params
            .views
            .into_iter()
            .map(|spec| {
                let mut render_state = crate::server::render_stream::ClientRenderState::new(
                    protocol::RenderEncoding::SemanticFrame,
                );
                render_state.enable_surface_reuse(true);
                ClientView {
                    spec,
                    render_state,
                    graphics_delivery: Default::default(),
                }
            })
            .collect::<Vec<_>>();
        if let Some(client) = self.clients.get_mut(&client_id) {
            client.views = Some(MultiViewState {
                revision: params.revision,
                views,
            });
            client.render_pending = true;
        }
        self.tab_geometry_controllers
            .retain(|tab, owner| *owner != client_id || tab_ids.contains(tab));
        for tab in tab_ids {
            self.tab_geometry_controllers
                .entry(tab)
                .or_insert(client_id);
        }
        self.resize_client_views(client_id);
        self.app.render_dirty.request_generic();
        self.app.render_notify.notify_one();
        Ok(true)
    }

    pub(super) fn resize_client_views(&mut self, client_id: u64) {
        let Some(client) = self.clients.get(&client_id) else {
            return;
        };
        let Some(views) = &client.views else {
            return;
        };
        let cell_size = client.cell_size;
        for view in &views.views {
            if self.tab_geometry_controllers.get(&view.spec.tab_id) != Some(&client_id) {
                continue;
            }
            let Some((workspace, tab)) = self.app.parse_tab_id(&view.spec.tab_id) else {
                continue;
            };
            crate::ui::resize_tab_surface(
                &self.app.state,
                &self.app.terminal_runtimes,
                workspace,
                tab,
                Rect::new(0, 0, view.spec.cols, view.spec.rows),
                cell_size,
            );
            if self.popup_owner_tab_id.as_deref() == Some(view.spec.tab_id.as_str()) {
                let _ = crate::server::client_shell::resize_popup_runtime(
                    &self.app,
                    Rect::new(0, 0, view.spec.cols, view.spec.rows),
                    cell_size,
                );
            }
        }
    }

    pub(super) fn resize_changed_view_geometry(&mut self, client_id: u64) {
        let Some(client) = self
            .clients
            .get(&client_id)
            .filter(|client| client.is_active_shell_client())
        else {
            return;
        };
        let Some(views) = &client.views else {
            return;
        };
        for view in &views.views {
            if self.tab_geometry_controllers.get(&view.spec.tab_id) != Some(&client_id) {
                continue;
            }
            let changed = view.render_state.last_pane_surface().is_none_or(|surface| {
                surface.panes.iter().any(|pane| {
                    self.app
                        .parse_pane_id(&pane.pane_id)
                        .and_then(|(workspace, id)| {
                            self.app.state.runtime_for_pane_in_workspace(
                                &self.app.terminal_runtimes,
                                workspace,
                                id,
                            )
                        })
                        .is_some_and(|runtime| {
                            runtime.alternate_screen_active() != pane.alternate_screen_active
                        })
                })
            });
            if changed {
                if let Some((workspace, tab)) = self.app.parse_tab_id(&view.spec.tab_id) {
                    crate::ui::resize_tab_surface(
                        &self.app.state,
                        &self.app.terminal_runtimes,
                        workspace,
                        tab,
                        Rect::new(0, 0, view.spec.cols, view.spec.rows),
                        client.cell_size,
                    );
                }
            }
        }
    }

    pub(super) fn client_view_targets(&self, client_id: u64) -> Vec<crate::ui::TabSurfaceTarget> {
        if let Some(views) = self
            .clients
            .get(&client_id)
            .and_then(|client| client.views.as_ref())
        {
            views
                .views
                .iter()
                .filter_map(|view| self.app.parse_tab_id(&view.spec.tab_id))
                .map(|(workspace_index, tab_index)| crate::ui::TabSurfaceTarget {
                    workspace_index,
                    tab_index,
                })
                .collect()
        } else {
            self.shell_target_for_client(client_id)
                .into_iter()
                .collect()
        }
    }

    pub(super) fn render_client_views(&mut self, client_id: u64) -> bool {
        let Some(client) = self.clients.get_mut(&client_id) else {
            return false;
        };
        let Some(mut views) = client.views.take() else {
            return true;
        };
        let projection = client.shell_projection_revision;
        let cell_size = client.cell_size;
        let writer = client.writer.clone();
        let mut prepared_views = Vec::new();
        let mut batch = Vec::new();
        let mut pending = false;
        for (index, view) in views.views.iter_mut().enumerate() {
            let Some((workspace_index, tab_index)) = self.app.parse_tab_id(&view.spec.tab_id)
            else {
                continue;
            };
            let rendered = render_client_shell_pane_surface(
                &mut self.app,
                Some(crate::ui::TabSurfaceTarget {
                    workspace_index,
                    tab_index,
                }),
                Rect::new(0, 0, view.spec.cols, view.spec.rows),
                false,
                view.spec.focused
                    && self.popup_owner_tab_id.as_deref() == Some(view.spec.tab_id.as_str()),
                cell_size,
                &view.graphics_delivery,
                client_id,
            );
            // 上游 #4508：同步输出批次未完成（或渲染期间内容变化）时该 view 推迟，
            // 仅使该 view 基线失效，等 PTY / 同步超时唤醒后完整重算。
            // 这里没有帧入队，不能等待 ClientWriterDrained，否则后续 PTY
            // 会被 writer 延期状态跳过，首次订阅可能永远收不到画面。
            let mut rendered = match rendered {
                Ok(rendered) => rendered,
                Err(reason) => {
                    view.render_state.request_recompute();
                    if matches!(
                        reason,
                        crate::server::client_shell::SurfaceRenderDeferred::Changed
                    ) {
                        self.app.render_dirty.request_generic();
                    }
                    continue;
                }
            };
            // 上游 #4561：原生图形按客户端单槽只服务主 surface；view 走内联资产，
            // 源文件在这里物化成像素载荷。
            self.materialize_native_sources(
                client_id,
                &mut rendered.graphics,
                &mut rendered.graphics_delivery,
                &mut rendered.graphics_sources,
            );
            let frame = protocol::PaneSurfaceFrame {
                boot_id: self.client_shell_boot_id.clone(),
                projection_revision: projection,
                surface_revision: 0,
                frame: rendered.frame,
                panes: rendered.panes,
                splits: rendered.splits,
                popup: rendered.popup,
                graphics: rendered.graphics,
            };
            let Some(mut prepared) = view.render_state.prepare_pane_surface(frame) else {
                continue;
            };
            let encode = |prepared: &crate::server::render_stream::PreparedRender| {
                let message = protocol::views::message(
                    &self.client_shell_boot_id,
                    views.revision,
                    &view.spec.view_id,
                    &view.spec.tab_id,
                    prepared.message(),
                )
                .map_err(io::Error::other)?;
                Self::frame_server_message_with_max(&message, MAX_GRAPHICS_FRAME_SIZE)
                    .map_err(io::Error::other)
            };
            // 超限时按上游做法逐个剔除最大的内联载荷（保留放置元数据）直到装得下；
            // 剔除过的 view 不更新 delivery，下一次完整渲染重发。
            let mut stripped = false;
            let framed = match encode(&prepared) {
                Ok(framed) => Ok(framed),
                Err(error) => {
                    let mut result = Err(error);
                    while prepared.pop_pane_surface_asset().is_some() {
                        stripped = true;
                        if let Ok(framed) = encode(&prepared) {
                            result = Ok(framed);
                            break;
                        }
                    }
                    result
                }
            };
            let Ok(framed) = framed else {
                pending = true;
                continue;
            };
            if batch.len().saturating_add(framed.len()) > MAX_GRAPHICS_FRAME_SIZE {
                pending = true;
                break;
            }
            batch.extend_from_slice(&framed);
            pending |= stripped;
            prepared_views.push((
                index,
                prepared,
                (!stripped).then_some(rendered.graphics_delivery),
            ));
        }
        let mut alive = true;
        if !batch.is_empty() {
            match writer.as_ref().map(|writer| writer.render.try_send(batch)) {
                Some(Ok(())) => {
                    for (index, prepared, delivery) in prepared_views {
                        if let Some(view) = views.views.get_mut(index) {
                            view.render_state.commit_sent_frame(prepared);
                            if let Some(delivery) = delivery {
                                pending |= delivery.has_pending();
                                view.graphics_delivery = delivery;
                            }
                        }
                    }
                }
                Some(Err(std::sync::mpsc::TrySendError::Full(_))) => pending = true,
                // RS-14：本连接已经没有 writer（发送端消失）与单视图路径一致——
                // 跳过并延期，不当作断开（断开只留给 writer 线程真的消失的
                // `Disconnected`）。
                None => pending = true,
                Some(Err(std::sync::mpsc::TrySendError::Disconnected(_))) => alive = false,
            }
        }
        if let Some(client) = self.clients.get_mut(&client_id) {
            client.views = Some(views);
            if pending {
                client.defer_full_render();
            } else {
                client.clear_deferred_render();
            }
        }
        alive
    }

    pub(super) fn handle_view_input(
        &mut self,
        client_id: u64,
        mut input: protocol::views::ViewInput,
    ) -> bool {
        if input.boot_id != self.client_shell_boot_id || self.handoff_in_progress {
            return false;
        }
        let valid = self
            .clients
            .get(&client_id)
            .filter(|client| client.is_active_shell_client())
            .and_then(|client| client.views.as_ref())
            .is_some_and(|views| {
                views.revision == input.views_revision
                    && views.views.iter().any(|view| {
                        view.spec.view_id == input.view_id && view.spec.tab_id == input.tab_id
                    })
            });
        let owned = self
            .app
            .parse_tab_id(&input.tab_id)
            .zip(self.app.parse_pane_id(&input.pane_id))
            .is_some_and(|((workspace, tab), (pane_workspace, pane))| {
                workspace == pane_workspace
                    && self.app.state.workspaces[workspace].tabs[tab]
                        .panes
                        .contains_key(&pane)
            });
        if !valid || !owned {
            input.events.retain(|event| {
                client_pane_input_releases_press(event)
                    && self
                        .clients
                        .get(&client_id)
                        .is_some_and(|client| client.owns_shell_release(&input.pane_id, event))
            });
        }
        if input.events.is_empty() {
            return false;
        }
        let interaction = client_pane_input_has_interaction(&input.events);
        let mut focus_changed = false;
        if valid && owned && interaction {
            focus_changed =
                self.shell_tab_id_for_client(client_id).as_deref() != Some(input.tab_id.as_str());
            if let Some(client) = self.clients.get_mut(&client_id) {
                if let Some(views) = &mut client.views {
                    for view in &mut views.views {
                        view.spec.focused = view.spec.view_id == input.view_id;
                    }
                }
            }
            self.focus_view_tab(client_id, &input.tab_id);
            if self
                .tab_geometry_controllers
                .insert(input.tab_id.clone(), client_id)
                != Some(client_id)
            {
                self.resize_client_views(client_id);
            }
        }
        self.handle_server_event(ServerEvent::ClientShellPaneInput {
            client_id,
            pane_id: input.pane_id,
            events: input.events,
        }) | focus_changed
    }

    fn focus_view_tab(&mut self, client_id: u64, tab: &str) {
        let before = self.shell_focus_target(client_id);
        let before_tabs = self.focused_shell_tabs();
        self.focus_shell_client_on_tab(client_id, tab);
        let after = self.shell_focus_target(client_id);
        let after_tabs = self.focused_shell_tabs();
        self.send_shell_navigation_focus_events(
            before.as_ref(),
            after.as_ref(),
            &before_tabs,
            &after_tabs,
        );
    }

    pub(super) fn client_views_result(&self, client_id: u64) -> ResponseResult {
        let views = self
            .clients
            .get(&client_id)
            .and_then(|client| client.views.as_ref());
        ResponseResult::ClientViewsSet {
            revision: views.map_or(0, |views| views.revision),
            views: views
                .map(|views| views.views.iter().map(|view| view.spec.clone()).collect())
                .unwrap_or_default(),
        }
    }
}
