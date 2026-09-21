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
    // 「停用现场之外的机器」现在默认关闭：显式打开后恢复先进确认页，确认才写目录。
    state.route_scenes_key(&key(KeyCode::Char('x')), &mut ClientShellInput::default());
    let mut outcome = ClientShellInput::default();
    state.restore_selected_scene(&mut outcome);
    state.activate_scene_primary(&mut outcome);

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
    assert!(
        overlay.restore_disable_others,
        "默认关闭，按 x 打开「停用现场之外的机器」"
    );
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

/// TOOL-03：A 改名为已存在的 B 时，两条现场都必须留在盘上，表单里报错而不是
/// 静默删掉被重命名的那一条。
#[test]
fn scene_rename_to_existing_name_keeps_both_scenes() {
    let dir = with_temp_state_home("rename-collision");
    let mut state = state_with_profiles(&[]);
    super::super::scenes_overlay::store_scenes_to(
        &super::super::scenes_overlay::scene_snapshots_path(),
        &[scene("morning", Vec::new()), scene("night", Vec::new())],
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
    *editor = text_editor_for("night");
    state.route_scenes_key(&key(KeyCode::Enter), &mut ClientShellInput::default());

    let stored =
        super::super::scenes_overlay::load_scenes_from(&scenes_path(&dir)).expect("load scenes");
    let names = stored
        .iter()
        .map(|scene| scene.name.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        vec!["morning".to_owned(), "night".to_owned()],
        "撞名改名不得删除任何一条现场"
    );

    let Some(ClientShellOverlay::Scenes(overlay)) = state.overlay.as_ref() else {
        panic!("scenes overlay");
    };
    let super::super::scenes_overlay::ClientScenesView::Rename { error, .. } = &overlay.view else {
        panic!("撞名后必须留在重命名表单里");
    };
    assert!(error.is_some(), "撞名必须给出可见错误");
    let _ = std::fs::remove_dir_all(&dir);
}

/// TOOL-02：列表单击只选中；恢复走按钮 / Enter / 同一行的二次点击。
#[test]
fn scenes_row_single_click_selects_without_restoring() {
    use crossterm::event::{MouseButton, MouseEventKind};

    let dir = with_temp_state_home("click");
    let build = profile("Build", "build.example", "1", true);
    let extra = profile("Extra", "extra.example", "3", true);
    seed_catalog(&[build.clone(), extra.clone()]);
    let mut state = state_with_profiles(&[build.clone(), extra.clone()]);
    state.config.double_click_window = std::time::Duration::from_secs(60);
    super::super::scenes_overlay::store_scenes_to(
        &super::super::scenes_overlay::scene_snapshots_path(),
        &[
            scene("morning", vec![scene_machine(&build)]),
            scene("night", vec![scene_machine(&build)]),
        ],
    )
    .expect("seed scenes");

    state.open_scenes_overlay();
    state.compose(106, 32).expect("scenes overlay frame");
    let (rect, _) = state
        .hits
        .scenes_rows
        .iter()
        .find(|(_, index)| *index == 1)
        .copied()
        .expect("second scene row");
    let click = |state: &mut ClientShellState| {
        state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: rect.x + 1,
            row: rect.y,
            modifiers: KeyModifiers::empty(),
        })])
    };

    let first = click(&mut state);
    assert!(first.actions.is_empty(), "单击不得触发恢复");
    let Some(ClientShellOverlay::Scenes(overlay)) = state.overlay.as_ref() else {
        panic!("单击后浮层必须留在原地");
    };
    assert_eq!(overlay.selected, 1, "单击只移动选中");

    let second = click(&mut state);
    assert!(
        state.overlay.is_none(),
        "同一行的二次点击才执行恢复并关闭浮层"
    );
    assert!(second.repaint);
    assert_eq!(state.sidebar_width, 31, "二次点击应真正恢复现场");
    let notice = state
        .visible_endpoint_notice
        .as_ref()
        .expect("restore toast");
    assert_eq!(notice.key.kind, ClientEndpointNoticeKind::Success);
    let _ = std::fs::remove_dir_all(&dir);
}

