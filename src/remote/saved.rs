use std::io;
use std::path::PathBuf;

use super::askpass::{AskpassEnvironment, SshAskpassChannel, SshAskpassPrompts, SshAuthApproval};
use super::attach::{find_installed_remote_herdr, RemoteSsh, SshStdioBridge};
use super::error::{classify_connection_error, ConnectionErrorKind};
use super::profile::ProfileSshOptions;
use crate::client::endpoint::{EndpointCatalog, ProxyJumpHop, SavedSshEndpoint};

pub(crate) struct SavedSshBridge {
    _bridge: SshStdioBridge,
    /// Keeps the approved askpass channel alive for as long as bridge ssh
    /// processes may still prompt. `None` on the default non-interactive path.
    _askpass: Option<SshAskpassChannel>,
}

pub(crate) struct SavedSshStream {
    pub(crate) stream: crate::ipc::LocalStream,
    pub(crate) bridge: SavedSshBridge,
}

pub(crate) fn connect_saved_ssh(profile: &SavedSshEndpoint) -> io::Result<SavedSshStream> {
    connect_saved_ssh_with(profile, None, None, None)
}

/// Approved interactive retry after a BatchMode probe classified the failure
/// as `ConnectionErrorKind::AuthRequired`: returns the askpass channel and its
/// prompt receiver up front, so the caller can service prompts (on its own
/// thread) while [`connect_saved_ssh_interactive`] drives the prompting ssh
/// probes to completion. Possession of the channel is the approval token: it
/// is only created with `SshAuthApproval::Approved`; anything else keeps
/// every connection fully non-interactive and fails immediately.
// Entry point of the next stage's interactive-auth flow (TUI approval).
#[allow(dead_code)]
pub(crate) fn start_interactive_auth_channel(
    approval: SshAuthApproval,
) -> io::Result<(SshAskpassChannel, SshAskpassPrompts)> {
    if approval != SshAuthApproval::Approved {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "interactive SSH authentication requires explicit approval",
        ));
    }
    SshAskpassChannel::start()
}

/// Interactive retry of a saved connection over an approved channel (see
/// [`start_interactive_auth_channel`]): ssh runs without BatchMode and with
/// the askpass channel attached, so password/passphrase prompts are delivered
/// through the channel for programmatic answering. The channel is moved into
/// the bridge and lives for as long as bridge ssh processes may still prompt.
// Entry point of the next stage's interactive-auth flow (TUI approval).
#[allow(dead_code)]
pub(crate) fn connect_saved_ssh_interactive(
    profile: &SavedSshEndpoint,
    channel: SshAskpassChannel,
) -> io::Result<SavedSshStream> {
    connect_saved_ssh_with(profile, Some(channel), None, None)
}

pub(crate) fn connect_saved_ssh_authenticated(
    profile: &SavedSshEndpoint,
    channel: SshAskpassChannel,
    prepare: bool,
    pin: Option<&(
        super::known_hosts::EffectiveHostKeyTarget,
        super::known_hosts::KnownHostKey,
    )>,
    progress: &dyn Fn(super::attach::SavedSshBootstrapStep),
) -> io::Result<SavedSshStream> {
    connect_saved_ssh_with(profile, Some(channel), prepare.then_some(progress), pin)
}

fn connect_saved_ssh_with(
    profile: &SavedSshEndpoint,
    askpass: Option<SshAskpassChannel>,
    prepare: Option<&dyn Fn(super::attach::SavedSshBootstrapStep)>,
    pin: Option<&(
        super::known_hosts::EffectiveHostKeyTarget,
        super::known_hosts::KnownHostKey,
    )>,
) -> io::Result<SavedSshStream> {
    let result = (|| {
        let askpass_environment = askpass
            .as_ref()
            .map(|channel| channel.environment().clone());
        let mut ssh = validated_saved_ssh(profile, askpass_environment)?;
        if let Some((target, key)) = pin {
            if super::known_hosts::effective_host_key_target(profile)? != *target {
                return Err(io::Error::other(
                    crate::i18n::texts().remote.ssh_config_changed_confirm,
                ));
            }
            ssh.pin_host_key(target, key)?;
        }
        if let Some(progress) = prepare {
            super::attach::prepare_approved_saved_ssh(&ssh, &profile.session, progress)?;
        }
        super::process::check_cancelled()?;
        let remote_herdr = find_installed_remote_herdr(&ssh)?;
        let path = saved_bridge_path(profile.id.as_str());
        let bridge = SshStdioBridge::start(
            profile.target.to_owned(),
            remote_herdr,
            path.clone(),
            profile.session.to_owned(),
            ssh.options(),
            true,
            askpass.as_ref().map(|channel| channel.environment()),
        )?;
        let stream = crate::ipc::connect_local_stream(&path)?;
        Ok(SavedSshStream {
            stream,
            bridge: SavedSshBridge {
                _bridge: bridge,
                _askpass: askpass,
            },
        })
    })();
    result.map_err(|error| classify_saved_ssh_error(error, profile))
}

