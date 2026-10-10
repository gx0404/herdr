//! 待处理事项与统一通知策略在 server 上的落点：用户中断后的完成不发「已完成」，
//! `pane.report_agent_attention` 的校验 / 序号 / 不改状态，失败锁存的通知与投影，
//! 以及一个失败的回合在每条失败路径上都只通知一次。

use super::*;
use crate::api::schema::{
    AgentAttentionKind, Method, PaneAgentAttentionKind, PaneAgentState,
    PaneReportAgentAttentionParams, PaneReportAgentParams,
};
use crate::detect::{Agent, AgentState};
use protocol::SemanticNotificationKind::{Custom, Finished, NeedsAttention};

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

// ---------------------------------------------------------------------------
// 一个失败的回合只通知一次
// ---------------------------------------------------------------------------

/// 屏障之前收到的通知：给客户端 shell 的语义通知，转发给前台客户端的声音与系统 toast。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Notice {
    Semantic(protocol::SemanticNotificationKind),
    Sound(String),
    Toast(String),
}

/// 一条失败路径的现场：pane 在后台工作区（toast 与声音不受「正看着」抑制），声音与系统
/// toast 都转发给前台客户端。生命周期与失败经 API 上报（集成钩子的同一入口），屏幕
/// 检测与会话洞察走各自的内部事件。
struct FailurePath {
    server: HeadlessServer,
    pane_id: crate::layout::PaneId,
    public_pane_id: String,
    agent: Agent,
    agent_label: &'static str,
    control_rx: std::sync::mpsc::Receiver<Vec<u8>>,
    _render_rx: std::sync::mpsc::Receiver<Vec<u8>>,
    seq: u64,
    ticket: u64,
}

impl FailurePath {
    fn new(agent: Agent, agent_label: &'static str) -> Self {
        let (writer, control_rx, render_rx) = test_client_writer();
        let (mut server, pane_id) = completion_guard_server(writer);
        server.foreground_client_id = Some(1);
        server.app.state.sound.enabled = true;
        server.app.state.toast_config.delivery = crate::config::ToastDelivery::System;
        server.sync_foreground_client_state();
        server.handle_internal_event_with_forwarding(AppEvent::AgentProcessDetected {
            pane_id,
            agent,
            observed_at: Instant::now(),
        });
        let public_pane_id = server.app.public_pane_id(0, pane_id).unwrap();
        Self {
            server,
            pane_id,
            public_pane_id,
            agent,
            agent_label,
            control_rx,
            _render_rx: render_rx,
            seq: 0,
            ticket: 0,
        }
    }

    /// 生命周期上报。用第三方来源：官方来源的完整生命周期权威还要求会话锚定，与这里
    /// 要测的通知规则无关。
    fn report(&mut self, state: PaneAgentState) {
        self.seq += 1;
        completion_guard_api_report(
            &mut self.server,
            Method::PaneReportAgent(PaneReportAgentParams {
                pane_id: self.public_pane_id.clone(),
                source: format!("custom:{}", self.agent_label),
                agent: self.agent_label.into(),
                state,
                message: None,
                seq: Some(self.seq),
                agent_session_id: None,
                agent_session_path: None,
                resume_argv: None,
            }),
        );
    }

    /// 集成钩子报的失败（`herdr:<agent>`）。
    fn fail(&mut self, detail: &str) {
        self.seq += 1;
        let response = api_response(
            &mut self.server,
            attention_report(
                &self.public_pane_id,
                &format!("herdr:{}", self.agent_label),
                self.agent_label,
                PaneAgentAttentionKind::Failed,
                Some(detail),
                Some(self.seq),
            ),
        );
        assert_eq!(response["result"]["type"], "ok", "{response}");
    }

    /// 屏幕检测到的状态。
    fn detect(&mut self, state: AgentState) {
        self.server
            .handle_internal_event_with_forwarding(state_changed(self.pane_id, self.agent, state));
    }

    /// 一次后台发现读到会话里的错误（错误时刻在本回合内）。
    fn insight_error(&mut self, error: &str) {
        let identity = self
            .server
            .app
            .state
            .agent_activity_subject(self.pane_id)
            .expect("活动身份")
            .identity;
        self.ticket += 1;
        self.server
            .handle_internal_event_with_forwarding(AppEvent::AgentActivityRefreshedFor {
                pane_id: self.pane_id,
                identity,
                started: Default::default(),
                ticket: self.ticket,
                result: Ok(Vec::new()),
                insight: Some(Box::new(crate::api::schema::AgentInsight {
                    error: Some(error.into()),
                    error_at_ms: Some(crate::server::observability::now_ms()),
                    ..Default::default()
                })),
            });
    }

    fn notices(&mut self) -> Vec<Notice> {
        self.server.send_to_client(
            1,
            ServerMessage::EndpointControl {
                kind: "test.notice-barrier".into(),
                data: String::new(),
            },
        );
        let mut notices = Vec::new();
        loop {
            match read_server_message(
                self.control_rx
                    .recv_timeout(LOADED_WAIT)
                    .expect("notification barrier"),
            ) {
                ServerMessage::EndpointControl { kind, .. } if kind == "test.notice-barrier" => {
                    return notices;
                }
                ServerMessage::SemanticNotification(notification) => {
                    notices.push(Notice::Semantic(notification.kind));
                }
                ServerMessage::Notify {
                    kind: protocol::NotifyKind::Sound,
                    message,
                    ..
                } => notices.push(Notice::Sound(message)),
                ServerMessage::Notify { message, .. } => notices.push(Notice::Toast(message)),
                _ => {}
            }
        }
    }

    fn failure_detail(&self) -> Option<String> {
        terminal(&self.server, self.pane_id)
            .agent_attention()
            .filter(|attention| attention.kind == AgentAttentionKind::Failed)
            .and_then(|attention| attention.detail)
    }
}

