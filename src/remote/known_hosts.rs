//! known_hosts inspection and pre-seeding helpers.
//!
//! Thin wrappers around the OpenSSH tools (`ssh-keygen -F`/`-R` and
//! `ssh-keyscan`) so the rest of the remote layer never shells out ad hoc.
//! Every spawned command goes through the shared timeout/bounded-capture
//! discipline in `super::process`; platform-specific paths come from
//! `crate::platform`.

use std::io;
use std::io::Write as _;
use std::process::{Command, Stdio};
use std::time::Duration;

use base64::Engine as _;
use sha2::{Digest as _, Sha256};

use super::error::HostKeyFingerprint;
use super::process::wait_with_output_timeout_bounded;

const KNOWN_HOSTS_COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const KEYSCAN_CONNECT_TIMEOUT_SECS: u32 = 5;
const KNOWN_HOSTS_STDOUT_LIMIT: usize = 64 * 1024;
const KNOWN_HOSTS_STDERR_LIMIT: usize = 16 * 1024;
/// Public host keys are not secret, but keep the parsing surface bounded.
const MAX_KNOWN_HOST_KEYS: usize = 64;

/// One host key record: its SSH key type and OpenSSH-style SHA256 fingerprint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct KnownHostKey {
    pub(crate) key_type: String,
    pub(crate) fingerprint: HostKeyFingerprint,
    /// Full `keytype base64` key line, reused when pre-seeding known_hosts.
    pub(crate) key_line: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EffectiveHostKeyTarget {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) lookup: String,
    pub(crate) files: Vec<std::path::PathBuf>,
    pub(crate) proxied: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HostKeyReview {
    pub(crate) target: EffectiveHostKeyTarget,
    pub(crate) keys: Vec<KnownHostKey>,
}

/// 使用与连接相同的 -F 配置，让 OpenSSH 处理 Host/Match/Include 和覆盖顺序。
pub(crate) fn effective_host_key_target(
    profile: &crate::client::endpoint::SavedSshEndpoint,
) -> io::Result<EffectiveHostKeyTarget> {
    let options = super::saved::saved_profile_ssh_options(profile)?;
    let mut config = super::attach::write_managed_ssh_config(options.as_ref())?;
    // `-G` 只打印有效配置，不需要控制主连接（HERDR-MACH-004）。
    config.options.control_path = None;
    let mut command = Command::new("ssh");
    super::attach::apply_managed_channel_options(&mut command, Some(&config.options));
    command
        .args(["-G", "-T", "--"])
        .arg(&profile.target)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = super::process::spawn(&mut command)?;
    let output = wait_with_output_timeout_bounded(
        child,
        KNOWN_HOSTS_COMMAND_TIMEOUT,
        KNOWN_HOSTS_STDOUT_LIMIT,
        KNOWN_HOSTS_STDERR_LIMIT,
    )?;
    if !output.status.success() {
        return Err(tool_failed("无法解析 SSH 配置", &output));
    }
    parse_effective_target(&String::from_utf8_lossy(&output.stdout), &profile.target)
}

