use super::*;

impl HeadlessServer {
    pub(super) fn submit_observation(
        &mut self,
        request: api::schema::Request,
        reply: crate::server::observability::Reply,
    ) {
        let pane = match &request.method {
            api::schema::Method::AccountBindingSet(params) => Some(params.pane_id.as_str()),
            api::schema::Method::AccountUsageReport(params) => params.pane_id.as_deref(),
            api::schema::Method::AccountUsageGet(params)
            | api::schema::Method::AccountUsageRefresh(params)
            | api::schema::Method::AccountUsageSubscribe(params) => params.pane_id.as_deref(),
            _ => None,
        };
        if pane.is_some_and(|pane| self.app.parse_pane_id(pane).is_none()) {
            reply.response(
                &request.id,
                Err((
                    "pane_not_found",
                    crate::i18n::texts().runtime.observation_pane_gone.into(),
                )),
            );
            return;
        }
        if self.observability.is_none() {
            match crate::server::observability::Runtime::start(self.client_shell_boot_id.clone()) {
                Ok(runtime) => self.observability = Some(runtime),
                Err(error) => {
                    let message = crate::i18n::fill(
                        crate::i18n::texts().runtime.observation_start_failed_fmt,
                        &[("error", &error.to_string())],
                    );
                    reply.response(&request.id, Err(("server_unavailable", message)));
                    return;
                }
            }
        }
        if let Some(runtime) = &self.observability {
            runtime.submit(request, reply);
        }
    }

