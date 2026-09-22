//! 端口转发编辑器：规则列表、删除确认（武装 → 确认）、添加表单与存盘。

use super::footer::{render_machine_footer, MachineHint};
use super::*;

/// Port-forward rules editor for one machine: lists the saved rules with
/// their live status and stages add/remove catalog writes (the catalog
/// watcher reconciles running forwards afterwards).
#[derive(Debug)]
pub(in crate::client::shell) struct ClientForwardRulesView {
    pub(in crate::client::shell) profile_id: ProfileId,
    /// 进入这个编辑器时停在哪个视图：宽屏 dashboard / 窄屏 List 直接按 `f`
    /// 进来的，Esc 必须回列表而不是掉进更窄的 Detail 模态（那正是 C-25 要
    /// 消除的尺寸跳变）。
    pub(in crate::client::shell) from_list: bool,
    pub(in crate::client::shell) selected: usize,
    /// 规则列表的滚动窗口起点；由渲染经 `OverlayRender::machines_scroll` 回写，
    /// 与机器列表同一套「compose 期回写 scroll」模式。
    pub(in crate::client::shell) scroll: usize,
    /// 一次性「把选中项滚进窗口」请求：键盘移动置位，compose 后清零。
    pub(in crate::client::shell) reveal: bool,
    pub(in crate::client::shell) adding: bool,
    pub(in crate::client::shell) form: ClientForwardRuleForm,
    /// 待确认删除的目标：`x` 只进入确认态，Enter 才真正删除。
    pub(in crate::client::shell) pending_remove: Option<PendingForwardRemoval>,
    pub(in crate::client::shell) error: Option<String>,
    pub(in crate::client::shell) message: Option<String>,
}

/// 武装中的转发删除目标。只记下标不够：目录 watcher（`set_endpoint_catalog`）
/// 会在浮层打开期间重新镜像 `saved_profiles`，另一个客户端或
/// `herdr machine forward remove` 都能在武装与确认之间改写规则表。确认落盘
/// 前比对整条规则，不一致就取消确认而不是照下标删（HERDR-MACH-005/025）。
#[derive(Debug, Clone)]
pub(in crate::client::shell) struct PendingForwardRemoval {
    pub(in crate::client::shell) index: usize,
    pub(in crate::client::shell) rule: PortForwardRule,
}

#[derive(Debug)]
pub(in crate::client::shell) struct ClientForwardRuleForm {
    /// Index into `FORWARD_KINDS`.
    pub(in crate::client::shell) kind: usize,
    pub(in crate::client::shell) listen_port: TextEditor,
    pub(in crate::client::shell) bind_address: TextEditor,
    pub(in crate::client::shell) target_host: TextEditor,
    pub(in crate::client::shell) target_port: TextEditor,
    pub(in crate::client::shell) focused: usize,
}

impl ClientForwardRuleForm {
    pub(in crate::client::shell) fn blank() -> Self {
        Self {
            kind: 0,
            listen_port: TextEditor::default(),
            bind_address: TextEditor::default(),
            target_host: TextEditor::default(),
            target_port: TextEditor::default(),
            focused: 0,
        }
    }
}

const FORWARD_KINDS: [crate::client::endpoint::PortForwardKind; 3] = [
    crate::client::endpoint::PortForwardKind::Local,
    crate::client::endpoint::PortForwardKind::Remote,
    crate::client::endpoint::PortForwardKind::Dynamic,
];

pub(super) const FORWARD_FORM_FIELDS: usize = 5;

impl ClientShellState {
    pub(in crate::client::shell) fn open_machine_forwards(&mut self, profile_id: &ProfileId) {
        if self.saved_profile(profile_id).is_none() {
            return;
        }
        if let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() {
            let from_list = matches!(overlay.view, ClientMachinesView::List);
            overlay.view = ClientMachinesView::Forwards(Box::new(ClientForwardRulesView {
                profile_id: profile_id.clone(),
                from_list,
                selected: 0,
                scroll: 0,
                reveal: true,
                adding: false,
                form: ClientForwardRuleForm::blank(),
                pending_remove: None,
                error: None,
                message: None,
            }));
        }
    }

