use clap::{Arg, Command};

use super::{flag, json_flag, option, path_option, repeatable_option};

pub(super) fn command() -> Command {
    let t = &crate::i18n::texts().cli_help;
    Command::new("machine")
        .about(t.machine_about)
        .subcommand(
            Command::new("list")
                .about(t.machine_list_about)
                .arg(json_flag()),
        )
        .subcommand(
            Command::new("add")
                .about(t.machine_add_about)
                .arg(
                    Arg::new("ssh-target")
                        .value_name("SSH_TARGET")
                        .required(true),
                )
                .arg(
                    option("label", "LABEL")
                        .required(true)
                        .help(t.machine_label_help),
                )
                .arg(option("remote-session", "NAME").help(t.machine_remote_session_help))
                .arg(option("group", "GROUP").help(t.machine_group_help))
                .arg(repeatable_option("tag", "TAG").help(t.machine_tag_help))
                .arg(option("color", "COLOR").help(t.machine_color_help))
                .arg(option("port", "PORT").help(t.machine_port_help))
                .arg(option("user", "USER").help(t.machine_user_help))
                .arg(
                    path_option("identity-file", "PATH")
                        .action(clap::ArgAction::Append)
                        .help(t.machine_identity_file_help),
                )
                .arg(flag("identities-only").help(t.machine_identities_only_help))
                .arg(path_option("identity-agent", "PATH").help(t.machine_identity_agent_help))
                .arg(
                    option("strict-host-key-checking", "WHEN")
                        .value_parser(["ask", "accept-new", "yes"])
                        .help(t.machine_strict_host_key_checking_help),
                )
                .arg(
                    repeatable_option("proxy-jump", "TARGET|profile:ID")
                        .help(t.machine_proxy_jump_help),
                )
                .arg(flag("forward-agent").help(t.machine_forward_agent_help))
                .arg(
                    option("server-alive-interval", "SECONDS")
                        .help(t.machine_server_alive_interval_help),
                )
                .arg(
                    option("server-alive-count-max", "COUNT")
                        .help(t.machine_server_alive_count_max_help),
                )
                .arg(option("control-persist", "VALUE").help(t.machine_control_persist_help))
                .arg(option("remote-command", "COMMAND").help(t.machine_remote_command_help))
                .arg(option("from-config", "HOST").help(t.machine_from_config_help)),
        )
        .subcommand(import_command())
        .subcommand(
            profile_command("rename", t.machine_rename_about).arg(
                option("label", "LABEL")
                    .required(true)
                    .help(t.machine_label_help),
            ),
        )
        .subcommand(profile_command("remove", t.machine_remove_about))
        .subcommand(profile_command("enable", t.machine_enable_about))
        .subcommand(profile_command("disable", t.machine_disable_about))
        .subcommand(forward_command())
        .subcommand(fs_command())
        .subcommand(log_command())
        .subcommand(status_command())
        .subcommand(exec_command())
}

fn fs_command() -> Command {
    let t = &crate::i18n::texts().cli_help;
    let fs = Command::new("fs").about(t.machine_fs_about).arg(
        Arg::new("profile")
            .value_name("PROFILE")
            .required(true)
            .help(t.machine_fs_profile_help),
    );
    // Operations are positional after the profile; they are declared as
    // trailing var args so help renders, with parsing in cli::machine_fs.
    fs.arg(
        Arg::new("operation")
            .value_name("OPERATION")
            .required(true)
            .help(t.machine_fs_operation_help),
    )
    .arg(
        Arg::new("args")
            .value_name("ARGS")
            .action(clap::ArgAction::Append)
            .help(t.machine_fs_args_help),
    )
}

fn log_command() -> Command {
    let t = &crate::i18n::texts().cli_help;
    Command::new("log")
        .about(t.machine_log_about)
        .arg(
            Arg::new("profile-id")
                .value_name("PROFILE_ID")
                .required(true),
        )
        .arg(
            Arg::new("action")
                .value_name("ACTION")
                .required(true)
                .help(t.machine_log_action_help),
        )
        .arg(
            Arg::new("args")
                .value_name("ARGS")
                .action(clap::ArgAction::Append)
                .help(t.machine_log_args_help),
        )
}

fn status_command() -> Command {
    let t = &crate::i18n::texts().cli_help;
    Command::new("status")
        .about(t.machine_status_about)
        .arg(
            Arg::new("profile")
                .value_name("PROFILE")
                .required(true)
                .help(t.machine_fs_profile_help),
        )
        .arg(json_flag())
}

fn exec_command() -> Command {
    let t = &crate::i18n::texts().cli_help;
    Command::new("exec")
        .about(t.machine_exec_about)
        .arg(
            Arg::new("profile")
                .value_name("PROFILE")
                .required(true)
                .help(t.machine_fs_profile_help),
        )
        .arg(
            Arg::new("command")
                .value_name("COMMAND")
                .action(clap::ArgAction::Append)
                .help(t.machine_exec_command_help),
        )
}

fn import_command() -> Command {
    let t = &crate::i18n::texts().cli_help;
    Command::new("import")
        .about(t.machine_import_about)
        .arg(path_option("file", "PATH").help(t.machine_import_file_help))
        .arg(repeatable_option("host", "PATTERN").help(t.machine_import_host_help))
        .arg(flag("yes").short('y').help(t.machine_import_yes_help))
        .arg(option("group", "GROUP").help(t.machine_import_group_help))
        .arg(flag("include-wildcards").help(t.machine_import_include_wildcards_help))
}

fn forward_command() -> Command {
    let t = &crate::i18n::texts().cli_help;
    Command::new("forward")
        .about(t.machine_forward_about)
        .subcommand(
            Command::new("list")
                .about(t.machine_forward_list_about)
                .arg(
                    Arg::new("profile-id")
                        .value_name("PROFILE_ID")
                        .required(true),
                )
                .arg(json_flag()),
        )
        .subcommand(
            Command::new("add")
                .about(t.machine_forward_add_about)
                .arg(
                    Arg::new("profile-id")
                        .value_name("PROFILE_ID")
                        .required(true),
                )
                .arg(
                    option("kind", "local|remote|dynamic")
                        .required(true)
                        .help(t.machine_forward_kind_help),
                )
                .arg(
                    option("listen-port", "PORT")
                        .required(true)
                        .help(t.machine_forward_listen_port_help),
                )
                .arg(option("bind-address", "ADDR").help(t.machine_forward_bind_address_help))
                .arg(option("target-host", "HOST").help(t.machine_forward_target_host_help))
                .arg(option("target-port", "PORT").help(t.machine_forward_target_port_help)),
        )
        .subcommand(
            Command::new("remove")
                .about(t.machine_forward_remove_about)
                .arg(
                    Arg::new("profile-id")
                        .value_name("PROFILE_ID")
                        .required(true),
                )
                .arg(
                    Arg::new("rule-number")
                        .value_name("RULE_NUMBER")
                        .required(true),
                ),
        )
}

fn profile_command(name: &'static str, about: &'static str) -> Command {
    Command::new(name).about(about).arg(
        Arg::new("profile-id")
            .value_name("PROFILE_ID")
            .required(true),
    )
}
