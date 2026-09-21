use super::*;
use crate::client::endpoint::{
    ClientEndpointId, ClientEndpointStatus, SavedSshEndpoint, SnippetLibrary,
};
use crossterm::event::{KeyCode, KeyModifiers};

fn with_temp_state_home(name: &str) -> std::path::PathBuf {
    let dir =
        std::env::temp_dir().join(format!("herdr-snippets-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp state home");
    // Safety: nextest isolates every test in its own process, so mutating the
    // process environment here cannot race other tests.
    unsafe { std::env::set_var("XDG_STATE_HOME", &dir) };
    dir
}

fn state() -> ClientShellState {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_endpoint_catalog(&[]);
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    state.set_endpoint_status(&ClientEndpointId::Local, ClientEndpointStatus::Online);
    state
}

fn key(code: KeyCode) -> crate::input::TerminalKey {
    crate::input::TerminalKey::new(code, KeyModifiers::empty())
}

fn seed_snippet(label: &str, command: &str, variables: &[&str]) {
    let mut library = SnippetLibrary::load().unwrap_or_default();
    library
        .add_snippet(
            label,
            command,
            Some("test snippet".into()),
            variables.iter().map(|v| (*v).to_owned()).collect(),
            Vec::new(),
        )
        .expect("seed snippet");
    library.store().expect("store seed library");
}

fn frame_text(state: &mut ClientShellState, cols: u16, rows: u16) -> String {
    let frame = state.compose(cols, rows).expect("composed frame");
    frame
        .cells
        .chunks(frame.width as usize)
        .map(|row| row.iter().map(|cell| cell.symbol.as_str()).collect())
        .collect::<Vec<String>>()
        .join("\n")
}

fn click_snippet_point(state: &mut ClientShellState, point: (u16, u16)) {
    state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: point.0,
        row: point.1,
        modifiers: KeyModifiers::empty(),
    })]);
}

/// 列表第 `row` 行当前展示的片段标题（筛选后的顺序）。
fn snippet_label_at(state: &ClientShellState, row: usize) -> Option<String> {
    let Some(ClientShellOverlay::Snippets(overlay)) = state.overlay.as_ref() else {
        panic!("snippets overlay");
    };
    let query = overlay.query.as_str().to_lowercase();
    overlay
        .library
        .snippets
        .iter()
        .filter(|snippet| {
            query.is_empty()
                || format!(
                    "{} {} {} {}",
                    snippet.label,
                    snippet.command,
                    snippet.description.as_deref().unwrap_or_default(),
                    snippet.tags.join(" ")
                )
                .to_lowercase()
                .contains(&query)
        })
        .nth(row)
        .map(|snippet| snippet.label.clone())
}

fn snippets_view(state: &ClientShellState) -> &super::super::snippets_overlay::ClientSnippetsView {
    let Some(ClientShellOverlay::Snippets(overlay)) = state.overlay.as_ref() else {
        panic!("snippets overlay");
    };
    &overlay.view
}

