//! 用量 statusline 按显式菜单操作启用；已有渲染命令保留在管道末端。
use jsonc_parser::cst::{CstInputValue, CstRootNode};
use std::{io, path::PathBuf};

pub(crate) fn configure(
    account: &crate::config::UsageAccountConfig,
    enabled: bool,
) -> io::Result<()> {
    let path = match account.agent.as_str() {
        "claude" => account
            .profile_dir
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or(super::env::claude_dir()?)
            .join("settings.json"),
        "antigravity" => account
            .profile_dir
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or(
                super::env::home_dir()?
                    .join(".gemini")
                    .join("antigravity-cli"),
            )
            .join("settings.json"),
        _ => return Err(io::Error::other("此厂商未提供受支持的 statusline 配额回调")),
    };
    super::config_file::check_config_target(&path)?;
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == io::ErrorKind::NotFound => "{}".into(),
        Err(error) => return Err(error),
    };
    let updated = edit(&content, &account.agent, enabled)?;
    if updated != content {
        super::config_file::write_config(&path, updated)?;
    }
    Ok(())
}

/// 「已接入 herdr 用量回调」的唯一判据：命令恰好是本平台的独立回调命令，或以本平台的
/// passthrough 管道前缀开头。`edit` 的幂等判断与只读检测共用它，命令形态（如 Windows 的
/// `-EncodedCommand`）一改两处同步。
fn herdr_statusline_command(agent: &str, command: &str) -> bool {
    command == crate::platform::usage_statusline_command(agent, false)
        || command.starts_with(&format!(
            "{} | (\n",
            crate::platform::usage_statusline_command(agent, true)
        ))
}

/// 只读检测：官方 `settings.json`（允许注释）的 `statusLine` 是否已是 herdr 的用量回调。
/// 内容无法解析或不是对象时为 `None`，调用方据此给出保守文案。
pub(crate) fn statusline_enabled(content: &str, agent: &str) -> Option<bool> {
    let root = CstRootNode::parse(content, &jsonc_parser::ParseOptions::default()).ok()?;
    let object = root.value()?.as_object()?;
    let Some(property) = object.get("statusLine") else {
        return Some(false);
    };
    let Some(statusline) = property.object_value() else {
        return Some(false);
    };
    let current = statusline.to_serde_value()?;
    if current.get("type").and_then(serde_json::Value::as_str) != Some("command") {
        return Some(false);
    }
    let command = current
        .get("command")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    Some(herdr_statusline_command(agent, command))
}

