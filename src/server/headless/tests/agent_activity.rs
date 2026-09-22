//! 活动树在 headless server 上的接线：刷新结果只动投影、调度随定时任务运行、
//! JSON API 与客户端端点的读取经后台线程异步应答。

use super::*;
use crate::api::schema::{AgentActivityNode, AgentActivityReadParams, AgentActivityStatus};
use crate::detect::{Agent, AgentState};

fn server_with_agent_pane() -> (HeadlessServer, crate::layout::PaneId) {
    let mut server = test_headless_server();
    server.app.state.workspaces = vec![crate::workspace::Workspace::test_new("activity")];
    server.app.state.ensure_test_terminals();
    server.app.state.active = Some(0);
    let pane_id = server.app.state.workspaces[0].tabs[0].root_pane;
    server.handle_internal_event_with_forwarding(AppEvent::StateChanged {
        pane_id,
        agent: Some(Agent::Claude),
        state: AgentState::Working,
        visible_blocker: false,
        visible_working: false,
        process_exited: false,
        observed_at: Instant::now(),
    });
    (server, pane_id)
}

/// 连上一个客户端 shell。两个接收端都要保活：测试写端由一个转发线程排空，
/// 任一接收端被丢弃都会让它停止转发后续控制帧。
fn connect_shell(
    server: &mut HeadlessServer,
    client_id: u64,
) -> (
    std::sync::mpsc::Receiver<Vec<u8>>,
    std::sync::mpsc::Receiver<Vec<u8>>,
) {
    let (writer, control_rx, render_rx) = test_client_writer();
    server.handle_server_event(ServerEvent::ClientShellConnected {
        surface_reuse: false,
        client_id,
        surface_cols: 80,
        surface_rows: 23,
        cell_width_px: 0,
        cell_height_px: 0,
        pixel_mouse: false,
        direct_graphics: false,
        endpoint_keybindings: false,
        mouse_capture: false,
        surface_active: true,
        ssh_auth_sock: None,
        writer,
    });
    (control_rx, render_rx)
}

/// 等下一份快照控制帧（跳过其他控制帧）。
fn next_snapshot(
    control_rx: &std::sync::mpsc::Receiver<Vec<u8>>,
) -> Box<crate::protocol::ClientShellSnapshot> {
    loop {
        let bytes = control_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("等待快照控制帧");
        if let ServerMessage::EndpointControl { kind, data } = read_server_message(bytes) {
            if kind == crate::protocol::endpoint::ENDPOINT_SNAPSHOT_KIND {
                return Box::new(
                    serde_json::from_str(&data).expect("decode client shell snapshot"),
                );
            }
        }
    }
}

fn node(id: &str, status: AgentActivityStatus) -> AgentActivityNode {
    AgentActivityNode {
        id: id.into(),
        label: format!("task {id}"),
        status,
        ..AgentActivityNode::default()
    }
}

