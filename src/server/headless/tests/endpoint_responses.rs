use super::transport_lifecycle::EndpointTransport;
use super::*;
use crate::server::client_commands::{
    self, EndpointResponseKind, EndpointResponseReady, EndpointResponseTicket,
};
#[cfg(unix)]
use interprocess::TryClone as _;

fn exact_response_body(bytes: usize, request_id: &str, complex: bool) -> String {
    let mut envelope = serde_json::json!({
        "id": request_id,
        "result": { "type": "pane_link_activated", "url": "https://example.invalid/", "handled": true }
    });
    let room = bytes - envelope.to_string().len();
    let pattern = if complex { "é\\\"\n🦀x" } else { "x" };
    let encoded_pattern_bytes = serde_json::to_string(pattern).unwrap().len() - 2;
    let url = envelope["result"]["url"].as_str().unwrap().to_owned()
        + &pattern.repeat(room / encoded_pattern_bytes)
        + &"x".repeat(room % encoded_pattern_bytes);
    envelope["result"]["url"] = serde_json::Value::String(url);
    let response = envelope.to_string();
    assert_eq!(
        response.len(),
        bytes,
        "budget includes the complete correlated JSON envelope"
    );
    let _: api::schema::SuccessResponse = serde_json::from_str(&response).unwrap();
    if complex {
        let chunk = client_commands::ENDPOINT_RESPONSE_CHUNK_BYTES;
        assert!(
            (chunk..bytes)
                .step_by(chunk)
                .any(|index| response.as_bytes()[index] & 0xc0 == 0x80),
            "test data must split a UTF-8 code point across chunks"
        );
        assert!(
            (chunk..bytes)
                .step_by(chunk)
                .any(|index| response.as_bytes()[index - 1] == b'\\'),
            "test data must split a JSON escape across chunks"
        );
    }
    response
}

pub(super) fn command_ticket(
    server: &mut HeadlessServer,
    client_id: u64,
    request_id: &str,
    navigate: bool,
) -> EndpointResponseTicket {
    let revision = server.clients[&client_id].shell_projection_revision;
    let ticket = server
        .admit_endpoint_response(
            client_id,
            server.client_shell_boot_id.clone(),
            request_id.into(),
            EndpointResponseKind::Command {
                surface_revision: revision,
                navigate,
            },
        )
        .unwrap();
    let client = server.clients.get_mut(&client_id).unwrap();
    client.shell_endpoint_command_in_flight = true;
    client.shell_endpoint_command_surface_revision = Some(revision);
    if navigate {
        let internal_id = format!(
            "endpoint:{}:{client_id}:{request_id}",
            server.client_shell_boot_id
        );
        client.shell_deferred_navigation_request_id = Some(internal_id.clone());
        ticket
            .identity
            .deferred_request_id
            .set(internal_id.clone())
            .unwrap();
        client
            .endpoint_responses
            .slots
            .back_mut()
            .unwrap()
            .deferred_request_id = Some(internal_id);
    }
    ticket
}

pub(super) fn response_event(
    ticket: EndpointResponseTicket,
    value: &serde_json::Value,
) -> ServerEvent {
    ServerEvent::EndpointResponseReady {
        response: EndpointResponseReady::new(ticket, value.to_string()),
    }
}

fn response_turn(server: &mut HeadlessServer, writer: &ClientWriter) -> usize {
    let before = writer.test_endpoint_frames_sent();
    server.endpoint_pump_remaining = client_commands::ENDPOINT_BLOCKS_PER_TURN;
    server.drain_server_events();
    let sent = writer.test_endpoint_frames_sent() - before;
    assert!(
        sent <= client_commands::ENDPOINT_BLOCKS_PER_TURN,
        "one turn enqueued {sent} bulk frames"
    );
    sent
}

