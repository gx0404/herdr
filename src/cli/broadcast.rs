//! `herdr broadcast`: the cross-endpoint input broadcast target set.
//!
//! The set lives in `endpoint::broadcast` (registration, query, clearing,
//! persistence). This executor fans text out to every registered pane
//! through the existing per-endpoint JSON API channels — the local API
//! socket for Local, a saved-machine SSH API bridge per machine — with
//! `pane.send-input` / `pane.send-text`, one target at a time, exactly like
//! `snippet run`. Two safety gates stay explicit: the set is disabled by
//! default and `send` refuses to run on a disabled or empty set.

use serde::Serialize;

use crate::api::client::{ApiClient, ConnectionTarget};
use crate::api::schema::{Method, PaneSendInputParams, PaneSendTextParams, Request};
use crate::client::endpoint::{BroadcastSet, BroadcastTarget, EndpointCatalog, ProfileId};

fn usage() -> &'static str {
    crate::i18n::texts().cli_errors.broadcast_usage
}

#[derive(Serialize)]
struct StatusRow {
    enabled: bool,
    targets: Vec<TargetRow>,
}

#[derive(Serialize)]
struct TargetRow {
    number: usize,
    machine: String,
    pane_id: String,
}

#[derive(Serialize)]
struct SendOutcomeRow {
    machine: String,
    pane_id: String,
    success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

pub(super) fn run_broadcast_command(args: &[String]) -> std::io::Result<i32> {
    match args.first().map(String::as_str) {
        Some("status") => status(&args[1..]),
        Some("enable") => set_enabled(&args[1..], true),
        Some("disable") => set_enabled(&args[1..], false),
        Some("add") => add(&args[1..]),
        Some("remove") => remove(&args[1..]),
        Some("clear") => clear(&args[1..]),
        Some("send") => send(&args[1..]),
        Some("help" | "--help" | "-h") => {
            println!("{}", usage());
            Ok(0)
        }
        _ => {
            eprintln!("{}", usage());
            Ok(2)
        }
    }
}

fn load_set() -> std::io::Result<BroadcastSet> {
    BroadcastSet::load().map_err(std::io::Error::other)
}

fn store_set(set: &BroadcastSet) -> std::io::Result<()> {
    set.store().map_err(std::io::Error::other)
}

fn machine_label(catalog: &EndpointCatalog, machine: Option<&ProfileId>) -> String {
    match machine {
        None => "local".to_string(),
        Some(id) => catalog
            .ssh
            .iter()
            .find(|profile| &profile.id == id)
            .map(|profile| profile.label.clone())
            .unwrap_or_else(|| id.to_string()),
    }
}

fn status(args: &[String]) -> std::io::Result<i32> {
    let json = match args {
        [] => false,
        [flag] if flag == "--json" => true,
        _ => {
            eprintln!("{}", usage());
            return Ok(2);
        }
    };
    let set = load_set()?;
    let catalog = EndpointCatalog::load().map_err(std::io::Error::other)?;
    let row = StatusRow {
        enabled: set.enabled,
        targets: set
            .targets()
            .iter()
            .enumerate()
            .map(|(index, target)| TargetRow {
                number: index + 1,
                machine: machine_label(&catalog, target.machine.as_ref()),
                pane_id: target.pane_id.clone(),
            })
            .collect(),
    };
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&row).map_err(std::io::Error::other)?
        );
        return Ok(0);
    }
    let t = &crate::i18n::texts().cli_output;
    println!(
        "broadcast\t{}",
        if row.enabled {
            t.broadcast_enabled
        } else {
            t.broadcast_disabled
        }
    );
    if row.targets.is_empty() {
        println!("{}", t.broadcast_no_targets);
    }
    for target in &row.targets {
        println!("{}\t{}\t{}", target.number, target.machine, target.pane_id);
    }
    Ok(0)
}