fn edit(content: &str, agent: &str, enabled: bool) -> io::Result<String> {
    if !matches!(agent, "claude" | "antigravity") {
        return Err(io::Error::other("不支持的 statusline 厂商"));
    }
    let root = CstRootNode::parse(content, &jsonc_parser::ParseOptions::default())
        .map_err(|_| io::Error::other("官方设置 JSON 无效"))?;
    let value = root
        .value()
        .ok_or_else(|| io::Error::other("官方设置为空"))?;
    super::claude_settings::reject_duplicate_keys(
        &value,
        std::path::Path::new("statusline-settings.json"),
    )?;
    let object = value
        .as_object()
        .ok_or_else(|| io::Error::other("官方设置必须是对象"))?;
    let prefix = crate::platform::usage_statusline_command(agent, false);
    let statusline = if let Some(property) = object.get("statusLine") {
        property
            .object_value()
            .ok_or_else(|| io::Error::other("现有 statusLine 不是命令对象，未修改"))?
    } else if enabled {
        object
            .append(
                "statusLine",
                CstInputValue::Object(vec![(
                    "type".into(),
                    CstInputValue::String("command".into()),
                )]),
            )
            .object_value()
            .ok_or_else(|| io::Error::other("无法建立 statusline"))?
    } else {
        return Ok(content.into());
    };
    let current = statusline
        .to_serde_value()
        .ok_or_else(|| io::Error::other("statusline 设置无效"))?;
    if current.get("type").and_then(serde_json::Value::as_str) != Some("command") {
        return Err(io::Error::other("现有 statusLine 不是命令类型，未修改"));
    }
    let original = current
        .get("command")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let pipe = format!(
        "{} | (\n",
        crate::platform::usage_statusline_command(agent, true)
    );
    let next = if enabled {
        if herdr_statusline_command(agent, original) {
            return Ok(content.into());
        }
        if original.is_empty() {
            prefix
        } else {
            format!("{pipe}{original}\n)")
        }
    } else if let Some(original) = original
        .strip_prefix(&pipe)
        .and_then(|value| value.strip_suffix("\n)"))
    {
        original.to_owned()
    } else if original == prefix {
        if let Some(property) = object.get("statusLine") {
            property.remove();
        }
        return Ok(root.to_string());
    } else {
        return Ok(content.into());
    };
    if let Some(property) = statusline.get("command") {
        property.remove();
    }
    statusline.append("command", CstInputValue::String(next));
    Ok(root.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn original_statusline_and_comments_survive_enable_disable() {
        let original = "{\n// 保留\n\"theme\":\"dark\",\"statusLine\":{\"type\":\"command\",\"command\":\"python custom.py\",\"padding\":2}}";
        let enabled = edit(original, "claude", true).unwrap();
        assert!(enabled.contains("// 保留"));
        assert_eq!(edit(&enabled, "claude", true).unwrap(), enabled);
        let restored = edit(&enabled, "claude", false).unwrap();
        let parse = |text: &str| {
            CstRootNode::parse(text, &Default::default())
                .unwrap()
                .value()
                .unwrap()
                .to_serde_value()
                .unwrap()
        };
        assert_eq!(parse(&restored), parse(original));
    }

    /// 只读检测与 `edit` 的幂等判据同源：用本平台生成的命令做期望（Windows 上是
    /// `-EncodedCommand` 形态，settings 里不含 `api usage-report` 明文）。
    #[test]
    fn statusline_enabled_recognizes_only_this_platforms_command_shapes() {
        let standalone = crate::platform::usage_statusline_command("claude", false);
        let settings = format!(
            "{{\n// 保留注释\n\"statusLine\":{{\"type\":\"command\",\"command\":{}}}}}",
            serde_json::Value::String(standalone.clone())
        );
        assert_eq!(statusline_enabled(&settings, "claude"), Some(true));
        assert_eq!(
            statusline_enabled(&settings, "antigravity"),
            Some(false),
            "命令按 agent 区分"
        );
        // enable 后的 passthrough 管道形态同样算已接入。
        let piped = edit(
            "{\"statusLine\":{\"type\":\"command\",\"command\":\"python custom.py\"}}",
            "claude",
            true,
        )
        .unwrap();
        assert_eq!(statusline_enabled(&piped, "claude"), Some(true));
        // 明文子串不是判据：另一台机器 / 另一平台形态的命令（含 `api usage-report`）不算
        // 本机已接入，否则 Windows 的 EncodedCommand 与 Unix 的 shell 形态会互相误判。
        let foreign = "{\"statusLine\":{\"type\":\"command\",\"command\":\"powershell.exe -NoLogo -EncodedCommand AAAA api usage-report --agent claude\"}}";
        assert_eq!(statusline_enabled(foreign, "claude"), Some(false));
        let custom = "{\"statusLine\":{\"type\":\"command\",\"command\":\"python custom.py\"}}";
        assert_eq!(statusline_enabled(custom, "claude"), Some(false));
        assert_eq!(statusline_enabled("{}", "claude"), Some(false));
        assert_eq!(
            statusline_enabled("{\"statusLine\":{\"type\":\"static\"}}", "claude"),
            Some(false)
        );
        assert_eq!(statusline_enabled("{", "claude"), None);
        assert_eq!(statusline_enabled("[]", "claude"), None);
    }
}
