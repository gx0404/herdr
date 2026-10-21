use super::super::feedback::{
    ChromeHover, ClientNotificationTarget, ClientToastLevel, ENTRANCE_DURATION,
    NOTIFICATION_HISTORY_LIMIT,
};
use super::*;

fn moved_mouse(col: u16, row: u16) -> MouseEvent {
    MouseEvent {
        kind: MouseEventKind::Moved,
        column: col,
        row,
        modifiers: KeyModifiers::empty(),
    }
}

fn chrome_state() -> ClientShellState {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    state.compose(106, 20).expect("chrome shell");
    state
}

#[test]
fn chrome_hover_marks_the_tab_under_the_pointer() {
    let mut state = chrome_state();
    let (tab_rect, tab_id) = state.hits.tabs[0].clone();

    let mut outcome = ClientShellInput::default();
    state.handle_mouse(moved_mouse(tab_rect.x, tab_rect.y), &mut outcome);
    assert!(outcome.repaint);
    assert_eq!(state.hover, Some(ChromeHover::Tab(tab_id)));

    // A stationary pointer keeps the identity without a repaint.
    let mut outcome = ClientShellInput::default();
    state.handle_mouse(moved_mouse(tab_rect.x, tab_rect.y), &mut outcome);
    assert!(!outcome.repaint);

    // Leaving the chrome target clears the highlight with one repaint.
    let mut outcome = ClientShellInput::default();
    state.handle_mouse(moved_mouse(105, 19), &mut outcome);
    assert!(outcome.repaint);
    assert_eq!(state.hover, None);
}

#[test]
fn chrome_hover_marks_workspace_rows() {
    let mut state = chrome_state();
    let row = state.hits.workspaces[0].rect;

    let mut outcome = ClientShellInput::default();
    state.handle_mouse(moved_mouse(row.x, row.y), &mut outcome);

    assert!(outcome.repaint);
    assert_eq!(
        state.hover,
        Some(ChromeHover::WorkspaceRow {
            endpoint_id: ClientEndpointId::Local,
            workspace_id: "ws_1".into(),
        })
    );
}

/// SB-04（与 C-28 同根）：terminal 主题的 `surface0` 曾经是 `Color::Reset`，
/// 侧栏行与标签的 hover 底色于是和常态行一模一样——指针移动没有任何反馈。
#[test]
fn terminal_theme_keeps_sidebar_and_tab_hover_visible() {
    let mut config = Config::default();
    config.theme.name = Some("terminal".into());
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&config));
    let mut projected = snapshot();
    let mut second_workspace = projected.workspaces[0].clone();
    second_workspace.workspace_id = "ws_2".into();
    second_workspace.number = 2;
    second_workspace.label = "second".into();
    second_workspace.focused = false;
    second_workspace.active_tab_id = "tab_3".into();
    projected.workspaces.push(second_workspace);
    // 标签条只画聚焦工作区的标签：第二个未聚焦标签留在 ws_1。
    let mut second_tab = projected.tabs[0].clone();
    second_tab.tab_id = "tab_2".into();
    second_tab.number = 2;
    second_tab.focused = false;
    projected.tabs.push(second_tab);
    let mut third_tab = projected.tabs[1].clone();
    third_tab.tab_id = "tab_3".into();
    third_tab.workspace_id = "ws_2".into();
    third_tab.number = 3;
    projected.tabs.push(third_tab);
    state.set_snapshot(Box::new(projected));
    state.set_pane_surface(surface());
    state.compose(106, 20).expect("chrome shell");

    let sidebar_row = state
        .hits
        .workspaces
        .iter()
        .find(|hit| hit.workspace_id == "ws_2")
        .map(|hit| hit.rect)
        .expect("ws_2 row");
    let (tab_rect, tab_id) = state
        .hits
        .tabs
        .iter()
        .find(|(_, tab_id)| tab_id == "tab_2")
        .cloned()
        .expect("tab_2 rect");

    let rendered_bg = |state: &mut ClientShellState, col: u16, row: u16| {
        state
            .compose(106, 20)
            .expect("composed")
            .to_ratatui_buffer()
            .expect("buffer")[(col, row)]
            .bg
    };
    let plain_sidebar = rendered_bg(&mut state, sidebar_row.x + 1, sidebar_row.y);
    let plain_tab = rendered_bg(&mut state, tab_rect.x + 1, tab_rect.y);

    let mut outcome = ClientShellInput::default();
    state.handle_mouse(moved_mouse(sidebar_row.x, sidebar_row.y), &mut outcome);
    assert_eq!(
        state.hover,
        Some(ChromeHover::WorkspaceRow {
            endpoint_id: ClientEndpointId::Local,
            workspace_id: "ws_2".into(),
        })
    );
    let hovered_sidebar = rendered_bg(&mut state, sidebar_row.x + 1, sidebar_row.y);
    assert_ne!(
        hovered_sidebar, plain_sidebar,
        "terminal 主题的侧栏 hover 与常态行同像素"
    );
    assert_ne!(hovered_sidebar, ratatui::style::Color::Reset);

    let mut outcome = ClientShellInput::default();
    state.handle_mouse(moved_mouse(tab_rect.x, tab_rect.y), &mut outcome);
    assert_eq!(state.hover, Some(ChromeHover::Tab(tab_id)));
    let hovered_tab = rendered_bg(&mut state, tab_rect.x, tab_rect.y);
    assert_ne!(
        hovered_tab, plain_tab,
        "terminal 主题的标签 hover 与常态标签同像素"
    );
    assert_ne!(hovered_tab, ratatui::style::Color::Reset);

    // 走完整解析链（`theme.name = "terminal"`）后，结构面确实落在 ANSI 灰阶上。
    assert_eq!(
        state.config.palette.surface0,
        ratatui::style::Color::DarkGray
    );
    assert_eq!(state.config.palette.surface1, ratatui::style::Color::Gray);
}

