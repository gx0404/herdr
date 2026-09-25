use super::*;
use crate::api::schema::{Method, ResponseResult};
use crate::terminal::text_snapshot::{FrozenCell, FrozenRow, FrozenText};
use crossterm::event::KeyEvent;

fn ready() -> (ClientShellState, MouseEvent) {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    state.set_endpoint_methods(Some(vec![
        "pane.text_snapshot.capture".into(),
        "pane.text_snapshot.read".into(),
        "pane.text_snapshot.retain".into(),
        "pane.text_snapshot.selection".into(),
        "pane.text_snapshot.release".into(),
        "pane.focus".into(),
    ]));
    state.compose(106, 20).unwrap();
    let hit = &state.hits.panes[0];
    let mouse = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: hit.inner_rect.x,
        row: hit.inner_rect.y,
        modifiers: KeyModifiers::empty(),
    };
    (state, mouse)
}

fn captured() -> ResponseResult {
    ResponseResult::PaneTextSnapshot {
        snapshot_id: "frozen-1".into(),
        pane_id: "pane_1".into(),
        boot_id: "boot-1".into(),
        text: Box::new(FrozenText {
            cols: 4,
            viewport_rows: 2,
            viewport_start: 0,
            row_origin: 0,
            range_start: 0,
            range_end: 2,
            total_rows: 2,
            alternate_screen: false,
            content_revision: 0,
            truncated: false,
            rows: ["LIVE", "PANE"]
                .iter()
                .map(|line| FrozenRow {
                    soft_wrapped: false,
                    cells: line
                        .chars()
                        .map(|ch| FrozenCell {
                            text: ch.to_string(),
                            fg: 0,
                            bg: 0,
                            modifier: 0,
                            width: 1,
                            hyperlink: None,
                        })
                        .collect(),
                })
                .collect(),
        }),
    }
}

fn capture_id(outcome: &ClientShellInput) -> String {
    outcome
        .actions
        .iter()
        .find_map(|action| match action {
            ClientShellAction::Endpoint { request, .. }
                if matches!(request.method, Method::PaneTextSnapshotCapture(_)) =>
            {
                Some(request.id.clone())
            }
            _ => None,
        })
        .unwrap()
}

fn reporting_codex() -> (ClientShellState, MouseEvent) {
    let (mut state, _) = ready();
    let mut projected = snapshot();
    projected.agents.push(ClientShellAgent {
        pane_id: "pane_1".into(),
        workspace_id: "ws_1".into(),
        tab_id: "tab_1".into(),
        name: None,
        display_agent: None,
        agent: Some("codex".into()),
        title: None,
        terminal_title: None,
        terminal_title_stripped: None,
        agent_status: AgentStatus::Idle,
        state_change_seq: 0,
        state_labels: Vec::new(),
        tokens: Vec::new(),
        focused: true,
        launch_seq: 0,
        activity: Default::default(),
    });
    state.set_snapshot(Box::new(projected));
    let mut pane_surface = surface();
    pane_surface.panes[0].mouse_reporting = true;
    state.set_pane_surface(pane_surface);
    state.compose(106, 20).unwrap();
    let hit = &state.hits.panes[0];
    let mouse = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: hit.inner_rect.x,
        row: hit.inner_rect.y,
        modifiers: KeyModifiers::empty(),
    };
    (state, mouse)
}

fn native_mouse_kinds(outcome: &ClientShellInput) -> Vec<crate::protocol::ClientMouseKind> {
    outcome
        .requests
        .iter()
        .flat_map(|request| match request {
            ClientMessage::ClientShellPaneInput { events, .. } => events.as_slice(),
            _ => &[],
        })
        .filter_map(|event| match event {
            ClientPaneInputEvent::Mouse { kind, .. } => Some(*kind),
            _ => None,
        })
        .collect()
}

#[test]
fn codex_native_gestures_cover_alt_unknown_agent_and_missing_capability() {
    for case in ["alt", "unknown", "other", "missing", "disabled"] {
        let (mut state, mut mouse) = reporting_codex();
        match case {
            "alt" => mouse.modifiers = KeyModifiers::ALT,
            "unknown" => {
                let mut projected = (**state.snapshot.as_ref().unwrap()).clone();
                projected.agents[0].agent = None;
                projected.agents[0].display_agent = Some("codex".into());
                state.set_snapshot(Box::new(projected));
            }
            "other" => {
                let mut projected = (**state.snapshot.as_ref().unwrap()).clone();
                projected.agents[0].agent = Some("claude".into());
                state.set_snapshot(Box::new(projected));
            }
            "missing" => {
                state.set_endpoint_methods(Some(vec!["pane.text_snapshot.capture".into()]))
            }
            "disabled" => state.config.mouse_capture = false,
            _ => unreachable!(),
        }
        let down = state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]);
        if case == "disabled" {
            assert!(state.pane_selection_press.is_none());
            continue;
        }
        assert_eq!(native_mouse_kinds(&down).len(), 1, "{case}");
        mouse.kind = MouseEventKind::Drag(MouseButton::Left);
        mouse.column += 2;
        let drag = state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]);
        assert_eq!(native_mouse_kinds(&drag).len(), 1, "{case}");
        mouse.kind = MouseEventKind::Up(MouseButton::Left);
        let up = state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]);
        assert_eq!(native_mouse_kinds(&up).len(), 1, "{case}");
        assert!(state.selection_capture.is_none(), "{case}");
    }
}

