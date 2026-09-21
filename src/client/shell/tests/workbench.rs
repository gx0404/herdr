use super::*;
use crate::api::schema::{ClientViewSpec, Method};
use crate::client::shell::dock::{Edge, PanelId};
use crate::protocol::ServerMessage;

fn ready() -> ClientShellState {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_endpoint_methods(Some(vec!["client.views.set".into(), "tab.focus".into()]));
    state.set_pane_surface(surface());
    state.compose(120, 40).expect("初始画面");
    state.tick_workbench(std::time::Instant::now(), &mut ClientShellInput::default());
    state.workbench.pending = false;
    state.workbench.acknowledged = state.workbench.revision;
    state
}

#[test]
fn layout_requires_explicit_server_capability() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.tick_workbench(std::time::Instant::now(), &mut ClientShellInput::default());
    assert!(!state.workbench.enabled);
}

#[test]
fn compact_projection_preserves_dock_tree_and_every_size_is_safe() {
    let mut state = ready();
    assert!(state.workbench.enabled);
    state.workbench_open(PanelId::Monitor);
    state.workbench_open(PanelId::Accounts);
    let layout = state.workbench.dock.clone();
    for (cols, rows) in [(1, 1), (12, 4), (28, 10), (80, 24), (160, 50)] {
        let frame = state.compose(cols, rows).expect("窄屏也可组合");
        assert_eq!(frame.cells.len(), usize::from(cols) * usize::from(rows));
    }
    assert_eq!(state.workbench.dock, layout);
}

#[test]
fn account_hover_remains_visible_over_a_focused_monitor_panel() {
    use crate::client::shell::observability::{Hover, HoverTarget, Page};
    let mut state = ready();
    state.workbench_open(PanelId::Monitor);
    state.observability.hover = Some(Hover {
        target: HoverTarget::Agent {
            endpoint_id: state.active_endpoint_id.clone(),
            pane: "pane_1".into(),
            agent: "claude".into(),
        },
        anchor: Rect::new(0, 20, 24, 2),
        since: std::time::Instant::now(),
        visible: true,
        leave_at: None,
        pinned: false,
    });
    state.compose(120, 40).expect("监控与悬浮层可同时显示");
    // 聚焦的监控面板拥有键盘并画出用户选中的 tab（默认系统页）；渲染期不改写。
    assert_eq!(state.observability.monitor_tab, Page::Monitor);
    assert_eq!(
        state.observability.page,
        Some(state.observability.monitor_tab)
    );
    assert!(!state.observability.page_rect.is_empty());
    assert!(!state.observability.hover_rect.is_empty());
    assert!(state.observability.hits.iter().any(|(rect, action)| {
        matches!(
            action,
            crate::client::shell::observability::Action::Page(Page::Accounts)
        ) && state
            .observability
            .hover_rect
            .contains((rect.x, rect.y).into())
    }));
    let overlap = state
        .observability
        .hover_rect
        .intersection(state.observability.page_rect);
    assert!(!overlap.is_empty());
    state.observability.hover.as_mut().unwrap().leave_at = Some(std::time::Instant::now());
    let mut outcome = ClientShellInput::default();
    state.handle_mouse(
        MouseEvent {
            kind: MouseEventKind::Moved,
            column: overlap.x,
            row: overlap.y,
            modifiers: KeyModifiers::NONE,
        },
        &mut outcome,
    );
    assert!(state
        .observability
        .hover
        .as_ref()
        .unwrap()
        .leave_at
        .is_none());
}

#[test]
fn hover_blank_space_and_wheel_cannot_reach_background_processes_or_cards() {
    use crate::client::shell::observability::{Action, Hover, HoverTarget};
    let mut state = ready();
    state.set_endpoint_methods(Some(vec![
        "client.views.set".into(),
        "system.process.get".into(),
    ]));
    // 悬浮层滚动按悬浮层作用域的账号数夹取，数据注入 hover_scope 而非页面。
    state.observability.hover_scope.accounts =
        vec![crate::api::schema::AccountUsageSnapshot::default()];
    state.workbench_open(PanelId::Monitor);
    state.observability.hover = Some(Hover {
        target: HoverTarget::Agent {
            endpoint_id: state.active_endpoint_id.clone(),
            pane: "pane_1".into(),
            agent: "claude".into(),
        },
        anchor: Rect::new(0, 20, 24, 2),
        since: std::time::Instant::now(),
        visible: true,
        leave_at: None,
        pinned: false,
    });
    state.compose(120, 40).unwrap();
    let hover = state.observability.hover_rect;
    let blank = Rect::new(hover.x + 3, hover.y + 3, 1, 1);
    assert!(!state
        .observability
        .hover_hits
        .iter()
        .any(|(rect, _)| rect.intersects(blank)));
    state
        .observability
        .hits
        .push((blank, Action::Card("processes".into())));
    state
        .observability
        .hits
        .push((blank, Action::Process(Default::default())));
    let mut outcome = ClientShellInput::default();
    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::ScrollDown,
    ] {
        state.handle_mouse(
            MouseEvent {
                kind,
                column: blank.x,
                row: blank.y,
                modifiers: KeyModifiers::NONE,
            },
            &mut outcome,
        );
    }
    assert!(outcome.actions.is_empty(), "浮层空白不能请求底层进程详情");
    assert!(state.observability.card_scroll.is_empty());
    // 悬浮层滚轮写悬浮层自己的滚动位置，页面的 account_scroll 不动。
    assert_eq!(state.observability.hover_scope.scroll, 1);
    assert_eq!(state.observability.account_scroll, 0);
}

