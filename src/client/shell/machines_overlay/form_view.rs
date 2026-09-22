//! 机器表单的版面与渲染。版面函数由视图计算阶段（揭示焦点、夹紧滚动）与
//! 渲染阶段共用（STATE-04），渲染只读表单状态。
//!
//! 版面：标题行 → 快速输入（仅添加）→ 左栏分组字段（可滚动）+ 右栏实时预览
//! （窄屏省去预览，测试结果改画在字段栏底部）→ 错误行 → 合并页脚。

use super::footer::{render_machine_footer, MachineHint};
use super::form::{
    ClientMachineBootstrap, FieldGroup, QuickStatus, TestRecovery, TriChoice, FORM_GROUPS,
    STRICT_HOST_KEY_CHOICES,
};
use super::*;
use crate::ui::kit::form_field::{form_field_height, render_form_field, FieldState, FormFieldSpec};
use ratatui::text::{Line, Span};

/// 表单弹窗尺寸：两栏（字段 + 预览）比其他机器页宽，与宽屏工作台同档。
const FORM_SIZE: crate::ui::ModalSize = crate::ui::ModalSize::Content {
    width: 116,
    height: 34,
};
/// 内框宽于此才放右侧预览栏。
const PREVIEW_MIN_WIDTH: u16 = 72;
/// 窄屏时测试结果在字段栏底部占的行数上限（标题 + 四步 + 结论 + 说明）。
const NARROW_TEST_ROWS: u16 = 8;

pub(super) const BOOTSTRAP_STEPS: [SavedSshBootstrapStep; 4] = [
    SavedSshBootstrapStep::DetectPlatform,
    SavedSshBootstrapStep::Install,
    SavedSshBootstrapStep::StartServer,
    SavedSshBootstrapStep::Verify,
];

pub(in crate::client::shell) fn bootstrap_step_label(step: SavedSshBootstrapStep) -> &'static str {
    let t = &crate::i18n::texts().machines;
    match step {
        SavedSshBootstrapStep::DetectPlatform => t.progress_detect,
        SavedSshBootstrapStep::Install => t.progress_install,
        SavedSshBootstrapStep::StartServer => t.progress_start,
        SavedSshBootstrapStep::Verify => t.progress_verify,
    }
}

/// 表单弹窗的外框与内框（视图计算用；渲染经 `modal_panel` 得到同一对）。
pub(super) fn form_panel(area: Rect, page_bounds: Option<Rect>) -> Option<(Rect, Rect)> {
    let outer = page_bounds
        .map(|rect| rect.intersection(area))
        .or_else(|| crate::ui::modal_rect(area, FORM_SIZE))?;
    let inner = super::super::render::panel_inner(outer)?;
    (inner.width >= 24 && inner.height >= 8).then_some((outer, inner))
}

/// 表单各区域。空矩形表示本帧不画。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct FormLayout {
    pub(super) header: Rect,
    pub(super) quick: Rect,
    pub(super) fields: Rect,
    pub(super) preview: Rect,
    /// 窄屏：测试结果画在字段栏下方。
    pub(super) test: Rect,
    pub(super) error: Rect,
    pub(super) footer: Rect,
}

pub(super) fn form_layout(inner: Rect, form: &ClientMachineForm) -> FormLayout {
    let mut layout = FormLayout {
        header: Rect::new(inner.x, inner.y, inner.width, 1),
        footer: Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1),
        ..FormLayout::default()
    };
    let mut bottom = layout.footer.y;
    if form.error.is_some() {
        bottom = bottom.saturating_sub(1);
        layout.error = Rect::new(inner.x, bottom, inner.width, 1);
    }
    // 标题下空一行；快速输入三行（标签 / 输入 / 结论）后再空一行。
    let mut top = inner.y.saturating_add(2);
    if form.editing.is_none() {
        layout.quick = Rect::new(inner.x, top, inner.width, 3);
        top = top.saturating_add(4);
    }
    // 正文与错误行 / 页脚之间留一行空白。
    let body_bottom = bottom.saturating_sub(1);
    if top >= body_bottom {
        return layout;
    }
    let body = Rect::new(inner.x, top, inner.width, body_bottom - top);
    if inner.width >= PREVIEW_MIN_WIDTH {
        let left = (body.width * 3 / 5).max(36).min(body.width);
        layout.fields = Rect::new(body.x, body.y, left, body.height);
        let preview_x = body.x.saturating_add(left).saturating_add(3);
        layout.preview = Rect::new(
            preview_x,
            body.y,
            body.right().saturating_sub(preview_x),
            body.height,
        );
    } else if form.bootstrap.is_some() {
        let test_rows = NARROW_TEST_ROWS.min(body.height / 2);
        let fields_height = body.height - test_rows;
        layout.fields = Rect::new(body.x, body.y, body.width, fields_height);
        layout.test = Rect::new(body.x, body.y + fields_height, body.width, test_rows);
    } else {
        layout.fields = body;
    }
    layout
}

