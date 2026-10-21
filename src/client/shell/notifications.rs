#[cfg(test)]
use super::notification_policy::COMPLETION_RECHECK_INTERVAL;
use super::*;
use ratatui::{
    style::Color,
    widgets::{Clear, Widget},
};

pub(super) fn render_mobile_notice_banner(
    buffer: &mut Buffer,
    area: Rect,
    title: &str,
    body: Option<&str>,
    icon: &str,
    icon_color: Color,
    offset_for_warning: bool,
    palette: &Palette,
) -> Rect {
    if area.is_empty() {
        return Rect::default();
    }
    let warning_offset = u16::from(offset_for_warning);
    let y = area.y
        + area
            .height
            .saturating_sub(1u16.saturating_add(warning_offset));
    let rect = Rect::new(area.x, y, area.width, 1);
    let background = palette.surface0;
    Clear.render(rect, buffer);
    buffer.set_style(rect, Style::default().bg(background));
    let mut x = rect.x;
    for (text, style) in [
        (" ", Style::default().bg(background)),
        (icon, Style::default().fg(icon_color).bg(background)),
        (" ", Style::default().bg(background)),
        (
            title,
            Style::default()
                .fg(palette.text)
                .bg(background)
                .add_modifier(Modifier::BOLD),
        ),
    ] {
        x = super::render::put_segment(buffer, x, rect.y, rect.right(), text, style);
    }
    if let Some(body) = body.filter(|body| !body.is_empty()) {
        x = super::render::put_segment(
            buffer,
            x,
            rect.y,
            rect.right(),
            " · ",
            Style::default().fg(palette.overlay0).bg(background),
        );
        super::render::put_text(
            buffer,
            x,
            rect.y,
            rect.right().saturating_sub(x),
            body,
            crate::ui::design::muted(palette).bg(background),
        );
    }
    rect
}

/// 通知横幅上的 agent 标签（NOTIF-01）：优先 pane 的显示名，`display_agent`
/// → `name` → `agent` → `title` 依次回退；解析不出时退到通知自带的 canonical
/// agent（`event.agent`）。
pub(super) fn notification_agent_label<'a>(
    endpoints: &'a [ClientShellEndpoint],
    notification: &'a ClientVisibleNotification,
) -> Option<&'a str> {
    let event = &notification.event;
    let from_pane = event.pane_id.as_deref().and_then(|pane_id| {
        endpoints
            .iter()
            .find(|endpoint| endpoint.endpoint_id == notification.endpoint_id)?
            .snapshot
            .as_deref()?
            .agents
            .iter()
            .find(|agent| agent.pane_id == pane_id)
            .and_then(|agent| {
                agent
                    .display_agent
                    .as_deref()
                    .or(agent.name.as_deref())
                    .or(agent.agent.as_deref())
                    .or(agent.title.as_deref())
            })
    });
    from_pane.or(event.agent.as_deref())
}

#[cfg(test)]
pub(super) fn render_mobile_notification_banner(
    buffer: &mut Buffer,
    area: Rect,
    notification: &ClientVisibleNotification,
    agent_label: Option<&str>,
    offset_for_warning: bool,
    palette: &Palette,
    components: &crate::app::state::ComponentStyles,
) -> Rect {
    let event = &notification.event;
    // `agent_label` 是调用方按 pane 解析出的显示名（`display_agent → name →
    // agent`）；通知自带的 `event.agent` 是 canonical id，只在解析不出时兜底
    // （NOTIF-01）。
    let title = match event.kind {
        SemanticNotificationKind::NeedsAttention => agent_label
            .map(|agent| {
                crate::i18n::fill(
                    crate::i18n::texts().notify.agent_waiting_fmt,
                    &[("agent", agent)],
                )
            })
            .unwrap_or_else(|| event.title.clone()),
        SemanticNotificationKind::Finished => agent_label
            .map(|agent| {
                crate::i18n::fill(
                    crate::i18n::texts().notify.agent_done_fmt,
                    &[("agent", agent)],
                )
            })
            .unwrap_or_else(|| event.title.clone()),
        SemanticNotificationKind::UpdateInstalled => {
            crate::i18n::texts().overlays.update_ready.to_owned()
        }
        SemanticNotificationKind::Custom => event.title.clone(),
    };
    let level = super::feedback::ClientToastLevel::from_notification_kind(event.kind);
    render_mobile_notice_banner(
        buffer,
        area,
        &title,
        event.body.as_deref(),
        level.icon(),
        level.color(components),
        offset_for_warning,
        palette,
    )
}

