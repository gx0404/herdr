use std::io::Write;

use clap::{Arg, ArgAction, ArgGroup, Command, ValueHint};

mod completion;
mod machine;

pub(super) fn command() -> Command {
    let t = &crate::i18n::texts().cli_help;
    let command = Command::new("herdr")
        .about(t.about)
        .disable_help_flag(true)
        .disable_version_flag(true)
        .arg(help_flag())
        .arg(option("session", "NAME").help(t.session_help))
        .arg(option("machine", "LABEL-OR-ID").help(t.machine_help))
        .arg(option("remote", "TARGET").help(t.remote_help))
        .arg(
            option("remote-keybindings", "MODE")
                .value_parser(["local", "server"])
                .help(t.remote_keybindings_help),
        )
        .arg(flag("handoff").help(t.handoff_help))
        .arg(flag("default-config").help(t.default_config_help))
        .arg(flag("skill").help(t.skill_help))
        .arg(
            Arg::new("version")
                .short('V')
                .long("version")
                .action(ArgAction::SetTrue)
                .help(t.version_help),
        )
        .subcommand(completion::command())
        .subcommand(update_command())
        .subcommand(status_command())
        .subcommand(config_command())
        .subcommand(channel_command())
        .subcommand(machine::command())
        .subcommand(broadcast_command())
        .subcommand(snippet_command())
        .subcommand(server_command())
        .subcommand(api_command())
        .subcommand(workspace_command())
        .subcommand(worktree_command())
        .subcommand(tab_command())
        .subcommand(notification_command())
        .subcommand(agent_command())
        .subcommand(pane_command())
        .subcommand(terminal_command())
        .subcommand(session_command())
        .subcommand(integration_command())
        .subcommand(plugin_command());
    configure_help(command, 0)
}

fn configure_help(command: Command, depth: usize) -> Command {
    let command = if depth == 0 {
        command
    } else {
        command.disable_help_flag(false)
    };
    let command = if depth == 1 && command.has_subcommands() {
        command.after_help(super::agent_help_footer())
    } else {
        command
    };
    command
        .disable_help_subcommand(true)
        .mut_subcommands(|subcommand| configure_help(subcommand, depth + 1))
}

pub(super) fn print_requested_help(args: &[String]) -> std::io::Result<bool> {
    let mut stdout = std::io::stdout().lock();
    write_requested_help(args, &mut stdout, crate::platform::begin_cli_output)
}

fn write_requested_help(
    args: &[String],
    output: &mut impl Write,
    before_write: impl FnOnce(),
) -> std::io::Result<bool> {
    let Some(help_index) = args
        .iter()
        .position(|arg| matches!(arg.as_str(), "--help" | "-h"))
    else {
        return Ok(false);
    };
    if help_index < 2 {
        return Ok(false);
    }
    if args[1..help_index].iter().any(|arg| arg == "--") {
        return Ok(false);
    }

    let mut root = command();
    root.build();
    let mut selected = &mut root;
    let mut path = vec!["herdr".to_string()];
    for segment in &args[1..help_index] {
        if selected.find_subcommand(segment).is_none() {
            break;
        }
        path.push(segment.clone());
        selected = selected
            .find_subcommand_mut(segment)
            .expect("subcommand checked immediately before mutable lookup");
    }
    if path.len() == 1 || help_index != path.len() {
        return Ok(false);
    }

    selected.set_bin_name(path.join(" "));
    before_write();
    selected.write_long_help(&mut *output)?;
    writeln!(output)?;
    Ok(true)
}

fn update_command() -> Command {
    let t = &crate::i18n::texts().cli_help;
    Command::new("update")
        .about(t.update_about)
        .arg(flag("handoff").help(t.update_handoff_help))
}

fn status_command() -> Command {
    let t = &crate::i18n::texts().cli_help;
    Command::new("status")
        .about(t.status_about)
        .arg(json_flag())
        .subcommand(
            Command::new("server")
                .about(t.status_server_about)
                .arg(json_flag()),
        )
        .subcommand(
            Command::new("client")
                .about(t.status_client_about)
                .arg(json_flag()),
        )
}

fn config_command() -> Command {
    let t = &crate::i18n::texts().cli_help;
    Command::new("config")
        .about(t.config_about)
        .subcommand(Command::new("check").about(t.config_check_about))
        .subcommand(Command::new("reset-keys").about(t.config_reset_keys_about))
}