fn parse_effective_target(output: &str, original: &str) -> io::Result<EffectiveHostKeyTarget> {
    let fields = output
        .lines()
        .filter_map(|line| line.split_once(' '))
        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim()))
        .collect::<std::collections::HashMap<_, _>>();
    let field = |name: &str| {
        fields
            .get(name)
            .copied()
            .filter(|value| !value.is_empty() && *value != "none")
    };
    let invalid = |message: &str| io::Error::new(io::ErrorKind::Unsupported, message);
    let host = field("hostname")
        .ok_or_else(|| invalid("SSH 配置缺少 HostName"))?
        .to_owned();
    let port = field("port")
        .and_then(|value| value.parse::<u16>().ok())
        .filter(|port| *port > 0)
        .ok_or_else(|| invalid("SSH 配置端口无效"))?;
    let original_host = parse_ssh_host_port(original)
        .map(|(host, _)| host)
        .ok_or_else(|| invalid("无法解析原始 SSH 主机名"))?;
    let alias = field("hostkeyalias");
    let lookup = alias.map(str::to_owned).unwrap_or_else(|| {
        if port == 22 {
            host.clone()
        } else {
            format!("[{host}]:{port}")
        }
    });
    if host.starts_with('-')
        || host.chars().any(|ch| ch.is_control() || ch.is_whitespace())
        || lookup
            .chars()
            .any(|ch| ch.is_control() || ch.is_whitespace() || matches!(ch, '*' | '?' | ',' | '!'))
    {
        return Err(invalid(
            "主机名或 HostKeyAlias 无法作为单一主机记录，请使用交互认证",
        ));
    }
    let home =
        crate::platform::ssh_config_home_dir().ok_or_else(|| invalid("无法定位 SSH 用户目录"))?;
    let home = home.to_string_lossy();
    let user = field("user").unwrap_or_default();
    let local_user = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_default();
    let mut files = Vec::new();
    // ssh -G 不保留含空格路径的引号。无法无歧义还原时拒绝自动写入，交由交互 SSH 处理。
    for raw in field("userknownhostsfile")
        .unwrap_or_default()
        .split_whitespace()
    {
        if !(raw.starts_with('/') || raw.starts_with("~/") || raw.as_bytes().get(1) == Some(&b':'))
        {
            return Err(invalid(
                "known_hosts 路径存在歧义，请使用交互认证确认主机密钥",
            ));
        }
        let mut expanded = String::new();
        let mut chars = raw.chars();
        while let Some(ch) = chars.next() {
            if ch != '%' {
                expanded.push(ch);
                continue;
            }
            let value = match chars.next() {
                Some('%') => "%",
                Some('d') => home.as_ref(),
                Some('h') => host.as_str(),
                Some('p') => {
                    expanded.push_str(&port.to_string());
                    continue;
                }
                Some('r') => user,
                Some('u') => &local_user,
                Some('n') => original_host.as_str(),
                Some('k') => alias.unwrap_or(&original_host),
                _ => {
                    return Err(invalid(
                        "known_hosts 路径包含未支持的替换符，请使用交互认证",
                    ))
                }
            };
            expanded.push_str(value);
        }
        if let Some(relative) = expanded.strip_prefix("~/") {
            expanded = format!("{home}/{relative}");
        }
        files.push(std::path::PathBuf::from(expanded));
    }
    if files.is_empty() {
        return Err(invalid(
            "此 SSH 配置未启用用户 known_hosts 文件，请使用交互认证",
        ));
    }
    Ok(EffectiveHostKeyTarget {
        host,
        port,
        lookup,
        files,
        proxied: field("proxyjump").is_some() || field("proxycommand").is_some(),
    })
}

pub(crate) fn review_profile_host_key(
    profile: &crate::client::endpoint::SavedSshEndpoint,
) -> io::Result<HostKeyReview> {
    let target = effective_host_key_target(profile)?;
    if target.proxied {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "此连接经过跳板机或代理，请使用交互认证确认指纹；不会绕过代理直接扫描",
        ));
    }
    let keys = scan_host_keys(&target.host, Some(target.port))?;
    Ok(HostKeyReview { target, keys })
}

pub(crate) fn remember_reviewed_host_key(
    profile: &crate::client::endpoint::SavedSshEndpoint,
    target: &EffectiveHostKeyTarget,
    key: &KnownHostKey,
) -> io::Result<usize> {
    if effective_host_key_target(profile)? != *target {
        return Err(io::Error::other("SSH 配置已变化，请重新查看并确认指纹"));
    }
    append_reviewed_key(target, key)
}

