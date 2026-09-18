//! 监控与账号界面的客户端状态；网络和设备访问均通过公开 API。

mod render;
use super::*;
use crate::api::schema::*;
use crate::config::{AccountUsageConfig, MonitorConfig, UsageDisplayFormat, UsageDisplayPosition};
use crossterm::event::{KeyCode, MouseButton, MouseEvent, MouseEventKind};
use std::time::{Duration, Instant};

pub(super) fn tr(en: &'static str, zh: &'static str) -> &'static str {
    if crate::i18n::lang().as_str().starts_with("zh") {
        zh
    } else {
        en
    }
}

/// Whether the accounts surface lists a provider: hidden only when the
/// server explicitly reports the CLI missing and no account is explicitly
/// configured for it. `None` (older server) counts as unknown and stays
/// listed per the generation-1 endpoint contract.
pub(super) fn provider_listed(provider: &UsageProviderInfo) -> bool {
    provider.installed != Some(false) || !provider.configured_accounts.is_empty()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Page {
    Monitor,
    Accounts,
    Settings,
}

#[derive(Clone, Debug)]
pub(super) enum Purpose {
    Metrics,
    Providers,
    Usage,
    Binding,
    Integration,
    Process,
    Terminate,
}

impl Purpose {
    fn key(&self) -> &'static str {
        match self {
            Self::Metrics => "metrics",
            Self::Providers => "providers",
            Self::Usage => "usage",
            Self::Binding => "binding",
            Self::Integration => "integration",
            Self::Process => "process",
            Self::Terminate => "terminate",
        }
    }
}

#[derive(Clone, Debug)]
pub(super) enum Action {
    Page(Page),
    Close,
    Pause,
    Configure,
    Refresh,
    ToggleAlerts,
    Interval,
    Metric(String),
    UsageEnabled,
    UsageFormat,
    UsagePosition,
    Provider(String),
    Account(String),
    CycleAccount,
    Bind,
    Source,
    UsageIntegration(bool),
    Process(ProcessIdentity),
    CancelProcess,
    Terminate(bool),
    ConfirmProcess,
    SortProcesses,
    FilterProcesses,
    Card(String),
    Core(usize),
    CardMove(String, isize),
    CardSize,
    HistoryRange,
    ProviderEnabled(String),
    Device(String),
    AlertThreshold(usize),
    AlertDuration(usize),
    AlertCooldown(usize),
}

#[derive(Clone)]
pub(super) struct HistoryPoint {
    pub at: u64,
    pub cpu: Option<f32>,
    pub memory: Option<f32>,
    pub cores: Vec<Option<f32>>,
}

pub(super) struct Hover {
    pub endpoint_id: ClientEndpointId,
    pub pane: String,
    pub agent: String,
    pub anchor: Rect,
    pub since: Instant,
    pub visible: bool,
    pub leave_at: Option<Instant>,
}

pub(super) struct ProcessDialog {
    pub process: ProcessMetric,
    pub force: bool,
    pub confirm: bool,
    pub pending: bool,
}

#[derive(Default)]
struct AlertState {
    since: Option<u64>,
    last_fired: Option<u64>,
    armed: bool,
}

pub(super) struct State {
    pub page: Option<Page>,
    pub monitor: MonitorConfig,
    pub usage: AccountUsageConfig,
    pub metrics: Option<Box<SystemMetricsSnapshot>>,
    pub history: VecDeque<HistoryPoint>,
    pub providers: Vec<UsageProviderInfo>,
    pub accounts: Vec<AccountUsageSnapshot>,
    pub selected_provider: Option<String>,
    pub selected_account: Option<String>,
    pub selected_pane: Option<String>,
    pub usage_endpoint: Option<ClientEndpointId>,
    pub hover: Option<Hover>,
    pub hover_rect: Rect,
    pub hover_hits: Vec<(Rect, Action)>,
    pub page_rect: Rect,
    pub hits: Vec<(Rect, Action)>,
    pub selected_hit: usize,
    pub selected_card: Option<String>,
    pub selected_core: Option<usize>,
    pub card_scroll: HashMap<String, usize>,
    pub account_scroll: usize,
    pub scroll: usize,
    pub paused: bool,
    pub process_dialog: Option<ProcessDialog>,
    pub process_filter: String,
    pub filtering_processes: bool,
    pub process_sort: ProcessSort,
    pub message: Option<String>,
    pub now_ms: u64,
    pub epoch: u64,
    source: String,
    usage_source: String,
    next_metrics: Instant,
    next_usage: Instant,
    refresh_usage: bool,
    pending: HashSet<&'static str>,
    alerts: HashMap<String, AlertState>,
}

impl State {
    fn scroll_accounts(&mut self, delta: isize) {
        self.account_scroll = self
            .account_scroll
            .saturating_add_signed(delta)
            .min(render::account_rows(self).saturating_sub(1));
    }

