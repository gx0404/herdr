use std::collections::HashSet;
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use super::ProfileId;

const LEGACY_CATALOG_VERSION_V1: u32 = 1;
const CATALOG_VERSION: u32 = 2;
const SELECTION_VERSION: u32 = 1;
const MAX_CATALOG_BYTES: u64 = 4 * 1024 * 1024;
const MAX_PROFILES: usize = 256;
const MAX_LABEL_BYTES: usize = 128;
const MAX_TARGET_BYTES: usize = 1024;
const MAX_GROUP_BYTES: usize = 128;
const MAX_TAGS: usize = 16;
const MAX_TAG_BYTES: usize = 64;
const MAX_COLOR_BYTES: usize = 64;
const MAX_USER_BYTES: usize = 255;
const MAX_IDENTITY_FILES: usize = 8;
const MAX_IDENTITY_FILE_BYTES: usize = 512;
const MAX_IDENTITY_AGENT_BYTES: usize = 512;
const MAX_PROXY_HOPS: usize = 8;
const MAX_PROXY_HOP_BYTES: usize = 512;
const MAX_CONTROL_PERSIST_BYTES: usize = 64;
const MAX_REMOTE_COMMAND_BYTES: usize = 1024;
const MAX_PORT_FORWARDS: usize = 16;
const MAX_FORWARD_HOST_BYTES: usize = 255;
static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum StrictHostKeyChecking {
    Ask,
    AcceptNew,
    Yes,
}

impl StrictHostKeyChecking {
    pub(crate) fn as_ssh_value(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::AcceptNew => "accept-new",
            Self::Yes => "yes",
        }
    }

    /// Non-interactive (background) connections run with BatchMode and cannot
    /// answer host-key prompts, so `ask` collapses to `yes` there; only
    /// `accept-new` relaxes the policy by recording new keys (TOFU).
    pub(crate) fn as_noninteractive_ssh_value(self) -> &'static str {
        match self {
            Self::Ask | Self::Yes => "yes",
            Self::AcceptNew => "accept-new",
        }
    }
}

/// One ProxyJump hop: a literal SSH target, or a reference to another saved
/// profile whose target is substituted when the connection is built.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProxyJumpHop {
    Target(String),
    Profile(ProfileId),
}

/// Session-logging preferences of a saved profile. All fields are optional;
/// the feature is off unless `enabled` is set. The mechanism lives in
/// `endpoint::session_log`: the client snapshots the machine's panes on an
/// interval and appends changed snapshots to local files.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionLogProfile {
    #[serde(default)]
    pub(crate) enabled: bool,
    /// Path template; see `endpoint::session_log::render_log_path` for the
    /// supported `{variables}`. Relative templates resolve under the client
    /// state directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) path_template: Option<String>,
    /// Per-file size cap before the log rotates to `<file>.1`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) max_bytes: Option<u64>,
    /// Snapshot interval in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) dump_interval_secs: Option<u16>,
}

impl SessionLogProfile {
    pub(crate) const MAX_TEMPLATE_BYTES: usize = 512;
    pub(crate) const MIN_MAX_BYTES: u64 = 4 * 1024;
    pub(crate) const MAX_MAX_BYTES: u64 = 1024 * 1024 * 1024;
    pub(crate) const MIN_DUMP_INTERVAL_SECS: u16 = 5;
    pub(crate) const MAX_DUMP_INTERVAL_SECS: u16 = 3600;

    fn validate(&self) -> Result<(), String> {
        if let Some(template) = &self.path_template {
            if template.trim().is_empty() {
                return Err("session log path template cannot be empty".into());
            }
            if template.len() > Self::MAX_TEMPLATE_BYTES || template.chars().any(char::is_control) {
                return Err(format!(
                    "session log path template must be at most {} bytes and contain no control characters",
                    Self::MAX_TEMPLATE_BYTES
                ));
            }
            // Unknown variables fail here at save time instead of at render
            // time inside a background worker.
            super::session_log::validate_path_template(template)?;
        }
        if let Some(max_bytes) = self.max_bytes {
            if !(Self::MIN_MAX_BYTES..=Self::MAX_MAX_BYTES).contains(&max_bytes) {
                return Err(format!(
                    "session log size limit must be between {} and {} bytes",
                    Self::MIN_MAX_BYTES,
                    Self::MAX_MAX_BYTES
                ));
            }
        }
        if let Some(interval) = self.dump_interval_secs {
            if !(Self::MIN_DUMP_INTERVAL_SECS..=Self::MAX_DUMP_INTERVAL_SECS).contains(&interval) {
                return Err(format!(
                    "session log snapshot interval must be between {} and {} seconds",
                    Self::MIN_DUMP_INTERVAL_SECS,
                    Self::MAX_DUMP_INTERVAL_SECS
                ));
            }
        }
        Ok(())
    }
}

/// SSH port forwarding direction of a saved-profile rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum PortForwardKind {
    /// `ssh -L`: listen locally, forward through the machine to the target.
    Local,
    /// `ssh -R`: listen on the machine, forward back to the target.
    Remote,
    /// `ssh -D`: local SOCKS proxy through the machine; no fixed target.
    Dynamic,
}

impl PortForwardKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Remote => "remote",
            Self::Dynamic => "dynamic",
        }
    }
}

/// One port forwarding rule kept in a saved SSH profile. Local and remote
/// rules forward to a fixed target; dynamic rules open a SOCKS proxy and
/// therefore carry no target.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PortForwardRule {
    pub(crate) kind: PortForwardKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) bind_address: Option<String>,
    pub(crate) listen_port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) target_host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) target_port: Option<u16>,
}

impl PortForwardRule {
    fn validate(&self) -> Result<(), String> {
        let kind = self.kind.as_str();
        if self.listen_port == 0 {
            return Err(format!(
                "SSH {kind} port forward listen port must be between 1 and 65535"
            ));
        }
        if let Some(bind_address) = &self.bind_address {
            validate_forward_host(bind_address, "SSH port forward bind address")?;
        }
        match self.kind {
            PortForwardKind::Local | PortForwardKind::Remote => {
                let Some(target_host) = &self.target_host else {
                    return Err(format!("SSH {kind} port forward requires a target host"));
                };
                validate_forward_host(target_host, "SSH port forward target host")?;
                match self.target_port {
                    Some(target_port) if target_port > 0 => Ok(()),
                    _ => Err(format!(
                        "SSH {kind} port forward target port must be between 1 and 65535"
                    )),
                }
            }
            PortForwardKind::Dynamic => {
                if self.target_host.is_some() || self.target_port.is_some() {
                    return Err("SSH dynamic port forward must not have a target".into());
                }
                Ok(())
            }
        }
    }
}

fn validate_forward_host(value: &str, what: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err(format!("{what} cannot be empty"));
    }
    if value.len() > MAX_FORWARD_HOST_BYTES
        || value
            .chars()
            .any(|ch| ch.is_control() || ch.is_whitespace())
    {
        return Err(format!(
            "{what} must be at most {MAX_FORWARD_HOST_BYTES} bytes and contain no control characters or whitespace"
        ));
    }
    if value.starts_with('-') {
        return Err(format!("{what} must not start with '-'"));
    }
    Ok(())
}

