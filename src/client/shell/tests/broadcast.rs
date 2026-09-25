use super::*;
use crate::client::endpoint::{
    BroadcastSet, BroadcastTarget, ClientEndpointId, ClientEndpointStatus, ProfileId,
    SavedSshEndpoint,
};
use crossterm::event::KeyCode;

fn profile(label: &str, target: &str, seed: &str) -> SavedSshEndpoint {
    let hex = format!("{seed:0>32}")
        .chars()
        .map(|ch| if ch.is_ascii_hexdigit() { ch } else { 'a' })
        .take(32)
        .collect::<String>();
    SavedSshEndpoint {
        id: ProfileId::parse(hex).expect("hex profile id"),
        label: label.into(),
        target: target.into(),
        ..SavedSshEndpoint::new(label, target, "default").expect("valid profile")
    }
}

fn state_with_profiles(profiles: &[SavedSshEndpoint]) -> ClientShellState {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_endpoint_catalog(profiles);
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    state
}

fn online_remote(state: &mut ClientShellState, profile: &SavedSshEndpoint, pane_id: &str) {
    let endpoint_id = ClientEndpointId::Ssh(profile.id.clone());
    let mut remote_snapshot = snapshot();
    remote_snapshot.boot_id = "boot-remote".into();
    remote_snapshot.focused_pane_id = Some(pane_id.into());
    remote_snapshot.panes[0].pane_id = pane_id.into();
    state.cache_endpoint_snapshot(&endpoint_id, Box::new(remote_snapshot));
    state.set_endpoint_status(&endpoint_id, ClientEndpointStatus::Online);
}

fn with_temp_state_home(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "herdr-broadcast-ui-test-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp state home");
    // Safety: nextest isolates every test in its own process, so mutating the
    // process environment here cannot race other tests.
    unsafe { std::env::set_var("XDG_STATE_HOME", &dir) };
    dir
}

fn key(code: KeyCode) -> crate::input::TerminalKey {
    crate::input::TerminalKey::new(code, crossterm::event::KeyModifiers::empty())
}

fn broadcast_set(pairs: &[(Option<ProfileId>, &str)], enabled: bool) -> BroadcastSet {
    let mut set = BroadcastSet::default();
    set.enabled = enabled;
    for (machine, pane_id) in pairs {
        set.add_target(BroadcastTarget {
            machine: machine.clone(),
            pane_id: (*pane_id).into(),
        })
        .expect("valid target");
    }
    set
}

