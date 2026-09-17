use super::*;
use crate::client::endpoint::{
    ClientEndpointId, ClientEndpointStatus, ProfileId, SavedSshEndpoint,
};
use crate::client::shell::machine_auth_overlay::{
    ClientMachineAuthOverlay, ClientMachineAuthView, MachineAuthButton,
};
use crate::client::shell::machines_overlay::{
    ClientMachineBootstrap, ClientMachinesView, MachineOverlayButton,
};
use crate::remote::{ConnectionErrorKind, HostKeyFingerprint};
use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEventKind};

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

fn frame_text(state: &mut ClientShellState, cols: u16, rows: u16) -> String {
    let frame = state.compose(cols, rows).expect("composed frame");
    frame
        .cells
        .chunks(frame.width as usize)
        .map(|row| row.iter().map(|cell| cell.symbol.as_str()).collect())
        .collect::<Vec<String>>()
        .join("\n")
}

/// Wide CJK glyphs occupy a following blank cell in the frame, so text
/// assertions compare with all whitespace stripped.
fn compact_frame_text(state: &mut ClientShellState, cols: u16, rows: u16) -> String {
    frame_text(state, cols, rows)
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .collect()
}

fn key(code: KeyCode) -> crate::input::TerminalKey {
    crate::input::TerminalKey::new(code, KeyModifiers::empty())
}

fn fingerprint() -> HostKeyFingerprint {
    HostKeyFingerprint {
        key_type: "ssh-ed25519".into(),
        fingerprint: "SHA256:abc123".into(),
    }
}

fn reviewed_key(host: &str, port: u16, keys: &[(&str, &str)]) -> crate::remote::HostKeyReview {
    crate::remote::HostKeyReview {
        target: crate::remote::EffectiveHostKeyTarget {
            host: host.into(),
            port,
            lookup: if port == 22 {
                host.into()
            } else {
                format!("[{host}]:{port}")
            },
            files: vec![std::path::PathBuf::from("/tmp/known_hosts-test")],
            proxied: false,
        },
        keys: keys
            .iter()
            .map(|(kind, fingerprint)| crate::remote::KnownHostKey {
                key_type: (*kind).into(),
                key_line: format!("{kind} AQID"),
                fingerprint: HostKeyFingerprint {
                    key_type: (*kind).into(),
                    fingerprint: (*fingerprint).into(),
                },
            })
            .collect(),
    }
}

fn finish_scan(state: &mut ClientShellState, host: &str, port: u16) {
    state.handle_machine_auth_update(
        MachineAuthUpdate::HostKeyOpFinished {
            ticket: 1,
            op: MachineHostKeyOp::Scan,
            result: Ok(MachineHostKeyOutcome::Scanned(reviewed_key(
                host,
                port,
                &[("ssh-ed25519", "SHA256:abc123")],
            ))),
        },
        &mut ClientShellInput::default(),
    );
}

fn auth_overlay(state: &ClientShellState) -> &ClientMachineAuthOverlay {
    let Some(ClientShellOverlay::MachineAuth(overlay)) = state.overlay.as_ref() else {
        panic!("expected machine auth overlay, got {:?}", state.overlay);
    };
    overlay
}

#[test]
fn unknown_host_key_kind_opens_tofu_dialog_with_fingerprint() {
    let machine = profile("Build", "dev@build.example", "1");
    let mut state = state_with_profiles(std::slice::from_ref(&machine));
    let endpoint_id = ClientEndpointId::Ssh(machine.id.clone());
    state.set_endpoint_connection_error_kind(
        &endpoint_id,
        Some(ConnectionErrorKind::HostKeyUnknown {
            fingerprint: Some(fingerprint()),
        }),
    );

    let mut outcome = ClientShellInput::default();
    assert!(state.open_machine_auth_for_endpoint(&machine.id, &mut outcome));

    // 获取与实际 profile 绑定的公钥，确认后只保存这份记录。
    assert!(outcome.actions.iter().any(|action| matches!(
        action,
        ClientShellAction::MachineHostKeyOp {
            op: MachineHostKeyOp::Scan,
            ..
        }
    )));
    finish_scan(&mut state, "build.example", 22);
    let text = frame_text(&mut state, 90, 30);
    assert!(text.contains("SHA256:abc123"), "frame: {text}");
    assert!(text.contains("ssh-ed25519"), "frame: {text}");
    assert!(text.contains("build.example"), "frame: {text}");

    // Trust & remember pre-collects the key.
    let mut outcome = ClientShellInput::default();
    state.activate_machine_auth_button(MachineAuthButton::TrustRemember, &mut outcome);
    assert!(outcome.actions.iter().any(|action| matches!(
        action,
        ClientShellAction::MachineHostKeyOp {
            op: MachineHostKeyOp::Precollect,
            profile, reviewed: Some((target, key)), ..
        } if profile.target == "dev@build.example" && target.port == 22 && key.fingerprint.fingerprint == "SHA256:abc123"
    )));
}