#[test]
fn chrome_hover_stays_off_when_hover_effects_are_disabled() {
    let mut state = chrome_state();
    state.config.feedback.hover_effects = false;
    let (tab_rect, _) = state.hits.tabs[0].clone();

    let mut outcome = ClientShellInput::default();
    state.handle_mouse(moved_mouse(tab_rect.x, tab_rect.y), &mut outcome);

    assert!(!outcome.repaint);
    assert_eq!(state.hover, None);
}

#[test]
fn modal_button_state_prefers_the_pointer_over_the_keyboard_base_state() {
    let config = ClientShellConfig::from_config(&Config::default());
    let primary = ChromeHover::OverlayPrimary;
    let cx = super::super::feedback::ChromeContext {
        page_bounds: None,
        palette: &config.palette,
        components: &config.components,
        glyphs: config.border_glyphs,
        hover: Some(&primary),
        spinner: "◐",
        now: std::time::Instant::now(),
    };
    assert_eq!(
        cx.button_state(
            &ChromeHover::OverlayPrimary,
            crate::ui::ModalButtonState::Normal
        ),
        crate::ui::ModalButtonState::Hovered
    );
    assert_eq!(
        cx.button_state(
            &ChromeHover::OverlayCancel,
            crate::ui::ModalButtonState::Focused
        ),
        crate::ui::ModalButtonState::Focused
    );
}

#[test]
fn toast_levels_map_kinds_to_icons_and_border_tokens() {
    let components = ClientShellConfig::from_config(&Config::default()).components;
    assert_eq!(
        ClientToastLevel::from_notification_kind(SemanticNotificationKind::NeedsAttention),
        ClientToastLevel::Warning,
        "需要你处理：琥珀色警告，不与执行失败共用红色"
    );
    assert_eq!(
        ClientToastLevel::from_notice_kind(ClientEndpointNoticeKind::Warning),
        ClientToastLevel::Warning
    );
    assert_eq!(
        ClientToastLevel::from_notice_kind(ClientEndpointNoticeKind::Rejected),
        ClientToastLevel::Error
    );
    assert_eq!(
        ClientToastLevel::from_notification_kind(SemanticNotificationKind::Finished),
        ClientToastLevel::Success
    );
    assert_eq!(
        ClientToastLevel::from_notification_kind(SemanticNotificationKind::UpdateInstalled),
        ClientToastLevel::Info
    );
    assert_eq!(
        ClientToastLevel::from_notification_kind(SemanticNotificationKind::Custom),
        ClientToastLevel::Info
    );
    assert_eq!(
        ClientToastLevel::Info.color(&components),
        components.toast_border_info
    );
    assert_eq!(
        ClientToastLevel::Success.color(&components),
        components.toast_border_success
    );
    assert_eq!(
        ClientToastLevel::Error.color(&components),
        components.toast_border_error
    );
    assert_eq!(
        ClientToastLevel::Warning.color(&components),
        components.status_input,
        "警告与「需要输入」状态同色"
    );
    assert_eq!(ClientToastLevel::Warning.icon(), "◆");
    assert!(!ClientToastLevel::Info.icon().is_empty());
}

#[test]
fn spinner_clock_starts_on_the_first_active_tick_then_advances_per_interval() {
    let mut state = chrome_state();
    state.set_endpoint_status(&ClientEndpointId::Local, ClientEndpointStatus::Connecting);
    let start = std::time::Instant::now();

    // First tick with a visible spinner only arms the clock: the frame stays
    // on glyph zero so the opening frame is stable.
    assert!(!state.tick_chrome_feedback(start));
    assert_eq!(state.spinner_tick, 0);

    let interval = state.config.feedback.spinner.interval;
    assert!(!state.tick_chrome_feedback(start + interval - std::time::Duration::from_millis(1)));
    assert_eq!(state.spinner_tick, 0);

    assert!(state.tick_chrome_feedback(start + interval));
    assert_eq!(state.spinner_tick, 1);
    assert_eq!(state.spinner_glyph(), "◓");

    // Once the spinner source goes away the clock disarms silently.
    state.set_endpoint_status(&ClientEndpointId::Local, ClientEndpointStatus::Online);
    assert!(!state.tick_chrome_feedback(start + interval * 2));
}

#[test]
fn spinner_stays_on_frame_zero_when_animations_are_off() {
    let mut state = chrome_state();
    state.config.feedback.animations = false;
    state.set_endpoint_status(&ClientEndpointId::Local, ClientEndpointStatus::Connecting);
    let start = std::time::Instant::now();

    assert!(!state.tick_chrome_feedback(start));
    assert!(!state.tick_chrome_feedback(start + std::time::Duration::from_secs(5)));
    assert_eq!(state.spinner_tick, 0);
    assert_eq!(state.spinner_glyph(), "◐");
}

#[test]
fn spinner_also_runs_for_a_worktree_create_in_flight() {
    let mut state = chrome_state();
    assert!(!state.spinner_active());
    state.overlay = Some(ClientShellOverlay::WorktreeCreate(
        ClientWorktreeCreateOverlay {
            source_workspace_id: "ws_1".into(),
            repo_name: "repo".into(),
            branch: "branch".into(),
            checkout_path: "path".into(),
            error: None,
            creating: true,
        },
    ));
    assert!(state.spinner_active());
}

