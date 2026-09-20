use super::*;
use crate::input::TerminalKey;

fn workspaces(count: usize) -> ClientShellSnapshot {
    let mut projected = snapshot();
    projected.workspaces = (1..=count)
        .map(|number| {
            let mut workspace = projected.workspaces[0].clone();
            workspace.workspace_id = format!("ws_{number}");
            workspace.number = number;
            workspace.focused = number == 1;
            workspace
        })
        .collect();
    projected
}

fn grouped_workspaces() -> ClientShellSnapshot {
    let mut projected = workspaces(3);
    for (index, linked) in [(0, false), (2, true)] {
        projected.workspaces[index].worktree = Some(ClientShellWorktree {
            key: "repo".into(),
            label: "repo".into(),
            is_linked_worktree: linked,
        });
    }
    projected
}

fn navigation_state(mut projected: ClientShellSnapshot) -> (ClientShellState, ClientEndpointId) {
    let (mut state, remote) = state_with_remote();
    state.set_snapshot(Box::new(projected.clone()));
    projected.boot_id = "remote-boot".into();
    state.set_endpoint_snapshot(&remote, Box::new(projected));
    (state, remote)
}

fn preview_key(state: &mut ClientShellState, bytes: &[u8]) {
    let outcome = state.handle_input_bytes(bytes);
    assert!(outcome.actions.is_empty(), "{bytes:?}");
    assert!(outcome.requests.is_empty(), "{bytes:?}");
    assert!(outcome.repaint, "{bytes:?}");
}

fn enter_navigation(state: &mut ClientShellState) {
    preview_key(state, &[0x02]);
    preview_key(state, b"w");
    assert_eq!(state.mode, ClientShellMode::Navigate);
}

fn assert_selected(state: &ClientShellState, endpoint: &ClientEndpointId, workspace: &str) {
    assert_eq!(
        state.navigate_workspace_id,
        state.navigation_target(endpoint, workspace)
    );
}

fn workspace_rect(state: &ClientShellState, endpoint: &ClientEndpointId, workspace: &str) -> Rect {
    state
        .hits
        .workspaces
        .iter()
        .find(|hit| &hit.endpoint_id == endpoint && hit.workspace_id == workspace)
        .map(|hit| hit.rect)
        .or_else(|| {
            state.hits.mobile_targets.iter().find_map(|(rect, target)| {
                matches!(target, ClientMobileTarget::Workspace { endpoint_id, workspace_id }
                if endpoint_id == endpoint && workspace_id == workspace)
                .then_some(*rect)
            })
        })
        .expect("visible workspace")
}

#[test]
fn navigation_highlights_only_the_preview_and_activates_on_enter() {
    for (compact, cols) in [(true, 100), (false, 100), (false, 44)] {
        for terminal_theme in [false, true] {
            let (mut state, remote) = navigation_state(workspaces(2));
            state.sidebar_collapsed = compact;
            if terminal_theme {
                state.config.palette = Palette::terminal();
            }
            state.compose(cols, 28).unwrap();
            enter_navigation(&mut state);
            for (endpoint, collision, steps) in [
                (&ClientEndpointId::Local, &remote, 1),
                (&remote, &ClientEndpointId::Local, 2),
            ] {
                for _ in 0..steps {
                    preview_key(&mut state, b"\x1b[B");
                }
                assert_selected(&state, endpoint, "ws_2");
                let buffer = state
                    .compose(cols, 28)
                    .unwrap()
                    .to_ratatui_buffer()
                    .unwrap();
                let selected = workspace_rect(&state, endpoint, "ws_2");
                let other = workspace_rect(&state, collision, "ws_2");
                let focused = workspace_rect(&state, &ClientEndpointId::Local, "ws_1");
                let palette = &state.config.palette;
                let color = if cols == 44 && palette.surface0 != ratatui::style::Color::Reset {
                    palette.surface0
                } else {
                    palette.selection_row_bg()
                };
                assert_eq!(buffer[(selected.x + 2, selected.y)].bg, color);
                assert_ne!(buffer[(other.x + 2, other.y)].bg, color);
                let focused_bg = if cols == 44 {
                    palette.surface_dim
                } else {
                    palette.active_row_bg
                };
                assert_eq!(buffer[(focused.x + 2, focused.y)].bg, focused_bg);
                // 导航光标必须和「聚焦行」可区分：terminal 主题曾让两者同为
                // DarkGray，选中位置完全看不出来（上游 #4300）。
                assert_ne!(
                    buffer[(selected.x + 2, selected.y)].bg,
                    buffer[(focused.x + 2, focused.y)].bg,
                    "cols={cols} terminal_theme={terminal_theme}"
                );
            }
            assert_eq!(state.snapshot.as_ref().unwrap().boot_id, "boot-1");
            assert_eq!(
                state
                    .snapshot
                    .as_ref()
                    .unwrap()
                    .focused_workspace_id
                    .as_deref(),
                Some("ws_1")
            );
            assert_eq!(state.pane_surface.as_ref().unwrap().boot_id, "boot-1");
            let enter = state.handle_input_bytes(b"\r");
            assert!(enter.requests.is_empty());
            assert!(
                matches!(enter.actions.as_slice(), [ClientShellAction::ActivateEndpoint {
                endpoint_id, target: Some(ClientEndpointFocusTarget::Workspace(id)),
            }] if endpoint_id == &remote && id == "ws_2")
            );
            assert_eq!(state.active_endpoint_id, ClientEndpointId::Local);
            assert_eq!(state.mode, ClientShellMode::Terminal);
            assert!(state.navigate_workspace_id.is_none());
        }
    }
}

