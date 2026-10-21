//! 会话侧栏、Inspector 组合中的导航与活动行为测试及渲染扩展 profile。

use super::*;
use crate::client::endpoint::{
    ClientEndpointId, ClientEndpointStatus, ProfileId, SavedSshEndpoint,
};
use crate::client::shell::agent_sidebar::agent_group_key;
use crate::client::shell::agent_tree::AgentTreeKind;
use crate::config::AgentPanelSortConfig;
use crate::protocol::{
    ClientShellActivityNode, ClientShellAgentActivity, ClientShellExternalAgent,
};

fn panel_agent(
    pane_id: &str,
    workspace_id: &str,
    tab_id: &str,
    name: &str,
    status: AgentStatus,
    seq: u64,
) -> ClientShellAgent {
    ClientShellAgent {
        insight: Default::default(),
        pane_id: pane_id.into(),
        workspace_id: workspace_id.into(),
        tab_id: tab_id.into(),
        name: Some(name.into()),
        display_agent: None,
        agent: Some("pi".into()),
        title: None,
        terminal_title: None,
        terminal_title_stripped: None,
        agent_status: status,
        state_change_seq: seq,
        state_labels: Vec::new(),
        tokens: Vec::new(),
        focused: false,
        launch_seq: 0,
        activity: Default::default(),
    }
}

/// 两个工作区、三个 agent（服务端顺序 one / two / three；启动顺序 two → three →
/// one，刻意与快照顺序不同）。状态刻意避开 `Done`：
/// 客户端首次见到某个 boot 时会把既有 `Done` 投影成 `Idle`
/// （`endpoint_agent_state::project_snapshot`），夹具里用它只会让断言含糊。
fn two_workspace_snapshot() -> ClientShellSnapshot {
    let mut projected = snapshot();
    let mut second_workspace = projected.workspaces[0].clone();
    second_workspace.workspace_id = "ws_2".into();
    second_workspace.active_tab_id = "tab_2".into();
    second_workspace.number = 2;
    second_workspace.label = "herdr".into();
    second_workspace.focused = false;
    projected.workspaces.push(second_workspace);
    let mut second_tab = projected.tabs[0].clone();
    second_tab.tab_id = "tab_2".into();
    second_tab.workspace_id = "ws_2".into();
    second_tab.focused = false;
    projected.tabs.push(second_tab);
    let mut second_pane = projected.panes[0].clone();
    second_pane.pane_id = "pane_2".into();
    second_pane.focused = false;
    let mut third_pane = second_pane.clone();
    third_pane.pane_id = "pane_3".into();
    third_pane.workspace_id = "ws_2".into();
    third_pane.tab_id = "tab_2".into();
    projected.panes.extend([second_pane, third_pane]);
    projected.agents = vec![
        panel_agent("pane_1", "ws_1", "tab_1", "one", AgentStatus::Idle, 10),
        panel_agent("pane_2", "ws_1", "tab_1", "two", AgentStatus::Blocked, 20),
        panel_agent("pane_3", "ws_2", "tab_2", "three", AgentStatus::Working, 30),
    ];
    for (agent, launch_seq) in projected.agents.iter_mut().zip([3, 1, 2]) {
        agent.launch_seq = launch_seq;
    }
    projected
}

/// 远端端点的快照：单工作区、两个 agent（r-one / r-two；启动顺序 r-two → r-one）。
fn remote_snapshot() -> ClientShellSnapshot {
    let mut remote = snapshot();
    remote.boot_id = "remote-boot".into();
    remote.workspaces[0].label = "remote-space".into();
    let mut second_pane = remote.panes[0].clone();
    second_pane.pane_id = "pane_2".into();
    second_pane.focused = false;
    remote.panes.push(second_pane);
    remote.agents = vec![
        panel_agent("pane_1", "ws_1", "tab_1", "r-one", AgentStatus::Working, 10),
        panel_agent("pane_2", "ws_1", "tab_1", "r-two", AgentStatus::Idle, 20),
    ];
    for (agent, launch_seq) in remote.agents.iter_mut().zip([2, 1]) {
        agent.launch_seq = launch_seq;
    }
    remote
}

fn remote_profile() -> SavedSshEndpoint {
    SavedSshEndpoint {
        id: ProfileId::parse("0123456789abcdef0123456789abcdef").expect("valid profile id"),
        label: "Build".into(),
        target: "dev@build.example".into(),
        session: "agents".into(),
        enabled: true,
        ..SavedSshEndpoint::new("base", "base", "default").expect("valid base profile")
    }
}

/// 默认行配置（`[状态图标 机器 工作区 标签页] / [agent]` 两行）+ Symbols 图标：
/// 三种状态各有不同字形（○ 空闲 / × 阻塞 / ◐ 工作），断言能区分行而不只看颜色。
fn panel_config(sort: AgentPanelSortConfig) -> ClientShellConfig {
    let mut config = Config::default();
    config.ui.agent_panel_sort = sort;
    config.ui.status_indicators = crate::config::StatusIndicatorStyle::Symbols;
    ClientShellConfig::from_config(&config)
}

/// classic 路径：单端点、未启用 workbench。
fn classic_state(sort: AgentPanelSortConfig) -> ClientShellState {
    classic_state_with(sort, two_workspace_snapshot())
}

fn classic_state_with(
    sort: AgentPanelSortConfig,
    projected: ClientShellSnapshot,
) -> ClientShellState {
    let mut state = ClientShellState::new(panel_config(sort));
    state.set_snapshot(Box::new(projected));
    state.set_pane_surface(surface());
    state
}

/// 联邦路径：Local + 一台在线远端（Build）。
fn federated_state(sort: AgentPanelSortConfig) -> (ClientShellState, ClientEndpointId) {
    let mut state = ClientShellState::new(panel_config(sort));
    let profile = remote_profile();
    let endpoint_id = ClientEndpointId::Ssh(profile.id.clone());
    state.set_endpoint_catalog(&[profile]);
    state.set_endpoint_status(&endpoint_id, ClientEndpointStatus::Online);
    state.set_snapshot(Box::new(two_workspace_snapshot()));
    state.set_pane_surface(surface());
    state.set_endpoint_snapshot(&endpoint_id, Box::new(remote_snapshot()));
    (state, endpoint_id)
}

/// 与 `workbench::ready()` 同构：宣告 `client.views.set` 后 tick 一次启用停靠工作台。
fn enable_workbench(state: &mut ClientShellState) {
    state.set_endpoint_methods(Some(vec!["client.views.set".into(), "tab.focus".into()]));
    state.compose(120, 40).expect("初始画面");
    state.tick_workbench(std::time::Instant::now(), &mut ClientShellInput::default());
    state.workbench.pending = false;
    state.workbench.acknowledged = state.workbench.revision;
    assert!(state.workbench.enabled, "夹具前提：workbench 布局已启用");
}

fn workbench_state(sort: AgentPanelSortConfig) -> ClientShellState {
    let mut state = classic_state(sort);
    enable_workbench(&mut state);
    state
}

/// 保留帧缓冲里 `rect` 覆盖的文本行。宽字符后面的占位格是空格，比较 CJK 文案时
/// 先过 `compact`。
fn rect_rows(state: &ClientShellState, rect: Rect) -> Vec<String> {
    let buffer = state.compose_buffer.as_ref().expect("保留帧缓冲");
    (rect.y..rect.bottom())
        .map(|y| {
            (rect.x..rect.right())
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect()
}

fn compact(text: &str) -> String {
    text.chars().filter(|ch| !ch.is_whitespace()).collect()
}

fn body_text(state: &ClientShellState) -> String {
    rect_rows(state, state.session_view.body).join("\n")
}

fn sort_label(state: &ClientShellState) -> String {
    compact(&rect_rows(state, state.hits.agent_sort_toggle).join(""))
}

fn classic_hit_ids(state: &ClientShellState) -> Vec<&str> {
    state
        .hits
        .session_agents
        .iter()
        .filter(|(_, endpoint, _)| endpoint.is_local())
        .map(|(_, _, pane)| pane.as_str())
        .collect()
}

fn toggle_keys(state: &ClientShellState) -> Vec<(bool, &str)> {
    state
        .hits
        .sessions
        .iter()
        .filter_map(|(_, hit)| match hit {
            super::super::chrome::sessions::SessionHit::Toggle(endpoint, key) => {
                Some((endpoint.is_local(), key.as_str()))
            }
            _ => None,
        })
        .chain(
            state
                .hits
                .session_tree_toggles
                .iter()
                .map(|(_, endpoint, key)| (endpoint.is_local(), key.as_str())),
        )
        .collect()
}

fn local_toggle_keys(state: &ClientShellState) -> Vec<&str> {
    toggle_keys(state).into_iter().map(|(_, key)| key).collect()
}

fn agent_panel_hits(state: &ClientShellState) -> Vec<(Rect, ClientEndpointId, String)> {
    state.hits.session_agents.clone()
}

fn endpoint_hit_ids(state: &ClientShellState) -> Vec<(ClientEndpointId, String)> {
    agent_panel_hits(state)
        .into_iter()
        .map(|(_, endpoint, pane)| (endpoint, pane))
        .collect()
}

fn local(pane_id: &str) -> (ClientEndpointId, String) {
    (ClientEndpointId::Local, pane_id.to_owned())
}

fn classic_agent_rect(state: &ClientShellState, pane_id: &str) -> Rect {
    state
        .hits
        .session_agents
        .iter()
        .find(|(_, endpoint, pane)| endpoint.is_local() && pane == pane_id)
        .unwrap_or_else(|| panic!("agent 会话行 {pane_id} 不在命中表里"))
        .0
}

fn toggle_rect(state: &ClientShellState, endpoint_id: &ClientEndpointId, key: &str) -> Rect {
    let wanted =
        super::super::chrome::sessions::SessionHit::Toggle(endpoint_id.clone(), key.to_owned());
    if let Some((rect, _)) = state.hits.sessions.iter().find(|(_, hit)| hit == &wanted) {
        return *rect;
    }
    state
        .hits
        .session_tree_toggles
        .iter()
        .find(|(_, endpoint, id)| endpoint == endpoint_id && id == key)
        .unwrap_or_else(|| panic!("折叠开关 {key} 不在命中表里"))
        .0
}

fn group_rect(state: &ClientShellState, workspace_id: &str) -> Rect {
    toggle_rect(
        state,
        &ClientEndpointId::Local,
        &agent_group_key(workspace_id),
    )
}

fn click(rect: Rect) -> RawInputEvent {
    click_at(rect.x, rect.y)
}

fn click_at(column: u16, row: u16) -> RawInputEvent {
    RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column,
        row,
        modifiers: KeyModifiers::empty(),
    })
}

fn moved(column: u16, row: u16) -> RawInputEvent {
    RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Moved,
        column,
        row,
        modifiers: KeyModifiers::empty(),
    })
}

const TREE_GLYPHS: [char; 4] = ['├', '└', '▾', '▸'];

fn assert_no_tree_glyphs(text: &str) {
    assert!(
        !text.contains(TREE_GLYPHS),
        "平铺视图不应出现树前缀或折叠箭头: {text}"
    );
}

/// 缓存里的 agent 行身份，按行序。
fn cached_agent_ids(state: &ClientShellState) -> Vec<(ClientEndpointId, String)> {
    state
        .session_rows
        .as_ref()
        .expect("会话行缓存")
        .rows
        .iter()
        .filter_map(|row| match &row.kind {
            super::super::chrome::sessions::SessionRowKind::Agent(agent) => {
                Some((row.endpoint_id.clone(), agent.pane_id.clone()))
            }
            _ => None,
        })
        .collect()
}

#[test]
fn characterization_classic_spaces_renders_two_level_workspace_tree() {
    let mut state = classic_state(AgentPanelSortConfig::Spaces);
    state.compose(106, 30).expect("统一会话树");
    assert_eq!(
        local_toggle_keys(&state),
        [agent_group_key("ws_1"), agent_group_key("ws_2")]
    );
    assert_eq!(classic_hit_ids(&state), ["pane_1", "pane_2", "pane_3"]);
    assert!(state.hits.agents.is_empty() && state.hits.agent_body.is_empty());
    assert!(state.hits.sidebar_section_divider.is_empty());
    assert_eq!(
        state.session_view.area.bottom(),
        state.layout(106, 30).sidebar.bottom()
    );
    let ys = [
        group_rect(&state, "ws_1").y,
        classic_agent_rect(&state, "pane_1").y,
        classic_agent_rect(&state, "pane_2").y,
        group_rect(&state, "ws_2").y,
        classic_agent_rect(&state, "pane_3").y,
    ];
    assert!(ys.windows(2).all(|pair| pair[0] < pair[1]), "{ys:?}");
    for (pane, name, glyph) in [
        ("pane_1", "one", "○"),
        ("pane_2", "two", "×"),
        ("pane_3", "three", "◐"),
    ] {
        let text = rect_rows(&state, classic_agent_rect(&state, pane)).join("");
        assert!(text.contains(name) && text.contains(glyph), "{text}");
        assert!(
            !text.contains("client-shell") && !text.contains("herdr"),
            "{text}"
        );
    }
}

#[test]
fn characterization_classic_collapsed_workspace_hides_children_and_flips_chevron() {
    let mut state = classic_state(AgentPanelSortConfig::Spaces);
    state.compose(106, 30).expect("展开帧");
    let before = group_rect(&state, "ws_2").y;
    let toggle = group_rect(&state, "ws_1");
    state.handle_raw_events(vec![click(toggle)]);
    assert!(state.collapsed_groups.contains("agent-panel:ws_1"));
    state.compose(106, 30).expect("折叠帧");
    assert_eq!(classic_hit_ids(&state), ["pane_3"]);
    let toggle = group_rect(&state, "ws_1");
    assert_eq!(rect_rows(&state, toggle)[0], "▸");
    assert!(group_rect(&state, "ws_2").y < before);
    state.handle_raw_events(vec![moved(toggle.x, toggle.y)]);
    assert!(
        matches!(state.hover, Some(super::super::feedback::ChromeHover::Session(super::super::chrome::sessions::SessionHit::Toggle(ClientEndpointId::Local, ref key))) if key == "agent-panel:ws_1")
    );
    state.handle_raw_events(vec![click(toggle)]);
    state.compose(106, 30).expect("重新展开");
    assert_eq!(classic_hit_ids(&state), ["pane_1", "pane_2", "pane_3"]);
}

#[test]
fn characterization_classic_launch_is_flat_in_global_launch_order() {
    let mut state = classic_state(AgentPanelSortConfig::Launch);
    state.compose(106, 30).expect("launch sessions");
    assert_eq!(classic_hit_ids(&state), ["pane_2", "pane_3", "pane_1"]);
    assert!(rect_rows(&state, classic_agent_rect(&state, "pane_2"))[0].contains("client-shell"));
    assert!(rect_rows(&state, classic_agent_rect(&state, "pane_3"))[0].contains("herdr"));

    // 旧 server 不下发 launch_seq：全为 0，稳定排序保持快照顺序。
    let mut projected = two_workspace_snapshot();
    for agent in &mut projected.agents {
        agent.launch_seq = 0;
    }
    let mut state = classic_state_with(AgentPanelSortConfig::Launch, projected);
    state
        .compose(106, 30)
        .expect("classic launch 帧（launch_seq 全 0）");
    assert_eq!(classic_hit_ids(&state), ["pane_1", "pane_2", "pane_3"]);
}

#[test]
fn characterization_classic_short_panel_degrades_to_flat_rows() {
    for sort in [AgentPanelSortConfig::Spaces, AgentPanelSortConfig::Launch] {
        let mut state = classic_state(sort);
        state.compose(106, 10).expect("short sessions");
        assert!(state.session_view.body.height > 0);
        assert!(state.hits.agent_body.is_empty());
        let mut seen = HashSet::new();
        for start in 0..10 {
            state.workspace_scroll = start;
            state.compose(106, 10).expect("session scroll");
            seen.extend(classic_hit_ids(&state).into_iter().map(str::to_owned));
        }
        assert_eq!(
            seen,
            HashSet::from(["pane_1".into(), "pane_2".into(), "pane_3".into()])
        );
    }
    let mut state = classic_state(AgentPanelSortConfig::Spaces);
    state.toggle_collapsed_group(&ClientEndpointId::Local, agent_group_key("ws_1"));
    state.compose(106, 30).expect("collapsed sessions");
    assert_eq!(classic_hit_ids(&state), ["pane_3"]);
}

#[test]
fn characterization_federated_collapsed_sidebar_keeps_agents_of_collapsed_workspaces() {
    let (mut state, remote) = federated_state(AgentPanelSortConfig::Spaces);
    state.toggle_collapsed_group(&ClientEndpointId::Local, agent_group_key("ws_1"));
    state.toggle_collapsed_group(&remote, agent_group_key("ws_1"));
    state.sidebar_collapsed = true;
    state.compose(106, 40).expect("federated rail");
    assert!(endpoint_hit_ids(&state).is_empty());
    assert_eq!(state.hits.workspaces.len(), 3);
    let rect = state
        .hits
        .workspaces
        .iter()
        .find(|hit| hit.endpoint_id == remote)
        .expect("remote workspace")
        .rect;
    let outcome = state.handle_raw_events(vec![
        click(rect),
        RawInputEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: rect.x,
            row: rect.y,
            modifiers: KeyModifiers::empty(),
        }),
    ]);
    assert!(
        matches!(&outcome.actions[..], [ClientShellAction::ActivateEndpoint { endpoint_id, target: Some(ClientEndpointFocusTarget::Workspace(workspace)) }] if endpoint_id == &remote && workspace == "ws_1")
    );
    state.collapsed_endpoints.insert(remote.clone());
    state.bump_tree_collapse_epoch();
    state.compose(106, 40).expect("collapsed machine");
    assert!(!state
        .hits
        .workspaces
        .iter()
        .any(|hit| hit.endpoint_id == remote));
    assert!(state
        .hits
        .machines
        .iter()
        .any(|hit| hit.endpoint_id == remote));
}

