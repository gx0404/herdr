use super::harness::*;

fn run_claude_hook(action: &str, hook_input: &str) -> Option<serde_json::Value> {
    run_shell_hook(
        "src/integration/assets/claude/herdr-agent-state.sh",
        &[action],
        hook_input,
    )
}

fn run_codex_hook(action: &str, hook_input: &str) -> Option<serde_json::Value> {
    run_shell_hook(
        "src/integration/assets/codex/herdr-agent-state.sh",
        &[action],
        hook_input,
    )
}

fn run_shell_hook(asset_path: &str, args: &[&str], hook_input: &str) -> Option<serde_json::Value> {
    run_shell_hook_with_env(asset_path, args, hook_input, &[])
}

fn run_shell_hook_with_env(
    asset_path: &str,
    args: &[&str],
    hook_input: &str,
    envs: &[(&str, &str)],
) -> Option<serde_json::Value> {
    let base = unique_test_dir();
    fs::create_dir_all(&base).unwrap();
    let socket_path = base.join("herdr.sock");
    let listener = UnixListener::bind(&socket_path).unwrap();

    let server = thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + Duration::from_millis(700);
        while Instant::now() < deadline {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let mut line = String::new();
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    reader.read_line(&mut line).unwrap();
                    let _ = stream.write_all(br#"{"id":"test","result":{"type":"ok"}}"#);
                    let _ = stream.write_all(b"\n");
                    let _ = stream.flush();
                    return Some(line);
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(err) => panic!("accept failed: {err}"),
            }
        }
        None
    });

    let hook_path = Path::new(env!("CARGO_MANIFEST_DIR")).join(asset_path);
    let mut command = Command::new("bash");
    command
        .arg(hook_path)
        .args(args)
        .env("HERDR_ENV", "1")
        .env("HERDR_SOCKET_PATH", &socket_path)
        .env("HERDR_PANE_ID", "p_test")
        .env_remove("CODEX_THREAD_ID")
        .env_remove("CURSOR_VERSION")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in envs {
        command.env(key, value);
    }
    let mut child = command.spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(hook_input.as_bytes()).unwrap();
    drop(stdin);

    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "hook failed: status={:?} stderr={} stdout={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );

    let request = server.join().unwrap();
    cleanup_test_base(&base);
    request.map(|line| serde_json::from_str(&line).unwrap())
}

#[test]
fn claude_hook_ignores_state_actions() {
    let subagent_input = r#"{"hook_event_name":"Notification","agent_id":"agent-abc123","agent_type":"Explore","notification_type":"permission_prompt"}"#;

    assert!(run_claude_hook("working", subagent_input).is_none());
    assert!(run_claude_hook("blocked", subagent_input).is_none());
}

#[test]
fn claude_hook_ignores_subagent_completion_reports() {
    let subagent_input =
        r#"{"hook_event_name":"SubagentStop","agent_id":"agent-abc123","agent_type":"Explore"}"#;

    assert!(run_claude_hook("working", subagent_input).is_none());
    assert!(run_claude_hook("idle", subagent_input).is_none());
    assert!(run_claude_hook("release", subagent_input).is_none());
}

#[test]
fn claude_hook_keeps_parent_agent_type_only_blocked() {
    let request = run_claude_hook(
        "blocked",
        r#"{"hook_event_name":"PermissionRequest","agent_type":"Explore"}"#,
    );

    assert!(request.is_none());
}

#[test]
fn claude_hook_reports_session_id_from_stdin() {
    let request = run_claude_hook(
        "session",
        r#"{"hook_event_name":"SessionStart","session_id":"claude-session"}"#,
    )
    .expect("session start should report session identity");

    assert_eq!(request["method"], "pane.report_agent_session");
    assert_eq!(request["params"]["agent_session_id"], "claude-session");
    assert!(request["params"].get("state").is_none());
}

