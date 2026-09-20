//! Snippet overlay: the client-local command snippet library (list, new/edit
//! form, delete confirmation), the run flow (target mode, pane/machine
//! pickers, `{{variable}}` form, confirmation), and the execution history.
//! Runs go out through the existing endpoint request lane
//! (`pane.send-input` / `pane.send-text`), so no wire or API change is
//! involved; every target resolves independently and outcomes land in the
//! library history plus a summary toast.

use super::*;
use crate::client::endpoint::{
    render_snippet_command, Snippet, SnippetExecution, SnippetId, SnippetLibrary,
};

use super::render::{
    display_width, modal_button, modal_button_row, modal_panel, put_right_text, put_text,
    render_key_hints, render_search_bar, OverlayRender, SearchBar,
};
use crossterm::event::KeyModifiers;

#[derive(Debug)]
pub(super) struct ClientSnippetsOverlay {
    pub(super) view: ClientSnippetsView,
    pub(super) query: TextEditor,
    pub(super) search_focused: bool,
    pub(super) selected: usize,
    pub(super) scroll: usize,
    /// One-shot feedback line shown in the list footer (saved/removed/...).
    pub(super) message: Option<String>,
    /// Opened through the palette's "run snippet…": the title switches and
    /// the empty state points at creating a snippet first.
    pub(super) pick_for_run: bool,
    /// The library, loaded once when the overlay opens and refreshed after
    /// every mutation; the render path never touches the disk.
    pub(super) library: SnippetLibrary,
    /// Library load failure replaces the list body.
    pub(super) library_error: Option<String>,
    /// 列表里最近一次被左键点击的片段身份 + 时刻。hover 会改写 `selected`，
    /// 所以「二次点击才运行」必须用独立的点击痕迹判定，不能看 `selected == row`；
    /// 记身份而不是行号，筛选、删除、保存重排等任何改动列表内容的路径都天然失效，
    /// 不需要逐个出口手工清痕迹。
    pub(super) last_click: Option<(SnippetId, std::time::Instant)>,
}

#[derive(Debug)]
pub(super) enum ClientSnippetsView {
    List,
    Form(Box<ClientSnippetForm>),
    DeleteConfirm(SnippetId),
    /// Target mode choice: current pane / pick one pane / one pane per machine.
    RunTargets(Box<ClientSnippetRunDraft>),
    RunPickPane(Box<ClientSnippetRunDraft>),
    RunPickMachines(Box<ClientSnippetRunDraft>),
    RunVariables(Box<ClientSnippetRunDraft>),
    RunConfirm(Box<ClientSnippetRunDraft>),
    History {
        selected: usize,
        scroll: usize,
    },
}

impl ClientSnippetsView {
    /// 运行流水线中的步序，喂给 `ClientInputContext::overlay_step`：任何视图
    /// 切换都让长按残余的 Repeat 失效，避免一次长按走完「列表 → 目标 → 确认」
    /// 把命令注入 pane。
    pub(super) fn step(&self) -> u32 {
        match self {
            Self::List => 0,
            Self::Form(_) => 1,
            Self::DeleteConfirm(_) => 2,
            Self::RunTargets(_) => 3,
            Self::RunPickPane(_) => 4,
            Self::RunPickMachines(_) => 5,
            Self::RunVariables(_) => 6,
            Self::RunConfirm(_) => 7,
            Self::History { .. } => 8,
        }
    }
}

#[derive(Debug)]
pub(super) struct ClientSnippetForm {
    pub(super) editing: Option<SnippetId>,
    pub(super) focused: usize,
    pub(super) label: TextEditor,
    pub(super) command: TextEditor,
    pub(super) description: TextEditor,
    pub(super) variables: TextEditor,
    pub(super) tags: TextEditor,
    pub(super) error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ClientSnippetTarget {
    pub(super) endpoint_id: ClientEndpointId,
    pub(super) pane_id: String,
    /// History convention: `local` or the saved profile id.
    pub(super) machine: String,
}

/// Everything one run needs: the snippet, resolved targets, variable values,
/// and whether Enter follows the text.
#[derive(Debug)]
pub(super) struct ClientSnippetRunDraft {
    pub(super) snippet: Snippet,
    pub(super) targets: Vec<ClientSnippetTarget>,
    /// Selection inside the target-mode and pane-picker lists.
    pub(super) target_selected: usize,
    /// Checkbox states parallel to the machine picker rows.
    pub(super) machine_selected: Vec<bool>,
    /// Declared variables with their input editors.
    pub(super) variables: Vec<(String, TextEditor)>,
    pub(super) variable_focused: usize,
    pub(super) press_enter: bool,
    pub(super) error: Option<String>,
}

/// Aggregate of one running snippet execution. `pending` counts requests
/// still in flight; when it reaches zero the run finalizes (history + toast).
#[derive(Debug)]
pub(super) struct ClientSnippetRunState {
    pub(super) snippet_id: SnippetId,
    pub(super) label: String,
    pub(super) total: usize,
    pub(super) pending: usize,
    /// (machine, pane_id, error) per resolved target.
    pub(super) outcomes: Vec<(String, String, Option<String>)>,
    pub(super) executed_at: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SnippetOverlayButton {
    New,
    RunSelected,
    EditSelected,
    DeleteSelected,
    History,
    Back,
    Close,
    Save,
    ConfirmDelete,
    CancelDelete,
    RunNow,
}

const FORM_FIELD_COUNT: usize = 5;

fn snippet_matches(snippet: &Snippet, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    query.is_empty()
        || format!(
            "{} {} {} {}",
            snippet.label,
            snippet.command,
            snippet.description.as_deref().unwrap_or_default(),
            snippet.tags.join(" ")
        )
        .to_lowercase()
        .contains(&query)
}

fn filtered_snippets<'a>(library: &'a SnippetLibrary, query: &str) -> Vec<&'a Snippet> {
    library
        .snippets
        .iter()
        .filter(|snippet| snippet_matches(snippet, query))
        .collect()
}

/// One pickable pane row: every pane of every online endpoint (cached
/// snapshots only — no traffic).
pub(super) struct SnippetPaneRow {
    pub(super) endpoint_id: ClientEndpointId,
    pub(super) machine_label: String,
    pub(super) pane_id: String,
    pub(super) pane_label: String,
}

fn endpoint_machine_name(endpoint_id: &ClientEndpointId) -> String {
    match endpoint_id {
        ClientEndpointId::Local => "local".to_owned(),
        ClientEndpointId::Ssh(profile_id) => profile_id.to_string(),
    }
}

fn pane_rows(endpoints: &[ClientShellEndpoint]) -> Vec<SnippetPaneRow> {
    let mut rows = Vec::new();
    for endpoint in endpoints {
        let Some(snapshot) = endpoint.snapshot.as_deref() else {
            continue;
        };
        if endpoint.status != ClientEndpointStatus::Online {
            continue;
        }
        for pane in &snapshot.panes {
            rows.push(SnippetPaneRow {
                endpoint_id: endpoint.endpoint_id.clone(),
                machine_label: endpoint.label.clone(),
                pane_id: pane.pane_id.clone(),
                pane_label: pane
                    .label
                    .clone()
                    .or_else(|| pane.foreground_cwd.clone().or(pane.cwd.clone()))
                    .unwrap_or_else(|| pane.pane_id.clone()),
            });
        }
    }
    rows
}

/// Machine picker rows: online endpoints that have at least one pane.
fn machine_rows(endpoints: &[ClientShellEndpoint]) -> Vec<(ClientEndpointId, String)> {
    endpoints
        .iter()
        .filter(|endpoint| endpoint.status == ClientEndpointStatus::Online)
        .filter(|endpoint| {
            endpoint
                .snapshot
                .as_deref()
                .is_some_and(|snapshot| !snapshot.panes.is_empty())
        })
        .map(|endpoint| (endpoint.endpoint_id.clone(), endpoint.label.clone()))
        .collect()
}

/// The pane a per-machine run goes to: the focused pane, else the first.
fn machine_default_pane(endpoint: &ClientShellEndpoint) -> Option<String> {
    let snapshot = endpoint.snapshot.as_deref()?;
    snapshot
        .focused_pane_id
        .clone()
        .or_else(|| snapshot.panes.first().map(|pane| pane.pane_id.clone()))
}

fn relative_epoch_ago(executed_at: u64) -> String {
    let t = &crate::i18n::texts().history;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let seconds = now.saturating_sub(executed_at);
    if seconds < 60 {
        return t.just_now.to_owned();
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return crate::i18n::fill(t.minutes_ago_fmt, &[("n", &minutes.to_string())]);
    }
    let hours = minutes / 60;
    if hours < 24 {
        return crate::i18n::fill(t.hours_ago_fmt, &[("n", &hours.to_string())]);
    }
    crate::i18n::fill(t.days_ago_fmt, &[("n", &(hours / 24).to_string())])
}

impl ClientSnippetRunDraft {
    fn new(snippet: Snippet) -> Self {
        let variables = snippet
            .variables
            .iter()
            .map(|name| (name.clone(), TextEditor::default()))
            .collect();
        Self {
            snippet,
            targets: Vec::new(),
            target_selected: 0,
            machine_selected: Vec::new(),
            variables,
            variable_focused: 0,
            press_enter: true,
            error: None,
        }
    }

    fn variable_values(&self) -> Vec<(String, String)> {
        self.variables
            .iter()
            .map(|(name, editor)| (name.clone(), editor.as_str().to_owned()))
            .collect()
    }

    fn rendered_command(&self) -> Result<String, String> {
        render_snippet_command(&self.snippet.command, &self.variable_values())
    }
}

impl ClientShellState {
    pub(super) fn open_snippets_overlay(&mut self, pick_for_run: bool) {
        self.chrome_drag = None;
        let (library, library_error) = match SnippetLibrary::load() {
            Ok(library) => (library, None),
            Err(error) => (SnippetLibrary::default(), Some(error)),
        };
        self.overlay = Some(ClientShellOverlay::Snippets(ClientSnippetsOverlay {
            view: ClientSnippetsView::List,
            query: TextEditor::default(),
            search_focused: false,
            selected: 0,
            scroll: 0,
            message: None,
            pick_for_run,
            library,
            library_error,
            last_click: None,
        }));
    }