#[test]
fn foreign_preview_blocks_keyboard_actions_but_keeps_active_action_context() {
    let (mut state, remote) = state_with_remote();
    state.compose(100, 28).unwrap();
    enter_navigation(&mut state);
    preview_key(&mut state, b"\x1b[B");
    for confirm in [false, true] {
        state.config.confirm_close = confirm;
        for key in [
            b"W".as_slice(),
            b"D",
            b"\x1b[D",
            b"\x1b[C",
            b"\t",
            b"1",
            b"c",
            b"N",
        ] {
            preview_key(&mut state, key);
            assert!(state.overlay.is_none());
            assert_eq!(state.mode, ClientShellMode::Navigate);
        }
    }
    assert_selected(&state, &remote, "ws_1");
    let mut remote_snapshot = workspaces(2);
    remote_snapshot.boot_id = "remote-boot".into();
    state.set_endpoint_snapshot(&remote, Box::new(remote_snapshot));
    preview_key(&mut state, b"\x1b[B");
    assert_selected(&state, &remote, "ws_2");
    assert_eq!(state.workspace_action_id().as_deref(), Some("ws_1"));
    state.config.prompt_new_workspace_name = false;
    let mut create = ClientShellInput::default();
    state.record_binding(
        crate::input::KeybindMatch::Action(crate::input::KeybindAction::NewWorkspace),
        &mut create,
    );
    assert!(
        matches!(create.actions.as_slice(), [ClientShellAction::Endpoint { endpoint_id: ClientEndpointId::Local, request, .. }]
        if matches!(&request.method, crate::api::schema::Method::WorkspaceCreate(params) if params.source_workspace_id.as_deref() == Some("ws_1")))
    );
    preview_key(&mut state, b"\x1b");
    assert!(state.navigate_workspace_id.is_none());
    assert_eq!(state.active_endpoint_id, ClientEndpointId::Local);
    assert!(state.activate_endpoint_projection(&remote));
    enter_navigation(&mut state);
    preview_key(&mut state, b"W");
    assert!(matches!(state.overlay, Some(ClientShellOverlay::Rename(_))));
}

#[test]
fn empty_workspace_navigation_enter_exits_without_focusing() {
    let (mut state, _) = state_with_remote();
    let mut empty = workspaces(0);
    empty.tabs.clear();
    empty.panes.clear();
    empty.focused_workspace_id = None;
    empty.focused_tab_id = None;
    empty.focused_pane_id = None;
    state.set_snapshot(Box::new(empty));
    enter_navigation(&mut state);
    assert!(state.navigate_workspace_id.is_none());
    let enter = state.handle_input_bytes(b"\r");
    assert!(enter.actions.is_empty() && enter.requests.is_empty() && enter.repaint);
    assert_eq!(state.mode, ClientShellMode::Terminal);
}

#[test]
fn foreign_workspace_preview_blocks_paste_into_hidden_copy_search() {
    let (mut state, _) = state_with_remote();
    let mut pane_surface = surface();
    pane_surface.panes[0].scroll = Some(crate::protocol::PaneSurfaceScrollMetrics {
        offset_from_bottom: 0,
        max_offset_from_bottom: 20,
        viewport_rows: 2,
    });
    state.set_pane_surface(pane_surface);
    state.compose(100, 28).unwrap();
    assert!(state.enter_copy_mode(&mut ClientShellInput::default()));
    enter_navigation(&mut state);
    // Seed the hidden prompt after navigation: text editing consumes the prefix key.
    state.copy_mode.as_mut().unwrap().search_prompt = Some(ClientCopySearchPrompt {
        direction: crate::api::schema::PaneCopySearchDirection::Forward,
        query: "original".into(),
    });
    preview_key(&mut state, b"\x1b[B");
    assert!(state.workspace_preview_action_blocked());
    assert!(!state.modal_paste_target_active());
    let key = crate::input::TerminalKey::new(KeyCode::Char('v'), KeyModifiers::CONTROL);
    assert!(!state.handle_modal_paste_shortcut_with(
        &key,
        &mut ClientShellInput::default(),
        || { panic!("hidden search must not read the clipboard") }
    ));
    let paste = state.handle_raw_events(vec![RawInputEvent::Paste("unexpected".into())]);
    assert!(paste.actions.is_empty() && paste.requests.is_empty());
    assert_eq!(
        state.copy_mode.unwrap().search_prompt.unwrap().query,
        "original".into()
    );
}

