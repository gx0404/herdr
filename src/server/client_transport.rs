//! Blocking client socket transport for the headless server.
//!
//! This module owns the thin-client handshake, read loop, and writer loop.
//! It converts socket I/O into [`ServerEvent`] values consumed by
//! `HeadlessServer`.

use std::collections::VecDeque;
#[cfg(test)]
use std::io::Write;
use std::io::{self, Read};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{SendError, TrySendError};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

#[cfg(unix)]
use interprocess::local_socket::traits::Stream as _;
#[cfg(unix)]
use interprocess::TryClone as _;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::ipc::LocalStream;
use crate::protocol::endpoint::{
    EndpointClientHello, EndpointServerWelcome, ENDPOINT_HELLO_KIND, ENDPOINT_PROTOCOL_GENERATION,
    ENDPOINT_WELCOME_KIND,
};
use crate::protocol::{
    self, AttachScrollDirection, AttachScrollSource, ClientMessage, ClientPaneInputEvent,
    RenderEncoding, ServerMessage, MAX_CLIPBOARD_IMAGE_PAYLOAD, MAX_FRAME_SIZE,
    MAX_GRAPHICS_FRAME_SIZE, PROTOCOL_VERSION,
};

/// Minimum accepted attached client size.
///
/// Narrow observers must be allowed to drive narrow renders, otherwise the
/// server wraps pane content against a wider width and the client sees the
/// right edge clipped.
const MIN_CLIENT_COLS: u16 = 1;
const MIN_CLIENT_ROWS: u16 = 1;

/// How long to wait for a client handshake before closing the connection.
/// Set to 4 seconds (rather than 5) to guarantee the connection is closed
/// within the 5-second deadline, even with OS timer slack, thread scheduling,
/// and cleanup overhead.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(4);

/// Maximum number of connections allowed to remain in the initial handshake.
///
/// A client that connects and never sends a hello must not be able to consume an
/// unbounded number of blocking handshake threads. The permit is released as soon
/// as the welcome has been queued, before the normal client read loop begins.
pub(crate) const MAX_CONCURRENT_CLIENT_HANDSHAKES: usize = 32;

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HandshakeTestStage {
    BeforeRegister,
    BeforeSpawn,
    AfterSpawn,
}

#[cfg(test)]
#[derive(Debug)]
struct HandshakeTestHook {
    stage: HandshakeTestStage,
    arrived: std::sync::mpsc::Sender<Arc<ClientWriterQueue>>,
    resume: std::sync::mpsc::Receiver<()>,
}

#[cfg(test)]
pub(crate) struct ClientHandshakeTestPause {
    arrived: std::sync::mpsc::Receiver<Arc<ClientWriterQueue>>,
    resume: std::sync::mpsc::Sender<()>,
}

#[cfg(test)]
impl ClientHandshakeTestPause {
    pub(crate) fn wait(&self) -> ClientTransportTestHandle {
        ClientTransportTestHandle(self.arrived.recv_timeout(Duration::from_secs(30)).unwrap())
    }
}

#[cfg(test)]
impl Drop for ClientHandshakeTestPause {
    fn drop(&mut self) {
        let _ = self.resume.send(());
    }
}

type ClientTransportRegistry =
    Option<std::collections::HashMap<usize, std::sync::Weak<ClientWriterQueue>>>;

#[derive(Debug)]
struct ClientTransportRegistration {
    owner: std::sync::Weak<ClientHandshakeLimiter>,
    identity: usize,
}

impl Drop for ClientTransportRegistration {
    fn drop(&mut self) {
        if let Some(owner) = self.owner.upgrade() {
            if let Some(transports) = owner.lock_transports().as_mut() {
                transports.remove(&self.identity);
            }
        }
    }
}

#[derive(Debug)]
pub(crate) struct ClientHandshakeLimiter {
    active: AtomicUsize,
    limit: usize,
    transports: Mutex<ClientTransportRegistry>,
    #[cfg(test)]
    checkpoint: Mutex<Option<HandshakeTestHook>>,
}

impl ClientHandshakeLimiter {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            active: AtomicUsize::new(0),
            limit: MAX_CONCURRENT_CLIENT_HANDSHAKES,
            transports: Mutex::new(Some(std::collections::HashMap::new())),
            #[cfg(test)]
            checkpoint: Mutex::new(None),
        })
    }

    #[cfg(all(test, windows))]
    fn with_limit(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            active: AtomicUsize::new(0),
            limit,
            transports: Mutex::new(Some(std::collections::HashMap::new())),
            checkpoint: Mutex::new(None),
        })
    }

    fn lock_transports(&self) -> std::sync::MutexGuard<'_, ClientTransportRegistry> {
        self.transports
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    fn register(self: &Arc<Self>, queue: &Arc<ClientWriterQueue>) -> bool {
        let mut state = queue.lock_state();
        let mut transports = self.lock_transports();
        let Some(transports) = transports.as_mut() else {
            return false;
        };
        let identity = Arc::as_ptr(queue) as usize;
        transports.insert(identity, Arc::downgrade(queue));
        state.registration = Some(ClientTransportRegistration {
            owner: Arc::downgrade(self),
            identity,
        });
        true
    }

    pub(crate) fn close_transports(&self) -> Vec<ClientTransportHandle> {
        self.lock_transports()
            .take()
            .into_iter()
            .flat_map(|transports| transports.into_values())
            .filter_map(|queue| queue.upgrade().map(ClientTransportHandle))
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn test_transport_count(&self) -> usize {
        self.lock_transports()
            .as_ref()
            .map_or(0, |entries| entries.len())
    }

    #[cfg(test)]
    pub(crate) fn test_pause_at(&self, stage: HandshakeTestStage) -> ClientHandshakeTestPause {
        let (arrived_tx, arrived) = std::sync::mpsc::channel();
        let (resume, resume_rx) = std::sync::mpsc::channel();
        *self.checkpoint.lock().unwrap() = Some(HandshakeTestHook {
            stage,
            arrived: arrived_tx,
            resume: resume_rx,
        });
        ClientHandshakeTestPause { arrived, resume }
    }

    #[cfg(test)]
    fn test_checkpoint(&self, queue: &Arc<ClientWriterQueue>, stage: HandshakeTestStage) {
        let hook = {
            let mut hook = self.checkpoint.lock().unwrap();
            if hook.as_ref().is_some_and(|hook| hook.stage == stage) {
                hook.take()
            } else {
                None
            }
        };
        if let Some(hook) = hook {
            let _ = hook.arrived.send(queue.clone());
            let _ = hook.resume.recv_timeout(Duration::from_secs(30));
        }
    }

    pub(crate) fn try_acquire(self: &Arc<Self>) -> Option<ClientHandshakePermit> {
        let mut active = self.active.load(Ordering::Acquire);
        loop {
            if active >= self.limit {
                return None;
            }
            match self.active.compare_exchange_weak(
                active,
                active.saturating_add(1),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Some(ClientHandshakePermit {
                        limiter: self.clone(),
                    });
                }
                Err(current) => active = current,
            }
        }
    }

    #[cfg(all(test, windows))]
    fn active(&self) -> usize {
        self.active.load(Ordering::Acquire)
    }
}

#[derive(Debug)]
pub(crate) struct ClientHandshakePermit {
    limiter: Arc<ClientHandshakeLimiter>,
}

impl Drop for ClientHandshakePermit {
    fn drop(&mut self) {
        self.limiter.active.fetch_sub(1, Ordering::Release);
    }
}

#[cfg(unix)]
const OBSERVER_WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// HSR-06/RS-16：客户端连接（含 TUI / endpoint 客户端）的发送停滞上限。
/// `write_client_stream` 在每次成功写入后重置计时，所以只有「完全不再读」的
/// 客户端会被断开。
const CLIENT_WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// HSR-06/RS-16：control 车道（可靠消息：关闭、通知、clipboard、投影）每客户端
/// 的排队字节上限。control 不能像 render 一样静默丢弃；超过上限说明客户端已经
/// 不读了，直接断开比无界增长安全。
const MAX_CLIENT_CONTROL_BACKLOG_BYTES: usize = 4 * 1024 * 1024;
/// 同一 backlog 的消息条数上限（防止大量小消息只吃字节计数）。
const MAX_CLIENT_CONTROL_BACKLOG_MESSAGES: usize = 4096;
const ENDPOINT_RESERVED_CONTROL_BYTES: usize = MAX_FRAME_SIZE + 4;
const ENDPOINT_RESERVED_CONTROL_MESSAGES: usize = 64;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ClientWriterCredits {
    pub(crate) render: bool,
    pub(crate) endpoint: bool,
}

/// Maximum input payload size (bytes) for a single `ClientMessage::Input`.
const MAX_INPUT_PAYLOAD: usize = 1024 * 1024; // 1 MB
const MAX_CLIENT_SHELL_DIMENSION: u16 = 4096;
const MAX_CLIENT_SHELL_CELLS: u32 = 1_000_000;
const MAX_CLIENT_CELL_SIZE_PX: u32 = 4096;

fn client_shell_geometry_error(
    surface_size: crate::protocol::ClientSurfaceSize,
    cell_width_px: u32,
    cell_height_px: u32,
) -> Option<&'static str> {
    if surface_size.cols == 0 || surface_size.rows == 0 {
        return Some("client shell requires a non-empty pane surface");
    }
    if surface_size.cols > MAX_CLIENT_SHELL_DIMENSION
        || surface_size.rows > MAX_CLIENT_SHELL_DIMENSION
        || u32::from(surface_size.cols) * u32::from(surface_size.rows) > MAX_CLIENT_SHELL_CELLS
    {
        return Some("client shell pane surface exceeds the safe geometry limit");
    }
    if cell_width_px > MAX_CLIENT_CELL_SIZE_PX || cell_height_px > MAX_CLIENT_CELL_SIZE_PX {
        return Some("client shell cell pixel size exceeds the safe geometry limit");
    }
    None
}

/// 从 endpoint hello 提取的 client shell 连接选项，`ServerEvent::ClientShellConnected`
/// 的字段来源。字段名与 `EndpointClientHello` 对齐。
#[derive(Debug)]
struct ClientShellHelloOptions {
    pixel_mouse: bool,
    direct_graphics: bool,
    endpoint_keybindings: bool,
    mouse_capture: bool,
    surface_active: bool,
    surface_reuse: bool,
    surface_delta: bool,
    ssh_auth_sock: Option<String>,
    surface_scroll: bool,
}

#[derive(serde::Deserialize)]
struct EndpointRequestHead {
    id: String,
    method: String,
}

enum DecodedEndpointRequest {
    Dispatch(Box<crate::api::schema::Request>),
    Error {
        request_id: String,
        code: &'static str,
        message: String,
    },
}

fn write_rejection(stream: &mut crate::platform::ServerClientStream, message: &ServerMessage) {
    if let Ok(control) = crate::platform::client_stream_control(stream) {
        control.set_deadline(Instant::now() + HANDSHAKE_TIMEOUT);
        if protocol::write_message(stream, message).is_ok() {
            if let Err(error) = crate::platform::finish_client_stream(stream) {
                debug!(%error, "client rejection drain aborted");
            }
        }
        control.shutdown();
    }
}

fn write_endpoint_rejection(
    stream: &mut crate::platform::ServerClientStream,
    code: &str,
    message: impl Into<String>,
) {
    let welcome = EndpointServerWelcome::incompatible(code, message);
    let response = ServerMessage::EndpointControl {
        kind: ENDPOINT_WELCOME_KIND.into(),
        data: serde_json::to_string(&welcome).unwrap_or_else(|_| "{}".into()),
    };
    write_rejection(stream, &response);
}

fn decode_endpoint_request(request: &str) -> serde_json::Result<DecodedEndpointRequest> {
    let head = serde_json::from_str::<EndpointRequestHead>(request)?;
    if !crate::server::client_commands::supports_client_shell_method_name(&head.method) {
        return Ok(DecodedEndpointRequest::Error {
            request_id: head.id,
            code: "unsupported_method",
            message: format!("method {:?} is not available on this machine", head.method),
        });
    }
    Ok(
        match serde_json::from_str::<crate::api::schema::Request>(request) {
            Ok(request) => DecodedEndpointRequest::Dispatch(Box::new(request)),
            Err(error) => DecodedEndpointRequest::Error {
                request_id: head.id,
                code: "invalid_request",
                message: format!("invalid endpoint request: {error}"),
            },
        },
    )
}
/// Maximum structured input events accepted in one client message.
const MAX_INPUT_EVENT_BATCH: usize = 4096;

/// Channels owned by the server side of a client writer thread.
#[derive(Clone, Debug)]
pub(crate) struct ClientWriter {
    /// Reliable control messages such as shutdown, notifications, and clipboard writes.
    pub(crate) control: ClientControlWriter,
    /// Droppable render messages. Capacity is one so slow clients cannot build lag.
    pub(crate) render: ClientRenderWriter,
}

#[derive(Debug)]
pub(crate) struct ClientTransportHandle(Arc<ClientWriterQueue>);

impl ClientTransportHandle {
    pub(crate) fn identity(&self) -> usize {
        Arc::as_ptr(&self.0) as usize
    }

    pub(crate) fn shutdown(&self, reason: &str, deadline: Option<Instant>) {
        self.0.shutdown(reason, deadline);
    }

    pub(crate) fn seal_until(&self, deadline: Instant) {
        self.0.seal(Some(deadline));
    }

    pub(crate) fn abort(&self) {
        self.0.close_writer();
    }

    pub(crate) fn was_aborted(&self) -> bool {
        self.0.lock_state().phase == ClientCloseState::Aborted
    }

    pub(crate) fn is_complete(&self) -> bool {
        let state = self.0.lock_state();
        !state.reader_running && !state.writer_running
    }

    pub(crate) async fn wait_complete(&self) {
        loop {
            let notified = self.0.completed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_complete() {
                return;
            }
            notified.await;
        }
    }
}

pub(crate) struct ClientEventAcknowledgement(Arc<ClientWriterQueue>);

impl ClientEventAcknowledgement {
    pub(crate) fn complete(self) -> bool {
        let mut state = self.0.lock_state();
        state.pending_events = state.pending_events.saturating_sub(1);
        self.0.publish_notifications(&state)
    }
}

#[cfg(test)]
pub(crate) use tests::endpoint_hello as test_endpoint_hello;

#[cfg(test)]
pub(crate) struct ClientTransportTestHandle(Arc<ClientWriterQueue>);

#[cfg(test)]
impl ClientTransportTestHandle {
    pub(crate) fn send_control(&self, message: &ServerMessage) {
        let mut frame = Vec::new();
        protocol::write_message(&mut frame, message).unwrap();
        self.0.send_control(frame).unwrap();
    }

    pub(crate) fn was_aborted(&self) -> bool {
        self.0.lock_state().phase == ClientCloseState::Aborted
    }

    pub(crate) fn writer_started(&self) -> bool {
        self.0.lock_state().writer_started
    }

    pub(crate) fn wait_writer_started(&self) {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut state = self.0.lock_state();
        while !state.writer_started {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .expect("writer starts");
            state = self.0.ready.wait_timeout(state, remaining).unwrap().0;
        }
    }

    pub(crate) fn abort(&self) {
        self.0.close_writer();
    }

    pub(crate) fn is_complete(&self) -> bool {
        let state = self.0.lock_state();
        !state.reader_running && !state.writer_running
    }

    pub(crate) fn wait_reader_started(&self) {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut state = self.0.lock_state();
        while !state.reader_waiting {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .expect("reader starts");
            state = self.0.ready.wait_timeout(state, remaining).unwrap().0;
        }
    }
}

