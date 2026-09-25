//! 右键菜单：条目按对象从动作表取（[`super::action_table::context_layout`]），
//! 渲染走 `kit::menu`（分隔线、右对齐快捷键、禁用置灰、勾选态、危险项、子菜单、
//! 悬浮与键盘高亮分离），键盘支持方向键、Home / End、首字母跳转与左右键进出
//! 子菜单。
//!
//! 行 id：`highlighted` / `hovered` 与 `hits.context_menu_rows` 的下标都是
//! [`ClientContextMenuOverlay::items`] 的平铺下标；子菜单的父项不是可执行条目，
//! 用 [`SUBMENU_ROW`] 表示。子菜单的子项也在平铺条目里，归属由
//! [`ContextMenuModel::submenu`] 给出。

use std::ops::Range;

use super::action_table::{
    action_spec, context_action, context_action_state, context_layout, ActionTarget, ContextKind,
    ContextLayoutEntry, ACTIONS,
};
use super::feedback::ChromeContext;
use super::render::OverlayRender;
use super::*;
use crate::ui::kit::menu::{
    menu_first_letter, menu_size, menu_step, render_menu, MenuItem, MenuState,
};
use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

/// 子菜单父项在行 id 空间里的标记。
pub(super) const SUBMENU_ROW: usize = usize::MAX;

/// 打开菜单时从客户端状态取的、渲染阶段拿不到的事实：用户当前键位下各条目
/// 的快捷键标签、右键的机器是不是当前活动机器。挂在右键对象上随菜单走；
/// `state.rs` 只持有它，字段只在本车道的文件里读写。
#[derive(Debug, Clone, Default)]
pub(super) struct ContextMenuEnv {
    shortcuts: Vec<(ClientContextMenuAction, String)>,
    active_machine: bool,
    selection: Option<ContextSelection>,
}

impl ContextMenuEnv {
    fn resolve(kind: ContextKind, keybinds: &crate::config::Keybinds) -> Self {
        Self {
            shortcuts: ACTIONS
                .iter()
                .filter_map(|spec| {
                    let (spec_kind, action) = spec.context?;
                    if spec_kind != kind {
                        return None;
                    }
                    spec.shortcut(keybinds).map(|label| (action, label))
                })
                .collect(),
            active_machine: false,
            selection: None,
        }
    }

    pub(super) fn can_copy_selection(&self) -> bool {
        self.selection.is_some()
    }

    fn shortcut(&self, action: ClientContextMenuAction) -> Option<&str> {
        self.shortcuts
            .iter()
            .find(|(candidate, _)| *candidate == action)
            .map(|(_, label)| label.as_str())
    }

    /// 右键的机器是当前活动机器（「切换到此机器」置灰）。
    pub(super) fn active_machine(&self) -> bool {
        self.active_machine
    }
}

/// 菜单只复制打开时的选区；端点重连或选区替换后旧条目不可执行。
#[derive(Debug, Clone, PartialEq, Eq)]
struct ContextSelection {
    endpoint: ClientEndpointId,
    boot: String,
    generation: Option<u64>,
    epoch: u64,
    cells: ((u32, u16), (u32, u16)),
}

/// 子菜单：父项标签与子项在平铺条目里的范围。
pub(super) struct ContextSubmenuSpec {
    pub(super) label: &'static str,
    pub(super) children: Range<usize>,
    pub(super) separator_before: bool,
}

/// 菜单版式：平铺条目（顶层与子菜单子项，按显示顺序）+ 至多一个子菜单。
pub(super) struct ContextMenuModel {
    pub(super) items: Vec<ClientContextMenuItem>,
    pub(super) submenu: Option<ContextSubmenuSpec>,
}

impl ContextMenuModel {
    /// 顶层的行：`(行 id, 分隔线在前)`，子菜单的子项收进父项。
    fn top_rows(&self) -> Vec<(usize, bool)> {
        let mut rows = Vec::with_capacity(self.items.len() + 1);
        for (index, item) in self.items.iter().enumerate() {
            match &self.submenu {
                Some(spec) if spec.children.contains(&index) => {
                    if index == spec.children.start {
                        rows.push((SUBMENU_ROW, spec.separator_before));
                    }
                }
                _ => rows.push((index, item.separator_before)),
            }
        }
        rows
    }