#[test]
fn mouse_clicks_cancel_remote_workspace_navigation() {
    for pane in [false, true] {
        let (mut state, _) = state_with_remote();
        state.compose(100, 28).unwrap();
        enter_navigation(&mut state);
        preview_key(&mut state, b"\x1b[B");
        state.compose(100, 28).unwrap();
        let rect = if pane {
            state.hits.panes[0].inner_rect
        } else {
            workspace_rect(&state, &ClientEndpointId::Local, "ws_1")
        };
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
                kind,
                column: rect.x + 2,
                row: rect.y,
                modifiers: KeyModifiers::empty(),
            })]);
        }
        assert_eq!(state.mode, ClientShellMode::Terminal);
        assert!(state.navigate_workspace_id.is_none());
        enter_navigation(&mut state);
        assert_selected(&state, &ClientEndpointId::Local, "ws_1");
    }
}

#[test]
fn single_machine_compact_navigation_includes_visible_collapsed_group_children() {
    let (mut state, _) = navigation_state(grouped_workspaces());
    state.set_endpoint_catalog(&[]);
    state.toggle_collapsed_group(&ClientEndpointId::Local, "repo".into());
    state.sidebar_collapsed = true;
    state.compose(100, 28).unwrap();
    workspace_rect(&state, &ClientEndpointId::Local, "ws_3");
    enter_navigation(&mut state);
    for id in ["ws_2", "ws_3"] {
        preview_key(&mut state, b"\x1b[B");
        assert_selected(&state, &ClientEndpointId::Local, id);
    }
}

#[test]
fn workspace_navigation_respects_each_machines_visible_worktree_groups() {
    for (cols, compact, unavailable, show_child) in [
        (100, false, false, false),
        (100, true, false, true),
        (44, false, false, true),
        (44, true, true, false),
    ] {
        let (mut state, remote) = navigation_state(grouped_workspaces());
        state.toggle_collapsed_group(&remote, "repo".into());
        state.sidebar_collapsed = compact;
        if unavailable {
            state.pane_surface = None;
        }
        state.compose(cols, 28).unwrap();
        enter_navigation(&mut state);
        let local = if compact && !unavailable {
            ["ws_2", "ws_3"]
        } else {
            ["ws_3", "ws_2"]
        };
        let remote_ids: &[&str] = if !show_child {
            &["ws_1", "ws_2"]
        } else if compact {
            &["ws_1", "ws_2", "ws_3"]
        } else {
            &["ws_1", "ws_3", "ws_2"]
        };
        for (endpoint, ids) in [
            (&ClientEndpointId::Local, local.as_slice()),
            (&remote, remote_ids),
        ] {
            for id in ids {
                preview_key(&mut state, b"\x1b[B");
                assert_selected(&state, endpoint, id);
                state.compose(cols, 28).unwrap();
                workspace_rect(&state, endpoint, id);
            }
        }
        assert!(state.group_is_collapsed(&remote, "repo"));
        assert!(!state.group_is_collapsed(&ClientEndpointId::Local, "repo"));
    }
}

#[test]
fn foreign_preview_survives_local_updates_and_rejects_stale_enter() {
    for invalidation in [
        "offline",
        "disabled",
        "removed",
        "deleted",
        "boot",
        "generation",
    ] {
        let (mut state, remote_id) = state_with_remote();
        let mut remote = workspaces(2);
        remote.boot_id = "remote-boot".into();
        state.set_endpoint_snapshot_for_generation(&remote_id, 7, Box::new(remote.clone()));
        state.compose(100, 28).unwrap();
        enter_navigation(&mut state);
        for _ in 0..2 {
            preview_key(&mut state, b"\x1b[B");
        }
        assert_selected(&state, &remote_id, "ws_2");
        let selected = state.navigate_workspace_id.clone();
        remote.revision += 1;
        state.set_endpoint_snapshot_for_generation(&remote_id, 7, Box::new(remote.clone()));
        assert_eq!(state.navigate_workspace_id, selected);
        assert!(state.navigation_target_valid(selected.as_ref().unwrap()));
        let mut local = snapshot();
        local.revision += 1;
        state.set_snapshot(Box::new(local));
        assert_eq!(state.navigate_workspace_id, selected);
        match invalidation {
            "offline" => state.set_endpoint_status(&remote_id, ClientEndpointStatus::Reconnecting),
            "disabled" => {
                let mut profile = remote_profile();
                profile.enabled = false;
                state.set_endpoint_catalog(&[profile]);
            }
            "removed" => state.set_endpoint_catalog(&[]),
            "deleted" => {
                remote.revision += 1;
                remote.workspaces.pop();
                state.set_endpoint_snapshot_for_generation(&remote_id, 7, Box::new(remote));
            }
            "boot" => {
                remote.boot_id = "restarted-remote".into();
                state.set_endpoint_snapshot_for_generation(&remote_id, 7, Box::new(remote));
            }
            "generation" => {
                state.cache_endpoint_snapshot_for_generation(&remote_id, 8, Box::new(remote))
            }
            _ => unreachable!(),
        }
        preview_key(&mut state, b"\r");
        assert_eq!(state.active_endpoint_id, ClientEndpointId::Local);
        assert_eq!(state.mode, ClientShellMode::Navigate);
        assert!(state.visible_endpoint_notice.is_some());
        assert!(!state.navigation_target_valid(state.navigate_workspace_id.as_ref().unwrap()));
        preview_key(&mut state, b"\x1b[B");
        assert!(state.navigation_target_valid(state.navigate_workspace_id.as_ref().unwrap()));
    }
}