#[test]
fn codex_same_cell_drag_remains_a_native_click() {
    let (mut state, mut mouse) = reporting_codex();
    state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]);
    mouse.kind = MouseEventKind::Drag(MouseButton::Left);
    assert!(state
        .handle_raw_events(vec![RawInputEvent::Mouse(mouse)])
        .requests
        .is_empty());
    assert!(state.selection_capture.is_none());
    mouse.kind = MouseEventKind::Up(MouseButton::Left);
    assert_eq!(
        native_mouse_kinds(&state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)])).len(),
        2
    );
}

#[test]
fn codex_release_in_another_cell_starts_selection_without_drag_report() {
    let (mut state, mut mouse) = reporting_codex();
    state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]);
    mouse.kind = MouseEventKind::Up(MouseButton::Left);
    mouse.column += 2;
    let up = state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]);
    assert!(up.requests.is_empty());
    let id = capture_id(&up);
    let (_, actions) = state.handle_endpoint_result("boot-1", &id, Ok(captured()));
    assert!(actions.iter().any(
        |action| matches!(action, ClientShellAction::Endpoint { request, .. }
        if matches!(request.method, Method::PaneTextSnapshotSelection(_)))
    ));
}

#[test]
fn codex_pending_press_never_replays_into_changed_identity_or_geometry() {
    for case in [
        "endpoint",
        "boot",
        "generation",
        "pane",
        "focus",
        "geometry",
        "overlay",
    ] {
        let (mut state, mut mouse) = reporting_codex();
        state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]);
        match case {
            "endpoint" => {
                state.active_endpoint_id =
                    ClientEndpointId::Ssh(crate::client::endpoint::ProfileId::generate())
            }
            "generation" => state.active_snapshot_generation = Some(999),
            "geometry" => state.hits.panes[0].inner_rect.x += 1,
            "overlay" => state.open_pane_context_menu("pane_1".into(), mouse.column, mouse.row),
            _ => {
                let mut projected = (**state.snapshot.as_ref().unwrap()).clone();
                match case {
                    "boot" => projected.boot_id = "boot-2".into(),
                    "pane" => projected.panes.clear(),
                    "focus" => projected.focused_pane_id = Some("pane_2".into()),
                    _ => unreachable!(),
                }
                state.set_snapshot(Box::new(projected));
            }
        }
        mouse.kind = MouseEventKind::Up(MouseButton::Left);
        let up = state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]);
        assert!(up.requests.is_empty(), "{case}");
        assert!(state.pane_selection_press.is_none(), "{case}");
        assert!(state.selection_capture.is_none(), "{case}");
    }
}

#[test]
fn codex_pending_press_is_cancelled_by_popup_before_compose() {
    for pending in [false, true] {
        let (mut state, mut mouse) = reporting_codex();
        state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]);
        if pending {
            state.popup_pending = true;
        } else {
            let mut popup = surface_with_popup();
            popup.panes[0].mouse_reporting = true;
            state.set_pane_surface(popup);
            assert!(state.pane_selection_press.is_none());
        }
        mouse.kind = MouseEventKind::Up(MouseButton::Left);
        let up = state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]);
        assert!(native_mouse_kinds(&up).is_empty(), "pending={pending}");
        assert!(state.pane_selection_press.is_none());
    }
}

#[test]
fn codex_pending_press_is_cancelled_by_committed_text_and_paste() {
    for event in [
        RawInputEvent::Text(crate::input::TextCommit::new("typed")),
        RawInputEvent::Paste("pasted".into()),
    ] {
        let (mut state, mut mouse) = reporting_codex();
        state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]);
        let input = state.handle_raw_events(vec![event]);
        assert!(!input.requests.is_empty());
        assert!(state.pane_selection_press.is_none());
        mouse.kind = MouseEventKind::Up(MouseButton::Left);
        let up = state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]);
        assert!(native_mouse_kinds(&up).is_empty());
    }
}

fn released_codex_selection() -> (ClientShellState, MouseEvent) {
    released_codex_selection_at_width(106)
}

fn released_codex_selection_at_width(width: u16) -> (ClientShellState, MouseEvent) {
    let (mut state, mut mouse) = reporting_codex();
    state.compose(width, 20).unwrap();
    mouse.column = state.hits.panes[0].inner_rect.x;
    mouse.row = state.hits.panes[0].inner_rect.y;
    state.config.copy_on_select = false;
    state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]);
    mouse.kind = MouseEventKind::Drag(MouseButton::Left);
    mouse.column += 2;
    let drag = state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]);
    state.handle_endpoint_result("boot-1", &capture_id(&drag), Ok(captured()));
    mouse.kind = MouseEventKind::Up(MouseButton::Left);
    let up = state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]);
    assert!(!up.actions.iter().any(
        |action| matches!(action, ClientShellAction::Endpoint { request, .. }
        if matches!(request.method, Method::PaneTextSnapshotSelection(_)))
    ));
    (state, mouse)
}

