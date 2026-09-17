use super::*;
use crate::client::endpoint::{
    ClientEndpointId, ClientEndpointStatus, ProfileId, SavedSshEndpoint,
};
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

fn key(code: KeyCode) -> crate::input::TerminalKey {
    crate::input::TerminalKey::new(code, KeyModifiers::empty())
}

#[test]
fn machines_overlay_lists_profiles_with_status_and_filter() {
    let build = profile("Build", "dev@build.example", "1");
    let stage = profile("Stage", "stage.example", "2");
    let mut state = state_with_profiles(&[build.clone(), stage.clone()]);
    let build_id = ClientEndpointId::Ssh(build.id.clone());
    state.set_endpoint_status(&build_id, ClientEndpointStatus::Online);
    state.set_endpoint_server_version(&build_id, Some("0.8.3".into()));

    state.open_machines_overlay();
    let text = frame_text(&mut state, 106, 32);
    assert!(text.contains("Build"), "frame: {text}");
    assert!(text.contains("Stage"), "frame: {text}");
    assert!(text.contains("dev@build.example"), "frame: {text}");
    assert!(text.contains("v0.8.3"), "frame: {text}");
    let connecting = crate::i18n::texts().endpoint.st_connecting;
    let compact: String = text.chars().filter(|ch| !ch.is_whitespace()).collect();
    assert!(compact.contains(connecting), "frame: {text}");

    // Search narrows the list down to the matching machine.
    if let Some(ClientShellOverlay::Machines(overlay)) = state.overlay.as_mut() {
        overlay.search_focused = true;
    }
    for ch in "stag".chars() {
        state.handle_raw_events(vec![RawInputEvent::Key(crate::input::TerminalKey::new(
            KeyCode::Char(ch),
            KeyModifiers::empty(),
        ))]);
    }
    let text = frame_text(&mut state, 106, 32);
    assert!(!text.contains("dev@build.example"), "frame: {text}");
    assert!(text.contains("stage.example"), "frame: {text}");

    // Enter opens the detail card of the filtered row.
    state.handle_raw_events(vec![RawInputEvent::Key(key(KeyCode::Enter))]);
    assert!(matches!(
        state.overlay,
        Some(ClientShellOverlay::Machines(
            super::super::machines_overlay::ClientMachinesOverlay {
                view: super::super::machines_overlay::ClientMachinesView::Detail(_),
                ..
            }
        ))
    ));
}

#[test]
fn machines_overlay_detail_shows_profile_fields_version_and_failure() {
    let mut options_profile = profile("Build", "dev@build.example:2222", "3");
    options_profile.session = "agents".into();
    options_profile.group = Some("prod".into());
    options_profile.tags = vec!["ci".into()];
    options_profile.color = Some("#1a2b3c".into());
    options_profile.user = Some("dev".into());
    options_profile.proxy_jump = vec![crate::client::endpoint::ProxyJumpHop::Target(
        "bastion".into(),
    )];
    let mut state = state_with_profiles(std::slice::from_ref(&options_profile));
    let endpoint_id = ClientEndpointId::Ssh(options_profile.id.clone());
    state.set_endpoint_status(&endpoint_id, ClientEndpointStatus::Attention);
    state.set_endpoint_status_detail(&endpoint_id, Some("Host key verification failed".into()));
    state.set_endpoint_server_version(&endpoint_id, Some("0.8.2".into()));

    state.open_machines_overlay_for(&options_profile.id);
    let text = frame_text(&mut state, 106, 36);
    for needle in [
        "Build",
        "dev@build.example:2222",
        "agents",
        "prod",
        "ci",
        "#1a2b3c",
        "2222",
        "dev",
        "bastion",
        "v0.8.2",
        "Host key verification failed",
        "herdr --remote",
    ] {
        assert!(text.contains(needle), "missing {needle} in frame: {text}");
    }
    let attention = crate::i18n::texts().endpoint.st_attention;
    let compact: String = text.chars().filter(|ch| !ch.is_whitespace()).collect();
    assert!(compact.contains(attention), "frame: {text}");
}

#[test]
fn machine_context_menu_items_track_endpoint_state() {
    let mut state = state_with_profiles(&[profile("Build", "build", "4")]);
    let local_items = {
        state.open_machine_context_menu(&ClientEndpointId::Local, 4, 4);
        let Some(ClientShellOverlay::ContextMenu(menu)) = state.overlay.as_ref() else {
            panic!("context menu");
        };
        menu.items().len()
    };
    assert_eq!(local_items, 1);

    let build_id =
        ClientEndpointId::Ssh(ProfileId::parse("00000000000000000000000000000004").unwrap());
    state.open_machine_context_menu(&build_id, 4, 4);
    let Some(ClientShellOverlay::ContextMenu(menu)) = state.overlay.as_ref() else {
        panic!("context menu");
    };
    let labels = menu
        .items()
        .iter()
        .map(|item| item.label)
        .collect::<Vec<_>>();
    let t = &crate::i18n::texts().context_menu;
    for expected in [
        t.manage_machines,
        t.edit_machine,
        t.reconnect_machine,
        t.disable_machine,
        t.remove_machine,
        t.copy_machine_fix_command,
    ] {
        assert!(labels.contains(&expected), "missing {expected}: {labels:?}");
    }
    // An online machine has nothing to reconnect; a disabled one shows enable.
    state.set_endpoint_status(&build_id, ClientEndpointStatus::Online);
    state.cache_endpoint_snapshot(&build_id, Box::new(snapshot()));
    state.open_machine_context_menu(&build_id, 4, 4);
    let Some(ClientShellOverlay::ContextMenu(menu)) = state.overlay.as_ref() else {
        panic!("context menu");
    };
    let labels = menu
        .items()
        .iter()
        .map(|item| item.label)
        .collect::<Vec<_>>();
    assert!(!labels.contains(&t.reconnect_machine), "{labels:?}");
    assert!(labels.contains(&t.disable_machine), "{labels:?}");
}