#[test]
fn navigation_uses_displayed_group_order_when_local_is_unavailable() {
    for cols in [100, 44] {
        let (mut state, remote) = navigation_state(grouped_workspaces());
        state.set_endpoint_status(&ClientEndpointId::Local, ClientEndpointStatus::Reconnecting);
        state.select_unavailable_local();
        state.sidebar_collapsed = true;
        state.compose(cols, 18).unwrap();
        enter_navigation(&mut state);
        for id in ["ws_1", "ws_3", "ws_2"] {
            preview_key(&mut state, b"\x1b[B");
            assert_selected(&state, &remote, id);
            state.compose(cols, 18).unwrap();
            workspace_rect(&state, &remote, id);
        }
        let enter = state.handle_input_bytes(b"\r");
        assert!(
            matches!(enter.actions.as_slice(), [ClientShellAction::ActivateEndpoint {
            endpoint_id, target: Some(ClientEndpointFocusTarget::Workspace(id)),
        }] if endpoint_id == &remote && id == "ws_2")
        );
    }
}

#[test]
fn active_preview_is_not_retargeted_by_deletion_or_reboot() {
    for invalidation in ["deleted", "boot", "generation"] {
        let (mut state, _) = state_with_remote();
        let mut local = workspaces(2);
        state.set_endpoint_snapshot_for_generation(
            &ClientEndpointId::Local,
            7,
            Box::new(local.clone()),
        );
        state.set_pane_surface(surface());
        state.compose(100, 28).unwrap();
        enter_navigation(&mut state);
        preview_key(&mut state, b"\x1b[B");
        assert_selected(&state, &ClientEndpointId::Local, "ws_2");
        let selected = state.navigate_workspace_id.clone();
        match invalidation {
            "boot" => local.boot_id = "new-local-boot".into(),
            "deleted" => {
                local.revision += 1;
                local.workspaces.pop();
            }
            _ => {}
        }
        let generation = if invalidation == "generation" { 8 } else { 7 };
        state.set_endpoint_snapshot_for_generation(
            &ClientEndpointId::Local,
            generation,
            Box::new(local),
        );
        assert_eq!(state.navigate_workspace_id, selected);
        preview_key(&mut state, b"\r");
        assert_eq!(state.mode, ClientShellMode::Navigate);
        assert!(state.visible_endpoint_notice.is_some());
        for confirm in [false, true] {
            state.config.confirm_close = confirm;
            for key in [b"W", b"D"] {
                preview_key(&mut state, key);
            }
            assert!(state.overlay.is_none());
            assert_eq!(state.mode, ClientShellMode::Navigate);
        }
        assert_eq!(state.workspace_action_id().as_deref(), Some("ws_1"));
    }
}

#[test]
fn aggregate_navigation_reveals_overflow_and_preserves_order() {
    for (compact, cols) in [(true, 100), (false, 100), (false, 44)] {
        let (mut state, remote_id) = state_with_remote();
        let mut remote = workspaces(15);
        remote.boot_id = "remote-boot".into();
        state.set_endpoint_snapshot(&remote_id, Box::new(remote));
        state.sidebar_collapsed = compact;
        state.collapsed_endpoints.insert(remote_id.clone());
        state.compose(cols, 18).unwrap();
        enter_navigation(&mut state);
        for number in 1..=15 {
            preview_key(&mut state, b"\x1b[B");
            let id = format!("ws_{number}");
            assert_selected(&state, &remote_id, &id);
            state.compose(cols, 18).unwrap();
            workspace_rect(&state, &remote_id, &id);
        }
        assert!(!state.collapsed_endpoints.contains(&remote_id));
        preview_key(&mut state, b"\x1b[B");
        if cols == 44 {
            assert_selected(&state, &remote_id, "ws_15");
        } else {
            assert_selected(&state, &ClientEndpointId::Local, "ws_1");
        }
        preview_key(&mut state, b"\x1b[A");
        assert_selected(
            &state,
            &remote_id,
            if cols == 44 { "ws_14" } else { "ws_15" },
        );
        state.set_endpoint_status(&remote_id, ClientEndpointStatus::Reconnecting);
        preview_key(&mut state, b"\x1b[B");
        assert_selected(&state, &ClientEndpointId::Local, "ws_1");
    }
}