#[test]
fn trust_once_uses_the_process_local_override_and_closes() {
    let machine = profile("Build", "dev@build.example", "1");
    let mut state = state_with_profiles(std::slice::from_ref(&machine));
    let endpoint_id = ClientEndpointId::Ssh(machine.id.clone());
    state.set_endpoint_connection_error_kind(
        &endpoint_id,
        Some(ConnectionErrorKind::HostKeyUnknown {
            fingerprint: Some(fingerprint()),
        }),
    );
    let mut outcome = ClientShellInput::default();
    assert!(state.open_machine_auth_for_endpoint(&machine.id, &mut outcome));

    finish_scan(&mut state, "build.example", 22);
    let mut outcome = ClientShellInput::default();
    state.activate_machine_auth_button(MachineAuthButton::TrustOnce, &mut outcome);
    assert!(outcome.actions.iter().any(|action| matches!(
        action,
        ClientShellAction::StartMachineInteractiveAuth { profile, pin: Some((target, key)), .. }
            if profile.id == machine.id && target.host == "build.example" && key.fingerprint.fingerprint == "SHA256:abc123"
    )));
    // 仅为本次连接使用临时公钥文件，允许后续密码提示继续由页面服务。
    assert!(matches!(
        state.overlay,
        Some(ClientShellOverlay::MachineAuth(_))
    ));
}

#[test]
fn missing_fingerprint_scans_before_trusting() {
    let machine = profile("Build", "dev@build.example:2222", "1");
    let mut state = state_with_profiles(std::slice::from_ref(&machine));
    state.set_endpoint_connection_error_kind(
        &ClientEndpointId::Ssh(machine.id.clone()),
        Some(ConnectionErrorKind::HostKeyUnknown { fingerprint: None }),
    );

    let mut outcome = ClientShellInput::default();
    assert!(state.open_machine_auth_for_endpoint(&machine.id, &mut outcome));
    assert!(outcome.actions.iter().any(|action| matches!(
        action,
        ClientShellAction::MachineHostKeyOp {
            op: MachineHostKeyOp::Scan,
            profile, ..
        } if profile.target == "dev@build.example:2222"
    )));

    // The scan result fills the fingerprint rows.
    let mut outcome = ClientShellInput::default();
    state.handle_machine_auth_update(
        MachineAuthUpdate::HostKeyOpFinished {
            ticket: 1,
            op: MachineHostKeyOp::Scan,
            result: Ok(MachineHostKeyOutcome::Scanned(reviewed_key(
                "build.example",
                2222,
                &[("ssh-rsa", "SHA256:rsa"), ("ssh-ed25519", "SHA256:ed")],
            ))),
        },
        &mut outcome,
    );
    let text = frame_text(&mut state, 90, 30);
    assert!(text.contains("SHA256:ed"), "frame: {text}");

    // A finished pre-collect marks the dialog complete and triggers a
    // reconnect.
    let mut outcome = ClientShellInput::default();
    state.handle_machine_auth_update(
        MachineAuthUpdate::HostKeyOpFinished {
            ticket: 1,
            op: MachineHostKeyOp::Precollect,
            result: Ok(MachineHostKeyOutcome::Precollected(2)),
        },
        &mut outcome,
    );
    assert!(outcome.actions.iter().any(|action| matches!(
        action,
        ClientShellAction::ReconnectEndpoint { endpoint_id: id }
            if id == &ClientEndpointId::Ssh(machine.id.clone())
    )));
    let text = frame_text(&mut state, 90, 30);
    assert!(!text.contains("trust & remember"), "frame: {text}");
}