    pub(super) fn reload_preferences(&mut self, config: &ClientShellConfig) {
        self.monitor = config
            .preferences
            .monitor
            .clone()
            .unwrap_or_else(|| config.monitor.clone());
        self.usage = config.account_usage.clone();
        if let Some(value) = config.preferences.usage_enabled {
            self.usage.enabled = value;
        }
        if let Some(value) = config.preferences.usage_format {
            self.usage.format = value;
        }
        if let Some(value) = config.preferences.usage_position {
            self.usage.position = value;
        }
        if let Some(value) = &config.preferences.usage_disabled_providers {
            self.usage.disabled_providers.clone_from(value);
        }
        self.next_metrics = Instant::now();
        self.next_usage = Instant::now();
    }
    pub fn new(config: &ClientShellConfig) -> Self {
        let mut usage = config.account_usage.clone();
        if let Some(value) = config.preferences.usage_enabled {
            usage.enabled = value;
        }
        if let Some(value) = config.preferences.usage_format {
            usage.format = value;
        }
        if let Some(value) = config.preferences.usage_position {
            usage.position = value;
        }
        if let Some(value) = &config.preferences.usage_disabled_providers {
            usage.disabled_providers.clone_from(value);
        }
        Self {
            page: None,
            monitor: config
                .preferences
                .monitor
                .clone()
                .unwrap_or_else(|| config.monitor.clone()),
            usage,
            metrics: None,
            history: VecDeque::new(),
            providers: Vec::new(),
            accounts: Vec::new(),
            selected_provider: None,
            selected_account: None,
            selected_pane: None,
            usage_endpoint: None,
            hover: None,
            hover_rect: Rect::default(),
            hover_hits: Vec::new(),
            page_rect: Rect::default(),
            hits: Vec::new(),
            selected_hit: 0,
            selected_card: None,
            selected_core: None,
            card_scroll: HashMap::new(),
            account_scroll: 0,
            scroll: 0,
            paused: false,
            process_dialog: None,
            process_filter: String::new(),
            filtering_processes: false,
            process_sort: ProcessSort::Cpu,
            message: None,
            now_ms: 0,
            epoch: 0,
            source: String::new(),
            usage_source: String::new(),
            next_metrics: Instant::now(),
            next_usage: Instant::now(),
            refresh_usage: false,
            pending: HashSet::new(),
            alerts: HashMap::new(),
        }
    }

    pub(super) fn paint(
        &mut self,
        frame: &mut FrameData,
        area: Rect,
        palette: &Palette,
    ) -> Option<Rect> {
        let visible = self.page.is_some()
            || self.process_dialog.is_some()
            || self.hover.as_ref().is_some_and(|hover| hover.visible);
        if !visible {
            self.hits.clear();
            self.hover_rect = Rect::default();
            self.hover_hits.clear();
            return None;
        }
        let mut buffer = frame.to_ratatui_buffer()?;
        let (covered, hits, hover_rect) = render::paint(&mut buffer, area, self, palette);
        self.hover_hits = if hover_rect.is_empty() {
            Vec::new()
        } else {
            hits.clone()
        };
        self.hits = hits;
        self.selected_hit = self.selected_hit.min(self.hits.len().saturating_sub(1));
        if self.page.is_some() {
            if let Some((rect, _)) = self.hits.get(self.selected_hit) {
                buffer.set_style(
                    *rect,
                    Style::default()
                        .fg(palette.accent)
                        .add_modifier(Modifier::UNDERLINED),
                );
            }
        }
        self.page_rect = if self.page.is_some() {
            area
        } else {
            Rect::default()
        };
        self.hover_rect = hover_rect;
        let cursor = if self.page.is_some() {
            None
        } else {
            frame.cursor.clone()
        };
        frame.replace_from_ratatui_buffer_preserving_effects(&buffer, cursor);
        Some(covered)
    }

    fn apply_metrics(&mut self, snapshot: Box<SystemMetricsSnapshot>) {
        if self
            .metrics
            .as_ref()
            .is_some_and(|old| old.boot_id == snapshot.boot_id && old.sequence >= snapshot.sequence)
        {
            return;
        }
        if self
            .metrics
            .as_ref()
            .is_some_and(|old| old.boot_id != snapshot.boot_id)
        {
            self.history.clear();
            self.alerts.clear();
        }
        if !self.paused && snapshot.sampled_at_ms > 0 {
            let memory = (snapshot.memory.total_bytes > 0).then(|| {
                snapshot.memory.used_bytes as f32 / snapshot.memory.total_bytes as f32 * 100.0
            });
            self.history.push_back(HistoryPoint {
                at: snapshot.sampled_at_ms,
                cpu: snapshot.cpu_percent,
                memory,
                cores: snapshot
                    .cores
                    .iter()
                    .map(|core| core.usage_percent)
                    .collect(),
            });
            let cutoff = snapshot
                .sampled_at_ms
                .saturating_sub(u64::from(self.monitor.history_minutes.clamp(1, 60)) * 60_000);
            let max_points =
                (16 * 1024 * 1024 / (64 + snapshot.cores.len().saturating_mul(8))).clamp(2, 7200);
            while self.history.len() > max_points
                || self.history.front().is_some_and(|point| point.at < cutoff)
            {
                self.history.pop_front();
            }
        }
        self.metrics = Some(snapshot);
    }