fn overflowing_agent_sidebar_state() -> ClientShellState {
    let mut projected = snapshot();
    projected.agents = (1..=12)
        .map(|index| ClientShellAgent {
            pane_id: format!("pane_{index}"),
            workspace_id: "ws_1".into(),
            tab_id: "tab_1".into(),
            name: Some(format!("agent-{index}")),
            display_agent: None,
            agent: Some("pi".into()),
            title: None,
            terminal_title: None,
            terminal_title_stripped: None,
            agent_status: AgentStatus::Idle,
            state_change_seq: index,
            state_labels: Vec::new(),
            tokens: Vec::new(),
            focused: index == 1,
        })
        .collect();
    projected.panes = projected
        .agents
        .iter()
        .map(|agent| ClientShellPane {
            pane_id: agent.pane_id.clone(),
            focused: agent.focused,
            ..projected.panes[0].clone()
        })
        .collect();
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(projected));
    state.set_pane_surface(surface());
    state
}

#[test]
fn sidebar_collapse_toggle_stays_clickable_when_the_agent_list_overflows() {
    // agents 溢出时滚动条轨道曾一直画到 body 底格，正好压住右下角的 « 折叠开关；
    // 鼠标分派里 agent_scrollbar 排在 sidebar_toggle 之前且无条件 return，
    // 结果点 « 变成「列表跳到底」。轨道必须让出底格。
    let mut state = overflowing_agent_sidebar_state();
    state.compose(106, 20).expect("overflowing agent sidebar");
    let track = state.hits.agent_scrollbar;
    let toggle = state.hits.sidebar_toggle;
    assert!(track.height > 0, "agents 应当溢出并画出滚动条");
    assert!(toggle.width > 0 && toggle.height > 0, "« 开关应当可见");
    assert!(
        !crate::client::shell::contains(track, (toggle.x, toggle.y)),
        "滚动条轨道 {track:?} 不得包含折叠开关中心 {toggle:?}"
    );

    let collapsed_before = state.sidebar_collapsed;
    state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: toggle.x,
        row: toggle.y,
        modifiers: KeyModifiers::NONE,
    })]);
    assert_eq!(
        state.sidebar_collapsed, !collapsed_before,
        "点 « 应当切换侧栏折叠而不是滚动 agents"
    );
    assert_eq!(state.agent_scroll, 0, "点 « 不应滚动 agents 列表");
}

#[test]
fn terminal_theme_keeps_the_navigation_selection_visible_without_a_machine_column() {
    // theme = "terminal" 的 selection_bg 是 Color::Reset：单端点侧栏此前直接用它做
    // 选中底色，选中行与未选中行像素完全相同；回退到 active_row_bg 又会与聚焦行
    // 同色，导航光标照样看不出来（上游 #4300）。
    // 从 `theme.name = "terminal"` 走完整解析链，而不是直接塞 Palette。
    let mut config = Config::default();
    config.theme.name = Some("terminal".into());
    let terminal_palette = ClientShellConfig::from_config(&config).palette;
    assert_eq!(
        terminal_palette.selection_bg,
        ratatui::style::Color::Reset,
        "terminal 主题应当保持 selection_bg 未定义，否则这条用例失去意义"
    );
    for collapsed in [false, true] {
        let mut state = ClientShellState::new(ClientShellConfig::from_config(&config));
        state.sidebar_collapsed = collapsed;
        state.set_snapshot(Box::new(workspaces(3)));
        state.set_pane_surface(surface());
        state.compose(100, 28).unwrap();
        enter_navigation(&mut state);
        preview_key(&mut state, b"\x1b[B");
        assert_selected(&state, &ClientEndpointId::Local, "ws_2");
        let buffer = state.compose(100, 28).unwrap().to_ratatui_buffer().unwrap();
        let selected = workspace_rect(&state, &ClientEndpointId::Local, "ws_2");
        let plain = workspace_rect(&state, &ClientEndpointId::Local, "ws_3");
        // ws_1 是聚焦的 workspace：它用 active_row_bg。
        let focused = workspace_rect(&state, &ClientEndpointId::Local, "ws_1");
        let palette = &state.config.palette;
        assert_eq!(
            buffer[(selected.x, selected.y)].bg,
            palette.selection_row_bg(),
            "collapsed={collapsed}"
        );
        assert_ne!(
            buffer[(selected.x, selected.y)].bg,
            ratatui::style::Color::Reset,
            "collapsed={collapsed}"
        );
        assert_ne!(
            buffer[(selected.x, selected.y)].bg,
            buffer[(plain.x, plain.y)].bg,
            "collapsed={collapsed}: 选中行必须与未选中行可区分"
        );
        assert_eq!(
            buffer[(focused.x, focused.y)].bg,
            palette.active_row_bg,
            "collapsed={collapsed}"
        );
        assert_ne!(
            buffer[(selected.x, selected.y)].bg,
            buffer[(focused.x, focused.y)].bg,
            "collapsed={collapsed}: 选中行必须与聚焦行可区分"
        );
    }
}

