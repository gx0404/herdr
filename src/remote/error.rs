//! Structured classification of SSH connection failures.
//!
//! The saved-machine paths used to decide "retry vs needs a human" by
//! matching substrings in ssh stderr. This module makes that classification
//! explicit: stderr text and the `io::ErrorKind` are parsed once into a
//! [`ConnectionErrorKind`], which then travels with the error (see
//! [`wrap_classified`]) so supervisors and, later, the UI can react to the
//! kind instead of re-parsing message text.

use std::fmt;
use std::io;

/// A host key fingerprint as reported by `ssh-keygen -l` (SHA256 hash form).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HostKeyFingerprint {
    /// SSH key type, for example `ssh-ed25519` or `ecdsa-sha2-nistp256`.
    pub(crate) key_type: String,
    /// Fingerprint in `SHA256:<base64>` form (no padding), matching OpenSSH.
    pub(crate) fingerprint: String,
}

/// Structured kind of a failed connection attempt.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum ConnectionErrorKind {
    /// The hostname could not be resolved.
    Dns,
    /// The connection or an SSH probe timed out.
    Timeout,
    /// The server rejected the offered credentials outright (for example
    /// `Too many authentication failures`). Unlike [`Self::AuthRequired`]
    /// this is not a "probe could not authenticate" signal: the offered keys
    /// were tried and refused.
    AuthDenied,
    /// The host key is not in known_hosts. Carries the scanned fingerprint
    /// once a best-effort `ssh-keyscan` probe filled it in.
    HostKeyUnknown {
        fingerprint: Option<HostKeyFingerprint>,
    },
    /// known_hosts holds a different key for this host (possible MITM).
    HostKeyChanged,
    /// Authentication could not proceed non-interactively; an approved
    /// interactive retry may succeed. `methods` lists the server-offered
    /// authentication methods parsed from `Permission denied (...)`;
    /// `identity_file` is the profile's configured key, when set.
    AuthRequired {
        methods: Vec<String>,
        identity_file: Option<String>,
    },
    /// The remote Herdr install is missing, outdated, or needs a restart
    /// that only an approved interactive setup may perform.
    RemoteInstallRequired,
    /// An approved remote install/update was attempted and failed.
    RemoteInstallFailed,
    /// Wire protocol or handshake incompatibility with the remote server.
    Protocol,
    /// Anything not covered above.
    #[default]
    Other,
}

impl ConnectionErrorKind {
    /// Whether this kind cannot be fixed by retrying the same background
    /// connection and needs a human decision. Note that the compatibility
    /// shim `saved_ssh_failure_needs_attention` deliberately keeps its
    /// historical verdicts and does not consult every kind (in particular
    /// `AuthDenied` stays retryable there); this predicate describes the
    /// intended policy for new consumers such as the machines UI.
    // Policy surface for the next stage's machines UI.
    #[allow(dead_code)]
    pub(crate) fn needs_attention(&self) -> bool {
        match self {
            Self::Dns | Self::Timeout | Self::Other => false,
            Self::AuthDenied
            | Self::HostKeyUnknown { .. }
            | Self::HostKeyChanged
            | Self::AuthRequired { .. }
            | Self::RemoteInstallRequired
            | Self::RemoteInstallFailed
            | Self::Protocol => true,
        }
    }

    /// Stable machine-readable token for logs and UI badges.
    // Badge token for the next stage's machines UI.
    #[allow(dead_code)]
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::Dns => "dns",
            Self::Timeout => "timeout",
            Self::AuthDenied => "auth-denied",
            Self::HostKeyUnknown { .. } => "host-key-unknown",
            Self::HostKeyChanged => "host-key-changed",
            Self::AuthRequired { .. } => "auth-required",
            Self::RemoteInstallRequired => "remote-install-required",
            Self::RemoteInstallFailed => "remote-install-failed",
            Self::Protocol => "protocol",
            Self::Other => "other",
        }
    }

    /// Attaches the profile's identity file to an `AuthRequired` kind.
    pub(crate) fn with_identity_file(self, identity_file: Option<String>) -> Self {
        match self {
            Self::AuthRequired { methods, .. } => Self::AuthRequired {
                methods,
                identity_file,
            },
            other => other,
        }
    }

    /// Attaches a scanned fingerprint to a `HostKeyUnknown` kind.
    // Enrichment helper for the next stage's host-key approval flow.
    #[allow(dead_code)]
    pub(crate) fn with_host_key_fingerprint(self, fingerprint: Option<HostKeyFingerprint>) -> Self {
        match self {
            Self::HostKeyUnknown { .. } => Self::HostKeyUnknown { fingerprint },
            other => other,
        }
    }
}