pub(super) fn append_reviewed_key(
    target: &EffectiveHostKeyTarget,
    key: &KnownHostKey,
) -> io::Result<usize> {
    let parts = key.key_line.split_whitespace().collect::<Vec<_>>();
    if parts.len() != 2 || key_line_to_known_host_key(parts[0], parts[1]).as_ref() != Some(key) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "主机密钥与已展示的指纹不一致",
        ));
    }
    let path = target
        .files
        .first()
        .ok_or_else(|| io::Error::other("没有可写入的 known_hosts 路径"))?;
    if path.is_file() {
        let existing = run_tool(
            "ssh-keygen",
            &[
                "-F".into(),
                target.lookup.clone(),
                "-f".into(),
                path.to_string_lossy().into_owned(),
            ],
        )?;
        if parse_key_lines(&String::from_utf8_lossy(&existing.stdout))
            .iter()
            .any(|known| known == key)
        {
            return Ok(0);
        }
    }
    super::process::check_cancelled()?;
    if let Some(parent) = path.parent().filter(|parent| !parent.exists()) {
        crate::platform::create_remote_private_dir(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)?;
    // 只写入本次界面已经展示的公钥，确认后不重新扫描网络。
    writeln!(file, "{} {}", target.lookup, key.key_line)?;
    Ok(1)
}

pub(crate) fn remove_profile_host_key(
    profile: &crate::client::endpoint::SavedSshEndpoint,
) -> io::Result<()> {
    let target = effective_host_key_target(profile)?;
    for path in target.files.iter().filter(|path| path.is_file()) {
        super::process::check_cancelled()?;
        let output = run_tool(
            "ssh-keygen",
            &[
                "-R".into(),
                target.lookup.clone(),
                "-f".into(),
                path.to_string_lossy().into_owned(),
            ],
        )?;
        if !output.status.success() {
            return Err(tool_failed("清理该主机的旧密钥失败", &output));
        }
    }
    Ok(())
}

/// Parses an SSH target (`user@host`, `host:port`, `ssh://user@host:port`,
/// `[v6]::1:2222`) into host and optional explicit port. Returns `None` for
/// empty authorities. Targets without a port scan the SSH default (22);
/// ports that only live in the user's ssh config are not resolved here.
pub(crate) fn parse_ssh_host_port(target: &str) -> Option<(String, Option<u16>)> {
    let authority = target.strip_prefix("ssh://").unwrap_or(target);
    let authority = authority.trim_end_matches('/');
    let host_port = authority.rsplit('@').next()?;
    if host_port.is_empty() {
        return None;
    }
    if let Some(rest) = host_port.strip_prefix('[') {
        let close = rest.find(']')?;
        let host = &rest[..close];
        if host.is_empty() {
            return None;
        }
        let port = rest[close + 1..]
            .strip_prefix(':')
            .and_then(|port| port.parse::<u16>().ok());
        return Some((host.to_string(), port));
    }
    if host_port.matches(':').count() > 1 {
        return Some((host_port.to_owned(), None));
    }
    match host_port.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() => match port.parse::<u16>() {
            Ok(port) => Some((host.to_string(), Some(port))),
            Err(_) => Some((host_port.to_string(), None)),
        },
        _ => Some((host_port.to_string(), None)),
    }
}

/// known_hosts lookup patterns for a host: plain for the default port,
/// `[host]:port` otherwise, matching OpenSSH's own recording format.
fn host_key_patterns(host: &str, port: Option<u16>) -> Vec<String> {
    match port {
        None | Some(22) => vec![host.to_string()],
        Some(port) => vec![format!("[{host}]:{port}")],
    }
}

/// Computes the OpenSSH `SHA256:` fingerprint of a base64 key blob. This is
/// exactly what `ssh-keygen -lf` prints, computed without a temp file.
fn fingerprint_for_key_blob(key_type: &str, key_base64: &str) -> Option<HostKeyFingerprint> {
    let blob = base64::engine::general_purpose::STANDARD
        .decode(key_base64)
        .ok()?;
    let digest = Sha256::digest(&blob);
    Some(HostKeyFingerprint {
        key_type: key_type.to_string(),
        fingerprint: format!(
            "SHA256:{}",
            base64::engine::general_purpose::STANDARD_NO_PAD.encode(digest)
        ),
    })
}