fn navigator_state_with_panes(labels: &[&str]) -> ClientShellState {
    let mut projected = snapshot();
    projected.panes = labels
        .iter()
        .enumerate()
        .map(|(index, label)| ClientShellPane {
            pane_id: format!("pane_{}", index + 1),
            label: Some((*label).to_string()),
            focused: index == 0,
            ..projected.panes[0].clone()
        })
        .collect();
    projected.focused_pane_id = Some("pane_1".into());
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(projected));
    state.set_pane_surface(surface());
    state
}

fn navigator_rows_for(state: &mut ClientShellState, query: &str) -> Vec<ClientNavigatorRow> {
    let Some(ClientShellOverlay::Navigator(navigator)) = state.overlay.as_mut() else {
        panic!("expected navigator");
    };
    navigator.query = query.into();
    navigator.selected = None;
    let Some(ClientShellOverlay::Navigator(navigator)) = state.overlay.as_ref() else {
        panic!("expected navigator");
    };
    render::client_navigator_rows(&state.endpoints, &state.active_endpoint_id, navigator)
}

#[test]
fn navigator_search_matches_tokens_in_any_order() {
    // 单一连续子串匹配让「server web」打不中「web server」（上游 #4273）。
    let mut state = navigator_state_with_panes(&["web server", "database"]);
    state.open_navigator_overlay();
    for query in ["server web", "web server", "  SERVER   web "] {
        let rows = navigator_rows_for(&mut state, query);
        assert!(
            rows.iter().any(|row| row.label == "web server"),
            "query={query:?} 应当命中 web server，实得 {:?}",
            rows.iter()
                .map(|row| row.label.as_str())
                .collect::<Vec<_>>()
        );
        assert!(
            !rows.iter().any(|row| row.label == "database"),
            "query={query:?} 不应命中 database"
        );
    }
}

#[test]
fn navigator_search_defaults_the_selection_to_the_matched_pane() {
    // NAV-02（上游 #4109）：搜索后默认选中第 0 行（workspace 祖先），
    // 直接回车会聚焦 workspace 而不是唯一命中的 pane。
    let mut state = navigator_state_with_panes(&["web server", "database"]);
    state.open_navigator_overlay();
    let rows = navigator_rows_for(&mut state, "server");
    let Some(ClientShellOverlay::Navigator(navigator)) = state.overlay.as_ref() else {
        panic!("expected navigator");
    };
    let index =
        crate::client::shell::aggregate_navigation::navigator_selected_index(&rows, navigator)
            .expect("默认选中");
    assert_eq!(rows[index].label, "web server");
    assert!(matches!(
        rows[index].target,
        ClientNavigatorTarget::Pane { .. }
    ));

    // 无查询无过滤时仍然默认第 0 行，不改变既有行为。
    let rows = navigator_rows_for(&mut state, "");
    let Some(ClientShellOverlay::Navigator(navigator)) = state.overlay.as_ref() else {
        panic!("expected navigator");
    };
    assert_eq!(
        crate::client::shell::aggregate_navigation::navigator_selected_index(&rows, navigator),
        Some(0)
    );
}

/// 两个 workspace：`web-app` 自身命中 `web` 但底下没有命中的 pane，`other`
/// 自身不命中却带着命中的 `web server`。用来分辨「第一个命中行」与
/// 「第一个命中的 pane」两种默认选中规则。
fn navigator_state_with_two_workspaces() -> ClientShellState {
    let mut projected = snapshot();
    projected.workspaces[0].label = "web-app".into();
    let mut second_workspace = projected.workspaces[0].clone();
    second_workspace.workspace_id = "ws_2".into();
    second_workspace.active_tab_id = "tab_2".into();
    second_workspace.number = 2;
    second_workspace.label = "other".into();
    second_workspace.focused = false;
    projected.workspaces.push(second_workspace);

    let mut second_tab = projected.tabs[0].clone();
    second_tab.tab_id = "tab_2".into();
    second_tab.workspace_id = "ws_2".into();
    second_tab.number = 2;
    second_tab.label = "2".into();
    second_tab.focused = false;
    projected.tabs.push(second_tab);

    projected.panes[0].label = Some("database".into());
    let mut second_pane = projected.panes[0].clone();
    second_pane.pane_id = "pane_2".into();
    second_pane.workspace_id = "ws_2".into();
    second_pane.tab_id = "tab_2".into();
    second_pane.label = Some("web server".into());
    second_pane.focused = false;
    projected.panes.push(second_pane);

    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(projected));
    state.set_pane_surface(surface());
    state
}