pub(crate) struct SavedSshApiBridge {
    path: PathBuf,
    bridge: SshStdioBridge,
}

impl SavedSshApiBridge {
    pub(crate) fn start(profile: &SavedSshEndpoint) -> io::Result<Self> {
        let result = (|| {
            let ssh = validated_saved_ssh(profile, None)?;
            let remote_herdr =
                super::attach::find_installed_remote_api_herdr(&ssh, &profile.session)?;
            let command =
                super::attach::remote_api_bridge_command(&remote_herdr, &profile.session, false);
            let profile_id = profile.id.as_str();
            let path = crate::platform::remote_bridge_endpoint_path(
                &format!("herdr-api-ssh-{}-{profile_id}.sock", std::process::id()),
                &format!(
                    "herdr-api-{}-{}.sock",
                    std::process::id(),
                    &profile_id[..16]
                ),
            );
            let bridge = SshStdioBridge::start_command(
                profile.target.to_owned(),
                command,
                path.clone(),
                ssh.options(),
                true,
                None,
            )?;
            Ok(Self { path, bridge })
        })();
        result.map_err(|error| classify_saved_ssh_error(error, profile))
    }

    pub(crate) fn socket_path(&self) -> &std::path::Path {
        &self.path
    }

    pub(crate) fn reported_failure(&self) -> Option<io::Error> {
        self.bridge.reported_failure()
    }
}

pub(crate) fn saved_ssh_bootstrap_command(target: &str, session: &str) -> String {
    format!(
        "herdr --remote {} --session {}",
        super::shell_quote(target),
        super::shell_quote(session)
    )
}

/// Classifies a saved-connection failure and wraps it so the structured kind
/// (enriched with the profile's identity file and, for unknown host keys, a
/// scanned fingerprint) travels with the error. `Display` and `io::ErrorKind`
/// are preserved.
fn classify_saved_ssh_error(error: io::Error, profile: &SavedSshEndpoint) -> io::Error {
    super::error::classify_and_wrap(
        error,
        &profile.target,
        profile.identity_file.first().cloned(),
    )
}

/// Historical retry-vs-attention verdict for saved connections. This consumes
/// the structured classification but deliberately preserves the exact legacy
/// verdicts: kinds the legacy string matching would not have flagged (DNS,
/// timeout, `AuthDenied`, other) stay retryable, and a string fallback covers
/// diagnostics the classifier maps to `Other` (for example
/// `no matching host key`).
pub(crate) fn saved_ssh_failure_needs_attention(error: &io::Error) -> bool {
    if matches!(
        error.kind(),
        io::ErrorKind::InvalidInput
            | io::ErrorKind::InvalidData
            | io::ErrorKind::NotFound
            | io::ErrorKind::PermissionDenied
            | io::ErrorKind::Unsupported
    ) {
        return true;
    }
    if matches!(
        classify_connection_error(error),
        ConnectionErrorKind::AuthRequired { .. }
            | ConnectionErrorKind::HostKeyUnknown { .. }
            | ConnectionErrorKind::HostKeyChanged
            | ConnectionErrorKind::RemoteInstallRequired
            | ConnectionErrorKind::RemoteInstallFailed
            | ConnectionErrorKind::Protocol
    ) {
        return true;
    }
    legacy_saved_failure_attention(&error.to_string())
}

fn legacy_saved_failure_attention(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    [
        "permission denied",
        "host key verification failed",
        "remote host identification has changed",
        "no matching host key",
        "unsupported remote platform",
        "not ready",
        "install or update",
        "protocol",
        "handshake",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

fn saved_bridge_path(profile_id: &str) -> PathBuf {
    let pid = std::process::id();
    let readable = format!("herdr-ssh-{pid}-{profile_id}.sock");
    let short = format!("herdr-s-{pid}-{}.sock", &profile_id[..16]);
    crate::platform::remote_bridge_endpoint_path(&readable, &short)
}

fn validated_saved_ssh(
    profile: &SavedSshEndpoint,
    askpass: Option<AskpassEnvironment>,
) -> io::Result<RemoteSsh> {
    validate_profile_path_id(profile.id.as_str())?;
    crate::session::validate_name(&profile.session)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let options = saved_profile_ssh_options(profile)?;
    Ok(match askpass {
        Some(askpass) => RemoteSsh::new_saved_with_askpass(
            profile.target.to_owned(),
            profile.session.clone(),
            options,
            askpass,
        ),
        None => RemoteSsh::new_saved(profile.target.to_owned(), profile.session.clone(), options),
    })
}

/// Resolves a profile's SSH connection options. The catalog is only read when
/// a ProxyJump hop references another profile and needs its target.
pub(super) fn saved_profile_ssh_options(
    profile: &SavedSshEndpoint,
) -> io::Result<Option<ProfileSshOptions>> {
    if !profile.has_connection_options() {
        return Ok(None);
    }
    let needs_catalog = profile
        .proxy_jump
        .iter()
        .any(|hop| matches!(hop, ProxyJumpHop::Profile(_)));
    let profiles = if needs_catalog {
        EndpointCatalog::load_profiles().map_err(io::Error::other)?
    } else {
        Vec::new()
    };
    ProfileSshOptions::from_profile(profile, &profiles)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))
}