/// Connection and presentation metadata accepted by `machine add`. Every
/// field is optional; an all-default value reproduces the v1 profile shape.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SshProfileOptions {
    pub(crate) group: Option<String>,
    pub(crate) tags: Vec<String>,
    pub(crate) color: Option<String>,
    pub(crate) port: Option<u16>,
    pub(crate) user: Option<String>,
    pub(crate) identity_file: Vec<String>,
    pub(crate) identities_only: Option<bool>,
    pub(crate) identity_agent: Option<String>,
    pub(crate) strict_host_key_checking: Option<StrictHostKeyChecking>,
    pub(crate) proxy_jump: Vec<ProxyJumpHop>,
    pub(crate) forward_agent: Option<bool>,
    pub(crate) server_alive_interval: Option<u16>,
    pub(crate) server_alive_count_max: Option<u16>,
    pub(crate) control_persist: Option<String>,
    pub(crate) remote_command: Option<String>,
    /// Session logging is client-side behavior, not an SSH connection
    /// option; it rides along here so add/update flows can carry it.
    pub(crate) session_log: Option<SessionLogProfile>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SavedSshEndpoint {
    pub(crate) id: ProfileId,
    pub(crate) label: String,
    pub(crate) target: String,
    pub(crate) session: String,
    pub(crate) enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) group: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) color: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) user: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) identity_file: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) identities_only: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) identity_agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) strict_host_key_checking: Option<StrictHostKeyChecking>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) proxy_jump: Vec<ProxyJumpHop>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) forward_agent: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) server_alive_interval: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) server_alive_count_max: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) control_persist: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) remote_command: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) port_forwards: Vec<PortForwardRule>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) session_log: Option<SessionLogProfile>,
}

impl SavedSshEndpoint {
    #[cfg(test)]
    pub(crate) fn new(
        label: impl Into<String>,
        target: impl Into<String>,
        session: impl Into<String>,
    ) -> Result<Self, String> {
        Self::with_options(label, target, session, SshProfileOptions::default())
    }

    pub(crate) fn with_options(
        label: impl Into<String>,
        target: impl Into<String>,
        session: impl Into<String>,
        options: SshProfileOptions,
    ) -> Result<Self, String> {
        let profile = Self {
            id: ProfileId::generate(),
            label: label.into(),
            target: target.into(),
            session: session.into(),
            enabled: true,
            group: options.group,
            tags: options.tags,
            color: options.color,
            port: options.port,
            user: options.user,
            identity_file: options.identity_file,
            identities_only: options.identities_only,
            identity_agent: options.identity_agent,
            strict_host_key_checking: options.strict_host_key_checking,
            proxy_jump: options.proxy_jump,
            forward_agent: options.forward_agent,
            server_alive_interval: options.server_alive_interval,
            server_alive_count_max: options.server_alive_count_max,
            control_persist: options.control_persist,
            remote_command: options.remote_command,
            port_forwards: Vec::new(),
            session_log: options.session_log,
        };
        profile.validate()?;
        Ok(profile)
    }

    pub(crate) fn same_connection(&self, other: &Self) -> bool {
        self.target == other.target
            && self.session == other.session
            && self.port == other.port
            && self.user == other.user
            && self.identity_file == other.identity_file
            && self.proxy_jump == other.proxy_jump
            && self.strict_host_key_checking == other.strict_host_key_checking
            && self.identities_only == other.identities_only
            && self.identity_agent == other.identity_agent
            && self.forward_agent == other.forward_agent
            && self.server_alive_interval == other.server_alive_interval
            && self.server_alive_count_max == other.server_alive_count_max
            && self.control_persist == other.control_persist
            && self.remote_command == other.remote_command
    }

    /// Whether any field that changes how the SSH connection is built is set.
    /// Presentation metadata (group, tags, color) is excluded on purpose.
    pub(crate) fn has_connection_options(&self) -> bool {
        self.port.is_some()
            || self.user.is_some()
            || !self.identity_file.is_empty()
            || self.identities_only.is_some()
            || self.identity_agent.is_some()
            || self.strict_host_key_checking.is_some()
            || !self.proxy_jump.is_empty()
            || self.forward_agent.is_some()
            || self.server_alive_interval.is_some()
            || self.server_alive_count_max.is_some()
            || self.control_persist.is_some()
            || self.remote_command.is_some()
    }

    fn validate(&self) -> Result<(), String> {
        ProfileId::parse(self.id.to_string())?;
        let label = self.label.trim();
        if label.is_empty() {
            return Err("SSH endpoint label cannot be empty".into());
        }
        if label.len() > MAX_LABEL_BYTES || label.chars().any(char::is_control) {
            return Err(format!(
                "SSH endpoint label must be at most {MAX_LABEL_BYTES} bytes and contain no control characters"
            ));
        }
        if self.target.len() > MAX_TARGET_BYTES || self.target.chars().any(char::is_control) {
            return Err(format!(
                "SSH target must be at most {MAX_TARGET_BYTES} bytes and contain no control characters"
            ));
        }
        crate::remote::validate_remote_target(&self.target).map(|_| ())?;
        reject_embedded_password(&self.target, "SSH target")?;
        crate::session::validate_name(&self.session)?;
        if let Some(group) = &self.group {
            validate_text_field(group, MAX_GROUP_BYTES, "SSH endpoint group", false)?;
        }
        if self.tags.len() > MAX_TAGS {
            return Err(format!("SSH endpoint allows at most {MAX_TAGS} tags"));
        }
        for tag in &self.tags {
            validate_text_field(tag, MAX_TAG_BYTES, "SSH endpoint tag", false)?;
        }
        if let Some(color) = &self.color {
            validate_text_field(color, MAX_COLOR_BYTES, "SSH endpoint color", false)?;
            if crate::config::try_parse_color(color).is_none() {
                return Err(format!(
                    "SSH endpoint color must be a hex (#rgb/#rrggbb), rgb(r,g,b), or named color: {color}"
                ));
            }
        }
        if let Some(port) = self.port {
            if port == 0 {
                return Err("SSH endpoint port must be between 1 and 65535".into());
            }
        }
        if let Some(user) = &self.user {
            validate_text_field(user, MAX_USER_BYTES, "SSH endpoint user", true)?;
            if user.chars().any(|ch| ch.is_whitespace() || ch == '@') {
                return Err("SSH endpoint user must not contain whitespace or '@'".into());
            }
        }
        if self.identity_file.len() > MAX_IDENTITY_FILES {
            return Err(format!(
                "SSH endpoint allows at most {MAX_IDENTITY_FILES} identity files"
            ));
        }
        for identity_file in &self.identity_file {
            validate_text_field(
                identity_file,
                MAX_IDENTITY_FILE_BYTES,
                "SSH endpoint identity file",
                true,
            )?;
        }
        if let Some(identity_agent) = &self.identity_agent {
            validate_text_field(
                identity_agent,
                MAX_IDENTITY_AGENT_BYTES,
                "SSH endpoint identity agent",
                true,
            )?;
        }
        if self.proxy_jump.len() > MAX_PROXY_HOPS {
            return Err(format!(
                "SSH endpoint allows at most {MAX_PROXY_HOPS} proxy jump hops"
            ));
        }
        for hop in &self.proxy_jump {
            match hop {
                ProxyJumpHop::Target(target) => {
                    validate_text_field(target, MAX_PROXY_HOP_BYTES, "SSH proxy jump hop", true)?;
                    if target.starts_with('-') {
                        return Err("SSH proxy jump hop must not start with '-'".into());
                    }
                    reject_embedded_password(target, "SSH proxy jump hop")?;
                }
                ProxyJumpHop::Profile(id) => {
                    ProfileId::parse(id.to_string())?;
                }
            }
        }
        if let Some(control_persist) = &self.control_persist {
            validate_text_field(
                control_persist,
                MAX_CONTROL_PERSIST_BYTES,
                "SSH endpoint control persist",
                true,
            )?;
            if !is_valid_control_persist(control_persist) {
                return Err(
                    "SSH endpoint control persist must be yes, no, or a duration like 600 or 10m"
                        .into(),
                );
            }
        }
        if let Some(remote_command) = &self.remote_command {
            validate_text_field(
                remote_command,
                MAX_REMOTE_COMMAND_BYTES,
                "SSH endpoint remote command",
                true,
            )?;
        }
        if self.port_forwards.len() > MAX_PORT_FORWARDS {
            return Err(format!(
                "SSH endpoint allows at most {MAX_PORT_FORWARDS} port forward rules"
            ));
        }
        for rule in &self.port_forwards {
            rule.validate()?;
        }
        if let Some(session_log) = &self.session_log {
            session_log.validate()?;
        }
        Ok(())
    }
}

