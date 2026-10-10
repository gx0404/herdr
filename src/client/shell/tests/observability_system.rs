//! 「监控」面板 characterization 用例：光标归属、面板可关闭与 tab 持久化、系统页采样
//! 节流与告警需求、卡片布局与滚动、进程对话框与偏好写回。
//!
//! 光标归属规则：只有真正覆盖终端光标坐标的页面 / 对话框 / overlay 才把整帧光标
//! 置空；停靠在旁边的监控面板不能抹掉聚焦终端的插入点。

use super::*;
use crate::api::schema::{Method, ResponseResult};
use crate::client::shell::dock::{Edge, PanelId};
use crate::client::shell::observability::{Action, Page};
use crate::client::shell::workbench::{body, View};
use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use std::time::{Duration, Instant};

/// 启用停靠工作台并注入终端视图；没有视图时终端面板落占位分支、根本没有光标。
fn docked() -> ClientShellState {
    docked_with(snapshot())
}

fn docked_with(snapshot: ClientShellSnapshot) -> ClientShellState {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot));
    state.set_endpoint_methods(Some(vec![
        "client.views.set".into(),
        "tab.focus".into(),
        "pane.focus".into(),
    ]));
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
        .map(|(panel, area)| body(*area, panel, state.workbench.geometry.compact))
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
fn classic_layout_monitor_page_still_owns_the_cursor() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    assert!(
        !state.workbench.enabled,
        "未通告 client.views.set 时走经典布局"
    );
    state.open_observation_page(Page::Monitor, &mut ClientShellInput::default());
    let frame = state.compose(120, 40).expect("经典布局监控页");
    assert!(
        frame.cursor.is_none(),
        "监控页铺满 pane 区域时光标归页面所有"
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
    state.open_observation_page(Page::Settings, &mut ClientShellInput::default());
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
fn selected_monitor_tab_survives_terminal_focus_and_keeps_system_polling() {
    let mut state = docked();
    state.set_endpoint_methods(Some(vec![
        "client.views.set".into(),
        "tab.focus".into(),
        "pane.focus".into(),
        "system.metrics.get".into(),
    ]));
    state.open_observation_page(Page::Settings, &mut ClientShellInput::default());
    assert_eq!(state.observability.page, Some(Page::Settings));
    state.compose(120, 40).expect("偏好页停靠");
    let terminal = terminal_body(&state);
    click(&mut state, terminal.x + 1, terminal.y + 1);
    assert_eq!(state.workbench.dock.focused, PanelId::Terminal(1));
    assert_eq!(state.observability.page, None, "键盘回到终端");
    assert_eq!(
        state.observability.monitor_tab,
        Page::Settings,
        "用户选中的 tab 不因焦点变化回落到系统页"
    );
    state.compose(120, 40).expect("终端聚焦、偏好页仍停靠");
    assert_eq!(
        state.observability.monitor_tab,
        Page::Settings,
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
        Some(Page::Settings),
        "点回面板时仍是偏好页"
    );
    // 换到系统页再把键盘交回终端：停靠的系统页照常采样。
    state.open_observation_page(Page::Monitor, &mut ClientShellInput::default());
    state.compose(120, 40).expect("系统页停靠");
    click(&mut state, terminal.x + 1, terminal.y + 1);
    assert_eq!(state.observability.page, None);
    let outcome = tick(&mut state, Instant::now() + Duration::from_secs(1));
    assert!(
        outcome.actions.iter().any(|action| matches!(
            action,
            ClientShellAction::Endpoint { request, .. }
                if matches!(request.method, Method::SystemMetricsGet(_))
        )),
        "系统页停靠时即使终端聚焦也继续采样"
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

fn tick(state: &mut ClientShellState, now: Instant) -> ClientShellInput {
    let mut outcome = ClientShellInput::default();
    state.tick_observability(now, &mut outcome);
    outcome
}

/// 停靠工作台 + 系统监控端点方法。预热一次 tick：真实客户端连上后持续 tick，活动
/// source 早已建立；否则用例里的首个 tick 会触发「换端点」大清理，掩盖被测行为。
fn system_ready() -> ClientShellState {
    let mut state = docked();
    state.set_endpoint_methods(Some(vec![
        "client.views.set".into(),
        "tab.focus".into(),
        "pane.focus".into(),
        "system.metrics.get".into(),
    ]));
    tick(&mut state, Instant::now());
    state
}

fn page_hit(state: &ClientShellState, matches: impl Fn(&Action) -> bool) -> Option<Rect> {
    state
        .observability
        .hits
        .iter()
        .find(|(_, action)| matches(action))
        .map(|(rect, _)| *rect)
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

#[test]
fn audit_p1_monitor_samples_only_visible_system_page_or_enabled_alerts() {
    for (page, alerts, expected) in [
        (Page::Settings, false, 0),
        (Page::Monitor, false, 1),
        (Page::Settings, true, 1),
    ] {
        let mut state = system_ready();
        state.set_endpoint_methods(Some(vec![
            "client.views.set".into(),
            "system.metrics.get".into(),
        ]));
        state.workbench_open(PanelId::Monitor);
        state.observability.monitor_tab = page;
        state.observability.page = None;
        state.observability.monitor.alerts_enabled = alerts;
        state.workbench.dock.focused = PanelId::Terminal(1);
        state.compose(120, 40).expect("docked monitor page");
        assert!(state.workbench.visible(&PanelId::Monitor));
        let outcome = tick(&mut state, Instant::now() + Duration::from_secs(1));
        let metrics = outcome
            .actions
            .iter()
            .filter(|action| {
                matches!(
                    action,
                    ClientShellAction::Endpoint { request, .. }
                        if matches!(request.method, Method::SystemMetricsGet(_))
                )
            })
            .count();

        assert_eq!(metrics, expected, "page={page:?}, alerts={alerts}");
    }
}

#[test]
fn audit_p1_monitor_alert_demand_tracks_rules_instead_of_hidden_cards() {
    for (page, visible, rules, expected) in [
        (
            Page::Settings,
            vec!["processes", "network"],
            vec!["memory", "disk"],
            vec!["memory", "disks"],
        ),
        (Page::Monitor, vec!["cpu"], vec!["gpu"], vec!["cpu", "gpu"]),
        (Page::Settings, vec!["cpu"], Vec::new(), Vec::new()),
    ] {
        let mut state = system_ready();
        state.set_endpoint_methods(Some(vec![
            "client.views.set".into(),
            "system.metrics.get".into(),
        ]));
        state.workbench_open(PanelId::Monitor);
        state.observability.monitor_tab = page;
        state.observability.page = None;
        state.observability.monitor.alerts_enabled = true;
        state.observability.monitor.visible = visible.into_iter().map(str::to_owned).collect();
        state
            .observability
            .monitor
            .alerts
            .retain(|rule| rules.contains(&rule.metric.as_str()));
        assert_eq!(state.observability.monitor.alerts.len(), rules.len());
        state.workbench.dock.focused = PanelId::Terminal(1);
        state.compose(120, 40).expect("docked monitor page");
        let outcome = tick(&mut state, Instant::now() + Duration::from_secs(1));
        let requests: Vec<_> = outcome
            .actions
            .iter()
            .filter_map(|action| match action {
                ClientShellAction::Endpoint { request, .. } => match &request.method {
                    Method::SystemMetricsGet(params) => Some(params),
                    _ => None,
                },
                _ => None,
            })
            .collect();

        if expected.is_empty() {
            assert!(requests.is_empty(), "no system page and no alert rules");
        } else {
            assert_eq!(requests.len(), 1);
            assert!(!requests[0].include_processes);
            let mut groups = requests[0].groups.clone();
            let mut expected: Vec<_> = expected.into_iter().map(str::to_owned).collect();
            groups.sort_unstable();
            expected.sort_unstable();
            assert_eq!(groups, expected);
        }
    }
}

fn system_metric_calls(
    outcome: &ClientShellInput,
) -> Vec<(String, crate::api::schema::SystemMetricsParams)> {
    outcome
        .actions
        .iter()
        .filter_map(|action| match action {
            ClientShellAction::Endpoint { request, .. } => match &request.method {
                Method::SystemMetricsGet(params) => Some((request.id.clone(), params.clone())),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

fn classic_metric_client() -> ClientShellState {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_endpoint_methods(Some(vec!["system.metrics.get".into()]));
    assert!(!state.workbench.enabled);
    state
}

#[test]
fn audit_p1_monitor_classic_pause_keeps_sampling_and_inflight_dedup() {
    for paused in [false, true] {
        let mut state = classic_metric_client();
        state.open_observation_page(Page::Monitor, &mut ClientShellInput::default());
        state.observability.paused = paused;
        state.observability.monitor.interval_ms = 500;
        let t0 = Instant::now() + Duration::from_secs(1);
        let first = system_metric_calls(&tick(&mut state, t0));
        assert_eq!(first.len(), 1);
        assert!(system_metric_calls(&tick(&mut state, t0 + Duration::from_secs(2))).is_empty());
        let boot_id = state.snapshot.as_ref().unwrap().boot_id.clone();
        let mut sample = smoke_system_sample();
        sample.boot_id = boot_id.clone();
        sample.sampled_at_ms = state.observability.now_ms;
        state.handle_endpoint_result(
            &boot_id,
            &first[0].0,
            Ok(ResponseResult::SystemMetrics { snapshot: sample }),
        );
        assert_eq!(
            state.observability.metrics.as_ref().unwrap().cpu_percent,
            Some(42.0)
        );
        assert_eq!(state.observability.history.len(), usize::from(!paused));
        let next = system_metric_calls(&tick(&mut state, t0 + Duration::from_secs(4)));
        assert_eq!(next.len(), 1);
        assert_ne!(first[0].0, next[0].0);
    }
}

#[test]
fn audit_p1_monitor_hidden_alerts_ignore_unknown_rules() {
    for recognized in [true, false] {
        let mut state = classic_metric_client();
        state.observability.monitor.alerts_enabled = true;
        state
            .observability
            .monitor
            .alerts
            .retain(|rule| rule.metric == "memory");
        assert_eq!(state.observability.monitor.alerts.len(), 1);
        if !recognized {
            state.observability.monitor.alerts[0].metric = "unknown-metric".into();
        }
        assert_eq!(state.observability.page, None);
        let requests = system_metric_calls(&tick(&mut state, Instant::now()));
        if recognized {
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].1.groups, ["memory"]);
            assert!(!requests[0].1.include_processes);
        } else {
            assert!(requests.is_empty());
        }
    }
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

/// 冒烟尺寸下的主机快照：只要采样时刻有效、带几项数据，系统页就画出全部卡片。
fn smoke_system_sample() -> Box<crate::api::schema::SystemMetricsSnapshot> {
    use crate::api::schema::{CpuCoreMetric, SystemMetricsSnapshot};
    Box::new(SystemMetricsSnapshot {
        boot_id: "boot".into(),
        sequence: 1,
        sampled_at_ms: 1_000,
        hostname: "devbox".into(),
        cpu_percent: Some(42.0),
        cores: (0..4)
            .map(|id| CpuCoreMetric {
                id,
                name: format!("cpu{id}"),
                usage_percent: Some(40.0),
                frequency_mhz: None,
            })
            .collect(),
        ..Default::default()
    })
}

/// 本帧完整画出的系统页卡片 id，按从上到下、从左到右排列；被页面底边裁掉、
/// 只露出一截的卡片（卡高不足）不算。
fn painted_cards(state: &ClientShellState) -> Vec<String> {
    let full = state.observability.monitor.card_height;
    let mut cards = state
        .observability
        .hits
        .iter()
        .filter_map(|(rect, action)| match action {
            Action::Card(id) if rect.height == full => Some((rect.y, rect.x, id.clone())),
            _ => None,
        })
        .collect::<Vec<_>>();
    cards.sort();
    cards.into_iter().map(|(_, _, id)| id).collect()
}

/// 像真实终端那样注入一个 SGR-1006 鼠标事件（参数坐标 0 起，序列里 1 起）。
fn sgr_mouse(state: &mut ClientShellState, button: u16, column: u16, row: u16) {
    let report = format!("\x1b[<{button};{};{}M", column + 1, row + 1);
    let events = crate::raw_input::parse_raw_input_bytes_sync(report.as_bytes());
    assert!(!events.is_empty(), "SGR 序列可解析");
    state.handle_raw_events(events);
}

const SGR_WHEEL_UP: u16 = 64;
const SGR_WHEEL_DOWN: u16 = 65;

/// 冒烟 S1 / M1（133×32：停靠面板约 53 列宽，系统页单列、一屏两张卡）：滚轮与
/// ↓ 每格前进一行卡片，依次露出全部 8 张卡（含曾经不可达的「内存」「网络」）；
/// 滚到底后多滚的格数不留存（写回钳位），反向第一格画面立刻变化；PageDown /
/// PageUp 一次一屏。
#[test]
fn system_page_scrolls_one_card_row_per_notch_and_reaches_every_card() {
    use crossterm::event::KeyCode;
    let _guard = crate::i18n::lang_guard(crate::i18n::Lang::ZhCn);
    let mut state = docked();
    state.open_observation_page(Page::Monitor, &mut ClientShellInput::default());
    state.observability.metrics = Some(smoke_system_sample());
    state.compose(133, 32).expect("系统页");
    let order = state.observability.monitor.visible.clone();
    assert_eq!(order.len(), 8, "默认 8 张卡");
    assert_eq!(
        painted_cards(&state),
        order[..2],
        "用例前提：单列、一屏两张卡"
    );
    let cpu = page_hit(
        &state,
        |action| matches!(action, Action::Card(id) if id == "cpu"),
    )
    .expect("CPU 卡");
    // 摘要条与首张卡之间的空行：在页面里、不在任何卡片上。
    let (column, row) = (cpu.x + 2, cpu.y - 1);
    let mut seen = painted_cards(&state);
    for step in 1..=6 {
        sgr_mouse(&mut state, SGR_WHEEL_DOWN, column, row);
        state.compose(133, 32).expect("重绘");
        let cards = painted_cards(&state);
        assert_eq!(cards, order[step..step + 2], "第 {step} 格前进一张卡");
        seen.extend(cards);
    }
    for id in &order {
        assert!(seen.contains(id), "{id} 卡可以滚到");
    }
    // 到底后多滚：位置钳在上界，不存「死格」。
    for _ in 0..5 {
        sgr_mouse(&mut state, SGR_WHEEL_DOWN, column, row);
    }
    state.compose(133, 32).expect("重绘");
    assert_eq!(painted_cards(&state), order[6..], "最后一张卡完整露出");
    assert_eq!(state.observability.scroll, 6, "滚动位置写回钳位");
    sgr_mouse(&mut state, SGR_WHEEL_UP, column, row);
    state.compose(133, 32).expect("重绘");
    assert_eq!(painted_cards(&state), order[5..7], "反向第一格画面就动");
    // 键盘同口径：↑ / ↓ 一行卡片，PageUp / PageDown 一屏（两行）。
    press_key(&mut state, KeyCode::Up);
    state.compose(133, 32).expect("重绘");
    assert_eq!(painted_cards(&state), order[4..6]);
    press_key(&mut state, KeyCode::PageUp);
    state.compose(133, 32).expect("重绘");
    assert_eq!(painted_cards(&state), order[2..4], "一屏两张卡");
    press_key(&mut state, KeyCode::PageUp);
    press_key(&mut state, KeyCode::PageUp);
    state.compose(133, 32).expect("重绘");
    assert_eq!(painted_cards(&state), order[..2]);
    assert_eq!(state.observability.scroll, 0, "顶端同样钳位");
    press_key(&mut state, KeyCode::Down);
    state.compose(133, 32).expect("重绘");
    assert_eq!(painted_cards(&state), order[1..3], "↓ 露出内存卡");
}

/// 把焦点交给终端（点终端正文）并重绘：停靠的监控面板照画 `monitor_tab`，键盘归终端。
/// 宣告 `pane.focus`：点窗格会发这个请求，未宣告时的「操作不可用」浮条会盖住面板头。
fn focus_terminal(state: &mut ClientShellState, cols: u16, rows: u16) {
    state.set_endpoint_methods(Some(vec![
        "client.views.set".into(),
        "tab.focus".into(),
        "pane.focus".into(),
    ]));
    let terminal = terminal_body(state);
    click(state, terminal.x + 1, terminal.y + 1);
    assert_eq!(state.workbench.dock.focused, PanelId::Terminal(1));
    assert_eq!(state.observability.page, None, "键盘归终端");
    state.compose(cols, rows).expect("终端聚焦、监控面板照画");
}

/// D5（文档终审轻级，与 H2 / N4 同一规则；三档：250×40 双列、133×32 与 80×24 单列，
/// 都停靠在终端旁）：监控面板停靠但没聚焦（键盘在终端上）时，卡片之间的空隙与偏好页
/// 上的滚轮不响应——页面滚动只认聚焦的页面。现在滚轮按指针落点滚面板画着的那一页，
/// 不要求先聚焦，焦点与键盘都不动：空隙与内容放得下的卡片滚页面、放不下的卡片滚自己；
/// 偏好页按行滚。反过来，偏好页聚焦时指针在终端上的滚轮也不滚偏好页。
#[test]
fn wheel_scrolls_the_docked_monitor_page_without_focusing_it() {
    use crossterm::event::KeyCode;
    let _guard = crate::i18n::lang_guard(crate::i18n::Lang::ZhCn);
    for (cols, rows) in [(250, 40), (133, 32), (80, 24)] {
        let size = format!("{cols}×{rows}");
        // 系统页：卡片之间的空隙。
        let mut state = docked();
        state.open_observation_page(Page::Monitor, &mut ClientShellInput::default());
        state.observability.metrics = Some(smoke_system_sample());
        state.compose(cols, rows).expect("系统页");
        focus_terminal(&mut state, cols, rows);
        let card = |state: &ClientShellState, id: &str| {
            page_hit(
                state,
                |action| matches!(action, Action::Card(card) if card == id),
            )
            .unwrap_or_else(|| panic!("{size}：{id} 卡已画出"))
        };
        let cpu = card(&state, "cpu");
        let mut gaps = vec![
            // 摘要条与首张卡之间的空行。
            (cpu.x + 2, cpu.y - 1),
            // 首行卡片与下一行之间的空行。
            (cpu.x + 2, cpu.bottom()),
        ];
        if cpu.right() < state.observability.page_rect.right() - 2 {
            // 双列：两张卡之间的那一列。
            gaps.push((cpu.right(), cpu.y + 2));
        }
        let first = painted_cards(&state);
        for (column, row) in gaps {
            sgr_mouse(&mut state, SGR_WHEEL_DOWN, column, row);
            assert_eq!(
                state.observability.scroll, 1,
                "{size}：({column}, {row}) 空隙上的滚轮滚动页面"
            );
            state.compose(cols, rows).expect("滚一格");
            assert_ne!(painted_cards(&state), first, "{size}：画面跟着动");
            sgr_mouse(&mut state, SGR_WHEEL_UP, column, row);
            assert_eq!(state.observability.scroll, 0, "{size}：反向滚回");
            state.compose(cols, rows).expect("滚回");
            assert_eq!(painted_cards(&state), first);
        }
        // 放得下的卡片（CPU）把滚轮交给页面（H2），同样不要求聚焦。
        let cpu = card(&state, "cpu");
        sgr_mouse(&mut state, SGR_WHEEL_DOWN, cpu.x + 3, cpu.y + 2);
        assert_eq!(
            state.observability.scroll, 1,
            "{size}：CPU 卡上的滚轮滚页面"
        );
        assert_eq!(
            state.workbench.dock.focused,
            PanelId::Terminal(1),
            "{size}：滚轮不抢焦点"
        );
        assert_eq!(state.observability.page, None, "{size}：键盘仍归终端");
        // 键盘路径：焦点在终端时 ↓ 落到终端，不滚页面。
        press_key(&mut state, KeyCode::Down);
        assert_eq!(state.observability.scroll, 1, "{size}：↓ 不滚未聚焦的页面");

        // 偏好页：面板任意内容区。一打告警规则：偏好页在每档尺寸下都高过一屏。
        let mut state = docked();
        state.open_observation_page(Page::Settings, &mut ClientShellInput::default());
        let rule = state.observability.monitor.alerts[0].clone();
        state.observability.monitor.alerts = vec![rule; 12];
        state.compose(cols, rows).expect("偏好页");
        focus_terminal(&mut state, cols, rows);
        let page = state.observability.page_rect;
        let before = region_text(&state, page);
        let (column, row) = (page.x + page.width / 2, page.y + page.height / 2);
        sgr_mouse(&mut state, SGR_WHEEL_DOWN, column, row);
        assert_eq!(
            state.observability.settings_scroll, 3,
            "{size}：偏好页上的滚轮一格三行"
        );
        state.compose(cols, rows).expect("滚一格");
        assert_ne!(
            region_text(&state, page),
            before,
            "{size}：偏好页画面跟着动"
        );
        assert_eq!(state.workbench.dock.focused, PanelId::Terminal(1));
        press_key(&mut state, KeyCode::Down);
        assert_eq!(
            state.observability.settings_scroll, 3,
            "{size}：↓ 不滚未聚焦的偏好页"
        );

        // 反过来：偏好页聚焦时，指针在终端上的滚轮不滚偏好页。
        let header = monitor_header(&state);
        click(&mut state, header.x + header.width / 2, header.y);
        assert_eq!(
            state.observability.page,
            Some(Page::Settings),
            "{size}：点面板头把焦点交回监控面板"
        );
        let terminal = terminal_body(&state);
        sgr_mouse(&mut state, SGR_WHEEL_DOWN, terminal.x + 1, terminal.y + 1);
        assert_eq!(
            state.observability.settings_scroll, 3,
            "{size}：终端上的滚轮不滚偏好页"
        );
    }
}

/// 复审轻级 2（D5 的经典布局一侧；三档 160×48 / 120×40 / 80×24，都宽于移动布局
/// 阈值）：经典布局下监控页铺在 pane 区，滚轮同样按落点分派：侧栏上的滚轮照常滚侧栏、
/// 不动页面，页面里的滚轮照常滚页面、不动侧栏。
#[test]
fn classic_preferences_page_leaves_the_sidebar_wheel_to_the_sidebar() {
    let _guard = crate::i18n::lang_guard(crate::i18n::Lang::ZhCn);
    for (cols, rows) in [(160, 48), (120, 40), (80, 24)] {
        let size = format!("{cols}×{rows}");
        // 30 个工作区：侧栏的工作区列表放不下，滚得动。
        let mut snapshot = snapshot();
        let first = snapshot.workspaces[0].clone();
        snapshot.workspaces = (1..=30)
            .map(|number| {
                let mut workspace = first.clone();
                workspace.workspace_id = format!("ws_{number}");
                workspace.number = number;
                workspace.focused = number == 1;
                workspace
            })
            .collect();
        let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
        state.set_snapshot(Box::new(snapshot));
        state.set_pane_surface(surface());
        state.open_observation_page(Page::Settings, &mut ClientShellInput::default());
        // 一打告警规则：偏好页在每档尺寸下都高过一屏。
        let rule = state.observability.monitor.alerts[0].clone();
        state.observability.monitor.alerts = vec![rule; 12];
        state.compose(cols, rows).expect("经典布局的偏好页");
        assert!(!state.workbench.enabled, "{size}：用例前提：经典布局");
        assert!(
            state.hits.workspace_max_scroll > 0,
            "{size}：用例前提：侧栏的工作区列表放不下"
        );
        assert!(
            state
                .observability
                .page_scroll_limits
                .get(Page::Settings)
                .is_some_and(|limits| limits.max > 0),
            "{size}：用例前提：偏好页高过一屏"
        );
        let sidebar = state.hits.workspace_body;
        let page = state.observability.page_rect;
        assert!(
            !contains(page, (sidebar.x, sidebar.y)),
            "{size}：用例前提：侧栏不在页面矩形里"
        );

        sgr_mouse(&mut state, SGR_WHEEL_DOWN, sidebar.x + 1, sidebar.y + 1);
        assert_eq!(
            state.observability.settings_scroll, 0,
            "{size}：侧栏上的滚轮不滚页面"
        );
        assert_eq!(
            state.workspace_scroll, state.config.mouse_scroll_lines,
            "{size}：侧栏上的滚轮照常滚侧栏"
        );

        let before = region_text(&state, page);
        sgr_mouse(
            &mut state,
            SGR_WHEEL_DOWN,
            page.x + page.width / 2,
            page.y + page.height / 2,
        );
        assert_eq!(
            state.observability.settings_scroll, 3,
            "{size}：页面里的滚轮照常滚页面"
        );
        assert_eq!(
            state.workspace_scroll, state.config.mouse_scroll_lines,
            "{size}：页面里的滚轮不动侧栏"
        );
        state.compose(cols, rows).expect("滚一格");
        assert_ne!(
            region_text(&state, page),
            before,
            "{size}：偏好页画面跟着动"
        );
    }
}

/// 每张卡都有数据的主机快照：卡片被页面底边裁掉时，露出的一截能看出真实首行。
fn populated_system_sample() -> Box<crate::api::schema::SystemMetricsSnapshot> {
    use crate::api::schema::{
        DiskMetric, GpuMetric, MemoryMetric, NetworkMetric, ProcessIdentity, ProcessMetric,
        SensorMetric,
    };
    const GIB: u64 = 1024 * 1024 * 1024;
    let mut sample = smoke_system_sample();
    sample.memory = MemoryMetric {
        total_bytes: 16 * GIB,
        used_bytes: 6 * GIB,
        available_bytes: 8 * GIB,
        swap_total_bytes: 2 * GIB,
        swap_used_bytes: 0,
    };
    sample.gpus = vec![GpuMetric {
        id: "gpu0".into(),
        name: "Test GPU".into(),
        usage_percent: Some(30.0),
        memory_used_bytes: Some(2 * GIB),
        memory_total_bytes: Some(8 * GIB),
        temperature_celsius: Some(65.0),
        ..Default::default()
    }];
    sample.disks = vec![DiskMetric {
        id: "/dev/sda1:/".into(),
        name: "/dev/sda1".into(),
        mount_point: "/".into(),
        total_bytes: 500 * GIB,
        available_bytes: 100 * GIB,
        ..Default::default()
    }];
    sample.networks = vec![NetworkMetric {
        id: "eth0".into(),
        received_bytes_per_second: Some(1_048_576.0),
        transmitted_bytes_per_second: Some(2048.0),
        ..Default::default()
    }];
    sample.sensors = vec![SensorMetric {
        name: "coretemp Core 0".into(),
        temperature_celsius: Some(60.0),
        critical_celsius: None,
    }];
    sample.processes = (1..=3)
        .map(|pid| ProcessMetric {
            identity: ProcessIdentity {
                pid,
                ..Default::default()
            },
            name: format!("proc{pid}"),
            cpu_percent: Some(1.0),
            memory_bytes: 1024 * 1024,
            ..Default::default()
        })
        .collect();
    sample
}

/// 复审（L13 回归）：停靠面板的页面去掉自带外框后正文多出两行，冒烟尺寸下（新
/// 会话默认停靠布局：工作区 | 终端 | 监控三栏，监控面板占满正文高度）除最后一屏
/// 外，页面底边都会露出下一行卡片的一截。露出的一截是那张卡的上半部分、裁在
/// 视口边缘——标题加真实的首行内容、没有下边框——而不是压扁成一张只有上下边框
/// 的「空卡」；它的命中区只含可见部分、不压页脚。
#[test]
fn a_card_cut_by_the_page_bottom_shows_its_top_rows_instead_of_an_empty_frame() {
    let _guard = crate::i18n::lang_guard(crate::i18n::Lang::ZhCn);
    for (width, height) in [(133_u16, 32_u16), (93, 32)] {
        let mut state = docked();
        state.open_observation_page(Page::Monitor, &mut ClientShellInput::default());
        state.observability.metrics = Some(populated_system_sample());
        state.compose(width, height).expect("系统页");
        // 前提：真实默认停靠布局——三栏，监控面板与终端同高（占满正文）。
        let panel = |wanted: &dyn Fn(&PanelId) -> bool| {
            state
                .workbench
                .geometry
                .panels
                .iter()
                .find(|(panel, _)| wanted(panel))
                .map(|(_, area)| *area)
                .expect("面板在布局中")
        };
        let workspaces = panel(&|panel| *panel == PanelId::Workspaces);
        let terminal = panel(&|panel| matches!(panel, PanelId::Terminal(_)));
        let monitor = panel(&|panel| *panel == PanelId::Monitor);
        assert!(
            workspaces.right() <= terminal.x && terminal.right() <= monitor.x,
            "{width}×{height}：工作区 | 终端 | 监控三栏"
        );
        assert_eq!(
            (monitor.y, monitor.height),
            (terminal.y, terminal.height),
            "{width}×{height}：监控面板占满正文高度"
        );
        let full = state.observability.monitor.card_height;
        let footer = state.observability.page_rect.bottom() - 1;
        let glyphs = state.config.border_glyphs;
        let max = state
            .observability
            .page_scroll_limits
            .get(Page::Monitor)
            .expect("系统页滚动度量")
            .max;
        let mut cut = Vec::new();
        for scroll in 0..=max {
            state.observability.scroll = scroll;
            state.compose(width, height).expect("重绘");
            let page = region_text(&state, state.observability.page_rect);
            let buffer = state.compose_buffer.as_ref().expect("帧缓冲");
            for (rect, action) in &state.observability.hits {
                let Action::Card(id) = action else {
                    continue;
                };
                let at = format!("{width}×{height} 第 {scroll} 行：{id} 卡 {rect:?}");
                assert!(rect.bottom() <= footer, "{at} 不压页脚\n{page}");
                if rect.height == full {
                    continue;
                }
                assert_eq!(
                    rect.bottom(),
                    footer,
                    "{at} 只有页面底边的卡片露出一截\n{page}"
                );
                assert_eq!(
                    buffer[(rect.x, rect.y)].symbol(),
                    glyphs.top_left,
                    "{at} 露出的是卡片顶边\n{page}"
                );
                for y in rect.y + 1..rect.bottom() {
                    assert_eq!(
                        buffer[(rect.x, y)].symbol(),
                        glyphs.vertical,
                        "{at} 裁在视口边缘、没有下边框（不是压扁的空卡）\n{page}"
                    );
                    let (row, _) =
                        region_row(buffer, Rect::new(rect.x + 1, y, rect.width - 2, 1), y);
                    assert!(!row.trim().is_empty(), "{at} 露出真实的首行内容\n{page}");
                }
                cut.push((scroll, id.clone(), rect.height));
            }
        }
        // 冒烟尺寸：两行卡片之下剩 3 行（1 行间隔 + 2 行），除最后一屏外都露出
        // 下一张卡的顶边与首行；页顶露出的是内存卡的「已用」gauge。
        assert_eq!(cut.len(), max, "{width}×{height}：{cut:?}");
        assert!(cut.iter().all(|(.., rows)| *rows == 2), "{cut:?}");
        state.observability.scroll = 0;
        state.compose(width, height).expect("回到页顶");
        let memory = page_hit(
            &state,
            |action| matches!(action, Action::Card(id) if id == "memory"),
        )
        .expect("内存卡露出一截");
        let first = region_text(&state, Rect::new(memory.x, memory.y, memory.width, 2));
        let compact = first.replace(' ', "");
        assert!(
            compact.contains("内存") && compact.contains("已用") && compact.contains("GiB"),
            "{width}×{height}：{first}"
        );
    }
}

/// H2：滚轮落在内容不可滚的卡片上（CPU / 内存卡没有可滚内容，逐核卡 4 个核
/// 放得下）时交给页面，不再吃掉整页滚动；内容放不下的卡片（40 个进程）照旧
/// 滚自己的内容，页面不动。
#[test]
fn wheel_over_a_card_with_nothing_to_scroll_scrolls_the_page() {
    use crate::api::schema::{ProcessIdentity, ProcessMetric};
    let _guard = crate::i18n::lang_guard(crate::i18n::Lang::ZhCn);
    let mut state = docked();
    state.open_observation_page(Page::Monitor, &mut ClientShellInput::default());
    let mut sample = smoke_system_sample();
    sample.processes = (1..=40)
        .map(|pid| ProcessMetric {
            identity: ProcessIdentity {
                pid,
                ..Default::default()
            },
            name: format!("proc{pid:02}"),
            ..Default::default()
        })
        .collect();
    state.observability.metrics = Some(sample);
    state.compose(133, 32).expect("系统页");
    let order = state.observability.monitor.visible.clone();
    let card = |state: &ClientShellState, id: &str| {
        page_hit(
            state,
            |action| matches!(action, Action::Card(card) if card == id),
        )
        .unwrap_or_else(|| panic!("{id} 卡已画出"))
    };
    // CPU 卡：不在可滚卡片之列。
    let cpu = card(&state, "cpu");
    sgr_mouse(&mut state, SGR_WHEEL_DOWN, cpu.x + 3, cpu.y + 3);
    state.compose(133, 32).expect("重绘");
    assert_eq!(painted_cards(&state), order[1..3], "CPU 卡上的滚轮滚动页面");
    // 逐核卡：4 个核放得下，上界 0。
    assert_eq!(
        state.observability.card_scroll_limits.get("cores"),
        Some(0),
        "用例前提：逐核卡内容放得下"
    );
    let cores = card(&state, "cores");
    sgr_mouse(&mut state, SGR_WHEEL_DOWN, cores.x + 3, cores.y + 2);
    state.compose(133, 32).expect("重绘");
    assert_eq!(painted_cards(&state), order[2..4], "逐核卡上的滚轮滚动页面");
    sgr_mouse(&mut state, SGR_WHEEL_UP, cores.x + 3, cores.y + 2);
    state.compose(133, 32).expect("重绘");
    assert_eq!(painted_cards(&state), order[1..3], "反向同样交给页面");
    // 进程卡：40 个进程放不下，滚轮滚卡片内容，页面不动。
    for _ in 0..4 {
        press_key(&mut state, crossterm::event::KeyCode::PageDown);
    }
    state.compose(133, 32).expect("重绘");
    assert_eq!(
        painted_cards(&state),
        order[6..],
        "用例前提：进程卡在画面里"
    );
    let before = painted_cards(&state);
    let processes = card(&state, "processes");
    sgr_mouse(&mut state, SGR_WHEEL_DOWN, processes.x + 3, processes.y + 3);
    state.compose(133, 32).expect("重绘");
    assert_eq!(painted_cards(&state), before, "页面不动");
    assert_eq!(
        state.observability.card_scroll.get("processes").copied(),
        Some(1),
        "进程卡内容滚一格"
    );
}

/// 冒烟 N4（133×32，H2 的键盘版）：选中内容不可滚的卡片（CPU、内存卡不在可滚卡片
/// 之列，逐核卡 4 个核放得下、上界 0）后 ↑/↓ 曾被卡片吃掉、页面不动；现在与滚轮
/// 同口径交给页面。选中内容放不下的进程卡时 ↑/↓ 照旧滚卡片内容，页面不动。
#[test]
fn arrow_keys_on_a_selected_card_with_nothing_to_scroll_scroll_the_page() {
    use crate::api::schema::{ProcessIdentity, ProcessMetric};
    use crossterm::event::KeyCode;
    let _guard = crate::i18n::lang_guard(crate::i18n::Lang::ZhCn);
    let mut state = docked();
    state.open_observation_page(Page::Monitor, &mut ClientShellInput::default());
    let mut sample = smoke_system_sample();
    sample.processes = (1..=40)
        .map(|pid| ProcessMetric {
            identity: ProcessIdentity {
                pid,
                ..Default::default()
            },
            name: format!("proc{pid:02}"),
            ..Default::default()
        })
        .collect();
    state.observability.metrics = Some(sample);
    state.compose(133, 32).expect("系统页");
    let order = state.observability.monitor.visible.clone();
    let card = |state: &ClientShellState, id: &str| {
        page_hit(
            state,
            |action| matches!(action, Action::Card(card) if card == id),
        )
        .unwrap_or_else(|| panic!("{id} 卡已画出"))
    };
    let select = |state: &mut ClientShellState, id: &str| {
        let rect = card(state, id);
        click(state, rect.x + 3, rect.y);
        assert_eq!(
            state.observability.selected_card.as_deref(),
            Some(id),
            "用例前提：{id} 卡已选中"
        );
        state.compose(133, 32).expect("重绘");
    };

    select(&mut state, "cpu");
    assert_eq!(painted_cards(&state), order[..2]);
    press_key(&mut state, KeyCode::Down);
    state.compose(133, 32).expect("重绘");
    assert_eq!(
        painted_cards(&state),
        order[1..3],
        "选中 CPU 卡时 ↓ 滚动页面"
    );
    press_key(&mut state, KeyCode::Up);
    state.compose(133, 32).expect("重绘");
    assert_eq!(painted_cards(&state), order[..2], "↑ 同样交给页面");

    // 逐核卡放得下（上界 0）：同样交给页面。
    assert_eq!(
        state.observability.card_scroll_limits.get("cores"),
        Some(0),
        "用例前提：逐核卡内容放得下"
    );
    select(&mut state, "cores");
    press_key(&mut state, KeyCode::Down);
    state.compose(133, 32).expect("重绘");
    assert_eq!(
        painted_cards(&state),
        order[1..3],
        "选中逐核卡时 ↓ 滚动页面"
    );

    // 内存卡：不在可滚卡片之列（上界 None）。
    select(&mut state, "memory");
    press_key(&mut state, KeyCode::Down);
    state.compose(133, 32).expect("重绘");
    assert_eq!(
        painted_cards(&state),
        order[2..4],
        "选中内存卡时 ↓ 滚动页面"
    );

    // 进程卡：40 个进程放不下，↓ 滚卡片内容，页面不动。
    for _ in 0..4 {
        press_key(&mut state, KeyCode::PageDown);
    }
    state.compose(133, 32).expect("重绘");
    assert_eq!(
        painted_cards(&state),
        order[6..],
        "用例前提：进程卡在画面里"
    );
    select(&mut state, "processes");
    let before = painted_cards(&state);
    press_key(&mut state, KeyCode::Down);
    state.compose(133, 32).expect("重绘");
    assert_eq!(painted_cards(&state), before, "页面不动");
    assert_eq!(
        state.observability.card_scroll.get("processes").copied(),
        Some(1),
        "进程卡内容滚一格"
    );
}

/// 冒烟 M1（偏好页）：向下滚过底后多出的格数不留存，反向第一格画面立刻变化。
#[test]
fn preferences_page_scroll_is_clamped_on_write_so_reversing_moves_at_once() {
    let _guard = crate::i18n::lang_guard(crate::i18n::Lang::ZhCn);
    let mut state = docked();
    state.open_observation_page(Page::Settings, &mut ClientShellInput::default());
    state.compose(133, 32).expect("监控偏好页");
    let page = state.observability.page_rect;
    let (column, row) = (page.x + page.width / 2, page.y + page.height / 2);
    for _ in 0..25 {
        sgr_mouse(&mut state, SGR_WHEEL_DOWN, column, row);
    }
    state.compose(133, 32).expect("重绘");
    let bottom = region_text(&state, page);
    let limit = state.observability.settings_scroll;
    assert!(limit > 0, "用例前提：偏好页比面板高\n{bottom}");
    sgr_mouse(&mut state, SGR_WHEEL_DOWN, column, row);
    assert_eq!(
        state.observability.settings_scroll, limit,
        "到底后多滚不再累加\n{bottom}"
    );
    sgr_mouse(&mut state, SGR_WHEEL_UP, column, row);
    state.compose(133, 32).expect("重绘");
    assert!(state.observability.settings_scroll < limit);
    assert_ne!(region_text(&state, page), bottom, "反向第一格画面就动");
}

/// `rect` 这一行里 gauge 格（`━` 已用 / `░` 空槽）的 (相对 `rect.x` 的起点, 格数)。
fn gauge_span(state: &ClientShellState, rect: Rect) -> (u16, u16) {
    let buffer = state.compose_buffer.as_ref().expect("帧缓冲");
    let cells = (rect.x..rect.right())
        .filter(|x| matches!(buffer[(*x, rect.y)].symbol(), "━" | "░"))
        .collect::<Vec<_>>();
    let (Some(first), Some(last)) = (cells.first(), cells.last()) else {
        panic!(
            "{rect:?} 这一行没有 gauge：{:?}",
            region_row(buffer, rect, rect.y).0
        );
    };
    assert_eq!(
        usize::from(last - first) + 1,
        cells.len(),
        "gauge 连续：{:?}",
        region_row(buffer, rect, rect.y).0
    );
    (first - rect.x, last - first + 1)
}

/// 冒烟 M6（133×32 与 93×32）：同一张卡里多行 meter 的标签列 / 数字列按全卡最宽
/// 的一项统一预算，条形起点与长度一致——逐核卡「0」与「10」、温度卡
/// 「acpitz」与「coretemp」两行不再因为标签或数字宽度不同而长短不一。
#[test]
fn meter_rows_in_one_card_share_the_gauge_column() {
    use crate::api::schema::{CpuCoreMetric, SensorMetric};
    let _guard = crate::i18n::lang_guard(crate::i18n::Lang::ZhCn);
    for (cols, rows) in [(133, 32), (93, 32)] {
        let mut state = docked();
        state.open_observation_page(Page::Monitor, &mut ClientShellInput::default());
        state.observability.monitor.visible = vec!["cores".into(), "sensors".into()];
        let mut sample = smoke_system_sample();
        sample.cores = (0..16)
            .map(|id| CpuCoreMetric {
                id,
                name: format!("cpu{id}"),
                usage_percent: Some(if id % 4 == 2 { 100.0 } else { 40.0 + id as f32 }),
                frequency_mhz: None,
            })
            .collect();
        sample.sensors = vec![
            SensorMetric {
                name: "acpitz temp1".into(),
                temperature_celsius: Some(28.0),
                critical_celsius: None,
            },
            SensorMetric {
                name: "coretemp Core 0".into(),
                temperature_celsius: Some(95.0),
                critical_celsius: None,
            },
            SensorMetric {
                name: "coretemp Core 1".into(),
                temperature_celsius: Some(79.0),
                critical_celsius: None,
            },
        ];
        state.observability.metrics = Some(sample);
        // 窄面板单列一行一个核：先把卡片内容滚到 4 号核，让一位数与两位数的核
        // 同屏（宽面板一行两个核，16 个核本来就都在画面里）。
        state.observability.card_scroll.insert("cores".into(), 4);
        state.compose(cols, rows).expect("系统页");
        let cores = state
            .observability
            .hits
            .iter()
            .filter_map(|(rect, action)| match action {
                Action::Core(id) => Some((*id, *rect)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(
            cores.iter().any(|(id, _)| *id < 10) && cores.iter().any(|(id, _)| *id >= 10),
            "{cols}x{rows}: 用例前提：一位数与两位数的核同屏 {cores:?}"
        );
        let (first, reference) = (cores[0].0, gauge_span(&state, cores[0].1));
        for (id, rect) in &cores {
            assert_eq!(
                gauge_span(&state, *rect),
                reference,
                "{cols}x{rows}: 核 {id} 与核 {first} 的条形起点、长度一致"
            );
        }
        let sensors = page_hit(
            &state,
            |action| matches!(action, Action::Card(card) if card == "sensors"),
        )
        .expect("温度卡");
        let row = |needle: &str| {
            let (_, y) = find_in(&state, sensors, needle)
                .unwrap_or_else(|| panic!("{cols}x{rows}: 温度卡有 {needle}"));
            Rect::new(sensors.x + 1, y, sensors.width - 2, 1)
        };
        assert_eq!(
            gauge_span(&state, row("acpitz")),
            gauge_span(&state, row("coretemp")),
            "{cols}x{rows}: 温度卡两行条形对齐\n{}",
            region_text(&state, sensors)
        );
    }
}

/// 冒烟 M7（133×32 与 93×32）：进程表 PID 列按快照里最长的 PID 定宽，7 位
/// PID 完整显示，不再截成「40905…」让几行 rustc 无法区分。
#[test]
fn process_table_shows_seven_digit_pids_in_full() {
    use crate::api::schema::{ProcessIdentity, ProcessMetric};
    let _guard = crate::i18n::lang_guard(crate::i18n::Lang::ZhCn);
    let pids = [4_090_512_u32, 4_086_001, 4_086_002, 3_969_912, 2_559];
    for (cols, rows) in [(133, 32), (93, 32)] {
        let mut state = docked();
        state.open_observation_page(Page::Monitor, &mut ClientShellInput::default());
        state.observability.monitor.visible = vec!["processes".into()];
        let mut sample = smoke_system_sample();
        sample.processes = pids
            .iter()
            .zip(["python3", "rustc", "rustc", "ToDesk_Session", "Xorg"])
            .map(|(pid, name)| ProcessMetric {
                identity: ProcessIdentity {
                    pid: *pid,
                    ..Default::default()
                },
                name: name.into(),
                cpu_percent: Some(5.0),
                memory_bytes: 2 * 1024 * 1024 * 1024,
                ..Default::default()
            })
            .collect();
        state.observability.metrics = Some(sample);
        state.compose(cols, rows).expect("系统页");
        let card = page_hit(
            &state,
            |action| matches!(action, Action::Card(card) if card == "processes"),
        )
        .expect("进程卡");
        let text = region_text(&state, card);
        for pid in pids {
            assert!(
                row_with(&state, card, &pid.to_string()).is_some(),
                "{cols}x{rows}: PID {pid} 完整显示，没有被截成省略号\n{text}"
            );
        }
    }
}

/// 冒烟 L12（133×32，默认主题）：仪表空槽「░」与卡片底色的对比度不低于
/// 1.5:1——原先用 `surface_dim`，与面板底色只有 1.07:1，条形总长看不出来。
#[test]
fn gauge_empty_track_is_visible_on_the_card_background() {
    use crate::ui::color::contrast_ratio;
    let _guard = crate::i18n::lang_guard(crate::i18n::Lang::ZhCn);
    let mut state = docked();
    state.open_observation_page(Page::Monitor, &mut ClientShellInput::default());
    state.observability.metrics = Some(smoke_system_sample());
    state.compose(133, 32).expect("系统页");
    let cpu = page_hit(
        &state,
        |action| matches!(action, Action::Card(card) if card == "cpu"),
    )
    .expect("CPU 卡");
    let buffer = state.compose_buffer.as_ref().expect("帧缓冲");
    let track = (cpu.x..cpu.right())
        .map(|x| &buffer[(x, cpu.y + 1)])
        .filter(|cell| cell.symbol() == "░")
        .collect::<Vec<_>>();
    assert!(!track.is_empty(), "CPU 42%：条形后半段是空槽");
    for cell in track {
        let style = cell.style();
        let (fg, bg) = (style.fg.expect("空槽前景色"), style.bg.expect("空槽底色"));
        let ratio = contrast_ratio(fg, bg).expect("内置主题的颜色都可换算");
        assert!(ratio >= 1.5, "空槽对比度 {ratio:.2}:1（{fg:?} on {bg:?}）");
    }
}

/// 冒烟 L13（133×32）：停靠面板里页面不再自带一圈外框——面板左侧分隔线、
/// 页面框、卡片框原先挤成「│││」；现在分隔线与卡片框之间隔一列空白，页面
/// 顶边那一行让给页签。
#[test]
fn docked_monitor_page_draws_no_frame_of_its_own() {
    let _guard = crate::i18n::lang_guard(crate::i18n::Lang::ZhCn);
    let mut state = docked();
    state.open_observation_page(Page::Monitor, &mut ClientShellInput::default());
    state.observability.metrics = Some(smoke_system_sample());
    state.compose(133, 32).expect("系统页");
    let page = state.observability.page_rect;
    let cpu = page_hit(
        &state,
        |action| matches!(action, Action::Card(card) if card == "cpu"),
    )
    .expect("CPU 卡");
    let buffer = state.compose_buffer.as_ref().expect("帧缓冲");
    let border = |x: u16, y: u16| {
        let symbol = buffer[(x, y)].symbol();
        !symbol.trim().is_empty()
            && symbol
                .chars()
                .all(|ch| ('\u{2500}'..='\u{257F}').contains(&ch))
    };
    let row = cpu.y + 2;
    assert!(border(cpu.x, row), "卡片左边框");
    assert!(
        cpu.x > page.x && !border(cpu.x - 1, row),
        "卡片框与面板边缘之间留一列空白：{:?}",
        region_row(buffer, page, row).0
    );
    assert!(
        !border(page.x, page.y) && !border(page.right() - 1, page.bottom() - 1),
        "页面四角不再画框：{:?}",
        region_row(buffer, page, page.y).0
    );
    assert!(
        find_in(&state, Rect::new(page.x, page.y, page.width, 1), "系统").is_some(),
        "页签画在面板正文第一行\n{}",
        region_text(&state, page)
    );
}

/// 冒烟 L13（133×32）：逐核卡多列时条形铺满卡片宽度，不再只占左半。
#[test]
fn core_card_slots_fill_the_card_width() {
    use crate::api::schema::CpuCoreMetric;
    let _guard = crate::i18n::lang_guard(crate::i18n::Lang::ZhCn);
    let mut state = docked();
    state.open_observation_page(Page::Monitor, &mut ClientShellInput::default());
    let mut sample = smoke_system_sample();
    sample.cores = (0..16)
        .map(|id| CpuCoreMetric {
            id,
            name: format!("cpu{id}"),
            usage_percent: Some(50.0),
            frequency_mhz: None,
        })
        .collect();
    state.observability.metrics = Some(sample);
    state.compose(133, 32).expect("系统页");
    let card = page_hit(
        &state,
        |action| matches!(action, Action::Card(card) if card == "cores"),
    )
    .expect("逐核卡");
    let slots = state
        .observability
        .hits
        .iter()
        .filter_map(|(rect, action)| matches!(action, Action::Core(_)).then_some(*rect))
        .filter(|rect| rect.y == card.y + 1)
        .collect::<Vec<_>>();
    assert!(slots.len() > 1, "用例前提：一行多个核 {slots:?}");
    let right = slots.iter().map(|rect| rect.right()).max().expect("核");
    assert_eq!(
        right,
        card.right() - 1,
        "最右一格贴到卡片内缘\n{}",
        region_text(&state, card)
    );
}

/// 冒烟 L13（133×32）：温度卡芯片不多时，空出的行画最高温度的历史迷你图，
/// 不再留一大片空白。
#[test]
fn sparse_temperature_card_plots_the_hottest_sensor_history() {
    use crate::api::schema::SensorMetric;
    use crate::client::shell::observability::HistoryPoint;
    let _guard = crate::i18n::lang_guard(crate::i18n::Lang::ZhCn);
    let mut state = docked();
    state.open_observation_page(Page::Monitor, &mut ClientShellInput::default());
    state.observability.monitor.visible = vec!["sensors".into()];
    let mut sample = smoke_system_sample();
    sample.sensors = vec![
        SensorMetric {
            name: "acpitz temp1".into(),
            temperature_celsius: Some(28.0),
            critical_celsius: None,
        },
        SensorMetric {
            name: "coretemp Core 0".into(),
            temperature_celsius: Some(95.0),
            critical_celsius: None,
        },
    ];
    state.observability.metrics = Some(sample);
    // 15 分钟窗口里均匀分布的 30 个点。
    state.observability.now_ms = 900_000;
    state.observability.history = (0..30)
        .map(|index| HistoryPoint {
            at: index * 30_000,
            cpu: None,
            memory: None,
            cores: Vec::new(),
            temperature: Some(60.0 + index as f32),
        })
        .collect();
    state.compose(133, 32).expect("系统页");
    let card = page_hit(
        &state,
        |action| matches!(action, Action::Card(card) if card == "sensors"),
    )
    .expect("温度卡");
    let text = region_text(&state, card);
    assert!(text.contains("最高温度"), "迷你图说明\n{text}");
    assert!(
        text.chars()
            .any(|ch| ('\u{2801}'..='\u{28FF}').contains(&ch)),
        "空出的行画出盲文迷你图\n{text}"
    );
}

/// 监控偏好页的控件：点分段 / 步进器 / 开关只回写各自的偏好键（`PreferenceKey`），
/// 落盘后重启可恢复；图表字形是独立的客户端偏好键 `monitor_chart_glyphs`。
#[test]
fn preferences_page_controls_write_back_only_their_preference_key() {
    use crate::client::shell::observability::ChartGlyphsPreference;
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
    // 分段控件：图表字形 → 方块，只写 monitor_chart_glyphs。
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
    // 开关：显示项整行可点，同样写进 monitor 键。
    state.compose(120, 60).expect("重绘");
    let processes = hit(
        &state,
        |action| matches!(action, Action::Metric(id) if id == "processes"),
    );
    click(&mut state, processes.x + 1, processes.y);
    assert!(!state
        .observability
        .monitor
        .visible
        .iter()
        .any(|id| id == "processes"));
    // 落盘并可恢复。
    let saved = preferences::load(&path).expect("偏好已写入");
    assert_eq!(
        saved.monitor_chart_glyphs,
        Some(ChartGlyphsPreference::Blocks)
    );
    let restored = ClientShellState::new(
        ClientShellConfig::from_config(&Config::default()).with_preferences_path(path.clone()),
    );
    assert_eq!(restored.observability.monitor.interval_ms, 2000);
    assert!(!restored
        .observability
        .monitor
        .visible
        .iter()
        .any(|id| id == "processes"));
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

/// Esc 先关进程对话框，页面还开着；再按一次 Esc 才关页面（经典布局）。
#[test]
fn escape_closes_the_process_dialog_before_the_page() {
    use crate::client::shell::observability::ProcessDialog;
    for size in [(80u16, 24u16), (133, 32), (200, 50)] {
        let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
        state.set_snapshot(Box::new(snapshot()));
        state.set_pane_surface(surface());
        tick(&mut state, Instant::now());
        state.open_observation_page(Page::Monitor, &mut ClientShellInput::default());
        state.observability.process_dialog = Some(ProcessDialog {
            process: Default::default(),
            force: false,
            confirm: false,
            pending: false,
        });
        state.compose(size.0, size.1).expect("经典布局帧");

        state.handle_input_bytes(b"\x1b");
        assert!(
            state.observability.process_dialog.is_none(),
            "{size:?}：第一次 Esc 关对话框"
        );
        assert_eq!(
            state.observability.page,
            Some(Page::Monitor),
            "{size:?}：页面还开着"
        );

        state.compose(size.0, size.1).expect("经典布局帧");
        state.handle_input_bytes(b"\x1b");
        assert_eq!(state.observability.page, None, "{size:?}：再按 Esc 关页面");
    }
}

/// 文档终审 D4：告警阈值步进器以前固定 ±5、不走档位表，配置里的 30 会被加到
/// 35（超出文档写的 50–100），52 这类档间值也跳不回档位。改为与其它步进器一样
/// 走档位表（50–100，步长 5）：档间值先走到相邻档，越过两端回绕，结果永远在
/// 档位表内；偏好页上画出的就是新值。
#[test]
fn alert_threshold_stepper_follows_its_ladder() {
    let mut state = docked();
    assert!(
        !state.observability.monitor.alerts.is_empty(),
        "用例前提：默认配置带告警规则"
    );
    for (start, delta, expected) in [
        (30.0, 1, 50.0),
        (30.0, -1, 100.0),
        (52.0, 1, 55.0),
        (52.0, -1, 50.0),
        (95.0, 1, 100.0),
        (100.0, 1, 50.0),
        (50.0, -1, 100.0),
        (101.0, -1, 100.0),
    ] {
        state.observability.monitor.alerts[0].threshold = start;
        state.observation_action(
            Action::AlertThreshold(0, delta),
            &mut ClientShellInput::default(),
        );
        assert_eq!(
            state.observability.monitor.alerts[0].threshold, expected,
            "{start} 按 {delta:+} 应到 {expected}"
        );
    }

    // 偏好页上的「+」：从配置值 30 起步，点一下落到 50。告警卡在页面靠下，
    // 先把页面滚到能看见第一条规则的步进器。
    state.observability.monitor.alerts[0].threshold = 30.0;
    state.observability.monitor.alerts_enabled = true;
    state.open_observation_page(Page::Settings, &mut ClientShellInput::default());
    let find_plus = |state: &ClientShellState| {
        state
            .observability
            .hits
            .iter()
            .find(|(_, action)| matches!(action, Action::AlertThreshold(0, 1)))
            .map(|(rect, _)| *rect)
    };
    state.compose(133, 32).expect("偏好页");
    let page = state.observability.page_rect;
    for _ in 0..40 {
        if find_plus(&state).is_some() {
            break;
        }
        state.handle_mouse(
            MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: page.x + 2,
                row: page.y + page.height / 2,
                modifiers: KeyModifiers::NONE,
            },
            &mut ClientShellInput::default(),
        );
        state.compose(133, 32).expect("滚动偏好页");
    }
    let plus = find_plus(&state).expect("告警阈值的「+」画在偏好页上");
    click(&mut state, plus.x, plus.y);
    assert_eq!(state.observability.monitor.alerts[0].threshold, 50.0);
}

#[test]
fn docked_fixture_can_focus_its_terminal_through_the_endpoint() {
    let mut state = docked();
    state.open_observation_page(Page::Monitor, &mut ClientShellInput::default());
    state.compose(120, 40).unwrap();
    let area = terminal_body(&state);
    let outcome = click(&mut state, area.x + 1, area.y + 1);
    assert!(outcome.actions.iter().any(|action| matches!(action,
        ClientShellAction::Endpoint { request, .. } if matches!(request.method, Method::PaneFocus(_)))));
}
