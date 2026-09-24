//! 用量 statusline 按显式菜单操作启用；已有渲染命令保留在管道末端。
use jsonc_parser::cst::{CstInputValue, CstRootNode};
use std::{io, path::PathBuf};

/// 官方回调接入的错误说明（经 `account.usage.integration` 的错误应答到达监控页）：按调用
/// 进程（server）的界面语言取（文档终审 D7）。
fn texts() -> &'static crate::i18n::UsageProbeTexts {
    &crate::i18n::texts().usage_probe
}

/// 支持 herdr 用量 statusline 回调的厂商：名单唯一真源。`configure` / `edit` 按它拒绝其它
/// 厂商，server 端 registry 也据此宣告 `UsageProviderInfo.supports_callback`；新增厂商只改
/// 这里（以及 `settings_path` 的目录推导与 `src/platform` 的管道形态）。
///
/// 只回答「官方 statusline 回调」这一种来源：herdr 改写厂商官方 `settings.json` 的
/// `statusLine` 来接入。由 herdr 自带集成扩展推送用量的厂商是另一种来源，见
/// `supports_extension_push`——两份名单互斥，不要合并成一个「支持回调」的谓词。
pub(crate) fn supports_statusline(agent: &str) -> bool {
    matches!(agent, "claude")
}

/// 已退役的 statusline 回调厂商（antigravity）：herdr 曾能为它们改写官方 `settings.json`，
/// 退役后只保留识别与解除——只删不装。退役前用 herdr 开过回调的用户仍能经
/// `configure(.., false)` 把包装剥掉、还原原渲染命令；启用、只读检测
/// （`statusline_enabled` 的调用方只查受支持厂商）与 server 端宣告都不认它们。
fn retired_statusline(agent: &str) -> bool {
    matches!(agent, "antigravity")
}

/// 退役厂商的官方 `settings.json` 所在默认目录，只供解除用。
fn retired_statusline_default_dir(agent: &str) -> io::Result<PathBuf> {
    match agent {
        // Antigravity CLI 的运行时数据目录：statusline 回调所在的官方 `settings.json` 在这里，
        // 与承载 hooks 的 `~/.gemini/config` 不是同一个目录（退役前 `antigravity_runtime_dir`
        // 的推导，原样保留）。
        "antigravity" => Ok(super::env::home_dir()?
            .join(".gemini")
            .join("antigravity-cli")),
        _ => Err(io::Error::other(texts().statusline_unsupported_provider)),
    }
}

/// 解除用的 `settings.json` 路径：受支持厂商同 `settings_path`；退役厂商按退役前的推导
/// 给出（`profile_dir` 同样优先），只用于解除。
fn removal_settings_path(account: &crate::config::UsageAccountConfig) -> io::Result<PathBuf> {
    if !retired_statusline(&account.agent) {
        return settings_path(account);
    }
    let dir = match account.profile_dir.as_ref() {
        Some(dir) => PathBuf::from(dir),
        None => retired_statusline_default_dir(&account.agent)?,
    };
    Ok(dir.join("settings.json"))
}

/// 用量由 herdr 自带集成扩展推送的厂商：名单唯一真源。扩展（`assets/<agent>/`）在会话事件里
/// 取数并经 socket 调 `account.usage.report`；这里没有可改写的官方 `settings.json`，所以
/// `configure` / `settings_path` 不接受这些厂商，接入与否取决于集成是否已安装。
pub(crate) fn supports_extension_push(agent: &str) -> bool {
    matches!(agent, "pi")
}

/// 账号的官方 `settings.json` 路径：`profile_dir` 优先于厂商默认目录。写入（`configure`）与
/// 只读检测（server 端 registry）共用，两边不会指向不同的文件。不支持回调的厂商报错。
pub(crate) fn settings_path(account: &crate::config::UsageAccountConfig) -> io::Result<PathBuf> {
    let default_dir = match account.agent.as_str() {
        "claude" => super::env::claude_dir(),
        _ => return Err(io::Error::other(texts().statusline_unsupported_provider)),
    };
    let dir = match account.profile_dir.as_ref() {
        Some(dir) => PathBuf::from(dir),
        None => default_dir?,
    };
    Ok(dir.join("settings.json"))
}

