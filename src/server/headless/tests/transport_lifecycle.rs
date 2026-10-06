use super::*;
use interprocess::local_socket::traits::Listener as _;
use std::io;

use crate::server::client_transport::{
    test_endpoint_hello, ClientTransportTestHandle, HandshakeTestStage as Stage,
};

pub(super) struct EndpointTransport {
    pub(super) peer: crate::platform::ServerClientStream,
    pub(super) observer: ClientTransportTestHandle,
    reader: Option<std::thread::JoinHandle<io::Result<()>>>,
    _scratch: ScratchDir,
}

impl EndpointTransport {
    pub(super) async fn new(server: &mut HeadlessServer) -> (Self, ServerEvent) {
        Self::with_id(server, 701).await
    }

    pub(super) async fn with_id(
        server: &mut HeadlessServer,
        client_id: u64,
    ) -> (Self, ServerEvent) {
        let scratch = ScratchDir::new("ht");
        let path = scratch.path().join("client.sock");
        let listener = bind_local_listener(&path).unwrap();
        let peer = crate::ipc::connect_local_stream(&path).unwrap();
        let stream = listener.accept().unwrap();
        let events = server.server_event_tx.clone();
        let quit = server.should_quit.clone();
        let permit = server.client_handshake_limiter.try_acquire().unwrap();
        let reader = std::thread::spawn(move || {
            crate::server::client_transport::handle_client_handshake_with_permit(
                stream,
                client_id,
                &events,
                &quit,
                Some(permit),
            )
        });
        let mut peer = crate::platform::prepare_server_client_stream(peer, LOADED_WAIT).unwrap();
        protocol::write_message(&mut peer, &test_endpoint_hello(80, 24)).unwrap();
        let welcome: ServerMessage = protocol::read_message(&mut peer, MAX_FRAME_SIZE).unwrap();
        assert!(matches!(welcome, ServerMessage::EndpointControl { .. }));
        let event = tokio::time::timeout(LOADED_WAIT, server.server_event_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let ServerEvent::ClientShellConnected { writer, .. } = &event else {
            panic!("expected endpoint connection");
        };
        let observer = writer.test_transport_handle();
        observer.wait_reader_started();
        (
            Self {
                peer,
                observer,
                reader: Some(reader),
                _scratch: scratch,
            },
            event,
        )
    }

    async fn join_reader(&mut self) {
        tokio::time::timeout(LOADED_WAIT, async {
            while self
                .reader
                .as_ref()
                .is_some_and(|reader| !reader.is_finished())
            {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("endpoint reader must exit without a new peer message");
        if let Some(reader) = self.reader.take() {
            reader.join().unwrap().unwrap();
        }
    }

    async fn completed(&self) -> bool {
        tokio::time::timeout(LOADED_WAIT, async {
            while !self.observer.is_complete() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .is_ok()
    }
}

impl Drop for EndpointTransport {
    fn drop(&mut self) {
        self.observer.abort();
        if let Some(reader) = self.reader.take() {
            reader.join().unwrap().unwrap();
        }
    }
}

fn fill_other_client_events(server: &HeadlessServer) {
    while server
        .server_event_tx
        .try_send(ServerEvent::ClientResize {
            client_id: 900,
            cols: 80,
            rows: 24,
            cell_width_px: 0,
            cell_height_px: 0,
            pixel_mouse: false,
        })
        .is_ok()
    {}
}

async fn input_before_disconnect(
    full_channel: bool,
    invalid_input: bool,
    write_failure: bool,
    connected_pending: bool,
) {
    let _dirs = crate::config::test_dirs::isolate_dirs("transport-input-order");
    let mut server = test_headless_server();
    let capacity = EXTERNAL_EVENT_DRAIN_LIMIT * 2 + usize::from(!full_channel);
    let (sender, receiver) = mpsc::channel(capacity);
    server.server_event_tx = sender;
    server.server_event_rx = receiver;
    let mut input = install_focused_test_runtime(&mut server, b"\x1b[>3u");
    let pane_id = server.app.session_snapshot().focused_pane_id.unwrap();
    let (mut connection, event) = EndpointTransport::new(&mut server).await;
    if connected_pending {
        server.server_event_tx.try_send(event).unwrap();
    } else {
        server.handle_server_event_with_render_impact(event);
        assert!(server.clients.contains_key(&701));
        assert!(server.server_event_rx.is_empty());
    }
    for _ in server.server_event_rx.len()..(EXTERNAL_EVENT_DRAIN_LIMIT * 2 - 1) {
        server
            .server_event_tx
            .try_send(ServerEvent::QuitSignal)
            .unwrap();
    }
    let key = |kind| protocol::ClientPaneInputEvent::Key {
        code: protocol::ClientKeyCode::Char('x'),
        modifiers: 0,
        kind,
        repeat_count: 1,
        shifted_codepoint: None,
        generated_text: (kind == protocol::ClientKeyKind::Press).then(|| "x".into()),
        tracks_release: true,
        physical_key_id: Some(0x2d),
        windows_record: None,
    };
    protocol::write_message(
        &mut connection.peer,
        &protocol::ClientMessage::ClientShellPaneInput {
            pane_id: pane_id.clone(),
            events: vec![
                protocol::ClientPaneInputEvent::TextCommit("legal-before-close".into()),
                key(protocol::ClientKeyKind::Press),
            ],
        },
    )
    .unwrap();
    tokio::time::timeout(LOADED_WAIT, async {
        while server.server_event_rx.len() < EXTERNAL_EVENT_DRAIN_LIMIT * 2 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    if write_failure {
        connection.observer.abort();
    } else {
        let closing = if invalid_input {
            protocol::ClientMessage::Input {
                data: vec![0; 1024 * 1024 + 1],
            }
        } else {
            protocol::ClientMessage::Detach
        };
        protocol::write_message(&mut connection.peer, &closing).unwrap();
    }
    connection.join_reader().await;
    if write_failure && !connected_pending {
        server.remove_client(701);
        assert!(
            server.clients.contains_key(&701),
            "write-failure removal must honor accepted input"
        );
    }
    server.drain_server_events();
    assert!(
        server.clients.contains_key(&701),
        "disconnect must not overtake accepted input"
    );
    fill_other_client_events(&server);
    server.drain_server_events();
    assert!(
        server.clients[&701].owns_shell_release(&pane_id, &key(protocol::ClientKeyKind::Release))
    );
    let accepted = tokio::time::timeout(LOADED_WAIT, input.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&accepted).contains("legal-before-close"),
        "accepted input must reach the test PTY: {accepted:?}"
    );
    for _ in 0..4 {
        fill_other_client_events(&server);
        server.drain_server_events();
        if !server.clients.contains_key(&701) {
            break;
        }
    }
    assert!(
        !server.clients.contains_key(&701),
        "disconnect cannot wait for the global channel to empty"
    );
    tokio::time::timeout(LOADED_WAIT, async {
        loop {
            let release = input
                .recv()
                .await
                .expect("held-key release reaches the test PTY");
            if release.windows(3).any(|bytes| bytes == b":3u") {
                break;
            }
        }
    })
    .await
    .expect("disconnect must synthesize the Kitty key-release event");
    assert!(
        !server.server_event_rx.is_empty(),
        "other clients still have a backlog"
    );
    connection.observer.abort();
    assert!(connection.completed().await);
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn accepted_input_precedes_queued_detach_and_releases_held_key() {
    input_before_disconnect(false, false, false, false).await;
}

#[tokio::test]
async fn accepted_input_precedes_full_channel_detach_and_releases_held_key() {
    input_before_disconnect(true, false, false, false).await;
}

#[tokio::test]
async fn legal_input_precedes_invalid_input_disconnect() {
    input_before_disconnect(true, true, false, false).await;
}

#[tokio::test]
async fn accepted_input_precedes_writer_failure_disconnect() {
    input_before_disconnect(true, false, true, false).await;
}

#[tokio::test]
async fn accepted_input_survives_failure_before_connected_dispatch() {
    input_before_disconnect(true, false, true, true).await;
}

#[tokio::test]
async fn selected_unregistered_endpoint_is_retired_on_shutdown() {
    let _dirs = crate::config::test_dirs::isolate_dirs("selected-endpoint-shutdown");
    let mut server = test_headless_server();
    let (mut connection, selected) = EndpointTransport::new(&mut server).await;
    server.should_quit.store(true, Ordering::Release);
    server.handle_selected_shutdown_event(LoopEvent::ServerEvent(selected));
    let shutdown: ServerMessage =
        protocol::read_message(&mut connection.peer, MAX_FRAME_SIZE).unwrap();
    assert!(matches!(shutdown, ServerMessage::ServerShutdown { .. }));
    server.complete_shutdown().await.unwrap();
    let completed = connection.completed().await;
    connection.observer.abort();
    connection.join_reader().await;
    assert!(
        completed,
        "selected endpoint must belong to shutdown retirement"
    );
}

fn pending_terminal_writer(server: &mut HeadlessServer, client_id: u64) -> ClientWriter {
    let writer = ClientWriter::test_paused();
    server.clients.insert(
        client_id,
        ClientConnection::new_with_mode(
            ClientConnectionMode::TerminalPending,
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            1,
            RenderEncoding::TerminalAnsi,
            Some(writer.clone()),
        ),
    );
    writer
}

#[test]
fn removal_boundary_zero_check_closes_event_admission_before_map_removal() {
    let _dirs = crate::config::test_dirs::isolate_dirs("zero-credit-removal");
    let mut server = test_headless_server();
    let writer = pending_terminal_writer(&mut server, 701);
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let reader_barrier = barrier.clone();
    let reader_writer = writer.clone();
    let events = server.server_event_tx.clone();
    let reader = std::thread::spawn(move || {
        reader_barrier.wait();
        reader_writer.test_accept_event(
            ServerEvent::ClientInput {
                client_id: 701,
                data: b"too-late".to_vec(),
            },
            &events,
        )
    });
    assert!(!writer.defer_removal_for_accepted_events());
    barrier.wait();
    let accepted = reader.join().unwrap();
    assert!(server.clients.remove(&701).is_some());
    writer.seal();
    assert!(
        !accepted,
        "reader cannot accept input after the zero-credit removal decision"
    );
    assert!(server.server_event_rx.is_empty());
}

#[tokio::test]
async fn removal_boundary_old_owner_cleanup_preserves_takeover_lease() {
    let _dirs = crate::config::test_dirs::isolate_dirs("takeover-owner-removal");
    let mut server = test_headless_server();
    let workspace = crate::workspace::Workspace::test_new("takeover");
    let pane_id = workspace.tabs[0].root_pane;
    let terminal_id = workspace.terminal_id(pane_id).unwrap().clone();
    let target = terminal_id.to_string();
    server.app.state.workspaces = vec![workspace];
    server.app.state.ensure_test_terminals();
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    let (runtime, mut input) =
        crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(80, 24, 0, b"", 4);
    server
        .app
        .terminal_runtimes
        .insert(terminal_id.clone(), runtime);
    let old = pending_terminal_writer(&mut server, 701);
    pending_terminal_writer(&mut server, 702);
    pending_terminal_writer(&mut server, 703);
    assert!(server.attach_terminal_client(701, target.clone(), false));
    assert!(old.test_accept_event(
        ServerEvent::ClientInput {
            client_id: 701,
            data: b"accepted-before-takeover".to_vec(),
        },
        &server.server_event_tx
    ));
    assert!(server.attach_terminal_client(702, target.clone(), true));
    assert!(
        server.clients.contains_key(&701),
        "old owner's accepted input delays removal"
    );
    assert_eq!(server.terminal_attach_owners.get(&target), Some(&702));
    server.drain_server_events();
    server.drain_server_events();
    assert!(!server.clients.contains_key(&701));
    assert_eq!(
        tokio::time::timeout(LOADED_WAIT, input.recv())
            .await
            .unwrap()
            .unwrap()
            .as_ref(),
        b"accepted-before-takeover"
    );
    assert_eq!(
        server.terminal_attach_owners.get(&target),
        Some(&702),
        "old cleanup cannot erase the new owner"
    );
    assert!(server
        .app
        .state
        .direct_attach_resize_locks
        .contains(&terminal_id));
    assert!(!server.attach_terminal_client(703, target.clone(), false));
    assert!(!server.clients.contains_key(&703));
    assert_eq!(server.terminal_attach_owners.get(&target), Some(&702));
    assert!(server
        .app
        .state
        .direct_attach_resize_locks
        .contains(&terminal_id));
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn queued_unregistered_endpoint_is_aborted_when_server_drops() {
    let _dirs = crate::config::test_dirs::isolate_dirs("queued-endpoint-drop");
    let mut server = test_headless_server();
    let (mut connection, queued) = EndpointTransport::new(&mut server).await;
    server.server_event_tx.try_send(queued).unwrap();
    assert_eq!(server.server_event_rx.len(), 1);
    drop(server);
    let completed = connection.completed().await;
    connection.observer.abort();
    connection.join_reader().await;
    assert!(
        completed,
        "queued endpoint must be aborted without peer input"
    );
}

fn paused_transport(
    server: &HeadlessServer,
    stage: Stage,
) -> (
    EndpointTransport,
    crate::server::client_transport::ClientHandshakeTestPause,
) {
    let limiter = &server.client_handshake_limiter;
    let pause = limiter.test_pause_at(stage);
    let permit = limiter.try_acquire().unwrap();
    let scratch = ScratchDir::new("pre");
    let path = scratch.path().join("client.sock");
    let listener = bind_local_listener(&path).unwrap();
    let peer = crate::ipc::connect_local_stream(&path).unwrap();
    let stream = listener.accept().unwrap();
    let events = server.server_event_tx.clone();
    let quit = server.should_quit.clone();
    let reader = std::thread::spawn(move || {
        crate::server::client_transport::handle_client_handshake_with_permit(
            stream,
            701,
            &events,
            &quit,
            Some(permit),
        )
    });
    let mut peer = crate::platform::prepare_server_client_stream(peer, LOADED_WAIT).unwrap();
    protocol::write_message(&mut peer, &test_endpoint_hello(80, 24)).unwrap();
    let _: ServerMessage = protocol::read_message(&mut peer, MAX_FRAME_SIZE).unwrap();
    let observer = pause.wait();
    if stage == Stage::AfterSpawn {
        observer.wait_writer_started();
    }
    (
        EndpointTransport {
            peer,
            observer,
            reader: Some(reader),
            _scratch: scratch,
        },
        pause,
    )
}

#[cfg(windows)]
#[tokio::test]
async fn pre_connected_writer_belongs_to_complete_shutdown() {
    let _dirs = crate::config::test_dirs::isolate_dirs("pre-connected-shutdown");
    let mut server = test_headless_server();
    let (mut connection, pause) = paused_transport(&server, Stage::AfterSpawn);
    server.server_event_rx.close();
    drop(pause);
    connection.join_reader().await;
    assert!(server.server_event_rx.is_empty());
    assert!(
        !connection.observer.is_complete(),
        "native writer must still be flushing"
    );
    let start = Instant::now();
    let result = server.complete_shutdown().await;
    let elapsed = start.elapsed();
    let complete_at_return = connection.observer.is_complete();
    connection.observer.abort();
    assert!(
        connection.completed().await,
        "test cleanup must reap the writer"
    );
    result.unwrap();
    assert!(
        elapsed < Duration::from_secs(3),
        "shared shutdown budget: {elapsed:?}"
    );
    assert!(
        complete_at_return,
        "complete_shutdown returned before the pre-Connected writer completed"
    );
}

fn occupy_writer(connection: &EndpointTransport) {
    connection
        .observer
        .send_control(&ServerMessage::EndpointControl {
            kind: "accepted-before-shutdown".into(),
            data: "x".repeat(512 * 1024),
        });
}

async fn wait_short_completion(connection: &EndpointTransport) -> bool {
    tokio::time::timeout(Duration::from_secs(3), async {
        while !connection.observer.is_complete() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .is_ok()
}

#[tokio::test]
async fn pre_connected_full_channel_writers_share_shutdown_deadline() {
    let _dirs = crate::config::test_dirs::isolate_dirs("pre-connected-full");
    let mut server = test_headless_server();
    fill_other_client_events(&server);
    let mut connections = Vec::new();
    for _ in 0..4 {
        let (connection, pause) = paused_transport(&server, Stage::AfterSpawn);
        occupy_writer(&connection);
        connections.push(connection);
        drop(pause);
    }
    assert_eq!(server.server_event_rx.len(), 64);
    assert_eq!(server.client_handshake_limiter.test_transport_count(), 4);
    let started = Instant::now();
    let result = server.complete_shutdown().await;
    let elapsed = started.elapsed();
    let complete_at_return = connections
        .iter()
        .all(|connection| connection.observer.is_complete());
    for connection in &mut connections {
        connection.observer.abort();
        connection.join_reader().await;
        assert!(connection.completed().await);
    }
    result.unwrap();
    assert!(
        elapsed < Duration::from_secs(3),
        "deadline multiplied by connection count: {elapsed:?}"
    );
    assert!(complete_at_return);
    assert!(server.shutdown_transports.is_empty());
}

async fn spawned_then_quit(drop_server: bool) {
    let _dirs = crate::config::test_dirs::isolate_dirs("spawned-then-quit");
    let mut server = test_headless_server();
    let (mut connection, pause) = paused_transport(&server, Stage::AfterSpawn);
    occupy_writer(&connection);
    server.should_quit.store(true, Ordering::Release);
    drop(pause);
    connection.join_reader().await;
    assert!(server.server_event_rx.is_empty());
    if drop_server {
        drop(server);
    } else {
        server.complete_shutdown().await.unwrap();
        assert!(connection.observer.is_complete());
    }
    let completed = wait_short_completion(&connection).await;
    connection.observer.abort();
    assert!(connection.completed().await);
    assert!(completed, "quit after spawn must preserve server ownership");
}

#[tokio::test]
async fn pre_connected_spawn_then_quit_is_owned_by_shutdown() {
    spawned_then_quit(false).await;
}

#[tokio::test]
async fn pre_connected_spawn_then_quit_is_owned_by_drop() {
    spawned_then_quit(true).await;
}

#[tokio::test]
async fn pre_connected_registration_after_close_rolls_back_without_spawn() {
    let _dirs = crate::config::test_dirs::isolate_dirs("late-registration");
    let mut server = test_headless_server();
    let (mut connection, pause) = paused_transport(&server, Stage::BeforeRegister);
    assert!(!connection.observer.writer_started());
    assert_eq!(server.client_handshake_limiter.test_transport_count(), 0);
    server.complete_shutdown().await.unwrap();
    drop(pause);
    connection.join_reader().await;
    assert!(connection.observer.is_complete());
    assert!(connection.observer.was_aborted());
    assert!(
        !connection.observer.writer_started(),
        "a late registration must never spawn"
    );
    assert!(server.server_event_rx.is_empty());
    assert_eq!(server.client_handshake_limiter.test_transport_count(), 0);
}

async fn poll_shutdown_once<F: std::future::Future>(future: std::pin::Pin<&mut F>) {
    let mut future = future;
    std::future::poll_fn(|cx| {
        assert!(
            future.as_mut().poll(cx).is_pending(),
            "shutdown cannot complete with an unstarted registered worker"
        );
        std::task::Poll::Ready(())
    })
    .await;
}

#[tokio::test]
async fn pre_connected_registered_before_spawn_cannot_report_complete() {
    let _dirs = crate::config::test_dirs::isolate_dirs("registered-before-spawn");
    let mut server = test_headless_server();
    let (mut connection, pause) = paused_transport(&server, Stage::BeforeSpawn);
    occupy_writer(&connection);
    assert_eq!(server.client_handshake_limiter.test_transport_count(), 1);
    let mut shutdown = Box::pin(server.complete_shutdown());
    poll_shutdown_once(shutdown.as_mut()).await;
    assert!(!connection.observer.is_complete());
    drop(pause);
    shutdown.await.unwrap();
    assert!(connection.observer.is_complete());
    connection.join_reader().await;
}

#[tokio::test]
async fn pre_connected_cancelled_shutdown_future_retains_drop_ownership() {
    let _dirs = crate::config::test_dirs::isolate_dirs("cancelled-transport-shutdown");
    let mut server = test_headless_server();
    let (mut connection, pause) = paused_transport(&server, Stage::BeforeSpawn);
    let mut shutdown = Box::pin(server.complete_shutdown());
    poll_shutdown_once(shutdown.as_mut()).await;
    drop(shutdown);
    assert_eq!(server.shutdown_transports.len(), 1);
    drop(server);
    let aborted_by_drop = connection.observer.was_aborted();
    drop(pause);
    connection.join_reader().await;
    let completed = wait_short_completion(&connection).await;
    connection.observer.abort();
    assert!(connection.completed().await);
    assert!(
        aborted_by_drop,
        "Drop must still find the snapshot after future cancellation"
    );
    assert!(completed);
}

#[tokio::test]
async fn pre_connected_cancellation_timeout_keeps_incomplete_owner() {
    let _dirs = crate::config::test_dirs::isolate_dirs("incomplete-transport-shutdown");
    let mut server = test_headless_server();
    let (mut connection, pause) = paused_transport(&server, Stage::BeforeSpawn);
    let result = server.complete_shutdown().await;
    let retained = server.shutdown_transports.len();
    let incomplete = !connection.observer.is_complete();
    drop(server);
    drop(pause);
    connection.join_reader().await;
    assert!(wait_short_completion(&connection).await);
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
    assert!(
        incomplete,
        "a paused worker is not a completed cancellation"
    );
    assert_eq!(
        retained, 1,
        "error return must not discard the last server owner"
    );
}

#[tokio::test]
async fn pre_connected_shutdown_preserves_single_tail_and_accepted_control() {
    let _dirs = crate::config::test_dirs::isolate_dirs("pre-connected-graceful");
    let mut server = test_headless_server();
    let (mut connection, pause) = paused_transport(&server, Stage::AfterSpawn);
    connection
        .observer
        .send_control(&ServerMessage::EndpointControl {
            kind: "accepted".into(),
            data: "x".repeat(1024),
        });
    server.server_event_rx.close();
    #[cfg(unix)]
    use interprocess::TryClone as _;
    let peer = connection.peer.try_clone().unwrap();
    let receiver = std::thread::spawn(move || {
        struct Fragmented(crate::platform::ServerClientStream);
        impl std::io::Read for Fragmented {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                let len = buffer.len().min(7);
                std::io::Read::read(&mut self.0, &mut buffer[..len])
            }
        }
        std::thread::sleep(Duration::from_millis(50));
        let mut peer = Fragmented(peer);
        let accepted: ServerMessage = protocol::read_message(&mut peer, MAX_FRAME_SIZE).unwrap();
        assert!(
            matches!(accepted, ServerMessage::EndpointControl { kind, data } if kind == "accepted" && data == "x".repeat(1024))
        );
        let tail: ServerMessage = protocol::read_message(&mut peer, MAX_FRAME_SIZE).unwrap();
        assert!(matches!(tail, ServerMessage::ServerShutdown { .. }));
        let end = std::io::Read::read(&mut peer, &mut [0]);
        assert!(
            matches!(end, Ok(0))
                || end.as_ref().is_err_and(|error| {
                    error.kind() == io::ErrorKind::BrokenPipe
                        || (cfg!(windows) && error.raw_os_error() == Some(233))
                }),
            "no duplicate or partial tail bytes before native EOF: {end:?}"
        );
    });
    let mut shutdown = Box::pin(server.complete_shutdown());
    poll_shutdown_once(shutdown.as_mut()).await;
    drop(pause);
    let result = shutdown.await;
    let graceful = !connection.observer.was_aborted();
    connection.join_reader().await;
    receiver.join().unwrap();
    result.unwrap();
    assert!(connection.observer.is_complete());
    assert!(
        graceful,
        "legal delayed/fragmented reads must drain rather than abort"
    );
}

#[tokio::test]
async fn pre_connected_full_channel_does_not_renew_handshake_deadline() {
    let _dirs = crate::config::test_dirs::isolate_dirs("connected-absolute-deadline");
    let mut server = test_headless_server();
    fill_other_client_events(&server);
    let started = Instant::now();
    let (mut connection, pause) = paused_transport(&server, Stage::AfterSpawn);
    occupy_writer(&connection);
    drop(pause);
    tokio::time::timeout(Duration::from_secs(6), connection.join_reader())
        .await
        .unwrap();
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(3500) && elapsed < Duration::from_secs(6),
        "absolute handshake deadline: {elapsed:?}"
    );
    assert_eq!(server.server_event_rx.len(), 64);
    assert_eq!(server.client_handshake_limiter.test_transport_count(), 1);
    server.complete_shutdown().await.unwrap();
    assert!(connection.observer.is_complete());
}

#[tokio::test]
async fn transport_registry_churn_unregisters_completed_native_connections() {
    let _dirs = crate::config::test_dirs::isolate_dirs("transport-registry-churn");
    let mut server = test_headless_server();
    for _ in 0..24 {
        let (mut connection, event) = EndpointTransport::new(&mut server).await;
        let ServerEvent::ClientShellConnected { writer, .. } = &event else {
            unreachable!()
        };
        assert_eq!(server.client_handshake_limiter.test_transport_count(), 1);
        writer.transport().shutdown("churn", None);
        let tail: ServerMessage =
            protocol::read_message(&mut connection.peer, MAX_FRAME_SIZE).unwrap();
        assert!(matches!(tail, ServerMessage::ServerShutdown { .. }));
        assert!(wait_short_completion(&connection).await);
        connection.join_reader().await;
        tokio::time::timeout(LOADED_WAIT, async {
            while server.client_handshake_limiter.test_transport_count() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        while server.server_event_rx.try_recv().is_ok() {}
        assert!(!connection.observer.was_aborted());
    }
    server.complete_shutdown().await.unwrap();
}
