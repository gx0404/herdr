use super::*;

fn queue_requests(server: &mut HeadlessServer, count: usize) -> std::sync::mpsc::Receiver<String> {
    let (sender, receiver) = mpsc::unbounded_channel();
    server.app.api_rx = receiver;
    let (respond_to, responses) = std::sync::mpsc::channel();
    for index in 0..count {
        sender
            .send(api::ApiRequestMessage {
                request: api::schema::Request {
                    id: index.to_string(),
                    method: api::schema::Method::WorkspaceList(api::schema::EmptyParams::default()),
                },
                respond_to: respond_to.clone(),
                response_write_complete: None,
                stream_active: None,
                observation_events: None,
                report_origin: None,
            })
            .unwrap();
    }
    responses
}

#[test]
fn external_api_batches_preserve_fifo_and_leave_work_for_the_next_turn() {
    let mut server = test_headless_server();
    let count = EXTERNAL_EVENT_DRAIN_LIMIT * 2 + 1;
    let responses = queue_requests(&mut server, count);
    let mut processed = 0;
    while !server.app.api_rx.is_empty() {
        server.drain_api_requests_with_shutdown_check();
        let batch: Vec<_> = responses.try_iter().collect();
        assert_eq!(
            batch.len(),
            EXTERNAL_EVENT_DRAIN_LIMIT.min(count - processed)
        );
        for response in batch {
            let response: serde_json::Value = serde_json::from_str(&response).unwrap();
            assert_eq!(response["id"], processed.to_string());
            assert!(response.get("result").is_some());
            processed += 1;
        }
        assert_eq!(server.app.api_rx.len(), count - processed);
    }
    assert_eq!(processed, count);
}

#[test]
fn external_server_events_yield_with_backlog_and_preserve_fifo() {
    let mut server = test_headless_server();
    let count = EXTERNAL_EVENT_DRAIN_LIMIT * 2 + 1;
    let (sender, receiver) = mpsc::channel(count);
    server.server_event_rx = receiver;
    server.server_event_tx = sender.clone();
    for client_id in 0..count as u64 {
        sender
            .try_send(ServerEvent::ClientDisconnected { client_id })
            .unwrap();
    }
    server.drain_server_events();
    assert_eq!(
        server.server_event_rx.len(),
        count - EXTERNAL_EVENT_DRAIN_LIMIT
    );
    let ServerEvent::ClientDisconnected { client_id } = server.server_event_rx.try_recv().unwrap()
    else {
        panic!("expected the next disconnect");
    };
    assert_eq!(client_id, EXTERNAL_EVENT_DRAIN_LIMIT as u64);
    server.drain_server_events();
    assert!(server.server_event_rx.is_empty());
}

#[test]
fn external_api_batch_stops_immediately_when_shutdown_is_requested() {
    let mut server = test_headless_server();
    let responses = queue_requests(&mut server, EXTERNAL_EVENT_DRAIN_LIMIT + 1);
    server.should_quit.store(true, Ordering::Release);
    assert!(!server.drain_api_requests_with_shutdown_check());
    assert_eq!(server.app.api_rx.len(), EXTERNAL_EVENT_DRAIN_LIMIT + 1);
    assert_eq!(responses.try_iter().count(), 0);
}

#[test]
fn scheduled_work_runs_between_external_api_batches() {
    let mut server = test_headless_server();
    let responses = queue_requests(&mut server, EXTERNAL_EVENT_DRAIN_LIMIT + 1);
    let now = Instant::now();
    server.app.config_diagnostic_deadline = Some(now);
    server.app.state.config_diagnostic = Some("expired diagnostic".into());

    server.drain_api_requests_with_shutdown_check();
    assert_eq!(responses.try_iter().count(), EXTERNAL_EVENT_DRAIN_LIMIT);
    assert_eq!(server.app.api_rx.len(), 1);
    let impact = server.handle_scheduled_tasks_headless(now, false);
    assert!(impact.chrome);
    assert!(server.app.state.config_diagnostic.is_none());
    assert!(server.app.config_diagnostic_deadline.is_none());
    assert_eq!(server.app.api_rx.len(), 1);
}

#[test]
fn api_request_preflight_preserves_query_boundary_and_leaves_late_events() {
    let mut server = test_headless_server();
    server
        .app
        .event_tx
        .try_send(AppEvent::UpdateReady {
            version: "4.0.before".into(),
            install_command: "herdr install".into(),
        })
        .unwrap();
    let responses = queue_requests(&mut server, 1);

    server.drain_api_requests_with_shutdown_check();

    let response = responses.try_iter().next().expect("API response");
    let response: serde_json::Value = serde_json::from_str(&response).unwrap();
    assert_eq!(response["id"], "0");
    assert_eq!(
        server.app.state.update_available.as_deref(),
        Some("4.0.before")
    );

    server
        .app
        .event_tx
        .try_send(AppEvent::UpdateReady {
            version: "4.0.after".into(),
            install_command: "herdr install".into(),
        })
        .unwrap();
    assert_eq!(server.app.event_rx.len(), 1);
}

