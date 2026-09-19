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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) monitor: Option<crate::config::MonitorConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) usage_enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) usage_format: Option<crate::config::UsageDisplayFormat>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) usage_position: Option<crate::config::UsageDisplayPosition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) usage_disabled_providers: Option<Vec<String>>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) agent_panel_sort: Option<crate::config::AgentPanelSortConfig>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) collapsed_groups: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) remote_collapsed_groups: Vec<ClientRemoteCollapsedGroups>,
    /// Command palette MRU ids (newest first), capped on write.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) palette_recent: Vec<String>,
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
            (key.len() <= 64 && window.valid()).then(|| (key.clone(), window))
        })
        .collect())
}

fn read_monitor_tab<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<super::observability::Page>, D::Error> {
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

pub(super) fn load(path: &Path) -> Option<ClientChromePreferences> {
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
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
