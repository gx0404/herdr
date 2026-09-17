//! `herdr machine log <profile-id> ...`: session-log preferences of a saved
//! machine (see `endpoint::session_log`). Toggling logging edits only the
//! profile's `session_log` field — the connection never restarts for it —
//! and `dump` runs one snapshot cycle synchronously for verification.

use serde::Serialize;

use crate::client::endpoint::{
    EndpointCatalog, ProfileId, SessionLogProfile, DEFAULT_MAX_LOG_BYTES,
};

fn usage_text() -> &'static str {
    crate::i18n::texts().cli_errors.machine_log_usage
}

#[derive(Serialize)]
struct LogConfigRow<'a> {
    enabled: bool,
    path_template: Option<&'a str>,
    max_bytes: Option<u64>,
    dump_interval_secs: Option<u16>,
    default_path_template: &'static str,
    default_max_bytes: u64,
}

pub(super) fn run_log_command(args: &[String]) -> std::io::Result<i32> {
    let Some(raw_id) = args.first() else {
        eprintln!("{}", usage_text());
        return Ok(2);
    };
    if matches!(raw_id.as_str(), "help" | "--help" | "-h") {
        println!("{}", usage_text());
        return Ok(0);
    }
    let id = match ProfileId::parse(raw_id.clone()) {
        Ok(id) => id,
        Err(error) => {
            eprintln!("{}{error}", crate::i18n::texts().cli_errors.error_prefix);
            return Ok(2);
        }
    };
    match args.get(1).map(String::as_str) {
        Some("show") => show(&id, &args[2..]),
        Some("on") => set_enabled(&id, true, &args[2..]),
        Some("off") => set_enabled(&id, false, &args[2..]),
        Some("dump") => dump(&id, &args[2..]),
        _ => {
            eprintln!("{}", usage_text());
            Ok(2)
        }
    }
}

fn show(id: &ProfileId, args: &[String]) -> std::io::Result<i32> {
    let json = match args {
        [] => false,
        [flag] if flag == "--json" => true,
        _ => {
            eprintln!("{}", usage_text());
            return Ok(2);
        }
    };
    let catalog = super::machine::load_catalog()?;
    let Some(profile) = catalog.ssh.iter().find(|profile| &profile.id == id) else {
        return profile_not_found(id);
    };
    let config = profile.session_log.clone().unwrap_or_default();
    let row = LogConfigRow {
        enabled: config.enabled,
        path_template: config.path_template.as_deref(),
        max_bytes: config.max_bytes,
        dump_interval_secs: config.dump_interval_secs,
        default_path_template: crate::client::endpoint::DEFAULT_PATH_TEMPLATE,
        default_max_bytes: DEFAULT_MAX_LOG_BYTES,
    };
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&row).map_err(std::io::Error::other)?
        );
        return Ok(0);
    }
    println!(
        "enabled\t{}\npath-template\t{}\nmax-bytes\t{}\ndump-interval-secs\t{}",
        row.enabled,
        row.path_template.unwrap_or(row.default_path_template),
        row.max_bytes.unwrap_or(row.default_max_bytes),
        row.dump_interval_secs
            .unwrap_or(crate::client::endpoint::DEFAULT_DUMP_INTERVAL_SECS),
    );
    Ok(0)
}

const LOG_VALUE_FLAGS: &[&str] = &["--path", "--max-bytes", "--interval"];

fn set_enabled(id: &ProfileId, enabled: bool, args: &[String]) -> std::io::Result<i32> {
    let mut catalog = super::machine::load_catalog()?;
    let Some(profile) = catalog.ssh.iter().find(|profile| &profile.id == id) else {
        return profile_not_found(id);
    };
    let mut config = profile.session_log.clone().unwrap_or_default();
    config.enabled = enabled;
    if enabled {
        match parse_on_args(args, &mut config) {
            Ok(()) => {}
            Err(message) => {
                eprintln!("{}{message}", crate::i18n::texts().cli_errors.error_prefix);
                return Ok(2);
            }
        }
    } else if !args.is_empty() {
        eprintln!("{}", usage_text());
        return Ok(2);
    }
    match catalog.set_session_log(id, Some(config)) {
        Ok(true) => {}
        Ok(false) => return profile_not_found(id),
        Err(message) => {
            eprintln!("{}{message}", crate::i18n::texts().cli_errors.error_prefix);
            return Ok(2);
        }
    }
    store_catalog(&catalog)?;
    let t = &crate::i18n::texts().cli_output;
    println!(
        "{}",
        crate::i18n::fill(
            if enabled {
                t.machine_log_enabled_fmt
            } else {
                t.machine_log_disabled_fmt
            },
            &[("id", id.as_str())],
        )
    );
    Ok(0)
}

fn parse_on_args(args: &[String], config: &mut SessionLogProfile) -> Result<(), String> {
    let args = super::expand_equals_args(args, LOG_VALUE_FLAGS);
    let t = &crate::i18n::texts().cli_errors;
    let mut index = 0;
    while index < args.len() {
        let Some(value) = args.get(index + 1) else {
            return Err(crate::i18n::fill(
                t.missing_value_for_fmt,
                &[("flag", &args[index])],
            ));
        };
        match args[index].as_str() {
            "--path" => config.path_template = Some(value.clone()),
            "--max-bytes" => {
                config.max_bytes = Some(value.parse::<u64>().map_err(|_| {
                    crate::i18n::fill(
                        t.invalid_flag_value_fmt,
                        &[("flag", "--max-bytes"), ("value", value)],
                    )
                })?);
            }
            "--interval" => {
                config.dump_interval_secs = Some(value.parse::<u16>().map_err(|_| {
                    crate::i18n::fill(
                        t.invalid_flag_value_fmt,
                        &[("flag", "--interval"), ("value", value)],
                    )
                })?);
            }
            other => {
                return Err(crate::i18n::fill(
                    t.unknown_option_fmt,
                    &[("option", other)],
                ))
            }
        }
        index += 2;
    }
    Ok(())
}

fn dump(id: &ProfileId, args: &[String]) -> std::io::Result<i32> {
    if !args.is_empty() {
        eprintln!("{}", usage_text());
        return Ok(2);
    }
    let catalog = super::machine::load_catalog()?;
    let Some(profile) = catalog.ssh.iter().find(|profile| &profile.id == id) else {
        return profile_not_found(id);
    };
    match crate::client::endpoint::dump_session_log_once(profile) {
        Ok(appended) => {
            println!(
                "{}",
                crate::i18n::fill(
                    crate::i18n::texts().cli_output.machine_log_dumped_fmt,
                    &[("count", &appended.to_string())],
                )
            );
            Ok(0)
        }
        Err(error) => {
            eprintln!("{}{error}", crate::i18n::texts().cli_errors.error_prefix);
            Ok(1)
        }
    }
}

fn profile_not_found(id: &ProfileId) -> std::io::Result<i32> {
    eprintln!(
        "{}{}",
        crate::i18n::texts().cli_errors.error_prefix,
        crate::i18n::fill(
            crate::i18n::texts()
                .cli_errors
                .machine_profile_not_found_fmt,
            &[("id", id.as_str())]
        )
    );
    Ok(1)
}

fn store_catalog(catalog: &EndpointCatalog) -> std::io::Result<()> {
    catalog.store_profiles().map_err(std::io::Error::other)
}
