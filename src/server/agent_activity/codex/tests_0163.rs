//! Codex 0.163 的合成记录回归；不读取本机真实会话，也不修改冻结夹具。

use super::*;
use serde_json::json;
use std::io::Cursor;

const ENCRYPTED: &str = "gAAAAABmFakeToken0123456789abcdefABCDEF_-==";

fn render(payload: Value) -> Option<String> {
    render_record(
        &json!({"type": "response_item", "payload": payload}).to_string(),
        None,
    )
}

fn call(name: &str, arguments: Value) -> Option<String> {
    render(json!({"type": "function_call", "name": name, "arguments": arguments.to_string()}))
}

fn lifecycle(records: &[Value], tail_bytes: u64) -> Lifecycle {
    let text = records
        .iter()
        .map(|record| format!("{record}\n"))
        .collect::<String>();
    lifecycle_from_reader(
        &mut Cursor::new(text.as_bytes()),
        text.len() as u64,
        tail_bytes,
    )
    .expect("合成尾部可读")
}

fn node_with(lifecycle: &Lifecycle) -> AgentActivityNode {
    let meta = parse_meta(
        &json!({"id": "synthetic-child", "agent_path": "/root/task"}),
        Path::new("synthetic.jsonl"),
    )
    .expect("合成元数据合法");
    let mut node = node_from_meta(&meta, None);
    apply_lifecycle(&mut node, lifecycle);
    node
}

#[test]
fn bookkeeping_and_additional_tools_stay_hidden_at_both_record_sizes() {
    for kind in BOOKKEEPING_RECORDS {
        let text = format!(
            r#"{{"timestamp":"2026-10-21T00:00:00Z","ordinal":9,"type":"{kind}","payload":{{"data":"{}"}}}}"#,
            "x".repeat(600)
        );
        assert_eq!(render_record(&text, None), None, "{kind}");
        assert_eq!(
            render_oversized(&text.as_bytes()[..256], None),
            None,
            "{kind}"
        );
    }
    for kind in HIDDEN_RESPONSE_ITEMS {
        let text = format!(
            r#"{{"timestamp":"2026-10-21T00:00:00Z","ordinal":9,"type":"response_item","payload":{{"type":"{kind}","tools":"{}"}}}}"#,
            "x".repeat(600)
        );
        assert_eq!(render_record(&text, None), None);
        assert_eq!(render_oversized(&text.as_bytes()[..256], None), None);
    }
}

#[test]
fn inter_agent_communication_shows_participants_and_hides_encrypted_content() {
    let cases = [
        (
            json!({"author": " Ada ", "recipient": "Bo", "content": "hello\n  world"}),
            "[message from Ada → Bo] hello world",
        ),
        (
            json!({"author": "Ada", "content": "hello"}),
            "[message from Ada] hello",
        ),
        (
            json!({"recipient": "Bo", "content": "hello"}),
            "[message] hello",
        ),
        (json!({"author": " \t", "recipient": "Bo"}), "[message]"),
        (
            json!({"author": "Ada", "recipient": "Bo", "content": ENCRYPTED}),
            "[message from Ada → Bo] (encrypted)",
        ),
        (
            json!({"author": "Ada", "recipient": "Bo", "content": "gAAAA is ordinary prose"}),
            "[message from Ada → Bo] gAAAA is ordinary prose",
        ),
    ];
    for (payload, expected) in cases {
        let record = json!({"type": "inter_agent_communication", "payload": payload});
        assert_eq!(
            render_record(&record.to_string(), None).as_deref(),
            Some(expected)
        );
    }
    // response_item 的旧格式仍然只写发送方，recipient 不改变其转写。
    assert_eq!(
        render(
            json!({"type": "agent_message", "author": "Ada", "recipient": "Bo",
                      "content": [{"type": "input_text", "text": "hello"}]})
        )
        .as_deref(),
        Some("[message from Ada] hello")
    );
}