#[test]
fn snippet_list_renders_and_filters() {
    let dir = with_temp_state_home("list");
    seed_snippet(
        "deploy",
        "kubectl rollout restart deploy/{{name}}",
        &["name"],
    );
    seed_snippet("logs", "kubectl logs -f svc/web", &[]);
    let mut state = state();

    state.open_snippets_overlay(false);
    let text = frame_text(&mut state, 106, 30);
    assert!(text.contains("deploy"), "frame: {text}");
    assert!(text.contains("logs"), "frame: {text}");
    assert!(text.contains("kubectl logs"), "frame: {text}");

    if let Some(ClientShellOverlay::Snippets(overlay)) = state.overlay.as_mut() {
        overlay.search_focused = true;
    }
    for ch in "depl".chars() {
        state.handle_raw_events(vec![RawInputEvent::Key(crate::input::TerminalKey::new(
            KeyCode::Char(ch),
            KeyModifiers::empty(),
        ))]);
    }
    let text = frame_text(&mut state, 106, 30);
    assert!(!text.contains("kubectl logs"), "frame: {text}");
    assert!(text.contains("rollout"), "frame: {text}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn snippet_form_saves_edits_and_deletes() {
    let dir = with_temp_state_home("form");
    let mut state = state();
    state.open_snippets_overlay(false);
    state.route_snippets_key(&key(KeyCode::Char('n')), &mut ClientShellInput::default());
    {
        let Some(ClientShellOverlay::Snippets(overlay)) = state.overlay.as_mut() else {
            panic!("overlay");
        };
        let super::super::snippets_overlay::ClientSnippetsView::Form(form) = &mut overlay.view
        else {
            panic!("form view");
        };
        form.label = TextEditor::new("restart", false);
        form.command = TextEditor::new("systemctl restart x", false);
        form.variables = TextEditor::new("unit", false);
    }
    state.route_snippets_key(&key(KeyCode::Enter), &mut ClientShellInput::default());
    let library = SnippetLibrary::load().expect("library");
    assert_eq!(library.snippets.len(), 1);
    assert_eq!(library.snippets[0].label, "restart");
    assert_eq!(library.snippets[0].variables, vec!["unit".to_string()]);

    // Edit in place: the id survives so history stays correlated.
    let id = library.snippets[0].id.clone();
    {
        let Some(ClientShellOverlay::Snippets(overlay)) = state.overlay.as_mut() else {
            panic!("overlay");
        };
        overlay.selected = 0;
    }
    state.route_snippets_key(&key(KeyCode::Char('e')), &mut ClientShellInput::default());
    {
        let Some(ClientShellOverlay::Snippets(overlay)) = state.overlay.as_mut() else {
            panic!("overlay");
        };
        let super::super::snippets_overlay::ClientSnippetsView::Form(form) = &mut overlay.view
        else {
            panic!("form view");
        };
        assert_eq!(form.editing.as_ref(), Some(&id));
        form.command = TextEditor::new("systemctl restart y", false);
    }
    state.route_snippets_key(&key(KeyCode::Enter), &mut ClientShellInput::default());
    let library = SnippetLibrary::load().expect("library");
    assert_eq!(library.snippets.len(), 1);
    assert_eq!(library.snippets[0].id, id, "edit keeps the id");
    assert_eq!(library.snippets[0].command, "systemctl restart y");

    // Delete via the confirmation.
    state.route_snippets_key(&key(KeyCode::Char('x')), &mut ClientShellInput::default());
    assert!(matches!(
        snippets_view(&state),
        super::super::snippets_overlay::ClientSnippetsView::DeleteConfirm(_)
    ));
    state.route_snippets_key(&key(KeyCode::Enter), &mut ClientShellInput::default());
    assert!(SnippetLibrary::load().expect("library").snippets.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn snippet_run_current_pane_sends_input_and_records_history() {
    let dir = with_temp_state_home("run");
    seed_snippet("deploy", "kubectl rollout restart deploy/api", &[]);
    let mut state = state();
    state.open_snippets_overlay(true);
    // Enter on the only snippet starts the run flow at the target chooser.
    state.route_snippets_key(&key(KeyCode::Enter), &mut ClientShellInput::default());
    assert!(matches!(
        snippets_view(&state),
        super::super::snippets_overlay::ClientSnippetsView::RunTargets(_)
    ));
    // First target row is the current pane; no variables, so this lands on
    // the confirmation.
    state.route_snippets_key(&key(KeyCode::Enter), &mut ClientShellInput::default());
    assert!(matches!(
        snippets_view(&state),
        super::super::snippets_overlay::ClientSnippetsView::RunConfirm(_)
    ));
    let text = frame_text(&mut state, 106, 30);
    assert!(
        text.contains("kubectl rollout restart deploy/api"),
        "frame: {text}"
    );

    let mut outcome = ClientShellInput::default();
    state.route_snippets_key(&key(KeyCode::Char('y')), &mut outcome);
    let [ClientShellAction::EndpointRequest {
        endpoint_id,
        request,
        ..
    }] = &outcome.actions[..]
    else {
        panic!("endpoint request: {:?}", outcome.actions);
    };
    assert_eq!(endpoint_id, &ClientEndpointId::Local);
    let crate::api::schema::Method::PaneSendInput(params) = &request.method else {
        panic!("pane send input: {:?}", request.method);
    };
    assert_eq!(params.pane_id, "pane_1");
    assert_eq!(params.text, "kubectl rollout restart deploy/api");
    assert_eq!(params.keys, vec!["Enter".to_string()]);
    assert!(
        state.overlay.is_none(),
        "overlay closes once the run starts"
    );

    // The server response resolves the run: history entry plus summary toast.
    let request_id = request.id.clone();
    state.handle_endpoint_result(
        "boot-1",
        &request_id,
        Ok(crate::api::schema::ResponseResult::Ok {}),
    );
    let library = SnippetLibrary::load().expect("library");
    assert_eq!(library.history.len(), 1);
    let record = &library.history[0];
    assert_eq!(record.snippet_label, "deploy");
    assert_eq!(record.machine, "local");
    assert_eq!(record.pane_id, "pane_1");
    assert!(record.success);
    let notice = state
        .visible_endpoint_notice
        .as_ref()
        .expect("summary toast shows");
    assert_eq!(notice.key.kind, ClientEndpointNoticeKind::Success);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn snippet_run_variables_render_into_the_command() {
    let dir = with_temp_state_home("vars");
    seed_snippet(
        "restart",
        "kubectl rollout restart deploy/{{name}}",
        &["name"],
    );
    let mut state = state();
    state.open_snippets_overlay(true);
    state.route_snippets_key(&key(KeyCode::Enter), &mut ClientShellInput::default());
    state.route_snippets_key(&key(KeyCode::Enter), &mut ClientShellInput::default());
    // The snippet declares a variable, so the variables form comes first.
    assert!(matches!(
        snippets_view(&state),
        super::super::snippets_overlay::ClientSnippetsView::RunVariables(_)
    ));
    {
        let Some(ClientShellOverlay::Snippets(overlay)) = state.overlay.as_mut() else {
            panic!("overlay");
        };
        let super::super::snippets_overlay::ClientSnippetsView::RunVariables(draft) =
            &mut overlay.view
        else {
            panic!("variables view");
        };
        draft.variables[0].1 = TextEditor::new("api", false);
    }
    state.route_snippets_key(&key(KeyCode::Enter), &mut ClientShellInput::default());
    assert!(matches!(
        snippets_view(&state),
        super::super::snippets_overlay::ClientSnippetsView::RunConfirm(_)
    ));
    let text = frame_text(&mut state, 106, 30);
    assert!(text.contains("rollout restart deploy/api"), "frame: {text}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn snippet_run_multi_machine_fans_out_per_endpoint() {
    let dir = with_temp_state_home("fanout");
    seed_snippet("uptime", "uptime", &[]);
    let build = SavedSshEndpoint::new("Build", "build.example", "default").expect("profile");
    let build_profile_id = build.id.clone();
    let build_id = ClientEndpointId::Ssh(build_profile_id.clone());
    let mut state = state();
    state.set_endpoint_catalog(&[build]);
    // Bring the machine online with its own snapshot (boot id differs).
    state.cache_endpoint_snapshot(&build_id, Box::new(snapshot()));
    state.set_endpoint_status(&build_id, ClientEndpointStatus::Online);

    state.open_snippets_overlay(true);
    state.route_snippets_key(&key(KeyCode::Enter), &mut ClientShellInput::default());
    // Third target row: one pane per machine.
    state.route_snippets_key(&key(KeyCode::Down), &mut ClientShellInput::default());
    state.route_snippets_key(&key(KeyCode::Down), &mut ClientShellInput::default());
    state.route_snippets_key(&key(KeyCode::Enter), &mut ClientShellInput::default());
    assert!(matches!(
        snippets_view(&state),
        super::super::snippets_overlay::ClientSnippetsView::RunPickMachines(_)
    ));
    // Both machines start selected; confirm resolves each to its focused pane.
    state.route_snippets_key(&key(KeyCode::Enter), &mut ClientShellInput::default());
    assert!(matches!(
        snippets_view(&state),
        super::super::snippets_overlay::ClientSnippetsView::RunConfirm(_)
    ));
    let mut outcome = ClientShellInput::default();
    state.route_snippets_key(&key(KeyCode::Char('y')), &mut outcome);
    let endpoints: Vec<ClientEndpointId> = outcome
        .actions
        .iter()
        .map(|action| match action {
            ClientShellAction::EndpointRequest { endpoint_id, .. } => endpoint_id.clone(),
            other => panic!("unexpected action: {other:?}"),
        })
        .collect();
    assert_eq!(endpoints.len(), 2, "one request per machine: {endpoints:?}");
    assert!(endpoints.contains(&ClientEndpointId::Local));
    assert!(endpoints.contains(&build_id));

    // Each machine resolves independently; the run completes after both.
    for action in &outcome.actions {
        let ClientShellAction::EndpointRequest {
            endpoint_id,
            boot_id,
            request,
        } = action
        else {
            continue;
        };
        state.handle_endpoint_result(
            boot_id,
            &request.id,
            if endpoint_id == &build_id {
                Err(ClientShellEndpointError {
                    code: Some("pane_missing".into()),
                    message: "no such pane".into(),
                })
            } else {
                Ok(crate::api::schema::ResponseResult::Ok {})
            },
        );
    }
    let library = SnippetLibrary::load().expect("library");
    assert_eq!(library.history.len(), 2, "{:?}", library.history);
    let failed = library
        .history
        .iter()
        .find(|record| !record.success)
        .expect("one failure recorded");
    assert_eq!(failed.machine, build_profile_id.to_string());
    assert_eq!(failed.error.as_deref(), Some("no such pane"));
    assert!(state.snippet_run.is_none(), "run state cleared");
    let notice = state.visible_endpoint_notice.as_ref().expect("toast");
    assert!(notice.body.contains("no such pane"), "{}", notice.body);
    let _ = std::fs::remove_dir_all(&dir);
}

/// TOOL-07：指针路过后的首次点击只允许选中。MENU-01 之后 hover 只写
/// `hovered`，键盘选中不再被指针劫持，但「二次点击才运行」仍靠独立的点击
/// 痕迹判定。窗口显式放大到 60 s，判定结果与真实耗时（nextest 并行下的调度
/// 抖动）无关。
#[test]
fn snippet_list_click_runs_only_on_the_second_click_after_hover() {
    let dir = with_temp_state_home("double-click");
    seed_snippet("deploy", "kubectl rollout restart deploy/web", &[]);
    seed_snippet("logs", "kubectl logs -f svc/web", &[]);
    let mut state = state();
    state.config.double_click_window = std::time::Duration::from_secs(60);
    state.open_snippets_overlay(false);
    state.compose(106, 30).expect("composed frame");

    let (rect, row) = *state
        .hits
        .snippet_rows
        .get(1)
        .expect("the list should expose clickable rows");
    let point = (rect.x, rect.y);

    // 指针移到该行：只写 hover，键盘选中不动（MENU-01）。
    state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Moved,
        column: point.0,
        row: point.1,
        modifiers: KeyModifiers::empty(),
    })]);
    let Some(ClientShellOverlay::Snippets(overlay)) = state.overlay.as_ref() else {
        panic!("snippets overlay");
    };
    assert_eq!(overlay.hovered, Some(row));
    assert_eq!(overlay.selected, 0, "hover 不改写键盘选中");
    assert!(
        overlay.last_click.is_none(),
        "hover alone must not leave a click trace"
    );
    assert!(matches!(
        snippets_view(&state),
        super::super::snippets_overlay::ClientSnippetsView::List
    ));

    // hover 之后的首次点击不得进入运行流，只留下点击痕迹。
    click_snippet_point(&mut state, point);
    assert!(
        matches!(
            snippets_view(&state),
            super::super::snippets_overlay::ClientSnippetsView::List
        ),
        "the first click after a hover must only select the row"
    );
    let Some(ClientShellOverlay::Snippets(overlay)) = state.overlay.as_ref() else {
        panic!("snippets overlay");
    };
    assert!(
        overlay.last_click.is_some(),
        "the first click must record a trace"
    );

    // 指针移出行区域：hover 清空（不能留在鼠标早已离开的那一行）。
    state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Moved,
        column: point.0,
        row: 0,
        modifiers: KeyModifiers::empty(),
    })]);
    let Some(ClientShellOverlay::Snippets(overlay)) = state.overlay.as_ref() else {
        panic!("snippets overlay");
    };
    assert_eq!(overlay.hovered, None, "移出行区域后不留残影");

    // 同一片段的第二次点击才开始运行流。
    click_snippet_point(&mut state, point);
    assert!(matches!(
        snippets_view(&state),
        super::super::snippets_overlay::ClientSnippetsView::RunTargets(_)
    ));
    let _ = std::fs::remove_dir_all(&dir);
}

