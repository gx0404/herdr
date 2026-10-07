use super::*;
use crate::api::schema::{ClientViewSpec, ClientViewsSetParams};
use crate::protocol::{ClientKeyCode, ClientKeyKind, ClientMessage, ClientPaneInputEvent};

struct ShellInputHarness {
    server: HeadlessServer,
    shell: crate::client::ClientShellState,
    input: tokio::sync::mpsc::Receiver<Bytes>,
    _control: std::sync::mpsc::Receiver<Vec<u8>>,
    _render: std::sync::mpsc::Receiver<Vec<u8>>,
    pane: String,
    tab: String,
    view_revision: Option<u64>,
}

impl ShellInputHarness {
    fn new(report_all: bool, views: bool) -> Self {
        let mut server = test_headless_server();
        let mode: &[u8] = if report_all {
            b"\x1b[>11u"
        } else {
            b"\x1b[>3u"
        };
        let input = install_focused_test_runtime(&mut server, mode);
        let (writer, control, render) = test_client_writer();
        server.handle_server_event(ServerEvent::ClientShellConnected {
            surface_reuse: false,
            surface_delta: false,
            surface_scroll: false,
            client_id: 1,
            surface_cols: 80,
            surface_rows: 24,
            cell_width_px: 0,
            cell_height_px: 0,
            pixel_mouse: false,
            direct_graphics: false,
            endpoint_keybindings: true,
            mouse_capture: true,
            surface_active: true,
            ssh_auth_sock: None,
            writer,
        });
        let snapshot = client_shell_snapshot(&control);
        let pane = snapshot.focused_pane_id.clone().unwrap();
        let tab = server.app.public_tab_id(0, 0).unwrap();
        let mut shell = crate::client::ClientShellState::new(
            crate::client::ClientShellConfig::from_config(&crate::config::Config::default()),
        );
        shell.set_host_reports_key_releases(true);
        shell.set_host_reports_all_keys(report_all);
        shell.set_snapshot(snapshot);
        let mut harness = Self {
            server,
            shell,
            input,
            _control: control,
            _render: render,
            pane,
            tab,
            view_revision: None,
        };
        if views {
            harness.change_layout();
        }
        assert!(harness.take_input().is_empty());
        harness
    }

    fn change_layout(&mut self) {
        let revision = self.view_revision.unwrap_or(0) + 1;
        self.server
            .set_client_views(
                1,
                ClientViewsSetParams {
                    revision,
                    views: vec![ClientViewSpec {
                        view_id: "typing".into(),
                        tab_id: self.tab.clone(),
                        cols: 40 + revision as u16,
                        rows: 12,
                        focused: true,
                    }],
                },
            )
            .unwrap();
        self.view_revision = Some(revision);
    }

    fn feed(&mut self, bytes: &[u8]) -> Vec<ClientPaneInputEvent> {
        let outcome = self.shell.handle_input_bytes(bytes);
        assert!(outcome.actions.is_empty());
        self.dispatch(outcome.requests)
    }

    fn dispatch(&mut self, requests: Vec<ClientMessage>) -> Vec<ClientPaneInputEvent> {
        let mut delivered = Vec::new();
        for request in requests {
            let request = match (self.view_revision, request) {
                (Some(revision), ClientMessage::ClientShellPaneInput { pane_id, events }) => {
                    ClientMessage::EndpointControl {
                        kind: protocol::views::INPUT_KIND.into(),
                        data: serde_json::to_string(&protocol::views::ViewInput {
                            boot_id: self.server.client_shell_boot_id.clone(),
                            views_revision: revision,
                            view_id: "typing".into(),
                            tab_id: self.tab.clone(),
                            pane_id,
                            events,
                        })
                        .unwrap(),
                    }
                }
                (_, request) => request,
            };
            let mut frame = Vec::new();
            protocol::write_message(&mut frame, &request).unwrap();
            let request: ClientMessage =
                protocol::read_message(&mut std::io::Cursor::new(frame), protocol::MAX_FRAME_SIZE)
                    .unwrap();
            let event = match request {
                ClientMessage::ClientShellPaneInput { pane_id, events } => {
                    delivered.extend(events.clone());
                    ServerEvent::ClientShellPaneInput {
                        client_id: 1,
                        pane_id,
                        events,
                    }
                }
                ClientMessage::EndpointControl { kind, data } => {
                    assert_eq!(kind, protocol::views::INPUT_KIND);
                    let input: protocol::views::ViewInput = serde_json::from_str(&data).unwrap();
                    delivered.extend(input.events.clone());
                    ServerEvent::ClientViewInput {
                        client_id: 1,
                        input,
                    }
                }
                ClientMessage::ClientShellFocus { focused } => ServerEvent::ClientShellFocus {
                    client_id: 1,
                    focused,
                },
                other => panic!("unexpected shell request: {other:?}"),
            };
            self.server.handle_server_event(event);
        }
        delivered
    }

    fn take_input(&mut self) -> Vec<u8> {
        let mut bytes = Vec::new();
        while let Ok(chunk) = self.input.try_recv() {
            bytes.extend_from_slice(&chunk);
        }
        bytes
    }

    fn assert_no_held_input(&mut self) {
        assert!(self
            .server
            .clients
            .get_mut(&1)
            .unwrap()
            .drain_shell_held_inputs()
            .is_empty());
    }

    fn assert_clean_teardown(&mut self) {
        self.assert_no_held_input();
        self.change_layout();
        assert!(self.take_input().is_empty(), "layout invented a release");
        self.server
            .handle_server_event(ServerEvent::ClientDetach { client_id: 1 });
        assert!(!self.server.clients.contains_key(&1));
        assert!(self.take_input().is_empty(), "detach invented a release");
    }
}

