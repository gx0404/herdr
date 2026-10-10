//! 待处理事项与统一通知策略在 server 上的落点：用户中断后的完成不发「已完成」，
//! `pane.report_agent_attention` 的校验 / 序号 / 不改状态，失败锁存的通知与投影。

use super::*;
use crate::api::schema::{
    AgentAttentionKind, Method, PaneAgentAttentionKind, PaneReportAgentAttentionParams,
};
use crate::detect::{Agent, AgentState};
use protocol::SemanticNotificationKind::{Custom, Finished};

/// 收集到屏障为止的语义通知（完整内容）。
fn semantic_notifications(
    server: &mut HeadlessServer,
    receiver: &std::sync::mpsc::Receiver<Vec<u8>>,
) -> Vec<protocol::SemanticNotification> {
    server.send_to_client(
        1,
        ServerMessage::EndpointControl {
            kind: "test.attention-barrier".into(),
            data: String::new(),
        },
    );
    let mut notifications = Vec::new();
    loop {
        let message = read_server_message(
            receiver
                .recv_timeout(LOADED_WAIT)
                .expect("notification barrier"),
        );
        if matches!(&message, ServerMessage::EndpointControl { kind, .. } if kind == "test.attention-barrier")
        {
            return notifications;
        }
        if let ServerMessage::SemanticNotification(notification) = message {
            notifications.push(notification);
        }
    }
}

fn state_changed(pane_id: crate::layout::PaneId, agent: Agent, state: AgentState) -> AppEvent {
    AppEvent::StateChanged {
        pane_id,
        agent: Some(agent),
        state,
        visible_blocker: false,
        visible_working: state == AgentState::Working,
        process_exited: false,
        observed_at: Instant::now(),
    }
}

fn ctrl_c() -> protocol::ClientPaneInputEvent {
    protocol::ClientPaneInputEvent::Key {
        code: protocol::ClientKeyCode::Char('c'),
        modifiers: crossterm::event::KeyModifiers::CONTROL.bits(),
        kind: protocol::ClientKeyKind::Press,
        repeat_count: 1,
        shifted_codepoint: None,
        generated_text: None,
        tracks_release: true,
        physical_key_id: None,
        windows_record: None,
    }
}

fn terminal(
    server: &HeadlessServer,
    pane_id: crate::layout::PaneId,
) -> &crate::terminal::TerminalState {
    let terminal_id = server
        .app
        .state
        .workspaces
        .iter()
        .find_map(|workspace| workspace.pane_state(pane_id))
        .expect("pane")
        .attached_terminal_id
        .clone();
    &server.app.state.terminals[&terminal_id]
}

#[tokio::test]
async fn completion_right_after_a_lone_ctrl_c_does_not_notify_finished() {
    let mut server = test_headless_server();
    let mut input_rx = install_focused_test_runtime(&mut server, b"");
    server.app.state.ensure_test_terminals();
    server.app.state.toast_config.delay_seconds = 0;
    let pane_id = server.app.state.workspaces[0].tabs[0].root_pane;
    let public_pane_id = server.app.public_pane_id(0, pane_id).unwrap();
    let (writer, control_rx, _render_rx) = test_client_writer();
    server.clients.insert(
        1,
        ClientConnection::new(
            (80, 24),
            Default::default(),
            1,
            RenderEncoding::SemanticFrame,
            Some(writer),
        ),
    );

    server.handle_internal_event_with_forwarding(AppEvent::AgentProcessDetected {
        pane_id,
        agent: Agent::Codex,
        observed_at: Instant::now(),
    });
    server.handle_internal_event_with_forwarding(state_changed(
        pane_id,
        Agent::Codex,
        AgentState::Idle,
    ));
    server.handle_internal_event_with_forwarding(state_changed(
        pane_id,
        Agent::Codex,
        AgentState::Working,
    ));
    semantic_notifications(&mut server, &control_rx);

    // 用户在这个 pane 里单独按了 Ctrl-C，agent 随即停下。
    server.handle_server_event(ServerEvent::ClientShellPaneInput {
        client_id: 1,
        pane_id: public_pane_id.clone(),
        events: vec![ctrl_c()],
    });
    assert_eq!(
        input_rx.try_recv().expect("中断送达 PTY"),
        Bytes::from_static(&[0x03])
    );
    assert!(terminal(&server, pane_id)
        .last_interrupt_input_at()
        .is_some());
    server.handle_internal_event_with_forwarding(state_changed(
        pane_id,
        Agent::Codex,
        AgentState::Idle,
    ));
    let interrupted = semantic_notifications(&mut server, &control_rx);
    assert!(
        interrupted
            .iter()
            .all(|notification| notification.kind != Finished),
        "用户中断后的完成不发「已完成」：{interrupted:?}"
    );

    // 正常的一回合照常通知。
    server.handle_internal_event_with_forwarding(state_changed(
        pane_id,
        Agent::Codex,
        AgentState::Working,
    ));
    server.handle_internal_event_with_forwarding(state_changed(
        pane_id,
        Agent::Codex,
        AgentState::Idle,
    ));
    let normal = semantic_notifications(&mut server, &control_rx);
    assert!(
        normal
            .iter()
            .any(|notification| notification.kind == Finished),
        "正常完成照常通知：{normal:?}"
    );
    shutdown_test_runtimes(&mut server);
}