#[test]
fn dashboard_can_scroll_to_quota_windows_below_the_viewport() {
    let mut state = ready();
    state.workbench_open(PanelId::Accounts);
    state.workbench.dock.maximized = Some(PanelId::Accounts);
    state.observability.accounts = vec![crate::api::schema::AccountUsageSnapshot {
        account_label: "account".into(),
        // 仪表盘每指标一行（额度条内联），30 个指标才会超出 24 行的视口。
        metrics: (0..30)
            .map(|index| crate::api::schema::UsageMetric {
                label: format!("metric-{index:02}"),
                scope: "account".into(),
                used_percent: Some(50.0),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    }];
    let before = state.compose(120, 24).unwrap();
    assert!(!frame_rows(&before).join("\n").contains("metric-29"));
    for _ in 0..12 {
        state.handle_input_bytes(b"\x1b[6~");
    }
    let after = state.compose(120, 24).unwrap();
    assert!(frame_rows(&after).join("\n").contains("metric-29"));
}

#[test]
fn captured_dock_drag_finishes_even_when_released_inside_hover_bounds() {
    let mut state = ready();
    state.compose(120, 40).unwrap();
    let mut outcome = ClientShellInput::default();
    let mouse = |kind, column, row| MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    };
    state.handle_mouse(
        mouse(MouseEventKind::Down(MouseButton::Left), 2, 1),
        &mut outcome,
    );
    // 模拟拖动期间浮层已占据落点，已有捕获必须仍能收到释放事件。
    state.observability.hover_rect = Rect::new(60, 8, 30, 20);
    state.handle_mouse(
        mouse(MouseEventKind::Drag(MouseButton::Left), 61, 10),
        &mut outcome,
    );
    state.handle_mouse(
        mouse(MouseEventKind::Up(MouseButton::Left), 61, 10),
        &mut outcome,
    );
    state.observability.hover_rect = Rect::default();
    state.compose(120, 40).unwrap();
    state.handle_mouse(
        mouse(MouseEventKind::Down(MouseButton::Left), 2, 0),
        &mut outcome,
    );
    assert!(
        matches!(state.overlay, Some(ClientShellOverlay::CommandPalette(_))),
        "释放后菜单点击不能再被拖动捕获"
    );
}

/// 与 `ready()` 同构，但用指定的 `ui.border_style` 起工作台。
fn ready_with_border_style(style: crate::config::BorderStyleConfig) -> ClientShellState {
    let mut raw = Config::default();
    raw.ui.border_style = style;
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&raw));
    state.set_snapshot(Box::new(snapshot()));
    state.set_endpoint_methods(Some(vec!["client.views.set".into(), "tab.focus".into()]));
    state.set_pane_surface(surface());
    state.compose(120, 40).expect("初始画面");
    state.tick_workbench(std::time::Instant::now(), &mut ClientShellInput::default());
    state.workbench.pending = false;
    state.workbench.acknowledged = state.workbench.revision;
    state
}

/// THEME-01/02：终端组弹窗边框走共享的带标题面板——字形跟随
/// `ui.border_style`、颜色取组件 token，不再硬编码 Rounded + accent。
#[test]
fn workbench_popup_border_follows_the_border_style_and_component_token() {
    use crate::client::shell::workbench::View;
    for style in [
        crate::config::BorderStyleConfig::Single,
        crate::config::BorderStyleConfig::Double,
    ] {
        let mut state = ready_with_border_style(style);
        state.workbench.views.insert(
            "1".into(),
            View {
                tab: "tab_1".into(),
                surface: surface_with_popup(),
                graphics: Default::default(),
            },
        );
        state.compose(120, 40).expect("弹窗帧");
        let popup = state.hits.popup.as_ref().expect("弹窗几何");
        let buffer = state.compose_buffer.as_ref().expect("保留帧缓冲");
        let corner = buffer[(popup.rect.x, popup.rect.y)].clone();
        assert_eq!(
            corner.symbol(),
            state.config.border_glyphs.top_left,
            "弹窗边框字形应跟随 ui.border_style（{style:?}）"
        );
        assert_eq!(
            corner.style().fg,
            Some(state.config.components.pane_border_focused),
            "弹窗边框色应取组件 token（{style:?}）"
        );
        // 标题仍画在顶边上。
        let title_first = "popup title".chars().next().expect("标题首字");
        let top_row: String = (popup.rect.x..popup.rect.right())
            .map(|x| buffer[(x, popup.rect.y)].symbol().to_owned())
            .collect();
        assert!(top_row.contains(title_first), "标题画在边框上：{top_row:?}");
    }
}

/// THEME-01/02：布局拖放的落点预览同样换成组件字形与边框 token，并且仍然
/// 只画轮廓（填底会盖住用来判断落点的终端内容）。
#[test]
fn workbench_drop_preview_border_follows_the_border_style() {
    let mut state = ready_with_border_style(crate::config::BorderStyleConfig::Double);
    state.workbench_open(PanelId::Monitor);
    state.compose(120, 40).expect("两个面板");
    let (panel, area) = state
        .workbench
        .geometry
        .panels
        .iter()
        .find(|(panel, _)| *panel == PanelId::Monitor)
        .map(|(panel, area)| (panel.clone(), *area))
        .expect("监控面板在布局里");
    let terminal = state
        .workbench
        .geometry
        .panels
        .iter()
        .find(|(panel, _)| matches!(panel, PanelId::Terminal(_)))
        .map(|(_, area)| *area)
        .expect("终端面板在布局里");
    // 真鼠标路径：按住终端面板头部拖动到监控面板中心即产生落点预览。
    let mut outcome = ClientShellInput::default();
    let mouse = |kind, column, row| MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    };
    state.handle_mouse(
        mouse(
            MouseEventKind::Down(MouseButton::Left),
            terminal.x + 2,
            terminal.y,
        ),
        &mut outcome,
    );
    state.handle_mouse(
        mouse(
            MouseEventKind::Drag(MouseButton::Left),
            area.x + area.width / 2,
            area.y + area.height / 2,
        ),
        &mut outcome,
    );
    let frame = state.compose(120, 40).expect("拖放预览帧");
    let buffer = state.compose_buffer.as_ref().expect("保留帧缓冲");
    // 落点在面板中心 → 无边缘取向 → 预览就是整块面板。
    let corner = buffer[(area.x, area.y)].clone();
    assert_eq!(
        corner.symbol(),
        state.config.border_glyphs.top_left,
        "拖放预览边框字形应跟随 ui.border_style"
    );
    assert_eq!(
        corner.style().fg,
        Some(state.config.components.pane_border_focused),
        "拖放预览边框色应取组件 token"
    );
    // 预览只画轮廓：落点面板的正文（页签行）仍在，没有被整块填底盖掉。
    let page = state.observability.page_rect;
    assert_eq!(
        page,
        super::super::workbench::body(area, &panel),
        "监控页铺满落点面板正文"
    );
    let inner_row = page.y + 1;
    let text: String = frame_rows(&frame)
        .get(inner_row as usize)
        .cloned()
        .unwrap_or_default();
    let tab_label = crate::client::shell::observability::tr("System", "系统");
    let first = tab_label.chars().next().expect("页签首字");
    assert!(
        text.contains(first),
        "落点面板的正文应保留（第 {inner_row} 行）：{text:?}"
    );
    assert!(matches!(panel, PanelId::Monitor));
}

