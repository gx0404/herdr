//! 机器表单：添加向导（Target / Connection / Session / Confirm）与编辑表单的
//! 状态机、远程 bootstrap 进度与渲染。

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::client::shell) enum MachineFormStep {
    Target,
    Connection,
    Session,
    Confirm,
}

impl MachineFormStep {
    const ALL: [Self; 4] = [Self::Target, Self::Connection, Self::Session, Self::Confirm];

    fn label(self) -> &'static str {
        let t = &crate::i18n::texts().machines;
        match self {
            Self::Target => t.step_target,
            Self::Connection => t.step_connection,
            Self::Session => t.step_session,
            Self::Confirm => t.step_confirm,
        }
    }

    pub(super) fn previous(self) -> Self {
        match self {
            Self::Target => Self::Target,
            Self::Connection => Self::Target,
            Self::Session => Self::Connection,
            Self::Confirm => Self::Session,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::client::shell) enum TriChoice {
    Default,
    Yes,
    No,
}

impl TriChoice {
    fn cycle(self, delta: isize) -> Self {
        const ALL: [TriChoice; 3] = [TriChoice::Default, TriChoice::Yes, TriChoice::No];
        let index = ALL.iter().position(|choice| *choice == self).unwrap_or(0);
        ALL[(index as isize + delta).rem_euclid(ALL.len() as isize) as usize]
    }

    fn as_bool(self) -> Option<bool> {
        match self {
            Self::Default => None,
            Self::Yes => Some(true),
            Self::No => Some(false),
        }
    }

    fn from_bool(value: Option<bool>) -> Self {
        match value {
            None => Self::Default,
            Some(true) => Self::Yes,
            Some(false) => Self::No,
        }
    }

    fn label(self) -> &'static str {
        let t = &crate::i18n::texts().machines;
        match self {
            Self::Default => t.choice_default,
            Self::Yes => t.choice_yes,
            Self::No => t.choice_no,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::client::shell) enum MachineField {
    Target,
    Label,
    Session,
    Group,
    Tags,
    Color,
    User,
    Port,
    IdentityFiles,
    IdentityAgent,
    IdentitiesOnly,
    StrictHostKey,
    ProxyJump,
    ForwardAgent,
    ServerAliveInterval,
    ServerAliveCountMax,
    ControlPersist,
    RemoteCommand,
    SessionLogEnabled,
    SessionLogPath,
    SessionLogMaxBytes,
    SessionLogInterval,
}

impl MachineField {
    fn label(self) -> &'static str {
        let t = &crate::i18n::texts().machines;
        match self {
            Self::Target => t.detail_target,
            Self::Label => t.field_label,
            Self::Session => t.detail_session,
            Self::Group => t.detail_group,
            Self::Tags => t.detail_tags,
            Self::Color => t.detail_color,
            Self::User => t.detail_user,
            Self::Port => t.detail_port,
            Self::IdentityFiles => t.detail_identity_files,
            Self::IdentityAgent => t.detail_identity_agent,
            Self::IdentitiesOnly => t.detail_identities_only,
            Self::StrictHostKey => t.detail_strict_host_key,
            Self::ProxyJump => t.detail_proxy_jump,
            Self::ForwardAgent => t.detail_forward_agent,
            Self::ServerAliveInterval => t.detail_server_alive_interval,
            Self::ServerAliveCountMax => t.detail_server_alive_count_max,
            Self::ControlPersist => t.detail_control_persist,
            Self::RemoteCommand => t.detail_remote_command,
            Self::SessionLogEnabled => t.field_session_log_enabled,
            Self::SessionLogPath => t.field_session_log_path,
            Self::SessionLogMaxBytes => t.field_session_log_max_bytes,
            Self::SessionLogInterval => t.field_session_log_interval,
        }
    }

    fn hint(self) -> Option<&'static str> {
        let t = &crate::i18n::texts().machines;
        match self {
            Self::IdentityFiles => Some(t.hint_identity_files),
            Self::ProxyJump => Some(t.hint_proxy_jump),
            Self::Color => Some(t.hint_color),
            Self::SessionLogPath => Some(t.hint_session_log_path),
            _ => None,
        }
    }

    fn is_choice(self) -> bool {
        matches!(
            self,
            Self::IdentitiesOnly
                | Self::StrictHostKey
                | Self::ForwardAgent
                | Self::SessionLogEnabled
        )
    }
}

const TARGET_STEP_FIELDS: &[MachineField] = &[MachineField::Target, MachineField::Label];

const CONNECTION_STEP_FIELDS: &[MachineField] = &[
    MachineField::User,
    MachineField::Port,
    MachineField::IdentityFiles,
    MachineField::IdentityAgent,
    MachineField::IdentitiesOnly,
    MachineField::StrictHostKey,
    MachineField::ProxyJump,
    MachineField::ForwardAgent,
    MachineField::ServerAliveInterval,
    MachineField::ServerAliveCountMax,
    MachineField::ControlPersist,
    MachineField::RemoteCommand,
];

const SESSION_STEP_FIELDS: &[MachineField] = &[
    MachineField::Session,
    MachineField::Group,
    MachineField::Tags,
    MachineField::Color,
];

const EDIT_FIELDS: &[MachineField] = &[
    MachineField::Label,
    MachineField::Session,
    MachineField::Group,
    MachineField::Tags,
    MachineField::Color,
    MachineField::User,
    MachineField::Port,
    MachineField::IdentityFiles,
    MachineField::IdentityAgent,
    MachineField::IdentitiesOnly,
    MachineField::StrictHostKey,
    MachineField::ProxyJump,
    MachineField::ForwardAgent,
    MachineField::ServerAliveInterval,
    MachineField::ServerAliveCountMax,
    MachineField::ControlPersist,
    MachineField::RemoteCommand,
    MachineField::SessionLogEnabled,
    MachineField::SessionLogPath,
    MachineField::SessionLogMaxBytes,
    MachineField::SessionLogInterval,
];