#[test]
fn visual_bell_marks_the_focused_tab_until_the_deadline_expires() {
    let mut state = chrome_state();
    let now = std::time::Instant::now();

    assert!(state.record_terminal_bell(now));
    assert!(state.visual_bell_active());

    state.compose(106, 20).expect("bell frame");
    let (tab_rect, _) = state.hits.tabs[0].clone();
    let rows = frame_rows(&state.compose(106, 20).expect("bell frame"));
    assert_eq!(
        rows[usize::from(tab_rect.y)]
            .chars()
            .nth(usize::from(tab_rect.right() - 2)),
        Some('!')
    );

    // Before the deadline nothing changes; at the deadline the marker
    // expires and requests exactly one repaint.
    assert!(!state.tick_chrome_feedback(now + std::time::Duration::from_millis(500)));
    assert!(state.visual_bell_active());
    assert!(state.tick_chrome_feedback(now + std::time::Duration::from_millis(1_500)));
    assert!(!state.visual_bell_active());
}

#[test]
fn visual_bell_respects_the_toggle() {
    let mut state = chrome_state();
    state.config.feedback.visual_bell = false;

    assert!(!state.record_terminal_bell(std::time::Instant::now()));
    assert!(!state.visual_bell_active());
}

#[test]
fn notification_history_ring_keeps_the_newest_entries() {
    let mut state = chrome_state();
    for index in 0..NOTIFICATION_HISTORY_LIMIT + 10 {
        state.record_notification(
            ClientToastLevel::Info,
            format!("notice-{index}"),
            None,
            None,
        );
    }

    assert_eq!(state.notification_history.len(), NOTIFICATION_HISTORY_LIMIT);
    assert_eq!(
        state
            .notification_history
            .front()
            .map(|record| record.title.as_str()),
        Some("notice-10")
    );
    assert_eq!(
        state
            .notification_history
            .back()
            .map(|record| record.title.as_str()),
        Some(format!("notice-{}", NOTIFICATION_HISTORY_LIMIT + 9).as_str())
    );
}

#[test]
fn notification_history_overlay_navigates_and_jumps_to_the_origin_pane() {
    let mut state = chrome_state();
    state.record_notification(
        ClientToastLevel::Error,
        "boom",
        Some("details".into()),
        Some(ClientNotificationTarget {
            endpoint_id: ClientEndpointId::Local,
            pane_id: Some("pane_1".into()),
            boot_id: None,
        }),
    );
    state.record_notification(ClientToastLevel::Info, "fyi", None, None);

    state.open_notification_history();
    let Some(ClientShellOverlay::NotificationHistory(overlay)) = state.overlay.as_ref() else {
        panic!("history overlay");
    };
    assert_eq!(overlay.selected, 1, "newest entry is preselected");

    state.move_notification_history_selection(-1);
    state.move_notification_history_selection(-1);
    let Some(ClientShellOverlay::NotificationHistory(overlay)) = state.overlay.as_ref() else {
        panic!("history overlay");
    };
    assert_eq!(overlay.selected, 0, "selection clamps at the oldest entry");

    let mut outcome = ClientShellInput::default();
    state.focus_notification_history_target(&mut outcome);
    assert!(
        state.overlay.is_none(),
        "a successful jump closes the overlay"
    );
    let [ClientShellAction::Endpoint { request, .. }] = &outcome.actions[..] else {
        panic!("history jump should use the endpoint API");
    };
    assert!(matches!(
        &request.method,
        crate::api::schema::Method::PaneFocus(target) if target.pane_id == "pane_1"
    ));
}

#[test]
fn notification_history_jump_to_an_offline_machine_keeps_the_overlay() {
    let mut state = chrome_state();
    let remote = ClientEndpointId::Ssh(
        crate::client::endpoint::ProfileId::parse("0123456789abcdef0123456789abcdef").unwrap(),
    );
    state.record_notification(
        ClientToastLevel::Info,
        "remote done",
        None,
        Some(ClientNotificationTarget {
            endpoint_id: remote,
            pane_id: Some("pane_9".into()),
            boot_id: None,
        }),
    );
    state.open_notification_history();

    let mut outcome = ClientShellInput::default();
    state.focus_notification_history_target(&mut outcome);

    assert!(outcome.repaint);
    assert!(outcome.actions.is_empty());
    assert!(
        matches!(
            state.overlay,
            Some(ClientShellOverlay::NotificationHistory(_))
        ),
        "an unreachable target keeps the overlay open"
    );
    assert!(state.visible_endpoint_notice.is_some());
}

#[test]
fn notification_history_overlay_projects_rows_for_mouse() {
    let mut state = chrome_state();
    for title in ["first", "second"] {
        state.record_notification(ClientToastLevel::Info, title, None, None);
    }
    state.open_notification_history();
    state.compose(106, 20).expect("history overlay frame");

    assert_eq!(state.hits.notification_history_rows.len(), 2);

    // 指针悬浮只写 hover：Enter 会跳到该条通知的源 pane，键盘选中不能被
    // 「鼠标路过」改写（MENU-01 / UX-04）。
    let (row_rect, row_index) = state.hits.notification_history_rows[0];
    let selected_before = match state.overlay.as_ref() {
        Some(ClientShellOverlay::NotificationHistory(overlay)) => overlay.selected,
        _ => panic!("history overlay"),
    };
    assert_ne!(selected_before, row_index, "默认选中最新一条，不是第一行");
    let mut outcome = ClientShellInput::default();
    state.handle_mouse(moved_mouse(row_rect.x, row_rect.y), &mut outcome);
    assert!(outcome.repaint);
    let Some(ClientShellOverlay::NotificationHistory(overlay)) = state.overlay.as_ref() else {
        panic!("history overlay");
    };
    assert_eq!(overlay.hovered, Some(row_index));
    assert_eq!(overlay.selected, selected_before, "键盘选中不被指针改写");

    // 指针移出列表：hover 必须清掉，否则弱底色留在鼠标早已离开的那一行。
    let mut outcome = ClientShellInput::default();
    state.handle_mouse(moved_mouse(row_rect.x, row_rect.bottom() + 4), &mut outcome);
    assert!(outcome.repaint);
    let Some(ClientShellOverlay::NotificationHistory(overlay)) = state.overlay.as_ref() else {
        panic!("history overlay");
    };
    assert_eq!(overlay.hovered, None);
    assert_eq!(overlay.selected, selected_before);
}

