//! 工具调用的统一单行展示：各家 harness 的工具名与参数形状各不相同，这里归一成
//! 「动词 + 对象」一行（`Edit src/app/state.rs`、`Shell cargo test`），供侧栏摘要、
//! Inspector、总览与通知正文共用。规则移植自 Paseo 的 `buildToolCallDisplayModel`
//! 与各 provider 的工具名映射，补上 Kimi / OpenCode / Pi 的工具名。

use serde_json::Value;

/// 工具的归一类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    Shell,
    Read,
    Write,
    Edit,
    Search,
    Fetch,
    Agent,
    Plan,
    /// 向用户提问的工具（Claude `AskUserQuestion`、Codex `request_user_input` 等）。
    Question,
    Other,
}

/// 一次工具调用的展示：名称（动词）与可选的对象摘要。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolDisplay {
    pub kind: ToolKind,
    pub name: String,
    pub summary: Option<String>,
}

impl ToolDisplay {
    /// 单行文本：`名称 摘要`，摘要压成一行。
    pub fn line(&self) -> String {
        match self
            .summary
            .as_deref()
            .filter(|summary| !summary.is_empty())
        {
            Some(summary) => format!("{} {}", self.name, summary),
            None => self.name.clone(),
        }
    }
}

const SHELL_NAMES: &[&str] = &[
    "Bash",
    "bash",
    "shell",
    "Shell",
    "PowerShell",
    "powershell",
    "exec_command",
    "exec",
    "command",
    "local_shell",
    "run_shell_command",
    "BashOutput",
];
const READ_NAMES: &[&str] = &[
    "Read",
    "read",
    "read_file",
    "ReadFile",
    "ReadMediaFile",
    "view_file",
    "view",
    "NotebookRead",
    "LS",
    "ls",
    "list",
];
const WRITE_NAMES: &[&str] = &["Write", "write", "write_file", "WriteFile", "create_file"];
const EDIT_NAMES: &[&str] = &[
    "Edit",
    "MultiEdit",
    "multi_edit",
    "edit",
    "apply_patch",
    "apply_diff",
    "patch",
    "str_replace_editor",
    "StrReplaceFile",
    "NotebookEdit",
    "replace",
];
const SEARCH_NAMES: &[&str] = &[
    "Grep",
    "grep",
    "Glob",
    "glob",
    "find",
    "search",
    "Search",
    "WebSearch",
    "web_search",
    "SearchWeb",
    "codebase_search",
];
const FETCH_NAMES: &[&str] = &[
    "WebFetch",
    "web_fetch",
    "webfetch",
    "WebFetchTool",
    "FetchURL",
    "fetch",
];
const AGENT_NAMES: &[&str] = &[
    "Task",
    "task",
    "Agent",
    "agent",
    "AgentSwarm",
    "spawn_agent",
    "subagent",
];
const PLAN_NAMES: &[&str] = &[
    "TodoWrite",
    "todowrite",
    "TodoList",
    "update_plan",
    "SetTodoList",
    "EnterPlanMode",
    "ExitPlanMode",
    "plan",
];
const QUESTION_NAMES: &[&str] = &["AskUserQuestion", "request_user_input", "question"];

/// 工具名 → 类别。
pub fn tool_kind(name: &str) -> ToolKind {
    let name = name.trim();
    let table: [(&[&str], ToolKind); 9] = [
        (SHELL_NAMES, ToolKind::Shell),
        (READ_NAMES, ToolKind::Read),
        (WRITE_NAMES, ToolKind::Write),
        (EDIT_NAMES, ToolKind::Edit),
        (SEARCH_NAMES, ToolKind::Search),
        (FETCH_NAMES, ToolKind::Fetch),
        (AGENT_NAMES, ToolKind::Agent),
        (PLAN_NAMES, ToolKind::Plan),
        (QUESTION_NAMES, ToolKind::Question),
    ];
    table
        .into_iter()
        .find(|(names, _)| names.contains(&name))
        .map(|(_, kind)| kind)
        .unwrap_or(ToolKind::Other)
}

