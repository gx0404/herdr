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
    Command::new("server")
        .about(t.server_about)
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
        )
}

fn api_command() -> Command {
    let t = &crate::i18n::texts().cli_help;
    Command::new("api")
        .about(t.api_about)
        .subcommand(
            Command::new("usage-report")
                .about(t.api_usage_report_about)
                .arg(option("agent", "AGENT").help(t.api_usage_report_agent_help))
                .arg(option("account", "ID").help(t.api_usage_report_account_help))
                .arg(flag("passthrough").help(t.api_usage_report_passthrough_help)),
        )
        .subcommand(Command::new("snapshot").about(t.api_snapshot_about))
        .subcommand(
            Command::new("schema")
                .about(t.api_schema_about)
                .arg(json_flag())
                .arg(path_option("output", "PATH")),
        )
}

fn workspace_command() -> Command {
    let t = &crate::i18n::texts().cli_help;
    Command::new("workspace")
        .about(t.workspace_about)
        .subcommand(Command::new("list").about(t.workspace_list_about))
        .subcommand(
            Command::new("create")
                .about(t.workspace_create_about)
                .arg(path_option("cwd", "PATH"))
                .arg(option("label", "TEXT"))
                .arg(env_option())
                .arg(flag("focus"))
                .arg(flag("no-focus")),
        )
        .subcommand(id_command("get", "workspace_id", t.workspace_get_about))
        .subcommand(id_command("focus", "workspace_id", t.workspace_focus_about))
        .subcommand(
            Command::new("rename")
                .about(t.workspace_rename_about)
                .arg(required("workspace_id", "WORKSPACE_ID"))
                .arg(required("label", "LABEL").num_args(1..)),
        )
        .subcommand(
            Command::new("report-metadata")
                .about(t.workspace_report_metadata_about)
                .arg(required("workspace_id", "WORKSPACE_ID"))
                .arg(option("source", "ID").required(true))
                .arg(repeatable_option("token", "NAME=VALUE"))
                .arg(repeatable_option("clear-token", "NAME"))
                .arg(option("seq", "N"))
                .arg(option("ttl-ms", "N")),
        )
        .subcommand(id_command("close", "workspace_id", t.workspace_close_about))
}

fn worktree_command() -> Command {
    let t = &crate::i18n::texts().cli_help;
    Command::new("worktree")
        .about(t.worktree_about)
        .subcommand(
            Command::new("list")
                .about(t.worktree_list_about)
                .arg(option("workspace", "ID"))
                .arg(path_option("cwd", "PATH"))
                .arg(flag("trust-repository")),
        )
        .subcommand(
            Command::new("create")
                .about(t.worktree_create_about)
                .arg(option("workspace", "ID"))
                .arg(path_option("cwd", "PATH"))
                .arg(option("branch", "NAME"))
                .arg(option("base", "REF"))
                .arg(path_option("path", "PATH"))
                .arg(option("label", "TEXT"))
                .arg(flag("focus"))
                .arg(flag("no-focus"))
                .arg(flag("trust-repository")),
        )
        .subcommand(
            Command::new("open")
                .about(t.worktree_open_about)
                .arg(option("workspace", "ID"))
                .arg(path_option("cwd", "PATH"))
                .arg(path_option("path", "PATH"))
                .arg(option("branch", "NAME"))
                .arg(option("label", "TEXT"))
                .arg(flag("focus"))
                .arg(flag("no-focus"))
                .arg(flag("trust-repository")),
        )
        .subcommand(
            Command::new("remove")
                .about(t.worktree_remove_about)
                .arg(option("workspace", "ID"))
                .arg(flag("force"))
                .arg(flag("trust-repository")),
        )
}

fn tab_command() -> Command {
    let t = &crate::i18n::texts().cli_help;
    Command::new("tab")
        .about(t.tab_about)
        .subcommand(
            Command::new("list")
                .about(t.tab_list_about)
                .arg(option("workspace", "WORKSPACE_ID")),
        )
        .subcommand(
            Command::new("create")
                .about(t.tab_create_about)
                .arg(option("workspace", "WORKSPACE_ID"))
                .arg(path_option("cwd", "PATH"))
                .arg(option("label", "TEXT"))
                .arg(env_option())
                .arg(flag("focus"))
                .arg(flag("no-focus")),
        )
        .subcommand(id_command("get", "tab_id", t.tab_get_about))
        .subcommand(id_command("focus", "tab_id", t.tab_focus_about))
        .subcommand(
            Command::new("rename")
                .about(t.tab_rename_about)
                .arg(required("tab_id", "TAB_ID"))
                .arg(required("label", "LABEL").num_args(1..)),
        )
        .subcommand(id_command("close", "tab_id", t.tab_close_about))
}

