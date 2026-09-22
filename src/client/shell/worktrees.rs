use super::*;

fn checkout_path_preview(root: &str, repo: &str, branch: &str) -> String {
    // This is an endpoint path, not a path on the machine drawing the dialog.
    let separator = if !root.starts_with('/') && root.contains('\\') {
        '\\'
    } else {
        '/'
    };
    format!(
        "{}{separator}{repo}{separator}{}",
        root.trim_end_matches(separator),
        crate::worktree::branch_to_path_slug(branch)
    )
}

impl ClientShellState {
    fn endpoint_worktree_directory(&self) -> Option<String> {
        self.snapshot
            .as_deref()
            .map(|snapshot| snapshot.worktree_directory.clone())
    }

    pub(super) fn insert_worktree_overlay_text(&mut self, text: &str) -> bool {
        match self.overlay.as_mut() {
            Some(ClientShellOverlay::WorktreeCreate(create)) if !create.creating => {
                if create.branch.insert(text) {
                    self.sync_worktree_create_path();
                }
                true
            }
            Some(ClientShellOverlay::WorktreeOpen(open))
                if open.search_focused && !open.opening =>
            {
                if open.query.insert(text) {
                    if let Some(first) = open.filtered_indices().first().copied() {
                        open.selected = first;
                    }
                }
                true
            }
            _ => false,
        }
    }

