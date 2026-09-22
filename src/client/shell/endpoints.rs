use super::*;

#[derive(Clone, Debug)]
pub(crate) struct ClientEndpointAgentViewProjection {
    generation: Option<u64>,
    boot_id: String,
    revision: u64,
    pub(crate) view: Result<Option<crate::api::schema::AgentViewSetParams>, ()>,
}

#[derive(Clone, Debug)]
pub(crate) struct ClientShellEndpoint {
    pub(crate) endpoint_id: ClientEndpointId,
    pub(crate) label: String,
    pub(crate) status: ClientEndpointStatus,
    pub(crate) snapshot: Option<Box<ClientShellSnapshot>>,
    /// Connection generation that produced `snapshot`. `None` is reserved for local tests.
    pub(crate) snapshot_generation: Option<u64>,
    pub(super) agent_presentation: super::endpoint_agent_state::EndpointAgentPresentation,
    pub(crate) agent_view_projection: Option<ClientEndpointAgentViewProjection>,
    pending_agent_view_projection: Option<ClientEndpointAgentViewProjection>,
    pub(crate) agent_view_projection_supported: bool,
    pub(crate) methods: Option<HashSet<String>>,
    /// Version reported by the endpoint welcome; display-only.
    pub(crate) server_version: Option<String>,
    /// Last supervisor status message (typically the failure reason); display-only.
    pub(crate) status_detail: Option<String>,
}

pub(super) struct MachineHit {
    pub(super) rect: Rect,
    pub(super) collapse_toggle: Rect,
    pub(super) endpoint_id: ClientEndpointId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ClientEndpointFocusTarget {
    Workspace(String),
    Tab(String),
    Pane(String),
}

impl ClientShellState {
    /// Mirrors the local catalog into render state and rebuilds the sidebar
    /// presentation lookup in one pass.
    pub(super) fn mirror_saved_profiles(&mut self, profiles: Vec<SavedSshEndpoint>) {
        self.machine_chrome = profiles
            .iter()
            .map(|profile| {
                (
                    profile.id.clone(),
                    MachineChrome {
                        group: profile.group.clone(),
                        color: profile
                            .color
                            .as_deref()
                            .and_then(crate::config::try_parse_color),
                    },
                )
            })
            .collect();
        self.saved_profiles = profiles;
    }

    pub(crate) fn set_endpoint_catalog(&mut self, profiles: &[SavedSshEndpoint]) {
        self.agent_rows_epoch = self.agent_rows_epoch.saturating_add(1);
        self.mirror_saved_profiles(profiles.to_vec());
        let mut next = Vec::with_capacity(profiles.len().saturating_add(1));
        let local = self
            .endpoints
            .iter()
            .find(|endpoint| endpoint.endpoint_id.is_local())
            .cloned()
            .unwrap_or_else(local_endpoint);
        next.push(local);
        for profile in profiles {
            let endpoint_id = ClientEndpointId::Ssh(profile.id.clone());
            let previous = self
                .endpoints
                .iter()
                .find(|endpoint| endpoint.endpoint_id == endpoint_id)
                .filter(|endpoint| {
                    profile.enabled && endpoint.status != ClientEndpointStatus::Disabled
                });
            next.push(ClientShellEndpoint {
                endpoint_id,
                label: profile.label.clone(),
                status: previous.map_or(
                    if profile.enabled {
                        ClientEndpointStatus::Connecting
                    } else {
                        ClientEndpointStatus::Disabled
                    },
                    |endpoint| endpoint.status,
                ),
                snapshot: previous.and_then(|endpoint| endpoint.snapshot.clone()),
                snapshot_generation: previous.and_then(|endpoint| endpoint.snapshot_generation),
                agent_presentation: previous
                    .map(|endpoint| endpoint.agent_presentation.clone())
                    .unwrap_or_default(),
                agent_view_projection: previous
                    .and_then(|endpoint| endpoint.agent_view_projection.clone()),
                pending_agent_view_projection: previous
                    .and_then(|endpoint| endpoint.pending_agent_view_projection.clone()),
                agent_view_projection_supported: previous
                    .is_some_and(|endpoint| endpoint.agent_view_projection_supported),
                methods: previous.and_then(|endpoint| endpoint.methods.clone()),
                server_version: previous.and_then(|endpoint| endpoint.server_version.clone()),
                status_detail: previous.and_then(|endpoint| endpoint.status_detail.clone()),
            });
        }

        if !next
            .iter()
            .any(|endpoint| endpoint.endpoint_id == self.active_endpoint_id)
        {
            self.select_unavailable_local();
        }
        self.collapsed_endpoints.retain(|endpoint_id| {
            next.iter()
                .any(|endpoint| &endpoint.endpoint_id == endpoint_id)
        });
        // 本函数开头已递增 `agent_rows_epoch`；这里仍显式递增折叠代际，让
        // 「折叠写入点必须 bump」的静态守门没有例外。
        self.bump_tree_collapse_epoch();
        self.endpoints = next;
    }

