//! 集成上报来源校验在事件循环上的落点：注入父链快照，走完整的 API 请求处理路径。

use super::*;
use crate::platform::{ProcessLineage, ProcessParentEntry};

/// 测试窗格的根进程 pid。
const PANE_ROOT: u32 = 4_000;

fn lineage(chain: &[(u32, &str)]) -> ProcessLineage {
    let processes = chain
        .iter()
        .enumerate()
        .map(|(index, (pid, name))| ProcessParentEntry {
            pid: *pid,
            parent_pid: chain.get(index + 1).map_or(0, |(parent, _)| *parent),
            name: (*name).into(),
        })
        .collect();
    ProcessLineage {
        processes,
        complete: true,
    }
}

/// 窗格 shell 下的前台 claude 钩子。
fn inside_pane() -> ProcessLineage {
    lineage(&[
        (4_020, "python3"),
        (4_010, "claude"),
        (PANE_ROOT, "zsh"),
        (1, "systemd"),
    ])
}

/// 继承了窗格环境、却挂在 systemd --user 下的后台会话钩子。
fn detached() -> ProcessLineage {
    lineage(&[
        (9_030, "python3"),
        (9_020, "claude"),
        (9_010, "bg-pty-host"),
        (9_000, "systemd"),
        (1, "systemd"),
    ])
}

fn origin(lineage: Option<ProcessLineage>) -> api::ReportOrigin {
    api::ReportOrigin::capture_with(Some(9_030), |_| lineage)
}

struct ReportFixture {
    server: HeadlessServer,
    public_pane_id: String,
    terminal_id: crate::terminal::TerminalId,
}

impl ReportFixture {
    fn new(pane_root: Option<u32>) -> Self {
        let mut server = test_headless_server();
        let mut workspace = crate::workspace::Workspace::test_new("reports");
        let pane_id = workspace.tabs[0].root_pane;
        let public_pane_id = format!("{}:p1", workspace.id);
        let (runtime, _rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        if let Some(pid) = pane_root {
            runtime.test_set_child_pid(pid);
        }
        workspace.insert_test_runtime(pane_id, runtime);
        server.app.state.workspaces = vec![workspace];
        server.app.state.ensure_test_terminals();
        let terminal_id = server.app.state.workspaces[0]
            .pane_state(pane_id)
            .unwrap()
            .attached_terminal_id
            .clone();
        server
            .app
            .state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .set_detected_state(
                Some(crate::detect::Agent::Pi),
                crate::detect::AgentState::Idle,
            );
        Self {
            server,
            public_pane_id,
            terminal_id,
        }
    }

    /// 以 `source` 上报 working，返回应答。校验只看 `herdr:` 前缀；用例用 `herdr:test`
    /// 而不是 `herdr:pi`，避开官方来源的完整生命周期状态机，让「状态变没变」直接反映
    /// 上报是否被处理。
    fn report(&mut self, source: &str, origin: Option<api::ReportOrigin>) -> serde_json::Value {
        let (respond_to, response_rx) = std::sync::mpsc::channel();
        self.server
            .handle_api_request_with_shutdown_check(api::ApiRequestMessage {
                request: api::schema::Request {
                    id: "report".into(),
                    method: api::schema::Method::PaneReportAgent(
                        api::schema::PaneReportAgentParams {
                            pane_id: self.public_pane_id.clone(),
                            source: source.into(),
                            agent: "pi".into(),
                            state: api::schema::PaneAgentState::Working,
                            message: None,
                            seq: None,
                            agent_session_id: None,
                            agent_session_path: None,
                        },
                    ),
                },
                respond_to,
                response_write_complete: None,
                stream_active: None,
                observation_events: None,
                report_origin: origin,
            });
        let response = response_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("上报总会得到应答");
        serde_json::from_str(&response).expect("应答是 JSON")
    }

    fn state(&self) -> crate::detect::AgentState {
        self.server
            .app
            .state
            .terminals
            .get(&self.terminal_id)
            .unwrap()
            .state
    }
}

#[tokio::test]
async fn report_from_inside_the_pane_process_tree_is_applied() {
    let mut fixture = ReportFixture::new(Some(PANE_ROOT));
    let response = fixture.report("herdr:test", Some(origin(Some(inside_pane()))));
    assert_eq!(response["result"]["type"], "ok", "{response}");
    assert_eq!(fixture.state(), crate::detect::AgentState::Working);
}

#[tokio::test]
async fn report_from_a_detached_process_is_dropped_silently() {
    let mut fixture = ReportFixture::new(Some(PANE_ROOT));
    let response = fixture.report("herdr:test", Some(origin(Some(detached()))));
    assert_eq!(
        response["result"]["type"], "ok",
        "丢弃照常回成功，钩子不报错不重试: {response}"
    );
    assert_eq!(
        fixture.state(),
        crate::detect::AgentState::Idle,
        "丢弃不改状态"
    );
}

#[tokio::test]
async fn unverifiable_origins_fail_open() {
    // 父链读不到。
    let mut fixture = ReportFixture::new(Some(PANE_ROOT));
    fixture.report("herdr:test", Some(origin(None)));
    assert_eq!(fixture.state(), crate::detect::AgentState::Working);

    // 拿不到对端 pid。
    let mut fixture = ReportFixture::new(Some(PANE_ROOT));
    let no_peer = api::ReportOrigin::capture_with(None, |_| Some(detached()));
    fixture.report("herdr:test", Some(no_peer));
    assert_eq!(fixture.state(), crate::detect::AgentState::Working);

    // 窗格没有根进程。
    let mut fixture = ReportFixture::new(None);
    fixture.report("herdr:test", Some(origin(Some(detached()))));
    assert_eq!(fixture.state(), crate::detect::AgentState::Working);

    // 请求不是从 API socket 来的（没有来源快照）。
    let mut fixture = ReportFixture::new(Some(PANE_ROOT));
    fixture.report("herdr:test", None);
    assert_eq!(fixture.state(), crate::detect::AgentState::Working);
}

#[tokio::test]
async fn third_party_sources_are_not_verified() {
    let mut fixture = ReportFixture::new(Some(PANE_ROOT));
    fixture.report("custom:pi-wrapper", Some(origin(Some(detached()))));
    assert_eq!(fixture.state(), crate::detect::AgentState::Working);
}

#[tokio::test]
async fn verification_switch_off_accepts_detached_reports() {
    let mut fixture = ReportFixture::new(Some(PANE_ROOT));
    fixture.server.app.verify_report_process = false;
    fixture.report("herdr:test", Some(origin(Some(detached()))));
    assert_eq!(fixture.state(), crate::detect::AgentState::Working);
}
