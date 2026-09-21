use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(1);

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct ClientRemoteCollapsedGroups {
    pub(super) profile_id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) collapsed_groups: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(super) struct ClientChromePreferences {
    #[serde(
        default,
        deserialize_with = "read_pages",
        skip_serializing_if = "std::collections::HashMap::is_empty"
    )]
    pub(super) pages: std::collections::HashMap<String, super::floating_pages::Window>,
    #[serde(
        default,
        deserialize_with = "read_layouts",
        skip_serializing_if = "std::collections::HashMap::is_empty"
    )]
    pub(super) layouts: std::collections::HashMap<String, super::dock::DockLayout>,
    /// 监控卡片配置；结构不合法（旧版本 / 手改）时按未设置处理，不让整份偏好失效。
    #[serde(
        default,
        deserialize_with = "read_lenient",
        skip_serializing_if = "Option::is_none"
    )]
    pub(super) monitor: Option<crate::config::MonitorConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) usage_enabled: Option<bool>,
    /// 未知枚举值 → None（枚举本体不加 `#[serde(other)]`，config.toml 的拼写错误
    /// 仍由配置诊断报出）。
    #[serde(
        default,
        deserialize_with = "read_lenient",
        skip_serializing_if = "Option::is_none"
    )]
    pub(super) usage_format: Option<crate::config::UsageDisplayFormat>,
    #[serde(
        default,
        deserialize_with = "read_lenient",
        skip_serializing_if = "Option::is_none"
    )]
    pub(super) usage_position: Option<crate::config::UsageDisplayPosition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) usage_disabled_providers: Option<Vec<String>>,
    /// 监控 → 设置 里改过的悬浮延时（ms）；未改过时沿用 config.toml 的
    /// `account_usage.hover_delay_ms`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) usage_hover_delay_ms: Option<u64>,
    /// 监控面板里用户选中的 tab；未知值按未设置处理，不让整份偏好失效。
    #[serde(
        default,
        deserialize_with = "read_monitor_tab",
        skip_serializing_if = "Option::is_none"
    )]
    pub(super) monitor_tab: Option<super::observability::Page>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) sidebar_width: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) sidebar_section_split: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) sidebar_collapsed: Option<bool>,
    #[serde(
        default,
        deserialize_with = "read_lenient",
        skip_serializing_if = "Option::is_none"
    )]
    pub(super) agent_panel_sort: Option<crate::config::AgentPanelSortConfig>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) collapsed_groups: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) remote_collapsed_groups: Vec<ClientRemoteCollapsedGroups>,
    /// 侧栏里折叠起来的端点（`ClientEndpointId::storage_key`）。与
    /// `collapsed_groups` 同族：重启/重新 attach 之后机器分组仍然保持折叠
    /// （STATE-05）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) collapsed_endpoints: Vec<String>,
    /// Command palette MRU ids (newest first), capped on write.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) palette_recent: Vec<String>,
}

impl ClientChromePreferences {
    /// usage_* 影子键里是否有本机覆盖：这些键分别遮住 config.toml `[account_usage]`
    /// 的 enabled / format / position / disabled_providers / hover_delay_ms，任一为
    /// Some 即该键不再跟随配置文件。
    pub(super) fn usage_overridden(&self) -> bool {
        self.usage_enabled.is_some()
            || self.usage_format.is_some()
            || self.usage_position.is_some()
            || self.usage_disabled_providers.is_some()
            || self.usage_hover_delay_ms.is_some()
    }

    /// 「恢复配置文件值」：整组清空 usage_* 影子键，让它们重新跟随 config.toml。
    pub(super) fn clear_usage_overrides(&mut self) {
        self.usage_enabled = None;
        self.usage_format = None;
        self.usage_position = None;
        self.usage_disabled_providers = None;
        self.usage_hover_delay_ms = None;
    }
}

fn read_pages<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<std::collections::HashMap<String, super::floating_pages::Window>, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(value
        .as_object()
        .into_iter()
        .flatten()
        .take(32)
        .filter_map(|(key, value)| {
            let window: super::floating_pages::Window =
                serde_json::from_value(value.clone()).ok()?;
            // 只认识已知浮层，并把旧版 Debug 名归一成稳定键（ARCH-01）。
            let kind = super::state::ClientShellOverlayKind::from_storage_key(key)?;
            (key.len() <= 64 && window.valid()).then(|| (kind.storage_key().to_owned(), window))
        })
        .collect())
}