#[derive(Debug)]
pub(in crate::client::shell) struct ClientMachineForm {
    pub(in crate::client::shell) editing: Option<ProfileId>,
    pub(in crate::client::shell) step: MachineFormStep,
    pub(in crate::client::shell) focused: usize,
    pub(in crate::client::shell) scroll: usize,
    pub(in crate::client::shell) target: TextEditor,
    pub(in crate::client::shell) label: TextEditor,
    pub(in crate::client::shell) session: TextEditor,
    pub(in crate::client::shell) group: TextEditor,
    pub(in crate::client::shell) tags: TextEditor,
    pub(in crate::client::shell) color: TextEditor,
    pub(in crate::client::shell) user: TextEditor,
    pub(in crate::client::shell) port: TextEditor,
    pub(in crate::client::shell) identity_files: TextEditor,
    pub(in crate::client::shell) identity_agent: TextEditor,
    pub(in crate::client::shell) identities_only: TriChoice,
    /// Index into `STRICT_HOST_KEY_CHOICES`; 0 keeps the SSH default.
    pub(in crate::client::shell) strict_host_key: usize,
    pub(in crate::client::shell) proxy_jump: TextEditor,
    pub(in crate::client::shell) forward_agent: TriChoice,
    pub(in crate::client::shell) server_alive_interval: TextEditor,
    pub(in crate::client::shell) server_alive_count_max: TextEditor,
    pub(in crate::client::shell) control_persist: TextEditor,
    pub(in crate::client::shell) remote_command: TextEditor,
    /// Session log editing: `Default` keeps the saved value untouched (the
    /// text fields are ignored); Yes/No rebuilds the profile from the text
    /// fields below (empty text = unset, i.e. the mechanism default).
    pub(in crate::client::shell) session_log_enabled: TriChoice,
    pub(in crate::client::shell) session_log_path: TextEditor,
    pub(in crate::client::shell) session_log_max_bytes: TextEditor,
    pub(in crate::client::shell) session_log_interval: TextEditor,
    /// The value loaded from (and preserved by) the form when
    /// `session_log_enabled` stays `Default`.
    pub(in crate::client::shell) session_log: Option<SessionLogProfile>,
    pub(in crate::client::shell) error: Option<String>,
    pub(in crate::client::shell) bootstrap: Option<ClientMachineBootstrap>,
}

#[derive(Debug)]
pub(in crate::client::shell) struct ClientMachineBootstrap {
    pub(in crate::client::shell) cancel: crate::remote::TaskCancellation,
    pub(in crate::client::shell) ticket: u64,
    pub(in crate::client::shell) step: Option<SavedSshBootstrapStep>,
    pub(in crate::client::shell) failure: Option<String>,
}

impl Drop for ClientMachineBootstrap {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

const STRICT_HOST_KEY_CHOICES: [Option<StrictHostKeyChecking>; 4] = [
    None,
    Some(StrictHostKeyChecking::Ask),
    Some(StrictHostKeyChecking::AcceptNew),
    Some(StrictHostKeyChecking::Yes),
];

impl ClientMachineForm {
    pub(super) fn blank() -> Self {
        Self {
            editing: None,
            step: MachineFormStep::Target,
            focused: 0,
            scroll: 0,
            target: TextEditor::default(),
            label: TextEditor::default(),
            session: TextEditor::default(),
            group: TextEditor::default(),
            tags: TextEditor::default(),
            color: TextEditor::default(),
            user: TextEditor::default(),
            port: TextEditor::default(),
            identity_files: TextEditor::default(),
            identity_agent: TextEditor::default(),
            identities_only: TriChoice::Default,
            strict_host_key: 0,
            proxy_jump: TextEditor::default(),
            forward_agent: TriChoice::Default,
            server_alive_interval: TextEditor::default(),
            server_alive_count_max: TextEditor::default(),
            control_persist: TextEditor::default(),
            remote_command: TextEditor::default(),
            session_log_enabled: TriChoice::Default,
            session_log_path: TextEditor::default(),
            session_log_max_bytes: TextEditor::default(),
            session_log_interval: TextEditor::default(),
            session_log: None,
            error: None,
            bootstrap: None,
        }
    }

    pub(super) fn from_profile(profile: &SavedSshEndpoint) -> Self {
        let mut form = Self::blank();
        form.editing = Some(profile.id.clone());
        form.target = TextEditor::new(&profile.target, false);
        form.label = TextEditor::new(&profile.label, false);
        form.session = TextEditor::new(&profile.session, false);
        form.group = TextEditor::new(profile.group.as_deref().unwrap_or_default(), false);
        form.tags = TextEditor::new(&profile.tags.join(", "), false);
        form.color = TextEditor::new(profile.color.as_deref().unwrap_or_default(), false);
        form.user = TextEditor::new(profile.user.as_deref().unwrap_or_default(), false);
        form.port = TextEditor::new(
            &profile
                .port
                .map(|port| port.to_string())
                .unwrap_or_default(),
            false,
        );
        form.identity_files = TextEditor::new(&profile.identity_file.join(", "), false);
        form.identity_agent =
            TextEditor::new(profile.identity_agent.as_deref().unwrap_or_default(), false);
        form.identities_only = TriChoice::from_bool(profile.identities_only);
        form.strict_host_key = STRICT_HOST_KEY_CHOICES
            .iter()
            .position(|choice| *choice == profile.strict_host_key_checking)
            .unwrap_or(0);
        form.proxy_jump = TextEditor::new(
            &profile
                .proxy_jump
                .iter()
                .map(|hop| match hop {
                    ProxyJumpHop::Target(target) => target.clone(),
                    ProxyJumpHop::Profile(id) => format!("profile:{id}"),
                })
                .collect::<Vec<_>>()
                .join(", "),
            false,
        );
        form.forward_agent = TriChoice::from_bool(profile.forward_agent);
        form.server_alive_interval = TextEditor::new(
            &profile
                .server_alive_interval
                .map(|value| value.to_string())
                .unwrap_or_default(),
            false,
        );
        form.server_alive_count_max = TextEditor::new(
            &profile
                .server_alive_count_max
                .map(|value| value.to_string())
                .unwrap_or_default(),
            false,
        );
        form.control_persist = TextEditor::new(
            profile.control_persist.as_deref().unwrap_or_default(),
            false,
        );
        form.remote_command =
            TextEditor::new(profile.remote_command.as_deref().unwrap_or_default(), false);
        let log = profile.session_log.clone().unwrap_or_default();
        form.session_log_enabled = if profile.session_log.is_none() {
            TriChoice::Default
        } else {
            TriChoice::from_bool(Some(log.enabled))
        };
        form.session_log_path =
            TextEditor::new(log.path_template.as_deref().unwrap_or_default(), false);
        form.session_log_max_bytes = TextEditor::new(
            &log.max_bytes
                .map(|value| value.to_string())
                .unwrap_or_default(),
            false,
        );
        form.session_log_interval = TextEditor::new(
            &log.dump_interval_secs
                .map(|value| value.to_string())
                .unwrap_or_default(),
            false,
        );
        form.session_log = profile.session_log.clone();
        form
    }