impl Drop for ShellInputHarness {
    fn drop(&mut self) {
        shutdown_test_runtimes(&mut self.server);
    }
}

#[tokio::test]
async fn text_key_real_releases_do_not_leave_server_leases() {
    let _dirs = crate::config::test_dirs::isolate_dirs("text-key-leases");
    for views in [false, true] {
        for (press, release, released_code) in [
            ("A", &b"\x1b[97;1:3u"[..], 'a'),
            ("?", &b"\x1b[47;1:3u"[..], '/'),
        ] {
            let mut harness = ShellInputHarness::new(false, views);
            for _ in 0..2 {
                let events = harness.feed(press.as_bytes());
                assert!(matches!(
                    &events[..],
                    [ClientPaneInputEvent::Key {
                        kind: ClientKeyKind::Press,
                        tracks_release: false,
                        ..
                    }]
                ));
                assert_eq!(harness.take_input(), press.as_bytes());
            }
            let events = harness.feed(release);
            assert!(matches!(
                &events[..],
                [ClientPaneInputEvent::Key {
                    code: ClientKeyCode::Char(code),
                    kind: ClientKeyKind::Release,
                    modifiers: 0,
                    ..
                }] if *code == released_code
            ));
            assert_eq!(harness.take_input(), release);
            harness.assert_clean_teardown();
        }
    }
}

#[tokio::test]
async fn ambiguous_text_and_ime_never_synthesize_server_releases() {
    let _dirs = crate::config::test_dirs::isolate_dirs("ime-server-leases");
    for views in [false, true] {
        for report_all in [false, true] {
            for blur in [false, true] {
                let mut harness = ShellInputHarness::new(report_all, views);
                for text in ["A", "?", "中", "文"] {
                    let events = harness.feed(text.as_bytes());
                    assert!(events.iter().all(|event| matches!(
                        event,
                        ClientPaneInputEvent::TextCommit(_)
                            | ClientPaneInputEvent::Key {
                                tracks_release: false,
                                ..
                            }
                    )));
                    assert_eq!(harness.take_input(), text.as_bytes());
                }
                if blur {
                    harness.feed(b"\x1b[O");
                    assert!(harness.take_input().is_empty());
                }
                harness.assert_clean_teardown();
            }
        }
    }
}

#[tokio::test]
async fn reported_key_repeats_and_cleanup_survive_ambiguous_text() {
    let _dirs = crate::config::test_dirs::isolate_dirs("reported-key-leases");
    for views in [false, true] {
        for cleanup in ["release", "blur", "layout", "detach"] {
            let mut harness = ShellInputHarness::new(false, views);
            let events = harness.feed(b"\x1b[120;;120u");
            assert!(matches!(
                &events[..],
                [ClientPaneInputEvent::Key {
                    tracks_release: true,
                    ..
                }]
            ));
            assert_eq!(harness.take_input(), b"x");
            let repeat = harness.feed(b"\x1b[120;1:2;120u");
            assert!(matches!(
                &repeat[..],
                [ClientPaneInputEvent::Key {
                    kind: ClientKeyKind::Repeat,
                    ..
                }]
            ));
            assert!(!harness.take_input().is_empty());
            for text in ["A", "中", "文"] {
                harness.feed(text.as_bytes());
                assert_eq!(harness.take_input(), text.as_bytes());
            }
            let release = ClientPaneInputEvent::from_terminal_key(
                crate::input::TerminalKey::new(
                    crossterm::event::KeyCode::Char('x'),
                    crossterm::event::KeyModifiers::NONE,
                )
                .with_kind(crossterm::event::KeyEventKind::Release),
            )
            .unwrap();
            assert!(harness.server.clients[&1].owns_shell_release(&harness.pane, &release));
            match cleanup {
                "release" => {
                    harness.feed(b"\x1b[120;1:3u");
                }
                "blur" => {
                    harness.feed(b"\x1b[O");
                }
                "layout" => harness.change_layout(),
                "detach" => {
                    harness
                        .server
                        .handle_server_event(ServerEvent::ClientDetach { client_id: 1 });
                }
                _ => unreachable!(),
            }
            assert_eq!(harness.take_input(), b"\x1b[120;1:3u", "{cleanup}");
            if cleanup != "detach" {
                harness.assert_clean_teardown();
            } else {
                assert!(!harness.server.clients.contains_key(&1));
            }
        }
    }
}

#[tokio::test]
async fn native_text_keys_keep_server_cleanup_leases() {
    let _dirs = crate::config::test_dirs::isolate_dirs("native-text-leases");
    for views in [false, true] {
        let mut harness = ShellInputHarness::new(false, views);
        let key = crate::input::TerminalKey::new(
            crossterm::event::KeyCode::Char('x'),
            crossterm::event::KeyModifiers::NONE,
        )
        .with_generated_text(Some("x".into()))
        .with_windows_record(crate::input::WindowsKeyRecord {
            key_down: true,
            repeat_count: 1,
            virtual_key_code: 0x58,
            virtual_scan_code: 0x2d,
            unicode: 'x' as u16,
            control_key_state: 0,
        });
        let outcome = harness
            .shell
            .handle_raw_events(vec![crate::raw_input::RawInputEvent::Key(key)]);
        assert!(outcome.actions.is_empty());
        let events = harness.dispatch(outcome.requests);
        assert!(matches!(
            &events[..],
            [ClientPaneInputEvent::Key {
                tracks_release: true,
                physical_key_id: Some(_),
                ..
            }]
        ));
        assert_eq!(harness.take_input(), b"x");
        harness.change_layout();
        assert_eq!(harness.take_input(), b"\x1b[120;1:3u");
        harness.assert_clean_teardown();
    }
}