fn validate_text_field(
    value: &str,
    max_bytes: usize,
    what: &str,
    rendered_into_ssh_config: bool,
) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err(format!("{what} cannot be empty"));
    }
    if value.len() > max_bytes || value.chars().any(char::is_control) {
        return Err(format!(
            "{what} must be at most {max_bytes} bytes and contain no control characters"
        ));
    }
    if rendered_into_ssh_config && value.contains('"') {
        return Err(format!("{what} must not contain double quotes"));
    }
    Ok(())
}

fn is_valid_control_persist(value: &str) -> bool {
    if matches!(value, "yes" | "no") {
        return true;
    }
    let digits = value
        .strip_suffix(['s', 'm', 'h', 'd', 'w'])
        .unwrap_or(value);
    !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
}

fn reject_embedded_password(target: &str, what: &str) -> Result<(), String> {
    let authority = target.strip_prefix("ssh://").unwrap_or(target);
    if authority
        .rsplit_once('@')
        .is_some_and(|(userinfo, _)| userinfo.contains(':'))
    {
        return Err(format!("{what} must not contain a password"));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EndpointCatalog {
    version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) selected_profile: Option<ProfileId>,
    #[serde(default)]
    pub(crate) ssh: Vec<SavedSshEndpoint>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EndpointSelection {
    version: u32,
    selected_profile: Option<ProfileId>,
}

impl Default for EndpointCatalog {
    fn default() -> Self {
        Self {
            version: CATALOG_VERSION,
            selected_profile: None,
            ssh: Vec::new(),
        }
    }
}

impl EndpointCatalog {
    pub(crate) fn load() -> Result<Self, String> {
        Self::load_from_paths(&catalog_path(), &selection_path())
    }

    pub(crate) fn load_profiles() -> Result<Vec<SavedSshEndpoint>, String> {
        // Live clients keep their own selection, independent of other attached clients.
        Self::load_from_path(&catalog_path()).map(|catalog| catalog.ssh)
    }

    fn load_from_paths(catalog_path: &Path, selection_path: &Path) -> Result<Self, String> {
        let mut catalog = Self::load_from_path(catalog_path)?;
        match load_selection_from_path(selection_path) {
            Ok(Some(selection)) => {
                let valid = selection.selected_profile.as_ref().is_none_or(|selected| {
                    catalog
                        .ssh
                        .iter()
                        .any(|profile| &profile.id == selected && profile.enabled)
                });
                if valid {
                    catalog.selected_profile = selection.selected_profile;
                } else {
                    tracing::warn!(
                        path = %selection_path.display(),
                        "saved endpoint selection is absent or disabled; using Local"
                    );
                }
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(
                    %error,
                    path = %selection_path.display(),
                    "saved endpoint selection is unavailable; using Local"
                );
            }
        }
        Ok(catalog)
    }

    pub(crate) fn store_profiles(&self) -> Result<(), String> {
        self.store_to_path(&catalog_path())
    }

    pub(crate) fn store_selection(&self) -> Result<(), String> {
        self.store_selection_to_path(&selection_path())
    }

    fn store_selection_to_path(&self, path: &Path) -> Result<(), String> {
        self.validate()?;
        let content = serde_json::to_vec_pretty(&EndpointSelection {
            version: SELECTION_VERSION,
            selected_profile: self.selected_profile.clone(),
        })
        .map_err(|error| format!("failed to encode endpoint selection: {error}"))?;
        store_private_json(path, &content, "endpoint selection")
    }

    #[cfg(test)]
    pub(crate) fn add_ssh(
        &mut self,
        label: impl Into<String>,
        target: impl Into<String>,
        session: impl Into<String>,
    ) -> Result<ProfileId, String> {
        self.add_ssh_with_options(label, target, session, SshProfileOptions::default())
    }

    pub(crate) fn add_ssh_with_options(
        &mut self,
        label: impl Into<String>,
        target: impl Into<String>,
        session: impl Into<String>,
        options: SshProfileOptions,
    ) -> Result<ProfileId, String> {
        if self.ssh.len() >= MAX_PROFILES {
            return Err(format!("at most {MAX_PROFILES} SSH endpoints can be saved"));
        }
        let profile = SavedSshEndpoint::with_options(label, target, session, options)?;
        self.validate_proxy_jump_references(&profile)?;
        let id = profile.id.clone();
        self.ssh.push(profile);
        Ok(id)
    }

    fn validate_proxy_jump_references(&self, profile: &SavedSshEndpoint) -> Result<(), String> {
        for hop in &profile.proxy_jump {
            let ProxyJumpHop::Profile(hop_id) = hop else {
                continue;
            };
            if hop_id == &profile.id {
                return Err("SSH proxy jump cannot reference the profile itself".into());
            }
            if !self.ssh.iter().any(|saved| &saved.id == hop_id) {
                return Err(format!(
                    "SSH proxy jump references unknown endpoint profile {hop_id}"
                ));
            }
        }
        Ok(())
    }

    /// Replaces the editable fields of an existing profile. `id` and `target`
    /// are identity and stay immutable; `enabled` is managed separately.
    pub(crate) fn update_ssh(
        &mut self,
        id: &ProfileId,
        label: impl Into<String>,
        session: impl Into<String>,
        options: SshProfileOptions,
    ) -> Result<bool, String> {
        let Some(index) = self.ssh.iter().position(|profile| &profile.id == id) else {
            return Ok(false);
        };
        let mut updated = self.ssh[index].clone();
        updated.label = label.into();
        updated.session = session.into();
        updated.group = options.group;
        updated.tags = options.tags;
        updated.color = options.color;
        updated.port = options.port;
        updated.user = options.user;
        updated.identity_file = options.identity_file;
        updated.identities_only = options.identities_only;
        updated.identity_agent = options.identity_agent;
        updated.strict_host_key_checking = options.strict_host_key_checking;
        updated.proxy_jump = options.proxy_jump;
        updated.forward_agent = options.forward_agent;
        updated.server_alive_interval = options.server_alive_interval;
        updated.server_alive_count_max = options.server_alive_count_max;
        updated.control_persist = options.control_persist;
        updated.remote_command = options.remote_command;
        updated.session_log = options.session_log;
        updated.validate()?;
        self.validate_proxy_jump_references(&updated)?;
        self.ssh[index] = updated;
        Ok(true)
    }

    /// Replaces the port forward rules of an existing profile. Rules are
    /// managed independently of the connection fields so forwarding changes
    /// never force a reconnect.
    pub(crate) fn set_port_forwards(
        &mut self,
        id: &ProfileId,
        port_forwards: Vec<PortForwardRule>,
    ) -> Result<bool, String> {
        let Some(index) = self.ssh.iter().position(|profile| &profile.id == id) else {
            return Ok(false);
        };
        let mut updated = self.ssh[index].clone();
        updated.port_forwards = port_forwards;
        updated.validate()?;
        self.ssh[index] = updated;
        Ok(true)
    }

    /// Replaces the session-log preferences of an existing profile. Like
    /// port forwards, logging is managed independently of the connection
    /// fields so toggling it never forces a reconnect.
    pub(crate) fn set_session_log(
        &mut self,
        id: &ProfileId,
        session_log: Option<SessionLogProfile>,
    ) -> Result<bool, String> {
        let Some(index) = self.ssh.iter().position(|profile| &profile.id == id) else {
            return Ok(false);
        };
        let mut updated = self.ssh[index].clone();
        updated.session_log = session_log;
        updated.validate()?;
        self.ssh[index] = updated;
        Ok(true)
    }

    pub(crate) fn rename_ssh(
        &mut self,
        id: &ProfileId,
        label: impl Into<String>,
    ) -> Result<bool, String> {
        let Some(index) = self.ssh.iter().position(|profile| &profile.id == id) else {
            return Ok(false);
        };
        let mut renamed = self.ssh[index].clone();
        renamed.label = label.into();
        renamed.validate()?;
        self.ssh[index] = renamed;
        Ok(true)
    }

    pub(crate) fn remove_ssh(&mut self, id: &ProfileId) -> bool {
        let previous_len = self.ssh.len();
        self.ssh.retain(|profile| &profile.id != id);
        if self.selected_profile.as_ref() == Some(id) {
            self.selected_profile = None;
        }
        self.ssh.len() != previous_len
    }

    pub(crate) fn select_local(&mut self) {
        self.selected_profile = None;
    }

    pub(crate) fn select_endpoint(&mut self, endpoint_id: &super::ClientEndpointId) -> bool {
        match endpoint_id {
            super::ClientEndpointId::Local => {
                self.select_local();
                true
            }
            super::ClientEndpointId::Ssh(profile_id) => self.select_ssh(profile_id),
        }
    }

    pub(crate) fn select_ssh(&mut self, id: &ProfileId) -> bool {
        if !self
            .ssh
            .iter()
            .any(|profile| &profile.id == id && profile.enabled)
        {
            return false;
        }
        self.selected_profile = Some(id.clone());
        true
    }

    pub(crate) fn has_enabled_ssh(&self) -> bool {
        self.ssh.iter().any(|profile| profile.enabled)
    }

    pub(crate) fn contains_enabled_target_session(&self, target: &str, session: &str) -> bool {
        self.ssh.iter().any(|profile| {
            profile.enabled && profile.target == target && profile.session == session
        })
    }

    pub(crate) fn set_enabled(&mut self, id: &ProfileId, enabled: bool) -> bool {
        let Some(profile) = self.ssh.iter_mut().find(|profile| &profile.id == id) else {
            return false;
        };
        profile.enabled = enabled;
        if !enabled && self.selected_profile.as_ref() == Some(id) {
            self.selected_profile = None;
        }
        true
    }

    fn validate(&self) -> Result<(), String> {
        if self.version != CATALOG_VERSION {
            return Err(format!(
                "unsupported endpoint catalog version {}; expected {CATALOG_VERSION}",
                self.version
            ));
        }
        if self.ssh.len() > MAX_PROFILES {
            return Err(format!(
                "endpoint catalog contains more than {MAX_PROFILES} SSH profiles"
            ));
        }
        let mut ids = HashSet::new();
        for profile in &self.ssh {
            profile.validate()?;
            if !ids.insert(profile.id.clone()) {
                return Err(format!("duplicate endpoint profile id {}", profile.id));
            }
        }
        if self.selected_profile.as_ref().is_some_and(|selected| {
            !self
                .ssh
                .iter()
                .any(|profile| &profile.id == selected && profile.enabled)
        }) {
            return Err("selected SSH endpoint is absent or disabled in the catalog".into());
        }
        Ok(())
    }

    fn load_from_path(path: &Path) -> Result<Self, String> {
        let file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => {
                return Err(format!(
                    "failed to open endpoint catalog {}: {error}",
                    path.display()
                ))
            }
        };
        let metadata = file
            .metadata()
            .map_err(|error| format!("failed to inspect endpoint catalog: {error}"))?;
        if metadata.len() > MAX_CATALOG_BYTES {
            return Err("endpoint catalog exceeds the storage limit".into());
        }
        let mut content = String::new();
        file.take(MAX_CATALOG_BYTES + 1)
            .read_to_string(&mut content)
            .map_err(|error| format!("failed to read endpoint catalog: {error}"))?;
        if content.len() as u64 > MAX_CATALOG_BYTES {
            return Err("endpoint catalog exceeds the storage limit".into());
        }
        let mut catalog: Self = serde_json::from_str(&content)
            .map_err(|error| format!("stored endpoint catalog is invalid: {error}"))?;
        catalog.migrate()?;
        catalog.validate()?;
        Ok(catalog)
    }

    /// Accepts v1 catalogs and upgrades them in memory; the next store writes
    /// the current version. Newer files are rejected so a downgrade never
    /// silently drops fields it cannot read.
    fn migrate(&mut self) -> Result<(), String> {
        match self.version {
            LEGACY_CATALOG_VERSION_V1 => {
                self.version = CATALOG_VERSION;
                Ok(())
            }
            CATALOG_VERSION => Ok(()),
            other => Err(format!(
                "unsupported endpoint catalog version {other}; expected {LEGACY_CATALOG_VERSION_V1} or {CATALOG_VERSION}"
            )),
        }
    }

    fn store_to_path(&self, path: &Path) -> Result<(), String> {
        self.validate()?;
        let content = serde_json::to_vec_pretty(self)
            .map_err(|error| format!("failed to encode endpoint catalog: {error}"))?;
        store_private_json(path, &content, "endpoint catalog")
    }
}

fn load_selection_from_path(path: &Path) -> Result<Option<EndpointSelection>, String> {
    let content = match std::fs::read(path) {
        Ok(content) => content,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("failed to read endpoint selection: {error}")),
    };
    if content.len() as u64 > MAX_CATALOG_BYTES {
        return Err("endpoint selection exceeds the storage limit".into());
    }
    let selection: EndpointSelection = serde_json::from_slice(&content)
        .map_err(|error| format!("stored endpoint selection is invalid: {error}"))?;
    if selection.version != SELECTION_VERSION {
        return Err(format!(
            "unsupported endpoint selection version {}; expected {SELECTION_VERSION}",
            selection.version
        ));
    }
    Ok(Some(selection))
}

