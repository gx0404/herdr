use super::*;
use crate::api::schema::{ClientViewSpec, ClientViewsSetParams};

fn set_views(server: &mut HeadlessServer, revision: u64, tabs: &[String]) {
    let views = tabs
        .iter()
        .enumerate()
        .map(|(index, tab)| ClientViewSpec {
            view_id: format!("view-{index}"),
            tab_id: tab.clone(),
            cols: 40 + index as u16 * 10,
            rows: 12,
            focused: index == 0,
        })
        .collect();
    assert!(server
        .set_client_views(1, ClientViewsSetParams { revision, views })
        .unwrap());
}

fn decode_batch(
    bytes: Vec<u8>,
    decoder: &mut protocol::views::Decoder,
) -> Vec<protocol::views::DecodedView> {
    let mut cursor = std::io::Cursor::new(bytes);
    let mut views = Vec::new();
    while cursor.position() < cursor.get_ref().len() as u64 {
        let message: ServerMessage =
            protocol::read_message(&mut cursor, MAX_GRAPHICS_FRAME_SIZE).unwrap();
        let ServerMessage::EndpointControl { kind, data } = message else {
            panic!("未封装的旧帧进入多视图连接");
        };
        assert_eq!(kind, protocol::views::SURFACE_KIND);
        if let Some(view) = decoder.decode(&data).unwrap() {
            views.push(view);
        }
    }
    views
}

#[tokio::test]
async fn secondary_tab_dirty_rows_are_routed_through_the_retained_view() {
    let (mut server, _, output, _) = retained_test_server_with_control(b"first");
    let mut second = crate::workspace::Workspace::test_new("second");
    let second_id = second.tabs[0].root_pane;
    second.insert_test_runtime(
        second_id,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(50, 12, b"second"),
    );
    server.app.state.workspaces.push(second);
    let tabs = vec![
        server.app.public_tab_id(0, 0).unwrap(),
        server.app.public_tab_id(1, 0).unwrap(),
    ];
    set_views(&mut server, 1, &tabs);
    server.render_and_stream();
    let mut decoder = protocol::views::Decoder::default();
    let initial = decode_batch(output.recv().unwrap(), &mut decoder);
    assert_eq!(initial.len(), 2);
    let runtime = server
        .app
        .state
        .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 1, second_id)
        .unwrap();
    runtime.test_process_pty_bytes(b"\rupdated");
    assert!(server.render_retained_pane_surface_and_stream(&HashSet::from([second_id])));
    let updated = decode_batch(output.recv().unwrap(), &mut decoder);
    assert!(updated.iter().any(|view| view.tab_id == tabs[1]));
    assert!(updated.iter().all(|view| view.tab_id != tabs[0]));
}

#[tokio::test]
async fn public_focus_refreshes_a_hidden_tab_even_when_default_target_is_unchanged() {
    let (mut server, _, _, _) = retained_test_server_with_control(b"first");
    server
        .app
        .state
        .workspaces
        .push(crate::workspace::Workspace::test_new("second"));
    server.reconcile_client_shell_locations();
    let first = server.app.public_tab_id(0, 0).unwrap();
    let second = server.app.public_tab_id(1, 0).unwrap();
    let default_target = server.default_shell_target();
    set_views(&mut server, 1, std::slice::from_ref(&second));
    assert!(server.focus_shell_client_on_tab(1, &second));
    assert_eq!(server.default_shell_target(), default_target);
    assert_eq!(server.shell_tab_id_for_client(1), Some(second.clone()));
    let (respond_to, response) = std::sync::mpsc::channel();
    let changed = server.handle_api_request_with_shutdown_check(api::ApiRequestMessage {
        request: api::schema::Request {
            id: "focus-hidden".into(),
            method: api::schema::Method::TabFocus(api::schema::TabTarget {
                tab_id: first.clone(),
            }),
        },
        respond_to,
        response_write_complete: None,
        observation_events: None,
        stream_active: None,
    });
    let response: serde_json::Value = serde_json::from_str(&response.recv().unwrap()).unwrap();
    assert!(response.get("result").is_some(), "{response}");
    assert_eq!(server.shell_tab_id_for_client(1), Some(first.clone()));
    assert!(changed, "客户端焦点改变必须触发新投影");
    let views = server.clients[&1]
        .views
        .as_ref()
        .unwrap()
        .views
        .iter()
        .map(|view| view.spec.clone())
        .collect();
    server
        .set_client_views(1, ClientViewsSetParams { revision: 2, views })
        .unwrap();
    assert_eq!(
        server.shell_tab_id_for_client(1),
        Some(first),
        "迟到的布局刷新不能撤销公开导航"
    );
}

