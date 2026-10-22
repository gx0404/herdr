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
// Allow Wi-Fi to return without restarting the historical failure ladder.
const WAKE_RECOVERY_PERIOD: Duration = Duration::from_secs(30);
const WAKE_RETRY_DELAY: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug)]
pub(crate) struct EndpointConnectOptions {
    pub(crate) cols: u16,
    pub(crate) rows: u16,
    pub(crate) cell_width_px: u32,
    pub(crate) cell_height_px: u32,
    pub(crate) pixel_geometry_exact: bool,
    pub(crate) pixel_mouse: bool,
    pub(crate) surface_size: ClientSurfaceSize,
    pub(crate) endpoint_keybindings: bool,
    pub(crate) mouse_capture: bool,
}

pub(crate) struct PreparedEndpointConnection {
    reader: crate::ipc::LocalStream,
    writer: NativeEndpointTransport,
    negotiation: EndpointNegotiation,
    attempt: Option<(crate::remote::TaskCancellation, Arc<AtomicBool>)>,
}

pub(crate) fn prepare_interactive_connection(
    connected: crate::remote::SavedSshStream,
    options: EndpointConnectOptions,
    cancel: &crate::remote::TaskCancellation,
) -> std::io::Result<PreparedEndpointConnection> {
    let completed = Arc::new(AtomicBool::new(false));
    let mut prepared = prepare_endpoint_connection(
        connected.stream,
        Box::new((connected.bridge, AttemptCompletion(Arc::clone(&completed)))),
        options,
        true,
        Some(cancel),
    )?;
    prepared.attempt = Some((cancel.clone(), completed));
    Ok(prepared)
}

pub(crate) enum EndpointSupervisorEvent {
    Status {
        endpoint_id: ClientEndpointId,
        generation: u64,
        status: ClientEndpointStatus,
        message: String,
        error_kind: Option<crate::remote::ConnectionErrorKind>,
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
    cancelled: Arc<AtomicBool>,
    completed: Arc<AtomicBool>,
    generation: Option<u64>,
    attempt_cancel: crate::remote::TaskCancellation,
    online_since: Option<Instant>,
    ready_since: Option<Instant>,
    retry_after_resume: bool,
}

impl ReconnectState {
    fn new(target: ConnectTarget, now: Instant) -> Self {
        let attempt_cancel = crate::remote::TaskCancellation::default();
        Self {
            target,
            attempts: 0,
            next_attempt: Some(now),
            in_flight: false,
            cancelled: attempt_cancel.flag(),
            completed: Arc::new(AtomicBool::new(true)),
            generation: None,
            attempt_cancel,
            online_since: None,
            ready_since: None,
            retry_after_resume: false,
        }
    }
}

impl Drop for ReconnectState {
    fn drop(&mut self) {
        self.attempt_cancel.cancel();
        self.cancelled.store(true, Ordering::Release);
    }
}

struct AttemptCompletion(Arc<AtomicBool>);

impl Drop for AttemptCompletion {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

pub(crate) struct EndpointSupervisors {
    endpoints: HashMap<ClientEndpointId, ReconnectState>,
    retired: HashMap<ClientEndpointId, Arc<AtomicBool>>,
    next_generation: u64,
    shutdown: Arc<AtomicBool>,
    /// Structured failure kind from the latest accepted status for each endpoint.
    error_kinds: HashMap<ClientEndpointId, crate::remote::ConnectionErrorKind>,
    /// Running SSH port forwards per endpoint: rebuilt after every successful
    /// connect and reconciled in place when catalog rules change underneath a
    /// healthy connection. A connection generation is prepared before its
    /// attempt and must still be current before that attempt may rebuild; this
    /// prevents a retired or cancelled connection from resurrecting old rules.
    forwards: Arc<std::sync::Mutex<crate::remote::PortForwardManager>>,
    /// Snapshot workers for profiles with session logging enabled; follows
    /// the same profile lifecycle as the connections themselves.
    session_logs: super::session_log::SessionLogManager,
    power_state: Option<crate::platform::HostPowerEvent>,
    wake_recovery_until: Option<Instant>,
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
            retired: HashMap::new(),
            next_generation: 2,
            shutdown: Arc::new(AtomicBool::new(false)),
            error_kinds: HashMap::new(),
            forwards: Arc::new(std::sync::Mutex::new(
                crate::remote::PortForwardManager::new(),
            )),
            session_logs,
            power_state: None,
            wake_recovery_until: None,
        }
    }

    pub(crate) fn adopt_interactive(
        &mut self,
        profile: super::SavedSshEndpoint,
        mut prepared: PreparedEndpointConnection,
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
        if let Some((cancel, completed)) = prepared.attempt.take() {
            state.attempt_cancel = cancel;
            state.completed = completed;
        }
        state.cancelled = state.attempt_cancel.flag();
        let attempt_cancel = state.attempt_cancel.clone();
        if let Some(previous) = self.endpoints.insert(endpoint_id.clone(), state) {
            if !previous.completed.load(Ordering::Acquire) {
                self.retired
                    .entry(endpoint_id.clone())
                    .or_insert_with(|| Arc::clone(&previous.completed));
            }
        }
        lock_forwards(&self.forwards).prepare_generation(&profile, generation);
        let profile_id = profile.id.clone();
        let forwards = self.forwards.clone();
        let event_tx = event_tx.clone();
        std::thread::spawn(move || {
            if attempt_cancel.is_cancelled()
                || !lock_forwards(&forwards).rebuild_if_current(&profile_id, generation)
            {
                return;
            }
            let _ = event_tx.blocking_send(EndpointSupervisorEvent::Connected {
                endpoint_id,
                generation,
                reader: prepared.reader,
                writer: prepared.writer,
                negotiation: prepared.negotiation,
            });
            attempt_cancel.complete();
        });
    }

    pub(crate) fn add_local(&mut self, path: PathBuf, generation: Option<u64>, now: Instant) {
        let mut state = ReconnectState::new(ConnectTarget::Local(path), now);
        state.generation = generation;
        if generation.is_some() {
            state.next_attempt = None;
        }
        if let Some(previous) = self.endpoints.insert(ClientEndpointId::Local, state) {
            if !previous.completed.load(Ordering::Acquire) {
                self.retired
                    .entry(ClientEndpointId::Local)
                    .or_insert_with(|| Arc::clone(&previous.completed));
            }
        }
    }