    /// Reloads the cached library from disk (history entries may have been
    /// written by a run or the CLI while the overlay was open).
    fn reload_snippets_library(&mut self) {
        if let Some(ClientShellOverlay::Snippets(overlay)) = self.overlay.as_mut() {
            match SnippetLibrary::load() {
                Ok(library) => {
                    overlay.library = library;
                    overlay.library_error = None;
                }
                Err(error) => overlay.library_error = Some(error),
            }
        }
    }

    /// Mutates the cached library and persists it; the cache stays in place
    /// on validation/store errors so the form can keep editing.
    fn mutate_snippet_library(
        &mut self,
        mutate: impl FnOnce(&mut SnippetLibrary) -> Result<(), String>,
    ) -> Result<(), String> {
        let Some(ClientShellOverlay::Snippets(overlay)) = self.overlay.as_mut() else {
            return Ok(());
        };
        let mut library = std::mem::take(&mut overlay.library);
        let result = mutate(&mut library).and_then(|()| library.store());
        overlay.library = library;
        result
    }

    fn selected_snippet(&self) -> Option<Snippet> {
        let Some(ClientShellOverlay::Snippets(overlay)) = self.overlay.as_ref() else {
            return None;
        };
        filtered_snippets(&overlay.library, overlay.query.as_str())
            .get(overlay.selected)
            .map(|snippet| (*snippet).clone())
    }

    fn move_snippet_selection(&mut self, delta: isize) {
        let Some(ClientShellOverlay::Snippets(overlay)) = self.overlay.as_mut() else {
            return;
        };
        let count = match &overlay.view {
            ClientSnippetsView::List => {
                filtered_snippets(&overlay.library, overlay.query.as_str()).len()
            }
            ClientSnippetsView::History { .. } => overlay.library.history.len(),
            _ => return,
        };
        match &mut overlay.view {
            ClientSnippetsView::List => {
                overlay.selected = (overlay.selected as isize + delta)
                    .clamp(0, count.saturating_sub(1) as isize)
                    as usize;
            }
            ClientSnippetsView::History { selected, .. } => {
                *selected = (*selected as isize + delta).clamp(0, count.saturating_sub(1) as isize)
                    as usize;
            }
            _ => {}
        }
    }

    fn scroll_snippet_list(&mut self, delta: isize) {
        if let Some(ClientShellOverlay::Snippets(overlay)) = self.overlay.as_mut() {
            overlay.scroll = overlay.scroll.saturating_add_signed(delta);
        }
    }

    fn open_snippet_form(&mut self, editing: Option<SnippetId>) {
        let form = match &editing {
            Some(id) => {
                let Some(ClientShellOverlay::Snippets(overlay)) = self.overlay.as_ref() else {
                    return;
                };
                let Some(snippet) = overlay
                    .library
                    .snippets
                    .iter()
                    .find(|snippet| &snippet.id == id)
                else {
                    return;
                };
                ClientSnippetForm {
                    editing: Some(id.clone()),
                    focused: 0,
                    label: TextEditor::new(&snippet.label, false),
                    command: TextEditor::new(&snippet.command, false),
                    description: TextEditor::new(
                        snippet.description.as_deref().unwrap_or_default(),
                        false,
                    ),
                    variables: TextEditor::new(&snippet.variables.join(", "), false),
                    tags: TextEditor::new(&snippet.tags.join(", "), false),
                    error: None,
                }
            }
            None => ClientSnippetForm {
                editing: None,
                focused: 0,
                label: TextEditor::default(),
                command: TextEditor::default(),
                description: TextEditor::default(),
                variables: TextEditor::default(),
                tags: TextEditor::default(),
                error: None,
            },
        };
        if let Some(ClientShellOverlay::Snippets(overlay)) = self.overlay.as_mut() {
            overlay.view = ClientSnippetsView::Form(Box::new(form));
        }
    }

    fn save_snippet_form(&mut self) {
        enum Saved {
            Added,
            Updated,
            Missing,
        }
        let prepared = {
            let Some(ClientShellOverlay::Snippets(overlay)) = self.overlay.as_mut() else {
                return;
            };
            let Some(form) = (match &mut overlay.view {
                ClientSnippetsView::Form(form) => Some(form),
                _ => None,
            }) else {
                return;
            };
            let split_list = |editor: &TextEditor| -> Vec<String> {
                editor
                    .split(',')
                    .map(str::trim)
                    .filter(|part| !part.is_empty())
                    .map(str::to_owned)
                    .collect()
            };
            (
                form.editing.clone(),
                form.label.trim().to_owned(),
                form.command.trim().to_owned(),
                (!form.description.trim().is_empty()).then(|| form.description.trim().to_owned()),
                split_list(&form.variables),
                split_list(&form.tags),
            )
        };
        let (editing, label, command, description, variables, tags) = prepared;
        let mut result = None;
        let error = self
            .mutate_snippet_library(|library| {
                let saved = match &editing {
                    Some(id) => match library.update_snippet(
                        id,
                        label.clone(),
                        command.clone(),
                        description.clone(),
                        variables.clone(),
                        tags.clone(),
                    ) {
                        Ok(true) => Saved::Updated,
                        Ok(false) => Saved::Missing,
                        Err(error) => return Err(error),
                    },
                    None => {
                        library.add_snippet(
                            label.clone(),
                            command.clone(),
                            description.clone(),
                            variables.clone(),
                            tags.clone(),
                        )?;
                        Saved::Added
                    }
                };
                result = Some(saved);
                Ok(())
            })
            .err();
        let Some(result) = result else {
            if let (Some(error), Some(ClientShellOverlay::Snippets(overlay))) =
                (error, self.overlay.as_mut())
            {
                if let ClientSnippetsView::Form(form) = &mut overlay.view {
                    form.error = Some(error);
                }
            }
            return;
        };
        let t = &crate::i18n::texts().snippets;
        if let Some(ClientShellOverlay::Snippets(overlay)) = self.overlay.as_mut() {
            overlay.view = ClientSnippetsView::List;
            overlay.message = Some(match result {
                Saved::Added | Saved::Updated => t.saved_message.to_owned(),
                // The snippet vanished underneath the form (external edit).
                Saved::Missing => t.removed_message.to_owned(),
            });
        }
    }

    fn delete_selected_snippet(&mut self) {
        let Some(snippet) = self.selected_snippet() else {
            return;
        };
        if let Some(ClientShellOverlay::Snippets(overlay)) = self.overlay.as_mut() {
            overlay.view = ClientSnippetsView::DeleteConfirm(snippet.id.clone());
        }
    }

    fn confirm_delete_snippet(&mut self) {
        let snippet_id = match self.overlay.as_ref() {
            Some(ClientShellOverlay::Snippets(overlay)) => match &overlay.view {
                ClientSnippetsView::DeleteConfirm(id) => id.clone(),
                _ => return,
            },
            _ => return,
        };
        let mut removed = false;
        let stored = self.mutate_snippet_library(|library| {
            removed = library.remove_snippet(&snippet_id);
            Ok(())
        });
        let t = &crate::i18n::texts().snippets;
        if let Some(ClientShellOverlay::Snippets(overlay)) = self.overlay.as_mut() {
            overlay.view = ClientSnippetsView::List;
            overlay.message = match (removed, stored) {
                (true, Ok(())) => Some(t.removed_message.to_owned()),
                (_, Err(error)) => Some(error),
                (false, Ok(())) => None,
            };
        }
    }

    // ----- run flow -----

    fn start_snippet_run_flow(&mut self, snippet: Snippet) {
        if let Some(ClientShellOverlay::Snippets(overlay)) = self.overlay.as_mut() {
            overlay.view =
                ClientSnippetsView::RunTargets(Box::new(ClientSnippetRunDraft::new(snippet)));
        }
    }

    /// Advances the run flow once targets are resolved: variables first when
    /// the snippet declares any, then the confirmation.
    fn advance_run_draft(&mut self) {
        if let Some(ClientShellOverlay::Snippets(overlay)) = self.overlay.as_mut() {
            let view = std::mem::replace(&mut overlay.view, ClientSnippetsView::List);
            overlay.view = match view {
                ClientSnippetsView::RunTargets(draft)
                | ClientSnippetsView::RunPickPane(draft)
                | ClientSnippetsView::RunPickMachines(draft)
                | ClientSnippetsView::RunVariables(draft) => {
                    if draft.targets.is_empty() {
                        ClientSnippetsView::RunTargets(draft)
                    } else if draft.snippet.variables.is_empty() {
                        ClientSnippetsView::RunConfirm(draft)
                    } else {
                        ClientSnippetsView::RunVariables(draft)
                    }
                }
                other => other,
            };
        }
    }

    fn run_draft_pick_target_mode(&mut self, mode: usize) {
        // Mode rows: 0 = current pane, 1 = pick a pane, 2 = one pane per machine.
        match mode {
            0 => {
                let target = self.focused_pane_id().map(|pane_id| ClientSnippetTarget {
                    endpoint_id: self.active_endpoint_id.clone(),
                    machine: endpoint_machine_name(&self.active_endpoint_id),
                    pane_id,
                });
                let Some(target) = target else {
                    return;
                };
                if let Some(ClientShellOverlay::Snippets(overlay)) = self.overlay.as_mut() {
                    if let ClientSnippetsView::RunTargets(draft) = &mut overlay.view {
                        draft.targets = vec![target];
                    }
                }
                self.advance_run_draft();
            }
            1 => {
                if let Some(ClientShellOverlay::Snippets(overlay)) = self.overlay.as_mut() {
                    let view = std::mem::replace(&mut overlay.view, ClientSnippetsView::List);
                    if let ClientSnippetsView::RunTargets(draft) = view {
                        overlay.view = ClientSnippetsView::RunPickPane(draft);
                    }
                }
            }
            2 => {
                let count = machine_rows(&self.endpoints).len();
                if count == 0 {
                    return;
                }
                if let Some(ClientShellOverlay::Snippets(overlay)) = self.overlay.as_mut() {
                    let view = std::mem::replace(&mut overlay.view, ClientSnippetsView::List);
                    if let ClientSnippetsView::RunTargets(mut draft) = view {
                        draft.machine_selected = vec![true; count];
                        overlay.view = ClientSnippetsView::RunPickMachines(draft);
                    }
                }
            }
            _ => {}
        }
    }

