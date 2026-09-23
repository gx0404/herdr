//! 「监控 → 账号用量」characterization 用例：光标归属、面板可关闭与 tab 持久化、
//! 悬浮层与账号页作用域拆分、强意图刷新与轮询节流。
//!
//! 光标归属规则：只有真正覆盖终端光标坐标的页面 / 悬浮层 / 对话框 / overlay
//! 才把整帧光标置空；停靠在旁边的监控面板不能抹掉聚焦终端的插入点。
//!
//! 作用域规则：悬浮层（`hover_scope`）与账号页（`selected_*` / `accounts` /
//! `epoch`）各自持有厂商、pane、端点与账号快照；页面轮询永不带悬浮层的
//! `pane_id`，悬浮层出现 / 消失不改写页面选择。

use super::*;
use crate::api::schema::{
    AccountUsageSnapshot, Method, ResponseResult, UsageParams, UsageProviderInfo,
};
use crate::client::shell::dock::{Edge, PanelId};
use crate::client::shell::observability::{Action, Hover, HoverTarget, Page, Purpose};
use crate::client::shell::workbench::{body, View};
use crate::protocol::ClientShellAgent;
use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use std::time::{Duration, Instant};

/// 启用停靠工作台并注入终端视图；没有视图时终端面板落占位分支、根本没有光标。
fn docked() -> ClientShellState {
    docked_with(snapshot())
}

fn docked_with(snapshot: ClientShellSnapshot) -> ClientShellState {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot));
    state.set_endpoint_methods(Some(vec!["client.views.set".into(), "tab.focus".into()]));
    state.set_pane_surface(surface());
    state.compose(120, 40).expect("初始画面");
    state.tick_workbench(std::time::Instant::now(), &mut ClientShellInput::default());
    state.workbench.pending = false;
    state.workbench.acknowledged = state.workbench.revision;
    assert!(state.workbench.enabled, "工作台已启用");
    state.workbench.views.insert(
        "1".into(),
        View {
            tab: "tab_1".into(),
            surface: surface(),
            graphics: Default::default(),
        },
    );
    state
}

fn terminal_body(state: &ClientShellState) -> Rect {
    state
        .workbench
        .geometry
        .panels
        .iter()
        .find(|(panel, _)| matches!(panel, PanelId::Terminal(_)))
        .map(|(panel, area)| body(*area, panel))
        .expect("终端面板在布局中")
}

fn monitor_header(state: &ClientShellState) -> Rect {
    state
        .workbench
        .geometry
        .panels
        .iter()
        .find(|(panel, _)| *panel == PanelId::Monitor)
        .map(|(_, area)| Rect::new(area.x, area.y, area.width, 1))
        .expect("监控面板在布局中")
}

fn click(state: &mut ClientShellState, column: u16, row: u16) -> ClientShellInput {
    let mut outcome = ClientShellInput::default();
    state.handle_mouse(
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        },
        &mut outcome,
    );
    state.handle_mouse(
        MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        },
        &mut outcome,
    );
    outcome
}

fn visible_cursor_inside(frame: &FrameData, area: Rect) {
    let cursor = frame.cursor.as_ref().expect("终端聚焦时整帧光标不能被抹掉");
    assert!(cursor.visible, "光标应可见: {cursor:?}");
    assert!(
        contains(area, (cursor.x, cursor.y)),
        "光标 {cursor:?} 应落在终端矩形 {area:?} 内"
    );
}

/// 监控面板渲染所需的组件上下文：测试只关心几何、调色板与组件 token。
fn chrome_context(config: &ClientShellConfig) -> crate::client::shell::feedback::ChromeContext<'_> {
    crate::client::shell::feedback::ChromeContext {
        page_bounds: None,
        palette: &config.palette,
        components: &config.components,
        glyphs: config.border_glyphs,
        hover: None,
        spinner: "",
        now: std::time::Instant::now(),
    }
}

#[test]
fn docked_monitor_panel_keeps_the_focused_terminal_cursor() {
    let mut state = docked();
    state.workbench_open(PanelId::Monitor);
    state.workbench.dock.focused = PanelId::Terminal(1);
    let frame = state.compose(120, 40).expect("终端与监控面板并排");
    assert!(state.workbench.visible(&PanelId::Monitor));
    visible_cursor_inside(&frame, terminal_body(&state));
}

#[test]
fn classic_layout_accounts_page_still_owns_the_cursor() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    assert!(
        !state.workbench.enabled,
        "未通告 client.views.set 时走经典布局"
    );
    state.open_observation_page(Page::Accounts, &mut ClientShellInput::default());
    let frame = state.compose(120, 40).expect("经典布局账号页");
    assert!(
        frame.cursor.is_none(),
        "账号页铺满 pane 区域时光标归页面所有"
    );
}

#[test]
fn monitor_panel_docked_left_of_the_terminal_keeps_the_cursor() {
    let mut state = docked();
    state.workbench_open(PanelId::Monitor);
    assert!(state
        .workbench
        .dock
        .dock(PanelId::Monitor, &PanelId::Terminal(1), Edge::Left));
    state.workbench.dock.focused = PanelId::Terminal(1);
    let frame = state.compose(120, 40).expect("监控面板停靠左侧");
    visible_cursor_inside(&frame, terminal_body(&state));
}

#[test]
fn maximized_monitor_panel_hides_the_terminal_cursor() {
    let mut state = docked();
    state.workbench_open(PanelId::Monitor);
    state.workbench.dock.maximized = Some(PanelId::Monitor);
    let frame = state.compose(120, 40).expect("监控面板最大化");
    assert!(
        frame.cursor.is_none(),
        "最大化后终端不在画面里，光标随之消失"
    );
}

#[test]
fn overlay_without_its_own_cursor_keeps_the_uncovered_terminal_cursor() {
    let mut state = docked();
    let before = state.compose(120, 40).expect("打开前画面");
    let cursor = before.cursor.clone().expect("终端光标");
    // 快捷键帮助不带文本输入（不拥有光标）：未被它盖住的终端插入点必须保留。
    state.handle_input_bytes(b"\x02?");
    assert!(
        matches!(state.overlay, Some(ClientShellOverlay::Help(_))),
        "prefix+? 打开快捷键帮助"
    );
    let frame = state.compose(120, 40).expect("快捷键帮助");
    assert!(
        !contains(state.hits.overlay_bounds, (cursor.x, cursor.y)),
        "用例前提：浮层矩形 {:?} 不覆盖光标 {cursor:?}",
        state.hits.overlay_bounds
    );
    assert_eq!(
        frame.cursor,
        Some(cursor),
        "未覆盖终端光标的浮层不得整帧抹掉光标"
    );
}

#[test]
fn account_hover_covering_the_terminal_cursor_hides_it() {
    let mut state = docked();
    let before = state.compose(120, 40).expect("打开前画面");
    let cursor = before.cursor.clone().expect("终端光标");
    // 锚点放在光标正上方一行：悬浮层按 kit 定位从锚点下方展开（左对齐），即覆盖
    // 光标坐标。
    assert!(cursor.y > 0, "用例前提：光标上方还有一行放锚点");
    state.observability.hover = Some(Hover {
        target: HoverTarget::Agent {
            endpoint_id: state.active_endpoint_id.clone(),
            pane: "pane_1".into(),
            agent: "claude".into(),
        },
        anchor: Rect::new(cursor.x, cursor.y - 1, 1, 1),
        since: std::time::Instant::now(),
        visible: true,
        leave_at: None,
        pinned: false,
    });
    let frame = state.compose(120, 40).expect("悬浮层覆盖终端光标");
    assert!(
        contains(state.observability.hover_rect, (cursor.x, cursor.y)),
        "用例前提：悬浮层 {:?} 覆盖光标 {cursor:?}",
        state.observability.hover_rect
    );
    assert!(frame.cursor.is_none(), "被悬浮层覆盖的光标应隐藏");
}

#[test]
fn close_action_closes_the_monitor_panel_rather_than_the_focused_terminal() {
    let mut state = docked();
    state.open_observation_page(Page::Monitor, &mut ClientShellInput::default());
    state.workbench.dock.focused = PanelId::Terminal(1);
    state.observation_action(Action::Close, &mut ClientShellInput::default());
    assert!(
        !state.workbench.dock.root.contains(&PanelId::Monitor),
        "关闭动作关的是监控面板"
    );
    assert!(state.workbench.dock.root.contains(&PanelId::Terminal(1)));
    assert_eq!(state.observability.page, None);
}

#[test]
fn monitor_header_close_button_closes_the_panel_while_the_terminal_is_focused() {
    let mut state = docked();
    state.open_observation_page(Page::Accounts, &mut ClientShellInput::default());
    state.workbench.dock.focused = PanelId::Terminal(1);
    let frame = state.compose(120, 40).expect("并排画面");
    let (x, y) = cell_symbol_position(&frame, monitor_header(&state), "×");
    click(&mut state, x, y);
    assert!(
        !state.workbench.dock.root.contains(&PanelId::Monitor),
        "点击面板头的 × 后监控面板离开 dock 树"
    );
    assert_eq!(state.observability.page, None);
    let frame = state.compose(120, 40).expect("关闭后画面");
    assert!(!state.workbench.visible(&PanelId::Monitor));
    visible_cursor_inside(&frame, terminal_body(&state));
}

/// 关闭的正是聚焦面板时焦点回落到终端：只上报一次 `tab.focus`（关闭动作
/// 与命中派发块不重复收尾），偏好也只写一次。
#[test]
fn closing_the_focused_monitor_panel_publishes_tab_focus_once() {
    let mut state = docked();
    state.open_observation_page(Page::Monitor, &mut ClientShellInput::default());
    assert_eq!(state.workbench.dock.focused, PanelId::Monitor);
    let frame = state.compose(120, 40).expect("监控面板聚焦");
    let (x, y) = cell_symbol_position(&frame, monitor_header(&state), "×");
    let outcome = click(&mut state, x, y);
    assert_eq!(state.workbench.dock.focused, PanelId::Terminal(1));
    let tab_focus = outcome
        .actions
        .iter()
        .filter(|action| {
            matches!(
                action,
                ClientShellAction::Endpoint { request, .. }
                    if matches!(request.method, crate::api::schema::Method::TabFocus(_))
            )
        })
        .count();
    assert_eq!(
        tab_focus, 1,
        "同一次关闭只上报一次焦点: {:?}",
        outcome.actions
    );
}

/// 只停靠 legacy 账号面板、终端聚焦时，新的用量响应也要立即重绘，而不是等
/// 下一次每秒 tick。
#[test]
fn usage_response_repaints_a_docked_legacy_accounts_panel() {
    use crate::client::shell::observability::Purpose;
    let mut state = docked();
    state.workbench_open(PanelId::Accounts);
    state.workbench.dock.focused = PanelId::Terminal(1);
    state.sync_observation_page_with_focus();
    state.compose(120, 40).expect("账号面板停靠、终端聚焦");
    assert!(state.workbench.visible(&PanelId::Accounts));
    assert!(!state.workbench.visible(&PanelId::Monitor));
    assert_eq!(state.observability.page, None);
    let epoch = state.observability.epoch;
    assert!(
        state.receive_observation(
            epoch,
            Purpose::Usage { agent: None },
            Ok(crate::api::schema::ResponseResult::AccountUsage {
                accounts: Vec::new(),
                refresh: None,
            }),
        ),
        "停靠的账号面板可见时用量响应触发即时重绘"
    );
}

#[test]
fn locked_layout_refuses_to_close_the_monitor_panel_and_explains_why() {
    let mut state = docked();
    state.open_observation_page(Page::Monitor, &mut ClientShellInput::default());
    state.workbench.dock.locked = true;
    state.workbench.dock.focused = PanelId::Terminal(1);
    let frame = state.compose(120, 40).expect("锁定布局");
    let (x, y) = cell_symbol_position(&frame, monitor_header(&state), "×");
    click(&mut state, x, y);
    assert!(
        state.workbench.dock.root.contains(&PanelId::Monitor),
        "锁定时面板不能被关闭"
    );
    assert!(
        state.observability.message.is_some(),
        "锁定时必须给出反馈而不是静默失败"
    );
}

#[test]
fn command_palette_closes_the_monitor_panel_from_a_focused_terminal() {
    let mut state = docked();
    state.open_observation_page(Page::Monitor, &mut ClientShellInput::default());
    state.workbench.dock.focused = PanelId::Terminal(1);
    state.compose(120, 40).expect("并排画面");
    state.open_command_search();
    palette_select(&mut state, "observation:close-monitor");
    state.handle_input_bytes(b"\r");
    assert!(state.overlay.is_none());
    assert!(
        !state.workbench.dock.root.contains(&PanelId::Monitor),
        "命令面板动作关闭监控面板"
    );
}

#[test]
fn observability_paint_keeps_the_pane_cursor_outside_the_page_rect() {
    use crate::client::shell::observability::State;
    let config = ClientShellConfig::from_config(&Config::default());
    let state = State::new(&config);
    let cursor = crate::protocol::CursorState {
        x: 5,
        y: 5,
        visible: true,
        shape: 2,
    };
    let mut beside =
        crate::client::shell::compose_canvas::ComposeCanvas::reuse_or_new(None, 120, 40);
    beside.set_cursor(Some(cursor.clone()));
    let cx = chrome_context(&config);
    let painted = state
        .paint(
            &mut beside,
            Rect::new(60, 1, 60, 38),
            &cx,
            Some(Page::Monitor),
            false,
        )
        .expect("页面已绘制");
    assert_eq!(painted.page_rect, Rect::new(60, 1, 60, 38));
    assert_eq!(
        beside.cursor(),
        Some(cursor.clone()),
        "页面矩形不含光标时保留"
    );

    let mut covering =
        crate::client::shell::compose_canvas::ComposeCanvas::reuse_or_new(None, 120, 40);
    covering.set_cursor(Some(cursor));
    state
        .paint(
            &mut covering,
            Rect::new(0, 1, 120, 38),
            &cx,
            Some(Page::Monitor),
            false,
        )
        .expect("页面已绘制");
    assert!(covering.cursor().is_none(), "页面矩形覆盖光标时置空");
}

#[test]
fn selected_monitor_tab_survives_terminal_focus_and_keeps_usage_polling() {
    let mut state = docked();
    state.set_endpoint_methods(Some(vec![
        "client.views.set".into(),
        "tab.focus".into(),
        "pane.focus".into(),
        "account.usage.get".into(),
        "account.usage.providers".into(),
    ]));
    // 总览态要等厂商列表到达才逐厂商轮询：先把列表喂进来。
    deliver_providers(&mut state, vec![provider("claude", &["claude:default"])]);
    state.open_observation_page(Page::Accounts, &mut ClientShellInput::default());
    assert_eq!(state.observability.page, Some(Page::Accounts));
    state.compose(120, 40).expect("账号页停靠");
    let terminal = terminal_body(&state);
    click(&mut state, terminal.x + 1, terminal.y + 1);
    assert_eq!(state.workbench.dock.focused, PanelId::Terminal(1));
    assert_eq!(state.observability.page, None, "键盘回到终端");
    assert_eq!(
        state.observability.monitor_tab,
        Page::Accounts,
        "用户选中的 tab 不因焦点变化回落到系统页"
    );
    state.compose(120, 40).expect("终端聚焦、账号页仍停靠");
    assert_eq!(
        state.observability.monitor_tab,
        Page::Accounts,
        "渲染期不改写选中的 tab"
    );
    assert!(
        !state.observability.page_rect.is_empty(),
        "面板仍在绘制页面"
    );
    // 点面板头中部（避开左缘的分隔线把手）把焦点交回监控面板。
    let header = monitor_header(&state);
    click(&mut state, header.x + header.width / 2, header.y);
    assert_eq!(state.workbench.dock.focused, PanelId::Monitor);
    assert_eq!(
        state.observability.page,
        Some(Page::Accounts),
        "点回面板时仍是账号页"
    );
    click(&mut state, terminal.x + 1, terminal.y + 1);
    assert_eq!(state.observability.page, None);
    let mut outcome = ClientShellInput::default();
    state.tick_observability(
        std::time::Instant::now() + std::time::Duration::from_secs(1),
        &mut outcome,
    );
    assert!(
        outcome.actions.iter().any(|action| matches!(
            action,
            ClientShellAction::Endpoint { request, .. }
                if matches!(request.method, crate::api::schema::Method::AccountUsageGet(_))
        )),
        "账号页停靠时即使终端聚焦也继续轮询用量"
    );
}

