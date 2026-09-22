//! 窄屏机器列表、详情卡与删除确认的渲染，以及 List / dashboard / Detail
//! 共用的页脚键表。

use super::*;

fn machine_color_tag(
    profile_color: Option<&str>,
    label: &str,
    p: &Palette,
) -> ratatui::style::Color {
    if let Some(color) = profile_color.and_then(crate::config::try_parse_color) {
        return color;
    }
    machine_hash_color(label, p)
}

/// Stable fallback tag for profiles without an explicit color: hash the label
/// into a small rotation of palette hues.
pub(in crate::client::shell) fn machine_hash_color(
    label: &str,
    p: &Palette,
) -> ratatui::style::Color {
    let hues = [p.blue, p.teal, p.green, p.yellow, p.peach, p.mauve, p.red];
    let hash = label.bytes().fold(0u32, |acc, byte| {
        acc.wrapping_mul(31).wrapping_add(u32::from(byte))
    });
    hues[(hash as usize) % hues.len()]
}

/// List 与宽屏 dashboard 共用的页脚键表：与 `handle_machine_action_key` 的
/// 动作表、dashboard 动作网格同序同集（HERDR-MACH-007）。
///
/// 顺序就是「丢弃优先级」：`render_key_hints` 填满 `area` 的行之后整项丢弃
/// 并画 `…`，所以页面级键（`/` 过滤、`esc` 关闭、`b` 广播——这三个在 List /
/// dashboard 上没有等效按钮或搜索框以外的入口）排在所有单机动作键之前，
/// 而末尾的 `a` 添加 / `i` 导入在两个视图里都有对应按钮兜底。`c` 的出现
/// 条件与按键一致（有选中即可用），不再按 Attention 门禁——侧栏右键菜单的
/// 「复制修复命令」同样不分状态。
pub(super) fn machine_list_hints(has_selection: bool, has_review: bool) -> Vec<(String, String)> {
    let t = &crate::i18n::texts().machines;
    let mut hints = vec![
        ("↑↓".to_owned(), t.hint_select.to_owned()),
        ("enter".to_owned(), t.hint_details.to_owned()),
        ("/".to_owned(), t.hint_filter.to_owned()),
        ("esc".to_owned(), t.hint_close.to_owned()),
        ("b".to_owned(), t.hint_broadcast.to_owned()),
    ];
    if has_selection {
        hints.extend(machine_action_hints(has_review));
    }
    hints.extend([
        ("a".to_owned(), t.hint_add.to_owned()),
        ("i".to_owned(), t.hint_import.to_owned()),
    ]);
    hints
}

/// Detail 页脚：与 List / dashboard 同一张单机动作表，只是把页面级键换成
/// `esc 返回` 与 `b 广播`（Detail 没有列表导航与添加 / 导入）。
/// 详情卡底部的动作标签：渲染与视图计算阶段共用——`action_row_count` 用它
/// 决定给按钮留几行，两边必须同源（STATE-04）。
pub(super) fn machine_detail_labels(
    profile: &SavedSshEndpoint,
    endpoints: &[ClientShellEndpoint],
    connection_errors: &HashMap<ClientEndpointId, crate::remote::ConnectionErrorKind>,
) -> Vec<&'static str> {
    let t = &crate::i18n::texts().machines;
    let status = endpoint_for(endpoints, &profile.id).map_or(
        if profile.enabled {
            ClientEndpointStatus::Connecting
        } else {
            ClientEndpointStatus::Disabled
        },
        |endpoint| endpoint.status,
    );
    let has_review = connection_errors
        .get(&ClientEndpointId::Ssh(profile.id.clone()))
        .is_some_and(super::super::machine_auth_overlay::failure_kind_has_review);
    let mut labels: Vec<&'static str> = vec![t.reconnect_button, t.edit_button];
    labels.push(if profile.enabled {
        t.disable_button
    } else {
        t.enable_button
    });
    labels.push(t.forwards_button);
    labels.push(t.browse_files_button);
    labels.push(t.broadcast_button);
    labels.push(t.remove_button);
    if status == ClientEndpointStatus::Attention && has_review {
        labels.push(crate::i18n::texts().machine_auth.review_button);
    }
    if status == ClientEndpointStatus::Attention {
        labels.push(t.copy_fix_button);
    }
    labels.push(crate::ui::modal_close_button_text());
    labels
}