    fn alert(&mut self) -> Option<String> {
        if !self.monitor.alerts_enabled {
            self.alerts.clear();
            return None;
        }
        let metrics = self.metrics.as_ref()?;
        if metrics.status != ObservationStatus::Ready
            || self.now_ms.saturating_sub(metrics.sampled_at_ms) > 15_000
        {
            return None;
        }
        for rule in &self.monitor.alerts {
            let group = if rule.metric == "disk" {
                "disks"
            } else {
                rule.metric.as_str()
            };
            if metrics
                .group_status
                .get(group)
                .is_some_and(|status| *status != ObservationStatus::Ready)
                || metrics
                    .group_sampled_at_ms
                    .get(group)
                    .is_some_and(|sampled| self.now_ms.saturating_sub(*sampled) > 15_000)
            {
                continue;
            }
            let value = match rule.metric.as_str() {
                "cpu" => metrics.cpu_percent.map(f64::from),
                "memory" => (metrics.memory.total_bytes > 0).then(|| {
                    metrics.memory.used_bytes as f64 / metrics.memory.total_bytes as f64 * 100.0
                }),
                "gpu" => metrics
                    .gpus
                    .iter()
                    .filter(|gpu| gpu.status == ObservationStatus::Ready)
                    .filter_map(|gpu| gpu.usage_percent)
                    .reduce(f32::max)
                    .map(f64::from),
                "disk" => metrics
                    .disks
                    .iter()
                    .filter(|disk| disk.total_bytes > 0)
                    .map(|disk| {
                        (disk.total_bytes.saturating_sub(disk.available_bytes)) as f64
                            / disk.total_bytes as f64
                            * 100.0
                    })
                    .reduce(f64::max),
                _ => None,
            };
            let state = self.alerts.entry(rule.metric.clone()).or_default();
            let Some(value) = value.filter(|v| v.is_finite()) else {
                state.since = None;
                continue;
            };
            if value < rule.threshold - 5.0 {
                state.since = None;
                state.armed = false;
            } else if value >= rule.threshold {
                let since = state.since.get_or_insert(self.now_ms);
                if !state.armed
                    && self.now_ms.saturating_sub(*since)
                        >= rule.duration_seconds.saturating_mul(1000)
                    && state.last_fired.is_none_or(|last| {
                        self.now_ms.saturating_sub(last)
                            >= rule.cooldown_seconds.saturating_mul(1000)
                    })
                {
                    state.armed = true;
                    state.last_fired = Some(self.now_ms);
                    return Some(format!(
                        "{} · {} {:.1}%",
                        metrics.hostname, rule.metric, value
                    ));
                }
            } else {
                state.since = None;
            }
        }
        None
    }
}

impl ClientShellState {
    pub(super) fn open_observation_page(&mut self, page: Page, outcome: &mut ClientShellInput) {
        // One dock panel hosts system, accounts, and settings pages; the
        // requested page becomes the active tab inside it.
        self.workbench_open(dock::PanelId::Monitor);
        self.observability.page = Some(page);
        self.observability.hover = None;
        self.observability.scroll = 0;
        self.observability.next_metrics = Instant::now();
        self.observability.next_usage = Instant::now();
        self.observability.message = None;
        self.selection = None;
        self.clear_link_hover();
        self.mode = ClientShellMode::Terminal;
        outcome.repaint = true;
    }

    fn observation_request(
        &mut self,
        method: Method,
        purpose: Purpose,
        outcome: &mut ClientShellInput,
    ) {
        if self.observability.pending.contains(purpose.key()) {
            return;
        }
        let key = purpose.key();
        let endpoint_id = if matches!(
            purpose,
            Purpose::Providers | Purpose::Usage | Purpose::Binding | Purpose::Integration
        ) {
            self.observability
                .usage_endpoint
                .as_ref()
                .unwrap_or(&self.active_endpoint_id)
        } else {
            &self.active_endpoint_id
        };
        let Some(endpoint) = self.endpoints.iter().find(|endpoint| {
            &endpoint.endpoint_id == endpoint_id && endpoint.status == ClientEndpointStatus::Online
        }) else {
            return;
        };
        if endpoint
            .methods
            .as_ref()
            .is_none_or(|methods| !methods.contains(crate::api::api_method_name(&method)))
        {
            self.observability.message = Some(
                tr(
                    "This server does not support this feature yet.",
                    "此主机尚未支持该功能，请更新 server。",
                )
                .into(),
            );
            return;
        }
        let Some(snapshot) = endpoint.snapshot.as_ref() else {
            return;
        };
        let request_id = format!("client-shell:{}", self.next_request_id);
        self.next_request_id = self.next_request_id.saturating_add(1);
        let endpoint_id = endpoint_id.clone();
        let boot_id = snapshot.boot_id.clone();
        self.pending_requests.insert(
            request_id.clone(),
            PendingEndpointRequest {
                boot_id: boot_id.clone(),
                method_name: crate::api::api_method_name(&method).into(),
                confirmation_workspace_id: None,
                kind: PendingEndpointKind::Observation {
                    epoch: self.observability.epoch,
                    endpoint_id: endpoint_id.clone(),
                    purpose,
                },
            },
        );
        outcome.actions.push(ClientShellAction::Endpoint {
            endpoint_id,
            boot_id,
            request: Box::new(Request {
                id: request_id,
                method,
            }),
        });
        self.observability.pending.insert(key);
    }

    pub(crate) fn is_observation_request(&self, id: &str) -> bool {
        self.pending_requests
            .get(id)
            .is_some_and(|request| matches!(request.kind, PendingEndpointKind::Observation { .. }))
    }