fn set_enabled(args: &[String], enabled: bool) -> std::io::Result<i32> {
    if !args.is_empty() {
        eprintln!("{}", usage());
        return Ok(2);
    }
    let mut set = load_set()?;
    set.enabled = enabled;
    store_set(&set)?;
    let t = &crate::i18n::texts().cli_output;
    println!(
        "{}",
        if enabled {
            t.broadcast_enabled
        } else {
            t.broadcast_disabled
        }
    );
    Ok(0)
}

fn add(args: &[String]) -> std::io::Result<i32> {
    let args = super::expand_equals_args(args, &["--machine", "--pane"]);
    let mut machine = None;
    let mut pane_id = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--machine" | "--pane" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!(
                        "{}{}",
                        crate::i18n::texts().cli_errors.error_prefix,
                        crate::i18n::fill(
                            crate::i18n::texts().cli_errors.missing_value_for_fmt,
                            &[("flag", &args[index])],
                        )
                    );
                    return Ok(2);
                };
                if args[index] == "--machine" {
                    machine = Some(value.clone());
                } else {
                    pane_id = Some(value.clone());
                }
                index += 2;
            }
            other => {
                eprintln!(
                    "{}{}",
                    crate::i18n::texts().cli_errors.error_prefix,
                    crate::i18n::fill(
                        crate::i18n::texts().cli_errors.unknown_option_fmt,
                        &[("option", other)],
                    )
                );
                return Ok(2);
            }
        }
    }
    let Some(pane_id) = pane_id else {
        eprintln!("{}", usage());
        return Ok(2);
    };
    let catalog = EndpointCatalog::load().map_err(std::io::Error::other)?;
    let machine = match &machine {
        None => None,
        Some(selector) => match super::target::resolve_machine(&catalog.ssh, selector) {
            Ok(profile) => Some(profile.id.clone()),
            Err(error) => {
                eprintln!("{}{error}", crate::i18n::texts().cli_errors.error_prefix);
                return Ok(2);
            }
        },
    };
    let mut set = load_set()?;
    if let Err(error) = set.add_target(BroadcastTarget {
        machine: machine.clone(),
        pane_id: pane_id.clone(),
    }) {
        eprintln!("{}{error}", crate::i18n::texts().cli_errors.error_prefix);
        return Ok(2);
    }
    store_set(&set)?;
    println!(
        "{}",
        crate::i18n::fill(
            crate::i18n::texts().cli_output.broadcast_registered_fmt,
            &[
                ("machine", &machine_label(&catalog, machine.as_ref())),
                ("pane", &pane_id),
            ],
        )
    );
    Ok(0)
}

fn remove(args: &[String]) -> std::io::Result<i32> {
    let [number] = args else {
        eprintln!("{}", usage());
        return Ok(2);
    };
    let number: usize = match number.parse() {
        Ok(number) => number,
        Err(_) => {
            eprintln!(
                "{}{}",
                crate::i18n::texts().cli_errors.error_prefix,
                crate::i18n::fill(
                    crate::i18n::texts().cli_errors.broadcast_invalid_number_fmt,
                    &[("value", number)],
                )
            );
            return Ok(2);
        }
    };
    let mut set = load_set()?;
    match set.remove_target(number) {
        Ok(removed) => {
            store_set(&set)?;
            println!(
                "{}",
                crate::i18n::fill(
                    crate::i18n::texts().cli_output.broadcast_removed_fmt,
                    &[("number", &number.to_string()), ("pane", &removed.pane_id)],
                )
            );
            Ok(0)
        }
        Err(error) => {
            eprintln!("{}{error}", crate::i18n::texts().cli_errors.error_prefix);
            Ok(2)
        }
    }
}

fn clear(args: &[String]) -> std::io::Result<i32> {
    if !args.is_empty() {
        eprintln!("{}", usage());
        return Ok(2);
    }
    let mut set = load_set()?;
    set.clear();
    store_set(&set)?;
    println!("{}", crate::i18n::texts().cli_output.broadcast_cleared);
    Ok(0)
}

