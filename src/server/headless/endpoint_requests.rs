use super::*;
use crate::server::client_commands::{
    self, EndpointResponseIdentity, EndpointResponseKind, EndpointResponseReady,
    EndpointResponseSender, EndpointResponseTarget, EndpointResponseTicket,
    ENDPOINT_RESPONSE_CHUNK_BYTES,
};

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

    fn endpoint_control_result(
        &self,
        client_id: u64,
        boot_id: &str,
        request_id: &str,
        result: Result<api::schema::ResponseResult, (&str, String)>,
    ) {
        if let Some(writer) = self
            .clients
            .get(&client_id)
            .and_then(|client| client.writer.as_ref())
        {
            client_commands::send_control_response(
                &writer.control,
                boot_id,
                request_id,
                client_commands::response_text(request_id, result),
            );
        }
    }

    pub(super) fn admit_endpoint_response(
        &mut self,
        client_id: u64,
        boot_id: String,
        request_id: String,
        kind: EndpointResponseKind,
    ) -> Option<EndpointResponseTicket> {
        let client = self.clients.get_mut(&client_id)?;
        client.prune_endpoint_responses();
        if client.endpoint_responses.contains(&boot_id, &request_id) {
            return None;
        }
        let ticket = client.endpoint_responses.admit(
            &self.endpoint_response_budget,
            client_id,
            boot_id,
            request_id,
            kind,
        )?;
        self.endpoint_response_owners
            .retain(|owner| owner.upgrade().is_some_and(|identity| identity.live()));
        self.endpoint_response_owners
            .push(Arc::downgrade(&ticket.identity));
        Some(ticket)
    }

    fn cancel_endpoint_response(&mut self, identity: &Arc<EndpointResponseIdentity>) {
        if let Some(client) = self.clients.get_mut(&identity.client_id) {
            if client.endpoint_responses.remove(identity).is_some() {
                client.finish_endpoint_command(identity);
            }
        }
    }

    pub(super) fn accept_endpoint_response(&mut self, response: EndpointResponseReady) -> bool {
        let identity = response.ticket.identity.clone();
        if self.shutting_down || identity.boot_id != self.client_shell_boot_id || !identity.active()
        {
            self.cancel_endpoint_response(&identity);
            return false;
        }
        let Some(client) = self.clients.get_mut(&identity.client_id) else {
            return false;
        };
        let Some(slot) = client
            .endpoint_responses
            .slots
            .iter_mut()
            .find(|slot| Arc::ptr_eq(&slot.identity, &identity))
        else {
            return false;
        };
        if slot.ready.is_some() {
            return false;
        }
        if matches!(
            slot.kind,
            EndpointResponseKind::Command { navigate: true, .. }
        ) {
            slot.navigation_tab = Self::deferred_endpoint_navigation_tab_id(&response.body);
        }
        slot.ready = Some(response);
        self.pump_endpoint_response(identity.client_id)
    }

    pub(super) fn pump_endpoint_response(&mut self, client_id: u64) -> bool {
        if self.shutting_down {
            return false;
        }
        let Some(client) = self.clients.get_mut(&client_id) else {
            return false;
        };
        client.prune_endpoint_responses();
        if !client.endpoint_responses.has_ready() {
            return false;
        }
        let Some(writer) = client.writer.clone() else {
            return false;
        };
        if self.endpoint_pump_remaining == 0 {
            writer.defer_endpoint_credit();
            self.transport_notifications.more_pending = true;
            return false;
        }
        let attempts = client.endpoint_responses.slots.len();
        let mut completed = None;
        let mut closed = false;
        let mut sent = false;
        for _ in 0..attempts {
            let Some(mut slot) = client.endpoint_responses.slots.pop_front() else {
                break;
            };
            if slot.identity.boot_id != self.client_shell_boot_id {
                client.finish_endpoint_command(&slot.identity);
                continue;
            }
            if !slot.identity.active() {
                client.endpoint_responses.slots.push_back(slot);
                continue;
            }
            let Some(response) = slot.ready.as_ref() else {
                client.endpoint_responses.slots.push_back(slot);
                continue;
            };
            let end = slot
                .offset
                .saturating_add(ENDPOINT_RESPONSE_CHUNK_BYTES)
                .min(response.body.len());
            let final_chunk = end == response.body.len();
            let message = ServerMessage::ClientShellEndpointResponseChunk {
                boot_id: slot.identity.boot_id.clone(),
                request_id: slot.identity.request_id.clone(),
                final_chunk,
                data: response.body[slot.offset..end].to_vec(),
            };
            let frame = match Self::frame_server_message(&message) {
                Ok(frame) => frame,
                Err(error) => {
                    warn!(client_id, %error, "endpoint frame encoding failed");
                    writer.abort();
                    closed = true;
                    break;
                }
            };
            match writer.control.try_send_endpoint_frame(frame) {
                Ok(()) => {
                    self.endpoint_pump_remaining -= 1;
                    sent = true;
                    slot.offset = end;
                    if final_chunk {
                        completed = Some(slot);
                    } else {
                        client.endpoint_responses.slots.push_back(slot);
                    }
                    break;
                }
                Err(std::sync::mpsc::TrySendError::Full(_)) => {
                    client.endpoint_responses.slots.push_back(slot);
                }
                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                    closed = true;
                    break;
                }
            }
        }
        if closed {
            client.cancel_endpoint_responses();
            self.remove_client_and_resize_if_needed(client_id);
            return true;
        }
        let mut navigation = None;
        if let Some(mut slot) = completed {
            if let EndpointResponseKind::Command {
                surface_revision, ..
            } = slot.kind
            {
                if client.finish_endpoint_command(&slot.identity)
                    && client.is_active_shell_client()
                    && client.shell_projection_revision == surface_revision
                {
                    navigation = slot.navigation_tab.take();
                }
            }
            drop(slot);
        }
        if sent && client.endpoint_responses.has_ready() {
            writer.defer_endpoint_credit();
            self.transport_notifications.more_pending = true;
        }
        let focus_before = self.shell_focus_target(client_id);
        let focused_tabs_before = self.focused_shell_tabs();
        let navigation_changed = navigation
            .as_deref()
            .is_some_and(|tab| self.focus_shell_client_on_tab(client_id, tab));
        let geometry_changed =
            navigation_changed && self.claim_shell_tab_geometry(client_id, false);
        if navigation_changed {
            self.reconcile_client_shell_locations();
            let focus_after = self.shell_focus_target(client_id);
            let focused_tabs_after = self.focused_shell_tabs();
            self.app.accept_current_focus_without_events();
            self.send_shell_navigation_focus_events(
                focus_before.as_ref(),
                focus_after.as_ref(),
                &focused_tabs_before,
                &focused_tabs_after,
            );
        }
        navigation_changed | geometry_changed
    }

    pub(super) fn handle_client_shell_endpoint_request(
        &mut self,
        client_id: u64,
        boot_id: String,
        mut request: Box<api::schema::Request>,
    ) -> bool {
        use api::schema::Method;
        if self.shutting_down {
            return false;
        }
        let Some(client) = self.clients.get_mut(&client_id) else {
            return false;
        };
        client.prune_endpoint_responses();
        if !client.is_shell_client() {
            self.remove_client_and_resize_if_needed(client_id);
            return true;
        }
        let request_id = request.id.clone();
        if client.endpoint_responses.contains(&boot_id, &request_id) {
            return false;
        }
        let surface_active = client.shell_surface_active;
        let surface_revision = client.shell_projection_revision;
        let command_busy = client.shell_endpoint_command_in_flight;
        if !client_commands::supports_client_shell_method(&request.method) {
            self.endpoint_control_result(
                client_id,
                &boot_id,
                &request_id,
                Err((
                    "unsupported_endpoint_command",
                    "this method is not available through the client shell command lane".into(),
                )),
            );
            return false;
        }
        if boot_id != self.client_shell_boot_id {
            self.endpoint_control_result(
                client_id,
                &boot_id,
                &request_id,
                Err((
                    "stale_boot",
                    "endpoint command targeted an earlier server boot".into(),
                )),
            );
            return false;
        }
        if let Method::ClientShellSurfaceSet(params) = &request.method {
            let Some((changed, projection_revision)) =
                self.set_client_shell_surface_active(client_id, params.active)
            else {
                return false;
            };
            self.endpoint_control_result(
                client_id,
                &boot_id,
                &request_id,
                Ok(api::schema::ResponseResult::ClientShellSurfaceSet {
                    active: params.active,
                    projection_revision,
                }),
            );
            return changed;
        }
        if let Method::ClientViewsSet(params) = &request.method {
            let result = self.set_client_views(client_id, params.clone());
            let changed = result.as_ref().copied().unwrap_or(false);
            let result = result
                .map(|_| self.client_views_result(client_id))
                .map_err(|error| ("invalid_views", error));
            self.endpoint_control_result(client_id, &boot_id, &request_id, result);
            return changed;
        }
        if matches!(&request.method, Method::PaneTextSnapshotRelease(_)) {
            let result = self.text_snapshot_request(&request.method, Some(client_id));
            self.endpoint_control_result(client_id, &boot_id, &request_id, result);
            return false;
        }
        if matches!(
            &request.method,
            Method::SystemMetricsUnsubscribe(_) | Method::AccountUsageUnsubscribe(_)
        ) {
            let target = self.clients.get_mut(&client_id).and_then(|client| {
                let writer = client.writer.as_ref()?.control.clone();
                client.endpoint_responses.admit_control(
                    client_id,
                    boot_id.clone(),
                    request_id.clone(),
                    writer,
                )
            });
            let Some(target) = target else {
                self.endpoint_control_result(
                    client_id,
                    &boot_id,
                    &request_id,
                    Err((
                        "endpoint_busy",
                        "an asynchronous control request is already pending".into(),
                    )),
                );
                return false;
            };
            let reply = self.observation_endpoint_reply(
                client_id,
                boot_id,
                EndpointResponseSender::new(target),
            );
            self.submit_observation(*request, reply);
            return false;
        }
        let reading = crate::server::text_snapshots::handles(&request.method);
        let activity = crate::server::agent_activity::handles(&request.method);
        let observation = crate::server::observability::is_background_method(&request.method);
        let kind = if reading {
            EndpointResponseKind::Reading
        } else if activity || observation {
            EndpointResponseKind::Background
        } else {
            if command_busy || !surface_active {
                let (code, message) = if command_busy {
                    (
                        "endpoint_busy",
                        "this endpoint is still processing another command",
                    )
                } else {
                    (
                        "surface_inactive",
                        "this method requires an active client shell surface",
                    )
                };
                self.endpoint_control_result(
                    client_id,
                    &boot_id,
                    &request_id,
                    Err((code, message.into())),
                );
                return false;
            }
            let navigate = match &request.method {
                Method::WorktreeCreate(params) => params.focus,
                Method::WorktreeOpen(params) => params.focus,
                _ => false,
            };
            EndpointResponseKind::Command {
                surface_revision,
                navigate,
            }
        };
        let Some(ticket) =
            self.admit_endpoint_response(client_id, boot_id.clone(), request_id.clone(), kind)
        else {
            self.endpoint_control_result(
                client_id,
                &boot_id,
                &request_id,
                Err((
                    "endpoint_busy",
                    "endpoint response capacity is exhausted".into(),
                )),
            );
            return false;
        };
        if reading {
            let result = self.text_snapshot_request(&request.method, Some(client_id));
            let response = EndpointResponseReady::new(
                ticket,
                client_commands::response_text(&request_id, result),
            );
            return self.accept_endpoint_response(response);
        }
        if activity {
            let response = EndpointResponseSender::new(EndpointResponseTarget::Bulk(ticket));
            let reply = crate::server::agent_activity::Reply::Endpoint {
                response: response.clone(),
                events: self.server_event_tx.clone(),
            };
            if let Err((code, message)) =
                self.agent_activity
                    .submit_request(&self.app, *request, reply, Instant::now())
            {
                if let Some(target) = response.take() {
                    target.send(
                        client_commands::error_response(request_id, code, message),
                        &self.server_event_tx,
                    );
                }
            }
            return false;
        }
        if observation {
            let reply = self.observation_endpoint_reply(
                client_id,
                boot_id,
                EndpointResponseSender::new(EndpointResponseTarget::Bulk(ticket)),
            );
            self.submit_observation(*request, reply);
            return false;
        }
        let identity = ticket.identity.clone();
        let api_request_id = format!(
            "endpoint:{}:{client_id}:{request_id}",
            self.client_shell_boot_id
        );
        request.id = api_request_id.clone();
        let (respond_to, response_rx) = std::sync::mpsc::channel();
        if let Err(error) = client_commands::spawn_response_waiter(
            ticket,
            response_rx,
            self.server_event_tx.clone(),
        ) {
            self.cancel_endpoint_response(&identity);
            self.endpoint_control_result(
                client_id,
                &boot_id,
                &request_id,
                Err((
                    "server_unavailable",
                    format!("failed to start endpoint response bridge: {error}"),
                )),
            );
            return false;
        }
        if let Some(client) = self.clients.get_mut(&client_id) {
            client.shell_endpoint_command_in_flight = true;
            client.shell_endpoint_command_surface_revision = Some(surface_revision);
            let deferred = matches!(
                &request.method,
                Method::WorktreeCreate(_)
                    | Method::WorktreeRemove(_)
                    | Method::WorktreeList(_)
                    | Method::WorktreeOpen(_)
            );
            client.shell_deferred_navigation_request_id = deferred.then(|| api_request_id.clone());
            if deferred {
                let _ = identity.deferred_request_id.set(api_request_id.clone());
            }
            if let Some(slot) = client
                .endpoint_responses
                .slots
                .iter_mut()
                .find(|slot| Arc::ptr_eq(&slot.identity, &identity))
            {
                slot.deferred_request_id = client.shell_deferred_navigation_request_id.clone();
            }
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
                    report_origin: None,
                    stream_active: None,
                },
            )
    }

    fn observation_endpoint_reply(
        &mut self,
        client_id: u64,
        boot_id: String,
        response: EndpointResponseSender,
    ) -> crate::server::observability::Reply {
        crate::server::observability::Reply::Endpoint {
            client_id,
            boot_id,
            response,
            events: self.server_event_tx.clone(),
            active: self
                .observation_liveness
                .entry(client_id)
                .or_insert_with(|| Arc::new(AtomicBool::new(true)))
                .clone(),
        }
    }
}