/// 归一一次工具调用的展示。`cwd` 用于把绝对路径显示成相对路径。
pub fn display(name: &str, input: Option<&Value>, cwd: Option<&str>) -> ToolDisplay {
    let kind = tool_kind(name);
    let path = || input.and_then(file_path).map(|path| strip_cwd(&path, cwd));
    let (display_name, summary) = match kind {
        ToolKind::Shell => ("Shell".to_owned(), input.and_then(shell_command)),
        ToolKind::Read => ("Read".to_owned(), path()),
        ToolKind::Write => ("Write".to_owned(), path()),
        ToolKind::Edit => (
            "Edit".to_owned(),
            path().or_else(|| {
                input
                    .and_then(patch_paths)
                    .map(|path| strip_cwd(&path, cwd))
            }),
        ),
        ToolKind::Search => (
            "Search".to_owned(),
            input.and_then(|input| string_field(input, &["pattern", "query", "q", "glob"])),
        ),
        ToolKind::Fetch => (
            "Fetch".to_owned(),
            input.and_then(|input| string_field(input, &["url", "uri"])),
        ),
        ToolKind::Agent => (
            input
                .and_then(|input| string_field(input, &["subagent_type", "agent_type", "agent"]))
                .unwrap_or_else(|| "Agent".to_owned()),
            input.and_then(agent_summary),
        ),
        ToolKind::Plan => ("Plan".to_owned(), input.and_then(plan_summary)),
        ToolKind::Question => (
            "Ask".to_owned(),
            input.and_then(question).map(|question| question.text),
        ),
        ToolKind::Other => (humanize(name), None),
    };
    ToolDisplay {
        kind,
        name: display_name,
        summary: summary
            .map(|summary| super::text::collapse(&summary))
            .filter(|summary| !summary.is_empty()),
    }
}

/// 子 agent 的任务摘要：兼容 Codex 的 task_name / message，不展示加密正文。
fn agent_summary(input: &Value) -> Option<String> {
    ["description", "title", "task_name", "prompt", "message"]
        .iter()
        .find_map(|key| {
            let text = input.get(*key)?.as_str()?.trim();
            let encrypted = text.starts_with("gAAAA")
                && text.len() >= 24
                && text
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'='));
            if text.is_empty() || encrypted {
                return None;
            }
            let text = if *key == "message" {
                super::text::first_line(text)?
            } else {
                text
            };
            Some(text.to_owned())
        })
}

/// 工具名人性化：命名空间形式（`mcp__server__tool`、`a.b`、`a/b`、`a:b`）原样保留；
/// 其余按分隔符与驼峰边界拆词、首字母大写（`read_mcp_resource` → `Read mcp resource`，
/// `TaskOutput` → `Task output`）。
pub fn humanize(name: &str) -> String {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return name.to_owned();
    }
    if trimmed.contains("__") || trimmed.contains(['.', '/', ':']) {
        return trimmed.to_owned();
    }
    let words = trimmed
        .split(['_', '-', ' '])
        .filter(|word| !word.is_empty())
        .map(split_camel_case)
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    let mut chars = words.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => trimmed.to_owned(),
    }
}

/// 在小写（或数字）后接大写处断词：`ReadMediaFile` → `Read Media File`；连续大写
/// （`URL`、`LS`）不拆。
fn split_camel_case(word: &str) -> String {
    let mut out = String::with_capacity(word.len() + 4);
    let mut previous: Option<char> = None;
    for ch in word.chars() {
        if ch.is_uppercase()
            && previous.is_some_and(|previous| previous.is_lowercase() || previous.is_ascii_digit())
        {
            out.push(' ');
        }
        out.push(ch);
        previous = Some(ch);
    }
    out
}

/// 一个待答问题：问题正文与可选回答。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    pub text: String,
    pub options: Vec<String>,
}