pub(crate) fn store_private_json(
    path: &Path,
    content: &[u8],
    description: &str,
) -> Result<(), String> {
    if content.len() as u64 > MAX_CATALOG_BYTES {
        return Err(format!("{description} exceeds the storage limit"));
    }
    let parent = path
        .parent()
        .ok_or_else(|| format!("invalid {description} path: {}", path.display()))?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("failed to create {description} directory: {error}"))?;
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(format!(
                "refusing to replace {description} through a non-file path"
            ));
        }
    }

    let sequence = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
    let temp_path = parent.join(format!(".endpoints-{}-{sequence}.tmp", std::process::id()));
    let mut temp = crate::platform::create_private_state_file(&temp_path)
        .map_err(|error| format!("failed to create {description}: {error}"))?;
    if let Err(error) = temp.write_all(content).and_then(|()| temp.sync_all()) {
        drop(temp);
        let _ = std::fs::remove_file(&temp_path);
        return Err(format!("failed to write {description}: {error}"));
    }
    drop(temp);
    if let Err(error) = crate::platform::replace_file(&temp_path, path) {
        let _ = std::fs::remove_file(&temp_path);
        return Err(format!("failed to activate {description}: {error}"));
    }
    crate::platform::sync_parent_directory(parent)
        .map_err(|error| format!("failed to persist {description} directory: {error}"))
}