#[test]
fn inter_agent_communication_clips_unicode_and_placeholds_oversized_records() {
    let author = "作".repeat(NAME_CHARS + 1);
    let recipient = "收".repeat(NAME_CHARS + 1);
    let content = "文".repeat(MESSAGE_CHARS + 1);
    let record = json!({"type": "inter_agent_communication",
                        "payload": {"author": author, "recipient": recipient, "content": content}});
    assert_eq!(
        render_record(&record.to_string(), None),
        Some(format!(
            "[message from {}… → {}…] {}…",
            "作".repeat(NAME_CHARS),
            "收".repeat(NAME_CHARS),
            "文".repeat(MESSAGE_CHARS)
        ))
    );
    let text = format!(
        r#"{{"timestamp":"2026-10-21T00:00:00Z","ordinal":9,"type":"inter_agent_communication","payload":{{"content":"{}"}}}}"#,
        "x".repeat(600)
    );
    assert_eq!(
        render_oversized(&text.as_bytes()[..256], None).as_deref(),
        Some(OVERSIZED_PLACEHOLDER)
    );
    assert_eq!(render_oversized(&text.as_bytes()[..256], Some(10)), None);
    assert_eq!(render_record(&text, Some(10)), None);
}

#[test]
fn namespaced_tools_keep_their_display_name_and_match_summaries_by_bare_name() {
    for (namespace, name, expected) in [
        (
            "mcp__codex_apps__github",
            "_fetch_file",
            "mcp__codex_apps__github___fetch_file",
        ),
        ("mcp__node_repl", "js", "mcp__node_repl__js"),
        ("clock", "sleep", "clock.sleep"),
        ("multi_agent_v1", "send_input", "send_input"),
        ("collaboration", "send_input", "send_input"),
        (" \t", "send_input", "send_input"),
    ] {
        let payload = json!({"type": "function_call", "namespace": namespace,
                             "name": name, "arguments": r#"{"target":"worker","message":"hello"}"#});
        assert_eq!(tool_name(&payload), (expected.to_owned(), name.to_owned()));
        let expected_line = if name == "send_input" {
            format!("[{expected}] → worker: hello")
        } else {
            format!("[{expected}] {{\"message\":\"hello\",\"target\":\"worker\"}}")
        };
        assert_eq!(render(payload).as_deref(), Some(expected_line.as_str()));
    }
    assert_eq!(
        tool_name(&json!({"name": " Read "})),
        ("Read".into(), "Read".into())
    );
    assert_eq!(tool_name(&json!({})), ("tool".into(), "tool".into()));
    assert_eq!(
        render(
            json!({"type": "custom_tool_call", "namespace": "mcp__node_repl",
                             "name": "js", "input": "const a = 1;"})
        )
        .as_deref(),
        Some("[mcp__node_repl__js] const a = 1;")
    );
    let payload = json!({"type": "function_call", "namespace": "n".repeat(NAME_CHARS),
                         "name": "tool", "arguments": "{}"});
    assert_eq!(
        render(payload),
        Some(format!(
            "[{}…]",
            format!("{}.tool", "n".repeat(NAME_CHARS))
                .chars()
                .take(NAME_CHARS)
                .collect::<String>()
        ))
    );
}

#[test]
fn encrypted_function_arguments_have_a_placeholder_without_exposing_tokens() {
    for arguments in [
        None,
        Some(Value::Null),
        Some(json!("")),
        Some(json!(" \n")),
        Some(json!("{}")),
    ] {
        let mut payload = json!({"type": "function_call", "name": "exec_command",
                                 "encrypted_function_args": ENCRYPTED});
        if let Some(arguments) = arguments {
            payload["arguments"] = arguments;
        }
        assert_eq!(
            render(payload).as_deref(),
            Some("[exec_command] (encrypted)")
        );
    }
    assert_eq!(
        render(json!({"type": "function_call", "name": "exec_command",
                             "arguments": r#"{"cmd":"cargo test"}"#,
                             "encrypted_function_args": ENCRYPTED}))
        .as_deref(),
        Some("[exec_command] cargo test")
    );
    assert_eq!(
        render(json!({"type": "function_call", "name": "list_agents",
                             "arguments": r#"{"other":"ignored"}"#,
                             "encrypted_function_args": ENCRYPTED}))
        .as_deref(),
        Some("[list_agents]"),
        "非空明文参数不能被加密占位覆盖"
    );
}

#[test]
fn spawn_and_control_tools_summarise_codex_v1_and_v2_arguments() {
    for (name, arguments, expected) in [
        (
            "spawn_agent",
            json!({"task_name": "review", "message": "ignored", "agent_type": "worker", "model": "gpt-test"}),
            "[spawn_agent] review · worker · gpt-test",
        ),
        (
            "spawn_agent",
            json!({"task_name": " ", "message": "\n first task\nsecond line", "agent_type": "explorer", "model": "gpt-test"}),
            "[spawn_agent] first task · explorer · gpt-test",
        ),
        (
            "spawn_agent",
            json!({"message": ENCRYPTED, "model": "gpt-test"}),
            "[spawn_agent] (encrypted) · gpt-test",
        ),
        (
            "spawn_agent",
            json!({"items": [{"type": "image", "text": "skip"}, {"type": "text", "text": " "}, {"type": "text", "text": "first\npart"}, {"type": "text", "text": "ignored"}]}),
            "[spawn_agent] first part",
        ),
        (
            "spawn_agent",
            json!({"items": [{"type": "text", "text": ENCRYPTED}]}),
            "[spawn_agent] (encrypted)",
        ),
        (
            "spawn_agent",
            json!({"agent_type": "worker", "model": "gpt-test"}),
            "[spawn_agent] worker · gpt-test",
        ),
        ("spawn_agent", json!({}), "[spawn_agent]"),
        (
            "list_agents",
            json!({"path_prefix": " /root/review "}),
            "[list_agents] → /root/review",
        ),
        ("list_agents", json!({"other": "ignored"}), "[list_agents]"),
        (
            "send_input",
            json!({"target": "worker", "items": [{"type": "text", "text": "run\n tests"}, {"type": "text", "text": "ignored"}]}),
            "[send_input] → worker: run tests",
        ),
        (
            "send_input",
            json!({"target": "worker", "items": [{"type": "text", "text": ENCRYPTED}]}),
            "[send_input] → worker: (encrypted)",
        ),
        (
            "send_input",
            json!({"target": "worker", "message": "preferred", "items": [{"type": "text", "text": "ignored"}]}),
            "[send_input] → worker: preferred",
        ),
        (
            "wait_agent",
            json!({"targets": ["a", "b"], "timeout_ms": 30}),
            "[wait_agent] → a, b",
        ),
        (
            "resume_agent",
            json!({"id": "worker"}),
            "[resume_agent] → worker",
        ),
        (
            "close_agent",
            json!({"target": "worker"}),
            "[close_agent] → worker",
        ),
    ] {
        assert_eq!(call(name, arguments).as_deref(), Some(expected));
    }
    let text = call(
        "spawn_agent",
        json!({"message": "文".repeat(200), "model": "gpt-test"}),
    )
    .expect("有摘要");
    assert!(text.ends_with('…'));
    assert_eq!(
        text.strip_prefix("[spawn_agent] ")
            .expect("调用前缀")
            .chars()
            .count(),
        SUMMARY_CHARS + 1
    );
}

#[test]
fn insight_current_tool_uses_composed_names_and_agent_message_summaries() {
    for (namespace, name, arguments, expected) in [
        (
            "mcp__codex_apps__github",
            "_fetch_file",
            json!({}),
            "mcp__codex_apps__github___fetch_file",
        ),
        ("mcp__node_repl", "js", json!({}), "mcp__node_repl__js"),
        ("clock", "sleep", json!({}), "clock.sleep"),
        (
            "multi_agent_v1",
            "spawn_agent",
            json!({"agent_type": "worker", "message": "\nreview src\nthen tests"}),
            "worker review src",
        ),
        ("collaboration", "wait_agent", json!({}), "Wait agent"),
    ] {
        let mut insight = CodexInsight::default();
        insight.record(&json!({"type": "response_item", "payload": {
            "type": "function_call", "namespace": namespace, "name": name,
            "call_id": "call", "arguments": arguments.to_string()
        }}));
        assert_eq!(
            insight
                .finish()
                .expect("有工具洞察")
                .current_tool
                .as_deref(),
            Some(expected)
        );
    }
}

#[test]
fn running_summary_uses_last_tool_and_normalizes_each_input_shape() {
    let started = json!({"type": "event_msg", "payload": {"type": "task_started"}});
    let cwd = json!({"type": "turn_context", "payload": {"cwd": "/src/herdr"}});
    for (payload, expected) in [
        (
            json!({"type": "function_call", "name": "Read", "arguments": r#"{"path":"/src/herdr/src/main.rs"}"#}),
            "Read src/main.rs",
        ),
        (
            json!({"type": "function_call", "name": "exec_command", "arguments": {"cmd": "bash -lc 'cargo test'"}}),
            "Shell cargo test",
        ),
        (
            json!({"type": "custom_tool_call", "name": "exec", "input": "cargo test"}),
            "Shell cargo test",
        ),
        (
            json!({"type": "custom_tool_call", "name": "apply_patch", "input": "*** Begin Patch\n*** Update File: /src/herdr/src/main.rs\n*** End Patch"}),
            "Edit src/main.rs",
        ),
        (
            json!({"type": "local_shell_call", "action": {"command": ["bash", "-lc", "cargo test"]}}),
            "Shell cargo test",
        ),
        (
            json!({"type": "function_call", "namespace": "mcp__codex_apps__github", "name": "_fetch_file", "arguments": "{}"}),
            "mcp__codex_apps__github___fetch_file",
        ),
    ] {
        let records = [
            cwd.clone(),
            started.clone(),
            json!({"type": "response_item", "payload": {"type": "function_call", "name": "old_tool"}}),
            json!({"type": "response_item", "payload": payload}),
            json!({"type": "response_item", "payload": {"type": "function_call_output", "output": "done"}}),
        ];
        let state = lifecycle(&records, STATUS_TAIL_BYTES);
        let node = node_with(&state);
        assert_eq!(node.status, AgentActivityStatus::Running);
        assert_eq!(node.summary.as_deref(), Some(expected));
    }
}

#[test]
fn completed_and_running_summaries_are_bounded_and_reset_on_a_new_turn() {
    let started = json!({"type": "event_msg", "payload": {"type": "task_started"}});
    let call = json!({"type": "response_item", "payload": {"type": "function_call", "name": "exec_command",
                       "arguments": json!({"cmd": "文".repeat(200)}).to_string()}});
    let done = json!({"type": "event_msg", "payload": {"type": "task_complete", "completed_at": 10,
                       "last_agent_message": format!("\n  {}\nsecond line", "文".repeat(200))}});
    for records in [
        vec![started.clone(), call.clone()],
        vec![started.clone(), call.clone(), done.clone()],
    ] {
        let node = node_with(&lifecycle(&records, STATUS_TAIL_BYTES));
        let summary = node.summary.expect("有摘要");
        assert_eq!(summary.chars().count(), SUMMARY_CHARS);
        assert!(summary.ends_with('…'));
        assert!(!summary.contains("second line"));
    }
    let records = [started.clone(), call, done, started];
    let node = node_with(&lifecycle(&records, STATUS_TAIL_BYTES));
    assert_eq!(node.status, AgentActivityStatus::Running);
    assert_eq!(node.summary.as_deref(), Some("/root/task"));
}

#[test]
fn lifecycle_summary_preserves_failures_and_falls_back_when_content_is_missing() {
    for (event, status, expected) in [
        (
            json!({"type": "task_started"}),
            AgentActivityStatus::Running,
            "/root/task",
        ),
        (
            json!({"type": "task_complete", "last_agent_message": " \n "}),
            AgentActivityStatus::Done,
            "/root/task",
        ),
        (
            json!({"type": "task_complete", "last_agent_message": "\n  finished  work\nmore"}),
            AgentActivityStatus::Done,
            "finished work",
        ),
        (
            json!({"type": "task_complete", "error": {"message": "failed reason"}, "last_agent_message": "ignored"}),
            AgentActivityStatus::Failed,
            "failed reason",
        ),
        (
            json!({"type": "turn_aborted", "reason": "user interrupted"}),
            AgentActivityStatus::Failed,
            "user interrupted",
        ),
        (
            json!({"type": "turn_aborted"}),
            AgentActivityStatus::Failed,
            "interrupted",
        ),
    ] {
        let state = lifecycle(
            &[json!({"type": "event_msg", "payload": event})],
            STATUS_TAIL_BYTES,
        );
        let node = node_with(&state);
        assert_eq!(node.status, status);
        assert_eq!(node.summary.as_deref(), Some(expected));
    }
}

#[test]
fn marker_free_tail_only_uses_tools_inside_the_same_read_window() {
    let last_call = json!({"type": "response_item", "payload": {"type": "function_call",
                           "name": "Read", "arguments": r#"{"path":"src/lib.rs"}"#}});
    let tail = format!("\n{last_call}\n");
    let text = format!("{}\n{tail}", "old context".repeat(100));
    let state = lifecycle_from_reader(
        &mut Cursor::new(text.as_bytes()),
        text.len() as u64,
        tail.len() as u64,
    )
    .expect("有界尾部可读");
    assert_eq!(
        node_with(&state).summary.as_deref(),
        Some("Read src/lib.rs")
    );

    let tail = "\n{\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\"}}\n";
    let text = format!("{last_call}\n{}\n{tail}", "old context".repeat(100));
    let state = lifecycle_from_reader(
        &mut Cursor::new(text.as_bytes()),
        text.len() as u64,
        tail.len() as u64,
    )
    .expect("有界尾部可读");
    assert_eq!(node_with(&state).summary.as_deref(), Some("/root/task"));
}