    pub(super) fn fields(&self) -> &'static [MachineField] {
        if self.editing.is_some() {
            return EDIT_FIELDS;
        }
        match self.step {
            MachineFormStep::Target => TARGET_STEP_FIELDS,
            MachineFormStep::Connection => CONNECTION_STEP_FIELDS,
            MachineFormStep::Session => SESSION_STEP_FIELDS,
            MachineFormStep::Confirm => &[],
        }
    }

    pub(super) fn focused_field(&self) -> Option<MachineField> {
        self.fields().get(self.focused).copied()
    }

    fn editor(&self, field: MachineField) -> Option<&TextEditor> {
        Some(match field {
            MachineField::Target => &self.target,
            MachineField::Label => &self.label,
            MachineField::Session => &self.session,
            MachineField::Group => &self.group,
            MachineField::Tags => &self.tags,
            MachineField::Color => &self.color,
            MachineField::User => &self.user,
            MachineField::Port => &self.port,
            MachineField::IdentityFiles => &self.identity_files,
            MachineField::IdentityAgent => &self.identity_agent,
            MachineField::ProxyJump => &self.proxy_jump,
            MachineField::ServerAliveInterval => &self.server_alive_interval,
            MachineField::ServerAliveCountMax => &self.server_alive_count_max,
            MachineField::ControlPersist => &self.control_persist,
            MachineField::RemoteCommand => &self.remote_command,
            MachineField::SessionLogPath => &self.session_log_path,
            MachineField::SessionLogMaxBytes => &self.session_log_max_bytes,
            MachineField::SessionLogInterval => &self.session_log_interval,
            MachineField::IdentitiesOnly
            | MachineField::StrictHostKey
            | MachineField::ForwardAgent
            | MachineField::SessionLogEnabled => return None,
        })
    }

    pub(super) fn editor_mut(&mut self, field: MachineField) -> Option<&mut TextEditor> {
        Some(match field {
            MachineField::Target => &mut self.target,
            MachineField::Label => &mut self.label,
            MachineField::Session => &mut self.session,
            MachineField::Group => &mut self.group,
            MachineField::Tags => &mut self.tags,
            MachineField::Color => &mut self.color,
            MachineField::User => &mut self.user,
            MachineField::Port => &mut self.port,
            MachineField::IdentityFiles => &mut self.identity_files,
            MachineField::IdentityAgent => &mut self.identity_agent,
            MachineField::ProxyJump => &mut self.proxy_jump,
            MachineField::ServerAliveInterval => &mut self.server_alive_interval,
            MachineField::ServerAliveCountMax => &mut self.server_alive_count_max,
            MachineField::ControlPersist => &mut self.control_persist,
            MachineField::RemoteCommand => &mut self.remote_command,
            MachineField::SessionLogPath => &mut self.session_log_path,
            MachineField::SessionLogMaxBytes => &mut self.session_log_max_bytes,
            MachineField::SessionLogInterval => &mut self.session_log_interval,
            MachineField::IdentitiesOnly
            | MachineField::StrictHostKey
            | MachineField::ForwardAgent
            | MachineField::SessionLogEnabled => return None,
        })
    }

    fn cycle_choice(&mut self, field: MachineField, delta: isize) {
        match field {
            MachineField::IdentitiesOnly => {
                self.identities_only = self.identities_only.cycle(delta);
            }
            MachineField::ForwardAgent => {
                self.forward_agent = self.forward_agent.cycle(delta);
            }
            MachineField::SessionLogEnabled => {
                self.session_log_enabled = self.session_log_enabled.cycle(delta);
            }
            MachineField::StrictHostKey => {
                let count = STRICT_HOST_KEY_CHOICES.len();
                self.strict_host_key =
                    (self.strict_host_key as isize + delta).rem_euclid(count as isize) as usize;
            }
            _ => {}
        }
    }

    fn choice_label(&self, field: MachineField) -> String {
        match field {
            MachineField::IdentitiesOnly => self.identities_only.label().to_owned(),
            MachineField::ForwardAgent => self.forward_agent.label().to_owned(),
            MachineField::SessionLogEnabled => self.session_log_enabled.label().to_owned(),
            MachineField::StrictHostKey => match STRICT_HOST_KEY_CHOICES[self.strict_host_key] {
                None => crate::i18n::texts().machines.choice_default.to_owned(),
                Some(value) => value.as_ssh_value().to_owned(),
            },
            _ => String::new(),
        }
    }

    fn effective_label(&self) -> String {
        let label = self.label.trim();
        if label.is_empty() {
            self.target.trim().to_owned()
        } else {
            label.to_owned()
        }
    }

    fn effective_session(&self) -> String {
        let session = self.session.trim();
        if session.is_empty() {
            crate::session::DEFAULT_SESSION_NAME.to_owned()
        } else {
            session.to_owned()
        }
    }

