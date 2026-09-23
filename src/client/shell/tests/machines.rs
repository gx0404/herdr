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
        t.enable_machine,
        t.remove_machine,
        t.copy_machine_fix_command,
    ] {
        assert!(labels.contains(&expected), "missing {expected}: {labels:?}");
    }
    // 启用是勾选项（不再用「启用 / 禁用」两套文案）；已在线的机器「立即重连」
    // 置灰而不是剔除。
    let item = |menu: &ClientContextMenuOverlay, action| {
        menu.items()
            .into_iter()
            .find(|item| item.action == action)
            .unwrap_or_else(|| panic!("{action:?} 应列出"))
    };
    assert_eq!(
        item(menu, ClientContextMenuAction::ToggleMachineEnabled).checked,
        Some(true)
    );
    assert!(item(menu, ClientContextMenuAction::ReconnectMachine).enabled);
    state.set_endpoint_status(&build_id, ClientEndpointStatus::Online);
    state.cache_endpoint_snapshot(&build_id, Box::new(snapshot()));
    state.open_machine_context_menu(&build_id, 4, 4);
    let Some(ClientShellOverlay::ContextMenu(menu)) = state.overlay.as_ref() else {
        panic!("context menu");
    };
    assert!(!item(menu, ClientContextMenuAction::ReconnectMachine).enabled);
    assert_eq!(
        item(menu, ClientContextMenuAction::ToggleMachineEnabled).checked,
        Some(true)
    );
}

#[test]
fn global_menu_and_prefix_m_open_the_machines_overlay() {
    let snapshot = snapshot();
    let items = super::super::global_menu::global_menu_items(&snapshot);
    assert!(items
        .iter()
        .any(|entry| entry.id == super::super::action_table::ActionId::ManageMachines));

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

    // 停用后「启用」勾选项取消勾选，「立即重连」置灰。
    state.open_machine_context_menu(&endpoint_id, 4, 4);
    let Some(ClientShellOverlay::ContextMenu(menu)) = state.overlay.as_ref() else {
        panic!("context menu");
    };
    let items = menu.items();
    let toggle = items
        .iter()
        .find(|item| item.action == ClientContextMenuAction::ToggleMachineEnabled)
        .expect("启用勾选项");
    assert_eq!(
        toggle.label,
        crate::i18n::texts().context_menu.enable_machine
    );
    assert_eq!(toggle.checked, Some(false));
    assert!(items
        .iter()
        .any(|item| item.action == ClientContextMenuAction::ReconnectMachine && !item.enabled));
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

/// dashboard 不再另画动作网格：页脚就是单机动作与页面动作的唯一入口，
/// 每个动作键都得出现在页脚里（且可点，见
/// `machine_pages_have_one_clickable_footer_instead_of_a_button_row`）。断言
/// 锚定在页脚那几行，并在三个现实宽度与两种语言上各跑一遍。
#[test]
fn dashboard_footer_lists_every_machine_action_key() {
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

// ---------------------------------------------------------------------
// 单页添加 / 编辑表单：快速输入、分组字段、内联校验、测试连接与保存
// ---------------------------------------------------------------------

use super::super::machines_overlay::{ClientMachineForm, MachineField, MachineOverlayButton};

fn ctrl(ch: char) -> crate::input::TerminalKey {
    crate::input::TerminalKey::new(KeyCode::Char(ch), KeyModifiers::CONTROL)
}

fn machine_form(state: &ClientShellState) -> &ClientMachineForm {
    let Some(ClientShellOverlay::Machines(overlay)) = state.overlay.as_ref() else {
        panic!("machines overlay");
    };
    let super::super::machines_overlay::ClientMachinesView::Form(form) = &overlay.view else {
        panic!("form view");
    };
    form
}

fn machine_form_mut(state: &mut ClientShellState) -> &mut ClientMachineForm {
    let Some(ClientShellOverlay::Machines(overlay)) = state.overlay.as_mut() else {
        panic!("machines overlay");
    };
    let super::super::machines_overlay::ClientMachinesView::Form(form) = &mut overlay.view else {
        panic!("form view");
    };
    form
}

/// 打开添加表单并直接写好目标 / 标签（绕过快速输入），焦点放在目标上。
fn add_form_overlay<'a>(
    state: &'a mut ClientShellState,
    target: &str,
    label: &str,
) -> &'a mut ClientMachineForm {
    state.open_machine_add_form();
    let form = machine_form_mut(state);
    form.target = TextEditor::new(target, false);
    form.label = TextEditor::new(label, false);
    form.focused = 1;
    form
}

fn focused(state: &ClientShellState) -> Option<MachineField> {
    let form = machine_form(state);
    form.fields().get(form.focused).copied()
}

fn press(state: &mut ClientShellState, key: crate::input::TerminalKey) -> ClientShellInput {
    let mut outcome = ClientShellInput::default();
    assert!(state.route_machines_key(&key, &mut outcome));
    outcome
}

fn type_text(state: &mut ClientShellState, text: &str) {
    for ch in text.chars() {
        press(state, key(KeyCode::Char(ch)));
    }
}

fn focus_field(state: &mut ClientShellState, field: MachineField) {
    let form = machine_form_mut(state);
    let index = form
        .fields()
        .iter()
        .position(|candidate| *candidate == field)
        .expect("focusable field");
    form.set_focus(index);
}

fn field_hit(state: &ClientShellState, field: MachineField) -> Rect {
    state
        .hits
        .machines_fields
        .iter()
        .find(|(_, candidate)| *candidate == field)
        .map(|(rect, _)| *rect)
        .unwrap_or_else(|| panic!("{field:?} not in hits: {:?}", state.hits.machines_fields))
}

fn left_click(col: u16, row: u16) -> crossterm::event::MouseEvent {
    crossterm::event::MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: col,
        row,
        modifiers: KeyModifiers::empty(),
    }
}

/// 测试连接是两段式：第一次 Ctrl+T 只弹确认条，第二次才下发 bootstrap。
fn start_test(state: &mut ClientShellState) -> u64 {
    let outcome = press(state, ctrl('t'));
    assert!(
        outcome.actions.is_empty(),
        "确认前不下发：{:?}",
        outcome.actions
    );
    let outcome = press(state, ctrl('t'));
    let [ClientShellAction::BootstrapMachine { ticket, .. }] = &outcome.actions[..] else {
        panic!("bootstrap action: {:?}", outcome.actions);
    };
    *ticket
}

#[test]
fn add_form_is_one_page_with_quick_input_groups_preview_and_footer() {
    let mut state = state_with_profiles(&[]);
    state.open_machine_add_form();
    let text = compact(&frame_text(&mut state, 120, 40));
    let t = &crate::i18n::texts().machines;
    let f = &crate::i18n::texts().machine_form;
    for expected in [
        t.add_title,
        f.quick_label,
        f.quick_placeholder,
        f.group_connection,
        f.group_auth,
        f.group_session,
        f.preview_title,
        f.test_connection,
        t.save_button,
    ] {
        assert!(
            text.contains(&compact(expected)),
            "单页表单缺少「{expected}」：{text}"
        );
    }
    // 不再有分步向导的步骤条。
    assert!(!text.contains("1目标"), "{text}");
    assert_eq!(
        focused(&state),
        Some(MachineField::Quick),
        "焦点从快速输入开始"
    );
    // 目标是必填：标签后带红星。
    let target = field_hit(&state, MachineField::Target);
    let frame = state.compose(120, 40).expect("frame");
    let row: String = frame.cells[usize::from(target.y) * usize::from(frame.width)..]
        .iter()
        .take(usize::from(frame.width))
        .map(|cell| cell.symbol.as_str())
        .collect();
    assert!(row.contains('*'), "必填星号：{row}");
    // 页脚是一条可点的提示：测试连接、保存都在命中表里。
    let buttons: Vec<_> = state
        .hits
        .machines_actions
        .iter()
        .map(|(_, button)| *button)
        .collect();
    assert!(
        buttons.contains(&MachineOverlayButton::TestConnection),
        "{buttons:?}"
    );
    assert!(buttons.contains(&MachineOverlayButton::Save), "{buttons:?}");
    assert!(buttons.contains(&MachineOverlayButton::Back), "{buttons:?}");
}

#[test]
fn pasting_an_ssh_command_fills_fields_and_the_live_preview() {
    let mut state = state_with_profiles(&[]);
    state.open_machine_add_form();
    assert!(state
        .insert_machines_overlay_text("ssh -p 2222 \\\n  -i ~/.ssh/k -J jump dev@build.example"));
    let form = machine_form(&state);
    assert_eq!(form.target.as_str(), "build.example");
    assert_eq!(form.user.as_str(), "dev");
    assert_eq!(form.port.as_str(), "2222");
    assert_eq!(form.identity_files.as_str(), "~/.ssh/k");
    assert_eq!(form.proxy_jump.as_str(), "jump");
    let text = compact(&frame_text(&mut state, 120, 40));
    let filled = crate::i18n::fill(
        crate::i18n::texts().machine_form.quick_parsed_fmt,
        &[("n", "5")],
    );
    assert!(text.contains(&compact(&filled)), "解析结论：{text}");
    assert!(
        text.contains("ssh-p2222-i~/.ssh/k-Jjumpdev@build.example"),
        "右侧预览是等价的 ssh 命令：{text}"
    );
    // 粘贴即已解析：Enter 直接保存（这里只断言不再是「填入」）。
    assert!(!machine_form(&state).quick_pending());
}

#[test]
fn typed_quick_input_parses_on_enter_and_reports_malformed_input() {
    let _dir = with_temp_state_home("form-quick-enter");
    let mut state = state_with_profiles(&[]);
    state.open_machine_add_form();
    type_text(&mut state, "ops@10.0.0.5:2200");
    assert!(machine_form(&state).quick_pending(), "打字不立即覆盖字段");
    assert_eq!(machine_form(&state).target.as_str(), "");
    press(&mut state, key(KeyCode::Enter));
    let form = machine_form(&state);
    assert_eq!(
        (form.target.as_str(), form.user.as_str(), form.port.as_str()),
        ("10.0.0.5", "ops", "2200")
    );
    assert!(
        crate::client::endpoint::EndpointCatalog::load()
            .expect("catalog")
            .ssh
            .is_empty(),
        "Enter 只是填入，不保存"
    );

    // 畸形输入：内联报错，已填的字段不动。
    machine_form_mut(&mut state).quick = TextEditor::default();
    type_text(&mut state, "root:secret@host");
    press(&mut state, key(KeyCode::Enter));
    assert_eq!(machine_form(&state).target.as_str(), "10.0.0.5");
    let text = compact(&frame_text(&mut state, 120, 40));
    let failed = compact(crate::i18n::texts().machine_form.quick_parse_failed);
    assert!(text.contains(&failed), "解析失败提示：{text}");
    // 失焦同样触发解析（这里内容没变，只是离开）：焦点移到目标。
    press(&mut state, key(KeyCode::Tab));
    assert_eq!(focused(&state), Some(MachineField::Target));
}

