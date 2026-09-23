//! 机器表单的版面与渲染。版面函数由视图计算阶段（揭示焦点、夹紧滚动）与
//! 渲染阶段共用（STATE-04），渲染只读表单状态。
//!
//! 版面：标题行 → 快速输入（仅添加）→ 左栏分组字段（可滚动）+ 右栏实时预览
//! （窄屏省去预览，测试结果改画在字段栏底部）→ 错误行 → 合并页脚。

use super::footer::{render_machine_footer, MachineHint};
use super::form::{
    ClientMachineBootstrap, FieldGroup, FormPrompt, QuickStatus, TestRecovery, TriChoice,
    FORM_GROUPS, STRICT_HOST_KEY_CHOICES,
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
/// 确认条折行后的行数上限（再多就截断，宽度 24 起步时也放得下说明）。
const PROMPT_MAX_ROWS: u16 = 4;
const PROMPT_ICON: &str = "⚠ ";

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
    /// 待确认的一步（紧贴页脚上方，宽窄屏都有）。
    pub(super) prompt: Rect,
    pub(super) footer: Rect,
}

fn prompt_text(prompt: FormPrompt) -> &'static str {
    let f = &crate::i18n::texts().machine_form;
    match prompt {
        FormPrompt::ConfirmTest => f.test_confirm_note,
        FormPrompt::Discard => f.discard_prompt,
    }
}

/// 确认条折行后的行数：与渲染（`render_lines`）同一个折行函数，两边不会
/// 一个按 ratatui 分词、一个按 `wrap_lines_for_display` 断行而差出一行。
fn prompt_rows(prompt: FormPrompt, width: u16) -> u16 {
    let line = Line::from(vec![Span::raw(PROMPT_ICON), Span::raw(prompt_text(prompt))]);
    let rows = wrap_lines_for_display(vec![line], width).len();
    u16::try_from(rows)
        .unwrap_or(PROMPT_MAX_ROWS)
        .clamp(1, PROMPT_MAX_ROWS)
}

pub(super) fn form_layout(inner: Rect, form: &ClientMachineForm) -> FormLayout {
    // 页脚按实际宽度换行，最多三行——与机器列表页同一口径（L10：窄宽度下
    // 固定一行会把放不下的项（「esc 返回」）直接丢掉，而不是换到下一行）。
    let footer_rows = super::footer::machine_footer_height(&form_hints(form), inner.width, 3);
    let mut layout = FormLayout {
        header: Rect::new(inner.x, inner.y, inner.width, 1),
        footer: Rect::new(
            inner.x,
            inner.bottom().saturating_sub(footer_rows),
            inner.width,
            footer_rows,
        ),
        ..FormLayout::default()
    };
    let mut bottom = layout.footer.y;
    if let Some(prompt) = form.prompt {
        let rows = prompt_rows(prompt, inner.width);
        bottom = bottom.saturating_sub(rows);
        layout.prompt = Rect::new(inner.x, bottom, inner.width, rows);
    }
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
    if let Some(prompt) = form.prompt {
        return match prompt {
            FormPrompt::ConfirmTest => vec![
                MachineHint::button(
                    "enter",
                    f.test_confirm_start,
                    MachineOverlayButton::TestConnection,
                )
                .primary(),
                MachineHint::button("esc", f.prompt_cancel, MachineOverlayButton::Back),
            ],
            FormPrompt::Discard => vec![
                MachineHint::button(
                    "enter",
                    f.discard_confirm,
                    MachineOverlayButton::DiscardForm,
                )
                .primary(),
                MachineHint::button("esc", f.keep_editing, MachineOverlayButton::Back),
            ],
        };
    }
    // 先动作后导航：kit 放不下时从尾部丢非 primary 项，排在最后的纯导航键
    // （tab / ←→）最先让位，「esc 返回」与测试 / 恢复入口留到最后。
    let mut hints = Vec::with_capacity(6);
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
    hints.push(MachineHint::key("tab", t.hint_fields));
    if form.focused_field().is_some_and(MachineField::is_choice) {
        hints.push(MachineHint::key("←→", t.hint_change));
    }
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
            is_choice: true,
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
            // 其余可选字段空着时画一个通用占位符，而不是留白（M8：肉眼
            // 分不清是空字段还是没画出来）。
            _ => crate::i18n::texts().machines.value_not_set,
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

/// 等价 `ssh` 命令里的一个词。一个词可能由几段拼成（`user@target` 的用户与
/// 目标来自不同字段），每段记下来源字段（`None` 是命令名、`@` 等固定部分），
/// 预览据此只把校验失败的那一段标红（L15）。
type SshWord = Vec<(String, Option<MachineField>)>;

/// 把当前字段折成等价 `ssh` 命令的词序列（只为预览核对，不参与连接）。
fn ssh_command_words(form: &ClientMachineForm) -> Vec<SshWord> {
    let mut words: Vec<SshWord> = vec![vec![("ssh".to_owned(), None)]];
    let mut push = |text: String, field: MachineField| words.push(vec![(text, Some(field))]);
    let port = form.port.trim();
    if !port.is_empty() {
        push(format!("-p {port}"), MachineField::Port);
    }
    for identity in form
        .identity_files
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        push(format!("-i {identity}"), MachineField::IdentityFiles);
    }
    let jump = form.proxy_jump.trim();
    if !jump.is_empty() {
        push(
            format!("-J {}", jump.replace(' ', "")),
            MachineField::ProxyJump,
        );
    }
    match form.forward_agent {
        TriChoice::Yes => push("-A".to_owned(), MachineField::ForwardAgent),
        TriChoice::No => push("-a".to_owned(), MachineField::ForwardAgent),
        TriChoice::Default => {}
    }
    match form.identities_only {
        TriChoice::Yes => push(
            "-o IdentitiesOnly=yes".to_owned(),
            MachineField::IdentitiesOnly,
        ),
        TriChoice::No => push(
            "-o IdentitiesOnly=no".to_owned(),
            MachineField::IdentitiesOnly,
        ),
        TriChoice::Default => {}
    }
    if let Some(checking) = STRICT_HOST_KEY_CHOICES[form.strict_host_key] {
        push(
            format!("-o StrictHostKeyChecking={}", checking.as_ssh_value()),
            MachineField::StrictHostKey,
        );
    }
    let user = form.user.trim();
    let target = form.target.trim();
    words.push(match (user.is_empty(), target.is_empty()) {
        (_, true) => vec![("…".to_owned(), Some(MachineField::Target))],
        (true, false) => vec![(target.to_owned(), Some(MachineField::Target))],
        (false, false) => vec![
            (user.to_owned(), Some(MachineField::User)),
            ("@".to_owned(), None),
            (target.to_owned(), Some(MachineField::Target)),
        ],
    });
    words
}