#[test]
fn characterization_collapsed_sidebar_machine_collapse_and_external_entries() {
    let (mut state, remote) = federated_state(AgentPanelSortConfig::Launch);
    let mut projected = two_workspace_snapshot();
    projected.external_agents.push(ClientShellExternalAgent {
        external_id: "zcode:abc".into(),
        source: "zcode".into(),
        label: "fix login".into(),
        readable: true,
        agent_status: AgentStatus::Working,
        agent: Some("zcode".into()),
        cwd: None,
        updated_at_ms: None,
        activity: Default::default(),
    });
    state.set_snapshot(Box::new(projected));
    state.compose(106, 40).expect("external sessions");
    assert_eq!(state.hits.session_external_agents.len(), 1);
    state.sidebar_collapsed = true;
    state.collapsed_endpoints.insert(remote.clone());
    state.bump_tree_collapse_epoch();
    state.compose(106, 40).expect("rail");
    assert!(state.hits.session_external_agents.is_empty());
    assert!(state.hits.session_agents.is_empty());
    assert!(!state
        .hits
        .workspaces
        .iter()
        .any(|hit| hit.endpoint_id == remote));
    assert_eq!(state.hits.workspaces.len(), 2);
}

#[test]
fn characterization_classic_tree_degrades_exactly_below_three_body_rows() {
    let mut state = classic_state(AgentPanelSortConfig::Spaces);
    for rows in 6..=30 {
        state.compose(106, rows).expect("classic sessions");
        assert!(state.hits.agent_body.is_empty() && state.hits.sidebar_section_divider.is_empty());
        assert_eq!(
            state.session_view.area.bottom(),
            state.layout(106, rows).sidebar.bottom()
        );
        for (rect, _, _) in &state.hits.session_agents {
            assert_eq!(rect.intersection(state.session_view.body), *rect);
        }
    }
}

#[test]
fn characterization_workbench_spaces_renders_the_same_workspace_tree_as_classic() {
    for cols in [120, 160] {
        let mut state = workbench_state(AgentPanelSortConfig::Spaces);
        state.compose(cols, 40).expect("workbench sessions");
        assert_eq!(
            local_toggle_keys(&state),
            [agent_group_key("ws_1"), agent_group_key("ws_2")]
        );
        assert!(state.hits.agents.is_empty() && state.hits.agent_body.is_empty());
        assert_eq!(
            endpoint_hit_ids(&state),
            [local("pane_1"), local("pane_2"), local("pane_3")]
        );
        for ((rect, _, _), (name, glyph)) in
            state
                .hits
                .session_agents
                .iter()
                .zip([("one", "○"), ("two", "×"), ("three", "◐")])
        {
            assert_eq!(*rect, rect.intersection(state.session_view.body));
            let text = rect_rows(&state, *rect).join("\n");
            assert!(text.contains(name) && text.contains(glyph), "{text}");
            assert!(
                !text.contains("Local")
                    && !text.contains("client-shell")
                    && !text.contains("herdr"),
                "{text}"
            );
        }
        state.toggle_collapsed_group(&ClientEndpointId::Local, agent_group_key("ws_1"));
        state
            .compose(cols, 40)
            .expect("collapsed workbench sessions");
        assert_eq!(endpoint_hit_ids(&state), [local("pane_3")]);
        assert_eq!(local_toggle_keys(&state).len(), 2);
        let toggle = state.hits.agent_sort_toggle;
        assert!(!toggle.is_empty());
        state.handle_raw_events(vec![click(toggle)]);
        assert_eq!(state.config.agent_panel_sort, AgentPanelSortConfig::Launch);
        state.compose(cols, 40).expect("launch workbench sessions");
        assert!(sort_label(&state).contains("↑"));
        assert!(state.hits.session_tree_toggles.is_empty());
        assert_eq!(
            endpoint_hit_ids(&state),
            [local("pane_2"), local("pane_3"), local("pane_1")]
        );
        for (rect, _, _) in &state.hits.session_agents {
            assert_no_tree_glyphs(&rect_rows(&state, *rect).join("\n"));
        }
    }
}

/// (d) 联邦（多端点、未启用 workbench）：机器层出现在工作区之上，各端点的折叠
/// 态各自生效；机器层复用 `collapsed_endpoints`，点机器行整体收起。
#[test]
fn characterization_federated_sidebar_nests_machines_above_workspaces() {
    let (mut state, remote) = federated_state(AgentPanelSortConfig::Spaces);
    assert!(!state.workbench.enabled);
    state.toggle_collapsed_group(&ClientEndpointId::Local, agent_group_key("ws_1"));
    state.toggle_collapsed_group(&remote, agent_group_key("ws_1"));
    state.compose(106, 40).expect("federated sessions");
    assert_eq!(endpoint_hit_ids(&state), [local("pane_3")]);
    assert!(state.hits.agents.is_empty() && state.hits.agent_body.is_empty());
    assert_eq!(
        state
            .hits
            .machines
            .iter()
            .map(|hit| hit.endpoint_id.clone())
            .collect::<Vec<_>>(),
        [ClientEndpointId::Local, remote.clone()]
    );
    for machine in &state.hits.machines {
        let label = if machine.endpoint_id.is_local() {
            "Local"
        } else {
            "Build"
        };
        assert!(rect_rows(&state, machine.rect).join("").contains(label));
        for workspace in state
            .hits
            .workspaces
            .iter()
            .filter(|hit| hit.endpoint_id == machine.endpoint_id)
        {
            assert!(workspace.rect.y > machine.rect.y);
        }
    }
    let remote_workspace = state
        .hits
        .workspaces
        .iter()
        .find(|hit| hit.endpoint_id == remote)
        .expect("remote workspace");
    let text = rect_rows(&state, remote_workspace.rect).join("\n");
    assert!(text.contains("remote-space"), "{text}");
    assert_eq!(
        rect_rows(
            &state,
            toggle_rect(
                &state,
                &remote,
                &agent_group_key(&remote_workspace.workspace_id)
            )
        )[0],
        "▸"
    );
    let three = rect_rows(&state, state.hits.session_agents[0].0).join("\n");
    assert!(
        three.contains("three") && !three.contains("Local"),
        "{three}"
    );
    let remote_row = state
        .hits
        .machines
        .iter()
        .find(|hit| hit.endpoint_id == remote)
        .expect("remote machine")
        .collapse_toggle;
    state.handle_raw_events(vec![click(remote_row)]);
    assert!(state.collapsed_endpoints.contains(&remote));
    state.compose(106, 40).expect("collapsed machine");
    assert!(!state
        .hits
        .workspaces
        .iter()
        .any(|hit| hit.endpoint_id == remote));
    assert_eq!(endpoint_hit_ids(&state), [local("pane_3")]);
    let machine = state
        .hits
        .machines
        .iter()
        .find(|hit| hit.endpoint_id == remote)
        .expect("collapsed machine stays visible");
    assert_eq!(machine.collapse_toggle.width, 2);
    assert_eq!(rect_rows(&state, machine.collapse_toggle)[0], "▸ ");
    state.handle_raw_events(vec![click(machine.collapse_toggle)]);
    assert!(!state.collapsed_endpoints.contains(&remote));
    state.compose(106, 40).expect("expanded machine");
    assert!(state
        .hits
        .workspaces
        .iter()
        .any(|hit| hit.endpoint_id == remote));
}