    pub(crate) fn tick_observability(&mut self, now: Instant, outcome: &mut ClientShellInput) {
        let source = self.snapshot.as_ref().map(|snapshot| {
            format!(
                "{}:{}",
                self.active_endpoint_id.storage_key(),
                snapshot.boot_id
            )
        });
        if source
            .as_deref()
            .is_some_and(|source| source != self.observability.source)
        {
            self.observability.source = source.unwrap_or_default();
            self.observability.epoch = self.observability.epoch.saturating_add(1);
            self.observability.metrics = None;
            self.observability.history.clear();
            self.observability.accounts.clear();
            self.observability.usage_endpoint = None;
            self.observability.providers.clear();
            self.observability.pending.clear();
            self.observability.hover = None;
            self.observability.process_dialog = None;
            self.observability.next_metrics = now;
            self.observability.next_usage = now;
        }
        let previous_second = self.observability.now_ms / 1000;
        self.observability.now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64;
        if let Some(hover) = &mut self.observability.hover {
            if hover.leave_at.is_some_and(|at| now >= at) {
                self.observability.hover = None;
                outcome.repaint = true;
            } else if !hover.visible
                && now.duration_since(hover.since)
                    >= Duration::from_millis(
                        self.observability.usage.hover_delay_ms.clamp(200, 2000),
                    )
            {
                hover.visible = true;
                self.observability.epoch = self.observability.epoch.saturating_add(1);
                self.observability.pending.clear();
                self.observability.accounts.clear();
                self.observability.selected_provider = Some(hover.agent.clone());
                self.observability.usage_endpoint = Some(hover.endpoint_id.clone());
                self.observability.selected_pane = Some(hover.pane.clone());
                self.observability.selected_account = None;
                self.observability.account_scroll = 0;
                self.observability.next_usage = now;
                outcome.repaint = true;
            }
        }
        let usage_visible = self.observability.page == Some(Page::Accounts)
            || self.workbench.visible(&dock::PanelId::Accounts)
            || self
                .observability
                .hover
                .as_ref()
                .is_some_and(|hover| hover.visible);
        let usage_endpoint = self
            .observability
            .usage_endpoint
            .as_ref()
            .unwrap_or(&self.active_endpoint_id);
        let usage_source = self
            .endpoints
            .iter()
            .find(|endpoint| &endpoint.endpoint_id == usage_endpoint)
            .and_then(|endpoint| endpoint.snapshot.as_ref())
            .map(|snapshot| format!("{}:{}", usage_endpoint.storage_key(), snapshot.boot_id))
            .unwrap_or_default();
        if self.observability.usage_source != usage_source {
            self.observability.usage_source = usage_source;
            self.observability.epoch = self.observability.epoch.saturating_add(1);
            self.observability.accounts.clear();
            self.observability.providers.clear();
            self.observability.pending.clear();
            self.observability.next_usage = now;
        }
        if self.observability.page.is_some() || usage_visible {
            outcome.repaint |= previous_second != self.observability.now_ms / 1000;
        }
        if !self.endpoint_is_online(&self.active_endpoint_id) {
            return;
        }
        if (self.observability.page == Some(Page::Monitor)
            || self.workbench.visible(&dock::PanelId::Monitor)
            || self.observability.monitor.alerts_enabled)
            && now >= self.observability.next_metrics
        {
            let config = &self.observability.monitor;
            let interval_ms = config.interval_ms.clamp(500, 5000);
            let params = SystemMetricsParams {
                interval_ms,
                include_processes: config.visible.iter().any(|item| item == "processes"),
                groups: config.visible.clone(),
            };
            self.observability.next_metrics = now + Duration::from_millis(interval_ms);
            self.observation_request(Method::SystemMetricsGet(params), Purpose::Metrics, outcome);
        }
        if (usage_visible || self.observability.page == Some(Page::Settings))
            && self.observability.usage.enabled
            && now >= self.observability.next_usage
        {
            self.observability.next_usage = now + Duration::from_secs(2);
            if self.observability.providers.is_empty() {
                self.observation_request(
                    Method::AccountUsageProviders(EmptyParams::default()),
                    Purpose::Providers,
                    outcome,
                );
            }
            let params = UsageParams {
                agent: self.observability.selected_provider.clone(),
                account_id: self.observability.selected_account.clone(),
                pane_id: self.observability.selected_pane.clone(),
            };
            if !self
                .observability
                .selected_provider
                .as_ref()
                .is_some_and(|agent| self.observability.usage.disabled_providers.contains(agent))
            {
                let method = if std::mem::take(&mut self.observability.refresh_usage) {
                    Method::AccountUsageRefresh(params)
                } else {
                    Method::AccountUsageGet(params)
                };
                self.observation_request(method, Purpose::Usage, outcome);
            }
        }
        if let Some(body) = self.observability.alert() {
            outcome.repaint |= self.push_endpoint_notice(
                ClientEndpointNoticeKind::Rejected,
                format!("monitor-{}", self.observability.now_ms),
                tr("Resource alert", "资源告警"),
                body,
            );
        }
    }

