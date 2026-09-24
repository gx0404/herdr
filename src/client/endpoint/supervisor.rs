use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{ClientEndpointId, ClientEndpointStatus, EndpointNegotiation, NativeEndpointTransport};
use crate::protocol::{ClientSurfaceSize, RenderEncoding};
use interprocess::TryClone as _;

const INITIAL_RETRY_DELAY: Duration = Duration::from_millis(500);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(120);
const MAX_LOCAL_RETRY_DELAY: Duration = Duration::from_secs(30);
const STABLE_CONNECTION_PERIOD: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug)]
pub(crate) struct EndpointConnectOptions {
    pub(crate) cols: u16,
    pub(crate) rows: u16,
    pub(crate) cell_width_px: u32,
    pub(crate) cell_height_px: u32,
    pub(crate) pixel_geometry_exact: bool,
    pub(crate) surface_size: ClientSurfaceSize,
    pub(crate) endpoint_keybindings: bool,
    pub(crate) mouse_capture: bool,
}

pub(crate) struct PreparedEndpointConnection {
    reader: crate::ipc::LocalStream,
    writer: NativeEndpointTransport,
    negotiation: EndpointNegotiation,
}

pub(crate) fn prepare_interactive_connection(
    connected: crate::remote::SavedSshStream,
    options: EndpointConnectOptions,
    cancel: &crate::remote::TaskCancellation,
) -> std::io::Result<PreparedEndpointConnection> {
    prepare_endpoint_connection(
        connected.stream,
        Box::new(connected.bridge),
        options,
        true,
        Some(cancel),
    )
}

pub(crate) enum EndpointSupervisorEvent {
    Status {
        endpoint_id: ClientEndpointId,
        generation: u64,
        status: ClientEndpointStatus,
        message: String,
    },
    Connected {
        endpoint_id: ClientEndpointId,
        generation: u64,
        reader: crate::ipc::LocalStream,
        writer: NativeEndpointTransport,
        negotiation: EndpointNegotiation,
    },
}

#[derive(Clone)]
enum ConnectTarget {
    Local(PathBuf),
    Ssh(Box<super::SavedSshEndpoint>),
}

struct ReconnectState {
    target: ConnectTarget,
    attempts: u32,
    next_attempt: Option<Instant>,
    in_flight: bool,
    generation: Option<u64>,
    online_since: Option<Instant>,
}

impl ReconnectState {
    fn new(target: ConnectTarget, now: Instant) -> Self {
        Self {
            target,
            attempts: 0,
            next_attempt: Some(now),
            in_flight: false,
            generation: None,
            online_since: None,
        }
    }
}

pub(crate) struct EndpointSupervisors {
    endpoints: HashMap<ClientEndpointId, ReconnectState>,
    next_generation: u64,
    shutdown: Arc<AtomicBool>,
    /// Latest structured connection-failure kind per endpoint, recorded
    /// alongside the status event so detail views can react to the kind
    /// (e.g. host-key or auth prompts) instead of parsing message text.
    error_kinds:
        Arc<std::sync::Mutex<HashMap<ClientEndpointId, crate::remote::ConnectionErrorKind>>>,
    /// Running SSH port forwards per endpoint: rebuilt after every successful
    /// connect and reconciled in place when catalog rules change underneath a
    /// healthy connection.
    forwards: Arc<std::sync::Mutex<crate::remote::PortForwardManager>>,
    /// Snapshot workers for profiles with session logging enabled; follows
    /// the same profile lifecycle as the connections themselves.
    session_logs: super::session_log::SessionLogManager,
}

impl EndpointSupervisors {
    pub(crate) fn new(profiles: &[super::SavedSshEndpoint], now: Instant) -> Self {
        let endpoints = profiles
            .iter()
            .filter(|profile| profile.enabled)
            .map(|profile| {
                (
                    ClientEndpointId::Ssh(profile.id.clone()),
                    ReconnectState::new(ConnectTarget::Ssh(Box::new(profile.clone())), now),
                )
            })
            .collect();
        let mut session_logs = super::session_log::SessionLogManager::new();
        session_logs.sync(profiles);
        Self {
            endpoints,
            next_generation: 2,
            shutdown: Arc::new(AtomicBool::new(false)),
            error_kinds: Arc::new(std::sync::Mutex::new(HashMap::new())),
            forwards: Arc::new(std::sync::Mutex::new(
                crate::remote::PortForwardManager::new(),
            )),
            session_logs,
        }
    }