fn channel_command() -> Command {
    let t = &crate::i18n::texts().cli_help;
    Command::new("channel")
        .about(t.channel_about)
        .subcommand(Command::new("show").about(t.channel_show_about))
        .subcommand(
            Command::new("set").about(t.channel_set_about).arg(
                Arg::new("channel")
                    .value_name("CHANNEL")
                    .required(true)
                    .value_parser(["stable", "preview"]),
            ),
        )
}

fn broadcast_command() -> Command {
    let t = &crate::i18n::texts().cli_help;
    Command::new("broadcast")
        .about(t.broadcast_about)
        .subcommand(
            Command::new("status")
                .about(t.broadcast_status_about)
                .arg(json_flag()),
        )
        .subcommand(Command::new("enable").about(t.broadcast_enable_about))
        .subcommand(Command::new("disable").about(t.broadcast_disable_about))
        .subcommand(
            Command::new("add")
                .about(t.broadcast_add_about)
                .arg(option("machine", "LABEL-OR-ID"))
                .arg(option("pane", "PANE_ID").required(true)),
        )
        .subcommand(
            Command::new("remove")
                .about(t.broadcast_remove_about)
                .arg(required("target-number", "NUMBER")),
        )
        .subcommand(Command::new("clear").about(t.broadcast_clear_about))
        .subcommand(
            Command::new("send")
                .about(t.broadcast_send_about)
                .arg(required("text", "TEXT"))
                .arg(flag("no-enter"))
                .arg(json_flag()),
        )
}

fn snippet_command() -> Command {
    let t = &crate::i18n::texts().cli_help;
    Command::new("snippet")
        .about(t.snippet_about)
        .subcommand(
            Command::new("list")
                .about(t.snippet_list_about)
                .arg(json_flag()),
        )
        .subcommand(
            Command::new("add")
                .about(t.snippet_add_about)
                .arg(option("label", "TEXT").required(true))
                .arg(option("command", "COMMAND").required(true))
                .arg(option("description", "TEXT"))
                .arg(repeatable_option("variable", "NAME"))
                .arg(repeatable_option("tag", "TAG")),
        )
        .subcommand(
            Command::new("remove")
                .about(t.snippet_remove_about)
                .arg(required("snippet", "ID_OR_LABEL")),
        )
        .subcommand(
            Command::new("run")
                .about(t.snippet_run_about)
                .arg(required("snippet", "ID_OR_LABEL"))
                .arg(repeatable_option("machine", "LABEL-OR-ID"))
                .arg(flag("local"))
                .arg(repeatable_option("pane", "PANE_ID").required(true))
                .arg(repeatable_option("var", "NAME=VALUE"))
                .arg(flag("no-enter"))
                .arg(json_flag()),
        )
}

fn server_command() -> Command {
    let t = &crate::i18n::texts().cli_help;
    let command = Command::new("server")
        .about(t.server_about)
        .subcommand(super::server::live_handoff_command())
        .subcommand(Command::new("stop").about(t.server_stop_about))
        .subcommand(Command::new("reload-config").about(t.server_reload_config_about))
        .subcommand(
            Command::new("agent-manifests")
                .about(t.server_agent_manifests_about)
                .arg(json_flag()),
        )
        .subcommand(
            Command::new("update-agent-manifests")
                .about(t.server_update_agent_manifests_about)
                .arg(json_flag()),
        )
        .subcommand(
            Command::new("reload-agent-manifests").about(t.server_reload_agent_manifests_about),
        );
    #[cfg(windows)]
    let command =
        command.arg(flag("allow-unelevated-clients").help(t.server_allow_unelevated_clients_help));
    command
}

fn api_command() -> Command {
    let t = &crate::i18n::texts().cli_help;
    Command::new("api")
        .about(t.api_about)
        .subcommand(Command::new("snapshot").about(t.api_snapshot_about))
        .subcommand(
            Command::new("activity-read")
                .about(t.api_activity_read_about)
                .arg(option("agent", "TARGET").help(t.api_activity_read_agent_help))
                .arg(option("external-id", "ID").help(t.api_activity_read_external_help))
                .arg(option("node-id", "ID").help(t.api_activity_read_node_help))
                .arg(option("cursor", "CURSOR").help(t.api_activity_read_cursor_help))
                .arg(option("max-bytes", "N").help(t.api_activity_read_max_bytes_help))
                .group(
                    ArgGroup::new("activity_target")
                        .args(["agent", "external-id"])
                        .required(true),
                ),
        )
        .subcommand(
            Command::new("schema")
                .about(t.api_schema_about)
                .arg(json_flag())
                .arg(path_option("output", "PATH")),
        )
}