    pub(super) fn route_worktree_overlay_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) -> bool {
        let (code, modifiers) = crate::config::normalize_key_combo((key.code, key.modifiers));
        match self.overlay.as_ref() {
            Some(ClientShellOverlay::WorktreeCreate(_)) => {
                let creating = matches!(
                    self.overlay,
                    Some(ClientShellOverlay::WorktreeCreate(
                        ClientWorktreeCreateOverlay { creating: true, .. }
                    ))
                );
                if !creating {
                    if let Some(ClientShellOverlay::WorktreeCreate(create)) = self.overlay.as_mut()
                    {
                        if let Some(content_changed) = create.branch.handle_key(key) {
                            if content_changed {
                                self.sync_worktree_create_path();
                            }
                            outcome.repaint = true;
                            return true;
                        }
                    }
                }
                match code {
                    KeyCode::Esc if !creating => {
                        self.overlay = None;
                        outcome.repaint = true;
                    }
                    KeyCode::Enter => self.submit_worktree_create(outcome),
                    _ => {}
                }
                true
            }
            Some(ClientShellOverlay::WorktreeOpen(_)) => {
                let opening = matches!(
                    self.overlay,
                    Some(ClientShellOverlay::WorktreeOpen(
                        ClientWorktreeOpenOverlay { opening: true, .. }
                    ))
                );
                let search_focused = matches!(
                    self.overlay,
                    Some(ClientShellOverlay::WorktreeOpen(
                        ClientWorktreeOpenOverlay {
                            search_focused: true,
                            ..
                        }
                    ))
                );
                if !opening && search_focused {
                    if let Some(ClientShellOverlay::WorktreeOpen(open)) = self.overlay.as_mut() {
                        if let Some(content_changed) = open.query.handle_key(key) {
                            if content_changed {
                                if let Some(first) = open.filtered_indices().first().copied() {
                                    open.selected = first;
                                }
                            }
                            outcome.repaint = true;
                            return true;
                        }
                    }
                }
                match code {
                    KeyCode::Esc if !opening => {
                        self.overlay = None;
                        outcome.repaint = true;
                    }
                    KeyCode::Enter => self.submit_worktree_open(outcome),
                    KeyCode::Up if !opening => {
                        self.move_worktree_open_selection(-1);
                        outcome.repaint = true;
                    }
                    KeyCode::Down if !opening => {
                        self.move_worktree_open_selection(1);
                        outcome.repaint = true;
                    }
                    KeyCode::Char('n' | 'p')
                        if !opening && modifiers == crossterm::event::KeyModifiers::CONTROL =>
                    {
                        self.move_worktree_open_selection(if code == KeyCode::Char('n') {
                            1
                        } else {
                            -1
                        });
                        outcome.repaint = true;
                    }
                    KeyCode::Char('/') if !opening && !search_focused => {
                        if let Some(ClientShellOverlay::WorktreeOpen(open)) = self.overlay.as_mut()
                        {
                            open.search_focused = true;
                        }
                        outcome.repaint = true;
                    }
                    _ => {}
                }
                true
            }
            Some(ClientShellOverlay::WorktreeRemove(_)) => {
                let (removing, forced) = match &self.overlay {
                    Some(ClientShellOverlay::WorktreeRemove(remove)) => {
                        (remove.removing, remove.force_confirmation)
                    }
                    _ => (false, false),
                };
                match code {
                    // y / n 是浮层画出的两个按钮：只有 Enter/Esc 时「取消」按钮
                    // 键盘走不到（HERDR-TOOL-24）。
                    KeyCode::Char('n' | 'N') if !removing => {
                        self.overlay = None;
                        outcome.repaint = true;
                    }
                    KeyCode::Esc if !removing => {
                        self.overlay = None;
                        outcome.repaint = true;
                    }
                    // 普通删除用回车确认；一旦失败武装了「强制删除」，这一步就换键
                    // （y 或 ctrl+↵）。换键与「重复是否可辨识」解耦：宿主不支持
                    // kitty 事件类型时自动重复只发普通 Press，lease 的步进指纹拦不
                    // 住，但换键后长按/连点回车都到不了强制删除。
                    KeyCode::Enter
                        if !forced
                            || modifiers.contains(crossterm::event::KeyModifiers::CONTROL) =>
                    {
                        self.submit_worktree_remove(outcome)
                    }
                    // 普通确认仍是回车：`y` 只在强制删除那一步生效，换键正是
                    // 「长按回车走不过去」的保护（见上面的注释）。
                    KeyCode::Char('y' | 'Y') if forced && !removing => {
                        self.submit_worktree_remove(outcome)
                    }
                    _ => {}
                }
                true
            }
            _ => false,
        }
    }

    pub(super) fn begin_worktree_action(
        &mut self,
        action: crate::input::KeybindAction,
        outcome: &mut ClientShellInput,
    ) {
        let Some(workspace_id) = self.workspace_action_id() else {
            return;
        };
        self.begin_worktree_action_for(action, workspace_id, outcome);
    }

    pub(super) fn begin_worktree_action_for(
        &mut self,
        action: crate::input::KeybindAction,
        workspace_id: String,
        outcome: &mut ClientShellInput,
    ) {
        use crate::api::schema::{Method, WorktreeListParams};
        use crate::input::KeybindAction;

        let workspace = self.snapshot.as_deref().and_then(|snapshot| {
            snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.workspace_id == workspace_id)
        });
        let linked = workspace
            .and_then(|workspace| workspace.worktree.as_ref())
            .is_some_and(|worktree| worktree.is_linked_worktree);
        let kind = match action {
            KeybindAction::NewWorktree | KeybindAction::OpenWorktree if linked => {
                self.set_endpoint_error(crate::i18n::texts().worktree.start_from_parent);
                outcome.repaint = true;
                return;
            }
            KeybindAction::NewWorktree => PendingEndpointKind::PrepareWorktreeCreate {
                workspace_id: workspace_id.clone(),
            },
            KeybindAction::OpenWorktree => PendingEndpointKind::PrepareWorktreeOpen {
                workspace_id: workspace_id.clone(),
            },
            KeybindAction::RemoveWorktree if !linked => {
                self.set_endpoint_error(crate::i18n::texts().worktree.not_a_worktree_checkout);
                outcome.repaint = true;
                return;
            }
            KeybindAction::RemoveWorktree => PendingEndpointKind::PrepareWorktreeRemove {
                workspace_id: workspace_id.clone(),
            },
            _ => return,
        };
        self.push_endpoint_method_with_kind(
            Method::WorktreeList(WorktreeListParams {
                workspace_id: Some(workspace_id),
                cwd: None,
                trust_repository: false,
            }),
            kind,
            outcome,
        );
    }

    pub(super) fn sync_worktree_create_path(&mut self) {
        let Some(worktree_directory) = self.endpoint_worktree_directory() else {
            return;
        };
        let Some(ClientShellOverlay::WorktreeCreate(create)) = self.overlay.as_mut() else {
            return;
        };
        create.checkout_path =
            checkout_path_preview(&worktree_directory, &create.repo_name, &create.branch);
        create.error = None;
    }

    pub(super) fn submit_worktree_create(&mut self, outcome: &mut ClientShellInput) {
        let Some(worktree_directory) = self.endpoint_worktree_directory() else {
            return;
        };
        let Some(ClientShellOverlay::WorktreeCreate(create)) = self.overlay.as_mut() else {
            return;
        };
        if create.creating {
            return;
        }
        let branch = create.branch.trim().to_owned();
        if branch.is_empty() {
            create.error = Some(crate::i18n::texts().worktree.branch_required.to_owned());
            outcome.repaint = true;
            return;
        }
        create.branch.trim_and_accept();
        create.checkout_path =
            checkout_path_preview(&worktree_directory, &create.repo_name, &branch);
        create.creating = true;
        create.error = None;
        let workspace_id = create.source_workspace_id.clone();
        if !self.push_endpoint_method_with_kind(
            crate::api::schema::Method::WorktreeCreate(crate::api::schema::WorktreeCreateParams {
                workspace_id: Some(workspace_id),
                cwd: None,
                branch: Some(branch),
                base: Some("HEAD".to_owned()),
                path: None,
                label: None,
                focus: false,
                trust_repository: false,
            }),
            PendingEndpointKind::WorktreeCreate,
            outcome,
        ) {
            if let Some(ClientShellOverlay::WorktreeCreate(create)) = self.overlay.as_mut() {
                create.creating = false;
            }
        }
        outcome.repaint = true;
    }

    pub(super) fn move_worktree_open_selection(&mut self, delta: isize) {
        let Some(ClientShellOverlay::WorktreeOpen(open)) = self.overlay.as_mut() else {
            return;
        };
        let filtered = open.filtered_indices();
        if filtered.is_empty() {
            open.selected = 0;
            return;
        }
        let current = filtered
            .iter()
            .position(|index| *index == open.selected)
            .unwrap_or(0);
        let next = (current as isize + delta).clamp(0, filtered.len() as isize - 1) as usize;
        open.selected = filtered[next];
    }

    pub(super) fn submit_worktree_open(&mut self, outcome: &mut ClientShellInput) {
        let Some(ClientShellOverlay::WorktreeOpen(open)) = self.overlay.as_mut() else {
            return;
        };
        if open.opening {
            return;
        }
        let Some(index) = open.selected_entry_index() else {
            return;
        };
        let Some(entry) = open.entries.get(index) else {
            return;
        };
        if !entry.can_open() {
            // 目录已缺失且未在 herdr 中打开的检出无法打开：就地提示，不发 worktree.open 请求。
            open.selected = index;
            open.error = Some(
                crate::i18n::texts()
                    .worktree
                    .prunable_cannot_open
                    .to_owned(),
            );
            outcome.repaint = true;
            return;
        }
        let workspace_id = open.source_workspace_id.clone();
        let path = entry.path.clone();
        open.selected = index;
        open.opening = true;
        open.error = None;
        if !self.push_endpoint_method_with_kind(
            crate::api::schema::Method::WorktreeOpen(crate::api::schema::WorktreeOpenParams {
                workspace_id: Some(workspace_id),
                cwd: None,
                path: Some(path),
                branch: None,
                label: None,
                focus: true,
                trust_repository: false,
            }),
            PendingEndpointKind::WorktreeOpen,
            outcome,
        ) {
            if let Some(ClientShellOverlay::WorktreeOpen(open)) = self.overlay.as_mut() {
                open.opening = false;
            }
        }
        outcome.repaint = true;
    }

    pub(super) fn submit_worktree_remove(&mut self, outcome: &mut ClientShellInput) {
        let Some(ClientShellOverlay::WorktreeRemove(remove)) = self.overlay.as_mut() else {
            return;
        };
        if remove.removing {
            return;
        }
        let workspace_id = remove.workspace_id.clone();
        let forced = remove.force_confirmation;
        remove.removing = true;
        remove.error = None;
        if !self.push_endpoint_method_with_kind(
            crate::api::schema::Method::WorktreeRemove(crate::api::schema::WorktreeRemoveParams {
                workspace_id,
                force: forced,
                trust_repository: false,
            }),
            PendingEndpointKind::WorktreeRemove { forced },
            outcome,
        ) {
            if let Some(ClientShellOverlay::WorktreeRemove(remove)) = self.overlay.as_mut() {
                remove.removing = false;
            }
        }
        outcome.repaint = true;
    }

    pub(super) fn handle_worktree_endpoint_result(
        &mut self,
        kind: PendingEndpointKind,
        result: Result<crate::api::schema::ResponseResult, ClientShellEndpointError>,
        outcome: &mut ClientShellInput,
    ) -> bool {
        use crate::api::schema::ResponseResult;

        match (kind, result) {
            (
                PendingEndpointKind::Observation { .. }
                | PendingEndpointKind::Views { .. }
                | PendingEndpointKind::AgentActivityRead { .. },
                _,
            ) => false,
            (
                PendingEndpointKind::PrepareWorktreeCreate { workspace_id },
                Ok(ResponseResult::WorktreeList { source, .. }),
            ) => {
                let seed = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|duration| duration.as_micros().min(u128::from(u64::MAX)) as u64)
                    .unwrap_or(0);
                let branch = crate::worktree::generated_branch_slug(seed);
                let Some(worktree_directory) = self.endpoint_worktree_directory() else {
                    return false;
                };
                let checkout_path =
                    checkout_path_preview(&worktree_directory, &source.repo_name, &branch);
                self.overlay = Some(ClientShellOverlay::WorktreeCreate(
                    ClientWorktreeCreateOverlay {
                        source_workspace_id: workspace_id,
                        repo_name: source.repo_name,
                        branch: TextEditor::new(&branch, true),
                        checkout_path,
                        error: None,
                        creating: false,
                    },
                ));
                true
            }
            (
                PendingEndpointKind::PrepareWorktreeOpen { workspace_id },
                Ok(ResponseResult::WorktreeList { worktrees, .. }),
            ) => {
                // prunable（目录已缺失）的检出保留在列表里并显式标注，不再静默隐藏。
                let entries = worktrees
                    .into_iter()
                    .filter(|entry| !entry.is_bare)
                    .map(|entry| {
                        let label = entry.branch.clone().unwrap_or_else(|| entry.label.clone());
                        ClientWorktreeOpenEntry {
                            path: entry.path,
                            branch: entry.branch,
                            is_linked_worktree: entry.is_linked_worktree,
                            is_detached: entry.is_detached,
                            is_prunable: entry.is_prunable,
                            open_workspace_id: entry.open_workspace_id,
                            label,
                        }
                    })
                    .collect::<Vec<_>>();
                if entries.is_empty() {
                    self.set_endpoint_error(crate::i18n::texts().worktree.no_worktrees_found);
                } else {
                    let selected = entries
                        .iter()
                        .position(ClientWorktreeOpenEntry::can_open)
                        .unwrap_or(0);
                    self.overlay = Some(ClientShellOverlay::WorktreeOpen(
                        ClientWorktreeOpenOverlay {
                            source_workspace_id: workspace_id,
                            entries,
                            selected,
                            query: TextEditor::default(),
                            search_focused: false,
                            error: None,
                            opening: false,
                        },
                    ));
                }
                true
            }
            (
                PendingEndpointKind::PrepareWorktreeRemove { workspace_id },
                Ok(ResponseResult::WorktreeList { worktrees, .. }),
            ) => {
                let path = worktrees
                    .into_iter()
                    .find(|entry| entry.open_workspace_id.as_deref() == Some(&workspace_id))
                    .map(|entry| entry.path);
                if let Some(path) = path {
                    self.overlay = Some(ClientShellOverlay::WorktreeRemove(
                        ClientWorktreeRemoveOverlay {
                            workspace_id,
                            path,
                            error: None,
                            removing: false,
                            force_confirmation: false,
                        },
                    ));
                } else {
                    self.set_endpoint_error(crate::i18n::texts().worktree.not_a_worktree_checkout);
                }
                true
            }
            (
                PendingEndpointKind::WorktreeCreate,
                Ok(ResponseResult::WorktreeCreated { tab, .. }),
            ) => {
                self.overlay = None;
                self.push_endpoint_method(
                    crate::api::schema::Method::TabFocus(crate::api::schema::TabTarget {
                        tab_id: tab.tab_id,
                    }),
                    outcome,
                );
                true
            }
            (PendingEndpointKind::WorktreeOpen, Ok(ResponseResult::WorktreeOpened { .. }))
            | (
                PendingEndpointKind::WorktreeRemove { .. },
                Ok(ResponseResult::WorktreeRemoved { .. }),
            ) => {
                self.overlay = None;
                true
            }
            (PendingEndpointKind::WorktreeCreate, Err(error)) => {
                if let Some(ClientShellOverlay::WorktreeCreate(create)) = self.overlay.as_mut() {
                    create.creating = false;
                    create.error = Some(error.message);
                }
                true
            }
            (PendingEndpointKind::WorktreeOpen, Err(error)) => {
                if let Some(ClientShellOverlay::WorktreeOpen(open)) = self.overlay.as_mut() {
                    open.opening = false;
                    open.error = Some(error.message);
                }
                true
            }
            (PendingEndpointKind::WorktreeRemove { forced: false }, Err(error))
                if error.code.as_deref() == Some("dirty_worktree_requires_force")
                    || (error.code.as_deref() == Some("worktree_remove_failed")
                        && crate::worktree::is_not_working_tree_remove_error(&error.message)) =>
            {
                if let Some(ClientShellOverlay::WorktreeRemove(remove)) = self.overlay.as_mut() {
                    remove.removing = false;
                    remove.force_confirmation = true;
                    remove.error = None;
                }
                true
            }
            (PendingEndpointKind::WorktreeRemove { .. }, Err(error)) => {
                if let Some(ClientShellOverlay::WorktreeRemove(remove)) = self.overlay.as_mut() {
                    remove.removing = false;
                    remove.error = Some(error.message);
                }
                true
            }
            (
                PendingEndpointKind::PrepareWorktreeCreate { .. }
                | PendingEndpointKind::PrepareWorktreeOpen { .. }
                | PendingEndpointKind::PrepareWorktreeRemove { .. },
                Err(_),
            ) => true,
            (_, Ok(_)) => {
                self.set_endpoint_error(crate::i18n::texts().worktree.unexpected_result);
                true
            }
            (
                PendingEndpointKind::TextCapture { .. }
                | PendingEndpointKind::TextWindow { .. }
                | PendingEndpointKind::TextCopy { .. }
                | PendingEndpointKind::TextRelease
                | PendingEndpointKind::Generic
                | PendingEndpointKind::ProductAnnouncementDismiss { .. }
                | PendingEndpointKind::ReleaseNotesDismiss
                | PendingEndpointKind::PopupCommand
                | PendingEndpointKind::ReloadConfig
                | PendingEndpointKind::IntegrationList
                | PendingEndpointKind::IntegrationInstall
                | PendingEndpointKind::SelectionCopy
                | PendingEndpointKind::PaneScroll { .. }
                | PendingEndpointKind::WordSelection { .. }
                | PendingEndpointKind::PaneLinkActivate { .. }
                | PendingEndpointKind::PaneLinkResolve { .. }
                | PendingEndpointKind::CopyMotion { .. }
                | PendingEndpointKind::CopySearch { .. },
                Err(_),
            ) => true,
            // Snippet runs resolve in `handle_endpoint_result` before this.
            (PendingEndpointKind::SnippetRun { .. }, _) => false,
            (PendingEndpointKind::BroadcastSend { .. }, _) => false,
        }
    }
}

#[cfg(test)]
mod path_tests {
    use super::checkout_path_preview;

    #[test]
    fn endpoint_path_style_is_independent_of_the_client_os() {
        assert_eq!(
            checkout_path_preview("/worktrees/", "repo", "feature/a"),
            "/worktrees/repo/feature-a"
        );
        assert_eq!(
            checkout_path_preview(r"C:\worktrees\", "repo", "feature/a"),
            r"C:\worktrees\repo\feature-a"
        );
        assert_eq!(
            checkout_path_preview(r"\\server\share", "repo", "feature/a"),
            r"\\server\share\repo\feature-a"
        );
    }
}
