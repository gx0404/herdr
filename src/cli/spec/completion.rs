use clap::{Arg, Command};

pub(super) fn command() -> Command {
    let t = &crate::i18n::texts().cli_help;
    Command::new("completion")
        .visible_alias("completions")
        .about(t.completion_about)
        .arg(
            Arg::new("shell")
                .value_name("SHELL")
                .required(true)
                .value_parser(super::super::completion::SUPPORTED_SHELLS)
                .help(t.completion_shell_help),
        )
}