    fn profile_options(&self) -> Result<SshProfileOptions, String> {
        let parse_u16 = |editor: &TextEditor, flag: &str| -> Result<Option<u16>, String> {
            let raw = editor.trim();
            if raw.is_empty() {
                return Ok(None);
            }
            raw.parse::<u16>().map(Some).map_err(|_| {
                crate::i18n::fill(
                    crate::i18n::texts().cli_errors.invalid_flag_value_fmt,
                    &[("flag", flag), ("value", raw)],
                )
            })
        };
        let split_list = |editor: &TextEditor| -> Vec<String> {
            editor
                .split(',')
                .map(str::trim)
                .filter(|part| !part.is_empty())
                .map(str::to_owned)
                .collect()
        };
        let mut proxy_jump = Vec::new();
        for hop in split_list(&self.proxy_jump) {
            match hop.strip_prefix("profile:") {
                Some(id) => proxy_jump.push(ProxyJumpHop::Profile(
                    ProfileId::parse(id).map_err(|error| error.to_string())?,
                )),
                None => proxy_jump.push(ProxyJumpHop::Target(hop)),
            }
        }
        let session_log = match self.session_log_enabled {
            // Untouched: the saved value passes through verbatim.
            TriChoice::Default => self.session_log.clone(),
            choice => {
                let parse_u64 = |editor: &TextEditor, flag: &str| -> Result<Option<u64>, String> {
                    let raw = editor.trim();
                    if raw.is_empty() {
                        return Ok(None);
                    }
                    raw.parse::<u64>().map(Some).map_err(|_| {
                        crate::i18n::fill(
                            crate::i18n::texts().cli_errors.invalid_flag_value_fmt,
                            &[("flag", flag), ("value", raw)],
                        )
                    })
                };
                Some(SessionLogProfile {
                    enabled: choice.as_bool() == Some(true),
                    path_template: nonempty(self.session_log_path.trim()),
                    max_bytes: parse_u64(&self.session_log_max_bytes, "--log-max-bytes")?,
                    dump_interval_secs: parse_u16(&self.session_log_interval, "--log-interval")?,
                })
            }
        };
        Ok(SshProfileOptions {
            group: nonempty(self.group.trim()),
            tags: split_list(&self.tags),
            color: nonempty(self.color.trim()),
            port: parse_u16(&self.port, "--port")?,
            user: nonempty(self.user.trim()),
            identity_file: split_list(&self.identity_files),
            identities_only: self.identities_only.as_bool(),
            identity_agent: nonempty(self.identity_agent.trim()),
            strict_host_key_checking: STRICT_HOST_KEY_CHOICES[self.strict_host_key],
            proxy_jump,
            forward_agent: self.forward_agent.as_bool(),
            server_alive_interval: parse_u16(
                &self.server_alive_interval,
                "--server-alive-interval",
            )?,
            server_alive_count_max: parse_u16(
                &self.server_alive_count_max,
                "--server-alive-count-max",
            )?,
            control_persist: nonempty(self.control_persist.trim()),
            remote_command: nonempty(self.remote_command.trim()),
            session_log,
        })
    }
}

impl ClientShellState {
    pub(super) fn advance_machine_form(&mut self, outcome: &mut ClientShellInput) {
        enum Advance {
            Save,
            Bootstrap,
            Done,
        }
        let advance = {
            let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() else {
                return;
            };
            let ClientMachinesView::Form(form) = &mut overlay.view else {
                return;
            };
            if form.bootstrap.is_some() {
                return;
            }
            if form.editing.is_some() {
                Advance::Save
            } else {
                match form.step {
                    MachineFormStep::Target => {
                        if form.target.trim().is_empty() {
                            form.error =
                                Some(crate::i18n::texts().machines.target_required.to_owned());
                        } else {
                            if form.label.trim().is_empty() {
                                let target = form.target.trim().to_owned();
                                form.label = TextEditor::new(&target, false);
                            }
                            form.error = None;
                            form.step = MachineFormStep::Connection;
                            form.focused = 0;
                        }
                        Advance::Done
                    }
                    MachineFormStep::Connection => {
                        match form.profile_options() {
                            Ok(_) => {
                                form.error = None;
                                form.step = MachineFormStep::Session;
                                form.focused = 0;
                            }
                            Err(error) => form.error = Some(error),
                        }
                        Advance::Done
                    }
                    MachineFormStep::Session => {
                        if let Err(error) = crate::session::validate_name(&form.effective_session())
                        {
                            form.error = Some(error);
                        } else {
                            form.error = None;
                            form.step = MachineFormStep::Confirm;
                            form.focused = 0;
                        }
                        Advance::Done
                    }
                    MachineFormStep::Confirm => Advance::Bootstrap,
                }
            }
        };
        match advance {
            Advance::Save => self.save_machine_edit(),
            Advance::Bootstrap => self.start_machine_bootstrap(outcome),
            Advance::Done => {}
        }
    }

    fn save_machine_edit(&mut self) {
        let prepared = {
            let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() else {
                return;
            };
            let ClientMachinesView::Form(form) = &mut overlay.view else {
                return;
            };
            let Some(profile_id) = form.editing.clone() else {
                return;
            };
            let options = match form.profile_options() {
                Ok(options) => options,
                Err(error) => {
                    form.error = Some(error);
                    return;
                }
            };
            let session = form.effective_session();
            if let Err(error) = crate::session::validate_name(&session) {
                form.error = Some(error);
                return;
            }
            (profile_id, options, form.effective_label(), session)
        };
        let (profile_id, options, label, session) = prepared;
        match self.mutate_machine_catalog(|catalog| {
            catalog.update_ssh(&profile_id, label, session, options)
        }) {
            Ok(true) => {
                if let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() {
                    overlay.view = ClientMachinesView::Detail(profile_id);
                }
            }
            Ok(false) => {
                if let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() {
                    overlay.view = ClientMachinesView::List;
                }
            }
            Err(error) => {
                if let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() {
                    if let ClientMachinesView::Form(form) = &mut overlay.view {
                        form.error = Some(error);
                    }
                }
            }
        }
    }