pub(crate) fn catalog_path() -> PathBuf {
    crate::config::state_dir()
        .join("client")
        .join("endpoints.json")
}

fn selection_path() -> PathBuf {
    crate::config::state_dir()
        .join("client")
        .join("endpoint-selection.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(name: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!(
                "herdr-endpoint-catalog-{}-{name}",
                std::process::id()
            ))
            .join("endpoints.json")
    }

    #[test]
    fn catalog_roundtrip_persists_profiles_without_secret_fields() {
        let path = path("roundtrip");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        let mut catalog = EndpointCatalog::default();
        let id = catalog
            .add_ssh("Build", "ssh://dev@build.example:2222", "agents")
            .unwrap();
        assert!(catalog.select_ssh(&id));
        catalog.store_to_path(&path).unwrap();

        let encoded = std::fs::read_to_string(&path).unwrap();
        assert!(!encoded.contains("password"));
        assert!(!encoded.contains("private_key"));
        assert!(!encoded.contains("control_socket"));
        let loaded = EndpointCatalog::load_from_path(&path).unwrap();
        assert_eq!(loaded, catalog);
        assert_eq!(loaded.ssh[0].id, id);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn duplicate_target_and_session_profiles_keep_distinct_opaque_ids() {
        let mut catalog = EndpointCatalog::default();
        let first = catalog.add_ssh("One", "build", "default").unwrap();
        let second = catalog.add_ssh("Two", "build", "default").unwrap();
        assert_ne!(first, second);
    }

    #[test]
    fn catalog_rejects_passwords_embedded_in_ssh_targets() {
        let mut catalog = EndpointCatalog::default();
        assert!(catalog
            .add_ssh("Build", "ssh://dev:secret@build.example", "default")
            .unwrap_err()
            .contains("must not contain a password"));
        assert!(catalog
            .add_ssh("Build", "dev:secret@build.example", "default")
            .is_err());
        assert!(catalog
            .add_ssh("Build", "ssh://dev@[::1]:2222", "default")
            .is_ok());
    }

    #[test]
    fn interactive_bootstrap_matches_only_enabled_target_and_session() {
        let mut catalog = EndpointCatalog::default();
        let id = catalog.add_ssh("Build", "build", "agents").unwrap();
        assert!(catalog.contains_enabled_target_session("build", "agents"));
        assert!(!catalog.contains_enabled_target_session("build", "default"));
        assert!(catalog.set_enabled(&id, false));
        assert!(!catalog.contains_enabled_target_session("build", "agents"));
    }

    #[test]
    fn rename_changes_only_the_machine_label() {
        let mut catalog = EndpointCatalog::default();
        let id = catalog.add_ssh("Old", "build", "agents").unwrap();
        let original = catalog.ssh[0].clone();

        assert!(catalog.rename_ssh(&id, "New").unwrap());
        assert_eq!(catalog.ssh[0].label, "New");
        assert_eq!(catalog.ssh[0].id, original.id);
        assert_eq!(catalog.ssh[0].target, original.target);
        assert_eq!(catalog.ssh[0].session, original.session);
        assert!(catalog.rename_ssh(&id, "\n").is_err());
        assert_eq!(catalog.ssh[0].label, "New");
    }

    #[test]
    fn removal_and_disable_return_selection_to_local() {
        let mut catalog = EndpointCatalog::default();
        let first = catalog.add_ssh("One", "one", "default").unwrap();
        assert!(catalog.select_ssh(&first));
        assert!(catalog.set_enabled(&first, false));
        assert_eq!(catalog.selected_profile, None);

        assert!(catalog.set_enabled(&first, true));
        assert!(catalog.select_ssh(&first));
        assert!(catalog.remove_ssh(&first));
        assert_eq!(catalog.selected_profile, None);
    }

    #[test]
    fn catalog_rejects_unknown_fields_instead_of_retaining_possible_secrets() {
        let path = path("unknown-field");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{
              "version": 1,
              "ssh": [{
                "id": "0123456789abcdef0123456789abcdef",
                "label": "Build",
                "target": "build",
                "session": "default",
                "enabled": true,
                "password": "must-not-be-accepted"
              }]
            }"#,
        )
        .unwrap();
        assert!(EndpointCatalog::load_from_path(&path)
            .unwrap_err()
            .contains("unknown field"));
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn storing_selection_does_not_rewrite_profile_membership() {
        let catalog_path = path("separate-selection");
        let selection_path = catalog_path.with_file_name("selection.json");
        let _ = std::fs::remove_dir_all(catalog_path.parent().unwrap());
        let mut catalog = EndpointCatalog::default();
        let id = catalog.add_ssh("Build", "build", "agents").unwrap();
        catalog.store_to_path(&catalog_path).unwrap();
        let profiles_before = std::fs::read(&catalog_path).unwrap();

        assert!(catalog.select_ssh(&id));
        catalog.store_selection_to_path(&selection_path).unwrap();

        assert_eq!(std::fs::read(&catalog_path).unwrap(), profiles_before);
        assert_eq!(
            load_selection_from_path(&selection_path)
                .unwrap()
                .unwrap()
                .selected_profile,
            Some(id)
        );
        std::fs::remove_dir_all(catalog_path.parent().unwrap()).unwrap();
    }

    #[test]
    fn malformed_selection_does_not_discard_saved_profiles() {
        let catalog_path = path("malformed-selection");
        let selection_path = catalog_path.with_file_name("selection.json");
        let _ = std::fs::remove_dir_all(catalog_path.parent().unwrap());
        let mut catalog = EndpointCatalog::default();
        let id = catalog.add_ssh("Build", "build", "agents").unwrap();
        catalog.store_to_path(&catalog_path).unwrap();
        std::fs::write(&selection_path, b"not json").unwrap();

        let loaded = EndpointCatalog::load_from_paths(&catalog_path, &selection_path).unwrap();
        assert_eq!(loaded.ssh.len(), 1);
        assert_eq!(loaded.ssh[0].id, id);
        assert_eq!(loaded.selected_profile, None);
        std::fs::remove_dir_all(catalog_path.parent().unwrap()).unwrap();
    }

    #[test]
    fn absent_selected_profile_falls_back_without_discarding_catalog() {
        let catalog_path = path("absent-selection");
        let selection_path = catalog_path.with_file_name("selection.json");
        let _ = std::fs::remove_dir_all(catalog_path.parent().unwrap());
        let mut catalog = EndpointCatalog::default();
        let saved = catalog.add_ssh("Build", "build", "agents").unwrap();
        catalog.store_to_path(&catalog_path).unwrap();
        let missing = ProfileId::parse("fedcba9876543210fedcba9876543210").unwrap();
        store_private_json(
            &selection_path,
            &serde_json::to_vec(&EndpointSelection {
                version: SELECTION_VERSION,
                selected_profile: Some(missing),
            })
            .unwrap(),
            "endpoint selection",
        )
        .unwrap();

        let loaded = EndpointCatalog::load_from_paths(&catalog_path, &selection_path).unwrap();
        assert_eq!(loaded.ssh[0].id, saved);
        assert_eq!(loaded.selected_profile, None);
        std::fs::remove_dir_all(catalog_path.parent().unwrap()).unwrap();
    }

    #[test]
    fn invalid_or_missing_selected_profile_is_rejected() {
        let catalog = EndpointCatalog {
            selected_profile: Some(ProfileId::parse("0123456789abcdef0123456789abcdef").unwrap()),
            ..EndpointCatalog::default()
        };
        assert!(catalog.validate().is_err());
    }

    #[test]
    fn v1_catalog_loads_and_stores_as_v2() {
        let path = path("v1-migration");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{
              "version": 1,
              "ssh": [{
                "id": "0123456789abcdef0123456789abcdef",
                "label": "Build",
                "target": "build",
                "session": "default",
                "enabled": true
              }]
            }"#,
        )
        .unwrap();

        let loaded = EndpointCatalog::load_from_path(&path).unwrap();
        assert_eq!(loaded.version, CATALOG_VERSION);
        assert_eq!(loaded.ssh[0].label, "Build");
        assert_eq!(loaded.ssh[0].group, None);
        assert!(loaded.ssh[0].tags.is_empty());
        assert!(!loaded.ssh[0].has_connection_options());

        loaded.store_to_path(&path).unwrap();
        let stored = std::fs::read_to_string(&path).unwrap();
        assert!(stored.contains("\"version\": 2"), "{stored}");
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn newer_catalog_versions_are_rejected() {
        let path = path("v3-rejected");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"{"version": 3, "ssh": []}"#).unwrap();
        assert!(EndpointCatalog::load_from_path(&path)
            .unwrap_err()
            .contains("unsupported endpoint catalog version 3"));
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    fn full_options() -> SshProfileOptions {
        SshProfileOptions {
            group: Some("prod".into()),
            tags: vec!["ci".into(), "eu".into()],
            color: Some("#1a2b3c".into()),
            port: Some(2222),
            user: Some("dev".into()),
            identity_file: vec!["~/.ssh/build".into()],
            identities_only: Some(true),
            identity_agent: Some("~/.ssh/agent.sock".into()),
            strict_host_key_checking: Some(StrictHostKeyChecking::AcceptNew),
            proxy_jump: vec![ProxyJumpHop::Target("bastion".into())],
            forward_agent: Some(true),
            server_alive_interval: Some(30),
            server_alive_count_max: Some(2),
            control_persist: Some("10m".into()),
            remote_command: Some("tmux attach".into()),
            session_log: None,
        }
    }

    #[test]
    fn catalog_roundtrip_preserves_connection_and_presentation_options() {
        let path = path("v2-roundtrip");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        let mut catalog = EndpointCatalog::default();
        let jump = catalog.add_ssh("Jump", "bastion", "default").unwrap();
        let mut options = full_options();
        options.proxy_jump.push(ProxyJumpHop::Profile(jump));
        let id = catalog
            .add_ssh_with_options("Build", "build", "agents", options)
            .unwrap();
        catalog.store_to_path(&path).unwrap();

        let stored = std::fs::read_to_string(&path).unwrap();
        assert!(stored.contains("\"version\": 2"), "{stored}");
        assert!(!stored.contains("password"));
        let loaded = EndpointCatalog::load_from_path(&path).unwrap();
        assert_eq!(loaded, catalog);
        let profile = loaded.ssh.iter().find(|p| p.id == id).unwrap();
        assert!(profile.has_connection_options());
        assert_eq!(profile.proxy_jump.len(), 2);
        assert_eq!(
            profile.strict_host_key_checking,
            Some(StrictHostKeyChecking::AcceptNew)
        );
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    fn forward_rule(kind: PortForwardKind) -> PortForwardRule {
        let (target_host, target_port) = match kind {
            PortForwardKind::Local | PortForwardKind::Remote => {
                (Some("127.0.0.1".to_string()), Some(5432))
            }
            PortForwardKind::Dynamic => (None, None),
        };
        PortForwardRule {
            kind,
            bind_address: None,
            listen_port: 15432,
            target_host,
            target_port,
        }
    }

    #[test]
    fn port_forward_rules_roundtrip_and_stay_out_of_connection_options() {
        let path = path("forwards-roundtrip");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        let mut catalog = EndpointCatalog::default();
        let id = catalog.add_ssh("Build", "build", "default").unwrap();
        let rules = vec![
            forward_rule(PortForwardKind::Local),
            forward_rule(PortForwardKind::Remote),
            PortForwardRule {
                bind_address: Some("127.0.0.1".into()),
                ..forward_rule(PortForwardKind::Dynamic)
            },
        ];
        assert!(catalog.set_port_forwards(&id, rules).unwrap());
        catalog.store_to_path(&path).unwrap();

        let stored = std::fs::read_to_string(&path).unwrap();
        assert!(stored.contains("\"port_forwards\""), "{stored}");
        assert!(stored.contains("\"kind\": \"local\""), "{stored}");
        let loaded = EndpointCatalog::load_from_path(&path).unwrap();
        assert_eq!(loaded, catalog);
        let profile = &loaded.ssh[0];
        assert_eq!(profile.port_forwards.len(), 3);
        assert!(
            !profile.has_connection_options(),
            "port forwards must not change how the connection is built"
        );
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn catalogs_without_port_forwards_load_with_an_empty_rule_set() {
        let path = path("forwards-absent");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{
              "version": 2,
              "ssh": [{
                "id": "0123456789abcdef0123456789abcdef",
                "label": "Build",
                "target": "build",
                "session": "default",
                "enabled": true
              }]
            }"#,
        )
        .unwrap();

        let loaded = EndpointCatalog::load_from_path(&path).unwrap();
        assert!(loaded.ssh[0].port_forwards.is_empty());
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn catalogs_without_session_log_load_with_no_logging() {
        let path = path("session-log-absent");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{
              "version": 2,
              "ssh": [{
                "id": "0123456789abcdef0123456789abcdef",
                "label": "Build",
                "target": "build",
                "session": "default",
                "enabled": true
              }]
            }"#,
        )
        .unwrap();

        let loaded = EndpointCatalog::load_from_path(&path).unwrap();
        assert_eq!(loaded.ssh[0].session_log, None);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn session_log_preferences_round_trip_and_validate() {
        let mut catalog = EndpointCatalog::default();
        let id = catalog.add_ssh("Build", "build", "default").unwrap();
        let config = SessionLogProfile {
            enabled: true,
            path_template: Some("{machine}/{date}-{pane}.log".into()),
            max_bytes: Some(1024 * 1024),
            dump_interval_secs: Some(60),
        };
        assert!(catalog.set_session_log(&id, Some(config.clone())).unwrap());
        assert_eq!(catalog.ssh[0].session_log, Some(config));

        for (bad, needle) in [
            (
                SessionLogProfile {
                    enabled: true,
                    path_template: Some("{unknown}.log".into()),
                    ..SessionLogProfile::default()
                },
                "not supported",
            ),
            (
                SessionLogProfile {
                    enabled: true,
                    path_template: Some(String::new()),
                    ..SessionLogProfile::default()
                },
                "cannot be empty",
            ),
            (
                SessionLogProfile {
                    max_bytes: Some(10),
                    ..SessionLogProfile::default()
                },
                "size limit",
            ),
            (
                SessionLogProfile {
                    dump_interval_secs: Some(1),
                    ..SessionLogProfile::default()
                },
                "interval",
            ),
        ] {
            let error = catalog.set_session_log(&id, Some(bad)).unwrap_err();
            assert!(error.contains(needle), "{error} must mention {needle}");
        }

        let missing = ProfileId::parse("ffffffffffffffffffffffffffffffff").unwrap();
        assert!(!catalog
            .set_session_log(&missing, Some(SessionLogProfile::default()))
            .unwrap());
        // Clearing returns to the absent-field shape.
        assert!(catalog.set_session_log(&id, None).unwrap());
        assert_eq!(catalog.ssh[0].session_log, None);
    }

    #[test]
    fn session_log_does_not_count_as_a_connection_option() {
        let profile = SavedSshEndpoint::with_options(
            "Build",
            "build",
            "default",
            SshProfileOptions {
                session_log: Some(SessionLogProfile {
                    enabled: true,
                    ..SessionLogProfile::default()
                }),
                ..SshProfileOptions::default()
            },
        )
        .expect("valid profile");
        assert!(
            !profile.has_connection_options(),
            "client-local logging must not alter how the connection is built"
        );
    }

    #[test]
    fn port_forward_rules_are_validated() {
        let cases: Vec<(PortForwardRule, &str)> = vec![
            (
                PortForwardRule {
                    listen_port: 0,
                    ..forward_rule(PortForwardKind::Local)
                },
                "listen port must be between",
            ),
            (
                PortForwardRule {
                    target_host: None,
                    ..forward_rule(PortForwardKind::Local)
                },
                "requires a target host",
            ),
            (
                PortForwardRule {
                    target_port: None,
                    ..forward_rule(PortForwardKind::Local)
                },
                "target port must be between",
            ),
            (
                PortForwardRule {
                    target_port: Some(0),
                    ..forward_rule(PortForwardKind::Remote)
                },
                "target port must be between",
            ),
            (
                PortForwardRule {
                    target_host: None,
                    ..forward_rule(PortForwardKind::Remote)
                },
                "requires a target host",
            ),
            (
                PortForwardRule {
                    target_host: Some("db".into()),
                    ..forward_rule(PortForwardKind::Dynamic)
                },
                "must not have a target",
            ),
            (
                PortForwardRule {
                    target_port: Some(5432),
                    ..forward_rule(PortForwardKind::Dynamic)
                },
                "must not have a target",
            ),
            (
                PortForwardRule {
                    bind_address: Some("127.0.0.1 ".into()),
                    ..forward_rule(PortForwardKind::Local)
                },
                "no control characters or whitespace",
            ),
            (
                PortForwardRule {
                    bind_address: Some("-bind".into()),
                    ..forward_rule(PortForwardKind::Local)
                },
                "must not start with '-'",
            ),
            (
                PortForwardRule {
                    target_host: Some("db\ninternal".into()),
                    ..forward_rule(PortForwardKind::Local)
                },
                "no control characters or whitespace",
            ),
        ];
        for (rule, expected) in cases {
            let mut catalog = EndpointCatalog::default();
            let id = catalog.add_ssh("Build", "build", "default").unwrap();
            let error = catalog.set_port_forwards(&id, vec![rule]).unwrap_err();
            assert!(
                error.contains(expected),
                "{error} should contain {expected}"
            );
        }
    }

    #[test]
    fn port_forward_rule_count_is_capped() {
        let mut catalog = EndpointCatalog::default();
        let id = catalog.add_ssh("Build", "build", "default").unwrap();
        let rules = vec![forward_rule(PortForwardKind::Dynamic); MAX_PORT_FORWARDS + 1];
        assert!(catalog
            .set_port_forwards(&id, rules)
            .unwrap_err()
            .contains("at most 16 port forward rules"));
    }

    #[test]
    fn set_port_forwards_replaces_rules_and_tracks_unknown_profiles() {
        let mut catalog = EndpointCatalog::default();
        let id = catalog.add_ssh("Build", "build", "default").unwrap();
        assert!(catalog
            .set_port_forwards(&id, vec![forward_rule(PortForwardKind::Local)])
            .unwrap());
        assert!(catalog.set_port_forwards(&id, Vec::new()).unwrap());
        assert!(catalog.ssh[0].port_forwards.is_empty());
        let missing = ProfileId::parse("fedcba9876543210fedcba9876543210").unwrap();
        assert!(!catalog
            .set_port_forwards(&missing, vec![forward_rule(PortForwardKind::Local)])
            .unwrap());
    }

    #[test]
    fn update_ssh_preserves_existing_port_forward_rules() {
        let mut catalog = EndpointCatalog::default();
        let id = catalog.add_ssh("Build", "build", "default").unwrap();
        assert!(catalog
            .set_port_forwards(&id, vec![forward_rule(PortForwardKind::Local)])
            .unwrap());
        assert!(catalog
            .update_ssh(&id, "Renamed", "agents", SshProfileOptions::default())
            .unwrap());
        assert_eq!(catalog.ssh[0].port_forwards.len(), 1);
    }

    #[test]
    fn default_options_serialize_to_the_v1_shape() {
        let mut catalog = EndpointCatalog::default();
        catalog.add_ssh("Build", "build", "default").unwrap();
        let encoded = serde_json::to_string(&catalog.ssh[0]).unwrap();
        for key in [
            "group",
            "tags",
            "color",
            "port",
            "user",
            "identity_file",
            "identities_only",
            "identity_agent",
            "strict_host_key_checking",
            "proxy_jump",
            "forward_agent",
            "server_alive_interval",
            "server_alive_count_max",
            "control_persist",
            "remote_command",
            "port_forwards",
        ] {
            assert!(
                !encoded.contains(key),
                "default profile must omit {key}: {encoded}"
            );
        }
    }

    #[test]
    fn proxy_jump_profile_references_must_exist_and_not_self_reference() {
        let mut catalog = EndpointCatalog::default();
        let jump = catalog.add_ssh("Jump", "bastion", "default").unwrap();
        assert!(catalog
            .add_ssh_with_options(
                "Build",
                "build",
                "default",
                SshProfileOptions {
                    proxy_jump: vec![ProxyJumpHop::Profile(jump)],
                    ..SshProfileOptions::default()
                },
            )
            .is_ok());
        let missing = ProfileId::parse("fedcba9876543210fedcba9876543210").unwrap();
        assert!(catalog
            .add_ssh_with_options(
                "Build",
                "build",
                "default",
                SshProfileOptions {
                    proxy_jump: vec![ProxyJumpHop::Profile(missing)],
                    ..SshProfileOptions::default()
                },
            )
            .unwrap_err()
            .contains("unknown endpoint profile"));
    }

    #[test]
    fn update_ssh_replaces_editable_fields_and_keeps_identity() {
        let mut catalog = EndpointCatalog::default();
        let jump = catalog.add_ssh("Jump", "bastion", "default").unwrap();
        let id = catalog.add_ssh("Build", "build", "default").unwrap();
        let target = catalog.ssh[1].target.clone();

        let options = SshProfileOptions {
            group: Some("prod".into()),
            tags: vec!["ci".into()],
            color: Some("#a1b2c3".into()),
            port: Some(2222),
            user: Some("dev".into()),
            proxy_jump: vec![ProxyJumpHop::Profile(jump)],
            ..SshProfileOptions::default()
        };
        assert!(catalog
            .update_ssh(&id, "Renamed", "agents", options)
            .unwrap());
        let profile = &catalog.ssh[1];
        assert_eq!(profile.id, id);
        assert_eq!(profile.target, target);
        assert!(profile.enabled);
        assert_eq!(profile.label, "Renamed");
        assert_eq!(profile.session, "agents");
        assert_eq!(profile.group.as_deref(), Some("prod"));
        assert_eq!(profile.port, Some(2222));
        assert!(profile.has_connection_options());

        let missing = ProfileId::parse("fedcba9876543210fedcba9876543210").unwrap();
        assert!(!catalog
            .update_ssh(&missing, "x", "default", SshProfileOptions::default())
            .unwrap());
        assert!(catalog
            .update_ssh(&id, "  ", "default", SshProfileOptions::default())
            .is_err());
        assert!(catalog
            .update_ssh(
                &id,
                "Build",
                "default",
                SshProfileOptions {
                    proxy_jump: vec![ProxyJumpHop::Profile(id.clone())],
                    ..SshProfileOptions::default()
                },
            )
            .is_err());
    }

    #[test]
    fn option_fields_are_validated() {
        let cases: Vec<(SshProfileOptions, &str)> = vec![
            (
                SshProfileOptions {
                    group: Some("  ".into()),
                    ..SshProfileOptions::default()
                },
                "group cannot be empty",
            ),
            (
                SshProfileOptions {
                    tags: vec!["ok".into(); MAX_TAGS + 1],
                    ..SshProfileOptions::default()
                },
                "at most 16 tags",
            ),
            (
                SshProfileOptions {
                    color: Some("not-a-color".into()),
                    ..SshProfileOptions::default()
                },
                "color must be",
            ),
            (
                SshProfileOptions {
                    port: Some(0),
                    ..SshProfileOptions::default()
                },
                "port must be between",
            ),
            (
                SshProfileOptions {
                    user: Some("dev ops".into()),
                    ..SshProfileOptions::default()
                },
                "user must not contain whitespace",
            ),
            (
                SshProfileOptions {
                    identity_file: vec!["~/.ssh/\"quoted\"".into()],
                    ..SshProfileOptions::default()
                },
                "must not contain double quotes",
            ),
            (
                SshProfileOptions {
                    proxy_jump: vec![ProxyJumpHop::Target("-ProxyCommand=bad".into())],
                    ..SshProfileOptions::default()
                },
                "must not start with '-'",
            ),
            (
                SshProfileOptions {
                    proxy_jump: vec![ProxyJumpHop::Target("dev:secret@bastion".into())],
                    ..SshProfileOptions::default()
                },
                "must not contain a password",
            ),
            (
                SshProfileOptions {
                    control_persist: Some("whenever".into()),
                    ..SshProfileOptions::default()
                },
                "control persist must be",
            ),
            (
                SshProfileOptions {
                    remote_command: Some("echo hi\nrm -rf /".into()),
                    ..SshProfileOptions::default()
                },
                "no control characters",
            ),
        ];
        for (options, expected) in cases {
            let mut catalog = EndpointCatalog::default();
            let error = catalog
                .add_ssh_with_options("Build", "build", "default", options)
                .unwrap_err();
            assert!(
                error.contains(expected),
                "{error} should contain {expected}"
            );
        }
        for color in [
            "red",
            "lightblue",
            "#abc",
            "#a1b2c3",
            "rgb(1,2,3)",
            "default",
        ] {
            let mut catalog = EndpointCatalog::default();
            assert!(
                catalog
                    .add_ssh_with_options(
                        "Build",
                        "build",
                        "default",
                        SshProfileOptions {
                            color: Some(color.into()),
                            ..SshProfileOptions::default()
                        },
                    )
                    .is_ok(),
                "color {color} should be accepted"
            );
        }
    }

    #[test]
    fn catalog_accepts_256_profiles_and_rejects_the_257th() {
        let mut catalog = EndpointCatalog::default();
        for index in 0..MAX_PROFILES {
            catalog
                .add_ssh(format!("m{index}"), "build", "default")
                .unwrap();
        }
        assert!(catalog
            .add_ssh("one-too-many", "build", "default")
            .unwrap_err()
            .contains("at most 256"));
    }
}
