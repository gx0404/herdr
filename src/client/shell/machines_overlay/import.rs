//! SSH config 导入向导：Discover / Select / Done 三步与导入计划执行。

use super::footer::{render_machine_footer, MachineHint};
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
    /// 主机清单（discover 步骤）、候选列表（select 步骤）与结果列表（done 步骤）
    /// 共用的滚动窗口起点，由视图计算阶段钳位回写；换步骤时归零。
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
        let unselected = crate::i18n::texts().machines.import_skip_unselected;
        for (index, planned) in view.plan.ready.iter().enumerate() {
            if !view.selected.get(index).copied().unwrap_or(false) {
                view.results.push(ClientImportResultRow {
                    label: planned.label.clone(),
                    detail: unselected.to_owned(),
                    outcome: ClientImportOutcome::Skipped,
                });
            }
        }
        // 发现阶段就跳过的主机（通配符、已存在、批内重复）也逐条列出并带原因
        // （文档终审 D3）：以前只计进汇总里的「跳过 N 台」，结果页看不到是哪几台。
        for skip in &view.plan.skipped {
            view.results.push(ClientImportResultRow {
                label: skip.label.clone(),
                detail: skip.reason.clone(),
                outcome: ClientImportOutcome::Skipped,
            });
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

    /// discover 步骤的主机清单与 done 步骤的结果列表共用的滚动：上界由视图计算
    /// 阶段写入（`view_max_scroll`），这里只按它钳位。
    pub(super) fn scroll_import_view(&mut self, delta: isize) {
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
        // 兼容旧式 G 与 kitty 的小写基键 + Shift/alternate，保留 Ctrl/Alt 区别。
        let capital_g = crate::config::terminal_key_matches_combo(
            key,
            (KeyCode::Char('g'), KeyModifiers::SHIFT),
        );
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
                    // 主机多于一屏时发现页可滚动（文档终审 D3），键位与 done 步骤
                    // 一致，另补翻页与首尾；上界由视图计算阶段给出。
                    KeyCode::Up | KeyCode::Char('k') if plain => self.scroll_import_view(-1),
                    KeyCode::Down | KeyCode::Char('j') if plain => self.scroll_import_view(1),
                    KeyCode::PageUp if plain => self.scroll_import_view(-10),
                    KeyCode::PageDown if plain => self.scroll_import_view(10),
                    KeyCode::Home | KeyCode::Char('g') if plain => {
                        self.scroll_import_view(isize::MIN)
                    }
                    KeyCode::End if plain => self.scroll_import_view(isize::MAX),
                    _ if capital_g => self.scroll_import_view(isize::MAX),
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
                    KeyCode::End if plain => self.move_import_focus(1000),
                    _ if capital_g => self.move_import_focus(1000),
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
                    self.scroll_import_view(-1);
                    outcome.repaint = true;
                }
                KeyCode::Down | KeyCode::Char('j') if plain => {
                    self.scroll_import_view(1);
                    outcome.repaint = true;
                }
                _ => {}
            },
        }
    }
}

/// 候选列表下方固定占住的行：通配符开关、分组输入、计数行。
const IMPORT_SELECT_FIXED_ROWS: u16 = 3;
/// 候选列表上方的表头行（「主机」+ 操作提示）。
const IMPORT_SELECT_HEADER_ROWS: u16 = 1;
/// body 宽于此才在右侧放预览栏；更窄时只画多选清单。
const IMPORT_PREVIEW_MIN_WIDTH: u16 = 72;

/// 导入向导弹窗的纵向分区（标题两行、正文、合并页脚）：视图计算与渲染共用
/// （STATE-04）。页脚按当前步骤的提示与宽度取一到两行，窄屏时 `esc 返回`
/// 不会被主动作挤掉。
pub(super) fn import_stack(
    inner: Rect,
    view: &ClientMachineImportView,
) -> crate::ui::ModalStackAreas {
    let footer_rows = super::footer::machine_footer_height(&import_hints(view), inner.width, 2);
    crate::ui::modal_stack_areas(inner, 2, footer_rows, 0, 1)
}