    fn start_machine_bootstrap(&mut self, outcome: &mut ClientShellInput) {
        let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() else {
            return;
        };
        let ClientMachinesView::Form(form) = &mut overlay.view else {
            return;
        };
        if form.bootstrap.is_some() {
            return;
        }
        let options = match form.profile_options() {
            Ok(options) => options,
            Err(error) => {
                form.error = Some(error);
                return;
            }
        };
        let label = form.effective_label();
        let target = form.target.trim().to_owned();
        let session = form.effective_session();
        if let Err(error) = crate::session::validate_name(&session) {
            form.error = Some(error);
            return;
        }
        // Mirror the CLI: validate and resolve SSH options against a throwaway
        // in-memory catalog add; the profile is only persisted after the
        // remote bootstrap succeeds.
        let mut catalog = match EndpointCatalog::load() {
            Ok(catalog) => catalog,
            Err(error) => {
                form.error = Some(error);
                return;
            }
        };
        let ssh_options = catalog
            .add_ssh_with_options(&label, &target, &session, options)
            .and_then(|profile_id| {
                let profile = catalog
                    .ssh
                    .iter()
                    .find(|profile| profile.id == profile_id)
                    .ok_or_else(|| "machine profile vanished during setup".to_owned())?;
                crate::remote::ProfileSshOptions::from_profile(profile, &catalog.ssh)
            });
        let ssh_options = match ssh_options {
            Ok(options) => options,
            Err(error) => {
                form.error = Some(error);
                return;
            }
        };
        let ticket = self.next_machine_bootstrap_ticket;
        self.next_machine_bootstrap_ticket = self.next_machine_bootstrap_ticket.saturating_add(1);
        form.error = None;
        let cancel = crate::remote::TaskCancellation::default();
        form.bootstrap = Some(ClientMachineBootstrap {
            cancel: cancel.clone(),
            ticket,
            step: None,
            failure: None,
        });
        outcome.actions.push(ClientShellAction::BootstrapMachine {
            cancel,
            ticket,
            target,
            session,
            options: ssh_options,
        });
        outcome.repaint = true;
    }

    pub(crate) fn handle_machine_bootstrap_update(
        &mut self,
        ticket: u64,
        update: MachineBootstrapUpdate,
    ) {
        enum Prepared {
            None,
            Save(Box<(SshProfileOptions, String, String, String)>),
        }
        let prepared = {
            let Some(ClientShellOverlay::Machines(overlay)) = self.content_page_mut() else {
                return;
            };
            let ClientMachinesView::Form(form) = &mut overlay.view else {
                return;
            };
            if form
                .bootstrap
                .as_ref()
                .is_none_or(|bootstrap| bootstrap.ticket != ticket)
            {
                return;
            }
            match update {
                MachineBootstrapUpdate::Step(step) => {
                    if let Some(bootstrap) = form.bootstrap.as_mut() {
                        bootstrap.step = Some(step);
                    }
                    Prepared::None
                }
                MachineBootstrapUpdate::Finished(Ok(())) => match form.profile_options() {
                    Ok(options) => Prepared::Save(Box::new((
                        options,
                        form.effective_label(),
                        form.target.trim().to_owned(),
                        form.effective_session(),
                    ))),
                    Err(error) => {
                        if let Some(bootstrap) = form.bootstrap.as_mut() {
                            bootstrap.failure = Some(error);
                        }
                        Prepared::None
                    }
                },
                MachineBootstrapUpdate::Finished(Err(error)) => {
                    if let Some(bootstrap) = form.bootstrap.as_mut() {
                        bootstrap.failure = Some(error);
                    }
                    Prepared::None
                }
            }
        };
        let Prepared::Save(pending) = prepared else {
            return;
        };
        let (options, label, target, session) = *pending;
        match self.persist_bootstrapped_machine(options, &label, &target, &session) {
            Ok(profile_id) => {
                if let Some(ClientShellOverlay::Machines(overlay)) = self.content_page_mut() {
                    overlay.view = ClientMachinesView::List;
                    overlay.set_message(crate::i18n::fill(
                        crate::i18n::texts().machines.progress_done_fmt,
                        &[("label", &label)],
                    ));
                }
                self.select_machine_row(&profile_id);
            }
            Err(error) => {
                if let Some(ClientShellOverlay::Machines(overlay)) = self.content_page_mut() {
                    if let ClientMachinesView::Form(form) = &mut overlay.view {
                        if let Some(bootstrap) = form.bootstrap.as_mut() {
                            bootstrap.failure = Some(error);
                        }
                    }
                }
            }
        }
    }

    pub(in crate::client::shell) fn persist_authenticated_wizard(
        &mut self,
        profile: &SavedSshEndpoint,
    ) -> Result<ProfileId, String> {
        let options = crate::client::endpoint::SshProfileOptions {
            group: profile.group.clone(),
            tags: profile.tags.clone(),
            color: profile.color.clone(),
            port: profile.port,
            user: profile.user.clone(),
            identity_file: profile.identity_file.clone(),
            proxy_jump: profile.proxy_jump.clone(),
            strict_host_key_checking: profile.strict_host_key_checking,
            identities_only: profile.identities_only,
            identity_agent: profile.identity_agent.clone(),
            forward_agent: profile.forward_agent,
            server_alive_interval: profile.server_alive_interval,
            server_alive_count_max: profile.server_alive_count_max,
            control_persist: profile.control_persist.clone(),
            remote_command: profile.remote_command.clone(),
            session_log: profile.session_log.clone(),
        };
        self.persist_bootstrapped_machine(
            options,
            &profile.label,
            &profile.target,
            &profile.session,
        )
    }

    fn persist_bootstrapped_machine(
        &mut self,
        options: SshProfileOptions,
        label: &str,
        target: &str,
        session: &str,
    ) -> Result<ProfileId, String> {
        let mut catalog = EndpointCatalog::load()?;
        let profile_id = catalog.add_ssh_with_options(label, target, session, options)?;
        catalog.store_profiles()?;
        self.mirror_saved_profiles(catalog.ssh.clone());
        Ok(profile_id)
    }