/// MENU-01：`hovered` 存的是行号，而 List / RunTargets / RunPickPane /
/// RunPickMachines / History 五个视图共用这一个字段。切视图不清就会在新视图
/// 里把同号的那一行画成「悬浮」，而指针其实停在别处，且要等下一次 `Moved`
/// 才纠正——切视图的唯一写点 `set_view` 必须连带清掉它。
#[test]
fn snippet_view_switch_clears_the_pointer_hover() {
    let dir = with_temp_state_home("hover-view-switch");
    seed_snippet("deploy", "kubectl rollout restart deploy/web", &[]);
    seed_snippet("logs", "kubectl logs -f svc/web", &[]);
    let mut state = state();
    state.config.double_click_window = std::time::Duration::from_secs(60);
    state.open_snippets_overlay(false);
    state.compose(106, 30).expect("composed frame");

    let (rect, row) = *state
        .hits
        .snippet_rows
        .get(1)
        .expect("the list should expose clickable rows");
    let point = (rect.x, rect.y);
    state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Moved,
        column: point.0,
        row: point.1,
        modifiers: KeyModifiers::empty(),
    })]);
    let Some(ClientShellOverlay::Snippets(overlay)) = state.overlay.as_ref() else {
        panic!("snippets overlay");
    };
    assert_eq!(overlay.hovered, Some(row));

    // 双击进运行流：换到 RunTargets，旧行号必须失效。
    click_snippet_point(&mut state, point);
    click_snippet_point(&mut state, point);
    assert!(matches!(
        snippets_view(&state),
        super::super::snippets_overlay::ClientSnippetsView::RunTargets(_)
    ));
    let Some(ClientShellOverlay::Snippets(overlay)) = state.overlay.as_ref() else {
        panic!("snippets overlay");
    };
    assert_eq!(overlay.hovered, None, "换视图必须清 hover");

    // 退回列表同样清。
    state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Moved,
        column: point.0,
        row: point.1,
        modifiers: KeyModifiers::empty(),
    })]);
    assert!(matches!(
        state.overlay.as_ref(),
        Some(ClientShellOverlay::Snippets(overlay)) if overlay.hovered.is_some()
    ));
    state.handle_input_bytes(b"\x1b");
    assert!(matches!(
        snippets_view(&state),
        super::super::snippets_overlay::ClientSnippetsView::List
    ));
    let Some(ClientShellOverlay::Snippets(overlay)) = state.overlay.as_ref() else {
        panic!("snippets overlay");
    };
    assert_eq!(overlay.hovered, None, "回到列表也清 hover");
    let _ = std::fs::remove_dir_all(&dir);
}

