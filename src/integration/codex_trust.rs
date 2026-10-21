//! codex 钩子的信任状态。
//!
//! codex 只运行用户信任过的钩子：`hooks.json` 里有新增或改过的钩子时，首启先弹
//! 「Hooks need review」，选「Continue without trusting」则这些钩子静默不跑，
//! herdr 拿不到会话身份与活动信号。信任记录写在 codex 的 `config.toml`：
//! `[hooks.state."<hooks.json 路径>:<事件>:<组序号>:<钩子序号>"]` 下的
//! `trusted_hash`；`enabled = false` 表示用户在 codex 的 `/hooks` 里停用了它。
//!
//! 取证基线：codex 0.150 / 0.163（官方仓库 tag `rust-v0.150.0` /
//! `rust-v0.163.0`，只读核对）。
//! - 键：`codex-rs/hooks/src/lib.rs::hook_key`，事件名取 `hook_event_key_label`
//!   （`SessionStart` → `session_start`）；路径取 `hooks.json` 的完整路径，
//!   `CODEX_HOME` 设了时 codex 会先 canonicalize。
//! - 哈希：`codex-rs/hooks/src/engine/discovery.rs::hook_hash` 把规范化的钩子身份
//!   `{event_name, matcher?, hooks: [handler]}` 转成 TOML 值，再由
//!   `codex-rs/config/src/fingerprint.rs::version_for_toml` 转 JSON、按键排序、紧凑
//!   序列化后取 sha256，写成 `sha256:<hex>`。command 型 handler 的规范化字段是
//!   `type`、`command`、`timeout` 与配置中的 `async`（缺省 false），其余可选字段
//!   为空时不出现。
//! - 归一：`discovery.rs::normalize_command_hook` 对 `SessionEnd` / `Interrupt`
//!   使用缺省 1 秒并夹紧到 1–3 秒；其他事件缺省 600 秒、至少 1 秒。除 `SessionEnd`
//!   外都支持后台运行；`SessionEnd` 即使配置 async 也同步跑，但哈希保留配置原值。
//! - 真机核对：按本算法重算 herdr 三条钩子的哈希，与 codex 在用户选「Trust all and
//!   continue」后写入的 `trusted_hash` 逐一相同（见本文件测试）。
//!
//! 只对 herdr 自己写的形状（组里只有 `hooks`，handler 只有 `type`/`command`/
//! `timeout`/`async`）下结论；条目被改成别的形状时不猜，宁可不提示也不误报。

use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::targets::CodexManagedHook;

/// herdr 的一条 codex 钩子在 codex 眼里的状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CodexHookTrust {
    /// 记录的哈希与当前钩子一致，codex 会运行它。
    Trusted,
    /// 没有信任记录：新装，或首启选了「Continue without trusting」。
    Untrusted,
    /// 信任之后钩子内容变了，codex 下次启动会再要求审阅。
    Modified,
    /// 用户在 codex 里停用了它。
    Disabled,
}

/// herdr 的 codex 钩子整体还差什么。读不到或认不出时两项都是 `false`。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct CodexHooksTrustSummary {
    /// 至少一条钩子未信任或信任后改过：codex 下次启动会弹「Hooks need review」。
    pub needs_review: bool,
    /// 至少一条钩子被用户在 codex 里停用。
    pub disabled: bool,
}

/// 读 `hooks.json` 与 `config.toml`，汇总 herdr 管理的钩子（`managed`）
/// 在 codex 里的信任状态。
pub(crate) fn codex_hooks_trust_summary(
    hooks_path: &Path,
    config_path: &Path,
    managed: &[CodexManagedHook],
) -> CodexHooksTrustSummary {
    let Some(hooks_file) = fs::read_to_string(hooks_path)
        .ok()
        .and_then(|content| serde_json::from_str::<Value>(&content).ok())
    else {
        return CodexHooksTrustSummary::default();
    };
    // 还没有 config.toml 就是还没有任何信任记录。
    let config = fs::read_to_string(config_path).unwrap_or_default();
    let mut key_sources = vec![hooks_path.display().to_string()];
    if let Ok(canonical) = hooks_path.canonicalize() {
        let canonical = canonical.display().to_string();
        if !key_sources.contains(&canonical) {
            key_sources.push(canonical);
        }
    }

    let mut summary = CodexHooksTrustSummary::default();
    for trust in managed_hooks_trust(&key_sources, &hooks_file, &config, managed)
        .into_iter()
        .flatten()
    {
        match trust {
            CodexHookTrust::Trusted => {}
            CodexHookTrust::Untrusted | CodexHookTrust::Modified => summary.needs_review = true,
            CodexHookTrust::Disabled => summary.disabled = true,
        }
    }
    summary
}