#[tokio::test]
async fn server_loop_drains_api_backlog_and_runs_scheduled_work() {
    let mut server = test_headless_server();
    let (sender, receiver) = mpsc::unbounded_channel();
    server.app.api_rx = receiver;
    let (respond_to, responses) = std::sync::mpsc::channel();
    let count = EXTERNAL_EVENT_DRAIN_LIMIT * 2 + 1;
    for index in 0..=count {
        // A queued stop terminates the real loop without a concurrent producer
        // or a timing assertion. Earlier requests must all receive responses.
        let method = if index == count {
            api::schema::Method::ServerStop(api::schema::EmptyParams::default())
        } else {
            api::schema::Method::WorkspaceList(api::schema::EmptyParams::default())
        };
        sender
            .send(api::ApiRequestMessage {
                request: api::schema::Request {
                    id: index.to_string(),
                    method,
                },
                respond_to: respond_to.clone(),
                response_write_complete: None,
                stream_active: None,
                observation_events: None,
                report_origin: None,
            })
            .unwrap();
    }
    server.app.config_diagnostic_deadline = Some(Instant::now());
    server.app.state.config_diagnostic = Some("expired diagnostic".into());

    tokio::time::timeout(LOADED_WAIT, server.run())
        .await
        .expect("queued API requests must wake the server loop")
        .expect("server loop shuts down cleanly");

    let responses: Vec<_> = responses.try_iter().collect();
    assert_eq!(responses.len(), count + 1);
    for (index, response) in responses.iter().enumerate() {
        let response: serde_json::Value = serde_json::from_str(response).unwrap();
        assert_eq!(response["id"], index.to_string());
        assert!(response.get("result").is_some());
    }
    assert!(server.app.api_rx.is_empty());
    assert!(server.app.state.config_diagnostic.is_none());
    assert!(server.app.config_diagnostic_deadline.is_none());
    assert_eq!(server.app.terminal_runtimes.len(), 0);
}