#[test]
fn changed_host_key_dialog_is_a_hard_blocker_with_mitm_wording() {
    let machine = profile("Build", "dev@build.example", "1");
    let mut state = state_with_profiles(std::slice::from_ref(&machine));
    state.set_endpoint_connection_error_kind(
        &ClientEndpointId::Ssh(machine.id.clone()),
        Some(ConnectionErrorKind::HostKeyChanged),
    );

    let mut outcome = ClientShellInput::default();
    assert!(state.open_machine_auth_for_endpoint(&machine.id, &mut outcome));
    let text = compact_frame_text(&mut state, 90, 30);
    let t = &crate::i18n::texts().machine_auth;
    let compact = |s: &str| {
        s.chars()
            .filter(|ch| !ch.is_whitespace())
            .collect::<String>()
    };
    assert!(text.contains(&compact(t.changed_title)), "frame: {text}");
    assert!(
        text.contains(&compact(t.changed_mitm_hint)),
        "frame: {text}"
    );

    let mut outcome = ClientShellInput::default();
    state.activate_machine_auth_button(MachineAuthButton::RemoveRetry, &mut outcome);
    assert!(outcome.actions.iter().any(|action| matches!(
        action,
        ClientShellAction::MachineHostKeyOp {
            op: MachineHostKeyOp::Remove,
            profile, ..
        } if profile.target == "dev@build.example"
    )));
}

#[test]
fn auth_guide_starts_interactive_auth_only_on_approval() {
    let machine = profile("Build", "dev@build.example", "1");
    let mut state = state_with_profiles(std::slice::from_ref(&machine));
    state.set_endpoint_connection_error_kind(
        &ClientEndpointId::Ssh(machine.id.clone()),
        Some(ConnectionErrorKind::AuthRequired {
            methods: vec!["publickey".into(), "password".into()],
            identity_file: Some("~/.ssh/build".into()),
        }),
    );

    let mut outcome = ClientShellInput::default();
    assert!(state.open_machine_auth_for_endpoint(&machine.id, &mut outcome));
    // Nothing interactive happens before the explicit approval click.
    assert!(outcome.actions.is_empty());
    let text = frame_text(&mut state, 90, 30);
    assert!(text.contains("publickey, password"), "frame: {text}");
    assert!(text.contains("~/.ssh/build"), "frame: {text}");

    let mut outcome = ClientShellInput::default();
    state.activate_machine_auth_button(MachineAuthButton::InteractiveAuth, &mut outcome);
    assert!(outcome.actions.iter().any(|action| matches!(
        action,
        ClientShellAction::StartMachineInteractiveAuth { ticket: 1, profile, .. }
            if profile.target == "dev@build.example"
    )));

    // A finished attempt reconnects the saved machine and offers the close
    // state.
    let mut outcome = ClientShellInput::default();
    state.handle_machine_auth_update(
        MachineAuthUpdate::InteractiveFinished {
            ticket: 1,
            result: Ok(()),
        },
        &mut outcome,
    );
    assert!(outcome.actions.iter().any(|action| matches!(
        action,
        ClientShellAction::ReconnectEndpoint { endpoint_id: id }
            if id == &ClientEndpointId::Ssh(machine.id.clone())
    )));
    let text = compact_frame_text(&mut state, 90, 30);
    let expected: String = crate::i18n::texts()
        .machine_auth
        .auth_success
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .collect();
    assert!(text.contains(&expected), "frame: {text}");

    // A failed attempt surfaces the error and never reconnects.
    let mut outcome = ClientShellInput::default();
    state.handle_machine_auth_update(
        MachineAuthUpdate::InteractiveFinished {
            ticket: 1,
            result: Err("permission denied".into()),
        },
        &mut outcome,
    );
    assert!(!outcome
        .actions
        .iter()
        .any(|action| matches!(action, ClientShellAction::ReconnectEndpoint { .. })));
    let text = frame_text(&mut state, 90, 30);
    assert!(text.contains("permission denied"), "frame: {text}");
}

#[test]
fn password_prompt_masks_input_and_answers_the_session() {
    let machine = profile("Build", "dev@build.example", "1");
    let mut state = state_with_profiles(std::slice::from_ref(&machine));
    state.set_endpoint_connection_error_kind(
        &ClientEndpointId::Ssh(machine.id.clone()),
        Some(ConnectionErrorKind::AuthRequired {
            methods: vec!["password".into()],
            identity_file: None,
        }),
    );
    let mut outcome = ClientShellInput::default();
    assert!(state.open_machine_auth_for_endpoint(&machine.id, &mut outcome));
    let mut outcome = ClientShellInput::default();
    state.activate_machine_auth_button(MachineAuthButton::InteractiveAuth, &mut outcome);

    // The askpass prompt switches the dialog to the masked password view.
    assert!(state.show_machine_auth_prompt(1, "dev@build.example's password:"));
    // A foreign ticket cannot claim the dialog.
    assert!(!state.show_machine_auth_prompt(99, "other prompt:"));
    state.insert_machine_auth_text("s3cret!");
    let text = frame_text(&mut state, 90, 30);
    assert!(!text.contains("s3cret!"), "frame: {text}");
    assert!(text.contains("•••••••"), "frame: {text}");
    assert!(
        text.contains("dev@build.example's password:"),
        "frame: {text}"
    );

    // Submitting answers the session and returns to the guide; the editor is
    // cleared before the answer travels on.
    let mut outcome = ClientShellInput::default();
    state.route_machine_auth_key(&key(KeyCode::Enter), &mut outcome);
    assert!(outcome.actions.iter().any(|action| matches!(
        action,
        ClientShellAction::AnswerMachineAuthPrompt {
            ticket: 1,
            answer: Some(answer),
        } if answer == "s3cret!"
    )));
    assert!(matches!(
        auth_overlay(&state).view.as_ref(),
        Some(ClientMachineAuthView::AuthGuide(_))
    ));
}