#[test]
fn process_dialog_blocks_background_panels_and_page_shortcuts() {
    use crate::client::shell::observability::{Action, Page, ProcessDialog};
    let mut state = ready();
    state.workbench_open(PanelId::Accounts);
    state.workbench_open(PanelId::Monitor);
    state.observability.process_dialog = Some(ProcessDialog {
        process: Default::default(),
        force: false,
        confirm: false,
        pending: false,
    });
    state.compose(140, 40).unwrap();
    assert!(state.observability.hits.iter().all(|(_, action)| matches!(
        action,
        Action::CancelProcess | Action::Terminate(_) | Action::ConfirmProcess
    )));
    state.handle_input_bytes(b"s");
    assert_eq!(state.observability.page, Some(Page::Monitor));
    let mut outcome = ClientShellInput::default();
    state.handle_mouse(
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 2,
            row: 0,
            modifiers: KeyModifiers::NONE,
        },
        &mut outcome,
    );
    assert!(state.overlay.is_none());
    assert!(state.observability.process_dialog.is_some());
    state.handle_input_bytes(b"\x1b");
    assert!(state.observability.process_dialog.is_none());
}

#[test]
fn background_tab_creation_repaints_chrome_without_resubmitting_geometry() {
    let mut state = ready();
    let mut updated = snapshot();
    let mut tab = updated.tabs[0].clone();
    tab.tab_id = "tab_2".into();
    tab.label = "BACKGROUND".into();
    tab.focused = false;
    updated.tabs.push(tab);
    state.set_snapshot(Box::new(updated));
    let mut outcome = ClientShellInput::default();
    state.tick_workbench(
        std::time::Instant::now() + std::time::Duration::from_secs(1),
        &mut outcome,
    );
    assert!(outcome.repaint, "未聚焦新标签也必须刷新标签栏");
    assert!(!outcome.actions.iter().any(|action| matches!(action, ClientShellAction::Endpoint { request, .. } if matches!(request.method, Method::ClientViewsSet(_)))));
    let frame = state.compose(120, 40).unwrap();
    assert!(frame_rows(&frame).join("\n").contains("BACKGROUND"));
}

#[test]
fn clicking_a_docked_tab_uses_explicit_navigation_before_geometry_refresh() {
    let mut state = ready();
    let mut updated = snapshot();
    let mut tab = updated.tabs[0].clone();
    tab.tab_id = "tab_2".into();
    tab.label = "second".into();
    tab.focused = false;
    updated.tabs.push(tab);
    state.set_snapshot(Box::new(updated));
    let now = std::time::Instant::now() + std::time::Duration::from_secs(1);
    state.tick_workbench(now, &mut ClientShellInput::default());
    state.workbench.pending = false;
    state.compose(120, 40).unwrap();
    let rect = state
        .hits
        .tabs
        .iter()
        .find(|(_, id)| id == "tab_2")
        .unwrap()
        .0;
    let mut navigation = ClientShellInput::default();
    state.handle_mouse(
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: rect.x,
            row: rect.y,
            modifiers: KeyModifiers::NONE,
        },
        &mut navigation,
    );
    assert!(navigation.actions.iter().any(|action| matches!(action, ClientShellAction::Endpoint { request, .. } if matches!(&request.method, Method::TabFocus(params) if params.tab_id == "tab_2"))));
    let mut geometry = ClientShellInput::default();
    state.tick_workbench(now + std::time::Duration::from_secs(1), &mut geometry);
    assert!(geometry.actions.iter().any(|action| matches!(action, ClientShellAction::Endpoint { request, .. } if matches!(&request.method, Method::ClientViewsSet(params) if params.views.iter().any(|view| view.tab_id == "tab_2")))));
    assert!(!geometry.actions.iter().any(|action| matches!(action, ClientShellAction::Endpoint { request, .. } if matches!(request.method, Method::TabFocus(_)))));
}

#[test]
fn releases_remain_routable_while_layout_waits_for_new_frames() {
    let mut state = ready();
    state.workbench.views.clear();
    state.workbench.requested = vec![ClientViewSpec {
        view_id: "1".into(),
        tab_id: "tab_1".into(),
        cols: 80,
        rows: 20,
        focused: true,
    }];
    let request = state
        .view_request(ClientMessage::ClientShellPaneInput {
            pane_id: "pane_1".into(),
            events: vec![ClientPaneInputEvent::TextCommit("x".into())],
        })
        .expect("根据拓扑定位，无需帧缓存");
    let ClientMessage::EndpointControl { kind, data } = request else {
        panic!("视图输入");
    };
    assert_eq!(kind, crate::protocol::views::INPUT_KIND);
    let input: crate::protocol::views::ViewInput = serde_json::from_str(&data).unwrap();
    assert_eq!(input.tab_id, "tab_1");
    assert_eq!(input.pane_id, "pane_1");
}

#[test]
fn detached_tabs_and_host_layouts_survive_preference_roundtrip() {
    let mut state = ready();
    state.workbench.dock.reconcile_workspace_tabs(
        &["tab_1".into(), "tab_2".into()],
        &["tab_1".into(), "tab_2".into()],
        None,
    );
    assert!(state
        .workbench
        .dock
        .move_tab("tab_2", 1, 0, Some(Edge::Right)));
    let saved = state.workbench.saved_layouts();
    let json = serde_json::to_string(&saved).unwrap();
    let restored: HashMap<String, crate::client::shell::dock::DockLayout> =
        serde_json::from_str(&json).unwrap();
    assert_eq!(restored, saved);
    assert!(restored.values().all(|layout| layout.valid()));
    let mut outcome = ClientShellInput::default();
    state.last_composed_size = Some((180, 60));
    state.workbench.pending = false;
    state.tick_workbench(
        std::time::Instant::now() + std::time::Duration::from_secs(1),
        &mut outcome,
    );
    assert!(outcome.actions.iter().any(|action| matches!(action, ClientShellAction::Endpoint { request, .. } if matches!(&request.method, Method::ClientViewsSet(_)))));
}