fn attention_report(
    public_pane_id: &str,
    source: &str,
    agent: &str,
    kind: PaneAgentAttentionKind,
    detail: Option<&str>,
    seq: Option<u64>,
) -> Method {
    Method::PaneReportAgentAttention(PaneReportAgentAttentionParams {
        pane_id: public_pane_id.into(),
        source: source.into(),
        agent: agent.into(),
        kind,
        title: None,
        detail: detail.map(Into::into),
        options: Vec::new(),
        seq,
    })
}

fn api_response(server: &mut HeadlessServer, method: Method) -> serde_json::Value {
    let (respond_to, response_rx) = std::sync::mpsc::channel();
    server.handle_api_request_with_shutdown_check(api::ApiRequestMessage {
        request: api::schema::Request {
            id: "attention".into(),
            method,
        },
        respond_to,
        response_write_complete: None,
        observation_events: None,
        report_origin: None,
        stream_active: None,
    });
    serde_json::from_str(&response_rx.recv_timeout(LOADED_WAIT).expect("应答")).expect("JSON")
}

fn idle_pi_server(writer: ClientWriter) -> (HeadlessServer, crate::layout::PaneId, String) {
    let (mut server, pane_id) = completion_guard_server(writer);
    server.handle_internal_event_with_forwarding(AppEvent::AgentProcessDetected {
        pane_id,
        agent: Agent::Pi,
        observed_at: Instant::now(),
    });
    server.handle_internal_event_with_forwarding(state_changed(
        pane_id,
        Agent::Pi,
        AgentState::Idle,
    ));
    let public_pane_id = server.app.public_pane_id(0, pane_id).unwrap();
    (server, pane_id, public_pane_id)
}

#[test]
fn report_agent_attention_latches_failure_without_changing_state_and_notifies_once() {
    let (writer, control_rx, _render_rx) = test_client_writer();
    let (mut server, pane_id, public_pane_id) = idle_pi_server(writer);
    semantic_notifications(&mut server, &control_rx);

    let response = api_response(
        &mut server,
        attention_report(
            &public_pane_id,
            "herdr:pi",
            "pi",
            PaneAgentAttentionKind::Failed,
            Some("## Rate limited\n\nThe API returned **429**."),
            Some(1),
        ),
    );
    assert_eq!(response["result"]["type"], "ok", "{response}");
    assert_eq!(
        terminal(&server, pane_id).state,
        AgentState::Idle,
        "不改状态"
    );
    let attention = terminal(&server, pane_id)
        .agent_attention()
        .expect("失败锁存");
    assert_eq!(attention.kind, AgentAttentionKind::Failed);

    let notifications = semantic_notifications(&mut server, &control_rx);
    let failed = notifications
        .iter()
        .find(|notification| notification.kind == Custom)
        .unwrap_or_else(|| panic!("失败通知：{notifications:?}"));
    assert!(
        failed.title.contains(crate::i18n::texts().notify.failed),
        "{failed:?}"
    );
    assert_eq!(
        failed.sound,
        Some(protocol::SemanticNotificationSound::Request)
    );
    assert!(
        failed
            .body
            .as_deref()
            .is_some_and(|body| body.ends_with("Rate limited The API returned 429.")),
        "正文是去 markdown 的摘要：{failed:?}"
    );
    assert!(
        notifications
            .iter()
            .all(|notification| notification.kind != Finished),
        "{notifications:?}"
    );

    // 同一失败重复上报、旧序号的清除都不再通知也不改锁存。
    api_response(
        &mut server,
        attention_report(
            &public_pane_id,
            "herdr:pi",
            "pi",
            PaneAgentAttentionKind::Failed,
            Some("## Rate limited\n\nThe API returned **429**."),
            Some(2),
        ),
    );
    api_response(
        &mut server,
        attention_report(
            &public_pane_id,
            "herdr:pi",
            "pi",
            PaneAgentAttentionKind::Clear,
            None,
            Some(1),
        ),
    );
    assert!(semantic_notifications(&mut server, &control_rx).is_empty());
    assert!(terminal(&server, pane_id).agent_attention().is_some());

    // 用户看到这个 tab：已通知的失败被确认。
    assert!(server.app.state.switch_workspace_tab(0, 0));
    assert_eq!(terminal(&server, pane_id).agent_attention(), None);
}