/// 恰好一次某种通知：语义通知、声音、系统 toast 各一条，没有别的通知。
fn assert_notified_once(notices: &[Notice], kind: protocol::SemanticNotificationKind, path: &str) {
    let texts = &crate::i18n::texts().notify;
    let (event, sound) = match kind {
        Custom => (texts.failed, "agent attention"),
        NeedsAttention => (texts.needs_attention, "agent attention"),
        Finished => (texts.finished, "agent done"),
        other => panic!("{other:?}"),
    };
    let mut counts = (0, 0, 0);
    for notice in notices {
        match notice {
            Notice::Semantic(seen) if *seen == kind => counts.0 += 1,
            Notice::Sound(message) if message == sound => counts.1 += 1,
            Notice::Toast(message) if message.contains(event) => counts.2 += 1,
            other => panic!("{path}：多了 {other:?}：{notices:?}"),
        }
    }
    assert_eq!(counts, (1, 1, 1), "{path}：{notices:?}");
}

/// 集成报的失败（Kimi `StopFailure`、Pi 出错的回合、OpenCode `session.error`）先于结束
/// 回合的 idle 到达：只发一次「失败」，没有「已完成」。之后会话文件读到的同一个错误与
/// 重复的 idle 都不再通知，详情保留集成报的那份；新回合照常「已完成」。
#[test]
fn a_failure_reported_before_idle_notifies_failed_once_per_turn() {
    for (agent, label) in [
        (Agent::Kimi, "kimi"),
        (Agent::Pi, "pi"),
        (Agent::OpenCode, "opencode"),
    ] {
        let mut path = FailurePath::new(agent, label);
        path.report(PaneAgentState::Idle);
        path.report(PaneAgentState::Working);
        path.notices();

        path.fail("rate limited");
        assert_eq!(path.notices(), [], "{label}：Working 时不通知");
        path.report(PaneAgentState::Idle);
        assert_notified_once(&path.notices(), Custom, label);

        path.insight_error("RateLimitError: rate limited");
        path.report(PaneAgentState::Idle);
        assert_eq!(path.notices(), [], "{label}：同一回合不再通知");
        assert_eq!(
            path.failure_detail().as_deref(),
            Some("rate limited"),
            "{label}"
        );

        path.report(PaneAgentState::Working);
        path.report(PaneAgentState::Idle);
        assert_notified_once(&path.notices(), Finished, label);
    }
}

/// 失败之后回合以受阻收尾（旧版 OpenCode 插件 `session.error` 报 `blocked`）：受阻的那
/// 次迁移发「失败」而不是「需要处理」，之后同一回合回到空闲、洞察再读到错误都不通知；
/// 新回合里的受阻照常「需要处理」。
#[test]
fn a_failure_announced_at_blocked_keeps_the_rest_of_the_turn_silent() {
    let mut path = FailurePath::new(Agent::OpenCode, "opencode");
    path.report(PaneAgentState::Idle);
    path.report(PaneAgentState::Working);
    path.notices();

    path.fail("Invalid api key");
    path.report(PaneAgentState::Blocked);
    assert_notified_once(&path.notices(), Custom, "失败后受阻");
    path.report(PaneAgentState::Idle);
    path.insight_error("ProviderAuthError: Invalid api key");
    assert_eq!(path.notices(), [], "同一回合的空闲与洞察");

    path.report(PaneAgentState::Working);
    path.report(PaneAgentState::Blocked);
    assert_notified_once(&path.notices(), NeedsAttention, "新回合");
}

/// 运行中退出（2a）：退出的那次更新只发一次「失败」与一次请求音。系统 toast 的文案取
/// pane 当前的 agent，agent 已随退出离开 pane，所以这条路径没有系统 toast。
#[test]
fn an_agent_exiting_while_working_notifies_failed_once() {
    let mut path = FailurePath::new(Agent::Codex, "codex");
    path.detect(AgentState::Idle);
    path.detect(AgentState::Working);
    path.notices();

    path.server
        .handle_internal_event_with_forwarding(AppEvent::StateChanged {
            pane_id: path.pane_id,
            agent: Some(Agent::Codex),
            state: AgentState::Idle,
            visible_blocker: false,
            visible_working: false,
            process_exited: true,
            observed_at: Instant::now(),
        });
    assert_eq!(
        path.notices(),
        [
            Notice::Semantic(Custom),
            Notice::Sound("agent attention".into())
        ]
    );
}

/// 会话洞察（2b）读到的错误：屏幕先报回合结束（这时还不知道失败，照常「已完成」），
/// 洞察读到错误时发一次「失败」；同一回合之后的重复错误、另一个错误、屏幕上的受阻与
/// 空闲都不再通知；新回合照常「已完成」。
#[test]
fn an_insight_error_notifies_failed_once_and_silences_the_rest_of_the_turn() {
    for (agent, label) in [(Agent::Codex, "codex"), (Agent::Claude, "claude")] {
        let mut path = FailurePath::new(agent, label);
        path.detect(AgentState::Idle);
        path.detect(AgentState::Working);
        path.notices();

        path.detect(AgentState::Idle);
        assert_notified_once(&path.notices(), Finished, label);
        path.insight_error("stream disconnected before completion");
        assert_notified_once(&path.notices(), Custom, label);

        path.insight_error("stream disconnected before completion");
        path.insight_error("another error in the same turn");
        path.detect(AgentState::Blocked);
        path.detect(AgentState::Idle);
        assert_eq!(path.notices(), [], "{label}：同一回合不再通知");

        path.detect(AgentState::Working);
        path.detect(AgentState::Idle);
        assert_notified_once(&path.notices(), Finished, label);
    }
}
