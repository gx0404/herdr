use super::*;
use crate::client::endpoint::{ProfileId, SavedSshEndpoint};
use crate::remote::{RemoteDirEntry, RemoteEntryKind};
use crossterm::event::KeyCode;

fn profile(label: &str, target: &str, seed: &str) -> SavedSshEndpoint {
    let hex = format!("{seed:0>32}")
        .chars()
        .map(|ch| if ch.is_ascii_hexdigit() { ch } else { 'a' })
        .take(32)
        .collect::<String>();
    SavedSshEndpoint {
        id: ProfileId::parse(hex).expect("hex profile id"),
        label: label.into(),
        target: target.into(),
        ..SavedSshEndpoint::new(label, target, "default").expect("valid profile")
    }
}

fn state_with_profiles(profiles: &[SavedSshEndpoint]) -> ClientShellState {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_endpoint_catalog(profiles);
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    state
}

fn entry(name: &str, kind: RemoteEntryKind) -> RemoteDirEntry {
    RemoteDirEntry {
        name: name.into(),
        kind,
        size: 42,
        mode: Some(0o644),
        link_target: None,
        modified: "Sep 17 10:03".into(),
    }
}

fn open_files(state: &mut ClientShellState, profile: &SavedSshEndpoint) -> ClientShellInput {
    let mut outcome = ClientShellInput::default();
    state.open_machine_files(&profile.id, &mut outcome);
    outcome
}

fn files_overlay(
    state: &mut ClientShellState,
) -> &mut super::super::machine_files_overlay::ClientMachineFilesOverlay {
    let Some(ClientShellOverlay::MachineFiles(overlay)) = state.overlay.as_mut() else {
        panic!("machine files overlay");
    };
    overlay
}

#[test]
fn superseded_reads_settle_every_ticket_without_replacing_latest_entries() {
    use super::super::machine_files_overlay::MachineFilesButton;
    let build = profile("Build", "build.example", "51");
    let mut state = state_with_profiles(std::slice::from_ref(&build));
    let first = open_files(&mut state, &build);
    let ClientShellAction::MachineFsOp { ticket: first, .. } = first.actions[0] else {
        panic!()
    };
    let mut second = ClientShellInput::default();
    state.activate_machine_files_button(MachineFilesButton::Refresh, &mut second);
    let ClientShellAction::MachineFsOp { ticket: second, .. } = second.actions[0] else {
        panic!()
    };
    assert_eq!(files_overlay(&mut state).pending, 2);
    state.handle_machine_fs_result(
        second,
        Ok(MachineFsOutcome::Entries {
            entries: vec![entry("new", RemoteEntryKind::File)],
        }),
        &mut ClientShellInput::default(),
    );
    state.handle_machine_fs_result(
        first,
        Ok(MachineFsOutcome::Entries {
            entries: vec![entry("old", RemoteEntryKind::File)],
        }),
        &mut ClientShellInput::default(),
    );
    assert_eq!(files_overlay(&mut state).pending, 0);
    assert_eq!(
        files_overlay(&mut state).entries.as_ref().unwrap()[0].name,
        "new"
    );
    state.handle_machine_fs_result(
        first,
        Err("重复响应".into()),
        &mut ClientShellInput::default(),
    );
    assert!(files_overlay(&mut state).error.is_none());
}

#[test]
fn closing_files_cancels_owned_worker_and_reopening_rejects_old_result() {
    use super::super::machine_files_overlay::MachineFilesButton;
    let build = profile("Build", "build.example", "52");
    let mut state = state_with_profiles(std::slice::from_ref(&build));
    let first = open_files(&mut state, &build);
    let ClientShellAction::MachineFsOp { ticket, cancel, .. } = &first.actions[0] else {
        panic!()
    };
    state.activate_machine_files_button(MachineFilesButton::Back, &mut ClientShellInput::default());
    assert!(cancel.is_cancelled());
    open_files(&mut state, &build);
    state.handle_machine_fs_result(
        *ticket,
        Ok(MachineFsOutcome::Entries {
            entries: vec![entry("old", RemoteEntryKind::File)],
        }),
        &mut ClientShellInput::default(),
    );
    assert!(files_overlay(&mut state).entries.is_none());
    assert_eq!(files_overlay(&mut state).pending, 1);
}