    fn is_child(&self, row: usize) -> bool {
        self.submenu
            .as_ref()
            .is_some_and(|spec| spec.children.contains(&row))
    }
}

impl ClientContextMenuOverlay {
    /// 平铺条目（顶层与子菜单的子项）：激活、测试与渲染都按这个下标说话。
    pub(super) fn items(&self) -> Vec<ClientContextMenuItem> {
        self.model().items
    }

    pub(super) fn model(&self) -> ContextMenuModel {
        use ClientContextMenuAction as Action;

        let item = |label, action| ClientContextMenuItem {
            label,
            action,
            enabled: true,
            shortcut: None,
            checked: None,
            separator_before: false,
        };
        let items = match &self.target {
            // agent 行的条目由接缝定稿：面板车道只接动作，菜单车道只重写渲染与
            // 键盘导航，都不改条目语义。
            ClientContextMenuTarget::Agent {
                agent,
                has_activity,
                renamable,
                ..
            } => {
                let t = &crate::i18n::texts().agent_panel;
                let mut items = vec![
                    item(t.menu_focus, Action::FocusAgent),
                    ClientContextMenuItem {
                        enabled: *has_activity,
                        ..item(t.menu_view_activity, Action::ViewAgentActivity)
                    },
                    // 其它端点离线（或没宣告 `pane.rename`）时重命名发不出去：置灰
                    // （文档终审 D9）。
                    ClientContextMenuItem {
                        enabled: *renamable,
                        ..item(t.menu_rename, Action::RenameAgent)
                    },
                ];
                // 只有 herdr 能跟踪账号用量的 agent 才列这两项（正向判据，与
                // 动作处理同一处）：muse 等已退役的 agent 与未知名没有用量可看、
                // 也不能绑定，列出来点了没反应（文档终审 D13、T1 审查轻 1）。
                if agent
                    .as_deref()
                    .is_some_and(super::observability::is_bindable_agent)
                {
                    // 「用量」打开并钉住该 agent 的用量悬停卡
                    // （`ClientShellState::pin_agent_usage_card`）。
                    items.push(item(t.menu_usage, Action::ShowAgentUsage));
                    items.push(item(t.menu_bind_account, Action::BindAgentAccount));
                }
                items.push(item(t.menu_close, Action::CloseAgentPane));
                items
            }
            ClientContextMenuTarget::ExternalAgent { .. } => vec![item(
                crate::i18n::texts().agent_panel.menu_view_activity,
                Action::ViewAgentActivity,
            )],
            target => return layout_model(target),
        };
        ContextMenuModel {
            items,
            submenu: None,
        }
    }
}

fn context_kind(target: &ClientContextMenuTarget) -> ContextKind {
    match target {
        ClientContextMenuTarget::Workspace { .. } => ContextKind::Workspace,
        ClientContextMenuTarget::Tab { .. } => ContextKind::Tab,
        ClientContextMenuTarget::Pane { .. } => ContextKind::Pane,
        ClientContextMenuTarget::Machine { .. } => ContextKind::Machine,
        ClientContextMenuTarget::Agent { .. } | ClientContextMenuTarget::ExternalAgent { .. } => {
            ContextKind::Agent
        }
    }
}

fn target_env(target: &ClientContextMenuTarget) -> Option<&ContextMenuEnv> {
    match target {
        ClientContextMenuTarget::Workspace { env, .. }
        | ClientContextMenuTarget::Tab { env, .. }
        | ClientContextMenuTarget::Pane { env, .. }
        | ClientContextMenuTarget::Machine { env, .. } => Some(env),
        ClientContextMenuTarget::Agent { .. } | ClientContextMenuTarget::ExternalAgent { .. } => {
            None
        }
    }
}