#[test]
fn overlay_entrance_dim_settles_on_the_timer() {
    let mut state = chrome_state();
    assert!(state.overlay_since.is_none());
    state.toggle_global_menu();
    state.compose(106, 20).expect("menu frame");
    assert!(
        state.overlay_since.is_some(),
        "opening starts the entrance clock"
    );

    let settle = std::time::Instant::now() + ENTRANCE_DURATION;
    assert!(state.tick_chrome_feedback(settle));
    assert!(
        state.overlay_since.is_none(),
        "the timer settles the entrance"
    );

    // With animations off, opening never starts the clock.
    state.overlay = None;
    state.config.feedback.animations = false;
    state.compose(106, 20).expect("frame");
    state.toggle_global_menu();
    state.compose(106, 20).expect("menu frame");
    assert!(state.overlay_since.is_none());
}

#[test]
fn feedback_toggles_come_from_the_ui_config_fields() {
    let mut config = Config::default();
    config.ui.hover_effects = false;
    config.ui.spinner = false;
    config.ui.animations = false;
    config.ui.visual_bell = false;
    let toggles = super::super::feedback::ClientFeedbackToggles::from_config(&config);
    assert!(!toggles.hover_effects);
    assert!(!toggles.spinner.enabled);
    assert!(!toggles.animations);
    assert!(!toggles.visual_bell);

    let defaults = super::super::feedback::ClientFeedbackToggles::from_config(&Config::default());
    assert!(defaults.hover_effects);
    assert!(defaults.spinner.enabled);
    assert!(defaults.animations);
    assert!(defaults.visual_bell);
}

#[test]
fn disabled_spinner_shows_a_static_glyph_and_stops_the_clock() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.config.feedback.spinner.enabled = false;
    state.set_endpoint_status(
        &crate::client::endpoint::ClientEndpointId::Local,
        crate::client::endpoint::ClientEndpointStatus::Reconnecting,
    );
    assert!(!state.spinner_active());
    let frame = state.spinner_glyph();
    state.spinner_tick = 3;
    assert_eq!(
        state.spinner_glyph(),
        frame,
        "disabled spinner never animates"
    );
}

#[test]
fn relative_time_labels_cover_just_now_minutes_hours_and_days() {
    let _lang = crate::i18n::lang_guard(crate::i18n::Lang::En);
    // `Instant - Duration` underflows when the system uptime is shorter than
    // the duration (Windows Instant starts at boot), so anchor the past at a
    // fresh instant and move `now` forward instead.
    let at = std::time::Instant::now();
    let ago = |duration: std::time::Duration| {
        super::super::feedback::relative_time_ago(at, at + duration)
    };
    assert_eq!(ago(std::time::Duration::from_secs(5)), "just now");
    assert_eq!(ago(std::time::Duration::from_secs(5 * 60)), "5m ago");
    assert_eq!(ago(std::time::Duration::from_secs(3 * 3600)), "3h ago");
    assert_eq!(ago(std::time::Duration::from_secs(2 * 86_400)), "2d ago");
}

/// HERDR-PERF-008：`chrome_hover_at` 每次 `Moved` 都要线性扫描全部命中区；
/// 视图计算阶段给出 chrome 悬浮区并集后，指针在 pane 上（最常见情形）可以直接
/// 短路，同时不能因此漏掉真正的 chrome 命中。
#[test]
fn chrome_hover_bounds_short_circuit_pane_pointer_without_losing_chrome_hits() {
    let mut state = chrome_state();
    let bounds = state.hits.chrome_bounds;

    // 指针停在 pane 上：不在并集里，`chrome_hover_at` 可以直接判定不是 chrome。
    let pane = state.hits.panes[0].inner_rect;
    let pane_point = (pane.x + pane.width / 2, pane.y + pane.height / 2);
    assert!(
        !bounds.contains(pane_point),
        "pane 内的指针不应落在 chrome 分组里: {pane_point:?}"
    );

    // chrome 命中仍然解析得出：标签条上的指针既在并集里，也能拿到目标。
    let (tab_rect, tab_id) = state.hits.tabs[0].clone();
    let tab_point = (tab_rect.x, tab_rect.y);
    assert!(
        bounds.contains(tab_point),
        "标签应落在 chrome 分组里: {tab_rect:?}"
    );
    let mut outcome = ClientShellInput::default();
    state.handle_mouse(moved_mouse(tab_rect.x, tab_rect.y), &mut outcome);
    assert_eq!(
        state.hover,
        Some(ChromeHover::Tab(tab_id)),
        "早退不得漏掉标签悬浮"
    );

    // 侧栏行同样解析得出。
    let row = state.hits.workspaces[0].rect;
    let row_point = (row.x + 3, row.y);
    assert!(bounds.contains(row_point), "工作区行应在分组里");
    let mut outcome = ClientShellInput::default();
    state.handle_mouse(moved_mouse(row_point.0, row_point.1), &mut outcome);
    assert_eq!(
        state.hover,
        Some(ChromeHover::WorkspaceRow {
            endpoint_id: ClientEndpointId::Local,
            workspace_id: "ws_1".into(),
        })
    );
}

