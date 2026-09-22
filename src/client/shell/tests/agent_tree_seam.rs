//! 接缝提交（S2）钉住的通用管线：新命中向量的悬浮解析、折叠代际、浮层种类
//! 的存储键、agent 行右键菜单与「Agent 活动」窗口的桩行为。车道填充前这些
//! 向量恒空、桩只做最小动作；这里保证管线本身已经接通。

use super::super::agent_activity_overlay::{AgentActivityButton, AgentActivityOwner};
use super::super::feedback::ChromeHover;
use super::super::state::{AgentActivityHit, ClientShellOverlayKind};
use super::*;

fn agent(pane_id: &str, activity_total: u32) -> ClientShellAgent {
    ClientShellAgent {
        pane_id: pane_id.into(),
        workspace_id: "ws_1".into(),
        tab_id: "tab_1".into(),
        name: Some(format!("agent-{pane_id}")),
        display_agent: None,
        agent: Some("pi".into()),
        title: None,
        terminal_title: None,
        terminal_title_stripped: None,
        agent_status: AgentStatus::Working,
        state_change_seq: 1,
        state_labels: Vec::new(),
        tokens: Vec::new(),
        focused: pane_id == "pane_1",
        launch_seq: 0,
        activity: crate::protocol::ClientShellAgentActivity {
            running: activity_total.min(1),
            total: activity_total,
            truncated: false,
            nodes: Vec::new(),
        },
    }
}

fn state_with_agents() -> ClientShellState {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    let mut projected = snapshot();
    projected.agents = vec![agent("pane_1", 0), agent("pane_2", 2)];
    state.set_snapshot(Box::new(projected));
    state.set_pane_surface(surface());
    state.compose(106, 24).expect("composed frame");
    state
}

fn mouse(kind: MouseEventKind, column: u16, row: u16) -> RawInputEvent {
    RawInputEvent::Mouse(MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::empty(),
    })
}

fn key(code: KeyCode) -> RawInputEvent {
    RawInputEvent::Key(crate::input::TerminalKey::new(code, KeyModifiers::empty()))
}

fn rows_key(state: &ClientShellState) -> super::super::endpoint_agents::AgentRowsKey {
    super::super::endpoint_agents::AgentRowsCache::key_for(
        &state.endpoints,
        &state.config,
        state.config_epoch,
        state.agent_rows_epoch,
        state.tree_collapse_epoch,
        state.agent_activity_epoch,
        &state.active_endpoint_id,
        None,
    )
}

/// 浮层种类的存储键稳定且互不相同：偏好文件按它记住浮窗位置，`from_storage_key`
/// 必须能把每个键还原成同一种类。
#[test]
fn overlay_kind_storage_keys_roundtrip_and_stay_unique() {
    let mut seen = HashSet::new();
    for kind in ClientShellOverlayKind::ALL {
        let key = kind.storage_key();
        assert!(seen.insert(key), "存储键重复: {key}");
        assert_eq!(ClientShellOverlayKind::from_storage_key(key), Some(kind));
        assert!(
            key.bytes()
                .all(|b| b.is_ascii_lowercase() || b == b'_' || b.is_ascii_digit()),
            "存储键只用小写字母、数字与下划线: {key}"
        );
    }
    assert_eq!(
        ClientShellOverlayKind::AgentActivity.storage_key(),
        "agent_activity"
    );
    assert_eq!(
        ClientShellOverlayKind::from_storage_key("usage_dashboard"),
        None
    );
}