pub(super) fn machine_detail_hints(has_review: bool) -> Vec<(String, String)> {
    let t = &crate::i18n::texts().machines;
    let mut hints = vec![
        ("esc".to_owned(), t.hint_back.to_owned()),
        ("b".to_owned(), t.hint_broadcast.to_owned()),
    ];
    hints.extend(machine_action_hints(has_review));
    hints
}

/// 单机动作键：三个视图（List / 宽屏 dashboard / Detail）同序同集，出现
/// 条件也只在这一处判定（HERDR-MACH-007）。`v` 只在失败类型真有界面内恢复
/// 路径时出现（`open_machine_auth_for_endpoint` 否则是空操作）；`c` 与按键
/// 一样不分状态，侧栏右键菜单的「复制修复命令」同样不分状态。
fn machine_action_hints(has_review: bool) -> Vec<(String, String)> {
    let t = &crate::i18n::texts().machines;
    let mut hints = Vec::with_capacity(9);
    if has_review {
        hints.push(("v".to_owned(), t.hint_review.to_owned()));
    }
    hints.extend([
        ("r".to_owned(), t.hint_reconnect.to_owned()),
        ("e".to_owned(), t.hint_edit.to_owned()),
        ("f".to_owned(), t.hint_forwards.to_owned()),
        ("o".to_owned(), t.hint_browse_files.to_owned()),
        ("d".to_owned(), t.hint_toggle_enabled.to_owned()),
        ("x".to_owned(), t.hint_remove.to_owned()),
        ("R".to_owned(), t.hint_rename.to_owned()),
        ("c".to_owned(), t.hint_copy_fix.to_owned()),
    ]);
    hints
}

/// 机器页页脚（List / 宽屏 dashboard / Detail）要的行数：完整键表在单行里
/// 必然被尾部截断，所以按实际宽度取，最多两行——再多会把列表可见行数吃掉。
/// 两行仍放不下时按上面的顺序从尾部丢（`R` / `c` / `a` / `i`，这几个都还有
/// 按钮或侧栏右键菜单兜底）。
pub(super) fn machine_footer_rows(hints: &[(String, String)], width: u16) -> u16 {
    super::super::render::key_hints_rows(hints, width).clamp(1, 2)
}