    pub(crate) fn select_unavailable_local(&mut self) {
        self.reset_endpoint_projection();
        self.active_endpoint_id = ClientEndpointId::Local;
        self.mode = ClientShellMode::Terminal;
        self.snapshot = None;
        self.graphics.set_scope("local:unavailable");
        self.reconcile_input_source();
    }

    pub(crate) fn retire_endpoint(&mut self, endpoint_id: &ClientEndpointId) {
        self.retire_endpoint_notifications(endpoint_id);
        self.endpoint_connection_errors.remove(endpoint_id);
        self.endpoint_port_forwards.remove(endpoint_id);
        self.reconnect_progress.remove(endpoint_id);
        if let Some(endpoint) = self
            .endpoints
            .iter_mut()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
        {
            endpoint.status = ClientEndpointStatus::Disabled;
            endpoint.snapshot = None;
            endpoint.snapshot_generation = None;
            endpoint.methods = None;
            endpoint.agent_presentation = Default::default();
            endpoint.agent_view_projection = None;
            endpoint.pending_agent_view_projection = None;
            endpoint.agent_view_projection_supported = false;
            endpoint.server_version = None;
            endpoint.status_detail = None;
        }
    }

    pub(crate) fn set_endpoint_status(
        &mut self,
        endpoint_id: &ClientEndpointId,
        status: ClientEndpointStatus,
    ) {
        self.agent_rows_epoch = self.agent_rows_epoch.saturating_add(1);
        if let Some(endpoint) = self
            .endpoints
            .iter_mut()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
        {
            endpoint.status = status;
            if status == ClientEndpointStatus::Online {
                endpoint.status_detail = None;
            }
        }
    }

    /// 状态详情只出现在侧栏端点行的副标题里，不进联邦 agents 行（`AgentRowsKey`
    /// 不含它），因此**不**递增 `agent_rows_epoch`（独立复审 中-1 口径核对）。
    pub(crate) fn set_endpoint_status_detail(
        &mut self,
        endpoint_id: &ClientEndpointId,
        detail: Option<String>,
    ) {
        if let Some(endpoint) = self
            .endpoints
            .iter_mut()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
        {
            endpoint.status_detail = detail.filter(|detail| !detail.is_empty());
        }
    }

    /// Mirrors the supervisor's structured connection-failure kind into the
    /// render state; `None` clears it (on connect or when the endpoint is
    /// retired).
    pub(crate) fn set_endpoint_connection_error_kind(
        &mut self,
        endpoint_id: &ClientEndpointId,
        kind: Option<crate::remote::ConnectionErrorKind>,
    ) {
        match kind {
            Some(kind) => {
                self.endpoint_connection_errors
                    .insert(endpoint_id.clone(), kind);
            }
            None => {
                self.endpoint_connection_errors.remove(endpoint_id);
            }
        }
    }

    pub(super) fn endpoint_connection_error_kind(
        &self,
        endpoint_id: &ClientEndpointId,
    ) -> Option<&crate::remote::ConnectionErrorKind> {
        self.endpoint_connection_errors.get(endpoint_id)
    }