    pub(super) fn handle_client_shell_endpoint_request(
        &mut self,
        client_id: u64,
        boot_id: String,
        mut request: Box<api::schema::Request>,
    ) -> bool {
        let Some(client) = self.clients.get(&client_id) else {
            return false;
        };
        if !matches!(client.mode, ClientConnectionMode::ClientShell) {
            self.remove_client_and_resize_if_needed(client_id);
            return true;
        }
        let request_id = request.id.clone();
        if !crate::server::client_commands::supports_client_shell_method(&request.method) {
            let message = crate::server::client_commands::error_message(
                boot_id,
                request_id,
                "unsupported_endpoint_command",
                "this method is not available through the client shell command lane",
            );
            self.send_to_client(client_id, message);
            return false;
        }
        if boot_id != self.client_shell_boot_id {
            let message = crate::server::client_commands::error_message(
                boot_id,
                request_id,
                "stale_boot",
                "endpoint command targeted an earlier server boot",
            );
            self.send_to_client(client_id, message);
            return false;
        }
        if crate::server::text_snapshots::handles(&request.method) {
            let response = match self.text_snapshot_request(&request.method, Some(client_id)) {
                Ok(result) => serde_json::to_string(&api::schema::SuccessResponse {
                    id: request_id.clone(),
                    result,
                })
                .unwrap_or_else(|error| {
                    crate::server::client_commands::error_response(
                        request_id.clone(),
                        "serialization_error",
                        error.to_string(),
                    )
                }),
                Err((code, message)) => crate::server::client_commands::error_response(
                    request_id.clone(),
                    code,
                    message,
                ),
            };
            let chunks = response.as_bytes().chunks(512 * 1024);
            let count = chunks.len();
            for (index, data) in chunks.enumerate() {
                self.send_to_client(
                    client_id,
                    crate::protocol::ServerMessage::ClientShellEndpointResponseChunk {
                        boot_id: boot_id.clone(),
                        request_id: request_id.clone(),
                        final_chunk: index + 1 == count,
                        data: data.to_vec(),
                    },
                );
            }
            return false;
        }
        if let api::schema::Method::ClientViewsSet(params) = &request.method {
            let result = self.set_client_views(client_id, params.clone());
            let message = match &result {
                Ok(_) => crate::server::client_commands::success_message_with_result(
                    boot_id,
                    request_id,
                    self.client_views_result(client_id),
                ),
                Err(message) => crate::server::client_commands::error_message(
                    boot_id,
                    request_id,
                    "invalid_views",
                    message,
                ),
            };
            self.send_to_client(client_id, message);
            return result.unwrap_or(false);
        }
        if crate::server::agent_activity::handles(&request.method) {
            // 与观测请求一样不占终端命令的焦点与 in-flight 名额：读取在活动树后台
            // 线程执行，应答经 server 事件通道按分块回到本客户端。
            let reply = crate::server::agent_activity::Reply::Endpoint {
                client_id,
                boot_id: boot_id.clone(),
                events: self.server_event_tx.clone(),
            };
            if let Err((code, message)) =
                self.agent_activity
                    .submit_request(&self.app, *request, reply, Instant::now())
            {
                self.send_to_client(
                    client_id,
                    crate::server::client_commands::error_message(
                        boot_id, request_id, code, message,
                    ),
                );
            }
            return false;
        }
        if crate::server::observability::is_background_method(&request.method) {
            let reply = crate::server::observability::Reply::Endpoint {
                client_id,
                boot_id,
                events: self.server_event_tx.clone(),
                active: self
                    .observation_liveness
                    .entry(client_id)
                    .or_insert_with(|| Arc::new(AtomicBool::new(true)))
                    .clone(),
            };
            self.submit_observation(*request, reply);
            return false;
        }
        let surface_active = client.shell_surface_active;
        if let api::schema::Method::ClientShellSurfaceSet(params) = &request.method {
            let Some((changed, projection_revision)) =
                self.set_client_shell_surface_active(client_id, params.active)
            else {
                return false;
            };
            self.send_to_client(
                client_id,
                crate::server::client_commands::success_message_with_result(
                    boot_id,
                    request_id,
                    api::schema::ResponseResult::ClientShellSurfaceSet {
                        active: params.active,
                        projection_revision,
                    },
                ),
            );
            return changed;
        }
        if client.shell_endpoint_command_in_flight {
            let message = crate::server::client_commands::error_message(
                boot_id,
                request_id,
                "endpoint_busy",
                "this endpoint is still processing another command",
            );
            self.send_to_client(client_id, message);
            return false;
        }
        if !surface_active {
            let message = crate::server::client_commands::error_message(
                boot_id,
                request_id,
                "surface_inactive",
                "this method requires an active client shell surface",
            );
            self.send_to_client(client_id, message);
            return false;
        }

        let api_request_id = format!(
            "endpoint:{}:{client_id}:{request_id}",
            self.client_shell_boot_id
        );
        request.id = api_request_id.clone();
        let (respond_to, response_rx) = std::sync::mpsc::channel();
        if let Err(err) = crate::server::client_commands::spawn_response_waiter(
            client_id,
            boot_id.clone(),
            request_id.clone(),
            response_rx,
            self.server_event_tx.clone(),
        ) {
            let message = crate::server::client_commands::error_message(
                boot_id,
                request_id,
                "server_unavailable",
                format!("failed to start endpoint response bridge: {err}"),
            );
            self.send_to_client(client_id, message);
            return false;
        }
        if let Some(client) = self.clients.get_mut(&client_id) {
            client.shell_endpoint_command_in_flight = true;
            // A later source restore has a new projection revision. Keep this request's lease
            // so a delayed worktree response cannot focus a pane after endpoint switching.
            client.shell_endpoint_command_surface_revision = Some(client.shell_projection_revision);
            let deferred_worktree = matches!(
                &request.method,
                api::schema::Method::WorktreeCreate(_) | api::schema::Method::WorktreeRemove(_)
            );
            let deferred_navigation = matches!(
                &request.method,
                api::schema::Method::WorktreeCreate(params) if params.focus
            );
            client.shell_deferred_navigation_request_id =
                deferred_worktree.then(|| api_request_id.clone());
            client.shell_deferred_navigation_response = deferred_navigation.then(Vec::new);
        }
        let foreground_changed = self.promote_client_to_foreground(client_id);
        foreground_changed
            | self.handle_client_shell_api_request(
                client_id,
                api::ApiRequestMessage {
                    request: *request,
                    respond_to,
                    response_write_complete: None,
                    observation_events: None,
                    stream_active: None,
                },
            )
    }
}