#[tokio::test]
async fn duplicate_tabs_and_stale_layouts_do_not_change_live_geometry() {
    let (mut server, _, _, _) = retained_test_server_with_control(b"");
    let tab = server.app.public_tab_id(0, 0).unwrap();
    set_views(&mut server, 2, std::slice::from_ref(&tab));
    assert!(server
        .set_client_views(
            1,
            ClientViewsSetParams {
                revision: 1,
                views: Vec::new(),
            }
        )
        .is_err());
    let spec = ClientViewSpec {
        view_id: "duplicate".into(),
        tab_id: tab.clone(),
        cols: 40,
        rows: 12,
        focused: false,
    };
    let mut focused = spec.clone();
    focused.view_id = "focused".into();
    focused.focused = true;
    assert!(server
        .set_client_views(
            1,
            ClientViewsSetParams {
                revision: 3,
                views: vec![spec, focused],
            }
        )
        .is_err());
    assert_eq!(server.clients[&1].views.as_ref().unwrap().revision, 2);
}

/// RS-14：多视图客户端没有 writer 时与单视图路径一致——跳过并延期，不断开
/// 连接（`Disconnected` 才是 writer 线程消失）。
#[tokio::test]
async fn multi_view_without_a_writer_defers_instead_of_disconnecting() {
    let (mut server, _, _, _) = retained_test_server_with_control(b"BASE");
    let tab = server.app.public_tab_id(0, 0).unwrap();
    set_views(&mut server, 1, std::slice::from_ref(&tab));
    server.clients.get_mut(&1).unwrap().writer = None;

    assert!(
        server.render_client_views(1),
        "missing writer must not be reported as a broken client"
    );
    assert!(server.clients.contains_key(&1));
    assert!(
        server.clients[&1].deferred_render() != DeferredRender::None,
        "deferred work must be armed so the frame is re-sent once a writer exists"
    );
}

#[tokio::test]
async fn resize_retires_held_keys_before_opening_the_new_view_generation() {
    let (mut server, _, _, pane) = retained_test_server_with_control(b"");
    let tab = server.app.public_tab_id(0, 0).unwrap();
    set_views(&mut server, 1, std::slice::from_ref(&tab));
    let pane = server.app.public_pane_id(0, pane).unwrap();
    let press = protocol::ClientPaneInputEvent::Key {
        code: protocol::ClientKeyCode::Char('a'),
        modifiers: 0,
        kind: protocol::ClientKeyKind::Press,
        repeat_count: 1,
        shifted_codepoint: None,
        generated_text: None,
        tracks_release: true,
        physical_key_id: None,
        windows_record: None,
    };
    server
        .clients
        .get_mut(&1)
        .unwrap()
        .track_shell_input(ClientShellInputTarget::Pane(pane), &[press]);
    set_views(&mut server, 2, &[tab]);
    assert!(server
        .clients
        .get_mut(&1)
        .unwrap()
        .drain_shell_held_inputs()
        .is_empty());
}

#[tokio::test]
async fn popup_pty_tracks_its_owning_view_after_resize() {
    let (mut server, _, output, _) = retained_test_server_with_control(b"parent");
    let popup = crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b"popup");
    let (_, terminal) = server.app.install_test_popup_runtime(popup);
    let tab = server.app.public_tab_id(0, 0).unwrap();
    server.popup_owner_tab_id = Some(tab.clone());
    let mut decoder = protocol::views::Decoder::default();
    for (revision, cols, rows) in [(1, 40, 12), (2, 90, 36)] {
        server
            .set_client_views(
                1,
                ClientViewsSetParams {
                    revision,
                    views: vec![ClientViewSpec {
                        view_id: "one".into(),
                        tab_id: tab.clone(),
                        cols,
                        rows,
                        focused: true,
                    }],
                },
            )
            .unwrap();
        server.render_and_stream();
        let frames = decode_batch(output.recv().unwrap(), &mut decoder);
        let ServerMessage::PaneSurface(surface) = &frames[0].message else {
            panic!("完整视图");
        };
        let popup = surface.popup.as_ref().expect("视图内的终端弹层");
        let runtime = server.app.terminal_runtimes.get(&terminal).unwrap();
        assert_eq!(
            runtime.current_size(),
            (popup.frame.height, popup.frame.width)
        );
    }
}