    /// Session-log writer drop counter for one profile. Zero clears the
    /// entry (the card only shows non-zero counts). Returns true when the
    /// mirror changed (a repaint is worthwhile).
    pub(crate) fn set_session_log_dropped(
        &mut self,
        profile_id: &crate::client::endpoint::ProfileId,
        dropped: u64,
    ) -> bool {
        if dropped == 0 {
            return self.session_log_dropped.remove(profile_id).is_some();
        }
        if self.session_log_dropped.get(profile_id) == Some(&dropped) {
            return false;
        }
        self.session_log_dropped.insert(profile_id.clone(), dropped);
        true
    }

    /// Mirrors the supervisor's per-rule port-forward status into the render
    /// state. Returns true when the mirror changed (a repaint is worthwhile).
    pub(crate) fn set_endpoint_port_forward_status(
        &mut self,
        endpoint_id: &ClientEndpointId,
        status: Vec<crate::remote::PortForwardStatus>,
    ) -> bool {
        if status.is_empty() {
            return self.endpoint_port_forwards.remove(endpoint_id).is_some();
        }
        if self.endpoint_port_forwards.get(endpoint_id) == Some(&status) {
            return false;
        }
        self.endpoint_port_forwards
            .insert(endpoint_id.clone(), status);
        true
    }

    /// The endpoint whose detail card is showing live forward status and is
    /// due for a refresh; `None` while no detail card is open. The cadence is
    /// deliberately slow: monitor-thread failures have no event, so the poll
    /// only runs while the card that displays them is on screen.
    pub(crate) fn port_forward_poll_due(
        &mut self,
        now: std::time::Instant,
    ) -> Option<ClientEndpointId> {
        const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);
        let endpoint_id = match self.overlay.as_ref() {
            Some(ClientShellOverlay::Machines(overlay)) => match &overlay.view {
                super::machines_overlay::ClientMachinesView::Detail(profile_id) => {
                    ClientEndpointId::Ssh(profile_id.clone())
                }
                super::machines_overlay::ClientMachinesView::Forwards(view) => {
                    ClientEndpointId::Ssh(view.profile_id.clone())
                }
                _ => return None,
            },
            _ => return None,
        };
        if self
            .port_forward_polled_at
            .is_some_and(|polled| now.duration_since(polled) < POLL_INTERVAL)
        {
            return None;
        }
        self.port_forward_polled_at = Some(now);
        Some(endpoint_id)
    }

    /// Counts one more reconnect attempt for the banner and estimates when
    /// the next one runs. The supervisor owns the real backoff; this mirrors
    /// its delay ladder for presentation only.
    pub(crate) fn note_endpoint_reconnect_attempt(
        &mut self,
        endpoint_id: &ClientEndpointId,
        now: std::time::Instant,
    ) {
        let attempts = self
            .reconnect_progress
            .get(endpoint_id)
            .map_or(0, |progress| progress.attempts)
            .saturating_add(1);
        let next_attempt_at = now + estimated_retry_delay(attempts, endpoint_id.is_local());
        self.reconnect_progress.insert(
            endpoint_id.clone(),
            ClientReconnectProgress {
                attempts,
                next_attempt_at,
            },
        );
    }

    pub(crate) fn clear_endpoint_reconnect_progress(&mut self, endpoint_id: &ClientEndpointId) {
        self.reconnect_progress.remove(endpoint_id);
    }

    pub(crate) fn endpoint_reconnect_progress(
        &self,
        endpoint_id: &ClientEndpointId,
    ) -> Option<ClientReconnectProgress> {
        self.reconnect_progress.get(endpoint_id).copied()
    }

    pub(crate) fn set_endpoint_server_version(
        &mut self,
        endpoint_id: &ClientEndpointId,
        server_version: Option<String>,
    ) {
        if let Some(endpoint) = self
            .endpoints
            .iter_mut()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
        {
            endpoint.server_version = server_version;
        }
    }

    pub(crate) fn mark_endpoint_disconnected(&mut self, endpoint_id: &ClientEndpointId) {
        self.set_endpoint_status(endpoint_id, ClientEndpointStatus::Reconnecting);
        if endpoint_id == &self.active_endpoint_id {
            let pending = self.pending_requests.keys().cloned().collect::<Vec<_>>();
            for request_id in pending {
                self.cancel_endpoint_request(&request_id);
            }
            self.pending_integration_installs = 0;
            self.pane_scroll_in_flight.clear();
            self.pane_scroll_queued.clear();
        }
        // 在途请求已按取消结局处理；订阅生命周期随连接一起复位（放最后，覆盖
        // 取消路径写下的退避）。
        self.observation_endpoint_disconnected(endpoint_id);
    }

