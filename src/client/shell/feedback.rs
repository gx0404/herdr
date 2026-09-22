//! Interactive feedback layer for client chrome: hover highlighting, the
//! configurable spinner, the visual bell, overlay/toast entrance fades, and
//! the toast severity model with its notification history. Everything here
//! is client-owned presentation state; the server/runtime contract is
//! untouched and the per-pane render loops never consult this module.

use super::*;

/// Cap on the in-memory notification history ring.
pub(super) const NOTIFICATION_HISTORY_LIMIT: usize = 50;
/// How long the visual bell marker and border emphasis stay on screen.
const VISUAL_BELL_DURATION: std::time::Duration = std::time::Duration::from_millis(1_500);
/// Length of the overlay/toast entrance fade: one dim frame, then settled.
pub(super) const ENTRANCE_DURATION: std::time::Duration = std::time::Duration::from_millis(120);

/// Spinner presentation resolved from `ui.spinner`: frame glyphs and the
/// frame interval. Renderers read the current frame off state; they never
/// consult config in the render loop.
#[derive(Debug, Clone)]
pub(super) struct ClientSpinnerStyle {
    /// Master switch behind `ui.spinner`: off renders a static glyph and
    /// stops the animation clock entirely.
    pub(super) enabled: bool,
    pub(super) frames: Vec<&'static str>,
    pub(super) interval: std::time::Duration,
}

impl Default for ClientSpinnerStyle {
    fn default() -> Self {
        Self {
            enabled: true,
            frames: vec!["◐", "◓", "◑", "◒"],
            interval: std::time::Duration::from_millis(100),
        }
    }
}

impl ClientSpinnerStyle {
    fn frame(&self, tick: u64) -> &'static str {
        let len = self.frames.len();
        if len == 0 {
            return "◐";
        }
        self.frames[usize::try_from(tick).unwrap_or(usize::MAX) % len]
    }
}

/// Presentation feedback switches behind `ui.hover_effects`, `ui.spinner`,
/// `ui.animations`, and `ui.visual_bell`, resolved once from the config
/// model so every consumer reads this one seam in the render loop.
#[derive(Debug, Clone)]
pub(super) struct ClientFeedbackToggles {
    pub(super) hover_effects: bool,
    pub(super) spinner: ClientSpinnerStyle,
    pub(super) animations: bool,
    pub(super) visual_bell: bool,
}

impl ClientFeedbackToggles {
    pub(super) fn from_config(config: &Config) -> Self {
        Self {
            hover_effects: config.ui.hover_effects,
            spinner: ClientSpinnerStyle {
                enabled: config.ui.spinner,
                ..ClientSpinnerStyle::default()
            },
            animations: config.ui.animations,
            visual_bell: config.ui.visual_bell,
        }
    }
}

impl Default for ClientFeedbackToggles {
    fn default() -> Self {
        Self {
            hover_effects: true,
            spinner: ClientSpinnerStyle::default(),
            animations: true,
            visual_bell: true,
        }
    }
}

/// Severity of a toast or history entry. Drives the leading icon and the
/// `toast_border_*` component token used for the border color.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ClientToastLevel {
    Info,
    Success,
    Error,
}

impl ClientToastLevel {
    pub(super) fn icon(self) -> &'static str {
        match self {
            Self::Info => "●",
            Self::Success => "✓",
            Self::Error => "✕",
        }
    }

    pub(super) fn color(
        self,
        components: &crate::app::state::ComponentStyles,
    ) -> ratatui::style::Color {
        match self {
            Self::Info => components.toast_border_info,
            Self::Success => components.toast_border_success,
            Self::Error => components.toast_border_error,
        }
    }

    pub(super) fn from_notification_kind(kind: SemanticNotificationKind) -> Self {
        match kind {
            SemanticNotificationKind::NeedsAttention => Self::Error,
            SemanticNotificationKind::Finished => Self::Success,
            SemanticNotificationKind::UpdateInstalled | SemanticNotificationKind::Custom => {
                Self::Info
            }
        }
    }

    pub(super) fn from_notice_kind(kind: ClientEndpointNoticeKind) -> Self {
        match kind {
            ClientEndpointNoticeKind::Unsupported | ClientEndpointNoticeKind::Rejected => {
                Self::Error
            }
            ClientEndpointNoticeKind::Timeout | ClientEndpointNoticeKind::Unavailable => Self::Info,
            ClientEndpointNoticeKind::Success => Self::Success,
        }
    }
}

