//! 机器表单：添加与编辑共用的单页表单——顶部「快速输入」、分组字段、逐字段
//! 内联校验、「测试连接」与保存。
//!
//! 保存与测试是两个独立动作：保存只写目录（目录 watcher 随后建立连接，与导入
//! 同一条路），测试连接复用远程 bootstrap 链（`BootstrapMachine`，按
//! `BOOTSTRAP_STEPS` 报进度）而不落盘；测试失败按失败类型把 host key / 认证
//! 问题交给 `MachineAuth` 二级浮层。版面与渲染在 `form_view`。

use super::quick::{flatten_pasted_command, parse_quick_input, QuickInput};
use super::*;

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

/// 表单里的一个输入位。`Quick` 是顶部的快速输入框（只在添加时出现），其余
/// 与机器档案字段一一对应。判别式同时是 `touched` 位掩码的位号。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::client::shell) enum MachineField {
    Quick,
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
    pub(super) fn label(self) -> &'static str {
        let t = &crate::i18n::texts().machines;
        match self {
            Self::Quick => crate::i18n::texts().machine_form.quick_label,
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

    pub(super) fn hint(self) -> Option<&'static str> {
        let t = &crate::i18n::texts().machines;
        match self {
            Self::IdentityFiles => Some(t.hint_identity_files),
            Self::ProxyJump => Some(t.hint_proxy_jump),
            Self::Color => Some(t.hint_color),
            Self::SessionLogPath => Some(t.hint_session_log_path),
            _ => None,
        }
    }

    pub(super) fn is_choice(self) -> bool {
        matches!(
            self,
            Self::IdentitiesOnly
                | Self::StrictHostKey
                | Self::ForwardAgent
                | Self::SessionLogEnabled
        )
    }

    /// 改了它，上一次测试连接的结论就不再代表当前设置。标签、分组、标签页
    /// 颜色与会话日志只影响呈现 / 记录，不影响连接。
    fn affects_connection(self) -> bool {
        !matches!(
            self,
            Self::Quick
                | Self::Label
                | Self::Group
                | Self::Tags
                | Self::Color
                | Self::SessionLogEnabled
                | Self::SessionLogPath
                | Self::SessionLogMaxBytes
                | Self::SessionLogInterval
        )
    }

    fn bit(self) -> u32 {
        1 << (self as u32)
    }
}

/// 字段分组（渲染为组标题）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FieldGroup {
    Connection,
    Auth,
    Session,
    Advanced,
}

impl FieldGroup {
    pub(super) fn title(self) -> &'static str {
        let t = &crate::i18n::texts().machine_form;
        match self {
            Self::Connection => t.group_connection,
            Self::Auth => t.group_auth,
            Self::Session => t.group_session,
            Self::Advanced => t.group_advanced,
        }
    }
}

/// 分组与组内字段的展示顺序；键盘焦点顺序由它派生（见 `ADD_FOCUS_ORDER`）。
pub(super) const FORM_GROUPS: [(FieldGroup, &[MachineField]); 4] = [
    (
        FieldGroup::Connection,
        &[
            MachineField::Target,
            MachineField::User,
            MachineField::Port,
            MachineField::ProxyJump,
        ],
    ),
    (
        FieldGroup::Auth,
        &[
            MachineField::IdentityFiles,
            MachineField::IdentityAgent,
            MachineField::IdentitiesOnly,
            MachineField::StrictHostKey,
            MachineField::ForwardAgent,
        ],
    ),
    (
        FieldGroup::Session,
        &[
            MachineField::Label,
            MachineField::Session,
            MachineField::Group,
            MachineField::Tags,
            MachineField::Color,
        ],
    ),
    (
        FieldGroup::Advanced,
        &[
            MachineField::ServerAliveInterval,
            MachineField::ServerAliveCountMax,
            MachineField::ControlPersist,
            MachineField::RemoteCommand,
            MachineField::SessionLogEnabled,
            MachineField::SessionLogPath,
            MachineField::SessionLogMaxBytes,
            MachineField::SessionLogInterval,
        ],
    ),
];

/// 添加时的键盘焦点顺序：快速输入 + 各组字段（与 `FORM_GROUPS` 同序）。编辑
/// 时去掉前两项：没有快速输入，目标是档案身份、只读展示不聚焦。
const ADD_FOCUS_ORDER: &[MachineField] = &[
    MachineField::Quick,
    MachineField::Target,
    MachineField::User,
    MachineField::Port,
    MachineField::ProxyJump,
    MachineField::IdentityFiles,
    MachineField::IdentityAgent,
    MachineField::IdentitiesOnly,
    MachineField::StrictHostKey,
    MachineField::ForwardAgent,
    MachineField::Label,
    MachineField::Session,
    MachineField::Group,
    MachineField::Tags,
    MachineField::Color,
    MachineField::ServerAliveInterval,
    MachineField::ServerAliveCountMax,
    MachineField::ControlPersist,
    MachineField::RemoteCommand,
    MachineField::SessionLogEnabled,
    MachineField::SessionLogPath,
    MachineField::SessionLogMaxBytes,
    MachineField::SessionLogInterval,
];

