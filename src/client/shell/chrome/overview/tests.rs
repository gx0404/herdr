use super::*;
use crate::api::schema::{AgentAttentionKind, AgentStatus};
mod behaviour;

#[test]
fn location_detail_geometry_scales_with_visible_panes() {
    let area = Rect::new(0, 0, 160, 45);
    for panes in [1, 15] {
        let mut buffer = Buffer::empty(area);
        for y in 0..panes {
            buffer.set_string(0, y, " ├─ ◆ ● agent-name", Style::default());
        }
        let start = std::time::Instant::now();
        for _ in 0..1000 {
            for y in 0..panes {
                let rect = location_detail_area(&buffer, Rect::new(0, y, 160, 1), "agent-name", 24)
                    .expect("location area");
                assert_eq!(rect.x, 20);
                std::hint::black_box(rect);
            }
        }
        println!(
            "location detail geometry: {panes} panes, 1000 frames, {:?}",
            start.elapsed()
        );
    }
}

#[test]
fn location_details_follow_painted_labels_without_overwriting_names() {
    for width in [28, 48, 80, 160] {
        for prefix in [" └─ ○ ", " ├─ ◆ ● ", "     └─ ✕ "] {
            for label in [
                "codex",
                "kimi",
                "长名称 e\u{301} 👩‍💻 工作会话",
                "a very long agent name that fills the available navigator row",
            ] {
                let area = Rect::new(0, 0, width, 1);
                let mut buffer = Buffer::empty(area);
                let reserve = if width >= 64 {
                    24
                } else if width >= 36 {
                    12
                } else {
                    0
                };
                let text = format!("{prefix}{label}");
                buffer.set_stringn(0, 0, &text, usize::from(width - reserve), Style::default());
                let before = buffer.clone();
                if let Some(detail) = location_detail_area(&buffer, area, label, reserve) {
                    let name_end = crate::ui::display_width_u16(&text);
                    assert_eq!(detail.x, name_end + 2, "{width}: {text}");
                    buffer.set_stringn(
                        detail.x,
                        0,
                        "Local / project / branch",
                        usize::from(detail.width),
                        Style::default(),
                    );
                    for x in 0..name_end {
                        assert_eq!(buffer[(x, 0)], before[(x, 0)], "{width}: {text}");
                    }
                    assert!(detail.right() < width - reserve);
                } else {
                    assert!(
                        crate::ui::display_width_u16(&text) + 2 >= width - reserve - 1,
                        "{width}: {text}"
                    );
                }
            }
        }
    }
}