#[test]
fn report_agent_attention_validates_source_lengths_and_pane() {
    let (writer, _control_rx, _render_rx) = test_client_writer();
    let (mut server, pane_id, public_pane_id) = idle_pi_server(writer);

    let mismatched = api_response(
        &mut server,
        attention_report(
            &public_pane_id,
            "herdr:claude",
            "pi",
            PaneAgentAttentionKind::Failed,
            None,
            None,
        ),
    );
    assert_eq!(
        mismatched["error"]["code"], "invalid_source",
        "{mismatched}"
    );
    let reserved = api_response(
        &mut server,
        attention_report(
            &public_pane_id,
            "herdr:unknown",
            "unknown",
            PaneAgentAttentionKind::Failed,
            None,
            None,
        ),
    );
    assert_eq!(reserved["error"]["code"], "invalid_source", "{reserved}");

    let too_long = "x".repeat(2001);
    let long = api_response(
        &mut server,
        attention_report(
            &public_pane_id,
            "custom:wrapper",
            "pi",
            PaneAgentAttentionKind::NeedsInput,
            Some(&too_long),
            None,
        ),
    );
    assert_eq!(long["error"]["code"], "invalid_attention", "{long}");

    let missing = api_response(
        &mut server,
        attention_report(
            "missing:p1",
            "custom:wrapper",
            "pi",
            PaneAgentAttentionKind::Failed,
            None,
            None,
        ),
    );
    assert_eq!(missing["error"]["code"], "pane_not_found", "{missing}");
    assert_eq!(
        terminal(&server, pane_id).agent_attention(),
        None,
        "被拒的上报不落锁存"
    );

    // 第三方来源可以上报；needs_input 只在受阻时露面且不改状态。
    let accepted = api_response(
        &mut server,
        attention_report(
            &public_pane_id,
            "custom:wrapper",
            "pi",
            PaneAgentAttentionKind::NeedsInput,
            Some("Approve the migration?"),
            Some(3),
        ),
    );
    assert_eq!(accepted["result"]["type"], "ok", "{accepted}");
    assert_eq!(terminal(&server, pane_id).state, AgentState::Idle);
    assert_eq!(terminal(&server, pane_id).agent_attention(), None);
}

#[test]
fn agent_exiting_while_working_notifies_failed_instead_of_finished() {
    let (writer, control_rx, _render_rx) = test_client_writer();
    let (mut server, pane_id) = completion_guard_server(writer);
    server.handle_internal_event_with_forwarding(AppEvent::AgentProcessDetected {
        pane_id,
        agent: Agent::Codex,
        observed_at: Instant::now(),
    });
    server.handle_internal_event_with_forwarding(state_changed(
        pane_id,
        Agent::Codex,
        AgentState::Working,
    ));
    semantic_notifications(&mut server, &control_rx);

    server.handle_internal_event_with_forwarding(AppEvent::StateChanged {
        pane_id,
        agent: Some(Agent::Codex),
        state: AgentState::Idle,
        visible_blocker: false,
        visible_working: false,
        process_exited: true,
        observed_at: Instant::now(),
    });
    let notifications = semantic_notifications(&mut server, &control_rx);
    let failed = notifications
        .iter()
        .find(|notification| notification.kind == Custom)
        .unwrap_or_else(|| panic!("运行中退出发失败通知：{notifications:?}"));
    assert!(
        failed.body.as_deref().is_some_and(|body| {
            body.contains(crate::i18n::texts().notify.process_exited_while_working)
        }),
        "{failed:?}"
    );
    assert!(
        notifications
            .iter()
            .all(|notification| notification.kind != Finished),
        "{notifications:?}"
    );
}

#[test]
fn agent_info_exposes_attention_and_turn_timing() {
    let (writer, _control_rx, _render_rx) = test_client_writer();
    let (mut server, _pane_id, public_pane_id) = idle_pi_server(writer);
    api_response(
        &mut server,
        attention_report(
            &public_pane_id,
            "herdr:pi",
            "pi",
            PaneAgentAttentionKind::Failed,
            Some("tool crashed"),
            None,
        ),
    );
    let response = api_response(
        &mut server,
        Method::AgentGet(api::schema::AgentTarget {
            target: public_pane_id.clone(),
        }),
    );
    let agent = &response["result"]["agent"];
    assert_eq!(agent["attention"]["kind"], "failed", "{response}");
    assert_eq!(agent["attention"]["detail"], "tool crashed", "{response}");
    assert!(
        agent["insight"]["state_changed_at_ms"].is_u64(),
        "回合计时随 insight 下发：{response}"
    );
    assert!(
        agent["insight"].get("working_since_ms").is_none(),
        "{response}"
    );
}