pub(crate) fn configure(
    account: &crate::config::UsageAccountConfig,
    enabled: bool,
) -> io::Result<()> {
    // 解除还认退役厂商（只删不装）；启用只认受支持厂商。
    let path = if enabled {
        settings_path(account)?
    } else {
        removal_settings_path(account)?
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
    if !supports_statusline(agent) {
        if !retired_statusline(agent) {
            return Err(io::Error::other(texts().statusline_unknown_provider));
        }
        if enabled {
            return Err(io::Error::other(texts().statusline_retired));
        }
    }
    let root = CstRootNode::parse(content, &jsonc_parser::ParseOptions::default())
        .map_err(|_| io::Error::other(texts().settings_invalid_json))?;
    let value = root
        .value()
        .ok_or_else(|| io::Error::other(texts().settings_empty))?;
    super::claude_settings::reject_duplicate_keys(
        &value,
        std::path::Path::new("statusline-settings.json"),
    )?;
    let object = value
        .as_object()
        .ok_or_else(|| io::Error::other(texts().settings_not_object))?;
    let statusline = if let Some(property) = object.get("statusLine") {
        property
            .object_value()
            .ok_or_else(|| io::Error::other(texts().statusline_not_object))?
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
            .ok_or_else(|| io::Error::other(texts().statusline_create_failed))?
    } else {
        return Ok(content.into());
    };
    let current = statusline
        .to_serde_value()
        .ok_or_else(|| io::Error::other(texts().statusline_invalid))?;
    if current.get("type").and_then(serde_json::Value::as_str) != Some("command") {
        return Err(io::Error::other(texts().statusline_not_command));
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
            statusline_enabled(&settings, "codex"),
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
                crate::platform::unrecognized_usage_statusline_message()
            );
        }
        // 别的厂商的回调同理：按另一个 agent 名检测时，claude 的回调是无法识别的包装。
        let other = format!(
            "{{\"statusLine\":{{\"type\":\"command\",\"command\":{}}}}}",
            serde_json::Value::String(
                crate::platform::usage_statusline_pipeline("claude", "python custom.py").unwrap()
            )
        );
        assert_eq!(statusline_enabled(&other, "codex"), None);
    }

    /// 「官方 statusline 回调」与「扩展推送」是两种来源：名单互斥；扩展推送型厂商没有可改写的
    /// 官方 settings，`edit` / `settings_path` 一律拒绝，不会去写一个不存在的 statusLine 契约。
    #[test]
    fn statusline_and_extension_push_sources_are_disjoint() {
        assert!(supports_statusline("claude") && !supports_extension_push("claude"));
        assert!(supports_extension_push("pi") && !supports_statusline("pi"));
        for agent in ["codex", "kimi", "opencode", "antigravity", "gemini"] {
            assert!(!supports_statusline(agent) && !supports_extension_push(agent));
        }
        for agent in ["pi", "codex", "antigravity"] {
            assert!(edit("{}", agent, true).is_err(), "{agent}");
            let account = crate::config::UsageAccountConfig {
                id: format!("{agent}:default"),
                agent: agent.into(),
                ..Default::default()
            };
            assert!(settings_path(&account).is_err(), "{agent}");
        }
    }

    /// 退役厂商（antigravity）只删不装：退役前 herdr 写下的包装（本平台形态）仍能识别并解除、
    /// 还原原渲染命令；启用一律报错；没有包装时解除是 no-op。
    #[test]
    fn retired_antigravity_callback_can_only_be_removed() {
        let wrapped =
            crate::platform::usage_statusline_pipeline("antigravity", "python custom.py").unwrap();
        let settings = format!(
            "{{\n// 保留\n\"statusLine\":{{\"type\":\"command\",\"command\":{},\"padding\":2}}}}",
            serde_json::Value::String(wrapped)
        );
        let restored = edit(&settings, "antigravity", false).unwrap();
        assert!(restored.contains("// 保留"), "注释保留");
        assert_eq!(command_of_jsonc(&restored), "python custom.py");
        let restored_value = CstRootNode::parse(&restored, &Default::default())
            .unwrap()
            .value()
            .unwrap()
            .to_serde_value()
            .unwrap();
        assert_eq!(restored_value["statusLine"]["padding"], 2, "用户的键不动");
        assert_eq!(
            edit(&restored, "antigravity", false).unwrap(),
            restored,
            "解除幂等"
        );

        // 独立回调（没有原渲染命令）解除后整个 statusLine 移除。
        let standalone = format!(
            "{{\"theme\":\"dark\",\"statusLine\":{{\"type\":\"command\",\"command\":{}}}}}",
            serde_json::Value::String(
                crate::platform::usage_statusline_pipeline("antigravity", "").unwrap()
            )
        );
        let removed: serde_json::Value =
            serde_json::from_str(&edit(&standalone, "antigravity", false).unwrap()).unwrap();
        assert_eq!(removed, serde_json::json!({"theme": "dark"}));

        // 只删不装：启用报错，文件内容不变的前提由调用方保证（报错即不写）。
        for content in ["{}", settings.as_str(), restored.as_str()] {
            let error = edit(content, "antigravity", true).unwrap_err();
            assert!(error.to_string().contains("退役"), "{error}");
        }
        // 其它从未支持的厂商照旧拒绝解除。
        assert!(edit(&settings, "codex", false).is_err());
    }

    /// `configure` 的解除路径按退役前的推导找到退役厂商的 settings.json（`profile_dir` 优先，
    /// 用临时目录隔离真实 HOME）：解除改写文件；启用报错且不动文件；文件不存在时解除不建文件。
    #[test]
    fn configure_removes_a_retired_antigravity_callback_without_installing() {
        let dir = std::env::temp_dir().join(format!(
            "herdr-usage-retired-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let account = crate::config::UsageAccountConfig {
            id: "antigravity:work".into(),
            agent: "antigravity".into(),
            profile_dir: Some(dir.clone()),
            ..Default::default()
        };
        let path = dir.join("settings.json");
        assert_eq!(removal_settings_path(&account).unwrap(), path);
        assert!(
            settings_path(&account).is_err(),
            "只读检测与启用仍不认退役厂商"
        );

        // 文件不存在：解除是 no-op，不建文件。
        configure(&account, false).unwrap();
        assert!(!path.exists());

        let wrapped =
            crate::platform::usage_statusline_pipeline("antigravity", "bash line.sh").unwrap();
        let settings = format!(
            "{{\"statusLine\":{{\"type\":\"command\",\"command\":{}}}}}",
            serde_json::Value::String(wrapped)
        );
        std::fs::write(&path, &settings).unwrap();
        assert!(configure(&account, true).is_err());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            settings,
            "启用失败不写文件"
        );

        configure(&account, false).unwrap();
        assert_eq!(
            command_of_jsonc(&std::fs::read_to_string(&path).unwrap()),
            "bash line.sh"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn command_of_jsonc(settings: &str) -> String {
        CstRootNode::parse(settings, &Default::default())
            .unwrap()
            .value()
            .unwrap()
            .to_serde_value()
            .unwrap()["statusLine"]["command"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
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

    /// 文档终审 D7：官方回调接入被拒的说明（经 `account.usage.integration` 的错误应答到达
    /// 监控页）按调用进程的界面语言给出——英文界面不含 CJK，中文界面是中文。
    #[test]
    fn integration_rejections_follow_the_interface_language() {
        use crate::i18n::{has_cjk, lang_guard, Lang};
        for (lang, chinese) in [(Lang::En, false), (Lang::ZhCn, true)] {
            let _guard = lang_guard(lang);
            let cases = [
                ("{", "claude", true),
                ("[]", "claude", true),
                ("{\"statusLine\": 1}", "claude", true),
                ("{\"statusLine\": {\"type\": \"static\"}}", "claude", true),
                ("{}", "codex", true),
                ("{}", "antigravity", true),
            ];
            for (content, agent, enabled) in cases {
                let message = edit(content, agent, enabled).unwrap_err().to_string();
                assert_eq!(has_cjk(&message), chinese, "{lang:?} {content}: {message}");
            }
        }
    }
}
