use super::*;

pub(super) const MIN_TAB_WIDTH: u16 = 8;
pub(super) const NEW_TAB_WIDTH: u16 = 3;
pub(super) const WORKSPACE_HEADER_ROWS: u16 = 2;
const ENDPOINT_ERROR_TIMEOUT_SECS: u64 = 5;

fn pane_surface_row<'a>(
    surface: &'a PaneSurfaceFrame,
    pane: &crate::protocol::PaneSurfacePane,
    absolute_row: u32,
) -> Option<&'a [crate::protocol::CellData]> {
    let viewport_top = pane
        .scroll
        .map(|scroll| {
            scroll
                .max_offset_from_bottom
                .saturating_sub(scroll.offset_from_bottom) as u32
        })
        .unwrap_or(0);
    let viewport_row = u16::try_from(absolute_row.checked_sub(viewport_top)?).ok()?;
    if viewport_row >= pane.inner_rect.height {
        return None;
    }
    let start = (usize::from(pane.inner_rect.y) + usize::from(viewport_row))
        * usize::from(surface.frame.width)
        + usize::from(pane.inner_rect.x);
    surface
        .frame
        .cells
        .get(start..start + usize::from(pane.inner_rect.width))
}

fn selection_cells_unchanged(
    selection: &crate::selection::Selection<String>,
    previous_surface: &PaneSurfaceFrame,
    previous_pane: &crate::protocol::PaneSurfacePane,
    next_surface: &PaneSurfaceFrame,
    next_pane: &crate::protocol::PaneSurfacePane,
) -> bool {
    let ((start_row, start_col), (end_row, end_col)) = selection.ordered_cells();
    (start_row..=end_row).all(|row| {
        let first_col = if row == start_row { start_col } else { 0 };
        let last_col = if row == end_row {
            end_col
        } else {
            previous_pane.inner_rect.width.saturating_sub(1)
        };
        pane_surface_row(previous_surface, previous_pane, row)
            .zip(pane_surface_row(next_surface, next_pane, row))
            .and_then(|(previous, next)| {
                previous
                    .get(usize::from(first_col)..=usize::from(last_col))
                    .zip(next.get(usize::from(first_col)..=usize::from(last_col)))
            })
            .is_some_and(|(previous, next)| {
                previous
                    .iter()
                    .zip(next)
                    .all(|(previous, next)| previous.symbol == next.symbol)
            })
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClientShellKeybindingSource {
    Local,
    RemoteLocal,
    Endpoint,
}

pub(crate) struct ClientShellConfig {
    pub(super) monitor: crate::config::MonitorConfig,
    pub(super) account_usage: crate::config::AccountUsageConfig,
    pub(super) sidebar_width: u16,
    pub(super) sidebar_min_width: u16,
    pub(super) sidebar_max_width: u16,
    pub(super) sidebar_start_collapsed: bool,
    pub(super) sidebar_collapsed_mode: SidebarCollapsedModeConfig,
    pub(super) mobile_width_threshold: u16,
    pub(super) tab_bar_position: TabBarPositionConfig,
    pub(super) hide_tab_bar_when_single_tab: bool,
    pub(super) spaces: SpacesSidebarConfig,
    pub(super) agents: crate::config::AgentsSidebarConfig,
    pub(super) agent_panel_sort: crate::config::AgentPanelSortConfig,
    pub(super) status_indicators: crate::config::StatusIndicatorStyle,
    pub(super) sound_enabled: bool,
    pub(super) toast_delivery: crate::config::ToastDelivery,
    pub(super) toast_delay_seconds: u64,
    pub(super) toast_position: crate::config::ToastHerdrPosition,
    pub(super) copy_on_select: bool,
    pub(super) clipboard_toast_enabled: bool,
    pub(super) clipboard_toast_position: crate::config::ToastClipboardPosition,
    pub(super) theme_name: String,
    pub(super) theme_runtime: crate::app::state::ThemeRuntimeConfig,
    pub(super) palette: Palette,
    /// Component tokens from the same theme pass as `palette`
    /// (`client_resolved_theme` with the locally detected host color depth).
    pub(super) components: crate::app::state::ComponentStyles,
    /// Host color depth resolved once from `ui.color_depth` at client start.
    pub(super) host_color_depth: crate::config::ColorDepth,
    /// Border glyph table resolved from `ui.border_style`.
    pub(super) border_glyphs: crate::ui::BorderGlyphs,
    /// Presentation feedback toggles (hover, spinner, animations, bell).
    pub(super) feedback: super::feedback::ClientFeedbackToggles,
    pub(super) keybinds: LiveKeybindConfig,
    pub(super) local_keys: crate::config::KeysConfig,
    pub(super) keybinding_source: ClientShellKeybindingSource,
    pub(super) prompt_new_tab_name: bool,
    pub(super) prompt_new_workspace_name: bool,
    pub(super) confirm_close: bool,
    pub(super) mouse_capture: bool,
    pub(super) mouse_scroll_lines: usize,
    /// Prefix-mode which-key popup switch resolved from `ui.which_key`.
    pub(super) which_key: bool,
    /// Double/triple-click streak window resolved from `ui.double_click_ms`.
    pub(super) double_click_window: std::time::Duration,
    /// Drag update throttle resolved from `ui.drag_throttle_ms`.
    pub(super) drag_throttle: std::time::Duration,
    /// Selection auto-scroll cadence resolved from `ui.selection_autoscroll_interval_ms`.
    pub(super) selection_autoscroll_interval: std::time::Duration,
    /// Selection auto-scroll edge step resolved from `ui.selection_autoscroll_min_lines`.
    pub(super) selection_autoscroll_min_lines: usize,
    /// Selection auto-scroll far-edge cap resolved from `ui.selection_autoscroll_max_lines`.
    pub(super) selection_autoscroll_max_lines: usize,
    pub(super) right_click_passthrough_modifiers: Option<crossterm::event::KeyModifiers>,
    pub(super) redraw_on_focus_gained: bool,
    pub(super) switch_ascii_input_source_in_prefix: bool,
    pub(super) local_config_path: std::path::PathBuf,
    pub(super) preferences_path: Option<std::path::PathBuf>,
    pub(super) preferences: preferences::ClientChromePreferences,
    pub(super) startup_config_diagnostic: Option<String>,
    pub(super) startup_onboarding: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ClientShellLayout {
    pub sidebar: Rect,
    pub tab_bar: Rect,
    pub mobile_header: Rect,
    pub pane_surface: Rect,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ClientMobileTarget {
    Machine(ClientEndpointId),
    NewWorkspace,
    Workspace {
        endpoint_id: ClientEndpointId,
        workspace_id: String,
    },
    NewTab,
    Tab {
        endpoint_id: ClientEndpointId,
        tab_id: String,
    },
    Agent {
        endpoint_id: ClientEndpointId,
        pane_id: String,
    },
    Menu(usize),
}

#[derive(Default)]
pub(super) struct ShellHitMap {
    /// 本表由一次完整 compose 产生（经典布局或停靠工作台），而不是
    /// `invalidate_pane_surface` / 快照换代 / 未配对 surface 留下的空表。tick 里
    /// 依赖命中区几何做判断（如总览浮层的孤儿防护）只在为真时进行：空表只说明
    /// 还没重绘，不代表锚点丢失。
    pub(super) composed: bool,
    pub(super) overlay_bounds: Rect,
    /// 本帧真正绘制的浮层种类：输入侧据此判断浮层命中区是否属于当前浮层
    /// （浮层刚打开、还没合成时为 `None`，此时才允许回退到几何推导）。
    pub(super) overlay_kind: Option<ClientShellOverlayKind>,
    pub(super) machines: Vec<MachineHit>,
    pub(super) workspaces: Vec<WorkspaceHit>,
    pub(super) workspace_body: Rect,
    pub(super) workspace_scrollbar: Rect,
    pub(super) workspace_scroll_metrics: Option<crate::pane::ScrollMetrics>,
    pub(super) workspace_max_scroll: usize,
    pub(super) tabs: Vec<(Rect, String)>,
    pub(super) panes: Vec<PaneHit>,
    pub(super) popup: Option<PaneHit>,
    pub(super) pane_splits: Vec<PaneSplitHit>,
    pub(super) agents: Vec<(Rect, String)>,
    pub(super) endpoint_agents: Vec<(Rect, ClientEndpointId, String)>,
    pub(super) agent_body: Rect,
    pub(super) agent_scrollbar: Rect,
    pub(super) agent_scroll_metrics: Option<crate::pane::ScrollMetrics>,
    pub(super) agent_max_scroll: usize,
    pub(super) agent_sort_toggle: Rect,
    pub(super) agent_group_toggles: Vec<(Rect, String)>,
    pub(super) agent_usage_toggle: Rect,
    /// 浮动用量仪表盘（非模态 overlay）内的账号行 / 按钮命中区。
    pub(super) usage_dashboard_actions: Vec<(Rect, super::observability::Action)>,
    pub(super) sidebar_divider: Rect,
    pub(super) sidebar_section_divider: Rect,
    pub(super) sidebar_toggle: Rect,
    pub(super) new_workspace: Rect,
    pub(super) new_tab: Rect,
    pub(super) tab_scroll_left: Rect,
    pub(super) tab_scroll_right: Rect,
    pub(super) mobile_switch: Rect,
    pub(super) mobile_close: Rect,
    pub(super) mobile_targets: Vec<(Rect, ClientMobileTarget)>,
    pub(super) mobile_max_scroll: usize,
    pub(super) global_launcher: Rect,
    pub(super) notification_toast: Rect,
    pub(super) menu_popup: Rect,
    pub(super) menu_search: Rect,
    pub(super) menu_scroll: usize,
    pub(super) global_menu_rows: Vec<(Rect, usize)>,
    pub(super) context_menu_rows: Vec<(Rect, usize)>,
    pub(super) notification_history_rows: Vec<(Rect, usize)>,
    pub(super) overlay_primary: Rect,
    pub(super) overlay_clear: Rect,
    pub(super) overlay_cancel: Rect,
    pub(super) navigator_popup: Rect,
    pub(super) navigator_search: Rect,
    pub(super) navigator_rows: Vec<(Rect, ClientNavigatorTarget)>,
    pub(super) worktree_search: Rect,
    pub(super) worktree_rows: Vec<(Rect, usize)>,
    pub(super) help_popup: Rect,
    pub(super) help_scrollbar: Rect,
    pub(super) help_scroll_metrics: Option<crate::pane::ScrollMetrics>,
    pub(super) help_max_scroll: usize,
    pub(super) settings_popup: Rect,
    pub(super) settings_scroll: usize,
    pub(super) settings_tabs: Vec<(Rect, ClientSettingsSection)>,
    pub(super) settings_choices: Vec<(Rect, usize)>,
    pub(super) machines_popup: Rect,
    pub(super) machines_detail_area: Rect,
    pub(super) machines_scroll: usize,
    /// 上一帧是否真的画了某个机器面板列表；见 `OverlayRender`。
    pub(super) machines_scroll_valid: bool,
    pub(super) machines_search: Rect,
    pub(super) machines_rows: Vec<(Rect, crate::client::endpoint::ProfileId)>,
    pub(super) machines_actions: Vec<(Rect, super::machines_overlay::MachineOverlayButton)>,
    pub(super) machines_fields: Vec<(Rect, super::machines_overlay::MachineField)>,
    pub(super) machines_wizard_rows: Vec<(Rect, usize)>,
    pub(super) machines_wizard_fields: Vec<(Rect, usize)>,
    pub(super) machines_max_scroll: usize,
    pub(super) machine_auth_max_scroll: usize,
    pub(super) machine_auth_actions: Vec<(Rect, super::machine_auth_overlay::MachineAuthButton)>,
    pub(super) broadcast_popup: Rect,
    pub(super) broadcast_rows: Vec<(Rect, usize)>,
    pub(super) broadcast_actions: Vec<(Rect, super::broadcast::BroadcastButton)>,
    pub(super) machine_files_popup: Rect,
    pub(super) machine_files_search: Rect,
    pub(super) machine_files_rows: Vec<(Rect, usize)>,
    pub(super) machine_files_actions: Vec<(Rect, super::machine_files_overlay::MachineFilesButton)>,
    pub(super) snippet_popup: Rect,
    pub(super) snippet_search: Rect,
    pub(super) snippet_rows: Vec<(Rect, usize)>,
    pub(super) snippet_fields: Vec<(Rect, usize)>,
    pub(super) snippet_actions: Vec<(Rect, super::snippets_overlay::SnippetOverlayButton)>,
    pub(super) scenes_popup: Rect,
    pub(super) scenes_rows: Vec<(Rect, usize)>,
    pub(super) scenes_fields: Vec<(Rect, usize)>,
    pub(super) scenes_actions: Vec<(Rect, super::scenes_overlay::SceneOverlayButton)>,
    /// Clickable regions of the reconnect lifecycle banner (no overlay open).
    pub(super) lifecycle_banner_retry: Rect,
    pub(super) lifecycle_banner_give_up: Rect,
    pub(super) product_announcement_scrollbar: Rect,
    pub(super) product_announcement_scroll_metrics: Option<crate::pane::ScrollMetrics>,
    pub(super) product_announcement_max_scroll: usize,
    pub(super) release_notes_scrollbar: Rect,
    pub(super) release_notes_scroll_metrics: Option<crate::pane::ScrollMetrics>,
    pub(super) release_notes_max_scroll: usize,
}

#[derive(Clone)]
pub(super) struct PaneHit {
    pub(super) rect: Rect,
    pub(super) inner_rect: Rect,
    pub(super) scrollbar_rect: Option<Rect>,
    pub(super) scroll: Option<crate::pane::ScrollMetrics>,
    pub(super) pane_id: String,
    pub(super) popup: bool,
    pub(super) mouse_reporting: bool,
    pub(super) sgr_pixel_mouse: bool,
    pub(super) pixel_width: u32,
    pub(super) pixel_height: u32,
}

#[derive(Clone)]
pub(super) struct PaneSplitHit {
    pub(super) tab_id: Option<String>,
    pub(super) direction: crate::protocol::PaneSurfaceSplitDirection,
    pub(super) pos: u16,
    pub(super) area: Rect,
    pub(super) hit_rect: Rect,
    pub(super) path: Vec<bool>,
    pub(super) topology_signature: u64,
}

pub(super) struct ClientPaneMouseGesture {
    pub(super) hit: PaneHit,
    pub(super) button: crossterm::event::MouseButton,
    pub(super) stripped_modifiers: crossterm::event::KeyModifiers,
    pub(super) last_event: crossterm::event::MouseEvent,
    pub(super) last_position: crate::protocol::ClientMousePosition,
}

pub(super) struct ClientWorkspacePress {
    pub(super) endpoint_id: ClientEndpointId,
    pub(super) workspace_id: String,
    /// 按下时的行：侧栏是纵向列表，只有纵向位移才可能升级成重排拖拽。
    pub(super) start_row: u16,
}

pub(super) struct ClientTabPress {
    pub(super) tab_id: String,
    pub(super) workspace_id: String,
    /// 按下时的列：标签条是横向列表，只有横向位移才可能升级成重排拖拽。
    pub(super) start_column: u16,
}

pub(super) enum ClientChromeDrag {
    SidebarWidth,
    SidebarSection,
    WorkspaceScrollbar {
        grab_row_offset: u16,
    },
    AgentScrollbar {
        grab_row_offset: u16,
    },
    HelpScrollbar {
        grab_row_offset: u16,
    },
    ProductAnnouncementScrollbar {
        grab_row_offset: u16,
    },
    ReleaseNotesScrollbar {
        grab_row_offset: u16,
    },
    Tab {
        tab_id: String,
        workspace_id: String,
        insert_index: Option<usize>,
    },
    Workspace {
        source_workspace_id: String,
        target: Option<(Option<String>, u16)>,
    },
    PaneSplit {
        hit: PaneSplitHit,
        tab_id: String,
        grab_offset: i32,
        last_sent_ratio: Option<f32>,
        last_sent_at: Option<std::time::Instant>,
    },
    PaneScrollbar {
        hit: PaneHit,
        grab_row_offset: u16,
        last_sent_offset: Option<usize>,
        last_sent_at: Option<std::time::Instant>,
    },
}

pub(super) struct WorkspaceHit {
    pub(super) rect: Rect,
    pub(super) endpoint_id: ClientEndpointId,
    pub(super) workspace_id: String,
    pub(super) indented: bool,
    pub(super) group_toggle: Option<(Rect, String)>,
}

/// Best-effort reconnect progress for one endpoint: how many attempts the
/// current outage has seen and when the next one is estimated to run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ClientReconnectProgress {
    pub(super) attempts: u32,
    pub(super) next_attempt_at: std::time::Instant,
}

#[derive(Debug)]
pub(crate) enum ClientShellAction {
    Endpoint {
        endpoint_id: ClientEndpointId,
        boot_id: String,
        request: Box<crate::api::schema::Request>,
        /// 由用户连续手势（键盘/鼠标切标签）发起：允许与 lane 队尾尚未写出的同类
        /// 手势请求折叠为最新目标。程序发起的聚焦（worktree 创建后的自动聚焦、
        /// 上下文菜单动作前置聚焦）保持 `false`，永不被折叠。
        coalesce: bool,
    },
    ClipboardWrite(Vec<u8>),
    OpenSafeWebUrl(String),
    ActivateEndpoint {
        endpoint_id: ClientEndpointId,
        target: Option<ClientEndpointFocusTarget>,
    },
    /// Ask the supervisor for an immediate reconnect attempt on a saved machine.
    ReconnectEndpoint {
        endpoint_id: ClientEndpointId,
    },
    /// Run one approved known_hosts operation for a machine target on a
    /// worker thread (scan for display, pre-collect after trust, or remove a
    /// stale record).
    MachineHostKeyOp {
        cancel: crate::remote::TaskCancellation,
        ticket: u64,
        op: MachineHostKeyOp,
        profile: Box<SavedSshEndpoint>,
        reviewed: Option<(
            crate::remote::EffectiveHostKeyTarget,
            crate::remote::KnownHostKey,
        )>,
    },
    /// Start an approved interactive authentication attempt for a machine:
    /// ssh runs with the askpass channel attached and its prompts surface as
    /// TUI dialogs. Only ever constructed after explicit user approval.
    StartMachineInteractiveAuth {
        bootstrap: bool,
        pin: Option<(
            crate::remote::EffectiveHostKeyTarget,
            crate::remote::KnownHostKey,
        )>,
        cancel: crate::remote::TaskCancellation,
        ticket: u64,
        profile: Box<SavedSshEndpoint>,
    },
    /// Answer the pending askpass prompt of an interactive auth attempt;
    /// `None` declines it. The answer travels straight to the prompt
    /// responder — it is never logged, persisted, or rendered.
    AnswerMachineAuthPrompt {
        ticket: u64,
        answer: Option<String>,
    },
    /// Cancel a running interactive auth attempt: pending prompts are
    /// declined and late results are ignored.
    CancelMachineInteractiveAuth {
        ticket: u64,
    },
    /// Run the non-interactive remote bootstrap for a wizard-approved machine.
    BootstrapMachine {
        cancel: crate::remote::TaskCancellation,
        ticket: u64,
        target: String,
        session: String,
        options: Option<crate::remote::ProfileSshOptions>,
    },
    /// Fire one API request at a specific (possibly non-active) endpoint. The
    /// snippet runner uses it to fan one run out to several machines without
    /// switching the user's active surface; responses route back to the
    /// pending request by id regardless of which endpoint is active.
    EndpointRequest {
        endpoint_id: ClientEndpointId,
        boot_id: String,
        request: Box<crate::api::schema::Request>,
    },
    /// Run one remote filesystem operation for the machine file browser on a
    /// worker thread; the result returns as `ClientLoopEvent::MachineFs`.
    MachineFsOp {
        cancel: crate::remote::TaskCancellation,
        ticket: u64,
        profile: Box<SavedSshEndpoint>,
        op: MachineFsOp,
    },
    ReplayMouse(Vec<crossterm::event::MouseEvent>),
}

/// One remote filesystem operation of the machine file browser, executed by
/// a worker thread through `remote::RemoteFs`. `message` on the mutating
/// operations is the localized success line shown after the follow-up
/// listing refresh.
#[derive(Debug)]
pub(crate) enum MachineFsOp {
    List {
        path: String,
    },
    Read {
        path: String,
    },
    Download {
        remote: String,
        local: String,
        message: String,
    },
    Upload {
        local: String,
        remote: String,
        message: String,
    },
    Mkdir {
        path: String,
        message: String,
    },
    Rename {
        from: String,
        to: String,
        message: String,
    },
    Delete {
        path: String,
        recursive: bool,
        message: String,
    },
}

/// Worker result of a [`MachineFsOp`].
#[derive(Debug)]
pub(crate) enum MachineFsOutcome {
    Entries {
        entries: Vec<crate::remote::RemoteDirEntry>,
    },
    FileContent {
        content: Vec<u8>,
    },
    /// A mutating operation succeeded; the overlay re-lists the directory.
    Changed {
        message: String,
    },
}

/// known_hosts operation approved from the machine auth dialogs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MachineHostKeyOp {
    /// Read-only scan used to display the presented fingerprints.
    Scan,
    /// Record the scanned keys after the user trusted them.
    Precollect,
    /// Remove every recorded key for the host (host-key-changed recovery).
    Remove,
}

/// Progress of one approved machine-connection recovery worker.
#[derive(Debug)]
pub(crate) enum MachineAuthUpdate {
    InteractiveStep {
        ticket: u64,
        step: crate::remote::SavedSshBootstrapStep,
    },
    HostKeyOpFinished {
        ticket: u64,
        op: MachineHostKeyOp,
        result: Result<MachineHostKeyOutcome, String>,
    },
    InteractiveFinished {
        ticket: u64,
        result: Result<(), String>,
    },
}

/// Result payload of a finished [`MachineHostKeyOp`].
#[derive(Debug)]
pub(crate) enum MachineHostKeyOutcome {
    /// Presented keys as `(key type, SHA256 fingerprint)` pairs.
    Scanned(crate::remote::HostKeyReview),
    /// How many keys were recorded into known_hosts.
    Precollected(usize),
    /// The host's records were removed (or were already absent).
    Removed,
}

/// Progress of one wizard-driven remote bootstrap running on a worker thread.
#[derive(Debug)]
pub(crate) enum MachineBootstrapUpdate {
    Step(crate::remote::SavedSshBootstrapStep),
    Finished(Result<(), String>),
}

#[derive(Default)]
pub(crate) struct ClientShellInput {
    pub detach: bool,
    pub repaint: bool,
    pub resize: bool,
    pub query_host_appearance: bool,
    pub query_host_theme: bool,
    pub requests: Vec<ClientMessage>,
    pub actions: Vec<ClientShellAction>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ClientShellMode {
    Terminal,
    Prefix,
    Navigate,
    Resize,
    Copy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ClientShellOverlayKind {
    Onboarding,
    ProductAnnouncement,
    ReleaseNotes,
    Rename,
    ConfirmClose,
    Help,
    Navigator,
    WorktreeCreate,
    WorktreeOpen,
    WorktreeRemove,
    ContextMenu,
    CommandPalette,
    Settings,
    Machines,
    MachineAuth,
    NotificationHistory,
    Snippets,
    Scenes,
    Broadcast,
    MachineFiles,
    UsageDashboard,
}

#[derive(Debug)]
pub(super) enum ClientRenameTarget {
    NewWorkspace {
        source_workspace_id: Option<String>,
        cwd: Option<String>,
        suggested_name: String,
    },
    Workspace {
        workspace_id: String,
    },
    NewTab {
        workspace_id: String,
        default_name: String,
    },
    Tab {
        tab_id: String,
        auto_name: bool,
        original_name: String,
    },
    Pane {
        pane_id: String,
    },
    Machine {
        profile_id: crate::client::endpoint::ProfileId,
    },
}

#[derive(Debug)]
pub(super) struct ClientRenameOverlay {
    pub(super) title: &'static str,
    pub(super) input: TextEditor,
    pub(super) target: ClientRenameTarget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ClientNavigatorFilter {
    Blocked,
    Working,
    Idle,
    Done,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ClientNavigatorTarget {
    Machine {
        endpoint_id: ClientEndpointId,
    },
    Workspace {
        endpoint_id: ClientEndpointId,
        workspace_id: String,
    },
    Tab {
        endpoint_id: ClientEndpointId,
        tab_id: String,
    },
    Pane {
        endpoint_id: ClientEndpointId,
        pane_id: String,
    },
}

#[derive(Clone, Debug)]
pub(super) struct ClientNavigatorRow {
    pub(super) depth: u8,
    pub(super) label: String,
    pub(super) meta: String,
    pub(super) status: Option<crate::api::schema::AgentStatus>,
    pub(super) stale: bool,
    pub(super) current: bool,
    /// 该行自身命中了当前查询/过滤（而不是因为某个后代命中才被带出来）。
    /// 没有查询也没有过滤时恒为 false，默认选中沿用「第一行」语义。
    pub(super) matched: bool,
    pub(super) target: ClientNavigatorTarget,
}

#[derive(Debug)]
pub(super) struct ClientNavigatorOverlay {
    pub(super) query: TextEditor,
    pub(super) search_focused: bool,
    pub(super) selected: Option<ClientNavigatorTarget>,
    /// 指针悬浮项：只由 `Moved` 改写。`selected` 只由键盘、搜索与点击改写——
    /// 导航浮层的 Enter 会切换 workspace/tab/pane（跨端点时还会激活端点投影），
    /// 指针划过列表就把它改掉是 MENU-01 / UX-04 的同一类问题。存身份而不是
    /// 行号，过滤与展开折叠后天然失效。
    pub(super) hovered: Option<ClientNavigatorTarget>,
    pub(super) scroll: usize,
    pub(super) filter: Option<ClientNavigatorFilter>,
    pub(super) expanded_workspaces: HashSet<(ClientEndpointId, String)>,
}

#[derive(Debug)]
pub(super) struct ClientHelpOverlay {
    pub(super) query: TextEditor,
    pub(super) search_focused: bool,
    pub(super) scroll: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ClientSettingsSection {
    Language,
    Theme,
    Indicators,
    Sound,
    Toast,
    Integrations,
}

impl ClientSettingsSection {
    pub(super) const ALL: &[Self] = &[
        Self::Language,
        Self::Theme,
        Self::Indicators,
        Self::Sound,
        Self::Toast,
        Self::Integrations,
    ];

    pub(super) fn label(self) -> &'static str {
        let t = &crate::i18n::texts().settings;
        match self {
            Self::Language => t.section_language,
            Self::Theme => t.section_theme,
            Self::Indicators => t.section_indicators,
            Self::Sound => t.section_sound,
            Self::Toast => t.section_toasts,
            Self::Integrations => t.section_integrations,
        }
    }
}

#[derive(Debug)]
pub(super) struct ClientSettingsOverlay {
    pub(super) focus: super::page::PageFocus,
    pub(super) current: usize,
    pub(super) scroll: usize,
    pub(super) reveal: bool,
    pub(super) section: ClientSettingsSection,
    pub(super) selected: usize,
    pub(super) original_theme_name: String,
    pub(super) original_palette: Palette,
    pub(super) original_components: crate::app::state::ComponentStyles,
    pub(super) integrations: Vec<crate::api::schema::IntegrationInfo>,
    /// Install report returned by the endpoint; lives until the next install
    /// or list refresh replaces it.
    pub(super) integration_messages: Vec<String>,
    /// One-shot client-side hint (e.g. "selected row needs no install").
    /// Kept apart from `integration_messages` so a hint never wipes the
    /// endpoint's report out from under the reader (TOOL-04).
    pub(super) integration_notice: Option<String>,
    pub(super) loading_integrations: bool,
    pub(super) installing_integrations: bool,
}

#[derive(Debug)]
pub(super) struct ClientWorktreeCreateOverlay {
    pub(super) source_workspace_id: String,
    pub(super) repo_name: String,
    pub(super) branch: TextEditor,
    pub(super) checkout_path: String,
    pub(super) error: Option<String>,
    pub(super) creating: bool,
}

#[derive(Debug, Clone)]
pub(super) struct ClientWorktreeOpenEntry {
    pub(super) path: String,
    pub(super) branch: Option<String>,
    pub(super) is_linked_worktree: bool,
    pub(super) is_detached: bool,
    /// 服务端判定的「目录已缺失但仍在 git 登记」状态；列表显式标出而不是隐藏。
    pub(super) is_prunable: bool,
    pub(super) open_workspace_id: Option<String>,
    pub(super) label: String,
}

impl ClientWorktreeOpenEntry {
    /// 选中后是否发 `worktree.open`：目录已缺失（prunable）的检出无法新开工作区，但它若仍
    /// 在 herdr 中打开着，服务端只做聚焦、不要求目录存在，所以「已打开」优先于「待清理」。
    pub(super) fn can_open(&self) -> bool {
        self.open_workspace_id.is_some() || !self.is_prunable
    }

    pub(super) fn status_label(&self) -> &'static str {
        let sidebar = &crate::i18n::texts().sidebar;
        if self.open_workspace_id.is_some() {
            sidebar.wt_open
        } else if self.is_prunable {
            sidebar.wt_prunable
        } else if self.branch.is_some() {
            ""
        } else if self.is_detached && self.is_linked_worktree {
            sidebar.wt_detached
        } else {
            sidebar.wt_root
        }
    }

    pub(super) fn matches_query(&self, query: &str) -> bool {
        let query = query.trim().to_lowercase();
        query.is_empty()
            || format!(
                "{} {} {} {}",
                self.label,
                self.branch.as_deref().unwrap_or_default(),
                self.path,
                self.status_label()
            )
            .to_lowercase()
            .contains(&query)
    }
}

#[derive(Debug)]
pub(super) struct ClientWorktreeOpenOverlay {
    pub(super) source_workspace_id: String,
    pub(super) entries: Vec<ClientWorktreeOpenEntry>,
    pub(super) selected: usize,
    pub(super) query: TextEditor,
    pub(super) search_focused: bool,
    pub(super) error: Option<String>,
    pub(super) opening: bool,
}

impl ClientWorktreeOpenOverlay {
    pub(super) fn filtered_indices(&self) -> Vec<usize> {
        self.entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| entry.matches_query(&self.query).then_some(index))
            .collect()
    }

    pub(super) fn selected_entry_index(&self) -> Option<usize> {
        let filtered = self.filtered_indices();
        filtered
            .contains(&self.selected)
            .then_some(self.selected)
            .or_else(|| filtered.first().copied())
    }
}

#[derive(Debug)]
pub(super) struct ClientWorktreeRemoveOverlay {
    pub(super) workspace_id: String,
    pub(super) path: String,
    pub(super) error: Option<String>,
    pub(super) removing: bool,
    pub(super) force_confirmation: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ClientContextMenuAction {
    Rename,
    Close,
    NewWorktree,
    OpenWorktree,
    RemoveWorktree,
    ToggleGroup,
    NewTab,
    RenamePane,
    ClearPaneName,
    SwapWithFocusedPane,
    SplitRight,
    SplitDown,
    Zoom,
    ToggleRightClickPassthrough,
    ClosePane,
    ManageMachines,
    RenameMachine,
    EditMachine,
    ReconnectMachine,
    ToggleMachineEnabled,
    RemoveMachine,
    CopyMachineFixCommand,
}

#[derive(Debug)]
pub(super) enum ClientContextMenuTarget {
    Workspace {
        workspace_id: String,
        is_git: bool,
        is_linked_worktree: bool,
        has_worktree_children: bool,
        collapsed: bool,
    },
    Tab {
        tab_id: String,
        workspace_id: String,
    },
    Pane {
        pane_id: String,
        workspace_id: String,
        source_pane_id: Option<String>,
        has_manual_label: bool,
        right_click_passthrough: bool,
    },
    Machine {
        endpoint_id: ClientEndpointId,
        enabled: bool,
        online: bool,
    },
}

#[derive(Debug)]
pub(super) struct ClientContextMenuOverlay {
    pub(super) target: ClientContextMenuTarget,
    pub(super) x: u16,
    pub(super) y: u16,
    /// 键盘选中项：回车激活的就是它，只由键盘与点击改写。
    pub(super) highlighted: usize,
    /// 指针悬浮项：只由 `Moved` 改写，指针离开行区域即回到 None。与
    /// `highlighted` 分离后，「鼠标路过」不再劫持键盘选择（MENU-01）。
    pub(super) hovered: Option<usize>,
}

pub(super) struct ClientContextMenuItem {
    pub(super) label: &'static str,
    pub(super) action: ClientContextMenuAction,
}

#[derive(Debug)]
pub(super) struct ClientConfirmCloseOverlay {
    pub(super) workspace_id: String,
    pub(super) title: String,
    pub(super) detail: String,
}

#[derive(Debug)]
pub(super) enum ClientShellOverlay {
    Onboarding,
    ProductAnnouncement(crate::app::state::ProductAnnouncementState),
    ReleaseNotes(crate::app::state::ReleaseNotesState),
    Rename(ClientRenameOverlay),
    ConfirmClose(ClientConfirmCloseOverlay),
    Help(ClientHelpOverlay),
    Navigator(ClientNavigatorOverlay),
    WorktreeCreate(ClientWorktreeCreateOverlay),
    WorktreeOpen(ClientWorktreeOpenOverlay),
    WorktreeRemove(ClientWorktreeRemoveOverlay),
    ContextMenu(ClientContextMenuOverlay),
    CommandPalette(super::command_palette::ClientCommandPaletteOverlay),
    Settings(ClientSettingsOverlay),
    Machines(super::machines_overlay::ClientMachinesOverlay),
    MachineAuth(super::machine_auth_overlay::ClientMachineAuthOverlay),
    NotificationHistory(super::feedback::ClientNotificationHistoryOverlay),
    Snippets(super::snippets_overlay::ClientSnippetsOverlay),
    Scenes(super::scenes_overlay::ClientScenesOverlay),
    Broadcast(super::broadcast::ClientBroadcastOverlay),
    MachineFiles(super::machine_files_overlay::ClientMachineFilesOverlay),
    UsageDashboard,
}

impl ClientShellOverlay {
    pub(super) fn kind(&self) -> ClientShellOverlayKind {
        match self {
            Self::Onboarding => ClientShellOverlayKind::Onboarding,
            Self::ProductAnnouncement(_) => ClientShellOverlayKind::ProductAnnouncement,
            Self::ReleaseNotes(_) => ClientShellOverlayKind::ReleaseNotes,
            Self::Rename(_) => ClientShellOverlayKind::Rename,
            Self::ConfirmClose(_) => ClientShellOverlayKind::ConfirmClose,
            Self::Help(_) => ClientShellOverlayKind::Help,
            Self::Navigator(_) => ClientShellOverlayKind::Navigator,
            Self::WorktreeCreate(_) => ClientShellOverlayKind::WorktreeCreate,
            Self::WorktreeOpen(_) => ClientShellOverlayKind::WorktreeOpen,
            Self::WorktreeRemove(_) => ClientShellOverlayKind::WorktreeRemove,
            Self::ContextMenu(_) => ClientShellOverlayKind::ContextMenu,
            Self::CommandPalette(_) => ClientShellOverlayKind::CommandPalette,
            Self::Settings(_) => ClientShellOverlayKind::Settings,
            Self::Machines(_) => ClientShellOverlayKind::Machines,
            Self::MachineAuth(_) => ClientShellOverlayKind::MachineAuth,
            Self::NotificationHistory(_) => ClientShellOverlayKind::NotificationHistory,
            Self::Snippets(_) => ClientShellOverlayKind::Snippets,
            Self::Scenes(_) => ClientShellOverlayKind::Scenes,
            Self::Broadcast(_) => ClientShellOverlayKind::Broadcast,
            Self::MachineFiles(_) => ClientShellOverlayKind::MachineFiles,
            Self::UsageDashboard => ClientShellOverlayKind::UsageDashboard,
        }
    }

    /// 浮层内部的步进指纹：同一浮层里每一步破坏性确认或视图切换都有不同取值。
    /// 只取「真实步进」的状态（视图判别式、已武装的强制确认），不取请求在途一类
    /// 的瞬时标志，这样同一步内的长按重复仍按原语义回放。
    pub(super) fn step(&self) -> u32 {
        match self {
            // 普通删除失败后武装的强制删除是独立一步。
            Self::WorktreeRemove(remove) => u32::from(remove.force_confirmation),
            // 片段库的运行流水线：列表 → 目标 → 变量 → 确认，每步各算一步。
            Self::Snippets(overlay) => overlay.view.step(),
            // 现场快照：列表的 Enter 就是破坏性恢复，确认页是同键的下一步。
            Self::Scenes(overlay) => overlay.view.step(),
            // 机器与机器文件：删除确认与列表必须是不同步，否则将来把确认键
            // 改回 Enter 会静默回归成「长按删完」。转发编辑器的删除武装态
            // 同样是破坏性一步，必须与未武装的编辑器区分开。
            Self::Machines(overlay) => match &overlay.view {
                super::machines_overlay::ClientMachinesView::ConfirmRemove(_) => 1,
                super::machines_overlay::ClientMachinesView::Forwards(view)
                    if view.pending_remove.is_some() =>
                {
                    2
                }
                _ => 0,
            },
            Self::MachineFiles(overlay) => u32::from(matches!(
                overlay.view,
                super::machine_files_overlay::ClientMachineFilesView::ConfirmDelete { .. }
            )),
            Self::Onboarding
            | Self::ProductAnnouncement(_)
            | Self::ReleaseNotes(_)
            | Self::Rename(_)
            | Self::ConfirmClose(_)
            | Self::Help(_)
            | Self::Navigator(_)
            | Self::WorktreeCreate(_)
            | Self::WorktreeOpen(_)
            | Self::ContextMenu(_)
            | Self::CommandPalette(_)
            | Self::Settings(_)
            | Self::MachineAuth(_)
            | Self::NotificationHistory(_)
            | Self::Broadcast(_)
            | Self::UsageDashboard => 0,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(super) struct MachineChrome {
    pub(super) group: Option<String>,
    pub(super) color: Option<ratatui::style::Color>,
}

#[derive(Debug)]
pub(super) enum PendingEndpointKind {
    TextCapture {
        epoch: u64,
        endpoint: ClientEndpointId,
    },
    TextWindow {
        epoch: u64,
    },
    TextCopy {
        epoch: u64,
    },
    TextRelease,
    Views {
        revision: u64,
    },
    Observation {
        epoch: u64,
        endpoint_id: ClientEndpointId,
        purpose: super::observability::Purpose,
    },
    Generic,
    ProductAnnouncementDismiss {
        version: String,
        id: String,
    },
    ReleaseNotesDismiss,
    PopupCommand,
    ReloadConfig,
    IntegrationList,
    IntegrationInstall,
    PrepareWorktreeCreate {
        workspace_id: String,
    },
    PrepareWorktreeOpen {
        workspace_id: String,
    },
    PrepareWorktreeRemove {
        workspace_id: String,
    },
    WorktreeCreate,
    WorktreeOpen,
    WorktreeRemove {
        forced: bool,
    },
    SelectionCopy,
    PaneScroll {
        pane_id: String,
        serial: u64,
    },
    WordSelection {
        pane_id: String,
        absolute_row: u32,
        generation: u64,
    },
    PaneLinkResolve {
        target: super::link_hover::LinkHoverTarget,
    },
    PaneLinkActivate {
        pane_id: String,
        inner_rect: Rect,
        fallback_events: Vec<crossterm::event::MouseEvent>,
    },
    CopyMotion {
        pane_id: String,
        origin: crate::api::schema::PaneTextPoint,
        session_generation: u64,
    },
    CopySearch {
        pane_id: String,
        origin: crate::api::schema::PaneTextPoint,
        query: String,
        direction: crate::api::schema::PaneCopySearchDirection,
        repeat: bool,
        generation: u64,
        session_generation: u64,
    },
    /// One target of a snippet run: the pane input request in flight to a
    /// (possibly non-active) endpoint. `machine` is the display/history name.
    SnippetRun {
        machine: String,
        pane_id: String,
    },
    /// One target of a live broadcast fan-out keystroke; resolves like a
    /// snippet run but reports through the throttled broadcast notice.
    BroadcastSend {
        machine: String,
    },
}

pub(super) struct PendingEndpointRequest {
    pub(super) boot_id: String,
    pub(super) method_name: String,
    pub(super) confirmation_workspace_id: Option<String>,
    pub(super) kind: PendingEndpointKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum ClientEndpointNoticeKind {
    Unsupported,
    Rejected,
    Timeout,
    Unavailable,
    /// Client-local success feedback (snippet run summaries, scene actions).
    Success,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct ClientEndpointNoticeKey {
    pub(super) boot_id: String,
    pub(super) kind: ClientEndpointNoticeKind,
    pub(super) code: String,
}

pub(super) struct ClientVisibleEndpointNotice {
    pub(super) key: ClientEndpointNoticeKey,
    pub(super) title: String,
    pub(super) body: String,
    pub(super) deadline: std::time::Instant,
}

pub(crate) struct ClientShellEndpointError {
    pub code: Option<String>,
    pub message: String,
}

pub(crate) enum ClientShellNotificationEffect {
    Sound {
        sound: crate::sound::Sound,
        agent: Option<String>,
    },
    Terminal {
        title: String,
        body: Option<String>,
    },
    System {
        title: String,
        body: Option<String>,
    },
}

pub(super) struct ClientPendingNotification {
    pub(super) endpoint_id: ClientEndpointId,
    pub(super) event: SemanticNotification,
    pub(super) deadline: std::time::Instant,
    pub(super) expires_at: std::time::Instant,
    pub(super) validate_state: bool,
}

pub(super) struct ClientVisibleNotification {
    pub(super) endpoint_id: ClientEndpointId,
    pub(super) event: SemanticNotification,
    pub(super) deadline: std::time::Instant,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ClientInputTarget {
    Pane(String),
    Popup(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ClientInputContext {
    pub(super) mode: ClientShellMode,
    pub(super) overlay: Option<ClientShellOverlayKind>,
    /// 浮层内部的步进指纹（`ClientShellOverlay::step`）：切换 view 或武装
    /// 破坏性确认都会改变它。`overlay` 只有种类粒度，会把「普通删除 → 强制
    /// 删除」「列表 → 运行确认」折叠成同一上下文，使自动重复的 Repeat 能替
    /// 用户按完后面几步；带上步进指纹后 lease 跨步即失效。
    pub(super) overlay_step: u32,
    pub(super) popup_terminal_id: Option<String>,
    pub(super) popup_pending: bool,
    pub(super) retained_selection: bool,
}

type ClientInputLeases = crate::input::InputLeaseTable<u8, ClientInputContext, ClientInputTarget>;

#[derive(Clone, Debug)]
pub(super) struct ClientPaneClick {
    pub(super) pane_id: String,
    pub(super) viewport_row: u16,
    pub(super) col: u16,
    pub(super) at: std::time::Instant,
    /// Position in the click streak: 1 single, 2 double, 3 triple.
    pub(super) streak: u8,
}

impl ClientPaneClick {
    /// Same-spot follow-up click inside the configured streak window.
    pub(super) fn continues_streak(&self, next: &Self, window: std::time::Duration) -> bool {
        self.pane_id == next.pane_id
            && next.at.duration_since(self.at) <= window
            && self.viewport_row.abs_diff(next.viewport_row) <= 1
            && self.col.abs_diff(next.col) <= 1
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ClientSelectionAutoscrollDirection {
    Up,
    Down,
}

#[derive(Clone, Debug)]
pub(super) struct ClientSelectionAutoscroll {
    pub(super) pane_id: String,
    pub(super) direction: ClientSelectionAutoscrollDirection,
    pub(super) last_mouse_column: u16,
    pub(super) last_mouse_row: u16,
    pub(super) inner_rect: Rect,
    pub(super) offset_from_bottom: usize,
    pub(super) max_offset_from_bottom: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ClientCopySelection {
    Character {
        anchor: crate::api::schema::PaneTextPoint,
    },
    Linewise {
        anchor_row: u32,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ClientCopySearchPrompt {
    pub(super) direction: crate::api::schema::PaneCopySearchDirection,
    pub(super) query: TextEditor,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ClientCopyOperation {
    Motion(crate::api::schema::PaneCopyMotion),
    Search {
        query: String,
        direction: crate::api::schema::PaneCopySearchDirection,
        repeat: bool,
    },
}

pub(super) struct ClientCopySearchResult {
    pub(super) content_revision: u64,
    pub(super) matches: Vec<crate::api::schema::PaneTextRange>,
    pub(super) total: u64,
    pub(super) current: Option<usize>,
    pub(super) current_global: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ClientCopyModeState {
    pub(super) pane_id: String,
    pub(super) content_revision: u64,
    pub(super) geometry: (u16, u16),
    pub(super) cursor: crate::api::schema::PaneTextPoint,
    pub(super) offset_from_bottom: usize,
    pub(super) max_offset_from_bottom: usize,
    pub(super) entry_offset_from_bottom: usize,
    pub(super) selection: Option<ClientCopySelection>,
    pub(super) search_prompt: Option<ClientCopySearchPrompt>,
    pub(super) search_query: String,
    pub(super) search_direction: Option<crate::api::schema::PaneCopySearchDirection>,
    pub(super) search_matches: Vec<crate::api::schema::PaneTextRange>,
    pub(super) search_total: u64,
    pub(super) search_current: Option<usize>,
    pub(super) search_current_global: Option<u64>,
    pub(super) search_generation: u64,
    pub(super) copy_after_search: bool,
}

pub(crate) struct ClientShellState {
    pub(crate) endpoint_connect_options: Option<crate::client::endpoint::EndpointConnectOptions>,
    pub(super) workbench: super::workbench::State,
    pub(super) observability: super::observability::State,
    pub(super) config: ClientShellConfig,
    pub(super) snapshot: Option<Box<ClientShellSnapshot>>,
    pub(super) active_snapshot_generation: Option<u64>,
    pub(super) pane_surface_generation: Option<u64>,
    pub(super) pane_surface: Option<PaneSurfaceFrame>,
    /// A future projection surface waits here until its matching snapshot arrives. The visible
    /// pane surface always remains an exact snapshot pair.
    pub(super) pending_pane_surface: Option<PaneSurfaceFrame>,
    pub(super) graphics: crate::kitty_graphics::surface::ClientState,
    pub(super) graphics_cell_size: crate::kitty_graphics::HostCellSize,
    pub(super) popup_terminal_id: Option<String>,
    pub(super) sidebar_collapsed: bool,
    pub(super) sidebar_collapsed_manual: bool,
    pub(super) sidebar_width: u16,
    pub(super) sidebar_width_manual: bool,
    pub(super) sidebar_section_split: f32,
    pub(super) sidebar_section_split_manual: bool,
    pub(super) agent_panel_sort_manual: bool,
    pub(super) last_sidebar_divider_click: Option<std::time::Instant>,
    pub(super) chrome_drag: Option<ClientChromeDrag>,
    pub(super) workspace_press: Option<ClientWorkspacePress>,
    pub(super) tab_press: Option<ClientTabPress>,
    pub(super) collapsed_groups: HashSet<String>,
    pub(super) remote_collapsed_groups: HashMap<ClientEndpointId, HashSet<String>>,
    pub(super) workspace_scroll: usize,
    pub(super) agent_scroll: usize,
    pub(super) tab_scroll: usize,
    pub(super) mobile_switcher_scroll: usize,
    pub(super) reveal_focused_workspace: bool,
    pub(super) reveal_mobile_workspace: bool,
    pub(super) mobile_switcher_suspended: bool,
    pub(super) reveal_focused_tab: bool,
    pub(super) last_tab_bar_width: Option<u16>,
    pub(super) last_composed_size: Option<(u16, u16)>,
    pub(super) last_composed_at: Option<std::time::Instant>,
    pub(super) selection_repaint_deadline: Option<std::time::Instant>,
    pub(super) hits: ShellHitMap,
    pub(super) endpoints: Vec<ClientShellEndpoint>,
    /// Local catalog mirror for the machines overlay and sidebar grouping;
    /// refreshed by `set_endpoint_catalog` on load and every watcher reload.
    pub(super) saved_profiles: Vec<SavedSshEndpoint>,
    /// Per-profile sidebar presentation lookup rebuilt with the catalog, so
    /// per-frame sidebar rendering never scans or reparses profiles.
    pub(super) machine_chrome: HashMap<crate::client::endpoint::ProfileId, MachineChrome>,
    pub(super) next_machine_bootstrap_ticket: u64,
    /// Latest structured connection-failure kind per endpoint, mirrored from
    /// the supervisor so detail views and recovery dialogs react to the kind
    /// instead of parsing message text.
    pub(super) endpoint_connection_errors:
        HashMap<ClientEndpointId, crate::remote::ConnectionErrorKind>,
    /// Latest per-rule port-forward status per endpoint, mirrored from the
    /// supervisor: refreshed on connect/disconnect events and polled at a low
    /// cadence while a machine detail card showing it is open.
    pub(super) endpoint_port_forwards:
        HashMap<ClientEndpointId, Vec<crate::remote::PortForwardStatus>>,
    /// Session-log writer drop counters per profile, mirrored from the
    /// supervisor on the same low cadence as the forward poll (they have no
    /// event of their own). Each machine's detail card reads only its own
    /// entry; zero-count profiles are absent.
    pub(super) session_log_dropped: HashMap<crate::client::endpoint::ProfileId, u64>,
    /// When the detail card polls the supervisor again (forward failures are
    /// recorded by monitor threads without an event).
    pub(super) port_forward_polled_at: Option<std::time::Instant>,
    /// In-flight snippet run: per-target outcomes collect here until every
    /// request resolves, then history is written and the summary toast shows.
    pub(super) snippet_run: Option<super::snippets_overlay::ClientSnippetRunState>,
    /// In-memory mirror of the persisted broadcast target set
    /// (`endpoint::broadcast`): the per-keystroke fan-out reads only this
    /// copy, refreshed on overlay open, after every mutation, and when the
    /// client-level watcher reports an external `broadcast.json` change.
    pub(super) broadcast: crate::client::endpoint::BroadcastSet,
    /// Machine file browser: ticket of the latest issued fs operation and
    /// the id allocator. Results for superseded tickets drop on arrival.
    pub(super) machine_files_ticket: Option<u64>,
    pub(super) next_machine_files_ticket: u64,
    /// Best-effort reconnect progress per endpoint (attempt count and the
    /// estimated next-attempt time) backing the lifecycle banner.
    pub(super) reconnect_progress: HashMap<ClientEndpointId, ClientReconnectProgress>,
    pub(super) next_machine_auth_ticket: u64,
    pub(super) active_endpoint_id: ClientEndpointId,
    pub(super) collapsed_endpoints: HashSet<ClientEndpointId>,
    pub(super) mode: ClientShellMode,
    pub(super) navigate_workspace_id: Option<WorkspaceNavigationTarget>,
    pub(super) reveal_navigation_workspace: bool,
    pub(super) overlay: Option<ClientShellOverlay>,
    pub(super) browser_return: Option<Box<ClientShellOverlay>>,
    pub(super) page_windows: HashMap<String, super::floating_pages::Window>,
    pub(super) page_drag: Option<super::floating_pages::Drag>,
    pub(super) previous_pane_id: Option<String>,
    pub(super) pane_mouse_gesture: Option<ClientPaneMouseGesture>,
    pub(super) link_hover: Option<super::link_hover::LinkHover>,
    /// Active link hints session (two-letter URL markers over the viewport).
    pub(super) link_hints: Option<super::link_hints::ClientLinkHints>,
    pub(super) url_click_consumes_until_up: bool,
    pub(super) replaying_url_click: bool,
    pub(super) selection: Option<crate::selection::Selection<String>>,
    pub(super) selection_capture: Option<super::frozen_selection::Capture>,
    pub(super) selection_epoch: u64,
    pub(super) selection_releases: Vec<super::frozen_selection::Release>,
    pub(super) last_pane_click: Option<ClientPaneClick>,
    pub(super) selection_autoscroll: Option<ClientSelectionAutoscroll>,
    pub(super) selection_autoscroll_deadline: Option<std::time::Instant>,
    pub(super) selection_highlight_clear_deadline: Option<std::time::Instant>,
    pub(super) word_selection_gesture: Option<ClientWordSelection>,
    pub(super) word_selection_generation: u64,
    pub(super) copy_mode: Option<ClientCopyModeState>,
    pub(super) copy_session_generation: u64,
    pub(super) copy_operation_in_flight: bool,
    pub(super) copy_operation_queue: VecDeque<ClientCopyOperation>,
    pub(super) copy_input_queue: VecDeque<crate::input::TerminalKey>,
    pub(super) next_scroll_serial: u64,
    pub(super) pane_scroll_in_flight: HashMap<String, u64>,
    pub(super) pane_scroll_queued: HashMap<String, usize>,
    pub(super) pane_scroll_targets: HashMap<String, usize>,
    pub(super) copy_feedback: Option<crate::app::state::CopyFeedback>,
    pub(super) copy_feedback_deadline: Option<std::time::Instant>,
    pub(super) host_mouse_pixels: Option<crate::input::mouse::HostPixels>,
    pub(super) input_leases: ClientInputLeases,
    pub(super) popup_pending: bool,
    pub(super) popup_pending_deadline: Option<std::time::Instant>,
    pub(super) next_request_id: u64,
    pub(super) pending_requests: HashMap<String, PendingEndpointRequest>,
    pub(super) pending_integration_installs: usize,
    pub(super) pending_notifications: Vec<ClientPendingNotification>,
    pub(super) visible_notification: Option<ClientVisibleNotification>,
    pub(super) queued_notifications: VecDeque<ClientVisibleNotification>,
    pub(super) endpoint_notice_seen: HashSet<ClientEndpointNoticeKey>,
    pub(super) visible_endpoint_notice: Option<ClientVisibleEndpointNotice>,
    /// Recent notification history ring backing the history overlay.
    pub(super) notification_history: VecDeque<super::feedback::ClientNotificationRecord>,
    /// Most-recently-used command palette ids (newest first), persisted to
    /// the client chrome preferences file.
    pub(super) palette_recent: Vec<String>,
    /// 偏好落盘去抖：高频输入（Layout 模式按键）只标脏，由
    /// `tick_chrome_preferences` 在真正静默 500 ms 后合并写一次；detach / 退出
    /// 路径必须 `flush_chrome_preferences`，否则丢最后一次布局变更。
    /// 每次标脏都重置为当前时刻（trailing edge）。
    pub(super) preferences_dirty_since: Option<std::time::Instant>,
    /// 本轮合并批次里第一次标脏的时刻：`PREFERENCES_FLUSH_MAX_DELAY` 的起算点，
    /// 防止「按住方向键不松手」永远等不到静默窗口。
    pub(super) preferences_dirty_first: Option<std::time::Instant>,
    /// 偏好文件的落盘尝试次数（C-13 的验收指标），测试与诊断的可断言真源。
    pub(super) preferences_writes: u64,
    /// Current chrome hover identity (previous frame's hit map).
    pub(super) hover: Option<super::feedback::ChromeHover>,
    pub(super) spinner_tick: u64,
    pub(super) spinner_advanced_at: Option<std::time::Instant>,
    pub(super) visual_bell_deadline: Option<std::time::Instant>,
    /// Entrance-fade clocks: set when an overlay/toast first appears.
    pub(super) overlay_since: Option<std::time::Instant>,
    pub(super) toast_since: Option<std::time::Instant>,
    pub(super) had_toast: bool,
    pub(super) last_overlay_kind: Option<ClientShellOverlayKind>,
    pub(super) outer_focused: Option<bool>,
    pub(super) ascii_input_source_active: bool,
    pub(super) pending_input_source_changes: Vec<bool>,
    pub(super) host_appearance: Option<crate::terminal_theme::HostAppearance>,
    pub(super) host_appearance_explicit: bool,
    pub(super) host_background: Option<crate::terminal_theme::RgbColor>,
    pub(super) local_config_diagnostic: Option<String>,
    pub(super) config_diagnostic: Option<String>,
    pub(super) endpoint_error: Option<String>,
    pub(super) endpoint_error_deadline: Option<std::time::Instant>,
    pub(super) dismissed_product_announcement: Option<(String, String)>,
}

pub(super) fn product_announcement_state(
    announcement: &crate::protocol::ClientShellProductAnnouncement,
) -> crate::app::state::ProductAnnouncementState {
    crate::app::state::ProductAnnouncementState {
        version: announcement.version.clone(),
        id: announcement.id.clone(),
        title: announcement.title.clone(),
        body: announcement.body.clone(),
        scroll: 0,
        preview: announcement.preview,
    }
}

pub(super) fn release_notes_state(
    notes: &crate::protocol::ClientShellReleaseNotes,
) -> crate::app::state::ReleaseNotesState {
    crate::app::state::ReleaseNotesState {
        version: notes.version.clone(),
        body: notes.body.clone(),
        scroll: 0,
        preview: notes.preview,
    }
}

#[derive(Clone, Copy)]
pub(super) struct WorkspaceEntry {
    pub(super) index: usize,
    pub(super) indented: bool,
    pub(super) last_child: bool,
}

impl ClientShellState {
    pub(crate) fn new(mut config: ClientShellConfig) -> Self {
        let workbench = super::workbench::State::new(&config);
        let observability = super::observability::State::new(&config);
        let preferences = config.preferences.clone();
        let local_config_diagnostic = config.startup_config_diagnostic.take();
        let overlay = config
            .startup_onboarding
            .then_some(ClientShellOverlay::Onboarding);
        let sidebar_collapsed = preferences
            .sidebar_collapsed
            .unwrap_or(config.sidebar_start_collapsed);
        let (min_width, max_width) = crate::config::validated_sidebar_bounds(
            config.sidebar_min_width,
            config.sidebar_max_width,
        )
        .unwrap_or((18, 36));
        let sidebar_width = preferences
            .sidebar_width
            .unwrap_or(config.sidebar_width)
            .clamp(min_width, max_width);
        let sidebar_section_split = preferences
            .sidebar_section_split
            .filter(|split| split.is_finite())
            .map(|split| split.clamp(0.1, 0.9))
            .unwrap_or(0.5);
        if let Some(sort) = preferences.agent_panel_sort {
            config.agent_panel_sort = sort;
        }
        let mut remote_collapsed_groups = HashMap::<ClientEndpointId, HashSet<String>>::new();
        for saved in preferences.remote_collapsed_groups {
            let Ok(profile_id) = crate::client::endpoint::ProfileId::parse(saved.profile_id) else {
                continue;
            };
            remote_collapsed_groups
                .entry(ClientEndpointId::Ssh(profile_id))
                .or_default()
                .extend(saved.collapsed_groups);
        }
        Self {
            endpoint_connect_options: None,
            workbench,
            observability,
            config,
            snapshot: None,
            active_snapshot_generation: None,
            pane_surface_generation: None,
            pane_surface: None,
            pending_pane_surface: None,
            graphics: crate::kitty_graphics::surface::ClientState::default(),
            graphics_cell_size: crate::kitty_graphics::HostCellSize {
                width_px: 1,
                height_px: 1,
            },
            popup_terminal_id: None,
            sidebar_collapsed,
            sidebar_collapsed_manual: preferences.sidebar_collapsed.is_some(),
            sidebar_width,
            sidebar_width_manual: preferences.sidebar_width.is_some(),
            sidebar_section_split,
            sidebar_section_split_manual: preferences.sidebar_section_split.is_some(),
            agent_panel_sort_manual: preferences.agent_panel_sort.is_some(),
            last_sidebar_divider_click: None,
            chrome_drag: None,
            workspace_press: None,
            tab_press: None,
            collapsed_groups: preferences.collapsed_groups.into_iter().collect(),
            remote_collapsed_groups,
            workspace_scroll: 0,
            agent_scroll: 0,
            tab_scroll: 0,
            mobile_switcher_scroll: 0,
            reveal_focused_workspace: true,
            reveal_mobile_workspace: false,
            mobile_switcher_suspended: false,
            reveal_focused_tab: true,
            last_tab_bar_width: None,
            last_composed_size: None,
            last_composed_at: None,
            selection_repaint_deadline: None,
            hits: ShellHitMap::default(),
            endpoints: vec![local_endpoint()],
            saved_profiles: Vec::new(),
            machine_chrome: HashMap::new(),
            next_machine_bootstrap_ticket: 1,
            endpoint_connection_errors: HashMap::new(),
            endpoint_port_forwards: HashMap::new(),
            session_log_dropped: HashMap::new(),
            port_forward_polled_at: None,
            snippet_run: None,
            broadcast: crate::client::endpoint::BroadcastSet::load().unwrap_or_default(),
            machine_files_ticket: None,
            next_machine_files_ticket: 1,
            reconnect_progress: HashMap::new(),
            next_machine_auth_ticket: 1,
            active_endpoint_id: ClientEndpointId::Local,
            collapsed_endpoints: HashSet::new(),
            mode: ClientShellMode::Terminal,
            navigate_workspace_id: None,
            reveal_navigation_workspace: false,
            overlay,
            browser_return: None,
            page_windows: preferences.pages.clone(),
            page_drag: None,
            previous_pane_id: None,
            pane_mouse_gesture: None,
            link_hover: None,
            link_hints: None,
            url_click_consumes_until_up: false,
            replaying_url_click: false,
            selection: None,
            selection_capture: None,
            selection_epoch: 0,
            selection_releases: Vec::new(),
            last_pane_click: None,
            selection_autoscroll: None,
            selection_autoscroll_deadline: None,
            selection_highlight_clear_deadline: None,
            word_selection_gesture: None,
            word_selection_generation: 0,
            copy_mode: None,
            copy_session_generation: 0,
            copy_operation_in_flight: false,
            copy_operation_queue: VecDeque::new(),
            copy_input_queue: VecDeque::new(),
            next_scroll_serial: 0,
            pane_scroll_in_flight: HashMap::new(),
            pane_scroll_queued: HashMap::new(),
            pane_scroll_targets: HashMap::new(),
            copy_feedback: None,
            copy_feedback_deadline: None,
            host_mouse_pixels: None,
            input_leases: ClientInputLeases::default(),
            popup_pending: false,
            popup_pending_deadline: None,
            next_request_id: 1,
            pending_requests: HashMap::new(),
            pending_integration_installs: 0,
            pending_notifications: Vec::new(),
            visible_notification: None,
            queued_notifications: VecDeque::new(),
            endpoint_notice_seen: HashSet::new(),
            visible_endpoint_notice: None,
            notification_history: VecDeque::new(),
            palette_recent: preferences
                .palette_recent
                .iter()
                .take(super::command_palette::PALETTE_RECENT_LIMIT)
                .cloned()
                .collect(),
            preferences_dirty_since: None,
            preferences_dirty_first: None,
            preferences_writes: 0,
            hover: None,
            spinner_tick: 0,
            spinner_advanced_at: None,
            visual_bell_deadline: None,
            overlay_since: None,
            toast_since: None,
            had_toast: false,
            last_overlay_kind: None,
            outer_focused: None,
            ascii_input_source_active: false,
            pending_input_source_changes: Vec::new(),
            host_appearance: None,
            host_appearance_explicit: false,
            host_background: None,
            config_diagnostic: local_config_diagnostic.clone(),
            local_config_diagnostic,
            endpoint_error: None,
            endpoint_error_deadline: None,
            dismissed_product_announcement: None,
        }
    }

    pub(super) fn resume_mobile_switcher_if_ready(&mut self) -> bool {
        if !self.mobile_switcher_suspended || self.overlay.is_some() {
            return false;
        }
        self.mobile_switcher_suspended = false;
        if self
            .snapshot
            .as_deref()
            .and_then(|snapshot| snapshot.focused_workspace_id.as_ref())
            .is_some()
        {
            self.mode = self.copy_or_terminal_mode();
            self.navigate_workspace_id = None;
        } else {
            self.mode = ClientShellMode::Navigate;
        }
        true
    }

    pub(super) fn mobile_layout_active(&self) -> bool {
        self.last_composed_size
            .is_some_and(|(cols, rows)| !self.layout(cols, rows).mobile_header.is_empty())
    }

    pub(super) fn collapsed_groups_for_endpoint(
        &self,
        endpoint_id: &ClientEndpointId,
    ) -> Option<&HashSet<String>> {
        if endpoint_id.is_local() {
            Some(&self.collapsed_groups)
        } else {
            self.remote_collapsed_groups.get(endpoint_id)
        }
    }

    pub(super) fn group_is_collapsed(&self, endpoint_id: &ClientEndpointId, key: &str) -> bool {
        self.collapsed_groups_for_endpoint(endpoint_id)
            .is_some_and(|groups| groups.contains(key))
    }

    pub(super) fn toggle_collapsed_group(&mut self, endpoint_id: &ClientEndpointId, key: String) {
        let groups = if endpoint_id.is_local() {
            &mut self.collapsed_groups
        } else {
            self.remote_collapsed_groups
                .entry(endpoint_id.clone())
                .or_default()
        };
        if !groups.remove(&key) {
            groups.insert(key);
        }
    }

    pub(super) fn navigation_workspace_entries(
        &self,
        snapshot: &ClientShellSnapshot,
    ) -> Vec<WorkspaceEntry> {
        let empty_collapsed_groups = HashSet::new();
        if self.mobile_layout_active() {
            render::workspace_entries(snapshot, &empty_collapsed_groups)
        } else {
            render::workspace_entries(
                snapshot,
                self.collapsed_groups_for_endpoint(&self.active_endpoint_id)
                    .unwrap_or(&empty_collapsed_groups),
            )
        }
    }

    pub(super) fn reveal_workspace(&mut self, workspace_id: &str) {
        if self
            .hits
            .workspaces
            .iter()
            .any(|hit| hit.workspace_id == workspace_id)
        {
            return;
        }
        let target = self.snapshot.as_deref().and_then(|snapshot| {
            self.navigation_workspace_entries(snapshot)
                .iter()
                .position(|entry| snapshot.workspaces[entry.index].workspace_id == workspace_id)
        });
        if let Some(target) = target {
            self.workspace_scroll = target.min(self.hits.workspace_max_scroll);
        }
    }

    pub(super) fn layout(&self, cols: u16, rows: u16) -> ClientShellLayout {
        if self.workbench.enabled {
            return self.workbench.layout(cols, rows);
        }
        self.config.layout(
            cols,
            rows,
            self.sidebar_collapsed,
            self.focused_tab_count(),
            self.sidebar_width,
        )
    }

    pub(crate) fn surface_size(&self, cols: u16, rows: u16) -> ClientSurfaceSize {
        let surface = self.layout(cols, rows).pane_surface;
        ClientSurfaceSize {
            cols: surface.width.max(1),
            rows: surface.height.max(1),
        }
    }

    pub(super) fn reset_endpoint_projection(&mut self) {
        self.browser_return = None;
        self.page_drag = None;
        self.cancel_frozen_selection();
        self.workbench.disconnect();
        self.hits = ShellHitMap::default();
        self.pane_surface = None;
        self.pending_pane_surface = None;
        self.input_leases = ClientInputLeases::default();
        self.popup_terminal_id = None;
        self.chrome_drag = None;
        self.workspace_press = None;
        self.tab_press = None;
        self.workspace_scroll = 0;
        self.agent_scroll = 0;
        self.tab_scroll = 0;
        self.mobile_switcher_scroll = 0;
        self.reveal_focused_workspace = true;
        self.reveal_mobile_workspace = false;
        self.mobile_switcher_suspended = false;
        self.reveal_focused_tab = true;
        self.last_tab_bar_width = None;
        self.last_composed_size = None;
        self.last_composed_at = None;
        self.selection_repaint_deadline = None;
        self.pending_requests.retain(|_, pending| {
            matches!(
                pending.kind,
                PendingEndpointKind::TextCapture { .. } | PendingEndpointKind::TextRelease
            )
        });
        self.pane_scroll_in_flight.clear();
        self.pane_scroll_queued.clear();
        self.pane_scroll_targets.clear();
        self.popup_pending = false;
        self.popup_pending_deadline = None;
        self.pending_integration_installs = 0;
        self.endpoint_notice_seen.clear();
        self.visible_endpoint_notice = None;
        self.endpoint_error = None;
        self.endpoint_error_deadline = None;
        self.navigate_workspace_id = None;
        self.overlay = self
            .config
            .startup_onboarding
            .then_some(ClientShellOverlay::Onboarding);
        self.previous_pane_id = None;
        self.pane_mouse_gesture = None;
        self.link_hover = None;
        self.link_hints = None;
        self.url_click_consumes_until_up = false;
        self.replaying_url_click = false;
        self.selection = None;
        self.last_pane_click = None;
        self.selection_autoscroll = None;
        self.selection_autoscroll_deadline = None;
        self.selection_highlight_clear_deadline = None;
        self.word_selection_gesture = None;
        self.copy_mode = None;
        if self.mode == ClientShellMode::Copy {
            self.mode = ClientShellMode::Terminal;
        }
        self.reset_copy_pipeline();
        self.copy_feedback = None;
        self.copy_feedback_deadline = None;
        self.host_mouse_pixels = None;
        self.dismissed_product_announcement = None;
    }

    pub(super) fn apply_active_snapshot(
        &mut self,
        mut snapshot: Box<ClientShellSnapshot>,
        generation: Option<u64>,
    ) {
        snapshot
            .commands
            .retain(|command| command.action != crate::protocol::ClientShellCommandAction::Unknown);
        let graphics_scope = match &self.active_endpoint_id {
            // Local direct uploads use image IDs authored by the server from its boot ID.
            ClientEndpointId::Local => snapshot.boot_id.clone(),
            endpoint_id => format!("{}:{}", endpoint_id.storage_key(), snapshot.boot_id),
        };
        let endpoint_boot_changed =
            self.snapshot.is_some() && self.graphics.scope() != graphics_scope;
        let generation_changed = self.active_snapshot_generation != generation;
        if !endpoint_boot_changed
            && !generation_changed
            && self.snapshot.as_ref().is_some_and(|current| {
                current.boot_id == snapshot.boot_id && snapshot.revision < current.revision
            })
        {
            return;
        }
        // Screen revisions restart per connection. Keep the displayed surface for selection
        // content comparisons, but retire speculative frames from the old connection.
        if generation_changed {
            self.pending_pane_surface = None;
        }
        self.active_snapshot_generation = generation;
        self.graphics.set_scope(&graphics_scope);
        let command_bindings_changed = self.snapshot.as_ref().is_none_or(|current| {
            current.commands.len() != snapshot.commands.len()
                || current
                    .commands
                    .iter()
                    .zip(&snapshot.commands)
                    .any(|(left, right)| {
                        left.binding_labels != right.binding_labels || left.action != right.action
                    })
        });
        let endpoint_profile_changed = self.snapshot.as_ref().is_none_or(|current| {
            current.server_keybindings_toml != snapshot.server_keybindings_toml
        });
        let snapshot_keybindings_changed = match self.config.keybinding_source {
            ClientShellKeybindingSource::Local => self
                .snapshot
                .as_ref()
                .is_none_or(|current| current.commands != snapshot.commands),
            ClientShellKeybindingSource::Endpoint => {
                endpoint_profile_changed
                    || self
                        .snapshot
                        .as_ref()
                        .is_none_or(|current| current.commands != snapshot.commands)
            }
            ClientShellKeybindingSource::RemoteLocal => false,
        };
        let active_keymap_changed = match self.config.keybinding_source {
            ClientShellKeybindingSource::Local => command_bindings_changed,
            ClientShellKeybindingSource::Endpoint => {
                endpoint_profile_changed || command_bindings_changed
            }
            ClientShellKeybindingSource::RemoteLocal => false,
        };
        self.config_diagnostic = super::config::merged_config_diagnostic(
            self.local_config_diagnostic.as_deref(),
            snapshot.config_diagnostic.as_deref(),
        );
        let boot_changed = endpoint_boot_changed
            || self
                .snapshot
                .as_ref()
                .is_some_and(|current| current.boot_id != snapshot.boot_id);
        if boot_changed
            || self
                .pane_surface
                .as_ref()
                .is_none_or(|surface| surface.projection_revision != snapshot.revision)
        {
            self.hits = ShellHitMap::default();
        }
        if boot_changed {
            // A reboot must not turn Enter on a stale preview into focus on a reused ID.
            let preview = (self.mode == ClientShellMode::Navigate)
                .then(|| self.navigate_workspace_id.take())
                .flatten();
            self.reset_endpoint_projection();
            self.navigate_workspace_id = preview;
        } else if let Some(previous) = self
            .snapshot
            .as_deref()
            .and_then(|current| current.focused_pane_id.as_ref())
            .filter(|previous| Some(previous.as_str()) != snapshot.focused_pane_id.as_deref())
        {
            self.previous_pane_id = Some(previous.clone());
        }
        if snapshot_keybindings_changed {
            if let Err(err) = self.config.apply_snapshot_keybindings(
                snapshot.server_keybindings_toml.as_deref(),
                &snapshot.commands,
            ) {
                self.set_endpoint_error(err);
            } else if active_keymap_changed
                && matches!(
                    self.mode,
                    ClientShellMode::Prefix | ClientShellMode::Navigate | ClientShellMode::Resize
                )
            {
                self.mode = ClientShellMode::Terminal;
            }
        }
        let tab_layout_changed = self.snapshot.as_deref().is_none_or(|current| {
            current.tabs.len() != snapshot.tabs.len()
                || current
                    .tabs
                    .iter()
                    .zip(&snapshot.tabs)
                    .any(|(left, right)| {
                        left.tab_id != right.tab_id
                            || left.workspace_id != right.workspace_id
                            || left.label != right.label
                            || left.zoomed != right.zoomed
                    })
                || render::tab_bar_status_width(current) != render::tab_bar_status_width(&snapshot)
        });
        if self
            .snapshot
            .as_deref()
            .and_then(|current| current.focused_workspace_id.as_deref())
            != snapshot.focused_workspace_id.as_deref()
        {
            self.reveal_focused_workspace = true;
        }
        if tab_layout_changed
            || self
                .snapshot
                .as_deref()
                .and_then(|current| current.focused_tab_id.as_deref())
                != snapshot.focused_tab_id.as_deref()
        {
            self.reveal_focused_tab = true;
        }
        let selection_focus_lost = if let Some(capture) = self.selection_capture.as_mut() {
            let focused = snapshot.focused_pane_id.as_deref();
            capture.focus_confirmed |= focused == Some(capture.hit.pane_id.as_str());
            !snapshot
                .panes
                .iter()
                .any(|pane| pane.pane_id == capture.hit.pane_id)
                || (capture.focus_confirmed
                    && focused.is_some_and(|pane| pane != capture.hit.pane_id))
        } else if let Some(gesture) = self.word_selection_gesture.as_mut() {
            let focused_pane = snapshot.focused_pane_id.as_deref();
            // Remember confirmed focus across intermediate snapshots with no
            // focused pane, without rejecting the gesture's in-flight focus request.
            gesture.focus_confirmed |= focused_pane == Some(gesture.pane_id.as_str());
            !snapshot
                .panes
                .iter()
                .any(|pane| pane.pane_id == gesture.pane_id)
                || (gesture.focus_confirmed
                    && focused_pane.is_some_and(|pane_id| pane_id != gesture.pane_id))
        } else {
            self.selection.as_ref().is_some_and(|selection| {
                snapshot.focused_pane_id.as_deref() != Some(selection.pane_id.as_str())
                    || !snapshot
                        .panes
                        .iter()
                        .any(|pane| pane.pane_id == selection.pane_id)
            })
        };
        if selection_focus_lost {
            self.cancel_frozen_selection();
            self.selection = None;
            self.selection_autoscroll = None;
            self.selection_autoscroll_deadline = None;
            self.selection_highlight_clear_deadline = None;
            self.word_selection_gesture = None;
            self.last_pane_click = None;
        }
        if let Some(copy_pane_id) = self
            .copy_mode
            .as_ref()
            .map(|copy_mode| copy_mode.pane_id.clone())
        {
            let pane_exists = snapshot
                .panes
                .iter()
                .any(|pane| pane.pane_id == copy_pane_id);
            let pane_focused = snapshot.focused_pane_id.as_deref() == Some(copy_pane_id.as_str());
            if !pane_exists {
                self.copy_mode = None;
                self.reset_copy_pipeline();
                if self
                    .selection
                    .as_ref()
                    .is_some_and(|selection| selection.pane_id == copy_pane_id)
                {
                    self.selection = None;
                    self.stop_selection_autoscroll();
                    self.selection_highlight_clear_deadline = None;
                }
                if self.mode == ClientShellMode::Copy {
                    self.mode = ClientShellMode::Terminal;
                }
            } else if pane_focused {
                if self.mode == ClientShellMode::Terminal {
                    self.mode = ClientShellMode::Copy;
                }
                if self.selection.is_none() {
                    self.sync_copy_selection();
                }
            } else {
                if self
                    .selection
                    .as_ref()
                    .is_some_and(|selection| selection.pane_id == copy_pane_id)
                {
                    self.selection = None;
                    self.stop_selection_autoscroll();
                    self.selection_highlight_clear_deadline = None;
                }
                if self.mode == ClientShellMode::Copy {
                    self.mode = ClientShellMode::Terminal;
                }
            }
        }
        if self.mode == ClientShellMode::Navigate && self.navigate_workspace_id.is_none() {
            self.navigate_workspace_id = snapshot
                .focused_workspace_id
                .as_deref()
                .and_then(|id| self.navigation_target(&self.active_endpoint_id, id));
            self.reveal_mobile_workspace = self.mobile_layout_active();
        }
        let pane_exists =
            |pane_id: &String| snapshot.panes.iter().any(|pane| &pane.pane_id == pane_id);
        self.pane_scroll_in_flight
            .retain(|pane_id, _| pane_exists(pane_id));
        self.pane_scroll_queued
            .retain(|pane_id, _| pane_exists(pane_id));
        self.pane_scroll_targets
            .retain(|pane_id, _| pane_exists(pane_id));

        if !self.config.startup_onboarding {
            match snapshot.product_announcement.as_ref() {
                Some(announcement) => {
                    let key = (announcement.version.clone(), announcement.id.clone());
                    let already_open = matches!(
                        self.overlay.as_ref(),
                        Some(ClientShellOverlay::ProductAnnouncement(current))
                            if current.version == announcement.version && current.id == announcement.id
                    );
                    let may_open = self.overlay.is_none()
                        || matches!(
                            self.overlay.as_ref(),
                            Some(ClientShellOverlay::ProductAnnouncement(_))
                        );
                    if self.dismissed_product_announcement.as_ref() != Some(&key)
                        && may_open
                        && !already_open
                    {
                        self.overlay = Some(ClientShellOverlay::ProductAnnouncement(
                            product_announcement_state(announcement),
                        ));
                    }
                }
                None if matches!(
                    self.overlay.as_ref(),
                    Some(ClientShellOverlay::ProductAnnouncement(_))
                ) =>
                {
                    self.overlay = None;
                    self.chrome_drag = None;
                    self.dismissed_product_announcement = None;
                }
                None => {
                    self.dismissed_product_announcement = None;
                }
            }
        }
        if let Some(ClientShellOverlay::ReleaseNotes(current)) = self.overlay.as_ref() {
            match snapshot.release_notes.as_ref() {
                Some(notes)
                    if current.version != notes.version
                        || current.body != notes.body
                        || current.preview != notes.preview =>
                {
                    self.overlay =
                        Some(ClientShellOverlay::ReleaseNotes(release_notes_state(notes)));
                    self.chrome_drag = None;
                }
                None => {
                    self.overlay = None;
                    self.chrome_drag = None;
                }
                Some(_) => {}
            }
        }
        self.snapshot = Some(snapshot);
        let pending_surface = self.pending_pane_surface.take();
        if let Some(surface) = pending_surface {
            let matching = self.snapshot.as_ref().is_some_and(|snapshot| {
                surface.boot_id == snapshot.boot_id
                    && surface.projection_revision == snapshot.revision
            });
            if matching {
                self.install_pane_surface(surface, false);
            } else if self.snapshot.as_ref().is_some_and(|snapshot| {
                surface.boot_id == snapshot.boot_id
                    && surface.projection_revision > snapshot.revision
            }) {
                self.pending_pane_surface = Some(surface);
            }
        }
        self.resume_mobile_switcher_if_ready();
        self.reconcile_input_source();
    }

    pub(crate) fn has_presented_surface(&self) -> bool {
        self.pane_surface.is_some()
    }

    pub(crate) fn set_pane_surface(&mut self, surface: PaneSurfaceFrame) {
        let Some(snapshot) = self.snapshot.as_ref() else {
            return;
        };
        if surface.boot_id != snapshot.boot_id || surface.projection_revision < snapshot.revision {
            return;
        }
        if self.pane_surface.as_ref().is_some_and(|current| {
            self.pane_surface_generation == self.active_snapshot_generation
                && current.boot_id == surface.boot_id
                && (surface.projection_revision < current.projection_revision
                    || (surface.projection_revision == current.projection_revision
                        && surface.surface_revision < current.surface_revision))
        }) {
            return;
        }
        if surface.projection_revision == snapshot.revision.saturating_add(1) {
            // The next expected surface waits separately for its exact snapshot. Keeping the
            // current pair avoids treating this speculative successor as presentation evidence.
            self.pending_pane_surface = Some(surface);
            self.hits = ShellHitMap::default();
            return;
        }
        // A surface that skips one or more revisions supersedes any retained pair, but is still
        // not rendered until its matching snapshot arrives. Retain it monotonically so delayed
        // intermediate surfaces cannot replace it.
        self.install_pane_surface(surface, true);
    }

    fn install_pane_surface(&mut self, mut surface: PaneSurfaceFrame, retain_future: bool) {
        let Some(snapshot) = self.snapshot.as_ref() else {
            return;
        };
        if surface.boot_id != snapshot.boot_id
            || surface.projection_revision < snapshot.revision
            || (!retain_future && surface.projection_revision != snapshot.revision)
            || self.pane_surface.as_ref().is_some_and(|current| {
                self.pane_surface_generation == self.active_snapshot_generation
                    && current.boot_id == surface.boot_id
                    && (surface.projection_revision < current.projection_revision
                        || (surface.projection_revision == current.projection_revision
                            && surface.surface_revision < current.surface_revision))
            })
        {
            return;
        }
        // A retained future surface is not presentable yet. Clear hit targets immediately; the
        // exact-pair compose guard prevents it from replacing the visible frame.
        if surface.projection_revision != snapshot.revision {
            self.hits = ShellHitMap::default();
        }
        self.acknowledge_active_surface_agents(&surface);
        let previous_popup = self.popup_terminal_id.clone();
        let next_popup = surface
            .popup
            .as_deref()
            .map(|popup| popup.terminal_id.clone());
        if previous_popup != next_popup {
            self.cancel_frozen_selection();
            if next_popup.is_some() && matches!(self.overlay, Some(ClientShellOverlay::Settings(_)))
            {
                self.cancel_settings_overlay();
            }
            if let Some(terminal_id) = previous_popup.as_ref() {
                self.input_leases
                    .remove_target(&ClientInputTarget::Popup(terminal_id.clone()));
            }
            self.mode = ClientShellMode::Terminal;
            self.navigate_workspace_id = None;
            if !matches!(
                self.overlay.as_ref(),
                Some(ClientShellOverlay::Onboarding | ClientShellOverlay::ProductAnnouncement(_))
            ) {
                self.overlay = self
                    .config
                    .startup_onboarding
                    .then_some(ClientShellOverlay::Onboarding);
            }
            self.selection = None;
            self.last_pane_click = None;
            self.selection_autoscroll = None;
            self.selection_autoscroll_deadline = None;
            self.selection_highlight_clear_deadline = None;
            self.word_selection_gesture = None;
            self.copy_mode = None;
            self.reset_copy_pipeline();
            self.chrome_drag = None;
            self.workspace_press = None;
            self.tab_press = None;
            if self.pane_mouse_gesture.as_ref().is_some_and(|gesture| {
                gesture.hit.popup && previous_popup.as_deref() == Some(gesture.hit.pane_id.as_str())
            }) {
                self.pane_mouse_gesture = None;
            }
            self.hits.popup = None;
            self.endpoint_error = None;
            self.endpoint_error_deadline = None;
        }
        if next_popup.is_some() {
            self.popup_pending = false;
            self.popup_pending_deadline = None;
        }
        let selection_pane = match &self.word_selection_gesture {
            Some(gesture) => Some(&gesture.pane_id),
            None => self.selection.as_ref().map(|selection| &selection.pane_id),
        };
        let selection_content_changed = self.selection_capture.is_none()
            && selection_pane.is_some_and(|pane_id| {
                let Some(previous_surface) = self.pane_surface.as_ref() else {
                    return false;
                };
                let previous = previous_surface
                    .panes
                    .iter()
                    .find(|pane| &pane.pane_id == pane_id);
                let next = surface.panes.iter().find(|pane| &pane.pane_id == pane_id);
                let (Some(previous), Some(next)) = (previous, next) else {
                    return false;
                };
                if previous.inner_rect.width != next.inner_rect.width
                    || previous.inner_rect.height != next.inner_rect.height
                    || previous.alternate_screen_active != next.alternate_screen_active
                {
                    return true;
                }
                if previous.content_revision == next.content_revision {
                    return false;
                }
                match (&self.word_selection_gesture, &self.selection) {
                    // Word gestures cache boundaries outside the selected cells too.
                    (Some(_), _) => true,
                    (None, Some(selection)) => {
                        self.config.copy_on_select
                            && (!previous.content_revision.is_multiple_of(2)
                                || !next.content_revision.is_multiple_of(2)
                                || !selection_cells_unchanged(
                                    selection,
                                    previous_surface,
                                    previous,
                                    &surface,
                                    next,
                                ))
                    }
                    (None, None) => false,
                }
            });
        if selection_content_changed {
            self.word_selection_gesture = None;
            self.selection = None;
            self.stop_selection_autoscroll();
            self.selection_highlight_clear_deadline = None;
        }
        for pane in &surface.panes {
            let Some(target) = self.pane_scroll_targets.get(&pane.pane_id).copied() else {
                continue;
            };
            let Some(scroll) = pane.scroll else {
                continue;
            };
            let target =
                target.min(usize::try_from(scroll.max_offset_from_bottom).unwrap_or(usize::MAX));
            if usize::try_from(scroll.offset_from_bottom).unwrap_or(usize::MAX) == target {
                self.pane_scroll_targets.remove(&pane.pane_id);
            }
        }
        let mut invalidated_copy_pane = None;
        if let Some(copy_mode) = self.copy_mode.as_mut() {
            if let Some(pane) = surface
                .panes
                .iter()
                .find(|pane| pane.pane_id == copy_mode.pane_id)
            {
                let geometry = (pane.inner_rect.width, pane.inner_rect.height);
                if copy_mode.content_revision != pane.content_revision
                    || copy_mode.geometry != geometry
                {
                    copy_mode.content_revision = pane.content_revision;
                    copy_mode.geometry = geometry;
                    copy_mode.selection = None;
                    copy_mode.search_matches.clear();
                    copy_mode.search_total = 0;
                    copy_mode.search_current = None;
                    copy_mode.search_current_global = None;
                    copy_mode.search_generation = copy_mode.search_generation.saturating_add(1);
                    copy_mode.copy_after_search = false;
                    invalidated_copy_pane = Some(copy_mode.pane_id.clone());
                }
                if let Some(scroll) = pane.scroll {
                    let actual_offset =
                        usize::try_from(scroll.offset_from_bottom).unwrap_or(usize::MAX);
                    if !self.pane_scroll_targets.contains_key(&pane.pane_id) {
                        copy_mode.offset_from_bottom = actual_offset;
                    }
                    copy_mode.max_offset_from_bottom =
                        usize::try_from(scroll.max_offset_from_bottom).unwrap_or(usize::MAX);
                }
            }
        }
        if invalidated_copy_pane.as_ref().is_some_and(|pane_id| {
            self.selection
                .as_ref()
                .is_some_and(|selection| &selection.pane_id == pane_id)
        }) {
            self.selection = None;
            self.stop_selection_autoscroll();
            self.selection_highlight_clear_deadline = None;
        }
        self.popup_terminal_id = next_popup;
        self.graphics
            .set_scene(std::mem::take(&mut surface.graphics));
        self.pane_surface = Some(surface);
        self.pane_surface_generation = self.active_snapshot_generation;
        self.invalidate_link_hover();
        // Hint markers index into the old frame; a fresh surface moves them.
        self.link_hints = None;
        self.resume_mobile_switcher_if_ready();
        self.reconcile_input_source();
    }

    pub(crate) fn tick_popup_pending(&mut self, now: std::time::Instant) {
        if self
            .popup_pending_deadline
            .is_some_and(|deadline| now >= deadline)
        {
            self.popup_pending = false;
            self.popup_pending_deadline = None;
        }
    }

    pub(crate) fn show_copy_feedback(&mut self, now: std::time::Instant) -> bool {
        if !self.config.clipboard_toast_enabled {
            return false;
        }
        let message = crate::i18n::texts().chrome.copied.to_owned();
        self.record_notification(
            super::feedback::ClientToastLevel::Success,
            message.clone(),
            None,
            None,
        );
        self.copy_feedback = Some(crate::app::state::CopyFeedback { message });
        self.copy_feedback_deadline = Some(now + std::time::Duration::from_secs(2));
        true
    }

    pub(crate) fn tick_copy_feedback(&mut self, now: std::time::Instant) -> bool {
        let mut repaint = false;
        if self
            .copy_feedback_deadline
            .is_some_and(|deadline| now >= deadline)
        {
            self.copy_feedback = None;
            self.copy_feedback_deadline = None;
            repaint = true;
        }
        if self
            .selection_highlight_clear_deadline
            .is_some_and(|deadline| now >= deadline)
        {
            self.selection = None;
            self.selection_highlight_clear_deadline = None;
            repaint = true;
        }
        repaint
    }

    /// Show a transient client-side action error, restarting its lifetime.
    ///
    /// Every assignment must go through this setter so a repeated identical
    /// message gets a fresh deadline instead of inheriting the previous one.
    pub(super) fn set_endpoint_error(&mut self, message: impl Into<String>) {
        let message = message.into();
        self.record_notification(
            super::feedback::ClientToastLevel::Error,
            message.clone(),
            None,
            None,
        );
        self.endpoint_error = Some(message);
        self.endpoint_error_deadline = Some(
            std::time::Instant::now() + std::time::Duration::from_secs(ENDPOINT_ERROR_TIMEOUT_SECS),
        );
    }

    pub(crate) fn tick_endpoint_error(&mut self, now: std::time::Instant) -> bool {
        if self.endpoint_error.is_none() {
            self.endpoint_error_deadline = None;
            return false;
        }
        if self
            .endpoint_error_deadline
            .is_some_and(|deadline| now >= deadline)
        {
            self.endpoint_error = None;
            self.endpoint_error_deadline = None;
            return true;
        }
        false
    }

    pub(crate) fn timer_delay(&self, now: std::time::Instant) -> std::time::Duration {
        let default = std::time::Duration::from_millis(100);
        self.selection_autoscroll_deadline
            .into_iter()
            .chain(self.selection_repaint_deadline)
            .chain(self.chrome_feedback_deadline())
            .chain(self.preferences_flush_deadline())
            .min()
            .map(|deadline| deadline.saturating_duration_since(now).min(default))
            .unwrap_or(default)
    }

    pub(crate) fn invalidate_pane_surface(&mut self) {
        self.pane_surface = None;
        self.pending_pane_surface = None;
        self.hits = ShellHitMap::default();
        self.host_mouse_pixels = None;
    }
}