#[tokio::test]
#[ignore = "手动记录 1 与 15 个可见视图的扩展成本"]
async fn multi_view_render_scaling_profile() {
    for count in [1usize, 15] {
        let (mut server, _, output, first) = retained_test_server_with_control(b"first");
        let mut tabs = vec![server.app.public_tab_id(0, 0).unwrap()];
        for index in 1..count {
            let mut workspace = crate::workspace::Workspace::test_new("scaling");
            let pane = workspace.tabs[0].root_pane;
            workspace.insert_test_runtime(
                pane,
                crate::terminal::TerminalRuntime::test_with_screen_bytes(40, 12, b"text"),
            );
            server.app.state.workspaces.push(workspace);
            tabs.push(server.app.public_tab_id(index, 0).unwrap());
        }
        set_views(&mut server, 1, &tabs);
        let start = Instant::now();
        server.render_and_stream();
        let _ = output.recv().unwrap();
        let full = start.elapsed();
        let start = Instant::now();
        for index in 0..100 {
            server
                .app
                .state
                .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, first)
                .unwrap()
                .test_process_pty_bytes(format!("\r{index:04}").as_bytes());
            assert!(server.render_retained_pane_surface_and_stream(&HashSet::from([first])));
            let _ = output.recv().unwrap();
        }
        eprintln!(
            "views={count} first_frame_us={} retained_100_us={}",
            full.as_micros(),
            start.elapsed().as_micros()
        );
    }
}

#[tokio::test]
#[ignore = "手动记录快照捕获和分页在 1/15 pane 下的成本"]
async fn render_scale_profile_text_snapshot() {
    use crate::api::schema::{
        Method, ResponseResult, TextSnapshotCaptureParams, TextSnapshotReadParams,
        TextSnapshotTarget,
    };
    for count in [1usize, 15] {
        let text = "snapshot payload 01234567890123456789\r\n".repeat(1900);
        let (mut server, _, output, first) = retained_test_server_with_control(b"first");
        let runtime = crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
            40,
            12,
            8 * 1024 * 1024,
            text.as_bytes(),
            8,
        )
        .0;
        server.app.state.workspaces[0].insert_test_runtime(first, runtime);
        let mut tabs = vec![server.app.public_tab_id(0, 0).unwrap()];
        for index in 1..count {
            let mut workspace = crate::workspace::Workspace::test_new("snapshot-scale");
            let pane = workspace.tabs[0].root_pane;
            workspace.insert_test_runtime(
                pane,
                crate::terminal::TerminalRuntime::test_with_screen_bytes(40, 12, b"background"),
            );
            server.app.state.workspaces.push(workspace);
            tabs.push(server.app.public_tab_id(index, 0).unwrap());
        }
        let views = tabs
            .iter()
            .enumerate()
            .map(|(index, tab)| ClientViewSpec {
                view_id: format!("view-{index}"),
                tab_id: tab.clone(),
                cols: 40,
                rows: 12,
                focused: index == 0,
            })
            .collect();
        server
            .set_client_views(1, ClientViewsSetParams { revision: 1, views })
            .unwrap();
        server.render_and_stream();
        let _ = output.recv().unwrap();
        let pane_id = server.app.public_pane_id(0, first).unwrap();
        let mut captures = Vec::new();
        let mut reads = Vec::new();
        let mut retained = Vec::new();
        let mut bytes = 0;
        for sample in 0..45 {
            let started = Instant::now();
            let capture = server
                .text_snapshot_request(
                    &Method::PaneTextSnapshotCapture(TextSnapshotCaptureParams {
                        pane_id: pane_id.clone(),
                    }),
                    Some(1),
                )
                .unwrap();
            let elapsed = started.elapsed();
            bytes = serde_json::to_vec(&capture).unwrap().len();
            let ResponseResult::PaneTextSnapshot {
                snapshot_id, text, ..
            } = capture
            else {
                panic!("capture")
            };
            let started = Instant::now();
            server
                .text_snapshot_request(
                    &Method::PaneTextSnapshotRead(TextSnapshotReadParams {
                        snapshot_id: snapshot_id.clone(),
                        start_row: text.range_start,
                        rows: 44,
                    }),
                    Some(1),
                )
                .unwrap();
            let read = started.elapsed();
            server
                .text_snapshot_request(
                    &Method::PaneTextSnapshotRelease(TextSnapshotTarget { snapshot_id }),
                    Some(1),
                )
                .unwrap();
            let started = Instant::now();
            server
                .app
                .state
                .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, first)
                .unwrap()
                .test_process_pty_bytes(format!("\r{sample:04}").as_bytes());
            assert!(server.render_retained_pane_surface_and_stream(&HashSet::from([first])));
            let _ = output.recv().unwrap();
            if sample >= 5 {
                captures.push(elapsed.as_micros());
                reads.push(read.as_micros());
                retained.push(started.elapsed().as_micros());
            }
        }
        captures.sort_unstable();
        reads.sort_unstable();
        retained.sort_unstable();
        eprintln!("snapshot panes={count} capture_median_us={} capture_p95_us={} page_median_us={} retained_median_us={} retained_p95_us={} first_window_bytes={bytes}", captures[20], captures[38], reads[20], retained[20], retained[38]);
    }
}

