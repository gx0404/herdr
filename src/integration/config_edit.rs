use std::io;
use std::path::Path;

use serde_json::{json, Map, Value};
use toml_edit::TableLike;

use super::command::{hook_command, legacy_bash_hook_command};
#[cfg(windows)]
use super::file_ops::legacy_bash_hook_path;
use super::{KIMI_CONFIG_BLOCK_BEGIN, KIMI_CONFIG_BLOCK_END, KIMI_HOOK_EVENTS};

pub(crate) fn ensure_hooks_object<'a>(
    settings: &'a mut Value,
    settings_path: &Path,
    root_description: &str,
    hooks_description: &str,
) -> io::Result<&'a mut Map<String, Value>> {
    let root = settings.as_object_mut().ok_or_else(|| {
        io::Error::other(format!(
            "{root_description} at {} must be a JSON object",
            settings_path.display()
        ))
    })?;

    let hooks = root.entry("hooks").or_insert_with(|| json!({}));
    hooks.as_object_mut().ok_or_else(|| {
        io::Error::other(format!(
            "{hooks_description} at {} must be a JSON object",
            settings_path.display()
        ))
    })
}

pub(crate) fn hooks_object_if_present<'a>(
    settings: &'a mut Value,
    settings_path: &Path,
    root_description: &str,
    hooks_description: &str,
) -> io::Result<Option<&'a mut Map<String, Value>>> {
    let root = settings.as_object_mut().ok_or_else(|| {
        io::Error::other(format!(
            "{root_description} at {} must be a JSON object",
            settings_path.display()
        ))
    })?;

    let Some(hooks) = root.get_mut("hooks") else {
        return Ok(None);
    };

    hooks.as_object_mut().map(Some).ok_or_else(|| {
        io::Error::other(format!(
            "{hooks_description} at {} must be a JSON object",
            settings_path.display()
        ))
    })
}

pub(crate) fn ensure_command_hook(
    hooks: &mut Map<String, Value>,
    event: &str,
    command: String,
    timeout: u64,
    matcher: Option<&str>,
) -> io::Result<()> {
    let entries = hooks
        .entry(event.to_string())
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or_else(|| io::Error::other(format!("hook entries for {event} must be an array")))?;

    let already_installed = entries.iter().any(|entry| {
        entry
            .get("hooks")
            .and_then(Value::as_array)
            .is_some_and(|hook_entries| {
                hook_entries.iter().any(|hook| {
                    hook.get("type").and_then(Value::as_str) == Some("command")
                        && hook.get("command").and_then(Value::as_str) == Some(command.as_str())
                })
            })
    });
    if already_installed {
        return Ok(());
    }

    let mut entry = Map::new();
    if let Some(matcher) = matcher {
        entry.insert("matcher".to_string(), Value::String(matcher.to_string()));
    }
    entry.insert(
        "hooks".to_string(),
        json!([
            {
                "type": "command",
                "command": command,
                "timeout": timeout,
            }
        ]),
    );

    entries.push(Value::Object(entry));
    Ok(())
}

pub(crate) fn remove_command_hook(
    hooks: &mut Map<String, Value>,
    event: &str,
    command: &str,
) -> io::Result<bool> {
    let Some(entries_value) = hooks.get_mut(event) else {
        return Ok(false);
    };

    let entries = entries_value
        .as_array_mut()
        .ok_or_else(|| io::Error::other(format!("hook entries for {event} must be an array")))?;

    let mut removed = false;
    entries.retain_mut(|entry| {
        let Some(entry_object) = entry.as_object_mut() else {
            return true;
        };
        let Some(hook_entries) = entry_object.get_mut("hooks") else {
            return true;
        };
        let Some(hook_entries) = hook_entries.as_array_mut() else {
            return true;
        };

        let before = hook_entries.len();
        hook_entries.retain(|hook| !is_matching_command_hook(hook, command));
        if hook_entries.len() != before {
            removed = true;
        }

        !hook_entries.is_empty()
    });

    let remove_event = entries.is_empty();
    if remove_event {
        hooks.remove(event);
    }

    Ok(removed)
}

pub(crate) fn remove_hook_commands(
    hooks: &mut Map<String, Value>,
    event: &str,
    hook_path: &Path,
    action: Option<&str>,
) -> io::Result<bool> {
    let mut removed = false;
    for command in hook_command_variants(hook_path, action) {
        removed |= remove_command_hook(hooks, event, &command)?;
    }
    Ok(removed)
}

pub(crate) fn hook_command_variants(hook_path: &Path, action: Option<&str>) -> Vec<String> {
    let mut commands = vec![hook_command(hook_path, action)];
    push_unique_command(&mut commands, legacy_bash_hook_command(hook_path, action));

    #[cfg(windows)]
    {
        push_unique_command(
            &mut commands,
            legacy_bash_hook_command(&legacy_bash_hook_path(hook_path), action),
        );
    }

    commands
}

