//! 「Agent 活动」二级窗口：版式与渲染（三档尺寸，断言字符与样式）、数据通道
//! （打开 / 选中 / 跟随 tick 发起的读取、代际丢弃、续读、pi 的无游标重读、旧
//! server 不支持）、键盘与鼠标两条交互路径，以及视图计算阶段写入的滚动上界。

use super::super::agent_activity_overlay::{
    format_elapsed, AgentActivityButton, AgentActivityFocus, AgentActivityOwner,
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
        },
    }
}

fn state() -> ClientShellState {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    let mut projected = snapshot();
    projected.agents = vec![agent("pane_1", 1, 4)];
    projected.external_agents = vec![crate::protocol::ClientShellExternalAgent {
        external_id: "zcode:s1".into(),
        source: "zcode".into(),
        agent_status: AgentStatus::Idle,
        label: "zcode desktop".into(),
        readable: true,
        agent: Some("zcode".into()),
        cwd: None,
        updated_at_ms: None,
        activity: crate::protocol::ClientShellAgentActivity::default(),
    }];
    state.set_snapshot(Box::new(projected));
    state.set_pane_surface(surface());
    state
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

/// 宽窗口：标题栏（标题 + 三个按钮）、左列树（前缀引导线、状态字形、标签、
/// 种类、耗时）、右列头行与正文、页脚提示；选中行 accent 反色、运行中强调色、
/// 失败红、完成灰。
#[test]
fn wide_window_renders_title_tree_content_and_footer() {
    for (cols, rows) in [(120, 40), (90, 30)] {
        let mut state = state();
        open_with_tree(&mut state);
        select_with_content(&mut state, "a", "line one\nline two\n");
        state.compose(cols, rows).expect("frame");
        let popup = state.hits.agent_activity_popup;
        assert_eq!(
            popup,
            crate::ui::modal_rect(Rect::new(0, 0, cols, rows), crate::ui::ModalSize::Large)
                .expect("large modal"),
            "{cols}x{rows}: 没有记住的位置时取 Large 模态"
        );
        let text = popup_text(&state).join("\n");
        let title = crate::i18n::fill(texts().title_fmt, &[("name", "claude-main")]);
        assert!(has(&text, &title), "{cols}x{rows} 标题:\n{text}");
        for label in ["explore repo", "read files", "write summary", "cargo test"] {
            assert!(text.contains(label), "{cols}x{rows} 树行 {label}:\n{text}");
        }
        assert!(text.contains("├─"), "{cols}x{rows} 连接线:\n{text}");
        assert!(text.contains("└─"), "{cols}x{rows} 末子连接线:\n{text}");
        assert!(text.contains('▾'), "{cols}x{rows} 折叠开关:\n{text}");
        assert!(text.contains("12s"), "{cols}x{rows} 耗时:\n{text}");
        assert!(
            text.contains("1m05s"),
            "{cols}x{rows} 失败节点耗时:\n{text}"
        );
        assert!(
            has(&text, texts().kind_subagent),
            "{cols}x{rows} 种类:\n{text}"
        );
        assert!(text.contains("line one"), "{cols}x{rows} 正文:\n{text}");
        assert!(text.contains("line two"), "{cols}x{rows} 正文:\n{text}");
        assert!(
            text.contains("Explore — scanning src"),
            "{cols}x{rows} 头行摘要:\n{text}"
        );
        assert!(has(&text, texts().tree_title), "{cols}x{rows} 左列表头");
        assert!(
            has(&text, texts().follow_on),
            "{cols}x{rows} 跟随状态:\n{text}"
        );
        assert!(has(&text, texts().hint_select), "{cols}x{rows} 页脚提示");
        assert!(has(&text, texts().refresh_button), "{cols}x{rows} 刷新按钮");
        assert!(text.contains('×'), "{cols}x{rows} 关闭按钮");
        assert!(text.contains('●'), "{cols}x{rows} 跟随按钮");
        let buttons = state
            .hits
            .agent_activity_actions
            .iter()
            .map(|(_, button)| *button)
            .collect::<Vec<_>>();
        assert_eq!(
            buttons,
            [
                AgentActivityButton::Close,
                AgentActivityButton::Refresh,
                AgentActivityButton::ToggleFollow
            ]
        );
        let order = state
            .hits
            .agent_activity_tree_rows
            .iter()
            .map(|(_, id)| id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(order, ["a", "a.1", "a.2", "b"], "前序展开");

        let palette = &state.config.palette;
        let tree_rows =
            state
                .hits
                .agent_activity_tree_rows
                .iter()
                .fold(Rect::default(), |acc, (rect, _)| {
                    if acc.is_empty() {
                        *rect
                    } else {
                        acc.union(*rect)
                    }
                });
        let explore = find_cell(&state, tree_rows, "explore repo").expect("选中行");
        assert_eq!(cell(&state, explore).bg, palette.accent, "选中行 accent 底");
        let cargo = find_cell(&state, tree_rows, "cargo test").expect("失败行");
        assert_eq!(cell(&state, cargo).fg, palette.red, "失败红");
        let read = find_cell(&state, tree_rows, "read files").expect("完成行");
        assert_eq!(cell(&state, read).fg, palette.overlay0, "完成灰");
        // 右列正文在分隔线右侧。
        let body = find_cell(&state, popup, "line one").expect("正文");
        assert!(body.0 > explore.0 + 20, "正文在右列");
        assert!(!state.hits.agent_activity_content.is_empty());
    }
}

/// 极窄（56×20）：内框窄于 60 列只显示一列，Tab 在树与内容之间切换。
#[test]
fn narrow_window_shows_one_column_and_tab_switches() {
    let mut state = state();
    open_with_tree(&mut state);
    select_with_content(&mut state, "a.1", "only in content\n");
    state.compose(56, 20).expect("frame");
    let text = popup_text(&state).join("\n");
    assert!(text.contains("read files"), "树可见:\n{text}");
    assert!(!text.contains("only in content"), "内容隐藏:\n{text}");
    assert!(text.contains("Tab"), "窄屏切列提示:\n{text}");
    assert!(has(&text, texts().hint_switch_column), "切列文案:\n{text}");
    assert!(state.hits.agent_activity_content.is_empty());

    state.handle_raw_events(vec![key(KeyCode::Tab)]);
    assert_eq!(overlay(&state).focus, AgentActivityFocus::Content);
    state.compose(56, 20).expect("frame");
    let text = popup_text(&state).join("\n");
    assert!(text.contains("only in content"), "内容可见:\n{text}");
    assert!(!text.contains("write summary"), "树隐藏:\n{text}");
    assert!(state.hits.agent_activity_tree_rows.is_empty());

    state.handle_raw_events(vec![key(KeyCode::BackTab)]);
    assert_eq!(overlay(&state).focus, AgentActivityFocus::Tree);
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
    assert_eq!(
        cell(&state, hovered).bg,
        state.config.palette.hover_row_bg(),
        "悬浮底色"
    );

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

    // 折叠开关：a 的前缀里本节点那一格。
    let a_row = row(&state, "a");
    state.handle_raw_events(vec![mouse(
        MouseEventKind::Down(MouseButton::Left),
        a_row.x + 1,
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
    assert!(state.overlay.is_some(), "窗内空白处点击吞掉");
    state.handle_raw_events(vec![mouse(
        MouseEventKind::Down(MouseButton::Left),
        popup.x.saturating_sub(2),
        popup.y,
    )]);
    assert!(state.overlay.is_none(), "窗外点击关闭");
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
    state.compose(90, 30).expect("frame");
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
    state.compose(90, 30).expect("frame");
    let width = usize::from(state.hits.agent_activity_content.width - 2);
    let rows = overlay(&state).content.as_ref().expect("内容").row_count();
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

/// 树的跟随刷新之外，跟随 tick 的节奏由 `FOLLOW_INTERVAL` 决定（不在 1 s 内重复）。
#[test]
fn follow_ticks_are_spaced_by_the_interval() {
    let mut state = state();
    open_with_tree(&mut state);
    let start = Instant::now() + FOLLOW_INTERVAL * 2;
    let mut outcome = ClientShellInput::default();
    state.tick_agent_activity(start, &mut outcome);
    let (id, _) = single_read(&outcome);
    respond(&mut state, &id, tree_result(sample_nodes()));
    let mut outcome = ClientShellInput::default();
    state.tick_agent_activity(start + Duration::from_millis(500), &mut outcome);
    assert!(reads(&outcome).is_empty(), "间隔内不重复");
    let mut outcome = ClientShellInput::default();
    state.tick_agent_activity(start + FOLLOW_INTERVAL, &mut outcome);
    assert_eq!(reads(&outcome).len(), 1, "满一个间隔再读");
}