/// 各步骤的合并页脚：键位即按钮，取代此前并排的键位行与按钮行。
fn import_hints(view: &ClientMachineImportView) -> Vec<MachineHint<'static>> {
    let t = &crate::i18n::texts().machines;
    match view.step {
        ClientImportStep::Discover => {
            let continue_enabled = view.fatal.is_none() && !view.plan.ready.is_empty();
            let mut hints = Vec::with_capacity(3);
            // 没有可导入的主机时「继续」无处可去：不画成灰态按钮，直接不
            // 显示，页脚只剩返回（L19：空态下仍显示「enter 继续」容易让人
            // 以为按了会有反应）。
            if continue_enabled {
                hints.push(
                    MachineHint::button(
                        "enter",
                        t.hint_continue,
                        MachineOverlayButton::ImportContinue,
                    )
                    .primary(),
                );
            }
            hints.push(MachineHint::button(
                "esc",
                t.hint_back,
                MachineOverlayButton::Back,
            ));
            // 有主机清单时可滚动（文档终审 D3）：导航提示排在动作之后，窄时先丢。
            if view.fatal.is_none() {
                hints.push(MachineHint::key("↑↓", t.hint_scroll));
            }
            hints
        }
        ClientImportStep::Select => vec![
            MachineHint::key("↑↓", t.hint_select),
            MachineHint::key("space", t.hint_toggle),
            MachineHint::key("a", t.hint_all_none),
            MachineHint::button("enter", t.hint_import, MachineOverlayButton::ImportRun).primary(),
            MachineHint::button("esc", t.hint_back, MachineOverlayButton::Back),
        ],
        ClientImportStep::Done => vec![
            MachineHint::key("↑↓", t.hint_scroll),
            MachineHint::button("esc/enter", t.hint_back, MachineOverlayButton::Back).primary(),
        ],
    }
}

/// select 步骤的版面：左侧多选清单（表头 + 可滚动候选 + 固定行），右侧预览。
/// 视图计算与渲染共用（STATE-04）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ImportSelectLayout {
    pub(super) header: Rect,
    pub(super) list: Rect,
    pub(super) fixed: Rect,
    pub(super) preview: Rect,
}

pub(super) fn import_select_layout(body: Rect) -> ImportSelectLayout {
    let (left, preview) = if body.width >= IMPORT_PREVIEW_MIN_WIDTH {
        let left_width = body.width * 11 / 20;
        let preview_x = body.x.saturating_add(left_width).saturating_add(3);
        (
            Rect::new(body.x, body.y, left_width, body.height),
            Rect::new(
                preview_x,
                body.y,
                body.right().saturating_sub(preview_x),
                body.height,
            ),
        )
    } else {
        (body, Rect::default())
    };
    let header_rows = IMPORT_SELECT_HEADER_ROWS.min(left.height);
    let header = Rect::new(left.x, left.y, left.width, header_rows);
    let list_height = left
        .height
        .saturating_sub(header_rows + IMPORT_SELECT_FIXED_ROWS);
    let list = Rect::new(left.x, header.bottom(), left.width, list_height);
    let fixed = Rect::new(
        left.x,
        list.bottom(),
        left.width,
        left.bottom().saturating_sub(list.bottom()),
    );
    ImportSelectLayout {
        header,
        list,
        fixed,
        preview,
    }
}

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
    // 与机器列表页同一档尺寸（L19）：宽屏同 dashboard，窄屏同朴素列表，从机器
    // 页按 `i` 打开导入时浮层宽高都不跳变；视图计算（`machines_body`）同口径。
    let (popup, inner) = modal_panel(
        b,
        super::machines_page_size(b.area, cx.page_bounds),
        p.accent,
        cx,
    )?;
    if inner.width < 24 || inner.height < 8 {
        return Some(OverlayRender {
            area: popup,
            machines_popup: popup,
            ..OverlayRender::default()
        });
    }
    let stack = import_stack(inner, view);
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
    // 路径紧跟步骤条之后画（留 1 列空隙），而不是与步骤条共享同一矩形右对齐：
    // 两者各自的宽度先协商好，路径超出剩余宽度时用省略号收尾（保留开头，
    // 不会像 `put_right_text` 硬截断那样吃掉路径的首字符）（M4）。
    let path_x = x.saturating_add(1).min(stack.header.right());
    let path_width = stack.header.right().saturating_sub(path_x);
    if path_width > 0 {
        let path_text =
            crate::ui::truncate_end(&view.path.display().to_string(), usize::from(path_width));
        put_text(
            b,
            path_x,
            stack.header.y + 1,
            path_width,
            &path_text,
            base.fg(p.overlay0),
        );
    }

    let body = stack.content;
    let mut cursor = None;
    // 各步骤的滚动窗口由视图计算阶段按步骤给出（STATE-04），渲染只读。
    let mut action_hits = Vec::new();
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

    let hints = import_hints(view);
    let footer = stack.footer.unwrap_or_default();
    action_hits.extend(render_machine_footer(b, footer, &hints, cx));
    Some(OverlayRender {
        area: popup,
        machines_popup: popup,
        machines_wizard_rows: wizard_rows,
        machines_actions: action_hits,
        machines_toast: footer,
        cursor,
        ..OverlayRender::default()
    })
}