/// 预览首行：等价 `ssh` 命令。校验失败的字段对应的那一段（如端口 99999 的
/// `-p 99999`）单独标红，所在的词后紧跟「（无效）」，不只靠颜色区分；其余
/// 部分保持 accent（L15 复审：此前整行 accent，非法值看着像已通过校验）。
fn ssh_command_line(
    form: &ClientMachineForm,
    saved: &[SavedSshEndpoint],
    base: Style,
    p: &Palette,
) -> Line<'static> {
    let suffix = crate::i18n::texts().machine_form.preview_invalid_suffix;
    let mut spans = Vec::new();
    for (index, word) in ssh_command_words(form).into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(" ", base.fg(p.accent)));
        }
        let mut word_invalid = false;
        for (text, field) in word {
            let invalid = field.is_some_and(|field| form.visible_error(field, saved).is_some());
            word_invalid |= invalid;
            let color = if invalid { p.red } else { p.accent };
            spans.push(Span::styled(text, base.fg(color)));
        }
        if word_invalid {
            spans.push(Span::styled(suffix, base.fg(p.red)));
        }
    }
    Line::from(spans)
}

/// 预览正文：等价 ssh 命令、各非空字段（与此前确认页同一批「标签 值」行），
/// 以及测试连接会做什么的说明（只在能测试的添加表单里）。字段校验失败时
/// （如端口报错），ssh 命令里对应的词与字段清单里的值都标红并追加「（无效）」，
/// 不再原样显示非法值当作什么事都没有（L15）。
fn preview_lines(
    form: &ClientMachineForm,
    saved: &[SavedSshEndpoint],
    base: Style,
    p: &Palette,
) -> Vec<Line<'static>> {
    let t = &crate::i18n::texts().machines;
    let f = &crate::i18n::texts().machine_form;
    let mut lines = vec![ssh_command_line(form, saved, base, p), Line::default()];
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
        let invalid = form.visible_error(*field, saved).is_some();
        let value = if invalid {
            format!("{value}{}", f.preview_invalid_suffix)
        } else {
            value
        };
        let value_style = if invalid {
            base.fg(p.red)
        } else {
            base.fg(p.text)
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{label}{}  ", " ".repeat(pad)), base.fg(p.overlay0)),
            Span::styled(value, value_style),
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
    use ratatui::widgets::{Paragraph, Widget};
    if area.is_empty() {
        return;
    }
    Paragraph::new(wrap_lines_for_display(lines, area.width)).render(area, b);
}