#[test]
fn monitor_tab_is_persisted_and_restored_from_preferences() {
    let path = std::env::temp_dir().join(format!(
        "herdr-shell-monitor-tab-{}.json",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let mut state = ClientShellState::new(
        ClientShellConfig::from_config(&Config::default()).with_preferences_path(path.clone()),
    );
    state.set_snapshot(Box::new(snapshot()));
    state.set_endpoint_methods(Some(vec!["client.views.set".into(), "tab.focus".into()]));
    state.set_pane_surface(surface());
    state.compose(120, 40).expect("初始画面");
    state.tick_workbench(std::time::Instant::now(), &mut ClientShellInput::default());
    state.open_observation_page(Page::Settings, &mut ClientShellInput::default());
    let saved = preferences::load(&path).expect("偏好已写入");
    assert_eq!(saved.monitor_tab, Some(Page::Settings));
    let restored = ClientShellState::new(
        ClientShellConfig::from_config(&Config::default()).with_preferences_path(path.clone()),
    );
    assert_eq!(restored.observability.monitor_tab, Page::Settings);
    std::fs::remove_file(path).expect("remove preferences");
}

#[test]
fn blit_clamps_an_out_of_range_cursor_instead_of_dropping_it() {
    // resize 首帧：旧 surface 比新区域大，光标越界时夹回区域边缘但标记不可见，
    // 宿主不会把边缘当成真实插入点；区域内的光标原样透传。
    let mut source = surface();
    source.frame.cursor = Some(crate::protocol::CursorState {
        x: 3,
        y: 1,
        visible: true,
        shape: 2,
    });
    let mut canvas =
        crate::client::shell::compose_canvas::ComposeCanvas::reuse_or_new(None, 40, 10);
    canvas.blit_frame(&source.frame, Rect::new(10, 2, 2, 1));
    assert_eq!(
        canvas.cursor(),
        Some(crate::protocol::CursorState {
            x: 11,
            y: 2,
            visible: false,
            shape: 2,
        }),
        "越界光标夹回边缘且不可见"
    );
    let mut canvas =
        crate::client::shell::compose_canvas::ComposeCanvas::reuse_or_new(None, 40, 10);
    canvas.blit_frame(&source.frame, Rect::new(10, 2, 10, 5));
    assert_eq!(
        canvas.cursor(),
        Some(crate::protocol::CursorState {
            x: 13,
            y: 3,
            visible: true,
            shape: 2,
        }),
        "区域内的光标保持可见"
    );
    let mut canvas =
        crate::client::shell::compose_canvas::ComposeCanvas::reuse_or_new(None, 40, 10);
    canvas.blit_frame(&source.frame, Rect::new(10, 2, 0, 0));
    assert!(canvas.cursor().is_none(), "空区域没有光标可放");
}

// ---------------------------------------------------------------------------
// 悬浮层 / 页面作用域、强意图刷新与轮询节流（计划 1.0 B 用例 + U-4/U-7/U-8/OBS-14）
// ---------------------------------------------------------------------------

fn agent_in_pane(pane_id: &str, agent: &str) -> ClientShellAgent {
    ClientShellAgent {
        pane_id: pane_id.into(),
        workspace_id: "ws_1".into(),
        tab_id: "tab_1".into(),
        name: Some(agent.into()),
        display_agent: None,
        agent: Some(agent.into()),
        title: None,
        terminal_title: None,
        terminal_title_stripped: None,
        agent_status: crate::api::schema::AgentStatus::Idle,
        state_change_seq: 1,
        state_labels: Vec::new(),
        tokens: Vec::new(),
        focused: true,
        launch_seq: 0,
        activity: Default::default(),
    }
}

fn provider(agent: &str, accounts: &[&str]) -> UsageProviderInfo {
    UsageProviderInfo {
        agent: agent.into(),
        label: agent.to_uppercase(),
        source_url: "https://example.invalid".into(),
        method: "cli".into(),
        account_scope: "account".into(),
        minimum_interval_seconds: 300,
        configured_accounts: accounts.iter().map(|id| (*id).to_string()).collect(),
        installed: Some(true),
        // 测试里 claude / antigravity 视为服务端宣告支持回调开关。
        supports_callback: matches!(agent, "claude" | "antigravity"),
    }
}

fn account(agent: &str, id: &str) -> AccountUsageSnapshot {
    AccountUsageSnapshot {
        account_id: id.into(),
        account_label: id.into(),
        agent: agent.into(),
        provider: agent.into(),
        status: crate::api::schema::ObservationStatus::Ready,
        ..Default::default()
    }
}

/// 往活动端点快照里再加一个运行 `agent` 的 pane（绑定入口要求 pane 在快照里
/// 且运行该账号厂商的 agent）。
fn add_agent_pane(state: &mut ClientShellState, pane_id: &str, agent: &str) {
    let mut snapshot = state.snapshot.as_deref().cloned().expect("活动端点有快照");
    snapshot.agents.push(agent_in_pane(pane_id, agent));
    state.set_snapshot(Box::new(snapshot));
}

/// 停靠工作台 + 全部用量端点方法 + 聚焦 pane 运行 claude 的快照。
fn usage_ready() -> ClientShellState {
    let mut snapshot = snapshot();
    snapshot.agents.push(agent_in_pane("pane_1", "claude"));
    usage_ready_with(snapshot)
}

/// 同 `usage_ready`，快照由调用方给。
fn usage_ready_with(snapshot: ClientShellSnapshot) -> ClientShellState {
    let mut state = docked_with(snapshot);
    state.set_endpoint_methods(Some(vec![
        "client.views.set".into(),
        "tab.focus".into(),
        "pane.focus".into(),
        "account.usage.get".into(),
        "account.usage.refresh".into(),
        "account.usage.providers".into(),
        "account.binding.set".into(),
    ]));
    // 预热一次 tick：真实客户端连上后持续 tick，活动 source 早已建立；
    // 否则用例里的首个 tick 会触发「换端点」大清理，掩盖被测行为。
    tick(&mut state, Instant::now());
    state
}

fn deliver_providers(state: &mut ClientShellState, providers: Vec<UsageProviderInfo>) {
    let epoch = state.observability.epoch;
    state.receive_observation(
        epoch,
        Purpose::Providers,
        Ok(ResponseResult::AccountUsageProviders { providers }),
    );
}

/// 投递页面作用域的用量响应。响应的 purpose 与请求一致：页面已选厂商时请求是
/// `usage:<agent>`（该厂商的响应即整个作用域的权威快照），总览态的整体请求是
/// `agent=None`；逐厂商的总览响应用 `deliver_usage_for`。
fn deliver_usage(state: &mut ClientShellState, accounts: Vec<AccountUsageSnapshot>) -> bool {
    let epoch = state.observability.epoch;
    let agent = state.observability.selected_provider.clone();
    state.receive_observation(
        epoch,
        Purpose::Usage { agent },
        Ok(ResponseResult::AccountUsage {
            accounts,
            refresh: None,
        }),
    )
}

fn tick(state: &mut ClientShellState, now: Instant) -> ClientShellInput {
    let mut outcome = ClientShellInput::default();
    state.tick_observability(now, &mut outcome);
    outcome
}

/// 本次 tick 发出的用量请求：`(manual, params)`。
fn usage_calls(outcome: &ClientShellInput) -> Vec<(bool, UsageParams)> {
    outcome
        .actions
        .iter()
        .filter_map(|action| match action {
            ClientShellAction::Endpoint { request, .. } => match &request.method {
                Method::AccountUsageGet(params) => Some((false, params.clone())),
                Method::AccountUsageRefresh(params) => Some((true, params.clone())),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

fn providers_calls(outcome: &ClientShellInput) -> usize {
    outcome
        .actions
        .iter()
        .filter(|action| {
            matches!(
                action,
                ClientShellAction::Endpoint { request, .. }
                    if matches!(request.method, Method::AccountUsageProviders(_))
            )
        })
        .count()
}

fn moved(state: &mut ClientShellState, column: u16, row: u16) {
    state.handle_mouse(
        MouseEvent {
            kind: MouseEventKind::Moved,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        },
        &mut ClientShellInput::default(),
    );
}

#[test]
fn switching_provider_invalidates_inflight_usage_and_requests_again() {
    let mut state = usage_ready();
    deliver_providers(
        &mut state,
        vec![
            provider("claude", &["claude:default"]),
            provider("codex", &["codex:default"]),
        ],
    );
    state.open_observation_page(Page::Accounts, &mut ClientShellInput::default());
    let t0 = Instant::now() + Duration::from_secs(1);
    let first = usage_calls(&tick(&mut state, t0));
    assert_eq!(first.len(), 1, "账号页打开后立即请求用量");
    assert_eq!(first[0].1.agent.as_deref(), Some("claude"));
    let old_epoch = state.observability.epoch;

    // 显式切换厂商：在途请求作废，立刻用强意图刷新重新请求新厂商。
    state.observation_action(
        Action::Provider("codex".into()),
        &mut ClientShellInput::default(),
    );
    assert_ne!(state.observability.epoch, old_epoch, "切换厂商推进 epoch");
    let again = usage_calls(&tick(&mut state, t0 + Duration::from_millis(10)));
    assert_eq!(again.len(), 1, "切换后不等在途请求返回就重新请求");
    assert!(
        again[0].0,
        "显式选择 = 强意图刷新，走 account.usage.refresh"
    );
    assert_eq!(again[0].1.agent.as_deref(), Some("codex"));
    assert_eq!(again[0].1.pane_id, None, "页面轮询不带 pane_id");

    // 旧 epoch 的响应到达：丢弃，不污染新厂商的页面。
    assert!(!state.receive_observation(
        old_epoch,
        Purpose::Usage { agent: None },
        Ok(ResponseResult::AccountUsage {
            accounts: vec![account("claude", "claude:default")],
            refresh: None,
        }),
    ));
    assert!(
        state.observability.accounts.is_empty(),
        "旧 epoch 的响应不得写入页面"
    );
    assert_eq!(
        state.observability.selected_provider.as_deref(),
        Some("codex")
    );

    // 新 epoch 的响应到达：写入；且响应内容不反写用户选择的厂商。
    assert!(deliver_usage(
        &mut state,
        vec![account("claude", "claude:default")]
    ));
    assert_eq!(state.observability.accounts.len(), 1);
    assert_eq!(
        state.observability.selected_provider.as_deref(),
        Some("codex"),
        "响应里的 agent 不得反写 selected_provider"
    );
}

#[test]
fn usage_polling_is_throttled_to_two_seconds_but_switching_bypasses_it() {
    let mut state = usage_ready();
    deliver_providers(
        &mut state,
        vec![
            provider("claude", &["claude:default"]),
            provider("codex", &["codex:default"]),
        ],
    );
    state.open_observation_page(Page::Accounts, &mut ClientShellInput::default());
    let t0 = Instant::now() + Duration::from_secs(1);
    assert_eq!(usage_calls(&tick(&mut state, t0)).len(), 1);
    assert!(deliver_usage(
        &mut state,
        vec![account("claude", "claude:default")]
    ));
    assert!(
        usage_calls(&tick(&mut state, t0 + Duration::from_secs(1))).is_empty(),
        "1 秒后仍在节流窗口内"
    );
    let polled = usage_calls(&tick(&mut state, t0 + Duration::from_secs(2)));
    assert_eq!(polled.len(), 1, "2 秒到期后再次轮询");
    assert!(!polled[0].0, "自动轮询是普通 get");
    assert!(deliver_usage(
        &mut state,
        vec![account("claude", "claude:default")]
    ));

    // 切换厂商绕过 2 秒节流，且是强意图刷新。
    state.observation_action(
        Action::Provider("codex".into()),
        &mut ClientShellInput::default(),
    );
    let switched = usage_calls(&tick(&mut state, t0 + Duration::from_millis(2500)));
    assert_eq!(switched.len(), 1, "切换厂商不等节流窗口");
    assert!(switched[0].0, "切换厂商走 account.usage.refresh");
    assert_eq!(switched[0].1.agent.as_deref(), Some("codex"));
}

#[test]
fn hover_scope_does_not_leak_into_accounts_page() {
    let mut state = usage_ready();
    deliver_providers(
        &mut state,
        vec![
            provider("claude", &["claude:default"]),
            provider("codex", &["codex:default"]),
        ],
    );
    state.open_observation_page(Page::Accounts, &mut ClientShellInput::default());
    let t0 = Instant::now() + Duration::from_secs(1);
    assert_eq!(usage_calls(&tick(&mut state, t0)).len(), 1);
    assert!(deliver_usage(
        &mut state,
        vec![account("claude", "claude:default")]
    ));
    // 页面作用域基线；页面已滚动到第 2 行，悬浮层不得把它清零。
    let provider_before = state.observability.selected_provider.clone();
    let account_before = state.observability.selected_account.clone();
    let accounts_before = state.observability.accounts.clone();
    let epoch_before = state.observability.epoch;
    state.observability.account_scroll = 2;
    assert_eq!(state.observability.selected_pane, None);

    state.compose(120, 40).expect("账号页停靠");
    let rect = state
        .hits
        .agents
        .iter()
        .find(|(_, pane)| pane == "pane_1")
        .map(|(rect, _)| *rect)
        .or_else(|| {
            state
                .hits
                .endpoint_agents
                .iter()
                .find(|(_, _, pane)| pane == "pane_1")
                .map(|(rect, _, _)| *rect)
        })
        .unwrap_or_else(|| {
            panic!(
                "Agents 面板列出 pane_1: agents={:?} endpoint_agents={:?} collapsed={}",
                state.hits.agents, state.hits.endpoint_agents, state.sidebar_collapsed
            )
        });
    moved(&mut state, rect.x, rect.y);
    assert!(
        state
            .observability
            .hover
            .as_ref()
            .is_some_and(|hover| !hover.visible),
        "扫过 Agents 行先进入延时状态"
    );
    // 推进 hover_delay_ms（默认 400ms）后悬浮层可见并发出自己的请求。
    let shown = tick(&mut state, t0 + Duration::from_millis(450));
    assert!(
        state
            .observability
            .hover
            .as_ref()
            .is_some_and(|hover| hover.visible),
        "400ms 后悬浮层可见"
    );
    let calls = usage_calls(&shown);
    assert_eq!(
        calls.len(),
        1,
        "悬浮层可见只发悬浮层自己的请求，不提前唤醒页面轮询: {calls:?}"
    );
    assert_eq!(
        calls[0].1.pane_id.as_deref(),
        Some("pane_1"),
        "悬浮层按 pane 请求用量"
    );
    assert_eq!(
        state.observability.hover_scope.provider.as_deref(),
        Some("claude")
    );
    assert_eq!(
        state.observability.hover_scope.pane.as_deref(),
        Some("pane_1")
    );
    assert_eq!(state.observability.selected_provider, provider_before);
    assert_eq!(state.observability.selected_account, account_before);
    assert_eq!(
        state.observability.selected_pane, None,
        "悬浮层不写页面的 pane"
    );
    assert_eq!(
        state.observability.accounts, accounts_before,
        "悬浮层不清空页面账号"
    );
    assert_eq!(
        state.observability.epoch, epoch_before,
        "悬浮层不作废页面在途请求"
    );
    assert_eq!(
        state.observability.account_scroll, 2,
        "悬浮层不清零页面的滚动位置"
    );
    assert_eq!(state.observability.hover_scope.scroll, 0);

    // 悬浮层结束：页面作用域依旧不变，pane 仍为空。
    if let Some(hover) = state.observability.hover.as_mut() {
        hover.leave_at = Some(t0 + Duration::from_millis(500));
    }
    tick(&mut state, t0 + Duration::from_secs(1));
    assert!(state.observability.hover.is_none());
    assert_eq!(state.observability.hover_scope.pane, None);
    assert_eq!(state.observability.hover_scope.provider, None);
    assert_eq!(state.observability.selected_pane, None);
    assert_eq!(state.observability.selected_provider, provider_before);
    assert_eq!(state.observability.accounts, accounts_before);
    assert_eq!(state.observability.epoch, epoch_before);
}

#[test]
fn hover_usage_response_lands_in_the_hover_scope_only() {
    let mut state = usage_ready();
    state.open_observation_page(Page::Accounts, &mut ClientShellInput::default());
    let page_accounts = vec![account("codex", "codex:default")];
    state.observability.accounts.clone_from(&page_accounts);
    state.observability.hover = Some(Hover {
        target: HoverTarget::Agent {
            endpoint_id: state.active_endpoint_id.clone(),
            pane: "pane_1".into(),
            agent: "claude".into(),
        },
        anchor: Rect::new(0, 20, 24, 2),
        since: Instant::now() - Duration::from_secs(1),
        visible: false,
        leave_at: None,
        pinned: false,
    });
    let outcome = tick(&mut state, Instant::now() + Duration::from_secs(1));
    let hover_call = usage_calls(&outcome)
        .into_iter()
        .find(|(_, params)| params.pane_id.as_deref() == Some("pane_1"))
        .expect("悬浮层请求");
    assert!(hover_call.0, "悬浮层首次可见走强意图刷新");
    assert_eq!(hover_call.1.agent.as_deref(), Some("claude"));
    let hover_epoch = state.observability.hover_scope.epoch;
    assert!(state.receive_observation(
        hover_epoch,
        Purpose::HoverUsage { agent: None },
        Ok(ResponseResult::AccountUsage {
            accounts: vec![account("claude", "claude:default")],
            refresh: None,
        }),
    ));
    assert_eq!(state.observability.hover_scope.accounts.len(), 1);
    assert_eq!(
        state.observability.accounts, page_accounts,
        "悬浮层响应不写页面账号"
    );
}

#[test]
fn hover_scope_targeting_an_offline_endpoint_falls_back_to_the_active_one() {
    let mut state = usage_ready();
    let ghost = ClientEndpointId::Ssh(crate::client::endpoint::ProfileId::generate());
    state.observability.hover = Some(Hover {
        target: HoverTarget::Agent {
            endpoint_id: ghost.clone(),
            pane: "pane_1".into(),
            agent: "claude".into(),
        },
        anchor: Rect::new(0, 20, 24, 2),
        since: Instant::now() - Duration::from_secs(1),
        visible: false,
        leave_at: None,
        pinned: false,
    });
    let outcome = tick(&mut state, Instant::now() + Duration::from_secs(1));
    let targets = outcome
        .actions
        .iter()
        .filter_map(|action| match action {
            ClientShellAction::Endpoint {
                endpoint_id,
                request,
                ..
            } if matches!(request.method, Method::AccountUsageRefresh(_)) => {
                Some(endpoint_id.clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        targets,
        vec![state.active_endpoint_id.clone()],
        "目标端点不在线时回退到活动端点"
    );
    assert!(
        state.observability.message.is_some(),
        "回退时在页脚说明原因"
    );
}

#[test]
fn manual_refresh_survives_a_pending_request_and_retries_within_200ms() {
    let mut state = usage_ready();
    deliver_providers(&mut state, vec![provider("claude", &["claude:default"])]);
    state.open_observation_page(Page::Accounts, &mut ClientShellInput::default());
    let t0 = Instant::now() + Duration::from_secs(1);
    assert_eq!(usage_calls(&tick(&mut state, t0)).len(), 1, "首个请求在途");
    // 在途期间按下刷新：被 pending 挡下，但不能丢。
    state.observation_action(Action::Refresh, &mut ClientShellInput::default());
    assert!(
        usage_calls(&tick(&mut state, t0 + Duration::from_millis(10))).is_empty(),
        "在途请求未返回时不重复发送"
    );
    assert!(deliver_usage(
        &mut state,
        vec![account("claude", "claude:default")]
    ));
    assert!(
        usage_calls(&tick(&mut state, t0 + Duration::from_millis(100))).is_empty(),
        "重试间隔未到"
    );
    let retried = usage_calls(&tick(&mut state, t0 + Duration::from_millis(260)));
    assert_eq!(retried.len(), 1, "200ms 内重试而不是等 2 秒");
    assert!(retried[0].0, "重试发出的仍是 account.usage.refresh");
}

#[test]
fn switching_provider_keeps_old_accounts_dimmed_while_refreshing() {
    let mut state = usage_ready();
    deliver_providers(
        &mut state,
        vec![
            provider("claude", &["claude:default"]),
            provider("codex", &["codex:default"]),
        ],
    );
    state.open_observation_page(Page::Accounts, &mut ClientShellInput::default());
    let t0 = Instant::now() + Duration::from_secs(1);
    assert_eq!(
        usage_calls(&tick(&mut state, t0)).len(),
        1,
        "自动选中的刷新已发出"
    );
    assert!(deliver_usage(
        &mut state,
        vec![account("claude", "claude:default")]
    ));
    assert!(!state.observability.refreshing(), "响应到达后不再刷新中");
    state.observation_action(
        Action::Provider("codex".into()),
        &mut ClientShellInput::default(),
    );
    assert_eq!(
        state.observability.accounts.len(),
        1,
        "切换后旧账号保留（变暗）而不是清空为「请选择厂商」"
    );
    assert!(
        state.observability.refreshing(),
        "切换即排队强意图刷新 = 刷新中"
    );
    let frame = state.compose(120, 40).expect("账号页");
    let (x, y) = find_text(&frame, "claude:default").expect("旧账号仍在画面上");
    assert!(
        cell_dimmed(&frame, x, y),
        "刷新中旧账号行应带 DIM: {:?}",
        frame_row(&frame, y)
    );
    assert!(
        !state
            .observability
            .hits
            .iter()
            .any(|(_, action)| matches!(action, Action::Refresh)),
        "刷新中「刷新」按钮退化为状态提示，不回填命中区"
    );
    let sent = usage_calls(&tick(&mut state, t0 + Duration::from_millis(10)));
    assert_eq!(sent.len(), 1);
    assert!(sent[0].0, "切换后发出的是 account.usage.refresh");
    assert!(state.observability.refreshing(), "手动刷新在途仍是刷新中");
    assert!(deliver_usage(
        &mut state,
        vec![account("codex", "codex:default")]
    ));
    assert!(!state.observability.refreshing(), "响应到达后清除刷新中");
    assert_eq!(state.observability.accounts[0].agent, "codex");
    let frame = state.compose(120, 40).expect("账号页");
    let (x, y) = find_text(&frame, "codex:default").expect("新账号已上屏");
    assert!(!cell_dimmed(&frame, x, y), "新数据到达后不再变暗");
    assert!(
        state
            .observability
            .hits
            .iter()
            .any(|(_, action)| matches!(action, Action::Refresh)),
        "刷新结束后「刷新」按钮恢复命中区"
    );
}

#[test]
fn accounts_page_has_a_refresh_button_that_sends_a_manual_refresh() {
    let mut state = usage_ready();
    deliver_providers(&mut state, vec![provider("claude", &["claude:default"])]);
    state.open_observation_page(Page::Accounts, &mut ClientShellInput::default());
    assert!(deliver_usage(
        &mut state,
        vec![account("claude", "claude:default")]
    ));
    let t0 = Instant::now() + Duration::from_secs(1);
    assert!(deliver_usage(
        &mut state,
        vec![account("claude", "claude:default")]
    ));
    // 先把首个请求收掉，让轮询进入节流窗口。
    assert_eq!(usage_calls(&tick(&mut state, t0)).len(), 1);
    assert!(deliver_usage(
        &mut state,
        vec![account("claude", "claude:default")]
    ));
    state.compose(120, 40).expect("账号页");
    let (rect, _) = state
        .observability
        .hits
        .iter()
        .find(|(_, action)| matches!(action, Action::Refresh))
        .cloned()
        .expect("账号页有「刷新」按钮");
    click(&mut state, rect.x, rect.y);
    let calls = usage_calls(&tick(&mut state, t0 + Duration::from_millis(10)));
    assert_eq!(calls.len(), 1, "点击刷新绕过节流立即请求");
    assert!(calls[0].0, "刷新按钮发 account.usage.refresh");
}

#[test]
fn cycle_account_is_disabled_without_a_second_candidate_and_explains_why() {
    let mut state = usage_ready();
    deliver_providers(&mut state, vec![provider("claude", &["claude:default"])]);
    state.open_observation_page(Page::Accounts, &mut ClientShellInput::default());
    assert!(deliver_usage(
        &mut state,
        vec![account("claude", "claude:default")]
    ));
    assert_eq!(
        state.observability.selected_provider.as_deref(),
        Some("claude")
    );
    let epoch = state.observability.epoch;
    state.observation_action(Action::CycleAccount, &mut ClientShellInput::default());
    assert_eq!(
        state.observability.epoch, epoch,
        "单账号时切换账号不发起请求"
    );
    assert_eq!(state.observability.selected_account, None);
    assert!(
        state.observability.message.is_some(),
        "禁用态给出说明而不是静默"
    );
    state.compose(120, 40).expect("账号页");
    assert!(
        !state
            .observability
            .hits
            .iter()
            .any(|(_, action)| matches!(action, Action::CycleAccount)),
        "单账号时「切换账号」按钮不回填命中区"
    );
}

#[test]
fn cycle_account_rotates_between_candidates_and_requests_the_next_one() {
    let mut state = usage_ready();
    deliver_providers(
        &mut state,
        vec![provider("claude", &["claude:work", "claude:home"])],
    );
    state.open_observation_page(Page::Accounts, &mut ClientShellInput::default());
    state.observation_action(Action::CycleAccount, &mut ClientShellInput::default());
    assert_eq!(
        state.observability.selected_account.as_deref(),
        Some("claude:work")
    );
    state.observation_action(Action::CycleAccount, &mut ClientShellInput::default());
    assert_eq!(
        state.observability.selected_account.as_deref(),
        Some("claude:home")
    );
    let calls = usage_calls(&tick(&mut state, Instant::now() + Duration::from_secs(1)));
    assert_eq!(calls.len(), 1);
    assert!(calls[0].0, "切换账号 = 强意图刷新");
    assert_eq!(calls[0].1.account_id.as_deref(), Some("claude:home"));

    // 总览态：在可见（已列出厂商的已配置）账号间轮转。
    state.observability.selected_provider = None;
    state.observability.selected_account = None;
    deliver_providers(
        &mut state,
        vec![
            provider("claude", &["claude:default"]),
            provider("codex", &["codex:default"]),
        ],
    );
    state.observability.selected_provider = None;
    state.observation_action(Action::CycleAccount, &mut ClientShellInput::default());
    assert_eq!(
        state.observability.selected_account.as_deref(),
        Some("claude:default")
    );
    state.observation_action(Action::CycleAccount, &mut ClientShellInput::default());
    assert_eq!(
        state.observability.selected_account.as_deref(),
        Some("codex:default")
    );
}

#[test]
fn accounts_page_auto_selects_the_focused_panes_provider_or_the_first_installed() {
    // 聚焦 pane 运行 claude：即使 codex 排在前面也选 claude。
    let mut state = usage_ready();
    deliver_providers(
        &mut state,
        vec![
            provider("codex", &["codex:default"]),
            provider("claude", &["claude:default"]),
        ],
    );
    state.open_observation_page(Page::Accounts, &mut ClientShellInput::default());
    assert_eq!(
        state.observability.selected_provider.as_deref(),
        Some("claude")
    );

    // 厂商列表晚于页面打开到达：到达时补选。
    let mut late = usage_ready();
    late.open_observation_page(Page::Accounts, &mut ClientShellInput::default());
    assert_eq!(late.observability.selected_provider, None);
    deliver_providers(
        &mut late,
        vec![
            provider("codex", &["codex:default"]),
            provider("claude", &["claude:default"]),
        ],
    );
    assert_eq!(
        late.observability.selected_provider.as_deref(),
        Some("claude")
    );

    // 没有 agent 在跑：选第一个已安装厂商，跳过未安装的。
    let mut plain = docked();
    plain.set_endpoint_methods(Some(vec![
        "client.views.set".into(),
        "tab.focus".into(),
        "account.usage.get".into(),
        "account.usage.providers".into(),
    ]));
    let mut missing = provider("kimi", &[]);
    missing.installed = Some(false);
    deliver_providers(
        &mut plain,
        vec![missing, provider("codex", &["codex:default"])],
    );
    plain.open_observation_page(Page::Accounts, &mut ClientShellInput::default());
    assert_eq!(
        plain.observability.selected_provider.as_deref(),
        Some("codex")
    );
}

#[test]
fn providers_are_refetched_on_page_open_and_every_five_minutes() {
    let mut state = usage_ready();
    state.open_observation_page(Page::Accounts, &mut ClientShellInput::default());
    let t0 = Instant::now() + Duration::from_secs(1);
    assert_eq!(providers_calls(&tick(&mut state, t0)), 1);
    deliver_providers(&mut state, vec![provider("claude", &["claude:default"])]);
    assert_eq!(
        providers_calls(&tick(&mut state, t0 + Duration::from_secs(2))),
        0,
        "已有厂商列表时不每 2 秒重拉"
    );
    assert_eq!(
        providers_calls(&tick(&mut state, t0 + Duration::from_secs(301))),
        1,
        "5 分钟后低频重拉"
    );
    deliver_providers(&mut state, vec![provider("claude", &["claude:default"])]);
    // 重新打开页面：立即重拉一次。
    state.open_observation_page(Page::Accounts, &mut ClientShellInput::default());
    assert_eq!(
        providers_calls(&tick(&mut state, t0 + Duration::from_secs(303))),
        1,
        "页面打开时重拉厂商列表"
    );
}

// ---------------------------------------------------------------------------
// 审查修复：跨端点绑定落点、绑定回流作用域、已关闭厂商、刷新中语义、
// 恢复路径自动选厂商、离线回退提示去重、悬浮层作用域随 hover 同生共死
// ---------------------------------------------------------------------------

/// 在帧里找第一处出现 `needle` 的 (x, y)。
fn find_text(frame: &FrameData, needle: &str) -> Option<(u16, u16)> {
    let width = frame.width as usize;
    frame.cells.chunks(width).enumerate().find_map(|(y, row)| {
        let text = row
            .iter()
            .map(|cell| cell.symbol.as_str())
            .collect::<String>();
        text.find(needle)
            .map(|byte| (text[..byte].chars().count() as u16, y as u16))
    })
}

fn frame_row(frame: &FrameData, y: u16) -> String {
    let width = frame.width as usize;
    frame
        .cells
        .chunks(width)
        .nth(usize::from(y))
        .map(|row| row.iter().map(|cell| cell.symbol.as_str()).collect())
        .unwrap_or_default()
}

fn cell_dimmed(frame: &FrameData, x: u16, y: u16) -> bool {
    let index = usize::from(y) * usize::from(frame.width) + usize::from(x);
    frame.cells[index].modifier & ratatui::style::Modifier::DIM.bits() != 0
}

/// 注册一个在线的远端端点：有快照（pane_1 运行 claude）与全部用量方法。
fn add_remote_usage_endpoint(state: &mut ClientShellState) -> ClientEndpointId {
    let profile = crate::client::endpoint::SavedSshEndpoint {
        id: crate::client::endpoint::ProfileId::parse("0123456789abcdef0123456789abcdef")
            .expect("固定 profile id"),
        label: "Build".into(),
        target: "dev@build.example".into(),
        session: "agents".into(),
        enabled: true,
        ..crate::client::endpoint::SavedSshEndpoint::new("base", "base", "default")
            .expect("valid base profile")
    };
    let remote = ClientEndpointId::Ssh(profile.id.clone());
    state.set_endpoint_catalog(&[profile]);
    state.set_endpoint_status(&remote, ClientEndpointStatus::Online);
    let mut projection = snapshot();
    projection.boot_id = "remote-boot".into();
    projection.agents.push(agent_in_pane("pane_1", "claude"));
    state.set_endpoint_snapshot(&remote, Box::new(projection));
    state.set_endpoint_methods_for(
        &remote,
        Some(vec![
            "account.usage.get".into(),
            "account.usage.refresh".into(),
            "account.usage.providers".into(),
            "account.binding.set".into(),
            "account.usage.integration".into(),
        ]),
    );
    remote
}

/// 直接进入「已停留满延时」的悬浮状态；下一次 tick 即可见。`endpoint_id` 为
/// `None` 时悬浮在活动端点上。
fn hover_on(
    state: &mut ClientShellState,
    endpoint_id: Option<ClientEndpointId>,
    pane: &str,
    agent: &str,
) {
    let endpoint_id = endpoint_id.unwrap_or_else(|| state.active_endpoint_id.clone());
    state.observability.hover = Some(Hover {
        target: HoverTarget::Agent {
            endpoint_id,
            pane: pane.into(),
            agent: agent.into(),
        },
        anchor: Rect::new(0, 20, 24, 2),
        since: Instant::now() - Duration::from_secs(1),
        visible: false,
        leave_at: None,
        pinned: false,
    });
}

/// 投递悬浮层作用域的用量响应（purpose 与请求一致：悬浮层已选厂商时是
/// `hover_usage:<agent>`）。
fn deliver_hover_usage(state: &mut ClientShellState, accounts: Vec<AccountUsageSnapshot>) -> bool {
    let epoch = state.observability.hover_scope.epoch;
    let agent = state.observability.hover_scope.provider.clone();
    state.receive_observation(
        epoch,
        Purpose::HoverUsage { agent },
        Ok(ResponseResult::AccountUsage {
            accounts,
            refresh: None,
        }),
    )
}

/// 本次 outcome 里发往各端点的 `account.binding.set`：`(endpoint, pane, account)`。
fn binding_calls(outcome: &ClientShellInput) -> Vec<(ClientEndpointId, String, String)> {
    outcome
        .actions
        .iter()
        .filter_map(|action| match action {
            ClientShellAction::Endpoint {
                endpoint_id,
                request,
                ..
            } => match &request.method {
                Method::AccountBindingSet(params) => Some((
                    endpoint_id.clone(),
                    params.pane_id.clone(),
                    params.account_id.clone(),
                )),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// 点击悬浮层里的某个动作按钮。
fn click_hover_action(
    state: &mut ClientShellState,
    matches: impl Fn(&Action) -> bool,
) -> ClientShellInput {
    state.compose(120, 40).expect("悬浮层上屏");
    let rect = state
        .observability
        .hover_hits
        .iter()
        .find(|(_, action)| matches(action))
        .map(|(rect, _)| *rect)
        .unwrap_or_else(|| panic!("悬浮层缺少按钮: {:?}", state.observability.hover_hits));
    click(state, rect.x, rect.y)
}

#[test]
fn hover_binding_on_a_remote_endpoint_targets_that_endpoint() {
    let mut state = usage_ready();
    // 官方回调开关只按服务端宣告的能力显示（`supports_callback`），先投递厂商列表。
    deliver_providers(&mut state, vec![provider("claude", &["claude:default"])]);
    let remote = add_remote_usage_endpoint(&mut state);
    assert_ne!(remote, state.active_endpoint_id);
    hover_on(&mut state, Some(remote.clone()), "pane_1", "claude");
    let shown = tick(&mut state, Instant::now() + Duration::from_secs(1));
    let targets = shown
        .actions
        .iter()
        .filter_map(|action| match action {
            ClientShellAction::Endpoint {
                endpoint_id,
                request,
                ..
            } if matches!(request.method, Method::AccountUsageRefresh(_)) => {
                Some(endpoint_id.clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        targets,
        vec![remote.clone()],
        "悬浮层用量请求发往 hover 的端点"
    );
    assert!(deliver_hover_usage(
        &mut state,
        vec![account("claude", "claude:default")]
    ));

    // 悬浮层里点「绑定账号」：绑定请求也必须落到 hover 的端点，而不是活动端点。
    let outcome = click_hover_action(&mut state, |action| matches!(action, Action::Bind));
    assert_eq!(
        binding_calls(&outcome),
        vec![(
            remote.clone(),
            "pane_1".to_owned(),
            "claude:default".to_owned()
        )],
        "远端 pane 的绑定发往远端端点"
    );

    // 「启用官方回调」同理。
    let outcome = click_hover_action(&mut state, |action| {
        matches!(action, Action::UsageIntegration(true))
    });
    let integration_targets = outcome
        .actions
        .iter()
        .filter_map(|action| match action {
            ClientShellAction::Endpoint {
                endpoint_id,
                request,
                ..
            } if matches!(request.method, Method::AccountUsageIntegration(_)) => {
                Some(endpoint_id.clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        integration_targets,
        vec![remote],
        "官方回调也发往 hover 的端点"
    );
}

#[test]
fn hover_binding_does_not_rewrite_the_accounts_page_selection() {
    let mut state = usage_ready();
    deliver_providers(
        &mut state,
        vec![
            provider("claude", &["claude:default"]),
            provider("codex", &["codex:default"]),
        ],
    );
    state.open_observation_page(Page::Accounts, &mut ClientShellInput::default());
    assert_eq!(
        state.observability.selected_provider.as_deref(),
        Some("claude")
    );
    let t0 = Instant::now() + Duration::from_secs(1);
    assert_eq!(usage_calls(&tick(&mut state, t0)).len(), 1);
    assert!(deliver_usage(
        &mut state,
        vec![account("claude", "claude:default")]
    ));
    let page_accounts = state.observability.accounts.clone();

    // 悬浮一个 codex pane 并在悬浮层里绑定。
    add_agent_pane(&mut state, "pane_2", "codex");
    hover_on(&mut state, None, "pane_2", "codex");
    tick(&mut state, t0 + Duration::from_millis(10));
    assert!(deliver_hover_usage(
        &mut state,
        vec![account("codex", "codex:default")]
    ));
    let outcome = click_hover_action(&mut state, |action| matches!(action, Action::Bind));
    let calls = binding_calls(&outcome);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1, "pane_2");
    assert_eq!(calls[0].2, "codex:default");
    let hover_epoch = state.observability.hover_scope.epoch;
    assert!(state.receive_observation(
        hover_epoch,
        Purpose::HoverBinding,
        Ok(ResponseResult::AccountBinding {
            pane_id: "pane_2".into(),
            account_id: "codex:default".into(),
        }),
    ));

    // 页面作用域纹丝不动：仍是 claude、未选账号、账号列表未变。
    assert_eq!(
        state.observability.selected_provider.as_deref(),
        Some("claude"),
        "悬浮层绑定不改写页面厂商"
    );
    assert_eq!(
        state.observability.selected_account, None,
        "悬浮层绑定不把 codex 账号写进 claude 页面（否则服务端按 agent+account 双重过滤返回空）"
    );
    assert_eq!(state.observability.accounts, page_accounts);
    assert!(!state.observability.refreshing(), "页面没有被拖进刷新中");
    // 悬浮层自己立刻重查（强意图刷新），页面轮询到期时仍按 claude 查询。
    let calls = usage_calls(&tick(&mut state, t0 + Duration::from_millis(20)));
    assert_eq!(calls.len(), 1, "只有悬浮层重查: {calls:?}");
    assert!(calls[0].0, "绑定回流 = 悬浮层强意图刷新");
    assert_eq!(calls[0].1.pane_id.as_deref(), Some("pane_2"));
    assert!(deliver_hover_usage(
        &mut state,
        vec![account("codex", "codex:default")]
    ));
    let calls = usage_calls(&tick(&mut state, t0 + Duration::from_secs(2)));
    let page_call = calls
        .iter()
        .find(|(_, params)| params.pane_id.is_none())
        .expect("页面轮询");
    assert_eq!(page_call.1.agent.as_deref(), Some("claude"));
    assert_eq!(page_call.1.account_id, None);
}

#[test]
fn selecting_a_disabled_provider_does_not_leave_the_page_stuck_refreshing() {
    let mut state = usage_ready();
    state.observability.usage.disabled_providers = vec!["codex".into()];
    deliver_providers(
        &mut state,
        vec![
            provider("claude", &["claude:default"]),
            provider("codex", &["codex:default"]),
        ],
    );
    state.open_observation_page(Page::Accounts, &mut ClientShellInput::default());
    let t0 = Instant::now() + Duration::from_secs(1);
    assert_eq!(usage_calls(&tick(&mut state, t0)).len(), 1);
    assert!(deliver_usage(
        &mut state,
        vec![account("claude", "claude:default")]
    ));
    state.compose(120, 40).expect("账号页");
    assert!(
        !state
            .observability
            .hits
            .iter()
            .any(|(_, action)| matches!(action, Action::Provider(agent) if agent == "codex")),
        "设置里关掉的厂商不出现在厂商侧栏 / 选择器里"
    );
    assert!(
        !state
            .observability
            .cycle_candidates(None)
            .any(|id| id == "codex:default"),
        "设置里关掉的厂商不出现在「切换账号」候选里"
    );

    // 即便被选中（例如先选中再去设置里关掉），也不能把页面卡在「刷新中…」。
    state.observation_action(
        Action::Provider("codex".into()),
        &mut ClientShellInput::default(),
    );
    assert!(state.observability.refreshing(), "选中即排队强意图刷新");
    assert!(
        usage_calls(&tick(&mut state, t0 + Duration::from_millis(10))).is_empty(),
        "已关闭的厂商不发请求"
    );
    assert!(
        !state.observability.refreshing(),
        "无请求可发时不能留下永久的刷新中"
    );
    assert!(
        state
            .observability
            .message
            .as_deref()
            .is_some_and(|message| message.contains("关闭") || message.contains("disabled")),
        "页脚说明厂商已在设置中关闭: {:?}",
        state.observability.message
    );
    state.compose(120, 40).expect("账号页");
    assert!(
        state
            .observability
            .hits
            .iter()
            .any(|(_, action)| matches!(action, Action::Refresh)),
        "「刷新」按钮仍有命中区，用户能原地自救"
    );

    // 聚焦 pane 的 agent 被关掉时自动选中跳过它。
    let mut plain = usage_ready();
    plain.observability.usage.disabled_providers = vec!["claude".into()];
    deliver_providers(
        &mut plain,
        vec![
            provider("claude", &["claude:default"]),
            provider("codex", &["codex:default"]),
        ],
    );
    plain.open_observation_page(Page::Accounts, &mut ClientShellInput::default());
    assert_eq!(
        plain.observability.selected_provider.as_deref(),
        Some("codex"),
        "自动选中跳过已关闭的厂商"
    );
}

#[test]
fn restoring_the_accounts_tab_auto_selects_a_provider_on_tick() {
    let mut state = usage_ready();
    deliver_providers(
        &mut state,
        vec![
            provider("codex", &["codex:default"]),
            provider("claude", &["claude:default"]),
        ],
    );
    // 模拟偏好恢复：上次停在账号 tab，Monitor 面板随布局恢复可见，
    // 全程不经过 open_observation_page。
    state.observability.monitor_tab = Page::Accounts;
    state.workbench_open(PanelId::Monitor);
    state.workbench.dock.focused = PanelId::Terminal(1);
    state.sync_observation_page_with_focus();
    state.compose(120, 40).expect("面板随布局恢复上屏");
    assert!(state.workbench.visible(&PanelId::Monitor));
    assert_eq!(state.observability.page, None);
    assert_eq!(state.observability.selected_provider, None);
    let calls = usage_calls(&tick(&mut state, Instant::now() + Duration::from_secs(1)));
    assert_eq!(
        state.observability.selected_provider.as_deref(),
        Some("claude"),
        "账号页可见即按聚焦 pane 自动选中厂商"
    );
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1.agent.as_deref(), Some("claude"));
}

#[test]
fn a_plain_poll_response_does_not_clear_a_queued_manual_refresh() {
    let mut state = usage_ready();
    deliver_providers(&mut state, vec![provider("claude", &["claude:default"])]);
    state.open_observation_page(Page::Accounts, &mut ClientShellInput::default());
    let t0 = Instant::now() + Duration::from_secs(1);
    assert_eq!(usage_calls(&tick(&mut state, t0)).len(), 1);
    assert!(deliver_usage(
        &mut state,
        vec![account("claude", "claude:default")]
    ));
    assert!(!state.observability.refreshing());
    // 普通轮询在途。
    let polled = usage_calls(&tick(&mut state, t0 + Duration::from_secs(2)));
    assert_eq!(polled.len(), 1);
    assert!(!polled[0].0, "到期轮询是普通 get");
    // 在途期间点「刷新」：排队并进入刷新中。
    state.observation_action(Action::Refresh, &mut ClientShellInput::default());
    assert!(state.observability.refreshing());
    // 普通轮询的响应到达：手动刷新仍在排队，不能提前去暗。
    assert!(deliver_usage(
        &mut state,
        vec![account("claude", "claude:default")]
    ));
    assert!(
        state.observability.refreshing(),
        "排队中的手动刷新未发出，不能因普通轮询返回就清除刷新中"
    );
    let retried = usage_calls(&tick(&mut state, t0 + Duration::from_millis(2260)));
    assert_eq!(retried.len(), 1);
    assert!(retried[0].0, "重试发出的是 account.usage.refresh");
    assert!(
        state.observability.refreshing(),
        "手动刷新在途期间仍是刷新中"
    );
    assert!(deliver_usage(
        &mut state,
        vec![account("claude", "claude:default")]
    ));
    assert!(
        !state.observability.refreshing(),
        "手动刷新的响应到达后才清除"
    );
}

#[test]
fn missing_refresh_method_falls_back_to_get_instead_of_stalling() {
    let mut state = usage_ready();
    // generation-1 server 只宣告 account.usage.get：手动刷新不可用，但轮询不能停摆。
    state.set_endpoint_methods(Some(vec![
        "client.views.set".into(),
        "tab.focus".into(),
        "account.usage.get".into(),
        "account.usage.providers".into(),
    ]));
    deliver_providers(&mut state, vec![provider("claude", &["claude:default"])]);
    state.open_observation_page(Page::Accounts, &mut ClientShellInput::default());
    assert!(state.observability.refreshing(), "自动选中排队了强意图刷新");
    let t0 = Instant::now() + Duration::from_secs(1);
    let calls = usage_calls(&tick(&mut state, t0));
    assert_eq!(
        calls.len(),
        1,
        "refresh 不可用时同一轮回落到 get: {calls:?}"
    );
    assert!(!calls[0].0, "回落发出的是 account.usage.get");
    assert!(
        !state.observability.refreshing(),
        "无法发出的强意图不能留下永久的刷新中"
    );
    assert!(deliver_usage(
        &mut state,
        vec![account("claude", "claude:default")]
    ));
    let calls = usage_calls(&tick(&mut state, t0 + Duration::from_secs(2)));
    assert_eq!(calls.len(), 1, "后续轮询继续用 get");
    assert!(!calls[0].0);
}

#[test]
fn hover_shows_refreshing_until_its_own_response_arrives() {
    let mut state = usage_ready();
    deliver_providers(&mut state, vec![provider("claude", &["claude:default"])]);
    state.open_observation_page(Page::Accounts, &mut ClientShellInput::default());
    let t0 = Instant::now() + Duration::from_secs(1);
    assert_eq!(usage_calls(&tick(&mut state, t0)).len(), 1);
    assert!(deliver_usage(
        &mut state,
        vec![account("claude", "claude:default")]
    ));
    hover_on(&mut state, None, "pane_1", "claude");
    let shown = usage_calls(&tick(&mut state, t0 + Duration::from_millis(450)));
    assert_eq!(
        shown.len(),
        1,
        "只发悬浮层请求，页面轮询不被提前唤醒: {shown:?}"
    );
    assert_eq!(shown[0].1.pane_id.as_deref(), Some("pane_1"));
    assert!(
        state.observability.hover_scope.refreshing(),
        "悬浮层强意图刷新在途 = 刷新中"
    );
    state.compose(120, 40).expect("悬浮层");
    assert!(
        !state
            .observability
            .hover_hits
            .iter()
            .any(|(_, action)| matches!(action, Action::Refresh)),
        "悬浮层刷新中时「刷新」退化为状态提示"
    );
    assert!(deliver_hover_usage(
        &mut state,
        vec![account("claude", "claude:default")]
    ));
    assert!(!state.observability.hover_scope.refreshing());
    state.compose(120, 40).expect("悬浮层");
    assert!(
        state
            .observability
            .hover_hits
            .iter()
            .any(|(_, action)| matches!(action, Action::Refresh)),
        "响应到达后悬浮层「刷新」按钮恢复"
    );
    // 悬浮层里点「刷新」只唤醒悬浮层：页面的 2 秒节流不受影响。
    let outcome = click_hover_action(&mut state, |action| matches!(action, Action::Refresh));
    assert!(usage_calls(&outcome).is_empty());
    let calls = usage_calls(&tick(&mut state, t0 + Duration::from_millis(500)));
    assert_eq!(calls.len(), 1, "只有悬浮层重查: {calls:?}");
    assert!(calls[0].0);
    assert_eq!(calls[0].1.pane_id.as_deref(), Some("pane_1"));
}

#[test]
fn offline_fallback_note_is_written_once_per_target() {
    let mut state = usage_ready();
    let ghost = ClientEndpointId::Ssh(crate::client::endpoint::ProfileId::generate());
    hover_on(&mut state, Some(ghost), "pane_1", "claude");
    let t0 = Instant::now() + Duration::from_secs(1);
    assert_eq!(usage_calls(&tick(&mut state, t0)).len(), 1);
    assert!(state.observability.message.is_some(), "首次回退在页脚说明");
    assert!(deliver_hover_usage(&mut state, Vec::new()));
    // 其它一次性提示（例如禁用态说明）不能被每 2 秒一次的回退说明刷掉。
    state.observability.message = Some("no other account".into());
    assert_eq!(
        usage_calls(&tick(&mut state, t0 + Duration::from_secs(2))).len(),
        1,
        "悬浮层继续轮询"
    );
    assert_eq!(
        state.observability.message.as_deref(),
        Some("no other account"),
        "回退目标未变化时不重写页脚"
    );
}

#[test]
fn clicking_outside_the_hover_resets_its_scope_too() {
    let mut state = usage_ready();
    state.workbench_open(PanelId::Monitor);
    hover_on(&mut state, None, "pane_1", "claude");
    tick(&mut state, Instant::now() + Duration::from_secs(1));
    assert_eq!(
        state.observability.hover_scope.pane.as_deref(),
        Some("pane_1")
    );
    let hover_epoch = state.observability.hover_scope.epoch;
    state.compose(120, 40).expect("悬浮层");
    let terminal = terminal_body(&state);
    let point = (terminal.x + 1, terminal.y + 1);
    assert!(!contains(state.observability.hover_rect, point));
    click(&mut state, point.0, point.1);
    assert!(state.observability.hover.is_none(), "浮层外按下即结束悬浮");
    assert_eq!(
        state.observability.hover_scope.pane, None,
        "悬浮层作用域随 hover 一起复位"
    );
    assert_ne!(
        state.observability.hover_scope.epoch, hover_epoch,
        "在途的悬浮层响应按新代际丢弃"
    );
    assert!(
        !state.receive_observation(
            hover_epoch,
            Purpose::HoverUsage { agent: None },
            Ok(ResponseResult::AccountUsage {
                accounts: vec![account("claude", "claude:default")],
                refresh: None,
            }),
        ),
        "旧代际的悬浮层响应被丢弃"
    );
    assert!(state.observability.hover_scope.accounts.is_empty());
}

// ---------------------------------------------------------------------------
// B-2 客户端半：消费 UsageRefreshState（自适应轮询、刷新中 / N 秒后可刷新、
// 目录信任与推断绑定文案）；B-12 客户端半：订阅用量事件；B-3 客户端：绑定入口
// ---------------------------------------------------------------------------

use crate::api::schema::{
    AccountUsageRefreshingEvent, AccountUsageUpdatedEvent, ObservationEventEnvelope,
    UsagePendingBinding, UsageRefreshState,
};
use crate::protocol::endpoint::EndpointObservationEvent;

fn refresh_state(account_id: &str) -> UsageRefreshState {
    UsageRefreshState {
        account_id: account_id.into(),
        ..Default::default()
    }
}

fn deliver_usage_with_refresh(
    state: &mut ClientShellState,
    accounts: Vec<AccountUsageSnapshot>,
    refresh: Vec<UsageRefreshState>,
) -> bool {
    let epoch = state.observability.epoch;
    let agent = state.observability.selected_provider.clone();
    state.receive_observation(
        epoch,
        Purpose::Usage { agent },
        Ok(ResponseResult::AccountUsage {
            accounts,
            refresh: Some(refresh),
        }),
    )
}

/// 停靠工作台 + 全部用量方法（含订阅）+ 聚焦 pane 运行 claude 的快照。
fn subscribing_ready() -> ClientShellState {
    let mut state = usage_ready();
    state.set_endpoint_methods(Some(vec![
        "client.views.set".into(),
        "tab.focus".into(),
        "pane.focus".into(),
        "account.usage.get".into(),
        "account.usage.refresh".into(),
        "account.usage.providers".into(),
        "account.usage.subscribe".into(),
        "account.usage.unsubscribe".into(),
        "account.usage.integration".into(),
        "account.binding.set".into(),
    ]));
    state
}

fn subscribe_calls(outcome: &ClientShellInput) -> Vec<UsageParams> {
    outcome
        .actions
        .iter()
        .filter_map(|action| match action {
            ClientShellAction::Endpoint { request, .. } => match &request.method {
                Method::AccountUsageSubscribe(params) => Some(params.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

fn unsubscribe_calls(outcome: &ClientShellInput) -> Vec<Option<String>> {
    outcome
        .actions
        .iter()
        .filter_map(|action| match action {
            ClientShellAction::Endpoint { request, .. } => match &request.method {
                Method::AccountUsageUnsubscribe(params) => Some(params.subscription_id.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

fn integration_calls(outcome: &ClientShellInput) -> Vec<(String, bool)> {
    outcome
        .actions
        .iter()
        .filter_map(|action| match action {
            ClientShellAction::Endpoint { request, .. } => match &request.method {
                Method::AccountUsageIntegration(params) => {
                    Some((params.account_id.clone(), params.enabled))
                }
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// 订阅生命周期不绑页面代际（代际恒为 0），与 `observation_request` 存入
/// `PendingEndpointKind::Observation` 的值一致。
fn deliver_subscription(state: &mut ClientShellState, id: &str, active: bool) -> bool {
    state.receive_observation(
        0,
        Purpose::Subscribe,
        Ok(ResponseResult::ObservationSubscription {
            subscription_id: id.into(),
            active,
        }),
    )
}

/// 模拟活动端点推来的 `endpoint.observation.v1` 控制帧。
fn push_event(
    state: &mut ClientShellState,
    boot_id: &str,
    event: ObservationEventEnvelope,
) -> bool {
    let endpoint_id = state.active_endpoint_id.clone();
    state.receive_endpoint_observation_event(
        &endpoint_id,
        0,
        EndpointObservationEvent {
            boot_id: boot_id.into(),
            event,
        },
    )
}

fn updated_event(
    accounts: Vec<AccountUsageSnapshot>,
    refresh: Vec<UsageRefreshState>,
) -> ObservationEventEnvelope {
    ObservationEventEnvelope::AccountUsageUpdated(AccountUsageUpdatedEvent {
        accounts,
        refresh: Some(refresh),
    })
}

/// 帧里是否出现某段文字：宽字符占两个 cell、第二个 cell 是空格，所以两边都去掉
/// 空格后比较（`find_text` 只适合 ASCII）。
fn frame_has(frame: &FrameData, needle: &str) -> bool {
    let needle = needle.replace(' ', "");
    (0..frame.height).any(|y| frame_row(frame, y).replace(' ', "").contains(&needle))
}

fn page_hit(state: &ClientShellState, matches: impl Fn(&Action) -> bool) -> Option<Rect> {
    state
        .observability
        .hits
        .iter()
        .find(|(_, action)| matches(action))
        .map(|(rect, _)| *rect)
}

#[test]
fn in_flight_refresh_state_tightens_polling_to_half_a_second() {
    let mut state = usage_ready();
    deliver_providers(&mut state, vec![provider("claude", &["claude:default"])]);
    state.open_observation_page(Page::Accounts, &mut ClientShellInput::default());
    let t0 = Instant::now() + Duration::from_secs(1);
    assert_eq!(usage_calls(&tick(&mut state, t0)).len(), 1);
    // 服务端说探测在途：轮询收紧到 500 ms。
    let mut in_flight = refresh_state("claude:default");
    in_flight.in_flight = true;
    assert!(deliver_usage_with_refresh(
        &mut state,
        vec![account("claude", "claude:default")],
        vec![in_flight],
    ));
    assert!(
        usage_calls(&tick(&mut state, t0 + Duration::from_millis(300))).is_empty(),
        "300 ms 内仍在节流窗口"
    );
    assert_eq!(
        usage_calls(&tick(&mut state, t0 + Duration::from_millis(600))).len(),
        1,
        "探测在途时 500 ms 轮询一次"
    );
    // 探测结束：回到 2 秒。
    assert!(deliver_usage_with_refresh(
        &mut state,
        vec![account("claude", "claude:default")],
        vec![refresh_state("claude:default")],
    ));
    assert!(
        usage_calls(&tick(&mut state, t0 + Duration::from_millis(1200))).is_empty(),
        "探测结束后恢复 2 秒节流"
    );
    assert_eq!(
        usage_calls(&tick(&mut state, t0 + Duration::from_millis(2700))).len(),
        1
    );
    // 旧 server 不带 refresh 字段：保持 2 秒。
    assert!(deliver_usage(
        &mut state,
        vec![account("claude", "claude:default")]
    ));
    assert!(usage_calls(&tick(&mut state, t0 + Duration::from_millis(3300))).is_empty());
    assert_eq!(
        usage_calls(&tick(&mut state, t0 + Duration::from_millis(4800))).len(),
        1
    );
}

#[test]
fn hover_polling_also_follows_the_in_flight_refresh_state() {
    let mut state = usage_ready();
    hover_on(&mut state, None, "pane_1", "claude");
    let t0 = Instant::now() + Duration::from_secs(1);
    assert_eq!(usage_calls(&tick(&mut state, t0)).len(), 1);
    let mut in_flight = refresh_state("claude:default");
    in_flight.in_flight = true;
    let hover_epoch = state.observability.hover_scope.epoch;
    // 响应的 purpose 与请求一致：悬浮层已选 claude，请求键是 hover_usage:claude。
    assert!(state.receive_observation(
        hover_epoch,
        Purpose::HoverUsage {
            agent: Some("claude".into()),
        },
        Ok(ResponseResult::AccountUsage {
            accounts: vec![account("claude", "claude:default")],
            refresh: Some(vec![in_flight]),
        }),
    ));
    assert_eq!(
        usage_calls(&tick(&mut state, t0 + Duration::from_millis(600))).len(),
        1,
        "悬浮层在探测在途时也 500 ms 轮询"
    );
    assert!(
        state.observability.accounts.is_empty(),
        "悬浮层的刷新状态不写页面"
    );
}

#[test]
fn server_side_refresh_progress_and_debounce_are_shown_on_the_page() {
    let mut state = usage_ready();
    deliver_providers(&mut state, vec![provider("claude", &["claude:default"])]);
    state.open_observation_page(Page::Accounts, &mut ClientShellInput::default());
    let t0 = Instant::now() + Duration::from_secs(1);
    assert_eq!(usage_calls(&tick(&mut state, t0)).len(), 1);
    // 服务端探测在途（例如订阅驱动的后台探测）：标题行显示「刷新中…」，但
    // 「刷新」按钮保留命中区（订阅期间在途可能长期为真，不能锁掉逃生口），
    // 旧数据也不变暗（这不是用户的强意图刷新）。
    let mut in_flight = refresh_state("claude:default");
    in_flight.in_flight = true;
    assert!(deliver_usage_with_refresh(
        &mut state,
        vec![account("claude", "claude:default")],
        vec![in_flight],
    ));
    assert!(
        !state.observability.refreshing(),
        "服务端在途不是本地强意图"
    );
    let frame = state.compose(120, 40).expect("账号页");
    assert!(
        frame_has(&frame, "刷新中") || frame_has(&frame, "Refreshing"),
        "服务端探测在途时页面显示刷新中"
    );
    assert!(
        page_hit(&state, |action| matches!(action, Action::Refresh)).is_some(),
        "服务端探测在途不锁「刷新」按钮"
    );
    let (x, y) = find_text(&frame, "claude:default").expect("账号行");
    assert!(!cell_dimmed(&frame, x, y), "服务端在途不把旧数据变暗");

    // 显式刷新被防抖：响应带 next_allowed_at_ms，页面显示「N 秒后可刷新」。
    let mut debounced = refresh_state("claude:default");
    debounced.next_allowed_at_ms = Some(state.observability.now_ms + 7_000);
    assert!(deliver_usage_with_refresh(
        &mut state,
        vec![account("claude", "claude:default")],
        vec![debounced],
    ));
    let frame = state.compose(120, 40).expect("账号页");
    assert!(
        frame_has(&frame, "秒后可刷新") || frame_has(&frame, "Refresh in"),
        "防抖期间提示 N 秒后可刷新: {}",
        frame_row(&frame, 38)
    );
    assert!(page_hit(&state, |action| matches!(action, Action::Refresh)).is_none());

    // 防抖到期：按钮恢复。
    let mut ready = refresh_state("claude:default");
    ready.next_allowed_at_ms = Some(state.observability.now_ms.saturating_sub(1));
    assert!(deliver_usage_with_refresh(
        &mut state,
        vec![account("claude", "claude:default")],
        vec![ready],
    ));
    state.compose(120, 40).expect("账号页");
    assert!(
        page_hit(&state, |action| matches!(action, Action::Refresh)).is_some(),
        "防抖到期后「刷新」按钮恢复"
    );
}

#[test]
fn trust_required_and_inferred_binding_are_explained_per_account() {
    let mut state = usage_ready();
    deliver_providers(&mut state, vec![provider("claude", &["claude:default"])]);
    state.open_observation_page(Page::Accounts, &mut ClientShellInput::default());
    tick(&mut state, Instant::now() + Duration::from_secs(1));
    let mut trust = refresh_state("claude:default");
    trust.trust_required = true;
    trust.binding_inferred = true;
    assert!(deliver_usage_with_refresh(
        &mut state,
        vec![account("claude", "claude:default")],
        vec![trust],
    ));
    let frame = state.compose(120, 40).expect("账号页");
    assert!(
        frame_has(&frame, "目录信任") || frame_has(&frame, "trust"),
        "trust_required 有文案"
    );
    assert!(
        frame_has(&frame, "推断") || frame_has(&frame, "inferred"),
        "binding_inferred 有标记"
    );
}

#[test]
fn visible_accounts_page_subscribes_and_stops_polling_until_hidden() {
    let mut state = subscribing_ready();
    deliver_providers(&mut state, vec![provider("claude", &["claude:default"])]);
    state.open_observation_page(Page::Accounts, &mut ClientShellInput::default());
    let t0 = Instant::now() + Duration::from_secs(1);
    let opened = tick(&mut state, t0);
    assert_eq!(usage_calls(&opened).len(), 1, "冷启动一次 get/refresh");
    let subscribed = subscribe_calls(&opened);
    assert_eq!(
        subscribed.len(),
        1,
        "账号页可见即订阅: {:?}",
        opened.actions
    );
    assert_eq!(subscribed[0].agent.as_deref(), Some("claude"));
    assert_eq!(subscribed[0].pane_id, None, "页面订阅不带 pane");
    assert!(deliver_subscription(&mut state, "usage-1", true));
    assert!(deliver_usage(
        &mut state,
        vec![account("claude", "claude:default")]
    ));
    assert!(
        usage_calls(&tick(&mut state, t0 + Duration::from_secs(2))).is_empty(),
        "订阅生效后不再 2 秒轮询"
    );
    assert!(
        subscribe_calls(&tick(&mut state, t0 + Duration::from_secs(3))).is_empty(),
        "已订阅不重复订阅"
    );
    assert_eq!(
        usage_calls(&tick(&mut state, t0 + Duration::from_secs(31))).len(),
        1,
        "订阅期间保留一条低频兜底轮询"
    );
    assert!(deliver_usage(
        &mut state,
        vec![account("claude", "claude:default")]
    ));

    // 事件按 account_id 合并到页面账号，并立即重绘。
    let mut updated = account("claude", "claude:default");
    updated.plan = Some("max".into());
    assert!(push_event(
        &mut state,
        "boot-1",
        updated_event(vec![updated], vec![refresh_state("claude:default")]),
    ));
    assert_eq!(state.observability.accounts.len(), 1);
    assert_eq!(state.observability.accounts[0].plan.as_deref(), Some("max"));

    // 页面隐藏：退订。
    state.observation_action(Action::Close, &mut ClientShellInput::default());
    let closed = tick(&mut state, t0 + Duration::from_secs(32));
    assert_eq!(
        unsubscribe_calls(&closed),
        vec![Some("usage-1".to_owned())],
        "页面不可见时退订"
    );
    assert!(subscribe_calls(&closed).is_empty());
    deliver_unsubscribed(&mut state, "usage-1");

    // 再次可见（面板随布局恢复，不经过 open_observation_page）：同一 tick 内
    // 冷启动一次 get 并重新订阅，而不是等 30 s 兜底轮询。
    state.workbench_open(PanelId::Monitor);
    let reopened = tick(&mut state, t0 + Duration::from_secs(33));
    assert_eq!(usage_calls(&reopened).len(), 1, "重新可见时冷启动一次 get");
    assert_eq!(subscribe_calls(&reopened).len(), 1, "重新可见时重新订阅");
}

#[test]
fn switching_the_page_scope_resubscribes_with_the_new_filter() {
    let mut state = subscribing_ready();
    deliver_providers(
        &mut state,
        vec![
            provider("claude", &["claude:default"]),
            provider("codex", &["codex:default"]),
        ],
    );
    state.open_observation_page(Page::Accounts, &mut ClientShellInput::default());
    let t0 = Instant::now() + Duration::from_secs(1);
    assert_eq!(subscribe_calls(&tick(&mut state, t0)).len(), 1);
    assert!(deliver_subscription(&mut state, "usage-1", true));
    state.observation_action(
        Action::Provider("codex".into()),
        &mut ClientShellInput::default(),
    );
    let switched = tick(&mut state, t0 + Duration::from_millis(10));
    assert_eq!(
        unsubscribe_calls(&switched),
        vec![Some("usage-1".to_owned())],
        "切换厂商先退掉旧订阅"
    );
    let resubscribed = subscribe_calls(&switched);
    assert_eq!(resubscribed.len(), 1, "再按新厂商订阅");
    assert_eq!(resubscribed[0].agent.as_deref(), Some("codex"));
    // 旧订阅的事件（claude）到达：不属于当前页面作用域，丢弃。
    assert!(deliver_subscription(&mut state, "usage-2", true));
    push_event(
        &mut state,
        "boot-1",
        updated_event(vec![account("claude", "claude:default")], Vec::new()),
    );
    assert!(
        state
            .observability
            .accounts
            .iter()
            .all(|account| account.agent == "codex"),
        "不属于当前厂商的事件不写入页面: {:?}",
        state.observability.accounts
    );
}

#[test]
fn rejected_subscription_falls_back_to_polling_and_retries_later() {
    let mut state = subscribing_ready();
    deliver_providers(&mut state, vec![provider("claude", &["claude:default"])]);
    state.open_observation_page(Page::Accounts, &mut ClientShellInput::default());
    let t0 = Instant::now() + Duration::from_secs(1);
    assert_eq!(subscribe_calls(&tick(&mut state, t0)).len(), 1);
    assert!(deliver_usage(
        &mut state,
        vec![account("claude", "claude:default")]
    ));
    state.receive_observation(
        0,
        Purpose::Subscribe,
        Err(ClientShellEndpointError {
            code: Some("subscription_limit".into()),
            message: "too many".into(),
        }),
    );
    let polled = usage_calls(&tick(&mut state, t0 + Duration::from_secs(2)));
    assert_eq!(polled.len(), 1, "订阅被拒后回退到轮询");
    assert!(
        subscribe_calls(&tick(&mut state, t0 + Duration::from_secs(3))).is_empty(),
        "被拒后不立即重试订阅"
    );
    assert!(deliver_usage(
        &mut state,
        vec![account("claude", "claude:default")]
    ));
    assert_eq!(
        subscribe_calls(&tick(&mut state, t0 + Duration::from_secs(40))).len(),
        1,
        "退避到期后重新尝试订阅"
    );
}

#[test]
fn usage_events_from_another_boot_are_dropped() {
    let mut state = subscribing_ready();
    deliver_providers(&mut state, vec![provider("claude", &["claude:default"])]);
    state.open_observation_page(Page::Accounts, &mut ClientShellInput::default());
    tick(&mut state, Instant::now() + Duration::from_secs(1));
    assert!(!push_event(
        &mut state,
        "boot-old",
        updated_event(vec![account("claude", "claude:default")], Vec::new()),
    ));
    assert!(
        state.observability.accounts.is_empty(),
        "旧 boot 的事件丢弃"
    );
}

#[test]
fn refreshing_event_marks_the_account_in_flight_without_dimming() {
    let mut state = subscribing_ready();
    deliver_providers(&mut state, vec![provider("claude", &["claude:default"])]);
    state.open_observation_page(Page::Accounts, &mut ClientShellInput::default());
    tick(&mut state, Instant::now() + Duration::from_secs(1));
    assert!(deliver_usage_with_refresh(
        &mut state,
        vec![account("claude", "claude:default")],
        vec![refresh_state("claude:default")],
    ));
    let mut in_flight = refresh_state("claude:default");
    in_flight.in_flight = true;
    assert!(push_event(
        &mut state,
        "boot-1",
        ObservationEventEnvelope::AccountUsageRefreshing(AccountUsageRefreshingEvent {
            refresh: vec![in_flight],
        }),
    ));
    let frame = state.compose(120, 40).expect("账号页");
    assert!(
        frame_has(&frame, "刷新中") || frame_has(&frame, "Refreshing"),
        "refreshing 事件让页面显示刷新中"
    );
    assert!(
        page_hit(&state, |action| matches!(action, Action::Refresh)).is_some(),
        "服务端在途不锁「刷新」按钮"
    );
    // updated 事件收尾：不再刷新中。
    assert!(push_event(
        &mut state,
        "boot-1",
        updated_event(
            vec![account("claude", "claude:default")],
            vec![refresh_state("claude:default")],
        ),
    ));
    let frame = state.compose(120, 40).expect("账号页");
    assert!(
        !(frame_has(&frame, "刷新中") || frame_has(&frame, "Refreshing")),
        "updated 事件收尾后不再显示刷新中"
    );
    assert!(page_hit(&state, |action| matches!(action, Action::Refresh)).is_some());
}

#[test]
fn accounts_page_binds_to_the_focused_pane_from_the_page() {
    let mut state = usage_ready();
    deliver_providers(&mut state, vec![provider("claude", &["claude:default"])]);
    state.open_observation_page(Page::Accounts, &mut ClientShellInput::default());
    let t0 = Instant::now() + Duration::from_secs(1);
    tick(&mut state, t0);
    assert!(deliver_usage(
        &mut state,
        vec![account("claude", "claude:default")]
    ));
    state.compose(120, 40).expect("账号页");
    let rect = page_hit(&state, |action| matches!(action, Action::BindFocused))
        .expect("账号页提供「绑定到聚焦 pane」入口");
    let outcome = click(&mut state, rect.x, rect.y);
    let calls = binding_calls(&outcome)
        .into_iter()
        .chain(binding_calls(&tick(
            &mut state,
            t0 + Duration::from_millis(10),
        )))
        .collect::<Vec<_>>();
    assert_eq!(
        calls,
        vec![(
            state.active_endpoint_id.clone(),
            "pane_1".to_owned(),
            "claude:default".to_owned()
        )],
        "聚焦 pane 运行 claude，直接绑定到唯一账号"
    );
}

#[test]
fn pane_picker_cycles_through_panes_running_the_selected_provider() {
    let mut snapshot = snapshot();
    snapshot.agents.push(agent_in_pane("pane_1", "claude"));
    snapshot.agents.push(agent_in_pane("pane_2", "codex"));
    snapshot.agents.push(agent_in_pane("pane_3", "claude"));
    let mut state = docked_with(snapshot);
    state.set_endpoint_methods(Some(vec![
        "client.views.set".into(),
        "tab.focus".into(),
        "account.usage.get".into(),
        "account.usage.refresh".into(),
        "account.usage.providers".into(),
        "account.binding.set".into(),
    ]));
    let t0 = Instant::now() + Duration::from_secs(1);
    tick(&mut state, t0);
    deliver_providers(&mut state, vec![provider("claude", &["claude:default"])]);
    state.open_observation_page(Page::Accounts, &mut ClientShellInput::default());
    tick(&mut state, t0 + Duration::from_millis(10));
    assert!(deliver_usage(
        &mut state,
        vec![account("claude", "claude:default")]
    ));
    assert_eq!(state.observability.selected_pane, None);
    state.compose(120, 40).expect("账号页");
    assert!(
        page_hit(&state, |action| matches!(action, Action::Bind)).is_none(),
        "未选 pane 时没有「确认账号绑定」"
    );
    let next = page_hit(&state, |action| matches!(action, Action::CyclePane(1)))
        .expect("账号页有 pane 选择器");
    click(&mut state, next.x, next.y);
    assert_eq!(state.observability.selected_pane.as_deref(), Some("pane_1"));
    state.observation_action(Action::CyclePane(1), &mut ClientShellInput::default());
    assert_eq!(
        state.observability.selected_pane.as_deref(),
        Some("pane_3"),
        "跳过运行其它厂商的 pane_2"
    );
    state.observation_action(Action::CyclePane(1), &mut ClientShellInput::default());
    assert_eq!(state.observability.selected_pane.as_deref(), Some("pane_1"));
    state.observation_action(Action::CyclePane(-1), &mut ClientShellInput::default());
    assert_eq!(state.observability.selected_pane.as_deref(), Some("pane_3"));
    let frame = state.compose(120, 40).expect("账号页");
    assert!(
        frame_has(&frame, "pane_3"),
        "选中的 pane 显示在绑定行: {}",
        frame_row(&frame, 36)
    );
    let bind = page_hit(&state, |action| matches!(action, Action::Bind))
        .expect("选中 pane 后出现「确认账号绑定」");
    let outcome = click(&mut state, bind.x, bind.y);
    assert_eq!(
        binding_calls(&outcome),
        vec![(
            state.active_endpoint_id.clone(),
            "pane_3".to_owned(),
            "claude:default".to_owned()
        )]
    );
}

#[test]
fn binding_without_a_pane_or_account_explains_instead_of_staying_silent() {
    let mut state = usage_ready();
    deliver_providers(
        &mut state,
        vec![provider("claude", &["claude:work", "claude:home"])],
    );
    state.open_observation_page(Page::Accounts, &mut ClientShellInput::default());
    let t0 = Instant::now() + Duration::from_secs(1);
    tick(&mut state, t0);
    assert!(deliver_usage(
        &mut state,
        vec![
            account("claude", "claude:work"),
            account("claude", "claude:home")
        ]
    ));
    state.observability.message = None;
    let mut outcome = ClientShellInput::default();
    state.observation_action(Action::Bind, &mut outcome);
    assert!(binding_calls(&outcome).is_empty());
    assert!(
        state.observability.message.is_some(),
        "缺 pane 时写 message 而不是静默"
    );
    // 有 pane 但两个账号都没选中：同样提示。
    state.observability.message = None;
    let mut outcome = ClientShellInput::default();
    state.observation_action(Action::BindFocused, &mut outcome);
    assert!(binding_calls(&outcome).is_empty());
    assert!(
        binding_calls(&tick(&mut state, t0 + Duration::from_millis(10))).is_empty(),
        "缺账号时不排队绑定"
    );
    assert!(state.observability.message.is_some(), "缺账号时写 message");
    // 选中账号后「绑定到聚焦 pane」可用。
    state.observation_action(
        Action::Account("claude:home".into()),
        &mut ClientShellInput::default(),
    );
    let mut outcome = ClientShellInput::default();
    state.observation_action(Action::BindFocused, &mut outcome);
    let calls = binding_calls(&outcome)
        .into_iter()
        .chain(binding_calls(&tick(
            &mut state,
            t0 + Duration::from_millis(20),
        )))
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1, "pane_1");
    assert_eq!(calls[0].2, "claude:home");
}

#[test]
fn enabling_the_official_callback_binds_the_active_pane_afterwards() {
    let mut state = subscribing_ready();
    deliver_providers(&mut state, vec![provider("claude", &["claude:default"])]);
    state.open_observation_page(Page::Accounts, &mut ClientShellInput::default());
    let t0 = Instant::now() + Duration::from_secs(1);
    tick(&mut state, t0);
    assert!(deliver_usage(
        &mut state,
        vec![account("claude", "claude:default")]
    ));
    let mut outcome = ClientShellInput::default();
    state.observation_action(Action::UsageIntegration(true), &mut outcome);
    assert_eq!(
        integration_calls(&outcome),
        vec![("claude:default".to_owned(), true)]
    );
    assert!(binding_calls(&outcome).is_empty(), "回调未确认前不绑定");
    let purpose =
        state
            .pending_requests
            .values()
            .find_map(|pending| match &pending.kind {
                crate::client::shell::state::PendingEndpointKind::Observation {
                    purpose, ..
                } if matches!(purpose, Purpose::Integration { .. }) => Some(purpose.clone()),
                _ => None,
            })
            .expect("官方回调请求在途");
    let epoch = state.observability.epoch;
    state.receive_observation(epoch, purpose, Ok(ResponseResult::Ok {}));
    let outcome = tick(&mut state, t0 + Duration::from_millis(10));
    assert_eq!(
        binding_calls(&outcome),
        vec![(
            state.active_endpoint_id.clone(),
            "pane_1".to_owned(),
            "claude:default".to_owned()
        )],
        "启用官方回调成功后顺手绑定当前活动 pane"
    );
    // 开关显示态来自账号的刷新状态：成功后不等 2 秒节流，下一个 tick 立即重拉用量。
    assert_eq!(
        usage_calls(&outcome).len(),
        1,
        "官方回调改写成功后立即重拉页面作用域的用量以刷新开关"
    );

    // 移除回调不触发绑定。
    let mut outcome = ClientShellInput::default();
    state.observation_action(Action::UsageIntegration(false), &mut outcome);
    let purpose =
        state
            .pending_requests
            .values()
            .find_map(|pending| match &pending.kind {
                crate::client::shell::state::PendingEndpointKind::Observation {
                    purpose, ..
                } if matches!(purpose, Purpose::Integration { .. }) => Some(purpose.clone()),
                _ => None,
            })
            .expect("移除回调请求在途");
    state.receive_observation(epoch, purpose, Ok(ResponseResult::Ok {}));
    assert!(binding_calls(&tick(&mut state, t0 + Duration::from_millis(20))).is_empty());
}

/// 同厂商两个账号、未选账号：开关没有作用对象，页面上是禁用态、直接派发也只写提示；
/// 选中账号后开关按该账号的服务端接入态显示，点击作用于同一个账号。
#[test]
fn callback_toggle_targets_the_selected_account_of_a_multi_account_provider() {
    let mut state = subscribing_ready();
    deliver_providers(
        &mut state,
        vec![provider("claude", &["claude:default", "claude:work"])],
    );
    state.open_observation_page(Page::Accounts, &mut ClientShellInput::default());
    let t0 = Instant::now() + Duration::from_secs(1);
    tick(&mut state, t0);
    let mut enabled = refresh_state("claude:default");
    enabled.callback_enabled = Some(true);
    let mut disabled = refresh_state("claude:work");
    disabled.callback_enabled = Some(false);
    assert!(deliver_usage_with_refresh(
        &mut state,
        vec![
            account("claude", "claude:default"),
            account("claude", "claude:work"),
        ],
        vec![enabled, disabled],
    ));
    let texts = &crate::i18n::texts().monitor;
    let frame = state.compose(120, 40).expect("账号页");
    assert!(
        page_hit(&state, |action| matches!(
            action,
            Action::UsageIntegration(_)
        ))
        .is_none(),
        "未选账号时开关没有作用对象，不给命中区"
    );
    assert!(
        frame_has(&frame, texts.select_account_first),
        "禁用态标注先选账号"
    );
    let mut outcome = ClientShellInput::default();
    state.observation_action(Action::UsageIntegration(true), &mut outcome);
    assert!(
        integration_calls(&outcome).is_empty(),
        "没有作用对象不发请求"
    );
    assert_eq!(
        state.observability.message.as_deref(),
        Some(texts.select_account_first),
        "直接派发也不静默"
    );

    // 选中账号 = 页面换代，刷新状态随之清空（旧快照保留变暗）：在新响应到达前开关是
    // 未知态；这里按真实流程投递选中后的响应。
    let reselect = |state: &mut ClientShellState, account_id: &str, at: Instant| {
        state.observation_action(
            Action::Account(account_id.into()),
            &mut ClientShellInput::default(),
        );
        state.compose(120, 40).expect("账号页");
        assert!(
            page_hit(state, |action| matches!(
                action,
                Action::UsageIntegration(true)
            ))
            .is_some(),
            "新数据到达前状态未知，提供「启用」"
        );
        tick(state, at);
        let mut enabled = refresh_state("claude:default");
        enabled.callback_enabled = Some(true);
        let mut disabled = refresh_state("claude:work");
        disabled.callback_enabled = Some(false);
        assert!(deliver_usage_with_refresh(
            state,
            vec![
                account("claude", "claude:default"),
                account("claude", "claude:work"),
            ],
            vec![enabled, disabled],
        ));
        state.compose(120, 40).expect("账号页");
    };
    // 选中未接入的账号：开关提供「启用」，点击作用于该账号。
    reselect(&mut state, "claude:work", t0 + Duration::from_secs(3));
    let rect = page_hit(&state, |action| {
        matches!(action, Action::UsageIntegration(true))
    })
    .expect("未接入账号提供启用");
    assert!(page_hit(&state, |action| matches!(
        action,
        Action::UsageIntegration(false)
    ))
    .is_none());
    let outcome = click(&mut state, rect.x, rect.y);
    assert_eq!(
        integration_calls(&outcome),
        vec![("claude:work".to_owned(), true)]
    );

    // 选中已接入的账号：同一个开关变成「移除」。
    reselect(&mut state, "claude:default", t0 + Duration::from_secs(6));
    assert!(page_hit(&state, |action| matches!(
        action,
        Action::UsageIntegration(false)
    ))
    .is_some());
    assert!(page_hit(&state, |action| matches!(
        action,
        Action::UsageIntegration(true)
    ))
    .is_none());
}

/// 「悬浮延时」按固定档位循环，只回写自己的偏好键：其它 usage_* 键保持未设置。
#[test]
fn hover_delay_cycles_fixed_steps_and_persists_only_its_own_key() {
    let path = std::env::temp_dir().join(format!(
        "herdr-shell-hover-delay-{}.json",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let mut state = ClientShellState::new(
        ClientShellConfig::from_config(&Config::default()).with_preferences_path(path.clone()),
    );
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    assert_eq!(state.config.preferences.usage_hover_delay_ms, None);
    assert_eq!(state.observability.usage.hover_delay_ms, 400, "配置默认值");
    let mut seen = Vec::new();
    for _ in 0..5 {
        state.observation_action(Action::HoverDelay(1), &mut ClientShellInput::default());
        seen.push(state.observability.usage.hover_delay_ms);
    }
    assert_eq!(seen, vec![800, 1200, 2000, 200, 400], "档位循环回到起点");
    let preferences = &state.config.preferences;
    assert_eq!(preferences.usage_hover_delay_ms, Some(400));
    assert_eq!(preferences.usage_enabled, None, "未改过的键不写影子值");
    assert_eq!(preferences.usage_format, None);
    assert_eq!(preferences.usage_position, None);
    assert_eq!(preferences.usage_disabled_providers, None);
    let saved = preferences::load(&path).expect("偏好已写入");
    assert_eq!(saved.usage_hover_delay_ms, Some(400));
    assert_eq!(saved.usage_format, None);
    // 配置文件里的非档位值：第一次点击落到下一档，不跳档。
    state.observability.usage.hover_delay_ms = 300;
    state.observation_action(Action::HoverDelay(1), &mut ClientShellInput::default());
    assert_eq!(state.observability.usage.hover_delay_ms, 400);
    std::fs::remove_file(path).expect("remove preferences");
}

/// 回到总览是显式选择：之后的 tick 不再按聚焦 pane / 首个厂商自动选回。
#[test]
fn returning_to_the_overview_is_not_undone_by_auto_selection() {
    let mut state = usage_ready();
    deliver_providers(&mut state, vec![provider("claude", &["claude:default"])]);
    state.open_observation_page(Page::Accounts, &mut ClientShellInput::default());
    let t0 = Instant::now() + Duration::from_secs(1);
    tick(&mut state, t0);
    assert_eq!(
        state.observability.selected_provider.as_deref(),
        Some("claude"),
        "打开账号页按聚焦 pane 自动选中"
    );
    state.observation_action(Action::Overview, &mut ClientShellInput::default());
    assert_eq!(state.observability.selected_provider, None);
    let outcome = tick(&mut state, t0 + Duration::from_secs(3));
    assert_eq!(
        state.observability.selected_provider, None,
        "自动选中不抢回"
    );
    let calls = usage_calls(&outcome);
    assert_eq!(calls.len(), 1, "总览态按本机启用的厂商逐个轮询");
    assert_eq!(calls[0].1.agent.as_deref(), Some("claude"));
}

#[test]
fn pending_binding_renders_a_one_click_binding_row() {
    let mut state = usage_ready();
    add_agent_pane(&mut state, "pane_2", "claude");
    deliver_providers(&mut state, vec![provider("claude", &["claude:default"])]);
    state.open_observation_page(Page::Accounts, &mut ClientShellInput::default());
    let t0 = Instant::now() + Duration::from_secs(1);
    tick(&mut state, t0);
    let mut pending = refresh_state("claude:default");
    pending.pending_binding = Some(UsagePendingBinding {
        pane_id: "pane_2".into(),
        agent: "claude".into(),
        candidates: vec!["claude:default".into()],
        rejected_at_ms: 1,
    });
    assert!(deliver_usage_with_refresh(
        &mut state,
        vec![account("claude", "claude:default")],
        vec![pending],
    ));
    let frame = state.compose(120, 40).expect("账号页");
    assert!(frame_has(&frame, "pane_2"), "待绑定的 pane 显示在页面上");
    let rect = page_hit(&state, |action| {
        matches!(action, Action::BindTo(pane, account) if pane == "pane_2" && account == "claude:default")
    })
    .expect("待绑定行提供一键绑定");
    let outcome = click(&mut state, rect.x, rect.y);
    assert_eq!(
        binding_calls(&outcome),
        vec![(
            state.active_endpoint_id.clone(),
            "pane_2".to_owned(),
            "claude:default".to_owned()
        )]
    );
}

/// 投递一条退订确认（服务端对 `account.usage.unsubscribe` 恒回 `active:false`）。
fn deliver_unsubscribed(state: &mut ClientShellState, id: &str) -> bool {
    state.receive_observation(
        0,
        Purpose::Unsubscribe,
        Ok(ResponseResult::ObservationSubscription {
            subscription_id: id.into(),
            active: false,
        }),
    )
}

/// 本次 outcome 里发往各端点的订阅 / 退订：`(endpoint, subscribe?, id)`。
fn subscription_targets(
    outcome: &ClientShellInput,
) -> Vec<(ClientEndpointId, bool, Option<String>)> {
    outcome
        .actions
        .iter()
        .filter_map(|action| match action {
            ClientShellAction::Endpoint {
                endpoint_id,
                request,
                ..
            } => match &request.method {
                Method::AccountUsageSubscribe(_) => Some((endpoint_id.clone(), true, None)),
                Method::AccountUsageUnsubscribe(params) => {
                    Some((endpoint_id.clone(), false, params.subscription_id.clone()))
                }
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// 在途的观测请求：`(request_id, boot_id, purpose)`，按 purpose 谓词筛选。
fn pending_observation(
    state: &ClientShellState,
    matches: impl Fn(&Purpose) -> bool,
) -> Option<(String, String, Purpose)> {
    state
        .pending_requests
        .iter()
        .find_map(|(id, pending)| match &pending.kind {
            crate::client::shell::state::PendingEndpointKind::Observation { purpose, .. }
                if matches(purpose) =>
            {
                Some((id.clone(), pending.boot_id.clone(), purpose.clone()))
            }
            _ => None,
        })
}

#[test]
fn subscription_confirmation_arrives_through_the_real_endpoint_path() {
    let mut state = subscribing_ready();
    deliver_providers(&mut state, vec![provider("claude", &["claude:default"])]);
    state.open_observation_page(Page::Accounts, &mut ClientShellInput::default());
    let t0 = Instant::now() + Duration::from_secs(1);
    assert_eq!(subscribe_calls(&tick(&mut state, t0)).len(), 1);
    let (request_id, boot_id, _) =
        pending_observation(&state, |purpose| matches!(purpose, Purpose::Subscribe))
            .expect("订阅请求在途");
    // 真实路径：响应经 handle_endpoint_result 按 boot 校验后落到订阅生命周期。
    let (repaint, actions) = state.handle_endpoint_result(
        &boot_id,
        &request_id,
        Ok(ResponseResult::ObservationSubscription {
            subscription_id: "usage-7".into(),
            active: true,
        }),
    );
    assert!(repaint, "账号页可见时确认订阅触发重绘");
    assert!(actions.is_empty());
    assert!(
        state
            .observability
            .subscription
            .active
            .as_ref()
            .is_some_and(|active| active.id == "usage-7"),
        "订阅经真实响应路径确认"
    );
    assert!(deliver_usage(
        &mut state,
        vec![account("claude", "claude:default")]
    ));
    assert!(
        usage_calls(&tick(&mut state, t0 + Duration::from_secs(2))).is_empty(),
        "订阅确认后轮询降为 30 s 兜底"
    );
}

#[test]
fn switching_the_active_endpoint_unsubscribes_on_the_old_endpoint() {
    let mut state = subscribing_ready();
    deliver_providers(&mut state, vec![provider("claude", &["claude:default"])]);
    state.open_observation_page(Page::Accounts, &mut ClientShellInput::default());
    let t0 = Instant::now() + Duration::from_secs(1);
    assert_eq!(subscribe_calls(&tick(&mut state, t0)).len(), 1);
    assert!(deliver_subscription(&mut state, "usage-1", true));
    let local = state.active_endpoint_id.clone();
    let remote = add_remote_usage_endpoint(&mut state);
    state.set_endpoint_methods_for(
        &remote,
        Some(vec![
            "account.usage.get".into(),
            "account.usage.refresh".into(),
            "account.usage.providers".into(),
            "account.usage.subscribe".into(),
            "account.usage.unsubscribe".into(),
        ]),
    );
    assert!(state.activate_endpoint_projection(&remote));
    let switched = tick(&mut state, t0 + Duration::from_millis(10));
    let targets = subscription_targets(&switched);
    assert!(
        targets.contains(&(local.clone(), false, Some("usage-1".to_owned()))),
        "旧端点上的订阅必须发回旧端点退订: {targets:?}"
    );
    assert!(
        targets.contains(&(remote.clone(), true, None)),
        "新端点按当前作用域订阅: {targets:?}"
    );
    assert!(
        !targets
            .iter()
            .any(|(endpoint, subscribe, _)| !*subscribe && endpoint == &remote),
        "退订不会发往新端点: {targets:?}"
    );
}

#[test]
fn failed_unsubscribe_is_retried_with_a_bounded_backoff() {
    let mut state = subscribing_ready();
    deliver_providers(&mut state, vec![provider("claude", &["claude:default"])]);
    state.open_observation_page(Page::Accounts, &mut ClientShellInput::default());
    let t0 = Instant::now() + Duration::from_secs(1);
    assert_eq!(subscribe_calls(&tick(&mut state, t0)).len(), 1);
    assert!(deliver_subscription(&mut state, "usage-1", true));
    state.observation_action(Action::Close, &mut ClientShellInput::default());
    assert_eq!(
        unsubscribe_calls(&tick(&mut state, t0 + Duration::from_secs(1))),
        vec![Some("usage-1".to_owned())]
    );
    // 退订在途：不重复发。
    assert!(unsubscribe_calls(&tick(&mut state, t0 + Duration::from_secs(2))).is_empty());
    // 退订失败（超时 / 传输错误）：id 留在队列，退避后重试。
    let fail = |state: &mut ClientShellState| {
        state.receive_observation(
            0,
            Purpose::Unsubscribe,
            Err(ClientShellEndpointError {
                code: Some("endpoint_timeout".into()),
                message: "timed out".into(),
            }),
        );
    };
    // 失败时刻按真实时钟记退避（响应处理没有 tick 的 now），后续 tick 以它为锚。
    fail(&mut state);
    let failed_at = Instant::now();
    assert!(
        unsubscribe_calls(&tick(&mut state, failed_at + Duration::from_secs(1))).is_empty(),
        "失败后不立即重试"
    );
    assert_eq!(
        unsubscribe_calls(&tick(&mut state, failed_at + Duration::from_secs(3))),
        vec![Some("usage-1".to_owned())],
        "退避到期后重发退订"
    );
    // 明确的 active:false 才出队。
    deliver_unsubscribed(&mut state, "usage-1");
    assert!(unsubscribe_calls(&tick(&mut state, failed_at + Duration::from_secs(6))).is_empty());
    assert!(unsubscribe_calls(&tick(&mut state, failed_at + Duration::from_secs(9))).is_empty());

    // 重试有上界：连续失败后放弃，不无限重试。
    state.workbench_open(PanelId::Monitor);
    assert_eq!(
        subscribe_calls(&tick(&mut state, failed_at + Duration::from_secs(10))).len(),
        1
    );
    assert!(deliver_subscription(&mut state, "usage-2", true));
    state.observation_action(Action::Close, &mut ClientShellInput::default());
    let mut at = failed_at + Duration::from_secs(15);
    let mut sent = 0;
    for _ in 0..6 {
        sent += unsubscribe_calls(&tick(&mut state, at)).len();
        fail(&mut state);
        at += Duration::from_secs(5);
    }
    assert!(
        (1..=3).contains(&sent),
        "退订重试有上界（实际发出 {sent} 次）"
    );
    assert!(unsubscribe_calls(&tick(&mut state, at + Duration::from_secs(5))).is_empty());
}

#[test]
fn reconnecting_the_active_endpoint_resubscribes_and_polls_again() {
    let mut state = subscribing_ready();
    deliver_providers(&mut state, vec![provider("claude", &["claude:default"])]);
    state.open_observation_page(Page::Accounts, &mut ClientShellInput::default());
    let t0 = Instant::now() + Duration::from_secs(1);
    assert_eq!(subscribe_calls(&tick(&mut state, t0)).len(), 1);
    assert!(deliver_subscription(&mut state, "usage-1", true));
    assert!(deliver_usage(
        &mut state,
        vec![account("claude", "claude:default")]
    ));
    assert!(usage_calls(&tick(&mut state, t0 + Duration::from_secs(2))).is_empty());

    // 同 boot 断线：服务端那份订阅随连接消亡；断线期间不发请求。
    let endpoint_id = state.active_endpoint_id.clone();
    state.mark_endpoint_disconnected(&endpoint_id);
    let offline = tick(&mut state, t0 + Duration::from_secs(3));
    assert!(usage_calls(&offline).is_empty());
    assert!(subscription_targets(&offline).is_empty());
    assert!(
        state.observability.subscription.active.is_none(),
        "断线即视为订阅失效"
    );

    // 恢复在线：同一 tick 重新订阅并立刻冷启动一次 get，不等 30 s 兜底。
    state.set_endpoint_status(&endpoint_id, ClientEndpointStatus::Online);
    let back = tick(&mut state, t0 + Duration::from_secs(4));
    assert_eq!(subscribe_calls(&back).len(), 1, "重连后重新订阅");
    assert_eq!(usage_calls(&back).len(), 1, "重连后立刻 get 一次");
    assert!(
        unsubscribe_calls(&back).is_empty(),
        "旧订阅已随连接释放，不再退订"
    );
    // 订阅被拒时回到 2 s 轮询而不是 30 s。
    state.receive_observation(
        0,
        Purpose::Subscribe,
        Err(ClientShellEndpointError {
            code: Some("subscription_limit".into()),
            message: "too many".into(),
        }),
    );
    assert!(deliver_usage(
        &mut state,
        vec![account("claude", "claude:default")]
    ));
    assert_eq!(
        usage_calls(&tick(&mut state, t0 + Duration::from_secs(7))).len(),
        1,
        "订阅不可用时恢复 2 s 轮询"
    );
}

#[test]
fn queued_binding_is_dropped_when_the_hover_moves_to_another_host() {
    let mut state = usage_ready();
    deliver_providers(&mut state, vec![provider("claude", &["claude:default"])]);
    let remote = add_remote_usage_endpoint(&mut state);
    hover_on(&mut state, Some(remote.clone()), "pane_1", "claude");
    let t0 = Instant::now() + Duration::from_secs(1);
    tick(&mut state, t0);
    assert!(deliver_hover_usage(
        &mut state,
        vec![account("claude", "claude:default")]
    ));
    let outcome = click_hover_action(&mut state, |action| {
        matches!(action, Action::UsageIntegration(true))
    });
    assert_eq!(integration_calls(&outcome).len(), 1);
    let (request_id, boot_id, _) = pending_observation(&state, |purpose| {
        matches!(purpose, Purpose::HoverIntegration { .. })
    })
    .expect("官方回调请求在途");
    assert_eq!(boot_id, "remote-boot");
    // 回调响应到达：排队绑定 (remote, pane_1, claude:default)。
    state.handle_endpoint_result(&boot_id, &request_id, Ok(ResponseResult::Ok {}));
    // 派发前鼠标移到了本机的 pane 上并停满延时：目标主机已变，必须放弃而不是
    // 把远端的 pane_1 绑到本机。
    hover_on(&mut state, None, "pane_1", "claude");
    state.observability.message = None;
    let moved = tick(&mut state, t0 + Duration::from_secs(1));
    assert!(
        binding_calls(&moved).is_empty(),
        "目标主机变化后不派发绑定: {:?}",
        binding_calls(&moved)
    );
    assert!(state.observability.message.is_some(), "页脚说明未绑定");
    assert!(
        binding_calls(&tick(&mut state, t0 + Duration::from_secs(2))).is_empty(),
        "放弃后不再重试"
    );
}

#[test]
fn queued_binding_targets_the_host_that_answered_the_callback() {
    let mut state = usage_ready();
    deliver_providers(&mut state, vec![provider("claude", &["claude:default"])]);
    let remote = add_remote_usage_endpoint(&mut state);
    hover_on(&mut state, Some(remote.clone()), "pane_1", "claude");
    let t0 = Instant::now() + Duration::from_secs(1);
    tick(&mut state, t0);
    assert!(deliver_hover_usage(
        &mut state,
        vec![account("claude", "claude:default")]
    ));
    click_hover_action(&mut state, |action| {
        matches!(action, Action::UsageIntegration(true))
    });
    let (request_id, boot_id, _) = pending_observation(&state, |purpose| {
        matches!(purpose, Purpose::HoverIntegration { .. })
    })
    .expect("官方回调请求在途");
    state.handle_endpoint_result(&boot_id, &request_id, Ok(ResponseResult::Ok {}));
    let calls = binding_calls(&tick(&mut state, t0 + Duration::from_millis(10)));
    assert_eq!(
        calls,
        vec![(remote, "pane_1".to_owned(), "claude:default".to_owned())],
        "悬浮层未变时绑定发往应答回调的主机"
    );
}

#[test]
fn page_binding_refuses_a_pane_running_another_provider() {
    let mut snapshot = snapshot();
    snapshot.agents.push(agent_in_pane("pane_1", "claude"));
    snapshot.agents.push(agent_in_pane("pane_2", "codex"));
    snapshot.agents.push(agent_in_pane("pane_3", "claude"));
    let mut state = docked_with(snapshot);
    state.set_endpoint_methods(Some(vec![
        "client.views.set".into(),
        "tab.focus".into(),
        "account.usage.get".into(),
        "account.usage.refresh".into(),
        "account.usage.providers".into(),
        "account.binding.set".into(),
    ]));
    let t0 = Instant::now() + Duration::from_secs(1);
    tick(&mut state, t0);
    deliver_providers(
        &mut state,
        vec![
            provider("claude", &["claude:default"]),
            provider("codex", &["codex:default"]),
        ],
    );
    state.open_observation_page(Page::Accounts, &mut ClientShellInput::default());
    tick(&mut state, t0 + Duration::from_millis(10));
    // 总览态（账号页首个标签「全部厂商」）：厂商未选中，页面只有一个 claude 账号。
    state.observability.selected_provider = None;
    assert!(deliver_usage(
        &mut state,
        vec![account("claude", "claude:default")]
    ));
    // pane 选择器只给出运行该账号厂商 agent 的 pane：跳过 codex 的 pane_2。
    state.observation_action(Action::CyclePane(1), &mut ClientShellInput::default());
    assert_eq!(state.observability.selected_pane.as_deref(), Some("pane_1"));
    state.observation_action(Action::CyclePane(1), &mut ClientShellInput::default());
    assert_eq!(
        state.observability.selected_pane.as_deref(),
        Some("pane_3"),
        "总览态下选择器也按账号厂商过滤"
    );
    // 即便手上拿到了跨厂商的 pane，确认绑定也拒绝并说明。
    state.observability.selected_pane = Some("pane_2".into());
    state.observability.selected_pane_label = Some("codex · pane_2".into());
    state.observability.message = None;
    let mut outcome = ClientShellInput::default();
    state.observation_action(Action::Bind, &mut outcome);
    assert!(
        binding_calls(&outcome).is_empty(),
        "跨厂商 pane 不产生 account.binding.set"
    );
    assert!(state.observability.message.is_some(), "页脚说明拒绝原因");
    // 一键绑定（服务端候选）同样过厂商校验。
    state.observability.message = None;
    let mut outcome = ClientShellInput::default();
    state.observation_action(
        Action::BindTo("pane_2".into(), "claude:default".into()),
        &mut outcome,
    );
    assert!(binding_calls(&outcome).is_empty());
    assert!(state.observability.message.is_some());
    // 同厂商 pane 正常绑定。
    let mut outcome = ClientShellInput::default();
    state.observation_action(
        Action::BindTo("pane_3".into(), "claude:default".into()),
        &mut outcome,
    );
    assert_eq!(binding_calls(&outcome).len(), 1);
}

#[test]
fn bind_focused_refuses_when_the_accounts_provider_is_unknown() {
    let mut state = usage_ready();
    deliver_providers(&mut state, vec![provider("claude", &["claude:default"])]);
    state.open_observation_page(Page::Accounts, &mut ClientShellInput::default());
    tick(&mut state, Instant::now() + Duration::from_secs(1));
    // 端点 / boot 变化后的窗口：厂商列表与账号快照被清空，选择保留。
    state.observability.providers.clear();
    state.observability.accounts.clear();
    state.observability.selected_account = Some("claude:default".into());
    state.observability.message = None;
    let mut outcome = ClientShellInput::default();
    state.observation_action(Action::BindFocused, &mut outcome);
    assert!(
        binding_calls(&outcome).is_empty(),
        "账号厂商解析不出时没有候选 pane，不能把任意聚焦 pane 绑上去"
    );
    assert!(state.observability.message.is_some());
}

#[test]
fn refreshing_events_for_other_scopes_are_dropped() {
    let mut state = subscribing_ready();
    deliver_providers(
        &mut state,
        vec![
            provider("claude", &["claude:default"]),
            provider("codex", &["codex:default"]),
        ],
    );
    state.open_observation_page(Page::Accounts, &mut ClientShellInput::default());
    tick(&mut state, Instant::now() + Duration::from_secs(1));
    assert_eq!(
        state.observability.selected_provider.as_deref(),
        Some("claude")
    );
    assert!(deliver_usage_with_refresh(
        &mut state,
        vec![account("claude", "claude:default")],
        vec![refresh_state("claude:default")],
    ));
    // 切换厂商后旧订阅的尾巴：codex 页面上收到 claude 的 refreshing 事件。
    state.observation_action(
        Action::Provider("codex".into()),
        &mut ClientShellInput::default(),
    );
    let mut stale = refresh_state("claude:default");
    stale.in_flight = true;
    let mut unknown = refresh_state("ghost:default");
    unknown.in_flight = true;
    push_event(
        &mut state,
        "boot-1",
        ObservationEventEnvelope::AccountUsageRefreshing(AccountUsageRefreshingEvent {
            refresh: vec![stale, unknown],
        }),
    );
    assert!(
        state.observability.refresh_states.is_empty(),
        "其它厂商 / 解析不出厂商的 refreshing 事件不写入页面: {:?}",
        state.observability.refresh_states
    );
    // 当前厂商的事件照常合并。
    let mut mine = refresh_state("codex:default");
    mine.in_flight = true;
    push_event(
        &mut state,
        "boot-1",
        ObservationEventEnvelope::AccountUsageRefreshing(AccountUsageRefreshingEvent {
            refresh: vec![mine],
        }),
    );
    assert_eq!(state.observability.refresh_states.len(), 1);
    assert_eq!(
        state.observability.refresh_states[0].account_id,
        "codex:default"
    );
}

#[test]
fn binding_success_clears_the_pending_binding_row() {
    let mut state = usage_ready();
    deliver_providers(&mut state, vec![provider("claude", &["claude:default"])]);
    state.open_observation_page(Page::Accounts, &mut ClientShellInput::default());
    let t0 = Instant::now() + Duration::from_secs(1);
    tick(&mut state, t0);
    let mut pending = refresh_state("claude:default");
    pending.pending_binding = Some(UsagePendingBinding {
        pane_id: "pane_2".into(),
        agent: "claude".into(),
        candidates: vec!["claude:default".into()],
        rejected_at_ms: 1,
    });
    assert!(deliver_usage_with_refresh(
        &mut state,
        vec![account("claude", "claude:default")],
        vec![pending],
    ));
    // 绑定由别的客户端 / CLI 完成，本端只收到绑定成功回流：待办行立即消失。
    let epoch = state.observability.epoch;
    state.receive_observation(
        epoch,
        Purpose::Binding,
        Ok(ResponseResult::AccountBinding {
            account_id: "claude:default".into(),
            pane_id: "pane_2".into(),
        }),
    );
    assert!(
        state
            .observability
            .refresh_states
            .iter()
            .all(|state| state.pending_binding.is_none()),
        "绑定成功后待办绑定不再挂着"
    );
    state.compose(120, 40).expect("账号页");
    assert!(
        page_hit(&state, |action| matches!(action, Action::BindTo(..))).is_none(),
        "一键绑定行随之消失"
    );
}

#[test]
fn refresh_wait_follows_the_selected_account_or_the_earliest_one() {
    let mut state = usage_ready();
    deliver_providers(
        &mut state,
        vec![provider("claude", &["claude:work", "claude:home"])],
    );
    state.open_observation_page(Page::Accounts, &mut ClientShellInput::default());
    let t0 = Instant::now() + Duration::from_secs(1);
    tick(&mut state, t0);
    let now_ms = state.observability.now_ms;
    let mut slow = refresh_state("claude:work");
    slow.next_allowed_at_ms = Some(now_ms + 50_000);
    let mut soon = refresh_state("claude:home");
    soon.next_allowed_at_ms = Some(now_ms + 5_000);
    let accounts = vec![
        account("claude", "claude:work"),
        account("claude", "claude:home"),
    ];
    assert!(deliver_usage_with_refresh(
        &mut state,
        accounts.clone(),
        vec![slow.clone(), soon.clone()],
    ));
    // 未选账号：按最早可刷新的账号提示，而不是被最慢的账号锁住整页。
    let frame = state.compose(120, 40).expect("账号页");
    assert!(
        frame_has(&frame, "5秒后可刷新") || frame_has(&frame, "Refresh in 5s"),
        "取作用域内最小等待: {}",
        frame_row(&frame, 38)
    );
    // 选中慢的账号：只看它（显式选择的强意图刷新先发出并得到响应）。
    state.observation_action(
        Action::Account("claude:work".into()),
        &mut ClientShellInput::default(),
    );
    tick(&mut state, t0 + Duration::from_millis(10));
    assert!(deliver_usage_with_refresh(
        &mut state,
        vec![account("claude", "claude:work")],
        vec![slow.clone()],
    ));
    let frame = state.compose(120, 40).expect("账号页");
    assert!(
        frame_has(&frame, "50秒后可刷新") || frame_has(&frame, "Refresh in 50s"),
        "选中账号时只看它: {}",
        frame_row(&frame, 38)
    );
    // 任一账号已可刷新：按钮可点。
    state.observation_action(
        Action::Provider("claude".into()),
        &mut ClientShellInput::default(),
    );
    tick(&mut state, t0 + Duration::from_millis(20));
    assert!(deliver_usage_with_refresh(
        &mut state,
        accounts.clone(),
        vec![slow.clone(), refresh_state("claude:home")],
    ));
    state.compose(120, 40).expect("账号页");
    assert!(
        page_hit(&state, |action| matches!(action, Action::Refresh)).is_some(),
        "有账号可刷新即允许点击"
    );
    // 超过可信上限的厂商退避：按钮可点（不锁全页），但在账号自己的行里说明。
    let mut backoff = refresh_state("claude:work");
    backoff.next_allowed_at_ms = Some(now_ms + 300_000);
    backoff.retry_after_ms = Some(now_ms + 300_000);
    assert!(deliver_usage_with_refresh(
        &mut state,
        accounts,
        vec![backoff, refresh_state("claude:home")],
    ));
    let frame = state.compose(120, 40).expect("账号页");
    assert!(
        page_hit(&state, |action| matches!(action, Action::Refresh)).is_some(),
        "超长退避不锁按钮"
    );
    assert!(
        frame_has(&frame, "退避") || frame_has(&frame, "backoff"),
        "超长退避在账号行说明: {:?}",
        (0..40).map(|y| frame_row(&frame, y)).collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------
// (C) agent 行悬浮卡：悬浮延时、离开宽限、钉住态与浮层内点击
// ---------------------------------------------------------------------------

/// 一个带百分比指标的账号快照：仪表盘会为它画进度条。
fn account_with_percent(agent: &str, id: &str, percent: f64) -> AccountUsageSnapshot {
    let mut snapshot = account(agent, id);
    snapshot.metrics.push(crate::api::schema::UsageMetric {
        id: "session".into(),
        label: "5h".into(),
        unit: "%".into(),
        scope: "session".into(),
        text_value: None,
        used: None,
        limit: None,
        remaining: None,
        used_percent: Some(percent),
        amount_decimal: None,
        resets_at: None,
        window_seconds: None,
    });
    snapshot
}

/// agent 行悬浮的快照：`(visible, pinned)`，None = 没有 hover。
fn agent_hover(state: &ClientShellState) -> Option<(bool, bool)> {
    state
        .observability
        .hover
        .as_ref()
        .map(|hover| (hover.visible, hover.pinned))
}

/// Agents 面板里某个 pane 的行矩形（经典布局记在 `agents`，停靠工作台记在
/// `endpoint_agents`）。
fn agent_row(state: &ClientShellState, pane_id: &str) -> Rect {
    state
        .hits
        .agents
        .iter()
        .find(|(_, pane)| pane == pane_id)
        .map(|(rect, _)| *rect)
        .or_else(|| {
            state
                .hits
                .endpoint_agents
                .iter()
                .find(|(_, _, pane)| pane == pane_id)
                .map(|(rect, _, _)| *rect)
        })
        .unwrap_or_else(|| panic!("Agents 面板列出 {pane_id}"))
}

/// 经生产入口（右键菜单「用量」走的同一个 `pin_agent_usage_card`）打开并钉住
/// 活动端点上某 agent 的用量卡。
fn pin_agent_hover(state: &mut ClientShellState, pane_id: &str, agent: &str) {
    let endpoint_id = state.active_endpoint_id.clone();
    state.pin_agent_usage_card(
        endpoint_id,
        pane_id.into(),
        agent.into(),
        &mut ClientShellInput::default(),
    );
}

/// 系统页「编辑布局」：↑↓ 移动选中的卡片而不是滚动卡片内容，顺序写回偏好；
/// Esc 先退出编辑，再按一次才关闭页面。
#[test]
fn edit_layout_mode_moves_the_selected_card_with_arrow_keys() {
    use crossterm::event::KeyCode;
    let mut state = docked();
    state.open_observation_page(Page::Monitor, &mut ClientShellInput::default());
    state.observability.selected_card = Some("memory".into());
    let before = state.observability.monitor.visible.clone();
    assert_eq!(before[2], "memory");
    press_key(&mut state, KeyCode::Up);
    assert_eq!(
        state.observability.monitor.visible, before,
        "非编辑模式 ↑ 不移动卡片"
    );
    state.observation_action(Action::EditLayout, &mut ClientShellInput::default());
    assert!(state.observability.layout_editing);
    press_key(&mut state, KeyCode::Up);
    assert_eq!(state.observability.monitor.visible[1], "memory");
    press_key(&mut state, KeyCode::Down);
    assert_eq!(state.observability.monitor.visible[2], "memory");
    assert_eq!(
        state
            .config
            .preferences
            .monitor
            .as_ref()
            .map(|monitor| monitor.visible.clone()),
        Some(state.observability.monitor.visible.clone()),
        "卡片顺序写回偏好"
    );
    press_key(&mut state, KeyCode::Esc);
    assert!(!state.observability.layout_editing, "Esc 先退出编辑布局");
    assert_eq!(state.observability.page, Some(Page::Monitor));
    press_key(&mut state, KeyCode::Esc);
    assert_eq!(state.observability.page, None, "再按 Esc 才关闭页面");
}

/// 系统页卡片滚动走真实绘制链路：`commit_paint` 把上一帧各卡的滚动上界写回，
/// 滚轮按它钳位——进程表滚到底停在最后一屏，反向滚第一格画面立刻变化。
#[test]
fn process_card_wheel_scroll_is_bounded_by_the_painted_table() {
    use crate::api::schema::{ProcessIdentity, ProcessMetric, ProcessSort, SystemMetricsSnapshot};
    let mut state = docked();
    state.open_observation_page(Page::Monitor, &mut ClientShellInput::default());
    state.observability.monitor.visible = vec!["processes".into()];
    state.observability.process_sort = ProcessSort::Pid;
    state.observability.metrics = Some(Box::new(SystemMetricsSnapshot {
        boot_id: "boot".into(),
        sequence: 1,
        sampled_at_ms: 1_000,
        processes: (1..=40)
            .map(|pid| ProcessMetric {
                identity: ProcessIdentity {
                    pid,
                    ..Default::default()
                },
                name: format!("proc{pid:02}"),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    }));
    state.compose(120, 40).expect("监控面板");
    let card = state
        .observability
        .hits
        .iter()
        .find_map(|(rect, action)| {
            matches!(action, Action::Card(id) if id == "processes").then_some(*rect)
        })
        .expect("进程卡已画出");
    let limit = state
        .observability
        .card_scroll_limits
        .get("processes")
        .expect("上一帧写回了进程卡的滚动上界");
    assert!(limit > 0, "40 个进程放不下一张卡");
    let wheel = |state: &mut ClientShellState, kind: MouseEventKind| {
        state.handle_mouse(
            MouseEvent {
                kind,
                column: card.x + 2,
                row: card.y + 3,
                modifiers: KeyModifiers::NONE,
            },
            &mut ClientShellInput::default(),
        );
    };
    let card_text = |state: &mut ClientShellState| {
        let frame = state.compose(120, 40).expect("重绘");
        (card.y..card.bottom())
            .map(|y| {
                (card.x..card.right())
                    .map(|x| {
                        frame.cells[usize::from(y) * usize::from(frame.width) + usize::from(x)]
                            .symbol
                            .as_str()
                    })
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    for _ in 0..limit + 10 {
        wheel(&mut state, MouseEventKind::ScrollDown);
    }
    assert_eq!(
        state.observability.card_scroll.get("processes").copied(),
        Some(limit),
        "滚到底停在上界"
    );
    let bottom = card_text(&mut state);
    assert!(
        bottom.contains("proc40"),
        "最后一屏露出最后一个进程\n{bottom}"
    );
    wheel(&mut state, MouseEventKind::ScrollUp);
    assert_eq!(
        state.observability.card_scroll.get("processes").copied(),
        Some(limit - 1)
    );
    let after = card_text(&mut state);
    assert!(!after.contains("proc40"), "反向第一格画面就变\n{after}");
}

/// 监控偏好页的控件：点分段 / 步进器 / 开关只回写各自的偏好键（`PreferenceKey`），
/// 落盘后重启可恢复；图表字形是独立的客户端偏好键 `monitor_chart_glyphs`。
#[test]
fn preferences_page_controls_write_back_only_their_preference_key() {
    use crate::client::shell::observability::ChartGlyphsPreference;
    use crate::config::UsageDisplayFormat;
    let path = std::env::temp_dir().join(format!(
        "herdr-shell-monitor-prefs-{}.json",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let mut state = ClientShellState::new(
        ClientShellConfig::from_config(&Config::default()).with_preferences_path(path.clone()),
    );
    state.set_snapshot(Box::new(snapshot()));
    state.set_endpoint_methods(Some(vec!["client.views.set".into(), "tab.focus".into()]));
    state.set_pane_surface(surface());
    state.compose(120, 60).expect("初始画面");
    state.tick_workbench(std::time::Instant::now(), &mut ClientShellInput::default());
    state.open_observation_page(Page::Settings, &mut ClientShellInput::default());
    state.compose(120, 60).expect("监控偏好页");
    let hit = |state: &ClientShellState, wanted: fn(&Action) -> bool| {
        page_hit(state, wanted).expect("控件命中区")
    };
    // 分段控件：用量样式 → 表格，只写 usage_format。
    let table = hit(&state, |action| {
        matches!(action, Action::UsageFormat(UsageDisplayFormat::Table))
    });
    click(&mut state, table.x + 1, table.y);
    assert_eq!(state.observability.usage.format, UsageDisplayFormat::Table);
    let preferences = &state.config.preferences;
    assert_eq!(preferences.usage_format, Some(UsageDisplayFormat::Table));
    assert_eq!(preferences.monitor, None, "未改过的键不写影子值");
    assert_eq!(preferences.usage_enabled, None);
    assert_eq!(preferences.usage_position, None);
    assert_eq!(preferences.monitor_chart_glyphs, None);
    // 分段控件：图表字形 → 方块，只写 monitor_chart_glyphs。
    state.compose(120, 60).expect("重绘");
    let blocks = hit(&state, |action| {
        matches!(action, Action::ChartGlyphs(ChartGlyphsPreference::Blocks))
    });
    click(&mut state, blocks.x + 1, blocks.y);
    assert_eq!(
        state.observability.chart_glyphs,
        ChartGlyphsPreference::Blocks
    );
    let preferences = &state.config.preferences;
    assert_eq!(
        preferences.monitor_chart_glyphs,
        Some(ChartGlyphsPreference::Blocks)
    );
    assert_eq!(preferences.monitor, None, "图表字形不写进 monitor 键");
    assert_eq!(preferences.usage_enabled, None);
    // 步进器：采样间隔 +1 档，只写 monitor 键。
    state.compose(120, 60).expect("重绘");
    let plus = hit(&state, |action| matches!(action, Action::Interval(1)));
    click(&mut state, plus.x + 1, plus.y);
    assert_eq!(state.observability.monitor.interval_ms, 2000);
    assert_eq!(
        state
            .config
            .preferences
            .monitor
            .as_ref()
            .map(|monitor| monitor.interval_ms),
        Some(2000)
    );
    assert_eq!(state.config.preferences.usage_enabled, None);
    // 开关：厂商账号用量整行可点。
    state.compose(120, 60).expect("重绘");
    let usage = hit(&state, |action| matches!(action, Action::UsageEnabled));
    click(&mut state, usage.x + 1, usage.y);
    assert!(!state.observability.usage.enabled);
    assert_eq!(state.config.preferences.usage_enabled, Some(false));
    assert_eq!(state.config.preferences.usage_position, None);
    // 落盘并可恢复。
    let saved = preferences::load(&path).expect("偏好已写入");
    assert_eq!(saved.usage_format, Some(UsageDisplayFormat::Table));
    assert_eq!(saved.usage_enabled, Some(false));
    assert_eq!(
        saved.monitor_chart_glyphs,
        Some(ChartGlyphsPreference::Blocks)
    );
    let restored = ClientShellState::new(
        ClientShellConfig::from_config(&Config::default()).with_preferences_path(path.clone()),
    );
    assert_eq!(
        restored.observability.usage.format,
        UsageDisplayFormat::Table
    );
    assert!(!restored.observability.usage.enabled);
    assert_eq!(restored.observability.monitor.interval_ms, 2000);
    assert_eq!(
        restored.observability.chart_glyphs,
        ChartGlyphsPreference::Blocks
    );
    std::fs::remove_file(path).expect("remove preferences");
}

fn press_key(state: &mut ClientShellState, code: crossterm::event::KeyCode) -> ClientShellInput {
    state.handle_raw_events(vec![RawInputEvent::Key(crate::input::TerminalKey::new(
        code,
        KeyModifiers::empty(),
    ))])
}

/// 终端正文里一处既不在悬浮层矩形内、也不在任何 agent 行上的点。
fn away_point(state: &ClientShellState) -> (u16, u16) {
    let terminal = terminal_body(state);
    let point = (
        terminal.right().saturating_sub(2),
        terminal.bottom().saturating_sub(2),
    );
    assert!(
        !contains(state.observability.hover_rect, point),
        "离开点 {point:?} 不应落在悬浮层 {:?} 内",
        state.observability.hover_rect
    );
    point
}

/// 默认配置（`usage.position = hover`）下扫过 agent 行即进入悬浮状态机：满
/// `hover_delay_ms` 才可见，移开 250 ms 宽限内移回不关、到期关闭。
#[test]
fn agent_row_hover_is_on_by_default_and_honors_delay_and_leave_grace() {
    let mut state = usage_ready();
    assert_eq!(
        state.observability.usage.position,
        crate::config::UsageDisplayPosition::Hover,
        "默认即开启 agent 行悬浮"
    );
    state.compose(120, 40).expect("工作台");
    let row = agent_row(&state, "pane_1");
    let t0 = Instant::now();
    moved(&mut state, row.x, row.y);
    let early = tick(&mut state, t0 + Duration::from_millis(200));
    assert_eq!(agent_hover(&state), Some((false, false)), "未满延时不可见");
    assert!(usage_calls(&early).is_empty(), "不可见就不发请求");
    let shown = tick(&mut state, t0 + Duration::from_millis(450));
    assert_eq!(agent_hover(&state), Some((true, false)));
    let calls = usage_calls(&shown);
    assert_eq!(calls.len(), 1, "可见即发悬浮层自己的请求: {calls:?}");
    assert_eq!(calls[0].1.pane_id.as_deref(), Some("pane_1"));

    // 移开：进入 250ms 离开宽限；宽限内移回不关。
    state.compose(120, 40).expect("悬浮卡");
    let away = away_point(&state);
    moved(&mut state, away.0, away.1);
    assert!(state
        .observability
        .hover
        .as_ref()
        .is_some_and(|hover| hover.leave_at.is_some()));
    moved(&mut state, row.x, row.y);
    assert!(state
        .observability
        .hover
        .as_ref()
        .is_some_and(|hover| hover.leave_at.is_none()));
    moved(&mut state, away.0, away.1);
    let t1 = Instant::now();
    tick(&mut state, t1 + Duration::from_millis(100));
    assert_eq!(agent_hover(&state), Some((true, false)), "宽限内仍可见");
    tick(&mut state, t1 + Duration::from_millis(300));
    assert!(state.observability.hover.is_none(), "离开 250ms 后关闭");
    assert!(state.observability.hover_scope.accounts.is_empty());

    // 自定义延时同样生效。
    state.observability.usage.hover_delay_ms = 1000;
    let t2 = Instant::now();
    moved(&mut state, row.x, row.y);
    tick(&mut state, t2 + Duration::from_millis(450));
    assert_eq!(agent_hover(&state), Some((false, false)), "1000ms 延时未到");
    tick(&mut state, t2 + Duration::from_millis(1100));
    assert_eq!(agent_hover(&state), Some((true, false)));
}

/// 停留未满延时就离开：离开计时中的卡不再转可见（kit 状态机口径），也就不发
/// 悬浮层请求；宽限到期后整张卡清掉。延时取最短档 200 ms，好让延时先于离开
/// 宽限（250 ms）到期。
#[test]
fn leaving_before_the_hover_delay_never_shows_the_card() {
    let mut state = usage_ready();
    state.observability.usage.hover_delay_ms = 200;
    state.compose(120, 40).expect("工作台");
    let row = agent_row(&state, "pane_1");
    moved(&mut state, row.x, row.y);
    let since = state
        .observability
        .hover
        .as_ref()
        .expect("进入悬浮状态机")
        .since;
    let away = away_point(&state);
    moved(&mut state, away.0, away.1);
    let leave_at = state
        .observability
        .hover
        .as_ref()
        .and_then(|hover| hover.leave_at)
        .expect("离开即开始宽限计时");
    let before_leave = leave_at - Duration::from_millis(10);
    assert!(
        before_leave >= since + Duration::from_millis(200),
        "用例前提：延时先于离开宽限到期"
    );
    let outcome = tick(&mut state, before_leave);
    assert_eq!(
        agent_hover(&state),
        Some((false, false)),
        "离开计时中不再出现"
    );
    assert!(usage_calls(&outcome).is_empty(), "从未可见就不发请求");
    tick(&mut state, leave_at);
    assert!(state.observability.hover.is_none(), "宽限到期清掉");
}

/// 悬浮层的下一个到期时刻（出现 / 离开宽限）进入客户端计时器：事件循环在
/// 到期时醒来，而不是等固定的 100 ms 轮询；已可见且指针在上、钉住时不占计时器。
#[test]
fn hover_deadlines_drive_the_client_timer() {
    let mut state = usage_ready();
    state.compose(120, 40).expect("工作台");
    let row = agent_row(&state, "pane_1");
    moved(&mut state, row.x, row.y);
    let since = state
        .observability
        .hover
        .as_ref()
        .expect("进入悬浮状态机")
        .since;
    assert_eq!(
        state.observability.hover_deadline(),
        Some(since + Duration::from_millis(400)),
        "出现时刻 = 进入 + hover_delay_ms"
    );
    assert!(
        state.timer_delay(since + Duration::from_millis(350)) <= Duration::from_millis(50),
        "离出现还有 50 ms 时计时器不晚于那一刻醒来"
    );
    tick(&mut state, since + Duration::from_millis(400));
    assert_eq!(agent_hover(&state), Some((true, false)));
    assert_eq!(
        state.observability.hover_deadline(),
        None,
        "已可见且指针在上"
    );
    state.compose(120, 40).expect("悬浮卡");
    let away = away_point(&state);
    moved(&mut state, away.0, away.1);
    let leave_at = state
        .observability
        .hover
        .as_ref()
        .and_then(|hover| hover.leave_at)
        .expect("离开宽限");
    assert_eq!(state.observability.hover_deadline(), Some(leave_at));
    // 指针移到卡片上：撤销离开计时（hold），计时器不再为它醒来。
    let card = state.observability.hover_rect;
    moved(&mut state, card.x + 1, card.y + 1);
    assert_eq!(
        state
            .observability
            .hover
            .as_ref()
            .and_then(|hover| hover.leave_at),
        None,
        "指针在卡上撤销离开计时"
    );
    assert_eq!(state.observability.hover_deadline(), None);
}

/// 经典布局（端点不宣告 `client.views.set`，停靠工作台不启用）+ 聚焦 pane 运行
/// claude + 用量方法：悬浮卡走 `composition.rs` 的观测 pass。
fn classic_usage_ready() -> ClientShellState {
    let mut snapshot = snapshot();
    snapshot.agents.push(agent_in_pane("pane_1", "claude"));
    classic_usage_ready_with(snapshot)
}

/// 同 `classic_usage_ready`，快照由调用方给。
fn classic_usage_ready_with(snapshot: ClientShellSnapshot) -> ClientShellState {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot));
    state.set_endpoint_methods(Some(vec![
        "pane.focus".into(),
        "account.usage.get".into(),
        "account.usage.refresh".into(),
        "account.usage.providers".into(),
        "account.binding.set".into(),
    ]));
    state.set_pane_surface(surface());
    state.compose(120, 40).expect("经典布局");
    tick(&mut state, Instant::now());
    assert!(
        !state.workbench.enabled,
        "未宣告 client.views.set 即经典布局"
    );
    state
}

/// 直接放一张已可见的 pane_1 / claude 悬浮卡，锚在 `anchor`。
fn show_hover_at(state: &mut ClientShellState, anchor: Rect) {
    state.observability.hover = Some(Hover {
        target: HoverTarget::Agent {
            endpoint_id: state.active_endpoint_id.clone(),
            pane: "pane_1".into(),
            agent: "claude".into(),
        },
        anchor,
        since: Instant::now(),
        visible: true,
        leave_at: None,
        pinned: false,
    });
}

fn cell_symbol(frame: &FrameData, x: u16, y: u16) -> &str {
    frame.cells[usize::from(y) * usize::from(frame.width) + usize::from(x)]
        .symbol
        .as_str()
}

/// 卡片画在 `card` 上：四角是边框字形，顶边从左框后一格写标题「 claude · …」。
fn assert_card_chrome(state: &ClientShellState, frame: &FrameData, card: Rect) {
    assert_agent_card_chrome(state, frame, card, "claude");
}

/// 同 `assert_card_chrome`，标题是 `agent` 的卡。
fn assert_agent_card_chrome(state: &ClientShellState, frame: &FrameData, card: Rect, agent: &str) {
    let glyphs = state.config.border_glyphs;
    for (x, y, glyph) in [
        (card.x, card.y, glyphs.top_left),
        (card.right() - 1, card.y, glyphs.top_right),
        (card.x, card.bottom() - 1, glyphs.bottom_left),
        (card.right() - 1, card.bottom() - 1, glyphs.bottom_right),
    ] {
        assert_eq!(
            cell_symbol(frame, x, y),
            glyph,
            "卡片 {card:?} 的角 ({x}, {y}): {:?}",
            frame_row(frame, y)
        );
    }
    let end = card.x + 2 + agent.len() as u16;
    let title = (card.x + 2..end)
        .map(|x| cell_symbol(frame, x, card.y))
        .collect::<String>();
    assert_eq!(title, agent, "标题行: {:?}", frame_row(frame, card.y));
}

/// 定位统一走 `kit::hover_card::place_hover_card`：锚点下方左对齐 → 放不下
/// 上翻 → 两侧都不够取大侧收缩；右侧放不下向左平移；永不盖住锚点。宽 / 窄 /
/// 极窄三档都成立（卡片宽 ≤68、高 ≤17，屏幕小时各让出 2 格）。
#[test]
fn agent_hover_card_placement_follows_the_kit_rules_at_every_width() {
    for (cols, rows) in [(120u16, 40u16), (60, 24), (24, 10)] {
        let mut state = classic_usage_ready();
        let size = (
            cols.saturating_sub(2).min(68),
            rows.saturating_sub(2).min(17),
        );
        // 下方放得下：锚点正下方、左对齐。
        let anchor = Rect::new(1, 1, 10, 1);
        show_hover_at(&mut state, anchor);
        let frame = state.compose(cols, rows).expect("悬浮卡");
        let card = state.observability.hover_rect;
        assert_eq!(
            card,
            Rect::new(1, 2, size.0, size.1),
            "{cols}x{rows}: 卡片在锚点正下方、与锚点左对齐"
        );
        assert_card_chrome(&state, &frame, card);
        // 锚点贴底：翻到上方，底边贴住锚点。
        let low = Rect::new(1, rows - 1, 10, 1);
        show_hover_at(&mut state, low);
        let frame = state.compose(cols, rows).expect("上翻的悬浮卡");
        let card = state.observability.hover_rect;
        assert_eq!(card.bottom(), low.y, "{cols}x{rows}: 下方没空间 → 上翻");
        assert_eq!(card.height, size.1);
        assert_card_chrome(&state, &frame, card);
        // 锚点贴右缘：卡片向左平移到右缘内。
        let right = Rect::new(cols - 3, 1, 3, 1);
        show_hover_at(&mut state, right);
        let frame = state.compose(cols, rows).expect("贴右缘的悬浮卡");
        let card = state.observability.hover_rect;
        assert_eq!(card.right(), cols, "{cols}x{rows}: 右侧放不下向左平移");
        assert_eq!(card.width, size.0);
        assert_card_chrome(&state, &frame, card);
        // 锚点在中间、上下都不够：取大侧并收缩高度。
        let middle = Rect::new(1, rows / 2, 10, 1);
        show_hover_at(&mut state, middle);
        state.compose(cols, rows).expect("收缩的悬浮卡");
        let card = state.observability.hover_rect;
        let below = rows - middle.bottom();
        let above = middle.y;
        if size.1 > below && size.1 > above {
            assert_eq!(card.height, below.max(above), "{cols}x{rows}: 取大侧收缩");
        }
        for anchor in [anchor, low, right, middle] {
            show_hover_at(&mut state, anchor);
            state.compose(cols, rows).expect("悬浮卡");
            let card = state.observability.hover_rect;
            assert!(!card.is_empty(), "{cols}x{rows}: {anchor:?} 有卡");
            assert!(
                !card.intersects(anchor),
                "{cols}x{rows}: 卡片 {card:?} 盖住了锚点 {anchor:?}"
            );
            assert_eq!(
                Rect::new(0, 0, cols, rows).intersection(card),
                card,
                "{cols}x{rows}: 卡片越界"
            );
        }
    }
}

/// Agents 面板里全部 agent 行的命中矩形（经典布局 `agents`、停靠工作台
/// `endpoint_agents`）。
fn all_agent_rows(state: &ClientShellState) -> Vec<Rect> {
    state
        .hits
        .agents
        .iter()
        .map(|(rect, _)| *rect)
        .chain(state.hits.endpoint_agents.iter().map(|(rect, _, _)| *rect))
        .collect()
}

/// 卡片不与任何 agent 行相交：卡宽 68、远宽于 Agents 面板，从行的正下 / 正上方
/// 展开会盖住相邻的行，上下扫行时指针会落进卡片被 hold。
fn assert_clear_of_agent_rows(state: &ClientShellState, card: Rect, label: &str) {
    let rows = all_agent_rows(state);
    assert!(rows.len() >= 2, "{label}: 用例前提：面板里有多行");
    for row in rows {
        assert!(
            !card.intersects(row),
            "{label}: 卡片 {card:?} 盖住了 agent 行 {row:?}"
        );
    }
}

/// 两条绘制路径（经典布局的观测 pass、停靠工作台的全局悬浮 pass）下，agent 行
/// 的卡片都放在 Agents 面板右侧：宿主把 kit 的摆放区域收窄到面板右边，纵向仍按
/// kit 规则（紧贴行的下方，放不下上翻），卡片满尺寸且不与任何 agent 行相交。
#[test]
fn agent_row_hover_card_sits_beside_the_agents_panel_in_either_layout() {
    for (label, mut state) in [
        (
            "经典布局",
            classic_usage_ready_with(three_agents_snapshot()),
        ),
        ("停靠工作台", usage_ready_with(three_agents_snapshot())),
    ] {
        state.compose(120, 40).expect("画面");
        let panel = state.hits.agent_body;
        for pane_id in ["pane_1", "pane_2", "pane_3"] {
            let agent = if pane_id == "pane_2" {
                "codex"
            } else {
                "claude"
            };
            state.observability.clear_hover();
            state.compose(120, 40).expect("清掉上一张卡");
            let row = agent_row(&state, pane_id);
            assert!(
                panel.x <= row.x && row.right() <= panel.right(),
                "{label}: 用例前提：行 {row:?} 在面板列表区 {panel:?} 内"
            );
            let t0 = Instant::now();
            moved(&mut state, row.x, row.y);
            tick(&mut state, t0 + Duration::from_millis(450));
            assert_eq!(agent_hover(&state), Some((true, false)), "{label}: 可见");
            let frame = state.compose(120, 40).expect("悬浮卡");
            let card = state.observability.hover_rect;
            assert_eq!(
                (card.width, card.height),
                (68, 17),
                "{label}: {pane_id} 的卡 {card:?} 满尺寸"
            );
            assert!(
                card.x > panel.right(),
                "{label}: 卡片 {card:?} 在面板 {panel:?} 右侧（隔一列）"
            );
            assert!(
                card.y == row.bottom() || card.bottom() == row.y,
                "{label}: 卡片 {card:?} 紧贴行 {row:?} 的下方或上方"
            );
            assert_clear_of_agent_rows(&state, card, label);
            assert_agent_card_chrome(&state, &frame, card, agent);
        }
    }
}

/// 停靠工作台里 Agents 面板停靠到最右侧：右边没有位置，卡片改放面板左侧，
/// 同样不与任何 agent 行相交。
#[test]
fn agent_row_hover_card_opens_left_of_an_agents_panel_docked_on_the_right() {
    let mut state = usage_ready_with(three_agents_snapshot());
    assert!(state
        .workbench
        .dock
        .dock(PanelId::Agents, &PanelId::Terminal(1), Edge::Right));
    state.compose(120, 40).expect("Agents 面板停靠右侧");
    let panel = state.hits.agent_body;
    assert!(
        panel.right() + 40 > 120 && panel.x > 40,
        "用例前提：面板 {panel:?} 贴右、左侧够宽"
    );
    let row = agent_row(&state, "pane_2");
    let t0 = Instant::now();
    moved(&mut state, row.x, row.y);
    tick(&mut state, t0 + Duration::from_millis(450));
    let frame = state.compose(120, 40).expect("悬浮卡");
    let card = state.observability.hover_rect;
    assert!(!card.is_empty(), "画出了悬浮卡");
    assert!(
        card.right() < panel.x,
        "卡片 {card:?} 在面板 {panel:?} 左侧（隔一列）"
    );
    assert!(card.y == row.bottom() || card.bottom() == row.y);
    assert_clear_of_agent_rows(&state, card, "面板停靠右侧");
    assert_agent_card_chrome(&state, &frame, card, "codex");
}

/// 面板两侧都放不下（旁侧不足 40 列）时退回整屏摆放：卡片仍按 kit 规则紧贴
/// 所属行的下方或上方、不盖住该行。65 列是经典布局还有侧栏的最窄宽度（≤64
/// 列走移动布局）。
#[test]
fn agent_row_hover_card_falls_back_to_the_full_screen_when_no_side_fits() {
    let mut state = classic_usage_ready_with(three_agents_snapshot());
    state.compose(65, 24).expect("窄屏");
    let panel = state.hits.agent_body;
    assert!(
        !panel.is_empty() && 65 - (panel.right() + 1) < 40 && panel.x < 40,
        "用例前提：面板 {panel:?} 两侧都不足 40 列"
    );
    let row = agent_row(&state, "pane_1");
    let t0 = Instant::now();
    moved(&mut state, row.x, row.y);
    tick(&mut state, t0 + Duration::from_millis(450));
    let frame = state.compose(65, 24).expect("悬浮卡");
    let card = state.observability.hover_rect;
    assert_eq!(card.width, 63, "整屏宽度让出 2 格");
    assert!(!card.intersects(row), "卡片 {card:?} 不盖所属行 {row:?}");
    assert!(card.y == row.bottom() || card.bottom() == row.y);
    assert_card_chrome(&state, &frame, card);
}

/// 真实尺寸的 pane 表面：帧铺满 `size`，`panes` 是 `(pane_id, 相对表面区的矩形)`。
/// 共用夹具 `surface()` 的 pane 只有 4x2，测不出「锚点是整个 pane」这类问题。
fn sized_surface(size: (u16, u16), panes: &[(&str, Rect)]) -> PaneSurfaceFrame {
    let mut frame = surface();
    let buffer = Buffer::empty(Rect::new(0, 0, size.0, size.1));
    frame.frame = FrameData::from_ratatui_buffer_with_hyperlinks(&buffer, None, &[]);
    let template = frame.panes[0].clone();
    frame.panes = panes
        .iter()
        .map(|(pane_id, rect)| crate::protocol::PaneSurfacePane {
            pane_id: (*pane_id).into(),
            rect: (*rect).into(),
            inner_rect: Rect::new(
                rect.x,
                rect.y + 1,
                rect.width,
                rect.height.saturating_sub(1),
            )
            .into(),
            focused: *pane_id == "pane_1",
            ..template.clone()
        })
        .collect();
    frame
}

/// 整屏单 pane（`split = false`）或上下两个 pane（`split = true`，pane_1 在上、
/// pane_2 在下）铺满终端区；pane_1 跑 claude、pane_2 跑 codex。
fn full_size_panes(size: (u16, u16), split: bool) -> PaneSurfaceFrame {
    let (width, height) = size;
    if split {
        let top = height / 2;
        sized_surface(
            size,
            &[
                ("pane_1", Rect::new(0, 0, width, top)),
                ("pane_2", Rect::new(0, top, width, height - top)),
            ],
        )
    } else {
        sized_surface(size, &[("pane_1", Rect::new(0, 0, width, height))])
    }
}

/// 快照：pane_1 跑 claude（聚焦）、pane_2 跑 codex、pane_3 跑 claude，三个 pane
/// 都在同一 tab 里，Agents 面板列出相邻的三行。
fn three_agents_snapshot() -> ClientShellSnapshot {
    let mut snapshot = snapshot();
    snapshot.agents.push(agent_in_pane("pane_1", "claude"));
    for (pane_id, agent) in [("pane_2", "codex"), ("pane_3", "claude")] {
        let mut entry = agent_in_pane(pane_id, agent);
        entry.focused = false;
        snapshot.agents.push(entry);
        let mut pane = snapshot.panes[0].clone();
        pane.pane_id = pane_id.into();
        pane.focused = false;
        snapshot.panes.push(pane);
    }
    snapshot
}

/// 经典布局或停靠工作台，终端区铺满真实尺寸的 pane（见 `full_size_panes`）。
fn title_hover_state(docked: bool, split: bool) -> ClientShellState {
    let mut state = if docked {
        let mut state = usage_ready_with(three_agents_snapshot());
        let area = terminal_body(&state);
        state.workbench.views.insert(
            "1".into(),
            View {
                tab: "tab_1".into(),
                surface: full_size_panes((area.width, area.height), split),
                graphics: Default::default(),
            },
        );
        state
    } else {
        let mut state = classic_usage_ready_with(three_agents_snapshot());
        let area = state.layout(120, 40).pane_surface;
        state.set_pane_surface(full_size_panes((area.width, area.height), split));
        state
    };
    state.compose(120, 40).expect("画面");
    assert_eq!(state.workbench.enabled, docked);
    let expected = if split { 2 } else { 1 };
    assert_eq!(state.hits.panes.len(), expected, "用例前提：pane 命中区");
    assert!(
        state.hits.panes.iter().all(|hit| hit.rect.height >= 15),
        "用例前提：pane 是真实尺寸: {:?}",
        state
            .hits
            .panes
            .iter()
            .map(|hit| hit.rect)
            .collect::<Vec<_>>()
    );
    state
}

/// CLI 标题悬浮的锚点只是标题行，不是整个 pane：整屏单 pane 与上下分屏、经典
/// 布局与停靠工作台下，卡片都是满尺寸（68x17），紧贴标题行的下方或上方，四角
/// 边框可见；不会被挤成一两行、压到菜单栏上，也不会落到离标题很远的别的 pane 里。
#[test]
fn cli_title_hover_card_is_full_size_next_to_its_title_in_either_layout() {
    for docked in [false, true] {
        for split in [false, true] {
            let label = format!(
                "{} / {}",
                if docked {
                    "停靠工作台"
                } else {
                    "经典布局"
                },
                if split {
                    "上下分屏"
                } else {
                    "整屏单 pane"
                }
            );
            let mut state = title_hover_state(docked, split);
            for hit in state.hits.panes.clone() {
                let agent = if hit.pane_id == "pane_1" {
                    "claude"
                } else {
                    "codex"
                };
                state.observability.clear_hover();
                state.compose(120, 40).expect("清掉上一张卡");
                let title = Rect::new(hit.rect.x, hit.rect.y, hit.rect.width, 1);
                let t0 = Instant::now();
                moved(&mut state, title.x + 2, title.y);
                tick(&mut state, t0 + Duration::from_millis(450));
                assert_eq!(
                    agent_hover(&state),
                    Some((true, false)),
                    "{label}: {} 标题悬浮可见",
                    hit.pane_id
                );
                assert_eq!(
                    state.observability.hover.as_ref().map(|hover| hover.anchor),
                    Some(title),
                    "{label}: 锚点只是 {} 的标题行",
                    hit.pane_id
                );
                let frame = state.compose(120, 40).expect("悬浮卡");
                let card = state.observability.hover_rect;
                assert_eq!(
                    (card.width, card.height),
                    (68, 17),
                    "{label}: {} 的卡 {card:?} 是满尺寸",
                    hit.pane_id
                );
                assert!(
                    card.y == title.bottom() || card.bottom() == title.y,
                    "{label}: 卡 {card:?} 紧贴标题行 {title:?} 的下方或上方"
                );
                assert!(!card.intersects(title), "{label}: 卡不盖标题行");
                assert_agent_card_chrome(&state, &frame, card, agent);
            }
        }
    }
}

/// 放不下一张可读的卡（上下边框 + 至少一行正文 + 间隔 + 「打开页面」= 5 行）
/// 时整张不画：不留看不见却独占鼠标输入的命中区，也就不会平白发悬浮请求。
#[test]
fn hover_card_too_short_to_read_is_not_drawn() {
    let mut state = classic_usage_ready();
    // 锚点盖住除最底 4 行外的整屏：下方只有 4 行、上方没有空间。
    show_hover_at(&mut state, Rect::new(0, 0, 120, 36));
    let frame = state.compose(120, 40).expect("画面");
    assert_eq!(state.observability.hover_rect, Rect::default(), "4 行不画");
    assert!(state.observability.hover_hits.is_empty(), "不留命中区");
    let glyphs = state.config.border_glyphs;
    assert_ne!(
        cell_symbol(&frame, 0, 36),
        glyphs.top_left,
        "底部没有残缺的卡片边框: {:?}",
        frame_row(&frame, 36)
    );
    // 恰好 5 行：画得出，四角与标题都在。
    show_hover_at(&mut state, Rect::new(0, 0, 120, 35));
    let frame = state.compose(120, 40).expect("画面");
    let card = state.observability.hover_rect;
    assert_eq!(card, Rect::new(0, 35, 68, 5), "5 行照画");
    assert_card_chrome(&state, &frame, card);
    assert!(
        !state.observability.hover_hits.is_empty(),
        "「打开页面」可点"
    );
}

#[test]
fn pinned_agent_hover_survives_leaving_and_closes_on_esc_or_outside_click() {
    let mut snapshot = snapshot();
    snapshot.agents.push(agent_in_pane("pane_1", "claude"));
    let mut other = agent_in_pane("pane_2", "codex");
    other.focused = false;
    snapshot.agents.push(other);
    let mut state = docked_with(snapshot);
    tick(&mut state, Instant::now());
    state.compose(120, 40).expect("工作台");
    pin_agent_hover(&mut state, "pane_1", "claude");
    assert_eq!(agent_hover(&state), Some((true, true)));
    // 钉住后离开不再关闭。
    state.compose(120, 40).expect("钉住的浮层");
    let away = away_point(&state);
    moved(&mut state, away.0, away.1);
    let t1 = Instant::now();
    tick(&mut state, t1 + Duration::from_secs(2));
    assert_eq!(agent_hover(&state), Some((true, true)), "钉住后离开不关");
    // 钉住后扫过别的 agent 行也不被替换。
    let other_row = agent_row(&state, "pane_2");
    moved(&mut state, other_row.x, other_row.y);
    assert!(
        matches!(
            state.observability.hover.as_ref().map(|hover| &hover.target),
            Some(HoverTarget::Agent { pane, .. }) if pane == "pane_1"
        ),
        "钉住的浮层不被别的 agent 行悬浮替换"
    );
    assert_eq!(agent_hover(&state), Some((true, true)));
    // Esc 关闭钉住的浮层。
    press_key(&mut state, crossterm::event::KeyCode::Esc);
    assert!(state.observability.hover.is_none(), "Esc 关闭钉住的浮层");
    // 钉住后在浮层外点击 → 关闭。
    state.compose(120, 40).expect("重绘");
    pin_agent_hover(&mut state, "pane_1", "claude");
    state.compose(120, 40).expect("钉住的浮层");
    let away = away_point(&state);
    click(&mut state, away.0, away.1);
    assert!(state.observability.hover.is_none(), "浮层外点击关闭");
}

/// 右键 agent 行打开菜单；返回菜单条目里「用量」的下标（断言它可点）。
fn open_agent_menu_at_usage(state: &mut ClientShellState, pane_id: &str) -> usize {
    let row = agent_row(state, pane_id);
    state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Right),
        column: row.x + 1,
        row: row.y,
        modifiers: KeyModifiers::NONE,
    })]);
    let Some(ClientShellOverlay::ContextMenu(menu)) = state.overlay.as_ref() else {
        panic!("agent 行右键应打开菜单: {:?}", state.overlay);
    };
    let items = menu.items();
    let usage = items
        .iter()
        .position(|item| item.action == ClientContextMenuAction::ShowAgentUsage)
        .expect("菜单里有「用量」");
    assert!(items[usage].enabled, "「用量」已接通，可点");
    usage
}

/// 钉住的用量卡画在屏幕上：边框与标题字符、标题栏的「{agent} · 用量」与钉住
/// 标记都在卡片顶边。
fn assert_pinned_card_drawn(state: &mut ClientShellState, cols: u16, rows: u16) {
    let frame = state.compose(cols, rows).expect("钉住的用量卡");
    let card = state.observability.hover_rect;
    assert!(!card.is_empty(), "{cols}x{rows}: 钉住的卡已画出");
    assert_card_chrome(state, &frame, card);
    let texts = &crate::i18n::texts().agent_panel;
    let compact = |text: &str| {
        text.chars()
            .filter(|ch| !ch.is_whitespace())
            .collect::<String>()
    };
    let top = compact(&frame_row(&frame, card.y));
    let title = crate::i18n::fill(texts.usage_card_title_fmt, &[("agent", "claude")]);
    assert!(
        top.contains(&compact(&title)),
        "{cols}x{rows}: 标题 {title:?} 在顶边: {top:?}"
    );
    if card.width >= 30 {
        assert!(
            top.contains(&compact(texts.usage_pinned)),
            "{cols}x{rows}: 钉住标记 {:?} 在顶边: {top:?}",
            texts.usage_pinned
        );
    }
}

/// 键盘路径：右键 agent 行 → ↓ 移到「用量」→ Enter：菜单关闭，卡片立即可见并
/// 钉住，锚在该 agent 行；悬浮层随即发自己的用量请求（带该 pane）。停靠工作台。
#[test]
fn context_menu_usage_pins_the_agent_card_by_keyboard_in_the_docked_layout() {
    let mut state = usage_ready();
    state.compose(120, 40).expect("工作台");
    let row = agent_row(&state, "pane_1");
    let usage = open_agent_menu_at_usage(&mut state, "pane_1");
    for _ in 0..8 {
        let Some(ClientShellOverlay::ContextMenu(menu)) = state.overlay.as_ref() else {
            panic!("菜单仍打开");
        };
        if menu.highlighted == usage {
            break;
        }
        press_key(&mut state, crossterm::event::KeyCode::Down);
    }
    press_key(&mut state, crossterm::event::KeyCode::Enter);
    assert!(state.overlay.is_none(), "激活后菜单关闭");
    assert_eq!(agent_hover(&state), Some((true, true)), "立即可见并钉住");
    let hover = state.observability.hover.as_ref().expect("钉住的卡");
    assert_eq!(hover.anchor, row, "锚在该 agent 行");
    assert!(matches!(
        &hover.target,
        HoverTarget::Agent { pane, agent, .. } if pane == "pane_1" && agent == "claude"
    ));
    let calls = usage_calls(&tick(&mut state, Instant::now()));
    assert_eq!(calls.len(), 1, "钉住即发悬浮层自己的请求: {calls:?}");
    assert_eq!(calls[0].1.pane_id.as_deref(), Some("pane_1"));
    assert_pinned_card_drawn(&mut state, 120, 40);
    assert!(!state.observability.hover_rect.intersects(row), "不盖住行");
    let panel = state.hits.agent_body;
    assert!(
        state.observability.hover_rect.x > panel.right(),
        "钉住的卡同样放在 Agents 面板右侧"
    );
}

/// 鼠标路径 + 经典布局：点菜单里的「用量」行同样钉住并画出卡片；宽 / 窄 / 极窄
/// 三档都画得出。
#[test]
fn context_menu_usage_click_pins_the_agent_card_in_the_classic_layout() {
    let mut state = classic_usage_ready();
    let usage = open_agent_menu_at_usage(&mut state, "pane_1");
    state.compose(120, 40).expect("菜单");
    let usage_row = state
        .hits
        .context_menu_rows
        .iter()
        .find(|(_, index)| *index == usage)
        .map(|(rect, _)| *rect)
        .expect("「用量」行可点");
    click(&mut state, usage_row.x + 1, usage_row.y);
    assert!(state.overlay.is_none());
    assert_eq!(agent_hover(&state), Some((true, true)));
    for (cols, rows) in [(120, 40), (60, 24), (30, 12)] {
        assert_pinned_card_drawn(&mut state, cols, rows);
    }
}

/// 钉住入口是用户的显式动作：`usage.position = page`（关掉指针悬浮）时照样
/// 打开；指针离开、扫过别的行都不关；换 agent 换卡；再对同一 agent 调用即关闭
/// （toggle）。
#[test]
fn pinned_usage_card_ignores_page_position_and_toggles_on_the_same_agent() {
    let mut snapshot = snapshot();
    snapshot.agents.push(agent_in_pane("pane_1", "claude"));
    let mut other = agent_in_pane("pane_2", "codex");
    other.focused = false;
    snapshot.agents.push(other);
    let mut state = docked_with(snapshot);
    tick(&mut state, Instant::now());
    state.observability.usage.position = crate::config::UsageDisplayPosition::Page;
    state.compose(120, 40).expect("工作台");
    // 指针悬浮在 page 模式下关闭。
    let row = agent_row(&state, "pane_1");
    moved(&mut state, row.x, row.y);
    assert!(state.observability.hover.is_none(), "page 模式没有指针悬浮");
    let pin = |state: &mut ClientShellState, pane: &str| {
        let mut outcome = ClientShellInput::default();
        let endpoint_id = state.active_endpoint_id.clone();
        state.activate_agent_context_action(
            endpoint_id,
            super::super::agent_activity_overlay::AgentActivityOwner::Pane {
                pane_id: pane.into(),
            },
            ClientContextMenuAction::ShowAgentUsage,
            &mut outcome,
        );
        outcome
    };
    let outcome = pin(&mut state, "pane_1");
    assert!(outcome.repaint);
    assert_eq!(agent_hover(&state), Some((true, true)), "page 模式照样钉住");
    assert_pinned_card_drawn(&mut state, 120, 40);
    let away = away_point(&state);
    moved(&mut state, away.0, away.1);
    let other_row = agent_row(&state, "pane_2");
    if !state.observability.hover_rect.intersects(other_row) {
        moved(&mut state, other_row.x, other_row.y);
    }
    tick(&mut state, Instant::now() + Duration::from_secs(2));
    assert_eq!(
        agent_hover(&state),
        Some((true, true)),
        "离开、扫过别的行都不关"
    );
    // 另一个 agent：换成它的卡（仍钉住），作用域跟着换。
    pin(&mut state, "pane_2");
    assert!(matches!(
        state.observability.hover.as_ref().map(|hover| &hover.target),
        Some(HoverTarget::Agent { pane, agent, .. }) if pane == "pane_2" && agent == "codex"
    ));
    assert_eq!(agent_hover(&state), Some((true, true)));
    assert_eq!(
        state.observability.hover_scope.provider.as_deref(),
        Some("codex"),
        "作用域跟着换"
    );
    // 同一 agent 再来一次：关闭。
    let outcome = pin(&mut state, "pane_2");
    assert!(outcome.repaint);
    assert!(
        state.observability.hover.is_none(),
        "同一 agent 再次调用即关闭"
    );
    assert_eq!(
        state.observability.hover_scope.provider, None,
        "作用域一并复位"
    );
}

/// 已被指针悬浮打开的同一张卡：原地钉住，不重置作用域（已拿到的数据与在途请求
/// 保留）。
#[test]
fn pinning_the_card_already_under_the_pointer_keeps_its_scope() {
    let mut state = usage_ready();
    state.compose(120, 40).expect("工作台");
    let row = agent_row(&state, "pane_1");
    let t0 = Instant::now();
    moved(&mut state, row.x, row.y);
    tick(&mut state, t0 + Duration::from_millis(450));
    assert_eq!(agent_hover(&state), Some((true, false)));
    let epoch = state.observability.hover_scope.epoch;
    pin_agent_hover(&mut state, "pane_1", "claude");
    assert_eq!(agent_hover(&state), Some((true, true)), "原地钉住");
    assert_eq!(state.observability.hover_scope.epoch, epoch, "作用域不换代");
}

/// 该 agent 的行不在画面上（滚出视口 / 面板折叠）：锚在 Agents 面板列表顶部
/// （零高锚线），卡片按 kit 规则贴着这条线向下展开，下方放不下则翻到上方。
#[test]
fn pinning_an_agent_without_a_visible_row_anchors_at_the_panel_top() {
    let mut state = usage_ready();
    state.compose(120, 40).expect("工作台");
    let body = state.hits.agent_body;
    assert!(!body.is_empty(), "用例前提：Agents 面板列表区已画出");
    pin_agent_hover(&mut state, "pane_9", "claude");
    let hover = state.observability.hover.as_ref().expect("钉住的卡");
    assert_eq!(hover.anchor, Rect::new(body.x, body.y, body.width, 0));
    state.compose(120, 40).expect("钉住的卡");
    let card = state.observability.hover_rect;
    assert!(
        card.y == body.y || card.bottom() == body.y,
        "卡片 {card:?} 贴着面板顶边 {body:?} 展开"
    );
}

/// 经典布局下监控页面打开时，钉住的卡照样画在页面之上（指针悬浮不画）；Esc
/// 先关钉住的卡、页面不动；点页面控件也算点外。
#[test]
fn pinned_card_draws_over_the_classic_monitor_page_and_esc_closes_it_first() {
    let mut state = classic_usage_ready();
    state.open_observation_page(Page::Monitor, &mut ClientShellInput::default());
    state.compose(120, 40).expect("监控页面");
    assert!(
        !state.observability.page_rect.is_empty(),
        "经典布局页面已画出"
    );
    pin_agent_hover(&mut state, "pane_1", "claude");
    assert_pinned_card_drawn(&mut state, 120, 40);
    press_key(&mut state, crossterm::event::KeyCode::Esc);
    assert!(state.observability.hover.is_none(), "Esc 先关钉住的卡");
    assert_eq!(state.observability.page, Some(Page::Monitor), "页面不动");
    // 点页面控件（页签）：同样关闭钉住的卡。
    pin_agent_hover(&mut state, "pane_1", "claude");
    state.compose(120, 40).expect("钉住的卡");
    let hover_rect = state.observability.hover_rect;
    let tab = page_hit(&state, |action| {
        matches!(action, Action::Page(Page::Settings))
    })
    .expect("偏好页页签");
    assert!(
        !hover_rect.intersects(tab),
        "用例前提：页签 {tab:?} 没被卡片 {hover_rect:?} 盖住"
    );
    click(&mut state, tab.x + 1, tab.y);
    assert!(state.observability.hover.is_none(), "点页面控件即点外");
}

/// 账号用量在设置里关闭时，钉住的卡照实说明，而不是一直停在「刷新中…」。
#[test]
fn pinned_card_explains_that_usage_is_disabled() {
    let mut state = usage_ready();
    state.observability.usage.enabled = false;
    state.compose(120, 40).expect("工作台");
    pin_agent_hover(&mut state, "pane_1", "claude");
    let calls = usage_calls(&tick(&mut state, Instant::now()));
    assert!(calls.is_empty(), "关闭时不发请求");
    let frame = state.compose(120, 40).expect("钉住的卡");
    let card = state.observability.hover_rect;
    let body = (card.y..card.bottom())
        .map(|y| frame_row(&frame, y))
        .collect::<Vec<_>>()
        .join("\n");
    let compact = body
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .collect::<String>();
    assert!(
        body.contains("Account usage is disabled in settings.")
            || compact.contains("账号用量已在设置中关闭。"),
        "卡片说明用量已关闭:\n{body}"
    );
    assert!(
        !body.contains("Refreshing…") && !compact.contains("刷新中…"),
        "不停在刷新中:\n{body}"
    );
}

#[test]
fn keyboard_navigation_never_activates_the_pinned_hover() {
    let mut state = usage_ready();
    state.open_observation_page(Page::Monitor, &mut ClientShellInput::default());
    state.compose(120, 40).expect("监控面板");
    pin_agent_hover(&mut state, "pane_1", "claude");
    state.compose(120, 40).expect("页面 + 钉住的浮层同帧");
    let hover_rect = state.observability.hover_rect;
    assert!(!hover_rect.is_empty());
    // 「落进浮层」= 高亮到浮层自己的命中区；被浮层盖住的页面控件（如页脚
    // 键位提示）仍是页面控件。
    let hover_hits = state.observability.hover_hits.clone();
    let inside = |rect: Rect| hover_hits.iter().any(|(hit, _)| *hit == rect);
    let page_hits = state.observability.page_hits;
    assert!(page_hits > 0, "页面有可 Tab 的控件");
    assert!(
        state.observability.hits.len() > page_hits,
        "总表仍列举浮层命中区（既有契约）"
    );
    let highlighted = |state: &ClientShellState| {
        state
            .observability
            .hits
            .get(state.observability.selected_hit)
            .map(|(rect, _)| *rect)
            .expect("高亮项存在")
    };
    // Tab 走完一整圈：高亮始终落在页面控件上，从不落进浮层。
    for step in 1..=page_hits {
        press_key(&mut state, crossterm::event::KeyCode::Tab);
        let rect = highlighted(&state);
        assert!(!inside(rect), "第 {step} 次 Tab 落进浮层: {rect:?}");
    }
    // 走完一圈后 Enter 激活的是页面控件，不是浮层的「打开页面」。
    press_key(&mut state, crossterm::event::KeyCode::Enter);
    assert_ne!(
        state.observability.page,
        Some(Page::Accounts),
        "Enter 不能激活浮层命中区"
    );
    // 反向同样不进浮层。
    state.compose(120, 40).expect("重绘");
    if state.observability.hover.is_none() {
        pin_agent_hover(&mut state, "pane_1", "claude");
        state.compose(120, 40).expect("重新钉住");
    }
    press_key(&mut state, crossterm::event::KeyCode::BackTab);
    let rect = highlighted(&state);
    assert!(!inside(rect), "BackTab 落进浮层: {rect:?}");
}

#[test]
fn clicking_open_page_inside_the_agent_hover_opens_the_accounts_page() {
    let mut state = usage_ready();
    deliver_providers(
        &mut state,
        vec![
            provider("claude", &["claude:default"]),
            provider("codex", &["codex:default"]),
        ],
    );
    state.compose(120, 40).expect("工作台");
    let row = agent_row(&state, "pane_1");
    let t0 = Instant::now();
    moved(&mut state, row.x, row.y);
    tick(&mut state, t0 + Duration::from_millis(450));
    assert_eq!(agent_hover(&state), Some((true, false)));
    assert!(deliver_hover_usage(
        &mut state,
        vec![account_with_percent("claude", "claude:default", 42.0)]
    ));
    state.compose(120, 40).expect("悬浮卡");
    assert!(!state.observability.hover_rect.is_empty());
    assert!(
        !state.observability.hover_scope.accounts.is_empty(),
        "悬浮层作用域已有数据"
    );
    let scope_epoch = state.observability.hover_scope.epoch;
    let open_page = state
        .observability
        .hover_hits
        .iter()
        .find(|(_, action)| matches!(action, Action::Page(Page::Accounts)))
        .map(|(rect, _)| *rect)
        .expect("悬浮卡底行有「打开页面」");
    click(&mut state, open_page.x, open_page.y);
    assert_eq!(
        state.observability.page,
        Some(Page::Accounts),
        "「打开页面」打开账号页"
    );
    assert!(state.observability.hover.is_none(), "打开页面即关闭浮层");
    assert!(
        state.observability.hover_scope.accounts.is_empty(),
        "浮层作用域不残留数据"
    );
    assert_eq!(state.observability.hover_scope.provider, None);
    assert_eq!(state.observability.hover_scope.endpoint, None);
    assert!(
        state.observability.hover_scope.epoch > scope_epoch,
        "作用域换代，在途悬浮层响应作废"
    );
}

// ---------------------------------------------------------------------------
// C-2 偏好影子真源：偏好按键上锁、「恢复配置文件值」、disabled_providers 双真源
// （TOML = 服务端底线，客户端偏好 = 本机覆盖，总览逐厂商轮询并按厂商合并）
// ---------------------------------------------------------------------------

/// 页面作用域某一厂商的用量响应（对应 `agent=Some(x)` 的逐厂商请求）。
fn deliver_usage_for(
    state: &mut ClientShellState,
    agent: &str,
    accounts: Vec<AccountUsageSnapshot>,
) -> bool {
    let epoch = state.observability.epoch;
    state.receive_observation(
        epoch,
        Purpose::Usage {
            agent: Some(agent.into()),
        },
        Ok(ResponseResult::AccountUsage {
            accounts,
            refresh: None,
        }),
    )
}

fn sorted_agents(calls: &[(bool, UsageParams)]) -> Vec<Option<String>> {
    let mut agents = calls
        .iter()
        .map(|(_, params)| params.agent.clone())
        .collect::<Vec<_>>();
    agents.sort();
    agents
}

fn account_ids(accounts: &[AccountUsageSnapshot]) -> Vec<&str> {
    accounts
        .iter()
        .map(|account| account.account_id.as_str())
        .collect()
}

/// 设置页每个动作只回写自己的偏好键：勾掉一个厂商不能把 enabled / format /
/// position 从 None 一次性固化成当前值（否则 config.toml 的后续修改被影子值遮住）。
#[test]
fn settings_actions_persist_only_their_own_usage_key() {
    let path = std::env::temp_dir().join(format!(
        "herdr-shell-usage-key-lock-{}.json",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let mut state = ClientShellState::new(
        ClientShellConfig::from_config(&Config::default()).with_preferences_path(path.clone()),
    );
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    state.observation_action(
        Action::ProviderEnabled("codex".into()),
        &mut ClientShellInput::default(),
    );
    let preferences = &state.config.preferences;
    assert_eq!(
        preferences.usage_disabled_providers.as_deref(),
        Some(&["codex".to_owned()][..])
    );
    assert_eq!(preferences.usage_enabled, None, "未改过的键不写影子值");
    assert_eq!(preferences.usage_format, None);
    assert_eq!(preferences.usage_position, None);
    assert_eq!(preferences.usage_hover_delay_ms, None);
    assert!(state.observability.usage_overridden, "已有本机覆盖");

    state.observation_action(
        Action::UsageFormat(crate::config::UsageDisplayFormat::Table),
        &mut ClientShellInput::default(),
    );
    let preferences = &state.config.preferences;
    assert_eq!(
        preferences.usage_format,
        Some(crate::config::UsageDisplayFormat::Table)
    );
    assert_eq!(preferences.usage_enabled, None);
    assert_eq!(preferences.usage_position, None);
    let saved = preferences::load(&path).expect("偏好已写入");
    assert_eq!(saved.usage_enabled, None);
    assert_eq!(saved.usage_position, None);
    assert_eq!(
        saved.usage_format,
        Some(crate::config::UsageDisplayFormat::Table)
    );
    assert_eq!(
        saved.usage_disabled_providers,
        Some(vec!["codex".to_owned()])
    );
    std::fs::remove_file(path).expect("remove preferences");
}

/// 设置页「恢复配置文件值」：清掉全部 usage_* 本机覆盖并按 config.toml 重载；
/// 没有本机覆盖时该行只是说明、没有命中区。
#[test]
fn restore_config_values_clears_usage_overrides_and_reloads_the_config_file() {
    let path = std::env::temp_dir().join(format!(
        "herdr-shell-usage-restore-{}.json",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let mut state = usage_ready();
    state.config.account_usage.format = crate::config::UsageDisplayFormat::Table;
    state.config.account_usage.disabled_providers = vec!["kimi".into()];
    state.observability.reload_preferences(&state.config);
    state.config.preferences_path = Some(path.clone());
    state.open_observation_page(Page::Settings, &mut ClientShellInput::default());
    state.compose(120, 40).expect("设置页");
    assert!(
        page_hit(&state, |action| matches!(
            action,
            Action::RestoreUsagePreferences
        ))
        .is_none(),
        "无本机覆盖时「恢复配置文件值」不可点"
    );

    state.observation_action(
        Action::UsageFormat(crate::config::UsageDisplayFormat::Dashboard),
        &mut ClientShellInput::default(),
    );
    state.observation_action(
        Action::ProviderEnabled("codex".into()),
        &mut ClientShellInput::default(),
    );
    state.observation_action(Action::HoverDelay(1), &mut ClientShellInput::default());
    assert_eq!(
        state.observability.usage.format,
        crate::config::UsageDisplayFormat::Dashboard
    );
    assert_eq!(
        state.observability.usage.disabled_providers,
        ["kimi", "codex"]
    );
    assert_eq!(state.observability.usage.hover_delay_ms, 800);
    assert!(state.observability.usage_overridden);
    state.compose(120, 40).expect("设置页");
    let restore = page_hit(&state, |action| {
        matches!(action, Action::RestoreUsagePreferences)
    })
    .expect("有本机覆盖时「恢复配置文件值」可点");
    let outcome = click(&mut state, restore.x + 1, restore.y);
    assert!(outcome.repaint);

    assert_eq!(
        state.observability.usage.format,
        crate::config::UsageDisplayFormat::Table,
        "回到 config.toml 的值而不是内置默认"
    );
    assert_eq!(state.observability.usage.disabled_providers, ["kimi"]);
    assert_eq!(state.observability.usage.hover_delay_ms, 400);
    assert!(!state.observability.usage_overridden);
    let preferences = &state.config.preferences;
    assert_eq!(preferences.usage_enabled, None);
    assert_eq!(preferences.usage_format, None);
    assert_eq!(preferences.usage_position, None);
    assert_eq!(preferences.usage_disabled_providers, None);
    assert_eq!(preferences.usage_hover_delay_ms, None);
    let saved = preferences::load(&path).expect("偏好已重写");
    assert_eq!(saved.usage_format, None);
    assert_eq!(saved.usage_disabled_providers, None);
    assert_eq!(saved.usage_hover_delay_ms, None);
    state.compose(120, 40).expect("设置页");
    assert!(
        page_hit(&state, |action| matches!(
            action,
            Action::RestoreUsagePreferences
        ))
        .is_none(),
        "恢复后该行回到说明态"
    );
    std::fs::remove_file(path).expect("remove preferences");
}

/// 进入账号页的跨厂商总览态（未选厂商）并视为强意图刷新：打开账号页、在 `at`
/// tick 一次让「页面首次可见」的自动选厂商走完，再显式回到总览
/// （`Action::Overview`）。返回那次 tick 的输出：页面首次可见时的厂商列表请求
/// 与订阅请求在这里。
fn open_accounts_overview(state: &mut ClientShellState, at: Instant) -> ClientShellInput {
    state.open_observation_page(Page::Accounts, &mut ClientShellInput::default());
    let opened = tick(state, at);
    state.observation_action(Action::Overview, &mut ClientShellInput::default());
    assert_eq!(
        state.observability.selected_provider, None,
        "总览态不选厂商"
    );
    opened
}

/// 跨厂商总览按本机 disabled 过滤后逐厂商请求（`agent=Some(x)`），响应按厂商合并：
/// 一个厂商的新数据不冲掉其它厂商，顺序按厂商列表稳定；服务端（TOML 未关闭）仍
/// 返回的本机已关闭厂商在响应侧丢弃。
#[test]
fn overview_polls_each_enabled_provider_separately_and_merges_by_agent() {
    let mut state = usage_ready();
    state.observability.usage.disabled_providers = vec!["kimi".into()];
    deliver_providers(
        &mut state,
        vec![
            provider("claude", &["claude:default"]),
            provider("codex", &["codex:default"]),
            provider("kimi", &["kimi:default"]),
        ],
    );
    let t0 = Instant::now() + Duration::from_secs(1);
    open_accounts_overview(&mut state, t0 - Duration::from_millis(500));
    let calls = usage_calls(&tick(&mut state, t0));
    assert!(
        calls.iter().all(|(manual, _)| *manual),
        "回到总览 = 强意图刷新"
    );
    assert!(calls.iter().all(|(_, params)| params.pane_id.is_none()));
    assert_eq!(
        sorted_agents(&calls),
        [Some("claude".to_owned()), Some("codex".to_owned())],
        "逐厂商请求，本机关闭的 kimi 不发: {calls:?}"
    );
    assert!(state.observability.refreshing());
    assert!(deliver_usage_for(
        &mut state,
        "codex",
        vec![account("codex", "codex:default")]
    ));
    assert!(state.observability.refreshing(), "另一厂商的刷新仍在途");
    assert!(deliver_usage_for(
        &mut state,
        "claude",
        vec![account_with_percent("claude", "claude:default", 10.0)]
    ));
    assert!(
        !state.observability.refreshing(),
        "全部厂商到齐才结束刷新中"
    );
    assert_eq!(
        account_ids(&state.observability.accounts),
        ["claude:default", "codex:default"],
        "合并后按厂商列表顺序排列，不受响应到达顺序影响"
    );

    // 下一轮：只到一个厂商的新数据，其它厂商保留。
    let calls = usage_calls(&tick(&mut state, t0 + Duration::from_secs(2)));
    assert_eq!(calls.len(), 2, "普通轮询同样逐厂商: {calls:?}");
    assert!(calls.iter().all(|(manual, _)| !*manual));
    assert!(deliver_usage_for(
        &mut state,
        "claude",
        vec![account_with_percent("claude", "claude:default", 20.0)]
    ));
    assert_eq!(
        account_ids(&state.observability.accounts),
        ["claude:default", "codex:default"]
    );
    assert_eq!(
        state.observability.accounts[0].metrics[0].used_percent,
        Some(20.0),
        "同厂商按响应整体替换"
    );

    // 服务端 TOML 没关 kimi 时仍会返回它：本机关闭的厂商不落入作用域。
    assert!(deliver_usage_for(
        &mut state,
        "kimi",
        vec![account("kimi", "kimi:default")]
    ));
    assert_eq!(
        account_ids(&state.observability.accounts),
        ["claude:default", "codex:default"]
    );
}

/// 厂商列表尚未到达时账号页不发 `agent=None` 的整体请求（那会让服务端探测本机
/// 关闭的厂商）：等列表到达后立刻发出，期间点的「刷新」（强意图）不丢。
#[test]
fn accounts_page_waits_for_the_provider_list_before_requesting_usage() {
    let mut state = usage_ready();
    state.open_observation_page(Page::Accounts, &mut ClientShellInput::default());
    state.observation_action(Action::Refresh, &mut ClientShellInput::default());
    let t0 = Instant::now() + Duration::from_secs(1);
    let opened = tick(&mut state, t0);
    assert_eq!(providers_calls(&opened), 1, "先拉厂商列表");
    assert!(
        usage_calls(&opened).is_empty(),
        "列表在途时不发整体请求: {:?}",
        usage_calls(&opened)
    );
    assert!(state.observability.refreshing(), "强意图刷新保留");
    deliver_providers(
        &mut state,
        vec![
            provider("codex", &["codex:default"]),
            provider("claude", &["claude:default"]),
        ],
    );
    let calls = usage_calls(&tick(&mut state, t0 + Duration::from_millis(50)));
    assert_eq!(
        sorted_agents(&calls),
        [Some("claude".to_owned())],
        "列表到达后自动选中聚焦 pane 的厂商并立刻发出: {calls:?}"
    );
    assert!(calls.iter().all(|(manual, _)| *manual), "仍是强意图刷新");
}

/// 端点不宣告厂商列表方法（旧 server）：总览回落到一次 `agent=None` 的整体请求，
/// 响应侧仍按本机关闭过滤。
#[test]
fn overview_falls_back_to_a_single_request_without_a_provider_list_method() {
    let mut state = usage_ready();
    state.set_endpoint_methods(Some(vec![
        "client.views.set".into(),
        "tab.focus".into(),
        "account.usage.get".into(),
        "account.usage.refresh".into(),
    ]));
    state.observability.usage.disabled_providers = vec!["kimi".into()];
    let t0 = Instant::now() + Duration::from_secs(1);
    open_accounts_overview(&mut state, t0 - Duration::from_millis(500));
    let calls = usage_calls(&tick(&mut state, t0));
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(calls[0].1.agent, None);
    assert!(deliver_usage(
        &mut state,
        vec![
            account("claude", "claude:default"),
            account("kimi", "kimi:default"),
        ]
    ));
    assert_eq!(
        account_ids(&state.observability.accounts),
        ["claude:default"],
        "整体响应也按本机关闭过滤"
    );
}

/// 设置里关掉一个厂商：两个作用域里它的账号立即消失、不再向它发请求；重新
/// 启用即刻补查它。
#[test]
fn disabling_a_provider_drops_its_accounts_and_enabling_polls_it_again() {
    let mut state = usage_ready();
    deliver_providers(
        &mut state,
        vec![
            provider("claude", &["claude:default"]),
            provider("codex", &["codex:default"]),
        ],
    );
    let t0 = Instant::now() + Duration::from_secs(1);
    open_accounts_overview(&mut state, t0 - Duration::from_millis(500));
    assert_eq!(usage_calls(&tick(&mut state, t0)).len(), 2);
    assert!(deliver_usage_for(
        &mut state,
        "claude",
        vec![account("claude", "claude:default")]
    ));
    assert!(deliver_usage_for(
        &mut state,
        "codex",
        vec![account("codex", "codex:default")]
    ));
    assert_eq!(state.observability.accounts.len(), 2);

    state.observation_action(
        Action::ProviderEnabled("codex".into()),
        &mut ClientShellInput::default(),
    );
    assert_eq!(
        account_ids(&state.observability.accounts),
        ["claude:default"],
        "关掉的厂商账号立即离开页面作用域"
    );
    let calls = usage_calls(&tick(&mut state, t0 + Duration::from_secs(2)));
    assert_eq!(sorted_agents(&calls), [Some("claude".to_owned())]);

    state.observation_action(
        Action::ProviderEnabled("codex".into()),
        &mut ClientShellInput::default(),
    );
    let calls = usage_calls(&tick(&mut state, t0 + Duration::from_millis(2_010)));
    assert_eq!(
        sorted_agents(&calls),
        [Some("codex".to_owned())],
        "重新启用立刻补查该厂商（claude 仍在途）: {calls:?}"
    );
}

/// 订阅推送（服务端按 TOML 过滤）里本机关闭的厂商不合并进页面。
#[test]
fn usage_events_for_a_client_disabled_provider_are_ignored() {
    let mut state = subscribing_ready();
    state.observability.usage.disabled_providers = vec!["codex".into()];
    let t0 = Instant::now() + Duration::from_secs(1);
    let opened = open_accounts_overview(&mut state, t0);
    deliver_providers(
        &mut state,
        vec![
            provider("claude", &["claude:default"]),
            provider("codex", &["codex:default"]),
        ],
    );
    assert_eq!(subscribe_calls(&opened).len(), 1, "总览态订阅");
    assert!(deliver_subscription(&mut state, "usage-1", true));
    assert!(push_event(
        &mut state,
        "boot-1",
        updated_event(
            vec![
                account("claude", "claude:default"),
                account("codex", "codex:default"),
            ],
            vec![
                refresh_state("claude:default"),
                refresh_state("codex:default"),
            ],
        ),
    ));
    assert_eq!(
        account_ids(&state.observability.accounts),
        ["claude:default"],
        "本机关闭的厂商事件被丢弃"
    );
    assert!(
        state
            .observability
            .refresh_states
            .iter()
            .all(|refresh| refresh.account_id != "codex:default"),
        "刷新状态同样不合并"
    );
}

fn deliver_usage_for_with_refresh(
    state: &mut ClientShellState,
    agent: &str,
    accounts: Vec<AccountUsageSnapshot>,
    refresh: Vec<UsageRefreshState>,
) -> bool {
    let epoch = state.observability.epoch;
    state.receive_observation(
        epoch,
        Purpose::Usage {
            agent: Some(agent.into()),
        },
        Ok(ResponseResult::AccountUsage {
            accounts,
            refresh: Some(refresh),
        }),
    )
}

/// 投递厂商列表请求的失败结局。
fn fail_providers(state: &mut ClientShellState) {
    let epoch = state.observability.epoch;
    state.receive_observation(
        epoch,
        Purpose::Providers,
        Err(ClientShellEndpointError {
            code: Some("server_context_required".into()),
            message: "server context required".into(),
        }),
    );
}

/// 总览扇出里一部分厂商被在途请求挡下时，显式刷新不能只对已发出的厂商生效：
/// 被挡下的厂商保留强意图并在 200 ms 重试时仍发 `refresh`，已发出的不重复发；
/// 全部目标都发出后才结束「刷新中」。
#[test]
fn partial_fan_out_keeps_the_manual_refresh_for_blocked_providers() {
    let mut state = usage_ready();
    deliver_providers(
        &mut state,
        vec![
            provider("claude", &["claude:default"]),
            provider("codex", &["codex:default"]),
        ],
    );
    let t0 = Instant::now() + Duration::from_secs(1);
    open_accounts_overview(&mut state, t0 - Duration::from_millis(500));
    let calls = usage_calls(&tick(&mut state, t0));
    assert_eq!(calls.len(), 2, "回到总览逐厂商发 refresh: {calls:?}");
    // 只有 codex 结算，claude 的请求仍在途；此时用户点「刷新」。
    assert!(deliver_usage_for(
        &mut state,
        "codex",
        vec![account("codex", "codex:default")]
    ));
    assert!(deliver_usage_for(
        &mut state,
        "claude",
        vec![account("claude", "claude:default")]
    ));
    assert!(!state.observability.refreshing(), "第一轮已收尾");
    let t1 = t0 + Duration::from_secs(2);
    let calls = usage_calls(&tick(&mut state, t1));
    assert_eq!(calls.len(), 2, "普通轮询逐厂商 get: {calls:?}");
    assert!(deliver_usage_for(
        &mut state,
        "codex",
        vec![account("codex", "codex:default")]
    ));
    state.observation_action(Action::Refresh, &mut ClientShellInput::default());
    assert!(state.observability.refreshing());
    let calls = usage_calls(&tick(&mut state, t1 + Duration::from_millis(10)));
    assert_eq!(
        calls,
        vec![(
            true,
            UsageParams {
                agent: Some("codex".into()),
                account_id: None,
                pane_id: None,
            }
        )],
        "空闲的 codex 立即发 refresh，claude 被在途 get 挡下: {calls:?}"
    );
    assert!(
        state.observability.refreshing(),
        "claude 的强意图未发出，仍是刷新中"
    );
    // claude 的旧 get 结算后 200 ms 重试：只补发 claude，且必须仍是 refresh。
    assert!(deliver_usage_for(
        &mut state,
        "claude",
        vec![account("claude", "claude:default")]
    ));
    assert!(
        state.observability.refreshing(),
        "已发出的 codex 响应未到，仍刷新中"
    );
    let calls = usage_calls(&tick(&mut state, t1 + Duration::from_millis(230)));
    assert_eq!(
        calls,
        vec![(
            true,
            UsageParams {
                agent: Some("claude".into()),
                account_id: None,
                pane_id: None,
            }
        )],
        "被挡下的厂商补发 refresh（不是 get），已发出的 codex 不重复: {calls:?}"
    );
    assert!(state.observability.refreshing());
    assert!(deliver_usage_for(
        &mut state,
        "codex",
        vec![account("codex", "codex:default")]
    ));
    assert!(state.observability.refreshing(), "claude 的 refresh 仍在途");
    assert!(deliver_usage_for(
        &mut state,
        "claude",
        vec![account("claude", "claude:default")]
    ));
    assert!(
        !state.observability.refreshing(),
        "全部厂商到齐才结束刷新中"
    );
    let calls = usage_calls(&tick(&mut state, t1 + Duration::from_millis(2_300)));
    assert!(
        calls.iter().all(|(manual, _)| !*manual),
        "强意图已消费，下一轮回到普通 get: {calls:?}"
    );
}

/// 扇出的基数只在服务端给出可判定信息时展开：`installed` 缺省（旧 server）
/// 一律回落一次 `agent=None`；已安装厂商超过上限也回落；请求数不随注册表规模
/// 线性增长。
#[test]
fn overview_fan_out_is_bounded_and_falls_back_when_installed_is_unknown() {
    let mut state = usage_ready();
    let unknown = (0..23)
        .map(|index| {
            let mut info = provider(&format!("vendor{index}"), &[]);
            info.installed = None;
            info
        })
        .collect::<Vec<_>>();
    deliver_providers(&mut state, unknown);
    let t0 = Instant::now() + Duration::from_secs(1);
    open_accounts_overview(&mut state, t0 - Duration::from_millis(500));
    let calls = usage_calls(&tick(&mut state, t0));
    assert_eq!(
        sorted_agents(&calls),
        [None],
        "旧 server 不宣告 installed：只发一次整体请求: {calls:?}"
    );
    assert!(deliver_usage(
        &mut state,
        vec![account("claude", "claude:default")]
    ));

    // 服务端宣告了 installed：只有已安装（或已配置账号）的厂商参与扇出。
    let mut listed = (0..20)
        .map(|index| {
            let mut info = provider(&format!("vendor{index}"), &[]);
            info.installed = Some(false);
            info
        })
        .collect::<Vec<_>>();
    listed.push(provider("claude", &["claude:default"]));
    listed.push(provider("codex", &[]));
    let mut configured_only = provider("kimi", &["kimi:work"]);
    configured_only.installed = Some(false);
    listed.push(configured_only);
    deliver_providers(&mut state, listed);
    let calls = usage_calls(&tick(&mut state, t0 + Duration::from_secs(2)));
    assert_eq!(
        sorted_agents(&calls),
        [
            Some("claude".to_owned()),
            Some("codex".to_owned()),
            Some("kimi".to_owned())
        ],
        "23 个厂商里只有 3 个可判定为已列出: {calls:?}"
    );
    for call in &calls {
        assert!(deliver_usage_for(
            &mut state,
            call.1.agent.as_deref().expect("逐厂商请求"),
            Vec::new()
        ));
    }

    // 已安装厂商超过扇出上限：回落整体请求，响应侧仍按本机关闭过滤。
    let many = (0..12)
        .map(|index| provider(&format!("vendor{index}"), &[]))
        .collect::<Vec<_>>();
    deliver_providers(&mut state, many);
    let calls = usage_calls(&tick(&mut state, t0 + Duration::from_secs(4)));
    assert_eq!(
        sorted_agents(&calls),
        [None],
        "超过上限不逐厂商扇出: {calls:?}"
    );
}

/// 厂商列表报错或迟迟不到时总览不能停摆：失败即回落整体请求并按退避重拉列表，
/// 等待期间不把轮询压到 200 ms，也不每 tick 重发列表请求。
#[test]
fn overview_polls_without_a_provider_list_after_it_fails_or_stalls() {
    let mut state = usage_ready();
    state.open_observation_page(Page::Accounts, &mut ClientShellInput::default());
    state.observation_action(Action::Refresh, &mut ClientShellInput::default());
    let t0 = Instant::now() + Duration::from_secs(1);
    let opened = tick(&mut state, t0);
    assert_eq!(providers_calls(&opened), 1);
    assert!(usage_calls(&opened).is_empty(), "列表刚发出：先等一等");
    let soon = tick(&mut state, t0 + Duration::from_millis(300));
    assert_eq!(providers_calls(&soon), 0, "等待期间不重发列表");
    assert!(
        usage_calls(&soon).is_empty(),
        "等待期间不压到 200 ms 轮询: {:?}",
        usage_calls(&soon)
    );
    fail_providers(&mut state);
    let after_failure = tick(&mut state, t0 + Duration::from_millis(400));
    assert_eq!(
        sorted_agents(&usage_calls(&after_failure)),
        [None],
        "列表失败：立即回落整体请求，强意图不丢: {:?}",
        usage_calls(&after_failure)
    );
    assert!(
        usage_calls(&after_failure)
            .iter()
            .all(|(manual, _)| *manual),
        "仍是强意图刷新"
    );
    assert_eq!(
        providers_calls(&after_failure),
        0,
        "失败后按退避重拉，不每 tick 重发"
    );
    assert!(deliver_usage(
        &mut state,
        vec![account("claude", "claude:default")]
    ));
    let later = tick(&mut state, t0 + Duration::from_secs(3));
    assert_eq!(providers_calls(&later), 0, "退避未到期");
    assert_eq!(sorted_agents(&usage_calls(&later)), [None]);
    assert!(deliver_usage(
        &mut state,
        vec![account("claude", "claude:default")]
    ));
    let retried = tick(&mut state, t0 + Duration::from_secs(12));
    assert_eq!(providers_calls(&retried), 1, "退避到期后重拉列表");

    // 列表请求发出后响应一直不到（连接未断、pending 键不释放）：超过等待上限
    // 即回落整体请求，正常 2 秒节奏。
    let mut stalled = usage_ready();
    stalled.open_observation_page(Page::Accounts, &mut ClientShellInput::default());
    let t1 = Instant::now() + Duration::from_secs(1);
    let opened = tick(&mut stalled, t1);
    assert_eq!(providers_calls(&opened), 1);
    assert!(usage_calls(&opened).is_empty());
    let fallback = tick(&mut stalled, t1 + Duration::from_secs(2));
    assert_eq!(
        sorted_agents(&usage_calls(&fallback)),
        [None],
        "列表丢失：等待超限后回落整体请求: {:?}",
        usage_calls(&fallback)
    );
    assert_eq!(providers_calls(&fallback), 0, "列表键仍在途，不重发");
}

/// 「一个厂商都没列出」与「所有已列出厂商被本机关闭」是两回事：前者说明此主机
/// 没有受支持的 agent CLI，不能把用户指去设置页「重新启用」。
#[test]
fn overview_without_listed_providers_explains_instead_of_blaming_settings() {
    let mut state = usage_ready();
    let mut missing = provider("claude", &[]);
    missing.installed = Some(false);
    deliver_providers(&mut state, vec![missing]);
    let t0 = Instant::now() + Duration::from_secs(1);
    open_accounts_overview(&mut state, t0 - Duration::from_millis(500));
    let calls = usage_calls(&tick(&mut state, t0));
    assert!(calls.is_empty(), "没有已列出的厂商：不发请求: {calls:?}");
    assert!(!state.observability.refreshing(), "强意图已消费");
    let message = state.observability.message.clone().unwrap_or_default();
    assert!(
        message.contains("未检测到") || message.contains("No installed"),
        "文案说明未检测到 agent CLI，而不是指向设置页: {message}"
    );

    // 对照：已列出但全部被本机关闭，才是「已在设置中关闭」。
    deliver_providers(&mut state, vec![provider("claude", &["claude:default"])]);
    state.observability.usage.disabled_providers = vec!["claude".into()];
    state.observability.message = None;
    state.observation_action(Action::Overview, &mut ClientShellInput::default());
    let calls = usage_calls(&tick(&mut state, t0 + Duration::from_secs(2)));
    assert!(calls.is_empty());
    let message = state.observability.message.clone().unwrap_or_default();
    assert!(
        message.contains("设置") || message.contains("settings"),
        "本机关闭才指向设置页: {message}"
    );
}

/// 逐厂商响应按厂商清理旧刷新状态：只经 refreshing 事件写入、尚未出现在快照里
/// 的账号状态也被本次权威响应替换，不残留、不重复。
#[test]
fn per_provider_response_replaces_orphaned_refresh_states_of_that_provider() {
    let mut state = subscribing_ready();
    let t0 = Instant::now() + Duration::from_secs(1);
    open_accounts_overview(&mut state, t0 - Duration::from_millis(500));
    deliver_providers(
        &mut state,
        vec![
            provider("claude", &["claude:default", "claude:work"]),
            provider("codex", &["codex:default"]),
        ],
    );
    tick(&mut state, t0);
    assert!(deliver_usage_for_with_refresh(
        &mut state,
        "claude",
        vec![account("claude", "claude:default")],
        vec![refresh_state("claude:default")],
    ));
    assert!(deliver_usage_for_with_refresh(
        &mut state,
        "codex",
        vec![account("codex", "codex:default")],
        vec![refresh_state("codex:default")],
    ));
    // 服务端新增了 claude:work，先推 refreshing 事件（账号尚未进快照）。
    let mut orphan = refresh_state("claude:work");
    orphan.in_flight = true;
    assert!(push_event(
        &mut state,
        "boot-1",
        ObservationEventEnvelope::AccountUsageRefreshing(AccountUsageRefreshingEvent {
            refresh: vec![orphan],
        }),
    ));
    assert!(state
        .observability
        .refresh_states
        .iter()
        .any(|refresh| refresh.account_id == "claude:work" && refresh.in_flight));
    // claude 的权威响应：work 账号探测已完成、default 状态更新一次。
    let mut settled = refresh_state("claude:work");
    settled.in_flight = false;
    assert!(deliver_usage_for_with_refresh(
        &mut state,
        "claude",
        vec![
            account("claude", "claude:default"),
            account("claude", "claude:work"),
        ],
        vec![refresh_state("claude:default"), settled],
    ));
    let mut ids = state
        .observability
        .refresh_states
        .iter()
        .map(|refresh| refresh.account_id.as_str())
        .collect::<Vec<_>>();
    ids.sort_unstable();
    assert_eq!(
        ids,
        ["claude:default", "claude:work", "codex:default"],
        "同一账号只有一条，别家厂商的状态保留"
    );
    assert!(
        state
            .observability
            .refresh_states
            .iter()
            .all(|refresh| !refresh.in_flight),
        "孤儿状态被权威响应替换而不是残留旧的 in_flight"
    );
}

/// B-12 核验（总览态逐厂商订阅）：总览（未选厂商）时客户端只开**一条**不带
/// 厂商过滤的订阅，事件按 `account_id` 合并，跨厂商的更新都收得到；因此不需要
/// 逐厂商开多条订阅（服务端 `matches_account` 对 `agent=None` 匹配全部账号，
/// 周期性重探测也按同参数扇出）。
#[test]
fn overview_subscription_is_broad_and_merges_every_provider() {
    let mut state = subscribing_ready();
    // 账号页的总览态（`Action::Overview` 复位厂商选择）；厂商列表随后到达。
    let t0 = Instant::now() + Duration::from_secs(1);
    let opened = open_accounts_overview(&mut state, t0);
    deliver_providers(
        &mut state,
        vec![
            provider("claude", &["claude:default"]),
            provider("codex", &["codex:default"]),
        ],
    );
    let subscribed = subscribe_calls(&opened);
    assert_eq!(
        subscribed.len(),
        1,
        "总览态只开一条订阅: {:?}",
        opened.actions
    );
    assert_eq!(subscribed[0].agent, None, "总览订阅不带厂商过滤");
    assert_eq!(subscribed[0].account_id, None);
    assert!(deliver_subscription(&mut state, "usage-1", true));

    // 两个厂商的更新都合并进页面（同一事件、同一订阅）。
    assert!(push_event(
        &mut state,
        "boot-1",
        updated_event(
            vec![
                account("claude", "claude:default"),
                account("codex", "codex:default"),
            ],
            Vec::new(),
        ),
    ));
    assert!(
        state
            .observability
            .accounts
            .iter()
            .any(|account| account.agent == "claude"),
        "claude 的推送落进页面"
    );
    assert!(
        state
            .observability
            .accounts
            .iter()
            .any(|account| account.agent == "codex"),
        "codex 的推送同样落进页面"
    );
}

// ---------------------------------------------------------------------------
// 账号页厂商专属卡片：每厂商在 120×40 / 80×24 页面与 68×17 悬浮层三档断言字符
// （数字可见、溢出标记、徽标文字、DIM）；概览紧凑卡、空态、悬浮层「+N」退化。
// 夹具全部手写、脱敏，不读任何真实凭据或用量文件。
// ---------------------------------------------------------------------------

/// 卡片用例的固定「现在」：窗口进度、距重置与新鲜度都据此计算。
const CARD_NOW_MS: u64 = 1_800_000_000_000;
const CARD_NOW_S: u64 = CARD_NOW_MS / 1000;

fn usage_metric(id: &str, label: &str, unit: &str, scope: &str) -> crate::api::schema::UsageMetric {
    crate::api::schema::UsageMetric {
        id: id.into(),
        label: label.into(),
        unit: unit.into(),
        scope: scope.into(),
        ..Default::default()
    }
}

fn vendor_account(
    agent: &str,
    metrics: Vec<crate::api::schema::UsageMetric>,
) -> AccountUsageSnapshot {
    AccountUsageSnapshot {
        metrics,
        observed_at_ms: CARD_NOW_MS - 13_000,
        ..account(agent, &format!("{agent}:default"))
    }
}

/// claude：5 小时 42%、每周 91%（已过重置，服务端沿用上次值）、消费额度 162.8%
/// （超限），本会话费用 / 时长，上下文在首次请求前为 null。
fn claude_card_account() -> AccountUsageSnapshot {
    use crate::api::schema::UsageMetric;
    vendor_account(
        "claude",
        vec![
            UsageMetric {
                used_percent: Some(42.0),
                resets_at: Some(CARD_NOW_S + 3 * 3600),
                ..usage_metric("five_hour", "5 小时额度", "%", "account")
            },
            UsageMetric {
                used_percent: Some(91.0),
                resets_at: Some(CARD_NOW_S - 60),
                text_value: Some("已过重置时间，沿用上次值".into()),
                ..usage_metric("seven_day", "每周额度", "%", "account")
            },
            UsageMetric {
                used_percent: Some(162.8),
                resets_at: Some(CARD_NOW_S + 20 * 86_400),
                ..usage_metric("spend_limit", "网关消费额度", "%", "account")
            },
            UsageMetric {
                amount_decimal: Some("1.234567".into()),
                ..usage_metric("cost/total_cost_usd", "本会话估算费用", "USD", "session")
            },
            UsageMetric {
                used: Some(3_720_000.0),
                text_value: Some("1h02m".into()),
                ..usage_metric("cost/total_duration_ms", "本会话时长", "ms", "session")
            },
            UsageMetric {
                text_value: Some("暂无数据（首次请求前或 /compact 后）".into()),
                ..usage_metric(
                    "context_window/used_percentage",
                    "上下文占用",
                    "%",
                    "session",
                )
            },
            UsageMetric {
                used: Some(200_000.0),
                ..usage_metric(
                    "context_window/context_window_size",
                    "上下文窗口大小",
                    "tokens",
                    "session",
                )
            },
        ],
    )
}

/// codex：两个限额桶（Codex 的 5h / 7d 窗口 + credits，GPT-5 Pro 的 5h 窗口）。
fn codex_card_account() -> AccountUsageSnapshot {
    use crate::api::schema::UsageMetric;
    vendor_account(
        "codex",
        vec![
            UsageMetric {
                used_percent: Some(30.0),
                window_seconds: Some(5 * 3600),
                resets_at: Some(CARD_NOW_S + 2 * 3600),
                ..usage_metric("codex/primary", "Codex · 主要额度", "%", "account")
            },
            UsageMetric {
                used_percent: Some(75.0),
                window_seconds: Some(7 * 86_400),
                resets_at: Some(CARD_NOW_S + 86_400),
                ..usage_metric("codex/secondary", "Codex · 次级额度", "%", "account")
            },
            UsageMetric {
                amount_decimal: Some("12.5".into()),
                ..usage_metric("codex/credits", "额外余额", "credits", "account")
            },
            UsageMetric {
                used_percent: Some(5.0),
                window_seconds: Some(5 * 3600),
                resets_at: Some(CARD_NOW_S + 4 * 3600),
                ..usage_metric("pro/primary", "GPT-5 Pro · 主要额度", "%", "account")
            },
        ],
    )
}

/// kimi：5 小时 / 7 天 / 月度窗口、可用余额（CNY）与额外用量钱包（USD）。
fn kimi_card_account() -> AccountUsageSnapshot {
    use crate::api::schema::UsageMetric;
    let money = |id: &str, label: &str, unit: &str, amount: &str| UsageMetric {
        amount_decimal: Some(amount.into()),
        ..usage_metric(id, label, unit, "account")
    };
    vendor_account(
        "kimi",
        vec![
            UsageMetric {
                used_percent: Some(25.0),
                resets_at: Some(CARD_NOW_S + 3600),
                ..usage_metric("limit5h", "5 小时额度", "%", "account")
            },
            UsageMetric {
                used_percent: Some(60.0),
                resets_at: Some(CARD_NOW_S + 3 * 86_400),
                ..usage_metric("limit7d", "7 天额度", "%", "account")
            },
            UsageMetric {
                used_percent: Some(10.0),
                ..usage_metric("monthTotal", "月度额度", "%", "account")
            },
            money("available_balance", "可用余额", "CNY", "12.3"),
            money("extra_usage/balance", "额外用量余额", "USD", "3.20"),
            money(
                "extra_usage/monthly_used",
                "本月额外用量费用",
                "USD",
                "1.00",
            ),
            money(
                "extra_usage/monthly_limit",
                "每月额外用量上限",
                "USD",
                "10.00",
            ),
        ],
    )
}

/// opencode：本机会话统计（scope = local）。
fn opencode_card_account() -> AccountUsageSnapshot {
    use crate::api::schema::UsageMetric;
    let count = |id: &str, label: &str, unit: &str, value: f64| UsageMetric {
        used: Some(value),
        ..usage_metric(id, label, unit, "local")
    };
    vendor_account(
        "opencode",
        vec![
            count("sessions", "会话数", "sessions", 42.0),
            count("child_sessions", "子 agent 会话数", "sessions", 7.0),
            UsageMetric {
                amount_decimal: Some("4.5600".into()),
                ..usage_metric("total_cost", "累计费用", "USD", "local")
            },
            count("input_tokens", "输入 token", "tokens", 1_234_567.0),
            count("output_tokens", "输出 token", "tokens", 345_678.0),
            count("reasoning_tokens", "推理 token", "tokens", 12_345.0),
            count("cache_read_tokens", "缓存读取 token", "tokens", 2_000_000.0),
            count("cache_write_tokens", "缓存写入 token", "tokens", 45_000.0),
        ],
    )
}

/// pi：扩展推送的会话统计（scope = session）。
fn pi_card_account() -> AccountUsageSnapshot {
    use crate::api::schema::UsageMetric;
    let tokens = |id: &str, label: &str, value: f64| UsageMetric {
        used: Some(value),
        ..usage_metric(id, label, "tokens", "session")
    };
    vendor_account(
        "pi",
        vec![
            UsageMetric {
                used: Some(45.0),
                text_value: Some("45%".into()),
                ..usage_metric("context/percent", "上下文占用", "%", "session")
            },
            tokens("context/tokens", "上下文 token", 90_000.0),
            tokens("context/context_window", "上下文窗口大小", 200_000.0),
            UsageMetric {
                amount_decimal: Some("0.1234".into()),
                ..usage_metric("session/cost_usd", "本会话费用", "USD", "session")
            },
            tokens("session/tokens/input", "输入 token", 80_000.0),
            tokens("session/tokens/output", "输出 token", 12_000.0),
            tokens("session/tokens/cache_read", "缓存读取 token", 5_000.0),
            tokens("session/tokens/cache_write", "缓存写入 token", 1_000.0),
            tokens("session/tokens/total", "合计 token", 98_000.0),
            UsageMetric {
                text_value: Some("vendor/model-x".into()),
                ..usage_metric("session/model", "当前模型", "", "session")
            },
        ],
    )
}

/// zcode：ZCode 桌面版数据库的本地统计（id 约定见匹配表）。
fn zcode_card_account() -> AccountUsageSnapshot {
    use crate::api::schema::UsageMetric;
    let local = |id: &str, label: &str, unit: &str, value: f64| UsageMetric {
        used: Some(value),
        ..usage_metric(id, label, unit, "local")
    };
    vendor_account(
        "zcode",
        vec![
            local("session/tokens/main", "主任务 token", "tokens", 1_500_000.0),
            local(
                "session/tokens/subagents",
                "子 agent token",
                "tokens",
                300_000.0,
            ),
            local("session/tokens/total", "合计 token", "tokens", 1_800_000.0),
            local("session/tool_uses", "工具调用", "count", 120.0),
            local("session/subagents", "子 agent", "count", 4.0),
            UsageMetric {
                text_value: Some("24h".into()),
                ..usage_metric("session/window_hours", "统计窗口", "", "local")
            },
        ],
    )
}

/// 经典布局（未通告 `client.views.set`）的账号页：直接写入厂商选择与账号快照后
/// 合成 `cols×rows`。页面铺满 pane 区，`observability.page_rect` 就是页面矩形。
fn cards_page(
    agent: Option<&str>,
    accounts: Vec<AccountUsageSnapshot>,
    cols: u16,
    rows: u16,
) -> ClientShellState {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    state.open_observation_page(Page::Accounts, &mut ClientShellInput::default());
    state.observability.selected_provider = agent.map(str::to_owned);
    state.observability.accounts = accounts;
    state.observability.now_ms = CARD_NOW_MS;
    state.compose(cols, rows).expect("账号页");
    assert!(
        !state.observability.refreshing(),
        "用例前提：没有强意图刷新，卡片不变暗"
    );
    state
}

/// agent 行悬浮层（账号用量卡）：账号快照放进悬浮层作用域，终端 120×40 时悬浮层
/// 正好是上限 68×17。
fn cards_hover(agent: &str, accounts: Vec<AccountUsageSnapshot>) -> ClientShellState {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    state.observability.hover_scope.provider = Some(agent.into());
    state.observability.hover_scope.accounts = accounts;
    state.observability.now_ms = CARD_NOW_MS;
    state.observability.hover = Some(Hover {
        target: HoverTarget::Agent {
            endpoint_id: state.active_endpoint_id.clone(),
            pane: "pane_1".into(),
            agent: agent.into(),
        },
        anchor: Rect::new(0, 2, 20, 1),
        since: Instant::now(),
        visible: true,
        leave_at: None,
        pinned: false,
    });
    state.compose(120, 40).expect("悬浮层");
    let hover = state.observability.hover_rect;
    assert_eq!(
        (hover.width, hover.height),
        (68, 17),
        "用例前提：悬浮层是上限尺寸"
    );
    state
}

/// 三档宿主：页面 120×40、页面 80×24、悬浮层 68×17；返回 (档名, 状态, 正文所在矩形)。
fn card_hosts(
    agent: &str,
    accounts: impl Fn() -> Vec<AccountUsageSnapshot>,
) -> Vec<(&'static str, ClientShellState, Rect)> {
    let mut hosts = Vec::new();
    for (name, cols, rows) in [("page 120x40", 120, 40), ("page 80x24", 80, 24)] {
        let state = cards_page(Some(agent), accounts(), cols, rows);
        let region = state.observability.page_rect;
        hosts.push((name, state, region));
    }
    let state = cards_hover(agent, accounts());
    let region = state.observability.hover_rect;
    hosts.push(("hover 68x17", state, region));
    hosts
}

/// 区域里一行的文本与每个字符所在的列（宽字符的续格不重复计入）。
fn region_row(buffer: &ratatui::buffer::Buffer, region: Rect, y: u16) -> (String, Vec<u16>) {
    let mut text = String::new();
    let mut columns = Vec::new();
    let mut x = region.x;
    while x < region.right() {
        let symbol = buffer[(x, y)].symbol();
        for ch in symbol.chars() {
            text.push(ch);
            columns.push(x);
        }
        x += (crate::ui::display_width(symbol) as u16).max(1);
    }
    (text, columns)
}

/// `needle` 在区域里首次出现的单元格。
fn find_in(state: &ClientShellState, region: Rect, needle: &str) -> Option<(u16, u16)> {
    let buffer = state.compose_buffer.as_ref().expect("帧缓冲");
    (region.y..region.bottom()).find_map(|y| {
        let (text, columns) = region_row(buffer, region, y);
        text.find(needle)
            .map(|byte| (columns[text[..byte].chars().count()], y))
    })
}

/// 区域里 `needle` 所在的那一行文本。
fn row_with(state: &ClientShellState, region: Rect, needle: &str) -> Option<String> {
    let buffer = state.compose_buffer.as_ref().expect("帧缓冲");
    find_in(state, region, needle).map(|(_, y)| region_row(buffer, region, y).0)
}

fn region_text(state: &ClientShellState, region: Rect) -> String {
    let buffer = state.compose_buffer.as_ref().expect("帧缓冲");
    (region.y..region.bottom())
        .map(|y| region_row(buffer, region, y).0)
        .collect::<Vec<_>>()
        .join("\n")
}

fn cell_style(state: &ClientShellState, (x, y): (u16, u16)) -> ratatui::style::Style {
    state.compose_buffer.as_ref().expect("帧缓冲")[(x, y)].style()
}

fn palette() -> crate::app::state::Palette {
    ClientShellConfig::from_config(&Config::default()).palette
}

#[test]
fn claude_card_shows_three_windows_overflow_stale_and_session_row() {
    let _guard = crate::i18n::lang_guard(crate::i18n::Lang::ZhCn);
    let palette = palette();
    let dim = ratatui::style::Modifier::DIM;
    for (host, state, region) in card_hosts("claude", || vec![claude_card_account()]) {
        let text = region_text(&state, region);
        assert!(
            row_with(&state, region, "claude:default").is_some_and(|row| row.contains("已更新")),
            "{host}: 卡头是账号标签 + 状态徽标\n{text}"
        );
        let five = row_with(&state, region, "5 小时").unwrap_or_default();
        assert!(five.contains("42%"), "{host}: 5 小时窗口数字可见\n{text}");
        // statusline 不带窗口长度（夹具与 `parse::claude` 一致）：按名义 5 小时画刻度。
        assert!(five.contains('┃'), "{host}: 5 小时窗口进度刻度\n{text}");
        // 消费额度超限：数字原样（不截成 100%），条形末格是溢出标记且为红色。
        let spend = row_with(&state, region, "消费额度").unwrap_or_default();
        assert!(spend.contains("162.8%"), "{host}: 超限数字原样显示\n{text}");
        let overflow = find_in(&state, region, "▸").expect("溢出标记");
        assert_eq!(
            cell_style(&state, overflow).fg,
            Some(palette.red),
            "{host}: 溢出标记是红色"
        );
        // 每周窗口已过重置时间：沿用上次值。只弱化一次——文字按 stale 转灰，DIM 只
        // 叠在条形格上；数字与标签不能既是 overlay0 又带 DIM（半亮渲染 DIM 的终端上
        // 看不清）。窄档至少保住数字。
        let weekly = find_in(&state, region, "每周").expect("每周窗口");
        assert!(
            row_with(&state, region, "每周").is_some_and(|row| row.contains("91%")),
            "{host}: 过期窗口仍显示上次值\n{text}"
        );
        let number = find_in(&state, region, "91%").expect("过期窗口的数字");
        for (what, cell) in [("数字", number), ("标签", weekly)] {
            let style = cell_style(&state, cell);
            assert!(
                !(style.fg == Some(palette.overlay0) && style.add_modifier.contains(dim)),
                "{host}: 过期窗口的{what}不被弱化两次: {style:?}"
            );
        }
        let bar = (region.x..region.right())
            .filter(|x| {
                let buffer = state.compose_buffer.as_ref().expect("帧缓冲");
                matches!(buffer[(*x, number.1)].symbol(), "━" | "░" | "┃" | "▸")
            })
            .collect::<Vec<_>>();
        assert!(!bar.is_empty(), "{host}: 过期窗口仍画条形\n{text}");
        assert!(
            bar.iter().all(|x| cell_style(&state, (*x, number.1))
                .add_modifier
                .contains(dim)),
            "{host}: 过期窗口的条形 DIM"
        );
        // 上下文为 null：「暂无数据」灰字，不当成 0%。
        let pending = find_in(&state, region, "暂无数据").expect("上下文暂无数据");
        assert_eq!(cell_style(&state, pending).fg, Some(palette.overlay0));
        assert!(
            row_with(&state, region, "上下文").is_some_and(|row| !row.contains("0%")),
            "{host}: 上下文未知不画成 0%\n{text}"
        );
        // 会话行：本会话费用。
        assert!(
            row_with(&state, region, "费用").is_some_and(|row| row.contains("$1.23")),
            "{host}: 本会话费用\n{text}"
        );
    }
    // 宽页面上过期窗口写明原因，说明同样只弱化一次。
    let state = cards_page(Some("claude"), vec![claude_card_account()], 120, 40);
    let region = state.observability.page_rect;
    assert!(
        row_with(&state, region, "每周").is_some_and(|row| row.contains("已过重置")),
        "{}",
        region_text(&state, region)
    );
    let note = find_in(&state, region, "已过重置").expect("过期说明");
    let style = cell_style(&state, note);
    assert!(
        !(style.fg == Some(palette.overlay0) && style.add_modifier.contains(dim)),
        "过期说明不被弱化两次: {style:?}"
    );
}

#[test]
fn codex_card_splits_buckets_and_keeps_credits() {
    let _guard = crate::i18n::lang_guard(crate::i18n::Lang::ZhCn);
    for (host, state, region) in card_hosts("codex", || vec![codex_card_account()]) {
        let text = region_text(&state, region);
        // 多桶分小节：两个桶名都作为小节标题出现。
        assert!(
            find_in(&state, region, "Codex ─").is_some(),
            "{host}\n{text}"
        );
        assert!(
            find_in(&state, region, "GPT-5 Pro ─").is_some(),
            "{host}\n{text}"
        );
        // 窗口按长度命名，数字可见。
        let primary = row_with(&state, region, "5h 窗口").unwrap_or_default();
        assert!(primary.contains("30%"), "{host}\n{text}");
        let secondary = row_with(&state, region, "7d 窗口").unwrap_or_default();
        assert!(secondary.contains("75%"), "{host}\n{text}");
        // 窗口进度刻度：5h 窗口过了 60%，条形里有刻度字形。
        assert!(primary.contains('┃'), "{host}: 窗口进度刻度\n{text}");
        // credits 余额只画数值。
        let credits = row_with(&state, region, "额外余额").unwrap_or_default();
        assert!(credits.contains("12.50 credits"), "{host}\n{text}");
        assert!(!credits.contains('━') && !credits.contains('░'), "{host}");
    }
}

#[test]
fn kimi_card_shows_windows_balance_and_extra_usage() {
    let _guard = crate::i18n::lang_guard(crate::i18n::Lang::ZhCn);
    for (host, state, region) in card_hosts("kimi", || vec![kimi_card_account()]) {
        let text = region_text(&state, region);
        for (label, value) in [("5 小时", "25%"), ("7 天", "60%"), ("月度", "10%")] {
            assert!(
                row_with(&state, region, label).is_some_and(|row| row.contains(value)),
                "{host}: {label} {value}\n{text}"
            );
        }
        // 5 小时 / 7 天窗口的报文不带窗口长度：按名义长度画刻度；月度周期不固定，不画。
        for (label, tick) in [("5 小时", true), ("7 天", true), ("月度", false)] {
            let row = row_with(&state, region, label).unwrap_or_default();
            assert_eq!(
                row.contains('┃'),
                tick,
                "{host}: {label} 窗口进度刻度\n{text}"
            );
        }
        assert!(
            row_with(&state, region, "可用余额").is_some_and(|row| row.contains("¥12.30")),
            "{host}: 余额按人民币显示\n{text}"
        );
        assert!(
            find_in(&state, region, "额外用量").is_some(),
            "{host}\n{text}"
        );
        let extra = row_with(&state, region, "$3.20").unwrap_or_default();
        assert!(
            extra.contains("$1.00") && extra.contains("$10.00"),
            "{host}: 额外用量余额 / 本月 / 上限\n{text}"
        );
    }
}

#[test]
fn opencode_card_is_labelled_local_stats_without_gauges() {
    let _guard = crate::i18n::lang_guard(crate::i18n::Lang::ZhCn);
    let palette = palette();
    for (host, state, region) in card_hosts("opencode", || vec![opencode_card_account()]) {
        let text = region_text(&state, region);
        let header = row_with(&state, region, "opencode:default").unwrap_or_default();
        assert!(
            header.contains("本地统计，非账号额度"),
            "{host}: 徽标声明本地统计\n{text}"
        );
        // 状态移到卡内首行。
        let status = find_in(&state, region, "已更新").expect("状态行");
        assert_eq!(cell_style(&state, status).fg, Some(palette.green));
        for value in ["42", "$4.56", "1.2M", "345.7k", "12.3k", "2M", "45k"] {
            assert!(
                find_in(&state, region, value).is_some(),
                "{host}: {value}\n{text}"
            );
        }
        // token 与费用只画数值，整张卡没有 gauge。
        assert!(!text.contains('━') && !text.contains('░'), "{host}\n{text}");
    }
}

#[test]
fn pi_card_shows_context_gauge_cost_tokens_and_model() {
    let _guard = crate::i18n::lang_guard(crate::i18n::Lang::ZhCn);
    for (host, state, region) in card_hosts("pi", || vec![pi_card_account()]) {
        let text = region_text(&state, region);
        assert!(
            row_with(&state, region, "pi:default").is_some_and(|row| row.contains("会话统计")),
            "{host}: 徽标声明会话统计\n{text}"
        );
        let context = row_with(&state, region, "上下文").unwrap_or_default();
        assert!(
            context.contains("45%") && context.contains('━'),
            "{host}: 上下文占用 gauge\n{text}"
        );
        assert!(
            row_with(&state, region, "费用").is_some_and(|row| row.contains("$0.1234")),
            "{host}\n{text}"
        );
        assert!(
            row_with(&state, region, "模型").is_some_and(|row| row.contains("vendor/model-x")),
            "{host}\n{text}"
        );
        for value in ["80k", "12k", "98k", "5k", "1k"] {
            assert!(
                find_in(&state, region, value).is_some(),
                "{host}: token {value}\n{text}"
            );
        }
    }
}

#[test]
fn pi_card_without_data_is_an_empty_state() {
    let _guard = crate::i18n::lang_guard(crate::i18n::Lang::ZhCn);
    for (host, state, region) in card_hosts("pi", || vec![vendor_account("pi", Vec::new())]) {
        let text = region_text(&state, region);
        assert!(
            find_in(&state, region, "暂无会话数据").is_some(),
            "{host}: 没有推送时是空态\n{text}"
        );
        assert!(
            find_in(&state, region, "pi:default").is_some(),
            "{host}\n{text}"
        );
    }
}

#[test]
fn zcode_card_appears_only_for_a_zcode_account() {
    let _guard = crate::i18n::lang_guard(crate::i18n::Lang::ZhCn);
    for (host, state, region) in card_hosts("zcode", || vec![zcode_card_account()]) {
        let text = region_text(&state, region);
        assert!(
            row_with(&state, region, "zcode:default")
                .is_some_and(|row| row.contains("本地统计，非账号额度")),
            "{host}: 徽标声明本地统计\n{text}"
        );
        for (label, value) in [("主任务", "1.5M"), ("合计", "1.8M"), ("工具调用", "120")] {
            assert!(
                row_with(&state, region, label).is_some_and(|row| row.contains(value)),
                "{host}: {label} {value}\n{text}"
            );
        }
        assert!(find_in(&state, region, "24h").is_some(), "{host}\n{text}");
        assert!(!text.contains('━'), "{host}: 本地统计不画 gauge\n{text}");
    }
    // 快照里没有 zcode 账号：不画 zcode 卡片（客户端不凭空造厂商）。
    let state = cards_page(
        None,
        vec![claude_card_account(), codex_card_account()],
        120,
        40,
    );
    let region = state.observability.page_rect;
    assert!(
        find_in(&state, region, "zcode").is_none(),
        "{}",
        region_text(&state, region)
    );
}

#[test]
fn unknown_vendor_falls_back_to_generic_rows() {
    let _guard = crate::i18n::lang_guard(crate::i18n::Lang::ZhCn);
    use crate::api::schema::UsageMetric;
    let accounts = || {
        vec![vendor_account(
            "future-agent",
            vec![
                UsageMetric {
                    used_percent: Some(33.0),
                    ..usage_metric("window-a", "Window A", "%", "account")
                },
                UsageMetric {
                    amount_decimal: Some("7.5".into()),
                    ..usage_metric("wallet", "Wallet", "USD", "account")
                },
            ],
        )]
    };
    for (host, state, region) in card_hosts("future-agent", accounts) {
        let text = region_text(&state, region);
        assert!(
            row_with(&state, region, "Window A")
                .is_some_and(|row| row.contains("33%") && row.contains('━')),
            "{host}: 未知厂商的百分比指标画通用 meter\n{text}"
        );
        assert!(
            row_with(&state, region, "Wallet").is_some_and(|row| row.contains("$7.50")),
            "{host}: 金额画成数值项\n{text}"
        );
        assert!(
            row_with(&state, region, "future-agent:default")
                .is_some_and(|row| row.contains("已更新")),
            "{host}\n{text}"
        );
    }
}

#[test]
fn overview_is_one_compact_card_per_vendor_and_drills_down() {
    let _guard = crate::i18n::lang_guard(crate::i18n::Lang::ZhCn);
    let accounts = || {
        vec![
            claude_card_account(),
            codex_card_account(),
            opencode_card_account(),
        ]
    };
    // 宽页面（经典布局减去侧栏后正文仍 ≥96 列）：双栏，每厂商一张紧凑卡（最紧张的
    // 额度 + 状态）。
    let state = cards_page(None, accounts(), 150, 40);
    let region = state.observability.page_rect;
    let text = region_text(&state, region);
    assert!(
        region.width >= 98,
        "用例前提：正文 ≥96 列（页面 {region:?}）"
    );
    let claude = find_in(&state, region, "消费额度").expect("claude 的最紧张额度");
    let codex = find_in(&state, region, "7d 窗口").expect("codex 的最紧张额度");
    assert_eq!(claude.1, codex.1, "≥96 列双栏：前两个厂商并排\n{text}");
    assert!(
        row_with(&state, region, "消费额度").is_some_and(|row| row.contains("162.8%")),
        "{text}"
    );
    assert!(
        row_with(&state, region, "┌ opencode ").is_some_and(|row| row.contains("本地统计")),
        "紧凑卡标题取厂商名（没有厂商列表时回退 agent 名），本地统计照样声明\n{text}"
    );
    assert!(find_in(&state, region, "$4.56").is_some(), "{text}");
    assert!(
        find_in(&state, region, "12.50 credits").is_none(),
        "紧凑卡只有首行额度与汇总，不画明细\n{text}"
    );
    // 窄页面：单栏。
    let narrow = cards_page(None, accounts(), 80, 24);
    let region = narrow.observability.page_rect;
    let claude = find_in(&narrow, region, "消费额度").expect("窄页面也有 claude");
    let codex = find_in(&narrow, region, "7d 窗口").expect("窄页面也有 codex");
    assert!(codex.1 > claude.1, "<96 列单栏：卡片上下排");

    // 鼠标：点紧凑卡进入该厂商。
    let mut state = cards_page(None, accounts(), 120, 40);
    let rect = page_hit(
        &state,
        |action| matches!(action, Action::Provider(agent) if agent == "codex"),
    )
    .expect("codex 紧凑卡可点");
    click(&mut state, rect.x + 2, rect.y + 1);
    assert_eq!(
        state.observability.selected_provider.as_deref(),
        Some("codex")
    );
    // 键盘：Tab 到紧凑卡、Enter 进入。
    let mut state = cards_page(None, accounts(), 120, 40);
    let target = state
        .observability
        .hits
        .iter()
        .position(|(_, action)| matches!(action, Action::Provider(agent) if agent == "claude"))
        .expect("claude 紧凑卡在命中表里");
    let chips = state
        .observability
        .hits
        .iter()
        .filter(|(_, action)| matches!(action, Action::Provider(agent) if agent == "claude"))
        .count();
    assert_eq!(chips, 1, "用例前提：没有厂商 chip 抢同一个动作");
    for _ in 0..state.observability.page_hits {
        if state.observability.selected_hit == target {
            break;
        }
        press_key(&mut state, crossterm::event::KeyCode::Tab);
    }
    assert_eq!(state.observability.selected_hit, target);
    press_key(&mut state, crossterm::event::KeyCode::Enter);
    assert_eq!(
        state.observability.selected_provider.as_deref(),
        Some("claude")
    );
}

#[test]
fn empty_accounts_page_uses_the_kit_empty_state() {
    let _guard = crate::i18n::lang_guard(crate::i18n::Lang::ZhCn);
    for (cols, rows) in [(120, 40), (80, 24)] {
        let state = cards_page(None, Vec::new(), cols, rows);
        let region = state.observability.page_rect;
        let text = region_text(&state, region);
        let title = find_in(&state, region, "暂无账号用量").expect("空态标题");
        assert!(
            cell_style(&state, title)
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD),
            "kit 空态标题加粗\n{text}"
        );
        assert!(
            find_in(&state, region, "请选择厂商以查询对应的官方用量。").is_some(),
            "{cols}x{rows}: 空态说明\n{text}"
        );
    }
}

#[test]
fn hover_card_folds_rows_beyond_its_height_into_a_plus_marker() {
    let _guard = crate::i18n::lang_guard(crate::i18n::Lang::ZhCn);
    use crate::api::schema::UsageMetric;
    // 一张比悬浮层还高的卡：多条额度窗口。
    let mut account = kimi_card_account();
    for index in 0..8 {
        account.metrics.push(UsageMetric {
            used_percent: Some(10.0 + f64::from(index)),
            ..usage_metric(
                &format!("window-{index}"),
                &format!("额度窗口 {index}"),
                "%",
                "account",
            )
        });
    }
    let state = cards_hover("kimi", vec![account.clone()]);
    let region = state.observability.hover_rect;
    let text = region_text(&state, region);
    // 卡片被悬浮层高度封顶：下边框写「+N」，前几条额度的数字照样可见。
    let marker = (region.y..region.bottom()).find_map(|y| {
        let buffer = state.compose_buffer.as_ref().expect("帧缓冲");
        let (row, _) = region_row(buffer, region, y);
        let start = row.find(" +")?;
        let digits = row[start + 2..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>();
        (!digits.is_empty() && row.contains('┘')).then_some(digits)
    });
    let hidden = marker.expect("下边框的「+N」");
    assert!(hidden.parse::<usize>().is_ok_and(|n| n > 0), "{text}");
    assert!(
        row_with(&state, region, "5 小时").is_some_and(|row| row.contains("25%")),
        "{text}"
    );
    assert!(
        find_in(&state, region, "打开页面").is_some(),
        "悬浮层底部的「打开页面」仍在\n{text}"
    );
    // 同一张卡在页面上不封顶：放不下时滚轮往下能看到最后一条窗口。
    let mut page = cards_page(Some("kimi"), vec![account], 80, 24);
    let region = page.observability.page_rect;
    assert!(
        find_in(&page, region, "额度窗口 7").is_none(),
        "用例前提：80×24 放不下整张卡\n{}",
        region_text(&page, region)
    );
    let point = find_in(&page, region, "5 小时").expect("卡片在页面上");
    for _ in 0..40 {
        page.handle_mouse(
            MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: point.0,
                row: point.1,
                modifiers: KeyModifiers::NONE,
            },
            &mut ClientShellInput::default(),
        );
    }
    page.compose(80, 24).expect("滚动后");
    assert!(
        find_in(&page, region, "额度窗口 7").is_some(),
        "页面上滚动能看到每一条额度\n{}",
        region_text(&page, region)
    );
}