#[test]
fn codex_copy_menu_works_with_keyboard_at_wide_and_narrow_widths() {
    for width in [106, 72, 40] {
        let (mut state, mut mouse) = released_codex_selection_at_width(width);
        mouse.kind = MouseEventKind::Down(MouseButton::Right);
        state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]);
        let Some(ClientShellOverlay::ContextMenu(menu)) = state.overlay.as_mut() else {
            panic!("menu");
        };
        let index = menu
            .items()
            .iter()
            .position(|item| item.action == ClientContextMenuAction::CopyPaneSelection)
            .unwrap();
        assert!(menu.items()[index].enabled);
        menu.highlighted = index;
        state.compose(width, 20).unwrap();
        let copy = state.handle_raw_events(vec![RawInputEvent::Key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()).into(),
        )]);
        assert!(copy.requests.is_empty());
        let request = copy
            .actions
            .iter()
            .find_map(|action| match action {
                ClientShellAction::Endpoint { request, .. }
                    if matches!(request.method, Method::PaneTextSnapshotSelection(_)) =>
                {
                    Some(request)
                }
                _ => None,
            })
            .expect("menu copies without terminal keys");
        let (_, copied) = state.handle_endpoint_result(
            "boot-1",
            &request.id,
            Ok(ResponseResult::PaneTextSnapshotSelection {
                snapshot_id: "frozen-1".into(),
                text: "LIV".into(),
            }),
        );
        assert!(copied.iter().any(
            |action| matches!(action, ClientShellAction::ClipboardWrite(bytes) if bytes == b"LIV")
        ));
    }
}

#[test]
fn pane_copy_menu_without_selection_is_disabled_and_stale_menu_is_inert() {
    let (mut state, mouse) = reporting_codex();
    state.open_pane_context_menu("pane_1".into(), mouse.column, mouse.row);
    let Some(ClientShellOverlay::ContextMenu(menu)) = state.overlay.as_ref() else {
        panic!("menu");
    };
    let index = menu
        .items()
        .iter()
        .position(|item| item.action == ClientContextMenuAction::CopyPaneSelection)
        .unwrap();
    assert!(!menu.items()[index].enabled);
    let mut outcome = ClientShellInput::default();
    state.activate_context_menu_item(index, &mut outcome);
    assert!(outcome.requests.is_empty() && outcome.actions.is_empty());

    let (mut state, mouse) = released_codex_selection();
    state.open_pane_context_menu("pane_1".into(), mouse.column, mouse.row);
    state.cancel_frozen_selection();
    state.activate_context_menu_item(index, &mut outcome);
    assert!(outcome.requests.is_empty() && outcome.actions.is_empty());
}

#[test]
fn codex_copy_menu_mouse_activation_preserves_selection_until_response() {
    let (mut state, mut mouse) = released_codex_selection();
    mouse.kind = MouseEventKind::Down(MouseButton::Right);
    state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]);
    state.compose(106, 20).unwrap();
    let Some(ClientShellOverlay::ContextMenu(menu)) = state.overlay.as_ref() else {
        panic!("menu");
    };
    let index = menu
        .items()
        .iter()
        .position(|item| item.action == ClientContextMenuAction::CopyPaneSelection)
        .unwrap();
    let (row, _) = state
        .hits
        .context_menu_rows
        .iter()
        .find(|(_, row)| *row == index)
        .unwrap();
    mouse.kind = MouseEventKind::Down(MouseButton::Left);
    mouse.column = row.x;
    mouse.row = row.y;
    let copy = state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]);
    assert!(copy.requests.is_empty());
    assert!(copy.actions.iter().any(
        |action| matches!(action, ClientShellAction::Endpoint { request, .. }
        if matches!(request.method, Method::PaneTextSnapshotSelection(_)))
    ));
    assert!(state.selection_capture.is_some());
}

#[test]
fn codex_right_click_passthrough_keeps_native_behavior() {
    let (mut state, mut mouse) = released_codex_selection();
    let mut projected = (**state.snapshot.as_ref().unwrap()).clone();
    projected.panes[0].right_click_passthrough = true;
    state.set_snapshot(Box::new(projected));
    mouse.kind = MouseEventKind::Down(MouseButton::Right);
    let down = state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]);
    assert_eq!(native_mouse_kinds(&down).len(), 1);
    assert!(state.overlay.is_none());
    assert!(state.selection_capture.is_none());
}