    fn forward_rule_from_form(form: &ClientForwardRuleForm) -> Result<PortForwardRule, String> {
        let t = &crate::i18n::texts().cli_errors;
        let parse_port = |editor: &TextEditor, flag: &str| -> Result<Option<u16>, String> {
            let raw = editor.trim();
            if raw.is_empty() {
                return Ok(None);
            }
            raw.parse::<u16>().map(Some).map_err(|_| {
                crate::i18n::fill(t.invalid_flag_value_fmt, &[("flag", flag), ("value", raw)])
            })
        };
        let kind = FORWARD_KINDS[form.kind.min(FORWARD_KINDS.len() - 1)];
        let listen_port = parse_port(&form.listen_port, "--listen-port")?
            .ok_or_else(|| t.machine_forward_listen_port_required.to_owned())?;
        let target_host = nonempty(form.target_host.trim());
        let target_port = parse_port(&form.target_port, "--target-port")?;
        // local/remote 转发必须有目标：这里先拦，避免存盘时才被 catalog 校验
        // 挡回来（那条错误是英文且面向 CLI）。
        if matches!(kind, PortForwardKind::Local | PortForwardKind::Remote) {
            let ui = &crate::i18n::texts().machines;
            if target_host.is_none() {
                return Err(ui.forward_target_host_required.to_owned());
            }
            if target_port.is_none_or(|port| port == 0) {
                return Err(ui.forward_target_port_required.to_owned());
            }
        }
        Ok(PortForwardRule {
            kind,
            bind_address: nonempty(form.bind_address.trim()),
            listen_port,
            target_host,
            target_port,
        })
    }

    /// Persists the rule list of the open forwards view; the catalog watcher
    /// reconciles running forwards afterwards.
    fn store_forward_rules(
        &mut self,
        profile_id: &ProfileId,
        rules: Vec<PortForwardRule>,
    ) -> Result<(), String> {
        self.mutate_machine_catalog(|catalog| catalog.set_port_forwards(profile_id, rules))?;
        Ok(())
    }

    /// 转发编辑器是否正处于删除确认态。
    pub(super) fn forward_remove_pending(&self) -> bool {
        matches!(
            self.overlay.as_ref(),
            Some(ClientShellOverlay::Machines(overlay))
                if matches!(&overlay.view, ClientMachinesView::Forwards(view) if view.pending_remove.is_some())
        )
    }

    /// `x` / 移除按钮的第一步：记下待删规则的下标与规则值，画面点名后等
    /// Enter 确认（HERDR-MACH-025）。
    pub(super) fn forward_arm_remove(&mut self) {
        let armed = match self.overlay.as_ref() {
            Some(ClientShellOverlay::Machines(overlay)) => match &overlay.view {
                ClientMachinesView::Forwards(view) => {
                    let Some(profile) = self.saved_profile(&view.profile_id) else {
                        return;
                    };
                    let rules = profile.port_forwards.as_slice();
                    if rules.is_empty() {
                        return;
                    }
                    let index = view.selected.min(rules.len() - 1);
                    let Some(rule) = rules.get(index) else {
                        return;
                    };
                    PendingForwardRemoval {
                        index,
                        rule: rule.clone(),
                    }
                }
                _ => return,
            },
            _ => return,
        };
        if let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() {
            if let ClientMachinesView::Forwards(view) = &mut overlay.view {
                view.selected = armed.index;
                view.pending_remove = Some(armed);
                view.message = None;
                view.error = None;
                view.reveal = true;
            }
        }
    }

    pub(super) fn forward_cancel_remove(&mut self) {
        if let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() {
            if let ClientMachinesView::Forwards(view) = &mut overlay.view {
                if view.pending_remove.take().is_some() {
                    view.message = Some(
                        crate::i18n::texts()
                            .machines
                            .forward_remove_cancelled
                            .to_owned(),
                    );
                }
            }
        }
    }