#[test]
fn restored_surface_renews_views_even_with_the_same_boot_and_geometry() {
    let mut state = ready();
    state.workbench.revision = 2;
    state.workbench.acknowledged = 2;
    let message = |revision| {
        let frame = surface();
        let ServerMessage::EndpointControl { data, .. } = crate::protocol::views::message(
            &frame.boot_id.clone(),
            revision,
            "1",
            "tab_1",
            &ServerMessage::PaneSurface(frame),
        )
        .unwrap() else {
            panic!("视图帧缺少扩展包");
        };
        data
    };
    let mut decoder = crate::protocol::views::Decoder::default();
    assert!(decoder.decode(&message(2)).unwrap().is_some());
    state.workbench_open(PanelId::Monitor);
    let layout = state.workbench.dock.clone();
    state.renew_workbench_surface();
    assert!(!state.workbench.enabled);
    let mut outcome = ClientShellInput::default();
    state.tick_workbench(std::time::Instant::now(), &mut outcome);
    assert_eq!(state.workbench.dock, layout);
    assert!(outcome.actions.iter().any(|action| matches!(action, ClientShellAction::Endpoint { request, .. } if matches!(request.method, Method::ClientViewsSet(_)))));
    assert!(state.workbench.revision > 2);
    assert!(decoder
        .decode(&message(state.workbench.revision))
        .unwrap()
        .is_some());
    assert!(decoder.decode(&message(2)).unwrap().is_none());
}

#[test]
fn dragging_an_inactive_groups_inner_split_targets_its_own_tab() {
    use crate::client::shell::workbench::View;
    let mut state = ready();
    let mut snapshot = snapshot();
    let mut second = snapshot.tabs[0].clone();
    second.tab_id = "tab_2".into();
    second.focused = false;
    snapshot.tabs.push(second);
    let mut pane = snapshot.panes[0].clone();
    pane.pane_id = "pane_2".into();
    pane.tab_id = "tab_2".into();
    pane.focused = false;
    snapshot.panes.push(pane);
    state.set_snapshot(Box::new(snapshot));
    state.set_endpoint_methods(Some(vec![
        "client.views.set".into(),
        "layout.set_split_ratio".into(),
    ]));
    state.workbench.dock.reconcile_workspace_tabs(
        &["tab_1".into(), "tab_2".into()],
        &["tab_1".into(), "tab_2".into()],
        None,
    );
    state
        .workbench
        .dock
        .move_tab("tab_2", 1, 0, Some(Edge::Right));
    state.workbench.dock.focused = PanelId::Terminal(1);
    state.workbench.views.insert(
        "1".into(),
        View {
            tab: "tab_1".into(),
            surface: surface(),
            graphics: Default::default(),
        },
    );
    let mut second = surface();
    second.panes[0].pane_id = "pane_2".into();
    second.splits.push(PaneSurfaceSplit {
        direction: PaneSurfaceSplitDirection::Horizontal,
        pos: 2,
        area: SurfaceRect {
            x: 0,
            y: 0,
            width: 4,
            height: 2,
        },
        hit_rect: SurfaceRect {
            x: 2,
            y: 1,
            width: 1,
            height: 1,
        },
        path: vec![true],
    });
    state.workbench.views.insert(
        "2".into(),
        View {
            tab: "tab_2".into(),
            surface: second,
            graphics: Default::default(),
        },
    );
    state.compose(160, 40).unwrap();
    let hit = state
        .hits
        .pane_splits
        .iter()
        .find(|hit| hit.tab_id.as_deref() == Some("tab_2"))
        .unwrap()
        .clone();
    let mut outcome = ClientShellInput::default();
    for (kind, column) in [
        (MouseEventKind::Down(MouseButton::Left), hit.hit_rect.x),
        (MouseEventKind::Drag(MouseButton::Left), hit.hit_rect.x + 1),
    ] {
        state.handle_mouse(
            MouseEvent {
                kind,
                column,
                row: hit.hit_rect.y,
                modifiers: KeyModifiers::NONE,
            },
            &mut outcome,
        );
    }
    assert_eq!(
        state.snapshot.as_ref().unwrap().focused_tab_id.as_deref(),
        Some("tab_1")
    );
    assert!(outcome.actions.iter().any(|action| matches!(action, ClientShellAction::Endpoint { request, .. } if matches!(&request.method, Method::LayoutSetSplitRatio(params) if params.tab_id.as_deref() == Some("tab_2")))));
}

/// HERDR-BUG-006 复现面：两个终端分组、各自 view 的画面内容不同；
/// group 1（pane_1）聚焦，group 2（pane_2）非聚焦。
fn two_group_state() -> ClientShellState {
    use crate::client::shell::workbench::View;
    let mut state = ready();
    let mut snapshot = snapshot();
    let mut second = snapshot.tabs[0].clone();
    second.tab_id = "tab_2".into();
    second.focused = false;
    snapshot.tabs.push(second);
    let mut pane = snapshot.panes[0].clone();
    pane.pane_id = "pane_2".into();
    pane.tab_id = "tab_2".into();
    pane.focused = false;
    snapshot.panes.push(pane);
    state.set_snapshot(Box::new(snapshot));
    state.set_endpoint_methods(Some(vec![
        "client.views.set".into(),
        "tab.focus".into(),
        "pane.selection.read".into(),
    ]));
    state.workbench.dock.reconcile_workspace_tabs(
        &["tab_1".into(), "tab_2".into()],
        &["tab_1".into(), "tab_2".into()],
        None,
    );
    state
        .workbench
        .dock
        .move_tab("tab_2", 1, 0, Some(Edge::Right));
    state.workbench.dock.focused = PanelId::Terminal(1);

    let mut first = surface();
    first.frame = FrameData::from_ratatui_buffer_with_hyperlinks(
        &Buffer::with_lines(["a         "]),
        None,
        &[],
    );
    first.panes[0].rect.width = 10;
    first.panes[0].inner_rect.width = 10;
    state.workbench.views.insert(
        "1".into(),
        View {
            tab: "tab_1".into(),
            surface: first,
            graphics: Default::default(),
        },
    );
    let mut second_surface = surface();
    second_surface.panes[0].pane_id = "pane_2".into();
    second_surface.panes[0].focused = false;
    second_surface.panes[0].content_revision = 42;
    second_surface.frame = FrameData::from_ratatui_buffer_with_hyperlinks(
        &Buffer::with_lines(["xy z      "]),
        None,
        &[],
    );
    second_surface.panes[0].rect.width = 10;
    second_surface.panes[0].inner_rect.width = 10;
    state.workbench.views.insert(
        "2".into(),
        View {
            tab: "tab_2".into(),
            surface: second_surface,
            graphics: Default::default(),
        },
    );
    state
}

