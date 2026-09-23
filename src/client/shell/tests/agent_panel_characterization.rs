//! Agents 面板统一树的 characterization（行为钉住）测试与渲染扩展 profile。
//!
//! W3 之前 classic / workbench（联邦）/ mobile 是三条并行渲染路径，这里最初钉的
//! 是**重构前**的可见行为；统一树落地后，用例改为钉住新行为：三条桌面路径共用
//! `agent_tree::build_agent_tree`（视图计算阶段进 `AgentRowsCache`）与
//! `agent_tree::render_agent_tree_rows`（渲染只读），mobile 只加活动徽标。
//!
//! - classic：`agent_sidebar::render_agent_panel`，单端点且未启用 workbench，
//!   agent 行写 `hits.agents`；
//! - workbench / 联邦：`endpoint_agents::render_expanded`，agent 行写
//!   `hits.endpoint_agents`；
//! - mobile 与排序来源：`aggregate_navigation::aggregate_agent_rows`。

use super::*;
use crate::client::endpoint::{
    ClientEndpointId, ClientEndpointStatus, ProfileId, SavedSshEndpoint,
};
use crate::client::shell::agent_sidebar::agent_group_key;
use crate::client::shell::agent_tree::{AgentTreeKind, MACHINE_TOGGLE_KEY};
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
    rect_rows(state, state.hits.agent_body).join("\n")
}

/// 表头右侧的排序标签：命中区即绘制区（`render_agent_panel_header`）。
fn sort_label(state: &ClientShellState) -> String {
    compact(&rect_rows(state, state.hits.agent_sort_toggle).join(""))
}

fn classic_hit_ids(state: &ClientShellState) -> Vec<&str> {
    state
        .hits
        .agents
        .iter()
        .map(|(_, pane_id)| pane_id.as_str())
        .collect()
}

/// 统一树的折叠开关命中区：`(端点是否本机, 折叠键)`，按行序。
fn toggle_keys(state: &ClientShellState) -> Vec<(bool, &str)> {
    state
        .hits
        .agent_tree_toggles
        .iter()
        .map(|(_, endpoint_id, key)| (endpoint_id.is_local(), key.as_str()))
        .collect()
}

fn local_toggle_keys(state: &ClientShellState) -> Vec<&str> {
    toggle_keys(state).into_iter().map(|(_, key)| key).collect()
}

fn endpoint_hit_ids(state: &ClientShellState) -> Vec<(ClientEndpointId, String)> {
    state
        .hits
        .endpoint_agents
        .iter()
        .map(|(_, endpoint_id, pane_id)| (endpoint_id.clone(), pane_id.clone()))
        .collect()
}

fn local(pane_id: &str) -> (ClientEndpointId, String) {
    (ClientEndpointId::Local, pane_id.to_owned())
}

fn classic_agent_rect(state: &ClientShellState, pane_id: &str) -> Rect {
    state
        .hits
        .agents
        .iter()
        .find(|(_, id)| id == pane_id)
        .unwrap_or_else(|| panic!("agent 行 {pane_id} 不在命中表里"))
        .0
}

/// 某端点上某折叠键的开关命中区（分组头整行、agent 行只有开关两列）。
fn toggle_rect(state: &ClientShellState, endpoint_id: &ClientEndpointId, key: &str) -> Rect {
    state
        .hits
        .agent_tree_toggles
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
        .federated_agent_rows
        .as_ref()
        .expect("行缓存")
        .rows()
        .iter()
        .filter_map(|row| {
            row.kind
                .agent()
                .map(|agent| (row.kind.endpoint_id.clone(), agent.pane_id.clone()))
        })
        .collect()
}

/// (a) classic + Spaces：「工作区头 + agent 行」两层树，前缀走 kit 树原语。工作区
/// 头以折叠开关开头、汇总最高优先级状态与 agent 数（右对齐）；子行带 `├──` /
/// `└──` 连接线，且子行不再重复工作区名。默认行配置
/// `[状态图标 机器 工作区 标签页] / [agent]` 丢掉分组头承载的 token 后首行只剩
/// 图标，并进下一行成「图标 名称 状态文案」一行。
#[test]
fn characterization_classic_spaces_renders_two_level_workspace_tree() {
    let mut state = classic_state(AgentPanelSortConfig::Spaces);
    state.compose(106, 30).expect("classic agents 面板");

    assert_eq!(
        local_toggle_keys(&state),
        [agent_group_key("ws_1"), agent_group_key("ws_2")],
        "折叠键带 agent-panel: 命名空间，按快照的工作区顺序"
    );
    assert_eq!(classic_hit_ids(&state), ["pane_1", "pane_2", "pane_3"]);
    assert!(
        state.hits.endpoint_agents.is_empty(),
        "classic 路径不写联邦命中区"
    );
    assert_eq!(
        sort_label(&state),
        compact(crate::i18n::texts().sidebar.sort_grouped)
    );

    let first_header = group_rect(&state, "ws_1");
    let header = &rect_rows(&state, first_header)[0];
    assert!(
        header.starts_with("▾ × client-shell"),
        "工作区头: {header:?}"
    );
    assert!(header.trim_end().ends_with("· 2"), "计数右对齐: {header:?}");
    let second_header = group_rect(&state, "ws_2");
    let header = &rect_rows(&state, second_header)[0];
    assert!(header.starts_with("▾ ◐ herdr"), "工作区头: {header:?}");
    assert!(header.trim_end().ends_with("· 1"), "计数右对齐: {header:?}");

    let status = &crate::i18n::texts().status;
    let one = rect_rows(&state, classic_agent_rect(&state, "pane_1"));
    assert_eq!(one.len(), 1, "只剩图标的首行并进名称行: {one:?}");
    assert!(one[0].starts_with("├── ○ one "), "非末子行前缀: {one:?}");
    assert!(
        compact(&one[0]).ends_with(&compact(status.idle)),
        "名称后跟状态文案（次要信息）: {one:?}"
    );
    let two = rect_rows(&state, classic_agent_rect(&state, "pane_2"));
    assert!(two[0].starts_with("└── × two "), "末子行前缀: {two:?}");
    assert!(
        compact(&two[0]).ends_with(&compact(status.blocked)),
        "{two:?}"
    );
    let three = rect_rows(&state, classic_agent_rect(&state, "pane_3"));
    assert!(
        three[0].starts_with("└── ◐ three "),
        "独子也是末子行: {three:?}"
    );
    for lines in [&one, &two, &three] {
        assert!(
            lines
                .iter()
                .all(|line| !line.contains("client-shell") && !line.contains("herdr")),
            "子行不重复工作区名: {lines:?}"
        );
    }

    // 纵向次序：头紧跟自己的子行，下一个工作区头排在上一组最后一个子行之后。
    let ys = [
        first_header.y,
        classic_agent_rect(&state, "pane_1").y,
        classic_agent_rect(&state, "pane_2").y,
        second_header.y,
        classic_agent_rect(&state, "pane_3").y,
    ];
    assert!(ys.windows(2).all(|pair| pair[0] < pair[1]), "行序: {ys:?}");
    assert_eq!(ys[1], first_header.y + 1, "子行紧贴工作区头");
    assert_eq!(
        first_header.width, state.hits.agent_body.width,
        "分组头整行都是折叠开关"
    );
}