#[test]
fn remote_join_and_parent_navigation() {
    use super::super::machine_files_overlay::{remote_join, remote_parent};
    assert_eq!(remote_join(".", "etc"), "etc");
    assert_eq!(remote_join("/var", "log"), "/var/log");
    assert_eq!(remote_join("/var/", "log"), "/var/log");
    assert_eq!(remote_join("/var", "/abs"), "/abs");
    assert_eq!(remote_parent("/"), "/");
    assert_eq!(remote_parent("/var"), "/");
    assert_eq!(remote_parent("/var/log"), "/var");
    assert_eq!(remote_parent("."), "..");
    assert_eq!(remote_parent("etc"), "..");
    assert_eq!(remote_parent(".."), "../..");
    assert_eq!(remote_parent("a/b"), "a");
}

#[test]
fn opening_files_issues_a_listing_and_results_update_state() {
    let build = profile("Build", "build.example", "50");
    let mut state = state_with_profiles(std::slice::from_ref(&build));

    let outcome = open_files(&mut state, &build);
    assert_eq!(outcome.actions.len(), 1);
    let ClientShellAction::MachineFsOp { ticket, op, .. } = &outcome.actions[0] else {
        panic!("expected fs op");
    };
    assert!(matches!(op, super::super::state::MachineFsOp::List { path } if path == "."));
    assert_eq!(files_overlay(&mut state).pending, 1);

    // A stale ticket drops silently.
    let mut outcome = ClientShellInput::default();
    state.handle_machine_fs_result(
        ticket.saturating_add(9),
        Ok(super::super::state::MachineFsOutcome::Entries { entries: vec![] }),
        &mut outcome,
    );
    assert_eq!(files_overlay(&mut state).pending, 1);

    let mut outcome = ClientShellInput::default();
    state.handle_machine_fs_result(
        *ticket,
        Ok(super::super::state::MachineFsOutcome::Entries {
            entries: vec![
                entry("src", RemoteEntryKind::Directory),
                entry("README.md", RemoteEntryKind::File),
            ],
        }),
        &mut outcome,
    );
    assert_eq!(files_overlay(&mut state).pending, 0);
    assert_eq!(
        files_overlay(&mut state).entries.as_ref().map(Vec::len),
        Some(2)
    );

    // Enter on the directory (first row) navigates and re-lists.
    state.handle_raw_events(vec![RawInputEvent::Key(crossterm_key(KeyCode::Enter))]);
    assert_eq!(files_overlay(&mut state).cwd, "src");
}

fn crossterm_key(code: KeyCode) -> crate::input::TerminalKey {
    crate::input::TerminalKey::new(code, crossterm::event::KeyModifiers::empty())
}

#[test]
fn read_result_opens_the_viewer_and_errors_surface() {
    let build = profile("Build", "build.example", "51");
    let mut state = state_with_profiles(std::slice::from_ref(&build));
    open_files(&mut state, &build);
    let mut outcome = ClientShellInput::default();
    state.handle_machine_fs_result(
        1,
        Ok(super::super::state::MachineFsOutcome::Entries {
            entries: vec![entry("app.log", RemoteEntryKind::File)],
        }),
        &mut outcome,
    );

    // Enter on the file issues a Read op.
    state.handle_raw_events(vec![RawInputEvent::Key(crossterm_key(KeyCode::Enter))]);
    let mut outcome = ClientShellInput::default();
    state.handle_machine_fs_result(
        2,
        Ok(super::super::state::MachineFsOutcome::FileContent {
            content: b"line one\nline two".to_vec(),
        }),
        &mut outcome,
    );
    let view = &files_overlay(&mut state).view;
    let super::super::machine_files_overlay::ClientMachineFilesView::Viewer {
        path, content, ..
    } = view
    else {
        panic!("viewer view: {view:?}");
    };
    assert_eq!(path, "app.log");
    assert!(content.contains("line two"));

    // A failed listing shows the error instead of an endless spinner.
    state.handle_raw_events(vec![RawInputEvent::Key(crossterm_key(KeyCode::Esc))]);
    state.handle_raw_events(vec![RawInputEvent::Key(crossterm_key(KeyCode::Char('r')))]);
    let mut outcome = ClientShellInput::default();
    state.handle_machine_fs_result(3, Err("permission denied".into()), &mut outcome);
    assert_eq!(
        files_overlay(&mut state).error.as_deref(),
        Some("permission denied")
    );
    assert!(files_overlay(&mut state).entries.is_some());
}