    pub(crate) fn set_endpoint_agent_view_projection_supported(
        &mut self,
        endpoint_id: &ClientEndpointId,
        supported: bool,
    ) {
        if let Some(endpoint) = self
            .endpoints
            .iter_mut()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
        {
            endpoint.agent_view_projection_supported = supported;
            if !supported {
                endpoint.agent_view_projection = None;
                endpoint.pending_agent_view_projection = None;
            }
        }
    }

    pub(crate) fn set_endpoint_methods_for(
        &mut self,
        endpoint_id: &ClientEndpointId,
        methods: Option<Vec<String>>,
    ) {
        let methods = methods.map(|methods| methods.into_iter().collect::<HashSet<_>>());
        if let Some(endpoint) = self
            .endpoints
            .iter_mut()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
        {
            endpoint.methods = methods;
        }
    }

    pub(crate) fn endpoint_projection_available(&self, endpoint_id: &ClientEndpointId) -> bool {
        self.endpoints.iter().any(|endpoint| {
            &endpoint.endpoint_id == endpoint_id
                && endpoint.status == ClientEndpointStatus::Online
                && endpoint.snapshot.is_some()
        })
    }

    pub(crate) fn activate_endpoint_projection(&mut self, endpoint_id: &ClientEndpointId) -> bool {
        let Some(endpoint) = self
            .endpoints
            .iter()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
        else {
            return false;
        };
        if endpoint.status != ClientEndpointStatus::Online {
            return false;
        }
        let Some(snapshot) = endpoint.snapshot.clone() else {
            return false;
        };
        let generation = endpoint.snapshot_generation;
        let switching_endpoint = endpoint_id != &self.active_endpoint_id;
        let agent_scroll = self.agent_scroll;
        if switching_endpoint {
            self.active_endpoint_id = endpoint_id.clone();
            self.pane_surface = None;
            self.pending_pane_surface = None;
        }
        self.apply_active_snapshot(snapshot, generation);
        if switching_endpoint {
            // The aggregate agent list belongs to the client, not one endpoint.
            self.agent_scroll = agent_scroll;
        }
        true
    }

    pub(crate) fn endpoint_status(
        &self,
        endpoint_id: &ClientEndpointId,
    ) -> Option<ClientEndpointStatus> {
        self.endpoints
            .iter()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
            .map(|endpoint| endpoint.status)
    }

    pub(crate) fn endpoint_has_snapshot(&self, endpoint_id: &ClientEndpointId) -> bool {
        self.endpoints
            .iter()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
            .is_some_and(|endpoint| endpoint.snapshot.is_some())
    }

    pub(crate) fn endpoint_is_online(&self, endpoint_id: &ClientEndpointId) -> bool {
        self.endpoint_status(endpoint_id) == Some(ClientEndpointStatus::Online)
            && self.endpoint_has_snapshot(endpoint_id)
    }