pub(crate) fn push_unique_command(commands: &mut Vec<String>, command: String) {
    if !commands.iter().any(|existing| existing == &command) {
        commands.push(command);
    }
}

pub(crate) fn is_matching_command_hook(hook: &Value, command: &str) -> bool {
    hook.get("type").and_then(Value::as_str) == Some("command")
        && hook.get("command").and_then(Value::as_str) == Some(command)
}

/// Enable Codex hooks while preserving user-authored TOML text and semantics.
///
/// The parsed table is the semantic baseline; `toml_edit` changes only the
/// feature spans, and the candidate is parsed again and compared against that
/// baseline with the intended `hooks = true` migration. Unsafe or unrelated
/// rewrites are rejected before the caller writes `config.toml`.
pub(crate) fn build_codex_config_with_hooks(content: &str) -> io::Result<String> {
    let mut expected: toml::Table = toml::from_str(content)
        .map_err(|error| invalid_codex_config(format!("failed to parse config.toml: {error}")))?;
    let expected_features = expected
        .entry("features".to_string())
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
        .ok_or_else(|| invalid_codex_config("features must be a table or inline table"))?;
    for key in ["hooks", "codex_hooks"] {
        if expected_features
            .get(key)
            .is_some_and(|value| value.as_bool().is_none())
        {
            return Err(invalid_codex_config(format!(
                "features.{key} must be a boolean"
            )));
        }
    }
    expected_features.remove("codex_hooks");
    expected_features.insert("hooks".to_string(), toml::Value::Boolean(true));

    let document = toml_edit::ImDocument::parse(content)
        .map_err(|error| invalid_codex_config(format!("failed to parse config.toml: {error}")))?;
    let feature_item = document.get("features");
    let features: Option<&dyn TableLike> = feature_item.and_then(toml_edit::Item::as_table_like);
    let hooks = features.and_then(|table| table.get("hooks"));
    let deprecated = features.and_then(|table| table.get_key_value("codex_hooks"));
    let mut edits = Vec::new();
    if let Some(hooks) = hooks {
        if hooks.as_value().and_then(toml_edit::Value::as_bool) != Some(true) {
            let span = hooks
                .span()
                .ok_or_else(|| invalid_codex_config("cannot locate features.hooks"))?;
            edits.push((span, "true"));
        }
    }
    if let Some((key, value)) = deprecated {
        let key_span = key
            .span()
            .ok_or_else(|| invalid_codex_config("cannot locate features.codex_hooks key"))?;
        let value_span = value
            .span()
            .ok_or_else(|| invalid_codex_config("cannot locate features.codex_hooks value"))?;
        if hooks.is_none() {
            edits.push((key_span, "hooks"));
            edits.push((value_span, "true"));
        } else {
            let inline_span = feature_item
                .and_then(toml_edit::Item::as_inline_table)
                .and_then(toml_edit::InlineTable::span);
            let range = codex_deprecated_flag_range(content, key_span, value_span, inline_span)?;
            edits.push((range, ""));
        }
    }

    let candidate = if hooks.is_none() && deprecated.is_none() {
        insert_codex_hooks(content, document.into_mut())?
    } else {
        let mut candidate = content.to_string();
        edits.sort_unstable_by_key(|(range, _)| std::cmp::Reverse(range.start));
        let mut previous_start = content.len();
        for (range, replacement) in edits {
            if range.end > previous_start || content.get(range.clone()).is_none() {
                return Err(invalid_codex_config(
                    "cannot safely edit overlapping feature flags",
                ));
            }
            previous_start = range.start;
            candidate.replace_range(range, replacement);
        }
        candidate
    };
    let parsed: toml::Table = toml::from_str(&candidate)
        .map_err(|error| invalid_codex_config(format!("invalid edited config.toml: {error}")))?;
    if parsed != expected {
        return Err(invalid_codex_config(
            "editing feature flags would change unrelated settings",
        ));
    }
    Ok(candidate)
}

