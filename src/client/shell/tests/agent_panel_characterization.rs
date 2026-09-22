//! Agents 面板三条并行渲染路径的 characterization（现状）测试。
//!
//! W3 要把 classic / workbench（联邦）/ mobile 三条路径统一到同一套树构建与行
//! 渲染；动手之前先把**重构前**的可见行为钉住，重构时每一处有意的行为变化都会
//! 在这里显式翻红，而不是悄悄漂移。
//!
//! - classic：`agent_sidebar::render_agent_panel`，单端点且未启用 workbench；
//! - workbench / 联邦：`endpoint_agents::render_expanded`，行来自视图计算阶段的
//!   `AgentRowsCache`；
//! - mobile 与排序来源：`aggregate_navigation::aggregate_agent_rows`。
//!
//! 标注「现状，W3 将改变」的断言描述的是已知缺陷而非期望行为。

use super::*;
use crate::client::endpoint::{
    ClientEndpointId, ClientEndpointStatus, ProfileId, SavedSshEndpoint,
};
use crate::client::shell::agent_sidebar::agent_group_key;
use crate::config::AgentPanelSortConfig;

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
    let mut state = ClientShellState::new(panel_config(sort));
    state.set_snapshot(Box::new(two_workspace_snapshot()));
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

fn group_keys(state: &ClientShellState) -> Vec<&str> {
    state
        .hits
        .agent_group_toggles
        .iter()
        .map(|(_, key)| key.as_str())
        .collect()
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

fn group_rect(state: &ClientShellState, workspace_id: &str) -> Rect {
    let key = agent_group_key(workspace_id);
    state
        .hits
        .agent_group_toggles
        .iter()
        .find(|(_, id)| id == &key)
        .unwrap_or_else(|| panic!("工作区头 {key} 不在命中表里"))
        .0
}

fn click(rect: Rect) -> RawInputEvent {
    RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: rect.x,
        row: rect.y,
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

/// (a) classic + Spaces：「工作区头 + agent 行」两层树。工作区头汇总最高优先级
/// 状态与 agent 数，子行带 `├─` / `└─` 前缀，续行用 `│` 或空白对齐，且子行不再
/// 重复工作区名（头已承载）。
#[test]
fn characterization_classic_spaces_renders_two_level_workspace_tree() {
    let mut state = classic_state(AgentPanelSortConfig::Spaces);
    state.compose(106, 30).expect("classic agents 面板");

    assert_eq!(
        group_keys(&state),
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
        header.starts_with(" × client-shell"),
        "工作区头: {header:?}"
    );
    assert!(header.ends_with("· 2 ▾"), "计数与展开箭头: {header:?}");
    let second_header = group_rect(&state, "ws_2");
    let header = &rect_rows(&state, second_header)[0];
    assert!(header.starts_with(" ◐ herdr"), "工作区头: {header:?}");
    assert!(header.ends_with("· 1 ▾"), "计数与展开箭头: {header:?}");

    let one = rect_rows(&state, classic_agent_rect(&state, "pane_1"));
    assert!(one[0].starts_with("   ├─ ○"), "非末子行前缀: {one:?}");
    assert!(one[1].starts_with("   │    one"), "非末子行续行: {one:?}");
    let two = rect_rows(&state, classic_agent_rect(&state, "pane_2"));
    assert!(two[0].starts_with("   └─ ×"), "末子行前缀: {two:?}");
    assert!(two[1].starts_with("        two"), "末子行续行: {two:?}");
    let three = rect_rows(&state, classic_agent_rect(&state, "pane_3"));
    assert!(three[0].starts_with("   └─ ◐"), "独子也是末子行: {three:?}");
    assert!(
        three[1].starts_with("        three"),
        "末子行续行: {three:?}"
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
}

/// (a) classic 折叠：折叠键进 `collapsed_groups` 后该工作区的子行消失、箭头变
/// `▸`、计数保留；其它工作区不受影响并上移补位。
#[test]
fn characterization_classic_collapsed_workspace_hides_children_and_flips_chevron() {
    let mut state = classic_state(AgentPanelSortConfig::Spaces);
    state.compose(106, 30).expect("展开帧");
    let expanded_second_header_y = group_rect(&state, "ws_2").y;

    state.toggle_collapsed_group(&ClientEndpointId::Local, agent_group_key("ws_1"));
    assert!(
        state.collapsed_groups.contains("agent-panel:ws_1"),
        "本机折叠态存于 collapsed_groups"
    );
    state.compose(106, 30).expect("折叠帧");

    assert_eq!(
        group_keys(&state),
        [agent_group_key("ws_1"), agent_group_key("ws_2")],
        "折叠后工作区头仍在"
    );
    assert_eq!(classic_hit_ids(&state), ["pane_3"], "折叠组的子行不再可点");
    let first_header = group_rect(&state, "ws_1");
    let header = &rect_rows(&state, first_header)[0];
    assert!(
        header.starts_with(" × client-shell"),
        "工作区头: {header:?}"
    );
    assert!(header.ends_with("· 2 ▸"), "折叠箭头，计数保留: {header:?}");
    let second_header = group_rect(&state, "ws_2");
    assert!(rect_rows(&state, second_header)[0].ends_with("· 1 ▾"));
    assert_eq!(second_header.y, first_header.y + 1, "后续工作区上移补位");
    assert!(second_header.y < expanded_second_header_y);

    let body = body_text(&state);
    assert!(!body.contains("one") && !body.contains("two"), "{body}");
    assert!(body.contains("three"), "{body}");
    assert!(!body.contains('├'), "只剩一个独子行: {body}");
}

/// (b) classic + Launch：**仍是两层树**，不是平铺。启动顺序只在每个工作区内部
/// 重排子行；工作区之间保持快照顺序，哪怕最早启动的 agent 在靠后的工作区。
/// `launch_seq` 全为 0（旧 server 不下发）时退回快照顺序。
#[test]
fn characterization_classic_launch_keeps_workspace_tree_and_sorts_within_workspace() {
    let mut state = classic_state(AgentPanelSortConfig::Launch);
    state.compose(106, 30).expect("classic launch 帧");

    assert_eq!(
        sort_label(&state),
        compact(crate::i18n::texts().agent_panel.sort_launch)
    );
    assert_eq!(
        group_keys(&state),
        [agent_group_key("ws_1"), agent_group_key("ws_2")],
        "工作区头仍在，且不按启动顺序重排工作区"
    );
    assert_eq!(
        classic_hit_ids(&state),
        ["pane_2", "pane_1", "pane_3"],
        "工作区内按启动顺序排；全局第二个启动的 pane_3 仍排在最后"
    );
    let two = rect_rows(&state, classic_agent_rect(&state, "pane_2"));
    assert!(two[0].starts_with("   ├─ ×"), "{two:?}");
    let one = rect_rows(&state, classic_agent_rect(&state, "pane_1"));
    assert!(one[0].starts_with("   └─ ○"), "{one:?}");
    let three = rect_rows(&state, classic_agent_rect(&state, "pane_3"));
    assert!(three[0].starts_with("   └─ ◐"), "{three:?}");

    // 旧 server 不下发 launch_seq：全为 0，稳定排序保持快照顺序。
    let mut projected = two_workspace_snapshot();
    for agent in &mut projected.agents {
        agent.launch_seq = 0;
    }
    let mut state = ClientShellState::new(panel_config(AgentPanelSortConfig::Launch));
    state.set_snapshot(Box::new(projected));
    state.set_pane_surface(surface());
    state
        .compose(106, 30)
        .expect("classic launch 帧（launch_seq 全 0）");
    assert_eq!(classic_hit_ids(&state), ["pane_1", "pane_2", "pane_3"]);
}

/// (c) classic 矮面板：列表区不足 3 行（放不下「工作区头 + 一个 agent 行」）时
/// 退化为平铺——无工作区头、无树前缀、工作区名回到行内，行序走全局排序。
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
        state.hits.agent_group_toggles.is_empty(),
        "退化后无工作区头"
    );
    assert_eq!(classic_hit_ids(&state).first(), Some(&"pane_1"));
    let one = rect_rows(&state, classic_agent_rect(&state, "pane_1"));
    assert!(
        one[0].starts_with(" ○ client-shell"),
        "平铺行缩进 1 列且带工作区名: {one:?}"
    );
    assert_no_tree_glyphs(&body_text(&state));

    // 同一状态换回足够高的面板，树立即回来：退化只由高度决定。
    state.compose(106, 30).expect("高面板帧");
    assert_eq!(state.hits.agent_group_toggles.len(), 2);

    // Launch 下退化视图按全局启动顺序平铺（树视图里则是 two / one / three）。
    let mut state = classic_state(AgentPanelSortConfig::Launch);
    let mut flat_order = Vec::new();
    for start in 0..3 {
        state.agent_scroll = start;
        state.compose(106, 10).expect("矮面板 launch 帧");
        assert!(state.hits.agent_group_toggles.is_empty());
        flat_order.push(classic_hit_ids(&state)[0].to_owned());
    }
    assert_eq!(flat_order, ["pane_2", "pane_3", "pane_1"]);
}

/// (c) 退化阈值：逐个终端高度扫一遍，「无工作区头」当且仅当列表区不足 3 行。
#[test]
fn characterization_classic_tree_degrades_exactly_below_three_body_rows() {
    let mut state = classic_state(AgentPanelSortConfig::Spaces);
    let mut seen_flat = false;
    let mut seen_tree = false;
    for rows in 6..=30 {
        state.agent_scroll = 0;
        state.compose(106, rows).expect("classic 帧");
        let body_height = state.hits.agent_body.height;
        let flat = state.hits.agent_group_toggles.is_empty();
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

/// (d) workbench 布局：即便排序是 Spaces、表头写着「按工作区分组」，列表也**恒为
/// 平铺**——没有工作区头、没有树前缀，折叠态对它不起作用；排序只改变行序。
///
/// 现状，W3 将改变：这是已知缺陷（用户所在的布局看不到任何分组结构），W3 统一
/// 三条路径后这里应变成与 classic 相同的树。
#[test]
fn characterization_workbench_spaces_sort_is_flat_despite_grouped_header() {
    let mut state = workbench_state(AgentPanelSortConfig::Spaces);
    state.compose(120, 40).expect("workbench 帧");

    assert_eq!(
        sort_label(&state),
        compact(crate::i18n::texts().sidebar.sort_grouped),
        "表头复用 classic 的，仍显示分组标签"
    );
    assert!(state.hits.agent_group_toggles.is_empty(), "无工作区头");
    assert!(
        state.hits.agents.is_empty(),
        "workbench 不写 classic 命中区"
    );
    assert_eq!(
        endpoint_hit_ids(&state),
        [local("pane_1"), local("pane_2"), local("pane_3")]
    );
    assert_no_tree_glyphs(&body_text(&state));
    let expected = [
        (" ○ Local · client-shell", "   one"),
        (" × Local · client-shell", "   two"),
        (" ◐ Local · herdr", "   three"),
    ];
    for ((rect, _, _), (first, second)) in state.hits.endpoint_agents.iter().zip(expected) {
        let lines = rect_rows(&state, *rect);
        assert!(lines[0].starts_with(first), "平铺行缩进 1 列: {lines:?}");
        assert!(lines[1].starts_with(second), "续行缩进 3 列: {lines:?}");
    }

    // 折叠某个工作区：classic 会藏掉子行，这里毫无变化。
    state.toggle_collapsed_group(&ClientEndpointId::Local, agent_group_key("ws_1"));
    state.compose(120, 40).expect("折叠后的 workbench 帧");
    assert_eq!(
        endpoint_hit_ids(&state),
        [local("pane_1"), local("pane_2"), local("pane_3")],
        "折叠态不影响 workbench 的行"
    );
    assert!(state.hits.agent_group_toggles.is_empty());

    // 点表头排序标签切到 Launch：只换行序，仍然平铺。
    let toggle = state.hits.agent_sort_toggle;
    assert!(!toggle.is_empty(), "排序标签可点");
    state.handle_raw_events(vec![click(toggle)]);
    assert_eq!(state.config.agent_panel_sort, AgentPanelSortConfig::Launch);
    state.compose(120, 40).expect("launch workbench 帧");
    assert_eq!(
        sort_label(&state),
        compact(crate::i18n::texts().agent_panel.sort_launch)
    );
    assert_eq!(
        endpoint_hit_ids(&state),
        [local("pane_2"), local("pane_3"), local("pane_1")]
    );
    assert_no_tree_glyphs(&body_text(&state));
}

/// (d) 联邦（多端点、未启用 workbench）走同一条 `endpoint_agents::render_expanded`：
/// Spaces 下也是平铺，按端点顺序再按各自快照顺序；本机与远端的折叠态都不起作用。
///
/// 现状，W3 将改变：同上。
#[test]
fn characterization_federated_sidebar_spaces_sort_is_flat_across_endpoints() {
    let (mut state, remote) = federated_state(AgentPanelSortConfig::Spaces);
    assert!(!state.workbench.enabled);
    state.toggle_collapsed_group(&ClientEndpointId::Local, agent_group_key("ws_1"));
    state.toggle_collapsed_group(&remote, agent_group_key("ws_1"));
    state.compose(106, 40).expect("联邦帧");

    assert_eq!(
        sort_label(&state),
        compact(crate::i18n::texts().sidebar.sort_grouped)
    );
    assert!(state.hits.agent_group_toggles.is_empty());
    assert!(state.hits.agents.is_empty());
    assert_eq!(
        endpoint_hit_ids(&state),
        [
            local("pane_1"),
            local("pane_2"),
            local("pane_3"),
            (remote.clone(), "pane_1".to_owned()),
            (remote.clone(), "pane_2".to_owned()),
        ]
    );
    assert_no_tree_glyphs(&body_text(&state));
    let last = state.hits.endpoint_agents[4].0;
    let lines = rect_rows(&state, last);
    assert!(
        lines[0].starts_with(" ○ Build · remote-space"),
        "机器名 token 区分端点: {lines:?}"
    );
    assert!(lines[1].starts_with("   r-two"), "{lines:?}");
}

/// (e) workbench 的 `hits.endpoint_agents` 与缓存行一一对应：同序、同（端点,
/// pane）身份，矩形自列表区顶部起逐行紧排，高度等于该行的 token 行数。
#[test]
fn characterization_workbench_endpoint_agent_hits_mirror_cached_rows() {
    let (mut state, remote) = federated_state(AgentPanelSortConfig::Spaces);
    enable_workbench(&mut state);
    state.compose(120, 40).expect("workbench 联邦帧");

    let body = state.hits.agent_body;
    let cache = state.federated_agent_rows.as_ref().expect("行缓存");
    let rows = cache.rows();
    assert_eq!(rows.len(), 5, "本机 3 + 远端 2");
    assert_eq!(
        state.hits.endpoint_agents.len(),
        rows.len(),
        "夹具前提：全部行都放得下"
    );
    let names = ["one", "two", "three", "r-one", "r-two"];
    let mut next_y = body.y;
    for ((rect, endpoint_id, pane_id), (row, name)) in state
        .hits
        .endpoint_agents
        .iter()
        .zip(rows.iter().zip(names))
    {
        assert_eq!(
            (endpoint_id, pane_id),
            (&row.endpoint_id, &row.agent.pane_id)
        );
        assert_eq!(rect.y, next_y, "逐行紧排（row_gap = 0）");
        assert_eq!(
            (rect.x, rect.width),
            (body.x, body.width),
            "无滚动条时占满列表区"
        );
        assert_eq!(usize::from(rect.height), row.agent.rows.len());
        let text = rect_rows(&state, *rect).join("\n");
        assert!(text.contains(name), "命中区里画的就是这一行: {text}");
        next_y += rect.height;
    }
    assert_eq!(state.hits.endpoint_agents[3].1, remote);

    // 点击命中区按（端点, pane）身份路由：远端行触发端点激活。
    let (rect, _, _) = state.hits.endpoint_agents[4].clone();
    let outcome = state.handle_raw_events(vec![click(rect)]);
    assert!(matches!(
        outcome.actions.as_slice(),
        [ClientShellAction::ActivateEndpoint {
            endpoint_id,
            target: Some(ClientEndpointFocusTarget::Pane(pane_id)),
        }] if endpoint_id == &remote && pane_id == "pane_2"
    ));
}

/// (e) 面板放不下全部行时，命中区只覆盖可见窗口：从 `agent_scroll` 指向的行起、
/// 与缓存行的一段连续切片逐一对应，并为滚动条让出 1 列。
#[test]
fn characterization_workbench_endpoint_agent_hits_cover_only_the_visible_window() {
    let (mut state, _) = federated_state(AgentPanelSortConfig::Spaces);
    enable_workbench(&mut state);
    for start in [0, 2] {
        state.agent_scroll = start;
        state.compose(120, 24).expect("矮 workbench 帧");
        let body = state.hits.agent_body;
        let rows = state.federated_agent_rows.as_ref().expect("行缓存").rows();
        let visible = state.hits.endpoint_agents.len();
        assert!(
            visible >= 1 && visible < rows.len(),
            "夹具前提：只放得下一部分行，可见 {visible} / {}，列表区 {body:?}",
            rows.len()
        );
        assert_eq!(state.agent_scroll, start, "起始行在可滚动范围内");
        let window = &rows[start..start + visible];
        for ((rect, endpoint_id, pane_id), row) in state.hits.endpoint_agents.iter().zip(window) {
            assert_eq!(
                (endpoint_id, pane_id),
                (&row.endpoint_id, &row.agent.pane_id)
            );
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

fn cached_pane_ids(state: &ClientShellState) -> Vec<&str> {
    state
        .federated_agent_rows
        .as_ref()
        .expect("行缓存")
        .rows()
        .iter()
        .map(|row| row.agent.pane_id.as_str())
        .collect()
}

/// (f) `AgentRowsCache`：键不变跨帧复用；折叠集合的代际（`tree_collapse_epoch`）
/// 在键里，所以**折叠态一变就重建**（统一树的行序列取决于折叠态；接缝 S2 起
/// 生效，行内容在面板车道把树接进缓存前仍与折叠无关）；快照 revision 与排序
/// 在键里，变了就重建。
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

    // 仅折叠态变化：折叠代际进键，重建；平铺的联邦行内容暂不受折叠影响。
    state.toggle_collapsed_group(&ClientEndpointId::Local, agent_group_key("ws_1"));
    assert!(state.group_is_collapsed(&ClientEndpointId::Local, &agent_group_key("ws_1")));
    state.compose(120, 40).expect("折叠后的帧");
    let collapsed_key = current_rows_key(&state);
    assert_ne!(collapsed_key, key, "折叠代际进缓存键");
    let collapsed_address = cached_rows_address(&state);
    assert_ne!(collapsed_address, address, "折叠即重建");
    assert_eq!(cached_pane_ids(&state), ["pane_1", "pane_2", "pane_3"]);
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
    state.compose(120, 40).expect("新快照帧");
    let renamed_address = cached_rows_address(&state);
    assert_ne!(renamed_address, revised_address);
    let first = state.hits.endpoint_agents[0].0;
    assert!(
        rect_rows(&state, first)[1].starts_with("   renamed"),
        "重建后的行上屏"
    );

    // 排序在键里：点排序标签 → 重建为 Launch 行序。
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

/// (g) `aggregate_agent_rows` 是联邦面板与 mobile 的行序来源。Spaces：端点顺序 →
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
/// 行序与 `aggregate_agent_rows` 同源，随排序设置变化。
#[test]
fn characterization_mobile_switcher_lists_agents_flat_in_aggregate_order() {
    for (sort, expected) in [
        (AgentPanelSortConfig::Spaces, ["pane_1", "pane_2", "pane_3"]),
        (AgentPanelSortConfig::Launch, ["pane_2", "pane_3", "pane_1"]),
    ] {
        let mut state = classic_state(sort);
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
        }
        assert!(state.hits.agent_group_toggles.is_empty());
    }
}