/// (a) classic 折叠：折叠键进 `collapsed_groups` 后该工作区的子行消失、开关变
/// `▸`、计数保留；其它工作区不受影响并上移补位。点分组头任意位置都能折叠，
/// 悬浮在分组头上整行高亮。
#[test]
fn characterization_classic_collapsed_workspace_hides_children_and_flips_chevron() {
    let mut state = classic_state(AgentPanelSortConfig::Spaces);
    state.compose(106, 30).expect("展开帧");
    let expanded_second_header_y = group_rect(&state, "ws_2").y;
    let first_header = group_rect(&state, "ws_1");

    // 点在头行的标签文字上（不是开关格）也折叠。
    state.handle_raw_events(vec![click_at(first_header.x + 6, first_header.y)]);
    assert!(
        state.collapsed_groups.contains("agent-panel:ws_1"),
        "本机折叠态存于 collapsed_groups"
    );
    state.compose(106, 30).expect("折叠帧");

    assert_eq!(
        local_toggle_keys(&state),
        [agent_group_key("ws_1"), agent_group_key("ws_2")],
        "折叠后工作区头仍在"
    );
    assert_eq!(classic_hit_ids(&state), ["pane_3"], "折叠组的子行不再可点");
    let first_header = group_rect(&state, "ws_1");
    let header = &rect_rows(&state, first_header)[0];
    assert!(
        header.starts_with("▸ × client-shell"),
        "折叠箭头: {header:?}"
    );
    assert!(header.trim_end().ends_with("· 2"), "计数保留: {header:?}");
    let second_header = group_rect(&state, "ws_2");
    assert!(rect_rows(&state, second_header)[0].starts_with("▾ ◐ herdr"));
    assert_eq!(second_header.y, first_header.y + 1, "后续工作区上移补位");
    assert!(second_header.y < expanded_second_header_y);

    let body = body_text(&state);
    assert!(!body.contains("one") && !body.contains("two"), "{body}");
    assert!(body.contains("three"), "{body}");
    assert!(!body.contains('├'), "只剩一个独子行: {body}");

    // 悬浮在分组头上：整行高亮（同悬浮底色），指针移开恢复。
    let plain_bg = state.compose_buffer.as_ref().expect("缓冲")
        [(first_header.x + 3, first_header.y)]
        .style()
        .bg;
    state.handle_raw_events(vec![moved(first_header.x + 6, first_header.y)]);
    assert!(matches!(
        state.hover,
        Some(super::super::feedback::ChromeHover::AgentTreeToggle(ClientEndpointId::Local, ref key))
            if key == "agent-panel:ws_1"
    ));
    state.compose(106, 30).expect("悬浮帧");
    let hovered_bg = state.compose_buffer.as_ref().expect("缓冲")
        [(first_header.x + 3, first_header.y)]
        .style()
        .bg;
    assert_ne!(hovered_bg, plain_bg, "分组头悬浮整行高亮");
    assert_eq!(hovered_bg, Some(state.config.palette.hover_row_bg()));
}