fn invalid_codex_config(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn codex_deprecated_flag_range(
    content: &str,
    key: std::ops::Range<usize>,
    value: std::ops::Range<usize>,
    inline: Option<std::ops::Range<usize>>,
) -> io::Result<std::ops::Range<usize>> {
    if let Some(inline) = inline {
        let before = content
            .get(inline.start + 1..key.start)
            .ok_or_else(|| invalid_codex_config("cannot locate inline feature separator"))?;
        let after = content
            .get(value.end..inline.end.saturating_sub(1))
            .ok_or_else(|| invalid_codex_config("cannot locate inline feature separator"))?;
        if let Some(comma) = after.find(',') {
            if after[..comma].trim().is_empty() {
                return Ok(key.start..value.end + comma + 1);
            }
        }
        let before = before.trim_end();
        if before.ends_with(',') {
            return Ok(inline.start + before.len()..value.end);
        }
        return Err(invalid_codex_config(
            "cannot safely remove inline features.codex_hooks",
        ));
    }

    let line_start = content[..key.start]
        .rfind('\n')
        .map_or(0, |index| index + 1);
    let prefix = &content[line_start..key.start];
    let indentation = prefix.len() - prefix.trim_start_matches([' ', '\t', '\u{feff}']).len();
    Ok(line_start + indentation..value.end)
}

fn insert_codex_hooks(content: &str, mut document: toml_edit::DocumentMut) -> io::Result<String> {
    let roundtrip = document.to_string();
    let omit_final_newline =
        !content.ends_with('\n') && roundtrip.strip_suffix('\n') == Some(content);
    if roundtrip != content && !omit_final_newline {
        return Err(invalid_codex_config(
            "cannot preserve config.toml formatting while adding hooks",
        ));
    }
    let features = document
        .entry("features")
        .or_insert_with(toml_edit::table)
        .as_table_like_mut()
        .ok_or_else(|| invalid_codex_config("features must be a table or inline table"))?;
    features.insert("hooks", toml_edit::value(true));
    let mut candidate = document.to_string();
    if omit_final_newline && candidate.ends_with('\n') {
        candidate.pop();
    }
    let prefix = content
        .bytes()
        .zip(candidate.bytes())
        .take_while(|(before, after)| before == after)
        .count();
    let suffix = content.as_bytes()[prefix..]
        .iter()
        .rev()
        .zip(candidate.as_bytes()[prefix..].iter().rev())
        .take_while(|(before, after)| before == after)
        .count();
    if prefix + suffix != content.len() {
        return Err(invalid_codex_config(
            "adding hooks would rewrite unrelated config.toml text",
        ));
    }
    Ok(candidate)
}

/// Replace only Herdr's marked Kimi hook block. Removing the old marker first
/// makes repeated installs idempotent and leaves unmarked user TOML untouched.
pub(crate) fn build_kimi_config_with_hooks(content: &str, hook_path: &Path) -> String {
    let mut result = remove_kimi_config_block(content)
        .trim_end_matches('\n')
        .to_string();
    if !result.is_empty() {
        result.push('\n');
        result.push('\n');
    }

    result.push_str(KIMI_CONFIG_BLOCK_BEGIN);
    result.push('\n');
    for (event, matcher, action) in KIMI_HOOK_EVENTS {
        result.push_str(&kimi_hook_table(event, matcher, hook_path, action));
    }
    result.push_str(KIMI_CONFIG_BLOCK_END);
    result.push('\n');
    result
}

pub(crate) fn kimi_hook_table(
    event: &str,
    matcher: Option<&str>,
    hook_path: &Path,
    action: &str,
) -> String {
    let command = hook_command(hook_path, Some(action));
    let matcher = matcher
        .map(|matcher| format!("matcher = {}\n", toml_basic_string(matcher)))
        .unwrap_or_default();
    format!(
        "[[hooks]]\nevent = {}\n{matcher}command = {}\ntimeout = 10\n\n",
        toml_basic_string(event),
        toml_basic_string(&command)
    )
}

pub(crate) fn remove_kimi_config_block(content: &str) -> String {
    let trailing_newline = content.ends_with('\n');
    let mut lines = Vec::new();
    let mut in_block = false;
    let mut removed_block = false;

    for line in content.lines() {
        if line.trim() == KIMI_CONFIG_BLOCK_BEGIN {
            in_block = true;
            removed_block = true;
            continue;
        }
        if in_block {
            if line.trim() == KIMI_CONFIG_BLOCK_END {
                in_block = false;
            }
            continue;
        }
        lines.push(line.to_string());
    }

    if !removed_block {
        return content.to_string();
    }

    let mut result = join_toml_lines(lines, trailing_newline);
    while result.ends_with("\n\n") {
        result.pop();
    }
    if result == "\n" {
        String::new()
    } else {
        result
    }
}

pub(crate) fn toml_basic_string(value: &str) -> String {
    let mut result = String::with_capacity(value.len() + 2);
    result.push('"');
    for ch in value.chars() {
        match ch {
            '"' => result.push_str("\\\""),
            '\\' => result.push_str("\\\\"),
            '\u{08}' => result.push_str("\\b"),
            '\t' => result.push_str("\\t"),
            '\n' => result.push_str("\\n"),
            '\u{0c}' => result.push_str("\\f"),
            '\r' => result.push_str("\\r"),
            ch if ch <= '\u{1f}' || ch == '\u{7f}' => {
                result.push_str(&format!("\\u{:04X}", ch as u32));
            }
            ch => result.push(ch),
        }
    }
    result.push('"');
    result
}

pub(crate) fn join_toml_lines(lines: Vec<String>, trailing_newline: bool) -> String {
    let mut result = lines.join("\n");
    if trailing_newline || result.is_empty() {
        result.push('\n');
    }
    result
}