/// 统一树的新命中向量各自解析成对应的 `ChromeHover`；折叠开关的矩形落在行矩形
/// 之内，解析与复核都要让开关优先于行。
#[test]
fn chrome_hover_resolves_agent_tree_hit_vectors_with_toggles_before_rows() {
    let mut state = state_with_agents();
    assert!(
        state.hits.agent_tree_toggles.is_empty() && state.hits.agent_activity_rows.is_empty(),
        "车道填充前新向量恒空"
    );
    let remote = ClientEndpointId::Ssh(
        crate::client::endpoint::ProfileId::parse("0123456789abcdef0123456789abcdef")
            .expect("profile id"),
    );
    // 在侧栏之外的空白区域摆放合成命中区，避免与真实行重叠。
    let base_y = 21;
    let row = Rect::new(2, base_y, 30, 1);
    let toggle = Rect::new(4, base_y, 1, 1);
    state.hits.agents.push((row, "pane_1".into()));
    state
        .hits
        .agent_tree_toggles
        .push((toggle, ClientEndpointId::Local, "agent-tab:tab_1".into()));
    state.hits.agent_activity_rows.push(AgentActivityHit {
        rect: Rect::new(2, base_y + 1, 30, 1),
        endpoint_id: remote.clone(),
        owner_key: "pane:pane_2".into(),
        node_id: "node-7".into(),
    });
    state.hits.external_agent_groups.push((
        Rect::new(40, base_y, 20, 1),
        ClientEndpointId::Local,
        "zcode".into(),
    ));
    state.hits.external_agents.push((
        Rect::new(40, base_y + 1, 20, 1),
        remote.clone(),
        "zcode:abc".into(),
    ));
    // 机器行与工作区行同属统一树，开关向量也必须先于它们解析（折叠键名由波 2
    // 面板车道定，这里只钉解析顺序）。
    let machine_row = Rect::new(2, base_y + 2, 30, 1);
    let machine_toggle = Rect::new(4, base_y + 2, 1, 1);
    state
        .hits
        .machines
        .push(super::super::endpoints::MachineHit {
            rect: machine_row,
            collapse_toggle: machine_toggle,
            endpoint_id: ClientEndpointId::Local,
        });
    state.hits.agent_tree_toggles.push((
        machine_toggle,
        ClientEndpointId::Local,
        "agent-machine:local".into(),
    ));
    let workspace_row = Rect::new(62, base_y + 2, 30, 1);
    let workspace_toggle = Rect::new(64, base_y + 2, 1, 1);
    state
        .hits
        .workspaces
        .push(super::super::state::WorkspaceHit {
            rect: workspace_row,
            endpoint_id: ClientEndpointId::Local,
            workspace_id: "ws_1".into(),
            indented: false,
            group_toggle: None,
        });
    state.hits.agent_tree_toggles.push((
        workspace_toggle,
        ClientEndpointId::Local,
        "agent-panel:ws_1".into(),
    ));
    state.hits.rebuild_chrome_bounds();

    let hover_at = |state: &mut ClientShellState, x: u16, y: u16| {
        state.handle_raw_events(vec![mouse(MouseEventKind::Moved, x, y)]);
        state.hover.clone()
    };
    assert_eq!(
        hover_at(&mut state, 2, base_y),
        Some(ChromeHover::AgentRow("pane_1".into()))
    );
    // 行内移到开关上：行悬浮的复核必须让位，切到开关。
    assert_eq!(
        hover_at(&mut state, 4, base_y),
        Some(ChromeHover::AgentTreeToggle(
            ClientEndpointId::Local,
            "agent-tab:tab_1".into()
        ))
    );
    // 再移回行内：开关的复核不再命中，回到行。
    assert_eq!(
        hover_at(&mut state, 10, base_y),
        Some(ChromeHover::AgentRow("pane_1".into()))
    );
    assert_eq!(
        hover_at(&mut state, 5, base_y + 1),
        Some(ChromeHover::AgentActivityRow(
            remote.clone(),
            "pane:pane_2".into(),
            "node-7".into()
        ))
    );
    assert_eq!(
        hover_at(&mut state, 45, base_y),
        Some(ChromeHover::ExternalAgentGroup(
            ClientEndpointId::Local,
            "zcode".into()
        ))
    );
    assert_eq!(
        hover_at(&mut state, 45, base_y + 1),
        Some(ChromeHover::ExternalAgentRow(remote, "zcode:abc".into()))
    );
    // 机器行：行内、开关上、再回行内。
    assert_eq!(
        hover_at(&mut state, 2, base_y + 2),
        Some(ChromeHover::MachineRow(ClientEndpointId::Local))
    );
    assert_eq!(
        hover_at(&mut state, 4, base_y + 2),
        Some(ChromeHover::AgentTreeToggle(
            ClientEndpointId::Local,
            "agent-machine:local".into()
        ))
    );
    assert_eq!(
        hover_at(&mut state, 10, base_y + 2),
        Some(ChromeHover::MachineRow(ClientEndpointId::Local))
    );
    // 工作区行：同样让位给开关。
    let workspace_hover = ChromeHover::WorkspaceRow {
        endpoint_id: ClientEndpointId::Local,
        workspace_id: "ws_1".into(),
    };
    assert_eq!(
        hover_at(&mut state, 62, base_y + 2),
        Some(workspace_hover.clone())
    );
    assert_eq!(
        hover_at(&mut state, 64, base_y + 2),
        Some(ChromeHover::AgentTreeToggle(
            ClientEndpointId::Local,
            "agent-panel:ws_1".into()
        ))
    );
    assert_eq!(hover_at(&mut state, 70, base_y + 2), Some(workspace_hover));
    assert_eq!(hover_at(&mut state, 100, base_y + 2), None);
}

