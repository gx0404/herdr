use super::*;
use crate::input::{KeybindAction, KeybindMatch};

#[test]
fn command_palette_fuzzy_filters_and_activates_with_highlight_data() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    state.toggle_global_menu();
    let item_count = palette_overlay(&state).items.len();
    assert!(item_count > 30, "all actions are indexed: {item_count}");

    assert!(state.insert_overlay_text("设置"));
    let rows = super::command_palette::palette_rows(palette_overlay(&state));
    let ids: Vec<&str> = rows.iter().map(|row| row.item.id.as_str()).collect();
    assert_eq!(rows.len(), 1, "fuzzy filter narrows to settings: {ids:?}");
    assert_eq!(rows[0].item.id, "binding:Settings");
    assert!(
        !rows[0].match_indices.is_empty(),
        "matched characters carry highlight positions"
    );

    let open = state.handle_input_bytes(b"\r");
    assert!(open.repaint);
    assert!(
        matches!(state.overlay, Some(ClientShellOverlay::Settings(_))),
        "enter runs the selected action"
    );
}

#[test]
fn command_palette_recent_commands_lead_and_persist() {
    let path =
        std::env::temp_dir().join(format!("herdr-palette-recent-{}.json", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let config =
        ClientShellConfig::from_config(&Config::default()).with_preferences_path(path.clone());
    let mut state = ClientShellState::new(config);
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());

    state.toggle_global_menu();
    let help_index = palette_row_index(&state, "binding:Help");
    state.activate_palette_item(help_index, &mut ClientShellInput::default());
    assert!(matches!(state.overlay, Some(ClientShellOverlay::Help(_))));
    state.overlay = None;

    state.toggle_global_menu();
    let rows = super::command_palette::palette_rows(palette_overlay(&state));
    assert_eq!(rows[0].item.id, "binding:Help", "most recent leads");
    assert!(rows[0].recent);
    assert!(!rows[1].recent);

    palette_select(&mut state, "binding:Settings");
    state.handle_input_bytes(b"\r");
    assert!(matches!(
        state.overlay,
        Some(ClientShellOverlay::Settings(_))
    ));
    state.overlay = None;

    // Persisted atomically to the client state file; a fresh client restores it.
    let saved = std::fs::read_to_string(&path).expect("palette recents persisted");
    assert!(saved.contains("binding:Settings"), "saved: {saved}");
    let restored = ClientShellState::new(
        ClientShellConfig::from_config(&Config::default()).with_preferences_path(path.clone()),
    );
    assert_eq!(restored.palette_recent[0], "binding:Settings");
    assert_eq!(restored.palette_recent[1], "binding:Help");
    std::fs::remove_file(path).expect("remove palette state");

    let mut restored = restored;
    restored.set_snapshot(Box::new(snapshot()));
    restored.set_pane_surface(surface());
    restored.toggle_global_menu();
    let rows = super::command_palette::palette_rows(palette_overlay(&restored));
    assert_eq!(rows[0].item.id, "binding:Settings");
    assert_eq!(rows[1].item.id, "binding:Help");
}

#[test]
fn command_palette_lists_machine_actions_and_runs_them() {
    let hex = "ab".repeat(16);
    let profile = SavedSshEndpoint {
        id: crate::client::endpoint::ProfileId::parse(&hex).expect("hex profile id"),
        label: "prod".into(),
        target: "prod@example.com".into(),
        ..SavedSshEndpoint::new("prod", "prod@example.com", "default").expect("valid profile")
    };
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_endpoint_catalog(&[profile]);
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());

    state.toggle_global_menu();
    assert!(state.insert_overlay_text("prod"));
    let rows = super::command_palette::palette_rows(palette_overlay(&state));
    let ids: Vec<&str> = rows.iter().map(|row| row.item.id.as_str()).collect();
    assert!(
        ids.contains(&"machine:connect:abababababababababababababababab"),
        "{ids:?}"
    );
    assert!(
        ids.contains(&"machine:edit:abababababababababababababababab"),
        "{ids:?}"
    );
    assert!(
        ids.contains(&"machine:toggle:abababababababababababababababab"),
        "{ids:?}"
    );

    let connect_index =
        palette_row_index(&state, "machine:connect:abababababababababababababababab");
    let outcome = {
        let mut outcome = ClientShellInput::default();
        state.activate_palette_item(connect_index, &mut outcome);
        outcome
    };
    assert!(
        outcome.actions.iter().any(|action| matches!(
            action,
            ClientShellAction::ReconnectEndpoint { endpoint_id }
                if *endpoint_id == ClientEndpointId::Ssh(
                    crate::client::endpoint::ProfileId::parse(&hex).expect("id")
                )
        )),
        "connect dispatches a reconnect"
    );
    assert!(state.overlay.is_none());
}