    pub(crate) fn adopt_interactive(
        &mut self,
        profile: super::SavedSshEndpoint,
        prepared: PreparedEndpointConnection,
        now: Instant,
        event_tx: &tokio::sync::mpsc::Sender<EndpointSupervisorEvent>,
    ) {
        let endpoint_id = ClientEndpointId::Ssh(profile.id.clone());
        let generation = self.next_generation;
        self.next_generation = self.next_generation.saturating_add(1);
        let mut state = ReconnectState::new(ConnectTarget::Ssh(Box::new(profile.clone())), now);
        state.generation = Some(generation);
        state.next_attempt = None;
        state.in_flight = true;
        self.endpoints.insert(endpoint_id.clone(), state);
        let forwards = self.forwards.clone();
        let event_tx = event_tx.clone();
        std::thread::spawn(move || {
            lock_forwards(&forwards).rebuild(&profile);
            let _ = event_tx.blocking_send(EndpointSupervisorEvent::Connected {
                endpoint_id,
                generation,
                reader: prepared.reader,
                writer: prepared.writer,
                negotiation: prepared.negotiation,
            });
        });
    }

    pub(crate) fn add_local(&mut self, path: PathBuf, generation: Option<u64>, now: Instant) {
        let mut state = ReconnectState::new(ConnectTarget::Local(path), now);
        state.generation = generation;
        if generation.is_some() {
            state.next_attempt = None;
        }
        self.endpoints.insert(ClientEndpointId::Local, state);
    }

    pub(crate) fn reconcile_profiles(
        &mut self,
        profiles: &[super::SavedSshEndpoint],
        now: Instant,
    ) -> Vec<ClientEndpointId> {
        let mut retired = Vec::new();
        self.endpoints.retain(|endpoint_id, state| {
            let ConnectTarget::Ssh(previous) = &state.target else {
                return true;
            };
            let keep = profiles.iter().any(|profile| {
                profile.id == previous.id && profile.enabled && profile.same_connection(previous)
            });
            if !keep {
                retired.push(endpoint_id.clone());
            }
            keep
        });
        {
            let mut error_kinds = lock_error_kinds(&self.error_kinds);
            let mut forwards = lock_forwards(&self.forwards);
            for endpoint_id in &retired {
                error_kinds.remove(endpoint_id);
                if let ClientEndpointId::Ssh(profile_id) = endpoint_id {
                    forwards.stop(profile_id);
                }
            }
        }
        for profile in profiles.iter().filter(|profile| profile.enabled) {
            let state = self
                .endpoints
                .entry(ClientEndpointId::Ssh(profile.id.clone()))
                .or_insert_with(|| {
                    ReconnectState::new(ConnectTarget::Ssh(Box::new(profile.clone())), now)
                });
            let rules_changed = match &state.target {
                ConnectTarget::Ssh(previous) => previous.port_forwards != profile.port_forwards,
                ConnectTarget::Local(_) => false,
            };
            state.target = ConnectTarget::Ssh(Box::new(profile.clone()));
            // Forward rules follow the live connection: only apply edits while
            // online. Everything else is picked up by the rebuild on the next
            // successful connect.
            if rules_changed && state.online_since.is_some() {
                lock_forwards(&self.forwards).reconcile(profile);
            }
        }
        // Session-log workers track the profile set independently of the
        // connection state: logging is a client-local background concern and
        // must not force or block a reconnect.
        self.session_logs.sync(profiles);
        retired
    }

    /// Snapshots dropped by the shared session-log writer queue for one
    /// profile; polled by the client on the forward-status cadence while
    /// that machine's detail card showing it is open.
    pub(crate) fn session_log_dropped(&self, profile_id: &super::ProfileId) -> u64 {
        self.session_logs.dropped_for(profile_id)
    }

