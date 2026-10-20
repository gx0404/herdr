use super::*;
use crate::api::schema::AgentStatus;
use crate::client::shell::dock::{DockLayout, InspectorVisibility, PanelId};

fn snapshot() -> ClientShellSnapshot {
    let mut snapshot = crate::client::shell::tests::snapshot();
    snapshot.agents.push(ClientShellAgent {
        pane_id: "pane_1".into(),
        workspace_id: "ws_1".into(),
        tab_id: "tab_1".into(),
        name: Some("claude".into()),
        display_agent: Some("claude".into()),
        agent: Some("claude".into()),
        title: Some("修复焦点与身份隔离".into()),
        terminal_title: None,
        terminal_title_stripped: None,
        agent_status: AgentStatus::Working,
        state_change_seq: 1,
        state_labels: Vec::new(),
        tokens: Vec::new(),
        focused: true,
        launch_seq: 1,
        activity: Default::default(),
        insight: crate::protocol::ClientShellAgentInsight {
            session_title: Some("修复焦点与身份隔离".into()),
            model: Some("Sol".into()),
            current_tool: Some("Read src/client/shell.rs".into()),
            ..Default::default()
        },
    });
    snapshot
}

fn ready(cols: u16) -> ClientShellState {
    let mut config = ClientShellConfig::from_config(&Config::default());
    config
        .preferences
        .inspector_visibility
        .insert("local".into(), InspectorVisibility::Auto);
    ready_with_config(cols, config)
}

fn ready_with_config(cols: u16, config: ClientShellConfig) -> ClientShellState {
    let mut state = ClientShellState::new(config);
    state.set_snapshot(Box::new(snapshot()));
    state.set_endpoint_methods(Some(vec![
        "client.views.set".into(),
        "agent.activity.read".into(),
        "pane.focus".into(),
        "tab.focus".into(),
    ]));
    state.compose(cols, 45).expect("classic");
    state.tick_workbench(Instant::now(), &mut ClientShellInput::default());
    state.compose(cols, 45).expect("workbench");
    state
}

fn read(state: &mut ClientShellState) -> (String, String) {
    let mut outcome = ClientShellInput::default();
    state.tick_inspector(Instant::now() + Duration::from_secs(10), &mut outcome);
    outcome
        .actions
        .into_iter()
        .find_map(|action| {
            let ClientShellAction::EndpointRequest {
                boot_id, request, ..
            } = action
            else {
                return None;
            };
            matches!(request.method, Method::AgentActivityRead(_)).then_some((boot_id, request.id))
        })
        .expect("tree read")
}

fn response() -> ResponseResult {
    ResponseResult::AgentActivity {
        nodes: vec![AgentActivityNode {
            id: "full-tree-node".into(),
            label: "Only in the full tree".into(),
            status: AgentActivityStatus::Done,
            ..Default::default()
        }],
        content: None,
    }
}

#[test]
fn inspector_defaults_to_hidden_without_persisting_a_preference_or_reading_activity() {
    let mut state = ready_with_config(200, ClientShellConfig::from_config(&Config::default()));
    state.compose(200, 50).expect("default wide workbench");
    assert!(!state
        .workbench
        .geometry
        .panels
        .iter()
        .any(|(id, _)| *id == PanelId::Agents));
    assert!(state.hits.inspector_body.is_empty());
    let mut outcome = ClientShellInput::default();
    state.tick_inspector(Instant::now(), &mut outcome);
    assert!(outcome.actions.is_empty());
    assert!(state.inspector.target.is_none());
    assert!(state.workbench.inspector_visibility.is_empty());
    let preferences = preferences::ClientChromePreferences {
        inspector_visibility: state.workbench.inspector_visibility.clone(),
        ..Default::default()
    };
    let saved = serde_json::to_value(preferences).expect("serialize preferences");
    assert!(saved.get("inspector_visibility").is_none());

    state.compose(200, 6).expect("compact default workbench");
    assert!(state.workbench.geometry.compact);
    assert!(state.workbench.hits.iter().all(|(_, action)| !matches!(
        action,
        workbench::interaction::Action::Switch(PanelId::Agents)
    )));
}