    fn run_draft_pick_pane(&mut self, row: usize) {
        let rows = pane_rows(&self.endpoints);
        let Some(row) = rows.get(row) else {
            return;
        };
        let target = ClientSnippetTarget {
            endpoint_id: row.endpoint_id.clone(),
            machine: endpoint_machine_name(&row.endpoint_id),
            pane_id: row.pane_id.clone(),
        };
        if let Some(ClientShellOverlay::Snippets(overlay)) = self.overlay.as_mut() {
            if let ClientSnippetsView::RunPickPane(draft) = &mut overlay.view {
                draft.targets = vec![target];
            }
        }
        self.advance_run_draft();
    }

    fn run_draft_toggle_machine(&mut self, row: usize) {
        if let Some(ClientShellOverlay::Snippets(overlay)) = self.overlay.as_mut() {
            if let ClientSnippetsView::RunPickMachines(draft) = &mut overlay.view {
                if let Some(selected) = draft.machine_selected.get_mut(row) {
                    *selected = !*selected;
                }
            }
        }
    }

    fn run_draft_toggle_all_machines(&mut self) {
        if let Some(ClientShellOverlay::Snippets(overlay)) = self.overlay.as_mut() {
            if let ClientSnippetsView::RunPickMachines(draft) = &mut overlay.view {
                let all = draft.machine_selected.iter().all(|selected| *selected);
                draft.machine_selected.fill(!all);
            }
        }
    }

    fn run_draft_confirm_machines(&mut self) {
        let rows = machine_rows(&self.endpoints);
        let mut targets = Vec::new();
        let mut skipped: Vec<String> = Vec::new();
        let selected = match self.overlay.as_ref() {
            Some(ClientShellOverlay::Snippets(overlay)) => match &overlay.view {
                ClientSnippetsView::RunPickMachines(draft) => draft.machine_selected.clone(),
                _ => return,
            },
            _ => return,
        };
        for ((endpoint_id, label), selected) in rows.iter().zip(selected.iter()) {
            if !selected {
                continue;
            }
            let pane = self
                .endpoints
                .iter()
                .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
                .and_then(machine_default_pane);
            match pane {
                Some(pane_id) => targets.push(ClientSnippetTarget {
                    endpoint_id: endpoint_id.clone(),
                    machine: endpoint_machine_name(endpoint_id),
                    pane_id,
                }),
                None => skipped.push(label.clone()),
            }
        }
        if targets.is_empty() {
            return;
        }
        if let Some(ClientShellOverlay::Snippets(overlay)) = self.overlay.as_mut() {
            if let ClientSnippetsView::RunPickMachines(draft) = &mut overlay.view {
                draft.targets = targets;
                // Machines without a usable pane surface as a note on the
                // confirmation instead of failing the run later.
                draft.error = skipped
                    .first()
                    .map(|label| {
                        crate::i18n::fill(
                            crate::i18n::texts().snippets.target_no_pane_fmt,
                            &[("label", label)],
                        )
                    })
                    .filter(|_| !skipped.is_empty());
            }
        }
        self.advance_run_draft();
    }

    fn run_draft_advance_variables(&mut self) {
        let valid = match self.overlay.as_mut() {
            Some(ClientShellOverlay::Snippets(overlay)) => match &mut overlay.view {
                ClientSnippetsView::RunVariables(draft) => match draft.rendered_command() {
                    Ok(_) => {
                        draft.error = None;
                        true
                    }
                    Err(error) => {
                        draft.error = Some(error);
                        false
                    }
                },
                _ => false,
            },
            _ => false,
        };
        if valid {
            if let Some(ClientShellOverlay::Snippets(overlay)) = self.overlay.as_mut() {
                let view = std::mem::replace(&mut overlay.view, ClientSnippetsView::List);
                if let ClientSnippetsView::RunVariables(draft) = view {
                    overlay.view = ClientSnippetsView::RunConfirm(draft);
                }
            }
        }
    }

    fn snippets_back(&mut self) {
        let Some(ClientShellOverlay::Snippets(overlay)) = self.overlay.as_mut() else {
            return;
        };
        let view = std::mem::replace(&mut overlay.view, ClientSnippetsView::List);
        overlay.view = match view {
            ClientSnippetsView::List => {
                self.overlay = None;
                return;
            }
            ClientSnippetsView::Form(_)
            | ClientSnippetsView::DeleteConfirm(_)
            | ClientSnippetsView::History { .. } => ClientSnippetsView::List,
            ClientSnippetsView::RunTargets(_) => ClientSnippetsView::List,
            ClientSnippetsView::RunPickPane(draft) | ClientSnippetsView::RunPickMachines(draft) => {
                ClientSnippetsView::RunTargets(draft)
            }
            ClientSnippetsView::RunVariables(draft) | ClientSnippetsView::RunConfirm(draft) => {
                ClientSnippetsView::RunTargets(draft)
            }
        };
    }

