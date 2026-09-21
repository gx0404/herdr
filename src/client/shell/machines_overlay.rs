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
    /// 本帧视图的滚动上界：由视图计算阶段（`compute_machines_view`）写入，
    /// 键盘 / 滚轮的边界判定读它（STATE-04；此前读上一帧的渲染命中区）。
    pub(super) view_max_scroll: usize,
    /// 指针悬浮的机器：只由 `Moved` 改写，`selected` 只由键盘与点击改写。
    /// 列表视图的 `d`（立即启停，会断掉在线 SSH）、`x`（删除）、`r`
    /// （重连）、`Shift+R`（改名）都取 `selected_machine_id()`——指针只是
    /// 划过列表就把这些破坏性键重新指向「鼠标最后路过的机器」是不可接受的
    /// （MENU-01 / UX-04）。存身份而不是行号，筛选与重排后天然失效。
    pub(super) hovered: Option<ProfileId>,
    /// 机器面板的一次性反馈（复制修复命令、向导成功）。带时间戳，由
    /// `tick_chrome_feedback` 到期清除；渲染落点见 `render_machines_toast`。
    pub(super) message: Option<MachineToast>,
}

/// 机器面板公共 toast：一行反馈 + 写入时刻。
#[derive(Debug, Clone)]
pub(super) struct MachineToast {
    pub(super) text: String,
    pub(super) at: std::time::Instant,
}

/// toast 在屏时长；到期由 feedback tick 清除并请求一次重绘。
pub(super) const MACHINE_TOAST_DURATION: std::time::Duration = std::time::Duration::from_secs(4);

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
            view_max_scroll: 0,
            hovered: None,
            message: None,
        }
    }

    /// 写入一条公共 toast（覆盖上一条，重新计时）。
    ///
    /// 时间戳在这里取：按键路由与向导 bootstrap 轮询这两条调用链都没有本帧
    /// 的 now 可传（`record_terminal_bell` 的生产调用点同样是就地
    /// `Instant::now()`），注入只会把同一行代码挪到调用方。`at` 是公开字段，
    /// 测试可以据此在 `MACHINE_TOAST_DURATION` 边界上精确断言。
    pub(super) fn set_message(&mut self, text: String) {
        self.message = Some(MachineToast {
            text,
            at: std::time::Instant::now(),
        });
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
    /// 进入这个编辑器时停在哪个视图：宽屏 dashboard / 窄屏 List 直接按 `f`
    /// 进来的，Esc 必须回列表而不是掉进更窄的 Detail 模态（那正是 C-25 要
    /// 消除的尺寸跳变）。
    pub(super) from_list: bool,
    pub(super) selected: usize,
    /// 规则列表的滚动窗口起点；由渲染经 `OverlayRender::machines_scroll` 回写，
    /// 与机器列表同一套「compose 期回写 scroll」模式。
    pub(super) scroll: usize,
    /// 一次性「把选中项滚进窗口」请求：键盘移动置位，compose 后清零。
    pub(super) reveal: bool,
    pub(super) adding: bool,
    pub(super) form: ClientForwardRuleForm,
    /// 待确认删除的目标：`x` 只进入确认态，Enter 才真正删除。
    pub(super) pending_remove: Option<PendingForwardRemoval>,
    pub(super) error: Option<String>,
    pub(super) message: Option<String>,
}