#[test]
fn stale_copy_menu_cannot_copy_after_endpoint_generation_changes() {
    let (mut state, mouse) = released_codex_selection();
    state.open_pane_context_menu("pane_1".into(), mouse.column, mouse.row);
    let Some(ClientShellOverlay::ContextMenu(menu)) = state.overlay.as_ref() else {
        panic!("menu");
    };
    let index = menu
        .items()
        .iter()
        .position(|item| item.action == ClientContextMenuAction::CopyPaneSelection)
        .unwrap();
    state.active_snapshot_generation = Some(999);
    let mut copy = ClientShellInput::default();
    state.activate_context_menu_item(index, &mut copy);
    assert!(copy.requests.is_empty() && copy.actions.is_empty());
}

#[test]
fn pane_copy_menu_reads_completed_live_selection_without_terminal_keys() {
    let (mut state, mouse) = ready();
    state.set_endpoint_methods(None);
    let mut selection =
        crate::selection::Selection::absolute_range("pane_1".into(), (0, 0), (0, 2));
    selection.finish();
    state.selection = Some(selection);
    state.open_pane_context_menu("pane_1".into(), mouse.column, mouse.row);
    let Some(ClientShellOverlay::ContextMenu(menu)) = state.overlay.as_ref() else {
        panic!("menu");
    };
    let index = menu
        .items()
        .iter()
        .position(|item| item.action == ClientContextMenuAction::CopyPaneSelection)
        .unwrap();
    let mut copy = ClientShellInput::default();
    state.activate_context_menu_item(index, &mut copy);
    assert!(copy.requests.is_empty());
    assert!(copy.actions.iter().any(
        |action| matches!(action, ClientShellAction::Endpoint { request, .. }
        if matches!(request.method, Method::PaneSelectionRead(_)))
    ));
    assert!(state.selection.is_none());
}

#[test]
fn codex_click_waits_for_release_then_preserves_native_click() {
    let (mut state, mut mouse) = reporting_codex();
    let down = state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]);
    assert!(down.requests.is_empty(), "按下先等是否拖动");
    mouse.kind = MouseEventKind::Up(MouseButton::Left);
    let up = state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]);
    let kinds = up
        .requests
        .iter()
        .flat_map(|request| match request {
            ClientMessage::ClientShellPaneInput { events, .. } => events.as_slice(),
            _ => &[],
        })
        .filter_map(|event| match event {
            ClientPaneInputEvent::Mouse { kind, .. } => Some(*kind),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        kinds,
        vec![
            crate::protocol::ClientMouseKind::Down(crate::protocol::ClientMouseButton::Left),
            crate::protocol::ClientMouseKind::Up(crate::protocol::ClientMouseButton::Left),
        ]
    );
    assert!(state.selection_capture.is_none());
}

#[test]
fn codex_drag_uses_frozen_selection_and_copies_after_delayed_capture() {
    let (mut state, mut mouse) = reporting_codex();
    state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]);
    mouse.kind = MouseEventKind::Drag(MouseButton::Left);
    mouse.column += 2;
    let drag = state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]);
    assert!(drag.requests.is_empty(), "拖选不触发应用鼠标/按键");
    let id = capture_id(&drag);
    mouse.kind = MouseEventKind::Up(MouseButton::Left);
    assert!(state
        .handle_raw_events(vec![RawInputEvent::Mouse(mouse)])
        .requests
        .is_empty());
    let (_, actions) = state.handle_endpoint_result("boot-1", &id, Ok(captured()));
    let request = actions
        .iter()
        .find_map(|action| match action {
            ClientShellAction::Endpoint { request, .. }
                if matches!(&request.method, Method::PaneTextSnapshotSelection(params)
                if params.anchor.col == 0 && params.cursor.col == 2) =>
            {
                Some(request)
            }
            _ => None,
        })
        .expect("释放后以最初按下坐标复制冻结选区");
    let (_, copied) = state.handle_endpoint_result(
        "boot-1",
        &request.id,
        Ok(ResponseResult::PaneTextSnapshotSelection {
            snapshot_id: "frozen-1".into(),
            text: "LIV".into(),
        }),
    );
    assert!(matches!(&copied[..], [ClientShellAction::ClipboardWrite(bytes)] if bytes == b"LIV"));
}

#[test]
fn pane_copy_menu_preserves_a_released_frozen_selection() {
    let _lang = crate::i18n::lang_guard(crate::i18n::Lang::En);
    let (mut state, mut mouse) = ready();
    state.config.copy_on_select = false;
    let id = capture_id(&state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]));
    state.handle_endpoint_result("boot-1", &id, Ok(captured()));
    mouse.kind = MouseEventKind::Drag(MouseButton::Left);
    mouse.column += 2;
    state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]);
    mouse.kind = MouseEventKind::Up(MouseButton::Left);
    state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]);
    mouse.kind = MouseEventKind::Down(MouseButton::Right);
    state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]);
    state.tick_frozen_selection(std::time::Instant::now(), &mut ClientShellInput::default());
    assert!(
        state.selection_capture.is_some(),
        "右键菜单不销毁同窗格已完成选区"
    );
    let Some(ClientShellOverlay::ContextMenu(menu)) = state.overlay.as_ref() else {
        panic!("pane context menu");
    };
    let index = menu
        .items()
        .iter()
        .position(|item| item.label == "Copy" && item.enabled)
        .expect("可用复制条目");
    let mut copy = ClientShellInput::default();
    state.activate_context_menu_item(index, &mut copy);
    assert!(copy.actions.iter().any(|action| matches!(action,
        ClientShellAction::Endpoint { request, .. }
            if matches!(&request.method, Method::PaneTextSnapshotSelection(params)
                if params.anchor.col == 0 && params.cursor.col == 2))));
}