    pub(crate) fn endpoint_boot_id(&self, endpoint_id: &ClientEndpointId) -> Option<&str> {
        self.endpoints
            .iter()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)?
            .snapshot
            .as_deref()
            .map(|snapshot| snapshot.boot_id.as_str())
    }

    pub(crate) fn endpoint_snapshot_matches(
        &self,
        endpoint_id: &ClientEndpointId,
        generation: u64,
        boot_id: &str,
        revision: u64,
    ) -> bool {
        self.endpoints
            .iter()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
            .is_some_and(|endpoint| {
                endpoint
                    .snapshot_generation
                    .is_none_or(|snapshot_generation| snapshot_generation == generation)
                    && endpoint.snapshot.as_deref().is_some_and(|snapshot| {
                        snapshot.boot_id == boot_id && snapshot.revision == revision
                    })
            })
    }

    pub(crate) fn endpoint_snapshot_identity(
        &self,
        endpoint_id: &ClientEndpointId,
        generation: u64,
    ) -> Option<(&str, u64)> {
        let endpoint = self
            .endpoints
            .iter()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)?;
        if endpoint
            .snapshot_generation
            .is_some_and(|snapshot_generation| snapshot_generation != generation)
        {
            return None;
        }
        endpoint
            .snapshot
            .as_deref()
            .map(|snapshot| (snapshot.boot_id.as_str(), snapshot.revision))
    }

    pub(crate) fn set_endpoint_agent_view_projection_for_generation(
        &mut self,
        endpoint_id: &ClientEndpointId,
        generation: u64,
        projection: crate::client::endpoint::DecodedAgentViewProjection,
    ) {
        self.set_endpoint_agent_view_projection(
            endpoint_id,
            Some(generation),
            projection.boot_id,
            projection.revision,
            projection.view,
        );
    }

    #[cfg(test)]
    pub(crate) fn set_test_endpoint_agent_view(
        &mut self,
        endpoint_id: &ClientEndpointId,
        view: Option<crate::api::schema::AgentViewSetParams>,
    ) {
        let Some((boot_id, revision)) = self
            .endpoints
            .iter()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
            .and_then(|endpoint| {
                endpoint
                    .snapshot
                    .as_deref()
                    .map(|snapshot| (snapshot.boot_id.clone(), snapshot.revision))
            })
        else {
            return;
        };
        self.set_endpoint_agent_view_projection_supported(endpoint_id, true);
        self.set_endpoint_agent_view_projection(endpoint_id, None, boot_id, revision, Ok(view));
    }

    #[cfg(test)]
    pub(crate) fn set_test_endpoint_agent_view_projection(
        &mut self,
        endpoint_id: &ClientEndpointId,
        boot_id: &str,
        revision: u64,
        view: Option<crate::api::schema::AgentViewSetParams>,
    ) {
        self.set_endpoint_agent_view_projection(
            endpoint_id,
            None,
            boot_id.to_owned(),
            revision,
            Ok(view),
        );
    }

    fn set_endpoint_agent_view_projection(
        &mut self,
        endpoint_id: &ClientEndpointId,
        generation: Option<u64>,
        boot_id: String,
        revision: u64,
        view: Result<Option<crate::api::schema::AgentViewSetParams>, ()>,
    ) {
        let Some(endpoint) = self
            .endpoints
            .iter_mut()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
        else {
            return;
        };
        if endpoint.snapshot_generation == generation
            && endpoint
                .snapshot
                .as_deref()
                .is_some_and(|snapshot| snapshot.boot_id == boot_id && snapshot.revision > revision)
        {
            return;
        }
        let next = ClientEndpointAgentViewProjection {
            generation,
            boot_id,
            revision,
            view,
        };
        let matches_snapshot = endpoint.snapshot_generation == next.generation
            && endpoint.snapshot.as_deref().is_some_and(|snapshot| {
                snapshot.boot_id == next.boot_id && snapshot.revision == next.revision
            });
        let slot = if matches_snapshot {
            &mut endpoint.agent_view_projection
        } else {
            &mut endpoint.pending_agent_view_projection
        };
        if slot.as_ref().is_some_and(|current| {
            current.generation == next.generation
                && current.boot_id == next.boot_id
                && current.revision >= next.revision
        }) {
            return;
        }
        *slot = Some(next);
    }

    pub(crate) fn endpoint_agent_view(
        endpoint: &ClientShellEndpoint,
    ) -> Option<&Result<Option<crate::api::schema::AgentViewSetParams>, ()>> {
        let snapshot = endpoint.snapshot.as_deref()?;
        let projection = endpoint.agent_view_projection.as_ref()?;
        (projection.generation == endpoint.snapshot_generation
            && projection.boot_id == snapshot.boot_id
            && projection.revision == snapshot.revision)
            .then_some(&projection.view)
    }

    /// A terminal normally starts focused. `None` means this host cannot report focus events,
    /// not that the endpoint has no viewer; activation therefore sends an explicit true baseline.
    pub(crate) fn host_focus_baseline(&self) -> bool {
        self.outer_focused.unwrap_or(true)
    }

    pub(crate) fn endpoint_label(&self, endpoint_id: &ClientEndpointId) -> &str {
        self.endpoints
            .iter()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
            .map_or(crate::i18n::texts().endpoint.unknown_endpoint, |endpoint| {
                endpoint.label.as_str()
            })
    }

    pub(crate) fn active_endpoint_label(&self) -> &str {
        self.endpoint_label(&self.active_endpoint_id)
    }

    pub(crate) fn endpoint_is_active(&self, endpoint_id: &ClientEndpointId) -> bool {
        &self.active_endpoint_id == endpoint_id
    }

    pub(crate) fn multi_endpoint_active(&self) -> bool {
        self.endpoints.len() > 1
    }

    #[cfg(test)]
    pub(crate) fn set_snapshot(&mut self, snapshot: Box<ClientShellSnapshot>) {
        self.agent_rows_epoch = self.agent_rows_epoch.saturating_add(1);
        let endpoint_id = self.active_endpoint_id.clone();
        self.set_endpoint_snapshot(&endpoint_id, snapshot);
    }

    #[cfg(test)]
    pub(crate) fn set_endpoint_methods(&mut self, methods: Option<Vec<String>>) {
        let endpoint_id = self.active_endpoint_id.clone();
        self.set_endpoint_methods_for(&endpoint_id, methods);
    }

    pub(super) fn supports_endpoint_method(&self, method: &crate::api::schema::Method) -> bool {
        self.supports_endpoint_method_for(&self.active_endpoint_id.clone(), method)
    }

    pub(super) fn supports_endpoint_method_for(
        &self,
        endpoint_id: &ClientEndpointId,
        method: &crate::api::schema::Method,
    ) -> bool {
        self.endpoints
            .iter()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
            .and_then(|endpoint| endpoint.methods.as_ref())
            .is_none_or(|methods| methods.contains(crate::api::api_method_name(method)))
    }

    pub(super) fn focused_tab_count(&self) -> usize {
        let Some(snapshot) = self.snapshot.as_deref() else {
            return 0;
        };
        snapshot
            .tabs
            .iter()
            .filter(|tab| {
                Some(tab.workspace_id.as_str()) == snapshot.focused_workspace_id.as_deref()
            })
            .count()
    }

    #[cfg(test)]
    pub(crate) fn cache_endpoint_snapshot(
        &mut self,
        endpoint_id: &ClientEndpointId,
        snapshot: Box<ClientShellSnapshot>,
    ) {
        self.cache_endpoint_snapshot_with_surface(endpoint_id, None, snapshot, true);
    }

    pub(crate) fn cache_endpoint_snapshot_for_generation(
        &mut self,
        endpoint_id: &ClientEndpointId,
        generation: u64,
        snapshot: Box<ClientShellSnapshot>,
    ) {
        self.cache_endpoint_snapshot_with_surface(endpoint_id, Some(generation), snapshot, true);
    }

    /// Metadata delivered while an endpoint surface is inactive must never advance this
    /// aggregate client's viewed watermark, even if the frozen source frame still exists.
    pub(crate) fn cache_endpoint_snapshot_inactive_for_generation(
        &mut self,
        endpoint_id: &ClientEndpointId,
        generation: u64,
        snapshot: Box<ClientShellSnapshot>,
    ) {
        self.cache_endpoint_snapshot_with_surface(endpoint_id, Some(generation), snapshot, false);
    }

    fn cache_endpoint_snapshot_with_surface(
        &mut self,
        endpoint_id: &ClientEndpointId,
        generation: Option<u64>,
        mut snapshot: Box<ClientShellSnapshot>,
        acknowledge_surface: bool,
    ) {
        snapshot
            .commands
            .retain(|command| command.action != crate::protocol::ClientShellCommandAction::Unknown);
        let Some(index) = self
            .endpoints
            .iter()
            .position(|endpoint| &endpoint.endpoint_id == endpoint_id)
        else {
            return;
        };
        if self.endpoints[index].snapshot_generation == generation
            && self.endpoints[index]
                .snapshot
                .as_deref()
                .is_some_and(|previous| {
                    previous.boot_id == snapshot.boot_id && previous.revision > snapshot.revision
                })
        {
            return;
        }
        let boot_changed = self.endpoints[index]
            .snapshot
            .as_deref()
            .is_some_and(|previous| previous.boot_id != snapshot.boot_id);
        if boot_changed {
            self.retire_endpoint_notifications(endpoint_id);
        }
        self.endpoints[index]
            .agent_presentation
            .project_snapshot(&mut snapshot);
        let presented_surface = if acknowledge_surface && endpoint_id == &self.active_endpoint_id {
            // workbench 下没有单一镜像：确认聚焦 view 的画面（HERDR-BUG-006）。
            if self.workbench.enabled {
                self.workbench.focused_view().map(|view| &view.surface)
            } else {
                self.pane_surface.as_ref()
            }
        } else {
            None
        };
        if let Some(surface) = presented_surface {
            self.endpoints[index]
                .agent_presentation
                .acknowledge_surface(&mut snapshot, surface, self.outer_focused);
        }
        // 端点快照换代：联邦 agents 行缓存（PERF-02）据此失效。这是**生产**
        // 快照写入路径，别在别处直接改 `endpoint.snapshot`（独立复审 中-1）。
        self.agent_rows_epoch = self.agent_rows_epoch.saturating_add(1);
        let endpoint = &mut self.endpoints[index];
        endpoint.snapshot_generation = generation;
        endpoint.snapshot = Some(snapshot);
        let pending_matches =
            endpoint
                .pending_agent_view_projection
                .as_ref()
                .is_some_and(|projection| {
                    projection.generation == generation
                        && endpoint.snapshot.as_deref().is_some_and(|snapshot| {
                            projection.boot_id == snapshot.boot_id
                                && projection.revision == snapshot.revision
                        })
                });
        if pending_matches {
            endpoint.agent_view_projection = endpoint.pending_agent_view_projection.take();
        } else {
            endpoint.pending_agent_view_projection = None;
            if endpoint
                .agent_view_projection
                .as_ref()
                .is_some_and(|projection| {
                    projection.generation != generation
                        || endpoint.snapshot.as_deref().is_some_and(|snapshot| {
                            projection.boot_id != snapshot.boot_id
                                || projection.revision != snapshot.revision
                        })
                })
            {
                endpoint.agent_view_projection = None;
            }
        }
    }

    pub(crate) fn acknowledge_active_surface_agents(&mut self, surface: &PaneSurfaceFrame) -> bool {
        let Some(updated) = acknowledge_active_surface_agents_on(
            &mut self.endpoints,
            &self.active_endpoint_id,
            self.outer_focused,
            surface,
        ) else {
            return false;
        };
        self.store_acknowledged_snapshot(updated);
        true
    }

    /// 写回「确认表面」原地改过的快照：`revision` 没变，但 agent_status 与聚合
    /// 状态变了（Done → Idle），联邦 agents 行缓存必须跟着换代，否则 Done 徽标
    /// 会滞留到下一次无关的 revision 推进（独立复审 中-1）。
    pub(super) fn store_acknowledged_snapshot(&mut self, snapshot: Box<ClientShellSnapshot>) {
        self.snapshot = Some(snapshot);
        self.agent_rows_epoch = self.agent_rows_epoch.saturating_add(1);
    }

    #[cfg(test)]
    pub(crate) fn set_endpoint_snapshot(
        &mut self,
        endpoint_id: &ClientEndpointId,
        snapshot: Box<ClientShellSnapshot>,
    ) {
        self.agent_rows_epoch = self.agent_rows_epoch.saturating_add(1);
        self.cache_endpoint_snapshot(endpoint_id, snapshot);
        self.apply_cached_endpoint_snapshot(endpoint_id);
    }

    pub(crate) fn set_endpoint_snapshot_for_generation(
        &mut self,
        endpoint_id: &ClientEndpointId,
        generation: u64,
        snapshot: Box<ClientShellSnapshot>,
    ) {
        self.cache_endpoint_snapshot_for_generation(endpoint_id, generation, snapshot);
        self.apply_cached_endpoint_snapshot(endpoint_id);
    }

    fn apply_cached_endpoint_snapshot(&mut self, endpoint_id: &ClientEndpointId) {
        let Some((snapshot, generation)) = self
            .endpoints
            .iter()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
            .and_then(|endpoint| {
                endpoint
                    .snapshot
                    .clone()
                    .map(|snapshot| (snapshot, endpoint.snapshot_generation))
            })
        else {
            return;
        };
        if endpoint_id == &self.active_endpoint_id {
            self.apply_active_snapshot(snapshot, generation);
        }
    }
}

