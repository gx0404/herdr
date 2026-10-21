//! 工具页画廊：片段、现场、工作树、广播的各个视图，深浅两套。
//! 运行：`cargo test --bin herdr --locked ui_gallery_tools -- --ignored`，再用
//! `scripts/ui_gallery.py --skip-test --filter tools` 截图。

use super::*;
use crate::client::endpoint::{
    BroadcastSet, BroadcastTarget, ProfileId, SavedSshEndpoint, SnippetExecution, SnippetLibrary,
};

type Prepare = Box<dyn Fn(&mut ClientShellState)>;

fn key(code: KeyCode) -> crate::input::TerminalKey {
    crate::input::TerminalKey::new(code, KeyModifiers::NONE)
}

fn snippet_keys(state: &mut ClientShellState, codes: &[KeyCode]) {
    for code in codes {
        state.route_snippets_key(&key(*code), &mut ClientShellInput::default());
    }
}

fn scene_keys(state: &mut ClientShellState, codes: &[KeyCode]) {
    for code in codes {
        state.route_scenes_key(&key(*code), &mut ClientShellInput::default());
    }
}

fn profile(label: &str, target: &str, seed: char) -> SavedSshEndpoint {
    let mut profile = SavedSshEndpoint::new(label, target, "default").expect("valid profile");
    profile.id = ProfileId::parse(seed.to_string().repeat(32)).expect("hex profile id");
    profile
}

fn profiles() -> Vec<SavedSshEndpoint> {
    vec![
        profile("Build", "dev@build.example", 'b'),
        profile("GPU box", "ml@gpu.example", 'c'),
    ]
}

/// 工具页的状态：本机快照 + 两台已保存（离线）的机器，片段 / 现场 / 广播读的是
/// 隔离状态目录里写好的样例。
fn tools_state(config: ClientShellConfig) -> ClientShellState {
    let mut state = ClientShellState::new(config);
    state.set_endpoint_catalog(&profiles());
    state.set_endpoint_snapshot_for_generation(
        &ClientEndpointId::Local,
        GENERATION,
        Box::new(gallery_snapshot()),
    );
    state.set_endpoint_methods(Some(vec![
        "client.views.set".into(),
        "tab.focus".into(),
        "agent.activity.read".into(),
    ]));
    state
}