/// 字段栏里的一行内容：组标题或一个字段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FormRow {
    Group(FieldGroup),
    Field(MachineField),
}

/// 字段栏里的一项及其纵向位置（相对字段栏内容顶部，单位行）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct FormItem {
    pub(super) row: FormRow,
    pub(super) top: u16,
    pub(super) height: u16,
}

/// 字段的显示状态与额外文字：禁用（运行中 / 编辑时的目标）> 错误 > 聚焦。
/// 聚焦且有错时仍按聚焦画（kit 只在 `Focused` 画光标、按光标水平滚动），
/// 错误文案借提示行显示，渲染后再把那一行染红。
struct FieldPresentation {
    state_kind: StateKind,
    error: Option<String>,
    hint: Option<&'static str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StateKind {
    Normal,
    Focused,
    Invalid,
    Disabled,
}

fn presentation(
    form: &ClientMachineForm,
    field: MachineField,
    saved: &[SavedSshEndpoint],
) -> FieldPresentation {
    let disabled = form.running() || !form.is_editable(field);
    let focused = !disabled && form.focused_field() == Some(field);
    let error = if disabled {
        None
    } else {
        form.visible_error(field, saved)
    };
    let state_kind = match (disabled, focused, error.is_some()) {
        (true, _, _) => StateKind::Disabled,
        (false, true, _) => StateKind::Focused,
        (false, false, true) => StateKind::Invalid,
        (false, false, false) => StateKind::Normal,
    };
    FieldPresentation {
        state_kind,
        hint: if focused { field.hint() } else { None },
        error,
    }
}

fn field_height(presentation: &FieldPresentation) -> u16 {
    let extra = presentation.error.is_some() || presentation.hint.is_some();
    // 与 kit::form_field_height 同口径：标签 + 输入，有提示或错误再加一行。
    form_field_height(&FormFieldSpec {
        hint: extra.then_some(""),
        ..FormFieldSpec::default()
    })
}

/// 字段栏全部内容的纵向排布：视图计算与渲染共用。
pub(super) fn form_items(form: &ClientMachineForm, saved: &[SavedSshEndpoint]) -> Vec<FormItem> {
    let mut items = Vec::with_capacity(28);
    let mut top = 0u16;
    for (index, (group, fields)) in FORM_GROUPS.iter().enumerate() {
        if index > 0 {
            top = top.saturating_add(1);
        }
        items.push(FormItem {
            row: FormRow::Group(*group),
            top,
            height: 1,
        });
        top = top.saturating_add(1);
        for field in fields.iter() {
            let height = field_height(&presentation(form, *field, saved));
            items.push(FormItem {
                row: FormRow::Field(*field),
                top,
                height,
            });
            top = top.saturating_add(height);
        }
    }
    items
}

fn content_height(items: &[FormItem]) -> u16 {
    items
        .last()
        .map_or(0, |item| item.top.saturating_add(item.height))
}