#[test]
fn notification_card_uses_the_configured_status_indicator_style() {
    use crate::config::StatusIndicatorStyle;
    for (style, glyph) in [
        (StatusIndicatorStyle::Graphite, "◆"),
        (StatusIndicatorStyle::Dots, "●"),
        (StatusIndicatorStyle::Symbols, "×"),
    ] {
        let mut config = Config::default();
        config.ui.status_indicators = style;
        let mut state = ClientShellState::new(ClientShellConfig::from_config(&config));
        state.set_snapshot(Box::new(snapshot()));
        state.set_pane_surface(surface());
        state.visible_notification = Some(ClientVisibleNotification {
            endpoint_id: ClientEndpointId::Local,
            event: SemanticNotification {
                kind: SemanticNotificationKind::NeedsAttention,
                title: "permission request".into(),
                body: Some("Confirm editing this file".into()),
                sound: None,
                agent: Some("codex".into()),
                workspace_id: None,
                tab_id: None,
                pane_id: Some("pane_1".into()),
                position: None,
            },
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(5),
        });
        let frame = state.compose(80, 24).expect("notification frame");
        let buffer = frame.to_ratatui_buffer().expect("notification buffer");
        let rect = state.hits.notification_toast;
        assert!(!rect.is_empty());
        let cell = &buffer[(rect.x + 2, rect.y + 1)];
        assert_eq!(cell.symbol(), glyph, "{style:?}");
        assert_eq!(cell.fg, state.config.components.status_input);

        state.visible_notification = None;
        state.record_notification(ClientToastLevel::Error, "permission request", None, None);
        state
            .notification_history
            .back_mut()
            .expect("history record")
            .bucket = Some(crate::harness::AgentBucket::NeedsInput);
        state.open_notification_history();
        let frame = state.compose(80, 24).expect("history frame");
        let buffer = frame.to_ratatui_buffer().expect("history buffer");
        let (rect, _) = state.hits.notification_history_rows[0];
        assert_eq!(buffer[(rect.x + 1, rect.y)].symbol(), glyph, "{style:?}");

        state.open_command_search();
        let Some(ClientShellOverlay::CommandPalette(palette)) = state.overlay.as_mut() else {
            panic!("command search");
        };
        let mut item = palette.items[0].clone();
        item.id = "test:agent".into();
        item.enabled = true;
        item.action = super::super::command_palette::ClientPaletteAction::Agent(
            ClientEndpointId::Local,
            "pane_1".into(),
            crate::harness::AgentBucket::NeedsInput,
            "boot-1".into(),
            0,
        );
        palette.items = vec![item];
        let frame = state.compose(80, 24).expect("command search frame");
        let buffer = frame.to_ratatui_buffer().expect("command search buffer");
        let (rect, _) = state.hits.global_menu_rows[0];
        assert_eq!(buffer[(rect.x + 1, rect.y)].symbol(), glyph, "{style:?}");
        assert_eq!(
            buffer[(rect.x + 1, rect.y)].fg,
            state.config.components.status_input
        );
    }
}

#[test]
fn notification_keyboard_actions_require_explicit_card_focus() {
    let mut state = chrome_state();
    state.receive_notification(
        &ClientEndpointId::Local,
        SemanticNotification {
            kind: SemanticNotificationKind::Custom,
            title: "message".into(),
            body: Some("完整正文".into()),
            sound: None,
            agent: Some("codex".into()),
            workspace_id: None,
            tab_id: None,
            pane_id: Some("pane_1".into()),
            position: None,
        },
        std::time::Instant::now(),
    );
    state.compose(80, 24).expect("card");
    assert!(!state.hits.notification_keys.is_empty());
    state.handle_input_bytes(b"\r");
    assert!(
        state.visible_notification.is_some(),
        "普通 Enter 不跳转/忽略通知"
    );
    state.handle_input_bytes(b"\x1b");
    assert!(state.visible_notification.is_some(), "普通 Esc 不忽略通知");
    let rect = state.hits.notification_keys;
    let click = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: rect.x,
        row: rect.y,
        modifiers: KeyModifiers::NONE,
    };
    state.handle_mouse(click, &mut ClientShellInput::default());
    assert!(state.notification_focused);
    let result = state.handle_input_bytes(b"\r");
    assert!(state.visible_notification.is_none());
    assert!(result.actions.iter().any(|action| matches!(action, ClientShellAction::Endpoint { request, .. } if matches!(&request.method, crate::api::schema::Method::PaneFocus(target) if target.pane_id == "pane_1"))));
    state.receive_notification(
        &ClientEndpointId::Local,
        SemanticNotification {
            kind: SemanticNotificationKind::Custom,
            title: "dismiss me".into(),
            body: None,
            sound: None,
            agent: None,
            workspace_id: None,
            tab_id: None,
            pane_id: None,
            position: None,
        },
        std::time::Instant::now(),
    );
    state.notification_focused = true;
    let dismissed = state.handle_input_bytes(b"\x1b");
    assert!(state.visible_notification.is_none());
    assert!(dismissed.actions.is_empty());
}

