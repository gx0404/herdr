//! SSH config 导入向导：Discover / Select / Done 三步与导入计划执行。

use super::*;

/// SSH config import wizard: discover → select → done. Discovery and
/// selection keep the parsed config and the current plan so the wildcard
/// toggle can re-plan without re-reading the file.
#[derive(Debug)]
pub(in crate::client::shell) struct ClientMachineImportView {
    pub(in crate::client::shell) step: ClientImportStep,
    pub(in crate::client::shell) path: std::path::PathBuf,
    pub(in crate::client::shell) config: Option<crate::remote::SshConfig>,
    pub(in crate::client::shell) warnings: usize,
    /// Load failure or "no hosts" empty state, shown instead of the plan.
    pub(in crate::client::shell) fatal: Option<String>,
    pub(in crate::client::shell) plan: crate::remote::ImportPlan,
    /// Checkbox states parallel to `plan.ready`.
    pub(in crate::client::shell) selected: Vec<bool>,
    pub(in crate::client::shell) include_wildcards: bool,
    /// Focus across candidate rows, then the wildcard toggle, then the group
    /// input (last two positions).
    pub(in crate::client::shell) focus_row: usize,
    /// 候选列表（select 步骤）与结果列表（done 步骤）共用的滚动窗口起点，
    /// 由渲染经 `OverlayRender::machines_scroll` 回写；换步骤时归零。
    pub(in crate::client::shell) scroll: usize,
    /// 一次性「把聚焦行滚进窗口」请求：焦点移动置位，compose 后清零。
    pub(in crate::client::shell) reveal: bool,
    pub(in crate::client::shell) group: TextEditor,
    pub(in crate::client::shell) results: Vec<ClientImportResultRow>,
    pub(in crate::client::shell) summary: (usize, usize, usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::client::shell) enum ClientImportStep {
    Discover,
    Select,
    Done,
}

#[derive(Debug)]
pub(in crate::client::shell) struct ClientImportResultRow {
    pub(in crate::client::shell) label: String,
    pub(in crate::client::shell) detail: String,
    pub(in crate::client::shell) outcome: ClientImportOutcome,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::client::shell) enum ClientImportOutcome {
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

impl ClientShellState {
    pub(in crate::client::shell) fn open_machine_import_wizard(&mut self) {
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
    pub(super) fn execute_machine_import(&mut self) {
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

    pub(super) fn move_import_focus(&mut self, delta: isize) {
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
    pub(super) fn scroll_import_results(&mut self, delta: isize) {
        let max_scroll = self.machines_view_max_scroll();
        if let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() {
            if let ClientMachinesView::Import(view) = &mut overlay.view {
                view.scroll = view.scroll.saturating_add_signed(delta).min(max_scroll);
                view.reveal = false;
            }
        }
    }

    pub(super) fn import_toggle_focused(&mut self) {
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

    pub(super) fn route_machine_import_key(
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
}

pub(super) const IMPORT_SELECT_FIXED_ROWS: u16 = 3;

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

pub(super) fn render_machine_import(
    b: &mut Buffer,
    view: &ClientMachineImportView,
    cx: &super::super::feedback::ChromeContext<'_>,
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
                    &super::super::feedback::ChromeHover::MachineButton(button),
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
    let scroll =
        super::super::page::list_start(view.scroll, focus, candidates, visible_rows, view.reveal);
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
    let scroll = super::super::page::list_start(
        view.scroll,
        view.scroll,
        total_lines,
        visible.max(1),
        false,
    );
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