/// 按动作表版式展开某个对象的菜单：不适用的条目不列，分隔线只画在两段都有
/// 条目的地方。
fn layout_model(target: &ClientContextMenuTarget) -> ContextMenuModel {
    let texts = crate::i18n::texts();
    let kind = context_kind(target);
    let env = target_env(target);
    let make = |id, separator_before| {
        let spec = action_spec(id);
        let (_, action) = spec.context?;
        let state = context_action_state(id, target);
        state.visible.then(|| ClientContextMenuItem {
            label: spec.label_text(texts, state.alternate),
            action,
            enabled: state.enabled,
            shortcut: env.and_then(|env| env.shortcut(action)).map(str::to_owned),
            checked: state.checked,
            separator_before,
        })
    };
    let mut items = Vec::new();
    let mut submenu = None;
    let mut separator = false;
    for entry in context_layout(kind) {
        match entry {
            ContextLayoutEntry::Separator => separator = true,
            ContextLayoutEntry::Item(id) => {
                if let Some(item) = make(*id, separator && !items.is_empty()) {
                    items.push(item);
                    separator = false;
                }
            }
            ContextLayoutEntry::Submenu(label, children) => {
                let start = items.len();
                let separator_before = separator && start > 0;
                items.extend(children.iter().filter_map(|id| make(*id, false)));
                if items.len() > start {
                    submenu = Some(ContextSubmenuSpec {
                        label: label(texts),
                        children: start..items.len(),
                        separator_before,
                    });
                    separator = false;
                }
            }
        }
    }
    ContextMenuModel { items, submenu }
}

/// 条目是否标红：动作表里的破坏性动作。
fn is_danger(kind: ContextKind, action: ClientContextMenuAction) -> bool {
    context_action(kind, action).is_some_and(|id| action_spec(id).danger)
}

fn kit_item(item: &ClientContextMenuItem, kind: ContextKind) -> MenuItem<'_> {
    MenuItem {
        shortcut: item.shortcut.as_deref(),
        enabled: item.enabled,
        checked: item.checked,
        danger: is_danger(kind, item.action),
        ..MenuItem::action(item.label)
    }
}

/// 顶层的 kit 条目与每条对应的行 id（分隔线为 `None`）。
fn top_menu(
    model: &ContextMenuModel,
    kind: ContextKind,
) -> (Vec<MenuItem<'_>>, Vec<Option<usize>>) {
    let rows = model.top_rows();
    let mut items = Vec::with_capacity(rows.len() * 2);
    let mut ids = Vec::with_capacity(rows.len() * 2);
    for (row, separator_before) in rows {
        if separator_before && !items.is_empty() {
            items.push(MenuItem::separator());
            ids.push(None);
        }
        let item = match (row, &model.submenu) {
            (SUBMENU_ROW, Some(spec)) => MenuItem::submenu(spec.label),
            _ => match model.items.get(row) {
                Some(item) => kit_item(item, kind),
                None => continue,
            },
        };
        items.push(item);
        ids.push(Some(row));
    }
    (items, ids)
}

/// 子菜单的 kit 条目（下标 + `children.start` = 平铺下标）。
fn sub_menu(model: &ContextMenuModel, kind: ContextKind) -> Vec<MenuItem<'_>> {
    model
        .submenu
        .as_ref()
        .map(|spec| {
            model.items[spec.children.clone()]
                .iter()
                .map(|item| kit_item(item, kind))
                .collect()
        })
        .unwrap_or_default()
}

fn kit_index(ids: &[Option<usize>], row: usize) -> usize {
    ids.iter()
        .position(|id| *id == Some(row))
        .unwrap_or(usize::MAX)
}

fn child_kit_index(model: &ContextMenuModel, row: usize) -> usize {
    match &model.submenu {
        Some(spec) if spec.children.contains(&row) => row - spec.children.start,
        _ => usize::MAX,
    }
}