fn char_display_width(ch: char) -> usize {
    let mut buf = [0u8; 4];
    usize::from(display_width(ch.encode_utf8(&mut buf)))
}

/// 一行折出的若干视觉行拼回 `Line`，保留每段原来的样式（相邻同样式的字符
/// 合并成一个 `Span`，避免每个字符单独一个 span）。
fn build_wrapped_span_line(chars: &[(char, Style)]) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    for &(ch, style) in chars {
        match spans.last_mut() {
            Some(span) if span.style == style => span.content.to_mut().push(ch),
            _ => spans.push(Span::styled(ch.to_string(), style)),
        }
    }
    Line::from(spans)
}

/// 不能出现在行首的字符（避头）：中文逗句号、闭括号、省略号，以及 ASCII 的
/// 收尾标点。
fn no_break_before(ch: char) -> bool {
    matches!(
        ch,
        '，' | '。'
            | '；'
            | '：'
            | '、'
            | '！'
            | '？'
            | '）'
            | '」'
            | '』'
            | '】'
            | '》'
            | '〉'
            | '…'
            | ')'
            | ']'
            | '}'
            | '.'
            | ','
            | ';'
            | ':'
            | '!'
            | '?'
    )
}

/// 全角开括号：不能出现在行尾（避尾）；括号里的短语（如「（无效）」）整体不拆。
fn is_wide_open(ch: char) -> bool {
    matches!(ch, '（' | '「' | '『' | '【' | '《' | '〈')
}

fn is_wide_close(ch: char) -> bool {
    matches!(ch, '）' | '」' | '』' | '】' | '》' | '〉')
}

/// 词内的 ASCII 分隔标点。它们**不是**正常断点；只有一个词本身比整行还宽、
/// 行内找不到别的断点时，才退而在它们之后断（主机名、路径被迫折行时至少断在
/// `.` `/` `-` 之后，而不是任意字符中间）。
fn is_word_separator(ch: char) -> bool {
    matches!(
        ch,
        '.' | '/' | '-' | '_' | '@' | ':' | ',' | ';' | '=' | '&' | '?' | '!'
    )
}

/// `next` 能否起新的一行（在 `prev` 与 `next` 之间折行）。按 UAX #14 的思路
/// 简化：空白之后可断；CJK 等宽字符前后可断（中文不靠空格分词）；避头 / 避尾
/// 字符两侧不断；全角括号内不断，字母数字后紧跟的全角开括号也不断（值与
/// 「（无效）」保持在一起）；ASCII 标点在词内不算断点——主机名、IP、路径只会
/// 整体换到下一行（L14 复审）。
fn can_break_between(prev: char, next: char, in_bracket: bool) -> bool {
    let prev_opens = is_wide_open(prev) || matches!(prev, '(' | '[' | '{');
    if next == ' ' || no_break_before(next) || prev_opens {
        return false;
    }
    if prev == ' ' {
        return true;
    }
    if in_bracket {
        return false;
    }
    let prev_wide = char_display_width(prev) >= 2;
    if is_wide_open(next) {
        return prev_wide;
    }
    prev_wide || char_display_width(next) >= 2
}