/// 视图计算阶段：按一次性的 `reveal` 把聚焦字段（连同它的组标题，如果它是
/// 组内第一项）滚进窗口，并把滚动夹进上界。返回滚动上界。
pub(super) fn reveal_focused_field(
    form: &mut ClientMachineForm,
    fields: Rect,
    saved: &[SavedSshEndpoint],
) -> usize {
    let items = form_items(form, saved);
    let visible = fields.height;
    let max_scroll = usize::from(content_height(&items).saturating_sub(visible));
    if form.reveal && visible > 0 {
        if let Some(focused) = form.focused_field() {
            if let Some(position) = items
                .iter()
                .position(|item| item.row == FormRow::Field(focused))
            {
                let item = items[position];
                // 组内第一项连组标题一起露出来。
                let top = match position.checked_sub(1).map(|index| items[index].row) {
                    Some(FormRow::Group(_)) => item.top.saturating_sub(1),
                    _ => item.top,
                };
                let bottom = item.top.saturating_add(item.height);
                let mut scroll = u16::try_from(form.scroll).unwrap_or(u16::MAX);
                if top < scroll {
                    scroll = top;
                }
                if bottom > scroll.saturating_add(visible) {
                    scroll = bottom.saturating_sub(visible);
                }
                form.scroll = usize::from(scroll);
            }
        }
    }
    form.reveal = false;
    form.scroll = form.scroll.min(max_scroll);
    max_scroll
}

/// 页脚：键位与按钮合一。快速输入聚焦时 Enter 是「填入」，否则是保存。
fn form_hints(form: &ClientMachineForm) -> Vec<MachineHint<'static>> {
    let t = &crate::i18n::texts().machines;
    let f = &crate::i18n::texts().machine_form;
    if form.running() {
        return vec![
            MachineHint::button("esc", f.cancel_test, MachineOverlayButton::Back).primary(),
            MachineHint::button(
                "ctrl+t",
                f.test_running,
                MachineOverlayButton::TestConnection,
            )
            .disabled(),
        ];
    }
    let focused = form.focused_field();
    let mut hints = vec![MachineHint::key("tab", t.hint_fields)];
    if focused.is_some_and(MachineField::is_choice) {
        hints.push(MachineHint::key("←→", t.hint_change));
    }
    if form.quick_pending() {
        hints.push(
            MachineHint::button("enter", f.hint_fill, MachineOverlayButton::QuickApply).primary(),
        );
    } else {
        hints.push(
            MachineHint::button("enter", t.save_button, MachineOverlayButton::Save).primary(),
        );
    }
    // 测试连接与恢复入口只在添加时提供（见 `ClientMachineForm::can_test`）。
    if form.can_test() {
        hints.push(MachineHint::button(
            "ctrl+t",
            f.test_connection,
            MachineOverlayButton::TestConnection,
        ));
        if let Some(route) = form
            .bootstrap
            .as_ref()
            .and_then(ClientMachineBootstrap::recovery)
        {
            hints.push(MachineHint::button(
                "ctrl+r",
                recovery_label(route),
                MachineOverlayButton::TestRecover,
            ));
        }
    }
    hints.push(MachineHint::button(
        "esc",
        t.hint_back,
        MachineOverlayButton::Back,
    ));
    hints
}

fn recovery_label(route: TestRecovery) -> &'static str {
    match route {
        TestRecovery::HostKey | TestRecovery::HostKeyChanged => {
            crate::i18n::texts().machine_form.review_host_key
        }
        TestRecovery::Auth => crate::i18n::texts().machine_auth.auth_interactive_button,
    }
}

fn cursor_state(cursor: (u16, u16)) -> crate::protocol::CursorState {
    crate::protocol::CursorState {
        x: cursor.0,
        y: cursor.1,
        visible: true,
        // 与 `text_editor::render` 同一个闪烁竖线。
        shape: 5,
    }
}

