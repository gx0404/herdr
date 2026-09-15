use clap::{Arg, Command};

use super::{json_flag, option};

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
                .arg(option("remote-session", "NAME").help(t.machine_remote_session_help)),
        )
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
}

fn profile_command(name: &'static str, about: &'static str) -> Command {
    Command::new(name).about(about).arg(
        Arg::new("profile-id")
            .value_name("PROFILE_ID")
            .required(true),
    )
}