async fn real_pipe_response_boundary(size: usize, complex: bool) {
    let oversized = size > client_commands::MAX_ENDPOINT_RESPONSE_BYTES;
    let _dirs = crate::config::test_dirs::isolate_dirs("endpoint-response-boundary");
    let mut server = test_headless_server();
    let _input = install_focused_test_runtime(&mut server, b"");
    let (connection, connected) = EndpointTransport::new(&mut server).await;
    let ServerEvent::ClientShellConnected { client_id, .. } = &connected else {
        panic!("endpoint handshake");
    };
    let client_id = *client_id;
    server.handle_server_event_with_render_impact(connected);
    let writer = server.clients[&client_id].writer.as_ref().unwrap().clone();
    let ticket = command_ticket(&mut server, client_id, "bulk", false);
    let correlated = exact_response_body(size, "bulk", complex);
    let mut source: serde_json::Value = serde_json::from_str(&correlated).unwrap();
    source["id"] = serde_json::json!("endpoint:internal-\"escaped\\id\":701:bulk");
    let (response_tx, response_rx) = std::sync::mpsc::channel();
    client_commands::spawn_response_waiter(ticket, response_rx, server.server_event_tx.clone())
        .unwrap();
    response_tx.send(source.to_string()).unwrap();
    drop(source);
    let ready = tokio::time::timeout(LOADED_WAIT, server.server_event_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let ServerEvent::EndpointResponseReady { response } = &ready else {
        panic!("one complete Ready event");
    };
    let expected = if oversized {
        assert!(response.body.len() < client_commands::ENDPOINT_RESPONSE_CHUNK_BYTES);
        let error: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
        assert_eq!(error["id"], "bulk");
        assert_eq!(error["error"]["code"], "endpoint_response_too_large");
        assert!(
            error.get("result").is_none(),
            "no successful JSON prefix may escape"
        );
        response.body.to_vec()
    } else {
        assert_eq!(response.body.len(), size);
        assert!(
            response.body.as_ref() == correlated.as_bytes(),
            "correlation must preserve the exact boundary body"
        );
        correlated.as_bytes().to_vec()
    };
    drop(correlated);
    let before = writer.test_endpoint_frames_sent();
    server.handle_server_event_with_render_impact(ready);
    assert_eq!(writer.test_endpoint_frames_sent() - before, 1);
    let mut max_turn_frames = 1;
    let mut max_queue_bytes = 0;
    for _ in 0..24 {
        max_turn_frames = max_turn_frames.max(response_turn(&mut server, &writer));
        let (bytes, messages) = writer.test_control_backlog();
        max_queue_bytes = max_queue_bytes.max(bytes);
        assert!(bytes <= 4 * 1024 * 1024 && messages <= 4096);
        assert!(bytes <= 4 * 1024 * 1024 - (MAX_FRAME_SIZE + 4));
        assert!(
            messages <= 4096 - 64,
            "ordinary-control reserve must remain available"
        );
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !writer.was_aborted(),
        "a valid response must wait for credit, not abort its reader and writer"
    );
    assert!(server.clients.contains_key(&client_id));
    if !oversized {
        assert!(server.clients[&client_id].endpoint_responses.slots[0].offset < expected.len());
    } else {
        assert!(server.clients[&client_id]
            .endpoint_responses
            .slots
            .is_empty());
    }

    let mut peer = connection.peer.try_clone().unwrap();
    let boot = server.client_shell_boot_id.clone();
    let (finished, result) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        let read = (|| -> Result<(Vec<u8>, usize, usize), String> {
            let mut assembler =
                crate::client::EndpointResponseTestAssembler::new(&boot, &[("bulk", 0)]);
            let mut body = Vec::new();
            let mut chunks = 0;
            let mut finals = 0;
            loop {
                let message: ServerMessage = protocol::read_message(&mut peer, MAX_FRAME_SIZE)
                    .map_err(|error| error.to_string())?;
                let ServerMessage::ClientShellEndpointResponseChunk {
                    boot_id,
                    request_id,
                    final_chunk,
                    data,
                } = message
                else {
                    continue;
                };
                assert_eq!(boot_id, boot);
                assert_eq!(request_id, "bulk");
                chunks += 1;
                if oversized {
                    assert!(
                        final_chunk && chunks == 1,
                        "oversized response must be rejected before any success prefix"
                    );
                    let value: serde_json::Value = serde_json::from_slice(&data).unwrap();
                    assert_eq!(value["error"]["code"], "endpoint_response_too_large");
                    assert!(value.get("result").is_none());
                }
                body.extend_from_slice(&data);
                let completed = assembler
                    .receive(&boot_id, &request_id, final_chunk, data)
                    .map_err(|error| error.to_string())?;
                if final_chunk {
                    finals += 1;
                    if oversized {
                        assert!(
                            matches!(completed, Some(Err(code)) if code == "endpoint_response_too_large")
                        );
                    } else {
                        assert!(matches!(
                            completed,
                            Some(Ok(api::schema::ResponseResult::PaneLinkActivated {
                                handled: true,
                                url: Some(_)
                            }))
                        ));
                    }
                    break;
                }
                assert!(completed.is_none());
            }
            assembler.start(&boot, "after", 0);
            let request = api::schema::Request {
                id: "after".into(),
                method: api::schema::Method::ClientShellSurfaceSet(
                    api::schema::ClientShellSurfaceSetParams { active: true },
                ),
            };
            protocol::write_message(
                &mut peer,
                &protocol::ClientMessage::ClientShellEndpointRequest {
                    boot_id: boot.clone(),
                    request: serde_json::to_string(&request).unwrap(),
                },
            )
            .map_err(|error| error.to_string())?;
            loop {
                let message: ServerMessage = protocol::read_message(&mut peer, MAX_FRAME_SIZE)
                    .map_err(|error| error.to_string())?;
                let ServerMessage::ClientShellEndpointResponseChunk {
                    boot_id,
                    request_id,
                    final_chunk,
                    data,
                } = message
                else {
                    continue;
                };
                assert_eq!(boot_id, boot);
                assert_eq!(request_id, "after", "bulk must not emit another final");
                assert!(final_chunk);
                let completed = assembler
                    .receive(&boot_id, &request_id, final_chunk, data)
                    .map_err(|error| error.to_string())?;
                assert!(matches!(
                    completed,
                    Some(Ok(api::schema::ResponseResult::ClientShellSurfaceSet {
                        active: true,
                        ..
                    }))
                ));
                break;
            }
            Ok((body, chunks, finals))
        })();
        let _ = finished.send(read);
    });
    let received = tokio::time::timeout(LOADED_WAIT, async {
        loop {
            max_turn_frames = max_turn_frames.max(response_turn(&mut server, &writer));
            let (bytes, messages) = writer.test_control_backlog();
            max_queue_bytes = max_queue_bytes.max(bytes);
            assert!(bytes <= 4 * 1024 * 1024 && messages <= 4096);
            match result.try_recv() {
                Ok(result) => break result,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    panic!("pipe reader ended without a result")
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    tokio::time::sleep(Duration::from_millis(1)).await
                }
            }
        }
    })
    .await;
    let aborted_before_cleanup = writer.was_aborted();
    connection.observer.abort();
    reader.join().unwrap();
    let (body, chunks, finals) = received
        .expect("bounded response drain")
        .expect("real pipe response");
    assert!(!aborted_before_cleanup);
    assert!(
        body == expected,
        "every received byte must match the correlated response"
    );
    let expected_chunks = if oversized {
        1
    } else {
        size.div_ceil(client_commands::ENDPOINT_RESPONSE_CHUNK_BYTES)
    };
    assert_eq!((chunks, finals), (expected_chunks, 1));
    assert_eq!(
        server.endpoint_response_budget.available_permits(),
        client_commands::MAX_SERVER_RESPONSES
    );
    println!("component-pipe correlated_bytes={size} delivered_bytes={} chunks={chunks} finals={finals} max_turn_frames={max_turn_frames} max_queue_bytes={max_queue_bytes} permits_returned={} small_request=PASS", body.len(), server.endpoint_response_budget.available_permits());
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn endpoint_response_eight_mib_does_not_abort_a_paused_real_pipe() {
    real_pipe_response_boundary(8 * 1024 * 1024, false).await;
}