#[test]
fn notification_history_rejects_a_target_from_a_previous_boot() {
    let mut state = chrome_state();
    state.record_notification(
        ClientToastLevel::Error,
        "old failure",
        Some("error details".into()),
        Some(ClientNotificationTarget {
            endpoint_id: ClientEndpointId::Local,
            pane_id: Some("pane_1".into()),
            boot_id: Some("previous-boot".into()),
        }),
    );
    state.open_notification_history();
    let mut outcome = ClientShellInput::default();
    state.focus_notification_history_target(&mut outcome);
    assert!(outcome.actions.is_empty());
    assert!(matches!(
        state.overlay,
        Some(ClientShellOverlay::NotificationHistory(_))
    ));
}

#[test]
fn settings_boundary_notification_text_and_paste_release_card_focus() {
    for paste in [false, true] {
        let mut state = chrome_state();
        state.receive_notification(
            &ClientEndpointId::Local,
            SemanticNotification {
                kind: SemanticNotificationKind::Custom,
                title: "input request".into(),
                body: None,
                sound: None,
                agent: None,
                workspace_id: None,
                tab_id: None,
                pane_id: Some("other-pane".into()),
                position: None,
            },
            std::time::Instant::now(),
        );
        state.compose(106, 20).expect("card");
        let keys = state.hits.notification_keys;
        state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: keys.x,
            row: keys.y,
            modifiers: KeyModifiers::NONE,
        })]);
        assert!(state.notification_focused);
        let commit = if paste {
            RawInputEvent::Paste("你好".into())
        } else {
            RawInputEvent::Text(crate::input::TextCommit::new("你好"))
        };
        let outcome = state.handle_raw_events(vec![
            commit,
            RawInputEvent::Key(crate::input::TerminalKey::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )),
        ]);
        assert!(
            outcome.actions.is_empty(),
            "text must not jump: {:?}",
            outcome.actions
        );
        let events = outcome
            .requests
            .iter()
            .flat_map(|request| match request {
                ClientMessage::ClientShellPaneInput { pane_id, events } => {
                    assert_eq!(pane_id, "pane_1");
                    events.clone()
                }
                other => panic!("unexpected input request: {other:?}"),
            })
            .collect::<Vec<_>>();
        assert!(events.iter().any(|event| matches!(event,
            ClientPaneInputEvent::TextCommit(text) | ClientPaneInputEvent::Paste(text) if text == "你好")));
        assert!(events.iter().any(|event| matches!(
            event,
            ClientPaneInputEvent::Key {
                code: crate::protocol::ClientKeyCode::Enter,
                ..
            }
        )));
        assert!(!state.notification_focused);
        assert!(state.visible_notification.is_some());
    }
}

fn toast_workbench_state() -> ClientShellState {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_endpoint_methods(Some(vec!["client.views.set".into(), "tab.focus".into()]));
    state.set_pane_surface(surface());
    state.compose(120, 40).expect("初始画面");
    state.tick_workbench(std::time::Instant::now(), &mut ClientShellInput::default());
    state.workbench.pending = false;
    state.workbench.acknowledged = state.workbench.revision;
    assert!(state.workbench.enabled, "夹具前提：workbench 布局已启用");
    state
}

fn toast_notification(
    kind: SemanticNotificationKind,
    position: crate::config::ToastHerdrPosition,
) -> ClientVisibleNotification {
    ClientVisibleNotification {
        endpoint_id: ClientEndpointId::Local,
        event: SemanticNotification {
            kind,
            title: "codex needs attention".into(),
            body: Some("请确认是否修改 src/client/shell/state.rs，当前终端等待权限输入。".into()),
            sound: None,
            agent: Some("codex".into()),
            workspace_id: None,
            tab_id: None,
            pane_id: Some("pane_1".into()),
            position: Some(position),
        },
        deadline: std::time::Instant::now() + std::time::Duration::from_secs(30),
    }
}

fn toast_notice(kind: ClientEndpointNoticeKind) -> ClientVisibleEndpointNotice {
    ClientVisibleEndpointNotice {
        key: ClientEndpointNoticeKey {
            endpoint_id: ClientEndpointId::Local,
            boot_id: "boot-1".into(),
            kind,
            code: "test".into(),
        },
        title: "工作区切换失败".into(),
        body: "目标工作区已不存在".into(),
        deadline: std::time::Instant::now() + std::time::Duration::from_secs(30),
    }
}

/// 工作台与经典布局在 60x20 / 80x24 / 160x45 下，四个位置的吐司都落在安全区内：
/// 不压标题栏、状态栏 / 模式条；右侧距边 2 格；两张吐司堆叠不重叠，端点通知离角落最近。
#[test]
fn toasts_stay_inside_the_safe_area_and_stack_without_overlap() {
    use crate::config::ToastHerdrPosition as P;
    for workbench in [true, false] {
        for (cols, rows) in [(60, 20), (80, 24), (160, 45)] {
            for position in [P::TopLeft, P::TopRight, P::BottomLeft, P::BottomRight] {
                let mut state = if workbench {
                    toast_workbench_state()
                } else {
                    let mut state = chrome_state();
                    state.config.toast_position = position;
                    state
                };
                state.config.toast_position = position;
                state.visible_notification = Some(toast_notification(
                    SemanticNotificationKind::NeedsAttention,
                    position,
                ));
                state.visible_endpoint_notice =
                    Some(toast_notice(ClientEndpointNoticeKind::Rejected));
                state.compose(cols, rows).expect("toast frame");
                let layout = state.layout(cols, rows);
                let safe = state.feedback_safe_area(layout, cols, rows);
                let semantic = state.hits.notification_toast;
                let notice = state.hits.endpoint_notice_toast;
                let label = format!("workbench={workbench} {cols}x{rows} {position:?}");
                assert!(!semantic.is_empty() && !notice.is_empty(), "{label}");
                for rect in [semantic, notice] {
                    assert_eq!(
                        rect.intersection(safe),
                        rect,
                        "{label}: {rect:?} ⊄ {safe:?}"
                    );
                    if workbench {
                        assert!(rect.y >= 1, "{label}: 压住标题栏");
                        assert!(rect.bottom() < rows, "{label}: 压住状态栏");
                    }
                }
                assert!(!semantic.intersects(notice), "{label}: 两张吐司重叠");
                let mobile = !layout.mobile_header.is_empty();
                if mobile {
                    // 移动端端点通知是贴底横幅，语义通知在它之上。
                    assert_eq!(notice.height, 1, "{label}");
                    continue;
                }
                let corner = match position {
                    P::TopLeft | P::TopRight => notice.y == safe.y && semantic.y == notice.bottom(),
                    P::BottomLeft | P::BottomRight => {
                        notice.bottom() == safe.bottom() && semantic.bottom() == notice.y
                    }
                };
                assert!(corner, "{label}: {notice:?} {semantic:?} in {safe:?}");
                if cols >= 60 {
                    match position {
                        P::TopRight | P::BottomRight => {
                            assert_eq!(semantic.right(), safe.right() - 2, "{label}")
                        }
                        P::TopLeft | P::BottomLeft => assert_eq!(semantic.x, safe.x + 2, "{label}"),
                    }
                }
            }
        }
    }
}