/// (e) workbench 的 `hits.session_agents` 与缓存里的 agent 行一一对应：同序、同
/// （端点, pane）身份，高度等于该行的 token 行数；点击按（端点, pane）路由。
#[test]
fn characterization_workbench_endpoint_agent_hits_mirror_cached_rows() {
    use super::super::chrome::sessions::SessionRowKind;
    for cols in [120, 160] {
        let (mut state, remote) = federated_state(AgentPanelSortConfig::Spaces);
        enable_workbench(&mut state);
        state
            .compose(cols, 60)
            .expect("workbench federated sessions");
        let body = state.session_view.body;
        let cache = state.session_rows.as_ref().expect("session cache");
        let agents = cache
            .rows
            .iter()
            .enumerate()
            .filter_map(|(index, row)| match &row.kind {
                SessionRowKind::Agent(agent) => Some((index, row, agent)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(agents.len(), 5);
        assert_eq!(cache.rows.len(), 5 + 2 + 3);
        assert_eq!(state.hits.session_agents.len(), agents.len());
        assert!(state.hits.workspace_scrollbar.is_empty());
        for (((rect, endpoint, pane), (index, row, agent)), name) in state
            .hits
            .session_agents
            .iter()
            .zip(&agents)
            .zip(["one", "two", "three", "r-one", "r-two"])
        {
            assert_eq!((endpoint, pane), (&row.endpoint_id, &agent.pane_id));
            assert_eq!((rect.x, rect.width), (body.x, body.width));
            assert_eq!(rect.height, cache.heights[*index]);
            assert_eq!(rect.height, agent.tokens.len().max(1) as u16);
            let text = rect_rows(&state, *rect).join("\n");
            assert!(text.contains(name), "{text}");
        }
        assert_eq!(state.hits.session_agents[3].1, remote);
        let rect = state.hits.session_agents[4].0;
        let outcome = state.handle_raw_events(vec![click_at(rect.x + 6, rect.y)]);
        assert!(
            matches!(&outcome.actions[..], [ClientShellAction::ActivateEndpoint { endpoint_id, target: Some(ClientEndpointFocusTarget::Pane(pane_id)) }] if endpoint_id == &remote && pane_id == "pane_2")
        );
    }
}

/// (e) 面板放不下全部行时，命中区只覆盖可见窗口：agent 行命中区是缓存 agent 行
/// 序列的一段连续切片，并为滚动条让出 1 列。
#[test]
fn characterization_workbench_endpoint_agent_hits_cover_only_the_visible_window() {
    use super::super::chrome::sessions::SessionRowKind;
    for cols in [120, 160] {
        let (mut state, _) = federated_state(AgentPanelSortConfig::Spaces);
        enable_workbench(&mut state);
        for start in [0, 3] {
            state.workspace_scroll = start;
            state.reveal_focused_workspace = false;
            state.compose(cols, 12).expect("short workbench sessions");
            let body = state.session_view.body;
            let cached = state
                .session_rows
                .as_ref()
                .expect("session cache")
                .rows
                .iter()
                .filter_map(|row| match &row.kind {
                    SessionRowKind::Agent(agent) => {
                        Some((row.endpoint_id.clone(), agent.pane_id.clone()))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            let visible = endpoint_hit_ids(&state);
            assert!(
                !visible.is_empty() && visible.len() < cached.len(),
                "{cols}: {visible:?} / {cached:?}, {body:?}"
            );
            assert_eq!(state.workspace_scroll, start);
            let offset = cached
                .iter()
                .position(|id| id == &visible[0])
                .expect("visible first row is cached");
            assert_eq!(cached[offset..offset + visible.len()], visible[..]);
            assert!(!state.hits.workspace_scrollbar.is_empty());
            for (rect, _, _) in &state.hits.session_agents {
                assert_eq!(rect.width, body.width - 1);
                assert_eq!(*rect, rect.intersection(body));
            }
        }
    }
}

fn current_rows_key(state: &ClientShellState) -> super::super::chrome::sessions::SessionRowsKey {
    super::super::chrome::sessions::SessionRowsKey::for_state(state)
}

/// 缓存行 Vec 的地址：重建时新 Vec 在旧缓存被替换之前分配，地址必然不同；
/// 复用则地址不变。比「键相等」更直接地回答「这一帧有没有重建」。
fn cached_rows_address(state: &ClientShellState) -> usize {
    let rows = &state.session_rows.as_ref().expect("会话行缓存").rows;
    assert!(!rows.is_empty(), "空 Vec 不分配，地址比较无意义");
    rows.as_ptr() as usize
}

fn cached_pane_ids(state: &ClientShellState) -> Vec<String> {
    cached_agent_ids(state)
        .into_iter()
        .map(|(_, pane_id)| pane_id)
        .collect()
}

/// (f) `SessionRowsCache`：键不变跨帧复用；折叠集合的代际（`tree_collapse_epoch`）
/// 在键里，折叠态一变就重建且行序列随之变化；快照 revision 与排序在键里，变了就
/// 重建。
#[test]
fn characterization_agent_rows_cache_rebuilds_on_revision_and_on_collapse() {
    let mut state = workbench_state(AgentPanelSortConfig::Spaces);
    state.compose(120, 40).expect("第一帧");
    let key = current_rows_key(&state);
    let address = cached_rows_address(&state);
    assert_eq!(cached_pane_ids(&state), ["pane_1", "pane_2", "pane_3"]);

    // 键不变：复用。
    state.compose(120, 40).expect("第二帧");
    assert_eq!(current_rows_key(&state), key);
    assert_eq!(cached_rows_address(&state), address, "无变化时跨帧复用");

    // 仅折叠态变化：折叠代际进键，重建，被折叠的子行离开序列。
    state.toggle_collapsed_group(&ClientEndpointId::Local, agent_group_key("ws_1"));
    assert!(state.group_is_collapsed(&ClientEndpointId::Local, &agent_group_key("ws_1")));
    state.compose(120, 40).expect("折叠后的帧");
    let collapsed_key = current_rows_key(&state);
    assert_ne!(collapsed_key, key, "折叠代际进缓存键");
    let collapsed_address = cached_rows_address(&state);
    assert_ne!(collapsed_address, address, "折叠即重建");
    assert_eq!(cached_pane_ids(&state), ["pane_3"]);
    state.compose(120, 40).expect("折叠后的第二帧");
    assert_eq!(
        cached_rows_address(&state),
        collapsed_address,
        "折叠后键稳定，跨帧复用"
    );
    let (key, address) = (collapsed_key, collapsed_address);

    // revision 在键里。这里绕开 `set_endpoint_snapshot` 原地推进 revision，只为把
    // 它与数据代际（agent_rows_epoch，生产写入路径会一并递增）隔离开。
    let epoch = state.agent_rows_epoch;
    let local_index = state
        .endpoints
        .iter()
        .position(|endpoint| endpoint.endpoint_id.is_local())
        .expect("本机端点");
    state.endpoints[local_index]
        .snapshot
        .as_mut()
        .expect("本机快照")
        .revision += 1;
    let revised_key = current_rows_key(&state);
    assert_eq!(state.agent_rows_epoch, epoch, "数据代际没动");
    assert_ne!(revised_key, key, "revision 单独变化就足以换键");
    state.refresh_session_rows();
    let revised_address = cached_rows_address(&state);
    assert_ne!(revised_address, address, "revision 变化触发重建");
    let cache = state.session_rows.as_ref().expect("会话行缓存");
    assert!(cache.key_matches(&revised_key) && !cache.key_matches(&key));

    // 生产路径：新快照（revision 前进、agent 改名）→ 重建，且新内容上屏。
    let mut next = two_workspace_snapshot();
    next.revision = 3;
    next.agents[0].name = Some("renamed".into());
    state.set_snapshot(Box::new(next));
    state.toggle_collapsed_group(&ClientEndpointId::Local, agent_group_key("ws_1"));
    state.compose(120, 40).expect("新快照帧");
    let renamed_address = cached_rows_address(&state);
    assert_ne!(renamed_address, revised_address);
    let first = state.hits.session_agents[0].0;
    let text = rect_rows(&state, first).join("\n");
    assert!(text.contains("○ renamed"), "重建后的会话行上屏：{text}");

    // 排序在键里：点排序标签 → 重建为 Launch 平铺行序。
    let toggle = state.hits.agent_sort_toggle;
    state.handle_raw_events(vec![click(toggle)]);
    state.compose(120, 40).expect("launch 帧");
    assert_ne!(cached_rows_address(&state), renamed_address);
    assert_eq!(cached_pane_ids(&state), ["pane_2", "pane_3", "pane_1"]);
}

fn aggregate_names(state: &ClientShellState, sort: AgentPanelSortConfig) -> Vec<String> {
    aggregate_navigation::aggregate_agent_rows(&state.endpoints, &state.active_endpoint_id, sort)
        .into_iter()
        .map(|row| {
            format!(
                "{}/{}",
                row.endpoint.label,
                row.agent.name.as_deref().expect("agent name")
            )
        })
        .collect()
}

/// (g) `aggregate_agent_rows` 是统一树与 mobile 的行序来源。Spaces：端点顺序 →
/// 各端点快照顺序，不看状态；Launch：stale 端点整体沉底，其余按端点序分组，
/// 组内再按 `launch_seq` 升序（0，即旧 server 未下发，排最后），组内 launch_seq
/// 打平时稳定排序保持快照序——语义是「端点内按启动顺序」，不同 server 各自计数
/// `launch_seq`，不跨端点直接比较。
#[test]
fn characterization_aggregate_agent_rows_order_under_spaces_and_launch() {
    let (mut state, remote) = federated_state(AgentPanelSortConfig::Spaces);
    // 端点是否宣告 agent view 投影决定走哪条分支；无自定义视图时两条分支同序。
    for projection_supported in [false, true] {
        state.set_endpoint_agent_view_projection_supported(
            &ClientEndpointId::Local,
            projection_supported,
        );
        assert_eq!(
            aggregate_names(&state, AgentPanelSortConfig::Spaces),
            [
                "Local/one",
                "Local/two",
                "Local/three",
                "Build/r-one",
                "Build/r-two"
            ],
            "projection_supported={projection_supported}"
        );
        assert_eq!(
            aggregate_names(&state, AgentPanelSortConfig::Launch),
            [
                "Local/two",
                "Local/three",
                "Local/one",
                "Build/r-two",
                "Build/r-one"
            ],
            "projection_supported={projection_supported}"
        );
    }

    // 远端掉线（stale）：Spaces 行序不变；Launch 把 stale 端点整体沉底，
    // 哪怕它的 agent 启动得更早。
    state.set_endpoint_status(&remote, ClientEndpointStatus::Reconnecting);
    assert_eq!(
        aggregate_names(&state, AgentPanelSortConfig::Spaces),
        [
            "Local/one",
            "Local/two",
            "Local/three",
            "Build/r-one",
            "Build/r-two"
        ]
    );
    assert_eq!(
        aggregate_names(&state, AgentPanelSortConfig::Launch),
        [
            "Local/two",
            "Local/three",
            "Local/one",
            "Build/r-two",
            "Build/r-one"
        ]
    );
    // 键盘按序号聚焦 agent 用的目标表：同一行序，但剔除 stale 端点。
    let targets = aggregate_navigation::online_agent_targets(
        &state.endpoints,
        &state.active_endpoint_id,
        AgentPanelSortConfig::Launch,
    )
    .into_iter()
    .map(|target| (target.endpoint_id, target.pane_id))
    .collect::<Vec<_>>();
    assert_eq!(targets, [local("pane_2"), local("pane_3"), local("pane_1")]);
}

/// 渲染扩展 profile 的夹具：`agents` 个 agent 平摊到若干工作区（每 4 个一个
/// 工作区、每工作区一个 tab），每个 agent 挂 `activity` 个活动节点（一半是根
/// 节点，另一半各挂在一个根下）。
fn scale_snapshot(agents: usize, activity: usize) -> ClientShellSnapshot {
    let mut projected = snapshot();
    projected.workspaces.clear();
    projected.tabs.clear();
    projected.panes.clear();
    projected.agents.clear();
    let workspace_count = agents.div_ceil(4).max(1);
    for index in 0..workspace_count {
        let mut workspace = snapshot().workspaces[0].clone();
        workspace.workspace_id = format!("ws_{index}");
        workspace.active_tab_id = format!("tab_{index}");
        workspace.number = index + 1;
        workspace.label = format!("space-{index}");
        workspace.focused = index == 0;
        projected.workspaces.push(workspace);
        let mut tab = snapshot().tabs[0].clone();
        tab.tab_id = format!("tab_{index}");
        tab.workspace_id = format!("ws_{index}");
        tab.focused = index == 0;
        projected.tabs.push(tab);
    }
    for index in 0..agents {
        let workspace = index / 4;
        let mut pane = snapshot().panes[0].clone();
        pane.pane_id = format!("pane_{index}");
        pane.workspace_id = format!("ws_{workspace}");
        pane.tab_id = format!("tab_{workspace}");
        pane.focused = index == 0;
        projected.panes.push(pane);
        let mut agent = panel_agent(
            &format!("pane_{index}"),
            &format!("ws_{workspace}"),
            &format!("tab_{workspace}"),
            &format!("agent-{index}"),
            [
                AgentStatus::Idle,
                AgentStatus::Working,
                AgentStatus::Blocked,
            ][index % 3],
            index as u64,
        );
        agent.launch_seq = (agents - index) as u64;
        let roots = activity.div_ceil(2);
        agent.activity.nodes = (0..activity)
            .map(|node| ClientShellActivityNode {
                id: format!("node_{node}"),
                kind: crate::api::schema::AgentActivityKind::Subagent,
                label: format!("task {node}"),
                status: if node % 2 == 0 {
                    crate::api::schema::AgentActivityStatus::Running
                } else {
                    crate::api::schema::AgentActivityStatus::Done
                },
                parent_id: (node >= roots).then(|| format!("node_{}", node - roots)),
                ..Default::default()
            })
            .collect();
        agent.activity.total = activity as u32;
        agent.activity.running = activity.div_ceil(2) as u32;
        projected.agents.push(agent);
    }
    projected.focused_workspace_id = Some("ws_0".into());
    projected.focused_tab_id = Some("tab_0".into());
    projected.focused_pane_id = Some("pane_0".into());
    projected
}

/// 非门禁扩展剖析（`just bench-render-scale` 的 `render_scale_profile` 过滤命中）：
/// Agents 面板在 1 / 15 / 52 个 agent、各带 0 与 8 个活动节点下的每帧合成耗时。
/// 四列：classic 稳态、workbench 稳态（行缓存命中）、workbench 每帧翻一次折叠
/// 态（行缓存每帧重建，量的是构建成本）、workbench 把每个 agent 的活动全部
/// 展开后的稳态（活动行上屏的渲染成本；8 个节点是整树形态，比生产默认的摘要
/// ——活跃节点加祖先链——相同，按上界看）。
#[test]
#[ignore = "manual agents panel composition scaling profile"]
fn agent_panel_render_scale_profile() {
    for (agents, activity) in [(1, 0), (1, 8), (15, 0), (15, 8), (52, 0), (52, 8)] {
        let mut classic = ClientShellState::new(panel_config(AgentPanelSortConfig::Spaces));
        classic.set_snapshot(Box::new(scale_snapshot(agents, activity)));
        classic.set_pane_surface(surface());
        let mut workbench = ClientShellState::new(panel_config(AgentPanelSortConfig::Spaces));
        workbench.set_snapshot(Box::new(scale_snapshot(agents, activity)));
        workbench.set_pane_surface(surface());
        enable_workbench(&mut workbench);
        let mut expanded = ClientShellState::new(panel_config(AgentPanelSortConfig::Spaces));
        expanded.set_snapshot(Box::new(scale_snapshot(agents, activity)));
        expanded.set_pane_surface(surface());
        enable_workbench(&mut expanded);
        for index in 0..agents {
            expanded.toggle_collapsed_group(
                &ClientEndpointId::Local,
                format!("agent-activity:pane:pane_{index}"),
            );
        }

        let measure = |state: &mut ClientShellState, toggle: bool| {
            for _ in 0..20 {
                std::hint::black_box(state.compose(120, 40).expect("agents panel frame"));
            }
            let start = std::time::Instant::now();
            for _ in 0..1000 {
                if toggle {
                    state.toggle_collapsed_group(
                        &ClientEndpointId::Local,
                        "agent-panel:ws_0".into(),
                    );
                }
                std::hint::black_box(state.compose(120, 40).expect("agents panel frame"));
            }
            start.elapsed().as_secs_f64() * 1000.0
        };
        let classic_us = measure(&mut classic, false);
        let workbench_us = measure(&mut workbench, false);
        let rebuild_us = measure(&mut workbench, true);
        let expanded_us = measure(&mut expanded, false);
        eprintln!(
            "agents panel: {agents} agents x {activity} nodes, classic {classic_us:.1} us/frame, \
             workbench {workbench_us:.1} us/frame, workbench+rebuild {rebuild_us:.1} us/frame, \
             workbench+expanded {expanded_us:.1} us/frame"
        );
    }
}

fn mobile_agent_targets(state: &ClientShellState) -> Vec<(Rect, String)> {
    state
        .hits
        .mobile_targets
        .iter()
        .filter_map(|(rect, target)| match target {
            ClientMobileTarget::Agent { pane_id, .. } => Some((*rect, pane_id.clone())),
            _ => None,
        })
        .collect()
}

#[test]
fn characterization_mobile_switcher_lists_agents_flat_in_aggregate_order() {
    use super::super::chrome::sessions::SessionHit;
    for (sort, expected) in [
        (AgentPanelSortConfig::Spaces, ["pane_1", "pane_2", "pane_3"]),
        (AgentPanelSortConfig::Launch, ["pane_2", "pane_3", "pane_1"]),
    ] {
        let mut projected = two_workspace_snapshot();
        projected.agents[1].activity = ClientShellAgentActivity {
            running: 1,
            total: 3,
            truncated: true,
            nodes: Vec::new(),
            ..Default::default()
        };
        let mut state = classic_state_with(sort, projected);
        state.toggle_collapsed_group(&ClientEndpointId::Local, agent_group_key("ws_1"));
        state.compose(44, 40).expect("mobile header");
        assert!(state.mobile_layout_active());
        state.handle_raw_events(vec![click(state.hits.mobile_switch)]);
        assert_eq!(state.mode, ClientShellMode::Navigate);
        state.compose(44, 40).expect("mobile sessions");
        assert_eq!(
            mobile_agent_targets(&state)
                .iter()
                .map(|(_, pane)| pane.as_str())
                .collect::<Vec<_>>(),
            if sort == AgentPanelSortConfig::Spaces {
                vec!["pane_3"]
            } else {
                expected.to_vec()
            }
        );
        let toggle = state
            .hits
            .sessions
            .iter()
            .find_map(|(rect, hit)| {
                matches!(hit, SessionHit::Toggle(_, key) if key == &agent_group_key("ws_1"))
                    .then_some(*rect)
            })
            .expect("workspace toggle");
        state.handle_raw_events(vec![click(toggle)]);
        state.compose(44, 40).expect("expanded shared sessions");
        let agents = mobile_agent_targets(&state);
        assert_eq!(
            agents
                .iter()
                .map(|(_, pane)| pane.as_str())
                .collect::<Vec<_>>(),
            expected
        );
        let aggregate = aggregate_navigation::aggregate_agent_rows(
            &state.endpoints,
            &state.active_endpoint_id,
            sort,
        )
        .into_iter()
        .map(|row| row.agent.pane_id.clone())
        .collect::<Vec<_>>();
        assert_eq!(aggregate, expected);
        for (rect, pane) in &agents {
            let text = rect_rows(&state, *rect).join("\n");
            let name = match pane.as_str() {
                "pane_1" => "one",
                "pane_2" => "two",
                _ => "three",
            };
            assert!(text.contains(name), "{pane}: {text}");
            assert_eq!(
                text.contains("▸1"),
                pane == "pane_2",
                "完整活动计数：{text}"
            );
        }
        assert!(state.hits.agent_tree_toggles.is_empty());
    }
}

#[test]
fn activity_badge_shows_running_then_finished_only_for_running_owners() {
    let badge = |status: AgentStatus, activity: ClientShellAgentActivity| {
        use super::super::chrome::sessions::SessionRowKind;
        let mut projected = two_workspace_snapshot();
        let mut agent = panel_agent("pane_1", "ws_1", "tab_1", "one", status, 1);
        agent.activity = activity;
        projected.agents = vec![agent];
        let mut state = classic_state_with(AgentPanelSortConfig::Spaces, projected);
        state.sidebar_width = 40;
        state.sidebar_width_manual = true;
        state.compose(106, 40).expect("real activity chip");
        let agent = state
            .session_rows
            .as_ref()
            .expect("session cache")
            .rows
            .iter()
            .find_map(|row| match &row.kind {
                SessionRowKind::Agent(agent) => Some(agent),
                _ => None,
            })
            .expect("agent row");
        let badge = agent.activity.as_ref().map(|(_, _, count)| count.clone());
        let text = rect_rows(&state, classic_agent_rect(&state, "pane_1")).join("");
        if let Some(count) = &badge {
            assert!(text.contains(count), "{text}");
        } else {
            assert!(!text.contains('▸'), "{text}");
        }
        badge.map(|count| count.trim_start_matches(['▸', '▾']).to_owned())
    };
    let counts = |running: u32, done: u32, failed: u32| ClientShellAgentActivity {
        running,
        active: running,
        done,
        failed,
        total: running + done + failed + 1,
        ..Default::default()
    };
    let node = |kind: crate::api::schema::AgentActivityKind,
                status: crate::api::schema::AgentActivityStatus| {
        ClientShellActivityNode {
            id: "n".into(),
            kind,
            status,
            ..Default::default()
        }
    };
    use crate::api::schema::AgentActivityKind::{Subagent, Todo};
    use crate::api::schema::AgentActivityStatus::{Pending, Running};
    for (lang, expected) in [
        (crate::i18n::Lang::En, ["2", "5", "1"]),
        (crate::i18n::Lang::ZhCn, ["2", "5", "1"]),
    ] {
        let _lang = crate::i18n::lang_guard(lang);
        let working = AgentStatus::Working;
        assert_eq!(
            badge(working, counts(2, 4, 1)).as_deref(),
            Some(expected[0]),
            "{lang:?}：有运行中的节点只报运行中"
        );
        assert_eq!(
            badge(working, counts(0, 4, 1)).as_deref(),
            Some(expected[1]),
            "{lang:?}：已完成 = 完成 + 失败"
        );
        assert_eq!(
            badge(AgentStatus::Blocked, counts(0, 0, 1)).as_deref(),
            Some(expected[2]),
            "{lang:?}：受阻也算在运行"
        );
        assert_eq!(badge(working, counts(0, 0, 0)), None, "没有可报的活动不画");
        assert_eq!(
            badge(working, ClientShellAgentActivity::default()),
            None,
            "没有活动不画"
        );
        for idle in [AgentStatus::Idle, AgentStatus::Done, AgentStatus::Unknown] {
            assert_eq!(
                badge(idle, counts(0, 4, 1)),
                None,
                "{lang:?}：属主 {idle:?} 不在运行，不画"
            );
        }
        // 空闲属主名下仍在跑的非待办节点：算在运行（后台子 agent）。
        let mut background = counts(1, 3, 0);
        background.nodes = vec![node(Subagent, Pending)];
        assert!(badge(AgentStatus::Idle, background).is_some(), "{lang:?}");
        // running 计数是共享摘要真源，不按客户端收到的节点子集猜测。
        let mut leftover = counts(1, 3, 0);
        leftover.nodes = vec![node(Todo, Running)];
        assert!(badge(AgentStatus::Idle, leftover).is_some(), "{lang:?}");
        // 有运行中的节点却一个也没下发（快照预算用尽）：算在运行。
        assert_eq!(
            badge(AgentStatus::Idle, counts(2, 0, 0)).as_deref(),
            Some(expected[0]),
            "{lang:?}"
        );
    }
}

#[test]
fn mobile_activity_badge_degrades_instead_of_truncating_digits() {
    const TAB_LABEL: &str = "feature-branch-with-a-very-long-tab-name";
    for lang in [crate::i18n::Lang::En, crate::i18n::Lang::ZhCn] {
        let _lang = crate::i18n::lang_guard(lang);
        for cols in [8u16, 16, 32, 44] {
            for running in [7, 1_234_567] {
                for len in 0..=TAB_LABEL.len() {
                    let mut projected = two_workspace_snapshot();
                    projected.tabs[1].label = TAB_LABEL[..len].into();
                    projected.tabs[1].custom_label = len > 0;
                    projected.agents[2].activity = ClientShellAgentActivity {
                        running,
                        total: running + 2,
                        truncated: true,
                        nodes: Vec::new(),
                        ..Default::default()
                    };
                    let mut state = classic_state_with(AgentPanelSortConfig::Spaces, projected);
                    state.compose(cols, 40).expect("mobile header");
                    assert!(state.mobile_layout_active());
                    state.handle_raw_events(vec![click(state.hits.mobile_switch)]);
                    state.compose(cols, 40).expect("shared mobile sessions");
                    let rect = mobile_agent_targets(&state)
                        .into_iter()
                        .find(|(_, pane)| pane == "pane_3")
                        .expect("pane_3 row")
                        .0;
                    let text = compact(&rect_rows(&state, rect).join("\n"));
                    let chip = format!("▸{running}");
                    if text.contains('▸') {
                        assert!(
                            text.contains(&chip),
                            "count must not truncate: {lang:?} {cols} {len} {text}"
                        );
                    }
                    if cols >= 32 || (cols >= 16 && running == 7) {
                        assert!(text.contains(&chip), "chip should fit: {text}");
                    }
                    if cols == 8 {
                        assert!(
                            !text.contains('▸'),
                            "hide the complete chip when too narrow: {text}"
                        );
                    }
                }
            }
        }
    }
}

/// agent 行右键的「重命名」实际执行 `pane.rename`、改 pane 标签（审查发现 4）：
/// 文案照实写「重命名窗格」（与 pane 右键菜单的同名项一致；那边归菜单车道，
/// 这里只钉本结构体的字面量，不跨车道比对）。
#[test]
fn agent_menu_rename_is_labelled_as_renaming_the_pane() {
    for (lang, expected) in [
        (crate::i18n::Lang::En, "Rename pane"),
        (crate::i18n::Lang::ZhCn, "重命名窗格"),
    ] {
        assert_eq!(
            crate::i18n::texts_for(lang).agent_panel.menu_rename,
            expected,
            "{lang:?}"
        );
    }
}

fn running_badge(running: u32) -> String {
    format!(
        "{running} {}",
        super::super::observability::tr("running", "运行中")
    )
}

fn finished_badge(finished: u32) -> String {
    format!(
        "{finished} {}",
        super::super::observability::tr("finished", "已完成")
    )
}

/// 一个 agent 挂 `activity` 个活动节点（`node_0..` 为根，`node_k` 挂在
/// `node_{k-roots}` 下）的单工作区夹具。
fn activity_snapshot(activity: usize) -> ClientShellSnapshot {
    scale_snapshot(1, activity)
}

/// 标签页层只在工作区有 >1 个标签页时出现，出现后 agent 行不再重复标签页 token；
/// 单标签页工作区的 agent 行直接挂在工作区下。
#[test]
fn tree_tab_level_appears_only_with_multiple_tabs_and_drops_tab_tokens() {
    let mut projected = two_workspace_snapshot();
    let mut second_tab = projected.tabs[0].clone();
    second_tab.tab_id = "tab_1b".into();
    second_tab.number = 2;
    second_tab.label = "review".into();
    second_tab.custom_label = true;
    second_tab.focused = false;
    projected.tabs.push(second_tab);
    projected.tabs[0].label = "main".into();
    projected.tabs[0].custom_label = true;
    projected.agents[1].tab_id = "tab_1b".into();
    projected.panes[1].tab_id = "tab_1b".into();
    let mut state = classic_state_with(AgentPanelSortConfig::Spaces, projected);
    state.compose(106, 30).expect("标签页层帧");

    assert_eq!(
        local_toggle_keys(&state),
        [
            "agent-panel:ws_1",
            "agent-tab:tab_1",
            "agent-tab:tab_1b",
            "agent-panel:ws_2"
        ],
        "ws_1 有两个标签页 → 标签页层；ws_2 只有一个 → 没有"
    );
    for (tab, label, glyph) in [("tab_1", "main", "○"), ("tab_1b", "review", "×")] {
        let hit =
            super::super::chrome::sessions::SessionHit::Tab(ClientEndpointId::Local, tab.into());
        let rect = state
            .hits
            .sessions
            .iter()
            .find(|(_, target)| target == &hit)
            .expect("tab header")
            .0;
        let text = rect_rows(&state, rect).join("");
        assert!(
            text.contains(label) && text.contains(glyph) && text.trim_end().ends_with('1'),
            "{text}"
        );
        assert_eq!(
            rect_rows(
                &state,
                toggle_rect(
                    &state,
                    &ClientEndpointId::Local,
                    &format!("agent-tab:{tab}")
                )
            )[0],
            "▾"
        );
    }
    for (pane, name, glyph) in [("pane_1", "one", "○"), ("pane_2", "two", "×")] {
        let text = rect_rows(&state, classic_agent_rect(&state, pane)).join("");
        assert!(text.contains(name) && text.contains(glyph), "{text}");
        assert!(!text.contains("main") && !text.contains("review"), "{text}");
    }
    let toggle = toggle_rect(&state, &ClientEndpointId::Local, "agent-tab:tab_1");
    state.handle_raw_events(vec![click(toggle)]);
    state.compose(106, 30).expect("collapsed tab");
    assert_eq!(classic_hit_ids(&state), ["pane_2", "pane_3"]);
    assert_eq!(
        rect_rows(
            &state,
            toggle_rect(&state, &ClientEndpointId::Local, "agent-tab:tab_1")
        )[0],
        "▸"
    );
    state.handle_raw_events(vec![click(toggle)]);
    state.compose(106, 30).expect("expanded tab");
    assert_eq!(classic_hit_ids(&state), ["pane_1", "pane_2", "pane_3"]);
}

/// 快照默认下发的活动摘要：计数（运行中 2、活跃 3、完成 4、失败 1）+ 活跃子集
/// （这里只下发了 1 个活跃节点，`truncated` 表示还有活跃节点没下发，整树走
/// `agent.activity.read`）。agent-0 自己空闲，名下的子 agent 还在跑（后台子
/// agent），算在运行。
fn summary_snapshot() -> ClientShellSnapshot {
    let mut projected = activity_snapshot(0);
    projected.agents[0].activity = ClientShellAgentActivity {
        running: 2,
        active: 3,
        done: 4,
        failed: 1,
        total: 12,
        truncated: true,
        nodes: vec![ClientShellActivityNode {
            id: "sub-7".into(),
            kind: crate::api::schema::AgentActivityKind::Subagent,
            label: "explore repo".into(),
            status: crate::api::schema::AgentActivityStatus::Running,
            // 摘要里父节点不在列表中：按根节点处理。
            parent_id: Some("sub-1".into()),
            ..Default::default()
        }],
    };
    projected
}

#[test]
fn tree_agent_rows_show_the_activity_summary_collapsed_by_default() {
    for sort in [AgentPanelSortConfig::Spaces, AgentPanelSortConfig::Launch] {
        let mut state = classic_state_with(sort, summary_snapshot());
        state.sidebar_width = 40;
        state.sidebar_width_manual = true;
        state.compose(106, 30).expect("collapsed activity");
        assert!(state.hits.session_activity_rows.is_empty());
        let text = rect_rows(&state, classic_agent_rect(&state, "pane_0")).join("");
        assert!(text.contains("○ agent-0") && text.contains("▸2"), "{text}");
        let key = "agent-activity:pane:pane_0";
        let toggle = toggle_rect(&state, &ClientEndpointId::Local, key);
        assert_eq!(toggle.width, 2);
        assert_eq!(
            text_cell(&state, toggle, "2").fg,
            state.config.components.status_running
        );
        state.handle_raw_events(vec![click(toggle)]);
        state.compose(106, 30).expect("expanded activity");
        assert!(state.collapsed_groups.contains(key));
        assert_eq!(
            state
                .hits
                .session_activity_rows
                .iter()
                .map(|hit| (hit.owner_key.as_str(), hit.node_id.as_str()))
                .collect::<Vec<_>>(),
            [
                ("pane:pane_0", "sub-7"),
                ("pane:pane_0", ""),
                (
                    "pane:pane_0",
                    super::super::agent_tree::ACTIVITY_DONE_HIT_ID
                )
            ]
        );
        let texts = &crate::i18n::texts().agent_panel;
        let expected = [
            "explore repo".to_owned(),
            crate::i18n::fill(texts.activity_more_fmt, &[("n", "2")]),
            format!(
                "{} · {}",
                crate::i18n::fill(texts.activity_done_fmt, &[("n", "4")]),
                crate::i18n::fill(texts.activity_failed_fmt, &[("n", "1")])
            ),
        ];
        for (index, text) in expected.iter().enumerate() {
            let rect = state.hits.session_activity_rows[index].rect;
            let line = rect_rows(&state, rect).join("");
            assert!(compact(&line).contains(&compact(text)), "{line}");
            assert!(line.contains(if index == 2 { '└' } else { '├' }), "{line}");
            if index == 0 {
                assert!(line.contains('◐'));
                assert!(compact(&line)
                    .contains(&compact(crate::i18n::texts().agent_activity.kind_subagent)));
            } else if index == 2 {
                let first = text.chars().next().expect("completion text").to_string();
                assert_eq!(
                    text_cell(&state, rect, &first).fg,
                    state.config.palette.overlay0
                );
            }
            state.handle_raw_events(vec![click_at(rect.x + 10, rect.y)]);
            let Some(ClientShellOverlay::AgentActivity(overlay)) = state.overlay.as_ref() else {
                panic!("activity overlay");
            };
            assert_eq!(
                overlay.owner,
                super::super::agent_activity_overlay::AgentActivityOwner::Pane {
                    pane_id: "pane_0".into()
                }
            );
            assert_eq!(
                overlay.selected_node.as_deref(),
                if index == 0 { Some("sub-7") } else { None }
            );
            state.overlay = None;
        }
        state.handle_raw_events(vec![click(toggle_rect(
            &state,
            &ClientEndpointId::Local,
            key,
        ))]);
        state.compose(106, 30).expect("collapsed again");
        assert!(state.hits.session_activity_rows.is_empty());
    }
    let mut projected = summary_snapshot();
    projected.agents[0].activity.running = 0;
    projected.agents[0].agent_status = AgentStatus::Working;
    let mut state = classic_state_with(AgentPanelSortConfig::Spaces, projected);
    state.compose(106, 30).expect("finished activity");
    let toggle = toggle_rect(
        &state,
        &ClientEndpointId::Local,
        "agent-activity:pane:pane_0",
    );
    assert_eq!(rect_rows(&state, toggle)[0], "▸5");
    assert_eq!(
        text_cell(&state, toggle, "5").fg,
        state.config.palette.overlay0
    );
}

/// 徽标在窄侧栏里的档位（审查发现 2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BadgeTier {
    Full,
    Digits,
    Hidden,
}

/// 单工作区夹具，agent-0 带「2 个运行中 / 共 5 个」的活动摘要；`extra_tab` 给该
/// 工作区加一个空标签页，让树多出标签页层。
fn badged_snapshot(extra_tab: bool) -> ClientShellSnapshot {
    let mut projected = activity_snapshot(0);
    projected.agents[0].activity = ClientShellAgentActivity {
        running: 2,
        total: 5,
        truncated: true,
        nodes: Vec::new(),
        ..Default::default()
    };
    if extra_tab {
        let mut tab = projected.tabs[0].clone();
        tab.tab_id = "tab_x".into();
        tab.label = "x".into();
        tab.focused = false;
        projected.tabs.push(tab);
    }
    projected
}

fn assert_badge_tier(
    state: &ClientShellState,
    rect: Rect,
    full: &str,
    tier: BadgeTier,
    case: &str,
) {
    let line = rect_rows(state, rect)[0].clone();
    assert!(
        !compact(&line).contains(&compact(full)),
        "会话行不画旧长徽标：{line}"
    );
    match tier {
        BadgeTier::Full | BadgeTier::Digits => {
            assert!(line.trim_end().ends_with("▸2"), "{case}: 完整计数：{line}");
            let buffer = state.compose_buffer.as_ref().expect("buffer");
            assert_eq!(buffer[(rect.right() - 2, rect.y)].symbol(), "2");
            assert_eq!(buffer[(rect.right() - 3, rect.y)].symbol(), "▸");
        }
        BadgeTier::Hidden => assert!(!line.contains('2'), "{case}: 数字整体让位：{line}"),
    }
}

#[test]
fn tree_badge_yields_to_the_status_icon_and_name_on_narrow_sidebars() {
    use BadgeTier::{Digits, Full, Hidden};
    for lang in [crate::i18n::Lang::En, crate::i18n::Lang::ZhCn] {
        let _lang = crate::i18n::lang_guard(lang);
        let full = running_badge(2);
        for (width, depth, tier) in [
            (18, 1, Digits),
            (18, 2, Hidden),
            (18, 3, Hidden),
            (26, 1, Full),
            (26, 2, Digits),
            (26, 3, Digits),
            (36, 1, Full),
            (36, 2, Full),
            (36, 3, Full),
        ] {
            assert_narrow_agent_row(lang, &full, width, depth, tier);
        }
        for (width, tier) in [(18, Hidden), (36, Full)] {
            let mut projected = badged_snapshot(false);
            projected.external_agents = vec![ClientShellExternalAgent {
                external_id: "zcode:abc".into(),
                source: "zcode".into(),
                agent_status: AgentStatus::Working,
                label: "fix login".into(),
                readable: true,
                agent: None,
                cwd: None,
                updated_at_ms: None,
                activity: projected.agents[0].activity.clone(),
            }];
            let mut state = classic_state_with(AgentPanelSortConfig::Spaces, projected);
            state.sidebar_width = width;
            state.sidebar_width_manual = true;
            state.compose(106, 40).expect("narrow external session");
            let case = format!("{lang:?} external sidebar {width}");
            let rect = state
                .hits
                .session_external_agents
                .first()
                .expect("external session hit")
                .0;
            assert_eq!(rect.width, width - 1);
            let buffer = state.compose_buffer.as_ref().expect("buffer");
            let content_x = rect.x + 3;
            assert_eq!(buffer[(content_x, rect.y)].symbol(), "◐", "{case}");
            let label = (content_x + 2..content_x + 8)
                .map(|x| buffer[(x, rect.y)].symbol())
                .collect::<String>();
            assert_eq!(label, "fix lo", "{case}: 标签至少保 6 列");
            assert_badge_tier(&state, rect, &full, tier, &case);
        }
    }
}

/// 侧栏 `width` 列、agent 在树里深 `depth` 层时，agent-0 首行的图标、名称前缀与
/// 徽标档位。深度 3 用联邦侧栏（机器 → 工作区 → 标签页 → agent）。
fn assert_narrow_agent_row(
    lang: crate::i18n::Lang,
    full: &str,
    width: u16,
    depth: u16,
    tier: BadgeTier,
) {
    let mut state = if depth == 3 {
        let (mut state, _) = federated_state(AgentPanelSortConfig::Spaces);
        // 换 boot：同一 boot 下新出现的 Idle agent 会被投影成未读的 Done（✓）。
        let mut projected = badged_snapshot(true);
        projected.boot_id = "badged-boot".into();
        state.set_snapshot(Box::new(projected));
        state
    } else {
        classic_state_with(AgentPanelSortConfig::Spaces, badged_snapshot(depth == 2))
    };
    state.sidebar_width = width;
    state.sidebar_width_manual = true;
    state.compose(106, 40).expect("窄侧栏帧");
    let case = format!("{lang:?}、侧栏 {width}、深度 {depth}");
    let rect = if depth == 3 {
        state
            .hits
            .session_agents
            .iter()
            .find(|(_, endpoint_id, pane_id)| endpoint_id.is_local() && pane_id == "pane_0")
            .unwrap_or_else(|| panic!("{case}: 本机 agent 行不在命中表里"))
            .0
    } else {
        classic_agent_rect(&state, "pane_0")
    };
    assert_eq!(
        rect.width,
        width - 1,
        "{case}: 夹具前提：行宽 = 侧栏宽 − 右缘分隔线"
    );
    let buffer = state.compose_buffer.as_ref().expect("缓冲");
    let cell = |x: u16| buffer[(x, rect.y)].symbol().to_owned();
    let content_x = rect.x + 3 + u16::from(depth >= 2) * 2;
    let toggle = toggle_rect(
        &state,
        &ClientEndpointId::Local,
        "agent-activity:pane:pane_0",
    );
    assert_eq!(
        buffer[(toggle.x, toggle.y)].symbol(),
        "▸",
        "{case}: 活动摘要的折叠开关"
    );
    assert_eq!(cell(content_x), "○", "{case}: 状态图标");
    // 名称优先：表里每一档都放得下整名「agent-0」，徽标不截名称（L4 复审：此前只
    // 断言保底 6 列，放过了默认宽度下名称被徽标挤成「agent…」的回归）。
    let name = (content_x + 2..content_x + 9).map(cell).collect::<String>();
    assert_eq!(name, "agent-0", "{case}: 名称完整可见");
    assert_badge_tier(&state, rect, full, tier, &case);
    if tier == BadgeTier::Hidden {
        let line = rect_rows(&state, rect)[0].clone();
        assert!(line.contains("○ agent-0"), "{case}: 名称完整: {line:?}");
    }
}

/// server 下发多个节点（整树形态，基准 / 旧行为）时同一套构建按 `parent_id`
/// 前序展开：根节点平列，子节点默认折叠、点开关逐层展开（键在集合里 = 展开）。
#[test]
fn tree_activity_with_several_nodes_expands_children_on_demand() {
    let mut state = classic_state_with(AgentPanelSortConfig::Spaces, activity_snapshot(4));
    state.toggle_collapsed_group(
        &ClientEndpointId::Local,
        "agent-activity:pane:pane_0".into(),
    );
    state.compose(106, 30).expect("多节点帧");
    assert_eq!(
        local_toggle_keys(&state),
        [
            "agent-panel:ws_0",
            "agent-activity:pane:pane_0",
            "agent-node:pane:pane_0:node_0",
            "agent-node:pane:pane_0:node_1",
        ],
        "根节点 node_0 / node_1 各有一个子节点，默认折叠"
    );
    let ids = |state: &ClientShellState| {
        state
            .hits
            .session_activity_rows
            .iter()
            .map(|hit| hit.node_id.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(&state), ["node_0", "node_1"]);
    let node_0 = rect_rows(&state, state.hits.session_activity_rows[0].rect);
    assert!(node_0[0].starts_with("├─▸ ◐ task 0"), "{node_0:?}");
    let node_1 = rect_rows(&state, state.hits.session_activity_rows[1].rect);
    assert!(node_1[0].starts_with("└─▸ ✓ task 1"), "{node_1:?}");
    let toggle = toggle_rect(
        &state,
        &ClientEndpointId::Local,
        "agent-node:pane:pane_0:node_0",
    );
    state.handle_raw_events(vec![click(toggle)]);
    assert!(state
        .collapsed_groups
        .contains("agent-node:pane:pane_0:node_0"));
    state.compose(106, 30).expect("expanded node_0");
    assert_eq!(ids(&state), ["node_0", "node_2", "node_1"]);
    let node_2 = rect_rows(&state, state.hits.session_activity_rows[1].rect);
    assert!(
        node_2[0].starts_with("│ └── ◐ task 2"),
        "深一层的子节点: {node_2:?}"
    );
}

/// 失败的活动子节点走共享的状态桶：画失败字形与失败色，不再借「需要输入」的 `◆`；
/// 受阻节点仍是需要输入。字形在三种指示风格下都一致。
#[test]
fn failed_activity_child_renders_the_failed_glyph_and_color() {
    use crate::api::schema::{AgentActivityKind, AgentActivityStatus};
    let mut projected = summary_snapshot();
    projected.agents[0].activity.truncated = false;
    projected.agents[0].activity.nodes = vec![
        ClientShellActivityNode {
            id: "boom".into(),
            kind: AgentActivityKind::Subagent,
            label: "boom".into(),
            status: AgentActivityStatus::Failed,
            ..Default::default()
        },
        ClientShellActivityNode {
            id: "ask".into(),
            kind: AgentActivityKind::Subagent,
            label: "ask".into(),
            status: AgentActivityStatus::Blocked,
            ..Default::default()
        },
    ];
    let mut state = classic_state_with(AgentPanelSortConfig::Spaces, projected);
    state.sidebar_width = 40;
    state.sidebar_width_manual = true;
    state.toggle_collapsed_group(
        &ClientEndpointId::Local,
        "agent-activity:pane:pane_0".into(),
    );
    state.compose(106, 30).expect("失败子节点");
    let components = state.config.components.clone();
    let row = |id: &str| {
        state
            .hits
            .session_activity_rows
            .iter()
            .find(|hit| hit.node_id == id)
            .expect("活动行")
            .rect
    };
    let boom = row("boom");
    let text = rect_rows(&state, boom).join("");
    assert!(text.contains("✕ boom") && !text.contains('◆'), "{text}");
    assert_eq!(text_cell(&state, boom, "✕").fg, components.status_failed);
    let ask = row("ask");
    let glyph = crate::ui::design::status_glyph(
        crate::harness::AgentBucket::NeedsInput,
        state.config.status_indicators,
        0,
        false,
    );
    assert!(rect_rows(&state, ask)
        .join("")
        .contains(&format!("{glyph} ask")));
    assert_eq!(text_cell(&state, ask, glyph).fg, components.status_input);
}

/// workflow → phase → 子 agent 的类型、层级与进度必须真实画到字符缓冲。
#[test]
fn active_workflow_tree_renders_group_types_progress_and_finished_summary() {
    use crate::api::schema::{AgentActivityKind, AgentActivityStatus};
    for (width, workflow_mark, phase_mark) in [(48, "⧉", "▤"), (18, "W", "P")] {
        let mut projected = summary_snapshot();
        projected.agents[0].activity.truncated = false;
        projected.agents[0].activity.nodes = vec![
            ClientShellActivityNode {
                id: "flow".into(),
                label: "flow".into(),
                kind: AgentActivityKind::Task,
                status: AgentActivityStatus::Running,
                agent_type: Some("workflow".into()),
                summary: Some("1/3 running".into()),
                ..Default::default()
            },
            ClientShellActivityNode {
                id: "phase".into(),
                parent_id: Some("flow".into()),
                label: "phase".into(),
                kind: AgentActivityKind::Task,
                status: AgentActivityStatus::Running,
                agent_type: Some("phase".into()),
                summary: Some("1/2 running".into()),
                ..Default::default()
            },
            ClientShellActivityNode {
                id: "child".into(),
                parent_id: Some("phase".into()),
                label: "child".into(),
                kind: AgentActivityKind::Subagent,
                status: AgentActivityStatus::Running,
                ..Default::default()
            },
        ];
        let mut state = classic_state_with(AgentPanelSortConfig::Spaces, projected);
        state.sidebar_width = width;
        state.sidebar_width_manual = true;
        for key in [
            "agent-activity:pane:pane_0",
            "agent-node:pane:pane_0:flow",
            "agent-node:pane:pane_0:phase",
        ] {
            state.toggle_collapsed_group(&ClientEndpointId::Local, key.into());
        }
        state.compose(106, 40).expect("workflow 树");
        let rows = &state.hits.session_activity_rows;
        assert_eq!(
            rows.iter()
                .map(|hit| hit.node_id.as_str())
                .collect::<Vec<_>>(),
            [
                "flow",
                "phase",
                "child",
                super::super::agent_tree::ACTIVITY_DONE_HIT_ID
            ]
        );
        let flow = rect_rows(&state, rows[0].rect)[0].clone();
        let phase = rect_rows(&state, rows[1].rect)[0].clone();
        assert!(flow.contains(workflow_mark), "{width}: {flow:?}");
        assert!(phase.contains(phase_mark), "{width}: {phase:?}");
        if width == 48 {
            assert!(flow.contains("flow 1/3"), "{flow:?}");
            assert!(phase.contains("phase 1/2"), "{phase:?}");
            assert!(rect_rows(&state, rows[2].rect)[0].contains("child"));
        }
        let finished = rows[3].rect;
        state.handle_raw_events(vec![click_at(finished.x + 10, finished.y)]);
        assert!(
            matches!(state.overlay.as_ref(), Some(ClientShellOverlay::AgentActivity(overlay)) if overlay.selected_node.is_none())
        );
    }
}

#[test]
fn idle_owner_hides_activity_children_badges_and_expansion_in_tree_and_launch() {
    for sort in [AgentPanelSortConfig::Spaces, AgentPanelSortConfig::Launch] {
        let mut projected = summary_snapshot();
        projected.agents[0].activity.running = 0;
        projected.agents[0].agent_status = AgentStatus::Idle;
        let mut state = classic_state_with(sort, projected);
        state.toggle_collapsed_group(
            &ClientEndpointId::Local,
            "agent-activity:pane:pane_0".into(),
        );
        state.compose(106, 40).expect("空闲属主");
        assert!(state.hits.session_activity_rows.is_empty());
        assert!(!local_toggle_keys(&state).contains(&"agent-activity:pane:pane_0"));
        let row = rect_rows(&state, classic_agent_rect(&state, "pane_0"));
        assert!(!compact(&row[0]).contains(&compact(&finished_badge(5))));
    }
}

#[test]
fn agents_panel_keyboard_opens_summary_rows_and_scrolls_to_offscreen_rows() {
    use super::super::chrome::sessions::SessionRowKind;
    use super::super::dock::PanelId;
    let key =
        |code| RawInputEvent::Key(crate::input::TerminalKey::new(code, KeyModifiers::empty()));
    for width in [120, 160, 62] {
        for done in [false, true] {
            let mut state = classic_state_with(AgentPanelSortConfig::Spaces, summary_snapshot());
            enable_workbench(&mut state);
            state.workbench.dock.focused = PanelId::Workspaces;
            if width < super::super::chrome::sessions::RAIL_BELOW_COLS {
                state.workbench.dock.maximized = Some(PanelId::Workspaces);
            }
            state.toggle_collapsed_group(
                &ClientEndpointId::Local,
                "agent-activity:pane:pane_0".into(),
            );
            state
                .compose(width, 40)
                .expect("real session keyboard tree");
            assert!(!state.session_view.rail && !state.session_view.body.is_empty());
            let index = state.session_rows.as_ref().expect("session cache").rows.iter().position(|row| matches!(&row.kind, SessionRowKind::Activity(activity, _) if if done { matches!(&activity.kind.kind, AgentTreeKind::ActivityDone { .. }) } else { matches!(&activity.kind.kind, AgentTreeKind::ActivityMore { .. }) })).expect("summary row");
            for _ in 0..=index {
                state.handle_raw_events(vec![key(KeyCode::Down)]);
                state.compose(width, 40).expect("keyboard selection");
            }
            state.handle_raw_events(vec![key(KeyCode::Enter)]);
            assert!(
                matches!(state.overlay.as_ref(), Some(ClientShellOverlay::AgentActivity(overlay)) if overlay.selected_node.is_none()),
                "{width} done={done}"
            );
        }
    }
    let mut state = classic_state_with(AgentPanelSortConfig::Spaces, scale_snapshot(30, 0));
    enable_workbench(&mut state);
    state.workbench.dock.focused = PanelId::Workspaces;
    state.compose(120, 14).expect("long session list");
    assert!(state.hits.workspace_max_scroll > 0);
    for _ in 0..25 {
        state.handle_raw_events(vec![key(KeyCode::Down)]);
        state.compose(120, 14).expect("keyboard reveal");
    }
    assert!(state.workspace_scroll > 0);
    assert!(state.hits.session_agents.iter().any(|(rect, _, _)| state
        .compose_buffer
        .as_ref()
        .expect("buffer")[(rect.x, rect.y)]
        .modifier
        .contains(Modifier::UNDERLINED)));
    let selection = state.workbench.agent_keyboard_target.clone();
    state.set_snapshot(Box::new(scale_snapshot(0, 0)));
    state.compose(120, 14).expect("removed selected session");
    let outcome = state.handle_raw_events(vec![key(KeyCode::Enter)]);
    assert!(outcome.actions.is_empty());
    assert!(selection.is_some() && state.workbench.agent_keyboard_target.is_none());
}

#[test]
fn collapsed_activity_keyboard_target_never_opens_the_hidden_node() {
    use super::super::dock::PanelId;
    let mut state = classic_state_with(AgentPanelSortConfig::Spaces, summary_snapshot());
    enable_workbench(&mut state);
    state.workbench.dock.focused = PanelId::Workspaces;
    let key = "agent-activity:pane:pane_0";
    state.toggle_collapsed_group(&ClientEndpointId::Local, key.into());
    state.compose(120, 40).expect("expanded sessions");
    let count = state
        .session_rows
        .as_ref()
        .expect("session cache")
        .rows
        .len();
    for _ in 0..count {
        state.handle_raw_events(vec![RawInputEvent::Key(crate::input::TerminalKey::new(
            KeyCode::Down,
            KeyModifiers::empty(),
        ))]);
    }
    assert!(state.workbench.agent_keyboard_target.is_some());
    state.toggle_collapsed_group(&ClientEndpointId::Local, key.into());
    state.compose(120, 40).expect("collapsed sessions");
    let outcome = state.handle_raw_events(vec![RawInputEvent::Key(
        crate::input::TerminalKey::new(KeyCode::Enter, KeyModifiers::empty()),
    )]);
    assert!(
        state.overlay.is_none() && outcome.actions.is_empty(),
        "隐藏节点不可激活"
    );
    assert!(state.workbench.agent_keyboard_target.is_none());
}

#[test]
fn agents_keyboard_navigation_does_not_intercept_other_panels_or_modes() {
    use super::super::dock::PanelId;
    let mut state = classic_state_with(AgentPanelSortConfig::Spaces, summary_snapshot());
    enable_workbench(&mut state);
    let key = crate::input::TerminalKey::new(KeyCode::Down, KeyModifiers::empty());
    state.workbench.dock.focused = PanelId::Workspaces;
    for mode in [
        ClientShellMode::Copy,
        ClientShellMode::Navigate,
        ClientShellMode::Prefix,
        ClientShellMode::Resize,
    ] {
        state.mode = mode;
        assert!(!state.agent_tree_key(&key, &mut ClientShellInput::default()));
    }
    state.mode = ClientShellMode::Terminal;
    for panel in [PanelId::Terminal(1), PanelId::Agents, PanelId::Monitor] {
        state.workbench.dock.focused = panel;
        assert!(!state.agent_tree_key(&key, &mut ClientShellInput::default()));
    }
    assert!(state.workbench.agent_keyboard_target.is_none());
}

#[test]
fn tree_multi_line_agent_rows_draw_continuation_guides() {
    use crate::config::AgentSidebarToken as Token;
    let mut config = Config::default();
    config.ui.status_indicators = crate::config::StatusIndicatorStyle::Symbols;
    config.ui.sidebar.agents.rows =
        vec![vec![Token::StateIcon, Token::Agent], vec![Token::StateText]];
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&config));
    state.set_snapshot(Box::new(two_workspace_snapshot()));
    state.set_pane_surface(surface());
    state.compose(106, 30).expect("多行配置帧");
    let status = &crate::i18n::texts().status;
    for (pane, primary, secondary) in [
        ("pane_1", "○one", status.idle),
        ("pane_2", "×two", status.blocked),
    ] {
        let lines = rect_rows(&state, classic_agent_rect(&state, pane));
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(compact(&lines[0]), primary, "首行不重复状态文案");
        assert_eq!(compact(&lines[1]), compact(secondary));
        assert_eq!(
            lines[0].chars().take_while(|ch| ch.is_whitespace()).count(),
            lines[1].chars().take_while(|ch| ch.is_whitespace()).count()
        );
    }
    let mut projected = summary_snapshot();
    projected.agents[0].agent_status = AgentStatus::Working;
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&config));
    state.set_snapshot(Box::new(projected));
    state.set_pane_surface(surface());
    state.toggle_collapsed_group(
        &ClientEndpointId::Local,
        "agent-activity:pane:pane_0".into(),
    );
    state.compose(106, 30).expect("expanded multiline session");
    let owner = classic_agent_rect(&state, "pane_0");
    let lines = rect_rows(&state, owner);
    assert!(
        lines[0].trim_start().starts_with("◐ agent-0") && lines[0].contains("▾2"),
        "{lines:?}"
    );
    assert_eq!(compact(&lines[1]), compact(status.working));
    assert_eq!(state.hits.session_activity_rows[0].rect.y, owner.bottom());
    assert!(state
        .hits
        .session_activity_rows
        .iter()
        .all(|hit| hit.owner_key == "pane:pane_0"));
}

/// 重复 id / 自指父节点的活动数据不会让构建死循环，也不重复出行。
#[test]
fn tree_activity_nodes_tolerate_cycles_and_duplicate_ids() {
    let mut projected = activity_snapshot(0);
    projected.agents[0].activity.nodes = vec![
        ClientShellActivityNode {
            id: "a".into(),
            label: "root".into(),
            ..Default::default()
        },
        ClientShellActivityNode {
            id: "a".into(),
            label: "dup".into(),
            parent_id: Some("a".into()),
            ..Default::default()
        },
        ClientShellActivityNode {
            id: "b".into(),
            label: "self".into(),
            parent_id: Some("b".into()),
            ..Default::default()
        },
    ];
    projected.agents[0].activity.total = 3;
    projected.agents[0].agent_status = AgentStatus::Working;
    let mut state = classic_state_with(AgentPanelSortConfig::Spaces, projected);
    state.toggle_collapsed_group(
        &ClientEndpointId::Local,
        "agent-activity:pane:pane_0".into(),
    );
    state.toggle_collapsed_group(&ClientEndpointId::Local, "agent-node:pane:pane_0:a".into());
    state.compose(106, 30).expect("环状活动帧");
    let labels = state
        .hits
        .session_activity_rows
        .iter()
        .map(|hit| rect_rows(&state, hit.rect)[0].trim().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        labels.len(),
        2,
        "root 与它的 dup 子节点各出一次；自指节点不可达: {labels:?}"
    );
    assert!(
        labels[0].contains("root") && labels[1].contains("dup"),
        "{labels:?}"
    );
}

/// 外部来源条目：按 source 单列「外部」分组（可折叠），条目行带状态、标签、agent
/// 名、「暂不可读」与活动徽标；点条目行打开活动窗口，右键有「查看 Agent 活动」。
#[test]
fn tree_external_agents_group_by_source_and_open_the_activity_window() {
    let mut projected = two_workspace_snapshot();
    projected.external_agents = vec![
        ClientShellExternalAgent {
            external_id: "zcode:abc".into(),
            source: "zcode".into(),
            agent_status: AgentStatus::Working,
            label: "fix login".into(),
            readable: true,
            agent: Some("zcode".into()),
            cwd: None,
            updated_at_ms: None,
            activity: ClientShellAgentActivity {
                running: 1,
                total: 2,
                truncated: false,
                nodes: vec![ClientShellActivityNode {
                    id: "n1".into(),
                    label: "sub".into(),
                    ..Default::default()
                }],
                ..Default::default()
            },
        },
        ClientShellExternalAgent {
            external_id: "zcode:def".into(),
            source: "zcode".into(),
            agent_status: AgentStatus::Idle,
            label: String::new(),
            readable: false,
            agent: None,
            cwd: None,
            updated_at_ms: None,
            activity: Default::default(),
        },
    ];
    let mut state = classic_state_with(AgentPanelSortConfig::Spaces, projected.clone());
    // 加宽侧栏，让「标签 + agent 名 + 徽标」都放得下（窄时按值 > 标签 > 次要信息裁）。
    state.sidebar_width = 36;
    state.sidebar_width_manual = true;
    state.compose(106, 34).expect("外部分组帧");

    let keys = local_toggle_keys(&state);
    assert_eq!(keys.last().copied(), Some("agent-activity:ext:zcode:abc"));
    assert!(keys.contains(&"agent-external:zcode"));
    let texts = &crate::i18n::texts().agent_panel;
    let group = rect_rows(
        &state,
        toggle_rect(&state, &ClientEndpointId::Local, "agent-external:zcode"),
    );
    assert!(group[0].trim_start().starts_with('▾'), "{group:?}");
    assert!(
        compact(&group[0]).contains(&compact(texts.external_group)) && group[0].contains("zcode"),
        "{group:?}"
    );
    assert!(group[0].trim_end().ends_with('2'), "{group:?}");
    assert_eq!(
        state
            .hits
            .session_external_agents
            .iter()
            .map(|(_, _, id)| id.as_str())
            .collect::<Vec<_>>(),
        ["zcode:abc", "zcode:def"]
    );
    let abc = rect_rows(&state, state.hits.session_external_agents[0].0);
    assert!(
        abc[0].trim_start().starts_with("◐ fix login zcode") && abc[0].trim_end().ends_with("▸1"),
        "{abc:?}"
    );
    assert!(state.hits.session_activity_rows.is_empty());
    let def = rect_rows(&state, state.hits.session_external_agents[1].0);
    assert!(
        def[0].trim_start().starts_with("○ zcode:def"),
        "没有标签时回退到 id: {def:?}"
    );
    assert!(
        compact(&def[0]).contains(&compact(texts.external_unreadable)),
        "{def:?}"
    );
    state.handle_raw_events(vec![click(toggle_rect(
        &state,
        &ClientEndpointId::Local,
        "agent-activity:ext:zcode:abc",
    ))]);
    state.compose(106, 34).expect("expanded external activity");
    let node = rect_rows(&state, state.hits.session_activity_rows[0].rect);
    assert!(node[0].starts_with("└── · sub"), "{node:?}");
    assert_eq!(
        state.hits.session_activity_rows[0].owner_key,
        "ext:zcode:abc"
    );
    let row = state.hits.session_external_agents[1].0;
    let outcome = state.handle_raw_events(vec![click_at(row.x + 8, row.y)]);
    assert!(
        matches!(state.overlay.as_ref(), Some(ClientShellOverlay::AgentActivity(overlay)) if overlay.owner == super::super::agent_activity_overlay::AgentActivityOwner::External { external_id: "zcode:def".into() })
    );
    assert!(!outcome.actions.iter().any(|action| matches!(action, ClientShellAction::Endpoint { request, .. } if matches!(&request.method, crate::api::schema::Method::PaneFocus(_)))));
    state.overlay = None;
    let row = state.hits.session_external_agents[0].0;
    state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Right),
        column: row.x + 8,
        row: row.y,
        modifiers: KeyModifiers::empty(),
    })]);
    assert!(
        matches!(state.overlay.as_ref(), Some(ClientShellOverlay::ContextMenu(menu)) if matches!(&menu.target, ClientContextMenuTarget::ExternalAgent { external_id, .. } if external_id == "zcode:abc"))
    );
    state.overlay = None;
    let group = toggle_rect(&state, &ClientEndpointId::Local, "agent-external:zcode");
    state.handle_raw_events(vec![click_at(group.x + 3, group.y)]);
    state.compose(106, 34).expect("collapsed external source");
    assert!(
        state.hits.session_external_agents.is_empty()
            && state.hits.session_activity_rows.is_empty()
    );
    assert!(local_toggle_keys(&state).contains(&"agent-external:zcode"));
    let mut state = classic_state_with(AgentPanelSortConfig::Launch, projected.clone());
    state.compose(106, 34).expect("launch external sessions");
    assert_eq!(state.hits.session_external_agents.len(), 2);
    let last_agent = state
        .hits
        .session_agents
        .iter()
        .map(|(rect, _, _)| rect.y)
        .max()
        .expect("agent rows");
    assert!(state
        .hits
        .session_external_agents
        .iter()
        .all(|(rect, _, _)| rect.y > last_agent));
    projected.agent_view_label = Some("blocked".into());
    let mut state = classic_state_with(AgentPanelSortConfig::Spaces, projected);
    state.compose(106, 34).expect("filtered sessions");
    assert!(state.hits.session_external_agents.is_empty());
    assert!(local_toggle_keys(&state)
        .iter()
        .all(|key| !key.starts_with("agent-external:")));
}