    pub(super) fn route_machine_form_key(
        &mut self,
        key: &crate::input::TerminalKey,
        code: KeyCode,
        modifiers: KeyModifiers,
        outcome: &mut ClientShellInput,
    ) {
        let plain = modifiers.is_empty();
        let bootstrap_state = match self.overlay.as_ref() {
            Some(ClientShellOverlay::Machines(ClientMachinesOverlay {
                view: ClientMachinesView::Form(form),
                ..
            })) => form
                .bootstrap
                .as_ref()
                .map(|bootstrap| bootstrap.failure.is_some()),
            _ => None,
        };
        if let Some(failed) = bootstrap_state {
            // The bootstrap runs in the background; the page is read-only
            // until it succeeds (returns to the list) or fails (Esc re-enters
            // editing). A failure also offers the recovery dialogs.
            if !failed {
                if code == KeyCode::Esc {
                    self.machines_back();
                    outcome.repaint = true;
                }
                return;
            }
            match code {
                KeyCode::Esc => {
                    self.machines_back();
                    outcome.repaint = true;
                }
                KeyCode::Char('i') if plain => {
                    self.activate_machine_button(
                        MachineOverlayButton::WizardInteractiveAuth,
                        outcome,
                    );
                }
                KeyCode::Char('k') if plain => {
                    self.activate_machine_button(
                        MachineOverlayButton::WizardHostKeyReview,
                        outcome,
                    );
                }
                _ => {}
            }
            return;
        }

        if code == KeyCode::Esc {
            self.machines_back();
            outcome.repaint = true;
            return;
        }
        if matches!(
            code,
            KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown
        ) {
            let max = self.machines_view_max_scroll();
            if let Some(ClientShellOverlay::Machines(page)) = self.overlay.as_mut() {
                if let ClientMachinesView::Form(form) = &mut page.view {
                    if form.step == MachineFormStep::Confirm && form.editing.is_none() {
                        let delta: isize = match code {
                            KeyCode::Up => -1,
                            KeyCode::PageUp => -5,
                            KeyCode::PageDown => 5,
                            _ => 1,
                        };
                        form.scroll = form.scroll.saturating_add_signed(delta).min(max);
                        outcome.repaint = true;
                        return;
                    }
                }
            }
        }
        if code == KeyCode::Enter {
            self.advance_machine_form(outcome);
            outcome.repaint = true;
            return;
        }

        let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() else {
            return;
        };
        let ClientMachinesView::Form(form) = &mut overlay.view else {
            return;
        };
        let field_count = form.fields().len();
        if field_count == 0 {
            return;
        }
        match code {
            KeyCode::Tab if plain => {
                form.focused = (form.focused + 1) % field_count;
                outcome.repaint = true;
            }
            KeyCode::BackTab if modifiers.difference(KeyModifiers::SHIFT).is_empty() => {
                form.focused = (form.focused + field_count - 1) % field_count;
                outcome.repaint = true;
            }
            KeyCode::Up if plain => {
                form.focused = form.focused.saturating_sub(1);
                outcome.repaint = true;
            }
            KeyCode::Down if plain => {
                form.focused = (form.focused + 1).min(field_count - 1);
                outcome.repaint = true;
            }
            _ => {
                let Some(field) = form.focused_field() else {
                    return;
                };
                if field.is_choice()
                    && plain
                    && matches!(code, KeyCode::Left | KeyCode::Right | KeyCode::Char(' '))
                {
                    let delta = if code == KeyCode::Left { -1 } else { 1 };
                    form.cycle_choice(field, delta);
                    outcome.repaint = true;
                    return;
                }
                if let Some(editor) = form.editor_mut(field) {
                    outcome.repaint |= editor.handle_key(key).is_some();
                }
            }
        }
    }

    /// Builds the throwaway profile an approved interactive auth attempt
    /// runs against on the wizard path (never persisted).
    pub(super) fn wizard_temp_profile(
        form: &ClientMachineForm,
    ) -> Result<SavedSshEndpoint, String> {
        let options = form.profile_options()?;
        SavedSshEndpoint::with_options(
            form.effective_label(),
            form.target.trim(),
            form.effective_session(),
            options,
        )
    }

    /// Focus a form field by mouse; choice fields also cycle one step.
    pub(in crate::client::shell) fn focus_machine_form_field(&mut self, field: MachineField) {
        let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() else {
            return;
        };
        let ClientMachinesView::Form(form) = &mut overlay.view else {
            return;
        };
        if form.bootstrap.is_some() {
            return;
        }
        if let Some(index) = form
            .fields()
            .iter()
            .position(|candidate| *candidate == field)
        {
            if form.focused == index && field.is_choice() {
                form.cycle_choice(field, 1);
            }
            form.focused = index;
        }
    }
}

const BOOTSTRAP_STEPS: [SavedSshBootstrapStep; 4] = [
    SavedSshBootstrapStep::DetectPlatform,
    SavedSshBootstrapStep::Install,
    SavedSshBootstrapStep::StartServer,
    SavedSshBootstrapStep::Verify,
];

pub(in crate::client::shell) fn bootstrap_step_label(step: SavedSshBootstrapStep) -> &'static str {
    let t = &crate::i18n::texts().machines;
    match step {
        SavedSshBootstrapStep::DetectPlatform => t.progress_detect,
        SavedSshBootstrapStep::Install => t.progress_install,
        SavedSshBootstrapStep::StartServer => t.progress_start,
        SavedSshBootstrapStep::Verify => t.progress_verify,
    }
}

