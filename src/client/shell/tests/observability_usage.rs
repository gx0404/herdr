//! 「监控 → 账号用量」characterization 用例：光标归属、面板可关闭与 tab 持久化。
//!
//! 光标归属规则：只有真正覆盖终端光标坐标的页面 / 悬浮层 / 对话框 / overlay
//! 才把整帧光标置空；停靠在旁边的监控面板不能抹掉聚焦终端的插入点。

use super::*;
use crate::client::shell::dock::{Edge, PanelId};
use crate::client::shell::observability::{Action, Hover, Page};
use crate::client::shell::workbench::{body, View};
use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

/// 启用停靠工作台并注入终端视图；没有视图时终端面板落占位分支、根本没有光标。
fn docked() -> ClientShellState {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
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
                accounts: Vec::new()
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
