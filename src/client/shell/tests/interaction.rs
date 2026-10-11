use super::*;
use crate::input::{KeybindAction, KeybindMatch};

#[test]
fn main_menu_and_search_have_distinct_shortcuts_and_restore_the_page() {
    use super::super::command_palette::BrowserView;
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    state.handle_input_bytes(b"\x02 ");
    assert_eq!(palette_overlay(&state).view, BrowserView::Menu(None));
    assert_eq!(
        super::command_palette::palette_rows(palette_overlay(&state)).len(),
        8
    );
    let group = palette_row_index(&state, "category:2");
    state.activate_palette_item(group, &mut ClientShellInput::default());
    assert_eq!(palette_overlay(&state).view, BrowserView::Menu(Some(2)));
    state.handle_input_bytes(b"\x1b");
    assert_eq!(palette_overlay(&state).view, BrowserView::Menu(None));
    state.handle_input_bytes(b"\x1b");
    state.handle_input_bytes(b"\x02/");
    assert_eq!(palette_overlay(&state).view, BrowserView::Search);
    state.close_command_browser();
    state.open_settings_overlay();
    state.move_settings_selection(1);
    let selected = match state.overlay.as_ref().unwrap() {
        ClientShellOverlay::Settings(settings) => settings.selected,
        _ => unreachable!(),
    };
    state.open_command_search();
    state.handle_input_bytes(b"\x1b");
    assert!(
        matches!(state.overlay, Some(ClientShellOverlay::Settings(ref settings)) if settings.selected == selected)
    );
}

#[test]
fn search_click_and_wheel_preserve_the_browser_and_unicode_matches() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    state.open_command_search();
    state.compose(80, 24).unwrap();
    let search = state.hits.menu_search;
    state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: search.x + 4,
        row: search.y,
        modifiers: KeyModifiers::NONE,
    })]);
    assert!(matches!(
        state.overlay,
        Some(ClientShellOverlay::CommandPalette(_))
    ));
    state.scroll_palette(6);
    state.compose(80, 24).unwrap();
    assert_eq!(palette_overlay(&state).scroll, 6);
    state.move_palette_selection(1);
    state.compose(80, 24).unwrap();
    let palette = palette_overlay(&state);
    let rows = super::command_palette::palette_rows(palette);
    let selected_line = super::super::chrome::command::visual(&rows)
        .iter()
        .position(|(index, _)| *index == Some(palette.selected))
        .expect("selected visual row");
    assert!(palette.scroll <= selected_line);
    assert!(state
        .hits
        .global_menu_rows
        .iter()
        .any(|(_, index)| *index == palette_overlay(&state).selected));
    // Unicode 下标：`İ` 的小写展开成 "i" + 组合点两个字符，命中下标仍按原文
    // 字符计。夹具保持 "İİB" 不变；冒烟 L6 起首字符必须落在词首，B 与前面
    // 的 İ 同属一个全大写词、不是词首，单查 "b" 从词中间起步不再命中（L6
    // 要去掉的正是这种命中）。改为从词首连续穿过两个展开字符，下标去重后
    // 仍是每个原文字符一个；词首上的 B 照样按原文下标报告。
    assert!(super::command_palette::fuzzy_match("b", "İİB").is_none());
    let (_, indices) = super::command_palette::fuzzy_match("İİb", "İİB").unwrap();
    assert_eq!(indices, vec![0, 1, 2]);
    let (_, indices) = super::command_palette::fuzzy_match("b", "İİ B").unwrap();
    assert_eq!(indices, vec![3]);
    assert!(state.insert_overlay_text("settings"));
    let rows = super::command_palette::palette_rows(palette_overlay(&state));
    assert!(rows.iter().any(|row| row.item.id == "binding:Settings"));
}

#[test]
fn offline_pages_and_narrow_settings_keep_every_category_accessible() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.open_command_search();
    state.compose(80, 24).unwrap();
    assert!(!state.hits.menu_popup.is_empty());
    state.close_command_browser();
    state.open_settings_overlay();
    for size in [(60, 16), (80, 24), (120, 40), (160, 50)] {
        state.compose(size.0, size.1).unwrap();
        assert_eq!(
            state.hits.settings_tabs.len(),
            ClientSettingsSection::ALL.len()
        );
        for (rect, _) in &state.hits.settings_tabs {
            assert!(!rect.is_empty());
            assert!(state.hits.settings_popup.contains((rect.x, rect.y).into()));
            assert!(rect.bottom() <= state.hits.settings_popup.bottom());
        }
    }
}

