use std::io::{IsTerminal as _, Write as _};

use serde::Serialize;

use crate::client::endpoint::{
    EndpointCatalog, PortForwardKind, PortForwardRule, ProfileId, ProxyJumpHop, SshProfileOptions,
    StrictHostKeyChecking,
};

#[derive(Serialize)]
struct MachineListRow<'a> {
    id: &'a str,
    label: &'a str,
    target: &'a str,
    session: &'a str,
    enabled: bool,
    selected: bool,
    group: Option<&'a str>,
    tags: &'a [String],
    color: Option<&'a str>,
    port: Option<u16>,
    user: Option<&'a str>,
    identity_file: &'a [String],
    identities_only: Option<bool>,
    identity_agent: Option<&'a str>,
    strict_host_key_checking: Option<StrictHostKeyChecking>,
    proxy_jump: &'a [ProxyJumpHop],
    forward_agent: Option<bool>,
    server_alive_interval: Option<u16>,
    server_alive_count_max: Option<u16>,
    control_persist: Option<&'a str>,
    remote_command: Option<&'a str>,
    port_forwards: &'a [PortForwardRule],
}

#[derive(Serialize)]
struct ForwardListRow<'a> {
    number: usize,
    #[serde(flatten)]
    rule: &'a PortForwardRule,
}

pub(super) fn run_machine_command(args: &[String]) -> std::io::Result<i32> {
    match args.first().map(String::as_str) {
        Some("list") => list(&args[1..]),
        Some("add") => add(&args[1..]),
        Some("import") => import(&args[1..]),
        Some("rename") => rename(&args[1..]),
        Some("remove") => remove(&args[1..]),
        Some("enable") => set_enabled(&args[1..], true),
        Some("disable") => set_enabled(&args[1..], false),
        Some("forward") => forward(&args[1..]),
        Some("fs") => super::machine_fs::run_fs_command(&args[1..]),
        Some("log") => super::machine_log::run_log_command(&args[1..]),
        Some("status") => super::machine_status::run_machine_status_command(&args[1..]),
        Some("exec") => super::machine_exec::run_machine_exec_command(&args[1..]),
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
            group: profile.group.as_deref(),
            tags: &profile.tags,
            color: profile.color.as_deref(),
            port: profile.port,
            user: profile.user.as_deref(),
            identity_file: &profile.identity_file,
            identities_only: profile.identities_only,
            identity_agent: profile.identity_agent.as_deref(),
            strict_host_key_checking: profile.strict_host_key_checking,
            proxy_jump: &profile.proxy_jump,
            forward_agent: profile.forward_agent,
            server_alive_interval: profile.server_alive_interval,
            server_alive_count_max: profile.server_alive_count_max,
            control_persist: profile.control_persist.as_deref(),
            remote_command: profile.remote_command.as_deref(),
            port_forwards: &profile.port_forwards,
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
            "{}\t{}\t{}\t{}\t{}\t{}\t{}",
            row.id,
            row.label,
            row.target,
            row.session,
            state,
            row.group.unwrap_or(t.value_none),
            row.color.unwrap_or(t.value_none),
        );
    }
    Ok(0)
}

#[derive(Debug, PartialEq, Eq)]
struct AddArgs {
    target: String,
    label: String,
    session: String,
    options: SshProfileOptions,
    from_config: Option<String>,
}

/// Options that take a value; boolean flags stay unsplit by
/// `expand_equals_args` so an attached `=value` still errors as unknown.
const ADD_VALUE_FLAGS: &[&str] = &[
    "--label",
    "--remote-session",
    "--group",
    "--tag",
    "--color",
    "--port",
    "--user",
    "--identity-file",
    "--identity-agent",
    "--strict-host-key-checking",
    "--proxy-jump",
    "--server-alive-interval",
    "--server-alive-count-max",
    "--control-persist",
    "--remote-command",
    "--from-config",
];

fn machine_option_specified_twice(option: &str) -> String {
    crate::i18n::fill(
        crate::i18n::texts()
            .cli_errors
            .machine_option_specified_twice_fmt,
        &[("option", option)],
    )
}

fn parse_add_args(args: &[String]) -> Result<AddArgs, String> {
    let t = &crate::i18n::texts().cli_errors;
    let args = super::expand_equals_args(args, ADD_VALUE_FLAGS);
    let mut target = None;
    let mut label = None;
    let mut session = None;
    let mut options = SshProfileOptions::default();
    let mut from_config = None;
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        if ADD_VALUE_FLAGS.contains(&arg) {
            let Some(value) = args.get(index + 1) else {
                return Err(crate::i18n::fill(t.missing_value_for_fmt, &[("flag", arg)]));
            };
            apply_add_option(
                &mut label,
                &mut session,
                &mut options,
                &mut from_config,
                arg,
                value,
            )?;
            index += 2;
            continue;
        }
        match arg {
            "--identities-only" => {
                set_once(&mut options.identities_only, true, arg)?;
                index += 1;
            }
            "--forward-agent" => {
                set_once(&mut options.forward_agent, true, arg)?;
                index += 1;
            }
            positional if !positional.starts_with('-') && target.is_none() => {
                target = Some(positional.to_owned());
                index += 1;
            }
            unknown => {
                return Err(crate::i18n::fill(
                    t.machine_add_unknown_option_fmt,
                    &[("option", unknown)],
                ));
            }
        }
    }
    if let Some(alias) = &from_config {
        if target.is_some() {
            return Err(t.machine_add_from_config_with_target.into());
        }
        if has_connection_options(&options) {
            return Err(t.machine_add_from_config_with_options.into());
        }
        let session = session.unwrap_or_else(|| crate::session::DEFAULT_SESSION_NAME.to_owned());
        return Ok(AddArgs {
            target: String::new(),
            label: label.unwrap_or_else(|| alias.clone()),
            session,
            options,
            from_config,
        });
    }
    let target = target.ok_or_else(|| t.machine_add_usage.to_owned())?;
    let label = label.ok_or_else(|| t.label_required.to_owned())?;
    let session = session.unwrap_or_else(|| crate::session::DEFAULT_SESSION_NAME.to_owned());
    Ok(AddArgs {
        target,
        label,
        session,
        options,
        from_config,
    })
}