    /// Executes the confirmed run: every target gets its own pane-input
    /// request through the endpoint lane; offline/unsupported targets resolve
    /// immediately as failures. The overlay closes so the user watches the
    /// panes; the completion toast summarizes per-target outcomes.
    fn execute_snippet_run(&mut self, outcome: &mut ClientShellInput) {
        let draft = match self.overlay.as_mut() {
            Some(ClientShellOverlay::Snippets(overlay)) => match &mut overlay.view {
                ClientSnippetsView::RunConfirm(draft) => match draft.rendered_command() {
                    Ok(text) => {
                        draft.error = None;
                        let snippet = draft.snippet.clone();
                        let draft =
                            std::mem::replace(&mut **draft, ClientSnippetRunDraft::new(snippet));
                        Some((draft, text))
                    }
                    Err(error) => {
                        draft.error = Some(error);
                        None
                    }
                },
                _ => None,
            },
            _ => None,
        };
        let Some((draft, text)) = draft else {
            outcome.repaint = true;
            return;
        };
        let t = &crate::i18n::texts().snippets;
        let method_for = |pane_id: &str| {
            if draft.press_enter {
                crate::api::schema::Method::PaneSendInput(crate::api::schema::PaneSendInputParams {
                    pane_id: pane_id.to_owned(),
                    text: text.clone(),
                    keys: vec!["Enter".into()],
                })
            } else {
                crate::api::schema::Method::PaneSendText(crate::api::schema::PaneSendTextParams {
                    pane_id: pane_id.to_owned(),
                    text: text.clone(),
                })
            }
        };
        let mut run = ClientSnippetRunState {
            snippet_id: draft.snippet.id.clone(),
            label: draft.snippet.label.clone(),
            total: draft.targets.len(),
            pending: 0,
            outcomes: Vec::new(),
            executed_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_secs())
                .unwrap_or(0),
        };
        for target in &draft.targets {
            let method = method_for(&target.pane_id);
            let online = self.endpoint_is_online(&target.endpoint_id);
            let supported =
                online && self.supports_endpoint_method_for(&target.endpoint_id, &method);
            if online && supported {
                let sent = self.push_endpoint_method_for(
                    &target.endpoint_id,
                    method,
                    PendingEndpointKind::SnippetRun {
                        machine: target.machine.clone(),
                        pane_id: target.pane_id.clone(),
                    },
                    outcome,
                );
                if sent {
                    run.pending = run.pending.saturating_add(1);
                    continue;
                }
            }
            let label = self.endpoint_label(&target.endpoint_id).to_owned();
            run.outcomes.push((
                target.machine.clone(),
                target.pane_id.clone(),
                Some(crate::i18n::fill(
                    t.target_offline_fmt,
                    &[("label", &label)],
                )),
            ));
        }
        self.overlay = None;
        if run.pending == 0 {
            self.finish_snippet_run(run, outcome);
        } else {
            self.snippet_run = Some(run);
        }
        outcome.repaint = true;
    }

    /// One run target resolved (response arrived). When the run completes,
    /// history is persisted and the summary toast shown.
    pub(super) fn complete_snippet_run_target(
        &mut self,
        machine: &str,
        pane_id: &str,
        error: Option<String>,
    ) -> bool {
        let Some(run) = self.snippet_run.as_mut() else {
            return false;
        };
        run.outcomes
            .push((machine.to_owned(), pane_id.to_owned(), error));
        run.pending = run.pending.saturating_sub(1);
        if run.pending > 0 {
            return true;
        }
        let Some(run) = self.snippet_run.take() else {
            return false;
        };
        let mut outcome = ClientShellInput::default();
        self.finish_snippet_run(run, &mut outcome);
        true
    }

    fn finish_snippet_run(&mut self, run: ClientSnippetRunState, outcome: &mut ClientShellInput) {
        let failures: Vec<&(String, String, Option<String>)> = run
            .outcomes
            .iter()
            .filter(|(.., error)| error.is_some())
            .collect();
        // Persist per-target history exactly like the CLI runner does.
        let mut store_error = None;
        match SnippetLibrary::load() {
            Ok(mut library) => {
                library.record_executions(
                    run.outcomes
                        .iter()
                        .map(|(machine, pane_id, error)| SnippetExecution {
                            snippet_id: run.snippet_id.clone(),
                            snippet_label: run.label.clone(),
                            machine: machine.clone(),
                            pane_id: pane_id.clone(),
                            executed_at: run.executed_at,
                            success: error.is_none(),
                            error: error.clone(),
                        })
                        .collect(),
                );
                if let Err(error) = library.store() {
                    store_error = Some(error);
                }
            }
            Err(error) => store_error = Some(error),
        }
        let t = &crate::i18n::texts().snippets;
        let ok = run.total.saturating_sub(failures.len());
        let title = crate::i18n::fill(
            t.run_summary_fmt,
            &[
                ("label", &run.label),
                ("ok", &ok.to_string()),
                ("total", &run.total.to_string()),
            ],
        );
        let mut body_parts: Vec<String> = failures
            .iter()
            .map(|(machine, pane_id, error)| {
                format!(
                    "{machine}/{pane_id}: {}",
                    error.as_deref().unwrap_or_default()
                )
            })
            .collect();
        if let Some(error) = store_error {
            body_parts.push(crate::i18n::fill(
                crate::i18n::texts()
                    .cli_errors
                    .snippet_history_store_failed_fmt,
                &[("error", &error)],
            ));
        }
        let (level_kind, body) = if failures.is_empty() && body_parts.is_empty() {
            (ClientEndpointNoticeKind::Success, t.run_ok_body.to_owned())
        } else {
            (
                ClientEndpointNoticeKind::Rejected,
                crate::i18n::fill(
                    t.run_failed_body_fmt,
                    &[("failures", &body_parts.join("; "))],
                ),
            )
        };
        self.push_endpoint_notice(
            level_kind,
            format!("snippet-run:{}", run.executed_at),
            title,
            body,
        );
        outcome.repaint = true;
    }

    // ----- input routing -----

    pub(super) fn insert_snippets_overlay_text(&mut self, text: &str) -> bool {
        let Some(ClientShellOverlay::Snippets(overlay)) = self.overlay.as_mut() else {
            return false;
        };
        match &mut overlay.view {
            ClientSnippetsView::Form(form) => {
                let editor = match form.focused {
                    0 => &mut form.label,
                    1 => &mut form.command,
                    2 => &mut form.description,
                    3 => &mut form.variables,
                    _ => &mut form.tags,
                };
                editor.insert(text)
            }
            ClientSnippetsView::RunVariables(draft) => draft
                .variables
                .get_mut(draft.variable_focused)
                .is_some_and(|(_, editor)| editor.insert(text)),
            _ if overlay.search_focused => overlay.query.insert(text),
            _ => false,
        }
    }

    pub(super) fn route_snippets_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) -> bool {
        if !matches!(self.overlay, Some(ClientShellOverlay::Snippets(_))) {
            return false;
        }
        let (code, modifiers) = crate::config::normalize_key_combo((key.code, key.modifiers));
        let plain = modifiers.is_empty();

        enum ViewKind {
            List,
            Form,
            DeleteConfirm,
            RunTargets,
            RunPickPane,
            RunPickMachines,
            RunVariables,
            RunConfirm,
            History,
        }
        let view = match self.overlay.as_ref() {
            Some(ClientShellOverlay::Snippets(overlay)) => match &overlay.view {
                ClientSnippetsView::List => ViewKind::List,
                ClientSnippetsView::Form(_) => ViewKind::Form,
                ClientSnippetsView::DeleteConfirm(_) => ViewKind::DeleteConfirm,
                ClientSnippetsView::RunTargets(_) => ViewKind::RunTargets,
                ClientSnippetsView::RunPickPane(_) => ViewKind::RunPickPane,
                ClientSnippetsView::RunPickMachines(_) => ViewKind::RunPickMachines,
                ClientSnippetsView::RunVariables(_) => ViewKind::RunVariables,
                ClientSnippetsView::RunConfirm(_) => ViewKind::RunConfirm,
                ClientSnippetsView::History { .. } => ViewKind::History,
            },
            _ => return false,
        };

        match view {
            ViewKind::DeleteConfirm => {
                match code {
                    KeyCode::Enter => self.confirm_delete_snippet(),
                    KeyCode::Esc => self.snippets_back(),
                    _ => {}
                }
                outcome.repaint = true;
                return true;
            }
            ViewKind::History => {
                match code {
                    KeyCode::Esc => self.snippets_back(),
                    KeyCode::Up | KeyCode::Char('k') if plain => self.move_snippet_selection(-1),
                    KeyCode::Down | KeyCode::Char('j') if plain => self.move_snippet_selection(1),
                    _ => {}
                }
                outcome.repaint = true;
                return true;
            }
            ViewKind::Form => {
                self.route_snippet_form_key(key, code, modifiers, outcome);
                return true;
            }
            ViewKind::RunTargets => {
                match code {
                    KeyCode::Esc => self.snippets_back(),
                    KeyCode::Up | KeyCode::Char('k') if plain => {
                        self.run_draft_move_target_selection(-1)
                    }
                    KeyCode::Down | KeyCode::Char('j') if plain => {
                        self.run_draft_move_target_selection(1)
                    }
                    KeyCode::Enter => {
                        let selected = match self.overlay.as_ref() {
                            Some(ClientShellOverlay::Snippets(overlay)) => match &overlay.view {
                                ClientSnippetsView::RunTargets(draft) => draft.target_selected,
                                _ => 0,
                            },
                            _ => 0,
                        };
                        self.run_draft_pick_target_mode(selected);
                    }
                    _ => {}
                }
                outcome.repaint = true;
                return true;
            }
            ViewKind::RunPickPane => {
                match code {
                    KeyCode::Esc => self.snippets_back(),
                    KeyCode::Up | KeyCode::Char('k') if plain => {
                        self.run_draft_move_target_selection(-1)
                    }
                    KeyCode::Down | KeyCode::Char('j') if plain => {
                        self.run_draft_move_target_selection(1)
                    }
                    KeyCode::Enter => {
                        let selected = match self.overlay.as_ref() {
                            Some(ClientShellOverlay::Snippets(overlay)) => match &overlay.view {
                                ClientSnippetsView::RunPickPane(draft) => draft.target_selected,
                                _ => 0,
                            },
                            _ => 0,
                        };
                        self.run_draft_pick_pane(selected);
                    }
                    _ => {}
                }
                outcome.repaint = true;
                return true;
            }
            ViewKind::RunPickMachines => {
                match code {
                    KeyCode::Esc => self.snippets_back(),
                    KeyCode::Up | KeyCode::Char('k') if plain => {
                        self.run_draft_move_target_selection(-1)
                    }
                    KeyCode::Down | KeyCode::Char('j') if plain => {
                        self.run_draft_move_target_selection(1)
                    }
                    KeyCode::Char(' ') if plain => {
                        let selected = match self.overlay.as_ref() {
                            Some(ClientShellOverlay::Snippets(overlay)) => match &overlay.view {
                                ClientSnippetsView::RunPickMachines(draft) => draft.target_selected,
                                _ => 0,
                            },
                            _ => 0,
                        };
                        self.run_draft_toggle_machine(selected);
                    }
                    KeyCode::Char('a') if plain => self.run_draft_toggle_all_machines(),
                    KeyCode::Enter => self.run_draft_confirm_machines(),
                    _ => {}
                }
                outcome.repaint = true;
                return true;
            }
            ViewKind::RunVariables => {
                self.route_snippet_variables_key(key, code, modifiers, outcome);
                return true;
            }
            ViewKind::RunConfirm => {
                match code {
                    KeyCode::Esc => self.snippets_back(),
                    KeyCode::Char(' ') if plain => {
                        if let Some(ClientShellOverlay::Snippets(overlay)) = self.overlay.as_mut() {
                            if let ClientSnippetsView::RunConfirm(draft) = &mut overlay.view {
                                draft.press_enter = !draft.press_enter;
                            }
                        }
                    }
                    KeyCode::Enter => self.execute_snippet_run(outcome),
                    _ => {}
                }
                outcome.repaint = true;
                return true;
            }
            ViewKind::List => {}
        }

        // List view.
        let search_focused = matches!(
            self.overlay,
            Some(ClientShellOverlay::Snippets(ClientSnippetsOverlay {
                search_focused: true,
                ..
            }))
        );
        if search_focused {
            if code == KeyCode::Esc {
                if let Some(ClientShellOverlay::Snippets(overlay)) = self.overlay.as_mut() {
                    overlay.search_focused = false;
                }
                outcome.repaint = true;
                return true;
            }
            if code == KeyCode::Enter {
                if let Some(snippet) = self.selected_snippet() {
                    self.start_snippet_run_flow(snippet);
                }
                outcome.repaint = true;
                return true;
            }
            if code == KeyCode::Up {
                self.move_snippet_selection(-1);
                outcome.repaint = true;
                return true;
            }
            if code == KeyCode::Down {
                self.move_snippet_selection(1);
                outcome.repaint = true;
                return true;
            }
            if let Some(ClientShellOverlay::Snippets(overlay)) = self.overlay.as_mut() {
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
                if let Some(ClientShellOverlay::Snippets(overlay)) = self.overlay.as_mut() {
                    overlay.search_focused = true;
                    overlay.message = None;
                }
                outcome.repaint = true;
            }
            KeyCode::Enter => {
                if let Some(snippet) = self.selected_snippet() {
                    self.start_snippet_run_flow(snippet);
                }
                outcome.repaint = true;
            }
            KeyCode::Up | KeyCode::Char('k') if plain => {
                self.move_snippet_selection(-1);
                outcome.repaint = true;
            }
            KeyCode::Down | KeyCode::Char('j') if plain => {
                self.move_snippet_selection(1);
                outcome.repaint = true;
            }
            KeyCode::Char('n') if plain => {
                self.open_snippet_form(None);
                outcome.repaint = true;
            }
            KeyCode::Char('e') if plain => {
                let id = self.selected_snippet().map(|snippet| snippet.id);
                if let Some(id) = id {
                    self.open_snippet_form(Some(id));
                }
                outcome.repaint = true;
            }
            KeyCode::Char('x') if plain => {
                self.delete_selected_snippet();
                outcome.repaint = true;
            }
            KeyCode::Char('h') if plain => {
                self.reload_snippets_library();
                if let Some(ClientShellOverlay::Snippets(overlay)) = self.overlay.as_mut() {
                    overlay.view = ClientSnippetsView::History {
                        selected: 0,
                        scroll: 0,
                    };
                }
                outcome.repaint = true;
            }
            _ => {}
        }
        true
    }

    fn run_draft_move_target_selection(&mut self, delta: isize) {
        let count = match self.overlay.as_ref() {
            Some(ClientShellOverlay::Snippets(overlay)) => match &overlay.view {
                ClientSnippetsView::RunTargets(_) => 3,
                ClientSnippetsView::RunPickPane(_) => pane_rows(&self.endpoints).len(),
                ClientSnippetsView::RunPickMachines(_) => machine_rows(&self.endpoints).len(),
                _ => 0,
            },
            _ => 0,
        };
        if count == 0 {
            return;
        }
        if let Some(ClientShellOverlay::Snippets(overlay)) = self.overlay.as_mut() {
            let selected = match &mut overlay.view {
                ClientSnippetsView::RunTargets(draft)
                | ClientSnippetsView::RunPickPane(draft)
                | ClientSnippetsView::RunPickMachines(draft) => &mut draft.target_selected,
                _ => return,
            };
            *selected =
                (*selected as isize + delta).clamp(0, count.saturating_sub(1) as isize) as usize;
        }
    }

    fn route_snippet_form_key(
        &mut self,
        key: &crate::input::TerminalKey,
        code: KeyCode,
        modifiers: KeyModifiers,
        outcome: &mut ClientShellInput,
    ) {
        let plain = modifiers.is_empty();
        if code == KeyCode::Esc {
            self.snippets_back();
            outcome.repaint = true;
            return;
        }
        if code == KeyCode::Enter {
            self.save_snippet_form();
            outcome.repaint = true;
            return;
        }
        let Some(ClientShellOverlay::Snippets(overlay)) = self.overlay.as_mut() else {
            return;
        };
        let ClientSnippetsView::Form(form) = &mut overlay.view else {
            return;
        };
        match code {
            KeyCode::Tab if plain => {
                form.focused = (form.focused + 1) % FORM_FIELD_COUNT;
                outcome.repaint = true;
            }
            KeyCode::BackTab if modifiers.difference(KeyModifiers::SHIFT).is_empty() => {
                form.focused = (form.focused + FORM_FIELD_COUNT - 1) % FORM_FIELD_COUNT;
                outcome.repaint = true;
            }
            KeyCode::Up if plain => {
                form.focused = form.focused.saturating_sub(1);
                outcome.repaint = true;
            }
            KeyCode::Down if plain => {
                form.focused = (form.focused + 1).min(FORM_FIELD_COUNT - 1);
                outcome.repaint = true;
            }
            _ => {
                let editor = match form.focused {
                    0 => &mut form.label,
                    1 => &mut form.command,
                    2 => &mut form.description,
                    3 => &mut form.variables,
                    _ => &mut form.tags,
                };
                outcome.repaint |= editor.handle_key(key).is_some();
            }
        }
    }

    fn route_snippet_variables_key(
        &mut self,
        key: &crate::input::TerminalKey,
        code: KeyCode,
        modifiers: KeyModifiers,
        outcome: &mut ClientShellInput,
    ) {
        let plain = modifiers.is_empty();
        if code == KeyCode::Esc {
            self.snippets_back();
            outcome.repaint = true;
            return;
        }
        if code == KeyCode::Enter {
            self.run_draft_advance_variables();
            outcome.repaint = true;
            return;
        }
        let Some(ClientShellOverlay::Snippets(overlay)) = self.overlay.as_mut() else {
            return;
        };
        let ClientSnippetsView::RunVariables(draft) = &mut overlay.view else {
            return;
        };
        let count = draft.variables.len();
        if count == 0 {
            return;
        }
        match code {
            KeyCode::Tab if plain => {
                draft.variable_focused = (draft.variable_focused + 1) % count;
                outcome.repaint = true;
            }
            KeyCode::BackTab if modifiers.difference(KeyModifiers::SHIFT).is_empty() => {
                draft.variable_focused = (draft.variable_focused + count - 1) % count;
                outcome.repaint = true;
            }
            KeyCode::Up if plain => {
                draft.variable_focused = draft.variable_focused.saturating_sub(1);
                outcome.repaint = true;
            }
            KeyCode::Down if plain => {
                draft.variable_focused = (draft.variable_focused + 1).min(count - 1);
                outcome.repaint = true;
            }
            _ => {
                if let Some((_, editor)) = draft.variables.get_mut(draft.variable_focused) {
                    outcome.repaint |= editor.handle_key(key).is_some();
                }
            }
        }
    }

    /// Mouse activation for one rendered snippets-overlay button.
    pub(super) fn activate_snippet_button(
        &mut self,
        button: SnippetOverlayButton,
        outcome: &mut ClientShellInput,
    ) {
        use SnippetOverlayButton as Btn;
        match button {
            Btn::New => self.open_snippet_form(None),
            Btn::RunSelected => {
                if let Some(snippet) = self.selected_snippet() {
                    self.start_snippet_run_flow(snippet);
                }
            }
            Btn::EditSelected => {
                let id = self.selected_snippet().map(|snippet| snippet.id);
                if let Some(id) = id {
                    self.open_snippet_form(Some(id));
                }
            }
            Btn::DeleteSelected => self.delete_selected_snippet(),
            Btn::History => {
                self.reload_snippets_library();
                if let Some(ClientShellOverlay::Snippets(overlay)) = self.overlay.as_mut() {
                    overlay.view = ClientSnippetsView::History {
                        selected: 0,
                        scroll: 0,
                    };
                }
            }
            Btn::Back => self.snippets_back(),
            Btn::Close => {
                self.overlay = None;
            }
            Btn::Save => self.save_snippet_form(),
            Btn::ConfirmDelete => self.confirm_delete_snippet(),
            Btn::CancelDelete => self.snippets_back(),
            Btn::RunNow => self.execute_snippet_run(outcome),
        }
        outcome.repaint = true;
    }

    /// Mouse click on a list/picker row, identified by its rendered index.
    pub(super) fn click_snippet_row(&mut self, row: usize, outcome: &mut ClientShellInput) {
        let double_click_window = self.config.double_click_window;
        // 列表分支先按行号解析出被点的片段身份：点击痕迹记身份，行序变化
        // （筛选、删除、保存后重排）自然不会让上一次点击落到别的片段上。
        let clicked = match self.overlay.as_ref() {
            Some(ClientShellOverlay::Snippets(overlay))
                if matches!(overlay.view, ClientSnippetsView::List) =>
            {
                filtered_snippets(&overlay.library, overlay.query.as_str())
                    .get(row)
                    .map(|snippet| snippet.id.clone())
            }
            _ => None,
        };
        let Some(ClientShellOverlay::Snippets(overlay)) = self.overlay.as_mut() else {
            return;
        };
        match &mut overlay.view {
            ClientSnippetsView::List => {
                // 指针路过就会把 `selected` 拉到该行，所以只有独立记录的同一个
                // 片段的点击痕迹才能算「二次点击」。
                let now = std::time::Instant::now();
                let second_click = match (clicked.as_ref(), overlay.last_click.as_ref()) {
                    (Some(id), Some((last_id, at))) => {
                        last_id == id && now.duration_since(*at) <= double_click_window
                    }
                    _ => false,
                };
                overlay.selected = row;
                overlay.last_click = clicked.map(|id| (id, now));
                if second_click {
                    // 运行后清痕迹，第三次点击重新从选中开始。
                    overlay.last_click = None;
                    if let Some(snippet) = self.selected_snippet() {
                        self.start_snippet_run_flow(snippet);
                    }
                }
            }
            ClientSnippetsView::RunTargets(draft) => {
                draft.target_selected = row;
                self.run_draft_pick_target_mode(row);
                outcome.repaint = true;
                return;
            }
            ClientSnippetsView::RunPickPane(draft) => {
                draft.target_selected = row;
                self.run_draft_pick_pane(row);
                outcome.repaint = true;
                return;
            }
            ClientSnippetsView::RunPickMachines(draft) => {
                draft.target_selected = row;
                self.run_draft_toggle_machine(row);
                outcome.repaint = true;
                return;
            }
            ClientSnippetsView::History { selected, .. } => {
                *selected = row;
            }
            _ => {}
        }
        outcome.repaint = true;
    }

    /// Hover over a list/picker row selects it without activating; returns
    /// true when the selection moved.
    pub(super) fn hover_snippet_row(&mut self, index: usize) -> bool {
        let Some(ClientShellOverlay::Snippets(overlay)) = self.overlay.as_mut() else {
            return false;
        };
        let selected = match &mut overlay.view {
            ClientSnippetsView::List => &mut overlay.selected,
            ClientSnippetsView::RunTargets(draft)
            | ClientSnippetsView::RunPickPane(draft)
            | ClientSnippetsView::RunPickMachines(draft) => &mut draft.target_selected,
            ClientSnippetsView::History { selected, .. } => selected,
            _ => return false,
        };
        let changed = *selected != index;
        if changed {
            *selected = index;
        }
        changed
    }

    pub(super) fn scroll_snippets_overlay(&mut self, delta: isize) {
        let Some(ClientShellOverlay::Snippets(overlay)) = self.overlay.as_ref() else {
            return;
        };
        match &overlay.view {
            ClientSnippetsView::List | ClientSnippetsView::History { .. } => {
                self.move_snippet_selection(delta)
            }
            ClientSnippetsView::RunTargets(_)
            | ClientSnippetsView::RunPickPane(_)
            | ClientSnippetsView::RunPickMachines(_) => self.run_draft_move_target_selection(delta),
            _ => self.scroll_snippet_list(delta),
        }
    }
}