fn read_monitor_tab<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<super::observability::Page>, D::Error> {
    read_lenient(deserializer)
}

/// 单字段容错：值解析失败（未知枚举值、结构不合法）时按未设置处理，只丢这一个
/// 字段，其余偏好照常生效。
fn read_lenient<'de, D: serde::Deserializer<'de>, T: serde::de::DeserializeOwned>(
    deserializer: D,
) -> Result<Option<T>, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).ok())
}

fn read_layouts<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<std::collections::HashMap<String, super::dock::DockLayout>, D::Error> {
    let raw = serde_json::Value::deserialize(deserializer)?;
    Ok(raw
        .as_object()
        .into_iter()
        .flatten()
        .take(128)
        .filter_map(|(id, value)| {
            let layout: super::dock::DockLayout = serde_json::from_value(value.clone()).ok()?;
            (id.len() <= 512 && layout.valid()).then(|| (id.clone(), layout))
        })
        .collect())
}

pub(super) fn path_for_local_endpoint(socket_path: &Path) -> PathBuf {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in socket_path.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    crate::config::state_dir()
        .join("client-shell")
        .join(format!("local-{hash:016x}.json"))
}

/// 读取偏好文件：逐字段容错——任一字段解析失败（类型不对、未知值）只丢该字段
/// 并记一次诊断，其余字段照常恢复；整个文件不是 JSON 对象才视为不可用。
pub(super) fn load(path: &Path) -> Option<ClientChromePreferences> {
    let content = std::fs::read_to_string(path).ok()?;
    let document = match serde_json::from_str::<serde_json::Value>(&content) {
        Ok(document) => document,
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                %error,
                "client shell preferences are not valid JSON; using defaults"
            );
            return None;
        }
    };
    let serde_json::Value::Object(fields) = document else {
        tracing::warn!(
            path = %path.display(),
            "client shell preferences are not a JSON object; using defaults"
        );
        return None;
    };
    let mut kept = serde_json::Map::with_capacity(fields.len());
    let mut dropped = Vec::new();
    for (key, value) in fields {
        let mut probe = serde_json::Map::with_capacity(1);
        probe.insert(key.clone(), value.clone());
        if serde_json::from_value::<ClientChromePreferences>(serde_json::Value::Object(probe))
            .is_ok()
        {
            kept.insert(key, value);
        } else {
            dropped.push(key);
        }
    }
    if !dropped.is_empty() {
        tracing::warn!(
            path = %path.display(),
            fields = ?dropped,
            "ignoring unreadable client shell preference fields"
        );
    }
    serde_json::from_value(serde_json::Value::Object(kept)).ok()
}

pub(super) fn store(path: &Path, preferences: ClientChromePreferences) -> Result<(), String> {
    let content = serde_json::to_vec_pretty(&preferences)
        .map_err(|error| format!("failed to encode client shell state: {error}"))?;
    store_bytes(path, &content)
}