pub(super) struct NotificationContext<'a> {
    pub(super) endpoints: &'a [ClientShellEndpoint],
    pub(super) keybinds: &'a LiveKeybindConfig,
    pub(super) status_indicators: crate::config::StatusIndicatorStyle,
    pub(super) history: &'a VecDeque<super::feedback::ClientNotificationRecord>,
    pub(super) focused: bool,
    pub(super) hovered: bool,
}

/// 首行的「agent 名 + 标题」：标题以 agent 名（显示名或 canonical id）开头时拆出来，
/// agent 名单独加粗；拆完为空时按通知种类补「等待中 / 已完成」。
fn split_agent_title(
    label: Option<&str>,
    canonical: Option<&str>,
    title: &str,
    kind: SemanticNotificationKind,
) -> (Option<String>, String) {
    let Some(label) = label else {
        return (None, title.to_owned());
    };
    let rest = [Some(label), canonical]
        .into_iter()
        .flatten()
        .find_map(|prefix| {
            let rest = title.strip_prefix(prefix)?;
            (rest.is_empty() || rest.starts_with(char::is_whitespace)).then(|| rest.trim_start())
        });
    let fallback = |fmt: &str| crate::i18n::fill(fmt, &[("agent", "")]).trim().to_owned();
    let title = match rest {
        Some("") => match kind {
            SemanticNotificationKind::NeedsAttention => {
                fallback(crate::i18n::texts().notify.agent_waiting_fmt)
            }
            SemanticNotificationKind::Finished => {
                fallback(crate::i18n::texts().notify.agent_done_fmt)
            }
            SemanticNotificationKind::UpdateInstalled | SemanticNotificationKind::Custom => {
                String::new()
            }
        },
        Some(rest) => rest.to_owned(),
        None => title.to_owned(),
    };
    (Some(label.to_owned()), title)
}

/// 语义通知的吐司视图（合成阶段组装，绘制只读）。
pub(super) fn shell_notification_toast(
    notification: &ClientVisibleNotification,
    presentation: &NotificationContext<'_>,
    now: std::time::Instant,
) -> super::chrome::toast::Toast<'static> {
    use super::chrome::command::text;
    use super::chrome::toast::{Action, ActionKind, Toast};
    use crate::harness::AgentBucket;
    let event = &notification.event;
    let bucket = super::chrome::notification::bucket(
        event,
        presentation.endpoints,
        &notification.endpoint_id,
    );
    let level = super::feedback::ClientToastLevel::for_notification(bucket, event.kind);
    let icon = if matches!(bucket, AgentBucket::NeedsInput | AgentBucket::Failed) {
        crate::ui::design::status_glyph(bucket, presentation.status_indicators, 0, false)
    } else {
        level.icon()
    };
    let (agent, title) = split_agent_title(
        notification_agent_label(presentation.endpoints, notification),
        event.agent.as_deref(),
        &event.title,
        event.kind,
    );
    let endpoint = presentation
        .endpoints
        .iter()
        .find(|e| e.endpoint_id == notification.endpoint_id);
    let location = endpoint
        .map(|e| {
            let workspace = e.snapshot.as_deref().and_then(|s| {
                s.workspaces
                    .iter()
                    .find(|w| Some(w.workspace_id.as_str()) == event.workspace_id.as_deref())
            });
            workspace
                .map(|w| {
                    format!(
                        "{} / {}{}",
                        e.label,
                        w.label,
                        w.branch
                            .as_ref()
                            .map(|branch| format!(" / {branch}"))
                            .unwrap_or_default()
                    )
                })
                .unwrap_or_else(|| e.label.clone())
        })
        .unwrap_or_else(|| notification.endpoint_id.storage_key());
    let at = presentation
        .history
        .iter()
        .rev()
        .find(|r| {
            r.title == event.title
                && r.target.as_ref().is_some_and(|t| {
                    t.endpoint_id == notification.endpoint_id && t.pane_id == event.pane_id
                })
        })
        .map(|r| r.received_at)
        .unwrap_or(now);
    let has_target = event.pane_id.is_some();
    let hint = |key: &str, label: &str| Action {
        key: key.to_owned(),
        label: label.to_owned(),
        kind: ActionKind::Hint,
    };
    let mut actions = Vec::new();
    if presentation.focused {
        // 卡片持有键盘焦点：enter / esc 归卡片。
        if has_target {
            actions.push(hint(
                "enter",
                if event.kind == SemanticNotificationKind::NeedsAttention {
                    text("jump & answer", "跳转并回答")
                } else {
                    text("jump", "跳转")
                },
            ));
        }
        actions.push(hint("esc", text("dismiss", "忽略")));
    } else {
        if let Some(jump) = has_target
            .then(|| {
                super::chrome::key_sequence(
                    presentation.keybinds,
                    &presentation.keybinds.keybinds.open_notification_target,
                )
            })
            .flatten()
        {
            actions.push(hint(&jump, text("jump", "跳转")));
        }
        // 普通输入归 pane：点「按键」把键盘焦点交给卡片，之后 enter / esc 才生效。
        actions.push(Action {
            key: text("keys", "按键").to_owned(),
            label: text("use enter/esc", "启用 enter/esc").to_owned(),
            kind: ActionKind::Focus,
        });
    }
    Toast {
        level,
        icon: icon.into(),
        agent: agent.map(Into::into),
        title: title.into(),
        age: Some(super::chrome::toast::age_label(now.saturating_duration_since(at)).into()),
        location: Some(location.into()),
        body: event.body.clone().unwrap_or_default().into(),
        actions,
        emphasized: presentation.focused || presentation.hovered,
        fit: false,
    }
}