fn workspace_command() -> Command {
    super::workspace_bindings::command()
}

fn worktree_command() -> Command {
    super::worktree::command()
}

fn tab_command() -> Command {
    super::tab::command()
}

fn notification_command() -> Command {
    super::notification::command()
}

fn agent_command() -> Command {
    super::agent::command()
}

fn pane_command() -> Command {
    super::pane::command()
}

fn terminal_command() -> Command {
    let t = &crate::i18n::texts().cli_help;
    Command::new("terminal")
        .about(t.terminal_about)
        .subcommand(
            Command::new("attach")
                .about(t.terminal_attach_about)
                .arg(required("terminal_id", "TERMINAL_ID"))
                .arg(flag("takeover")),
        )
        .subcommand(
            Command::new("session")
                .about(t.terminal_session_about)
                .subcommand(
                    Command::new("control")
                        .about(t.terminal_session_control_about)
                        .arg(required("target", "TARGET"))
                        .arg(flag("takeover"))
                        .arg(option("cols", "N"))
                        .arg(option("rows", "N")),
                )
                .subcommand(
                    Command::new("observe")
                        .about(t.terminal_session_observe_about)
                        .arg(required("target", "TARGET"))
                        .arg(option("cols", "N"))
                        .arg(option("rows", "N")),
                ),
        )
        .subcommand(super::title::command())
}

fn session_command() -> Command {
    let t = &crate::i18n::texts().cli_help;
    Command::new("session")
        .about(t.session_about)
        .subcommand(
            Command::new("list")
                .about(t.session_list_about)
                .arg(json_flag()),
        )
        .subcommand(
            Command::new("attach")
                .about(t.session_attach_about)
                .arg(required("name", "NAME")),
        )
        .subcommand(
            Command::new("stop")
                .about(t.session_stop_about)
                .arg(required("name", "NAME"))
                .arg(json_flag()),
        )
        .subcommand(
            Command::new("delete")
                .about(t.session_delete_about)
                .arg(required("name", "NAME"))
                .arg(json_flag()),
        )
}

fn integration_command() -> Command {
    let t = &crate::i18n::texts().cli_help;
    Command::new("integration")
        .about(t.integration_about)
        .subcommand(
            Command::new("install")
                .about(t.integration_install_about)
                .arg(integration_target_arg()),
        )
        .subcommand(
            Command::new("uninstall")
                .about(t.integration_uninstall_about)
                .arg(integration_target_arg()),
        )
        .subcommand(
            Command::new("status")
                .about(t.integration_status_about)
                .arg(flag("outdated-only")),
        )
}

fn plugin_command() -> Command {
    super::plugin::command()
}

fn integration_target_arg() -> Arg {
    Arg::new("target")
        .value_name("TARGET")
        .required(true)
        .value_parser(integration_target_values())
}

fn integration_target_values() -> Vec<&'static str> {
    crate::api::schema::IntegrationTarget::ALL
        .into_iter()
        .map(crate::integration::integration_target_label)
        .collect()
}

fn json_flag() -> Arg {
    flag("json")
}

fn help_flag() -> Arg {
    Arg::new("help")
        .short('h')
        .long("help")
        .action(ArgAction::SetTrue)
        .help(crate::i18n::texts().cli_help.help_flag_help)
}

fn flag(name: &'static str) -> Arg {
    Arg::new(name).long(name).action(ArgAction::SetTrue)
}

fn option(name: &'static str, value_name: &'static str) -> Arg {
    Arg::new(name)
        .long(name)
        .value_name(value_name)
        .action(ArgAction::Set)
}

fn repeatable_option(name: &'static str, value_name: &'static str) -> Arg {
    option(name, value_name).action(ArgAction::Append)
}

fn path_option(name: &'static str, value_name: &'static str) -> Arg {
    option(name, value_name).value_hint(ValueHint::AnyPath)
}

fn required(name: &'static str, value_name: &'static str) -> Arg {
    Arg::new(name).value_name(value_name).required(true)
}

#[cfg(test)]
mod tests {
    use clap::{Arg, Command};

