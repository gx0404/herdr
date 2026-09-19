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

/// Accounts overview table reused by the floating usage dashboard overlay;
/// display-only, so row actions are discarded.
pub(super) fn render_usage_table(
    buffer: &mut Buffer,
    area: Rect,
    state: &State,
    palette: &Palette,
) {
    render::usage_table(
        buffer,
        area,
        state,
        &render::page_scope(state),
        palette,
        &mut Vec::new(),
    );
}

/// 监控面板内的页面（tab）；可持久化为客户端偏好，因此 wire 名固定为 snake_case。
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
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
    /// 悬浮层自己的用量请求：带 hover 的 pane，落到 `hover_scope`，永不写页面。
    HoverUsage,
    Binding,
    /// 悬浮层发起的绑定：发往 hover 的端点，回流只刷新悬浮层。
    HoverBinding,
    Integration,
    /// 悬浮层发起的官方回调开关：发往 hover 的端点。
    HoverIntegration,
    Process,
    Terminate,
}

impl Purpose {
    fn key(&self) -> &'static str {
        match self {
            Self::Metrics => "metrics",
            Self::Providers => "providers",
            Self::Usage => "usage",
            Self::HoverUsage => "hover_usage",
            Self::Binding => "binding",
            Self::HoverBinding => "hover_binding",
            Self::Integration => "integration",
            Self::HoverIntegration => "hover_integration",
            Self::Process => "process",
            Self::Terminate => "terminate",
        }
    }

    /// 悬浮层作用域的请求：目标端点与代际都取自 `hover_scope`。
    fn is_hover(&self) -> bool {
        matches!(
            self,
            Self::HoverUsage | Self::HoverBinding | Self::HoverIntegration
        )
    }

    const HOVER_KEYS: [&'static str; 3] = ["hover_usage", "hover_binding", "hover_integration"];
}

/// `observation_request` 的结局：调用方据此决定强意图刷新标志的去留。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RequestOutcome {
    /// 请求已发出。
    Sent,
    /// 同类请求在途：保留标志稍后重试。
    Busy,
    /// 端点离线 / 未宣告该方法 / 无快照：该动作当前不可用，标志应清除。
    Unavailable,
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

/// 悬浮层独立的数据作用域：厂商 / pane / 端点 / 账号快照 / epoch / 刷新标志 /
/// 滚动位置 / 轮询时刻，与账号页的 `selected_*` / `accounts` / `epoch` /
/// `account_scroll` / `next_usage` 互不污染。页面轮询永不带这里的 pane；悬浮层
/// 出现、消失都不改写页面选择，也不提前唤醒页面轮询。
pub(super) struct HoverScope {
    pub provider: Option<String>,
    pub pane: Option<String>,
    pub endpoint: Option<ClientEndpointId>,
    pub accounts: Vec<AccountUsageSnapshot>,
    /// 悬浮层请求的代际：作用域复位即推进，旧响应按此丢弃。
    pub epoch: u64,
    /// 排队中的强意图刷新（悬浮层首次可见 / 点「刷新」/ 绑定回流）；请求真正
    /// 发出后才清除，被在途请求挡下时保留并 200 ms 重试。
    pub refresh: bool,
    /// 悬浮层的 `account.usage.refresh` 在途；响应到达即清除。
    manual_in_flight: bool,
    /// 悬浮层自己的账号列表滚动位置。
    pub scroll: usize,
    /// 悬浮层下一次轮询时刻，与页面的 `next_usage` 各自独立。
    next_usage: Instant,
}

impl Default for HoverScope {
    fn default() -> Self {
        Self {
            provider: None,
            pane: None,
            endpoint: None,
            accounts: Vec::new(),
            epoch: 0,
            refresh: false,
            manual_in_flight: false,
            scroll: 0,
            next_usage: Instant::now(),
        }
    }
}

impl HoverScope {
    /// 悬浮层处于「刷新中」：强意图刷新已排队或正在途中（语义与页面的
    /// `State::refreshing` 对齐）。
    pub(super) fn refreshing(&self) -> bool {
        self.refresh || self.manual_in_flight
    }