/// 画一个字段，返回 (命中矩形, 聚焦时的光标)。
fn render_field(
    b: &mut Buffer,
    area: Rect,
    form: &ClientMachineForm,
    field: MachineField,
    saved: &[SavedSshEndpoint],
    p: &Palette,
) -> (Rect, Option<(u16, u16)>) {
    let t = &crate::i18n::texts().machine_form;
    let presentation = presentation(form, field, saved);
    let error_on_focus =
        presentation.state_kind == StateKind::Focused && presentation.error.is_some();
    let state = match presentation.state_kind {
        StateKind::Normal => FieldState::Normal,
        StateKind::Focused => FieldState::Focused,
        StateKind::Invalid => {
            FieldState::Invalid(presentation.error.as_deref().unwrap_or_default())
        }
        StateKind::Disabled => FieldState::Disabled,
    };
    let hint = if error_on_focus {
        presentation.error.as_deref()
    } else {
        presentation.hint
    };
    let choice_value;
    let label_placeholder;
    let spec = if field.is_choice() {
        choice_value = format!("‹ {} ›", form.choice_label(field));
        FormFieldSpec {
            label: field.label(),
            value: &choice_value,
            state,
            hint,
            ..FormFieldSpec::default()
        }
    } else {
        let placeholder = match field {
            MachineField::Target => t.target_placeholder,
            MachineField::Port => "22",
            MachineField::Session => crate::session::DEFAULT_SESSION_NAME,
            // 标签留空时取目标：占位直接显示最终会用的名字。
            MachineField::Label => {
                label_placeholder = form.target.trim().to_owned();
                label_placeholder.as_str()
            }
            _ => "",
        };
        let Some(editor) = form.editor(field) else {
            return (Rect::default(), None);
        };
        super::super::form::field_spec(
            editor,
            field.label(),
            placeholder,
            state,
            field == MachineField::Target && form.editing.is_none(),
            hint,
        )
    };
    let rendered = render_form_field(b, area, &spec, p);
    if error_on_focus && rendered.height >= 3 {
        b.set_style(
            Rect::new(area.x, area.y + 2, area.width, 1),
            Style::default().fg(p.red),
        );
    }
    let mut hit = Rect::new(area.x, area.y, area.width, rendered.height);
    if !rendered.input.is_empty() {
        hit = hit.union(rendered.input);
    }
    (hit, rendered.cursor)
}

fn render_group_title(b: &mut Buffer, area: Rect, group: FieldGroup, p: &Palette) {
    let title = group.title();
    let width = display_width(title).min(area.width);
    put_text(
        b,
        area.x,
        area.y,
        width,
        title,
        Style::default()
            .fg(p.accent)
            .bg(p.panel_bg)
            .add_modifier(Modifier::BOLD),
    );
    let rule_x = area.x.saturating_add(width).saturating_add(1);
    if rule_x < area.right() {
        let rule = "─".repeat(usize::from(area.right() - rule_x));
        put_text(
            b,
            rule_x,
            area.y,
            area.right() - rule_x,
            &rule,
            Style::default().fg(p.surface1).bg(p.panel_bg),
        );
    }
}

/// 预览栏与窄屏测试块共用：测试连接的步骤清单与结论。
fn test_lines(
    form: &ClientMachineForm,
    bootstrap: &ClientMachineBootstrap,
    base: Style,
    cx: &super::super::feedback::ChromeContext<'_>,
) -> Vec<Line<'static>> {
    let p = cx.palette;
    let t = &crate::i18n::texts().machines;
    let f = &crate::i18n::texts().machine_form;
    let mut lines = vec![Line::styled(
        f.test_connection.to_owned(),
        base.fg(p.text).add_modifier(Modifier::BOLD),
    )];
    let reached = bootstrap.step;
    let failed = bootstrap.failure.is_some();
    for step in BOOTSTRAP_STEPS {
        let (glyph, style) = if bootstrap.passed {
            ("✓".to_owned(), base.fg(p.green))
        } else {
            match reached {
                Some(reached) if step < reached => ("✓".to_owned(), base.fg(p.green)),
                Some(reached) if step == reached && failed => {
                    ("✗".to_owned(), base.fg(p.red).add_modifier(Modifier::BOLD))
                }
                Some(reached) if step == reached => (
                    cx.spinner.to_owned(),
                    base.fg(p.yellow).add_modifier(Modifier::BOLD),
                ),
                _ => ("·".to_owned(), base.fg(p.overlay0)),
            }
        };
        lines.push(Line::styled(
            format!(" {glyph} {}", bootstrap_step_label(step)),
            style,
        ));
    }
    if bootstrap.passed {
        lines.push(Line::from(vec![
            Span::styled(
                format!(" ✓ {}", f.test_passed),
                base.fg(p.green).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("  {}", f.test_not_saved), base.fg(p.overlay0)),
        ]));
    } else if let Some(failure) = bootstrap.failure.as_deref() {
        let step = reached.map_or("SSH", bootstrap_step_label);
        lines.push(Line::styled(
            format!(
                " {}",
                crate::i18n::fill(f.test_failed_fmt, &[("step", step)])
            ),
            base.fg(p.red).add_modifier(Modifier::BOLD),
        ));
        lines.push(Line::styled(format!(" {failure}"), base.fg(p.red)));
        match bootstrap.recovery().filter(|_| form.can_test()) {
            Some(route) => lines.push(Line::styled(
                format!(" ctrl+r · {}", recovery_label(route).trim()),
                base.fg(p.yellow),
            )),
            None => {
                lines.push(Line::styled(format!(" {}", t.fix_hint), base.fg(p.yellow)));
                let command = crate::remote::saved_ssh_bootstrap_command(
                    form.target.trim(),
                    &form.effective_session(),
                );
                lines.push(Line::styled(
                    format!("  {command}"),
                    base.fg(p.text).add_modifier(Modifier::BOLD),
                ));
            }
        }
    } else {
        lines.push(Line::styled(
            format!(" {}", f.test_running),
            base.fg(p.yellow),
        ));
    }
    lines
}