    #[test]
    fn shared_command_help_uses_the_selected_language() {
        for lang in [crate::i18n::Lang::ZhCn, crate::i18n::Lang::En] {
            let _guard = crate::i18n::lang_guard(lang);
            let command = super::command();
            let texts = &crate::i18n::texts().cli_help;
            for (path, expected) in [
                (&["workspace"][..], texts.workspace_about),
                (&["tab"][..], texts.tab_about),
                (&["worktree"][..], texts.worktree_about),
                (&["notification"][..], texts.notification_about),
                (&["agent", "prompt"][..], texts.agent_prompt_about),
                (
                    &["pane", "report-agent-attention"][..],
                    texts.pane_report_agent_attention_about,
                ),
                (
                    &["pane", "report-agent-activity"][..],
                    texts.pane_report_agent_activity_about,
                ),
                (&["plugin", "action"][..], texts.plugin_action_about),
                (&["terminal", "title"][..], texts.terminal_title_about),
            ] {
                assert_eq!(
                    command_path(&command, path)
                        .get_about()
                        .unwrap()
                        .to_string(),
                    expected,
                    "{path:?}"
                );
            }
            let prompt = command_path(&command, &["agent", "prompt"]);
            let until = prompt
                .get_arguments()
                .find(|arg| arg.get_id() == "until")
                .unwrap();
            assert_eq!(
                until.get_help().unwrap().to_string(),
                texts.agent_prompt_until_help
            );
        }
    }