#[test]
fn broadcast_mirror_follows_external_file_changes() {
    let dir = with_temp_state_home("watcher");
    let build = profile("Build", "build.example", "44");
    let mut state = state_with_profiles(std::slice::from_ref(&build));
    assert!(state.broadcast_indicator_count().is_none());

    // An external (CLI) write lands in the mirror, the mode-bar badge, and
    // the fan-out target list.
    broadcast_set(&[(None, "pane_1")], true)
        .store()
        .expect("external store");
    assert!(state.refresh_broadcast_mirror());
    assert_eq!(state.broadcast_indicator_count(), Some(1));
    assert_eq!(state.broadcast.targets().len(), 1);

    // A second external change refreshes the same surfaces.
    broadcast_set(
        &[(None, "pane_1"), (Some(build.id.clone()), "pane_r1")],
        true,
    )
    .store()
    .expect("external store");
    assert!(state.refresh_broadcast_mirror());
    assert_eq!(state.broadcast_indicator_count(), Some(2));
    assert_eq!(state.broadcast.targets()[1].pane_id, "pane_r1");

    // No on-disk change: the mirror reports no change.
    assert!(!state.refresh_broadcast_mirror());

    // An unreadable file keeps the last good mirror.
    std::fs::write(
        crate::client::endpoint::broadcast_path(),
        b"{ definitely not json",
    )
    .expect("corrupt the file");
    assert!(!state.refresh_broadcast_mirror());
    assert_eq!(state.broadcast_indicator_count(), Some(2));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn broadcast_mirror_refresh_keeps_overlay_editing_state_and_local_writes_win() {
    let dir = with_temp_state_home("watcher-edit");
    let mut state = state_with_profiles(&[]);
    broadcast_set(&[(None, "pane_1")], true)
        .store()
        .expect("store");
    state.open_broadcast_overlay();
    assert_eq!(state.broadcast.targets().len(), 1);

    // A local overlay edit stores its own version first, so the watcher's
    // re-read finds exactly that version: no regression.
    state.handle_raw_events(vec![RawInputEvent::Key(key(KeyCode::Char('g')))]);
    assert!(!state.broadcast.enabled);
    assert!(!state.refresh_broadcast_mirror());
    assert!(!state.broadcast.enabled);
    assert_eq!(state.broadcast.targets().len(), 1);

    // An external change while the overlay is being edited updates the
    // mirror but leaves the transient overlay state alone.
    if let Some(ClientShellOverlay::Broadcast(overlay)) = state.overlay.as_mut() {
        overlay.view = super::super::broadcast::ClientBroadcastView::PickMachine;
        overlay.selected = 0;
        overlay.message = Some("editing".into());
    }
    broadcast_set(&[(None, "pane_9")], true)
        .store()
        .expect("external store");
    assert!(state.refresh_broadcast_mirror());
    assert!(state.broadcast.enabled);
    assert_eq!(state.broadcast.targets()[0].pane_id, "pane_9");
    let Some(ClientShellOverlay::Broadcast(overlay)) = state.overlay.as_ref() else {
        panic!("broadcast overlay still open");
    };
    assert!(matches!(
        overlay.view,
        super::super::broadcast::ClientBroadcastView::PickMachine
    ));
    assert_eq!(overlay.message.as_deref(), Some("editing"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn broadcast_overlay_defaults_to_disabled_empty_and_gate_persists() {
    let dir = with_temp_state_home("gate");
    let mut state = state_with_profiles(&[]);
    state.open_broadcast_overlay();
    assert!(matches!(
        state.overlay,
        Some(ClientShellOverlay::Broadcast(
            super::super::broadcast::ClientBroadcastOverlay {
                view: super::super::broadcast::ClientBroadcastView::List,
                ..
            }
        ))
    ));
    assert!(!state.broadcast.enabled);
    assert!(state.broadcast.is_empty());

    state.handle_raw_events(vec![RawInputEvent::Key(key(KeyCode::Char('g')))]);
    assert!(state.broadcast.enabled);
    assert!(BroadcastSet::load().expect("stored set").enabled);

    // An empty set still blocks the fan-out indicator.
    assert!(state.broadcast_indicator_count().is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn broadcast_picker_registers_one_pane_per_endpoint() {
    let dir = with_temp_state_home("picker");
    let build = profile("Build", "build.example", "40");
    let mut state = state_with_profiles(std::slice::from_ref(&build));
    online_remote(&mut state, &build, "pane_r1");

    state.open_broadcast_overlay();
    // a → pick machine; Build is the second row (Local leads).
    state.handle_raw_events(vec![RawInputEvent::Key(key(KeyCode::Char('a')))]);
    state.handle_raw_events(vec![RawInputEvent::Key(key(KeyCode::Down))]);
    state.handle_raw_events(vec![RawInputEvent::Key(key(KeyCode::Enter))]);
    assert!(matches!(
        state.overlay,
        Some(ClientShellOverlay::Broadcast(
            super::super::broadcast::ClientBroadcastOverlay {
                view: super::super::broadcast::ClientBroadcastView::PickPane { .. },
                ..
            }
        ))
    ));
    state.handle_raw_events(vec![RawInputEvent::Key(key(KeyCode::Enter))]);

    let set = BroadcastSet::load().expect("stored set");
    assert_eq!(set.targets().len(), 1);
    assert_eq!(set.targets()[0].pane_id, "pane_r1");
    assert_eq!(set.targets()[0].machine, Some(build.id.clone()));

    // The registered machine leaves the candidate list on the next add.
    state.handle_raw_events(vec![RawInputEvent::Key(key(KeyCode::Char('a')))]);
    let candidates = super::super::broadcast::broadcast_machine_candidates(
        &state.broadcast,
        &state.endpoints,
        &state.saved_profiles,
    );
    assert_eq!(candidates.len(), 1);
    assert!(candidates[0].0.is_local());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn broadcast_indicator_renders_in_terminal_mode_bar_only_when_live() {
    let mut state = state_with_profiles(&[]);
    let frame_text = |state: &mut ClientShellState| {
        let frame = state.compose(106, 32).expect("composed frame");
        frame
            .cells
            .chunks(frame.width as usize)
            .map(|row| row.iter().map(|cell| cell.symbol.as_str()).collect())
            .collect::<Vec<String>>()
            .join("\n")
    };
    assert!(!frame_text(&mut state).contains("⇄"));

    state.broadcast = broadcast_set(&[(None, "pane_1")], false);
    assert!(!frame_text(&mut state).contains("⇄"));

    state.broadcast = broadcast_set(&[(None, "pane_1")], true);
    let text = frame_text(&mut state);
    assert!(text.contains("⇄"), "frame: {text}");
    assert_eq!(state.broadcast_indicator_count(), Some(1));
}

#[test]
fn broadcast_text_fans_out_to_every_target_except_the_typed_pane() {
    let build = profile("Build", "build.example", "41");
    let mut state = state_with_profiles(std::slice::from_ref(&build));
    online_remote(&mut state, &build, "pane_r1");
    state.broadcast = broadcast_set(
        &[(None, "pane_1"), (Some(build.id.clone()), "pane_r1")],
        true,
    );

    let mut outcome = ClientShellInput::default();
    state.broadcast_text("ls -la", false, &mut outcome);

    assert_eq!(outcome.actions.len(), 1);
    let ClientShellAction::EndpointRequest {
        endpoint_id,
        request,
        ..
    } = &outcome.actions[0]
    else {
        panic!("expected one endpoint request: {:?}", outcome.actions);
    };
    assert_eq!(endpoint_id, &ClientEndpointId::Ssh(build.id.clone()));
    assert!(matches!(
        &request.method,
        crate::api::schema::Method::PaneSendText(params)
            if params.pane_id == "pane_r1" && params.text == "ls -la"
    ));
    let pending = state
        .pending_requests
        .get(&request.id)
        .expect("pending request");
    assert!(matches!(
        pending.kind,
        super::super::state::PendingEndpointKind::BroadcastSend { .. }
    ));

    // Paste routes through pane.send-input (bracketed paste on the target).
    let mut outcome = ClientShellInput::default();
    state.broadcast_text("multi\nline", true, &mut outcome);
    let ClientShellAction::EndpointRequest { request, .. } = &outcome.actions[0] else {
        panic!("expected endpoint request");
    };
    assert!(matches!(
        &request.method,
        crate::api::schema::Method::PaneSendInput(params)
            if params.pane_id == "pane_r1" && params.text == "multi\nline" && params.keys.is_empty()
    ));

    // Disabled or empty sets short-circuit to zero requests.
    state.broadcast = broadcast_set(&[(None, "pane_1")], false);
    let mut outcome = ClientShellInput::default();
    state.broadcast_text("x", false, &mut outcome);
    assert!(outcome.actions.is_empty());
}

#[test]
fn broadcast_key_fans_out_named_keys_and_skips_releases() {
    let build = profile("Build", "build.example", "42");
    let mut state = state_with_profiles(std::slice::from_ref(&build));
    online_remote(&mut state, &build, "pane_r1");
    state.broadcast = broadcast_set(&[(Some(build.id.clone()), "pane_r1")], true);

    let mut outcome = ClientShellInput::default();
    state.broadcast_key(&key(KeyCode::Enter), &mut outcome);
    assert_eq!(outcome.actions.len(), 1);
    let ClientShellAction::EndpointRequest { request, .. } = &outcome.actions[0] else {
        panic!("expected endpoint request");
    };
    assert!(matches!(
        &request.method,
        crate::api::schema::Method::PaneSendInput(params)
            if params.pane_id == "pane_r1" && params.keys == ["enter"]
    ));

    let release =
        crate::input::TerminalKey::new(KeyCode::Enter, crossterm::event::KeyModifiers::empty())
            .with_kind(crossterm::event::KeyEventKind::Release);
    let mut outcome = ClientShellInput::default();
    state.broadcast_key(&release, &mut outcome);
    assert!(outcome.actions.is_empty());
}

#[test]
fn terminal_input_fans_out_through_the_normal_pane_path() {
    let build = profile("Build", "build.example", "43");
    let mut state = state_with_profiles(std::slice::from_ref(&build));
    online_remote(&mut state, &build, "pane_r1");
    state.broadcast = broadcast_set(
        &[(None, "pane_1"), (Some(build.id.clone()), "pane_r1")],
        true,
    );

    // A text commit in Terminal mode still reaches the focused pane through
    // the wire protocol, and fans out to the remote target as an API request.
    let outcome = state.handle_raw_events(vec![RawInputEvent::Text(
        crate::input::TextCommit::new("ls"),
    )]);
    assert!(outcome.requests.iter().any(|request| matches!(
        request,
        crate::protocol::ClientMessage::ClientShellPaneInput { pane_id, .. } if pane_id == "pane_1"
    )));
    assert_eq!(outcome.actions.len(), 1);
    let ClientShellAction::EndpointRequest {
        endpoint_id,
        request,
        ..
    } = &outcome.actions[0]
    else {
        panic!("expected one endpoint request: {:?}", outcome.actions);
    };
    assert_eq!(endpoint_id, &ClientEndpointId::Ssh(build.id.clone()));
    assert!(matches!(
        &request.method,
        crate::api::schema::Method::PaneSendText(params)
            if params.pane_id == "pane_r1" && params.text == "ls"
    ));

    // A named key takes the same fan-out lane as pane.send-input.
    let outcome = state.handle_raw_events(vec![RawInputEvent::Key(key(KeyCode::Enter))]);
    assert!(outcome.actions.iter().any(|action| matches!(
        action,
        ClientShellAction::EndpointRequest { request, .. }
            if matches!(
                &request.method,
                crate::api::schema::Method::PaneSendInput(params)
                    if params.pane_id == "pane_r1" && params.keys == ["enter"]
            )
    )));

    // Disabled set: the wire path is untouched and nothing fans out.
    state.broadcast = broadcast_set(&[(Some(build.id.clone()), "pane_r1")], false);
    let outcome = state.handle_raw_events(vec![RawInputEvent::Text(
        crate::input::TextCommit::new("ls"),
    )]);
    assert!(outcome.actions.is_empty());
    assert!(outcome.requests.iter().any(|request| matches!(
        request,
        crate::protocol::ClientMessage::ClientShellPaneInput { pane_id, .. } if pane_id == "pane_1"
    )));
}

#[test]
fn broadcast_send_failures_dedupe_per_endpoint_and_success_rearms() {
    let mut state = state_with_profiles(&[]);
    let error = || {
        Some(ClientShellEndpointError {
            code: Some("pane_send_failed".into()),
            message: "pane is gone".into(),
        })
    };
    assert!(state.complete_broadcast_send(&ClientEndpointId::Local, "boot-1", "Build", error()));
    assert!(state.visible_endpoint_notice.is_some());
    // Same endpoint, same failure: deduped while the notice set holds it.
    assert!(!state.complete_broadcast_send(&ClientEndpointId::Local, "boot-1", "Build", error()));
    // A success re-arms reporting for that endpoint.
    assert!(!state.complete_broadcast_send(&ClientEndpointId::Local, "boot-1", "Build", None));
    assert!(state.complete_broadcast_send(&ClientEndpointId::Local, "boot-1", "Build", error()));
}