    /// 悬浮层的强意图刷新：下一次 tick 立即发 `account.usage.refresh`。
    fn request_refresh(&mut self) {
        self.refresh = true;
        self.next_usage = Instant::now();
    }
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

/// `State::paint` 一次绘制的结果；hits/rect 由调用方通过 `commit_paint` 写回。
pub(super) struct Painted {
    /// 供 kitty 图片 occlusion 使用的覆盖区（进程对话框 > 悬浮层 > 页面）。
    pub covered: Rect,
    pub hits: Vec<(Rect, Action)>,
    pub page_rect: Rect,
    pub hover_rect: Rect,
    pub dialog: bool,
}

pub(super) struct State {
    /// 当前拥有键盘输入的页面：经典布局下即打开的页面；停靠工作台下仅当
    /// 监控 / 账号面板聚焦时为 `Some`，与 `dock.focused` 同步（不在渲染期改写）。
    pub page: Option<Page>,
    /// 用户在监控面板里选中的 tab；焦点回到终端后面板仍画这个 tab，并持久化。
    pub monitor_tab: Page,
    pub monitor: MonitorConfig,
    pub usage: AccountUsageConfig,
    pub metrics: Option<Box<SystemMetricsSnapshot>>,
    pub history: VecDeque<HistoryPoint>,
    pub providers: Vec<UsageProviderInfo>,
    /// 账号页 / 浮动仪表盘的账号快照（页面作用域）。
    pub accounts: Vec<AccountUsageSnapshot>,
    pub selected_provider: Option<String>,
    pub selected_account: Option<String>,
    /// 页面作用域里待绑定的 pane：只供 `Action::Bind` 使用，页面轮询不带它。
    ///
    /// 悬浮层不再写它，因此在 B-3 的 pane 选择器 /「绑定到聚焦 pane」入口接入
    /// 之前没有 `Some` 写入点：账号页底行的「确认账号绑定」按钮与
    /// `Action::Bind` 的页面分支是 B-3 的待接入点，悬浮层仍是唯一绑定入口。
    pub selected_pane: Option<String>,
    pub hover: Option<Hover>,
    /// 悬浮层作用域，见 `HoverScope`。
    pub hover_scope: HoverScope,
    /// 页面作用域的 `account.usage.refresh` 在途；响应到达即清除。与
    /// `refresh_usage`（排队）一起构成 `refreshing()`。
    manual_in_flight: bool,
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
    /// 活动端点的 (端点, boot_id)；按引用比较，tick 内不分配（OBS-13）。
    source: Option<(ClientEndpointId, String)>,
    /// 用量轮询看到的 (活动端点, boot_id)；快照缺失时为 `None`，变化即作废
    /// 页面作用域（覆盖同 boot 断线重连的情形，`source` 不会变）。
    usage_source: Option<(ClientEndpointId, String)>,
    next_metrics: Instant,
    /// 页面作用域下一次轮询时刻；悬浮层用 `hover_scope.next_usage`。
    next_usage: Instant,
    /// 厂商列表的低频重拉时刻：页面打开时立即，之后每 5 分钟（OBS-14）。
    next_providers: Instant,
    /// 页面作用域排队中的强意图刷新：请求真正发出后才清除，被在途请求挡下时
    /// 保留并在 200 ms 后重试；端点不支持 / 厂商已关闭时清除（回落到普通 get）。
    refresh_usage: bool,
    /// 账号页打开时尚无厂商列表：列表到达后按聚焦 pane 的 agent / 首个已安装
    /// 厂商补选一次。
    auto_select_provider: bool,
    /// 上一 tick 账号页是否可见：由不可见变可见（含偏好恢复 / 面板随布局
    /// 恢复）且尚未选厂商时触发自动选中。
    accounts_page_seen: bool,
    /// 已在页脚说明过「所选主机不在线」的目标端点：回退目标不变时不重写页脚，
    /// 避免每 2 秒刷掉其它一次性提示。
    fallback_noted: Option<ClientEndpointId>,
    pending: HashSet<&'static str>,
    alerts: HashMap<String, AlertState>,
}

impl State {
    /// 页面作用域处于「刷新中」：强意图刷新已排队或正在途中。渲染期据此把旧
    /// 数据变暗、空态显示「刷新中…」、「刷新」按钮退化为状态提示。
    pub(super) fn refreshing(&self) -> bool {
        self.refresh_usage || self.manual_in_flight
    }

    /// 滚动账号列表；`hover` 为真时按悬浮层作用域的账号数夹取并写悬浮层自己的
    /// 滚动位置，否则按页面。
    fn scroll_accounts(&mut self, delta: isize, hover: bool) {
        let accounts = if hover {
            &self.hover_scope.accounts
        } else {
            &self.accounts
        };
        let limit = render::account_rows(self, accounts).saturating_sub(1);
        let scroll = if hover {
            &mut self.hover_scope.scroll
        } else {
            &mut self.account_scroll
        };
        *scroll = scroll.saturating_add_signed(delta).min(limit);
    }

    /// 页面作用域换代：作废所有在途页面请求（悬浮层的请求不受影响）。
    fn bump_page_epoch(&mut self) {
        self.epoch = self.epoch.saturating_add(1);
        self.pending.retain(|key| Purpose::HOVER_KEYS.contains(key));
        self.manual_in_flight = false;
    }

    /// 页面作用域的强意图刷新：下一次 tick 立即发 `account.usage.refresh`。
    fn request_refresh(&mut self) {
        self.refresh_usage = true;
        self.next_usage = Instant::now();
    }

    /// 复位悬浮层作用域并换代，作废在途悬浮层请求。
    fn reset_hover_scope(&mut self) {
        let epoch = self.hover_scope.epoch.saturating_add(1);
        self.hover_scope = HoverScope {
            epoch,
            ..HoverScope::default()
        };
        self.pending
            .retain(|key| !Purpose::HOVER_KEYS.contains(key));
    }

    /// 结束悬浮层：hover 本身与悬浮层作用域一并复位，页面作用域
    /// （`selected_*` / `accounts` / `epoch` / `account_scroll`）保持不动。
    pub(super) fn clear_hover(&mut self) {
        self.hover = None;
        self.reset_hover_scope();
    }

    /// 「切换账号」的候选：选中厂商时为其已配置账号；总览态为所有已列出
    /// 厂商的已配置账号（顺序稳定，不受当前账号快照影响）。设置里关闭的厂商
    /// 两种情况下都不参与。渲染期只取长度，因此返回迭代器而不分配。
    pub(super) fn cycle_candidates<'a>(
        &'a self,
        provider: Option<&'a str>,
    ) -> impl Iterator<Item = &'a str> + 'a {
        self.providers
            .iter()
            .filter(move |candidate| match provider {
                Some(agent) => candidate.agent == agent,
                None => provider_listed(candidate),
            })
            .filter(|candidate| !self.usage.disabled_providers.contains(&candidate.agent))
            .flat_map(|candidate| candidate.configured_accounts.iter().map(String::as_str))
    }

