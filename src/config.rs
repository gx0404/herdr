use crossterm::event::{KeyCode, KeyModifiers};

mod io;
mod keybinds;
mod model;
mod observability;
mod sidebar;
mod sound;
mod tab_bar;
mod theme;
mod window_title;
mod write;

pub use self::{
    io::{
        config_diagnostic_summary, config_dir, config_path, load_live_config,
        remove_keybinding_config_sections, remove_section_key, state_dir, upsert_section_bool,
        upsert_section_value,
    },
    keybinds::{
        format_key_combo, format_prefix_combos, normalize_key_combo, terminal_key_matches_combo,
        ActionKeybinds, BindingConfig, CommandKeybindConfig, CustomCommandAction,
        CustomCommandKeybind, IndexedKeybind, KeyCombo, Keybinds, LiveKeybindConfig,
    },
    model::{
        validated_sidebar_bounds, AgentPanelSortConfig, BorderStyleConfig, ColorDepth,
        ColorDepthConfig, Config, ConfigReloadReport, ConfigReloadStatus, HostCursorModeConfig,
        NewTerminalCwdConfig, PaneBordersConfig, RepeatImeCursorAnchorConfig, ShellModeConfig,
        SidebarCollapsedModeConfig, StatusIndicatorStyle, TabBarPositionConfig,
        ToastClipboardPosition, ToastConfig, ToastDelivery, ToastHerdrPosition, UiConfig,
        UpdateChannelConfig, MAX_TOAST_DELAY_SECONDS,
    },
    sidebar::{
        AgentSidebarToken, AgentsSidebarConfig, SidebarConfig, SidebarTokenStyle,
        SpaceSidebarToken, SpacesSidebarConfig,
    },
    sound::SoundConfig,
    tab_bar::TabBarRightEntryConfig,
    theme::{
        degrade_color_to_256, parse_color, resolve_color_depth, try_parse_color, CustomThemeColors,
        ModeThemeColors, ThemeComponentsConfig, ThemeConfig, DEFAULT_SELECTION_MIX_RATIO,
        THEME_NAMES,
    },
    window_title::{WindowTitlePart, WindowTitleTemplate, WindowTitleToken},
};

#[cfg(test)]
pub(crate) use self::io::test_dirs;
pub(crate) use self::keybinds::parse_key_combo;
pub(crate) use self::write::{update_file_at, write_edit, ConfigEdit};
pub(crate) use self::{
    io::{upsert_top_level_bool, upsert_top_level_value},
    tab_bar::{
        parse_tab_bar_datetime_format, tab_bar_right_diagnostics,
        MAX_TAB_BAR_COMMAND_INTERVAL_SECONDS, MAX_TAB_BAR_COMMAND_TIMEOUT_SECONDS,
        MAX_TAB_BAR_RIGHT_ENTRIES,
    },
    theme::{canonical_theme_name, unknown_color_diagnostic},
    window_title::{sanitize_window_title_text, window_title_diagnostics},
};
pub use observability::{
    AccountUsageConfig, MonitorConfig, UsageAccountConfig, UsageDisplayFormat, UsageDisplayPosition,
};

pub(crate) use self::model::LoadedConfig;
pub(crate) use self::{keybinds::CommandKeybindType, model::KeysConfig};

pub const CONFIG_PATH_ENV_VAR: &str = "HERDR_CONFIG_PATH";

/// `Config::load` 读取或解析失败时回退默认值并留下这两种前缀的诊断（真源是 `io.rs` 的
/// `Config::load`）。消费方据此区分「用户改了配置」与「配置暂时不可用」，不得自行嗅探字符串。
pub(crate) fn is_config_load_failure(diagnostic: &str) -> bool {
    diagnostic.starts_with("config parse error:") || diagnostic.starts_with("config read error:")
}

pub(crate) fn is_keybinding_config_diagnostic(diagnostic: &str) -> bool {
    if is_config_load_failure(diagnostic) {
        return false;
    }
    diagnostic.contains("keybinding") || diagnostic.contains("keys.")
}

