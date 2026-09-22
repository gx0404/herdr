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
    // 默认摘要形态：计数 + 最新节点，整树经 agent.activity.read 取。
    assert_eq!(
        (activity.running, activity.total, activity.truncated),
        (1, 2, true)
    );
    assert_eq!(
        activity
            .nodes
            .iter()
            .map(|node| node.id.as_str())
            .collect::<Vec<_>>(),
        ["a"]
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

fn endpoint_responses(
    control_rx: &std::sync::mpsc::Receiver<Vec<u8>>,
    request_id: &str,
) -> serde_json::Value {
    let mut data = Vec::new();
    loop {
        let bytes = control_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("等待端点应答");
        if let ServerMessage::ClientShellEndpointResponseChunk {
            request_id: chunk_request,
            final_chunk,
            data: chunk,
            ..
        } = read_server_message(bytes)
        {
            if chunk_request != request_id {
                continue;
            }
            data.extend(chunk);
            if final_chunk {
                return serde_json::from_slice(&data).expect("应答是 JSON");
            }
        }
    }
}

#[tokio::test]
async fn client_endpoint_activity_reads_bypass_the_command_lane() {
    let (mut server, pane_id) = server_with_agent_pane();
    let client_id = 53;
    let (control_rx, _render_rx) = connect_shell(&mut server, client_id);
    let _ = next_snapshot(&control_rx);
    let boot_id = server.client_shell_boot_id.clone();
    let public = server.app.public_pane_id(0, pane_id).expect("公开 id");

    for method in [
        api::schema::Method::AgentActivityRead(AgentActivityReadParams::default()),
        api::schema::Method::AgentExternalList(api::schema::EmptyParams::default()),
    ] {
        assert!(
            crate::server::client_commands::supports_client_shell_method(&method),
            "已宣告进客户端端点"
        );
    }

    // 参数错误：主线程同步回错误分块。
    assert!(
        !server.handle_server_event(ServerEvent::ClientShellEndpointRequest {
            client_id,
            boot_id: boot_id.clone(),
            request: Box::new(api::schema::Request {
                id: "client-shell:invalid".into(),
                method: api::schema::Method::AgentActivityRead(AgentActivityReadParams::default()),
            }),
        })
    );
    let invalid = endpoint_responses(&control_rx, "client-shell:invalid");
    assert_eq!(invalid["error"]["code"], "invalid_params");

    // 合法读取：不占终端命令的 in-flight 名额，应答经 server 事件通道回来。
    assert!(
        !server.handle_server_event(ServerEvent::ClientShellEndpointRequest {
            client_id,
            boot_id: boot_id.clone(),
            request: Box::new(api::schema::Request {
                id: "client-shell:activity".into(),
                method: api::schema::Method::AgentActivityRead(AgentActivityReadParams {
                    pane_id: Some(public),
                    ..AgentActivityReadParams::default()
                }),
            }),
        })
    );
    assert!(!server.clients[&client_id].shell_endpoint_command_in_flight);
    let ready = tokio::time::timeout(Duration::from_secs(5), server.server_event_rx.recv())
        .await
        .expect("后台应答")
        .expect("通道未关闭");
    assert!(matches!(ready, ServerEvent::ObservationResponse { .. }));
    assert!(!server.handle_server_event(ready));
    let response = endpoint_responses(&control_rx, "client-shell:activity");
    // claude 适配器仍是空壳。
    assert_eq!(
        response["error"]["code"],
        crate::server::agent_activity::NOT_IMPLEMENTED_CODE
    );
    shutdown_test_runtimes(&mut server);
}

/// 外部来源（ZCode）的一次刷新：一条桌面会话。
fn zcode_refreshed(label: &str) -> AppEvent {
    AppEvent::ExternalAgentsRefreshed {
        source: "zcode".into(),
        result: Ok(vec![crate::api::schema::ExternalAgentInfo {
            external_id: "zcode:s-1".into(),
            source: "zcode".into(),
            agent_status: crate::api::schema::AgentStatus::Working,
            label: label.into(),
            readable: true,
            agent: None,
            cwd: Some("/tmp".into()),
            updated_at_ms: Some(1),
            activity: vec![node("t", AgentActivityStatus::Running)],
        }]),
    }
}

/// 限时收下一帧 surface：修复前这些用例里根本不会有帧，不能无限阻塞。
fn recv_surface_within(
    render_rx: &std::sync::mpsc::Receiver<Vec<u8>>,
    context: &str,
) -> crate::protocol::PaneSurfaceFrame {
    let bytes = render_rx
        .recv_timeout(Duration::from_secs(2))
        .unwrap_or_else(|error| panic!("{context}: {error}"));
    match read_server_message(bytes) {
        ServerMessage::PaneSurface(surface) => surface,
        other => panic!("{context}: expected pane surface, got {other:?}"),
    }
}

/// 限时收下一条 retained 补丁。
fn recv_patch_within(
    render_rx: &std::sync::mpsc::Receiver<Vec<u8>>,
    context: &str,
) -> crate::protocol::PaneSurfacePatch {
    let bytes = render_rx
        .recv_timeout(Duration::from_secs(2))
        .unwrap_or_else(|error| panic!("{context}: {error}"));
    match read_server_message(bytes) {
        ServerMessage::PaneSurfacePatch(patch) => patch,
        other => panic!("{context}: expected pane surface patch, got {other:?}"),
    }
}

fn patch_text(patch: &crate::protocol::PaneSurfacePatch) -> String {
    patch
        .rows
        .iter()
        .flat_map(|row| row.cells.iter().map(|cell| cell.symbol.as_str()))
        .collect()
}

/// 客户端只画修订号与快照精确配对的 surface（workbench 下不配对就画「正在同步
/// 终端…」）。外部条目落库让快照修订号前进时，同一 tick 必须补一帧新修订号的
/// surface——否则空闲 pane 一直停在占位上，直到它下一次输出。
///
/// 补的是已提交基线的改戳帧，不重渲染 pane：终端内容先变但不标脏，改戳帧仍是
/// 旧画面（整帧渲染会画出新内容）。此后的输出 tick 以改戳后的基线走 retained
/// 补丁，不退化成整帧。
#[tokio::test]
async fn external_refresh_pairs_the_advanced_snapshot_with_a_surface() {
    let mut server = test_headless_server();
    let pane_id = install_shared_view_test_runtime(&mut server);
    let (control_rx, render_rx) = connect_matching_test_shell(&mut server, 61);
    let _ = next_snapshot(&control_rx);
    server.render_and_stream();
    let baseline = recv_surface_within(&render_rx, "baseline");

    write_shared_test_pane(&mut server, pane_id, b"\rLATE");
    assert!(!server.handle_internal_event_with_forwarding(zcode_refreshed("desktop session")));
    assert!(server.agent_activity.projection_dirty());
    server.dispatch_render_tick(true, false, &HashSet::new(), false);

    let snapshot = next_snapshot(&control_rx);
    assert_eq!(snapshot.external_agents.len(), 1);
    assert!(snapshot.revision > baseline.projection_revision);
    let surface = recv_surface_within(&render_rx, "surface paired with the advanced snapshot");
    assert_eq!(surface.projection_revision, snapshot.revision);
    assert_eq!(
        surface.frame, baseline.frame,
        "改戳帧沿用已提交基线，不重渲染"
    );
    assert!(!frame_text(&surface.frame).contains("LATE"));
    assert!(!server.agent_activity.projection_dirty());

    server.dispatch_render_tick(false, false, &HashSet::from([pane_id]), false);
    let patch = recv_patch_within(&render_rx, "retained patch on the restamped baseline");
    assert_eq!(patch.projection_revision, snapshot.revision);
    assert_eq!(patch.base_surface_revision, surface.surface_revision);
    assert!(patch_text(&patch).contains("LATE"));

    // 同一批条目再来一次：快照不变，不重发 surface（RS-12 的省略仍然成立）。
    assert!(!server.handle_internal_event_with_forwarding(zcode_refreshed("desktop session")));
    server.dispatch_render_tick(true, false, &HashSet::new(), false);
    assert!(render_rx.try_recv().is_err(), "投影未变时不应重发 surface");
    shutdown_test_runtimes(&mut server);
}

/// 协商了复用编码的连接：改戳帧只带非单元格部分，客户端按基线补齐单元格。
#[tokio::test]
async fn projection_restamp_rides_the_surface_reuse_codec() {
    let mut server = test_headless_server();
    let _pane_id = install_shared_view_test_runtime(&mut server);
    let (writer, control_rx, render_rx) = test_client_writer();
    server.handle_server_event(ServerEvent::ClientShellConnected {
        surface_reuse: true,
        client_id: 64,
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
    let _ = next_snapshot(&control_rx);
    server.render_and_stream();
    let mut decoder = crate::protocol::surface_reuse::Decoder::default();
    let ServerMessage::PaneSurface(baseline) = decoder
        .decode(read_server_message(
            render_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("baseline"),
        ))
        .expect("decode baseline")
    else {
        panic!("baseline is a full surface");
    };

    assert!(!server.handle_internal_event_with_forwarding(zcode_refreshed("desktop session")));
    server.dispatch_render_tick(true, false, &HashSet::new(), false);
    let snapshot = next_snapshot(&control_rx);
    let bytes = render_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("restamped surface");
    assert!(
        bytes.len() < 4096,
        "改戳帧 {} 字节，不应重发单元格",
        bytes.len()
    );
    let message = read_server_message(bytes);
    assert!(matches!(
        &message,
        ServerMessage::EndpointControl { kind, .. }
            if kind == crate::protocol::surface_reuse::MESSAGE_KIND
    ));
    let ServerMessage::PaneSurface(decoded) = decoder.decode(message).expect("decode restamp")
    else {
        panic!("restamp decodes to a full surface");
    };
    assert_eq!(decoded.projection_revision, snapshot.revision);
    assert_eq!(decoded.frame, baseline.frame);
    shutdown_test_runtimes(&mut server);
}

/// 投影前进与可见 pane 输出同 tick：渲染槽已被改戳帧占住，脏源重新挂回渲染
/// 信号，下一 tick 以改戳后的基线发 retained 补丁——不升级成整帧，也不丢输出。
#[tokio::test]
async fn coincident_output_follows_the_restamp_as_a_retained_patch() {
    let mut server = test_headless_server();
    let pane_id = install_shared_view_test_runtime(&mut server);
    let (control_rx, render_rx) = connect_matching_test_shell(&mut server, 65);
    let _ = next_snapshot(&control_rx);
    server.render_and_stream();
    let baseline = recv_surface_within(&render_rx, "baseline");
    let _ = server.app.render_dirty.take();

    assert!(!server.handle_internal_event_with_forwarding(zcode_refreshed("desktop session")));
    write_shared_test_pane(&mut server, pane_id, b"\rCOINCIDENT");
    let sources = HashSet::from([pane_id]);
    assert!(server.pty_sources_visible_to_any_render_target(&sources));
    server.dispatch_render_tick(true, false, &sources, false);

    let snapshot = next_snapshot(&control_rx);
    let restamped = recv_surface_within(&render_rx, "restamp");
    assert_eq!(restamped.projection_revision, snapshot.revision);
    assert_eq!(restamped.frame, baseline.frame, "同 tick 不升级成整帧");
    let requeued = server.app.render_dirty.take();
    assert!(
        requeued.pty_sources.contains(&pane_id),
        "脏源要挂回下一 tick"
    );

    server.dispatch_render_tick(false, false, &requeued.pty_sources, false);
    let patch = recv_patch_within(&render_rx, "coincident output as a retained patch");
    assert_eq!(patch.projection_revision, snapshot.revision);
    assert!(patch_text(&patch).contains("COINCIDENT"));
    shutdown_test_runtimes(&mut server);
}

/// 没有已提交基线（连接后还没画过）时改戳不适用：同 tick 回退整帧渲染，仍然
/// 补上与新快照配对的 surface。
#[tokio::test]
async fn restamp_without_a_baseline_falls_back_to_a_full_render() {
    let mut server = test_headless_server();
    let pane_id = install_shared_view_test_runtime(&mut server);
    let (control_rx, render_rx) = connect_matching_test_shell(&mut server, 66);
    let initial = next_snapshot(&control_rx);
    assert!(server.clients[&66]
        .render_state
        .last_pane_surface()
        .is_none());

    write_shared_test_pane(&mut server, pane_id, b"\rLIVE");
    assert!(!server.handle_internal_event_with_forwarding(zcode_refreshed("desktop session")));
    server.dispatch_render_tick(true, false, &HashSet::new(), false);

    let snapshot = next_snapshot(&control_rx);
    assert!(snapshot.revision > initial.revision);
    let surface = recv_surface_within(&render_rx, "full render fallback");
    assert_eq!(surface.projection_revision, snapshot.revision);
    assert!(frame_text(&surface.frame).contains("LIVE"));
    shutdown_test_runtimes(&mut server);
}

/// 基线陈旧（快照修订号已前进）时 retained 快路径剔除唯一的接收者：必须武装
/// 延期全量渲染并唤醒渲染，否则此后每个输出 tick 都「全部剔除即成功」早退，
/// pane 输出永远到不了客户端。
#[tokio::test]
async fn a_stale_sole_baseline_arms_a_full_render_instead_of_stalling() {
    let mut server = test_headless_server();
    let pane_id = install_shared_view_test_runtime(&mut server);
    let (control_rx, render_rx) = connect_matching_test_shell(&mut server, 62);
    let _ = next_snapshot(&control_rx);
    server.render_and_stream();
    let baseline = recv_surface_within(&render_rx, "baseline");

    // 只刷投影、不补 surface：唯一接收者的基线随之陈旧。
    assert!(!server.handle_internal_event_with_forwarding(zcode_refreshed("desktop session")));
    server.stream_client_shell_projections();
    let snapshot = next_snapshot(&control_rx);
    assert!(snapshot.revision > baseline.projection_revision);
    let _ = server.app.render_dirty.take();

    write_shared_test_pane(&mut server, pane_id, b"\rAFTER");
    assert!(server.render_retained_pane_surface_and_stream(&HashSet::from([pane_id])));
    assert!(render_rx.try_recv().is_err(), "陈旧基线上不能发补丁");
    assert_eq!(server.clients[&62].deferred_render(), DeferredRender::Full);
    assert!(
        server.app.render_dirty.is_pending(),
        "延期必须安排一次全量渲染恢复基线"
    );

    server.render_and_stream();
    let recovered = recv_surface_within(&render_rx, "recovery full render");
    assert_eq!(recovered.projection_revision, snapshot.revision);
    assert!(frame_text(&recovered.frame).contains("AFTER"));
    assert_eq!(server.clients[&62].deferred_render(), DeferredRender::None);
    shutdown_test_runtimes(&mut server);
}

/// 纯 chrome tick 撞上同 tick 的全量渲染需求（server 事件、无 pty 脏源）：投影
/// 刷新之后仍要走全量渲染，不能被「无 surface 工作」早退吞掉。
#[tokio::test]
async fn projection_only_tick_keeps_a_coincident_full_render() {
    let mut server = test_headless_server();
    let pane_id = install_shared_view_test_runtime(&mut server);
    let (control_rx, render_rx) = connect_matching_test_shell(&mut server, 63);
    let _ = next_snapshot(&control_rx);
    server.render_and_stream();
    let _ = recv_surface_within(&render_rx, "baseline");

    // 投影纪元递增但快照不变（修订号不前进），同时有一处需要全量渲染的变化。
    server.app.state.bump_projection_epoch();
    write_shared_test_pane(&mut server, pane_id, b"\rFULL");
    server.dispatch_render_tick(true, true, &HashSet::new(), false);

    let surface = recv_surface_within(&render_rx, "coincident full render");
    assert!(frame_text(&surface.frame).contains("FULL"));
    shutdown_test_runtimes(&mut server);
}