#[tokio::test]
async fn endpoint_response_exact_sixty_four_mib_preserves_utf8_and_escapes() {
    real_pipe_response_boundary(64 * 1024 * 1024, true).await;
}

#[tokio::test]
async fn endpoint_response_sixty_four_mib_plus_one_is_only_a_small_error() {
    real_pipe_response_boundary(64 * 1024 * 1024 + 1, true).await;
}

fn paused_client(server: &mut HeadlessServer, client_id: u64) -> ClientWriter {
    let writer = ClientWriter::test_paused();
    server.clients.insert(
        client_id,
        ClientConnection::new(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            1,
            RenderEncoding::SemanticFrame,
            Some(writer.clone()),
        ),
    );
    writer
}

fn admit_reading(server: &mut HeadlessServer, client_id: u64, id: &str) -> EndpointResponseTicket {
    server
        .admit_endpoint_response(
            client_id,
            server.client_shell_boot_id.clone(),
            id.into(),
            EndpointResponseKind::Reading,
        )
        .unwrap()
}

#[test]
fn response_fairness_full_channel_credit_resumes_without_input_or_render_theft() {
    let _dirs = crate::config::test_dirs::isolate_dirs("response-credit-resume");
    for render_credit in [false, true] {
        let mut server = test_headless_server();
        let writer = paused_client(&mut server, 1);
        let chunk = client_commands::ENDPOINT_RESPONSE_CHUNK_BYTES;
        let expected = exact_response_body(6 * chunk + 257, "resume", false);
        let ticket = admit_reading(&mut server, 1, "resume");
        server.accept_endpoint_response(EndpointResponseReady::new(ticket, expected.clone()));
        for _ in 0..4 {
            response_turn(&mut server, &writer);
        }
        assert_eq!(writer.test_endpoint_frames_sent(), 3);
        assert_eq!(
            server.clients[&1].endpoint_responses.slots[0].offset,
            3 * chunk
        );
        assert_eq!(response_turn(&mut server, &writer), 0);
        for _ in 0..EXTERNAL_EVENT_DRAIN_LIMIT {
            server
                .server_event_tx
                .try_send(ServerEvent::QuitSignal)
                .unwrap();
        }
        assert_eq!(server.server_event_rx.len(), EXTERNAL_EVENT_DRAIN_LIMIT);
        server.clients.get_mut(&1).unwrap().defer_full_render();
        let mut frames = vec![writer.test_dequeue_control(1, &server.server_event_tx)];
        if render_credit {
            writer.test_notify_drained(1, &server.server_event_tx);
            writer.test_notify_drained(1, &server.server_event_tx);
        }
        assert_eq!(server.server_event_rx.len(), EXTERNAL_EVENT_DRAIN_LIMIT);
        assert_eq!(response_turn(&mut server, &writer), 1);
        assert_eq!(
            server.clients[&1].deferred_render(),
            if render_credit {
                DeferredRender::None
            } else {
                DeferredRender::Full
            },
            "endpoint-only credit must not take the deferred render"
        );
        for _ in 0..10 {
            while writer.test_control_backlog().1 != 0 {
                frames.push(writer.test_dequeue_control(1, &server.server_event_tx));
            }
            response_turn(&mut server, &writer);
            if server.clients[&1].endpoint_responses.slots.is_empty()
                && writer.test_control_backlog().1 == 0
            {
                break;
            }
        }
        let boot = server.client_shell_boot_id.clone();
        let mut assembler =
            crate::client::EndpointResponseTestAssembler::new(&boot, &[("resume", 2)]);
        let mut body = Vec::new();
        let mut finals = 0;
        let chunks = frames.len();
        for frame in frames {
            let ServerMessage::ClientShellEndpointResponseChunk {
                boot_id,
                request_id,
                final_chunk,
                data,
            } = read_server_message(frame)
            else {
                panic!("only endpoint response frames expected");
            };
            assert_eq!(boot_id, boot);
            assert_eq!(request_id, "resume");
            assert_eq!(finals, 0);
            body.extend_from_slice(&data);
            let completed = assembler
                .receive(&boot_id, &request_id, final_chunk, data)
                .unwrap();
            if final_chunk {
                finals += 1;
                assert!(matches!(
                    completed,
                    Some(Ok(api::schema::ResponseResult::PaneLinkActivated {
                        handled: true,
                        ..
                    }))
                ));
            } else {
                assert!(completed.is_none());
            }
        }
        assert_eq!(body, expected.as_bytes());
        assert_eq!((chunks, finals), (7, 1));
        assert_eq!(server.endpoint_response_budget.available_permits(), 6);
        assert_eq!(response_turn(&mut server, &writer), 0);
        assert_eq!(response_turn(&mut server, &writer), 0);
        assert_eq!(
            server.clients[&1].deferred_render(),
            if render_credit {
                DeferredRender::None
            } else {
                DeferredRender::Full
            }
        );
        writer.test_notify_drained(1, &server.server_event_tx);
        assert_eq!(response_turn(&mut server, &writer), 0);
        assert_eq!(server.clients[&1].deferred_render(), DeferredRender::None);
        assert!(writer.test_drain().is_empty());
        assert!(!writer.was_aborted());
        println!("response-fairness credit_resume merged_render={render_credit} blocked_at_chunks=3 chunks={chunks} finals={finals} new_input=0 permits_returned={}", server.endpoint_response_budget.available_permits());
    }
}