    pub(crate) fn spawn_due(
        &mut self,
        now: Instant,
        options: EndpointConnectOptions,
        event_tx: &tokio::sync::mpsc::Sender<EndpointSupervisorEvent>,
    ) {
        for (endpoint_id, state) in &mut self.endpoints {
            if state.in_flight || state.next_attempt.is_none_or(|deadline| deadline > now) {
                continue;
            }
            state.in_flight = true;
            state.next_attempt = None;
            let generation = self.next_generation;
            state.generation = Some(generation);
            self.next_generation = self.next_generation.saturating_add(1);
            let endpoint_id = endpoint_id.clone();
            let target = state.target.clone();
            let forward_profile = match &target {
                ConnectTarget::Ssh(profile) => Some((**profile).clone()),
                ConnectTarget::Local(_) => None,
            };
            let event_tx = event_tx.clone();
            let shutdown = self.shutdown.clone();
            let error_kinds = self.error_kinds.clone();
            let forwards = self.forwards.clone();
            tokio::spawn(async move {
                if shutdown.load(Ordering::Acquire) {
                    return;
                }
                let task_endpoint_id = endpoint_id.clone();
                let result = tokio::task::spawn_blocking(move || {
                    let event = connect_once(&target, options, endpoint_id, generation)?;
                    // The link is up: (re)start this profile's port forwards
                    // before the Online event publishes the endpoint.
                    if let Some(profile) = &forward_profile {
                        lock_forwards(&forwards).rebuild(profile);
                    }
                    Ok(event)
                })
                .await;
                let event = match result {
                    Ok(Ok(event)) => {
                        lock_error_kinds(&error_kinds).remove(&task_endpoint_id);
                        event
                    }
                    Ok(Err(error)) => {
                        lock_error_kinds(&error_kinds).insert(
                            task_endpoint_id.clone(),
                            crate::remote::classify_connection_error(&error),
                        );
                        EndpointSupervisorEvent::Status {
                            endpoint_id: task_endpoint_id,
                            generation,
                            status: if failure_needs_attention(&error) {
                                ClientEndpointStatus::Attention
                            } else {
                                ClientEndpointStatus::Reconnecting
                            },
                            message: error.to_string(),
                        }
                    }
                    Err(error) => EndpointSupervisorEvent::Status {
                        endpoint_id: task_endpoint_id,
                        generation,
                        status: ClientEndpointStatus::Reconnecting,
                        message: format!("endpoint connection task stopped unexpectedly: {error}"),
                    },
                };
                if !shutdown.load(Ordering::Acquire) {
                    let _ = event_tx.send(event).await;
                }
            });
        }
    }

    /// The structured kind of the endpoint's latest connection failure, when
    /// one has been recorded since the last successful connect. Cleared on
    /// `Online`; kept for `Attention` so the detail view can offer the
    /// matching remedy (approve host key, answer an auth prompt, ...).
    pub(crate) fn connection_error_kind(
        &self,
        endpoint_id: &ClientEndpointId,
    ) -> Option<crate::remote::ConnectionErrorKind> {
        lock_error_kinds(&self.error_kinds)
            .get(endpoint_id)
            .cloned()
    }

    /// Latest per-rule port-forward status for an SSH endpoint (empty for
    /// Local or when nothing is tracked). Failed rules carry the recorded
    /// reason so the detail view can show it without parsing process output.
    pub(crate) fn port_forward_status(
        &self,
        endpoint_id: &ClientEndpointId,
    ) -> Vec<crate::remote::PortForwardStatus> {
        match endpoint_id {
            ClientEndpointId::Ssh(profile_id) => lock_forwards(&self.forwards).status(profile_id),
            ClientEndpointId::Local => Vec::new(),
        }
    }

    pub(crate) fn record_status(
        &mut self,
        endpoint_id: &ClientEndpointId,
        generation: u64,
        status: ClientEndpointStatus,
        now: Instant,
    ) -> bool {
        let Some(state) = self.endpoints.get_mut(endpoint_id) else {
            return false;
        };
        if state.generation != Some(generation) {
            return false;
        }
        state.in_flight = false;
        match status {
            ClientEndpointStatus::Online => {
                if endpoint_id.is_local() {
                    state.attempts = 0;
                }
                state.online_since.get_or_insert(now);
                state.next_attempt = None;
                lock_error_kinds(&self.error_kinds).remove(endpoint_id);
            }
            // Attention stops retries: recovery runs in-UI (approved interactive
            // authentication or a manual reconnect via `reconnect_now`), and a
            // background BatchMode retry must not interleave with an approved
            // interactive attempt for the same machine.
            ClientEndpointStatus::Attention | ClientEndpointStatus::Disabled => {
                state.online_since = None;
                state.next_attempt = None;
            }
            ClientEndpointStatus::Connecting | ClientEndpointStatus::Reconnecting => {
                // A brief maintenance wake can complete a handshake without restoring the link.
                if state.online_since.take().is_some_and(|connected| {
                    now.saturating_duration_since(connected) >= STABLE_CONNECTION_PERIOD
                }) {
                    state.attempts = 0;
                }
                state.attempts = state.attempts.saturating_add(1);
                let delay = retry_delay(state.attempts);
                state.next_attempt = Some(
                    now + if endpoint_id.is_local() {
                        delay.min(MAX_LOCAL_RETRY_DELAY)
                    } else {
                        delay
                    },
                );
            }
        }
        true
    }

