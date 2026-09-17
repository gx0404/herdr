//! `herdr snippet`: client-local command snippets. Snippets render
//! `{{variable}}` placeholders and are sent to panes through the existing
//! JSON API (`pane.send-input` / `pane.send-text`), so no wire change is
//! involved; `--machine` targets go through the saved-machine SSH bridge one
//! machine at a time and collect one outcome per target.

use serde::Serialize;

use crate::api::client::{ApiClient, ConnectionTarget};
use crate::api::schema::{Method, PaneSendInputParams, PaneSendTextParams, Request};
use crate::client::endpoint::{
    render_snippet_command, EndpointCatalog, SavedSshEndpoint, SnippetExecution, SnippetLibrary,
};

// Snippet strings live in the i18n text tables (`cli_output` / `cli_errors`),
// like every other CLI surface.

const SNIPPET_ADD_VALUE_FLAGS: &[&str] = &[
    "--label",
    "--command",
    "--description",
    "--variable",
    "--tag",
];
const SNIPPET_RUN_VALUE_FLAGS: &[&str] = &["--machine", "--pane", "--var"];

#[derive(Serialize)]
struct SnippetListRow<'a> {
    id: &'a str,
    label: &'a str,
    command: &'a str,
    description: Option<&'a str>,
    variables: &'a [String],
    tags: &'a [String],
}

#[derive(Serialize)]
struct RunOutcomeRow {
    machine: String,
    pane_id: String,
    success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

pub(super) fn run_snippet_command(args: &[String]) -> std::io::Result<i32> {
    match args.first().map(String::as_str) {
        Some("list") => list(&args[1..]),
        Some("add") => add(&args[1..]),
        Some("remove") => remove(&args[1..]),
        Some("run") => run(&args[1..]),
        Some("help" | "--help" | "-h") => {
            eprintln!("{}", crate::i18n::texts().cli_errors.snippet_usage);
            Ok(0)
        }
        _ => {
            eprintln!("{}", crate::i18n::texts().cli_errors.snippet_usage);
            Ok(2)
        }
    }
}

fn list(args: &[String]) -> std::io::Result<i32> {
    let json = match args {
        [] => false,
        [flag] if flag == "--json" => true,
        _ => {
            eprintln!("{}", crate::i18n::texts().cli_errors.snippet_usage);
            return Ok(2);
        }
    };
    let library = SnippetLibrary::load().map_err(std::io::Error::other)?;
    let rows = library
        .snippets
        .iter()
        .map(|snippet| SnippetListRow {
            id: snippet.id.as_str(),
            label: &snippet.label,
            command: &snippet.command,
            description: snippet.description.as_deref(),
            variables: &snippet.variables,
            tags: &snippet.tags,
        })
        .collect::<Vec<_>>();
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&rows).map_err(std::io::Error::other)?
        );
        return Ok(0);
    }
    if rows.is_empty() {
        println!("{}", crate::i18n::texts().cli_output.snippet_none_saved);
        return Ok(0);
    }
    for row in rows {
        println!("{}\t{}\t{}", row.id, row.label, row.command);
    }
    Ok(0)
}

#[derive(Debug, PartialEq, Eq)]
struct AddArgs {
    label: String,
    command: String,
    description: Option<String>,
    variables: Vec<String>,
    tags: Vec<String>,
}