pub(crate) fn config_diagnostic_summary_without_keybindings(
    diagnostics: &[String],
) -> Option<String> {
    let diagnostics = diagnostics
        .iter()
        .filter(|diagnostic| !is_keybinding_config_diagnostic(diagnostic))
        .cloned()
        .collect::<Vec<_>>();
    config_diagnostic_summary(&diagnostics)
}
pub const DEFAULT_SCROLLBACK_LIMIT_BYTES: usize = 10_000_000;
pub const DEFAULT_MOUSE_SCROLL_LINES: usize = 3;
pub const DEFAULT_MOBILE_WIDTH_THRESHOLD: u16 = 64;
pub const DEFAULT_HEADLESS_COLS: u16 = 120;
pub const DEFAULT_HEADLESS_ROWS: u16 = 40;

#[cfg(test)]
pub(crate) fn app_dir_name() -> &'static str {
    io::app_dir_name()
}

#[cfg(test)]
pub(crate) use self::theme::rgb_to_xterm256;

#[cfg(test)]
pub(crate) use self::test_env_lock::{TestEnvGuard, TestEnvLock};

/// 全 crate 唯一的测试环境变量锁：凡是改进程环境变量（`std::env::set_var` /
/// `remove_var`）的测试都必须持有它，并且持有到不再依赖改过的值为止。`cargo test` 在
/// 同一进程里多线程并发跑测试，环境变量是进程全局的；各模块各用一把锁等于没锁。
///
/// 本线程最外层的 `lock()` 给进程环境拍快照，最外层 guard 析构（含 panic 展开）时先按
/// 快照还原再放锁：新增的删掉，改过或删掉的改回原值。测试因此不必手动还原，断言失败也
/// 不会把改动漏给后面的测试。反过来，设了环境变量却在返回前放掉 guard 的辅助函数是错的
/// ——放锁即还原；要么把 guard 交给调用方，要么由调用方先持锁。
///
/// 同一线程可重入（嵌套的测试辅助函数可以各自加锁，只有最外层还原）；持有者 panic 时随
/// guard 析构释放、不中毒，一个失败的测试不会连带后面的测试。持锁的线程不能去等另一个
/// 也要加锁的线程，否则死锁。目录隔离优先用 `config::test_dirs::isolate_dirs` 的线程本地
/// 覆盖，不必改环境变量也就不用持锁。
#[cfg(test)]
pub(crate) fn test_config_env_lock() -> &'static TestEnvLock {
    static LOCK: TestEnvLock = TestEnvLock::new();
    &LOCK
}

#[cfg(test)]
mod test_env_lock {
    use std::collections::HashMap;
    use std::ffi::{OsStr, OsString};
    use std::marker::PhantomData;
    use std::panic::AssertUnwindSafe;
    use std::sync::{Condvar, LockResult, Mutex, MutexGuard, PoisonError};
    use std::thread::ThreadId;

    type EnvVars = Vec<(OsString, OsString)>;

    /// Windows 的环境变量名不分大小写：`Path` 与 `PATH` 是同一个变量。
    const ENV_NAMES_IGNORE_ASCII_CASE: bool = cfg!(windows);

    /// 同一线程可重入、永不中毒、最外层放锁时还原环境变量的互斥锁（口径见
    /// `test_config_env_lock`）。
    pub(crate) struct TestEnvLock {
        holder: Mutex<Holder>,
        released: Condvar,
    }

    struct Holder {
        thread: Option<ThreadId>,
        depth: usize,
        /// 最外层加锁时的进程环境，最外层放锁时按它还原。
        snapshot: Option<EnvVars>,
    }

    /// 持锁凭证，析构（含 panic 展开）时释放一层，最外层先还原环境再放锁。持有者按线程
    /// 记账，所以不能跨线程移动。
    pub(crate) struct TestEnvGuard {
        lock: &'static TestEnvLock,
        _not_send: PhantomData<*const ()>,
    }

    impl TestEnvLock {
        pub(crate) const fn new() -> Self {
            Self {
                holder: Mutex::new(Holder {
                    thread: None,
                    depth: 0,
                    snapshot: None,
                }),
                released: Condvar::new(),
            }
        }