    pub(super) fn receive_observation(
        &mut self,
        epoch: u64,
        purpose: Purpose,
        result: Result<ResponseResult, ClientShellEndpointError>,
    ) -> bool {
        if epoch != self.observability.epoch {
            return false;
        }
        self.observability.pending.remove(purpose.key());
        match result {
            Ok(ResponseResult::SystemMetrics { snapshot }) => {
                self.observability.apply_metrics(snapshot)
            }
            Ok(ResponseResult::AccountUsageProviders { providers }) => {
                self.observability.providers = providers
            }
            Ok(ResponseResult::AccountUsage { accounts }) => {
                if self.observability.selected_provider.is_some() {
                    if let Some(first) = accounts
                        .first()
                        .filter(|first| accounts.iter().all(|account| account.agent == first.agent))
                    {
                        self.observability.selected_provider = Some(first.agent.clone());
                    }
                }
                self.observability.accounts = accounts;
            }
            Ok(ResponseResult::AccountBinding { account_id, .. }) => {
                self.observability.selected_account = Some(account_id);
                self.observability.next_usage = Instant::now();
                self.observability.message = None;
            }
            Ok(ResponseResult::SystemProcess { process }) => {
                self.observability.process_dialog = Some(ProcessDialog {
                    process,
                    force: false,
                    confirm: false,
                    pending: false,
                });
            }
            Ok(ResponseResult::SystemProcessTerminated { .. }) => {
                self.observability.process_dialog = None;
                self.observability.message = Some(
                    tr(
                        "Termination sent. Waiting for the next sample.",
                        "已发送结束请求，等待下一次采样。",
                    )
                    .into(),
                );
            }
            Ok(ResponseResult::Ok {}) if matches!(purpose, Purpose::Integration) => {
                self.observability.message = Some(tr("Official statusline integration updated. The CLI publishes data on its next status update.", "官方状态栏回调已更新，等待 CLI 下一次状态更新。" ).into());
            }
            Ok(_) => {}
            Err(error) => {
                self.observability.message = Some(error.message);
                if let Some(dialog) = &mut self.observability.process_dialog {
                    dialog.pending = false;
                    dialog.confirm = false;
                }
            }
        }
        self.observability.page.is_some() || self.observability.hover.is_some()
    }

