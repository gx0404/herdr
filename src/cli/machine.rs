use serde::Serialize;

use crate::client::endpoint::{EndpointCatalog, ProfileId};

#[derive(Serialize)]
struct MachineListRow<'a> {
    id: &'a str,
    label: &'a str,
    target: &'a str,
    session: &'a str,
    enabled: bool,
    selected: bool,
}

pub(super) fn run_machine_command(args: &[String]) -> std::io::Result<i32> {
    match args.first().map(String::as_str) {
        Some("list") => list(&args[1..]),
        Some("add") => add(&args[1..]),
        Some("rename") => rename(&args[1..]),
        Some("remove") => remove(&args[1..]),
        Some("enable") => set_enabled(&args[1..], true),
        Some("disable") => set_enabled(&args[1..], false),
        Some("help" | "--help" | "-h") => {
            println!("{}", crate::i18n::texts().cli_output.machine_help);
            Ok(0)
        }
        _ => {
            eprintln!("{}", crate::i18n::texts().cli_output.machine_help);
            Ok(2)
        }
    }
}

fn list(args: &[String]) -> std::io::Result<i32> {
    let json = match args {
        [] => false,
        [flag] if flag == "--json" => true,
        _ => {
            eprintln!("{}", crate::i18n::texts().cli_errors.machine_list_usage);
            return Ok(2);
        }
    };
    let catalog = load_catalog()?;
    let rows = catalog
        .ssh
        .iter()
        .map(|profile| MachineListRow {
            id: profile.id.as_str(),
            label: &profile.label,
            target: &profile.target,
            session: &profile.session,
            enabled: profile.enabled,
            selected: catalog.selected_profile.as_ref() == Some(&profile.id),
        })
        .collect::<Vec<_>>();
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&rows).map_err(std::io::Error::other)?
        );
        return Ok(0);
    }
    let t = &crate::i18n::texts().cli_output;
    if rows.is_empty() {
        println!("{}", t.machine_none_saved);
        return Ok(0);
    }
    for row in rows {
        let state = if row.enabled {
            t.state_enabled
        } else {
            t.state_disabled
        };
        println!(
            "{}\t{}\t{}\t{}\t{}",
            row.id, row.label, row.target, row.session, state
        );
    }
    Ok(0)
}

#[derive(Debug, PartialEq, Eq)]
struct AddArgs {
    target: String,
    label: String,
    session: String,
}

fn parse_add_args(args: &[String]) -> Result<AddArgs, String> {
    let t = &crate::i18n::texts().cli_errors;
    let args = super::expand_equals_args(args, &["--label", "--remote-session"]);
    let mut target = None;
    let mut label = None;
    let mut session = None;
    let mut index = 0;
    while index < args.len() {
        let (name, value) = match args[index].as_str() {
            "--label" | "--remote-session" => {
                let Some(value) = args.get(index + 1) else {
                    return Err(crate::i18n::fill(
                        t.missing_value_for_fmt,
                        &[("flag", args[index].as_str())],
                    ));
                };
                index += 2;
                (args[index - 2].as_str(), value.clone())
            }
            positional if !positional.starts_with('-') && target.is_none() => {
                target = Some(positional.to_owned());
                index += 1;
                continue;
            }
            unknown => {
                return Err(crate::i18n::fill(
                    t.machine_add_unknown_option_fmt,
                    &[("option", unknown)],
                ));
            }
        };
        match name {
            "--label" if label.is_none() => label = Some(value),
            "--remote-session" if session.is_none() => session = Some(value),
            "--remote-session" => {
                return Err(t.remote_session_specified_twice.into());
            }
            "--label" => {
                return Err(t.label_specified_twice.into());
            }
            _ => unreachable!("validated machine add option"),
        }
    }
    let target = target.ok_or_else(|| t.machine_add_usage.to_owned())?;
    let label = label.ok_or_else(|| t.label_required.to_owned())?;
    let session = session.unwrap_or_else(|| crate::session::DEFAULT_SESSION_NAME.to_owned());
    Ok(AddArgs {
        target,
        label,
        session,
    })
}