/// Client-chrome hover identity, resolved from the previous frame's hit map
/// on mouse-move events and compared by renderers. Identity-based (not
/// rect-based) so it survives relayout between frames.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ChromeHover {
    WorkspaceRow {
        endpoint_id: ClientEndpointId,
        workspace_id: String,
    },
    MachineRow(ClientEndpointId),
    AgentRow(String),
    AgentGroupRow(String),
    EndpointAgentRow(ClientEndpointId, String),
    Tab(String),
    NewTab,
    TabScrollLeft,
    TabScrollRight,
    GlobalLauncher,
    SidebarToggle,
    AgentSortToggle,
    WorkspaceScrollbarThumb,
    AgentScrollbarThumb,
    HelpScrollbarThumb,
    ReleaseNotesScrollbarThumb,
    ProductAnnouncementScrollbarThumb,
    SidebarDivider,
    SidebarSectionDivider,
    NotificationToast,
    OverlayPrimary,
    OverlayClear,
    OverlayCancel,
    MachineButton(super::machines_overlay::MachineOverlayButton),
    MachineAuthButton(super::machine_auth_overlay::MachineAuthButton),
    SnippetButton(super::snippets_overlay::SnippetOverlayButton),
    SceneButton(super::scenes_overlay::SceneOverlayButton),
    BroadcastButton(super::broadcast::BroadcastButton),
    MachineFilesButton(super::machine_files_overlay::MachineFilesButton),
    LifecycleBannerRetry,
    LifecycleBannerGiveUp,
    /// 统一树节点的折叠开关：`(端点, 折叠键)`。
    AgentTreeToggle(ClientEndpointId, String),
    /// agent 行下的活动节点行：`(端点, owner_key, node_id)`；`owner_key` 是
    /// `pane:<id>` / `ext:<id>`，用字符串避免枚举嵌套。
    AgentActivityRow(ClientEndpointId, String, String),
    /// 「外部」来源分组头：`(端点, source)`。
    ExternalAgentGroup(ClientEndpointId, String),
    /// 外部来源条目行：`(端点, external_id)`。
    ExternalAgentRow(ClientEndpointId, String),
    /// 「Agent 活动」窗口左列的树行（节点 id）。
    AgentActivityNode(String),
    AgentActivityButton(super::agent_activity_overlay::AgentActivityButton),
}

/// Resolved render context for client chrome: palette, component tokens,
/// border glyphs, hover identity, current spinner frame, and the compose
/// clock (for relative timestamps). Bundled so overlay renderers stop
/// growing parallel parameter lists.
#[derive(Clone, Copy)]
pub(super) struct ChromeContext<'a> {
    pub(super) page_bounds: Option<Rect>,
    pub(super) palette: &'a Palette,
    pub(super) components: &'a crate::app::state::ComponentStyles,
    pub(super) glyphs: crate::ui::BorderGlyphs,
    pub(super) hover: Option<&'a ChromeHover>,
    pub(super) spinner: &'a str,
    pub(super) now: std::time::Instant,
}

impl<'a> ChromeContext<'a> {
    pub(super) fn hovered(&self, target: &ChromeHover) -> bool {
        self.hover == Some(target)
    }

    /// Modal button state with hover: the pointer resting on a button wins
    /// over the keyboard-driven base state.
    pub(super) fn button_state(
        &self,
        target: &ChromeHover,
        base: crate::ui::ModalButtonState,
    ) -> crate::ui::ModalButtonState {
        if self.hovered(target) {
            crate::ui::ModalButtonState::Hovered
        } else {
            base
        }
    }

    /// Scrollbar thumb color: a brightness lift while the pointer rests on
    /// the thumb, the usual color otherwise.
    pub(super) fn thumb_color(
        &self,
        target: &ChromeHover,
        base: ratatui::style::Color,
    ) -> ratatui::style::Color {
        if self.hovered(target) {
            self.palette.subtext0
        } else {
            base
        }
    }
}

/// Jump target of a history entry; mirrors the ids a semantic notification
/// carries so Enter can focus the originating pane across endpoints.
#[derive(Clone, Debug)]
pub(super) struct ClientNotificationTarget {
    pub(super) endpoint_id: ClientEndpointId,
    pub(super) pane_id: Option<String>,
}

/// One entry of the notification history ring.
#[derive(Clone, Debug)]
pub(super) struct ClientNotificationRecord {
    pub(super) level: ClientToastLevel,
    pub(super) title: String,
    pub(super) body: Option<String>,
    pub(super) received_at: std::time::Instant,
    pub(super) target: Option<ClientNotificationTarget>,
}

/// Modal overlay listing the recent notification history.
#[derive(Debug)]
pub(super) struct ClientNotificationHistoryOverlay {
    pub(super) selected: usize,
    /// 指针悬浮行：只由 `Moved` 改写。`selected` 只由键盘、点击与滚轮改写——
    /// Enter 会跳到该条通知的源 pane（可能跨端点），指针划过列表就把它改掉
    /// 是 MENU-01 / UX-04 的同一类问题。
    pub(super) hovered: Option<usize>,
}