/// TOOL-02：`restore_disable_others` 默认关闭；打开后恢复必须先确认要断开的
/// 机器，确认前不得写端点目录。
#[test]
fn scene_restore_disable_others_defaults_off_and_confirms_before_disconnecting() {
    let dir = with_temp_state_home("disable-others");
    let build = profile("Build", "build.example", "1", true);
    let extra = profile("Extra", "extra.example", "3", true);
    seed_catalog(&[build.clone(), extra.clone()]);
    let mut state = state_with_profiles(&[build.clone(), extra.clone()]);
    super::super::scenes_overlay::store_scenes_to(
        &super::super::scenes_overlay::scene_snapshots_path(),
        std::slice::from_ref(&scene("crew", vec![scene_machine(&build)])),
    )
    .expect("seed scenes");

    state.open_scenes_overlay();
    let Some(ClientShellOverlay::Scenes(overlay)) = state.overlay.as_ref() else {
        panic!("scenes overlay");
    };
    assert!(
        !overlay.restore_disable_others,
        "默认不再停用现场之外的机器"
    );
    let mut outcome = ClientShellInput::default();
    state.restore_selected_scene(&mut outcome);
    let catalog = EndpointCatalog::load().expect("catalog");
    assert!(
        catalog.ssh.iter().all(|profile| profile.enabled),
        "默认恢复不得断开现场之外的机器"
    );

    state.open_scenes_overlay();
    state.route_scenes_key(&key(KeyCode::Char('x')), &mut ClientShellInput::default());
    state.route_scenes_key(&key(KeyCode::Enter), &mut ClientShellInput::default());
    assert!(
        matches!(
            state.overlay,
            Some(ClientShellOverlay::Scenes(
                super::super::scenes_overlay::ClientScenesOverlay {
                    view: super::super::scenes_overlay::ClientScenesView::ConfirmRestore { .. },
                    ..
                }
            ))
        ),
        "开关打开后恢复必须先进确认页"
    );
    let catalog = EndpointCatalog::load().expect("catalog");
    assert!(
        catalog.ssh.iter().all(|profile| profile.enabled),
        "确认前不得写端点目录"
    );
    let text = frame_text(&mut state, 106, 32);
    assert!(text.contains("Extra"), "确认页应列出将被停用的机器: {text}");

    // 确认键与列表的恢复键不同（y / ctrl+↵），回车在确认页是空操作。
    state.route_scenes_key(&key(KeyCode::Char('y')), &mut ClientShellInput::default());
    let catalog = EndpointCatalog::load().expect("catalog");
    assert_eq!(
        catalog
            .ssh
            .iter()
            .find(|profile| profile.id == extra.id)
            .map(|profile| profile.enabled),
        Some(false),
        "确认后才真正停用现场之外的机器"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// 批 2 遗留：scenes 各视图必须有互不相同的步进指纹，否则同视图同键的
/// 破坏性恢复会被自动重复的 Repeat 反复触发。
#[test]
fn scenes_views_have_distinct_overlay_steps() {
    use super::super::scenes_overlay::{ClientSceneForm, ClientScenesView};

    let views = [
        ClientScenesView::List,
        ClientScenesView::Save(Box::new(ClientSceneForm {
            name: TextEditor::default(),
            note: TextEditor::default(),
            focused: 0,
            error: None,
        })),
        ClientScenesView::Rename {
            index: 0,
            editor: TextEditor::default(),
            error: None,
        },
        ClientScenesView::ConfirmDelete(0),
        ClientScenesView::ConfirmRestore {
            index: 0,
            disable: Vec::new(),
            labels: Vec::new(),
        },
    ];
    let steps = views
        .iter()
        .map(super::super::scenes_overlay::ClientScenesView::step)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        steps.len(),
        views.len(),
        "每个 scenes 视图都要有独立的 step 值"
    );
}

/// TOOL-02 + C-02：长按回车不得从列表一路走完「恢复确认」。
#[test]
fn held_enter_does_not_confirm_scene_restore() {
    use crossterm::event::KeyEventKind;

    let dir = with_temp_state_home("held-enter");
    let build = profile("Build", "build.example", "1", true);
    let extra = profile("Extra", "extra.example", "3", true);
    seed_catalog(&[build.clone(), extra.clone()]);
    let mut state = state_with_profiles(&[build.clone(), extra.clone()]);
    super::super::scenes_overlay::store_scenes_to(
        &super::super::scenes_overlay::scene_snapshots_path(),
        std::slice::from_ref(&scene("crew", vec![scene_machine(&build)])),
    )
    .expect("seed scenes");

    state.open_scenes_overlay();
    state.route_scenes_key(&key(KeyCode::Char('x')), &mut ClientShellInput::default());
    let enter = key(KeyCode::Enter);
    state.handle_raw_events(vec![RawInputEvent::Key(enter.clone())]);
    assert!(matches!(
        state.overlay,
        Some(ClientShellOverlay::Scenes(
            super::super::scenes_overlay::ClientScenesOverlay {
                view: super::super::scenes_overlay::ClientScenesView::ConfirmRestore { .. },
                ..
            }
        ))
    ));
    for _ in 0..4 {
        state.handle_raw_events(vec![RawInputEvent::Key(
            enter.clone().with_kind(KeyEventKind::Repeat),
        )]);
        assert!(
            matches!(
                state.overlay,
                Some(ClientShellOverlay::Scenes(
                    super::super::scenes_overlay::ClientScenesOverlay {
                        view: super::super::scenes_overlay::ClientScenesView::ConfirmRestore { .. },
                        ..
                    }
                ))
            ),
            "长按回车不得确认恢复"
        );
    }
    let catalog = EndpointCatalog::load().expect("catalog");
    assert!(
        catalog.ssh.iter().all(|profile| profile.enabled),
        "长按期间不得写端点目录"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// TOOL-02 + C-02（非 kitty 宿主面）：确认页的确认键与列表的恢复键不同，
/// 普通 `Press` 连发回车（宿主不支持 kitty 事件类型时自动重复的真实形态）
/// 走不完「列表 → 确认页 → 真的恢复」；只有 `y` 才落盘。
#[test]
fn plain_enter_presses_never_confirm_scene_restore() {
    let dir = with_temp_state_home("plain-enter");
    let build = profile("Build", "build.example", "1", true);
    let extra = profile("Extra", "extra.example", "3", true);
    seed_catalog(&[build.clone(), extra.clone()]);
    let mut state = state_with_profiles(&[build.clone(), extra.clone()]);
    super::super::scenes_overlay::store_scenes_to(
        &super::super::scenes_overlay::scene_snapshots_path(),
        std::slice::from_ref(&scene("crew", vec![scene_machine(&build)])),
    )
    .expect("seed scenes");

    state.open_scenes_overlay();
    state.route_scenes_key(&key(KeyCode::Char('x')), &mut ClientShellInput::default());
    for _ in 0..4 {
        state.handle_input_bytes(b"\r");
    }
    assert!(
        matches!(
            state.overlay,
            Some(ClientShellOverlay::Scenes(
                super::super::scenes_overlay::ClientScenesOverlay {
                    view: super::super::scenes_overlay::ClientScenesView::ConfirmRestore { .. },
                    ..
                }
            ))
        ),
        "连发普通回车不得走完确认页"
    );
    let catalog = EndpointCatalog::load().expect("catalog");
    assert!(
        catalog.ssh.iter().all(|profile| profile.enabled),
        "确认前不得写端点目录"
    );

    state.handle_input_bytes(b"y");
    assert!(state.overlay.is_none(), "y 才确认恢复");
    let catalog = EndpointCatalog::load().expect("catalog");
    assert_eq!(
        catalog
            .ssh
            .iter()
            .find(|profile| profile.id == extra.id)
            .map(|profile| profile.enabled),
        Some(false),
        "确认后才真正停用现场之外的机器"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// TOOL-02：确认页的价值是把名单摊开，名单放不下时必须显式说明还有几台，
/// 不能静默丢弃溢出行。
#[test]
fn scene_restore_confirm_names_overflowing_machines_with_a_remainder_line() {
    let dir = with_temp_state_home("confirm-overflow");
    let build = profile("Build", "build.example", "1", true);
    let mut profiles = vec![build.clone()];
    for index in 0..24u32 {
        profiles.push(profile(
            &format!("Fleet{index:02}"),
            "fleet.example",
            &format!("{:x}", index + 0x20),
            true,
        ));
    }
    seed_catalog(&profiles);
    let mut state = state_with_profiles(&profiles);
    super::super::scenes_overlay::store_scenes_to(
        &super::super::scenes_overlay::scene_snapshots_path(),
        std::slice::from_ref(&scene("crew", vec![scene_machine(&build)])),
    )
    .expect("seed scenes");

    state.open_scenes_overlay();
    state.route_scenes_key(&key(KeyCode::Char('x')), &mut ClientShellInput::default());
    state.route_scenes_key(&key(KeyCode::Enter), &mut ClientShellInput::default());
    let text = frame_text(&mut state, 106, 32);
    assert!(text.contains("Fleet00"), "首批机器名必须完整列出: {text}");
    // 宽字符在帧里带补位空格，比较时两边都去空格。
    let squeeze = |value: &str| value.replace(' ', "");
    let more_prefix = squeeze(
        crate::i18n::texts()
            .scenes
            .restore_confirm_more_fmt
            .split("{count}")
            .next()
            .unwrap_or(""),
    );
    assert!(!more_prefix.is_empty(), "溢出提示文案需要有可断言的前缀");
    assert!(
        squeeze(&text).contains(&more_prefix),
        "名单溢出时必须提示还有几台: {text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// TOOL-02：确认页展示的名单就是确认后执行的名单。确认页会停留任意时长，
/// 期间快照事件可能新增已启用的机器——它没被摊给用户看过，就不许被停用。
#[test]
fn scene_restore_confirm_only_disables_the_machines_it_listed() {
    let dir = with_temp_state_home("confirm-frozen");
    let build = profile("Build", "build.example", "1", true);
    let extra = profile("Extra", "extra.example", "3", true);
    seed_catalog(&[build.clone(), extra.clone()]);
    let mut state = state_with_profiles(&[build.clone(), extra.clone()]);
    super::super::scenes_overlay::store_scenes_to(
        &super::super::scenes_overlay::scene_snapshots_path(),
        std::slice::from_ref(&scene("crew", vec![scene_machine(&build)])),
    )
    .expect("seed scenes");

    state.open_scenes_overlay();
    state.route_scenes_key(&key(KeyCode::Char('x')), &mut ClientShellInput::default());
    state.route_scenes_key(&key(KeyCode::Enter), &mut ClientShellInput::default());

    // 确认页停留期间，服务端快照新增一台已启用的机器。
    let late = profile("Late", "late.example", "4", true);
    let profiles = [build.clone(), extra.clone(), late.clone()];
    seed_catalog(&profiles);
    state.set_endpoint_catalog(&profiles);

    state.handle_input_bytes(b"y");
    let catalog = EndpointCatalog::load().expect("catalog");
    let enabled_of = |id: &ProfileId| {
        catalog
            .ssh
            .iter()
            .find(|profile| &profile.id == id)
            .map(|profile| profile.enabled)
    };
    assert_eq!(
        enabled_of(&late.id),
        Some(true),
        "确认页没列出的机器不得被停用"
    );
    assert_eq!(enabled_of(&extra.id), Some(false), "列出的机器照常停用");
    let _ = std::fs::remove_dir_all(&dir);
}

/// TOOL-02 的快路径：开关打开但没有任何机器需要停用时直接恢复，不弹空名单
/// 确认页。这条守门防止条件被回归成「总是确认」或「总是跳过」。
#[test]
fn scene_restore_with_nothing_to_disable_skips_the_confirmation() {
    let dir = with_temp_state_home("confirm-fast-path");
    let build = profile("Build", "build.example", "1", true);
    let idle = profile("Idle", "idle.example", "2", false);
    seed_catalog(&[build.clone(), idle.clone()]);
    let mut state = state_with_profiles(&[build.clone(), idle.clone()]);
    super::super::scenes_overlay::store_scenes_to(
        &super::super::scenes_overlay::scene_snapshots_path(),
        std::slice::from_ref(&scene("crew", vec![scene_machine(&build)])),
    )
    .expect("seed scenes");

    state.open_scenes_overlay();
    state.route_scenes_key(&key(KeyCode::Char('x')), &mut ClientShellInput::default());
    state.route_scenes_key(&key(KeyCode::Enter), &mut ClientShellInput::default());

    assert!(
        state.overlay.is_none(),
        "没有机器要停用时不得插入确认页，直接恢复"
    );
    assert_eq!(state.sidebar_width, 31, "快路径仍然真的恢复了现场");
    let catalog = EndpointCatalog::load().expect("catalog");
    assert_eq!(
        catalog
            .ssh
            .iter()
            .find(|profile| profile.id == build.id)
            .map(|profile| profile.enabled),
        Some(true),
        "现场内的机器保持启用"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// TOOL-03 的另一半：保存表单手输一个已存在的现场名，旧快照不得被静默替换，
/// 语义与改名撞名一致（拒绝并报错）。
#[test]
fn scene_save_with_an_existing_name_is_rejected() {
    let dir = with_temp_state_home("save-collision");
    let build = profile("Build", "build.example", "1", true);
    let mut state = state_with_profiles(&[]);
    super::super::scenes_overlay::store_scenes_to(
        &super::super::scenes_overlay::scene_snapshots_path(),
        std::slice::from_ref(&scene("morning", vec![scene_machine(&build)])),
    )
    .expect("seed scenes");

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

    let stored =
        super::super::scenes_overlay::load_scenes_from(&scenes_path(&dir)).expect("load scenes");
    assert_eq!(stored.len(), 1, "撞名保存不得新增条目");
    assert_eq!(
        stored[0].machines.len(),
        1,
        "旧快照的机器集合不得被静默替换"
    );
    let Some(ClientShellOverlay::Scenes(overlay)) = state.overlay.as_ref() else {
        panic!("scenes overlay");
    };
    let super::super::scenes_overlay::ClientScenesView::Save(form) = &overlay.view else {
        panic!("撞名后必须留在保存表单里");
    };
    assert!(form.error.is_some(), "撞名保存必须给出可见错误");
    let _ = std::fs::remove_dir_all(&dir);
}

/// TOOL-02：双击窗口外的第二次点击只选中——窗口判据本身也要有守门。
#[test]
fn scenes_click_outside_the_double_click_window_only_selects() {
    use crossterm::event::{MouseButton, MouseEventKind};

    let dir = with_temp_state_home("click-window");
    let build = profile("Build", "build.example", "1", true);
    seed_catalog(std::slice::from_ref(&build));
    let mut state = state_with_profiles(std::slice::from_ref(&build));
    state.config.double_click_window = std::time::Duration::from_nanos(1);
    super::super::scenes_overlay::store_scenes_to(
        &super::super::scenes_overlay::scene_snapshots_path(),
        &[
            scene("morning", vec![scene_machine(&build)]),
            scene("night", vec![scene_machine(&build)]),
        ],
    )
    .expect("seed scenes");

    state.open_scenes_overlay();
    state.compose(106, 32).expect("scenes overlay frame");
    let (rect, _) = state
        .hits
        .scenes_rows
        .iter()
        .find(|(_, index)| *index == 1)
        .copied()
        .expect("second scene row");
    for _ in 0..2 {
        state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: rect.x + 1,
            row: rect.y,
            modifiers: KeyModifiers::empty(),
        })]);
    }
    let Some(ClientShellOverlay::Scenes(overlay)) = state.overlay.as_ref() else {
        panic!("窗口外的两次点击都只选中，浮层必须留在原地");
    };
    assert_eq!(overlay.selected, 1);
    let _ = std::fs::remove_dir_all(&dir);
}

/// TOOL-02：离开子视图会清掉点击痕迹，否则「点一行 → r 改名 → Esc 回列表 →
/// 再点同一行」会在双击窗口内直接恢复。
#[test]
fn leaving_a_scene_subview_clears_the_click_trail() {
    use crossterm::event::{MouseButton, MouseEventKind};

    let dir = with_temp_state_home("click-trail");
    let build = profile("Build", "build.example", "1", true);
    seed_catalog(std::slice::from_ref(&build));
    let mut state = state_with_profiles(std::slice::from_ref(&build));
    state.config.double_click_window = std::time::Duration::from_secs(60);
    super::super::scenes_overlay::store_scenes_to(
        &super::super::scenes_overlay::scene_snapshots_path(),
        &[
            scene("morning", vec![scene_machine(&build)]),
            scene("night", vec![scene_machine(&build)]),
        ],
    )
    .expect("seed scenes");

    state.open_scenes_overlay();
    state.compose(106, 32).expect("scenes overlay frame");
    let (rect, _) = state
        .hits
        .scenes_rows
        .iter()
        .find(|(_, index)| *index == 1)
        .copied()
        .expect("second scene row");
    let click = |state: &mut ClientShellState| {
        state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: rect.x + 1,
            row: rect.y,
            modifiers: KeyModifiers::empty(),
        })]);
    };
    click(&mut state);
    state.route_scenes_key(&key(KeyCode::Char('r')), &mut ClientShellInput::default());
    state.route_scenes_key(&key(KeyCode::Esc), &mut ClientShellInput::default());
    state.compose(106, 32).expect("scenes overlay frame");
    click(&mut state);

    let Some(ClientShellOverlay::Scenes(overlay)) = state.overlay.as_ref() else {
        panic!("进出子视图后第一次点击只能选中");
    };
    assert_eq!(overlay.selected, 1);
    let _ = std::fs::remove_dir_all(&dir);
}

/// MENU-01：指针划过现场列表只写 `hovered`，键盘选中不动——否则 ↑↓ 选好一条
/// 之后鼠标随便路过一行，回车恢复的就是「鼠标最后路过的现场」。
#[test]
fn scene_hover_does_not_move_the_keyboard_selection() {
    use crossterm::event::MouseEventKind;

    let dir = with_temp_state_home("hover-selection");
    let build = profile("Build", "build.example", "1", true);
    seed_catalog(std::slice::from_ref(&build));
    let mut state = state_with_profiles(std::slice::from_ref(&build));
    super::super::scenes_overlay::store_scenes_to(
        &super::super::scenes_overlay::scene_snapshots_path(),
        &[
            scene("morning", vec![scene_machine(&build)]),
            scene("night", vec![scene_machine(&build)]),
        ],
    )
    .expect("seed scenes");

    state.open_scenes_overlay();
    state.compose(106, 32).expect("scenes overlay frame");
    let (second, _) = state
        .hits
        .scenes_rows
        .iter()
        .find(|(_, index)| *index == 1)
        .copied()
        .expect("second scene row");

    state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Moved,
        column: second.x + 1,
        row: second.y,
        modifiers: KeyModifiers::empty(),
    })]);
    let Some(ClientShellOverlay::Scenes(overlay)) = state.overlay.as_ref() else {
        panic!("scenes overlay");
    };
    assert_eq!(overlay.hovered, Some(1));
    assert_eq!(overlay.selected, 0, "指针不改写键盘选中");

    // 悬浮行用弱色，选中行仍是 accent 反色。
    let frame = state.compose(106, 32).expect("hovered scenes overlay");
    let buffer = frame.to_ratatui_buffer().expect("frame should reconstruct");
    let (first, _) = state
        .hits
        .scenes_rows
        .iter()
        .find(|(_, index)| *index == 0)
        .copied()
        .expect("first scene row");
    assert_eq!(
        buffer[(second.x, second.y)].bg,
        state.config.components.hover_bg
    );
    assert_eq!(buffer[(first.x, first.y)].bg, state.config.palette.accent);

    // 指针移出行区域：hover 清空，键盘选中不动，回车恢复的仍是第 0 条。
    state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Moved,
        column: second.x + 1,
        row: 0,
        modifiers: KeyModifiers::empty(),
    })]);
    let Some(ClientShellOverlay::Scenes(overlay)) = state.overlay.as_ref() else {
        panic!("scenes overlay");
    };
    assert_eq!(overlay.hovered, None, "移出行区域后不留残影");
    assert_eq!(overlay.selected, 0);
    let _ = std::fs::remove_dir_all(&dir);
}

