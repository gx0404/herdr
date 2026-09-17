use super::*;
use crate::api::schema::{Method, ResponseResult};
use crate::terminal::text_snapshot::{FrozenCell, FrozenRow, FrozenText};

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
    state
        .workbench
        .dock
        .reconcile_tabs(&["tab_1".into()], Some("tab_1"));
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
