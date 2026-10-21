use super::*;

pub(super) fn render_worktree_create_overlay(
    b: &mut Buffer,
    create: &ClientWorktreeCreateOverlay,
    cx: &super::feedback::ChromeContext<'_>,
) -> Option<OverlayRender> {
    let p = cx.palette;
    let (popup, inner) = modal_panel(
        b,
        crate::ui::ModalSize::Content {
            width: 68,
            height: 12,
        },
        cx,
    )?;
    let stack = crate::ui::modal_stack_areas(inner, 1, 0, 1, 1);
    put_text(
        b,
        stack.header.x,
        stack.header.y,
        stack.header.width,
        crate::i18n::texts().worktree.new_worktree,
        crate::ui::design::title(p).bg(p.panel_bg),
    );
    let content = stack.content;
    put_text(
        b,
        content.x,
        content.y,
        content.width,
        crate::i18n::texts().worktree.branch_hint,
        Style::default().fg(p.overlay1).bg(p.panel_bg),
    );
    let input = Rect::new(content.x, content.y + 1, content.width, 1);
    let field = crate::ui::input_field_style(p);
    b.set_style(input, field);
    let cursor = text_editor::render(
        b,
        Rect::new(input.x + 1, input.y, input.width.saturating_sub(1), 1),
        &create.branch,
        field,
    );
    put_text(
        b,
        content.x,
        content.y + 3,
        content.width,
        crate::i18n::texts().worktree.checkout_hint,
        Style::default().fg(p.overlay1).bg(p.panel_bg),
    );
    put_text(
        b,
        content.x,
        content.y + 4,
        content.width,
        &format!(" {}", create.checkout_path),
        Style::default().fg(p.subtext0).bg(p.panel_bg),
    );
    let status_y = stack
        .actions
        .map(|actions| actions.y.saturating_sub(1))
        .unwrap_or(content.bottom());
    if create.creating {
        put_text(
            b,
            content.x,
            status_y,
            content.width,
            &format!("{} {}", cx.spinner, crate::i18n::texts().worktree.creating),
            Style::default().fg(p.accent).bg(p.panel_bg),
        );
    } else if let Some(error) = create.error.as_deref() {
        put_text(
            b,
            content.x,
            status_y,
            content.width,
            &format!(" {error}"),
            Style::default().fg(p.red).bg(p.panel_bg),
        );
    }
    let create_label = crate::i18n::texts().worktree.create_and_open;
    let cancel_label = crate::i18n::texts().overlays.cancel_button;
    let buttons = modal_button_row(
        stack.actions.unwrap_or_default(),
        &[create_label, cancel_label],
        2,
    );
    let [primary, cancel] = buttons.as_slice() else {
        return None;
    };
    modal_button(
        b,
        *primary,
        create_label,
        crate::ui::ModalButtonTone::Primary,
        if create.creating {
            crate::ui::ModalButtonState::Disabled
        } else {
            cx.button_state(
                &super::feedback::ChromeHover::OverlayPrimary,
                crate::ui::ModalButtonState::Focused,
            )
        },
        p,
    );
    modal_button(
        b,
        *cancel,
        cancel_label,
        crate::ui::ModalButtonTone::Secondary,
        cx.button_state(
            &super::feedback::ChromeHover::OverlayCancel,
            crate::ui::ModalButtonState::Normal,
        ),
        p,
    );
    Some(OverlayRender {
        area: popup,
        primary: *primary,
        clear: Rect::default(),
        cancel: *cancel,
        navigator_popup: Rect::default(),
        navigator_search: Rect::default(),
        navigator_rows: Vec::new(),
        worktree_search: Rect::default(),
        worktree_rows: Vec::new(),
        cursor: cursor.filter(|_| !create.creating),
        ..OverlayRender::default()
    })
}