#[test]
fn delayed_capture_uses_release_position_and_copies_once() {
    let (mut state, mut mouse) = ready();
    let id = capture_id(&state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]));
    mouse.kind = MouseEventKind::Drag(MouseButton::Left);
    mouse.column += 1;
    state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]);
    mouse.kind = MouseEventKind::Up(MouseButton::Left);
    mouse.column += 2;
    let release = state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]);
    assert!(release.actions.is_empty());
    let (_, actions) = state.handle_endpoint_result("boot-1", &id, Ok(captured()));
    let request = actions
        .iter()
        .find_map(|action| match action {
            ClientShellAction::Endpoint { request, .. }
                if matches!(request.method, Method::PaneTextSnapshotSelection(_)) =>
            {
                Some(request)
            }
            _ => None,
        })
        .expect("等待快照后复制");
    assert!(
        matches!(&request.method, Method::PaneTextSnapshotSelection(params) if params.anchor.col == 0 && params.cursor.col == 3)
    );
    let copied = ResponseResult::PaneTextSnapshotSelection {
        snapshot_id: "frozen-1".into(),
        text: "LIVE".into(),
    };
    let (_, actions) = state.handle_endpoint_result("boot-1", &request.id, Ok(copied.clone()));
    assert!(matches!(&actions[..], [ClientShellAction::ClipboardWrite(text)] if text == b"LIVE"));
    assert!(state
        .handle_endpoint_result("boot-1", &request.id, Ok(copied))
        .1
        .is_empty());
    assert!(state.selection_capture.is_none());
}

#[test]
fn streaming_surface_cannot_erase_retained_snapshot() {
    let (mut state, mut mouse) = ready();
    state.config.copy_on_select = false;
    let id = capture_id(&state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]));
    state.handle_endpoint_result("boot-1", &id, Ok(captured()));
    mouse.kind = MouseEventKind::Drag(MouseButton::Left);
    mouse.column += 2;
    state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]);
    let mut next = surface();
    next.surface_revision += 1;
    next.panes[0].content_revision = 2;
    next.panes[0].alternate_screen_active = true;
    next.frame.cells[0].symbol = "X".into();
    state.set_pane_surface(next);
    let frame = state.compose(106, 20).unwrap();
    let hit = &state.hits.panes[0];
    assert_eq!(
        frame.cells[usize::from(hit.inner_rect.y) * usize::from(frame.width)
            + usize::from(hit.inner_rect.x)]
        .symbol,
        "L"
    );
    assert_eq!(
        state.pane_surface.as_ref().unwrap().frame.cells[0].symbol,
        "X"
    );
    mouse.kind = MouseEventKind::Up(MouseButton::Left);
    assert!(state
        .handle_raw_events(vec![RawInputEvent::Mouse(mouse)])
        .actions
        .is_empty());
    let mut outcome = ClientShellInput::default();
    state.request_selection_copy(&mut outcome, true);
    assert!(
        matches!(&outcome.actions[..], [ClientShellAction::Endpoint { request, .. }] if matches!(request.method, Method::PaneTextSnapshotSelection(_)))
    );
}

#[test]
fn cancelled_capture_only_releases_late_token() {
    let (mut state, mouse) = ready();
    let id = capture_id(&state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]));
    state.reset_endpoint_projection();
    assert!(state
        .handle_endpoint_result("boot-1", &id, Ok(captured()))
        .1
        .is_empty());
    assert!(state.selection_capture.is_none());
    assert!(state.selection.is_none());
    assert_eq!(state.selection_releases.len(), 1);
}

#[test]
fn frozen_wheel_never_scrolls_live_terminal_and_resize_cancels() {
    let (mut state, mut mouse) = ready();
    let id = capture_id(&state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]));
    state.handle_endpoint_result("boot-1", &id, Ok(captured()));
    mouse.kind = MouseEventKind::ScrollUp;
    let outcome = state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]);
    assert!(outcome.actions.is_empty());
    state.compose(80, 24).unwrap();
    assert!(state.selection_capture.is_none());
    assert_eq!(state.selection_releases.len(), 1);
}

#[test]
fn plain_click_discards_capture_without_copy() {
    let (mut state, mut mouse) = ready();
    let id = capture_id(&state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]));
    mouse.kind = MouseEventKind::Up(MouseButton::Left);
    state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]);
    let (_, actions) = state.handle_endpoint_result("boot-1", &id, Ok(captured()));
    assert!(actions.is_empty());
    assert!(state.selection_capture.is_none());
}