/// Best-effort mirror of the supervisor reconnect backoff
/// (`src/client/endpoint/supervisor.rs`): 500ms doubling, capped at 120s
/// (30s for the local endpoint). Presentation-only; the supervisor's own
/// schedule is authoritative.
pub(super) fn estimated_retry_delay(attempts: u32, local: bool) -> std::time::Duration {
    const INITIAL: std::time::Duration = std::time::Duration::from_millis(500);
    const MAX: std::time::Duration = std::time::Duration::from_secs(120);
    const MAX_LOCAL: std::time::Duration = std::time::Duration::from_secs(30);
    let delay = INITIAL
        .saturating_mul(
            1_u32
                .checked_shl(attempts.saturating_sub(1).min(8))
                .unwrap_or(u32::MAX),
        )
        .min(MAX);
    if local {
        delay.min(MAX_LOCAL)
    } else {
        delay
    }
}

pub(super) fn endpoint_status_label(status: ClientEndpointStatus) -> &'static str {
    let texts = &crate::i18n::texts().endpoint;
    match status {
        ClientEndpointStatus::Connecting => texts.st_connecting,
        ClientEndpointStatus::Online => texts.st_online,
        ClientEndpointStatus::Reconnecting => texts.st_reconnecting,
        ClientEndpointStatus::Attention => texts.st_attention,
        ClientEndpointStatus::Disabled => texts.st_disabled,
    }
}