/// Atomic temp-file + rename write shared by the client-shell JSON state
/// files (chrome preferences, scene snapshots): a crash mid-write can never
/// leave a truncated document behind.
pub(super) fn store_bytes(path: &Path, content: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("invalid client shell state path: {}", path.display()))?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("failed to create client shell state directory: {error}"))?;
    let sequence = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
    let mut temp_name = path
        .file_name()
        .ok_or_else(|| format!("invalid client shell state path: {}", path.display()))?
        .to_os_string();
    temp_name.push(format!(".tmp-{}-{sequence}", std::process::id()));
    let temp_path = parent.join(temp_name);
    std::fs::write(&temp_path, content)
        .map_err(|error| format!("failed to write client shell state: {error}"))?;
    crate::platform::replace_file(&temp_path, path).map_err(|error| {
        let _ = std::fs::remove_file(&temp_path);
        format!("failed to replace client shell state: {error}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_paths_are_stable_and_distinct() {
        let first = path_for_local_endpoint(Path::new("/run/herdr/one.sock"));
        let again = path_for_local_endpoint(Path::new("/run/herdr/one.sock"));
        let second = path_for_local_endpoint(Path::new("/run/herdr/two.sock"));
        assert_eq!(first, again);
        assert_ne!(first, second);
    }

    /// ARCH-01：浮窗位置用稳定键写盘，旧版本的 Debug 名读回时归一化，
    /// 枚举改名不再静默丢配置；未知键按未设置处理。
    #[test]
    fn floating_page_keys_are_stable_and_legacy_debug_names_migrate() {
        let window = r#"{"x":0.1,"y":0.2,"width":0.3,"height":0.4}"#;
        let preferences: ClientChromePreferences = serde_json::from_str(&format!(
            r#"{{"pages":{{"product_announcement":{window},"Help":{window},"mystery":{window}}}}}"#
        ))
        .expect("pages");
        assert!(
            preferences.pages.contains_key("product_announcement"),
            "稳定键原样保留"
        );
        assert!(
            preferences.pages.contains_key("help"),
            "旧 Debug 名归一成稳定键：{:?}",
            preferences.pages.keys().collect::<Vec<_>>()
        );
        assert_eq!(preferences.pages.len(), 2, "未知键被丢弃");
    }

    #[test]
    fn unknown_monitor_tab_is_ignored_instead_of_failing_the_whole_file() {
        let preferences: ClientChromePreferences =
            serde_json::from_str(r#"{"monitor_tab":"nonsense","sidebar_width":30}"#)
                .expect("未知 tab 值不应让整份偏好失效");
        assert_eq!(preferences.monitor_tab, None);
        assert_eq!(preferences.sidebar_width, Some(30));
        let preferences: ClientChromePreferences =
            serde_json::from_str(r#"{"monitor_tab":"accounts"}"#).expect("已知 tab 值");
        assert_eq!(
            preferences.monitor_tab,
            Some(super::super::observability::Page::Accounts)
        );
    }

    #[test]
    fn bad_enum_values_only_drop_their_own_field() {
        let preferences: ClientChromePreferences = serde_json::from_str(
            r#"{"usage_format":"weird","usage_position":"hover","agent_panel_sort":"nope","monitor":{"interval_ms":"fast"},"usage_hover_dashboard":false,"sidebar_width":30}"#,
        )
        .expect("坏枚举值不应让整份偏好失效");
        assert_eq!(preferences.usage_format, None);
        assert_eq!(
            preferences.usage_position,
            Some(crate::config::UsageDisplayPosition::Hover)
        );
        assert_eq!(preferences.agent_panel_sort, None);
        assert!(preferences.monitor.is_none());
        assert_eq!(preferences.sidebar_width, Some(30));
    }

    /// 用量入口拆除后旧偏好文件里的残留键：`usage_hover_dashboard` 已无对应字段，
    /// `pages.usage_dashboard` 已无对应浮层——都只被忽略，其余字段照常恢复，
    /// 下一次写盘不再带它们。
    #[test]
    fn load_ignores_keys_of_the_removed_usage_entry_points() {
        let path = std::env::temp_dir().join(format!(
            "herdr-shell-removed-usage-preferences-{}.json",
            std::process::id()
        ));
        std::fs::write(
            &path,
            r#"{"usage_hover_dashboard":false,"sidebar_width":31,"usage_position":"both","pages":{"usage_dashboard":{"x":0.1,"y":0.1,"width":0.5,"height":0.5},"settings":{"x":0.1,"y":0.1,"width":0.5,"height":0.5}},"palette_recent":["observation:usage-dashboard"]}"#,
        )
        .expect("write preferences");
        let loaded = load(&path).expect("残留键不应让整份偏好失效");
        let _ = std::fs::remove_file(&path);
        assert_eq!(loaded.sidebar_width, Some(31));
        assert_eq!(
            loaded.usage_position,
            Some(crate::config::UsageDisplayPosition::Both)
        );
        assert!(
            !loaded.pages.contains_key("usage_dashboard"),
            "已移除浮层的窗口位置被丢弃"
        );
        assert!(loaded.pages.contains_key("settings"), "其余浮层位置保留");
        assert_eq!(loaded.palette_recent, ["observation:usage-dashboard"]);
        let saved = serde_json::to_string(&loaded).expect("serialize preferences");
        assert!(!saved.contains("usage_hover_dashboard"), "saved: {saved}");
        assert!(!saved.contains("\"usage_dashboard\""), "saved: {saved}");
    }

    #[test]
    fn load_drops_unreadable_fields_instead_of_the_whole_file() {
        let path = std::env::temp_dir().join(format!(
            "herdr-shell-lenient-preferences-{}.json",
            std::process::id()
        ));
        std::fs::write(
            &path,
            r#"{"sidebar_width":"wide","sidebar_collapsed":true,"monitor_tab":"accounts","palette_recent":["a"]}"#,
        )
        .expect("write preferences");
        let loaded = load(&path).expect("坏字段只丢自己，文件仍可用");
        assert_eq!(loaded.sidebar_width, None, "类型不对的字段被丢弃");
        assert_eq!(loaded.sidebar_collapsed, Some(true));
        assert_eq!(
            loaded.monitor_tab,
            Some(super::super::observability::Page::Accounts)
        );
        assert_eq!(loaded.palette_recent, ["a"]);
        std::fs::write(&path, "[1,2,3]").expect("write preferences");
        assert!(load(&path).is_none(), "不是对象的文件视为不可用");
        std::fs::remove_file(path).expect("remove preferences");
    }

    /// usage_* 键是 config.toml `[account_usage]` 的本机影子：任一为 Some 即「有本机
    /// 覆盖」；「恢复配置文件值」整组清空，非 usage 键不动。
    #[test]
    fn usage_overrides_are_detected_and_cleared_as_a_group() {
        let mut preferences = ClientChromePreferences {
            usage_format: Some(crate::config::UsageDisplayFormat::Table),
            sidebar_width: Some(30),
            ..ClientChromePreferences::default()
        };
        assert!(preferences.usage_overridden());
        preferences.clear_usage_overrides();
        assert!(!preferences.usage_overridden());
        assert_eq!(preferences.usage_format, None);
        assert_eq!(preferences.sidebar_width, Some(30), "非 usage 键不动");
        preferences.usage_disabled_providers = Some(Vec::new());
        assert!(preferences.usage_overridden(), "空列表也是显式覆盖");
        preferences.clear_usage_overrides();
        preferences.usage_hover_delay_ms = Some(800);
        assert!(preferences.usage_overridden(), "悬浮延时同属影子键");
    }

    /// 类型不对的 `usage_disabled_providers` 只丢自己，同文件里的其它 usage 键照常恢复。
    #[test]
    fn bad_disabled_providers_value_only_drops_its_own_field() {
        let path = std::env::temp_dir().join(format!(
            "herdr-shell-bad-disabled-providers-{}.json",
            std::process::id()
        ));
        std::fs::write(
            &path,
            r#"{"usage_disabled_providers":"claude","usage_position":"page","usage_enabled":true}"#,
        )
        .expect("write preferences");
        let loaded = load(&path).expect("坏字段只丢自己");
        assert_eq!(loaded.usage_disabled_providers, None);
        assert_eq!(
            loaded.usage_position,
            Some(crate::config::UsageDisplayPosition::Page)
        );
        assert_eq!(loaded.usage_enabled, Some(true));
        std::fs::remove_file(path).expect("remove preferences");
    }

    #[test]
    fn legacy_preferences_default_remote_collapses() {
        let preferences: ClientChromePreferences =
            serde_json::from_str(r#"{"collapsed_groups":["/repo"]}"#)
                .expect("legacy client chrome preferences");

        assert_eq!(preferences.collapsed_groups, ["/repo"]);
        assert!(preferences.remote_collapsed_groups.is_empty());
    }

    #[test]
    fn concurrent_stores_leave_complete_preferences() {
        let path = std::env::temp_dir().join(format!(
            "herdr-shell-concurrent-preferences-{}.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let writers = (20..28)
            .map(|width| {
                let path = path.clone();
                std::thread::spawn(move || {
                    store(
                        &path,
                        ClientChromePreferences {
                            sidebar_width: Some(width),
                            ..ClientChromePreferences::default()
                        },
                    )
                })
            })
            .collect::<Vec<_>>();
        for writer in writers {
            writer.join().expect("preference writer").expect("store");
        }
        assert!(load(&path)
            .and_then(|saved| saved.sidebar_width)
            .is_some_and(|width| (20..28).contains(&width)));
        std::fs::remove_file(path).expect("remove preferences");
    }

    #[test]
    fn repeated_store_replaces_existing_preferences() {
        let path = std::env::temp_dir().join(format!(
            "herdr-shell-preferences-{}.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        store(
            &path,
            ClientChromePreferences {
                sidebar_width: Some(24),
                ..ClientChromePreferences::default()
            },
        )
        .expect("first preference store");
        store(
            &path,
            ClientChromePreferences {
                sidebar_width: Some(32),
                ..ClientChromePreferences::default()
            },
        )
        .expect("replacement preference store");
        assert_eq!(load(&path).and_then(|saved| saved.sidebar_width), Some(32));
        std::fs::remove_file(path).expect("remove preferences");
    }
}