#[test]
fn command_palette_fuzzy_filters_and_activates_with_highlight_data() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    state.open_command_search();
    let item_count = palette_overlay(&state).items.len();
    assert!(item_count > 30, "all actions are indexed: {item_count}");

    // 监控单入口写出它的两个页签，按页签名（第二个是「偏好」）也能搜到。
    assert!(state.insert_overlay_text("偏好"));
    let rows = super::command_palette::palette_rows(palette_overlay(&state));
    let ids: Vec<&str> = rows.iter().map(|row| row.item.id.as_str()).collect();
    assert!(
        ids.contains(&"observation:monitor"),
        "监控单入口包含偏好页签：{ids:?}"
    );

    state.open_command_search();
    assert!(state.insert_overlay_text("设置"));
    let rows = super::command_palette::palette_rows(palette_overlay(&state));
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

    state.open_command_search();
    let help_index = palette_row_index(&state, "binding:Help");
    state.activate_palette_item(help_index, &mut ClientShellInput::default());
    assert!(matches!(state.overlay, Some(ClientShellOverlay::Help(_))));
    state.overlay = None;

    state.open_command_search();
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

/// 已移除的命令面板项（浮动用量仪表盘）可能残留在旧偏好的最近列表里：找不到
/// 对应条目就跳过，不 panic、不占位，其余最近项照常领头。
#[test]
fn command_palette_ignores_recent_ids_of_removed_entries() {
    let path = std::env::temp_dir().join(format!(
        "herdr-palette-recent-removed-{}.json",
        std::process::id()
    ));
    std::fs::write(
        &path,
        r#"{"palette_recent":["observation:usage-dashboard","binding:Help"]}"#,
    )
    .expect("write preferences");
    let mut state = ClientShellState::new(
        ClientShellConfig::from_config(&Config::default()).with_preferences_path(path.clone()),
    );
    let _ = std::fs::remove_file(&path);
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    assert_eq!(state.palette_recent[0], "observation:usage-dashboard");

    state.open_command_search();
    let rows = super::command_palette::palette_rows(palette_overlay(&state));
    assert!(
        rows.iter()
            .all(|row| row.item.id != "observation:usage-dashboard"),
        "已移除的条目不再出现"
    );
    assert_eq!(rows[0].item.id, "binding:Help", "其余最近项照常领头");
    assert!(rows[0].recent);
    assert!(!rows[1].recent, "残留 id 不占最近项的位置");
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

    state.open_command_search();
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
    state.open_command_search();
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

/// HERDR-UX-007：1003 模式下终端全程上报指针移动，旧实现把任何 Moved 都当成
/// 「退出 link hints」，手指还没离开触控板、桌面震一下 hints 就消失了。只有
/// 真正的按下/拖拽/滚轮才结束这个键盘驱动的模式。
#[test]
fn link_hints_survive_pointer_motion_and_end_on_a_press() {
    let mut state = hints_state(&["see https://example.com/docs here", "second row"]);
    let mut outcome = ClientShellInput::default();
    state.record_binding(KeybindMatch::Action(KeybindAction::LinkHints), &mut outcome);
    assert!(state.link_hints.is_some(), "hints mode entered");
    let pane = state.hits.panes[0].clone();

    for column in 0..3u16 {
        state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Moved,
            column: pane.inner_rect.x + column,
            row: pane.inner_rect.y,
            modifiers: KeyModifiers::empty(),
        })]);
        assert!(state.link_hints.is_some(), "移动不取消 hints");
    }
    // 移动过后仍然可以把两字母 marker 敲完。
    state.handle_input_bytes(b"a");
    let opened = state.handle_input_bytes(b"a");
    assert!(
        matches!(&opened.actions[..], [ClientShellAction::OpenSafeWebUrl(url)]
            if url == "https://example.com/docs"),
        "移动之后 marker 仍然有效：{:?}",
        opened.actions
    );

    // 按下是明确的指针动作：hints 退出。
    let mut state = hints_state(&["see https://example.com/docs here", "second row"]);
    let mut outcome = ClientShellInput::default();
    state.record_binding(KeybindMatch::Action(KeybindAction::LinkHints), &mut outcome);
    let pane = state.hits.panes[0].clone();
    state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: pane.inner_rect.x,
        row: pane.inner_rect.y,
        modifiers: KeyModifiers::empty(),
    })]);
    assert!(state.link_hints.is_none(), "按下结束 hints 模式");

    // 滚轮同样结束。
    let mut state = hints_state(&["see https://example.com/docs here", "second row"]);
    let mut outcome = ClientShellInput::default();
    state.record_binding(KeybindMatch::Action(KeybindAction::LinkHints), &mut outcome);
    let pane = state.hits.panes[0].clone();
    state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::ScrollUp,
        column: pane.inner_rect.x,
        row: pane.inner_rect.y,
        modifiers: KeyModifiers::empty(),
    })]);
    assert!(state.link_hints.is_none(), "滚轮结束 hints 模式");

    // 拖拽与抬起也都是明确的指针动作：取消集合是「除 Moved 外全部」，
    // 三语文档写的「点击、拖动或滚轮」必须逐个有断言兜底（C-23）。
    for kind in [
        MouseEventKind::Drag(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        let mut state = hints_state(&["see https://example.com/docs here", "second row"]);
        let mut outcome = ClientShellInput::default();
        state.record_binding(KeybindMatch::Action(KeybindAction::LinkHints), &mut outcome);
        let pane = state.hits.panes[0].clone();
        state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
            kind,
            column: pane.inner_rect.x,
            row: pane.inner_rect.y,
            modifiers: KeyModifiers::empty(),
        })]);
        assert!(state.link_hints.is_none(), "拖拽 / 抬起结束 hints 模式");
    }
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

