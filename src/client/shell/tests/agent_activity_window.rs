//! 「Agent 活动」二级窗口：版式与渲染（三档尺寸，断言字符与样式）、数据通道
//! （打开 / 选中 / 跟随 tick 发起的读取、代际丢弃、续读、pi 的无游标重读、旧
//! server 不支持）、键盘与鼠标两条交互路径，以及视图计算阶段写入的滚动上界。

use super::super::agent_activity_overlay::{
    format_elapsed, AgentActivityButton, AgentActivityFocus, AgentActivityOwner, AgentPageView,
    ClientAgentActivityOverlay, CONTENT_CAP_BYTES, FOLLOW_INTERVAL,
};
use super::super::state::PendingEndpointKind;
use super::*;
use crate::api::schema::{
    AgentActivityContent, AgentActivityContentFormat, AgentActivityKind, AgentActivityNode,
    AgentActivityReadParams, AgentActivityStatus, Method, ResponseResult,
};
use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
use std::time::{Duration, Instant};

fn agent(pane_id: &str, running: u32, total: u32) -> ClientShellAgent {
    ClientShellAgent {
        insight: Default::default(),
        pane_id: pane_id.into(),
        workspace_id: "ws_1".into(),
        tab_id: "tab_1".into(),
        name: Some("claude-main".into()),
        display_agent: None,
        agent: Some("claude".into()),
        title: None,
        terminal_title: None,
        terminal_title_stripped: None,
        agent_status: AgentStatus::Working,
        state_change_seq: 1,
        state_labels: Vec::new(),
        tokens: Vec::new(),
        focused: true,
        launch_seq: 0,
        activity: crate::protocol::ClientShellAgentActivity {
            running,
            total,
            truncated: true,
            nodes: Vec::new(),
            ..Default::default()
        },
    }
}

fn state() -> ClientShellState {
    state_named("claude-main", "zcode desktop")
}

/// 属主显示名可定制的初始状态：pane 里 agent 的名字、外部条目的标签。
fn state_named(agent_name: &str, external_label: &str) -> ClientShellState {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(named_snapshot(agent_name, external_label)));
    state.set_pane_surface(surface());
    state
}

/// 初始状态的快照：pane_1 里的 agent（1/4）与一个没有活动的外部条目 zcode:s1。
fn named_snapshot(agent_name: &str, external_label: &str) -> ClientShellSnapshot {
    let mut projected = snapshot();
    let mut owner = agent("pane_1", 1, 4);
    owner.name = Some(agent_name.into());
    projected.agents = vec![owner];
    projected.external_agents = vec![crate::protocol::ClientShellExternalAgent {
        external_id: "zcode:s1".into(),
        source: "zcode".into(),
        agent_status: AgentStatus::Idle,
        label: external_label.into(),
        readable: true,
        agent: Some("zcode".into()),
        cwd: None,
        updated_at_ms: None,
        activity: crate::protocol::ClientShellAgentActivity::default(),
    }];
    projected
}

/// 外部条目 zcode:s1 在快照里的活动摘要换成 `running / total`，其余同初始状态。
fn external_summary_snapshot(running: u32, total: u32) -> ClientShellSnapshot {
    let mut projected = named_snapshot("claude-main", "zcode desktop");
    projected.external_agents[0].activity = crate::protocol::ClientShellAgentActivity {
        running,
        total,
        truncated: total > 1,
        nodes: Vec::new(),
        ..Default::default()
    };
    projected
}

fn pane_owner() -> AgentActivityOwner {
    AgentActivityOwner::Pane {
        pane_id: "pane_1".into(),
    }
}

fn node(
    id: &str,
    parent: Option<&str>,
    kind: AgentActivityKind,
    status: AgentActivityStatus,
    label: &str,
    span: Option<(u64, u64)>,
) -> AgentActivityNode {
    AgentActivityNode {
        id: id.into(),
        kind,
        label: label.into(),
        status,
        parent_id: parent.map(str::to_owned),
        agent_type: None,
        content_ref: Some(format!("ref-{id}")),
        summary: None,
        started_at_ms: span.map(|(start, _)| start),
        ended_at_ms: span.map(|(_, end)| end),
    }
}

/// a（子 agent，运行中）→ a.1（任务，完成 12 s）、a.2（待办）；b（后台，失败）。
fn sample_nodes() -> Vec<AgentActivityNode> {
    let mut root = node(
        "a",
        None,
        AgentActivityKind::Subagent,
        AgentActivityStatus::Running,
        "explore repo",
        None,
    );
    root.agent_type = Some("Explore".into());
    root.summary = Some("scanning src".into());
    vec![
        root,
        node(
            "a.1",
            Some("a"),
            AgentActivityKind::Task,
            AgentActivityStatus::Done,
            "read files",
            Some((2_000, 14_000)),
        ),
        node(
            "a.2",
            Some("a"),
            AgentActivityKind::Todo,
            AgentActivityStatus::Pending,
            "write summary",
            None,
        ),
        node(
            "b",
            None,
            AgentActivityKind::Background,
            AgentActivityStatus::Failed,
            "cargo test",
            Some((0, 65_000)),
        ),
    ]
}

fn overlay(state: &ClientShellState) -> &ClientAgentActivityOverlay {
    match state.overlay.as_ref() {
        Some(ClientShellOverlay::AgentActivity(overlay)) => overlay,
        other => panic!("应打开 Agent 活动窗口: {other:?}"),
    }
}

fn overlay_mut(state: &mut ClientShellState) -> &mut ClientAgentActivityOverlay {
    match state.overlay.as_mut() {
        Some(ClientShellOverlay::AgentActivity(overlay)) => overlay,
        other => panic!("应打开 Agent 活动窗口: {other:?}"),
    }
}