    /// 确认后真正落盘删除。只按武装时记下的规则删：没武装就什么都不做，
    /// 规则表在武装与确认之间被外部改写（目录 watcher / CLI / 另一个客户端）
    /// 就取消确认并提示重选，绝不按下标兜底删掉另一条。
    pub(super) fn forward_remove_confirmed(&mut self) {
        let (profile_id, armed) = {
            let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_ref() else {
                return;
            };
            let ClientMachinesView::Forwards(view) = &overlay.view else {
                return;
            };
            let Some(armed) = view.pending_remove.clone() else {
                return;
            };
            (view.profile_id.clone(), armed)
        };
        let Some(profile) = self.saved_profile(&profile_id).cloned() else {
            return;
        };
        if profile.port_forwards.get(armed.index) != Some(&armed.rule) {
            self.forward_remove_stale();
            return;
        }
        let mut rules = profile.port_forwards.clone();
        rules.remove(armed.index);
        let remaining = rules.len();
        let outcome = self.store_forward_rules(&profile_id, rules);
        if let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() {
            if let ClientMachinesView::Forwards(view) = &mut overlay.view {
                view.pending_remove = None;
                // 落盘失败必须走红色 error 分支，不能以绿色「成功」样式显示。
                match outcome {
                    Ok(()) => {
                        view.message =
                            Some(crate::i18n::texts().machines.forward_removed.to_owned());
                        view.error = None;
                        // 原位补位；删掉末条时回退一行。
                        view.selected = armed.index.min(remaining.saturating_sub(1));
                    }
                    Err(error) => {
                        view.error = Some(error);
                        view.message = None;
                    }
                }
                view.reveal = true;
            }
        }
    }

    /// 武装期间规则表被外部改写：清掉确认态并提示重选。
    fn forward_remove_stale(&mut self) {
        if let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() {
            if let ClientMachinesView::Forwards(view) = &mut overlay.view {
                view.pending_remove = None;
                view.error = Some(
                    crate::i18n::texts()
                        .machines
                        .forward_remove_stale
                        .to_owned(),
                );
                view.message = None;
                view.reveal = true;
            }
        }
    }

    pub(super) fn forward_save_new_rule(&mut self) {
        let (profile_id, rule) = {
            let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() else {
                return;
            };
            let ClientMachinesView::Forwards(view) = &mut overlay.view else {
                return;
            };
            match Self::forward_rule_from_form(&view.form) {
                Ok(rule) => (view.profile_id.clone(), rule),
                Err(error) => {
                    view.error = Some(error);
                    return;
                }
            }
        };
        let Some(profile) = self.saved_profile(&profile_id).cloned() else {
            return;
        };
        let rule_count = profile.port_forwards.len();
        let mut rules = profile.port_forwards.clone();
        rules.push(rule);
        match self.store_forward_rules(&profile_id, rules) {
            Ok(()) => {
                if let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() {
                    if let ClientMachinesView::Forwards(view) = &mut overlay.view {
                        view.adding = false;
                        view.form = ClientForwardRuleForm::blank();
                        view.error = None;
                        view.message = Some(crate::i18n::texts().machines.forward_saved.to_owned());
                        view.selected = rule_count;
                    }
                }
            }
            Err(error) => {
                if let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() {
                    if let ClientMachinesView::Forwards(view) = &mut overlay.view {
                        // 失败只走红色 error 分支，别把上一次的绿色成功文案留在画面上。
                        view.error = Some(error);
                        view.message = None;
                    }
                }
            }
        }
    }

    /// Focus a forward add-form field by mouse; the kind field also cycles.
    pub(in crate::client::shell) fn focus_machine_forward_field(&mut self, field: usize) {
        let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() else {
            return;
        };
        let ClientMachinesView::Forwards(view) = &mut overlay.view else {
            return;
        };
        if !view.adding || field >= FORWARD_FORM_FIELDS {
            return;
        }
        if view.form.focused == field && field == 0 {
            view.form.kind = (view.form.kind + 1) % FORWARD_KINDS.len();
        }
        view.form.focused = field;
    }