// ---------- rendering ----------

pub(super) fn render_snippets_overlay(
    b: &mut Buffer,
    overlay: &ClientSnippetsOverlay,
    endpoints: &[ClientShellEndpoint],
    saved_profiles: &[SavedSshEndpoint],
    cx: &super::feedback::ChromeContext<'_>,
) -> Option<OverlayRender> {
    match &overlay.view {
        ClientSnippetsView::List => render_snippet_list(b, overlay, cx),
        ClientSnippetsView::Form(form) => render_snippet_form(b, form, cx),
        ClientSnippetsView::DeleteConfirm(id) => {
            render_snippet_delete_confirm(b, id, &overlay.library, cx)
        }
        ClientSnippetsView::RunTargets(draft) => render_run_targets(b, draft, cx),
        ClientSnippetsView::RunPickPane(draft) => render_run_pick_pane(b, draft, endpoints, cx),
        ClientSnippetsView::RunPickMachines(draft) => {
            render_run_pick_machines(b, draft, endpoints, cx)
        }
        ClientSnippetsView::RunVariables(draft) => render_run_variables(b, draft, cx),
        ClientSnippetsView::RunConfirm(draft) => {
            render_run_confirm(b, draft, endpoints, saved_profiles, cx)
        }
        ClientSnippetsView::History { selected, scroll } => {
            render_snippet_history(b, overlay, *selected, *scroll, saved_profiles, cx)
        }
    }
}