#[test]
fn claude_hook_reports_subagent_and_task_activity() {
    // 载荷形状照 claude 2.1.278 的钩子 schema：Subagent* 必带被派生子 agent 的
    // agent_id，Task* 带 task_id；在子 agent 里触发的 Task* 另带调用方 agent_id。
    let cases = [
        (
            r#"{"hook_event_name":"SubagentStart","session_id":"s1","agent_id":"a1b2c3","agent_type":"Explore"}"#,
            "SubagentStart",
            Some("a1b2c3"),
        ),
        (
            r#"{"hook_event_name":"SubagentStop","session_id":"s1","agent_id":"a1b2c3","agent_type":"Explore","agent_transcript_path":"/tmp/agent-a1b2c3.jsonl","stop_hook_active":false}"#,
            "SubagentStop",
            Some("a1b2c3"),
        ),
        (
            r#"{"hook_event_name":"TaskCreated","session_id":"s1","agent_id":"a1b2c3","task_id":"7","task_subject":"demo"}"#,
            "TaskCreated",
            Some("task:7"),
        ),
        (
            r#"{"hook_event_name":"TaskCompleted","session_id":"s1","task_subject":"demo"}"#,
            "TaskCompleted",
            None,
        ),
    ];

    for (input, hint, node_id) in cases {
        let request = run_claude_hook("activity", input)
            .unwrap_or_else(|| panic!("{hint} should report activity"));
        assert_eq!(request["method"], "pane.report_agent_activity", "{hint}");
        let params = &request["params"];
        assert_eq!(params["pane_id"], "p_test");
        assert_eq!(params["source"], "herdr:claude");
        assert_eq!(params["agent"], "claude");
        assert_eq!(params["hint"], hint);
        assert!(params["seq"].as_u64().is_some(), "{hint}");
        assert_eq!(
            params.get("node_id").and_then(|v| v.as_str()),
            node_id,
            "{hint}"
        );
    }
}

#[test]
fn claude_hook_keeps_activity_and_session_reports_apart() {
    // activity 只认四个活动事件；session 仍只认根会话的 SessionStart。
    for input in [
        r#"{"hook_event_name":"SessionStart","session_id":"s1"}"#,
        r#"{"hook_event_name":"Stop","session_id":"s1"}"#,
        r#"{"hook_event_name":"future-event","agent_id":"a1"}"#,
    ] {
        assert!(run_claude_hook("activity", input).is_none(), "{input}");
    }
    assert!(run_claude_hook(
        "session",
        r#"{"hook_event_name":"SubagentStart","session_id":"s1","agent_id":"a1","agent_type":"Explore"}"#,
    )
    .is_none());
    assert!(run_claude_hook(
        "session",
        r#"{"hook_event_name":"SessionStart","session_id":"s1","agent_id":"a1"}"#,
    )
    .is_none());
}

#[test]
fn claude_hook_ignores_cursor_compatibility_payloads() {
    assert!(run_claude_hook(
        "session",
        r#"{"hook_event_name":"sessionStart","session_id":"cursor-session"}"#,
    )
    .is_none());

    assert!(run_claude_hook(
        "session",
        r#"{"hook_event_name":"SessionStart","session_id":"cursor-session","cursor_version":"2026.08.11-e8db854"}"#,
    )
    .is_none());

    for cursor_version in ["2026.08.11-e8db854", ""] {
        assert!(run_shell_hook_with_env(
            "src/integration/assets/claude/herdr-agent-state.sh",
            &["session"],
            r#"{"hook_event_name":"SessionStart","session_id":"cursor-session"}"#,
            &[("CURSOR_VERSION", cursor_version)],
        )
        .is_none());
    }
}

#[test]
fn codex_hook_reports_persisted_root_session_and_ignores_ephemeral_or_nested_sessions() {
    let request = run_codex_hook(
        "session",
        r#"{"hook_event_name":"SessionStart","session_id":"codex-session","transcript_path":"/tmp/codex-session.jsonl"}"#,
    )
    .expect("codex hook should report session identity");

    assert_eq!(request["method"], "pane.report_agent_session");
    assert_eq!(request["params"]["agent_session_id"], "codex-session");
    assert!(request["params"].get("state").is_none());

    let matching_request = run_shell_hook_with_env(
        "src/integration/assets/codex/herdr-agent-state.sh",
        &["session"],
        r#"{"hook_event_name":"SessionStart","session_id":"codex-session","transcript_path":"/tmp/codex-session.jsonl"}"#,
        &[("CODEX_THREAD_ID", "codex-session")],
    )
    .expect("matching inherited session should still report");
    assert_eq!(
        matching_request["params"]["agent_session_id"],
        "codex-session"
    );

    assert!(run_codex_hook(
        "session",
        r#"{"hook_event_name":"SessionStart","session_id":"side-session","transcript_path":null}"#,
    )
    .is_none());

    assert!(run_shell_hook_with_env(
        "src/integration/assets/codex/herdr-agent-state.sh",
        &["session"],
        r#"{"hook_event_name":"SessionStart","session_id":"nested-session","transcript_path":"/tmp/nested-session.jsonl"}"#,
        &[("CODEX_THREAD_ID", "parent-session")],
    )
    .is_none());
}
