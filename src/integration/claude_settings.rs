use std::collections::HashSet;
use std::io;
use std::path::Path;

use jsonc_parser::ast::{Array as AstArray, Object as AstObject, Value as AstValue};
use jsonc_parser::common::Ranged;
use jsonc_parser::cst::{CstInputValue, CstNode, CstObject, CstRootNode};
use jsonc_parser::{parse_to_ast, CollectOptions, ParseOptions};
use serde_json::{Map, Value};

use super::command::hook_command;
use super::config_edit::{
    ensure_hooks_object, hook_command_variants, hooks_object_if_present, is_matching_command_hook,
};

// Claude's documented SessionStart sources. Third-party tools that import Claude
// hooks may fire other source names; filter before an unnecessary hook process
// starts.
const SESSION_START_MATCHER: &str = "^(startup|resume|clear|compact|fork)$";

/// `Notification` 只取两种等用户处理的通知：权限对话框（`permission_prompt`）与 MCP
/// 表单（`elicitation_dialog`）。claude 在对话框出现后、用户 6 秒没有操作时才发它们。
const NOTIFICATION_MATCHER: &str = "^(permission_prompt|elicitation_dialog)$";

/// herdr 写进 claude settings 的每条钩子的超时（秒）。
const HOOK_TIMEOUT_SECS: u64 = 10;

/// herdr 安装的一条钩子：事件、脚本动作、matcher（`None` 表示该事件不支持
/// matcher，条目里不写这个键），以及是否放到后台跑（`background` 写成
/// `"async": true`：claude 不等它结束，也不读它的输出）。
struct HookInstall {
    event: &'static str,
    action: &'static str,
    matcher: Option<&'static str>,
    background: bool,
}

static SESSION_HOOK: HookInstall = HookInstall {
    event: "SessionStart",
    action: "session",
    matcher: Some(SESSION_START_MATCHER),
    background: false,
};

/// 待处理事项详情钩子，只补 needs-input 的说明（`pane.report_agent_attention`）：
/// claude 的状态仍由屏幕检测决定，详情只在 pane 受阻时露面，状态离开受阻时 server
/// 自己清掉，所以不另装清除用的钩子。`PermissionRequest` {tool_name, tool_input,
/// permission_suggestions?} 以工具名作 matcher，`Notification` {message, title?,
/// notification_type} 以通知类型作 matcher（claude 2.1.76 核对）。两者只上报、从不替
/// claude 做决定，放到后台跑。
///
/// 不订阅 `StopFailure`（claude 2.1.78 起才有，晚于 `CLAUDE_MIN_VERSION`）：claude
/// 2.1.101 之前，settings 里出现它不认识的事件会让整份 settings.json 作废（2.1.76
/// 实测），连同用户自己的设置。
static ATTENTION_HOOKS: [HookInstall; 2] = [
    HookInstall {
        event: "Notification",
        action: "attention",
        matcher: Some(NOTIFICATION_MATCHER),
        background: true,
    },
    HookInstall {
        event: "PermissionRequest",
        action: "attention",
        matcher: Some("*"),
        background: true,
    },
];

/// 活动信号钩子，只发「有变化」提示（`pane.report_agent_activity`），活动树本身
/// 由 server 读转录得出，所以放到后台跑。事件与载荷按 claude 2.1.278 二进制内嵌的
/// schema 核对：`SubagentStart` {agent_id, agent_type} 与 `SubagentStop` {agent_id,
/// agent_type, agent_transcript_path, last_assistant_message?} 以 agent_type 作 matcher
/// 查询，`*` 取全部；`TaskCompleted` {task_id, task_subject, task_description?} 不支持
/// matcher。不再订阅 `TaskCreated`：它从 claude 2.1.84 起才有，晚于
/// `CLAUDE_MIN_VERSION`，更早的版本见到它会作废整份 settings.json；新建的任务由活动树
/// 轮询补上。Windows 的 PowerShell 资产经 `herdr pane report-agent-activity` 上报，各
/// 平台装同一套。
static ACTIVITY_HOOKS: [HookInstall; 3] = [
    HookInstall {
        event: "SubagentStart",
        action: "activity",
        matcher: Some("*"),
        background: true,
    },
    HookInstall {
        event: "SubagentStop",
        action: "activity",
        matcher: Some("*"),
        background: true,
    },
    HookInstall {
        event: "TaskCompleted",
        action: "activity",
        matcher: None,
        background: true,
    },
];

/// herdr 应当装好的全部钩子，按写入顺序。
fn installed_hooks() -> impl Iterator<Item = &'static HookInstall> {
    std::iter::once(&SESSION_HOOK)
        .chain(ATTENTION_HOOKS.iter())
        .chain(ACTIVITY_HOOKS.iter())
}

fn installed_hook_for(event: &str) -> Option<&'static HookInstall> {
    installed_hooks().find(|hook| hook.event == event)
}

struct HookRemoval {
    event: &'static str,
    actions: &'static [&'static str],
}

/// 安装前清理与卸载时移除的 herdr 命令。安装时各事件的规范条目（`installed_hooks`）
/// 原样保留，其余匹配到的旧命令一律删除。
const HOOK_REMOVALS: &[HookRemoval] = &[
    HookRemoval {
        event: "PostToolUse",
        actions: &["working"],
    },
    HookRemoval {
        event: "PostToolUseFailure",
        actions: &["working"],
    },
    HookRemoval {
        event: "SubagentStart",
        actions: &["activity"],
    },
    HookRemoval {
        event: "SubagentStop",
        actions: &["working", "activity"],
    },
    HookRemoval {
        event: "TaskCreated",
        actions: &["activity"],
    },
    HookRemoval {
        event: "TaskCompleted",
        actions: &["activity"],
    },
    HookRemoval {
        event: "Notification",
        actions: &["attention"],
    },
    HookRemoval {
        event: "PermissionRequest",
        actions: &["blocked", "attention"],
    },
    HookRemoval {
        event: "SessionStart",
        actions: &["idle", "session"],
    },
    HookRemoval {
        event: "UserPromptSubmit",
        actions: &["working"],
    },
    HookRemoval {
        event: "PreToolUse",
        actions: &["working"],
    },
    HookRemoval {
        event: "Stop",
        actions: &["idle"],
    },
    HookRemoval {
        event: "SessionEnd",
        actions: &["release"],
    },
];