#[test]
fn changed_capture_after_release_requires_explicit_copy() {
    let (mut state, mut mouse) = ready();
    let id = capture_id(&state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]));
    mouse.kind = MouseEventKind::Drag(MouseButton::Left);
    mouse.column += 2;
    state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]);
    mouse.kind = MouseEventKind::Up(MouseButton::Left);
    state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]);
    let mut result = captured();
    if let ResponseResult::PaneTextSnapshot { text, .. } = &mut result {
        text.rows[0].cells[0].text = "X".into();
        text.content_revision = 2;
    }
    let (_, actions) = state.handle_endpoint_result("boot-1", &id, Ok(result));
    assert!(!actions.iter().any(|action| matches!(action, ClientShellAction::Endpoint { request, .. } if matches!(request.method, Method::PaneTextSnapshotSelection(_)))));
    assert!(state.selection_capture.is_some());
    assert!(state.endpoint_error.is_some());
    let mut outcome = ClientShellInput::default();
    state.copy_frozen_selection(&mut outcome);
    assert!(
        matches!(&outcome.actions[..], [ClientShellAction::Endpoint { request, .. }] if matches!(request.method, Method::PaneTextSnapshotSelection(_)))
    );
}

#[test]
fn malformed_capture_cannot_mix_live_cells_or_overflow_coordinates() {
    for overflow in [false, true] {
        let (mut state, mouse) = ready();
        let id = capture_id(&state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]));
        let mut result = captured();
        if let ResponseResult::PaneTextSnapshot { text, .. } = &mut result {
            if overflow {
                text.viewport_start = u32::MAX;
                text.row_origin = u32::MAX;
            } else {
                text.rows[0].cells.clear();
            }
        }
        assert!(state
            .handle_endpoint_result("boot-1", &id, Ok(result))
            .1
            .is_empty());
        assert!(state.selection_capture.is_none());
        assert_eq!(state.selection_releases.len(), 1);
    }
}

#[test]
fn retained_range_survives_scrolling_and_internal_resize_cancels_copy() {
    let (mut state, mut mouse) = ready();
    state.config.copy_on_select = false;
    let id = capture_id(&state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]));
    let mut result = captured();
    if let ResponseResult::PaneTextSnapshot { text, .. } = &mut result {
        text.rows.extend(text.rows.clone());
        text.rows.extend(text.rows.clone());
        text.range_end = 8;
        text.total_rows = 8;
        text.viewport_start = 4;
    }
    state.handle_endpoint_result("boot-1", &id, Ok(result));
    mouse.kind = MouseEventKind::Drag(MouseButton::Left);
    mouse.column += 2;
    state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]);
    mouse.kind = MouseEventKind::Up(MouseButton::Left);
    state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]);
    let range = state.selection.as_ref().unwrap().ordered_cells();
    mouse.kind = MouseEventKind::ScrollUp;
    state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]);
    assert_eq!(state.selection.as_ref().unwrap().ordered_cells(), range);
    let mut next = surface();
    next.surface_revision += 1;
    next.panes[0].inner_rect.width = 3;
    state.set_pane_surface(next);
    let mut outcome = ClientShellInput::default();
    state.copy_frozen_selection(&mut outcome);
    assert!(outcome.actions.is_empty());
    assert!(state.selection_capture.is_none());
}

#[test]
fn release_during_paging_seals_range_before_later_browsing() {
    let (mut state, mut mouse) = ready();
    state.config.copy_on_select = false;
    let id = capture_id(&state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]));
    let mut initial = captured();
    if let ResponseResult::PaneTextSnapshot { text, .. } = &mut initial {
        text.row_origin = 4;
        text.viewport_start = 4;
        text.range_start = 0;
        text.range_end = 8;
        text.total_rows = 8;
    }
    state.handle_endpoint_result("boot-1", &id, Ok(initial));
    mouse.kind = MouseEventKind::Drag(MouseButton::Left);
    mouse.column += 2;
    state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]);
    mouse.kind = MouseEventKind::ScrollUp;
    let page = state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]);
    let request = page
        .actions
        .iter()
        .find_map(|action| match action {
            ClientShellAction::Endpoint { request, .. }
                if matches!(request.method, Method::PaneTextSnapshotRead(_)) =>
            {
                Some(request)
            }
            _ => None,
        })
        .unwrap();
    mouse.kind = MouseEventKind::Up(MouseButton::Left);
    state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]);
    mouse.kind = MouseEventKind::ScrollUp;
    state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]);
    let mut page = captured();
    if let ResponseResult::PaneTextSnapshot { text, .. } = &mut page {
        text.rows.extend(text.rows.clone());
        text.viewport_start = 4;
        text.range_end = 8;
        text.total_rows = 8;
    }
    state.handle_endpoint_result("boot-1", &request.id, Ok(page));
    let mut copy = ClientShellInput::default();
    state.copy_frozen_selection(&mut copy);
    assert!(copy.actions.iter().any(|action| matches!(action,
        ClientShellAction::Endpoint { request, .. } if matches!(&request.method,
            Method::PaneTextSnapshotSelection(params) if params.anchor.row == 1 && params.anchor.col == 2
                && params.cursor.row == 4 && params.cursor.col == 0))));
}

