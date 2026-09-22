use crate::api::schema::IntegrationTarget;

/// Localized CLI error templates for this subcommand surface.
fn errors() -> &'static crate::i18n::CliErrorTexts {
    &crate::i18n::texts().cli_errors
}

pub(super) fn run_integration_command(args: &[String]) -> std::io::Result<i32> {
    let Some(subcommand) = args.first().map(|arg| arg.as_str()) else {
        print_integration_help();
        return Ok(2);
    };

    match subcommand {
        "install" => integration_install(&args[1..]),
        "uninstall" => integration_uninstall(&args[1..]),
        "status" => integration_status(&args[1..]),
        "help" | "--help" | "-h" => {
            print_integration_help();
            Ok(0)
        }
        _ => {
            print_integration_help();
            Ok(2)
        }
    }
}

fn integration_status(args: &[String]) -> std::io::Result<i32> {
    let outdated_only = match args {
        [] => false,
        [flag] if flag == "--outdated-only" => true,
        _ => {
            eprintln!("{}", errors().integration_status_usage);
            return Ok(2);
        }
    };

    if outdated_only {
        crate::integration::print_outdated_update_notice();
        return Ok(0);
    }

    for status in crate::integration::installed_integration_statuses() {
        let target = crate::integration::integration_target_label(status.target);
        let state = describe_integration_state(
            status.state,
            status.installed_version,
            status.expected_version,
        );
        println!("{target}: {state} ({})", status.path.display());
    }

    Ok(0)
}

fn describe_integration_state(
    state: crate::integration::IntegrationStatusKind,
    installed_version: Option<u32>,
    expected_version: u32,
) -> String {
    let t = &crate::i18n::texts().cli_output;
    let version = match installed_version {
        Some(version) => format!("v{version}"),
        None => t.integration_legacy.to_string(),
    };
    match state {
        crate::integration::IntegrationStatusKind::NotInstalled => {
            t.integration_not_installed.to_string()
        }
        crate::integration::IntegrationStatusKind::Current => {
            crate::i18n::fill(t.integration_current_fmt, &[("version", version.as_str())])
        }
        crate::integration::IntegrationStatusKind::Outdated
            if installed_version.is_some_and(|installed| installed >= expected_version) =>
        {
            crate::i18n::fill(
                t.integration_needs_repair_fmt,
                &[("version", version.as_str())],
            )
        }
        crate::integration::IntegrationStatusKind::Outdated => crate::i18n::fill(
            t.integration_outdated_fmt,
            &[
                ("version", version.as_str()),
                ("expected", &format!("v{expected_version}")),
            ],
        ),
    }
}

fn integration_install(args: &[String]) -> std::io::Result<i32> {
    let Some(target) = parse_integration_target(args, "install")? else {
        return Ok(2);
    };

    match crate::integration::install_target(target) {
        Ok(messages) => {
            print_integration_messages(messages);
            Ok(0)
        }
        Err(err) => {
            eprintln!("{err}");
            Ok(1)
        }
    }
}

fn integration_uninstall(args: &[String]) -> std::io::Result<i32> {
    let Some(target) = parse_integration_target(args, "uninstall")? else {
        return Ok(2);
    };

    match crate::integration::uninstall_target(target) {
        Ok(messages) => {
            print_integration_messages(messages);
            Ok(0)
        }
        Err(err) => {
            eprintln!("{err}");
            Ok(1)
        }
    }
}

fn print_integration_messages(messages: Vec<String>) {
    for message in messages {
        println!("{message}");
    }
}