fn key_line_to_known_host_key(key_type: &str, key_base64: &str) -> Option<KnownHostKey> {
    let fingerprint = fingerprint_for_key_blob(key_type, key_base64)?;
    Some(KnownHostKey {
        key_type: key_type.to_string(),
        fingerprint,
        key_line: format!("{key_type} {key_base64}"),
    })
}

fn ssh_keygen_lookup_args(pattern: &str) -> Vec<String> {
    vec!["-F".to_string(), pattern.to_string()]
}

fn ssh_keygen_remove_args(pattern: &str) -> Vec<String> {
    vec!["-R".to_string(), pattern.to_string()]
}

fn ssh_keyscan_args(host: &str, port: Option<u16>) -> Vec<String> {
    let mut args = vec!["-T".to_string(), KEYSCAN_CONNECT_TIMEOUT_SECS.to_string()];
    if let Some(port) = port {
        args.push("-p".to_string());
        args.push(port.to_string());
    }
    args.push(host.to_string());
    args
}

fn run_tool(program: &str, args: &[String]) -> io::Result<std::process::Output> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = super::process::spawn(&mut command).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            io::Error::new(
                io::ErrorKind::Unsupported,
                format!("{program} is not available on this system: {error}"),
            )
        } else {
            error
        }
    })?;
    wait_with_output_timeout_bounded(
        child,
        KNOWN_HOSTS_COMMAND_TIMEOUT,
        KNOWN_HOSTS_STDOUT_LIMIT,
        KNOWN_HOSTS_STDERR_LIMIT,
    )
}

fn tool_failed(context: &str, output: &std::process::Output) -> io::Error {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    if stderr.is_empty() {
        io::Error::other(format!("{context}: {}", output.status))
    } else {
        io::Error::other(format!("{context}: {stderr}"))
    }
}

/// Parses `host keytype base64` lines (skipping `#` comments and banners) as
/// produced by `ssh-keygen -F` and `ssh-keyscan`.
fn parse_key_lines(output: &str) -> Vec<KnownHostKey> {
    output
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let mut fields = line.split_whitespace();
            // ssh-keygen -F prints the looked-up name first; ssh-keyscan the
            // scanned host. The key material is always fields two and three.
            let _hosts = fields.next()?;
            let key_type = fields.next()?;
            if !key_type.starts_with("ssh-") && !key_type.starts_with("ecdsa-") {
                return None;
            }
            let key_base64 = fields.next()?;
            key_line_to_known_host_key(key_type, key_base64)
        })
        .take(MAX_KNOWN_HOST_KEYS)
        .collect()
}

/// Lists the host keys recorded for `host` in the default known_hosts file
/// (`ssh-keygen -F`). An empty result means the host is unknown.
// Mechanism surface for the next stage's host-key approval UI.
#[allow(dead_code)]
pub(crate) fn lookup_host_keys(host: &str, port: Option<u16>) -> io::Result<Vec<KnownHostKey>> {
    let mut keys = Vec::new();
    for pattern in host_key_patterns(host, port) {
        let output = run_tool("ssh-keygen", &ssh_keygen_lookup_args(&pattern))?;
        if !output.status.success() {
            // Exit 1: host not found in known_hosts.
            if output.stdout.is_empty() {
                continue;
            }
            return Err(tool_failed("known_hosts lookup failed", &output));
        }
        keys.extend(parse_key_lines(&String::from_utf8_lossy(&output.stdout)));
    }
    keys.truncate(MAX_KNOWN_HOST_KEYS);
    Ok(keys)
}