/// 打开子菜单。`keyboard` 为真时键盘焦点进子菜单、高亮第一个可用子项；指针
/// 悬浮打开时焦点留在顶层。
fn open_submenu(menu: &mut ClientContextMenuOverlay, model: &ContextMenuModel, keyboard: bool) {
    let Some(spec) = model.submenu.as_ref() else {
        return;
    };
    let (_, ids) = top_menu(model, context_kind(&menu.target));
    let highlighted = if keyboard {
        spec.children
            .clone()
            .find(|row| model.items[*row].enabled)
            .unwrap_or(usize::MAX)
    } else {
        usize::MAX
    };
    menu.submenu = Some(ClientContextSubmenu {
        parent: kit_index(&ids, SUBMENU_ROW),
        highlighted,
        hovered: None,
    });
}

impl ClientShellState {
    fn open_context_menu(&mut self, target: ClientContextMenuTarget, x: u16, y: u16) {
        let mut menu = ClientContextMenuOverlay {
            target,
            x,
            y,
            highlighted: 0,
            hovered: None,
            submenu: None,
        };
        let model = menu.model();
        menu.highlighted = model
            .top_rows()
            .into_iter()
            .map(|(row, _)| row)
            .find(|row| *row == SUBMENU_ROW || model.items[*row].enabled)
            .unwrap_or(usize::MAX);
        self.overlay = Some(ClientShellOverlay::ContextMenu(menu));
    }

    fn context_env(&self, kind: ContextKind) -> ContextMenuEnv {
        ContextMenuEnv::resolve(kind, &self.config.keybinds.keybinds)
    }

    pub(super) fn open_workspace_context_menu(&mut self, workspace_id: String, x: u16, y: u16) {
        let Some(snapshot) = self.snapshot.as_deref() else {
            return;
        };
        let Some(workspace) = snapshot
            .workspaces
            .iter()
            .find(|workspace| workspace.workspace_id == workspace_id)
        else {
            return;
        };
        let worktree = workspace.worktree.as_ref();
        let has_worktree_children = worktree.is_some_and(|worktree| {
            !worktree.is_linked_worktree
                && snapshot
                    .workspaces
                    .iter()
                    .filter(|candidate| {
                        candidate
                            .worktree
                            .as_ref()
                            .is_some_and(|candidate| candidate.key == worktree.key)
                    })
                    .count()
                    >= 2
        });
        let collapsed = worktree.is_some_and(|worktree| {
            self.group_is_collapsed(&self.active_endpoint_id, &worktree.key)
        });
        let target = ClientContextMenuTarget::Workspace {
            workspace_id,
            is_git: worktree.is_some() || workspace.branch.is_some(),
            is_linked_worktree: worktree.is_some_and(|worktree| worktree.is_linked_worktree),
            has_worktree_children,
            collapsed,
            env: self.context_env(ContextKind::Workspace),
        };
        self.open_context_menu(target, x, y);
    }

    pub(super) fn open_tab_context_menu(&mut self, tab_id: String, x: u16, y: u16) {
        let Some(tab) = self
            .snapshot
            .as_deref()
            .and_then(|snapshot| snapshot.tabs.iter().find(|tab| tab.tab_id == tab_id))
        else {
            return;
        };
        let target = ClientContextMenuTarget::Tab {
            tab_id,
            workspace_id: tab.workspace_id.clone(),
            env: self.context_env(ContextKind::Tab),
        };
        self.open_context_menu(target, x, y);
    }

    fn menu_selection(&self, pane_id: &str) -> Option<ContextSelection> {
        if !self.has_copyable_pane_selection(pane_id) {
            return None;
        }
        Some(ContextSelection {
            endpoint: self.active_endpoint_id.clone(),
            boot: self.snapshot.as_ref()?.boot_id.clone(),
            generation: self.active_snapshot_generation,
            epoch: self.selection_epoch,
            cells: self.copyable_pane_selection_cells(pane_id)?,
        })
    }

    /// 同一次复制应答只替换选区存储；已经打开的菜单继续指向同一份文本。
    pub(super) fn refresh_copied_selection_menu(&mut self, pane_id: &str, previous_epoch: u64) {
        let Some(selection) = self.menu_selection(pane_id) else {
            return;
        };
        let Some(ClientShellOverlay::ContextMenu(menu)) = self.overlay.as_mut() else {
            return;
        };
        let ClientContextMenuTarget::Pane {
            pane_id: target,
            env,
            ..
        } = &mut menu.target
        else {
            return;
        };
        let mut previous = selection.clone();
        previous.epoch = previous_epoch;
        if target == pane_id && env.selection.as_ref() == Some(&previous) {
            env.selection = Some(selection);
        }
    }

