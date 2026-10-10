//! 硬件监控配置（`[monitor]`）。

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

/// 诊断文案按调用进程的界面语言给出（`herdr config check` 与界面共用）。
pub(crate) fn diagnostics(monitor: &MonitorConfig) -> Vec<String> {
    let texts = &crate::i18n::texts().monitor_config;
    let mut messages = Vec::new();
    if ![500, 1000, 2000, 5000].contains(&monitor.interval_ms) {
        messages.push(texts.interval_invalid.into());
    }
    if !(1..=60).contains(&monitor.history_minutes) {
        messages.push(texts.history_invalid.into());
    }
    messages
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monitor_defaults_keep_alerts_off() {
        let config: MonitorConfig = toml::from_str("").unwrap();
        assert_eq!(config, MonitorConfig::default());
        assert!(!config.alerts_enabled);
        assert!(diagnostics(&config).is_empty());
    }

    /// 诊断按界面语言给出：英文界面不含 CJK，中文界面是中文。
    #[test]
    fn diagnostics_follow_the_interface_language() {
        use crate::i18n::{has_cjk, lang_guard, Lang};
        let monitor = MonitorConfig {
            interval_ms: 3,
            history_minutes: 0,
            ..MonitorConfig::default()
        };
        for (lang, chinese) in [(Lang::En, false), (Lang::ZhCn, true)] {
            let _guard = lang_guard(lang);
            let messages = diagnostics(&monitor);
            assert_eq!(messages.len(), 2, "{messages:?}");
            for message in &messages {
                assert_eq!(has_cjk(message), chinese, "{lang:?}: {message}");
            }
        }
    }
}