/// 逐条给出 `managed` 里每条钩子的状态；在 `hooks.json` 里找不到、形状认不出或
/// `config.toml` 解析失败时该条为 `None`。`key_sources` 是 codex 可能用作键前缀的
/// `hooks.json` 路径写法。
pub(crate) fn managed_hooks_trust(
    key_sources: &[String],
    hooks_file: &Value,
    config: &str,
    managed: &[CodexManagedHook],
) -> Vec<Option<CodexHookTrust>> {
    let table = match toml::from_str::<toml::Table>(config) {
        Ok(table) => table,
        Err(err) => {
            tracing::debug!(
                event = "integration.codex_trust_config_unreadable",
                subsystem = "integration",
                error = %err,
                "codex config.toml could not be parsed for hook trust"
            );
            return vec![None; managed.len()];
        }
    };
    let states = table
        .get("hooks")
        .and_then(toml::Value::as_table)
        .and_then(|hooks| hooks.get("state"))
        .and_then(toml::Value::as_table);

    managed
        .iter()
        .map(|hook| {
            let (group_index, handler_index, timeout, background) =
                find_handler(hooks_file, hook.event, &hook.command)?;
            let current = codex_hook_trust_hash(hook.event, &hook.command, timeout, background)?;
            let label = codex_event_key_label(hook.event)?;
            let state = states.and_then(|states| {
                key_sources.iter().find_map(|source| {
                    states
                        .get(&format!("{source}:{label}:{group_index}:{handler_index}"))
                        .and_then(toml::Value::as_table)
                })
            });
            Some(classify(state, &current))
        })
        .collect()
}

fn classify(state: Option<&toml::Table>, current: &str) -> CodexHookTrust {
    let Some(state) = state else {
        return CodexHookTrust::Untrusted;
    };
    if state.get("enabled").and_then(toml::Value::as_bool) == Some(false) {
        return CodexHookTrust::Disabled;
    }
    match state.get("trusted_hash").and_then(toml::Value::as_str) {
        None => CodexHookTrust::Untrusted,
        Some(recorded) if recorded == current => CodexHookTrust::Trusted,
        Some(_) => CodexHookTrust::Modified,
    }
}

/// 在 `hooks.json` 的 `event` 下找 herdr 写的那条 handler：返回组序号、组内序号、
/// 配置超时与是否后台。组或 handler 带了 herdr 不写的字段时返回 `None`。
fn find_handler(
    hooks_file: &Value,
    event: &str,
    command: &str,
) -> Option<(usize, usize, Option<u64>, bool)> {
    let groups = hooks_file.get("hooks")?.get(event)?.as_array()?;
    for (group_index, group) in groups.iter().enumerate() {
        let Some(handlers) = group.get("hooks").and_then(Value::as_array) else {
            continue;
        };
        for (handler_index, handler) in handlers.iter().enumerate() {
            if handler.get("command").and_then(Value::as_str) != Some(command) {
                continue;
            }
            let group_is_plain = group
                .as_object()
                .is_some_and(|object| object.keys().all(|key| key == "hooks"));
            let handler_is_plain = handler.as_object().is_some_and(|object| {
                object.get("type").and_then(Value::as_str) == Some("command")
                    && object.iter().all(|(key, value)| match key.as_str() {
                        "type" | "command" => true,
                        "timeout" => value.as_u64().is_some(),
                        "async" => value.as_bool().is_some(),
                        _ => false,
                    })
            });
            if !group_is_plain || !handler_is_plain {
                return None;
            }
            let timeout = handler.get("timeout").and_then(Value::as_u64);
            let background = handler
                .get("async")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            return Some((group_index, handler_index, timeout, background));
        }
    }
    None
}

/// codex 为一条无 matcher 的 command 钩子记的信任哈希。`event` 用 `hooks.json` 里的
/// 事件名（`SessionStart`）；按 codex 规则归一超时，未知事件返回 `None`。
pub(crate) fn codex_hook_trust_hash(
    event: &str,
    command: &str,
    timeout_sec: Option<u64>,
    background: bool,
) -> Option<String> {
    let identity = codex_hook_identity(event, command, timeout_sec, background)?;
    let digest = Sha256::digest(identity.as_bytes());
    let mut hash = String::with_capacity("sha256:".len() + digest.len() * 2);
    hash.push_str("sha256:");
    for byte in digest {
        // 写进 String 不会失败。
        let _ = write!(hash, "{byte:02x}");
    }
    Some(hash)
}

