//! 系统监控与账号用量的公开 JSON 类型；不进入冻结的终端帧 codec。

use serde::{Deserialize, Serialize};

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ObservationStatus {
    Ready,
    #[default]
    Warming,
    Unsupported,
    PermissionDenied,
    NotAuthenticated,
    NeedsBinding,
    Unavailable,
    Stale,
    Error,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CpuCoreMetric {
    pub id: usize,
    pub name: String,
    pub usage_percent: Option<f32>,
    pub frequency_mhz: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MemoryMetric {
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub available_bytes: u64,
    pub swap_total_bytes: u64,
    pub swap_used_bytes: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GpuMetric {
    pub id: String,
    pub name: String,
    pub vendor: String,
    pub status: ObservationStatus,
    pub source: String,
    pub usage_percent: Option<f32>,
    pub memory_used_bytes: Option<u64>,
    pub memory_total_bytes: Option<u64>,
    pub shared_memory_used_bytes: Option<u64>,
    pub temperature_celsius: Option<f32>,
    pub power_watts: Option<f32>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DiskMetric {
    pub id: String,
    pub name: String,
    pub mount_point: String,
    pub total_bytes: u64,
    pub available_bytes: u64,
    pub read_bytes_per_second: Option<f64>,
    pub written_bytes_per_second: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NetworkMetric {
    pub id: String,
    pub received_bytes_per_second: Option<f64>,
    pub transmitted_bytes_per_second: Option<f64>,
    pub total_received_bytes: u64,
    pub total_transmitted_bytes: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SensorMetric {
    pub name: String,
    pub temperature_celsius: Option<f32>,
    pub critical_celsius: Option<f32>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub started_at: u64,
    pub boot_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_token: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProcessMetric {
    pub identity: ProcessIdentity,
    pub parent_pid: Option<u32>,
    pub name: String,
    pub cpu_percent: Option<f32>,
    pub memory_bytes: u64,
    pub status: String,
    pub user: Option<String>,
    pub protected: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SystemMetricsSnapshot {
    #[serde(default)]
    pub group_status: std::collections::HashMap<String, ObservationStatus>,
    #[serde(default)]
    pub group_sampled_at_ms: std::collections::HashMap<String, u64>,
    pub boot_id: String,
    pub sequence: u64,
    pub sampled_at_ms: u64,
    pub interval_ms: u64,
    pub hostname: String,
    pub operating_system: String,
    pub environment: String,
    pub uptime_seconds: u64,
    pub status: ObservationStatus,
    pub cpu_percent: Option<f32>,
    pub cpu_brand: String,
    pub physical_core_count: Option<usize>,
    pub cores: Vec<CpuCoreMetric>,
    pub memory: MemoryMetric,
    pub gpus: Vec<GpuMetric>,
    pub disks: Vec<DiskMetric>,
    pub networks: Vec<NetworkMetric>,
    pub sensors: Vec<SensorMetric>,
    pub processes: Vec<ProcessMetric>,
    pub messages: Vec<String>,
}

fn default_interval() -> u64 {
    1000
}
fn default_limit() -> usize {
    100
}
fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SystemMetricsParams {
    #[serde(default = "default_interval")]
    pub interval_ms: u64,
    #[serde(default)]
    pub include_processes: bool,
    #[serde(default)]
    pub groups: Vec<String>,
}

impl Default for SystemMetricsParams {
    fn default() -> Self {
        Self {
            interval_ms: default_interval(),
            include_processes: false,
            groups: Vec::new(),
        }
    }
}

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ProcessSort {
    #[default]
    Cpu,
    Memory,
    Name,
    Pid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProcessListParams {
    #[serde(default)]
    pub filter: String,
    #[serde(default)]
    pub sort: ProcessSort,
    #[serde(default = "default_true")]
    pub descending: bool,
    #[serde(default)]
    pub offset: usize,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

impl Default for ProcessListParams {
    fn default() -> Self {
        Self {
            filter: String::new(),
            sort: ProcessSort::Cpu,
            descending: true,
            offset: 0,
            limit: default_limit(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProcessGetParams {
    pub identity: ProcessIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProcessTerminateParams {
    pub identity: ProcessIdentity,
    #[serde(default)]
    pub action_token: Option<String>,
    #[serde(default)]
    pub force: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct UsageParams {
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub account_id: Option<String>,
    #[serde(default)]
    pub pane_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AccountBindingParams {
    pub pane_id: String,
    pub account_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct UsageIntegrationParams {
    pub account_id: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct UsageReportParams {
    #[serde(default)]
    pub account_id: String,
    #[serde(default)]
    pub pane_id: Option<String>,
    #[serde(default)]
    pub snapshot: AccountUsageSnapshot,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub official_payload: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct UsageMetric {
    pub id: String,
    pub label: String,
    pub unit: String,
    pub scope: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_value: Option<String>,
    pub used: Option<f64>,
    pub limit: Option<f64>,
    pub remaining: Option<f64>,
    pub used_percent: Option<f64>,
    /// 金额保留厂商十进制表示，不由请求次数推算。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub amount_decimal: Option<String>,
    pub resets_at: Option<u64>,
    pub window_seconds: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AccountUsageSnapshot {
    pub account_id: String,
    pub account_label: String,
    pub agent: String,
    pub provider: String,
    pub auth_mode: String,
    /// 官方返回的非凭据账号标识，用于识别 profile 切换登录。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_identity: Option<String>,
    pub plan: Option<String>,
    pub status: ObservationStatus,
    pub source: String,
    pub source_url: String,
    pub observed_at_ms: u64,
    pub metrics: Vec<UsageMetric>,
    pub message: Option<String>,
}

/// 未绑定 pane 的官方回调被拒后留下的待办：供客户端一键绑定。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct UsagePendingBinding {
    pub pane_id: String,
    pub agent: String,
    /// 该 agent 下可绑定的账号；为空表示该 agent 没有配置账号。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub candidates: Vec<String>,
    pub rejected_at_ms: u64,
}

/// 与 `AccountUsageSnapshot` 并行的刷新状态：快照类型进入冻结摘要，不能再加字段，
/// 所以探测进度、防抖与推断绑定都放在这里，按 `account_id` 对齐。旧 server 不返回。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct UsageRefreshState {
    pub account_id: String,
    /// 探测已派发且尚未完成（含排队中）。
    #[serde(default)]
    pub in_flight: bool,
    /// 探测已入队但查询线程还没开始执行。
    #[serde(default)]
    pub queued: bool,
    /// 最近一次显式刷新（`account.usage.refresh`）到达的时间，含被防抖的请求。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_at_ms: Option<u64>,
    /// 最近一次真正派发探测的时间；`observed_at_ms` 只在成功时更新。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempted_at_ms: Option<u64>,
    /// 下一次显式刷新会被接受的最早时间；缺省表示现在即可。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_allowed_at_ms: Option<u64>,
    /// 厂商要求的退避截止时间（HTTP 429），显式刷新也不豁免。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u64>,
    /// 请求带了未绑定 pane，但该 agent 只有一个账号：数据按唯一候选返回，绑定并未写入。
    #[serde(default)]
    pub binding_inferred: bool,
    /// 最近一次交互探测（显式刷新 + `account_usage.interactive_probe`）停在了官方 CLI 的
    /// 目录信任对话上，需要用户在自己的 CLI 中对快照 `message` 里给出的稳定探测目录确认一次
    /// 信任；herdr 不会代为应答。快照状态仍由登录预检决定（未登录 → `not_authenticated`，
    /// 已登录 → `needs_binding` 等待回调），探测成功、得到其它结果或官方回调被接受时清位。
    /// 回调闩锁下被保留的回调快照不会挂上它。
    #[serde(default)]
    pub trust_required: bool,
    /// 该厂商只能靠官方回调（statusline 等）得到额度样本：显式刷新不会产生新的额度样本，
    /// 但仍可能刷新登录 / 绑定占位（如 Claude 的非交互登录预检），所以它不是「刷新按钮无用」
    /// 的一刀切依据；客户端可据此说明「额度只来自回调」。
    #[serde(default)]
    pub callback_only: bool,
    /// 与该账号同 agent 的待办绑定（请求带 pane 时只看该 pane）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_binding: Option<UsagePendingBinding>,
    /// 该账号的官方回调（statusline 等）当前是否已接入 herdr：服务端读该账号的官方
    /// `settings.json` 判定，与 `account.usage.integration` 作用于同一个账号。厂商不支持
    /// 回调、文件无法解析或旧 server 时缺省，客户端把 `None` 当作未知（仍提供「启用」）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callback_enabled: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct UsageProviderInfo {
    pub agent: String,
    pub label: String,
    pub source_url: String,
    pub method: String,
    pub account_scope: String,
    pub minimum_interval_seconds: u64,
    pub configured_accounts: Vec<String>,
    /// Whether the provider's official CLI is installed on this host.
    /// Absent on older servers; clients must treat `None` as unknown and
    /// keep listing the provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed: Option<bool>,
    /// 服务端宣告该厂商支持官方回调开关（`account.usage.integration` 能改写其官方
    /// `settings.json`）。能力由服务端声明：旧 server 缺省为 `false`，客户端据此只隐藏
    /// 开关，不自行维护厂商名单。各账号的当前接入态在 `UsageRefreshState.callback_enabled`。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub supports_callback: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ObservationSubscriptionParams {
    #[serde(default)]
    pub subscription_id: Option<String>,
    #[serde(default)]
    pub interval_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ClientViewSpec {
    pub view_id: String,
    pub tab_id: String,
    pub cols: u16,
    pub rows: u16,
    #[serde(default)]
    pub focused: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ClientViewsSetParams {
    pub revision: u64,
    pub views: Vec<ClientViewSpec>,
}

/// 观测事件的种类：后台观测服务（`account.usage.subscribe` / `system.metrics.subscribe`）
/// 推送的 `event` 字段。与 `events.subscribe` 的 `EventKind` 平行，不共用同一张表——观测
/// 事件是合并丢帧语义（订阅者只保证拿到最新一帧），不进事件回放缓冲。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub enum ObservationEventKind {
    /// 账号快照已变化：探测完成、官方回调被接受或回调闩锁到期。
    #[serde(rename = "account.usage.updated")]
    AccountUsageUpdated,
    /// 探测已派发（`in_flight` 由假变真）；快照本身不变，只推刷新状态。
    #[serde(rename = "account.usage.refreshing")]
    AccountUsageRefreshing,
    /// 系统指标采样已更新。
    #[serde(rename = "system.metrics.updated")]
    SystemMetricsUpdated,
    /// 新 server 推送了本版本不认识的事件种类；客户端应忽略。不进生成的 schema。
    #[serde(other, rename = "unknown")]
    #[schemars(skip)]
    Unknown,
}

impl ObservationEventKind {
    pub fn dot_name(self) -> &'static str {
        match self {
            Self::AccountUsageUpdated => "account.usage.updated",
            Self::AccountUsageRefreshing => "account.usage.refreshing",
            Self::SystemMetricsUpdated => "system.metrics.updated",
            Self::Unknown => "unknown",
        }
    }
}

/// `account.usage.updated` 的负载：变化后的账号快照，及与之按 `account_id` 对齐的刷新
/// 状态（旧 server 省略）。`binding_inferred` / `pending_binding` 是按请求填的字段，事件里
/// 保持缺省，以 `account.usage.get` 响应为准。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AccountUsageUpdatedEvent {
    pub accounts: Vec<AccountUsageSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh: Option<Vec<UsageRefreshState>>,
}

/// `account.usage.refreshing` 的负载：刚派发探测的账号的刷新状态（`in_flight` 为真）。
/// 订阅者据此立即显示「正在读取」，不必等下一轮拉取；探测完成由 `account.usage.updated`
/// 收尾。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AccountUsageRefreshingEvent {
    pub refresh: Vec<UsageRefreshState>,
}

/// `system.metrics.updated` 的负载。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SystemMetricsUpdatedEvent {
    pub snapshot: Box<SystemMetricsSnapshot>,
}

/// 观测事件信封：线上形状为 `{"event": "<kind>", "data": {...}}`，与 `EventEnvelope` /
/// `SubscriptionEventEnvelope` 同形，字段名与类型化之前完全一致。用 serde 邻接标签让
/// `event` 直接判别 `data` 的类型，不做 untagged 的按序试探。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "event", content = "data")]
pub enum ObservationEventEnvelope {
    #[serde(rename = "account.usage.updated")]
    AccountUsageUpdated(AccountUsageUpdatedEvent),
    #[serde(rename = "account.usage.refreshing")]
    AccountUsageRefreshing(AccountUsageRefreshingEvent),
    #[serde(rename = "system.metrics.updated")]
    SystemMetricsUpdated(SystemMetricsUpdatedEvent),
}

impl ObservationEventEnvelope {
    pub fn kind(&self) -> ObservationEventKind {
        match self {
            Self::AccountUsageUpdated(_) => ObservationEventKind::AccountUsageUpdated,
            Self::AccountUsageRefreshing(_) => ObservationEventKind::AccountUsageRefreshing,
            Self::SystemMetricsUpdated(_) => ObservationEventKind::SystemMetricsUpdated,
        }
    }
}