#[test]
fn command_palette_mouse_click_runs_the_row() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    state.compose(106, 30).expect("shell frame");
    state.toggle_global_menu();
    state.compose(106, 30).expect("palette frame");
    let help_index = palette_row_index(&state, "binding:Help");
    let (row, _) = state
        .hits
        .global_menu_rows
        .iter()
        .find(|(_, index)| *index == help_index)
        .expect("help row hit");
    let click = state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: row.x,
        row: row.y,
        modifiers: KeyModifiers::empty(),
    })]);
    assert!(click.repaint);
    assert!(matches!(state.overlay, Some(ClientShellOverlay::Help(_))));
}

fn hints_state(lines: &[&'static str]) -> ClientShellState {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    let mut pane_surface = surface();
    let buffer = Buffer::with_lines(lines.iter().copied());
    pane_surface.frame = FrameData::from_ratatui_buffer_with_hyperlinks(&buffer, None, &[]);
    let width = lines.iter().map(|line| line.len()).max().unwrap_or(1) as u16;
    let height = lines.len() as u16;
    pane_surface.panes[0].rect.width = width;
    pane_surface.panes[0].rect.height = height;
    pane_surface.panes[0].inner_rect = pane_surface.panes[0].rect;
    state.set_pane_surface(pane_surface);
    state.compose(106, 20).expect("composed frame");
    state
}

#[test]
fn link_hints_mark_visible_urls_and_open_the_typed_marker() {
    let mut state = hints_state(&["see https://example.com/docs here", "second row"]);
    let mut outcome = ClientShellInput::default();
    state.record_binding(KeybindMatch::Action(KeybindAction::LinkHints), &mut outcome);
    assert!(state.link_hints.is_some(), "hints mode entered");
    assert_eq!(state.link_hints.as_ref().map(|h| h.hints.len()), Some(1));

    let frame = state.compose(106, 20).expect("hints frame");
    let hint = &state.link_hints.as_ref().expect("hints").hints[0];
    let pane = state.hits.panes[0].clone();
    let x = pane.inner_rect.x + hint.col;
    let y = pane.inner_rect.y + hint.row;
    assert_eq!((hint.col, hint.row), (4, 0), "marker sits at the URL start");
    let buffer = frame.to_ratatui_buffer().expect("ratatui buffer");
    assert_eq!(buffer[(x, y)].symbol(), "a");
    assert_eq!(buffer[(x + 1, y)].symbol(), "a");
    assert_eq!(buffer[(x, y)].bg, state.config.palette.accent);

    // Typing the two-letter marker opens the URL and exits hints mode.
    let first = state.handle_input_bytes(b"a");
    assert!(first.actions.is_empty(), "partial marker waits");
    assert!(state.link_hints.is_some());
    let second = state.handle_input_bytes(b"a");
    assert!(
        matches!(&second.actions[..], [ClientShellAction::OpenSafeWebUrl(url)]
            if url == "https://example.com/docs"),
        "typed marker opens the URL: {:?}",
        second.actions
    );
    assert!(state.link_hints.is_none());
}

#[test]
fn link_hints_esc_exits_and_empty_viewports_report_no_links() {
    let mut state = hints_state(&["see https://example.com/docs here"]);
    let mut outcome = ClientShellInput::default();
    state.record_binding(KeybindMatch::Action(KeybindAction::LinkHints), &mut outcome);
    assert!(state.link_hints.is_some());
    state.handle_input_bytes(b"\x1b");
    assert!(state.link_hints.is_none(), "esc leaves hints mode");

    let mut empty = hints_state(&["no links here", "just text"]);
    let mut outcome = ClientShellInput::default();
    empty.record_binding(KeybindMatch::Action(KeybindAction::LinkHints), &mut outcome);
    assert!(
        empty.link_hints.is_none(),
        "no links: hints mode not entered"
    );
    assert!(
        empty.copy_feedback.is_some(),
        "no links: transient feedback is shown"
    );
}

#[test]
fn which_key_popup_renders_in_prefix_mode_and_respects_the_toggle() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    state.mode = ClientShellMode::Prefix;
    let frame = state.compose(106, 30).expect("prefix frame");
    let text = frame
        .cells
        .iter()
        .map(|cell| cell.symbol.as_str())
        .collect::<String>()
        .replace(' ', "");
    assert!(
        text.contains("分离"),
        "which-key lists prefix actions beyond the mode bar hints"
    );

    let mut off = Config::default();
    off.ui.which_key = false;
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&off));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    state.mode = ClientShellMode::Prefix;
    let frame = state
        .compose(106, 30)
        .expect("prefix frame without which-key");
    let text = frame
        .cells
        .iter()
        .map(|cell| cell.symbol.as_str())
        .collect::<String>()
        .replace(' ', "");
    assert!(
        !text.contains("分离"),
        "ui.which_key = false hides the popup"
    );
}