#[test]
fn keyboard_focus_moves_with_tab_shift_tab_and_arrows() {
    let mut state = state_with_profiles(&[]);
    state.open_machine_add_form();
    press(&mut state, key(KeyCode::Tab));
    assert_eq!(focused(&state), Some(MachineField::Target));
    press(&mut state, key(KeyCode::Down));
    assert_eq!(focused(&state), Some(MachineField::User));
    press(&mut state, key(KeyCode::Up));
    press(&mut state, key(KeyCode::Up));
    assert_eq!(focused(&state), Some(MachineField::Quick));
    press(&mut state, key(KeyCode::Up));
    assert_eq!(focused(&state), Some(MachineField::Quick), "↑ 到顶不回绕");
    press(
        &mut state,
        crate::input::TerminalKey::new(KeyCode::BackTab, KeyModifiers::SHIFT),
    );
    assert_eq!(
        focused(&state),
        Some(MachineField::SessionLogInterval),
        "Shift+Tab 回绕到最后一项"
    );
    // 选择字段用 ←→ / 空格切换取值。
    focus_field(&mut state, MachineField::ForwardAgent);
    press(&mut state, key(KeyCode::Right));
    assert!(matches!(
        machine_form(&state).forward_agent,
        super::super::machines_overlay::TriChoice::Yes
    ));
    // 编辑表单：没有快速输入，目标只读不聚焦。
    let saved = profile("Build", "build.example", "70");
    let mut state = state_with_profiles(std::slice::from_ref(&saved));
    state.open_machine_edit_form(&saved.id);
    assert_eq!(focused(&state), Some(MachineField::User));
    let text = compact(&frame_text(&mut state, 120, 40));
    assert!(text.contains("build.example"), "目标只读展示：{text}");
    assert!(
        !text.contains(&compact(crate::i18n::texts().machine_form.quick_label)),
        "编辑表单没有快速输入：{text}"
    );
}

#[test]
fn inline_validation_waits_for_blur_or_submit_then_blocks_save() {
    let _dir = with_temp_state_home("form-validation");
    let mut state = state_with_profiles(&[]);
    state.open_machine_add_form();
    let t = &crate::i18n::texts().machine_form;
    let text = compact(&frame_text(&mut state, 120, 40));
    assert!(
        !text.contains(&compact(t.err_required)),
        "首屏不报错：{text}"
    );

    // 端口：输入时就校验（动过的字段）。
    focus_field(&mut state, MachineField::Port);
    type_text(&mut state, "99999");
    let text = compact(&frame_text(&mut state, 120, 40));
    assert!(text.contains(&compact(t.err_port)), "端口内联错误：{text}");
    let port = field_hit(&state, MachineField::Port);
    let frame = state.compose(120, 40).expect("frame");
    let error_cell =
        &frame.cells[usize::from(port.y + 2) * usize::from(frame.width) + usize::from(port.x)];
    assert_eq!(
        error_cell.fg,
        crate::protocol::color_to_u32(state.config.palette.red),
        "聚焦中的错误行染红"
    );
    assert!(frame.cursor.is_some(), "聚焦且有错时光标仍在");

    // 目标没动过：失焦前不报；提交（Enter 保存）后全部现形、焦点跳到第一个错处。
    press(&mut state, key(KeyCode::Enter));
    assert_eq!(focused(&state), Some(MachineField::Target));
    let text = compact(&frame_text(&mut state, 120, 40));
    assert!(
        text.contains(&compact(t.err_required)),
        "提交后目标必填：{text}"
    );
    assert!(text.contains(&compact(t.fix_fields)), "表单级提示：{text}");
    assert!(
        crate::client::endpoint::EndpointCatalog::load()
            .expect("catalog")
            .ssh
            .is_empty(),
        "有错不落盘"
    );
    // 测试连接同样被拦下：连确认条都不弹。
    let outcome = press(&mut state, ctrl('t'));
    assert!(outcome.actions.is_empty(), "{:?}", outcome.actions);
    assert!(machine_form(&state).bootstrap.is_none());
    assert!(machine_form(&state).prompt.is_none());
}

/// L15：端口报错后，右侧实时预览此前原样显示非法值（字段清单里的「端口
/// 99999」一行）没有任何标记，像是校验通过了。
#[test]
fn preview_flags_invalid_field_values_instead_of_showing_them_as_if_valid() {
    let _dir = with_temp_state_home("form-preview-invalid");
    let mut state = state_with_profiles(&[]);
    state.open_machine_add_form();
    let t = &crate::i18n::texts().machine_form;
    focus_field(&mut state, MachineField::Target);
    type_text(&mut state, "db.example");
    focus_field(&mut state, MachineField::Port);
    type_text(&mut state, "99999");

    // 宽屏下预览栏可见，字段仍处于校验失败状态（动过、未失焦）。
    let text = compact(&frame_text(&mut state, 130, 40));
    assert!(
        text.contains(&compact(t.err_port)),
        "端口内联错误仍在：{text}"
    );
    let expected_value = format!("99999{}", t.preview_invalid_suffix);
    assert!(
        text.contains(&compact(&expected_value)),
        "预览应该把非法端口标成「99999（无效）」：{text}"
    );

    // 预览栏里带端口号的那一行要标红，不能和其它正常字段同色。
    let frame = state.compose(130, 40).expect("frame");
    let width = usize::from(frame.width);
    let red = crate::protocol::color_to_u32(state.config.palette.red);
    let found_red_port = frame.cells.chunks(width).any(|cells| {
        let row: String = cells.iter().map(|c| c.symbol.as_str()).collect();
        match row.find("99999") {
            Some(byte_offset) => {
                let column = row[..byte_offset].chars().count();
                cells.get(column).is_some_and(|cell| cell.fg == red)
            }
            None => false,
        }
    });
    assert!(found_red_port, "预览里的非法端口值应该标红");
}

/// 在整帧里按单元格找一段 ASCII 文本，返回 (行, 起始列)。宽字符的续格不参与
/// 比较，行内有 CJK 时列号仍是真实的屏幕列。
fn find_ascii_cells(frame: &crate::protocol::FrameData, needle: &str) -> Option<(usize, usize)> {
    let width = usize::from(frame.width);
    let needle: Vec<char> = needle.chars().collect();
    frame
        .cells
        .chunks(width)
        .enumerate()
        .find_map(|(row, cells)| {
            (0..cells.len().saturating_sub(needle.len() - 1)).find_map(|col| {
                needle
                    .iter()
                    .enumerate()
                    .all(|(offset, ch)| cells[col + offset].symbol.as_str() == ch.to_string())
                    .then_some((row, col))
            })
        })
}

/// 复审 L15（中）：预览首行的等价 ssh 命令此前整行 accent 色，端口 99999 报错
/// 后仍原样显示 `ssh -p 99999 user@203.0.113.5`，看着像已通过校验
/// （`smoke-23627143` `84` 行 11；上一版只修了行 15 的字段清单）。非法字段
/// 对应的那个词要单独标红并紧跟「（无效）」，其余词保持 accent。
#[test]
fn preview_ssh_line_marks_only_the_invalid_word() {
    let _dir = with_temp_state_home("form-preview-ssh-invalid");
    let mut state = state_with_profiles(&[]);
    state.open_machine_add_form();
    focus_field(&mut state, MachineField::Target);
    type_text(&mut state, "203.0.113.5");
    focus_field(&mut state, MachineField::User);
    type_text(&mut state, "user");
    focus_field(&mut state, MachineField::Port);
    type_text(&mut state, "99999");
    focus_field(&mut state, MachineField::ProxyJump);

    let frame = state.compose(134, 32).expect("frame");
    let width = usize::from(frame.width);
    let red = crate::protocol::color_to_u32(state.config.palette.red);
    let accent = crate::protocol::color_to_u32(state.config.palette.accent);
    let (row, col) = find_ascii_cells(&frame, "ssh -p 99999").expect("预览首行是等价 ssh 命令");
    let cells = &frame.cells[row * width..(row + 1) * width];
    assert_eq!(cells[col].fg, accent, "命令本身保持 accent");
    for offset in 4..12 {
        assert_eq!(
            cells[col + offset].fg,
            red,
            "非法端口所在的词 `-p 99999` 应当整个标红（第 {offset} 格）"
        );
    }
    let rest: String = cells[col + 12..]
        .iter()
        .map(|c| c.symbol.as_str())
        .collect();
    let suffix = crate::i18n::texts().machine_form.preview_invalid_suffix;
    assert!(
        compact(&rest).starts_with(&compact(suffix)),
        "非法的词后面紧跟「无效」标记，不只靠颜色：{rest:?}"
    );
    let suffix_col = (col + 12..width)
        .find(|&index| !cells[index].symbol.trim().is_empty())
        .expect("「无效」标记");
    assert_eq!(cells[suffix_col].fg, red, "「无效」标记同样标红");
    let (target_row, target_col) =
        find_ascii_cells(&frame, "user@203.0.113.5").expect("目标仍在 ssh 行里");
    assert_eq!(target_row, row, "目标与端口在同一行（134 列下不折行）");
    assert_eq!(
        cells[target_col].fg, accent,
        "合法的目标保持 accent，不被一并标红"
    );
}

/// 复审 L14 附带：`wrap_lines_for_display` 只读 span 自己的样式，`Line::styled`
/// 的行级颜色在折行后全部丢成终端默认色——冒烟 `81` 行 16 的安装说明原本是
/// yellow，上一版之后变成默认前景（测试步骤的绿 / 红同理）。
#[test]
fn preview_notes_keep_their_colors_after_wrapping() {
    let mut state = state_with_profiles(&[]);
    state.open_machine_add_form();
    let frame = state.compose(134, 32).expect("frame");
    let width = usize::from(frame.width);
    let yellow = crate::protocol::color_to_u32(state.config.palette.yellow);
    let (row, col) = find_ascii_cells(&frame, "Herdr").expect("预览里的安装说明");
    assert_eq!(
        frame.cells[row * width + col].fg,
        yellow,
        "安装说明应当是 yellow"
    );
    let (next_row, next_col) = find_ascii_cells(&frame, "server").expect("安装说明折行后的下半句");
    assert!(next_row > row, "43 列预览栏下安装说明要折成两行");
    assert_eq!(
        frame.cells[next_row * width + next_col].fg,
        yellow,
        "折到下一行的部分同样是 yellow"
    );
}