    pub(super) fn context_menu_preserves_selection(&self) -> bool {
        let Some(ClientShellOverlay::ContextMenu(menu)) = &self.overlay else {
            return false;
        };
        self.menu_target_preserves_selection(&menu.target)
    }

    fn menu_target_preserves_selection(&self, target: &ClientContextMenuTarget) -> bool {
        let ClientContextMenuTarget::Pane { pane_id, env, .. } = target else {
            return false;
        };
        let Some(expected) = &env.selection else {
            return false;
        };
        self.has_copyable_pane_selection(pane_id)
            && expected.endpoint == self.active_endpoint_id
            && expected.generation == self.active_snapshot_generation
            && expected.epoch == self.selection_epoch
            && self
                .snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.boot_id == expected.boot)
            && self.copyable_pane_selection_cells(pane_id) == Some(expected.cells)
    }

    pub(super) fn open_pane_context_menu(&mut self, pane_id: String, x: u16, y: u16) {
        let Some(snapshot) = self.snapshot.as_deref() else {
            return;
        };
        let Some(pane) = snapshot.panes.iter().find(|pane| pane.pane_id == pane_id) else {
            return;
        };
        let source_pane_id = snapshot
            .focused_pane_id
            .clone()
            .filter(|focused| focused != &pane_id);
        let mut env = self.context_env(ContextKind::Pane);
        env.selection = self.menu_selection(&pane_id);
        let target = ClientContextMenuTarget::Pane {
            pane_id,
            workspace_id: pane.workspace_id.clone(),
            source_pane_id,
            has_manual_label: pane.label.is_some(),
            right_click_passthrough: pane.right_click_passthrough,
            env,
        };
        self.open_context_menu(target, x, y);
    }

    pub(super) fn open_machine_context_menu(
        &mut self,
        endpoint_id: &ClientEndpointId,
        x: u16,
        y: u16,
    ) {
        let (enabled, online) = match endpoint_id {
            ClientEndpointId::Local => (true, true),
            ClientEndpointId::Ssh(profile_id) => {
                let enabled = self
                    .saved_profiles
                    .iter()
                    .any(|profile| &profile.id == profile_id && profile.enabled);
                if !enabled
                    && !self
                        .saved_profiles
                        .iter()
                        .any(|profile| &profile.id == profile_id)
                {
                    return;
                }
                (enabled, self.endpoint_is_online(endpoint_id))
            }
        };
        let target = ClientContextMenuTarget::Machine {
            endpoint_id: endpoint_id.clone(),
            enabled,
            online,
            env: ContextMenuEnv {
                active_machine: &self.active_endpoint_id == endpoint_id,
                ..self.context_env(ContextKind::Machine)
            },
        };
        self.open_context_menu(target, x, y);
    }

    /// 右键菜单打开时的按键：方向键在当前层（子菜单有键盘焦点时是子菜单）里
    /// 跳过分隔线与禁用项回绕移动，Home / End 到首尾，可打印字符按首字母跳转，
    /// → / Enter 进子菜单，← / Esc 退出子菜单，再按 Esc 关闭菜单。不在该浮层时
    /// 返回 `false`。
    pub(super) fn route_context_menu_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) -> bool {
        let Some(ClientShellOverlay::ContextMenu(menu)) = self.overlay.as_mut() else {
            return false;
        };
        let model = menu.model();
        let kind = context_kind(&menu.target);
        let (top, ids) = top_menu(&model, kind);
        let children = sub_menu(&model, kind);
        let child_focus = menu
            .submenu
            .as_ref()
            .filter(|open| open.highlighted != usize::MAX)
            .map(|open| open.highlighted);
        // 当前层的条目与键盘高亮所在的 kit 下标。
        let (level, from) = match child_focus {
            Some(row) => (&children, child_kit_index(&model, row)),
            None => (&top, kit_index(&ids, menu.highlighted)),
        };
        let step = match key.code {
            KeyCode::Up => menu_step(level, from, -1),
            KeyCode::Down => menu_step(level, from, 1),
            KeyCode::Home => menu_step(level, usize::MAX, 1),
            KeyCode::End => menu_step(level, usize::MAX, -1),
            KeyCode::Char(ch)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                menu_first_letter(level, ch, from)
            }
            _ => None,
        };
        let mut activate = None;
        let mut close = false;
        match key.code {
            KeyCode::Up | KeyCode::Down | KeyCode::Home | KeyCode::End | KeyCode::Char(_) => {
                if let Some(index) = step {
                    match (child_focus, &model.submenu, menu.submenu.as_mut()) {
                        (Some(_), Some(spec), Some(open)) => {
                            open.highlighted = spec.children.start + index;
                        }
                        _ => {
                            if let Some(Some(row)) = ids.get(index) {
                                menu.highlighted = *row;
                                // 键盘离开了子菜单父项：悬浮打开的子菜单随之收起。
                                if *row != SUBMENU_ROW {
                                    menu.submenu = None;
                                }
                            }
                        }
                    }
                }
            }
            KeyCode::Right if child_focus.is_none() && menu.highlighted == SUBMENU_ROW => {
                open_submenu(menu, &model, true);
            }
            KeyCode::Left if menu.submenu.is_some() => menu.submenu = None,
            KeyCode::Esc if menu.submenu.is_some() => menu.submenu = None,
            KeyCode::Esc => close = true,
            KeyCode::Enter => match child_focus {
                Some(row) => activate = Some(row),
                None if menu.highlighted == SUBMENU_ROW => open_submenu(menu, &model, true),
                None => activate = Some(menu.highlighted),
            },
            _ => {}
        }
        if close {
            self.overlay = None;
        }
        if let Some(row) = activate {
            self.activate_context_menu_item(row, outcome);
        }
        outcome.repaint = true;
        true
    }

    pub(super) fn activate_context_menu_item(
        &mut self,
        index: usize,
        outcome: &mut ClientShellInput,
    ) {
        let Some(ClientShellOverlay::ContextMenu(menu)) = self.overlay.take() else {
            return;
        };
        let Some((action, enabled)) = menu
            .items()
            .get(index)
            .map(|item| (item.action, item.enabled))
        else {
            outcome.repaint = true;
            return;
        };
        if !enabled {
            // 禁用项不可激活：菜单保持打开，高亮不动。
            self.overlay = Some(ClientShellOverlay::ContextMenu(menu));
            return;
        }
        if action == ClientContextMenuAction::CopyPaneSelection
            && !self.menu_target_preserves_selection(&menu.target)
        {
            outcome.repaint = true;
            return;
        }
        let (kind, target) = context_action_target(menu.target);
        if let Some(id) = context_action(kind, action) {
            self.run_action(id, target, outcome);
        }
        outcome.repaint = true;
    }
}