    /// Manual reconnect request: schedule an immediate attempt with a fresh
    /// backoff ladder. An attempt already in flight is left alone.
    pub(crate) fn reconnect_now(&mut self, endpoint_id: &ClientEndpointId, now: Instant) -> bool {
        let Some(state) = self.endpoints.get_mut(endpoint_id) else {
            return false;
        };
        if state.in_flight {
            return true;
        }
        state.attempts = 0;
        state.next_attempt = Some(now);
        true
    }

    pub(crate) fn disconnected(
        &mut self,
        endpoint_id: &ClientEndpointId,
        generation: u64,
        now: Instant,
    ) -> bool {
        self.record_status(
            endpoint_id,
            generation,
            ClientEndpointStatus::Reconnecting,
            now,
        )
    }
}

impl Drop for EndpointSupervisors {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        lock_forwards(&self.forwards).stop_all();
        self.session_logs.stop_all();
    }
}

fn connect_once(
    target: &ConnectTarget,
    options: EndpointConnectOptions,
    endpoint_id: ClientEndpointId,
    generation: u64,
) -> Result<EndpointSupervisorEvent, std::io::Error> {
    let (stream, lifetime): (_, Box<dyn Send>) = match target {
        ConnectTarget::Local(path) => {
            let stream = crate::ipc::connect_local_stream(path).map_err(|error| {
                // An absent Local socket is transient, unlike a missing SSH install.
                if error.kind() == std::io::ErrorKind::NotFound {
                    std::io::Error::new(
                        std::io::ErrorKind::ConnectionRefused,
                        "Local is unavailable; start its server to reconnect",
                    )
                } else {
                    error
                }
            })?;
            (stream, Box::new(()))
        }
        ConnectTarget::Ssh(profile) => {
            let connected = crate::remote::connect_saved_ssh(profile).map_err(|error| {
                if failure_needs_attention(&error) {
                    // Extend the message with the standalone fix command while
                    // keeping the structured classification attached.
                    let kind = crate::remote::classify_connection_error(&error);
                    let wrapped = std::io::Error::new(error.kind(), format!("{error}. Run `{}` interactively to approve setup, then restart this client", crate::remote::saved_ssh_bootstrap_command(&profile.target, &profile.session)));
                    crate::remote::wrap_classified(wrapped, kind)
                } else { error }
            })?;
            (connected.stream, Box::new(connected.bridge))
        }
    };
    let prepared =
        prepare_endpoint_connection(stream, lifetime, options, !endpoint_id.is_local(), None)?;
    Ok(EndpointSupervisorEvent::Connected {
        endpoint_id,
        generation,
        reader: prepared.reader,
        writer: prepared.writer,
        negotiation: prepared.negotiation,
    })
}

fn prepare_endpoint_connection(
    mut stream: crate::ipc::LocalStream,
    lifetime: Box<dyn Send>,
    options: EndpointConnectOptions,
    remote: bool,
    cancel: Option<&crate::remote::TaskCancellation>,
) -> std::io::Result<PreparedEndpointConnection> {
    let handshake = super::super::do_handshake(
        &mut stream,
        options.cols,
        options.rows,
        options.cell_width_px,
        options.cell_height_px,
        options.pixel_geometry_exact,
        Some(options.surface_size),
        options.endpoint_keybindings,
        options.mouse_capture,
        false,
        // Only the Local endpoint shares this client's filesystem; SSH bridges
        // expose a local socket but never server-owned graphics files.
        !remote,
        cancel,
    )
    .map_err(handshake_error)?;
    if handshake.encoding != RenderEncoding::SemanticFrame {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "endpoint did not negotiate the semantic client shell",
        ));
    }
    let negotiation = EndpointNegotiation::new(
        handshake.endpoint_methods.unwrap_or_default(),
        handshake.endpoint_capabilities.unwrap_or_default(),
    )
    .with_server_version(handshake.server_version);
    if !negotiation.supports_surface_interest() || (remote && !negotiation.supports_health_check())
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "this machine needs a server update before it can participate in multi-machine viewing",
        ));
    }
    let reader = stream.try_clone()?;
    let writer = NativeEndpointTransport::with_lifetime(stream, lifetime)?;
    Ok(PreparedEndpointConnection {
        reader,
        writer,
        negotiation,
    })
}