/// 在线 / 离线端点的行：离线端点的整棵子树变暗（DIM），机器行带状态文案。
#[test]
fn tree_offline_endpoint_rows_are_dimmed_with_a_status_label() {
    let (mut state, remote) = federated_state(AgentPanelSortConfig::Spaces);
    state.set_endpoint_status(&remote, ClientEndpointStatus::Reconnecting);
    state.compose(106, 40).expect("离线端点帧");
    let machine = state
        .hits
        .machines
        .iter()
        .find(|hit| hit.endpoint_id == remote)
        .expect("remote machine")
        .rect;
    let line = &rect_rows(&state, machine)[0];
    assert!(
        line.contains("▾ Build") && line.contains(state.spinner_glyph()),
        "重连中的状态字形: {line:?}"
    );
    let status_label =
        crate::client::shell::endpoints::endpoint_status_label(ClientEndpointStatus::Reconnecting);
    assert!(compact(line).contains(&compact(status_label)), "{line:?}");
    let buffer = state.compose_buffer.as_ref().expect("缓冲");
    assert!(
        buffer[(machine.x + 4, machine.y)]
            .style()
            .add_modifier
            .contains(ratatui::style::Modifier::DIM),
        "离线端点整行变暗"
    );
    let (rect, endpoint_id, _) = &state.hits.session_agents[3];
    assert_eq!(endpoint_id, &remote);
    assert!(
        buffer[(rect.x + 6, rect.y)]
            .style()
            .add_modifier
            .contains(ratatui::style::Modifier::DIM),
        "离线端点的 agent 行也变暗"
    );
}

