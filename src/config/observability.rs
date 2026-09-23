//! 监控和厂商用量配置：只保存凭据引用，不保存令牌。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MonitorConfig {
    pub interval_ms: u64,
    pub history_minutes: u16,
    pub visible: Vec<String>,
    pub card_height: u16,
    pub hidden_devices: Vec<String>,
    pub alerts_enabled: bool,
    pub alerts: Vec<MonitorAlertRule>,
}

impl Default for MonitorConfig {
    fn default() -> Self {
        Self {
            interval_ms: 1000,
            history_minutes: 15,
            visible: [
                "cpu",
                "cores",
                "memory",
                "gpu",
                "disks",
                "network",
                "sensors",
                "processes",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
            card_height: 10,
            hidden_devices: Vec::new(),
            alerts_enabled: false,
            alerts: vec![
                MonitorAlertRule::new("cpu", 90.0),
                MonitorAlertRule::new("memory", 90.0),
                MonitorAlertRule::new("gpu", 95.0),
                MonitorAlertRule::new("disk", 90.0),
            ],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MonitorAlertRule {
    pub metric: String,
    pub threshold: f64,
    pub duration_seconds: u64,
    pub cooldown_seconds: u64,
}

impl MonitorAlertRule {
    fn new(metric: &str, threshold: f64) -> Self {
        Self {
            metric: metric.into(),
            threshold,
            duration_seconds: 30,
            cooldown_seconds: 60,
        }
    }
}

impl Default for MonitorAlertRule {
    fn default() -> Self {
        Self::new("cpu", 90.0)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageDisplayFormat {
    #[default]
    Dashboard,
    Table,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageDisplayPosition {
    #[default]
    Hover,
    Page,
    Both,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AccountUsageConfig {
    pub enabled: bool,
    pub format: UsageDisplayFormat,
    pub position: UsageDisplayPosition,
    pub hover_delay_ms: u64,
    pub api_refresh_seconds: u64,
    pub cli_refresh_seconds: u64,
    pub probe_timeout_seconds: u64,
    /// 交互探测：显式刷新时在隔离 PTY 里启动官方 CLI 并输入 `/usage`（claude 用固定的
    /// `<state_dir>/account-usage/probe/<账号>` 目录，需用户在 CLI 中确认一次目录信任）。
    /// 默认关闭——它复用真实登录态，并发启动可能触发凭据轮换；开启后也只在显式刷新时
    /// 触发，自动轮询不会起 PTY。与之独立的是 claude 的非交互登录预检
    /// `claude auth status --json`：默认开启、只读地复用真实登录态，在等待回调期间按终态的
    /// 慢周期（10 min 起翻倍）运行，不受本开关控制。
    pub interactive_probe: bool,
    pub disabled_providers: Vec<String>,
    pub accounts: Vec<UsageAccountConfig>,
}

impl Default for AccountUsageConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            format: UsageDisplayFormat::Dashboard,
            position: UsageDisplayPosition::Hover,
            hover_delay_ms: 400,
            api_refresh_seconds: 60,
            cli_refresh_seconds: 300,
            probe_timeout_seconds: 20,
            interactive_probe: false,
            disabled_providers: Vec::new(),
            accounts: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UsageAccountConfig {
    pub id: String,
    pub label: String,
    pub agent: String,
    pub provider: String,
    pub auth_mode: String,
    pub profile_dir: Option<std::path::PathBuf>,
    pub credential_env: Option<String>,
    pub organization: Option<String>,
    /// 已弃用：没有任何厂商再用它。仍接受以免旧配置被整段拒收，填了它的账号在
    /// [`diagnostics`] 里得到一条弃用提示。
    pub account_user: Option<String>,
    pub billing_scope: Option<String>,
    pub base_url: Option<String>,
}

pub(crate) fn diagnostics(monitor: &MonitorConfig, usage: &AccountUsageConfig) -> Vec<String> {
    let mut messages = Vec::new();
    if ![500, 1000, 2000, 5000].contains(&monitor.interval_ms) {
        messages.push("monitor.interval_ms 必须为 500、1000、2000 或 5000".into());
    }
    if !(1..=60).contains(&monitor.history_minutes) {
        messages.push("monitor.history_minutes 必须介于 1 和 60".into());
    }
    let mut ids = std::collections::HashSet::new();
    for account in &usage.accounts {
        if account.id.is_empty() || account.id.len() > 128 || !ids.insert(&account.id) {
            messages.push("account_usage.accounts 的 id 必须非空且唯一，长度不超过 128".into());
        }
        if account.credential_env.as_ref().is_some_and(|name| {
            name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        }) {
            messages.push("account_usage.accounts.credential_env 必须是环境变量名称".into());
        }
        if account.account_user.is_some() {
            messages.push(format!(
                "account_usage.accounts.account_user is deprecated and ignored (account '{}'); no provider uses it, remove it",
                account.id
            ));
        }
    }
    messages
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_configuration_keeps_bounded_defaults_and_no_credentials() {
        let config: AccountUsageConfig = toml::from_str("").unwrap();
        assert_eq!(config.api_refresh_seconds, 60);
        assert_eq!(config.cli_refresh_seconds, 300);
        assert!(config.accounts.is_empty());
        assert!(!MonitorConfig::default().alerts_enabled);
        // 交互探测会复用真实登录态，旧配置与默认配置都必须保持关闭。
        assert!(!config.interactive_probe);
        assert!(!AccountUsageConfig::default().interactive_probe);
        let opted_in: AccountUsageConfig = toml::from_str("interactive_probe = true").unwrap();
        assert!(opted_in.interactive_probe);
    }

    /// 文档终审 D13：`account_user` 已没有任何厂商使用。字段仍保留，旧配置不会因它被
    /// 整段拒收；填了它的账号给一条弃用诊断，提示删掉。没填的不报。
    #[test]
    fn account_user_is_accepted_but_reported_as_deprecated() {
        let mut config: AccountUsageConfig = toml::from_str(
            r#"
[[accounts]]
id = "work"
agent = "codex"
account_user = "me"
"#,
        )
        .unwrap();
        assert_eq!(config.accounts[0].account_user.as_deref(), Some("me"));
        let messages = diagnostics(&MonitorConfig::default(), &config);
        assert_eq!(messages.len(), 1, "{messages:?}");
        assert!(
            messages[0].contains("account_user") && messages[0].contains("work"),
            "{messages:?}"
        );
        config.accounts[0].account_user = None;
        assert!(diagnostics(&MonitorConfig::default(), &config).is_empty());
    }

    #[test]
    fn duplicate_accounts_and_literal_credentials_are_rejected() {
        let mut config = AccountUsageConfig::default();
        let account = UsageAccountConfig {
            id: "work".into(),
            credential_env: Some("not an env var".into()),
            ..Default::default()
        };
        config.accounts = vec![account.clone(), account];
        assert_eq!(diagnostics(&MonitorConfig::default(), &config).len(), 3);
    }
}