    /// 作用域内可操作的账号：显式选中且属于该作用域，否则该作用域唯一的账号。
    fn scoped_account(&self, hover: bool) -> Option<String> {
        let accounts = if hover {
            &self.hover_scope.accounts
        } else {
            &self.accounts
        };
        self.selected_account
            .clone()
            .filter(|selected| {
                !hover
                    || accounts
                        .iter()
                        .any(|account| &account.account_id == selected)
            })
            .or_else(|| (accounts.len() == 1).then(|| accounts[0].account_id.clone()))
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
        if let Some(value) = config.preferences.monitor_tab {
            self.monitor_tab = value;
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
            monitor_tab: config.preferences.monitor_tab.unwrap_or(Page::Monitor),
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
            hover: None,
            hover_scope: HoverScope::default(),
            manual_in_flight: false,
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
            source: None,
            usage_source: None,
            next_metrics: Instant::now(),
            next_usage: Instant::now(),
            next_providers: Instant::now(),
            refresh_usage: false,
            auto_select_provider: false,
            accounts_page_seen: false,
            fallback_noted: None,
            pending: HashSet::new(),
            alerts: HashMap::new(),
        }
    }

    /// 每帧绘制前复位上一帧的命中区与矩形；之后每次 `paint` 的结果经
    /// `commit_paint` 累加写回。
    pub(super) fn begin_paint(&mut self) {
        self.hits.clear();
        self.hover_hits.clear();
        self.hover_rect = Rect::default();
        self.page_rect = Rect::default();
    }

    /// 渲染纯函数：把 `painting_page`（停靠面板传该面板的 tab，全局浮层传 `None`）、
    /// 可见的悬浮层与进程对话框画进 `frame` 的 `area`，状态只读。
    ///
    /// 光标归属按覆盖区判定：只有页面矩形、悬浮层矩形或对话框矩形真正包含
    /// `frame.cursor` 坐标时才把整帧光标置空；停靠在旁边的监控面板不能抹掉
    /// 聚焦终端的插入点（经典布局下页面铺满 pane 区域，行为不变）。
    pub(super) fn paint(
        &self,
        frame: &mut FrameData,
        area: Rect,
        palette: &Palette,
        painting_page: Option<Page>,
    ) -> Option<Painted> {
        let visible = painting_page.is_some()
            || self.process_dialog.is_some()
            || self.hover.as_ref().is_some_and(|hover| hover.visible);
        if !visible {
            return None;
        }
        let mut buffer = frame.to_ratatui_buffer()?;
        let output = render::paint(&mut buffer, area, self, palette, painting_page);
        if painting_page.is_some() {
            let selected = self.selected_hit.min(output.hits.len().saturating_sub(1));
            if let Some((rect, _)) = output.hits.get(selected) {
                buffer.set_style(
                    *rect,
                    Style::default()
                        .fg(palette.accent)
                        .add_modifier(Modifier::UNDERLINED),
                );
            }
        }
        let cursor = frame.cursor.clone().filter(|cursor| {
            let point = (cursor.x, cursor.y);
            !(contains(output.page_rect, point)
                || contains(output.hover_rect, point)
                || contains(output.dialog_rect, point))
        });
        frame.replace_from_ratatui_buffer_preserving_effects(&buffer, cursor);
        let dialog = !output.dialog_rect.is_empty();
        let covered = if dialog {
            output.dialog_rect
        } else if !output.hover_rect.is_empty() {
            output.hover_rect
        } else {
            output.page_rect
        };
        Some(Painted {
            covered,
            hits: output.hits,
            page_rect: output.page_rect,
            hover_rect: output.hover_rect,
            dialog,
        })
    }