pub(crate) fn install(content: &str, settings_path: &Path, hook_path: &Path) -> io::Result<String> {
    // 逐条安装：每一趟都是完整的「解析 → 清理 → 补齐 → 保格式改写 → 回读校验」。
    // 清理时保留所有事件的规范条目，后一趟不会删掉前一趟刚装上的钩子；全部已是
    // 规范形状时每一趟都原样返回，整体字节不变。
    let mut current = content.to_string();
    for hook in installed_hooks() {
        current = install_one(&current, settings_path, hook_path, hook)?;
    }
    Ok(current)
}

fn install_one(
    content: &str,
    settings_path: &Path,
    hook_path: &Path,
    hook: &HookInstall,
) -> io::Result<String> {
    let original = parse_value(content, settings_path)?;
    let mut desired = original.clone();
    let hooks = ensure_hooks_object(
        &mut desired,
        settings_path,
        "claude settings",
        "claude settings hooks",
    )?;
    apply_value_removals(hooks, hook_path, true)?;
    ensure_canonical_hook(hooks, hook_path, hook)?;

    if desired == original {
        return Ok(content.to_string());
    }

    rewrite(content, settings_path, hook_path, Some(hook), &desired)
}

pub(crate) fn uninstall(
    content: &str,
    settings_path: &Path,
    hook_path: &Path,
) -> io::Result<String> {
    let original = parse_value(content, settings_path)?;
    let mut desired = original.clone();
    let mut removed = false;

    if let Some(hooks) = hooks_object_if_present(
        &mut desired,
        settings_path,
        "claude settings",
        "claude settings hooks",
    )? {
        removed = apply_value_removals(hooks, hook_path, false)?;
    }

    if !removed {
        return Ok(content.to_string());
    }

    rewrite(content, settings_path, hook_path, None, &desired)
}

/// 清理之后补上这条钩子的规范条目。清理只留下规范条目里的 herdr 命令，所以「已有
/// 相等的条目」就是「已经装好」。
fn ensure_canonical_hook(
    hooks: &mut Map<String, Value>,
    hook_path: &Path,
    hook: &HookInstall,
) -> io::Result<()> {
    let canonical = canonical_hook_value(hook_path, hook);
    let entries = hooks
        .entry(hook.event.to_string())
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or_else(|| {
            io::Error::other(format!("hook entries for {} must be an array", hook.event))
        })?;
    if !entries.contains(&canonical) {
        entries.push(canonical);
    }
    Ok(())
}

/// 安装时要原样保留的规范条目；卸载时一律为 `None`。
fn preserved_canonical(installing: bool, event: &str, hook_path: &Path) -> Option<Value> {
    if !installing {
        return None;
    }
    installed_hook_for(event).map(|hook| canonical_hook_value(hook_path, hook))
}

fn apply_value_removals(
    hooks: &mut Map<String, Value>,
    hook_path: &Path,
    installing: bool,
) -> io::Result<bool> {
    let mut removed = false;
    for policy in HOOK_REMOVALS {
        let commands = removal_commands(policy, hook_path);
        let canonical = preserved_canonical(installing, policy.event, hook_path);
        removed |= remove_value_event_commands(hooks, policy.event, &commands, canonical.as_ref())?;
    }
    Ok(removed)
}

fn remove_value_event_commands(
    hooks: &mut Map<String, Value>,
    event: &str,
    commands: &[String],
    canonical: Option<&Value>,
) -> io::Result<bool> {
    let Some(entries_value) = hooks.get_mut(event) else {
        return Ok(false);
    };
    let entries = entries_value
        .as_array_mut()
        .ok_or_else(|| io::Error::other(format!("hook entries for {event} must be an array")))?;
    let mut removed = false;
    let mut canonical_preserved = false;

    entries.retain_mut(|entry| {
        if !canonical_preserved && canonical.is_some_and(|canonical| entry == canonical) {
            canonical_preserved = true;
            return true;
        }
        let Some(command_entries) = entry.get_mut("hooks").and_then(Value::as_array_mut) else {
            return true;
        };
        let before = command_entries.len();
        command_entries.retain(|entry| {
            !commands
                .iter()
                .any(|command| is_matching_command_hook(entry, command))
        });
        removed |= command_entries.len() != before;
        !command_entries.is_empty()
    });

    if entries.is_empty() && canonical.is_none() {
        hooks.remove(event);
    }
    Ok(removed)
}