/// Error payload carried inside an `io::Error` so the structured kind
/// survives the trip through supervisors. `Display` is exactly the original
/// message, keeping user-visible text and log output byte-identical.
#[derive(Debug)]
struct ClassifiedConnectionError {
    kind: ConnectionErrorKind,
    message: String,
}

impl fmt::Display for ClassifiedConnectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ClassifiedConnectionError {}

/// Wraps `error` with its structured classification, preserving the
/// `io::ErrorKind` and the exact `Display` text.
pub(crate) fn wrap_classified(error: io::Error, kind: ConnectionErrorKind) -> io::Error {
    io::Error::new(
        error.kind(),
        ClassifiedConnectionError {
            kind,
            message: error.to_string(),
        },
    )
}

/// Returns the structured kind of `error`. Errors previously wrapped by
/// [`wrap_classified`] return their stored kind (including enriched payloads
/// such as fingerprints); everything else is parsed from the `io::ErrorKind`
/// and the message text.
pub(crate) fn classify_connection_error(error: &io::Error) -> ConnectionErrorKind {
    if let Some(classified) = error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<ClassifiedConnectionError>())
    {
        return classified.kind.clone();
    }
    classify_message(error.kind(), &error.to_string())
}

fn classify_message(kind: io::ErrorKind, message: &str) -> ConnectionErrorKind {
    if kind == io::ErrorKind::TimedOut {
        return ConnectionErrorKind::Timeout;
    }
    let lower = message.to_ascii_lowercase();
    let contains = |needle: &str| lower.contains(needle);

    // Host key checks come first: a changed key also prints the generic
    // "Host key verification failed." trailer, so the specific one wins.
    if contains("remote host identification has changed") {
        return ConnectionErrorKind::HostKeyChanged;
    }
    if contains("host key verification failed") {
        return ConnectionErrorKind::HostKeyUnknown { fingerprint: None };
    }
    if contains("too many authentication failures") {
        return ConnectionErrorKind::AuthDenied;
    }
    // Install failures come before auth: a remote chmod/write "Permission
    // denied" is a filesystem problem, not an authentication prompt.
    if contains("install") && (contains("fail") || contains("exit")) {
        return ConnectionErrorKind::RemoteInstallFailed;
    }
    if contains("permission denied") {
        return ConnectionErrorKind::AuthRequired {
            methods: parse_permission_denied_methods(&lower),
            identity_file: None,
        };
    }
    if contains("could not resolve hostname")
        || contains("name or service not known")
        || contains("temporary failure in name resolution")
        || contains("no address associated with hostname")
        || contains("nodename nor servname provided")
    {
        return ConnectionErrorKind::Dns;
    }
    if contains("timed out") || contains("timeout") {
        return ConnectionErrorKind::Timeout;
    }
    if contains("install or update")
        || contains("not ready")
        || contains("needs one final update")
        || contains("needs a server update")
    {
        return ConnectionErrorKind::RemoteInstallRequired;
    }
    if contains("protocol") || contains("handshake") {
        return ConnectionErrorKind::Protocol;
    }
    ConnectionErrorKind::Other
}