        /// 返回 `LockResult` 只为沿用 std `Mutex` 的调用写法（`.lock().unwrap()`、
        /// `.unwrap_or_else(PoisonError::into_inner)`），恒为 `Ok`。
        pub(crate) fn lock(&'static self) -> LockResult<TestEnvGuard> {
            let current = std::thread::current().id();
            let mut holder = self
                .released
                .wait_while(self.holder(), |holder| {
                    holder.thread.is_some_and(|thread| thread != current)
                })
                .unwrap_or_else(PoisonError::into_inner);
            if holder.depth == 0 {
                holder.snapshot = Some(std::env::vars_os().collect());
            }
            holder.thread = Some(current);
            holder.depth += 1;
            Ok(TestEnvGuard {
                lock: self,
                _not_send: PhantomData,
            })
        }

        // 内部 Mutex 只在加锁/释放的几行记账里持有，不会带着 panic 中毒；万一中毒也照常取用。
        fn holder(&self) -> MutexGuard<'_, Holder> {
            self.holder.lock().unwrap_or_else(PoisonError::into_inner)
        }
    }

    impl Drop for TestEnvGuard {
        fn drop(&mut self) {
            let mut holder = self.lock.holder();
            if holder.depth > 1 {
                holder.depth -= 1;
                return;
            }
            // 最外层：还原期间锁仍记在本线程名下，别的线程进不来，不必占着内部 Mutex。
            let snapshot = holder.snapshot.take();
            drop(holder);
            if let Some(snapshot) = snapshot {
                // 析构可能正处在 panic 展开中：还原万一出意外只丢掉这次还原，锁照常释放，
                // 不能让整个测试进程卡死在这把锁上。
                let _ = std::panic::catch_unwind(AssertUnwindSafe(|| restore_env(&snapshot)));
            }
            let mut holder = self.lock.holder();
            holder.depth = 0;
            holder.thread = None;
            drop(holder);
            self.lock.released.notify_one();
        }
    }

    fn restore_env(snapshot: &[(OsString, OsString)]) {
        let current: EnvVars = std::env::vars_os().collect();
        for change in restore_plan(snapshot, &current, ENV_NAMES_IGNORE_ASCII_CASE) {
            match change {
                EnvChange::Remove(name) => std::env::remove_var(name),
                EnvChange::Set(name, value) => std::env::set_var(name, value),
            }
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    pub(super) enum EnvChange {
        Remove(OsString),
        Set(OsString, OsString),
    }

    /// 从 `current` 回到 `snapshot` 要做的改动：先删掉新增的，再补回删掉的、改回变了的。
    /// 名字不分大小写时，换了大小写写进去的变量按新写法删、再按快照里的写法设回，所以
    /// 必须先删后设。
    pub(super) fn restore_plan(
        snapshot: &[(OsString, OsString)],
        current: &[(OsString, OsString)],
        ignore_ascii_case: bool,
    ) -> Vec<EnvChange> {
        let before: HashMap<_, _> = restorable(snapshot)
            .map(|(name, value)| (env_key(name, ignore_ascii_case), value))
            .collect();
        let now: HashMap<_, _> = restorable(current)
            .map(|(name, value)| (env_key(name, ignore_ascii_case), value))
            .collect();
        let added = restorable(current)
            .filter(|(name, _)| !before.contains_key(&env_key(name, ignore_ascii_case)))
            .map(|(name, _)| EnvChange::Remove(name.clone()));
        let reverted = restorable(snapshot)
            .filter(|(name, value)| now.get(&env_key(name, ignore_ascii_case)) != Some(&value))
            .map(|(name, value)| EnvChange::Set(name.clone(), value.clone()));
        added.chain(reverted).collect()
    }

    /// `set_var` / `remove_var` 碰到空名、名字含 `=` 或 NUL、值含 NUL 会 panic；Windows 的
    /// `=C:` 一类隐藏条目（随当前目录变化）就是这种名字，还原时一律不碰。
    fn restorable(vars: &[(OsString, OsString)]) -> impl Iterator<Item = &(OsString, OsString)> {
        vars.iter().filter(|(name, value)| {
            let name = name.as_encoded_bytes();
            !name.is_empty()
                && !name.contains(&b'=')
                && !name.contains(&0)
                && !value.as_encoded_bytes().contains(&0)
        })
    }

    fn env_key(name: &OsStr, ignore_ascii_case: bool) -> Vec<u8> {
        let name = name.as_encoded_bytes();
        if ignore_ascii_case {
            name.to_ascii_uppercase()
        } else {
            name.to_vec()
        }
    }
}

impl Config {
    pub fn should_show_onboarding(&self) -> bool {
        self.onboarding.unwrap_or(true)
    }

    pub fn kitty_graphics_enabled(&self) -> bool {
        self.terminal
            .kitty_graphics
            .or(self.experimental.kitty_graphics)
            .unwrap_or(true)
    }

    pub fn prefix_keys(&self) -> Vec<(KeyCode, KeyModifiers)> {
        self.validated_keybinds().1
    }

    /// Parsed keybinds for Herdr actions.
    pub fn keybinds(&self) -> Keybinds {
        self.validated_keybinds().3
    }

    pub fn collect_diagnostics(&self) -> Vec<String> {
        let (prefix_diag, _, keybind_diags, _) = self.validated_keybinds();
        prefix_diag
            .into_iter()
            .chain(keybind_diags)
            .chain(self.remote_image_paste_key().err())
            .chain(self.theme.diagnostics())
            .chain(unknown_color_diagnostic("ui.accent", &self.ui.accent))
            .chain(observability::diagnostics(
                &self.monitor,
                &self.account_usage,
            ))
            .chain(self.ui.sound.diagnostics())
            .chain(tab_bar_right_diagnostics(&self.ui.tab_bar_right))
            .chain(window_title_diagnostics(&self.ui.window_title))
            .chain(self.invalid_sidebar_bounds_diagnostic())
            .chain(self.invalid_headless_size_diagnostic())
            .collect()
    }

    pub(crate) fn headless_size(&self) -> (u16, u16) {
        if self.invalid_headless_size_diagnostic().is_some() {
            (DEFAULT_HEADLESS_COLS, DEFAULT_HEADLESS_ROWS)
        } else {
            (self.server.headless_cols, self.server.headless_rows)
        }
    }

    pub(crate) fn invalid_headless_size_diagnostic(&self) -> Option<String> {
        (self.server.headless_cols == 0 || self.server.headless_rows == 0).then(|| {
            format!(
                "server.headless_cols and server.headless_rows must be greater than zero (got {}x{})",
                self.server.headless_cols, self.server.headless_rows
            )
        })
    }

    pub(crate) fn invalid_sidebar_bounds_diagnostic(&self) -> Option<String> {
        validated_sidebar_bounds(self.ui.sidebar_min_width, self.ui.sidebar_max_width)
            .is_none()
            .then(|| {
                format!(
                    "ui.sidebar_min_width ({}) is greater than sidebar_max_width ({})",
                    self.ui.sidebar_min_width, self.ui.sidebar_max_width
                )
            })
    }

    pub(crate) fn remote_image_paste_key(&self) -> Result<Option<(KeyCode, KeyModifiers)>, String> {
        let raw = self.keys.remote_image_paste.trim();
        if raw.is_empty() {
            return Ok(None);
        }
        parse_key_combo(raw).map(Some).ok_or_else(|| {
            format!("invalid keybinding: keys.remote_image_paste = {raw:?}; disabling binding")
        })
    }

    pub(crate) fn live_keybinds_with_diagnostics(
        &self,
    ) -> Result<(LiveKeybindConfig, Vec<String>), Vec<String>> {
        let (prefix_diag, prefix, keybind_diags, keybinds) = self.validated_keybinds();
        if let Some(prefix_diag) = prefix_diag {
            Err(std::iter::once(prefix_diag).chain(keybind_diags).collect())
        } else {
            Ok((LiveKeybindConfig { prefix, keybinds }, keybind_diags))
        }
    }

    pub(crate) fn local_keybindings_profile_toml(&self) -> Result<String, toml::ser::Error> {
        #[derive(serde::Serialize)]
        struct KeysProfile {
            keys: model::KeysConfigOverlay,
        }

        let mut keys = self.keys.local_profile(&self.keybinds());
        keys.set_prefixes(&self.prefix_keys());
        toml::to_string_pretty(&KeysProfile { keys })
    }
}

pub(crate) fn keybindings_from_profile_toml(profile: &str) -> Result<LiveKeybindConfig, String> {
    let config = toml::from_str::<Config>(profile)
        .map_err(|err| format!("invalid keybinding profile: {err}"))?;
    config
        .live_keybinds_with_diagnostics()
        .map(|(keybinds, _diagnostics)| keybinds)
        .map_err(|diagnostics| diagnostics.join("; "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn local_keybindings_profile_includes_defaults_and_excludes_commands() {
        let config: Config = toml::from_str(
            r#"
[keys]
prefix = "ctrl+a"
new_tab = "prefix+t"

[[keys.command]]
key = "prefix+g"
command = "lazygit"
"#,
        )
        .unwrap();

        let profile = config.local_keybindings_profile_toml().unwrap();
        assert!(profile.contains("[keys]"));
        assert!(profile.contains("prefix = \"ctrl+a\""));
        assert!(profile.contains("new_tab = \"prefix+t\""));
        assert!(profile.contains("next_tab = \"prefix+n\""));
        assert!(!profile.contains("lazygit"));
        assert!(!profile.contains("command ="));
        assert!(!profile.contains("[[keys.command]]"));
    }

    #[test]
    fn local_keybindings_profile_publishes_the_effective_prefix_fallback() {
        let config: Config = toml::from_str(
            r#"
[keys]
prefix = "ctrl+"
"#,
        )
        .unwrap();

        let profile = config.local_keybindings_profile_toml().unwrap();
        let keybinds = keybindings_from_profile_toml(&profile).unwrap();

        assert!(profile.contains("prefix = \"ctrl+b\""));
        assert_eq!(keybinds.prefix, config.prefix_keys());
    }

    #[test]
    fn local_keybindings_profile_publishes_additional_prefixes_for_old_clients() {
        let config: Config = toml::from_str(
            r#"
[keys]
prefix = ["ctrl+space", "ctrl+s"]
"#,
        )
        .unwrap();

        let profile = config.local_keybindings_profile_toml().unwrap();

        // Generation-1 clients parse `prefix` as a single string; the extra
        // prefixes ride in the optional `extra_prefixes` field they ignore.
        assert!(profile.contains("prefix = \"ctrl+space\""));
        assert!(!profile.contains("prefix = ["));
        assert!(profile.contains("extra_prefixes = [\"ctrl+s\"]"));

        let keybinds = keybindings_from_profile_toml(&profile).unwrap();
        assert_eq!(keybinds.prefix, config.prefix_keys());
    }

    #[test]
    fn local_keybindings_profile_omits_extra_prefixes_for_a_single_prefix() {
        let config: Config = toml::from_str(
            r#"
[keys]
prefix = "ctrl+b"
"#,
        )
        .unwrap();
        let profile = config.local_keybindings_profile_toml().unwrap();
        assert!(!profile.contains("extra_prefixes"));
    }

    #[test]
    fn local_keybindings_profile_preserves_user_default_provenance() {
        let config: Config = toml::from_str(
            r#"
[keys]
zoom = "prefix+?"
"#,
        )
        .unwrap();

        let profile = config.local_keybindings_profile_toml().unwrap();
        let round_tripped: Config = toml::from_str(&profile).unwrap();

        assert!(profile.contains("zoom = \"prefix+?\""));
        assert!(!profile.contains("help = \"prefix+?\""));
        assert!(round_tripped
            .keybinds()
            .zoom
            .bindings
            .iter()
            .any(|binding| binding.label == "prefix+?"));
        assert!(round_tripped.keybinds().help.bindings.is_empty());
    }

    #[test]
    fn local_keybindings_profile_omits_default_displaced_by_user_prefix() {
        let config: Config = toml::from_str(
            r#"
[keys]
prefix = "n"
"#,
        )
        .unwrap();

        let profile = config.local_keybindings_profile_toml().unwrap();
        let round_tripped: Config = toml::from_str(&profile).unwrap();

        assert!(profile.contains("prefix = \"n\""));
        assert!(!profile.contains("next_tab = \"prefix+n\""));
        assert!(round_tripped.keybinds().next_tab.bindings.is_empty());
    }

    #[test]
    fn local_keybindings_profile_preserves_legacy_indexed_tab_source() {
        let config: Config = toml::from_str(
            r#"
[keys.indexed]
tabs = "ctrl"
"#,
        )
        .unwrap();

        let profile = config.local_keybindings_profile_toml().unwrap();
        let round_tripped: Config = toml::from_str(&profile).unwrap();
        let keybinds = round_tripped.keybinds();
        let switch_tab_labels: Vec<_> = keybinds
            .switch_tab
            .iter()
            .map(|binding| binding.label.as_str())
            .collect();

        assert!(profile.contains("[keys.indexed]"));
        assert!(profile.contains("tabs = \"ctrl\""));
        assert!(!profile.contains("switch_tab = \"prefix+1..9\""));
        assert_eq!(switch_tab_labels.len(), 9);
        assert!(switch_tab_labels
            .iter()
            .all(|label| label.starts_with("ctrl+")));
    }

    #[test]
    fn local_keybindings_profile_keeps_invalid_legacy_indexed_default_disabled() {
        let config: Config = toml::from_str(
            r#"
[keys.indexed]
tabs = "bogus"
"#,
        )
        .unwrap();

        let profile = config.local_keybindings_profile_toml().unwrap();
        let round_tripped: Config = toml::from_str(&profile).unwrap();

        assert!(profile.contains("[keys.indexed]"));
        assert!(profile.contains("tabs = \"bogus\""));
        assert!(!profile.contains("switch_tab = \"prefix+1..9\""));
        assert!(round_tripped.keybinds().switch_tab.is_empty());
    }

    #[test]
    fn local_keybindings_profile_keeps_default_displaced_by_omitted_command_disabled() {
        let config: Config = toml::from_str(
            r#"
[[keys.command]]
key = "prefix+n"
command = "echo next"
"#,
        )
        .unwrap();

        let profile = config.local_keybindings_profile_toml().unwrap();
        let round_tripped: Config = toml::from_str(&profile).unwrap();

        assert!(!profile.contains("[[keys.command]]"));
        assert!(!profile.contains("command ="));
        assert!(profile.contains("next_tab = \"\""));
        assert!(round_tripped.keybinds().next_tab.bindings.is_empty());
    }

    #[test]
    fn local_keybindings_profile_preserves_partially_displaced_indexed_default() {
        let config: Config = toml::from_str(
            r#"
[[keys.command]]
key = "prefix+1"
command = "echo one"
"#,
        )
        .unwrap();

        let profile = config.local_keybindings_profile_toml().unwrap();
        let round_tripped: Config = toml::from_str(&profile).unwrap();
        let keybinds = round_tripped.keybinds();
        let switch_tab_labels: Vec<_> = keybinds
            .switch_tab
            .iter()
            .map(|binding| binding.label.as_str())
            .collect();

        assert!(!profile.contains("[[keys.command]]"));
        assert!(!profile.contains("switch_tab = \"prefix+1..9\""));
        assert!(profile.contains("\"prefix+2\""));
        assert!(profile.contains("\"prefix+9\""));
        assert!(!switch_tab_labels.contains(&"prefix+1"));
        assert_eq!(switch_tab_labels.len(), 8);
        assert!(switch_tab_labels
            .iter()
            .all(|label| label.starts_with("prefix+")));
    }

    #[test]
    fn remote_image_paste_key_defaults_to_ctrl_v() {
        let config = Config::default();
        assert_eq!(
            config.remote_image_paste_key().unwrap(),
            Some((KeyCode::Char('v'), KeyModifiers::CONTROL))
        );
    }

    #[test]
    fn remote_image_paste_key_can_be_disabled() {
        let config: Config = toml::from_str("[keys]\nremote_image_paste = ''\n").unwrap();
        assert_eq!(config.remote_image_paste_key().unwrap(), None);
    }

    #[test]
    fn ui_host_cursor_defaults_to_auto_and_parses_overrides() {
        let default_config = Config::default();
        assert_eq!(default_config.ui.host_cursor, HostCursorModeConfig::Auto);

        let native: Config = toml::from_str("[ui]\nhost_cursor = 'native'\n").unwrap();
        assert_eq!(native.ui.host_cursor, HostCursorModeConfig::Native);

        let drawn: Config = toml::from_str("[ui]\nhost_cursor = 'drawn'\n").unwrap();
        assert_eq!(drawn.ui.host_cursor, HostCursorModeConfig::Drawn);
    }

    // 被测的独立锁实例。它最外层放锁时同样按快照还原环境，所以用它的测试全程持全局锁：
    // 否则会把同进程里别的测试刚设的环境变量改回去。
    fn leaked_test_env_lock() -> &'static TestEnvLock {
        Box::leak(Box::new(TestEnvLock::new()))
    }

    /// 在另一线程加锁；拿到锁（随即释放）后通道收到一条消息。
    fn lock_on_other_thread(lock: &'static TestEnvLock) -> mpsc::Receiver<()> {
        let (acquired_tx, acquired_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _guard = lock.lock().unwrap();
            let _ = acquired_tx.send(());
        });
        acquired_rx
    }

    fn env_var(name: &str) -> Option<String> {
        std::env::var(name).ok()
    }

    #[test]
    fn test_env_lock_is_reentrant_and_released_by_the_outermost_guard() {
        let _global = test_config_env_lock().lock().unwrap();
        let lock = leaked_test_env_lock();
        let outer = lock.lock().unwrap();
        let inner = lock.lock().expect("同一线程再次加锁不得阻塞或报错");
        drop(inner);

        // 内层释放后外层仍持有，别的线程进不来；外层释放后才进得来。
        let acquired = lock_on_other_thread(lock);
        assert!(acquired.recv_timeout(Duration::from_millis(100)).is_err());
        drop(outer);
        acquired
            .recv_timeout(Duration::from_secs(30))
            .expect("完全释放后等待的线程必须拿到锁");
    }

    #[test]
    fn test_env_lock_is_released_without_poisoning_when_the_holder_panics() {
        let _global = test_config_env_lock().lock().unwrap();
        let lock = leaked_test_env_lock();
        let panicked = std::thread::spawn(move || {
            let _outer = lock.lock().unwrap();
            let _inner = lock.lock().unwrap();
            panic!("持锁的测试失败");
        })
        .join();
        assert!(panicked.is_err());

        // panic 展开时两层 guard 都已释放：后来者照常拿锁，拿到的是 Ok 而不是中毒错误。
        lock_on_other_thread(lock)
            .recv_timeout(Duration::from_secs(30))
            .expect("持有者 panic 后锁必须已释放");
        assert!(lock.lock().is_ok());
    }

    #[test]
    fn test_env_lock_excludes_other_threads() {
        let _global = test_config_env_lock().lock().unwrap();
        let lock = leaked_test_env_lock();
        let inside = AtomicUsize::new(0);
        let max_inside = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    for _ in 0..50 {
                        let _guard = lock.lock().unwrap();
                        let _nested = lock.lock().unwrap();
                        let now = inside.fetch_add(1, Ordering::SeqCst) + 1;
                        max_inside.fetch_max(now, Ordering::SeqCst);
                        std::thread::yield_now();
                        inside.fetch_sub(1, Ordering::SeqCst);
                    }
                });
            }
        });
        assert_eq!(max_inside.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_env_lock_restores_added_changed_and_removed_variables_on_release() {
        let _global = test_config_env_lock().lock().unwrap();
        let (added, changed, removed, recased) = (
            "HERDR_TEST_ENV_LOCK_ADDED",
            "HERDR_TEST_ENV_LOCK_CHANGED",
            "HERDR_TEST_ENV_LOCK_REMOVED",
            "HERDR_TEST_ENV_LOCK_RECASED",
        );
        std::env::remove_var(added);
        for name in [changed, removed, recased] {
            std::env::set_var(name, "before");
        }

        let guard = leaked_test_env_lock().lock().unwrap();
        std::env::set_var(added, "during");
        std::env::set_var(changed, "during");
        std::env::remove_var(removed);
        // Windows 上换个大小写写进去的仍是同一个变量，unix 上则是新增的另一个变量。
        let lowercase = recased.to_ascii_lowercase();
        std::env::set_var(&lowercase, "during");
        drop(guard);

        assert_eq!(env_var(added), None);
        assert_eq!(env_var(changed).as_deref(), Some("before"));
        assert_eq!(env_var(removed).as_deref(), Some("before"));
        assert_eq!(env_var(recased).as_deref(), Some("before"));
        assert!(std::env::vars_os().all(|(name, _)| name != lowercase.as_str()));
    }

    #[test]
    fn test_env_lock_restores_the_environment_when_the_holder_panics() {
        let _global = test_config_env_lock().lock().unwrap();
        let (added, changed) = (
            "HERDR_TEST_ENV_LOCK_PANIC_ADDED",
            "HERDR_TEST_ENV_LOCK_PANIC_CHANGED",
        );
        std::env::remove_var(added);
        std::env::set_var(changed, "before");
        let lock = leaked_test_env_lock();
        let panicked = std::thread::spawn(move || {
            let _guard = lock.lock().unwrap();
            std::env::set_var(added, "during");
            std::env::set_var(changed, "during");
            panic!("持锁的测试失败");
        })
        .join();
        assert!(panicked.is_err());

        assert_eq!(env_var(added), None);
        assert_eq!(env_var(changed).as_deref(), Some("before"));
    }

    #[test]
    fn test_env_lock_restores_only_when_the_last_guard_is_released() {
        let _global = test_config_env_lock().lock().unwrap();
        let (outer_var, inner_var) = ("HERDR_TEST_ENV_LOCK_OUTER", "HERDR_TEST_ENV_LOCK_INNER");
        std::env::remove_var(outer_var);
        std::env::remove_var(inner_var);
        let lock = leaked_test_env_lock();

        let outer = lock.lock().unwrap();
        std::env::set_var(outer_var, "outer");
        let inner = lock.lock().unwrap();
        std::env::set_var(inner_var, "inner");
        drop(inner);
        // 内层放锁不还原：外层还依赖这些改动。
        assert_eq!(env_var(outer_var).as_deref(), Some("outer"));
        assert_eq!(env_var(inner_var).as_deref(), Some("inner"));
        drop(outer);
        assert_eq!(env_var(outer_var), None);
        assert_eq!(env_var(inner_var), None);

        // 先放拍快照的那一层也一样：还原发生在最后一层释放时。
        let first = lock.lock().unwrap();
        let second = lock.lock().unwrap();
        std::env::set_var(outer_var, "outer");
        drop(first);
        assert_eq!(env_var(outer_var).as_deref(), Some("outer"));
        drop(second);
        assert_eq!(env_var(outer_var), None);
    }

    #[test]
    fn test_env_restore_skips_hidden_and_invalid_names() {
        use super::test_env_lock::{restore_plan, EnvChange};
        let vars = |entries: &[(&str, &str)]| -> Vec<(OsString, OsString)> {
            entries
                .iter()
                .map(|&(name, value)| (OsString::from(name), OsString::from(value)))
                .collect()
        };
        let set = |name: &str, value: &str| EnvChange::Set(name.into(), value.into());
        let remove = |name: &str| EnvChange::Remove(name.into());
        // Windows 的 `=C:` 一类隐藏条目随当前目录变化；它们和空名、含 NUL 的名字一样会让
        // set_var / remove_var panic，变了也不碰。
        let snapshot = vars(&[
            ("=C:", "C:\\before"),
            ("Path", "a"),
            ("KEEP", "same"),
            ("GONE", "x"),
            ("BAD\0NAME", "x"),
        ]);
        let current = vars(&[
            ("=C:", "C:\\after"),
            ("=D:", "D:\\"),
            ("", "empty"),
            ("PATH", "b"),
            ("KEEP", "same"),
            ("NEW", "y"),
            ("NUL\0", "y"),
        ]);

        // Windows：名字不分大小写，`PATH` 就是 `Path`。
        assert_eq!(
            restore_plan(&snapshot, &current, true),
            vec![remove("NEW"), set("Path", "a"), set("GONE", "x")]
        );
        // unix：大小写不同就是两个变量。
        assert_eq!(
            restore_plan(&snapshot, &current, false),
            vec![
                remove("PATH"),
                remove("NEW"),
                set("Path", "a"),
                set("GONE", "x")
            ]
        );
        assert!(restore_plan(&current, &current, true).is_empty());

        // 真实进程环境（Windows 上可能带着隐藏条目）走一遍加锁放锁：不 panic，也不动它。
        let _global = test_config_env_lock().lock().unwrap();
        let before: Vec<_> = std::env::vars_os().collect();
        drop(leaked_test_env_lock().lock().unwrap());
        assert_eq!(std::env::vars_os().collect::<Vec<_>>(), before);
    }

    #[test]
    fn global_test_env_lock_keeps_std_mutex_call_sites_working() {
        let _outer = test_config_env_lock().lock().unwrap();
        let _inner = test_config_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
    }
}