/// HERDR-BUG-006：选区/悬停/复制读取 pane 画面时必须落到该 pane 所在 view 的
/// surface；非聚焦分组不得读到聚焦分组的画面。
#[test]
fn inactive_group_reads_its_own_view_surface() {
    let mut state = two_group_state();
    assert_eq!(
        state
            .visible_surface_for_pane("pane_2")
            .and_then(|surface| surface
                .panes
                .iter()
                .find(|pane| pane.pane_id == "pane_2")
                .map(|pane| pane.content_revision)),
        Some(42),
        "非聚焦分组的 pane 必须解析到自己 view 的 surface"
    );
    state.compose(160, 40).expect("compose workbench");
    let hit = state
        .hits
        .panes
        .iter()
        .find(|hit| hit.pane_id == "pane_2")
        .expect("pane_2 hit")
        .clone();

    // 三击选行：行尾必须是非聚焦组画面里 "xy z" 的最后一个非空格列（3），
    // 而不是镜像缺失时的兜底宽度。
    let mut outcome = ClientShellInput::default();
    state.select_line_at(&hit, 0, &mut outcome);
    let selection = state.selection.as_ref().expect("line selection");
    assert_eq!(selection.ordered_cells(), ((0, 0), (0, 3)));

    // 双击选词手势的内容版本也必须来自 pane_2 自己的 view。
    let mut outcome = ClientShellInput::default();
    state.request_word_selection(&hit, 0, 1, &mut outcome);
    let read = outcome.actions.iter().find_map(|action| match action {
        ClientShellAction::EndpointRequest { request, .. }
        | ClientShellAction::Endpoint { request, .. } => match &request.method {
            Method::PaneSelectionRead(params) => Some(params),
            _ => None,
        },
        _ => None,
    });
    let params = read.expect("word selection row read request");
    assert_eq!(params.pane_id, "pane_2");
    assert_eq!(params.content_revision, Some(42));
}

#[test]
fn borderless_terminal_text_is_not_overwritten_by_pane_drag_handles() {
    let mut state = ready();
    let mut view = surface();
    let mut buffer = ratatui::buffer::Buffer::empty(Rect::new(0, 0, 12, 2));
    buffer.set_string(0, 0, "FIRST TEXT", ratatui::style::Style::default());
    view.frame = FrameData::from_ratatui_buffer_with_hyperlinks(&buffer, None, &[]);
    view.panes[0].rect = SurfaceRect {
        x: 0,
        y: 0,
        width: 12,
        height: 2,
    };
    view.panes[0].inner_rect = view.panes[0].rect;
    state.workbench.views.insert(
        "1".into(),
        crate::client::shell::workbench::View {
            tab: "tab_1".into(),
            surface: view,
            graphics: Default::default(),
        },
    );
    let frame = state.compose(120, 40).unwrap();
    assert!(frame_rows(&frame).join("\n").contains("FIRST TEXT"));
}

#[test]
fn copy_search_prompt_and_cursor_are_visible_in_docked_terminal() {
    let mut state = ready();
    let mut view = surface();
    view.panes[0].scroll = Some(crate::protocol::PaneSurfaceScrollMetrics {
        offset_from_bottom: 0,
        max_offset_from_bottom: 0,
        viewport_rows: 2,
    });
    state.workbench.views.insert(
        "1".into(),
        crate::client::shell::workbench::View {
            tab: "tab_1".into(),
            surface: view,
            graphics: Default::default(),
        },
    );
    state.compose(120, 40).unwrap();
    assert!(state.enter_copy_mode(&mut ClientShellInput::default()));
    state.handle_input_bytes(b"/needle");
    let frame = state.compose(120, 40).unwrap();
    assert!(frame_rows(&frame).join("\n").contains("needle"));
    assert!(frame.cursor.is_none());
}

#[test]
fn focusing_another_workspace_swaps_the_primary_terminal_strip() {
    let mut state = ready();
    assert_eq!(
        state.workbench.dock.groups[0].tabs,
        vec!["tab_1".to_owned()],
        "初始只显示聚焦工作区的标签"
    );

    let mut switched = snapshot();
    switched.workspaces[0].focused = false;
    let mut workspace = switched.workspaces[0].clone();
    workspace.workspace_id = "ws_2".into();
    workspace.active_tab_id = "tab_2".into();
    workspace.number = 2;
    workspace.label = "other-shell".into();
    workspace.focused = true;
    switched.workspaces.push(workspace);
    let mut tab = switched.tabs[0].clone();
    tab.tab_id = "tab_2".into();
    tab.workspace_id = "ws_2".into();
    switched.tabs.push(tab);
    switched.focused_workspace_id = Some("ws_2".into());
    switched.focused_tab_id = Some("tab_2".into());
    state.set_snapshot(Box::new(switched));
    state.workbench.pending = false;

    let mut outcome = ClientShellInput::default();
    state.tick_workbench(
        std::time::Instant::now() + std::time::Duration::from_secs(1),
        &mut outcome,
    );
    assert!(outcome.repaint, "切换工作区必须触发重绘");
    let group = &state.workbench.dock.groups[0];
    assert_eq!(
        group.tabs,
        vec!["tab_2".to_owned()],
        "主终端组整体换成新工作区的标签"
    );
    assert_eq!(group.active.as_deref(), Some("tab_2"));
    assert!(outcome.actions.iter().any(|action| matches!(action,
        ClientShellAction::Endpoint { request, .. } if matches!(&request.method,
            Method::ClientViewsSet(params) if params.views.iter().any(|view| view.tab_id == "tab_2")))),
        "服务端视图必须投影新工作区的活动标签");
    state.compose(120, 40).expect("切换后可组合画面");
    assert!(state.hits.tabs.iter().any(|(_, id)| id == "tab_2"));
    assert!(
        !state.hits.tabs.iter().any(|(_, id)| id == "tab_1"),
        "另一工作区的标签不再出现在终端条"
    );
}