/// 「Agent 活动」窗口内的命中区在有浮层时解析：树行与按钮各自命中（二者在
/// 窗口布局里不重叠）。
#[test]
fn chrome_hover_resolves_agent_activity_window_hits_when_the_overlay_is_open() {
    let mut state = state_with_agents();
    state.open_agent_activity(
        ClientEndpointId::Local,
        AgentActivityOwner::Pane {
            pane_id: "pane_2".into(),
        },
    );
    state.compose(106, 24).expect("activity overlay frame");
    let popup = state.hits.agent_activity_popup;
    assert!(!popup.is_empty(), "渲染桩登记了窗口矩形");
    let tree_row = Rect::new(popup.x + 1, popup.y + 2, 10, 1);
    let button = Rect::new(popup.x + 1, popup.y + 1, 2, 1);
    state
        .hits
        .agent_activity_tree_rows
        .push((tree_row, "node-1".into()));
    state
        .hits
        .agent_activity_actions
        .push((button, AgentActivityButton::Refresh));

    state.handle_raw_events(vec![mouse(MouseEventKind::Moved, popup.x + 6, popup.y + 2)]);
    assert_eq!(
        state.hover,
        Some(ChromeHover::AgentActivityNode("node-1".into()))
    );
    state.handle_raw_events(vec![mouse(MouseEventKind::Moved, popup.x + 1, popup.y + 1)]);
    assert_eq!(
        state.hover,
        Some(ChromeHover::AgentActivityButton(
            AgentActivityButton::Refresh
        ))
    );
}

/// 折叠 / 展开集合的每个写入点都递增 `tree_collapse_epoch`，agents 行缓存的键
/// 随之变化（`apply_scene_sidebar` 是私有路径，由
/// `test_ui_hot_path_architecture::test_collapse_state_writes_bump_tree_epoch`
/// 静态守门覆盖）。
#[test]
fn collapse_state_writes_change_the_agent_rows_cache_key() {
    let mut state = state_with_agents();
    let mut previous = rows_key(&state);
    let mut expect_changed = |state: &ClientShellState, what: &str| {
        let next = rows_key(state);
        assert_ne!(next, previous, "{what} 之后行缓存键应变化");
        previous = next;
    };

    state.toggle_collapsed_group(&ClientEndpointId::Local, "agent-panel:ws_1".into());
    expect_changed(&state, "toggle_collapsed_group");

    state.set_endpoint_catalog(&[]);
    expect_changed(&state, "set_endpoint_catalog");

    // 机器行的折叠开关：合成一条本机机器命中区。
    let toggle = Rect::new(1, 22, 1, 1);
    state
        .hits
        .machines
        .push(super::super::endpoints::MachineHit {
            rect: Rect::new(0, 22, 20, 1),
            collapse_toggle: toggle,
            endpoint_id: ClientEndpointId::Local,
        });
    let mut outcome = ClientShellInput::default();
    assert!(state.handle_endpoint_machine_click((toggle.x, toggle.y), &mut outcome));
    assert!(state.collapsed_endpoints.contains(&ClientEndpointId::Local));
    expect_changed(&state, "handle_endpoint_machine_click");

    // 工作区导航会展开目标端点：从折叠态出发才算写入。
    state.move_navigate_workspace(1);
    assert!(!state.collapsed_endpoints.contains(&ClientEndpointId::Local));
    expect_changed(&state, "move_navigate_workspace");

    // 未折叠时导航不改集合，也不动代际。
    state.move_navigate_workspace(1);
    assert_eq!(rows_key(&state), previous);
}

