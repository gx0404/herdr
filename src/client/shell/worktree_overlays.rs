use super::*;

pub(super) fn render_worktree_create_overlay(
    b: &mut Buffer,
    create: &ClientWorktreeCreateOverlay,
    p: &Palette,
) -> Option<OverlayRender> {
    let popup = popup(b.area, 68, 12)?;
    let inner = panel(b, popup, p.accent, p.panel_bg)?;
    put_text(
        b,
        inner.x,
        inner.y,
        inner.width,
        crate::i18n::texts().worktree.new_worktree,
        Style::default()
            .fg(p.text)
            .bg(p.panel_bg)
            .add_modifier(Modifier::BOLD),
    );
    put_text(
        b,
        inner.x,
        inner.y + 2,
        inner.width,
        crate::i18n::texts().worktree.branch_hint,
        Style::default().fg(p.overlay0).bg(p.panel_bg),
    );
    let input = Rect::new(inner.x, inner.y + 3, inner.width, 1);
    b.set_style(input, Style::default().fg(p.text).bg(p.surface0));
    let cursor = text_editor::render(
        b,
        Rect::new(input.x + 1, input.y, input.width.saturating_sub(1), 1),
        &create.branch,
        Style::default().fg(p.text).bg(p.surface0),
    );
    put_text(
        b,
        inner.x,
        inner.y + 5,
        inner.width,
        crate::i18n::texts().worktree.checkout_hint,
        Style::default().fg(p.overlay0).bg(p.panel_bg),
    );
    put_text(
        b,
        inner.x,
        inner.y + 6,
        inner.width,
        &format!(" {}", create.checkout_path),
        Style::default().fg(p.subtext0).bg(p.panel_bg),
    );
    if create.creating {
        put_text(
            b,
            inner.x,
            inner.y + 8,
            inner.width,
            crate::i18n::texts().worktree.creating,
            Style::default().fg(p.accent).bg(p.panel_bg),
        );
    } else if let Some(error) = create.error.as_deref() {
        put_text(
            b,
            inner.x,
            inner.y + 8,
            inner.width,
            &format!(" {error}"),
            Style::default().fg(p.red).bg(p.panel_bg),
        );
    }
    let create_label = crate::i18n::texts().worktree.create_and_open;
    let cancel_label = crate::i18n::texts().overlays.cancel_button;
    let buttons = row(
        inner,
        &[display_width(create_label), display_width(cancel_label)],
        2,
        9,
    );
    let [primary, cancel] = buttons.as_slice() else {
        return None;
    };
    button(
        b,
        *primary,
        create_label,
        Style::default()
            .fg(contrast(p))
            .bg(p.accent)
            .add_modifier(Modifier::BOLD),
    );
    button(
        b,
        *cancel,
        cancel_label,
        Style::default()
            .fg(p.text)
            .bg(p.surface0)
            .add_modifier(Modifier::BOLD),
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
    p: &Palette,
) -> Option<OverlayRender> {
    let popup_height = (open.entries.len().saturating_mul(2) + 7).clamp(12, 26) as u16;
    let popup = popup(b.area, 96, popup_height)?;
    let inner = panel(b, popup, p.accent, p.panel_bg)?;
    put_text(
        b,
        inner.x,
        inner.y,
        inner.width,
        crate::i18n::texts().worktree.open_worktree,
        Style::default()
            .fg(p.text)
            .bg(p.panel_bg)
            .add_modifier(Modifier::BOLD),
    );
    let search = Rect::new(inner.x, inner.y + 1, inner.width, 1);
    let filtered = open.filtered_indices();
    put_text(
        b,
        search.x,
        search.y,
        search.width,
        &if open.search_focused {
            " / ".to_owned()
        } else if !open.query.is_empty() {
            format!(" / {}", open.query)
        } else {
            crate::i18n::texts().worktree.filter_worktrees.to_owned()
        },
        Style::default()
            .fg(if open.search_focused {
                p.text
            } else {
                p.overlay0
            })
            .bg(p.panel_bg),
    );
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
    let cursor = if open.search_focused {
        text_editor::render(
            b,
            Rect::new(
                search.x + 3,
                search.y,
                search.width.saturating_sub(4 + display_width(&count)),
                1,
            ),
            &open.query,
            Style::default().fg(p.text).bg(p.panel_bg),
        )
    } else {
        None
    };
    put_right_text(
        b,
        search,
        search.y,
        &count,
        Style::default().fg(p.overlay0).bg(p.panel_bg),
    );
    put_text(
        b,
        inner.x,
        inner.y + 2,
        inner.width,
        &"─".repeat(inner.width as usize),
        Style::default().fg(p.surface1).bg(p.panel_bg),
    );
    let body = Rect::new(
        inner.x,
        inner.y + 3,
        inner.width,
        inner.height.saturating_sub(6),
    );
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
        let style = if selected {
            Style::default().fg(contrast(p)).bg(p.accent)
        } else {
            Style::default().fg(p.text).bg(p.panel_bg)
        };
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
            put_right_text(b, rect, rect.y, status, style);
        }
        put_text(
            b,
            rect.x,
            rect.y + 1,
            rect.width,
            &format!(" {}", entry.path),
            if selected {
                style
            } else {
                Style::default().fg(p.overlay0).bg(p.panel_bg)
            },
        );
    }
    if filtered.is_empty() {
        put_text(
            b,
            body.x,
            body.y,
            body.width,
            crate::i18n::texts().worktree.no_matching,
            Style::default().fg(p.overlay0).bg(p.panel_bg),
        );
    }
    if open.opening {
        put_text(
            b,
            inner.x,
            inner.bottom() - 3,
            inner.width,
            crate::i18n::texts().worktree.opening,
            Style::default().fg(p.accent).bg(p.panel_bg),
        );
    } else if let Some(error) = open.error.as_deref() {
        put_text(
            b,
            inner.x,
            inner.bottom() - 3,
            inner.width,
            &format!(" {error}"),
            Style::default().fg(p.red).bg(p.panel_bg),
        );
    }
    let open_label = crate::i18n::texts().worktree.open_button;
    let cancel_label = crate::i18n::texts().overlays.cancel_button;
    let buttons = row(
        inner,
        &[display_width(open_label), display_width(cancel_label)],
        2,
        inner.height.saturating_sub(1),
    );
    let [primary, cancel] = buttons.as_slice() else {
        return None;
    };
    button(
        b,
        *primary,
        open_label,
        Style::default()
            .fg(contrast(p))
            .bg(p.accent)
            .add_modifier(Modifier::BOLD),
    );
    button(
        b,
        *cancel,
        cancel_label,
        Style::default()
            .fg(p.text)
            .bg(p.surface0)
            .add_modifier(Modifier::BOLD),
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
    p: &Palette,
) -> Option<OverlayRender> {
    let popup = popup(b.area, 72, 10)?;
    let inner = panel(b, popup, p.red, p.panel_bg)?;
    put_text(
        b,
        inner.x,
        inner.y,
        inner.width,
        crate::i18n::texts().worktree.delete_title,
        Style::default()
            .fg(p.red)
            .bg(p.panel_bg)
            .add_modifier(Modifier::BOLD),
    );
    put_text(
        b,
        inner.x,
        inner.y + 1,
        inner.width,
        crate::i18n::texts().worktree.removes_folder,
        Style::default().fg(p.text).bg(p.panel_bg),
    );
    put_text(
        b,
        inner.x,
        inner.y + 2,
        inner.width,
        &format!(" {}", remove.path),
        Style::default().fg(p.subtext0).bg(p.panel_bg),
    );
    put_text(
        b,
        inner.x,
        inner.y + 3,
        inner.width,
        crate::i18n::texts().worktree.branch_not_deleted,
        Style::default().fg(p.text).bg(p.panel_bg),
    );
    if remove.force_confirmation {
        put_text(
            b,
            inner.x,
            inner.y + 4,
            inner.width,
            crate::i18n::texts().worktree.dirty_warning,
            Style::default().fg(p.red).bg(p.panel_bg),
        );
    }
    if remove.removing {
        put_text(
            b,
            inner.x,
            inner.y + 5,
            inner.width,
            crate::i18n::texts().worktree.removing,
            Style::default().fg(p.accent).bg(p.panel_bg),
        );
    } else if let Some(error) = remove.error.as_deref() {
        put_text(
            b,
            inner.x,
            inner.y + 5,
            inner.width,
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
    let buttons = row(
        inner,
        &[display_width(primary_label), display_width(cancel_label)],
        2,
        7,
    );
    let [primary, cancel] = buttons.as_slice() else {
        return None;
    };
    button(
        b,
        *primary,
        primary_label,
        Style::default()
            .fg(contrast(p))
            .bg(p.red)
            .add_modifier(Modifier::BOLD),
    );
    button(
        b,
        *cancel,
        cancel_label,
        Style::default()
            .fg(p.text)
            .bg(p.surface0)
            .add_modifier(Modifier::BOLD),
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
