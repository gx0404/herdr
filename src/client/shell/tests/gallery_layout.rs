//! 工作台布局细节画廊：拖动把手的显隐（默认隐藏、面板头悬浮、窗格上边框悬浮、调整
//! 布局）、去重后的面包屑（同名、单标签、多端点）、侧栏选中行与悬浮行、状态栏芯片。
//! `python -B scripts/ui_gallery.py --filter layout`。

use super::*;
use crate::client::shell::dock::PanelId;
use crate::client::shell::feedback::ChromeHover;

/// 换一份快照（修订号前进），保留工作台与命中区。
fn reseed(state: &mut ClientShellState, edit: fn(&mut ClientShellSnapshot)) {
    let mut next = gallery_snapshot();
    next.revision += 1;
    edit(&mut next);
    state.set_endpoint_snapshot_for_generation(
        &ClientEndpointId::Local,
        GENERATION,
        Box::new(next),
    );
}

fn focus_pane(snapshot: &mut ClientShellSnapshot, pane: &str) {
    snapshot.focused_pane_id = Some(pane.into());
    for entry in &mut snapshot.panes {
        entry.focused = entry.pane_id == pane;
    }
    for entry in &mut snapshot.agents {
        entry.focused = entry.pane_id == pane;
    }
    if let Some(selected) = snapshot.panes.iter().find(|entry| entry.pane_id == pane) {
        snapshot.focused_tab_id = Some(selected.tab_id.clone());
        snapshot.focused_workspace_id = Some(selected.workspace_id.clone());
    }
    for workspace in &mut snapshot.workspaces {
        workspace.focused =
            snapshot.focused_workspace_id.as_deref() == Some(&workspace.workspace_id);
    }
    for tab in &mut snapshot.tabs {
        tab.focused = snapshot.focused_tab_id.as_deref() == Some(&tab.tab_id);
    }
}

fn first_terminal(state: &ClientShellState) -> Option<PanelId> {
    state
        .workbench
        .dock
        .groups
        .first()
        .map(|group| PanelId::Terminal(group.id))
}

#[test]
#[ignore = "manual visual gallery; run `just ui-gallery`"]
fn ui_gallery_layout() {
    let _lang = crate::i18n::lang_guard(crate::i18n::Lang::ZhCn);
    let dark = || ClientShellConfig::from_config(&Config::default());
    let light = || {
        let mut config = Config::default();
        config.theme.name = Some("graphite-light".into());
        ClientShellConfig::from_config(&config)
    };
    let none: Prepare = |_| {};
    let panel_hover: Prepare = |state| {
        state.hover = first_terminal(state).map(ChromeHover::PanelHeader);
    };
    let pane_hover: Prepare = |state| {
        state.hover = Some(ChromeHover::PaneHeader("p1".into()));
    };
    let arrange: Prepare = |state| state.workbench.arranging = true;
    let same_name: Prepare = |state| {
        reseed(state, |snapshot| {
            // 标签起名与 agent 同名、工作区也叫 claude：面包屑压成「claude › ⠋ claude」，
            // 不再出现三遍。
            if let Some(tab) = snapshot.tabs.iter_mut().find(|tab| tab.tab_id == "ws1-t1") {
                tab.label = "claude".into();
                tab.custom_label = true;
            }
            if let Some(ws) = snapshot
                .workspaces
                .iter_mut()
                .find(|ws| ws.workspace_id == "ws1")
            {
                ws.label = "claude".into();
            }
        });
    };
    let single_tab: Prepare = |state| {
        reseed(state, |snapshot| focus_pane(snapshot, "p4"));
    };
    let multi_endpoint: Prepare = |state| {
        let profile = crate::client::endpoint::SavedSshEndpoint {
            id: crate::client::endpoint::ProfileId::parse("0123456789abcdef0123456789abcdef")
                .expect("profile id"),
            label: "devbox".into(),
            target: "dev@devbox.example".into(),
            session: "agents".into(),
            enabled: true,
            ..crate::client::endpoint::SavedSshEndpoint::new("base", "base", "default")
                .expect("valid base profile")
        };
        let remote = ClientEndpointId::Ssh(profile.id.clone());
        state.set_endpoint_catalog(&[profile]);
        state.set_endpoint_status(&remote, ClientEndpointStatus::Online);
        let mut projection = gallery_snapshot();
        projection.boot_id = "remote-boot".into();
        state.set_endpoint_snapshot(&remote, Box::new(projection));
    };
    let sidebar_selected: Prepare = |state| {
        // 前缀 + w 进会话导航，光标下移一行：选中行底色 + 粗体 + 首列竖条；指针同时悬浮
        // 在另一行上，只换底色。
        state.handle_input_bytes(&[0x02]);
        state.handle_input_bytes(b"w");
        state.handle_input_bytes(b"\x1b[B");
        state.hover = Some(ChromeHover::WorkspaceRow {
            endpoint_id: ClientEndpointId::Local,
            workspace_id: "ws4".into(),
        });
    };
    let sidebar_hover: Prepare = |state| {
        state.hover = Some(ChromeHover::WorkspaceRow {
            endpoint_id: ClientEndpointId::Local,
            workspace_id: "ws3".into(),
        });
    };

    let mut scenes: Vec<(&str, (u16, u16), Prepare)> = Vec::new();
    for size in [(200, 50), (160, 45), (80, 24), (60, 20)] {
        scenes.push(("default", size, none));
    }
    for size in [(160, 45), (80, 24)] {
        scenes.push(("panel-header-hover", size, panel_hover));
        scenes.push(("pane-header-hover", size, pane_hover));
        scenes.push(("arrange", size, arrange));
    }
    scenes.push(("same-name-breadcrumb", (160, 45), same_name));
    scenes.push(("single-tab-breadcrumb", (160, 45), single_tab));
    scenes.push(("multi-endpoint-breadcrumb", (160, 45), multi_endpoint));
    scenes.push(("multi-endpoint-breadcrumb", (80, 24), multi_endpoint));
    scenes.push(("sidebar-selected", (160, 45), sidebar_selected));
    scenes.push(("sidebar-hover", (160, 45), sidebar_hover));

    let mut shots = Vec::new();
    let mut step = 0;
    for (name, size, prepare) in scenes {
        for (theme, host) in [("dark", DARK_HOST), ("light", LIGHT_HOST)] {
            let config = if theme == "light" { light() } else { dark() };
            let frame = shoot_scene(config, size, prepare, &mut step);
            shots.push(Shot {
                title: format!("layout {name} {theme} {}x{}", size.0, size.1),
                frame,
                host,
            });
        }
    }
    write_shots("layout", &shots);
}