fn has_connection_options(options: &SshProfileOptions) -> bool {
    options.port.is_some()
        || options.user.is_some()
        || !options.identity_file.is_empty()
        || options.identities_only.is_some()
        || options.identity_agent.is_some()
        || options.strict_host_key_checking.is_some()
        || !options.proxy_jump.is_empty()
        || options.forward_agent.is_some()
        || options.server_alive_interval.is_some()
        || options.server_alive_count_max.is_some()
        || options.control_persist.is_some()
        || options.remote_command.is_some()
}

fn set_once<T>(slot: &mut Option<T>, value: T, option: &str) -> Result<(), String> {
    if slot.is_some() {
        return Err(machine_option_specified_twice(option));
    }
    *slot = Some(value);
    Ok(())
}

fn invalid_flag_value(flag: &str, value: &str) -> String {
    crate::i18n::fill(
        crate::i18n::texts().cli_errors.invalid_flag_value_fmt,
        &[("flag", flag), ("value", value)],
    )
}

fn apply_add_option(
    label: &mut Option<String>,
    session: &mut Option<String>,
    options: &mut SshProfileOptions,
    from_config: &mut Option<String>,
    flag: &str,
    value: &str,
) -> Result<(), String> {
    let t = &crate::i18n::texts().cli_errors;
    match flag {
        "--label" => {
            if label.is_some() {
                return Err(t.label_specified_twice.into());
            }
            *label = Some(value.to_owned());
        }
        "--remote-session" => {
            if session.is_some() {
                return Err(t.remote_session_specified_twice.into());
            }
            *session = Some(value.to_owned());
        }
        "--from-config" => set_once(from_config, value.to_owned(), flag)?,
        "--group" => set_once(&mut options.group, value.to_owned(), flag)?,
        "--tag" => options.tags.push(value.to_owned()),
        "--color" => set_once(&mut options.color, value.to_owned(), flag)?,
        "--port" => set_once(
            &mut options.port,
            value
                .parse::<u16>()
                .map_err(|_| invalid_flag_value(flag, value))?,
            flag,
        )?,
        "--user" => set_once(&mut options.user, value.to_owned(), flag)?,
        "--identity-file" => options.identity_file.push(value.to_owned()),
        "--identity-agent" => set_once(&mut options.identity_agent, value.to_owned(), flag)?,
        "--strict-host-key-checking" => set_once(
            &mut options.strict_host_key_checking,
            match value {
                "ask" => StrictHostKeyChecking::Ask,
                "accept-new" => StrictHostKeyChecking::AcceptNew,
                "yes" => StrictHostKeyChecking::Yes,
                _ => return Err(invalid_flag_value(flag, value)),
            },
            flag,
        )?,
        "--proxy-jump" => {
            let hop = match value.strip_prefix("profile:") {
                Some(id) => ProxyJumpHop::Profile(
                    ProfileId::parse(id).map_err(|_| invalid_flag_value(flag, value))?,
                ),
                None => ProxyJumpHop::Target(value.to_owned()),
            };
            options.proxy_jump.push(hop);
        }
        "--server-alive-interval" => set_once(
            &mut options.server_alive_interval,
            value
                .parse::<u16>()
                .map_err(|_| invalid_flag_value(flag, value))?,
            flag,
        )?,
        "--server-alive-count-max" => set_once(
            &mut options.server_alive_count_max,
            value
                .parse::<u16>()
                .map_err(|_| invalid_flag_value(flag, value))?,
            flag,
        )?,
        "--control-persist" => set_once(&mut options.control_persist, value.to_owned(), flag)?,
        "--remote-command" => set_once(&mut options.remote_command, value.to_owned(), flag)?,
        _ => unreachable!("validated machine add value flag"),
    }
    Ok(())
}

fn add(args: &[String]) -> std::io::Result<i32> {
    let t = &crate::i18n::texts().cli_errors;
    let AddArgs {
        target,
        label,
        session,
        options,
        from_config,
    } = match parse_add_args(args) {
        Ok(args) => args,
        Err(error) => {
            eprintln!("{error}");
            return Ok(2);
        }
    };
    let (target, options) = match from_config {
        Some(alias) => match resolve_host_from_config(&alias, options) {
            Ok(resolved) => resolved,
            Err(error) => {
                eprintln!("{}{error}", t.error_prefix);
                return Ok(2);
            }
        },
        None => (target, options),
    };
    save_prepared_machine(&target, label, session, options)
}

/// Resolves `--from-config <host>` against the user's SSH config: the alias
/// becomes the label default, the resolved `HostName` the target, and the
/// remaining extracted directives the connection options. Organization
/// metadata flags (`--group`/`--tag`/`--color`) merge over the config values.
/// Returns the target and the merged options.
fn resolve_host_from_config(
    alias: &str,
    mut options: SshProfileOptions,
) -> Result<(String, SshProfileOptions), String> {
    let t = &crate::i18n::texts().cli_errors;
    let path = crate::platform::remote_ssh_config_paths()
        .user_config
        .ok_or_else(|| t.machine_add_from_config_no_config.to_string())?;
    let config = crate::remote::SshConfig::load(&path).map_err(|error| {
        crate::i18n::fill(
            t.machine_import_config_read_failed_fmt,
            &[
                ("path", &path.display().to_string()),
                ("error", &error.to_string()),
            ],
        )
    })?;
    let Some(candidate) = config.import_candidate_named(alias) else {
        return Err(crate::i18n::fill(
            t.machine_add_from_config_not_found_fmt,
            &[("alias", alias), ("path", &path.display().to_string())],
        ));
    };
    if candidate.wildcard {
        return Err(crate::i18n::fill(
            t.machine_add_from_config_wildcard_fmt,
            &[("label", &candidate.label)],
        ));
    }
    for warning in config.warnings() {
        eprintln!(
            "{}",
            crate::i18n::fill(
                t.machine_import_config_warning_fmt,
                &[
                    ("origin", &warning.origin.display().to_string()),
                    ("line", &warning.line.to_string()),
                    ("message", &warning.message),
                ]
            )
        );
    }
    for note in &candidate.notes {
        eprintln!(
            "{}",
            crate::i18n::fill(
                crate::i18n::texts().cli_errors.machine_config_note_fmt,
                &[("note", note)]
            )
        );
    }
    options.port = candidate.options.port;
    options.user = candidate.options.user;
    options.identity_file = candidate.options.identity_file;
    options.identities_only = candidate.options.identities_only;
    options.identity_agent = candidate.options.identity_agent;
    options.strict_host_key_checking = candidate.options.strict_host_key_checking;
    options.proxy_jump = candidate.options.proxy_jump;
    options.forward_agent = candidate.options.forward_agent;
    options.server_alive_interval = candidate.options.server_alive_interval;
    options.server_alive_count_max = candidate.options.server_alive_count_max;
    options.control_persist = candidate.options.control_persist;
    options.remote_command = candidate.options.remote_command;
    Ok((candidate.target, options))
}