fn validate_profile_path_id(profile_id: &str) -> io::Result<()> {
    if profile_id.len() == 32
        && profile_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid SSH endpoint profile id",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bridge_paths_use_profile_identity_not_target_or_session() {
        let first = saved_bridge_path("0123456789abcdef0123456789abcdef");
        let second = saved_bridge_path("fedcba9876543210fedcba9876543210");
        assert_ne!(first, second);
        assert!(!first.to_string_lossy().contains("example.com"));
        assert!(!first.to_string_lossy().contains("default"));
    }

    #[test]
    fn bootstrap_command_preserves_the_explicit_remote_session() {
        assert_eq!(
            saved_ssh_bootstrap_command("build host", "agent work"),
            "herdr --remote 'build host' --session 'agent work'"
        );
    }

    #[test]
    fn prompt_and_compatibility_failures_require_attention() {
        for message in [
            "Permission denied (publickey)",
            "Host key verification failed",
            "matching Herdr is not ready; install or update",
            "handshake rejected",
        ] {
            assert!(saved_ssh_failure_needs_attention(&io::Error::other(
                message
            )));
        }
        assert!(!saved_ssh_failure_needs_attention(&io::Error::new(
            io::ErrorKind::TimedOut,
            "network timed out"
        )));
    }

    #[test]
    fn attention_verdicts_match_the_legacy_string_matching() {
        for attention in [
            "Permission denied (publickey)",
            "Permission denied (keyboard-interactive)",
            "Host key verification failed",
            "REMOTE HOST IDENTIFICATION HAS CHANGED!",
            "no matching host key type found. Their offer: ssh-rsa",
            "unsupported remote platform: plan9",
            "matching Herdr is not ready; install or update",
            "protocol version mismatch",
            "handshake rejected",
        ] {
            assert!(
                saved_ssh_failure_needs_attention(&io::Error::other(attention)),
                "{attention}"
            );
        }
        for retryable in [
            "ssh: Could not resolve hostname build.example: Name or service not known",
            "ssh: connect to host 192.0.2.1 port 22: Connection timed out",
            "Too many authentication failures",
            "server closed connection",
        ] {
            assert!(
                !saved_ssh_failure_needs_attention(&io::Error::other(retryable)),
                "{retryable}"
            );
        }
        for kind in [
            io::ErrorKind::InvalidInput,
            io::ErrorKind::InvalidData,
            io::ErrorKind::NotFound,
            io::ErrorKind::PermissionDenied,
            io::ErrorKind::Unsupported,
        ] {
            assert!(saved_ssh_failure_needs_attention(&io::Error::new(
                kind,
                "gated by error kind"
            )));
        }
    }

    #[test]
    fn classification_survives_wrapping() {
        let error = io::Error::other("user@host: Permission denied (publickey,password).");
        let profile = SavedSshEndpoint::with_options(
            "label",
            "user@host",
            "default",
            crate::client::endpoint::SshProfileOptions {
                identity_file: vec!["~/.ssh/build".into()],
                ..crate::client::endpoint::SshProfileOptions::default()
            },
        )
        .expect("valid profile");
        let wrapped = classify_saved_ssh_error(error, &profile);
        assert_eq!(
            wrapped.to_string(),
            "user@host: Permission denied (publickey,password)."
        );
        assert_eq!(
            classify_connection_error(&wrapped),
            ConnectionErrorKind::AuthRequired {
                methods: vec!["publickey".to_string(), "password".to_string()],
                identity_file: Some("~/.ssh/build".to_string()),
            }
        );
    }

    #[test]
    fn interactive_retry_requires_explicit_approval() {
        let error = start_interactive_auth_channel(SshAuthApproval::NonInteractive)
            .map(|_| ())
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }
}