#[test]
fn declining_a_password_cancels_the_session() {
    let machine = profile("Build", "dev@build.example", "1");
    let mut state = state_with_profiles(std::slice::from_ref(&machine));
    state.set_endpoint_connection_error_kind(
        &ClientEndpointId::Ssh(machine.id.clone()),
        Some(ConnectionErrorKind::AuthRequired {
            methods: vec!["password".into()],
            identity_file: None,
        }),
    );
    let mut outcome = ClientShellInput::default();
    assert!(state.open_machine_auth_for_endpoint(&machine.id, &mut outcome));
    let mut outcome = ClientShellInput::default();
    state.activate_machine_auth_button(MachineAuthButton::InteractiveAuth, &mut outcome);
    assert!(state.show_machine_auth_prompt(1, "password:"));
    state.insert_machine_auth_text("draft");
    let mut outcome = ClientShellInput::default();
    state.route_machine_auth_key(&key(KeyCode::Esc), &mut outcome);
    assert!(outcome.actions.iter().any(|action| matches!(
        action,
        ClientShellAction::AnswerMachineAuthPrompt {
            ticket: 1,
            answer: None,
        }
    )));
    assert!(outcome.actions.iter().any(|action| matches!(
        action,
        ClientShellAction::CancelMachineInteractiveAuth { ticket: 1 }
    )));
}

#[test]
fn wizard_host_key_review_scans_and_skips_trust_once() {
    let mut state = state_with_profiles(&[]);
    let mut outcome = ClientShellInput::default();
    state.open_machine_host_key_review(
        Box::new(profile("Stage", "stage.example", "3")),
        &mut outcome,
    );
    assert!(outcome.actions.iter().any(|action| matches!(
        action,
        ClientShellAction::MachineHostKeyOp {
            op: MachineHostKeyOp::Scan,
            profile, ..
        } if profile.target == "stage.example"
    )));

    // Without a saved profile there is no endpoint to trust once: the button
    // row offers trust & remember plus abort only.
    let text = compact_frame_text(&mut state, 90, 30);
    let t = &crate::i18n::texts().machine_auth;
    let compact = |s: &str| {
        s.chars()
            .filter(|ch| !ch.is_whitespace())
            .collect::<String>()
    };
    assert!(
        text.contains(&compact(t.trust_remember_button)),
        "frame: {text}"
    );
    assert!(
        !text.contains(&compact(t.trust_once_button)),
        "frame: {text}"
    );
    let mut outcome = ClientShellInput::default();
    state.activate_machine_auth_button(MachineAuthButton::TrustOnce, &mut outcome);
    assert!(outcome.actions.is_empty());
}

#[test]
fn wizard_bootstrap_failure_offers_recovery_entries() {
    let machine = profile("Build", "dev@build.example", "1");
    let mut state = state_with_profiles(&[]);
    state.open_machine_add_form();
    let Some(ClientShellOverlay::Machines(overlay)) = state.overlay.as_mut() else {
        panic!("machines overlay");
    };
    let ClientMachinesView::Form(form) = &mut overlay.view else {
        panic!("form view");
    };
    form.target = crate::client::shell::TextEditor::new("dev@build.example", false);
    form.bootstrap = Some(ClientMachineBootstrap {
        cancel: crate::remote::TaskCancellation::default(),
        ticket: 1,
        step: None,
        failure: Some("Permission denied (publickey)".into()),
    });
    let _ = machine;

    // Keyboard entry starts the guided interactive auth against a throwaway
    // profile.
    let mut outcome = ClientShellInput::default();
    state.route_machines_key(&key(KeyCode::Char('i')), &mut outcome);
    assert!(matches!(
        state.overlay,
        Some(ClientShellOverlay::MachineAuth(_))
    ));
    let mut outcome = ClientShellInput::default();
    state.activate_machine_auth_button(MachineAuthButton::InteractiveAuth, &mut outcome);
    assert!(outcome.actions.iter().any(|action| matches!(
        action,
        ClientShellAction::StartMachineInteractiveAuth { profile, .. }
            if profile.target == "dev@build.example"
    )));
}