/// 复审 M8（严重）：添加表单文本字段的 Normal / Focused / Invalid 三态在输入行
/// 上必须肉眼可辨，占位符也不能被聚焦底色吞掉。对照 `smoke-23627143`：`81`
/// 行 6（聚焦的快速添加：占位符 overlay0 叠 surface0，与常态逐字段相同）、
/// `81` 行 13（空的可选字段「用户」只剩一行底色）、`84` 行 15–16（端口 99999
/// 报错：标签与错误行标红，输入行仍是常态底色）。上一版修复给聚焦态换的
/// 「更亮底色」在 terminal 主题下与占位符同为 Gray（整行占位符不可见）、在
/// vesper / rose-pine 下与常态的对比度不足 1.10，所以逐主题扫一遍。
#[test]
fn add_form_text_field_states_stay_distinct_across_themes() {
    use crate::app::state::Palette;
    use crate::protocol::{u16_to_modifier, u32_to_color};
    use crate::ui::color::contrast_ratio;
    use ratatui::style::Modifier;

    let _dir = with_temp_state_home("form-field-states");
    let texts = crate::i18n::texts();
    let marks = Modifier::UNDERLINED | Modifier::BOLD;
    for (name, palette) in [
        ("catppuccin", Palette::catppuccin()),
        ("terminal", Palette::terminal()),
        ("vesper", Palette::vesper()),
        ("rose-pine", Palette::rose_pine()),
        ("dracula", Palette::dracula()),
    ] {
        let mut state = state_with_profiles(&[]);
        state.config.palette = palette.clone();
        state.open_machine_add_form();
        let frame = state.compose(134, 32).expect("frame");
        let width = usize::from(frame.width);
        let cell = |x: u16, y: u16| &frame.cells[usize::from(y) * width + usize::from(x)];
        let row_from = |x: u16, y: u16| -> String {
            frame.cells[usize::from(y) * width + usize::from(x)..(usize::from(y) + 1) * width]
                .iter()
                .map(|c| c.symbol.as_str())
                .collect()
        };

        // Focused：打开即聚焦快速添加，输入行首格是占位符的首字符。
        let quick = field_hit(&state, MachineField::Quick);
        let focused = cell(quick.x, quick.y + 1);
        assert!(
            compact(&row_from(quick.x, quick.y + 1))
                .starts_with(&compact(texts.machine_form.quick_placeholder)),
            "{name}：聚焦的快速添加应当画占位符"
        );
        assert_ne!(
            focused.fg, focused.bg,
            "{name}：聚焦态占位符与底色同色，整行不可见"
        );

        // Normal：空的可选字段「用户」画「未设置」占位符，不再只剩一行底色。
        let user = field_hit(&state, MachineField::User);
        let normal = cell(user.x, user.y + 1);
        assert!(
            compact(&row_from(user.x, user.y + 1))
                .starts_with(&compact(texts.machines.value_not_set)),
            "{name}：空的可选字段应当画「未设置」占位符：{:?}",
            row_from(user.x, user.y + 1)
        );
        assert_ne!(normal.fg, normal.bg, "{name}：常态占位符与底色同色");
        assert_ne!(
            normal.bg,
            crate::protocol::color_to_u32(palette.panel_bg),
            "{name}：常态输入行与面板同色，字段没有边界"
        );

        // 聚焦与常态可辨：底色换了一档且过 1.10 门槛，或加了非颜色标记。
        let focused_marks = u16_to_modifier(focused.modifier) & marks;
        let normal_marks = u16_to_modifier(normal.modifier) & marks;
        if focused.bg == normal.bg {
            assert_ne!(
                focused_marks, normal_marks,
                "{name}：聚焦态与常态底色相同，又没有非颜色标记，看不出聚焦"
            );
        } else {
            let step = contrast_ratio(u32_to_color(focused.bg), u32_to_color(normal.bg));
            assert!(
                step.is_none_or(|ratio| ratio >= 1.10),
                "{name}：聚焦底色与常态只差 {step:?}，肉眼不可辨"
            );
            // 换了底色时占位符不能比常态更难读（catppuccin 曾从 2.57 掉到 1.87）。
            let focused_ratio = contrast_ratio(u32_to_color(focused.fg), u32_to_color(focused.bg));
            let normal_ratio = contrast_ratio(u32_to_color(normal.fg), u32_to_color(normal.bg));
            if let (Some(focused_ratio), Some(normal_ratio)) = (focused_ratio, normal_ratio) {
                assert!(
                    focused_ratio + 0.01 >= normal_ratio,
                    "{name}：聚焦态占位符对比度 {focused_ratio:.2} 低于常态 {normal_ratio:.2}"
                );
            }
        }
    }

    // Invalid（`84` 行 15–16）：端口 99999 失焦后标签与错误行标红，输入行仍是
    // 常态底色，值照常显示。
    let mut state = state_with_profiles(&[]);
    state.open_machine_add_form();
    focus_field(&mut state, MachineField::Port);
    type_text(&mut state, "99999");
    focus_field(&mut state, MachineField::ProxyJump);
    let frame = state.compose(134, 32).expect("frame");
    let width = usize::from(frame.width);
    let cell = |x: u16, y: u16| &frame.cells[usize::from(y) * width + usize::from(x)];
    let red = crate::protocol::color_to_u32(state.config.palette.red);
    let port = field_hit(&state, MachineField::Port);
    let user = field_hit(&state, MachineField::User);
    assert_eq!(cell(port.x, port.y).fg, red, "非法字段的标签应当标红");
    assert_eq!(cell(port.x, port.y + 1).symbol, "9", "输入行照常显示值");
    assert_eq!(
        cell(port.x, port.y + 1).bg,
        cell(user.x, user.y + 1).bg,
        "非法字段的输入行保持常态底色"
    );
    let error_row: String = frame.cells
        [usize::from(port.y + 2) * width + usize::from(port.x)..usize::from(port.y + 3) * width]
        .iter()
        .map(|c| c.symbol.as_str())
        .collect();
    assert!(
        compact(&error_row).starts_with(&compact(texts.machine_form.err_port)),
        "错误行应当是端口校验文案：{error_row:?}"
    );
    assert_eq!(cell(port.x, port.y + 2).fg, red, "错误行应当标红");
}

/// L10：窄宽度下添加表单的页脚固定只有一行，放不下的项被直接丢掉而不是
/// 换到下一行——`esc 返回` 消失了。页脚应该跟列表页一样按宽度换行。
#[test]
fn narrow_add_form_footer_wraps_instead_of_dropping_the_back_hint() {
    let mut state = state_with_profiles(&[]);
    state.open_machine_add_form();
    let t = &crate::i18n::texts().machines;
    let footer = compact(&machines_footer_text(&mut state, 62, 32));
    assert!(
        footer.contains(&compact(t.hint_back)),
        "窄宽度下「esc 返回」不该被丢掉：{footer}"
    );
    assert!(footer.contains("esc"), "「esc」键帽也不该被丢掉：{footer}");
    // 页脚确实换成了不止一行（否则这条测试没测到东西）。
    assert!(
        state.hits.machines_footer.height > 1,
        "62 列下页脚应该换行，不能还是 1 行"
    );
}

#[test]
fn single_field_validators_cover_each_rule() {
    let taken = profile("Build", "build.example", "71");
    let saved = std::slice::from_ref(&taken);
    let mut form = ClientMachineForm::blank();
    let check = |form: &ClientMachineForm, field: MachineField| form.validate_field(field, saved);
    let t = &crate::i18n::texts().machine_form;
    assert_eq!(
        check(&form, MachineField::Target),
        Err(t.err_required.to_owned())
    );
    for bad in ["-oProxyCommand=x", "bad host", "user:pw@host"] {
        form.target = TextEditor::new(bad, false);
        assert_eq!(
            check(&form, MachineField::Target),
            Err(t.err_host.to_owned()),
            "{bad}"
        );
    }
    form.target = TextEditor::new("dev@build.example", false);
    assert_eq!(check(&form, MachineField::Target), Ok(()));
    // 名称查重不分大小写；留空时按目标算。
    form.label = TextEditor::new("build", false);
    assert_eq!(
        check(&form, MachineField::Label),
        Err(t.err_duplicate_name.to_owned())
    );
    form.label = TextEditor::new("Stage", false);
    assert_eq!(check(&form, MachineField::Label), Ok(()));
    for (field, bad) in [
        (MachineField::Port, "0"),
        (MachineField::Port, "x"),
        (MachineField::User, "a b"),
        (MachineField::Color, "not-a-color"),
        (MachineField::ControlPersist, "soon"),
        (MachineField::ServerAliveInterval, "-1"),
        (MachineField::ProxyJump, "profile:zz"),
        (MachineField::Session, "bad/session"),
    ] {
        let editor = match field {
            MachineField::Port => &mut form.port,
            MachineField::User => &mut form.user,
            MachineField::Color => &mut form.color,
            MachineField::ControlPersist => &mut form.control_persist,
            MachineField::ServerAliveInterval => &mut form.server_alive_interval,
            MachineField::ProxyJump => &mut form.proxy_jump,
            MachineField::Session => &mut form.session,
            _ => unreachable!(),
        };
        *editor = TextEditor::new(bad, false);
        assert!(check(&form, field).is_err(), "{field:?} 应拒绝 {bad:?}");
        let editor = match field {
            MachineField::Port => &mut form.port,
            MachineField::User => &mut form.user,
            MachineField::Color => &mut form.color,
            MachineField::ControlPersist => &mut form.control_persist,
            MachineField::ServerAliveInterval => &mut form.server_alive_interval,
            MachineField::ProxyJump => &mut form.proxy_jump,
            MachineField::Session => &mut form.session,
            _ => unreachable!(),
        };
        *editor = TextEditor::default();
        assert_eq!(check(&form, field), Ok(()), "{field:?} 留空合法");
    }
    form.port = TextEditor::new("65535", false);
    form.color = TextEditor::new("#00ff00", false);
    form.control_persist = TextEditor::new("10m", false);
    for field in [
        MachineField::Port,
        MachineField::Color,
        MachineField::ControlPersist,
    ] {
        assert_eq!(check(&form, field), Ok(()), "{field:?}");
    }
}

#[test]
fn mouse_click_focuses_a_field_and_maps_the_column_to_the_cursor() {
    let mut state = state_with_profiles(&[]);
    add_form_overlay(&mut state, "build.example", "");
    machine_form_mut(&mut state).focused = 0;
    frame_text(&mut state, 120, 40);
    let target = field_hit(&state, MachineField::Target);
    // 点输入行第 5 列：聚焦目标，光标落在第 5 个字符之前。
    let mut outcome = ClientShellInput::default();
    state.handle_mouse(left_click(target.x + 5, target.y + 1), &mut outcome);
    assert_eq!(focused(&state), Some(MachineField::Target));
    assert_eq!(machine_form(&state).target.cursor_char_index(), 5);
    let frame = state.compose(120, 40).expect("frame");
    let cursor = frame.cursor.expect("聚焦字段有光标");
    assert_eq!((cursor.x, cursor.y), (target.x + 5, target.y + 1));
    // 点在文本之后：落到末尾。
    state.handle_mouse(left_click(target.x + 30, target.y + 1), &mut outcome);
    assert_eq!(
        machine_form(&state).target.cursor_char_index(),
        "build.example".chars().count()
    );
    // 选择字段：第一次点只聚焦，再点才切换取值。
    frame_text(&mut state, 120, 40);
    let forward = field_hit(&state, MachineField::ForwardAgent);
    state.handle_mouse(left_click(forward.x + 1, forward.y + 1), &mut outcome);
    assert_eq!(focused(&state), Some(MachineField::ForwardAgent));
    assert!(matches!(
        machine_form(&state).forward_agent,
        super::super::machines_overlay::TriChoice::Default
    ));
    frame_text(&mut state, 120, 40);
    let forward = field_hit(&state, MachineField::ForwardAgent);
    state.handle_mouse(left_click(forward.x + 1, forward.y + 1), &mut outcome);
    assert!(matches!(
        machine_form(&state).forward_agent,
        super::super::machines_overlay::TriChoice::Yes
    ));
}