fn parse_add_args(args: &[String]) -> Result<AddArgs, String> {
    let t = &crate::i18n::texts().cli_errors;
    let args = super::expand_equals_args(args, SNIPPET_ADD_VALUE_FLAGS);
    let mut label = None;
    let mut command = None;
    let mut description = None;
    let mut variables = Vec::new();
    let mut tags = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        if SNIPPET_ADD_VALUE_FLAGS.contains(&arg) {
            let Some(value) = args.get(index + 1) else {
                return Err(crate::i18n::fill(t.missing_value_for_fmt, &[("flag", arg)]));
            };
            match arg {
                "--label" => {
                    if label.is_some() {
                        return Err(crate::i18n::fill(
                            t.machine_option_specified_twice_fmt,
                            &[("option", arg)],
                        ));
                    }
                    label = Some(value.clone());
                }
                "--command" => {
                    if command.is_some() {
                        return Err(crate::i18n::fill(
                            t.machine_option_specified_twice_fmt,
                            &[("option", arg)],
                        ));
                    }
                    command = Some(value.clone());
                }
                "--description" => {
                    if description.is_some() {
                        return Err(crate::i18n::fill(
                            t.machine_option_specified_twice_fmt,
                            &[("option", arg)],
                        ));
                    }
                    description = Some(value.clone());
                }
                "--variable" => variables.push(value.clone()),
                "--tag" => tags.push(value.clone()),
                _ => unreachable!("validated snippet add value flag"),
            }
            index += 2;
            continue;
        }
        return Err(crate::i18n::fill(
            t.unknown_option_or_argument_fmt,
            &[("option", arg)],
        ));
    }
    let Some(label) = label else {
        return Err(t.snippet_label_required.into());
    };
    let Some(command) = command else {
        return Err(t.snippet_command_required.into());
    };
    Ok(AddArgs {
        label,
        command,
        description,
        variables,
        tags,
    })
}

fn add(args: &[String]) -> std::io::Result<i32> {
    let parsed = match parse_add_args(args) {
        Ok(parsed) => parsed,
        Err(error) => {
            eprintln!("{}{error}", crate::i18n::texts().cli_errors.error_prefix);
            return Ok(2);
        }
    };
    let mut library = SnippetLibrary::load().map_err(std::io::Error::other)?;
    let id = match library.add_snippet(
        parsed.label,
        parsed.command,
        parsed.description,
        parsed.variables,
        parsed.tags,
    ) {
        Ok(id) => id,
        Err(error) => {
            eprintln!("{}{error}", crate::i18n::texts().cli_errors.error_prefix);
            return Ok(2);
        }
    };
    library.store().map_err(std::io::Error::other)?;
    println!(
        "{}",
        crate::i18n::fill(
            crate::i18n::texts().cli_output.snippet_saved_fmt,
            &[("id", id.as_str())]
        )
    );
    Ok(0)
}