fn notification_command() -> Command {
    let t = &crate::i18n::texts().cli_help;
    Command::new("notification")
        .about(t.notification_about)
        .subcommand(
            Command::new("show")
                .about(t.notification_show_about)
                .arg(required("title", "TITLE"))
                .arg(option("body", "TEXT"))
                .arg(option("position", "POSITION").value_parser([
                    "top-left",
                    "top-right",
                    "bottom-left",
                    "bottom-right",
                ]))
                .arg(option("sound", "SOUND").value_parser(["none", "done", "request"])),
        )
}

fn agent_command() -> Command {
    let t = &crate::i18n::texts().cli_help;
    Command::new("agent")
        .about(t.agent_about)
        .subcommand(Command::new("list").about(t.agent_list_about))
        .subcommand(id_command("get", "target", t.agent_get_about))
        .subcommand(
            Command::new("read")
                .about(t.agent_read_about)
                .override_usage("herdr agent read <TARGET> [OPTIONS]")
                .arg(required("target", "TARGET"))
                .arg(read_source_option(true))
                .arg(option("lines", "N"))
                .arg(text_ansi_format_option())
                .arg(flag("ansi")),
        )
        .subcommand(
            Command::new("send-keys")
                .about(t.agent_send_keys_about)
                .arg(required("target", "TARGET"))
                .arg(required("key", "KEY").num_args(1..))
                .after_help(t.send_keys_after_help),
        )
        .subcommand(
            Command::new("prompt")
                .about(t.agent_prompt_about)
                .override_usage("herdr agent prompt <TARGET> <TEXT> [OPTIONS]")
                .arg(required("target", "TARGET"))
                .arg(required("text", "TEXT"))
                .arg(flag("wait").help(t.agent_prompt_wait_help))
                .arg(
                    option("until", "STATUS")
                        .action(ArgAction::Append)
                        .requires("wait")
                        .value_parser(["idle", "working", "blocked", "done", "unknown"])
                        .help(t.agent_prompt_until_help),
                )
                .arg(
                    option("timeout", "MS")
                        .requires("wait")
                        .help(t.timeout_ms_help),
                )
                .after_help(t.agent_prompt_after_help),
        )
        .subcommand(
            Command::new("rename")
                .about(t.agent_rename_about)
                .override_usage("herdr agent rename <TARGET> <NAME>|--clear")
                .arg(required("target", "TARGET"))
                .arg(Arg::new("name").value_name("NAME"))
                .arg(flag("clear"))
                .group(
                    ArgGroup::new("rename")
                        .args(["name", "clear"])
                        .required(true),
                ),
        )
        .subcommand(id_command("focus", "target", t.agent_focus_about))
        .subcommand(
            Command::new("wait")
                .about(t.agent_wait_about)
                .override_usage("herdr agent wait <TARGET> [OPTIONS]")
                .arg(required("target", "TARGET"))
                .arg(
                    option("until", "STATUS")
                        .action(ArgAction::Append)
                        .value_parser(["idle", "working", "blocked", "done", "unknown"])
                        .help(t.agent_wait_until_help),
                )
                .arg(option("timeout", "MS").help(t.timeout_ms_help))
                .after_help(t.agent_wait_after_help),
        )
        .subcommand(
            Command::new("attach")
                .about(t.agent_attach_about)
                .override_usage("herdr agent attach <TARGET> [OPTIONS]")
                .arg(required("target", "TARGET"))
                .arg(flag("takeover")),
        )
        .subcommand(
            Command::new("start")
                .about(t.agent_start_about)
                .override_usage(
                    "herdr agent start <NAME> --kind <KIND> --pane <ID> [OPTIONS] [-- [AGENT_ARG]...]",
                )
                .arg(required("name", "NAME"))
                .arg(
                    option("kind", "KIND")
                        .required(true)
                        .value_parser(agent_kind_values())
                        .help(t.agent_start_kind_help),
                )
                .arg(
                    option("pane", "ID")
                        .required(true)
                        .help(t.agent_start_pane_help),
                )
                .arg(option("timeout", "MS").help(t.agent_start_timeout_help))
                .arg(
                    Arg::new("agent_args")
                        .value_name("AGENT_ARG")
                        .num_args(0..)
                        .last(true),
                )
                .after_help(t.agent_start_after_help),
        )
        .subcommand(
            Command::new("explain")
                .about(t.agent_explain_about)
                .arg(Arg::new("target").value_name("TARGET"))
                .arg(path_option("file", "PATH"))
                .arg(option("agent", "LABEL"))
                .arg(json_flag())
                .arg(text_json_format_option())
                .arg(
                    Arg::new("verbose")
                        .short('v')
                        .long("verbose")
                        .action(ArgAction::SetTrue),
                ),
        )
}

