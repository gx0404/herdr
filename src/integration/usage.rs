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
    // 错误里带上文件路径：无法识别的 herdr 回调等情况要用户手动清理，得知道清理哪个文件。
    let updated = edit(&content, &account.agent, enabled)
        .map_err(|error| io::Error::new(error.kind(), format!("{}: {error}", path.display())))?;
    if updated != content {
        super::config_file::write_config(&path, updated)?;
    }
    Ok(())
}

/// 只读检测：官方 `settings.json`（允许注释）的 `statusLine` 是否已是 herdr 的用量回调。
/// 「已接入」的唯一判据是本平台能按标记（或旧模板形态）从命令里剥离出 herdr 回调——管道
/// 拼装 / 拆解都在 `src/platform` 里（形态因平台而异），本模块只做 JSON 编辑。内容无法解析、
/// 不是对象，或 statusLine 是带 herdr 包装特征却剥不下来的命令（其它平台 / 版本 / 厂商的
/// 形态，`edit` 对它报错而不是再包一层）时为 `None`，调用方据此给出保守文案，而不是引导
/// 用户去点一个必然失败的开关。
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
    crate::platform::strip_usage_statusline(agent, command)
        .ok()
        .map(|renderer| renderer.is_some())
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
    // 无法识别的 herdr 回调（其它平台 / 版本 / 厂商的形态）在这里报错，启用与解除都不改写它。
    let renderer = crate::platform::strip_usage_statusline(agent, original)?;
    let next = if enabled {
        // 已是本平台包装时按剥出的渲染命令重新拼装：新形态原样不动（幂等），旧模板顺势升级。
        let next = crate::platform::usage_statusline_pipeline(
            agent,
            renderer.as_deref().unwrap_or(original),
        )?;
        if next == original {
            return Ok(content.into());
        }
        next
    } else {
        match renderer {
            None => return Ok(content.into()),
            Some(renderer) if renderer.is_empty() => {
                // 独立回调没有可还原的渲染命令：statusLine 只有 type / command（是启用时建的）时
                // 整体移除；用户还留有 padding、refreshInterval 等键时只去掉 command，不动用户的键。
                let keeps_user_keys = current.as_object().is_some_and(|fields| {
                    fields
                        .keys()
                        .any(|key| !matches!(key.as_str(), "type" | "command"))
                });
                if keeps_user_keys {
                    if let Some(property) = statusline.get("command") {
                        property.remove();
                    }
                } else if let Some(property) = object.get("statusLine") {
                    property.remove();
                }
                return Ok(root.to_string());
            }
            Some(renderer) => renderer,
        }
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
        let standalone = crate::platform::usage_statusline_pipeline("claude", "").unwrap();
        let settings = format!(
            "{{\n// 保留注释\n\"statusLine\":{{\"type\":\"command\",\"command\":{}}}}}",
            serde_json::Value::String(standalone.clone())
        );
        assert_eq!(statusline_enabled(&settings, "claude"), Some(true));
        assert_eq!(
            statusline_enabled(&settings, "antigravity"),
            None,
            "命令按 agent 区分：别的厂商的 herdr 回调对本厂商是无法识别的包装，不能算未接入"
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

    fn command_of(settings: &str) -> serde_json::Value {
        let value: serde_json::Value = serde_json::from_str(settings).unwrap();
        value["statusLine"]["command"].clone()
    }

    /// 旧二进制写进用户 settings.json 的模板（逐字保留 padding 等其它键）：新二进制必须仍
    /// 识别为已接入，并能解除回原渲染命令；再次启用则升级为带标记的新形态。
    #[cfg(unix)]
    #[test]
    fn legacy_statusline_template_is_recognized_and_removed_by_the_new_binary() {
        let legacy = "{\n  \"statusLine\": {\n    \"type\": \"command\",\n    \"padding\": 0,\n    \"refreshInterval\": 30,\n    \"hideVimModeIndicator\": true,\n    \"command\": \"(if [ \\\"${HERDR_ENV:-}\\\" = 1 ] && [ -n \\\"${HERDR_BIN_PATH:-}\\\" ]; then \\\"$HERDR_BIN_PATH\\\" api usage-report --agent claude --passthrough; else cat; fi) | (\\nbash /home/xyz/.claude/statusline-command.sh\\n)\"\n  }\n}";
        assert_eq!(
            command_of(legacy),
            "(if [ \"${HERDR_ENV:-}\" = 1 ] && [ -n \"${HERDR_BIN_PATH:-}\" ]; then \"$HERDR_BIN_PATH\" api usage-report --agent claude --passthrough; else cat; fi) | (\nbash /home/xyz/.claude/statusline-command.sh\n)",
            "fixture 就是旧模板本身"
        );
        assert_eq!(statusline_enabled(legacy, "claude"), Some(true));

        let restored = edit(legacy, "claude", false).unwrap();
        assert_eq!(
            command_of(&restored),
            "bash /home/xyz/.claude/statusline-command.sh"
        );
        let restored_value: serde_json::Value = serde_json::from_str(&restored).unwrap();
        assert_eq!(restored_value["statusLine"]["padding"], 0);
        assert_eq!(restored_value["statusLine"]["refreshInterval"], 30);
        assert_eq!(statusline_enabled(&restored, "claude"), Some(false));

        let upgraded = edit(legacy, "claude", true).unwrap();
        let upgraded_command = command_of(&upgraded);
        let upgraded_command = upgraded_command.as_str().unwrap();
        assert!(upgraded_command.starts_with("# herdr-usage v1\n"));
        assert!(upgraded_command
            .contains("exec \"$HERDR_BIN_PATH\" api usage-report --agent claude --passthrough"));
        assert!(upgraded_command.ends_with("| (\nbash /home/xyz/.claude/statusline-command.sh\n)"));
        assert_eq!(
            edit(&upgraded, "claude", true).unwrap(),
            upgraded,
            "新形态幂等"
        );
        assert_eq!(
            command_of(&edit(&upgraded, "claude", false).unwrap()),
            "bash /home/xyz/.claude/statusline-command.sh"
        );

        // 旧的独立回调（没有原渲染命令）解除后整个 statusLine 移除。
        let legacy_standalone = "{\"theme\":\"dark\",\"statusLine\":{\"type\":\"command\",\"command\":\"(if [ \\\"${HERDR_ENV:-}\\\" = 1 ] && [ -n \\\"${HERDR_BIN_PATH:-}\\\" ]; then \\\"$HERDR_BIN_PATH\\\" api usage-report --agent claude; else :; fi)\"}}";
        assert_eq!(statusline_enabled(legacy_standalone, "claude"), Some(true));
        let removed: serde_json::Value =
            serde_json::from_str(&edit(legacy_standalone, "claude", false).unwrap()).unwrap();
        assert_eq!(removed, serde_json::json!({"theme": "dark"}));
    }

    /// 带 herdr 包装特征却不是本平台可识别形态的命令（手工改过的包装、从别的平台同步来的
    /// settings）：启用与解除都报可识别错误，不再包一层也不静默放过；只读检测为 `None`，
    /// 账号页据此提示手动清理，而不是引导去点一个必然失败的开关。
    #[cfg(any(unix, windows))]
    #[test]
    fn unrecognized_herdr_callback_is_rejected_instead_of_rewrapped() {
        // 旧模板少了 `-n` 判定：整串比对不匹配，但 HERDR_ENV + api usage-report 的特征俱在。
        let edited = "{\"statusLine\":{\"type\":\"command\",\"command\":\"(if [ \\\"${HERDR_ENV:-}\\\" = 1 ]; then \\\"$HERDR_BIN_PATH\\\" api usage-report --agent claude --passthrough; else cat; fi) | (\\nbash x.sh\\n)\"}}";
        assert_eq!(statusline_enabled(edited, "claude"), None);
        for enabled in [true, false] {
            let error = edit(edited, "claude", enabled).unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
            assert_eq!(
                error.to_string(),
                crate::platform::UNRECOGNIZED_USAGE_STATUSLINE
            );
        }
        // 别的厂商的回调同理：antigravity 的设置里不能把 claude 的回调再包一层。
        let other = format!(
            "{{\"statusLine\":{{\"type\":\"command\",\"command\":{}}}}}",
            serde_json::Value::String(
                crate::platform::usage_statusline_pipeline("claude", "python custom.py").unwrap()
            )
        );
        assert_eq!(statusline_enabled(&other, "antigravity"), None);
        assert!(edit(&other, "antigravity", true).is_err());
    }

    /// 文档推荐的手写集成（`herdr api usage-report --agent <agent>`）没有 herdr 包装特征：
    /// 按自定义渲染器处理——只读检测为「未接入」，解除是无害的 no-op，启用把它接在管道末端。
    #[cfg(any(unix, windows))]
    #[test]
    fn hand_written_usage_report_command_is_treated_as_a_custom_renderer() {
        let settings = "{\"statusLine\":{\"type\":\"command\",\"command\":\"herdr api usage-report --agent claude\"}}";
        assert_eq!(statusline_enabled(settings, "claude"), Some(false));
        assert_eq!(edit(settings, "claude", false).unwrap(), settings);
        let enabled = edit(settings, "claude", true).unwrap();
        assert_eq!(statusline_enabled(&enabled, "claude"), Some(true));
        assert_eq!(
            command_of(&edit(&enabled, "claude", false).unwrap()),
            "herdr api usage-report --agent claude"
        );
    }

    /// 独立回调（用户原本没有渲染命令）解除时，用户在 statusLine 里留下的其它键不能一起丢掉：
    /// 只去掉 command；statusLine 只剩 type / command（是启用时建的）才整体移除。
    #[cfg(any(unix, windows))]
    #[test]
    fn disabling_a_standalone_callback_keeps_user_statusline_keys() {
        let standalone = crate::platform::usage_statusline_pipeline("claude", "").unwrap();
        let with_user_keys = format!(
            "{{\"statusLine\":{{\"type\":\"command\",\"padding\":0,\"refreshInterval\":30,\"command\":{}}}}}",
            serde_json::Value::String(standalone.clone())
        );
        assert_eq!(statusline_enabled(&with_user_keys, "claude"), Some(true));
        let restored: serde_json::Value =
            serde_json::from_str(&edit(&with_user_keys, "claude", false).unwrap()).unwrap();
        assert_eq!(
            restored,
            serde_json::json!({"statusLine": {"type": "command", "padding": 0, "refreshInterval": 30}}),
            "padding / refreshInterval 保留，只去掉 command"
        );
        // 再次启用回到独立形态，用户键仍在。
        let enabled: serde_json::Value =
            serde_json::from_str(&edit(&restored.to_string(), "claude", true).unwrap()).unwrap();
        assert_eq!(enabled["statusLine"]["command"], standalone);
        assert_eq!(enabled["statusLine"]["refreshInterval"], 30);

        let created_by_herdr = format!(
            "{{\"theme\":\"dark\",\"statusLine\":{{\"type\":\"command\",\"command\":{}}}}}",
            serde_json::Value::String(standalone)
        );
        let removed: serde_json::Value =
            serde_json::from_str(&edit(&created_by_herdr, "claude", false).unwrap()).unwrap();
        assert_eq!(removed, serde_json::json!({"theme": "dark"}));
    }

    /// 多行、带分号与括号的渲染脚本整体进管道分组，解除后逐字节还原。
    #[cfg(any(unix, windows))]
    #[test]
    fn multiline_renderer_survives_enable_and_disable() {
        let renderer = "a; b\nc | d\n)\n(e)";
        let settings = format!(
            "{{\"statusLine\":{{\"type\":\"command\",\"command\":{}}}}}",
            serde_json::Value::String(renderer.into())
        );
        let enabled = edit(&settings, "claude", true).unwrap();
        assert_eq!(statusline_enabled(&enabled, "claude"), Some(true));
        assert_eq!(
            command_of(&edit(&enabled, "claude", false).unwrap()),
            renderer
        );
    }
}
