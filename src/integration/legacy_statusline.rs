//! 已移除的账号用量功能曾把 Claude `settings.json` 的 `statusLine.command` 包成 herdr 回调
//! （标记 `# herdr-usage v1`，Windows 上是 PowerShell `-EncodedCommand` 形态，更早还有不带
//! 标记的旧模板）。herdr 不再提供该回调，残留包装会让 statusline 丢掉原渲染输出；这里只保留
//! 剥离能力：Claude 集成安装 / 卸载时把原渲染命令还原回去。没有包装时原样返回；带 herdr 包装
//! 特征却无法识别的命令同样原样返回并记日志，不冒险改写用户配置。包装形态的解析因平台而异，
//! 在 `crate::platform::unwrap_legacy_statusline` 里。

use std::path::Path;

use jsonc_parser::cst::{CstInputValue, CstRootNode};

/// 去掉 Claude settings（允许注释）里残留的 herdr 用量 statusline 包装，保留其余内容与格式。
pub(super) fn strip_claude_statusline(content: &str, settings_path: &Path) -> String {
    strip(content, "claude", settings_path)
}

fn strip(content: &str, agent: &str, settings_path: &Path) -> String {
    let Ok(root) = CstRootNode::parse(content, &jsonc_parser::ParseOptions::default()) else {
        return content.to_owned();
    };
    let Some(object) = root.value().and_then(|value| value.as_object()) else {
        return content.to_owned();
    };
    let Some(statusline) = object
        .get("statusLine")
        .and_then(|property| property.object_value())
    else {
        return content.to_owned();
    };
    let Some(current) = statusline.to_serde_value() else {
        return content.to_owned();
    };
    if current.get("type").and_then(serde_json::Value::as_str) != Some("command") {
        return content.to_owned();
    }
    let command = current
        .get("command")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let renderer = match crate::platform::unwrap_legacy_statusline(agent, command) {
        Ok(Some(renderer)) => renderer,
        Ok(None) => return content.to_owned(),
        Err(error) => {
            tracing::warn!(
                event = "integration.legacy_statusline.unrecognized",
                agent,
                path = %settings_path.display(),
                %error,
                "statusLine 带旧 herdr 用量包装特征但无法识别，保持原样"
            );
            return content.to_owned();
        }
    };
    if renderer.is_empty() {
        // 独立回调没有可还原的渲染命令：statusLine 只有 type / command（是当年启用时建的）
        // 时整体移除；用户还留有 padding、refreshInterval 等键时只去掉 command。
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
    } else {
        if let Some(property) = statusline.get("command") {
            property.remove();
        }
        statusline.append("command", CstInputValue::String(renderer));
    }
    root.to_string()
}

#[cfg(all(test, any(unix, windows)))]
mod tests {
    use super::*;

    fn settings_path() -> &'static Path {
        Path::new("settings.json")
    }

    fn parse(text: &str) -> serde_json::Value {
        CstRootNode::parse(text, &Default::default())
            .unwrap()
            .value()
            .unwrap()
            .to_serde_value()
            .unwrap()
    }

    fn settings_with_command(command: &str, extra: &str) -> String {
        format!(
            "{{\n// 保留\n\"theme\":\"dark\",\"statusLine\":{{\"type\":\"command\",\"command\":{}{extra}}}}}",
            serde_json::Value::String(command.into())
        )
    }

    /// 带渲染命令的包装：还原原命令，注释与用户的其它键保留，再剥一次是 no-op。
    #[test]
    fn wrapped_renderer_is_restored_with_comments_and_user_keys() {
        for renderer in ["python custom.py", "a; b\nc | d\n)\n(e)"] {
            let wrapped = crate::platform::marked_legacy_statusline_wrapper("claude", renderer);
            let settings = settings_with_command(&wrapped, ",\"padding\":2");
            let restored = strip_claude_statusline(&settings, settings_path());
            assert!(restored.contains("// 保留"), "注释保留");
            let value = parse(&restored);
            assert_eq!(value["statusLine"]["command"], renderer);
            assert_eq!(value["statusLine"]["padding"], 2, "用户的键不动");
            assert_eq!(value["theme"], "dark");
            assert_eq!(
                strip_claude_statusline(&restored, settings_path()),
                restored,
                "剥离幂等"
            );
        }
    }

    /// 独立回调（用户原本没有渲染命令）：statusLine 只剩 type / command 时整体移除；用户留下的
    /// 其它键在时只去掉 command。
    #[test]
    fn standalone_callback_is_removed_but_user_statusline_keys_stay() {
        let standalone = crate::platform::marked_legacy_statusline_wrapper("claude", "");
        let created_by_herdr = settings_with_command(&standalone, "");
        assert_eq!(
            parse(&strip_claude_statusline(&created_by_herdr, settings_path())),
            serde_json::json!({"theme": "dark"})
        );
        let with_user_keys = settings_with_command(&standalone, ",\"refreshInterval\":30");
        assert_eq!(
            parse(&strip_claude_statusline(&with_user_keys, settings_path())),
            serde_json::json!({
                "theme": "dark",
                "statusLine": {"type": "command", "refreshInterval": 30}
            })
        );
    }

    /// 没有包装、用户手写的命令、别的厂商的包装、无法识别的包装与不合法的内容都原样返回。
    #[test]
    fn content_without_a_recognized_wrapper_is_returned_unchanged() {
        let other_agent =
            crate::platform::marked_legacy_statusline_wrapper("antigravity", "python custom.py");
        for content in [
            "{}".to_owned(),
            "{".to_owned(),
            "[]".to_owned(),
            "{\"statusLine\":{\"type\":\"static\"}}".to_owned(),
            "{\"statusLine\":1}".to_owned(),
            settings_with_command("python custom.py", ""),
            settings_with_command("herdr api usage-report --agent claude", ""),
            settings_with_command(&other_agent, ""),
            settings_with_command(
                "powershell.exe -NoLogo -NoProfile -NonInteractive -EncodedCommand AAAA",
                "",
            ),
        ] {
            assert_eq!(
                strip_claude_statusline(&content, settings_path()),
                content,
                "{content}"
            );
        }
    }
}
