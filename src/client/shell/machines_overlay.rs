//! Machines management overlay: list, detail, add wizard, and edit form for
//! saved SSH endpoints. Machine CRUD works on the client-local catalog file;
//! the catalog watcher reconciles live connections afterwards, so no private
//! socket behavior is added here.

use super::*;
mod dashboard;
use crate::client::endpoint::{
    EndpointCatalog, PortForwardKind, PortForwardRule, ProfileId, ProxyJumpHop, SessionLogProfile,
    SshProfileOptions, StrictHostKeyChecking,
};
use crate::remote::SavedSshBootstrapStep;
use crossterm::event::KeyModifiers;

use super::render::{
    display_width, modal_button, modal_button_row, modal_panel, put_right_text, put_text,
    render_key_hints, render_search_bar, OverlayRender, SearchBar,
};

#[derive(Debug)]
pub(super) struct ClientMachinesOverlay {
    pub(super) view: ClientMachinesView,
    pub(super) query: TextEditor,
    pub(super) search_focused: bool,
    pub(super) selected: usize,
    pub(super) scroll: usize,
    pub(super) reveal: bool,
    pub(super) detail_scroll: usize,
    /// 指针悬浮的机器：只由 `Moved` 改写，`selected` 只由键盘与点击改写。
    /// 列表视图的 `d`（立即启停，会断掉在线 SSH）、`x`（删除）、`r`
    /// （重连）、`Shift+R`（改名）都取 `selected_machine_id()`——指针只是
    /// 划过列表就把这些破坏性键重新指向「鼠标最后路过的机器」是不可接受的
    /// （MENU-01 / UX-04）。存身份而不是行号，筛选与重排后天然失效。
    pub(super) hovered: Option<ProfileId>,
    /// One-shot feedback line shown in the list view (e.g. copied command).
    pub(super) message: Option<String>,
}

impl ClientMachinesOverlay {
    fn blank() -> Self {
        Self {
            view: ClientMachinesView::List,
            query: TextEditor::default(),
            search_focused: false,
            selected: 0,
            scroll: 0,
            reveal: true,
            detail_scroll: 0,
            hovered: None,
            message: None,
        }
    }

    /// A wizard bootstrap is running (no failure yet): drives the spinner.
    pub(super) fn bootstrap_running(&self) -> bool {
        matches!(
            &self.view,
            ClientMachinesView::Form(form)
                if form.bootstrap.as_ref().is_some_and(|bootstrap| bootstrap.failure.is_none())
        )
    }
}

#[derive(Debug)]
pub(super) enum ClientMachinesView {
    List,
    Detail(ProfileId),
    ConfirmRemove(ProfileId),
    Form(Box<ClientMachineForm>),
    Forwards(Box<ClientForwardRulesView>),
    Import(Box<ClientMachineImportView>),
}

/// Port-forward rules editor for one machine: lists the saved rules with
/// their live status and stages add/remove catalog writes (the catalog
/// watcher reconciles running forwards afterwards).
#[derive(Debug)]
pub(super) struct ClientForwardRulesView {
    pub(super) profile_id: ProfileId,
    pub(super) selected: usize,
    pub(super) adding: bool,
    pub(super) form: ClientForwardRuleForm,
    pub(super) error: Option<String>,
    pub(super) message: Option<String>,
}

#[derive(Debug)]
pub(super) struct ClientForwardRuleForm {
    /// Index into `FORWARD_KINDS`.
    pub(super) kind: usize,
    pub(super) listen_port: TextEditor,
    pub(super) bind_address: TextEditor,
    pub(super) target_host: TextEditor,
    pub(super) target_port: TextEditor,
    pub(super) focused: usize,
}

impl ClientForwardRuleForm {
    fn blank() -> Self {
        Self {
            kind: 0,
            listen_port: TextEditor::default(),
            bind_address: TextEditor::default(),
            target_host: TextEditor::default(),
            target_port: TextEditor::default(),
            focused: 0,
        }
    }
}

const FORWARD_KINDS: [crate::client::endpoint::PortForwardKind; 3] = [
    crate::client::endpoint::PortForwardKind::Local,
    crate::client::endpoint::PortForwardKind::Remote,
    crate::client::endpoint::PortForwardKind::Dynamic,
];

const FORWARD_FORM_FIELDS: usize = 5;