/// 按 `desired` 保格式改写 settings。`install` 为 `Some` 时补齐这一条钩子，
/// `None` 表示卸载。
fn rewrite(
    content: &str,
    settings_path: &Path,
    hook_path: &Path,
    install: Option<&HookInstall>,
    desired: &Value,
) -> io::Result<String> {
    let installing = install.is_some();
    let root = CstRootNode::parse(content, &strict_parse_options()).map_err(|err| {
        io::Error::other(format!(
            "failed to parse {}: {err}",
            settings_path.display()
        ))
    })?;
    let root_value = root.value().ok_or_else(|| {
        io::Error::other(format!(
            "claude settings at {} must be a JSON object",
            settings_path.display()
        ))
    })?;
    reject_duplicate_keys(&root_value, settings_path)?;
    let root_object = root_value.as_object().ok_or_else(|| {
        io::Error::other(format!(
            "claude settings at {} must be a JSON object",
            settings_path.display()
        ))
    })?;

    let hooks = match (root_object.get("hooks"), install) {
        (Some(property), _) => property.object_value().ok_or_else(|| {
            io::Error::other(format!(
                "claude settings hooks at {} must be a JSON object",
                settings_path.display()
            ))
        })?,
        (None, Some(hook)) if direct_children_are_compact(&root_object.children()) => {
            let updated = append_hooks_property_compact(content, hook_path, settings_path, hook)?;
            return verify_updated(updated, settings_path, desired);
        }
        (None, Some(_)) => root_object
            .append("hooks", CstInputValue::Object(Vec::new()))
            .object_value()
            .ok_or_else(|| io::Error::other("failed to create claude settings hooks object"))?,
        (None, None) => return Ok(content.to_string()),
    };

    let mut canonical_preserved = false;
    for policy in HOOK_REMOVALS {
        let commands = removal_commands(policy, hook_path);
        let preserved =
            remove_event_commands(&hooks, policy.event, &commands, installing, hook_path)?;
        if install.is_some_and(|hook| hook.event == policy.event) {
            canonical_preserved |= preserved;
        }
    }

    let Some(hook) = install.filter(|_| !canonical_preserved) else {
        return verify_updated(root.to_string(), settings_path, desired);
    };
    match hooks.get(hook.event) {
        Some(property) => {
            let entries = property.array_value().ok_or_else(|| {
                io::Error::other(format!("hook entries for {} must be an array", hook.event))
            })?;
            if direct_children_are_compact(&entries.children()) {
                let updated =
                    append_hook_entry_compact(&root.to_string(), hook_path, settings_path, hook)?;
                return verify_updated(updated, settings_path, desired);
            }
            entries.append(canonical_hook_input(hook_path, hook));
        }
        None if direct_children_are_compact(&hooks.children()) => {
            let updated =
                append_hook_property_compact(&root.to_string(), hook_path, settings_path, hook)?;
            return verify_updated(updated, settings_path, desired);
        }
        None => {
            let entries = hooks
                .append(hook.event, CstInputValue::Array(Vec::new()))
                .array_value()
                .ok_or_else(|| {
                    io::Error::other(format!("failed to create {} hook array", hook.event))
                })?;
            entries.append(canonical_hook_input(hook_path, hook));
        }
    }

    verify_updated(root.to_string(), settings_path, desired)
}

/// CST 侧的清理，必须与 `remove_value_event_commands` 逐条对应，否则回读校验失败。
/// 返回该事件的规范条目是否被原样保留。
fn remove_event_commands(
    hooks: &CstObject,
    event: &str,
    commands: &[String],
    installing: bool,
    hook_path: &Path,
) -> io::Result<bool> {
    let Some(event_property) = hooks.get(event) else {
        return Ok(false);
    };
    let entries = event_property
        .array_value()
        .ok_or_else(|| io::Error::other(format!("hook entries for {event} must be an array")))?;
    let canonical = preserved_canonical(installing, event, hook_path);
    let mut canonical_preserved = false;

    for entry in entries.elements() {
        if !canonical_preserved
            && canonical.is_some()
            && entry.to_serde_value().as_ref() == canonical.as_ref()
        {
            canonical_preserved = true;
            continue;
        }

        let Some(entry_object) = entry.as_object() else {
            continue;
        };
        let Some(command_entries) = entry_object
            .get("hooks")
            .and_then(|property| property.array_value())
        else {
            continue;
        };

        for command_entry in command_entries.elements() {
            let matches = command_entry.to_serde_value().is_some_and(|value| {
                commands
                    .iter()
                    .any(|command| is_matching_command_hook(&value, command))
            });
            if matches {
                command_entry.remove();
            }
        }

        if command_entries.elements().is_empty() {
            entry.remove();
        }
    }

    if entries.elements().is_empty() && canonical.is_none() {
        event_property.remove();
    }

    Ok(canonical_preserved)
}

fn removal_commands(policy: &HookRemoval, hook_path: &Path) -> Vec<String> {
    policy
        .actions
        .iter()
        .flat_map(|action| hook_command_variants(hook_path, Some(action)))
        .collect()
}

/// 规范条目的值形状；`canonical_hook_input` 与 `canonical_hook_json` 写出的文本必须
/// 解析成同一个值。
fn canonical_hook_value(hook_path: &Path, hook: &HookInstall) -> Value {
    let mut handler = Map::new();
    handler.insert("type".to_string(), Value::from("command"));
    handler.insert(
        "command".to_string(),
        Value::from(hook_command(hook_path, Some(hook.action))),
    );
    handler.insert("timeout".to_string(), Value::from(HOOK_TIMEOUT_SECS));
    if hook.background {
        handler.insert("async".to_string(), Value::Bool(true));
    }
    let mut entry = Map::new();
    if let Some(matcher) = hook.matcher {
        entry.insert("matcher".to_string(), Value::from(matcher));
    }
    entry.insert(
        "hooks".to_string(),
        Value::Array(vec![Value::Object(handler)]),
    );
    Value::Object(entry)
}

fn canonical_hook_input(hook_path: &Path, hook: &HookInstall) -> CstInputValue {
    let mut handler = vec![
        ("type".to_string(), CstInputValue::from("command")),
        (
            "command".to_string(),
            CstInputValue::from(hook_command(hook_path, Some(hook.action))),
        ),
        (
            "timeout".to_string(),
            CstInputValue::from(HOOK_TIMEOUT_SECS),
        ),
    ];
    if hook.background {
        handler.push(("async".to_string(), CstInputValue::Bool(true)));
    }
    let mut properties = Vec::with_capacity(2);
    if let Some(matcher) = hook.matcher {
        properties.push(("matcher".to_string(), CstInputValue::from(matcher)));
    }
    properties.push((
        "hooks".to_string(),
        CstInputValue::Array(vec![CstInputValue::Object(handler)]),
    ));
    CstInputValue::Object(properties)
}

fn append_hooks_property_compact(
    content: &str,
    hook_path: &Path,
    settings_path: &Path,
    hook: &HookInstall,
) -> io::Result<String> {
    let root = parse_ast_root_object(content, settings_path)?;
    let event = serde_json::to_string(hook.event)?;
    let value = format!("{{{event}:[{}]}}", canonical_hook_json(hook_path, hook)?);
    Ok(append_object_property(content, &root, "hooks", &value))
}