    pub(super) fn route_machine_forwards_key(
        &mut self,
        key: &crate::input::TerminalKey,
        code: KeyCode,
        modifiers: KeyModifiers,
        outcome: &mut ClientShellInput,
    ) {
        let plain = modifiers.is_empty();
        let adding = matches!(
            self.overlay.as_ref(),
            Some(ClientShellOverlay::Machines(overlay))
                if matches!(&overlay.view, ClientMachinesView::Forwards(view) if view.adding)
        );
        if adding {
            if code == KeyCode::Esc {
                if let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() {
                    if let ClientMachinesView::Forwards(view) = &mut overlay.view {
                        view.adding = false;
                        view.error = None;
                    }
                }
                outcome.repaint = true;
                return;
            }
            if code == KeyCode::Enter {
                self.forward_save_new_rule();
                outcome.repaint = true;
                return;
            }
            let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() else {
                return;
            };
            let ClientMachinesView::Forwards(view) = &mut overlay.view else {
                return;
            };
            match code {
                KeyCode::Tab if plain => {
                    view.form.focused = (view.form.focused + 1) % FORWARD_FORM_FIELDS;
                }
                KeyCode::BackTab if modifiers.difference(KeyModifiers::SHIFT).is_empty() => {
                    view.form.focused =
                        (view.form.focused + FORWARD_FORM_FIELDS - 1) % FORWARD_FORM_FIELDS;
                }
                KeyCode::Up if plain => {
                    view.form.focused = view.form.focused.saturating_sub(1);
                }
                KeyCode::Down if plain => {
                    view.form.focused = (view.form.focused + 1).min(FORWARD_FORM_FIELDS - 1);
                }
                KeyCode::Left | KeyCode::Right if plain && view.form.focused == 0 => {
                    let delta = if code == KeyCode::Left { -1 } else { 1 };
                    view.form.kind = (view.form.kind as isize + delta)
                        .rem_euclid(FORWARD_KINDS.len() as isize)
                        as usize;
                }
                KeyCode::Char(' ') if plain && view.form.focused == 0 => {
                    view.form.kind = (view.form.kind + 1) % FORWARD_KINDS.len();
                }
                _ => {
                    let editor = match view.form.focused {
                        1 => &mut view.form.listen_port,
                        2 => &mut view.form.bind_address,
                        3 => &mut view.form.target_host,
                        _ => &mut view.form.target_port,
                    };
                    outcome.repaint |= editor.handle_key(key).is_some();
                }
            }
            outcome.repaint = true;
            return;
        }
        // 删除确认态独占 Enter/Esc：Esc 只取消确认，不退出编辑器。
        let pending = self.forward_remove_pending();
        match code {
            KeyCode::Esc if pending => self.forward_cancel_remove(),
            KeyCode::Esc => self.machines_back(),
            KeyCode::Enter if pending => self.forward_remove_confirmed(),
            // 确认键必须与触发键不同（与机器删除的 `ConfirmRemove` 对齐）：
            // 否则长按 `x` 会 arm → confirm → arm → … 把规则删光。
            KeyCode::Char('x') if plain && pending => self.forward_cancel_remove(),
            KeyCode::Up | KeyCode::Char('k') if plain => {
                if let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() {
                    if let ClientMachinesView::Forwards(view) = &mut overlay.view {
                        view.selected = view.selected.saturating_sub(1);
                        view.pending_remove = None;
                        view.reveal = true;
                    }
                }
            }
            KeyCode::Down | KeyCode::Char('j') if plain => {
                let count = self.forward_rule_count();
                if let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() {
                    if let ClientMachinesView::Forwards(view) = &mut overlay.view {
                        view.selected = (view.selected + 1).min(count.saturating_sub(1));
                        view.pending_remove = None;
                        view.reveal = true;
                    }
                }
            }
            KeyCode::Char('a') if plain => {
                if let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() {
                    if let ClientMachinesView::Forwards(view) = &mut overlay.view {
                        view.adding = true;
                        view.pending_remove = None;
                        view.message = None;
                        view.error = None;
                    }
                }
            }
            KeyCode::Char('x') if plain => self.forward_arm_remove(),
            _ => {}
        }
        outcome.repaint = true;
    }