pub(super) fn render_machine_form(
    b: &mut Buffer,
    form: &ClientMachineForm,
    cx: &super::super::feedback::ChromeContext<'_>,
) -> Option<OverlayRender> {
    let p = cx.palette;
    let t = &crate::i18n::texts().machines;
    let editing = form.editing.is_some();
    let (popup, inner) = modal_panel(b, crate::ui::ModalSize::Large.with_height(24), p.accent, cx)?;
    if inner.width < 24 || inner.height < 8 {
        return Some(OverlayRender {
            area: popup,
            machines_popup: popup,
            ..OverlayRender::default()
        });
    }
    let stack = crate::ui::modal_stack_areas(inner, 2, 1, 1, 1);
    let base = Style::default()
        .bg(p.panel_bg)
        .remove_modifier(Modifier::DIM);
    put_text(
        b,
        stack.header.x,
        stack.header.y,
        stack.header.width,
        &format!(" {}", if editing { t.edit_title } else { t.add_title }),
        base.fg(p.text).add_modifier(Modifier::BOLD),
    );
    if !editing {
        let mut x = stack.header.x;
        for (index, step) in MachineFormStep::ALL.iter().enumerate() {
            let label = format!(" {} {} ", index + 1, step.label());
            let style = if *step == form.step {
                base.fg(p.accent).add_modifier(Modifier::BOLD)
            } else {
                base.fg(p.overlay0)
            };
            let width = display_width(&label).min(stack.header.right().saturating_sub(x));
            put_text(b, x, stack.header.y + 1, width, &label, style);
            x = x.saturating_add(width);
        }
    }

    let body = stack.content;
    let mut field_hits = Vec::new();
    let mut cursor = None;
    if let Some(bootstrap) = form.bootstrap.as_ref() {
        render_bootstrap_progress(b, body, form, bootstrap, base, cx);
    } else if form.step == MachineFormStep::Confirm && !editing {
        render_form_confirm(b, body, form, base, p);
    } else {
        let fields = form.fields();
        let visible = usize::from(body.height).max(1);
        let focused = form.focused.min(fields.len().saturating_sub(1));
        let scroll = form
            .scroll
            .max(focused.saturating_sub(visible.saturating_sub(1)))
            .min(focused)
            .min(fields.len().saturating_sub(visible));
        for (index, field) in fields.iter().enumerate().skip(scroll).take(visible) {
            let y = body.y + (index - scroll) as u16;
            let rect = Rect::new(body.x, y, body.width, 1);
            field_hits.push((rect, *field));
            let is_focused = index == focused;
            let label = format!(" {}", field.label());
            let label_width = fields
                .iter()
                .map(|field| display_width(field.label()) + 2)
                .max()
                .unwrap_or(12)
                .min(body.width / 2)
                .max(8.min(body.width));
            put_text(
                b,
                rect.x,
                rect.y,
                label_width,
                &label,
                base.fg(if is_focused { p.text } else { p.overlay0 }),
            );
            let input = Rect::new(
                rect.x + label_width,
                rect.y,
                rect.width.saturating_sub(label_width),
                1,
            );
            if field.is_choice() {
                let style = if is_focused {
                    Style::default()
                        .fg(panel_contrast_fg(p))
                        .bg(p.accent)
                        .add_modifier(Modifier::BOLD)
                } else {
                    crate::ui::input_field_style(p)
                };
                b.set_style(input, style);
                put_text(
                    b,
                    input.x,
                    input.y,
                    input.width,
                    &format!("‹ {} ›", form.choice_label(*field)),
                    style,
                );
            } else if let Some(editor) = form.editor(*field) {
                let field_style = crate::ui::input_field_style(p);
                b.set_style(input, field_style);
                let inner_input = Rect::new(input.x + 1, input.y, input.width.saturating_sub(1), 1);
                let field_cursor = text_editor::render(b, inner_input, editor, field_style);
                if is_focused {
                    cursor = field_cursor;
                    if let Some(hint) = field.hint() {
                        let used = display_width(editor.as_str()) + 2;
                        if used < input.width {
                            put_text(
                                b,
                                input.x + used,
                                input.y,
                                input.width - used,
                                hint,
                                base.fg(p.overlay0),
                            );
                        }
                    }
                }
            }
        }
    }

    if let Some(footer) = stack.footer {
        let hints: Vec<(String, String)> =
            if form.bootstrap.is_some() || form.step == MachineFormStep::Confirm {
                vec![
                    ("enter".to_owned(), t.hint_confirm.to_owned()),
                    ("esc".to_owned(), t.hint_back.to_owned()),
                ]
            } else {
                vec![
                    ("tab/↑↓".to_owned(), t.hint_fields.to_owned()),
                    ("←→".to_owned(), t.hint_change.to_owned()),
                    ("enter".to_owned(), t.hint_next.to_owned()),
                    ("esc".to_owned(), t.hint_back.to_owned()),
                ]
            };
        render_key_hints(b, footer, &hints, p, cx.components);
    }
    if let Some(error) = form.error.as_deref() {
        let y = stack
            .actions
            .map(|actions| actions.y.saturating_sub(1))
            .unwrap_or_else(|| body.bottom().saturating_sub(1));
        if y >= body.y {
            put_text(
                b,
                body.x,
                y,
                body.width,
                &format!(" {error}"),
                base.fg(p.red),
            );
        }
    }

    let mut action_hits = Vec::new();
    let running = form
        .bootstrap
        .as_ref()
        .is_some_and(|bootstrap| bootstrap.failure.is_none());
    let failed = form
        .bootstrap
        .as_ref()
        .is_some_and(|bootstrap| bootstrap.failure.is_some());
    // (label, button, enabled, tone) — the first enabled button is focused.
    let auth_texts = &crate::i18n::texts().machine_auth;
    let back_label = crate::i18n::texts().overlays.cancel_button;
    let mut buttons: Vec<(&str, MachineOverlayButton, bool, crate::ui::ModalButtonTone)> =
        Vec::new();
    if running {
        buttons.push((
            t.start_setup_button,
            MachineOverlayButton::StartSetup,
            false,
            crate::ui::ModalButtonTone::Primary,
        ));
        buttons.push((
            back_label,
            MachineOverlayButton::Back,
            true,
            crate::ui::ModalButtonTone::Secondary,
        ));
    } else if failed {
        // A failed setup offers the recovery entries next to the way back:
        // interactive auth for AuthRequired-style failures and the host-key
        // review for unknown/changed keys.
        buttons.push((
            t.next_button,
            MachineOverlayButton::Back,
            true,
            crate::ui::ModalButtonTone::Secondary,
        ));
        buttons.push((
            auth_texts.auth_interactive_button,
            MachineOverlayButton::WizardInteractiveAuth,
            true,
            crate::ui::ModalButtonTone::Primary,
        ));
        buttons.push((
            auth_texts.auth_precollect_button,
            MachineOverlayButton::WizardHostKeyReview,
            true,
            crate::ui::ModalButtonTone::Secondary,
        ));
        buttons.push((
            back_label,
            MachineOverlayButton::Back,
            true,
            crate::ui::ModalButtonTone::Secondary,
        ));
    } else {
        let (label, button) = if editing {
            (t.save_button, MachineOverlayButton::Save)
        } else if form.step == MachineFormStep::Confirm {
            (t.start_setup_button, MachineOverlayButton::StartSetup)
        } else {
            (t.next_button, MachineOverlayButton::Next)
        };
        buttons.push((label, button, true, crate::ui::ModalButtonTone::Primary));
        buttons.push((
            back_label,
            MachineOverlayButton::Back,
            true,
            crate::ui::ModalButtonTone::Secondary,
        ));
    }
    let labels: Vec<&str> = buttons.iter().map(|(label, ..)| *label).collect();
    let rects = modal_button_row(stack.actions.unwrap_or_default(), &labels, 2);
    if rects.len() == labels.len() {
        let mut focused_consumed = false;
        for (index, rect) in rects.iter().enumerate() {
            let (label, button, enabled, tone) = buttons[index];
            let base_state = if !enabled {
                crate::ui::ModalButtonState::Disabled
            } else if !focused_consumed {
                focused_consumed = true;
                crate::ui::ModalButtonState::Focused
            } else {
                crate::ui::ModalButtonState::Normal
            };
            let state = if enabled {
                cx.button_state(
                    &super::super::feedback::ChromeHover::MachineButton(button),
                    base_state,
                )
            } else {
                base_state
            };
            modal_button(b, *rect, label, tone, state, p);
            if enabled {
                action_hits.push((*rect, button));
            }
        }
    }

    Some(OverlayRender {
        area: popup,
        machines_popup: popup,
        machines_fields: field_hits,
        machines_actions: action_hits,
        cursor,
        ..OverlayRender::default()
    })
}

