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

fn moved_mouse(col: u16, row: u16) -> crossterm::event::MouseEvent {
    crossterm::event::MouseEvent {
        kind: MouseEventKind::Moved,
        column: col,
        row,
        modifiers: KeyModifiers::empty(),
    }
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

/// MENU-01 / UX-04：机器列表的 `Moved` 只写 hover。List 键表里的 `d`
/// （立即启停，会断掉在线 SSH）、`x`（删除）、`r`（重连）、`Shift+R`（改名）
/// 全部取键盘选中的那台机器——指针划过列表就把它们重新指向「鼠标最后路过的
/// 机器」是不可接受的。
#[test]
fn machine_hover_does_not_move_the_keyboard_selection() {
    let build = profile("Build", "dev@build.example", "1");
    let stage = profile("Stage", "stage.example", "2");
    let mut state = state_with_profiles(&[build, stage]);
    state.open_machines_overlay();
    // 窄一点，走朴素列表而不是宽屏 dashboard（dashboard 有 detail 区，
    // `Moved` 分支本来就不接管）。
    state.compose(80, 24).expect("machines list");
    assert!(
        state.hits.machines_detail_area.is_empty(),
        "这一档布局应当是朴素列表"
    );
    assert_eq!(state.hits.machines_rows.len(), 2);

    let (second_rect, second_id) = state.hits.machines_rows[1].clone();
    let mut outcome = ClientShellInput::default();
    state.handle_mouse(moved_mouse(second_rect.x + 1, second_rect.y), &mut outcome);
    assert!(outcome.repaint);
    let Some(ClientShellOverlay::Machines(overlay)) = state.overlay.as_ref() else {
        panic!("machines overlay");
    };
    assert_eq!(overlay.hovered.as_ref(), Some(&second_id));
    assert_eq!(overlay.selected, 0, "键盘选中不被指针改写");

    // 指针移出列表：hover 清空，选中仍不动。
    let mut outcome = ClientShellInput::default();
    state.handle_mouse(moved_mouse(second_rect.x + 1, 0), &mut outcome);
    assert!(outcome.repaint);
    let Some(ClientShellOverlay::Machines(overlay)) = state.overlay.as_ref() else {
        panic!("machines overlay");
    };
    assert_eq!(overlay.hovered, None);
    assert_eq!(overlay.selected, 0);

    // 点击才是显式选择。
    state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: second_rect.x + 1,
        row: second_rect.y,
        modifiers: KeyModifiers::empty(),
    })]);
    let Some(ClientShellOverlay::Machines(overlay)) = state.overlay.as_ref() else {
        panic!("machines overlay");
    };
    assert_eq!(overlay.selected, 1);
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
    state.route_machines_key(&key(KeyCode::Enter), &mut ClientShellInput::default());
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

// ---------------------------------------------------------------------
// C-10 / C-11：导入向导与端口转发列表的滚动窗口与删除确认
// ---------------------------------------------------------------------

/// 生成 `count` 台主机的 SSH 配置，用来把候选列表撑出一屏。
fn many_hosts_config(count: usize) -> String {
    let mut text = String::new();
    for index in 0..count {
        text.push_str(&format!(
            "Host host{index:02}\n    HostName host{index:02}.internal\n\n"
        ));
    }
    text
}

/// 一次 compose 同时取回整帧文本（去空白便于匹配 CJK 宽字符）与光标。
fn frame_compact(
    state: &mut ClientShellState,
    cols: u16,
    rows: u16,
) -> (String, Option<crate::protocol::CursorState>) {
    let frame = state.compose(cols, rows).expect("composed frame");
    let text = frame
        .cells
        .chunks(frame.width as usize)
        .map(|row| {
            row.iter()
                .map(|cell| cell.symbol.as_str())
                .collect::<String>()
        })
        .collect::<Vec<String>>()
        .join("\n");
    (
        text.chars().filter(|ch| !ch.is_whitespace()).collect(),
        frame.cursor.clone(),
    )
}

fn seed_forward_profile(rules: usize) -> SavedSshEndpoint {
    let mut catalog = crate::client::endpoint::EndpointCatalog::default();
    let id = catalog
        .add_ssh("Build", "build.example", "default")
        .expect("seed profile");
    let forwards = (0..rules)
        .map(|index| crate::client::endpoint::PortForwardRule {
            kind: crate::client::endpoint::PortForwardKind::Local,
            bind_address: None,
            listen_port: 9000 + index as u16,
            target_host: Some("127.0.0.1".into()),
            target_port: Some(80),
        })
        .collect();
    catalog
        .set_port_forwards(&id, forwards)
        .expect("seed forwards");
    catalog.store_profiles().expect("seed store");
    crate::client::endpoint::EndpointCatalog::load()
        .expect("catalog")
        .ssh[0]
        .clone()
}