/// 一次统一树的 kind 清点：Spaces 树里每种节点的种类与深度都对得上层级设计。
#[test]
fn tree_rows_follow_the_machine_workspace_tab_agent_activity_hierarchy() {
    let (mut state, _) = federated_state(AgentPanelSortConfig::Spaces);
    let mut projected = activity_snapshot(2);
    let mut tab = projected.tabs[0].clone();
    tab.tab_id = "tab_x".into();
    tab.label = "x".into();
    tab.focused = false;
    projected.tabs.push(tab);
    state.set_snapshot(Box::new(projected));
    state.toggle_collapsed_group(
        &ClientEndpointId::Local,
        "agent-activity:pane:pane_0".into(),
    );
    state.compose(106, 40).expect("层级帧");
    use super::super::chrome::sessions::SessionRowKind;
    let rows = &state.session_rows.as_ref().expect("session cache").rows;
    let shape = rows
        .iter()
        .map(|row| match &row.kind {
            SessionRowKind::Machine { collapsed, .. } => ("machine", 0, true, *collapsed),
            SessionRowKind::Workspace { indent, toggle, .. } => (
                "workspace",
                1 + indent / 2,
                toggle.is_some(),
                toggle.as_ref().is_some_and(|(_, collapsed)| *collapsed),
            ),
            SessionRowKind::Tab {
                indent,
                collapsed,
                rollup,
                ..
            } => ("tab", 1 + indent / 2, rollup.is_some(), *collapsed),
            SessionRowKind::Agent(agent) => (
                "agent",
                2 + agent.indent / 2,
                agent.activity.is_some(),
                agent
                    .activity
                    .as_ref()
                    .is_some_and(|(_, expanded, _)| !expanded),
            ),
            SessionRowKind::Activity(activity, indent) => (
                "activity",
                2 + indent / 2 + u16::from(activity.depth),
                activity.has_children,
                activity.collapsed,
            ),
            SessionRowKind::Terminal { indent, .. } => ("terminal", 2 + indent / 2, false, false),
            other => panic!("unexpected hierarchy row: {other:?}"),
        })
        .collect::<Vec<_>>();
    assert_eq!(
        shape,
        [
            ("machine", 0, true, false),
            ("workspace", 1, true, false),
            ("tab", 2, true, false),
            ("agent", 3, true, false),
            ("activity", 4, true, true),
            ("tab", 2, false, false),
            ("machine", 0, true, false),
            ("workspace", 1, true, false),
            ("agent", 2, false, false),
            ("agent", 2, false, false),
        ]
    );
}