fn failure_needs_attention(error: &std::io::Error) -> bool {
    crate::remote::saved_ssh_failure_needs_attention(error)
}

fn lock_error_kinds(
    error_kinds: &std::sync::Mutex<HashMap<ClientEndpointId, crate::remote::ConnectionErrorKind>>,
) -> std::sync::MutexGuard<'_, HashMap<ClientEndpointId, crate::remote::ConnectionErrorKind>> {
    error_kinds
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn lock_forwards(
    forwards: &std::sync::Mutex<crate::remote::PortForwardManager>,
) -> std::sync::MutexGuard<'_, crate::remote::PortForwardManager> {
    forwards
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn handshake_error(error: crate::client::ClientError) -> std::io::Error {
    use crate::client::ClientError;
    use crate::protocol::FramingError;
    match error {
        ClientError::ConnectionFailed(error) | ClientError::ConnectionLost(error) => error,
        ClientError::HandshakeRejected { error, .. } => {
            std::io::Error::new(std::io::ErrorKind::Unsupported, error)
        }
        ClientError::Protocol(FramingError::Io(error)) => error,
        ClientError::Protocol(error) => {
            std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string())
        }
        ClientError::ServerShutdown { reason } => std::io::Error::new(
            std::io::ErrorKind::ConnectionAborted,
            reason.unwrap_or_else(|| "server shut down during handshake".into()),
        ),
    }
}