#[test]
fn parked_copy_mode_does_not_cancel_selection_in_another_pane() {
    let (mut state, mut mouse) = ready();
    let mut initial = surface();
    initial.panes[0].scroll = Some(crate::protocol::PaneSurfaceScrollMetrics {
        offset_from_bottom: 0,
        max_offset_from_bottom: 10,
        viewport_rows: 2,
    });
    state.set_pane_surface(initial);
    state.compose(106, 20).unwrap();
    state.record_binding(
        crate::input::KeybindMatch::Action(crate::input::KeybindAction::CopyMode),
        &mut ClientShellInput::default(),
    );
    assert!(state.copy_mode.is_some());
    let mut next = snapshot();
    next.focused_pane_id = Some("pane_2".into());
    next.panes[0].focused = false;
    let mut other = next.panes[0].clone();
    other.pane_id = "pane_2".into();
    other.focused = true;
    next.panes.push(other);
    state.set_snapshot(Box::new(next));
    let mut other = surface();
    other.panes[0].pane_id = "pane_2".into();
    state.set_pane_surface(other);
    state.compose(106, 20).unwrap();
    assert_eq!(state.mode, ClientShellMode::Terminal);
    let id = capture_id(&state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]));
    let mut result = captured();
    if let ResponseResult::PaneTextSnapshot { pane_id, .. } = &mut result {
        *pane_id = "pane_2".into();
    }
    state.handle_endpoint_result("boot-1", &id, Ok(result));
    mouse.kind = MouseEventKind::Drag(MouseButton::Left);
    mouse.column += 2;
    state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]);
    state.tick_frozen_selection(std::time::Instant::now(), &mut ClientShellInput::default());
    assert_eq!(state.copy_mode.as_ref().unwrap().pane_id, "pane_1");
    assert!(state.selection_capture.is_some());
    mouse.kind = MouseEventKind::Up(MouseButton::Left);
    let outcome = state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]);
    assert!(outcome.actions.iter().any(|action| matches!(action,
        ClientShellAction::Endpoint { request, .. } if matches!(request.method, Method::PaneTextSnapshotSelection(_)))));
}

#[test]
fn local_workbench_focus_waits_for_server_confirmation_during_selection() {
    use crate::client::shell::workbench::View;
    let (mut state, mouse) = ready();
    let mut old = snapshot();
    let mut other = old.panes[0].clone();
    other.pane_id = "pane_2".into();
    other.focused = false;
    old.panes.push(other);
    state.set_snapshot(Box::new(old.clone()));
    state.workbench.enabled = true;
    state.workbench.dock.reconcile_workspace_tabs(
        &["tab_1".into()],
        &["tab_1".into()],
        Some("tab_1"),
    );
    let mut other = surface();
    other.panes[0].pane_id = "pane_2".into();
    state.workbench.views.insert(
        "1".into(),
        View {
            tab: "tab_1".into(),
            surface: other,
            graphics: Default::default(),
        },
    );
    assert_eq!(state.focused_pane_id().as_deref(), Some("pane_2"));
    let mut hit = state.hits.panes[0].clone();
    hit.pane_id = "pane_2".into();
    let mut outcome = ClientShellInput::default();
    assert!(state.begin_frozen_selection(&hit, mouse, 1, &mut outcome));
    let id = capture_id(&outcome);
    assert!(!state.selection_capture.as_ref().unwrap().focus_confirmed);
    old.revision += 1;
    state.set_snapshot(Box::new(old));
    assert!(state.selection_capture.is_some());
    let mut result = captured();
    if let ResponseResult::PaneTextSnapshot { pane_id, .. } = &mut result {
        *pane_id = "pane_2".into();
    }
    state.handle_endpoint_result("boot-1", &id, Ok(result));
    let mouse = MouseEvent {
        kind: MouseEventKind::Drag(MouseButton::Left),
        column: mouse.column + 2,
        ..mouse
    };
    state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]);
    let released = state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Up(MouseButton::Left),
        ..mouse
    })]);
    assert!(released.actions.iter().any(|action| matches!(action,
        ClientShellAction::Endpoint { request, .. } if matches!(request.method, Method::PaneTextSnapshotSelection(_)))));
}