/// discover 步骤。没有可导入的主机（无配置、读失败、没有 Host）时画成空
/// 状态：说明文字 + 配置路径，不给动作按钮（页脚的 esc 返回即出口）。
fn render_import_discover(
    b: &mut Buffer,
    body: Rect,
    view: &ClientMachineImportView,
    base: Style,
    p: &Palette,
) {
    let t = &crate::i18n::texts().machines;
    if let Some(fatal) = view.fatal.as_deref() {
        let path = view.path.display().to_string();
        crate::ui::kit::empty_state::render_empty_state(
            b,
            body,
            &crate::ui::kit::empty_state::EmptyState {
                title: fatal.trim(),
                body: Some(&path),
                ..Default::default()
            },
            p,
        );
        return;
    }
    // 一屏放不下时按 `view.scroll` 滚动（文档终审 D3）；上界与视图计算阶段同一
    // 口径，这里只做只读钳位。只格式化落在窗口里的行：发现页随每次重绘都画，
    // 主机数可达几十上百。
    let visible = usize::from(body.height);
    let scroll = view.scroll.min(discover_max_scroll(view, body.height));
    let end = discover_line_count(view).min(scroll.saturating_add(visible));
    for (y, index) in (body.y..).zip(scroll..end) {
        let Some(line) = discover_line(view, index) else {
            break;
        };
        let (text, color) = match line {
            DiscoverLine::Warnings => (
                format!(
                    " {}",
                    crate::i18n::fill(
                        t.import_warnings_fmt,
                        &[("count", &view.warnings.to_string())]
                    )
                ),
                p.yellow,
            ),
            DiscoverLine::ReadyHeader => (t.import_ready_header.to_owned(), p.overlay0),
            DiscoverLine::Ready(planned) => {
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
                (text, p.text)
            }
            DiscoverLine::SkipHeader => (t.import_skip_header.to_owned(), p.overlay0),
            DiscoverLine::Skip(skip) => (format!(" {} — {}", skip.label, skip.reason), p.overlay1),
        };
        put_text(b, body.x, y, body.width, &text, base.fg(color));
    }
}

/// discover 步骤的行模型：解析告警行（有告警时）、「主机：」表头与每个候选一行、
/// 「跳过：」表头与每个跳过项一行。视图计算（滚动上界）与渲染共用同一口径
/// （STATE-04）。
enum DiscoverLine<'a> {
    Warnings,
    ReadyHeader,
    Ready(&'a crate::remote::PlannedImport),
    SkipHeader,
    Skip(&'a crate::remote::ImportSkip),
}

/// discover 步骤的总行数；空状态（`fatal`）没有可滚动的清单。
pub(super) fn discover_line_count(view: &ClientMachineImportView) -> usize {
    if view.fatal.is_some() {
        return 0;
    }
    let section = |len: usize| if len == 0 { 0 } else { len + 1 };
    usize::from(view.warnings > 0)
        + section(view.plan.ready.len())
        + section(view.plan.skipped.len())
}

/// discover 步骤在 `height` 行正文里的滚动上界。
pub(super) fn discover_max_scroll(view: &ClientMachineImportView, height: u16) -> usize {
    discover_line_count(view).saturating_sub(usize::from(height))
}

/// 第 `index` 行的内容，按 [`discover_line_count`] 的顺序逐段扣减，不分配。
fn discover_line(view: &ClientMachineImportView, mut index: usize) -> Option<DiscoverLine<'_>> {
    if view.warnings > 0 {
        if index == 0 {
            return Some(DiscoverLine::Warnings);
        }
        index -= 1;
    }
    if !view.plan.ready.is_empty() {
        if index == 0 {
            return Some(DiscoverLine::ReadyHeader);
        }
        index -= 1;
        if let Some(planned) = view.plan.ready.get(index) {
            return Some(DiscoverLine::Ready(planned));
        }
        index -= view.plan.ready.len();
    }
    if !view.plan.skipped.is_empty() {
        if index == 0 {
            return Some(DiscoverLine::SkipHeader);
        }
        return view.plan.skipped.get(index - 1).map(DiscoverLine::Skip);
    }
    None
}

/// select 步骤的渲染产物：窗口内的可点击行（候选、通配符开关、分组输入）、
/// 分组输入聚焦时的文本光标，以及本帧采用的滚动窗口起点与上界。
struct ImportSelectRender {
    rows: Vec<(Rect, usize)>,
    cursor: Option<crate::protocol::CursorState>,
}