/// agent 行右键动作（W3 接上）：「重命名」沿用 pane 重命名浮层；「关闭窗格」发
/// `pane.close`。其它端点的 agent：重命名打开同一浮层、目标带上端点，关闭直接
/// 发往该端点；两者都不切换当前端点。
#[test]
fn tree_agent_context_actions_rename_and_close_the_agent_pane() {
    use super::super::agent_activity_overlay::AgentActivityOwner;
    let pane = |pane_id: &str| AgentActivityOwner::Pane {
        pane_id: pane_id.into(),
    };
    let (mut state, remote) = federated_state(AgentPanelSortConfig::Spaces);
    state.compose(106, 40).expect("联邦帧");

    // 重命名（当前端点）：pane 重命名浮层，目标是这个 pane。
    let mut outcome = ClientShellInput::default();
    state.activate_agent_context_action(
        ClientEndpointId::Local,
        pane("pane_2"),
        ClientContextMenuAction::RenameAgent,
        &mut outcome,
    );
    match state.overlay.as_ref() {
        Some(ClientShellOverlay::Rename(rename)) => {
            assert!(matches!(
                &rename.target,
                ClientRenameTarget::Pane { pane_id } if pane_id == "pane_2"
            ));
            assert_eq!(
                rename.title,
                crate::i18n::texts().dialogs.rename_pane,
                "浮层标题同键盘重命名路径（dialogs.rename_pane）"
            );
        }
        other => panic!("应打开 pane 重命名浮层: {other:?}"),
    }
    assert!(outcome.actions.is_empty());
    state.overlay = None;

    // 重命名（其它在线端点）：同一个浮层，目标带上端点，不切换当前端点
    // （文档终审 D9；提交路径见
    // `renaming_a_remote_agent_pane_goes_to_its_endpoint_and_dims_when_offline`）。
    let mut outcome = ClientShellInput::default();
    state.activate_agent_context_action(
        remote.clone(),
        pane("pane_1"),
        ClientContextMenuAction::RenameAgent,
        &mut outcome,
    );
    assert!(outcome.actions.is_empty(), "打开浮层不发请求、不切端点");
    assert!(matches!(
        state.overlay.as_ref(),
        Some(ClientShellOverlay::Rename(rename)) if matches!(
            &rename.target,
            ClientRenameTarget::EndpointPane { endpoint_id, pane_id }
                if endpoint_id == &remote && pane_id == "pane_1"
        )
    ));
    state.overlay = None;

    // 关闭（当前端点）：pane.close。
    let mut outcome = ClientShellInput::default();
    state.activate_agent_context_action(
        ClientEndpointId::Local,
        pane("pane_3"),
        ClientContextMenuAction::CloseAgentPane,
        &mut outcome,
    );
    let [ClientShellAction::Endpoint { request, .. }] = &outcome.actions[..] else {
        panic!("关闭应走端点 API: {:?}", outcome.actions);
    };
    assert!(matches!(
        &request.method,
        crate::api::schema::Method::PaneClose(target) if target.pane_id == "pane_3"
    ));

    // 关闭（其它在线端点）：直接发往该端点，不切换当前端点。
    let mut outcome = ClientShellInput::default();
    state.activate_agent_context_action(
        remote.clone(),
        pane("pane_2"),
        ClientContextMenuAction::CloseAgentPane,
        &mut outcome,
    );
    let [ClientShellAction::EndpointRequest {
        endpoint_id,
        request,
        ..
    }] = &outcome.actions[..]
    else {
        panic!("远端关闭应发往该端点: {:?}", outcome.actions);
    };
    assert_eq!(endpoint_id, &remote);
    assert!(matches!(
        &request.method,
        crate::api::schema::Method::PaneClose(target) if target.pane_id == "pane_2"
    ));
    assert!(state.active_endpoint_id.is_local());
}

#[test]
fn empty_agent_panel_draws_the_no_agents_empty_state() {
    use super::super::chrome::sessions::SessionHit;
    for workbench in [false, true] {
        let cols = if workbench { 160 } else { 106 };
        let mut plain = classic_state_with(AgentPanelSortConfig::Spaces, snapshot());
        if workbench {
            plain.workbench.inspector_visibility.insert(
                "local".into(),
                crate::client::shell::dock::InspectorVisibility::Shown,
            );
            enable_workbench(&mut plain);
        }
        plain.compose(cols, 32).expect("plain terminal session");
        assert!(plain.hits.agent_body.is_empty() && plain.hits.session_agents.is_empty());
        assert!(plain
            .hits
            .sessions
            .iter()
            .any(|(_, hit)| matches!(hit, SessionHit::Terminal(_, pane) if pane == "pane_1")));
        if workbench {
            let text = compact(&rect_rows(&plain, plain.hits.inspector_body).join("\n"));
            assert!(
                text.contains(&compact(super::super::observability::tr(
                    "No agent selected",
                    "未选择智能体"
                ))),
                "{text}"
            );
        }
        let mut empty = snapshot();
        empty.workspaces.clear();
        empty.tabs.clear();
        empty.panes.clear();
        empty.agents.clear();
        empty.focused_workspace_id = None;
        empty.focused_pane_id = None;
        let mut state = classic_state_with(AgentPanelSortConfig::Spaces, empty);
        if workbench {
            enable_workbench(&mut state);
        }
        state.compose(cols, 32).expect("empty session list");
        let body = state.session_view.body;
        assert!(body.height >= 3);
        let expected = compact(super::super::observability::tr(
            "No sessions yet",
            "还没有会话",
        ));
        let rows = rect_rows(&state, body);
        let (y, line) = rows
            .iter()
            .enumerate()
            .find(|(_, line)| compact(line).contains(&expected))
            .expect("visible empty-state title");
        assert!(y > 0, "空状态垂直居中：{rows:?}");
        let x = body.x + line.chars().take_while(|ch| ch.is_whitespace()).count() as u16;
        assert!(x > body.x, "空状态水平居中：{line:?}");
        let cell = &state.compose_buffer.as_ref().expect("buffer")[(x, body.y + y as u16)];
        assert_eq!(cell.fg, state.config.palette.text);
        assert!(cell.modifier.contains(Modifier::BOLD));
        let mut filtered = snapshot();
        filtered.agent_view_label = Some("working".into());
        let mut state = classic_state_with(AgentPanelSortConfig::Spaces, filtered);
        if workbench {
            enable_workbench(&mut state);
        }
        state.compose(cols, 32).expect("no matching agent sessions");
        let text = compact(&body_text(&state));
        assert!(
            text.contains(&compact(crate::i18n::texts().sidebar.no_matching_agents)),
            "{text}"
        );
        assert!(
            !text.contains(&compact(crate::i18n::texts().mobile.no_agents)),
            "{text}"
        );
        assert!(state
            .hits
            .sessions
            .iter()
            .any(|(_, hit)| matches!(hit, SessionHit::Terminal(_, pane) if pane == "pane_1")));
    }
}

#[test]
fn agent_panel_header_keeps_a_gap_between_title_and_sort_toggle() {
    use super::super::chrome::sessions::SessionHit;
    use super::super::dock::PanelId;
    let _lang = crate::i18n::lang_guard(crate::i18n::Lang::En);
    let mut state = workbench_state(AgentPanelSortConfig::Spaces);
    let mut seen_truncated = false;
    let mut seen_full = false;
    for cols in [100, 160] {
        state.compose(cols, 32).expect("session header");
        assert!(!state.session_view.rail);
        let label = state.hits.agent_sort_toggle;
        let grouping = state
            .hits
            .sessions
            .iter()
            .find(|(_, hit)| *hit == SessionHit::Grouping)
            .expect("grouping control")
            .0;
        let panel = state
            .workbench
            .geometry
            .panels
            .iter()
            .find(|(id, _)| *id == PanelId::Workspaces)
            .expect("session panel")
            .1;
        assert!(!label.is_empty() && !grouping.is_empty());
        assert!(label.y > panel.y, "面板标题与排序控件分行");
        assert_eq!(label.y, grouping.y);
        assert!(
            label.right() < grouping.x,
            "标签和分组按钮之间有空格：{label:?} {grouping:?}"
        );
        assert_eq!(grouping.right(), state.session_view.grouping.right() - 1);
        let text = sort_label(&state);
        if label.width < crate::ui::display_width_u16("Workspaces") {
            seen_truncated = true;
            assert!(text.ends_with('…'), "{cols}: {text}");
        } else {
            seen_full = true;
            assert_eq!(text, "Workspaces");
        }
    }
    assert!(seen_truncated && seen_full);
}