#[cfg(test)]
thread_local! {
    static TEST_NOTIFICATION_TAKES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

impl ClientWriter {
    #[cfg(test)]
    pub(crate) fn test_transport_handle(&self) -> ClientTransportTestHandle {
        ClientTransportTestHandle(self.control.queue.clone())
    }

    #[cfg(test)]
    pub(crate) fn test_notification_take_count() -> usize {
        TEST_NOTIFICATION_TAKES.with(std::cell::Cell::get)
    }

    pub(crate) fn acknowledge_event_after_dispatch(&self) -> ClientEventAcknowledgement {
        ClientEventAcknowledgement(self.control.queue.clone())
    }

    pub(crate) fn defer_removal_for_accepted_events(&self) -> bool {
        {
            let mut state = self.control.queue.lock_state();
            if state.pending_events == 0 {
                state.disconnect_notified = true;
                return false;
            }
            if !state.disconnect_notified {
                state.disconnect_notified = true;
                state.disconnect_pending = Some(false);
                self.control.queue.publish_notifications(&state);
            }
        }
        self.seal();
        true
    }

    pub(crate) fn seal(&self) {
        self.control.queue.seal(None);
    }

    pub(crate) fn transport(&self) -> ClientTransportHandle {
        ClientTransportHandle(self.control.queue.clone())
    }

    #[cfg(all(test, windows))]
    pub(crate) fn seal_until(&self, deadline: Instant) {
        self.control.queue.seal(Some(deadline));
    }

    pub(crate) fn abort(&self) {
        self.control.queue.close_writer();
    }

    #[cfg(test)]
    pub(crate) fn was_aborted(&self) -> bool {
        self.transport().was_aborted()
    }

    pub(crate) fn is_complete(&self) -> bool {
        let state = self.control.queue.lock_state();
        !state.reader_running && !state.writer_running
    }

    #[cfg(all(test, windows))]
    pub(crate) async fn wait_complete(&self) {
        self.transport().wait_complete().await;
    }

    pub(crate) fn has_transport_notifications(&self) -> bool {
        self.control
            .queue
            .notification_pending
            .load(Ordering::Acquire)
    }

    pub(crate) fn take_transport_notifications(&self) -> (Option<bool>, bool) {
        if !self.has_transport_notifications() {
            return (None, false);
        }
        #[cfg(test)]
        TEST_NOTIFICATION_TAKES.with(|count| count.set(count.get() + 1));
        let mut state = self.control.queue.lock_state();
        let disconnected = if state.pending_events == 0 {
            state.disconnect_pending.take()
        } else {
            None
        };
        let drained = std::mem::take(&mut state.drained_pending);
        self.control.queue.publish_notifications(&state);
        (disconnected, drained)
    }

    pub(crate) fn acknowledge_transport_drain(&self) -> ClientWriterCredits {
        let mut state = self.control.queue.lock_state();
        state.drained_notified = false;
        std::mem::take(&mut state.credits)
    }

    pub(crate) fn has_endpoint_credit(&self) -> bool {
        self.control.queue.lock_state().credits.endpoint
    }

    pub(crate) fn defer_endpoint_credit(&self) {
        let mut state = self.control.queue.lock_state();
        if state.phase != ClientCloseState::Open || state.disconnect_notified {
            return;
        }
        state.credits.endpoint = true;
        if !state.drained_notified {
            state.drained_notified = true;
            state.drained_pending = true;
            self.control.queue.publish_notifications(&state);
        }
    }

    /// Drops render-lane work that has not yet been claimed by the writer.
    pub(crate) fn discard_pending_render(&self) {
        self.render.queue.discard_pending_render();
    }

    #[cfg(test)]
    pub(crate) fn test_accept_event(
        &self,
        event: ServerEvent,
        events: &mpsc::Sender<ServerEvent>,
    ) -> bool {
        send_client_event(
            &self.control.queue,
            events,
            event,
            &AtomicBool::new(false),
            Some(Instant::now() + Duration::from_secs(30)),
        )
        .is_ok()
    }

    #[cfg(test)]
    pub(crate) fn test_notify_disconnect(
        &self,
        client_id: u64,
        events: &mpsc::Sender<ServerEvent>,
    ) {
        self.control
            .queue
            .notify_disconnect(client_id, false, events);
    }

    #[cfg(test)]
    pub(crate) fn test_notify_drained(&self, client_id: u64, events: &mpsc::Sender<ServerEvent>) {
        self.control.queue.notify_drained(client_id, events);
    }

    #[cfg(test)]
    pub(crate) fn test_paused() -> Self {
        let queue = ClientWriterQueue::new();
        Self {
            control: ClientControlWriter::queue(queue.clone()),
            render: ClientRenderWriter::queue(queue),
        }
    }

    #[cfg(test)]
    pub(crate) fn test_endpoint_frames_sent(&self) -> usize {
        self.control
            .queue
            .endpoint_frames_sent
            .load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn test_control_backlog(&self) -> (usize, usize) {
        let state = self.control.queue.lock_state();
        (state.control_bytes, state.control.len())
    }

    #[cfg(test)]
    pub(crate) fn test_dequeue_control(
        &self,
        client_id: u64,
        events: &mpsc::Sender<ServerEvent>,
    ) -> Vec<u8> {
        assert!(!self.control.queue.lock_state().control.is_empty());
        let Some(ClientWriteItem::Control(data)) = self.control.queue.recv() else {
            panic!("paused writer must dequeue a control frame");
        };
        self.control.queue.notify_endpoint_credit(client_id, events);
        data
    }

    #[cfg(test)]
    pub(crate) fn test_drain(&self) -> Vec<Vec<u8>> {
        let mut state = self.render.queue.lock_state();
        let mut frames = state.control.drain(..).collect::<Vec<_>>();
        state.control_bytes = 0;
        frames.extend(state.ordered.drain(..));
        frames.extend(state.render.take());
        frames
    }

    #[cfg(test)]
    pub(crate) fn test_fill_render(&self, data: Vec<u8>) {
        self.render.try_send(data).unwrap();
    }

    #[cfg(test)]
    pub(crate) fn test_close(&self) {
        self.render.queue.close_writer();
    }

    #[cfg(test)]
    pub(crate) fn test_channel(
        control: std::sync::mpsc::Sender<Vec<u8>>,
        render: std::sync::mpsc::SyncSender<Vec<u8>>,
    ) -> Self {
        let queue = ClientWriterQueue::new();
        let drain = queue.clone();
        let control_writer = ClientControlWriter::queue(queue.clone());
        let mut render_writer = ClientRenderWriter::queue(queue);
        render_writer.test_render = Some(render.clone());
        let writer = Self {
            control: control_writer,
            render: render_writer,
        };
        std::thread::spawn(move || {
            while let Some(item) = drain.recv() {
                let sent = match item {
                    ClientWriteItem::Control(data) => control.send(data).is_ok(),
                    ClientWriteItem::Render(data) => render.send(data).is_ok(),
                };
                if !sent {
                    break;
                }
            }
            drain.close_writer();
        });
        writer
    }
}

#[derive(Debug)]
pub(crate) struct ClientControlWriter {
    queue: Arc<ClientWriterQueue>,
    #[cfg(test)]
    test_render: Option<std::sync::mpsc::SyncSender<Vec<u8>>>,
}

#[derive(Debug)]
pub(crate) struct ClientRenderWriter {
    queue: Arc<ClientWriterQueue>,
    #[cfg(test)]
    test_render: Option<std::sync::mpsc::SyncSender<Vec<u8>>>,
}

macro_rules! writer_handle {
    ($type:ty) => {
        impl Clone for $type {
            fn clone(&self) -> Self {
                self.queue.add_sender();
                Self {
                    queue: self.queue.clone(),
                    #[cfg(test)]
                    test_render: self.test_render.clone(),
                }
            }
        }
        impl Drop for $type {
            fn drop(&mut self) {
                self.queue.remove_sender();
            }
        }
    };
}
writer_handle!(ClientControlWriter);
writer_handle!(ClientRenderWriter);

impl ClientControlWriter {
    fn queue(queue: Arc<ClientWriterQueue>) -> Self {
        queue.add_sender();
        Self {
            queue,
            #[cfg(test)]
            test_render: None,
        }
    }

    pub(crate) fn send(&self, data: Vec<u8>) -> Result<(), SendError<Vec<u8>>> {
        self.queue.send_control(data)
    }

    pub(crate) fn try_send_endpoint_frame(
        &self,
        data: Vec<u8>,
    ) -> Result<(), TrySendError<Vec<u8>>> {
        let mut state = self.queue.lock_state();
        if state.phase != ClientCloseState::Open || state.disconnect_notified {
            return Err(TrySendError::Disconnected(data));
        }
        if state.control_bytes.saturating_add(data.len())
            > MAX_CLIENT_CONTROL_BACKLOG_BYTES - ENDPOINT_RESERVED_CONTROL_BYTES
            || state.control.len()
                >= MAX_CLIENT_CONTROL_BACKLOG_MESSAGES - ENDPOINT_RESERVED_CONTROL_MESSAGES
        {
            self.queue.endpoint_waiting.store(true, Ordering::Release);
            return Err(TrySendError::Full(data));
        }
        state.control_bytes += data.len();
        state.control.push_back(data);
        #[cfg(test)]
        self.queue
            .endpoint_frames_sent
            .fetch_add(1, Ordering::Release);
        self.queue.ready.notify_all();
        Ok(())
    }
}

impl ClientRenderWriter {
    fn queue(queue: Arc<ClientWriterQueue>) -> Self {
        queue.add_sender();
        Self {
            queue,
            #[cfg(test)]
            test_render: None,
        }
    }

    pub(crate) fn try_send(&self, data: Vec<u8>) -> Result<(), TrySendError<Vec<u8>>> {
        #[cfg(test)]
        if let Some(sender) = &self.test_render {
            return sender.try_send(data);
        }
        self.queue.try_send_render(data)
    }

    pub(crate) fn send_ordered(&self, data: Vec<u8>) -> Result<(), TrySendError<Vec<u8>>> {
        self.queue.send_ordered(data)
    }
}

#[derive(Debug)]
struct ClientWriterQueue {
    state: Mutex<ClientWriterQueueState>,
    ready: Condvar,
    completed: tokio::sync::Notify,
    notification_pending: AtomicBool,
    endpoint_waiting: AtomicBool,
    #[cfg(test)]
    endpoint_frames_sent: AtomicUsize,
    write_timeout: Duration,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ClientCloseState {
    #[default]
    Open,
    Sealed,
    Draining,
    Closed,
    Aborted,
}

#[derive(Debug, Default)]
struct ClientWriterQueueState {
    control: VecDeque<Vec<u8>>,
    control_bytes: usize,
    ordered: VecDeque<Vec<u8>>,
    render: Option<Vec<u8>>,
    senders: usize,
    phase: ClientCloseState,
    registration: Option<ClientTransportRegistration>,
    io: Option<Arc<crate::platform::ClientStreamControl>>,
    reader_running: bool,
    #[cfg(test)]
    reader_waiting: bool,
    #[cfg(test)]
    writer_started: bool,
    writer_running: bool,
    pending_events: usize,
    disconnect_pending: Option<bool>,
    disconnect_notified: bool,
    drained_pending: bool,
    drained_notified: bool,
    credits: ClientWriterCredits,
}

#[derive(Debug, PartialEq, Eq)]
enum ClientWriteItem {
    Control(Vec<u8>),
    Render(Vec<u8>),
}

impl ClientWriterQueue {
    fn publish_notifications(&self, state: &ClientWriterQueueState) -> bool {
        let ready = state.drained_pending
            || (state.disconnect_pending.is_some() && state.pending_events == 0);
        self.notification_pending.store(ready, Ordering::Release);
        ready
    }

    fn new() -> Arc<Self> {
        Self::with_timeout(CLIENT_WRITE_TIMEOUT)
    }

    fn with_timeout(write_timeout: Duration) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(ClientWriterQueueState::default()),
            ready: Condvar::new(),
            completed: tokio::sync::Notify::new(),
            notification_pending: AtomicBool::new(false),
            endpoint_waiting: AtomicBool::new(false),
            #[cfg(test)]
            endpoint_frames_sent: AtomicUsize::new(0),
            write_timeout,
        })
    }

    fn attach(&self, stream: &crate::platform::ServerClientStream, reader: bool) -> io::Result<()> {
        let io = crate::platform::client_stream_control(stream)?;
        let mut state = self.lock_state();
        if state.phase == ClientCloseState::Aborted {
            io.shutdown();
        }
        state.io = Some(io);
        state.reader_running = reader;
        state.writer_running = true;
        Ok(())
    }

    fn add_sender(&self) {
        let mut state = self.lock_state();
        state.senders = state.senders.saturating_add(1);
    }

    fn remove_sender(&self) {
        let mut state = self.lock_state();
        state.senders = state.senders.saturating_sub(1);
        self.ready.notify_all();
    }

    fn send_control(&self, data: Vec<u8>) -> Result<(), SendError<Vec<u8>>> {
        let mut state = self.lock_state();
        if state.phase != ClientCloseState::Open {
            return Err(SendError(data));
        }
        if state.control_bytes.saturating_add(data.len()) > MAX_CLIENT_CONTROL_BACKLOG_BYTES
            || state.control.len() >= MAX_CLIENT_CONTROL_BACKLOG_MESSAGES
        {
            self.abort_locked(&mut state);
            return Err(SendError(data));
        }
        state.control_bytes = state.control_bytes.saturating_add(data.len());
        state.control.push_back(data);
        self.ready.notify_all();
        Ok(())
    }

    fn try_send_render(&self, data: Vec<u8>) -> Result<(), TrySendError<Vec<u8>>> {
        let mut state = self.lock_state();
        if state.phase != ClientCloseState::Open {
            return Err(TrySendError::Disconnected(data));
        }
        if state.render.is_some() {
            return Err(TrySendError::Full(data));
        }
        state.render = Some(data);
        self.ready.notify_all();
        Ok(())
    }

    fn discard_pending_render(&self) {
        let mut state = self.lock_state();
        state.render = None;
        state.ordered.clear();
        self.ready.notify_all();
    }

    fn send_ordered(&self, data: Vec<u8>) -> Result<(), TrySendError<Vec<u8>>> {
        let mut state = self.lock_state();
        if state.phase != ClientCloseState::Open {
            return Err(TrySendError::Disconnected(data));
        }
        if !state.ordered.is_empty() {
            return Err(TrySendError::Full(data));
        }
        if let Some(older) = state.render.take() {
            state.ordered.push_back(older);
        }
        state.ordered.push_back(data);
        self.ready.notify_all();
        Ok(())
    }

    fn recv(&self) -> Option<ClientWriteItem> {
        let mut state = self.lock_state();
        loop {
            if matches!(
                state.phase,
                ClientCloseState::Closed | ClientCloseState::Aborted
            ) {
                return None;
            }
            if let Some(data) = state.control.pop_front() {
                state.control_bytes = state.control_bytes.saturating_sub(data.len());
                return Some(ClientWriteItem::Control(data));
            }
            if let Some(data) = state.ordered.pop_front() {
                self.ready.notify_all();
                return Some(ClientWriteItem::Render(data));
            }
            if let Some(data) = state.render.take() {
                return Some(ClientWriteItem::Render(data));
            }
            if state.senders == 0 || state.phase != ClientCloseState::Open {
                state.phase = ClientCloseState::Draining;
                return None;
            }
            state = self.ready.wait(state).unwrap_or_else(|e| e.into_inner());
        }
    }

    fn shutdown(&self, reason: &str, deadline: Option<Instant>) {
        let mut framed = Vec::new();
        let encoded = protocol::write_message(
            &mut framed,
            &ServerMessage::ServerShutdown {
                reason: Some(reason.to_owned()),
            },
        );
        let mut state = self.lock_state();
        if state.phase == ClientCloseState::Open {
            if encoded.is_err()
                || state.control_bytes.saturating_add(framed.len())
                    > MAX_CLIENT_CONTROL_BACKLOG_BYTES
                || state.control.len() >= MAX_CLIENT_CONTROL_BACKLOG_MESSAGES
            {
                self.abort_locked(&mut state);
                return;
            }
            state.control_bytes += framed.len();
            state.control.push_back(framed);
        }
        self.seal_locked(&mut state, deadline);
    }

    fn seal(&self, deadline: Option<Instant>) {
        self.seal_locked(&mut self.lock_state(), deadline);
    }

    fn seal_locked(&self, state: &mut ClientWriterQueueState, deadline: Option<Instant>) {
        if state.phase == ClientCloseState::Open {
            state.phase = ClientCloseState::Sealed;
            state.render = None;
            state.ordered.clear();
        }
        if let (Some(io), Some(deadline)) = (&state.io, deadline) {
            io.set_deadline(deadline);
        }
        self.ready.notify_all();
    }

    fn abort_locked(&self, state: &mut ClientWriterQueueState) {
        if state.phase == ClientCloseState::Closed {
            return;
        }
        state.phase = ClientCloseState::Aborted;
        state.control.clear();
        state.control_bytes = 0;
        state.render = None;
        state.ordered.clear();
        if let Some(io) = &state.io {
            io.shutdown();
        }
        self.ready.notify_all();
    }

    fn close_writer(&self) {
        self.abort_locked(&mut self.lock_state());
    }

    fn notify_disconnect(
        &self,
        client_id: u64,
        detached: bool,
        events: &mpsc::Sender<ServerEvent>,
    ) {
        let mut state = self.lock_state();
        if state.disconnect_notified {
            return;
        }
        state.disconnect_notified = true;
        let event = if detached {
            ServerEvent::ClientDetach { client_id }
        } else {
            ServerEvent::ClientDisconnected { client_id }
        };
        if events.try_send(event).is_err() {
            state.disconnect_pending = Some(detached);
            self.publish_notifications(&state);
        }
    }

    fn notify_drained(&self, client_id: u64, events: &mpsc::Sender<ServerEvent>) {
        self.notify_credit(client_id, events, true);
    }

    fn notify_endpoint_credit(&self, client_id: u64, events: &mpsc::Sender<ServerEvent>) {
        if self.endpoint_waiting.swap(false, Ordering::AcqRel) {
            self.notify_credit(client_id, events, false);
        }
    }

    fn notify_credit(&self, client_id: u64, events: &mpsc::Sender<ServerEvent>, render: bool) {
        let mut state = self.lock_state();
        if state.disconnect_notified {
            return;
        }
        state.credits.render |= render;
        state.credits.endpoint |= !render;
        if state.drained_notified {
            return;
        }
        state.drained_notified = true;
        if events
            .try_send(ServerEvent::ClientWriterDrained { client_id })
            .is_err()
        {
            state.drained_pending = true;
            self.publish_notifications(&state);
        }
    }

    fn worker_done(&self, reader: bool) {
        let mut state = self.lock_state();
        if reader {
            state.reader_running = false;
        } else {
            state.writer_running = false;
        }
        let registration = if !state.reader_running && !state.writer_running {
            state.io = None;
            state.registration.take()
        } else {
            None
        };
        drop(state);
        drop(registration);
        self.completed.notify_waiters();
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, ClientWriterQueueState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn send_client_event(
    queue: &ClientWriterQueue,
    events: &mpsc::Sender<ServerEvent>,
    mut event: ServerEvent,
    should_quit: &AtomicBool,
    deadline: Option<Instant>,
) -> Result<(), Box<ServerEvent>> {
    loop {
        let mut state = queue.lock_state();
        if should_quit.load(Ordering::Acquire)
            || deadline.is_some_and(|deadline| Instant::now() >= deadline)
            || state.phase != ClientCloseState::Open
            || state.disconnect_notified
        {
            return Err(Box::new(event));
        }
        // Acceptance and disconnect publication share this lock. A failed try_send adds no
        // credit, and a consumer cannot acknowledge a successful send before its credit exists.
        match events.try_send(event) {
            Ok(()) => {
                state.pending_events += 1;
                return Ok(());
            }
            Err(mpsc::error::TrySendError::Closed(event)) => return Err(Box::new(event)),
            Err(mpsc::error::TrySendError::Full(pending)) => event = pending,
        }
        let _ = queue.ready.wait_timeout(state, Duration::from_millis(10));
    }
}

/// Internal event sent from client transport threads to the main event loop.
#[derive(Debug)]
pub(crate) enum ServerEvent {
    ClientViewInput {
        client_id: u64,
        input: crate::protocol::views::ViewInput,
    },
    /// A new client completed the handshake.
    ClientConnected {
        client_id: u64,
        cols: u16,
        rows: u16,
        cell_width_px: u32,
        cell_height_px: u32,
        pixel_mouse: bool,
        writer: ClientWriter,
    },
    /// A client-owned shell completed its dedicated handshake.
    ClientShellConnected {
        client_id: u64,
        surface_cols: u16,
        surface_rows: u16,
        cell_width_px: u32,
        cell_height_px: u32,
        pixel_mouse: bool,
        direct_graphics: bool,
        endpoint_keybindings: bool,
        mouse_capture: bool,
        surface_active: bool,
        surface_reuse: bool,
        surface_delta: bool,
        /// 前台 client 宿主环境上报的 `SSH_AUTH_SOCK`（WEZ-INT-01 自愈链兜底源）。
        ssh_auth_sock: Option<String>,
        surface_scroll: bool,
        writer: ClientWriter,
    },
    /// A client sent an input message.
    ClientInput { client_id: u64, data: Vec<u8> },
    /// A client reported the one armed Kitty regular-file response.
    GraphicsTransmissionResult {
        client_id: u64,
        transfer_id: u64,
        image_id: u32,
        success: bool,
    },
    GraphicsTransmissionStarted {
        client_id: u64,
        transfer_id: u64,
        image_id: u32,
    },
    /// A fully decoded interactive paste exceeded the text-input limit.
    ClientPasteRejected {
        client_id: u64,
        size: usize,
        max: usize,
    },
    /// A client sent local clipboard image bytes to paste into a remote pane.
    ClientClipboardImage {
        client_id: u64,
        target: crate::protocol::ClientClipboardImageTarget,
        extension: String,
        data: Vec<u8>,
    },
    /// A client requested direct attach to one terminal.
    ClientAttachTerminal {
        client_id: u64,
        terminal_id: String,
        takeover: bool,
    },
    /// A client requested read-only observation of one terminal.
    ClientObserveTerminal { client_id: u64, target: String },
    /// A client requested writable control of one terminal.
    ClientControlTerminal {
        client_id: u64,
        target: String,
        takeover: bool,
    },
    /// A direct terminal attach client requested scrollback movement.
    ClientAttachScroll {
        client_id: u64,
        source: AttachScrollSource,
        direction: AttachScrollDirection,
        lines: u16,
        column: Option<u16>,
        row: Option<u16>,
        modifiers: u8,
    },
    /// A direct terminal attach client delivered one structured mouse event.
    ClientAttachMouse {
        client_id: u64,
        kind: crate::protocol::ClientMouseKind,
        position: crate::protocol::ClientMousePosition,
        geometry: Option<crate::protocol::ClientMouseGeometry>,
        modifiers: u8,
        lines: u16,
    },
    /// A client sent a resize message.
    ClientResize {
        client_id: u64,
        cols: u16,
        rows: u16,
        cell_width_px: u32,
        cell_height_px: u32,
        pixel_mouse: bool,
    },
    /// A client-owned shell recomputed its pane viewport.
    ClientShellResize {
        client_id: u64,
        surface_cols: u16,
        surface_rows: u16,
        cell_width_px: u32,
        cell_height_px: u32,
        pixel_mouse: bool,
    },
    /// A client-owned shell delivered semantic input to one stable pane target.
    ClientShellPaneInput {
        client_id: u64,
        pane_id: String,
        events: Vec<ClientPaneInputEvent>,
    },
    /// A client-owned shell delivered semantic input to its active popup terminal.
    ClientShellPopupInput {
        client_id: u64,
        terminal_id: String,
        events: Vec<ClientPaneInputEvent>,
    },
    /// A client-owned shell published one host terminal theme observation.
    ClientShellHostTheme {
        client_id: u64,
        update: crate::protocol::ClientHostThemeUpdate,
    },
    /// A client-owned shell reported whether its outer terminal has focus.
    ClientShellFocus { client_id: u64, focused: bool },
    /// A client-owned shell updated its local mouse-capture preference.
    ClientShellMouseCapture { client_id: u64, enabled: bool },
    /// The committed shell asks the server to replay presentation effects before input resumes.
    ClientShellPresentationSync { client_id: u64, token: String },
    /// A client-owned shell invoked one endpoint operation through this connection.
    ClientShellEndpointRequest {
        client_id: u64,
        boot_id: String,
        request: Box<crate::api::schema::Request>,
    },
    /// A well-framed endpoint request could not be dispatched by this server.
    ClientShellEndpointRequestError {
        client_id: u64,
        boot_id: String,
        request_id: String,
        code: &'static str,
        message: String,
    },
    EndpointResponseReady {
        response: super::client_commands::EndpointResponseReady,
    },
    /// 后台观测结果不参与终端命令的焦点和 in-flight 状态。
    ObservationResponse {
        client_id: u64,
        boot_id: String,
        message: ServerMessage,
    },
    /// A client detached gracefully.
    ClientDetach { client_id: u64 },
    /// A client connection was lost.
    ClientDisconnected { client_id: u64 },
    /// A client writer drained its render slot and can accept another render.
    ClientWriterDrained { client_id: u64 },
    /// Ctrl+C or external shutdown signal received.
    QuitSignal,
}

impl ServerEvent {
    pub(crate) fn transport_client_id(&self) -> Option<u64> {
        match self {
            Self::ClientConnected { client_id, .. }
            | Self::ClientShellConnected { client_id, .. }
            | Self::ClientViewInput { client_id, .. }
            | Self::ClientInput { client_id, .. }
            | Self::GraphicsTransmissionResult { client_id, .. }
            | Self::GraphicsTransmissionStarted { client_id, .. }
            | Self::ClientPasteRejected { client_id, .. }
            | Self::ClientClipboardImage { client_id, .. }
            | Self::ClientAttachTerminal { client_id, .. }
            | Self::ClientObserveTerminal { client_id, .. }
            | Self::ClientControlTerminal { client_id, .. }
            | Self::ClientAttachScroll { client_id, .. }
            | Self::ClientAttachMouse { client_id, .. }
            | Self::ClientResize { client_id, .. }
            | Self::ClientShellResize { client_id, .. }
            | Self::ClientShellPaneInput { client_id, .. }
            | Self::ClientShellPopupInput { client_id, .. }
            | Self::ClientShellHostTheme { client_id, .. }
            | Self::ClientShellFocus { client_id, .. }
            | Self::ClientShellMouseCapture { client_id, .. }
            | Self::ClientShellPresentationSync { client_id, .. }
            | Self::ClientShellEndpointRequest { client_id, .. }
            | Self::ClientShellEndpointRequestError { client_id, .. } => Some(*client_id),
            Self::EndpointResponseReady { .. }
            | Self::ObservationResponse { .. }
            | Self::ClientDetach { .. }
            | Self::ClientDisconnected { .. }
            | Self::ClientWriterDrained { .. }
            | Self::QuitSignal => None,
        }
    }
}

/// Clamp client-reported terminal dimensions to a minimum viable size.
pub(crate) fn clamp_terminal_size(cols: u16, rows: u16) -> (u16, u16) {
    let clamped_cols = cols.max(MIN_CLIENT_COLS);
    let clamped_rows = rows.max(MIN_CLIENT_ROWS);
    (clamped_cols, clamped_rows)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputEventLimit {
    WithinLimits,
    TooManyEvents,
    PasteTooLarge { size: usize },
    InputPayloadTooLarge { size: usize },
}

fn pane_input_event_limit(events: &[ClientPaneInputEvent]) -> InputEventLimit {
    let mut expanded_events = 0usize;
    let mut paste_bytes = 0usize;
    let mut input_bytes = 0usize;
    for event in events {
        expanded_events = expanded_events.saturating_add(match event {
            ClientPaneInputEvent::Key { repeat_count, .. } => usize::from((*repeat_count).max(1)),
            ClientPaneInputEvent::Mouse {
                kind:
                    crate::protocol::ClientMouseKind::ScrollUp
                    | crate::protocol::ClientMouseKind::ScrollDown,
                lines,
                ..
            } => usize::from((*lines).max(1)),
            ClientPaneInputEvent::TextCommit(_)
            | ClientPaneInputEvent::Mouse { .. }
            | ClientPaneInputEvent::Paste(_) => 1,
        });
        match event {
            ClientPaneInputEvent::Key {
                repeat_count,
                generated_text,
                ..
            } => {
                if let Some(text) = generated_text {
                    input_bytes = input_bytes.saturating_add(
                        text.len()
                            .saturating_mul(usize::from((*repeat_count).max(1))),
                    );
                }
            }
            ClientPaneInputEvent::TextCommit(text) => {
                input_bytes = input_bytes.saturating_add(text.len());
            }
            ClientPaneInputEvent::Mouse { .. } => {}
            ClientPaneInputEvent::Paste(text) => {
                paste_bytes = paste_bytes.saturating_add(text.len());
            }
        }
    }

    classify_input_event_size(expanded_events, paste_bytes, input_bytes)
}

fn classify_input_event_size(
    expanded_events: usize,
    paste_bytes: usize,
    input_bytes: usize,
) -> InputEventLimit {
    if expanded_events > MAX_INPUT_EVENT_BATCH {
        return InputEventLimit::TooManyEvents;
    }

    let payload_bytes = paste_bytes.saturating_add(input_bytes);
    if payload_bytes <= MAX_INPUT_PAYLOAD {
        InputEventLimit::WithinLimits
    } else if input_bytes == 0 {
        InputEventLimit::PasteTooLarge {
            size: payload_bytes,
        }
    } else {
        InputEventLimit::InputPayloadTooLarge {
            size: payload_bytes,
        }
    }
}

/// Reads one framed hello against one absolute deadline.
///
/// On Unix each partial read receives only the remaining timeout; on Windows
/// both readiness waits and overlapped reads use the same absolute deadline. Thus a client
/// cannot keep the handshake alive by sending an incomplete frame in pieces.
/// `read_message` separately rejects a declared first frame over `MAX_FRAME_SIZE`
/// (2 MiB), before deserializing it.
struct HandshakeReader<'a> {
    stream: &'a mut crate::platform::ServerClientStream,
    deadline: Instant,
}

impl<'a> HandshakeReader<'a> {
    fn new(stream: &'a mut crate::platform::ServerClientStream, deadline: Instant) -> Self {
        Self { stream, deadline }
    }
}

impl Read for HandshakeReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        crate::platform::read_client_handshake(self.stream, buffer, self.deadline)
    }
}