fn render_form_confirm(
    b: &mut Buffer,
    area: Rect,
    form: &ClientMachineForm,
    base: Style,
    p: &Palette,
) -> usize {
    use ratatui::widgets::{Paragraph, Widget, Wrap};
    let lines = form_confirm_lines(form, base, p);
    let measured = lines
        .iter()
        .cloned()
        .map(|line| (line.width(), line))
        .collect::<Vec<_>>();
    let metrics = crate::ui::display_lines_scroll_metrics(
        &measured,
        form.scroll.min(u16::MAX as usize) as u16,
        area,
    );
    Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((form.scroll.min(metrics.max_offset_from_bottom) as u16, 0))
        .render(area, b);
    metrics.max_offset_from_bottom
}

/// 确认步骤正文的最大滚动量：视图计算阶段与渲染阶段共用同一批行（STATE-04）。
pub(super) fn form_confirm_max_scroll(
    form: &ClientMachineForm,
    area: Rect,
    palette: &Palette,
) -> usize {
    let base = Style::default();
    let lines = form_confirm_lines(form, base, palette);
    let measured = lines
        .iter()
        .cloned()
        .map(|line| (line.width(), line))
        .collect::<Vec<_>>();
    crate::ui::display_lines_scroll_metrics(&measured, 0, area).max_offset_from_bottom
}

/// 确认步骤的正文行：`render_form_confirm` 与 `form_confirm_max_scroll` 共用。
fn form_confirm_lines<'a>(
    form: &'a ClientMachineForm,
    base: Style,
    p: &Palette,
) -> Vec<ratatui::text::Line<'a>> {
    use ratatui::text::{Line, Span};
    let t = &crate::i18n::texts().machines;
    let mut lines = vec![
        Line::styled(t.confirm_install_note, base.fg(p.yellow)),
        Line::styled(t.confirm_auth_note, base.fg(p.overlay0)),
        Line::default(),
    ];
    for field in std::iter::once(&MachineField::Target).chain(EDIT_FIELDS.iter()) {
        let value = if field.is_choice() {
            form.choice_label(*field).to_owned()
        } else {
            form.editor(*field)
                .map(|editor| editor.as_str().to_owned())
                .unwrap_or_default()
        };
        if !value.is_empty() {
            lines.push(Line::from(vec![
                Span::styled(format!("{}  ", field.label()), base.fg(p.overlay0)),
                Span::styled(value, base.fg(p.text)),
            ]));
        }
    }
    lines
}

fn render_bootstrap_progress(
    b: &mut Buffer,
    area: Rect,
    form: &ClientMachineForm,
    bootstrap: &ClientMachineBootstrap,
    base: Style,
    cx: &super::super::feedback::ChromeContext<'_>,
) {
    let p = cx.palette;
    let reached = bootstrap.step;
    for (index, step) in BOOTSTRAP_STEPS.iter().enumerate() {
        let y = area.y + index as u16;
        if y >= area.bottom() {
            break;
        }
        let (glyph, style) = match reached {
            Some(reached) if *step < reached => ("✓", base.fg(p.green)),
            Some(reached) if *step == reached => {
                (cx.spinner, base.fg(p.yellow).add_modifier(Modifier::BOLD))
            }
            _ => ("·", base.fg(p.overlay0)),
        };
        put_text(
            b,
            area.x,
            y,
            area.width,
            &format!(" {glyph} {}", bootstrap_step_label(*step)),
            style,
        );
    }
    if let Some(failure) = bootstrap.failure.as_deref() {
        let t = &crate::i18n::texts().machines;
        let y = area.y + BOOTSTRAP_STEPS.len() as u16 + 1;
        if y < area.bottom() {
            put_text(
                b,
                area.x,
                y,
                area.width,
                &crate::i18n::fill(t.progress_failed_fmt, &[("error", failure)]),
                base.fg(p.red),
            );
        }
        if y + 1 < area.bottom() {
            put_text(b, area.x, y + 1, area.width, t.fix_hint, base.fg(p.yellow));
        }
        if y + 2 < area.bottom() {
            let command = crate::remote::saved_ssh_bootstrap_command(
                form.target.trim(),
                &form.effective_session(),
            );
            put_text(
                b,
                area.x,
                y + 2,
                area.width,
                &format!("  {command}"),
                base.fg(p.text).add_modifier(Modifier::BOLD),
            );
        }
    }
}