/// 吐司按等级描边：需要输入琥珀、被拒绝红；动作命中区就是画出的「按键」键帽。
#[test]
fn toast_borders_use_level_colors_and_the_keys_hit_matches_the_paint() {
    let mut state = toast_workbench_state();
    state.visible_notification = Some(toast_notification(
        SemanticNotificationKind::NeedsAttention,
        crate::config::ToastHerdrPosition::BottomRight,
    ));
    state.visible_endpoint_notice = Some(toast_notice(ClientEndpointNoticeKind::Rejected));
    let frame = state.compose(120, 40).expect("toast frame");
    let buffer = frame.to_ratatui_buffer().expect("toast buffer");
    let semantic = state.hits.notification_toast;
    let notice = state.hits.endpoint_notice_toast;
    let components = &state.config.components;
    assert_eq!(buffer[(semantic.x, semantic.y)].symbol(), "╭");
    assert_eq!(buffer[(semantic.x, semantic.y)].fg, components.status_input);
    assert_eq!(
        buffer[(notice.x, notice.y)].fg,
        components.toast_border_error
    );
    assert_eq!(buffer[(notice.x + 2, notice.y + 1)].symbol(), "✕");
    let keys = state.hits.notification_keys;
    assert!(!keys.is_empty());
    assert_eq!(keys.intersection(semantic), keys);
    assert_eq!(keys.y, semantic.bottom() - 2);
    assert_eq!(buffer[(keys.x, keys.y)].bg, state.config.palette.surface1);
    let text: String = (keys.x..keys.right())
        .map(|x| buffer[(x, keys.y)].symbol())
        .collect();
    assert!(text.contains("keys") || text.contains("按"), "{text}");
    // 点「按键」把键盘焦点交给卡片；聚焦后末行换成 enter / esc，边框仍是琥珀色。
    state.handle_mouse(
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: keys.x,
            row: keys.y,
            modifiers: KeyModifiers::NONE,
        },
        &mut ClientShellInput::default(),
    );
    assert!(state.notification_focused);
    let frame = state.compose(120, 40).expect("focused frame");
    let buffer = frame.to_ratatui_buffer().expect("focused buffer");
    let semantic = state.hits.notification_toast;
    assert_eq!(
        buffer[(semantic.x, semantic.y)].fg,
        components_status_input(&state)
    );
    let footer: String = (semantic.x..semantic.right())
        .map(|x| buffer[(x, semantic.bottom() - 2)].symbol())
        .collect();
    assert!(
        footer.contains("enter") && footer.contains("esc"),
        "{footer}"
    );
    assert!(state.hits.notification_keys.is_empty());
    // 点端点通知只关掉它，不动语义通知。
    state.handle_mouse(
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: notice.x + 1,
            row: notice.y + 1,
            modifiers: KeyModifiers::NONE,
        },
        &mut ClientShellInput::default(),
    );
    assert!(state.visible_endpoint_notice.is_none());
    assert!(state.visible_notification.is_some());
}

fn components_status_input(state: &ClientShellState) -> ratatui::style::Color {
    state.config.components.status_input
}

#[test]
fn toast_pointer_and_focused_keys_take_precedence_over_settings() {
    let mut state = toast_workbench_state();
    state.open_settings_overlay();
    state.visible_notification = Some(toast_notification(
        SemanticNotificationKind::NeedsAttention,
        crate::config::ToastHerdrPosition::BottomRight,
    ));
    state.compose(80, 24).expect("settings and notification");
    let keys = state.hits.notification_keys;
    assert!(!keys.is_empty());
    // The settings control behind the card must not keep hover or receive wheel input.
    let before = match state.overlay.as_ref() {
        Some(ClientShellOverlay::Settings(settings)) => (settings.selected, settings.scroll),
        _ => panic!("settings"),
    };
    state.hover = Some(ChromeHover::SettingsChoice(before.0));
    for kind in [MouseEventKind::Moved, MouseEventKind::ScrollDown] {
        state.handle_mouse(
            MouseEvent {
                kind,
                column: keys.x,
                row: keys.y,
                modifiers: KeyModifiers::NONE,
            },
            &mut ClientShellInput::default(),
        );
    }
    assert_eq!(state.hover, Some(ChromeHover::NotificationToast));
    state.handle_mouse(
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: keys.x,
            row: keys.y,
            modifiers: KeyModifiers::NONE,
        },
        &mut ClientShellInput::default(),
    );
    assert!(state.notification_focused);
    let Some(ClientShellOverlay::Settings(settings)) = state.overlay.as_ref() else {
        panic!("notification click must not close settings");
    };
    assert_eq!((settings.selected, settings.scroll), before);
    state.handle_raw_events(vec![crate::raw_input::RawInputEvent::Key(
        crate::input::TerminalKey::new(KeyCode::Esc, KeyModifiers::NONE),
    )]);
    assert!(state.visible_notification.is_none());
    assert!(!state.notification_focused);
    assert!(matches!(
        state.overlay,
        Some(ClientShellOverlay::Settings(_))
    ));
}

