use super::*;
use crate::client::endpoint::{
    ClientEndpointId, ClientEndpointStatus, EndpointCatalog, ProfileId, SavedSshEndpoint,
};
use crossterm::event::{KeyCode, KeyModifiers};

fn profile(label: &str, target: &str, seed: &str, enabled: bool) -> SavedSshEndpoint {
    let hex = format!("{seed:0>32}")
        .chars()
        .map(|ch| if ch.is_ascii_hexdigit() { ch } else { 'a' })
        .take(32)
        .collect::<String>();
    let mut profile = SavedSshEndpoint::new(label, target, "default").expect("valid profile");
    profile.id = ProfileId::parse(hex).expect("hex profile id");
    profile.enabled = enabled;
    profile
}

fn with_temp_state_home(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("herdr-scenes-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp state home");
    // Safety: nextest isolates every test in its own process, so mutating the
    // process environment here cannot race other tests.
    unsafe { std::env::set_var("XDG_STATE_HOME", &dir) };
    dir
}

fn state_with_profiles(profiles: &[SavedSshEndpoint]) -> ClientShellState {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_endpoint_catalog(profiles);
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    state.set_endpoint_status(&ClientEndpointId::Local, ClientEndpointStatus::Online);
    state
}

fn seed_catalog(profiles: &[SavedSshEndpoint]) {
    let mut catalog = EndpointCatalog::default();
    catalog.ssh = profiles.to_vec();
    catalog.store_profiles().expect("seed catalog");
}