    pub(super) fn observation_action(&mut self, action: Action, outcome: &mut ClientShellInput) {
        let monitor_changed = matches!(
            &action,
            Action::Interval
                | Action::Metric(_)
                | Action::CardMove(..)
                | Action::CardSize
                | Action::HistoryRange
                | Action::Device(_)
                | Action::ToggleAlerts
                | Action::AlertThreshold(_)
                | Action::AlertDuration(_)
                | Action::AlertCooldown(_)
        );
        let usage_changed = matches!(
            &action,
            Action::UsageEnabled
                | Action::UsageFormat
                | Action::UsagePosition
                | Action::ProviderEnabled(_)
        );
        match action {
            Action::CycleAccount => {
                if let Some(provider) = self.observability.providers.iter().find(|provider| {
                    Some(&provider.agent) == self.observability.selected_provider.as_ref()
                }) {
                    let current = provider
                        .configured_accounts
                        .iter()
                        .position(|id| Some(id) == self.observability.selected_account.as_ref());
                    if let Some(next) = provider
                        .configured_accounts
                        .get(current.map_or(0, |index| {
                            (index + 1) % provider.configured_accounts.len().max(1)
                        }))
                        .cloned()
                    {
                        self.observation_action(Action::Account(next), outcome);
                    }
                }
            }
            Action::UsageIntegration(enabled) => {
                if let Some(account_id) =
                    self.observability.selected_account.clone().or_else(|| {
                        (self.observability.accounts.len() == 1)
                            .then(|| self.observability.accounts[0].account_id.clone())
                    })
                {
                    self.observation_request(
                        Method::AccountUsageIntegration(UsageIntegrationParams {
                            account_id,
                            enabled,
                        }),
                        Purpose::Integration,
                        outcome,
                    );
                }
            }
            Action::Card(card) => self.observability.selected_card = Some(card),
            Action::Core(core) => {
                self.observability.selected_core = if self.observability.selected_core == Some(core)
                {
                    None
                } else {
                    Some(core)
                }
            }
            Action::CardMove(card, delta) => {
                if let Some(index) = self
                    .observability
                    .monitor
                    .visible
                    .iter()
                    .position(|id| id == &card)
                {
                    let next = index
                        .saturating_add_signed(delta)
                        .min(self.observability.monitor.visible.len().saturating_sub(1));
                    self.observability.monitor.visible.swap(index, next);
                }
            }
            Action::CardSize => {
                self.observability.monitor.card_height =
                    match self.observability.monitor.card_height {
                        7 => 10,
                        10 => 16,
                        16 => 24,
                        _ => 7,
                    }
            }
            Action::HistoryRange => {
                self.observability.monitor.history_minutes =
                    match self.observability.monitor.history_minutes {
                        1 => 5,
                        5 => 15,
                        15 => 30,
                        30 => 60,
                        _ => 1,
                    }
            }
            Action::Device(id) => {
                let hidden = &mut self.observability.monitor.hidden_devices;
                if hidden.contains(&id) {
                    hidden.retain(|value| value != &id);
                } else {
                    hidden.push(id);
                }
            }
            Action::ProviderEnabled(agent) => {
                let disabled = &mut self.observability.usage.disabled_providers;
                if disabled.contains(&agent) {
                    disabled.retain(|value| value != &agent);
                } else {
                    disabled.push(agent);
                }
            }
            Action::AlertThreshold(index) => {
                if let Some(rule) = self.observability.monitor.alerts.get_mut(index) {
                    rule.threshold = if rule.threshold >= 100.0 {
                        50.0
                    } else {
                        rule.threshold + 5.0
                    };
                }
            }
            Action::AlertDuration(index) => {
                if let Some(rule) = self.observability.monitor.alerts.get_mut(index) {
                    rule.duration_seconds = match rule.duration_seconds {
                        10 => 30,
                        30 => 60,
                        60 => 300,
                        _ => 10,
                    };
                }
            }
            Action::AlertCooldown(index) => {
                if let Some(rule) = self.observability.monitor.alerts.get_mut(index) {
                    rule.cooldown_seconds = match rule.cooldown_seconds {
                        60 => 300,
                        300 => 900,
                        _ => 60,
                    };
                }
            }
            Action::Page(page) => self.open_observation_page(page, outcome),
            Action::Close => {
                if self.workbench.enabled {
                    self.workbench
                        .dock
                        .close_panel(&self.workbench.dock.focused.clone());
                }
                self.observability.page = None;
                self.observability.hover = None;
                self.observability.process_dialog = None;
            }
            Action::Configure => self.open_observation_page(Page::Settings, outcome),
            Action::Pause => self.observability.paused = !self.observability.paused,
            Action::Interval => {
                self.observability.monitor.interval_ms =
                    match self.observability.monitor.interval_ms {
                        500 => 1000,
                        1000 => 2000,
                        2000 => 5000,
                        _ => 500,
                    };
                self.observability.next_metrics = Instant::now();
            }
            Action::Metric(metric) => {
                let items = &mut self.observability.monitor.visible;
                if items.contains(&metric) {
                    items.retain(|item| item != &metric);
                } else {
                    items.push(metric);
                }
            }
            Action::ToggleAlerts => {
                self.observability.monitor.alerts_enabled =
                    !self.observability.monitor.alerts_enabled
            }
            Action::UsageEnabled => {
                self.observability.usage.enabled = !self.observability.usage.enabled
            }
            Action::UsageFormat => {
                self.observability.account_scroll = 0;
                self.observability.usage.format = match self.observability.usage.format {
                    UsageDisplayFormat::Dashboard => UsageDisplayFormat::Table,
                    UsageDisplayFormat::Table => UsageDisplayFormat::Dashboard,
                }
            }
            Action::UsagePosition => {
                self.observability.usage.position = match self.observability.usage.position {
                    UsageDisplayPosition::Hover => UsageDisplayPosition::Page,
                    UsageDisplayPosition::Page => UsageDisplayPosition::Both,
                    UsageDisplayPosition::Both => UsageDisplayPosition::Hover,
                }
            }
            Action::Provider(agent) => {
                self.observability.account_scroll = 0;
                self.observability.epoch = self.observability.epoch.saturating_add(1);
                self.observability.pending.clear();
                self.observability.accounts.clear();
                self.observability.selected_provider = Some(agent);
                self.observability.selected_account = None;
                self.observability.selected_pane = None;
                self.observability.next_usage = Instant::now();
            }
            Action::Account(account) => {
                self.observability.account_scroll = 0;
                self.observability.epoch = self.observability.epoch.saturating_add(1);
                self.observability.pending.clear();
                self.observability.accounts.clear();
                self.observability.selected_account = Some(account);
                self.observability.next_usage = Instant::now();
            }
            Action::Bind => {
                if let Some(pane_id) = self.observability.selected_pane.clone() {
                    let account_id = self.observability.selected_account.clone().or_else(|| {
                        (self.observability.accounts.len() == 1)
                            .then(|| self.observability.accounts[0].account_id.clone())
                    });
                    if let Some(account_id) = account_id {
                        self.observation_request(
                            Method::AccountBindingSet(AccountBindingParams {
                                pane_id,
                                account_id,
                            }),
                            Purpose::Binding,
                            outcome,
                        );
                    }
                }
            }
            Action::Refresh => {
                self.observability.refresh_usage = true;
                self.observability.next_metrics = Instant::now();
                self.observability.next_usage = Instant::now();
            }
            Action::Source => {
                if let Some(url) = self
                    .observability
                    .accounts
                    .first()
                    .map(|account| account.source_url.clone())
                    .filter(|url| crate::app::actions::safe_web_url(url).is_some())
                {
                    outcome.actions.push(ClientShellAction::OpenSafeWebUrl(url));
                }
            }
            Action::Process(identity) => self.observation_request(
                Method::SystemProcessGet(ProcessGetParams { identity }),
                Purpose::Process,
                outcome,
            ),
            Action::CancelProcess => self.observability.process_dialog = None,
            Action::Terminate(force) => {
                if let Some(dialog) = &mut self.observability.process_dialog {
                    dialog.force = force;
                    dialog.confirm = true;
                }
            }
            Action::ConfirmProcess => {
                if let Some(dialog) = self.observability.process_dialog.as_mut().filter(|dialog| {
                    dialog.confirm
                        && !dialog.pending
                        && !dialog.process.protected
                        && dialog.process.action_token.is_some()
                }) {
                    dialog.pending = true;
                    let params = ProcessTerminateParams {
                        identity: dialog.process.identity.clone(),
                        action_token: dialog.process.action_token.clone(),
                        force: dialog.force,
                    };
                    self.observation_request(
                        Method::SystemProcessTerminate(params),
                        Purpose::Terminate,
                        outcome,
                    );
                }
            }
            Action::SortProcesses => {
                self.observability.process_sort = match self.observability.process_sort {
                    ProcessSort::Cpu => ProcessSort::Memory,
                    ProcessSort::Memory => ProcessSort::Name,
                    _ => ProcessSort::Cpu,
                }
            }
            Action::FilterProcesses => {
                self.observability.filtering_processes = !self.observability.filtering_processes
            }
        }
        if monitor_changed {
            self.config.preferences.monitor = Some(self.observability.monitor.clone());
        }
        if usage_changed {
            self.config.preferences.usage_enabled = Some(self.observability.usage.enabled);
            self.config.preferences.usage_format = Some(self.observability.usage.format);
            self.config.preferences.usage_position = Some(self.observability.usage.position);
            self.config.preferences.usage_disabled_providers =
                Some(self.observability.usage.disabled_providers.clone());
        }
        if monitor_changed || usage_changed {
            self.persist_chrome_preferences(outcome);
        }
        outcome.repaint = true;
    }

