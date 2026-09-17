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
        if original == prefix || original.starts_with(&pipe) {
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
}