/// C-20 残留面：滚轮只滚视口，不改键盘选中；回车恢复的仍是原选中现场，
/// 不是「滚到的那一条」。
#[test]
fn scenes_wheel_scrolls_the_viewport_without_moving_the_selection() {
    use crossterm::event::MouseEventKind;

    let dir = with_temp_state_home("wheel-selection");
    let profiles = (0..12)
        .map(|index| {
            profile(
                &format!("m{index:02}"),
                &format!("m{index:02}.example"),
                &format!("{index}"),
                false,
            )
        })
        .collect::<Vec<_>>();
    seed_catalog(&profiles);
    let mut state = state_with_profiles(&profiles);
    let snapshots = (0..12)
        .map(|index| {
            scene(
                &format!("s{index:02}"),
                vec![scene_machine(&profiles[index])],
            )
        })
        .collect::<Vec<_>>();
    super::super::scenes_overlay::store_scenes_to(
        &super::super::scenes_overlay::scene_snapshots_path(),
        &snapshots,
    )
    .expect("seed scenes");

    state.open_scenes_overlay();
    state.compose(106, 24).expect("scenes overlay frame");
    let popup = state.hits.scenes_popup;
    assert!(!popup.is_empty(), "浮层几何");
    let (scroll, selected) = match state.overlay.as_ref() {
        Some(ClientShellOverlay::Scenes(overlay)) => (overlay.scroll, overlay.selected),
        _ => panic!("scenes overlay"),
    };
    assert_eq!((scroll, selected), (0, 0));

    for _ in 0..2 {
        state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: popup.x + popup.width / 2,
            row: popup.y + popup.height / 2,
            modifiers: KeyModifiers::empty(),
        })]);
    }
    state.compose(106, 24).expect("scrolled frame");
    let Some(ClientShellOverlay::Scenes(overlay)) = state.overlay.as_ref() else {
        panic!("scenes overlay");
    };
    assert!(
        overlay.scroll > 0,
        "滚轮应移动视口: scroll={}",
        overlay.scroll
    );
    assert_eq!(overlay.selected, 0, "滚轮不改写键盘选中");

    // 视口真的滚了：第一条现场不再显示，后面的现场露出来。
    let text = frame_text(&mut state, 106, 24);
    assert!(!text.contains("s00"), "frame: {text}");
    assert!(text.contains("s06"), "frame: {text}");

    // 回车恢复的仍是 s00：只有 m00 被启用。
    state.route_scenes_key(&key(KeyCode::Enter), &mut ClientShellInput::default());
    let catalog = EndpointCatalog::load().expect("catalog");
    let enabled = |profile: &SavedSshEndpoint| {
        catalog
            .ssh
            .iter()
            .find(|candidate| candidate.id == profile.id)
            .is_some_and(|candidate| candidate.enabled)
    };
    assert!(enabled(&profiles[0]), "回车应恢复原选中现场 s00");
    assert!(!enabled(&profiles[6]), "滚到的 s06 不该被恢复");
    let _ = std::fs::remove_dir_all(&dir);
}