/// 武装中的转发删除目标。只记下标不够：目录 watcher（`set_endpoint_catalog`）
/// 会在浮层打开期间重新镜像 `saved_profiles`，另一个客户端或
/// `herdr machine forward remove` 都能在武装与确认之间改写规则表。确认落盘
/// 前比对整条规则，不一致就取消确认而不是照下标删（HERDR-MACH-005/025）。
#[derive(Debug, Clone)]
pub(super) struct PendingForwardRemoval {
    pub(super) index: usize,
    pub(super) rule: PortForwardRule,
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
    pub(super) fn blank() -> Self {
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
    /// 候选列表（select 步骤）与结果列表（done 步骤）共用的滚动窗口起点，
    /// 由渲染经 `OverlayRender::machines_scroll` 回写；换步骤时归零。
    pub(super) scroll: usize,
    /// 一次性「把聚焦行滚进窗口」请求：焦点移动置位，compose 后清零。
    pub(super) reveal: bool,
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
            scroll: 0,
            reveal: true,
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
    if query.is_empty() {
        return true;
    }
    // 逐词匹配：每个词单独出现即可，不必相邻；标签（tags）也是搜索面。
    let mut haystack = format!("{} {}", profile.label, profile.target);
    if let Some(group) = &profile.group {
        haystack.push(' ');
        haystack.push_str(group);
    }
    for tag in &profile.tags {
        haystack.push(' ');
        haystack.push_str(tag);
    }
    let haystack = haystack.to_lowercase();
    query.split_whitespace().all(|word| haystack.contains(word))
}

pub(super) fn machine_list_rows(
    saved_profiles: &[SavedSshEndpoint],
    endpoints: &[ClientShellEndpoint],
    query: &str,
) -> Vec<MachineListRow> {
    // 端点索引建一次：原来每个 profile 都要线性扫一遍 endpoints，并为比较
    // 克隆一份 `ClientEndpointId`（HERDR-MACH-017）。
    let mut by_profile: HashMap<&ProfileId, &ClientShellEndpoint> =
        HashMap::with_capacity(endpoints.len());
    for endpoint in endpoints {
        if let ClientEndpointId::Ssh(profile_id) = &endpoint.endpoint_id {
            by_profile.entry(profile_id).or_insert(endpoint);
        }
    }
    saved_profiles
        .iter()
        .filter(|profile| machine_matches(profile, query))
        .map(|profile| {
            let endpoint = by_profile.get(&profile.id).copied();
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
    ForwardCancelRemove,
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

    /// 复制该机器的修复 / 启动命令，并给一条反馈。机器面板开着时落到公共
    /// toast；侧栏右键菜单触发时面板根本没开（C-27 点名的第三个触发点），
    /// 走通用 Success 通知通道，不至于零反馈。
    pub(super) fn machine_copy_fix_command(
        &mut self,
        profile_id: &ProfileId,
        outcome: &mut ClientShellInput,
    ) {
        let Some(profile) = self.saved_profile(profile_id) else {
            return;
        };
        let label = profile.label.clone();
        let command = crate::remote::saved_ssh_bootstrap_command(&profile.target, &profile.session);
        outcome
            .actions
            .push(ClientShellAction::ClipboardWrite(command.into_bytes()));
        let text = crate::i18n::texts().machines.copied_fix_command;
        // 取法与向导成功路径、feedback tick 一致：命令面板盖在机器页之上时
        // 仍写进机器页自己的 toast。
        if let Some(ClientShellOverlay::Machines(overlay)) = self.content_page_mut() {
            overlay.set_message(text.to_owned());
            return;
        }
        self.push_endpoint_notice(
            super::state::ClientEndpointNoticeKind::Success,
            "machine:copy_fix_command",
            text,
            label,
        );
        outcome.repaint = true;
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
                    if view.from_list {
                        Back::List
                    } else {
                        Back::Detail(view.profile_id.clone())
                    }
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
            let from_list = matches!(overlay.view, ClientMachinesView::List);
            overlay.view = ClientMachinesView::Forwards(Box::new(ClientForwardRulesView {
                profile_id: profile_id.clone(),
                from_list,
                selected: 0,
                scroll: 0,
                reveal: true,
                adding: false,
                form: ClientForwardRuleForm::blank(),
                pending_remove: None,
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
        // local/remote 转发必须有目标：这里先拦，避免存盘时才被 catalog 校验
        // 挡回来（那条错误是英文且面向 CLI）。
        if matches!(kind, PortForwardKind::Local | PortForwardKind::Remote) {
            let ui = &crate::i18n::texts().machines;
            if target_host.is_none() {
                return Err(ui.forward_target_host_required.to_owned());
            }
            if target_port.is_none_or(|port| port == 0) {
                return Err(ui.forward_target_port_required.to_owned());
            }
        }
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

    /// 转发编辑器是否正处于删除确认态。
    fn forward_remove_pending(&self) -> bool {
        matches!(
            self.overlay.as_ref(),
            Some(ClientShellOverlay::Machines(overlay))
                if matches!(&overlay.view, ClientMachinesView::Forwards(view) if view.pending_remove.is_some())
        )
    }

    /// `x` / 移除按钮的第一步：记下待删规则的下标与规则值，画面点名后等
    /// Enter 确认（HERDR-MACH-025）。
    fn forward_arm_remove(&mut self) {
        let armed = match self.overlay.as_ref() {
            Some(ClientShellOverlay::Machines(overlay)) => match &overlay.view {
                ClientMachinesView::Forwards(view) => {
                    let Some(profile) = self.saved_profile(&view.profile_id) else {
                        return;
                    };
                    let rules = profile.port_forwards.as_slice();
                    if rules.is_empty() {
                        return;
                    }
                    let index = view.selected.min(rules.len() - 1);
                    let Some(rule) = rules.get(index) else {
                        return;
                    };
                    PendingForwardRemoval {
                        index,
                        rule: rule.clone(),
                    }
                }
                _ => return,
            },
            _ => return,
        };
        if let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() {
            if let ClientMachinesView::Forwards(view) = &mut overlay.view {
                view.selected = armed.index;
                view.pending_remove = Some(armed);
                view.message = None;
                view.error = None;
                view.reveal = true;
            }
        }
    }

    fn forward_cancel_remove(&mut self) {
        if let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() {
            if let ClientMachinesView::Forwards(view) = &mut overlay.view {
                if view.pending_remove.take().is_some() {
                    view.message = Some(
                        crate::i18n::texts()
                            .machines
                            .forward_remove_cancelled
                            .to_owned(),
                    );
                }
            }
        }
    }

    /// 确认后真正落盘删除。只按武装时记下的规则删：没武装就什么都不做，
    /// 规则表在武装与确认之间被外部改写（目录 watcher / CLI / 另一个客户端）
    /// 就取消确认并提示重选，绝不按下标兜底删掉另一条。
    fn forward_remove_confirmed(&mut self) {
        let (profile_id, armed) = {
            let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_ref() else {
                return;
            };
            let ClientMachinesView::Forwards(view) = &overlay.view else {
                return;
            };
            let Some(armed) = view.pending_remove.clone() else {
                return;
            };
            (view.profile_id.clone(), armed)
        };
        let Some(profile) = self.saved_profile(&profile_id).cloned() else {
            return;
        };
        if profile.port_forwards.get(armed.index) != Some(&armed.rule) {
            self.forward_remove_stale();
            return;
        }
        let mut rules = profile.port_forwards.clone();
        rules.remove(armed.index);
        let remaining = rules.len();
        let outcome = self.store_forward_rules(&profile_id, rules);
        if let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() {
            if let ClientMachinesView::Forwards(view) = &mut overlay.view {
                view.pending_remove = None;
                // 落盘失败必须走红色 error 分支，不能以绿色「成功」样式显示。
                match outcome {
                    Ok(()) => {
                        view.message =
                            Some(crate::i18n::texts().machines.forward_removed.to_owned());
                        view.error = None;
                        // 原位补位；删掉末条时回退一行。
                        view.selected = armed.index.min(remaining.saturating_sub(1));
                    }
                    Err(error) => {
                        view.error = Some(error);
                        view.message = None;
                    }
                }
                view.reveal = true;
            }
        }
    }

    /// 武装期间规则表被外部改写：清掉确认态并提示重选。
    fn forward_remove_stale(&mut self) {
        if let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() {
            if let ClientMachinesView::Forwards(view) = &mut overlay.view {
                view.pending_remove = None;
                view.error = Some(
                    crate::i18n::texts()
                        .machines
                        .forward_remove_stale
                        .to_owned(),
                );
                view.message = None;
                view.reveal = true;
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
                        // 失败只走红色 error 分支，别把上一次的绿色成功文案留在画面上。
                        view.error = Some(error);
                        view.message = None;
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
                        scroll: 0,
                        reveal: true,
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
        view.scroll = 0;
        view.reveal = true;
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
        view.scroll = 0;
        view.reveal = false;
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
                view.reveal = true;
            }
        }
    }

    /// done 步骤的结果列表滚动：上界由渲染回填（`machines_max_scroll`）。
    fn scroll_import_results(&mut self, delta: isize) {
        let max_scroll = self.machines_view_max_scroll();
        if let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() {
            if let ClientMachinesView::Import(view) = &mut overlay.view {
                view.scroll = view.scroll.saturating_add_signed(delta).min(max_scroll);
                view.reveal = false;
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
                view.pending_remove = None;
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
        // 删除确认态独占 Enter/Esc：Esc 只取消确认，不退出编辑器。
        let pending = self.forward_remove_pending();
        match code {
            KeyCode::Esc if pending => self.forward_cancel_remove(),
            KeyCode::Esc => self.machines_back(),
            KeyCode::Enter if pending => self.forward_remove_confirmed(),
            // 确认键必须与触发键不同（与机器删除的 `ConfirmRemove` 对齐）：
            // 否则长按 `x` 会 arm → confirm → arm → … 把规则删光。
            KeyCode::Char('x') if plain && pending => self.forward_cancel_remove(),
            KeyCode::Up | KeyCode::Char('k') if plain => {
                if let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() {
                    if let ClientMachinesView::Forwards(view) = &mut overlay.view {
                        view.selected = view.selected.saturating_sub(1);
                        view.pending_remove = None;
                        view.reveal = true;
                    }
                }
            }
            KeyCode::Down | KeyCode::Char('j') if plain => {
                let count = self.forward_rule_count();
                if let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() {
                    if let ClientMachinesView::Forwards(view) = &mut overlay.view {
                        view.selected = (view.selected + 1).min(count.saturating_sub(1));
                        view.pending_remove = None;
                        view.reveal = true;
                    }
                }
            }
            KeyCode::Char('a') if plain => {
                if let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() {
                    if let ClientMachinesView::Forwards(view) = &mut overlay.view {
                        view.adding = true;
                        view.pending_remove = None;
                        view.message = None;
                        view.error = None;
                    }
                }
            }
            KeyCode::Char('x') if plain => self.forward_arm_remove(),
            _ => {}
        }
        outcome.repaint = true;
    }

    /// 当前转发编辑器对应机器的规则条数。
    fn forward_rule_count(&self) -> usize {
        match self.overlay.as_ref() {
            Some(ClientShellOverlay::Machines(overlay)) => match &overlay.view {
                ClientMachinesView::Forwards(view) => self
                    .saved_profile(&view.profile_id)
                    .map(|profile| profile.port_forwards.len())
                    .unwrap_or(0),
                _ => 0,
            },
            _ => 0,
        }
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
                                    view.scroll = 0;
                                    view.reveal = true;
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
                    // 候选可能有几十条：补翻页与首尾（HERDR-MACH-026）。步长交给
                    // `move_import_focus` 自己钳位。
                    KeyCode::PageUp if plain => self.move_import_focus(-10),
                    KeyCode::PageDown if plain => self.move_import_focus(10),
                    KeyCode::Home | KeyCode::Char('g') if plain => self.move_import_focus(-1000),
                    KeyCode::End | KeyCode::Char('G') if plain => self.move_import_focus(1000),
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
            ClientImportStep::Done => match code {
                KeyCode::Esc | KeyCode::Enter => {
                    self.machines_back();
                    outcome.repaint = true;
                }
                KeyCode::Up | KeyCode::Char('k') if plain => {
                    self.scroll_import_results(-1);
                    outcome.repaint = true;
                }
                KeyCode::Down | KeyCode::Char('j') if plain => {
                    self.scroll_import_results(1);
                    outcome.repaint = true;
                }
                _ => {}
            },
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
                    KeyCode::Char('b') if plain => self.open_broadcast_overlay(),
                    // 其余动作键与 List 共用一张表（HERDR-MACH-007）。
                    _ => {
                        if !self.handle_machine_action_key(&id, code, modifiers, outcome) {
                            return true;
                        }
                    }
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
            KeyCode::Char('b') if plain => {
                self.open_broadcast_overlay();
                outcome.repaint = true;
            }
            // 宽屏 dashboard 接管 List 后网格里有转发 / 浏览文件 / 查看问题 /
            // 复制修复命令，键盘必须有同样的入口（HERDR-MACH-007）。目标一律
            // 取键盘选中的那台机器，指针悬浮不参与。
            _ => {
                if let Some(id) = self.selected_machine_id() {
                    if self.handle_machine_action_key(&id, code, modifiers, outcome) {
                        outcome.repaint = true;
                    }
                }
            }
        }
        true
    }

    /// List 与 Detail 共用的单机动作表：`e/R/r/v/d/x/c/f/o`。返回 true 表示
    /// 这次按键被消费（调用方负责置 `repaint`）。
    fn handle_machine_action_key(
        &mut self,
        profile_id: &ProfileId,
        code: KeyCode,
        modifiers: KeyModifiers,
        outcome: &mut ClientShellInput,
    ) -> bool {
        let plain = modifiers.is_empty();
        match code {
            KeyCode::Char('e') if plain => self.open_machine_edit_form(profile_id),
            KeyCode::Char('R') if modifiers == KeyModifiers::SHIFT => {
                self.open_machine_rename_overlay(profile_id)
            }
            KeyCode::Char('r') if plain => self.machine_reconnect(profile_id, outcome),
            KeyCode::Char('v') if plain => {
                // 没有可复核的失败时是空操作（照样算消费，避免落到别的键）。
                self.open_machine_auth_for_endpoint(profile_id, outcome);
            }
            KeyCode::Char('d') if plain => self.machine_toggle_enabled(profile_id),
            KeyCode::Char('x') if plain => {
                if let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() {
                    overlay.view = ClientMachinesView::ConfirmRemove(profile_id.clone());
                }
            }
            KeyCode::Char('c') if plain => self.machine_copy_fix_command(profile_id, outcome),
            KeyCode::Char('f') if plain => self.open_machine_forwards(profile_id),
            KeyCode::Char('o') if plain => self.open_machine_files(profile_id, outcome),
            _ => return false,
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
                            view.scroll = 0;
                            view.reveal = true;
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
                        view.pending_remove = None;
                        view.message = None;
                        view.error = None;
                    }
                }
            }
            Btn::ForwardRemove => {
                if self.forward_remove_pending() {
                    self.forward_remove_confirmed();
                } else {
                    self.forward_arm_remove();
                }
            }
            Btn::ForwardCancelRemove => self.forward_cancel_remove(),
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
            let max = overlay.view_max_scroll;
            overlay.detail_scroll = overlay.detail_scroll.saturating_add_signed(delta).min(max);
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
                let max_scroll = self.machines_view_max_scroll();
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
                let max = self.machines_view_max_scroll();
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
                let count = self.forward_rule_count();
                if let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() {
                    if let ClientMachinesView::Forwards(view) = &mut overlay.view {
                        view.selected = (view.selected as isize + delta)
                            .clamp(0, count.saturating_sub(1) as isize)
                            as usize;
                        view.pending_remove = None;
                        view.reveal = true;
                    }
                }
            }
            ScrollTarget::Import => {
                let step = match self.overlay.as_ref() {
                    Some(ClientShellOverlay::Machines(overlay)) => match &overlay.view {
                        ClientMachinesView::Import(view) => Some(view.step),
                        _ => None,
                    },
                    _ => None,
                };
                match step {
                    // select 步骤滚轮走焦点：reveal 会把窗口带过去，焦点不脱屏。
                    Some(ClientImportStep::Select) => self.move_import_focus(delta),
                    Some(ClientImportStep::Done) => self.scroll_import_results(delta),
                    // discover 步骤不消费 `view.scroll`，滚轮在这里没有目标。
                    Some(ClientImportStep::Discover) | None => {}
                }
            }
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

/// List 与宽屏 dashboard 共用的页脚键表：与 `handle_machine_action_key` 的
/// 动作表、dashboard 动作网格同序同集（HERDR-MACH-007）。
///
/// 顺序就是「丢弃优先级」：`render_key_hints` 填满 `area` 的行之后整项丢弃
/// 并画 `…`，所以页面级键（`/` 过滤、`esc` 关闭、`b` 广播——这三个在 List /
/// dashboard 上没有等效按钮或搜索框以外的入口）排在所有单机动作键之前，
/// 而末尾的 `a` 添加 / `i` 导入在两个视图里都有对应按钮兜底。`c` 的出现
/// 条件与按键一致（有选中即可用），不再按 Attention 门禁——侧栏右键菜单的
/// 「复制修复命令」同样不分状态。
fn machine_list_hints(has_selection: bool, has_review: bool) -> Vec<(String, String)> {
    let t = &crate::i18n::texts().machines;
    let mut hints = vec![
        ("↑↓".to_owned(), t.hint_select.to_owned()),
        ("enter".to_owned(), t.hint_details.to_owned()),
        ("/".to_owned(), t.hint_filter.to_owned()),
        ("esc".to_owned(), t.hint_close.to_owned()),
        ("b".to_owned(), t.hint_broadcast.to_owned()),
    ];
    if has_selection {
        hints.extend(machine_action_hints(has_review));
    }
    hints.extend([
        ("a".to_owned(), t.hint_add.to_owned()),
        ("i".to_owned(), t.hint_import.to_owned()),
    ]);
    hints
}

/// Detail 页脚：与 List / dashboard 同一张单机动作表，只是把页面级键换成
/// `esc 返回` 与 `b 广播`（Detail 没有列表导航与添加 / 导入）。
/// 详情卡底部的动作标签：渲染与视图计算阶段共用——`action_row_count` 用它
/// 决定给按钮留几行，两边必须同源（STATE-04）。
fn machine_detail_labels(
    profile: &SavedSshEndpoint,
    endpoints: &[ClientShellEndpoint],
    connection_errors: &HashMap<ClientEndpointId, crate::remote::ConnectionErrorKind>,
) -> Vec<&'static str> {
    let t = &crate::i18n::texts().machines;
    let status = endpoint_for(endpoints, &profile.id).map_or(
        if profile.enabled {
            ClientEndpointStatus::Connecting
        } else {
            ClientEndpointStatus::Disabled
        },
        |endpoint| endpoint.status,
    );
    let has_review = connection_errors
        .get(&ClientEndpointId::Ssh(profile.id.clone()))
        .is_some_and(super::machine_auth_overlay::failure_kind_has_review);
    let mut labels: Vec<&'static str> = vec![t.reconnect_button, t.edit_button];
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
    labels
}

fn machine_detail_hints(has_review: bool) -> Vec<(String, String)> {
    let t = &crate::i18n::texts().machines;
    let mut hints = vec![
        ("esc".to_owned(), t.hint_back.to_owned()),
        ("b".to_owned(), t.hint_broadcast.to_owned()),
    ];
    hints.extend(machine_action_hints(has_review));
    hints
}

/// 单机动作键：三个视图（List / 宽屏 dashboard / Detail）同序同集，出现
/// 条件也只在这一处判定（HERDR-MACH-007）。`v` 只在失败类型真有界面内恢复
/// 路径时出现（`open_machine_auth_for_endpoint` 否则是空操作）；`c` 与按键
/// 一样不分状态，侧栏右键菜单的「复制修复命令」同样不分状态。
fn machine_action_hints(has_review: bool) -> Vec<(String, String)> {
    let t = &crate::i18n::texts().machines;
    let mut hints = Vec::with_capacity(9);
    if has_review {
        hints.push(("v".to_owned(), t.hint_review.to_owned()));
    }
    hints.extend([
        ("r".to_owned(), t.hint_reconnect.to_owned()),
        ("e".to_owned(), t.hint_edit.to_owned()),
        ("f".to_owned(), t.hint_forwards.to_owned()),
        ("o".to_owned(), t.hint_browse_files.to_owned()),
        ("d".to_owned(), t.hint_toggle_enabled.to_owned()),
        ("x".to_owned(), t.hint_remove.to_owned()),
        ("R".to_owned(), t.hint_rename.to_owned()),
        ("c".to_owned(), t.hint_copy_fix.to_owned()),
    ]);
    hints
}

/// 机器页页脚（List / 宽屏 dashboard / Detail）要的行数：完整键表在单行里
/// 必然被尾部截断，所以按实际宽度取，最多两行——再多会把列表可见行数吃掉。
/// 两行仍放不下时按上面的顺序从尾部丢（`R` / `c` / `a` / `i`，这几个都还有
/// 按钮或侧栏右键菜单兜底）。
fn machine_footer_rows(hints: &[(String, String)], width: u16) -> u16 {
    super::render::key_hints_rows(hints, width).clamp(1, 2)
}

/// 机器面板公共 toast：视图各自把页脚行报成 `OverlayRender::machines_toast`，
/// 这里在顶层统一画一次，所以 List / Detail / dashboard 反馈落点一致
/// （HERDR-MACH-006）。
fn render_machines_toast(b: &mut Buffer, rect: Rect, message: &str, p: &Palette) {
    use ratatui::widgets::Widget as _;
    if rect.is_empty() {
        return;
    }
    let row = Rect::new(rect.x, rect.bottom().saturating_sub(1), rect.width, 1);
    ratatui::widgets::Clear.render(row, b);
    b.set_style(row, Style::default().bg(p.panel_bg));
    let icon_width = display_width("● ").min(row.width);
    put_text(
        b,
        row.x,
        row.y,
        icon_width,
        "● ",
        Style::default().fg(p.green).bg(p.panel_bg),
    );
    put_text(
        b,
        row.x.saturating_add(icon_width),
        row.y,
        row.width.saturating_sub(icon_width),
        message,
        Style::default()
            .fg(p.green)
            .bg(p.panel_bg)
            .add_modifier(Modifier::BOLD),
    );
}

// 机器面板渲染入口：视图分派 + 公共 toast。参数与既有机器页渲染入口一致，
// 集中在一次投影中避免从全局重新查状态；打包成 struct 反而要多一层生命周期
// 标注，故豁免 too_many_arguments。
#[allow(clippy::too_many_arguments)]
/// 机器面板各视图的正文几何：视图计算阶段与渲染阶段共用（STATE-04）。
/// 返回值 `None` 表示该视图本帧不画列表（窗口太小 / discover 步骤）。
pub(super) enum MachinesBody {
    /// 窄屏列表：正文 + 行高 2。
    List(Rect),
    /// 详情卡。
    Detail(Rect),
    /// 转发规则编辑器：正文 + 可见行数。
    Forwards(Rect),
    /// 导入向导的候选 / 目标选择列表：正文 + 可见行数。
    ImportSelect(Rect, usize),
    /// 导入向导的结果列表。
    ImportDone(Rect),
    /// 新建 / 编辑表单：正文（上界按步骤算）。
    Form(Rect),
}

pub(super) fn machines_body(
    area: Rect,
    page_bounds: Option<Rect>,
    overlay: &ClientMachinesOverlay,
    endpoints: &[ClientShellEndpoint],
    saved_profiles: &[SavedSshEndpoint],
    connection_errors: &HashMap<ClientEndpointId, crate::remote::ConnectionErrorKind>,
) -> Option<MachinesBody> {
    match &overlay.view {
        ClientMachinesView::List => {
            if page_bounds.unwrap_or(area).width >= 96 {
                let layout = dashboard::dashboard_layout(
                    area,
                    page_bounds,
                    overlay,
                    saved_profiles,
                    endpoints,
                    connection_errors,
                )?;
                let left_width = (layout.content.width / 3).clamp(24, 36);
                return Some(MachinesBody::List(Rect::new(
                    layout.content.x,
                    layout.content.y,
                    left_width,
                    layout.content.height,
                )));
            }
            let (_, inner) = machines_panel(area, page_bounds, 24)?;
            let rows = machine_list_rows(saved_profiles, endpoints, overlay.query.as_str());
            let selected = if rows.is_empty() {
                0
            } else {
                overlay.selected.min(rows.len() - 1)
            };
            let has_review = rows.get(selected).is_some_and(|row| {
                connection_errors
                    .get(&ClientEndpointId::Ssh(row.id.clone()))
                    .is_some_and(super::machine_auth_overlay::failure_kind_has_review)
            });
            let hints = machine_list_hints(!rows.is_empty(), has_review);
            let stack = crate::ui::modal_stack_areas(
                inner,
                2,
                machine_footer_rows(&hints, inner.width),
                1,
                1,
            );
            Some(MachinesBody::List(stack.content))
        }
        ClientMachinesView::Detail(id) => {
            let (_, inner) = machines_panel(area, page_bounds, 26)?;
            let profile = saved_profiles.iter().find(|profile| &profile.id == id)?;
            let has_review = saved_profiles.iter().any(|profile| {
                connection_errors.contains_key(&ClientEndpointId::Ssh(profile.id.clone()))
            });
            let hints = machine_detail_hints(has_review);
            let labels = machine_detail_labels(profile, endpoints, connection_errors);
            let stack = super::page::PageLayout::with_footer_rows(
                inner,
                0,
                false,
                super::page::action_row_count(inner.width, &labels),
                machine_footer_rows(&hints, inner.width),
            );
            Some(MachinesBody::Detail(stack.content))
        }
        ClientMachinesView::Forwards(_) => {
            let (_, inner) = machines_panel(area, page_bounds, 20)?;
            let stack = crate::ui::modal_stack_areas(inner, 1, 1, 1, 1);
            Some(MachinesBody::Forwards(stack.content))
        }
        ClientMachinesView::Import(view) => {
            let (_, inner) = machines_panel(area, page_bounds, 24)?;
            let stack = crate::ui::modal_stack_areas(inner, 2, 1, 1, 1);
            match view.step {
                ClientImportStep::Discover => None,
                ClientImportStep::Select => {
                    // 候选列表要扣掉固定行（通配符开关 / 分组输入 / 计数行）。
                    let list_height = stack
                        .content
                        .height
                        .saturating_sub(IMPORT_SELECT_FIXED_ROWS);
                    Some(MachinesBody::ImportSelect(
                        stack.content,
                        usize::from(list_height.max(1)),
                    ))
                }
                ClientImportStep::Done => Some(MachinesBody::ImportDone(stack.content)),
            }
        }
        ClientMachinesView::Form(_) => {
            let (_, inner) = machines_panel(area, page_bounds, 24)?;
            let stack = crate::ui::modal_stack_areas(inner, 2, 1, 1, 1);
            Some(MachinesBody::Form(stack.content))
        }
        _ => None,
    }
}

/// 机器面板的弹窗与内框，视图计算与渲染共用（各视图高度不同）。
fn machines_panel(area: Rect, page_bounds: Option<Rect>, height: u16) -> Option<(Rect, Rect)> {
    let outer = page_bounds
        .map(|rect| rect.intersection(area))
        .or_else(|| crate::ui::modal_rect(area, crate::ui::ModalSize::Large.with_height(height)))?;
    let inner = super::render::panel_inner(outer)?;
    (inner.width >= 24 && inner.height >= 8).then_some((outer, inner))
}

const IMPORT_SELECT_FIXED_ROWS: u16 = 3;

impl ClientShellState {
    /// 当前机器面板视图的滚动上界：由视图计算阶段写入（STATE-04）。
    fn machines_view_max_scroll(&self) -> usize {
        match self.overlay.as_ref() {
            Some(ClientShellOverlay::Machines(page)) => page.view_max_scroll,
            _ => 0,
        }
    }

    /// 机器面板的滚动视图计算（STATE-04）：几何与渲染共用 `machines_body`，
    /// 数字在这里写回状态；渲染阶段只读。`reveal` 是一次性请求，消费即清。
    pub(super) fn compute_machines_view(&mut self, area: Rect, page_bounds: Option<Rect>) {
        if !matches!(self.overlay, Some(ClientShellOverlay::Machines(_))) {
            return;
        }
        let endpoints = std::mem::take(&mut self.endpoints);
        let saved_profiles = std::mem::take(&mut self.saved_profiles);
        let connection_errors = std::mem::take(&mut self.endpoint_connection_errors);
        let port_forwards = std::mem::take(&mut self.endpoint_port_forwards);
        let session_log_dropped = std::mem::take(&mut self.session_log_dropped);
        let palette = self.config.palette.clone();
        self.compute_machines_view_with(
            area,
            page_bounds,
            &palette,
            &endpoints,
            &saved_profiles,
            &connection_errors,
            &port_forwards,
            &session_log_dropped,
        );
        self.endpoints = endpoints;
        self.saved_profiles = saved_profiles;
        self.endpoint_connection_errors = connection_errors;
        self.endpoint_port_forwards = port_forwards;
        self.session_log_dropped = session_log_dropped;
    }

    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_arguments)]
    fn compute_machines_view_with(
        &mut self,
        area: Rect,
        page_bounds: Option<Rect>,
        palette: &Palette,
        endpoints: &[ClientShellEndpoint],
        saved_profiles: &[SavedSshEndpoint],
        connection_errors: &HashMap<ClientEndpointId, crate::remote::ConnectionErrorKind>,
        port_forwards: &HashMap<ClientEndpointId, Vec<crate::remote::PortForwardStatus>>,
        session_log_dropped: &HashMap<ProfileId, u64>,
    ) {
        let body = {
            let Some(ClientShellOverlay::Machines(page)) = self.overlay.as_ref() else {
                return;
            };
            machines_body(
                area,
                page_bounds,
                page,
                endpoints,
                saved_profiles,
                connection_errors,
            )
        };
        let Some(body) = body else {
            return;
        };
        let Some(ClientShellOverlay::Machines(page)) = self.overlay.as_mut() else {
            return;
        };
        match body {
            MachinesBody::List(rect) => {
                let rows = machine_list_rows(saved_profiles, endpoints, page.query.as_str());
                let selected = if rows.is_empty() {
                    0
                } else {
                    page.selected.min(rows.len() - 1)
                };
                let window = super::page::list_window(
                    rect,
                    2,
                    rows.len(),
                    page.scroll,
                    selected,
                    page.reveal,
                );
                if !rows.is_empty() && rect.height > 0 {
                    page.scroll = window.start;
                }
                page.view_max_scroll = rows.len().saturating_sub(window.visible);
            }
            MachinesBody::Detail(rect) => {
                let ClientMachinesView::Detail(id) = &page.view else {
                    return;
                };
                let Some(profile) = saved_profiles.iter().find(|profile| &profile.id == id) else {
                    return;
                };
                let endpoint = endpoints
                    .iter()
                    .find(|endpoint| endpoint.endpoint_id == ClientEndpointId::Ssh(id.clone()));
                let status = endpoint.map_or(
                    if profile.enabled {
                        ClientEndpointStatus::Connecting
                    } else {
                        ClientEndpointStatus::Disabled
                    },
                    |endpoint| endpoint.status,
                );
                let error_kind = connection_errors.get(&ClientEndpointId::Ssh(id.clone()));
                let attention_rows =
                    if status == ClientEndpointStatus::Attention && error_kind.is_some() {
                        4
                    } else if status == ClientEndpointStatus::Attention {
                        2
                    } else {
                        0
                    };
                let lines = detail_lines(
                    profile,
                    endpoint,
                    port_forwards
                        .get(&ClientEndpointId::Ssh(id.clone()))
                        .map(Vec::as_slice),
                    session_log_dropped.get(id).copied(),
                );
                let visible = usize::from(rect.height).saturating_sub(attention_rows);
                let max_scroll = lines.len().saturating_sub(visible.max(1));
                page.view_max_scroll = max_scroll;
                page.detail_scroll = page.detail_scroll.min(max_scroll);
            }
            MachinesBody::Form(rect) => {
                let ClientMachinesView::Form(form) = &mut page.view else {
                    return;
                };
                if form.bootstrap.is_some() {
                    page.view_max_scroll = 0;
                    return;
                }
                if form.step == MachineFormStep::Confirm && form.editing.is_none() {
                    page.view_max_scroll = form_confirm_max_scroll(form, rect, palette);
                    return;
                }
                let fields = form.fields();
                let visible = usize::from(rect.height).max(1);
                let focused = form.focused.min(fields.len().saturating_sub(1));
                // 表单没有独立的 reveal 位：聚焦字段必须始终可见（渲染与
                // 输入路径都按同一口径夹紧）。
                let window =
                    super::page::list_window(rect, 1, fields.len(), form.scroll, focused, true);
                if !fields.is_empty() && rect.height > 0 {
                    form.scroll = window.start;
                }
                page.view_max_scroll = fields.len().saturating_sub(visible);
            }
            MachinesBody::Forwards(rect) => {
                let ClientMachinesView::Forwards(view) = &mut page.view else {
                    return;
                };
                let Some(profile) = saved_profiles
                    .iter()
                    .find(|profile| profile.id == view.profile_id)
                else {
                    return;
                };
                let reserved = if view.adding {
                    FORWARD_FORM_FIELDS as u16 + 3
                } else {
                    1
                };
                let list_height = rect.height.saturating_sub(reserved);
                let visible_rows = usize::from(list_height).max(1);
                let rules = profile.port_forwards.as_slice();
                let selected = if rules.is_empty() {
                    0
                } else {
                    view.selected.min(rules.len() - 1)
                };
                // 可见行数取扣掉固定席位后的列表高度，与渲染口径一致。
                let list_rect = Rect::new(rect.x, rect.y, rect.width, list_height);
                let window = super::page::list_window(
                    list_rect,
                    1,
                    rules.len(),
                    view.scroll,
                    selected,
                    view.reveal,
                );
                if !rules.is_empty() && list_height > 0 {
                    view.scroll = window.start;
                }
                view.reveal = false;
                page.view_max_scroll = rules.len().saturating_sub(visible_rows);
            }
            MachinesBody::ImportSelect(rect, visible_rows) => {
                let ClientMachinesView::Import(view) = &mut page.view else {
                    return;
                };
                let candidates = view.plan.ready.len();
                let focus = view.focus_row.min(candidates.saturating_sub(1));
                let list_rect = Rect::new(
                    rect.x,
                    rect.y,
                    rect.width,
                    u16::try_from(visible_rows).unwrap_or(u16::MAX),
                );
                let window = super::page::list_window(
                    list_rect,
                    1,
                    candidates,
                    view.scroll,
                    focus,
                    view.reveal,
                );
                if candidates > 0 && rect.height > 0 {
                    view.scroll = window.start;
                }
                view.reveal = false;
                page.view_max_scroll = candidates.saturating_sub(visible_rows);
            }
            MachinesBody::ImportDone(rect) => {
                let ClientMachinesView::Import(view) = &page.view else {
                    return;
                };
                let imported = view
                    .results
                    .iter()
                    .filter(|row| row.outcome == ClientImportOutcome::Imported)
                    .count();
                let total_lines = view.results.len() + usize::from(imported > 0);
                let visible = usize::from(rect.height.saturating_sub(1));
                page.view_max_scroll = total_lines.saturating_sub(visible);
            }
        }
    }
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
    let render = render_machines_view(
        b,
        overlay,
        endpoints,
        saved_profiles,
        connection_errors,
        port_forwards,
        session_log_dropped,
        cx,
    );
    // 公共 toast 最后画，盖在视图自己的页脚提示之上。
    if let (Some(render), Some(toast)) = (render.as_ref(), overlay.message.as_ref()) {
        render_machines_toast(b, render.machines_toast, &toast.text, cx.palette);
    }
    render
}

// 参数与 `render_machines_overlay` 逐个一致（视图分派本身不带公共 chrome），
// 同理豁免 too_many_arguments：收成 struct 只会把同一组只读引用再包一层。
#[allow(clippy::too_many_arguments)]
fn render_machines_view(
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
        ClientMachinesView::List => {
            render_machine_list(b, overlay, endpoints, saved_profiles, connection_errors, cx)
        }
        ClientMachinesView::Detail(id) => {
            if !saved_profiles.iter().any(|profile| &profile.id == id) {
                // The profile vanished underneath the overlay (external edit);
                // degrade to the list rather than aborting composition.
                return render_machine_list(
                    b,
                    overlay,
                    endpoints,
                    saved_profiles,
                    connection_errors,
                    cx,
                );
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
                return render_machine_list(
                    b,
                    overlay,
                    endpoints,
                    saved_profiles,
                    connection_errors,
                    cx,
                );
            }
            render_machine_confirm_remove(b, id, saved_profiles, cx)
        }
        ClientMachinesView::Form(form) => render_machine_form(b, form, cx),
        ClientMachinesView::Forwards(view) => {
            if !saved_profiles
                .iter()
                .any(|profile| profile.id == view.profile_id)
            {
                return render_machine_list(
                    b,
                    overlay,
                    endpoints,
                    saved_profiles,
                    connection_errors,
                    cx,
                );
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
    connection_errors: &HashMap<ClientEndpointId, crate::remote::ConnectionErrorKind>,
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
    let rows = machine_list_rows(saved_profiles, endpoints, overlay.query.as_str());
    // `v 处理失败` 的出现条件与 dashboard 同一张表：用选中机器的失败类型，
    // 而不是硬编码 false（窄屏 List 此前永远不显示它）。
    let selected = if rows.is_empty() {
        0
    } else {
        overlay.selected.min(rows.len() - 1)
    };
    let has_review = rows.get(selected).is_some_and(|row| {
        connection_errors
            .get(&ClientEndpointId::Ssh(row.id.clone()))
            .is_some_and(super::machine_auth_overlay::failure_kind_has_review)
    });
    let hints = machine_list_hints(!rows.is_empty(), has_review);
    let stack =
        crate::ui::modal_stack_areas(inner, 2, machine_footer_rows(&hints, inner.width), 1, 1);
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
        render_key_hints(b, footer, &hints, p, cx.components);
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
        machines_search: Rect::new(stack.header.x, stack.header.y + 1, stack.header.width, 1),
        machines_rows: row_hits,
        machines_actions: action_hits,
        machines_toast: stack.footer.unwrap_or_default(),
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
    let labels = machine_detail_labels(profile, endpoints, connection_errors);
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
    // 页脚键表先算出来：Detail 的单机动作表同样在单行里必然被尾部截断
    // （HERDR-MACH-007「同序同集」只有在页脚真的画得下时才成立）。
    let hints = machine_detail_hints(has_review);
    let stack = super::page::PageLayout::with_footer_rows(
        inner,
        0,
        false,
        super::page::action_row_count(inner.width, &labels),
        machine_footer_rows(&hints, inner.width),
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

    render_key_hints(b, stack.footer, &hints, p, cx.components);

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
        machines_toast: stack.footer,
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
fn form_confirm_max_scroll(form: &ClientMachineForm, area: Rect, palette: &Palette) -> usize {
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
    // 规则列表只占 body 的上半部：添加表单（空行 + 标题 + 字段 + 错误行）与
    // 非添加态的消息/确认行都有固定席位，规则多于一屏时列表自己滚动而不是把
    // 表单挤出画面（HERDR-MACH-005）。
    let reserved = if view.adding {
        FORWARD_FORM_FIELDS as u16 + 3
    } else {
        1
    };
    let list_height = body.height.saturating_sub(reserved);
    // 窗口起点与滚动上界共用同一个提升后的行数，否则 body 高度不足时上界比
    // 渲染实际接受的位点多一整屏。
    let visible_rows = usize::from(list_height).max(1);
    let rules = profile.port_forwards.as_slice();
    let selected = if rules.is_empty() {
        0
    } else {
        view.selected.min(rules.len() - 1)
    };
    // 确认态在本帧是否仍然成立：下标与规则值都要对得上，否则整帧（横幅、
    // 页脚、按钮行）一致地按「非确认态」画，不出现点不到名的确认横幅。
    let pending_rule = view.pending_remove.as_ref().and_then(|armed| {
        rules
            .get(armed.index)
            .filter(|rule| **rule == armed.rule && !view.adding)
    });
    let scroll = super::page::list_start(
        view.scroll,
        selected,
        rules.len(),
        visible_rows,
        view.reveal,
    );
    if rules.is_empty() && !view.adding {
        put_text(
            b,
            body.x,
            body.y,
            body.width,
            t.forward_none,
            base.fg(p.overlay1),
        );
        put_text(
            b,
            body.x,
            body.y + 1,
            body.width,
            t.forward_none_hint,
            base.fg(p.overlay0),
        );
    }
    for (index, rule) in rules
        .iter()
        .enumerate()
        .skip(scroll)
        .take(usize::from(list_height))
    {
        let rect = Rect::new(body.x, body.y + (index - scroll) as u16, body.width, 1);
        row_hits.push((rect, index));
        let is_selected = index == selected && !view.adding;
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
    }
    // 列表之后的内容一律从固定席位开始，不受滚动窗口影响。
    let mut y = body.y + list_height;

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
                        crate::ui::input_field_style(p)
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
                    let field_style = crate::ui::input_field_style(p);
                    b.set_style(input, field_style);
                    let inner_input =
                        Rect::new(input.x + 1, input.y, input.width.saturating_sub(1), 1);
                    let field_cursor = text_editor::render(b, inner_input, editor, field_style);
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
    } else if let Some(rule) = pending_rule {
        // 待确认删除：点名规则本身，避免「删掉看不见的那一条」（HERDR-MACH-025）。
        if y < body.bottom() {
            put_text(
                b,
                body.x,
                y,
                body.width,
                &format!(
                    " {}",
                    crate::i18n::fill(
                        t.forward_remove_confirm_fmt,
                        &[("rule", &forward_rule_display(rule))]
                    )
                ),
                base.fg(p.red).add_modifier(Modifier::BOLD),
            );
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
        } else if pending_rule.is_some() {
            vec![
                ("enter".to_owned(), t.hint_confirm.to_owned()),
                (
                    "esc".to_owned(),
                    crate::i18n::texts()
                        .overlays
                        .cancel_button
                        .trim()
                        .to_owned(),
                ),
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
    } else if pending_rule.is_some() {
        (
            vec![
                crate::i18n::texts().overlays.confirm_button,
                crate::i18n::texts().overlays.cancel_button,
                back_label,
            ],
            vec![
                MachineOverlayButton::ForwardRemove,
                MachineOverlayButton::ForwardCancelRemove,
                MachineOverlayButton::Back,
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
            let enabled = button != MachineOverlayButton::ForwardRemove || !rules.is_empty();
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
    // discover 步骤没有可滚动列表：窗口由视图计算阶段按步骤给出（STATE-04）。
    let wizard_rows: Vec<(Rect, usize)> = match view.step {
        ClientImportStep::Discover => {
            render_import_discover(b, body, view, base, p);
            Vec::new()
        }
        ClientImportStep::Select => {
            let rendered = render_import_select(b, body, view, base, p);
            cursor = rendered.cursor;
            rendered.rows
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
            ClientImportStep::Done => vec![
                ("↑↓".to_owned(), t.hint_scroll.to_owned()),
                ("esc/enter".to_owned(), t.hint_back.to_owned()),
            ],
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
    if !view.plan.skipped.is_empty() && y < body.bottom() {
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

/// select 步骤的渲染产物：窗口内的可点击行（候选、通配符开关、分组输入）、
/// 分组输入聚焦时的文本光标，以及本帧采用的滚动窗口起点与上界。
struct ImportSelectRender {
    rows: Vec<(Rect, usize)>,
    cursor: Option<crate::protocol::CursorState>,
}

/// 候选列表在上方滚动，通配符开关 / 分组输入 / 计数行固定占据 body 底部三行，
/// 主机多于一屏时它们仍然可见可点（HERDR-MACH-001）。只收录窗口内的候选行，
/// 保证鼠标命中与画面一致。
fn render_import_select(
    b: &mut Buffer,
    body: Rect,
    view: &ClientMachineImportView,
    base: Style,
    p: &Palette,
) -> ImportSelectRender {
    /// 通配符开关、分组输入、计数行。
    const FIXED_ROWS: u16 = 3;
    let t = &crate::i18n::texts().machines;
    let mut hits = Vec::new();
    let candidates = view.plan.ready.len();
    let list_height = body.height.saturating_sub(FIXED_ROWS);
    // 窗口起点与滚动上界共用同一个提升后的行数，口径保持一致。
    let visible_rows = usize::from(list_height).max(1);
    let focus = view.focus_row.min(candidates.saturating_sub(1));
    let scroll = super::page::list_start(view.scroll, focus, candidates, visible_rows, view.reveal);
    let max_scroll = candidates.saturating_sub(visible_rows);
    for (index, planned) in view
        .plan
        .ready
        .iter()
        .enumerate()
        .skip(scroll)
        .take(usize::from(list_height))
    {
        let rect = Rect::new(body.x, body.y + (index - scroll) as u16, body.width, 1);
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
    }
    let mut y = body.y + list_height;
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
        let field_style = crate::ui::input_field_style(p);
        b.set_style(input, field_style);
        let inner_input = Rect::new(input.x + 1, input.y, input.width.saturating_sub(1), 1);
        let field_cursor = text_editor::render(b, inner_input, &view.group, field_style);
        if focused {
            cursor = field_cursor;
        }
        y += 1;
    }
    // Selection counter.
    if y < body.bottom() {
        let selected = view.selected.iter().filter(|selected| **selected).count();
        let mut counter = format!(
            " {}",
            crate::i18n::fill(
                t.import_selected_fmt,
                &[
                    ("selected", &selected.to_string()),
                    ("total", &candidates.to_string())
                ]
            )
        );
        // 滚动位置走自己的本地化条目：裸 `X/Y` 直接拼在「已选 12/30」后面
        // 会读成第二个计数。
        if max_scroll > 0 {
            counter.push_str("  ");
            counter.push_str(&crate::i18n::fill(
                t.import_scroll_position_fmt,
                &[
                    ("start", &(scroll + 1).to_string()),
                    ("total", &candidates.to_string()),
                ],
            ));
        }
        put_text(b, body.x, y, body.width, &counter, base.fg(p.overlay0));
    }
    ImportSelectRender { rows: hits, cursor }
}

/// done 步骤：汇总行固定在顶部，逐条结果（含失败提示与结尾说明）进入可滚动
/// 区域，主机多时末尾条目不再被截断（HERDR-MACH-010）。返回滚动窗口起点与上界。
fn render_import_done(
    b: &mut Buffer,
    body: Rect,
    view: &ClientMachineImportView,
    base: Style,
    p: &Palette,
) -> (usize, usize) {
    let t = &crate::i18n::texts().machines;
    if body.height == 0 {
        return (0, 0);
    }
    let (imported, skipped, failed) = view.summary;
    put_text(
        b,
        body.x,
        body.y,
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
    // done 步骤和其它步骤一样每次 repaint 都重画（后台 pane 有流式输出时
    // 30–60 fps），所以行数只做计数、不做格式化：全量 `format!` 会按
    // 频率(repaint) × 基数(~/.ssh/config 的 Host 数) 加宽每帧工作量。
    let failed_hints = view
        .results
        .iter()
        .filter(|row| row.outcome == ClientImportOutcome::Failed)
        .count();
    let total_lines = view.results.len() + failed_hints + usize::from(imported > 0);
    let list = Rect::new(
        body.x,
        body.y + 1,
        body.width,
        body.height.saturating_sub(1),
    );
    let visible = usize::from(list.height);
    let scroll =
        super::page::list_start(view.scroll, view.scroll, total_lines, visible.max(1), false);
    let max_scroll = total_lines.saturating_sub(visible);
    // 只对落在窗口里的行做 `format!`：`line` 按与计数完全相同的顺序推进。
    let end = scroll.saturating_add(visible);
    let mut line = 0usize;
    for row in &view.results {
        if line >= end {
            break;
        }
        let (tag, color) = match row.outcome {
            ClientImportOutcome::Imported => (t.import_result_imported, p.green),
            ClientImportOutcome::Skipped => (t.import_result_skipped, p.overlay0),
            ClientImportOutcome::Failed => (t.import_result_failed, p.red),
        };
        if line >= scroll {
            let text = if row.detail.is_empty() {
                format!(" {tag} {}", row.label)
            } else {
                format!(" {tag} {} — {}", row.label, row.detail)
            };
            put_text(
                b,
                list.x,
                list.y + (line - scroll) as u16,
                list.width,
                &text,
                base.fg(color),
            );
        }
        line += 1;
        if row.outcome == ClientImportOutcome::Failed {
            if line >= end {
                break;
            }
            if line >= scroll {
                put_text(
                    b,
                    list.x,
                    list.y + (line - scroll) as u16,
                    list.width,
                    &format!("   {}", t.import_failed_hint),
                    base.fg(p.yellow),
                );
            }
            line += 1;
        }
    }
    if imported > 0 && line >= scroll && line < end {
        put_text(
            b,
            list.x,
            list.y + (line - scroll) as u16,
            list.width,
            t.import_connect_note,
            base.fg(p.overlay1),
        );
    }
    (scroll, max_scroll)
}