/// 带预览的多选清单：表头一行（「主机」+ 空格 / Enter 提示），候选列表在
/// 其下滚动，通配符开关 / 分组输入 / 计数行固定在清单底部三行，主机多于一屏
/// 时它们仍然可见可点（HERDR-MACH-001）。宽屏右侧预览聚焦候选将要写入的
/// 字段与被丢弃的设置。只收录窗口内的候选行，保证鼠标命中与画面一致。
fn render_import_select(
    b: &mut Buffer,
    body: Rect,
    view: &ClientMachineImportView,
    base: Style,
    p: &Palette,
) -> ImportSelectRender {
    let t = &crate::i18n::texts().machines;
    let layout = import_select_layout(body);
    let mut hits = Vec::new();
    let candidates = view.plan.ready.len();
    let list = layout.list;
    // 窗口起点与滚动上界共用同一个提升后的行数，口径保持一致。
    let visible_rows = usize::from(list.height).max(1);
    let focus = view.focus_row.min(candidates.saturating_sub(1));
    let scroll =
        super::super::page::list_start(view.scroll, focus, candidates, visible_rows, view.reveal);
    let max_scroll = candidates.saturating_sub(visible_rows);

    // 表头：左「主机」，右侧操作提示。
    if !layout.header.is_empty() {
        put_text(
            b,
            layout.header.x,
            layout.header.y,
            layout.header.width,
            t.import_ready_header,
            base.fg(p.overlay0),
        );
        put_right_text(
            b,
            layout.header,
            layout.header.y,
            crate::i18n::texts().machine_form.import_select_hint,
            base.fg(p.overlay0),
        );
    }
    for (index, planned) in view
        .plan
        .ready
        .iter()
        .enumerate()
        .skip(scroll)
        .take(usize::from(list.height))
    {
        let rect = Rect::new(list.x, list.y + (index - scroll) as u16, list.width, 1);
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
        // 有 notes（会被丢弃的设置）时行尾要留 2 列给黄色记号；正文只在
        // 剩下的列里画，超出的部分用省略号收尾，不能让硬截断把数字砍成
        // 另一个合法值（如 `:2222` 被砍成 `:22!`，C2）。
        let has_notes = !planned.notes.is_empty();
        let mark_width = if has_notes { 2 } else { 0 };
        let content_width = rect.width.saturating_sub(mark_width);
        let text = crate::ui::truncate_end(
            &format!(
                " {} {} → {}",
                if checked { "[x]" } else { "[ ]" },
                planned.label,
                import_planned_summary(planned)
            ),
            usize::from(content_width),
        );
        put_text(b, rect.x, rect.y, content_width, &text, style);
        if has_notes {
            put_right_text(
                b,
                rect,
                rect.y,
                "! ",
                if focused { style } else { base.fg(p.yellow) },
            );
        }
    }
    let fixed = layout.fixed;
    let mut y = fixed.y;
    // Wildcard toggle row.
    if y < fixed.bottom() {
        let rect = Rect::new(fixed.x, y, fixed.width, 1);
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
    if y < fixed.bottom() {
        let rect = Rect::new(fixed.x, y, fixed.width, 1);
        hits.push((rect, candidates + 1));
        let focused = view.focus_row == candidates + 1;
        let label_width = 10u16.min(fixed.width);
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
    if y < fixed.bottom() {
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
        put_text(b, fixed.x, y, fixed.width, &counter, base.fg(p.overlay0));
    }
    if !layout.preview.is_empty() {
        render_import_preview(b, layout.preview, view, base, p);
    }
    ImportSelectRender { rows: hits, cursor }
}

/// 预览栏：聚焦候选时列出它将写入目录的字段（与详情卡同一批标签）与被丢弃
/// 的设置；焦点在通配符开关 / 分组输入上时给出本次导入的汇总。
fn render_import_preview(
    b: &mut Buffer,
    area: Rect,
    view: &ClientMachineImportView,
    base: Style,
    p: &Palette,
) {
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Paragraph, Widget, Wrap};
    let t = &crate::i18n::texts().machines;
    for y in area.y..area.bottom() {
        put_text(
            b,
            area.x.saturating_sub(2),
            y,
            1,
            "│",
            Style::default().fg(p.surface1).bg(p.panel_bg),
        );
    }
    put_text(
        b,
        area.x,
        area.y,
        area.width,
        crate::i18n::texts().machine_form.preview_title,
        base.fg(p.text).add_modifier(Modifier::BOLD),
    );
    let group = view.group.trim();
    let mut lines: Vec<Line<'static>> = Vec::new();
    match view.plan.ready.get(view.focus_row) {
        Some(planned) => {
            let checked = view.selected.get(view.focus_row).copied().unwrap_or(false);
            lines.push(Line::styled(
                format!("{} {}", if checked { "[x]" } else { "[ ]" }, planned.label),
                base.fg(p.text).add_modifier(Modifier::BOLD),
            ));
            lines.push(Line::styled(
                import_planned_summary(planned),
                base.fg(p.accent),
            ));
            lines.push(Line::default());
            let mut rows = import_option_rows(planned);
            if !group.is_empty() {
                rows.push((t.detail_group, group.to_owned()));
            }
            let label_width = rows
                .iter()
                .map(|(label, _)| usize::from(display_width(label)))
                .max()
                .unwrap_or(0)
                .min(20);
            for (label, value) in rows {
                let pad = label_width.saturating_sub(usize::from(display_width(label)));
                lines.push(Line::from(vec![
                    Span::styled(format!("{label}{}  ", " ".repeat(pad)), base.fg(p.overlay0)),
                    Span::styled(value, base.fg(p.text)),
                ]));
            }
            if !planned.notes.is_empty() {
                lines.push(Line::default());
                lines.push(Line::styled(
                    crate::i18n::fill(
                        t.import_notes_fmt,
                        &[("count", &planned.notes.len().to_string())],
                    ),
                    base.fg(p.yellow),
                ));
                for note in &planned.notes {
                    lines.push(Line::styled(format!(" · {note}"), base.fg(p.overlay1)));
                }
            }
        }
        None => {
            let selected = view.selected.iter().filter(|selected| **selected).count();
            lines.push(Line::styled(
                crate::i18n::fill(
                    t.import_selected_fmt,
                    &[
                        ("selected", &selected.to_string()),
                        ("total", &view.plan.ready.len().to_string()),
                    ],
                ),
                base.fg(p.text).add_modifier(Modifier::BOLD),
            ));
            if !group.is_empty() {
                lines.push(Line::from(vec![
                    Span::styled(format!("{}  ", t.detail_group), base.fg(p.overlay0)),
                    Span::styled(group.to_owned(), base.fg(p.text)),
                ]));
            }
        }
    }
    let content = Rect::new(
        area.x,
        area.y.saturating_add(2),
        area.width,
        area.height.saturating_sub(2),
    );
    if !content.is_empty() {
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .render(content, b);
    }
}

/// 一个候选将写入目录的连接字段（只列出 SSH 配置里真正出现的项）。
fn import_option_rows(planned: &crate::remote::PlannedImport) -> Vec<(&'static str, String)> {
    let t = &crate::i18n::texts().machines;
    let options = &planned.options;
    let yes_no = |value: bool| {
        if value {
            t.choice_yes.to_owned()
        } else {
            t.choice_no.to_owned()
        }
    };
    let mut rows = vec![(t.detail_target, planned.target.clone())];
    if let Some(user) = &options.user {
        rows.push((t.detail_user, user.clone()));
    }
    if let Some(port) = options.port {
        rows.push((t.detail_port, port.to_string()));
    }
    if !options.identity_file.is_empty() {
        rows.push((t.detail_identity_files, options.identity_file.join(", ")));
    }
    if let Some(value) = options.identities_only {
        rows.push((t.detail_identities_only, yes_no(value)));
    }
    if let Some(agent) = &options.identity_agent {
        rows.push((t.detail_identity_agent, agent.clone()));
    }
    if let Some(checking) = options.strict_host_key_checking {
        rows.push((t.detail_strict_host_key, checking.as_ssh_value().to_owned()));
    }
    if !options.proxy_jump.is_empty() {
        let hops = options
            .proxy_jump
            .iter()
            .map(|hop| match hop {
                ProxyJumpHop::Target(target) => target.clone(),
                ProxyJumpHop::Profile(id) => format!("profile:{id}"),
            })
            .collect::<Vec<_>>()
            .join(", ");
        rows.push((t.detail_proxy_jump, hops));
    }
    if let Some(value) = options.forward_agent {
        rows.push((t.detail_forward_agent, yes_no(value)));
    }
    if let Some(interval) = options.server_alive_interval {
        rows.push((t.detail_server_alive_interval, interval.to_string()));
    }
    if let Some(count) = options.server_alive_count_max {
        rows.push((t.detail_server_alive_count_max, count.to_string()));
    }
    if let Some(persist) = &options.control_persist {
        rows.push((t.detail_control_persist, persist.clone()));
    }
    if let Some(command) = &options.remote_command {
        rows.push((t.detail_remote_command, command.clone()));
    }
    rows
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