#[test]
fn flat_view_rows_do_not_depend_on_the_first_tree_row() {
    use super::super::chrome::sessions::SessionRowKind;
    let mut state = classic_state(AgentPanelSortConfig::Spaces);
    state.toggle_collapsed_group(&ClientEndpointId::Local, agent_group_key("ws_1"));
    state.compose(106, 40).expect("collapsed tree");
    let rows = &state.session_rows.as_ref().expect("session cache").rows;
    assert!(rows.len() > 1);
    assert!(matches!(
        &rows[0].kind,
        SessionRowKind::Workspace {
            toggle: Some((_, true)),
            ..
        }
    ));
    assert_eq!(cached_pane_ids(&state), ["pane_3"]);
    let tree_address = rows.as_ptr();
    state.config.agent_panel_sort = AgentPanelSortConfig::Launch;
    state.config_epoch += 1;
    state.refresh_session_rows();
    assert_eq!(cached_pane_ids(&state), ["pane_2", "pane_3", "pane_1"]);
    assert_ne!(
        state
            .session_rows
            .as_ref()
            .expect("flat session cache")
            .rows
            .as_ptr(),
        tree_address
    );
    let targets = state
        .session_rows
        .as_ref()
        .expect("session cache")
        .rows
        .iter()
        .enumerate()
        .filter_map(|(index, row)| match &row.kind {
            SessionRowKind::Agent(agent) => Some((index, agent.pane_id.clone())),
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut listed = Vec::new();
    for (start, pane) in targets {
        state.workspace_scroll = start;
        state.reveal_focused_workspace = false;
        state
            .compose(106, 2)
            .expect("real one-row session viewport");
        assert_eq!(state.session_view.body.height, 1);
        let (rect, _, shown) = state.hits.session_agents.first().expect("visible session");
        assert_eq!(shown, &pane);
        assert_eq!(*rect, rect.intersection(state.session_view.body));
        listed.push(shown.clone());
    }
    assert_eq!(listed, ["pane_2", "pane_3", "pane_1"]);
}

#[test]
fn one_row_agent_list_keeps_the_agent_name_visible() {
    use super::super::chrome::sessions::SessionRowKind;
    use crate::config::AgentSidebarToken as Token;
    let mut state = classic_state(AgentPanelSortConfig::Launch);
    state.config.agents.rows = vec![
        vec![Token::StateIcon, Token::Workspace],
        vec![Token::Agent, Token::Custom("absent".into())],
    ];
    state.config_epoch += 1;
    state.refresh_session_rows();
    let target = state
        .session_rows
        .as_ref()
        .expect("sessions")
        .rows
        .iter()
        .position(
            |row| matches!(&row.kind, SessionRowKind::Agent(agent) if agent.pane_id == "pane_1"),
        )
        .expect("agent row");
    for (height, expected) in [(2, 1), (3, 2)] {
        state.workspace_scroll = target;
        state.reveal_focused_workspace = false;
        state.compose(106, height).expect("short viewport");
        assert_eq!(state.session_view.body.height, expected);
        let rect = classic_agent_rect(&state, "pane_1");
        assert_eq!(rect.height, expected);
        let lines = rect_rows(&state, rect);
        if expected == 1 {
            let line = compact(&lines[0]);
            assert!(
                line.starts_with('○') && line.contains("one") && line.contains("client-shell"),
                "{line}"
            );
        } else {
            assert!(compact(&lines[0]).starts_with("○client-shell"), "{lines:?}");
            assert_eq!(compact(&lines[1]), "one");
        }
    }
}

/// `rect` 里从某格起依次拼出 `text` 的第一个单元格（找不到就失败），用来断言
/// 一段 ASCII 文字的样式；CJK 文案传首字即可。
fn text_cell(state: &ClientShellState, rect: Rect, text: &str) -> ratatui::buffer::Cell {
    let buffer = state.compose_buffer.as_ref().expect("保留帧缓冲");
    let wanted = text.chars().map(|ch| ch.to_string()).collect::<Vec<_>>();
    for y in rect.y..rect.bottom() {
        for x in rect.x..rect.right() {
            let spelled = wanted.iter().enumerate().all(|(offset, symbol)| {
                let column = x + offset as u16;
                column < rect.right() && buffer[(column, y)].symbol() == symbol
            });
            if spelled {
                return buffer[(x, y)].clone();
            }
        }
    }
    panic!("{rect:?} 里没有 {text:?}：{:?}", rect_rows(state, rect));
}

fn readable(cell: &ratatui::buffer::Cell) -> f32 {
    crate::ui::color::contrast_ratio(cell.fg, cell.bg).expect("前景与底色都可换算")
}

/// 冒烟 L3：agent 叶子行的名称与状态文案对行底色的对比度 ≥ 4.5:1（原先 muted 色
/// 只有 3.36–3.59:1）；「已阻塞」除颜色外还有文字与粗体，汇总为阻塞的分组头在
/// 计数前多一个粗体「!」——默认的圆点图标下阻塞与工作中同是「●」，不能只靠颜色。
#[test]
fn agent_leaf_rows_are_readable_and_blocked_is_not_color_only() {
    use crate::config::AgentSidebarToken as Token;
    {
        let mut config = Config::default();
        config.ui.agent_panel_sort = AgentPanelSortConfig::Spaces;
        let mut state = ClientShellState::new(ClientShellConfig::from_config(&config));
        state.set_snapshot(Box::new(two_workspace_snapshot()));
        state.set_pane_surface(surface());
        enable_workbench(&mut state);
        state.compose(133, 32).expect("graphite sessions");
        let rect = state
            .hits
            .workspaces
            .iter()
            .find(|hit| hit.workspace_id == "ws_1")
            .expect("workspace")
            .rect;
        let header = rect_rows(&state, rect)[0].clone();
        assert!(header.contains('◆'), "{header}");
    }
    let mut config = Config::default();
    config.ui.agent_panel_sort = AgentPanelSortConfig::Spaces;
    config.ui.status_indicators = crate::config::StatusIndicatorStyle::Dots;
    config.ui.sidebar.agents.rows =
        vec![vec![Token::StateIcon, Token::Agent], vec![Token::StateText]];
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&config));
    state.set_snapshot(Box::new(two_workspace_snapshot()));
    state.set_pane_surface(surface());
    enable_workbench(&mut state);
    state
        .compose(133, 32)
        .expect("configured session state text");
    let status = &crate::i18n::texts().status;
    for (pane, name, label, blocked) in [
        ("pane_1", "one", status.idle, false),
        ("pane_2", "two", status.blocked, true),
        ("pane_3", "three", status.working, false),
    ] {
        let rect = classic_agent_rect(&state, pane);
        let name_cell = text_cell(&state, rect, name);
        assert!(
            readable(&name_cell) >= 4.5,
            "{name} 名称对比度 {}",
            readable(&name_cell)
        );
        let first = label.chars().next().expect("state text").to_string();
        let label_cell = text_cell(&state, rect, &first);
        assert!(
            readable(&label_cell) >= 4.5,
            "{label} 对比度 {}",
            readable(&label_cell)
        );
        assert_eq!(
            label_cell.modifier.contains(Modifier::BOLD),
            blocked,
            "{label}"
        );
    }
    let blocked = state
        .hits
        .workspaces
        .iter()
        .find(|hit| hit.workspace_id == "ws_1")
        .expect("blocked workspace")
        .rect;
    let mark = text_cell(&state, blocked, "!");
    assert!(mark.modifier.contains(Modifier::BOLD));
    let working = state
        .hits
        .workspaces
        .iter()
        .find(|hit| hit.workspace_id == "ws_2")
        .expect("working workspace")
        .rect;
    assert!(!rect_rows(&state, working)[0].contains('!'));
}

fn render_tree_at(
    state: &mut ClientShellState,
    width: u16,
) -> (Vec<(String, String, Rect)>, Buffer) {
    state.sidebar_width = width + 1;
    state.sidebar_width_manual = true;
    state.sidebar_collapsed = false;
    state.sidebar_collapsed_manual = true;
    state.compose(106, 40).expect("actual session rendering");
    let buffer = state
        .compose_buffer
        .as_ref()
        .expect("retained buffer")
        .clone();
    let rows = state
        .hits
        .session_agents
        .iter()
        .map(|(rect, _, pane)| {
            assert_eq!(rect.width, width);
            let text = (rect.x..rect.right())
                .map(|x| buffer[(x, rect.y)].symbol())
                .collect::<String>();
            (pane.clone(), text, *rect)
        })
        .collect();
    (rows, buffer)
}

fn dots_state(
    long_names: bool,
    indicators: crate::config::StatusIndicatorStyle,
) -> ClientShellState {
    let mut projected = two_workspace_snapshot();
    if long_names {
        for agent in &mut projected.agents {
            let name = agent.name.take().unwrap_or_default();
            agent.name = Some(format!("{name}-with-a-long-name"));
        }
    }
    let mut config = Config::default();
    config.ui.agent_panel_sort = AgentPanelSortConfig::Spaces;
    config.ui.status_indicators = indicators;
    if !long_names {
        use crate::config::AgentSidebarToken as Token;
        config.ui.sidebar.agents.rows =
            vec![vec![Token::StateIcon, Token::Agent, Token::StateText]];
    }
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&config));
    state.set_snapshot(Box::new(projected));
    state.set_pane_surface(surface());
    state.compose(106, 40).expect("classic 帧");
    state
}

#[test]
fn blocked_rows_keep_a_non_color_cue_in_narrow_panels() {
    use crate::config::StatusIndicatorStyle;
    let status = &crate::i18n::texts().status;
    for (long_names, width, cue, name) in [
        (false, 30u16, status.blocked, "two"),
        (true, 30, "!", "two-with-a-long-name"),
    ] {
        let mut state = dots_state(long_names, StatusIndicatorStyle::Dots);
        let (rows, buffer) = render_tree_at(&mut state, width);
        let row = |pane: &str| {
            rows.iter()
                .find(|(id, _, _)| id == pane)
                .unwrap_or_else(|| panic!("{width}: {pane} {rows:?}"))
        };
        let (_, blocked, rect) = row("pane_2");
        assert!(
            compact(blocked).contains(name),
            "{width}: 名称完整：{blocked}"
        );
        if long_names {
            assert!(compact(blocked).starts_with('!'), "{blocked}");
        } else {
            assert!(compact(blocked).ends_with(&compact(cue)), "{blocked}");
        }
        let first = cue.chars().next().expect("cue").to_string();
        let cue_cell = (rect.x..rect.right())
            .map(|x| &buffer[(x, rect.y)])
            .find(|cell| cell.symbol() == first)
            .expect("visible cue");
        assert!(cue_cell.modifier.contains(Modifier::BOLD));
        let (_, working, _) = row("pane_3");
        assert!(!working.contains('!'), "{working}");
        if long_names {
            assert!(
                !compact(working).contains(&compact(status.working)),
                "{working}"
            );
        }
    }
    let mut state = dots_state(true, StatusIndicatorStyle::Symbols);
    let (rows, _) = render_tree_at(&mut state, 30);
    let (_, blocked, _) = rows
        .iter()
        .find(|(id, _, _)| id == "pane_2")
        .expect("blocked row");
    assert!(blocked.contains('×'), "{blocked}");
    assert!(!blocked.contains('!'), "符号图标已按形状区分：{blocked}");
}

/// 冒烟 L8：Agents 树的工作区分组头右键打开与工作区列表同一份工作区菜单（截屏
/// 23 里毫无反应，工作区列表里同一工作区右键有菜单，截屏 22）。classic 与
/// workbench 两条路径都走同一个入口。
#[test]
fn right_clicking_a_workspace_group_header_opens_the_workspace_menu() {
    let classic = classic_state(AgentPanelSortConfig::Spaces);
    let workbench = workbench_state(AgentPanelSortConfig::Spaces);
    for (path, mut state, (cols, rows)) in [
        ("classic", classic, (106, 40)),
        ("workbench", workbench, (133, 32)),
    ] {
        state.compose(cols, rows).expect("帧");
        let header = group_rect(&state, "ws_2");
        let press = format!("\x1b[<2;{};{}M", header.x + 4, header.y + 1);
        let outcome = state.handle_input_bytes(press.as_bytes());
        assert!(outcome.repaint, "{path}：右键分组头要重绘");
        match &state.overlay {
            Some(ClientShellOverlay::ContextMenu(menu)) => assert!(
                matches!(
                    &menu.target,
                    ClientContextMenuTarget::Workspace { workspace_id, .. }
                        if workspace_id == "ws_2"
                ),
                "{path}：打开的是 ws_2 的工作区菜单"
            ),
            _ => panic!("{path}：右键工作区分组头没有打开菜单"),
        }
    }
}

/// 真机 L1（realcli claude-08 行 41）：Agents 面板里活动子行的标签放不下时硬截成
/// 「执行 echo 命令并返」，看不出被截断。agent 名早已按 `truncate_end` 带省略号；
/// 活动节点、外部条目与分组头的标签同口径，放不下就截短并以「…」收尾。宽度与
/// 真机一致：侧栏 26 列。
#[test]
fn tree_row_labels_end_with_an_ellipsis_when_truncated() {
    let mut projected = summary_snapshot();
    projected.agents[0].agent_status = AgentStatus::Working;
    projected.agents[0].activity.nodes[0].label = "执行 echo 命令并返回 herdr-probe".into();
    projected.workspaces[0].label = "a-workspace-with-a-long-name".into();
    projected.external_agents = vec![ClientShellExternalAgent {
        external_id: "zcode:abc".into(),
        source: "zcode".into(),
        agent_status: AgentStatus::Working,
        label: "an external session with a long title".into(),
        readable: true,
        agent: None,
        cwd: None,
        updated_at_ms: None,
        activity: Default::default(),
    }];
    let mut state = classic_state_with(AgentPanelSortConfig::Spaces, projected);
    state.sidebar_width = 26;
    state.sidebar_width_manual = true;
    state.compose(106, 30).expect("narrow session labels");
    state.handle_raw_events(vec![click(toggle_rect(
        &state,
        &ClientEndpointId::Local,
        "agent-activity:pane:pane_0",
    ))]);
    state.compose(106, 30).expect("expanded activity labels");
    let activity = compact(&rect_rows(&state, state.hits.session_activity_rows[0].rect)[0]);
    assert!(
        activity.contains("执行echo") && activity.ends_with('…'),
        "{activity}"
    );
    let rect = state
        .hits
        .workspaces
        .iter()
        .find(|hit| hit.workspace_id == "ws_0")
        .expect("workspace")
        .rect;
    let group = rect_rows(&state, rect);
    assert!(
        compact(&group[0]).contains("a-workspace")
            && group[0].contains('…')
            && group[0].trim_end().ends_with('1'),
        "分组头截短且保留汇总：{group:?}"
    );
    let external = compact(&rect_rows(&state, state.hits.session_external_agents[0].0)[0]);
    assert!(
        external.contains("anexternal") && external.ends_with('…'),
        "{external}"
    );
}

/// classic 单端点：按 `size` 合成一帧，SGR 右键 `pane_id` 的 agent 行，再合成一帧
/// 画出菜单。返回菜单条目的动作与画面上可点条目的文字（逐行去空白），随后关掉菜单。
fn classic_agent_menu(
    state: &mut ClientShellState,
    pane_id: &str,
    (width, height): (u16, u16),
) -> (Vec<ClientContextMenuAction>, Vec<String>) {
    state.compose(width, height).expect("classic 帧");
    let rect = state
        .hits
        .session_agents
        .iter()
        .find(|(_, _, pane)| pane == pane_id)
        .map(|(rect, _, _)| *rect)
        .unwrap_or_else(|| panic!("{width}x{height}：{pane_id} 的 agent 行在命中表里"));
    state.handle_input_bytes(format!("\x1b[<2;{};{}M", rect.x + 3, rect.y + 1).as_bytes());
    let Some(ClientShellOverlay::ContextMenu(menu)) = state.overlay.as_ref() else {
        panic!("右键 {pane_id} 应打开菜单：{:?}", state.overlay);
    };
    let actions = menu.items().iter().map(|item| item.action).collect();
    state.compose(width, height).expect("菜单帧");
    let buffer = state.compose_buffer.as_ref().expect("保留帧缓冲");
    let rows = state
        .hits
        .context_menu_rows
        .iter()
        .map(|(rect, _)| {
            compact(
                &(rect.x..rect.right())
                    .map(|x| buffer[(x, rect.y)].symbol())
                    .collect::<String>(),
            )
        })
        .collect();
    state.overlay = None;
    (actions, rows)
}

/// agent 行右键菜单与 agent 种类无关：五家官方 agent、退役名（muse、gemini、droid）
/// 与未知名都列同样四项。窄 / 中 / 宽三档逐帧读出菜单文字，确认菜单确实画出来了。
#[test]
fn agent_context_menu_lists_the_same_actions_for_every_agent() {
    use ClientContextMenuAction as Action;
    let t = &crate::i18n::texts().agent_panel;
    for size in [(80, 24), (106, 40), (200, 60)] {
        for agent in [
            "muse", "gemini", "droid", "my-bot", "claude", "codex", "kimi", "opencode", "pi",
        ] {
            let mut projected = two_workspace_snapshot();
            projected.agents[1].agent = Some(agent.into());
            let mut state = classic_state_with(AgentPanelSortConfig::Spaces, projected);
            let (actions, rows) = classic_agent_menu(&mut state, "pane_2", size);
            assert_eq!(
                actions,
                [
                    Action::FocusAgent,
                    Action::ViewAgentActivity,
                    Action::RenameAgent,
                    Action::CloseAgentPane,
                ],
                "{size:?} {agent}"
            );
            assert!(
                rows.iter()
                    .any(|row| row.starts_with(&compact(t.menu_close))),
                "{size:?} {agent}：菜单确实画出来了：{rows:?}"
            );
        }
    }
}

/// SGR 1006 右键某端点 agent 行（坐标取 0 起的单元格）。
fn right_click_endpoint_agent(
    state: &mut ClientShellState,
    endpoint_id: &ClientEndpointId,
    pane_id: &str,
) {
    let rect = state
        .hits
        .session_agents
        .iter()
        .find(|(_, endpoint, pane)| endpoint == endpoint_id && pane == pane_id)
        .map(|(rect, ..)| *rect)
        .expect("该端点的 agent 行在命中表里");
    let (x, y) = (rect.x + 2, rect.y);
    state.handle_input_bytes(format!("\x1b[<2;{};{}M", x + 1, y + 1).as_bytes());
}

/// 打开中的右键菜单里某个动作的条目下标与是否可点。
fn context_item(state: &ClientShellState, action: ClientContextMenuAction) -> (usize, bool) {
    let Some(ClientShellOverlay::ContextMenu(menu)) = state.overlay.as_ref() else {
        panic!("右键菜单应已打开：{:?}", state.overlay);
    };
    let items = menu.items();
    let index = items
        .iter()
        .position(|item| item.action == action)
        .unwrap_or_else(|| panic!("菜单缺少 {action:?}"));
    (index, items[index].enabled)
}

/// 画面上某段文字（去掉空白比较）第一次出现的那几格。CJK 宽字符的续格是空白，
/// 逐行跳过空白格再比，相邻两个字最多隔一格。
fn text_cells(state: &ClientShellState, text: &str) -> Vec<(u16, u16)> {
    let buffer = state.compose_buffer.as_ref().expect("保留帧缓冲");
    let needle: Vec<String> = text
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .map(String::from)
        .collect();
    let area = buffer.area;
    for y in area.y..area.bottom() {
        let cells: Vec<(u16, &str)> = (area.x..area.right())
            .map(|x| (x, buffer[(x, y)].symbol()))
            .filter(|(_, symbol)| !symbol.trim().is_empty())
            .collect();
        let found = cells.windows(needle.len()).find(|window| {
            window
                .iter()
                .zip(&needle)
                .all(|((_, symbol), ch)| symbol == ch)
                && window.windows(2).all(|pair| pair[1].0 - pair[0].0 <= 2)
        });
        if let Some(window) = found {
            return window.iter().map(|(x, _)| (*x, y)).collect();
        }
    }
    Vec::new()
}