#[test]
fn settings_boundary_palette_dynamic_targets_revalidate_boot_and_identity() {
    for agent_target in [false, true] {
        for transition in ["restart", "offline", "removed", "valid"] {
            let mut state =
                ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
            state.set_snapshot(Box::new(snapshot()));
            state.set_pane_surface(surface());
            let profile =
                SavedSshEndpoint::new("Remote", "dev@example.com", "default").expect("profile");
            let endpoint = ClientEndpointId::Ssh(profile.id.clone());
            state.set_endpoint_catalog(&[profile]);
            state.set_endpoint_status(&endpoint, ClientEndpointStatus::Online);
            let mut remote = snapshot();
            remote.agents.push(crate::protocol::ClientShellAgent {
                pane_id: "pane_1".into(),
                workspace_id: "ws_1".into(),
                tab_id: "tab_1".into(),
                name: Some("codex".into()),
                display_agent: None,
                agent: Some("codex".into()),
                title: None,
                terminal_title: None,
                terminal_title_stripped: None,
                agent_status: crate::api::schema::AgentStatus::Idle,
                state_change_seq: 1,
                state_labels: Vec::new(),
                tokens: Vec::new(),
                focused: false,
                launch_seq: 7,
                activity: Default::default(),
                insight: Default::default(),
            });
            state.set_endpoint_snapshot(&endpoint, Box::new(remote.clone()));
            state.open_command_search();
            let prefix = if agent_target {
                "target:agent:"
            } else {
                "target:workspace:"
            };
            let rows = super::command_palette::palette_rows(palette_overlay(&state));
            let index = rows
                .iter()
                .position(|row| {
                    row.item.id.starts_with(prefix) && row.item.id.contains(&endpoint.storage_key())
                })
                .expect("remote target");
            let id = rows[index].item.id.clone();
            match transition {
                "restart" => {
                    remote.boot_id = "boot-2".into();
                    state.set_endpoint_snapshot(&endpoint, Box::new(remote));
                }
                "offline" => {
                    state.set_endpoint_status(&endpoint, ClientEndpointStatus::Reconnecting)
                }
                "removed" => {
                    remote.revision += 1;
                    remote.agents.clear();
                    remote.panes.clear();
                    remote.workspaces.clear();
                    state.set_endpoint_snapshot(&endpoint, Box::new(remote));
                }
                _ => {}
            }
            let mut outcome = ClientShellInput::default();
            state.activate_palette_item(index, &mut outcome);
            if transition == "valid" {
                assert!(outcome.actions.iter().any(|action| matches!(action,
                    ClientShellAction::ActivateEndpoint { endpoint_id, .. } if endpoint_id == &endpoint)));
                assert_eq!(state.palette_recent.first(), Some(&id));
            } else {
                assert!(
                    outcome.actions.is_empty(),
                    "{agent_target} {transition}: {:?}",
                    outcome.actions
                );
                assert!(!state.palette_recent.contains(&id));
                assert!(matches!(
                    state.overlay,
                    Some(ClientShellOverlay::CommandPalette(_))
                ));
            }
        }
    }
}