fn add(args: &[String]) -> std::io::Result<i32> {
    let t = &crate::i18n::texts().cli_errors;
    let AddArgs {
        target,
        label,
        session,
    } = match parse_add_args(args) {
        Ok(args) => args,
        Err(error) => {
            eprintln!("{error}");
            return Ok(2);
        }
    };
    let mut catalog = load_catalog()?;
    match catalog.add_ssh(label.clone(), &target, session.clone()) {
        Ok(_) => {}
        Err(error) => {
            eprintln!("{}{error}", t.error_prefix);
            return Ok(2);
        }
    }
    if let Err(error) = crate::remote::prepare_saved_ssh(&target, &session) {
        eprintln!(
            "{}",
            crate::i18n::fill(t.machine_not_saved_fmt, &[("error", &error.to_string())])
        );
        crate::remote::print_saved_ssh_error_hint(&error, &target);
        return Ok(1);
    }
    // Setup can wait for human approval. Do not overwrite catalog edits made meanwhile.
    let mut catalog = load_catalog().map_err(|error| {
        std::io::Error::other(crate::i18n::fill(
            t.machine_prepared_not_saved_fmt,
            &[("error", &error.to_string())],
        ))
    })?;
    let id = match catalog.add_ssh(label, target, session) {
        Ok(id) => id,
        Err(error) => {
            eprintln!("{}{error}", t.error_prefix);
            return Ok(2);
        }
    };
    store_catalog(&catalog).map_err(|error| {
        std::io::Error::other(crate::i18n::fill(
            t.machine_prepared_not_saved_fmt,
            &[("error", &error.to_string())],
        ))
    })?;
    let t = &crate::i18n::texts().cli_output;
    println!(
        "{}",
        crate::i18n::fill(t.machine_saved_fmt, &[("id", id.as_str())])
    );
    println!("{}", t.machine_clients_connect);
    Ok(0)
}

fn rename(args: &[String]) -> std::io::Result<i32> {
    let t = &crate::i18n::texts().cli_errors;
    let args = super::expand_equals_args(args, &["--label"]);
    let [raw_id, flag, label] = args.as_slice() else {
        eprintln!("{}", t.machine_rename_usage);
        return Ok(2);
    };
    if flag != "--label" {
        eprintln!("{}", t.machine_rename_usage);
        return Ok(2);
    }
    let id = match ProfileId::parse(raw_id.clone()) {
        Ok(id) => id,
        Err(error) => {
            eprintln!("{}{error}", t.error_prefix);
            return Ok(2);
        }
    };
    let mut catalog = load_catalog()?;
    match catalog.rename_ssh(&id, label) {
        Ok(true) => {}
        Ok(false) => {
            eprintln!(
                "{}",
                crate::i18n::fill(t.machine_profile_not_found_fmt, &[("id", id.as_str())])
            );
            return Ok(1);
        }
        Err(error) => {
            eprintln!("{}{error}", t.error_prefix);
            return Ok(2);
        }
    }
    store_catalog(&catalog)?;
    println!(
        "{}",
        crate::i18n::fill(
            crate::i18n::texts().cli_output.machine_renamed_fmt,
            &[("id", id.as_str())]
        )
    );
    Ok(0)
}

fn remove(args: &[String]) -> std::io::Result<i32> {
    let Some(id) = one_profile_id(args, crate::i18n::texts().cli_errors.machine_remove_usage)?
    else {
        return Ok(2);
    };
    let mut catalog = load_catalog()?;
    let previous_selection = catalog.selected_profile.clone();
    if !catalog.remove_ssh(&id) {
        eprintln!(
            "{}",
            crate::i18n::fill(
                crate::i18n::texts()
                    .cli_errors
                    .machine_profile_not_found_fmt,
                &[("id", id.as_str())]
            )
        );
        return Ok(1);
    }
    store_catalog(&catalog)?;
    if catalog.selected_profile != previous_selection {
        catalog.store_selection().map_err(std::io::Error::other)?;
    }
    println!(
        "{}",
        crate::i18n::fill(
            crate::i18n::texts().cli_output.machine_removed_fmt,
            &[("id", id.as_str())]
        )
    );
    Ok(0)
}