fn snapshot() -> ClientShellSnapshot {
    let mut snapshot = crate::client::shell::tests::snapshot();
    for (index, status) in [
        AgentStatus::Idle,
        AgentStatus::Working,
        AgentStatus::Blocked,
        AgentStatus::Idle,
        AgentStatus::Done,
    ]
    .into_iter()
    .enumerate()
    {
        let pane = format!("p{index}");
        let mut terminal = snapshot.panes[0].clone();
        terminal.pane_id = pane.clone();
        snapshot.panes.push(terminal);
        snapshot.agents.push(crate::protocol::ClientShellAgent {
            pane_id: pane,
            workspace_id: "ws_1".into(),
            tab_id: "tab_1".into(),
            name: Some(format!("agent{index}")),
            display_agent: None,
            agent: Some("kimi".into()),
            title: None,
            terminal_title: None,
            terminal_title_stripped: None,
            agent_status: status,
            state_change_seq: 1,
            state_labels: Vec::new(),
            tokens: Vec::new(),
            focused: false,
            launch_seq: index as u64 + 1,
            activity: Default::default(),
            insight: crate::protocol::ClientShellAgentInsight {
                session_title: Some(format!("修复焦点 {index}")),
                model: Some("Sol".into()),
                question: (status == AgentStatus::Blocked)
                    .then(|| "Allow edit? 完整收到的问题".into()),
                last_message: Some("Message received from server".into()),
                last_prompt: Some("Prompt received from server".into()),
                attention: (index == 3).then(|| crate::protocol::ClientShellAgentAttention {
                    kind: AgentAttentionKind::Failed,
                    detail: Some("Command failed".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
        });
    }
    snapshot
}
fn state() -> ClientShellState {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_endpoint_methods(Some(vec!["agent.prompt".into(), "pane.focus".into()]));
    state.open_navigator_overlay();
    state.compose(160, 45).expect("overview");
    state
}
fn select(state: &mut ClientShellState, pane: &str) {
    let n = state.overview().expect("overview");
    let index = n
        .overview
        .rows
        .iter()
        .position(|row| {
            row.identity
                .as_ref()
                .is_some_and(|target| target.pane == pane)
        })
        .expect("row");
    state.activate_overview(Action::Row(index), &mut ClientShellInput::default());
    state.compose(160, 45).expect("selected");
}
#[test]
fn status_group_order_preview_received_fields_and_no_permission_reply() {
    let mut state = state();
    let buckets = state
        .overview()
        .expect("overview")
        .overview
        .rows
        .iter()
        .filter(|row| row.group.is_some())
        .map(|row| row.bucket)
        .collect::<Vec<_>>();
    assert_eq!(
        buckets,
        [
            AgentBucket::NeedsInput,
            AgentBucket::Failed,
            AgentBucket::Working,
            AgentBucket::Idle
        ]
    );
    select(&mut state, "p2");
    state.activate_overview(Action::Preview, &mut ClientShellInput::default());
    state.compose(80, 30).expect("narrow preview");
    let n = state.overview().expect("preview");
    let text = n.overview.lines.join("\n");
    assert!(
        text.contains("Allow edit?")
            && text.contains("Message received")
            && text.contains("Prompt received")
    );
    state.activate_overview(Action::Reply, &mut ClientShellInput::default());
    assert!(!state.overview().expect("overview").overview.editing);
    let mut outcome = ClientShellInput::default();
    state.send_overview_reply(&mut outcome);
    assert!(outcome.actions.is_empty() && outcome.requests.is_empty());
}
#[test]
fn reply_capability_multiline_single_flight_failure_keeps_draft() {
    let mut state = state();
    select(&mut state, "p0");
    state.activate_overview(Action::Reply, &mut ClientShellInput::default());
    assert!(state.insert_overview_text("第一行\nsecond line"));
    assert!(state
        .overview()
        .expect("editor")
        .overview
        .reply
        .contains('\n'));
    let mut outcome = ClientShellInput::default();
    state.send_overview_reply(&mut outcome);
    state.send_overview_reply(&mut outcome);
    assert_eq!(outcome.actions.len(), 1);
    assert!(outcome.requests.is_empty());
    let ClientShellAction::EndpointRequest {
        boot_id, request, ..
    } = &outcome.actions[0]
    else {
        panic!("API request");
    };
    assert!(
        matches!(&request.method,Method::AgentPrompt(params)if params.wait.is_none()&&params.target=="p0"&&params.text=="第一行\nsecond line")
    );
    let (boot, id) = (boot_id.clone(), request.id.clone());
    assert!(
        state
            .handle_endpoint_result(
                &boot,
                &id,
                Err(ClientShellEndpointError {
                    code: Some("endpoint_timeout".into()),
                    message: "timed out".into()
                })
            )
            .0
    );
    assert_eq!(
        state.overview().expect("editor").overview.reply.as_str(),
        "第一行\nsecond line"
    );
}
#[test]
fn old_server_unknown_capability_and_working_refuse_reply() {
    for methods in [None, Some(vec!["pane.focus".into()])] {
        let mut state = state();
        state.set_endpoint_methods(methods);
        select(&mut state, "p0");
        state.activate_overview(Action::Reply, &mut ClientShellInput::default());
        assert!(!state.overview().expect("overview").overview.editing);
    }
    let mut state = state();
    select(&mut state, "p1");
    state.activate_overview(Action::Reply, &mut ClientShellInput::default());
    assert!(!state.overview().expect("overview").overview.editing);
}
#[test]
fn unread_survives_surface_and_read_all_leaves_failure_fact() {
    let mut state = state();
    let agent = state.snapshot.as_ref().expect("snapshot").agents[0].clone();
    let target = Target::for_agent(
        ClientEndpointId::Local,
        state.snapshot.as_ref().expect("snapshot"),
        &agent,
    );
    assert!(state.mark_agent_read(&target, false));
    assert_eq!(
        state.snapshot.as_ref().expect("snapshot").agents[0].agent_status,
        AgentStatus::Done
    );
    let mut surface = crate::client::shell::tests::surface();
    surface.panes[0].pane_id = "p0".into();
    state.acknowledge_active_surface_agents(&surface);
    assert_eq!(
        state.snapshot.as_ref().expect("snapshot").agents[0].agent_status,
        AgentStatus::Done
    );
    state.mark_all_agents_read();
    assert_eq!(
        state.snapshot.as_ref().expect("snapshot").agents[0].agent_status,
        AgentStatus::Idle
    );
    assert_eq!(
        client_agent_bucket(&state.snapshot.as_ref().expect("snapshot").agents[3]),
        AgentBucket::Failed
    );
    let prefs = preferences::ClientChromePreferences {
        agent_marks: state.saved_agent_marks(),
        ..Default::default()
    };
    let raw = serde_json::to_string(&prefs).expect("serialize");
    let loaded: preferences::ClientChromePreferences = serde_json::from_str(&raw).expect("read");
    assert!(!loaded.agent_marks.is_empty());
}
#[test]
fn prompt_target_change_and_close_drop_late_response() {
    let mut state = state();
    select(&mut state, "p0");
    state.activate_overview(Action::Reply, &mut ClientShellInput::default());
    state.insert_overview_text("draft");
    let mut outcome = ClientShellInput::default();
    state.send_overview_reply(&mut outcome);
    let ClientShellAction::EndpointRequest {
        boot_id, request, ..
    } = &outcome.actions[0]
    else {
        panic!("request");
    };
    let (boot, id) = (boot_id.clone(), request.id.clone());
    select(&mut state, "p1");
    assert!(
        !state
            .handle_endpoint_result(
                &boot,
                &id,
                Err(ClientShellEndpointError {
                    code: None,
                    message: "late".into()
                })
            )
            .0
    );
    state.close_overview();
    assert!(state.overlay.is_none());
    assert!(
        state.inspector.target.is_none()
            || state
                .inspector
                .target
                .as_ref()
                .is_some_and(|target| target.pane == "pane_1")
    );
}