pub(super) fn agent_kind_values() -> Vec<&'static str> {
    crate::detect::Agent::ALL
        .into_iter()
        .map(crate::detect::agent_label)
        .collect()
}

fn pane_command() -> Command {
    let t = &crate::i18n::texts().cli_help;
    Command::new("pane")
        .about(t.pane_about)
        .subcommand(
            Command::new("list")
                .about(t.pane_list_about)
                .arg(option("workspace", "WORKSPACE_ID")),
        )
        .subcommand(
            Command::new("current")
                .about(t.pane_current_about)
                .args(current_pane_args()),
        )
        .subcommand(id_command("get", "pane_id", t.pane_get_about))
        .subcommand(
            Command::new("layout")
                .about(t.pane_layout_about)
                .args(current_pane_args()),
        )
        .subcommand(
            Command::new("process-info")
                .about(t.pane_process_info_about)
                .args(current_pane_args()),
        )
        .subcommand(
            Command::new("neighbor")
                .about(t.pane_neighbor_about)
                .arg(required_direction_option())
                .args(current_pane_args()),
        )
        .subcommand(
            Command::new("edges")
                .about(t.pane_edges_about)
                .args(current_pane_args()),
        )
        .subcommand(
            Command::new("focus")
                .about(t.pane_focus_about)
                .arg(required_direction_option())
                .args(current_pane_args()),
        )
        .subcommand(
            Command::new("resize")
                .about(t.pane_resize_about)
                .arg(required_direction_option())
                .arg(option("amount", "FLOAT"))
                .args(current_pane_args()),
        )
        .subcommand(
            Command::new("zoom")
                .about(t.pane_zoom_about)
                .arg(Arg::new("pane_id").value_name("PANE_ID"))
                .args(current_pane_args())
                .arg(flag("toggle"))
                .arg(flag("on"))
                .arg(flag("off")),
        )
        .subcommand(
            Command::new("read")
                .about(t.pane_read_about)
                .arg(required("pane_id", "PANE_ID"))
                .arg(read_source_option(true))
                .arg(option("lines", "N"))
                .arg(text_ansi_format_option())
                .arg(flag("ansi"))
                .arg(flag("raw")),
        )
        .subcommand(
            Command::new("rename")
                .about(t.pane_rename_about)
                .arg(required("pane_id", "PANE_ID"))
                .arg(Arg::new("label").value_name("LABEL").num_args(1..))
                .arg(flag("clear")),
        )
        .subcommand(
            Command::new("input")
                .about(t.pane_input_about)
                .arg(Arg::new("pane_id").value_name("PANE_ID"))
                .args(current_pane_args())
                .arg(
                    option("right-click", "TARGET")
                        .value_parser(["herdr", "pane"])
                        .required(true),
                ),
        )
        .subcommand(
            Command::new("split")
                .about(t.pane_split_about)
                .arg(Arg::new("pane_id").value_name("PANE_ID"))
                .args(current_pane_args())
                .arg(split_direction_option())
                .arg(option("ratio", "FLOAT"))
                .arg(path_option("cwd", "PATH"))
                .arg(env_option())
                .arg(option("right-click", "TARGET").value_parser(["herdr", "pane"]))
                .arg(flag("focus"))
                .arg(flag("no-focus")),
        )
        .subcommand(
            Command::new("swap")
                .about(t.pane_swap_about)
                .arg(direction_option())
                .args(current_pane_args())
                .arg(option("source-pane", "ID"))
                .arg(option("target-pane", "ID")),
        )
        .subcommand(
            Command::new("move")
                .about(t.pane_move_about)
                .arg(required("pane_id", "PANE_ID"))
                .arg(option("tab", "TAB_ID"))
                .arg(option("split", "DIRECTION").value_parser(["right", "down"]))
                .arg(option("target-pane", "ID"))
                .arg(option("ratio", "FLOAT"))
                .arg(flag("new-tab"))
                .arg(option("workspace", "ID"))
                .arg(flag("new-workspace"))
                .arg(option("label", "TEXT"))
                .arg(option("tab-label", "TEXT"))
                .arg(flag("focus"))
                .arg(flag("no-focus")),
        )
        .subcommand(id_command("close", "pane_id", t.pane_close_about))
        .subcommand(
            Command::new("send-text")
                .about(t.pane_send_text_about)
                .arg(required("pane_id", "PANE_ID"))
                .arg(required("text", "TEXT"))
                .after_help(t.pane_send_text_after_help),
        )
        .subcommand(
            Command::new("send-keys")
                .about(t.pane_send_keys_about)
                .arg(required("pane_id", "PANE_ID"))
                .arg(required("key", "KEY").num_args(1..))
                .after_help(t.send_keys_after_help),
        )
        .subcommand(
            Command::new("wait-output")
                .about(t.pane_wait_output_about)
                .arg(required("pane_id", "PANE_ID"))
                .arg(
                    option("match", "TEXT")
                        .conflicts_with("regex")
                        .required_unless_present("regex")
                        .help(t.pane_wait_output_match_help),
                )
                .arg(
                    option("regex", "PATTERN")
                        .conflicts_with("match")
                        .required_unless_present("match")
                        .help(t.pane_wait_output_regex_help),
                )
                .arg(read_source_option(false))
                .arg(option("lines", "N").help(t.pane_wait_output_lines_help))
                .arg(option("timeout", "MS").help(t.timeout_ms_help))
                .arg(flag("raw").help(t.pane_wait_output_raw_help))
                .group(
                    ArgGroup::new("matcher")
                        .args(["match", "regex"])
                        .required(true),
                )
                .after_help(t.pane_wait_output_after_help),
        )
        .subcommand(
            Command::new("run")
                .about(t.pane_run_about)
                .arg(required("pane_id", "PANE_ID"))
                .arg(required("command", "COMMAND").num_args(1..)),
        )
        .subcommand(report_agent_command())
        .subcommand(report_agent_session_command())
        .subcommand(release_agent_command())
        .subcommand(report_metadata_command())
}