/// Relative "n units ago" label for history rows.
pub(super) fn relative_time_ago(at: std::time::Instant, now: std::time::Instant) -> String {
    let t = &crate::i18n::texts().history;
    let seconds = now.saturating_duration_since(at).as_secs();
    if seconds < 60 {
        return t.just_now.to_owned();
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return crate::i18n::fill(t.minutes_ago_fmt, &[("n", minutes.to_string().as_str())]);
    }
    let hours = minutes / 60;
    if hours < 24 {
        return crate::i18n::fill(t.hours_ago_fmt, &[("n", hours.to_string().as_str())]);
    }
    crate::i18n::fill(t.days_ago_fmt, &[("n", (hours / 24).to_string().as_str())])
}

fn thumb_contains(
    track: Rect,
    metrics: Option<crate::pane::ScrollMetrics>,
    point: (u16, u16),
) -> bool {
    metrics
        .and_then(|metrics| crate::ui::scrollbar_thumb(metrics, track))
        .is_some_and(|thumb| {
            super::contains(Rect::new(track.x, thumb.top, track.width, thumb.len), point)
        })
}

impl ClientShellState {
    /// Current spinner frame while any long-running state is visible. With
    /// animations off — or the spinner disabled outright — the first frame
    /// is shown statically.
    pub(super) fn spinner_glyph(&self) -> &'static str {
        if self.config.feedback.animations && self.config.feedback.spinner.enabled {
            self.config.feedback.spinner.frame(self.spinner_tick)
        } else {
            self.config.feedback.spinner.frame(0)
        }
    }

    /// Whether any visible surface should animate a spinner right now:
    /// endpoint connecting/reconnecting rows, machine bootstrap progress,
    /// worktree create/open/remove in flight, or integrations loading.
    pub(super) fn spinner_active(&self) -> bool {
        self.config.feedback.spinner.enabled
            && (self.endpoints.iter().any(|endpoint| {
                matches!(
                    endpoint.status,
                    ClientEndpointStatus::Connecting | ClientEndpointStatus::Reconnecting
                )
            }) || match self.overlay.as_ref() {
                Some(ClientShellOverlay::Machines(overlay)) => overlay.bootstrap_running(),
                Some(ClientShellOverlay::MachineAuth(overlay)) => overlay.auth_work_running(),
                Some(ClientShellOverlay::WorktreeCreate(overlay)) => overlay.creating,
                Some(ClientShellOverlay::WorktreeOpen(overlay)) => overlay.opening,
                Some(ClientShellOverlay::WorktreeRemove(overlay)) => overlay.removing,
                Some(ClientShellOverlay::Settings(overlay)) => {
                    overlay.loading_integrations || overlay.installing_integrations
                }
                _ => false,
            })
    }

    /// Advances the feedback-layer clocks. Returns true when a repaint is
    /// needed: spinner frame changed, bell expired, or an entrance settled.
    pub(crate) fn tick_chrome_feedback(&mut self, now: std::time::Instant) -> bool {
        let mut repaint = false;
        if self
            .visual_bell_deadline
            .is_some_and(|deadline| now >= deadline)
        {
            self.visual_bell_deadline = None;
            repaint = true;
        }
        // 机器面板的公共 toast 到期自动消失（与 `feedback.animations` 无关：
        // 它是一次性反馈，不是入场动画）。
        if let Some(ClientShellOverlay::Machines(overlay)) = self.content_page_mut() {
            if overlay.message.as_ref().is_some_and(|toast| {
                now.duration_since(toast.at) >= super::machines_overlay::MACHINE_TOAST_DURATION
            }) {
                overlay.message = None;
                repaint = true;
            }
        }
        if self.config.feedback.animations {
            for since in [&mut self.overlay_since, &mut self.toast_since] {
                if since.is_some_and(|start| now.duration_since(start) >= ENTRANCE_DURATION) {
                    *since = None;
                    repaint = true;
                }
            }
            if self.spinner_active() {
                let interval = self.config.feedback.spinner.interval;
                match self.spinner_advanced_at {
                    // First tick with a visible spinner: start the clock on
                    // frame zero; the interval wake does the first advance.
                    None => {
                        self.spinner_advanced_at = Some(now);
                    }
                    Some(last) if now.duration_since(last) >= interval => {
                        self.spinner_tick = self.spinner_tick.saturating_add(1);
                        self.spinner_advanced_at = Some(now);
                        repaint = true;
                    }
                    _ => {}
                }
            } else {
                self.spinner_advanced_at = None;
            }
        } else {
            self.overlay_since = None;
            self.toast_since = None;
            self.spinner_advanced_at = None;
        }
        repaint
    }

    /// Next deadline the feedback layer needs a timer wake for, if any.
    pub(super) fn chrome_feedback_deadline(&self) -> Option<std::time::Instant> {
        let mut deadline = self.visual_bell_deadline;
        let mut chain = |next: std::time::Instant| {
            deadline = Some(deadline.map_or(next, |current| current.min(next)));
        };
        if let Some(ClientShellOverlay::Machines(overlay)) = self.content_page() {
            if let Some(toast) = overlay.message.as_ref() {
                chain(toast.at + super::machines_overlay::MACHINE_TOAST_DURATION);
            }
        }
        if self.config.feedback.animations {
            for since in [self.overlay_since, self.toast_since].into_iter().flatten() {
                chain(since + ENTRANCE_DURATION);
            }
            if self.spinner_active() {
                if let Some(last) = self.spinner_advanced_at {
                    chain(last + self.config.feedback.spinner.interval);
                }
            }
        }
        deadline
    }

    /// Whether the visual bell emphasis is currently on screen.
    pub(super) fn visual_bell_active(&self) -> bool {
        self.visual_bell_deadline.is_some()
    }

    /// A pane on the active endpoint emitted BEL. The wire message carries
    /// no pane identity (generation-stable `TerminalBell { count }`), so the
    /// marker attributes to the focused tab/pane the user is looking at —
    /// the same attribution the audible host bell already has today.
    pub(crate) fn record_terminal_bell(&mut self, now: std::time::Instant) -> bool {
        if !self.config.feedback.visual_bell {
            return false;
        }
        self.visual_bell_deadline = Some(now + VISUAL_BELL_DURATION);
        true
    }

    /// Appends one entry to the notification history ring (newest last).
    pub(super) fn record_notification(
        &mut self,
        level: ClientToastLevel,
        title: impl Into<String>,
        body: Option<String>,
        target: Option<ClientNotificationTarget>,
    ) {
        if self.notification_history.len() >= NOTIFICATION_HISTORY_LIMIT {
            self.notification_history.pop_front();
        }
        self.notification_history
            .push_back(ClientNotificationRecord {
                level,
                title: title.into(),
                body,
                received_at: std::time::Instant::now(),
                target,
            });
    }

    pub(super) fn open_notification_history(&mut self) {
        let selected = self.notification_history.len().saturating_sub(1);
        self.overlay = Some(ClientShellOverlay::NotificationHistory(
            ClientNotificationHistoryOverlay {
                selected,
                hovered: None,
            },
        ));
    }

    pub(super) fn move_notification_history_selection(&mut self, delta: isize) {
        if self.notification_history.is_empty() {
            return;
        }
        let Some(ClientShellOverlay::NotificationHistory(overlay)) = self.overlay.as_mut() else {
            return;
        };
        let last = self.notification_history.len().saturating_sub(1) as isize;
        overlay.selected = (overlay.selected as isize + delta).clamp(0, last) as usize;
        // 视口跟着选中走，指针下面的行可能已经换了。
        overlay.hovered = None;
    }

    /// 指针悬浮行。`None` 表示指针不在任何行上——出界也要写，否则弱底色会
    /// 留在鼠标早已离开的那一行（MENU-01）。只写 `hovered`，不动 `selected`。
    pub(super) fn set_notification_history_hover(&mut self, hovered: Option<usize>) -> bool {
        let count = self.notification_history.len();
        let Some(ClientShellOverlay::NotificationHistory(overlay)) = self.overlay.as_mut() else {
            return false;
        };
        let hovered = hovered.filter(|index| *index < count);
        let changed = overlay.hovered != hovered;
        overlay.hovered = hovered;
        changed
    }

    /// 键盘或点击直接落到某一行；返回 true 表示选中行变了。
    pub(super) fn set_notification_history_selection(&mut self, index: usize) -> bool {
        if self.notification_history.is_empty() {
            return false;
        }
        let Some(ClientShellOverlay::NotificationHistory(overlay)) = self.overlay.as_mut() else {
            return false;
        };
        let next = index.min(self.notification_history.len().saturating_sub(1));
        let changed = overlay.selected != next;
        overlay.selected = next;
        changed
    }

    /// Enter on a history row: jump to the originating pane (across
    /// endpoints when needed), mirroring `focus_visible_notification`.
    pub(super) fn focus_notification_history_target(&mut self, outcome: &mut ClientShellInput) {
        let Some(ClientShellOverlay::NotificationHistory(overlay)) = self.overlay.as_ref() else {
            return;
        };
        let Some(target) = self
            .notification_history
            .get(overlay.selected)
            .and_then(|record| record.target.clone())
        else {
            return;
        };
        let Some(pane_id) = target.pane_id.clone() else {
            return;
        };
        if !self.endpoint_is_online(&target.endpoint_id) {
            let label = self.endpoint_label(&target.endpoint_id).to_owned();
            self.receive_endpoint_unavailable(format!("{label} is unavailable"));
            outcome.repaint = true;
            return;
        }
        self.overlay = None;
        if target.endpoint_id == self.active_endpoint_id {
            self.push_endpoint_method(
                crate::api::schema::Method::PaneFocus(crate::api::schema::PaneTarget { pane_id }),
                outcome,
            );
        } else {
            outcome.actions.push(ClientShellAction::ActivateEndpoint {
                endpoint_id: target.endpoint_id,
                target: Some(ClientEndpointFocusTarget::Pane(pane_id)),
            });
        }
        outcome.repaint = true;
    }

    /// Recomputes the chrome hover target on mouse move; repaints only when
    /// the identity changed. With `ui.hover_effects` off (or while dragging)
    /// this is a single bool check per move event and never allocates.
    pub(super) fn update_chrome_hover(
        &mut self,
        point: (u16, u16),
        outcome: &mut ClientShellInput,
    ) {
        if !self.config.feedback.hover_effects || !self.config.mouse_capture {
            if self.hover.take().is_some() {
                outcome.repaint = true;
            }
            return;
        }
        if self.chrome_drag.is_some() {
            if self.hover.take().is_some() {
                outcome.repaint = true;
            }
            return;
        }
        if let Some(current) = self.hover.as_ref() {
            if self.chrome_hover_contains(current, point) {
                return;
            }
        }
        let next = self.chrome_hover_at(point);
        if next != self.hover {
            self.hover = next;
            outcome.repaint = true;
        }
    }

    /// Cheap revalidation of the current hover identity against the latest
    /// hit map; true when the pointer still rests on the same target.
    fn chrome_hover_contains(&self, hover: &ChromeHover, point: (u16, u16)) -> bool {
        let hits = &self.hits;
        match hover {
            ChromeHover::WorkspaceRow {
                endpoint_id,
                workspace_id,
            } => hits.workspaces.iter().any(|hit| {
                &hit.endpoint_id == endpoint_id
                    && &hit.workspace_id == workspace_id
                    && super::contains(hit.rect, point)
            }),
            ChromeHover::MachineRow(endpoint_id) => hits
                .machines
                .iter()
                .any(|hit| &hit.endpoint_id == endpoint_id && super::contains(hit.rect, point)),
            // 折叠开关落在行矩形之内：指针移到开关上时行悬浮必须让位。
            ChromeHover::AgentRow(pane_id) => {
                !on_agent_tree_toggle(hits, point)
                    && hits
                        .agents
                        .iter()
                        .any(|(rect, id)| id == pane_id && super::contains(*rect, point))
            }
            ChromeHover::AgentGroupRow(key) => hits
                .agent_group_toggles
                .iter()
                .any(|(rect, id)| id == key && super::contains(*rect, point)),
            ChromeHover::EndpointAgentRow(endpoint_id, pane_id) => {
                !on_agent_tree_toggle(hits, point)
                    && hits.endpoint_agents.iter().any(|(rect, id, pane)| {
                        id == endpoint_id && pane == pane_id && super::contains(*rect, point)
                    })
            }
            ChromeHover::AgentTreeToggle(endpoint_id, key) => {
                hits.agent_tree_toggles.iter().any(|(rect, id, toggle)| {
                    id == endpoint_id && toggle == key && super::contains(*rect, point)
                })
            }
            ChromeHover::AgentActivityRow(endpoint_id, owner_key, node_id) => {
                !on_agent_tree_toggle(hits, point)
                    && hits.agent_activity_rows.iter().any(|hit| {
                        &hit.endpoint_id == endpoint_id
                            && &hit.owner_key == owner_key
                            && &hit.node_id == node_id
                            && super::contains(hit.rect, point)
                    })
            }
            ChromeHover::ExternalAgentGroup(endpoint_id, source) => {
                hits.external_agent_groups.iter().any(|(rect, id, group)| {
                    id == endpoint_id && group == source && super::contains(*rect, point)
                })
            }
            ChromeHover::ExternalAgentRow(endpoint_id, external_id) => {
                !on_agent_tree_toggle(hits, point)
                    && hits.external_agents.iter().any(|(rect, id, external)| {
                        id == endpoint_id
                            && external == external_id
                            && super::contains(*rect, point)
                    })
            }
            ChromeHover::AgentActivityNode(node_id) => hits
                .agent_activity_tree_rows
                .iter()
                .any(|(rect, id)| id == node_id && super::contains(*rect, point)),
            ChromeHover::AgentActivityButton(button) => hits
                .agent_activity_actions
                .iter()
                .any(|(rect, candidate)| candidate == button && super::contains(*rect, point)),
            ChromeHover::Tab(tab_id) => hits
                .tabs
                .iter()
                .any(|(rect, id)| id == tab_id && super::contains(*rect, point)),
            ChromeHover::NewTab => super::contains(hits.new_tab, point),
            ChromeHover::TabScrollLeft => super::contains(hits.tab_scroll_left, point),
            ChromeHover::TabScrollRight => super::contains(hits.tab_scroll_right, point),
            ChromeHover::GlobalLauncher => super::contains(hits.global_launcher, point),
            ChromeHover::SidebarToggle => super::contains(hits.sidebar_toggle, point),
            ChromeHover::AgentSortToggle => super::contains(hits.agent_sort_toggle, point),
            ChromeHover::WorkspaceScrollbarThumb => thumb_contains(
                hits.workspace_scrollbar,
                hits.workspace_scroll_metrics,
                point,
            ),
            ChromeHover::AgentScrollbarThumb => {
                thumb_contains(hits.agent_scrollbar, hits.agent_scroll_metrics, point)
            }
            ChromeHover::HelpScrollbarThumb => {
                thumb_contains(hits.help_scrollbar, hits.help_scroll_metrics, point)
            }
            ChromeHover::ReleaseNotesScrollbarThumb => thumb_contains(
                hits.release_notes_scrollbar,
                hits.release_notes_scroll_metrics,
                point,
            ),
            ChromeHover::ProductAnnouncementScrollbarThumb => thumb_contains(
                hits.product_announcement_scrollbar,
                hits.product_announcement_scroll_metrics,
                point,
            ),
            ChromeHover::SidebarDivider => super::contains(hits.sidebar_divider, point),
            ChromeHover::SidebarSectionDivider => {
                super::contains(hits.sidebar_section_divider, point)
            }
            ChromeHover::NotificationToast => super::contains(hits.notification_toast, point),
            ChromeHover::OverlayPrimary => super::contains(hits.overlay_primary, point),
            ChromeHover::OverlayClear => super::contains(hits.overlay_clear, point),
            ChromeHover::OverlayCancel => super::contains(hits.overlay_cancel, point),
            ChromeHover::MachineButton(button) => hits
                .machines_actions
                .iter()
                .any(|(rect, candidate)| candidate == button && super::contains(*rect, point)),
            ChromeHover::MachineAuthButton(button) => hits
                .machine_auth_actions
                .iter()
                .any(|(rect, candidate)| candidate == button && super::contains(*rect, point)),
            ChromeHover::SnippetButton(button) => hits
                .snippet_actions
                .iter()
                .any(|(rect, candidate)| candidate == button && super::contains(*rect, point)),
            ChromeHover::SceneButton(button) => hits
                .scenes_actions
                .iter()
                .any(|(rect, candidate)| candidate == button && super::contains(*rect, point)),
            ChromeHover::BroadcastButton(button) => hits
                .broadcast_actions
                .iter()
                .any(|(rect, candidate)| candidate == button && super::contains(*rect, point)),
            ChromeHover::MachineFilesButton(button) => hits
                .machine_files_actions
                .iter()
                .any(|(rect, candidate)| candidate == button && super::contains(*rect, point)),
            ChromeHover::LifecycleBannerRetry => {
                super::contains(hits.lifecycle_banner_retry, point)
            }
            ChromeHover::LifecycleBannerGiveUp => {
                super::contains(hits.lifecycle_banner_give_up, point)
            }
        }
    }

    /// Full hover resolution against the previous frame's hit map. Runs only
    /// when the current target stopped matching, so the common case (pointer
    /// stationary over one row) never allocates.
    fn chrome_hover_at(&self, point: (u16, u16)) -> Option<ChromeHover> {
        let hits = &self.hits;
        if self.overlay.is_some() {
            if super::contains(hits.overlay_primary, point) {
                return Some(ChromeHover::OverlayPrimary);
            }
            if super::contains(hits.overlay_clear, point) {
                return Some(ChromeHover::OverlayClear);
            }
            if super::contains(hits.overlay_cancel, point) {
                return Some(ChromeHover::OverlayCancel);
            }
            if let Some((_, button)) = hits
                .machines_actions
                .iter()
                .find(|(rect, _)| super::contains(*rect, point))
            {
                return Some(ChromeHover::MachineButton(*button));
            }
            if let Some((_, button)) = hits
                .machine_auth_actions
                .iter()
                .find(|(rect, _)| super::contains(*rect, point))
            {
                return Some(ChromeHover::MachineAuthButton(*button));
            }
            if let Some((_, button)) = hits
                .snippet_actions
                .iter()
                .find(|(rect, _)| super::contains(*rect, point))
            {
                return Some(ChromeHover::SnippetButton(*button));
            }
            if let Some((_, button)) = hits
                .scenes_actions
                .iter()
                .find(|(rect, _)| super::contains(*rect, point))
            {
                return Some(ChromeHover::SceneButton(*button));
            }
            if let Some((_, button)) = hits
                .broadcast_actions
                .iter()
                .find(|(rect, _)| super::contains(*rect, point))
            {
                return Some(ChromeHover::BroadcastButton(*button));
            }
            if let Some((_, button)) = hits
                .machine_files_actions
                .iter()
                .find(|(rect, _)| super::contains(*rect, point))
            {
                return Some(ChromeHover::MachineFilesButton(*button));
            }
            if let Some((_, button)) = hits
                .agent_activity_actions
                .iter()
                .find(|(rect, _)| super::contains(*rect, point))
            {
                return Some(ChromeHover::AgentActivityButton(*button));
            }
            if let Some((_, node_id)) = hits
                .agent_activity_tree_rows
                .iter()
                .find(|(rect, _)| super::contains(*rect, point))
            {
                return Some(ChromeHover::AgentActivityNode(node_id.clone()));
            }
            if thumb_contains(hits.help_scrollbar, hits.help_scroll_metrics, point) {
                return Some(ChromeHover::HelpScrollbarThumb);
            }
            if thumb_contains(
                hits.release_notes_scrollbar,
                hits.release_notes_scroll_metrics,
                point,
            ) {
                return Some(ChromeHover::ReleaseNotesScrollbarThumb);
            }
            if thumb_contains(
                hits.product_announcement_scrollbar,
                hits.product_announcement_scroll_metrics,
                point,
            ) {
                return Some(ChromeHover::ProductAnnouncementScrollbarThumb);
            }
            return None;
        }
        // 指针不在任何 chrome 分组里（最常见的「浮在 pane 上」）：直接判定不是
        // chrome 悬浮，不再走下面整份线性扫描（HERDR-PERF-008）。
        if !hits.chrome_bounds.contains(point) {
            return None;
        }
        if super::contains(hits.notification_toast, point)
            && (self.visible_endpoint_notice.is_some()
                || self
                    .visible_notification
                    .as_ref()
                    .is_some_and(|notification| notification.event.pane_id.is_some()))
        {
            return Some(ChromeHover::NotificationToast);
        }
        if super::contains(hits.lifecycle_banner_retry, point) {
            return Some(ChromeHover::LifecycleBannerRetry);
        }
        if super::contains(hits.lifecycle_banner_give_up, point) {
            return Some(ChromeHover::LifecycleBannerGiveUp);
        }
        for (rect, tab_id) in &hits.tabs {
            if super::contains(*rect, point) {
                return Some(ChromeHover::Tab(tab_id.clone()));
            }
        }
        if super::contains(hits.new_tab, point) {
            return Some(ChromeHover::NewTab);
        }
        if super::contains(hits.tab_scroll_left, point) {
            return Some(ChromeHover::TabScrollLeft);
        }
        if super::contains(hits.tab_scroll_right, point) {
            return Some(ChromeHover::TabScrollRight);
        }
        for hit in &hits.machines {
            if super::contains(hit.rect, point) {
                return Some(ChromeHover::MachineRow(hit.endpoint_id.clone()));
            }
        }
        for hit in &hits.workspaces {
            if super::contains(hit.rect, point) {
                return Some(ChromeHover::WorkspaceRow {
                    endpoint_id: hit.endpoint_id.clone(),
                    workspace_id: hit.workspace_id.clone(),
                });
            }
        }
        // 折叠开关的矩形落在行矩形之内：必须排在 agents / endpoint_agents /
        // agent_activity_rows / external_agents 之前。
        for (rect, endpoint_id, key) in &hits.agent_tree_toggles {
            if super::contains(*rect, point) {
                return Some(ChromeHover::AgentTreeToggle(
                    endpoint_id.clone(),
                    key.clone(),
                ));
            }
        }
        for (rect, pane_id) in &hits.agents {
            if super::contains(*rect, point) {
                return Some(ChromeHover::AgentRow(pane_id.clone()));
            }
        }
        for (rect, key) in &hits.agent_group_toggles {
            if super::contains(*rect, point) {
                return Some(ChromeHover::AgentGroupRow(key.clone()));
            }
        }
        for (rect, endpoint_id, pane_id) in &hits.endpoint_agents {
            if super::contains(*rect, point) {
                return Some(ChromeHover::EndpointAgentRow(
                    endpoint_id.clone(),
                    pane_id.clone(),
                ));
            }
        }
        for hit in &hits.agent_activity_rows {
            if super::contains(hit.rect, point) {
                return Some(ChromeHover::AgentActivityRow(
                    hit.endpoint_id.clone(),
                    hit.owner_key.clone(),
                    hit.node_id.clone(),
                ));
            }
        }
        for (rect, endpoint_id, source) in &hits.external_agent_groups {
            if super::contains(*rect, point) {
                return Some(ChromeHover::ExternalAgentGroup(
                    endpoint_id.clone(),
                    source.clone(),
                ));
            }
        }
        for (rect, endpoint_id, external_id) in &hits.external_agents {
            if super::contains(*rect, point) {
                return Some(ChromeHover::ExternalAgentRow(
                    endpoint_id.clone(),
                    external_id.clone(),
                ));
            }
        }
        if thumb_contains(
            hits.workspace_scrollbar,
            hits.workspace_scroll_metrics,
            point,
        ) {
            return Some(ChromeHover::WorkspaceScrollbarThumb);
        }
        if thumb_contains(hits.agent_scrollbar, hits.agent_scroll_metrics, point) {
            return Some(ChromeHover::AgentScrollbarThumb);
        }
        if super::contains(hits.agent_sort_toggle, point) {
            return Some(ChromeHover::AgentSortToggle);
        }
        if super::contains(hits.sidebar_section_divider, point) {
            return Some(ChromeHover::SidebarSectionDivider);
        }
        if super::contains(hits.sidebar_toggle, point) {
            return Some(ChromeHover::SidebarToggle);
        }
        if super::contains(hits.global_launcher, point) {
            return Some(ChromeHover::GlobalLauncher);
        }
        if super::contains(hits.sidebar_divider, point) {
            return Some(ChromeHover::SidebarDivider);
        }
        None
    }
}