/// Handles the client handshake on a blocking thread.
///
/// Reads the `TerminalHello` or `ClientShellHello` message, validates the version,
/// sends `Welcome`, and then enters a read loop forwarding messages to the server event channel.
#[cfg(test)]
pub(crate) fn handle_client_handshake(
    stream: LocalStream,
    client_id: u64,
    server_event_tx: &mpsc::Sender<ServerEvent>,
    should_quit: &Arc<AtomicBool>,
) -> io::Result<()> {
    handle_client_handshake_with_permit(stream, client_id, server_event_tx, should_quit, None)
}

pub(crate) fn handle_client_handshake_with_permit(
    stream: LocalStream,
    client_id: u64,
    server_event_tx: &mpsc::Sender<ServerEvent>,
    should_quit: &Arc<AtomicBool>,
    handshake_permit: Option<ClientHandshakePermit>,
) -> io::Result<()> {
    let handshake_deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    let writer_queue = ClientWriterQueue::new();
    let mut stream =
        crate::platform::prepare_server_client_stream(stream, writer_queue.write_timeout)?;
    if should_quit.load(Ordering::Acquire) {
        return Ok(());
    }

    let hello: ClientMessage = match protocol::read_message(
        &mut HandshakeReader::new(&mut stream, handshake_deadline),
        MAX_FRAME_SIZE,
    ) {
        Ok(msg) => msg,
        Err(protocol::FramingError::UnexpectedEof) => {
            debug!(client_id, "client disconnected before handshake");
            return Ok(());
        }
        Err(protocol::FramingError::Oversized { claimed, max }) => {
            warn!(client_id, claimed, max, "oversized handshake from client");
            return Ok(());
        }
        Err(err) => {
            debug!(client_id, err = %err, "failed to read client hello");
            return Ok(());
        }
    };

    let (
        client_cols,
        client_rows,
        cell_width_px,
        cell_height_px,
        terminal_pixel_mouse,
        shell_options,
    ) = match hello {
        ClientMessage::TerminalHello {
            version,
            cols,
            rows,
            cell_width_px,
            cell_height_px,
            pixel_mouse,
        } => {
            if let protocol::VersionCheck::Incompatible(reason) =
                protocol::check_client_version(version)
            {
                let welcome = ServerMessage::Welcome {
                    version: PROTOCOL_VERSION,
                    encoding: RenderEncoding::TerminalAnsi,
                    error: Some(reason),
                };
                write_rejection(&mut stream, &welcome);
                return Ok(());
            }
            let (cols, rows) = clamp_terminal_size(cols, rows);
            (cols, rows, cell_width_px, cell_height_px, pixel_mouse, None)
        }
        ClientMessage::EndpointControl { kind, data } if kind == ENDPOINT_HELLO_KIND => {
            let hello: EndpointClientHello = match serde_json::from_str(&data) {
                Ok(hello) => hello,
                Err(error) => {
                    write_endpoint_rejection(
                        &mut stream,
                        "invalid_hello",
                        format!("invalid endpoint hello: {error}"),
                    );
                    return Ok(());
                }
            };
            let incompatibility = if hello.generation != ENDPOINT_PROTOCOL_GENERATION {
                Some((
                    "unsupported_generation",
                    format!(
                        "endpoint generation {} is unsupported; this server supports generation {ENDPOINT_PROTOCOL_GENERATION}",
                        hello.generation
                    ),
                ))
            } else if !hello.supports_required_codecs() {
                Some((
                    "no_common_core",
                    "client and server have no compatible endpoint core codecs".to_owned(),
                ))
            } else {
                client_shell_geometry_error(
                    hello.surface_size,
                    hello.cell_width_px,
                    hello.cell_height_px,
                )
                .map(|reason| ("invalid_surface", reason.to_owned()))
            };
            if let Some((code, reason)) = incompatibility {
                write_endpoint_rejection(&mut stream, code, reason);
                return Ok(());
            }
            (
                hello.surface_size.cols,
                hello.surface_size.rows,
                hello.cell_width_px,
                hello.cell_height_px,
                false,
                Some(ClientShellHelloOptions {
                    pixel_mouse: hello.pixel_mouse,
                    direct_graphics: hello.direct_graphics,
                    endpoint_keybindings: hello.endpoint_keybindings,
                    mouse_capture: hello.mouse_capture,
                    surface_active: hello.surface_active,
                    surface_reuse: hello.surface_reuse,
                    surface_delta: hello.surface_delta,
                    ssh_auth_sock: hello.ssh_auth_sock,
                    surface_scroll: hello.surface_scroll,
                }),
            )
        }
        ClientMessage::ClientShellHello { .. } => {
            let welcome = ServerMessage::Welcome {
                version: PROTOCOL_VERSION,
                encoding: RenderEncoding::SemanticFrame,
                error: Some(
                    "this client predates the stable endpoint protocol; upgrade the Herdr client"
                        .to_owned(),
                ),
            };
            write_rejection(&mut stream, &welcome);
            return Ok(());
        }
        _ => {
            debug!(client_id, "first message was not a handshake, closing");
            let welcome = ServerMessage::Welcome {
                version: PROTOCOL_VERSION,
                encoding: RenderEncoding::SemanticFrame,
                error: Some(
                    "expected TerminalHello or ClientShellHello as first message".to_owned(),
                ),
            };
            write_rejection(&mut stream, &welcome);
            return Ok(());
        }
    };

    if should_quit.load(Ordering::Acquire) {
        return Ok(());
    }

    // Send the negotiated welcome. Endpoint compatibility is independent from
    // the same-install protocol used by direct terminal clients.
    let render_encoding = if shell_options.is_some() {
        RenderEncoding::SemanticFrame
    } else {
        RenderEncoding::TerminalAnsi
    };
    let welcome = if shell_options.is_some() {
        let welcome = EndpointServerWelcome::compatible(
            crate::server::client_commands::supported_client_shell_method_names()
                .iter()
                .map(|method| (*method).to_owned())
                .collect(),
        );
        ServerMessage::EndpointControl {
            kind: ENDPOINT_WELCOME_KIND.into(),
            data: serde_json::to_string(&welcome).map_err(io::Error::other)?,
        }
    } else {
        ServerMessage::Welcome {
            version: PROTOCOL_VERSION,
            encoding: render_encoding,
            error: None,
        }
    };
    protocol::write_message(&mut stream, &welcome).map_err(|e| io::Error::other(e.to_string()))?;

    #[cfg(unix)]
    stream.set_recv_timeout(None)?;

    let writer = ClientWriter {
        control: ClientControlWriter::queue(writer_queue.clone()),
        render: ClientRenderWriter::queue(writer_queue.clone()),
    };
    let write_stream = stream.try_clone()?;
    writer_queue.attach(&stream, true)?;
    #[cfg(test)]
    if let Some(permit) = &handshake_permit {
        permit
            .limiter
            .test_checkpoint(&writer_queue, HandshakeTestStage::BeforeRegister);
    }
    if handshake_permit
        .as_ref()
        .is_some_and(|permit| !permit.limiter.register(&writer_queue))
    {
        writer_queue.close_writer();
        drop(write_stream);
        drop(stream);
        writer_queue.worker_done(false);
        writer_queue.worker_done(true);
        return Ok(());
    }
    #[cfg(test)]
    if let Some(permit) = &handshake_permit {
        permit
            .limiter
            .test_checkpoint(&writer_queue, HandshakeTestStage::BeforeSpawn);
    }
    let writer_event_tx = server_event_tx.clone();
    let worker_queue = writer_queue.clone();
    if let Err(error) = crate::thread_spawn::spawn_named("herdr-client-writer", move || {
        run_client_writer(write_stream, client_id, worker_queue, writer_event_tx);
    }) {
        writer_queue.close_writer();
        drop(stream);
        writer_queue.worker_done(false);
        writer_queue.worker_done(true);
        return Err(error);
    }
    #[cfg(test)]
    if let Some(permit) = &handshake_permit {
        permit
            .limiter
            .test_checkpoint(&writer_queue, HandshakeTestStage::AfterSpawn);
    }

    if should_quit.load(Ordering::Acquire) {
        send_shutdown_to_unregistered_client(&writer);
        drop(stream);
        writer_queue.worker_done(true);
        return Ok(());
    }

    // Notify the main loop about the new client.
    let endpoint_control_writer = shell_options.as_ref().map(|_| writer.control.clone());
    let connected = if let Some(shell_options) = shell_options {
        ServerEvent::ClientShellConnected {
            client_id,
            surface_cols: client_cols,
            surface_rows: client_rows,
            cell_width_px,
            cell_height_px,
            pixel_mouse: shell_options.pixel_mouse,
            direct_graphics: shell_options.direct_graphics,
            endpoint_keybindings: shell_options.endpoint_keybindings,
            mouse_capture: shell_options.mouse_capture,
            surface_active: shell_options.surface_active,
            surface_reuse: shell_options.surface_reuse,
            surface_delta: shell_options.surface_delta,
            ssh_auth_sock: shell_options.ssh_auth_sock,
            surface_scroll: shell_options.surface_scroll,
            writer,
        }
    } else {
        ServerEvent::ClientConnected {
            client_id,
            cols: client_cols,
            rows: client_rows,
            cell_width_px,
            cell_height_px,
            pixel_mouse: terminal_pixel_mouse,
            writer,
        }
    };
    if let Err(event) = send_client_event(
        &writer_queue,
        server_event_tx,
        connected,
        should_quit,
        Some(handshake_deadline),
    ) {
        if let ServerEvent::ClientConnected { writer, .. }
        | ServerEvent::ClientShellConnected { writer, .. } = *event
        {
            send_shutdown_to_unregistered_client(&writer);
        }
        drop(stream);
        writer_queue.worker_done(true);
        return Ok(());
    }
    drop(handshake_permit);

    client_read_loop_with_endpoint_controls(
        stream,
        client_id,
        server_event_tx,
        should_quit,
        endpoint_control_writer.as_ref(),
        &writer_queue,
    )
}