#[test]
fn mutating_results_refresh_the_listing_with_a_message() {
    let build = profile("Build", "build.example", "52");
    let mut state = state_with_profiles(std::slice::from_ref(&build));
    open_files(&mut state, &build);
    let mut outcome = ClientShellInput::default();
    state.handle_machine_fs_result(
        1,
        Ok(super::super::state::MachineFsOutcome::Changed {
            message: "created src".into(),
        }),
        &mut outcome,
    );
    assert_eq!(
        files_overlay(&mut state).message.as_deref(),
        Some("created src")
    );
    // The refresh is a new List op on the new pending ticket.
    let refresh = outcome
        .actions
        .iter()
        .find_map(|action| match action {
            ClientShellAction::MachineFsOp { op, .. } => Some(op),
            _ => None,
        })
        .expect("refresh op");
    assert!(matches!(
        refresh,
        super::super::state::MachineFsOp::List { .. }
    ));
}

#[test]
fn prompts_build_the_right_operations() {
    let build = profile("Build", "build.example", "53");
    let mut state = state_with_profiles(std::slice::from_ref(&build));
    open_files(&mut state, &build);
    let mut outcome = ClientShellInput::default();
    state.handle_machine_fs_result(
        1,
        Ok(super::super::state::MachineFsOutcome::Entries {
            entries: vec![
                entry("logs", RemoteEntryKind::Directory),
                entry("notes.txt", RemoteEntryKind::File),
            ],
        }),
        &mut outcome,
    );

    fn fs_op(outcome: &ClientShellInput) -> &super::super::state::MachineFsOp {
        outcome
            .actions
            .iter()
            .find_map(|action| match action {
                ClientShellAction::MachineFsOp { op, .. } => Some(op),
                _ => None,
            })
            .expect("fs op")
    }

    fn finish_write(state: &mut ClientShellState, result: &ClientShellInput) {
        let ticket = result
            .actions
            .iter()
            .find_map(|action| match action {
                ClientShellAction::MachineFsOp { ticket, .. } => Some(*ticket),
                _ => None,
            })
            .unwrap();
        state.handle_machine_fs_result(
            ticket,
            Ok(MachineFsOutcome::Changed {
                message: "完成".into(),
            }),
            &mut ClientShellInput::default(),
        );
    }

    // Download with an empty input falls back to the entry name.
    files_overlay(&mut state).selected = 1;
    state.handle_raw_events(vec![RawInputEvent::Key(crossterm_key(KeyCode::Char('d')))]);
    let outcome = state.handle_raw_events(vec![RawInputEvent::Key(crossterm_key(KeyCode::Enter))]);
    assert!(matches!(
        fs_op(&outcome),
        super::super::state::MachineFsOp::Download { remote, local, .. }
            if remote == "notes.txt" && local == "notes.txt"
    ));

    finish_write(&mut state, &outcome);

    // Mkdir joins the current directory.
    state.handle_raw_events(vec![RawInputEvent::Key(crossterm_key(KeyCode::Char('m')))]);
    for ch in "out".chars() {
        state.insert_overlay_text(&ch.to_string());
    }
    let outcome = state.handle_raw_events(vec![RawInputEvent::Key(crossterm_key(KeyCode::Enter))]);
    assert!(matches!(
        fs_op(&outcome),
        super::super::state::MachineFsOp::Mkdir { path, .. } if path == "out"
    ));

    finish_write(&mut state, &outcome);

    // Delete on a directory asks for a recursive confirmation.
    files_overlay(&mut state).selected = 0;
    state.handle_raw_events(vec![RawInputEvent::Key(crossterm_key(KeyCode::Char('x')))]);
    assert!(matches!(
        files_overlay(&mut state).view,
        super::super::machine_files_overlay::ClientMachineFilesView::ConfirmDelete {
            recursive: true,
            ..
        }
    ));
    let outcome = state.handle_raw_events(vec![RawInputEvent::Key(crossterm_key(KeyCode::Enter))]);
    assert!(matches!(
        fs_op(&outcome),
        super::super::state::MachineFsOp::Delete { path, recursive: true, .. } if path == "logs"
    ));
}