    fn command_path<'a>(cmd: &'a Command, path: &[&str]) -> &'a Command {
        let mut current = cmd;
        for name in path {
            current = current
                .get_subcommands()
                .find(|subcommand| subcommand.get_name() == *name)
                .unwrap_or_else(|| panic!("missing command path segment {name}"));
        }
        current
    }

    fn option_values(cmd: &Command, option: &str) -> Vec<String> {
        let arg = cmd
            .get_arguments()
            .find(|arg| arg.get_long() == Some(option))
            .unwrap_or_else(|| panic!("missing --{option}"));
        arg.get_value_parser()
            .possible_values()
            .into_iter()
            .flatten()
            .map(|value| value.get_name().to_string())
            .collect()
    }

    fn has_option(cmd: &Command, option: &str) -> bool {
        cmd.get_arguments()
            .any(|arg| arg.get_long() == Some(option))
    }

    fn option_arg<'a>(cmd: &'a Command, option: &str) -> &'a Arg {
        cmd.get_arguments()
            .find(|arg| arg.get_long() == Some(option))
            .unwrap_or_else(|| panic!("missing --{option}"))
    }

    fn argument<'a>(cmd: &'a Command, id: &str) -> &'a Arg {
        cmd.get_arguments()
            .find(|arg| arg.get_id() == id)
            .unwrap_or_else(|| panic!("missing argument {id}"))
    }

    fn collect_subcommand_paths(
        cmd: &Command,
        path: &mut Vec<String>,
        paths: &mut Vec<Vec<String>>,
    ) {
        for subcommand in cmd.get_subcommands() {
            path.push(subcommand.get_name().to_string());
            paths.push(path.clone());
            collect_subcommand_paths(subcommand, path, paths);
            path.pop();
        }
    }

    fn assert_command_descriptions(cmd: &Command, path: &mut Vec<String>) {
        if !path.is_empty() {
            assert!(
                cmd.get_about().is_some(),
                "missing completion description for {}",
                path.join(" ")
            );
        }
        for subcommand in cmd.get_subcommands() {
            path.push(subcommand.get_name().to_string());
            assert_command_descriptions(subcommand, path);
            path.pop();
        }
    }

    #[test]
    fn spec_describes_all_completion_commands() {
        let cmd = super::command();
        assert_command_descriptions(&cmd, &mut Vec::new());
    }

    #[test]
    fn plugin_update_help_follows_the_cli_language() {
        use crate::i18n::{has_cjk, lang_guard, texts, Lang};
        for (lang, chinese) in [(Lang::En, false), (Lang::ZhCn, true)] {
            let _guard = lang_guard(lang);
            let help = long_help(&["plugin", "update"]);
            let about = texts().cli_help.plugin_update_about;
            assert!(help.contains(about), "{lang:?}: {help}");
            assert_eq!(has_cjk(about), chinese);
            assert!(help.contains("--yes"), "{help}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn unelevated_clients_help_follows_the_cli_language() {
        use crate::i18n::{has_cjk, lang_guard, texts, Lang};
        for (lang, chinese) in [(Lang::En, false), (Lang::ZhCn, true)] {
            let _guard = lang_guard(lang);
            let command = super::server_command();
            let flag = command
                .get_arguments()
                .find(|arg| arg.get_long() == Some("allow-unelevated-clients"))
                .unwrap();
            let help = flag.get_help().unwrap().to_string();
            assert_eq!(help, texts().cli_help.server_allow_unelevated_clients_help);
            assert_eq!(has_cjk(&help), chinese);
        }
    }

    #[test]
    fn spec_passes_clap_invariants() {
        super::command().debug_assert();
    }

    #[test]
    fn every_spec_subcommand_renders_short_and_long_help() {
        let mut paths = Vec::new();
        collect_subcommand_paths(&super::command(), &mut Vec::new(), &mut paths);

        for path in paths {
            for flag in ["-h", "--help"] {
                let mut args = vec!["herdr".to_string()];
                args.extend(path.iter().cloned());
                args.push(flag.to_string());
                let mut output = Vec::new();
                assert!(
                    super::write_requested_help(&args, &mut output, || {}).unwrap(),
                    "help was not handled for herdr {} {flag}",
                    path.join(" ")
                );
                let output = String::from_utf8(output).unwrap();
                assert!(
                    output.contains(&format!("Usage: herdr {}", path.join(" "))),
                    "unexpected help for herdr {}: {output}",
                    path.join(" ")
                );
            }
        }
    }

    #[test]
    fn spec_includes_completion_alias_and_shells() {
        let cmd = super::command();
        let completion = command_path(&cmd, &["completion"]);
        assert!(completion
            .get_all_aliases()
            .any(|alias| alias == "completions"));
        let shells = completion
            .get_arguments()
            .find(|arg| arg.get_id() == "shell")
            .unwrap()
            .get_value_parser()
            .possible_values()
            .unwrap()
            .map(|value| value.get_name().to_string())
            .collect::<Vec<_>>();
        assert!(shells.contains(&"zsh".to_string()));
        assert!(shells.contains(&"fish".to_string()));
    }

    #[test]
    fn spec_matches_all_integration_targets() {
        let cmd = super::command();
        // 取值表钉死为本 fork 的五家官方集成：退役变体与已删除的 CLI-only 旁路都不得
        // 出现在 `--help` 与补全里。
        let expected = ["pi", "claude", "codex", "kimi", "opencode"];
        assert_eq!(
            crate::api::schema::IntegrationTarget::ALL
                .map(crate::integration::integration_target_label),
            expected
        );
        for action in ["install", "uninstall"] {
            let subcommand = command_path(&cmd, &["integration", action]);
            assert_eq!(
                argument(subcommand, "target")
                    .get_value_parser()
                    .possible_values()
                    .unwrap()
                    .map(|value| value.get_name().to_string())
                    .collect::<Vec<_>>(),
                expected
            );
        }
    }

    #[test]
    fn spec_marks_runtime_required_options_as_required() {
        for (path, options) in [
            (&["workspace", "report-metadata"][..], &["source"][..]),
            (&["pane", "neighbor"][..], &["direction"][..]),
            (&["pane", "focus"][..], &["direction"][..]),
            (&["pane", "resize"][..], &["direction"][..]),
            (&["pane", "report-agent"][..], &["source", "agent"][..]),
            (
                &["pane", "report-agent-session"][..],
                &["source", "agent"][..],
            ),
            (
                &["pane", "report-agent-attention"][..],
                &["source", "agent", "kind"][..],
            ),
            (
                &["pane", "report-agent-activity"][..],
                &["source", "agent"][..],
            ),
            (&["pane", "release-agent"][..], &["source", "agent"][..]),
            (&["pane", "report-metadata"][..], &["source"][..]),
        ] {
            let cmd = command_path(&super::command(), path).clone();
            for option in options {
                assert!(
                    option_arg(&cmd, option).is_required_set(),
                    "herdr {} --{option} should be required",
                    path.join(" ")
                );
            }
        }
    }

    #[test]
    fn agent_prompt_until_requires_wait() {
        let error = super::command()
            .try_get_matches_from([
                "herdr", "agent", "prompt", "reviewer", "hello", "--until", "idle",
            ])
            .unwrap_err();
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
    }

    #[test]
    fn agent_rename_requires_exactly_one_name_or_clear() {
        for valid in [
            &["herdr", "agent", "rename", "reviewer", "worker"][..],
            &["herdr", "agent", "rename", "reviewer", "--clear"][..],
        ] {
            assert!(super::command().try_get_matches_from(valid).is_ok());
        }
        for invalid in [
            &["herdr", "agent", "rename", "reviewer"][..],
            &["herdr", "agent", "rename", "reviewer", "worker", "--clear"][..],
        ] {
            assert!(super::command().try_get_matches_from(invalid).is_err());
        }

        let mut help = Vec::new();
        super::write_requested_help(
            &[
                "herdr".to_string(),
                "agent".to_string(),
                "rename".to_string(),
                "--help".to_string(),
            ],
            &mut help,
            || {},
        )
        .unwrap();
        assert!(String::from_utf8(help)
            .unwrap()
            .contains("Usage: herdr agent rename <TARGET> <NAME>|--clear"));
    }

    /// 文档终审 D13：`machine add --from-config HOST` 既不带目标也不必带 `--label`
    /// （标签缺省取 HOST），帮助却把两者都标成必填。规格与
    /// `cli/machine.rs::parse_add_args` 同口径：目标与 `--from-config` 二选一且必有
    /// 其一；`--label` 只在给了目标时必填；`--from-config` 不能与 SSH 连接选项同用。
    #[test]
    fn machine_add_spec_matches_the_from_config_form() {
        for valid in [
            &["herdr", "machine", "add", "--from-config", "devbox"][..],
            &[
                "herdr",
                "machine",
                "add",
                "--from-config",
                "devbox",
                "--label",
                "Dev",
                "--group",
                "lab",
            ][..],
            &[
                "herdr", "machine", "add", "me@host", "--label", "Dev", "--port", "2222",
            ][..],
        ] {
            assert!(
                super::command().try_get_matches_from(valid).is_ok(),
                "{valid:?}"
            );
        }
        for invalid in [
            &["herdr", "machine", "add"][..],
            &["herdr", "machine", "add", "me@host"][..],
            &[
                "herdr",
                "machine",
                "add",
                "me@host",
                "--label",
                "Dev",
                "--from-config",
                "devbox",
            ][..],
            &[
                "herdr",
                "machine",
                "add",
                "--from-config",
                "devbox",
                "--port",
                "2222",
            ][..],
        ] {
            assert!(
                super::command().try_get_matches_from(invalid).is_err(),
                "{invalid:?}"
            );
        }

        let _guard = crate::i18n::lang_guard(crate::i18n::Lang::En);
        let mut help = Vec::new();
        super::write_requested_help(
            &[
                "herdr".to_string(),
                "machine".to_string(),
                "add".to_string(),
                "--help".to_string(),
            ],
            &mut help,
            || {},
        )
        .unwrap();
        let help = String::from_utf8(help).unwrap();
        let usage = help
            .lines()
            .find(|line| line.starts_with("Usage:"))
            .expect("帮助有 Usage 行");
        assert!(
            !usage.contains("--label <LABEL>"),
            "--from-config 用法不需要 --label：{usage}"
        );
        assert!(usage.contains("--from-config <HOST>"), "{usage}");
    }

    /// T1 审查轻 3：中文里机器的 label 字段统一叫「名称」（与添加 / 编辑表单的字段
    /// 名、重名报错「名称已被使用」同一个词），不再叫「机器标签」——「标签」留给
    /// `--tag` 的组织标签。`machine add` / `machine rename` 的 `--label` 帮助与
    /// `--machine` 选择器的两条报错都照此写。
    #[test]
    fn zh_machine_label_texts_call_the_label_a_name() {
        let _guard = crate::i18n::lang_guard(crate::i18n::Lang::ZhCn);
        let name = crate::i18n::texts().machines.field_label;
        assert_eq!(name, "名称", "用例前提：表单字段名");
        for subcommand in ["add", "rename"] {
            let mut help = Vec::new();
            super::write_requested_help(
                &[
                    "herdr".to_string(),
                    "machine".to_string(),
                    subcommand.to_string(),
                    "--help".to_string(),
                ],
                &mut help,
                || {},
            )
            .unwrap();
            let help = String::from_utf8(help).unwrap();
            // 选项说明较长时 clap 把它折到下一行：取到下一个选项之前的整段。
            let option = |flag: &str| {
                let mut lines = help
                    .lines()
                    .skip_while(|line| !line.trim_start().starts_with(flag));
                let head = lines.next()?;
                let rest = lines
                    .take_while(|line| {
                        let line = line.trim_start();
                        !line.is_empty() && !line.starts_with('-')
                    })
                    .collect::<Vec<_>>();
                Some(format!("{head} {}", rest.join(" ")))
            };
            let label = option("--label")
                .unwrap_or_else(|| panic!("machine {subcommand} 帮助有 --label：{help}"));
            assert!(
                label.contains(name) && !label.contains("标签"),
                "machine {subcommand} --label：{label}"
            );
            if let Some(tag) = option("--tag") {
                assert!(tag.contains("标签"), "组织标签仍叫标签：{tag}");
            }
        }
        let errors = &crate::i18n::texts().cli_errors;
        for text in [
            errors.machine_requires_saved_label,
            errors.machine_label_ambiguous_fmt,
        ] {
            assert!(text.contains(name) && !text.contains("标签"), "{text}");
        }
    }

    /// 交接 T8 RL11：`herdr api activity-read --help` 两种界面语言都有说明，参数与 JSON
    /// API 的 `agent.activity.read` 对应，`--agent` / `--external-id` 恰给其一。
    #[test]
    fn api_activity_read_help_is_translated_and_mirrors_the_method() {
        let help = |lang| {
            let _guard = crate::i18n::lang_guard(lang);
            let mut help = Vec::new();
            super::write_requested_help(
                &[
                    "herdr".to_string(),
                    "api".to_string(),
                    "activity-read".to_string(),
                    "--help".to_string(),
                ],
                &mut help,
                || {},
            )
            .unwrap();
            String::from_utf8(help).unwrap()
        };
        let english = help(crate::i18n::Lang::En);
        let about = english.lines().next().unwrap_or_default();
        assert!(
            about.is_ascii() && about.contains("agent.activity.read"),
            "{english}"
        );
        for flag in [
            "--agent <TARGET>",
            "--external-id <ID>",
            "--node-id <ID>",
            "--cursor <CURSOR>",
            "--max-bytes <N>",
        ] {
            assert!(english.contains(flag), "缺 {flag}：{english}");
        }
        let chinese = help(crate::i18n::Lang::ZhCn);
        let about = chinese.lines().next().unwrap_or_default();
        assert!(
            about.contains("活动树") && about.contains("agent.activity.read"),
            "{chinese}"
        );
        assert!(chinese.contains("外部条目"), "{chinese}");

        let cmd = super::command();
        let read = command_path(&cmd, &["api", "activity-read"]);
        let group = read
            .get_groups()
            .find(|group| group.get_id() == "activity_target")
            .expect("目标参数组");
        assert!(group.is_required_set());
        assert!(
            !group.clone().is_multiple(),
            "--agent 与 --external-id 互斥"
        );
        assert_eq!(
            group.get_args().map(|id| id.as_str()).collect::<Vec<_>>(),
            ["agent", "external-id"]
        );
    }

    #[test]
    fn worktree_json_compatibility_flag_stays_out_of_public_help() {
        let cmd = super::command();
        for subcommand in ["list", "create", "open", "remove"] {
            let worktree_command = command_path(&cmd, &["worktree", subcommand]);
            assert!(option_arg(worktree_command, "json").is_hide_set());
            assert!(
                !worktree_command
                    .clone()
                    .render_long_help()
                    .to_string()
                    .contains("--json"),
                "herdr worktree {subcommand} should not advertise --json"
            );
        }
    }

    #[test]
    fn spec_includes_nested_plugin_pane_open_options() {
        let cmd = super::command();
        let open = command_path(&cmd, &["plugin", "pane", "open"]);
        assert!(open
            .get_arguments()
            .any(|arg| arg.get_long() == Some("entrypoint")));
        assert!(option_values(open, "placement").contains(&"zoomed".to_string()));
    }

    #[test]
    fn spec_keeps_agent_wait_status_free() {
        let cmd = super::command();
        let wait = command_path(&cmd, &["agent", "wait"]);
        assert!(!has_option(wait, "status"));
        assert_eq!(
            option_values(wait, "until"),
            ["idle", "working", "blocked", "done", "unknown"]
        );
        assert!(has_option(wait, "timeout"));
    }

    #[test]
    fn spec_matches_refactored_agent_and_pane_commands() {
        let cmd = super::command();
        assert!(cmd
            .get_subcommands()
            .all(|subcommand| subcommand.get_name() != "wait"));

        let agent = command_path(&cmd, &["agent"]);
        assert!(agent
            .get_subcommands()
            .any(|subcommand| subcommand.get_name() == "send-keys"));
        assert!(agent
            .get_subcommands()
            .any(|subcommand| subcommand.get_name() == "wait"));
        assert!(agent
            .get_subcommands()
            .all(|subcommand| subcommand.get_name() != "send"));

        let pane = command_path(&cmd, &["pane"]);
        assert!(pane
            .get_subcommands()
            .any(|subcommand| subcommand.get_name() == "wait-output"));
    }

    #[test]
    fn spec_includes_pane_read_raw_flag() {
        let cmd = super::command();
        let pane_read = command_path(&cmd, &["pane", "read"]);
        assert!(has_option(pane_read, "raw"));
    }

    #[test]
    fn spec_matches_pane_split_direction_flag() {
        let cmd = super::command();
        let pane_split = command_path(&cmd, &["pane", "split"]);
        assert!(has_option(pane_split, "direction"));
        assert!(!has_option(pane_split, "split"));
        assert_eq!(option_values(pane_split, "direction"), ["right", "down"]);
    }

    #[test]
    fn spec_models_agent_start_target_and_trailing_args() {
        let cmd = super::command();
        let agent_start = command_path(&cmd, &["agent", "start"]);
        assert!(has_option(agent_start, "kind"));
        assert_eq!(
            option_values(agent_start, "kind"),
            crate::detect::Agent::ALL
                .map(crate::detect::agent_label)
                .map(str::to_string)
        );
        assert!(has_option(agent_start, "pane"));
        for legacy in ["cwd", "workspace", "tab", "split", "focus", "env", "argv"] {
            assert!(!has_option(agent_start, legacy), "legacy option --{legacy}");
        }
        assert!(agent_start
            .get_arguments()
            .any(|arg| arg.get_id() == "agent_args"));
    }

    fn long_help(path: &[&str]) -> String {
        let mut args = vec!["herdr".to_string()];
        args.extend(path.iter().map(|segment| segment.to_string()));
        args.push("--help".to_string());
        let mut output = Vec::new();
        assert!(
            super::write_requested_help(&args, &mut output, || {}).unwrap(),
            "help was not handled for herdr {}",
            path.join(" ")
        );
        String::from_utf8(output).unwrap()
    }

    #[test]
    fn agent_resources_appear_on_command_groups_but_not_leaf_commands() {
        for group in ["agent", "pane", "workspace", "terminal"] {
            let help = long_help(&[group]);
            assert!(
                help.contains(super::super::agent_help_footer()),
                "herdr {group} is missing agent resources: {help}"
            );
        }

        let leaf = long_help(&["agent", "wait"]);
        assert!(
            !leaf.contains(super::super::agent_help_footer()),
            "leaf help should stay focused: {leaf}"
        );
    }

    #[test]
    fn next_step_hints_render_without_replacing_existing_after_help() {
        let _guard = crate::i18n::lang_guard(crate::i18n::Lang::En);
        let agent_start = long_help(&["agent", "start"]);
        assert!(
            agent_start.contains("The pane must be at its interactive shell prompt."),
            "agent start dropped its existing after_help: {agent_start}"
        );
        assert!(
            agent_start.contains("next: herdr agent prompt <TARGET> <TEXT> --wait"),
            "agent start is missing its next-step hint: {agent_start}"
        );

        let pane_send_text = long_help(&["pane", "send-text"]);
        assert!(
            pane_send_text.contains(
                "next: herdr pane run <PANE_ID> <COMMAND> sends text and Enter in one call"
            ),
            "pane send-text is missing its next-step hint: {pane_send_text}"
        );
    }

    #[test]
    fn completion_generation_succeeds_for_every_supported_shell() {
        for shell in [
            clap_complete::Shell::Bash,
            clap_complete::Shell::Elvish,
            clap_complete::Shell::Fish,
            clap_complete::Shell::PowerShell,
            clap_complete::Shell::Zsh,
        ] {
            let mut cmd = super::command();
            let mut output = Vec::new();
            clap_complete::generate(shell, &mut cmd, "herdr", &mut output);
            assert!(!output.is_empty(), "empty {shell:?} completion output");
        }
    }
}