impl ClientShellState {
    pub(super) fn route_notification_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) -> bool {
        if !self.notification_focused
            || self.overlay.is_some()
            || self.visible_notification.is_none()
        {
            return false;
        }
        if key.modifiers.is_empty() {
            match key.code {
                crossterm::event::KeyCode::Enter => {
                    self.notification_focused = false;
                    self.focus_visible_notification(outcome);
                    return true;
                }
                crossterm::event::KeyCode::Esc => {
                    self.dismiss_visible_notification();
                    outcome.repaint = true;
                    return true;
                }
                _ => {}
            }
        }
        self.notification_focused = false;
        outcome.repaint = true;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notification() -> ClientVisibleNotification {
        ClientVisibleNotification {
            endpoint_id: ClientEndpointId::Local,
            event: SemanticNotification {
                kind: SemanticNotificationKind::Custom,
                title: "notice".into(),
                body: None,
                sound: None,
                agent: None,
                workspace_id: None,
                tab_id: None,
                pane_id: None,
                position: None,
            },
            deadline: std::time::Instant::now(),
        }
    }

    #[test]
    fn mobile_notification_is_a_bottom_banner_with_released_title() {
        let palette = crate::app::client_palette_from_config(&Config::default());
        let components = crate::app::state::ComponentStyles::from_palette(&palette);
        let mut notification = notification();
        notification.event.kind = SemanticNotificationKind::NeedsAttention;
        notification.event.agent = Some("pi".into());
        notification.event.body = Some("workspace · tab 1".into());
        let area = Rect::new(0, 0, 44, 20);
        let mut buffer = Buffer::empty(area);
        for cell in &mut buffer.content {
            cell.set_symbol("X");
        }
        let rect = render_mobile_notification_banner(
            &mut buffer,
            area,
            &notification,
            Some("pi"),
            true,
            &palette,
            &components,
        );
        assert_eq!(rect, Rect::new(0, 18, 44, 1));
        let text = buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        let compact_title = text
            .chars()
            .filter(|ch| !ch.is_whitespace())
            .collect::<String>();
        assert!(compact_title.contains("pi等待中"));
        assert!(text.contains("workspace · tab 1"));
        assert!(buffer.content[18 * 44..19 * 44]
            .iter()
            .all(|cell| cell.symbol() != "X"));
    }

    /// NOTIF-01：横幅用 pane 的显示名（如「Claude Code」），不是 canonical id。
    #[test]
    fn mobile_notification_banner_prefers_the_pane_display_agent() {
        let palette = crate::app::client_palette_from_config(&Config::default());
        let components = crate::app::state::ComponentStyles::from_palette(&palette);
        let mut notification = notification();
        notification.event.kind = SemanticNotificationKind::Finished;
        notification.event.agent = Some("claude".into());
        let area = Rect::new(0, 0, 44, 20);
        let mut buffer = Buffer::empty(area);
        render_mobile_notification_banner(
            &mut buffer,
            area,
            &notification,
            Some("Claude Code"),
            true,
            &palette,
            &components,
        );
        let text = buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("Claude Code"), "横幅用显示名: {text}");
        assert!(!text.contains("claude "), "不再显示 canonical id: {text}");
    }

    /// NOTIF-01 的解析链：pane 的显示名优先，其次 name / agent / title，最后
    /// 才退到通知自带的 canonical agent；pane 不在快照里时同样退回 canonical。
    #[test]
    fn notification_agent_label_prefers_the_pane_display_chain() {
        let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
        let mut snapshot = super::super::tests::snapshot();
        snapshot.agents.push(crate::protocol::ClientShellAgent {
            insight: Default::default(),
            pane_id: "pane_1".into(),
            workspace_id: "ws_1".into(),
            tab_id: "tab_1".into(),
            name: Some("Friendly Name".into()),
            display_agent: Some("Claude Code".into()),
            agent: Some("claude".into()),
            title: None,
            terminal_title: None,
            terminal_title_stripped: None,
            agent_status: crate::api::schema::AgentStatus::Idle,
            state_change_seq: 1,
            state_labels: Vec::new(),
            tokens: Vec::new(),
            focused: true,
            launch_seq: 0,
            activity: Default::default(),
        });
        state.set_snapshot(Box::new(snapshot));
        let mut notification = notification();
        notification.event.pane_id = Some("pane_1".into());
        notification.event.agent = Some("claude".into());
        assert_eq!(
            notification_agent_label(&state.endpoints, &notification),
            Some("Claude Code"),
            "显示名优先于 canonical id"
        );

        // pane 不在快照里：退回通知自带的 canonical agent。
        notification.event.pane_id = Some("pane-missing".into());
        assert_eq!(
            notification_agent_label(&state.endpoints, &notification),
            Some("claude")
        );
        notification.event.pane_id = None;
        assert_eq!(
            notification_agent_label(&state.endpoints, &notification),
            Some("claude")
        );
    }

    #[test]
    fn unavailable_notification_target_stays_visible_and_reports_the_machine() {
        let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
        state.visible_notification = Some(ClientVisibleNotification {
            endpoint_id: ClientEndpointId::Local,
            event: SemanticNotification {
                kind: SemanticNotificationKind::NeedsAttention,
                title: "agent needs attention".into(),
                body: None,
                sound: None,
                agent: Some("agent".into()),
                workspace_id: Some("workspace".into()),
                tab_id: Some("tab".into()),
                pane_id: Some("pane".into()),
                position: None,
            },
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(5),
        });
        let mut outcome = ClientShellInput::default();

        state.focus_visible_notification(&mut outcome);

        assert!(state.visible_notification.is_some());
        assert!(state
            .visible_endpoint_notice
            .as_ref()
            .is_some_and(|notice| notice.body.contains("Local is unavailable")));
        assert!(outcome.repaint);
    }

    #[test]
    fn immediate_finished_evidence_for_acknowledged_idle_never_toasts_or_sounds() {
        let mut config = ClientShellConfig::from_config(&Config::default());
        config.toast_delivery = crate::config::ToastDelivery::Herdr;
        config.toast_delay_seconds = 0;
        let mut state = ClientShellState::new(config);
        let mut snapshot = super::super::tests::snapshot();
        snapshot.agents.push(crate::protocol::ClientShellAgent {
            insight: Default::default(),
            pane_id: "pane_1".into(),
            workspace_id: "ws_1".into(),
            tab_id: "tab_1".into(),
            name: Some("agent".into()),
            display_agent: None,
            agent: Some("agent".into()),
            title: None,
            terminal_title: None,
            terminal_title_stripped: None,
            agent_status: crate::api::schema::AgentStatus::Idle,
            state_change_seq: 2,
            state_labels: Vec::new(),
            tokens: Vec::new(),
            focused: true,
            launch_seq: 0,
            activity: Default::default(),
        });
        state.set_snapshot(Box::new(snapshot));

        let (effects, repaint) = state.receive_notification(
            &ClientEndpointId::Local,
            SemanticNotification {
                kind: SemanticNotificationKind::Finished,
                title: "agent finished".into(),
                body: None,
                sound: Some(SemanticNotificationSound::Done),
                agent: Some("agent".into()),
                workspace_id: Some("ws_1".into()),
                tab_id: Some("tab_1".into()),
                pane_id: Some("pane_1".into()),
                position: None,
            },
            std::time::Instant::now(),
        );

        assert!(effects.is_empty());
        assert!(!repaint);
        assert!(state.visible_notification.is_none());
    }

    #[test]
    fn finished_hint_waits_for_the_projected_completion_snapshot() {
        let mut config = ClientShellConfig::from_config(&Config::default());
        config.toast_delivery = crate::config::ToastDelivery::Terminal;
        config.toast_delay_seconds = 0;
        let mut state = ClientShellState::new(config);
        let mut snapshot = super::super::tests::snapshot();
        snapshot.agents.push(crate::protocol::ClientShellAgent {
            insight: Default::default(),
            pane_id: "pane_1".into(),
            workspace_id: "ws_1".into(),
            tab_id: "tab_1".into(),
            name: Some("agent".into()),
            display_agent: None,
            agent: Some("agent".into()),
            title: None,
            terminal_title: None,
            terminal_title_stripped: None,
            agent_status: crate::api::schema::AgentStatus::Working,
            state_change_seq: 1,
            state_labels: Vec::new(),
            tokens: Vec::new(),
            focused: false,
            launch_seq: 0,
            activity: Default::default(),
        });
        state.set_snapshot(Box::new(snapshot.clone()));
        let now = std::time::Instant::now();
        let event = SemanticNotification {
            kind: SemanticNotificationKind::Finished,
            title: "agent finished".into(),
            body: None,
            sound: None,
            agent: Some("agent".into()),
            workspace_id: Some("ws_1".into()),
            tab_id: Some("background-tab".into()),
            pane_id: Some("pane_1".into()),
            position: None,
        };

        let (effects, _) = state.receive_notification(&ClientEndpointId::Local, event, now);
        assert!(effects.is_empty());
        assert_eq!(state.pending_notifications.len(), 1);

        snapshot.revision = snapshot.revision.saturating_add(1);
        snapshot.agents[0].agent_status = crate::api::schema::AgentStatus::Idle;
        snapshot.agents[0].state_change_seq = 2;
        state.set_snapshot(Box::new(snapshot));
        let (effects, _) = state.tick_notifications(now + COMPLETION_RECHECK_INTERVAL);
        assert!(matches!(
            effects.as_slice(),
            [ClientShellNotificationEffect::Terminal { title, .. }] if title == "agent finished"
        ));
        assert!(state.pending_notifications.is_empty());
    }

    #[test]
    fn retiring_one_endpoint_preserves_other_endpoint_notifications() {
        let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
        let mut local = notification();
        local.event.title = "local".into();
        let remote_id = ClientEndpointId::Ssh(
            crate::client::endpoint::ProfileId::parse("0123456789abcdef0123456789abcdef").unwrap(),
        );
        let mut remote = notification();
        remote.endpoint_id = remote_id.clone();
        remote.event.title = "remote".into();
        state.visible_notification = Some(local);
        state.queued_notifications.push_back(remote);

        state.retire_endpoint_notifications(&ClientEndpointId::Local);

        assert_eq!(
            state
                .visible_notification
                .as_ref()
                .map(|notification| (&notification.endpoint_id, notification.event.title.as_str())),
            Some((&remote_id, "remote"))
        );
    }

    #[test]
    fn concurrent_endpoint_notifications_are_queued_in_arrival_order() {
        let mut config = Config::default();
        config.ui.toast.delivery = crate::config::ToastDelivery::Herdr;
        let mut state = ClientShellState::new(ClientShellConfig::from_config(&config));
        let now = std::time::Instant::now();
        for title in ["first", "second"] {
            state.receive_notification(
                &ClientEndpointId::Local,
                SemanticNotification {
                    kind: SemanticNotificationKind::Custom,
                    title: title.into(),
                    body: None,
                    sound: None,
                    agent: None,
                    workspace_id: None,
                    tab_id: None,
                    pane_id: Some(title.into()),
                    position: None,
                },
                now,
            );
        }

        assert_eq!(
            state
                .visible_notification
                .as_ref()
                .map(|notification| notification.event.title.as_str()),
            Some("first")
        );
        assert_eq!(state.queued_notifications.len(), 1);

        state.tick_notifications(now + std::time::Duration::from_secs(5));

        assert_eq!(
            state
                .visible_notification
                .as_ref()
                .map(|notification| notification.event.title.as_str()),
            Some("second")
        );
        assert!(state.queued_notifications.is_empty());
    }

    #[test]
    fn notification_rect_stays_inside_short_nonzero_area() {
        use super::super::chrome::toast::{measure, place, Anchor};
        let config = ClientShellConfig::from_config(&Config::default());
        let history = VecDeque::new();
        let presentation = NotificationContext {
            endpoints: &[],
            keybinds: &config.keybinds,
            status_indicators: config.status_indicators,
            history: &history,
            focused: false,
            hovered: false,
        };
        let now = std::time::Instant::now();
        let toast = shell_notification_toast(&notification(), &presentation, now);
        for height in [1, 2] {
            let area = Rect::new(3, 4, 8, height);
            for anchor in [Anchor::TopRight, Anchor::BottomRight] {
                let measured = measure(&toast, area);
                let rect = place(area, anchor, measured.width, measured.height, 1);
                assert!(rect.y >= area.y);
                assert!(rect.bottom() <= area.bottom());
                assert!(rect.right() <= area.right());
            }
        }
    }

    #[test]
    fn notification_title_splits_the_agent_name_and_maps_levels() {
        assert_eq!(
            split_agent_title(
                Some("Claude Code"),
                Some("claude"),
                "claude needs attention",
                SemanticNotificationKind::NeedsAttention,
            ),
            (Some("Claude Code".into()), "needs attention".into())
        );
        assert_eq!(
            split_agent_title(
                Some("codex"),
                Some("codex"),
                "codex",
                SemanticNotificationKind::Finished
            )
            .1,
            crate::i18n::fill(crate::i18n::texts().notify.agent_done_fmt, &[("agent", "")]).trim()
        );
        assert_eq!(
            split_agent_title(None, None, "notice", SemanticNotificationKind::Custom),
            (None, "notice".into())
        );
        assert_eq!(
            split_agent_title(
                Some("codex"),
                None,
                "codexbot ran",
                SemanticNotificationKind::Custom
            ),
            (Some("codex".into()), "codexbot ran".into())
        );
        use super::super::feedback::ClientToastLevel as L;
        use crate::harness::AgentBucket as B;
        for (bucket, kind, level) in [
            (B::NeedsInput, SemanticNotificationKind::Custom, L::Warning),
            (B::Failed, SemanticNotificationKind::Custom, L::Error),
            (B::Review, SemanticNotificationKind::Finished, L::Success),
            (
                B::None,
                SemanticNotificationKind::NeedsAttention,
                L::Warning,
            ),
            (B::None, SemanticNotificationKind::UpdateInstalled, L::Info),
            (B::None, SemanticNotificationKind::Custom, L::Info),
        ] {
            assert_eq!(
                L::for_notification(bucket, kind),
                level,
                "{bucket:?} {kind:?}"
            );
        }
    }

    #[test]
    fn failed_custom_notification_is_not_a_permission_request() {
        let mut event = notification().event;
        event.agent = Some("codex".into());
        event.pane_id = Some("pane_1".into());
        event.sound = Some(SemanticNotificationSound::Request);
        event.title = "codex failed".into();
        assert_eq!(
            super::super::chrome::notification::bucket(&event, &[], &ClientEndpointId::Local),
            crate::harness::AgentBucket::Failed
        );
        event.kind = SemanticNotificationKind::NeedsAttention;
        assert_eq!(
            super::super::chrome::notification::bucket(&event, &[], &ClientEndpointId::Local),
            crate::harness::AgentBucket::NeedsInput
        );
    }

    #[test]
    fn new_boot_retires_visible_pending_and_queued_notifications() {
        let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
        let snapshot = super::super::tests::snapshot();
        state.set_snapshot(Box::new(snapshot.clone()));
        let now = std::time::Instant::now();
        for title in ["first", "second"] {
            let mut event = notification().event;
            event.title = title.into();
            state.receive_notification(&ClientEndpointId::Local, event, now);
        }
        let mut changed = snapshot;
        changed.boot_id = "replacement-boot".into();
        state.set_snapshot(Box::new(changed));
        assert!(state.visible_notification.is_none());
        assert!(state.queued_notifications.is_empty());
        assert!(state.pending_notifications.is_empty());
    }

    #[test]
    fn delayed_permission_card_rechecks_state_and_external_delivery_keeps_the_body() {
        for delivery in [
            crate::config::ToastDelivery::Herdr,
            crate::config::ToastDelivery::Terminal,
            crate::config::ToastDelivery::System,
        ] {
            let mut config = ClientShellConfig::from_config(&Config::default());
            config.toast_delivery = delivery;
            config.toast_delay_seconds = 1;
            let mut state = ClientShellState::new(config);
            let mut projection = super::super::tests::snapshot();
            projection.agents.push(crate::protocol::ClientShellAgent {
                pane_id: "pane_1".into(),
                workspace_id: "ws_1".into(),
                tab_id: "tab_1".into(),
                name: Some("codex".into()),
                display_agent: None,
                agent: Some("codex".into()),
                title: None,
                terminal_title: None,
                terminal_title_stripped: None,
                agent_status: crate::api::schema::AgentStatus::Blocked,
                state_change_seq: 1,
                state_labels: Vec::new(),
                tokens: Vec::new(),
                focused: false,
                launch_seq: 0,
                activity: Default::default(),
                insight: Default::default(),
            });
            state.set_snapshot(Box::new(projection.clone()));
            let mut event = notification().event;
            event.kind = SemanticNotificationKind::NeedsAttention;
            event.agent = Some("codex".into());
            event.pane_id = Some("pane_1".into());
            event.tab_id = Some("other-tab".into());
            event.body = Some("完整权限问题".into());
            let now = std::time::Instant::now();
            state.receive_notification(&ClientEndpointId::Local, event.clone(), now);
            projection.agents[0].agent_status = crate::api::schema::AgentStatus::Working;
            projection.revision += 1;
            state.set_snapshot(Box::new(projection.clone()));
            let (effects, _) = state.tick_notifications(now + std::time::Duration::from_secs(2));
            assert!(effects.is_empty());
            assert!(state.visible_notification.is_none());
            projection.agents[0].agent_status = crate::api::schema::AgentStatus::Blocked;
            projection.revision += 1;
            state.set_snapshot(Box::new(projection));
            state.receive_notification(&ClientEndpointId::Local, event, now);
            let (effects, _) = state.tick_notifications(now + std::time::Duration::from_secs(2));
            match delivery {
                crate::config::ToastDelivery::Herdr => assert_eq!(state.visible_notification.as_ref().and_then(|n| n.event.body.as_deref()), Some("完整权限问题")),
                _ => assert!(effects.iter().any(|effect| matches!(effect, ClientShellNotificationEffect::Terminal { body, .. } | ClientShellNotificationEffect::System { body, .. } if body.as_deref() == Some("完整权限问题")))),
            }
        }
    }

    #[test]
    fn notification_jump_activates_the_real_remote_pane() {
        let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
        let profile =
            SavedSshEndpoint::new("Remote", "dev@example.com", "default").expect("profile");
        let endpoint = ClientEndpointId::Ssh(profile.id.clone());
        state.set_endpoint_catalog(&[profile]);
        state.set_endpoint_status(&endpoint, ClientEndpointStatus::Online);
        state.set_endpoint_snapshot(&endpoint, Box::new(super::super::tests::snapshot()));
        let mut visible = notification();
        visible.endpoint_id = endpoint.clone();
        visible.event.pane_id = Some("pane_1".into());
        state.visible_notification = Some(visible);
        let mut outcome = ClientShellInput::default();
        state.focus_visible_notification(&mut outcome);
        assert!(
            matches!(outcome.actions.as_slice(), [ClientShellAction::ActivateEndpoint { endpoint_id, target: Some(ClientEndpointFocusTarget::Pane(pane)) }] if endpoint_id == &endpoint && pane == "pane_1")
        );
    }
}