/// 表单上待确认的一步：页脚上方一条说明 + 页脚换成确认 / 取消两项。确认条
/// 在时表单只读：Enter 确认、Esc 取消，其余键与字段点击都不动表单。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::client::shell) enum FormPrompt {
    /// 测试连接以预先授权模式跑 bootstrap：必要时在远端安装 / 更新，并在
    /// 更新要求时停止正在运行的 server 及其 pane 进程。先写明后果再下发。
    ConfirmTest,
}

/// 快速输入框最近一次解析的结论（显示在输入框下方）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum QuickStatus {
    Filled(usize),
    Failed,
}

#[derive(Debug)]
pub(in crate::client::shell) struct ClientMachineForm {
    pub(in crate::client::shell) editing: Option<ProfileId>,
    /// 下标进 `fields()`（键盘焦点顺序）。
    pub(in crate::client::shell) focused: usize,
    /// 字段栏的滚动起点（行）：视图计算阶段按 `reveal` 与版面写入，渲染只读。
    pub(in crate::client::shell) scroll: usize,
    /// 一次性「把聚焦字段滚进窗口」请求：键盘 / 点击改焦点或输入时置位，
    /// 视图计算阶段消费；滚轮滚动时清掉，免得下一帧又被拉回焦点。
    pub(in crate::client::shell) reveal: bool,
    pub(in crate::client::shell) quick: TextEditor,
    pub(super) quick_status: Option<QuickStatus>,
    /// 快速输入改过、还没解析：失焦或保存 / 测试前先解析一次。
    pub(super) quick_dirty: bool,
    /// 用户动过（输入或失焦）的字段位掩码：只有动过的字段才显示内联错误，
    /// 首屏不会一片红；保存 / 测试尝试之后 `submitted` 让全部错误现形。
    pub(super) touched: u32,
    pub(super) submitted: bool,
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
    /// 目录层面的失败（落盘出错、字段之间的约束）：画在页脚上方。
    pub(in crate::client::shell) error: Option<String>,
    /// 页脚上方待确认的一步（见 `FormPrompt`）。
    pub(in crate::client::shell) prompt: Option<FormPrompt>,
    /// 测试连接：运行中 / 通过 / 失败。
    pub(in crate::client::shell) bootstrap: Option<ClientMachineBootstrap>,
}

/// 一次「测试连接」：远程 bootstrap 链的进度与结论。只测不存。
#[derive(Debug)]
pub(in crate::client::shell) struct ClientMachineBootstrap {
    pub(in crate::client::shell) cancel: crate::remote::TaskCancellation,
    pub(in crate::client::shell) ticket: u64,
    pub(in crate::client::shell) step: Option<SavedSshBootstrapStep>,
    pub(in crate::client::shell) failure: Option<String>,
    pub(in crate::client::shell) passed: bool,
}

impl Drop for ClientMachineBootstrap {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl ClientMachineBootstrap {
    pub(super) fn running(&self) -> bool {
        self.failure.is_none() && !self.passed
    }

    /// 失败文本的结构化分类。bootstrap 链经 `classify_and_wrap` 保留了原始
    /// `Display`，按文本重新分类与 supervisor 同口径。
    pub(super) fn failure_kind(&self) -> Option<crate::remote::ConnectionErrorKind> {
        self.failure.as_deref().map(|failure| {
            crate::remote::classify_connection_error(&std::io::Error::other(failure.to_owned()))
        })
    }

    /// 失败后可走的二级恢复入口。
    pub(super) fn recovery(&self) -> Option<TestRecovery> {
        self.failure_kind().as_ref().and_then(test_recovery)
    }
}

/// 测试失败后的恢复入口：都在 `MachineAuth` 二级浮层里完成。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TestRecovery {
    /// 主机密钥未知：扫描指纹后确认信任。
    HostKey,
    /// 主机密钥变了：硬阻断，只提供清掉旧密钥后重试。
    HostKeyChanged,
    /// 认证：经批准的交互认证（也能应答 ssh 的主机密钥提问）。
    Auth,
}

/// 失败类型 → 恢复入口。DNS / 超时 / 远程安装 / 协议问题不是交互能解决的，
/// 只给修复提示与命令；未能归类的失败按认证处理（交互认证兼容 ssh 的
/// 各种提问，最通用）。
pub(super) fn test_recovery(kind: &crate::remote::ConnectionErrorKind) -> Option<TestRecovery> {
    use crate::remote::ConnectionErrorKind as Kind;
    match kind {
        Kind::HostKeyUnknown { .. } => Some(TestRecovery::HostKey),
        Kind::HostKeyChanged => Some(TestRecovery::HostKeyChanged),
        Kind::AuthRequired { .. } | Kind::AuthDenied | Kind::Other => Some(TestRecovery::Auth),
        Kind::Dns
        | Kind::Timeout
        | Kind::RemoteInstallRequired
        | Kind::RemoteInstallFailed
        | Kind::Protocol => None,
    }
}