/// (b) classic + Launch：**平铺**，按全局启动顺序排（不再按工作区分组），行保留
/// 工作区 token 以示归属；没有分组头、没有连接线。`launch_seq` 全为 0（旧 server
/// 不下发）时退回快照顺序。
#[test]
fn characterization_classic_launch_is_flat_in_global_launch_order() {
    let mut state = classic_state(AgentPanelSortConfig::Launch);
    state.compose(106, 30).expect("classic launch 帧");

    assert_eq!(
        sort_label(&state),
        compact(crate::i18n::texts().agent_panel.sort_launch)
    );
    assert!(
        state.hits.agent_tree_toggles.is_empty(),
        "平铺：没有分组头，也没有可展开的活动"
    );
    assert_eq!(
        classic_hit_ids(&state),
        ["pane_2", "pane_3", "pane_1"],
        "全局启动顺序，跨工作区混排"
    );
    assert_no_tree_glyphs(&body_text(&state));
    let two = rect_rows(&state, classic_agent_rect(&state, "pane_2"));
    assert!(
        two[0].starts_with("  × client-shell"),
        "平铺行带工作区名: {two:?}"
    );
    assert!(two[1].starts_with("    two"), "续行缩进对齐名称: {two:?}");
    let three = rect_rows(&state, classic_agent_rect(&state, "pane_3"));
    assert!(three[0].starts_with("  ◐ herdr"), "{three:?}");

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

/// (c) classic 矮面板：列表区不足 3 行（放不下「分组头 + 一个 agent 行」）时
/// 退化为平铺视图——无分组头、无树前缀。行带完整 token（工作区名不再由分组头
/// 承载），按聚合顺序列出全部 agent：这个视图没有分组头，面板内折叠了的工作区
/// 在这里展不开，所以折叠也藏不住它的 agent（审查发现 1）。
#[test]
fn characterization_classic_short_panel_degrades_to_flat_rows() {
    let mut state = classic_state(AgentPanelSortConfig::Spaces);
    state.compose(106, 10).expect("矮面板帧");
    let body = state.hits.agent_body;
    assert!(
        body.height > 0 && body.height < 3,
        "夹具前提：列表区不足 3 行，实际 {body:?}"
    );
    assert!(
        state.hits.agent_tree_toggles.is_empty(),
        "退化后无分组头 / 开关"
    );
    assert_eq!(classic_hit_ids(&state).first(), Some(&"pane_1"));
    let one = rect_rows(&state, classic_agent_rect(&state, "pane_1"));
    assert!(
        one[0].starts_with("  ○ client-shell"),
        "平铺行只有两列前缀，首行带工作区名: {one:?}"
    );
    assert_no_tree_glyphs(&body_text(&state));

    // 同一状态换回足够高的面板，树立即回来：退化只由高度决定。
    state.compose(106, 30).expect("高面板帧");
    assert_eq!(state.hits.agent_tree_toggles.len(), 2);

    // 折叠 ws_1 之后，退化视图仍能逐个滚到全部三个 agent，各带自己的工作区名。
    state.toggle_collapsed_group(&ClientEndpointId::Local, agent_group_key("ws_1"));
    let mut seen = Vec::new();
    for start in 0..3 {
        state.agent_scroll = start;
        state.compose(106, 10).expect("折叠后的矮面板帧");
        assert!(state.hits.agent_tree_toggles.is_empty());
        let pane_id = classic_hit_ids(&state)[0].to_owned();
        let first_line = rect_rows(&state, classic_agent_rect(&state, &pane_id))[0].clone();
        assert_no_tree_glyphs(&first_line);
        seen.push((pane_id, compact(&first_line)));
    }
    assert_eq!(
        seen,
        [
            ("pane_1".to_owned(), "○client-shell".to_owned()),
            ("pane_2".to_owned(), "×client-shell".to_owned()),
            ("pane_3".to_owned(), "◐herdr".to_owned()),
        ],
        "折叠的工作区藏不住平铺视图里的 agent"
    );

    // Launch 下退化视图按全局启动顺序平铺。
    let mut state = classic_state(AgentPanelSortConfig::Launch);
    let mut flat_order = Vec::new();
    for start in 0..3 {
        state.agent_scroll = start;
        state.compose(106, 10).expect("矮面板 launch 帧");
        assert!(state.hits.agent_tree_toggles.is_empty());
        flat_order.push(classic_hit_ids(&state)[0].to_owned());
    }
    assert_eq!(flat_order, ["pane_2", "pane_3", "pane_1"]);
}

/// 联邦折叠侧栏（单列「机器首字母 + 状态图标」）同样走平铺视图：面板内折叠了
/// 工作区，其下的 agent 仍按聚合顺序列出、可点（这里没有分组头，展不开）；机器
/// 层折叠照旧藏起该端点的 agent（上方工作区区的机器行可切换）（审查发现 1）。
#[test]
fn characterization_federated_collapsed_sidebar_keeps_agents_of_collapsed_workspaces() {
    let (mut state, remote) = federated_state(AgentPanelSortConfig::Spaces);
    state.toggle_collapsed_group(&ClientEndpointId::Local, agent_group_key("ws_1"));
    state.toggle_collapsed_group(&remote, agent_group_key("ws_1"));
    state.sidebar_collapsed = true;
    state.compose(106, 40).expect("联邦折叠侧栏帧");

    assert_eq!(
        endpoint_hit_ids(&state),
        [
            local("pane_1"),
            local("pane_2"),
            local("pane_3"),
            (remote.clone(), "pane_1".to_owned()),
            (remote.clone(), "pane_2".to_owned()),
        ],
        "两端的 ws_1 都折叠了，折叠侧栏仍列出全部 agent"
    );
    let cells = |state: &ClientShellState, index: usize| {
        compact(&rect_rows(state, state.hits.endpoint_agents[index].0)[0])
    };
    assert_eq!(cells(&state, 0), "L○", "本机 one：Idle");
    assert_eq!(cells(&state, 1), "L×", "本机 two：Blocked");
    assert_eq!(cells(&state, 3), "B◐", "远端 r-one：Working");

    // 折叠工作区里的远端 agent 照样可点：切到该端点并聚焦它。
    let (rect, _, _) = state.hits.endpoint_agents[3].clone();
    let outcome = state.handle_raw_events(vec![click(rect)]);
    assert!(
        matches!(
            outcome.actions.as_slice(),
            [ClientShellAction::ActivateEndpoint {
                endpoint_id,
                target: Some(ClientEndpointFocusTarget::Pane(pane_id)),
            }] if endpoint_id == &remote && pane_id == "pane_1"
        ),
        "{:?}",
        outcome.actions
    );

    // 机器层折叠：远端的 agent 不再列出。
    state.collapsed_endpoints.insert(remote.clone());
    state.bump_tree_collapse_epoch();
    state.compose(106, 40).expect("远端机器折叠帧");
    assert_eq!(
        endpoint_hit_ids(&state),
        [local("pane_1"), local("pane_2"), local("pane_3")]
    );
}

/// (c) 退化阈值：逐个终端高度扫一遍，「无分组头」当且仅当列表区不足 3 行。
#[test]
fn characterization_classic_tree_degrades_exactly_below_three_body_rows() {
    let mut state = classic_state(AgentPanelSortConfig::Spaces);
    let mut seen_flat = false;
    let mut seen_tree = false;
    for rows in 6..=30 {
        state.agent_scroll = 0;
        state.compose(106, rows).expect("classic 帧");
        let body_height = state.hits.agent_body.height;
        let flat = state.hits.agent_tree_toggles.is_empty();
        assert_eq!(
            flat,
            body_height < 3,
            "终端高 {rows}、列表区高 {body_height}"
        );
        if body_height > 0 {
            assert!(
                !state.hits.agents.is_empty(),
                "两种视图都至少保住一个可点的 agent 行（终端高 {rows}）"
            );
        }
        seen_flat |= flat && body_height > 0;
        seen_tree |= !flat;
    }
    assert!(seen_flat && seen_tree, "扫描范围应同时覆盖两种视图");
}

/// (d) workbench 布局：与 classic 同一棵树——Spaces 下有工作区头与连接线，折叠
/// 态生效；agent 行写端点限定的命中区。切到 Launch 平铺并换行序。
#[test]
fn characterization_workbench_spaces_renders_the_same_workspace_tree_as_classic() {
    let mut state = workbench_state(AgentPanelSortConfig::Spaces);
    state.compose(120, 40).expect("workbench 帧");

    assert_eq!(
        sort_label(&state),
        compact(crate::i18n::texts().sidebar.sort_grouped)
    );
    assert_eq!(
        local_toggle_keys(&state),
        [agent_group_key("ws_1"), agent_group_key("ws_2")]
    );
    assert!(
        state.hits.agents.is_empty(),
        "workbench 不写 classic 命中区"
    );
    assert_eq!(
        endpoint_hit_ids(&state),
        [local("pane_1"), local("pane_2"), local("pane_3")]
    );
    let expected = ["├── ○ one ", "└── × two ", "└── ◐ three "];
    for ((rect, _, _), first) in state.hits.endpoint_agents.iter().zip(expected) {
        let lines = rect_rows(&state, *rect);
        assert_eq!(lines.len(), 1, "与 classic 同样并成一行: {lines:?}");
        assert!(lines[0].starts_with(first), "树前缀: {lines:?}");
        assert!(
            !lines[0].contains("Local"),
            "单端点不画机器层，也不带机器 token: {lines:?}"
        );
    }

    // 折叠某个工作区：子行藏起来。
    state.toggle_collapsed_group(&ClientEndpointId::Local, agent_group_key("ws_1"));
    state.compose(120, 40).expect("折叠后的 workbench 帧");
    assert_eq!(endpoint_hit_ids(&state), [local("pane_3")]);
    assert_eq!(local_toggle_keys(&state).len(), 2);

    // 点表头排序标签切到 Launch：平铺、按启动顺序。
    let toggle = state.hits.agent_sort_toggle;
    assert!(!toggle.is_empty(), "排序标签可点");
    state.handle_raw_events(vec![click(toggle)]);
    assert_eq!(state.config.agent_panel_sort, AgentPanelSortConfig::Launch);
    state.compose(120, 40).expect("launch workbench 帧");
    assert_eq!(
        sort_label(&state),
        compact(crate::i18n::texts().agent_panel.sort_launch)
    );
    assert!(state.hits.agent_tree_toggles.is_empty());
    assert_eq!(
        endpoint_hit_ids(&state),
        [local("pane_2"), local("pane_3"), local("pane_1")]
    );
    assert_no_tree_glyphs(&body_text(&state));
}

/// (d) 联邦（多端点、未启用 workbench）：机器层出现在工作区之上，各端点的折叠
/// 态各自生效；机器层复用 `collapsed_endpoints`，点机器行整体收起。
#[test]
fn characterization_federated_sidebar_nests_machines_above_workspaces() {
    let (mut state, remote) = federated_state(AgentPanelSortConfig::Spaces);
    assert!(!state.workbench.enabled);
    state.toggle_collapsed_group(&ClientEndpointId::Local, agent_group_key("ws_1"));
    state.toggle_collapsed_group(&remote, agent_group_key("ws_1"));
    state.compose(106, 40).expect("联邦帧");

    assert_eq!(
        sort_label(&state),
        compact(crate::i18n::texts().sidebar.sort_grouped)
    );
    assert!(state.hits.agents.is_empty());
    assert_eq!(
        toggle_keys(&state),
        [
            (true, MACHINE_TOGGLE_KEY),
            (true, "agent-panel:ws_1"),
            (true, "agent-panel:ws_2"),
            (false, MACHINE_TOGGLE_KEY),
            (false, "agent-panel:ws_1"),
        ],
        "机器 → 工作区，按端点顺序"
    );
    assert_eq!(
        endpoint_hit_ids(&state),
        [local("pane_3")],
        "两端的 ws_1 都折叠了，只剩本机 ws_2 的独子"
    );
    let local_machine = rect_rows(
        &state,
        toggle_rect(&state, &ClientEndpointId::Local, MACHINE_TOGGLE_KEY),
    );
    assert!(
        local_machine[0].starts_with("▾ ● Local"),
        "{local_machine:?}"
    );
    assert!(
        local_machine[0].trim_end().ends_with("· 3"),
        "机器行计数: {local_machine:?}"
    );
    let remote_machine = rect_rows(&state, toggle_rect(&state, &remote, MACHINE_TOGGLE_KEY));
    assert!(
        remote_machine[0].starts_with("▾ ● Build"),
        "{remote_machine:?}"
    );
    let remote_workspace = rect_rows(&state, toggle_rect(&state, &remote, "agent-panel:ws_1"));
    assert!(
        remote_workspace[0].starts_with("└─▸ ◐ remote-space"),
        "远端工作区挂在机器下且折叠，状态取 r-one 的 Working: {remote_workspace:?}"
    );
    let three = rect_rows(&state, state.hits.endpoint_agents[0].0);
    assert!(
        three[0].starts_with("  └── ◐ three"),
        "深两层：ws_2 是本机的末工作区，机器层的引导线留白: {three:?}"
    );
    assert!(
        three.iter().all(|line| !line.contains("Local")),
        "机器层承载机器名，agent 行不再带机器 token: {three:?}"
    );

    // 点远端机器行：整个端点收起，写进 collapsed_endpoints（与工作区区共用）。
    let remote_row = toggle_rect(&state, &remote, MACHINE_TOGGLE_KEY);
    state.handle_raw_events(vec![click_at(remote_row.x + 4, remote_row.y)]);
    assert!(state.collapsed_endpoints.contains(&remote));
    state.compose(106, 40).expect("远端折叠帧");
    assert_eq!(
        toggle_keys(&state),
        [
            (true, MACHINE_TOGGLE_KEY),
            (true, "agent-panel:ws_1"),
            (true, "agent-panel:ws_2"),
            (false, MACHINE_TOGGLE_KEY),
        ]
    );
    let remote_machine = rect_rows(&state, toggle_rect(&state, &remote, MACHINE_TOGGLE_KEY));
    assert!(
        remote_machine[0].starts_with("▸ ● Build"),
        "{remote_machine:?}"
    );
    // 再点一次展开。
    state.handle_raw_events(vec![click_at(remote_row.x + 4, remote_row.y)]);
    assert!(!state.collapsed_endpoints.contains(&remote));
}

/// (e) workbench 的 `hits.endpoint_agents` 与缓存里的 agent 行一一对应：同序、同
/// （端点, pane）身份，高度等于该行的 token 行数；点击按（端点, pane）路由。
#[test]
fn characterization_workbench_endpoint_agent_hits_mirror_cached_rows() {
    let (mut state, remote) = federated_state(AgentPanelSortConfig::Spaces);
    enable_workbench(&mut state);
    // 机器 2 + 工作区头 3 + agent 5 行：要一个够高的面板才全放得下。
    state.compose(120, 60).expect("workbench 联邦帧");

    let body = state.hits.agent_body;
    let cache = state.federated_agent_rows.as_ref().expect("行缓存");
    let agent_rows = cache
        .rows()
        .iter()
        .filter(|row| row.kind.agent().is_some())
        .collect::<Vec<_>>();
    assert_eq!(agent_rows.len(), 5, "本机 3 + 远端 2");
    assert_eq!(
        cache.rows().len(),
        5 + 2 + 3,
        "外加两个机器行与三个工作区头"
    );
    assert_eq!(
        state.hits.endpoint_agents.len(),
        agent_rows.len(),
        "夹具前提：全部行都放得下"
    );
    let names = ["one", "two", "three", "r-one", "r-two"];
    for ((rect, endpoint_id, pane_id), (row, name)) in state
        .hits
        .endpoint_agents
        .iter()
        .zip(agent_rows.iter().zip(names))
    {
        let agent = row.kind.agent().expect("agent 行");
        assert_eq!(
            (endpoint_id, pane_id),
            (&row.kind.endpoint_id, &agent.pane_id)
        );
        assert_eq!(
            (rect.x, rect.width),
            (body.x, body.width),
            "无滚动条时占满列表区"
        );
        assert_eq!(usize::from(rect.height), agent.rows.len());
        let text = rect_rows(&state, *rect).join("\n");
        assert!(text.contains(name), "命中区里画的就是这一行: {text}");
    }
    assert_eq!(state.hits.endpoint_agents[3].1, remote);

    // 点击命中区按（端点, pane）身份路由：远端行触发端点激活。
    let (rect, _, _) = state.hits.endpoint_agents[4].clone();
    let outcome = state.handle_raw_events(vec![click_at(rect.x + 6, rect.y)]);
    assert!(matches!(
        outcome.actions.as_slice(),
        [ClientShellAction::ActivateEndpoint {
            endpoint_id,
            target: Some(ClientEndpointFocusTarget::Pane(pane_id)),
        }] if endpoint_id == &remote && pane_id == "pane_2"
    ));
}

/// (e) 面板放不下全部行时，命中区只覆盖可见窗口：agent 行命中区是缓存 agent 行
/// 序列的一段连续切片，并为滚动条让出 1 列。
#[test]
fn characterization_workbench_endpoint_agent_hits_cover_only_the_visible_window() {
    let (mut state, _) = federated_state(AgentPanelSortConfig::Spaces);
    enable_workbench(&mut state);
    for start in [0, 3] {
        state.agent_scroll = start;
        state.compose(120, 26).expect("矮 workbench 帧");
        let body = state.hits.agent_body;
        let cached = cached_agent_ids(&state);
        let visible = endpoint_hit_ids(&state);
        assert!(
            !visible.is_empty() && visible.len() < cached.len(),
            "夹具前提：只放得下一部分行，可见 {} / {}，列表区 {body:?}",
            visible.len(),
            cached.len()
        );
        assert_eq!(state.agent_scroll, start, "起始行在可滚动范围内");
        let offset = cached
            .iter()
            .position(|id| id == &visible[0])
            .expect("可见的第一行来自缓存");
        assert_eq!(
            cached[offset..offset + visible.len()],
            visible[..],
            "可见行是缓存 agent 行的连续切片"
        );
        for (rect, _, _) in &state.hits.endpoint_agents {
            assert_eq!(rect.width, body.width - 1, "滚动条占最右 1 列");
            assert!(rect.y >= body.y && rect.bottom() <= body.bottom());
        }
    }
}

fn current_rows_key(
    state: &ClientShellState,
) -> crate::client::shell::endpoint_agents::AgentRowsKey {
    crate::client::shell::endpoint_agents::AgentRowsCache::key_for(
        &state.endpoints,
        &state.config,
        state.config_epoch,
        state.agent_rows_epoch,
        state.tree_collapse_epoch,
        state.agent_activity_epoch,
        &state.active_endpoint_id,
        state
            .snapshot
            .as_deref()
            .and_then(|snapshot| snapshot.agent_view_label.as_deref()),
    )
}

/// 缓存行 Vec 的地址：重建时新 Vec 在旧缓存被替换之前分配，地址必然不同；
/// 复用则地址不变。比「键相等」更直接地回答「这一帧有没有重建」。
fn cached_rows_address(state: &ClientShellState) -> usize {
    let rows = state.federated_agent_rows.as_ref().expect("行缓存").rows();
    assert!(!rows.is_empty(), "空 Vec 不分配，地址比较无意义");
    rows.as_ptr() as usize
}

fn cached_pane_ids(state: &ClientShellState) -> Vec<String> {
    cached_agent_ids(state)
        .into_iter()
        .map(|(_, pane_id)| pane_id)
        .collect()
}

/// (f) `AgentRowsCache`：键不变跨帧复用；折叠集合的代际（`tree_collapse_epoch`）
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
    state.refresh_federated_agent_rows();
    let revised_address = cached_rows_address(&state);
    assert_ne!(revised_address, address, "revision 变化触发重建");
    let cache = state.federated_agent_rows.as_ref().expect("行缓存");
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
    let first = state.hits.endpoint_agents[0].0;
    assert!(
        rect_rows(&state, first)[0].starts_with("├── ○ renamed"),
        "重建后的行上屏"
    );

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
/// ——至多 1 个最新节点——重，按上界看）。
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

/// (g) mobile 切换器的 agent 段：恒为平铺（无工作区头、无树前缀、折叠态无效），
/// 行序与 `aggregate_agent_rows` 同源，随排序设置变化；有活动的 agent 在详情行
/// 末尾带活动徽标（与桌面树同一口径：`badge_text` 的「运行中 / 总数」形态）。
#[test]
fn characterization_mobile_switcher_lists_agents_flat_in_aggregate_order() {
    let badge = running_badge(1, 3);
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
        };
        let mut state = classic_state_with(sort, projected);
        state.toggle_collapsed_group(&ClientEndpointId::Local, agent_group_key("ws_1"));
        state.compose(44, 40).expect("mobile 头部");
        assert!(state.mobile_layout_active(), "夹具前提：窄屏走 mobile 布局");
        let switch = state.hits.mobile_switch;
        state.handle_raw_events(vec![click(switch)]);
        assert_eq!(state.mode, ClientShellMode::Navigate);
        state.compose(44, 40).expect("mobile 切换器");

        let agents = mobile_agent_targets(&state);
        assert_eq!(
            agents
                .iter()
                .map(|(_, pane_id)| pane_id.as_str())
                .collect::<Vec<_>>(),
            expected,
            "{sort:?}：折叠态不隐藏任何 agent"
        );
        let aggregate = aggregate_navigation::aggregate_agent_rows(
            &state.endpoints,
            &state.active_endpoint_id,
            sort,
        )
        .into_iter()
        .map(|row| row.agent.pane_id.clone())
        .collect::<Vec<_>>();
        assert_eq!(aggregate, expected, "与聚合行序同源");
        for (rect, pane_id) in &agents {
            let text = rect_rows(&state, *rect).join("\n");
            assert_no_tree_glyphs(&text);
            let name = match pane_id.as_str() {
                "pane_1" => "one",
                "pane_2" => "two",
                _ => "three",
            };
            assert!(text.contains(name), "{pane_id}: {text}");
            assert_eq!(
                compact(&text).ends_with(&compact(&badge)),
                pane_id == "pane_2",
                "只有有活动的 agent 带徽标: {pane_id}: {text}"
            );
        }
        assert!(state.hits.agent_tree_toggles.is_empty());
    }
}

