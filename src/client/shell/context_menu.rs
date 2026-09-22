use super::action_table::{ActionTarget, ContextKind};
use super::feedback::ChromeContext;
use super::render::{display_width, panel, put_text, OverlayRender};
use super::*;
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

impl ClientContextMenuOverlay {
    pub(super) fn items(&self) -> Vec<ClientContextMenuItem> {
        use ClientContextMenuAction as Action;

        let t = &crate::i18n::texts().context_menu;
        let item = |label, action| ClientContextMenuItem {
            label,
            action,
            enabled: true,
            shortcut: None,
            checked: None,
            separator_before: false,
        };
        match &self.target {
            ClientContextMenuTarget::Workspace { is_git: false, .. } => {
                vec![item(t.rename, Action::Rename), item(t.close, Action::Close)]
            }
            ClientContextMenuTarget::Workspace {
                is_linked_worktree: false,
                has_worktree_children: false,
                ..
            } => vec![
                item(t.rename, Action::Rename),
                item(t.close, Action::Close),
                item(t.new_worktree, Action::NewWorktree),
                item(t.open_worktree, Action::OpenWorktree),
            ],
            ClientContextMenuTarget::Workspace {
                is_linked_worktree: true,
                ..
            } => vec![
                item(t.rename, Action::Rename),
                item(t.close, Action::Close),
                item(t.delete_worktree, Action::RemoveWorktree),
            ],
            ClientContextMenuTarget::Workspace {
                has_worktree_children: true,
                collapsed,
                ..
            } => vec![
                item(t.rename, Action::Rename),
                item(t.close_group, Action::Close),
                item(t.new_worktree, Action::NewWorktree),
                item(t.open_worktree, Action::OpenWorktree),
                item(
                    if *collapsed { t.expand } else { t.collapse },
                    Action::ToggleGroup,
                ),
            ],
            ClientContextMenuTarget::Tab { .. } => vec![
                item(t.new_tab, Action::NewTab),
                item(t.rename, Action::Rename),
                item(t.close, Action::Close),
            ],
            ClientContextMenuTarget::Pane {
                source_pane_id,
                has_manual_label,
                right_click_passthrough,
                ..
            } => {
                let mut items = vec![item(t.rename_pane, Action::RenamePane)];
                if *has_manual_label {
                    items.push(item(t.clear_pane_name, Action::ClearPaneName));
                }
                if source_pane_id.is_some() {
                    items.push(item(t.swap_with_focused, Action::SwapWithFocusedPane));
                }
                items.extend([
                    item(t.split_right, Action::SplitRight),
                    item(t.split_down, Action::SplitDown),
                    item(t.zoom, Action::Zoom),
                    item(
                        if *right_click_passthrough {
                            t.use_herdr_menu
                        } else {
                            t.send_right_clicks
                        },
                        Action::ToggleRightClickPassthrough,
                    ),
                    item(t.close_pane, Action::ClosePane),
                ]);
                items
            }
            ClientContextMenuTarget::Machine {
                endpoint_id,
                enabled,
                online,
            } => {
                let mut items = vec![item(t.manage_machines, Action::ManageMachines)];
                if endpoint_id.is_local() {
                    return items;
                }
                items.push(item(t.rename, Action::RenameMachine));
                items.push(item(t.edit_machine, Action::EditMachine));
                if *enabled && !*online {
                    items.push(item(t.reconnect_machine, Action::ReconnectMachine));
                }
                items.push(item(
                    if *enabled {
                        t.disable_machine
                    } else {
                        t.enable_machine
                    },
                    Action::ToggleMachineEnabled,
                ));
                items.push(item(t.remove_machine, Action::RemoveMachine));
                items.push(item(
                    t.copy_machine_fix_command,
                    Action::CopyMachineFixCommand,
                ));
                items
            }
            // agent 行的条目由接缝定稿：面板车道只接动作，菜单车道只重写渲染与
            // 键盘导航，都不改条目语义。
            ClientContextMenuTarget::Agent {
                agent,
                has_activity,
                ..
            } => {
                let t = &crate::i18n::texts().agent_panel;
                // seam-stub(agent-panel)：动作尚未接通的条目一律灰显——激活后
                // `activate_agent_context_action` 只会落到空臂，菜单却已关掉。
                // 波 2 面板车道接上动作时把对应的 `enabled` 翻回 true。
                let stub = |label, action| ClientContextMenuItem {
                    enabled: false,
                    ..item(label, action)
                };
                let mut items = vec![
                    item(t.menu_focus, Action::FocusAgent),
                    ClientContextMenuItem {
                        enabled: *has_activity,
                        ..item(t.menu_view_activity, Action::ViewAgentActivity)
                    },
                    stub(t.menu_rename, Action::RenameAgent),
                ];
                if agent.is_some() {
                    items.push(stub(t.menu_usage, Action::ShowAgentUsage));
                    items.push(stub(t.menu_bind_account, Action::BindAgentAccount));
                }
                items.push(stub(t.menu_close, Action::CloseAgentPane));
                items
            }
            ClientContextMenuTarget::ExternalAgent { .. } => vec![item(
                crate::i18n::texts().agent_panel.menu_view_activity,
                Action::ViewAgentActivity,
            )],
        }
    }
}
impl ClientShellState {
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
        self.overlay = Some(ClientShellOverlay::ContextMenu(ClientContextMenuOverlay {
            target: ClientContextMenuTarget::Workspace {
                workspace_id,
                is_git: worktree.is_some() || workspace.branch.is_some(),
                is_linked_worktree: worktree.is_some_and(|worktree| worktree.is_linked_worktree),
                has_worktree_children,
                collapsed,
            },
            x,
            y,
            highlighted: 0,
            hovered: None,
            submenu: None,
        }));
    }

    pub(super) fn open_tab_context_menu(&mut self, tab_id: String, x: u16, y: u16) {
        let Some(tab) = self
            .snapshot
            .as_deref()
            .and_then(|snapshot| snapshot.tabs.iter().find(|tab| tab.tab_id == tab_id))
        else {
            return;
        };
        self.overlay = Some(ClientShellOverlay::ContextMenu(ClientContextMenuOverlay {
            target: ClientContextMenuTarget::Tab {
                tab_id,
                workspace_id: tab.workspace_id.clone(),
            },
            x,
            y,
            highlighted: 0,
            hovered: None,
            submenu: None,
        }));
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
        self.overlay = Some(ClientShellOverlay::ContextMenu(ClientContextMenuOverlay {
            target: ClientContextMenuTarget::Pane {
                pane_id,
                workspace_id: pane.workspace_id.clone(),
                source_pane_id,
                has_manual_label: pane.label.is_some(),
                right_click_passthrough: pane.right_click_passthrough,
            },
            x,
            y,
            highlighted: 0,
            hovered: None,
            submenu: None,
        }));
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
        self.overlay = Some(ClientShellOverlay::ContextMenu(ClientContextMenuOverlay {
            target: ClientContextMenuTarget::Machine {
                endpoint_id: endpoint_id.clone(),
                enabled,
                online,
            },
            x,
            y,
            highlighted: 0,
            hovered: None,
            submenu: None,
        }));
    }

    /// 上下键在菜单里回绕，与工作台 Layout 模式、导航器同一口径
    /// （HERDR-UX-08 之前是 clamp，到头就卡住）。
    pub(super) fn move_context_menu_selection(&mut self, delta: isize) {
        let Some(ClientShellOverlay::ContextMenu(menu)) = self.overlay.as_mut() else {
            return;
        };
        let item_count = menu.items().len();
        if item_count == 0 {
            return;
        }
        let count = item_count as isize;
        menu.highlighted = (menu.highlighted as isize + delta).rem_euclid(count) as usize;
    }

    /// Home / End 直接跳到首尾项（HERDR-UX-08：其它列表都有）。
    pub(super) fn set_context_menu_selection(&mut self, last: bool) {
        let Some(ClientShellOverlay::ContextMenu(menu)) = self.overlay.as_mut() else {
            return;
        };
        let item_count = menu.items().len();
        if item_count == 0 {
            return;
        }
        menu.highlighted = if last { item_count - 1 } else { 0 };
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
        let (kind, target) = context_action_target(menu.target);
        if let Some(id) = super::action_table::context_action(kind, action) {
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
    /// `mouse.rs::handle_mouse` 继续往下走。
    pub(super) fn handle_context_menu_mouse(
        &mut self,
        mouse: MouseEvent,
        point: (u16, u16),
        outcome: &mut ClientShellInput,
    ) -> bool {
        if !matches!(self.overlay, Some(ClientShellOverlay::ContextMenu(_))) {
            return false;
        }
        let row_hit = self
            .hits
            .context_menu_rows
            .iter()
            .find(|(rect, _)| super::contains(*rect, point))
            .copied();
        match mouse.kind {
            MouseEventKind::Moved => {
                if let Some(ClientShellOverlay::ContextMenu(menu)) = self.overlay.as_mut() {
                    let hovered = row_hit.map(|(_, index)| index);
                    if menu.hovered != hovered {
                        menu.hovered = hovered;
                        outcome.repaint = true;
                    }
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some((_, index)) = row_hit {
                    self.activate_context_menu_item(index, outcome);
                } else {
                    self.overlay = None;
                    outcome.repaint = true;
                }
            }
            _ => {}
        }
        true
    }
}

pub(super) fn render_context_menu(
    buffer: &mut Buffer,
    menu: &ClientContextMenuOverlay,
    cx: &ChromeContext<'_>,
) -> Option<OverlayRender> {
    let palette = cx.palette;
    let items = menu.items();
    let screen = buffer.area;
    let max_item_width = items
        .iter()
        .map(|item| display_width(item.label))
        .max()
        .unwrap_or(0);
    let width = max_item_width
        .saturating_add(4)
        .max(14)
        .min(screen.width.max(1));
    let height = (items.len() as u16)
        .saturating_add(2)
        .min(screen.height.max(1));
    let x = menu
        .x
        .min(screen.x.saturating_add(screen.width.saturating_sub(width)));
    let y = menu.y.min(
        screen
            .y
            .saturating_add(screen.height.saturating_sub(height)),
    );
    let rect = Rect::new(x, y, width, height);
    let inner = panel(buffer, rect, palette.accent, palette.panel_bg, cx.glyphs)?;
    let mut rows = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let row_y = inner.y.saturating_add(index as u16);
        if row_y >= inner.bottom() {
            break;
        }
        let row = Rect::new(inner.x, row_y, inner.width, 1);
        let style = list_row_style(
            palette,
            cx.components,
            index == menu.highlighted,
            menu.hovered == Some(index),
        );
        let style = if item.enabled {
            style
        } else {
            style.fg(palette.overlay0)
        };
        buffer.set_style(row, style);
        put_text(buffer, row.x, row.y, row.width, item.label, style);
        rows.push((row, index));
    }
    Some(OverlayRender {
        area: rect,
        menu_rows: rows,
        ..OverlayRender::default()
    })
}