fn mark_unread_snapshot() -> ClientShellSnapshot {
    let mut projected = snapshot();
    projected.agents.push(ClientShellAgent {
        pane_id: "pane_1".into(),
        workspace_id: "ws_1".into(),
        tab_id: "tab_1".into(),
        name: Some("kimi".into()),
        display_agent: None,
        agent: Some("kimi".into()),
        title: None,
        terminal_title: None,
        terminal_title_stripped: None,
        agent_status: AgentStatus::Idle,
        state_change_seq: 1,
        state_labels: Vec::new(),
        tokens: Vec::new(),
        focused: true,
        launch_seq: 7,
        activity: Default::default(),
        insight: Default::default(),
    });
    projected
}

fn fill_other_agent_marks(state: &mut ClientShellState, projected: &mut ClientShellSnapshot) {
    let template = projected.agents[0].clone();
    for index in 0..256 {
        let mut agent = template.clone();
        agent.pane_id = format!("capacity-{index}");
        agent.focused = false;
        projected.agents.push(agent);
    }
    projected.revision += 1;
    state.set_snapshot(Box::new(projected.clone()));
    for agent in projected.agents.iter().skip(1) {
        let target = super::super::chrome::inspector::Target::for_agent(
            ClientEndpointId::Local,
            projected,
            agent,
        );
        assert!(state.mark_agent_read(&target, false));
    }
}

#[test]
fn mark_unread_catalog_and_search_disable_unavailable_focused_targets() {
    use super::super::action_table::{global_action_state, ActionId};
    for condition in [
        "shell", "working", "blocked", "failed", "offline", "full", "idle", "review",
    ] {
        let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
        let mut projected = mark_unread_snapshot();
        match condition {
            "shell" => projected.agents.clear(),
            "working" => projected.agents[0].agent_status = AgentStatus::Working,
            "blocked" => projected.agents[0].agent_status = AgentStatus::Blocked,
            "failed" => {
                projected.agents[0].insight.attention =
                    Some(crate::protocol::ClientShellAgentAttention {
                        kind: crate::api::schema::AgentAttentionKind::Failed,
                        ..Default::default()
                    });
            }
            _ => {}
        }
        state.set_snapshot(Box::new(projected.clone()));
        if condition == "offline" {
            state.set_endpoint_status(&ClientEndpointId::Local, ClientEndpointStatus::Reconnecting);
        } else if condition == "full" {
            fill_other_agent_marks(&mut state, &mut projected);
        } else if condition == "review" {
            let target = super::super::chrome::inspector::Target::for_agent(
                ClientEndpointId::Local,
                &projected,
                &projected.agents[0],
            );
            assert!(state.mark_agent_read(&target, false));
        }
        let available = matches!(condition, "idle" | "review");
        assert_eq!(
            global_action_state(ActionId::MarkUnread, &state.global_action_context()).enabled,
            available,
            "{condition}"
        );
        state.toggle_global_menu();
        let category = palette_row_index(&state, "category:0");
        state.activate_palette_item(category, &mut ClientShellInput::default());
        let index = palette_row_index(&state, "agent:mark-unread");
        let rows = super::command_palette::palette_rows(palette_overlay(&state));
        assert_eq!(rows[index].item.enabled, available, "{condition}");
        if !available {
            state.activate_palette_item(index, &mut ClientShellInput::default());
            assert!(matches!(
                state.overlay,
                Some(ClientShellOverlay::CommandPalette(_))
            ));
            assert!(state.palette_recent.is_empty());
        }
        state.open_command_search();
        assert_eq!(
            super::command_palette::palette_rows(palette_overlay(&state))
                .iter()
                .any(|row| row.item.id == "agent:mark-unread"),
            available,
            "{condition}"
        );
    }
}