/// 按显示宽度折行，保留每段样式（行级样式并进每个 span，`Line::styled` 的颜色
/// 不会在折行后丢失）。断点见 [`can_break_between`]；一个词本身比整行还宽时，
/// 退而在词内的 `.` `/` `-` 等之后断，还不行才按字符硬断。L14 复审：旧实现把
/// 词内的 `.` `:` 也当断点、取溢出前最后一个，主机名被拦腰折成
/// `…corp.` / `example.com`；纯 CJK 长句也只在标点处断，留下「此处 SSH」这样
/// 的短行。
fn wrap_lines_for_display(lines: Vec<Line<'static>>, width: u16) -> Vec<Line<'static>> {
    let width = usize::from(width.max(1));
    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        let line_style = line.style;
        let chars: Vec<(char, Style)> = line
            .spans
            .iter()
            .flat_map(|span| {
                let style = line_style.patch(span.style);
                span.content.chars().map(move |ch| (ch, style))
            })
            .collect();
        if chars.is_empty() {
            out.push(Line::default());
            continue;
        }
        // 先按整行上下文算好每个位置之前能否折行：`breakable` 是正常断点，
        // `fallback` 是词内分隔符之后的应急断点。
        let mut breakable = vec![false; chars.len()];
        let mut fallback = vec![false; chars.len()];
        let mut bracket_depth = 0usize;
        let mut prev: Option<char> = None;
        for (index, &(ch, _)) in chars.iter().enumerate() {
            if let Some(prev) = prev {
                breakable[index] = can_break_between(prev, ch, bracket_depth > 0);
                fallback[index] = is_word_separator(prev) && ch != ' ' && !no_break_before(ch);
            }
            if is_wide_open(ch) {
                bracket_depth += 1;
            } else if is_wide_close(ch) {
                bracket_depth = bracket_depth.saturating_sub(1);
            }
            prev = Some(ch);
        }
        let mut row_start = 0usize;
        while row_start < chars.len() {
            let mut col = 0usize;
            let mut last_break = None;
            let mut last_fallback = None;
            let mut split = None;
            for (index, &(ch, _)) in chars.iter().enumerate().skip(row_start) {
                if index > row_start {
                    if breakable[index] {
                        last_break = Some(index);
                    }
                    if fallback[index] {
                        last_fallback = Some(index);
                    }
                }
                let ch_width = char_display_width(ch);
                if col + ch_width > width && index > row_start {
                    split = Some(last_break.or(last_fallback).unwrap_or(index));
                    break;
                }
                col += ch_width;
            }
            let row_end = split.unwrap_or(chars.len());
            let mut trimmed = row_end;
            while trimmed > row_start && chars[trimmed - 1].0 == ' ' {
                trimmed -= 1;
            }
            out.push(build_wrapped_span_line(&chars[row_start..trimmed]));
            let Some(split) = split else {
                break;
            };
            row_start = split;
            while row_start < chars.len() && chars[row_start].0 == ' ' {
                row_start += 1;
            }
        }
    }
    out
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
        lines.extend(preview_lines(form, saved, base, p));
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
    if let (false, Some(prompt)) = (layout.prompt.is_empty(), form.prompt) {
        let warn = base.fg(p.yellow);
        render_lines(
            b,
            layout.prompt,
            vec![Line::from(vec![
                Span::styled(PROMPT_ICON, warn.add_modifier(Modifier::BOLD)),
                Span::styled(prompt_text(prompt), warn),
            ])],
        );
    }
    let action_hits = render_machine_footer(b, layout.footer, &form_hints(form), cx);
    // 确认条在时表单只读：不给输入光标。
    let cursor = if form.prompt.is_some() { None } else { cursor };

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

#[cfg(test)]
mod wrap_tests {
    use super::*;

    fn row_texts(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    fn wrap_text(text: &str, width: u16) -> Vec<String> {
        row_texts(&wrap_lines_for_display(
            vec![Line::raw(text.to_owned())],
            width,
        ))
    }

    /// L14 复审：词内的 ASCII 标点不是断点。预览栏 43 列（130 列终端）下，含
    /// 点分主机名 / IP / 路径的 ssh 命令只在空白处折行，主机名整体换到下一行；
    /// 旧实现折成 `…build-server.corp.` / `example.com`。
    #[test]
    fn ssh_commands_wrap_only_at_whitespace() {
        assert_eq!(
            wrap_text("ssh -p 2222 deploy@build-server.corp.example.com", 43),
            ["ssh -p 2222", "deploy@build-server.corp.example.com"]
        );
        assert_eq!(
            wrap_text("ssh -p 2222 -i ~/.ssh/id_ed25519 deploy@203.0.113.5", 30),
            ["ssh -p 2222 -i", "~/.ssh/id_ed25519", "deploy@203.0.113.5"]
        );
        // 字段清单行：标签后的长主机名整体换行，不在 `.` 后拦腰断开。
        let row = format!("目标{}build-server.corp.example.com", " ".repeat(16));
        assert_eq!(
            wrap_text(&row, 43),
            ["目标", "build-server.corp.example.com"]
        );
    }

    /// 一个词本身比整行还宽时只能在词内断：退而断在 `.` `/` 等分隔符之后，
    /// 连分隔符都没有才按字符硬断。
    #[test]
    fn an_oversized_word_breaks_after_a_separator() {
        assert_eq!(
            wrap_text("deploy@build-server.corp.example.com", 20),
            ["deploy@build-server.", "corp.example.com"]
        );
        assert_eq!(wrap_text("abcdefghij", 4), ["abcd", "efgh", "ij"]);
    }

    /// L14：CJK 句子按字符可断（UAX #14），行尽量填满，不再出现「此处 SSH」
    /// 独占一行的短行；中文标点不落到行首（避头），「（无效）」这样的括号
    /// 短语不拆开。
    #[test]
    fn cjk_sentences_fill_rows_and_keep_punctuation_off_row_starts() {
        let t = &crate::i18n::texts_for(crate::i18n::Lang::ZhCn).machines;
        // 冒烟 `81` 行 16–20 的两段说明在 43 列预览栏下的折行。
        assert_eq!(
            wrap_text(t.confirm_auth_note, 43),
            [
                "此处 SSH 不会交互提问；请确保密钥认证与主机",
                "密钥已可用。"
            ]
        );
        assert_eq!(
            wrap_text(t.confirm_install_note, 43),
            [
                "Herdr 会检查远程机器，并在必要时安装、更新",
                "或重启其 server。"
            ]
        );
        for note in [t.confirm_auth_note, t.confirm_install_note] {
            for width in [8u16, 12, 20, 26, 30, 43] {
                let rows = wrap_text(note, width);
                for row in &rows {
                    assert!(
                        display_width(row) <= width,
                        "宽度 {width} 下这一行超宽：{row:?}"
                    );
                    assert!(
                        !row.starts_with(no_break_before),
                        "宽度 {width} 下标点落到了行首：{rows:?}"
                    );
                }
                let rebuilt: String = rows
                    .concat()
                    .chars()
                    .filter(|c| !c.is_whitespace())
                    .collect();
                let original: String = note.chars().filter(|c| !c.is_whitespace()).collect();
                assert_eq!(rebuilt, original, "折行不能丢字：{rows:?}");
            }
        }
    }

    /// 值与紧跟的「（无效）」保持在同一行：标签后的空白处换行，括号不拆。
    #[test]
    fn a_value_keeps_its_invalid_suffix_on_the_same_row() {
        let red = Style::default().fg(crate::app::state::Palette::catppuccin().red);
        let line = Line::from(vec![
            Span::raw(format!("端口{}", " ".repeat(14))),
            Span::styled("99999（无效）", red),
        ]);
        let wrapped = wrap_lines_for_display(vec![line], 30);
        assert_eq!(row_texts(&wrapped), ["端口", "99999（无效）"]);
        assert!(
            wrapped[1].spans.iter().all(|span| span.style == red),
            "折行后红色样式仍要在：{wrapped:?}"
        );
    }

    /// 回归：旧实现只读 span 自己的样式，`Line::styled` 的行级颜色（安装说明
    /// 的 yellow、测试步骤的绿 / 红）折行后全部丢成终端默认色。
    #[test]
    fn line_level_styles_survive_wrapping() {
        let palette = crate::app::state::Palette::catppuccin();
        let yellow = Style::default().fg(palette.yellow);
        let wrapped = wrap_lines_for_display(
            vec![Line::styled("Herdr 会检查远程机器，并在必要时安装", yellow)],
            12,
        );
        assert!(wrapped.len() > 1, "12 列下必须折行");
        for line in &wrapped {
            for span in &line.spans {
                assert_eq!(span.style.fg, Some(palette.yellow), "{wrapped:?}");
            }
        }
        // span 自己的样式叠在行级样式之上。
        let red = Style::default().fg(palette.red);
        let wrapped = wrap_lines_for_display(
            vec![Line::from(vec![Span::raw("ok "), Span::styled("bad", red)]).style(yellow)],
            40,
        );
        assert_eq!(wrapped[0].spans[0].style.fg, Some(palette.yellow));
        assert_eq!(wrapped[0].spans[1].style.fg, Some(palette.red));
    }

    /// 确认条的行数与渲染同一个折行函数：两边口径不一致会让最后一行被裁掉。
    #[test]
    fn prompt_rows_match_the_rendered_wrap() {
        for prompt in [FormPrompt::ConfirmTest, FormPrompt::Discard] {
            for width in [24u16, 30, 40, 60, 100] {
                let line = Line::from(vec![Span::raw(PROMPT_ICON), Span::raw(prompt_text(prompt))]);
                let rendered = wrap_lines_for_display(vec![line], width).len();
                let expected = u16::try_from(rendered)
                    .unwrap_or(PROMPT_MAX_ROWS)
                    .clamp(1, PROMPT_MAX_ROWS);
                assert_eq!(prompt_rows(prompt, width), expected, "{prompt:?} @ {width}");
            }
        }
    }
}