#[test]
fn wheel_scrolls_the_field_column_and_focus_changes_reveal_the_field() {
    let mut state = state_with_profiles(&[]);
    state.open_machine_add_form();
    frame_text(&mut state, 120, 34);
    assert_eq!(machine_form(&state).scroll, 0);
    let target = field_hit(&state, MachineField::Target);
    let mut outcome = ClientShellInput::default();
    state.handle_mouse(
        crossterm::event::MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: target.x + 2,
            row: target.y,
            modifiers: KeyModifiers::empty(),
        },
        &mut outcome,
    );
    frame_text(&mut state, 120, 34);
    assert!(machine_form(&state).scroll > 0, "滚轮滚动字段栏");
    assert_eq!(focused(&state), Some(MachineField::Quick), "滚轮不动焦点");
    // 键盘把焦点移到最后一项：它必须被滚进窗口并出现在命中表里。
    focus_field(&mut state, MachineField::SessionLogInterval);
    frame_text(&mut state, 120, 34);
    field_hit(&state, MachineField::SessionLogInterval);
    // 渲染是纯函数：连续两帧滚动不变。
    let scroll = machine_form(&state).scroll;
    frame_text(&mut state, 120, 34);
    assert_eq!(machine_form(&state).scroll, scroll);
}

#[test]
fn save_persists_a_new_machine_without_testing() {
    let dir = with_temp_state_home("form-save");
    let mut state = state_with_profiles(&[]);
    add_form_overlay(&mut state, "build.example", "");
    let outcome = press(&mut state, key(KeyCode::Enter));
    assert!(
        outcome.actions.is_empty(),
        "保存不跑 bootstrap：{:?}",
        outcome.actions
    );
    let catalog = crate::client::endpoint::EndpointCatalog::load().expect("catalog");
    assert_eq!(catalog.ssh.len(), 1);
    assert_eq!(catalog.ssh[0].label, "build.example", "标签留空取目标");
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
    let saved = crate::i18n::fill(
        crate::i18n::texts().machine_form.saved_fmt,
        &[("label", "build.example")],
    );
    let text = compact(&frame_text(&mut state, 106, 32));
    assert!(text.contains(&compact(&saved)), "保存反馈：{text}");
    assert_eq!(state.saved_profiles.len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_connection_runs_the_bootstrap_chain_without_saving() {
    let dir = with_temp_state_home("form-test-ok");
    let mut state = state_with_profiles(&[]);
    add_form_overlay(&mut state, "build.example", "Build");
    // 第一次只弹确认条，Enter 确认后才下发。
    let outcome = press(&mut state, ctrl('t'));
    assert!(outcome.actions.is_empty(), "{:?}", outcome.actions);
    let outcome = press(&mut state, key(KeyCode::Enter));
    let [ClientShellAction::BootstrapMachine {
        ticket,
        target,
        session,
        ..
    }] = &outcome.actions[..]
    else {
        panic!("bootstrap action: {:?}", outcome.actions);
    };
    assert_eq!(
        (target.as_str(), session.as_str()),
        ("build.example", "default")
    );
    let ticket = *ticket;
    // 运行中：表单只读，打字无效。
    type_text(&mut state, "x");
    assert_eq!(machine_form(&state).target.as_str(), "build.example");
    state.handle_machine_bootstrap_update(
        ticket,
        MachineBootstrapUpdate::Step(crate::remote::SavedSshBootstrapStep::StartServer),
    );
    let text = compact(&frame_text(&mut state, 120, 40));
    let running = compact(crate::i18n::texts().machine_form.test_running);
    assert!(text.contains(&running), "运行中：{text}");
    // 过期票据不影响。
    state.handle_machine_bootstrap_update(ticket + 9, MachineBootstrapUpdate::Finished(Ok(())));
    assert!(
        !machine_form(&state)
            .bootstrap
            .as_ref()
            .expect("test")
            .passed
    );

    state.handle_machine_bootstrap_update(ticket, MachineBootstrapUpdate::Finished(Ok(())));
    let form = machine_form(&state);
    assert!(form.bootstrap.as_ref().is_some_and(|test| test.passed));
    assert!(
        crate::client::endpoint::EndpointCatalog::load()
            .expect("catalog")
            .ssh
            .is_empty(),
        "测试不落盘"
    );
    let text = compact(&frame_text(&mut state, 120, 40));
    let passed = compact(crate::i18n::texts().machine_form.test_passed);
    assert!(text.contains(&passed), "测试通过：{text}");
    assert!(text.matches('✓').count() >= 4, "四步都打勾：{text}");
    // 改连接字段让结论作废；改标签不影响。
    focus_field(&mut state, MachineField::Label);
    type_text(&mut state, "!");
    assert!(machine_form(&state).bootstrap.is_some(), "标签不影响连接");
    focus_field(&mut state, MachineField::Port);
    type_text(&mut state, "2");
    assert!(
        machine_form(&state).bootstrap.is_none(),
        "端口改了，结论作废"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn esc_cancels_a_running_test_and_keeps_the_form() {
    let _dir = with_temp_state_home("form-test-cancel");
    let mut state = state_with_profiles(&[]);
    add_form_overlay(&mut state, "build.example", "Build");
    start_test(&mut state);
    let cancel = machine_form(&state)
        .bootstrap
        .as_ref()
        .expect("running")
        .cancel
        .clone();
    press(&mut state, key(KeyCode::Esc));
    assert!(cancel.is_cancelled(), "Esc 取消后台测试");
    assert!(
        machine_form(&state).bootstrap.is_none(),
        "表单还在、回到可编辑"
    );
    press(&mut state, key(KeyCode::Esc));
    assert!(matches!(
        state.overlay,
        Some(ClientShellOverlay::Machines(
            super::super::machines_overlay::ClientMachinesOverlay {
                view: super::super::machines_overlay::ClientMachinesView::List,
                ..
            }
        ))
    ));
}

#[test]
fn failed_test_with_unknown_host_key_opens_the_host_key_review() {
    let _dir = with_temp_state_home("form-test-hostkey");
    let mut state = state_with_profiles(&[]);
    add_form_overlay(&mut state, "build.example", "Build");
    let ticket = start_test(&mut state);
    state.handle_machine_bootstrap_update(
        ticket,
        MachineBootstrapUpdate::Finished(Err("Host key verification failed.".into())),
    );
    let text = compact(&frame_text(&mut state, 120, 40));
    let f = &crate::i18n::texts().machine_form;
    assert!(
        text.contains(&compact(f.review_host_key)),
        "恢复入口：{text}"
    );
    assert!(text.contains('✗'), "失败步骤打叉：{text}");
    let outcome = press(&mut state, ctrl('r'));
    let Some(ClientShellOverlay::MachineAuth(auth)) = state.overlay.as_ref() else {
        panic!("host key 未知应进入 MachineAuth");
    };
    assert!(matches!(
        auth.view,
        Some(super::super::machine_auth_overlay::ClientMachineAuthView::HostKeyUnknown(_))
    ));
    assert!(outcome.actions.iter().any(|action| matches!(
        action,
        ClientShellAction::MachineHostKeyOp {
            op: MachineHostKeyOp::Scan,
            profile,
            ..
        } if profile.target == "build.example"
    )));
    // 放弃后回到表单，失败结论与字段都还在。
    state.activate_machine_auth_button(
        super::super::machine_auth_overlay::MachineAuthButton::Abort,
        &mut ClientShellInput::default(),
    );
    let form = machine_form(&state);
    assert_eq!(form.target.as_str(), "build.example");
    assert!(form
        .bootstrap
        .as_ref()
        .is_some_and(|test| test.failure.is_some()));
}

#[test]
fn failed_test_with_a_changed_host_key_opens_the_blocking_review() {
    let _dir = with_temp_state_home("form-test-hostkey-changed");
    let mut state = state_with_profiles(&[]);
    add_form_overlay(&mut state, "build.example", "Build");
    let ticket = start_test(&mut state);
    state.handle_machine_bootstrap_update(
        ticket,
        MachineBootstrapUpdate::Finished(Err(
            "WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!".into()
        )),
    );
    let text = compact(&frame_text(&mut state, 120, 40));
    let f = &crate::i18n::texts().machine_form;
    assert!(
        text.contains(&compact(f.review_host_key)),
        "恢复入口：{text}"
    );
    let outcome = press(&mut state, ctrl('r'));
    let Some(ClientShellOverlay::MachineAuth(auth)) = state.overlay.as_ref() else {
        panic!("host key 变化应进入 MachineAuth");
    };
    let Some(super::super::machine_auth_overlay::ClientMachineAuthView::HostKeyChanged(view)) =
        auth.view.as_ref()
    else {
        panic!("硬阻断的变更对话框");
    };
    assert!(view.profile_id.is_none(), "临时档案未落盘");
    assert_eq!(view.profile.target, "build.example");
    assert!(outcome.actions.is_empty(), "打开对话框本身不动 known_hosts");
    assert!(
        crate::client::endpoint::EndpointCatalog::load()
            .expect("catalog")
            .ssh
            .is_empty(),
        "测试失败与恢复入口都不落盘"
    );
}

#[test]
fn failed_test_with_auth_errors_opens_the_interactive_auth_guide() {
    let _dir = with_temp_state_home("form-test-auth");
    let mut state = state_with_profiles(&[]);
    add_form_overlay(&mut state, "build.example", "Build");
    machine_form_mut(&mut state).user = TextEditor::new("dev", false);
    let ticket = start_test(&mut state);
    state.handle_machine_bootstrap_update(
        ticket,
        MachineBootstrapUpdate::Step(crate::remote::SavedSshBootstrapStep::DetectPlatform),
    );
    state.handle_machine_bootstrap_update(
        ticket,
        MachineBootstrapUpdate::Finished(Err("Permission denied (publickey)".into())),
    );
    let text = compact(&frame_text(&mut state, 120, 40));
    assert!(text.contains("Permissiondenied"), "失败原因：{text}");
    let failed_at = crate::i18n::fill(
        crate::i18n::texts().machine_form.test_failed_fmt,
        &[("step", crate::i18n::texts().machines.progress_detect)],
    );
    assert!(text.contains(&compact(&failed_at)), "失败步骤：{text}");
    // 点页脚的恢复入口（按钮与 Ctrl+R 同义）。
    let recover = state
        .hits
        .machines_actions
        .iter()
        .find(|(_, button)| *button == MachineOverlayButton::TestRecover)
        .map(|(rect, _)| *rect)
        .expect("恢复入口可点");
    let mut outcome = ClientShellInput::default();
    state.handle_mouse(left_click(recover.x + 1, recover.y), &mut outcome);
    let Some(ClientShellOverlay::MachineAuth(auth)) = state.overlay.as_ref() else {
        panic!("认证失败应进入 MachineAuth");
    };
    let Some(super::super::machine_auth_overlay::ClientMachineAuthView::AuthGuide(guide)) =
        auth.view.as_ref()
    else {
        panic!("交互认证引导");
    };
    assert!(guide.wizard, "临时档案走向导路径");
    assert_eq!(guide.profile.target, "build.example");
    assert_eq!(guide.profile.user.as_deref(), Some("dev"));
}

#[test]
fn failed_test_without_an_interactive_fix_shows_the_command() {
    let _dir = with_temp_state_home("form-test-dns");
    let mut state = state_with_profiles(&[]);
    add_form_overlay(&mut state, "build.example", "Build");
    let ticket = start_test(&mut state);
    state.handle_machine_bootstrap_update(
        ticket,
        MachineBootstrapUpdate::Finished(Err(
            "ssh: Could not resolve hostname build.example".into()
        )),
    );
    let text = compact(&frame_text(&mut state, 120, 40));
    assert!(text.contains("herdr--remote"), "修复命令：{text}");
    assert!(
        !state
            .hits
            .machines_actions
            .iter()
            .any(|(_, button)| *button == MachineOverlayButton::TestRecover),
        "DNS 失败没有交互恢复入口"
    );
    let outcome = press(&mut state, ctrl('r'));
    assert!(outcome.actions.is_empty());
    assert!(matches!(
        state.overlay,
        Some(ClientShellOverlay::Machines(_))
    ));
}

#[test]
fn narrow_form_drops_the_preview_and_shows_test_steps_under_the_fields() {
    let _dir = with_temp_state_home("form-narrow");
    let mut state = state_with_profiles(&[]);
    add_form_overlay(&mut state, "build.example", "Build");
    let text = compact(&frame_text(&mut state, 70, 32));
    let f = &crate::i18n::texts().machine_form;
    assert!(
        !text.contains(&compact(f.preview_title)),
        "窄屏无预览栏：{text}"
    );
    let ticket = start_test(&mut state);
    state.handle_machine_bootstrap_update(
        ticket,
        MachineBootstrapUpdate::Step(crate::remote::SavedSshBootstrapStep::Install),
    );
    let text = compact(&frame_text(&mut state, 70, 32));
    let install = compact(crate::i18n::texts().machines.progress_install);
    assert!(text.contains(&install), "测试步骤在字段栏下方：{text}");
}

/// 表单页脚先动作后导航：保存、测试连接、返回在前，tab / ←→ 在最后。放不下时
/// kit 从尾部丢非 primary 项，纯导航键最先让位，「esc 返回」一直留到导航键
/// 丢完之后。
#[test]
fn form_footer_puts_actions_first_and_drops_navigation_keys_first() {
    let _dir = with_temp_state_home("form-footer-order");
    let _lang = crate::i18n::lang_guard(crate::i18n::Lang::En);
    let mut state = state_with_profiles(&[]);
    add_form_overlay(&mut state, "build.example", "Build");
    let wide = machines_footer_text(&mut state, 140, 40);
    let at = |needle: &str| {
        wide.find(needle)
            .unwrap_or_else(|| panic!("页脚缺 {needle:?}：{wide}"))
    };
    assert!(
        at(" enter ") < at(" ctrl+t ") && at(" ctrl+t ") < at(" esc ") && at(" esc ") < at(" tab "),
        "先动作后导航：{wide}"
    );
    // 聚焦选择类字段时多一项 ←→，同样排在最后。
    focus_field(&mut state, MachineField::StrictHostKey);
    let choice = machines_footer_text(&mut state, 140, 40);
    let tab = choice.find(" tab ").expect("tab 提示");
    let arrows = choice.find(" ←→ ").expect("←→ 提示");
    let esc = choice.find(" esc ").expect("esc 提示");
    assert!(esc < tab && tab < arrows, "{choice}");

    // L10：页脚按宽度换行而不是把放不下的项丢掉，所以窄宽度下「esc 返回」
    // 与导航键应该都还在（换到下一行），不再是「先丢导航键」。
    let mut wrapped_to_multiple_rows = false;
    for cols in (30..=140).rev() {
        if state.compose(cols, 40).is_none() {
            continue;
        }
        let footer = machines_footer_text(&mut state, cols, 40);
        let has_navigation = footer.contains(" tab ") || footer.contains(" ←→ ");
        let has_back = machine_buttons(&state).contains(&MachineOverlayButton::Back);
        assert!(
            !has_navigation || has_back,
            "{cols} 列：导航键不能比「esc 返回」留得久：{footer}"
        );
        assert!(
            has_navigation,
            "{cols} 列：导航键不该被丢掉，应该换到下一行：{footer}"
        );
        wrapped_to_multiple_rows |= state.hits.machines_footer.height > 1;
    }
    assert!(wrapped_to_multiple_rows, "扫描应覆盖触发换行的宽度");
}

/// 在临时状态目录里落一条档案并读回（带目录分配的真实 id）。
fn seed_catalog_machine(label: &str, target: &str) -> SavedSshEndpoint {
    let mut catalog = crate::client::endpoint::EndpointCatalog::default();
    catalog
        .add_ssh_with_options(
            label,
            target,
            "default",
            crate::client::endpoint::SshProfileOptions::default(),
        )
        .expect("seed profile");
    catalog.store_profiles().expect("seed store");
    crate::client::endpoint::EndpointCatalog::load()
        .expect("catalog")
        .ssh[0]
        .clone()
}

/// 编辑已保存的机器不提供测试连接与恢复入口：恢复路径的临时档案带新 id，
/// 交互认证成功会按新机器落盘，编辑态走这条路会复制出第二条同目标档案、
/// 原档案却收不到编辑。键盘、页脚按钮与残留的失败结论都进不了这条路。
#[test]
fn edit_form_offers_no_test_connection_or_recovery() {
    let dir = with_temp_state_home("edit-no-test");
    let saved = seed_catalog_machine("Build", "build.example");
    let mut state = state_with_profiles(std::slice::from_ref(&saved));
    state.open_machine_edit_form(&saved.id);
    focus_field(&mut state, MachineField::IdentityFiles);
    type_text(&mut state, "~/.ssh/other");

    let text = compact(&frame_text(&mut state, 120, 40));
    let f = &crate::i18n::texts().machine_form;
    assert!(!text.contains("ctrl+t"), "编辑态页脚无测试：{text}");
    assert!(
        !text.contains(&compact(crate::i18n::texts().machines.confirm_install_note)),
        "编辑态预览不写测试会做什么：{text}"
    );
    let buttons = machine_buttons(&state);
    assert!(
        !buttons.contains(&MachineOverlayButton::TestConnection),
        "{buttons:?}"
    );

    // Ctrl+T 与测试按钮都不下发 bootstrap。
    let outcome = press(&mut state, ctrl('t'));
    assert!(outcome.actions.is_empty(), "{:?}", outcome.actions);
    let mut outcome = ClientShellInput::default();
    state.activate_machine_button(MachineOverlayButton::TestConnection, &mut outcome);
    assert!(outcome.actions.is_empty(), "{:?}", outcome.actions);
    assert!(machine_form(&state).bootstrap.is_none());

    // 即便表单里残留一次认证失败的结论，Ctrl+R 与恢复按钮也不进 MachineAuth。
    machine_form_mut(&mut state).bootstrap =
        Some(super::super::machines_overlay::ClientMachineBootstrap {
            cancel: crate::remote::TaskCancellation::default(),
            ticket: 1,
            step: None,
            failure: Some("Permission denied (publickey)".into()),
            passed: false,
        });
    press(&mut state, ctrl('r'));
    state.activate_machine_button(
        MachineOverlayButton::TestRecover,
        &mut ClientShellInput::default(),
    );
    assert!(
        matches!(state.overlay, Some(ClientShellOverlay::Machines(_))),
        "编辑态没有恢复入口"
    );
    let text = compact(&frame_text(&mut state, 120, 40));
    assert!(!text.contains("ctrl+r"), "{text}");
    assert!(!text.contains(&compact(f.review_host_key)), "{text}");

    // Enter 保存的是原档案：目录里仍只有一条，编辑生效。
    press(&mut state, key(KeyCode::Enter));
    let catalog = crate::client::endpoint::EndpointCatalog::load().expect("catalog");
    assert_eq!(catalog.ssh.len(), 1, "不会复制出第二条档案");
    assert_eq!(catalog.ssh[0].id, saved.id);
    assert_eq!(
        catalog.ssh[0].identity_file,
        vec!["~/.ssh/other".to_owned()]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// 从带 `⚠` 的那一行起，取弹窗内框里连续几行的文字（到右边框为止）拼起来
/// 去空白：确认条会折行，整帧 contains 对不上折行处。
fn prompt_block_text(text: &str) -> String {
    let rows: Vec<Vec<char>> = text.lines().map(|line| line.chars().collect()).collect();
    let Some((start, column)) = rows
        .iter()
        .enumerate()
        .find_map(|(y, row)| row.iter().position(|ch| *ch == '⚠').map(|x| (y, x)))
    else {
        return String::new();
    };
    let mut block = String::new();
    for row in rows.iter().skip(start).take(4) {
        block.extend(
            row.iter()
                .skip(column)
                .take_while(|ch| **ch != '│')
                .filter(|ch| !ch.is_whitespace()),
        );
    }
    block
}

/// 测试连接以预先授权模式跑 bootstrap（必要时安装 / 更新并停止远端 server
/// 及其 pane 进程），所以第一次 Ctrl+T 先在页脚上方写明后果——窄屏没有预览
/// 栏，这条说明同样要在；Esc 收起确认条不下发，确认（Ctrl+T / Enter / 点
/// 「开始测试」）才下发。
#[test]
fn test_connection_asks_for_confirmation_on_every_width() {
    let _dir = with_temp_state_home("form-test-confirm");
    let f = &crate::i18n::texts().machine_form;
    let note = compact(f.test_confirm_note);
    for (cols, rows) in [(70u16, 32u16), (120, 40)] {
        let mut state = state_with_profiles(&[]);
        add_form_overlay(&mut state, "build.example", "Build");
        let text = frame_text(&mut state, cols, rows);
        assert!(
            prompt_block_text(&text).is_empty(),
            "{cols} 列：未请求前无确认条"
        );

        let outcome = press(&mut state, ctrl('t'));
        assert!(
            outcome.actions.is_empty(),
            "{cols} 列：{:?}",
            outcome.actions
        );
        assert!(machine_form(&state).bootstrap.is_none());
        let text = frame_text(&mut state, cols, rows);
        assert!(
            prompt_block_text(&text).contains(&note),
            "{cols} 列：测试前写明后果：{text}"
        );
        let buttons = machine_buttons(&state);
        assert_eq!(
            buttons,
            vec![
                MachineOverlayButton::TestConnection,
                MachineOverlayButton::Back
            ],
            "{cols} 列：确认条上只有开始 / 取消"
        );
        assert!(
            compact(&text).contains(&compact(f.test_confirm_start)),
            "{cols} 列：{text}"
        );
        // 没有输入光标（合成层可能回落到弹窗外的 pane 光标）。
        let frame = state.compose(cols, rows).expect("frame");
        let popup = state.hits.machines_popup;
        assert!(
            frame.cursor.as_ref().is_none_or(|cursor| {
                cursor.x < popup.x
                    || cursor.x >= popup.right()
                    || cursor.y < popup.y
                    || cursor.y >= popup.bottom()
            }),
            "{cols} 列：确认条期间弹窗内无输入光标"
        );

        // 确认条期间表单只读：打字、粘贴都不动字段。
        type_text(&mut state, "x");
        assert!(!state.insert_machines_overlay_text("y"));
        assert_eq!(machine_form(&state).label.as_str(), "Build");

        // Esc 只收起确认条：不下发、表单还在。
        let outcome = press(&mut state, key(KeyCode::Esc));
        assert!(outcome.actions.is_empty());
        assert!(machine_form(&state).prompt.is_none());
        let text = frame_text(&mut state, cols, rows);
        assert!(prompt_block_text(&text).is_empty(), "{cols} 列：{text}");

        // 再请求一次，点页脚的「开始测试」下发 bootstrap。
        press(&mut state, ctrl('t'));
        frame_text(&mut state, cols, rows);
        let start = state
            .hits
            .machines_actions
            .iter()
            .find(|(_, button)| *button == MachineOverlayButton::TestConnection)
            .map(|(rect, _)| *rect)
            .expect("开始测试可点");
        let mut outcome = ClientShellInput::default();
        state.handle_mouse(left_click(start.x, start.y), &mut outcome);
        assert!(
            matches!(
                &outcome.actions[..],
                [ClientShellAction::BootstrapMachine { .. }]
            ),
            "{cols} 列：{:?}",
            outcome.actions
        );
        assert!(machine_form(&state)
            .bootstrap
            .as_ref()
            .is_some_and(|test| test.failure.is_none() && !test.passed));
        assert!(machine_form(&state).prompt.is_none());
    }
}

fn machines_view_is_list(state: &ClientShellState) -> bool {
    matches!(
        state.overlay,
        Some(ClientShellOverlay::Machines(
            super::super::machines_overlay::ClientMachinesOverlay {
                view: super::super::machines_overlay::ClientMachinesView::List,
                ..
            }
        ))
    )
}

/// 测试结束（通过或失败）后第一次 Esc 只清掉结论，表单与字段都在（旧向导
/// FormEdit 的语义）；没有改动的表单再按 Esc 才离开。
#[test]
fn esc_after_a_finished_test_only_clears_the_result() {
    let _dir = with_temp_state_home("form-test-esc");
    for result in [Ok(()), Err("Permission denied (publickey)".to_owned())] {
        let mut state = state_with_profiles(&[]);
        add_form_overlay(&mut state, "build.example", "Build");
        let ticket = start_test(&mut state);
        state.handle_machine_bootstrap_update(
            ticket,
            MachineBootstrapUpdate::Finished(result.clone()),
        );
        press(&mut state, key(KeyCode::Esc));
        let form = machine_form(&state);
        assert!(form.bootstrap.is_none(), "{result:?}：第一次 Esc 清掉结论");
        assert_eq!(form.target.as_str(), "build.example");
        assert_eq!(form.label.as_str(), "Build");
        press(&mut state, key(KeyCode::Esc));
        assert!(machines_view_is_list(&state), "{result:?}：再按才离开");
    }
}

/// 有未保存的改动时 Esc 不直接丢表单：页脚上方先问「放弃更改？」，Esc 继续
/// 编辑、Enter 或点「放弃更改」才离开；有改动时点窗外也不关页面。编辑表单
/// 放弃后回详情，目录不变。
#[test]
fn esc_with_unsaved_changes_asks_before_discarding_the_form() {
    let dir = with_temp_state_home("form-discard");
    let f = &crate::i18n::texts().machine_form;
    let mut state = state_with_profiles(&[]);
    state.open_machine_add_form();
    type_text(&mut state, "dev@build.example");

    press(&mut state, key(KeyCode::Esc));
    assert!(machine_form(&state).prompt.is_some(), "先问，不离开");
    let text = frame_text(&mut state, 120, 40);
    assert!(
        prompt_block_text(&text).contains(&compact(f.discard_prompt)),
        "放弃确认条：{text}"
    );
    assert_eq!(
        machine_buttons(&state),
        vec![
            MachineOverlayButton::DiscardForm,
            MachineOverlayButton::Back
        ]
    );
    // Esc = 继续编辑：确认条收起，输入还在。
    press(&mut state, key(KeyCode::Esc));
    let form = machine_form(&state);
    assert!(form.prompt.is_none());
    assert_eq!(form.quick.as_str(), "dev@build.example");

    // 有改动时点窗外不关页面。
    frame_text(&mut state, 120, 40);
    let popup = state.hits.machines_popup;
    assert!(popup.x > 0, "{popup:?}");
    state.handle_mouse(
        left_click(popup.x - 1, popup.y),
        &mut ClientShellInput::default(),
    );
    assert!(
        matches!(state.overlay, Some(ClientShellOverlay::Machines(_))),
        "有改动时点窗外不关"
    );
    machine_form(&state);

    // Esc 再问，Enter 放弃：回列表，什么都没落盘。
    press(&mut state, key(KeyCode::Esc));
    press(&mut state, key(KeyCode::Enter));
    assert!(machines_view_is_list(&state));
    assert!(crate::client::endpoint::EndpointCatalog::load()
        .expect("catalog")
        .ssh
        .is_empty());

    // 编辑表单：改一个字段，Esc 问，点「放弃更改」回详情，档案原样。
    let saved = seed_catalog_machine("Build", "build.example");
    let mut state = state_with_profiles(std::slice::from_ref(&saved));
    state.open_machine_edit_form(&saved.id);
    focus_field(&mut state, MachineField::Port);
    type_text(&mut state, "2222");
    press(&mut state, key(KeyCode::Esc));
    frame_text(&mut state, 120, 40);
    click_machine_button(&mut state, MachineOverlayButton::DiscardForm);
    assert!(matches!(
        state.overlay,
        Some(ClientShellOverlay::Machines(
            super::super::machines_overlay::ClientMachinesOverlay {
                view: super::super::machines_overlay::ClientMachinesView::Detail(_),
                ..
            }
        ))
    ));
    let catalog = crate::client::endpoint::EndpointCatalog::load().expect("catalog");
    assert_eq!(catalog.ssh.len(), 1);
    assert_eq!(catalog.ssh[0].port, None, "放弃的改动不落盘");

    // 只切焦点、没改内容：Esc 直接离开。
    state.open_machine_edit_form(&saved.id);
    press(&mut state, key(KeyCode::Tab));
    press(&mut state, key(KeyCode::Esc));
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

/// toast 盖在列表页脚的最后一行上时，那一行的页脚项既看不见也不可点：
/// 点 toast 文字不会触发底下被盖住的按钮；toast 到期后页脚恢复可点。
#[test]
fn machine_toast_row_swallows_no_hidden_footer_clicks() {
    let saved = profile("Build", "dev@build.example", "90");
    for (cols, rows) in [(80u16, 30u16), (130, 40)] {
        let mut state = state_with_profiles(std::slice::from_ref(&saved));
        state.config.feedback.animations = false;
        state.open_machines_overlay();
        frame_text(&mut state, cols, rows);
        let footer = state.hits.machines_footer;
        let last_row = footer.bottom() - 1;
        let covered: Vec<(Rect, MachineOverlayButton)> = state
            .hits
            .machines_actions
            .iter()
            .filter(|(rect, _)| rect.y == last_row)
            .copied()
            .collect();
        assert!(!covered.is_empty(), "{cols} 列：页脚最后一行应有按钮");

        state.route_machines_key(&key(KeyCode::Char('c')), &mut ClientShellInput::default());
        let text = frame_text(&mut state, cols, rows);
        assert!(
            compact(&text).contains(&compact(crate::i18n::texts().machines.copied_fix_command)),
            "{cols} 列：{text}"
        );
        assert!(
            state
                .hits
                .machines_actions
                .iter()
                .all(|(rect, _)| rect.y != last_row),
            "{cols} 列：toast 行不留命中区：{:?}",
            state.hits.machines_actions
        );
        for (rect, button) in &covered {
            let mut outcome = ClientShellInput::default();
            state.handle_mouse(left_click(rect.x, rect.y), &mut outcome);
            assert!(
                outcome.actions.is_empty(),
                "{cols} 列 {button:?}：{:?}",
                outcome.actions
            );
            assert!(
                machines_view_is_list(&state),
                "{cols} 列：点 toast 不触发被盖住的 {button:?}"
            );
        }

        // 到期后页脚最后一行恢复可点。
        let at = machine_toast_at(&state).expect("toast");
        state.tick_chrome_feedback(at + super::super::machines_overlay::MACHINE_TOAST_DURATION);
        frame_text(&mut state, cols, rows);
        let restored = state
            .hits
            .machines_actions
            .iter()
            .filter(|(rect, _)| rect.y == last_row)
            .count();
        assert_eq!(restored, covered.len(), "{cols} 列");
    }
}

/// 保存新机器的 toast 落在列表；4 秒内再打开添加表单，表单唯一的一行页脚
/// 不被 toast 盖住（toast 只在列表 / 详情显示），回到列表又能看到它。表单
/// 里触发的复制修复命令改走通用通知。
#[test]
fn machine_toast_stays_off_the_form_footer() {
    let dir = with_temp_state_home("toast-form");
    let mut state = state_with_profiles(&[]);
    add_form_overlay(&mut state, "build.example", "");
    press(&mut state, key(KeyCode::Enter));
    let saved_text = compact(&crate::i18n::fill(
        crate::i18n::texts().machine_form.saved_fmt,
        &[("label", "build.example")],
    ));
    assert!(compact(&frame_text(&mut state, 106, 32)).contains(&saved_text));

    press(&mut state, key(KeyCode::Char('a')));
    let text = compact(&frame_text(&mut state, 106, 32));
    assert!(
        !text.contains(&saved_text),
        "表单页脚不被 toast 盖住：{text}"
    );
    let buttons = machine_buttons(&state);
    for expected in [
        MachineOverlayButton::Save,
        MachineOverlayButton::TestConnection,
        MachineOverlayButton::Back,
    ] {
        assert!(buttons.contains(&expected), "{buttons:?}");
    }

    let profile_id = state.saved_profiles[0].id.clone();
    let mut outcome = ClientShellInput::default();
    state.machine_copy_fix_command(&profile_id, &mut outcome);
    assert_eq!(
        state
            .visible_endpoint_notice
            .as_ref()
            .map(|notice| notice.title.as_str()),
        Some(crate::i18n::texts().machines.copied_fix_command),
        "表单里没有 toast 落点，走通用通知"
    );

    press(&mut state, key(KeyCode::Esc));
    assert!(machines_view_is_list(&state));
    assert!(compact(&frame_text(&mut state, 106, 32)).contains(&saved_text));
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------
// 合并页脚、空状态与带预览的导入清单
// ---------------------------------------------------------------------

/// 可点的页脚项（按钮）集合。
fn machine_buttons(state: &ClientShellState) -> Vec<MachineOverlayButton> {
    state
        .hits
        .machines_actions
        .iter()
        .map(|(_, button)| *button)
        .collect()
}

fn click_machine_button(state: &mut ClientShellState, button: MachineOverlayButton) {
    let rect = state
        .hits
        .machines_actions
        .iter()
        .find(|(_, candidate)| *candidate == button)
        .map(|(rect, _)| *rect)
        .unwrap_or_else(|| panic!("{button:?} 不可点：{:?}", state.hits.machines_actions));
    state.handle_mouse(left_click(rect.x, rect.y), &mut ClientShellInput::default());
}

/// 列表 / 工作台 / 详情的动作都在同一条页脚里：命中矩形全部落在页脚行内，
/// 不再有单独的按钮行；点页脚项等同于按键。
#[test]
fn machine_pages_have_one_clickable_footer_instead_of_a_button_row() {
    let _dir = with_temp_state_home("merged-footer");
    let saved = profile("Build", "dev@build.example", "80");
    for (cols, rows) in [(80u16, 30u16), (130, 40)] {
        let mut state = state_with_profiles(std::slice::from_ref(&saved));
        state.open_machines_overlay();
        frame_text(&mut state, cols, rows);
        let footer = state.hits.machines_footer;
        assert!(!footer.is_empty(), "{cols} 列应有页脚");
        for (rect, button) in &state.hits.machines_actions {
            assert!(
                rect.y >= footer.y && rect.bottom() <= footer.bottom(),
                "{cols} 列 {button:?} 应在页脚行内：{rect:?} / {footer:?}"
            );
        }
        let buttons = machine_buttons(&state);
        for expected in [
            MachineOverlayButton::Add,
            MachineOverlayButton::Import,
            MachineOverlayButton::Close,
            MachineOverlayButton::Reconnect,
            MachineOverlayButton::Edit,
        ] {
            assert!(buttons.contains(&expected), "{cols} 列：{buttons:?}");
        }
        // 点页脚的「编辑」进入选中机器的编辑表单。
        click_machine_button(&mut state, MachineOverlayButton::Edit);
        let form = machine_form(&state);
        assert_eq!(form.editing.as_ref(), Some(&saved.id));
    }

    // 详情页同理：返回 / 单机动作都在页脚里。
    let mut state = state_with_profiles(std::slice::from_ref(&saved));
    state.open_machines_overlay_for(&saved.id);
    frame_text(&mut state, 106, 36);
    let footer = state.hits.machines_footer;
    let buttons = machine_buttons(&state);
    assert!(buttons.contains(&MachineOverlayButton::Back), "{buttons:?}");
    assert!(
        buttons.contains(&MachineOverlayButton::Forwards),
        "{buttons:?}"
    );
    assert!(state
        .hits
        .machines_actions
        .iter()
        .all(|(rect, _)| rect.y >= footer.y && rect.bottom() <= footer.bottom()));
    click_machine_button(&mut state, MachineOverlayButton::Back);
    assert!(matches!(
        state.overlay,
        Some(ClientShellOverlay::Machines(
            super::super::machines_overlay::ClientMachinesOverlay {
                view: super::super::machines_overlay::ClientMachinesView::List,
                ..
            }
        ))
    ));
}

/// 停用的机器：页脚仍显示 `r 重连`（与按键表一致），但置灰且不可点。
#[test]
fn reconnect_hint_is_disabled_for_a_disabled_machine() {
    let mut saved = profile("Build", "dev@build.example", "81");
    saved.enabled = false;
    let mut state = state_with_profiles(std::slice::from_ref(&saved));
    state.open_machines_overlay_for(&saved.id);
    let footer = compact(&machines_footer_text(&mut state, 106, 36));
    let reconnect = compact(crate::i18n::texts().machines.hint_reconnect);
    assert!(footer.contains(&reconnect), "{footer}");
    assert!(
        !machine_buttons(&state).contains(&MachineOverlayButton::Reconnect),
        "停用机器的重连不可点"
    );
}

/// 没有机器时列表画 kit 空状态，主按钮「添加」可点并打开表单；有机器但过滤
/// 为空时只说没有匹配，不给添加按钮。
#[test]
fn empty_machine_list_uses_the_empty_state_with_an_add_action() {
    let t = &crate::i18n::texts().machines;
    for (cols, rows) in [(80u16, 30u16), (130, 40)] {
        let mut state = state_with_profiles(&[]);
        state.open_machines_overlay();
        let text = compact(&frame_text(&mut state, cols, rows));
        assert!(text.contains(&compact(t.empty)), "{cols} 列：{text}");
        assert!(text.contains(&compact(t.empty_hint)), "{cols} 列：{text}");
        let footer = state.hits.machines_footer;
        let add = state
            .hits
            .machines_actions
            .iter()
            .find(|(rect, button)| {
                *button == MachineOverlayButton::Add && rect.bottom() <= footer.y
            })
            .map(|(rect, _)| *rect)
            .expect("空状态的添加按钮在正文里");
        state.handle_mouse(left_click(add.x, add.y), &mut ClientShellInput::default());
        assert!(machine_form(&state).editing.is_none(), "打开添加表单");
    }

    let saved = profile("Build", "dev@build.example", "82");
    let mut state = state_with_profiles(std::slice::from_ref(&saved));
    state.open_machines_overlay();
    type_text(&mut state, "/zzz");
    let text = compact(&frame_text(&mut state, 80, 30));
    assert!(text.contains(&compact(t.no_matches)), "{text}");
    assert!(!text.contains(&compact(t.empty)), "{text}");
}

/// L9：空列表没有可选中 / 查看详情的行，页脚「↑↓ 选择」「enter 详情」应该
/// 置灰，不能和「esc 关闭」「a 添加」这些真的可点的项同一个亮度。
#[test]
fn empty_machine_list_footer_greys_out_selection_and_details_hints() {
    let t = &crate::i18n::texts().machines;
    let mut state = state_with_profiles(&[]);
    state.open_machines_overlay();
    let frame = state.compose(80, 30).expect("空列表机器页");
    let footer = state.hits.machines_footer;
    let width = usize::from(frame.width);
    let footer_row: Vec<&crate::protocol::CellData> = (footer.y..footer.bottom())
        .flat_map(|y| {
            let row = &frame.cells[usize::from(y) * width..usize::from(y) * width + width];
            row[usize::from(footer.x)..].iter()
        })
        .collect();
    let footer_text: String = footer_row.iter().map(|c| c.symbol.as_str()).collect();
    assert!(
        compact(&footer_text).contains(&compact(t.hint_select)),
        "页脚应该有「↑↓ 选择」：{footer_text}"
    );

    let disabled_bg = crate::protocol::color_to_u32(state.config.palette.surface_dim);
    let enabled_bg = crate::protocol::color_to_u32(state.config.palette.surface0);
    // 每个 cell 在拼接串里贡献恰好一个字符（宽字符续格是单个空格），字节
    // 偏移量按字符数换算成 cell 下标即可，不能直接当字节下标用。
    let cell_index_of = |byte_offset: usize| footer_text[..byte_offset].chars().count();
    let select_key_at = cell_index_of(footer_text.find('↑').expect("↑↓ 键帽位置"));
    let close_key_at = cell_index_of(footer_text.find("esc").expect("esc 关闭键帽位置"));
    assert_eq!(
        footer_row[select_key_at].bg, disabled_bg,
        "「↑↓」键帽空列表下应该置灰：{footer_text}"
    );
    assert_eq!(
        footer_row[close_key_at].bg, enabled_bg,
        "「esc」仍然可点，不该被一起置灰：{footer_text}"
    );
}

/// 端口转发没有规则时同样用空状态，主按钮直接进入添加表单；`x 移除` 置灰。
#[test]
fn empty_forwards_editor_offers_an_add_action() {
    let _dir = with_temp_home("forwards-empty");
    let saved = seed_forward_profile(0);
    let mut state = state_with_profiles(std::slice::from_ref(&saved));
    state.open_machines_overlay_for(&saved.id);
    state.open_machine_forwards(&saved.id);
    let text = compact(&frame_text(&mut state, 106, 36));
    let t = &crate::i18n::texts().machines;
    assert!(text.contains(&compact(t.forward_none)), "{text}");
    let buttons = machine_buttons(&state);
    assert!(
        !buttons.contains(&MachineOverlayButton::ForwardRemove),
        "没有规则时移除不可点：{buttons:?}"
    );
    let footer = state.hits.machines_footer;
    let add = state
        .hits
        .machines_actions
        .iter()
        .find(|(rect, button)| {
            *button == MachineOverlayButton::ForwardAddStart && rect.bottom() <= footer.y
        })
        .map(|(rect, _)| *rect)
        .expect("空状态的添加按钮");
    state.handle_mouse(left_click(add.x, add.y), &mut ClientShellInput::default());
    let Some(ClientShellOverlay::Machines(overlay)) = state.overlay.as_ref() else {
        panic!("machines overlay");
    };
    let super::super::machines_overlay::ClientMachinesView::Forwards(view) = &overlay.view else {
        panic!("forwards view");
    };
    assert!(view.adding, "点空状态按钮进入添加");
}

const IMPORT_PREVIEW_FIXTURE: &str = "\
Host bastion
    HostName bastion.internal

Host web
    HostName web.internal
    User deploy
    Port 2201
    ProxyJump bastion
    StrictHostKeyChecking no
";

/// C2：导入 select 候选行有 notes（会被丢弃的设置）时右侧画一个 `!`
/// 记号；旧实现先按整行宽度画正文、再用 `put_right_text` 在行尾硬叠上
/// 记号，会把最后 2 列的正文原地砍掉——数字因此可能被砍成另一个合法值
/// （`:2222` 变成 `:22!`，读起来像端口真的是 22）。正文应当只在
/// `rect.width - 2` 列里画，超出用省略号收尾。
#[test]
fn import_select_row_reserves_the_notes_marker_column_and_ellipsizes() {
    let dir = with_temp_home("c2-notes-marker");
    // HostName 长度精确算过：候选行在 64 列外层终端下宽 58 列，`" [x] "
    // + 标签 + " → " + 37 个 a + ":2222"` 正好把最后 4 位端口号推到行尾，
    // 旧实现会把 "2222" 砍成 "22!"。
    let fixture = "\
Host longhost
    HostName aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
    Port 2222
    StrictHostKeyChecking no
";
    std::fs::write(dir.join(".ssh").join("config"), fixture).unwrap();
    let mut state = state_with_profiles(&[]);
    state.open_machine_import_wizard();
    state.route_machines_key(&key(KeyCode::Enter), &mut ClientShellInput::default());
    let frame = state.compose(64, 30).expect("composed");
    let row: String = frame
        .cells
        .chunks(frame.width as usize)
        .map(|row| row.iter().map(|c| c.symbol.as_str()).collect::<String>())
        .find(|row| row.contains("longhost"))
        .expect("候选行");

    assert!(
        !row.contains("22!"),
        "端口号被硬截断砍成了另一个合法值：{row:?}"
    );
    assert!(row.contains('…'), "超出的正文应当用省略号收尾：{row:?}");
    assert!(row.contains('!'), "notes 记号仍应可见：{row:?}");
    // 省略号与记号之间不能挨着——记号有独立的 2 列预算。
    let ellipsis_at = row.find('…').expect("省略号位置");
    let mark_at = row.rfind('!').expect("记号位置");
    assert!(
        mark_at > ellipsis_at + 1,
        "notes 记号紧贴在省略号后面，判定仍在争抢同一列：{row:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// 导入仍是三步，Select 是带预览的多选清单：表头给出空格 / Enter 提示，
/// 右侧预览随焦点列出将写入的字段与被丢弃的设置；窄屏省去预览。
#[test]
fn import_select_is_a_checklist_with_a_live_preview() {
    let dir = with_temp_home("import-preview");
    std::fs::write(dir.join(".ssh").join("config"), IMPORT_PREVIEW_FIXTURE).unwrap();
    let mut state = state_with_profiles(&[]);
    state.open_machine_import_wizard();
    state.route_machines_key(&key(KeyCode::Enter), &mut ClientShellInput::default());
    let t = &crate::i18n::texts().machines;
    let f = &crate::i18n::texts().machine_form;

    let text = compact(&frame_text(&mut state, 110, 32));
    assert!(text.contains(&compact(f.import_select_hint)), "{text}");
    assert!(text.contains(&compact(f.preview_title)), "{text}");
    assert!(text.contains("[x]bastion"), "{text}");
    assert!(
        !text.contains(&compact(t.detail_proxy_jump)),
        "焦点在 bastion，预览不含 web 的跳板行：{text}"
    );

    // 焦点移到 web：预览列出用户、端口、跳板与被丢弃的设置。
    state.route_machines_key(&key(KeyCode::Down), &mut ClientShellInput::default());
    let text = compact(&frame_text(&mut state, 110, 32));
    for expected in [
        compact(t.detail_user),
        "deploy".to_owned(),
        compact(t.detail_port),
        "2201".to_owned(),
        compact(t.detail_proxy_jump),
        compact(&crate::i18n::fill(t.import_notes_fmt, &[("count", "1")])),
        "StrictHostKeyCheckingno".to_owned(),
    ] {
        assert!(text.contains(&expected), "预览缺少 {expected}：{text}");
    }
    // 预览跟随勾选状态。
    state.route_machines_key(&key(KeyCode::Char(' ')), &mut ClientShellInput::default());
    let text = compact(&frame_text(&mut state, 110, 32));
    assert!(text.contains("[]web"), "取消勾选后清单与预览同步：{text}");
    // 命中行只在左栏：预览栏不可点成候选。
    let preview_x = state
        .hits
        .machines_wizard_rows
        .iter()
        .map(|(rect, _)| rect.right())
        .max()
        .expect("清单行");
    assert!(preview_x < 110, "清单行不覆盖预览栏");

    // 窄屏：没有预览栏，清单占满宽度。
    let text = compact(&frame_text(&mut state, 70, 32));
    assert!(!text.contains(&compact(f.preview_title)), "{text}");
    // 页脚（窄屏可能两行）仍有返回。
    let footer = compact(&machines_footer_text(&mut state, 70, 32));
    assert!(footer.contains(&compact(t.hint_back)), "{footer}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// 导入的 discover 空状态走 kit 空状态：说明 + 配置路径，页脚只剩返回。
#[test]
fn import_discover_without_hosts_is_an_empty_state_with_the_path() {
    let dir = with_temp_home("import-empty-state");
    let mut state = state_with_profiles(&[]);
    state.open_machine_import_wizard();
    let text = compact(&frame_text(&mut state, 110, 30));
    assert!(text.contains(".ssh/config"), "{text}");
    let buttons = machine_buttons(&state);
    assert!(
        !buttons.contains(&MachineOverlayButton::ImportContinue),
        "没有主机时继续不可点：{buttons:?}"
    );
    click_machine_button(&mut state, MachineOverlayButton::Back);
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

/// L19（尺寸）：导入向导与它出发的机器页必须同尺寸，按 `i` 打开导入时浮层
/// 不能突然变宽 / 变窄 / 变矮。机器页 page 宽 ≥96 列走 116×34 的 dashboard，
/// 以下走 76×24 的朴素列表；上一版让导入无条件请求 116×24，结果 86–95 列时
/// 导入反而比机器页宽（82–91 对 76），≥96 列宽度一致了高度仍是 24 对 34。
/// 两页改用同一个尺寸函数后，扫 80–140 列、两种行高逐一核对宽高。
#[test]
fn import_wizard_matches_the_machines_page_size_at_every_width() {
    let dir = with_temp_home("l19-import-size");
    let build = profile("Build", "dev@build.example", "1");
    for rows in [32u16, 40] {
        for cols in 80u16..=140 {
            let mut state = state_with_profiles(std::slice::from_ref(&build));
            state.open_machines_overlay();
            state.compose(cols, rows).expect("机器页");
            let page = state.hits.machines_popup;
            state.open_machine_import_wizard();
            state.compose(cols, rows).expect("导入向导");
            let import = state.hits.machines_popup;
            assert_eq!(
                (import.width, import.height),
                (page.width, page.height),
                "{cols}×{rows}：导入向导与机器页尺寸不一致"
            );
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// L19（无主机时的「继续」）：discover 没有可导入的主机时，「enter 继续」
/// 已经不可点（`import_discover_without_hosts_is_an_empty_state_with_the_path`
/// 覆盖），但仍然画在页脚里，容易让人以为按了会有反应。没有主机可继续时
/// 干脆不画这一项，页脚只剩「esc 返回」。
#[test]
fn import_footer_hides_the_continue_hint_when_there_are_no_hosts() {
    let dir = with_temp_home("l19-no-continue-hint");
    let mut state = state_with_profiles(&[]);
    state.open_machine_import_wizard();
    let footer = compact(&machines_footer_text(&mut state, 90, 26));
    let t = &crate::i18n::texts().machines;
    assert!(
        !footer.contains(&compact(t.hint_continue)),
        "没有主机可继续时不该再画「继续」提示：{footer}"
    );
    assert!(
        footer.contains(&compact(t.hint_back)),
        "返回提示仍要在：{footer}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// 把一行单元格还原成 (列, 字形) 序列：宽字符占两格，续格不单独列出——被
/// 别的字符写进续格的内容（M4 的症状）在屏幕上看不见，这里同样看不见。
fn row_graphemes(
    cells: &[crate::protocol::CellData],
    from: usize,
    to: usize,
) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut col = from;
    while col < to {
        let symbol = cells[col].symbol.as_str();
        out.push((col, symbol));
        col += usize::from(crate::ui::display_width_u16(symbol).max(1));
    }
    out
}

/// 在字形序列里找 `needle`，返回 (起始列, 结束列)（结束列不含）。
fn find_graphemes(graphemes: &[(usize, &str)], needle: &str) -> Option<(usize, usize)> {
    let needle: Vec<String> = needle.chars().map(String::from).collect();
    (0..graphemes.len()).find_map(|start| {
        let matched = needle
            .iter()
            .enumerate()
            .all(|(offset, ch)| graphemes.get(start + offset).is_some_and(|(_, g)| g == ch));
        matched.then(|| {
            let (last_col, last) = graphemes[start + needle.len() - 1];
            (
                graphemes[start].0,
                last_col + usize::from(crate::ui::display_width_u16(last).max(1)),
            )
        })
    })
}

/// M4：导入页步骤条与配置路径重叠（`smoke-23627143` `86` 行 7：「3 完成var/
/// tmp/…」，路径首字符「/」被吃掉）。在 80 列终端下跑：导入浮层是
/// `ModalSize::Large` 的 76 列，内框 74 列，三个步骤徽标占去 24 列，52 字符的
/// 路径放不下——步骤条与路径之间至少留 1 列空白，路径以「/」开头，放不下的
/// 部分以「…」收尾；而不是右对齐硬叠上去，把「/」写进「成」的续格。
#[test]
fn import_header_reserves_room_for_the_path_next_to_the_step_indicator() {
    let dir = with_temp_home("m4-path-overlap");
    let mut state = state_with_profiles(&[]);
    state.open_machine_import_wizard();
    let long_path = "/var/tmp/herdr-smoke-20260923121719/home/.ssh/config";
    if let Some(ClientShellOverlay::Machines(overlay)) = state.overlay.as_mut() {
        if let super::super::machines_overlay::ClientMachinesView::Import(view) = &mut overlay.view
        {
            view.path = std::path::PathBuf::from(long_path);
        }
    }
    let frame = state.compose(80, 32).expect("composed");
    let popup = state.hits.machines_popup;
    assert_eq!(popup.width, 76, "80 列终端下导入浮层是 76 列");
    let width = usize::from(frame.width);
    // 上边框、标题行之后就是步骤条与路径共用的那一行；只看内框（去掉左右边框）。
    let y = usize::from(popup.y) + 2;
    let cells = &frame.cells[y * width..(y + 1) * width];
    let inner_left = usize::from(popup.x) + 1;
    let inner_right = usize::from(popup.right()) - 1;
    let graphemes = row_graphemes(cells, inner_left, inner_right);
    let text: String = graphemes.iter().map(|(_, g)| *g).collect();

    let texts = &crate::i18n::texts().machines;
    for label in [
        texts.import_step_discover,
        texts.import_step_select,
        texts.import_step_done,
    ] {
        assert!(
            find_graphemes(&graphemes, label).is_some(),
            "步骤条被吃掉：{text:?}"
        );
    }
    let (_, steps_end) =
        find_graphemes(&graphemes, texts.import_step_done).expect("最后一个步骤徽标");
    let (path_col, _) = find_graphemes(&graphemes, "/var/tmp/herdr-smoke")
        .unwrap_or_else(|| panic!("路径缺失或首字符「/」被吃掉：{text:?}"));
    assert!(
        path_col > steps_end,
        "步骤条与路径之间没有留白，判定重叠：{text:?}"
    );
    assert!(
        graphemes
            .iter()
            .filter(|(col, _)| (steps_end..path_col).contains(col))
            .all(|(_, g)| *g == " "),
        "步骤条与路径之间只能是空白：{text:?}"
    );
    // 放不下的路径以省略号收尾，且省略号之前是路径的原样前缀。
    let path_text: String = graphemes
        .iter()
        .filter(|(col, _)| *col >= path_col)
        .map(|(_, g)| *g)
        .collect::<String>()
        .trim_end()
        .to_owned();
    let kept = path_text
        .strip_suffix('…')
        .unwrap_or_else(|| panic!("超宽的路径应当以「…」收尾：{path_text:?}"));
    assert!(
        long_path.starts_with(kept) && kept.len() < long_path.len(),
        "省略号之前应当是路径的原样前缀：{path_text:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
