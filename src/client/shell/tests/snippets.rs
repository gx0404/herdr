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
    state.route_snippets_key(&key(KeyCode::Enter), &mut outcome);
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
    state.route_snippets_key(&key(KeyCode::Enter), &mut outcome);
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
