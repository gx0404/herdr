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

    // Mouse hover lands directly on a row and moves the selection.
    let (row_rect, row_index) = state.hits.notification_history_rows[0];
    let mut outcome = ClientShellInput::default();
    state.handle_mouse(moved_mouse(row_rect.x, row_rect.y), &mut outcome);
    assert!(outcome.repaint);
    let Some(ClientShellOverlay::NotificationHistory(overlay)) = state.overlay.as_ref() else {
        panic!("history overlay");
    };
    assert_eq!(overlay.selected, row_index);
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
    let now = std::time::Instant::now();
    let ago = |duration: std::time::Duration| {
        super::super::feedback::relative_time_ago(now - duration, now)
    };
    assert_eq!(ago(std::time::Duration::from_secs(5)), "just now");
    assert_eq!(ago(std::time::Duration::from_secs(5 * 60)), "5m ago");
    assert_eq!(ago(std::time::Duration::from_secs(3 * 3600)), "3h ago");
    assert_eq!(ago(std::time::Duration::from_secs(2 * 86_400)), "2d ago");
}