fn navigator_rows_now(state: &ClientShellState) -> Vec<ClientNavigatorRow> {
    let Some(ClientShellOverlay::Navigator(navigator)) = state.overlay.as_ref() else {
        panic!("expected navigator");
    };
    render::client_navigator_rows(&state.endpoints, &state.active_endpoint_id, navigator)
}

fn navigator_target_now(state: &ClientShellState) -> Option<ClientNavigatorTarget> {
    let rows = navigator_rows_now(state);
    let Some(ClientShellOverlay::Navigator(navigator)) = state.overlay.as_ref() else {
        panic!("expected navigator");
    };
    crate::client::shell::aggregate_navigation::selected_navigator_target(&rows, navigator)
}

/// 走真实按键路径：prefix+g 开导航 → `/` 聚焦搜索框 → 逐字输入查询。
fn open_navigator_and_type(state: &mut ClientShellState, query: &str) {
    preview_key(state, &[0x02]);
    preview_key(state, b"g");
    assert!(matches!(
        state.overlay,
        Some(ClientShellOverlay::Navigator(_))
    ));
    preview_key(state, b"/");
    for byte in query.as_bytes() {
        preview_key(state, &[*byte]);
    }
}

/// MENU-01 / UX-04：导航浮层的 `Moved` 只写 hover。回车会真的切走
/// （跨端点时还会激活端点投影），键盘选中不能被「鼠标路过」改写。
#[test]
fn navigator_hover_does_not_move_the_keyboard_selection() {
    let mut state = navigator_state_with_two_workspaces();
    state.open_navigator_overlay();
    state.compose(106, 24).expect("navigator overlay");
    assert!(state.hits.navigator_rows.len() > 2);
    let selected_before = match state.overlay.as_ref() {
        Some(ClientShellOverlay::Navigator(navigator)) => navigator.selected.clone(),
        _ => panic!("expected navigator"),
    };
    let (row_rect, row_target) = state
        .hits
        .navigator_rows
        .iter()
        .find(|(_, target)| Some(target) != selected_before.as_ref())
        .cloned()
        .expect("有一行不是当前选中的");

    let mut outcome = ClientShellInput::default();
    state.handle_mouse(
        crossterm::event::MouseEvent {
            kind: MouseEventKind::Moved,
            column: row_rect.x + 1,
            row: row_rect.y,
            modifiers: KeyModifiers::empty(),
        },
        &mut outcome,
    );
    assert!(outcome.repaint);
    let Some(ClientShellOverlay::Navigator(navigator)) = state.overlay.as_ref() else {
        panic!("expected navigator");
    };
    assert_eq!(navigator.hovered.as_ref(), Some(&row_target));
    assert_eq!(navigator.selected, selected_before, "键盘选中不被指针改写");

    // 指针移出行区域：hover 清空，选中仍不动。
    let mut outcome = ClientShellInput::default();
    state.handle_mouse(
        crossterm::event::MouseEvent {
            kind: MouseEventKind::Moved,
            column: row_rect.x + 1,
            row: 0,
            modifiers: KeyModifiers::empty(),
        },
        &mut outcome,
    );
    assert!(outcome.repaint);
    let Some(ClientShellOverlay::Navigator(navigator)) = state.overlay.as_ref() else {
        panic!("expected navigator");
    };
    assert_eq!(navigator.hovered, None);
    assert_eq!(navigator.selected, selected_before);
}

#[test]
fn navigator_search_does_not_skip_an_earlier_matched_row() {
    // 默认选中必须是**文档序第一个自身命中的行**。偏好叶子 pane 会跳过排在
    // 前面、同样命中的 workspace 行，回车打开的是另一个 workspace 深处的 pane。
    let mut state = navigator_state_with_two_workspaces();
    state.open_navigator_overlay();
    let rows = navigator_rows_for(&mut state, "web");
    let labels = rows
        .iter()
        .map(|row| row.label.as_str())
        .collect::<Vec<_>>();
    assert_eq!(labels, ["web-app", "other", "2", "web server"]);
    assert!(rows[0].matched, "web-app 自身命中");
    assert!(!rows[1].matched, "other 只是被后代带出来");

    let Some(ClientShellOverlay::Navigator(navigator)) = state.overlay.as_ref() else {
        panic!("expected navigator");
    };
    let index =
        crate::client::shell::aggregate_navigation::navigator_selected_index(&rows, navigator)
            .expect("默认选中");
    assert_eq!(index, 0, "实得 {:?}", rows[index].label);
    assert!(matches!(
        rows[index].target,
        ClientNavigatorTarget::Workspace { .. }
    ));
}