    /// 把一次绘制的命中区写回：页面命中区累加（多个停靠面板依次绘制），
    /// 悬浮层命中区单独记录以便浮层独占输入，进程对话框出现时独占全部命中区。
    pub(super) fn commit_paint(&mut self, painted: Painted) {
        if painted.dialog {
            self.hits = painted.hits;
            self.hover_rect = Rect::default();
            self.hover_hits.clear();
        } else {
            if !painted.hover_rect.is_empty() {
                self.hover_hits = painted.hits.clone();
                self.hover_rect = painted.hover_rect;
            }
            self.hits.extend(painted.hits);
        }
        if !painted.page_rect.is_empty() {
            self.page_rect = painted.page_rect;
        }
        self.selected_hit = self.selected_hit.min(self.hits.len().saturating_sub(1));
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
    pub(super) fn toggle_usage_dashboard(&mut self, outcome: &mut ClientShellInput) {
        if matches!(self.overlay, Some(ClientShellOverlay::UsageDashboard)) {
            self.overlay = None;
        } else {
            self.overlay = Some(ClientShellOverlay::UsageDashboard);
            // 仪表盘是跨厂商总览：悬浮层作用域整体复位，页面选择回到总览态并
            // 换代，打开本身视为强意图刷新；厂商列表也顺手重拉一次。
            self.observability.clear_hover();
            self.observability.auto_select_provider = false;
            self.observability.account_scroll = 0;
            self.observability.bump_page_epoch();
            self.observability.selected_provider = None;
            self.observability.selected_account = None;
            self.observability.selected_pane = None;
            self.observability.request_refresh();
            self.observability.next_providers = Instant::now();
        }
        outcome.repaint = true;
    }

    /// 显式选择厂商（用户点击或账号页打开时的自动选中）：页面作用域换代、
    /// 强意图刷新；旧账号快照保留到新数据到达（渲染期变暗）。
    fn select_usage_provider(&mut self, agent: String) {
        self.observability.account_scroll = 0;
        self.observability.bump_page_epoch();
        self.observability.selected_provider = Some(agent);
        self.observability.selected_account = None;
        self.observability.selected_pane = None;
        self.observability.request_refresh();
    }

    /// 账号页可见且尚未选厂商时：按聚焦 pane 正在运行的 agent 选，否则选第一个
    /// 已列出（已安装且未在设置里关闭）的厂商；厂商列表未到时保留标志，列表
    /// 到达后补选。
    fn auto_select_usage_provider(&mut self) {
        if !self.observability.auto_select_provider {
            return;
        }
        if self.observability.selected_provider.is_some() {
            self.observability.auto_select_provider = false;
            return;
        }
        let listed = self
            .observability
            .providers
            .iter()
            .filter(|provider| provider_listed(provider))
            .filter(|provider| {
                !self
                    .observability
                    .usage
                    .disabled_providers
                    .contains(&provider.agent)
            })
            .map(|provider| provider.agent.clone())
            .collect::<Vec<_>>();
        let Some(first) = listed.first().cloned() else {
            return;
        };
        // 停靠工作台下打开页面时焦点已在监控面板，视图焦点为空；回退到
        // 快照里服务端记录的聚焦 pane。
        let focused = self
            .focused_pane_id()
            .or_else(|| self.snapshot.as_ref()?.focused_pane_id.clone())
            .and_then(|pane_id| {
                self.snapshot
                    .as_ref()?
                    .agents
                    .iter()
                    .find(|agent| agent.pane_id == pane_id)?
                    .agent
                    .clone()
            });
        let pick = focused
            .filter(|agent| listed.iter().any(|listed| listed == agent))
            .unwrap_or(first);
        self.observability.auto_select_provider = false;
        self.select_usage_provider(pick);
    }

    pub(super) fn open_observation_page(&mut self, page: Page, outcome: &mut ClientShellInput) {
        // One dock panel hosts system, accounts, and settings pages; the
        // requested page becomes the active tab inside it and is remembered
        // across focus changes (persisted as a client preference).
        let tab_changed = self.config.preferences.monitor_tab != Some(page);
        self.observability.monitor_tab = page;
        self.workbench_open(dock::PanelId::Monitor);
        self.observability.page = Some(page);
        if tab_changed {
            self.config.preferences.monitor_tab = Some(page);
            self.persist_chrome_preferences(outcome);
        }
        self.observability.clear_hover();
        self.observability.scroll = 0;
        self.observability.next_metrics = Instant::now();
        self.observability.next_usage = Instant::now();
        self.observability.next_providers = Instant::now();
        self.observability.message = None;
        if page == Page::Accounts && self.observability.selected_provider.is_none() {
            self.observability.auto_select_provider = true;
            self.auto_select_usage_provider();
        }
        self.selection = None;
        self.clear_link_hover();
        self.mode = ClientShellMode::Terminal;
        outcome.repaint = true;
    }

    /// 发出一次观测请求；结局见 `RequestOutcome`：被在途请求挡下（`Busy`）时
    /// 调用方保留强意图刷新标志稍后重试，端点离线 / 未宣告方法 / 无快照
    /// （`Unavailable`）时清除标志，只禁用该动作而不让轮询停摆。
    ///
    /// 悬浮层作用域的请求（`Purpose::is_hover`：用量、绑定、官方回调）走
    /// `hover_scope.endpoint` 与 `hover_scope.epoch`，其余走活动端点与页面
    /// `epoch`；目标端点不在线时回退到活动端点，并在回退目标变化时在页脚说明。
    fn observation_request(
        &mut self,
        method: Method,
        purpose: Purpose,
        outcome: &mut ClientShellInput,
    ) -> RequestOutcome {
        let key = purpose.key();
        if self.observability.pending.contains(key) {
            return RequestOutcome::Busy;
        }
        let preferred = if purpose.is_hover() {
            self.observability.hover_scope.endpoint.clone()
        } else {
            None
        }
        .unwrap_or_else(|| self.active_endpoint_id.clone());
        let online = |endpoints: &[ClientShellEndpoint], id: &ClientEndpointId| {
            endpoints.iter().any(|endpoint| {
                &endpoint.endpoint_id == id && endpoint.status == ClientEndpointStatus::Online
            })
        };
        let endpoint_id = if online(&self.endpoints, &preferred) {
            if self.observability.fallback_noted.as_ref() == Some(&preferred) {
                // 目标端点回来了：下次再离线时重新说明。
                self.observability.fallback_noted = None;
            }
            preferred
        } else if preferred != self.active_endpoint_id
            && online(&self.endpoints, &self.active_endpoint_id)
        {
            if self.observability.fallback_noted.as_ref() != Some(&preferred) {
                self.observability.message = Some(
                    tr(
                        "The selected host is offline; querying the active host instead.",
                        "所选主机不在线，已改为查询当前主机。",
                    )
                    .into(),
                );
                self.observability.fallback_noted = Some(preferred);
            }
            self.active_endpoint_id.clone()
        } else {
            return RequestOutcome::Unavailable;
        };
        let Some(endpoint) = self
            .endpoints
            .iter()
            .find(|endpoint| endpoint.endpoint_id == endpoint_id)
        else {
            return RequestOutcome::Unavailable;
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
            return RequestOutcome::Unavailable;
        }
        let Some(snapshot) = endpoint.snapshot.as_ref() else {
            return RequestOutcome::Unavailable;
        };
        let request_id = format!("client-shell:{}", self.next_request_id);
        self.next_request_id = self.next_request_id.saturating_add(1);
        let boot_id = snapshot.boot_id.clone();
        let epoch = if purpose.is_hover() {
            self.observability.hover_scope.epoch
        } else {
            self.observability.epoch
        };
        self.pending_requests.insert(
            request_id.clone(),
            PendingEndpointRequest {
                boot_id: boot_id.clone(),
                method_name: crate::api::api_method_name(&method).into(),
                confirmation_workspace_id: None,
                kind: PendingEndpointKind::Observation {
                    epoch,
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
        RequestOutcome::Sent
    }

    /// 发一次页面作用域的用量请求：`manual` 为真时走 `account.usage.refresh`。
    fn page_usage_request(
        &mut self,
        manual: bool,
        outcome: &mut ClientShellInput,
    ) -> RequestOutcome {
        // 页面轮询永不带 pane_id：pane 只属于 `Action::Bind`。
        let params = UsageParams {
            agent: self.observability.selected_provider.clone(),
            account_id: self.observability.selected_account.clone(),
            pane_id: None,
        };
        let method = if manual {
            Method::AccountUsageRefresh(params)
        } else {
            Method::AccountUsageGet(params)
        };
        self.observation_request(method, Purpose::Usage, outcome)
    }

    /// 发一次悬浮层作用域的用量请求（带 hover 的 pane）。
    fn hover_usage_request(
        &mut self,
        manual: bool,
        outcome: &mut ClientShellInput,
    ) -> RequestOutcome {
        let params = UsageParams {
            agent: self.observability.hover_scope.provider.clone(),
            account_id: None,
            pane_id: self.observability.hover_scope.pane.clone(),
        };
        let method = if manual {
            Method::AccountUsageRefresh(params)
        } else {
            Method::AccountUsageGet(params)
        };
        self.observation_request(method, Purpose::HoverUsage, outcome)
    }

    pub(crate) fn is_observation_request(&self, id: &str) -> bool {
        self.pending_requests
            .get(id)
            .is_some_and(|request| matches!(request.kind, PendingEndpointKind::Observation { .. }))
    }

    pub(crate) fn tick_observability(&mut self, now: Instant, outcome: &mut ClientShellInput) {
        // 活动 source（端点 + boot_id）按引用比较，只有变化时才分配（OBS-13）。
        let source_changed = self.snapshot.as_ref().is_some_and(|snapshot| {
            !self
                .observability
                .source
                .as_ref()
                .is_some_and(|(endpoint, boot_id)| {
                    endpoint == &self.active_endpoint_id && boot_id == &snapshot.boot_id
                })
        });
        if source_changed {
            self.observability.source = self
                .snapshot
                .as_ref()
                .map(|snapshot| (self.active_endpoint_id.clone(), snapshot.boot_id.clone()));
            // 大清理：新端点 / 新 boot，此前的观测数据、两个作用域与在途请求全部作废。
            self.observability.epoch = self.observability.epoch.saturating_add(1);
            self.observability.metrics = None;
            self.observability.history.clear();
            self.observability.accounts.clear();
            self.observability.providers.clear();
            self.observability.pending.clear();
            self.observability.clear_hover();
            self.observability.refresh_usage = false;
            self.observability.manual_in_flight = false;
            self.observability.fallback_noted = None;
            self.observability.process_dialog = None;
            self.observability.next_metrics = now;
            self.observability.next_usage = now;
            self.observability.next_providers = now;
        }
        let previous_second = self.observability.now_ms / 1000;
        self.observability.now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64;
        // 悬浮层状态机：离开延时到期即结束；停留满 hover_delay_ms 则可见，并把
        // 目标写进悬浮层作用域（页面作用域不动）。
        enum HoverStep {
            Leave,
            Show,
        }
        let hover_delay =
            Duration::from_millis(self.observability.usage.hover_delay_ms.clamp(200, 2000));
        let step = self.observability.hover.as_ref().and_then(|hover| {
            if hover.leave_at.is_some_and(|at| now >= at) {
                Some(HoverStep::Leave)
            } else if !hover.visible && now.duration_since(hover.since) >= hover_delay {
                Some(HoverStep::Show)
            } else {
                None
            }
        });
        match step {
            Some(HoverStep::Leave) => {
                self.observability.clear_hover();
                outcome.repaint = true;
            }
            Some(HoverStep::Show) => {
                let target = self.observability.hover.as_mut().map(|hover| {
                    hover.visible = true;
                    (
                        hover.agent.clone(),
                        hover.endpoint_id.clone(),
                        hover.pane.clone(),
                    )
                });
                if let Some((agent, endpoint_id, pane)) = target {
                    self.observability.reset_hover_scope();
                    self.observability.hover_scope.provider = Some(agent);
                    self.observability.hover_scope.endpoint = Some(endpoint_id);
                    self.observability.hover_scope.pane = Some(pane);
                    // 悬浮层首次可见 = 悬浮层自己的强意图刷新；页面的滚动位置与
                    // 轮询节奏都不动。
                    self.observability.hover_scope.request_refresh();
                    outcome.repaint = true;
                }
            }
            None => {}
        }
        let hover_visible = self
            .observability
            .hover
            .as_ref()
            .is_some_and(|hover| hover.visible);
        // 账号页（经典布局页面 / 停靠面板的账号 tab / legacy 账号面板）可见；
        // 浮动仪表盘是跨厂商总览，单列出来以免触发自动选厂商。
        let accounts_page_visible = self.observability.page == Some(Page::Accounts)
            || (self.workbench.visible(&dock::PanelId::Monitor)
                && self.observability.monitor_tab == Page::Accounts)
            || self.workbench.visible(&dock::PanelId::Accounts);
        let page_visible = accounts_page_visible
            || matches!(self.overlay, Some(ClientShellOverlay::UsageDashboard));
        let usage_visible = page_visible || hover_visible;
        // 账号页由不可见变可见（含偏好恢复 monitor_tab=accounts、面板随布局
        // 恢复，这些路径不经过 open_observation_page）且尚未选厂商：自动选中。
        if accounts_page_visible
            && !self.observability.accounts_page_seen
            && self.observability.selected_provider.is_none()
        {
            self.observability.auto_select_provider = true;
            self.auto_select_usage_provider();
        }
        self.observability.accounts_page_seen = accounts_page_visible;
        // 用量轮询的 source：活动端点 + 其快照的 boot_id，快照缺失时为 None；
        // 与 `source` 的差别在于同 boot 断线重连也会作废页面作用域。
        let usage_changed = {
            let boot_id = self
                .snapshot
                .as_ref()
                .map(|snapshot| snapshot.boot_id.as_str());
            let current = boot_id.map(|boot_id| (&self.active_endpoint_id, boot_id));
            let saved = self
                .observability
                .usage_source
                .as_ref()
                .map(|(endpoint, boot_id)| (endpoint, boot_id.as_str()));
            (current != saved)
                .then(|| current.map(|(endpoint, boot_id)| (endpoint.clone(), boot_id.to_owned())))
        };
        if let Some(usage_source) = usage_changed {
            self.observability.usage_source = usage_source;
            self.observability.bump_page_epoch();
            self.observability.accounts.clear();
            self.observability.providers.clear();
            self.observability.next_usage = now;
            self.observability.next_providers = now;
        }
        if usage_visible || self.observation_surface_visible() {
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
        let settings_open = self.observability.page == Some(Page::Settings);
        // 页面与悬浮层各有自己的轮询时刻：悬浮层出现 / 点「刷新」只唤醒悬浮层，
        // 页面的 2 秒节流不受影响；反之亦然。
        let page_due = (page_visible || settings_open) && now >= self.observability.next_usage;
        let hover_due = hover_visible && now >= self.observability.hover_scope.next_usage;
        if self.observability.usage.enabled && (page_due || hover_due) {
            // 厂商列表：首次、页面打开时与每 5 分钟低频重拉（OBS-14）。
            if (self.observability.providers.is_empty() || now >= self.observability.next_providers)
                && self.observation_request(
                    Method::AccountUsageProviders(EmptyParams::default()),
                    Purpose::Providers,
                    outcome,
                ) == RequestOutcome::Sent
            {
                self.observability.next_providers = now + Duration::from_secs(300);
            }
            let cadence = |retry_soon: bool| {
                now + if retry_soon {
                    Duration::from_millis(200)
                } else {
                    Duration::from_secs(2)
                }
            };
            if page_due {
                let disabled = self
                    .observability
                    .selected_provider
                    .as_ref()
                    .is_some_and(|agent| {
                        self.observability.usage.disabled_providers.contains(agent)
                    });
                let mut retry_soon = false;
                if disabled {
                    // 设置里关掉的厂商不发请求；排队中的强意图刷新必须消费掉，
                    // 否则页面永远停在「刷新中…」且「刷新」按钮失去命中区。
                    if std::mem::take(&mut self.observability.refresh_usage) {
                        self.observability.message = Some(
                            tr(
                                "This provider is disabled in settings.",
                                "此厂商已在设置中关闭，可在设置页重新启用。",
                            )
                            .into(),
                        );
                    }
                } else {
                    let manual = self.observability.refresh_usage;
                    let mut sent = self.page_usage_request(manual, outcome);
                    if manual && sent == RequestOutcome::Unavailable {
                        // 端点未宣告 account.usage.refresh（或此刻不可用）：只禁用
                        // 手动刷新，同一轮回落到普通 get，轮询不停摆。
                        self.observability.refresh_usage = false;
                        sent = self.page_usage_request(false, outcome);
                    }
                    match sent {
                        RequestOutcome::Sent => {
                            // 只在请求真正发出后清除强意图；发出的是 refresh 时进入在途。
                            if std::mem::take(&mut self.observability.refresh_usage) {
                                self.observability.manual_in_flight = true;
                            }
                        }
                        // 被在途请求挡下的强意图刷新不能丢：保留标志并 200 ms 后重试。
                        RequestOutcome::Busy => retry_soon = self.observability.refresh_usage,
                        RequestOutcome::Unavailable => {}
                    }
                }
                self.observability.next_usage = cadence(retry_soon);
            }
            if hover_due {
                let disabled = self
                    .observability
                    .hover_scope
                    .provider
                    .as_ref()
                    .is_some_and(|agent| {
                        self.observability.usage.disabled_providers.contains(agent)
                    });
                let mut retry_soon = false;
                if disabled {
                    self.observability.hover_scope.refresh = false;
                } else {
                    let manual = self.observability.hover_scope.refresh;
                    let mut sent = self.hover_usage_request(manual, outcome);
                    if manual && sent == RequestOutcome::Unavailable {
                        self.observability.hover_scope.refresh = false;
                        sent = self.hover_usage_request(false, outcome);
                    }
                    match sent {
                        RequestOutcome::Sent => {
                            if std::mem::take(&mut self.observability.hover_scope.refresh) {
                                self.observability.hover_scope.manual_in_flight = true;
                            }
                        }
                        RequestOutcome::Busy => retry_soon = self.observability.hover_scope.refresh,
                        RequestOutcome::Unavailable => {}
                    }
                }
                self.observability.hover_scope.next_usage = cadence(retry_soon);
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
        // 悬浮层请求按悬浮层作用域的代际判旧，其余按页面代际。
        let expected = if purpose.is_hover() {
            self.observability.hover_scope.epoch
        } else {
            self.observability.epoch
        };
        if epoch != expected {
            return false;
        }
        self.observability.pending.remove(purpose.key());
        // 用量响应（成功或失败）到达即结束该作用域的在途手动刷新；排队中的下一次
        // 强意图（`refresh_usage` / `hover_scope.refresh`）不受影响，仍显示刷新中。
        match purpose {
            Purpose::Usage => self.observability.manual_in_flight = false,
            Purpose::HoverUsage => self.observability.hover_scope.manual_in_flight = false,
            _ => {}
        }
        match result {
            Ok(ResponseResult::SystemMetrics { snapshot }) => {
                self.observability.apply_metrics(snapshot)
            }
            Ok(ResponseResult::AccountUsageProviders { providers }) => {
                self.observability.providers = providers;
                self.auto_select_usage_provider();
            }
            Ok(ResponseResult::AccountUsage { accounts }) => {
                // 响应只写自己的作用域，且不反写用户选择的厂商。
                if matches!(purpose, Purpose::HoverUsage) {
                    self.observability.hover_scope.accounts = accounts;
                } else {
                    self.observability.accounts = accounts;
                }
            }
            Ok(ResponseResult::AccountBinding { account_id, .. }) => {
                self.observability.message = None;
                if matches!(purpose, Purpose::HoverBinding) {
                    // 悬浮层发起的绑定只刷新悬浮层：页面的厂商 / 账号选择不动，
                    // 否则会出现「页面厂商 claude + 账号 codex:default」这种服务端
                    // 双重过滤后必空的组合。
                    if self
                        .observability
                        .hover
                        .as_ref()
                        .is_some_and(|hover| hover.visible)
                    {
                        self.observability.hover_scope.request_refresh();
                    }
                } else {
                    // 页面发起的绑定回流 = 页面强意图刷新；被绑定账号所属厂商与
                    // 当前选中不一致时先切到该厂商，保证 agent / account 一致。
                    let owner = self
                        .observability
                        .providers
                        .iter()
                        .find(|provider| provider.configured_accounts.contains(&account_id))
                        .map(|provider| provider.agent.clone());
                    if let Some(agent) = owner.filter(|agent| {
                        Some(agent) != self.observability.selected_provider.as_ref()
                    }) {
                        self.select_usage_provider(agent);
                    }
                    self.observability.selected_account = Some(account_id);
                    self.observability.request_refresh();
                }
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
            Ok(ResponseResult::Ok {})
                if matches!(purpose, Purpose::Integration | Purpose::HoverIntegration) =>
            {
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
        self.observation_surface_visible()
    }

    /// 监控 / 账号数据当前是否有可见的呈现面：页面、悬浮层、停靠的监控或
    /// legacy 账号面板、浮动用量仪表盘。新响应到达时据此立即重绘，每秒 tick
    /// 据此刷新时间显示；与 `usage_visible` 的面板判据保持同一套。
    fn observation_surface_visible(&self) -> bool {
        self.observability.page.is_some()
            || self.observability.hover.is_some()
            || self.workbench.visible(&dock::PanelId::Monitor)
            || self.workbench.visible(&dock::PanelId::Accounts)
            || matches!(self.overlay, Some(ClientShellOverlay::UsageDashboard))
    }

    pub(super) fn observation_action(&mut self, action: Action, outcome: &mut ClientShellInput) {
        self.observation_action_in(action, false, outcome);
    }

    /// `from_hover` 标记动作来自悬浮层的命中区：读取账号列表的动作（绑定、官方
    /// 回调、官方来源、刷新、切换账号候选）按悬浮层作用域取数；选择类动作
    /// （选厂商 / 选账号 / 打开页面）本就是用户对页面的显式操作，直接落页面。
    fn observation_action_in(
        &mut self,
        action: Action,
        from_hover: bool,
        outcome: &mut ClientShellInput,
    ) {
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
                // 候选来自厂商的已配置账号（总览态为全部已列出厂商）；不足两个
                // 时是禁用态：不发请求，只在页脚说明。
                let provider = if from_hover {
                    self.observability.hover_scope.provider.clone()
                } else {
                    self.observability.selected_provider.clone()
                };
                let candidates = self
                    .observability
                    .cycle_candidates(provider.as_deref())
                    .map(str::to_owned)
                    .collect::<Vec<_>>();
                if candidates.len() <= 1 {
                    self.observability.message =
                        Some(tr("No other account to switch to.", "没有其它账号可切换。").into());
                } else {
                    if from_hover && provider != self.observability.selected_provider {
                        if let Some(agent) = provider {
                            self.select_usage_provider(agent);
                        }
                    }
                    let current = candidates
                        .iter()
                        .position(|id| Some(id) == self.observability.selected_account.as_ref());
                    let next = candidates
                        [current.map_or(0, |index| (index + 1) % candidates.len())]
                    .clone();
                    self.observation_action(Action::Account(next), outcome);
                }
            }
            Action::UsageIntegration(enabled) => {
                if let Some(account_id) = self.observability.scoped_account(from_hover) {
                    // 悬浮层发起的请求发往 hover 的端点（可能是远端主机）。
                    let purpose = if from_hover {
                        Purpose::HoverIntegration
                    } else {
                        Purpose::Integration
                    };
                    self.observation_request(
                        Method::AccountUsageIntegration(UsageIntegrationParams {
                            account_id,
                            enabled,
                        }),
                        purpose,
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
                self.observability.clear_hover();
                self.observability.process_dialog = None;
                if self.workbench.enabled {
                    // 关闭的是承载页面的面板（而非当前聚焦面板）：终端聚焦时
                    // 也能从命令面板关掉监控面板；锁定布局时给出反馈。
                    let panel = if self.workbench.dock.focused == dock::PanelId::Accounts {
                        dock::PanelId::Accounts
                    } else {
                        dock::PanelId::Monitor
                    };
                    self.close_workbench_panel(panel, outcome);
                } else {
                    self.observability.page = None;
                }
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
            Action::Provider(agent) => self.select_usage_provider(agent),
            Action::Account(account) => {
                // 显式选账号 = 强意图刷新；旧快照保留到新数据到达（变暗）。
                // 不清 selected_pane：它是页面绑定入口的前提。
                self.observability.account_scroll = 0;
                self.observability.bump_page_epoch();
                self.observability.selected_account = Some(account);
                self.observability.request_refresh();
            }
            Action::Bind => {
                // 页面分支的 `selected_pane` 在 B-3 接入 pane 选择器前没有写入点，
                // 恒走下面的提示；悬浮层分支是当前唯一可达的绑定入口。
                let pane_id = if from_hover {
                    self.observability.hover_scope.pane.clone()
                } else {
                    self.observability.selected_pane.clone()
                };
                match (pane_id, self.observability.scoped_account(from_hover)) {
                    (Some(pane_id), Some(account_id)) => {
                        // 悬浮层发起的绑定发往 hover 的端点，回流只刷新悬浮层。
                        let purpose = if from_hover {
                            Purpose::HoverBinding
                        } else {
                            Purpose::Binding
                        };
                        self.observation_request(
                            Method::AccountBindingSet(AccountBindingParams {
                                pane_id,
                                account_id,
                            }),
                            purpose,
                            outcome,
                        );
                    }
                    _ => {
                        self.observability.message = Some(
                            tr(
                                "Pick a pane and an account before binding.",
                                "请先选择要绑定的 pane 与账号。",
                            )
                            .into(),
                        );
                    }
                }
            }
            Action::Refresh => {
                // 各自只唤醒自己的作用域：悬浮层的「刷新」不提前触发页面轮询。
                if from_hover {
                    self.observability.hover_scope.request_refresh();
                } else {
                    self.observability.request_refresh();
                    self.observability.next_metrics = Instant::now();
                }
            }
            Action::Source => {
                let accounts = if from_hover {
                    &self.observability.hover_scope.accounts
                } else {
                    &self.observability.accounts
                };
                if let Some(url) = accounts
                    .first()
                    .map(|account| account.source_url.clone())
                    .filter(|url| crate::app::actions::safe_web_url(url).is_some())
                {
                    outcome.actions.push(ClientShellAction::OpenSafeWebUrl(url));
                }
            }
            Action::Process(identity) => {
                self.observation_request(
                    Method::SystemProcessGet(ProcessGetParams { identity }),
                    Purpose::Process,
                    outcome,
                );
            }
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
            if self.observability.hover.is_some() {
                self.observability.clear_hover();
            }
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
                    self.observability.scroll_accounts(1, true);
                    outcome.repaint = true;
                }
                MouseEventKind::ScrollUp => {
                    self.observability.scroll_accounts(-1, true);
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
                        self.observation_action_in(action, true, outcome);
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
                self.observability.scroll_accounts(
                    if mouse.kind == MouseEventKind::ScrollDown {
                        1
                    } else {
                        -1
                    },
                    false,
                );
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
        if matches!(mouse.kind, MouseEventKind::Down(_)) && self.observability.hover.is_some() {
            self.observability.clear_hover();
            outcome.repaint = true;
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
                self.observability.scroll_accounts(3, false);
                None
            }
            KeyCode::Up | KeyCode::PageUp if self.observability.page == Some(Page::Accounts) => {
                self.observability.scroll_accounts(-3, false);
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

    /// 经典布局：页面、悬浮层与进程对话框一次画进 pane 区域；页面打开时
    /// pane 命中区整体让位给页面。
    pub(super) fn paint_observability(
        &mut self,
        frame: &mut FrameData,
        area: Rect,
    ) -> Option<Rect> {
        self.observability.begin_paint();
        let painted =
            self.observability
                .paint(frame, area, &self.config.palette, self.observability.page)?;
        let covered = painted.covered;
        self.observability.commit_paint(painted);
        if self.observability.page.is_some() {
            self.hits.panes.clear();
            self.hits.pane_splits.clear();
        }
        Some(covered)
    }
}