#[test]
fn cached_mark_unread_palette_rechecks_target_and_capacity_before_closing_or_mru() {
    for browser in ["menu", "search"] {
        for transition in [
            "working", "blocked", "failed", "offline", "none", "full", "valid", "review",
        ] {
            let mut state =
                ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
            let mut projected = mark_unread_snapshot();
            state.set_snapshot(Box::new(projected.clone()));
            if transition == "review" {
                let target = super::super::chrome::inspector::Target::for_agent(
                    ClientEndpointId::Local,
                    &projected,
                    &projected.agents[0],
                );
                assert!(state.mark_agent_read(&target, false));
            }
            if browser == "menu" {
                state.toggle_global_menu();
                let category = palette_row_index(&state, "category:0");
                state.activate_palette_item(category, &mut ClientShellInput::default());
            } else {
                state.open_command_search();
            }
            let index = palette_row_index(&state, "agent:mark-unread");
            assert!(
                super::command_palette::palette_rows(palette_overlay(&state))[index]
                    .item
                    .enabled
            );
            match transition {
                "working" => projected.agents[0].agent_status = AgentStatus::Working,
                "blocked" => projected.agents[0].agent_status = AgentStatus::Blocked,
                "failed" => {
                    projected.agents[0].insight.attention =
                        Some(crate::protocol::ClientShellAgentAttention {
                            kind: crate::api::schema::AgentAttentionKind::Failed,
                            ..Default::default()
                        });
                }
                "none" => projected.agents.clear(),
                "full" => fill_other_agent_marks(&mut state, &mut projected),
                _ => {}
            }
            if transition == "offline" {
                state.set_endpoint_status(
                    &ClientEndpointId::Local,
                    ClientEndpointStatus::Reconnecting,
                );
            } else if transition != "full" {
                projected.revision += 1;
                state.set_snapshot(Box::new(projected));
            }
            let before = state.saved_agent_marks();
            let mut outcome = ClientShellInput::default();
            state.activate_palette_item(index, &mut outcome);
            assert!(outcome.requests.is_empty() && outcome.actions.is_empty());
            if matches!(transition, "valid" | "review") {
                assert!(!matches!(
                    state.overlay,
                    Some(ClientShellOverlay::CommandPalette(_))
                ));
                assert_eq!(
                    state.palette_recent.first().map(String::as_str),
                    Some("agent:mark-unread")
                );
                assert_eq!(
                    state.snapshot.as_ref().expect("snapshot").agents[0].agent_status,
                    AgentStatus::Done
                );
                assert_eq!(state.saved_agent_marks()["local"].len(), 1);
            } else {
                assert!(
                    matches!(state.overlay, Some(ClientShellOverlay::CommandPalette(_))),
                    "{browser} {transition}"
                );
                assert!(!state
                    .palette_recent
                    .iter()
                    .any(|id| id == "agent:mark-unread"));
                assert_eq!(state.saved_agent_marks(), before, "{browser} {transition}");
                assert!(
                    state.endpoint_error.is_some(),
                    "{browser} {transition}: visible feedback"
                );
                assert!(outcome.repaint);
            }
        }
    }
}

#[test]
fn direct_mark_unread_action_reports_missing_target_instead_of_silent_success() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    let mut outcome = ClientShellInput::default();
    state.run_action(
        super::super::action_table::ActionId::MarkUnread,
        super::super::action_table::ActionTarget::Focused,
        &mut outcome,
    );
    assert!(state.endpoint_error.is_some());
    assert!(outcome.repaint);
    assert!(outcome.requests.is_empty() && outcome.actions.is_empty());
    assert!(state.saved_agent_marks().is_empty());
}

#[test]
fn settings_boundary_search_enter_reveals_the_original_offscreen_action() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    state.open_command_search();
    let selected = palette_row_index(&state, "binding:ClosePane");
    if let Some(ClientShellOverlay::CommandPalette(palette)) = state.overlay.as_mut() {
        palette.selected = selected;
        palette.reveal = true;
    }
    state.compose(80, 16).expect("selected action");
    state.scroll_palette(-1000);
    state.compose(80, 16).expect("wheel viewport");
    assert_eq!(palette_overlay(&state).selected, selected);
    assert!(!state
        .hits
        .global_menu_rows
        .iter()
        .any(|(_, index)| *index == selected));
    let first = state.handle_input_bytes(b"\r");
    assert!(first.actions.is_empty());
    assert!(matches!(
        state.overlay,
        Some(ClientShellOverlay::CommandPalette(_))
    ));
    assert_eq!(palette_overlay(&state).selected, selected);
    state.compose(80, 16).expect("revealed original action");
    assert!(state
        .hits
        .global_menu_rows
        .iter()
        .any(|(_, index)| *index == selected));
    let second = state.handle_input_bytes(b"\r");
    assert!(second.actions.iter().any(|action| matches!(action,
        ClientShellAction::Endpoint { request, .. } if matches!(request.method, crate::api::schema::Method::PaneClose(_)))));
}