/// Removes every known_hosts record for `host` (`ssh-keygen -R`). Returns
/// whether any record was removed.
// Mechanism surface for the next stage's host-key approval UI.
#[allow(dead_code)]
pub(crate) fn remove_host_key(host: &str, port: Option<u16>) -> io::Result<bool> {
    let mut removed = false;
    for pattern in host_key_patterns(host, port) {
        let output = run_tool("ssh-keygen", &ssh_keygen_remove_args(&pattern))?;
        if output.status.success() {
            removed = true;
            continue;
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("not found") {
            continue;
        }
        return Err(tool_failed("known_hosts removal failed", &output));
    }
    Ok(removed)
}

/// Scans the host's currently presented keys (`ssh-keyscan`) without
/// recording them. Used to show the fingerprint of an unknown key before the
/// user decides to trust it.
pub(crate) fn scan_host_keys(host: &str, port: Option<u16>) -> io::Result<Vec<KnownHostKey>> {
    let output = run_tool("ssh-keyscan", &ssh_keyscan_args(host, port))?;
    if !output.status.success() && output.stdout.is_empty() {
        return Err(tool_failed("host key scan failed", &output));
    }
    Ok(parse_key_lines(&String::from_utf8_lossy(&output.stdout)))
}

/// Pre-seeds the default known_hosts file with the host's scanned keys.
/// Lines already present (same key type and blob) are not duplicated. Returns
/// every scanned key so callers can display or pin the fingerprints.
// Mechanism surface for the next stage's host-key approval UI.
#[allow(dead_code)]
pub(crate) fn precollect_host_keys(host: &str, port: Option<u16>) -> io::Result<Vec<KnownHostKey>> {
    let scanned = scan_host_keys(host, port)?;
    if scanned.is_empty() {
        return Ok(scanned);
    }
    let Some(path) = crate::platform::default_known_hosts_path() else {
        return Err(io::Error::other(
            "could not locate the default known_hosts file",
        ));
    };
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let patterns = host_key_patterns(host, port);
    let mut additions = String::new();
    for key in &scanned {
        if existing.lines().any(|line| line.contains(&key.key_line)) {
            continue;
        }
        for pattern in &patterns {
            additions.push_str(&format!("{pattern} {}\n", key.key_line));
        }
    }
    if !additions.is_empty() {
        if let Some(parent) = path.parent().filter(|parent| !parent.exists()) {
            crate::platform::create_remote_private_dir(parent)?;
        }
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&path)?;
        file.write_all(additions.as_bytes())?;
    }
    Ok(scanned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_host_key_target_uses_resolved_port_alias_and_path() {
        let target = parse_effective_target("hostname internal.example\nport 2207\nuser deploy\nhostkeyalias build-key\nuserknownhostsfile /tmp/known-%h-%p-%r\nproxyjump jump.example\n", "build").unwrap();
        assert_eq!(target.host, "internal.example");
        assert_eq!(target.port, 2207);
        assert_eq!(target.lookup, "build-key");
        assert_eq!(
            target.files,
            vec![std::path::PathBuf::from(
                "/tmp/known-internal.example-2207-deploy"
            )]
        );
        assert!(target.proxied);
        let without_alias = parse_effective_target(
            "hostname host.example\nport 2222\nuserknownhostsfile /tmp/known\n",
            "build",
        )
        .unwrap();
        assert_eq!(without_alias.lookup, "[host.example]:2222");
        for original in ["user@2001:db8::1", "ssh://user@[2001:db8::1]:2222"] {
            let ipv6 = parse_effective_target(
                "hostname 2001:db8::1\nport 2222\nuserknownhostsfile /tmp/%k-%n\n",
                original,
            )
            .unwrap();
            assert_eq!(
                ipv6.files,
                vec![std::path::PathBuf::from("/tmp/2001:db8::1-2001:db8::1")]
            );
        }
    }

    #[test]
    fn ambiguous_or_expanding_host_records_are_rejected_before_write() {
        for fields in [
            "userknownhostsfile /tmp/with space\n",
            "hostkeyalias *\nuserknownhostsfile /tmp/known\n",
            "userknownhostsfile /tmp/%x\n",
        ] {
            assert!(parse_effective_target(
                &format!("hostname host.example\nport 22\n{fields}"),
                "build"
            )
            .is_err());
        }
    }

    #[test]
    fn reviewed_key_cannot_be_replaced_after_confirmation() {
        let target = EffectiveHostKeyTarget {
            host: "example.com".into(),
            port: 22,
            lookup: "example.com".into(),
            files: vec![],
            proxied: false,
        };
        let mut key = key_line_to_known_host_key("ssh-ed25519", "AQID").unwrap();
        key.key_line = "ssh-ed25519 BAUG".into();
        assert_eq!(
            append_reviewed_key(&target, &key).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn parses_targets_into_host_and_port() {
        assert_eq!(
            parse_ssh_host_port("build.example"),
            Some(("build.example".to_string(), None))
        );
        assert_eq!(
            parse_ssh_host_port("dev@build.example"),
            Some(("build.example".to_string(), None))
        );
        assert_eq!(
            parse_ssh_host_port("ssh://dev@build.example:2222"),
            Some(("build.example".to_string(), Some(2222)))
        );
        assert_eq!(
            parse_ssh_host_port("build.example:2222"),
            Some(("build.example".to_string(), Some(2222)))
        );
        assert_eq!(
            parse_ssh_host_port("[2001:db8::1]:2222"),
            Some(("2001:db8::1".to_string(), Some(2222)))
        );
        assert_eq!(parse_ssh_host_port(""), None);
        assert_eq!(parse_ssh_host_port("user@"), None);
    }

    #[test]
    fn host_key_patterns_follow_openssh_recording_format() {
        assert_eq!(host_key_patterns("host", None), vec!["host".to_string()]);
        assert_eq!(
            host_key_patterns("host", Some(22)),
            vec!["host".to_string()]
        );
        assert_eq!(
            host_key_patterns("host", Some(2222)),
            vec!["[host]:2222".to_string()]
        );
    }

    #[test]
    fn tool_arguments_are_constructed_for_openssh() {
        assert_eq!(ssh_keygen_lookup_args("host"), vec!["-F", "host"]);
        assert_eq!(ssh_keygen_remove_args("[h]:2222"), vec!["-R", "[h]:2222"]);
        assert_eq!(
            ssh_keyscan_args("host", None),
            vec![
                "-T".to_string(),
                KEYSCAN_CONNECT_TIMEOUT_SECS.to_string(),
                "host".to_string()
            ]
        );
        assert_eq!(
            ssh_keyscan_args("host", Some(2222)),
            vec![
                "-T".to_string(),
                KEYSCAN_CONNECT_TIMEOUT_SECS.to_string(),
                "-p".to_string(),
                "2222".to_string(),
                "host".to_string()
            ]
        );
    }

    #[test]
    fn fingerprint_matches_openssh_sha256_format() {
        // Generated with `ssh-keygen -lf` on an ed25519 test key.
        let fingerprint = fingerprint_for_key_blob(
            "ssh-ed25519",
            "AAAAC3NzaC1lZDI1NTE5AAAAINBnbT1aEIGd0svL1jLRnYrFoYQgWCP+gMpgLVTj3A3p",
        )
        .expect("valid key blob");
        assert_eq!(fingerprint.key_type, "ssh-ed25519");
        assert!(
            fingerprint.fingerprint.starts_with("SHA256:"),
            "{}",
            fingerprint.fingerprint
        );
        assert!(!fingerprint.fingerprint.ends_with('='));
        assert!(fingerprint_for_key_blob("ssh-ed25519", "not base64!").is_none());
    }

    #[test]
    fn parses_key_lines_from_lookup_and_scan_output() {
        let output = "\
# Host build.example found: line 3 /home/dev/.ssh/known_hosts
build.example ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAINBnbT1aEIGd0svL1jLRnYrFoYQgWCP+gMpgLVTj3A3p
# 192.0.2.1:22 SSH-2.0-OpenSSH_9.6
192.0.2.1 ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBNciqYBGVqbKkC3Zip2BXQttxbB6LhkqR2KKsGFmPGLB3n8yuK1vQvoTdRdFiJxPPaGvDCqCbjkQWVQr+4kXwSE=
banner line without enough fields
";
        let keys = parse_key_lines(output);
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].key_type, "ssh-ed25519");
        assert_eq!(keys[1].key_type, "ecdsa-sha2-nistp256");
        assert!(keys[0].fingerprint.fingerprint.starts_with("SHA256:"));
        assert!(keys[0].key_line.starts_with("ssh-ed25519 AAAA"));
    }
}