/// 右键对象 → 动作表的对象种类与执行目标。
fn context_action_target(target: ClientContextMenuTarget) -> (ContextKind, ActionTarget) {
    match target {
        ClientContextMenuTarget::Workspace { workspace_id, .. } => (
            ContextKind::Workspace,
            ActionTarget::Workspace { workspace_id },
        ),
        ClientContextMenuTarget::Tab {
            tab_id,
            workspace_id,
            ..
        } => (
            ContextKind::Tab,
            ActionTarget::Tab {
                tab_id,
                workspace_id,
            },
        ),
        ClientContextMenuTarget::Pane {
            pane_id,
            workspace_id,
            source_pane_id,
            right_click_passthrough,
            ..
        } => (
            ContextKind::Pane,
            ActionTarget::Pane {
                pane_id,
                workspace_id,
                source_pane_id,
                right_click_passthrough,
            },
        ),
        ClientContextMenuTarget::Machine { endpoint_id, .. } => {
            (ContextKind::Machine, ActionTarget::Machine(endpoint_id))
        }
        ClientContextMenuTarget::Agent {
            endpoint_id,
            pane_id,
            ..
        } => (
            ContextKind::Agent,
            ActionTarget::Agent {
                endpoint_id,
                owner: super::agent_activity_overlay::AgentActivityOwner::Pane { pane_id },
            },
        ),
        ClientContextMenuTarget::ExternalAgent {
            endpoint_id,
            external_id,
        } => (
            ContextKind::Agent,
            ActionTarget::Agent {
                endpoint_id,
                owner: super::agent_activity_overlay::AgentActivityOwner::External { external_id },
            },
        ),
    }
}