/// 把当前字段折成等价的一行 `ssh` 命令（只为预览核对，不参与连接）。
pub(super) fn ssh_command_preview(form: &ClientMachineForm) -> String {
    let mut parts = vec!["ssh".to_owned()];
    let port = form.port.trim();
    if !port.is_empty() {
        parts.push(format!("-p {port}"));
    }
    for identity in form
        .identity_files
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        parts.push(format!("-i {identity}"));
    }
    let jump = form.proxy_jump.trim();
    if !jump.is_empty() {
        parts.push(format!("-J {}", jump.replace(' ', "")));
    }
    match form.forward_agent {
        TriChoice::Yes => parts.push("-A".to_owned()),
        TriChoice::No => parts.push("-a".to_owned()),
        TriChoice::Default => {}
    }
    match form.identities_only {
        TriChoice::Yes => parts.push("-o IdentitiesOnly=yes".to_owned()),
        TriChoice::No => parts.push("-o IdentitiesOnly=no".to_owned()),
        TriChoice::Default => {}
    }
    if let Some(checking) = STRICT_HOST_KEY_CHOICES[form.strict_host_key] {
        parts.push(format!(
            "-o StrictHostKeyChecking={}",
            checking.as_ssh_value()
        ));
    }
    let user = form.user.trim();
    let target = form.target.trim();
    parts.push(match (user.is_empty(), target.is_empty()) {
        (_, true) => "…".to_owned(),
        (true, false) => target.to_owned(),
        (false, false) => format!("{user}@{target}"),
    });
    parts.join(" ")
}

/// 预览正文：等价 ssh 命令 + 各非空字段（与此前确认页同一批「标签 值」行）
/// + 测试连接会做什么的说明（只在能测试的添加表单里）。
fn preview_lines(form: &ClientMachineForm, base: Style, p: &Palette) -> Vec<Line<'static>> {
    let t = &crate::i18n::texts().machines;
    let mut lines = vec![
        Line::styled(ssh_command_preview(form), base.fg(p.accent)),
        Line::default(),
    ];
    let label_width = FORM_GROUPS
        .iter()
        .flat_map(|(_, fields)| fields.iter())
        .map(|field| display_width(field.label()))
        .max()
        .unwrap_or(0)
        .min(20);
    for field in FORM_GROUPS.iter().flat_map(|(_, fields)| fields.iter()) {
        let value = match field {
            MachineField::Label => form.effective_label(),
            MachineField::Session => form.effective_session(),
            _ if field.is_choice() => {
                let value = form.choice_label(*field);
                if value == t.choice_default {
                    String::new()
                } else {
                    value
                }
            }
            _ => form
                .editor(*field)
                .map(|editor| editor.trim().to_owned())
                .unwrap_or_default(),
        };
        if value.is_empty() {
            continue;
        }
        let label = field.label();
        let pad = usize::from(label_width.saturating_sub(display_width(label)));
        lines.push(Line::from(vec![
            Span::styled(format!("{label}{}  ", " ".repeat(pad)), base.fg(p.overlay0)),
            Span::styled(value, base.fg(p.text)),
        ]));
    }
    if form.can_test() {
        lines.push(Line::default());
        lines.push(Line::styled(
            t.confirm_install_note.to_owned(),
            base.fg(p.yellow),
        ));
        lines.push(Line::styled(
            t.confirm_auth_note.to_owned(),
            base.fg(p.overlay0),
        ));
    }
    lines
}