    /// 当前转发编辑器对应机器的规则条数。
    pub(super) fn forward_rule_count(&self) -> usize {
        match self.overlay.as_ref() {
            Some(ClientShellOverlay::Machines(overlay)) => match &overlay.view {
                ClientMachinesView::Forwards(view) => self
                    .saved_profile(&view.profile_id)
                    .map(|profile| profile.port_forwards.len())
                    .unwrap_or(0),
                _ => 0,
            },
            _ => 0,
        }
    }
}

/// Rule one-liner shared by the detail card and the rules editor (matches
/// the CLI's `machine forward list` text).
pub(in crate::client::shell) fn forward_rule_display(rule: &PortForwardRule) -> String {
    let listen = match &rule.bind_address {
        Some(bind) => format!("{bind}:{}", rule.listen_port),
        None => rule.listen_port.to_string(),
    };
    match rule.kind {
        PortForwardKind::Local | PortForwardKind::Remote => format!(
            "{} {listen} -> {}:{}",
            rule.kind.as_str(),
            rule.target_host.as_deref().unwrap_or_default(),
            rule.target_port.unwrap_or_default()
        ),
        PortForwardKind::Dynamic => format!("{} {listen} (SOCKS)", rule.kind.as_str()),
    }
}

/// 端口转发编辑器的纵向分区（标题、正文、单条合并页脚）：视图计算与渲染
/// 共用（STATE-04）。
pub(super) fn forwards_stack(inner: Rect) -> crate::ui::ModalStackAreas {
    crate::ui::modal_stack_areas(inner, 1, 1, 0, 1)
}