/// 输出里的 `agent.activity.read` 请求：(请求 id, 参数)。
fn reads(outcome: &ClientShellInput) -> Vec<(String, AgentActivityReadParams)> {
    outcome
        .actions
        .iter()
        .filter_map(|action| match action {
            ClientShellAction::EndpointRequest { request, .. } => match &request.method {
                Method::AgentActivityRead(params) => Some((request.id.clone(), params.clone())),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

fn single_read(outcome: &ClientShellInput) -> (String, AgentActivityReadParams) {
    let reads = reads(outcome);
    assert_eq!(reads.len(), 1, "恰好一个读取: {:?}", outcome.actions);
    reads.into_iter().next().expect("one read")
}

fn open(state: &mut ClientShellState, owner: AgentActivityOwner) -> ClientShellInput {
    let mut outcome = ClientShellInput::default();
    state.open_agent_activity(ClientEndpointId::Local, owner, &mut outcome);
    outcome
}

/// 走真实的应答路由（`handle_endpoint_result` → `PendingEndpointKind`）。
fn respond(
    state: &mut ClientShellState,
    request_id: &str,
    result: Result<ResponseResult, ClientShellEndpointError>,
) -> (bool, ClientShellInput) {
    let (repaint, actions) = state.handle_endpoint_result("boot-1", request_id, result);
    (
        repaint,
        ClientShellInput {
            actions,
            ..ClientShellInput::default()
        },
    )
}

fn tree_result(nodes: Vec<AgentActivityNode>) -> Result<ResponseResult, ClientShellEndpointError> {
    Ok(ResponseResult::AgentActivity {
        nodes,
        content: None,
    })
}

fn content_result(
    node_id: &str,
    text: &str,
    eof: bool,
    next_cursor: Option<&str>,
) -> Result<ResponseResult, ClientShellEndpointError> {
    content_result_with(
        node_id,
        text,
        eof,
        next_cursor,
        AgentActivityContentFormat::Text,
    )
}

fn content_result_with(
    node_id: &str,
    text: &str,
    eof: bool,
    next_cursor: Option<&str>,
    format: AgentActivityContentFormat,
) -> Result<ResponseResult, ClientShellEndpointError> {
    Ok(ResponseResult::AgentActivity {
        nodes: Vec::new(),
        content: Some(AgentActivityContent {
            node_id: node_id.into(),
            format,
            text: text.into(),
            eof,
            truncated: false,
            next_cursor: next_cursor.map(str::to_owned),
        }),
    })
}

/// 打开窗口并应答首个树读取，返回应答后排出的请求。
fn open_with_tree(state: &mut ClientShellState) -> ClientShellInput {
    let outcome = open(state, pane_owner());
    let (id, _) = single_read(&outcome);
    respond(state, &id, tree_result(sample_nodes())).1
}

/// 选中节点并应答它的第一页内容。
fn select_with_content(state: &mut ClientShellState, node_id: &str, text: &str) {
    let mut outcome = ClientShellInput::default();
    state.select_agent_activity_node(node_id.into(), &mut outcome);
    let (id, params) = single_read(&outcome);
    assert_eq!(params.node_id.as_deref(), Some(node_id));
    let (_, next) = respond(state, &id, content_result(node_id, text, true, Some("end")));
    assert!(reads(&next).is_empty(), "读完不续读");
}

/// 在途读取的请求 id（按代际找）。
fn pending_id(state: &ClientShellState, epoch: u64) -> String {
    state
        .pending_requests
        .iter()
        .find(|(_, pending)| {
            matches!(
                pending.kind,
                PendingEndpointKind::AgentActivityRead { epoch: candidate, .. } if candidate == epoch
            )
        })
        .map(|(id, _)| id.clone())
        .expect("在途读取已登记")
}

/// 把在途读取逐个应答完：树读取回 `sample_nodes`，内容读取回 `text`（读完）。
fn settle(state: &mut ClientShellState, text: &str) {
    for _ in 0..32 {
        let Some(read) = overlay(state).in_flight.clone() else {
            return;
        };
        let id = pending_id(state, read.epoch);
        let result = match read.node_id.as_deref() {
            None => tree_result(sample_nodes()),
            Some(node) => content_result(node, text, true, Some("end")),
        };
        respond(state, &id, result);
    }
    panic!("读取没有停下");
}

fn key(code: KeyCode) -> RawInputEvent {
    RawInputEvent::Key(crate::input::TerminalKey::new(code, KeyModifiers::empty()))
}

fn mouse(kind: MouseEventKind, column: u16, row: u16) -> RawInputEvent {
    RawInputEvent::Mouse(MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::empty(),
    })
}

/// 保留帧缓冲里 `area` 的逐行文本（宽字符的续格去掉，便于按字符查找）。
fn rows_text(state: &ClientShellState, area: Rect) -> Vec<String> {
    let buffer = state.compose_buffer.as_ref().expect("保留帧缓冲");
    (area.y..area.bottom())
        .map(|y| {
            (area.x..area.right())
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect()
}

/// 去掉空格后比较：宽字符的续格在逐格字形里是空格。
fn has(haystack: &str, needle: &str) -> bool {
    let compact = |text: &str| text.chars().filter(|ch| *ch != ' ').collect::<String>();
    compact(haystack).contains(&compact(needle))
}

fn popup_text(state: &ClientShellState) -> Vec<String> {
    rows_text(state, state.hits.agent_activity_popup)
}

/// `needle` 首字符在帧缓冲里的格（绝对坐标）；按逐格字形匹配。
fn find_cell(state: &ClientShellState, area: Rect, needle: &str) -> Option<(u16, u16)> {
    let buffer = state.compose_buffer.as_ref().expect("保留帧缓冲");
    let wanted = needle.chars().map(String::from).collect::<Vec<_>>();
    for y in area.y..area.bottom() {
        let cells = (area.x..area.right())
            .map(|x| buffer[(x, y)].symbol().to_owned())
            .filter(|symbol| !symbol.is_empty())
            .collect::<Vec<_>>();
        let xs = (area.x..area.right())
            .filter(|x| !buffer[(*x, y)].symbol().is_empty())
            .collect::<Vec<_>>();
        if let Some(position) = cells
            .windows(wanted.len())
            .position(|window| window == wanted.as_slice())
        {
            return Some((xs[position], y));
        }
    }
    None
}

fn cell(state: &ClientShellState, at: (u16, u16)) -> &ratatui::buffer::Cell {
    &state.compose_buffer.as_ref().expect("保留帧缓冲")[at]
}

fn texts() -> &'static crate::i18n::AgentActivityTexts {
    &crate::i18n::texts().agent_activity
}

#[test]
fn elapsed_is_compact() {
    assert_eq!(format_elapsed(12_000), "12s");
    assert_eq!(format_elapsed(65_000), "1m05s");
    assert_eq!(format_elapsed(3_900_000), "1h05m");
}

/// 活跃后代带上祖先排到前面；结束节点按时间倒序，相同时间保持来源顺序。
#[test]
fn activity_tree_prioritizes_running_ancestry_and_recently_ended_nodes() {
    let mut state = state();
    let outcome = open(&mut state, pane_owner());
    let (id, _) = single_read(&outcome);
    let nodes = vec![
        node(
            "old",
            None,
            AgentActivityKind::Task,
            AgentActivityStatus::Done,
            "old",
            Some((0, 10)),
        ),
        node(
            "workflow",
            None,
            AgentActivityKind::Task,
            AgentActivityStatus::Done,
            "workflow",
            Some((0, 5)),
        ),
        node(
            "unknown",
            None,
            AgentActivityKind::Task,
            AgentActivityStatus::Unknown,
            "unknown end",
            None,
        ),
        node(
            "recent",
            None,
            AgentActivityKind::Task,
            AgentActivityStatus::Failed,
            "recent",
            Some((0, 20)),
        ),
        node(
            "equal",
            None,
            AgentActivityKind::Task,
            AgentActivityStatus::Done,
            "equal",
            Some((0, 20)),
        ),
        node(
            "running",
            Some("workflow"),
            AgentActivityKind::Subagent,
            AgentActivityStatus::Running,
            "running",
            None,
        ),
    ];
    respond(&mut state, &id, tree_result(nodes));
    assert_eq!(
        overlay(&state).visible_node_ids().collect::<Vec<_>>(),
        ["workflow", "running", "recent", "equal", "old", "unknown"]
    );
    state.compose(120, 40).expect("窗口帧");
    let unknown = state
        .hits
        .agent_activity_tree_rows
        .iter()
        .find(|(_, id)| id == "unknown")
        .expect("unknown 行")
        .0;
    let at = find_cell(&state, unknown, "unknown end").expect("已结束未知标签");
    assert_eq!(
        cell(&state, at).fg,
        state.config.palette.text,
        "状态只由字形表达，标签保持正文色"
    );
}

/// 宽页面（经典布局占满整帧、最后一行是页面页脚）：页头（返回、标题、属主、按钮）、
/// 视图标签、两行的列表项（树前缀、状态字形、标签、角色 / 种类、耗时、摘要行）、
/// 右栏转写与跟随行；选中项选中底色 + 竖条，状态只经字形与状态色表达，标签保持
/// 正文色。
#[test]
fn wide_page_renders_header_list_transcript_and_footer() {
    for (cols, rows) in [(200, 50), (160, 45), (120, 40)] {
        let mut state = state();
        open_with_tree(&mut state);
        select_with_content(&mut state, "a", "line one\nline two\n");
        state.compose(cols, rows).expect("frame");
        let page = state.hits.agent_activity_popup;
        assert_eq!(
            page,
            Rect::new(0, 0, cols, rows),
            "{cols}x{rows}: 经典布局占满整帧"
        );
        let lines = popup_text(&state);
        let text = lines.join("\n");
        assert!(
            has(&lines[0], texts().page_title),
            "{cols}x{rows} 标题:\n{text}"
        );
        assert!(
            lines[0].contains("claude-main"),
            "{cols}x{rows} 属主:\n{text}"
        );
        for label in ["explore repo", "read files", "write summary", "cargo test"] {
            assert!(
                text.contains(label),
                "{cols}x{rows} 列表项 {label}:\n{text}"
            );
        }
        assert!(text.contains("├─"), "{cols}x{rows} 连接线:\n{text}");
        assert!(text.contains("└─"), "{cols}x{rows} 末子连接线:\n{text}");
        assert!(text.contains('▾'), "{cols}x{rows} 折叠开关:\n{text}");
        assert!(text.contains("12s"), "{cols}x{rows} 耗时:\n{text}");
        assert!(
            text.contains("1m05s"),
            "{cols}x{rows} 失败节点耗时:\n{text}"
        );
        assert!(text.contains("Explore"), "{cols}x{rows} 角色:\n{text}");
        assert!(has(&text, texts().kind_task), "{cols}x{rows} 种类:\n{text}");
        assert!(
            text.contains("scanning src"),
            "{cols}x{rows} 摘要行:\n{text}"
        );
        assert!(
            text.contains("line one") && text.contains("line two"),
            "{cols}x{rows} 正文:\n{text}"
        );
        for view in [
            texts().view_tree,
            texts().view_status,
            texts().view_timeline,
        ] {
            assert!(has(&text, view), "{cols}x{rows} 视图标签 {view}:\n{text}");
        }
        assert!(
            has(&text, texts().follow_tail),
            "{cols}x{rows} 跟随行:\n{text}"
        );
        assert!(
            has(&lines[usize::from(rows) - 1], texts().hint_select),
            "{cols}x{rows} 页面页脚"
        );
        let buttons = state
            .hits
            .agent_activity_actions
            .iter()
            .map(|(_, button)| *button)
            .collect::<Vec<_>>();
        for button in [
            AgentActivityButton::Close,
            AgentActivityButton::Jump,
            AgentActivityButton::Refresh,
            AgentActivityButton::ToggleFollow,
            AgentActivityButton::Back,
            AgentActivityButton::View(AgentPageView::Tree),
            AgentActivityButton::View(AgentPageView::Timeline),
            AgentActivityButton::Filter,
            AgentActivityButton::ToggleDone,
        ] {
            assert!(
                buttons.contains(&button),
                "{cols}x{rows} {button:?}: {buttons:?}"
            );
        }
        let order = state
            .hits
            .agent_activity_tree_rows
            .iter()
            .map(|(_, id)| id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(order, ["a", "a.1", "a.2", "b"], "前序展开");
        assert!(
            state
                .hits
                .agent_activity_tree_rows
                .iter()
                .all(|(rect, _)| rect.height == 2),
            "{cols}x{rows} 每项两行"
        );

        let palette = &state.config.palette;
        let list = state.hits.agent_activity_tree_rows[0].0;
        let explore = find_cell(&state, list, "explore repo").expect("选中项");
        assert_eq!(
            cell(&state, explore).bg,
            palette.selection_row_bg(),
            "选中底色"
        );
        assert_eq!(cell(&state, (list.x, list.y)).symbol(), "▌", "选中竖条");
        let b_row = state.hits.agent_activity_tree_rows[3].0;
        let cargo = find_cell(&state, b_row, "cargo test").expect("失败项");
        assert_eq!(cell(&state, cargo).fg, palette.text, "标签正文色");
        let failed = find_cell(&state, b_row, "✕").expect("失败字形");
        assert_eq!(
            cell(&state, failed).fg,
            crate::ui::design::status_color(
                crate::harness::AgentBucket::Failed,
                palette,
                &state.config.components
            ),
            "失败字形用失败色"
        );
        let body = find_cell(&state, page, "line one").expect("正文");
        assert!(body.0 > list.right(), "正文在右栏");
        assert!(!state.hits.agent_activity_content.is_empty());
    }
}

/// 窄页面：内宽 < 100 单栏，Tab 在列表与转写之间切换；< 60 每项只占一行。
#[test]
fn narrow_page_shows_one_column_and_tab_switches() {
    for (cols, rows, compact) in [(90, 30, false), (56, 20, true)] {
        let mut state = state();
        open_with_tree(&mut state);
        select_with_content(&mut state, "a.1", "only in content\n");
        state.compose(cols, rows).expect("frame");
        let text = popup_text(&state).join("\n");
        assert!(
            text.contains("read files"),
            "{cols}x{rows} 列表可见:\n{text}"
        );
        assert!(
            !text.contains("only in content"),
            "{cols}x{rows} 转写隐藏:\n{text}"
        );
        assert!(state.hits.agent_activity_content.is_empty());
        assert_eq!(
            state
                .hits
                .agent_activity_tree_rows
                .iter()
                .all(|(rect, _)| rect.height == 1),
            compact,
            "{cols}x{rows} 每项行数"
        );

        state.handle_raw_events(vec![key(KeyCode::Tab)]);
        assert_eq!(overlay(&state).focus, AgentActivityFocus::Content);
        state.compose(cols, rows).expect("frame");
        let text = popup_text(&state).join("\n");
        assert!(
            text.contains("only in content"),
            "{cols}x{rows} 转写可见:\n{text}"
        );
        assert!(
            !text.contains("write summary"),
            "{cols}x{rows} 列表隐藏:\n{text}"
        );
        assert!(state.hits.agent_activity_tree_rows.is_empty());

        state.handle_raw_events(vec![key(KeyCode::BackTab)]);
        assert_eq!(overlay(&state).focus, AgentActivityFocus::Tree);
    }
}

/// 打开立即读树（pane 属主、follow = true）；树应答后选中节点读内容（从头读），
/// 应答走 `handle_endpoint_result` → `PendingEndpointKind::AgentActivityRead`。
#[test]
fn opening_reads_the_tree_and_selection_reads_the_node() {
    let mut state = state();
    let outcome = open(&mut state, pane_owner());
    let (id, params) = single_read(&outcome);
    assert_eq!(params.pane_id.as_deref(), Some("pane_1"));
    assert_eq!(params.external_id, None);
    assert_eq!(params.node_id, None);
    assert!(params.follow);
    let pending = state.pending_requests.get(&id).expect("pending");
    let epoch = match &pending.kind {
        PendingEndpointKind::AgentActivityRead { epoch, node_id } => {
            assert_eq!(node_id, &None);
            *epoch
        }
        other => panic!("应登记为活动读取: {other:?}"),
    };
    assert_eq!(overlay(&state).read_epoch, epoch);

    let (repaint, next) = respond(&mut state, &id, tree_result(sample_nodes()));
    assert!(repaint);
    assert!(reads(&next).is_empty(), "没有选中时不读内容");
    assert!(overlay(&state).tree_loaded);

    state.handle_raw_events(vec![key(KeyCode::Down)]);
    assert_eq!(overlay(&state).selected_node.as_deref(), Some("a"));
    let pending = state
        .pending_requests
        .iter()
        .find(|(_, pending)| {
            matches!(
                &pending.kind,
                PendingEndpointKind::AgentActivityRead { node_id: Some(node), .. } if node == "a"
            )
        })
        .map(|(id, _)| id.clone())
        .expect("选中即读内容");
    let in_flight = overlay(&state).in_flight.clone().expect("在途");
    assert_eq!(in_flight.node_id.as_deref(), Some("a"));
    assert_eq!(in_flight.cursor, None, "新选中从头读");
    assert!(in_flight.replace);
    assert!(in_flight.epoch > epoch, "代际递增");
    let (repaint, _) = respond(
        &mut state,
        &pending,
        content_result("a", "hello\n", true, Some("5")),
    );
    assert!(repaint);
    let content = overlay(&state).content.as_ref().expect("内容");
    assert_eq!(content.text(), "hello\n");
    assert!(content.eof);
}

/// 单飞与代际：选中改变时旧内容读取仍在途，新读取等它回来（应答被丢弃）再发；
/// 对不上在途读取的应答（旧窗口、伪造 epoch）一律丢弃。
#[test]
fn stale_responses_are_dropped_and_the_next_read_follows() {
    let mut state = state();
    open_with_tree(&mut state);
    let mut outcome = ClientShellInput::default();
    state.select_agent_activity_node("a".into(), &mut outcome);
    let (first, _) = single_read(&outcome);

    let mut outcome = ClientShellInput::default();
    state.select_agent_activity_node("b".into(), &mut outcome);
    assert!(reads(&outcome).is_empty(), "单飞：旧读取在途时不发新读取");

    let epoch = overlay(&state).read_epoch;
    let (repaint, actions) = state.receive_agent_activity_read(
        epoch + 7,
        Some("a".into()),
        content_result("a", "x", true, None),
    );
    assert!(!repaint && actions.is_empty(), "对不上在途读取的应答被丢弃");

    let (repaint, next) = respond(&mut state, &first, content_result("a", "old\n", true, None));
    assert!(!repaint, "旧节点的内容被丢弃");
    assert!(overlay(&state).content.is_none());
    let (_, params) = single_read(&next);
    assert_eq!(
        params.node_id.as_deref(),
        Some("b"),
        "旧应答回来后立即发新读取"
    );
    assert_eq!(params.cursor, None);
}

/// 没读完（`eof = false`）按 `next_cursor` 立即续读；读完后跟随 tick 每秒续读
/// 一次（先树后内容）。
#[test]
fn unfinished_content_is_read_on_with_the_cursor() {
    let mut state = state();
    open_with_tree(&mut state);
    let mut outcome = ClientShellInput::default();
    state.select_agent_activity_node("a".into(), &mut outcome);
    let (id, _) = single_read(&outcome);
    let (_, next) = respond(
        &mut state,
        &id,
        content_result("a", "page one\n", false, Some("c1")),
    );
    let (id, params) = single_read(&next);
    assert_eq!(params.cursor.as_deref(), Some("c1"), "续读带游标");
    assert_eq!(params.node_id.as_deref(), Some("a"));
    let (_, next) = respond(
        &mut state,
        &id,
        content_result("a", "page two\n", true, Some("c2")),
    );
    assert!(reads(&next).is_empty(), "读完停下");
    assert_eq!(
        overlay(&state).content.as_ref().expect("内容").text(),
        "page one\npage two\n",
        "按页追加"
    );

    // 跟随：未到间隔不发；到了先读树（follow = true），树回来再续读内容。
    let mut outcome = ClientShellInput::default();
    state.tick_agent_activity(Instant::now(), &mut outcome);
    assert!(reads(&outcome).is_empty(), "未到跟随间隔");
    let mut outcome = ClientShellInput::default();
    state.tick_agent_activity(Instant::now() + FOLLOW_INTERVAL * 2, &mut outcome);
    let (id, params) = single_read(&outcome);
    assert_eq!(params.node_id, None, "跟随先刷新树");
    assert!(params.follow);
    let (_, next) = respond(&mut state, &id, tree_result(sample_nodes()));
    let (id, params) = single_read(&next);
    assert_eq!(params.node_id.as_deref(), Some("a"));
    assert_eq!(params.cursor.as_deref(), Some("c2"), "跟随续读用上次的游标");
    let (_, next) = respond(
        &mut state,
        &id,
        content_result("a", "tail\n", true, Some("c3")),
    );
    assert!(reads(&next).is_empty());
    assert_eq!(
        overlay(&state).content.as_ref().expect("内容").text(),
        "page one\npage two\ntail\n"
    );
}

/// pi：`eof` 且 `next_cursor` 为 None，下一次跟随从头重读并整体替换，不重复拼接。
#[test]
fn pi_content_without_a_cursor_is_reread_and_replaced() {
    let mut state = state();
    open_with_tree(&mut state);
    let mut outcome = ClientShellInput::default();
    state.select_agent_activity_node("a".into(), &mut outcome);
    let (id, _) = single_read(&outcome);
    respond(
        &mut state,
        &id,
        content_result("a", "partial output", true, None),
    );
    assert_eq!(
        overlay(&state).content.as_ref().expect("内容").text(),
        "partial output"
    );

    let mut outcome = ClientShellInput::default();
    state.tick_agent_activity(Instant::now() + FOLLOW_INTERVAL * 2, &mut outcome);
    let (id, _) = single_read(&outcome);
    let (_, next) = respond(&mut state, &id, tree_result(sample_nodes()));
    let (id, params) = single_read(&next);
    assert_eq!(params.cursor, None, "没有游标：从头重读");
    let in_flight = overlay(&state).in_flight.clone().expect("在途");
    assert!(in_flight.replace, "整体替换");
    respond(
        &mut state,
        &id,
        content_result("a", "partial output grew\n", true, None),
    );
    assert_eq!(
        overlay(&state).content.as_ref().expect("内容").text(),
        "partial output grew\n",
        "替换而不是追加"
    );
}

/// 暂停跟随后 tick 不再读取；关窗后 tick 不读取，迟到的应答也不再生效。
#[test]
fn follow_pauses_and_closing_stops_all_reads() {
    let mut state = state();
    open_with_tree(&mut state);
    state.handle_raw_events(vec![key(KeyCode::Char('f'))]);
    assert!(!overlay(&state).follow);
    let mut outcome = ClientShellInput::default();
    state.tick_agent_activity(Instant::now() + FOLLOW_INTERVAL * 5, &mut outcome);
    assert!(reads(&outcome).is_empty(), "暂停跟随不轮询");

    state.handle_raw_events(vec![key(KeyCode::Char('f'))]);
    assert!(overlay(&state).follow);
    let mut outcome = ClientShellInput::default();
    state.tick_agent_activity(Instant::now(), &mut outcome);
    let (late, _) = single_read(&outcome);
    assert!(reads(&outcome)[0].1.follow, "恢复跟随立即刷新一轮");

    state.handle_raw_events(vec![key(KeyCode::Esc)]);
    assert!(state.overlay.is_none());
    let mut outcome = ClientShellInput::default();
    state.tick_agent_activity(Instant::now() + FOLLOW_INTERVAL * 5, &mut outcome);
    assert!(outcome.actions.is_empty(), "关窗后不再读取");
    let (repaint, next) = respond(&mut state, &late, tree_result(sample_nodes()));
    assert!(!repaint && next.actions.is_empty(), "迟到的应答被丢弃");
}

/// 外部来源：带 external_id、不跟随；标题旁「外部来源，只读」徽标；f 无效、
/// tick 不轮询，跟随按钮灰显不可点。
#[test]
fn external_owner_is_read_only_and_never_follows() {
    let mut state = state();
    let outcome = open(
        &mut state,
        AgentActivityOwner::External {
            external_id: "zcode:s1".into(),
        },
    );
    let (id, params) = single_read(&outcome);
    assert_eq!(params.external_id.as_deref(), Some("zcode:s1"));
    assert_eq!(params.pane_id, None);
    assert!(!params.follow);
    respond(&mut state, &id, tree_result(sample_nodes()));
    state.handle_raw_events(vec![key(KeyCode::Char('f'))]);
    assert!(!overlay(&state).follow_active());
    let mut outcome = ClientShellInput::default();
    state.tick_agent_activity(Instant::now() + FOLLOW_INTERVAL * 5, &mut outcome);
    assert!(reads(&outcome).is_empty(), "外部来源不轮询");

    state.compose(120, 40).expect("frame");
    let text = popup_text(&state).join("\n");
    assert!(has(&text, texts().external_read_only), "只读徽标:\n{text}");
    assert!(text.contains("zcode desktop"), "外部条目标签:\n{text}");
    assert!(
        !state
            .hits
            .agent_activity_actions
            .iter()
            .any(|(_, button)| *button == AgentActivityButton::ToggleFollow),
        "跟随按钮不可点"
    );
}

/// 旧 server：没有宣告该方法时不发请求、显示「不提供」；应答说方法不存在时
/// 同样显示并不再重试。
#[test]
fn old_servers_show_unsupported_and_are_never_retried() {
    let mut state = state();
    state.set_endpoint_methods(Some(vec!["pane.focus".into()]));
    let outcome = open(&mut state, pane_owner());
    assert!(reads(&outcome).is_empty(), "未宣告的方法不发");
    assert!(overlay(&state).unsupported);
    state.compose(120, 40).expect("frame");
    let text = popup_text(&state).join("\n");
    assert!(has(&text, texts().unsupported), "不支持文案:\n{text}");
    let mut outcome = ClientShellInput::default();
    state.tick_agent_activity(Instant::now() + FOLLOW_INTERVAL * 5, &mut outcome);
    assert!(reads(&outcome).is_empty(), "不再重试");

    let mut state = self::state();
    let outcome = open(&mut state, pane_owner());
    let (id, _) = single_read(&outcome);
    let (repaint, next) = respond(
        &mut state,
        &id,
        Err(ClientShellEndpointError {
            code: Some("unsupported_method".into()),
            message: "method \"agent.activity.read\" is not available on this machine".into(),
        }),
    );
    assert!(repaint);
    assert!(next.actions.is_empty());
    assert!(overlay(&state).unsupported);
    state.handle_raw_events(vec![key(KeyCode::Char('r'))]);
    let mut outcome = ClientShellInput::default();
    state.tick_agent_activity(Instant::now() + FOLLOW_INTERVAL * 5, &mut outcome);
    assert!(reads(&outcome).is_empty(), "刷新与跟随都不再重试");
    state.compose(120, 40).expect("frame");
    assert!(has(&popup_text(&state).join("\n"), texts().unsupported));

    // 更早的 server 不认识方法名时回 `invalid_request`（serde 的 unknown variant）。
    let mut state = self::state();
    let outcome = open(&mut state, pane_owner());
    let (id, _) = single_read(&outcome);
    respond(
        &mut state,
        &id,
        Err(ClientShellEndpointError {
            code: Some("invalid_request".into()),
            message: "invalid endpoint request: unknown variant `agent.activity.read`".into(),
        }),
    );
    assert!(overlay(&state).unsupported);

    // 来源未实现只是读取失败：显示错误，跟随时下一轮照常重试。
    let mut state = self::state();
    let outcome = open(&mut state, pane_owner());
    let (id, _) = single_read(&outcome);
    respond(
        &mut state,
        &id,
        Err(ClientShellEndpointError {
            code: Some("not_implemented".into()),
            message: "agent activity is not implemented by this server".into(),
        }),
    );
    assert!(!overlay(&state).unsupported);
    assert!(overlay(&state).tree_error.is_some());
    let mut outcome = ClientShellInput::default();
    state.tick_agent_activity(Instant::now() + FOLLOW_INTERVAL * 2, &mut outcome);
    assert_eq!(reads(&outcome).len(), 1, "跟随重试");
}

/// 读取失败显示错误文案；空树、加载中各有文案。
#[test]
fn loading_empty_and_failed_states_have_their_own_text() {
    let mut state = state();
    let outcome = open(&mut state, pane_owner());
    state.compose(120, 40).expect("frame");
    assert!(
        has(&popup_text(&state).join("\n"), texts().loading),
        "加载中"
    );
    let (id, _) = single_read(&outcome);
    respond(
        &mut state,
        &id,
        Err(ClientShellEndpointError {
            code: Some("agent_not_found".into()),
            message: "pane gone".into(),
        }),
    );
    state.compose(120, 40).expect("frame");
    let failed = crate::i18n::fill(texts().read_failed_fmt, &[("error", "pane gone")]);
    assert!(has(&popup_text(&state).join("\n"), &failed), "读取失败");

    let mut state = self::state();
    let outcome = open(&mut state, pane_owner());
    let (id, _) = single_read(&outcome);
    respond(&mut state, &id, tree_result(Vec::new()));
    state.compose(120, 40).expect("frame");
    let text = popup_text(&state).join("\n");
    assert!(has(&text, texts().empty_tree), "空树:\n{text}");
    assert!(has(&text, texts().empty_content), "未选中:\n{text}");
}

/// 键盘：↑↓ 选节点，←→ 折叠 / 展开（← 在子节点上回到父节点），Enter 从头重读
/// 并把焦点交给内容，焦点在内容时 ↑↓ / PgUp / PgDn / Home / End 滚内容，
/// Esc 回到 return_to。
#[test]
fn keyboard_selects_collapses_scrolls_and_returns() {
    let mut state = state();
    state.overlay = Some(ClientShellOverlay::Help(ClientHelpOverlay {
        query: TextEditor::default(),
        search_focused: false,
        max_scroll: 0,
        scroll: 0,
    }));
    open_with_tree(&mut state);
    assert!(overlay(&state).return_to.is_some());

    state.handle_raw_events(vec![key(KeyCode::Down)]);
    assert_eq!(overlay(&state).selected_node.as_deref(), Some("a"));
    state.handle_raw_events(vec![key(KeyCode::Left)]);
    assert!(overlay(&state).collapsed.contains("a"), "← 折叠");
    assert_eq!(
        overlay(&state).visible_node_ids().collect::<Vec<_>>(),
        ["a", "b"]
    );
    state.handle_raw_events(vec![key(KeyCode::Right)]);
    assert!(!overlay(&state).collapsed.contains("a"), "→ 展开");
    state.handle_raw_events(vec![key(KeyCode::Right)]);
    assert_eq!(
        overlay(&state).selected_node.as_deref(),
        Some("a.1"),
        "已展开时 → 进第一个子节点"
    );
    state.handle_raw_events(vec![key(KeyCode::Left)]);
    assert_eq!(
        overlay(&state).selected_node.as_deref(),
        Some("a"),
        "叶子上 ← 回父节点"
    );
    state.handle_raw_events(vec![key(KeyCode::End)]);
    assert_eq!(overlay(&state).selected_node.as_deref(), Some("b"));
    state.handle_raw_events(vec![key(KeyCode::Up)]);
    assert_eq!(overlay(&state).selected_node.as_deref(), Some("a.2"));

    // 内容：连续选中只留最后一个节点的读取（单飞 + 代际）。
    let content = (0..60)
        .map(|index| format!("line {index}"))
        .collect::<Vec<_>>()
        .join("\n");
    settle(&mut state, &content);
    assert_eq!(
        overlay(&state).content.as_ref().expect("内容").node_id,
        "a.2"
    );
    // Enter 从头重读并把焦点交给内容。
    let outcome = state.handle_raw_events(vec![key(KeyCode::Enter)]);
    assert_eq!(overlay(&state).focus, AgentActivityFocus::Content);
    let (_, params) = single_read(&outcome);
    assert_eq!(params.cursor, None, "Enter 从头重读");
    settle(&mut state, &content);
    overlay_mut(&mut state).content_tail = false;
    overlay_mut(&mut state).content_scroll = 0;
    state.compose(120, 40).expect("frame");
    let max = overlay(&state).content_max_scroll;
    assert!(max > 0, "内容超出一屏");
    state.handle_raw_events(vec![key(KeyCode::Down)]);
    assert_eq!(overlay(&state).content_scroll, 1, "焦点在内容时 ↓ 滚一行");
    state.handle_raw_events(vec![key(KeyCode::PageDown)]);
    assert!(overlay(&state).content_scroll > 1, "PgDn 翻页");
    state.handle_raw_events(vec![key(KeyCode::End)]);
    state.compose(120, 40).expect("frame");
    assert_eq!(overlay(&state).content_scroll, max);
    state.handle_raw_events(vec![key(KeyCode::Home)]);
    assert_eq!(overlay(&state).content_scroll, 0);
    state.handle_raw_events(vec![key(KeyCode::Left)]);
    assert_eq!(overlay(&state).focus, AgentActivityFocus::Tree, "← 回到树");

    state.handle_raw_events(vec![key(KeyCode::Char('x'))]);
    assert!(matches!(
        state.overlay,
        Some(ClientShellOverlay::AgentActivity(_))
    ));
    state.handle_raw_events(vec![key(KeyCode::Esc)]);
    assert!(
        matches!(state.overlay, Some(ClientShellOverlay::Help(_))),
        "Esc 回到 return_to"
    );
}

/// 鼠标：Moved 只改悬浮（MENU-01），点树行选中，点开关折叠，点按钮刷新 /
/// 切跟随 / 关闭，点内容把焦点交给内容，窗外点击关闭。
#[test]
fn mouse_hovers_selects_toggles_and_clicks_buttons() {
    let mut state = state();
    open_with_tree(&mut state);
    state.compose(120, 40).expect("frame");
    let row = |state: &ClientShellState, id: &str| {
        state
            .hits
            .agent_activity_tree_rows
            .iter()
            .find(|(_, node)| node == id)
            .map(|(rect, _)| *rect)
            .expect("树行命中区")
    };
    let b_row = row(&state, "b");
    state.handle_raw_events(vec![mouse(MouseEventKind::Moved, b_row.x + 8, b_row.y)]);
    assert_eq!(overlay(&state).hovered_node.as_deref(), Some("b"));
    assert_eq!(overlay(&state).selected_node, None, "悬浮不改选中");
    state.compose(120, 40).expect("frame");
    let hovered = find_cell(&state, b_row, "cargo").expect("悬浮行");
    let palette = &state.config.palette;
    let hover =
        if crate::app::state::Palette::row_bg_is_distinct(palette.panel_bg, palette.surface0) {
            palette.surface0
        } else {
            palette.hover_row_bg()
        };
    assert_eq!(cell(&state, hovered).bg, hover, "悬浮底色");

    let outcome = state.handle_raw_events(vec![mouse(
        MouseEventKind::Down(MouseButton::Left),
        b_row.x + 8,
        b_row.y,
    )]);
    assert_eq!(overlay(&state).selected_node.as_deref(), Some("b"));
    assert_eq!(
        overlay(&state).hovered_node.as_deref(),
        Some("b"),
        "点击不改写悬浮"
    );
    assert_eq!(reads(&outcome)[0].1.node_id.as_deref(), Some("b"));
    let (id, _) = single_read(&outcome);
    respond(
        &mut state,
        &id,
        content_result("b", "boom\n", true, Some("1")),
    );

    // 折叠开关：a 的前缀里本节点那一格（竖条与留白之后）。
    let a_row = row(&state, "a");
    state.handle_raw_events(vec![mouse(
        MouseEventKind::Down(MouseButton::Left),
        a_row.x + 2,
        a_row.y,
    )]);
    assert!(overlay(&state).collapsed.contains("a"), "点开关折叠");
    assert_eq!(
        overlay(&state).selected_node.as_deref(),
        Some("b"),
        "点开关不改选中"
    );
    state.compose(120, 40).expect("frame");
    assert!(
        !state
            .hits
            .agent_activity_tree_rows
            .iter()
            .any(|(_, id)| id == "a.1"),
        "折叠后子节点不再画"
    );
    assert!(popup_text(&state).join("\n").contains('▸'));

    // 按钮：刷新重读树与内容；跟随切换；× 关闭。
    let button = |state: &ClientShellState, which: AgentActivityButton| {
        state
            .hits
            .agent_activity_actions
            .iter()
            .find(|(_, candidate)| *candidate == which)
            .map(|(rect, _)| *rect)
            .expect("按钮命中区")
    };
    let refresh = button(&state, AgentActivityButton::Refresh);
    let outcome = state.handle_raw_events(vec![mouse(
        MouseEventKind::Down(MouseButton::Left),
        refresh.x,
        refresh.y,
    )]);
    let (id, params) = single_read(&outcome);
    assert_eq!(params.node_id, None, "刷新先读树");
    let (_, next) = respond(&mut state, &id, tree_result(sample_nodes()));
    let (_, params) = single_read(&next);
    assert_eq!(params.node_id.as_deref(), Some("b"));
    assert_eq!(params.cursor, None, "刷新从头重读内容");

    let follow = button(&state, AgentActivityButton::ToggleFollow);
    state.handle_raw_events(vec![mouse(
        MouseEventKind::Down(MouseButton::Left),
        follow.x,
        follow.y,
    )]);
    assert!(!overlay(&state).follow, "点跟随按钮切换");

    let content = state.hits.agent_activity_content;
    state.handle_raw_events(vec![mouse(
        MouseEventKind::Down(MouseButton::Left),
        content.x + 2,
        content.bottom() - 2,
    )]);
    assert_eq!(overlay(&state).focus, AgentActivityFocus::Content);

    state.compose(120, 40).expect("frame");
    let close = button(&state, AgentActivityButton::Close);
    state.handle_raw_events(vec![mouse(
        MouseEventKind::Down(MouseButton::Left),
        close.x + 1,
        close.y,
    )]);
    assert!(state.overlay.is_none(), "× 关闭");

    let mut state = self::state();
    open_with_tree(&mut state);
    state.compose(120, 40).expect("frame");
    let popup = state.hits.agent_activity_popup;
    state.handle_raw_events(vec![mouse(
        MouseEventKind::Down(MouseButton::Left),
        popup.x + 2,
        popup.y + 1,
    )]);
    assert!(state.overlay.is_some(), "页内空白处点击吞掉");
    let back = button(&state, AgentActivityButton::Back);
    state.handle_raw_events(vec![mouse(
        MouseEventKind::Down(MouseButton::Left),
        back.x + 1,
        back.y,
    )]);
    assert!(state.overlay.is_none(), "‹ 返回 关闭页面");
}

/// 滚轮按指针所在的列滚动：在内容上滚内容，在树上滚树（不改选中）。
#[test]
fn wheel_scrolls_the_column_under_the_pointer() {
    let mut state = state();
    open_with_tree(&mut state);
    let long = (0..80)
        .map(|index| format!("row {index}"))
        .collect::<Vec<_>>()
        .join("\n");
    select_with_content(&mut state, "a", &long);
    state.compose(120, 40).expect("frame");
    assert!(overlay(&state).content_tail, "跟随时默认钉在底部");
    let max = overlay(&state).content_max_scroll;
    assert_eq!(overlay(&state).content_scroll, max);
    let content = state.hits.agent_activity_content;
    state.handle_raw_events(vec![mouse(
        MouseEventKind::ScrollUp,
        content.x + 3,
        content.y + 3,
    )]);
    assert!(overlay(&state).content_scroll < max, "滚轮上滚");
    assert!(!overlay(&state).content_tail, "上滚解除钉底");
    assert!(!state.hits.agent_activity_scrollbar.is_empty(), "滚动条");
    assert!(state.hits.agent_activity_scroll_metrics.is_some());

    let tree_row = state.hits.agent_activity_tree_rows[0].0;
    let before = overlay(&state).content_scroll;
    state.handle_raw_events(vec![mouse(
        MouseEventKind::ScrollDown,
        tree_row.x + 2,
        tree_row.y,
    )]);
    assert_eq!(overlay(&state).content_scroll, before, "树上的滚轮不滚内容");
    assert_eq!(overlay(&state).selected_node.as_deref(), Some("a"));
}

/// 内容滚动上界只在视图计算阶段写入：越界的滚动位置在下一帧前被夹紧；长行按
/// 正文宽度折行，上界随之计算。
#[test]
fn content_scroll_bounds_are_computed_before_rendering() {
    let mut state = state();
    open_with_tree(&mut state);
    let long = (0..40)
        .map(|index| format!("entry {index}"))
        .collect::<Vec<_>>()
        .join("\n");
    select_with_content(&mut state, "a", &long);
    overlay_mut(&mut state).content_tail = false;
    overlay_mut(&mut state).content_scroll = 10_000;
    state.compose(120, 40).expect("frame");
    let max = overlay(&state).content_max_scroll;
    assert!(max > 0);
    assert_eq!(overlay(&state).content_scroll, max, "夹到上界");
    let text_rows = state
        .hits
        .agent_activity_scroll_metrics
        .expect("滚动指标")
        .viewport_rows;
    assert_eq!(max, 40 - text_rows);

    select_with_content(&mut state, "a.1", &"x".repeat(400));
    state.compose(120, 40).expect("frame");
    let width = usize::from(state.hits.agent_activity_content.width - 3);
    let rows = overlay(&state).transcript.rows().len();
    assert_eq!(rows, 400usize.div_ceil(width), "按正文宽度折行");
}

/// 未跟随时属主的活动摘要变了：标题旁提示「有更新」，r 刷新后消失。
#[test]
fn summary_change_without_follow_shows_the_updates_badge() {
    let mut state = state();
    open_with_tree(&mut state);
    state.handle_raw_events(vec![key(KeyCode::Char('f'))]);
    state.compose(120, 40).expect("frame");
    assert!(!overlay(&state).has_updates);
    let mut projected = snapshot();
    projected.agents = vec![agent("pane_1", 2, 6)];
    state.set_snapshot(Box::new(projected));
    state.compose(120, 40).expect("frame");
    assert!(overlay(&state).has_updates);
    assert!(
        has(&popup_text(&state).join("\n"), texts().updates_badge),
        "标题旁提示"
    );
    let outcome = state.handle_raw_events(vec![key(KeyCode::Char('r'))]);
    assert!(!overlay(&state).has_updates);
    let (id, _) = single_read(&outcome);
    respond(&mut state, &id, tree_result(sample_nodes()));
    state.compose(120, 40).expect("frame");
    assert!(!has(&popup_text(&state).join("\n"), texts().updates_badge));
}

/// N17：外部条目的快照摘要来自来源的列表查询，列表让各条目分摊行数上限，被挤掉
/// 的条目在快照里只剩一截残树；窗口读到的却是只针对该条目的单独查询（整树）。
/// 两者对不上不说明有变化：列表随别的条目变来变去时不提示「有更新」。快照与树
/// 对上过之后再偏离，照常提示（宽、中两档画出徽标；窄档放不下时按取舍先丢它）。
#[test]
fn external_updates_badge_ignores_a_listed_tree_cut_short_by_the_list() {
    for (cols, rows) in [(120, 40), (90, 30), (56, 20)] {
        let mut state = state();
        let outcome = open(
            &mut state,
            AgentActivityOwner::External {
                external_id: "zcode:s1".into(),
            },
        );
        state.compose(cols, rows).expect("frame");
        let (id, _) = single_read(&outcome);
        respond(&mut state, &id, tree_result(sample_nodes()));
        state.compose(cols, rows).expect("frame");
        assert!(!overlay(&state).has_updates);

        for (running, total) in [(0, 2), (1, 3), (0, 1)] {
            state.set_snapshot(Box::new(external_summary_snapshot(running, total)));
            state.compose(cols, rows).expect("frame");
            assert!(
                !overlay(&state).has_updates,
                "{cols}x{rows} 列表里的残树 {running}/{total} 不算更新"
            );
        }
        assert!(!has(&popup_text(&state).join("\n"), texts().updates_badge));

        // 列表放得下整棵树了：与读到的树对上，不提示；之后再变才提示。
        state.set_snapshot(Box::new(external_summary_snapshot(1, 4)));
        state.compose(cols, rows).expect("frame");
        assert!(!overlay(&state).has_updates, "{cols}x{rows} 对上了");
        state.set_snapshot(Box::new(external_summary_snapshot(2, 5)));
        state.compose(cols, rows).expect("frame");
        assert!(overlay(&state).has_updates, "{cols}x{rows} 对上之后的变化");
        // 「只读」常驻页头第 1 行，「有更新」在第 2 行最右侧（不和按钮争宽度）。
        let lines = popup_text(&state);
        assert!(
            has(&lines[0], texts().external_read_only),
            "{cols}x{rows} 只读徽标: {:?}",
            lines[0]
        );
        assert!(
            has(&lines[1], texts().updates_badge),
            "{cols}x{rows} 有更新徽标: {:?}",
            lines[1]
        );

        // r 刷新：读到的新树与快照不同（又被挤掉）时，重新等快照对上。
        let outcome = state.handle_raw_events(vec![key(KeyCode::Char('r'))]);
        assert!(!overlay(&state).has_updates);
        let (id, _) = single_read(&outcome);
        respond(&mut state, &id, tree_result(sample_nodes()));
        state.set_snapshot(Box::new(external_summary_snapshot(0, 2)));
        state.compose(cols, rows).expect("frame");
        assert!(!overlay(&state).has_updates, "{cols}x{rows} 刷新后仍是残树");
    }
}

/// 外部根退出活跃列表后主动取一次历史树，结束状态不能永远停留在 Running。
#[test]
fn an_external_owner_leaving_the_active_list_refreshes_its_final_tree_once() {
    let mut state = state();
    state.set_snapshot(Box::new(external_summary_snapshot(1, 4)));
    let outcome = open(
        &mut state,
        AgentActivityOwner::External {
            external_id: "zcode:s1".into(),
        },
    );
    state.compose(120, 40).expect("活跃外部根");
    let (id, _) = single_read(&outcome);
    respond(&mut state, &id, tree_result(sample_nodes()));
    state.compose(120, 40).expect("运行树");
    let mut ended = external_summary_snapshot(0, 4);
    ended.external_agents.clear();
    state.set_snapshot(Box::new(ended));
    state.compose(120, 40).expect("根结束退出列表");
    let mut outcome = ClientShellInput::default();
    state.tick_agent_activity(Instant::now(), &mut outcome);
    let (id, params) = single_read(&outcome);
    assert_eq!(params.external_id.as_deref(), Some("zcode:s1"));
    assert_eq!(params.node_id, None);
    // 多次 compose / tick 不追加请求；在途结束后也不变成周期轮询。
    for _ in 0..3 {
        state.compose(120, 40).expect("等待最终树");
        let mut duplicate = ClientShellInput::default();
        state.tick_agent_activity(Instant::now() + FOLLOW_INTERVAL * 3, &mut duplicate);
        assert!(reads(&duplicate).is_empty());
    }
    let mut final_nodes = sample_nodes();
    final_nodes[0].status = AgentActivityStatus::Done;
    final_nodes[0].ended_at_ms = Some(90_000);
    respond(&mut state, &id, tree_result(final_nodes));
    state.compose(120, 40).expect("最终树");
    let rect = state
        .hits
        .agent_activity_tree_rows
        .iter()
        .find(|(_, id)| id == "a")
        .expect("完成行")
        .0;
    let at = find_cell(&state, rect, "explore repo").expect("完成标签");
    assert_eq!(cell(&state, at).fg, state.config.palette.text);
    assert!(find_cell(&state, rect, "●").is_some(), "已完成用待查看字形");
    assert!(!overlay(&state).has_updates);
    let mut duplicate = ClientShellInput::default();
    state.tick_agent_activity(Instant::now() + FOLLOW_INTERVAL * 8, &mut duplicate);
    assert!(reads(&duplicate).is_empty());
}

/// markdown 只给标题与列表记号着色；jsonl 取 type / role / name 做前缀。
#[test]
fn markdown_and_jsonl_lines_are_lightly_styled() {
    let mut state = state();
    open_with_tree(&mut state);
    let mut outcome = ClientShellInput::default();
    state.select_agent_activity_node("a".into(), &mut outcome);
    let (id, _) = single_read(&outcome);
    respond(
        &mut state,
        &id,
        content_result_with(
            "a",
            "# Plan\n- first step\nplain words\n",
            true,
            Some("m"),
            AgentActivityContentFormat::Markdown,
        ),
    );
    state.compose(120, 40).expect("frame");
    let content = state.hits.agent_activity_content;
    let palette = state.config.palette.clone();
    let heading = find_cell(&state, content, "# Plan").expect("标题");
    assert_eq!(cell(&state, heading).fg, palette.accent);
    assert!(cell(&state, heading)
        .modifier
        .contains(ratatui::style::Modifier::BOLD));
    let bullet = find_cell(&state, content, "- first").expect("列表");
    assert_eq!(cell(&state, bullet).fg, palette.accent, "列表记号着色");
    let word = find_cell(&state, content, "first step").expect("列表正文");
    assert_eq!(cell(&state, word).fg, palette.text, "列表正文常规色");

    let mut outcome = ClientShellInput::default();
    state.select_agent_activity_node("a.1".into(), &mut outcome);
    let (id, _) = single_read(&outcome);
    respond(
        &mut state,
        &id,
        content_result_with(
            "a.1",
            "{\"type\":\"message\",\"role\":\"assistant\",\"text\":\"hi\"}\nnot json\n",
            true,
            Some("j"),
            AgentActivityContentFormat::Jsonl,
        ),
    );
    state.compose(120, 40).expect("frame");
    let text = rows_text(&state, content).join("\n");
    assert!(
        text.contains("[message assistant] {\"text\":\"hi\"}"),
        "jsonl 前缀:\n{text}"
    );
    assert!(text.contains("not json"), "非 JSON 行原样:\n{text}");
    let prefix = find_cell(&state, content, "[message").expect("前缀");
    assert_eq!(cell(&state, prefix).fg, palette.blue);
}

/// 内容超过客户端上限时从头部丢行并提示「仅显示最新部分」；ESC 序列与制表符
/// 不进单元格。
#[test]
fn oversized_content_keeps_the_latest_part() {
    let mut state = state();
    open_with_tree(&mut state);
    let mut outcome = ClientShellInput::default();
    state.select_agent_activity_node("a".into(), &mut outcome);
    let (id, _) = single_read(&outcome);
    let line = "y".repeat(1023);
    let page = format!(
        "\u{1b}[31mfirst\u{1b}[0m\tline\n{}",
        format!("{line}\n").repeat(CONTENT_CAP_BYTES / 1024 + 8)
    );
    respond(&mut state, &id, content_result("a", &page, true, Some("z")));
    let content = overlay(&state).content.as_ref().expect("内容");
    assert!(content.truncated, "丢过头部");
    assert!(!content.text().contains("first"), "最早的行被丢弃");
    state.compose(120, 40).expect("frame");
    let text = popup_text(&state).join("\n");
    assert!(has(&text, texts().truncated), "顶部提示:\n{text}");

    let mut state = self::state();
    open_with_tree(&mut state);
    select_with_content(&mut state, "a", "\u{1b}[1mbold\u{1b}[0m\tafter\n");
    let content = overlay(&state).content.as_ref().expect("内容");
    assert_eq!(content.text(), "bold    after\n", "去掉 ESC、制表符换空格");
}

/// Agents 面板的活动行：点击打开窗口并选中该节点，树读回来后接着读它的内容。
#[test]
fn clicking_an_activity_row_in_the_panel_opens_the_window_on_that_node() {
    let mut state = state();
    state.compose(120, 40).expect("frame");
    state
        .hits
        .agent_activity_rows
        .push(super::super::state::AgentActivityHit {
            rect: Rect::new(1, 30, 20, 1),
            endpoint_id: ClientEndpointId::Local,
            owner_key: "pane:pane_1".into(),
            node_id: "a.1".into(),
        });
    let mut outcome = ClientShellInput::default();
    assert!(state.handle_agent_tree_click((3, 30), &mut outcome));
    assert_eq!(overlay(&state).selected_node.as_deref(), Some("a.1"));
    let (id, params) = single_read(&outcome);
    assert_eq!(params.node_id, None, "先读树");
    let (_, next) = respond(&mut state, &id, tree_result(sample_nodes()));
    let (_, params) = single_read(&next);
    assert_eq!(params.node_id.as_deref(), Some("a.1"), "再读选中节点");
}

/// 跟随节奏：间隔从树应答落地时起算，1 s 内不重复。
#[test]
fn follow_ticks_are_spaced_by_the_interval() {
    let mut state = state();
    open_with_tree(&mut state);
    let mut outcome = ClientShellInput::default();
    state.tick_agent_activity(Instant::now() + FOLLOW_INTERVAL * 2, &mut outcome);
    let (id, _) = single_read(&outcome);
    respond(&mut state, &id, tree_result(sample_nodes()));
    let landed = Instant::now();
    let mut outcome = ClientShellInput::default();
    state.tick_agent_activity(landed + Duration::from_millis(500), &mut outcome);
    assert!(reads(&outcome).is_empty(), "间隔内不重复");
    let mut outcome = ClientShellInput::default();
    state.tick_agent_activity(landed + FOLLOW_INTERVAL, &mut outcome);
    assert_eq!(reads(&outcome).len(), 1, "树应答落地满一个间隔再读");
}

/// 取消（断线、代际不符、泳道退役）不是读取失败：取消路径不许再产生动作
/// （`cancel_endpoint_request` 在 debug 构建里断言），被取消的读取意图放回去，
/// 下一次 tick 重发；内容续读保留原游标与追加语义。其它错误结局也不在应答里
/// 续发，交给下一次 tick。
#[test]
fn cancelled_reads_are_requeued_for_the_next_tick() {
    let mut state = state();
    let outcome = open(&mut state, pane_owner());
    let (tree_id, _) = single_read(&outcome);
    // 树读取在途时选中节点：单飞，内容读取排队。
    let mut outcome = ClientShellInput::default();
    state.select_agent_activity_node("a".into(), &mut outcome);
    assert!(reads(&outcome).is_empty(), "单飞");

    state.cancel_endpoint_request(&tree_id);
    assert!(overlay(&state).in_flight.is_none(), "取消后没有在途读取");
    assert!(overlay(&state).tree_error.is_none(), "取消不是读取失败");
    let mut outcome = ClientShellInput::default();
    state.tick_agent_activity(Instant::now(), &mut outcome);
    let (tree_id, params) = single_read(&outcome);
    assert_eq!(params.node_id, None, "下一次 tick 重发树读取");
    let (_, next) = respond(&mut state, &tree_id, tree_result(sample_nodes()));
    let (content_id, params) = single_read(&next);
    assert_eq!(params.node_id.as_deref(), Some("a"), "树回来后读内容");
    assert_eq!(params.cursor, None);

    // 续读被取消：重发时保留原游标，应答仍追加在已有内容后面。
    let (_, next) = respond(
        &mut state,
        &content_id,
        content_result("a", "page one\n", false, Some("c1")),
    );
    let (continue_id, params) = single_read(&next);
    assert_eq!(params.cursor.as_deref(), Some("c1"));
    state.cancel_endpoint_request(&continue_id);
    assert!(overlay(&state).in_flight.is_none());
    assert!(overlay(&state).error.is_none(), "取消不是读取失败");
    let mut outcome = ClientShellInput::default();
    state.tick_agent_activity(Instant::now(), &mut outcome);
    let (continue_id, params) = single_read(&outcome);
    assert_eq!(params.node_id.as_deref(), Some("a"));
    assert_eq!(params.cursor.as_deref(), Some("c1"), "保留原游标");
    assert!(
        !overlay(&state).in_flight.as_ref().expect("在途").replace,
        "仍是追加"
    );
    respond(
        &mut state,
        &continue_id,
        content_result("a", "page two\n", true, Some("c2")),
    );
    assert_eq!(
        overlay(&state).content.as_ref().expect("内容").text(),
        "page one\npage two\n"
    );

    // 其它错误结局（如超时）同样不在应答里续发：刷新时树读取超时，排队的内容
    // 读取留给下一次 tick。
    let outcome = state.handle_raw_events(vec![key(KeyCode::Char('r'))]);
    let (tree_id, _) = single_read(&outcome);
    let (repaint, next) = respond(
        &mut state,
        &tree_id,
        Err(ClientShellEndpointError {
            code: Some("endpoint_timeout".into()),
            message: "timed out".into(),
        }),
    );
    assert!(repaint, "读取失败要重绘");
    assert!(next.actions.is_empty(), "错误结局不在应答里续发");
    assert!(overlay(&state).tree_error.is_some());
    let mut outcome = ClientShellInput::default();
    state.tick_agent_activity(Instant::now(), &mut outcome);
    let (_, params) = single_read(&outcome);
    assert_eq!(
        params.node_id.as_deref(),
        Some("a"),
        "下一次 tick 发内容读取"
    );
    assert_eq!(params.cursor, None, "刷新从头重读");
}

/// 树读取往返超过跟随间隔时内容续读不被饿死：树读取在途时 tick 不再追加树
/// 读取，树应答回来后先续读内容，下一轮间隔从落地时起算。
#[test]
fn slow_tree_reads_do_not_starve_content_follow_reads() {
    let mut state = state();
    open_with_tree(&mut state);
    let mut outcome = ClientShellInput::default();
    state.select_agent_activity_node("a".into(), &mut outcome);
    let (id, _) = single_read(&outcome);
    respond(
        &mut state,
        &id,
        content_result("a", "line 0\n", true, Some("c0")),
    );
    let mut clock = Instant::now() + FOLLOW_INTERVAL * 2;
    let mut content_reads = 0;
    for round in 1..=5 {
        let mut outcome = ClientShellInput::default();
        state.tick_agent_activity(clock, &mut outcome);
        let (tree_id, params) = single_read(&outcome);
        assert_eq!(params.node_id, None, "第 {round} 轮先读树");
        // 树往返 1.1 s：期间的 tick 早已过了间隔，但不再排树读取。
        for step in 1..=11u64 {
            let mut outcome = ClientShellInput::default();
            state.tick_agent_activity(clock + Duration::from_millis(100 * step), &mut outcome);
            assert!(reads(&outcome).is_empty(), "单飞");
        }
        let (_, next) = respond(&mut state, &tree_id, tree_result(sample_nodes()));
        let (content_id, params) = single_read(&next);
        assert_eq!(
            params.node_id.as_deref(),
            Some("a"),
            "第 {round} 轮：树回来后续读内容"
        );
        let previous = format!("c{}", round - 1);
        assert_eq!(params.cursor.as_deref(), Some(previous.as_str()));
        content_reads += 1;
        let cursor = format!("c{round}");
        let (_, next) = respond(
            &mut state,
            &content_id,
            content_result("a", &format!("line {round}\n"), true, Some(&cursor)),
        );
        assert!(reads(&next).is_empty());
        clock = Instant::now() + FOLLOW_INTERVAL;
    }
    assert_eq!(content_reads, 5);
    assert!(overlay(&state)
        .content
        .as_ref()
        .expect("内容")
        .text()
        .ends_with("line 5\n"));
}

/// 右列头行的元信息（种类 · 状态 · 耗时）按整段取舍：耗时要么完整出现、要么
/// 不出现，不留悬空的「·」；标签过长时先截标签（带省略号）给耗时与种类让位。
#[test]
fn content_header_meta_is_dropped_whole_never_cut() {
    let elapsed = format_elapsed(725_000);
    assert_eq!(elapsed, "12m05s");
    let labels = [
        "cargo test suite",
        "cargo test suite!",
        "run the cargo tests",
        "run the whole cargo test suite across every crate in the workspace",
    ];
    for (cols, rows) in [(120, 40), (90, 30), (56, 20)] {
        for label in labels {
            let mut state = state();
            let outcome = open(&mut state, pane_owner());
            let (id, _) = single_read(&outcome);
            let mut nodes = sample_nodes();
            nodes[3].label = label.into();
            nodes[3].started_at_ms = Some(0);
            nodes[3].ended_at_ms = Some(725_000);
            respond(&mut state, &id, tree_result(nodes));
            select_with_content(&mut state, "b", "boom\n");
            if cols < 100 {
                state.handle_raw_events(vec![key(KeyCode::Tab)]);
            }
            state.compose(cols, rows).expect("frame");
            let content = state.hits.agent_activity_content;
            assert!(!content.is_empty(), "{cols}x{rows} 内容列可见");
            let header =
                rows_text(&state, Rect::new(content.x, content.y, content.width, 1)).join("");
            let compact = header.chars().filter(|ch| *ch != ' ').collect::<String>();
            let context = format!("{cols}x{rows} {label:?}: {header:?}");
            if compact.contains("12m") || compact.contains("·1") {
                assert!(compact.contains(&elapsed), "耗时不许截半: {context}");
            }
            let chars = compact.chars().collect::<Vec<_>>();
            for (index, ch) in chars.iter().enumerate() {
                if *ch == '·' {
                    let next = chars.get(index + 1);
                    assert!(
                        next.is_some_and(|next| !matches!(next, '·' | '●' | '○' | '…')),
                        "不留悬空的分隔符: {context}"
                    );
                }
            }
            if cols >= 90 {
                assert!(compact.contains(&elapsed), "宽窗口放得下耗时: {context}");
                assert!(
                    has(&header, texts().kind_background),
                    "宽窗口放得下种类: {context}"
                );
            }
            let short = label.len() <= 20;
            if cols == 120 && short {
                assert!(has(&header, label), "短标签完整: {context}");
            }
            if !has(&header, label) {
                assert!(header.contains('…'), "截断的标签带省略号: {context}");
            }
        }
    }
}

/// 标题栏：属主名很长时先截标题（带省略号），徽标仍画出；外部来源的只读徽标
/// 与未跟随时的「有更新」提示都不会被长标题挤掉。
#[test]
fn long_owner_names_keep_the_title_badges() {
    let long = "Refactor the parser for streaming mode across every crate in the workspace";
    for (cols, rows) in [(120, 40), (90, 30), (56, 20)] {
        let mut state = state_named("claude-main", long);
        let outcome = open(
            &mut state,
            AgentActivityOwner::External {
                external_id: "zcode:s1".into(),
            },
        );
        let (id, _) = single_read(&outcome);
        respond(&mut state, &id, tree_result(sample_nodes()));
        state.compose(cols, rows).expect("frame");
        let title = popup_text(&state)[0].clone();
        assert!(
            has(&title, texts().external_read_only),
            "{cols}x{rows} 只读徽标: {title:?}"
        );
        if cols >= 90 {
            assert!(title.contains('…'), "{cols}x{rows} 属主名截断: {title:?}");
        }
        assert!(title.contains('×'), "{cols}x{rows} 关闭按钮: {title:?}");
    }

    let long_agent = "claude refactoring the streaming parser end to end";
    for (cols, rows) in [(120, 40), (90, 30)] {
        let mut state = state_named(long_agent, "zcode desktop");
        open_with_tree(&mut state);
        state.handle_raw_events(vec![key(KeyCode::Char('f'))]);
        state.compose(cols, rows).expect("frame");
        let mut projected = snapshot();
        let mut owner = agent("pane_1", 2, 6);
        owner.name = Some(long_agent.into());
        projected.agents = vec![owner];
        state.set_snapshot(Box::new(projected));
        state.compose(cols, rows).expect("frame");
        assert!(overlay(&state).has_updates);
        let lines = popup_text(&state);
        assert!(
            has(&lines[1], texts().updates_badge),
            "{cols}x{rows} 有更新徽标: {:?}",
            lines[1]
        );
        assert!(
            lines[0].contains('…'),
            "{cols}x{rows} 属主名截断: {:?}",
            lines[0]
        );
    }
}

#[test]
fn stale_rollout_cursor_clears_old_text_and_restarts_on_the_next_tick() {
    let mut state = state();
    open_with_tree(&mut state);
    let mut outcome = ClientShellInput::default();
    state.select_agent_activity_node("a".into(), &mut outcome);
    let (id, _) = single_read(&outcome);
    let (_, next) = respond(
        &mut state,
        &id,
        content_result("a", "OLD\n", false, Some("old-cursor")),
    );
    let (id, _) = single_read(&next);
    let (_, next) = respond(
        &mut state,
        &id,
        Err(ClientShellEndpointError {
            code: Some("activity_cursor_stale".into()),
            message: "changed".into(),
        }),
    );
    assert!(
        reads(&next).is_empty(),
        "errors must not cause an immediate request loop"
    );
    assert!(
        overlay(&state).content.is_none(),
        "stale text must be removed"
    );
    let mut next = ClientShellInput::default();
    state.tick_agent_activity(Instant::now(), &mut next);
    let (id, params) = single_read(&next);
    assert!(params.cursor.is_none());
    respond(
        &mut state,
        &id,
        content_result("a", "NEW\n", true, Some("new-cursor")),
    );
    assert_eq!(overlay(&state).content.as_ref().unwrap().text(), "NEW\n");
}

#[test]
fn stale_cursor_from_an_old_generation_does_not_clear_the_current_view() {
    for change_node in [false, true] {
        let mut state = state();
        open_with_tree(&mut state);
        let mut outcome = ClientShellInput::default();
        state.select_agent_activity_node("a".into(), &mut outcome);
        let (id, _) = single_read(&outcome);
        let (_, next) = respond(
            &mut state,
            &id,
            content_result("a", "OLD\n", false, Some("old")),
        );
        let (old, _) = single_read(&next);
        if change_node {
            state.select_agent_activity_node("b".into(), &mut ClientShellInput::default());
        } else {
            state.handle_raw_events(vec![key(KeyCode::Char('r'))]);
        }
        let (repaint, actions) = respond(
            &mut state,
            &old,
            Err(ClientShellEndpointError {
                code: Some("activity_cursor_stale".into()),
                message: "old generation".into(),
            }),
        );
        assert!(!repaint);
        assert!(reads(&actions).is_empty());
        assert!(overlay(&state).error.is_none());
        assert_eq!(
            overlay(&state).selected_node.as_deref(),
            Some(if change_node { "b" } else { "a" })
        );
        if !change_node {
            assert_eq!(overlay(&state).content.as_ref().unwrap().text(), "OLD\n");
        }
    }
}

// ---- 子智能体页面（全屏）：几何、视图、筛选、收起已完成、跳转与状态栏 ----

/// 工作台状态：宣告 `client.views.set`，tick 一次让工作台接管布局。
fn workbench_page_state() -> ClientShellState {
    let mut state = state();
    state.set_endpoint_methods(Some(vec![
        "client.views.set".into(),
        "agent.activity.read".into(),
        "pane.focus".into(),
    ]));
    state.tick_workbench(Instant::now(), &mut ClientShellInput::default());
    assert!(state.workbench.enabled, "工作台已接管");
    state
}

fn press(state: &mut ClientShellState, keys: &str) -> ClientShellInput {
    state.handle_raw_events(keys.chars().map(|ch| key(KeyCode::Char(ch))).collect())
}

fn frame_row(state: &ClientShellState, cols: u16, y: u16) -> String {
    rows_text(state, Rect::new(0, y, cols, 1)).join("")
}

/// 工作台：页面是标题栏与状态栏之间的整块内容区；计算与绘制用同一个页面矩形；
/// 状态栏显示「子智能体」模式块与页面固定键位（外部来源不列跳到终端）；页面外
/// （标题栏）的点击吞掉、不关闭页面。
#[test]
fn workbench_page_fills_the_content_area_and_drives_the_status_bar() {
    for (cols, rows) in [(160, 45), (100, 30)] {
        let mut state = workbench_page_state();
        open_with_tree(&mut state);
        state.compose(cols, rows).expect("frame");
        let page = state.hits.agent_activity_popup;
        assert_eq!(page, super::super::workbench::content_area(cols, rows));
        assert_eq!(
            state.overlay_bounds(cols, rows),
            Some(page),
            "{cols}x{rows}: 视图计算与绘制共用页面矩形"
        );
        let status = frame_row(&state, cols, rows - 1);
        assert!(has(&status, texts().mode_chip.trim()), "模式块: {status:?}");
        assert!(has(&status, texts().hint_select), "页面提示: {status:?}");
        let title = frame_row(&state, cols, 0);
        assert!(
            !has(&title, texts().page_title) || page.y == 1,
            "标题栏不归页面"
        );
        state.handle_raw_events(vec![mouse(MouseEventKind::Down(MouseButton::Left), 1, 0)]);
        assert!(
            matches!(state.overlay, Some(ClientShellOverlay::AgentActivity(_))),
            "{cols}x{rows}: 标题栏的点击不关闭页面"
        );
    }
    let mut state = workbench_page_state();
    let outcome = open(
        &mut state,
        AgentActivityOwner::External {
            external_id: "zcode:s1".into(),
        },
    );
    let (id, _) = single_read(&outcome);
    respond(&mut state, &id, tree_result(sample_nodes()));
    state.compose(200, 45).expect("frame");
    let status = frame_row(&state, 200, 44);
    assert!(has(&status, texts().hint_select), "{status:?}");
    assert!(
        !has(&status, texts().hint_jump),
        "外部来源不列跳到终端: {status:?}"
    );
}

/// 经典布局各档：单栏（< 100）、每项一行（< 60）、按钮只留图标（< 80）、隐藏页头
/// 第 2 行（高 < 12）、最小提示（< 40 × 8）；页面占满整帧，最后一行是页面页脚。
#[test]
fn page_geometry_degrades_at_each_threshold() {
    use super::super::chrome::agent_page::page_layout;
    let mut state = state();
    open_with_tree(&mut state);
    state.compose(120, 40).expect("frame");
    let layout = |state: &ClientShellState, cols: u16, rows: u16| {
        let area = Rect::new(0, 0, cols, rows);
        page_layout(area, state.overlay_bounds(cols, rows), overlay(state)).expect("layout")
    };
    let wide = layout(&state, 120, 40);
    assert!(!wide.single && !wide.compact && !wide.icons && !wide.tiny);
    assert_eq!(wide.page, Rect::new(0, 0, 120, 40));
    assert_eq!(wide.footer, Rect::new(0, 39, 120, 1), "经典布局自画页脚");
    assert!(!wide.header2.is_empty());
    assert!(!wide.divider.is_empty());
    let list = wide.list.width;
    assert!((36..=64).contains(&list), "列表宽夹在 36–64: {list}");
    assert!(layout(&state, 99, 30).single);
    assert!(!layout(&state, 100, 30).single);
    assert!(layout(&state, 59, 30).compact);
    assert!(!layout(&state, 60, 30).compact);
    assert!(layout(&state, 79, 30).icons);
    assert!(!layout(&state, 80, 30).icons);
    assert!(layout(&state, 80, 11).header2.is_empty());
    assert!(!layout(&state, 80, 12).header2.is_empty());
    assert!(layout(&state, 39, 20).tiny);
    assert!(layout(&state, 80, 7).tiny);
    assert!(!layout(&state, 40, 8).tiny);

    state.compose(79, 24).expect("frame");
    let header = popup_text(&state)[0].clone();
    assert!(
        !has(&header, texts().refresh_button),
        "按钮只留图标: {header:?}"
    );
    assert!(header.contains('↻') && header.contains('×'), "{header:?}");
    state.compose(38, 7).expect("frame");
    assert!(
        has(&popup_text(&state).join("\n"), texts().too_small),
        "最小提示"
    );
}

/// 三种视图的行模型：1 层级（前序）、2 状态（按状态桶分组：失败、已完成、运行中、
/// 等待）、3 时间线（泳道，没有开始时间写「时间未知」）。所选视图记在本次会话，
/// 重开页面沿用。
#[test]
fn views_switch_with_digits_and_are_remembered_for_the_session() {
    use super::super::chrome::agent_page::ListLine;
    let mut state = state();
    open_with_tree(&mut state);
    state.compose(160, 45).expect("frame");
    assert_eq!(overlay(&state).view, AgentPageView::Tree);
    assert_eq!(
        overlay(&state).visible_node_ids().collect::<Vec<_>>(),
        ["a", "a.1", "a.2", "b"]
    );

    press(&mut state, "2");
    assert_eq!(state.agent_page_view, AgentPageView::Status);
    assert_eq!(
        overlay(&state).visible_node_ids().collect::<Vec<_>>(),
        ["b", "a.1", "a", "a.2"],
        "失败、已完成、运行中、等待"
    );
    state.compose(160, 45).expect("frame");
    let groups = overlay(&state)
        .list_lines
        .iter()
        .filter_map(|line| match line {
            ListLine::Group { bucket, count } => Some((*bucket, *count)),
            _ => None,
        })
        .collect::<Vec<_>>();
    use crate::harness::AgentBucket;
    assert_eq!(
        groups,
        [
            (AgentBucket::Failed, 1),
            (AgentBucket::Review, 1),
            (AgentBucket::Working, 1),
            (AgentBucket::Idle, 1),
        ]
    );
    let text = popup_text(&state).join("\n");
    assert!(has(&text, texts().status_failed), "组标题:\n{text}");

    press(&mut state, "3");
    state.compose(160, 45).expect("frame");
    assert_eq!(overlay(&state).view, AgentPageView::Timeline);
    assert_eq!(
        state.hits.agent_activity_tree_rows.len(),
        4,
        "每个节点一条泳道"
    );
    assert!(
        state
            .hits
            .agent_activity_tree_rows
            .iter()
            .all(|(rect, _)| rect.height == 1),
        "泳道一行"
    );
    let text = popup_text(&state).join("\n");
    assert!(
        has(&text, texts().timeline_unknown),
        "没有开始时间:\n{text}"
    );
    assert!(has(&text, texts().timeline_now), "刻度右端:\n{text}");
    state.handle_raw_events(vec![key(KeyCode::Down)]);
    assert_eq!(
        overlay(&state).selected_node.as_deref(),
        Some("a"),
        "↑↓ 选泳道"
    );

    state.handle_raw_events(vec![key(KeyCode::Esc)]);
    assert!(state.overlay.is_none());
    open_with_tree(&mut state);
    assert_eq!(overlay(&state).view, AgentPageView::Timeline, "重开沿用");
    press(&mut state, "1");
    assert_eq!(overlay(&state).view, AgentPageView::Tree);
}

/// 筛选：`/` 进入输入，按标签 / 摘要 / 角色不分大小写匹配并保留祖先；Enter 确认并
/// 保留筛选词；Esc 先清空筛选、再按一次才关闭页面；输入中 Esc 清空并退出输入。
#[test]
fn filter_keeps_ancestors_and_esc_clears_before_closing() {
    let mut state = state();
    open_with_tree(&mut state);
    press(&mut state, "/READ");
    assert!(overlay(&state).filter_editing);
    assert_eq!(overlay(&state).filter, "READ", "输入中字母不当快捷键");
    assert_eq!(
        overlay(&state).visible_node_ids().collect::<Vec<_>>(),
        ["a", "a.1"],
        "保留匹配项的祖先"
    );
    state.compose(160, 45).expect("frame");
    assert!(popup_text(&state).join("\n").contains("/ READ▏"), "输入框");
    state.handle_raw_events(vec![key(KeyCode::Backspace)]);
    assert_eq!(overlay(&state).filter, "REA");
    state.handle_raw_events(vec![key(KeyCode::Enter)]);
    assert!(!overlay(&state).filter_editing);
    assert_eq!(overlay(&state).filter, "REA");
    state.handle_raw_events(vec![key(KeyCode::Esc)]);
    assert!(state.overlay.is_some(), "第一次 Esc 只清空筛选");
    assert!(overlay(&state).filter.is_empty());
    assert_eq!(overlay(&state).visible_node_ids().count(), 4);

    press(&mut state, "/explore");
    assert_eq!(
        overlay(&state).visible_node_ids().collect::<Vec<_>>(),
        ["a"],
        "按角色（agent_type）匹配"
    );
    state.handle_raw_events(vec![key(KeyCode::Esc)]);
    assert!(!overlay(&state).filter_editing && overlay(&state).filter.is_empty());
    press(&mut state, "/scanning");
    assert_eq!(
        overlay(&state).visible_node_ids().collect::<Vec<_>>(),
        ["a"],
        "按摘要匹配"
    );
    state.handle_raw_events(vec![key(KeyCode::Esc), key(KeyCode::Esc)]);
    assert!(state.overlay.is_none(), "清空后再按 Esc 关闭页面");
}

/// 收起已完成（c 或点「已完成 N 项」行）：只隐藏整棵子树都已完成的节点，失败节点
/// 与其祖先永远可见；再按一次展开。
#[test]
fn collapsing_done_hides_whole_done_subtrees_but_never_failures() {
    let mut state = state();
    let outcome = open(&mut state, pane_owner());
    let (id, _) = single_read(&outcome);
    let mut nodes = sample_nodes();
    nodes.extend([
        node(
            "d",
            None,
            AgentActivityKind::Task,
            AgentActivityStatus::Done,
            "done parent",
            Some((0, 5_000)),
        ),
        node(
            "d.1",
            Some("d"),
            AgentActivityKind::Task,
            AgentActivityStatus::Done,
            "done child",
            Some((0, 4_000)),
        ),
        node(
            "e",
            None,
            AgentActivityKind::Task,
            AgentActivityStatus::Done,
            "done with failure",
            Some((0, 3_000)),
        ),
        node(
            "e.1",
            Some("e"),
            AgentActivityKind::Task,
            AgentActivityStatus::Failed,
            "failed child",
            Some((0, 2_000)),
        ),
    ]);
    respond(&mut state, &id, tree_result(nodes));
    press(&mut state, "c");
    assert!(overlay(&state).hide_done);
    let visible = overlay(&state).visible_node_ids().collect::<Vec<_>>();
    for hidden in ["a.1", "d", "d.1"] {
        assert!(
            !visible.contains(&hidden),
            "{hidden} 整棵已完成: {visible:?}"
        );
    }
    for shown in ["a", "a.2", "b", "e", "e.1"] {
        assert!(visible.contains(&shown), "{shown} 可见: {visible:?}");
    }
    state.compose(160, 45).expect("frame");
    assert!(has(&popup_text(&state).join("\n"), texts().show_done));
    let toggle = state
        .hits
        .agent_activity_actions
        .iter()
        .find(|(_, button)| *button == AgentActivityButton::ToggleDone)
        .map(|(rect, _)| *rect)
        .expect("已完成行");
    state.handle_raw_events(vec![mouse(
        MouseEventKind::Down(MouseButton::Left),
        toggle.x + 1,
        toggle.y,
    )]);
    assert!(!overlay(&state).hide_done, "点「已完成 N 项」展开");
    assert_eq!(overlay(&state).visible_node_ids().count(), 8);
}

/// t / 跳到终端：pane 属主关闭页面并聚焦 pane；外部来源没有终端，什么也不做。
#[test]
fn jump_to_terminal_closes_the_page_only_for_pane_owners() {
    let mut state = state();
    open_with_tree(&mut state);
    press(&mut state, "t");
    assert!(state.overlay.is_none(), "跳到终端关闭页面");

    let mut state = self::state();
    let outcome = open(
        &mut state,
        AgentActivityOwner::External {
            external_id: "zcode:s1".into(),
        },
    );
    let (id, _) = single_read(&outcome);
    respond(&mut state, &id, tree_result(sample_nodes()));
    press(&mut state, "t");
    assert!(state.overlay.is_some(), "外部来源不跳转");
    state.compose(160, 45).expect("frame");
    assert!(
        !state
            .hits
            .agent_activity_actions
            .iter()
            .any(|(_, button)| *button == AgentActivityButton::Jump),
        "外部来源没有跳到终端按钮"
    );
}

/// 转写：Text 内容走转写排版，`first_line` 是客户端累计丢弃的行数；焦点在转写时
/// Enter 交给 `toggle_all`（不重读、不换焦点）。
#[test]
fn transcript_content_feeds_the_transcript_view() {
    let mut state = state();
    open_with_tree(&mut state);
    select_with_content(&mut state, "a", "first\nsecond\n");
    state.compose(160, 45).expect("frame");
    let rows = overlay(&state).transcript.rows().len();
    assert!(rows >= 2, "转写已折行: {rows}");
    assert_eq!(overlay(&state).content.as_ref().expect("内容").dropped, 0);
    state.handle_raw_events(vec![key(KeyCode::Tab)]);
    let outcome = state.handle_raw_events(vec![key(KeyCode::Enter)]);
    assert!(reads(&outcome).is_empty(), "转写里的 Enter 不重读");
    assert_eq!(overlay(&state).focus, AgentActivityFocus::Content);

    // 头部丢行：累计丢弃的行数随内容增长。
    let mut outcome = ClientShellInput::default();
    state.select_agent_activity_node("a.1".into(), &mut outcome);
    let (id, _) = single_read(&outcome);
    let line = "z".repeat(1023);
    let page = format!("{line}\n").repeat(CONTENT_CAP_BYTES / 1024 + 8);
    respond(
        &mut state,
        &id,
        content_result("a.1", &page, true, Some("z")),
    );
    let dropped = overlay(&state).content.as_ref().expect("内容").dropped;
    assert!(dropped >= 8, "记下丢弃的行数: {dropped}");
}

/// 页面不可拖动、不保存位置：`window_key` 排除活动页，其余浮层照旧有键。
#[test]
fn the_page_has_no_floating_window_key() {
    let mut state = state();
    open_with_tree(&mut state);
    let page = state.overlay.as_ref().expect("页面");
    assert_eq!(super::super::floating_pages::window_key(page), None);
    let help = ClientShellOverlay::Help(ClientHelpOverlay {
        query: TextEditor::default(),
        search_focused: false,
        max_scroll: 0,
        scroll: 0,
    });
    assert!(super::super::floating_pages::window_key(&help).is_some());
}