/// CLI 只接受 `IntegrationTarget::ALL` 里的官方集成。其余名字里，能解析成冻结枚举
/// 变体的是本 fork 已退役的集成，给出明确的退役提示；否则按未知目标处理。
fn parse_integration_target(
    args: &[String],
    action: &str,
) -> std::io::Result<Option<IntegrationTarget>> {
    let Some(target) = args.first().map(|arg| arg.as_str()) else {
        eprintln!(
            "{}",
            crate::i18n::fill(errors().integration_target_usage_fmt, &[("action", action)])
        );
        return Ok(None);
    };
    if args.len() != 1 {
        eprintln!(
            "{}",
            crate::i18n::fill(errors().integration_target_usage_fmt, &[("action", action)])
        );
        return Ok(None);
    }

    match parse_integration_target_name(target) {
        IntegrationTargetName::Supported(parsed) => Ok(Some(parsed)),
        IntegrationTargetName::Retired(_) => {
            eprintln!("{}", retired_target_message(target));
            eprintln!("{}", errors().integration_targets_supported);
            Ok(None)
        }
        IntegrationTargetName::Unknown => {
            eprintln!(
                "{}",
                crate::i18n::fill(
                    errors().integration_target_unknown_fmt,
                    &[("target", target)]
                )
            );
            eprintln!("{}", errors().integration_targets_supported);
            Ok(None)
        }
    }
}

/// 退役目标的提示文案：回显用户输入的原始字符串，而不是 serde wire 名
/// （两者可能不同，如 `antigravity-cli` 的 wire 名是 `antigravity_cli`）。
fn retired_target_message(raw_input: &str) -> String {
    crate::i18n::fill(
        errors().integration_target_retired_fmt,
        &[("target", raw_input)],
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IntegrationTargetName {
    Supported(IntegrationTarget),
    Retired(IntegrationTarget),
    Unknown,
}

fn parse_integration_target_name(name: &str) -> IntegrationTargetName {
    if let Some(target) = IntegrationTarget::ALL
        .into_iter()
        .find(|target| crate::integration::integration_target_label(*target) == name)
    {
        return IntegrationTargetName::Supported(target);
    }
    match IntegrationTarget::from_wire_name(name) {
        Some(target) if target.is_retired() => IntegrationTargetName::Retired(target),
        _ => IntegrationTargetName::Unknown,
    }
}

fn print_integration_help() {
    eprintln!("herdr integration commands:");
    for action in ["install", "uninstall"] {
        for target in IntegrationTarget::ALL {
            eprintln!(
                "  herdr integration {action} {}",
                crate::integration::integration_target_label(target)
            );
        }
    }
    eprintln!("  herdr integration status [--outdated-only]");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_accepts_exactly_the_official_targets() {
        for target in IntegrationTarget::ALL {
            let label = crate::integration::integration_target_label(target);
            assert_eq!(
                parse_integration_target_name(label),
                IntegrationTargetName::Supported(target)
            );
        }
    }

    #[test]
    fn cli_reports_retired_targets_instead_of_installing_them() {
        // 退役变体仍在冻结枚举里，CLI 认得名字但不再接受。
        for name in ["cursor", "omp", "antigravity-cli", "antigravity_cli"] {
            let IntegrationTargetName::Retired(target) = parse_integration_target_name(name) else {
                panic!("{name} 应解析为退役目标");
            };
            assert!(target.is_retired());
        }
        // 从未进入冻结枚举的名字（含已删除的 CLI-only 旁路）是未知目标。
        for name in ["letta", "zcode", "", "Claude"] {
            assert_eq!(
                parse_integration_target_name(name),
                IntegrationTargetName::Unknown
            );
        }
    }

    #[test]
    fn retired_target_message_echoes_raw_input_not_wire_name() {
        // serde wire 名用下划线（`antigravity_cli`），用户很可能按 CLI 惯例敲
        // 连字符（`antigravity-cli`）；提示必须原样回显用户输入。
        let raw = "antigravity-cli";
        let target = IntegrationTarget::from_wire_name(raw).expect("known retired variant");
        assert!(target.is_retired());
        assert_ne!(target.wire_name(), raw, "夹具前提：wire 名应与原始输入不同");

        let message = retired_target_message(raw);
        assert!(
            message.contains(raw),
            "提示应包含用户输入的原始字符串 {raw:?}：{message}"
        );
        assert!(
            !message.contains(&target.wire_name()),
            "提示不应回显 serde wire 名 {:?}：{message}",
            target.wire_name()
        );
    }
}