fn frame_text(frame: &crate::protocol::FrameData) -> String {
    frame
        .cells
        .chunks(frame.width as usize)
        .map(|row| {
            row.iter()
                .map(|cell| cell.symbol.as_str())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn usage_provider(
    agent: &str,
    label: &str,
    installed: Option<bool>,
    configured_accounts: Vec<String>,
) -> crate::api::schema::UsageProviderInfo {
    crate::api::schema::UsageProviderInfo {
        agent: agent.into(),
        label: label.into(),
        source_url: "https://example.com".into(),
        method: "usage".into(),
        account_scope: "account".into(),
        minimum_interval_seconds: 300,
        configured_accounts,
        installed,
        supports_callback: false,
    }
}

#[test]
fn accounts_page_lists_only_installed_or_configured_providers() {
    use crate::client::shell::observability::Page;
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    // 经典布局下账号页铺满整个 pane 区：工具栏 chip 行（≥60 列）列出每个厂商。
    state.observability.page = Some(Page::Accounts);
    state.observability.providers = vec![
        usage_provider("codex", "Codex", Some(true), Vec::new()),
        usage_provider("kimi", "Kimi Code", Some(false), Vec::new()),
        usage_provider("grok", "Grok", Some(false), vec!["grok-main".into()]),
        usage_provider("letta", "Letta", None, Vec::new()),
    ];
    let frame = state.compose(120, 40).expect("账号页");
    let text = frame_text(&frame);
    let compact: String = text.chars().filter(|ch| !ch.is_whitespace()).collect();
    assert!(
        compact.contains("Codex"),
        "installed provider listed: {text}"
    );
    assert!(
        compact.contains("Grok"),
        "explicitly configured account keeps the provider listed: {text}"
    );
    assert!(
        compact.contains("Letta"),
        "unknown availability (older server) stays listed: {text}"
    );
    assert!(
        !compact.contains("Kimi"),
        "missing CLI without configured accounts is hidden: {text}"
    );
    let chips = state
        .observability
        .hits
        .iter()
        .filter_map(|(_, action)| match action {
            crate::client::shell::observability::Action::Provider(agent) => Some(agent.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        chips,
        vec!["codex", "grok", "letta"],
        "chip 行按厂商顺序给出选中动作，隐藏的厂商没有 chip"
    );
    assert!(
        state.observability.hits.iter().any(|(_, action)| matches!(
            action,
            crate::client::shell::observability::Action::Overview
        )),
        "首个 chip「全部厂商」回到总览"
    );
}

/// Layout 模式下最大化后仍要能用键盘轮转面板：几何取自去最大化投影，
/// 最大化目标跟随新焦点。
#[test]
fn maximized_layout_keyboard_rotates_panels_and_moves_the_maximized_target() {
    let mut state = ready();
    state.workbench_open(PanelId::Monitor);
    state.workbench.arranging = true;
    let focused = state.workbench.dock.focused.clone();
    state.workbench.dock.maximized = Some(focused.clone());

    let mut outcome = ClientShellInput::default();
    assert!(state.workbench_key(
        &crate::input::TerminalKey::new(crossterm::event::KeyCode::Tab, KeyModifiers::empty()),
        &mut outcome,
    ));
    let next = state.workbench.dock.focused.clone();
    assert_ne!(next, focused, "最大化后 Tab 仍要轮转面板");
    assert_eq!(
        state.workbench.dock.maximized.as_ref(),
        Some(&next),
        "最大化跟随新焦点"
    );

    // Shift+方向键在最大化状态下仍能停靠到另一个面板。
    let before = state.workbench.dock.revision;
    let mut outcome = ClientShellInput::default();
    assert!(state.workbench_key(
        &crate::input::TerminalKey::new(crossterm::event::KeyCode::Right, KeyModifiers::SHIFT),
        &mut outcome,
    ));
    assert!(
        state.workbench.dock.revision > before,
        "最大化后 Shift+→ 仍要重排布局"
    );
    assert!(
        state.workbench.dock.maximized.is_none(),
        "Shift+方向键走 `DockLayout::dock`，按既有语义顺带退出最大化"
    );
}

fn preferences_path(name: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "herdr-workbench-preferences-{name}-{}.json",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    path
}

/// Layout 模式按键落盘去抖：未处理按键不标脏，处理过的按键 500 ms 静默后
/// 合并落盘，退出路径必须 flush。
#[test]
fn layout_keys_debounce_preference_writes_and_flush_before_exit() {
    let path = preferences_path("debounce");
    let mut state = ready();
    state.config.preferences_path = Some(path.clone());
    state.workbench.arranging = true;

    // 未处理按键：不写盘、不标脏。
    let mut outcome = ClientShellInput::default();
    assert!(state.workbench_key(
        &crate::input::TerminalKey::new(
            crossterm::event::KeyCode::Char('q'),
            KeyModifiers::empty(),
        ),
        &mut outcome,
    ));
    assert!(
        state.preferences_dirty_since.is_none(),
        "未处理按键不触发偏好落盘"
    );
    assert_eq!(state.preferences_writes, 0, "未处理按键不写偏好文件");

    // 处理过的按键：只标脏，去抖窗口内不写盘。
    let start = std::time::Instant::now();
    let mut outcome = ClientShellInput::default();
    assert!(state.workbench_key(
        &crate::input::TerminalKey::new(crossterm::event::KeyCode::Right, KeyModifiers::empty()),
        &mut outcome,
    ));
    assert!(state.preferences_dirty_since.is_some(), "布局变更标脏");
    assert_eq!(state.preferences_writes, 0, "去抖窗口内不落盘");
    assert!(!state.tick_chrome_preferences(start + std::time::Duration::from_millis(100)));
    assert_eq!(state.preferences_writes, 0, "100 ms 时仍在去抖窗口内");

    // 静默 500 ms 后合并写一次。
    state.tick_chrome_preferences(start + std::time::Duration::from_millis(600));
    assert_eq!(state.preferences_writes, 1, "静默 500 ms 后只落盘一次");
    assert!(path.exists(), "静默 500 ms 后落盘");
    assert!(state.preferences_dirty_since.is_none());
    assert!(state.preferences_dirty_first.is_none());

    // 退出前 flush：未到期的脏状态也必须落盘。
    let mut outcome = ClientShellInput::default();
    assert!(state.workbench_key(
        &crate::input::TerminalKey::new(crossterm::event::KeyCode::Left, KeyModifiers::empty()),
        &mut outcome,
    ));
    assert!(state.preferences_dirty_since.is_some());
    state.flush_chrome_preferences(&mut outcome);
    assert_eq!(
        state.preferences_writes, 2,
        "退出路径 flush 掉最后一次布局变更"
    );
    assert!(state.preferences_dirty_since.is_none());
    let _ = std::fs::remove_file(&path);
}

/// 去抖是 trailing edge：窗口内再次标脏必须把计时整个推后，否则「按住方向键」
/// 会退化成每 500 ms 落一次盘的节流（C-13）。
#[test]
fn preference_debounce_restarts_on_every_dirty_event() {
    let path = preferences_path("restart");
    let mut state = ready();
    state.config.preferences_path = Some(path.clone());
    let start = std::time::Instant::now();
    let at = |ms: u64| start + std::time::Duration::from_millis(ms);

    state.schedule_chrome_preferences(at(0));
    assert!(!state.tick_chrome_preferences(at(400)));
    assert_eq!(state.preferences_writes, 0, "400 ms 时仍在去抖窗口内");

    // 窗口内的第二次输入把到期时刻推到 900 ms。
    state.schedule_chrome_preferences(at(400));
    assert!(!state.tick_chrome_preferences(at(600)));
    assert_eq!(
        state.preferences_writes, 0,
        "去抖必须重置计时：600 ms 还不该落盘"
    );
    state.tick_chrome_preferences(at(900));
    assert_eq!(state.preferences_writes, 1, "静默满 500 ms 才合并写一次");

    state.tick_chrome_preferences(at(2_000));
    assert_eq!(state.preferences_writes, 1, "没有新脏事件就不再写");
    let _ = std::fs::remove_file(&path);
}

/// C-13 的验收指标：按住方向键 5 秒（≈30 键/s）落盘次数 ≤2；防饿死上限保证
/// 长按也不会永远不写。
#[test]
fn held_layout_keys_collapse_into_at_most_two_preference_writes() {
    let path = preferences_path("held");
    let mut state = ready();
    state.config.preferences_path = Some(path.clone());
    let start = std::time::Instant::now();
    let at = |ms: u64| start + std::time::Duration::from_millis(ms);

    // 5 秒按住：每 33 ms 一次脏事件（≈30 键/s），主循环 100 ms 节拍照常 tick。
    let mut next_press = 0u64;
    let mut next_tick = 0u64;
    for millis in 0..5_000u64 {
        if millis >= next_press {
            state.schedule_chrome_preferences(at(millis));
            next_press += 33;
        }
        if millis >= next_tick {
            state.tick_chrome_preferences(at(millis));
            next_tick += 100;
        }
    }
    assert!(
        state.preferences_writes <= 1,
        "按住期间最多只有防饿死上限那一次写，实际 {}",
        state.preferences_writes
    );

    // 松手后静默一个去抖窗口，合并写收尾。
    state.tick_chrome_preferences(at(5_600));
    assert!(
        state.preferences_writes <= 2,
        "5 秒窗口内落盘次数必须 ≤2，实际 {}",
        state.preferences_writes
    );
    assert!(state.preferences_dirty_since.is_none(), "收尾后脏标记清空");
    assert!(path.exists(), "最后一次布局变更必须落盘");
    let _ = std::fs::remove_file(&path);
}

/// 只有一个面板时 Tab 轮转是彻底的空操作：不得标脏，更不得落盘（C-13）。
#[test]
fn single_panel_layout_tab_never_marks_preferences_dirty() {
    let path = preferences_path("single-tab");
    let mut state = ready();
    state.config.preferences_path = Some(path.clone());
    state.workbench.arranging = true;
    // 收成单面板布局：Tab 轮转在这里必然回到自己。
    let focused = state.workbench.dock.focused.clone();
    state.workbench.dock.root = crate::client::shell::dock::DockNode::Panel {
        panel: focused.clone(),
    };
    let panels = state
        .workbench
        .dock
        .layout_geometry(Rect::new(0, 0, 4096, 4096))
        .panels
        .len();
    assert_eq!(panels, 1, "布局已收成一个面板");

    let mut outcome = ClientShellInput::default();
    assert!(state.workbench_key(
        &crate::input::TerminalKey::new(crossterm::event::KeyCode::Tab, KeyModifiers::empty()),
        &mut outcome,
    ));
    assert_eq!(state.workbench.dock.focused, focused, "单面板 Tab 不换焦点");
    assert!(
        state.preferences_dirty_since.is_none(),
        "无效果的 Tab 不得标脏"
    );
    assert_eq!(state.preferences_writes, 0);
    let _ = std::fs::remove_file(&path);
}

/// 最大化状态下的方向键：先退出最大化再 resize（与 `DockLayout::dock` 一致），
/// 不留「无声不可见改动」；Shift+方向键同样以 `maximized = None` 收尾。
#[test]
fn maximized_layout_arrow_keys_leave_maximize_before_resizing() {
    let path = preferences_path("maximized-arrow");
    let mut state = ready();
    state.config.preferences_path = Some(path.clone());
    state.workbench_open(PanelId::Monitor);
    state.workbench.arranging = true;
    let focused = state.workbench.dock.focused.clone();
    state.workbench.dock.maximized = Some(focused);

    let before = state.workbench.dock.revision;
    let mut outcome = ClientShellInput::default();
    assert!(state.workbench_key(
        &crate::input::TerminalKey::new(crossterm::event::KeyCode::Right, KeyModifiers::empty()),
        &mut outcome,
    ));
    assert!(
        state.workbench.dock.maximized.is_none(),
        "最大化下的方向键先退出最大化，改动才看得见"
    );
    assert!(
        state.workbench.dock.revision > before,
        "退出最大化后的 resize 必须真的改写布局"
    );
    assert!(state.preferences_dirty_since.is_some(), "布局改动要标脏");
    let _ = std::fs::remove_file(&path);
}

/// 分隔线已经顶到 clamp 边界后继续按方向键不是改动：不得标脏、不得落盘。
#[test]
fn layout_arrow_keys_stop_marking_dirty_at_the_resize_clamp() {
    let path = preferences_path("clamp");
    let mut state = ready();
    state.workbench_open(PanelId::Monitor);
    state.config.preferences_path = Some(path.clone());
    state.workbench.arranging = true;

    // 一直推到 clamp 边界。
    for _ in 0..80 {
        let mut outcome = ClientShellInput::default();
        state.workbench_key(
            &crate::input::TerminalKey::new(crossterm::event::KeyCode::Left, KeyModifiers::empty()),
            &mut outcome,
        );
    }
    state.preferences_dirty_since = None;
    state.preferences_dirty_first = None;

    let mut outcome = ClientShellInput::default();
    assert!(state.workbench_key(
        &crate::input::TerminalKey::new(crossterm::event::KeyCode::Left, KeyModifiers::empty()),
        &mut outcome,
    ));
    assert!(
        state.preferences_dirty_since.is_none(),
        "贴边后的方向键没有改动，不得标脏"
    );
    assert_eq!(state.preferences_writes, 0);
    let _ = std::fs::remove_file(&path);
}

/// `workbench_mouse` 的非脏集合：标签滚动、全局菜单与 `arranging` 开关都不写
/// `ClientChromePreferences`，命中后不得标脏、不得落盘（C-13）。
#[test]
fn presentation_only_workbench_clicks_never_mark_preferences_dirty() {
    use crate::client::shell::workbench::interaction::Action;
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    // 每个动作都用全新状态：全局菜单会打开 overlay，复用状态会让后续点击被
    // `workbench_mouse` 的 overlay 守卫直接吞掉。
    let click = |name: &str, wanted: fn(&Action) -> bool| {
        let path = preferences_path(name);
        let mut state = ready();
        state.config.mouse_capture = true;
        state.workbench_open(PanelId::Monitor);
        state.compose(120, 40).expect("布局命中图");
        state.config.preferences_path = Some(path.clone());
        let rect = state
            .workbench
            .hits
            .iter()
            .find(|(_, action)| wanted(action))
            .map(|(rect, _)| *rect)
            .unwrap_or_else(|| panic!("工作台应当有 {name} 命中区"));
        let mut outcome = ClientShellInput::default();
        assert!(
            state.workbench_mouse(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: rect.x + rect.width / 2,
                    row: rect.y,
                    modifiers: KeyModifiers::empty(),
                },
                &mut outcome,
            ),
            "{name} 命中区应当被工作台消费"
        );
        assert!(
            state.preferences_dirty_since.is_none(),
            "{name} 只是呈现状态，不得标脏"
        );
        assert_eq!(state.preferences_writes, 0, "{name} 不得落盘");
        let _ = std::fs::remove_file(&path);
    };

    click("menu", |action| matches!(action, Action::Menu));
    click("arrange", |action| matches!(action, Action::Arrange));
}

/// 延迟落盘新增的数据丢失面：`run_client_loop` 还有 `ServerShutdown` /
/// `ConnectionLost` / `?` 传播等不经过显式 flush 的返回。`ClientState` 的 Drop
/// 是统一收口，删掉它这条测试必须变红。
#[test]
fn dropping_the_client_state_flushes_deferred_chrome_preferences() {
    let path = preferences_path("client-drop");
    let mut client = crate::client::state::test_client_state();
    let shell = client.shell.as_mut().expect("shell 模式");
    shell.config.preferences_path = Some(path.clone());
    shell.schedule_chrome_preferences(std::time::Instant::now());
    assert!(!path.exists(), "去抖窗口内不落盘");

    drop(client);
    assert!(path.exists(), "异常退出路径也必须落盘最后一次布局变更");
    let _ = std::fs::remove_file(&path);
}

/// 写失败不得吞掉整个合并批次：脏标记必须保留，下一个去抖周期或退出前的
/// flush 自动重试（否则最多 500 ms 的布局变更会永久丢失）。
#[test]
fn failed_preference_write_keeps_the_batch_dirty_for_the_next_retry() {
    // 让父路径是一个普通文件，`create_dir_all` 必然失败。
    let blocker = preferences_path("write-failure");
    std::fs::write(&blocker, b"not a directory").expect("准备不可写父路径");
    let path = blocker.join("preferences.json");

    let mut state = ready();
    state.config.preferences_path = Some(path);
    let start = std::time::Instant::now();
    state.schedule_chrome_preferences(start);

    let mut outcome = ClientShellInput::default();
    state.flush_chrome_preferences(&mut outcome);
    assert_eq!(state.preferences_writes, 1, "尝试写过一次");
    assert!(
        state.preferences_dirty_since.is_some(),
        "写失败必须保留脏标记以便重试"
    );
    assert!(outcome.repaint, "写失败要提示用户");

    // 修好路径后下一次 flush 真的补上。
    std::fs::remove_file(&blocker).expect("清理阻塞文件");
    let mut outcome = ClientShellInput::default();
    state.flush_chrome_preferences(&mut outcome);
    assert_eq!(state.preferences_writes, 2);
    assert!(state.preferences_dirty_since.is_none(), "重试成功后清脏");
    let _ = std::fs::remove_dir_all(&blocker);
}

/// LEAK-01：分组销毁后回收它的标签页视口状态（`tab_scroll` / `tab_focus`），
/// 否则按分组 id 索引的两张表会随工作区开关无限增长。
#[test]
fn tab_view_state_is_pruned_to_live_groups() {
    let mut state = ready();
    state.compose(160, 40).expect("初始画面");
    let live = state
        .workbench
        .dock
        .groups
        .first()
        .map(|group| group.id)
        .expect("至少一个终端组");
    let stale = state
        .workbench
        .dock
        .groups
        .iter()
        .map(|group| group.id)
        .max()
        .unwrap_or(live)
        + 7;

    // 现存分组保留，已销毁分组的残留条目要在 tick 里被回收。
    state.workbench.tab_scroll.insert(live, 2);
    state.workbench.tab_scroll.insert(stale, 3);
    state
        .workbench
        .tab_focus
        .insert(stale, (Some("tab_x".into()), 40));

    state.tick_workbench(
        std::time::Instant::now() + std::time::Duration::from_millis(10),
        &mut ClientShellInput::default(),
    );
    assert!(
        !state.workbench.tab_scroll.contains_key(&stale),
        "销毁的分组不再保留 tab_scroll（LEAK-01）"
    );
    assert!(
        !state.workbench.tab_focus.contains_key(&stale),
        "销毁的分组不再保留 tab_focus（LEAK-01）"
    );
    assert_eq!(
        state.workbench.tab_scroll.get(&live).copied(),
        Some(2),
        "现存分组的视口状态保留"
    );
}