fn save_prepared_machine(
    target: &str,
    label: String,
    session: String,
    options: SshProfileOptions,
) -> std::io::Result<i32> {
    let t = &crate::i18n::texts().cli_errors;
    let mut catalog = load_catalog()?;
    let profile_options =
        match catalog.add_ssh_with_options(label.clone(), target, session.clone(), options.clone())
        {
            Ok(id) => {
                let profile = catalog
                    .ssh
                    .iter()
                    .find(|profile| profile.id == id)
                    .expect("freshly added profile");
                match crate::remote::ProfileSshOptions::from_profile(profile, &catalog.ssh) {
                    Ok(options) => options,
                    Err(error) => {
                        eprintln!("{}{error}", t.error_prefix);
                        return Ok(2);
                    }
                }
            }
            Err(error) => {
                eprintln!("{}{error}", t.error_prefix);
                return Ok(2);
            }
        };
    let metadata =
        match crate::remote::prepare_saved_ssh(target, &session, profile_options.as_ref()) {
            Ok(metadata) => metadata,
            Err(error) => {
                eprintln!(
                    "{}",
                    crate::i18n::fill(t.machine_not_saved_fmt, &[("error", &error.to_string())])
                );
                crate::remote::print_saved_ssh_error_hint(&error, target);
                return Ok(1);
            }
        };
    // Setup can wait for human approval. Do not overwrite catalog edits made meanwhile.
    let mut catalog = load_catalog().map_err(|error| {
        std::io::Error::other(crate::i18n::fill(
            t.machine_prepared_not_saved_fmt,
            &[("error", &error.to_string())],
        ))
    })?;
    let id = match catalog.add_ssh_with_options(label, target, session.clone(), options) {
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
    // 预热远端 herdr 路径缓存：之后 `--machine` 形态的 CLI 不必每次重新探测。
    if let Some(metadata) = metadata {
        crate::client::endpoint::SshMetadataCache::new(id.as_str(), target, &session)?
            .store(&metadata);
    }
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
    let metadata_cache = catalog
        .ssh
        .iter()
        .find(|profile| profile.id == id)
        .map(|profile| {
            crate::client::endpoint::SshMetadataCache::new(
                id.as_str(),
                &profile.target,
                &profile.session,
            )
        })
        .transpose()?;
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
    if let Some(cache) = metadata_cache {
        cache.invalidate();
    }
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

fn forward(args: &[String]) -> std::io::Result<i32> {
    match args.first().map(String::as_str) {
        Some("list") => forward_list(&args[1..]),
        Some("add") => forward_add(&args[1..]),
        Some("remove") => forward_remove(&args[1..]),
        Some("help" | "--help" | "-h") => {
            eprintln!("{}", crate::i18n::texts().cli_errors.machine_forward_usage);
            Ok(0)
        }
        _ => {
            eprintln!("{}", crate::i18n::texts().cli_errors.machine_forward_usage);
            Ok(2)
        }
    }
}

fn find_profile<'a>(
    catalog: &'a EndpointCatalog,
    id: &ProfileId,
) -> Option<&'a crate::client::endpoint::SavedSshEndpoint> {
    catalog.ssh.iter().find(|profile| &profile.id == id)
}

fn parse_forward_profile(raw: &str) -> Option<ProfileId> {
    match ProfileId::parse(raw.to_owned()) {
        Ok(id) => Some(id),
        Err(error) => {
            eprintln!("{}{error}", crate::i18n::texts().cli_errors.error_prefix);
            None
        }
    }
}

pub(super) fn forward_rule_text(rule: &PortForwardRule) -> String {
    let listen = match &rule.bind_address {
        Some(bind) => format!("{bind}:{}", rule.listen_port),
        None => rule.listen_port.to_string(),
    };
    match rule.kind {
        PortForwardKind::Local | PortForwardKind::Remote => format!(
            "{listen} -> {}:{}",
            rule.target_host.as_deref().unwrap_or_default(),
            rule.target_port.unwrap_or_default()
        ),
        PortForwardKind::Dynamic => format!("{listen} (SOCKS)"),
    }
}

