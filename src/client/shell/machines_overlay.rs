//! Machines management overlay: list, detail, add wizard, and edit form for
//! saved SSH endpoints. Machine CRUD works on the client-local catalog file;
//! the catalog watcher reconciles live connections afterwards, so no private
//! socket behavior is added here.
//!
//! 根模块只做视图分派、键路由与状态投影；各视图的状态机与渲染按
//! HERDR-MACH-021 拆在子模块里（`form` / `forwards` / `import` /
//! `list_detail`，宽屏工作台在 `dashboard`，鼠标分派在 `mouse`）。

use super::*;
mod dashboard;
mod footer;
mod form;
mod form_view;
mod forwards;
mod import;
mod list_detail;
mod mouse;
mod quick;
use crate::client::endpoint::{
    EndpointCatalog, PortForwardKind, PortForwardRule, ProfileId, ProxyJumpHop, SessionLogProfile,
    SshProfileOptions, StrictHostKeyChecking,
};
use crate::remote::SavedSshBootstrapStep;
use crossterm::event::KeyModifiers;
#[cfg(test)]
pub(super) use form::{ClientMachineBootstrap, TriChoice};
pub(super) use form::{ClientMachineForm, MachineField};
pub(super) use form_view::bootstrap_step_label;
use form_view::*;
use forwards::*;
#[cfg(test)]
pub(super) use forwards::{ClientForwardRuleForm, ClientForwardRulesView, PendingForwardRemoval};
#[cfg(test)]
pub(super) use import::ClientImportStep;
use import::*;
pub(super) use list_detail::machine_hash_color;
use list_detail::*;

use super::render::{
    display_width, modal_panel, put_right_text, put_text, render_search_bar, OverlayRender,
    SearchBar,
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

    /// 表单里的测试连接正在跑（尚未通过或失败）：驱动 spinner。
    pub(super) fn bootstrap_running(&self) -> bool {
        matches!(&self.view, ClientMachinesView::Form(form) if form.running())
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
    Save,
    /// 表单：测试连接（不落盘）。
    TestConnection,
    /// 表单：测试失败后按失败类型打开 host key / 认证二级浮层。
    TestRecover,
    /// 表单：解析快速输入并填入字段。
    QuickApply,
    Edit,
    Reconnect,
    ToggleEnabled,
    Remove,
    CopyFix,
    ConfirmRemove,
    CancelRemove,
    ReviewIssue,
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
            CancelTest,
            DismissPrompt,
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
                    if form.running() {
                        Back::CancelTest
                    } else if form.prompt.is_some() {
                        // 确认条上的 Esc 只收起确认条，表单原样留着。
                        Back::DismissPrompt
                    } else {
                        match &form.editing {
                            Some(id) if self.saved_profile(id).is_some() => {
                                Back::Detail(id.clone())
                            }
                            _ => Back::List,
                        }
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
            Back::CancelTest => {
                if let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() {
                    if let ClientMachinesView::Form(form) = &mut overlay.view {
                        // 丢弃即取消（`Drop` 触发 cancel），表单回到可编辑。
                        form.bootstrap = None;
                    }
                }
            }
            Back::DismissPrompt => {
                if let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() {
                    if let ClientMachinesView::Form(form) = &mut overlay.view {
                        form.prompt = None;
                    }
                }
            }
        }
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

    pub(super) fn insert_machines_overlay_text(&mut self, text: &str) -> bool {
        let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() else {
            return false;
        };
        match &mut overlay.view {
            ClientMachinesView::Form(form) => form.insert_text(text),
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
                matches!(&overlay.view, ClientMachinesView::Form(form) if form.running()),
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
            Btn::Save => self.save_machine_form(),
            Btn::TestConnection => self.request_machine_test(outcome),
            Btn::TestRecover => self.open_machine_test_recovery(None, outcome),
            Btn::QuickApply => self.apply_machine_quick_input(),
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
        }
        outcome.repaint = true;
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
                        form.scroll_rows(delta, max);
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
            let reconnect_enabled = rows.get(selected).is_none_or(|row| row.enabled);
            let hints = machine_list_hints(!rows.is_empty(), has_review, reconnect_enabled);
            let stack = crate::ui::modal_stack_areas(
                inner,
                2,
                machine_footer_rows(&hints, inner.width),
                0,
                1,
            );
            Some(MachinesBody::List(stack.content))
        }
        ClientMachinesView::Detail(id) => {
            let (_, inner) = machines_panel(area, page_bounds, 26)?;
            let profile = saved_profiles.iter().find(|profile| &profile.id == id)?;
            // 与渲染同一口径：只看这台机器的失败类型有没有界面内恢复路径。
            let has_review = connection_errors
                .get(&ClientEndpointId::Ssh(id.clone()))
                .is_some_and(super::machine_auth_overlay::failure_kind_has_review);
            let hints = machine_detail_hints(has_review, profile.enabled);
            let stack = super::page::PageLayout::with_footer_rows(
                inner,
                0,
                false,
                0,
                machine_footer_rows(&hints, inner.width),
            );
            Some(MachinesBody::Detail(stack.content))
        }
        ClientMachinesView::Forwards(_) => {
            let (_, inner) = machines_panel(area, page_bounds, 20)?;
            Some(MachinesBody::Forwards(forwards_stack(inner).content))
        }
        ClientMachinesView::Import(view) => {
            let (_, inner) = machines_panel(area, page_bounds, 24)?;
            let stack = import_stack(inner, view);
            match view.step {
                ClientImportStep::Discover => None,
                ClientImportStep::Select => {
                    // 候选列表扣掉表头与固定行（通配符开关 / 分组输入 / 计数行），
                    // 宽屏还要让出右侧预览栏：与渲染同一个版面函数。
                    let list = import_select_layout(stack.content).list;
                    Some(MachinesBody::ImportSelect(
                        list,
                        usize::from(list.height.max(1)),
                    ))
                }
                ClientImportStep::Done => Some(MachinesBody::ImportDone(stack.content)),
            }
        }
        ClientMachinesView::Form(form) => {
            let (_, inner) = form_panel(area, page_bounds)?;
            Some(MachinesBody::Form(form_layout(inner, form).fields))
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
        self.compute_machines_view_with(
            area,
            page_bounds,
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
    fn compute_machines_view_with(
        &mut self,
        area: Rect,
        page_bounds: Option<Rect>,
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
                page.view_max_scroll = reveal_focused_field(form, rect, saved_profiles);
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
                // 与渲染同口径：每条失败结果下面还有一行修复提示。
                let failed_hints = view
                    .results
                    .iter()
                    .filter(|row| row.outcome == ClientImportOutcome::Failed)
                    .count();
                let total_lines = view.results.len() + failed_hints + usize::from(imported > 0);
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
        ClientMachinesView::Form(form) => render_machine_form(b, form, saved_profiles, cx),
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