#[test]
fn response_fairness_three_ready_lanes_finish_small_bodies_before_command() {
    let _dirs = crate::config::test_dirs::isolate_dirs("response-three-lanes");
    let mut server = test_headless_server();
    let writer = paused_client(&mut server, 1);
    let chunk = client_commands::ENDPOINT_RESPONSE_CHUNK_BYTES;
    let ids = ["command", "background", "reading"];
    let sizes = [9 * chunk + 257, 2 * chunk + 257, 4097];
    let expected = ids
        .iter()
        .zip(sizes)
        .map(|(id, size)| {
            let mut body = exact_response_body(size, id, false).into_bytes();
            for (index, offset) in (0..size).step_by(chunk).enumerate() {
                body[offset + 128] = b'A' + index as u8;
            }
            String::from_utf8(body).unwrap()
        })
        .collect::<Vec<_>>();
    let tickets = [
        command_ticket(&mut server, 1, ids[0], false),
        server
            .admit_endpoint_response(
                1,
                server.client_shell_boot_id.clone(),
                ids[1].into(),
                EndpointResponseKind::Background,
            )
            .unwrap(),
        admit_reading(&mut server, 1, ids[2]),
    ];
    server.endpoint_pump_remaining = 0;
    for (ticket, body) in tickets.into_iter().zip(&expected) {
        server.accept_endpoint_response(EndpointResponseReady::new(ticket, body.clone()));
    }
    assert_eq!(writer.test_endpoint_frames_sent(), 0);
    assert!(server.clients[&1]
        .endpoint_responses
        .slots
        .iter()
        .all(|slot| slot.ready.is_some() && slot.offset == 0));
    let boot = server.client_shell_boot_id.clone();
    let mut assembler = crate::client::EndpointResponseTestAssembler::new(
        &boot,
        &[(ids[0], 0), (ids[1], 1), (ids[2], 2)],
    );
    let mut bodies = [Vec::new(), Vec::new(), Vec::new()];
    let mut chunks = [0; 3];
    let mut finals = [0; 3];
    let mut order = Vec::new();
    let mut final_order = Vec::new();
    let mut max_turn_frames = 0;
    for turn in 1..=20 {
        max_turn_frames = max_turn_frames.max(response_turn(&mut server, &writer));
        for frame in writer.test_drain() {
            let ServerMessage::ClientShellEndpointResponseChunk {
                boot_id,
                request_id,
                final_chunk,
                data,
            } = read_server_message(frame)
            else {
                panic!("only endpoint response frames expected");
            };
            assert_eq!(boot_id, boot);
            let index = ids.iter().position(|id| *id == request_id).unwrap();
            assert_eq!(finals[index], 0, "no frames after a response final");
            assert!(!data.is_empty() && data.len() <= chunk);
            let offset = bodies[index].len();
            assert_eq!(
                data.as_slice(),
                &expected[index].as_bytes()[offset..(offset + chunk).min(sizes[index])],
                "request {request_id} preserves frame order at offset {offset}"
            );
            chunks[index] += 1;
            bodies[index].extend_from_slice(&data);
            order.push(request_id.clone());
            let completed = assembler
                .receive(&boot_id, &request_id, final_chunk, data)
                .unwrap();
            if final_chunk {
                finals[index] += 1;
                final_order.push((request_id, turn));
                assert_eq!(bodies[index].len(), sizes[index]);
                assert!(matches!(
                    completed,
                    Some(Ok(api::schema::ResponseResult::PaneLinkActivated {
                        handled: true,
                        ..
                    }))
                ));
                if index != 0 {
                    assert_eq!(finals[0], 0);
                    assert!(bodies[0].len() < sizes[0]);
                    assert!(server.clients[&1].shell_endpoint_command_in_flight);
                }
            } else {
                assert!(completed.is_none());
            }
        }
        if finals == [1; 3] {
            break;
        }
    }
    assert_eq!(&order[..3], &ids);
    assert_eq!(finals, [1; 3], "all lanes finish within twenty turns");
    assert_eq!(chunks, [10, 3, 1]);
    assert_eq!(
        final_order
            .iter()
            .map(|(id, _)| id.as_str())
            .collect::<Vec<_>>(),
        ["reading", "background", "command"]
    );
    for (actual, expected) in bodies.iter().zip(&expected) {
        assert_eq!(actual.as_slice(), expected.as_bytes());
    }
    assert!(!server.clients[&1].shell_endpoint_command_in_flight);
    assert!(server.clients[&1].endpoint_responses.slots.is_empty());
    assert_eq!(server.endpoint_response_budget.available_permits(), 6);
    assert_eq!(response_turn(&mut server, &writer), 0);
    assert!(writer.test_drain().is_empty());
    println!("response-fairness three_lanes chunks={chunks:?} finals={finals:?} final_order={final_order:?} max_turn_frames={max_turn_frames}");
}

struct FairnessResponse {
    request_id: String,
    expected: String,
    body: Vec<u8>,
    chunks: usize,
    finals: usize,
    assembler: crate::client::EndpointResponseTestAssembler,
}

impl FairnessResponse {
    fn new(boot: &str, request_id: &str, bytes: usize) -> Self {
        Self {
            request_id: request_id.into(),
            expected: exact_response_body(bytes, request_id, false),
            body: Vec::new(),
            chunks: 0,
            finals: 0,
            assembler: crate::client::EndpointResponseTestAssembler::new(boot, &[(request_id, 2)]),
        }
    }

    fn receive(&mut self, boot: &str, message: ServerMessage) {
        let ServerMessage::ClientShellEndpointResponseChunk {
            boot_id,
            request_id,
            final_chunk,
            data,
        } = message
        else {
            panic!("expected an endpoint response frame");
        };
        assert_eq!(boot_id, boot);
        assert_eq!(request_id, self.request_id);
        assert_eq!(self.finals, 0, "only one final is allowed");
        let end = (self.body.len() + client_commands::ENDPOINT_RESPONSE_CHUNK_BYTES)
            .min(self.expected.len());
        assert_eq!(
            data.as_slice(),
            &self.expected.as_bytes()[self.body.len()..end]
        );
        self.body.extend_from_slice(&data);
        self.chunks += 1;
        let completed = self
            .assembler
            .receive(&boot_id, &request_id, final_chunk, data)
            .unwrap();
        if final_chunk {
            self.finals += 1;
            assert_eq!(self.body, self.expected.as_bytes());
            assert!(matches!(
                completed,
                Some(Ok(api::schema::ResponseResult::PaneLinkActivated {
                    handled: true,
                    ..
                }))
            ));
        } else {
            assert!(completed.is_none());
        }
    }
}