fn render_lines(b: &mut Buffer, area: Rect, lines: Vec<Line<'static>>) {
    use ratatui::widgets::{Paragraph, Widget, Wrap};
    if area.is_empty() {
        return;
    }
    Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .render(area, b);
}

pub(super) fn render_machine_form(
    b: &mut Buffer,
    form: &ClientMachineForm,
    saved: &[SavedSshEndpoint],
    cx: &super::super::feedback::ChromeContext<'_>,
) -> Option<OverlayRender> {
    let p = cx.palette;
    let t = &crate::i18n::texts().machines;
    let f = &crate::i18n::texts().machine_form;
    let (popup, inner) = modal_panel(b, FORM_SIZE, p.accent, cx)?;
    if inner.width < 24 || inner.height < 8 {
        return Some(OverlayRender {
            area: popup,
            machines_popup: popup,
            ..OverlayRender::default()
        });
    }
    let layout = form_layout(inner, form);
    let base = Style::default()
        .bg(p.panel_bg)
        .remove_modifier(Modifier::DIM);

    // 标题行：左标题，右侧是测试结论的一眼摘要。
    put_text(
        b,
        layout.header.x,
        layout.header.y,
        layout.header.width,
        &format!(
            " {}",
            if form.editing.is_some() {
                t.edit_title
            } else {
                t.add_title
            }
        ),
        base.fg(p.text).add_modifier(Modifier::BOLD),
    );
    if let Some(bootstrap) = form.bootstrap.as_ref() {
        let (text, color) = if bootstrap.passed {
            (format!("✓ {}", f.test_passed), p.green)
        } else if bootstrap.failure.is_some() {
            let step = bootstrap.step.map_or("SSH", bootstrap_step_label);
            (
                format!(
                    "✗ {}",
                    crate::i18n::fill(f.test_failed_fmt, &[("step", step)])
                ),
                p.red,
            )
        } else {
            (format!("{} {}", cx.spinner, f.test_running), p.yellow)
        };
        put_right_text(b, layout.header, layout.header.y, &text, base.fg(color));
    }

    let mut field_hits = Vec::new();
    let mut cursor = None;

    // 快速输入：粘贴即解析，结论显示在输入框下方。
    if !layout.quick.is_empty() {
        let status_text;
        let running = form.running();
        let focused = !running && form.focused_field() == Some(MachineField::Quick);
        let (state, hint) = match form.quick_status {
            _ if running => (FieldState::Disabled, None),
            Some(QuickStatus::Failed) => (FieldState::Invalid(f.quick_parse_failed), None),
            Some(QuickStatus::Filled(count)) => {
                status_text = crate::i18n::fill(f.quick_parsed_fmt, &[("n", &count.to_string())]);
                (
                    if focused {
                        FieldState::Focused
                    } else {
                        FieldState::Normal
                    },
                    Some(status_text.as_str()),
                )
            }
            None if focused => (FieldState::Focused, None),
            None => (FieldState::Normal, None),
        };
        // 解析失败时仍要能继续改：聚焦中按聚焦画（光标在），失败文案借提示行染红。
        let failed_on_focus = focused && matches!(state, FieldState::Invalid(_));
        let state = if failed_on_focus {
            FieldState::Focused
        } else {
            state
        };
        let hint = if failed_on_focus {
            Some(f.quick_parse_failed)
        } else {
            hint
        };
        let spec = super::super::form::field_spec(
            &form.quick,
            f.quick_label,
            f.quick_placeholder,
            state,
            false,
            hint,
        );
        let rendered = render_form_field(b, layout.quick, &spec, p);
        let hint_row = Rect::new(layout.quick.x, layout.quick.y + 2, layout.quick.width, 1);
        if failed_on_focus {
            b.set_style(hint_row, Style::default().fg(p.red));
        } else if matches!(form.quick_status, Some(QuickStatus::Filled(_))) && !running {
            b.set_style(hint_row, Style::default().fg(p.green));
        }
        if focused {
            cursor = rendered.cursor.map(cursor_state);
        }
        field_hits.push((
            Rect::new(
                layout.quick.x,
                layout.quick.y,
                layout.quick.width,
                rendered.height,
            ),
            MachineField::Quick,
        ));
    }

    // 左栏：分组字段，按视图计算阶段给出的滚动起点画窗口内的项。
    let fields = layout.fields;
    if !fields.is_empty() {
        let items = form_items(form, saved);
        let max_scroll = content_height(&items).saturating_sub(fields.height);
        let scroll = u16::try_from(form.scroll)
            .unwrap_or(u16::MAX)
            .min(max_scroll);
        let window_end = scroll.saturating_add(fields.height);
        for item in &items {
            if item.top < scroll || item.top >= window_end {
                continue;
            }
            let y = fields.y + (item.top - scroll);
            let height = item.height.min(fields.bottom() - y);
            let area = Rect::new(fields.x, y, fields.width.saturating_sub(1), height);
            match item.row {
                FormRow::Group(group) => render_group_title(b, area, group, p),
                FormRow::Field(field) => {
                    let (hit, field_cursor) = render_field(b, area, form, field, saved, p);
                    if let Some(field_cursor) = field_cursor {
                        cursor = Some(cursor_state(field_cursor));
                    }
                    if !hit.is_empty() {
                        field_hits.push((hit, field));
                    }
                }
            }
        }
        // 滚动提示：上下还有内容时在字段栏右缘画箭头。
        let edge = fields.right().saturating_sub(1);
        if scroll > 0 {
            put_text(b, edge, fields.y, 1, "▲", base.fg(p.overlay0));
        }
        if scroll < max_scroll {
            put_text(b, edge, fields.bottom() - 1, 1, "▼", base.fg(p.overlay0));
        }
    }

    // 右栏：实时预览；有测试结果时步骤清单排在最前。
    if !layout.preview.is_empty() {
        for y in layout.preview.y..layout.preview.bottom() {
            put_text(
                b,
                layout.preview.x.saturating_sub(2),
                y,
                1,
                "│",
                Style::default().fg(p.surface1).bg(p.panel_bg),
            );
        }
        put_text(
            b,
            layout.preview.x,
            layout.preview.y,
            layout.preview.width,
            f.preview_title,
            base.fg(p.text).add_modifier(Modifier::BOLD),
        );
        let mut lines = Vec::new();
        if let Some(bootstrap) = form.bootstrap.as_ref() {
            lines.extend(test_lines(form, bootstrap, base, cx));
            lines.push(Line::default());
        }
        lines.extend(preview_lines(form, base, p));
        render_lines(
            b,
            Rect::new(
                layout.preview.x,
                layout.preview.y.saturating_add(2),
                layout.preview.width,
                layout.preview.height.saturating_sub(2),
            ),
            lines,
        );
    }
    if let (false, Some(bootstrap)) = (layout.test.is_empty(), form.bootstrap.as_ref()) {
        render_lines(b, layout.test, test_lines(form, bootstrap, base, cx));
    }

    if let Some(error) = form.error.as_deref() {
        put_text(
            b,
            layout.error.x,
            layout.error.y,
            layout.error.width,
            &format!(" {error}"),
            base.fg(p.red),
        );
    }
    let action_hits = render_machine_footer(b, layout.footer, &form_hints(form), cx);

    Some(OverlayRender {
        area: popup,
        machines_popup: popup,
        machines_fields: field_hits,
        machines_actions: action_hits,
        machines_toast: layout.footer,
        cursor,
        ..OverlayRender::default()
    })
}