/// agent 行右键：条目由接缝定稿；没有活动时「查看 Agent 活动」禁用且不可激活；
/// 「聚焦」对当前端点发 `pane.focus`。
#[test]
fn agent_row_context_menu_lists_seam_items_and_routes_stub_actions() {
    let mut state = state_with_agents();
    let (row, pane_id) = state.hits.agents[0].clone();
    assert_eq!(pane_id, "pane_1");
    state.handle_raw_events(vec![mouse(
        MouseEventKind::Down(MouseButton::Right),
        row.x + 1,
        row.y,
    )]);
    let items = match state.overlay.as_ref() {
        Some(ClientShellOverlay::ContextMenu(menu)) => {
            assert!(matches!(
                menu.target,
                ClientContextMenuTarget::Agent {
                    endpoint_id: ClientEndpointId::Local,
                    ref pane_id,
                    has_activity: false,
                    agent: Some(_),
                    ..
                } if pane_id == "pane_1"
            ));
            menu.items()
        }
        other => panic!("agent 行右键应打开菜单: {other:?}"),
    };
    let actions = items.iter().map(|item| item.action).collect::<Vec<_>>();
    assert_eq!(
        actions,
        [
            ClientContextMenuAction::FocusAgent,
            ClientContextMenuAction::ViewAgentActivity,
            ClientContextMenuAction::RenameAgent,
            ClientContextMenuAction::ShowAgentUsage,
            ClientContextMenuAction::BindAgentAccount,
            ClientContextMenuAction::CloseAgentPane,
        ]
    );
    let view_activity = items
        .iter()
        .position(|item| item.action == ClientContextMenuAction::ViewAgentActivity)
        .expect("查看活动项");
    assert!(!items[view_activity].enabled, "没有活动时禁用");
    // 动作未接通的条目灰显：否则点了只会关掉菜单、什么都不发生。
    for action in [
        ClientContextMenuAction::RenameAgent,
        ClientContextMenuAction::ShowAgentUsage,
        ClientContextMenuAction::BindAgentAccount,
        ClientContextMenuAction::CloseAgentPane,
    ] {
        let index = items
            .iter()
            .position(|item| item.action == action)
            .unwrap_or_else(|| panic!("缺少条目: {action:?}"));
        assert!(!items[index].enabled, "{action:?} 未接通时应禁用");
    }

    // 禁用项：点击不激活，菜单保持打开。
    state.compose(106, 24).expect("context menu frame");
    let disabled_row = state.hits.context_menu_rows[view_activity].0;
    state.handle_raw_events(vec![mouse(
        MouseEventKind::Down(MouseButton::Left),
        disabled_row.x + 1,
        disabled_row.y,
    )]);
    assert!(matches!(
        state.overlay,
        Some(ClientShellOverlay::ContextMenu(_))
    ));

    // 「聚焦」：当前端点直接发 pane.focus。
    let focus_row = state.hits.context_menu_rows[0].0;
    let outcome = state.handle_raw_events(vec![mouse(
        MouseEventKind::Down(MouseButton::Left),
        focus_row.x + 1,
        focus_row.y,
    )]);
    assert!(state.overlay.is_none());
    let [ClientShellAction::Endpoint { request, .. }] = &outcome.actions[..] else {
        panic!("聚焦应走端点 API: {:?}", outcome.actions);
    };
    assert!(matches!(
        &request.method,
        crate::api::schema::Method::PaneFocus(target) if target.pane_id == "pane_1"
    ));

    // 未识别 agent 的行：没有用量 / 绑定账号两项。
    let mut projected = snapshot();
    let mut anonymous = agent("pane_1", 0);
    anonymous.agent = None;
    projected.agents = vec![anonymous];
    state.set_snapshot(Box::new(projected));
    state.compose(106, 24).expect("anonymous agent frame");
    let (row, _) = state.hits.agents[0].clone();
    state.handle_raw_events(vec![mouse(
        MouseEventKind::Down(MouseButton::Right),
        row.x + 1,
        row.y,
    )]);
    let actions = match state.overlay.as_ref() {
        Some(ClientShellOverlay::ContextMenu(menu)) => menu
            .items()
            .iter()
            .map(|item| item.action)
            .collect::<Vec<_>>(),
        other => panic!("agent 行右键应打开菜单: {other:?}"),
    };
    assert!(!actions.contains(&ClientContextMenuAction::ShowAgentUsage));
    assert!(!actions.contains(&ClientContextMenuAction::BindAgentAccount));
}