fn remove(args: &[String]) -> std::io::Result<i32> {
    let [selector] = args else {
        eprintln!("{}", crate::i18n::texts().cli_errors.snippet_usage);
        return Ok(2);
    };
    let mut library = SnippetLibrary::load().map_err(std::io::Error::other)?;
    let snippet = match library.resolve(selector) {
        Ok(Some(snippet)) => snippet.clone(),
        Ok(None) => {
            eprintln!(
                "{}{}",
                crate::i18n::texts().cli_errors.error_prefix,
                crate::i18n::fill(
                    crate::i18n::texts().cli_errors.snippet_not_found_fmt,
                    &[("selector", selector)]
                )
            );
            return Ok(1);
        }
        Err(error) => {
            eprintln!("{}{error}", crate::i18n::texts().cli_errors.error_prefix);
            return Ok(2);
        }
    };
    if !library.remove_snippet(&snippet.id) {
        eprintln!(
            "{}{}",
            crate::i18n::texts().cli_errors.error_prefix,
            crate::i18n::fill(
                crate::i18n::texts().cli_errors.snippet_not_found_fmt,
                &[("selector", selector)]
            )
        );
        return Ok(1);
    }
    library.store().map_err(std::io::Error::other)?;
    println!(
        "{}",
        crate::i18n::fill(
            crate::i18n::texts().cli_output.snippet_removed_fmt,
            &[("id", snippet.id.as_str())]
        )
    );
    Ok(0)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RunTarget {
    /// Saved machine selector; `None` targets the local server.
    machine: Option<String>,
    pane_id: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
struct RunArgs {
    selector: String,
    targets: Vec<RunTarget>,
    variables: Vec<(String, String)>,
    press_enter: bool,
    json: bool,
}

fn parse_run_args(args: &[String]) -> Result<RunArgs, String> {
    let t = &crate::i18n::texts().cli_errors;
    let args = super::expand_equals_args(args, SNIPPET_RUN_VALUE_FLAGS);
    let Some(selector) = args.first().cloned() else {
        return Err(t.snippet_usage.into());
    };
    if selector.starts_with('-') {
        return Err(t.snippet_usage.into());
    }
    let mut targets: Vec<RunTarget> = Vec::new();
    let mut variables = Vec::new();
    let mut press_enter = true;
    let mut json = false;
    let mut index = 1;
    while index < args.len() {
        let arg = args[index].as_str();
        match arg {
            "--machine" => {
                let Some(value) = args.get(index + 1) else {
                    return Err(crate::i18n::fill(t.missing_value_for_fmt, &[("flag", arg)]));
                };
                targets.push(RunTarget {
                    machine: Some(value.clone()),
                    pane_id: None,
                });
                index += 2;
            }
            "--local" => {
                targets.push(RunTarget {
                    machine: None,
                    pane_id: None,
                });
                index += 1;
            }
            "--pane" => {
                let Some(value) = args.get(index + 1) else {
                    return Err(crate::i18n::fill(t.missing_value_for_fmt, &[("flag", arg)]));
                };
                match targets.last_mut() {
                    Some(target) if target.pane_id.is_none() => {
                        target.pane_id = Some(value.clone());
                    }
                    Some(_) => return Err(t.snippet_run_pane_twice.into()),
                    None => {
                        targets.push(RunTarget {
                            machine: None,
                            pane_id: Some(value.clone()),
                        });
                    }
                }
                index += 2;
            }
            "--var" => {
                let Some(value) = args.get(index + 1) else {
                    return Err(crate::i18n::fill(t.missing_value_for_fmt, &[("flag", arg)]));
                };
                let (key, value) = super::parse_env_assignment(value)?;
                variables.push((key, value));
                index += 2;
            }
            "--no-enter" => {
                press_enter = false;
                index += 1;
            }
            "--json" => {
                json = true;
                index += 1;
            }
            unknown => {
                return Err(crate::i18n::fill(
                    t.unknown_option_or_argument_fmt,
                    &[("option", unknown)],
                ))
            }
        }
    }
    if targets.is_empty() {
        return Err(t.snippet_run_target_required.into());
    }
    for target in &targets {
        if target.pane_id.is_none() {
            let machine = target.machine.as_deref().unwrap_or("local");
            return Err(crate::i18n::fill(
                t.snippet_run_pane_required_fmt,
                &[("machine", machine)],
            ));
        }
    }
    Ok(RunArgs {
        selector,
        targets,
        variables,
        press_enter,
        json,
    })
}

fn run(args: &[String]) -> std::io::Result<i32> {
    let parsed = match parse_run_args(args) {
        Ok(parsed) => parsed,
        Err(error) => {
            eprintln!("{}{error}", crate::i18n::texts().cli_errors.error_prefix);
            return Ok(2);
        }
    };
    let mut library = SnippetLibrary::load().map_err(std::io::Error::other)?;
    let snippet = match library.resolve(&parsed.selector) {
        Ok(Some(snippet)) => snippet.clone(),
        Ok(None) => {
            eprintln!(
                "{}{}",
                crate::i18n::texts().cli_errors.error_prefix,
                crate::i18n::fill(
                    crate::i18n::texts().cli_errors.snippet_not_found_fmt,
                    &[("selector", &parsed.selector)]
                )
            );
            return Ok(1);
        }
        Err(error) => {
            eprintln!("{}{error}", crate::i18n::texts().cli_errors.error_prefix);
            return Ok(2);
        }
    };
    let text = match render_snippet_command(&snippet.command, &parsed.variables) {
        Ok(text) => text,
        Err(error) => {
            eprintln!("{}{error}", crate::i18n::texts().cli_errors.error_prefix);
            return Ok(2);
        }
    };
    let profiles = EndpointCatalog::load_profiles().map_err(std::io::Error::other)?;
    let executed_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    // Bridges keep their ssh child alive until every request is done.
    let mut bridges = Vec::new();
    let mut rows = Vec::new();
    for target in &parsed.targets {
        let pane_id = target.pane_id.clone().unwrap_or_default();
        let (machine, result) =
            execute_target(target, &profiles, &text, parsed.press_enter, &mut bridges);
        rows.push(RunOutcomeRow {
            machine,
            pane_id,
            success: result.is_ok(),
            error: result.err(),
        });
    }
    drop(bridges);

    library.record_executions(
        rows.iter()
            .map(|row| SnippetExecution {
                snippet_id: snippet.id.clone(),
                snippet_label: snippet.label.clone(),
                machine: row.machine.clone(),
                pane_id: row.pane_id.clone(),
                executed_at,
                success: row.success,
                error: row.error.clone(),
            })
            .collect(),
    );
    if let Err(error) = library.store() {
        eprintln!(
            "{}{}",
            crate::i18n::texts().cli_errors.error_prefix,
            crate::i18n::fill(
                crate::i18n::texts()
                    .cli_errors
                    .snippet_history_store_failed_fmt,
                &[("error", &error)]
            )
        );
    }

    let failed = rows.iter().filter(|row| !row.success).count();
    if parsed.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&rows).map_err(std::io::Error::other)?
        );
    } else {
        for row in &rows {
            match &row.error {
                Some(error) => eprintln!(
                    "{}",
                    crate::i18n::fill(
                        crate::i18n::texts().cli_errors.snippet_run_failed_fmt,
                        &[
                            ("machine", &row.machine),
                            ("pane", &row.pane_id),
                            ("error", error)
                        ]
                    )
                ),
                None => println!(
                    "{}",
                    crate::i18n::fill(
                        crate::i18n::texts().cli_output.snippet_run_sent_fmt,
                        &[("machine", &row.machine), ("pane", &row.pane_id)]
                    )
                ),
            }
        }
    }
    Ok(i32::from(failed > 0))
}