fn report_agent_command() -> Command {
    let t = &crate::i18n::texts().cli_help;
    Command::new("report-agent")
        .about(t.pane_report_agent_about)
        .arg(required("pane_id", "PANE_ID"))
        .arg(option("source", "ID").required(true))
        .arg(option("agent", "LABEL").required(true))
        .arg(pane_agent_state_option("state"))
        .arg(option("message", "TEXT"))
        .arg(option("seq", "N"))
        .arg(option("agent-session-id", "ID"))
        .arg(path_option("agent-session-path", "PATH"))
}

fn report_agent_session_command() -> Command {
    let t = &crate::i18n::texts().cli_help;
    Command::new("report-agent-session")
        .about(t.pane_report_agent_session_about)
        .arg(required("pane_id", "PANE_ID"))
        .arg(option("source", "ID").required(true))
        .arg(option("agent", "LABEL").required(true))
        .arg(option("seq", "N"))
        .arg(option("agent-session-id", "ID"))
        .arg(path_option("agent-session-path", "PATH"))
        .arg(option("session-start-source", "SOURCE"))
}

fn release_agent_command() -> Command {
    let t = &crate::i18n::texts().cli_help;
    Command::new("release-agent")
        .about(t.pane_release_agent_about)
        .arg(required("pane_id", "PANE_ID"))
        .arg(option("source", "ID").required(true))
        .arg(option("agent", "LABEL").required(true))
        .arg(option("seq", "N"))
}