pub(super) const STRICT_HOST_KEY_CHOICES: [Option<StrictHostKeyChecking>; 4] = [
    None,
    Some(StrictHostKeyChecking::Ask),
    Some(StrictHostKeyChecking::AcceptNew),
    Some(StrictHostKeyChecking::Yes),
];

fn invalid_value(flag: &str, raw: &str) -> String {
    crate::i18n::fill(
        crate::i18n::texts().cli_errors.invalid_flag_value_fmt,
        &[("flag", flag), ("value", raw)],
    )
}

fn parse_u16_field(editor: &TextEditor, flag: &str) -> Result<Option<u16>, String> {
    let raw = editor.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    raw.parse::<u16>()
        .map(Some)
        .map_err(|_| invalid_value(flag, raw))
}

fn parse_u64_field(editor: &TextEditor, flag: &str) -> Result<Option<u64>, String> {
    let raw = editor.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    raw.parse::<u64>()
        .map(Some)
        .map_err(|_| invalid_value(flag, raw))
}

fn parse_port_field(editor: &TextEditor) -> Result<Option<u16>, String> {
    let raw = editor.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    match raw.parse::<u16>() {
        Ok(port) if port > 0 => Ok(Some(port)),
        _ => Err(crate::i18n::texts().machine_form.err_port.to_owned()),
    }
}

fn split_list(editor: &TextEditor) -> Vec<String> {
    editor
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect()
}

fn parse_proxy_jump(editor: &TextEditor) -> Result<Vec<ProxyJumpHop>, String> {
    let mut hops = Vec::new();
    for hop in split_list(editor) {
        match hop.strip_prefix("profile:") {
            Some(id) => hops.push(ProxyJumpHop::Profile(
                ProfileId::parse(id).map_err(|error| error.to_string())?,
            )),
            None => hops.push(ProxyJumpHop::Target(hop)),
        }
    }
    Ok(hops)
}

/// 与目录校验同口径（`catalog::is_valid_control_persist`）：yes / no / 数字加
/// 可选单位。提前在字段上报，而不是存盘时才被目录挡回。
fn valid_control_persist(value: &str) -> bool {
    if matches!(value, "yes" | "no") {
        return true;
    }
    let digits = value
        .strip_suffix(['s', 'm', 'h', 'd', 'w'])
        .unwrap_or(value);
    !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
}

/// 目标主机的字段级校验：必填、不以 `-` 开头、不含空白 / 控制字符、不带
/// 密码。其余（长度上限等）交给目录校验。
fn validate_target(raw: &str) -> Result<(), String> {
    let t = &crate::i18n::texts().machine_form;
    if raw.is_empty() {
        return Err(t.err_required.to_owned());
    }
    let authority = raw.strip_prefix("ssh://").unwrap_or(raw);
    let password = authority
        .rsplit_once('@')
        .is_some_and(|(userinfo, _)| userinfo.contains(':'));
    if raw.starts_with('-')
        || password
        || raw.chars().any(|ch| ch.is_whitespace() || ch.is_control())
    {
        return Err(t.err_host.to_owned());
    }
    Ok(())
}