fn seed_snippets() {
    let mut library = SnippetLibrary::default();
    for (label, command, variables, tags) in [
        (
            "deploy",
            "kubectl rollout restart deploy/{{name}} -n {{namespace}}",
            vec!["name", "namespace"],
            vec!["k8s"],
        ),
        (
            "logs",
            "kubectl logs -f svc/web --since=10m",
            vec![],
            vec!["k8s", "web"],
        ),
        (
            "重建索引",
            "cargo run --bin indexer -- --rebuild --verbose",
            vec![],
            vec![],
        ),
        (
            "disk usage",
            "du -sh * | sort -h | tail -20",
            vec![],
            vec!["ops"],
        ),
    ] {
        library
            .add_snippet(
                label,
                command,
                None,
                variables.into_iter().map(str::to_owned).collect(),
                tags.into_iter().map(str::to_owned).collect(),
            )
            .expect("seed snippet");
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let deploy = library.snippets[0].id.clone();
    let logs = library.snippets[1].id.clone();
    library.record_executions(vec![
        SnippetExecution {
            snippet_id: logs.clone(),
            snippet_label: "logs".into(),
            machine: "local".into(),
            pane_id: "p1".into(),
            executed_at: now.saturating_sub(4 * 3_600),
            success: true,
            error: None,
        },
        SnippetExecution {
            snippet_id: deploy.clone(),
            snippet_label: "deploy".into(),
            machine: "b".repeat(32),
            pane_id: "p2".into(),
            executed_at: now.saturating_sub(25 * 60),
            success: false,
            error: Some("pane p2 is gone".into()),
        },
        SnippetExecution {
            snippet_id: deploy,
            snippet_label: "deploy".into(),
            machine: "local".into(),
            pane_id: "p1".into(),
            executed_at: now.saturating_sub(20),
            success: true,
            error: None,
        },
    ]);
    library.store().expect("store snippets");
}

fn sample_scene(
    name: &str,
    note: Option<&str>,
    created_at: &str,
    machines: &[SavedSshEndpoint],
) -> super::super::scenes_overlay::ClientSceneSnapshot {
    super::super::scenes_overlay::ClientSceneSnapshot {
        name: name.to_owned(),
        note: note.map(str::to_owned),
        created_at: created_at.to_owned(),
        machines: machines
            .iter()
            .map(|profile| super::super::scenes_overlay::ClientSceneMachine {
                id: profile.id.as_str().to_owned(),
                label: profile.label.clone(),
            })
            .collect(),
        active_machine: "local".to_owned(),
        machine_focus: Vec::new(),
        sidebar: super::super::scenes_overlay::ClientSceneSidebar {
            width: Some(31),
            collapsed: false,
            collapsed_groups: Vec::new(),
            remote_collapsed_groups: Vec::new(),
        },
    }
}

fn seed_scenes() {
    let machines = profiles();
    super::super::scenes_overlay::store_scenes_to(
        &super::super::scenes_overlay::scene_snapshots_path(),
        &[
            sample_scene(
                "早间巡检",
                Some("构建机 + GPU，盯训练日志"),
                "2026-10-21 09:12",
                &machines,
            ),
            sample_scene("release", None, "2026-10-20 18:40", &machines[..1]),
            sample_scene("本机调试", Some("只开本机"), "2026-10-18 21:05", &[]),
        ],
    )
    .expect("seed scenes");
}

fn seed_broadcast() {
    let mut set = BroadcastSet::default();
    set.enabled = true;
    set.add_target(BroadcastTarget {
        machine: None,
        pane_id: "p1".into(),
    })
    .expect("local target");
    set.add_target(BroadcastTarget {
        machine: Some(profiles()[0].id.clone()),
        pane_id: "w2:p3".into(),
    })
    .expect("remote target");
    set.store().expect("store broadcast");
}

fn worktree_entries() -> Vec<ClientWorktreeOpenEntry> {
    vec![
        ClientWorktreeOpenEntry {
            path: "~/src/herdr".into(),
            branch: Some("feature/gx_herdr".into()),
            is_linked_worktree: false,
            is_detached: false,
            is_prunable: false,
            open_workspace_id: Some("ws1".into()),
            label: "herdr · feature/gx_herdr".into(),
        },
        ClientWorktreeOpenEntry {
            path: "~/src/herdr-worktrees/ui-unify".into(),
            branch: Some("codex/ui-unify-p2".into()),
            is_linked_worktree: true,
            is_detached: false,
            is_prunable: false,
            open_workspace_id: None,
            label: "herdr · codex/ui-unify-p2".into(),
        },
        ClientWorktreeOpenEntry {
            path: "~/src/herdr-worktrees/old-spike".into(),
            branch: Some("spike/kitty-graphics".into()),
            is_linked_worktree: true,
            is_detached: false,
            is_prunable: true,
            open_workspace_id: None,
            label: "herdr · spike/kitty-graphics".into(),
        },
    ]
}

fn open_worktrees(
    state: &mut ClientShellState,
    query: &str,
    entries: Vec<ClientWorktreeOpenEntry>,
) {
    state.overlay = Some(ClientShellOverlay::WorktreeOpen(
        ClientWorktreeOpenOverlay {
            source_workspace_id: "ws1".into(),
            entries,
            selected: 1,
            query: TextEditor::new(query, false),
            search_focused: !query.is_empty(),
            error: None,
            opening: false,
        },
    ));
}

fn remove_worktree(state: &mut ClientShellState, force: bool) {
    state.overlay = Some(ClientShellOverlay::WorktreeRemove(
        ClientWorktreeRemoveOverlay {
            workspace_id: "ws3".into(),
            path: "~/src/web-app-worktrees/fix-login-redirect".into(),
            error: None,
            removing: false,
            force_confirmation: force,
        },
    ));
}

fn scenes() -> Vec<(&'static str, Prepare, bool)> {
    let mut scenes: Vec<(&'static str, Prepare, bool)> = Vec::new();
    // ---- 片段 ----
    scenes.push((
        "snippets list",
        Box::new(|state| state.open_snippets_overlay(false)),
        true,
    ));
    scenes.push((
        "snippets empty",
        Box::new(|state| {
            state.open_snippets_overlay(false);
            if let Some(ClientShellOverlay::Snippets(overlay)) = state.overlay.as_mut() {
                overlay.library = SnippetLibrary::default();
            }
        }),
        false,
    ));
    scenes.push((
        "snippets no match",
        Box::new(|state| {
            state.open_snippets_overlay(false);
            if let Some(ClientShellOverlay::Snippets(overlay)) = state.overlay.as_mut() {
                overlay.query = TextEditor::new("zzz", false);
                overlay.search_focused = true;
            }
        }),
        false,
    ));
    scenes.push((
        "snippets form",
        Box::new(|state| {
            state.open_snippets_overlay(false);
            snippet_keys(state, &[KeyCode::Char('e')]);
        }),
        false,
    ));
    scenes.push((
        "snippets run targets",
        Box::new(|state| {
            state.open_snippets_overlay(false);
            snippet_keys(state, &[KeyCode::Down, KeyCode::Enter]);
        }),
        false,
    ));
    scenes.push((
        "snippets pick pane",
        Box::new(|state| {
            state.open_snippets_overlay(false);
            snippet_keys(
                state,
                &[KeyCode::Down, KeyCode::Enter, KeyCode::Down, KeyCode::Enter],
            );
        }),
        false,
    ));
    scenes.push((
        "snippets pick machines",
        Box::new(|state| {
            state.open_snippets_overlay(false);
            snippet_keys(
                state,
                &[
                    KeyCode::Down,
                    KeyCode::Enter,
                    KeyCode::Down,
                    KeyCode::Down,
                    KeyCode::Enter,
                ],
            );
        }),
        false,
    ));
    scenes.push((
        "snippets variables",
        Box::new(|state| {
            state.open_snippets_overlay(false);
            snippet_keys(state, &[KeyCode::Enter, KeyCode::Enter]);
        }),
        false,
    ));
    scenes.push((
        "snippets confirm",
        Box::new(|state| {
            state.open_snippets_overlay(false);
            snippet_keys(state, &[KeyCode::Down, KeyCode::Enter, KeyCode::Enter]);
        }),
        true,
    ));
    scenes.push((
        "snippets history",
        Box::new(|state| {
            state.open_snippets_overlay(false);
            snippet_keys(state, &[KeyCode::Char('h'), KeyCode::Down]);
        }),
        false,
    ));
    scenes.push((
        "snippets delete",
        Box::new(|state| {
            state.open_snippets_overlay(false);
            snippet_keys(state, &[KeyCode::Char('x')]);
        }),
        false,
    ));
    // ---- 现场 ----
    scenes.push((
        "scenes list",
        Box::new(|state| state.open_scenes_overlay()),
        true,
    ));
    scenes.push((
        "scenes empty",
        Box::new(|state| {
            state.open_scenes_overlay();
            if let Some(ClientShellOverlay::Scenes(overlay)) = state.overlay.as_mut() {
                overlay.scenes.clear();
            }
        }),
        false,
    ));
    scenes.push((
        "scenes save",
        Box::new(|state| {
            state.open_scenes_overlay();
            scene_keys(state, &[KeyCode::Char('s')]);
        }),
        false,
    ));
    scenes.push((
        "scenes rename",
        Box::new(|state| {
            state.open_scenes_overlay();
            scene_keys(state, &[KeyCode::Char('r')]);
        }),
        false,
    ));
    scenes.push((
        "scenes delete",
        Box::new(|state| {
            state.open_scenes_overlay();
            scene_keys(state, &[KeyCode::Down, KeyCode::Char('d')]);
        }),
        false,
    ));
    scenes.push((
        "scenes restore confirm",
        Box::new(|state| {
            state.open_scenes_overlay();
            let machines = profiles();
            if let Some(ClientShellOverlay::Scenes(overlay)) = state.overlay.as_mut() {
                overlay.view = super::super::scenes_overlay::ClientScenesView::ConfirmRestore {
                    index: 1,
                    disable: vec![machines[1].id.clone()],
                    labels: vec![machines[1].label.clone()],
                };
            }
        }),
        false,
    ));
    // ---- 工作树 ----
    scenes.push((
        "worktree create",
        Box::new(|state| {
            state.overlay = Some(ClientShellOverlay::WorktreeCreate(
                ClientWorktreeCreateOverlay {
                    source_workspace_id: "ws1".into(),
                    repo_name: "herdr".into(),
                    branch: TextEditor::new("feature/toast-safe-area", false),
                    checkout_path: "~/src/herdr-worktrees/feature-toast-safe-area".into(),
                    error: None,
                    creating: false,
                },
            ));
        }),
        false,
    ));
    scenes.push((
        "worktree open",
        Box::new(|state| open_worktrees(state, "", worktree_entries())),
        true,
    ));
    scenes.push((
        "worktree no match",
        Box::new(|state| open_worktrees(state, "zzz", worktree_entries())),
        false,
    ));
    scenes.push((
        "worktree empty",
        Box::new(|state| open_worktrees(state, "", Vec::new())),
        false,
    ));
    scenes.push((
        "worktree remove",
        Box::new(|state| remove_worktree(state, false)),
        false,
    ));
    scenes.push((
        "worktree remove force",
        Box::new(|state| remove_worktree(state, true)),
        false,
    ));
    // ---- 广播 ----
    scenes.push((
        "broadcast list",
        Box::new(|state| {
            state.refresh_broadcast_mirror();
            state.open_broadcast_overlay();
        }),
        true,
    ));
    scenes.push((
        "broadcast empty",
        Box::new(|state| {
            state.open_broadcast_overlay();
            state.broadcast = BroadcastSet::default();
        }),
        false,
    ));
    scenes.push((
        "broadcast pick machine",
        Box::new(|state| {
            state.refresh_broadcast_mirror();
            state.open_broadcast_overlay();
            state.route_broadcast_key(&key(KeyCode::Char('a')), &mut ClientShellInput::default());
        }),
        false,
    ));
    scenes
}

#[test]
#[ignore = "manual visual gallery; run `just ui-gallery`"]
fn ui_gallery_tools() {
    let _lang = crate::i18n::lang_guard(crate::i18n::Lang::ZhCn);
    let _dir = isolated_state_home("gallery-tools");
    seed_snippets();
    seed_scenes();
    seed_broadcast();
    let dark = || ClientShellConfig::from_config(&Config::default());
    let light = || {
        let mut config = Config::default();
        config.theme.name = Some("graphite-light".into());
        ClientShellConfig::from_config(&config)
    };
    let mut shots = Vec::new();
    let mut step = 0;
    for (name, prepare, narrow) in scenes() {
        let mut sizes = vec![(160u16, 45u16)];
        if narrow {
            sizes.push((80, 24));
        }
        for (cols, rows) in sizes {
            for (theme, host) in [("dark", DARK_HOST), ("light", LIGHT_HOST)] {
                let config = if theme == "light" { light() } else { dark() };
                let mut state = tools_state(config);
                let _ = shoot_workbench(&mut state, cols, rows, &mut step);
                prepare(&mut state);
                let frame = shoot_workbench(&mut state, cols, rows, &mut step);
                shots.push(Shot {
                    title: format!("{name} {theme} {cols}x{rows}"),
                    frame,
                    host,
                });
            }
        }
    }
    write_shots("tools", &shots);
}
