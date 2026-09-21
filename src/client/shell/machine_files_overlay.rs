//! Remote file browser for one saved machine: directory listing, a
//! read-only small-file viewer, and the download/upload/mkdir/rename/delete
//! operations of `remote::RemoteFs`. Every operation runs on a worker thread
//! (`ClientShellAction::MachineFsOp`) and reports back as a client loop
//! event, so the UI never blocks on SSH; the frame loop has no polling —
//! listings refresh only on navigation and after a mutation lands.

use super::render::{
    modal_button, modal_button_row, modal_panel, put_right_text, put_text, render_key_hints,
    render_search_bar, OverlayRender, SearchBar,
};
use super::*;
use crate::client::endpoint::ProfileId;
use crate::remote::{RemoteDirEntry, RemoteEntryKind};
use crossterm::event::KeyModifiers;

#[derive(Debug)]
pub(super) struct ClientMachineFilesOverlay {
    pub(super) profile_id: ProfileId,
    pub(super) cwd: String,
    /// `None` while the first listing is in flight.
    pub(super) entries: Option<Vec<RemoteDirEntry>>,
    /// 与 `entries` 同步重建的小写名目（C-16：过滤不再逐条 `to_lowercase`）。
    names_lower: Vec<String>,
    /// 当前 query 下的过滤计数缓存；与 selected clamp 同机更新（条目到达、
    /// query 编辑、目录切换都经同一组同步点）。
    pub(super) filtered_count: usize,
    pub(super) selected: usize,
    pub(super) query: TextEditor,
    pub(super) search_focused: bool,
    pub(super) view: ClientMachineFilesView,
    /// In-flight operations (drives the loading line); the ticket of the
    /// latest one, so stale results are dropped.
    pub(super) pending: u64,
    requests: HashMap<u64, (FileRequest, crate::remote::TaskCancellation)>,
    latest_read: Option<u64>,
    pub(super) message: Option<String>,
    pub(super) error: Option<String>,
}

impl ClientMachineFilesOverlay {
    /// 预小写 query 的零克隆过滤迭代（C-16）：名目小写在条目到达时已算好；
    /// 可见窗口由调用方用 enumerate + skip + take 决定。
    pub(super) fn filtered_entries(&self) -> impl Iterator<Item = &RemoteDirEntry> {
        let query = self.query.trim().to_lowercase();
        self.entries
            .as_deref()
            .unwrap_or_default()
            .iter()
            .zip(self.names_lower.iter())
            .filter(move |(_, name_lower)| query.is_empty() || name_lower.contains(&query))
            .map(|(entry, _)| entry)
    }

    /// 条目集合变更的同机更新：重建小写名目、重算过滤计数。
    fn set_entries(&mut self, entries: Option<Vec<RemoteDirEntry>>) {
        self.names_lower = entries
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|entry| entry.name.to_lowercase())
            .collect();
        self.entries = entries;
        self.resync_filter_count();
    }

    /// 过滤计数重算（query 或条目变化后调用方负责 selected 的复位/夹取）。
    fn resync_filter_count(&mut self) {
        self.filtered_count = self.filtered_entries().count();
    }
}

/// `str::lines()` 语义的行偏移表：按 `\n` 分段、行尾 `\r` 不计入长度、
/// 末尾无换行的残段收尾；空内容零行。切片 `&content[start..start + len]`
/// 即该行文本，零分配。
fn viewer_line_offsets(content: &str) -> Vec<(usize, usize)> {
    let bytes = content.as_bytes();
    let mut offsets = Vec::new();
    let mut start = 0;
    for (index, _) in content.match_indices('\n') {
        let mut len = index - start;
        if len > 0 && bytes[start + len - 1] == b'\r' {
            len -= 1;
        }
        offsets.push((start, len));
        start = index + 1;
    }
    if start < bytes.len() {
        let mut len = bytes.len() - start;
        if len > 0 && bytes[start + len - 1] == b'\r' {
            len -= 1;
        }
        offsets.push((start, len));
    }
    offsets
}

#[cfg(test)]
mod viewer_line_offsets_tests {
    #[test]
    fn offsets_match_str_lines() {
        for content in [
            "",
            "a",
            "a\n",
            "a\n\n",
            "a\nb\n",
            "a\r\nb\r\n",
            "\n\n\n",
            "tail 无换行",
        ] {
            let offsets = super::viewer_line_offsets(content);
            let sliced: Vec<&str> = offsets
                .iter()
                .map(|&(start, len)| &content[start..start + len])
                .collect();
            let expected: Vec<&str> = content.lines().collect();
            assert_eq!(sliced, expected, "content: {content:?}");
        }
    }
}

impl Drop for ClientMachineFilesOverlay {
    fn drop(&mut self) {
        for (_, cancel) in self.requests.values() {
            cancel.cancel();
        }
    }
}

#[derive(Debug)]
enum FileRequest {
    List { path: String },
    Read { path: String },
    Mutation,
}