#[tokio::test]
async fn activity_refresh_only_touches_the_projection_until_it_reaches_clients() {
    let (mut server, pane_id) = server_with_agent_pane();
    let (control_rx, _render_rx) = connect_shell(&mut server, 51);
    let initial = next_snapshot(&control_rx);
    assert_eq!(initial.agents.len(), 1);
    assert_eq!(
        initial.agents[0].launch_seq, 1,
        "headless 快照带 server 分配的启动序号"
    );
    assert_eq!(
        initial.agents[0].activity,
        crate::protocol::ClientShellAgentActivity::default()
    );

    let changed = server.handle_internal_event_with_forwarding(AppEvent::AgentActivityRefreshed {
        pane_id,
        result: Ok(vec![
            node("a", AgentActivityStatus::Running),
            node("b", AgentActivityStatus::Done),
        ]),
    });
    assert!(!changed, "活动树只进投影，不触发整帧重绘");
    assert!(server.agent_activity.projection_dirty());

    // 定时任务把脏标记翻成 chrome 影响，并保持到投影真正同步为止。
    let impact = server.handle_scheduled_tasks_headless(Instant::now(), false);
    assert!(impact.chrome);
    assert!(server.agent_activity.projection_dirty(), "未渲染前不清除");
    server.dispatch_render_tick(true, false, &HashSet::new(), false);
    assert!(!server.agent_activity.projection_dirty());

    let snapshot = next_snapshot(&control_rx);
    let activity = &snapshot.agents[0].activity;
    assert_eq!((activity.running, activity.total), (1, 2));
    assert_eq!(
        activity
            .nodes
            .iter()
            .map(|node| node.id.as_str())
            .collect::<Vec<_>>(),
        ["a", "b"]
    );
    assert_eq!(snapshot.agents[0].launch_seq, 1);

    // 同一棵树再来一次：不脏、不下发。
    assert!(
        !server.handle_internal_event_with_forwarding(AppEvent::AgentActivityRefreshed {
            pane_id,
            result: Ok(vec![
                node("a", AgentActivityStatus::Running),
                node("b", AgentActivityStatus::Done),
            ]),
        })
    );
    assert!(!server.agent_activity.projection_dirty());
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn external_agents_ride_the_snapshot() {
    let (mut server, _) = server_with_agent_pane();
    let (control_rx, _render_rx) = connect_shell(&mut server, 52);
    let _ = next_snapshot(&control_rx);
    assert!(
        !server.handle_internal_event_with_forwarding(AppEvent::ExternalAgentsRefreshed {
            source: "zcode".into(),
            result: Ok(vec![crate::api::schema::ExternalAgentInfo {
                external_id: "zcode:s-1".into(),
                source: "zcode".into(),
                agent_status: crate::api::schema::AgentStatus::Working,
                label: "desktop session".into(),
                readable: true,
                agent: None,
                cwd: Some("/tmp".into()),
                updated_at_ms: Some(1),
                activity: vec![node("t", AgentActivityStatus::Running)],
            }]),
        })
    );
    assert!(server.agent_activity.projection_dirty());
    server.dispatch_render_tick(true, false, &HashSet::new(), false);
    let snapshot = next_snapshot(&control_rx);
    assert_eq!(snapshot.external_agents.len(), 1);
    let external = &snapshot.external_agents[0];
    assert_eq!(external.external_id, "zcode:s-1");
    assert_eq!((external.activity.running, external.activity.total), (1, 1));
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn scheduled_tasks_submit_discovery_and_release_the_slot_on_the_result() {
    let (mut server, pane_id) = server_with_agent_pane();
    let _ = server.handle_scheduled_tasks_headless(Instant::now(), false);
    // 注册表里的 claude 仍是空壳：后台线程回 Err，经 app 事件通道回来。
    let event = tokio::time::timeout(Duration::from_secs(5), server.app.event_rx.recv())
        .await
        .expect("后台发现结果")
        .expect("通道未关闭");
    let AppEvent::AgentActivityRefreshed {
        pane_id: refreshed,
        result,
    } = &event
    else {
        panic!("应为活动树刷新事件：{event:?}");
    };
    assert_eq!(*refreshed, pane_id);
    assert!(result.is_err(), "空壳适配器报 unsupported");
    assert!(!server.handle_internal_event_with_forwarding(event));
    assert!(
        !server.agent_activity.projection_dirty(),
        "失败结果保留旧树"
    );
    shutdown_test_runtimes(&mut server);
}

fn api_request(
    server: &mut HeadlessServer,
    method: api::schema::Method,
) -> std::sync::mpsc::Receiver<String> {
    let (respond_to, response_rx) = std::sync::mpsc::channel();
    assert!(!server.handle_api_request_with_shutdown_check_inner(
        api::ApiRequestMessage {
            request: api::schema::Request {
                id: "api-activity".into(),
                method,
            },
            respond_to,
            response_write_complete: None,
            observation_events: None,
            stream_active: None,
        },
        false,
    ));
    response_rx
}

#[tokio::test]
async fn json_api_activity_reads_answer_asynchronously() {
    let (mut server, pane_id) = server_with_agent_pane();
    let public = server.app.public_pane_id(0, pane_id).expect("公开 id");

    // 参数错误在主线程同步拒绝。
    let invalid = api_request(
        &mut server,
        api::schema::Method::AgentActivityRead(AgentActivityReadParams::default()),
    )
    .recv_timeout(Duration::from_secs(5))
    .expect("同步应答");
    assert!(invalid.contains("invalid_params"), "{invalid}");

    // 合法请求交给后台线程；claude 适配器仍是空壳 → not_implemented。
    let response = api_request(
        &mut server,
        api::schema::Method::AgentActivityRead(AgentActivityReadParams {
            pane_id: Some(public),
            ..AgentActivityReadParams::default()
        }),
    )
    .recv_timeout(Duration::from_secs(5))
    .expect("异步应答");
    let response: api::schema::ErrorResponse = serde_json::from_str(&response).expect("错误应答");
    assert_eq!(response.id, "api-activity");
    assert_eq!(
        response.error.code,
        crate::server::agent_activity::NOT_IMPLEMENTED_CODE
    );

    // 外部来源只有空壳 zcode（默认实现回空列表）：列表为空、成功。
    let list = api_request(
        &mut server,
        api::schema::Method::AgentExternalList(api::schema::EmptyParams::default()),
    )
    .recv_timeout(Duration::from_secs(5))
    .expect("异步应答");
    let list: api::schema::SuccessResponse = serde_json::from_str(&list).expect("成功应答");
    assert!(matches!(
        list.result,
        api::schema::ResponseResult::ExternalAgentList { agents } if agents.is_empty()
    ));
    shutdown_test_runtimes(&mut server);
}