#[test]
fn prefix_i_shows_and_hides_the_default_inspector_with_an_endpoint_preference() {
    let mut state = ready_with_config(200, ClientShellConfig::from_config(&Config::default()));
    assert!(state.handle_input_bytes(b"\x02i").resize);
    state.compose(200, 50).expect("manually shown inspector");
    assert!(state
        .workbench
        .geometry
        .panels
        .iter()
        .any(|(id, _)| *id == PanelId::Agents));
    assert!(!state.hits.inspector_body.is_empty());
    assert_eq!(
        state.workbench.inspector_visibility["local"],
        InspectorVisibility::Shown
    );

    assert!(state.handle_input_bytes(b"\x02i").resize);
    state.compose(200, 50).expect("manually hidden inspector");
    assert!(!state
        .workbench
        .geometry
        .panels
        .iter()
        .any(|(id, _)| *id == PanelId::Agents));
    assert_eq!(
        state.workbench.inspector_visibility["local"],
        InspectorVisibility::Hidden
    );
}

#[test]
fn not_implemented_activity_is_a_neutral_terminal_state_without_follow_or_refresh() {
    let mut state = ready(200);
    let (boot, request) = read(&mut state);
    let error_message = "agent activity is not implemented by this server";
    assert!(
        state
            .handle_endpoint_result(
                &boot,
                &request,
                Err(ClientShellEndpointError {
                    code: Some("not_implemented".into()),
                    message: error_message.into(),
                })
            )
            .0
    );
    state.compose(200, 50).expect("activity not reported");
    let texts = crate::i18n::texts();
    let rows = state
        .inspector
        .rows
        .iter()
        .map(|row| row.text.as_str())
        .collect::<String>();
    assert!(rows.contains(texts.agent_activity.not_reported), "{rows}");
    assert!(!rows.contains(error_message));
    assert!(!rows.contains(texts.agent_activity.loading));
    assert!(!state
        .inspector
        .rows
        .iter()
        .any(|row| matches!(row.tone, Tone::Failed)));
    let tree = &state.inspector.trees[state.inspector.target.as_ref().expect("target")];
    assert!(tree.not_reported && tree.unsupported && tree.loaded);
    assert!(tree.error.is_none());
    let mut outcome = ClientShellInput::default();
    state.tick_inspector(Instant::now() + Duration::from_secs(120), &mut outcome);
    state.activate_inspector(Action::Refresh, &mut outcome);
    assert!(outcome.actions.is_empty());
    assert!(state.inspector.in_flight.is_none());
}

#[test]
fn inspector_reads_full_tree_single_flight_and_rejects_target_race() {
    let mut state = ready(160);
    let (boot, request) = read(&mut state);
    let mut repeated = ClientShellInput::default();
    state.tick_inspector(Instant::now() + Duration::from_secs(30), &mut repeated);
    assert!(repeated.actions.is_empty());
    let mut next = snapshot();
    next.revision += 1;
    next.agents[0].pane_id = "pane_2".into();
    next.panes[0].pane_id = "pane_2".into();
    next.focused_pane_id = Some("pane_2".into());
    state.set_snapshot(Box::new(next));
    state.compose(160, 45).expect("switch target");
    assert!(
        !state
            .handle_endpoint_result(&boot, &request, Ok(response()))
            .0
    );
    let (boot, request) = read(&mut state);
    assert!(
        state
            .handle_endpoint_result(&boot, &request, Ok(response()))
            .0
    );
    state.compose(160, 45).expect("tree");
    assert!(state
        .inspector
        .rows
        .iter()
        .any(|row| row.text.contains("Only in the full tree")));
}

#[test]
fn inspector_rejects_old_boot_and_launch_identity() {
    for changed in ["boot", "launch"] {
        let mut state = ready(160);
        let (boot, request) = read(&mut state);
        let mut next = snapshot();
        next.revision += 1;
        if changed == "boot" {
            next.boot_id = "boot-2".into();
        } else {
            next.agents[0].launch_seq += 1;
        }
        state.set_snapshot(Box::new(next));
        state.compose(160, 45).expect("new identity");
        assert!(
            !state
                .handle_endpoint_result(&boot, &request, Ok(response()))
                .0
        );
        assert!(state
            .inspector
            .trees
            .values()
            .all(|tree| tree.nodes.is_empty()));
    }
}

#[test]
fn inspector_hidden_cancels_locally_and_stops_follow_without_changing_tree() {
    let mut state = ready(160);
    let (boot, request) = read(&mut state);
    let tree = state.workbench.dock.clone();
    state.toggle_inspector(&mut ClientShellInput::default());
    assert!(state.inspector.target.is_none());
    assert!(!state.pending_requests.contains_key(&request));
    assert!(
        !state
            .handle_endpoint_result(&boot, &request, Ok(response()))
            .0
    );
    let mut outcome = ClientShellInput::default();
    state.tick_inspector(Instant::now() + Duration::from_secs(120), &mut outcome);
    assert!(outcome.actions.is_empty());
    state.compose(160, 45).expect("hidden");
    assert!(state.hits.inspector_body.is_empty());
    assert_eq!(state.workbench.dock.root, tree.root);
}