    pub(crate) fn reconcile_profiles(
        &mut self,
        profiles: &[super::SavedSshEndpoint],
        now: Instant,
    ) -> Vec<ClientEndpointId> {
        let mut retired = Vec::new();
        let retiring = &mut self.retired;
        self.endpoints.retain(|endpoint_id, state| {
            let ConnectTarget::Ssh(previous) = &state.target else {
                return true;
            };
            let keep = profiles.iter().any(|profile| {
                profile.id == previous.id && profile.enabled && profile.same_connection(previous)
            });
            if !keep {
                state.attempt_cancel.cancel();
                retired.push(endpoint_id.clone());
                // A stale Connected event still owns its bridge until the client rejects it.
                if !state.completed.load(Ordering::Acquire) {
                    retiring
                        .entry(endpoint_id.clone())
                        .or_insert_with(|| Arc::clone(&state.completed));
                }
            }
            keep
        });
        {
            let mut forwards = lock_forwards(&self.forwards);
            for endpoint_id in &retired {
                self.error_kinds.remove(endpoint_id);
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
            let mut forwards = lock_forwards(&self.forwards);
            forwards.update_profile(profile);
            // Forward rules follow the live connection: only apply edits while
            // online. Everything else is picked up by the rebuild on the next
            // successful connect.
            if rules_changed && state.online_since.is_some() {
                forwards.reconcile(profile);
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
        if self.power_state == Some(crate::platform::HostPowerEvent::Suspend) {
            return;
        }
        self.retired
            .retain(|_, completed| !completed.load(Ordering::Acquire));
        for (endpoint_id, state) in &mut self.endpoints {
            if state.cancelled.load(Ordering::Acquire) && state.completed.load(Ordering::Acquire) {
                state.in_flight = false;
            }
            if state.in_flight
                || !state.completed.load(Ordering::Acquire)
                || self.retired.contains_key(endpoint_id)
                || state.next_attempt.is_none_or(|deadline| deadline > now)
            {
                continue;
            }
            state.in_flight = true;
            state.attempt_cancel = crate::remote::TaskCancellation::default();
            state.cancelled = state.attempt_cancel.flag();
            state.completed = Arc::new(AtomicBool::new(false));
            let completed = Arc::clone(&state.completed);
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
            if let Some(profile) = &forward_profile {
                lock_forwards(&self.forwards).prepare_generation(profile, generation);
            }
            let attempt_cancel = state.attempt_cancel.clone();
            let event_tx = event_tx.clone();
            let shutdown = self.shutdown.clone();
            let forwards = self.forwards.clone();
            tokio::spawn(async move {
                if shutdown.load(Ordering::Acquire) {
                    attempt_cancel.cancel();
                    completed.store(true, Ordering::Release);
                    return;
                }
                let task_endpoint_id = endpoint_id.clone();
                let worker_cancel = attempt_cancel.clone();
                let result = tokio::task::spawn_blocking(move || {
                    let completion = AttemptCompletion(completed);
                    worker_cancel.run(|| {
                        let event = connect_once(
                            &target,
                            options,
                            endpoint_id,
                            generation,
                            &worker_cancel,
                            completion,
                        )?;
                        if worker_cancel.is_cancelled() {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::Interrupted,
                                "endpoint connection generation was cancelled",
                            ));
                        }
                        // The link is up: commit forwards only if this connection
                        // generation is still authoritative. The manager also uses
                        // the latest catalog profile, not the task's stale snapshot.
                        if let Some(profile) = &forward_profile {
                            let committed = lock_forwards(&forwards)
                                .rebuild_if_current(&profile.id, generation);
                            if !committed {
                                return Err(std::io::Error::new(
                                    std::io::ErrorKind::Interrupted,
                                    "endpoint connection generation was retired",
                                ));
                            }
                        }
                        Ok(event)
                    })
                })
                .await;
                complete_connection_attempt(
                    result,
                    task_endpoint_id,
                    generation,
                    &shutdown,
                    &attempt_cancel,
                    &event_tx,
                )
                .await;
            });
        }
    }

    /// The kind from the latest accepted status. An unclassified status,
    /// `Online`, `Disabled`, or retirement clears the previous diagnosis.
    pub(crate) fn connection_error_kind(
        &self,
        endpoint_id: &ClientEndpointId,
    ) -> Option<crate::remote::ConnectionErrorKind> {
        self.error_kinds.get(endpoint_id).cloned()
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
        error_kind: Option<crate::remote::ConnectionErrorKind>,
        now: Instant,
    ) -> bool {
        let Some(state) = self.endpoints.get_mut(endpoint_id) else {
            return false;
        };
        if state.generation != Some(generation) {
            return false;
        }
        match error_kind.filter(|_| {
            !matches!(
                status,
                ClientEndpointStatus::Online | ClientEndpointStatus::Disabled
            )
        }) {
            Some(kind) => {
                self.error_kinds.insert(endpoint_id.clone(), kind);
            }
            None => {
                self.error_kinds.remove(endpoint_id);
            }
        }
        state.in_flight = false;
        let retry_after_resume = std::mem::take(&mut state.retry_after_resume);
        match status {
            ClientEndpointStatus::Online => {
                state.online_since.get_or_insert(now);
                if endpoint_id.is_local() {
                    state.attempts = 0;
                }
                state.next_attempt = None;
            }
            // Attention stops retries: recovery runs in-UI (approved interactive
            // authentication or a manual reconnect via `reconnect_now`), and a
            // background BatchMode retry must not interleave with an approved
            // interactive attempt for the same machine.
            ClientEndpointStatus::Attention | ClientEndpointStatus::Disabled => {
                state.online_since = None;
                state.ready_since = None;
                state.next_attempt = None;
            }
            ClientEndpointStatus::Connecting | ClientEndpointStatus::Reconnecting => {
                state.online_since = None;
                // A brief maintenance wake can complete a handshake without restoring the link.
                if state.ready_since.take().is_some_and(|ready| {
                    now.saturating_duration_since(ready) >= STABLE_CONNECTION_PERIOD
                }) {
                    state.attempts = 0;
                }
                state.attempts = state.attempts.saturating_add(1);
                let delay = retry_delay(state.attempts);
                let delay = if retry_after_resume {
                    Duration::ZERO
                } else if self.wake_recovery_until.is_some_and(|until| now < until) {
                    delay.min(WAKE_RETRY_DELAY)
                } else {
                    delay
                };
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

    /// Returns whether live connections need a fresh probe, without replacing them.
    pub(crate) fn host_power_event(
        &mut self,
        event: crate::platform::HostPowerEvent,
        now: Instant,
    ) -> bool {
        use crate::platform::HostPowerEvent::*;
        if event == Suspend {
            if self.power_state == Some(Suspend) {
                return false;
            }
            self.power_state = Some(Suspend);
            self.wake_recovery_until = None;
            self.cancel_connecting(now);
            return false;
        }
        if (event == InteractiveResume && self.wake_recovery_until.is_some())
            || (event != InteractiveResume
                && self.power_state.is_some_and(|state| state != Suspend))
        {
            return false;
        }
        if self.power_state.is_none() {
            // Registration may have missed the preceding suspend notification.
            self.cancel_connecting(now);
        }
        self.power_state = Some(event);
        if event == InteractiveResume {
            self.wake_recovery_until = Some(now + WAKE_RECOVERY_PERIOD);
        }
        if matches!(event, InteractiveResume | UnclassifiedResume) {
            for state in self.endpoints.values_mut() {
                // A live transport may discover its wake failure only after the fresh probe.
                state.retry_after_resume = event == UnclassifiedResume
                    && state.next_attempt.is_none()
                    && state.generation.is_some();
                if state.next_attempt.is_some() {
                    state.next_attempt = Some(now);
                }
            }
        }
        true
    }

    fn cancel_connecting(&mut self, now: Instant) {
        for state in self.endpoints.values_mut() {
            state.ready_since = None;
            state.retry_after_resume = false;
            if state.in_flight {
                state.cancelled.store(true, Ordering::Release);
                state.generation = None;
                state.next_attempt = Some(now);
            }
        }
    }

    #[cfg(any(unix, test))]
    pub(crate) fn user_activity(&mut self, now: Instant) {
        if self.power_state == Some(crate::platform::HostPowerEvent::UnclassifiedResume) {
            self.host_power_event(crate::platform::HostPowerEvent::InteractiveResume, now);
        }
    }

    pub(crate) fn received(&mut self, endpoint_id: &ClientEndpointId, generation: u64) {
        if let Some(state) = self
            .endpoints
            .get_mut(endpoint_id)
            .filter(|state| state.generation == Some(generation))
        {
            state.retry_after_resume = false;
        }
    }

    pub(crate) fn mark_ready(
        &mut self,
        endpoint_id: &ClientEndpointId,
        generation: u64,
        now: Instant,
    ) {
        if let Some(state) = self
            .endpoints
            .get_mut(endpoint_id)
            .filter(|state| state.generation == Some(generation))
        {
            state.ready_since.get_or_insert(now);
        }
    }

    /// A user asked for this machine now, so skip its pending backoff and restart the ladder.
    /// Returns false when no attempt is waiting: it is online, in flight, or not retried at all.
    pub(crate) fn retry_now(&mut self, endpoint_id: &ClientEndpointId, now: Instant) -> bool {
        let Some(state) = self.endpoints.get_mut(endpoint_id) else {
            return false;
        };
        if state.in_flight || state.next_attempt.is_none() {
            return false;
        }
        state.attempts = 0;
        state.next_attempt = Some(now);
        true
    }

    /// Explicit recovery remains available after an attention failure paused retries.
    pub(crate) fn reconnect_now(&mut self, endpoint_id: &ClientEndpointId, now: Instant) -> bool {
        if self.retry_now(endpoint_id, now) {
            return true;
        }
        let Some(state) = self.endpoints.get_mut(endpoint_id) else {
            return false;
        };
        if !state.in_flight {
            state.attempts = 0;
            state.next_attempt = Some(now);
        }
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
            None,
            now,
        )
    }
}

impl Drop for EndpointSupervisors {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        for state in self.endpoints.values() {
            state.attempt_cancel.cancel();
        }
        lock_forwards(&self.forwards).stop_all();
        self.session_logs.stop_all();
    }
}

async fn complete_connection_attempt(
    result: Result<std::io::Result<EndpointSupervisorEvent>, tokio::task::JoinError>,
    endpoint_id: ClientEndpointId,
    generation: u64,
    shutdown: &AtomicBool,
    attempt_cancel: &crate::remote::TaskCancellation,
    event_tx: &tokio::sync::mpsc::Sender<EndpointSupervisorEvent>,
) {
    let event = match result {
        Ok(Ok(event)) => event,
        Ok(Err(error)) => EndpointSupervisorEvent::Status {
            endpoint_id,
            generation,
            status: if failure_needs_attention(&error) {
                ClientEndpointStatus::Attention
            } else {
                ClientEndpointStatus::Reconnecting
            },
            message: error.to_string(),
            error_kind: Some(crate::remote::classify_connection_error(&error)),
        },
        Err(error) => EndpointSupervisorEvent::Status {
            endpoint_id,
            generation,
            status: ClientEndpointStatus::Reconnecting,
            message: format!("endpoint connection task stopped unexpectedly: {error}"),
            error_kind: None,
        },
    };
    if !shutdown.load(Ordering::Acquire) && !attempt_cancel.is_cancelled() {
        let _ = event_tx.send(event).await;
    }
    attempt_cancel.complete();
}

fn connect_once(
    target: &ConnectTarget,
    options: EndpointConnectOptions,
    endpoint_id: ClientEndpointId,
    generation: u64,
    cancel: &crate::remote::TaskCancellation,
    completion: AttemptCompletion,
) -> Result<EndpointSupervisorEvent, std::io::Error> {
    let cancelled = cancel.flag();
    let (stream, lifetime): (_, Box<dyn Send>) = match target {
        ConnectTarget::Local(path) => {
            let stream = crate::ipc::connect_local_stream_cancellable(
                path,
                &cancelled,
                Instant::now() + super::super::handshake::LOCAL_HANDSHAKE_READ_TIMEOUT,
            )
            .map_err(|error| {
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
    let prepared = prepare_endpoint_connection(
        stream,
        Box::new((lifetime, completion)),
        options,
        !endpoint_id.is_local(),
        Some(cancel),
    )?;
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
        super::super::HandshakePixelCapabilities {
            exact_geometry: options.pixel_geometry_exact,
            sgr_mouse: options.pixel_mouse,
        },
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
    if cancel.is_some_and(crate::remote::TaskCancellation::is_cancelled) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::ConnectionAborted,
            "connection attempt cancelled",
        ));
    }
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
        attempt: None,
    })
}

fn failure_needs_attention(error: &std::io::Error) -> bool {
    crate::remote::saved_ssh_failure_needs_attention(error)
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

    fn connect_options() -> EndpointConnectOptions {
        EndpointConnectOptions {
            cols: 80,
            rows: 24,
            cell_width_px: 0,
            cell_height_px: 0,
            pixel_geometry_exact: false,
            pixel_mouse: false,
            surface_size: ClientSurfaceSize { cols: 80, rows: 24 },
            endpoint_keybindings: false,
            mouse_capture: false,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn replacement_waits_for_queued_connection_teardown() {
        use crate::protocol::endpoint::{EndpointServerWelcome, ENDPOINT_WELCOME_KIND};
        use crate::protocol::{self, ClientMessage, ServerMessage};
        use interprocess::local_socket::traits::Listener as _;
        use interprocess::local_socket::traits::Stream as _;
        let path = std::env::temp_dir().join(format!("hretired-{}.sock", std::process::id()));
        let listener = crate::ipc::bind_private_local_listener(&path).unwrap();
        let now = Instant::now();
        let mut supervisors = EndpointSupervisors::new(&[], now);
        supervisors.add_local(path.clone(), None, now);
        let (events, mut received) = tokio::sync::mpsc::channel(4);
        supervisors.spawn_due(now, connect_options(), &events);
        let mut peer = listener.accept().unwrap();
        let _: ClientMessage = protocol::read_message(&mut peer, protocol::MAX_FRAME_SIZE).unwrap();
        protocol::write_message(
            &mut peer,
            &ServerMessage::EndpointControl {
                kind: ENDPOINT_WELCOME_KIND.into(),
                data: serde_json::to_string(&EndpointServerWelcome::compatible(vec![
                    "client_shell.surface.set".into(),
                ]))
                .unwrap(),
            },
        )
        .unwrap();
        let queued = tokio::time::timeout(Duration::from_secs(2), received.recv())
            .await
            .unwrap()
            .unwrap();
        let EndpointSupervisorEvent::Connected { generation, .. } = &queued else {
            panic!("handshake failed");
        };
        let old_generation = *generation;
        // Repeated catalog replacements must keep the original unfinished barrier.
        for _ in 0..3 {
            supervisors.add_local(path.clone(), None, now);
            supervisors.spawn_due(now, connect_options(), &events);
            assert!(supervisors.endpoints[&ClientEndpointId::Local]
                .generation
                .is_none());
            assert_eq!(supervisors.retired.len(), 1);
        }
        assert!(!supervisors.record_status(
            &ClientEndpointId::Local,
            old_generation,
            ClientEndpointStatus::Online,
            None,
            now
        ));
        peer.set_nonblocking(true).unwrap();
        assert!(matches!(
            crate::ipc::poll_local_stream_read_count(&mut peer, &mut [0; 1]).unwrap(),
            crate::ipc::LocalStreamReadCount::Pending
        ));
        drop(queued);
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            supervisors.spawn_due(now, connect_options(), &events);
            if supervisors.endpoints[&ClientEndpointId::Local]
                .generation
                .is_some()
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "retired transport did not release its attempt"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(supervisors.retired.is_empty());
        assert!(matches!(
            crate::ipc::poll_local_stream_read_count(&mut peer, &mut [0; 1]).unwrap(),
            crate::ipc::LocalStreamReadCount::Closed
        ));
        let _replacement = listener.accept().unwrap();
        drop(supervisors);
        drop(listener);
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wake_replacement_waits_for_the_obsolete_attempt_to_close() {
        use crate::platform::HostPowerEvent::*;
        use crate::protocol::{self, ClientMessage};
        use interprocess::local_socket::traits::Listener as _;
        use interprocess::local_socket::traits::Stream as _;
        for observed_suspend in [true, false] {
            let path = std::env::temp_dir().join(format!(
                "hwake-{}-{observed_suspend}.sock",
                std::process::id()
            ));
            let listener = crate::ipc::bind_private_local_listener(&path).unwrap();
            let now = Instant::now();
            let mut supervisors = EndpointSupervisors::new(&[], now);
            supervisors.add_local(path.clone(), None, now);
            let (events, mut received) = tokio::sync::mpsc::channel(4);
            supervisors.spawn_due(now, connect_options(), &events);
            let mut peer = listener.accept().unwrap();
            let _: ClientMessage =
                protocol::read_message(&mut peer, protocol::MAX_FRAME_SIZE).unwrap();
            let completed = supervisors.endpoints[&ClientEndpointId::Local]
                .completed
                .clone();
            supervisors.host_power_event(
                if observed_suspend {
                    Suspend
                } else {
                    InteractiveResume
                },
                now,
            );
            assert!(supervisors.endpoints[&ClientEndpointId::Local].in_flight);
            if observed_suspend {
                supervisors.spawn_due(now, connect_options(), &events);
            }
            assert!(supervisors.endpoints[&ClientEndpointId::Local]
                .generation
                .is_none());
            let deadline = Instant::now() + Duration::from_secs(2);
            while !completed.load(Ordering::Acquire) {
                assert!(
                    Instant::now() < deadline,
                    "cancelled attempt did not finish"
                );
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            assert!(
                received.try_recv().is_err(),
                "cancelled attempts must not publish stale status"
            );
            peer.set_nonblocking(true).unwrap();
            // Windows interprocess drops flush through its background linger pool.
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                if matches!(
                    crate::ipc::poll_local_stream_read_count(&mut peer, &mut [0; 1]).unwrap(),
                    crate::ipc::LocalStreamReadCount::Closed
                ) {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "cancelled attempt retained its stream"
                );
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            supervisors.host_power_event(InteractiveResume, now);
            supervisors.spawn_due(now, connect_options(), &events);
            let replacement = supervisors.endpoints[&ClientEndpointId::Local].generation;
            assert_ne!(replacement, Some(2));
            assert!(replacement.is_some());
            for _ in 0..3 {
                supervisors.host_power_event(InteractiveResume, now);
                supervisors.spawn_due(now, connect_options(), &events);
                assert_eq!(
                    supervisors.endpoints[&ClientEndpointId::Local].generation,
                    replacement
                );
            }
            let _replacement = listener.accept().unwrap();
            drop(supervisors);
            drop(listener);
            std::fs::remove_file(path).unwrap();
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn retiring_or_shutting_down_a_silent_attempt_closes_its_connection() {
        use crate::protocol::{self, ClientMessage};
        use interprocess::local_socket::traits::Listener as _;
        use interprocess::local_socket::traits::Stream as _;
        for shutdown in [false, true] {
            let path = std::env::temp_dir()
                .join(format!("hcancel-{}-{shutdown}.sock", std::process::id()));
            let listener = crate::ipc::bind_private_local_listener(&path).unwrap();
            let now = Instant::now();
            let mut supervisors = EndpointSupervisors::new(&[], now);
            supervisors.add_local(path.clone(), None, now);
            let (events, mut received) = tokio::sync::mpsc::channel(4);
            supervisors.spawn_due(now, connect_options(), &events);
            let mut peer = listener.accept().unwrap();
            let _: ClientMessage =
                protocol::read_message(&mut peer, protocol::MAX_FRAME_SIZE).unwrap();
            drop(listener);
            std::fs::remove_file(path).unwrap();
            let started = Instant::now();
            if shutdown {
                drop(supervisors);
            } else {
                supervisors.add_local(PathBuf::from("replacement"), None, now);
                assert!(received.try_recv().is_err());
            }
            peer.set_nonblocking(true).unwrap();
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                if matches!(
                    crate::ipc::poll_local_stream_read_count(&mut peer, &mut [0; 1]).unwrap(),
                    crate::ipc::LocalStreamReadCount::Closed
                ) {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "cancelled attempt retained its stream"
                );
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            assert!(started.elapsed() < Duration::from_secs(1));
        }
    }
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
        assert!(!supervisors.record_status(&id, 7, ClientEndpointStatus::Online, None, now));
        profile.enabled = true;
        assert!(supervisors
            .reconcile_profiles(&[profile.clone()], now)
            .is_empty());
        assert!(!supervisors.record_status(&id, 7, ClientEndpointStatus::Online, None, now));
        assert_eq!(supervisors.next_generation, 8);
        assert_eq!(supervisors.reconcile_profiles(&[], now), vec![id.clone()]);
        assert!(!supervisors.record_status(&id, 7, ClientEndpointStatus::Online, None, now));
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
            assert!(supervisors.record_status(
                &id,
                2,
                ClientEndpointStatus::Online,
                None,
                connected
            ));
            let failed = connected + Duration::from_secs(15);
            assert!(supervisors.disconnected(&id, 2, failed));
            assert_eq!(
                supervisors.endpoints[&id].next_attempt,
                Some(failed + INITIAL_RETRY_DELAY * (1 << (attempt - 1)))
            );
        }
        let connected = now + Duration::from_secs(200);
        assert!(supervisors.record_status(&id, 2, ClientEndpointStatus::Online, None, connected));
        let failed = connected + Duration::from_secs(60);
        assert!(supervisors.disconnected(&id, 2, failed));
        assert_eq!(
            supervisors.endpoints[&id].next_attempt,
            Some(failed + Duration::from_secs(16))
        );

        let connected = now + Duration::from_secs(300);
        assert!(supervisors.record_status(&id, 2, ClientEndpointStatus::Online, None, connected));
        let ready = connected + Duration::from_secs(45);
        supervisors.mark_ready(&id, 2, ready);
        supervisors.mark_ready(&id, 2, ready + Duration::from_secs(30));
        let failed = ready + Duration::from_secs(60);
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
        assert!(supervisors.record_status(
            &endpoint_id,
            3,
            ClientEndpointStatus::Attention,
            None,
            now
        ));
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

    fn auth_kind() -> crate::remote::ConnectionErrorKind {
        crate::remote::ConnectionErrorKind::AuthRequired {
            methods: vec!["publickey".into(), "keyboard-interactive".into()],
            identity_file: Some("in-memory-identity".into()),
        }
    }

    async fn completion_result(
        endpoint_id: &ClientEndpointId,
        generation: u64,
        case: usize,
    ) -> Result<std::io::Result<EndpointSupervisorEvent>, tokio::task::JoinError> {
        match case {
            0 => Ok(Ok(EndpointSupervisorEvent::Status {
                endpoint_id: endpoint_id.clone(),
                generation,
                status: ClientEndpointStatus::Online,
                message: "synthetic successful completion".into(),
                error_kind: None,
            })),
            1 => Ok(Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "late timeout",
            ))),
            2 => Ok(Err(crate::remote::wrap_classified(
                std::io::Error::other("classified authentication failure"),
                auth_kind(),
            ))),
            3 => Ok(Err(std::io::Error::other("unclassified failure"))),
            4 => tokio::spawn(async { panic!("injected connection completion panic") }).await,
            5 => Ok(Err(crate::remote::wrap_classified(
                std::io::Error::other("Too many authentication failures"),
                crate::remote::ConnectionErrorKind::AuthDenied,
            ))),
            _ => unreachable!(),
        }
    }

    fn accept_completion(
        supervisors: &mut EndpointSupervisors,
        event: EndpointSupervisorEvent,
        now: Instant,
    ) -> bool {
        let EndpointSupervisorEvent::Status {
            endpoint_id,
            generation,
            status,
            error_kind,
            ..
        } = event
        else {
            panic!("expected synthetic status completion");
        };
        supervisors.record_status(&endpoint_id, generation, status, error_kind, now)
    }

    #[tokio::test]
    async fn late_connection_completions_preserve_new_generation_diagnostics() {
        for case in 0..6 {
            let now = Instant::now();
            let id = ClientEndpointId::Ssh(profile().id);
            let mut supervisors = EndpointSupervisors::new(&[profile()], now);
            supervisors.add_local(PathBuf::from("unused-local"), Some(1), now);
            assert!(supervisors.record_status(
                &ClientEndpointId::Local,
                1,
                ClientEndpointStatus::Reconnecting,
                Some(crate::remote::ConnectionErrorKind::Dns),
                now,
            ));
            let local_retry = supervisors.endpoints[&ClientEndpointId::Local].next_attempt;
            let result = completion_result(&id, 2, case).await;
            let (release, barrier) = tokio::sync::oneshot::channel();
            let (events, mut received) = tokio::sync::mpsc::channel(1);
            let stale_id = id.clone();
            let shutdown = supervisors.shutdown.clone();
            let worker = tokio::spawn(async move {
                barrier.await.unwrap();
                complete_connection_attempt(
                    result,
                    stale_id,
                    2,
                    &shutdown,
                    &crate::remote::TaskCancellation::default(),
                    &events,
                )
                .await;
            });
            supervisors.endpoints.get_mut(&id).unwrap().generation = Some(3);
            assert!(supervisors.record_status(
                &id,
                3,
                ClientEndpointStatus::Reconnecting,
                Some(auth_kind()),
                now,
            ));
            let retry = supervisors.endpoints[&id].next_attempt;
            let attempts = supervisors.endpoints[&id].attempts;
            release.send(()).unwrap();
            worker.await.unwrap();
            assert_eq!(
                supervisors.connection_error_kind(&id),
                Some(auth_kind()),
                "before stale event consumption, case={case}"
            );
            assert!(!accept_completion(
                &mut supervisors,
                received.try_recv().unwrap(),
                now
            ));
            assert_eq!(supervisors.connection_error_kind(&id), Some(auth_kind()));
            assert_eq!(supervisors.endpoints[&id].generation, Some(3));
            assert_eq!(supervisors.endpoints[&id].attempts, attempts);
            assert_eq!(supervisors.endpoints[&id].next_attempt, retry);
            assert!(!supervisors.endpoints[&id].in_flight);
            assert_eq!(
                supervisors.endpoints[&ClientEndpointId::Local].next_attempt,
                local_retry
            );
            assert_eq!(
                supervisors.connection_error_kind(&ClientEndpointId::Local),
                Some(crate::remote::ConnectionErrorKind::Dns)
            );
        }
    }

    #[tokio::test]
    async fn current_connection_completions_commit_kind_only_on_acceptance() {
        for case in 0..6 {
            let now = Instant::now();
            let id = ClientEndpointId::Ssh(profile().id);
            let mut supervisors = EndpointSupervisors::new(&[profile()], now);
            supervisors.endpoints.get_mut(&id).unwrap().generation = Some(3);
            assert!(supervisors.record_status(
                &id,
                3,
                ClientEndpointStatus::Attention,
                Some(auth_kind()),
                now,
            ));
            let (events, mut received) = tokio::sync::mpsc::channel(1);
            complete_connection_attempt(
                completion_result(&id, 3, case).await,
                id.clone(),
                3,
                &supervisors.shutdown,
                &crate::remote::TaskCancellation::default(),
                &events,
            )
            .await;
            assert_eq!(supervisors.connection_error_kind(&id), Some(auth_kind()));
            let event = received.try_recv().unwrap();
            let EndpointSupervisorEvent::Status {
                status, error_kind, ..
            } = &event
            else {
                panic!("expected status");
            };
            let expected = match case {
                0 | 4 => None,
                1 => Some(crate::remote::ConnectionErrorKind::Timeout),
                2 => Some(auth_kind()),
                3 => Some(crate::remote::ConnectionErrorKind::Other),
                5 => Some(crate::remote::ConnectionErrorKind::AuthDenied),
                _ => unreachable!(),
            };
            assert_eq!(error_kind, &expected);
            assert_eq!(
                *status,
                match case {
                    0 => ClientEndpointStatus::Online,
                    2 => ClientEndpointStatus::Attention,
                    _ => ClientEndpointStatus::Reconnecting,
                }
            );
            assert!(accept_completion(&mut supervisors, event, now));
            assert_eq!(supervisors.connection_error_kind(&id), expected);
            assert_eq!(
                supervisors.endpoints[&id].next_attempt,
                (!matches!(case, 0 | 2)).then_some(now + INITIAL_RETRY_DELAY)
            );
        }
    }

    #[tokio::test]
    async fn cancelled_connection_completions_cannot_restore_diagnostics() {
        for mode in 0..5 {
            for case in 0..2 {
                let now = Instant::now();
                let mut saved = profile();
                let id = ClientEndpointId::Ssh(saved.id.clone());
                let mut supervisors = EndpointSupervisors::new(&[saved.clone()], now);
                supervisors.endpoints.get_mut(&id).unwrap().generation = Some(2);
                let cancel = supervisors.endpoints[&id].attempt_cancel.clone();
                assert!(supervisors.record_status(
                    &id,
                    2,
                    ClientEndpointStatus::Attention,
                    Some(auth_kind()),
                    now,
                ));
                let result = completion_result(&id, 2, case).await;
                match mode {
                    0 => {
                        supervisors.reconcile_profiles(&[], now);
                    }
                    1 => {
                        saved.enabled = false;
                        supervisors.reconcile_profiles(&[saved], now);
                    }
                    2 => {
                        saved.session = "changed".into();
                        supervisors.reconcile_profiles(&[saved], now);
                    }
                    3 => cancel.cancel(),
                    4 => supervisors.shutdown.store(true, Ordering::Release),
                    _ => unreachable!(),
                }
                let expected = (mode >= 3).then_some(auth_kind());
                let (events, mut received) = tokio::sync::mpsc::channel(1);
                complete_connection_attempt(
                    result,
                    id.clone(),
                    2,
                    &supervisors.shutdown,
                    &cancel,
                    &events,
                )
                .await;
                assert!(received.try_recv().is_err());
                assert_eq!(supervisors.connection_error_kind(&id), expected);
                if mode == 2 {
                    assert_eq!(supervisors.endpoints[&id].generation, None);
                }
            }
        }
    }

    #[test]
    fn connection_error_kind_is_exposed_until_online_or_retired() {
        let now = Instant::now();
        let id = ClientEndpointId::Ssh(profile().id);
        let mut supervisors = EndpointSupervisors::new(&[profile()], now);
        supervisors.endpoints.get_mut(&id).unwrap().generation = Some(2);
        for status in [
            ClientEndpointStatus::Online,
            ClientEndpointStatus::Disabled,
            ClientEndpointStatus::Attention,
            ClientEndpointStatus::Reconnecting,
        ] {
            assert!(supervisors.record_status(
                &id,
                2,
                ClientEndpointStatus::Attention,
                Some(auth_kind()),
                now,
            ));
            let incoming = matches!(
                status,
                ClientEndpointStatus::Online | ClientEndpointStatus::Disabled
            )
            .then_some(auth_kind());
            assert!(supervisors.record_status(&id, 2, status, incoming, now));
            assert!(supervisors.connection_error_kind(&id).is_none());
        }
        assert!(supervisors.record_status(
            &id,
            2,
            ClientEndpointStatus::Attention,
            Some(auth_kind()),
            now,
        ));
        assert_eq!(supervisors.reconcile_profiles(&[], now), vec![id.clone()]);
        assert!(supervisors.connection_error_kind(&id).is_none());
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
        assert!(supervisors.record_status(
            &endpoint_id,
            4,
            ClientEndpointStatus::Online,
            None,
            now
        ));
        assert!(!supervisors.disconnected(&endpoint_id, 3, now));
        assert!(supervisors.endpoints[&endpoint_id].next_attempt.is_none());
        assert!(supervisors.disconnected(&endpoint_id, 4, now));
        assert_eq!(
            supervisors.endpoints[&endpoint_id].next_attempt,
            Some(now + INITIAL_RETRY_DELAY)
        );
        assert!(supervisors.record_status(
            &endpoint_id,
            4,
            ClientEndpointStatus::Attention,
            None,
            now
        ));
        assert!(supervisors.endpoints[&endpoint_id].next_attempt.is_none());
    }

    #[test]
    fn retired_connection_barrier_cannot_commit_forwards() {
        let now = Instant::now();
        let mut profile = profile();
        profile
            .port_forwards
            .push(crate::client::endpoint::PortForwardRule {
                kind: crate::client::endpoint::PortForwardKind::Remote,
                bind_address: None,
                listen_port: 18090,
                target_host: Some("127.0.0.1".into()),
                target_port: Some(80),
            });
        let endpoint_id = ClientEndpointId::Ssh(profile.id.clone());
        let mut supervisors = EndpointSupervisors::new(&[profile.clone()], now);
        lock_forwards(&supervisors.forwards).prepare_generation(&profile, 7);
        let cancel = supervisors.endpoints[&endpoint_id].attempt_cancel.clone();
        let forwards = supervisors.forwards.clone();
        let profile_id = profile.id.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (proceed_tx, proceed_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            ready_tx.send(()).expect("connection barrier ready");
            proceed_rx.recv().expect("connection barrier released");
            lock_forwards(&forwards).rebuild_if_current(&profile_id, 7)
        });
        ready_rx.recv().expect("connection worker reached barrier");

        profile.enabled = false;
        assert_eq!(
            supervisors.reconcile_profiles(&[profile.clone()], now),
            vec![endpoint_id]
        );
        assert!(cancel.is_cancelled());
        profile.enabled = true;
        assert!(supervisors.reconcile_profiles(&[profile], now).is_empty());
        proceed_tx.send(()).expect("release stale connection");
        assert!(!worker.join().expect("connection worker"));
    }
    #[test]
    fn a_handshake_without_readiness_does_not_reset_backoff() {
        let now = Instant::now();
        let profile = profile();
        let id = ClientEndpointId::Ssh(profile.id.clone());
        let mut supervisors = EndpointSupervisors::new(&[profile], now);
        let state = supervisors.endpoints.get_mut(&id).unwrap();
        state.generation = Some(2);
        state.attempts = 9;
        assert!(supervisors.record_status(&id, 2, ClientEndpointStatus::Online, None, now));
        let failed = now + Duration::from_secs(75);
        assert!(supervisors.disconnected(&id, 2, failed));
        assert_eq!(
            supervisors.endpoints[&id].next_attempt,
            Some(failed + MAX_RETRY_DELAY)
        );
    }

    #[test]
    fn requested_retry_skips_backoff_without_duplicating_attempts() {
        let now = Instant::now();
        let profile = profile();
        let id = ClientEndpointId::Ssh(profile.id.clone());
        let mut supervisors = EndpointSupervisors::new(&[profile], now);
        let state = supervisors.endpoints.get_mut(&id).unwrap();
        state.generation = Some(2);
        state.attempts = 9;
        assert!(supervisors.disconnected(&id, 2, now));
        assert_eq!(
            supervisors.endpoints[&id].next_attempt,
            Some(now + MAX_RETRY_DELAY)
        );

        let clicked = now + Duration::from_secs(5);
        assert!(supervisors.retry_now(&id, clicked));
        assert_eq!(supervisors.endpoints[&id].next_attempt, Some(clicked));
        // A failed requested attempt resumes from the shortest delay.
        assert!(supervisors.disconnected(&id, 2, clicked));
        assert_eq!(
            supervisors.endpoints[&id].next_attempt,
            Some(clicked + INITIAL_RETRY_DELAY)
        );

        supervisors.endpoints.get_mut(&id).unwrap().in_flight = true;
        assert!(!supervisors.retry_now(&id, clicked));
        assert!(supervisors.record_status(&id, 2, ClientEndpointStatus::Online, None, clicked));
        assert!(!supervisors.retry_now(&id, clicked));
        assert!(supervisors.endpoints[&id].next_attempt.is_none());
    }

    #[test]
    fn interactive_wake_retries_slow_wifi_without_renewing_the_window_or_failure_history() {
        use crate::platform::HostPowerEvent::*;
        let now = Instant::now();
        let profile = profile();
        let id = ClientEndpointId::Ssh(profile.id.clone());
        let mut supervisors = EndpointSupervisors::new(&[profile], now);
        let state = supervisors.endpoints.get_mut(&id).unwrap();
        state.generation = Some(2);
        state.attempts = 9;
        state.next_attempt = Some(now + Duration::from_secs(120));
        supervisors.host_power_event(Suspend, now);
        assert!(supervisors.host_power_event(AutomaticResume, now));
        assert_eq!(
            supervisors.endpoints[&id].next_attempt,
            Some(now + Duration::from_secs(120))
        );
        assert!(supervisors.host_power_event(InteractiveResume, now));
        assert_eq!(supervisors.endpoints[&id].next_attempt, Some(now));
        assert_eq!(supervisors.endpoints[&id].attempts, 9);
        // Wi-Fi remains unavailable for twenty seconds after the interactive wake.
        for seconds in (0..=20).step_by(2) {
            let failed = now + Duration::from_secs(seconds);
            assert!(supervisors.disconnected(&id, 2, failed));
            assert_eq!(
                supervisors.endpoints[&id].next_attempt,
                Some(failed + Duration::from_secs(2))
            );
        }
        let failures = supervisors.endpoints[&id].attempts;
        let late = now + Duration::from_secs(31);
        for event in [
            AutomaticResume,
            InteractiveResume,
            UnclassifiedResume,
            InteractiveResume,
        ] {
            assert!(!supervisors.host_power_event(event, late));
        }
        supervisors.disconnected(&id, 2, late);
        assert_eq!(supervisors.endpoints[&id].attempts, failures + 1);
        assert_eq!(
            supervisors.endpoints[&id].next_attempt,
            Some(late + Duration::from_secs(120))
        );
        supervisors.host_power_event(Suspend, late);
        assert!(supervisors.host_power_event(InteractiveResume, late));
        assert_eq!(supervisors.endpoints[&id].next_attempt, Some(late));
    }

    #[test]
    fn unclassified_resume_expedites_once_until_user_activity() {
        use crate::platform::HostPowerEvent::*;
        let now = Instant::now();
        let profile = profile();
        let id = ClientEndpointId::Ssh(profile.id.clone());
        let mut supervisors = EndpointSupervisors::new(&[profile], now);
        let state = supervisors.endpoints.get_mut(&id).unwrap();
        state.generation = Some(2);
        state.attempts = 9;
        state.next_attempt = Some(now + Duration::from_secs(120));
        supervisors.host_power_event(Suspend, now);
        supervisors.host_power_event(UnclassifiedResume, now);
        assert_eq!(supervisors.endpoints[&id].next_attempt, Some(now));
        supervisors.disconnected(&id, 2, now);
        assert!(!supervisors.host_power_event(UnclassifiedResume, now));
        assert_eq!(
            supervisors.endpoints[&id].next_attempt,
            Some(now + Duration::from_secs(120))
        );
        supervisors.user_activity(now);
        assert_eq!(supervisors.endpoints[&id].next_attempt, Some(now));
        let deadline = supervisors.wake_recovery_until;
        supervisors.user_activity(now + Duration::from_secs(20));
        supervisors.host_power_event(UnclassifiedResume, now + Duration::from_secs(40));
        supervisors.user_activity(now + Duration::from_secs(40));
        assert_eq!(supervisors.wake_recovery_until, deadline);
        assert_eq!(supervisors.endpoints[&id].attempts, 10);
    }

    #[test]
    fn unclassified_wake_expedites_a_live_connection_after_its_fresh_probe_fails() {
        use crate::platform::HostPowerEvent::*;
        for healthy in [false, true] {
            let now = Instant::now();
            let profile = profile();
            let id = ClientEndpointId::Ssh(profile.id.clone());
            let mut supervisors = EndpointSupervisors::new(&[profile], now);
            let state = supervisors.endpoints.get_mut(&id).unwrap();
            state.generation = Some(2);
            state.attempts = 9;
            state.next_attempt = None;
            supervisors.host_power_event(Suspend, now);
            supervisors.host_power_event(UnclassifiedResume, now);
            // Only traffic from the current transport consumes its recovery allowance.
            supervisors.received(&id, if healthy { 2 } else { 1 });
            let expired = now + Duration::from_secs(10);
            supervisors.disconnected(&id, 2, expired);
            assert_eq!(
                supervisors.endpoints[&id].next_attempt,
                Some(
                    expired
                        + if healthy {
                            Duration::from_secs(120)
                        } else {
                            Duration::ZERO
                        }
                )
            );
            supervisors.disconnected(&id, 2, expired);
            assert_eq!(
                supervisors.endpoints[&id].next_attempt,
                Some(expired + Duration::from_secs(120))
            );
            assert_eq!(supervisors.endpoints[&id].attempts, 11);
        }
    }

    #[test]
    fn requested_retry_does_not_revive_local_needing_attention() {
        let now = Instant::now();
        let mut supervisors = EndpointSupervisors::new(&[], now);
        supervisors.add_local(PathBuf::from("local.sock"), Some(1), now);
        assert!(supervisors.record_status(
            &ClientEndpointId::Local,
            1,
            ClientEndpointStatus::Attention,
            None,
            now
        ));
        assert!(!supervisors.retry_now(&ClientEndpointId::Local, now));
        assert!(supervisors.endpoints[&ClientEndpointId::Local]
            .next_attempt
            .is_none());
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn adopted_interactive_connection_keeps_its_cancel_and_teardown_barrier() {
        use interprocess::local_socket::traits::Listener as _;
        for retire in [false, true] {
            let dirs = crate::config::test_dirs::isolate_dirs("interactive-barrier");
            let path = std::env::temp_dir().join(format!(
                "hadopt-{}-{}.sock",
                std::process::id(),
                crate::config::test_dirs::unique_id()
            ));
            let listener = crate::ipc::bind_private_local_listener(&path).unwrap();
            let stream = crate::ipc::connect_local_stream(&path).unwrap();
            let peer = listener.accept().unwrap();
            let cancel = crate::remote::TaskCancellation::default();
            // The auth worker has already handed ownership to the UI.
            cancel.complete();
            let completed = Arc::new(AtomicBool::new(false));
            let prepared = PreparedEndpointConnection {
                reader: stream.try_clone().unwrap(),
                writer: NativeEndpointTransport::with_lifetime(
                    stream,
                    AttemptCompletion(completed.clone()),
                )
                .unwrap(),
                negotiation: EndpointNegotiation::new(Vec::new(), Vec::new()),
                attempt: Some((cancel.clone(), completed.clone())),
            };
            let now = Instant::now();
            let mut profile = profile();
            let id = ClientEndpointId::Ssh(profile.id.clone());
            let mut supervisors = EndpointSupervisors::new(&[profile.clone()], now);
            let (events, mut received) = tokio::sync::mpsc::channel(4);
            supervisors.adopt_interactive(profile.clone(), prepared, now, &events);
            let queued = tokio::time::timeout(Duration::from_secs(30), received.recv())
                .await
                .unwrap()
                .unwrap();
            assert!(!completed.load(Ordering::Acquire));
            if retire {
                profile.target = "replacement".into();
                assert_eq!(
                    supervisors.reconcile_profiles(&[profile], now),
                    vec![id.clone()]
                );
            } else {
                supervisors.host_power_event(crate::platform::HostPowerEvent::Suspend, now);
                supervisors
                    .host_power_event(crate::platform::HostPowerEvent::InteractiveResume, now);
            }
            assert!(cancel.is_cancelled());
            supervisors.spawn_due(now, connect_options(), &events);
            assert!(supervisors.endpoints[&id].generation.is_none());
            drop(queued);
            let deadline = Instant::now() + Duration::from_secs(30);
            while !completed.load(Ordering::Acquire) {
                assert!(
                    Instant::now() < deadline,
                    "interactive transport retained its completion guard"
                );
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            drop((supervisors, peer, listener, dirs));
            let _ = std::fs::remove_file(path);
        }
    }
}