/// 文档终审 D7：选区与阅读快照的提示以前写死中文，英文界面也显示中文。改走
/// i18n：英文界面下这几处提示按表给出、不含 CJK 字符。
#[test]
fn selection_notices_follow_the_interface_language() {
    let _guard = crate::i18n::lang_guard(crate::i18n::Lang::En);
    let t = &crate::i18n::texts().runtime;
    let error = |state: &ClientShellState| state.endpoint_error.clone().unwrap_or_default();

    // 捕获完成前画面已变：释放后提示先确认再复制。
    let (mut state, mut mouse) = ready();
    let id = capture_id(&state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]));
    mouse.kind = MouseEventKind::Drag(MouseButton::Left);
    mouse.column += 2;
    state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]);
    mouse.kind = MouseEventKind::Up(MouseButton::Left);
    state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]);
    let mut result = captured();
    if let ResponseResult::PaneTextSnapshot { text, .. } = &mut result {
        text.rows[0].cells[0].text = "X".into();
        text.content_revision = 2;
    }
    state.handle_endpoint_result("boot-1", &id, Ok(result));
    assert_eq!(error(&state), t.selection_changed_before_copy);
    assert!(!crate::i18n::has_cjk(&error(&state)), "{}", error(&state));

    // 捕获响应既不是快照、也没带错误说明：兜底提示。
    let (mut state, mouse) = ready();
    let id = capture_id(&state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]));
    state.handle_endpoint_result(
        "boot-1",
        &id,
        Ok(ResponseResult::PaneTextSnapshotReleased {
            snapshot_id: "frozen-1".into(),
        }),
    );
    assert_eq!(error(&state), t.selection_capture_failed);
    assert!(!crate::i18n::has_cjk(&error(&state)), "{}", error(&state));

    // 阅读快照迟迟不到：15 秒后超时。
    let (mut state, mouse) = ready();
    capture_id(&state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]));
    state.tick_frozen_selection(
        std::time::Instant::now() + std::time::Duration::from_secs(16),
        &mut ClientShellInput::default(),
    );
    assert_eq!(error(&state), t.selection_timed_out);
    assert!(!crate::i18n::has_cjk(&error(&state)), "{}", error(&state));
}

#[test]
fn frozen_preview_blanks_a_wide_cell_cut_at_the_pane_edge() {
    let (mut state, mouse) = ready();
    state.config.copy_on_select = false;
    let id = capture_id(&state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]));
    // 备用屏缩窄后，第 2 行最后一列只剩「字」的首格（宽度 2、没有尾格）；第 1 行的「中」完整。
    let cell = |text: &str, width: u8| FrozenCell {
        text: text.into(),
        fg: 0,
        bg: 0,
        modifier: 0,
        width,
        hyperlink: None,
    };
    let mut result = captured();
    let ResponseResult::PaneTextSnapshot { text, .. } = &mut result else {
        panic!("captured() 应返回文本快照");
    };
    text.rows[0].cells = vec![cell("中", 2), cell(" ", 0), cell("V", 1), cell("E", 1)];
    text.rows[1].cells = vec![cell("P", 1), cell("A", 1), cell("N", 1), cell("字", 2)];
    state.handle_endpoint_result("boot-1", &id, Ok(result));

    let frame = state.compose(106, 20).unwrap();
    let hit = &state.hits.panes[0];
    let symbol = |x: u16, y: u16| {
        frame.cells[usize::from(hit.inner_rect.y + y) * usize::from(frame.width)
            + usize::from(hit.inner_rect.x + x)]
        .symbol
        .to_string()
    };
    assert_eq!(symbol(0, 0), "中");
    assert_eq!(symbol(3, 0), "E");
    // 放不下的首格画成空白，2 宽字形不越出窗格。
    assert_eq!(symbol(3, 1), " ");
}

#[test]
fn frozen_preview_blanks_a_two_wide_grapheme_in_a_narrow_last_cell() {
    let (mut state, mouse) = ready();
    state.config.copy_on_select = false;
    let id = capture_id(&state.handle_raw_events(vec![RawInputEvent::Mouse(mouse)]));
    // 关闭 2027 后 VS16 挂在窄格上：「⚠️」「⌨️」宽度记 1，符号却是 2 宽字素。
    let cell = |text: &str| FrozenCell {
        text: text.into(),
        fg: 0,
        bg: 0,
        modifier: 0,
        width: 1,
        hyperlink: None,
    };
    let mut result = captured();
    let ResponseResult::PaneTextSnapshot { text, .. } = &mut result else {
        panic!("captured() 应返回文本快照");
    };
    text.rows[0].cells = vec![cell("L"), cell("⌨\u{fe0f}"), cell("V"), cell("⚠\u{fe0f}")];
    state.handle_endpoint_result("boot-1", &id, Ok(result));

    let frame = state.compose(106, 20).unwrap();
    let hit = &state.hits.panes[0];
    let symbol = |x: u16| {
        frame.cells[usize::from(hit.inner_rect.y) * usize::from(frame.width)
            + usize::from(hit.inner_rect.x + x)]
        .symbol
        .to_string()
    };
    // 行中间维持原样，最后一列画空白，不越出窗格。
    assert_eq!(symbol(1), "⌨\u{fe0f}");
    assert_eq!(symbol(3), " ");
}