/// 指针是否落在某个树节点折叠开关上。开关矩形在行矩形之内，行类悬浮目标的
/// 复核要先排除它，否则指针从行移到开关上时悬浮不会切换。
fn on_agent_tree_toggle(hits: &ShellHitMap, point: (u16, u16)) -> bool {
    hits.agent_tree_toggles
        .iter()
        .any(|(rect, _, _)| super::contains(*rect, point))
}

/// chrome 悬浮区的分组包围盒：侧栏、顶栏（标签条 / 全局入口）、横幅。
/// 分组而不是一个大并集——标签条在顶、侧栏在左、横幅在底部，并集很快退化成
/// 整屏，早退就永远不生效。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct ChromeBounds {
    sidebar: Rect,
    top: Rect,
    banners: Rect,
}

impl ChromeBounds {
    pub(super) fn contains(&self, point: (u16, u16)) -> bool {
        super::contains(self.sidebar, point)
            || super::contains(self.top, point)
            || super::contains(self.banners, point)
    }
}

impl ShellHitMap {
    /// 重算 chrome 悬浮区分组包围盒（每帧 compose 收尾调用一次）。矩形来自
    /// `chrome_hover_at` 会查询的每一处，缺一个就会让那块区域的悬浮失效，
    /// 所以新增命中区字段时这里要同步。
    pub(super) fn rebuild_chrome_bounds(&mut self) {
        let mut sidebar = Rect::default();
        let mut top = Rect::default();
        let mut banners = Rect::default();
        let extend = |bounds: &mut Rect, rect: Rect| {
            if rect.is_empty() {
                return;
            }
            *bounds = if bounds.is_empty() {
                rect
            } else {
                bounds.union(rect)
            };
        };
        extend(&mut banners, self.notification_toast);
        extend(&mut banners, self.lifecycle_banner_retry);
        extend(&mut banners, self.lifecycle_banner_give_up);
        for (rect, _) in &self.tabs {
            extend(&mut top, *rect);
        }
        extend(&mut top, self.new_tab);
        extend(&mut top, self.tab_scroll_left);
        extend(&mut top, self.tab_scroll_right);
        for hit in &self.machines {
            extend(&mut sidebar, hit.rect);
        }
        for hit in &self.workspaces {
            extend(&mut sidebar, hit.rect);
        }
        for (rect, _) in &self.agents {
            extend(&mut sidebar, *rect);
        }
        for (rect, _) in &self.agent_group_toggles {
            extend(&mut sidebar, *rect);
        }
        for (rect, _, _) in &self.endpoint_agents {
            extend(&mut sidebar, *rect);
        }
        // 统一树的新命中向量全部并进侧栏组：否则包围盒早退（HERDR-PERF-008）会
        // 让这些行收不到悬浮。
        for (rect, _, _) in &self.agent_tree_toggles {
            extend(&mut sidebar, *rect);
        }
        for hit in &self.agent_activity_rows {
            extend(&mut sidebar, hit.rect);
        }
        for (rect, _, _) in &self.external_agent_groups {
            extend(&mut sidebar, *rect);
        }
        for (rect, _, _) in &self.external_agents {
            extend(&mut sidebar, *rect);
        }
        extend(&mut sidebar, self.workspace_scrollbar);
        extend(&mut sidebar, self.agent_scrollbar);
        extend(&mut sidebar, self.agent_sort_toggle);
        extend(&mut sidebar, self.sidebar_section_divider);
        extend(&mut sidebar, self.sidebar_toggle);
        extend(&mut sidebar, self.sidebar_divider);
        // 全局入口在侧栏页脚（经典布局）或顶栏（工作台）：两处都并进侧栏组，
        // 它既不与标签条同排也不在横幅区。
        extend(&mut sidebar, self.global_launcher);
        self.chrome_bounds = ChromeBounds {
            sidebar,
            top,
            banners,
        };
    }
}
