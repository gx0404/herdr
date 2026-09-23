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
    state.compose(120, 40).expect("监控面板");
    let page = state.observability.page_rect;
    assert!(!page.is_empty(), "用例前提：监控面板已画出");
    // 悬浮层按 kit 定位从锚点下方左对齐展开：锚点放在监控面板左上角，卡片
    // 必然与面板重叠。
    state.observability.hover = Some(Hover {
        target: HoverTarget::Agent {
            endpoint_id: state.active_endpoint_id.clone(),
            pane: "pane_1".into(),
            agent: "claude".into(),
        },
        anchor: Rect::new(page.x, page.y, 24, 1),
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
    // 悬浮层滚动按悬浮层作用域上一帧画出的上界夹取（冒烟 N5）：数据注入
    // hover_scope 而非页面，且三张账号卡（各 6 个指标）合起来高过卡片上限，滚轮
    // 才滚得动（单张卡在悬浮层里按视口封顶，一张卡滚不动）。
    state.observability.hover_scope.accounts = (0..3)
        .map(|account| crate::api::schema::AccountUsageSnapshot {
            account_id: format!("claude:{account}"),
            metrics: (0..6)
                .map(|index| crate::api::schema::UsageMetric {
                    label: format!("metric-{index:02}"),
                    scope: "account".into(),
                    used_percent: Some(50.0),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        })
        .collect();
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
    // 停靠面板里页面不自带外框（冒烟 L13），页签就在正文第一行。
    let inner_row = page.y;
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

/// 冒烟 M12：紧凑视图（62 列、焦点在 Agents 面板，没有任何终端 view）拉回 134
/// 列后，服务端这次 resize 没有东西可推，tick 又按上一次组合的尺寸算 views，
/// 画面停在旧尺寸直到下一次输入。宿主 resize 之后工作台必须立即按新尺寸出帧，
/// 下一次 tick 再按新几何重发 views。
#[test]
fn widening_out_of_the_compact_agents_view_repaints_at_the_new_size() {
    let mut shell = ready();
    // 监控面板让布局最小宽度超过 62 列：62 列时只投影聚焦面板。
    shell.workbench_open(PanelId::Monitor);
    shell.workbench.dock.focused = PanelId::Agents;
    shell.compose(62, 32).expect("紧凑视图");
    assert!(
        shell.workbench.geometry.compact,
        "用例前提：62 列是紧凑视图"
    );
    // 越过请求节流（75 ms）再 tick，确保 views 真按紧凑几何重发。
    let now = std::time::Instant::now() + std::time::Duration::from_secs(1);
    shell.tick_workbench(now, &mut ClientShellInput::default());
    shell.workbench.pending = false;
    shell.workbench.acknowledged = shell.workbench.revision;
    assert!(
        shell.workbench.requested.is_empty(),
        "用例前提：只剩 Agents 面板，没有终端 view"
    );

    let mut client = crate::client::state::test_client_state();
    client.shell = Some(shell);
    client.reported_size = (62, 32);
    client.apply_terminal_resize(134, 32, 0, 0, false);
    client.present_after_resize();
    assert!(!client.repaint_pending, "resize 后的整帧重绘已经呈现");

    let shell = client.shell.as_mut().expect("shell 模式");
    assert_eq!(
        shell.last_composed_size,
        Some((134, 32)),
        "resize 后立即按新尺寸出帧"
    );
    assert!(!shell.workbench.geometry.compact, "134 列退出紧凑视图");
    let mut outcome = ClientShellInput::default();
    shell.tick_workbench(now + std::time::Duration::from_secs(1), &mut outcome);
    assert!(outcome.repaint, "新几何的 views 请求要求重绘");
    let terminal = shell
        .workbench
        .geometry
        .panels
        .iter()
        .find(|(panel, _)| matches!(panel, PanelId::Terminal(_)))
        .map(|(panel, area)| crate::client::shell::workbench::body(*area, panel))
        .expect("134 列时终端面板可见");
    assert_eq!(
        shell
            .workbench
            .requested
            .iter()
            .map(|view| (view.cols, view.rows))
            .collect::<Vec<_>>(),
        vec![(terminal.width, terminal.height)],
        "按 134 列的几何重发终端 view"
    );
}

/// 经典布局的 resize 仍等服务端按新尺寸推来配对的 surface：本地不抢先出帧
/// （此时组合只会画「不可用」占位）。
#[test]
fn classic_resize_waits_for_the_server_surface_before_presenting() {
    let mut client = crate::client::state::test_client_state();
    let shell = client.shell.as_mut().expect("shell 模式");
    shell.set_snapshot(Box::new(snapshot()));
    shell.set_pane_surface(surface());
    shell.compose(100, 30).expect("经典画面");
    client.apply_terminal_resize(120, 40, 0, 0, false);
    client.present_after_resize();
    assert!(client.repaint_pending, "等服务端的 surface，重绘挂起");
    assert_eq!(
        client
            .shell
            .as_ref()
            .and_then(|shell| shell.last_composed_size),
        Some((100, 30)),
        "经典布局不在 resize 事件里组合"
    );
}

/// 冒烟 M3：焦点停在 26 列宽的 Agents 面板时按前缀键，which-key 以前被夹在
/// 聚焦面板的 body 里，只放得下「全局」一组；它应与其它浮层同口径占用整帧
/// 内容区，三组快捷键完整列出。
#[test]
fn which_key_spans_the_content_area_when_the_agents_panel_is_focused() {
    let mut state = ready();
    state.workbench.dock.focused = PanelId::Agents;
    state.compose(133, 32).expect("Agents 面板聚焦");
    state.handle_input_bytes(b"\x02");
    assert_eq!(state.mode, ClientShellMode::Prefix, "ctrl+b 进入前缀模式");
    let frame = state.compose(133, 32).expect("前缀帧");
    // 宽字符的续格是空格：去掉空白再比较 CJK 组名。
    let compact = |text: &str| text.split_whitespace().collect::<String>();
    let rows = frame_rows(&frame);
    let texts = crate::i18n::texts();
    let title_row = rows
        .iter()
        .position(|row| compact(row).contains(&compact(texts.keybinds.group_global)))
        .unwrap_or_else(|| panic!("which-key 未画出：{rows:#?}"));
    for group in [
        texts.keybinds.group_workspaces_tabs,
        texts.keybinds.group_panes,
    ] {
        assert!(
            compact(&rows[title_row]).contains(&compact(group)),
            "which-key 标题行缺少「{group}」组：{:?}",
            rows[title_row]
        );
    }
    // 浮层在顶栏与页脚（模式条）之间，二者都不被压住。
    assert!(title_row > 1, "上边框不压顶栏");
    let bottom = rows
        .iter()
        .skip(title_row)
        .position(|row| row.contains('┘'))
        .map(|offset| offset + title_row)
        .expect("which-key 下边框");
    assert!(bottom < 31, "下边框不压页脚：{bottom}");
}

/// 页脚行里某个字符所在的单元格（找不到就失败），按字符定位样式断言。
fn footer_cell(state: &ClientShellState, needle: &str) -> ratatui::buffer::Cell {
    let buffer = state.compose_buffer.as_ref().expect("保留帧缓冲");
    let y = buffer.area.bottom() - 1;
    (0..buffer.area.width)
        .map(|x| buffer[(x, y)].clone())
        .find(|cell| cell.symbol() == needle)
        .unwrap_or_else(|| panic!("页脚没有 {needle:?}"))
}

/// 冒烟 L1：锁定布局后拖动已被拒绝，状态栏不再提示拖动、面板标题也不再显示
/// `⠿` 把手；调整布局模式里「调尺寸 / 移动」两条提示随之置灰，切换面板等
/// 锁定下仍可用的提示照常。
#[test]
fn locked_layout_stops_advertising_drag_affordances() {
    let compact = |text: &str| text.split_whitespace().collect::<String>();
    let mut state = ready();
    let rows = frame_rows(&state.compose(133, 32).expect("未锁定"));
    assert!(rows[1].contains('⠿'), "未锁定：面板标题带拖动把手");
    assert!(compact(&rows[31]).contains("拖动"), "未锁定：页脚提示拖动");

    state.workbench.dock.locked = true;
    let rows = frame_rows(&state.compose(133, 32).expect("已锁定"));
    assert!(
        rows.iter().all(|row| !row.contains('⠿')),
        "锁定后不再显示把手：{rows:#?}"
    );
    assert!(
        compact(&rows[1]).contains("工作区"),
        "标题文字仍在：{:?}",
        rows[1]
    );
    let footer = compact(&rows[31]);
    assert!(!footer.contains("拖动"), "锁定后页脚不再提示拖动：{footer}");
    assert!(footer.contains("已锁定"), "页脚说明布局已锁定：{footer}");

    state.workbench.arranging = true;
    state.compose(133, 32).expect("锁定下的调整布局");
    let palette = state.config.palette.clone();
    for disabled in ["尺", "移"] {
        assert_eq!(
            footer_cell(&state, disabled).fg,
            palette.overlay0,
            "锁定时「{disabled}」所在提示置灰"
        );
    }
    assert_eq!(
        footer_cell(&state, "切").fg,
        palette.subtext0,
        "切换面板在锁定时照常可用"
    );
}

/// 调整布局模式下，把一组无边框窗格（相对面板 body 的矩形）与若干行文字铺进
/// 133×32 的工作台：终端面板 body 是 (30, 3) 起 103×28。
fn arranged_borderless_panes(
    panes: &[(SurfaceRect, &str)],
    texts: &[(u16, &str)],
) -> ClientShellState {
    let mut state = ready();
    let mut view = surface();
    let mut buffer = ratatui::buffer::Buffer::empty(Rect::new(0, 0, 103, 28));
    for (y, text) in texts {
        buffer.set_string(0, *y, *text, ratatui::style::Style::default());
    }
    view.frame = FrameData::from_ratatui_buffer_with_hyperlinks(&buffer, None, &[]);
    view.panes = panes
        .iter()
        .map(|(rect, id)| PaneSurfacePane {
            pane_id: (*id).into(),
            rect: *rect,
            inner_rect: *rect,
            focused: false,
            ..view.panes[0].clone()
        })
        .collect();
    state.workbench.views.insert(
        "1".into(),
        crate::client::shell::workbench::View {
            tab: "tab_1".into(),
            surface: view,
            graphics: Default::default(),
        },
    );
    state.workbench.arranging = true;
    state
}

fn pane_handles(state: &ClientShellState) -> Vec<(String, Rect)> {
    use crate::client::shell::workbench::interaction::Action;
    state
        .workbench
        .hits
        .iter()
        .filter_map(|(rect, action)| match action {
            Action::Pane(pane) => Some((pane.clone(), *rect)),
            _ => None,
        })
        .collect()
}

/// 终端面板 body（x = 30 起）里第 `y` 行的前 `len` 个字符。
fn body_text(rows: &[String], y: u16, len: usize) -> String {
    rows[usize::from(y)].chars().skip(30).take(len).collect()
}

/// 冒烟 L2：调整布局模式下无边框窗格的 `⠿` 把手不再压住终端首行（截屏 40 里
/// 是「x⠿z%」），改放到正上方标签栏行里不压标签与按钮的空位；把手照样可拖。
#[test]
fn arrange_mode_pane_handle_leaves_the_first_terminal_row_intact() {
    use crate::client::shell::workbench::interaction::Action;
    let full = SurfaceRect {
        x: 0,
        y: 0,
        width: 103,
        height: 28,
    };
    let mut state = arranged_borderless_panes(&[(full, "pane_1")], &[(0, "xyz% FIRST TEXT")]);
    let rows = frame_rows(&state.compose(133, 32).expect("调整布局模式"));
    let pane = state.hits.panes[0].rect;
    assert_eq!(pane, Rect::new(30, 3, 103, 28), "终端面板 body 起点");
    assert_eq!(
        body_text(&rows, pane.y, 15),
        "xyz% FIRST TEXT",
        "把手不压终端首行"
    );
    let handles = pane_handles(&state);
    let [(id, handle)] = handles.as_slice() else {
        panic!("调整布局模式为无边框窗格画一个把手：{handles:?}");
    };
    assert_eq!(id, "pane_1");
    assert_eq!(handle.y + 1, pane.y, "把手在正上方的标签栏行");
    assert!(
        handle.x > pane.x && handle.right() <= pane.right(),
        "把手在窗格列宽内：{handle:?}"
    );
    assert!(
        state
            .workbench
            .hits
            .iter()
            .filter(|(_, action)| matches!(
                action,
                Action::Tab { .. } | Action::NewTab(_) | Action::ScrollTabs(..)
            ))
            .all(|(rect, _)| !rect.intersects(*handle)),
        "把手不压标签与按钮：{handle:?}"
    );
    let buffer = state.compose_buffer.as_ref().expect("保留帧缓冲");
    let cell = &buffer[(handle.x, handle.y)];
    assert_eq!(cell.symbol(), "⠿");
    assert_eq!(cell.fg, state.config.palette.accent);
}

/// 冒烟 L2（续）：上下堆叠的无边框窗格。有 1 行空隙时下方窗格的把手落在空隙
/// 行，两个窗格的内容都不被压；没有空隙时正上方是另一个窗格的内容，把手不去
/// 压它，退回自己的首行——没有 chrome 行可用时宁可盖住一格也保住拖动能力。
#[test]
fn stacked_borderless_pane_handles_never_cover_the_pane_above() {
    let upper = SurfaceRect {
        x: 0,
        y: 0,
        width: 103,
        height: 13,
    };
    let lower = SurfaceRect {
        x: 0,
        y: 14,
        width: 103,
        height: 14,
    };
    let texts = [(0, "UPPER TOP"), (12, "UPPER LAST"), (14, "LOWER TOP")];
    let mut state = arranged_borderless_panes(&[(upper, "pane_1"), (lower, "pane_2")], &texts);
    let rows = frame_rows(&state.compose(133, 32).expect("有空隙"));
    assert_eq!(body_text(&rows, 3, 9), "UPPER TOP");
    assert_eq!(body_text(&rows, 15, 10), "UPPER LAST");
    assert_eq!(body_text(&rows, 17, 9), "LOWER TOP");
    let handles = pane_handles(&state);
    let handle_of = |id: &str| {
        handles
            .iter()
            .find(|(pane, _)| pane == id)
            .map(|(_, rect)| *rect)
            .unwrap_or_else(|| panic!("{id} 的把手：{handles:?}"))
    };
    assert_eq!(handle_of("pane_1").y, 2, "上方窗格的把手在标签栏行");
    assert_eq!(handle_of("pane_2").y, 16, "下方窗格的把手在空隙行");

    let flush = SurfaceRect {
        y: 13,
        height: 15,
        ..lower
    };
    let texts = [(12, "UPPER LAST"), (13, "LOWER TOP")];
    let mut state = arranged_borderless_panes(&[(upper, "pane_1"), (flush, "pane_2")], &texts);
    let rows = frame_rows(&state.compose(133, 32).expect("无空隙"));
    assert_eq!(body_text(&rows, 15, 10), "UPPER LAST", "不压上方窗格的内容");
    let handles = pane_handles(&state);
    let lower_handle = handles
        .iter()
        .find(|(pane, _)| pane == "pane_2")
        .map(|(_, rect)| *rect)
        .expect("没有 chrome 行也保留把手");
    assert_eq!(lower_handle.y, 16, "退回自己的首行");
}

/// 页脚行里某个字符所在的列（找不到就失败），用来往那一格发鼠标事件。
fn footer_x(state: &ClientShellState, needle: &str) -> u16 {
    let buffer = state.compose_buffer.as_ref().expect("保留帧缓冲");
    let y = buffer.area.bottom() - 1;
    (0..buffer.area.width)
        .find(|x| buffer[(*x, y)].symbol() == needle)
        .unwrap_or_else(|| panic!("页脚没有 {needle:?}"))
}

/// SGR 1006 鼠标移动（无按键）报告，坐标取 0 起的单元格。
fn sgr_move(x: u16, y: u16) -> Vec<u8> {
    format!("\x1b[<35;{};{}M", x + 1, y + 1).into_bytes()
}

/// 复审轻级 B2：「调整布局」页脚里可点的「Esc 完成」悬浮时标签换底（与其它
/// 可点页脚同一套悬浮反馈），移开即恢复；不可点的提示不给悬浮反馈。
#[test]
fn arrange_footer_done_hint_shows_hover_feedback() {
    let mut state = ready();
    state.workbench.arranging = true;
    state.compose(133, 32).expect("调整布局模式");
    let palette = state.config.palette.clone();
    let footer = 31;
    let plain = footer_cell(&state, "完");
    assert_ne!(plain.bg, palette.hover_row_bg(), "未悬浮时是常态底色");

    let moved = state.handle_input_bytes(&sgr_move(footer_x(&state, "完"), footer));
    assert!(moved.repaint, "指针移到可点提示上要重绘");
    state.compose(133, 32).expect("悬浮");
    let hovered = footer_cell(&state, "完");
    assert_eq!(hovered.bg, palette.hover_row_bg(), "悬浮标签换底");
    assert_eq!(hovered.fg, palette.text, "悬浮标签提亮");

    let moved = state.handle_input_bytes(&sgr_move(footer_x(&state, "切"), footer));
    assert!(moved.repaint, "移开可点提示要重绘");
    state.compose(133, 32).expect("移到不可点提示");
    assert_eq!(footer_cell(&state, "完").bg, plain.bg, "移开后恢复常态");
    assert_ne!(
        footer_cell(&state, "切").bg,
        palette.hover_row_bg(),
        "不可点的提示不给悬浮反馈"
    );
}

/// 冒烟 L3：非聚焦面板标题（截屏 10 里 overlay0 叠 surface0 只有 2.57:1）按对比度
/// 选色，对标题栏底色 ≥ 4.5:1；聚焦面板仍用 accent 区分。
#[test]
fn unfocused_panel_titles_are_readable() {
    let mut state = ready();
    state.compose(133, 32).expect("工作台");
    let buffer = state.compose_buffer.as_ref().expect("保留帧缓冲");
    let title = (0..133)
        .find(|x| buffer[(*x, 1)].symbol() == "工")
        .expect("工作区面板标题");
    let cell = &buffer[(title, 1)];
    assert_ne!(
        state.workbench.dock.focused,
        crate::client::shell::dock::PanelId::Workspaces,
        "夹具前提：工作区面板未聚焦"
    );
    let ratio = crate::ui::color::contrast_ratio(cell.fg, cell.bg).expect("可比较");
    assert!(ratio >= 4.5, "非聚焦标题对比度 {ratio}");
    assert_ne!(cell.fg, state.config.palette.accent, "与聚焦标题仍可区分");
}

/// 视图帧（经渲染连接）：终端组 1 的 view、`tab_1`，投影修订号 `projection_revision`。
fn view_frame(
    state: &ClientShellState,
    projection_revision: u64,
) -> crate::protocol::views::DecodedView {
    let mut frame = surface();
    frame.projection_revision = projection_revision;
    crate::protocol::views::DecodedView {
        boot_id: frame.boot_id.clone(),
        views_revision: state.workbench.revision,
        view_id: "1".into(),
        tab_id: "tab_1".into(),
        message: ServerMessage::PaneSurface(frame),
    }
}

/// 快照（经控制连接），换成给定的投影修订号；连接代次 1，与视图帧配套。
fn advance_snapshot(state: &mut ClientShellState, revision: u64) {
    let mut next = snapshot();
    next.revision = revision;
    state.set_endpoint_snapshot_for_generation(
        &crate::client::endpoint::ClientEndpointId::Local,
        1,
        Box::new(next),
    );
}

/// 整帧文本去掉空白：宽字符后的占位格是空格，「正在同步终端」按原样找不到。
fn screen(state: &mut ClientShellState) -> String {
    frame_rows(&state.compose(133, 32).expect("工作台帧"))
        .join("\n")
        .split_whitespace()
        .collect()
}

const SYNCING: &str = "正在同步终端";

/// 外部来源变化等纯 chrome 刷新让快照修订号前进时，新快照走控制连接、同 tick 补
/// 的改戳帧走渲染连接，两路先后不定：快照先到时空闲窗格沿用上一帧，不闪一帧
/// 「正在同步终端…」，配对的改戳帧到达后照常；改戳帧先到也一样。
#[test]
fn unpaired_snapshot_and_view_keep_the_last_frame_instead_of_the_placeholder() {
    let mut state = ready();
    advance_snapshot(&mut state, 1);
    assert!(state.receive_view(1, view_frame(&state, 1)), "配对的视图帧");
    assert!(
        screen(&mut state).contains("LIVE"),
        "夹具前提：画出终端内容"
    );

    // 快照先到、改戳帧后到。
    advance_snapshot(&mut state, 2);
    let text = screen(&mut state);
    assert!(
        text.contains("LIVE") && !text.contains(SYNCING),
        "快照先到：沿用上一帧，不画占位：{text}"
    );
    assert!(state.receive_view(1, view_frame(&state, 2)), "改戳帧");
    let text = screen(&mut state);
    assert!(text.contains("LIVE") && !text.contains(SYNCING), "{text}");

    // 改戳帧先到、快照后到。
    assert!(state.receive_view(1, view_frame(&state, 3)), "改戳帧先到");
    let text = screen(&mut state);
    assert!(
        text.contains("LIVE") && !text.contains(SYNCING),
        "改戳帧先到：画面照常：{text}"
    );
    advance_snapshot(&mut state, 3);
    let text = screen(&mut state);
    assert!(text.contains("LIVE") && !text.contains(SYNCING), "{text}");
}

/// 不配对最多沿用 `UNPAIRED_GRACE`：配对帧迟迟不来（例如改戳帧丢了）就回到
/// 占位；`tick_workbench` 在到期那一刻请求重绘，占位按时出现而不是等下一次输入。
#[test]
fn unpaired_view_falls_back_to_the_placeholder_once_the_grace_expires() {
    use crate::client::shell::workbench::UNPAIRED_GRACE;
    let mut state = ready();
    advance_snapshot(&mut state, 1);
    assert!(state.receive_view(1, view_frame(&state, 1)));
    screen(&mut state);
    advance_snapshot(&mut state, 2);
    assert!(!screen(&mut state).contains(SYNCING), "宽限内沿用上一帧");
    let until = state
        .workbench
        .stale_until
        .expect("沿用旧帧时记下宽限到期时刻");
    let mut due = ClientShellInput::default();
    state.tick_workbench(until, &mut due);
    assert!(due.repaint, "宽限到期要重绘");
    assert!(state.workbench.stale_until.is_none(), "到期只触发一次");

    // 组合读真实时钟：把配对断开的时刻挪到宽限之前，模拟宽限已过。
    let past = std::time::Instant::now()
        .checked_sub(UNPAIRED_GRACE + std::time::Duration::from_millis(10))
        .expect("单调时钟足够早");
    state.workbench.snapshot_seen = Some((2, past));
    for stamp in state.workbench.stamps.values_mut() {
        stamp.1 = past;
    }
    assert!(screen(&mut state).contains(SYNCING), "宽限过后回到占位");
    assert!(state.workbench.stale_until.is_none(), "没有视图在沿用旧帧");
}

/// 窗格结构变了（快照里已经没有画面上的窗格）就不沿用旧帧：宁可画占位，也不让
/// 已关闭的窗格留在屏幕上接收点击。
#[test]
fn unpaired_view_whose_pane_is_gone_shows_the_placeholder() {
    let mut state = ready();
    advance_snapshot(&mut state, 1);
    assert!(state.receive_view(1, view_frame(&state, 1)));
    screen(&mut state);
    let mut next = snapshot();
    next.revision = 2;
    next.panes[0].pane_id = "pane_9".into();
    next.focused_pane_id = Some("pane_9".into());
    state.set_endpoint_snapshot_for_generation(
        &crate::client::endpoint::ClientEndpointId::Local,
        1,
        Box::new(next),
    );
    let text = screen(&mut state);
    assert!(text.contains(SYNCING), "画面上的窗格已不在快照里：{text}");
}

/// 标题栏（第 1 行）去掉空白后的文字：宽字符后的占位格是空格。
fn title_text(state: &ClientShellState) -> String {
    let buffer = state.compose_buffer.as_ref().expect("保留帧缓冲");
    (0..buffer.area.width)
        .map(|x| buffer[(x, 1)].symbol())
        .collect::<String>()
        .split_whitespace()
        .collect()
}

/// 标题栏里第一个 `needle` 字符所在的列（找不到就失败）。
fn title_x(state: &ClientShellState, needle: &str) -> u16 {
    let buffer = state.compose_buffer.as_ref().expect("保留帧缓冲");
    (0..buffer.area.width)
        .find(|x| buffer[(*x, 1)].symbol() == needle)
        .unwrap_or_else(|| panic!("标题栏没有 {needle:?}：{}", title_text(state)))
}

/// SGR 1006 左键按下再松开，坐标取 0 起的单元格。
fn sgr_click(state: &mut ClientShellState, x: u16, y: u16) {
    state.handle_input_bytes(format!("\x1b[<0;{};{}M", x + 1, y + 1).as_bytes());
    state.handle_input_bytes(format!("\x1b[<0;{};{}m", x + 1, y + 1).as_bytes());
}

/// 冒烟 L18：workbench 不走 mobile 单列布局，窗口小于停靠布局的最小尺寸时改用
/// 「紧凑视图」只投影聚焦面板，以前切面板只能进「调整布局」再按 Tab。紧凑视图的
/// 标题栏改成面板切换条：按布局顺序列出全部面板，当前面板反色，点别的名字直接
/// 切过去；页脚随之说明。
#[test]
fn compact_view_title_bar_switches_panels_with_one_click() {
    // 文档里的阈值：默认布局需要 37 列、13 行；终端旁停靠监控面板后需要 66 列。
    let mut state = ready();
    for (cols, rows, compact) in [(37, 13, false), (36, 32, true), (37, 12, true)] {
        state.compose(cols, rows).expect("默认布局");
        assert_eq!(
            state.workbench.geometry.compact, compact,
            "默认布局 {cols}×{rows}"
        );
    }
    state.workbench_open(PanelId::Monitor);
    for (cols, compact) in [(66, false), (65, true)] {
        state.compose(cols, 32).expect("停靠监控面板");
        assert_eq!(
            state.workbench.geometry.compact, compact,
            "带监控面板 {cols} 列"
        );
    }
    let mut state = ready();
    state.config.mouse_capture = true;
    // 监控面板让布局最小宽度超过 62 列：62 列时只投影聚焦面板（截屏 93）。
    state.workbench_open(PanelId::Monitor);
    state.workbench.dock.focused = PanelId::Agents;
    state.compose(62, 32).expect("紧凑视图");
    assert!(
        state.workbench.geometry.compact,
        "用例前提：62 列是紧凑视图"
    );
    let title = title_text(&state);
    for name in ["工作区", "Agents", "client-shell", "监控"] {
        assert!(title.contains(name), "切换条列出「{name}」：{title}");
    }
    assert!(
        !title.contains('⠿'),
        "紧凑视图没有可停靠的目标，不画拖动把手"
    );
    let buffer = state.compose_buffer.as_ref().expect("保留帧缓冲");
    let current = &buffer[(title_x(&state, "A"), 1)];
    assert_eq!(current.bg, state.config.palette.accent, "当前面板反色");
    let other = &buffer[(title_x(&state, "工"), 1)];
    assert_ne!(other.bg, state.config.palette.accent, "其它面板不反色");
    let ratio = crate::ui::color::contrast_ratio(other.fg, other.bg).expect("可比较");
    assert!(ratio >= 4.5, "其它面板名对标题栏底色的对比度 {ratio}");
    let footer = frame_rows(&state.compose(62, 32).expect("页脚"))[31]
        .split_whitespace()
        .collect::<String>();
    assert!(
        footer.contains("点上方的面板名"),
        "页脚说明点名字切换：{footer}"
    );

    // 按住名字拖进面板中部再松开：只切换，不开始停靠拖动（紧凑视图只投影一个
    // 面板，没有可停靠的目标），不画「放到这里」预览。
    let x = title_x(&state, "c");
    state.handle_input_bytes(format!("\x1b[<0;{};2M", x + 1).as_bytes());
    assert!(
        matches!(state.workbench.dock.focused, PanelId::Terminal(_)),
        "点终端组的名字切到终端：{:?}",
        state.workbench.dock.focused
    );
    state.handle_input_bytes(b"\x1b[<32;31;16M");
    let dragged = frame_rows(&state.compose(62, 32).expect("拖动中"))
        .concat()
        .split_whitespace()
        .collect::<String>();
    assert!(!dragged.contains("放到这里"), "没有停靠预览：{dragged}");
    state.handle_input_bytes(b"\x1b[<0;31;16m");
    state.compose(62, 32).expect("切换后");
    assert!(
        matches!(
            state.workbench.geometry.panels.as_slice(),
            [(PanelId::Terminal(_), _)]
        ),
        "紧凑视图改投影终端组"
    );
    let buffer = state.compose_buffer.as_ref().expect("保留帧缓冲");
    assert_eq!(
        buffer[(title_x(&state, "c"), 1)].bg,
        state.config.palette.accent,
        "反色跟到终端组"
    );
}

/// 最大化后再变窄：紧凑投影画的是最大化面板。点切换条的名字时最大化随之移过去
/// （同调整布局模式的 Tab），否则画面不变、键盘却落到看不见的面板上。
#[test]
fn compact_switcher_moves_the_maximized_panel_with_the_focus() {
    let mut state = ready();
    state.config.mouse_capture = true;
    state.workbench_open(PanelId::Monitor);
    let terminal = PanelId::Terminal(1);
    state.workbench.dock.focused = terminal.clone();
    state.workbench.dock.maximized = Some(terminal.clone());
    state.compose(62, 32).expect("最大化后的紧凑视图");
    assert!(
        matches!(state.workbench.geometry.panels.as_slice(), [(panel, _)] if *panel == terminal),
        "用例前提：只投影最大化的终端组"
    );

    let x = title_x(&state, "A");
    sgr_click(&mut state, x, 1);
    assert_eq!(state.workbench.dock.focused, PanelId::Agents);
    assert_eq!(
        state.workbench.dock.maximized,
        Some(PanelId::Agents),
        "最大化跟随新焦点"
    );
    state.compose(62, 32).expect("切换后");
    assert!(
        matches!(
            state.workbench.geometry.panels.as_slice(),
            [(PanelId::Agents, _)]
        ),
        "画面换成 Agents 面板"
    );
}

/// 面板名整排放不下时退回只画聚焦面板的标题，页脚仍指向「调整布局」；页脚放不下
/// 时截短并以省略号收尾，不在字中间硬切。
#[test]
fn compact_switcher_falls_back_to_the_focused_title_when_names_do_not_fit() {
    use crate::client::shell::workbench::interaction::Action;
    let mut state = ready();
    state.workbench_open(PanelId::Monitor);
    state.workbench.dock.focused = PanelId::Agents;
    let rows = frame_rows(&state.compose(24, 32).expect("很窄的紧凑视图"));
    assert!(state.workbench.geometry.compact, "用例前提：紧凑视图");
    let title = rows[1].split_whitespace().collect::<String>();
    assert!(
        title.contains("Agents") && !title.contains("工作区"),
        "只画聚焦面板的标题：{title}"
    );
    assert!(
        state
            .workbench
            .hits
            .iter()
            .all(|(_, action)| !matches!(action, Action::Switch(_))),
        "没有切换条命中区"
    );
    let footer = rows[31].split_whitespace().collect::<String>();
    assert!(footer.starts_with("紧凑视图"), "页脚说明紧凑视图：{footer}");
    assert!(footer.ends_with('…'), "放不下的页脚以省略号收尾：{footer}");
}

/// 复审轻级 W1（62×32，截屏 93 同尺寸）：`mouse_capture = false` 时工作台根本不接
/// 鼠标（`workbench_mouse` 开头就放行），点切换条上的面板名到不了。页脚不能再
/// 提示「点上方的面板名」、把键盘用户唯一可见的「调整布局 + Tab」提示换掉，也不
/// 登记点不到的切换命中区；切换条照画，标出当前在哪个面板。
#[test]
fn compact_switcher_without_mouse_capture_keeps_the_keyboard_hint() {
    use crate::client::shell::workbench::interaction::Action;
    let _guard = crate::i18n::lang_guard(crate::i18n::Lang::ZhCn);
    let mut state = ready();
    state.config.mouse_capture = false;
    state.workbench_open(PanelId::Monitor);
    state.workbench.dock.focused = PanelId::Agents;
    let rows = frame_rows(&state.compose(62, 32).expect("紧凑视图"));
    assert!(
        state.workbench.geometry.compact,
        "用例前提：62 列是紧凑视图"
    );
    let title = title_text(&state);
    for name in ["工作区", "Agents", "client-shell", "监控"] {
        assert!(title.contains(name), "切换条照画「{name}」：{title}");
    }
    assert!(
        state
            .workbench
            .hits
            .iter()
            .all(|(_, action)| !matches!(action, Action::Switch(_))),
        "不接鼠标时不登记切换条命中区"
    );
    let footer = rows[31].split_whitespace().collect::<String>();
    assert!(
        footer.contains("在「调整布局」模式用Tab切换面板"),
        "页脚保留键盘切换提示：{footer}"
    );
    assert!(
        !footer.contains("点上方的面板名"),
        "点不到就不提示点击：{footer}"
    );

    let x = title_x(&state, "工");
    sgr_click(&mut state, x, 1);
    assert_eq!(
        state.workbench.dock.focused,
        PanelId::Agents,
        "不接鼠标：点名字不切换"
    );
}

/// 标题栏（第 1 行）里只落在面板标题命中区（`Action::Header`）上的列：段间空隙、
/// 名字右侧的空白、放不下时的回退标题。按命中表自后向前取最上层，与分派同口径。
fn header_only_columns(state: &ClientShellState) -> Vec<u16> {
    use crate::client::shell::workbench::interaction::Action;
    let width = state
        .compose_buffer
        .as_ref()
        .expect("保留帧缓冲")
        .area
        .width;
    (0..width)
        .filter(|x| {
            let hit = state.workbench.hits.iter().rev().find(|(rect, _)| {
                *x >= rect.x && *x < rect.right() && 1 >= rect.y && 1 < rect.bottom()
            });
            matches!(hit, Some((_, Action::Header(_))))
        })
        .collect()
}

/// 在标题栏 `x` 列按下、拖进面板中部，返回拖动中那一帧去掉空白的文字；最后松开。
fn drag_from_title_into_the_panel(state: &mut ClientShellState, x: u16, cols: u16) -> String {
    state.handle_input_bytes(format!("\x1b[<0;{};2M", x + 1).as_bytes());
    let (to_x, to_y) = (cols / 2, 16);
    state.handle_input_bytes(format!("\x1b[<32;{};{}M", to_x + 1, to_y + 1).as_bytes());
    let dragged = frame_rows(&state.compose(cols, 32).expect("拖动中"))
        .concat()
        .split_whitespace()
        .collect::<String>();
    state.handle_input_bytes(format!("\x1b[<0;{};{}m", to_x + 1, to_y + 1).as_bytes());
    dragged
}

/// 复审轻级 W2（62×32）：紧凑视图只投影一个面板，没有可停靠的目标。标题栏上只有
/// 面板名段是切换命中区，段间空隙与名字右侧的空白仍是面板标题：以前在那里按下
/// 拖动会开始停靠拖动，画出无意义的「放到这里」预览。现在只切焦点，不开始拖动。
#[test]
fn compact_title_bar_blank_space_never_starts_a_dock_drag() {
    let _guard = crate::i18n::lang_guard(crate::i18n::Lang::ZhCn);
    let mut state = ready();
    state.config.mouse_capture = true;
    state.workbench_open(PanelId::Monitor);
    state.workbench.dock.focused = PanelId::Agents;
    state.compose(62, 32).expect("紧凑视图");
    assert!(
        state.workbench.geometry.compact,
        "用例前提：62 列是紧凑视图"
    );
    let layout = state.workbench.dock.clone();
    let columns = header_only_columns(&state);
    let (Some(&gap), Some(&blank)) = (columns.first(), columns.last()) else {
        panic!("用例前提：标题栏有面板标题命中区：{}", title_text(&state));
    };
    assert!(
        gap < title_x(&state, "A"),
        "用例前提：第一处是名字之间的空隙（{gap}）"
    );
    for x in [gap, blank] {
        let dragged = drag_from_title_into_the_panel(&mut state, x, 62);
        assert!(
            !dragged.contains("放到这里"),
            "第 {x} 列按下拖动：不画停靠预览：{dragged}"
        );
        assert_eq!(state.workbench.dock, layout, "第 {x} 列按下拖动：布局不变");
        assert_eq!(state.workbench.dock.focused, PanelId::Agents);
    }
}

/// 复审轻级 W2（24×32）：面板名整排放不下时退回只画聚焦面板的标题。紧凑视图没有
/// 停靠目标，回退标题不画 `⠿` 拖动把手，按住它拖进面板也不画「放到这里」。
#[test]
fn compact_fallback_title_has_no_drag_handle() {
    let _guard = crate::i18n::lang_guard(crate::i18n::Lang::ZhCn);
    let mut state = ready();
    state.config.mouse_capture = true;
    state.workbench_open(PanelId::Monitor);
    state.workbench.dock.focused = PanelId::Agents;
    let rows = frame_rows(&state.compose(24, 32).expect("很窄的紧凑视图"));
    assert!(state.workbench.geometry.compact, "用例前提：紧凑视图");
    assert!(
        rows[1].contains("Agents") && !rows[1].contains("工作区"),
        "用例前提：回退为只画聚焦面板的标题：{}",
        rows[1]
    );
    assert!(!rows[1].contains('⠿'), "回退标题不画拖动把手：{}", rows[1]);
    let layout = state.workbench.dock.clone();
    let x = title_x(&state, "A");
    let dragged = drag_from_title_into_the_panel(&mut state, x, 24);
    assert!(!dragged.contains("放到这里"), "不画停靠预览：{dragged}");
    assert_eq!(state.workbench.dock, layout, "布局不变");
}