fn set_enabled(args: &[String], enabled: bool) -> std::io::Result<i32> {
    let action = if enabled { "enable" } else { "disable" };
    let usage = crate::i18n::fill(
        crate::i18n::texts()
            .cli_errors
            .machine_set_enabled_usage_fmt,
        &[("action", action)],
    );
    let Some(id) = one_profile_id(args, &usage)? else {
        return Ok(2);
    };
    let mut catalog = load_catalog()?;
    let previous_selection = catalog.selected_profile.clone();
    if !catalog.set_enabled(&id, enabled) {
        eprintln!(
            "{}",
            crate::i18n::fill(
                crate::i18n::texts()
                    .cli_errors
                    .machine_profile_not_found_fmt,
                &[("id", id.as_str())]
            )
        );
        return Ok(1);
    }
    store_catalog(&catalog)?;
    if catalog.selected_profile != previous_selection {
        catalog.store_selection().map_err(std::io::Error::other)?;
    }
    let t = &crate::i18n::texts().cli_output;
    println!(
        "{}",
        crate::i18n::fill(
            if enabled {
                t.machine_enabled_fmt
            } else {
                t.machine_disabled_fmt
            },
            &[("id", id.as_str())]
        )
    );
    Ok(0)
}

fn one_profile_id(args: &[String], usage: &str) -> std::io::Result<Option<ProfileId>> {
    let [raw] = args else {
        eprintln!("{usage}");
        return Ok(None);
    };
    match ProfileId::parse(raw.clone()) {
        Ok(id) => Ok(Some(id)),
        Err(error) => {
            eprintln!("{}{error}", crate::i18n::texts().cli_errors.error_prefix);
            Ok(None)
        }
    }
}

fn load_catalog() -> std::io::Result<EndpointCatalog> {
    EndpointCatalog::load().map_err(std::io::Error::other)
}

fn store_catalog(catalog: &EndpointCatalog) -> std::io::Result<()> {
    catalog.store_profiles().map_err(std::io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_parser_preserves_values_across_argument_orders() {
        for (args, session) in [
            (vec!["--label", "coder", "workstation.coder"], "default"),
            (vec!["workstation.coder", "--label", "coder"], "default"),
            (
                vec![
                    "--remote-session",
                    "agents",
                    "workstation.coder",
                    "--label",
                    "coder",
                ],
                "agents",
            ),
            (
                vec![
                    "--label=coder",
                    "--remote-session=agents",
                    "workstation.coder",
                ],
                "agents",
            ),
        ] {
            let args = args.into_iter().map(str::to_owned).collect::<Vec<_>>();
            assert_eq!(
                parse_add_args(&args).unwrap(),
                AddArgs {
                    target: "workstation.coder".into(),
                    label: "coder".into(),
                    session: session.into(),
                },
                "{args:?}"
            );
        }
    }

    #[test]
    fn add_parser_rejects_incomplete_duplicate_and_extra_arguments() {
        for args in [
            vec![],
            vec!["--label", "coder"],
            vec!["workstation.coder"],
            vec!["workstation.coder", "--label"],
            vec!["workstation.coder", "--label", "coder", "--remote-session"],
            vec!["--label", "coder", "--label", "other", "workstation.coder"],
            vec![
                "workstation.coder",
                "--label",
                "coder",
                "--remote-session",
                "a",
                "--remote-session",
                "b",
            ],
            vec!["--label", "coder", "workstation.coder", "other-host"],
            vec!["--unknown", "workstation.coder", "--label", "coder"],
            vec!["--label", "--remote-session", "agents", "workstation.coder"],
        ] {
            let args = args.into_iter().map(str::to_owned).collect::<Vec<_>>();
            assert!(parse_add_args(&args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn profile_id_parser_rejects_target_text() {
        assert!(one_profile_id(&["build.example".into()], "usage")
            .unwrap()
            .is_none());
    }

    #[test]
    fn list_rows_do_not_have_credential_fields() {
        let encoded = serde_json::to_string(&MachineListRow {
            id: "0123456789abcdef0123456789abcdef",
            label: "Build",
            target: "dev@build",
            session: "agents",
            enabled: true,
            selected: false,
        })
        .unwrap();
        assert!(!encoded.contains("password"));
        assert!(!encoded.contains("key"));
    }
}