fn key(code: KeyCode) -> crate::input::TerminalKey {
    crate::input::TerminalKey::new(code, KeyModifiers::empty())
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

fn scene(
    name: &str,
    machines: Vec<super::super::scenes_overlay::ClientSceneMachine>,
) -> super::super::scenes_overlay::ClientSceneSnapshot {
    super::super::scenes_overlay::ClientSceneSnapshot {
        name: name.to_owned(),
        note: None,
        created_at: "2026-09-17 09:00".to_owned(),
        machines,
        active_machine: "local".to_owned(),
        machine_focus: Vec::new(),
        sidebar: super::super::scenes_overlay::ClientSceneSidebar {
            width: Some(31),
            collapsed: false,
            collapsed_groups: vec!["/scene".to_owned()],
            remote_collapsed_groups: Vec::new(),
        },
    }
}

fn scene_machine(profile: &SavedSshEndpoint) -> super::super::scenes_overlay::ClientSceneMachine {
    super::super::scenes_overlay::ClientSceneMachine {
        id: profile.id.as_str().to_owned(),
        label: profile.label.clone(),
    }
}

fn scenes_path(_dir: &std::path::Path) -> std::path::PathBuf {
    // The env override set by `with_temp_state_home` resolves through the
    // same helper the overlay uses (the app dir name differs in debug).
    super::super::scenes_overlay::scene_snapshots_path()
}

#[test]
fn scene_save_persists_snapshot_and_shows_success_notice() {
    let dir = with_temp_state_home("save");
    let build = profile("Build", "build.example", "1", true);
    let stage = profile("Stage", "stage.example", "2", false);
    let mut state = state_with_profiles(&[build.clone(), stage.clone()]);
    state.sidebar_width = 27;
    state.collapsed_groups.insert("/repo".to_owned());

    state.open_scenes_overlay();
    state.open_scene_save_form();
    let Some(ClientShellOverlay::Scenes(overlay)) = state.overlay.as_mut() else {
        panic!("scenes overlay");
    };
    let super::super::scenes_overlay::ClientScenesView::Save(form) = &mut overlay.view else {
        panic!("save form view");
    };
    form.name = text_editor_for("morning");
    let mut outcome = ClientShellInput::default();
    state.submit_scene_save_form(&mut outcome);

    let content = std::fs::read_to_string(scenes_path(&dir)).expect("scene file");
    let saved: serde_json::Value = serde_json::from_str(&content).expect("scene json");
    assert_eq!(saved["version"], 1);
    let entry = &saved["scenes"][0];
    assert_eq!(entry["name"], "morning");
    assert_eq!(entry["active_machine"], "local");
    assert_eq!(entry["sidebar"]["width"], 27);
    assert_eq!(entry["sidebar"]["collapsed_groups"][0], "/repo");
    let machine_ids = entry["machines"]
        .as_array()
        .expect("machines array")
        .iter()
        .map(|machine| machine["id"].as_str().expect("id").to_owned())
        .collect::<Vec<_>>();
    assert_eq!(machine_ids, vec![build.id.as_str().to_owned()]);

    let notice = state.visible_endpoint_notice.as_ref().expect("save toast");
    assert_eq!(notice.key.kind, ClientEndpointNoticeKind::Success);
    assert!(matches!(
        state.overlay,
        Some(ClientShellOverlay::Scenes(
            super::super::scenes_overlay::ClientScenesOverlay {
                view: super::super::scenes_overlay::ClientScenesView::List,
                ..
            }
        ))
    ));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn scene_restore_enables_disables_and_focuses_active_machine() {
    let dir = with_temp_state_home("restore");
    let build = profile("Build", "build.example", "1", true);
    let stage = profile("Stage", "stage.example", "2", false);
    let extra = profile("Extra", "extra.example", "3", true);
    seed_catalog(&[build.clone(), stage.clone(), extra.clone()]);
    let mut state = state_with_profiles(&[build.clone(), stage.clone(), extra.clone()]);
    let stage_id = ClientEndpointId::Ssh(stage.id.clone());
    state.set_endpoint_status(&stage_id, ClientEndpointStatus::Online);
    state.cache_endpoint_snapshot(&stage_id, Box::new(snapshot()));

    let mut snapshot_scene = scene("crew", vec![scene_machine(&build), scene_machine(&stage)]);
    snapshot_scene.active_machine = format!("ssh:{}", stage.id.as_str());
    snapshot_scene.machine_focus = vec![super::super::scenes_overlay::ClientSceneMachineFocus {
        machine: format!("ssh:{}", stage.id.as_str()),
        workspace_id: Some("ws_1".to_owned()),
        tab_id: None,
    }];
    super::super::scenes_overlay::store_scenes_to(
        &super::super::scenes_overlay::scene_snapshots_path(),
        std::slice::from_ref(&snapshot_scene),
    )
    .expect("seed scenes");

    state.open_scenes_overlay();
    let mut outcome = ClientShellInput::default();
    state.restore_selected_scene(&mut outcome);

    let catalog = EndpointCatalog::load().expect("catalog");
    let enabled_of = |id: &ProfileId| {
        catalog
            .ssh
            .iter()
            .find(|profile| &profile.id == id)
            .map(|profile| profile.enabled)
    };
    assert_eq!(
        enabled_of(&build.id),
        Some(true),
        "scene machines stay enabled"
    );
    assert_eq!(enabled_of(&stage.id), Some(true), "scene machines enabled");
    assert_eq!(
        enabled_of(&extra.id),
        Some(false),
        "machines outside the scene disabled"
    );

    let activation = outcome
        .actions
        .iter()
        .find_map(|action| match action {
            ClientShellAction::ActivateEndpoint {
                endpoint_id,
                target,
            } => Some((endpoint_id.clone(), target.clone())),
            _ => None,
        })
        .expect("activation action");
    assert_eq!(activation.0, stage_id);
    assert_eq!(
        activation.1,
        Some(ClientEndpointFocusTarget::Workspace("ws_1".to_owned()))
    );

    assert_eq!(state.sidebar_width, 31, "scene sidebar width applied");
    assert!(state.collapsed_groups.contains("/scene"));
    assert!(state.overlay.is_none(), "overlay closes after restore");
    let notice = state
        .visible_endpoint_notice
        .as_ref()
        .expect("restore toast");
    assert_eq!(notice.key.kind, ClientEndpointNoticeKind::Success);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn scene_restore_reports_missing_machines_without_failing() {
    let dir = with_temp_state_home("missing");
    let build = profile("Build", "build.example", "1", true);
    seed_catalog(std::slice::from_ref(&build));
    let mut state = state_with_profiles(std::slice::from_ref(&build));
    let ghost = super::super::scenes_overlay::ClientSceneMachine {
        id: "f".repeat(32),
        label: "Ghost".to_owned(),
    };
    let snapshot_scene = scene("old crew", vec![scene_machine(&build), ghost]);
    super::super::scenes_overlay::store_scenes_to(
        &super::super::scenes_overlay::scene_snapshots_path(),
        std::slice::from_ref(&snapshot_scene),
    )
    .expect("seed scenes");

    state.open_scenes_overlay();
    let mut outcome = ClientShellInput::default();
    state.restore_selected_scene(&mut outcome);

    let notice = state
        .visible_endpoint_notice
        .as_ref()
        .expect("restore toast");
    assert_eq!(notice.key.kind, ClientEndpointNoticeKind::Success);
    assert!(notice.body.contains("Ghost"), "{}", notice.body);
    let catalog = EndpointCatalog::load().expect("catalog");
    assert!(
        catalog.ssh.iter().all(|profile| profile.enabled),
        "present machines stay enabled"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn scenes_overlay_lists_scenes_and_delete_flow_updates_store() {
    let dir = with_temp_state_home("list");
    let mut state = state_with_profiles(&[]);
    super::super::scenes_overlay::store_scenes_to(
        &super::super::scenes_overlay::scene_snapshots_path(),
        &[scene("morning", Vec::new()), scene("night", Vec::new())],
    )
    .expect("seed scenes");

    state.open_scenes_overlay();
    let text = frame_text(&mut state, 106, 32);
    assert!(text.contains("morning"), "frame: {text}");
    assert!(text.contains("night"), "frame: {text}");

    // Toggle the restore option and move the selection.
    state.route_scenes_key(&key(KeyCode::Char('x')), &mut ClientShellInput::default());
    let Some(ClientShellOverlay::Scenes(overlay)) = state.overlay.as_ref() else {
        panic!("scenes overlay");
    };
    assert!(!overlay.restore_disable_others);
    state.route_scenes_key(&key(KeyCode::Down), &mut ClientShellInput::default());
    let Some(ClientShellOverlay::Scenes(overlay)) = state.overlay.as_ref() else {
        panic!("scenes overlay");
    };
    assert_eq!(overlay.selected, 1);

    // Delete the selected scene after confirmation.
    state.route_scenes_key(&key(KeyCode::Char('d')), &mut ClientShellInput::default());
    assert!(matches!(
        state.overlay,
        Some(ClientShellOverlay::Scenes(
            super::super::scenes_overlay::ClientScenesOverlay {
                view: super::super::scenes_overlay::ClientScenesView::ConfirmDelete(1),
                ..
            }
        ))
    ));
    state.route_scenes_key(&key(KeyCode::Enter), &mut ClientShellInput::default());
    let remaining =
        super::super::scenes_overlay::load_scenes_from(&scenes_path(&dir)).expect("load scenes");
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].name, "morning");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn scene_rename_flow_updates_store() {
    let dir = with_temp_state_home("rename");
    let mut state = state_with_profiles(&[]);
    super::super::scenes_overlay::store_scenes_to(
        &super::super::scenes_overlay::scene_snapshots_path(),
        &[scene("morning", Vec::new())],
    )
    .expect("seed scenes");

    state.open_scenes_overlay();
    state.route_scenes_key(&key(KeyCode::Char('r')), &mut ClientShellInput::default());
    let Some(ClientShellOverlay::Scenes(overlay)) = state.overlay.as_mut() else {
        panic!("scenes overlay");
    };
    let super::super::scenes_overlay::ClientScenesView::Rename { editor, .. } = &mut overlay.view
    else {
        panic!("rename view");
    };
    assert_eq!(editor.as_str(), "morning");
    *editor = text_editor_for("standup");
    state.route_scenes_key(&key(KeyCode::Enter), &mut ClientShellInput::default());

    let remaining =
        super::super::scenes_overlay::load_scenes_from(&scenes_path(&dir)).expect("load scenes");
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].name, "standup");
    assert!(matches!(
        state.overlay,
        Some(ClientShellOverlay::Scenes(
            super::super::scenes_overlay::ClientScenesOverlay {
                view: super::super::scenes_overlay::ClientScenesView::List,
                ..
            }
        ))
    ));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn palette_offers_scene_actions() {
    let _dir = with_temp_state_home("palette");
    let mut state = state_with_profiles(&[]);
    state.open_command_search();
    let row = {
        let Some(ClientShellOverlay::CommandPalette(palette)) = state.overlay.as_ref() else {
            panic!("palette");
        };
        super::super::command_palette::palette_rows(palette)
            .iter()
            .position(|row| row.item.id == "scene:save")
            .expect("scene save row")
    };
    let mut outcome = ClientShellInput::default();
    state.activate_palette_item(row, &mut outcome);
    assert!(matches!(
        state.overlay,
        Some(ClientShellOverlay::Scenes(
            super::super::scenes_overlay::ClientScenesOverlay {
                view: super::super::scenes_overlay::ClientScenesView::Save(_),
                ..
            }
        ))
    ));
}

#[test]
fn notice_success_level_maps_to_success_toast_style() {
    assert_eq!(
        super::super::feedback::ClientToastLevel::from_notice_kind(
            ClientEndpointNoticeKind::Success
        ),
        super::super::feedback::ClientToastLevel::Success
    );
}

fn text_editor_for(text: &str) -> TextEditor {
    TextEditor::new(text, false)
}
