//! `herdr machine status <profile> [--json]`: one saved machine's connection
//! and configuration status in a single report.
//!
//! The connection probe reuses the saved-machine SSH API bridge: reaching the
//! remote server's status endpoint proves the whole chain (ssh transport,
//! remote install, protocol handshake). Failures keep the structured
//! connection classification (`remote::classify_connection_error`) so the
//! operator sees the failure kind, not just stderr text. Port-forward rows
//! are the saved catalog rules — their live phases belong to the running
//! client supervisor and are intentionally out of scope here.
//!
//! The human-readable rows use stable English keys (like `machine log show`)
//! and the `--json` payload mirrors them; both are part of the CLI's stable
//! output surface.

use serde::Serialize;

use crate::api::client::{ApiClient, ConnectionTarget};
use crate::client::endpoint::{EndpointCatalog, SavedSshEndpoint};

fn usage_text() -> &'static str {
    crate::i18n::texts().cli_errors.machine_status_usage
}

#[derive(Serialize)]
struct StatusReport {
    id: String,
    label: String,
    target: String,
    session: String,
    enabled: bool,
    connection: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    remote_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    remote_protocol: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    protocol_compatible: Option<bool>,
    forwards: Vec<String>,
    session_log: SessionLogReport,
}

#[derive(Serialize)]
struct SessionLogReport {
    enabled: bool,
    path_template: String,
    max_bytes: u64,
    dump_interval_secs: u16,
}

pub(super) fn run_machine_status_command(args: &[String]) -> std::io::Result<i32> {
    let (selector, json) = match args {
        [selector] => (selector, false),
        [selector, flag] if flag == "--json" => (selector, true),
        _ => {
            eprintln!("{}", usage_text());
            return Ok(2);
        }
    };
    if matches!(selector.as_str(), "help" | "--help" | "-h") {
        println!("{}", usage_text());
        return Ok(0);
    }
    let profiles = EndpointCatalog::load_profiles().map_err(std::io::Error::other)?;
    let profile = match resolve_for_status(&profiles, selector) {
        Ok(profile) => profile.clone(),
        Err(error) => {
            eprintln!("{}{error}", crate::i18n::texts().cli_errors.error_prefix);
            return Ok(2);
        }
    };
    let report = probe(&profile);
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(std::io::Error::other)?
        );
        return Ok(0);
    }
    print_report(&report);
    Ok(0)
}

/// `machine status` answers questions about disabled machines too, so the
/// selector resolution mirrors `target::resolve_machine` without the
/// enabled-only gate.
fn resolve_for_status<'a>(
    profiles: &'a [SavedSshEndpoint],
    selector: &str,
) -> Result<&'a SavedSshEndpoint, String> {
    let t = &crate::i18n::texts().cli_errors;
    if let Some(profile) = profiles
        .iter()
        .find(|profile| profile.id.as_str() == selector)
    {
        return Ok(profile);
    }
    let mut matches = profiles.iter().filter(|profile| profile.label == selector);
    let profile = matches
        .next()
        .ok_or_else(|| crate::i18n::fill(t.machine_unknown_fmt, &[("selector", selector)]))?;
    if matches.next().is_some() {
        return Err(crate::i18n::fill(
            t.machine_label_ambiguous_fmt,
            &[("selector", selector)],
        ));
    }
    Ok(profile)
}