fn append_hook_property_compact(
    content: &str,
    hook_path: &Path,
    settings_path: &Path,
    hook: &HookInstall,
) -> io::Result<String> {
    let root = parse_ast_root_object(content, settings_path)?;
    let hooks = root.get_object("hooks").ok_or_else(|| {
        io::Error::other(format!(
            "claude settings hooks at {} must be a JSON object",
            settings_path.display()
        ))
    })?;
    let value = format!("[{}]", canonical_hook_json(hook_path, hook)?);
    Ok(append_object_property(content, hooks, hook.event, &value))
}

fn append_hook_entry_compact(
    content: &str,
    hook_path: &Path,
    settings_path: &Path,
    hook: &HookInstall,
) -> io::Result<String> {
    let root = parse_ast_root_object(content, settings_path)?;
    let entries = root
        .get_object("hooks")
        .and_then(|hooks| hooks.get_array(hook.event))
        .ok_or_else(|| {
            io::Error::other(format!("hook entries for {} must be an array", hook.event))
        })?;
    Ok(append_array_element(
        content,
        entries,
        &canonical_hook_json(hook_path, hook)?,
    ))
}

fn parse_ast_root_object<'a>(content: &'a str, settings_path: &Path) -> io::Result<AstObject<'a>> {
    let parsed = parse_to_ast(content, &CollectOptions::default(), &strict_parse_options())
        .map_err(|err| {
            io::Error::other(format!(
                "failed to parse {}: {err}",
                settings_path.display()
            ))
        })?;
    match parsed.value {
        Some(AstValue::Object(object)) => Ok(object),
        _ => Err(io::Error::other(format!(
            "claude settings at {} must be a JSON object",
            settings_path.display()
        ))),
    }
}

fn append_object_property(
    content: &str,
    object: &AstObject<'_>,
    name: &str,
    value: &str,
) -> String {
    let key = serde_json::to_string(name).expect("JSON object keys are serializable");
    let key_value_separator = object
        .properties
        .first()
        .map(|property| &content[property.name.range().end..property.value.range().start])
        .unwrap_or(":");
    let insertion = format!("{key}{key_value_separator}{value}");
    let delimiter = object_delimiter(content, object);
    append_to_container(
        content,
        object.range,
        !object.properties.is_empty(),
        delimiter,
        &insertion,
    )
}

fn append_array_element(content: &str, array: &AstArray<'_>, value: &str) -> String {
    let delimiter = array_delimiter(content, array);
    append_to_container(
        content,
        array.range,
        !array.elements.is_empty(),
        delimiter,
        value,
    )
}

fn object_delimiter<'a>(content: &'a str, object: &AstObject<'_>) -> &'a str {
    match object.properties.as_slice() {
        [first, second, ..] => delimiter_suffix(&content[first.range.end..second.range.start]),
        [first] => &content[object.range.start + 1..first.range.start],
        [] => "",
    }
}

fn array_delimiter<'a>(content: &'a str, array: &AstArray<'_>) -> &'a str {
    match array.elements.as_slice() {
        [first, second, ..] => delimiter_suffix(&content[first.range().end..second.range().start]),
        [first] => &content[array.range.start + 1..first.range().start],
        [] => "",
    }
}

fn delimiter_suffix(delimiter: &str) -> &str {
    delimiter
        .split_once(',')
        .map(|(_, suffix)| suffix)
        .unwrap_or(delimiter)
}

fn append_to_container(
    content: &str,
    range: jsonc_parser::common::Range,
    has_elements: bool,
    delimiter: &str,
    value: &str,
) -> String {
    let closing = range.end - 1;
    let insertion_index = if has_elements {
        content[..closing].trim_end_matches([' ', '\t']).len()
    } else {
        closing
    };
    let mut updated = String::with_capacity(content.len() + delimiter.len() + value.len() + 1);
    updated.push_str(&content[..insertion_index]);
    if has_elements {
        updated.push(',');
        updated.push_str(delimiter);
    }
    updated.push_str(value);
    updated.push_str(&content[insertion_index..]);
    updated
}

/// 紧凑容器里追加的规范条目文本（与 `canonical_hook_value` 语义相同）。
fn canonical_hook_json(hook_path: &Path, hook: &HookInstall) -> io::Result<String> {
    let command = serde_json::to_string(&hook_command(hook_path, Some(hook.action)))?;
    let background = if hook.background {
        ",\"async\":true"
    } else {
        ""
    };
    let hooks = format!(
        "[{{\"type\":\"command\",\"command\":{command},\"timeout\":{HOOK_TIMEOUT_SECS}{background}}}]"
    );
    Ok(match hook.matcher {
        Some(matcher) => format!(
            "{{\"matcher\":{},\"hooks\":{hooks}}}",
            serde_json::to_string(matcher)?
        ),
        None => format!("{{\"hooks\":{hooks}}}"),
    })
}

fn verify_updated(updated: String, settings_path: &Path, desired: &Value) -> io::Result<String> {
    let actual = parse_value(&updated, settings_path)?;
    if &actual != desired {
        return Err(io::Error::other(format!(
            "failed to safely update claude settings at {}",
            settings_path.display()
        )));
    }
    Ok(updated)
}

fn direct_children_are_compact(children: &[CstNode]) -> bool {
    !children.iter().any(CstNode::is_newline)
}

fn parse_value(content: &str, settings_path: &Path) -> io::Result<Value> {
    serde_json::from_str(content).map_err(|err| {
        io::Error::other(format!(
            "failed to parse {}: {err}",
            settings_path.display()
        ))
    })
}

pub(super) fn reject_duplicate_keys(node: &CstNode, settings_path: &Path) -> io::Result<()> {
    if let Some(object) = node.as_object() {
        let mut names = HashSet::new();
        for property in object.properties() {
            let name = property
                .name()
                .ok_or_else(|| io::Error::other("JSON object property is missing a name"))?
                .decoded_value()
                .map_err(|err| io::Error::other(format!("failed to decode JSON key: {err}")))?;
            if !names.insert(name.clone()) {
                return Err(io::Error::other(format!(
                    "claude settings at {} contains duplicate key {name:?}",
                    settings_path.display()
                )));
            }
            if let Some(value) = property.value() {
                reject_duplicate_keys(&value, settings_path)?;
            }
        }
    } else if let Some(array) = node.as_array() {
        for element in array.elements() {
            reject_duplicate_keys(&element, settings_path)?;
        }
    }
    Ok(())
}