/// SSH config import wizard: discover → select → done. Discovery and
/// selection keep the parsed config and the current plan so the wildcard
/// toggle can re-plan without re-reading the file.
#[derive(Debug)]
pub(super) struct ClientMachineImportView {
    pub(super) step: ClientImportStep,
    pub(super) path: std::path::PathBuf,
    pub(super) config: Option<crate::remote::SshConfig>,
    pub(super) warnings: usize,
    /// Load failure or "no hosts" empty state, shown instead of the plan.
    pub(super) fatal: Option<String>,
    pub(super) plan: crate::remote::ImportPlan,
    /// Checkbox states parallel to `plan.ready`.
    pub(super) selected: Vec<bool>,
    pub(super) include_wildcards: bool,
    /// Focus across candidate rows, then the wildcard toggle, then the group
    /// input (last two positions).
    pub(super) focus_row: usize,
    pub(super) group: TextEditor,
    pub(super) results: Vec<ClientImportResultRow>,
    pub(super) summary: (usize, usize, usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ClientImportStep {
    Discover,
    Select,
    Done,
}

#[derive(Debug)]
pub(super) struct ClientImportResultRow {
    pub(super) label: String,
    pub(super) detail: String,
    pub(super) outcome: ClientImportOutcome,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ClientImportOutcome {
    Imported,
    Skipped,
    Failed,
}

impl ClientMachineImportView {
    /// A wizard that cannot show candidates: the message replaces the plan.
    fn fatal(path: std::path::PathBuf, message: String) -> Self {
        Self {
            step: ClientImportStep::Discover,
            path,
            config: None,
            warnings: 0,
            fatal: Some(message),
            plan: crate::remote::ImportPlan::default(),
            selected: Vec::new(),
            include_wildcards: false,
            focus_row: 0,
            group: TextEditor::default(),
            results: Vec::new(),
            summary: (0, 0, 0),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MachineFormStep {
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

    fn previous(self) -> Self {
        match self {
            Self::Target => Self::Target,
            Self::Connection => Self::Target,
            Self::Session => Self::Connection,
            Self::Confirm => Self::Session,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TriChoice {
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
pub(super) enum MachineField {
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
pub(super) struct ClientMachineForm {
    pub(super) editing: Option<ProfileId>,
    pub(super) step: MachineFormStep,
    pub(super) focused: usize,
    pub(super) scroll: usize,
    pub(super) target: TextEditor,
    pub(super) label: TextEditor,
    pub(super) session: TextEditor,
    pub(super) group: TextEditor,
    pub(super) tags: TextEditor,
    pub(super) color: TextEditor,
    pub(super) user: TextEditor,
    pub(super) port: TextEditor,
    pub(super) identity_files: TextEditor,
    pub(super) identity_agent: TextEditor,
    pub(super) identities_only: TriChoice,
    /// Index into `STRICT_HOST_KEY_CHOICES`; 0 keeps the SSH default.
    pub(super) strict_host_key: usize,
    pub(super) proxy_jump: TextEditor,
    pub(super) forward_agent: TriChoice,
    pub(super) server_alive_interval: TextEditor,
    pub(super) server_alive_count_max: TextEditor,
    pub(super) control_persist: TextEditor,
    pub(super) remote_command: TextEditor,
    /// Session log editing: `Default` keeps the saved value untouched (the
    /// text fields are ignored); Yes/No rebuilds the profile from the text
    /// fields below (empty text = unset, i.e. the mechanism default).
    pub(super) session_log_enabled: TriChoice,
    pub(super) session_log_path: TextEditor,
    pub(super) session_log_max_bytes: TextEditor,
    pub(super) session_log_interval: TextEditor,
    /// The value loaded from (and preserved by) the form when
    /// `session_log_enabled` stays `Default`.
    pub(super) session_log: Option<SessionLogProfile>,
    pub(super) error: Option<String>,
    pub(super) bootstrap: Option<ClientMachineBootstrap>,
}

#[derive(Debug)]
pub(super) struct ClientMachineBootstrap {
    pub(super) cancel: crate::remote::TaskCancellation,
    pub(super) ticket: u64,
    pub(super) step: Option<SavedSshBootstrapStep>,
    pub(super) failure: Option<String>,
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
    fn blank() -> Self {
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

    fn from_profile(profile: &SavedSshEndpoint) -> Self {
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

    fn fields(&self) -> &'static [MachineField] {
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

    fn focused_field(&self) -> Option<MachineField> {
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

    fn editor_mut(&mut self, field: MachineField) -> Option<&mut TextEditor> {
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

fn nonempty(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_owned())
}

/// One row in the machines list view.
pub(super) struct MachineListRow {
    pub(super) id: ProfileId,
    pub(super) label: String,
    pub(super) target: String,
    pub(super) group: Option<String>,
    pub(super) enabled: bool,
    pub(super) color: Option<String>,
    pub(super) status: ClientEndpointStatus,
    pub(super) server_version: Option<String>,
}

fn machine_matches(profile: &SavedSshEndpoint, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    query.is_empty()
        || format!(
            "{} {} {}",
            profile.label,
            profile.target,
            profile.group.as_deref().unwrap_or_default()
        )
        .to_lowercase()
        .contains(&query)
}

pub(super) fn machine_list_rows(
    saved_profiles: &[SavedSshEndpoint],
    endpoints: &[ClientShellEndpoint],
    query: &str,
) -> Vec<MachineListRow> {
    saved_profiles
        .iter()
        .filter(|profile| machine_matches(profile, query))
        .map(|profile| {
            let endpoint_id = ClientEndpointId::Ssh(profile.id.clone());
            let endpoint = endpoints
                .iter()
                .find(|endpoint| endpoint.endpoint_id == endpoint_id);
            MachineListRow {
                id: profile.id.clone(),
                label: profile.label.clone(),
                target: profile.target.clone(),
                group: profile.group.clone(),
                enabled: profile.enabled,
                color: profile.color.clone(),
                status: endpoint.map_or(
                    if profile.enabled {
                        ClientEndpointStatus::Connecting
                    } else {
                        ClientEndpointStatus::Disabled
                    },
                    |endpoint| endpoint.status,
                ),
                server_version: endpoint.and_then(|endpoint| endpoint.server_version.clone()),
            }
        })
        .collect()
}

fn endpoint_for<'a>(
    endpoints: &'a [ClientShellEndpoint],
    profile_id: &ProfileId,
) -> Option<&'a ClientShellEndpoint> {
    let endpoint_id = ClientEndpointId::Ssh(profile_id.clone());
    endpoints
        .iter()
        .find(|endpoint| endpoint.endpoint_id == endpoint_id)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MachineOverlayButton {
    Add,
    Close,
    Back,
    Next,
    Save,
    StartSetup,
    Edit,
    Reconnect,
    ToggleEnabled,
    Remove,
    CopyFix,
    ConfirmRemove,
    CancelRemove,
    ReviewIssue,
    WizardInteractiveAuth,
    WizardHostKeyReview,
    Import,
    ImportContinue,
    ImportRun,
    Forwards,
    ForwardAddStart,
    ForwardRemove,
    ForwardSave,
    ForwardCancel,
    Broadcast,
    BrowseFiles,
}

impl ClientShellState {
    pub(super) fn open_machines_overlay(&mut self) {
        if matches!(self.overlay, Some(ClientShellOverlay::Machines(_))) {
            return;
        }
        self.chrome_drag = None;
        self.overlay = Some(ClientShellOverlay::Machines(ClientMachinesOverlay::blank()));
    }

    pub(super) fn open_machines_overlay_for(&mut self, profile_id: &ProfileId) {
        self.open_machines_overlay();
        if self
            .saved_profiles
            .iter()
            .any(|profile| &profile.id == profile_id)
        {
            if let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() {
                overlay.view = ClientMachinesView::Detail(profile_id.clone());
            }
        }
    }

    pub(super) fn open_machine_add_form(&mut self) {
        self.open_machines_overlay();
        if let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() {
            overlay.view = ClientMachinesView::Form(Box::new(ClientMachineForm::blank()));
        }
    }

    pub(super) fn open_machine_edit_form(&mut self, profile_id: &ProfileId) {
        let profile = self
            .saved_profiles
            .iter()
            .find(|profile| &profile.id == profile_id)
            .cloned();
        self.open_machines_overlay();
        if let (Some(profile), Some(ClientShellOverlay::Machines(overlay))) =
            (profile, self.overlay.as_mut())
        {
            overlay.view =
                ClientMachinesView::Form(Box::new(ClientMachineForm::from_profile(&profile)));
        }
    }

    pub(super) fn open_machine_remove_confirm(&mut self, profile_id: &ProfileId) {
        self.open_machines_overlay_for(profile_id);
        if let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() {
            if matches!(overlay.view, ClientMachinesView::Detail(_)) {
                overlay.view = ClientMachinesView::ConfirmRemove(profile_id.clone());
            }
        }
    }

    fn saved_profile(&self, profile_id: &ProfileId) -> Option<&SavedSshEndpoint> {
        self.saved_profiles
            .iter()
            .find(|profile| &profile.id == profile_id)
    }

    /// Applies one catalog mutation against a freshly loaded catalog and, on
    /// success, persists it and mirrors the result into the render state.
    fn mutate_machine_catalog(
        &mut self,
        mutate: impl FnOnce(&mut EndpointCatalog) -> Result<bool, String>,
    ) -> Result<bool, String> {
        let mut catalog = EndpointCatalog::load()?;
        let previous_selection = catalog.selected_profile.clone();
        let changed = mutate(&mut catalog)?;
        if !changed {
            return Ok(false);
        }
        catalog.store_profiles()?;
        if catalog.selected_profile != previous_selection {
            catalog.store_selection()?;
        }
        self.mirror_saved_profiles(catalog.ssh.clone());
        Ok(true)
    }

    pub(super) fn machine_set_enabled(&mut self, profile_id: &ProfileId, enabled: bool) {
        if let Err(error) =
            self.mutate_machine_catalog(|catalog| Ok(catalog.set_enabled(profile_id, enabled)))
        {
            self.receive_endpoint_unavailable(error);
        }
    }

    fn machine_toggle_enabled(&mut self, profile_id: &ProfileId) {
        let Some(enabled) = self
            .saved_profile(profile_id)
            .map(|profile| profile.enabled)
        else {
            return;
        };
        self.machine_set_enabled(profile_id, !enabled);
    }

    fn machine_remove(&mut self, profile_id: &ProfileId) {
        if let Err(error) =
            self.mutate_machine_catalog(|catalog| Ok(catalog.remove_ssh(profile_id)))
        {
            self.receive_endpoint_unavailable(error);
        }
    }

    pub(super) fn machine_rename(&mut self, profile_id: &ProfileId, label: &str) {
        let label = label.trim().to_owned();
        if label.is_empty() {
            return;
        }
        match self.mutate_machine_catalog(|catalog| catalog.rename_ssh(profile_id, label)) {
            Ok(true) => {}
            Ok(false) => {
                self.receive_endpoint_unavailable(crate::i18n::fill(
                    crate::i18n::texts()
                        .cli_errors
                        .machine_profile_not_found_fmt,
                    &[("id", profile_id.as_str())],
                ));
            }
            Err(error) => {
                self.receive_endpoint_unavailable(error);
            }
        }
    }

    pub(super) fn machine_reconnect(
        &mut self,
        profile_id: &ProfileId,
        outcome: &mut ClientShellInput,
    ) {
        let Some(profile) = self.saved_profile(profile_id) else {
            return;
        };
        if !profile.enabled {
            return;
        }
        outcome.actions.push(ClientShellAction::ReconnectEndpoint {
            endpoint_id: ClientEndpointId::Ssh(profile_id.clone()),
        });
    }

    fn machine_copy_fix_command(&mut self, profile_id: &ProfileId, outcome: &mut ClientShellInput) {
        let Some(profile) = self.saved_profile(profile_id) else {
            return;
        };
        let command = crate::remote::saved_ssh_bootstrap_command(&profile.target, &profile.session);
        outcome
            .actions
            .push(ClientShellAction::ClipboardWrite(command.into_bytes()));
        if let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() {
            overlay.message = Some(crate::i18n::texts().machines.copied_fix_command.to_owned());
        }
    }

    fn filtered_machine_rows(&self, query: &str) -> Vec<MachineListRow> {
        machine_list_rows(&self.saved_profiles, &self.endpoints, query)
    }

    fn selected_machine_id(&self) -> Option<ProfileId> {
        let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_ref() else {
            return None;
        };
        let rows = self.filtered_machine_rows(overlay.query.as_str());
        if rows.is_empty() {
            return None;
        }
        let selected = overlay.selected.min(rows.len() - 1);
        Some(rows[selected].id.clone())
    }

    fn move_machines_selection(&mut self, delta: isize) {
        let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_ref() else {
            return;
        };
        let count = self.filtered_machine_rows(overlay.query.as_str()).len();
        let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() else {
            return;
        };
        if count == 0 {
            overlay.selected = 0;
            overlay.scroll = 0;
            return;
        }
        overlay.reveal = true;
        overlay.detail_scroll = 0;
        overlay.selected =
            (overlay.selected as isize + delta).clamp(0, count.saturating_sub(1) as isize) as usize;
    }

    pub(super) fn select_machine_row(&mut self, profile_id: &ProfileId) {
        let Some(ClientShellOverlay::Machines(overlay)) = self.content_page() else {
            return;
        };
        let rows = self.filtered_machine_rows(overlay.query.as_str());
        let Some(index) = rows.iter().position(|row| &row.id == profile_id) else {
            return;
        };
        if let Some(ClientShellOverlay::Machines(overlay)) = self.content_page_mut() {
            overlay.selected = index;
        }
    }

    pub(super) fn open_machine_detail(&mut self, profile_id: &ProfileId) {
        if self.saved_profile(profile_id).is_none() {
            return;
        }
        if let Some(ClientShellOverlay::Machines(overlay)) = self.content_page_mut() {
            overlay.view = ClientMachinesView::Detail(profile_id.clone());
            overlay.detail_scroll = 0;
        }
    }

    fn machines_back(&mut self) {
        enum Back {
            Close,
            List,
            Detail(ProfileId),
            FormEdit,
            FormStep,
        }
        let action = match self.overlay.as_ref() {
            Some(ClientShellOverlay::Machines(overlay)) => match &overlay.view {
                ClientMachinesView::List => Back::Close,
                ClientMachinesView::Detail(_) | ClientMachinesView::ConfirmRemove(_) => Back::List,
                ClientMachinesView::Forwards(view) => {
                    if view.adding {
                        // Esc while adding only cancels the add form.
                        if let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() {
                            if let ClientMachinesView::Forwards(view) = &mut overlay.view {
                                view.adding = false;
                                view.error = None;
                            }
                        }
                        return;
                    }
                    Back::Detail(view.profile_id.clone())
                }
                ClientMachinesView::Import(view) => match view.step {
                    ClientImportStep::Select => {
                        if let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() {
                            if let ClientMachinesView::Import(view) = &mut overlay.view {
                                view.step = ClientImportStep::Discover;
                            }
                        }
                        return;
                    }
                    ClientImportStep::Discover | ClientImportStep::Done => Back::List,
                },
                ClientMachinesView::Form(form) => {
                    if let Some(bootstrap) = form.bootstrap.as_ref() {
                        bootstrap.cancel.cancel();
                        Back::FormEdit
                    } else if form.editing.is_some() {
                        match &form.editing {
                            Some(id) if self.saved_profile(id).is_some() => {
                                Back::Detail(id.clone())
                            }
                            _ => Back::List,
                        }
                    } else if form.step == MachineFormStep::Target {
                        Back::List
                    } else {
                        Back::FormStep
                    }
                }
            },
            _ => return,
        };
        match action {
            Back::Close => self.overlay = None,
            Back::List => {
                if let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() {
                    overlay.view = ClientMachinesView::List;
                }
            }
            Back::Detail(id) => {
                if let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() {
                    overlay.view = ClientMachinesView::Detail(id);
                }
            }
            Back::FormEdit => {
                if let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() {
                    if let ClientMachinesView::Form(form) = &mut overlay.view {
                        form.bootstrap = None;
                    }
                }
            }
            Back::FormStep => {
                if let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() {
                    if let ClientMachinesView::Form(form) = &mut overlay.view {
                        let previous = form.step.previous();
                        form.step = previous;
                        form.focused = form.fields().len().saturating_sub(1);
                    }
                }
            }
        }
    }

    fn advance_machine_form(&mut self, outcome: &mut ClientShellInput) {
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
                                Some(crate::i18n::texts().cli_errors.label_required.to_owned());
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
                    overlay.message = Some(crate::i18n::fill(
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

    pub(super) fn persist_authenticated_wizard(
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

    // ----- port forwards editor -----

    pub(super) fn open_machine_forwards(&mut self, profile_id: &ProfileId) {
        if self.saved_profile(profile_id).is_none() {
            return;
        }
        if let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() {
            overlay.view = ClientMachinesView::Forwards(Box::new(ClientForwardRulesView {
                profile_id: profile_id.clone(),
                selected: 0,
                adding: false,
                form: ClientForwardRuleForm::blank(),
                error: None,
                message: None,
            }));
        }
    }

    fn forward_rule_from_form(form: &ClientForwardRuleForm) -> Result<PortForwardRule, String> {
        let t = &crate::i18n::texts().cli_errors;
        let parse_port = |editor: &TextEditor, flag: &str| -> Result<Option<u16>, String> {
            let raw = editor.trim();
            if raw.is_empty() {
                return Ok(None);
            }
            raw.parse::<u16>().map(Some).map_err(|_| {
                crate::i18n::fill(t.invalid_flag_value_fmt, &[("flag", flag), ("value", raw)])
            })
        };
        let kind = FORWARD_KINDS[form.kind.min(FORWARD_KINDS.len() - 1)];
        let listen_port = parse_port(&form.listen_port, "--listen-port")?
            .ok_or_else(|| t.machine_forward_listen_port_required.to_owned())?;
        let target_host = nonempty(form.target_host.trim());
        let target_port = parse_port(&form.target_port, "--target-port")?;
        Ok(PortForwardRule {
            kind,
            bind_address: nonempty(form.bind_address.trim()),
            listen_port,
            target_host,
            target_port,
        })
    }

    /// Persists the rule list of the open forwards view; the catalog watcher
    /// reconciles running forwards afterwards.
    fn store_forward_rules(
        &mut self,
        profile_id: &ProfileId,
        rules: Vec<PortForwardRule>,
    ) -> Result<(), String> {
        self.mutate_machine_catalog(|catalog| catalog.set_port_forwards(profile_id, rules))?;
        Ok(())
    }

    fn forward_remove_selected(&mut self) {
        let (profile_id, index) = {
            let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_ref() else {
                return;
            };
            let ClientMachinesView::Forwards(view) = &overlay.view else {
                return;
            };
            (view.profile_id.clone(), view.selected)
        };
        let Some(profile) = self.saved_profile(&profile_id).cloned() else {
            return;
        };
        if profile.port_forwards.is_empty() {
            return;
        }
        let index = index.min(profile.port_forwards.len() - 1);
        let mut rules = profile.port_forwards.clone();
        rules.remove(index);
        let message = match self.store_forward_rules(&profile_id, rules) {
            Ok(()) => Some(crate::i18n::texts().machines.forward_removed.to_owned()),
            Err(error) => Some(error),
        };
        if let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() {
            if let ClientMachinesView::Forwards(view) = &mut overlay.view {
                view.error = None;
                view.message = message;
                view.selected = view.selected.saturating_sub(1);
            }
        }
    }

    fn forward_save_new_rule(&mut self) {
        let (profile_id, rule) = {
            let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() else {
                return;
            };
            let ClientMachinesView::Forwards(view) = &mut overlay.view else {
                return;
            };
            match Self::forward_rule_from_form(&view.form) {
                Ok(rule) => (view.profile_id.clone(), rule),
                Err(error) => {
                    view.error = Some(error);
                    return;
                }
            }
        };
        let Some(profile) = self.saved_profile(&profile_id).cloned() else {
            return;
        };
        let rule_count = profile.port_forwards.len();
        let mut rules = profile.port_forwards.clone();
        rules.push(rule);
        match self.store_forward_rules(&profile_id, rules) {
            Ok(()) => {
                if let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() {
                    if let ClientMachinesView::Forwards(view) = &mut overlay.view {
                        view.adding = false;
                        view.form = ClientForwardRuleForm::blank();
                        view.error = None;
                        view.message = Some(crate::i18n::texts().machines.forward_saved.to_owned());
                        view.selected = rule_count;
                    }
                }
            }
            Err(error) => {
                if let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() {
                    if let ClientMachinesView::Forwards(view) = &mut overlay.view {
                        view.error = Some(error);
                    }
                }
            }
        }
    }

    // ----- SSH config import wizard -----

    pub(super) fn open_machine_import_wizard(&mut self) {
        self.open_machines_overlay();
        let t = &crate::i18n::texts().machines;
        let view = match crate::platform::remote_ssh_config_paths().user_config {
            Some(path) => match crate::remote::SshConfig::load(&path) {
                Ok(config) => {
                    let warnings = config.warnings().len();
                    let existing = EndpointCatalog::load()
                        .map(|catalog| catalog.ssh)
                        .unwrap_or_default();
                    let plan =
                        crate::remote::plan_import(config.import_candidates(), &existing, false);
                    let fatal = if plan.ready.is_empty() && plan.skipped.is_empty() {
                        Some(crate::i18n::fill(
                            t.import_no_hosts_fmt,
                            &[("path", &path.display().to_string())],
                        ))
                    } else {
                        None
                    };
                    let selected = vec![true; plan.ready.len()];
                    ClientMachineImportView {
                        step: ClientImportStep::Discover,
                        path,
                        config: Some(config),
                        warnings,
                        fatal,
                        plan,
                        selected,
                        include_wildcards: false,
                        focus_row: 0,
                        group: TextEditor::default(),
                        results: Vec::new(),
                        summary: (0, 0, 0),
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    ClientMachineImportView::fatal(path, t.import_no_config.to_owned())
                }
                Err(error) => ClientMachineImportView::fatal(
                    path,
                    crate::i18n::fill(t.import_read_failed_fmt, &[("error", &error.to_string())]),
                ),
            },
            None => ClientMachineImportView::fatal(
                std::path::PathBuf::from("~/.ssh/config"),
                t.import_no_config.to_owned(),
            ),
        };
        if let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() {
            overlay.view = ClientMachinesView::Import(Box::new(view));
        }
    }

    /// Re-plans the selection step after the wildcard toggle flips: labels
    /// that were checked keep their state when still importable.
    fn replan_machine_import(&mut self, include_wildcards: bool) {
        let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() else {
            return;
        };
        let ClientMachinesView::Import(view) = &mut overlay.view else {
            return;
        };
        let Some(config) = &view.config else {
            return;
        };
        let checked: HashSet<String> = view
            .plan
            .ready
            .iter()
            .zip(view.selected.iter())
            .filter(|(_, selected)| **selected)
            .map(|(planned, _)| planned.label.to_ascii_lowercase())
            .collect();
        let existing = EndpointCatalog::load()
            .map(|catalog| catalog.ssh)
            .unwrap_or_default();
        view.plan =
            crate::remote::plan_import(config.import_candidates(), &existing, include_wildcards);
        view.selected = view
            .plan
            .ready
            .iter()
            .map(|planned| checked.contains(&planned.label.to_ascii_lowercase()))
            .collect();
        view.include_wildcards = include_wildcards;
        view.focus_row = 0;
    }

    /// Executes the checked imports in dependency order, reporting per host.
    /// Like the CLI, import only writes catalog entries; remote server setup
    /// stays an explicit later step.
    fn execute_machine_import(&mut self) {
        // Move the wizard out of the overlay so catalog mutation and the
        // profile mirror can borrow `self` freely; the view always goes back.
        let mut view = {
            let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() else {
                return;
            };
            match std::mem::replace(&mut overlay.view, ClientMachinesView::List) {
                ClientMachinesView::Import(view) => *view,
                other => {
                    overlay.view = other;
                    return;
                }
            }
        };
        self.execute_machine_import_owned(&mut view);
        if let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() {
            overlay.view = ClientMachinesView::Import(Box::new(view));
        }
    }

    fn execute_machine_import_owned(&mut self, view: &mut ClientMachineImportView) {
        let group = nonempty(view.group.trim());
        let mut catalog = match EndpointCatalog::load() {
            Ok(catalog) => catalog,
            Err(error) => {
                view.fatal = Some(error);
                return;
            }
        };
        let mut ids: Vec<Option<ProfileId>> = vec![None; view.plan.ready.len()];
        let mut imported = 0usize;
        let mut failed = 0usize;
        let skipped_pre: usize = view.plan.skipped.len();
        for (index, planned) in view.plan.ready.iter().enumerate() {
            let selected = view.selected.get(index).copied().unwrap_or(false);
            if !selected {
                continue;
            }
            let mut options = planned.options_with_resolved_hops(&ids);
            if let Some(group) = &group {
                options.group = Some(group.clone());
            }
            match catalog.add_ssh_with_options(
                &planned.label,
                &planned.target,
                crate::session::DEFAULT_SESSION_NAME,
                options,
            ) {
                Ok(id) => {
                    ids[index] = Some(id);
                    imported += 1;
                    view.results.push(ClientImportResultRow {
                        label: planned.label.clone(),
                        detail: planned.notes.join("; "),
                        outcome: ClientImportOutcome::Imported,
                    });
                }
                Err(error) => {
                    failed += 1;
                    view.results.push(ClientImportResultRow {
                        label: planned.label.clone(),
                        detail: error,
                        outcome: ClientImportOutcome::Failed,
                    });
                }
            }
        }
        if imported > 0 {
            match catalog.store_profiles() {
                Ok(()) => self.mirror_saved_profiles(catalog.ssh.clone()),
                Err(error) => {
                    view.fatal = Some(error);
                    return;
                }
            }
        }
        // Hosts the user unchecked report as skipped alongside the plan skips.
        for (index, planned) in view.plan.ready.iter().enumerate() {
            if !view.selected.get(index).copied().unwrap_or(false) {
                view.results.push(ClientImportResultRow {
                    label: planned.label.clone(),
                    detail: String::new(),
                    outcome: ClientImportOutcome::Skipped,
                });
            }
        }
        let deselected = view
            .plan
            .ready
            .iter()
            .zip(view.selected.iter())
            .filter(|(_, selected)| !**selected)
            .count();
        view.summary = (imported, skipped_pre + deselected, failed);
        view.step = ClientImportStep::Done;
    }

    /// Focus rows on the select step: candidates, then the wildcard toggle,
    /// then the group input.
    fn import_row_count(&self) -> usize {
        match self.overlay.as_ref() {
            Some(ClientShellOverlay::Machines(overlay)) => match &overlay.view {
                ClientMachinesView::Import(view) => view.plan.ready.len() + 2,
                _ => 0,
            },
            _ => 0,
        }
    }

    fn move_import_focus(&mut self, delta: isize) {
        let count = self.import_row_count();
        if count == 0 {
            return;
        }
        if let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() {
            if let ClientMachinesView::Import(view) = &mut overlay.view {
                view.focus_row = (view.focus_row as isize + delta)
                    .clamp(0, count.saturating_sub(1) as isize)
                    as usize;
            }
        }
    }

    fn import_toggle_focused(&mut self) {
        let replan = {
            let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() else {
                return;
            };
            let ClientMachinesView::Import(view) = &mut overlay.view else {
                return;
            };
            let candidates = view.plan.ready.len();
            if view.focus_row < candidates {
                if let Some(selected) = view.selected.get_mut(view.focus_row) {
                    *selected = !*selected;
                }
                None
            } else if view.focus_row == candidates {
                Some(!view.include_wildcards)
            } else {
                None
            }
        };
        if let Some(include_wildcards) = replan {
            self.replan_machine_import(include_wildcards);
        }
    }

    fn import_toggle_all(&mut self) {
        let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() else {
            return;
        };
        let ClientMachinesView::Import(view) = &mut overlay.view else {
            return;
        };
        let all = view.selected.iter().all(|selected| *selected);
        view.selected.fill(!all);
    }

    /// Mouse click on a wizard/editor row, identified by its rendered index.
    pub(super) fn click_machine_wizard_row(&mut self, row: usize, outcome: &mut ClientShellInput) {
        let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() else {
            return;
        };
        match &mut overlay.view {
            ClientMachinesView::Import(view) => {
                view.focus_row = row;
                if row < view.plan.ready.len() + 1 {
                    self.import_toggle_focused();
                }
            }
            ClientMachinesView::Forwards(view) => {
                view.selected = row;
            }
            _ => {}
        }
        outcome.repaint = true;
    }

    /// Focus a forward add-form field by mouse; the kind field also cycles.
    pub(super) fn focus_machine_forward_field(&mut self, field: usize) {
        let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() else {
            return;
        };
        let ClientMachinesView::Forwards(view) = &mut overlay.view else {
            return;
        };
        if !view.adding || field >= FORWARD_FORM_FIELDS {
            return;
        }
        if view.form.focused == field && field == 0 {
            view.form.kind = (view.form.kind + 1) % FORWARD_KINDS.len();
        }
        view.form.focused = field;
    }

    fn route_machine_forwards_key(
        &mut self,
        key: &crate::input::TerminalKey,
        code: KeyCode,
        modifiers: KeyModifiers,
        outcome: &mut ClientShellInput,
    ) {
        let plain = modifiers.is_empty();
        let adding = matches!(
            self.overlay.as_ref(),
            Some(ClientShellOverlay::Machines(overlay))
                if matches!(&overlay.view, ClientMachinesView::Forwards(view) if view.adding)
        );
        if adding {
            if code == KeyCode::Esc {
                if let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() {
                    if let ClientMachinesView::Forwards(view) = &mut overlay.view {
                        view.adding = false;
                        view.error = None;
                    }
                }
                outcome.repaint = true;
                return;
            }
            if code == KeyCode::Enter {
                self.forward_save_new_rule();
                outcome.repaint = true;
                return;
            }
            let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() else {
                return;
            };
            let ClientMachinesView::Forwards(view) = &mut overlay.view else {
                return;
            };
            match code {
                KeyCode::Tab if plain => {
                    view.form.focused = (view.form.focused + 1) % FORWARD_FORM_FIELDS;
                }
                KeyCode::BackTab if modifiers.difference(KeyModifiers::SHIFT).is_empty() => {
                    view.form.focused =
                        (view.form.focused + FORWARD_FORM_FIELDS - 1) % FORWARD_FORM_FIELDS;
                }
                KeyCode::Up if plain => {
                    view.form.focused = view.form.focused.saturating_sub(1);
                }
                KeyCode::Down if plain => {
                    view.form.focused = (view.form.focused + 1).min(FORWARD_FORM_FIELDS - 1);
                }
                KeyCode::Left | KeyCode::Right if plain && view.form.focused == 0 => {
                    let delta = if code == KeyCode::Left { -1 } else { 1 };
                    view.form.kind = (view.form.kind as isize + delta)
                        .rem_euclid(FORWARD_KINDS.len() as isize)
                        as usize;
                }
                KeyCode::Char(' ') if plain && view.form.focused == 0 => {
                    view.form.kind = (view.form.kind + 1) % FORWARD_KINDS.len();
                }
                _ => {
                    let editor = match view.form.focused {
                        1 => &mut view.form.listen_port,
                        2 => &mut view.form.bind_address,
                        3 => &mut view.form.target_host,
                        _ => &mut view.form.target_port,
                    };
                    outcome.repaint |= editor.handle_key(key).is_some();
                }
            }
            outcome.repaint = true;
            return;
        }
        match code {
            KeyCode::Esc => self.machines_back(),
            KeyCode::Up | KeyCode::Char('k') if plain => {
                if let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() {
                    if let ClientMachinesView::Forwards(view) = &mut overlay.view {
                        view.selected = view.selected.saturating_sub(1);
                    }
                }
            }
            KeyCode::Down | KeyCode::Char('j') if plain => {
                let count = match self.overlay.as_ref() {
                    Some(ClientShellOverlay::Machines(overlay)) => match &overlay.view {
                        ClientMachinesView::Forwards(view) => self
                            .saved_profile(&view.profile_id)
                            .map(|profile| profile.port_forwards.len())
                            .unwrap_or(0),
                        _ => 0,
                    },
                    _ => 0,
                };
                if let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() {
                    if let ClientMachinesView::Forwards(view) = &mut overlay.view {
                        view.selected = (view.selected + 1).min(count.saturating_sub(1));
                    }
                }
            }
            KeyCode::Char('a') if plain => {
                if let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() {
                    if let ClientMachinesView::Forwards(view) = &mut overlay.view {
                        view.adding = true;
                        view.message = None;
                        view.error = None;
                    }
                }
            }
            KeyCode::Char('x') if plain => self.forward_remove_selected(),
            _ => {}
        }
        outcome.repaint = true;
    }

    fn route_machine_import_key(
        &mut self,
        key: &crate::input::TerminalKey,
        code: KeyCode,
        modifiers: KeyModifiers,
        outcome: &mut ClientShellInput,
    ) {
        let plain = modifiers.is_empty();
        let step = match self.overlay.as_ref() {
            Some(ClientShellOverlay::Machines(overlay)) => match &overlay.view {
                ClientMachinesView::Import(view) => view.step,
                _ => return,
            },
            _ => return,
        };
        match step {
            ClientImportStep::Discover => {
                match code {
                    KeyCode::Esc => self.machines_back(),
                    KeyCode::Enter => {
                        let advance = matches!(
                            self.overlay.as_ref(),
                            Some(ClientShellOverlay::Machines(overlay))
                                if matches!(&overlay.view, ClientMachinesView::Import(view) if !view.plan.ready.is_empty() && view.fatal.is_none())
                        );
                        if advance {
                            if let Some(ClientShellOverlay::Machines(overlay)) =
                                self.overlay.as_mut()
                            {
                                if let ClientMachinesView::Import(view) = &mut overlay.view {
                                    view.step = ClientImportStep::Select;
                                }
                            }
                        }
                    }
                    _ => {}
                }
                outcome.repaint = true;
            }
            ClientImportStep::Select => {
                if code == KeyCode::Esc {
                    self.machines_back();
                    outcome.repaint = true;
                    return;
                }
                let group_focused = matches!(
                    self.overlay.as_ref(),
                    Some(ClientShellOverlay::Machines(overlay))
                        if matches!(&overlay.view, ClientMachinesView::Import(view) if view.focus_row == view.plan.ready.len() + 1)
                );
                if group_focused {
                    if code == KeyCode::Enter {
                        self.execute_machine_import();
                        outcome.repaint = true;
                        return;
                    }
                    let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() else {
                        return;
                    };
                    let ClientMachinesView::Import(view) = &mut overlay.view else {
                        return;
                    };
                    match code {
                        KeyCode::Tab if plain => view.focus_row = 0,
                        KeyCode::Up if plain => self.move_import_focus(-1),
                        _ => {
                            outcome.repaint |= view.group.handle_key(key).is_some();
                            return;
                        }
                    }
                    outcome.repaint = true;
                    return;
                }
                match code {
                    KeyCode::Up | KeyCode::Char('k') if plain => self.move_import_focus(-1),
                    KeyCode::Down | KeyCode::Char('j') if plain => self.move_import_focus(1),
                    KeyCode::Tab if plain => self.move_import_focus(1),
                    KeyCode::BackTab if modifiers.difference(KeyModifiers::SHIFT).is_empty() => {
                        self.move_import_focus(-1)
                    }
                    KeyCode::Char(' ') if plain => self.import_toggle_focused(),
                    KeyCode::Char('a') if plain => self.import_toggle_all(),
                    KeyCode::Enter => self.execute_machine_import(),
                    _ => {}
                }
                outcome.repaint = true;
            }
            ClientImportStep::Done => {
                if matches!(code, KeyCode::Esc | KeyCode::Enter) {
                    self.machines_back();
                    outcome.repaint = true;
                }
            }
        }
    }

    pub(super) fn insert_machines_overlay_text(&mut self, text: &str) -> bool {
        let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() else {
            return false;
        };
        match &mut overlay.view {
            ClientMachinesView::Form(form) if form.bootstrap.is_none() => {
                let Some(field) = form.focused_field() else {
                    return false;
                };
                let Some(editor) = form.editor_mut(field) else {
                    return false;
                };
                editor.insert(text)
            }
            ClientMachinesView::Forwards(view) if view.adding => {
                let editor = match view.form.focused {
                    1 => &mut view.form.listen_port,
                    2 => &mut view.form.bind_address,
                    3 => &mut view.form.target_host,
                    _ => &mut view.form.target_port,
                };
                editor.insert(text)
            }
            ClientMachinesView::Import(view)
                if view.step == ClientImportStep::Select
                    && view.focus_row == view.plan.ready.len() + 1 =>
            {
                view.group.insert(text)
            }
            _ if overlay.search_focused => overlay.query.insert(text),
            _ => false,
        }
    }

    pub(super) fn route_machines_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) -> bool {
        if !matches!(self.overlay, Some(ClientShellOverlay::Machines(_))) {
            return false;
        }
        let (code, modifiers) = crate::config::normalize_key_combo((key.code, key.modifiers));
        let plain = modifiers.is_empty();

        enum ViewKind {
            List,
            Detail(ProfileId),
            ConfirmRemove(ProfileId),
            Form,
            Forwards,
            Import,
        }
        let view = match self.overlay.as_ref() {
            Some(ClientShellOverlay::Machines(overlay)) => match &overlay.view {
                ClientMachinesView::List => ViewKind::List,
                ClientMachinesView::Detail(id) => ViewKind::Detail(id.clone()),
                ClientMachinesView::ConfirmRemove(id) => ViewKind::ConfirmRemove(id.clone()),
                ClientMachinesView::Form(_) => ViewKind::Form,
                ClientMachinesView::Forwards(_) => ViewKind::Forwards,
                ClientMachinesView::Import(_) => ViewKind::Import,
            },
            _ => return false,
        };

        match view {
            ViewKind::Forwards => {
                self.route_machine_forwards_key(key, code, modifiers, outcome);
                return true;
            }
            ViewKind::Import => {
                self.route_machine_import_key(key, code, modifiers, outcome);
                return true;
            }
            _ => {}
        }

        match view {
            ViewKind::ConfirmRemove(id) => {
                if code == KeyCode::Enter {
                    self.machine_remove(&id);
                    self.machines_back();
                    outcome.repaint = true;
                } else if code == KeyCode::Esc {
                    self.machines_back();
                    outcome.repaint = true;
                }
                return true;
            }
            ViewKind::Detail(id) => {
                match code {
                    KeyCode::Esc => self.machines_back(),
                    KeyCode::Up | KeyCode::Char('k') if plain => {
                        if let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() {
                            overlay.detail_scroll = overlay.detail_scroll.saturating_sub(1);
                        }
                    }
                    KeyCode::Down | KeyCode::Char('j') if plain => {
                        if let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() {
                            overlay.detail_scroll =
                                overlay.detail_scroll.saturating_add(1).min(256);
                        }
                    }
                    KeyCode::Char('e') if plain => self.open_machine_edit_form(&id),
                    KeyCode::Char('R') if modifiers == KeyModifiers::SHIFT => {
                        self.open_machine_rename_overlay(&id)
                    }
                    KeyCode::Char('r') if plain => self.machine_reconnect(&id, outcome),
                    KeyCode::Char('v') if plain => {
                        self.open_machine_auth_for_endpoint(&id, outcome);
                    }
                    KeyCode::Char('d') if plain => self.machine_toggle_enabled(&id),
                    KeyCode::Char('x') if plain => {
                        if let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() {
                            overlay.view = ClientMachinesView::ConfirmRemove(id);
                        }
                    }
                    KeyCode::Char('c') if plain => self.machine_copy_fix_command(&id, outcome),
                    KeyCode::Char('f') if plain => self.open_machine_forwards(&id),
                    KeyCode::Char('b') if plain => self.open_broadcast_overlay(),
                    KeyCode::Char('o') if plain => self.open_machine_files(&id, outcome),
                    _ => return true,
                }
                outcome.repaint = true;
                return true;
            }
            ViewKind::Form => {
                self.route_machine_form_key(key, code, modifiers, outcome);
                return true;
            }
            ViewKind::Forwards | ViewKind::Import => {
                // Dispatched above; unreachable here.
                return true;
            }
            ViewKind::List => {}
        }

        // List view.
        let search_focused = matches!(
            self.overlay,
            Some(ClientShellOverlay::Machines(ClientMachinesOverlay {
                search_focused: true,
                ..
            }))
        );
        if search_focused {
            if code == KeyCode::Esc {
                if let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() {
                    overlay.search_focused = false;
                }
                outcome.repaint = true;
                return true;
            }
            if code == KeyCode::Enter {
                let id = self.selected_machine_id();
                if let Some(id) = id {
                    self.open_machine_detail(&id);
                }
                outcome.repaint = true;
                return true;
            }
            if code == KeyCode::Up {
                self.move_machines_selection(-1);
                outcome.repaint = true;
                return true;
            }
            if code == KeyCode::Down {
                self.move_machines_selection(1);
                outcome.repaint = true;
                return true;
            }
            if let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() {
                if let Some(content_changed) = overlay.query.handle_key(key) {
                    if content_changed {
                        overlay.selected = 0;
                        overlay.scroll = 0;
                    }
                    outcome.repaint = true;
                    return true;
                }
            }
            return true;
        }

        match code {
            KeyCode::Esc => {
                self.overlay = None;
                outcome.repaint = true;
            }
            KeyCode::Char('/') if plain => {
                if let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() {
                    overlay.search_focused = true;
                    overlay.message = None;
                }
                outcome.repaint = true;
            }
            KeyCode::Enter => {
                let id = self.selected_machine_id();
                if let Some(id) = id {
                    self.open_machine_detail(&id);
                }
                outcome.repaint = true;
            }
            KeyCode::Up | KeyCode::Char('k') if plain => {
                self.move_machines_selection(-1);
                outcome.repaint = true;
            }
            KeyCode::Down | KeyCode::Char('j') if plain => {
                self.move_machines_selection(1);
                outcome.repaint = true;
            }
            KeyCode::Char('u') if modifiers == KeyModifiers::CONTROL => {
                self.move_machines_selection(-8);
                outcome.repaint = true;
            }
            KeyCode::Char('d') if modifiers == KeyModifiers::CONTROL => {
                self.move_machines_selection(8);
                outcome.repaint = true;
            }
            KeyCode::Char('a') if plain => {
                self.open_machine_add_form();
                outcome.repaint = true;
            }
            KeyCode::Char('i') if plain => {
                self.open_machine_import_wizard();
                outcome.repaint = true;
            }
            KeyCode::Char('e' | 'r' | 'd' | 'x') if plain => {
                if let Some(id) = self.selected_machine_id() {
                    match code {
                        KeyCode::Char('e') => self.open_machine_edit_form(&id),
                        KeyCode::Char('r') => self.machine_reconnect(&id, outcome),
                        KeyCode::Char('d') => self.machine_toggle_enabled(&id),
                        KeyCode::Char('x') => {
                            if let Some(ClientShellOverlay::Machines(overlay)) =
                                self.overlay.as_mut()
                            {
                                overlay.view = ClientMachinesView::ConfirmRemove(id);
                            }
                        }
                        _ => {}
                    }
                }
                outcome.repaint = true;
            }
            KeyCode::Char('R') if modifiers == KeyModifiers::SHIFT => {
                if let Some(id) = self.selected_machine_id() {
                    self.open_machine_rename_overlay(&id);
                }
                outcome.repaint = true;
            }
            KeyCode::Char('b') if plain => {
                self.open_broadcast_overlay();
                outcome.repaint = true;
            }
            _ => {}
        }
        true
    }

    fn open_machine_rename_overlay(&mut self, profile_id: &ProfileId) {
        let Some(profile) = self.saved_profile(profile_id).cloned() else {
            return;
        };
        self.overlay = Some(ClientShellOverlay::Rename(ClientRenameOverlay {
            title: crate::i18n::texts().machines.edit_title,
            input: TextEditor::new(&profile.label, false),
            target: ClientRenameTarget::Machine {
                profile_id: profile_id.clone(),
            },
        }));
    }

    fn route_machine_form_key(
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
            let max = self.hits.machines_max_scroll;
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
    fn wizard_temp_profile(form: &ClientMachineForm) -> Result<SavedSshEndpoint, String> {
        let options = form.profile_options()?;
        SavedSshEndpoint::with_options(
            form.effective_label(),
            form.target.trim(),
            form.effective_session(),
            options,
        )
    }

    /// Mouse activation for one rendered machines-overlay button.
    pub(super) fn activate_machine_button(
        &mut self,
        button: MachineOverlayButton,
        outcome: &mut ClientShellInput,
    ) {
        use MachineOverlayButton as Btn;
        let (detail_id, form_running) = match self.overlay.as_ref() {
            Some(ClientShellOverlay::Machines(overlay)) => (
                match &overlay.view {
                    ClientMachinesView::Detail(id) | ClientMachinesView::ConfirmRemove(id) => {
                        Some(id.clone())
                    }
                    ClientMachinesView::List => self.selected_machine_id(),
                    _ => None,
                },
                matches!(
                    &overlay.view,
                    ClientMachinesView::Form(form) if form.bootstrap.is_some()
                ),
            ),
            _ => (None, false),
        };
        match button {
            Btn::Add => self.open_machine_add_form(),
            Btn::Close => {
                if form_running {
                    return;
                }
                self.overlay = None;
            }
            Btn::Back => self.machines_back(),
            Btn::Next | Btn::Save | Btn::StartSetup => self.advance_machine_form(outcome),
            Btn::Import => self.open_machine_import_wizard(),
            Btn::ImportContinue => {
                if let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() {
                    if let ClientMachinesView::Import(view) = &mut overlay.view {
                        if view.step == ClientImportStep::Discover
                            && view.fatal.is_none()
                            && !view.plan.ready.is_empty()
                        {
                            view.step = ClientImportStep::Select;
                        }
                    }
                }
            }
            Btn::ImportRun => self.execute_machine_import(),
            Btn::Forwards => {
                if let Some(id) = detail_id {
                    self.open_machine_forwards(&id);
                }
            }
            Btn::ForwardAddStart => {
                if let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() {
                    if let ClientMachinesView::Forwards(view) = &mut overlay.view {
                        view.adding = true;
                        view.message = None;
                        view.error = None;
                    }
                }
            }
            Btn::ForwardRemove => self.forward_remove_selected(),
            Btn::ForwardSave => self.forward_save_new_rule(),
            Btn::ForwardCancel => {
                if let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() {
                    if let ClientMachinesView::Forwards(view) = &mut overlay.view {
                        view.adding = false;
                        view.error = None;
                    }
                }
            }
            Btn::Edit => {
                if let Some(id) = detail_id {
                    self.open_machine_edit_form(&id);
                }
            }
            Btn::Reconnect => {
                if let Some(id) = detail_id {
                    self.machine_reconnect(&id, outcome);
                }
            }
            Btn::ToggleEnabled => {
                if let Some(id) = detail_id {
                    self.machine_toggle_enabled(&id);
                }
            }
            Btn::Remove => {
                if let Some(id) = detail_id {
                    if let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() {
                        overlay.view = ClientMachinesView::ConfirmRemove(id);
                    }
                }
            }
            Btn::CopyFix => {
                if let Some(id) = detail_id {
                    self.machine_copy_fix_command(&id, outcome);
                }
            }
            Btn::Broadcast => self.open_broadcast_overlay(),
            Btn::BrowseFiles => {
                if let Some(id) = detail_id {
                    self.open_machine_files(&id, outcome);
                }
            }
            Btn::ConfirmRemove => {
                if let Some(id) = detail_id {
                    self.machine_remove(&id);
                    self.machines_back();
                }
            }
            Btn::CancelRemove => self.machines_back(),
            Btn::ReviewIssue => {
                if let Some(id) = detail_id {
                    self.open_machine_auth_for_endpoint(&id, outcome);
                }
            }
            Btn::WizardInteractiveAuth => {
                let prepared = match self.overlay.as_ref() {
                    Some(ClientShellOverlay::Machines(overlay)) => match &overlay.view {
                        ClientMachinesView::Form(form)
                            if form
                                .bootstrap
                                .as_ref()
                                .is_some_and(|bootstrap| bootstrap.failure.is_some()) =>
                        {
                            Some(Self::wizard_temp_profile(form))
                        }
                        _ => None,
                    },
                    _ => None,
                };
                match prepared {
                    Some(Ok(profile)) => self.open_machine_auth_guide(Box::new(profile), outcome),
                    Some(Err(error)) => {
                        if let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() {
                            if let ClientMachinesView::Form(form) = &mut overlay.view {
                                form.error = Some(error);
                            }
                        }
                    }
                    None => {}
                }
            }
            Btn::WizardHostKeyReview => {
                let target = match self.overlay.as_ref() {
                    Some(ClientShellOverlay::Machines(overlay)) => match &overlay.view {
                        ClientMachinesView::Form(form)
                            if form
                                .bootstrap
                                .as_ref()
                                .is_some_and(|bootstrap| bootstrap.failure.is_some()) =>
                        {
                            Self::wizard_temp_profile(form).ok()
                        }
                        _ => None,
                    },
                    _ => None,
                };
                if let Some(target) = target {
                    self.open_machine_host_key_review(Box::new(target), outcome);
                }
            }
        }
        outcome.repaint = true;
    }

    /// Focus a form field by mouse; choice fields also cycle one step.
    pub(super) fn focus_machine_form_field(&mut self, field: MachineField) {
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

    /// 指针悬浮的机器行。`None` 表示指针不在任何行上——出界也要写，否则弱
    /// 底色会留在鼠标早已离开的那一行（MENU-01）。只写 `hovered`，不动
    /// `selected`。返回 true 表示悬浮项变了。
    pub(super) fn hover_machine_row(&mut self, profile_id: Option<&ProfileId>) -> bool {
        let Some(ClientShellOverlay::Machines(overlay)) = self.content_page_mut() else {
            return false;
        };
        let hovered = profile_id.cloned();
        let changed = overlay.hovered != hovered;
        overlay.hovered = hovered;
        changed
    }

    /// Mouse-wheel scrolling routed per view: list moves the selection,
    /// detail scrolls the field card, the form moves the focused field.
    pub(super) fn scroll_machine_details(&mut self, delta: isize) {
        if let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() {
            overlay.detail_scroll = overlay
                .detail_scroll
                .saturating_add_signed(delta)
                .min(self.hits.machines_max_scroll);
        }
    }

    pub(super) fn scroll_machines_overlay(&mut self, delta: isize) {
        enum ScrollTarget {
            List,
            Detail,
            Form,
            Forwards,
            Import,
            None,
        }
        let target = match self.overlay.as_ref() {
            Some(ClientShellOverlay::Machines(overlay)) => match overlay.view {
                ClientMachinesView::List => ScrollTarget::List,
                ClientMachinesView::Detail(_) => ScrollTarget::Detail,
                ClientMachinesView::Form(_) => ScrollTarget::Form,
                ClientMachinesView::ConfirmRemove(_) => ScrollTarget::None,
                ClientMachinesView::Forwards(_) => ScrollTarget::Forwards,
                ClientMachinesView::Import(_) => ScrollTarget::Import,
            },
            _ => ScrollTarget::None,
        };
        match target {
            ScrollTarget::List => {
                if let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() {
                    overlay.scroll = overlay.scroll.saturating_add_signed(delta);
                    overlay.reveal = false;
                }
            }
            ScrollTarget::Detail => {
                let max_scroll = self.hits.machines_max_scroll;
                if let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() {
                    overlay.detail_scroll = if delta.is_negative() {
                        overlay.detail_scroll.saturating_sub(delta.unsigned_abs())
                    } else {
                        overlay.detail_scroll.saturating_add(delta as usize)
                    }
                    .min(max_scroll);
                }
            }
            ScrollTarget::Form => {
                let max = self.hits.machines_max_scroll;
                if let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() {
                    if let ClientMachinesView::Form(form) = &mut overlay.view {
                        if form.step == MachineFormStep::Confirm && form.editing.is_none() {
                            form.scroll = form.scroll.saturating_add_signed(delta).min(max);
                            return;
                        }
                        let count = form.fields().len();
                        if count > 0 {
                            form.focused = (form.focused as isize + delta)
                                .clamp(0, count.saturating_sub(1) as isize)
                                as usize;
                        }
                    }
                }
            }
            ScrollTarget::Forwards => {
                if let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() {
                    if let ClientMachinesView::Forwards(view) = &mut overlay.view {
                        view.selected = (view.selected as isize + delta).max(0) as usize;
                    }
                }
            }
            ScrollTarget::Import => self.move_import_focus(delta),
            ScrollTarget::None => {}
        }
    }
}

// ---------- rendering ----------

fn machine_color_tag(
    profile_color: Option<&str>,
    label: &str,
    p: &Palette,
) -> ratatui::style::Color {
    if let Some(color) = profile_color.and_then(crate::config::try_parse_color) {
        return color;
    }
    machine_hash_color(label, p)
}

/// Stable fallback tag for profiles without an explicit color: hash the label
/// into a small rotation of palette hues.
pub(super) fn machine_hash_color(label: &str, p: &Palette) -> ratatui::style::Color {
    let hues = [p.blue, p.teal, p.green, p.yellow, p.peach, p.mauve, p.red];
    let hash = label.bytes().fold(0u32, |acc, byte| {
        acc.wrapping_mul(31).wrapping_add(u32::from(byte))
    });
    hues[(hash as usize) % hues.len()]
}

pub(super) fn render_machines_overlay(
    b: &mut Buffer,
    overlay: &ClientMachinesOverlay,
    endpoints: &[ClientShellEndpoint],
    saved_profiles: &[SavedSshEndpoint],
    connection_errors: &HashMap<ClientEndpointId, crate::remote::ConnectionErrorKind>,
    port_forwards: &HashMap<ClientEndpointId, Vec<crate::remote::PortForwardStatus>>,
    session_log_dropped: &HashMap<ProfileId, u64>,
    cx: &super::feedback::ChromeContext<'_>,
) -> Option<OverlayRender> {
    if matches!(overlay.view, ClientMachinesView::List)
        && cx.page_bounds.unwrap_or(b.area).width >= 96
    {
        return dashboard::render_dashboard(
            b,
            overlay,
            endpoints,
            saved_profiles,
            connection_errors,
            port_forwards,
            session_log_dropped,
            cx,
        );
    }
    match &overlay.view {
        ClientMachinesView::List => render_machine_list(b, overlay, endpoints, saved_profiles, cx),
        ClientMachinesView::Detail(id) => {
            if !saved_profiles.iter().any(|profile| &profile.id == id) {
                // The profile vanished underneath the overlay (external edit);
                // degrade to the list rather than aborting composition.
                return render_machine_list(b, overlay, endpoints, saved_profiles, cx);
            }
            render_machine_detail(
                b,
                overlay,
                id,
                endpoints,
                saved_profiles,
                connection_errors,
                port_forwards,
                session_log_dropped,
                cx,
            )
        }
        ClientMachinesView::ConfirmRemove(id) => {
            if !saved_profiles.iter().any(|profile| &profile.id == id) {
                return render_machine_list(b, overlay, endpoints, saved_profiles, cx);
            }
            render_machine_confirm_remove(b, id, saved_profiles, cx)
        }
        ClientMachinesView::Form(form) => render_machine_form(b, form, cx),
        ClientMachinesView::Forwards(view) => {
            if !saved_profiles
                .iter()
                .any(|profile| profile.id == view.profile_id)
            {
                return render_machine_list(b, overlay, endpoints, saved_profiles, cx);
            }
            render_machine_forwards(b, view, saved_profiles, port_forwards, cx)
        }
        ClientMachinesView::Import(view) => render_machine_import(b, view, cx),
    }
}

fn render_machine_list(
    b: &mut Buffer,
    overlay: &ClientMachinesOverlay,
    endpoints: &[ClientShellEndpoint],
    saved_profiles: &[SavedSshEndpoint],
    cx: &super::feedback::ChromeContext<'_>,
) -> Option<OverlayRender> {
    let p = cx.palette;
    let t = &crate::i18n::texts().machines;
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
        &format!(" {}", t.title),
        base.fg(p.text).add_modifier(Modifier::BOLD),
    );
    let rows = machine_list_rows(saved_profiles, endpoints, overlay.query.as_str());
    let count = crate::i18n::fill(t.count_fmt, &[("count", &rows.len().to_string())]);
    let cursor = render_search_bar(
        b,
        Rect::new(stack.header.x, stack.header.y + 1, stack.header.width, 1),
        &SearchBar {
            focused: overlay.search_focused,
            query: &overlay.query,
            hint: t.search_hint,
            status: None,
            echo_query: true,
            count: Some(count),
        },
        p,
    );

    let body = stack.content;
    let row_height = 2usize;
    let visible = (usize::from(body.height) / row_height).max(1);
    let selected = if rows.is_empty() {
        0
    } else {
        overlay.selected.min(rows.len() - 1)
    };
    let scroll = super::page::list_start(
        overlay.scroll,
        selected,
        rows.len(),
        visible,
        overlay.reveal,
    );
    let mut row_hits = Vec::new();
    for (index, row) in rows.iter().enumerate().skip(scroll).take(visible) {
        let y = body.y + ((index - scroll) * row_height) as u16;
        let rect = Rect::new(body.x, y, body.width, row_height as u16);
        row_hits.push((rect, row.id.clone()));
        let is_selected = index == selected;
        // 三态与其它浮层同一口径：选中 accent 反色 > 悬浮弱底色 > 常态。
        let is_hovered = !is_selected && overlay.hovered.as_ref() == Some(&row.id);
        let row_bg = super::list_row_bg(p, cx.components, is_selected, is_hovered);
        let style = if is_selected {
            Style::default()
                .fg(panel_contrast_fg(p))
                .bg(p.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(p.text).bg(row_bg)
        };
        b.set_style(rect, style);
        let tag = machine_color_tag(row.color.as_deref(), &row.label, p);
        let tag_style = if is_selected {
            style
        } else {
            Style::default().fg(tag).bg(row_bg)
        };
        put_text(b, rect.x, rect.y, 2, " ▪", tag_style);
        put_text(
            b,
            rect.x + 2,
            rect.y,
            rect.width.saturating_sub(2),
            &format!(" {}", row.label),
            if row.enabled {
                style
            } else {
                style.add_modifier(Modifier::DIM)
            },
        );
        let (glyph, state, color) = endpoint_status_presentation(row.status, p, cx.spinner);
        let signal = if row.status == ClientEndpointStatus::Online {
            glyph.to_owned()
        } else {
            format!("{glyph} {state}")
        };
        let signal_style = if is_selected {
            style
        } else {
            Style::default().fg(color).bg(row_bg)
        };
        put_right_text(b, rect, rect.y, &signal, signal_style);
        let meta_style = if is_selected {
            style
        } else {
            Style::default().fg(p.overlay0).bg(row_bg)
        };
        let group = row.group.as_deref().unwrap_or_default();
        let meta = if group.is_empty() {
            format!("   {}", row.target)
        } else {
            format!("   {} · {}", row.target, group)
        };
        put_text(b, rect.x, rect.y + 1, rect.width, &meta, meta_style);
        if let Some(version) = row.server_version.as_deref() {
            put_right_text(b, rect, rect.y + 1, &format!("v{version}"), meta_style);
        }
    }
    if rows.is_empty() {
        put_text(b, body.x, body.y, body.width, t.empty, base.fg(p.overlay1));
        put_text(
            b,
            body.x,
            body.y + 1,
            body.width,
            t.empty_hint,
            base.fg(p.overlay0),
        );
    }
    if let Some(footer) = stack.footer {
        if let Some(message) = overlay.message.as_deref() {
            put_text(
                b,
                footer.x,
                footer.y,
                footer.width,
                message,
                base.fg(p.green),
            );
        } else {
            render_key_hints(
                b,
                footer,
                &[
                    ("↑↓".to_owned(), t.hint_select.to_owned()),
                    ("enter".to_owned(), t.hint_details.to_owned()),
                    ("a".to_owned(), t.hint_add.to_owned()),
                    ("i".to_owned(), t.hint_import.to_owned()),
                    ("b".to_owned(), t.hint_broadcast.to_owned()),
                    ("/".to_owned(), t.hint_filter.to_owned()),
                    ("esc".to_owned(), t.hint_close.to_owned()),
                ],
                p,
                cx.components,
            );
        }
    }

    let mut action_hits = Vec::new();
    let add_label = t.add_button;
    let import_label = t.import_button;
    let close_label = crate::ui::modal_close_button_text();
    let buttons = modal_button_row(
        stack.actions.unwrap_or_default(),
        &[add_label, import_label, close_label],
        2,
    );
    if let [add, import, close] = buttons.as_slice() {
        modal_button(
            b,
            *add,
            add_label,
            crate::ui::ModalButtonTone::Primary,
            cx.button_state(
                &super::feedback::ChromeHover::MachineButton(MachineOverlayButton::Add),
                crate::ui::ModalButtonState::Focused,
            ),
            p,
        );
        modal_button(
            b,
            *import,
            import_label,
            crate::ui::ModalButtonTone::Secondary,
            cx.button_state(
                &super::feedback::ChromeHover::MachineButton(MachineOverlayButton::Import),
                crate::ui::ModalButtonState::Normal,
            ),
            p,
        );
        modal_button(
            b,
            *close,
            close_label,
            crate::ui::ModalButtonTone::Secondary,
            cx.button_state(
                &super::feedback::ChromeHover::MachineButton(MachineOverlayButton::Close),
                crate::ui::ModalButtonState::Normal,
            ),
            p,
        );
        action_hits.push((*add, MachineOverlayButton::Add));
        action_hits.push((*import, MachineOverlayButton::Import));
        action_hits.push((*close, MachineOverlayButton::Close));
    }

    Some(OverlayRender {
        area: popup,
        machines_popup: popup,
        machines_scroll: scroll,
        machines_search: Rect::new(stack.header.x, stack.header.y + 1, stack.header.width, 1),
        machines_rows: row_hits,
        machines_actions: action_hits,
        machines_max_scroll: rows.len().saturating_sub(visible),
        cursor,
        ..OverlayRender::default()
    })
}

/// Rule one-liner shared by the detail card and the rules editor (matches
/// the CLI's `machine forward list` text).
pub(super) fn forward_rule_display(rule: &PortForwardRule) -> String {
    let listen = match &rule.bind_address {
        Some(bind) => format!("{bind}:{}", rule.listen_port),
        None => rule.listen_port.to_string(),
    };
    match rule.kind {
        PortForwardKind::Local | PortForwardKind::Remote => format!(
            "{} {listen} -> {}:{}",
            rule.kind.as_str(),
            rule.target_host.as_deref().unwrap_or_default(),
            rule.target_port.unwrap_or_default()
        ),
        PortForwardKind::Dynamic => format!("{} {listen} (SOCKS)", rule.kind.as_str()),
    }
}

fn detail_lines(
    profile: &SavedSshEndpoint,
    endpoint: Option<&ClientShellEndpoint>,
    forward_status: Option<&[crate::remote::PortForwardStatus]>,
    session_log_dropped: Option<u64>,
) -> Vec<(String, String)> {
    let t = &crate::i18n::texts().machines;
    let mut lines: Vec<(String, String)> = Vec::new();
    let yes_no = |value: Option<bool>| match value {
        Some(true) => t.choice_yes.to_owned(),
        Some(false) => t.choice_no.to_owned(),
        None => t.value_not_set.to_owned(),
    };
    let mut push = |label: &str, value: String| lines.push((label.to_owned(), value));
    push(t.detail_target, profile.target.clone());
    push(t.detail_session, profile.session.clone());
    push(t.detail_enabled, yes_no(Some(profile.enabled)));
    if let Some(endpoint) = endpoint {
        push(
            t.detail_status,
            endpoint_status_label(endpoint.status).to_owned(),
        );
        if let Some(version) = endpoint.server_version.as_deref() {
            push(t.detail_server_version, format!("v{version}"));
        }
        if let Some(detail) = endpoint.status_detail.as_deref() {
            push(t.detail_last_error, detail.to_owned());
        }
    }
    push(t.detail_id, profile.id.to_string());
    if let Some(group) = profile.group.as_deref() {
        push(t.detail_group, group.to_owned());
    }
    if !profile.tags.is_empty() {
        push(t.detail_tags, profile.tags.join(", "));
    }
    if let Some(color) = profile.color.as_deref() {
        push(t.detail_color, color.to_owned());
    }
    if let Some(port) = profile.port {
        push(t.detail_port, port.to_string());
    }
    if let Some(user) = profile.user.as_deref() {
        push(t.detail_user, user.to_owned());
    }
    if !profile.identity_file.is_empty() {
        push(t.detail_identity_files, profile.identity_file.join(", "));
    }
    if profile.identities_only.is_some() {
        push(t.detail_identities_only, yes_no(profile.identities_only));
    }
    if let Some(agent) = profile.identity_agent.as_deref() {
        push(t.detail_identity_agent, agent.to_owned());
    }
    if let Some(checking) = profile.strict_host_key_checking {
        push(t.detail_strict_host_key, checking.as_ssh_value().to_owned());
    }
    if !profile.proxy_jump.is_empty() {
        push(
            t.detail_proxy_jump,
            profile
                .proxy_jump
                .iter()
                .map(|hop| match hop {
                    ProxyJumpHop::Target(target) => target.clone(),
                    ProxyJumpHop::Profile(id) => format!("profile:{id}"),
                })
                .collect::<Vec<_>>()
                .join(", "),
        );
    }
    if profile.forward_agent.is_some() {
        push(t.detail_forward_agent, yes_no(profile.forward_agent));
    }
    if let Some(interval) = profile.server_alive_interval {
        push(t.detail_server_alive_interval, interval.to_string());
    }
    if let Some(count) = profile.server_alive_count_max {
        push(t.detail_server_alive_count_max, count.to_string());
    }
    if let Some(persist) = profile.control_persist.as_deref() {
        push(t.detail_control_persist, persist.to_owned());
    }
    if let Some(command) = profile.remote_command.as_deref() {
        push(t.detail_remote_command, command.to_owned());
    }
    // Session logging: the saved preferences plus this machine's own live
    // writer-queue drop counter when the supervisor mirror has one.
    if let Some(log) = profile.session_log.as_ref() {
        let mut value = if log.enabled {
            t.choice_yes.to_owned()
        } else {
            t.choice_no.to_owned()
        };
        if log.enabled {
            let template = log
                .path_template
                .as_deref()
                .unwrap_or(crate::client::endpoint::DEFAULT_PATH_TEMPLATE);
            let max_bytes = log
                .max_bytes
                .unwrap_or(crate::client::endpoint::DEFAULT_MAX_LOG_BYTES);
            let interval = log
                .dump_interval_secs
                .unwrap_or(crate::client::endpoint::DEFAULT_DUMP_INTERVAL_SECS);
            value = format!("{value} · {template} · {max_bytes} B · {interval} s");
            if let Some(dropped) = session_log_dropped.filter(|dropped| *dropped > 0) {
                value = format!(
                    "{value} · {}",
                    crate::i18n::fill(
                        t.session_log_dropped_fmt,
                        &[("count", &dropped.to_string())]
                    )
                );
            }
        }
        push(t.detail_session_log, value);
    }
    // Port forwards: saved rules plus their live phase when the supervisor
    // mirror has one (offline machines show rules with the waiting note).
    if !profile.port_forwards.is_empty() {
        for (index, rule) in profile.port_forwards.iter().enumerate() {
            let label = if index == 0 {
                t.detail_port_forwards.to_owned()
            } else {
                String::new()
            };
            let status = forward_status
                .and_then(|statuses| statuses.iter().find(|status| status.rule == *rule));
            let text = match status {
                Some(status) => match status.phase {
                    crate::remote::PortForwardPhase::Active => format!(
                        "{} · {}",
                        forward_rule_display(rule),
                        t.forward_status_active
                    ),
                    crate::remote::PortForwardPhase::Failed => format!(
                        "{} · {}: {}",
                        forward_rule_display(rule),
                        t.forward_status_failed,
                        status.detail.as_deref().unwrap_or_default()
                    ),
                },
                None => format!("{} · {}", forward_rule_display(rule), t.forward_waiting),
            };
            lines.push((label, text));
        }
    }
    lines
}

fn render_machine_detail(
    b: &mut Buffer,
    overlay: &ClientMachinesOverlay,
    profile_id: &ProfileId,
    endpoints: &[ClientShellEndpoint],
    saved_profiles: &[SavedSshEndpoint],
    connection_errors: &HashMap<ClientEndpointId, crate::remote::ConnectionErrorKind>,
    port_forwards: &HashMap<ClientEndpointId, Vec<crate::remote::PortForwardStatus>>,
    session_log_dropped: &HashMap<ProfileId, u64>,
    cx: &super::feedback::ChromeContext<'_>,
) -> Option<OverlayRender> {
    let p = cx.palette;
    let t = &crate::i18n::texts().machines;
    let profile = saved_profiles
        .iter()
        .find(|profile| &profile.id == profile_id);
    let profile = profile?;
    let endpoint = endpoint_for(endpoints, profile_id);
    let status = endpoint.map_or(
        if profile.enabled {
            ClientEndpointStatus::Connecting
        } else {
            ClientEndpointStatus::Disabled
        },
        |endpoint| endpoint.status,
    );
    let error_kind = connection_errors.get(&ClientEndpointId::Ssh(profile_id.clone()));
    let (popup, inner) = modal_panel(b, crate::ui::ModalSize::Large.with_height(26), p.accent, cx)?;
    if inner.width < 24 || inner.height < 8 {
        return Some(OverlayRender {
            area: popup,
            machines_popup: popup,
            ..OverlayRender::default()
        });
    }
    let has_review = error_kind.is_some_and(super::machine_auth_overlay::failure_kind_has_review);
    let mut labels: Vec<&str> = vec![t.reconnect_button, t.edit_button];
    labels.push(if profile.enabled {
        t.disable_button
    } else {
        t.enable_button
    });
    labels.push(t.forwards_button);
    labels.push(t.browse_files_button);
    labels.push(t.broadcast_button);
    labels.push(t.remove_button);
    if status == ClientEndpointStatus::Attention && has_review {
        labels.push(crate::i18n::texts().machine_auth.review_button);
    }
    if status == ClientEndpointStatus::Attention {
        labels.push(t.copy_fix_button);
    }
    labels.push(crate::ui::modal_close_button_text());
    let mut buttons: Vec<MachineOverlayButton> = vec![
        MachineOverlayButton::Reconnect,
        MachineOverlayButton::Edit,
        MachineOverlayButton::ToggleEnabled,
        MachineOverlayButton::Forwards,
        MachineOverlayButton::BrowseFiles,
        MachineOverlayButton::Broadcast,
        MachineOverlayButton::Remove,
    ];
    if status == ClientEndpointStatus::Attention && has_review {
        buttons.push(MachineOverlayButton::ReviewIssue);
    }
    if status == ClientEndpointStatus::Attention {
        buttons.push(MachineOverlayButton::CopyFix);
    }
    buttons.push(MachineOverlayButton::Close);
    let stack = super::page::PageLayout::with_action_rows(
        inner,
        0,
        false,
        super::page::action_row_count(inner.width, &labels),
    );
    let base = Style::default()
        .bg(p.panel_bg)
        .remove_modifier(Modifier::DIM);
    let tag = machine_color_tag(profile.color.as_deref(), &profile.label, p);
    put_text(b, stack.header.x, stack.header.y, 2, " ▪", base.fg(tag));
    put_text(
        b,
        stack.header.x + 2,
        stack.header.y,
        stack.header.width.saturating_sub(2),
        &format!(" {}", profile.label),
        base.fg(p.text).add_modifier(Modifier::BOLD),
    );
    let (glyph, state, color) = endpoint_status_presentation(status, p, cx.spinner);
    let header_row = Rect::new(stack.header.x, stack.header.y, stack.header.width, 1);
    put_right_text(
        b,
        header_row,
        stack.header.y,
        &format!("{glyph} {state}"),
        base.fg(color),
    );

    let body = stack.content;
    let forward_status = port_forwards.get(&ClientEndpointId::Ssh(profile_id.clone()));
    let lines = detail_lines(
        profile,
        endpoint,
        forward_status.map(Vec::as_slice),
        session_log_dropped.get(profile_id).copied(),
    );
    let label_width = lines
        .iter()
        .map(|(label, _)| usize::from(display_width(label)))
        .max()
        .unwrap_or(0)
        .min(28);
    let attention = status == ClientEndpointStatus::Attention;
    // Structured failure classification rows replace the plain fix hint when
    // the supervisor's error kind is mirrored (it always is for fresh
    // failures); otherwise the legacy two rows remain.
    let attention_rows = if attention && error_kind.is_some() {
        4
    } else if attention {
        2
    } else {
        0
    };
    let visible = usize::from(body.height).saturating_sub(attention_rows);
    let max_scroll = lines.len().saturating_sub(visible.max(1));
    let scroll = overlay.detail_scroll.min(max_scroll);
    for (offset, (label, value)) in lines.iter().skip(scroll).take(visible.max(1)).enumerate() {
        let y = body.y + offset as u16;
        put_text(
            b,
            body.x,
            y,
            (label_width + 2) as u16,
            &format!(" {label}"),
            base.fg(p.overlay0),
        );
        put_text(
            b,
            body.x + label_width as u16 + 2,
            y,
            body.width.saturating_sub(label_width as u16 + 2),
            value,
            base.fg(p.text),
        );
    }
    let mut action_hits = Vec::new();
    if attention {
        let mut y = body.y + visible.max(1) as u16;
        let mut put_attention_line = |b: &mut Buffer, text: &str, style: Style| {
            if y < body.bottom() {
                put_text(b, body.x, y, body.width, text, style);
            }
            y += 1;
        };
        if let Some(kind) = error_kind {
            let (kind_label, next_hint) =
                super::machine_auth_overlay::failure_kind_presentation(kind);
            put_attention_line(
                b,
                &format!(
                    " {}: {kind_label}",
                    crate::i18n::texts().machine_auth.detail_failure
                ),
                base.fg(p.red),
            );
            put_attention_line(b, next_hint, base.fg(p.yellow));
        }
        put_attention_line(b, t.fix_hint, base.fg(p.yellow));
        let command = crate::remote::saved_ssh_bootstrap_command(&profile.target, &profile.session);
        put_attention_line(
            b,
            &format!("  {command}"),
            base.fg(p.text).add_modifier(Modifier::BOLD),
        );
    }

    {
        let footer = stack.footer;
        let mut hints: Vec<(String, String)> = vec![
            ("e".to_owned(), t.hint_edit.to_owned()),
            ("R".to_owned(), t.hint_rename.to_owned()),
            ("r".to_owned(), t.hint_reconnect.to_owned()),
        ];
        if attention {
            hints.push(("v".to_owned(), t.hint_review.to_owned()));
        }
        hints.extend([
            ("d".to_owned(), t.hint_toggle_enabled.to_owned()),
            ("x".to_owned(), t.hint_remove.to_owned()),
            ("f".to_owned(), t.hint_forwards.to_owned()),
            ("o".to_owned(), t.hint_browse_files.to_owned()),
            ("b".to_owned(), t.hint_broadcast.to_owned()),
            ("esc".to_owned(), t.hint_back.to_owned()),
        ]);
        render_key_hints(b, footer, &hints, p, cx.components);
    }

    let rects = super::page::action_grid(stack.actions, &labels);
    if rects.len() == labels.len() {
        for (index, rect) in rects.iter().enumerate() {
            let button = buttons[index];
            let (tone, state) = match button {
                MachineOverlayButton::Reconnect if !profile.enabled => (
                    crate::ui::ModalButtonTone::Secondary,
                    crate::ui::ModalButtonState::Disabled,
                ),
                MachineOverlayButton::Reconnect | MachineOverlayButton::Edit => (
                    crate::ui::ModalButtonTone::Primary,
                    crate::ui::ModalButtonState::Focused,
                ),
                MachineOverlayButton::ReviewIssue => (
                    crate::ui::ModalButtonTone::Primary,
                    crate::ui::ModalButtonState::Focused,
                ),
                MachineOverlayButton::Remove => (
                    crate::ui::ModalButtonTone::Danger,
                    crate::ui::ModalButtonState::Normal,
                ),
                _ => (
                    crate::ui::ModalButtonTone::Secondary,
                    crate::ui::ModalButtonState::Normal,
                ),
            };
            let state =
                cx.button_state(&super::feedback::ChromeHover::MachineButton(button), state);
            modal_button(b, *rect, labels[index], tone, state, p);
            action_hits.push((*rect, button));
        }
    }

    Some(OverlayRender {
        area: popup,
        machines_popup: popup,
        machines_actions: action_hits,
        machines_max_scroll: max_scroll,
        ..OverlayRender::default()
    })
}

fn render_machine_confirm_remove(
    b: &mut Buffer,
    profile_id: &ProfileId,
    saved_profiles: &[SavedSshEndpoint],
    cx: &super::feedback::ChromeContext<'_>,
) -> Option<OverlayRender> {
    let p = cx.palette;
    let t = &crate::i18n::texts().machines;
    let label = saved_profiles
        .iter()
        .find(|profile| &profile.id == profile_id)
        .map(|profile| profile.label.clone())
        .unwrap_or_else(|| profile_id.to_string());
    let (popup, inner) = modal_panel(b, crate::ui::ModalSize::Medium.with_height(6), p.red, cx)?;
    let stack = crate::ui::modal_stack_areas(inner, 2, 0, 1, 0);
    put_text(
        b,
        stack.header.x,
        stack.header.y,
        stack.header.width,
        &format!(
            " {}",
            crate::i18n::fill(t.remove_title_fmt, &[("label", &label)])
        ),
        Style::default()
            .fg(p.red)
            .bg(p.panel_bg)
            .add_modifier(Modifier::BOLD),
    );
    put_text(
        b,
        stack.header.x,
        stack.header.y + 1,
        stack.header.width,
        &format!(" {}", t.remove_detail),
        Style::default().fg(p.text).bg(p.panel_bg),
    );
    let confirm_label = crate::i18n::texts().overlays.confirm_button;
    let cancel_label = crate::i18n::texts().overlays.cancel_button;
    let rects = modal_button_row(
        stack.actions.unwrap_or_default(),
        &[confirm_label, cancel_label],
        2,
    );
    let [confirm, cancel] = rects.as_slice() else {
        return None;
    };
    modal_button(
        b,
        *confirm,
        confirm_label,
        crate::ui::ModalButtonTone::Danger,
        cx.button_state(
            &super::feedback::ChromeHover::MachineButton(MachineOverlayButton::ConfirmRemove),
            crate::ui::ModalButtonState::Focused,
        ),
        p,
    );
    modal_button(
        b,
        *cancel,
        cancel_label,
        crate::ui::ModalButtonTone::Secondary,
        cx.button_state(
            &super::feedback::ChromeHover::MachineButton(MachineOverlayButton::CancelRemove),
            crate::ui::ModalButtonState::Normal,
        ),
        p,
    );
    Some(OverlayRender {
        area: popup,
        machines_popup: popup,
        machines_actions: vec![
            (*confirm, MachineOverlayButton::ConfirmRemove),
            (*cancel, MachineOverlayButton::CancelRemove),
        ],
        ..OverlayRender::default()
    })
}

const BOOTSTRAP_STEPS: [SavedSshBootstrapStep; 4] = [
    SavedSshBootstrapStep::DetectPlatform,
    SavedSshBootstrapStep::Install,
    SavedSshBootstrapStep::StartServer,
    SavedSshBootstrapStep::Verify,
];

pub(super) fn bootstrap_step_label(step: SavedSshBootstrapStep) -> &'static str {
    let t = &crate::i18n::texts().machines;
    match step {
        SavedSshBootstrapStep::DetectPlatform => t.progress_detect,
        SavedSshBootstrapStep::Install => t.progress_install,
        SavedSshBootstrapStep::StartServer => t.progress_start,
        SavedSshBootstrapStep::Verify => t.progress_verify,
    }
}

fn render_machine_form(
    b: &mut Buffer,
    form: &ClientMachineForm,
    cx: &super::feedback::ChromeContext<'_>,
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
    let mut max_scroll = 0;
    if let Some(bootstrap) = form.bootstrap.as_ref() {
        render_bootstrap_progress(b, body, form, bootstrap, base, cx);
    } else if form.step == MachineFormStep::Confirm && !editing {
        max_scroll = render_form_confirm(b, body, form, base, p);
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
                    Style::default().fg(p.text).bg(p.surface0)
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
                b.set_style(input, Style::default().fg(p.text).bg(p.surface0));
                let inner_input = Rect::new(input.x + 1, input.y, input.width.saturating_sub(1), 1);
                let field_cursor = text_editor::render(
                    b,
                    inner_input,
                    editor,
                    Style::default().fg(p.text).bg(p.surface0),
                );
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
                    &super::feedback::ChromeHover::MachineButton(button),
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
        machines_max_scroll: max_scroll,
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
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Paragraph, Widget, Wrap};
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

fn render_bootstrap_progress(
    b: &mut Buffer,
    area: Rect,
    form: &ClientMachineForm,
    bootstrap: &ClientMachineBootstrap,
    base: Style,
    cx: &super::feedback::ChromeContext<'_>,
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

// ---------- port forwards editor ----------

fn render_machine_forwards(
    b: &mut Buffer,
    view: &ClientForwardRulesView,
    saved_profiles: &[SavedSshEndpoint],
    port_forwards: &HashMap<ClientEndpointId, Vec<crate::remote::PortForwardStatus>>,
    cx: &super::feedback::ChromeContext<'_>,
) -> Option<OverlayRender> {
    let p = cx.palette;
    let t = &crate::i18n::texts().machines;
    let profile = saved_profiles
        .iter()
        .find(|profile| profile.id == view.profile_id)?;
    let (popup, inner) = modal_panel(b, crate::ui::ModalSize::Large.with_height(20), p.accent, cx)?;
    if inner.width < 24 || inner.height < 8 {
        return Some(OverlayRender {
            area: popup,
            machines_popup: popup,
            ..OverlayRender::default()
        });
    }
    let stack = crate::ui::modal_stack_areas(inner, 1, 1, 1, 1);
    let base = Style::default()
        .bg(p.panel_bg)
        .remove_modifier(Modifier::DIM);
    put_text(
        b,
        stack.header.x,
        stack.header.y,
        stack.header.width,
        &format!(
            " {}",
            crate::i18n::fill(t.forwards_title_fmt, &[("label", &profile.label)])
        ),
        base.fg(p.text).add_modifier(Modifier::BOLD),
    );

    let body = stack.content;
    let statuses = port_forwards.get(&ClientEndpointId::Ssh(view.profile_id.clone()));
    let mut row_hits = Vec::new();
    let mut y = body.y;
    if profile.port_forwards.is_empty() && !view.adding {
        put_text(
            b,
            body.x,
            y,
            body.width,
            t.forward_none,
            base.fg(p.overlay1),
        );
        put_text(
            b,
            body.x,
            y + 1,
            body.width,
            t.forward_none_hint,
            base.fg(p.overlay0),
        );
    }
    for (index, rule) in profile.port_forwards.iter().enumerate() {
        if y >= body.bottom() {
            break;
        }
        let rect = Rect::new(body.x, y, body.width, 1);
        row_hits.push((rect, index));
        let is_selected = index == view.selected && !view.adding;
        let style = if is_selected {
            Style::default()
                .fg(panel_contrast_fg(p))
                .bg(p.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(p.text).bg(p.panel_bg)
        };
        b.set_style(rect, style);
        let status = statuses.and_then(|statuses| statuses.iter().find(|s| s.rule == *rule));
        let (status_text, status_color) = match status {
            Some(status) => match status.phase {
                crate::remote::PortForwardPhase::Active => {
                    (t.forward_status_active.to_owned(), p.green)
                }
                crate::remote::PortForwardPhase::Failed => (
                    format!(
                        "{}: {}",
                        t.forward_status_failed,
                        status.detail.as_deref().unwrap_or_default()
                    ),
                    p.red,
                ),
            },
            None => (t.forward_waiting.to_owned(), p.overlay0),
        };
        put_text(
            b,
            rect.x,
            rect.y,
            rect.width,
            &format!(" {}", forward_rule_display(rule)),
            style,
        );
        let status_style = if is_selected {
            style
        } else {
            Style::default().fg(status_color).bg(p.panel_bg)
        };
        put_right_text(b, rect, rect.y, &status_text, status_style);
        y += 1;
    }

    // Add-rule form: kind choice plus the four text fields.
    let mut field_hits = Vec::new();
    let mut cursor = None;
    if view.adding {
        y += 1;
        if y < body.bottom() {
            put_text(
                b,
                body.x,
                y,
                body.width,
                &format!(" {}", t.forward_add_title),
                base.fg(p.text).add_modifier(Modifier::BOLD),
            );
            y += 1;
        }
        let fields: [(&str, Option<&TextEditor>, Option<&str>); FORWARD_FORM_FIELDS] = [
            (t.forward_field_kind, None, None),
            (
                t.forward_field_listen_port,
                Some(&view.form.listen_port),
                None,
            ),
            (
                t.forward_field_bind_address,
                Some(&view.form.bind_address),
                None,
            ),
            (
                t.forward_field_target_host,
                Some(&view.form.target_host),
                None,
            ),
            (
                t.forward_field_target_port,
                Some(&view.form.target_port),
                None,
            ),
        ];
        for (index, (label, editor, _)) in fields.iter().enumerate() {
            if y >= body.bottom() {
                break;
            }
            let rect = Rect::new(body.x, y, body.width, 1);
            field_hits.push((rect, index));
            let is_focused = view.form.focused == index;
            let label_width = 16u16.min(body.width);
            put_text(
                b,
                rect.x,
                rect.y,
                label_width,
                &format!(" {label:<14}"),
                base.fg(if is_focused { p.text } else { p.overlay0 }),
            );
            let input = Rect::new(
                rect.x + label_width,
                rect.y,
                rect.width.saturating_sub(label_width),
                1,
            );
            match editor {
                None => {
                    let style = if is_focused {
                        Style::default()
                            .fg(panel_contrast_fg(p))
                            .bg(p.accent)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(p.text).bg(p.surface0)
                    };
                    b.set_style(input, style);
                    put_text(
                        b,
                        input.x,
                        input.y,
                        input.width,
                        &format!("‹ {} ›", FORWARD_KINDS[view.form.kind].as_str()),
                        style,
                    );
                }
                Some(editor) => {
                    b.set_style(input, Style::default().fg(p.text).bg(p.surface0));
                    let inner_input =
                        Rect::new(input.x + 1, input.y, input.width.saturating_sub(1), 1);
                    let field_cursor = text_editor::render(
                        b,
                        inner_input,
                        editor,
                        Style::default().fg(p.text).bg(p.surface0),
                    );
                    if is_focused {
                        cursor = field_cursor;
                    }
                }
            }
            y += 1;
        }
        if let Some(error) = view.error.as_deref() {
            if y < body.bottom() {
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
    } else if let Some(message) = view.message.as_deref() {
        if y < body.bottom() {
            put_text(b, body.x, y, body.width, message, base.fg(p.green));
        }
    } else if let Some(error) = view.error.as_deref() {
        if y < body.bottom() {
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

    if let Some(footer) = stack.footer {
        let hints: Vec<(String, String)> = if view.adding {
            vec![
                ("tab/↑↓".to_owned(), t.hint_fields.to_owned()),
                ("←→".to_owned(), t.hint_change.to_owned()),
                ("enter".to_owned(), t.hint_confirm.to_owned()),
                ("esc".to_owned(), t.hint_back.to_owned()),
            ]
        } else {
            vec![
                ("↑↓".to_owned(), t.hint_select.to_owned()),
                ("a".to_owned(), t.hint_add.to_owned()),
                ("x".to_owned(), t.hint_remove.to_owned()),
                ("esc".to_owned(), t.hint_back.to_owned()),
            ]
        };
        render_key_hints(b, footer, &hints, p, cx.components);
    }

    let mut action_hits = Vec::new();
    let back_label = crate::i18n::texts().overlays.back_button;
    let (labels, buttons): (Vec<&str>, Vec<MachineOverlayButton>) = if view.adding {
        (
            vec![t.save_button, back_label],
            vec![
                MachineOverlayButton::ForwardSave,
                MachineOverlayButton::ForwardCancel,
            ],
        )
    } else {
        (
            vec![t.add_button, t.remove_button, back_label],
            vec![
                MachineOverlayButton::ForwardAddStart,
                MachineOverlayButton::ForwardRemove,
                MachineOverlayButton::Back,
            ],
        )
    };
    let rects = modal_button_row(stack.actions.unwrap_or_default(), &labels, 2);
    if rects.len() == labels.len() {
        for (index, rect) in rects.iter().enumerate() {
            let button = buttons[index];
            let enabled =
                button != MachineOverlayButton::ForwardRemove || !profile.port_forwards.is_empty();
            let (tone, base_state) = match button {
                MachineOverlayButton::ForwardAddStart | MachineOverlayButton::ForwardSave => (
                    crate::ui::ModalButtonTone::Primary,
                    crate::ui::ModalButtonState::Focused,
                ),
                MachineOverlayButton::ForwardRemove => (
                    crate::ui::ModalButtonTone::Danger,
                    crate::ui::ModalButtonState::Normal,
                ),
                _ => (
                    crate::ui::ModalButtonTone::Secondary,
                    crate::ui::ModalButtonState::Normal,
                ),
            };
            let state = if enabled {
                cx.button_state(
                    &super::feedback::ChromeHover::MachineButton(button),
                    base_state,
                )
            } else {
                crate::ui::ModalButtonState::Disabled
            };
            modal_button(b, *rect, labels[index], tone, state, p);
            if enabled {
                action_hits.push((*rect, button));
            }
        }
    }

    Some(OverlayRender {
        area: popup,
        machines_popup: popup,
        machines_wizard_rows: row_hits,
        machines_wizard_fields: field_hits,
        machines_actions: action_hits,
        cursor,
        ..OverlayRender::default()
    })
}

// ---------- SSH config import wizard ----------

/// user@target:port one-liner for a planned import (same shape as the CLI's
/// interactive listing).
fn import_planned_summary(planned: &crate::remote::PlannedImport) -> String {
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

fn render_machine_import(
    b: &mut Buffer,
    view: &ClientMachineImportView,
    cx: &super::feedback::ChromeContext<'_>,
) -> Option<OverlayRender> {
    let p = cx.palette;
    let t = &crate::i18n::texts().machines;
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
        &format!(" {}", t.import_title),
        base.fg(p.text).add_modifier(Modifier::BOLD),
    );
    // Step indicator + source path.
    let steps = [
        t.import_step_discover,
        t.import_step_select,
        t.import_step_done,
    ];
    let mut x = stack.header.x;
    for (index, label) in steps.iter().enumerate() {
        let active = matches!(
            (index, view.step),
            (0, ClientImportStep::Discover)
                | (1, ClientImportStep::Select)
                | (2, ClientImportStep::Done)
        );
        let text = format!(" {} {} ", index + 1, label);
        let style = if active {
            base.fg(p.accent).add_modifier(Modifier::BOLD)
        } else {
            base.fg(p.overlay0)
        };
        let width = display_width(&text);
        put_text(b, x, stack.header.y + 1, width, &text, style);
        x = x.saturating_add(width);
    }
    put_right_text(
        b,
        Rect::new(stack.header.x, stack.header.y + 1, stack.header.width, 1),
        stack.header.y + 1,
        &view.path.display().to_string(),
        base.fg(p.overlay0),
    );

    let body = stack.content;
    let mut cursor = None;
    let wizard_rows: Vec<(Rect, usize)> = match view.step {
        ClientImportStep::Discover => {
            render_import_discover(b, body, view, base, p);
            Vec::new()
        }
        ClientImportStep::Select => {
            let (rows, select_cursor) = render_import_select(b, body, view, base, p);
            cursor = select_cursor;
            rows
        }
        ClientImportStep::Done => {
            render_import_done(b, body, view, base, p);
            Vec::new()
        }
    };

    if let Some(footer) = stack.footer {
        let hints: Vec<(String, String)> = match view.step {
            ClientImportStep::Discover => vec![
                ("enter".to_owned(), t.hint_continue.to_owned()),
                ("esc".to_owned(), t.hint_back.to_owned()),
            ],
            ClientImportStep::Select => vec![
                ("↑↓".to_owned(), t.hint_select.to_owned()),
                ("space".to_owned(), t.hint_toggle.to_owned()),
                ("a".to_owned(), t.hint_all_none.to_owned()),
                ("enter".to_owned(), t.hint_import.to_owned()),
                ("esc".to_owned(), t.hint_back.to_owned()),
            ],
            ClientImportStep::Done => vec![("esc/enter".to_owned(), t.hint_back.to_owned())],
        };
        render_key_hints(b, footer, &hints, p, cx.components);
    }

    // Buttons per step.
    let close_label = crate::ui::modal_close_button_text();
    let back_label = crate::i18n::texts().overlays.back_button;
    let continue_enabled = view.fatal.is_none() && !view.plan.ready.is_empty();
    let (labels, buttons): (Vec<&str>, Vec<MachineOverlayButton>) = match view.step {
        ClientImportStep::Discover => (
            vec![t.next_button, close_label],
            vec![
                MachineOverlayButton::ImportContinue,
                MachineOverlayButton::Close,
            ],
        ),
        ClientImportStep::Select => (
            vec![t.import_run_button, back_label],
            vec![MachineOverlayButton::ImportRun, MachineOverlayButton::Back],
        ),
        ClientImportStep::Done => (vec![close_label], vec![MachineOverlayButton::Close]),
    };
    let rects = modal_button_row(stack.actions.unwrap_or_default(), &labels, 2);
    let mut action_hits = Vec::new();
    if rects.len() == labels.len() {
        for (index, rect) in rects.iter().enumerate() {
            let button = buttons[index];
            let enabled = button != MachineOverlayButton::ImportContinue || continue_enabled;
            let (tone, base_state) = match button {
                MachineOverlayButton::ImportContinue | MachineOverlayButton::ImportRun => (
                    crate::ui::ModalButtonTone::Primary,
                    crate::ui::ModalButtonState::Focused,
                ),
                _ => (
                    crate::ui::ModalButtonTone::Secondary,
                    crate::ui::ModalButtonState::Normal,
                ),
            };
            let state = if enabled {
                cx.button_state(
                    &super::feedback::ChromeHover::MachineButton(button),
                    base_state,
                )
            } else {
                crate::ui::ModalButtonState::Disabled
            };
            modal_button(b, *rect, labels[index], tone, state, p);
            if enabled {
                action_hits.push((*rect, button));
            }
        }
    }
    Some(OverlayRender {
        area: popup,
        machines_popup: popup,
        machines_wizard_rows: wizard_rows,
        machines_actions: action_hits,
        cursor,
        ..OverlayRender::default()
    })
}

fn render_import_discover(
    b: &mut Buffer,
    body: Rect,
    view: &ClientMachineImportView,
    base: Style,
    p: &Palette,
) {
    let t = &crate::i18n::texts().machines;
    let mut y = body.y;
    if let Some(fatal) = view.fatal.as_deref() {
        put_text(b, body.x, y, body.width, fatal, base.fg(p.overlay1));
        return;
    }
    if view.warnings > 0 {
        put_text(
            b,
            body.x,
            y,
            body.width,
            &format!(
                " {}",
                crate::i18n::fill(
                    t.import_warnings_fmt,
                    &[("count", &view.warnings.to_string())]
                )
            ),
            base.fg(p.yellow),
        );
        y += 1;
    }
    if !view.plan.ready.is_empty() {
        put_text(
            b,
            body.x,
            y,
            body.width,
            t.import_ready_header,
            base.fg(p.overlay0),
        );
        y += 1;
        for planned in &view.plan.ready {
            if y >= body.bottom() {
                break;
            }
            let mut text = format!(" {} → {}", planned.label, import_planned_summary(planned));
            if !planned.notes.is_empty() {
                text.push_str(&format!(
                    " ({})",
                    crate::i18n::fill(
                        t.import_notes_fmt,
                        &[("count", &planned.notes.len().to_string())]
                    )
                ));
            }
            put_text(b, body.x, y, body.width, &text, base.fg(p.text));
            y += 1;
        }
    }
    if !view.plan.skipped.is_empty() {
        put_text(
            b,
            body.x,
            y,
            body.width,
            t.import_skip_header,
            base.fg(p.overlay0),
        );
        y += 1;
        for skip in &view.plan.skipped {
            if y >= body.bottom() {
                break;
            }
            put_text(
                b,
                body.x,
                y,
                body.width,
                &format!(" {} — {}", skip.label, skip.reason),
                base.fg(p.overlay1),
            );
            y += 1;
        }
    }
}

/// Renders the select step; returns the clickable row rects (candidate
/// toggles, wildcard toggle, group input) plus the text cursor while the
/// group input is focused.
fn render_import_select(
    b: &mut Buffer,
    body: Rect,
    view: &ClientMachineImportView,
    base: Style,
    p: &Palette,
) -> (Vec<(Rect, usize)>, Option<crate::protocol::CursorState>) {
    let t = &crate::i18n::texts().machines;
    let mut hits = Vec::new();
    let candidates = view.plan.ready.len();
    let mut y = body.y;
    for (index, planned) in view.plan.ready.iter().enumerate() {
        if y >= body.bottom() {
            break;
        }
        let rect = Rect::new(body.x, y, body.width, 1);
        hits.push((rect, index));
        let focused = view.focus_row == index;
        let checked = view.selected.get(index).copied().unwrap_or(false);
        let style = if focused {
            Style::default()
                .fg(panel_contrast_fg(p))
                .bg(p.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(p.text).bg(p.panel_bg)
        };
        b.set_style(rect, style);
        put_text(
            b,
            rect.x,
            rect.y,
            rect.width,
            &format!(
                " {} {} → {}",
                if checked { "[x]" } else { "[ ]" },
                planned.label,
                import_planned_summary(planned)
            ),
            style,
        );
        y += 1;
    }
    // Wildcard toggle row.
    if y < body.bottom() {
        let rect = Rect::new(body.x, y, body.width, 1);
        hits.push((rect, candidates));
        let focused = view.focus_row == candidates;
        let style = if focused {
            Style::default()
                .fg(panel_contrast_fg(p))
                .bg(p.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(p.text).bg(p.panel_bg)
        };
        b.set_style(rect, style);
        put_text(
            b,
            rect.x,
            rect.y,
            rect.width,
            &format!(
                " {} {}",
                if view.include_wildcards { "[x]" } else { "[ ]" },
                t.import_include_wildcards
            ),
            style,
        );
        y += 1;
    }
    // Group input row.
    let mut cursor = None;
    if y < body.bottom() {
        let rect = Rect::new(body.x, y, body.width, 1);
        hits.push((rect, candidates + 1));
        let focused = view.focus_row == candidates + 1;
        let label_width = 10u16.min(body.width);
        put_text(
            b,
            rect.x,
            rect.y,
            label_width,
            &format!(" {:<8}", t.import_group_label),
            base.fg(if focused { p.text } else { p.overlay0 }),
        );
        let input = Rect::new(
            rect.x + label_width,
            rect.y,
            rect.width.saturating_sub(label_width),
            1,
        );
        b.set_style(input, Style::default().fg(p.text).bg(p.surface0));
        let inner_input = Rect::new(input.x + 1, input.y, input.width.saturating_sub(1), 1);
        let field_cursor = text_editor::render(
            b,
            inner_input,
            &view.group,
            Style::default().fg(p.text).bg(p.surface0),
        );
        if focused {
            cursor = field_cursor;
        }
        y += 1;
    }
    // Selection counter.
    if y < body.bottom() {
        let selected = view.selected.iter().filter(|selected| **selected).count();
        put_text(
            b,
            body.x,
            y,
            body.width,
            &format!(
                " {}",
                crate::i18n::fill(
                    t.import_selected_fmt,
                    &[
                        ("selected", &selected.to_string()),
                        ("total", &candidates.to_string())
                    ]
                )
            ),
            base.fg(p.overlay0),
        );
    }
    (hits, cursor)
}

fn render_import_done(
    b: &mut Buffer,
    body: Rect,
    view: &ClientMachineImportView,
    base: Style,
    p: &Palette,
) {
    let t = &crate::i18n::texts().machines;
    let mut y = body.y;
    let (imported, skipped, failed) = view.summary;
    put_text(
        b,
        body.x,
        y,
        body.width,
        &format!(
            " {}",
            crate::i18n::fill(
                t.import_summary_fmt,
                &[
                    ("imported", &imported.to_string()),
                    ("skipped", &skipped.to_string()),
                    ("failed", &failed.to_string()),
                ]
            )
        ),
        base.fg(p.text).add_modifier(Modifier::BOLD),
    );
    y += 1;
    for row in &view.results {
        if y >= body.bottom() {
            break;
        }
        let (tag, color) = match row.outcome {
            ClientImportOutcome::Imported => (t.import_result_imported, p.green),
            ClientImportOutcome::Skipped => (t.import_result_skipped, p.overlay0),
            ClientImportOutcome::Failed => (t.import_result_failed, p.red),
        };
        let text = if row.detail.is_empty() {
            format!(" {tag} {}", row.label)
        } else {
            format!(" {tag} {} — {}", row.label, row.detail)
        };
        put_text(b, body.x, y, body.width, &text, base.fg(color));
        y += 1;
        if row.outcome == ClientImportOutcome::Failed && y < body.bottom() {
            put_text(
                b,
                body.x,
                y,
                body.width,
                &format!("   {}", t.import_failed_hint),
                base.fg(p.yellow),
            );
            y += 1;
        }
    }
    if imported > 0 && y < body.bottom() {
        put_text(
            b,
            body.x,
            y,
            body.width,
            t.import_connect_note,
            base.fg(p.overlay1),
        );
    }
}