#[test]
fn global_menu_and_prefix_m_open_the_machines_overlay() {
    let snapshot = snapshot();
    let items = super::super::global_menu::global_menu_items(&snapshot);
    assert!(items.iter().any(|(_, action)| matches!(
        action,
        super::super::global_menu::ClientGlobalMenuAction::Binding(
            crate::input::KeybindAction::ManageMachines
        )
    )));

    let keybinds = crate::config::Keybinds::default();
    let matched = crate::input::resolve_prefix_binding(
        &keybinds,
        &crate::input::TerminalKey::new(KeyCode::Char('m'), KeyModifiers::empty()),
    );
    assert!(matches!(
        matched,
        Some(crate::input::KeybindMatch::Action(
            crate::input::KeybindAction::ManageMachines
        ))
    ));

    let mut state = state_with_profiles(&[]);
    state.record_binding(
        crate::input::KeybindMatch::Action(crate::input::KeybindAction::ManageMachines),
        &mut ClientShellInput::default(),
    );
    assert!(matches!(
        state.overlay,
        Some(ClientShellOverlay::Machines(_))
    ));
}

#[test]
fn user_prefix_m_custom_command_wins_over_the_builtin_machines_binding() {
    let config: Config = toml::from_str(
        r#"
[[keys.command]]
key = "prefix+m"
command = "echo hi"
"#,
    )
    .expect("config with custom command");
    let keybinds = config.keybinds();
    let matched = crate::input::resolve_prefix_binding(
        &keybinds,
        &crate::input::TerminalKey::new(KeyCode::Char('m'), KeyModifiers::empty()),
    );
    assert!(
        matches!(matched, Some(crate::input::KeybindMatch::Command(_))),
        "user custom command must keep prefix+m: {matched:?}"
    );
    assert!(keybinds.manage_machines.label().is_none());

    // Help still lists the action, showing it as unset in this configuration.
    let (live, _) = config
        .live_keybinds_with_diagnostics()
        .expect("live keybinds");
    let groups = crate::input::keybind_help_groups(&live.keybinds, live.prefix);
    let global = groups
        .iter()
        .flat_map(|(_, entries)| entries.iter())
        .any(|(_, label)| label.as_ref() == crate::i18n::texts().keybinds.manage_machines);
    assert!(global, "help should list manage machines");
}

#[test]
fn sidebar_groups_machines_under_group_headers_with_color_tags() {
    let alpha = {
        let mut profile = profile("Alpha", "alpha.example", "10");
        profile.group = Some("prod".into());
        profile
    };
    let solo = profile("Solo", "solo.example", "11");
    let beta = {
        let mut profile = profile("Beta", "beta.example", "12");
        profile.group = Some("prod".into());
        profile
    };
    let mut state = state_with_profiles(&[alpha, solo, beta]);
    let text = frame_text(&mut state, 106, 30);
    let alpha_pos = text.find("Alpha").expect("alpha row");
    let beta_pos = text.find("Beta").expect("beta row");
    let solo_pos = text.find("Solo").expect("solo row");
    assert!(alpha_pos < beta_pos, "group members stay together: {text}");
    assert!(
        beta_pos < solo_pos,
        "group block keeps first-appearance slot: {text}"
    );
    assert_eq!(text.matches("prod").count(), 1, "one group header: {text}");
    assert!(text.contains('▪'), "color tag on machine rows: {text}");
}

#[test]
fn attention_machine_click_opens_the_detail_overlay() {
    let mut state = state_with_profiles(&[profile("Build", "build", "20")]);
    let endpoint_id =
        ClientEndpointId::Ssh(ProfileId::parse("00000000000000000000000000000020").unwrap());
    state.set_endpoint_status(&endpoint_id, ClientEndpointStatus::Attention);
    frame_text(&mut state, 106, 30);
    let (hit_rect, hit_endpoint) = {
        let hit = state
            .hits
            .machines
            .iter()
            .find(|hit| hit.endpoint_id == endpoint_id)
            .expect("machine hit");
        (hit.rect, hit.endpoint_id.clone())
    };
    assert_eq!(hit_endpoint, endpoint_id);
    state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Right),
        column: hit_rect.x + 2,
        row: hit_rect.y,
        modifiers: KeyModifiers::empty(),
    })]);
    assert!(matches!(
        state.overlay,
        Some(ClientShellOverlay::ContextMenu(_))
    ));
    state.overlay = None;

    let mut outcome = ClientShellInput::default();
    // Click the label (outside the collapse toggle) on the attention row.
    assert!(state.handle_endpoint_machine_click((hit_rect.x + 6, hit_rect.y), &mut outcome));
    assert!(matches!(
        state.overlay,
        Some(ClientShellOverlay::Machines(
            super::super::machines_overlay::ClientMachinesOverlay {
                view: super::super::machines_overlay::ClientMachinesView::Detail(_),
                ..
            }
        ))
    ));
}