fn forward_list(args: &[String]) -> std::io::Result<i32> {
    let (raw_id, json) = match args {
        [raw_id] => (raw_id, false),
        [raw_id, flag] if flag == "--json" => (raw_id, true),
        _ => {
            eprintln!("{}", crate::i18n::texts().cli_errors.machine_forward_usage);
            return Ok(2);
        }
    };
    let Some(id) = parse_forward_profile(raw_id) else {
        return Ok(2);
    };
    let catalog = load_catalog()?;
    let Some(profile) = find_profile(&catalog, &id) else {
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
    };
    let rows = profile
        .port_forwards
        .iter()
        .enumerate()
        .map(|(index, rule)| ForwardListRow {
            number: index + 1,
            rule,
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
        println!("{}", crate::i18n::texts().cli_output.machine_forward_none);
        return Ok(0);
    }
    for row in rows {
        println!(
            "{}\t{}\t{}",
            row.number,
            row.rule.kind.as_str(),
            forward_rule_text(row.rule)
        );
    }
    Ok(0)
}

#[derive(Debug, PartialEq, Eq)]
struct ForwardAddArgs {
    profile: ProfileId,
    rule: PortForwardRule,
}

const FORWARD_ADD_VALUE_FLAGS: &[&str] = &[
    "--kind",
    "--listen-port",
    "--bind-address",
    "--target-host",
    "--target-port",
];

fn parse_forward_add_args(args: &[String]) -> Result<ForwardAddArgs, String> {
    let t = &crate::i18n::texts().cli_errors;
    let args = super::expand_equals_args(args, FORWARD_ADD_VALUE_FLAGS);
    let mut profile = None;
    let mut kind = None;
    let mut listen_port = None;
    let mut bind_address = None;
    let mut target_host = None;
    let mut target_port = None;
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        if FORWARD_ADD_VALUE_FLAGS.contains(&arg) {
            let Some(value) = args.get(index + 1) else {
                return Err(crate::i18n::fill(t.missing_value_for_fmt, &[("flag", arg)]));
            };
            match arg {
                "--kind" => {
                    if kind.is_some() {
                        return Err(machine_option_specified_twice(arg));
                    }
                    kind = Some(match value.as_str() {
                        "local" => PortForwardKind::Local,
                        "remote" => PortForwardKind::Remote,
                        "dynamic" => PortForwardKind::Dynamic,
                        _ => {
                            return Err(crate::i18n::fill(
                                t.machine_forward_kind_invalid_fmt,
                                &[("value", value)],
                            ))
                        }
                    });
                }
                "--listen-port" => {
                    if listen_port.is_some() {
                        return Err(machine_option_specified_twice(arg));
                    }
                    listen_port = Some(
                        value
                            .parse::<u16>()
                            .map_err(|_| invalid_flag_value(arg, value))?,
                    );
                }
                "--bind-address" => {
                    if bind_address.is_some() {
                        return Err(machine_option_specified_twice(arg));
                    }
                    bind_address = Some(value.clone());
                }
                "--target-host" => {
                    if target_host.is_some() {
                        return Err(machine_option_specified_twice(arg));
                    }
                    target_host = Some(value.clone());
                }
                "--target-port" => {
                    if target_port.is_some() {
                        return Err(machine_option_specified_twice(arg));
                    }
                    target_port = Some(
                        value
                            .parse::<u16>()
                            .map_err(|_| invalid_flag_value(arg, value))?,
                    );
                }
                _ => unreachable!("validated forward add value flag"),
            }
            index += 2;
            continue;
        }
        match arg {
            positional if !positional.starts_with('-') && profile.is_none() => {
                profile = Some(ProfileId::parse(positional.to_owned()).map_err(|error| {
                    crate::i18n::fill(
                        t.machine_forward_invalid_profile_fmt,
                        &[("error", &error.to_string())],
                    )
                })?);
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
    let Some(profile) = profile else {
        return Err(t.machine_forward_usage.into());
    };
    let Some(kind) = kind else {
        return Err(t.machine_forward_kind_required.into());
    };
    let Some(listen_port) = listen_port else {
        return Err(t.machine_forward_listen_port_required.into());
    };
    let rule = PortForwardRule {
        kind,
        bind_address,
        listen_port,
        target_host,
        target_port,
    };
    Ok(ForwardAddArgs { profile, rule })
}

fn forward_add(args: &[String]) -> std::io::Result<i32> {
    let ForwardAddArgs { profile, rule } = match parse_forward_add_args(args) {
        Ok(parsed) => parsed,
        Err(error) => {
            eprintln!("{}{error}", crate::i18n::texts().cli_errors.error_prefix);
            return Ok(2);
        }
    };
    let mut catalog = load_catalog()?;
    let Some(existing) = find_profile(&catalog, &profile) else {
        eprintln!(
            "{}",
            crate::i18n::fill(
                crate::i18n::texts()
                    .cli_errors
                    .machine_profile_not_found_fmt,
                &[("id", profile.as_str())]
            )
        );
        return Ok(1);
    };
    let mut rules = existing.port_forwards.clone();
    rules.push(rule);
    match catalog.set_port_forwards(&profile, rules) {
        Ok(true) => {}
        Ok(false) => {
            eprintln!(
                "{}",
                crate::i18n::fill(
                    crate::i18n::texts()
                        .cli_errors
                        .machine_profile_not_found_fmt,
                    &[("id", profile.as_str())]
                )
            );
            return Ok(1);
        }
        Err(error) => {
            eprintln!("{}{error}", crate::i18n::texts().cli_errors.error_prefix);
            return Ok(2);
        }
    }
    let added = catalog
        .ssh
        .iter()
        .find(|entry| entry.id == profile)
        .and_then(|entry| entry.port_forwards.last().cloned());
    store_catalog(&catalog)?;
    let t = &crate::i18n::texts().cli_output;
    match added {
        Some(rule) => println!(
            "{}",
            crate::i18n::fill(
                t.machine_forward_added_fmt,
                &[
                    ("kind", rule.kind.as_str()),
                    ("rule", &forward_rule_text(&rule)),
                    ("id", profile.as_str()),
                ]
            )
        ),
        None => println!(
            "{}",
            crate::i18n::fill(t.machine_forward_updated_fmt, &[("id", profile.as_str())])
        ),
    }
    Ok(0)
}

fn forward_remove(args: &[String]) -> std::io::Result<i32> {
    let [raw_id, raw_number] = args else {
        eprintln!("{}", crate::i18n::texts().cli_errors.machine_forward_usage);
        return Ok(2);
    };
    let Some(id) = parse_forward_profile(raw_id) else {
        return Ok(2);
    };
    let number = match raw_number.parse::<usize>() {
        Ok(number) if number > 0 => number,
        _ => {
            eprintln!(
                "{}{}",
                crate::i18n::texts().cli_errors.error_prefix,
                crate::i18n::fill(
                    crate::i18n::texts()
                        .cli_errors
                        .machine_forward_rule_number_invalid_fmt,
                    &[("number", raw_number)]
                )
            );
            return Ok(2);
        }
    };
    let mut catalog = load_catalog()?;
    let Some(existing) = find_profile(&catalog, &id) else {
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
    };
    if number > existing.port_forwards.len() {
        eprintln!(
            "{}{}",
            crate::i18n::texts().cli_errors.error_prefix,
            crate::i18n::fill(
                crate::i18n::texts()
                    .cli_errors
                    .machine_forward_rule_number_unknown_fmt,
                &[
                    ("id", id.as_str()),
                    ("count", &existing.port_forwards.len().to_string()),
                    ("number", &number.to_string()),
                ]
            )
        );
        return Ok(1);
    }
    let mut rules = existing.port_forwards.clone();
    let removed = rules.remove(number - 1);
    match catalog.set_port_forwards(&id, rules) {
        Ok(true) => {}
        Ok(false) => {
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
        Err(error) => {
            eprintln!("{}{error}", crate::i18n::texts().cli_errors.error_prefix);
            return Ok(2);
        }
    }
    store_catalog(&catalog)?;
    println!(
        "{}",
        crate::i18n::fill(
            crate::i18n::texts().cli_output.machine_forward_removed_fmt,
            &[
                ("kind", removed.kind.as_str()),
                ("rule", &forward_rule_text(&removed)),
                ("id", id.as_str()),
            ]
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

const IMPORT_VALUE_FLAGS: &[&str] = &["--file", "--host", "--group"];

#[derive(Debug, Default, PartialEq, Eq)]
struct ImportArgs {
    file: Option<String>,
    hosts: Vec<String>,
    yes: bool,
    group: Option<String>,
    include_wildcards: bool,
}

fn parse_import_args(args: &[String]) -> Result<ImportArgs, String> {
    let t = &crate::i18n::texts().cli_errors;
    let args = super::expand_equals_args(args, IMPORT_VALUE_FLAGS);
    let mut parsed = ImportArgs::default();
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        if IMPORT_VALUE_FLAGS.contains(&arg) {
            let Some(value) = args.get(index + 1) else {
                return Err(crate::i18n::fill(t.missing_value_for_fmt, &[("flag", arg)]));
            };
            match arg {
                "--file" => set_once(&mut parsed.file, value.clone(), arg)?,
                "--host" => parsed.hosts.push(value.clone()),
                "--group" => set_once(&mut parsed.group, value.clone(), arg)?,
                _ => unreachable!("validated machine import value flag"),
            }
            index += 2;
            continue;
        }
        match arg {
            "--yes" | "-y" => {
                if parsed.yes {
                    return Err(machine_option_specified_twice("--yes"));
                }
                parsed.yes = true;
                index += 1;
            }
            "--include-wildcards" => {
                if parsed.include_wildcards {
                    return Err(machine_option_specified_twice("--include-wildcards"));
                }
                parsed.include_wildcards = true;
                index += 1;
            }
            unknown => {
                return Err(crate::i18n::fill(
                    t.unknown_option_fmt,
                    &[("option", unknown)],
                ));
            }
        }
    }
    Ok(parsed)
}

fn import(args: &[String]) -> std::io::Result<i32> {
    let parsed = match parse_import_args(args) {
        Ok(parsed) => parsed,
        Err(error) => {
            eprintln!("{}{error}", crate::i18n::texts().cli_errors.error_prefix);
            eprintln!("{}", crate::i18n::texts().cli_errors.machine_import_usage);
            return Ok(2);
        }
    };
    let path = match &parsed.file {
        Some(file) => crate::worktree::expand_tilde_path(file),
        None => match crate::platform::remote_ssh_config_paths().user_config {
            Some(path) => path,
            None => {
                eprintln!(
                    "{}{}",
                    crate::i18n::texts().cli_errors.error_prefix,
                    crate::i18n::texts()
                        .cli_errors
                        .machine_import_no_config_path
                );
                return Ok(2);
            }
        },
    };
    let config = match crate::remote::SshConfig::load(&path) {
        Ok(config) => config,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            eprintln!(
                "{}",
                crate::i18n::fill(
                    crate::i18n::texts()
                        .cli_errors
                        .machine_import_config_not_found_fmt,
                    &[("path", &path.display().to_string())]
                )
            );
            return Ok(1);
        }
        Err(error) => {
            eprintln!(
                "{}{}",
                crate::i18n::texts().cli_errors.error_prefix,
                crate::i18n::fill(
                    crate::i18n::texts()
                        .cli_errors
                        .machine_import_config_read_failed_fmt,
                    &[
                        ("path", &path.display().to_string()),
                        ("error", &error.to_string())
                    ]
                )
            );
            return Ok(1);
        }
    };
    for warning in config.warnings() {
        eprintln!(
            "{}",
            crate::i18n::fill(
                crate::i18n::texts()
                    .cli_errors
                    .machine_import_config_warning_fmt,
                &[
                    ("origin", &warning.origin.display().to_string()),
                    ("line", &warning.line.to_string()),
                    ("message", &warning.message),
                ]
            )
        );
    }
    let mut candidates = config.import_candidates();
    if !parsed.hosts.is_empty() {
        candidates.retain(|candidate| {
            parsed
                .hosts
                .iter()
                .any(|pattern| host_filter_matches(pattern, &candidate.label))
        });
        if candidates.is_empty() {
            eprintln!(
                "{}",
                crate::i18n::fill(
                    crate::i18n::texts()
                        .cli_errors
                        .machine_import_no_matching_hosts_fmt,
                    &[("path", &path.display().to_string())]
                )
            );
            return Ok(1);
        }
    }
    if candidates.is_empty() {
        eprintln!(
            "{}",
            crate::i18n::fill(
                crate::i18n::texts().cli_errors.machine_import_no_hosts_fmt,
                &[("path", &path.display().to_string())]
            )
        );
        return Ok(1);
    }
    let mut catalog = load_catalog()?;
    let plan = crate::remote::plan_import(candidates, &catalog.ssh, parsed.include_wildcards);
    let out = &crate::i18n::texts().cli_output;
    for skip in &plan.skipped {
        println!(
            "{}",
            crate::i18n::fill(
                out.machine_import_skipped_fmt,
                &[("label", &skip.label), ("reason", &skip.reason)]
            )
        );
    }
    if plan.ready.is_empty() {
        println!(
            "{}",
            crate::i18n::fill(
                out.machine_import_summary_fmt,
                &[
                    ("imported", "0"),
                    ("skipped", &plan.skipped.len().to_string()),
                    ("failed", "0"),
                ]
            )
        );
        return Ok(0);
    }
    let selected = if parsed.yes {
        vec![true; plan.ready.len()]
    } else if std::io::stdin().is_terminal() {
        match prompt_import_selection(&path, &plan)? {
            Some(selected) => selected,
            None => {
                eprintln!(
                    "{}",
                    crate::i18n::texts().cli_errors.machine_import_cancelled
                );
                return Ok(1);
            }
        }
    } else {
        eprintln!(
            "{}{}",
            crate::i18n::texts().cli_errors.error_prefix,
            crate::i18n::texts()
                .cli_errors
                .machine_import_not_a_terminal
        );
        return Ok(2);
    };
    let mut ids: Vec<Option<ProfileId>> = vec![None; plan.ready.len()];
    let mut imported = 0;
    let mut failed = 0;
    for (index, planned) in plan.ready.iter().enumerate() {
        if !selected[index] {
            continue;
        }
        let mut options = planned.options_with_resolved_hops(&ids);
        if let Some(group) = &parsed.group {
            options.group = Some(group.clone());
        }
        match catalog.add_ssh_with_options(
            &planned.label,
            &planned.target,
            crate::session::DEFAULT_SESSION_NAME,
            options,
        ) {
            Ok(id) => {
                ids[index] = Some(id.clone());
                imported += 1;
                println!(
                    "{}",
                    crate::i18n::fill(
                        out.machine_imported_fmt,
                        &[("label", &planned.label), ("id", id.as_str())]
                    )
                );
                for note in &planned.notes {
                    println!(
                        "{}",
                        crate::i18n::fill(out.machine_import_note_fmt, &[("note", note)])
                    );
                }
            }
            Err(error) => {
                failed += 1;
                eprintln!(
                    "{}",
                    crate::i18n::fill(
                        crate::i18n::texts().cli_errors.machine_import_failed_fmt,
                        &[("label", &planned.label), ("error", &error)]
                    )
                );
            }
        }
    }
    if imported > 0 {
        store_catalog(&catalog)?;
    }
    println!(
        "{}",
        crate::i18n::fill(
            out.machine_import_summary_fmt,
            &[
                ("imported", &imported.to_string()),
                ("skipped", &plan.skipped.len().to_string()),
                ("failed", &failed.to_string()),
            ]
        )
    );
    if imported > 0 {
        println!("{}", out.machine_import_connect_note);
    }
    Ok(if failed > 0 { 1 } else { 0 })
}

/// Interactive host picker: lists the importable hosts and reads a selection
/// of 1-based numbers and ranges (`1,3-5`), `all`, or `none`; an empty answer
/// imports everything. Returns `None` when stdin reaches EOF (cancelled).
fn prompt_import_selection(
    path: &std::path::Path,
    plan: &crate::remote::ImportPlan,
) -> std::io::Result<Option<Vec<bool>>> {
    let out = &crate::i18n::texts().cli_output;
    println!(
        "{}",
        crate::i18n::fill(
            out.machine_import_found_fmt,
            &[
                ("count", &plan.ready.len().to_string()),
                ("path", &path.display().to_string())
            ]
        )
    );
    for (index, planned) in plan.ready.iter().enumerate() {
        println!(
            "  {}.\t{}\t{}",
            index + 1,
            planned.label,
            planned_summary(planned)
        );
    }
    loop {
        eprint!("{}", out.machine_import_prompt);
        std::io::stderr().flush()?;
        let mut answer = String::new();
        if std::io::stdin().read_line(&mut answer)? == 0 {
            return Ok(None);
        }
        match parse_import_selection(&answer, plan.ready.len()) {
            Ok(indices) => {
                let mut selected = vec![false; plan.ready.len()];
                for index in indices {
                    selected[index] = true;
                }
                return Ok(Some(selected));
            }
            Err(message) => eprintln!("{message}"),
        }
    }
}

fn planned_summary(planned: &crate::remote::PlannedImport) -> String {
    let mut summary = String::new();
    if let Some(user) = &planned.options.user {
        summary.push_str(user);
        summary.push('@');
    }
    summary.push_str(&planned.target);
    if let Some(port) = planned.options.port {
        summary.push_str(&format!(":{port}"));
    }
    summary
}

fn parse_import_selection(input: &str, count: usize) -> Result<Vec<usize>, String> {
    let t = &crate::i18n::texts().cli_errors;
    let trimmed = input.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("all") {
        return Ok((0..count).collect());
    }
    if trimmed.eq_ignore_ascii_case("none") {
        return Ok(Vec::new());
    }
    let mut selected = Vec::new();
    for part in trimmed.split(',') {
        let part = part.trim();
        if part.is_empty() {
            return Err(t.machine_import_selection_empty.into());
        }
        let (start, end) = match part.split_once('-') {
            Some((start, end)) => (
                parse_selection_index(start, count, part)?,
                parse_selection_index(end, count, part)?,
            ),
            None => {
                let index = parse_selection_index(part, count, part)?;
                (index, index)
            }
        };
        if start > end {
            return Err(crate::i18n::fill(
                t.machine_import_selection_reversed_fmt,
                &[("part", part)],
            ));
        }
        selected.extend(start..=end);
    }
    selected.sort_unstable();
    selected.dedup();
    Ok(selected)
}

fn parse_selection_index(text: &str, count: usize, part: &str) -> Result<usize, String> {
    match text.trim().parse::<usize>() {
        Ok(number) if (1..=count).contains(&number) => Ok(number - 1),
        _ => Err(crate::i18n::fill(
            crate::i18n::texts()
                .cli_errors
                .machine_import_selection_range_fmt,
            &[("part", part), ("count", &count.to_string())],
        )),
    }
}

/// `--host` filters match an alias exactly (case-insensitively) or, when the
/// pattern contains `*`/`?`, as an OpenSSH-style host pattern.
fn host_filter_matches(pattern: &str, alias: &str) -> bool {
    if pattern.contains('*') || pattern.contains('?') {
        return crate::remote::ssh_host_pattern_matches(pattern, alias);
    }
    pattern.eq_ignore_ascii_case(alias)
}

pub(super) fn load_catalog() -> std::io::Result<EndpointCatalog> {
    EndpointCatalog::load().map_err(std::io::Error::other)
}

fn store_catalog(catalog: &EndpointCatalog) -> std::io::Result<()> {
    catalog.store_profiles().map_err(std::io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_add_args(session: &str) -> AddArgs {
        AddArgs {
            target: "workstation.coder".into(),
            label: "coder".into(),
            session: session.into(),
            options: SshProfileOptions::default(),
            from_config: None,
        }
    }

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
                base_add_args(session),
                "{args:?}"
            );
        }
    }

    #[test]
    fn add_parser_collects_profile_options() {
        let args = [
            "build.example",
            "--label",
            "build",
            "--group=prod",
            "--tag",
            "ci",
            "--tag=linux",
            "--color",
            "#a1b2c3",
            "--port",
            "2222",
            "--user=dev",
            "--identity-file",
            "~/.ssh/build key",
            "--identity-file",
            "~/.ssh/fallback",
            "--identities-only",
            "--identity-agent=~/.ssh/agent.sock",
            "--strict-host-key-checking",
            "accept-new",
            "--proxy-jump",
            "bastion",
            "--proxy-jump=profile:0123456789abcdef0123456789abcdef",
            "--forward-agent",
            "--server-alive-interval=30",
            "--server-alive-count-max",
            "2",
            "--control-persist",
            "10m",
            "--remote-command=tmux attach",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
        let parsed = parse_add_args(&args).unwrap();
        assert_eq!(parsed.target, "build.example");
        assert_eq!(parsed.label, "build");
        assert_eq!(parsed.session, "default");
        assert_eq!(
            parsed.options,
            SshProfileOptions {
                group: Some("prod".into()),
                tags: vec!["ci".into(), "linux".into()],
                color: Some("#a1b2c3".into()),
                port: Some(2222),
                user: Some("dev".into()),
                identity_file: vec!["~/.ssh/build key".into(), "~/.ssh/fallback".into()],
                identities_only: Some(true),
                identity_agent: Some("~/.ssh/agent.sock".into()),
                strict_host_key_checking: Some(StrictHostKeyChecking::AcceptNew),
                proxy_jump: vec![
                    ProxyJumpHop::Target("bastion".into()),
                    ProxyJumpHop::Profile(
                        ProfileId::parse("0123456789abcdef0123456789abcdef").unwrap()
                    ),
                ],
                forward_agent: Some(true),
                server_alive_interval: Some(30),
                server_alive_count_max: Some(2),
                control_persist: Some("10m".into()),
                remote_command: Some("tmux attach".into()),
                session_log: None,
            }
        );
    }

    #[test]
    fn add_parser_rejects_bad_option_values_and_duplicates() {
        for args in [
            vec!["host", "--label", "a", "--port", "not-a-number"],
            vec!["host", "--label", "a", "--port", "99999"],
            vec![
                "host",
                "--label",
                "a",
                "--strict-host-key-checking",
                "maybe",
            ],
            vec!["host", "--label", "a", "--proxy-jump", "profile:not-hex"],
            vec!["host", "--label", "a", "--group", "one", "--group", "two"],
            vec![
                "host",
                "--label",
                "a",
                "--identities-only",
                "--identities-only",
            ],
            vec!["host", "--label", "a", "--port"],
            vec!["host", "--label", "a", "--tag"],
            vec!["host", "--label", "a", "--identities-only=yes"],
        ] {
            let args = args.into_iter().map(str::to_owned).collect::<Vec<_>>();
            assert!(parse_add_args(&args).is_err(), "{args:?}");
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
    fn add_parser_from_config_defaults_label_and_skips_target_requirement() {
        for args in [
            vec!["--from-config", "web"],
            vec!["--from-config=web"],
            vec!["--from-config", "web", "--label", "Web", "--group", "prod"],
        ] {
            let args = args.into_iter().map(str::to_owned).collect::<Vec<_>>();
            let parsed = parse_add_args(&args).unwrap();
            assert_eq!(parsed.from_config.as_deref(), Some("web"), "{args:?}");
            assert_eq!(parsed.target, "");
            assert_eq!(parsed.session, "default");
            assert!(!has_connection_options(&parsed.options));
        }
        let args = vec!["--from-config".to_owned(), "web".to_owned()];
        assert_eq!(parse_add_args(&args).unwrap().label, "web");
        let args = vec![
            "--from-config".to_owned(),
            "web".to_owned(),
            "--label".to_owned(),
            "Web".to_owned(),
        ];
        assert_eq!(parse_add_args(&args).unwrap().label, "Web");
    }

    #[test]
    fn add_parser_from_config_rejects_conflicting_arguments() {
        for args in [
            vec!["--from-config", "web", "positional-target"],
            vec!["--from-config", "web", "--user", "dev"],
            vec!["--from-config", "web", "--port", "2222"],
            vec!["--from-config", "web", "--identity-file", "~/.ssh/id"],
            vec!["--from-config", "web", "--identities-only"],
            vec!["--from-config", "web", "--proxy-jump", "bastion"],
            vec!["--from-config", "web", "--remote-command", "tmux"],
            vec!["--from-config", "one", "--from-config", "two"],
            vec!["--from-config"],
        ] {
            let args = args.into_iter().map(str::to_owned).collect::<Vec<_>>();
            assert!(parse_add_args(&args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn import_parser_collects_all_flags() {
        let args = [
            "--file",
            "~/elsewhere/config",
            "--host",
            "web",
            "--host=db-*",
            "--yes",
            "--group",
            "prod",
            "--include-wildcards",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
        assert_eq!(
            parse_import_args(&args).unwrap(),
            ImportArgs {
                file: Some("~/elsewhere/config".into()),
                hosts: vec!["web".into(), "db-*".into()],
                yes: true,
                group: Some("prod".into()),
                include_wildcards: true,
            }
        );
        assert_eq!(parse_import_args(&[]).unwrap(), ImportArgs::default());
        let short_yes = vec!["-y".to_owned()];
        assert!(parse_import_args(&short_yes).unwrap().yes);
    }

    #[test]
    fn import_parser_rejects_bad_arguments() {
        for args in [
            vec!["--file"],
            vec!["--file", "a", "--file", "b"],
            vec!["--yes", "--yes"],
            vec!["--group", "one", "--group", "two"],
            vec!["--host"],
            vec!["--bogus"],
            vec!["positional"],
        ] {
            let args = args.into_iter().map(str::to_owned).collect::<Vec<_>>();
            assert!(parse_import_args(&args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn import_selection_parses_all_none_and_ranges() {
        assert_eq!(parse_import_selection("", 3).unwrap(), vec![0, 1, 2]);
        assert_eq!(parse_import_selection("all", 3).unwrap(), vec![0, 1, 2]);
        assert_eq!(
            parse_import_selection("none", 3).unwrap(),
            Vec::<usize>::new()
        );
        assert_eq!(parse_import_selection("1,3", 3).unwrap(), vec![0, 2]);
        assert_eq!(parse_import_selection("2-3", 4).unwrap(), vec![1, 2]);
        assert_eq!(parse_import_selection(" 1 , 1-2 ", 4).unwrap(), vec![0, 1]);
        for bad in ["0", "4", "2-9", "x", "3-1", "1-", ",", "1,,2"] {
            assert!(parse_import_selection(bad, 3).is_err(), "{bad}");
        }
    }

    #[test]
    fn host_filter_matches_exact_or_glob() {
        assert!(host_filter_matches("web", "WEB"));
        assert!(host_filter_matches("db-*", "db-1"));
        assert!(!host_filter_matches("db-*", "web"));
        assert!(!host_filter_matches("web", "web-1"));
    }

    #[test]
    fn forward_add_parser_collects_a_full_rule() {
        let id = ProfileId::generate();
        let args = vec![
            id.as_str(),
            "--kind",
            "local",
            "--listen-port",
            "8080",
            "--bind-address=0.0.0.0",
            "--target-host",
            "127.0.0.1",
            "--target-port=80",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
        assert_eq!(
            parse_forward_add_args(&args).unwrap(),
            ForwardAddArgs {
                profile: id,
                rule: PortForwardRule {
                    kind: PortForwardKind::Local,
                    bind_address: Some("0.0.0.0".into()),
                    listen_port: 8080,
                    target_host: Some("127.0.0.1".into()),
                    target_port: Some(80),
                },
            }
        );
    }

    #[test]
    fn forward_add_parser_rejects_bad_input() {
        let id = ProfileId::generate();
        let id = id.as_str();
        for args in [
            vec![id, "--kind", "sideways", "--listen-port", "80"],
            vec![id, "--kind", "local"],
            vec![id, "--listen-port", "8080"],
            vec![id, "--kind", "local", "--listen-port", "not-a-port"],
            vec![
                id,
                "--kind",
                "local",
                "--kind",
                "remote",
                "--listen-port",
                "8080",
            ],
            vec![id, "--kind", "local", "--listen-port"],
            vec!["not-hex", "--kind", "local", "--listen-port", "8080"],
            vec![id, "--kind", "local", "--listen-port", "8080", "--bogus"],
            vec!["--kind", "local", "--listen-port", "8080"],
        ] {
            let args = args.into_iter().map(str::to_owned).collect::<Vec<_>>();
            assert!(parse_forward_add_args(&args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn forward_rule_text_describes_every_kind() {
        let local = PortForwardRule {
            kind: PortForwardKind::Local,
            bind_address: None,
            listen_port: 8080,
            target_host: Some("127.0.0.1".into()),
            target_port: Some(80),
        };
        assert_eq!(forward_rule_text(&local), "8080 -> 127.0.0.1:80");
        let remote = PortForwardRule {
            kind: PortForwardKind::Remote,
            bind_address: Some("0.0.0.0".into()),
            listen_port: 9000,
            target_host: Some("db.internal".into()),
            target_port: Some(5432),
        };
        assert_eq!(
            forward_rule_text(&remote),
            "0.0.0.0:9000 -> db.internal:5432"
        );
        let dynamic = PortForwardRule {
            kind: PortForwardKind::Dynamic,
            bind_address: None,
            listen_port: 1080,
            target_host: None,
            target_port: None,
        };
        assert_eq!(forward_rule_text(&dynamic), "1080 (SOCKS)");
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
            group: None,
            tags: &[],
            color: None,
            port: None,
            user: None,
            identity_file: &[],
            identities_only: None,
            identity_agent: None,
            strict_host_key_checking: None,
            proxy_jump: &[],
            forward_agent: None,
            server_alive_interval: None,
            server_alive_count_max: None,
            control_persist: None,
            remote_command: None,
            port_forwards: &[],
        })
        .unwrap();
        assert!(!encoded.contains("password"));
        assert!(!encoded.contains("private_key"));
        assert!(!encoded.contains("secret"));
    }
}
