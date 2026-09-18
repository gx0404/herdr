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