impl ClientMachineForm {
    pub(in crate::client::shell) fn blank() -> Self {
        Self {
            editing: None,
            focused: 0,
            scroll: 0,
            reveal: true,
            quick: TextEditor::default(),
            quick_status: None,
            quick_dirty: false,
            touched: 0,
            submitted: false,
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
            prompt: None,
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

    /// 键盘焦点顺序。
    pub(in crate::client::shell) fn fields(&self) -> &'static [MachineField] {
        if self.editing.is_some() {
            &ADD_FOCUS_ORDER[2..]
        } else {
            ADD_FOCUS_ORDER
        }
    }

    pub(super) fn focused_field(&self) -> Option<MachineField> {
        self.fields().get(self.focused).copied()
    }

    /// 字段此刻能否聚焦 / 编辑：编辑时目标只读，快速输入只在添加时存在。
    pub(super) fn is_editable(&self, field: MachineField) -> bool {
        self.fields().contains(&field)
    }

    /// 这张表单是否提供「测试连接」及其恢复入口：只有添加时提供。编辑已保存
    /// 的机器不跑 bootstrap（与旧向导一致）：测试走预先授权的安装链，恢复
    /// 路径的临时档案带新 id，交互认证成功会按新机器落盘，编辑态走这条路会
    /// 复制出第二条同目标档案而原档案收不到编辑。
    pub(super) fn can_test(&self) -> bool {
        self.editing.is_none()
    }

    /// 测试连接正在跑：表单只读，只接受 Esc 取消。
    pub(super) fn running(&self) -> bool {
        self.bootstrap
            .as_ref()
            .is_some_and(ClientMachineBootstrap::running)
    }

    pub(super) fn editor(&self, field: MachineField) -> Option<&TextEditor> {
        Some(match field {
            MachineField::Quick => &self.quick,
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
            MachineField::Quick => &mut self.quick,
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
            _ => return,
        }
        self.mark_edited(field);
    }

    pub(super) fn choice_label(&self, field: MachineField) -> String {
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

    pub(super) fn effective_label(&self) -> String {
        let label = self.label.trim();
        if label.is_empty() {
            self.target.trim().to_owned()
        } else {
            label.to_owned()
        }
    }

    pub(super) fn effective_session(&self) -> String {
        let session = self.session.trim();
        if session.is_empty() {
            crate::session::DEFAULT_SESSION_NAME.to_owned()
        } else {
            session.to_owned()
        }
    }

    /// 字段内容变了：记为动过；连接相关的改动让上一次测试结论作废（运行中
    /// 表单只读，到不了这里）。
    fn mark_edited(&mut self, field: MachineField) {
        self.touched |= field.bit();
        self.reveal = true;
        if field == MachineField::Quick {
            self.quick_dirty = true;
            self.quick_status = None;
        } else if field.affects_connection() && !self.running() {
            self.bootstrap = None;
        }
    }

    /// 改焦点：离开的字段记为动过（失焦校验），离开快速输入且内容没解析过就
    /// 先解析。
    pub(in crate::client::shell) fn set_focus(&mut self, index: usize) {
        let count = self.fields().len();
        if count == 0 {
            return;
        }
        let index = index.min(count - 1);
        if let Some(previous) = self.focused_field() {
            if index != self.focused {
                self.touched |= previous.bit();
                if previous == MachineField::Quick && self.quick_dirty {
                    self.apply_quick();
                }
            }
        }
        self.focused = index;
        self.reveal = true;
    }

    fn move_focus(&mut self, delta: isize, wrap: bool) {
        let count = self.fields().len() as isize;
        if count == 0 {
            return;
        }
        let next = self.focused as isize + delta;
        let next = if wrap {
            next.rem_euclid(count)
        } else {
            next.clamp(0, count - 1)
        };
        self.set_focus(next as usize);
    }

    /// 解析快速输入并填入字段。目标 / 用户 / 端口三项视作一体（对应同一个
    /// `user@host:port`），总是整组覆盖；其余字段只在输入里出现时才填。
    /// 返回是否成功。
    pub(super) fn apply_quick(&mut self) -> bool {
        self.quick_dirty = false;
        if self.quick.trim().is_empty() {
            self.quick_status = None;
            return false;
        }
        match parse_quick_input(self.quick.as_str()) {
            Ok(parsed) => {
                let filled = self.fill_from_quick(&parsed);
                self.quick_status = Some(QuickStatus::Filled(filled));
                if !self.running() {
                    self.bootstrap = None;
                }
                true
            }
            Err(_) => {
                self.quick_status = Some(QuickStatus::Failed);
                false
            }
        }
    }

    fn fill_from_quick(&mut self, parsed: &QuickInput) -> usize {
        let mut filled = 0usize;
        let mut set =
            |editor: &mut TextEditor, value: &str, field: MachineField, touched: &mut u32| {
                *editor = TextEditor::new(value, false);
                *touched |= field.bit();
                if !value.is_empty() {
                    filled += 1;
                }
            };
        let mut touched = self.touched;
        set(
            &mut self.target,
            &parsed.host,
            MachineField::Target,
            &mut touched,
        );
        set(
            &mut self.user,
            parsed.user.as_deref().unwrap_or_default(),
            MachineField::User,
            &mut touched,
        );
        set(
            &mut self.port,
            &parsed.port.map(|port| port.to_string()).unwrap_or_default(),
            MachineField::Port,
            &mut touched,
        );
        if !parsed.identity_files.is_empty() {
            set(
                &mut self.identity_files,
                &parsed.identity_files.join(", "),
                MachineField::IdentityFiles,
                &mut touched,
            );
        }
        if let Some(hops) = &parsed.proxy_jump {
            set(
                &mut self.proxy_jump,
                &hops.join(", "),
                MachineField::ProxyJump,
                &mut touched,
            );
        }
        if let Some(agent) = &parsed.identity_agent {
            set(
                &mut self.identity_agent,
                agent,
                MachineField::IdentityAgent,
                &mut touched,
            );
        }
        if let Some(interval) = parsed.server_alive_interval {
            set(
                &mut self.server_alive_interval,
                &interval.to_string(),
                MachineField::ServerAliveInterval,
                &mut touched,
            );
        }
        if let Some(count) = parsed.server_alive_count_max {
            set(
                &mut self.server_alive_count_max,
                &count.to_string(),
                MachineField::ServerAliveCountMax,
                &mut touched,
            );
        }
        if let Some(persist) = &parsed.control_persist {
            set(
                &mut self.control_persist,
                persist,
                MachineField::ControlPersist,
                &mut touched,
            );
        }
        if let Some(flag) = parsed.forward_agent {
            self.forward_agent = TriChoice::from_bool(Some(flag));
            filled += 1;
        }
        if let Some(flag) = parsed.identities_only {
            self.identities_only = TriChoice::from_bool(Some(flag));
            filled += 1;
        }
        if let Some(checking) = parsed.strict_host_key {
            if let Some(index) = STRICT_HOST_KEY_CHOICES
                .iter()
                .position(|choice| *choice == Some(checking))
            {
                self.strict_host_key = index;
                filled += 1;
            }
        }
        self.touched = touched;
        filled
    }

    /// 单字段校验：`Ok` 表示该字段本身合法（可以为空）。`saved` 用于名称查重
    /// （编辑时排除自己）。
    pub(in crate::client::shell) fn validate_field(
        &self,
        field: MachineField,
        saved: &[SavedSshEndpoint],
    ) -> Result<(), String> {
        match field {
            MachineField::Quick => Ok(()),
            MachineField::Target => {
                if self.editing.is_some() {
                    // 目标是档案身份，编辑时不可改，也就无从校验。
                    Ok(())
                } else {
                    validate_target(self.target.trim())
                }
            }
            MachineField::Label => {
                let label = self.effective_label();
                let taken = !label.is_empty()
                    && saved.iter().any(|profile| {
                        Some(&profile.id) != self.editing.as_ref()
                            && profile.label.trim().eq_ignore_ascii_case(&label)
                    });
                if taken {
                    Err(crate::i18n::texts()
                        .machine_form
                        .err_duplicate_name
                        .to_owned())
                } else {
                    Ok(())
                }
            }
            MachineField::Session => crate::session::validate_name(&self.effective_session()),
            MachineField::Port => parse_port_field(&self.port).map(|_| ()),
            MachineField::User => {
                let user = self.user.trim();
                if user.chars().any(|ch| ch.is_whitespace() || ch == '@') {
                    Err(invalid_value("--user", user))
                } else {
                    Ok(())
                }
            }
            MachineField::Color => {
                let color = self.color.trim();
                if color.is_empty() || crate::config::try_parse_color(color).is_some() {
                    Ok(())
                } else {
                    Err(invalid_value("--color", color))
                }
            }
            MachineField::ProxyJump => parse_proxy_jump(&self.proxy_jump).map(|_| ()),
            MachineField::ServerAliveInterval => {
                parse_u16_field(&self.server_alive_interval, "--server-alive-interval").map(|_| ())
            }
            MachineField::ServerAliveCountMax => {
                parse_u16_field(&self.server_alive_count_max, "--server-alive-count-max")
                    .map(|_| ())
            }
            MachineField::ControlPersist => {
                let persist = self.control_persist.trim();
                if persist.is_empty() || valid_control_persist(persist) {
                    Ok(())
                } else {
                    Err(invalid_value("--control-persist", persist))
                }
            }
            // 会话日志文本只在显式选了是 / 否时才参与档案（默认沿用已存值）。
            MachineField::SessionLogMaxBytes if self.session_log_enabled != TriChoice::Default => {
                parse_u64_field(&self.session_log_max_bytes, "--log-max-bytes").map(|_| ())
            }
            MachineField::SessionLogInterval if self.session_log_enabled != TriChoice::Default => {
                parse_u16_field(&self.session_log_interval, "--log-interval").map(|_| ())
            }
            MachineField::Group
            | MachineField::Tags
            | MachineField::IdentityFiles
            | MachineField::IdentityAgent
            | MachineField::IdentitiesOnly
            | MachineField::StrictHostKey
            | MachineField::ForwardAgent
            | MachineField::RemoteCommand
            | MachineField::SessionLogEnabled
            | MachineField::SessionLogPath
            | MachineField::SessionLogMaxBytes
            | MachineField::SessionLogInterval => Ok(()),
        }
    }

    /// 字段此刻要显示的内联错误：动过的字段，或提交过一次之后的全部字段。
    pub(super) fn visible_error(
        &self,
        field: MachineField,
        saved: &[SavedSshEndpoint],
    ) -> Option<String> {
        if !self.submitted && self.touched & field.bit() == 0 {
            return None;
        }
        self.validate_field(field, saved).err()
    }

    /// 焦点顺序里第一个不合法的字段。
    fn first_invalid(&self, saved: &[SavedSshEndpoint]) -> Option<usize> {
        self.fields()
            .iter()
            .position(|field| self.validate_field(*field, saved).is_err())
    }

    /// 保存 / 测试前的统一闸门：先解析没解析过的快速输入，再逐字段校验；
    /// 有错就让全部错误现形并把焦点移到第一个错处。
    fn submit_gate(&mut self, saved: &[SavedSshEndpoint]) -> bool {
        if self.quick_dirty {
            self.apply_quick();
        }
        self.submitted = true;
        match self.first_invalid(saved) {
            Some(index) => {
                self.set_focus(index);
                self.error = Some(crate::i18n::texts().machine_form.fix_fields.to_owned());
                false
            }
            None => {
                self.error = None;
                true
            }
        }
    }

    /// 由各字段的解析函数组装档案选项（与 `validate_field` 共用同一批解析）。
    fn profile_options(&self) -> Result<SshProfileOptions, String> {
        let session_log = match self.session_log_enabled {
            // Untouched: the saved value passes through verbatim.
            TriChoice::Default => self.session_log.clone(),
            choice => Some(SessionLogProfile {
                enabled: choice.as_bool() == Some(true),
                path_template: nonempty(self.session_log_path.trim()),
                max_bytes: parse_u64_field(&self.session_log_max_bytes, "--log-max-bytes")?,
                dump_interval_secs: parse_u16_field(&self.session_log_interval, "--log-interval")?,
            }),
        };
        Ok(SshProfileOptions {
            group: nonempty(self.group.trim()),
            tags: split_list(&self.tags),
            color: nonempty(self.color.trim()),
            port: parse_port_field(&self.port)?,
            user: nonempty(self.user.trim()),
            identity_file: split_list(&self.identity_files),
            identities_only: self.identities_only.as_bool(),
            identity_agent: nonempty(self.identity_agent.trim()),
            strict_host_key_checking: STRICT_HOST_KEY_CHOICES[self.strict_host_key],
            proxy_jump: parse_proxy_jump(&self.proxy_jump)?,
            forward_agent: self.forward_agent.as_bool(),
            server_alive_interval: parse_u16_field(
                &self.server_alive_interval,
                "--server-alive-interval",
            )?,
            server_alive_count_max: parse_u16_field(
                &self.server_alive_count_max,
                "--server-alive-count-max",
            )?,
            control_persist: nonempty(self.control_persist.trim()),
            remote_command: nonempty(self.remote_command.trim()),
            session_log,
        })
    }

    /// 粘贴 / 宿主插入文本到聚焦字段。快速输入先压平续行符，粘贴即解析。
    pub(super) fn insert_text(&mut self, text: &str) -> bool {
        if self.running() || self.prompt.is_some() {
            return false;
        }
        let Some(field) = self.focused_field() else {
            return false;
        };
        if field == MachineField::Quick {
            let flat = flatten_pasted_command(text);
            if !self.quick.insert(&flat) {
                return false;
            }
            self.mark_edited(field);
            self.apply_quick();
            return true;
        }
        let Some(editor) = self.editor_mut(field) else {
            return false;
        };
        if !editor.insert(text) {
            return false;
        }
        self.mark_edited(field);
        true
    }

    /// 快速输入聚焦且有没解析的改动：此时 Enter 是「填入」而不是保存。
    pub(in crate::client::shell) fn quick_pending(&self) -> bool {
        self.quick_dirty && self.focused_field() == Some(MachineField::Quick)
    }

    /// 滚轮滚动字段栏（行）：不动焦点，也不再把焦点拉回窗口。
    pub(super) fn scroll_rows(&mut self, delta: isize, max_scroll: usize) {
        self.scroll = self.scroll.saturating_add_signed(delta).min(max_scroll);
        self.reveal = false;
    }
}

impl ClientShellState {
    fn machine_form_mut(&mut self) -> Option<&mut ClientMachineForm> {
        match self.overlay.as_mut() {
            Some(ClientShellOverlay::Machines(overlay)) => match &mut overlay.view {
                ClientMachinesView::Form(form) => Some(form),
                _ => None,
            },
            _ => None,
        }
    }

    /// 保存（Enter / 保存按钮）：逐字段校验通过后写目录。新机器直接落盘，
    /// 回列表并选中它，由目录 watcher 建立连接；编辑回详情。
    pub(super) fn save_machine_form(&mut self) {
        let prepared = {
            let saved = &self.saved_profiles;
            let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() else {
                return;
            };
            let ClientMachinesView::Form(form) = &mut overlay.view else {
                return;
            };
            if form.running() || form.prompt.is_some() || !form.submit_gate(saved) {
                return;
            }
            match form.profile_options() {
                Ok(options) => (
                    form.editing.clone(),
                    options,
                    form.effective_label(),
                    form.target.trim().to_owned(),
                    form.effective_session(),
                ),
                Err(error) => {
                    form.error = Some(error);
                    return;
                }
            }
        };
        let (editing, options, label, target, session) = prepared;
        let result = match &editing {
            Some(profile_id) => self
                .mutate_machine_catalog(|catalog| {
                    catalog.update_ssh(profile_id, label.clone(), session, options)
                })
                .map(|updated| updated.then(|| profile_id.clone())),
            None => self
                .persist_new_machine(options, &label, &target, &session)
                .map(Some),
        };
        let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() else {
            return;
        };
        match (result, editing.is_some()) {
            (Ok(Some(profile_id)), true) => {
                overlay.view = ClientMachinesView::Detail(profile_id);
            }
            (Ok(Some(profile_id)), false) => {
                overlay.view = ClientMachinesView::List;
                overlay.set_message(crate::i18n::fill(
                    crate::i18n::texts().machine_form.saved_fmt,
                    &[("label", &label)],
                ));
                self.select_machine_row(&profile_id);
            }
            // 编辑期间档案被外部删掉：回列表。
            (Ok(None), _) => overlay.view = ClientMachinesView::List,
            (Err(error), _) => {
                if let ClientMachinesView::Form(form) = &mut overlay.view {
                    form.error = Some(error);
                }
            }
        }
    }

    /// 测试连接（Ctrl+T / 测试按钮）的两段式入口。bootstrap 以预先授权模式
    /// 运行、不会再问，所以第一次只过字段校验并在页脚上方写明后果（必要时
    /// 安装 / 更新，更新要求时停止远端 server 及其 pane 进程）；确认条上
    /// 再按一次（Enter / Ctrl+T / 点「开始测试」）才真正下发。宽窄屏都画
    /// 这条说明（窄屏没有预览栏）。
    pub(super) fn request_machine_test(&mut self, outcome: &mut ClientShellInput) {
        let confirmed = {
            let saved = &self.saved_profiles;
            let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() else {
                return;
            };
            let ClientMachinesView::Form(form) = &mut overlay.view else {
                return;
            };
            if !form.can_test() || form.running() {
                return;
            }
            match form.prompt {
                Some(FormPrompt::ConfirmTest) => true,
                None => {
                    if form.submit_gate(saved) {
                        form.prompt = Some(FormPrompt::ConfirmTest);
                    }
                    false
                }
            }
        };
        if confirmed {
            self.start_machine_test(outcome);
        }
        outcome.repaint = true;
    }

    /// 确认条上的「开始测试」：与保存同一道校验，再按当前字段在内存目录里
    /// 解析出 SSH 选项，跑一遍预先授权的 bootstrap（检测平台、必要时安装 /
    /// 更新并启动 server、验证）。只测不存。
    fn start_machine_test(&mut self, outcome: &mut ClientShellInput) {
        let ticket = self.next_machine_bootstrap_ticket;
        let saved = &self.saved_profiles;
        let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() else {
            return;
        };
        let ClientMachinesView::Form(form) = &mut overlay.view else {
            return;
        };
        if form.prompt != Some(FormPrompt::ConfirmTest) {
            return;
        }
        form.prompt = None;
        if !form.can_test() || form.running() || !form.submit_gate(saved) {
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
        // 与 CLI 同一套选项解析：在临时目录里加一次（ProxyJump 的 profile:
        // 引用要靠目录解析），不落盘。
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
        form.error = None;
        let cancel = crate::remote::TaskCancellation::default();
        form.bootstrap = Some(ClientMachineBootstrap {
            cancel: cancel.clone(),
            ticket,
            step: None,
            failure: None,
            passed: false,
        });
        self.next_machine_bootstrap_ticket = ticket.saturating_add(1);
        outcome.actions.push(ClientShellAction::BootstrapMachine {
            cancel,
            ticket,
            target,
            session,
            options: ssh_options,
        });
        outcome.repaint = true;
    }

    /// 测试连接的进度与结论。只更新表单里的测试状态，不落盘。
    pub(crate) fn handle_machine_bootstrap_update(
        &mut self,
        ticket: u64,
        update: MachineBootstrapUpdate,
    ) {
        let Some(ClientShellOverlay::Machines(overlay)) = self.content_page_mut() else {
            return;
        };
        let ClientMachinesView::Form(form) = &mut overlay.view else {
            return;
        };
        let Some(bootstrap) = form
            .bootstrap
            .as_mut()
            .filter(|bootstrap| bootstrap.ticket == ticket && bootstrap.running())
        else {
            return;
        };
        match update {
            MachineBootstrapUpdate::Step(step) => bootstrap.step = Some(step),
            MachineBootstrapUpdate::Finished(Ok(())) => bootstrap.passed = true,
            MachineBootstrapUpdate::Finished(Err(error)) => bootstrap.failure = Some(error),
        }
    }

    /// 测试失败后的恢复入口（Ctrl+R / 恢复按钮）：按失败类型打开 `MachineAuth`
    /// 二级浮层；`route` 为 `None` 时取分类给出的入口。临时档案不落盘，交互
    /// 认证成功时由 `complete_interactive_connection` 保存并接管连接。
    pub(super) fn open_machine_test_recovery(
        &mut self,
        route: Option<TestRecovery>,
        outcome: &mut ClientShellInput,
    ) {
        let prepared = match self.overlay.as_ref() {
            Some(ClientShellOverlay::Machines(overlay)) => match &overlay.view {
                // 编辑态没有测试，也就没有恢复入口（见 `can_test`）。
                ClientMachinesView::Form(form) if form.can_test() => form
                    .bootstrap
                    .as_ref()
                    .filter(|bootstrap| bootstrap.failure.is_some())
                    .and_then(|bootstrap| route.or_else(|| bootstrap.recovery()))
                    .map(|route| (route, Self::wizard_temp_profile(form))),
                _ => None,
            },
            _ => None,
        };
        match prepared {
            Some((TestRecovery::HostKey, Ok(profile))) => {
                self.open_machine_host_key_review(Box::new(profile), outcome)
            }
            Some((TestRecovery::HostKeyChanged, Ok(profile))) => {
                self.open_machine_host_key_changed_review(Box::new(profile), outcome)
            }
            Some((TestRecovery::Auth, Ok(profile))) => {
                self.open_machine_auth_guide(Box::new(profile), outcome)
            }
            Some((_, Err(error))) => {
                if let Some(form) = self.machine_form_mut() {
                    form.error = Some(error);
                }
            }
            None => {}
        }
    }

    /// 快速输入的「填入」（快速输入框里按 Enter / 点页脚）。
    pub(super) fn apply_machine_quick_input(&mut self) {
        if let Some(form) = self.machine_form_mut() {
            if !form.running() && form.prompt.is_none() {
                form.apply_quick();
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
        self.persist_new_machine(options, &profile.label, &profile.target, &profile.session)
    }

    fn persist_new_machine(
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
        let ctrl = modifiers == KeyModifiers::CONTROL;
        let running = self.machine_form_mut().is_some_and(|form| form.running());
        if code == KeyCode::Esc {
            // 运行中的测试：Esc 只取消测试；否则离开表单。
            self.machines_back();
            outcome.repaint = true;
            return;
        }
        if running {
            // 测试在后台跑：表单只读，直到通过或失败。
            return;
        }
        if let Some(prompt) = self.machine_form_mut().and_then(|form| form.prompt) {
            // 确认条独占 Enter（测试确认上 Ctrl+T 同义）；其余键不动表单。
            match prompt {
                FormPrompt::ConfirmTest
                    if code == KeyCode::Enter || (code == KeyCode::Char('t') && ctrl) =>
                {
                    self.request_machine_test(outcome);
                }
                FormPrompt::ConfirmTest => {}
            }
            return;
        }
        match code {
            KeyCode::Char('t') if ctrl => {
                self.request_machine_test(outcome);
                outcome.repaint = true;
                return;
            }
            KeyCode::Char('r') if ctrl => {
                self.open_machine_test_recovery(None, outcome);
                outcome.repaint = true;
                return;
            }
            KeyCode::Enter => {
                // 快速输入里有没解析的改动：Enter 先填入；否则 Enter 就是保存
                // （粘贴即已解析，粘完直接 Enter 保存）。
                if self
                    .machine_form_mut()
                    .is_some_and(|form| form.quick_pending())
                {
                    self.apply_machine_quick_input();
                } else {
                    self.save_machine_form();
                }
                outcome.repaint = true;
                return;
            }
            _ => {}
        }
        let Some(form) = self.machine_form_mut() else {
            return;
        };
        match code {
            KeyCode::Tab if plain => form.move_focus(1, true),
            KeyCode::BackTab if modifiers.difference(KeyModifiers::SHIFT).is_empty() => {
                form.move_focus(-1, true)
            }
            KeyCode::Up if plain => form.move_focus(-1, false),
            KeyCode::Down if plain => form.move_focus(1, false),
            KeyCode::PageUp if plain => form.move_focus(-5, false),
            KeyCode::PageDown if plain => form.move_focus(5, false),
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
                let Some(editor) = form.editor_mut(field) else {
                    return;
                };
                match editor.handle_key(key) {
                    Some(true) => form.mark_edited(field),
                    Some(false) => form.reveal = true,
                    None => return,
                }
            }
        }
        outcome.repaint = true;
    }

    /// Builds the throwaway profile a test-connection recovery runs against
    /// (never persisted by the form itself).
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

    /// 鼠标点字段：聚焦（失焦校验照常）；点在输入行上按字符换算光标列；
    /// 再点已聚焦的选择字段切到下一个取值。`rect` 是该字段的命中区（首行是
    /// 标签，第二行是输入框）。
    pub(super) fn click_machine_form_field(
        &mut self,
        field: MachineField,
        rect: Rect,
        point: (u16, u16),
    ) {
        let Some(form) = self.machine_form_mut() else {
            return;
        };
        if form.running() || form.prompt.is_some() {
            return;
        }
        let Some(index) = form
            .fields()
            .iter()
            .position(|candidate| *candidate == field)
        else {
            return;
        };
        let was_focused = form.focused == index;
        form.set_focus(index);
        if field.is_choice() {
            if was_focused {
                form.cycle_choice(field, 1);
            }
            return;
        }
        let input_row = rect.y.saturating_add(1);
        if point.1 != input_row || point.0 < rect.x {
            return;
        }
        let Some(editor) = form.editor_mut(field) else {
            return;
        };
        // 聚焦之前这一帧 kit 是从头画的；已聚焦时按当时的光标滚动。
        let cursor = was_focused.then(|| editor.cursor_char_index());
        let column = point.0 - rect.x;
        let index =
            super::super::form::char_index_at_column(editor.as_str(), cursor, rect.width, column);
        editor.set_cursor_char_index(index);
    }
}