pub(super) fn render_worktree_open_overlay(
    b: &mut Buffer,
    open: &ClientWorktreeOpenOverlay,
    cx: &super::feedback::ChromeContext<'_>,
) -> Option<OverlayRender> {
    let p = cx.palette;
    let popup_height = (open.entries.len().saturating_mul(2) + 7).clamp(12, 26) as u16;
    let (popup, inner) = modal_panel(
        b,
        crate::ui::ModalSize::Content {
            width: 96,
            height: popup_height,
        },
        cx,
    )?;
    let stack = crate::ui::modal_stack_areas(inner, 2, 1, 1, 1);
    put_text(
        b,
        stack.header.x,
        stack.header.y,
        stack.header.width,
        crate::i18n::texts().worktree.open_worktree,
        crate::ui::design::title(p).bg(p.panel_bg),
    );
    let search = Rect::new(stack.header.x, stack.header.y + 1, stack.header.width, 1);
    let filtered = open.filtered_indices();
    let count = if filtered.len() == open.entries.len() {
        crate::i18n::fill(
            crate::i18n::texts().worktree.checkouts_fmt,
            &[("count", &open.entries.len().to_string())],
        )
    } else {
        crate::i18n::fill(
            crate::i18n::texts().worktree.checkouts_filtered_fmt,
            &[
                ("filtered", &filtered.len().to_string()),
                ("count", &open.entries.len().to_string()),
            ],
        )
    };
    let cursor = render_search_bar(
        b,
        search,
        &SearchBar {
            focused: open.search_focused,
            query: &open.query,
            hint: crate::i18n::texts().worktree.filter_worktrees,
            status: None,
            echo_query: true,
            count: Some(count),
        },
        p,
    );
    put_text(
        b,
        inner.x,
        stack.header.bottom(),
        inner.width,
        &"─".repeat(inner.width as usize),
        Style::default().fg(p.surface1).bg(p.panel_bg),
    );
    let body = stack.content;
    let visible_count = (body.height / 2).max(1) as usize;
    let selected_position = filtered
        .iter()
        .position(|index| *index == open.selected)
        .unwrap_or(0);
    let start = selected_position
        .saturating_sub(visible_count.saturating_sub(1))
        .min(filtered.len().saturating_sub(visible_count));
    let mut row_hits = Vec::new();
    for (visible, entry_index) in filtered
        .iter()
        .copied()
        .skip(start)
        .take(visible_count)
        .enumerate()
    {
        let entry = &open.entries[entry_index];
        let rect = Rect::new(body.x, body.y + visible as u16 * 2, body.width, 2);
        row_hits.push((rect, entry_index));
        let selected = entry_index == open.selected;
        // 选中：中性选中底色 + 粗体 + 首列 ▌（不再 accent 反色）。
        let style = list_row_style(p, cx.components, selected, false);
        b.set_style(rect, style);
        put_text(
            b,
            rect.x,
            rect.y,
            rect.width,
            &format!(" {}", entry.label),
            style.add_modifier(Modifier::BOLD),
        );
        let status = entry.status_label();
        if !status.is_empty() {
            put_right_text(
                b,
                rect,
                rect.y,
                status,
                style.fg(p.overlay1).remove_modifier(Modifier::BOLD),
            );
        }
        // 第二行是检出路径（说明层级）：选中行也保持说明色，只换底色。
        put_text(
            b,
            rect.x,
            rect.y + 1,
            rect.width,
            &format!(" {}", entry.path),
            style.fg(p.subtext0).remove_modifier(Modifier::BOLD),
        );
        paint_list_selection(b, rect, selected, p);
    }
    if filtered.is_empty() {
        // 区分「这个仓库还没有工作树」与「筛选无匹配」。
        let title = if open.entries.is_empty() {
            super::observability::tr("No worktrees yet", "还没有工作树")
        } else {
            crate::i18n::texts().worktree.no_matching.trim()
        };
        crate::ui::kit::empty_state::render_empty_state(
            b,
            body,
            &crate::ui::kit::empty_state::EmptyState {
                glyph: Some("◌"),
                title,
                body: None,
                action: None,
            },
            p,
        );
    }
    let status_y = stack.footer.map(|footer| footer.y).unwrap_or(body.bottom());
    if open.opening {
        put_text(
            b,
            inner.x,
            status_y,
            inner.width,
            &format!("{} {}", cx.spinner, crate::i18n::texts().worktree.opening),
            Style::default().fg(p.accent).bg(p.panel_bg),
        );
    } else if let Some(error) = open.error.as_deref() {
        put_text(
            b,
            inner.x,
            status_y,
            inner.width,
            &format!(" {error}"),
            Style::default().fg(p.red).bg(p.panel_bg),
        );
    }
    let open_label = crate::i18n::texts().worktree.open_button;
    let cancel_label = crate::i18n::texts().overlays.cancel_button;
    let buttons = modal_button_row(
        stack.actions.unwrap_or_default(),
        &[open_label, cancel_label],
        2,
    );
    let [primary, cancel] = buttons.as_slice() else {
        return None;
    };
    modal_button(
        b,
        *primary,
        open_label,
        crate::ui::ModalButtonTone::Primary,
        if open.opening {
            crate::ui::ModalButtonState::Disabled
        } else {
            cx.button_state(
                &super::feedback::ChromeHover::OverlayPrimary,
                crate::ui::ModalButtonState::Focused,
            )
        },
        p,
    );
    modal_button(
        b,
        *cancel,
        cancel_label,
        crate::ui::ModalButtonTone::Secondary,
        cx.button_state(
            &super::feedback::ChromeHover::OverlayCancel,
            crate::ui::ModalButtonState::Normal,
        ),
        p,
    );
    Some(OverlayRender {
        area: popup,
        primary: *primary,
        clear: Rect::default(),
        cancel: *cancel,
        navigator_popup: Rect::default(),
        navigator_search: Rect::default(),
        navigator_rows: Vec::new(),
        worktree_search: search,
        worktree_rows: row_hits,
        cursor: cursor.filter(|_| !open.opening),
        ..OverlayRender::default()
    })
}