#[test]
fn old_server_without_insight_or_read_stays_connected_and_plain_terminal_has_empty_state() {
    let mut state = ready(160);
    state.set_endpoint_methods(Some(vec!["client.views.set".into()]));
    state.tick_inspector(Instant::now(), &mut ClientShellInput::default());
    state.compose(160, 45).expect("old server");
    assert!(state.workbench.enabled && state.inspector.trees.values().all(|tree| tree.unsupported));
    let mut plain = snapshot();
    plain.revision += 1;
    plain.agents.clear();
    state.set_snapshot(Box::new(plain));
    state.compose(160, 45).expect("plain terminal");
    assert!(state.inspector.target.is_none());
    assert!(state
        .inspector
        .rows
        .iter()
        .any(|row| row.text.contains("Terminal") || row.text.contains("终端")));
    assert!(state.inspector.timelines.is_empty());
}

#[test]
fn timeline_has_eight_edges_no_duplicates_and_boot_pane_endpoint_isolation() {
    let mut inspector = State::default();
    let mut snapshot = snapshot();
    for i in 0..20 {
        snapshot.agents[0].agent_status = if i % 2 == 0 {
            AgentStatus::Working
        } else {
            AgentStatus::Blocked
        };
        inspector.observe_snapshot(&ClientEndpointId::Local, &snapshot);
        inspector.observe_snapshot(&ClientEndpointId::Local, &snapshot);
    }
    let target = Target::for_agent(ClientEndpointId::Local, &snapshot, &snapshot.agents[0]);
    assert_eq!(inspector.timelines[&target].len(), 8);
    snapshot.boot_id = "new-boot".into();
    inspector.observe_snapshot(&ClientEndpointId::Local, &snapshot);
    assert!(!inspector.timelines.contains_key(&target));
    assert_eq!(inspector.timelines.len(), 1);
    snapshot.agents.clear();
    inspector.observe_snapshot(&ClientEndpointId::Local, &snapshot);
    assert!(inspector.timelines.is_empty());
}

#[test]
fn timeline_and_tree_cache_are_bounded() {
    let mut state = State::default();
    let mut snapshot = snapshot();
    for index in 0..200 {
        let mut agent = snapshot.agents[0].clone();
        agent.pane_id = format!("pane-{index}");
        snapshot.agents.push(agent);
    }
    state.observe_snapshot(&ClientEndpointId::Local, &snapshot);
    assert_eq!(state.timelines.len(), TIMELINE_AGENTS);
    for agent in &snapshot.agents {
        let target = Target::for_agent(ClientEndpointId::Local, &snapshot, agent);
        state.switch_target(Some(target.clone()));
        state.apply_tree(
            &target,
            (0..2000)
                .map(|index| AgentActivityNode {
                    id: index.to_string(),
                    label: "字".repeat(1000),
                    summary: Some("a".repeat(2000)),
                    ..Default::default()
                })
                .collect(),
        );
    }
    assert_eq!(state.trees.len(), TREE_CACHE);
    assert!(state
        .trees
        .values()
        .all(|tree| tree.nodes.len() <= TREE_NODES && tree.truncated));
}

#[test]
fn prefix_i_and_endpoint_preference_use_one_geometry_for_request_and_paint() {
    let mut state = ready(120);
    assert!(!state
        .workbench
        .geometry
        .panels
        .iter()
        .any(|(id, _)| *id == PanelId::Agents));
    let saved = state.workbench.dock.clone();
    let outcome = state.handle_input_bytes(b"\x02i");
    assert!(outcome.requests.is_empty() && outcome.resize);
    state.workbench.pending = false;
    state.tick_workbench(
        Instant::now() + Duration::from_secs(1),
        &mut ClientShellInput::default(),
    );
    state.compose(120, 45).expect("shown narrow");
    assert!(!state.hits.inspector_body.is_empty());
    for spec in &state.workbench.requested {
        let (panel, area) = state.workbench.geometry.panels.iter().find(|(panel, _)| matches!(panel, PanelId::Terminal(id) if id.to_string() == spec.view_id)).expect("terminal");
        let body = workbench::body(*area, panel, state.workbench.geometry.compact);
        assert_eq!((spec.cols, spec.rows), (body.width, body.height));
    }
    assert_eq!(state.workbench.dock.root, saved.root);
    assert_eq!(
        state.workbench.inspector_visibility["local"],
        InspectorVisibility::Shown
    );
    let preferences = preferences::ClientChromePreferences {
        inspector_visibility: state.workbench.inspector_visibility.clone(),
        ..Default::default()
    };
    let config = ClientShellConfig {
        preferences,
        ..ClientShellConfig::from_config(&Config::default())
    };
    let restored = workbench::State::new(&config);
    assert_eq!(
        restored.inspector_visibility["local"],
        InspectorVisibility::Shown
    );
}