/// 有活动的 agent：「查看 Agent 活动」打开只读窗口；Esc 关闭、点窗外关闭，
/// 过期代际的读取响应被丢弃。
#[test]
fn agent_activity_window_opens_from_the_menu_and_closes_on_esc_or_outside_click() {
    let mut state = state_with_agents();
    let (row, pane_id) = state.hits.agents[1].clone();
    assert_eq!(pane_id, "pane_2");
    state.handle_raw_events(vec![mouse(
        MouseEventKind::Down(MouseButton::Right),
        row.x + 1,
        row.y,
    )]);
    state.compose(106, 24).expect("context menu frame");
    let (items, view_activity) = match state.overlay.as_ref() {
        Some(ClientShellOverlay::ContextMenu(menu)) => {
            let items = menu.items();
            let index = items
                .iter()
                .position(|item| item.action == ClientContextMenuAction::ViewAgentActivity)
                .expect("查看活动项");
            (items, index)
        }
        other => panic!("agent 行右键应打开菜单: {other:?}"),
    };
    assert!(items[view_activity].enabled, "有活动时可用");
    let activity_row = state.hits.context_menu_rows[view_activity].0;
    state.handle_raw_events(vec![mouse(
        MouseEventKind::Down(MouseButton::Left),
        activity_row.x + 1,
        activity_row.y,
    )]);
    match state.overlay.as_ref() {
        Some(ClientShellOverlay::AgentActivity(overlay)) => {
            assert_eq!(overlay.endpoint_id, ClientEndpointId::Local);
            assert_eq!(
                overlay.owner,
                AgentActivityOwner::Pane {
                    pane_id: "pane_2".into()
                }
            );
            assert!(overlay.return_to.is_none(), "从右键菜单打开不记返回目标");
        }
        other => panic!("应打开 Agent 活动窗口: {other:?}"),
    }
    let frame = state.compose(106, 24).expect("activity overlay frame");
    let text = frame
        .cells
        .iter()
        .map(|cell| cell.symbol.as_str())
        .collect::<String>();
    assert!(text.contains("agent-pane_2"), "标题带属主名: {text}");
    let popup = state.hits.agent_activity_popup;
    assert!(!popup.is_empty());

    // 窗内点击被吞掉，窗外点击关闭。
    state.handle_raw_events(vec![mouse(
        MouseEventKind::Down(MouseButton::Left),
        popup.x + 1,
        popup.y + 1,
    )]);
    assert!(matches!(
        state.overlay,
        Some(ClientShellOverlay::AgentActivity(_))
    ));
    let outside = if popup.x > 0 {
        (0, 0)
    } else {
        (popup.right(), 0)
    };
    assert!(!super::super::contains(popup, outside));
    state.handle_raw_events(vec![mouse(
        MouseEventKind::Down(MouseButton::Left),
        outside.0,
        outside.1,
    )]);
    assert!(state.overlay.is_none(), "点窗外关闭");

    // 从别的浮层进入时 Esc 回到它。
    state.overlay = Some(ClientShellOverlay::Help(ClientHelpOverlay {
        query: TextEditor::default(),
        search_focused: false,
        max_scroll: 0,
        scroll: 0,
    }));
    state.open_agent_activity(
        ClientEndpointId::Local,
        AgentActivityOwner::External {
            external_id: "zcode:abc".into(),
        },
    );
    state.handle_raw_events(vec![key(KeyCode::Char('x'))]);
    assert!(
        matches!(state.overlay, Some(ClientShellOverlay::AgentActivity(_))),
        "其余按键被吞掉"
    );
    state.handle_raw_events(vec![key(KeyCode::Esc)]);
    assert!(matches!(state.overlay, Some(ClientShellOverlay::Help(_))));
    state.overlay = None;

    // 读取响应：代际对不上丢弃，对上才写入。
    state.open_agent_activity(
        ClientEndpointId::Local,
        AgentActivityOwner::Pane {
            pane_id: "pane_2".into(),
        },
    );
    let content = crate::api::schema::AgentActivityContent {
        node_id: "n1".into(),
        text: "hello".into(),
        ..Default::default()
    };
    let response = |content: &crate::api::schema::AgentActivityContent| {
        Ok(crate::api::schema::ResponseResult::AgentActivity {
            nodes: Vec::new(),
            content: Some(content.clone()),
        })
    };
    let (repaint, actions) = state.receive_agent_activity_read(99, None, response(&content));
    assert!(!repaint && actions.is_empty(), "过期代际被丢弃");
    let (repaint, _) = state.receive_agent_activity_read(0, None, response(&content));
    assert!(repaint);
    match state.overlay.as_ref() {
        Some(ClientShellOverlay::AgentActivity(overlay)) => {
            assert_eq!(overlay.content.as_ref(), Some(&content));
            assert!(overlay.error.is_none());
        }
        other => panic!("窗口仍应打开: {other:?}"),
    }
    let (repaint, _) = state.receive_agent_activity_read(
        0,
        None,
        Err(ClientShellEndpointError {
            code: Some("not_implemented".into()),
            message: "no activity".into(),
        }),
    );
    assert!(repaint);
    match state.overlay.as_ref() {
        Some(ClientShellOverlay::AgentActivity(overlay)) => {
            assert_eq!(overlay.error.as_deref(), Some("no activity"));
        }
        other => panic!("窗口仍应打开: {other:?}"),
    }
}