fn send(args: &[String]) -> std::io::Result<i32> {
    let mut press_enter = true;
    let mut json = false;
    let mut text_parts = Vec::new();
    for arg in args {
        match arg.as_str() {
            "--no-enter" => press_enter = false,
            "--json" => json = true,
            _ => text_parts.push(arg.as_str()),
        }
    }
    let text = text_parts.join(" ");
    if text.is_empty() {
        eprintln!("{}", usage());
        return Ok(2);
    }
    let set = load_set()?;
    if !set.enabled {
        eprintln!(
            "{}{}",
            crate::i18n::texts().cli_errors.error_prefix,
            crate::i18n::texts().cli_errors.broadcast_gate_disabled
        );
        return Ok(2);
    }
    if set.is_empty() {
        eprintln!(
            "{}{}",
            crate::i18n::texts().cli_errors.error_prefix,
            crate::i18n::texts().cli_errors.broadcast_gate_empty
        );
        return Ok(2);
    }
    let catalog = EndpointCatalog::load().map_err(std::io::Error::other)?;
    // Bridges keep their ssh child alive until every request is done.
    let mut bridges = Vec::new();
    let mut rows = Vec::new();
    for target in set.targets() {
        let (machine, result) = send_target(&catalog, target, &text, press_enter, &mut bridges);
        rows.push(SendOutcomeRow {
            machine,
            pane_id: target.pane_id.clone(),
            success: result.is_ok(),
            error: result.err(),
        });
    }
    drop(bridges);
    let failed = rows.iter().filter(|row| !row.success).count();
    if json {
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
                        crate::i18n::texts().cli_output.broadcast_failed_fmt,
                        &[
                            ("machine", &row.machine),
                            ("pane", &row.pane_id),
                            ("error", error)
                        ],
                    )
                ),
                None => println!(
                    "{}",
                    crate::i18n::fill(
                        crate::i18n::texts().cli_output.broadcast_sent_fmt,
                        &[("machine", &row.machine), ("pane", &row.pane_id)],
                    )
                ),
            }
        }
    }
    Ok(i32::from(failed > 0))
}

fn send_target(
    catalog: &EndpointCatalog,
    target: &BroadcastTarget,
    text: &str,
    press_enter: bool,
    bridges: &mut Vec<crate::remote::SavedSshApiBridge>,
) -> (String, Result<(), String>) {
    let machine = machine_label(catalog, target.machine.as_ref());
    let client = match &target.machine {
        None => Ok(ApiClient::local()),
        Some(id) => match catalog.ssh.iter().find(|profile| &profile.id == id) {
            Some(profile) => crate::remote::SavedSshApiBridge::start(profile)
                .map(|bridge| {
                    let client = ApiClient::for_target(ConnectionTarget::SocketPath(
                        bridge.socket_path().to_owned(),
                    ));
                    bridges.push(bridge);
                    client
                })
                .map_err(|error| error.to_string()),
            None => Err(format!("machine {id} is no longer saved")),
        },
    };
    let client = match client {
        Ok(client) => client,
        Err(error) => return (machine, Err(error)),
    };
    let request_id = format!("cli:broadcast:send:{machine}");
    if let Err(error) = super::ensure_server_protocol_compatible(&client, &request_id) {
        return (machine, Err(error.to_string()));
    }
    let method = if press_enter {
        Method::PaneSendInput(PaneSendInputParams {
            pane_id: target.pane_id.clone(),
            text: text.to_owned(),
            keys: vec!["Enter".into()],
        })
    } else {
        Method::PaneSendText(PaneSendTextParams {
            pane_id: target.pane_id.clone(),
            text: text.to_owned(),
        })
    };
    let response = client.request_value(&Request {
        id: request_id,
        method,
    });
    let response = match response {
        Ok(response) => response,
        Err(error) => return (machine, Err(error.to_string())),
    };
    if let Some(error) = response.get("error") {
        let message = error
            .get("message")
            .and_then(|message| message.as_str())
            .unwrap_or("broadcast request failed")
            .to_string();
        return (machine, Err(message));
    }
    (machine, Ok(()))
}