#[test]
fn needs_input_jump_mouse_has_real_pane_focus_no_answer_and_hover_bounds() {
    let mut state = ready(160);
    let mut next = snapshot();
    next.revision += 1;
    next.agents[0].agent_status = AgentStatus::Blocked;
    next.agents[0].insight.question = Some("允许修改 src/client/shell.rs 吗？".into());
    next.agents[0].insight.options = vec!["允许一次".into(), "拒绝".into()];
    state.set_snapshot(Box::new(next));
    state.compose(160, 45).expect("question");
    let rect = state
        .hits
        .inspector_actions
        .iter()
        .find(|(_, action)| *action == Action::Jump)
        .expect("jump hit")
        .0;
    assert!(state.hits.chrome_bounds.contains((rect.x, rect.y)));
    let mut outcome = ClientShellInput::default();
    assert!(state.workbench_mouse(
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: rect.x,
            row: rect.y,
            modifiers: crossterm::event::KeyModifiers::NONE
        },
        &mut outcome
    ));
    assert!(outcome.actions.iter().any(|action| matches!(action, ClientShellAction::Endpoint { request, .. } if matches!(&request.method, Method::PaneFocus(target) if target.pane_id == "pane_1"))));
    assert!(outcome.requests.is_empty());
    assert_eq!(state.workbench.dock.focused, PanelId::Terminal(1));
}

#[test]
fn acknowledgement_without_a_revision_change_refreshes_inspector_and_timeline() {
    let mut state = ready(160);
    let key = state.inspector.key.clone();
    let mut acknowledged = state.snapshot.as_deref().cloned().expect("snapshot");
    acknowledged.agents[0].agent_status = AgentStatus::Idle;
    state.store_acknowledged_snapshot(Box::new(acknowledged));
    state.compute_inspector_view(state.inspector.area);
    assert_ne!(state.inspector.key, key);
    assert!(state
        .inspector
        .rows
        .iter()
        .any(|row| row.bucket == Some(AgentBucket::Idle)));
    let target = state.inspector.target.as_ref().expect("target");
    assert_eq!(state.inspector.timelines[target].len(), 2);
    assert!(!state.inspector.animated());
}

#[test]
fn identical_full_tree_follow_response_does_not_repaint() {
    let mut state = ready(160);
    let (boot, request) = read(&mut state);
    assert!(
        state
            .handle_endpoint_result(&boot, &request, Ok(response()))
            .0
    );
    let version = state.inspector.version;
    let (boot, request) = read(&mut state);
    assert!(
        !state
            .handle_endpoint_result(&boot, &request, Ok(response()))
            .0
    );
    assert_eq!(state.inspector.version, version);
}

#[test]
fn closing_inspector_retires_the_pending_read_immediately() {
    let mut state = ready(160);
    let (boot, request) = read(&mut state);
    assert!(state.close_workbench_panel(PanelId::Agents, &mut ClientShellInput::default()));
    assert!(state.inspector.target.is_none());
    assert!(
        !state
            .handle_endpoint_result(&boot, &request, Ok(response()))
            .0
    );
    assert!(state.inspector.in_flight.is_none());
}

#[test]
fn preview_target_isolates_endpoints_even_with_the_same_boot_pane_and_launch() {
    let mut state = ready(160);
    let (boot, old) = read(&mut state);
    let remote = ClientEndpointId::Ssh(
        crate::client::endpoint::ProfileId::parse("22222222222222222222222222222222")
            .expect("profile"),
    );
    let mut endpoint = state.endpoints[0].clone();
    endpoint.endpoint_id = remote.clone();
    state.endpoints.push(endpoint);
    state.set_endpoint_snapshot(&remote, Box::new(snapshot()));
    let snapshot = state
        .endpoints
        .iter()
        .find(|endpoint| endpoint.endpoint_id == remote)
        .and_then(|endpoint| endpoint.snapshot.as_deref())
        .expect("remote snapshot");
    let target = Target::for_agent(remote.clone(), snapshot, &snapshot.agents[0]);
    state.set_inspector_preview_target(Some(target.clone()));
    state.compose(160, 45).expect("preview");
    assert!(!state.handle_endpoint_result(&boot, &old, Ok(response())).0);
    let (boot, current) = read(&mut state);
    assert_eq!(state.pending_requests[&current].endpoint_id, remote);
    assert!(
        state
            .handle_endpoint_result(&boot, &current, Ok(response()))
            .0
    );
    assert_eq!(state.inspector.target.as_ref(), Some(&target));
    assert_eq!(state.inspector.timelines.len(), 2);
}