/// Extracts the offered methods from `Permission denied (publickey,password)`
/// style diagnostics. Returns an empty list when ssh printed no method group.
fn parse_permission_denied_methods(lower_message: &str) -> Vec<String> {
    let Some(start) = lower_message.find("permission denied") else {
        return Vec::new();
    };
    let rest = &lower_message[start..];
    let Some(open) = rest.find('(') else {
        return Vec::new();
    };
    let Some(close) = rest[open + 1..].find(')') else {
        return Vec::new();
    };
    rest[open + 1..open + 1 + close]
        .split(',')
        .map(str::trim)
        .filter(|method| !method.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Classifies `error`, enriches the kind with connection context (profile
/// identity file, scanned host-key fingerprint), and wraps it so the kind
/// travels with the error. `Display` and `io::ErrorKind` are preserved.
pub(crate) fn classify_and_wrap(
    error: io::Error,
    target: &str,
    identity_file: Option<String>,
) -> io::Error {
    let kind =
        enrich_connection_error_kind(classify_connection_error(&error), target, identity_file);
    wrap_classified(error, kind)
}

/// Best-effort enrichment of a freshly classified kind: attaches the
/// profile's identity file to `AuthRequired` and, for an unknown host key,
/// scans the host once for the fingerprint it currently presents. Scan
/// failures are ignored (the fingerprint stays absent).
pub(crate) fn enrich_connection_error_kind(
    kind: ConnectionErrorKind,
    target: &str,
    identity_file: Option<String>,
) -> ConnectionErrorKind {
    let kind = kind.with_identity_file(identity_file);
    match kind {
        ConnectionErrorKind::HostKeyUnknown { fingerprint: None } => {
            let fingerprint = super::known_hosts::parse_ssh_host_port(target)
                .and_then(|(host, port)| {
                    super::known_hosts::scan_host_keys(&host, port)
                        .inspect_err(|error| {
                            tracing::debug!(%error, "host key scan for enrichment failed");
                        })
                        .ok()
                })
                .and_then(|keys| {
                    select_preferred_host_key(&keys).map(|key| key.fingerprint.clone())
                });
            ConnectionErrorKind::HostKeyUnknown { fingerprint }
        }
        other => other,
    }
}

/// Picks the key to show the user: strongest commonly preferred type first.
fn select_preferred_host_key(
    keys: &[super::known_hosts::KnownHostKey],
) -> Option<&super::known_hosts::KnownHostKey> {
    fn rank(key_type: &str) -> u8 {
        match key_type {
            "ssh-ed25519" => 0,
            _ if key_type.starts_with("ecdsa-") => 1,
            "ssh-rsa" => 2,
            _ => 3,
        }
    }
    keys.iter().min_by_key(|key| rank(&key.key_type))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classify(message: &str) -> ConnectionErrorKind {
        classify_connection_error(&io::Error::other(message))
    }

    #[test]
    fn classification_table_covers_ssh_diagnostics() {
        let cases: &[(&str, ConnectionErrorKind)] = &[
            (
                "ssh: Could not resolve hostname build.example: Name or service not known",
                ConnectionErrorKind::Dns,
            ),
            (
                "ssh: connect to host 192.0.2.1 port 22: Connection timed out",
                ConnectionErrorKind::Timeout,
            ),
            (
                "Operation timed out",
                ConnectionErrorKind::Timeout,
            ),
            (
                "REMOTE HOST IDENTIFICATION HAS CHANGED!\nHost key verification failed.",
                ConnectionErrorKind::HostKeyChanged,
            ),
            (
                "No RSA host key is known for fake-host and you have requested strict checking.\nHost key verification failed.",
                ConnectionErrorKind::HostKeyUnknown { fingerprint: None },
            ),
            (
                "user@host: Permission denied (publickey).",
                ConnectionErrorKind::AuthRequired {
                    methods: vec!["publickey".to_string()],
                    identity_file: None,
                },
            ),
            (
                "user@host: Permission denied (publickey,password).",
                ConnectionErrorKind::AuthRequired {
                    methods: vec!["publickey".to_string(), "password".to_string()],
                    identity_file: None,
                },
            ),
            (
                "Permission denied, please try again.",
                ConnectionErrorKind::AuthRequired {
                    methods: Vec::new(),
                    identity_file: None,
                },
            ),
            (
                "Received disconnect from 192.0.2.1 port 22:2: Too many authentication failures",
                ConnectionErrorKind::AuthDenied,
            ),
            (
                "matching Herdr is not ready; install or update it",
                ConnectionErrorKind::RemoteInstallRequired,
            ),
            (
                "remote herdr server needs one final update before this bridge can attach; rerun `herdr --remote`",
                ConnectionErrorKind::RemoteInstallRequired,
            ),
            (
                "this machine needs a server update before it can participate in multi-machine viewing",
                ConnectionErrorKind::RemoteInstallRequired,
            ),
            (
                "remote install preparation failed: Permission denied",
                ConnectionErrorKind::RemoteInstallFailed,
            ),
            (
                "remote install commit failed: disk quota exceeded",
                ConnectionErrorKind::RemoteInstallFailed,
            ),
            (
                "remote install exited with exit status: 1",
                ConnectionErrorKind::RemoteInstallFailed,
            ),
            (
                "remote server startup failed: protocol version mismatch",
                ConnectionErrorKind::Protocol,
            ),
            (
                "handshake rejected: surface capability missing",
                ConnectionErrorKind::Protocol,
            ),
            (
                "no matching host key type found. Their offer: ssh-rsa",
                ConnectionErrorKind::Other,
            ),
            ("server closed connection", ConnectionErrorKind::Other),
        ];
        for (message, expected) in cases {
            assert_eq!(&classify(message), expected, "{message}");
        }
    }

    #[test]
    fn error_kind_timeout_wins_over_message_text() {
        let error = io::Error::new(io::ErrorKind::TimedOut, "network timed out");
        assert_eq!(
            classify_connection_error(&error),
            ConnectionErrorKind::Timeout
        );
    }

    #[test]
    fn attention_policy_matches_the_intended_verdicts() {
        for (kind, expected) in [
            (ConnectionErrorKind::Dns, false),
            (ConnectionErrorKind::Timeout, false),
            (ConnectionErrorKind::Other, false),
            (ConnectionErrorKind::AuthDenied, true),
            (
                ConnectionErrorKind::HostKeyUnknown { fingerprint: None },
                true,
            ),
            (ConnectionErrorKind::HostKeyChanged, true),
            (
                ConnectionErrorKind::AuthRequired {
                    methods: Vec::new(),
                    identity_file: None,
                },
                true,
            ),
            (ConnectionErrorKind::RemoteInstallRequired, true),
            (ConnectionErrorKind::RemoteInstallFailed, true),
            (ConnectionErrorKind::Protocol, true),
        ] {
            assert_eq!(kind.needs_attention(), expected, "{kind:?}");
        }
    }

    #[test]
    fn kind_tokens_are_stable() {
        assert_eq!(ConnectionErrorKind::Dns.as_str(), "dns");
        assert_eq!(ConnectionErrorKind::Other.as_str(), "other");
        assert_eq!(
            ConnectionErrorKind::HostKeyUnknown { fingerprint: None }.as_str(),
            "host-key-unknown"
        );
    }

    #[test]
    fn wrapped_errors_keep_kind_and_display_and_round_trip() {
        let original = io::Error::new(
            io::ErrorKind::PermissionDenied,
            "remote platform detection failed: user@host: Permission denied (publickey).",
        );
        let kind = classify_connection_error(&original);
        let wrapped = wrap_classified(original, kind.clone());

        assert_eq!(wrapped.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(
            wrapped.to_string(),
            "remote platform detection failed: user@host: Permission denied (publickey)."
        );
        assert_eq!(classify_connection_error(&wrapped), kind);
    }

    #[test]
    fn enrichment_only_touches_matching_variants() {
        let kind = ConnectionErrorKind::AuthRequired {
            methods: vec!["publickey".to_string()],
            identity_file: None,
        }
        .with_identity_file(Some("~/.ssh/build".to_string()));
        assert_eq!(
            kind,
            ConnectionErrorKind::AuthRequired {
                methods: vec!["publickey".to_string()],
                identity_file: Some("~/.ssh/build".to_string()),
            }
        );

        let fingerprint = HostKeyFingerprint {
            key_type: "ssh-ed25519".to_string(),
            fingerprint: "SHA256:abc".to_string(),
        };
        let kind = ConnectionErrorKind::HostKeyUnknown { fingerprint: None }
            .with_host_key_fingerprint(Some(fingerprint.clone()));
        assert_eq!(
            kind,
            ConnectionErrorKind::HostKeyUnknown {
                fingerprint: Some(fingerprint.clone()),
            }
        );
        // Unrelated variants are left alone.
        assert_eq!(
            ConnectionErrorKind::Dns.with_host_key_fingerprint(Some(fingerprint)),
            ConnectionErrorKind::Dns
        );
    }
}