/// STATE-03：列表变空的瞬间不能把滚动位置清零——快照重载 / 全部删除后列表
/// 回来时，用户滚到的位置要还在。
#[test]
fn emptying_the_scene_list_keeps_the_scroll_position() {
    use crossterm::event::MouseEventKind;

    let dir = with_temp_state_home("state-03");
    let profiles = (0..12)
        .map(|index| {
            profile(
                &format!("m{index:02}"),
                &format!("m{index:02}.example"),
                &format!("{index}"),
                true,
            )
        })
        .collect::<Vec<_>>();
    seed_catalog(&profiles);
    let mut state = state_with_profiles(&profiles);
    let snapshots = (0..12)
        .map(|index| {
            scene(
                &format!("s{index:02}"),
                vec![scene_machine(&profiles[index])],
            )
        })
        .collect::<Vec<_>>();
    super::super::scenes_overlay::store_scenes_to(
        &super::super::scenes_overlay::scene_snapshots_path(),
        &snapshots,
    )
    .expect("seed scenes");

    state.open_scenes_overlay();
    state.compose(106, 24).expect("scenes overlay frame");
    let popup = state.hits.scenes_popup;
    for _ in 0..2 {
        state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: popup.x + popup.width / 2,
            row: popup.y + popup.height / 2,
            modifiers: KeyModifiers::empty(),
        })]);
    }
    state.compose(106, 24).expect("scrolled frame");
    let scrolled = match state.overlay.as_ref() {
        Some(ClientShellOverlay::Scenes(overlay)) => overlay.scroll,
        _ => panic!("scenes overlay"),
    };
    assert!(scrolled > 0, "滚轮先滚动视口");

    // 列表被清空（快照重载 / 全部删除）：渲染不得把位置清零。
    if let Some(ClientShellOverlay::Scenes(overlay)) = state.overlay.as_mut() {
        overlay.scenes.clear();
    }
    state.compose(106, 24).expect("empty list frame");
    let after_empty = match state.overlay.as_ref() {
        Some(ClientShellOverlay::Scenes(overlay)) => overlay.scroll,
        _ => panic!("scenes overlay"),
    };
    assert_eq!(after_empty, scrolled, "空列表不改写滚动位置（STATE-03）");

    // 列表回来：位置还在。
    if let Some(ClientShellOverlay::Scenes(overlay)) = state.overlay.as_mut() {
        overlay.scenes = snapshots;
    }
    state.compose(106, 24).expect("restored list frame");
    let restored = match state.overlay.as_ref() {
        Some(ClientShellOverlay::Scenes(overlay)) => overlay.scroll,
        _ => panic!("scenes overlay"),
    };
    assert_eq!(restored, scrolled, "列表回来后视口位置保持");
    let _ = std::fs::remove_dir_all(&dir);
}