fn with_temp_state_home(name: &str) -> std::path::PathBuf {
    let dir =
        std::env::temp_dir().join(format!("herdr-machines-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp state home");
    // Safety: nextest isolates every test in its own process, so mutating the
    // process environment here cannot race other tests.
    unsafe { std::env::set_var("XDG_STATE_HOME", &dir) };
    dir
}

fn add_form_overlay<'a>(
    state: &'a mut ClientShellState,
    target: &str,
    label: &str,
) -> &'a mut super::super::machines_overlay::ClientMachineForm {
    state.open_machine_add_form();
    let Some(ClientShellOverlay::Machines(overlay)) = state.overlay.as_mut() else {
        panic!("machines overlay");
    };
    let super::super::machines_overlay::ClientMachinesView::Form(form) = &mut overlay.view else {
        panic!("add form view");
    };
    form.target = TextEditor::new(target, false);
    form.label = TextEditor::new(label, false);
    form.step = super::super::machines_overlay::MachineFormStep::Confirm;
    form
}

#[test]
fn bootstrap_success_persists_profile_and_returns_to_list() {
    let dir = with_temp_state_home("bootstrap-ok");
    let mut state = state_with_profiles(&[]);
    let ticket = {
        let form = add_form_overlay(&mut state, "build.example", "Build");
        form.bootstrap = Some(super::super::machines_overlay::ClientMachineBootstrap {
            cancel: crate::remote::TaskCancellation::default(),
            ticket: 41,
            step: None,
            failure: None,
        });
        41
    };
    state.handle_machine_bootstrap_update(ticket, MachineBootstrapUpdate::Finished(Ok(())));

    let catalog = crate::client::endpoint::EndpointCatalog::load().expect("catalog");
    assert_eq!(catalog.ssh.len(), 1);
    assert_eq!(catalog.ssh[0].label, "Build");
    assert_eq!(catalog.ssh[0].target, "build.example");
    assert_eq!(catalog.ssh[0].session, "default");
    assert!(matches!(
        state.overlay,
        Some(ClientShellOverlay::Machines(
            super::super::machines_overlay::ClientMachinesOverlay {
                view: super::super::machines_overlay::ClientMachinesView::List,
                ..
            }
        ))
    ));
    assert_eq!(state.saved_profiles.len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn bootstrap_failure_keeps_form_with_structured_error() {
    let dir = with_temp_state_home("bootstrap-fail");
    let mut state = state_with_profiles(&[]);
    let ticket = {
        let form = add_form_overlay(&mut state, "build.example", "Build");
        form.bootstrap = Some(super::super::machines_overlay::ClientMachineBootstrap {
            cancel: crate::remote::TaskCancellation::default(),
            ticket: 42,
            step: Some(crate::remote::SavedSshBootstrapStep::Install),
            failure: None,
        });
        42
    };
    state.handle_machine_bootstrap_update(
        ticket,
        MachineBootstrapUpdate::Finished(Err("Permission denied (publickey)".into())),
    );

    let Some(ClientShellOverlay::Machines(overlay)) = state.overlay.as_ref() else {
        panic!("machines overlay stays open");
    };
    let super::super::machines_overlay::ClientMachinesView::Form(form) = &overlay.view else {
        panic!("form view kept after failure");
    };
    assert_eq!(
        form.bootstrap.as_ref().and_then(|b| b.failure.as_deref()),
        Some("Permission denied (publickey)")
    );
    let text = frame_text(&mut state, 106, 32);
    assert!(text.contains("Permission denied"), "frame: {text}");
    assert!(text.contains("herdr --remote"), "frame: {text}");
    assert!(crate::client::endpoint::EndpointCatalog::load()
        .expect("catalog")
        .ssh
        .is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn edit_form_updates_group_and_connection_fields() {
    let dir = with_temp_state_home("edit-save");
    let saved = profile("Build", "build.example", "30");
    {
        let mut catalog = crate::client::endpoint::EndpointCatalog::default();
        catalog
            .add_ssh_with_options(
                saved.label.clone(),
                saved.target.clone(),
                saved.session.clone(),
                crate::client::endpoint::SshProfileOptions::default(),
            )
            .expect("seed profile");
        catalog.store_profiles().expect("seed store");
    }
    let seed_id = crate::client::endpoint::EndpointCatalog::load()
        .expect("catalog")
        .ssh[0]
        .id
        .clone();
    let mut state = state_with_profiles(&[]);
    state.set_endpoint_catalog(
        &crate::client::endpoint::EndpointCatalog::load()
            .unwrap()
            .ssh,
    );
    state.open_machine_edit_form(&seed_id);
    {
        let Some(ClientShellOverlay::Machines(overlay)) = state.overlay.as_mut() else {
            panic!("machines overlay");
        };
        let super::super::machines_overlay::ClientMachinesView::Form(form) = &mut overlay.view
        else {
            panic!("edit form");
        };
        form.group = TextEditor::new("prod", false);
        form.port = TextEditor::new("2222", false);
    }
    state.handle_raw_events(vec![RawInputEvent::Key(key(KeyCode::Enter))]);

    let catalog = crate::client::endpoint::EndpointCatalog::load().expect("catalog");
    assert_eq!(catalog.ssh[0].group.as_deref(), Some("prod"));
    assert_eq!(catalog.ssh[0].port, Some(2222));
    assert_eq!(catalog.ssh[0].id, seed_id);
    assert!(matches!(
        state.overlay,
        Some(ClientShellOverlay::Machines(
            super::super::machines_overlay::ClientMachinesOverlay {
                view: super::super::machines_overlay::ClientMachinesView::Detail(_),
                ..
            }
        ))
    ));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn edit_form_updates_session_log_fields() {
    let dir = with_temp_state_home("edit-session-log");
    {
        let mut catalog = crate::client::endpoint::EndpointCatalog::default();
        catalog
            .add_ssh("Build", "build.example", "default")
            .expect("seed profile");
        catalog.store_profiles().expect("seed store");
    }
    let seed_id = crate::client::endpoint::EndpointCatalog::load()
        .expect("catalog")
        .ssh[0]
        .id
        .clone();
    let mut state = state_with_profiles(&[]);
    state.set_endpoint_catalog(
        &crate::client::endpoint::EndpointCatalog::load()
            .unwrap()
            .ssh,
    );
    state.open_machine_edit_form(&seed_id);
    {
        let Some(ClientShellOverlay::Machines(overlay)) = state.overlay.as_mut() else {
            panic!("machines overlay");
        };
        let super::super::machines_overlay::ClientMachinesView::Form(form) = &mut overlay.view
        else {
            panic!("edit form");
        };
        // Default keeps the saved (absent) value; cycling to Yes rebuilds it
        // from the text fields.
        assert!(matches!(
            form.session_log_enabled,
            super::super::machines_overlay::TriChoice::Default
        ));
        form.session_log_enabled = super::super::machines_overlay::TriChoice::Yes;
        form.session_log_path = TextEditor::new("{pane}.log", false);
        form.session_log_max_bytes = TextEditor::new("8192", false);
        form.session_log_interval = TextEditor::new("10", false);
    }
    state.handle_raw_events(vec![RawInputEvent::Key(key(KeyCode::Enter))]);

    let catalog = crate::client::endpoint::EndpointCatalog::load().expect("catalog");
    let log = catalog.ssh[0]
        .session_log
        .as_ref()
        .expect("session log saved");
    assert!(log.enabled);
    assert_eq!(log.path_template.as_deref(), Some("{pane}.log"));
    assert_eq!(log.max_bytes, Some(8192));
    assert_eq!(log.dump_interval_secs, Some(10));

    // Re-open and leave the choice at Default: the saved value passes
    // through an unrelated edit untouched.
    state.open_machine_edit_form(&seed_id);
    {
        let Some(ClientShellOverlay::Machines(overlay)) = state.overlay.as_mut() else {
            panic!("machines overlay");
        };
        let super::super::machines_overlay::ClientMachinesView::Form(form) = &mut overlay.view
        else {
            panic!("edit form");
        };
        assert!(matches!(
            form.session_log_enabled,
            super::super::machines_overlay::TriChoice::Yes
        ));
        form.session_log_enabled = super::super::machines_overlay::TriChoice::Default;
        form.session_log_path = TextEditor::new("ignored.log", false);
        form.group = TextEditor::new("prod", false);
    }
    state.handle_raw_events(vec![RawInputEvent::Key(key(KeyCode::Enter))]);
    let catalog = crate::client::endpoint::EndpointCatalog::load().expect("catalog");
    assert_eq!(catalog.ssh[0].group.as_deref(), Some("prod"));
    assert_eq!(
        catalog.ssh[0]
            .session_log
            .as_ref()
            .and_then(|log| log.path_template.as_deref()),
        Some("{pane}.log")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn detail_card_shows_session_log_status_and_dropped_count() {
    let dir = with_temp_state_home("detail-session-log");
    {
        let mut catalog = crate::client::endpoint::EndpointCatalog::default();
        let id = catalog
            .add_ssh("Build", "build.example", "default")
            .expect("seed profile");
        catalog
            .set_session_log(
                &id,
                Some(crate::client::endpoint::SessionLogProfile {
                    enabled: true,
                    path_template: Some("{pane}.log".into()),
                    max_bytes: None,
                    dump_interval_secs: None,
                }),
            )
            .expect("seed log config");
        catalog.store_profiles().expect("seed store");
    }
    let seed_id = crate::client::endpoint::EndpointCatalog::load()
        .expect("catalog")
        .ssh[0]
        .id
        .clone();
    let mut state = state_with_profiles(&[]);
    state.set_endpoint_catalog(
        &crate::client::endpoint::EndpointCatalog::load()
            .unwrap()
            .ssh,
    );
    state.set_session_log_dropped(&seed_id, 7);
    state.open_machines_overlay_for(&seed_id);

    let frame = state.compose(106, 32).expect("composed frame");
    let text = frame
        .cells
        .chunks(frame.width as usize)
        .map(|row| row.iter().map(|cell| cell.symbol.as_str()).collect())
        .collect::<Vec<String>>()
        .join("\n");
    // Wide CJK glyphs carry a padding cell in the frame, so compare on the
    // whitespace-stripped text.
    let compact: String = text.chars().filter(|ch| !ch.is_whitespace()).collect();
    let session_log_label = crate::i18n::texts().machines.detail_session_log;
    assert!(compact.contains(session_log_label), "frame: {text}");
    assert!(compact.contains("{pane}.log"), "frame: {text}");
    let dropped = crate::i18n::fill(
        crate::i18n::texts().machines.session_log_dropped_fmt,
        &[("count", "7")],
    );
    let dropped_compact: String = dropped.chars().filter(|ch| !ch.is_whitespace()).collect();
    assert!(compact.contains(&dropped_compact), "frame: {text}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn session_log_dropped_mirror_tracks_per_profile_changes() {
    let mut state = state_with_profiles(&[]);
    let a = ProfileId::parse("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").expect("id");
    let b = ProfileId::parse("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").expect("id");
    assert!(state.set_session_log_dropped(&a, 1));
    assert!(!state.set_session_log_dropped(&a, 1));
    assert!(state.set_session_log_dropped(&b, 2));
    assert_eq!(state.session_log_dropped.get(&a), Some(&1));
    assert_eq!(state.session_log_dropped.get(&b), Some(&2));
    // Zero clears the entry (the card only shows non-zero counts).
    assert!(state.set_session_log_dropped(&a, 0));
    assert!(!state.set_session_log_dropped(&a, 0));
    assert!(!state.session_log_dropped.contains_key(&a));
    assert_eq!(state.session_log_dropped.get(&b), Some(&2));
}

#[test]
fn session_log_dropped_counts_render_per_machine() {
    let dir = with_temp_state_home("detail-session-log-per-machine");
    let (id_a, id_b);
    {
        let mut catalog = crate::client::endpoint::EndpointCatalog::default();
        id_a = catalog
            .add_ssh("Build", "build.example", "default")
            .expect("seed profile");
        id_b = catalog
            .add_ssh("Stage", "stage.example", "default")
            .expect("seed profile");
        for id in [&id_a, &id_b] {
            catalog
                .set_session_log(
                    id,
                    Some(crate::client::endpoint::SessionLogProfile {
                        enabled: true,
                        // Short template so the drop counter fits the card.
                        path_template: Some("{pane}.log".into()),
                        max_bytes: None,
                        dump_interval_secs: None,
                    }),
                )
                .expect("seed log config");
        }
        catalog.store_profiles().expect("seed store");
    }
    let mut state = state_with_profiles(&[]);
    state.set_endpoint_catalog(
        &crate::client::endpoint::EndpointCatalog::load()
            .expect("catalog")
            .ssh,
    );
    state.set_session_log_dropped(&id_a, 3);
    let dropped = crate::i18n::fill(
        crate::i18n::texts().machines.session_log_dropped_fmt,
        &[("count", "3")],
    );
    let dropped_compact: String = dropped.chars().filter(|ch| !ch.is_whitespace()).collect();

    // The machine with drops shows its own counter...
    state.open_machines_overlay_for(&id_a);
    let compact: String = frame_text(&mut state, 106, 32)
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .collect();
    assert!(compact.contains(&dropped_compact), "frame: {compact}");

    // ...and the other machine's card does not inherit it.
    state.open_machines_overlay_for(&id_b);
    let compact: String = frame_text(&mut state, 106, 32)
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .collect();
    assert!(!compact.contains(&dropped_compact), "frame: {compact}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn remove_confirmation_deletes_profile_from_catalog() {
    let dir = with_temp_state_home("remove");
    {
        let mut catalog = crate::client::endpoint::EndpointCatalog::default();
        catalog
            .add_ssh("Build", "build.example", "default")
            .expect("seed profile");
        catalog.store_profiles().expect("seed store");
    }
    let seed = crate::client::endpoint::EndpointCatalog::load()
        .unwrap()
        .ssh[0]
        .clone();
    let mut state = state_with_profiles(std::slice::from_ref(&seed));

    state.open_machine_remove_confirm(&seed.id);
    state.handle_raw_events(vec![RawInputEvent::Key(key(KeyCode::Enter))]);

    assert!(crate::client::endpoint::EndpointCatalog::load()
        .expect("catalog")
        .ssh
        .is_empty());
    assert!(state.saved_profiles.is_empty());
    assert!(matches!(
        state.overlay,
        Some(ClientShellOverlay::Machines(
            super::super::machines_overlay::ClientMachinesOverlay {
                view: super::super::machines_overlay::ClientMachinesView::List,
                ..
            }
        ))
    ));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rename_overlay_renames_machine_in_catalog() {
    let dir = with_temp_state_home("rename");
    {
        let mut catalog = crate::client::endpoint::EndpointCatalog::default();
        catalog
            .add_ssh("Build", "build.example", "default")
            .expect("seed profile");
        catalog.store_profiles().expect("seed store");
    }
    let seed = crate::client::endpoint::EndpointCatalog::load()
        .unwrap()
        .ssh[0]
        .clone();
    let mut state = state_with_profiles(std::slice::from_ref(&seed));

    state.open_machine_context_menu(&ClientEndpointId::Ssh(seed.id.clone()), 4, 4);
    let rename_index = {
        let Some(ClientShellOverlay::ContextMenu(menu)) = state.overlay.as_ref() else {
            panic!("context menu");
        };
        menu.items()
            .iter()
            .position(|item| item.action == ClientContextMenuAction::RenameMachine)
            .expect("rename item")
    };
    let mut outcome = ClientShellInput::default();
    state.activate_context_menu_item(rename_index, &mut outcome);
    let Some(ClientShellOverlay::Rename(rename)) = state.overlay.as_mut() else {
        panic!("rename overlay");
    };
    rename.input = TextEditor::new("Renamed", false);
    let mut outcome = ClientShellInput::default();
    state.save_rename_overlay(&mut outcome);

    let catalog = crate::client::endpoint::EndpointCatalog::load().expect("catalog");
    assert_eq!(catalog.ssh[0].label, "Renamed");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn reconnect_button_emits_supervisor_action() {
    let saved = profile("Build", "build.example", "40");
    let mut state = state_with_profiles(std::slice::from_ref(&saved));
    state.open_machines_overlay_for(&saved.id);
    let mut outcome = ClientShellInput::default();
    state.route_machines_key(&key(KeyCode::Char('r')), &mut outcome);
    assert!(matches!(
        outcome.actions.as_slice(),
        [ClientShellAction::ReconnectEndpoint { endpoint_id }]
            if endpoint_id == &ClientEndpointId::Ssh(saved.id.clone())
    ));

    let mut outcome = ClientShellInput::default();
    state.machine_reconnect(&saved.id, &mut outcome);
    assert_eq!(outcome.actions.len(), 1);
}

#[test]
fn copy_fix_command_targets_the_clipboard() {
    let saved = profile("Build", "dev@build.example", "50");
    let mut state = state_with_profiles(std::slice::from_ref(&saved));
    state.open_machines_overlay_for(&saved.id);
    let mut outcome = ClientShellInput::default();
    state.route_machines_key(&key(KeyCode::Char('c')), &mut outcome);
    let [ClientShellAction::ClipboardWrite(bytes)] = &outcome.actions[..] else {
        panic!("clipboard write: {:?}", outcome.actions);
    };
    assert_eq!(
        String::from_utf8_lossy(bytes),
        "herdr --remote dev@build.example --session default"
    );
}

#[test]
fn wizard_advances_steps_and_prefills_label() {
    let mut state = state_with_profiles(&[]);
    state.open_machine_add_form();
    {
        let Some(ClientShellOverlay::Machines(overlay)) = state.overlay.as_mut() else {
            panic!("machines overlay");
        };
        let super::super::machines_overlay::ClientMachinesView::Form(form) = &mut overlay.view
        else {
            panic!("add form");
        };
        form.target = TextEditor::new("build.example", false);
    }
    let mut outcome = ClientShellInput::default();
    state.route_machines_key(&key(KeyCode::Enter), &mut outcome);
    let Some(ClientShellOverlay::Machines(overlay)) = state.overlay.as_ref() else {
        panic!("machines overlay");
    };
    let super::super::machines_overlay::ClientMachinesView::Form(form) = &overlay.view else {
        panic!("add form");
    };
    assert_eq!(
        form.step,
        super::super::machines_overlay::MachineFormStep::Connection
    );
    assert_eq!(form.label.as_str(), "build.example");

    let mut outcome = ClientShellInput::default();
    state.route_machines_key(&key(KeyCode::Enter), &mut outcome);
    state.route_machines_key(&key(KeyCode::Enter), &mut outcome);
    let Some(ClientShellOverlay::Machines(overlay)) = state.overlay.as_ref() else {
        panic!("machines overlay");
    };
    let super::super::machines_overlay::ClientMachinesView::Form(form) = &overlay.view else {
        panic!("add form");
    };
    assert_eq!(
        form.step,
        super::super::machines_overlay::MachineFormStep::Confirm
    );

    // Esc walks back one step per press.
    state.route_machines_key(&key(KeyCode::Esc), &mut ClientShellInput::default());
    let Some(ClientShellOverlay::Machines(overlay)) = state.overlay.as_ref() else {
        panic!("machines overlay");
    };
    let super::super::machines_overlay::ClientMachinesView::Form(form) = &overlay.view else {
        panic!("add form");
    };
    assert_eq!(
        form.step,
        super::super::machines_overlay::MachineFormStep::Session
    );
}

#[test]
fn wizard_rejects_invalid_port_before_bootstrap() {
    let dir = with_temp_state_home("bad-port");
    let mut state = state_with_profiles(&[]);
    {
        let form = add_form_overlay(&mut state, "build.example", "Build");
        form.port = TextEditor::new("not-a-port", false);
    }
    let mut outcome = ClientShellInput::default();
    state.route_machines_key(&key(KeyCode::Enter), &mut outcome);
    assert!(outcome.actions.is_empty());
    let Some(ClientShellOverlay::Machines(overlay)) = state.overlay.as_ref() else {
        panic!("machines overlay");
    };
    let super::super::machines_overlay::ClientMachinesView::Form(form) = &overlay.view else {
        panic!("add form");
    };
    assert!(form.error.is_some(), "port error must surface in the form");
    assert!(form.bootstrap.is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn disabling_a_machine_updates_the_catalog_and_context_menu() {
    let dir = with_temp_state_home("disable");
    {
        let mut catalog = crate::client::endpoint::EndpointCatalog::default();
        catalog
            .add_ssh("Build", "build.example", "default")
            .expect("seed profile");
        catalog.store_profiles().expect("seed store");
    }
    let seed = crate::client::endpoint::EndpointCatalog::load()
        .unwrap()
        .ssh[0]
        .clone();
    let mut state = state_with_profiles(std::slice::from_ref(&seed));
    let endpoint_id = ClientEndpointId::Ssh(seed.id.clone());

    state.open_machine_context_menu(&endpoint_id, 4, 4);
    let disable_index = {
        let Some(ClientShellOverlay::ContextMenu(menu)) = state.overlay.as_ref() else {
            panic!("context menu");
        };
        menu.items()
            .iter()
            .position(|item| item.action == ClientContextMenuAction::ToggleMachineEnabled)
            .expect("toggle item")
    };
    let mut outcome = ClientShellInput::default();
    state.activate_context_menu_item(disable_index, &mut outcome);

    let catalog = crate::client::endpoint::EndpointCatalog::load().expect("catalog");
    assert!(!catalog.ssh[0].enabled);

    // The context menu now offers enable and drops reconnect.
    state.open_machine_context_menu(&endpoint_id, 4, 4);
    let Some(ClientShellOverlay::ContextMenu(menu)) = state.overlay.as_ref() else {
        panic!("context menu");
    };
    let labels = menu
        .items()
        .iter()
        .map(|item| item.label)
        .collect::<Vec<_>>();
    let t = &crate::i18n::texts().context_menu;
    assert!(labels.contains(&t.enable_machine), "{labels:?}");
    assert!(!labels.contains(&t.reconnect_machine), "{labels:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn key_routing_ignores_non_machines_overlays() {
    let mut state = state_with_profiles(&[]);
    state.overlay = Some(ClientShellOverlay::Rename(ClientRenameOverlay {
        title: "rename pane",
        input: TextEditor::default(),
        target: ClientRenameTarget::Pane {
            pane_id: "pane_1".into(),
        },
    }));
    let mut outcome = ClientShellInput::default();
    assert!(!state.route_machines_key(&key(KeyCode::Esc), &mut outcome));
}

// ---------------------------------------------------------------------
// C1: SSH config import wizard, port-forward editor, forward status card
// ---------------------------------------------------------------------

fn with_temp_home(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("herdr-machines-c1-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join(".ssh")).expect("temp home");
    // Safety: nextest isolates every test in its own process, so mutating the
    // process environment here cannot race other tests.
    unsafe {
        std::env::set_var("HOME", &dir);
        std::env::set_var("XDG_STATE_HOME", dir.join("state"));
    }
    dir
}

/// Wide CJK glyphs occupy two cells in the composed frame; match on the
/// compacted text.
fn compact_frame(state: &mut ClientShellState, cols: u16, rows: u16) -> String {
    frame_text(state, cols, rows)
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .collect()
}

const IMPORT_FIXTURE: &str = "\
Host bastion
    HostName bastion.internal

Host web
    HostName web.internal
    User deploy
    ProxyJump bastion

Host *.wild
";

#[test]
fn import_wizard_discovers_selects_and_imports() {
    let dir = with_temp_home("wizard");
    std::fs::write(dir.join(".ssh").join("config"), IMPORT_FIXTURE).unwrap();
    let mut state = state_with_profiles(&[]);

    state.open_machine_import_wizard();
    let text = frame_text(&mut state, 110, 32);
    assert!(text.contains("bastion"), "discover lists hosts: {text}");
    assert!(text.contains("web.internal"), "frame: {text}");
    assert!(
        text.contains("*.wild"),
        "wildcard listed with its reason: {text}"
    );

    // Continue to the selection step; everything importable starts checked.
    state.route_machines_key(&key(KeyCode::Enter), &mut ClientShellInput::default());
    {
        let Some(ClientShellOverlay::Machines(overlay)) = state.overlay.as_ref() else {
            panic!("machines overlay");
        };
        let super::super::machines_overlay::ClientMachinesView::Import(view) = &overlay.view else {
            panic!("import view");
        };
        assert_eq!(
            view.step,
            super::super::machines_overlay::ClientImportStep::Select
        );
        assert_eq!(view.plan.ready.len(), 2, "bastion + web");
        assert!(view.selected.iter().all(|selected| *selected));
    }

    // Import; the jump host lands before its dependent and the hop resolves
    // to a profile reference.
    state.route_machines_key(&key(KeyCode::Enter), &mut ClientShellInput::default());
    {
        let Some(ClientShellOverlay::Machines(overlay)) = state.overlay.as_ref() else {
            panic!("machines overlay");
        };
        let super::super::machines_overlay::ClientMachinesView::Import(view) = &overlay.view else {
            panic!("import view");
        };
        assert_eq!(
            view.step,
            super::super::machines_overlay::ClientImportStep::Done
        );
        assert_eq!(view.summary, (2, 1, 0), "{:?}", view.results);
    }
    let catalog = crate::client::endpoint::EndpointCatalog::load().expect("catalog");
    assert_eq!(catalog.ssh.len(), 2);
    assert_eq!(catalog.ssh[0].label, "bastion");
    assert_eq!(catalog.ssh[1].label, "web");
    let bastion_id = catalog.ssh[0].id.clone();
    assert_eq!(
        catalog.ssh[1].proxy_jump,
        vec![crate::client::endpoint::ProxyJumpHop::Profile(bastion_id)]
    );
    let text = compact_frame(&mut state, 110, 32);
    let imported = crate::i18n::texts().machines.import_result_imported;
    assert!(text.contains(imported), "done step reports: {text}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn import_wizard_empty_config_is_a_clean_empty_state() {
    let dir = with_temp_home("empty");
    // No config file at all: the fatal empty state must render and close.
    let mut state = state_with_profiles(&[]);
    state.open_machine_import_wizard();
    let text = compact_frame(&mut state, 110, 30);
    let expected: String = crate::i18n::texts()
        .machines
        .import_no_config
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .collect();
    assert!(text.contains(&expected), "frame: {text}");
    state.route_machines_key(&key(KeyCode::Esc), &mut ClientShellInput::default());
    assert!(matches!(
        state.overlay,
        Some(ClientShellOverlay::Machines(
            super::super::machines_overlay::ClientMachinesOverlay {
                view: super::super::machines_overlay::ClientMachinesView::List,
                ..
            }
        ))
    ));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn import_wizard_wildcard_toggle_replans_and_deselect_works() {
    let dir = with_temp_home("wildcard");
    std::fs::write(dir.join(".ssh").join("config"), IMPORT_FIXTURE).unwrap();
    let mut state = state_with_profiles(&[]);
    state.open_machine_import_wizard();
    state.route_machines_key(&key(KeyCode::Enter), &mut ClientShellInput::default());

    // Toggle the wildcard row on: the plan grows a third candidate.
    {
        let Some(ClientShellOverlay::Machines(overlay)) = state.overlay.as_mut() else {
            panic!("overlay");
        };
        let super::super::machines_overlay::ClientMachinesView::Import(view) = &mut overlay.view
        else {
            panic!("import view");
        };
        view.focus_row = view.plan.ready.len();
    }
    state.route_machines_key(&key(KeyCode::Char(' ')), &mut ClientShellInput::default());
    {
        let Some(ClientShellOverlay::Machines(overlay)) = state.overlay.as_ref() else {
            panic!("overlay");
        };
        let super::super::machines_overlay::ClientMachinesView::Import(view) = &overlay.view else {
            panic!("import view");
        };
        assert!(view.include_wildcards);
        assert_eq!(view.plan.ready.len(), 3, "wildcard host joined the plan");
    }

    // Deselect everything (all -> none over two toggles), then run: all
    // three report as skipped.
    state.route_machines_key(&key(KeyCode::Char('a')), &mut ClientShellInput::default());
    state.route_machines_key(&key(KeyCode::Char('a')), &mut ClientShellInput::default());
    state.route_machines_key(&key(KeyCode::Enter), &mut ClientShellInput::default());
    let catalog = crate::client::endpoint::EndpointCatalog::load().expect("catalog");
    assert!(catalog.ssh.is_empty(), "nothing imported when unchecked");
    let Some(ClientShellOverlay::Machines(overlay)) = state.overlay.as_ref() else {
        panic!("overlay");
    };
    let super::super::machines_overlay::ClientMachinesView::Import(view) = &overlay.view else {
        panic!("import view");
    };
    assert_eq!(view.summary, (0, 3, 0), "{:?}", view.summary);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn forwards_editor_adds_and_removes_rules_in_the_catalog() {
    let dir = with_temp_home("forwards");
    let saved = {
        let mut catalog = crate::client::endpoint::EndpointCatalog::default();
        catalog
            .add_ssh("Build", "build.example", "default")
            .expect("seed profile");
        catalog.store_profiles().expect("seed store");
        crate::client::endpoint::EndpointCatalog::load()
            .unwrap()
            .ssh[0]
            .clone()
    };
    let mut state = state_with_profiles(std::slice::from_ref(&saved));
    state.open_machines_overlay_for(&saved.id);
    state.route_machines_key(&key(KeyCode::Char('f')), &mut ClientShellInput::default());
    assert!(matches!(
        state.overlay,
        Some(ClientShellOverlay::Machines(
            super::super::machines_overlay::ClientMachinesOverlay {
                view: super::super::machines_overlay::ClientMachinesView::Forwards(_),
                ..
            }
        ))
    ));

    // Add a local forward through the form.
    state.route_machines_key(&key(KeyCode::Char('a')), &mut ClientShellInput::default());
    {
        let Some(ClientShellOverlay::Machines(overlay)) = state.overlay.as_mut() else {
            panic!("overlay");
        };
        let super::super::machines_overlay::ClientMachinesView::Forwards(view) = &mut overlay.view
        else {
            panic!("forwards view");
        };
        view.form.focused = 1;
        view.form.listen_port = TextEditor::new("8080", false);
        view.form.target_host = TextEditor::new("127.0.0.1", false);
        view.form.target_port = TextEditor::new("80", false);
    }
    state.route_machines_key(&key(KeyCode::Enter), &mut ClientShellInput::default());
    let catalog = crate::client::endpoint::EndpointCatalog::load().expect("catalog");
    assert_eq!(catalog.ssh[0].port_forwards.len(), 1);
    let rule = &catalog.ssh[0].port_forwards[0];
    assert_eq!(rule.kind, crate::client::endpoint::PortForwardKind::Local);
    assert_eq!(rule.listen_port, 8080);

    // The detail card lists the rule with the waiting note (offline machine).
    state.route_machines_key(&key(KeyCode::Esc), &mut ClientShellInput::default());
    let text = frame_text(&mut state, 110, 34);
    assert!(text.contains("8080 -> 127.0.0.1:80"), "frame: {text}");

    // Remove it again.
    state.route_machines_key(&key(KeyCode::Char('f')), &mut ClientShellInput::default());
    state.route_machines_key(&key(KeyCode::Char('x')), &mut ClientShellInput::default());
    let catalog = crate::client::endpoint::EndpointCatalog::load().expect("catalog");
    assert!(catalog.ssh[0].port_forwards.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn detail_card_shows_live_forward_status_from_the_mirror() {
    let saved = {
        let mut profile = profile("Build", "build.example", "60");
        profile.port_forwards = vec![crate::client::endpoint::PortForwardRule {
            kind: crate::client::endpoint::PortForwardKind::Local,
            bind_address: None,
            listen_port: 8080,
            target_host: Some("127.0.0.1".into()),
            target_port: Some(80),
        }];
        profile
    };
    let mut state = state_with_profiles(std::slice::from_ref(&saved));
    let endpoint_id = ClientEndpointId::Ssh(saved.id.clone());
    state.set_endpoint_status(&endpoint_id, ClientEndpointStatus::Online);
    state.set_endpoint_port_forward_status(
        &endpoint_id,
        vec![crate::remote::PortForwardStatus {
            rule: saved.port_forwards[0].clone(),
            phase: crate::remote::PortForwardPhase::Active,
            detail: None,
        }],
    );
    state.open_machines_overlay_for(&saved.id);
    let text = compact_frame(&mut state, 110, 34);
    let active = crate::i18n::texts().machines.forward_status_active;
    assert!(text.contains(active), "frame: {text}");

    // A failed rule shows its reason.
    state.set_endpoint_port_forward_status(
        &endpoint_id,
        vec![crate::remote::PortForwardStatus {
            rule: saved.port_forwards[0].clone(),
            phase: crate::remote::PortForwardPhase::Failed,
            detail: Some("address already in use".into()),
        }],
    );
    let text = compact_frame(&mut state, 110, 34);
    assert!(text.contains("addressalreadyinuse"), "frame: {text}");
    // And the poll gate opens while the detail card is up.
    assert_eq!(
        state.port_forward_poll_due(std::time::Instant::now()),
        Some(endpoint_id)
    );
}