#[derive(Debug)]
pub(super) enum ClientMachineFilesView {
    List,
    Viewer {
        path: String,
        content: String,
        /// C-17：内容落地时一次算好的行偏移（字节起点、不含换行/`\r` 的长度），
        /// 渲染按 scroll 切片借用 `&str`，不再逐帧 `content.lines().collect()`。
        line_offsets: Vec<(usize, usize)>,
        /// HERDR-MACH-009：滚动上界 = 行数 − 可见行数；渲染期随最新可见行数回写，
        /// 落地时先按行数兜底。所有改写 scroll 的键/滚轮都 clamp 到它。
        max_scroll: usize,
        scroll: usize,
    },
    ConfirmDelete {
        path: String,
        recursive: bool,
    },
    Prompt {
        kind: MachineFilesPrompt,
        input: TextEditor,
        /// Name of the entry the prompt acts on (download/rename).
        context: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MachineFilesPrompt {
    Download,
    Upload,
    Mkdir,
    Rename,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MachineFilesButton {
    Up,
    Refresh,
    Download,
    Upload,
    Mkdir,
    Rename,
    Delete,
    ConfirmDelete,
    CancelDelete,
    PromptConfirm,
    PromptCancel,
    Back,
}

/// Joins `name` onto `dir` in remote (POSIX) path syntax; absolute `name`
/// wins. Pure string logic so it is testable without SSH.
pub(super) fn remote_join(dir: &str, name: &str) -> String {
    if name.starts_with('/') {
        return name.to_owned();
    }
    if dir.is_empty() || dir == "." {
        return name.to_owned();
    }
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// Parent of a remote path. Relative paths walk up with `..` (the sftp
/// server resolves them against the login directory); `/` is its own
/// parent.
pub(super) fn remote_parent(dir: &str) -> String {
    let trimmed = dir.trim_end_matches('/');
    if trimmed.is_empty() {
        return "/".to_owned();
    }
    match trimmed.rsplit_once('/') {
        Some(("", _)) => "/".to_owned(),
        Some((parent, _)) => parent.to_owned(),
        None => {
            if trimmed == "." {
                "..".to_owned()
            } else if trimmed.starts_with("..") {
                format!("../{trimmed}")
            } else {
                "..".to_owned()
            }
        }
    }
}

impl ClientShellState {
    pub(super) fn open_machine_files(
        &mut self,
        profile_id: &ProfileId,
        outcome: &mut ClientShellInput,
    ) {
        if self
            .saved_profiles
            .iter()
            .all(|profile| &profile.id != profile_id)
        {
            return;
        }
        let overlay = ClientMachineFilesOverlay {
            profile_id: profile_id.clone(),
            cwd: ".".to_owned(),
            entries: None,
            names_lower: Vec::new(),
            filtered_count: 0,
            selected: 0,
            query: TextEditor::default(),
            search_focused: false,
            view: ClientMachineFilesView::List,
            pending: 0,
            requests: HashMap::new(),
            latest_read: None,
            message: None,
            error: None,
        };
        self.overlay = Some(ClientShellOverlay::MachineFiles(overlay));
        self.machine_files_refresh(outcome);
    }

    fn machine_files_overlay(&self) -> Option<&ClientMachineFilesOverlay> {
        match self.overlay.as_ref() {
            Some(ClientShellOverlay::MachineFiles(overlay)) => Some(overlay),
            Some(ClientShellOverlay::CommandPalette(_)) => match self.browser_return.as_deref() {
                Some(ClientShellOverlay::MachineFiles(overlay)) => Some(overlay),
                _ => None,
            },
            _ => None,
        }
    }

    fn machine_files_overlay_mut(&mut self) -> Option<&mut ClientMachineFilesOverlay> {
        match self.overlay.as_mut() {
            Some(ClientShellOverlay::MachineFiles(overlay)) => Some(overlay),
            Some(ClientShellOverlay::CommandPalette(_)) => match self.browser_return.as_deref_mut()
            {
                Some(ClientShellOverlay::MachineFiles(overlay)) => Some(overlay),
                _ => None,
            },
            _ => None,
        }
    }

    /// Issues one fs operation on a worker thread. The ticket supersedes any
    /// earlier in-flight operation: late results are dropped on arrival.
    fn push_machine_files_op(&mut self, op: MachineFsOp, outcome: &mut ClientShellInput) {
        let Some(overlay) = self.machine_files_overlay() else {
            return;
        };
        let Some(profile) = self
            .saved_profiles
            .iter()
            .find(|profile| profile.id == overlay.profile_id)
            .cloned()
        else {
            return;
        };
        if let Some(page) = self.machine_files_overlay_mut() {
            let reading = matches!(op, MachineFsOp::List { .. } | MachineFsOp::Read { .. });
            if !reading
                && page
                    .requests
                    .values()
                    .any(|(request, _)| matches!(request, FileRequest::Mutation))
            {
                return;
            }
            if reading {
                for (request, cancel) in page.requests.values() {
                    if !matches!(request, FileRequest::Mutation) {
                        cancel.cancel();
                    }
                }
            }
        }
        let cancel = crate::remote::TaskCancellation::default();
        let ticket = self.next_machine_files_ticket;
        self.next_machine_files_ticket = self.next_machine_files_ticket.saturating_add(1);
        if let Some(overlay) = self.machine_files_overlay_mut() {
            let request = match &op {
                MachineFsOp::List { path } => {
                    overlay.latest_read = Some(ticket);
                    FileRequest::List { path: path.clone() }
                }
                MachineFsOp::Read { path } => {
                    overlay.latest_read = Some(ticket);
                    FileRequest::Read { path: path.clone() }
                }
                _ => {
                    overlay.latest_read = None;
                    FileRequest::Mutation
                }
            };
            overlay.requests.insert(ticket, (request, cancel.clone()));
            overlay.pending = overlay.requests.len() as u64;
        }
        self.machine_files_ticket = Some(ticket);
        outcome.actions.push(ClientShellAction::MachineFsOp {
            cancel,
            ticket,
            profile: Box::new(profile),
            op,
        });
        outcome.repaint = true;
    }

    fn machine_files_refresh(&mut self, outcome: &mut ClientShellInput) {
        let Some(overlay) = self.machine_files_overlay() else {
            return;
        };
        let path = overlay.cwd.clone();
        self.push_machine_files_op(MachineFsOp::List { path }, outcome);
    }

    fn machine_files_cd(&mut self, path: String, outcome: &mut ClientShellInput) {
        if let Some(overlay) = self.machine_files_overlay_mut() {
            overlay.cwd = path;
            overlay.query.clear();
            overlay.set_entries(None);
            overlay.selected = 0;
            overlay.view = ClientMachineFilesView::List;
            overlay.error = None;
            overlay.message = None;
        }
        self.machine_files_refresh(outcome);
    }

    fn selected_machine_files_entry(&self) -> Option<RemoteDirEntry> {
        let overlay = self.machine_files_overlay()?;
        let count = overlay.filtered_count;
        if count == 0 {
            return None;
        }
        let index = overlay.selected.min(count - 1);
        overlay.filtered_entries().nth(index).cloned()
    }

    fn machine_files_open_selected(&mut self, outcome: &mut ClientShellInput) {
        let Some(entry) = self.selected_machine_files_entry() else {
            return;
        };
        let Some(overlay) = self.machine_files_overlay() else {
            return;
        };
        let path = remote_join(&overlay.cwd, &entry.name);
        match entry.kind {
            RemoteEntryKind::Directory => self.machine_files_cd(path, outcome),
            RemoteEntryKind::File => {
                if let Some(overlay) = self.machine_files_overlay_mut() {
                    overlay.error = None;
                }
                self.push_machine_files_op(MachineFsOp::Read { path }, outcome);
            }
            RemoteEntryKind::Symlink | RemoteEntryKind::Other => {}
        }
        outcome.repaint = true;
    }

    fn move_machine_files_selection(&mut self, delta: isize) {
        let count = self.machine_files_overlay().map_or(0, |o| o.filtered_count);
        let Some(overlay) = self.machine_files_overlay_mut() else {
            return;
        };
        if count == 0 {
            overlay.selected = 0;
            return;
        }
        overlay.selected =
            (overlay.selected as isize + delta).clamp(0, count.saturating_sub(1) as isize) as usize;
    }

    fn machine_files_back(&mut self, outcome: &mut ClientShellInput) {
        enum Back {
            List,
            Detail(ProfileId),
        }
        let action = match self.machine_files_overlay() {
            None => return,
            Some(overlay) => match &overlay.view {
                ClientMachineFilesView::List => Back::Detail(overlay.profile_id.clone()),
                ClientMachineFilesView::Viewer { .. }
                | ClientMachineFilesView::ConfirmDelete { .. }
                | ClientMachineFilesView::Prompt { .. } => Back::List,
            },
        };
        match action {
            Back::List => {
                if let Some(overlay) = self.machine_files_overlay_mut() {
                    overlay.view = ClientMachineFilesView::List;
                    overlay.error = None;
                }
            }
            Back::Detail(profile_id) => {
                // Returning to the machine card reopens the machines overlay
                // on this profile's detail view.
                self.open_machines_overlay_for(&profile_id);
            }
        }
        outcome.repaint = true;
    }

    fn machine_files_start_prompt(&mut self, kind: MachineFilesPrompt) {
        let prefill = match kind {
            MachineFilesPrompt::Download | MachineFilesPrompt::Rename => self
                .selected_machine_files_entry()
                .map(|entry| entry.name)
                .unwrap_or_default(),
            _ => String::new(),
        };
        // Download and rename need a selected entry.
        if matches!(
            kind,
            MachineFilesPrompt::Download | MachineFilesPrompt::Rename
        ) && prefill.is_empty()
        {
            return;
        }
        if matches!(kind, MachineFilesPrompt::Download)
            && self
                .selected_machine_files_entry()
                .is_none_or(|entry| entry.kind == RemoteEntryKind::Directory)
        {
            return;
        }
        if let Some(overlay) = self.machine_files_overlay_mut() {
            overlay.view = ClientMachineFilesView::Prompt {
                kind,
                input: TextEditor::new(&prefill, false),
                context: prefill,
            };
            overlay.error = None;
            overlay.message = None;
        }
    }

    fn machine_files_write_pending(&self) -> bool {
        self.machine_files_overlay().is_some_and(|page| {
            page.requests
                .values()
                .any(|(request, _)| matches!(request, FileRequest::Mutation))
        })
    }

    fn machine_files_submit_prompt(&mut self, outcome: &mut ClientShellInput) {
        if self.machine_files_write_pending() {
            outcome.repaint = true;
            return;
        }
        let (kind, value, context) = {
            let Some(overlay) = self.machine_files_overlay_mut() else {
                return;
            };
            let ClientMachineFilesView::Prompt {
                kind,
                input,
                context,
            } = &mut overlay.view
            else {
                return;
            };
            (*kind, input.trim().to_owned(), context.clone())
        };
        let Some(overlay) = self.machine_files_overlay() else {
            return;
        };
        let cwd = overlay.cwd.clone();
        let t = &crate::i18n::texts().machine_files;
        match kind {
            MachineFilesPrompt::Download => {
                let local = if value.is_empty() {
                    context.clone()
                } else {
                    value
                };
                let remote = remote_join(&cwd, &context);
                let message = crate::i18n::fill(t.downloaded_fmt, &[("path", &local)]);
                if let Some(overlay) = self.machine_files_overlay_mut() {
                    overlay.view = ClientMachineFilesView::List;
                }
                self.push_machine_files_op(
                    MachineFsOp::Download {
                        remote,
                        local,
                        message,
                    },
                    outcome,
                );
            }
            MachineFilesPrompt::Upload => {
                if value.is_empty() {
                    return;
                }
                let name = std::path::Path::new(&value)
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned());
                let Some(name) = name else {
                    return;
                };
                let remote = remote_join(&cwd, &name);
                let message = crate::i18n::fill(t.uploaded_fmt, &[("path", &remote)]);
                if let Some(overlay) = self.machine_files_overlay_mut() {
                    overlay.view = ClientMachineFilesView::List;
                }
                self.push_machine_files_op(
                    MachineFsOp::Upload {
                        local: value,
                        remote,
                        message,
                    },
                    outcome,
                );
            }
            MachineFilesPrompt::Mkdir => {
                if value.is_empty() {
                    return;
                }
                let path = remote_join(&cwd, &value);
                let message = crate::i18n::fill(t.created_fmt, &[("path", &path)]);
                if let Some(overlay) = self.machine_files_overlay_mut() {
                    overlay.view = ClientMachineFilesView::List;
                }
                self.push_machine_files_op(MachineFsOp::Mkdir { path, message }, outcome);
            }
            MachineFilesPrompt::Rename => {
                if value.is_empty() || value == context {
                    if let Some(overlay) = self.machine_files_overlay_mut() {
                        overlay.view = ClientMachineFilesView::List;
                    }
                    outcome.repaint = true;
                    return;
                }
                let from = remote_join(&cwd, &context);
                let to = remote_join(&cwd, &value);
                let message = crate::i18n::fill(t.renamed_fmt, &[("path", &to)]);
                if let Some(overlay) = self.machine_files_overlay_mut() {
                    overlay.view = ClientMachineFilesView::List;
                }
                self.push_machine_files_op(MachineFsOp::Rename { from, to, message }, outcome);
            }
        }
        outcome.repaint = true;
    }

    fn machine_files_delete_selected(&mut self) {
        let Some(entry) = self.selected_machine_files_entry() else {
            return;
        };
        let Some(overlay) = self.machine_files_overlay() else {
            return;
        };
        let path = remote_join(&overlay.cwd, &entry.name);
        let recursive = entry.kind == RemoteEntryKind::Directory;
        if let Some(overlay) = self.machine_files_overlay_mut() {
            overlay.view = ClientMachineFilesView::ConfirmDelete { path, recursive };
            overlay.error = None;
        }
    }

    fn machine_files_confirm_delete(&mut self, outcome: &mut ClientShellInput) {
        if self.machine_files_write_pending() {
            outcome.repaint = true;
            return;
        }
        let (path, recursive) = match self.machine_files_overlay() {
            Some(ClientMachineFilesOverlay {
                view: ClientMachineFilesView::ConfirmDelete { path, recursive },
                ..
            }) => (path.clone(), *recursive),
            _ => return,
        };
        let message = crate::i18n::fill(
            crate::i18n::texts().machine_files.deleted_fmt,
            &[("path", &path)],
        );
        if let Some(overlay) = self.machine_files_overlay_mut() {
            overlay.view = ClientMachineFilesView::List;
        }
        self.push_machine_files_op(
            MachineFsOp::Delete {
                path,
                recursive,
                message,
            },
            outcome,
        );
    }

    /// Result of one worker-thread fs operation (see
    /// `ClientShellAction::MachineFsOp`). Stale tickets drop silently.
    pub(crate) fn handle_machine_fs_result(
        &mut self,
        ticket: u64,
        result: Result<MachineFsOutcome, String>,
        outcome: &mut ClientShellInput,
    ) {
        let Some(overlay) = self.machine_files_overlay_mut() else {
            return;
        };
        let Some((request, _cancel)) = overlay.requests.remove(&ticket) else {
            return;
        };
        overlay.pending = overlay.requests.len() as u64;
        outcome.repaint = true;
        let stale_read = match &request {
            FileRequest::List { path } => {
                overlay.latest_read != Some(ticket) || &overlay.cwd != path
            }
            FileRequest::Read { .. } => overlay.latest_read != Some(ticket),
            FileRequest::Mutation => false,
        };
        if stale_read {
            return;
        }
        let refresh_after = matches!(result, Ok(MachineFsOutcome::Changed { .. }));
        match result {
            Ok(MachineFsOutcome::Entries { entries }) => {
                // 同机更新：名目小写表 + 过滤计数缓存 + selected 夹取。
                overlay.set_entries(Some(entries));
                overlay.error = None;
                overlay.selected = overlay
                    .selected
                    .min(overlay.filtered_count.saturating_sub(1));
            }
            Ok(MachineFsOutcome::FileContent { content }) => {
                let FileRequest::Read { path } = request else {
                    return;
                };
                // C-17：行偏移在内容落地时一次算好；max_scroll 先按行数兜底，
                // 渲染期回写为「行数 − 可见行数」（HERDR-MACH-009）。
                let content = String::from_utf8_lossy(&content).into_owned();
                let line_offsets = viewer_line_offsets(&content);
                let max_scroll = line_offsets.len().saturating_sub(1);
                overlay.view = ClientMachineFilesView::Viewer {
                    path,
                    content,
                    line_offsets,
                    max_scroll,
                    scroll: 0,
                };
                overlay.error = None;
            }
            Ok(MachineFsOutcome::Changed { message }) => {
                overlay.message = Some(message);
                overlay.error = None;
            }
            Err(error) => {
                overlay.error = Some(error);
                if overlay.entries.is_none() {
                    overlay.set_entries(Some(Vec::new()));
                }
            }
        }
        outcome.repaint = true;
        if refresh_after {
            self.machine_files_refresh(outcome);
        }
    }

    pub(super) fn route_machine_files_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) -> bool {
        if !matches!(self.overlay, Some(ClientShellOverlay::MachineFiles(_))) {
            return false;
        }
        let (code, modifiers) = crate::config::normalize_key_combo((key.code, key.modifiers));
        let plain = modifiers.is_empty();

        // Sub-views first: viewer scrolls, confirm acts, prompt edits.
        match self.machine_files_overlay().map(|overlay| &overlay.view) {
            Some(ClientMachineFilesView::Viewer { .. }) => {
                match code {
                    KeyCode::Esc | KeyCode::Char('q') if plain => {
                        self.machine_files_back(outcome);
                    }
                    KeyCode::Up | KeyCode::Char('k') if plain => {
                        if let Some(ClientMachineFilesOverlay {
                            view: ClientMachineFilesView::Viewer { scroll, .. },
                            ..
                        }) = self.machine_files_overlay_mut()
                        {
                            *scroll = scroll.saturating_sub(1);
                        }
                        outcome.repaint = true;
                    }
                    KeyCode::Down | KeyCode::Char('j') if plain => {
                        if let Some(ClientMachineFilesOverlay {
                            view:
                                ClientMachineFilesView::Viewer {
                                    scroll, max_scroll, ..
                                },
                            ..
                        }) = self.machine_files_overlay_mut()
                        {
                            *scroll = scroll.saturating_add(1).min(*max_scroll);
                        }
                        outcome.repaint = true;
                    }
                    KeyCode::PageUp => {
                        if let Some(ClientMachineFilesOverlay {
                            view: ClientMachineFilesView::Viewer { scroll, .. },
                            ..
                        }) = self.machine_files_overlay_mut()
                        {
                            *scroll = scroll.saturating_sub(8);
                        }
                        outcome.repaint = true;
                    }
                    KeyCode::PageDown => {
                        if let Some(ClientMachineFilesOverlay {
                            view:
                                ClientMachineFilesView::Viewer {
                                    scroll, max_scroll, ..
                                },
                            ..
                        }) = self.machine_files_overlay_mut()
                        {
                            *scroll = scroll.saturating_add(8).min(*max_scroll);
                        }
                        outcome.repaint = true;
                    }
                    _ => {}
                }
                return true;
            }
            Some(ClientMachineFilesView::ConfirmDelete { .. }) => {
                match code {
                    KeyCode::Enter => self.machine_files_confirm_delete(outcome),
                    KeyCode::Esc => self.machine_files_back(outcome),
                    _ => {}
                }
                return true;
            }
            Some(ClientMachineFilesView::Prompt { .. }) => {
                match code {
                    KeyCode::Enter => self.machine_files_submit_prompt(outcome),
                    KeyCode::Esc => self.machine_files_back(outcome),
                    _ => {
                        if let Some(ClientMachineFilesOverlay {
                            view: ClientMachineFilesView::Prompt { input, .. },
                            ..
                        }) = self.machine_files_overlay_mut()
                        {
                            outcome.repaint |= input.handle_key(key).is_some();
                        }
                    }
                }
                return true;
            }
            _ => {}
        }

        // Search focus intercepts most keys while active.
        let search_focused = self
            .machine_files_overlay()
            .is_some_and(|overlay| overlay.search_focused);
        if search_focused {
            match code {
                KeyCode::Esc => {
                    if let Some(overlay) = self.machine_files_overlay_mut() {
                        overlay.search_focused = false;
                    }
                    outcome.repaint = true;
                }
                KeyCode::Enter => {
                    if let Some(overlay) = self.machine_files_overlay_mut() {
                        overlay.search_focused = false;
                    }
                    self.machine_files_open_selected(outcome);
                }
                KeyCode::Up => {
                    self.move_machine_files_selection(-1);
                    outcome.repaint = true;
                }
                KeyCode::Down => {
                    self.move_machine_files_selection(1);
                    outcome.repaint = true;
                }
                _ => {
                    let changed = self.machine_files_overlay_mut().and_then(|overlay| {
                        overlay.query.handle_key(key).inspect(|changed| {
                            if *changed {
                                overlay.selected = 0;
                                overlay.resync_filter_count();
                            }
                        })
                    });
                    if changed.is_some() {
                        outcome.repaint = true;
                    }
                }
            }
            return true;
        }

        match code {
            KeyCode::Esc => self.machine_files_back(outcome),
            KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right if plain => {
                self.machine_files_open_selected(outcome)
            }
            KeyCode::Backspace | KeyCode::Char('h') | KeyCode::Left if plain => {
                let parent = self
                    .machine_files_overlay()
                    .map(|overlay| remote_parent(&overlay.cwd));
                if let Some(parent) = parent {
                    self.machine_files_cd(parent, outcome);
                }
            }
            KeyCode::Up | KeyCode::Char('k') if plain => {
                self.move_machine_files_selection(-1);
                outcome.repaint = true;
            }
            KeyCode::Down | KeyCode::Char('j') if plain => {
                self.move_machine_files_selection(1);
                outcome.repaint = true;
            }
            KeyCode::Char('/') if plain => {
                if let Some(overlay) = self.machine_files_overlay_mut() {
                    overlay.search_focused = true;
                    overlay.message = None;
                }
                outcome.repaint = true;
            }
            KeyCode::Char('r') if plain => {
                self.machine_files_refresh(outcome);
            }
            KeyCode::Char('d') if plain => {
                self.machine_files_start_prompt(MachineFilesPrompt::Download);
                outcome.repaint = true;
            }
            KeyCode::Char('u') if plain => {
                self.machine_files_start_prompt(MachineFilesPrompt::Upload);
                outcome.repaint = true;
            }
            KeyCode::Char('m') if plain => {
                self.machine_files_start_prompt(MachineFilesPrompt::Mkdir);
                outcome.repaint = true;
            }
            KeyCode::Char('R') if modifiers == KeyModifiers::SHIFT => {
                self.machine_files_start_prompt(MachineFilesPrompt::Rename);
                outcome.repaint = true;
            }
            KeyCode::Char('x') if plain => {
                self.machine_files_delete_selected();
                outcome.repaint = true;
            }
            _ => {}
        }
        true
    }

    pub(super) fn activate_machine_files_button(
        &mut self,
        button: MachineFilesButton,
        outcome: &mut ClientShellInput,
    ) {
        match button {
            MachineFilesButton::Up => {
                let parent = self
                    .machine_files_overlay()
                    .map(|overlay| remote_parent(&overlay.cwd));
                if let Some(parent) = parent {
                    self.machine_files_cd(parent, outcome);
                }
            }
            MachineFilesButton::Refresh => self.machine_files_refresh(outcome),
            MachineFilesButton::Download => {
                self.machine_files_start_prompt(MachineFilesPrompt::Download)
            }
            MachineFilesButton::Upload => {
                self.machine_files_start_prompt(MachineFilesPrompt::Upload)
            }
            MachineFilesButton::Mkdir => self.machine_files_start_prompt(MachineFilesPrompt::Mkdir),
            MachineFilesButton::Rename => {
                self.machine_files_start_prompt(MachineFilesPrompt::Rename)
            }
            MachineFilesButton::Delete => self.machine_files_delete_selected(),
            MachineFilesButton::ConfirmDelete => self.machine_files_confirm_delete(outcome),
            MachineFilesButton::CancelDelete | MachineFilesButton::PromptCancel => {
                self.machine_files_back(outcome)
            }
            MachineFilesButton::PromptConfirm => self.machine_files_submit_prompt(outcome),
            MachineFilesButton::Back => self.machine_files_back(outcome),
        }
        outcome.repaint = true;
    }

    /// Mouse click on a file row: select; a second click on the selected row
    /// opens it (enter directory / view file).
    pub(super) fn click_machine_files_row(&mut self, row: usize, outcome: &mut ClientShellInput) {
        let already = self
            .machine_files_overlay()
            .is_some_and(|overlay| overlay.selected == row);
        if let Some(overlay) = self.machine_files_overlay_mut() {
            overlay.selected = row;
        }
        if already {
            self.machine_files_open_selected(outcome);
        }
        outcome.repaint = true;
    }

    pub(super) fn scroll_machine_files_overlay(&mut self, delta: isize) {
        if matches!(
            self.machine_files_overlay().map(|overlay| &overlay.view),
            Some(ClientMachineFilesView::Viewer { .. })
        ) {
            if let Some(ClientMachineFilesOverlay {
                view:
                    ClientMachineFilesView::Viewer {
                        scroll, max_scroll, ..
                    },
                ..
            }) = self.machine_files_overlay_mut()
            {
                // HERDR-MACH-009：滚轮向下不得越过最后一页。
                *scroll = scroll.saturating_add_signed(delta * 3).min(*max_scroll);
            }
        } else {
            self.move_machine_files_selection(delta);
        }
    }

    pub(super) fn insert_machine_files_text(&mut self, text: &str) -> bool {
        let Some(overlay) = self.machine_files_overlay_mut() else {
            return false;
        };
        match &mut overlay.view {
            ClientMachineFilesView::Prompt { input, .. } => input.insert(text),
            _ if overlay.search_focused => {
                let changed = overlay.query.insert(text);
                if changed {
                    overlay.selected = 0;
                    overlay.resync_filter_count();
                }
                changed
            }
            _ => false,
        }
    }
}

// ---------- rendering ----------

fn entry_kind_label(kind: RemoteEntryKind) -> &'static str {
    let t = &crate::i18n::texts().machine_files;
    match kind {
        RemoteEntryKind::Directory => t.kind_directory,
        RemoteEntryKind::File => t.kind_file,
        RemoteEntryKind::Symlink => t.kind_symlink,
        RemoteEntryKind::Other => t.kind_other,
    }
}

#[allow(clippy::too_many_arguments)]
/// 远程文件浮层的弹窗与正文矩形：视图计算阶段与渲染阶段共用（STATE-04）。
pub(super) fn machine_files_geometry(
    area: Rect,
    page_bounds: Option<Rect>,
) -> Option<(Rect, Rect)> {
    let outer = page_bounds
        .map(|rect| rect.intersection(area))
        .or_else(|| crate::ui::modal_rect(area, crate::ui::ModalSize::Large.with_height(22)))?;
    let inner = super::render::panel_inner(outer)?;
    if inner.width < 24 || inner.height < 8 {
        return None;
    }
    let stack = crate::ui::modal_stack_areas(inner, 2, 1, 1, 1);
    Some((outer, stack.content))
}

pub(super) fn render_machine_files_overlay(
    b: &mut Buffer,
    overlay: &ClientMachineFilesOverlay,
    machine_label: &str,
    cx: &super::feedback::ChromeContext<'_>,
) -> Option<OverlayRender> {
    let p = cx.palette;
    let t = &crate::i18n::texts().machine_files;
    let (popup, inner) = modal_panel(b, crate::ui::ModalSize::Large.with_height(22), p.accent, cx)?;
    if inner.width < 24 || inner.height < 8 {
        return Some(OverlayRender {
            area: popup,
            machine_files_popup: popup,
            ..OverlayRender::default()
        });
    }
    let stack = crate::ui::modal_stack_areas(inner, 2, 1, 1, 1);
    let base = Style::default()
        .bg(p.panel_bg)
        .remove_modifier(Modifier::DIM);
    let title = crate::i18n::fill(t.title_fmt, &[("label", machine_label)]);
    put_text(
        b,
        stack.header.x,
        stack.header.y,
        stack.header.width,
        &format!(" {title}"),
        base.fg(p.text).add_modifier(Modifier::BOLD),
    );
    put_right_text(
        b,
        Rect::new(stack.header.x, stack.header.y, stack.header.width, 1),
        stack.header.y,
        &overlay.cwd,
        base.fg(p.overlay0),
    );

    let mut action_hits = Vec::new();
    let mut row_hits = Vec::new();
    let mut cursor = None;
    let body = stack.content;

    match &overlay.view {
        ClientMachineFilesView::Viewer {
            path,
            content,
            line_offsets,
            scroll,
            ..
        } => {
            put_text(
                b,
                stack.header.x,
                stack.header.y + 1,
                stack.header.width,
                &crate::i18n::fill(t.viewer_title_fmt, &[("path", path)]),
                base.fg(p.overlay1),
            );
            let visible = usize::from(body.height).max(1);
            let scroll = (*scroll).min(line_offsets.len().saturating_sub(visible));
            for (offset, (start, len)) in line_offsets.iter().skip(scroll).take(visible).enumerate()
            {
                put_text(
                    b,
                    body.x,
                    body.y + offset as u16,
                    body.width,
                    &content[*start..*start + *len],
                    base.fg(p.text),
                );
            }
            if let Some(footer) = stack.footer {
                render_key_hints(
                    b,
                    footer,
                    &[
                        ("↑↓ PgUp/PgDn".to_owned(), t.hint_scroll.to_owned()),
                        ("esc".to_owned(), t.hint_back.to_owned()),
                    ],
                    p,
                    cx.components,
                );
            }
            let back_label = crate::i18n::texts().overlays.back_button;
            let rects = modal_button_row(stack.actions.unwrap_or_default(), &[back_label], 2);
            if let [back] = rects.as_slice() {
                modal_button(
                    b,
                    *back,
                    back_label,
                    crate::ui::ModalButtonTone::Secondary,
                    cx.button_state(
                        &super::feedback::ChromeHover::MachineFilesButton(MachineFilesButton::Back),
                        crate::ui::ModalButtonState::Focused,
                    ),
                    p,
                );
                action_hits.push((*back, MachineFilesButton::Back));
            }
        }
        ClientMachineFilesView::ConfirmDelete { path, recursive } => {
            put_text(
                b,
                body.x,
                body.y,
                body.width,
                &format!(
                    " {}",
                    crate::i18n::fill(t.confirm_delete_fmt, &[("path", path)])
                ),
                base.fg(p.red).add_modifier(Modifier::BOLD),
            );
            if *recursive {
                put_text(
                    b,
                    body.x,
                    body.y + 1,
                    body.width,
                    &format!(" {}", t.confirm_delete_recursive),
                    base.fg(p.yellow),
                );
            }
            if let Some(error) = overlay.error.as_deref() {
                put_text(
                    b,
                    body.x,
                    body.y + 3,
                    body.width,
                    &crate::i18n::fill(t.error_fmt, &[("error", error)]),
                    base.fg(p.red),
                );
            }
            let confirm_label = crate::i18n::texts().overlays.confirm_button;
            let cancel_label = crate::i18n::texts().overlays.cancel_button;
            let rects = modal_button_row(
                stack.actions.unwrap_or_default(),
                &[confirm_label, cancel_label],
                2,
            );
            if let [confirm, cancel] = rects.as_slice() {
                modal_button(
                    b,
                    *confirm,
                    confirm_label,
                    crate::ui::ModalButtonTone::Danger,
                    cx.button_state(
                        &super::feedback::ChromeHover::MachineFilesButton(
                            MachineFilesButton::ConfirmDelete,
                        ),
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
                        &super::feedback::ChromeHover::MachineFilesButton(
                            MachineFilesButton::CancelDelete,
                        ),
                        crate::ui::ModalButtonState::Normal,
                    ),
                    p,
                );
                action_hits.push((*confirm, MachineFilesButton::ConfirmDelete));
                action_hits.push((*cancel, MachineFilesButton::CancelDelete));
            }
        }
        ClientMachineFilesView::Prompt {
            kind,
            input,
            context,
        } => {
            let prompt_text = match kind {
                MachineFilesPrompt::Download => {
                    crate::i18n::fill(t.prompt_download_fmt, &[("name", context)])
                }
                MachineFilesPrompt::Upload => t.prompt_upload.to_owned(),
                MachineFilesPrompt::Mkdir => t.prompt_mkdir.to_owned(),
                MachineFilesPrompt::Rename => {
                    crate::i18n::fill(t.prompt_rename_fmt, &[("name", context)])
                }
            };
            put_text(
                b,
                body.x,
                body.y,
                body.width,
                &format!(" {prompt_text}"),
                base.fg(p.text),
            );
            let input_rect = Rect::new(body.x, body.y + 2, body.width, 1);
            let field = crate::ui::input_field_style(p);
            b.set_style(input_rect, field);
            cursor = text_editor::render(
                b,
                Rect::new(
                    input_rect.x + 1,
                    input_rect.y,
                    input_rect.width.saturating_sub(2),
                    1,
                ),
                input,
                field,
            );
            let confirm_label = crate::i18n::texts().overlays.confirm_button;
            let cancel_label = crate::i18n::texts().overlays.cancel_button;
            let rects = modal_button_row(
                stack.actions.unwrap_or_default(),
                &[confirm_label, cancel_label],
                2,
            );
            if let [confirm, cancel] = rects.as_slice() {
                modal_button(
                    b,
                    *confirm,
                    confirm_label,
                    crate::ui::ModalButtonTone::Primary,
                    cx.button_state(
                        &super::feedback::ChromeHover::MachineFilesButton(
                            MachineFilesButton::PromptConfirm,
                        ),
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
                        &super::feedback::ChromeHover::MachineFilesButton(
                            MachineFilesButton::PromptCancel,
                        ),
                        crate::ui::ModalButtonState::Normal,
                    ),
                    p,
                );
                action_hits.push((*confirm, MachineFilesButton::PromptConfirm));
                action_hits.push((*cancel, MachineFilesButton::PromptCancel));
            }
            if let Some(footer) = stack.footer {
                render_key_hints(
                    b,
                    footer,
                    &[
                        ("enter".to_owned(), t.hint_confirm.to_owned()),
                        ("esc".to_owned(), t.hint_back.to_owned()),
                    ],
                    p,
                    cx.components,
                );
            }
        }
        ClientMachineFilesView::List => {
            let count = if overlay.pending > 0 && overlay.filtered_count == 0 {
                t.loading.to_owned()
            } else {
                // C-16：计数走缓存（与条目到达/query 编辑同机更新），不再逐帧过滤。
                crate::i18n::fill(
                    t.count_fmt,
                    &[("count", &overlay.filtered_count.to_string())],
                )
            };
            cursor = render_search_bar(
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
            let visible = usize::from(body.height).max(1);
            let selected = if overlay.filtered_count == 0 {
                0
            } else {
                overlay.selected.min(overlay.filtered_count - 1)
            };
            let scroll = selected
                .saturating_sub(visible.saturating_sub(1))
                .min(selected);
            // C-16：只对可见窗口工作，不物化整个目录。
            for (index, entry) in overlay
                .filtered_entries()
                .enumerate()
                .skip(scroll)
                .take(visible)
            {
                let y = body.y + (index - scroll) as u16;
                let rect = Rect::new(body.x, y, body.width, 1);
                row_hits.push((rect, index));
                let is_selected = index == selected;
                let style = if is_selected {
                    Style::default()
                        .fg(panel_contrast_fg(p))
                        .bg(p.accent)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(p.text).bg(p.panel_bg)
                };
                b.set_style(rect, style);
                let name = if entry.kind == RemoteEntryKind::Directory {
                    format!(" {}/", entry.name)
                } else {
                    format!(" {}", entry.name)
                };
                let name_style = if is_selected || entry.kind != RemoteEntryKind::Directory {
                    style
                } else {
                    Style::default().fg(p.blue).bg(p.panel_bg)
                };
                put_text(b, rect.x, rect.y, rect.width, &name, name_style);
                let meta = if entry.modified.is_empty() {
                    format!("{} · {}", entry_kind_label(entry.kind), entry.size)
                } else {
                    format!(
                        "{} · {} · {}",
                        entry_kind_label(entry.kind),
                        entry.size,
                        entry.modified
                    )
                };
                let meta_style = if is_selected {
                    style
                } else {
                    Style::default().fg(p.overlay0).bg(p.panel_bg)
                };
                put_right_text(b, rect, rect.y, &meta, meta_style);
            }
            if overlay.filtered_count == 0 && overlay.pending == 0 {
                let line = if overlay.error.is_some() {
                    None
                } else {
                    Some(t.empty)
                };
                if let Some(line) = line {
                    put_text(b, body.x, body.y, body.width, line, base.fg(p.overlay1));
                }
            }
            if let Some(error) = overlay.error.as_deref() {
                put_text(
                    b,
                    body.x,
                    body.y,
                    body.width,
                    &crate::i18n::fill(t.error_fmt, &[("error", error)]),
                    base.fg(p.red),
                );
            } else if let Some(message) = overlay.message.as_deref() {
                put_text(b, body.x, body.y, body.width, message, base.fg(p.green));
            }
            if let Some(footer) = stack.footer {
                render_key_hints(
                    b,
                    footer,
                    &[
                        ("↑↓".to_owned(), t.hint_select.to_owned()),
                        ("enter".to_owned(), t.hint_open.to_owned()),
                        ("⌫".to_owned(), t.hint_up.to_owned()),
                        ("d".to_owned(), t.download_button.trim().to_owned()),
                        ("u".to_owned(), t.upload_button.trim().to_owned()),
                        ("m".to_owned(), t.mkdir_button.trim().to_owned()),
                        ("R".to_owned(), t.rename_button.trim().to_owned()),
                        ("x".to_owned(), t.delete_button.trim().to_owned()),
                        ("/".to_owned(), t.hint_filter.to_owned()),
                        ("esc".to_owned(), t.hint_back.to_owned()),
                    ],
                    p,
                    cx.components,
                );
            }
            let labels = [
                t.up_button,
                t.refresh_button,
                t.download_button,
                t.upload_button,
                t.mkdir_button,
                t.rename_button,
                t.delete_button,
            ];
            let buttons = [
                MachineFilesButton::Up,
                MachineFilesButton::Refresh,
                MachineFilesButton::Download,
                MachineFilesButton::Upload,
                MachineFilesButton::Mkdir,
                MachineFilesButton::Rename,
                MachineFilesButton::Delete,
            ];
            let rects = modal_button_row(stack.actions.unwrap_or_default(), &labels, 1);
            if rects.len() == labels.len() {
                for (index, rect) in rects.iter().enumerate() {
                    let button = buttons[index];
                    let enabled = !matches!(
                        button,
                        MachineFilesButton::Download
                            | MachineFilesButton::Rename
                            | MachineFilesButton::Delete
                    ) || overlay.filtered_count > 0;
                    let (tone, base_state) = match button {
                        MachineFilesButton::Delete => (
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
                            &super::feedback::ChromeHover::MachineFilesButton(button),
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
        }
    }

    Some(OverlayRender {
        area: popup,
        machine_files_popup: popup,
        machine_files_search: Rect::new(stack.header.x, stack.header.y + 1, stack.header.width, 1),
        machine_files_rows: row_hits,
        machine_files_actions: action_hits,
        cursor,
        ..OverlayRender::default()
    })
}