fn probe(profile: &SavedSshEndpoint) -> StatusReport {
    let config = profile.session_log.clone().unwrap_or_default();
    let session_log = SessionLogReport {
        enabled: config.enabled,
        path_template: config
            .path_template
            .clone()
            .unwrap_or_else(|| crate::client::endpoint::DEFAULT_PATH_TEMPLATE.to_owned()),
        max_bytes: config
            .max_bytes
            .unwrap_or(crate::client::endpoint::DEFAULT_MAX_LOG_BYTES),
        dump_interval_secs: config
            .dump_interval_secs
            .unwrap_or(crate::client::endpoint::DEFAULT_DUMP_INTERVAL_SECS),
    };
    let mut report = StatusReport {
        id: profile.id.to_string(),
        label: profile.label.clone(),
        target: profile.target.clone(),
        session: profile.session.clone(),
        enabled: profile.enabled,
        connection: "disabled",
        error_kind: None,
        error: None,
        remote_version: None,
        remote_protocol: None,
        protocol_compatible: None,
        forwards: profile
            .port_forwards
            .iter()
            .map(super::machine::forward_rule_text)
            .collect(),
        session_log,
    };
    if !profile.enabled {
        return report;
    }
    let probed = (|| -> std::io::Result<(Option<String>, Option<u32>)> {
        let bridge = crate::remote::SavedSshApiBridge::start(profile)?;
        let client = ApiClient::for_target(ConnectionTarget::SocketPath(
            bridge.socket_path().to_owned(),
        ));
        let status = client
            .status()
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        Ok((status.version, status.protocol))
    })();
    match probed {
        Ok((version, protocol)) => {
            report.connection = "online";
            report.remote_version = version;
            report.remote_protocol = protocol;
            report.protocol_compatible =
                protocol.map(|value| value == crate::protocol::PROTOCOL_VERSION);
        }
        Err(error) => {
            report.connection = "unreachable";
            report.error_kind = Some(
                crate::remote::classify_connection_error(&error)
                    .as_str()
                    .to_owned(),
            );
            report.error = Some(error.to_string());
        }
    }
    report
}

fn print_report(report: &StatusReport) {
    println!("id\t{}", report.id);
    println!("label\t{}", report.label);
    println!("target\t{}", report.target);
    println!("session\t{}", report.session);
    println!("enabled\t{}", report.enabled);
    println!("connection\t{}", report.connection);
    if let Some(kind) = report.error_kind.as_deref() {
        println!("error-kind\t{kind}");
    }
    if let Some(error) = report.error.as_deref() {
        println!("error\t{error}");
    }
    if let Some(version) = report.remote_version.as_deref() {
        println!("remote-version\t{version}");
    }
    if let Some(protocol) = report.remote_protocol {
        println!("remote-protocol\t{protocol}");
    }
    if let Some(compatible) = report.protocol_compatible {
        println!("protocol-compatible\t{compatible}");
    }
    println!("forwards\t{}", report.forwards.len());
    for rule in &report.forwards {
        println!("forward\t{rule}");
    }
    println!("session-log\t{}", report.session_log.enabled);
    println!("log-path-template\t{}", report.session_log.path_template);
    println!("log-max-bytes\t{}", report.session_log.max_bytes);
    println!(
        "log-interval-secs\t{}",
        report.session_log.dump_interval_secs
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_machine_reports_without_a_probe() {
        let mut profile =
            SavedSshEndpoint::new("build", "build.example", "agents").expect("valid profile");
        profile.enabled = false;
        let report = probe(&profile);
        assert_eq!(report.connection, "disabled");
        assert!(report.error.is_none());
        assert!(report.remote_version.is_none());
        assert!(!report.session_log.enabled);
        assert_eq!(
            report.session_log.max_bytes,
            crate::client::endpoint::DEFAULT_MAX_LOG_BYTES
        );
    }

    #[test]
    fn status_selector_allows_disabled_but_rejects_unknown_and_ambiguous() {
        let mut disabled = SavedSshEndpoint::new("mac", "mac-ssh", "agents").unwrap();
        disabled.enabled = false;
        let other = SavedSshEndpoint::new("build", "builder", "default").unwrap();
        let profiles = vec![disabled.clone(), other.clone()];
        assert_eq!(resolve_for_status(&profiles, "mac").unwrap(), &disabled);
        assert_eq!(
            resolve_for_status(&profiles, disabled.id.as_str()).unwrap(),
            &disabled
        );
        assert!(resolve_for_status(&profiles, "missing").is_err());
        let duplicate = SavedSshEndpoint::new("mac", "other", "default").unwrap();
        assert!(resolve_for_status(&[disabled.clone(), duplicate], "mac").is_err());
    }

    #[test]
    fn session_log_report_mirrors_the_profile_config() {
        let mut profile =
            SavedSshEndpoint::new("build", "build.example", "agents").expect("valid profile");
        profile.enabled = false;
        profile.session_log = Some(crate::client::endpoint::SessionLogProfile {
            enabled: true,
            path_template: Some("{pane}.log".into()),
            max_bytes: Some(8192),
            dump_interval_secs: Some(10),
        });
        let report = probe(&profile);
        assert!(report.session_log.enabled);
        assert_eq!(report.session_log.path_template, "{pane}.log");
        assert_eq!(report.session_log.max_bytes, 8192);
        assert_eq!(report.session_log.dump_interval_secs, 10);
    }
}