#[test]
fn compact_switcher_does_not_offer_an_automatically_hidden_inspector() {
    let mut state = ready(120);
    state.compose(120, 6).expect("compact");
    assert!(state.workbench.geometry.compact);
    assert!(state.workbench.hits.iter().all(|(_, action)| !matches!(
        action,
        workbench::interaction::Action::Switch(PanelId::Agents)
    )));
    state.toggle_inspector(&mut ClientShellInput::default());
    state.compose(120, 6).expect("explicitly shown");
    assert!(state.workbench.hits.iter().any(|(_, action)| matches!(
        action,
        workbench::interaction::Action::Switch(PanelId::Agents)
    )));
}

#[test]
fn arrange_mode_cycles_only_the_current_projection_without_destroying_hidden_panels() {
    let mut state = ready(120);
    let root = state.workbench.dock.root.clone();
    state.workbench.arranging = true;
    state.workbench.dock.focused = PanelId::Agents;
    for _ in 0..4 {
        state.workbench_key(
            &crate::input::TerminalKey::new(KeyCode::Tab, crossterm::event::KeyModifiers::NONE),
            &mut ClientShellInput::default(),
        );
        assert_ne!(state.workbench.dock.focused, PanelId::Agents);
    }
    assert_eq!(state.workbench.dock.root, root);
}

#[test]
fn cjk_wrapping_scroll_keys_and_mouse_are_bounded_and_cached() {
    let mut state = ready(160);
    state.workbench.dock.focused = PanelId::Agents;
    state.compose(160, 15).expect("short");
    let key = state.inspector.key.clone();
    state.compute_inspector_view(state.inspector.area);
    assert_eq!(state.inspector.key, key);
    let mut outcome = ClientShellInput::default();
    assert!(state.inspector_key(
        &crate::input::TerminalKey::new(KeyCode::End, crossterm::event::KeyModifiers::NONE),
        &mut outcome
    ));
    assert_eq!(state.inspector.scroll, state.inspector.max_scroll);
    for width in 1..25 {
        state.compute_inspector_view(Rect::new(0, 0, width, 8));
        assert!(state
            .inspector
            .rows
            .iter()
            .all(|row| crate::ui::display_width(&row.text)
                <= usize::from(width.saturating_sub(3).max(1))));
    }
}

#[test]
fn v1_layout_toast_only_once_and_migrated_layout_is_saved() {
    let mut config = ClientShellConfig::from_config(&Config::default());
    config.preferences.layouts.insert(
        "local".into(),
        DockLayout {
            version: 1,
            locked: true,
            ..Default::default()
        },
    );
    let mut state = ClientShellState::new(config);
    state.set_snapshot(Box::new(snapshot()));
    state.set_endpoint_methods(Some(vec!["client.views.set".into()]));
    state.compose(160, 45).expect("initial");
    let mut outcome = ClientShellInput::default();
    state.tick_workbench(Instant::now(), &mut outcome);
    assert_eq!(state.workbench.dock.version, 2);
    assert!(state.workbench.dock.locked);
    assert!(state.workbench.migrated.is_empty());
    let first = state
        .visible_endpoint_notice
        .as_ref()
        .map(|notice| notice.deadline);
    state.tick_workbench(Instant::now() + Duration::from_secs(1), &mut outcome);
    assert_eq!(
        state
            .visible_endpoint_notice
            .as_ref()
            .map(|notice| notice.deadline),
        first
    );
    assert_eq!(state.workbench.saved_layouts()["local"].version, 2);
    let saved = serde_json::to_string(&preferences::ClientChromePreferences {
        layouts: state.workbench.saved_layouts(),
        ..Default::default()
    })
    .expect("serialized");
    let config = ClientShellConfig {
        preferences: serde_json::from_str(&saved).expect("preferences"),
        ..ClientShellConfig::from_config(&Config::default())
    };
    assert!(workbench::State::new(&config).migrated.is_empty());
}