pub(super) fn render_machine_list(
    b: &mut Buffer,
    overlay: &ClientMachinesOverlay,
    endpoints: &[ClientShellEndpoint],
    saved_profiles: &[SavedSshEndpoint],
    connection_errors: &HashMap<ClientEndpointId, crate::remote::ConnectionErrorKind>,
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
    let rows = machine_list_rows(saved_profiles, endpoints, overlay.query.as_str());
    // `v 处理失败` 的出现条件与 dashboard 同一张表：用选中机器的失败类型，
    // 而不是硬编码 false（窄屏 List 此前永远不显示它）。
    let selected = if rows.is_empty() {
        0
    } else {
        overlay.selected.min(rows.len() - 1)
    };
    let has_review = rows.get(selected).is_some_and(|row| {
        connection_errors
            .get(&ClientEndpointId::Ssh(row.id.clone()))
            .is_some_and(super::super::machine_auth_overlay::failure_kind_has_review)
    });
    let hints = machine_list_hints(!rows.is_empty(), has_review);
    let stack =
        crate::ui::modal_stack_areas(inner, 2, machine_footer_rows(&hints, inner.width), 1, 1);
    let base = Style::default()
        .bg(p.panel_bg)
        .remove_modifier(Modifier::DIM);
    put_text(
        b,
        stack.header.x,
        stack.header.y,
        stack.header.width,
        &format!(" {}", t.title),
        base.fg(p.text).add_modifier(Modifier::BOLD),
    );
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
    let scroll = super::super::page::list_start(
        overlay.scroll,
        selected,
        rows.len(),
        visible,
        overlay.reveal,
    );
    let mut row_hits = Vec::new();
    for (index, row) in rows.iter().enumerate().skip(scroll).take(visible) {
        let y = body.y + ((index - scroll) * row_height) as u16;
        let rect = Rect::new(body.x, y, body.width, row_height as u16);
        row_hits.push((rect, row.id.clone()));
        let is_selected = index == selected;
        // 三态与其它浮层同一口径：选中 accent 反色 > 悬浮弱底色 > 常态。
        let is_hovered = !is_selected && overlay.hovered.as_ref() == Some(&row.id);
        let row_bg = super::super::list_row_bg(p, cx.components, is_selected, is_hovered);
        let style = if is_selected {
            Style::default()
                .fg(panel_contrast_fg(p))
                .bg(p.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(p.text).bg(row_bg)
        };
        b.set_style(rect, style);
        let tag = machine_color_tag(row.color.as_deref(), &row.label, p);
        let tag_style = if is_selected {
            style
        } else {
            Style::default().fg(tag).bg(row_bg)
        };
        put_text(b, rect.x, rect.y, 2, " ▪", tag_style);
        put_text(
            b,
            rect.x + 2,
            rect.y,
            rect.width.saturating_sub(2),
            &format!(" {}", row.label),
            if row.enabled {
                style
            } else {
                style.add_modifier(Modifier::DIM)
            },
        );
        let (glyph, state, color) = endpoint_status_presentation(row.status, p, cx.spinner);
        let signal = if row.status == ClientEndpointStatus::Online {
            glyph.to_owned()
        } else {
            format!("{glyph} {state}")
        };
        let signal_style = if is_selected {
            style
        } else {
            Style::default().fg(color).bg(row_bg)
        };
        put_right_text(b, rect, rect.y, &signal, signal_style);
        let meta_style = if is_selected {
            style
        } else {
            Style::default().fg(p.overlay0).bg(row_bg)
        };
        let group = row.group.as_deref().unwrap_or_default();
        let meta = if group.is_empty() {
            format!("   {}", row.target)
        } else {
            format!("   {} · {}", row.target, group)
        };
        put_text(b, rect.x, rect.y + 1, rect.width, &meta, meta_style);
        if let Some(version) = row.server_version.as_deref() {
            put_right_text(b, rect, rect.y + 1, &format!("v{version}"), meta_style);
        }
    }
    if rows.is_empty() {
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
    if let Some(footer) = stack.footer {
        render_key_hints(b, footer, &hints, p, cx.components);
    }

    let mut action_hits = Vec::new();
    let add_label = t.add_button;
    let import_label = t.import_button;
    let close_label = crate::ui::modal_close_button_text();
    let buttons = modal_button_row(
        stack.actions.unwrap_or_default(),
        &[add_label, import_label, close_label],
        2,
    );
    if let [add, import, close] = buttons.as_slice() {
        modal_button(
            b,
            *add,
            add_label,
            crate::ui::ModalButtonTone::Primary,
            cx.button_state(
                &super::super::feedback::ChromeHover::MachineButton(MachineOverlayButton::Add),
                crate::ui::ModalButtonState::Focused,
            ),
            p,
        );
        modal_button(
            b,
            *import,
            import_label,
            crate::ui::ModalButtonTone::Secondary,
            cx.button_state(
                &super::super::feedback::ChromeHover::MachineButton(MachineOverlayButton::Import),
                crate::ui::ModalButtonState::Normal,
            ),
            p,
        );
        modal_button(
            b,
            *close,
            close_label,
            crate::ui::ModalButtonTone::Secondary,
            cx.button_state(
                &super::super::feedback::ChromeHover::MachineButton(MachineOverlayButton::Close),
                crate::ui::ModalButtonState::Normal,
            ),
            p,
        );
        action_hits.push((*add, MachineOverlayButton::Add));
        action_hits.push((*import, MachineOverlayButton::Import));
        action_hits.push((*close, MachineOverlayButton::Close));
    }

    Some(OverlayRender {
        area: popup,
        machines_popup: popup,
        machines_search: Rect::new(stack.header.x, stack.header.y + 1, stack.header.width, 1),
        machines_rows: row_hits,
        machines_actions: action_hits,
        machines_toast: stack.footer.unwrap_or_default(),
        cursor,
        ..OverlayRender::default()
    })
}

pub(super) fn detail_lines(
    profile: &SavedSshEndpoint,
    endpoint: Option<&ClientShellEndpoint>,
    forward_status: Option<&[crate::remote::PortForwardStatus]>,
    session_log_dropped: Option<u64>,
) -> Vec<(String, String)> {
    let t = &crate::i18n::texts().machines;
    let mut lines: Vec<(String, String)> = Vec::new();
    let yes_no = |value: Option<bool>| match value {
        Some(true) => t.choice_yes.to_owned(),
        Some(false) => t.choice_no.to_owned(),
        None => t.value_not_set.to_owned(),
    };
    let mut push = |label: &str, value: String| lines.push((label.to_owned(), value));
    push(t.detail_target, profile.target.clone());
    push(t.detail_session, profile.session.clone());
    push(t.detail_enabled, yes_no(Some(profile.enabled)));
    if let Some(endpoint) = endpoint {
        push(
            t.detail_status,
            endpoint_status_label(endpoint.status).to_owned(),
        );
        if let Some(version) = endpoint.server_version.as_deref() {
            push(t.detail_server_version, format!("v{version}"));
        }
        if let Some(detail) = endpoint.status_detail.as_deref() {
            push(t.detail_last_error, detail.to_owned());
        }
    }
    push(t.detail_id, profile.id.to_string());
    if let Some(group) = profile.group.as_deref() {
        push(t.detail_group, group.to_owned());
    }
    if !profile.tags.is_empty() {
        push(t.detail_tags, profile.tags.join(", "));
    }
    if let Some(color) = profile.color.as_deref() {
        push(t.detail_color, color.to_owned());
    }
    if let Some(port) = profile.port {
        push(t.detail_port, port.to_string());
    }
    if let Some(user) = profile.user.as_deref() {
        push(t.detail_user, user.to_owned());
    }
    if !profile.identity_file.is_empty() {
        push(t.detail_identity_files, profile.identity_file.join(", "));
    }
    if profile.identities_only.is_some() {
        push(t.detail_identities_only, yes_no(profile.identities_only));
    }
    if let Some(agent) = profile.identity_agent.as_deref() {
        push(t.detail_identity_agent, agent.to_owned());
    }
    if let Some(checking) = profile.strict_host_key_checking {
        push(t.detail_strict_host_key, checking.as_ssh_value().to_owned());
    }
    if !profile.proxy_jump.is_empty() {
        push(
            t.detail_proxy_jump,
            profile
                .proxy_jump
                .iter()
                .map(|hop| match hop {
                    ProxyJumpHop::Target(target) => target.clone(),
                    ProxyJumpHop::Profile(id) => format!("profile:{id}"),
                })
                .collect::<Vec<_>>()
                .join(", "),
        );
    }
    if profile.forward_agent.is_some() {
        push(t.detail_forward_agent, yes_no(profile.forward_agent));
    }
    if let Some(interval) = profile.server_alive_interval {
        push(t.detail_server_alive_interval, interval.to_string());
    }
    if let Some(count) = profile.server_alive_count_max {
        push(t.detail_server_alive_count_max, count.to_string());
    }
    if let Some(persist) = profile.control_persist.as_deref() {
        push(t.detail_control_persist, persist.to_owned());
    }
    if let Some(command) = profile.remote_command.as_deref() {
        push(t.detail_remote_command, command.to_owned());
    }
    // Session logging: the saved preferences plus this machine's own live
    // writer-queue drop counter when the supervisor mirror has one.
    if let Some(log) = profile.session_log.as_ref() {
        let mut value = if log.enabled {
            t.choice_yes.to_owned()
        } else {
            t.choice_no.to_owned()
        };
        if log.enabled {
            let template = log
                .path_template
                .as_deref()
                .unwrap_or(crate::client::endpoint::DEFAULT_PATH_TEMPLATE);
            let max_bytes = log
                .max_bytes
                .unwrap_or(crate::client::endpoint::DEFAULT_MAX_LOG_BYTES);
            let interval = log
                .dump_interval_secs
                .unwrap_or(crate::client::endpoint::DEFAULT_DUMP_INTERVAL_SECS);
            value = format!("{value} · {template} · {max_bytes} B · {interval} s");
            if let Some(dropped) = session_log_dropped.filter(|dropped| *dropped > 0) {
                value = format!(
                    "{value} · {}",
                    crate::i18n::fill(
                        t.session_log_dropped_fmt,
                        &[("count", &dropped.to_string())]
                    )
                );
            }
        }
        push(t.detail_session_log, value);
    }
    // Port forwards: saved rules plus their live phase when the supervisor
    // mirror has one (offline machines show rules with the waiting note).
    if !profile.port_forwards.is_empty() {
        for (index, rule) in profile.port_forwards.iter().enumerate() {
            let label = if index == 0 {
                t.detail_port_forwards.to_owned()
            } else {
                String::new()
            };
            let status = forward_status
                .and_then(|statuses| statuses.iter().find(|status| status.rule == *rule));
            let text = match status {
                Some(status) => match status.phase {
                    crate::remote::PortForwardPhase::Active => format!(
                        "{} · {}",
                        forward_rule_display(rule),
                        t.forward_status_active
                    ),
                    crate::remote::PortForwardPhase::Failed => format!(
                        "{} · {}: {}",
                        forward_rule_display(rule),
                        t.forward_status_failed,
                        status.detail.as_deref().unwrap_or_default()
                    ),
                },
                None => format!("{} · {}", forward_rule_display(rule), t.forward_waiting),
            };
            lines.push((label, text));
        }
    }
    lines
}

pub(super) fn render_machine_detail(
    b: &mut Buffer,
    overlay: &ClientMachinesOverlay,
    profile_id: &ProfileId,
    endpoints: &[ClientShellEndpoint],
    saved_profiles: &[SavedSshEndpoint],
    connection_errors: &HashMap<ClientEndpointId, crate::remote::ConnectionErrorKind>,
    port_forwards: &HashMap<ClientEndpointId, Vec<crate::remote::PortForwardStatus>>,
    session_log_dropped: &HashMap<ProfileId, u64>,
    cx: &super::super::feedback::ChromeContext<'_>,
) -> Option<OverlayRender> {
    let p = cx.palette;
    let t = &crate::i18n::texts().machines;
    let profile = saved_profiles
        .iter()
        .find(|profile| &profile.id == profile_id);
    let profile = profile?;
    let endpoint = endpoint_for(endpoints, profile_id);
    let status = endpoint.map_or(
        if profile.enabled {
            ClientEndpointStatus::Connecting
        } else {
            ClientEndpointStatus::Disabled
        },
        |endpoint| endpoint.status,
    );
    let error_kind = connection_errors.get(&ClientEndpointId::Ssh(profile_id.clone()));
    let (popup, inner) = modal_panel(b, crate::ui::ModalSize::Large.with_height(26), p.accent, cx)?;
    if inner.width < 24 || inner.height < 8 {
        return Some(OverlayRender {
            area: popup,
            machines_popup: popup,
            ..OverlayRender::default()
        });
    }
    let has_review =
        error_kind.is_some_and(super::super::machine_auth_overlay::failure_kind_has_review);
    let labels = machine_detail_labels(profile, endpoints, connection_errors);
    let mut buttons: Vec<MachineOverlayButton> = vec![
        MachineOverlayButton::Reconnect,
        MachineOverlayButton::Edit,
        MachineOverlayButton::ToggleEnabled,
        MachineOverlayButton::Forwards,
        MachineOverlayButton::BrowseFiles,
        MachineOverlayButton::Broadcast,
        MachineOverlayButton::Remove,
    ];
    if status == ClientEndpointStatus::Attention && has_review {
        buttons.push(MachineOverlayButton::ReviewIssue);
    }
    if status == ClientEndpointStatus::Attention {
        buttons.push(MachineOverlayButton::CopyFix);
    }
    buttons.push(MachineOverlayButton::Close);
    // 页脚键表先算出来：Detail 的单机动作表同样在单行里必然被尾部截断
    // （HERDR-MACH-007「同序同集」只有在页脚真的画得下时才成立）。
    let hints = machine_detail_hints(has_review);
    let stack = super::super::page::PageLayout::with_footer_rows(
        inner,
        0,
        false,
        super::super::page::action_row_count(inner.width, &labels),
        machine_footer_rows(&hints, inner.width),
    );
    let base = Style::default()
        .bg(p.panel_bg)
        .remove_modifier(Modifier::DIM);
    let tag = machine_color_tag(profile.color.as_deref(), &profile.label, p);
    put_text(b, stack.header.x, stack.header.y, 2, " ▪", base.fg(tag));
    put_text(
        b,
        stack.header.x + 2,
        stack.header.y,
        stack.header.width.saturating_sub(2),
        &format!(" {}", profile.label),
        base.fg(p.text).add_modifier(Modifier::BOLD),
    );
    let (glyph, state, color) = endpoint_status_presentation(status, p, cx.spinner);
    let header_row = Rect::new(stack.header.x, stack.header.y, stack.header.width, 1);
    put_right_text(
        b,
        header_row,
        stack.header.y,
        &format!("{glyph} {state}"),
        base.fg(color),
    );

    let body = stack.content;
    let forward_status = port_forwards.get(&ClientEndpointId::Ssh(profile_id.clone()));
    let lines = detail_lines(
        profile,
        endpoint,
        forward_status.map(Vec::as_slice),
        session_log_dropped.get(profile_id).copied(),
    );
    let label_width = lines
        .iter()
        .map(|(label, _)| usize::from(display_width(label)))
        .max()
        .unwrap_or(0)
        .min(28);
    let attention = status == ClientEndpointStatus::Attention;
    // Structured failure classification rows replace the plain fix hint when
    // the supervisor's error kind is mirrored (it always is for fresh
    // failures); otherwise the legacy two rows remain.
    let attention_rows = if attention && error_kind.is_some() {
        4
    } else if attention {
        2
    } else {
        0
    };
    let visible = usize::from(body.height).saturating_sub(attention_rows);
    let max_scroll = lines.len().saturating_sub(visible.max(1));
    let scroll = overlay.detail_scroll.min(max_scroll);
    for (offset, (label, value)) in lines.iter().skip(scroll).take(visible.max(1)).enumerate() {
        let y = body.y + offset as u16;
        put_text(
            b,
            body.x,
            y,
            (label_width + 2) as u16,
            &format!(" {label}"),
            base.fg(p.overlay0),
        );
        put_text(
            b,
            body.x + label_width as u16 + 2,
            y,
            body.width.saturating_sub(label_width as u16 + 2),
            value,
            base.fg(p.text),
        );
    }
    let mut action_hits = Vec::new();
    if attention {
        let mut y = body.y + visible.max(1) as u16;
        let mut put_attention_line = |b: &mut Buffer, text: &str, style: Style| {
            if y < body.bottom() {
                put_text(b, body.x, y, body.width, text, style);
            }
            y += 1;
        };
        if let Some(kind) = error_kind {
            let (kind_label, next_hint) =
                super::super::machine_auth_overlay::failure_kind_presentation(kind);
            put_attention_line(
                b,
                &format!(
                    " {}: {kind_label}",
                    crate::i18n::texts().machine_auth.detail_failure
                ),
                base.fg(p.red),
            );
            put_attention_line(b, next_hint, base.fg(p.yellow));
        }
        put_attention_line(b, t.fix_hint, base.fg(p.yellow));
        let command = crate::remote::saved_ssh_bootstrap_command(&profile.target, &profile.session);
        put_attention_line(
            b,
            &format!("  {command}"),
            base.fg(p.text).add_modifier(Modifier::BOLD),
        );
    }

    render_key_hints(b, stack.footer, &hints, p, cx.components);

    let rects = super::super::page::action_grid(stack.actions, &labels);
    if rects.len() == labels.len() {
        for (index, rect) in rects.iter().enumerate() {
            let button = buttons[index];
            let (tone, state) = match button {
                MachineOverlayButton::Reconnect if !profile.enabled => (
                    crate::ui::ModalButtonTone::Secondary,
                    crate::ui::ModalButtonState::Disabled,
                ),
                MachineOverlayButton::Reconnect | MachineOverlayButton::Edit => (
                    crate::ui::ModalButtonTone::Primary,
                    crate::ui::ModalButtonState::Focused,
                ),
                MachineOverlayButton::ReviewIssue => (
                    crate::ui::ModalButtonTone::Primary,
                    crate::ui::ModalButtonState::Focused,
                ),
                MachineOverlayButton::Remove => (
                    crate::ui::ModalButtonTone::Danger,
                    crate::ui::ModalButtonState::Normal,
                ),
                _ => (
                    crate::ui::ModalButtonTone::Secondary,
                    crate::ui::ModalButtonState::Normal,
                ),
            };
            let state = cx.button_state(
                &super::super::feedback::ChromeHover::MachineButton(button),
                state,
            );
            modal_button(b, *rect, labels[index], tone, state, p);
            action_hits.push((*rect, button));
        }
    }

    Some(OverlayRender {
        area: popup,
        machines_popup: popup,
        machines_actions: action_hits,
        machines_toast: stack.footer,
        ..OverlayRender::default()
    })
}

pub(super) fn render_machine_confirm_remove(
    b: &mut Buffer,
    profile_id: &ProfileId,
    saved_profiles: &[SavedSshEndpoint],
    cx: &super::super::feedback::ChromeContext<'_>,
) -> Option<OverlayRender> {
    let p = cx.palette;
    let t = &crate::i18n::texts().machines;
    let label = saved_profiles
        .iter()
        .find(|profile| &profile.id == profile_id)
        .map(|profile| profile.label.clone())
        .unwrap_or_else(|| profile_id.to_string());
    let (popup, inner) = modal_panel(b, crate::ui::ModalSize::Medium.with_height(6), p.red, cx)?;
    let stack = crate::ui::modal_stack_areas(inner, 2, 0, 1, 0);
    put_text(
        b,
        stack.header.x,
        stack.header.y,
        stack.header.width,
        &format!(
            " {}",
            crate::i18n::fill(t.remove_title_fmt, &[("label", &label)])
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
        &format!(" {}", t.remove_detail),
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
            &super::super::feedback::ChromeHover::MachineButton(
                MachineOverlayButton::ConfirmRemove,
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
            &super::super::feedback::ChromeHover::MachineButton(MachineOverlayButton::CancelRemove),
            crate::ui::ModalButtonState::Normal,
        ),
        p,
    );
    Some(OverlayRender {
        area: popup,
        machines_popup: popup,
        machines_actions: vec![
            (*confirm, MachineOverlayButton::ConfirmRemove),
            (*cancel, MachineOverlayButton::CancelRemove),
        ],
        ..OverlayRender::default()
    })
}