/// 提问类工具的入参里第一个问题。各家共用同一形状：`{questions: [{question, header?,
/// options: [{label, description?}] | [string]}]}`（Claude `AskUserQuestion`、Codex
/// `request_user_input`、Kimi 交互请求、OpenCode `question`）；不止一个问题时正文注明
/// 余下的个数。
pub fn question(input: &Value) -> Option<Question> {
    let questions = input.get("questions")?.as_array()?;
    let first = questions.first()?;
    let text = string_field(first, &["question", "header", "text"])?;
    let options = first
        .get("options")
        .and_then(Value::as_array)
        .map(|options| {
            options
                .iter()
                .filter_map(|option| match option {
                    Value::String(label) => Some(label.trim().to_owned()),
                    _ => string_field(option, &["label", "value"]),
                })
                .filter(|label| !label.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let text = match questions.len() {
        0 | 1 => text,
        count => format!("{text} (+{})", count - 1),
    };
    Some(Question { text, options })
}

/// 绝对路径在 `cwd` 之下时显示相对部分（分隔符两种都认）。
pub fn strip_cwd(path: &str, cwd: Option<&str>) -> String {
    let Some(cwd) = cwd.map(|cwd| cwd.trim_end_matches(['/', '\\'])) else {
        return path.to_owned();
    };
    if cwd.is_empty() {
        return path.to_owned();
    }
    let normalize = |text: &str| text.replace('\\', "/");
    let (path_norm, cwd_norm) = (normalize(path), normalize(cwd));
    match path_norm.strip_prefix(&cwd_norm) {
        Some(rest) if rest.starts_with('/') && rest.len() > 1 => rest[1..].to_owned(),
        _ => path.to_owned(),
    }
}

/// 去掉 shell 包装：`bash -lc '…'`、`sh -c "…"`、`pwsh -Command …`、`cmd /c …`。
pub fn unwrap_shell(command: &str) -> String {
    let trimmed = command.trim();
    let wrappers: [(&[&str], &[&str]); 3] = [
        (
            &["bash", "sh", "zsh", "/bin/bash", "/bin/sh", "/bin/zsh"],
            &["-lc", "-c", "-ic"],
        ),
        (
            &["pwsh", "powershell", "pwsh.exe", "powershell.exe"],
            &["-Command", "-command", "-c", "-NoProfile -Command"],
        ),
        (&["cmd", "cmd.exe"], &["/c", "/C"]),
    ];
    for (programs, flags) in wrappers {
        for program in programs {
            for flag in flags {
                let prefix = format!("{program} {flag} ");
                if let Some(rest) = trimmed.strip_prefix(&prefix) {
                    return strip_quotes(rest.trim()).to_owned();
                }
            }
        }
    }
    trimmed.to_owned()
}

fn strip_quotes(text: &str) -> &str {
    for quote in ['\'', '"'] {
        if text.len() >= 2 && text.starts_with(quote) && text.ends_with(quote) {
            return &text[1..text.len() - 1];
        }
    }
    text
}

fn string_field(input: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        input
            .get(*key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    })
}

fn file_path(input: &Value) -> Option<String> {
    string_field(
        input,
        &[
            "file_path",
            "path",
            "filePath",
            "notebook_path",
            "filename",
            "file",
        ],
    )
}

fn shell_command(input: &Value) -> Option<String> {
    let command = match input.get("command").or_else(|| input.get("cmd")) {
        Some(Value::String(command)) => command.clone(),
        Some(Value::Array(parts)) => {
            let parts = parts.iter().filter_map(Value::as_str).collect::<Vec<_>>();
            // argv 形式的 `["bash", "-lc", "…"]`：直接取脚本本体。
            match parts.as_slice() {
                [shell, flag, script]
                    if matches!(*shell, "bash" | "sh" | "zsh" | "/bin/bash" | "/bin/sh")
                        && matches!(*flag, "-lc" | "-c" | "-ic") =>
                {
                    (*script).to_owned()
                }
                _ => parts.join(" "),
            }
        }
        _ => return None,
    };
    let command = unwrap_shell(&command);
    (!command.is_empty()).then_some(command)
}

/// `apply_patch` 的补丁文本（`input` 本身是字符串，或在 `input` / `patch` 字段里）。
fn patch_paths(input: &Value) -> Option<String> {
    let text = match input {
        Value::String(text) => text.as_str(),
        _ => input
            .get("input")
            .or_else(|| input.get("patch"))
            .and_then(Value::as_str)?,
    };
    let paths = text
        .lines()
        .filter_map(|line| {
            ["*** Update File: ", "*** Add File: ", "*** Delete File: "]
                .iter()
                .find_map(|marker| line.strip_prefix(marker))
        })
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .collect::<Vec<_>>();
    match paths.as_slice() {
        [] => None,
        [one] => Some((*one).to_owned()),
        [first, rest @ ..] => Some(format!("{first} (+{})", rest.len())),
    }
}

fn plan_summary(input: &Value) -> Option<String> {
    let items = input
        .get("todos")
        .or_else(|| input.get("plan"))
        .or_else(|| input.get("items"))?;
    match items {
        Value::Array(items) => {
            let done = items
                .iter()
                .filter(|item| {
                    item.get("status")
                        .and_then(Value::as_str)
                        .is_some_and(|status| matches!(status, "completed" | "done"))
                })
                .count();
            Some(format!("{done}/{}", items.len()))
        }
        Value::String(text) => text.lines().next().map(str::to_owned),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn line(name: &str, input: Value) -> String {
        display(name, Some(&input), Some("/src/herdr")).line()
    }

    #[test]
    fn shell_tools_show_the_unwrapped_command() {
        assert_eq!(
            line("Bash", json!({"command": "cargo test"})),
            "Shell cargo test"
        );
        assert_eq!(
            line(
                "exec_command",
                json!({"cmd": ["bash", "-lc", "npm run build"]})
            ),
            "Shell npm run build"
        );
        assert_eq!(
            line("shell", json!({"command": "bash -lc 'git status --short'"})),
            "Shell git status --short"
        );
        assert_eq!(
            line("Shell", json!({"command": "pwsh -Command Get-ChildItem"})),
            "Shell Get-ChildItem"
        );
        assert_eq!(
            line("Bash", json!({"command": "echo a\n  && echo b"})),
            "Shell echo a && echo b"
        );
    }

    #[test]
    fn file_tools_show_cwd_relative_paths() {
        assert_eq!(
            line("Read", json!({"file_path": "/src/herdr/src/main.rs"})),
            "Read src/main.rs"
        );
        assert_eq!(
            line("ReadFile", json!({"path": "C:\\src\\herdr\\Cargo.toml"})),
            "Read C:\\src\\herdr\\Cargo.toml"
        );
        assert_eq!(
            line("StrReplaceFile", json!({"path": "/src/herdr/src/ui.rs"})),
            "Edit src/ui.rs"
        );
        assert_eq!(
            line("write", json!({"filePath": "/tmp/out.txt"})),
            "Write /tmp/out.txt"
        );
        assert_eq!(
            display(
                "ReadFile",
                Some(&json!({"path": "C:\\src\\herdr\\Cargo.toml"})),
                Some("C:\\src\\herdr")
            )
            .line(),
            "Read Cargo.toml"
        );
    }

    #[test]
    fn apply_patch_lists_the_first_file_and_a_count() {
        let patch = "*** Begin Patch\n*** Update File: /src/herdr/src/a.rs\n@@\n*** Add File: src/b.rs\n*** End Patch";
        assert_eq!(
            line("apply_patch", json!({"input": patch})),
            "Edit src/a.rs (+1)"
        );
        assert_eq!(line("apply_patch", json!(patch)), "Edit src/a.rs (+1)");
    }

    #[test]
    fn search_fetch_agent_and_plan_tools() {
        assert_eq!(
            line("Grep", json!({"pattern": "focused_pane_id", "path": "src"})),
            "Search focused_pane_id"
        );
        assert_eq!(
            line("WebSearch", json!({"query": "ratatui 0.30"})),
            "Search ratatui 0.30"
        );
        assert_eq!(
            line("WebFetch", json!({"url": "https://example.com/a"})),
            "Fetch https://example.com/a"
        );
        assert_eq!(
            line(
                "Task",
                json!({"subagent_type": "Explore", "description": "检测模块"})
            ),
            "Explore 检测模块"
        );
        assert_eq!(line("Agent", json!({"prompt": "review"})), "Agent review");
        assert_eq!(
            line(
                "TodoWrite",
                json!({"todos": [{"status": "completed"}, {"status": "in_progress"}, {"status": "pending"}]})
            ),
            "Plan 1/3"
        );
    }

    #[test]
    fn unknown_tools_are_humanized_but_namespaced_names_stay_raw() {
        assert_eq!(line("read_mcp_resource", json!({})), "Read mcp resource");
        assert_eq!(
            line("mcp__github__create_issue", json!({})),
            "mcp__github__create_issue"
        );
        assert_eq!(line("browser.click", json!({})), "browser.click");
        assert_eq!(display("Think", None, None).kind, ToolKind::Other);
    }

    #[test]
    fn agent_tools_show_codex_task_names_and_the_first_message_line() {
        assert_eq!(
            line(
                "spawn_agent",
                json!({"task_name": "review", "message": "ignored", "agent_type": "worker"})
            ),
            "worker review"
        );
        assert_eq!(
            line(
                "spawn_agent",
                json!({"message": "\n  review code\nthen run tests"})
            ),
            "Agent review code"
        );
        let encrypted = "gAAAAABmFakeToken0123456789abcdefABCDEF_-==";
        for key in ["description", "title", "task_name", "prompt", "message"] {
            assert_eq!(line("spawn_agent", json!({key: encrypted})), "Agent");
        }
        assert_eq!(
            line(
                "spawn_agent",
                json!({"task_name": encrypted, "message": "fallback"})
            ),
            "Agent fallback"
        );
        for text in [
            "gAAAAshort",
            "gAAAA ordinary prose with spaces",
            "gAAAA1234567890123456789+",
        ] {
            assert_eq!(
                line("spawn_agent", json!({"message": text})),
                format!("Agent {text}")
            );
        }
    }

    #[test]
    fn multi_agent_controls_keep_their_own_verbs() {
        for (name, expected) in [
            ("wait_agent", "Wait agent"),
            ("send_input", "Send input"),
            ("send_message", "Send message"),
            ("followup_task", "Followup task"),
            ("interrupt_agent", "Interrupt agent"),
            ("resume_agent", "Resume agent"),
            ("close_agent", "Close agent"),
            ("list_agents", "List agents"),
        ] {
            let display = display(
                name,
                Some(&json!({"agent_type": "worker", "message": "ignored"})),
                None,
            );
            assert_eq!(display.kind, ToolKind::Other);
            assert_eq!(display.line(), expected);
        }
    }

    #[test]
    fn camel_case_names_are_split_into_words() {
        assert_eq!(humanize("TaskOutput"), "Task output");
        assert_eq!(humanize("WaitFor"), "Wait for");
        assert_eq!(humanize("FetchURL"), "Fetch url");
        assert_eq!(humanize("v2Tool"), "V2 tool");
        assert_eq!(line("Skill", json!({"skill": "review"})), "Skill");
    }

    /// 本机各家转录里实测的工具名（只核对名字与参数键，Claude / Kimi 的形状一致）。
    #[test]
    fn harness_specific_tool_names_map_to_their_kind() {
        assert_eq!(
            line("PowerShell", json!({"command": "Get-ChildItem"})),
            "Shell Get-ChildItem"
        );
        assert_eq!(
            line(
                "ReadMediaFile",
                json!({"path": "/src/herdr/assets/logo.png"})
            ),
            "Read assets/logo.png"
        );
        assert_eq!(
            line("TodoList", json!({"todos": [{"status": "done"}]})),
            "Plan 1/1"
        );
        assert_eq!(line("find", json!({"pattern": "*.rs"})), "Search *.rs");
        assert_eq!(
            line("AgentSwarm", json!({"description": "review shards"})),
            "Agent review shards"
        );
    }

    #[test]
    fn question_tools_show_their_first_question() {
        let input = json!({"questions": [
            {"header": "Retry", "question": "Retry on 429?", "multiSelect": false,
             "options": [{"label": "Yes", "description": "retry"}, {"label": "No"}]},
            {"header": "Other", "question": "Second?", "options": []}
        ]});
        assert_eq!(
            question(&input),
            Some(Question {
                text: "Retry on 429? (+1)".into(),
                options: vec!["Yes".into(), "No".into()],
            })
        );
        assert_eq!(
            line("AskUserQuestion", input.clone()),
            "Ask Retry on 429? (+1)"
        );
        assert_eq!(
            question(&json!({"questions": [{"question": "Pick", "options": ["a", " ", "b"]}]}))
                .map(|question| question.options),
            Some(vec!["a".to_owned(), "b".to_owned()])
        );
        assert_eq!(question(&json!({"questions": []})), None);
        assert_eq!(question(&json!({"questions": [{"options": []}]})), None);
        assert_eq!(question(&json!("not an object")), None);
        assert_eq!(display("request_user_input", None, None).line(), "Ask");
    }

    #[test]
    fn missing_input_falls_back_to_the_verb() {
        assert_eq!(display("Read", None, None).line(), "Read");
        assert_eq!(
            display("Bash", Some(&json!({"command": "   "})), None).line(),
            "Shell"
        );
    }
}
