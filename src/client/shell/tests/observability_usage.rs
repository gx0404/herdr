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
use crate::client::shell::observability::{Action, Hover, Page, Purpose};
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
    state.sync_workbench_surface();
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
fn floating_usage_dashboard_keeps_the_uncovered_terminal_cursor() {
    let mut state = docked();
    let before = state.compose(120, 40).expect("打开前画面");
    let cursor = before.cursor.clone().expect("终端光标");
    state.toggle_usage_dashboard(&mut ClientShellInput::default());
    let frame = state.compose(120, 40).expect("浮动用量仪表盘");
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
    // 锚点放在光标左侧，悬浮层从锚点右侧展开即覆盖光标坐标。
    state.observability.hover = Some(Hover {
        endpoint_id: state.active_endpoint_id.clone(),
        pane: "pane_1".into(),
        agent: "claude".into(),
        anchor: Rect::new(cursor.x.saturating_sub(2), cursor.y, 1, 1),
        since: std::time::Instant::now(),
        visible: true,
        leave_at: None,
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
            Purpose::Usage,
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
    let blank = Buffer::empty(Rect::new(0, 0, 120, 40));
    let mut beside =
        FrameData::from_ratatui_buffer_with_hyperlinks(&blank, Some(cursor.clone()), &[]);
    let painted = state
        .paint(
            &mut beside,
            Rect::new(60, 1, 60, 38),
            &config.palette,
            Some(Page::Monitor),
        )
        .expect("页面已绘制");
    assert_eq!(painted.page_rect, Rect::new(60, 1, 60, 38));
    assert_eq!(
        beside.cursor,
        Some(cursor.clone()),
        "页面矩形不含光标时保留"
    );

    let mut covering = FrameData::from_ratatui_buffer_with_hyperlinks(&blank, Some(cursor), &[]);
    state
        .paint(
            &mut covering,
            Rect::new(0, 1, 120, 38),
            &config.palette,
            Some(Page::Monitor),
        )
        .expect("页面已绘制");
    assert!(covering.cursor.is_none(), "页面矩形覆盖光标时置空");
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
    let blank = Buffer::empty(Rect::new(0, 0, 40, 10));
    let mut target = FrameData::from_ratatui_buffer_with_hyperlinks(&blank, None, &[]);
    blit_pane_surface(&mut target, &source.frame, Rect::new(10, 2, 2, 1));
    assert_eq!(
        target.cursor,
        Some(crate::protocol::CursorState {
            x: 11,
            y: 2,
            visible: false,
            shape: 2,
        }),
        "越界光标夹回边缘且不可见"
    );
    let mut fits = FrameData::from_ratatui_buffer_with_hyperlinks(&blank, None, &[]);
    blit_pane_surface(&mut fits, &source.frame, Rect::new(10, 2, 10, 5));
    assert_eq!(
        fits.cursor,
        Some(crate::protocol::CursorState {
            x: 13,
            y: 3,
            visible: true,
            shape: 2,
        }),
        "区域内的光标保持可见"
    );
    let mut empty = FrameData::from_ratatui_buffer_with_hyperlinks(&blank, None, &[]);
    blit_pane_surface(&mut empty, &source.frame, Rect::new(10, 2, 0, 0));
    assert!(empty.cursor.is_none(), "空区域没有光标可放");
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

/// 停靠工作台 + 全部用量端点方法 + 聚焦 pane 运行 claude 的快照。
fn usage_ready() -> ClientShellState {
    let mut snapshot = snapshot();
    snapshot.agents.push(agent_in_pane("pane_1", "claude"));
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

fn deliver_usage(state: &mut ClientShellState, accounts: Vec<AccountUsageSnapshot>) -> bool {
    let epoch = state.observability.epoch;
    state.receive_observation(
        epoch,
        Purpose::Usage,
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
        Purpose::Usage,
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
        endpoint_id: state.active_endpoint_id.clone(),
        pane: "pane_1".into(),
        agent: "claude".into(),
        anchor: Rect::new(0, 20, 24, 2),
        since: Instant::now() - Duration::from_secs(1),
        visible: false,
        leave_at: None,
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
        Purpose::HoverUsage,
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
        endpoint_id: ghost.clone(),
        pane: "pane_1".into(),
        agent: "claude".into(),
        anchor: Rect::new(0, 20, 24, 2),
        since: Instant::now() - Duration::from_secs(1),
        visible: false,
        leave_at: None,
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

    // 浮动仪表盘是总览：不自动选厂商。
    plain.toggle_usage_dashboard(&mut ClientShellInput::default());
    assert_eq!(plain.observability.selected_provider, None);
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

#[test]
fn opening_the_usage_dashboard_resets_hover_scope_and_forces_a_refresh() {
    let mut state = usage_ready();
    state.observability.hover = Some(Hover {
        endpoint_id: state.active_endpoint_id.clone(),
        pane: "pane_1".into(),
        agent: "claude".into(),
        anchor: Rect::new(0, 20, 24, 2),
        since: Instant::now() - Duration::from_secs(1),
        visible: false,
        leave_at: None,
    });
    let t0 = Instant::now() + Duration::from_secs(1);
    tick(&mut state, t0);
    assert_eq!(
        state.observability.hover_scope.pane.as_deref(),
        Some("pane_1")
    );
    state.toggle_usage_dashboard(&mut ClientShellInput::default());
    assert!(state.observability.hover.is_none());
    assert_eq!(state.observability.hover_scope.pane, None);
    assert_eq!(state.observability.hover_scope.provider, None);
    assert_eq!(state.observability.hover_scope.endpoint, None);
    let calls = usage_calls(&tick(&mut state, t0 + Duration::from_millis(10)));
    assert_eq!(calls.len(), 1);
    assert!(calls[0].0, "打开仪表盘 = 强意图刷新");
    assert_eq!(calls[0].1.pane_id, None);
    assert_eq!(calls[0].1.agent, None, "仪表盘是跨厂商总览");
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
        endpoint_id,
        pane: pane.into(),
        agent: agent.into(),
        anchor: Rect::new(0, 20, 24, 2),
        since: Instant::now() - Duration::from_secs(1),
        visible: false,
        leave_at: None,
    });
}

fn deliver_hover_usage(state: &mut ClientShellState, accounts: Vec<AccountUsageSnapshot>) -> bool {
    let epoch = state.observability.hover_scope.epoch;
    state.receive_observation(
        epoch,
        Purpose::HoverUsage,
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
            Purpose::HoverUsage,
            Ok(ResponseResult::AccountUsage {
                accounts: vec![account("claude", "claude:default")],
                refresh: None,
            }),
        ),
        "旧代际的悬浮层响应被丢弃"
    );
    assert!(state.observability.hover_scope.accounts.is_empty());
}