#[test]
fn detail_card_shows_structured_failure_and_review_entry() {
    let machine = profile("Build", "dev@build.example", "1");
    let mut state = state_with_profiles(std::slice::from_ref(&machine));
    let endpoint_id = ClientEndpointId::Ssh(machine.id.clone());
    state.set_endpoint_status(&endpoint_id, ClientEndpointStatus::Attention);
    state.set_endpoint_connection_error_kind(
        &endpoint_id,
        Some(ConnectionErrorKind::AuthRequired {
            methods: vec!["publickey".into()],
            identity_file: None,
        }),
    );
    state.open_machines_overlay_for(&machine.id);
    let text = compact_frame_text(&mut state, 106, 32);
    let t = &crate::i18n::texts().machine_auth;
    let compact = |s: &str| {
        s.chars()
            .filter(|ch| !ch.is_whitespace())
            .collect::<String>()
    };
    assert!(
        text.contains(&compact(t.kind_auth_required)),
        "frame: {text}"
    );
    assert!(
        text.contains(&compact(t.next_auth_required)),
        "frame: {text}"
    );
    assert!(text.contains(&compact(t.review_button)), "frame: {text}");

    // The review button opens the matching recovery dialog.
    let mut outcome = ClientShellInput::default();
    state.activate_machine_button(MachineOverlayButton::ReviewIssue, &mut outcome);
    assert!(matches!(
        state.overlay,
        Some(ClientShellOverlay::MachineAuth(_))
    ));
}

#[test]
fn reconnect_banner_counts_attempts_and_registers_actions() {
    let machine = profile("Build", "dev@build.example", "1");
    let mut state = state_with_profiles(std::slice::from_ref(&machine));
    let endpoint_id = ClientEndpointId::Ssh(machine.id.clone());
    state.set_endpoint_status(&endpoint_id, ClientEndpointStatus::Reconnecting);
    state.active_endpoint_id = endpoint_id.clone();
    // One recorded attempt: the 500ms first backoff keeps the displayed
    // countdown at 0s for the whole test.
    state.note_endpoint_reconnect_attempt(&endpoint_id, std::time::Instant::now());
    assert_eq!(
        state
            .endpoint_reconnect_progress(&endpoint_id)
            .map(|progress| progress.attempts),
        Some(1)
    );

    let compact = compact_frame_text(&mut state, 106, 32);
    let expected: String = crate::i18n::fill(
        crate::i18n::texts().machine_auth.banner_reconnecting_fmt,
        &[("label", "Build"), ("attempt", "1"), ("seconds", "0")],
    )
    .chars()
    .filter(|ch| !ch.is_whitespace())
    .collect();
    assert!(compact.contains(&expected), "frame: {compact}");
    assert!(!state.hits.lifecycle_banner_retry.is_empty());
    assert!(!state.hits.lifecycle_banner_give_up.is_empty());

    // Clicking retry asks for an immediate reconnect.
    let retry = state.hits.lifecycle_banner_retry;
    let mut outcome = ClientShellInput::default();
    state.handle_mouse(
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: retry.x,
            row: retry.y,
            modifiers: KeyModifiers::empty(),
        },
        &mut outcome,
    );
    assert!(outcome.actions.iter().any(|action| matches!(
        action,
        ClientShellAction::ReconnectEndpoint { endpoint_id: id }
            if id == &endpoint_id
    )));

    state.clear_endpoint_reconnect_progress(&endpoint_id);
    let compact = compact_frame_text(&mut state, 106, 32);
    let expected: String = crate::i18n::texts()
        .endpoint
        .st_reconnecting
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .collect();
    assert!(compact.contains(&expected), "frame: {compact}");
}

#[test]
fn retry_delay_mirror_matches_the_supervisor_ladder() {
    let delay = super::super::endpoints::estimated_retry_delay;
    assert_eq!(delay(1, false), std::time::Duration::from_millis(500));
    assert_eq!(delay(2, false), std::time::Duration::from_secs(1));
    assert_eq!(delay(100, false), std::time::Duration::from_secs(120));
    assert_eq!(delay(100, true), std::time::Duration::from_secs(30));
}
