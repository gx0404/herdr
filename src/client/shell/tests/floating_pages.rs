use super::*;

fn ready() -> ClientShellState {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    state.open_settings_overlay();
    state.compose(120, 40).unwrap();
    state
}

fn mouse(state: &mut ClientShellState, kind: MouseEventKind, x: u16, y: u16) -> ClientShellInput {
    state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
        kind,
        column: x,
        row: y,
        modifiers: KeyModifiers::NONE,
    })])
}

#[test]
fn page_header_drag_and_edge_resize_preserve_terminal_and_page_state() {
    let mut state = ready();
    state.move_settings_selection(2);
    let original = state.hits.overlay_bounds;
    let source = state.pane_surface.as_ref().unwrap().frame.cells.clone();
    mouse(
        &mut state,
        MouseEventKind::Down(MouseButton::Left),
        original.x + 5,
        original.y,
    );
    mouse(
        &mut state,
        MouseEventKind::Drag(MouseButton::Left),
        original.x + 9,
        original.y + 2,
    );
    mouse(
        &mut state,
        MouseEventKind::Up(MouseButton::Left),
        original.x + 9,
        original.y + 2,
    );
    state.compose(120, 40).unwrap();
    let moved = state.hits.overlay_bounds;
    assert_eq!((moved.x, moved.y), (original.x + 4, original.y + 2));
    mouse(
        &mut state,
        MouseEventKind::Down(MouseButton::Left),
        moved.right() - 1,
        moved.bottom() - 1,
    );
    mouse(
        &mut state,
        MouseEventKind::Drag(MouseButton::Left),
        moved.right() - 11,
        moved.bottom() + 1,
    );
    mouse(
        &mut state,
        MouseEventKind::Up(MouseButton::Left),
        moved.right() - 11,
        moved.bottom() + 1,
    );
    state.compose(120, 40).unwrap();
    assert_eq!(state.hits.overlay_bounds.width, moved.width - 10);
    assert_eq!(state.hits.overlay_bounds.height, moved.height + 2);
    assert_eq!(state.pane_surface.as_ref().unwrap().frame.cells, source);
    assert!(
        matches!(state.overlay, Some(ClientShellOverlay::Settings(ref page)) if page.selected == 2)
    );
}

#[test]
fn dragged_pages_remain_inside_every_terminal_size() {
    let mut state = ready();
    let rect = state.hits.overlay_bounds;
    mouse(
        &mut state,
        MouseEventKind::Down(MouseButton::Left),
        rect.x + 4,
        rect.y,
    );
    mouse(&mut state, MouseEventKind::Drag(MouseButton::Left), 119, 39);
    mouse(&mut state, MouseEventKind::Up(MouseButton::Left), 119, 39);
    for (cols, rows) in [(160, 50), (80, 24), (60, 16), (16, 6), (1, 1)] {
        let frame = state
            .compose(cols, rows)
            .expect("尺寸过小时也输出可关闭的页面");
        assert_eq!(frame.cells.len(), usize::from(cols) * usize::from(rows));
        assert!(state.hits.overlay_bounds.right() <= cols);
        assert!(state.hits.overlay_bounds.bottom() <= rows);
    }
}

#[test]
fn visible_launcher_opens_menu_without_dropping_settings() {
    let mut state = ready();
    state.move_settings_selection(1);
    let launcher = state.hits.global_launcher;
    mouse(
        &mut state,
        MouseEventKind::Down(MouseButton::Left),
        launcher.x,
        launcher.y,
    );
    assert!(matches!(
        state.overlay,
        Some(ClientShellOverlay::CommandPalette(_))
    ));
    state.close_command_browser();
    assert!(
        matches!(state.overlay, Some(ClientShellOverlay::Settings(ref page)) if page.selected == 1)
    );
}