fn transport_test_writer(server: &mut HeadlessServer, client_id: u64) -> ClientWriter {
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

fn fill_server_event_channel(server: &HeadlessServer) {
    while server
        .server_event_tx
        .try_send(ServerEvent::QuitSignal)
        .is_ok()
    {}
}

fn handled_server_events() -> usize {
    TEST_HANDLED_SERVER_EVENTS.with(std::cell::Cell::get)
}

#[test]
fn transport_notifications_share_one_budget_with_queued_events() {
    let _dirs = crate::config::test_dirs::isolate_dirs("notification-budget");
    let mut server = test_headless_server();
    fill_server_event_channel(&server);
    let queued = server.server_event_rx.len();
    let fallback = EXTERNAL_EVENT_DRAIN_LIMIT * 2 + 1;
    for client_id in 1..=fallback as u64 {
        transport_test_writer(&mut server, client_id)
            .test_notify_disconnect(client_id, &server.server_event_tx);
    }
    let initial = handled_server_events();
    for _ in 0..8 {
        let before = handled_server_events();
        server.drain_server_events();
        assert!(
            handled_server_events() - before <= EXTERNAL_EVENT_DRAIN_LIMIT,
            "channel and fallback must share one event budget"
        );
        if server.clients.is_empty() && server.server_event_rx.is_empty() {
            break;
        }
        assert!(
            server.transport_notifications.more_pending,
            "leftover fallback work must request another turn without waiting for I/O"
        );
    }
    assert!(
        server.clients.is_empty(),
        "unprocessed disconnects must survive for later turns"
    );
    assert!(server.server_event_rx.is_empty());
    assert_eq!(handled_server_events() - initial, queued + fallback);
}

#[test]
fn transport_shutdown_does_not_take_pending_notifications() {
    let _dirs = crate::config::test_dirs::isolate_dirs("notification-shutdown");
    let mut server = test_headless_server();
    let writer = transport_test_writer(&mut server, 1);
    fill_server_event_channel(&server);
    writer.test_notify_drained(1, &server.server_event_tx);
    let queued = server.server_event_rx.len();
    let before = handled_server_events();
    server.should_quit.store(true, Ordering::Release);
    assert!(!server.drain_server_events());
    assert_eq!(handled_server_events(), before);
    assert_eq!(server.server_event_rx.len(), queued);
    assert_eq!(writer.take_transport_notifications(), (None, true));
}

#[test]
fn transport_queued_drain_is_not_replayed_as_fallback() {
    let _dirs = crate::config::test_dirs::isolate_dirs("notification-dedup");
    let mut server = test_headless_server();
    let writer = transport_test_writer(&mut server, 1);
    writer.test_notify_drained(1, &server.server_event_tx);
    let before = handled_server_events();
    server.drain_server_events();
    assert_eq!(handled_server_events() - before, 1);
    server.drain_server_events();
    assert_eq!(handled_server_events() - before, 1);
    writer.test_notify_drained(1, &server.server_event_tx);
    writer.test_notify_drained(1, &server.server_event_tx);
    assert_eq!(
        server.server_event_rx.len(),
        1,
        "coalesce drains until their handler acknowledges"
    );
    server.drain_server_events();
    assert_eq!(handled_server_events() - before, 2);
}

#[test]
fn transport_fallback_remains_fair_under_continuous_channel_and_client_pressure() {
    let _dirs = crate::config::test_dirs::isolate_dirs("notification-round-robin");
    let mut server = test_headless_server();
    let writers = (1..=96)
        .map(|id| (id, transport_test_writer(&mut server, id)))
        .collect::<Vec<_>>();
    let last = transport_test_writer(&mut server, 200);
    fill_server_event_channel(&server);
    last.test_notify_disconnect(200, &server.server_event_tx);
    for _ in 0..6 {
        fill_server_event_channel(&server);
        for (id, writer) in &writers {
            writer.test_notify_drained(*id, &server.server_event_tx);
        }
        let before = handled_server_events();
        server.drain_server_events();
        assert!(handled_server_events() - before <= EXTERNAL_EVENT_DRAIN_LIMIT);
        if !server.clients.contains_key(&200) {
            return;
        }
    }
    panic!("continuously refreshed low-id clients must not starve the last disconnect");
}

#[test]
fn transport_new_clients_cannot_starve_a_previous_round_disconnect() {
    let _dirs = crate::config::test_dirs::isolate_dirs("notification-new-clients");
    let mut server = test_headless_server();
    fill_server_event_channel(&server);
    for id in 1..=64 {
        transport_test_writer(&mut server, id).test_notify_drained(id, &server.server_event_tx);
    }
    server.drain_server_events();
    fill_server_event_channel(&server);
    let writer = server.clients[&1].writer.as_ref().unwrap().clone();
    writer.test_notify_disconnect(1, &server.server_event_tx);
    for round in 0..6 {
        fill_server_event_channel(&server);
        for id in (1000 + round * 64)..(1064 + round * 64) {
            transport_test_writer(&mut server, id)
                .test_notify_disconnect(id, &server.server_event_tx);
        }
        let before = handled_server_events();
        server.drain_server_events();
        assert!(handled_server_events() - before <= EXTERNAL_EVENT_DRAIN_LIMIT);
        if !server.clients.contains_key(&1) {
            return;
        }
    }
    panic!("new clients must not extend the current round indefinitely");
}

#[test]
fn transport_idle_clients_do_not_lock_notification_queues() {
    let _dirs = crate::config::test_dirs::isolate_dirs("notification-idle-scan");
    let mut server = test_headless_server();
    for id in 1..=512 {
        transport_test_writer(&mut server, id);
    }
    let before = ClientWriter::test_notification_take_count();
    let scanned = TEST_SCANNED_NOTIFICATION_CLIENTS.with(std::cell::Cell::get);
    server.drain_server_events();
    assert_eq!(
        TEST_SCANNED_NOTIFICATION_CLIENTS.with(std::cell::Cell::get) - scanned,
        512,
        "each client may be examined only once per batch"
    );
    assert_eq!(
        ClientWriter::test_notification_take_count(),
        before,
        "idle scans must not lock every client's writer queue"
    );
    assert!(!server.transport_notifications.more_pending);
}

#[test]
#[ignore = "manual external API burst scheduling profile"]
fn external_api_burst_profile() {
    for pane_count in [1, 15] {
        let mut server = test_headless_server();
        let mut workspace = crate::workspace::Workspace::test_new("api-profile");
        for _ in 1..pane_count {
            workspace.test_split(ratatui::layout::Direction::Horizontal);
        }
        server.app.state.workspaces = vec![workspace];
        server.app.state.ensure_test_terminals();
        for count in [64, 512, 4096] {
            let mut first_samples = Vec::new();
            let mut total_samples = Vec::new();
            let mut first_count = 0;
            for sample in 0..10 {
                let responses = queue_requests(&mut server, count);
                let start = Instant::now();
                server.drain_api_requests_with_shutdown_check();
                let first = start.elapsed();
                first_count = count - server.app.api_rx.len();
                while !server.app.api_rx.is_empty() {
                    server.drain_api_requests_with_shutdown_check();
                }
                let total = start.elapsed();
                assert_eq!(responses.try_iter().count(), count);
                if sample >= 3 {
                    first_samples.push(first);
                    total_samples.push(total);
                }
            }
            first_samples.sort_unstable();
            total_samples.sort_unstable();
            println!("api-burst panes={pane_count} requests={count} first_count={first_count} first_us={:.3} total_us={:.3}",
                first_samples[3].as_secs_f64() * 1e6, total_samples[3].as_secs_f64() * 1e6);
        }
    }
}
