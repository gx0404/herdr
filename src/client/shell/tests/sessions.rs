use super::*;
use crate::client::shell::chrome::sessions::{SessionGrouping, SessionHit, SessionRowKind};
use crate::client::shell::dock::PanelId;

const NOW_MS: u64 = 1_790_000_000_000;

fn agent(
    pane: &str,
    workspace: &str,
    tab: &str,
    name: &str,
    status: AgentStatus,
) -> ClientShellAgent {
    ClientShellAgent {
        insight: crate::protocol::ClientShellAgentInsight {
            session_title: Some(format!("{name} session")),
            state_changed_at_ms: Some(NOW_MS - 5 * 60_000),
            ..Default::default()
        },
        pane_id: pane.into(),
        workspace_id: workspace.into(),
        tab_id: tab.into(),
        name: Some(name.into()),
        display_agent: Some(name.into()),
        agent: Some(name.into()),
        title: Some(name.into()),
        terminal_title: Some(name.into()),
        terminal_title_stripped: Some(name.into()),
        agent_status: status,
        state_change_seq: 0,
        state_labels: Vec::new(),
        tokens: Vec::new(),
        focused: false,
        launch_seq: 0,
        activity: Default::default(),
    }
}
fn pane(id: &str, workspace: &str, tab: &str) -> ClientShellPane {
    ClientShellPane {
        pane_id: id.into(),
        workspace_id: workspace.into(),
        tab_id: tab.into(),
        label: None,
        cwd: Some(format!("/repo/{id}")),
        foreground_cwd: Some(format!("/repo/{id}")),
        focused: false,
        right_click_passthrough: false,
    }
}
fn sessions_snapshot() -> ClientShellSnapshot {
    let mut snapshot = snapshot();
    snapshot.revision = 2;
    let mut second = snapshot.workspaces[0].clone();
    second.workspace_id = "ws_2".into();
    second.active_tab_id = "tab_2".into();
    second.number = 2;
    second.label = "web".into();
    second.focused = false;
    snapshot.workspaces.push(second);
    let mut tab = snapshot.tabs[0].clone();
    tab.tab_id = "tab_2".into();
    tab.workspace_id = "ws_2".into();
    tab.focused = false;
    snapshot.tabs.push(tab);
    snapshot.panes.extend([
        pane("pane_2", "ws_1", "tab_1"),
        pane("pane_3", "ws_1", "tab_1"),
        pane("pane_4", "ws_2", "tab_2"),
        pane("pane_5", "ws_2", "tab_2"),
        pane("pane_6", "ws_2", "tab_2"),
    ]);
    let mut failed = agent("pane_5", "ws_2", "tab_2", "pi", AgentStatus::Idle);
    failed.insight.attention = Some(crate::protocol::ClientShellAgentAttention {
        kind: crate::api::schema::AgentAttentionKind::Failed,
        detail: Some("cargo test failed".into()),
        ..Default::default()
    });
    let mut review = agent("pane_4", "ws_2", "tab_2", "kimi", AgentStatus::Done);
    review.state_change_seq = 1;
    snapshot.agents = vec![
        agent("pane_2", "ws_1", "tab_1", "claude", AgentStatus::Idle),
        agent("pane_3", "ws_1", "tab_1", "codex", AgentStatus::Blocked),
        review,
        failed,
        agent("pane_6", "ws_2", "tab_2", "opencode", AgentStatus::Blocked),
    ];
    snapshot
}
fn ready_with(snapshot: ClientShellSnapshot, cols: u16, rows: u16) -> ClientShellState {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.wall_clock_override_ms = Some(NOW_MS);
    let mut working = snapshot.clone();
    working.revision = snapshot.revision.saturating_sub(1);
    for agent in &mut working.agents {
        if agent.agent_status == AgentStatus::Done {
            agent.agent_status = AgentStatus::Working;
            agent.state_change_seq = agent.state_change_seq.saturating_sub(1);
        }
    }
    state.set_snapshot(Box::new(working));
    state.set_snapshot(Box::new(snapshot));
    state.set_endpoint_methods(Some(vec![
        "client.views.set".into(),
        "tab.focus".into(),
        "pane.focus".into(),
        "workspace.focus".into(),
        "workspace.create".into(),
    ]));
    state.set_pane_surface(surface());
    state.compose(cols, rows).expect("初始画面");
    state.tick_workbench(std::time::Instant::now(), &mut ClientShellInput::default());
    state.workbench.pending = false;
    state.workbench.acknowledged = state.workbench.revision;
    state.compose(cols, rows).expect("工作台画面");
    state
}
fn ready() -> ClientShellState {
    ready_with(sessions_snapshot(), 120, 40)
}
fn click(state: &mut ClientShellState, rect: Rect) -> ClientShellInput {
    let down = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: rect.x,
        row: rect.y,
        modifiers: KeyModifiers::NONE,
    };
    let outcome = state.handle_raw_events(vec![RawInputEvent::Mouse(down)]);
    state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Up(MouseButton::Left),
        ..down
    })]);
    outcome
}
fn methods(outcome: &ClientShellInput) -> Vec<&crate::api::schema::Method> {
    outcome
        .actions
        .iter()
        .filter_map(|action| match action {
            ClientShellAction::Endpoint { request, .. }
            | ClientShellAction::EndpointRequest { request, .. } => Some(&request.method),
            _ => None,
        })
        .collect()
}
fn focused_pane_request(outcome: &ClientShellInput) -> Option<String> {
    methods(outcome)
        .into_iter()
        .find_map(|method| match method {
            crate::api::schema::Method::PaneFocus(target) => Some(target.pane_id.clone()),
            _ => None,
        })
}
fn session_hit(state: &ClientShellState, wanted: &SessionHit) -> Rect {
    state
        .hits
        .sessions
        .iter()
        .find(|(_, hit)| hit == wanted)
        .map(|(rect, _)| *rect)
        .unwrap_or_else(|| panic!("没有画出 {wanted:?}: {:?}", state.hits.sessions))
}
fn row_labels(state: &ClientShellState) -> Vec<String> {
    state
        .session_rows
        .as_ref()
        .expect("行缓存")
        .rows
        .iter()
        .map(|row| match &row.kind {
            SessionRowKind::MachineGroup { label } => format!("group:{label}"),
            SessionRowKind::Machine { label, .. } => format!("machine:{label}"),
            SessionRowKind::Workspace { label, .. } => format!("ws:{label}"),
            SessionRowKind::Tab { label, .. } => format!("tab:{label}"),
            SessionRowKind::Agent(agent) => format!("agent:{}", agent.name),
            SessionRowKind::Terminal { title, .. } => format!("term:{title}"),
            SessionRowKind::Activity(..) => "activity".into(),
            SessionRowKind::ExternalGroup { label, .. } => format!("external:{label}"),
            SessionRowKind::External { label, .. } => format!("ext:{label}"),
            SessionRowKind::Section { label, .. } => format!("section:{label}"),
            SessionRowKind::MobileAction { label, .. } => format!("action:{label}"),
            SessionRowKind::Empty(text) => format!("empty:{text}"),
        })
        .collect()
}
fn screen_text(state: &mut ClientShellState, cols: u16, rows: u16) -> String {
    let frame = state.compose(cols, rows).expect("画面").frame.clone();
    frame_rows(&frame).join("\n")
}
#[test]
fn by_workspace_lists_every_pane_under_its_workspace() {
    let _lang = crate::i18n::lang_guard(crate::i18n::Lang::En);
    let state = ready();
    assert_eq!(
        row_labels(&state),
        [
            "ws:client-shell",
            "term:repo",
            "agent:claude",
            "agent:codex",
            "ws:web",
            "agent:kimi",
            "agent:pi",
            "agent:opencode"
        ]
    );
}
#[test]
fn by_status_lists_sections_in_listing_order_and_hides_empty_ones() {
    let _lang = crate::i18n::lang_guard(crate::i18n::Lang::En);
    let mut state = ready();
    state.session_grouping = SessionGrouping::Status;
    state.compose(120, 40).expect("画面");
    assert_eq!(
        row_labels(&state),
        [
            "section:Needs input · 2",
            "agent:codex",
            "agent:opencode",
            "section:Failed · 1",
            "agent:pi",
            "section:Review · 1",
            "agent:kimi",
            "section:Idle · 1",
            "agent:claude",
            "section:Other · 1",
            "term:repo · client-shell"
        ],
        "需要输入 → 失败 → 待查看 → 运行中 → 空闲；没有运行中的分区就不画"
    );
    let rows = &state.session_rows.as_ref().expect("行缓存").rows;
    let SessionRowKind::Agent(codex) = &rows[1].kind else {
        panic!("第二行是 agent");
    };
    assert_eq!(
        codex.location.as_deref(),
        Some("client-shell"),
        "按状态分组时带上工作区"
    );
}
#[test]
fn grouping_toggle_click_switches_grouping_and_persists_per_endpoint() {
    let _lang = crate::i18n::lang_guard(crate::i18n::Lang::En);
    let path = std::env::temp_dir().join(format!(
        "herdr-sessions-grouping-{}-{}.json",
        std::process::id(),
        crate::config::test_dirs::unique_id()
    ));
    let _ = std::fs::remove_file(&path);
    let mut state = ready();
    state.config.preferences_path = Some(path.clone());
    let toggle = session_hit(&state, &SessionHit::Grouping);
    click(&mut state, toggle);
    assert_eq!(state.session_grouping, SessionGrouping::Status);
    state.flush_chrome_preferences(&mut ClientShellInput::default());
    let saved = std::fs::read_to_string(&path).expect("偏好已写盘");
    assert!(
        saved.contains("\"session_grouping\": \"status\""),
        "{saved}"
    );
    let loaded = super::super::preferences::load(&path).expect("读回偏好");
    assert_eq!(loaded.session_grouping, Some(SessionGrouping::Status));
    let mut config = ClientShellConfig::from_config(&Config::default());
    config.preferences = loaded;
    let restored = ClientShellState::new(config);
    assert_eq!(
        restored.session_grouping,
        SessionGrouping::Status,
        "重启后按偏好恢复"
    );
    let lenient: super::super::preferences::ClientChromePreferences =
        serde_json::from_str(r#"{"session_grouping":"sideways","sidebar_width":30}"#)
            .expect("未知分组值不让整份偏好失效");
    assert_eq!(lenient.session_grouping, None);
    assert_eq!(lenient.sidebar_width, Some(30));
    let _ = std::fs::remove_file(&path);
}
#[test]
fn command_palette_toggles_grouping() {
    let mut state = ready();
    state.open_command_search();
    state.compose(120, 40).expect("命令面板");
    palette_select(&mut state, "sessions:grouping");
    state.compose(120, 40).expect("grouping action revealed");
    state.handle_input_bytes(b"\r");
    assert_eq!(state.session_grouping, SessionGrouping::Status);
}
#[test]
fn next_attention_cycles_needs_input_then_failed_then_review() {
    let mut state = ready();
    let mut visited = Vec::new();
    for _ in 0..5 {
        let mut outcome = ClientShellInput::default();
        assert!(state.focus_next_attention(&mut outcome));
        let target = focused_pane_request(&outcome).expect("聚焦请求");
        visited.push(target.clone());
        let mut snapshot = state.snapshot.as_deref().cloned().expect("snapshot");
        snapshot.revision += 1;
        snapshot.focused_pane_id = Some(target);
        state.set_snapshot(Box::new(snapshot));
    }
    assert_eq!(
        visited,
        ["pane_3", "pane_6", "pane_5", "pane_4", "pane_3"],
        "codex、opencode（需要输入）→ pi（失败）→ kimi（待查看）→ 回到开头"
    );
}
#[test]
fn next_attention_key_is_bound_to_prefix_a_and_reports_when_nothing_waits() {
    let mut state = ready();
    let bindings = state.config.keybinds.keybinds.next_attention.labels();
    assert_eq!(bindings, ["prefix+a"]);
    state.handle_input_bytes(b"\x02");
    let outcome = state.handle_input_bytes(b"a");
    assert_eq!(
        focused_pane_request(&outcome).as_deref(),
        Some("pane_3"),
        "prefix+a 跳到第一个需要输入的会话"
    );
    let mut quiet = sessions_snapshot();
    for agent in &mut quiet.agents {
        agent.agent_status = AgentStatus::Idle;
        agent.insight.attention = None;
    }
    let mut state = ready_with(quiet, 120, 40);
    let mut outcome = ClientShellInput::default();
    assert!(!state.focus_next_attention(&mut outcome));
    assert!(methods(&outcome).is_empty());
}
#[test]
fn clicking_session_rows_focuses_their_panes() {
    let mut state = ready();
    let terminal = session_hit(
        &state,
        &SessionHit::Terminal(ClientEndpointId::Local, "pane_1".into()),
    );
    let outcome = click(&mut state, terminal);
    assert_eq!(
        focused_pane_request(&outcome).as_deref(),
        Some("pane_1"),
        "点击终端行聚焦它"
    );
    let codex = state
        .hits
        .session_agents
        .iter()
        .find(|(_, _, pane)| pane == "pane_3")
        .map(|(rect, _, _)| *rect)
        .expect("codex 行");
    let outcome = click(&mut state, codex);
    assert_eq!(
        focused_pane_request(&outcome).as_deref(),
        Some("pane_3"),
        "点击 agent 行聚焦它"
    );
}
#[test]
fn workspace_chevron_collapses_its_sessions_and_shares_the_tree_key() {
    let mut state = ready();
    let key = super::super::agent_sidebar::agent_group_key("ws_2");
    let chevron = session_hit(
        &state,
        &SessionHit::Toggle(ClientEndpointId::Local, key.clone()),
    );
    click(&mut state, chevron);
    state.compose(120, 40).expect("画面");
    assert!(
        state.collapsed_groups.contains(&key),
        "与 Agents 面板同一折叠键"
    );
    let labels = row_labels(&state);
    assert!(labels.contains(&"ws:web".to_owned()));
    assert!(
        !labels.iter().any(|label| label == "agent:kimi"),
        "收起后不列会话：{labels:?}"
    );
    let rows = &state.session_rows.as_ref().expect("行缓存").rows;
    let rollup = rows.iter().find_map(|row| match &row.kind {
        SessionRowKind::Workspace { label, rollup, .. } if label == "web" => *rollup,
        _ => None,
    });
    assert_eq!(
        rollup.map(|rollup| (rollup.bucket, rollup.count)),
        Some((crate::harness::AgentBucket::NeedsInput, 1))
    );
}
#[test]
fn overview_header_and_footer_controls_open_their_targets() {
    let mut state = ready();
    let overview = session_hit(&state, &SessionHit::Overview);
    click(&mut state, overview);
    assert!(matches!(
        state.overlay,
        Some(ClientShellOverlay::Navigator(_))
    ));
    state.overlay = None;
    state.compose(120, 40).expect("画面");
    let settings = session_hit(&state, &SessionHit::Settings);
    click(&mut state, settings);
    assert!(matches!(
        state.overlay,
        Some(ClientShellOverlay::Settings(_))
    ));
    state.overlay = None;
    state.compose(120, 40).expect("画面");
    let help = session_hit(&state, &SessionHit::Help);
    click(&mut state, help);
    assert!(matches!(state.overlay, Some(ClientShellOverlay::Help(_))));
    state.overlay = None;
    state.compose(120, 40).expect("画面");
    let search = session_hit(&state, &SessionHit::Search);
    click(&mut state, search);
    assert!(
        matches!(state.overlay, Some(ClientShellOverlay::Navigator(_))),
        "面板头的 ⌕ 打开会话导航"
    );
    state.overlay = None;
    state.compose(120, 40).expect("画面");
    let new = session_hit(&state, &SessionHit::NewWorkspace);
    let outcome = click(&mut state, new);
    assert!(
        methods(&outcome)
            .iter()
            .any(|method| matches!(method, crate::api::schema::Method::WorkspaceCreate(_)))
            || matches!(
                state.overlay,
                Some(ClientShellOverlay::Rename(ClientRenameOverlay {
                    target: ClientRenameTarget::NewWorkspace { .. },
                    ..
                }))
            ),
        "面板头的 ✚ 新建工作区"
    );
}
#[test]
fn session_rows_render_summary_age_and_rollup_without_inner_label_rows() {
    let _lang = crate::i18n::lang_guard(crate::i18n::Lang::En);
    let mut state = ready();
    let text = screen_text(&mut state, 120, 40);
    assert!(text.contains("Sessions"), "{text}");
    let overview = session_hit(&state, &SessionHit::Overview);
    let label = (overview.x..overview.right())
        .map(|x| state.compose_buffer.as_ref().expect("buffer")[(x, overview.y)].symbol())
        .collect::<String>();
    assert!(
        label.contains("Agent over") && label.contains('…'),
        "窄面板里截短带省略号：{label}"
    );
    assert!(text.contains("By workspace ▾"), "{text}");
    assert!(text.contains("5m"), "时长按分钟取整：{text}");
    assert!(
        !text.contains("Machines"),
        "不再画重复的「机器」标签行：{text}"
    );
}
#[test]
fn ages_change_at_most_once_per_minute() {
    let mut state = ready();
    let rebuilt = |state: &ClientShellState| {
        state
            .session_rows
            .as_ref()
            .map(|cache| cache.rows.as_ptr() as usize)
    };
    let before = rebuilt(&state);
    let now = std::time::Instant::now();
    state.wall_clock_override_ms = Some(NOW_MS + 20_000);
    assert!(!state.tick_chrome_feedback(now), "同一分钟内不为时长重画");
    state.compose(120, 40).expect("画面");
    assert_eq!(rebuilt(&state), before, "同一分钟内复用行缓存");
    state.wall_clock_override_ms = Some(NOW_MS + 61_000);
    assert!(state.tick_chrome_feedback(now), "跨分钟重画一次");
    state.compose(120, 40).expect("画面");
    assert_ne!(rebuilt(&state), before, "跨分钟才重建");
    assert!(!state.tick_chrome_feedback(now), "重建后同一分钟内不再重画");
}
fn profile(
    label: &str,
    seed: char,
    group: Option<&str>,
    color: Option<&str>,
) -> crate::client::endpoint::SavedSshEndpoint {
    crate::client::endpoint::SavedSshEndpoint {
        id: crate::client::endpoint::ProfileId::parse(seed.to_string().repeat(32))
            .expect("profile id"),
        label: label.into(),
        target: format!("dev@{label}"),
        session: "agents".into(),
        enabled: true,
        group: group.map(str::to_owned),
        color: color.map(str::to_owned),
        ..crate::client::endpoint::SavedSshEndpoint::new("base", "base", "default")
            .expect("valid base profile")
    }
}
#[test]
fn federated_machines_keep_catalog_groups_tags_and_connecting_rows() {
    use crate::client::shell::chrome::sessions::MachineTag;
    let mut state = ready();
    let build = profile("build", 'a', Some("lab"), Some("#ff8800"));
    let edge = profile("edge", 'b', None, None);
    let gpu = profile("gpu", 'c', Some("lab"), None);
    let build_id = ClientEndpointId::Ssh(build.id.clone());
    state.set_endpoint_catalog(&[build, edge, gpu]);
    state.set_endpoint_status(&build_id, ClientEndpointStatus::Online);
    let mut remote = snapshot();
    remote.boot_id = "build-boot".into();
    state.set_endpoint_snapshot(&build_id, Box::new(remote));
    state.compose(120, 40).expect("画面");
    let labels = row_labels(&state);
    let tail = &labels[labels
        .iter()
        .position(|label| label == "group:lab")
        .expect("目录分组头")..];
    assert_eq!(
        tail,
        [
            "group:lab",
            "machine:build",
            "ws:client-shell",
            "term:repo",
            "machine:gpu",
            "machine:edge"
        ],
        "同组机器排在组头下（组内保持顺序），连接中还没有快照的机器只有机器行"
    );
    assert_eq!(labels[0], "machine:Local", "多台机器时本机也有机器行");
    let tags = state
        .session_rows
        .as_ref()
        .expect("行缓存")
        .rows
        .iter()
        .filter_map(|row| match &row.kind {
            SessionRowKind::Machine { label, tag, .. } => Some((label.clone(), *tag)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        tags,
        [
            ("Local".to_owned(), None),
            (
                "build".to_owned(),
                Some(MachineTag::Color(ratatui::style::Color::Rgb(
                    0xff, 0x88, 0x00
                )))
            ),
            ("gpu".to_owned(), Some(MachineTag::Hashed)),
            ("edge".to_owned(), Some(MachineTag::Hashed))
        ]
    );
    assert_eq!(state.hits.machines.len(), 4, "每台机器一个可点的机器行");
}
#[test]
fn dropping_after_the_last_workspace_draws_below_its_sessions() {
    let mut state = ready();
    let last = state
        .hits
        .workspaces
        .iter()
        .find(|hit| hit.workspace_id == "ws_2")
        .map(|hit| hit.rect)
        .expect("最后一个工作区行");
    let last_session = state
        .hits
        .session_agents
        .iter()
        .find(|(_, _, pane)| pane == "pane_6")
        .map(|(rect, _, _)| rect.y)
        .expect("最后一个会话行");
    state.chrome_drag = Some(ClientChromeDrag::Workspace {
        source_workspace_id: "ws_1".into(),
        target: Some((None, last.bottom())),
    });
    state.compose(120, 40).expect("拖动中");
    assert_eq!(
        state.session_view.drop_row,
        Some(last_session),
        "「放到最后」的指示线画在最后一个工作区的会话之后，不压在它的会话之间"
    );
    let first = state
        .hits
        .workspaces
        .iter()
        .find(|hit| hit.workspace_id == "ws_1")
        .map(|hit| hit.rect)
        .expect("第一个工作区行");
    state.chrome_drag = Some(ClientChromeDrag::Workspace {
        source_workspace_id: "ws_2".into(),
        target: Some((Some("ws_1".into()), first.y - 1)),
    });
    let frame = state.compose(120, 40).expect("拖动中").frame.clone();
    assert_eq!(state.session_view.drop_row, Some(first.y - 1));
    assert!(
        frame_rows(&frame)[usize::from(first.y - 1)].contains("────"),
        "放到最前时指示线画在分组行上"
    );
}
#[test]
fn narrow_terminal_collapses_the_sidebar_into_a_rail() {
    let mut state = ready_with(sessions_snapshot(), 90, 30);
    assert!(state.workbench.geometry.rail, "窄于 100 列收成图标栏");
    assert!(state.session_view.rail);
    let panel = state
        .workbench
        .geometry
        .panels
        .iter()
        .find(|(panel, _)| *panel == PanelId::Workspaces)
        .map(|(_, area)| *area)
        .expect("会话面板");
    assert_eq!(panel.width, 4);
    assert!(
        !state
            .workbench
            .geometry
            .panels
            .iter()
            .any(|(panel, _)| *panel == PanelId::Agents),
        "图标栏模式不投影同栏的 Agents 面板"
    );
    let rail = session_hit(
        &state,
        &SessionHit::Rail(ClientEndpointId::Local, "ws_2".into()),
    );
    let outcome = click(&mut state, rail);
    assert!(
        methods(&outcome).iter().any(|method| matches!(method,
        crate::api::schema::Method::WorkspaceFocus(target) if target.workspace_id == "ws_2")),
        "点击图标栏的工作区切换过去：{:?}",
        methods(&outcome)
    );
    let wide = ready_with(sessions_snapshot(), 120, 40);
    assert!(!wide.workbench.geometry.rail, "宽屏照常显示会话面板");
}
#[test]
fn navigation_reveal_scrolls_the_selected_workspace_into_view() {
    let mut snapshot = sessions_snapshot();
    for index in 3..=12 {
        let mut workspace = snapshot.workspaces[1].clone();
        workspace.workspace_id = format!("ws_{index}");
        workspace.number = index;
        workspace.label = format!("project-{index}");
        snapshot.workspaces.push(workspace);
    }
    let mut state = ready_with(snapshot, 120, 24);
    assert_eq!(state.workspace_scroll, 0);
    state.mode = ClientShellMode::Navigate;
    let mut target = state.focused_navigation_target().expect("导航目标");
    target.workspace_id = "ws_12".into();
    state.navigate_workspace_id = Some(target);
    state.reveal_navigation_workspace = true;
    state.compose(120, 24).expect("画面");
    assert!(
        !state.reveal_navigation_workspace,
        "reveal 在视图计算阶段消费"
    );
    assert!(state.workspace_scroll > 0, "滚到选中的工作区");
    assert!(
        state
            .hits
            .workspaces
            .iter()
            .any(|hit| hit.workspace_id == "ws_12"),
        "选中的工作区可见"
    );
}
fn assert_request_geometry(state: &mut ClientShellState, cols: u16, rows: u16) {
    state.compose(cols, rows).expect("compute geometry");
    state.workbench.pending = false;
    state.tick_workbench(
        std::time::Instant::now()
            + std::time::Duration::from_secs(state.workbench.revision.saturating_mul(2) + 1),
        &mut ClientShellInput::default(),
    );
    for request in &state.workbench.requested {
        let panel = PanelId::Terminal(request.view_id.parse().expect("view id"));
        let area = state
            .workbench
            .geometry
            .panels
            .iter()
            .find(|(id, _)| *id == panel)
            .map(|(_, area)| *area)
            .expect("visible terminal panel");
        let body = super::super::workbench::body(area, &panel, state.workbench.geometry.compact);
        assert_eq!((request.cols, request.rows), (body.width, body.height));
        if request.focused {
            assert_eq!(state.layout(cols, rows).pane_surface, body);
            assert_eq!(
                (
                    state.surface_size(cols, rows).cols,
                    state.surface_size(cols, rows).rows
                ),
                (body.width, body.height)
            );
        }
    }
    assert!(!state.workbench.requested.is_empty());
}
#[test]
fn view_requests_follow_rail_body_across_narrow_wide_switches() {
    let mut state = ready_with(sessions_snapshot(), 90, 30);
    let dock = state.workbench.dock.clone();
    for (cols, rows, rail) in [
        (90, 30, true),
        (160, 50, false),
        (99, 30, true),
        (100, 30, false),
    ] {
        assert_request_geometry(&mut state, cols, rows);
        assert_eq!(state.workbench.geometry.rail, rail);
        if rail {
            assert_eq!(state.workbench.requested[0].cols, cols - 4);
        }
    }
    assert_eq!(state.workbench.dock, dock, "rail 不改保存的比例与布局");
}
#[test]
fn custom_dock_requests_keep_the_actual_body_and_compact_fallback() {
    use super::super::dock::Edge;
    let mut state = ready_with(sessions_snapshot(), 90, 30);
    assert!(state
        .workbench
        .dock
        .dock(PanelId::Workspaces, &PanelId::Terminal(1), Edge::Bottom));
    state.workbench.dock.focused = PanelId::Terminal(1);
    for (cols, rows) in [(90, 30), (160, 50), (30, 12)] {
        assert_request_geometry(&mut state, cols, rows);
        assert!(!state.workbench.geometry.rail, "非左栏布局不强行收成 rail");
    }
    assert!(state.workbench.geometry.compact);
    state.workbench.dock.maximized = Some(PanelId::Terminal(1));
    assert_request_geometry(&mut state, 90, 30);
    assert!(!state.workbench.geometry.rail);
    assert_eq!(state.workbench.requested[0].cols, 90);
}
#[test]
fn cache_invalidates_each_epoch_and_endpoint_identity_but_not_paint_inputs() {
    let mut state = ready();
    let pointer =
        |state: &ClientShellState| state.session_rows.as_ref().expect("cache").rows.as_ptr();
    for invalidate in [
        (|state: &mut ClientShellState| state.config_epoch += 1) as fn(&mut ClientShellState),
        |state| state.agent_rows_epoch += 1,
        |state| state.tree_collapse_epoch += 1,
        |state| state.agent_activity_epoch += 1,
        |state| state.endpoints[0].label.push('!'),
        |state| state.endpoints[0].status = ClientEndpointStatus::Reconnecting,
        |state| state.session_grouping = SessionGrouping::Status,
        |state| {
            state.active_endpoint_id = ClientEndpointId::Ssh(profile("other", 'd', None, None).id)
        },
    ] {
        let before = pointer(&state);
        invalidate(&mut state);
        state.refresh_session_rows();
        assert_ne!(pointer(&state), before);
        let stable = pointer(&state);
        state.refresh_session_rows();
        assert_eq!(pointer(&state), stable);
    }
    let mut state = ready();
    for change_boot in [false, true] {
        let before = pointer(&state);
        let mut snapshot = state.snapshot.as_deref().cloned().expect("snapshot");
        if change_boot {
            snapshot.boot_id.push('!');
        } else {
            snapshot.revision += 1;
        }
        state.set_snapshot(Box::new(snapshot));
        state.refresh_session_rows();
        assert_ne!(pointer(&state), before);
    }
    let stable = pointer(&state);
    state.workspace_scroll = 1;
    state.compose(160, 45).expect("resize");
    assert_eq!(pointer(&state), stable, "尺寸、滚动与绘制不重建数据行");
}
#[test]
fn worktree_children_nest_and_parent_collapse_rolls_up_the_group() {
    let mut snapshot = sessions_snapshot();
    snapshot.workspaces[0].worktree = Some(ClientShellWorktree {
        key: "repo".into(),
        label: "repo".into(),
        is_linked_worktree: false,
    });
    snapshot.workspaces[1].worktree = Some(ClientShellWorktree {
        key: "repo".into(),
        label: "repo".into(),
        is_linked_worktree: true,
    });
    snapshot.workspaces[1].custom_label = false;
    snapshot.workspaces[1].branch = Some("worktree/feature".into());
    let mut state = ready_with(snapshot, 160, 45);
    assert!(state
        .hits
        .workspaces
        .iter()
        .any(|hit| hit.workspace_id == "ws_2" && hit.indented));
    assert!(row_labels(&state).contains(&"ws:feature".to_owned()));
    let toggle = session_hit(
        &state,
        &SessionHit::Toggle(ClientEndpointId::Local, "repo".into()),
    );
    click(&mut state, toggle);
    state.compose(160, 45).expect("collapsed");
    assert!(state.collapsed_groups.contains("repo"));
    assert!(!state
        .hits
        .workspaces
        .iter()
        .any(|hit| hit.workspace_id == "ws_2"));
    assert!(!row_labels(&state)
        .iter()
        .any(|label| label.starts_with("agent:")));
    let rows = &state.session_rows.as_ref().expect("cache").rows;
    assert!(rows.iter().any(|row| matches!(&row.kind, SessionRowKind::Workspace { rollup: Some(rollup), .. } if rollup.bucket == crate::harness::AgentBucket::NeedsInput && rollup.count == 2)));
}
fn running_activity() -> crate::protocol::ClientShellAgentActivity {
    crate::protocol::ClientShellAgentActivity {
        running: 1,
        total: 3,
        active: 1,
        done: 1,
        failed: 1,
        nodes: vec![crate::protocol::ClientShellActivityNode {
            id: "child".into(),
            label: "Explore layout".into(),
            status: crate::api::schema::AgentActivityStatus::Running,
            ..Default::default()
        }],
        ..Default::default()
    }
}
#[test]
fn status_grouping_retains_activity_clicks_and_plain_terminals() {
    let _lang = crate::i18n::lang_guard(crate::i18n::Lang::En);
    let mut snapshot = sessions_snapshot();
    snapshot.agents[1].activity = running_activity();
    let mut state = ready_with(snapshot, 160, 50);
    state.set_endpoint_methods(Some(vec!["agent.activity.read".into()]));
    state.session_grouping = SessionGrouping::Status;
    state.toggle_collapsed_group(
        &ClientEndpointId::Local,
        "agent-activity:pane:pane_3".into(),
    );
    state.compose(160, 50).expect("activity rows");
    let activity = state
        .hits
        .session_activity_rows
        .iter()
        .find(|hit| hit.node_id == "child")
        .expect("child hit")
        .rect;
    let outcome = click(&mut state, activity);
    assert!(matches!(
        state.overlay,
        Some(ClientShellOverlay::AgentActivity(_))
    ));
    assert!(methods(&outcome).iter().any(|method| matches!(method, crate::api::schema::Method::AgentActivityRead(params) if params.pane_id.as_deref() == Some("pane_3"))));
    state.overlay = None;
    state.compose(160, 50).expect("sessions");
    assert!(row_labels(&state)
        .iter()
        .any(|label| label.starts_with("term:")));
    assert!(
        row_labels(&state).contains(&"section:Needs input · 2".to_owned()),
        "活动行不计入会话分区数量"
    );
}
#[test]
fn external_activity_expands_in_both_groupings_without_fabricating_pane_focus() {
    let mut snapshot = sessions_snapshot();
    snapshot
        .external_agents
        .push(crate::protocol::ClientShellExternalAgent {
            external_id: "external:1".into(),
            source: "legacy".into(),
            agent_status: AgentStatus::Working,
            label: "external worker".into(),
            readable: true,
            agent: None,
            cwd: None,
            updated_at_ms: Some(NOW_MS - 60_000),
            activity: running_activity(),
        });
    let mut state = ready_with(snapshot, 160, 50);
    state.set_endpoint_methods(Some(vec![
        "agent.activity.read".into(),
        "pane.focus".into(),
    ]));
    let key = "agent-activity:ext:external:1".to_owned();
    for grouping in [SessionGrouping::Workspace, SessionGrouping::Status] {
        state.session_grouping = grouping;
        state.compose(160, 50).expect("sessions");
        if !state.collapsed_groups.contains(&key) {
            let toggle = session_hit(
                &state,
                &SessionHit::Toggle(ClientEndpointId::Local, key.clone()),
            );
            click(&mut state, toggle);
            state.compose(160, 50).expect("expanded");
        }
        assert!(state
            .hits
            .session_activity_rows
            .iter()
            .any(|hit| hit.owner_key == "ext:external:1"));
        let external = state
            .hits
            .session_external_agents
            .iter()
            .find(|(_, _, id)| id == "external:1")
            .expect("external hit")
            .0;
        let outcome = click(&mut state, external);
        assert!(focused_pane_request(&outcome).is_none());
        assert!(methods(&outcome).iter().any(|method| matches!(method, crate::api::schema::Method::AgentActivityRead(params) if params.external_id.as_deref() == Some("external:1") && params.pane_id.is_none())));
        state.overlay = None;
    }
    let counts = state.session_rows.as_ref().expect("cache").counts;
    assert_eq!(
        counts.working, 0,
        "总览与 a 只计可聚焦的在线 pane agents，不计外部来源"
    );
    assert!(
        !super::super::chrome::sessions::attention_targets(&state.endpoints)
            .iter()
            .any(|(_, id, _)| id == "external:1")
    );
    let group = super::super::agent_tree::external_group_key("legacy");
    state.session_grouping = SessionGrouping::Workspace;
    state.toggle_collapsed_group(&ClientEndpointId::Local, group);
    state.compose(160, 50).expect("source collapsed");
    assert!(state.hits.session_external_agents.is_empty());
}
#[test]
fn wheel_scroll_and_scrollbar_track_and_drag_keep_session_rows_reachable() {
    let mut snapshot = sessions_snapshot();
    for index in 3..=30 {
        let mut workspace = snapshot.workspaces[1].clone();
        workspace.workspace_id = format!("ws_{index}");
        workspace.number = index;
        workspace.label = format!("project-{index}");
        snapshot.workspaces.push(workspace);
    }
    let mut state = ready_with(snapshot, 160, 30);
    let body = state.hits.workspace_body;
    let mouse = |kind, x, y| {
        RawInputEvent::Mouse(MouseEvent {
            kind,
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        })
    };
    let outcome = state.handle_raw_events(vec![mouse(MouseEventKind::ScrollDown, body.x, body.y)]);
    assert!(outcome.repaint);
    assert!(state.workspace_scroll > 0);
    state.compose(160, 30).expect("scrolled");
    let track = state.hits.workspace_scrollbar;
    assert!(!track.is_empty());
    click(&mut state, Rect::new(track.x, track.bottom() - 1, 1, 1));
    state.compose(160, 30).expect("track clicked");
    assert!(state
        .hits
        .workspaces
        .iter()
        .any(|hit| hit.workspace_id == "ws_30"));
    let metrics = state.hits.workspace_scroll_metrics.expect("metrics");
    let thumb = crate::ui::scrollbar_thumb(metrics, state.hits.workspace_scrollbar).expect("thumb");
    state.handle_raw_events(vec![mouse(
        MouseEventKind::Down(MouseButton::Left),
        track.x,
        thumb.top,
    )]);
    assert!(matches!(
        state.chrome_drag,
        Some(ClientChromeDrag::WorkspaceScrollbar { .. })
    ));
    state.handle_raw_events(vec![mouse(
        MouseEventKind::Drag(MouseButton::Left),
        track.x,
        track.y,
    )]);
    state.handle_raw_events(vec![mouse(
        MouseEventKind::Up(MouseButton::Left),
        track.x,
        track.y,
    )]);
    state.compose(160, 30).expect("dragged to top");
    assert_eq!(state.workspace_scroll, 0);
    state.compose(90, 20).expect("rail");
    let rail = state.hits.workspace_body;
    let outcome = state.handle_raw_events(vec![mouse(MouseEventKind::ScrollDown, rail.x, rail.y)]);
    assert!(outcome.repaint);
    state.compose(90, 20).expect("rail scrolled");
    assert!(state.workspace_scroll > 0);
    assert!(state
        .hits
        .sessions
        .iter()
        .any(|(_, hit)| matches!(hit, SessionHit::Rail(_, id) if id == "ws_15")));
}
#[test]
fn attention_a_activates_remote_panes_and_ignores_offline_cached_agents() {
    let mut snapshot = sessions_snapshot();
    for agent in &mut snapshot.agents {
        agent.agent_status = AgentStatus::Idle;
        agent.insight.attention = None;
    }
    let mut state = ready_with(snapshot, 160, 50);
    let build = profile("build", 'a', None, None);
    let remote_id = ClientEndpointId::Ssh(build.id.clone());
    state.set_endpoint_catalog(&[build]);
    state.set_endpoint_status(&remote_id, ClientEndpointStatus::Online);
    let mut remote = sessions_snapshot();
    remote.boot_id = "remote-boot".into();
    state.set_endpoint_snapshot(&remote_id, Box::new(remote));
    state.compose(160, 50).expect("federated sessions");
    state.handle_input_bytes(b"\x02");
    let outcome = state.handle_input_bytes(b"a");
    assert!(outcome.actions.iter().any(|action| matches!(action, ClientShellAction::ActivateEndpoint { endpoint_id, target: Some(ClientEndpointFocusTarget::Pane(pane_id)) } if endpoint_id == &remote_id && pane_id == "pane_3")));
    assert!(state.activate_endpoint_projection(&ClientEndpointId::Local));
    state.set_endpoint_status(&remote_id, ClientEndpointStatus::Reconnecting);
    state.compose(160, 50).expect("offline cache");
    let remote_rows = &state.session_rows.as_ref().expect("cache").rows;
    assert!(remote_rows.iter().any(|row| row.endpoint_id == remote_id
        && row.stale
        && matches!(row.kind, SessionRowKind::Agent(_))));
    let counts = state.session_rows.as_ref().expect("cache").counts;
    assert_eq!(counts.needs_input + counts.failed + counts.review, 0);
    let mut outcome = ClientShellInput::default();
    assert!(!state.focus_next_attention(&mut outcome));
    assert!(outcome.actions.is_empty());
    let agent = state
        .hits
        .session_agents
        .iter()
        .find(|(_, id, pane)| id == &remote_id && pane == "pane_3")
        .expect("cached agent row")
        .0;
    let outcome = click(&mut state, agent);
    assert!(focused_pane_request(&outcome).is_none());
    assert!(!outcome
        .actions
        .iter()
        .any(|action| matches!(action, ClientShellAction::ActivateEndpoint { .. })));
}
fn classic_ready(cols: u16, rows: u16) -> ClientShellState {
    let mut state = ready();
    state.workbench.disconnect();
    state.set_endpoint_methods(Some(vec![
        "pane.focus".into(),
        "workspace.focus".into(),
        "tab.focus".into(),
        "agent.activity.read".into(),
    ]));
    let mut frame = surface();
    frame.projection_revision = state.snapshot.as_ref().expect("snapshot").revision;
    state.set_pane_surface(frame);
    state.compose(cols, rows).expect("old server pane surface");
    assert!(!state.workbench.enabled);
    state
}
#[test]
fn old_server_classic_uses_session_view_hits_and_prefix_attention_without_views_set() {
    let mut state = classic_ready(160, 45);
    let layout = state.layout(160, 45);
    assert_eq!(
        state.session_view.sidebar_divider.x,
        layout.sidebar.right() - 1
    );
    assert_eq!(state.hits.workspace_body, state.session_view.body);
    assert!(state
        .hits
        .session_agents
        .iter()
        .all(|(rect, _, _)| rect.intersection(state.session_view.body) == *rect));
    let terminal = session_hit(
        &state,
        &SessionHit::Terminal(ClientEndpointId::Local, "pane_1".into()),
    );
    assert_eq!(
        focused_pane_request(&click(&mut state, terminal)).as_deref(),
        Some("pane_1")
    );
    state.handle_input_bytes(b"\x02");
    let outcome = state.handle_input_bytes(b"a");
    assert_eq!(focused_pane_request(&outcome).as_deref(), Some("pane_3"));
    assert!(!methods(&outcome)
        .iter()
        .any(|method| matches!(method, crate::api::schema::Method::ClientViewsSet(_))));
}
#[test]
fn mobile_portrait_landscape_share_cached_rows_cjk_targets_and_navigation_reveal() {
    let _lang = crate::i18n::lang_guard(crate::i18n::Lang::ZhCn);
    let mut state = classic_ready(160, 45);
    state.config.mobile_width_threshold = 80;
    let mut snapshot = state.snapshot.as_deref().cloned().expect("snapshot");
    snapshot.workspaces[1].label = "国际化工作区分支示例".into();
    snapshot.revision += 1;
    let mut frame = surface();
    frame.projection_revision = snapshot.revision;
    state.set_snapshot(Box::new(snapshot));
    state.set_pane_surface(frame);
    state.refresh_session_rows();
    let pointer = state.session_rows.as_ref().expect("cache").rows.as_ptr();
    for (cols, rows) in [(44, 40), (72, 24)] {
        state.mode = ClientShellMode::Navigate;
        state.navigate_workspace_id = state.navigation_target(&ClientEndpointId::Local, "ws_2");
        state.reveal_mobile_workspace = true;
        state.mobile_switcher_scroll = usize::MAX;
        state.compose(cols, rows).expect("compact shared switcher");
        assert!(state.session_view.mobile);
        assert_eq!(state.session_view.body.width, cols);
        assert_eq!(
            state.session_rows.as_ref().expect("cache").rows.as_ptr(),
            pointer
        );
        assert!(!state.reveal_mobile_workspace);
        let target = state
            .hits
            .mobile_targets
            .iter()
            .find_map(|(rect, target)| {
                matches!(target,
            ClientMobileTarget::Workspace { workspace_id, .. } if workspace_id == "ws_2")
                .then_some(*rect)
            })
            .expect("revealed workspace");
        assert_eq!(target.intersection(state.session_view.body), target);
        let outcome = click(&mut state, target);
        assert!(methods(&outcome).iter().any(|method| matches!(method, crate::api::schema::Method::WorkspaceFocus(params) if params.workspace_id == "ws_2")));
    }
}
#[test]
fn session_paint_does_not_consume_scroll_or_reveal_in_classic_and_mobile() {
    for (cols, rows) in [(160, 45), (44, 40)] {
        let mut state = classic_ready(cols, rows);
        if cols == 44 {
            state.mode = ClientShellMode::Navigate;
            state.compose(cols, rows).expect("switcher");
        }
        state.workspace_scroll = 999;
        state.mobile_switcher_scroll = 777;
        state.reveal_focused_workspace = true;
        state.reveal_navigation_workspace = true;
        state.reveal_mobile_workspace = true;
        let mut buffer = ratatui::buffer::Buffer::empty(Rect::new(0, 0, cols, rows));
        let glyphs = state.status_glyphs();
        super::super::chrome::sessions::paint_sessions(
            &mut buffer,
            &super::super::chrome::sessions::SessionPaint {
                cache: state.session_rows.as_ref().expect("cache"),
                view: &state.session_view,
                grouping: state.session_grouping,
                config: &state.config,
                glyphs,
                hover: None,
                spinner: state.spinner_glyph(),
                endpoints: &state.endpoints,
                active_endpoint_id: &state.active_endpoint_id,
            },
            &mut ShellHitMap::default(),
        );
        assert_eq!(
            (state.workspace_scroll, state.mobile_switcher_scroll),
            (999, 777)
        );
        assert!(
            state.reveal_focused_workspace
                && state.reveal_navigation_workspace
                && state.reveal_mobile_workspace
        );
    }
}
#[test]
fn mobile_scrollbar_and_unavailable_switcher_use_the_computed_view() {
    let mut state = classic_ready(44, 20);
    state.mode = ClientShellMode::Navigate;
    state.navigate_workspace_id = state.focused_navigation_target();
    state.compose(44, 20).expect("switcher");
    let track = state.hits.workspace_scrollbar;
    assert!(!track.is_empty());
    let metrics = state.hits.workspace_scroll_metrics.expect("metrics");
    let thumb = crate::ui::scrollbar_thumb(metrics, track).expect("thumb");
    let event = |kind, row| {
        RawInputEvent::Mouse(MouseEvent {
            kind,
            column: track.x,
            row,
            modifiers: KeyModifiers::NONE,
        })
    };
    let workspace_scroll = state.workspace_scroll;
    state.handle_raw_events(vec![event(
        MouseEventKind::Down(MouseButton::Left),
        thumb.top,
    )]);
    state.handle_raw_events(vec![event(
        MouseEventKind::Drag(MouseButton::Left),
        track.bottom() - 1,
    )]);
    state.handle_raw_events(vec![event(
        MouseEventKind::Up(MouseButton::Left),
        track.bottom() - 1,
    )]);
    assert!(state.mobile_switcher_scroll > 0);
    assert_eq!(
        state.workspace_scroll, workspace_scroll,
        "mobile 滚动不修改经典 workspace scroll"
    );
    state.pane_surface = None;
    state.compose(44, 20).expect("unavailable session switcher");
    assert_eq!(state.hits.workspace_body, state.session_view.body);
    assert!(!state.hits.mobile_close.is_empty());
    assert!(state
        .hits
        .mobile_targets
        .iter()
        .any(|(_, target)| matches!(target, ClientMobileTarget::Menu(_))));
    state.compose(44, 2).expect("tiny switcher");
    assert_eq!(state.mobile_switcher_scroll, 0);
    assert!(state.hits.mobile_targets.is_empty());
}
#[test]
fn classic_sessions_occupy_the_whole_sidebar_without_duplicate_agent_regions() {
    for (cols, rows) in [(160, 45), (80, 24), (106, 30)] {
        let mut state = classic_ready(cols, rows);
        for split in [0.1, 0.5, 0.9] {
            state.sidebar_section_split = split;
            state.compose(cols, rows).expect("full sidebar");
            let sidebar = state.layout(cols, rows).sidebar;
            assert_eq!(state.session_view.header.y, sidebar.y);
            assert_eq!(state.session_view.area.bottom(), sidebar.bottom());
            assert!(state.session_view.agents.is_empty());
            assert!(
                state.hits.agent_body.is_empty() && state.hits.sidebar_section_divider.is_empty()
            );
            let mut seen = HashSet::new();
            for (rect, endpoint, pane) in &state.hits.session_agents {
                assert!(seen.insert((endpoint, pane)), "duplicate agent {pane}");
                assert_eq!(rect.intersection(state.session_view.body), *rect);
            }
        }
    }
}
#[test]
fn classic_agent_navigation_reveals_session_rows_only_during_compute() {
    let mut state = classic_ready(160, 12);
    state.workspace_scroll = usize::MAX;
    state.reveal_focused_workspace = false;
    state.compose(160, 12).expect("bottom of sessions");
    let before = state.workspace_scroll;
    let mut outcome = ClientShellInput::default();
    state.record_binding(
        crate::input::KeybindMatch::Action(crate::input::KeybindAction::FocusAgent(0)),
        &mut outcome,
    );
    assert_eq!(
        state.workspace_scroll, before,
        "input queues reveal but does not lay out rows"
    );
    state.compose(160, 12).expect("revealed sessions");
    assert!(state
        .hits
        .session_agents
        .iter()
        .any(|(_, _, pane)| pane == "pane_2"));
    assert!(state.reveal_session_target.is_none());
    let revealed = state.workspace_scroll;
    state.compose(160, 12).expect("next frame");
    assert_eq!(state.workspace_scroll, revealed);
}
#[test]
fn authoritative_pane_focus_in_the_same_workspace_reveals_once_without_overriding_manual_scroll() {
    let mut state = classic_ready(160, 5);
    state.workspace_scroll = 0;
    state.reveal_focused_workspace = false;
    state.compose(160, 5).expect("small session viewport");
    assert!(state.hits.workspace_max_scroll > 0);
    assert!(!state
        .hits
        .session_agents
        .iter()
        .any(|(_, _, pane)| pane == "pane_3"));
    let mut snapshot = state.snapshot.as_deref().cloned().expect("snapshot");
    snapshot.revision += 1;
    snapshot.focused_pane_id = Some("pane_3".into());
    let workspace = snapshot.focused_workspace_id.clone();
    for pane in &mut snapshot.panes {
        pane.focused = pane.pane_id == "pane_3";
    }
    for agent in &mut snapshot.agents {
        agent.focused = agent.pane_id == "pane_3";
    }
    let mut surface = surface();
    surface.projection_revision = snapshot.revision;
    state.set_snapshot(Box::new(snapshot));
    assert_eq!(
        state
            .snapshot
            .as_ref()
            .expect("snapshot")
            .focused_workspace_id,
        workspace
    );
    assert!(state.reveal_focused_workspace);
    state.set_pane_surface(surface);
    state.compose(160, 5).expect("revealed new focus");
    assert!(state
        .hits
        .session_agents
        .iter()
        .any(|(_, _, pane)| pane == "pane_3"));
    assert!(!state.reveal_focused_workspace);
    state.workspace_scroll = usize::MAX;
    state.compose(160, 5).expect("manual scroll away");
    let manual = state.workspace_scroll;
    assert!(!state
        .hits
        .session_agents
        .iter()
        .any(|(_, _, pane)| pane == "pane_3"));
    let unchanged = state.snapshot.as_deref().cloned().expect("snapshot");
    state.set_snapshot(Box::new(unchanged));
    state.compose(160, 5).expect("same focus update");
    assert_eq!(state.workspace_scroll, manual);
    assert!(!state.reveal_focused_workspace);
}
#[test]
fn external_agent_metadata_keeps_full_name_unreadable_marker_and_age_spacing() {
    let _lang = crate::i18n::lang_guard(crate::i18n::Lang::En);
    let mut state = classic_ready(160, 45);
    state.sidebar_width = 42;
    state.sidebar_width_manual = true;
    let mut snapshot = state.snapshot.as_deref().cloned().expect("snapshot");
    snapshot.revision += 1;
    snapshot
        .external_agents
        .push(crate::protocol::ClientShellExternalAgent {
            external_id: "zcode:spacing".into(),
            source: "zcode".into(),
            agent_status: AgentStatus::Idle,
            label: "fix login".into(),
            readable: false,
            agent: Some("zcode".into()),
            cwd: None,
            updated_at_ms: Some(NOW_MS - 8 * 60_000),
            activity: Default::default(),
        });
    let mut surface = surface();
    surface.projection_revision = snapshot.revision;
    state.set_snapshot(Box::new(snapshot));
    state.set_pane_surface(surface);
    state.compose(160, 45).expect("external metadata");
    let rect = state
        .hits
        .session_external_agents
        .iter()
        .find(|(_, _, id)| id == "zcode:spacing")
        .expect("external row")
        .0;
    let buffer = state.compose_buffer.as_ref().expect("buffer");
    let text = (rect.x..rect.right())
        .map(|x| buffer[(x, rect.y)].symbol())
        .collect::<String>();
    assert!(
        text.contains("fix login") && text.contains("zcode"),
        "{text}"
    );
    assert!(
        text.contains(crate::i18n::texts().agent_panel.external_unreadable),
        "{text}"
    );
    assert!(
        text.trim_end().ends_with(" 8m"),
        "age must remain separated: {text}"
    );
    assert!(!text.contains("zcode8m"), "{text}");
}

/// 侧栏选中态（Graphite 精修）：键盘导航光标所在行 = 选中底色 + 粗体 + 首列 accent
/// `▌`；指针悬浮行只换底色、不画竖条；光标在场时整个侧栏只有它一道竖条。
#[test]
fn sidebar_selection_has_the_bar_and_hover_only_changes_the_background() {
    use crate::client::shell::feedback::ChromeHover;
    let _lang = crate::i18n::lang_guard(crate::i18n::Lang::ZhCn);
    let mut state = ready();
    let palette = state.config.palette.clone();
    let row_of = |state: &ClientShellState, id: &str| {
        state
            .hits
            .workspaces
            .iter()
            .find(|hit| hit.workspace_id == id)
            .map(|hit| hit.rect)
            .expect("workspace row")
    };
    state.hover = Some(ChromeHover::WorkspaceRow {
        endpoint_id: crate::client::endpoint::ClientEndpointId::Local,
        workspace_id: "ws_2".into(),
    });
    state.compose(120, 40).expect("悬浮");
    let hovered = row_of(&state, "ws_2");
    let buffer = state.compose_buffer.as_ref().expect("buffer");
    assert_eq!(
        buffer[(hovered.x + 3, hovered.y)].bg,
        palette.hover_row_bg(),
        "悬浮换底"
    );
    assert_ne!(buffer[(hovered.x, hovered.y)].symbol(), "▌", "悬浮不画竖条");

    state.handle_input_bytes(&[0x02]);
    state.handle_input_bytes(b"w");
    assert_eq!(state.mode, ClientShellMode::Navigate);
    state.compose(120, 40).expect("导航");
    let selected = state
        .session_view
        .selected
        .clone()
        .expect("进入会话导航后有键盘光标");
    let rect = state
        .hits
        .workspaces
        .iter()
        .find(|hit| selected.matches(&hit.endpoint_id, &hit.workspace_id))
        .map(|hit| hit.rect)
        .expect("光标行");
    let buffer = state.compose_buffer.as_ref().expect("buffer");
    let mark = &buffer[(rect.x, rect.y)];
    assert_eq!(mark.symbol(), "▌", "光标行首列是选中竖条");
    assert_eq!(mark.fg, palette.accent);
    assert_eq!(mark.bg, palette.selection_row_bg(), "竖条沿用选中底色");
    let label = (rect.x..rect.right())
        .map(|x| &buffer[(x, rect.y)])
        .find(|cell| cell.symbol().chars().all(char::is_alphanumeric) && cell.symbol() != " ")
        .expect("label cell");
    assert!(label.modifier.contains(Modifier::BOLD), "光标行文字加粗");
    let body = state.session_view.body;
    let bars = (body.y..body.bottom())
        .filter(|y| buffer[(body.x, *y)].symbol() == "▌")
        .count();
    assert_eq!(bars, 1, "光标在场时侧栏只有一道竖条");
}
