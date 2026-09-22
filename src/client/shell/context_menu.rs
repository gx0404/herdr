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
                let mut items = vec![
                    item(t.menu_focus, Action::FocusAgent),
                    ClientContextMenuItem {
                        enabled: *has_activity,
                        ..item(t.menu_view_activity, Action::ViewAgentActivity)
                    },
                    item(t.menu_rename, Action::RenameAgent),
                ];
                if agent.is_some() {
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
        match menu.target {
            ClientContextMenuTarget::Workspace { workspace_id, .. } => {
                self.activate_workspace_context_action(workspace_id, action, outcome)
            }
            ClientContextMenuTarget::Tab {
                tab_id,
                workspace_id,
            } => self.activate_tab_context_action(tab_id, workspace_id, action, outcome),
            ClientContextMenuTarget::Pane {
                pane_id,
                workspace_id,
                source_pane_id,
                right_click_passthrough,
                ..
            } => self.activate_pane_context_action(
                pane_id,
                workspace_id,
                source_pane_id,
                right_click_passthrough,
                action,
                outcome,
            ),
            ClientContextMenuTarget::Machine { endpoint_id, .. } => {
                self.activate_machine_context_action(&endpoint_id, action, outcome)
            }
            ClientContextMenuTarget::Agent {
                endpoint_id,
                pane_id,
                ..
            } => self.activate_agent_context_action(
                endpoint_id,
                super::agent_activity_overlay::AgentActivityOwner::Pane { pane_id },
                action,
                outcome,
            ),
            ClientContextMenuTarget::ExternalAgent {
                endpoint_id,
                external_id,
            } => self.activate_agent_context_action(
                endpoint_id,
                super::agent_activity_overlay::AgentActivityOwner::External { external_id },
                action,
                outcome,
            ),
        }
        outcome.repaint = true;
    }

    fn activate_machine_context_action(
        &mut self,
        endpoint_id: &ClientEndpointId,
        action: ClientContextMenuAction,
        outcome: &mut ClientShellInput,
    ) {
        use ClientContextMenuAction as Action;
        match action {
            Action::ManageMachines => self.open_machines_overlay(),
            Action::RenameMachine
            | Action::EditMachine
            | Action::ReconnectMachine
            | Action::ToggleMachineEnabled
            | Action::RemoveMachine
            | Action::CopyMachineFixCommand => {
                let ClientEndpointId::Ssh(profile_id) = endpoint_id else {
                    return;
                };
                let profile_id = profile_id.clone();
                match action {
                    Action::RenameMachine => {
                        let label = self
                            .saved_profiles
                            .iter()
                            .find(|profile| profile.id == profile_id)
                            .map(|profile| profile.label.clone());
                        if let Some(label) = label {
                            self.overlay = Some(ClientShellOverlay::Rename(ClientRenameOverlay {
                                title: crate::i18n::texts().machines.edit_title,
                                input: TextEditor::new(&label, false),
                                target: ClientRenameTarget::Machine { profile_id },
                            }));
                        }
                    }
                    Action::EditMachine => self.open_machine_edit_form(&profile_id),
                    Action::ReconnectMachine => self.machine_reconnect(&profile_id, outcome),
                    Action::ToggleMachineEnabled => {
                        let enabled = self
                            .saved_profiles
                            .iter()
                            .any(|profile| profile.id == profile_id && profile.enabled);
                        self.machine_set_enabled(&profile_id, !enabled);
                    }
                    Action::RemoveMachine => self.open_machine_remove_confirm(&profile_id),
                    // 与机器面板的 `c` 同一条路径：自己拼命令会漏掉反馈
                    // （C-27：这个触发点此前零反馈）。
                    Action::CopyMachineFixCommand => {
                        self.machine_copy_fix_command(&profile_id, outcome)
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn activate_workspace_context_action(
        &mut self,
        workspace_id: String,
        action: ClientContextMenuAction,
        outcome: &mut ClientShellInput,
    ) {
        use crate::input::KeybindAction;

        match action {
            ClientContextMenuAction::Rename => {
                let label = self
                    .snapshot
                    .as_deref()
                    .and_then(|snapshot| {
                        snapshot
                            .workspaces
                            .iter()
                            .find(|workspace| workspace.workspace_id == workspace_id)
                    })
                    .map(|workspace| workspace.label.clone());
                if let Some(label) = label {
                    self.overlay = Some(ClientShellOverlay::Rename(ClientRenameOverlay {
                        title: "rename workspace",
                        input: TextEditor::new(&label, false),
                        target: ClientRenameTarget::Workspace { workspace_id },
                    }));
                }
            }
            ClientContextMenuAction::Close => {
                if self.config.confirm_close {
                    self.open_confirm_close_overlay(workspace_id);
                } else {
                    self.push_endpoint_method(
                        crate::api::schema::Method::WorkspaceClose(
                            crate::api::schema::WorkspaceCloseParams {
                                workspace_id,
                                close_group: true,
                            },
                        ),
                        outcome,
                    );
                }
            }
            ClientContextMenuAction::NewWorktree => {
                self.begin_worktree_action_for(KeybindAction::NewWorktree, workspace_id, outcome)
            }
            ClientContextMenuAction::OpenWorktree => {
                self.begin_worktree_action_for(KeybindAction::OpenWorktree, workspace_id, outcome)
            }
            ClientContextMenuAction::RemoveWorktree => {
                self.begin_worktree_action_for(KeybindAction::RemoveWorktree, workspace_id, outcome)
            }
            ClientContextMenuAction::ToggleGroup => {
                let key = self.snapshot.as_deref().and_then(|snapshot| {
                    snapshot
                        .workspaces
                        .iter()
                        .find(|workspace| workspace.workspace_id == workspace_id)
                        .and_then(|workspace| workspace.worktree.as_ref())
                        .map(|worktree| worktree.key.clone())
                });
                if let Some(key) = key {
                    let endpoint_id = self.active_endpoint_id.clone();
                    self.toggle_collapsed_group(&endpoint_id, key);
                    self.persist_chrome_preferences(outcome);
                }
            }
            _ => {}
        }
    }

    fn activate_tab_context_action(
        &mut self,
        tab_id: String,
        workspace_id: String,
        action: ClientContextMenuAction,
        outcome: &mut ClientShellInput,
    ) {
        use crate::api::schema::{Method, TabTarget};

        self.push_endpoint_method(
            Method::TabFocus(TabTarget {
                tab_id: tab_id.clone(),
            }),
            outcome,
        );
        match action {
            ClientContextMenuAction::NewTab => {
                if self.config.prompt_new_tab_name {
                    let default_name = (self
                        .snapshot
                        .as_deref()
                        .map(|snapshot| {
                            snapshot
                                .tabs
                                .iter()
                                .filter(|tab| tab.workspace_id == workspace_id)
                                .count()
                        })
                        .unwrap_or(0)
                        + 1)
                    .to_string();
                    self.overlay = Some(ClientShellOverlay::Rename(ClientRenameOverlay {
                        title: "new tab",
                        input: TextEditor::new(&default_name, true),
                        target: ClientRenameTarget::NewTab {
                            workspace_id,
                            default_name,
                        },
                    }));
                } else {
                    self.push_endpoint_method(
                        Method::TabCreate(crate::api::schema::TabCreateParams {
                            workspace_id: Some(workspace_id),
                            cwd: None,
                            focus: true,
                            label: None,
                            env: Default::default(),
                        }),
                        outcome,
                    );
                }
            }
            ClientContextMenuAction::Rename => {
                let tab = self
                    .snapshot
                    .as_deref()
                    .and_then(|snapshot| snapshot.tabs.iter().find(|tab| tab.tab_id == tab_id));
                if let Some(tab) = tab {
                    self.overlay = Some(ClientShellOverlay::Rename(ClientRenameOverlay {
                        title: "rename tab",
                        input: TextEditor::new(&tab.label, false),
                        target: ClientRenameTarget::Tab {
                            tab_id,
                            auto_name: !tab.custom_label,
                            original_name: tab.label.clone(),
                        },
                    }));
                }
            }
            ClientContextMenuAction::Close => {
                self.push_endpoint_method(Method::TabClose(TabTarget { tab_id }), outcome);
            }
            _ => {}
        }
    }

    fn activate_pane_context_action(
        &mut self,
        pane_id: String,
        workspace_id: String,
        source_pane_id: Option<String>,
        right_click_passthrough: bool,
        action: ClientContextMenuAction,
        outcome: &mut ClientShellInput,
    ) {
        use crate::api::schema::{
            Method, PaneInputSetParams, PaneRenameParams, PaneRightClickTarget, PaneSplitParams,
            PaneSwapParams, PaneTarget, PaneZoomMode, PaneZoomParams, SplitDirection,
        };

        match action {
            ClientContextMenuAction::RenamePane => {
                let label = self.snapshot.as_deref().and_then(|snapshot| {
                    snapshot
                        .panes
                        .iter()
                        .find(|pane| pane.pane_id == pane_id)
                        .and_then(|pane| pane.label.clone())
                });
                self.overlay = Some(ClientShellOverlay::Rename(ClientRenameOverlay {
                    title: "rename pane",
                    input: TextEditor::new(label.as_deref().unwrap_or_default(), label.is_none()),
                    target: ClientRenameTarget::Pane { pane_id },
                }));
            }
            ClientContextMenuAction::ClearPaneName => self.push_endpoint_method(
                Method::PaneRename(PaneRenameParams {
                    pane_id,
                    label: None,
                }),
                outcome,
            ),
            ClientContextMenuAction::SwapWithFocusedPane => {
                if let Some(source_pane_id) = source_pane_id {
                    self.push_endpoint_method(
                        Method::PaneSwap(PaneSwapParams {
                            pane_id: None,
                            direction: None,
                            source_pane_id: Some(source_pane_id.clone()),
                            target_pane_id: Some(pane_id),
                        }),
                        outcome,
                    );
                    self.push_endpoint_method(
                        Method::PaneFocus(PaneTarget {
                            pane_id: source_pane_id,
                        }),
                        outcome,
                    );
                }
            }
            ClientContextMenuAction::SplitRight | ClientContextMenuAction::SplitDown => {
                self.push_endpoint_method(
                    Method::PaneSplit(PaneSplitParams {
                        workspace_id: Some(workspace_id),
                        target_pane_id: Some(pane_id),
                        direction: if action == ClientContextMenuAction::SplitRight {
                            SplitDirection::Right
                        } else {
                            SplitDirection::Down
                        },
                        ratio: None,
                        cwd: None,
                        focus: true,
                        right_click: Default::default(),
                        env: Default::default(),
                    }),
                    outcome,
                );
            }
            ClientContextMenuAction::Zoom => self.push_endpoint_method(
                Method::PaneZoom(PaneZoomParams {
                    pane_id: Some(pane_id),
                    mode: PaneZoomMode::Toggle,
                }),
                outcome,
            ),
            ClientContextMenuAction::ToggleRightClickPassthrough => self.push_endpoint_method(
                Method::PaneInputSet(PaneInputSetParams {
                    pane_id,
                    right_click: if right_click_passthrough {
                        PaneRightClickTarget::Herdr
                    } else {
                        PaneRightClickTarget::Pane
                    },
                }),
                outcome,
            ),
            ClientContextMenuAction::ClosePane => {
                self.push_endpoint_method(Method::PaneClose(PaneTarget { pane_id }), outcome)
            }
            _ => {}
        }
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