pub(super) fn render_machine_forwards(
    b: &mut Buffer,
    view: &ClientForwardRulesView,
    saved_profiles: &[SavedSshEndpoint],
    port_forwards: &HashMap<ClientEndpointId, Vec<crate::remote::PortForwardStatus>>,
    cx: &super::super::feedback::ChromeContext<'_>,
) -> Option<OverlayRender> {
    let p = cx.palette;
    let t = &crate::i18n::texts().machines;
    let profile = saved_profiles
        .iter()
        .find(|profile| profile.id == view.profile_id)?;
    let (popup, inner) = modal_panel(b, crate::ui::ModalSize::Large.with_height(20), p.accent, cx)?;
    if inner.width < 24 || inner.height < 8 {
        return Some(OverlayRender {
            area: popup,
            machines_popup: popup,
            ..OverlayRender::default()
        });
    }
    let stack = forwards_stack(inner);
    let base = Style::default()
        .bg(p.panel_bg)
        .remove_modifier(Modifier::DIM);
    put_text(
        b,
        stack.header.x,
        stack.header.y,
        stack.header.width,
        &format!(
            " {}",
            crate::i18n::fill(t.forwards_title_fmt, &[("label", &profile.label)])
        ),
        base.fg(p.text).add_modifier(Modifier::BOLD),
    );

    let body = stack.content;
    let statuses = port_forwards.get(&ClientEndpointId::Ssh(view.profile_id.clone()));
    let mut row_hits = Vec::new();
    // 规则列表只占 body 的上半部：添加表单（空行 + 标题 + 字段 + 错误行）与
    // 非添加态的消息/确认行都有固定席位，规则多于一屏时列表自己滚动而不是把
    // 表单挤出画面（HERDR-MACH-005）。
    let reserved = if view.adding {
        FORWARD_FORM_FIELDS as u16 + 3
    } else {
        1
    };
    let list_height = body.height.saturating_sub(reserved);
    // 窗口起点与滚动上界共用同一个提升后的行数，否则 body 高度不足时上界比
    // 渲染实际接受的位点多一整屏。
    let visible_rows = usize::from(list_height).max(1);
    let rules = profile.port_forwards.as_slice();
    let selected = if rules.is_empty() {
        0
    } else {
        view.selected.min(rules.len() - 1)
    };
    // 确认态在本帧是否仍然成立：下标与规则值都要对得上，否则整帧（横幅、
    // 页脚、按钮行）一致地按「非确认态」画，不出现点不到名的确认横幅。
    let pending_rule = view.pending_remove.as_ref().and_then(|armed| {
        rules
            .get(armed.index)
            .filter(|rule| **rule == armed.rule && !view.adding)
    });
    let scroll = super::super::page::list_start(
        view.scroll,
        selected,
        rules.len(),
        visible_rows,
        view.reveal,
    );
    let mut action_hits = Vec::new();
    if rules.is_empty() && !view.adding {
        // 空状态：说明规则跟随连接，主按钮直接开始添加。
        let empty = Rect::new(body.x, body.y, body.width, list_height);
        if let Some(rect) = crate::ui::kit::empty_state::render_empty_state(
            b,
            empty,
            &crate::ui::kit::empty_state::EmptyState {
                glyph: None,
                title: t.forward_none.trim(),
                body: Some(t.forward_none_hint.trim()),
                action: Some(t.add_button.trim()),
            },
            p,
        ) {
            action_hits.push((rect, MachineOverlayButton::ForwardAddStart));
        }
    }
    for (index, rule) in rules
        .iter()
        .enumerate()
        .skip(scroll)
        .take(usize::from(list_height))
    {
        let rect = Rect::new(body.x, body.y + (index - scroll) as u16, body.width, 1);
        row_hits.push((rect, index));
        let is_selected = index == selected && !view.adding;
        let style = if is_selected {
            Style::default()
                .fg(panel_contrast_fg(p))
                .bg(p.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(p.text).bg(p.panel_bg)
        };
        b.set_style(rect, style);
        let status = statuses.and_then(|statuses| statuses.iter().find(|s| s.rule == *rule));
        let (status_text, status_color) = match status {
            Some(status) => match status.phase {
                crate::remote::PortForwardPhase::Active => {
                    (t.forward_status_active.to_owned(), p.green)
                }
                crate::remote::PortForwardPhase::Failed => (
                    format!(
                        "{}: {}",
                        t.forward_status_failed,
                        status.detail.as_deref().unwrap_or_default()
                    ),
                    p.red,
                ),
            },
            None => (t.forward_waiting.to_owned(), p.overlay0),
        };
        put_text(
            b,
            rect.x,
            rect.y,
            rect.width,
            &format!(" {}", forward_rule_display(rule)),
            style,
        );
        let status_style = if is_selected {
            style
        } else {
            Style::default().fg(status_color).bg(p.panel_bg)
        };
        put_right_text(b, rect, rect.y, &status_text, status_style);
    }
    // 列表之后的内容一律从固定席位开始，不受滚动窗口影响。
    let mut y = body.y + list_height;

    // Add-rule form: kind choice plus the four text fields.
    let mut field_hits = Vec::new();
    let mut cursor = None;
    if view.adding {
        y += 1;
        if y < body.bottom() {
            put_text(
                b,
                body.x,
                y,
                body.width,
                &format!(" {}", t.forward_add_title),
                base.fg(p.text).add_modifier(Modifier::BOLD),
            );
            y += 1;
        }
        let fields: [(&str, Option<&TextEditor>, Option<&str>); FORWARD_FORM_FIELDS] = [
            (t.forward_field_kind, None, None),
            (
                t.forward_field_listen_port,
                Some(&view.form.listen_port),
                None,
            ),
            (
                t.forward_field_bind_address,
                Some(&view.form.bind_address),
                None,
            ),
            (
                t.forward_field_target_host,
                Some(&view.form.target_host),
                None,
            ),
            (
                t.forward_field_target_port,
                Some(&view.form.target_port),
                None,
            ),
        ];
        for (index, (label, editor, _)) in fields.iter().enumerate() {
            if y >= body.bottom() {
                break;
            }
            let rect = Rect::new(body.x, y, body.width, 1);
            field_hits.push((rect, index));
            let is_focused = view.form.focused == index;
            let label_width = 16u16.min(body.width);
            put_text(
                b,
                rect.x,
                rect.y,
                label_width,
                &format!(" {label:<14}"),
                base.fg(if is_focused { p.text } else { p.overlay0 }),
            );
            let input = Rect::new(
                rect.x + label_width,
                rect.y,
                rect.width.saturating_sub(label_width),
                1,
            );
            match editor {
                None => {
                    let style = if is_focused {
                        Style::default()
                            .fg(panel_contrast_fg(p))
                            .bg(p.accent)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        crate::ui::input_field_style(p)
                    };
                    b.set_style(input, style);
                    put_text(
                        b,
                        input.x,
                        input.y,
                        input.width,
                        &format!("‹ {} ›", FORWARD_KINDS[view.form.kind].as_str()),
                        style,
                    );
                }
                Some(editor) => {
                    let field_style = crate::ui::input_field_style(p);
                    b.set_style(input, field_style);
                    let inner_input =
                        Rect::new(input.x + 1, input.y, input.width.saturating_sub(1), 1);
                    let field_cursor = text_editor::render(b, inner_input, editor, field_style);
                    if is_focused {
                        cursor = field_cursor;
                    }
                }
            }
            y += 1;
        }
        if let Some(error) = view.error.as_deref() {
            if y < body.bottom() {
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
    } else if let Some(rule) = pending_rule {
        // 待确认删除：点名规则本身，避免「删掉看不见的那一条」（HERDR-MACH-025）。
        if y < body.bottom() {
            put_text(
                b,
                body.x,
                y,
                body.width,
                &format!(
                    " {}",
                    crate::i18n::fill(
                        t.forward_remove_confirm_fmt,
                        &[("rule", &forward_rule_display(rule))]
                    )
                ),
                base.fg(p.red).add_modifier(Modifier::BOLD),
            );
        }
    } else if let Some(message) = view.message.as_deref() {
        if y < body.bottom() {
            put_text(b, body.x, y, body.width, message, base.fg(p.green));
        }
    } else if let Some(error) = view.error.as_deref() {
        if y < body.bottom() {
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

    // 合并页脚：键位即按钮，取代此前并排的键位行与按钮行。
    let t_overlays = &crate::i18n::texts().overlays;
    let hints: Vec<MachineHint<'static>> = if view.adding {
        vec![
            MachineHint::key("tab/↑↓", t.hint_fields),
            MachineHint::key("←→", t.hint_change),
            MachineHint::button("enter", t.hint_confirm, MachineOverlayButton::ForwardSave)
                .primary(),
            MachineHint::button("esc", t.hint_back, MachineOverlayButton::ForwardCancel),
        ]
    } else if pending_rule.is_some() {
        vec![
            MachineHint::button("enter", t.hint_confirm, MachineOverlayButton::ForwardRemove)
                .primary(),
            MachineHint::button(
                "esc",
                t_overlays.cancel_button,
                MachineOverlayButton::ForwardCancelRemove,
            ),
        ]
    } else {
        let remove = MachineHint::button("x", t.hint_remove, MachineOverlayButton::ForwardRemove);
        vec![
            MachineHint::key("↑↓", t.hint_select),
            MachineHint::button("a", t.hint_add, MachineOverlayButton::ForwardAddStart).primary(),
            if rules.is_empty() {
                remove.disabled()
            } else {
                remove
            },
            MachineHint::button("esc", t.hint_back, MachineOverlayButton::Back),
        ]
    };
    let footer = stack.footer.unwrap_or_default();
    action_hits.extend(render_machine_footer(b, footer, &hints, cx));

    Some(OverlayRender {
        area: popup,
        machines_popup: popup,
        machines_wizard_rows: row_hits,
        machines_wizard_fields: field_hits,
        machines_actions: action_hits,
        machines_toast: footer,
        cursor,
        ..OverlayRender::default()
    })
}