fn execute_target(
    target: &RunTarget,
    profiles: &[SavedSshEndpoint],
    text: &str,
    press_enter: bool,
    bridges: &mut Vec<crate::remote::SavedSshApiBridge>,
) -> (String, Result<(), String>) {
    let pane_id = target.pane_id.clone().unwrap_or_default();
    let (machine, client) = match &target.machine {
        None => ("local".to_string(), Ok(ApiClient::local())),
        Some(selector) => match super::target::resolve_machine(profiles, selector) {
            Ok(profile) => {
                let machine = profile.id.to_string();
                let client = crate::remote::SavedSshApiBridge::start(profile).map(|bridge| {
                    let client = ApiClient::for_target(ConnectionTarget::SocketPath(
                        bridge.socket_path().to_owned(),
                    ));
                    bridges.push(bridge);
                    client
                });
                (machine, client)
            }
            Err(error) => (selector.clone(), Err(std::io::Error::other(error))),
        },
    };
    let client = match client {
        Ok(client) => client,
        Err(error) => return (machine, Err(error.to_string())),
    };
    let request_id = format!("cli:snippet:run:{machine}");
    if let Err(error) = super::ensure_server_protocol_compatible(&client, &request_id) {
        let message = if super::protocol_mismatch_was_reported(&error) {
            crate::i18n::texts()
                .cli_errors
                .snippet_run_protocol_mismatch
                .to_string()
        } else {
            error.to_string()
        };
        return (machine, Err(message));
    }
    let method = if press_enter {
        Method::PaneSendInput(PaneSendInputParams {
            pane_id: pane_id.clone(),
            text: text.to_owned(),
            keys: vec!["Enter".into()],
        })
    } else {
        Method::PaneSendText(PaneSendTextParams {
            pane_id: pane_id.clone(),
            text: text.to_owned(),
        })
    };
    let response = client
        .request_value(&Request {
            id: request_id,
            method,
        })
        .map_err(|error| {
            let error = match error {
                crate::api::client::ApiClientError::Io(error) => error,
                other => std::io::Error::other(other),
            };
            if super::server_not_running_error(&error) {
                crate::i18n::texts()
                    .cli_errors
                    .snippet_run_server_not_running
                    .to_string()
            } else {
                error.to_string()
            }
        });
    let response = match response {
        Ok(response) => response,
        Err(message) => return (machine, Err(message)),
    };
    if let Some(error) = response.get("error") {
        let message = error
            .get("message")
            .and_then(|message| message.as_str())
            .unwrap_or(crate::i18n::texts().cli_errors.snippet_run_request_failed)
            .to_string();
        return (machine, Err(message));
    }
    (machine, Ok(()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn add_parser_collects_every_field() {
        let parsed = parse_add_args(&args(&[
            "--label",
            "deploy",
            "--command=kubectl rollout restart deploy/{{name}}",
            "--description",
            "restart it",
            "--variable",
            "name",
            "--tag=ops",
        ]))
        .unwrap();
        assert_eq!(
            parsed,
            AddArgs {
                label: "deploy".into(),
                command: "kubectl rollout restart deploy/{{name}}".into(),
                description: Some("restart it".into()),
                variables: vec!["name".into()],
                tags: vec!["ops".into()],
            }
        );
    }

    #[test]
    fn add_parser_requires_label_and_command() {
        assert!(parse_add_args(&args(&["--command", "x"])).is_err());
        assert!(parse_add_args(&args(&["--label", "x"])).is_err());
        assert!(parse_add_args(&args(&["--label", "x", "--command"])).is_err());
        assert!(parse_add_args(&args(&["--label", "x", "--command", "y", "z"])).is_err());
        assert!(
            parse_add_args(&args(&["--label", "a", "--label", "b", "--command", "y"])).is_err()
        );
    }

    #[test]
    fn run_parser_pairs_panes_with_their_machine() {
        let parsed = parse_run_args(&args(&[
            "deploy",
            "--machine",
            "build",
            "--pane",
            "w1:p1",
            "--machine=mac",
            "--pane",
            "w2:p3",
            "--local",
            "--pane",
            "w9:p9",
            "--var",
            "name=api",
            "--var=ns=prod",
            "--no-enter",
            "--json",
        ]))
        .unwrap();
        assert_eq!(
            parsed.targets,
            vec![
                RunTarget {
                    machine: Some("build".into()),
                    pane_id: Some("w1:p1".into()),
                },
                RunTarget {
                    machine: Some("mac".into()),
                    pane_id: Some("w2:p3".into()),
                },
                RunTarget {
                    machine: None,
                    pane_id: Some("w9:p9".into()),
                },
            ]
        );
        assert_eq!(
            parsed.variables,
            vec![
                ("name".to_string(), "api".to_string()),
                ("ns".to_string(), "prod".to_string()),
            ]
        );
        assert!(!parsed.press_enter);
        assert!(parsed.json);
    }

    #[test]
    fn run_parser_defaults_to_an_implicit_local_target() {
        let parsed = parse_run_args(&args(&["deploy", "--pane", "w1:p1"])).unwrap();
        assert_eq!(
            parsed.targets,
            vec![RunTarget {
                machine: None,
                pane_id: Some("w1:p1".into()),
            }]
        );
        assert!(parsed.press_enter);
    }

    #[test]
    fn run_parser_rejects_missing_or_duplicate_panes() {
        assert!(parse_run_args(&args(&["deploy"])).is_err());
        assert!(parse_run_args(&args(&["deploy", "--machine", "mac"])).is_err());
        assert!(parse_run_args(&args(&["deploy", "--pane", "a", "--pane", "b"])).is_err());
        assert!(parse_run_args(&args(&["deploy", "--machine"])).is_err());
        assert!(parse_run_args(&args(&["deploy", "--var", "no-equals"])).is_err());
        assert!(parse_run_args(&args(&["--pane", "w1:p1"])).is_err());
        assert!(parse_run_args(&args(&["deploy", "--unknown"])).is_err());
    }
}