fn report_metadata_command() -> Command {
    let t = &crate::i18n::texts().cli_help;
    Command::new("report-metadata")
        .about(t.pane_report_metadata_about)
        .arg(required("pane_id", "PANE_ID"))
        .arg(option("source", "ID").required(true))
        .arg(option("agent", "LABEL"))
        .arg(option("applies-to-source", "ID"))
        .arg(option("title", "TEXT"))
        .arg(flag("clear-title"))
        .arg(option("display-agent", "TEXT"))
        .arg(flag("clear-display-agent"))
        .arg(option("state-label", "STATUS=TEXT"))
        .arg(flag("clear-state-labels"))
        .arg(repeatable_option("token", "NAME=VALUE"))
        .arg(repeatable_option("clear-token", "NAME"))
        .arg(option("seq", "N"))
        .arg(option("ttl-ms", "N"))
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
        .subcommand(
            Command::new("title")
                .about(t.terminal_title_about)
                .subcommand(
                    Command::new("set")
                        .about(t.terminal_title_set_about)
                        .arg(required("title", "TITLE")),
                )
                .subcommand(Command::new("clear").about(t.terminal_title_clear_about)),
        )
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
    let t = &crate::i18n::texts().cli_help;
    Command::new("plugin")
        .about(t.plugin_about)
        .subcommand(
            Command::new("install")
                .about(t.plugin_install_about)
                .arg(required("source", "OWNER/REPO[/SUBDIR]"))
                .arg(option("ref", "REF"))
                .arg(
                    Arg::new("yes")
                        .short('y')
                        .long("yes")
                        .action(ArgAction::SetTrue),
                ),
        )
        .subcommand(
            Command::new("uninstall")
                .about(t.plugin_uninstall_about)
                .arg(required("plugin", "PLUGIN")),
        )
        .subcommand(
            Command::new("link")
                .about(t.plugin_link_about)
                .arg(path_arg("path", "PATH"))
                .arg(flag("disabled"))
                .arg(flag("enabled")),
        )
        .subcommand(
            Command::new("unlink")
                .about(t.plugin_unlink_about)
                .arg(required("plugin_id", "PLUGIN_ID")),
        )
        .subcommand(
            Command::new("enable")
                .about(t.plugin_enable_about)
                .arg(required("plugin_id", "PLUGIN_ID")),
        )
        .subcommand(
            Command::new("disable")
                .about(t.plugin_disable_about)
                .arg(required("plugin_id", "PLUGIN_ID")),
        )
        .subcommand(
            Command::new("list")
                .about(t.plugin_list_about)
                .arg(option("plugin", "ID"))
                .arg(json_flag()),
        )
        .subcommand(
            Command::new("config-dir")
                .about(t.plugin_config_dir_about)
                .arg(required("plugin_id", "PLUGIN_ID")),
        )
        .subcommand(
            Command::new("action")
                .about(t.plugin_action_about)
                .subcommand(
                    Command::new("list")
                        .about(t.plugin_action_list_about)
                        .arg(option("plugin", "ID")),
                )
                .subcommand(
                    Command::new("invoke")
                        .about(t.plugin_action_invoke_about)
                        .arg(required("action_id", "ACTION_ID"))
                        .arg(option("plugin", "ID")),
                ),
        )
        .subcommand(
            Command::new("log")
                .about(t.plugin_log_about)
                .visible_alias("logs")
                .subcommand(
                    Command::new("list")
                        .about(t.plugin_log_list_about)
                        .arg(option("plugin", "ID"))
                        .arg(option("limit", "N")),
                ),
        )
        .subcommand(
            Command::new("pane")
                .about(t.plugin_pane_about)
                .subcommand(
                    Command::new("open")
                        .about(t.plugin_pane_open_about)
                        .arg(option("plugin", "ID"))
                        .arg(option("entrypoint", "ID"))
                        .arg(
                            option("placement", "PLACEMENT")
                                .value_parser(["overlay", "split", "tab", "zoomed"]),
                        )
                        .arg(option("workspace", "ID"))
                        .arg(option("target-pane", "PANE"))
                        .arg(split_direction_option())
                        .arg(path_option("cwd", "PATH"))
                        .arg(env_option())
                        .arg(flag("focus"))
                        .arg(flag("no-focus")),
                )
                .subcommand(
                    Command::new("focus")
                        .about(t.plugin_pane_focus_about)
                        .arg(required("pane_id", "PANE_ID")),
                )
                .subcommand(
                    Command::new("close")
                        .about(t.plugin_pane_close_about)
                        .arg(required("pane_id", "PANE_ID")),
                ),
        )
}