    pub(super) fn observation_mouse(
        &mut self,
        mouse: MouseEvent,
        outcome: &mut ClientShellInput,
    ) -> bool {
        if self.overlay.is_some() || self.popup_terminal_id.is_some() {
            self.observability.hover = None;
            return false;
        }
        let point = (mouse.column, mouse.row);
        if self.observability.process_dialog.is_some() {
            if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
                if let Some(action) =
                    self.observability
                        .hits
                        .iter()
                        .rev()
                        .find_map(|(rect, action)| {
                            (contains(*rect, point)
                                && matches!(
                                    action,
                                    Action::CancelProcess
                                        | Action::Terminate(_)
                                        | Action::ConfirmProcess
                                ))
                            .then(|| action.clone())
                        })
                {
                    self.observation_action(action, outcome);
                }
            }
            return true;
        }
        // 浮层独占内部输入，空白和滚轮也不能穿透到底层卡片或进程行。
        if contains(self.observability.hover_rect, point) {
            if let Some(hover) = &mut self.observability.hover {
                hover.leave_at = None;
            }
            match mouse.kind {
                MouseEventKind::ScrollDown => {
                    self.observability.scroll_accounts(1);
                    outcome.repaint = true;
                }
                MouseEventKind::ScrollUp => {
                    self.observability.scroll_accounts(-1);
                    outcome.repaint = true;
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    if let Some(action) = self
                        .observability
                        .hover_hits
                        .iter()
                        .rev()
                        .find(|(rect, _)| contains(*rect, point))
                        .map(|(_, action)| action.clone())
                    {
                        self.observation_action(action, outcome);
                    }
                }
                _ => {}
            }
            return true;
        }
        if matches!(
            mouse.kind,
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp
        ) {
            if let Some(card) = self.observability.hits.iter().find_map(|(rect, action)| {
                if contains(*rect, point) {
                    if let Action::Card(id) = action {
                        Some(id.clone())
                    } else {
                        None
                    }
                } else {
                    None
                }
            }) {
                let count =
                    self.observability
                        .metrics
                        .as_ref()
                        .map_or(0, |sample| match card.as_str() {
                            "cores" => sample.cores.len(),
                            "gpu" => sample.gpus.len(),
                            "disks" => sample.disks.len(),
                            "network" => sample.networks.len(),
                            "sensors" => sample.sensors.len(),
                            "processes" => sample.processes.len(),
                            _ => 0,
                        });
                let scroll = self.observability.card_scroll.entry(card).or_default();
                *scroll = if mouse.kind == MouseEventKind::ScrollDown {
                    scroll.saturating_add(1).min(count.saturating_sub(1))
                } else {
                    scroll.saturating_sub(1)
                };
                outcome.repaint = true;
                return true;
            }
            if self.observability.page == Some(Page::Accounts) {
                self.observability
                    .scroll_accounts(if mouse.kind == MouseEventKind::ScrollDown {
                        1
                    } else {
                        -1
                    });
                outcome.repaint = true;
                return true;
            }
        }
        if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
            if let Some(action) = self
                .observability
                .hits
                .iter()
                .rev()
                .find(|(rect, _)| contains(*rect, point))
                .map(|(_, action)| action.clone())
            {
                self.observation_action(action, outcome);
                return true;
            }
        }
        if self.observability.page.is_some() && contains(self.observability.page_rect, point) {
            match mouse.kind {
                MouseEventKind::ScrollDown => {
                    self.observability.scroll = self.observability.scroll.saturating_add(3)
                }
                MouseEventKind::ScrollUp => {
                    self.observability.scroll = self.observability.scroll.saturating_sub(3)
                }
                _ => {}
            }
            outcome.repaint = true;
            return true;
        }
        if matches!(mouse.kind, MouseEventKind::Down(_)) {
            outcome.repaint |= self.observability.hover.take().is_some();
        }
        if self.observability.usage.enabled
            && self.observability.usage.position != UsageDisplayPosition::Page
            && mouse.kind == MouseEventKind::Moved
            && self.chrome_drag.is_none()
            && self.pane_mouse_gesture.is_none()
        {
            let pane = self
                .hits
                .endpoint_agents
                .iter()
                .find(|(rect, _, _)| contains(*rect, point))
                .cloned()
                .or_else(|| {
                    self.hits
                        .agents
                        .iter()
                        .find(|(rect, _)| contains(*rect, point))
                        .map(|(rect, pane)| (*rect, self.active_endpoint_id.clone(), pane.clone()))
                })
                .or_else(|| {
                    self.hits
                        .panes
                        .iter()
                        .find(|hit| mouse.row == hit.rect.y && contains(hit.rect, point))
                        .map(|hit| {
                            (
                                hit.rect,
                                self.active_endpoint_id.clone(),
                                hit.pane_id.clone(),
                            )
                        })
                });
            let target = pane.and_then(|(rect, endpoint, pane)| {
                self.endpoints
                    .iter()
                    .find(|entry| entry.endpoint_id == endpoint)?
                    .snapshot
                    .as_ref()?
                    .agents
                    .iter()
                    .find(|agent| agent.pane_id == pane)
                    .and_then(|agent| {
                        agent
                            .agent
                            .clone()
                            .filter(|agent| !agent.eq_ignore_ascii_case("muse"))
                            .map(|agent| (rect, endpoint, pane, agent))
                    })
            });
            if let Some((anchor, endpoint_id, pane, agent)) = target {
                if self
                    .observability
                    .hover
                    .as_ref()
                    .is_none_or(|hover| hover.pane != pane || hover.endpoint_id != endpoint_id)
                {
                    self.observability.hover = Some(Hover {
                        endpoint_id,
                        pane,
                        agent,
                        anchor,
                        since: Instant::now(),
                        visible: false,
                        leave_at: None,
                    });
                    self.observability.hover_rect = Rect::default();
                    outcome.repaint = true;
                } else if let Some(hover) = &mut self.observability.hover {
                    hover.leave_at = None;
                }
            } else if let Some(hover) = &mut self.observability.hover {
                hover
                    .leave_at
                    .get_or_insert(Instant::now() + Duration::from_millis(250));
            }
        }
        false
    }

    pub(super) fn observation_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) -> bool {
        if self.observability.page.is_none() && self.observability.process_dialog.is_none() {
            return false;
        }
        if self.observability.process_dialog.is_some()
            && !matches!(
                key.code,
                KeyCode::Tab | KeyCode::BackTab | KeyCode::Enter | KeyCode::Esc
            )
        {
            return true;
        }
        if self.observability.filtering_processes {
            match key.code {
                KeyCode::Esc | KeyCode::Enter => self.observability.filtering_processes = false,
                KeyCode::Backspace => {
                    self.observability.process_filter.pop();
                }
                KeyCode::Char(ch)
                    if !ch.is_control() && self.observability.process_filter.len() < 256 =>
                {
                    self.observability.process_filter.push(ch)
                }
                _ => {}
            }
            outcome.repaint = true;
            return true;
        }
        if self.observability.page == Some(Page::Monitor)
            && matches!(key.code, KeyCode::Up | KeyCode::Down)
        {
            if let Some(card) = &self.observability.selected_card {
                let count =
                    self.observability
                        .metrics
                        .as_ref()
                        .map_or(0, |sample| match card.as_str() {
                            "cores" => sample.cores.len(),
                            "gpu" => sample.gpus.len(),
                            "disks" => sample.disks.len(),
                            "network" => sample.networks.len(),
                            "sensors" => sample.sensors.len(),
                            "processes" => sample.processes.len(),
                            _ => 0,
                        });
                let scroll = self
                    .observability
                    .card_scroll
                    .entry(card.clone())
                    .or_default();
                *scroll = if key.code == KeyCode::Down {
                    scroll.saturating_add(1).min(count.saturating_sub(1))
                } else {
                    scroll.saturating_sub(1)
                };
                outcome.repaint = true;
                return true;
            }
        }
        let action = match key.code {
            KeyCode::Tab | KeyCode::BackTab => {
                let len = self.observability.hits.len();
                if len > 0 {
                    self.observability.selected_hit = if key.code == KeyCode::BackTab {
                        (self.observability.selected_hit + len - 1) % len
                    } else {
                        (self.observability.selected_hit + 1) % len
                    };
                }
                None
            }
            KeyCode::Enter => self
                .observability
                .hits
                .get(self.observability.selected_hit)
                .map(|(_, action)| action.clone()),
            KeyCode::Esc if self.observability.process_dialog.is_some() => {
                Some(Action::CancelProcess)
            }
            KeyCode::Esc => Some(Action::Close),
            KeyCode::Char('1') => Some(Action::Page(Page::Monitor)),
            KeyCode::Char('2') => Some(Action::Page(Page::Accounts)),
            KeyCode::Char('3') | KeyCode::Char('s') => Some(Action::Configure),
            KeyCode::Char('r') => Some(Action::Refresh),
            KeyCode::Char(' ') if self.observability.page == Some(Page::Monitor) => {
                Some(Action::Pause)
            }
            KeyCode::Char('/') if self.observability.page == Some(Page::Monitor) => {
                Some(Action::FilterProcesses)
            }
            KeyCode::Down | KeyCode::PageDown
                if self.observability.page == Some(Page::Accounts) =>
            {
                self.observability.scroll_accounts(3);
                None
            }
            KeyCode::Up | KeyCode::PageUp if self.observability.page == Some(Page::Accounts) => {
                self.observability.scroll_accounts(-3);
                None
            }
            KeyCode::Down | KeyCode::PageDown => {
                self.observability.scroll = self.observability.scroll.saturating_add(3);
                None
            }
            KeyCode::Up | KeyCode::PageUp => {
                self.observability.scroll = self.observability.scroll.saturating_sub(3);
                None
            }
            _ => None,
        };
        if let Some(action) = action {
            self.observation_action(action, outcome);
        }
        outcome.repaint = true;
        true
    }

    pub(super) fn paint_observability(
        &mut self,
        frame: &mut FrameData,
        area: Rect,
    ) -> Option<Rect> {
        let covered = self
            .observability
            .paint(frame, area, &self.config.palette)?;
        if self.observability.page.is_some() {
            self.hits.panes.clear();
            self.hits.pane_splits.clear();
        }
        Some(covered)
    }
}