#[test]
fn navigator_search_enter_opens_the_matched_pane_through_the_key_path() {
    // NAV-02 的用户可见行为：搜索后直接回车打开搜到的 pane，而不是只为容纳它
    // 才被带出来的 workspace 祖先。整条路径都走按键，不手工改 selected。
    let mut state = navigator_state_with_two_workspaces();
    state.compose(120, 28).expect("compose");
    open_navigator_and_type(&mut state, "web server");
    let labels = navigator_rows_now(&state)
        .iter()
        .map(|row| row.label.clone())
        .collect::<Vec<_>>();
    assert_eq!(labels, ["other", "2", "web server"]);

    let outcome = state.handle_input_bytes(b"\r");
    let [ClientShellAction::Endpoint { request, .. }] = &outcome.actions[..] else {
        panic!("回车应当走端点 API，实得 {:?}", outcome.actions.len());
    };
    assert!(
        matches!(
            &request.method,
            crate::api::schema::Method::PaneFocus(target) if target.pane_id == "pane_2"
        ),
        "回车应当聚焦命中的 pane，实得 {:?}",
        request.method
    );
}

#[test]
fn navigator_home_returns_to_the_first_row_while_a_query_is_active() {
    // Home 曾用 `selected = None` 当「回到顶部」的哨兵。`None` 的真实含义是
    // 「尚未选择」，搜索生效时会落到第一个命中行，Home 于是再也回不到首行。
    let mut state = navigator_state_with_two_workspaces();
    state.compose(120, 28).expect("compose");
    open_navigator_and_type(&mut state, "web server");
    // Esc 退出搜索框但保留查询。
    preview_key(&mut state, b"\x1b");
    let Some(ClientShellOverlay::Navigator(navigator)) = state.overlay.as_ref() else {
        panic!("expected navigator");
    };
    assert!(!navigator.search_focused);
    assert_eq!(navigator.query.as_str(), "web server");

    preview_key(&mut state, b"\x1b[B"); // ↓ 走到别处
    let rows = navigator_rows_now(&state);
    assert_ne!(navigator_target_now(&state), Some(rows[0].target.clone()));

    let outcome = state.handle_raw_events(vec![RawInputEvent::Key(TerminalKey::new(
        KeyCode::Home,
        KeyModifiers::NONE,
    ))]);
    assert!(outcome.actions.is_empty() && outcome.requests.is_empty());
    assert_eq!(
        navigator_target_now(&state),
        Some(rows[0].target.clone()),
        "Home 必须回到列表首行"
    );
    let Some(ClientShellOverlay::Navigator(navigator)) = state.overlay.as_ref() else {
        panic!("expected navigator");
    };
    assert_eq!(navigator.scroll, 0);
}

#[test]
fn navigator_search_folds_case_the_same_way_on_both_sides() {
    // 查询侧是 `str::to_lowercase`（做 Final_Sigma 等上下文映射），haystack 侧
    // 若按 `char::to_lowercase` 逐字符折叠，ΟΔΟΣ 会变成 οδοσ 而查询是 οδος，
    // 照抄标签反而搜不到。
    let mut state = navigator_state_with_panes(&["ΟΔΟΣ", "database"]);
    state.open_navigator_overlay();
    for query in ["ΟΔΟΣ", "οδος", "ΟΔΟΣ"] {
        let rows = navigator_rows_for(&mut state, query);
        assert!(
            rows.iter().any(|row| row.label == "ΟΔΟΣ"),
            "query={query:?} 应当命中 ΟΔΟΣ，实得 {:?}",
            rows.iter()
                .map(|row| row.label.as_str())
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn navigator_search_on_a_machine_name_keeps_the_machine_row_selected() {
    // 端点名进了该端点每一行的 haystack：整串查询落在机器名里时全端点行都自身
    // 命中。默认选中取文档序第一个命中行 → Machine 行，「搜机器名 + 回车 = 切到
    // 那台机器」的老行为得以保留。
    let (mut state, remote) = navigation_state(workspaces(2));
    state.open_navigator_overlay();
    let rows = navigator_rows_for(&mut state, "Build");
    assert!(
        matches!(&rows[0].target, ClientNavigatorTarget::Machine { endpoint_id } if *endpoint_id == remote),
        "首行应当是 Build 机器行，实得 {:?}",
        rows.iter()
            .map(|row| row.label.as_str())
            .collect::<Vec<_>>()
    );
    assert!(rows[0].matched);
    let Some(ClientShellOverlay::Navigator(navigator)) = state.overlay.as_ref() else {
        panic!("expected navigator");
    };
    assert_eq!(
        crate::client::shell::aggregate_navigation::navigator_selected_index(&rows, navigator),
        Some(0)
    );
}