fn retry_delay(attempt: u32) -> Duration {
    INITIAL_RETRY_DELAY
        .saturating_mul(
            1_u32
                .checked_shl(attempt.saturating_sub(1).min(8))
                .unwrap_or(u32::MAX),
        )
        .min(MAX_RETRY_DELAY)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::endpoint::ProfileId;

    fn profile() -> super::super::SavedSshEndpoint {
        super::super::SavedSshEndpoint {
            id: ProfileId::parse("0123456789abcdef0123456789abcdef").unwrap(),
            label: "Build".into(),
            target: "build".into(),
            session: "agents".into(),
            enabled: true,
            ..super::super::SavedSshEndpoint::new("base", "base", "default")
                .expect("valid base profile")
        }
    }

    #[test]
    fn live_catalog_preserves_renamed_connections_and_local_recovery() {
        let now = Instant::now();
        let mut profile = profile();
        let id = ClientEndpointId::Ssh(profile.id.clone());
        let mut supervisors = EndpointSupervisors::new(&[profile.clone()], now);
        supervisors.add_local(PathBuf::from("local"), Some(1), now);
        let state = supervisors.endpoints.get_mut(&id).unwrap();
        state.generation = Some(7);
        state.attempts = 3;
        state.next_attempt = Some(now + Duration::from_secs(4));
        profile.label = "Renamed".into();
        assert!(supervisors.reconcile_profiles(&[profile], now).is_empty());
        let state = &supervisors.endpoints[&id];
        assert_eq!(state.generation, Some(7));
        assert_eq!(state.attempts, 3);
        assert_eq!(state.next_attempt, Some(now + Duration::from_secs(4)));
        assert_eq!(
            supervisors.endpoints[&ClientEndpointId::Local].generation,
            Some(1)
        );
    }

    #[test]
    fn live_catalog_add_disable_enable_and_remove_fence_late_connections() {
        let now = Instant::now();
        let mut profile = profile();
        let id = ClientEndpointId::Ssh(profile.id.clone());
        let mut supervisors = EndpointSupervisors::new(&[], now);
        assert!(supervisors
            .reconcile_profiles(&[profile.clone()], now)
            .is_empty());
        assert_eq!(supervisors.endpoints[&id].next_attempt, Some(now));
        supervisors.endpoints.get_mut(&id).unwrap().generation = Some(7);
        supervisors.next_generation = 8;
        profile.enabled = false;
        assert_eq!(
            supervisors.reconcile_profiles(&[profile.clone()], now),
            vec![id.clone()]
        );
        assert!(!supervisors.record_status(&id, 7, ClientEndpointStatus::Online, now));
        profile.enabled = true;
        assert!(supervisors
            .reconcile_profiles(&[profile.clone()], now)
            .is_empty());
        assert!(!supervisors.record_status(&id, 7, ClientEndpointStatus::Online, now));
        assert_eq!(supervisors.next_generation, 8);
        assert_eq!(supervisors.reconcile_profiles(&[], now), vec![id.clone()]);
        assert!(!supervisors.record_status(&id, 7, ClientEndpointStatus::Online, now));
    }

    #[test]
    fn live_catalog_destination_change_retires_only_that_machine() {
        let now = Instant::now();
        let mut changed = profile();
        let other = super::super::SavedSshEndpoint::new("Other", "other", "main").unwrap();
        let id = ClientEndpointId::Ssh(changed.id.clone());
        let other_id = ClientEndpointId::Ssh(other.id.clone());
        let mut supervisors = EndpointSupervisors::new(&[changed.clone(), other.clone()], now);
        supervisors.endpoints.get_mut(&id).unwrap().generation = Some(2);
        supervisors.endpoints.get_mut(&other_id).unwrap().generation = Some(3);
        changed.session = "another-session".into();
        assert_eq!(
            supervisors.reconcile_profiles(&[changed, other], now),
            vec![id.clone()]
        );
        assert_eq!(supervisors.endpoints[&id].generation, None);
        assert_eq!(supervisors.endpoints[&other_id].generation, Some(3));
    }

    #[test]
    fn brief_ssh_reconnections_do_not_reset_backoff() {
        let now = Instant::now();
        let profile = profile();
        let id = ClientEndpointId::Ssh(profile.id.clone());
        let mut supervisors = EndpointSupervisors::new(&[profile], now);
        supervisors.endpoints.get_mut(&id).unwrap().generation = Some(2);
        for attempt in 1..=5 {
            let connected = now + Duration::from_secs(attempt * 20);
            assert!(supervisors.record_status(&id, 2, ClientEndpointStatus::Online, connected));
            let failed = connected + Duration::from_secs(15);
            assert!(supervisors.disconnected(&id, 2, failed));
            assert_eq!(
                supervisors.endpoints[&id].next_attempt,
                Some(failed + INITIAL_RETRY_DELAY * (1 << (attempt - 1)))
            );
        }
        let connected = now + Duration::from_secs(200);
        assert!(supervisors.record_status(&id, 2, ClientEndpointStatus::Online, connected));
        let failed = connected + Duration::from_secs(60);
        assert!(supervisors.disconnected(&id, 2, failed));
        assert_eq!(
            supervisors.endpoints[&id].next_attempt,
            Some(failed + INITIAL_RETRY_DELAY)
        );
    }

    #[test]
    fn reconnect_now_reschedules_an_immediate_attempt_with_fresh_backoff() {
        let now = Instant::now();
        let profile = profile();
        let endpoint_id = ClientEndpointId::Ssh(profile.id.clone());
        let mut supervisors = EndpointSupervisors::new(&[profile], now);
        let state = supervisors.endpoints.get_mut(&endpoint_id).unwrap();
        state.generation = Some(3);
        state.attempts = 5;
        state.next_attempt = Some(now + Duration::from_secs(60));
        assert!(supervisors.reconnect_now(&endpoint_id, now));
        let state = &supervisors.endpoints[&endpoint_id];
        assert_eq!(state.attempts, 0);
        assert_eq!(state.next_attempt, Some(now));

        // Attention stops retries entirely; a manual reconnect resumes them.
        assert!(supervisors.record_status(&endpoint_id, 3, ClientEndpointStatus::Attention, now));
        assert!(supervisors.endpoints[&endpoint_id].next_attempt.is_none());
        assert!(supervisors.reconnect_now(&endpoint_id, now));
        assert_eq!(supervisors.endpoints[&endpoint_id].next_attempt, Some(now));

        // An in-flight attempt is left alone; unknown endpoints are rejected.
        supervisors
            .endpoints
            .get_mut(&endpoint_id)
            .unwrap()
            .in_flight = true;
        let scheduled = supervisors.endpoints[&endpoint_id].generation;
        assert!(supervisors.reconnect_now(&endpoint_id, now));
        assert_eq!(supervisors.endpoints[&endpoint_id].generation, scheduled);
        assert!(!supervisors.reconnect_now(&ClientEndpointId::Local, now));
    }

    #[test]
    fn retry_backoff_is_bounded() {
        assert_eq!(retry_delay(1), INITIAL_RETRY_DELAY);
        assert_eq!(retry_delay(100), MAX_RETRY_DELAY);
    }

    #[test]
    fn connection_error_kind_is_exposed_until_online_or_retired() {
        let now = Instant::now();
        let profile = profile();
        let endpoint_id = ClientEndpointId::Ssh(profile.id.clone());
        let mut supervisors = EndpointSupervisors::new(&[profile], now);
        assert!(supervisors.connection_error_kind(&endpoint_id).is_none());

        let kind = crate::remote::ConnectionErrorKind::HostKeyUnknown { fingerprint: None };
        lock_error_kinds(&supervisors.error_kinds).insert(endpoint_id.clone(), kind.clone());
        assert_eq!(
            supervisors.connection_error_kind(&endpoint_id),
            Some(kind.clone())
        );

        supervisors
            .endpoints
            .get_mut(&endpoint_id)
            .unwrap()
            .generation = Some(2);
        assert!(supervisors.record_status(&endpoint_id, 2, ClientEndpointStatus::Online, now));
        assert!(supervisors.connection_error_kind(&endpoint_id).is_none());

        lock_error_kinds(&supervisors.error_kinds).insert(endpoint_id.clone(), kind);
        assert_eq!(
            supervisors.reconcile_profiles(&[], now),
            vec![endpoint_id.clone()]
        );
        assert!(supervisors.connection_error_kind(&endpoint_id).is_none());
    }

    #[test]
    fn handshake_network_failures_retry_but_incompatibility_needs_attention() {
        let timeout = handshake_error(crate::client::ClientError::ConnectionLost(
            std::io::Error::new(std::io::ErrorKind::TimedOut, "timed out"),
        ));
        assert!(!failure_needs_attention(&timeout));
        let rejected = handshake_error(crate::client::ClientError::HandshakeRejected {
            version: 1,
            error: "surface capability missing".into(),
        });
        assert_eq!(rejected.kind(), std::io::ErrorKind::Unsupported);
        assert!(failure_needs_attention(&rejected));
    }

    #[test]
    fn healthy_local_only_retries_after_its_connection_fails() {
        let now = Instant::now();
        let mut supervisors = EndpointSupervisors::new(&[profile()], now);
        supervisors.add_local(PathBuf::from("local.sock"), Some(1), now);
        assert!(supervisors.endpoints[&ClientEndpointId::Local]
            .next_attempt
            .is_none());
        assert!(!supervisors.disconnected(&ClientEndpointId::Local, 0, now));
        assert!(supervisors.endpoints[&ClientEndpointId::Local]
            .next_attempt
            .is_none());
        assert!(supervisors.disconnected(&ClientEndpointId::Local, 1, now));
        assert_eq!(
            supervisors.endpoints[&ClientEndpointId::Local].next_attempt,
            Some(now + INITIAL_RETRY_DELAY)
        );
        assert_eq!(
            supervisors.endpoints[&ClientEndpointId::Ssh(profile().id)].next_attempt,
            Some(now)
        );
    }

    #[test]
    fn ssh_recovery_rejects_stale_generations_and_stops_retries_for_attention() {
        let now = Instant::now();
        let mut supervisors = EndpointSupervisors::new(&[profile()], now);
        let endpoint_id = ClientEndpointId::Ssh(profile().id);
        supervisors
            .endpoints
            .get_mut(&endpoint_id)
            .unwrap()
            .generation = Some(4);
        assert!(supervisors.record_status(&endpoint_id, 4, ClientEndpointStatus::Online, now));
        assert!(!supervisors.disconnected(&endpoint_id, 3, now));
        assert!(supervisors.endpoints[&endpoint_id].next_attempt.is_none());
        assert!(supervisors.disconnected(&endpoint_id, 4, now));
        assert_eq!(
            supervisors.endpoints[&endpoint_id].next_attempt,
            Some(now + INITIAL_RETRY_DELAY)
        );
        assert!(supervisors.record_status(&endpoint_id, 4, ClientEndpointStatus::Attention, now));
        assert!(supervisors.endpoints[&endpoint_id].next_attempt.is_none());
    }
}