#[tokio::test]
async fn response_fairness_competing_clients_bound_bulk_and_preserve_real_input() {
    let _dirs = crate::config::test_dirs::isolate_dirs("response-competing-clients");
    let mut server = test_headless_server();
    let mut input = install_focused_test_runtime(&mut server, b"");
    let pane_id = server.app.session_snapshot().focused_pane_id.unwrap();
    let (first, connected) = EndpointTransport::with_id(&mut server, 701).await;
    server.handle_server_event_with_render_impact(connected);
    let (mut second, connected) = EndpointTransport::with_id(&mut server, 702).await;
    server.handle_server_event_with_render_impact(connected);
    assert!(server.server_event_rx.is_empty());
    let client_ids = [701, 101, 102, 103, 104, 702];
    let writers = client_ids
        .iter()
        .map(|id| {
            if matches!(id, 701 | 702) {
                server.clients[id].writer.as_ref().unwrap().clone()
            } else {
                paused_client(&mut server, *id)
            }
        })
        .collect::<Vec<_>>();
    let pressure = (1..=96)
        .map(|id| {
            let writer = paused_client(&mut server, id);
            server.clients.get_mut(&id).unwrap().mode = ClientConnectionMode::TerminalPending;
            (id, writer)
        })
        .collect::<Vec<_>>();
    let boot = server.client_shell_boot_id.clone();
    let chunk = client_commands::ENDPOINT_RESPONSE_CHUNK_BYTES;
    let sizes = [
        16 * chunk + 257,
        6 * chunk + 257,
        6 * chunk + 257,
        6 * chunk + 257,
        6 * chunk + 257,
        4097,
    ];
    let mut responses = client_ids
        .iter()
        .zip(sizes)
        .map(|(id, size)| FairnessResponse::new(&boot, &format!("client-{id}"), size))
        .collect::<Vec<_>>();
    server.endpoint_pump_remaining = 0;
    for (id, response) in client_ids.iter().zip(&responses) {
        let ticket = admit_reading(&mut server, *id, &response.request_id);
        server.accept_endpoint_response(EndpointResponseReady::new(
            ticket,
            response.expected.clone(),
        ));
    }
    assert_eq!(server.endpoint_response_budget.available_permits(), 0);
    assert!(writers
        .iter()
        .all(|writer| writer.test_endpoint_frames_sent() == 0));
    let (received_tx, received) = std::sync::mpsc::channel();
    let readers = [(0, &first), (5, &second)]
        .into_iter()
        .map(|(index, connection)| {
            let mut peer = connection.peer.try_clone().unwrap();
            let received_tx = received_tx.clone();
            std::thread::spawn(move || loop {
                match protocol::read_message::<_, ServerMessage>(&mut peer, MAX_FRAME_SIZE) {
                    Ok(message @ ServerMessage::ClientShellEndpointResponseChunk { .. }) => {
                        let done = matches!(
                            &message,
                            ServerMessage::ClientShellEndpointResponseChunk {
                                final_chunk: true,
                                ..
                            }
                        );
                        if received_tx.send(Ok((index, message))).is_err() || done {
                            break;
                        }
                    }
                    Ok(_) => {}
                    Err(error) => {
                        let _ = received_tx.send(Err(error.to_string()));
                        break;
                    }
                }
            })
        })
        .collect::<Vec<_>>();
    drop(received_tx);
    for _ in 0..EXTERNAL_EVENT_DRAIN_LIMIT - 1 {
        server
            .server_event_tx
            .try_send(ServerEvent::QuitSignal)
            .unwrap();
    }
    protocol::write_message(
        &mut second.peer,
        &protocol::ClientMessage::ClientShellPaneInput {
            pane_id,
            events: vec![protocol::ClientPaneInputEvent::TextCommit(
                "fair-input".into(),
            )],
        },
    )
    .unwrap();
    tokio::time::timeout(LOADED_WAIT, async {
        while server.server_event_rx.len() != EXTERNAL_EVENT_DRAIN_LIMIT {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("real input admitted behind 63 channel events");
    let mut first_frame_turns = [0; 6];
    let mut turn = 0;
    let mut max_turn_frames = 0;
    let mut max_turn_events = 0;
    let mut max_fallback = 0;
    let drained = tokio::time::timeout(LOADED_WAIT, async {
        loop {
            turn += 1;
            while server
                .server_event_tx
                .try_send(ServerEvent::QuitSignal)
                .is_ok()
            {}
            assert_eq!(server.server_event_rx.len(), EXTERNAL_EVENT_DRAIN_LIMIT);
            for (id, writer) in &pressure {
                writer.test_notify_drained(*id, &server.server_event_tx);
            }
            let before = writers
                .iter()
                .map(ClientWriter::test_endpoint_frames_sent)
                .sum::<usize>();
            let events_before = TEST_HANDLED_SERVER_EVENTS.with(std::cell::Cell::get);
            let fallback_before = ClientWriter::test_notification_take_count();
            server.endpoint_pump_remaining = client_commands::ENDPOINT_BLOCKS_PER_TURN;
            server.drain_server_events();
            let after = writers
                .iter()
                .map(ClientWriter::test_endpoint_frames_sent)
                .sum::<usize>();
            let sent = after - before;
            let handled = TEST_HANDLED_SERVER_EVENTS.with(std::cell::Cell::get) - events_before;
            let fallback = ClientWriter::test_notification_take_count() - fallback_before;
            assert!(
                sent <= client_commands::ENDPOINT_BLOCKS_PER_TURN,
                "all six connections share the four-frame budget"
            );
            assert_eq!(handled, EXTERNAL_EVENT_DRAIN_LIMIT);
            assert_eq!(fallback, EXTERNAL_EVENT_DRAIN_LIMIT / 2);
            assert!(server.server_event_rx.len() >= EXTERNAL_EVENT_DRAIN_LIMIT / 2);
            max_turn_frames = max_turn_frames.max(sent);
            max_turn_events = max_turn_events.max(handled);
            max_fallback = max_fallback.max(fallback);
            for (index, writer) in writers.iter().enumerate() {
                if first_frame_turns[index] == 0 && writer.test_endpoint_frames_sent() != 0 {
                    first_frame_turns[index] = turn;
                }
                assert!(!writer.was_aborted());
                let (bytes, messages) = writer.test_control_backlog();
                assert!(bytes <= 4 * 1024 * 1024 && messages <= 4096);
            }
            for index in 1..5 {
                while writers[index].test_control_backlog().1 != 0 {
                    let frame = writers[index]
                        .test_dequeue_control(client_ids[index], &server.server_event_tx);
                    responses[index].receive(&boot, read_server_message(frame));
                }
            }
            while let Ok(message) = received.try_recv() {
                let (index, message) = message.expect("real endpoint pipe remains open");
                responses[index].receive(&boot, message);
            }
            if turn == 2 {
                assert!(
                    first_frame_turns.iter().all(|turn| (1..=2).contains(turn)),
                    "all six pending clients get a frame within two turns: {first_frame_turns:?}"
                );
                let accepted = tokio::time::timeout(LOADED_WAIT, input.recv())
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(accepted.as_ref(), b"fair-input");
                while responses[5].finals == 0 {
                    match received.try_recv() {
                        Ok(message) => {
                            let (index, message) = message.expect("small real response");
                            responses[index].receive(&boot, message);
                        }
                        Err(std::sync::mpsc::TryRecvError::Empty) => {
                            tokio::time::sleep(Duration::from_millis(1)).await
                        }
                        Err(error) => panic!("small response reader stopped: {error}"),
                    }
                }
                assert_eq!(responses[0].finals, 0);
                assert!(server.clients[&701].endpoint_responses.has_ready());
                assert_eq!(responses[5].finals, 1);
            }
            if responses.iter().all(|response| response.finals == 1) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await;
    let aborted_before_cleanup = writers.iter().any(ClientWriter::was_aborted);
    first.observer.abort();
    second.observer.abort();
    for reader in readers {
        reader.join().unwrap();
    }
    drained.expect("every response finishes under continued channel and fallback pressure");
    assert!(!aborted_before_cleanup);
    assert_eq!(max_turn_frames, client_commands::ENDPOINT_BLOCKS_PER_TURN);
    for (index, response) in responses.iter().enumerate() {
        assert_eq!(response.body, response.expected.as_bytes());
        assert_eq!(response.finals, 1);
        assert_eq!(response.chunks, sizes[index].div_ceil(chunk));
        assert_eq!(writers[index].test_endpoint_frames_sent(), response.chunks);
        assert!(server.clients[&client_ids[index]]
            .endpoint_responses
            .slots
            .is_empty());
    }
    assert_eq!(server.endpoint_response_budget.available_permits(), 6);
    assert!(input.try_recv().is_err());
    println!("response-fairness clients=6 real_pipes=2 initial_channel=64 renewed_fallback=96 first_frame_turns={first_frame_turns:?} input_turn=2 small_final_by_turn=2 total_turns={turn} max_turn_frames={max_turn_frames} max_turn_events={max_turn_events} max_fallback={max_fallback} chunks={:?} finals={:?}", responses.iter().map(|response| response.chunks).collect::<Vec<_>>(), responses.iter().map(|response| response.finals).collect::<Vec<_>>());
    shutdown_test_runtimes(&mut server);
}

fn take_response(writer: &ClientWriter, expected_id: &str) -> serde_json::Value {
    let frames = writer.test_drain();
    let mut responses = frames.into_iter().filter_map(|frame| {
        let message: ServerMessage =
            protocol::read_message(&mut frame.as_slice(), MAX_FRAME_SIZE).unwrap();
        if let ServerMessage::ClientShellEndpointResponseChunk {
            request_id,
            final_chunk,
            data,
            ..
        } = message
        {
            assert_eq!(request_id, expected_id);
            assert!(final_chunk);
            Some(serde_json::from_slice::<serde_json::Value>(&data).unwrap())
        } else {
            None
        }
    });
    let response = responses.next().expect("correlated response");
    assert!(responses.next().is_none(), "one final per request");
    response
}

fn rename_request(server: &HeadlessServer, id: &str) -> Box<api::schema::Request> {
    Box::new(api::schema::Request {
        id: id.into(),
        method: api::schema::Method::WorkspaceRename(api::schema::WorkspaceRenameParams {
            workspace_id: server.app.state.workspaces[0].id.clone(),
            label: "must-not-run".into(),
        }),
    })
}

#[tokio::test]
async fn response_matrix_bulk_limits_reject_before_mutation() {
    let _dirs = crate::config::test_dirs::isolate_dirs("response-admission");
    let mut server = test_headless_server();
    let _input = install_focused_test_runtime(&mut server, b"");
    let first = paused_client(&mut server, 1);
    paused_client(&mut server, 2);
    let third = paused_client(&mut server, 3);
    let original = server.app.state.workspaces[0].custom_name.clone();
    let mut tickets = (0..3)
        .map(|index| admit_reading(&mut server, 1, &format!("first-{index}")))
        .collect::<Vec<_>>();
    assert_eq!(server.endpoint_response_budget.available_permits(), 3);
    let request = rename_request(&server, "fourth");
    server.handle_client_shell_endpoint_request(1, server.client_shell_boot_id.clone(), request);
    assert_eq!(
        take_response(&first, "fourth")["error"]["code"],
        "endpoint_busy"
    );
    assert_eq!(server.app.state.workspaces[0].custom_name, original);
    assert_eq!(server.clients[&1].endpoint_responses.slots.len(), 3);
    for index in 0..3 {
        tickets.push(admit_reading(&mut server, 2, &format!("second-{index}")));
    }
    assert_eq!(server.endpoint_response_budget.available_permits(), 0);
    let request = rename_request(&server, "seventh");
    server.handle_client_shell_endpoint_request(3, server.client_shell_boot_id.clone(), request);
    assert_eq!(
        take_response(&third, "seventh")["error"]["code"],
        "endpoint_busy"
    );
    assert_eq!(server.app.state.workspaces[0].custom_name, original);
    assert!(server.clients[&3].endpoint_responses.slots.is_empty());
    drop(tickets);
    assert_eq!(server.endpoint_response_budget.available_permits(), 6);
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn response_matrix_duplicate_ids_do_not_execute_or_emit_fake_finals() {
    let _dirs = crate::config::test_dirs::isolate_dirs("response-duplicate");
    let mut server = test_headless_server();
    let _input = install_focused_test_runtime(&mut server, b"");
    let writer = paused_client(&mut server, 1);
    let ticket = admit_reading(&mut server, 1, "active");
    let original = server.app.state.workspaces[0].custom_name.clone();
    let request = rename_request(&server, "active");
    server.handle_client_shell_endpoint_request(1, server.client_shell_boot_id.clone(), request);
    for method in [
        api::schema::Method::Ping(api::schema::PingParams::default()),
        api::schema::Method::ClientShellSurfaceSet(api::schema::ClientShellSurfaceSetParams {
            active: false,
        }),
    ] {
        server.handle_client_shell_endpoint_request(
            1,
            server.client_shell_boot_id.clone(),
            Box::new(api::schema::Request {
                id: "active".into(),
                method,
            }),
        );
    }
    for code in ["invalid_request", "unsupported_method"] {
        server.handle_server_event(ServerEvent::ClientShellEndpointRequestError {
            client_id: 1,
            boot_id: server.client_shell_boot_id.clone(),
            request_id: "active".into(),
            code,
            message: "duplicate invalid envelope".into(),
        });
    }
    assert!(writer.test_drain().is_empty());
    assert_eq!(server.app.state.workspaces[0].custom_name, original);
    assert!(server.clients[&1].shell_surface_active);
    assert_eq!(server.clients[&1].endpoint_responses.slots.len(), 1);
    assert_eq!(server.endpoint_response_budget.available_permits(), 5);
    assert!(ticket.identity.active());
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn response_matrix_control_identity_is_bounded_and_not_bulk_starved() {
    use crate::server::client_commands::EndpointResponseSender;
    let _dirs = crate::config::test_dirs::isolate_dirs("response-control-identity");
    let mut server = test_headless_server();
    let _input = install_focused_test_runtime(&mut server, b"");
    let writer = paused_client(&mut server, 1);
    let tickets = (0..3)
        .map(|index| admit_reading(&mut server, 1, &format!("bulk-{index}")))
        .collect::<Vec<_>>();
    let boot = server.client_shell_boot_id.clone();
    let target = server
        .clients
        .get_mut(&1)
        .unwrap()
        .endpoint_responses
        .admit_control(
            1,
            boot.clone(),
            "unsubscribe".into(),
            writer.control.clone(),
        )
        .unwrap();
    let reply = EndpointResponseSender::new(target);
    assert!(server
        .clients
        .get_mut(&1)
        .unwrap()
        .endpoint_responses
        .admit_control(1, boot.clone(), "second".into(), writer.control.clone())
        .is_none());
    let request = rename_request(&server, "unsubscribe");
    server.handle_client_shell_endpoint_request(1, boot.clone(), request);
    assert!(
        writer.test_drain().is_empty(),
        "control ids share duplicate suppression with bulk"
    );
    let params = api::schema::ObservationSubscriptionParams {
        subscription_id: Some("sub".into()),
        ..Default::default()
    };
    for method in [
        api::schema::Method::SystemMetricsUnsubscribe(params.clone()),
        api::schema::Method::AccountUsageUnsubscribe(params),
    ] {
        server.handle_client_shell_endpoint_request(
            1,
            boot.clone(),
            Box::new(api::schema::Request {
                id: "second".into(),
                method,
            }),
        );
        assert_eq!(
            take_response(&writer, "second")["error"]["code"],
            "endpoint_busy"
        );
    }
    assert!(
        server.observability.is_none(),
        "rejected control cannot create a background runtime"
    );
    server.handle_client_shell_endpoint_request(
        1,
        boot.clone(),
        Box::new(api::schema::Request {
            id: "off".into(),
            method: api::schema::Method::ClientShellSurfaceSet(
                api::schema::ClientShellSurfaceSetParams { active: false },
            ),
        }),
    );
    assert_eq!(take_response(&writer, "off")["result"]["active"], false);
    assert_eq!(server.endpoint_response_budget.available_permits(), 3);
    while server
        .server_event_tx
        .try_send(ServerEvent::QuitSignal)
        .is_ok()
    {}
    let queued = server.server_event_rx.len();
    let subscription_clone = reply.clone();
    reply.take().unwrap().send(
        client_commands::response_text(
            "unsubscribe",
            Ok(api::schema::ResponseResult::ObservationSubscription {
                subscription_id: "sub".into(),
                active: false,
            }),
        ),
        &server.server_event_tx,
    );
    assert_eq!(
        take_response(&writer, "unsubscribe")["result"]["active"],
        false
    );
    assert_eq!(
        server.server_event_rx.len(),
        queued,
        "small control never spawns a waiting event future"
    );
    assert!(subscription_clone.take().is_none());
    assert!(!server.clients[&1]
        .endpoint_responses
        .contains(&boot, "unsubscribe"));
    assert_eq!(server.endpoint_response_budget.available_permits(), 3);
    drop(tickets);
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn response_matrix_old_ready_and_duplicate_identity_leave_new_command() {
    let _dirs = crate::config::test_dirs::isolate_dirs("response-old-ready");
    let mut server = test_headless_server();
    let _input = install_focused_test_runtime(&mut server, b"");
    let writer = paused_client(&mut server, 1);
    let old = command_ticket(&mut server, 1, "same", false);
    server.set_client_shell_surface_active(1, false).unwrap();
    assert_eq!(
        server.endpoint_response_budget.available_permits(),
        5,
        "preparing worker still owns its permit"
    );
    server.set_client_shell_surface_active(1, true).unwrap();
    let current = command_ticket(&mut server, 1, "same", false);
    let identity = current.identity.clone();
    let lease = server.clients[&1].shell_endpoint_command_surface_revision;
    let text = client_commands::response_text("same", Ok(api::schema::ResponseResult::Ok {}));
    server.accept_endpoint_response(EndpointResponseReady::new(old, text.clone()));
    assert!(server.clients[&1].shell_endpoint_command_in_flight);
    assert_eq!(
        server.clients[&1].shell_endpoint_command_surface_revision,
        lease
    );
    assert_eq!(server.endpoint_response_budget.available_permits(), 5);
    server.endpoint_pump_remaining = 0;
    server.accept_endpoint_response(EndpointResponseReady::new(current, text.clone()));
    let duplicate = EndpointResponseTicket::acquire(
        &server.endpoint_response_budget,
        &Arc::new(tokio::sync::Semaphore::new(3)),
        identity.clone(),
    )
    .unwrap();
    server.accept_endpoint_response(EndpointResponseReady::new(duplicate, text.clone()));
    for boot in ["old-boot".to_owned(), server.client_shell_boot_id.clone()] {
        let foreign = EndpointResponseTicket::acquire(
            &server.endpoint_response_budget,
            &Arc::new(tokio::sync::Semaphore::new(3)),
            client_commands::EndpointResponseIdentity::new(1, boot, "same".into()),
        )
        .unwrap();
        server.accept_endpoint_response(EndpointResponseReady::new(foreign, text.clone()));
    }
    assert!(server.clients[&1].shell_endpoint_command_in_flight);
    assert!(server.clients[&1]
        .endpoint_responses
        .command
        .as_ref()
        .is_some_and(|token| Arc::ptr_eq(token, &identity)));
    assert_eq!(server.clients[&1].endpoint_responses.slots.len(), 1);
    assert_eq!(server.clients[&1].endpoint_responses.slots[0].offset, 0);
    assert_eq!(server.endpoint_response_budget.available_permits(), 5);
    for frame in writer.test_drain() {
        assert!(
            !matches!(
                read_server_message(frame),
                ServerMessage::ClientShellEndpointResponseChunk { .. }
            ),
            "stale or duplicate Ready must not emit a response for the active request"
        );
    }
    response_turn(&mut server, &writer);
    assert!(!server.clients[&1].shell_endpoint_command_in_flight);
    assert!(take_response(&writer, "same").get("result").is_some());
    assert_eq!(server.endpoint_response_budget.available_permits(), 6);
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn response_matrix_ready_and_sending_disconnect_and_shutdown_release_cursor_only() {
    let _dirs = crate::config::test_dirs::isolate_dirs("response-sending-cancel");
    for shutdown in [false, true] {
        for budget in [0, client_commands::ENDPOINT_BLOCKS_PER_TURN] {
            let mut server = test_headless_server();
            let writer = paused_client(&mut server, 1);
            let ticket = admit_reading(&mut server, 1, "sending");
            server.endpoint_pump_remaining = budget;
            server.accept_endpoint_response(EndpointResponseReady::new(
                ticket,
                exact_response_body(3 * 1024 * 1024, "sending", false),
            ));
            let accepted = usize::from(budget != 0);
            assert_eq!(writer.test_endpoint_frames_sent(), accepted);
            assert_eq!(server.endpoint_response_budget.available_permits(), 5);
            if shutdown {
                server.initiate_shutdown();
            } else {
                server.remove_client(1);
            }
            assert_eq!(server.endpoint_response_budget.available_permits(), 6);
            let preserved = writer
                .test_drain()
                .into_iter()
                .map(read_server_message)
                .filter(|message| {
                    matches!(
                        message,
                        ServerMessage::ClientShellEndpointResponseChunk {
                            final_chunk: false,
                            ..
                        }
                    )
                })
                .count();
            assert_eq!(
                preserved, accepted,
                "normal seal must preserve every already-accepted frame"
            );
            assert!(writer.control.try_send_endpoint_frame(vec![1]).is_err());
        }
    }
}

#[test]
fn dropped_unclaimed_ready_clears_command_on_one_prune() {
    let _dirs = crate::config::test_dirs::isolate_dirs("endpoint-ready-drop");
    let mut server = test_headless_server();
    server.clients.insert(
        701,
        ClientConnection::new(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            1,
            RenderEncoding::SemanticFrame,
            Some(ClientWriter::test_paused()),
        ),
    );
    let ticket = command_ticket(&mut server, 701, "abandoned", true);
    let ready = EndpointResponseReady::new(
        ticket,
        client_commands::response_text("abandoned", Ok(api::schema::ResponseResult::Ok {})),
    );
    drop(ready);
    let client = server.clients.get_mut(&701).unwrap();
    client.prune_endpoint_responses();
    assert!(!client.shell_endpoint_command_in_flight);
    assert_eq!(client.shell_endpoint_command_surface_revision, None);
    assert_eq!(client.shell_deferred_navigation_request_id, None);
    assert!(client.endpoint_responses.command.is_none());
    assert!(client.endpoint_responses.slots.is_empty());
    assert_eq!(
        server.endpoint_response_budget.available_permits(),
        client_commands::MAX_SERVER_RESPONSES
    );
    let tickets = (0..client_commands::MAX_CLIENT_RESPONSES)
        .map(|index| {
            server
                .admit_endpoint_response(
                    701,
                    server.client_shell_boot_id.clone(),
                    format!("retry-{index}"),
                    EndpointResponseKind::Reading,
                )
                .unwrap()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        tickets.len(),
        3,
        "both client and server permits must be returned"
    );
}