/// 点击痕迹记的是片段身份，不是行号：删掉一个片段后整列上移，原先记下的行号
/// 指向的已经是另一个片段，这一次点击必须只算首击。
#[test]
fn deleting_a_snippet_invalidates_the_click_trace_for_that_row() {
    let dir = with_temp_state_home("delete-trace");
    seed_snippet("alpha", "echo alpha", &[]);
    seed_snippet("beta", "echo beta", &[]);
    seed_snippet("gamma", "echo gamma", &[]);
    let mut state = state();
    state.config.double_click_window = std::time::Duration::from_secs(60);
    state.open_snippets_overlay(false);
    state.compose(106, 30).expect("composed frame");

    let (rect, _) = *state
        .hits
        .snippet_rows
        .get(1)
        .expect("the list should expose clickable rows");
    let point = (rect.x, rect.y);

    // 第 1 行（beta）点一次：留下 beta 的点击痕迹。
    click_snippet_point(&mut state, point);
    let clicked_label = snippet_label_at(&state, 1);
    assert_eq!(clicked_label.as_deref(), Some("beta"));

    // 删掉 beta：确认视图回到列表后，第 1 行已经是 gamma。
    state.handle_raw_events(vec![RawInputEvent::Key(key(KeyCode::Char('x')))]);
    assert!(matches!(
        snippets_view(&state),
        super::super::snippets_overlay::ClientSnippetsView::DeleteConfirm(_)
    ));
    state.handle_raw_events(vec![RawInputEvent::Key(key(KeyCode::Enter))]);
    assert!(matches!(
        snippets_view(&state),
        super::super::snippets_overlay::ClientSnippetsView::List
    ));
    state.compose(106, 30).expect("composed frame");
    assert_eq!(snippet_label_at(&state, 1).as_deref(), Some("gamma"));

    // 「确认删除」按钮在确认视图关闭后正好被列表体覆盖，所以一次双击就能让第二个
    // Down 落进列表行——挤上来的片段不得被这一击直接运行。
    click_snippet_point(&mut state, point);
    assert!(
        matches!(
            snippets_view(&state),
            super::super::snippets_overlay::ClientSnippetsView::List
        ),
        "a snippet that moved up into the clicked row must not run on the first click"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// 窗口过期分支：超过 `ui.double_click_ms` 的第二次点击仍然只算首击。
#[test]
fn snippet_click_outside_the_double_click_window_only_selects() {
    let dir = with_temp_state_home("expired-window");
    seed_snippet("deploy", "kubectl rollout restart deploy/web", &[]);
    seed_snippet("logs", "kubectl logs -f svc/web", &[]);
    let mut state = state();
    // 1 ns 的窗口保证两次事件之间必然过期。
    state.config.double_click_window = std::time::Duration::from_nanos(1);
    state.open_snippets_overlay(false);
    state.compose(106, 30).expect("composed frame");
    let (rect, _) = *state
        .hits
        .snippet_rows
        .get(1)
        .expect("the list should expose clickable rows");
    let point = (rect.x, rect.y);

    click_snippet_point(&mut state, point);
    click_snippet_point(&mut state, point);
    assert!(
        matches!(
            snippets_view(&state),
            super::super::snippets_overlay::ClientSnippetsView::List
        ),
        "a click outside the double-click window must only select"
    );
    let Some(ClientShellOverlay::Snippets(overlay)) = state.overlay.as_ref() else {
        panic!("snippets overlay");
    };
    assert!(
        overlay.last_click.is_some(),
        "the expired click still becomes the new trace"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// 整套长按保护取决于 `step()` 为每个破坏性步进返回不同值：把某个变体并进零值组
/// 就静默失去保护，这里把「两两不同」固化下来。
#[test]
fn overlay_step_separates_every_destructive_snippet_step() {
    use super::super::snippets_overlay::{
        ClientSnippetForm, ClientSnippetRunDraft, ClientSnippetsView,
    };

    let mut library = SnippetLibrary::default();
    library
        .add_snippet("deploy", "echo deploy", None, Vec::new(), Vec::new())
        .expect("seed snippet");
    let snippet = library.snippets[0].clone();
    let draft = || {
        Box::new(ClientSnippetRunDraft {
            snippet: snippet.clone(),
            targets: Vec::new(),
            target_selected: 0,
            machine_selected: Vec::new(),
            variables: Vec::new(),
            variable_focused: 0,
            press_enter: true,
            error: None,
        })
    };
    let views = [
        ClientSnippetsView::List,
        ClientSnippetsView::Form(Box::new(ClientSnippetForm {
            editing: None,
            focused: 0,
            label: TextEditor::default(),
            command: TextEditor::default(),
            description: TextEditor::default(),
            variables: TextEditor::default(),
            tags: TextEditor::default(),
            error: None,
        })),
        ClientSnippetsView::DeleteConfirm(snippet.id.clone()),
        ClientSnippetsView::RunTargets(draft()),
        ClientSnippetsView::RunPickPane(draft()),
        ClientSnippetsView::RunPickMachines(draft()),
        ClientSnippetsView::RunVariables(draft()),
        ClientSnippetsView::RunConfirm(draft()),
        ClientSnippetsView::History {
            selected: 0,
            scroll: 0,
        },
    ];
    let steps = views
        .iter()
        .map(super::super::snippets_overlay::ClientSnippetsView::step)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        steps.len(),
        views.len(),
        "every snippets view needs its own step value"
    );

    // worktree 的强制删除是同一浮层里的另一步，武装前后也必须不同。
    let remove = |force_confirmation: bool| {
        ClientShellOverlay::WorktreeRemove(super::super::state::ClientWorktreeRemoveOverlay {
            workspace_id: "ws_1".into(),
            path: "/repo-feature".into(),
            error: None,
            removing: false,
            force_confirmation,
        })
    };
    assert_ne!(remove(false).step(), remove(true).step());
}

/// TOOL-01：列表 → 运行目标 → 确认是三步不同的确认，长按回车不得一路走完
/// 并把命令注入 pane。
#[test]
fn held_enter_does_not_walk_the_snippet_run_flow() {
    use crossterm::event::KeyEventKind;

    let dir = with_temp_state_home("held-enter");
    seed_snippet("deploy", "kubectl rollout restart deploy/web", &[]);
    let mut state = state();
    state.open_snippets_overlay(false);

    let enter = key(KeyCode::Enter);
    state.handle_raw_events(vec![RawInputEvent::Key(enter.clone())]);
    assert!(matches!(
        snippets_view(&state),
        super::super::snippets_overlay::ClientSnippetsView::RunTargets(_)
    ));

    for _ in 0..4 {
        let repeat = state.handle_raw_events(vec![RawInputEvent::Key(
            enter.clone().with_kind(KeyEventKind::Repeat),
        )]);
        assert!(
            repeat.actions.is_empty() && repeat.requests.is_empty(),
            "held enter must not advance the run flow"
        );
        assert!(
            matches!(
                snippets_view(&state),
                super::super::snippets_overlay::ClientSnippetsView::RunTargets(_)
            ),
            "held enter must not advance past the target picker"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// C-02 残留面（已拍板）：RunConfirm 的执行键换成 y（与 worktree 强删同构）。
/// 非 kitty 宿主下自动重复只发普通 Press，三次普通回车不得注入命令；回车在该步
/// 不再执行，`y`（或 ctrl+↵）才执行。
#[test]
fn plain_enter_presses_never_execute_snippet_run() {
    let dir = with_temp_state_home("plain-enter-run");
    seed_snippet("deploy", "kubectl rollout restart deploy/web", &[]);
    let mut state = state();
    state.open_snippets_overlay(true);
    // List → RunTargets → RunConfirm。
    state.route_snippets_key(&key(KeyCode::Enter), &mut ClientShellInput::default());
    state.route_snippets_key(&key(KeyCode::Enter), &mut ClientShellInput::default());
    assert!(matches!(
        snippets_view(&state),
        super::super::snippets_overlay::ClientSnippetsView::RunConfirm(_)
    ));

    for _ in 0..3 {
        let mut outcome = ClientShellInput::default();
        state.route_snippets_key(&key(KeyCode::Enter), &mut outcome);
        assert!(
            outcome.actions.is_empty() && outcome.requests.is_empty(),
            "plain enter must not execute: {:?}",
            outcome.actions
        );
        assert!(
            matches!(
                snippets_view(&state),
                super::super::snippets_overlay::ClientSnippetsView::RunConfirm(_)
            ),
            "plain enter must stay on the confirm step"
        );
    }

    let mut outcome = ClientShellInput::default();
    state.route_snippets_key(&key(KeyCode::Char('y')), &mut outcome);
    let [ClientShellAction::EndpointRequest { request, .. }] = &outcome.actions[..] else {
        panic!("y executes the run: {:?}", outcome.actions);
    };
    let crate::api::schema::Method::PaneSendInput(params) = &request.method else {
        panic!("pane send input: {:?}", request.method);
    };
    assert_eq!(params.text, "kubectl rollout restart deploy/web");
    assert!(
        state.overlay.is_none(),
        "overlay closes once the run starts"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ctrl_enter_executes_snippet_run() {
    let dir = with_temp_state_home("ctrl-enter-run");
    seed_snippet("deploy", "kubectl rollout restart deploy/web", &[]);
    let mut state = state();
    state.open_snippets_overlay(true);
    state.route_snippets_key(&key(KeyCode::Enter), &mut ClientShellInput::default());
    state.route_snippets_key(&key(KeyCode::Enter), &mut ClientShellInput::default());
    assert!(matches!(
        snippets_view(&state),
        super::super::snippets_overlay::ClientSnippetsView::RunConfirm(_)
    ));

    let mut outcome = ClientShellInput::default();
    state.route_snippets_key(
        &crate::input::TerminalKey::new(KeyCode::Enter, KeyModifiers::CONTROL),
        &mut outcome,
    );
    assert!(
        matches!(
            outcome.actions[..],
            [ClientShellAction::EndpointRequest { .. }]
        ),
        "ctrl+enter executes the run: {:?}",
        outcome.actions
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn palette_lists_snippet_and_import_actions() {
    let mut state = state();
    state.open_command_search();
    let run_row = palette_row_index(&state, "snippets:run");
    let mut outcome = ClientShellInput::default();
    state.activate_palette_item(run_row, &mut outcome);
    assert!(matches!(
        state.overlay,
        Some(ClientShellOverlay::Snippets(
            super::super::snippets_overlay::ClientSnippetsOverlay {
                pick_for_run: true,
                ..
            }
        ))
    ));
}
