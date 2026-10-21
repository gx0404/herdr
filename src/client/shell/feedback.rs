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
/// border color: `toast_border_*` component tokens for info / success / error,
/// and the shared "needs input" status token for warnings, so a toast that asks
/// the user to act reads the same as the agent state it reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ClientToastLevel {
    Info,
    Success,
    /// 需要你处理（agent 等待输入、资源告警等）：琥珀色 `◆`，与「需要输入」状态同色，
    /// 不与执行失败共用红色。
    Warning,
    Error,
}

impl ClientToastLevel {
    pub(super) fn icon(self) -> &'static str {
        match self {
            Self::Info => "●",
            Self::Success => "✓",
            Self::Warning => "◆",
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
            Self::Warning => components.status_input,
            Self::Error => components.toast_border_error,
        }
    }

    pub(super) fn from_notification_kind(kind: SemanticNotificationKind) -> Self {
        match kind {
            SemanticNotificationKind::NeedsAttention => Self::Warning,
            SemanticNotificationKind::Finished => Self::Success,
            SemanticNotificationKind::UpdateInstalled | SemanticNotificationKind::Custom => {
                Self::Info
            }
        }
    }

    /// 语义通知的等级：状态桶优先（需要输入 → 告警，失败 → 错误，完成待查看 → 成功），
    /// 没有状态桶时按通知种类。吐司描边、通知历史图标与颜色共用这一份映射。
    pub(super) fn for_notification(
        bucket: crate::harness::AgentBucket,
        kind: SemanticNotificationKind,
    ) -> Self {
        use crate::harness::AgentBucket;
        match bucket {
            AgentBucket::NeedsInput => Self::Warning,
            AgentBucket::Failed => Self::Error,
            AgentBucket::Review => Self::Success,
            _ => Self::from_notification_kind(kind),
        }
    }

    pub(super) fn from_notice_kind(kind: ClientEndpointNoticeKind) -> Self {
        match kind {
            // 警告请你处理，用琥珀色；被拒绝 / 不支持是请求失败，用错误色。
            ClientEndpointNoticeKind::Warning => Self::Warning,
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
    /// 标签上的关闭按钮（指针在它上面时所在标签也算悬浮）。
    TabClose(String),
    NewTab,
    TabScrollLeft,
    TabScrollRight,
    GlobalLauncher,
    /// 标题栏的注意力计数（按状态桶）。
    TitleChip(crate::harness::AgentBucket),
    /// 标题栏的命令搜索提示。
    TitleCommand,
    /// 标题栏的 `⋯` 布局菜单入口。
    TitleMenu,
    /// 标题栏的「调整布局」标记（点它退出调整布局）。
    TitleArrange,
    /// 停靠面板的面板头：悬浮时把手与按钮亮起来。
    PanelHeader(super::dock::PanelId),
    /// 窗格的上边框（标题行）：悬浮时画 `⠿` 拖动把手。按窗格 id 认身份。
    PaneHeader(String),
    /// 面板头右侧的按钮。
    PanelButton(
        super::dock::PanelId,
        super::chrome::panel_header::PanelButton,
    ),
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
    SettingsSection(ClientSettingsSection),
    SettingsChoice(usize),
    WelcomeAction(super::chrome::welcome::Action),
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
    Inspector(Rect),
    Session(super::chrome::sessions::SessionHit),
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
    pub(super) boot_id: Option<String>,
}

/// One entry of the notification history ring.
#[derive(Clone, Debug)]
pub(super) struct ClientNotificationRecord {
    pub(super) level: ClientToastLevel,
    pub(super) bucket: Option<crate::harness::AgentBucket>,
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
    crate::ui::design::ago_label(now.saturating_duration_since(at).as_secs())
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

    /// Agent 状态字形的取法：运行中按全局转圈帧画 `design::SPINNER_FRAMES`；关掉动画
    /// 或转圈时画静态 `◌`。侧栏行、标签与标题栏计数共用这一个时钟，同相位。
    pub(super) fn status_glyphs(&self) -> StatusGlyphs {
        StatusGlyphs {
            style: self.config.status_indicators,
            frame: usize::try_from(self.spinner_tick).unwrap_or(0),
            animate: self.config.feedback.animations && self.config.feedback.spinner.enabled,
        }
    }

    /// 当前投影的端点不在线（断线 / 重连中）：标签、面包屑里的状态字形来自缓存快照，
    /// 与侧栏的 stale 行同口径画静态字形、置灰。
    pub(super) fn active_endpoint_stale(&self) -> bool {
        self.endpoint_stale(&self.active_endpoint_id)
    }

    /// 某个端点不在线（断线 / 重连中 / 需处理）：它的数据来自缓存快照，状态字形画
    /// 静态、不驱动转圈。与 [`Self::active_endpoint_stale`] 同一口径。
    pub(super) fn endpoint_stale(&self, endpoint_id: &ClientEndpointId) -> bool {
        self.endpoint_status(endpoint_id)
            .is_some_and(|status| status != ClientEndpointStatus::Online)
    }

    /// Whether any visible surface should animate a spinner right now:
    /// endpoint connecting/reconnecting rows, machine bootstrap progress,
    /// worktree create/open/remove in flight, integrations loading, or an
    /// agent (or one of its activity nodes) working on an online endpoint.
    /// The agent part reads the view-phase cache, so a frame with nothing
    /// running never arms the clock.
    pub(super) fn spinner_active(&self) -> bool {
        self.config.feedback.spinner.enabled
            && (self
                .agent_attention
                .as_ref()
                .is_some_and(|rows| rows.animated())
                || self.inspector.animated() && !self.active_endpoint_stale()
                || self.endpoints.iter().any(|endpoint| {
                    matches!(
                        endpoint.status,
                        ClientEndpointStatus::Connecting | ClientEndpointStatus::Reconnecting
                    )
                })
                || match self.overlay.as_ref() {
                    Some(ClientShellOverlay::Machines(overlay)) => overlay.bootstrap_running(),
                    Some(ClientShellOverlay::MachineAuth(overlay)) => overlay.auth_work_running(),
                    Some(ClientShellOverlay::WorktreeCreate(overlay)) => overlay.creating,
                    Some(ClientShellOverlay::WorktreeOpen(overlay)) => overlay.opening,
                    Some(ClientShellOverlay::WorktreeRemove(overlay)) => overlay.removing,
                    Some(ClientShellOverlay::Settings(overlay)) => {
                        overlay.loading_integrations || overlay.installing_integrations
                    }
                    // 子智能体页面上的运行中节点画转圈；属主自己不在运行时也要走时钟。页面所属
                    // 端点离线时节点来自缓存，不转。
                    Some(ClientShellOverlay::AgentActivity(overlay)) => {
                        overlay.has_running() && !self.endpoint_stale(&overlay.endpoint_id)
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
        if !self.session_view.area.is_empty()
            && self
                .session_rows
                .as_ref()
                .is_some_and(|cache| cache.ages_expired(self.wall_clock_ms()))
        {
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
                bucket: None,
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
        if target
            .boot_id
            .as_deref()
            .is_some_and(|boot| self.endpoint_boot_id(&target.endpoint_id) != Some(boot))
        {
            self.receive_endpoint_unavailable(
                super::chrome::command::text(
                    "This notification belongs to an earlier server session",
                    "此通知来自较早的服务器会话",
                )
                .to_owned(),
            );
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
            // 折叠开关落在行矩形之内：指针移到开关上时行悬浮必须让位。统一树的
            // 每一层（机器 / 工作区 / tab / agent / 活动节点）都要带这个前缀。
            ChromeHover::WorkspaceRow {
                endpoint_id,
                workspace_id,
            } => {
                !on_agent_tree_toggle(hits, point)
                    && hits.workspaces.iter().any(|hit| {
                        &hit.endpoint_id == endpoint_id
                            && &hit.workspace_id == workspace_id
                            && super::contains(hit.rect, point)
                    })
            }
            ChromeHover::MachineRow(endpoint_id) => {
                !on_agent_tree_toggle(hits, point)
                    && hits.machines.iter().any(|hit| {
                        &hit.endpoint_id == endpoint_id && super::contains(hit.rect, point)
                    })
            }
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
                    && hits.endpoint_agents.iter().chain(&hits.session_agents).any(
                        |(rect, id, pane)| {
                            id == endpoint_id && pane == pane_id && super::contains(*rect, point)
                        },
                    )
            }
            ChromeHover::AgentTreeToggle(endpoint_id, key) => hits
                .agent_tree_toggles
                .iter()
                .chain(&hits.session_tree_toggles)
                .any(|(rect, id, toggle)| {
                    id == endpoint_id && toggle == key && super::contains(*rect, point)
                }),
            ChromeHover::AgentActivityRow(endpoint_id, owner_key, node_id) => {
                !on_agent_tree_toggle(hits, point)
                    && hits
                        .agent_activity_rows
                        .iter()
                        .chain(&hits.session_activity_rows)
                        .any(|hit| {
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
                    && hits
                        .external_agents
                        .iter()
                        .chain(&hits.session_external_agents)
                        .any(|(rect, id, external)| {
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
            ChromeHover::Inspector(target) => hits
                .inspector_actions
                .iter()
                .any(|(rect, _)| rect == target && super::contains(*rect, point)),
            ChromeHover::Session(target) => hits
                .sessions
                .iter()
                .any(|(rect, hit)| hit == target && super::contains(*rect, point)),
            ChromeHover::Tab(tab_id) => {
                !on_tab_close(hits, point)
                    && hits
                        .tabs
                        .iter()
                        .any(|(rect, id)| id == tab_id && super::contains(*rect, point))
            }
            ChromeHover::TabClose(tab_id) => hits
                .tab_close
                .iter()
                .any(|(rect, id)| id == tab_id && super::contains(*rect, point)),
            ChromeHover::NewTab => super::contains(hits.new_tab, point),
            ChromeHover::TabScrollLeft => super::contains(hits.tab_scroll_left, point),
            ChromeHover::TabScrollRight => super::contains(hits.tab_scroll_right, point),
            ChromeHover::GlobalLauncher => super::contains(hits.global_launcher, point),
            ChromeHover::TitleChip(bucket) => hits
                .title_chips
                .iter()
                .any(|(rect, candidate)| candidate == bucket && super::contains(*rect, point)),
            ChromeHover::TitleCommand => super::contains(hits.title_command, point),
            ChromeHover::TitleMenu => super::contains(hits.title_menu, point),
            ChromeHover::TitleArrange => super::contains(hits.title_arrange, point),
            ChromeHover::PanelHeader(panel) => {
                !on_panel_header_child(hits, point)
                    && hits
                        .panel_headers
                        .iter()
                        .any(|(rect, id)| id == panel && super::contains(*rect, point))
            }
            ChromeHover::PaneHeader(pane_id) => self
                .pane_header_at(point)
                .is_some_and(|hovered| hovered == pane_id),
            ChromeHover::PanelButton(panel, button) => {
                hits.panel_buttons.iter().any(|(rect, id, candidate)| {
                    id == panel && candidate == button && super::contains(*rect, point)
                })
            }
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
            ChromeHover::NotificationToast => {
                super::contains(hits.notification_toast, point)
                    || super::contains(hits.endpoint_notice_toast, point)
            }
            ChromeHover::SettingsSection(section) => hits
                .settings_tabs
                .iter()
                .any(|(rect, candidate)| candidate == section && super::contains(*rect, point)),
            ChromeHover::SettingsChoice(index) => hits
                .settings_choices
                .iter()
                .any(|(rect, candidate)| candidate == index && super::contains(*rect, point)),
            ChromeHover::WelcomeAction(action) => hits
                .welcome_actions
                .iter()
                .any(|(rect, candidate)| candidate == action && super::contains(*rect, point)),
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
            if let Some((_, section)) = hits
                .settings_tabs
                .iter()
                .find(|(rect, _)| super::contains(*rect, point))
            {
                return Some(ChromeHover::SettingsSection(*section));
            }
            if let Some((_, index)) = hits
                .settings_choices
                .iter()
                .find(|(rect, _)| super::contains(*rect, point))
            {
                return Some(ChromeHover::SettingsChoice(*index));
            }
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
        if let Some((_, action)) = hits
            .welcome_actions
            .iter()
            .find(|(rect, _)| super::contains(*rect, point))
        {
            return Some(ChromeHover::WelcomeAction(*action));
        }
        // 指针不在任何 chrome 分组里（最常见的「浮在 pane 上」）：直接判定不是
        // chrome 悬浮，不再走下面整份线性扫描（HERDR-PERF-008）。面板头散落在各个
        // 面板顶端，并进分组会把包围盒撑成整屏，单独按面板数（个位数）查一遍。
        if !hits.chrome_bounds.contains(point) {
            return panel_header_hover(hits, point).or_else(|| self.pane_header_hover(point));
        }
        if let Some((rect, _)) = hits
            .inspector_actions
            .iter()
            .find(|(rect, _)| super::contains(*rect, point))
        {
            return Some(ChromeHover::Inspector(*rect));
        }
        // 两张吐司共用一个悬浮身份：同时显示时只有语义通知响应悬浮，端点通知
        // 单独显示时才响应（避免两张卡片一起加粗）。
        if (super::contains(hits.notification_toast, point)
            && self
                .visible_notification
                .as_ref()
                .is_some_and(|notification| notification.event.pane_id.is_some()))
            || (super::contains(hits.endpoint_notice_toast, point)
                && hits.notification_toast.is_empty())
        {
            return Some(ChromeHover::NotificationToast);
        }
        if super::contains(hits.lifecycle_banner_retry, point) {
            return Some(ChromeHover::LifecycleBannerRetry);
        }
        if super::contains(hits.lifecycle_banner_give_up, point) {
            return Some(ChromeHover::LifecycleBannerGiveUp);
        }
        // 关闭按钮落在标签矩形之内：必须先于标签查。
        for (rect, tab_id) in &hits.tab_close {
            if super::contains(*rect, point) {
                return Some(ChromeHover::TabClose(tab_id.clone()));
            }
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
        for (rect, bucket) in &hits.title_chips {
            if super::contains(*rect, point) {
                return Some(ChromeHover::TitleChip(*bucket));
            }
        }
        if super::contains(hits.title_command, point) {
            return Some(ChromeHover::TitleCommand);
        }
        if super::contains(hits.title_menu, point) {
            return Some(ChromeHover::TitleMenu);
        }
        if super::contains(hits.title_arrange, point) {
            return Some(ChromeHover::TitleArrange);
        }
        if let Some((_, hit)) = hits
            .sessions
            .iter()
            .rev()
            .find(|(rect, _)| super::contains(*rect, point))
        {
            return Some(ChromeHover::Session(hit.clone()));
        }
        // 折叠开关的矩形落在行矩形之内：必须排在统一树的全部行向量（machines /
        // workspaces / agents / endpoint_agents / agent_activity_rows /
        // external_agents）之前，否则机器层与工作区层的开关永远解析不到。
        for (rect, endpoint_id, key) in hits
            .agent_tree_toggles
            .iter()
            .chain(&hits.session_tree_toggles)
        {
            if super::contains(*rect, point) {
                return Some(ChromeHover::AgentTreeToggle(
                    endpoint_id.clone(),
                    key.clone(),
                ));
            }
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
        for (rect, endpoint_id, pane_id) in hits.endpoint_agents.iter().chain(&hits.session_agents)
        {
            if super::contains(*rect, point) {
                return Some(ChromeHover::EndpointAgentRow(
                    endpoint_id.clone(),
                    pane_id.clone(),
                ));
            }
        }
        for hit in hits
            .agent_activity_rows
            .iter()
            .chain(&hits.session_activity_rows)
        {
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
        for (rect, endpoint_id, external_id) in hits
            .external_agents
            .iter()
            .chain(&hits.session_external_agents)
        {
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
        panel_header_hover(hits, point).or_else(|| self.pane_header_hover(point))
    }

    fn pane_header_hover(&self, point: (u16, u16)) -> Option<ChromeHover> {
        self.pane_header_at(point)
            .map(|pane_id| ChromeHover::PaneHeader(pane_id.to_owned()))
    }

    /// 指针落在哪个窗格的上边框感应区里（只在工作台拖得动窗格时有）。感应区由上一帧
    /// 的窗格命中区推出（[`pane_header_sense`]），与把手显没显示无关——把手隐藏后
    /// 指针移回边框照样能让它重新出现。窗格数是个位到两位数，线性查即可。
    fn pane_header_at(&self, point: (u16, u16)) -> Option<&str> {
        if !self.workbench.enabled || self.workbench.dock.locked || !self.config.mouse_capture {
            return None;
        }
        if self
            .hits
            .popup
            .as_ref()
            .is_some_and(|popup| super::contains(popup.rect, point))
        {
            return None;
        }
        self.hits
            .panes
            .iter()
            .find(|hit| pane_header_sense(hit).is_some_and(|rect| super::contains(rect, point)))
            .map(|hit| hit.pane_id.as_str())
    }
}

/// 窗格上边框的悬浮感应区：带上边框、宽度够画把手（> 4 列）的窗格的整条上边框行。
/// 无边框窗格的把手只在调整布局时画，不需要悬浮感应。绘制与悬浮解析共用这一个判据。
pub(super) fn pane_header_sense(hit: &super::PaneHit) -> Option<Rect> {
    (hit.rect.width > 4 && hit.inner_rect.y > hit.rect.y)
        .then(|| Rect::new(hit.rect.x, hit.rect.y, hit.rect.width, 1))
}

/// 指针是否落在某个树节点折叠开关上。开关矩形在行矩形之内，行类悬浮目标的
/// 复核要先排除它，否则指针从行移到开关上时悬浮不会切换。
fn on_agent_tree_toggle(hits: &ShellHitMap, point: (u16, u16)) -> bool {
    hits.agent_tree_toggles
        .iter()
        .chain(&hits.session_tree_toggles)
        .any(|(rect, _, _)| super::contains(*rect, point))
        || hits.sessions.iter().any(|(rect, hit)| {
            matches!(hit, super::chrome::sessions::SessionHit::Toggle(..))
                && super::contains(*rect, point)
        })
}

/// 指针是否落在标签的关闭按钮上（按钮在标签矩形之内，标签悬浮的复核先排除它）。
fn on_tab_close(hits: &ShellHitMap, point: (u16, u16)) -> bool {
    hits.tab_close
        .iter()
        .any(|(rect, _)| super::contains(*rect, point))
}

/// 指针是否落在面板头里的子目标上：标签、标签关闭按钮、新建 / 滚动按钮与面板
/// 按钮都画在面板头那一行，各自的悬浮优先于面板头本身。
fn on_panel_header_child(hits: &ShellHitMap, point: (u16, u16)) -> bool {
    on_tab_close(hits, point)
        || hits
            .tabs
            .iter()
            .any(|(rect, _)| super::contains(*rect, point))
        || [hits.new_tab, hits.tab_scroll_left, hits.tab_scroll_right]
            .iter()
            .any(|rect| super::contains(*rect, point))
        || hits
            .panel_buttons
            .iter()
            .any(|(rect, _, _)| super::contains(*rect, point))
        || hits
            .sessions
            .iter()
            .any(|(rect, _)| super::contains(*rect, point))
}

/// 面板头的悬浮解析：先按钮、后整行。面板数是个位数，线性查即可。
fn panel_header_hover(hits: &ShellHitMap, point: (u16, u16)) -> Option<ChromeHover> {
    if let Some((_, panel, button)) = hits
        .panel_buttons
        .iter()
        .find(|(rect, _, _)| super::contains(*rect, point))
    {
        return Some(ChromeHover::PanelButton(panel.clone(), *button));
    }
    if on_panel_header_child(hits, point) {
        return None;
    }
    hits.panel_headers
        .iter()
        .find(|(rect, _)| super::contains(*rect, point))
        .map(|(_, panel)| ChromeHover::PanelHeader(panel.clone()))
}

/// chrome 悬浮区的分组包围盒：侧栏（含主菜单入口）、顶部（标题栏的可点项与标签条）、
/// 横幅与 Inspector。
/// 分组而不是一个大并集——标签条在顶、侧栏在左、横幅在底部，并集很快退化成
/// 整屏，早退就永远不生效。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct ChromeBounds {
    sidebar: Rect,
    top: Rect,
    banners: Rect,
    inspector: Rect,
}

impl ChromeBounds {
    pub(super) fn contains(&self, point: (u16, u16)) -> bool {
        super::contains(self.sidebar, point)
            || super::contains(self.top, point)
            || super::contains(self.banners, point)
            || super::contains(self.inspector, point)
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
        extend(&mut banners, self.endpoint_notice_toast);
        extend(&mut banners, self.lifecycle_banner_retry);
        extend(&mut banners, self.lifecycle_banner_give_up);
        for (rect, _) in &self.tabs {
            extend(&mut top, *rect);
        }
        for (rect, _) in &self.tab_close {
            extend(&mut top, *rect);
        }
        extend(&mut top, self.new_tab);
        extend(&mut top, self.tab_scroll_left);
        extend(&mut top, self.tab_scroll_right);
        // 标题栏（工作台第 0 行）的可点项与标签条同排在顶部；面板头不并进来（散落在
        // 各面板顶端，见 `chrome_hover_at`）。
        for (rect, _) in &self.title_chips {
            extend(&mut top, *rect);
        }
        extend(&mut top, self.title_command);
        extend(&mut top, self.title_menu);
        extend(&mut top, self.title_arrange);
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
        for (rect, _) in &self.sessions {
            extend(&mut sidebar, *rect);
        }
        for (rect, _, _) in self.endpoint_agents.iter().chain(&self.session_agents) {
            extend(&mut sidebar, *rect);
        }
        for (rect, _, _) in &self.session_tree_toggles {
            extend(&mut sidebar, *rect);
        }
        for hit in &self.session_activity_rows {
            extend(&mut sidebar, hit.rect);
        }
        for (rect, _, _) in &self.session_external_agents {
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
        // 全局入口在侧栏页脚（经典布局）或标题栏左端（工作台）：两处都并进侧栏组
        // （工作台的标题栏入口在侧栏面板正上方），也不在横幅区。
        extend(&mut sidebar, self.global_launcher);
        self.chrome_bounds = ChromeBounds {
            sidebar,
            top,
            banners,
            inspector: self.inspector_body,
        };
    }
}