pub(super) fn render_worktree_remove_overlay(
    b: &mut Buffer,
    remove: &ClientWorktreeRemoveOverlay,
    cx: &super::feedback::ChromeContext<'_>,
) -> Option<OverlayRender> {
    let p = cx.palette;
    let (popup, inner) = modal_panel(
        b,
        crate::ui::ModalSize::Content {
            width: 72,
            height: 10,
        },
        cx,
    )?;
    let stack = crate::ui::modal_stack_areas(inner, 1, 0, 1, 0);
    let content = stack.content;
    put_text(
        b,
        stack.header.x,
        stack.header.y,
        stack.header.width,
        crate::i18n::texts().worktree.delete_title,
        // 浮层边框是中性色：删除的危险语义落在红色标题与危险按钮上。
        crate::ui::design::title(p).fg(p.red).bg(p.panel_bg),
    );
    put_text(
        b,
        content.x,
        content.y,
        content.width,
        crate::i18n::texts().worktree.removes_folder,
        Style::default().fg(p.text).bg(p.panel_bg),
    );
    put_text(
        b,
        content.x,
        content.y + 1,
        content.width,
        &format!(" {}", remove.path),
        Style::default().fg(p.subtext0).bg(p.panel_bg),
    );
    put_text(
        b,
        content.x,
        content.y + 2,
        content.width,
        crate::i18n::texts().worktree.branch_not_deleted,
        Style::default().fg(p.text).bg(p.panel_bg),
    );
    if remove.force_confirmation {
        put_text(
            b,
            content.x,
            content.y + 3,
            content.width,
            crate::i18n::texts().worktree.dirty_warning,
            Style::default().fg(p.red).bg(p.panel_bg),
        );
        // 强制删除换了确认键，必须在浮层里写清楚，否则回车「没反应」像是卡住。
        put_text(
            b,
            content.x,
            content.y + 5,
            content.width,
            crate::i18n::texts().worktree.force_confirm_hint,
            Style::default().fg(p.subtext0).bg(p.panel_bg),
        );
    }
    if remove.removing {
        put_text(
            b,
            content.x,
            content.y + 4,
            content.width,
            &format!("{} {}", cx.spinner, crate::i18n::texts().worktree.removing),
            Style::default().fg(p.accent).bg(p.panel_bg),
        );
    } else if let Some(error) = remove.error.as_deref() {
        put_text(
            b,
            content.x,
            content.y + 4,
            content.width,
            &format!(" {error}"),
            Style::default().fg(p.red).bg(p.panel_bg),
        );
    }
    let primary_label = if remove.force_confirmation {
        crate::i18n::texts().worktree.delete_anyway
    } else {
        crate::i18n::texts().worktree.remove
    };
    let cancel_label = crate::i18n::texts().overlays.cancel_button;
    let buttons = modal_button_row(
        stack.actions.unwrap_or_default(),
        &[primary_label, cancel_label],
        2,
    );
    let [primary, cancel] = buttons.as_slice() else {
        return None;
    };
    modal_button(
        b,
        *primary,
        primary_label,
        crate::ui::ModalButtonTone::Danger,
        if remove.removing {
            crate::ui::ModalButtonState::Disabled
        } else {
            // 武装强制删除后主按钮不再是默认强调态：这一步要用户明确瞄准，
            // 不该让「继续按同一个默认动作」显得顺理成章。
            cx.button_state(
                &super::feedback::ChromeHover::OverlayPrimary,
                if remove.force_confirmation {
                    crate::ui::ModalButtonState::Normal
                } else {
                    crate::ui::ModalButtonState::Focused
                },
            )
        },
        p,
    );
    modal_button(
        b,
        *cancel,
        cancel_label,
        crate::ui::ModalButtonTone::Secondary,
        cx.button_state(
            &super::feedback::ChromeHover::OverlayCancel,
            // Danger 色调下 Focused 与 Normal 的样式表结果相同，所以「降级主按钮」
            // 只靠它看不出来：强制删除这一步把默认强调态交给取消按钮，视觉上的
            // 默认动作就是放弃。
            if remove.force_confirmation {
                crate::ui::ModalButtonState::Focused
            } else {
                crate::ui::ModalButtonState::Normal
            },
        ),
        p,
    );
    Some(OverlayRender {
        area: popup,
        primary: *primary,
        clear: Rect::default(),
        cancel: *cancel,
        navigator_popup: Rect::default(),
        navigator_search: Rect::default(),
        navigator_rows: Vec::new(),
        worktree_search: Rect::default(),
        worktree_rows: Vec::new(),
        cursor: None,
        ..OverlayRender::default()
    })
}
