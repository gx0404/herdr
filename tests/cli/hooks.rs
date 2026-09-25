use super::harness::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

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

/// 等钩子请求的兜底上限：判定「没有上报」看的是钩子已经退出，不是时间（见
/// `run_shell_hook_with_env`）；这个上限只防主线程永远等不到退出标记，正常用不到。
const HOOK_SAFETY_WAIT: Duration = Duration::from_secs(60);

/// 收下 `stream` 上的一行请求并回一行 ok。
fn answer_hook_request(mut stream: UnixStream) -> String {
    let mut line = String::new();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    reader.read_line(&mut line).unwrap();
    let _ = stream.write_all(br#"{"id":"test","result":{"type":"ok"}}"#);
    let _ = stream.write_all(b"\n");
    let _ = stream.flush();
    line
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
    // 钩子进程已退出（主线程等到它结束后置位）。
    let hook_exited = Arc::new(AtomicBool::new(false));
    let server_hook_exited = Arc::clone(&hook_exited);

    // T1：以前只收 700 ms，负载 25–35 时 bash 与 python 起得慢，要上报的钩子还没
    // 连上就被判成「没有请求」。钩子里的 python 同步发请求，connect 在进程退出前就已
    // 完成、连接留在 listener 的 backlog 里，所以改为收到钩子退出为止：先读退出标记
    // 再 accept，标记为真时这次 accept 一定能拿到退出前入队的连接，拿不到就是没有
    // 上报。判定与负载无关。
    let server = thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + HOOK_SAFETY_WAIT;
        loop {
            let exited = server_hook_exited.load(Ordering::Acquire);
            match listener.accept() {
                Ok((stream, _)) => return Some(answer_hook_request(stream)),
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    if exited || Instant::now() >= deadline {
                        return None;
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Err(err) => panic!("accept failed: {err}"),
            }
        }
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
        // 在 Claude Code 后台会话里跑测试时这些变量会从外层继承进来，claude 钩子
        // 看到它们就不上报；清掉，让用例只看自己显式给的环境。
        .env_remove("CLAUDE_CODE_SESSION_KIND")
        .env_remove("CLAUDE_JOB_DIR")
        .env_remove("HERDR_REPORT_BG_SESSIONS")
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
    hook_exited.store(true, Ordering::Release);
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
fn claude_hook_skips_background_sessions_unless_opted_in() {
    // Claude Code daemon 托管的后台会话从启动 daemon 的窗格继承了 HERDR_PANE_ID /
    // HERDR_SOCKET_PATH。daemon 给它们设 CLAUDE_CODE_SESSION_KIND=bg 与
    // CLAUDE_JOB_DIR；Claude Code 2.1.281 交给钩子的环境里只剩后者，两个标记分别
    // 都要能挡住 session 与 activity 两种上报。
    let session_input = r#"{"hook_event_name":"SessionStart","session_id":"bg-session"}"#;
    let activity_input = r#"{"hook_event_name":"SubagentStart","session_id":"bg-session","agent_id":"a1","agent_type":"Explore"}"#;
    let markers: [(&str, &str); 2] = [
        ("CLAUDE_CODE_SESSION_KIND", "bg"),
        ("CLAUDE_JOB_DIR", "/home/user/.claude/jobs/0123abcd"),
    ];

    for marker in markers {
        for (action, input) in [("session", session_input), ("activity", activity_input)] {
            for report_bg in [None, Some("0"), Some("")] {
                let mut envs = vec![marker];
                if let Some(value) = report_bg {
                    envs.push(("HERDR_REPORT_BG_SESSIONS", value));
                }
                assert!(
                    run_shell_hook_with_env(
                        "src/integration/assets/claude/herdr-agent-state.sh",
                        &[action],
                        input,
                        &envs,
                    )
                    .is_none(),
                    "{marker:?} {action} report_bg={report_bg:?} must not report"
                );
            }

            // 逃生开关：HERDR_REPORT_BG_SESSIONS=1 时照常上报。
            let request = run_shell_hook_with_env(
                "src/integration/assets/claude/herdr-agent-state.sh",
                &[action],
                input,
                &[marker, ("HERDR_REPORT_BG_SESSIONS", "1")],
            )
            .unwrap_or_else(|| panic!("{marker:?} {action} should report when opted in"));
            assert_eq!(
                request["params"]["pane_id"], "p_test",
                "{marker:?} {action}"
            );
            match action {
                "session" => {
                    assert_eq!(request["method"], "pane.report_agent_session");
                    assert_eq!(request["params"]["agent_session_id"], "bg-session");
                }
                _ => {
                    assert_eq!(request["method"], "pane.report_agent_activity");
                    assert_eq!(request["params"]["node_id"], "a1");
                }
            }
        }
    }

    // 标记之外的值不算后台会话：其它 session kind 与空的 CLAUDE_JOB_DIR 照常上报。
    for marker in [
        ("CLAUDE_CODE_SESSION_KIND", "interactive"),
        ("CLAUDE_JOB_DIR", ""),
    ] {
        let request = run_shell_hook_with_env(
            "src/integration/assets/claude/herdr-agent-state.sh",
            &["session"],
            session_input,
            &[marker],
        )
        .unwrap_or_else(|| panic!("{marker:?} is not a background session"));
        assert_eq!(request["method"], "pane.report_agent_session");
    }
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
    assert_eq!(
        request["params"]["agent_session_path"],
        "/tmp/codex-session.jsonl"
    );
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

#[test]
fn codex_hook_forwards_pane_home_paths_on_resume_and_fork() {
    for (source, id, suffix) in [
        ("resume", "root", ".jsonl"),
        ("fork", "forked", ".jsonl.zst"),
    ] {
        let path = format!(
            "/pane codex home/sessions/2026/09/25/rollout-2026-09-25T11-22-33-{id}{suffix}"
        );
        let payload = serde_json::json!({ "hook_event_name": "SessionStart", "session_id": id, "source": source, "transcript_path": path });
        let request = run_shell_hook_with_env(
            "src/integration/assets/codex/herdr-agent-state.sh",
            &["session"],
            &payload.to_string(),
            &[("CODEX_HOME", "/pane codex home")],
        )
        .expect("session report");
        assert_eq!(request["params"]["agent_session_id"], id);
        assert_eq!(request["params"]["agent_session_path"], path);
        assert_eq!(request["params"]["session_start_source"], source);
    }
}

#[test]
fn codex_hook_reports_subagent_thread_activity() {
    // 载荷形状照 codex-cli 0.155.1 内嵌的钩子 schema：SubagentStart / SubagentStop 必带
    // 子线程 id agent_id；SubagentStop 另带 agent_transcript_path 与 last_assistant_message。
    let cases = [
        (
            r#"{"hook_event_name":"SubagentStart","session_id":"root-thread","agent_id":"01a0c6d8-4f60-7000-8000-0000000000a2","agent_type":"worker","cwd":"/work","model":"demo","permission_mode":"default","transcript_path":"/tmp/root-thread.jsonl","turn_id":"turn-1"}"#,
            "SubagentStart",
            Some("01a0c6d8-4f60-7000-8000-0000000000a2"),
        ),
        (
            r#"{"hook_event_name":"SubagentStop","session_id":"root-thread","agent_id":"01a0c6d8-4f60-7000-8000-0000000000a2","agent_type":"worker","agent_transcript_path":"/tmp/child.jsonl","last_assistant_message":null,"stop_hook_active":false,"cwd":"/work","model":"demo","permission_mode":"default","transcript_path":"/tmp/root-thread.jsonl","turn_id":"turn-1"}"#,
            "SubagentStop",
            Some("01a0c6d8-4f60-7000-8000-0000000000a2"),
        ),
        // 缺 agent_id 也照样提示「树变了」，只是没有节点 id。
        (
            r#"{"hook_event_name":"SubagentStop","session_id":"root-thread"}"#,
            "SubagentStop",
            None,
        ),
    ];

    for (input, hint, node_id) in cases {
        let request = run_codex_hook("activity", input)
            .unwrap_or_else(|| panic!("{hint} should report activity"));
        assert_eq!(request["method"], "pane.report_agent_activity", "{hint}");
        let params = &request["params"];
        assert_eq!(params["pane_id"], "p_test");
        assert_eq!(params["source"], "herdr:codex");
        assert_eq!(params["agent"], "codex");
        assert_eq!(params["hint"], hint);
        assert!(params["seq"].as_u64().is_some(), "{hint}");
        assert_eq!(
            params.get("node_id").and_then(|v| v.as_str()),
            node_id,
            "{hint}"
        );
        assert!(params.get("agent_session_id").is_none(), "{hint}");
    }

    // 嵌套 codex 进程里的活动提示照发：最多让同一棵树多刷一次，不像会话身份那样会认错。
    let nested = run_shell_hook_with_env(
        "src/integration/assets/codex/herdr-agent-state.sh",
        &["activity"],
        r#"{"hook_event_name":"SubagentStart","session_id":"nested-thread","agent_id":"child-1","agent_type":"worker"}"#,
        &[("CODEX_THREAD_ID", "parent-thread")],
    )
    .expect("nested activity should still hint");
    assert_eq!(nested["params"]["hint"], "SubagentStart");
    assert_eq!(nested["params"]["node_id"], "child-1");
}

#[test]
fn codex_hook_keeps_activity_and_session_reports_apart() {
    // activity 只认两个子 agent 事件；session 仍只认 SessionStart。
    for input in [
        r#"{"hook_event_name":"SessionStart","session_id":"root-thread","transcript_path":"/tmp/root-thread.jsonl"}"#,
        r#"{"hook_event_name":"SessionEnd","session_id":"root-thread","reason":"exit"}"#,
        r#"{"hook_event_name":"Stop","session_id":"root-thread"}"#,
        r#"{"hook_event_name":"future-event","agent_id":"child-1"}"#,
    ] {
        assert!(run_codex_hook("activity", input).is_none(), "{input}");
    }
    assert!(run_codex_hook(
        "session",
        r#"{"hook_event_name":"SubagentStart","session_id":"root-thread","agent_id":"child-1","agent_type":"worker","transcript_path":"/tmp/root-thread.jsonl"}"#,
    )
    .is_none());
}