fn current_pane_args() -> [Arg; 2] {
    [option("pane", "ID"), flag("current")]
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

fn id_command(name: &'static str, id: &'static str, about: &'static str) -> Command {
    Command::new(name).about(about).arg(required(id, id))
}

fn direction_option() -> Arg {
    option("direction", "DIRECTION").value_parser(["left", "right", "up", "down"])
}

fn required_direction_option() -> Arg {
    direction_option().required(true)
}

fn split_direction_option() -> Arg {
    option("direction", "DIRECTION").value_parser(["right", "down"])
}

fn pane_agent_state_option(name: &'static str) -> Arg {
    option(name, "STATUS")
        .required(true)
        .value_parser(["idle", "working", "blocked", "unknown"])
}

fn read_source_option(include_detection: bool) -> Arg {
    let values = if include_detection {
        vec!["visible", "recent", "recent-unwrapped", "detection"]
    } else {
        vec!["visible", "recent", "recent-unwrapped"]
    };
    option("source", "SOURCE")
        .value_parser(values)
        .help(crate::i18n::texts().cli_help.source_help)
}

fn text_ansi_format_option() -> Arg {
    option("format", "FORMAT").value_parser(["text", "ansi"])
}

fn text_json_format_option() -> Arg {
    option("format", "FORMAT").value_parser(["text", "json"])
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

fn env_option() -> Arg {
    option("env", "KEY=VALUE")
        .action(ArgAction::Append)
        .help(crate::i18n::texts().cli_help.env_help)
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

fn path_arg(name: &'static str, value_name: &'static str) -> Arg {
    required(name, value_name).value_hint(ValueHint::AnyPath)
}

#[cfg(test)]
mod tests {
    use clap::{Arg, Command};

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

    /// 文档终审 D7：`herdr api usage-report --help` 的说明曾是写死的中文。
    #[test]
    fn api_usage_report_help_follows_the_cli_language() {
        use crate::i18n::{has_cjk, lang_guard, texts, Lang};
        for (lang, chinese) in [(Lang::En, false), (Lang::ZhCn, true)] {
            let _guard = lang_guard(lang);
            let mut help = Vec::new();
            super::write_requested_help(
                &[
                    "herdr".to_string(),
                    "api".to_string(),
                    "usage-report".to_string(),
                    "--help".to_string(),
                ],
                &mut help,
                || {},
            )
            .unwrap();
            let help = String::from_utf8(help).unwrap();
            let about = help.lines().next().unwrap_or_default();
            assert!(!about.is_empty(), "{help}");
            assert_eq!(has_cjk(about), chinese, "{lang:?}: {help}");
            if !chinese {
                assert!(help.is_ascii(), "{help}");
            }
            // T1 服务端审查轻 4：三个选项都带说明，按界面语言写。
            let t = &texts().cli_help;
            for (option, text) in [
                ("--agent", t.api_usage_report_agent_help),
                ("--account", t.api_usage_report_account_help),
                ("--passthrough", t.api_usage_report_passthrough_help),
            ] {
                assert!(
                    help.lines()
                        .any(|line| line.trim_start().starts_with(option)),
                    "{option} 不在帮助里：{help}"
                );
                let head: String = text.chars().take(8).collect();
                assert!(help.contains(&head), "{lang:?} {option}: {help}");
                assert_eq!(has_cjk(text), chinese, "{lang:?}: {text}");
            }
        }
    }

    #[test]
    fn worktree_json_compatibility_flag_stays_out_of_public_spec() {
        let cmd = super::command();
        for subcommand in ["list", "create", "open", "remove"] {
            let worktree_command = command_path(&cmd, &["worktree", subcommand]);
            assert!(
                !has_option(worktree_command, "json"),
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