/// C-16：过滤计数缓存与 selected clamp 同机更新；渲染只处理可见窗口，
/// 不再每帧克隆整个目录并对每个条目 to_lowercase。
#[test]
fn machine_files_filter_count_caches_and_clamps_with_query() {
    let build = profile("Build", "build.example", "60");
    let mut state = state_with_profiles(std::slice::from_ref(&build));
    open_files(&mut state, &build);
    let entries: Vec<_> = (0..5000)
        .map(|i| entry(&format!("file-{i:04}"), RemoteEntryKind::File))
        .collect();
    state.handle_machine_fs_result(
        1,
        Ok(super::super::state::MachineFsOutcome::Entries { entries }),
        &mut ClientShellInput::default(),
    );
    assert_eq!(files_overlay(&mut state).filtered_count, 5000);

    // 搜索过滤：计数缓存同步更新、selected 复位。
    files_overlay(&mut state).search_focused = true;
    for ch in "file-4999".chars() {
        state.insert_overlay_text(&ch.to_string());
    }
    assert_eq!(files_overlay(&mut state).filtered_count, 1);
    assert_eq!(files_overlay(&mut state).selected, 0);

    // 渲染只画可见窗口；计数走缓存。
    let frame = state.compose(100, 28).expect("files frame");
    let text = frame_rows(&frame).join("\n");
    assert!(text.contains("file-4999"), "frame: {text}");
    assert!(text.contains("1"), "frame: {text}");
}

/// C-17 + HERDR-MACH-009：查看器行偏移在内容落地时一次算好，渲染按 scroll
/// 切片借用 &str；scroll 有上界（行数 − 可见行数），越过底部后停在最后一页。
#[test]
fn machine_files_viewer_slices_lines_and_clamps_scroll() {
    let build = profile("Build", "build.example", "61");
    let mut state = state_with_profiles(std::slice::from_ref(&build));
    open_files(&mut state, &build);
    state.handle_machine_fs_result(
        1,
        Ok(super::super::state::MachineFsOutcome::Entries {
            entries: vec![entry("app.log", RemoteEntryKind::File)],
        }),
        &mut ClientShellInput::default(),
    );
    // Enter on the file issues a Read op (ticket 2).
    state.handle_raw_events(vec![RawInputEvent::Key(crossterm_key(KeyCode::Enter))]);
    let content: String = (0..10000).map(|i| format!("line {i}\n")).collect();
    state.handle_machine_fs_result(
        2,
        Ok(super::super::state::MachineFsOutcome::FileContent {
            content: content.into_bytes(),
        }),
        &mut ClientShellInput::default(),
    );
    // 滚轮一路向下：scroll 停在 max_scroll，不再无限增长。
    for _ in 0..4000 {
        state.scroll_machine_files_overlay(1);
    }
    let view = &files_overlay(&mut state).view;
    let super::super::machine_files_overlay::ClientMachineFilesView::Viewer {
        scroll,
        max_scroll,
        line_offsets,
        ..
    } = view
    else {
        panic!("viewer view");
    };
    assert_eq!(line_offsets.len(), 10000);
    assert!(*max_scroll > 0 && *max_scroll < 10000);
    assert_eq!(
        *scroll, *max_scroll,
        "scroll 停在最后一页（无上界增长已修复）"
    );

    let frame = state.compose(100, 28).expect("viewer frame");
    let text = frame_rows(&frame).join("\n");
    assert!(text.contains("line 9999"), "frame tail: {text}");
    assert!(
        !text.contains("line 0001"),
        "frame must not render the head: {text}"
    );
}