fn codex_hook_identity(
    event: &str,
    command: &str,
    timeout_sec: Option<u64>,
    background: bool,
) -> Option<String> {
    let label = codex_event_key_label(event)?;
    let timeout_sec = if matches!(label, "session_end" | "interrupt") {
        timeout_sec.unwrap_or(1).clamp(1, 3)
    } else {
        timeout_sec.unwrap_or(600).max(1)
    };
    // 规范化身份按键排序后的紧凑 JSON：顶层 event_name < hooks，handler 内
    // async < command < timeout < type。
    Some(format!(
        "{{\"event_name\":{},\"hooks\":[{{\"async\":{background},\"command\":{},\"timeout\":{timeout_sec},\"type\":\"command\"}}]}}",
        Value::String(label.to_string()),
        Value::String(command.to_string()),
    ))
}

/// codex 持久化键里的事件名（`hook_event_key_label`）。
fn codex_event_key_label(event: &str) -> Option<&'static str> {
    Some(match event {
        "PreToolUse" => "pre_tool_use",
        "PermissionRequest" => "permission_request",
        "PostToolUse" => "post_tool_use",
        "PreCompact" => "pre_compact",
        "PostCompact" => "post_compact",
        "SessionStart" => "session_start",
        "SessionEnd" => "session_end",
        "UserPromptSubmit" => "user_prompt_submit",
        "SubagentStart" => "subagent_start",
        "SubagentStop" => "subagent_stop",
        "Stop" => "stop",
        "Interrupt" => "interrupt",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::json;

    const HOOKS_PATH: &str = "/home/xyz/.codex/hooks.json";

    fn command(action: &str) -> String {
        format!("bash '/home/xyz/.codex/herdr-agent-state.sh' {action}")
    }

    fn managed_hook(event: &'static str, command: String) -> CodexManagedHook {
        CodexManagedHook {
            event,
            command,
            timeout: 10,
            background: false,
        }
    }

    fn managed() -> Vec<CodexManagedHook> {
        vec![
            managed_hook("SessionStart", command("session")),
            managed_hook("SubagentStart", command("activity")),
            managed_hook("SubagentStop", command("activity")),
        ]
    }

    fn hooks_file() -> Value {
        let group = |action: &str| json!([{ "hooks": [{ "command": command(action), "timeout": 10, "type": "command" }] }]);
        json!({
            "hooks": {
                "SessionStart": group("session"),
                "SubagentStart": group("activity"),
                "SubagentStop": group("activity"),
            }
        })
    }

    /// codex 0.156.1 在真机验证时（证据 `evidence/realcli-729a9b0f/codex-02`，随后
    /// 选了「Trust all and continue」）为 herdr v9 的三条钩子写下的 `trusted_hash`。
    const RECORDED: [(&str, &str); 3] = [
        (
            "session_start",
            "sha256:8716ce4dc8b6c5c17f30ae59474662276953df942ef66e2b42041a50943df94b",
        ),
        (
            "subagent_start",
            "sha256:c55cbf980f4dd91d6beb65ab83f59175f99f1b900d051c27ae0d87200dac4023",
        ),
        (
            "subagent_stop",
            "sha256:3e3a2ac307e14d355478ac57f9d146d48796bd55f6ba9318be91b95f9f99f662",
        ),
    ];

    fn recorded_config(extra: &str) -> String {
        let mut config = String::from("model = \"gpt-6-luna\"\n");
        for (label, hash) in RECORDED {
            config.push_str(&format!(
                "\n[hooks.state.\"{HOOKS_PATH}:{label}:0:0\"]\ntrusted_hash = \"{hash}\"\n{extra}"
            ));
        }
        config
    }

    #[test]
    fn hook_hash_matches_what_codex_recorded() {
        for (hook, (_, recorded)) in managed().iter().zip(RECORDED) {
            assert_eq!(
                codex_hook_trust_hash(hook.event, &hook.command, Some(10), false).as_deref(),
                Some(recorded),
                "{}",
                hook.event
            );
        }
    }

    #[test]
    fn recorded_hashes_count_as_trusted_and_anything_else_needs_review() {
        let sources = [HOOKS_PATH.to_string()];

        let trusted =
            managed_hooks_trust(&sources, &hooks_file(), &recorded_config(""), &managed());
        assert_eq!(trusted, vec![Some(CodexHookTrust::Trusted); 3]);

        // 没有任何记录：新装，或首启选了「Continue without trusting」。
        let fresh = managed_hooks_trust(&sources, &hooks_file(), "model = \"x\"\n", &managed());
        assert_eq!(fresh, vec![Some(CodexHookTrust::Untrusted); 3]);

        // 信任之后命令变了（例如 herdr 换了脚本路径）：codex 会再问。
        let mut moved = hooks_file();
        moved["hooks"]["SessionStart"][0]["hooks"][0]["command"] =
            json!("bash '/opt/herdr/herdr-agent-state.sh' session");
        let managed_moved = vec![managed_hook(
            "SessionStart",
            "bash '/opt/herdr/herdr-agent-state.sh' session".to_string(),
        )];
        assert_eq!(
            managed_hooks_trust(&sources, &moved, &recorded_config(""), &managed_moved),
            vec![Some(CodexHookTrust::Modified)]
        );

        let disabled = managed_hooks_trust(
            &sources,
            &hooks_file(),
            &recorded_config("enabled = false\n"),
            &managed(),
        );
        assert_eq!(disabled, vec![Some(CodexHookTrust::Disabled); 3]);
    }

    #[test]
    fn trust_is_looked_up_by_position_and_under_any_known_path_spelling() {
        // 同一事件下 herdr 的钩子排在用户自己的钩子之后：键里的组序号是 1。
        let mut hooks = hooks_file();
        hooks["hooks"]["SessionStart"] = json!([
            { "hooks": [{ "command": "echo mine", "type": "command" }] },
            { "hooks": [{ "command": command("session"), "timeout": 10, "type": "command" }] },
        ]);
        let config = format!(
            "[hooks.state.\"/real/codex/hooks.json:session_start:1:0\"]\ntrusted_hash = \"{}\"\n",
            RECORDED[0].1
        );
        let session_only = vec![managed_hook("SessionStart", command("session"))];

        let plain = managed_hooks_trust(&[HOOKS_PATH.to_string()], &hooks, &config, &session_only);
        assert_eq!(plain, vec![Some(CodexHookTrust::Untrusted)]);

        let sources = [HOOKS_PATH.to_string(), "/real/codex/hooks.json".to_string()];
        let canonical = managed_hooks_trust(&sources, &hooks, &config, &session_only);
        assert_eq!(canonical, vec![Some(CodexHookTrust::Trusted)]);
    }

    #[test]
    fn unfamiliar_shapes_and_unreadable_config_get_no_verdict() {
        let sources = [HOOKS_PATH.to_string()];
        let mut hooks = hooks_file();
        hooks["hooks"]["SessionStart"][0]["hooks"][0]["statusMessage"] = json!("syncing");
        hooks["hooks"]["SubagentStart"][0]["matcher"] = json!("worker");
        hooks["hooks"]
            .as_object_mut()
            .map(|events| events.remove("SubagentStop"));

        assert_eq!(
            managed_hooks_trust(&sources, &hooks, &recorded_config(""), &managed()),
            vec![None, None, None]
        );
        assert_eq!(
            managed_hooks_trust(&sources, &hooks_file(), "not = [toml", &managed()),
            vec![None, None, None]
        );
    }

    #[test]
    fn async_hook_hash_keeps_the_configured_value_in_the_identity() {
        let command = command("activity");
        let identity = codex_hook_identity("SubagentStart", &command, Some(10), true).unwrap();
        assert_eq!(
            identity,
            format!(
                "{{\"event_name\":\"subagent_start\",\"hooks\":[{{\"async\":true,\"command\":{},\"timeout\":10,\"type\":\"command\"}}]}}",
                json!(command)
            )
        );
        let synchronous =
            codex_hook_trust_hash("SubagentStart", &command, Some(10), false).unwrap();
        let asynchronous =
            codex_hook_trust_hash("SubagentStart", &command, Some(10), true).unwrap();
        assert_ne!(synchronous, asynchronous);
        assert_eq!(
            asynchronous,
            format!("sha256:{:x}", Sha256::digest(identity.as_bytes()))
        );
        // SessionEnd 同步执行，但身份保留配置里的 async 原值。
        assert_ne!(
            codex_hook_trust_hash("SessionEnd", &command, None, false).unwrap(),
            codex_hook_trust_hash("SessionEnd", &command, None, true).unwrap()
        );
    }

    #[test]
    fn interrupt_and_session_end_hashes_normalize_the_timeout() {
        let command = command("idle");
        for event in ["Interrupt", "SessionEnd"] {
            let hash = |timeout| codex_hook_trust_hash(event, &command, timeout, false).unwrap();
            let default = hash(None);
            let clamped = hash(Some(10));
            let two = hash(Some(2));
            assert_eq!(default, hash(Some(1)), "{event}");
            assert_eq!(default, hash(Some(0)), "{event}");
            assert_eq!(clamped, hash(Some(3)), "{event}");
            assert_ne!(default, clamped, "{event}");
            assert_ne!(default, two, "{event}");
            assert_ne!(two, clamped, "{event}");
            for (configured, normalized) in [(None, 1), (Some(10), 3), (Some(2), 2)] {
                let identity: Value = serde_json::from_str(
                    &codex_hook_identity(event, &command, configured, false).unwrap(),
                )
                .unwrap();
                assert_eq!(identity["hooks"][0]["timeout"], normalized, "{event}");
            }
        }
        assert_eq!(
            codex_hook_trust_hash("Unknown", &command, None, false),
            None
        );
        assert_eq!(
            codex_hook_trust_hash("Stop", &command, None, false),
            codex_hook_trust_hash("Stop", &command, Some(600), false)
        );
        assert_eq!(
            codex_hook_trust_hash("Stop", &command, Some(0), false),
            codex_hook_trust_hash("Stop", &command, Some(1), false)
        );
    }

    #[test]
    fn find_handler_accepts_async_and_preserves_missing_timeout() {
        let mut hooks = hooks_file();
        let command = command("activity");
        for background in [false, true] {
            hooks["hooks"]["SubagentStart"][0]["hooks"][0]["async"] = json!(background);
            assert_eq!(
                find_handler(&hooks, "SubagentStart", &command),
                Some((0, 0, Some(10), background))
            );
        }
        hooks["hooks"]["SubagentStart"][0]["hooks"][0]
            .as_object_mut()
            .unwrap()
            .remove("timeout");
        assert_eq!(
            find_handler(&hooks, "SubagentStart", &command),
            Some((0, 0, None, true))
        );
        hooks["hooks"]["SubagentStart"][0]["hooks"][0]["statusMessage"] = json!("syncing");
        assert_eq!(find_handler(&hooks, "SubagentStart", &command), None);
    }

    #[test]
    fn trust_uses_actual_async_and_timeout_instead_of_install_defaults() {
        let sources = [HOOKS_PATH.to_string()];
        let mut hooks = hooks_file();
        // v9 的旧同步 activity 钩子仍按真实文件计算，不能套用新安装表里的后台值。
        let mut hooks_to_manage = managed();
        hooks_to_manage[1].background = true;
        hooks_to_manage[1].timeout = 30;
        assert_eq!(
            managed_hooks_trust(&sources, &hooks, &recorded_config(""), &hooks_to_manage),
            vec![Some(CodexHookTrust::Trusted); 3]
        );
        hooks["hooks"]["SubagentStart"][0]["hooks"][0]["async"] = json!(true);
        assert_eq!(
            managed_hooks_trust(&sources, &hooks, &recorded_config(""), &hooks_to_manage)[1],
            Some(CodexHookTrust::Modified)
        );
        hooks["hooks"]["SubagentStart"][0]["hooks"][0]["async"] = json!(false);
        assert_eq!(
            managed_hooks_trust(&sources, &hooks, &recorded_config(""), &hooks_to_manage)[1],
            Some(CodexHookTrust::Trusted)
        );
    }

    #[test]
    fn trust_summary_includes_interrupt() {
        let dir = crate::config::test_dirs::TempDir::new("codex-interrupt-trust");
        let hooks_path = dir.join("hooks.json");
        let config_path = dir.join("config.toml");
        let command = command("idle");
        let hooks = json!({"hooks": {"Interrupt": [{"hooks": [
            {"type": "command", "command": command, "timeout": 10}
        ]}]}});
        fs::write(&hooks_path, hooks.to_string()).unwrap();
        let managed = [managed_hook("Interrupt", command.clone())];
        assert!(codex_hooks_trust_summary(&hooks_path, &config_path, &managed).needs_review);
        let hash = codex_hook_trust_hash("Interrupt", &command, Some(3), false).unwrap();
        let config = format!(
            "[hooks.state.{}]\ntrusted_hash = \"{hash}\"\n",
            super::super::config_edit::toml_basic_string(&format!(
                "{}:interrupt:0:0",
                hooks_path.display()
            ))
        );
        fs::write(&config_path, &config).unwrap();
        assert_eq!(
            codex_hooks_trust_summary(&hooks_path, &config_path, &managed),
            CodexHooksTrustSummary::default()
        );
        fs::write(&config_path, format!("{config}enabled = false\n")).unwrap();
        assert_eq!(
            codex_hooks_trust_summary(&hooks_path, &config_path, &managed),
            CodexHooksTrustSummary {
                needs_review: false,
                disabled: true
            }
        );
    }
}
