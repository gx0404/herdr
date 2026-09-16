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
    use crate::client::shell::observability::{Hover, Page};
    let mut state = ready();
    state.workbench_open(PanelId::Monitor);
    state.observability.hover = Some(Hover {
        endpoint_id: state.active_endpoint_id.clone(),
        pane: "pane_1".into(),
        agent: "claude".into(),
        anchor: Rect::new(0, 20, 24, 2),
        since: std::time::Instant::now(),
        visible: true,
        leave_at: None,
    });
    state.compose(120, 40).expect("监控与悬浮层可同时显示");
    assert_eq!(state.observability.page, Some(Page::Monitor));
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
    use crate::client::shell::observability::{Action, Hover};
    let mut state = ready();
    state.set_endpoint_methods(Some(vec![
        "client.views.set".into(),
        "system.process.get".into(),
    ]));
    state.observability.accounts = vec![crate::api::schema::AccountUsageSnapshot::default()];
    state.workbench_open(PanelId::Monitor);
    state.observability.hover = Some(Hover {
        endpoint_id: state.active_endpoint_id.clone(),
        pane: "pane_1".into(),
        agent: "claude".into(),
        anchor: Rect::new(0, 20, 24, 2),
        since: std::time::Instant::now(),
        visible: true,
        leave_at: None,
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
    assert_eq!(state.observability.account_scroll, 1);
}

#[test]
fn dashboard_can_scroll_to_quota_windows_below_the_viewport() {
    let mut state = ready();
    state.workbench_open(PanelId::Accounts);
    state.workbench.dock.maximized = Some(PanelId::Accounts);
    state.observability.accounts = vec![crate::api::schema::AccountUsageSnapshot {
        account_label: "account".into(),
        metrics: (0..10)
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
    assert!(!frame_rows(&before).join("\n").contains("metric-09"));
    for _ in 0..12 {
        state.handle_input_bytes(b"\x1b[6~");
    }
    let after = state.compose(120, 24).unwrap();
    assert!(frame_rows(&after).join("\n").contains("metric-09"));
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
        matches!(state.overlay, Some(ClientShellOverlay::GlobalMenu(_))),
        "释放后菜单点击不能再被拖动捕获"
    );
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
    state
        .workbench
        .dock
        .reconcile_tabs(&["tab_1".into(), "tab_2".into()], None);
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
    state
        .workbench
        .dock
        .reconcile_tabs(&["tab_1".into(), "tab_2".into()], None);
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
    state.sync_workbench_surface();
    state.compose(120, 40).unwrap();
    assert!(state.enter_copy_mode(&mut ClientShellInput::default()));
    state.handle_input_bytes(b"/needle");
    let frame = state.compose(120, 40).unwrap();
    assert!(frame_rows(&frame).join("\n").contains("needle"));
    assert!(frame.cursor.is_none());
}