#[tokio::test]
async fn retained_patch_keeps_a_visible_ime_anchor_when_the_pane_hides_its_cursor() {
    let (mut server, _, output, pane) = retained_test_server_with_control(b"first");
    let tabs = vec![server.app.public_tab_id(0, 0).unwrap()];
    set_views(&mut server, 1, &tabs);
    server.render_and_stream();
    let mut decoder = protocol::views::Decoder::default();
    let _ = decode_batch(output.recv().unwrap(), &mut decoder);
    let runtime = server
        .app
        .state
        .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane)
        .unwrap();
    runtime.test_process_pty_bytes(b"\x1b[?25lupdated");
    assert!(server.render_retained_pane_surface_and_stream(&HashSet::from([pane])));
    let updated = decode_batch(output.recv().unwrap(), &mut decoder);
    let patch = updated
        .iter()
        .find_map(|view| match &view.message {
            ServerMessage::PaneSurfacePatch(patch) => Some(patch),
            _ => None,
        })
        .expect("retained patch");
    let cursor = patch
        .cursor
        .as_ref()
        .expect("ime anchor cursor survives the retained fast path");
    assert!(cursor.visible, "reveal keeps the anchor visible after ?25l");
    assert_eq!(cursor.shape, 5, "anchor uses the blinking-bar shape");
}

/// 多视图连接的投影改戳：每个 view 的已提交基线各自改戳，合成一次写入；解码后
/// 与初始帧逐格一致，修订号与连接的新快照修订号配对。
#[tokio::test]
async fn projection_restamp_covers_every_view_in_one_batch() {
    // 控制端要保活：投影刷新经它下发快照，接收端丢了客户端会被当成断开。
    let (mut server, _control, output, _) = retained_test_server_with_control(b"first");
    let mut second = crate::workspace::Workspace::test_new("second");
    let second_id = second.tabs[0].root_pane;
    second.insert_test_runtime(
        second_id,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(50, 12, b"second"),
    );
    server.app.state.workspaces.push(second);
    let tabs = vec![
        server.app.public_tab_id(0, 0).unwrap(),
        server.app.public_tab_id(1, 0).unwrap(),
    ];
    set_views(&mut server, 1, &tabs);
    server.render_and_stream();
    let mut decoder = protocol::views::Decoder::default();
    let initial = decode_batch(output.recv().unwrap(), &mut decoder);
    assert_eq!(initial.len(), 2);
    let before = server.clients[&1].shell_projection_revision;

    server.app.state.tab_bar_right = vec![crate::app::state::TabBarStatusSegment::Text(Some(
        "12:00".into(),
    ))];
    server.app.state.bump_projection_epoch();
    server.dispatch_render_tick(true, false, &HashSet::new(), false);

    let revision = server.clients[&1].shell_projection_revision;
    assert!(revision > before);
    let restamped = decode_batch(
        output
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("restamped views"),
        &mut decoder,
    );
    assert_eq!(restamped.len(), 2);
    for (view, first) in restamped.iter().zip(&initial) {
        let (ServerMessage::PaneSurface(surface), ServerMessage::PaneSurface(original)) =
            (&view.message, &first.message)
        else {
            panic!("views decode to full surfaces");
        };
        assert_eq!(view.view_id, first.view_id);
        assert_eq!(surface.projection_revision, revision);
        assert_eq!(surface.frame, original.frame);
    }
}
