//! `herdr machine exec <profile> -- <cmd...>`: a one-off remote command over
//! the profile's managed ssh channel (see `remote::exec`). ssh inherits the
//! terminal's stdio, so the command's output passes through verbatim and its
//! exit status becomes this command's.

use crate::client::endpoint::EndpointCatalog;

fn usage_text() -> &'static str {
    crate::i18n::texts().cli_errors.machine_exec_usage
}

#[derive(Debug, PartialEq, Eq)]
enum ExecArgs<'a> {
    Run {
        selector: &'a str,
        command: &'a [String],
    },
    Help,
    UsageError(&'static str),
}

/// The command is everything after an explicit `--` separator; without one,
/// option-looking words would silently become remote garbage. Tokens between
/// the profile selector and the separator are rejected.
fn parse_exec_args<'a>(args: &'a [String]) -> ExecArgs<'a> {
    let Some(selector) = args.first() else {
        return ExecArgs::UsageError(crate::i18n::texts().cli_errors.machine_exec_usage);
    };
    if matches!(selector.as_str(), "help" | "--help" | "-h") {
        return ExecArgs::Help;
    }
    let rest = &args[1..];
    let Some(separator) = rest.iter().position(|arg| arg == "--") else {
        return ExecArgs::UsageError(
            crate::i18n::texts()
                .cli_errors
                .machine_exec_command_required,
        );
    };
    if !rest[..separator].is_empty() {
        return ExecArgs::UsageError(crate::i18n::texts().cli_errors.machine_exec_usage);
    }
    let command = &rest[separator + 1..];
    if command.is_empty() {
        return ExecArgs::UsageError(
            crate::i18n::texts()
                .cli_errors
                .machine_exec_command_required,
        );
    }
    ExecArgs::Run { selector, command }
}

pub(super) fn run_machine_exec_command(args: &[String]) -> std::io::Result<i32> {
    let (selector, command) = match parse_exec_args(args) {
        ExecArgs::Run { selector, command } => (selector, command),
        ExecArgs::Help => {
            println!("{}", usage_text());
            return Ok(0);
        }
        ExecArgs::UsageError(message) => {
            eprintln!("{}{message}", crate::i18n::texts().cli_errors.error_prefix);
            if message != usage_text() {
                eprintln!("{}", usage_text());
            }
            return Ok(2);
        }
    };
    let profiles = EndpointCatalog::load_profiles().map_err(std::io::Error::other)?;
    let profile = match super::target::resolve_machine(&profiles, selector) {
        Ok(profile) => profile.clone(),
        Err(error) => {
            eprintln!("{}{error}", crate::i18n::texts().cli_errors.error_prefix);
            return Ok(2);
        }
    };
    match crate::remote::exec_saved_ssh(&profile, command) {
        Ok(code) => Ok(code),
        Err(error) => {
            eprintln!("{}{error}", crate::i18n::texts().cli_errors.error_prefix);
            Ok(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).into()).collect()
    }

    #[test]
    fn exec_parser_requires_the_separator_and_a_command() {
        for input in [
            &[][..],
            &["mac"][..],
            &["mac", "--"][..],
            &["mac", "ls"][..],
            &["mac", "-l", "--", "ls"][..],
        ] {
            let owned = args(input);
            let parsed = parse_exec_args(&owned);
            assert!(
                matches!(parsed, ExecArgs::UsageError(_)),
                "{input:?} -> {parsed:?}"
            );
        }
    }

    #[test]
    fn exec_parser_passes_the_command_through_verbatim() {
        let owned = args(&["mac", "--", "ls", "-la", "/srv app"]);
        let parsed = parse_exec_args(&owned);
        assert_eq!(
            parsed,
            ExecArgs::Run {
                selector: "mac",
                command: &["ls".into(), "-la".into(), "/srv app".into()],
            }
        );
        assert_eq!(parse_exec_args(&args(&["--help"])), ExecArgs::Help);
    }
}