/// 活动徽标三种情况（审查发现 3）：有运行中的节点写「运行中 / 总数」（运行中
/// 等于总数时同样如此），没有运行中的只写总数（总数为 1 用单数），没有活动不画。
/// 中英文案逐字钉住，文档里写的就是这些。
#[test]
fn activity_badge_distinguishes_running_total_and_none() {
    let agent = |running: u32, total: u32| {
        let mut agent = panel_agent("pane_1", "ws_1", "tab_1", "one", AgentStatus::Idle, 1);
        agent.activity.running = running;
        agent.activity.total = total;
        // 宽度不设限，只看文案本身。
        super::super::agent_tree::mobile_activity_badge(&agent, u16::MAX)
    };
    for (lang, expected) in [
        (
            crate::i18n::Lang::En,
            ["2/5 running", "3/3 running", "5 activities", "1 activity"],
        ),
        (
            crate::i18n::Lang::ZhCn,
            ["运行中 2/5", "运行中 3/3", "5 个活动", "1 个活动"],
        ),
    ] {
        let _lang = crate::i18n::lang_guard(lang);
        assert_eq!(agent(2, 5).as_deref(), Some(expected[0]), "{lang:?}");
        assert_eq!(agent(3, 3).as_deref(), Some(expected[1]), "{lang:?}");
        assert_eq!(agent(0, 5).as_deref(), Some(expected[2]), "{lang:?}");
        assert_eq!(agent(0, 1).as_deref(), Some(expected[3]), "{lang:?}");
        assert_eq!(agent(0, 0), None, "没有活动不画徽标");
        // 总数比运行中小（上报不一致）时按运行中补齐总数。
        assert_eq!(agent(2, 0), agent(2, 2), "{lang:?}");
    }
}