/// The glyph borrows the caller's spinner frame for connecting states and
/// stays a static literal otherwise — no allocation in sidebar rows.
pub(super) fn endpoint_status_presentation<'a>(
    status: ClientEndpointStatus,
    palette: &Palette,
    spinner: &'a str,
) -> (&'a str, &'static str, ratatui::style::Color) {
    let label = endpoint_status_label(status);
    match status {
        ClientEndpointStatus::Connecting => (spinner, label, palette.yellow),
        ClientEndpointStatus::Online => ("●", label, palette.green),
        ClientEndpointStatus::Reconnecting => (spinner, label, palette.yellow),
        ClientEndpointStatus::Attention => ("!", label, palette.red),
        ClientEndpointStatus::Disabled => ("·", label, palette.overlay0),
    }
}

pub(super) fn local_endpoint() -> ClientShellEndpoint {
    ClientShellEndpoint {
        endpoint_id: ClientEndpointId::Local,
        label: "Local".into(),
        status: ClientEndpointStatus::Online,
        snapshot: None,
        snapshot_generation: None,
        agent_presentation: Default::default(),
        agent_view_projection: None,
        pending_agent_view_projection: None,
        agent_view_projection_supported: false,
        methods: None,
        server_version: None,
        status_detail: None,
    }
}

/// `acknowledge_active_surface_agents` 的字段级形态：调用方拆开 `ClientShellState`
/// 的借用后逐 surface 调用（workbench 下每个可见 view 各调一次，HERDR-BUG-006）。
/// 有变化时返回更新后的快照，由调用方写回。
pub(super) fn acknowledge_active_surface_agents_on(
    endpoints: &mut [ClientShellEndpoint],
    active_endpoint_id: &ClientEndpointId,
    outer_focused: Option<bool>,
    surface: &PaneSurfaceFrame,
) -> Option<Box<ClientShellSnapshot>> {
    let endpoint = endpoints
        .iter_mut()
        .find(|endpoint| endpoint.endpoint_id == *active_endpoint_id)?;
    let changed = {
        let snapshot = endpoint.snapshot.as_deref_mut()?;
        endpoint
            .agent_presentation
            .acknowledge_surface(snapshot, surface, outer_focused)
    };
    if changed {
        endpoint.snapshot.clone()
    } else {
        None
    }
}