#[test]
fn unhandled_binding_surfaces_a_notice_instead_of_dropping_silently() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    state.compose(106, 20).expect("composed frame");
    let mut outcome = ClientShellInput::default();
    // No previous pane exists, so LastPane has no target.
    state.record_binding(KeybindMatch::Action(KeybindAction::LastPane), &mut outcome);
    assert!(outcome.repaint);
    let notice = state
        .visible_endpoint_notice
        .as_ref()
        .expect("notice shown for untargetable action");
    assert_eq!(
        notice.title,
        crate::i18n::texts().endpoint.notice_action_unavailable
    );
}

#[test]
fn triple_click_selects_the_whole_line_and_copies() {
    let mut state = {
        let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
        state.set_snapshot(Box::new(snapshot()));
        let mut pane_surface = surface();
        let buffer = Buffer::with_lines(["alpha bravo charlie", "delta echo foxtrot  "]);
        pane_surface.frame = FrameData::from_ratatui_buffer_with_hyperlinks(&buffer, None, &[]);
        pane_surface.panes[0].rect.width = 19;
        pane_surface.panes[0].rect.height = 2;
        pane_surface.panes[0].inner_rect = pane_surface.panes[0].rect;
        state.set_pane_surface(pane_surface);
        state.compose(106, 20).expect("composed frame");
        state
    };
    let click = |state: &mut ClientShellState, kind: MouseEventKind| {
        let pane = state.hits.panes[0].clone();
        state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
            kind,
            column: pane.inner_rect.x + 8,
            row: pane.inner_rect.y,
            modifiers: KeyModifiers::empty(),
        })])
    };
    click(&mut state, MouseEventKind::Down(MouseButton::Left));
    click(&mut state, MouseEventKind::Up(MouseButton::Left));
    click(&mut state, MouseEventKind::Down(MouseButton::Left));
    click(&mut state, MouseEventKind::Up(MouseButton::Left));
    assert!(
        state.word_selection_gesture.is_some(),
        "double click selects a word"
    );
    let third = click(&mut state, MouseEventKind::Down(MouseButton::Left));
    assert!(
        state.word_selection_gesture.is_none(),
        "triple click leaves word mode"
    );
    let selection = state.selection.as_ref().expect("line selection");
    assert!(selection.is_finalized());
    assert_eq!(
        selection.ordered_cells(),
        ((0, 0), (0, 18)),
        "whole line selected to the last non-blank cell"
    );
    assert!(
        third.actions.iter().any(|action| matches!(action,
            ClientShellAction::Endpoint { request, .. }
                if matches!(&request.method, crate::api::schema::Method::PaneSelectionRead(params)
                    if params.anchor.col == 0 && params.cursor.col == 18))),
        "copy_on_select copies the line"
    );
    // The fourth click starts a fresh streak (plain anchor, no line copy).
    click(&mut state, MouseEventKind::Up(MouseButton::Left));
    let fourth = click(&mut state, MouseEventKind::Down(MouseButton::Left));
    assert!(
        !fourth.actions.iter().any(|action| matches!(action,
            ClientShellAction::Endpoint { request, .. }
                if matches!(&request.method, crate::api::schema::Method::PaneSelectionRead(_)))),
        "fourth click does not read a selection"
    );
    assert!(
        state
            .selection
            .as_ref()
            .is_some_and(|selection| selection.is_in_progress()),
        "fourth click is a fresh drag anchor"
    );
}