/// mobile 详情行的活动徽标只拿其余字段排完后剩下的宽度（审查发现：原先把完整
/// 文案接在行尾再整体截断，长标签页名会把数字截成「运行中 2…」或「2/…」，看着
/// 像只有 2 个活动）。32 / 44 列、中英各扫一遍标签页名长度（0 表示不显示标签
/// 页）：徽标要么是完整文案、要么是完整的 `2/5`、要么整段不出现，画出来时整行
/// 不截断；每种宽度下三档都实际出现过。
#[test]
fn mobile_activity_badge_degrades_instead_of_truncating_digits() {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    enum Tier {
        Full,
        Compact,
        Hidden,
    }
    const TAB_LABEL: &str = "feature-branch-with-a-very-long-tab-name";
    for lang in [crate::i18n::Lang::En, crate::i18n::Lang::ZhCn] {
        let _lang = crate::i18n::lang_guard(lang);
        let full = compact(&running_badge(2, 5));
        let lead = full.chars().next().expect("徽标非空");
        for cols in [32u16, 44] {
            let mut tiers = HashSet::new();
            for len in 0..=TAB_LABEL.len() {
                let mut projected = two_workspace_snapshot();
                // pane_3 独占 ws_2 / tab_2：只有自定义标签页名才会进详情行。
                projected.tabs[1].label = TAB_LABEL[..len].into();
                projected.tabs[1].custom_label = len > 0;
                projected.agents[2].activity = ClientShellAgentActivity {
                    running: 2,
                    total: 5,
                    truncated: false,
                    nodes: Vec::new(),
                };
                let mut state = classic_state_with(AgentPanelSortConfig::Spaces, projected);
                state.compose(cols, 40).expect("mobile 头部");
                assert!(
                    state.mobile_layout_active(),
                    "夹具前提：{cols} 列走 mobile 布局"
                );
                let switch = state.hits.mobile_switch;
                state.handle_raw_events(vec![click(switch)]);
                state.compose(cols, 40).expect("mobile 切换器");
                let (rect, _) = mobile_agent_targets(&state)
                    .into_iter()
                    .find(|(_, pane_id)| pane_id == "pane_3")
                    .expect("pane_3 行");
                let rows = rect_rows(&state, rect);
                let detail = compact(&rows[1]);
                let context = format!("{lang:?} {cols} 列，标签页名 {len} 字符：{:?}", rows[1]);
                let tier = if detail.contains(&full) {
                    Tier::Full
                } else if detail.contains("2/5") {
                    Tier::Compact
                } else {
                    Tier::Hidden
                };
                match tier {
                    Tier::Full | Tier::Compact => {
                        assert!(!detail.contains('…'), "徽标画出来时整行不截断：{context}");
                        let shown = if tier == Tier::Full {
                            full.as_str()
                        } else {
                            "2/5"
                        };
                        assert!(detail.ends_with(shown), "徽标完整收尾：{context}");
                    }
                    Tier::Hidden => assert!(
                        !detail.contains('2') && !detail.contains(lead),
                        "放不下就整段不画，不留残段：{context}"
                    ),
                }
                tiers.insert(tier);
            }
            assert_eq!(
                tiers.len(),
                3,
                "{lang:?} {cols} 列：扫描应覆盖完整 / 只留数字 / 不画三档：{tiers:?}"
            );
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

/// 「运行中 / 总数」形态的活动徽标（有运行中的节点时）。
fn running_badge(running: u32, total: u32) -> String {
    crate::i18n::fill(
        crate::i18n::texts().agent_panel.activity_badge_running_fmt,
        &[
            ("running", &running.to_string()),
            ("total", &total.to_string()),
        ],
    )
}

/// 「总数」形态的活动徽标（没有运行中的节点，总数 > 1 时）。
fn total_badge(total: u32) -> String {
    crate::i18n::fill(
        crate::i18n::texts().agent_panel.activity_badge_total_fmt,
        &[("n", &total.to_string())],
    )
}

/// 右对齐画在 `rect` 首行的徽标 `badge` 里，`digits` 首字符所在的列（徽标的
/// 文字部分可能在数字前，例如中文「运行中 2/5」）。
fn badge_digits_x(rect: Rect, badge: &str, digits: &str) -> u16 {
    let offset = badge.find(digits).expect("徽标里有数字");
    rect.right() - crate::ui::display_width(badge) as u16
        + crate::ui::display_width(&badge[..offset]) as u16
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
    let main = rect_rows(
        &state,
        toggle_rect(&state, &ClientEndpointId::Local, "agent-tab:tab_1"),
    );
    assert!(
        main[0].starts_with("├─▾ ○ main"),
        "标签页头带状态与名字: {main:?}"
    );
    assert!(main[0].trim_end().ends_with("· 1"), "{main:?}");
    let review = rect_rows(
        &state,
        toggle_rect(&state, &ClientEndpointId::Local, "agent-tab:tab_1b"),
    );
    assert!(review[0].starts_with("└─▾ × review"), "{review:?}");
    let one = rect_rows(&state, classic_agent_rect(&state, "pane_1"));
    assert!(one[0].starts_with("│ └── ○"), "深两层的 agent 行: {one:?}");
    assert!(
        !one.iter().any(|line| line.contains("main")),
        "标签页头承载标签页名，子行不重复: {one:?}"
    );
    let two = rect_rows(&state, classic_agent_rect(&state, "pane_2"));
    assert!(two[0].starts_with("  └── ×"), "末标签页下的末子: {two:?}");
    assert!(!two.iter().any(|line| line.contains("review")), "{two:?}");

    // 折叠一个标签页只藏它自己的 agent。
    state.toggle_collapsed_group(&ClientEndpointId::Local, "agent-tab:tab_1".into());
    state.compose(106, 30).expect("折叠标签页帧");
    assert_eq!(classic_hit_ids(&state), ["pane_2", "pane_3"]);
}

/// 快照默认下发的活动摘要：running / total 计数 + 至多 1 个最新节点，
/// `truncated` 表示还有更多（整树走 `agent.activity.read`）。
fn summary_snapshot() -> ClientShellSnapshot {
    let mut projected = activity_snapshot(0);
    projected.agents[0].activity = ClientShellAgentActivity {
        running: 2,
        total: 5,
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

/// agent 行的活动摘要（W3 按摘要渲染）：默认折叠，行尾徽标写「运行中 / 总数」；
/// 点开关展开出「最新节点」一行，被截断时再跟「还有 N 项」；点最新节点行打开
/// 「Agent 活动」窗口并选中它，点「还有 N 项」打开窗口不预选。
#[test]
fn tree_agent_rows_show_the_activity_summary_collapsed_by_default() {
    let mut state = classic_state_with(AgentPanelSortConfig::Spaces, summary_snapshot());
    state.sidebar_width = 40;
    state.sidebar_width_manual = true;
    state.compose(106, 30).expect("活动摘要帧");

    assert_eq!(
        local_toggle_keys(&state),
        ["agent-panel:ws_0", "agent-activity:pane:pane_0"],
        "有活动的 agent 行带开关"
    );
    assert!(
        state.hits.agent_activity_rows.is_empty(),
        "活动摘要默认折叠"
    );
    let badge = running_badge(2, 5);
    let agent = rect_rows(&state, classic_agent_rect(&state, "pane_0"));
    assert!(agent[0].starts_with("└─▸ ○ agent-0 "), "{agent:?}");
    assert!(
        compact(&agent[0]).ends_with(&compact(&badge)),
        "行尾徽标 = 运行中 / 总数: {agent:?}"
    );
    let rect = classic_agent_rect(&state, "pane_0");
    let buffer = state.compose_buffer.as_ref().expect("缓冲");
    // 徽标右对齐：取数字首格（宽字符的占位格不带样式）。
    let badge_x = badge_digits_x(rect, &badge, "2/5");
    assert_eq!(buffer[(badge_x, rect.y)].symbol(), "2");
    assert_eq!(
        buffer[(badge_x, rect.y)].style().fg,
        Some(state.config.palette.yellow),
        "有运行中的活动：徽标用工作色"
    );

    // 点开关：展开（键在集合里 = 展开）。
    let toggle = toggle_rect(
        &state,
        &ClientEndpointId::Local,
        "agent-activity:pane:pane_0",
    );
    assert_eq!(toggle.width, 2, "agent 行的开关只占开关与间隔两列");
    state.handle_raw_events(vec![click(toggle)]);
    assert!(state
        .collapsed_groups
        .contains("agent-activity:pane:pane_0"));
    state.compose(106, 30).expect("展开摘要帧");
    let activity_ids = state
        .hits
        .agent_activity_rows
        .iter()
        .map(|hit| (hit.owner_key.as_str(), hit.node_id.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(
        activity_ids,
        [("pane:pane_0", "sub-7"), ("pane:pane_0", "")],
        "最新节点 + 「还有 N 项」（node_id 为空）"
    );
    let agent = rect_rows(&state, classic_agent_rect(&state, "pane_0"));
    assert!(agent[0].starts_with("└─▾ ○ agent-0 "), "{agent:?}");
    let latest = rect_rows(&state, state.hits.agent_activity_rows[0].rect);
    let texts = &crate::i18n::texts().agent_activity;
    assert!(
        latest[0].starts_with("  ├── ◐ explore repo "),
        "最新节点：状态图标 + 标签: {latest:?}"
    );
    assert!(
        compact(&latest[0]).contains(&compact(texts.kind_subagent)),
        "种类是次要信息: {latest:?}"
    );
    let more = rect_rows(&state, state.hits.agent_activity_rows[1].rect);
    let more_text = crate::i18n::fill(
        crate::i18n::texts().agent_panel.activity_more_fmt,
        &[("n", "4")],
    );
    assert!(more[0].starts_with("  └── "), "{more:?}");
    assert!(compact(&more[0]).contains(&compact(&more_text)), "{more:?}");

    // 点最新节点行：打开活动窗口并选中它。
    let row = state.hits.agent_activity_rows[0].rect;
    state.handle_raw_events(vec![click_at(row.x + 10, row.y)]);
    match state.overlay.as_ref() {
        Some(ClientShellOverlay::AgentActivity(overlay)) => {
            assert_eq!(
                overlay.owner,
                super::super::agent_activity_overlay::AgentActivityOwner::Pane {
                    pane_id: "pane_0".into()
                }
            );
            assert_eq!(overlay.selected_node.as_deref(), Some("sub-7"));
        }
        other => panic!("应打开 Agent 活动窗口: {other:?}"),
    }
    state.overlay = None;
    // 点「还有 N 项」：打开窗口，不预选节点。
    let row = state.hits.agent_activity_rows[1].rect;
    state.handle_raw_events(vec![click_at(row.x + 8, row.y)]);
    assert!(matches!(
        state.overlay.as_ref(),
        Some(ClientShellOverlay::AgentActivity(overlay)) if overlay.selected_node.is_none()
    ));
    state.overlay = None;

    // 再点开关收起。
    state.handle_raw_events(vec![click(toggle)]);
    state.compose(106, 30).expect("收起摘要帧");
    assert!(state.hits.agent_activity_rows.is_empty());

    // 没有运行中的活动：只写总数，徽标退为次要色。
    let mut projected = summary_snapshot();
    projected.agents[0].activity.running = 0;
    let mut state = classic_state_with(AgentPanelSortConfig::Spaces, projected);
    state.sidebar_width = 40;
    state.sidebar_width_manual = true;
    state.compose(106, 30).expect("无运行中活动帧");
    let rect = classic_agent_rect(&state, "pane_0");
    let agent = rect_rows(&state, rect);
    let badge = total_badge(5);
    assert!(compact(&agent[0]).ends_with(&compact(&badge)), "{agent:?}");
    let buffer = state.compose_buffer.as_ref().expect("缓冲");
    let badge_x = badge_digits_x(rect, &badge, "5");
    assert_eq!(buffer[(badge_x, rect.y)].symbol(), "5");
    assert_eq!(
        buffer[(badge_x, rect.y)].style().fg,
        Some(state.config.palette.overlay0)
    );

    // Launch 平铺下 agent 行仍可展开活动摘要。
    let mut state = classic_state_with(AgentPanelSortConfig::Launch, summary_snapshot());
    state.compose(106, 30).expect("launch 活动帧");
    assert_eq!(
        local_toggle_keys(&state),
        ["agent-activity:pane:pane_0"],
        "平铺没有分组头，只有 agent 行自己的开关"
    );
    let agent = rect_rows(&state, classic_agent_rect(&state, "pane_0"));
    assert!(
        agent[0].starts_with("▸ ○ space-0"),
        "平铺行带工作区 token: {agent:?}"
    );
    state.handle_raw_events(vec![click(toggle_rect(
        &state,
        &ClientEndpointId::Local,
        "agent-activity:pane:pane_0",
    ))]);
    state.compose(106, 30).expect("launch 展开帧");
    assert_eq!(state.hits.agent_activity_rows.len(), 2);
    let latest = rect_rows(&state, state.hits.agent_activity_rows[0].rect);
    assert!(latest[0].starts_with("├── ◐ explore repo"), "{latest:?}");
}

/// 徽标在窄侧栏里的档位（审查发现 2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BadgeTier {
    /// 完整文案。
    Full,
    /// 只留数字 `2/5`。
    Digits,
    /// 不画。
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

/// 断言 `rect` 首行右端按 `tier` 画徽标：`Full` 是完整文案 `full`，`Digits` 只剩
/// 数字 `2/5`（前一格是间隔），`Hidden` 整行不出现 `2/5`。数字逐格比对。
fn assert_badge_tier(
    state: &ClientShellState,
    rect: Rect,
    full: &str,
    tier: BadgeTier,
    case: &str,
) {
    let buffer = state.compose_buffer.as_ref().expect("缓冲");
    let cell = |x: u16| buffer[(x, rect.y)].symbol().to_owned();
    let line = rect_rows(state, rect)[0].clone();
    let digits_at = |x: u16| [cell(x), cell(x + 1), cell(x + 2)];
    match tier {
        BadgeTier::Full => {
            assert!(
                compact(&line).ends_with(&compact(full)),
                "{case}: 完整徽标: {line:?}"
            );
            assert_eq!(
                digits_at(badge_digits_x(rect, full, "2/5")),
                ["2", "/", "5"],
                "{case}"
            );
        }
        BadgeTier::Digits => {
            let x = rect.right() - 3;
            assert_eq!(digits_at(x), ["2", "/", "5"], "{case}: 只留数字: {line:?}");
            assert_eq!(cell(x - 1), " ", "{case}: 徽标前留 1 列间隔: {line:?}");
            assert!(
                !compact(&line).contains(&compact(full)),
                "{case}: 完整文案放不下: {line:?}"
            );
        }
        BadgeTier::Hidden => {
            assert!(!line.contains("2/5"), "{case}: 徽标让位: {line:?}");
        }
    }
}

/// 窄侧栏的宽度预算（审查发现 2）：状态图标与名称先保 8 列（图标 2 列 + 名称
/// ≥ 6 列），徽标再按剩余宽度三档退化——完整文案 → 只留数字 → 不画。侧栏
/// 18 / 26 / 36 列 × agent 深度 1–3（工作区 / +标签页 / +机器）逐格断言图标字符、
/// 名称前缀与徽标数字；外部条目行同一预算。
///
/// 行宽 = 侧栏宽 − 1（右缘分隔线），内容区再扣树前缀（每层 2 列 + 开关 2 列），
/// 徽标可用 = 内容区 − 8 − 1 列间隔。例：侧栏 18、深度 2 → 17 − 6 − 9 = 2 列，
/// 连 `2/5` 都放不下，整个让位。中英文完整徽标差 1 列（`2/5 running` 11 列、
/// 「运行中 2/5」10 列），侧栏 26、深度 2 恰好落在两档之间，两种语言各钉一遍。
#[test]
fn tree_badge_yields_to_the_status_icon_and_name_on_narrow_sidebars() {
    use BadgeTier::{Digits, Full, Hidden};
    for (lang, full_width, tiers) in [
        (
            crate::i18n::Lang::En,
            11,
            [
                (18, 1, Digits),
                (18, 2, Hidden),
                (18, 3, Hidden),
                (26, 1, Full),
                (26, 2, Digits),
                (26, 3, Digits),
                (36, 1, Full),
                (36, 2, Full),
                (36, 3, Full),
            ],
        ),
        (
            crate::i18n::Lang::ZhCn,
            10,
            [
                (18, 1, Digits),
                (18, 2, Hidden),
                (18, 3, Hidden),
                (26, 1, Full),
                (26, 2, Full),
                (26, 3, Digits),
                (36, 1, Full),
                (36, 2, Full),
                (36, 3, Full),
            ],
        ),
    ] {
        let _lang = crate::i18n::lang_guard(lang);
        let full = running_badge(2, 5);
        assert_eq!(
            crate::ui::display_width(&full),
            full_width,
            "{lang:?}: 夹具前提：完整徽标 {full:?} 的宽度"
        );
        for (width, depth, tier) in tiers {
            assert_narrow_agent_row(lang, &full, width, depth, tier);
        }

        // 外部条目行：同一预算（深度 1：外部分组头之下）。
        for (width, tier) in [(18, Digits), (36, Full)] {
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
            state.compose(106, 40).expect("窄侧栏外部条目帧");
            let case = format!("{lang:?}、外部条目、侧栏 {width}");
            let rect = state
                .hits
                .external_agents
                .first()
                .unwrap_or_else(|| panic!("{case}: 外部条目不在命中表里"))
                .0;
            assert_eq!(
                rect.width,
                width - 1,
                "{case}: 夹具前提：行宽 = 侧栏宽 − 右缘分隔线"
            );
            let buffer = state.compose_buffer.as_ref().expect("缓冲");
            let cell = |x: u16| buffer[(x, rect.y)].symbol().to_owned();
            let content_x = rect.x + 4;
            assert_eq!(cell(content_x), "◐", "{case}: 状态图标");
            let label = (content_x + 2..content_x + 8).map(cell).collect::<String>();
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
            .endpoint_agents
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
    // 前缀每层 2 列 + 开关与间隔 2 列。
    let content_x = rect.x + 2 * depth + 2;
    assert_eq!(cell(content_x - 2), "▸", "{case}: 活动摘要的折叠开关");
    assert_eq!(cell(content_x), "○", "{case}: 状态图标");
    // 名称保 6 列：整名放不下时第 6 列是省略号（例如中文侧栏 26、深度 2 恰好
    // 只剩这 6 列给名称）。
    let name = (content_x + 2..content_x + 8).map(cell).collect::<String>();
    assert!(
        name == "agent-" || name == "agent…",
        "{case}: 名称至少保 6 列: {name:?}"
    );
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
            .agent_activity_rows
            .iter()
            .map(|hit| hit.node_id.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(&state), ["node_0", "node_1"]);
    let node_0 = rect_rows(&state, state.hits.agent_activity_rows[0].rect);
    assert!(node_0[0].starts_with("  ├─▸ ◐ task 0"), "{node_0:?}");
    let node_1 = rect_rows(&state, state.hits.agent_activity_rows[1].rect);
    assert!(node_1[0].starts_with("  └─▸ ✓ task 1"), "{node_1:?}");

    let toggle = toggle_rect(
        &state,
        &ClientEndpointId::Local,
        "agent-node:pane:pane_0:node_0",
    );
    state.handle_raw_events(vec![click(toggle)]);
    assert!(state
        .collapsed_groups
        .contains("agent-node:pane:pane_0:node_0"));
    state.compose(106, 30).expect("展开 node_0 帧");
    assert_eq!(ids(&state), ["node_0", "node_2", "node_1"]);
    let node_2 = rect_rows(&state, state.hits.agent_activity_rows[1].rect);
    assert!(
        node_2[0].starts_with("  │ └── ◐ task 2"),
        "深一层的子节点: {node_2:?}"
    );
}

/// 多行行配置（这里 `[状态图标 agent] / [状态文案]`）：续行画祖先引导线并缩进
/// 两列对齐名称；agent 行展开活动时，续行在开关列接一条引导线到下面的活动行。
/// 行配置自带状态文案时不再另补。
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
    let one = rect_rows(&state, classic_agent_rect(&state, "pane_1"));
    assert_eq!(one.len(), 2, "{one:?}");
    assert!(one[0].starts_with("├── ○ one"), "{one:?}");
    assert_eq!(
        compact(&one[0]),
        compact("├── ○ one"),
        "行配置带状态文案：首行不另补: {one:?}"
    );
    assert!(
        one[1].starts_with("│     "),
        "非末子续行接祖先引导线: {one:?}"
    );
    assert_eq!(compact(&one[1]), compact(&format!("│ {}", status.idle)));
    let two = rect_rows(&state, classic_agent_rect(&state, "pane_2"));
    assert!(two[1].starts_with("      "), "末子续行留白: {two:?}");

    // 展开活动的 agent 行：续行在开关列接引导线。
    let mut projected = summary_snapshot();
    projected.agents[0].agent_status = AgentStatus::Working;
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&config));
    state.set_snapshot(Box::new(projected));
    state.set_pane_surface(surface());
    state.toggle_collapsed_group(
        &ClientEndpointId::Local,
        "agent-activity:pane:pane_0".into(),
    );
    state.compose(106, 30).expect("展开活动的多行帧");
    let agent = rect_rows(&state, classic_agent_rect(&state, "pane_0"));
    assert!(agent[0].starts_with("└─▾ ◐ agent-0"), "{agent:?}");
    assert!(
        agent[1].starts_with("  │   "),
        "开关列接到活动行: {agent:?}"
    );
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
    let mut state = classic_state_with(AgentPanelSortConfig::Spaces, projected);
    state.toggle_collapsed_group(
        &ClientEndpointId::Local,
        "agent-activity:pane:pane_0".into(),
    );
    state.toggle_collapsed_group(&ClientEndpointId::Local, "agent-node:pane:pane_0:a".into());
    state.compose(106, 30).expect("环状活动帧");
    let labels = state
        .hits
        .agent_activity_rows
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
    assert_eq!(
        keys.last().copied(),
        Some("agent-activity:ext:zcode:abc"),
        "有活动的外部条目可展开: {keys:?}"
    );
    assert!(keys.contains(&"agent-external:zcode"), "{keys:?}");
    let texts = &crate::i18n::texts().agent_panel;
    let group = rect_rows(
        &state,
        toggle_rect(&state, &ClientEndpointId::Local, "agent-external:zcode"),
    );
    assert!(group[0].starts_with('▾'), "{group:?}");
    assert!(
        compact(&group[0]).contains(&compact(texts.external_group)) && group[0].contains("zcode"),
        "分组头写「外部 · source」: {group:?}"
    );
    assert!(group[0].trim_end().ends_with("· 2"), "{group:?}");
    let externals = state
        .hits
        .external_agents
        .iter()
        .map(|(_, _, id)| id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(externals, ["zcode:abc", "zcode:def"]);
    let abc = rect_rows(&state, state.hits.external_agents[0].0);
    assert!(
        abc[0].starts_with("├─▸ ◐ fix login zcode"),
        "活动摘要默认折叠: {abc:?}"
    );
    let badge = running_badge(1, 2);
    assert!(compact(&abc[0]).ends_with(&compact(&badge)), "{abc:?}");
    assert!(state.hits.agent_activity_rows.is_empty());
    let def = rect_rows(&state, state.hits.external_agents[1].0);
    assert!(
        def[0].starts_with("└── ○ zcode:def"),
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
    state.compose(106, 34).expect("展开外部条目活动帧");
    let node = rect_rows(&state, state.hits.agent_activity_rows[0].rect);
    assert!(
        node[0].starts_with("│ └── · sub"),
        "外部条目下的活动节点: {node:?}"
    );
    assert_eq!(state.hits.agent_activity_rows[0].owner_key, "ext:zcode:abc");

    // 点条目行：打开活动窗口。
    let row = state.hits.external_agents[1].0;
    state.handle_raw_events(vec![click_at(row.x + 8, row.y)]);
    assert!(matches!(
        state.overlay.as_ref(),
        Some(ClientShellOverlay::AgentActivity(overlay))
            if overlay.owner == super::super::agent_activity_overlay::AgentActivityOwner::External {
                external_id: "zcode:def".into()
            }
    ));
    state.overlay = None;

    // 右键：外部条目菜单。
    let row = state.hits.external_agents[0].0;
    state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Right),
        column: row.x + 8,
        row: row.y,
        modifiers: KeyModifiers::empty(),
    })]);
    assert!(matches!(
        state.overlay.as_ref(),
        Some(ClientShellOverlay::ContextMenu(menu))
            if matches!(&menu.target, ClientContextMenuTarget::ExternalAgent { external_id, .. } if external_id == "zcode:abc")
    ));
    state.overlay = None;

    // 折叠分组：条目与活动行都藏起来。
    let group = toggle_rect(&state, &ClientEndpointId::Local, "agent-external:zcode");
    state.handle_raw_events(vec![click_at(group.x + 3, group.y)]);
    state.compose(106, 34).expect("折叠外部分组帧");
    assert!(state.hits.external_agents.is_empty());
    assert!(state.hits.agent_activity_rows.is_empty());
    assert!(local_toggle_keys(&state).contains(&"agent-external:zcode"));

    // 平铺（Launch）下外部分组仍单列在 agent 之后。
    let mut state = classic_state_with(AgentPanelSortConfig::Launch, projected.clone());
    state.compose(106, 34).expect("launch 外部分组帧");
    assert_eq!(state.hits.external_agents.len(), 2);
    let last_agent = state.hits.agents.iter().map(|(rect, _)| rect.y).max();
    assert!(
        state
            .hits
            .external_agents
            .iter()
            .all(|(rect, _, _)| Some(rect.y) > last_agent),
        "外部条目排在全部 agent 之后"
    );

    // 状态过滤视图只过滤 pane 里的 agent，外部条目不列出。
    projected.agent_view_label = Some("blocked".into());
    let mut state = classic_state_with(AgentPanelSortConfig::Spaces, projected);
    state.compose(106, 34).expect("过滤视图帧");
    assert!(state.hits.external_agents.is_empty());
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
    let machine = toggle_rect(&state, &remote, MACHINE_TOGGLE_KEY);
    let line = &rect_rows(&state, machine)[0];
    assert!(line.starts_with("▾ … Build"), "重连中的状态字形: {line:?}");
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
    let (rect, endpoint_id, _) = &state.hits.endpoint_agents[3];
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
    let rows = state.federated_agent_rows.as_ref().expect("行缓存").rows();
    let shape = rows
        .iter()
        .map(|row| {
            let kind = match &row.kind.kind {
                AgentTreeKind::Machine { .. } => "machine",
                AgentTreeKind::Workspace { .. } => "workspace",
                AgentTreeKind::Tab { .. } => "tab",
                AgentTreeKind::Agent { .. } => "agent",
                AgentTreeKind::Activity { .. } => "activity",
                AgentTreeKind::ActivityMore { .. } => "more",
                AgentTreeKind::ExternalGroup { .. } => "external-group",
                AgentTreeKind::ExternalAgent { .. } => "external",
            };
            (kind, row.depth, row.has_children, row.collapsed)
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
            ("machine", 0, true, false),
            ("workspace", 1, true, false),
            ("agent", 2, false, false),
            ("agent", 2, false, false),
        ]
    );
}

/// agent 行右键动作（W3 接上）：「重命名」沿用 pane 重命名浮层；「关闭窗格」发
/// `pane.close`；「绑定账号」打开监控 → 账号页、选中该 agent 的厂商并把待绑定
/// pane 设为它；「用量」打开并钉住该 agent 的用量卡。其它端点的
/// agent：重命名先切端点并聚焦（不打开浮层），关闭直接发往该端点，绑定切过去
/// 并打开账号页但不预设 pane。
#[test]
fn tree_agent_context_actions_rename_bind_and_close_the_agent_pane() {
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
                "浮层标题与 pane 右键菜单的重命名同一条 i18n 文案"
            );
        }
        other => panic!("应打开 pane 重命名浮层: {other:?}"),
    }
    assert!(outcome.actions.is_empty());
    state.overlay = None;

    // 重命名（其它端点）：先切过去并聚焦，不打开浮层。
    let mut outcome = ClientShellInput::default();
    state.activate_agent_context_action(
        remote.clone(),
        pane("pane_1"),
        ClientContextMenuAction::RenameAgent,
        &mut outcome,
    );
    assert!(state.overlay.is_none());
    assert!(matches!(
        outcome.actions.as_slice(),
        [ClientShellAction::ActivateEndpoint {
            endpoint_id,
            target: Some(ClientEndpointFocusTarget::Pane(pane_id)),
        }] if endpoint_id == &remote && pane_id == "pane_1"
    ));

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

    // 用量：打开并钉住该 agent 的用量卡（不发端点请求、不开浮层）；同一 agent
    // 再来一次即关闭。
    let mut outcome = ClientShellInput::default();
    state.activate_agent_context_action(
        ClientEndpointId::Local,
        pane("pane_1"),
        ClientContextMenuAction::ShowAgentUsage,
        &mut outcome,
    );
    assert!(state.overlay.is_none() && outcome.actions.is_empty());
    let hover = state.observability.hover.as_ref().expect("用量卡已打开");
    assert!(hover.visible && hover.pinned);
    assert!(matches!(
        &hover.target,
        super::super::observability::HoverTarget::Agent { endpoint_id, pane, agent }
            if endpoint_id.is_local() && pane == "pane_1" && agent == "pi"
    ));
    state.activate_agent_context_action(
        ClientEndpointId::Local,
        pane("pane_1"),
        ClientContextMenuAction::ShowAgentUsage,
        &mut outcome,
    );
    assert!(
        state.observability.hover.is_none(),
        "同一 agent 再来一次即关闭"
    );

    // 绑定账号（当前端点）：账号页 + 厂商 + 待绑定 pane。
    let mut outcome = ClientShellInput::default();
    state.activate_agent_context_action(
        ClientEndpointId::Local,
        pane("pane_2"),
        ClientContextMenuAction::BindAgentAccount,
        &mut outcome,
    );
    assert_eq!(
        state.observability.page,
        Some(super::super::observability::Page::Accounts)
    );
    assert_eq!(state.observability.selected_provider.as_deref(), Some("pi"));
    assert_eq!(state.observability.selected_pane.as_deref(), Some("pane_2"));
    assert_eq!(
        state.observability.selected_pane_label.as_deref(),
        Some("two · pane_2")
    );

    // 绑定账号（其它端点）：切过去并打开账号页，不预设 pane。
    let mut outcome = ClientShellInput::default();
    state.activate_agent_context_action(
        remote.clone(),
        pane("pane_1"),
        ClientContextMenuAction::BindAgentAccount,
        &mut outcome,
    );
    assert!(outcome.actions.iter().any(|action| matches!(
        action,
        ClientShellAction::ActivateEndpoint { endpoint_id, .. } if endpoint_id == &remote
    )));
    assert_eq!(
        state.observability.page,
        Some(super::super::observability::Page::Accounts)
    );
    assert_eq!(state.observability.selected_provider.as_deref(), Some("pi"));
    assert_eq!(state.observability.selected_pane, None);
}