fn snippets_panel(
    b: &mut Buffer,
    height: u16,
    cx: &super::feedback::ChromeContext<'_>,
) -> Option<(Rect, Rect)> {
    modal_panel(
        b,
        crate::ui::ModalSize::Large.with_height(height),
        cx.palette.accent,
        cx,
    )
}

fn snippet_footer_hints(
    b: &mut Buffer,
    footer: Option<Rect>,
    hints: &[(String, String)],
    cx: &super::feedback::ChromeContext<'_>,
) {
    if let Some(footer) = footer {
        render_key_hints(b, footer, hints, cx.palette, cx.components);
    }
}

fn render_snippet_list(
    b: &mut Buffer,
    overlay: &ClientSnippetsOverlay,
    cx: &super::feedback::ChromeContext<'_>,
) -> Option<OverlayRender> {
    let p = cx.palette;
    let t = &crate::i18n::texts().snippets;
    let (popup, inner) = snippets_panel(b, 22, cx)?;
    if inner.width < 24 || inner.height < 8 {
        return Some(OverlayRender {
            area: popup,
            ..OverlayRender::default()
        });
    }
    let stack = crate::ui::modal_stack_areas(inner, 2, 1, 1, 1);
    let base = Style::default()
        .bg(p.panel_bg)
        .remove_modifier(Modifier::DIM);
    let title = if overlay.pick_for_run {
        t.run_pick_title
    } else {
        t.title
    };
    put_text(
        b,
        stack.header.x,
        stack.header.y,
        stack.header.width,
        &format!(" {title}"),
        base.fg(p.text).add_modifier(Modifier::BOLD),
    );
    let rows: Vec<&Snippet> = filtered_snippets(&overlay.library, overlay.query.as_str());
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
    let max_scroll = rows.len().saturating_sub(visible);
    let scroll = overlay
        .scroll
        .max(selected.saturating_sub(visible.saturating_sub(1)))
        .min(selected)
        .min(max_scroll);
    let mut row_hits = Vec::new();
    if let Some(error) = overlay.library_error.as_deref() {
        put_text(
            b,
            body.x,
            body.y,
            body.width,
            &format!(" {error}"),
            base.fg(p.red),
        );
    } else if rows.is_empty() {
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
    for (index, snippet) in rows.iter().enumerate().skip(scroll).take(visible) {
        let y = body.y + ((index - scroll) * row_height) as u16;
        let rect = Rect::new(body.x, y, body.width, row_height as u16);
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
        put_text(
            b,
            rect.x,
            rect.y,
            rect.width,
            &format!(" {}", snippet.label),
            style,
        );
        if !snippet.tags.is_empty() {
            let tags = snippet.tags.join(" ");
            put_right_text(b, rect, rect.y, &tags, style);
        }
        let meta_style = if is_selected {
            style
        } else {
            Style::default().fg(p.overlay0).bg(p.panel_bg)
        };
        put_text(
            b,
            rect.x,
            rect.y + 1,
            rect.width,
            &format!("   {}", snippet.command),
            meta_style,
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
            snippet_footer_hints(
                b,
                Some(footer),
                &[
                    ("↑↓".to_owned(), t.hint_select.to_owned()),
                    ("enter".to_owned(), t.hint_run.to_owned()),
                    ("n".to_owned(), t.hint_new.to_owned()),
                    ("e".to_owned(), t.hint_edit.to_owned()),
                    ("x".to_owned(), t.hint_delete.to_owned()),
                    ("h".to_owned(), t.hint_history.to_owned()),
                    ("/".to_owned(), t.search_hint.trim().to_owned()),
                    ("esc".to_owned(), t.hint_close.to_owned()),
                ],
                cx,
            );
        }
    }

    let mut action_hits = Vec::new();
    let back_label = crate::i18n::texts().overlays.back_button;
    let close_label = crate::ui::modal_close_button_text();
    let has_selection = !rows.is_empty();
    let button_specs: [(&str, SnippetOverlayButton, bool, crate::ui::ModalButtonTone); 6] = [
        (
            t.run_button,
            SnippetOverlayButton::RunSelected,
            has_selection,
            crate::ui::ModalButtonTone::Primary,
        ),
        (
            t.new_button,
            SnippetOverlayButton::New,
            true,
            crate::ui::ModalButtonTone::Primary,
        ),
        (
            t.edit_button,
            SnippetOverlayButton::EditSelected,
            has_selection,
            crate::ui::ModalButtonTone::Secondary,
        ),
        (
            t.delete_button,
            SnippetOverlayButton::DeleteSelected,
            has_selection,
            crate::ui::ModalButtonTone::Danger,
        ),
        (
            t.history_button,
            SnippetOverlayButton::History,
            true,
            crate::ui::ModalButtonTone::Secondary,
        ),
        (
            close_label,
            SnippetOverlayButton::Close,
            true,
            crate::ui::ModalButtonTone::Secondary,
        ),
    ];
    let _ = back_label;
    let labels: Vec<&str> = button_specs.iter().map(|(label, ..)| *label).collect();
    let rects = modal_button_row(stack.actions.unwrap_or_default(), &labels, 1);
    if rects.len() == labels.len() {
        for (index, rect) in rects.iter().enumerate() {
            let (label, button, enabled, tone) = button_specs[index];
            let base_state = if !enabled {
                crate::ui::ModalButtonState::Disabled
            } else if matches!(button, SnippetOverlayButton::RunSelected) {
                crate::ui::ModalButtonState::Focused
            } else {
                crate::ui::ModalButtonState::Normal
            };
            let state = if enabled {
                cx.button_state(
                    &super::feedback::ChromeHover::SnippetButton(button),
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
        snippet_popup: popup,
        snippet_search: Rect::new(stack.header.x, stack.header.y + 1, stack.header.width, 1),
        snippet_rows: row_hits,
        snippet_actions: action_hits,
        cursor,
        ..OverlayRender::default()
    })
}

fn render_snippet_form(
    b: &mut Buffer,
    form: &ClientSnippetForm,
    cx: &super::feedback::ChromeContext<'_>,
) -> Option<OverlayRender> {
    let p = cx.palette;
    let t = &crate::i18n::texts().snippets;
    let editing = form.editing.is_some();
    let (popup, inner) = snippets_panel(b, 13, cx)?;
    if inner.width < 24 || inner.height < 8 {
        return Some(OverlayRender {
            area: popup,
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
        &format!(" {}", if editing { t.edit_title } else { t.new_title }),
        base.fg(p.text).add_modifier(Modifier::BOLD),
    );

    let fields: [(&str, &TextEditor, Option<&str>); FORM_FIELD_COUNT] = [
        (t.field_label, &form.label, None),
        (t.field_command, &form.command, None),
        (t.field_description, &form.description, None),
        (t.field_variables, &form.variables, Some(t.hint_variables)),
        (t.field_tags, &form.tags, Some(t.hint_tags)),
    ];
    let body = stack.content;
    let mut field_hits = Vec::new();
    let mut cursor = None;
    let focused = form.focused.min(FORM_FIELD_COUNT - 1);
    for (index, (label, editor, hint)) in fields.iter().enumerate() {
        let y = body.y + index as u16;
        if y >= body.bottom() {
            break;
        }
        let rect = Rect::new(body.x, y, body.width, 1);
        field_hits.push((rect, index));
        let is_focused = index == focused;
        let label_width = 14u16.min(body.width);
        put_text(
            b,
            rect.x,
            rect.y,
            label_width,
            &format!(" {label:<12}"),
            base.fg(if is_focused { p.text } else { p.overlay0 }),
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
            editor,
            Style::default().fg(p.text).bg(p.surface0),
        );
        if is_focused {
            cursor = field_cursor;
            if let Some(hint) = hint {
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

    snippet_footer_hints(
        b,
        stack.footer,
        &[
            ("tab/↑↓".to_owned(), t.hint_fields.to_owned()),
            ("enter".to_owned(), t.hint_confirm.to_owned()),
            ("esc".to_owned(), t.hint_back.to_owned()),
        ],
        cx,
    );

    let mut action_hits = Vec::new();
    let labels = [
        crate::i18n::texts().overlays.save_button,
        crate::i18n::texts().overlays.cancel_button,
    ];
    let rects = modal_button_row(stack.actions.unwrap_or_default(), &labels, 2);
    if let [save, cancel] = rects.as_slice() {
        modal_button(
            b,
            *save,
            labels[0],
            crate::ui::ModalButtonTone::Primary,
            cx.button_state(
                &super::feedback::ChromeHover::SnippetButton(SnippetOverlayButton::Save),
                crate::ui::ModalButtonState::Focused,
            ),
            p,
        );
        modal_button(
            b,
            *cancel,
            labels[1],
            crate::ui::ModalButtonTone::Secondary,
            cx.button_state(
                &super::feedback::ChromeHover::SnippetButton(SnippetOverlayButton::Back),
                crate::ui::ModalButtonState::Normal,
            ),
            p,
        );
        action_hits.push((*save, SnippetOverlayButton::Save));
        action_hits.push((*cancel, SnippetOverlayButton::Back));
    }
    Some(OverlayRender {
        area: popup,
        snippet_popup: popup,
        snippet_fields: field_hits,
        snippet_actions: action_hits,
        cursor,
        ..OverlayRender::default()
    })
}

fn render_snippet_delete_confirm(
    b: &mut Buffer,
    snippet_id: &SnippetId,
    library: &SnippetLibrary,
    cx: &super::feedback::ChromeContext<'_>,
) -> Option<OverlayRender> {
    let p = cx.palette;
    let t = &crate::i18n::texts().snippets;
    let label = library
        .snippets
        .iter()
        .find(|snippet| &snippet.id == snippet_id)
        .map(|snippet| snippet.label.clone())
        .unwrap_or_else(|| snippet_id.to_string());
    let (popup, inner) = modal_panel(b, crate::ui::ModalSize::Medium.with_height(6), p.red, cx)?;
    let stack = crate::ui::modal_stack_areas(inner, 2, 0, 1, 0);
    put_text(
        b,
        stack.header.x,
        stack.header.y,
        stack.header.width,
        &format!(
            " {}",
            crate::i18n::fill(t.delete_title_fmt, &[("label", &label)])
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
        &format!(" {}", t.delete_detail),
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
            &super::feedback::ChromeHover::SnippetButton(SnippetOverlayButton::ConfirmDelete),
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
            &super::feedback::ChromeHover::SnippetButton(SnippetOverlayButton::CancelDelete),
            crate::ui::ModalButtonState::Normal,
        ),
        p,
    );
    Some(OverlayRender {
        area: popup,
        snippet_popup: popup,
        snippet_actions: vec![
            (*confirm, SnippetOverlayButton::ConfirmDelete),
            (*cancel, SnippetOverlayButton::CancelDelete),
        ],
        ..OverlayRender::default()
    })
}

fn run_title(snippet: &Snippet) -> String {
    crate::i18n::fill(
        crate::i18n::texts().snippets.run_title_fmt,
        &[("label", &snippet.label)],
    )
}

fn render_run_targets(
    b: &mut Buffer,
    draft: &ClientSnippetRunDraft,
    cx: &super::feedback::ChromeContext<'_>,
) -> Option<OverlayRender> {
    let p = cx.palette;
    let t = &crate::i18n::texts().snippets;
    let (popup, inner) = snippets_panel(b, 10, cx)?;
    if inner.width < 24 || inner.height < 6 {
        return Some(OverlayRender {
            area: popup,
            ..OverlayRender::default()
        });
    }
    let stack = crate::ui::modal_stack_areas(inner, 2, 1, 0, 1);
    let base = Style::default()
        .bg(p.panel_bg)
        .remove_modifier(Modifier::DIM);
    put_text(
        b,
        stack.header.x,
        stack.header.y,
        stack.header.width,
        &format!(" {}", run_title(&draft.snippet)),
        base.fg(p.text).add_modifier(Modifier::BOLD),
    );
    put_text(
        b,
        stack.header.x,
        stack.header.y + 1,
        stack.header.width,
        &format!(" {}", t.target_title),
        base.fg(p.overlay0),
    );
    let current_pane = crate::i18n::fill(t.target_current_fmt, &[("pane", "focused")]);
    let options = [
        current_pane,
        t.target_pick_pane.to_owned(),
        t.target_machines.to_owned(),
    ];
    let body = stack.content;
    let selected = draft.target_selected.min(options.len() - 1);
    let mut row_hits = Vec::new();
    for (index, label) in options.iter().enumerate() {
        let y = body.y + index as u16;
        if y >= body.bottom() {
            break;
        }
        let rect = Rect::new(body.x, y, body.width, 1);
        row_hits.push((rect, index));
        let style = if index == selected {
            Style::default()
                .fg(panel_contrast_fg(p))
                .bg(p.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(p.text).bg(p.panel_bg)
        };
        b.set_style(rect, style);
        put_text(b, rect.x, rect.y, rect.width, &format!(" {label}"), style);
    }
    snippet_footer_hints(
        b,
        stack.footer,
        &[
            ("↑↓".to_owned(), t.hint_select.to_owned()),
            ("enter".to_owned(), t.hint_continue.to_owned()),
            ("esc".to_owned(), t.hint_back.to_owned()),
        ],
        cx,
    );
    Some(OverlayRender {
        area: popup,
        snippet_popup: popup,
        snippet_rows: row_hits,
        ..OverlayRender::default()
    })
}

fn render_run_pick_pane(
    b: &mut Buffer,
    draft: &ClientSnippetRunDraft,
    endpoints: &[ClientShellEndpoint],
    cx: &super::feedback::ChromeContext<'_>,
) -> Option<OverlayRender> {
    let p = cx.palette;
    let t = &crate::i18n::texts().snippets;
    let (popup, inner) = snippets_panel(b, 18, cx)?;
    if inner.width < 24 || inner.height < 8 {
        return Some(OverlayRender {
            area: popup,
            ..OverlayRender::default()
        });
    }
    let stack = crate::ui::modal_stack_areas(inner, 2, 1, 0, 1);
    let base = Style::default()
        .bg(p.panel_bg)
        .remove_modifier(Modifier::DIM);
    put_text(
        b,
        stack.header.x,
        stack.header.y,
        stack.header.width,
        &format!(" {}", run_title(&draft.snippet)),
        base.fg(p.text).add_modifier(Modifier::BOLD),
    );
    put_text(
        b,
        stack.header.x,
        stack.header.y + 1,
        stack.header.width,
        &format!(" {}", t.pane_picker_title),
        base.fg(p.overlay0),
    );
    let rows = pane_rows(endpoints);
    let body = stack.content;
    let visible = usize::from(body.height).max(1);
    let selected = draft.target_selected.min(rows.len().saturating_sub(1));
    let scroll = selected
        .saturating_sub(visible.saturating_sub(1))
        .min(selected);
    let mut row_hits = Vec::new();
    if rows.is_empty() {
        put_text(
            b,
            body.x,
            body.y,
            body.width,
            t.picker_empty,
            base.fg(p.overlay0),
        );
    }
    for (index, row) in rows.iter().enumerate().skip(scroll).take(visible) {
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
        put_text(
            b,
            rect.x,
            rect.y,
            rect.width,
            &format!(" {} · {}", row.machine_label, row.pane_label),
            style,
        );
        let id_style = if is_selected {
            style
        } else {
            Style::default().fg(p.overlay0).bg(p.panel_bg)
        };
        put_right_text(b, rect, rect.y, &row.pane_id, id_style);
    }
    snippet_footer_hints(
        b,
        stack.footer,
        &[
            ("↑↓".to_owned(), t.hint_select.to_owned()),
            ("enter".to_owned(), t.hint_continue.to_owned()),
            ("esc".to_owned(), t.hint_back.to_owned()),
        ],
        cx,
    );
    Some(OverlayRender {
        area: popup,
        snippet_popup: popup,
        snippet_rows: row_hits,
        ..OverlayRender::default()
    })
}

fn render_run_pick_machines(
    b: &mut Buffer,
    draft: &ClientSnippetRunDraft,
    endpoints: &[ClientShellEndpoint],
    cx: &super::feedback::ChromeContext<'_>,
) -> Option<OverlayRender> {
    let p = cx.palette;
    let t = &crate::i18n::texts().snippets;
    let (popup, inner) = snippets_panel(b, 18, cx)?;
    if inner.width < 24 || inner.height < 8 {
        return Some(OverlayRender {
            area: popup,
            ..OverlayRender::default()
        });
    }
    let stack = crate::ui::modal_stack_areas(inner, 2, 1, 0, 1);
    let base = Style::default()
        .bg(p.panel_bg)
        .remove_modifier(Modifier::DIM);
    put_text(
        b,
        stack.header.x,
        stack.header.y,
        stack.header.width,
        &format!(" {}", run_title(&draft.snippet)),
        base.fg(p.text).add_modifier(Modifier::BOLD),
    );
    put_text(
        b,
        stack.header.x,
        stack.header.y + 1,
        stack.header.width,
        &format!(" {}", t.machines_picker_title),
        base.fg(p.overlay0),
    );
    let rows = machine_rows(endpoints);
    let body = stack.content;
    let visible = usize::from(body.height).max(1);
    let selected = draft.target_selected.min(rows.len().saturating_sub(1));
    let scroll = selected
        .saturating_sub(visible.saturating_sub(1))
        .min(selected);
    let mut row_hits = Vec::new();
    if rows.is_empty() {
        put_text(
            b,
            body.x,
            body.y,
            body.width,
            t.picker_empty,
            base.fg(p.overlay0),
        );
    }
    for (index, (_, label)) in rows.iter().enumerate().skip(scroll).take(visible) {
        let y = body.y + (index - scroll) as u16;
        let rect = Rect::new(body.x, y, body.width, 1);
        row_hits.push((rect, index));
        let is_selected = index == selected;
        let checked = draft.machine_selected.get(index).copied().unwrap_or(false);
        let style = if is_selected {
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
            &format!(" {} {label}", if checked { "[x]" } else { "[ ]" }),
            style,
        );
    }
    snippet_footer_hints(
        b,
        stack.footer,
        &[
            ("space".to_owned(), t.hint_toggle.to_owned()),
            ("a".to_owned(), t.hint_all_none.to_owned()),
            ("enter".to_owned(), t.hint_continue.to_owned()),
            ("esc".to_owned(), t.hint_back.to_owned()),
        ],
        cx,
    );
    Some(OverlayRender {
        area: popup,
        snippet_popup: popup,
        snippet_rows: row_hits,
        ..OverlayRender::default()
    })
}

fn render_run_variables(
    b: &mut Buffer,
    draft: &ClientSnippetRunDraft,
    cx: &super::feedback::ChromeContext<'_>,
) -> Option<OverlayRender> {
    let p = cx.palette;
    let t = &crate::i18n::texts().snippets;
    let (popup, inner) = snippets_panel(b, 12, cx)?;
    if inner.width < 24 || inner.height < 8 {
        return Some(OverlayRender {
            area: popup,
            ..OverlayRender::default()
        });
    }
    let stack = crate::ui::modal_stack_areas(inner, 2, 1, 0, 1);
    let base = Style::default()
        .bg(p.panel_bg)
        .remove_modifier(Modifier::DIM);
    put_text(
        b,
        stack.header.x,
        stack.header.y,
        stack.header.width,
        &format!(" {}", run_title(&draft.snippet)),
        base.fg(p.text).add_modifier(Modifier::BOLD),
    );
    put_text(
        b,
        stack.header.x,
        stack.header.y + 1,
        stack.header.width,
        &format!(" {}", t.variables_title),
        base.fg(p.overlay0),
    );
    let body = stack.content;
    let mut field_hits = Vec::new();
    let mut cursor = None;
    let focused = draft
        .variable_focused
        .min(draft.variables.len().saturating_sub(1));
    for (index, (name, editor)) in draft.variables.iter().enumerate() {
        let y = body.y + index as u16;
        if y >= body.bottom() {
            break;
        }
        let rect = Rect::new(body.x, y, body.width, 1);
        field_hits.push((rect, index));
        let is_focused = index == focused;
        let label_width = 18u16.min(body.width);
        put_text(
            b,
            rect.x,
            rect.y,
            label_width,
            &format!(" {name:<16}"),
            base.fg(if is_focused { p.text } else { p.overlay0 }),
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
            editor,
            Style::default().fg(p.text).bg(p.surface0),
        );
        if is_focused {
            cursor = field_cursor;
        }
    }
    if let Some(error) = draft.error.as_deref() {
        let y = body.bottom().saturating_sub(1);
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
    snippet_footer_hints(
        b,
        stack.footer,
        &[
            ("tab/↑↓".to_owned(), t.hint_fields.to_owned()),
            ("enter".to_owned(), t.hint_continue.to_owned()),
            ("esc".to_owned(), t.hint_back.to_owned()),
        ],
        cx,
    );
    Some(OverlayRender {
        area: popup,
        snippet_popup: popup,
        snippet_fields: field_hits,
        cursor,
        ..OverlayRender::default()
    })
}

fn render_run_confirm(
    b: &mut Buffer,
    draft: &ClientSnippetRunDraft,
    endpoints: &[ClientShellEndpoint],
    saved_profiles: &[SavedSshEndpoint],
    cx: &super::feedback::ChromeContext<'_>,
) -> Option<OverlayRender> {
    let p = cx.palette;
    let t = &crate::i18n::texts().snippets;
    let (popup, inner) = snippets_panel(b, 16, cx)?;
    if inner.width < 24 || inner.height < 8 {
        return Some(OverlayRender {
            area: popup,
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
        &format!(" {}", run_title(&draft.snippet)),
        base.fg(p.text).add_modifier(Modifier::BOLD),
    );
    put_text(
        b,
        stack.header.x,
        stack.header.y + 1,
        stack.header.width,
        &format!(" {}", t.confirm_title),
        base.fg(p.overlay0),
    );
    let body = stack.content;
    let mut y = body.y;
    let command = draft
        .rendered_command()
        .unwrap_or_else(|_| draft.snippet.command.clone());
    put_text(
        b,
        body.x,
        y,
        body.width,
        &format!(" {}", t.confirm_command),
        base.fg(p.overlay0),
    );
    y += 1;
    put_text(
        b,
        body.x,
        y,
        body.width,
        &format!(" {command}"),
        base.fg(p.text),
    );
    y += 1;
    let targets = crate::i18n::fill(
        t.confirm_targets_fmt,
        &[("count", &draft.targets.len().to_string())],
    );
    put_text(
        b,
        body.x,
        y,
        body.width,
        &format!(" {targets}"),
        base.fg(p.overlay0),
    );
    y += 1;
    for target in draft.targets.iter().take(3) {
        if y >= body.bottom() {
            break;
        }
        let label = endpoints
            .iter()
            .find(|endpoint| endpoint.endpoint_id == target.endpoint_id)
            .map(|endpoint| endpoint.label.clone())
            .unwrap_or_else(|| target.machine.clone());
        put_text(
            b,
            body.x,
            y,
            body.width,
            &format!("   {label} · {}", target.pane_id),
            base.fg(p.subtext0),
        );
        y += 1;
    }
    if draft.targets.len() > 3 && y < body.bottom() {
        put_text(
            b,
            body.x,
            y,
            body.width,
            &format!("   … +{}", draft.targets.len() - 3),
            base.fg(p.subtext0),
        );
        y += 1;
    }
    let _ = saved_profiles;
    // Press-enter toggle row (clickable).
    let mut row_hits = Vec::new();
    if y < body.bottom() {
        let rect = Rect::new(body.x, y, body.width, 1);
        row_hits.push((rect, 0));
        put_text(
            b,
            rect.x,
            rect.y,
            rect.width,
            &format!(
                " {} {}",
                if draft.press_enter { "[x]" } else { "[ ]" },
                t.confirm_press_enter
            ),
            base.fg(p.text),
        );
    }
    if let Some(error) = draft.error.as_deref() {
        let error_y = stack
            .actions
            .map(|actions| actions.y.saturating_sub(1))
            .unwrap_or_else(|| body.bottom().saturating_sub(1));
        if error_y >= body.y {
            put_text(
                b,
                body.x,
                error_y,
                body.width,
                &format!(" {error}"),
                base.fg(p.red),
            );
        }
    }
    snippet_footer_hints(
        b,
        stack.footer,
        &[
            ("space".to_owned(), t.confirm_press_enter.to_owned()),
            ("enter".to_owned(), t.hint_run.to_owned()),
            ("esc".to_owned(), t.hint_back.to_owned()),
        ],
        cx,
    );
    let mut action_hits = Vec::new();
    let labels = [t.run_now_button, crate::i18n::texts().overlays.back_button];
    let rects = modal_button_row(stack.actions.unwrap_or_default(), &labels, 2);
    if let [run, back] = rects.as_slice() {
        modal_button(
            b,
            *run,
            labels[0],
            crate::ui::ModalButtonTone::Primary,
            cx.button_state(
                &super::feedback::ChromeHover::SnippetButton(SnippetOverlayButton::RunNow),
                crate::ui::ModalButtonState::Focused,
            ),
            p,
        );
        modal_button(
            b,
            *back,
            labels[1],
            crate::ui::ModalButtonTone::Secondary,
            cx.button_state(
                &super::feedback::ChromeHover::SnippetButton(SnippetOverlayButton::Back),
                crate::ui::ModalButtonState::Normal,
            ),
            p,
        );
        action_hits.push((*run, SnippetOverlayButton::RunNow));
        action_hits.push((*back, SnippetOverlayButton::Back));
    }
    Some(OverlayRender {
        area: popup,
        snippet_popup: popup,
        snippet_rows: row_hits,
        snippet_actions: action_hits,
        ..OverlayRender::default()
    })
}

fn render_snippet_history(
    b: &mut Buffer,
    overlay: &ClientSnippetsOverlay,
    selected: usize,
    scroll: usize,
    saved_profiles: &[SavedSshEndpoint],
    cx: &super::feedback::ChromeContext<'_>,
) -> Option<OverlayRender> {
    let p = cx.palette;
    let t = &crate::i18n::texts().snippets;
    let (popup, inner) = snippets_panel(b, 20, cx)?;
    if inner.width < 24 || inner.height < 8 {
        return Some(OverlayRender {
            area: popup,
            ..OverlayRender::default()
        });
    }
    let stack = crate::ui::modal_stack_areas(inner, 1, 1, 0, 1);
    let base = Style::default()
        .bg(p.panel_bg)
        .remove_modifier(Modifier::DIM);
    put_text(
        b,
        stack.header.x,
        stack.header.y,
        stack.header.width,
        &format!(" {}", t.history_title),
        base.fg(p.text).add_modifier(Modifier::BOLD),
    );
    let history: Vec<SnippetExecution> = overlay.library.history.iter().rev().cloned().collect();
    let machine_label = |machine: &str| -> String {
        if machine == "local" {
            return crate::i18n::texts().sidebar.local.to_owned();
        }
        saved_profiles
            .iter()
            .find(|profile| profile.id.as_str() == machine)
            .map(|profile| profile.label.clone())
            .unwrap_or_else(|| machine.to_owned())
    };
    let body = stack.content;
    let visible = usize::from(body.height).max(1);
    let selected = selected.min(history.len().saturating_sub(1));
    let scroll = scroll
        .max(selected.saturating_sub(visible.saturating_sub(1)))
        .min(selected);
    let mut row_hits = Vec::new();
    if history.is_empty() {
        put_text(
            b,
            body.x,
            body.y,
            body.width,
            t.history_empty,
            base.fg(p.overlay0),
        );
    }
    for (index, record) in history.iter().enumerate().skip(scroll).take(visible) {
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
        let (state_label, state_color) = if record.success {
            (t.history_sent, p.green)
        } else {
            (t.history_failed, p.red)
        };
        let text = format!(
            " {} · {} · {}",
            record.snippet_label,
            machine_label(&record.machine),
            record.pane_id
        );
        put_text(b, rect.x, rect.y, rect.width, &text, style);
        let right = format!("{state_label} · {}", relative_epoch_ago(record.executed_at));
        let right_style = if is_selected {
            style
        } else {
            Style::default().fg(state_color).bg(p.panel_bg)
        };
        put_right_text(b, rect, rect.y, &right, right_style);
        if !record.success && index == selected {
            if let Some(error) = record.error.as_deref() {
                if y + 1 < body.bottom() {
                    put_text(
                        b,
                        rect.x,
                        y + 1,
                        rect.width,
                        &format!("   {error}"),
                        base.fg(p.red),
                    );
                }
            }
        }
    }
    let _ = overlay;
    snippet_footer_hints(
        b,
        stack.footer,
        &[
            ("↑↓".to_owned(), t.hint_select.to_owned()),
            ("esc".to_owned(), t.hint_back.to_owned()),
        ],
        cx,
    );
    Some(OverlayRender {
        area: popup,
        snippet_popup: popup,
        snippet_rows: row_hits,
        ..OverlayRender::default()
    })
}