fn send_shutdown_to_unregistered_client(writer: &ClientWriter) {
    writer.transport().shutdown("server is shutting down", None);
}

#[cfg(test)]
fn client_writer_loop(
    stream: LocalStream,
    client_id: u64,
    queue: Arc<ClientWriterQueue>,
    events: mpsc::Sender<ServerEvent>,
) {
    #[cfg(windows)]
    let stream =
        crate::platform::prepare_server_client_stream(stream, queue.write_timeout).unwrap();
    queue.attach(&stream, false).unwrap();
    run_client_writer(stream, client_id, queue, events);
}

fn run_client_writer(
    mut stream: crate::platform::ServerClientStream,
    client_id: u64,
    queue: Arc<ClientWriterQueue>,
    events: mpsc::Sender<ServerEvent>,
) {
    #[cfg(test)]
    {
        queue.lock_state().writer_started = true;
        queue.ready.notify_all();
    }
    while let Some(item) = queue.recv() {
        let written = match item {
            ClientWriteItem::Control(data) => {
                queue.notify_endpoint_credit(client_id, &events);
                write_framed_bytes(&mut stream, &data)
            }
            ClientWriteItem::Render(data) => {
                queue.notify_drained(client_id, &events);
                write_framed_bytes(&mut stream, &data)
            }
        };
        if !written {
            queue.close_writer();
            break;
        }
    }
    if queue.lock_state().phase != ClientCloseState::Aborted {
        if let Err(err) = crate::platform::finish_client_stream(&mut stream) {
            debug!(client_id, %err, "client drain aborted");
            queue.close_writer();
        } else {
            let mut state = queue.lock_state();
            if state.phase != ClientCloseState::Aborted {
                state.phase = ClientCloseState::Closed;
            }
        }
    }
    queue.notify_disconnect(client_id, false, &events);
    drop(stream);
    queue.worker_done(false);
    debug!(client_id, "client writer thread exiting");
}

fn write_framed_bytes(stream: &mut crate::platform::ServerClientStream, data: &[u8]) -> bool {
    if let Err(err) = crate::platform::write_client_stream(stream, data) {
        debug!(err = %err, "client write failed, closing writer");
        return false;
    }
    true
}

/// The client read loop — reads messages from the client and forwards to the server event channel.
#[cfg(test)]
fn client_read_loop(
    stream: LocalStream,
    client_id: u64,
    server_event_tx: &mpsc::Sender<ServerEvent>,
    should_quit: &Arc<AtomicBool>,
) -> io::Result<()> {
    let stream = crate::platform::prepare_server_client_stream(stream, CLIENT_WRITE_TIMEOUT)?;
    let queue = ClientWriterQueue::new();
    queue.attach(&stream, true)?;
    queue.lock_state().writer_running = false;
    client_read_loop_with_endpoint_controls(
        stream,
        client_id,
        server_event_tx,
        should_quit,
        None,
        &queue,
    )
}

struct ClientEventSender<'a> {
    queue: &'a ClientWriterQueue,
    events: &'a mpsc::Sender<ServerEvent>,
    should_quit: &'a AtomicBool,
    detached: std::cell::Cell<bool>,
}

impl ClientEventSender<'_> {
    fn send(&self, event: ServerEvent) -> Result<(), Box<ServerEvent>> {
        match event {
            ServerEvent::ClientDisconnected { .. } => Ok(()),
            ServerEvent::ClientDetach { .. } => {
                self.detached.set(true);
                Ok(())
            }
            event => send_client_event(self.queue, self.events, event, self.should_quit, None),
        }
    }
}