#[test]
fn different_toast_anchors_do_not_overlap_in_narrow_layouts() {
    use crate::config::ToastHerdrPosition as P;
    let positions = [P::TopLeft, P::TopRight, P::BottomLeft, P::BottomRight];
    for workbench in [true, false] {
        for (cols, rows) in [(60, 20), (80, 24), (160, 45)] {
            for endpoint_position in positions {
                for semantic_position in positions {
                    let mut state = if workbench {
                        toast_workbench_state()
                    } else {
                        chrome_state()
                    };
                    state.config.toast_position = endpoint_position;
                    state.visible_endpoint_notice =
                        Some(toast_notice(ClientEndpointNoticeKind::Rejected));
                    state.visible_notification = Some(toast_notification(
                        SemanticNotificationKind::NeedsAttention,
                        semantic_position,
                    ));
                    state.compose(cols, rows).expect("mixed anchors");
                    let a = state.hits.endpoint_notice_toast;
                    let b = state.hits.notification_toast;
                    assert!(!a.is_empty() && !b.is_empty());
                    assert!(
                        !a.intersects(b),
                        "{cols}x{rows}: {endpoint_position:?} {semantic_position:?}: {a:?} {b:?}"
                    );
                }
            }
        }
    }
}

#[test]
fn toast_does_not_capture_a_drag_release_or_keep_hidden_keyboard_focus() {
    let mut state = toast_workbench_state();
    state.visible_notification = Some(toast_notification(
        SemanticNotificationKind::NeedsAttention,
        crate::config::ToastHerdrPosition::BottomRight,
    ));
    state.compose(80, 24).expect("notification");
    let rect = state.hits.notification_toast;
    state.chrome_drag = Some(ClientChromeDrag::SidebarWidth);
    state.handle_mouse(
        MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: rect.x + 1,
            row: rect.y + 1,
            modifiers: KeyModifiers::NONE,
        },
        &mut ClientShellInput::default(),
    );
    assert!(
        state.chrome_drag.is_none(),
        "release belongs to the captured gesture"
    );

    state.notification_focused = true;
    state.visible_endpoint_notice = Some(toast_notice(ClientEndpointNoticeKind::Success));
    state.compose(80, 10).expect("too small for two cards");
    assert!(state.hits.notification_toast.is_empty());
    assert!(
        state.visible_notification.is_some(),
        "hidden notification is retained"
    );
    assert!(
        !state.notification_focused,
        "invisible card cannot consume Enter/Esc"
    );
    assert!(!state.route_notification_key(
        &crate::input::TerminalKey::new(KeyCode::Enter, KeyModifiers::NONE),
        &mut ClientShellInput::default(),
    ));
}

#[test]
fn noninteractive_notification_blocks_hover_on_underlying_settings() {
    let mut state = toast_workbench_state();
    state.open_settings_overlay();
    let mut notice = toast_notification(
        SemanticNotificationKind::UpdateInstalled,
        crate::config::ToastHerdrPosition::BottomRight,
    );
    notice.event.pane_id = None;
    state.visible_notification = Some(notice);
    state
        .compose(80, 24)
        .expect("noninteractive notice over settings");
    let rect = state.hits.notification_toast;
    state.hits.settings_choices.push((rect, 0));
    state.hover = Some(ChromeHover::SettingsChoice(0));
    state.handle_mouse(
        moved_mouse(rect.x + 1, rect.y + 1),
        &mut ClientShellInput::default(),
    );
    assert!(state.hover.is_none());
}

/// 复制反馈限定在安全区内，与通知吐司同处底部时让开不重叠。
#[test]
fn copy_feedback_stays_in_the_safe_area_and_avoids_toasts() {
    let mut state = toast_workbench_state();
    state.config.clipboard_toast_position = crate::config::ToastClipboardPosition::BottomRight;
    state.visible_notification = Some(toast_notification(
        SemanticNotificationKind::Finished,
        crate::config::ToastHerdrPosition::BottomRight,
    ));
    state.copy_feedback = Some(crate::app::state::CopyFeedback {
        message: "已复制到剪贴板".into(),
    });
    let frame = state.compose(80, 24).expect("copy frame");
    let buffer = frame.to_ratatui_buffer().expect("copy buffer");
    let toast = state.hits.notification_toast;
    let marker = (0..24u16)
        .flat_map(|y| (0..80u16).map(move |x| (x, y)))
        .find(|&(x, y)| buffer[(x, y)].symbol() == "✓" && !toast.contains((x, y).into()))
        .expect("copy feedback marker");
    let safe = state.feedback_safe_area(state.layout(80, 24), 80, 24);
    assert!(safe.contains(marker.into()));
    assert!(marker.1 < toast.y, "复制反馈让到通知之上");
    assert_eq!(
        buffer[(marker.0 - 2, marker.1 - 1)].fg,
        state.config.components.toast_border_success
    );
}