impl ClientShellState {
    /// 右键菜单打开时的鼠标分派：不在该浮层时返回 `false`，由
    /// `mouse.rs::handle_mouse` 继续往下走。指针悬浮只写 `hovered`（悬到子菜单
    /// 父项上顺带展开子菜单，悬到别的顶层项上收起悬浮展开的子菜单）；点击可用
    /// 行激活，点在菜单内的禁用行 / 分隔线 / 边框上什么都不做，点在菜单外关闭。
    pub(super) fn handle_context_menu_mouse(
        &mut self,
        mouse: MouseEvent,
        point: (u16, u16),
        outcome: &mut ClientShellInput,
    ) -> bool {
        if !matches!(self.overlay, Some(ClientShellOverlay::ContextMenu(_))) {
            return false;
        }
        // 命中表按绘制顺序登记（顶层在前、子菜单在后），后画的压在上面：从后
        // 往前找。渲染阶段已把被子菜单盖住的顶层行裁掉，这里再兜一层底。
        let row_hit = self
            .hits
            .context_menu_rows
            .iter()
            .rev()
            .find(|(rect, _)| super::contains(*rect, point))
            .map(|(_, row)| *row);
        let inside = super::contains(self.hits.overlay_bounds, point);
        match mouse.kind {
            MouseEventKind::Moved => {
                if let Some(ClientShellOverlay::ContextMenu(menu)) = self.overlay.as_mut() {
                    let model = menu.model();
                    let before = (menu.hovered, menu.submenu.as_ref().map(|open| open.hovered));
                    match row_hit {
                        Some(row) if row != SUBMENU_ROW && model.is_child(row) => {
                            menu.hovered = None;
                            if let Some(open) = menu.submenu.as_mut() {
                                open.hovered = Some(row);
                            }
                        }
                        hovered => {
                            menu.hovered = hovered;
                            if let Some(open) = menu.submenu.as_mut() {
                                open.hovered = None;
                            }
                            match hovered {
                                Some(SUBMENU_ROW) if menu.submenu.is_none() => {
                                    open_submenu(menu, &model, false);
                                    outcome.repaint = true;
                                }
                                Some(_)
                                    if menu
                                        .submenu
                                        .as_ref()
                                        .is_some_and(|open| open.highlighted == usize::MAX) =>
                                {
                                    menu.submenu = None;
                                    outcome.repaint = true;
                                }
                                _ => {}
                            }
                        }
                    }
                    let after = (menu.hovered, menu.submenu.as_ref().map(|open| open.hovered));
                    outcome.repaint |= before != after;
                }
            }
            MouseEventKind::Down(MouseButton::Left) => match row_hit {
                Some(SUBMENU_ROW) => {
                    if let Some(ClientShellOverlay::ContextMenu(menu)) = self.overlay.as_mut() {
                        let model = menu.model();
                        menu.highlighted = SUBMENU_ROW;
                        open_submenu(menu, &model, true);
                    }
                    outcome.repaint = true;
                }
                Some(row) => self.activate_context_menu_item(row, outcome),
                None if inside => {}
                None => {
                    self.overlay = None;
                    outcome.repaint = true;
                }
            },
            _ => {}
        }
        true
    }
}