/// 右键 `endpoint_id` 上 `pane_id` 的 agent 行，读出「重命名窗格」：模型里可不可点、
/// 画面上每个字是不是 overlay0 灰、在不在可点命中表里。随后关掉菜单。
fn rename_item_on_screen(
    state: &mut ClientShellState,
    endpoint_id: &ClientEndpointId,
    pane_id: &str,
    (cols, rows): (u16, u16),
) -> (bool, bool, bool) {
    state.overlay = None;
    state.compose(cols, rows).expect("右键前的帧");
    right_click_endpoint_agent(state, endpoint_id, pane_id);
    let (rename, enabled) = context_item(state, ClientContextMenuAction::RenameAgent);
    state.compose(cols, rows).expect("菜单帧");
    let label = crate::i18n::texts().agent_panel.menu_rename;
    let cells = text_cells(state, label);
    assert!(!cells.is_empty(), "{cols}x{rows}：菜单画出「{label}」");
    let overlay0 = state.config.palette.overlay0;
    let buffer = state.compose_buffer.as_ref().expect("保留帧缓冲");
    let gray = cells.iter().all(|&(x, y)| buffer[(x, y)].fg == overlay0);
    let clickable = state
        .hits
        .context_menu_rows
        .iter()
        .any(|(_, index)| *index == rename);
    state.overlay = None;
    (enabled, gray, clickable)
}

/// 文档终审 D9：对其它端点（远端机器）上 agent 的右键「重命名窗格」以前既不置灰，
/// 点了也只会切过去并聚焦。按现有 API 能力改为直接重命名：该端点在线且宣告了
/// `pane.rename` 时打开同一个重命名浮层，提交经 `push_endpoint_method_for` 发往
/// 该端点、不切换当前端点（与「关闭窗格」同口径）；端点离线、或在线但没宣告
/// `pane.rename`（旧版 server）时该项置灰、点不了。窄 / 中 / 宽三档逐格读出该项的
/// 字与颜色：可用时不是灰色，置灰时每个字都是 overlay0 灰（T1 审查轻 6）。
#[test]
fn renaming_a_remote_agent_pane_goes_to_its_endpoint_and_dims_when_unavailable() {
    for size in [(80, 24), (106, 40), (180, 50)] {
        let (cols, rows) = size;
        let (mut state, remote) = federated_state(AgentPanelSortConfig::Spaces);
        let (enabled, gray, clickable) = rename_item_on_screen(&mut state, &remote, "pane_1", size);
        assert!(
            enabled && !gray && clickable,
            "{size:?}：在线远端的「重命名窗格」可用、不是灰色"
        );

        // 点这一项：打开重命名浮层，目标是远端的 pane_1。
        state.compose(cols, rows).expect("联邦帧");
        right_click_endpoint_agent(&mut state, &remote, "pane_1");
        let (rename, _) = context_item(&state, ClientContextMenuAction::RenameAgent);
        state.compose(cols, rows).expect("菜单帧");
        let row = state
            .hits
            .context_menu_rows
            .iter()
            .find(|(_, index)| *index == rename)
            .map(|(rect, _)| *rect)
            .expect("可点的重命名行");
        state.handle_input_bytes(format!("\x1b[<0;{};{}M", row.x + 2, row.y + 1).as_bytes());
        state.handle_input_bytes(format!("\x1b[<0;{};{}m", row.x + 2, row.y + 1).as_bytes());
        match state.overlay.as_ref() {
            Some(ClientShellOverlay::Rename(overlay)) => {
                assert_eq!(overlay.title, crate::i18n::texts().dialogs.rename_pane);
                assert!(
                    matches!(
                        &overlay.target,
                        ClientRenameTarget::EndpointPane { endpoint_id, pane_id }
                            if endpoint_id == &remote && pane_id == "pane_1"
                    ),
                    "{size:?}：浮层目标是远端的窗格：{:?}",
                    overlay.target
                );
            }
            other => panic!("{size:?}：应打开重命名浮层：{other:?}"),
        }

        // 输入新名字回车：pane.rename 直接发往远端，当前端点不变。
        assert!(state.insert_overlay_text("build-agent"));
        let outcome = state.handle_raw_events(vec![RawInputEvent::Key(
            crate::input::TerminalKey::new(KeyCode::Enter, KeyModifiers::empty()),
        )]);
        let [ClientShellAction::EndpointRequest {
            endpoint_id,
            request,
            ..
        }] = &outcome.actions[..]
        else {
            panic!("{size:?}：重命名应发往远端端点：{:?}", outcome.actions);
        };
        assert_eq!(endpoint_id, &remote);
        assert!(matches!(
            &request.method,
            crate::api::schema::Method::PaneRename(params)
                if params.pane_id == "pane_1" && params.label.as_deref() == Some("build-agent")
        ));
        assert!(
            state.active_endpoint_id.is_local(),
            "{size:?}：不切换当前端点"
        );
        assert!(state.overlay.is_none());

        // 在线但没宣告 `pane.rename`（旧版 server）：置灰、画成灰色、点不了。
        state.set_endpoint_methods_for(&remote, Some(vec!["pane.close".into()]));
        let (enabled, gray, clickable) = rename_item_on_screen(&mut state, &remote, "pane_1", size);
        assert!(
            !enabled && gray && !clickable,
            "{size:?}：在线但未宣告 pane.rename 时置灰：{enabled} {gray} {clickable}"
        );
        // 宣告了就恢复可用。
        state.set_endpoint_methods_for(&remote, Some(vec!["pane.rename".into()]));
        let (enabled, gray, clickable) = rename_item_on_screen(&mut state, &remote, "pane_1", size);
        assert!(
            enabled && !gray && clickable,
            "{size:?}：宣告了 pane.rename 就可用"
        );

        // 端点离线：置灰、画成灰色、点不了。
        state.set_endpoint_status(&remote, ClientEndpointStatus::Reconnecting);
        let (enabled, gray, clickable) = rename_item_on_screen(&mut state, &remote, "pane_1", size);
        assert!(
            !enabled && gray && !clickable,
            "{size:?}：离线远端的「重命名窗格」置灰：{enabled} {gray} {clickable}"
        );

        // 本机端点的 agent 照常可重命名。
        state.compose(cols, rows).expect("关菜单");
        let (local, pane) = state
            .hits
            .session_agents
            .iter()
            .find(|(_, endpoint, _)| endpoint.is_local())
            .map(|(_, endpoint, pane)| (endpoint.clone(), pane.clone()))
            .expect("本机 agent 行");
        let (enabled, gray, clickable) = rename_item_on_screen(&mut state, &local, &pane, size);
        assert!(
            enabled && !gray && clickable,
            "{size:?}：本机 agent 照常可重命名"
        );
    }
}

/// 对远端 agent 的窗格发起「重命名窗格」（输入新名字回车）或「关闭窗格」，返回
/// 发往远端的那一个请求。
fn fire_remote_pane_action(
    state: &mut ClientShellState,
    remote: &ClientEndpointId,
    action: ClientContextMenuAction,
) -> (String, crate::api::schema::Request) {
    use super::super::agent_activity_overlay::AgentActivityOwner;
    let mut outcome = ClientShellInput::default();
    state.activate_agent_context_action(
        remote.clone(),
        AgentActivityOwner::Pane {
            pane_id: "pane_1".into(),
        },
        action,
        &mut outcome,
    );
    if action == ClientContextMenuAction::RenameAgent {
        assert!(state.insert_overlay_text("build-agent"));
        outcome = state.handle_raw_events(vec![RawInputEvent::Key(
            crate::input::TerminalKey::new(KeyCode::Enter, KeyModifiers::empty()),
        )]);
    }
    let [ClientShellAction::EndpointRequest {
        endpoint_id,
        boot_id,
        request,
    }] = &outcome.actions[..]
    else {
        panic!("{action:?} 应发往远端端点：{:?}", outcome.actions);
    };
    assert_eq!(endpoint_id, remote);
    (boot_id.clone(), (**request).clone())
}

/// T1 审查轻 5：对其它机器上的窗格「重命名窗格」「关闭窗格」，请求直接发往该端点、
/// 不切换当前端点（D9）。应答回来时该端点仍不是当前端点：以前这类请求不在
/// `pending_request_allows_inactive_endpoint` 白名单里，应答被当作过期请求取消、
/// 静默丢弃，`pane_not_found` 这类失败用户毫无提示。现在放行，失败与当前端点的
/// 动作同一个「操作被拒绝」提示并写出服务端的原因；成功不提示。
/// T1 复审轻 2：关闭需要确认时没法替另一台机器弹确认框，正文以前是服务端的英文
/// 原文、也没说怎么办；现在按界面语言写明原因与下一步（切到那台机器上再关闭）。
#[test]
fn remote_pane_rename_and_close_failures_show_a_notice() {
    for lang in [crate::i18n::Lang::ZhCn, crate::i18n::Lang::En] {
        let _guard = crate::i18n::lang_guard(lang);
        let texts = &crate::i18n::texts().endpoint;
        let needs_confirmation = crate::i18n::fill(
            texts.notice_remote_close_needs_confirmation_fmt,
            &[("label", "Build")],
        );
        for (action, code, message, expected) in [
            (
                ClientContextMenuAction::RenameAgent,
                "pane_not_found",
                "pane pane_1 not found",
                "pane pane_1 not found".to_owned(),
            ),
            (
                ClientContextMenuAction::CloseAgentPane,
                "pane_not_found",
                "pane pane_1 not found",
                "pane pane_1 not found".to_owned(),
            ),
            (
                ClientContextMenuAction::CloseAgentPane,
                "confirmation_required",
                "closing this pane would close a worktree group",
                needs_confirmation.clone(),
            ),
        ] {
            let case = format!("{lang:?} {action:?} {code}");
            let (mut state, remote) = federated_state(AgentPanelSortConfig::Spaces);
            state.compose(106, 40).expect("联邦帧");
            let (boot_id, request) = fire_remote_pane_action(&mut state, &remote, action);
            assert!(
                !state.endpoint_is_active(&remote),
                "用例前提：远端不是当前端点"
            );
            assert!(
                state.pending_request_allows_inactive_endpoint(&request.id),
                "{case}：远端不是当前端点也放行应答"
            );
            let (repaint, actions) = state.handle_endpoint_result(
                &boot_id,
                &request.id,
                Err(ClientShellEndpointError {
                    code: Some(code.into()),
                    message: message.into(),
                }),
            );
            assert!(repaint && actions.is_empty(), "{case}");
            let notice = state
                .visible_endpoint_notice
                .as_ref()
                .unwrap_or_else(|| panic!("{case}：失败要提示"));
            assert_eq!(notice.title, texts.notice_action_rejected, "{case}");
            assert_eq!(notice.body, expected, "{case}");
            assert!(
                !matches!(state.overlay, Some(ClientShellOverlay::ConfirmClose(_))),
                "{case}：不替另一台机器的工作区弹确认框"
            );
            // 吐司按显示宽度折行（最多两行，超出以 … 截断）：取卡片内的文字拼起来比对。
            let frame = state.compose(106, 40).expect("提示帧").frame.clone();
            let card = state.hits.endpoint_notice_toast;
            let text = compact(
                &(card.y + 1..card.bottom().saturating_sub(1))
                    .flat_map(|y| (card.x + 2..card.right().saturating_sub(2)).map(move |x| (x, y)))
                    .map(|(x, y)| {
                        frame.cells[usize::from(y) * usize::from(frame.width) + usize::from(x)]
                            .symbol
                            .clone()
                    })
                    .collect::<String>(),
            );
            let body = compact(&expected);
            let shown = text
                .split_once(&compact(texts.notice_action_rejected))
                .map(|(_, rest)| rest.trim_end_matches('…').to_owned())
                .unwrap_or_default();
            assert!(
                !shown.is_empty() && body.starts_with(&shown),
                "{case}：提示画在画面上：{text}"
            );
        }

        // 成功：不提示，在途记录清掉。
        for action in [
            ClientContextMenuAction::RenameAgent,
            ClientContextMenuAction::CloseAgentPane,
        ] {
            let (mut state, remote) = federated_state(AgentPanelSortConfig::Spaces);
            state.compose(106, 40).expect("联邦帧");
            let (boot_id, request) = fire_remote_pane_action(&mut state, &remote, action);
            let (_, actions) = state.handle_endpoint_result(
                &boot_id,
                &request.id,
                Ok(crate::api::schema::ResponseResult::Ok {}),
            );
            assert!(actions.is_empty());
            assert!(
                state.visible_endpoint_notice.is_none(),
                "{lang:?} {action:?}：成功不提示"
            );
            assert!(!state.pending_requests.contains_key(&request.id));
        }
    }
}

/// T1 复审轻 3：发出远端重命名或关闭后切换当前端点，`reset_endpoint_projection`
/// 以前只留 TextCapture / TextRelease，把这类待处理记录一并清掉：应答回来时找不到
/// 记录，失败静默。应答按目标端点自己的 boot_id 校验，切换后保留是安全的；现在
/// 切到那台机器、或切过去再切回本机，失败提示都照常出现。
#[test]
fn remote_pane_action_failures_survive_an_endpoint_switch() {
    let texts = &crate::i18n::texts().endpoint;
    for action in [
        ClientContextMenuAction::RenameAgent,
        ClientContextMenuAction::CloseAgentPane,
    ] {
        for switch_back in [false, true] {
            let case = format!("{action:?} switch_back={switch_back}");
            let (mut state, remote) = federated_state(AgentPanelSortConfig::Spaces);
            state.compose(106, 40).expect("联邦帧");
            let (boot_id, request) = fire_remote_pane_action(&mut state, &remote, action);
            assert!(
                state.activate_endpoint_projection(&remote),
                "{case}：切到远端"
            );
            assert!(state.endpoint_is_active(&remote), "{case}");
            if switch_back {
                assert!(
                    state.activate_endpoint_projection(&ClientEndpointId::Local),
                    "{case}：再切回本机"
                );
                assert!(
                    state.pending_request_allows_inactive_endpoint(&request.id),
                    "{case}：切回本机后远端的应答照样放行"
                );
            }
            assert!(
                state.pending_requests.contains_key(&request.id),
                "{case}：切换端点不丢待处理记录"
            );
            let (repaint, actions) = state.handle_endpoint_result(
                &boot_id,
                &request.id,
                Err(ClientShellEndpointError {
                    code: Some("pane_not_found".into()),
                    message: "pane pane_1 not found".into(),
                }),
            );
            assert!(repaint && actions.is_empty(), "{case}");
            let notice = state
                .visible_endpoint_notice
                .as_ref()
                .unwrap_or_else(|| panic!("{case}：失败提示照常出现"));
            assert_eq!(notice.title, texts.notice_action_rejected, "{case}");
            assert_eq!(notice.body, "pane pane_1 not found", "{case}");
            let text = compact(&frame_rows(&state.compose(106, 40).expect("提示帧")).join(""));
            assert!(
                text.contains(&compact(texts.notice_action_rejected))
                    && text.contains("panepane_1notfound"),
                "{case}：提示画在画面上：{text}"
            );
        }
    }
}

#[test]
fn tree_rows_and_header_keep_a_one_column_inset_from_both_edges() {
    use super::super::chrome::sessions::SessionHit;
    for width in [36u16, 26, 18] {
        let mut state = classic_state(AgentPanelSortConfig::Spaces);
        state.sidebar_width = width;
        state.sidebar_width_manual = true;
        state.compose(106, 30).expect("actual session insets");
        let buffer = state.compose_buffer.as_ref().expect("buffer");
        let header = state
            .hits
            .workspaces
            .iter()
            .find(|hit| hit.workspace_id == "ws_1")
            .expect("workspace row")
            .rect;
        assert_eq!(header.width, state.session_view.body.width);
        assert_eq!(buffer[(header.x, header.y)].symbol(), " ");
        assert_eq!(buffer[(header.x + 1, header.y)].symbol(), "▾");
        assert_eq!(
            buffer[(header.right() - 2, header.y)].symbol(),
            "1",
            "最高优先级汇总计数"
        );
        assert_eq!(buffer[(header.right() - 1, header.y)].symbol(), " ");
        let one = classic_agent_rect(&state, "pane_1");
        assert_eq!(one.width, state.session_view.body.width);
        assert_eq!(buffer[(one.x, one.y)].symbol(), " ");
        assert_eq!(buffer[(one.right() - 1, one.y)].symbol(), " ");
        assert!(rect_rows(&state, one)[0].contains("○ one"));
        let grouping = state
            .hits
            .sessions
            .iter()
            .find(|(_, hit)| *hit == SessionHit::Grouping)
            .expect("grouping control")
            .0;
        assert_eq!(grouping.right(), state.session_view.grouping.right() - 1);
        assert!(!state.hits.agent_sort_toggle.is_empty());
    }
}

#[test]
fn default_width_badges_leave_the_whole_agent_name_visible() {
    for lang in [crate::i18n::Lang::ZhCn, crate::i18n::Lang::En] {
        let _lang = crate::i18n::lang_guard(lang);
        for layout in ["classic 侧栏 26", "工作台 120"] {
            let case = format!("{lang:?}、{layout}");
            let mut projected = badged_snapshot(false);
            projected.agents[0].name = Some("opencode".into());
            let mut state = classic_state_with(AgentPanelSortConfig::Spaces, projected);
            let rect = if layout == "工作台 120" {
                enable_workbench(&mut state);
                state.compose(120, 40).expect("工作台帧");
                state
                    .hits
                    .session_agents
                    .iter()
                    .find(|(_, _, pane_id)| pane_id == "pane_0")
                    .unwrap_or_else(|| panic!("{case}: agent 行不在命中表里"))
                    .0
            } else {
                state.sidebar_width = 26;
                state.sidebar_width_manual = true;
                state.compose(106, 40).expect("classic 帧");
                classic_agent_rect(&state, "pane_0")
            };
            let line = rect_rows(&state, rect)[0].clone();
            assert!(
                line.contains("opencode") && !line.contains('…'),
                "{case}: 名称完整可见，不被徽标截断：{line:?}"
            );
            assert!(line.contains("2"), "{case}: 徽标至少保留数字档：{line:?}");
        }
    }
}
