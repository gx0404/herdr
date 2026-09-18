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
    AgentUsageToggle,
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
            ClientNotificationHistoryOverlay { selected },
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
    }

    /// Mouse hover lands directly on a row; returns true when it moved.
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
            ChromeHover::AgentRow(pane_id) => hits
                .agents
                .iter()
                .any(|(rect, id)| id == pane_id && super::contains(*rect, point)),
            ChromeHover::AgentGroupRow(key) => hits
                .agent_group_toggles
                .iter()
                .any(|(rect, id)| id == key && super::contains(*rect, point)),
            ChromeHover::AgentUsageToggle => super::contains(hits.agent_usage_toggle, point),
            ChromeHover::EndpointAgentRow(endpoint_id, pane_id) => {
                hits.endpoint_agents.iter().any(|(rect, id, pane)| {
                    id == endpoint_id && pane == pane_id && super::contains(*rect, point)
                })
            }
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
        if super::contains(hits.agent_usage_toggle, point) {
            return Some(ChromeHover::AgentUsageToggle);
        }
        for (rect, endpoint_id, pane_id) in &hits.endpoint_agents {
            if super::contains(*rect, point) {
                return Some(ChromeHover::EndpointAgentRow(
                    endpoint_id.clone(),
                    pane_id.clone(),
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