/// 画右键菜单（渲染只读状态）：顶层菜单锚在右键位置，放不下时平移贴边；展开
/// 的子菜单贴在父项右侧，右侧放不下翻到左侧，两侧都放不下时放在空间大的一侧
/// 并贴住屏幕边（此时会压住顶层菜单的一部分）。返回的 `menu_rows` 覆盖两层
/// 画出来的可激活行——被子菜单盖住的顶层行只登记露在外面的部分，盖满的整行
/// 不登记，点子菜单的边框或行不会落到底下看不见的顶层行上；`area` 是两层的
/// 包围盒。
pub(super) fn render_context_menu(
    buffer: &mut Buffer,
    menu: &ClientContextMenuOverlay,
    cx: &ChromeContext<'_>,
) -> Option<OverlayRender> {
    let model = menu.model();
    let kind = context_kind(&menu.target);
    let (items, ids) = top_menu(&model, kind);
    let bounds = buffer.area;
    let hover_bg = Some(cx.components.hover_bg);
    let state = MenuState {
        highlighted: kit_index(&ids, menu.highlighted),
        hovered: menu.hovered.map(|row| kit_index(&ids, row)),
        hover_bg,
        scroll: 0,
    };
    let main = render_menu(
        buffer,
        (menu.x, menu.y),
        bounds,
        &items,
        &state,
        cx.glyphs,
        false,
        cx.palette,
    );
    if main.area.is_empty() {
        return None;
    }
    let mut rows = main
        .rows
        .iter()
        .filter_map(|(rect, index)| ids.get(*index).copied().flatten().map(|row| (*rect, row)))
        .collect::<Vec<_>>();
    let mut area = main.area;
    if let (Some(open), Some(spec)) = (menu.submenu.as_ref(), model.submenu.as_ref()) {
        if let Some((parent, _)) = main.rows.iter().find(|(_, index)| *index == open.parent) {
            let children = sub_menu(&model, kind);
            let (width, _) = menu_size(&children);
            let x = submenu_x(main.area, bounds, width);
            let sub_state = MenuState {
                highlighted: child_kit_index(&model, open.highlighted),
                hovered: open.hovered.map(|row| child_kit_index(&model, row)),
                hover_bg,
                scroll: 0,
            };
            let sub = render_menu(
                buffer,
                (x, parent.y.saturating_sub(1)),
                bounds,
                &children,
                &sub_state,
                cx.glyphs,
                false,
                cx.palette,
            );
            // 子菜单压住的顶层行只保留露在外面的部分，盖满的整行丢掉。
            rows.retain_mut(|(rect, _)| match uncovered_part(*rect, sub.area) {
                Some(visible) => {
                    *rect = visible;
                    true
                }
                None => false,
            });
            rows.extend(
                sub.rows
                    .iter()
                    .map(|(rect, index)| (*rect, spec.children.start + index)),
            );
            area = area.union(sub.area);
        }
    }
    Some(OverlayRender {
        area,
        menu_rows: rows,
        ..OverlayRender::default()
    })
}

/// 子菜单左上角的 x：右侧放得下贴父菜单右边，否则左侧放得下贴父菜单左边；
/// 两侧都放不下时选空间大的一侧、贴住屏幕边，把两层的重叠压到最少（重叠
/// 部分由 [`uncovered_part`] 从顶层命中表里裁掉）。
fn submenu_x(main: Rect, bounds: Rect, width: u16) -> u16 {
    let right_space = bounds.right().saturating_sub(main.right());
    let left_space = main.x.saturating_sub(bounds.x);
    if width <= right_space {
        main.right()
    } else if width <= left_space {
        main.x - width
    } else if right_space >= left_space {
        bounds.right().saturating_sub(width).max(bounds.x)
    } else {
        bounds.x
    }
}

/// 单行矩形 `row` 被 `cover` 盖住后露在外面的部分：不相交原样返回；被盖住
/// 一段时取左右两段里较宽的那段；整行被盖满返回 `None`。
fn uncovered_part(row: Rect, cover: Rect) -> Option<Rect> {
    if !row.intersects(cover) {
        return Some(row);
    }
    let left = cover.x.saturating_sub(row.x);
    let right = row.right().saturating_sub(cover.right());
    if left == 0 && right == 0 {
        None
    } else if left >= right {
        Some(Rect::new(row.x, row.y, left, row.height))
    } else {
        Some(Rect::new(cover.right(), row.y, right, row.height))
    }
}