fn client_read_loop_with_endpoint_controls(
    mut stream: crate::platform::ServerClientStream,
    client_id: u64,
    server_event_tx: &mpsc::Sender<ServerEvent>,
    should_quit: &Arc<AtomicBool>,
    endpoint_control_writer: Option<&ClientControlWriter>,
    queue: &ClientWriterQueue,
) -> io::Result<()> {
    let server_event_tx = ClientEventSender {
        queue,
        events: server_event_tx,
        should_quit,
        detached: std::cell::Cell::new(false),
    };
    while !should_quit.load(Ordering::Acquire) && queue.lock_state().phase == ClientCloseState::Open
    {
        #[cfg(test)]
        {
            queue.lock_state().reader_waiting = true;
            queue.ready.notify_all();
        }
        #[cfg(unix)]
        let message = protocol::read_message(
            &mut crate::platform::ClientStreamReader(&mut stream),
            MAX_GRAPHICS_FRAME_SIZE,
        );
        #[cfg(windows)]
        let message = protocol::read_message(&mut stream, MAX_GRAPHICS_FRAME_SIZE);
        let msg: ClientMessage = match message {
            Ok(msg) => msg,
            Err(protocol::FramingError::UnexpectedEof) => {
                // Client disconnected.
                let _ = server_event_tx.send(ServerEvent::ClientDisconnected { client_id });
                break;
            }
            Err(protocol::FramingError::Oversized { claimed, max }) => {
                warn!(
                    client_id,
                    claimed, max, "oversized message from client, closing"
                );
                let _ = server_event_tx.send(ServerEvent::ClientDisconnected { client_id });
                break;
            }
            Err(err) => {
                debug!(client_id, err = %err, "client read error, closing");
                let _ = server_event_tx.send(ServerEvent::ClientDisconnected { client_id });
                break;
            }
        };

        let event = match msg {
            ClientMessage::Input { data } => {
                // Validate input size.
                if data.len() > MAX_INPUT_PAYLOAD {
                    if crate::raw_input::is_complete_text_bracketed_paste(&data) {
                        warn!(
                            client_id,
                            size = data.len(),
                            max = MAX_INPUT_PAYLOAD,
                            "oversized bracketed paste from client, rejecting"
                        );
                        ServerEvent::ClientPasteRejected {
                            client_id,
                            size: data.len(),
                            max: MAX_INPUT_PAYLOAD,
                        }
                    } else {
                        warn!(
                            client_id,
                            size = data.len(),
                            "oversized input from client, closing"
                        );
                        let _ = server_event_tx.send(ServerEvent::ClientDisconnected { client_id });
                        break;
                    }
                } else {
                    ServerEvent::ClientInput { client_id, data }
                }
            }
            ClientMessage::ObserveTerminal { target } => {
                #[cfg(unix)]
                {
                    // macOS Unix sockets can block even with per-send MSG_DONTWAIT.
                    // ClientStreamReader preserves blocking reads on the shared socket.
                    let configured = stream
                        .set_send_timeout(Some(OBSERVER_WRITE_TIMEOUT))
                        .and_then(|()| stream.set_nonblocking(true));
                    if let Err(err) = configured {
                        queue.close_writer();
                        queue.notify_disconnect(client_id, false, server_event_tx.events);
                        drop(stream);
                        queue.worker_done(true);
                        return Err(err);
                    }
                }
                ServerEvent::ClientObserveTerminal { client_id, target }
            }
            ClientMessage::ControlTerminal { target, takeover } => {
                ServerEvent::ClientControlTerminal {
                    client_id,
                    target,
                    takeover,
                }
            }
            ClientMessage::GraphicsTransmissionResult {
                transfer_id,
                image_id,
                success,
            } => ServerEvent::GraphicsTransmissionResult {
                client_id,
                transfer_id,
                image_id,
                success,
            },
            ClientMessage::GraphicsTransmissionStarted {
                transfer_id,
                image_id,
            } => ServerEvent::GraphicsTransmissionStarted {
                client_id,
                transfer_id,
                image_id,
            },
            ClientMessage::ClipboardImage {
                target,
                extension,
                data,
            } => {
                if data.len() > MAX_CLIPBOARD_IMAGE_PAYLOAD {
                    warn!(
                        client_id,
                        size = data.len(),
                        "oversized clipboard image from client, closing"
                    );
                    let _ = server_event_tx.send(ServerEvent::ClientDisconnected { client_id });
                    break;
                } else {
                    ServerEvent::ClientClipboardImage {
                        client_id,
                        target,
                        extension,
                        data,
                    }
                }
            }
            ClientMessage::Resize {
                cols,
                rows,
                cell_width_px,
                cell_height_px,
                pixel_mouse,
            } => {
                let (clamped_cols, clamped_rows) = clamp_terminal_size(cols, rows);
                ServerEvent::ClientResize {
                    client_id,
                    cols: clamped_cols,
                    rows: clamped_rows,
                    cell_width_px,
                    cell_height_px,
                    pixel_mouse,
                }
            }
            ClientMessage::ClientShellResize {
                cell_width_px,
                cell_height_px,
                surface_size,
                pixel_mouse,
            } => {
                if let Some(reason) =
                    client_shell_geometry_error(surface_size, cell_width_px, cell_height_px)
                {
                    warn!(client_id, %reason, "invalid client shell resize, closing");
                    let _ = server_event_tx.send(ServerEvent::ClientDisconnected { client_id });
                    break;
                }
                ServerEvent::ClientShellResize {
                    client_id,
                    surface_cols: surface_size.cols,
                    surface_rows: surface_size.rows,
                    cell_width_px,
                    cell_height_px,
                    pixel_mouse,
                }
            }
            ClientMessage::ClientShellHostTheme { update } => {
                if matches!(
                    &update,
                    crate::protocol::ClientHostThemeUpdate::PaletteColors(colors)
                        if colors.len() > 256
                ) {
                    warn!(client_id, "invalid client shell host theme update, closing");
                    let _ = server_event_tx.send(ServerEvent::ClientDisconnected { client_id });
                    break;
                }
                ServerEvent::ClientShellHostTheme { client_id, update }
            }
            ClientMessage::ClientShellFocus { focused } => {
                ServerEvent::ClientShellFocus { client_id, focused }
            }
            ClientMessage::ClientShellMouseCapture { enabled } => {
                ServerEvent::ClientShellMouseCapture { client_id, enabled }
            }
            ClientMessage::ClientShellPaneInput { pane_id, events } => {
                match pane_input_event_limit(&events) {
                    InputEventLimit::WithinLimits => ServerEvent::ClientShellPaneInput {
                        client_id,
                        pane_id,
                        events,
                    },
                    InputEventLimit::TooManyEvents => {
                        warn!(
                            client_id,
                            count = events.len(),
                            "oversized targeted pane input batch, closing"
                        );
                        let _ = server_event_tx.send(ServerEvent::ClientDisconnected { client_id });
                        break;
                    }
                    InputEventLimit::PasteTooLarge { size } => {
                        warn!(
                            client_id,
                            size,
                            max = MAX_INPUT_PAYLOAD,
                            "oversized targeted pane paste, rejecting"
                        );
                        ServerEvent::ClientPasteRejected {
                            client_id,
                            size,
                            max: MAX_INPUT_PAYLOAD,
                        }
                    }
                    InputEventLimit::InputPayloadTooLarge { size } => {
                        warn!(
                            client_id,
                            size,
                            max = MAX_INPUT_PAYLOAD,
                            "oversized targeted pane input, closing"
                        );
                        let _ = server_event_tx.send(ServerEvent::ClientDisconnected { client_id });
                        break;
                    }
                }
            }
            ClientMessage::ClientShellPopupInput {
                terminal_id,
                events,
            } => match pane_input_event_limit(&events) {
                InputEventLimit::WithinLimits => ServerEvent::ClientShellPopupInput {
                    client_id,
                    terminal_id,
                    events,
                },
                InputEventLimit::TooManyEvents => {
                    warn!(
                        client_id,
                        count = events.len(),
                        "oversized popup input batch, closing"
                    );
                    let _ = server_event_tx.send(ServerEvent::ClientDisconnected { client_id });
                    break;
                }
                InputEventLimit::PasteTooLarge { size } => {
                    warn!(
                        client_id,
                        size,
                        max = MAX_INPUT_PAYLOAD,
                        "oversized popup paste, rejecting"
                    );
                    ServerEvent::ClientPasteRejected {
                        client_id,
                        size,
                        max: MAX_INPUT_PAYLOAD,
                    }
                }
                InputEventLimit::InputPayloadTooLarge { size } => {
                    warn!(
                        client_id,
                        size,
                        max = MAX_INPUT_PAYLOAD,
                        "oversized popup input, closing"
                    );
                    let _ = server_event_tx.send(ServerEvent::ClientDisconnected { client_id });
                    break;
                }
            },
            ClientMessage::ClientShellEndpointRequest { boot_id, request } => {
                if boot_id.len() > crate::server::client_commands::MAX_ENDPOINT_BOOT_ID_BYTES
                    || request.len() > crate::server::client_commands::MAX_ENDPOINT_COMMAND_BYTES
                {
                    warn!(
                        client_id,
                        boot_id_size = boot_id.len(),
                        request_size = request.len(),
                        "oversized client shell endpoint command, closing"
                    );
                    let _ = server_event_tx.send(ServerEvent::ClientDisconnected { client_id });
                    break;
                }
                let decoded = match decode_endpoint_request(&request) {
                    Ok(decoded) => decoded,
                    Err(error) => {
                        warn!(client_id, %error, "invalid endpoint request envelope, closing");
                        let _ = server_event_tx.send(ServerEvent::ClientDisconnected { client_id });
                        break;
                    }
                };
                let request_id = match &decoded {
                    DecodedEndpointRequest::Dispatch(request) => request.id.as_str(),
                    DecodedEndpointRequest::Error { request_id, .. } => request_id,
                };
                if request_id.len() > crate::server::client_commands::MAX_ENDPOINT_REQUEST_ID_BYTES
                {
                    warn!(
                        client_id,
                        "oversized client shell endpoint request id, closing"
                    );
                    let _ = server_event_tx.send(ServerEvent::ClientDisconnected { client_id });
                    break;
                }
                match decoded {
                    DecodedEndpointRequest::Dispatch(request) => {
                        ServerEvent::ClientShellEndpointRequest {
                            client_id,
                            boot_id,
                            request,
                        }
                    }
                    DecodedEndpointRequest::Error {
                        request_id,
                        code,
                        message,
                    } => ServerEvent::ClientShellEndpointRequestError {
                        client_id,
                        boot_id,
                        request_id,
                        code,
                        message,
                    },
                }
            }
            ClientMessage::EndpointControl { kind, data }
                if kind == crate::protocol::views::INPUT_KIND =>
            {
                let Ok(input) = serde_json::from_str::<crate::protocol::views::ViewInput>(&data)
                else {
                    continue;
                };
                if data.len() > MAX_INPUT_PAYLOAD
                    || !matches!(
                        pane_input_event_limit(&input.events),
                        InputEventLimit::WithinLimits
                    )
                {
                    continue;
                }
                ServerEvent::ClientViewInput { client_id, input }
            }
            ClientMessage::EndpointControl { kind, data }
                if kind == crate::protocol::endpoint::PRESENTATION_EFFECTS_SYNC_KIND =>
            {
                ServerEvent::ClientShellPresentationSync {
                    client_id,
                    token: data,
                }
            }
            ClientMessage::EndpointControl { kind, data } => {
                let Some(response) = crate::server::client_endpoint_control::response(&kind, data)
                else {
                    debug!(client_id, %kind, "ignoring unknown endpoint control message");
                    continue;
                };
                let Some(writer) = endpoint_control_writer else {
                    continue;
                };
                let mut framed = Vec::new();
                if protocol::write_message(&mut framed, &response).is_err()
                    || writer.send(framed).is_err()
                {
                    break;
                }
                continue;
            }
            ClientMessage::Detach => {
                let _ = server_event_tx.send(ServerEvent::ClientDetach { client_id });
                break;
            }
            ClientMessage::AttachTerminal {
                terminal_id,
                takeover,
            } => ServerEvent::ClientAttachTerminal {
                client_id,
                terminal_id,
                takeover,
            },
            ClientMessage::AttachScroll {
                source,
                direction,
                lines,
                column,
                row,
                modifiers,
            } => ServerEvent::ClientAttachScroll {
                client_id,
                source,
                direction,
                lines,
                column,
                row,
                modifiers,
            },
            ClientMessage::AttachMouse {
                kind,
                position,
                geometry,
                modifiers,
                lines,
            } => ServerEvent::ClientAttachMouse {
                client_id,
                kind,
                position,
                geometry,
                modifiers,
                lines,
            },
            ClientMessage::TerminalHello { .. } | ClientMessage::ClientShellHello { .. } => {
                // Duplicate handshake — ignore.
                continue;
            }
        };

        if server_event_tx.send(event).is_err() {
            break; // Main loop gone.
        }
    }

    let detached = server_event_tx.detached.get();
    {
        let mut state = queue.lock_state();
        if state.phase == ClientCloseState::Open
            && !detached
            && !should_quit.load(Ordering::Acquire)
        {
            queue.abort_locked(&mut state);
        }
    }
    queue.notify_disconnect(client_id, detached, server_event_tx.events);
    drop(stream);
    queue.worker_done(true);
    debug!(client_id, "client read thread exiting");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use interprocess::local_socket::traits::Listener as _;
    use std::path::PathBuf;

    #[test]
    fn transport_registry_close_and_register_are_atomic_in_both_orders() {
        for register_first in [false, true] {
            let limiter = ClientHandshakeLimiter::new();
            let writer = ClientWriter::test_paused();
            let queue = &writer.control.queue;
            queue.lock_state().writer_running = true;
            let senders = queue.lock_state().senders;
            let snapshot = if register_first {
                assert!(limiter.register(queue));
                limiter.close_transports()
            } else {
                let snapshot = limiter.close_transports();
                assert!(!limiter.register(queue));
                snapshot
            };
            assert_eq!(snapshot.len(), usize::from(register_first));
            assert_eq!(queue.lock_state().senders, senders);
            assert!(snapshot.iter().all(|transport| !transport.is_complete()));
            assert!(limiter.close_transports().is_empty());
            assert!(!limiter.register(&ClientWriterQueue::new()));
            queue.close_writer();
            queue.worker_done(false);
            assert!(snapshot.iter().all(ClientTransportHandle::is_complete));
        }
    }

    #[test]
    fn transport_registry_simultaneous_register_close_never_loses_an_owner() {
        for _ in 0..128 {
            let limiter = ClientHandshakeLimiter::new();
            let queue = ClientWriterQueue::new();
            queue.lock_state().writer_running = true;
            let barrier = std::sync::Barrier::new(2);
            let (registered, snapshot) = std::thread::scope(|scope| {
                let close = scope.spawn(|| {
                    barrier.wait();
                    limiter.close_transports()
                });
                barrier.wait();
                (limiter.register(&queue), close.join().unwrap())
            });
            assert_eq!(snapshot.len(), usize::from(registered));
            assert!(snapshot
                .iter()
                .all(|handle| handle.identity() == Arc::as_ptr(&queue) as usize));
            queue.close_writer();
            queue.worker_done(false);
        }
    }

    #[test]
    fn transport_registry_completion_and_queue_drop_remove_receipts_without_producers() {
        let limiter = ClientHandshakeLimiter::new();
        for reader_first in [false, true] {
            let writer = ClientWriter::test_paused();
            let queue = writer.control.queue.clone();
            {
                let mut state = queue.lock_state();
                state.reader_running = true;
                state.writer_running = true;
            }
            assert!(limiter.register(&queue));
            assert_eq!(queue.lock_state().senders, 2);
            drop(writer);
            assert_eq!(queue.lock_state().senders, 0);
            assert_eq!(
                queue.recv(),
                None,
                "registry cannot keep producer admission alive"
            );
            queue.worker_done(reader_first);
            assert_eq!(limiter.test_transport_count(), 1);
            queue.worker_done(!reader_first);
            assert_eq!(
                limiter.test_transport_count(),
                0,
                "a retained observer must not retain registry history"
            );
            assert!(queue.lock_state().registration.is_none());
        }
        for _ in 0..128 {
            let queue = ClientWriterQueue::new();
            assert!(limiter.register(&queue));
            assert_eq!(limiter.test_transport_count(), 1);
            drop(queue);
            assert_eq!(
                limiter.test_transport_count(),
                0,
                "queue destruction unregisters as a fallback"
            );
        }
    }

    #[test]
    fn transport_shutdown_tail_is_atomic_under_competing_owners() {
        for _ in 0..64 {
            let writer = ClientWriter::test_paused();
            writer.control.send(vec![1, 2, 3]).unwrap();
            writer.render.try_send(vec![4, 5]).unwrap();
            let handle = writer.transport();
            let barrier = std::sync::Barrier::new(2);
            std::thread::scope(|scope| {
                scope.spawn(|| {
                    barrier.wait();
                    send_shutdown_to_unregistered_client(&writer);
                });
                barrier.wait();
                handle.shutdown("server is shutting down", None);
            });
            let frames = writer.test_drain();
            assert_eq!(frames.len(), 2);
            assert_eq!(frames[0], vec![1, 2, 3]);
            let tail: ServerMessage =
                protocol::read_message(&mut frames[1].as_slice(), MAX_FRAME_SIZE).unwrap();
            assert!(matches!(tail, ServerMessage::ServerShutdown { .. }));
            assert_eq!(
                writer.control.queue.lock_state().phase,
                ClientCloseState::Sealed
            );
            assert!(writer.control.send(vec![9]).is_err());
        }
    }

    #[test]
    fn transport_shutdown_tail_never_revives_closed_phases_or_exceeds_control_limits() {
        for phase in [
            ClientCloseState::Sealed,
            ClientCloseState::Draining,
            ClientCloseState::Closed,
            ClientCloseState::Aborted,
        ] {
            let writer = ClientWriter::test_paused();
            writer.control.queue.lock_state().phase = phase;
            writer.transport().shutdown("late", None);
            writer.transport().shutdown("again", None);
            assert_eq!(writer.control.queue.lock_state().phase, phase);
            assert!(writer.test_drain().is_empty());
        }
        for bytes_limit in [false, true] {
            let writer = ClientWriter::test_paused();
            if bytes_limit {
                writer
                    .control
                    .send(vec![0; MAX_CLIENT_CONTROL_BACKLOG_BYTES])
                    .unwrap();
            } else {
                for _ in 0..MAX_CLIENT_CONTROL_BACKLOG_MESSAGES {
                    writer.control.send(vec![]).unwrap();
                }
            }
            writer.transport().shutdown("no capacity", None);
            assert!(writer.was_aborted());
            assert!(writer.test_drain().is_empty());
        }
    }

    struct TestSocketPath(PathBuf);

    impl Drop for TestSocketPath {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn unique_test_path(name: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let filename = format!(
            "h{}-{nanos}-{}.sock",
            std::process::id(),
            crate::config::test_dirs::unique_id()
        );
        #[cfg(unix)]
        {
            let _ = name;
            PathBuf::from("/tmp").join(filename)
        }
        #[cfg(windows)]
        {
            std::env::temp_dir().join(format!("herdr-{name}-{filename}"))
        }
    }

    fn local_stream_pair(name: &str) -> (LocalStream, LocalStream, TestSocketPath) {
        let path = unique_test_path(name);
        let _ = std::fs::remove_file(&path);
        let listener = crate::ipc::bind_local_listener(&path).unwrap();
        let client = crate::ipc::connect_local_stream(&path).unwrap();
        let server = listener.accept().unwrap();
        (client, server, TestSocketPath(path))
    }

    pub(crate) fn endpoint_hello(surface_cols: u16, surface_rows: u16) -> ClientMessage {
        let hello = EndpointClientHello {
            generation: ENDPOINT_PROTOCOL_GENERATION,
            cell_width_px: 8,
            cell_height_px: 16,
            surface_size: crate::protocol::ClientSurfaceSize {
                cols: surface_cols,
                rows: surface_rows,
            },
            pixel_mouse: true,
            direct_graphics: true,
            endpoint_keybindings: true,
            mouse_capture: true,
            surface_active: true,
            surface_reuse: false,
            surface_delta: false,
            surface_scroll: false,
            snapshot_codecs: vec![crate::protocol::endpoint::SNAPSHOT_CODEC_V1.into()],
            surface_codecs: vec![crate::protocol::endpoint::SURFACE_CODEC_V1.into()],
            input_codecs: vec![crate::protocol::endpoint::INPUT_CODEC_V1.into()],
            blob_codecs: vec![crate::protocol::endpoint::BLOB_CODEC_V1.into()],
            ssh_auth_sock: None,
        };
        ClientMessage::EndpointControl {
            kind: ENDPOINT_HELLO_KIND.into(),
            data: serde_json::to_string(&hello).unwrap(),
        }
    }

    fn endpoint_welcome(message: ServerMessage) -> EndpointServerWelcome {
        let ServerMessage::EndpointControl { kind, data } = message else {
            panic!("expected endpoint welcome");
        };
        assert_eq!(kind, ENDPOINT_WELCOME_KIND);
        serde_json::from_str(&data).unwrap()
    }

    const LOADED_WAIT: Duration = Duration::from_secs(30);

    #[test]
    fn client_writer_spawn_failure_releases_registered_transport_and_handshake_permit() {
        let _dirs = crate::config::test_dirs::isolate_dirs("client-writer-spawn-failure");
        let limiter = ClientHandshakeLimiter::new();
        let pause = limiter.test_pause_at(HandshakeTestStage::BeforeSpawn);
        let permit = limiter.try_acquire().expect("handshake slot");
        let (client, server, _path) = local_stream_pair("writer-spawn-failure");
        let (events, mut receiver) = mpsc::channel(4);
        let (done_tx, done) = std::sync::mpsc::sync_channel(1);
        let reader = std::thread::spawn(move || {
            crate::thread_spawn::test_hook::fail_next_spawns(1);
            let result = handle_client_handshake_with_permit(
                server,
                42,
                &events,
                &Arc::new(AtomicBool::new(false)),
                Some(permit),
            );
            done_tx.send(result).unwrap();
        });
        let mut client =
            crate::platform::prepare_server_client_stream(client, LOADED_WAIT).unwrap();
        protocol::write_message(&mut client, &endpoint_hello(80, 24)).unwrap();
        let welcome: ServerMessage = protocol::read_message(
            &mut HandshakeReader::new(&mut client, Instant::now() + LOADED_WAIT),
            MAX_FRAME_SIZE,
        )
        .unwrap();
        assert!(endpoint_welcome(welcome).error.is_none());
        let observer = pause.wait();
        assert_eq!(limiter.test_transport_count(), 1);
        assert!(!observer.writer_started());
        drop(pause);

        let err = done.recv_timeout(LOADED_WAIT).unwrap().unwrap_err();
        reader.join().unwrap();
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
        assert!(observer.is_complete());
        assert!(observer.was_aborted());
        assert!(!observer.writer_started());
        assert_eq!(limiter.test_transport_count(), 0);
        assert_eq!(limiter.active.load(Ordering::Acquire), 0);
        assert!(receiver.try_recv().is_err());
        assert!(limiter.try_acquire().is_some());
    }

    #[cfg(windows)]
    #[test]
    fn client_accept_spawn_failure_releases_bound_listener() {
        let path = TestSocketPath(unique_test_path("accept-spawn-failure"));
        let listener = crate::ipc::bind_local_listener(&path.0).unwrap();
        let limiter = ClientHandshakeLimiter::new();
        let (events, _receiver) = mpsc::channel(4);
        crate::thread_spawn::test_hook::fail_next_spawns(1);
        let err = crate::server::client_accept::spawn_windows_client_accept_thread(
            listener,
            Arc::new(AtomicBool::new(false)),
            events,
            limiter.clone(),
        )
        .expect_err("accept thread spawn fails");
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(limiter.active.load(Ordering::Acquire), 0);
        assert_eq!(limiter.test_transport_count(), 0);
        let _listener = crate::ipc::bind_local_listener(&path.0).expect("listener was released");
    }

    #[cfg(unix)]
    #[test]
    fn client_connection_spawn_failure_releases_permit_and_keeps_accepting() {
        let path = TestSocketPath(unique_test_path("connection-spawn-failure"));
        let listener = crate::ipc::bind_local_listener(&path.0).unwrap();
        listener
            .set_nonblocking(interprocess::local_socket::ListenerNonblockingMode::Accept)
            .unwrap();
        let limiter = ClientHandshakeLimiter::new();
        let (events, mut receiver) = mpsc::channel(4);
        let should_quit = Arc::new(AtomicBool::new(false));
        let mut next_client_id = 1;
        let failed_client = crate::ipc::connect_local_stream(&path.0).unwrap();
        crate::thread_spawn::test_hook::fail_next_spawns(1);
        crate::server::client_accept::accept_pending_client_connections(
            &listener,
            &mut next_client_id,
            &should_quit,
            &events,
            &limiter,
        )
        .unwrap();
        assert_eq!(next_client_id, 2);
        assert_eq!(limiter.active.load(Ordering::Acquire), 0);
        assert_eq!(limiter.test_transport_count(), 0);
        assert!(receiver.try_recv().is_err());
        drop(failed_client);

        let client = crate::ipc::connect_local_stream(&path.0).unwrap();
        let mut client =
            crate::platform::prepare_server_client_stream(client, LOADED_WAIT).unwrap();
        protocol::write_message(&mut client, &endpoint_hello(80, 24)).unwrap();
        crate::server::client_accept::accept_pending_client_connections(
            &listener,
            &mut next_client_id,
            &should_quit,
            &events,
            &limiter,
        )
        .unwrap();
        let welcome: ServerMessage = protocol::read_message(
            &mut HandshakeReader::new(&mut client, Instant::now() + LOADED_WAIT),
            MAX_FRAME_SIZE,
        )
        .unwrap();
        assert!(endpoint_welcome(welcome).error.is_none());
        let deadline = Instant::now() + LOADED_WAIT;
        let connected = loop {
            if let Ok(event) = receiver.try_recv() {
                break event;
            }
            assert!(
                Instant::now() < deadline,
                "second connection reaches the server"
            );
            std::thread::sleep(Duration::from_millis(1));
        };
        let ServerEvent::ClientShellConnected {
            client_id, writer, ..
        } = connected
        else {
            panic!("expected second client connection");
        };
        assert_eq!(client_id, 2);
        writer.test_close();
        drop(client);
        while !writer.is_complete() || limiter.active.load(Ordering::Acquire) != 0 {
            assert!(Instant::now() < deadline, "second transport finishes");
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(limiter.test_transport_count(), 0);
    }

    #[cfg(windows)]
    struct LiveTransport {
        peer: crate::platform::ServerClientStream,
        writer: ClientWriter,
        queue: Arc<ClientWriterQueue>,
        events: mpsc::Receiver<ServerEvent>,
        threads: Vec<std::thread::JoinHandle<()>>,
        _path: TestSocketPath,
    }

    #[cfg(windows)]
    impl LiveTransport {
        fn new(timeout: Duration, full_events: bool) -> Self {
            let (peer, stream, path) = local_stream_pair("bounded-transport");
            let peer = crate::platform::prepare_server_client_stream(peer, LOADED_WAIT).unwrap();
            let stream = crate::platform::prepare_server_client_stream(stream, timeout).unwrap();
            let write_stream = stream.try_clone().unwrap();
            let queue = ClientWriterQueue::with_timeout(timeout);
            queue.attach(&stream, true).unwrap();
            let writer = ClientWriter {
                control: ClientControlWriter::queue(queue.clone()),
                render: ClientRenderWriter::queue(queue.clone()),
            };
            let (events, receiver) = mpsc::channel(1);
            if full_events {
                events.try_send(ServerEvent::QuitSignal).unwrap();
            }
            let writer_events = events.clone();
            let writer_queue = queue.clone();
            let writer_thread = std::thread::spawn(move || {
                run_client_writer(write_stream, 96, writer_queue, writer_events);
            });
            let reader_queue = queue.clone();
            let controls = writer.control.clone();
            let reader_thread = std::thread::spawn(move || {
                client_read_loop_with_endpoint_controls(
                    stream,
                    96,
                    &events,
                    &Arc::new(AtomicBool::new(false)),
                    Some(&controls),
                    &reader_queue,
                )
                .unwrap();
            });
            Self {
                peer,
                writer,
                queue,
                events: receiver,
                threads: vec![writer_thread, reader_thread],
                _path: path,
            }
        }

        fn read_exact(&mut self, bytes: &mut [u8]) {
            HandshakeReader::new(&mut self.peer, Instant::now() + LOADED_WAIT)
                .read_exact(bytes)
                .unwrap_or_else(|error| {
                    panic!(
                        "read {} bytes: {error}; {:?}",
                        bytes.len(),
                        self.queue.lock_state()
                    )
                });
        }

        fn join(&mut self) {
            let deadline = Instant::now() + LOADED_WAIT;
            while !self.threads.iter().all(|thread| thread.is_finished()) {
                assert!(Instant::now() < deadline, "transport worker did not exit");
                std::thread::sleep(Duration::from_millis(2));
            }
            for thread in self.threads.drain(..) {
                thread.join().unwrap();
            }
            assert!(self.writer.is_complete());
        }
    }

    #[cfg(windows)]
    impl Drop for LiveTransport {
        fn drop(&mut self) {
            self.writer.abort();
            for thread in self.threads.drain(..) {
                thread.join().unwrap();
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_stalled_transport_aborts_both_workers_with_full_event_channel() {
        let _dirs = crate::config::test_dirs::isolate_dirs("stalled-transport");
        let mut connection = LiveTransport::new(Duration::from_millis(150), true);
        connection
            .writer
            .render
            .try_send(vec![b'x'; 3 * 1024 * 1024])
            .unwrap();
        connection.peer.write_all(&[2, 0]).unwrap();
        connection.join();
        assert_eq!(
            connection.queue.lock_state().phase,
            ClientCloseState::Aborted
        );
        assert_eq!(
            connection.writer.take_transport_notifications().0,
            Some(false)
        );
        assert!(matches!(
            connection.events.try_recv(),
            Ok(ServerEvent::QuitSignal)
        ));
    }

    #[cfg(windows)]
    #[test]
    fn windows_overflow_and_repeated_abort_release_both_workers() {
        let _dirs = crate::config::test_dirs::isolate_dirs("overflow-transport");
        let mut connection = LiveTransport::new(LOADED_WAIT, true);
        connection
            .writer
            .render
            .try_send(vec![0; 3 * 1024 * 1024])
            .unwrap();
        assert!(connection
            .writer
            .control
            .send(vec![0; MAX_CLIENT_CONTROL_BACKLOG_BYTES + 1])
            .is_err());
        connection.writer.abort();
        connection.writer.abort();
        connection.join();
        assert_eq!(
            connection.queue.lock_state().phase,
            ClientCloseState::Aborted
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_slow_progress_outlives_write_timeout_and_shutdown_tail_is_drained() {
        let _dirs = crate::config::test_dirs::isolate_dirs("slow-transport");
        let mut connection = LiveTransport::new(Duration::from_millis(250), true);
        let bytes = vec![b'x'; 3 * 1024 * 1024];
        connection.writer.control.send(bytes.clone()).unwrap();
        let start = Instant::now();
        let mut received = vec![0; bytes.len()];
        for chunk in received.chunks_mut(64 * 1024) {
            connection.read_exact(chunk);
            std::thread::sleep(Duration::from_millis(15));
        }
        assert_eq!(received, bytes);
        assert!(start.elapsed() > Duration::from_millis(500));
        assert_eq!(connection.queue.lock_state().phase, ClientCloseState::Open);
        let tail = frame_server_message(&ServerMessage::ServerShutdown {
            reason: Some("done".repeat(32)),
        });
        connection.writer.control.send(tail.clone()).unwrap();
        connection.writer.seal();
        assert!(connection.writer.control.send(vec![1]).is_err());
        std::thread::sleep(Duration::from_millis(60));
        let mut actual = vec![0; tail.len()];
        for chunk in actual.chunks_mut(2) {
            connection.read_exact(chunk);
            std::thread::sleep(Duration::from_millis(15));
        }
        assert_eq!(actual, tail);
        connection.join();
        assert_eq!(
            connection.queue.lock_state().phase,
            ClientCloseState::Closed
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_seal_keeps_inflight_frame_before_shutdown_tail() {
        let _dirs = crate::config::test_dirs::isolate_dirs("inflight-drain");
        let mut connection = LiveTransport::new(LOADED_WAIT, true);
        let bytes = vec![b'x'; 3 * 1024 * 1024];
        let mut received = vec![0; bytes.len()];
        connection.writer.render.try_send(bytes.clone()).unwrap();
        connection.read_exact(&mut received[..4096]);
        let tail = frame_server_message(&ServerMessage::ServerShutdown { reason: None });
        connection.writer.control.send(tail.clone()).unwrap();
        connection.writer.seal();
        connection.read_exact(&mut received[4096..]);
        assert_eq!(received, bytes);
        let mut received_tail = vec![0; tail.len()];
        connection.read_exact(&mut received_tail);
        assert_eq!(received_tail, tail);
        connection.join();
        assert_eq!(
            connection.queue.lock_state().phase,
            ClientCloseState::Closed
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_full_pipe_renews_timeout_for_single_byte_progress() {
        let _dirs = crate::config::test_dirs::isolate_dirs("byte-progress");
        let mut connection = LiveTransport::new(Duration::from_millis(200), true);
        connection
            .writer
            .render
            .try_send(vec![0; 3 * 1024 * 1024])
            .unwrap();
        let started = Instant::now();
        for _ in 0..30 {
            connection.read_exact(&mut [0]);
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(started.elapsed() > Duration::from_millis(500));
        assert_eq!(connection.queue.lock_state().phase, ClientCloseState::Open);
        connection.writer.abort();
        connection.join();
    }

    #[cfg(windows)]
    #[test]
    fn windows_blocking_interprocess_peer_receives_large_frame_and_tail() {
        let _dirs = crate::config::test_dirs::isolate_dirs("blocking-peer");
        let (mut peer, stream, _path) = local_stream_pair("blocking-peer");
        let (events, mut receiver) = mpsc::channel(4);
        let reader = std::thread::spawn(move || {
            handle_client_handshake(stream, 101, &events, &Arc::new(AtomicBool::new(false)))
                .unwrap();
        });
        protocol::write_message(&mut peer, &endpoint_hello(80, 24)).unwrap();
        let _: ServerMessage = protocol::read_message(&mut peer, MAX_FRAME_SIZE).unwrap();
        let ServerEvent::ClientShellConnected { writer, .. } =
            recv_server_event(&mut receiver, "blocking peer")
        else {
            panic!("missing connection")
        };
        let (done_tx, done) = std::sync::mpsc::channel();
        let peer_reader = std::thread::spawn(move || {
            let message: ServerMessage =
                protocol::read_message(&mut peer, MAX_GRAPHICS_FRAME_SIZE).unwrap();
            assert!(
                matches!(message, ServerMessage::WindowTitle { title: Some(title) } if title.len() == 3 * 1024 * 1024)
            );
            let message: ServerMessage = protocol::read_message(&mut peer, MAX_FRAME_SIZE).unwrap();
            assert!(matches!(message, ServerMessage::ServerShutdown { .. }));
            let _ = peer.read(&mut [0]);
            done_tx.send(()).unwrap();
        });
        writer
            .control
            .send(frame_server_message(&ServerMessage::WindowTitle {
                title: Some("x".repeat(3 * 1024 * 1024)),
            }))
            .unwrap();
        writer
            .control
            .send(frame_server_message(&ServerMessage::ServerShutdown {
                reason: None,
            }))
            .unwrap();
        writer.seal();
        let completed = done.recv_timeout(LOADED_WAIT).is_ok();
        if !completed {
            writer.abort();
        }
        peer_reader.join().unwrap();
        reader.join().unwrap();
        assert!(
            completed,
            "blocking peer must receive the entire frame and tail"
        );
        assert!(!writer.was_aborted());
    }

    #[cfg(windows)]
    #[test]
    fn windows_endpoint_final_chunk_does_not_close_the_connection() {
        let _dirs = crate::config::test_dirs::isolate_dirs("endpoint-final-chunk");
        let mut connection = LiveTransport::new(LOADED_WAIT, false);
        for request_id in ["first", "second"] {
            let message = ServerMessage::ClientShellEndpointResponseChunk {
                boot_id: "boot".into(),
                request_id: request_id.into(),
                final_chunk: true,
                data: b"{}".to_vec(),
            };
            let frame = frame_server_message(&message);
            connection.writer.control.send(frame.clone()).unwrap();
            let mut received = vec![0; frame.len()];
            connection.read_exact(&mut received);
            assert_eq!(received, frame);
            assert_eq!(connection.queue.lock_state().phase, ClientCloseState::Open);
        }
        connection.writer.seal();
        connection.join();
    }

    #[cfg(windows)]
    #[test]
    fn windows_full_input_event_channel_is_cancellable() {
        let _dirs = crate::config::test_dirs::isolate_dirs("full-input-events");
        let mut connection = LiveTransport::new(LOADED_WAIT, true);
        protocol::write_message(
            &mut connection.peer,
            &ClientMessage::Input { data: vec![1] },
        )
        .unwrap();
        connection.writer.abort();
        connection.join();
    }

    #[cfg(windows)]
    #[test]
    fn windows_rejection_survives_delayed_fragmented_reads() {
        let _dirs = crate::config::test_dirs::isolate_dirs("rejection-tail");
        let (peer, server, _path) = local_stream_pair("rejection-tail");
        let mut peer = crate::platform::prepare_server_client_stream(peer, LOADED_WAIT).unwrap();
        let (events, _receiver) = mpsc::channel(1);
        let thread = std::thread::spawn(move || {
            handle_client_handshake(server, 97, &events, &Arc::new(AtomicBool::new(false)))
                .unwrap();
        });
        protocol::write_message(&mut peer, &ClientMessage::Input { data: vec![] }).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        let mut reader = HandshakeReader::new(&mut peer, Instant::now() + LOADED_WAIT);
        let mut prefix = [0; 4];
        for byte in &mut prefix {
            reader.read_exact(std::slice::from_mut(byte)).unwrap();
            std::thread::sleep(Duration::from_millis(5));
        }
        let mut frame = prefix.to_vec();
        for _ in 0..u32::from_le_bytes(prefix) {
            let mut byte = [0];
            reader.read_exact(&mut byte).unwrap();
            frame.push(byte[0]);
        }
        let message: ServerMessage =
            protocol::read_message(&mut frame.as_slice(), MAX_FRAME_SIZE).unwrap();
        assert!(matches!(
            message,
            ServerMessage::Welcome { error: Some(_), .. }
        ));
        thread.join().unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_transport_handles_threads_and_idle_cpu_remain_bounded() {
        let _dirs = crate::config::test_dirs::isolate_dirs("transport-resources");
        let cycle = |index: usize| {
            let mut connection = LiveTransport::new(Duration::from_millis(100), true);
            if index.is_multiple_of(3) {
                connection.writer.control.send(vec![1, 2, 3, 4]).unwrap();
                connection.writer.seal_until(Instant::now() + LOADED_WAIT);
                connection.read_exact(&mut [0; 4]);
            } else if index % 3 == 1 {
                connection
                    .writer
                    .render
                    .try_send(vec![0; 2 * 1024 * 1024])
                    .unwrap();
                connection.writer.abort();
            } else {
                connection
                    .writer
                    .control
                    .send(vec![0; MAX_CLIENT_CONTROL_BACKLOG_BYTES + 1])
                    .unwrap_err();
            }
            connection.join();
        };
        for index in 0..10 {
            cycle(index);
        }
        let before = crate::platform::client_transport_process_sample();
        for index in 0..100 {
            cycle(index);
        }
        let after = crate::platform::client_transport_process_sample();
        assert!(
            after.0 <= before.0 + 2,
            "handles accumulated: {before:?} -> {after:?}"
        );
        assert!(
            after.1 <= before.1 + 1,
            "threads accumulated: {before:?} -> {after:?}"
        );
        let mut idle = (0..16)
            .map(|_| LiveTransport::new(LOADED_WAIT, false))
            .collect::<Vec<_>>();
        let cpu_before = crate::platform::client_transport_process_sample().2;
        std::thread::sleep(Duration::from_millis(400));
        let cpu = crate::platform::client_transport_process_sample().2 - cpu_before;
        assert!(
            cpu < Duration::from_millis(100),
            "idle transports used {cpu:?} CPU"
        );
        let started = Instant::now();
        protocol::write_message(&mut idle[0].peer, &ClientMessage::Input { data: vec![7] })
            .unwrap();
        assert!(matches!(
            recv_server_event(&mut idle[0].events, "idle wake"),
            ServerEvent::ClientInput { .. }
        ));
        let wake = started.elapsed();
        assert!(
            wake < Duration::from_secs(1),
            "event-driven reader wake is delayed"
        );
        println!("transport resources: before={before:?} after_100={after:?}; idle_16_cpu_400ms={cpu:?}; reader_wake={wake:?}");
    }

    #[cfg(windows)]
    #[test]
    fn windows_fragmented_handshake_does_not_extend_absolute_deadline() {
        let _dirs = crate::config::test_dirs::isolate_dirs("fragmented-handshake");
        let (peer, server, _path) = local_stream_pair("drip-handshake");
        let mut peer = crate::platform::prepare_server_client_stream(peer, LOADED_WAIT).unwrap();
        let (events, _receiver) = mpsc::channel(1);
        let (done_tx, done) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let result =
                handle_client_handshake(server, 98, &events, &Arc::new(AtomicBool::new(false)));
            done_tx.send(result).unwrap();
        });
        let started = Instant::now();
        let mut frame = Vec::new();
        protocol::write_message(&mut frame, &endpoint_hello(80, 24)).unwrap();
        for byte in &frame[..8] {
            if peer.write_all(std::slice::from_ref(byte)).is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        done.recv_timeout(Duration::from_secs(1))
            .expect("absolute handshake deadline")
            .unwrap();
        assert!(started.elapsed() < HANDSHAKE_TIMEOUT + Duration::from_secs(1));
        thread.join().unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_partial_input_and_second_client_work_during_stalled_output() {
        let _dirs = crate::config::test_dirs::isolate_dirs("parallel-clients");
        let stalled = LiveTransport::new(LOADED_WAIT, true);
        stalled
            .writer
            .render
            .try_send(vec![0; 3 * 1024 * 1024])
            .unwrap();
        let (peer, server, _path) = local_stream_pair("second-client");
        let mut peer = crate::platform::prepare_server_client_stream(peer, LOADED_WAIT).unwrap();
        let (events, mut receiver) = mpsc::channel(4);
        let thread = std::thread::spawn(move || {
            handle_client_handshake(server, 99, &events, &Arc::new(AtomicBool::new(false)))
                .unwrap();
        });
        let started = Instant::now();
        protocol::write_message(&mut peer, &endpoint_hello(80, 24)).unwrap();
        let _: ServerMessage = protocol::read_message(
            &mut HandshakeReader::new(&mut peer, Instant::now() + LOADED_WAIT),
            MAX_FRAME_SIZE,
        )
        .unwrap();
        let ServerEvent::ClientShellConnected { writer, .. } =
            recv_server_event(&mut receiver, "second connected")
        else {
            panic!("missing connection")
        };
        let mut input = Vec::new();
        protocol::write_message(&mut input, &ClientMessage::Input { data: vec![1, 2] }).unwrap();
        peer.write_all(&input[..2]).unwrap();
        std::thread::sleep(Duration::from_millis(30));
        peer.write_all(&input[2..]).unwrap();
        assert!(
            matches!(recv_server_event(&mut receiver, "partial input"), ServerEvent::ClientInput { data, .. } if data == [1, 2])
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        writer.abort();
        thread.join().unwrap();
    }

    #[test]
    fn sealed_queue_keeps_control_and_rejects_all_new_producers() {
        let (writer, queue) = test_queue_writer();
        writer.control.send(vec![1, 2]).unwrap();
        writer.render.try_send(vec![3]).unwrap();
        writer.seal();
        writer.seal();
        assert!(writer.control.send(vec![4]).is_err());
        assert!(writer.render.try_send(vec![5]).is_err());
        assert!(writer.render.send_ordered(vec![6]).is_err());
        assert_eq!(queue.recv(), Some(ClientWriteItem::Control(vec![1, 2])));
        assert_eq!(queue.recv(), None);
        assert_eq!(queue.lock_state().phase, ClientCloseState::Draining);
    }

    fn recv_server_event(receiver: &mut mpsc::Receiver<ServerEvent>, context: &str) -> ServerEvent {
        let deadline = std::time::Instant::now() + LOADED_WAIT;
        loop {
            match receiver.try_recv() {
                Ok(event) => return event,
                Err(mpsc::error::TryRecvError::Empty) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(err) => panic!("{context}: {err}"),
            }
        }
    }

    fn bracketed_paste_with_total_len(total_len: usize) -> Vec<u8> {
        const DELIMITER_BYTES: usize = b"\x1b[200~".len() + b"\x1b[201~".len();
        assert!(total_len >= DELIMITER_BYTES);
        let mut data = Vec::with_capacity(total_len);
        data.extend_from_slice(b"\x1b[200~");
        data.resize(total_len - b"\x1b[201~".len(), b'x');
        data.extend_from_slice(b"\x1b[201~");
        data
    }

    fn test_queue_writer() -> (ClientWriter, Arc<ClientWriterQueue>) {
        let queue = ClientWriterQueue::new();
        (
            ClientWriter {
                control: ClientControlWriter::queue(queue.clone()),
                render: ClientRenderWriter::queue(queue.clone()),
            },
            queue,
        )
    }

    #[test]
    fn response_fairness_credit_reasons_merge_without_duplicate_acknowledgement() {
        for full_channel in [false, true] {
            for render_first in [false, true] {
                let (writer, queue) = test_queue_writer();
                let (events, mut receiver) = mpsc::channel(1);
                if full_channel {
                    events.try_send(ServerEvent::QuitSignal).unwrap();
                }
                let chunk = vec![0; super::super::client_commands::ENDPOINT_RESPONSE_CHUNK_BYTES];
                while writer
                    .control
                    .try_send_endpoint_frame(chunk.clone())
                    .is_ok()
                {}
                assert!(!writer.was_aborted());
                assert!(queue.endpoint_waiting.load(Ordering::Acquire));
                if render_first {
                    queue.notify_drained(1, &events);
                }
                assert_eq!(writer.test_dequeue_control(1, &events), chunk);
                queue.notify_drained(1, &events);
                queue.notify_drained(1, &events);
                queue.notify_endpoint_credit(1, &events);
                assert_eq!(receiver.len(), 1);
                assert_eq!(writer.take_transport_notifications(), (None, full_channel));
                assert_eq!(writer.take_transport_notifications(), (None, false));
                let token = receiver.try_recv().unwrap();
                if full_channel {
                    assert!(matches!(token, ServerEvent::QuitSignal));
                } else {
                    assert!(matches!(
                        token,
                        ServerEvent::ClientWriterDrained { client_id: 1 }
                    ));
                }
                let credits = writer.acknowledge_transport_drain();
                assert!(credits.render && credits.endpoint);
                let duplicate = writer.acknowledge_transport_drain();
                assert!(!duplicate.render && !duplicate.endpoint);
                queue.notify_endpoint_credit(1, &events);
                assert!(receiver.is_empty());
                assert_eq!(writer.take_transport_notifications(), (None, false));
                println!("response-fairness credit_merge full_channel={full_channel} render_first={render_first} render={} endpoint={} duplicate_render={} duplicate_endpoint={}", credits.render, credits.endpoint, duplicate.render, duplicate.endpoint);
            }
        }
    }

    fn frame_server_message(message: &ServerMessage) -> Vec<u8> {
        let mut bytes = Vec::new();
        protocol::write_message(&mut bytes, message).expect("frame server message");
        bytes
    }

    #[cfg(windows)]
    fn assert_handshake_deadline_closes_client(name: &str, partial_hello: bool) {
        let (mut client_stream, server_stream, _path) = local_stream_pair(name);
        let (server_event_tx, _server_event_rx) = mpsc::channel(4);
        let should_quit = Arc::new(AtomicBool::new(false));
        let handshake_quit = should_quit.clone();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            let result =
                handle_client_handshake(server_stream, 90, &server_event_tx, &handshake_quit);
            let _ = done_tx.send(result);
        });

        if partial_hello {
            client_stream
                .write_all(&[0x01])
                .expect("write partial hello prefix");
        }

        let started = Instant::now();
        let result = done_rx
            .recv_timeout(HANDSHAKE_TIMEOUT + Duration::from_secs(1))
            .expect("handshake thread must stop at its absolute deadline");
        assert!(
            result.is_ok(),
            "deadline should close the handshake cleanly"
        );
        assert!(
            started.elapsed() < HANDSHAKE_TIMEOUT + Duration::from_secs(1),
            "handshake exceeded its deadline: {:?}",
            started.elapsed()
        );

        let close_deadline = Instant::now() + Duration::from_secs(1);
        while !crate::ipc::local_stream_peer_closed(&mut client_stream)
            .expect("probe named-pipe closure")
        {
            assert!(
                Instant::now() < close_deadline,
                "server must release the pipe after the handshake deadline"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        handle.join().expect("handshake thread join");
    }

    #[cfg(windows)]
    #[test]
    fn empty_windows_handshake_closes_at_absolute_deadline() {
        assert_handshake_deadline_closes_client("empty-handshake-deadline", false);
    }

    #[cfg(windows)]
    #[test]
    fn partial_windows_handshake_closes_at_absolute_deadline() {
        assert_handshake_deadline_closes_client("partial-handshake-deadline", true);
    }

    #[cfg(windows)]
    #[test]
    fn client_handshake_limiter_bounds_idle_connections_and_allows_second_client() {
        let limiter = ClientHandshakeLimiter::with_limit(2);
        let (idle_client_one, idle_server_one, _path_one) =
            local_stream_pair("handshake-limit-idle-one");
        let (idle_client_two, idle_server_two, _path_two) =
            local_stream_pair("handshake-limit-idle-two");
        let (server_event_tx, _server_event_rx) = mpsc::channel(8);
        let should_quit = Arc::new(AtomicBool::new(false));
        let mut idle_handles = Vec::new();
        for (client_id, server_stream) in [(91, idle_server_one), (92, idle_server_two)] {
            let permit = limiter.try_acquire().expect("idle handshake slot");
            let event_tx = server_event_tx.clone();
            let handshake_quit = should_quit.clone();
            idle_handles.push(std::thread::spawn(move || {
                handle_client_handshake_with_permit(
                    server_stream,
                    client_id,
                    &event_tx,
                    &handshake_quit,
                    Some(permit),
                )
            }));
        }

        let deadline = Instant::now() + Duration::from_secs(1);
        while limiter.active() != 2 {
            assert!(
                Instant::now() < deadline,
                "idle handshakes did not occupy both slots"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(
            limiter.try_acquire().is_none(),
            "a third idle connection must not create another handshake worker"
        );

        drop(idle_client_one);
        let deadline = Instant::now() + Duration::from_secs(1);
        while limiter.active() != 1 {
            assert!(
                Instant::now() < deadline,
                "closed idle pipe did not release its slot"
            );
            std::thread::sleep(Duration::from_millis(2));
        }

        let (mut normal_client, normal_server, _normal_path) =
            local_stream_pair("handshake-limit-normal");
        let permit = limiter.try_acquire().expect("second normal client slot");
        let event_tx = server_event_tx.clone();
        let handshake_quit = should_quit.clone();
        let normal_handle = std::thread::spawn(move || {
            handle_client_handshake_with_permit(
                normal_server,
                93,
                &event_tx,
                &handshake_quit,
                Some(permit),
            )
        });
        protocol::write_message(
            &mut normal_client,
            &ClientMessage::TerminalHello {
                version: PROTOCOL_VERSION,
                cols: 80,
                rows: 24,
                cell_width_px: 8,
                cell_height_px: 16,
                pixel_mouse: false,
            },
        )
        .expect("write normal hello");
        assert!(matches!(
            protocol::read_message::<_, ServerMessage>(&mut normal_client, MAX_FRAME_SIZE)
                .expect("read normal welcome"),
            ServerMessage::Welcome { error: None, .. }
        ));

        drop(normal_client);
        drop(idle_client_two);
        for handle in idle_handles {
            handle.join().expect("idle handshake thread join").unwrap();
        }
        normal_handle
            .join()
            .expect("normal handshake thread join")
            .unwrap();
        assert_eq!(limiter.active(), 0, "all handshake slots must be released");
    }

    #[cfg(windows)]
    #[test]
    fn windows_close_writer_cancels_established_reader() {
        let _dirs = crate::config::test_dirs::isolate_dirs("transport-close-reader");
        let (mut client, server, _path) = local_stream_pair("close-reader");
        let (events, mut receiver) = mpsc::channel(4);
        let (done_tx, done) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            let result =
                handle_client_handshake(server, 95, &events, &Arc::new(AtomicBool::new(false)));
            let _ = done_tx.send(result);
        });
        protocol::write_message(&mut client, &endpoint_hello(80, 24)).unwrap();
        let _: ServerMessage = protocol::read_message(&mut client, MAX_FRAME_SIZE).unwrap();
        let ServerEvent::ClientShellConnected { writer, .. } = receiver.blocking_recv().unwrap()
        else {
            panic!("expected shell connection");
        };
        writer.test_close();
        writer.test_close();
        let closed = done.recv_timeout(Duration::from_secs(2)).is_ok();
        drop(client);
        reader.join().unwrap();
        assert!(closed, "closing the writer must release the paired reader");
    }

    #[test]
    fn client_close_discards_accepted_control_on_abort() {
        let (writer, queue) = test_queue_writer();
        writer.control.send(vec![1]).unwrap();
        writer.test_close();
        assert!(
            queue.recv().is_none(),
            "aborted control must not be replayed"
        );
    }

    /// HSR-06/RS-16：control 车道字节或条数越界说明客户端已经不读——丢队列、
    /// 让 writer 收尾，调用方据此断开，而不是无界增长。
    #[test]
    fn client_control_backlog_is_bounded_and_wedges_the_client() {
        let (writer, queue) = test_queue_writer();
        let chunk = vec![b'x'; 64 * 1024];
        let fits = MAX_CLIENT_CONTROL_BACKLOG_BYTES / chunk.len();
        for _ in 0..fits {
            writer.control.send(chunk.clone()).expect("backlog fits");
        }
        assert!(matches!(writer.control.send(chunk), Err(SendError(_))));
        assert!(queue.recv().is_none(), "backlog is dropped on overflow");
        assert!(matches!(writer.control.send(vec![b'y']), Err(SendError(_))));
        assert!(matches!(
            writer.render.try_send(vec![b'z']),
            Err(TrySendError::Disconnected(_))
        ));

        let (writer, _queue) = test_queue_writer();
        for _ in 0..MAX_CLIENT_CONTROL_BACKLOG_MESSAGES {
            writer.control.send(vec![1]).expect("message fits");
        }
        assert!(matches!(writer.control.send(vec![1]), Err(SendError(_))));
    }

    #[cfg(unix)]
    #[test]
    fn client_shell_handshake_arms_a_send_timeout() {
        let (mut client_stream, server_stream, _path) =
            local_stream_pair("client-shell-write-timeout");
        let server_probe = server_stream.try_clone().expect("clone server stream");
        let (server_event_tx, mut server_event_rx) = mpsc::channel(4);
        let should_quit = Arc::new(AtomicBool::new(false));
        let handshake_quit = should_quit.clone();
        let handle = std::thread::spawn(move || {
            handle_client_handshake(server_stream, 45, &server_event_tx, &handshake_quit)
        });

        protocol::write_message(&mut client_stream, &endpoint_hello(80, 24))
            .expect("write shell hello");
        let _: ServerMessage =
            protocol::read_message(&mut client_stream, MAX_FRAME_SIZE).expect("read welcome");
        let _ = server_event_rx
            .blocking_recv()
            .expect("client shell connected event");

        let LocalStream::UdSocket(socket) = &server_probe;
        assert_eq!(
            socket.inner().write_timeout().unwrap(),
            Some(CLIENT_WRITE_TIMEOUT),
            "HSR-06: a stalled client must hit the send timeout"
        );

        drop(client_stream);
        should_quit.store(true, Ordering::Release);
        handle
            .join()
            .expect("handshake thread join")
            .expect("handshake thread result");
    }

    #[test]
    fn client_writer_queue_keeps_render_slot_bounded() {
        let (writer, _queue) = test_queue_writer();
        let first = frame_server_message(&ServerMessage::WindowTitle {
            title: Some("first".into()),
        });
        let second = frame_server_message(&ServerMessage::WindowTitle {
            title: Some("second".into()),
        });

        writer.render.try_send(first).expect("first render fits");
        assert!(matches!(
            writer.render.try_send(second),
            Err(TrySendError::Full(_))
        ));
    }

    #[test]
    fn ordered_direct_follows_older_render_and_stays_bounded() {
        let (writer, queue) = test_queue_writer();
        writer.render.try_send(b"old".to_vec()).unwrap();
        writer.render.send_ordered(b"direct".to_vec()).unwrap();
        assert!(matches!(
            writer.render.send_ordered(b"second".to_vec()),
            Err(TrySendError::Full(_))
        ));
        writer.render.try_send(b"new".to_vec()).unwrap();

        for expected in [b"old".as_slice(), b"direct", b"new"] {
            assert_eq!(
                queue.recv(),
                Some(ClientWriteItem::Render(expected.to_vec()))
            );
        }
        queue.close_writer();
        assert!(matches!(
            writer.render.send_ordered(b"closed".to_vec()),
            Err(TrySendError::Disconnected(_))
        ));
    }

    #[test]
    fn client_writer_prioritizes_control_and_reports_render_drain() {
        let (mut client_stream, server_stream, _path) = local_stream_pair("client-writer-priority");
        let (writer, queue) = test_queue_writer();
        writer
            .render
            .try_send(frame_server_message(&ServerMessage::WindowTitle {
                title: Some("render".into()),
            }))
            .expect("queue render");
        writer
            .control
            .send(frame_server_message(&ServerMessage::ReloadSoundConfig))
            .expect("queue control");

        let (server_event_tx, mut server_event_rx) = mpsc::channel(4);
        let handle = std::thread::spawn(move || {
            client_writer_loop(server_stream, 9, queue, server_event_tx);
        });

        match protocol::read_message(&mut client_stream, MAX_FRAME_SIZE).expect("read control") {
            ServerMessage::ReloadSoundConfig => {}
            other => panic!("expected control message first, got {other:?}"),
        }
        match protocol::read_message(&mut client_stream, MAX_FRAME_SIZE).expect("read render") {
            ServerMessage::WindowTitle { title } => assert_eq!(title.as_deref(), Some("render")),
            other => panic!("expected render message second, got {other:?}"),
        }
        match server_event_rx
            .blocking_recv()
            .expect("writer drained render slot")
        {
            ServerEvent::ClientWriterDrained { client_id } => assert_eq!(client_id, 9),
            other => panic!("expected writer drained event, got {other:?}"),
        }

        drop(writer);
        handle.join().expect("writer exits after senders drop");
    }

    #[test]
    fn client_writer_exits_when_all_writer_handles_drop() {
        let (_client_stream, server_stream, _path) = local_stream_pair("client-writer-drop");
        let (writer, queue) = test_queue_writer();
        let (server_event_tx, _server_event_rx) = mpsc::channel(4);
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            client_writer_loop(server_stream, 11, queue, server_event_tx);
            let _ = done_tx.send(());
        });

        drop(writer);
        done_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("writer exits without polling after senders drop");
    }

    #[test]
    fn client_writer_clone_keeps_loop_alive_until_final_drop() {
        let (mut client_stream, server_stream, _path) =
            local_stream_pair("client-writer-clone-drop");
        let (writer, queue) = test_queue_writer();
        let cloned_writer = writer.clone();
        let (server_event_tx, _server_event_rx) = mpsc::channel(4);
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            client_writer_loop(server_stream, 12, queue, server_event_tx);
            let _ = done_tx.send(());
        });

        drop(writer);
        cloned_writer
            .control
            .send(frame_server_message(&ServerMessage::ReloadSoundConfig))
            .expect("cloned writer still sends after original drops");
        match protocol::read_message(&mut client_stream, MAX_FRAME_SIZE)
            .expect("read control from cloned writer")
        {
            ServerMessage::ReloadSoundConfig => {}
            other => panic!("expected cloned control message, got {other:?}"),
        }
        assert!(
            done_rx.recv_timeout(Duration::from_millis(100)).is_err(),
            "writer exited while cloned handles were still alive"
        );

        drop(cloned_writer);
        done_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("writer exits after final cloned writer drops");
    }

    #[test]
    fn client_writer_closes_queue_after_socket_write_failure() {
        let (client_stream, server_stream, _path) =
            local_stream_pair("client-writer-socket-failure");
        #[cfg(not(windows))]
        server_stream
            .set_send_timeout(Some(Duration::from_millis(100)))
            .expect("set test send timeout");
        let (writer, queue) = test_queue_writer();
        let (server_event_tx, _server_event_rx) = mpsc::channel(4);
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            client_writer_loop(server_stream, 13, queue, server_event_tx);
            let _ = done_tx.send(());
        });

        drop(client_stream);
        writer
            .control
            .send(vec![b'x'; 1024 * 1024])
            .expect("message is accepted before the writer observes socket failure");
        done_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("writer exits after socket write failure");

        assert!(matches!(writer.control.send(vec![b'y']), Err(SendError(_))));
        assert!(matches!(
            writer.render.try_send(vec![b'z']),
            Err(TrySendError::Disconnected(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn observer_write_timeout_resets_when_sending_makes_progress() {
        use std::io::Read as _;

        let (mut client, mut server, _path) = local_stream_pair("slow-observer");
        server
            .set_send_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        server.set_nonblocking(true).unwrap();
        let worker = std::thread::spawn(move || {
            assert!(write_framed_bytes(&mut server, &vec![b'x'; 1024 * 1024]));
        });
        client
            .set_recv_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut received = 0;
        let mut buffer = [0; 16 * 1024];
        while received < 1024 * 1024 {
            let count = client.read(&mut buffer).unwrap();
            assert_ne!(count, 0, "observer disconnected while making progress");
            received += count;
            std::thread::sleep(Duration::from_millis(5));
        }
        worker.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn stalled_observer_timeout_releases_writer_and_reader() {
        let (mut client, server, _path) = local_stream_pair("stalled-observer");
        let writer_stream = server.try_clone().expect("clone writer stream");
        let (writer, queue) = test_queue_writer();
        let (events, event_rx) = mpsc::channel(8);
        let reader_events = events.clone();
        let (reader_done_tx, reader_done) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            let result = client_read_loop(
                server,
                14,
                &reader_events,
                &Arc::new(AtomicBool::new(false)),
            );
            let _ = reader_done_tx.send(result);
        });
        protocol::write_message(
            &mut client,
            &ClientMessage::ObserveTerminal {
                target: "w1:p1".into(),
            },
        )
        .expect("observe request");
        let mut event_rx = event_rx;
        assert!(matches!(
            event_rx.blocking_recv(),
            Some(ServerEvent::ClientObserveTerminal { client_id: 14, .. })
        ));
        let LocalStream::UdSocket(socket) = &writer_stream;
        assert_eq!(
            socket.inner().write_timeout().unwrap(),
            Some(Duration::from_secs(30))
        );
        assert_eq!(socket.inner().read_timeout().unwrap(), None);
        let mut resize = Vec::new();
        protocol::write_message(
            &mut resize,
            &ClientMessage::Resize {
                cols: 80,
                rows: 24,
                cell_width_px: 0,
                cell_height_px: 0,
                pixel_mouse: false,
            },
        )
        .unwrap();
        client.write_all(&resize[..2]).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        client.write_all(&resize[2..]).unwrap();
        assert!(matches!(
            event_rx.blocking_recv(),
            Some(ServerEvent::ClientResize { client_id: 14, .. })
        ));
        writer_stream
            .set_send_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        let (writer_done_tx, writer_done) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            client_writer_loop(writer_stream, 14, queue, events);
            let _ = writer_done_tx.send(());
        });
        writer.render.try_send(vec![0; 4 * 1024 * 1024]).unwrap();
        writer_done
            .recv_timeout(Duration::from_millis(350))
            .expect("writer timed out");
        reader_done
            .recv_timeout(Duration::from_secs(2))
            .expect("reader released")
            .unwrap();
        worker.join().unwrap();
        reader.join().unwrap();
        assert!(writer.control.send(vec![1]).is_err());
    }

    #[test]
    fn clamp_terminal_size_zero_zero() {
        assert_eq!(
            clamp_terminal_size(0, 0),
            (MIN_CLIENT_COLS, MIN_CLIENT_ROWS)
        );
    }

    #[test]
    fn clamp_terminal_size_one_one() {
        assert_eq!(clamp_terminal_size(1, 1), (1, 1));
    }

    #[test]
    fn clamp_terminal_size_preserves_narrow_client_size() {
        assert_eq!(clamp_terminal_size(40, 12), (40, 12));
    }

    #[test]
    fn clamp_terminal_size_valid() {
        assert_eq!(clamp_terminal_size(120, 40), (120, 40));
    }

    #[test]
    fn clamp_terminal_size_exact_minimum() {
        assert_eq!(
            clamp_terminal_size(MIN_CLIENT_COLS, MIN_CLIENT_ROWS),
            (MIN_CLIENT_COLS, MIN_CLIENT_ROWS)
        );
    }

    #[test]
    fn client_shell_geometry_rejects_unsafe_dimensions_and_cell_sizes() {
        assert!(client_shell_geometry_error(
            crate::protocol::ClientSurfaceSize { cols: 80, rows: 24 },
            8,
            16,
        )
        .is_none());
        assert!(client_shell_geometry_error(
            crate::protocol::ClientSurfaceSize {
                cols: MAX_CLIENT_SHELL_DIMENSION,
                rows: MAX_CLIENT_SHELL_DIMENSION,
            },
            8,
            16,
        )
        .is_some());
        assert!(client_shell_geometry_error(
            crate::protocol::ClientSurfaceSize { cols: 80, rows: 24 },
            MAX_CLIENT_CELL_SIZE_PX + 1,
            16,
        )
        .is_some());
    }

    #[test]
    fn unknown_endpoint_method_returns_correlated_error() {
        let decoded = decode_endpoint_request(
            r#"{"id":"req-1","method":"plugin.future","params":{"value":1}}"#,
        )
        .unwrap();
        assert!(matches!(
            decoded,
            DecodedEndpointRequest::Error {
                request_id,
                code: "unsupported_method",
                ..
            } if request_id == "req-1"
        ));
    }

    #[test]
    fn malformed_known_endpoint_method_returns_correlated_error() {
        let decoded =
            decode_endpoint_request(r#"{"id":"req-2","method":"workspace.focus","params":{}}"#)
                .unwrap();
        assert!(matches!(
            decoded,
            DecodedEndpointRequest::Error {
                request_id,
                code: "invalid_request",
                ..
            } if request_id == "req-2"
        ));
    }

    #[test]
    fn handshake_negotiates_terminal_ansi_encoding() {
        let (mut client_stream, server_stream, _path) = local_stream_pair("client-handshake-ansi");
        let (server_event_tx, mut server_event_rx) = mpsc::channel(4);
        let should_quit = Arc::new(AtomicBool::new(false));
        let handshake_quit = should_quit.clone();
        let handle = std::thread::spawn(move || {
            handle_client_handshake(server_stream, 42, &server_event_tx, &handshake_quit)
        });

        protocol::write_message(
            &mut client_stream,
            &ClientMessage::TerminalHello {
                version: PROTOCOL_VERSION,
                cols: 100,
                rows: 30,
                cell_width_px: 8,
                cell_height_px: 16,
                pixel_mouse: true,
            },
        )
        .expect("write hello");

        let welcome: ServerMessage =
            protocol::read_message(&mut client_stream, MAX_FRAME_SIZE).expect("read welcome");
        match welcome {
            ServerMessage::Welcome {
                version,
                encoding,
                error,
            } => {
                assert_eq!(version, PROTOCOL_VERSION);
                assert_eq!(encoding, RenderEncoding::TerminalAnsi);
                assert_eq!(error, None);
            }
            other => panic!("expected Welcome, got {other:?}"),
        }

        match server_event_rx
            .blocking_recv()
            .expect("client connected event")
        {
            ServerEvent::ClientConnected {
                client_id,
                cols,
                rows,
                cell_width_px,
                cell_height_px,
                pixel_mouse,
                writer,
            } => {
                assert_eq!(client_id, 42);
                assert_eq!((cols, rows), (100, 30));
                assert_eq!((cell_width_px, cell_height_px), (8, 16));
                assert!(pixel_mouse);
                drop(writer);
            }
            other => panic!("expected ClientConnected, got {other:?}"),
        }

        drop(client_stream);
        should_quit.store(true, Ordering::Release);
        handle
            .join()
            .expect("handshake thread join")
            .expect("handshake thread result");
    }

    #[test]
    fn client_shell_handshake_carries_client_reported_ssh_auth_sock() {
        let (mut client_stream, server_stream, _path) =
            local_stream_pair("client-shell-ssh-auth-sock");
        let (server_event_tx, mut server_event_rx) = mpsc::channel(4);
        let should_quit = Arc::new(AtomicBool::new(false));
        let handshake_quit = should_quit.clone();
        let handle = std::thread::spawn(move || {
            handle_client_handshake(server_stream, 44, &server_event_tx, &handshake_quit)
        });

        let hello = endpoint_hello(80, 24);
        let ClientMessage::EndpointControl { kind, data } = hello else {
            panic!("expected endpoint hello");
        };
        let mut value: serde_json::Value = serde_json::from_str(&data).unwrap();
        value["ssh_auth_sock"] = serde_json::json!("/run/user/1000/wezterm/agent.7");
        protocol::write_message(
            &mut client_stream,
            &ClientMessage::EndpointControl {
                kind,
                data: serde_json::to_string(&value).unwrap(),
            },
        )
        .expect("write shell hello");

        let welcome: ServerMessage =
            protocol::read_message(&mut client_stream, MAX_FRAME_SIZE).expect("read welcome");
        assert!(endpoint_welcome(welcome).error.is_none());
        match server_event_rx
            .blocking_recv()
            .expect("client shell connected event")
        {
            ServerEvent::ClientShellConnected { ssh_auth_sock, .. } => {
                assert_eq!(
                    ssh_auth_sock.as_deref(),
                    Some("/run/user/1000/wezterm/agent.7")
                );
            }
            other => panic!("expected ClientShellConnected, got {other:?}"),
        }

        drop(client_stream);
        should_quit.store(true, Ordering::Release);
        handle
            .join()
            .expect("handshake thread join")
            .expect("handshake thread result");
    }

    #[test]
    fn dedicated_client_shell_handshake_uses_surface_viewport() {
        let (mut client_stream, server_stream, _path) = local_stream_pair("client-shell-handshake");
        let (server_event_tx, mut server_event_rx) = mpsc::channel(4);
        let should_quit = Arc::new(AtomicBool::new(false));
        let handshake_quit = should_quit.clone();
        let handle = std::thread::spawn(move || {
            handle_client_handshake(server_stream, 43, &server_event_tx, &handshake_quit)
        });

        protocol::write_message(&mut client_stream, &endpoint_hello(80, 29))
            .expect("write shell hello");

        let welcome: ServerMessage =
            protocol::read_message(&mut client_stream, MAX_FRAME_SIZE).expect("read welcome");
        let welcome = endpoint_welcome(welcome);
        assert_eq!(welcome.generation, ENDPOINT_PROTOCOL_GENERATION);
        assert!(welcome.error.is_none());
        match server_event_rx
            .blocking_recv()
            .expect("client shell connected event")
        {
            ServerEvent::ClientShellConnected {
                client_id,
                surface_cols,
                surface_rows,
                cell_width_px,
                cell_height_px,
                pixel_mouse,
                direct_graphics,
                endpoint_keybindings,
                mouse_capture,
                surface_active,
                surface_reuse,
                surface_delta,
                ssh_auth_sock,
                surface_scroll,
                writer,
            } => {
                assert!(!surface_reuse);
                assert!(!surface_delta);
                assert_eq!(ssh_auth_sock, None);
                assert!(!surface_scroll);
                assert_eq!(client_id, 43);
                assert_eq!((surface_cols, surface_rows), (80, 29));
                assert_eq!((cell_width_px, cell_height_px), (8, 16));
                assert!(pixel_mouse);
                assert!(direct_graphics);
                assert!(endpoint_keybindings);
                assert!(mouse_capture);
                assert!(surface_active);
                drop(writer);
            }
            other => panic!("expected ClientShellConnected, got {other:?}"),
        }

        drop(client_stream);
        should_quit.store(true, Ordering::Release);
        handle
            .join()
            .expect("handshake thread join")
            .expect("handshake thread result");
    }

    #[test]
    fn dedicated_client_shell_handshake_rejects_empty_surface() {
        let (mut client_stream, server_stream, _path) =
            local_stream_pair("client-shell-empty-surface");
        let (server_event_tx, mut server_event_rx) = mpsc::channel(4);
        let should_quit = Arc::new(AtomicBool::new(false));
        let handshake_quit = should_quit.clone();
        let handle = std::thread::spawn(move || {
            handle_client_handshake(server_stream, 43, &server_event_tx, &handshake_quit)
        });

        protocol::write_message(&mut client_stream, &endpoint_hello(0, 29))
            .expect("write empty shell hello");

        let welcome: ServerMessage =
            protocol::read_message(&mut client_stream, MAX_FRAME_SIZE).expect("read welcome");
        let welcome = endpoint_welcome(welcome);
        assert!(welcome
            .error
            .is_some_and(|error| error.message.contains("non-empty pane surface")));
        handle
            .join()
            .expect("handshake thread join")
            .expect("handshake thread result");
        assert!(server_event_rx.try_recv().is_err());
    }

    #[test]
    fn client_read_loop_stops_after_detach() {
        let (mut client_stream, server_stream, _path) = local_stream_pair("client-read-detach");
        let (server_event_tx, mut server_event_rx) = mpsc::channel(4);
        let should_quit = Arc::new(AtomicBool::new(false));
        let read_quit = should_quit.clone();
        let handle = std::thread::spawn(move || {
            client_read_loop(server_stream, 7, &server_event_tx, &read_quit)
        });

        let mut messages = Vec::new();
        protocol::write_message(&mut messages, &ClientMessage::Detach).unwrap();
        protocol::write_message(
            &mut messages,
            &ClientMessage::ClipboardImage {
                target: crate::protocol::ClientClipboardImageTarget::DirectTerminal,
                extension: "png".into(),
                data: vec![1, 2, 3],
            },
        )
        .unwrap();
        client_stream
            .write_all(&messages)
            .expect("write detach and trailing message");

        assert!(matches!(
            recv_server_event(&mut server_event_rx, "detach event"),
            ServerEvent::ClientDetach { client_id: 7 }
        ));
        handle
            .join()
            .expect("read thread join")
            .expect("read thread result");
        assert!(server_event_rx.try_recv().is_err());
    }

    #[test]
    fn client_read_loop_ignores_unknown_endpoint_control() {
        let (mut client_stream, server_stream, _path) =
            local_stream_pair("client-read-future-control");
        let (server_event_tx, mut server_event_rx) = mpsc::channel(4);
        let should_quit = Arc::new(AtomicBool::new(false));
        let read_quit = should_quit.clone();
        let handle = std::thread::spawn(move || {
            client_read_loop(server_stream, 7, &server_event_tx, &read_quit)
        });

        protocol::write_message(
            &mut client_stream,
            &ClientMessage::EndpointControl {
                kind: "future.optional.v1".into(),
                data: "{}".into(),
            },
        )
        .unwrap();
        protocol::write_message(&mut client_stream, &ClientMessage::Detach).unwrap();

        assert!(matches!(
            recv_server_event(&mut server_event_rx, "detach after future control"),
            ServerEvent::ClientDetach { client_id: 7 }
        ));
        handle
            .join()
            .expect("read thread join")
            .expect("read thread result");
    }

    #[test]
    fn client_read_loop_closes_on_unsafe_shell_resize() {
        let (mut client_stream, server_stream, _path) =
            local_stream_pair("client-read-unsafe-resize");
        let (server_event_tx, mut server_event_rx) = mpsc::channel(4);
        let should_quit = Arc::new(AtomicBool::new(false));
        let read_quit = should_quit.clone();
        let handle = std::thread::spawn(move || {
            client_read_loop(server_stream, 7, &server_event_tx, &read_quit)
        });

        protocol::write_message(
            &mut client_stream,
            &ClientMessage::ClientShellResize {
                cell_width_px: 8,
                cell_height_px: 16,
                surface_size: crate::protocol::ClientSurfaceSize {
                    cols: MAX_CLIENT_SHELL_DIMENSION,
                    rows: MAX_CLIENT_SHELL_DIMENSION,
                },
                pixel_mouse: false,
            },
        )
        .unwrap();

        assert!(matches!(
            recv_server_event(&mut server_event_rx, "unsafe resize disconnect"),
            ServerEvent::ClientDisconnected { client_id: 7 }
        ));
        handle
            .join()
            .expect("read thread join")
            .expect("read thread result");
    }

    #[test]
    fn client_read_loop_rejects_oversized_bracketed_paste_without_disconnect() {
        let (mut client_stream, server_stream, _path) = local_stream_pair("client-read-oversized");
        let (server_event_tx, mut server_event_rx) = mpsc::channel(4);
        let should_quit = Arc::new(AtomicBool::new(false));
        let read_quit = should_quit.clone();
        let handle = std::thread::spawn(move || {
            client_read_loop(server_stream, 7, &server_event_tx, &read_quit)
        });

        protocol::write_message(
            &mut client_stream,
            &ClientMessage::Input {
                data: bracketed_paste_with_total_len(MAX_INPUT_PAYLOAD),
            },
        )
        .expect("write maximum-size bracketed paste");

        match recv_server_event(&mut server_event_rx, "maximum-size paste event") {
            ServerEvent::ClientInput { client_id, data } => {
                assert_eq!(client_id, 7);
                assert_eq!(data.len(), MAX_INPUT_PAYLOAD);
            }
            other => panic!("expected maximum-size ClientInput, got {other:?}"),
        }

        protocol::write_message(
            &mut client_stream,
            &ClientMessage::Input {
                data: bracketed_paste_with_total_len(MAX_INPUT_PAYLOAD + 1),
            },
        )
        .expect("write oversized bracketed paste");

        match recv_server_event(&mut server_event_rx, "oversized paste rejection") {
            ServerEvent::ClientPasteRejected {
                client_id,
                size,
                max,
            } => {
                assert_eq!(client_id, 7);
                assert_eq!(size, MAX_INPUT_PAYLOAD + 1);
                assert_eq!(max, MAX_INPUT_PAYLOAD);
            }
            ServerEvent::ClientDisconnected { .. } => {
                panic!("oversized input must be rejected without disconnecting the client")
            }
            other => panic!("expected ClientPasteRejected, got {other:?}"),
        }

        protocol::write_message(
            &mut client_stream,
            &ClientMessage::Input {
                data: b"still connected".to_vec(),
            },
        )
        .expect("write valid input after rejection");

        match recv_server_event(&mut server_event_rx, "valid input after rejection") {
            ServerEvent::ClientInput { client_id, data } => {
                assert_eq!(client_id, 7);
                assert_eq!(data, b"still connected");
            }
            other => panic!("expected ClientInput after rejection, got {other:?}"),
        }

        drop(client_stream);
        should_quit.store(true, Ordering::Release);
        handle
            .join()
            .expect("read thread join")
            .expect("read thread result");
    }

    #[test]
    fn client_read_loop_disconnects_oversized_non_paste_input() {
        let (mut client_stream, server_stream, _path) =
            local_stream_pair("client-read-oversized-non-paste");
        let (server_event_tx, mut server_event_rx) = mpsc::channel(4);
        let should_quit = Arc::new(AtomicBool::new(false));
        let read_quit = should_quit.clone();
        let handle = std::thread::spawn(move || {
            client_read_loop(server_stream, 7, &server_event_tx, &read_quit)
        });

        protocol::write_message(
            &mut client_stream,
            &ClientMessage::Input {
                data: vec![b'x'; MAX_INPUT_PAYLOAD + 1],
            },
        )
        .expect("write oversized non-paste input");

        assert!(matches!(
            recv_server_event(&mut server_event_rx, "oversized non-paste disconnect"),
            ServerEvent::ClientDisconnected { client_id: 7 }
        ));

        drop(client_stream);
        should_quit.store(true, Ordering::Release);
        handle
            .join()
            .expect("read thread join")
            .expect("read thread result");
    }

    #[test]
    fn client_read_loop_disconnects_marker_wrapped_invalid_utf8() {
        let (mut client_stream, server_stream, _path) =
            local_stream_pair("client-read-invalid-utf8-paste");
        let (server_event_tx, mut server_event_rx) = mpsc::channel(4);
        let should_quit = Arc::new(AtomicBool::new(false));
        let read_quit = should_quit.clone();
        let handle = std::thread::spawn(move || {
            client_read_loop(server_stream, 7, &server_event_tx, &read_quit)
        });
        let mut data = bracketed_paste_with_total_len(MAX_INPUT_PAYLOAD + 1);
        data[b"\x1b[200~".len()] = 0xff;

        protocol::write_message(&mut client_stream, &ClientMessage::Input { data })
            .expect("write marker-wrapped invalid UTF-8 input");

        assert!(matches!(
            recv_server_event(&mut server_event_rx, "invalid UTF-8 input disconnect"),
            ServerEvent::ClientDisconnected { client_id: 7 }
        ));

        drop(client_stream);
        should_quit.store(true, Ordering::Release);
        handle
            .join()
            .expect("read thread join")
            .expect("read thread result");
    }

    #[test]
    fn client_read_loop_uses_authoritative_shell_resize_surface() {
        let (mut client_stream, server_stream, _path) = local_stream_pair("client-read-resize");
        let (server_event_tx, mut server_event_rx) = mpsc::channel(4);
        let should_quit = Arc::new(AtomicBool::new(false));
        let read_quit = should_quit.clone();
        let handle = std::thread::spawn(move || {
            client_read_loop(server_stream, 7, &server_event_tx, &read_quit)
        });

        protocol::write_message(
            &mut client_stream,
            &ClientMessage::ClientShellResize {
                cell_width_px: 8,
                cell_height_px: 16,
                surface_size: crate::protocol::ClientSurfaceSize { cols: 60, rows: 15 },
                pixel_mouse: true,
            },
        )
        .expect("write shell resize");
        assert!(matches!(
            recv_server_event(&mut server_event_rx, "shell resize"),
            ServerEvent::ClientShellResize {
                client_id: 7,
                surface_cols: 60,
                surface_rows: 15,
                cell_width_px: 8,
                cell_height_px: 16,
                pixel_mouse: true,
            }
        ));

        protocol::write_message(&mut client_stream, &ClientMessage::Detach).expect("write detach");
        assert!(matches!(
            recv_server_event(&mut server_event_rx, "detach event"),
            ServerEvent::ClientDetach { client_id: 7 }
        ));
        handle
            .join()
            .expect("read thread join")
            .expect("read thread result");
    }

    #[test]
    fn client_read_loop_keeps_single_host_theme_updates_ordered_and_palette_bounded() {
        let (mut client_stream, server_stream, _path) = local_stream_pair("client-read-host-theme");
        let (server_event_tx, mut server_event_rx) = mpsc::channel(4);
        let should_quit = Arc::new(AtomicBool::new(false));
        let read_quit = should_quit.clone();
        let handle = std::thread::spawn(move || {
            client_read_loop(server_stream, 7, &server_event_tx, &read_quit)
        });

        let colors = (0..=u8::MAX)
            .map(|index| {
                (
                    index,
                    crate::protocol::ClientHostColor {
                        r: index,
                        g: 0,
                        b: 0,
                    },
                )
            })
            .collect();
        protocol::write_message(
            &mut client_stream,
            &ClientMessage::ClientShellHostTheme {
                update: crate::protocol::ClientHostThemeUpdate::PaletteColors(colors),
            },
        )
        .expect("write bounded palette update");
        protocol::write_message(
            &mut client_stream,
            &ClientMessage::ClientShellHostTheme {
                update: crate::protocol::ClientHostThemeUpdate::Appearance(
                    crate::protocol::ClientHostAppearance::Dark,
                ),
            },
        )
        .expect("write ordered appearance update");

        assert!(matches!(
            recv_server_event(&mut server_event_rx, "bounded palette update"),
            ServerEvent::ClientShellHostTheme {
                client_id: 7,
                update: crate::protocol::ClientHostThemeUpdate::PaletteColors(colors),
            } if colors.len() == 256
        ));
        assert!(matches!(
            recv_server_event(&mut server_event_rx, "ordered appearance update"),
            ServerEvent::ClientShellHostTheme {
                client_id: 7,
                update: crate::protocol::ClientHostThemeUpdate::Appearance(
                    crate::protocol::ClientHostAppearance::Dark
                ),
            }
        ));

        let colors = vec![(0, crate::protocol::ClientHostColor { r: 0, g: 0, b: 0 },); 257];
        protocol::write_message(
            &mut client_stream,
            &ClientMessage::ClientShellHostTheme {
                update: crate::protocol::ClientHostThemeUpdate::PaletteColors(colors),
            },
        )
        .expect("write oversized palette update");
        assert!(matches!(
            recv_server_event(&mut server_event_rx, "oversized palette disconnect"),
            ServerEvent::ClientDisconnected { client_id: 7 }
        ));

        drop(client_stream);
        should_quit.store(true, Ordering::Release);
        handle
            .join()
            .expect("read thread join")
            .expect("read thread result");
    }

    #[test]
    fn pane_input_limits_charge_scroll_repeats() {
        let oversized_scroll = ClientPaneInputEvent::Mouse {
            kind: crate::protocol::ClientMouseKind::ScrollUp,
            position: crate::protocol::ClientMousePosition::Cell { column: 0, row: 0 },
            geometry: None,
            modifiers: 0,
            lines: (MAX_INPUT_EVENT_BATCH + 1) as u16,
        };
        assert_eq!(
            pane_input_event_limit(&[oversized_scroll]),
            InputEventLimit::TooManyEvents
        );
    }

    #[test]
    fn handshake_timeout_is_within_five_second_deadline() {
        // The handshake timeout must be short enough that
        // the connection is guaranteed to close within 5 seconds even with
        // OS overhead (thread scheduling, timer slack, cleanup).
        assert!(
            HANDSHAKE_TIMEOUT < Duration::from_secs(5),
            "HANDSHAKE_TIMEOUT ({:?}) must be less than 5 seconds to guarantee \
             connection close within the 5-second deadline",
            HANDSHAKE_TIMEOUT
        );
    }
}