#[test]
fn import_select_scrolls_so_the_group_input_stays_visible() {
    let dir = with_temp_home("import-scroll");
    std::fs::write(dir.join(".ssh").join("config"), many_hosts_config(30)).unwrap();
    let mut state = state_with_profiles(&[]);
    state.open_machine_import_wizard();
    // discover → select
    state.route_machines_key(&key(KeyCode::Enter), &mut ClientShellInput::default());
    // 30 个候选 + 通配符开关，分组输入框是第 31 个焦点位。
    for _ in 0..31 {
        state.route_machines_key(&key(KeyCode::Down), &mut ClientShellInput::default());
    }
    let (text, cursor) = frame_compact(&mut state, 110, 32);
    let t = &crate::i18n::texts().machines;
    assert!(
        text.contains(&t.import_group_label.replace(' ', "")),
        "分组输入行可见：{text}"
    );
    assert!(
        text.contains(&t.import_include_wildcards.replace(' ', "")),
        "通配符开关可见：{text}"
    );
    // 整帧光标可能回落到未被浮层盖住的终端光标，必须断言它落在分组输入行上。
    let group_row = state
        .hits
        .machines_wizard_rows
        .iter()
        .find(|(_, index)| *index == 31)
        .map(|(rect, _)| *rect)
        .expect("分组输入行在命中表里");
    let cursor = cursor.expect("分组输入框聚焦时返回光标");
    assert!(cursor.visible, "光标可见");
    assert_eq!(cursor.y, group_row.y, "光标落在分组输入行");
    assert!(
        cursor.x >= group_row.x && cursor.x < group_row.right(),
        "光标横坐标落在分组输入行内：{cursor:?} / {group_row:?}"
    );
    assert!(
        !text.contains("host00.internal"),
        "列表已滚动，第一个候选移出窗口：{text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn import_wizard_rows_track_the_scrolled_window() {
    let dir = with_temp_home("import-rows");
    std::fs::write(dir.join(".ssh").join("config"), many_hosts_config(30)).unwrap();
    let mut state = state_with_profiles(&[]);
    state.open_machine_import_wizard();
    state.route_machines_key(&key(KeyCode::Enter), &mut ClientShellInput::default());
    for _ in 0..20 {
        state.route_machines_key(&key(KeyCode::Down), &mut ClientShellInput::default());
    }
    let _ = state.compose(110, 32).expect("composed frame");
    let candidate_rows: Vec<usize> = state
        .hits
        .machines_wizard_rows
        .iter()
        .map(|(_, index)| *index)
        .filter(|index| *index < 30)
        .collect();
    assert!(
        candidate_rows.contains(&20),
        "聚焦行在命中表里：{candidate_rows:?}"
    );
    assert!(
        candidate_rows.iter().copied().min().unwrap_or(0) > 0,
        "滚出窗口的行不再收录：{candidate_rows:?}"
    );
    let popup = state.hits.machines_popup;
    assert!(
        state
            .hits
            .machines_wizard_rows
            .iter()
            .all(|(rect, _)| rect.y >= popup.y && rect.bottom() <= popup.bottom()),
        "命中矩形不越出弹窗"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn import_done_step_scrolls_through_every_result_row() {
    let dir = with_temp_home("import-done");
    std::fs::write(dir.join(".ssh").join("config"), many_hosts_config(30)).unwrap();
    let mut state = state_with_profiles(&[]);
    state.open_machine_import_wizard();
    state.route_machines_key(&key(KeyCode::Enter), &mut ClientShellInput::default());
    state.route_machines_key(&key(KeyCode::Enter), &mut ClientShellInput::default());
    let (text, _) = frame_compact(&mut state, 110, 32);
    assert!(text.contains("host00"), "首条结果可见：{text}");
    assert!(!text.contains("host29"), "末条结果一屏放不下：{text}");
    for _ in 0..40 {
        state.route_machines_key(&key(KeyCode::Down), &mut ClientShellInput::default());
    }
    let (text, _) = frame_compact(&mut state, 110, 32);
    assert!(text.contains("host29"), "向下滚动后末条结果可见：{text}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// 把 `forward_remove_confirm_fmt` 按给定规则文案压成无空白串，便于在整帧
/// 文本里匹配。
fn forward_confirm_prompt(rule: &str) -> String {
    crate::i18n::fill(
        crate::i18n::texts().machines.forward_remove_confirm_fmt,
        &[("rule", rule)],
    )
    .chars()
    .filter(|ch| !ch.is_whitespace())
    .collect()
}

#[test]
fn forwards_editor_scrolls_to_the_selected_rule() {
    let dir = with_temp_home("forwards-scroll");
    // 目录上限是每台机器 16 条转发规则，取满额后选中末条。
    let saved = seed_forward_profile(16);
    let mut state = state_with_profiles(std::slice::from_ref(&saved));
    state.open_machines_overlay_for(&saved.id);
    state.route_machines_key(&key(KeyCode::Char('f')), &mut ClientShellInput::default());
    for _ in 0..15 {
        state.route_machines_key(&key(KeyCode::Down), &mut ClientShellInput::default());
    }
    let (text, _) = frame_compact(&mut state, 110, 32);
    assert!(
        text.contains("9015->127.0.0.1:80"),
        "选中的末条规则可见：{text}"
    );
    assert!(
        !text.contains("9000->127.0.0.1:80"),
        "列表已滚动，首条规则移出窗口：{text}"
    );

    // C-11 的完整链路：列表已滚动时 `x` 点名的必须是当前选中项，Enter 删掉
    // 的也必须是它，而不是窗口外的首条。
    state.route_machines_key(&key(KeyCode::Char('x')), &mut ClientShellInput::default());
    let (text, _) = frame_compact(&mut state, 110, 32);
    assert!(
        text.contains(&forward_confirm_prompt("local 9015 -> 127.0.0.1:80")),
        "确认文案点名滚动后的选中规则：{text}"
    );
    assert!(
        !text.contains(&forward_confirm_prompt("local 9000 -> 127.0.0.1:80")),
        "不得点名窗口外的首条规则：{text}"
    );
    state.route_machines_key(&key(KeyCode::Enter), &mut ClientShellInput::default());
    let catalog = crate::client::endpoint::EndpointCatalog::load().expect("catalog");
    assert_eq!(catalog.ssh[0].port_forwards.len(), 15);
    assert!(
        catalog.ssh[0]
            .port_forwards
            .iter()
            .all(|rule| rule.listen_port != 9015),
        "删掉的是被点名的那一条"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// 滚轮在转发编辑器里移动选中项，上界是规则条数：此前 `(selected + delta)`
/// 只有下界，能把 `selected` 推到规则数以外。
#[test]
fn forwards_editor_wheel_clamps_the_selection_to_the_rule_count() {
    let dir = with_temp_home("forwards-wheel");
    let saved = seed_forward_profile(4);
    let mut state = state_with_profiles(std::slice::from_ref(&saved));
    state.open_machines_overlay_for(&saved.id);
    state.route_machines_key(&key(KeyCode::Char('f')), &mut ClientShellInput::default());
    state.scroll_machines_overlay(100);
    let selected = match state.overlay.as_ref() {
        Some(ClientShellOverlay::Machines(overlay)) => match &overlay.view {
            super::super::machines_overlay::ClientMachinesView::Forwards(view) => view.selected,
            _ => panic!("forwards view"),
        },
        _ => panic!("overlay"),
    };
    assert_eq!(selected, 3, "滚轮不得把选中项推到规则数以外");
    state.scroll_machines_overlay(-100);
    let selected = match state.overlay.as_ref() {
        Some(ClientShellOverlay::Machines(overlay)) => match &overlay.view {
            super::super::machines_overlay::ClientMachinesView::Forwards(view) => view.selected,
            _ => panic!("forwards view"),
        },
        _ => panic!("overlay"),
    };
    assert_eq!(selected, 0);
    let _ = std::fs::remove_dir_all(&dir);
}

/// HERDR-MACH-025 的真实失败模式在 lease 层：长按 `x` 的自动重复不得把
/// 「武装 → 确认」一路走完。经 `handle_raw_events` 覆盖 Press + Repeat 与
/// 不上报 Repeat 的终端（连发 Press）两条路。
#[test]
fn held_x_does_not_walk_the_forward_removal() {
    use crossterm::event::KeyEventKind;

    let dir = with_temp_home("forwards-held-x");
    let saved = seed_forward_profile(4);
    let mut state = state_with_profiles(std::slice::from_ref(&saved));
    state.open_machines_overlay_for(&saved.id);
    state.route_machines_key(&key(KeyCode::Char('f')), &mut ClientShellInput::default());

    let remove = key(KeyCode::Char('x'));
    state.handle_raw_events(vec![RawInputEvent::Key(remove.clone())]);
    for _ in 0..5 {
        state.handle_raw_events(vec![RawInputEvent::Key(
            remove
                .clone()
                .with_kind(KeyEventKind::Repeat)
                .with_repeat_count(4),
        )]);
    }
    let catalog = crate::client::endpoint::EndpointCatalog::load().expect("catalog");
    assert_eq!(
        catalog.ssh[0].port_forwards.len(),
        4,
        "长按 x 不得删除任何规则"
    );

    // 不上报 Repeat 的终端：连发 Press 也只在武装/取消之间来回，绝不落盘。
    for _ in 0..6 {
        state.handle_raw_events(vec![RawInputEvent::Key(remove.clone())]);
    }
    let catalog = crate::client::endpoint::EndpointCatalog::load().expect("catalog");
    assert_eq!(
        catalog.ssh[0].port_forwards.len(),
        4,
        "连发 x 不得删除任何规则"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// 整套长按保护取决于 `step()` 为每个破坏性步进返回不同值（与
/// `snippets::overlay_step_separates_every_destructive_snippet_step` 同源）。
#[test]
fn overlay_step_separates_every_destructive_machine_step() {
    use super::super::machines_overlay::{
        ClientForwardRuleForm, ClientForwardRulesView, ClientMachinesOverlay, ClientMachinesView,
        PendingForwardRemoval,
    };

    let dir = with_temp_home("machine-step");
    let saved = seed_forward_profile(2);
    let armed = saved.port_forwards[0].clone();
    let forwards = |pending: Option<PendingForwardRemoval>| {
        ClientShellOverlay::Machines(ClientMachinesOverlay {
            view: ClientMachinesView::Forwards(Box::new(ClientForwardRulesView {
                profile_id: saved.id.clone(),
                from_list: false,
                selected: 0,
                scroll: 0,
                reveal: false,
                adding: false,
                form: ClientForwardRuleForm::blank(),
                pending_remove: pending,
                error: None,
                message: None,
            })),
            query: TextEditor::default(),
            search_focused: false,
            selected: 0,
            scroll: 0,
            reveal: false,
            detail_scroll: 0,
            view_max_scroll: 0,
            hovered: None,
            message: None,
        })
    };
    let idle = forwards(None);
    let pending = forwards(Some(PendingForwardRemoval {
        index: 0,
        rule: armed,
    }));
    assert_ne!(
        idle.step(),
        pending.step(),
        "转发删除武装前后必须是不同的步进指纹"
    );
    let confirm_remove = ClientShellOverlay::Machines(ClientMachinesOverlay {
        view: ClientMachinesView::ConfirmRemove(saved.id.clone()),
        query: TextEditor::default(),
        search_focused: false,
        selected: 0,
        scroll: 0,
        reveal: false,
        detail_scroll: 0,
        view_max_scroll: 0,
        hovered: None,
        message: None,
    });
    assert_ne!(idle.step(), confirm_remove.step());
    assert_ne!(pending.step(), confirm_remove.step());
    let _ = std::fs::remove_dir_all(&dir);
}

/// 目录 watcher（`set_endpoint_catalog`）会在浮层打开期间重新镜像规则表：
/// 武装与确认之间规则变了就取消确认，绝不按下标删掉另一条。
#[test]
fn forward_remove_is_cancelled_when_the_rules_change_while_armed() {
    let dir = with_temp_home("forwards-stale");
    let saved = seed_forward_profile(3);
    let mut state = state_with_profiles(std::slice::from_ref(&saved));
    state.open_machines_overlay_for(&saved.id);
    state.route_machines_key(&key(KeyCode::Char('f')), &mut ClientShellInput::default());
    state.route_machines_key(&key(KeyCode::Down), &mut ClientShellInput::default());
    state.route_machines_key(&key(KeyCode::Down), &mut ClientShellInput::default());
    state.route_machines_key(&key(KeyCode::Char('x')), &mut ClientShellInput::default());

    // 外部把规则表缩短：原来的下标 2 已不存在。
    let mut shortened = saved.clone();
    shortened.port_forwards.truncate(1);
    state.set_endpoint_catalog(std::slice::from_ref(&shortened));

    state.route_machines_key(&key(KeyCode::Enter), &mut ClientShellInput::default());
    let catalog = crate::client::endpoint::EndpointCatalog::load().expect("catalog");
    assert_eq!(
        catalog.ssh[0].port_forwards.len(),
        3,
        "规则变化后 Enter 不得落盘删除"
    );
    let (text, _) = frame_compact(&mut state, 110, 32);
    let stale: String = crate::i18n::texts()
        .machines
        .forward_remove_stale
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .collect();
    assert!(text.contains(&stale), "给出「规则已变化」提示：{text}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn forward_remove_asks_for_confirmation_before_deleting() {
    let dir = with_temp_home("forwards-confirm");
    let saved = seed_forward_profile(2);
    let mut state = state_with_profiles(std::slice::from_ref(&saved));
    state.open_machines_overlay_for(&saved.id);
    state.route_machines_key(&key(KeyCode::Char('f')), &mut ClientShellInput::default());

    // 第一次 x 只进入确认态，规则原封不动。
    state.route_machines_key(&key(KeyCode::Char('x')), &mut ClientShellInput::default());
    let catalog = crate::client::endpoint::EndpointCatalog::load().expect("catalog");
    assert_eq!(catalog.ssh[0].port_forwards.len(), 2, "x 不立即删除");
    let (text, _) = frame_compact(&mut state, 110, 32);
    assert!(
        text.contains(&forward_confirm_prompt("local 9000 -> 127.0.0.1:80")),
        "确认文案点名待删规则：{text}"
    );

    // 确认态下再按一次 `x` 只取消，不落盘：确认键必须与触发键不同。
    state.route_machines_key(&key(KeyCode::Char('x')), &mut ClientShellInput::default());
    let catalog = crate::client::endpoint::EndpointCatalog::load().expect("catalog");
    assert_eq!(catalog.ssh[0].port_forwards.len(), 2, "第二次 x 不确认删除");

    // Esc 取消确认，仍停在转发编辑器里。
    state.route_machines_key(&key(KeyCode::Char('x')), &mut ClientShellInput::default());
    state.route_machines_key(&key(KeyCode::Esc), &mut ClientShellInput::default());
    assert!(
        matches!(
            state.overlay,
            Some(ClientShellOverlay::Machines(
                super::super::machines_overlay::ClientMachinesOverlay {
                    view: super::super::machines_overlay::ClientMachinesView::Forwards(_),
                    ..
                }
            ))
        ),
        "取消确认不退出编辑器"
    );
    let catalog = crate::client::endpoint::EndpointCatalog::load().expect("catalog");
    assert_eq!(catalog.ssh[0].port_forwards.len(), 2);

    // x + enter 才真正删除。
    state.route_machines_key(&key(KeyCode::Char('x')), &mut ClientShellInput::default());
    state.route_machines_key(&key(KeyCode::Enter), &mut ClientShellInput::default());
    let catalog = crate::client::endpoint::EndpointCatalog::load().expect("catalog");
    assert_eq!(catalog.ssh[0].port_forwards.len(), 1, "确认后删除一条");
    assert_eq!(catalog.ssh[0].port_forwards[0].listen_port, 9001);
    let _ = std::fs::remove_dir_all(&dir);
}

/// 宽字符在帧里按显示宽度占两格，`frame_text` 会在它们之间留空格：比对
/// i18n 文案一律去空白后再比。
fn compact(text: &str) -> String {
    text.chars().filter(|ch| !ch.is_whitespace()).collect()
}

/// 机器面板 toast 的写入时刻；`None` 表示当前没有 toast。
fn machine_toast_at(state: &ClientShellState) -> Option<std::time::Instant> {
    match state.overlay.as_ref() {
        Some(ClientShellOverlay::Machines(overlay)) => {
            overlay.message.as_ref().map(|toast| toast.at)
        }
        _ => None,
    }
}

/// 只取页脚那几行（`OverlayRender::machines_toast` 报出来的 rect，也是 toast
/// 的落点）并按 rect 的列范围裁剪。对整帧做 contains 会被动作网格的按钮文案
/// 蒙对——`edit` ⊂ ` edit `、`重连` ⊂ ` 重连 `——所以页脚断言必须锚定到行。
fn machines_footer_text(state: &mut ClientShellState, cols: u16, rows: u16) -> String {
    let text = frame_text(state, cols, rows);
    let lines = text.lines().map(str::to_owned).collect::<Vec<_>>();
    let footer = state.hits.machines_footer;
    assert!(!footer.is_empty(), "该视图应报出页脚行");
    (footer.y..footer.bottom())
        .filter_map(|y| lines.get(y as usize))
        .map(|line| {
            line.chars()
                .skip(usize::from(footer.x))
                .take(usize::from(footer.width))
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// HERDR-MACH-006：`overlay.message` 是机器面板的公共 toast。Detail 视图按
/// `c` 之后反馈必须画在弹窗底部（此前唯一渲染点在窄屏 List 分支，Detail 与
/// 宽屏 dashboard 零反馈），并带时间戳由 feedback tick 自动清除。
#[test]
fn machine_toast_renders_in_the_detail_view_and_expires() {
    let saved = profile("Build", "dev@build.example", "60");
    let mut state = state_with_profiles(std::slice::from_ref(&saved));
    // 入场动画会让 `overlay_since` 也挂一个截止时刻，那样「toast 自己挂唤醒
    // 时刻」这条断言只要有动画就恒真。关掉动画，`chrome_feedback_deadline()`
    // 的唯一来源就只剩 toast。
    state.config.feedback.animations = false;
    state.open_machines_overlay_for(&saved.id);
    let mut outcome = ClientShellInput::default();
    state.route_machines_key(&key(KeyCode::Char('c')), &mut outcome);

    let copied = compact(crate::i18n::texts().machines.copied_fix_command);
    let text = frame_text(&mut state, 106, 36);
    assert!(
        compact(&text).contains(&copied),
        "Detail 视图应显示 toast: {text}"
    );

    // toast 必须自己挂一个截止时刻，否则没有后续事件时它永远不消失。
    let at = machine_toast_at(&state).expect("toast 写入时刻");
    assert_eq!(
        state.chrome_feedback_deadline(),
        Some(at + super::super::machines_overlay::MACHINE_TOAST_DURATION),
        "唤醒时刻必须由 toast 自己给出"
    );

    // 边界：到期前一刻不清，到期即清。
    let almost = at + super::super::machines_overlay::MACHINE_TOAST_DURATION
        - std::time::Duration::from_millis(1);
    assert!(!state.tick_chrome_feedback(almost), "到期前不应重绘");
    assert!(
        machine_toast_at(&state).is_some(),
        "到期前 toast 不应被清除"
    );

    let expired = at + super::super::machines_overlay::MACHINE_TOAST_DURATION;
    assert!(
        state.tick_chrome_feedback(expired),
        "toast 到期必须请求一次重绘"
    );
    assert!(machine_toast_at(&state).is_none(), "到期后 toast 应被清除");
    let text = frame_text(&mut state, 106, 36);
    assert!(
        !compact(&text).contains(&copied),
        "toast 到期后应消失: {text}"
    );
}

/// toast 是一次性反馈而不是入场动画：`feedback.animations` 开着时同样按
/// `MACHINE_TOAST_DURATION` 清除（提交注释声称的性质，此前无测试兜底）。
#[test]
fn machine_toast_expires_with_animations_enabled() {
    let saved = profile("Build", "dev@build.example", "64");
    let mut state = state_with_profiles(std::slice::from_ref(&saved));
    assert!(
        state.config.feedback.animations,
        "默认应开启入场动画，这条用例才有意义"
    );
    state.open_machines_overlay_for(&saved.id);
    state.route_machines_key(&key(KeyCode::Char('c')), &mut ClientShellInput::default());

    let at = machine_toast_at(&state).expect("toast 写入时刻");
    assert!(
        state.tick_chrome_feedback(at + super::super::machines_overlay::MACHINE_TOAST_DURATION),
        "toast 到期必须请求一次重绘"
    );
    assert!(machine_toast_at(&state).is_none(), "到期后 toast 应被清除");
}

/// 同一条 toast 在宽屏 dashboard 上也必须可见（向导成功的「机器 X 已就绪」
/// 此前在宽屏直接消失）。
#[test]
fn machine_toast_renders_on_the_wide_dashboard() {
    let saved = profile("Build", "dev@build.example", "61");
    let mut state = state_with_profiles(std::slice::from_ref(&saved));
    state.open_machines_overlay();
    let mut outcome = ClientShellInput::default();
    state.route_machines_key(&key(KeyCode::Char('c')), &mut outcome);

    let copied = compact(crate::i18n::texts().machines.copied_fix_command);
    let text = frame_text(&mut state, 130, 40);
    assert!(
        compact(&text).contains(&copied),
        "dashboard 应显示 toast: {text}"
    );
}

/// HERDR-MACH-007：宽屏 dashboard 接管 List 之后，网格里的「转发 / 浏览文件 /
/// 查看问题 / 复制修复命令」必须同样有键盘入口（`f/o/v/c`），取的是键盘选中
/// 的机器而不是指针悬浮的那台。
#[test]
fn wide_list_routes_the_machine_action_keys() {
    let dir = with_temp_home("machine-action-keys");
    let saved = profile("Build", "dev@build.example", "62");
    let mut state = state_with_profiles(std::slice::from_ref(&saved));
    state.open_machines_overlay();
    state.compose(130, 40).expect("dashboard frame");

    // f：端口转发编辑器。
    state.route_machines_key(&key(KeyCode::Char('f')), &mut ClientShellInput::default());
    assert!(
        matches!(
            state.overlay,
            Some(ClientShellOverlay::Machines(
                super::super::machines_overlay::ClientMachinesOverlay {
                    view: super::super::machines_overlay::ClientMachinesView::Forwards(_),
                    ..
                }
            ))
        ),
        "List 的 f 应进入转发编辑器"
    );
    // Esc 必须回列表：从列表按 `f` 进来的转发编辑器退回更窄的 Detail 模态
    // 正是 C-25 要消除的尺寸跳变。
    state.route_machines_key(&key(KeyCode::Esc), &mut ClientShellInput::default());
    assert!(
        matches!(
            state.overlay,
            Some(ClientShellOverlay::Machines(
                super::super::machines_overlay::ClientMachinesOverlay {
                    view: super::super::machines_overlay::ClientMachinesView::List,
                    ..
                }
            ))
        ),
        "从列表进入的转发编辑器按 Esc 应回列表，而不是 Detail"
    );

    // 从 Detail 进入时反过来：Esc 回 Detail。
    state.open_machine_detail(&saved.id);
    state.route_machines_key(&key(KeyCode::Char('f')), &mut ClientShellInput::default());
    state.route_machines_key(&key(KeyCode::Esc), &mut ClientShellInput::default());
    assert!(
        matches!(
            state.overlay,
            Some(ClientShellOverlay::Machines(
                super::super::machines_overlay::ClientMachinesOverlay {
                    view: super::super::machines_overlay::ClientMachinesView::Detail(_),
                    ..
                }
            ))
        ),
        "从详情进入的转发编辑器按 Esc 应回详情"
    );
    state.route_machines_key(&key(KeyCode::Esc), &mut ClientShellInput::default());

    // o：远程文件浏览器。
    state.route_machines_key(&key(KeyCode::Char('o')), &mut ClientShellInput::default());
    assert!(
        matches!(state.overlay, Some(ClientShellOverlay::MachineFiles(_))),
        "List 的 o 应打开远程文件浏览器"
    );

    // c：复制修复命令。
    state.open_machines_overlay();
    let mut outcome = ClientShellInput::default();
    state.route_machines_key(&key(KeyCode::Char('c')), &mut outcome);
    let [ClientShellAction::ClipboardWrite(bytes)] = &outcome.actions[..] else {
        panic!("clipboard write: {:?}", outcome.actions);
    };
    assert_eq!(
        String::from_utf8_lossy(bytes),
        "herdr --remote dev@build.example --session default"
    );

    // v：失败复核（只有真有失败时才有落点）。
    state.set_endpoint_connection_error_kind(
        &ClientEndpointId::Ssh(saved.id.clone()),
        Some(crate::remote::ConnectionErrorKind::HostKeyChanged),
    );
    state.route_machines_key(&key(KeyCode::Char('v')), &mut ClientShellInput::default());
    assert!(
        matches!(state.overlay, Some(ClientShellOverlay::MachineAuth(_))),
        "List 的 v 应打开失败复核"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// dashboard 页脚必须和它自己的动作网格同一套键，否则用户看得到按钮却以为
/// 只有 `↑↓ / Enter / / / Esc`。断言锚定在页脚那几行（整帧 contains 会被
/// 动作网格按钮文案蒙对），并在三个现实宽度与两种语言上各跑一遍。
#[test]
fn dashboard_footer_advertises_the_action_grid_keys() {
    for lang in [crate::i18n::Lang::ZhCn, crate::i18n::Lang::En] {
        let _guard = crate::i18n::lang_guard(lang);
        for cols in [96u16, 116, 130] {
            let saved = profile("Build", "dev@build.example", "63");
            let mut state = state_with_profiles(std::slice::from_ref(&saved));
            state.open_machines_overlay();
            let footer = compact(&machines_footer_text(&mut state, cols, 40));
            let t = &crate::i18n::texts().machines;
            // 动作网格里的每个按钮都要在页脚有对应键，外加页面级键。
            for hint in [
                t.hint_reconnect,
                t.hint_edit,
                t.hint_browse_files,
                t.hint_forwards,
                t.hint_toggle_enabled,
                t.hint_remove,
                t.hint_details,
                t.hint_select,
                t.hint_filter,
                t.hint_close,
                t.hint_broadcast,
            ] {
                assert!(
                    footer.contains(&compact(hint)),
                    "{lang:?} @ {cols} 列页脚缺少 {hint}: {footer}"
                );
            }
        }
    }
}

/// 页面级键（`/` 过滤、`esc` 关闭、`b` 广播）在任何现实宽度下都不能被单机
/// 动作键挤掉——它们在 List / dashboard 上没有等效的可见入口。
#[test]
fn machine_page_level_keys_survive_every_width() {
    for lang in [crate::i18n::Lang::ZhCn, crate::i18n::Lang::En] {
        let _guard = crate::i18n::lang_guard(lang);
        for (cols, rows) in [(80u16, 30u16), (96, 34), (116, 36), (130, 40)] {
            let saved = profile("Build", "dev@build.example", "65");
            let mut state = state_with_profiles(std::slice::from_ref(&saved));
            state.open_machines_overlay();
            let footer = compact(&machines_footer_text(&mut state, cols, rows));
            let t = &crate::i18n::texts().machines;
            for hint in [t.hint_filter, t.hint_close, t.hint_broadcast] {
                assert!(
                    footer.contains(&compact(hint)),
                    "{lang:?} @ {cols} 列页脚缺少页面级键 {hint}: {footer}"
                );
            }
        }
    }
}

/// 窄屏 List 的 `v 处理失败` 此前恒被写死成 false，键能用但页脚永远不提示。
#[test]
fn narrow_list_footer_advertises_review_when_the_failure_has_one() {
    let saved = profile("Build", "dev@build.example", "66");
    let mut state = state_with_profiles(std::slice::from_ref(&saved));
    state.open_machines_overlay();
    let review = compact(crate::i18n::texts().machines.hint_review);

    // 没有失败时不提示（否则按下去是空操作）。
    let footer = compact(&machines_footer_text(&mut state, 80, 30));
    assert!(!footer.contains(&review), "无失败时不应提示 v: {footer}");

    state.set_endpoint_connection_error_kind(
        &ClientEndpointId::Ssh(saved.id.clone()),
        Some(crate::remote::ConnectionErrorKind::HostKeyChanged),
    );
    let footer = compact(&machines_footer_text(&mut state, 80, 30));
    assert!(
        footer.contains(&review),
        "窄屏 List 有可复核失败时页脚应出现 v: {footer}"
    );
}

/// C-27 点名的第三个触发点：侧栏右键菜单的「复制修复命令」。机器面板没开，
/// 反馈必须走通用通知通道，而不是无声无息。
#[test]
fn sidebar_copy_fix_command_reports_feedback_without_the_machines_page() {
    let saved = profile("Build", "dev@build.example", "67");
    let mut state = state_with_profiles(std::slice::from_ref(&saved));
    let endpoint_id = ClientEndpointId::Ssh(saved.id.clone());
    state.open_machine_context_menu(&endpoint_id, 0, 0);
    let index = match state.overlay.as_ref() {
        Some(ClientShellOverlay::ContextMenu(menu)) => menu
            .items()
            .iter()
            .position(|item| {
                item.action == super::super::state::ClientContextMenuAction::CopyMachineFixCommand
            })
            .expect("复制修复命令项"),
        _ => panic!("右键菜单未打开"),
    };
    let mut outcome = ClientShellInput::default();
    state.activate_context_menu_item(index, &mut outcome);

    let [ClientShellAction::ClipboardWrite(bytes)] = &outcome.actions[..] else {
        panic!("clipboard write: {:?}", outcome.actions);
    };
    assert_eq!(
        String::from_utf8_lossy(bytes),
        "herdr --remote dev@build.example --session default"
    );
    assert_eq!(
        state
            .visible_endpoint_notice
            .as_ref()
            .map(|notice| notice.title.as_str()),
        Some(crate::i18n::texts().machines.copied_fix_command),
        "面板未打开时必须有通用反馈"
    );
}

/// STATE-04 守门（独立复审 中-2）：机器列表的滚动状态只在视图计算阶段更新，
/// 渲染是纯函数——连续 compose 之间逐字段不变。
#[test]
fn composing_twice_leaves_the_machine_list_scroll_untouched() {
    let profiles: Vec<SavedSshEndpoint> = (0..12)
        .map(|index| {
            profile(
                &format!("machine-{index:02}"),
                &format!("host-{index}.example"),
                &format!("{index}"),
            )
        })
        .collect();
    let mut state = state_with_profiles(&profiles);
    state.open_machines_overlay();
    state.compose(106, 16).expect("first frame");
    let scroll_state = |state: &ClientShellState| match state.overlay.as_ref() {
        Some(ClientShellOverlay::Machines(overlay)) => {
            (overlay.scroll, overlay.selected, overlay.reveal)
        }
        _ => panic!("machines overlay"),
    };
    let before = scroll_state(&state);

    // 键盘移动一次：选中行被推离初始值，reveal 被消费。
    state.handle_raw_events(vec![RawInputEvent::Key(key(KeyCode::Char('j')))]);
    state.compose(106, 16).expect("second frame");
    let after_input = scroll_state(&state);
    assert_ne!(
        after_input.1, before.1,
        "输入应移动机器列表的选中行：{before:?} -> {after_input:?}"
    );

    // 再画一帧：没有任何输入，状态必须逐字段不变。
    state.compose(106, 16).expect("third frame");
    assert_eq!(
        scroll_state(&state),
        after_input,
        "重绘不得改写机器列表的滚动状态"
    );
}