fn strict_parse_options() -> ParseOptions {
    ParseOptions {
        allow_comments: false,
        allow_loose_object_property_names: false,
        allow_trailing_commas: false,
        allow_missing_commas: false,
        allow_single_quoted_strings: false,
        allow_hexadecimal_numbers: false,
        allow_unary_plus_numbers: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> (&'static Path, &'static Path) {
        (
            Path::new("/home/test/.claude/settings.json"),
            Path::new("/home/test/.claude/hooks/herdr-agent-state.sh"),
        )
    }

    /// 紧凑容器里 SessionStart 之后依次追加的其余钩子属性。
    fn hooks_tail(hook_path: &Path) -> String {
        installed_hooks()
            .filter(|hook| hook.event != SESSION_HOOK.event)
            .map(|hook| {
                format!(
                    ",\"{}\":[{}]",
                    hook.event,
                    canonical_hook_json(hook_path, hook).unwrap()
                )
            })
            .collect()
    }

    #[test]
    fn install_preserves_untouched_formatting_and_complete_trailing_suffix() {
        let (settings_path, hook_path) = paths();
        let input = concat!(
            "{\r\n",
            "    \"zeta\" : {\"escaped\":\"\\u0061\", \"number\":1e+02},\r\n",
            "    \"hooks\" : {\r\n",
            "        \"PreCompact\" : [{\"matcher\":\"keep\",\"hooks\":[]}]\r\n",
            "    },\r\n",
            "    \"alpha\" : 1\r\n",
            "}\r\n\r\n",
        );

        let updated = install(input, settings_path, hook_path).unwrap();

        assert!(updated.starts_with(concat!(
            "{\r\n",
            "    \"zeta\" : {\"escaped\":\"\\u0061\", \"number\":1e+02},\r\n",
            "    \"hooks\" : {\r\n",
            "        \"PreCompact\" : [{\"matcher\":\"keep\",\"hooks\":[]}],\r\n",
        )));
        assert!(updated.ends_with(concat!(
            "\r\n    },\r\n",
            "    \"alpha\" : 1\r\n",
            "}\r\n\r\n",
        )));
        assert!(!updated.replace("\r\n", "").contains('\n'));
        assert!(updated.contains("\"SessionStart\""));
        for hook in installed_hooks() {
            assert!(
                updated.contains(&format!("\"{}\"", hook.event)),
                "{}",
                hook.event
            );
        }
        assert_eq!(
            serde_json::from_str::<Value>(&updated).unwrap()["zeta"]["number"],
            100.0
        );
    }

    #[test]
    fn install_keeps_compact_containers_compact() {
        let (settings_path, hook_path) = paths();
        let canonical = canonical_hook_json(hook_path, &SESSION_HOOK).unwrap();
        // 每种形状里 SessionStart 都落在 hooks 的最后，其余钩子紧随其后逐个追加。
        let tail = hooks_tail(hook_path);
        let cases = [
            (
                "{\"zeta\":{\"escaped\":\"\\u0061\",\"n\":1e+02},\"alpha\":1}\r\n",
                format!(
                    "{{\"zeta\":{{\"escaped\":\"\\u0061\",\"n\":1e+02}},\"alpha\":1,\"hooks\":{{\"SessionStart\":[{canonical}]{tail}}}}}\r\n"
                ),
            ),
            (
                "{\"hooks\":{\"PreCompact\":[{\"matcher\":\"keep\",\"hooks\":[]}]}, \"alpha\":1}",
                format!(
                    "{{\"hooks\":{{\"PreCompact\":[{{\"matcher\":\"keep\",\"hooks\":[]}}],\"SessionStart\":[{canonical}]{tail}}}, \"alpha\":1}}"
                ),
            ),
            (
                "{\"hooks\":{\"SessionStart\":[{\"matcher\":\"keep\",\"hooks\":[{\"type\":\"command\",\"command\":\"echo keep\"}]}]}}",
                format!(
                    "{{\"hooks\":{{\"SessionStart\":[{{\"matcher\":\"keep\",\"hooks\":[{{\"type\":\"command\",\"command\":\"echo keep\"}}]}},{canonical}]{tail}}}}}"
                ),
            ),
            (
                "{\"zeta\":{\n  \"x\":1\n},\"alpha\":1}",
                format!(
                    "{{\"zeta\":{{\n  \"x\":1\n}},\"alpha\":1,\"hooks\":{{\"SessionStart\":[{canonical}]{tail}}}}}"
                ),
            ),
            (
                "{\"hooks\":{\"PreCompact\":[\n  {\"matcher\":\"keep\",\"hooks\":[]}\n]},\"alpha\":1}",
                format!(
                    "{{\"hooks\":{{\"PreCompact\":[\n  {{\"matcher\":\"keep\",\"hooks\":[]}}\n],\"SessionStart\":[{canonical}]{tail}}},\"alpha\":1}}"
                ),
            ),
            (
                "{\"hooks\":{\"SessionStart\":[{\n  \"matcher\":\"keep\",\n  \"hooks\":[{\"type\":\"command\",\"command\":\"echo keep\"}]\n}]}}",
                format!(
                    "{{\"hooks\":{{\"SessionStart\":[{{\n  \"matcher\":\"keep\",\n  \"hooks\":[{{\"type\":\"command\",\"command\":\"echo keep\"}}]\n}},{canonical}]{tail}}}}}"
                ),
            ),
        ];

        for (input, expected) in cases {
            assert_eq!(install(input, settings_path, hook_path).unwrap(), expected);
        }
    }

    #[test]
    fn install_scopes_claude_session_start_sources() {
        let (settings_path, hook_path) = paths();
        let installed = install("{}", settings_path, hook_path).unwrap();
        let settings: Value = serde_json::from_str(&installed).unwrap();
        let matcher = settings["hooks"]["SessionStart"][0]["matcher"]
            .as_str()
            .unwrap();
        assert_eq!(matcher, "^(startup|resume|clear|compact|fork)$");
        let pattern = regex::Regex::new(matcher).unwrap();
        for source in ["startup", "resume", "clear", "compact", "fork"] {
            assert!(pattern.is_match(source), "Claude source: {source}");
        }
        for source in ["new", "load", "", "future-source", "startup-extra"] {
            assert!(!pattern.is_match(source), "non-Claude source: {source}");
        }
    }

    #[test]
    fn install_is_a_byte_exact_noop_for_a_canonical_hook() {
        let (settings_path, hook_path) = paths();
        let command = serde_json::to_string(&hook_command(hook_path, Some("session"))).unwrap();
        let tail = hooks_tail(hook_path);
        let input = format!(
            "{{\"hooks\":{{\"SessionStart\":[{{\"hooks\":[{{\"timeout\":10,\"command\":{command},\"type\":\"command\"}}],\"matcher\":\"{SESSION_START_MATCHER}\"}}]{tail}}},\"escaped\":\"\\u0061\"}}  \r\n\r\n"
        );

        let updated = install(&input, settings_path, hook_path).unwrap();

        assert_eq!(updated, input);
    }

    #[test]
    fn install_migrates_wildcard_session_start_and_preserves_user_hook() {
        let (settings_path, hook_path) = paths();
        let command = serde_json::to_string(&hook_command(hook_path, Some("session"))).unwrap();
        let user_hook = r#"{ "type" : "command", "command" : "echo keep", "timeout" : 3 }"#;
        let input = format!(
            "{{\n  \"hooks\": {{\n    \"SessionStart\": [{{\"matcher\":\"*\",\"hooks\":[{{\"type\":\"command\",\"command\":{command},\"timeout\":10}},{user_hook}]}}]\n  }}\n}}\n\n"
        );
        let installed = install(&input, settings_path, hook_path).unwrap();
        assert!(installed.contains(user_hook));
        assert!(installed.ends_with("}\n\n"));
        let settings: Value = serde_json::from_str(&installed).unwrap();
        let groups = settings["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0]["matcher"], "*");
        assert_eq!(groups[0]["hooks"].as_array().unwrap().len(), 1);
        assert_eq!(groups[0]["hooks"][0]["command"], "echo keep");
        assert_eq!(groups[1], canonical_hook_value(hook_path, &SESSION_HOOK));
        assert_eq!(
            install(&installed, settings_path, hook_path).unwrap(),
            installed
        );

        let removed = uninstall(&installed, settings_path, hook_path).unwrap();
        assert!(removed.contains(user_hook));
        assert!(!removed.contains(&command));
        let settings: Value = serde_json::from_str(&removed).unwrap();
        assert_eq!(
            settings["hooks"]["SessionStart"].as_array().unwrap().len(),
            1
        );
    }

    #[test]
    fn install_preserves_canonical_session_start_position_during_migration() {
        let (settings_path, hook_path) = paths();
        let canonical = canonical_hook_json(hook_path, &SESSION_HOOK).unwrap();
        let tail = hooks_tail(hook_path);
        let old_command = serde_json::to_string(&hook_command(hook_path, Some("working"))).unwrap();
        let session_start = format!(
            "\"SessionStart\":[{canonical},{{\"matcher\":\"foreign\",\"hooks\":[{{\"type\":\"command\",\"command\":\"echo keep\"}}]}}]"
        );
        let old_event = [
            "\"PostToolUse\":[{\"matcher\":\"*\",\"hooks\":[{\"type\":\"command\",\"command\":",
            &old_command,
            "}]}]",
        ]
        .concat();
        let input = ["{\"hooks\":{", &session_start, ",", &old_event, "}}"].concat();
        let expected = ["{\"hooks\":{", &session_start, &tail, "}}"].concat();

        let updated = install(&input, settings_path, hook_path).unwrap();

        assert_eq!(updated, expected);
    }

    #[test]
    fn install_removes_only_owned_commands_from_shared_hook_groups() {
        let (settings_path, hook_path) = paths();
        let old_command = serde_json::to_string(&hook_command(hook_path, Some("working"))).unwrap();
        let input = format!(
            concat!(
                "{{\n",
                "  \"hooks\": {{\n",
                "    \"PostToolUse\": [{{\n",
                "      \"matcher\": \"*\",\n",
                "      \"hooks\": [\n",
                "        {{\"type\":\"command\",\"command\":{old_command},\"timeout\":10}},\n",
                "        {{  \"type\" : \"command\", \"command\" : \"echo keep\", \"timeout\" : 3  }}\n",
                "      ]\n",
                "    }}],\n",
                "    \"PreCompact\": [{{\"matcher\":\"keep\",\"hooks\":[]}}]\n",
                "  }}\n",
                "}}\n",
            ),
            old_command = old_command,
        );

        let updated = install(&input, settings_path, hook_path).unwrap();

        assert!(!updated.contains(&old_command));
        assert!(updated.contains(
            "        {  \"type\" : \"command\", \"command\" : \"echo keep\", \"timeout\" : 3  }"
        ));
        assert!(updated.contains("    \"PreCompact\": [{\"matcher\":\"keep\",\"hooks\":[]}]"));
        let parsed: Value = serde_json::from_str(&updated).unwrap();
        assert_eq!(
            parsed["hooks"]["PostToolUse"][0]["hooks"][0]["command"],
            "echo keep"
        );
        assert_eq!(parsed["hooks"]["SessionStart"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn uninstall_preserves_unrelated_hook_text() {
        let (settings_path, hook_path) = paths();
        let command = serde_json::to_string(&hook_command(hook_path, Some("session"))).unwrap();
        let input = format!(
            concat!(
                "{{\n",
                "    \"before\" : \"\\u0061\",\n",
                "    \"hooks\" : {{\n",
                "        \"SessionStart\" : [{{\n",
                "            \"matcher\" : \"*\",\n",
                "            \"hooks\" : [\n",
                "                {{\"type\":\"command\",\"command\":{command},\"timeout\":10}},\n",
                "                {{  \"type\" : \"command\", \"command\" : \"echo keep\"  }}\n",
                "            ]\n",
                "        }}]\n",
                "    }},\n",
                "    \"after\" : 1e+02\n",
                "}}\n\n",
            ),
            command = command,
        );

        let updated = uninstall(&input, settings_path, hook_path).unwrap();

        assert_ne!(updated, input);
        assert!(!updated.contains(&command));
        assert!(updated
            .contains("                {  \"type\" : \"command\", \"command\" : \"echo keep\"  }"));
        assert!(updated.starts_with("{\n    \"before\" : \"\\u0061\","));
        assert!(updated.ends_with("    \"after\" : 1e+02\n}\n\n"));
    }

    #[test]
    fn install_subscribes_detail_and_activity_hooks_in_the_background() {
        let (settings_path, hook_path) = paths();
        let installed = install("{}", settings_path, hook_path).unwrap();
        let settings: Value = serde_json::from_str(&installed).unwrap();
        let hooks = settings["hooks"].as_object().unwrap();

        for (hook, action) in ATTENTION_HOOKS
            .iter()
            .map(|hook| (hook, " attention"))
            .chain(ACTIVITY_HOOKS.iter().map(|hook| (hook, " activity")))
        {
            let groups = hooks[hook.event].as_array().unwrap();
            assert_eq!(groups.len(), 1, "{}", hook.event);
            // Notification 按通知类型、PermissionRequest 按工具名、SubagentStart/Stop 按
            // agent_type 匹配；TaskCompleted 不支持 matcher，不写键。
            assert_eq!(
                groups[0].get("matcher").and_then(Value::as_str),
                hook.matcher,
                "{}",
                hook.event
            );
            let handler = &groups[0]["hooks"][0];
            let command = handler["command"].as_str().unwrap();
            assert!(command.ends_with(action), "{command}");
            assert_eq!(handler["timeout"], HOOK_TIMEOUT_SECS);
            assert_eq!(handler["async"], true, "{}", hook.event);
        }
        // 会话身份仍同步上报。
        assert!(hooks["SessionStart"][0]["hooks"][0].get("async").is_none());
        assert_eq!(
            hooks.keys().map(String::as_str).collect::<HashSet<_>>(),
            installed_hooks()
                .map(|hook| hook.event)
                .collect::<HashSet<_>>()
        );
        // claude 不认识的事件会让整份 settings 作废：不装只有新版本才有的事件。
        for event in ["TaskCreated", "StopFailure"] {
            assert!(hooks.get(event).is_none(), "{event}");
        }

        // 重装字节不变；卸载后一条 herdr 钩子都不剩。
        assert_eq!(
            install(&installed, settings_path, hook_path).unwrap(),
            installed
        );
        let removed = uninstall(&installed, settings_path, hook_path).unwrap();
        let settings: Value = serde_json::from_str(&removed).unwrap();
        assert_eq!(settings["hooks"], serde_json::json!({}));
    }

    /// 每个安装的钩子事件最早出现的 claude 版本（claude 更新日志；`Notification` 记的是
    /// 支持 matcher 的版本）。
    const EVENT_SINCE: [(&str, &str); 6] = [
        ("SessionStart", "1.0.62"),
        ("SubagentStop", "1.0.41"),
        ("Notification", "2.0.37"),
        ("SubagentStart", "2.0.43"),
        ("PermissionRequest", "2.0.45"),
        ("TaskCompleted", "2.1.33"),
    ];

    /// claude 2.1.101 之前，settings.json 里出现它不认识的事件会让整份文件作废：安装的
    /// 每个事件都必须登记，且不晚于安装时校验的最低版本。
    #[test]
    fn every_installed_event_is_known_at_the_minimum_claude_version() {
        use super::super::version::extract_version_triple;
        let min = extract_version_triple(super::super::CLAUDE_MIN_VERSION).unwrap();
        for hook in installed_hooks() {
            let (_, since) = EVENT_SINCE
                .iter()
                .find(|(event, _)| *event == hook.event)
                .unwrap_or_else(|| panic!("{}：登记它最早出现的 claude 版本", hook.event));
            assert!(
                extract_version_triple(since).unwrap() <= min,
                "{} 从 claude {since} 起才有，晚于最低版本",
                hook.event
            );
        }
        for (event, since) in [("StopFailure", "2.1.78"), ("TaskCreated", "2.1.84")] {
            assert!(extract_version_triple(since).unwrap() > min, "{event}");
            assert!(
                installed_hook_for(event).is_none(),
                "{event} 晚于最低版本，不能装"
            );
        }
    }

    #[test]
    fn notification_matcher_takes_only_the_prompts_that_wait_for_the_user() {
        let pattern = regex::Regex::new(NOTIFICATION_MATCHER).unwrap();
        for kind in ["permission_prompt", "elicitation_dialog"] {
            assert!(pattern.is_match(kind), "{kind}");
        }
        for kind in [
            "idle_prompt",
            "auth_success",
            "elicitation_complete",
            "worker_permission_prompt",
            "agent_needs_input",
            "",
        ] {
            assert!(!pattern.is_match(kind), "{kind}");
        }
    }

    #[test]
    fn install_adds_attention_hooks_beside_user_hooks_and_drops_the_legacy_blocked_hook() {
        let (settings_path, hook_path) = paths();
        let blocked = serde_json::to_string(&hook_command(hook_path, Some("blocked"))).unwrap();
        let input = format!(
            concat!(
                "{{\n",
                "  \"hooks\": {{\n",
                "    \"Notification\": [{{\"matcher\":\"idle_prompt\",\"hooks\":[{{\"type\":\"command\",\"command\":\"notify-send idle\"}}]}}],\n",
                "    \"PermissionRequest\": [{{\n",
                "      \"matcher\": \"*\",\n",
                "      \"hooks\": [\n",
                "        {{\"type\":\"command\",\"command\":{blocked},\"timeout\":10}},\n",
                "        {{\"type\":\"command\",\"command\":\"echo keep-permission\"}}\n",
                "      ]\n",
                "    }}]\n",
                "  }}\n",
                "}}\n",
            ),
            blocked = blocked,
        );

        let updated = install(&input, settings_path, hook_path).unwrap();
        let settings: Value = serde_json::from_str(&updated).unwrap();

        assert!(!updated.contains(&blocked));
        let notification = settings["hooks"]["Notification"].as_array().unwrap();
        assert_eq!(notification.len(), 2);
        assert_eq!(notification[0]["hooks"][0]["command"], "notify-send idle");
        assert_eq!(
            notification[1],
            canonical_hook_value(hook_path, &ATTENTION_HOOKS[0])
        );
        let permission = settings["hooks"]["PermissionRequest"].as_array().unwrap();
        assert_eq!(permission.len(), 2);
        assert_eq!(
            permission[0]["hooks"],
            serde_json::json!([{"type": "command", "command": "echo keep-permission"}])
        );
        assert_eq!(
            permission[1],
            canonical_hook_value(hook_path, &ATTENTION_HOOKS[1])
        );
        assert_eq!(
            install(&updated, settings_path, hook_path).unwrap(),
            updated
        );

        let removed = uninstall(&updated, settings_path, hook_path).unwrap();
        let settings: Value = serde_json::from_str(&removed).unwrap();
        assert_eq!(
            settings["hooks"]["Notification"],
            serde_json::json!([{"matcher": "idle_prompt", "hooks": [{"type": "command", "command": "notify-send idle"}]}])
        );
        assert_eq!(
            settings["hooks"]["PermissionRequest"][0]["hooks"],
            serde_json::json!([{"type": "command", "command": "echo keep-permission"}])
        );
        assert_eq!(
            settings["hooks"]["PermissionRequest"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn install_moves_foreground_activity_hooks_to_the_background() {
        let (settings_path, hook_path) = paths();
        // v12 写的活动钩子：同一条命令，没有 `async`。
        let foreground = HookInstall {
            background: false,
            ..ACTIVITY_HOOKS[0]
        };
        let old_entry =
            serde_json::to_string(&canonical_hook_value(hook_path, &foreground)).unwrap();
        let input = format!("{{\"hooks\":{{\"SubagentStart\":[{old_entry}]}}}}");

        let updated = install(&input, settings_path, hook_path).unwrap();
        let settings: Value = serde_json::from_str(&updated).unwrap();

        assert_eq!(
            settings["hooks"]["SubagentStart"],
            serde_json::json!([canonical_hook_value(hook_path, &ACTIVITY_HOOKS[0])])
        );
        assert_eq!(
            install(&updated, settings_path, hook_path).unwrap(),
            updated
        );
    }

    #[test]
    fn install_replaces_legacy_subagent_stop_hook_and_keeps_user_hooks() {
        let (settings_path, hook_path) = paths();
        let working = serde_json::to_string(&hook_command(hook_path, Some("working"))).unwrap();
        // v11/v12 装过的 TaskCreated 活动钩子：这个事件不再安装，整条删掉。
        let activity = serde_json::to_string(&hook_command(hook_path, Some("activity"))).unwrap();
        let input = format!(
            concat!(
                "{{\n",
                "  \"hooks\": {{\n",
                "    \"SubagentStop\": [{{\n",
                "      \"matcher\": \"Explore\",\n",
                "      \"hooks\": [\n",
                "        {{\"type\":\"command\",\"command\":{working},\"timeout\":10}},\n",
                "        {{\"type\":\"command\",\"command\":\"echo keep-subagent\"}}\n",
                "      ]\n",
                "    }}],\n",
                "    \"TaskCreated\": [{{\"hooks\":[{{\"type\":\"command\",\"command\":{activity},\"timeout\":10}}]}}],\n",
                "    \"TaskCompleted\": [{{\"hooks\":[{{\"type\":\"command\",\"command\":\"echo keep-task\"}}]}}]\n",
                "  }}\n",
                "}}\n",
            ),
            working = working,
            activity = activity,
        );

        let updated = install(&input, settings_path, hook_path).unwrap();
        let settings: Value = serde_json::from_str(&updated).unwrap();

        // 旧的 working 命令被删，用户命令与它所在的分组原位保留，规范条目追加在后。
        assert!(!updated.contains(&working));
        let subagent_stop = settings["hooks"]["SubagentStop"].as_array().unwrap();
        assert_eq!(subagent_stop.len(), 2);
        assert_eq!(subagent_stop[0]["matcher"], "Explore");
        assert_eq!(
            subagent_stop[0]["hooks"][0]["command"],
            "echo keep-subagent"
        );
        assert_eq!(
            subagent_stop[1],
            canonical_hook_value(hook_path, &ACTIVITY_HOOKS[1])
        );
        assert!(settings["hooks"].get("TaskCreated").is_none(), "{updated}");
        // 用户自己的 TaskCompleted 命令保留在前，规范条目追加在后。
        let task_completed = settings["hooks"]["TaskCompleted"].as_array().unwrap();
        assert_eq!(task_completed.len(), 2);
        assert_eq!(task_completed[0]["hooks"][0]["command"], "echo keep-task");
        assert_eq!(
            task_completed[1],
            canonical_hook_value(hook_path, &ACTIVITY_HOOKS[2])
        );
        assert!(updated.ends_with("}\n"));
        assert_eq!(
            install(&updated, settings_path, hook_path).unwrap(),
            updated
        );
    }

    #[test]
    fn install_rejects_duplicate_keys() {
        let (settings_path, hook_path) = paths();
        let error = install(
            r#"{"alpha": 1, "alpha": 2, "hooks": {}}"#,
            settings_path,
            hook_path,
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("duplicate key \"alpha\""), "{error}");
    }

    #[test]
    fn install_keeps_structurally_invalid_content_unchanged() {
        let (settings_path, hook_path) = paths();
        for input in ["[]", r#"{"hooks": []}"#, r#"{"hooks":{"SessionStart":{}}}"#] {
            assert!(install(input, settings_path, hook_path).is_err());
        }
    }
}
