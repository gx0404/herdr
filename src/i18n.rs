//! Bilingual UI string tables and the process-wide language selection.
//!
//! The default is `zh-CN`; `en` keeps the upstream wording. The language is
//! chosen once at process start from `HERDR_LANG` (wins) or the `language`
//! key in config.toml, and is re-applied on client config reload. All
//! user-visible chrome text goes through the `Texts` tables so both
//! languages stay complete by construction.

use std::sync::atomic::{AtomicU8, Ordering};

pub mod en;
pub mod zh_cn;

pub const LANG_ENV_VAR: &str = "HERDR_LANG";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
pub enum Lang {
    #[default]
    #[serde(rename = "zh-CN")]
    ZhCn,
    #[serde(rename = "en")]
    En,
}

/// Tolerant deserialization: a mistyped `language` value falls back to the
/// default instead of rejecting the whole config file.
impl<'de> serde::Deserialize<'de> for Lang {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Ok(Lang::parse(&value).unwrap_or_default())
    }
}

impl Lang {
    pub const fn as_str(self) -> &'static str {
        match self {
            Lang::ZhCn => "zh-CN",
            Lang::En => "en",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "zh-CN" => Some(Lang::ZhCn),
            "en" => Some(Lang::En),
            _ => None,
        }
    }
}

static LANG: AtomicU8 = AtomicU8::new(Lang::ZhCn as u8);

/// Apply a config-file language change. An explicit `HERDR_LANG` pin wins so
/// reloading the config (e.g. saving an unrelated setting) cannot flip the
/// language away from what the operator forced for this run.
pub fn apply_config_language(lang: Lang) {
    let env_pinned = std::env::var_os(LANG_ENV_VAR).is_some_and(|value| !value.is_empty());
    if !env_pinned {
        set_lang(lang);
    }
}

pub fn set_lang(lang: Lang) {
    LANG.store(lang as u8, Ordering::Relaxed);
}

pub fn lang() -> Lang {
    match LANG.load(Ordering::Relaxed) {
        1 => Lang::En,
        _ => Lang::ZhCn,
    }
}

/// Test helper: switch the language for the guard's lifetime.
#[cfg(test)]
pub fn lang_guard(lang: Lang) -> LangGuard {
    let previous = self::lang();
    set_lang(lang);
    LangGuard(previous)
}

#[cfg(test)]
pub struct LangGuard(Lang);

#[cfg(test)]
impl Drop for LangGuard {
    fn drop(&mut self) {
        set_lang(self.0);
    }
}

/// Resolve the language before any user-visible output: `HERDR_LANG` wins so
/// a broken config cannot pin the wrong language, then a minimal peek at
/// config.toml's `language` key (full config load happens later, with
/// diagnostics).
pub fn init_early() {
    if let Some(lang) = std::env::var(LANG_ENV_VAR)
        .ok()
        .and_then(|value| Lang::parse(&value))
    {
        set_lang(lang);
        return;
    }
    if let Some(lang) = peek_config_language() {
        set_lang(lang);
    }
}

fn peek_config_language() -> Option<Lang> {
    let content = std::fs::read_to_string(crate::config::config_path()).ok()?;
    let value = content.parse::<toml::Value>().ok()?;
    Lang::parse(value.get("language")?.as_str()?)
}

pub struct ChromeTexts {
    pub close_button: &'static str,
    pub continue_button: &'static str,
    pub copied: &'static str,
}

pub struct OnboardingTexts {
    pub subtitle: &'static str,
    pub description: [&'static str; 3],
    pub prefix_suffix: &'static str,
    pub help_suffix: &'static str,
    pub next: &'static str,
}

pub struct ContextMenuTexts {
    pub rename: &'static str,
    pub close: &'static str,
    pub close_group: &'static str,
    pub close_pane: &'static str,
    pub new_worktree: &'static str,
    pub open_worktree: &'static str,
    pub delete_worktree: &'static str,
    pub expand: &'static str,
    pub collapse: &'static str,
    pub new_tab: &'static str,
    pub rename_pane: &'static str,
    pub clear_pane_name: &'static str,
    pub swap_with_focused: &'static str,
    pub split_right: &'static str,
    pub split_down: &'static str,
    pub zoom: &'static str,
    pub use_herdr_menu: &'static str,
    pub send_right_clicks: &'static str,
}

pub struct GlobalMenuTexts {
    pub settings: &'static str,
    pub keybinds: &'static str,
    pub reload_config: &'static str,
    pub update_ready: &'static str,
    pub whats_new: &'static str,
    pub detach: &'static str,
}

pub struct KeybindTexts {
    pub group_global: &'static str,
    pub group_navigation: &'static str,
    pub group_workspaces_tabs: &'static str,
    pub group_panes: &'static str,
    pub group_custom: &'static str,
    pub unset: &'static str,
    pub prefix_mode: &'static str,
    pub keybinds: &'static str,
    pub settings: &'static str,
    pub detach: &'static str,
    pub reload_config: &'static str,
    pub open_notification_target: &'static str,
    pub back: &'static str,
    pub workspace_list: &'static str,
    pub move_focus: &'static str,
    pub cycle_pane: &'static str,
    pub open_workspace: &'static str,
    pub switch_workspace: &'static str,
    pub workspace_navigation: &'static str,
    pub session_navigator: &'static str,
    pub new_workspace: &'static str,
    pub new_worktree: &'static str,
    pub open_worktree: &'static str,
    pub delete_worktree_checkout: &'static str,
    pub rename_workspace: &'static str,
    pub close_workspace: &'static str,
    pub previous_workspace: &'static str,
    pub next_workspace: &'static str,
    pub switch_workspace_1_9: &'static str,
    pub previous_agent: &'static str,
    pub next_agent: &'static str,
    pub focus_agent_1_9: &'static str,
    pub new_tab: &'static str,
    pub rename_tab: &'static str,
    pub previous_tab: &'static str,
    pub next_tab: &'static str,
    pub move_tab_left: &'static str,
    pub move_tab_right: &'static str,
    pub switch_tab_1_9: &'static str,
    pub close_tab: &'static str,
    pub split_vertical: &'static str,
    pub split_horizontal: &'static str,
    pub close_pane: &'static str,
    pub rename_pane: &'static str,
    pub edit_scrollback: &'static str,
    pub copy_mode: &'static str,
    pub zoom_pane: &'static str,
    pub resize_mode: &'static str,
    pub resize_pane_left: &'static str,
    pub resize_pane_down: &'static str,
    pub resize_pane_up: &'static str,
    pub resize_pane_right: &'static str,
    pub toggle_sidebar: &'static str,
    pub focus_pane_left: &'static str,
    pub focus_pane_down: &'static str,
    pub focus_pane_up: &'static str,
    pub focus_pane_right: &'static str,
    pub cycle_pane_next: &'static str,
    pub cycle_pane_previous: &'static str,
    pub last_pane: &'static str,
    pub custom_command: &'static str,
}

pub struct OverlayTexts {
    pub update_ready: &'static str,
    pub release_preview_title: &'static str,
    pub whats_new_in_release: &'static str,
    pub product_announcement_preview: &'static str,
    pub product_announcement: &'static str,
    pub keybinds_title: &'static str,
    pub footer_scroll: &'static str,
    pub footer_wheel: &'static str,
    pub footer_sep: &'static str,
    pub footer_close: &'static str,
    pub footer_keys: &'static str,
    pub save_button: &'static str,
    pub clear_button: &'static str,
    pub cancel_button: &'static str,
    pub confirm_button: &'static str,
    pub back_button: &'static str,
    pub filter_blocked: &'static str,
    pub filter_working: &'static str,
    pub filter_idle: &'static str,
    pub filter_done: &'static str,
    pub search_panes_hint: &'static str,
    pub navigator_search_footer: &'static str,
    pub navigator_footer: &'static str,
    pub help_filter_hint: &'static str,
    pub edit_footer: &'static str,
    pub search_footer: &'static str,
    pub no_matching_keybinds: &'static str,
}

pub struct DialogTexts {
    pub new_workspace: &'static str,
    pub rename_workspace: &'static str,
    pub new_tab: &'static str,
    pub rename_tab: &'static str,
    pub rename_pane: &'static str,
    pub close_workspace_q: &'static str,
    pub close_worktree_group_q: &'static str,
    pub one_pane: &'static str,
    pub pane_count_fmt: &'static str,
    pub group_scope_fmt: &'static str,
}

pub struct WorktreeTexts {
    pub new_worktree: &'static str,
    pub open_worktree: &'static str,
    pub branch_hint: &'static str,
    pub checkout_hint: &'static str,
    pub creating: &'static str,
    pub create_and_open: &'static str,
    pub filter_worktrees: &'static str,
    pub checkouts_fmt: &'static str,
    pub checkouts_filtered_fmt: &'static str,
    pub no_matching: &'static str,
    pub opening: &'static str,
    pub open_button: &'static str,
    pub delete_title: &'static str,
    pub removes_folder: &'static str,
    pub branch_not_deleted: &'static str,
    pub dirty_warning: &'static str,
    pub removing: &'static str,
    pub delete_anyway: &'static str,
    pub remove: &'static str,
}

pub struct SettingsTexts {
    pub title: &'static str,
    pub section_language: &'static str,
    pub section_theme: &'static str,
    pub section_indicators: &'static str,
    pub section_sound: &'static str,
    pub section_toasts: &'static str,
    pub section_integrations: &'static str,
    pub language: &'static str,
    pub language_hint: &'static str,
    pub lang_zh: &'static str,
    pub lang_en: &'static str,
    pub indicators: &'static str,
    pub indicators_hint: &'static str,
    pub indicator_dots: &'static str,
    pub indicator_symbols: &'static str,
    pub sound: &'static str,
    pub sound_hint: &'static str,
    pub toasts: &'static str,
    pub toasts_hint: &'static str,
    pub sound_on: &'static str,
    pub sound_off: &'static str,
    pub toast_off: &'static str,
    pub toast_herdr: &'static str,
    pub toast_terminal: &'static str,
    pub toast_system: &'static str,
    pub install_button: &'static str,
    pub apply_button: &'static str,
    pub footer: &'static str,
    pub integrations: &'static str,
    pub integrations_hint: &'static str,
    pub loading: &'static str,
    pub no_targets: &'static str,
    pub state_installed: &'static str,
    pub state_update_available: &'static str,
    pub state_available: &'static str,
    pub state_not_found: &'static str,
    pub installing: &'static str,
}

pub struct SidebarTexts {
    pub spaces: &'static str,
    pub agents: &'static str,
    pub new: &'static str,
    pub machines: &'static str,
    pub new_endpoint_fmt: &'static str,
    pub menu: &'static str,
    pub attention_menu: &'static str,
    pub local: &'static str,
    pub wt_open: &'static str,
    pub wt_detached: &'static str,
    pub wt_root: &'static str,
    pub sort_grouped: &'static str,
    pub sort_priority: &'static str,
}

pub struct StatusTexts {
    pub blocked: &'static str,
    pub done: &'static str,
    pub working: &'static str,
    pub idle: &'static str,
}

pub struct ModeBarTexts {
    pub error: &'static str,
    pub prefix: &'static str,
    pub prefix_cancel: &'static str,
    pub prefix_send: &'static str,
    pub prefix_nav: &'static str,
    pub prefix_keybinds: &'static str,
    pub navigate: &'static str,
    pub nav_back: &'static str,
    pub nav_workspace: &'static str,
    pub nav_pane: &'static str,
    pub resize: &'static str,
    pub resize_width: &'static str,
    pub resize_height: &'static str,
    pub resize_done: &'static str,
    pub copy: &'static str,
    pub copy_footer: &'static str,
}

pub struct NotifyTexts {
    pub agent_waiting_fmt: &'static str,
    pub agent_done_fmt: &'static str,
    pub needs_attention: &'static str,
    pub finished: &'static str,
    pub updated: &'static str,
    pub title_fmt: &'static str,
    pub title_with_context_fmt: &'static str,
    pub version_available_fmt: &'static str,
    pub herdr_version_available_fmt: &'static str,
    pub detection_updated: &'static str,
}

pub struct MobileTexts {
    pub no_workspace: &'static str,
    pub switch: &'static str,
    pub switch_label: &'static str,
    pub st_connecting: &'static str,
    pub st_online: &'static str,
    pub st_reconnecting: &'static str,
    pub st_attention: &'static str,
    pub st_disabled: &'static str,
    pub tab_label_fmt: &'static str,
    pub tab_label_pos_fmt: &'static str,
    pub no_agents: &'static str,
    pub all_idle: &'static str,
    pub section_machines: &'static str,
    pub section_spaces: &'static str,
    pub section_tabs: &'static str,
    pub section_menu: &'static str,
    pub agents_label_fmt: &'static str,
    pub agents_plain: &'static str,
    pub no_matching_agents: &'static str,
    pub new_workspace: &'static str,
    pub new_tab: &'static str,
    pub close: &'static str,
    pub not_ready_fmt: &'static str,
    pub reconnecting_fmt: &'static str,
}

pub struct UpdateTexts {
    pub install_run_fmt: &'static str,
    pub install_nix: &'static str,
}

pub struct EndpointTexts {
    pub local_unavailable: &'static str,
    pub saved_machines_fmt: &'static str,
}

/// Help/about strings for the clap CLI surface. Option and subcommand
/// names, value placeholders and parsed values stay untranslated; only
/// the descriptive text differs per language.
pub struct CliHelpTexts {
    pub about: &'static str,
    pub session_help: &'static str,
    pub machine_help: &'static str,
    pub remote_help: &'static str,
    pub remote_keybindings_help: &'static str,
    pub handoff_help: &'static str,
    pub default_config_help: &'static str,
    pub skill_help: &'static str,
    pub version_help: &'static str,
    pub help_flag_help: &'static str,
    pub env_help: &'static str,
    pub source_help: &'static str,
    pub timeout_ms_help: &'static str,
    pub send_keys_after_help: &'static str,
    pub completion_about: &'static str,
    pub completion_shell_help: &'static str,
    pub update_about: &'static str,
    pub update_handoff_help: &'static str,
    pub status_about: &'static str,
    pub status_server_about: &'static str,
    pub status_client_about: &'static str,
    pub config_about: &'static str,
    pub config_check_about: &'static str,
    pub config_reset_keys_about: &'static str,
    pub channel_about: &'static str,
    pub channel_show_about: &'static str,
    pub channel_set_about: &'static str,
    pub server_about: &'static str,
    pub server_stop_about: &'static str,
    pub server_reload_config_about: &'static str,
    pub server_agent_manifests_about: &'static str,
    pub server_update_agent_manifests_about: &'static str,
    pub server_reload_agent_manifests_about: &'static str,
    pub api_about: &'static str,
    pub api_snapshot_about: &'static str,
    pub api_schema_about: &'static str,
    pub workspace_about: &'static str,
    pub workspace_list_about: &'static str,
    pub workspace_create_about: &'static str,
    pub workspace_get_about: &'static str,
    pub workspace_focus_about: &'static str,
    pub workspace_rename_about: &'static str,
    pub workspace_report_metadata_about: &'static str,
    pub workspace_close_about: &'static str,
    pub worktree_about: &'static str,
    pub worktree_list_about: &'static str,
    pub worktree_create_about: &'static str,
    pub worktree_open_about: &'static str,
    pub worktree_remove_about: &'static str,
    pub tab_about: &'static str,
    pub tab_list_about: &'static str,
    pub tab_create_about: &'static str,
    pub tab_get_about: &'static str,
    pub tab_focus_about: &'static str,
    pub tab_rename_about: &'static str,
    pub tab_close_about: &'static str,
    pub notification_about: &'static str,
    pub notification_show_about: &'static str,
    pub agent_about: &'static str,
    pub agent_list_about: &'static str,
    pub agent_get_about: &'static str,
    pub agent_read_about: &'static str,
    pub agent_send_keys_about: &'static str,
    pub agent_prompt_about: &'static str,
    pub agent_prompt_wait_help: &'static str,
    pub agent_prompt_until_help: &'static str,
    pub agent_prompt_after_help: &'static str,
    pub agent_rename_about: &'static str,
    pub agent_focus_about: &'static str,
    pub agent_wait_about: &'static str,
    pub agent_wait_until_help: &'static str,
    pub agent_wait_after_help: &'static str,
    pub agent_attach_about: &'static str,
    pub agent_start_about: &'static str,
    pub agent_start_kind_help: &'static str,
    pub agent_start_pane_help: &'static str,
    pub agent_start_timeout_help: &'static str,
    pub agent_start_after_help: &'static str,
    pub agent_explain_about: &'static str,
    pub pane_about: &'static str,
    pub pane_list_about: &'static str,
    pub pane_current_about: &'static str,
    pub pane_get_about: &'static str,
    pub pane_layout_about: &'static str,
    pub pane_process_info_about: &'static str,
    pub pane_neighbor_about: &'static str,
    pub pane_edges_about: &'static str,
    pub pane_focus_about: &'static str,
    pub pane_resize_about: &'static str,
    pub pane_zoom_about: &'static str,
    pub pane_read_about: &'static str,
    pub pane_rename_about: &'static str,
    pub pane_input_about: &'static str,
    pub pane_split_about: &'static str,
    pub pane_swap_about: &'static str,
    pub pane_move_about: &'static str,
    pub pane_close_about: &'static str,
    pub pane_send_text_about: &'static str,
    pub pane_send_text_after_help: &'static str,
    pub pane_send_keys_about: &'static str,
    pub pane_wait_output_about: &'static str,
    pub pane_wait_output_match_help: &'static str,
    pub pane_wait_output_regex_help: &'static str,
    pub pane_wait_output_lines_help: &'static str,
    pub pane_wait_output_raw_help: &'static str,
    pub pane_wait_output_after_help: &'static str,
    pub pane_run_about: &'static str,
    pub pane_report_agent_about: &'static str,
    pub pane_report_agent_session_about: &'static str,
    pub pane_release_agent_about: &'static str,
    pub pane_report_metadata_about: &'static str,
    pub terminal_about: &'static str,
    pub terminal_attach_about: &'static str,
    pub terminal_session_about: &'static str,
    pub terminal_session_control_about: &'static str,
    pub terminal_session_observe_about: &'static str,
    pub terminal_title_about: &'static str,
    pub terminal_title_set_about: &'static str,
    pub terminal_title_clear_about: &'static str,
    pub session_about: &'static str,
    pub session_list_about: &'static str,
    pub session_attach_about: &'static str,
    pub session_stop_about: &'static str,
    pub session_delete_about: &'static str,
    pub integration_about: &'static str,
    pub integration_install_about: &'static str,
    pub integration_uninstall_about: &'static str,
    pub integration_status_about: &'static str,
    pub plugin_about: &'static str,
    pub plugin_install_about: &'static str,
    pub plugin_uninstall_about: &'static str,
    pub plugin_link_about: &'static str,
    pub plugin_unlink_about: &'static str,
    pub plugin_enable_about: &'static str,
    pub plugin_disable_about: &'static str,
    pub plugin_list_about: &'static str,
    pub plugin_config_dir_about: &'static str,
    pub plugin_action_about: &'static str,
    pub plugin_action_list_about: &'static str,
    pub plugin_action_invoke_about: &'static str,
    pub plugin_log_about: &'static str,
    pub plugin_log_list_about: &'static str,
    pub plugin_pane_about: &'static str,
    pub plugin_pane_open_about: &'static str,
    pub plugin_pane_focus_about: &'static str,
    pub plugin_pane_close_about: &'static str,
    pub machine_about: &'static str,
    pub machine_list_about: &'static str,
    pub machine_add_about: &'static str,
    pub machine_label_help: &'static str,
    pub machine_remote_session_help: &'static str,
    pub machine_rename_about: &'static str,
    pub machine_remove_about: &'static str,
    pub machine_enable_about: &'static str,
    pub machine_disable_about: &'static str,
    pub terminal_session_observe_usage: &'static str,
    pub terminal_session_control_usage: &'static str,
    pub agent_help_footer: &'static str,
    pub main_tagline: &'static str,
    pub main_usage_line: &'static str,
}

/// Human-readable stdout output for CLI commands in non-`--json` mode:
/// status field labels, list rows, state values, and action summaries.
/// JSON payloads stay byte-identical across languages; usage/help and
/// error strings are handled by separate groups. Label entries carry
/// their trailing separator (`": "` / `"："`) so call sites compose with
/// `format!`; `*_fmt` entries are `fill` templates for word-order changes.
pub struct CliOutputTexts {
    pub machine_help: &'static str,
    pub machine_none_saved: &'static str,
    pub machine_saved_fmt: &'static str,
    pub machine_clients_connect: &'static str,
    pub machine_renamed_fmt: &'static str,
    pub machine_removed_fmt: &'static str,
    pub machine_enabled_fmt: &'static str,
    pub machine_disabled_fmt: &'static str,
    pub state_enabled: &'static str,
    pub state_disabled: &'static str,
    pub status_client_header: &'static str,
    pub status_server_header: &'static str,
    pub status_update_header: &'static str,
    pub status_label: &'static str,
    pub label_version: &'static str,
    pub label_channel: &'static str,
    pub label_protocol: &'static str,
    pub label_endpoint_generation: &'static str,
    pub label_binary: &'static str,
    pub label_socket: &'static str,
    pub label_endpoint_compatible: &'static str,
    pub label_private_protocol: &'static str,
    pub label_private_protocol_compatible: &'static str,
    pub label_restart_needed: &'static str,
    pub label_server_binary_stale: &'static str,
    pub status_running: &'static str,
    pub status_not_running: &'static str,
    pub value_yes: &'static str,
    pub value_no: &'static str,
    pub value_unknown: &'static str,
    pub value_none: &'static str,
    pub manifests_last_check_label: &'static str,
    pub manifests_result_label: &'static str,
    pub manifests_never: &'static str,
    pub manifests_not_checked: &'static str,
    pub manifests_row_active: &'static str,
    pub manifests_row_remote: &'static str,
    pub manifests_local_override_note: &'static str,
    pub integration_not_installed: &'static str,
    pub integration_legacy: &'static str,
    pub integration_current_fmt: &'static str,
    pub integration_needs_repair_fmt: &'static str,
    pub integration_outdated_fmt: &'static str,
    pub integration_experimental_label: &'static str,
    pub explain_agent_label: &'static str,
    pub explain_state_label: &'static str,
    pub explain_manifest_label: &'static str,
    pub explain_rule_label: &'static str,
    pub explain_rule_none: &'static str,
    pub explain_evidence_label: &'static str,
    pub explain_fallback_label: &'static str,
    pub explain_screen_skip_label: &'static str,
    pub explain_skipped_update_label: &'static str,
    pub explain_warning_label: &'static str,
    pub explain_visible_label: &'static str,
    pub explain_cached_remote_label: &'static str,
    pub explain_local_override_label: &'static str,
    pub explain_remote_status_label: &'static str,
    pub explain_remote_error_label: &'static str,
    pub explain_evaluated_rules_header: &'static str,
    pub explain_matchers_label: &'static str,
    pub explain_region_label: &'static str,
    pub api_schema_summary_fmt: &'static str,
    pub api_schema_written_fmt: &'static str,
    pub channel_set_fmt: &'static str,
    pub config_ok: &'static str,
    pub config_issues_found: &'static str,
    pub config_reset_no_file_fmt: &'static str,
    pub config_reset_no_keys_fmt: &'static str,
    pub config_reset_backup_fmt: &'static str,
    pub config_reset_removed_fmt: &'static str,
    pub config_reset_v2_note: &'static str,
    pub config_reset_reload_note: &'static str,
    pub config_reset_restore_fmt: &'static str,
    pub session_stopped_fmt: &'static str,
    pub session_deleted_fmt: &'static str,
    pub session_col_name: &'static str,
    pub session_col_status: &'static str,
    pub session_col_directory: &'static str,
    pub session_col_socket: &'static str,
    pub session_state_running: &'static str,
    pub session_state_stopped: &'static str,
    pub plugin_installed_fmt: &'static str,
    pub plugin_config_label: &'static str,
    pub plugin_uninstalled_fmt: &'static str,
    pub plugin_none_installed: &'static str,
    pub plugin_list_count_fmt: &'static str,
    pub plugin_warning_count_fmt: &'static str,
    pub plugin_config_path_label: &'static str,
    pub plugin_warning_label: &'static str,
}

/// User-visible CLI error strings: messages `eprintln!`ed to the operator or
/// returned as errors that main eventually prints. JSON error-response codes
/// and payloads stay byte-identical; only the human-readable message text is
/// localized. `*_fmt` entries are `fill` templates; each documents its
/// placeholder names in a trailing `// args:` comment.
pub struct CliErrorTexts {
    // Shared prefixes and generic parser messages.
    pub error_prefix: &'static str,
    pub missing_value_for_fmt: &'static str,   // args: flag
    pub unknown_option_fmt: &'static str,      // args: option
    pub unexpected_argument_fmt: &'static str, // args: argument
    pub invalid_flag_value_fmt: &'static str,  // args: flag, value

    // src/cli.rs
    pub token_must_use_name_value: &'static str,
    pub token_name_empty: &'static str,
    pub env_must_use_key_value: &'static str,
    pub env_key_empty: &'static str,
    pub env_nul_bytes: &'static str,
    pub channel_set_usage: &'static str,
    pub config_invalid_toml_channel_fmt: &'static str, // args: path, error
    pub channel_change_invalid_toml_fmt: &'static str, // args: path, error
    pub update_failed_fmt: &'static str,               // args: error
    pub update_retry_hint: &'static str,
    pub config_check_usage: &'static str,
    pub config_reset_keys_usage: &'static str,
    pub config_invalid_toml_manual_fix_fmt: &'static str, // args: path, error
    pub config_top_level_table_fmt: &'static str,         // args: path
    pub config_keys_remove_unsafe_fmt: &'static str,      // args: path
    pub config_keys_remove_invalid_toml_fmt: &'static str, // args: path, error
    pub session_list_usage: &'static str,
    pub session_attach_usage: &'static str,
    pub session_stop_usage: &'static str,
    pub session_delete_usage: &'static str,
    pub terminal_attach_usage: &'static str,
    pub unknown_terminal_session_option_fmt: &'static str, // args: command, option
    pub terminal_dimension_range_fmt: &'static str,        // args: flag, max
    pub terminal_dimension_positive_fmt: &'static str,     // args: flag
    pub terminal_title_set_usage: &'static str,
    pub terminal_title_clear_usage: &'static str,
    pub terminal_title_help_clear_line: &'static str,
    pub invalid_split_direction_fmt: &'static str, // args: value
    pub invalid_read_source_fmt: &'static str,     // args: value
    pub invalid_read_format_fmt: &'static str,     // args: value
    pub invalid_agent_status_fmt: &'static str,    // args: value
    pub invalid_pane_agent_state_fmt: &'static str, // args: value
    pub server_ping_no_protocol: &'static str,

    // src/cli/protocol_guard.rs
    pub protocol_newer_fmt: &'static str, // args: client_protocol, server_protocol, restart_guidance
    pub protocol_older_fmt: &'static str, // args: client_protocol, server_protocol

    // src/cli/server_not_running.rs
    pub no_server_running_fmt: &'static str, // args: path, command

    // src/cli/target.rs
    pub machine_specified_twice: &'static str,
    pub machine_requires_value: &'static str,
    pub machine_requires_saved_label: &'static str,
    pub machine_no_other_launch_options: &'static str,
    pub machine_prefix_usage: &'static str,
    pub machine_unknown_fmt: &'static str, // args: selector
    pub machine_label_ambiguous_fmt: &'static str, // args: selector
    pub machine_disabled_fmt: &'static str, // args: selector
    pub machine_unsupported_command_fmt: &'static str, // args: command, subcommand
    pub machine_bridge_error_fmt: &'static str, // args: label, error
    pub machine_session_error_fmt: &'static str, // args: label, session, error
    pub machine_restart_guidance_fmt: &'static str, // args: label, session

    // src/cli/machine.rs
    pub machine_list_usage: &'static str,
    pub machine_add_usage: &'static str,
    pub machine_rename_usage: &'static str,
    pub machine_remove_usage: &'static str,
    pub machine_set_enabled_usage_fmt: &'static str, // args: action
    pub machine_add_unknown_option_fmt: &'static str, // args: option
    pub remote_session_specified_twice: &'static str,
    pub label_specified_twice: &'static str,
    pub label_required: &'static str,
    pub machine_not_saved_fmt: &'static str, // args: error
    pub machine_prepared_not_saved_fmt: &'static str, // args: error
    pub machine_profile_not_found_fmt: &'static str, // args: id

    // src/cli/api.rs
    pub api_schema_usage: &'static str,
    pub api_snapshot_usage: &'static str,

    // src/cli/agent.rs
    pub agent_list_usage: &'static str,
    pub agent_get_usage: &'static str,
    pub agent_focus_usage: &'static str,
    pub agent_attach_usage: &'static str,
    pub agent_wait_usage: &'static str,
    pub agent_rename_usage: &'static str,
    pub agent_prompt_usage: &'static str,
    pub agent_send_keys_usage: &'static str,
    pub agent_read_usage: &'static str,
    pub agent_start_usage: &'static str,
    pub agent_explain_usage: &'static str,
    pub agent_explain_target_usage: &'static str,
    pub agent_explain_file_usage: &'static str,
    pub agent_explain_file_json_usage: &'static str,
    pub agent_explain_file_requires_agent: &'static str,
    pub agent_explain_file_read_failed_fmt: &'static str, // args: path, error
    pub agent_only_with_file: &'static str,
    pub format_invalid_fmt: &'static str, // args: value
    pub kind_required: &'static str,
    pub pane_flag_required: &'static str,
    pub agent_kind_unsupported_fmt: &'static str, // args: kind
    pub agent_start_no_terminal_id: &'static str,
    pub agent_attach_no_terminal_id: &'static str,
    pub agent_kind_mismatch_fmt: &'static str, // args: expected, detected
    pub agent_blocked_during_startup_fmt: &'static str, // args: name
    pub agent_exited_before_interactive: &'static str,
    pub agent_name_lost_fmt: &'static str, // args: name
    pub agent_start_timeout: &'static str,
    pub until_requires_status: &'static str,
    pub until_requires_wait: &'static str,
    pub timeout_requires_wait: &'static str,
    pub agent_prompt_requires_text: &'static str,

    // src/cli/pane.rs
    pub pane_get_usage: &'static str,
    pub pane_neighbor_usage: &'static str,
    pub pane_focus_direction_usage: &'static str,
    pub pane_resize_usage: &'static str,
    pub pane_rename_usage: &'static str,
    pub pane_read_usage: &'static str,
    pub pane_input_usage: &'static str,
    pub pane_split_usage: &'static str,
    pub pane_move_usage: &'static str,
    pub pane_swap_usage: &'static str,
    pub pane_close_usage: &'static str,
    pub pane_send_text_usage: &'static str,
    pub pane_send_keys_usage: &'static str,
    pub pane_run_usage: &'static str,
    pub pane_wait_output_usage: &'static str,
    pub pane_report_agent_usage: &'static str,
    pub pane_report_agent_session_usage: &'static str,
    pub pane_release_agent_usage: &'static str,
    pub pane_report_metadata_usage: &'static str,
    pub invalid_amount_fmt: &'static str, // args: value
    pub invalid_ratio_fmt: &'static str,  // args: value
    pub zoom_mode_conflict: &'static str,
    pub pane_selector_conflict: &'static str,
    pub current_requires_env_pane: &'static str,
    pub invalid_right_click_target_fmt: &'static str, // args: value
    pub invalid_split_direction_expected_fmt: &'static str, // args: value
    pub invalid_pane_direction_fmt: &'static str,     // args: value
    pub match_regex_exclusive: &'static str,
    pub match_or_regex_required: &'static str,
    pub source_required: &'static str,
    pub agent_flag_required: &'static str,
    pub state_required: &'static str,
    pub state_label_format: &'static str,
    pub unknown_state_label_fmt: &'static str, // args: status
    pub metadata_set_clear_conflict: &'static str,
    pub metadata_field_required: &'static str,

    // src/cli/plugin.rs
    pub plugin_link_usage: &'static str,
    pub plugin_install_usage: &'static str,
    pub plugin_install_usage_short: &'static str,
    pub plugin_install_v1_shorthand_only: &'static str,
    pub plugin_install_requires_yes: &'static str,
    pub plugin_install_cancelled: &'static str,
    pub plugin_config_dir_usage: &'static str,
    pub plugin_uninstall_usage: &'static str,
    pub plugin_unlink_usage: &'static str,
    pub plugin_set_enabled_usage_fmt: &'static str, // args: action
    pub plugin_not_installed_fmt: &'static str,     // args: target
    pub plugin_limit_invalid_fmt: &'static str,     // args: value
    pub plugin_action_invoke_usage: &'static str,
    pub plugin_required: &'static str,
    pub entrypoint_required: &'static str,
    pub plugin_pane_focus_usage: &'static str,
    pub plugin_pane_close_usage: &'static str,
    pub plugin_pane_placement_invalid_fmt: &'static str, // args: value
    pub plugin_remote_path_absolute: &'static str,
    pub plugin_already_linked_local_fmt: &'static str, // args: plugin
    pub github_segment_empty_fmt: &'static str,        // args: label
    pub github_segment_invalid_fmt: &'static str,      // args: label, value
    pub github_segment_invalid_chars_fmt: &'static str, // args: label, value
    pub plugin_subdir_invalid_fmt: &'static str,       // args: value
    pub command_failed_fmt: &'static str,              // args: program, status
    pub command_failed_stderr_fmt: &'static str,       // args: program, status, stderr
    pub plugin_build_failed: &'static str,
    pub plugin_build_start_failed_fmt: &'static str, // args: error
    pub plugin_build_wait_failed_fmt: &'static str,  // args: error
    pub plugin_build_status_fmt: &'static str,       // args: status
    pub build_output_truncated_fmt: &'static str,    // args: label, max
    pub plugin_not_installed_after_failure: &'static str,
    pub plugin_build_command_empty: &'static str,
    pub plugin_build_changed_manifest: &'static str,
    pub plugin_server_source_metadata_missing: &'static str,
    pub plugin_registration_undo_failed_fmt: &'static str, // args: error, detail
    pub plugin_refusing_unmanaged_delete_fmt: &'static str, // args: path
    pub plugin_checkout_lifecycle_fmt: &'static str,       // args: operation, path, error

    // src/cli/status.rs
    pub status_server_usage: &'static str,
    pub status_client_usage: &'static str,

    // src/cli/server.rs
    pub server_stop_usage: &'static str,
    pub server_reload_config_usage: &'static str,
    pub server_agent_manifests_usage: &'static str,
    pub server_reload_agent_manifests_usage: &'static str,
    pub server_update_agent_manifests_usage: &'static str,
    pub server_live_handoff_usage: &'static str,
    pub manifests_update_failed_fmt: &'static str, // args: error

    // src/cli/completion.rs
    pub unknown_shell_fmt: &'static str,    // args: shell
    pub completion_usage_fmt: &'static str, // args: shells

    // src/cli/worktree.rs
    pub worktree_list_usage: &'static str,
    pub worktree_create_usage: &'static str,
    pub worktree_open_usage: &'static str,
    pub worktree_remove_usage: &'static str,
    pub remote_worktree_path_absolute: &'static str,

    // src/cli/workspace.rs
    pub workspace_list_usage: &'static str,
    pub workspace_get_usage: &'static str,
    pub workspace_focus_usage: &'static str,
    pub workspace_rename_usage: &'static str,
    pub workspace_report_metadata_usage: &'static str,
    pub workspace_close_usage: &'static str,
    pub workspace_token_required: &'static str,

    // src/cli/tab.rs
    pub tab_get_usage: &'static str,
    pub tab_focus_usage: &'static str,
    pub tab_rename_usage: &'static str,
    pub tab_close_usage: &'static str,

    // src/cli/notification.rs
    pub notification_show_usage: &'static str,
    pub invalid_position_fmt: &'static str, // args: value
    pub invalid_sound_fmt: &'static str,    // args: value

    // src/cli/integration.rs
    pub integration_status_usage: &'static str,
    pub integration_target_usage_fmt: &'static str, // args: action
    pub integration_target_unknown_fmt: &'static str, // args: target
    pub integration_targets_supported: &'static str,

    // src/integration (registry notice + install support check)
    pub integrations_need_updating_fmt: &'static str, // args: instructions
    pub integration_instructions_run_fmt: &'static str, // args: command
    pub integration_instructions_run_list_fmt: &'static str, // args: commands, last
    pub integration_not_supported_windows_fmt: &'static str, // args: target

    // src/update.rs
    pub self_update_disabled_homebrew_preview: &'static str,
    pub self_update_disabled_homebrew: &'static str,
    pub self_update_disabled_mise_preview: &'static str,
    pub self_update_disabled_mise: &'static str,
    pub self_update_disabled_nix_preview: &'static str,
    pub self_update_disabled_nix: &'static str,
    pub update_run_outside: &'static str,
    pub update_usage: &'static str,
    pub unknown_update_option_fmt: &'static str, // args: option
    pub preview_rejection_homebrew: &'static str,
    pub preview_rejection_mise: &'static str,
    pub preview_rejection_nix: &'static str,
    pub curl_failed_fmt: &'static str, // args: error
    pub manifest_fetch_failed: &'static str,
    pub manifest_parse_failed_fmt: &'static str, // args: error
    pub manifest_invalid_version_fmt: &'static str, // args: version
    pub manifest_missing_release_metadata_fmt: &'static str, // args: version
    pub manifest_notes_empty: &'static str,
    pub manifest_no_binary_fmt: &'static str, // args: key
    pub manifest_asset_missing_sha256_fmt: &'static str, // args: key
    pub preview_channel_invalid_fmt: &'static str, // args: channel
    pub preview_build_id_empty: &'static str,
    pub preview_base_version_invalid_fmt: &'static str, // args: version
    pub preview_notes_empty: &'static str,
    pub preview_no_binary_fmt: &'static str, // args: key
    pub manifest_asset_unsupported_format_fmt: &'static str, // args: format
    pub asset_url_empty: &'static str,
    pub asset_url_invalid: &'static str,
    pub homebrew_fetch_failed: &'static str,
    pub current_binary_not_found_fmt: &'static str, // args: error
    pub binary_directory_not_found: &'static str,
    pub install_dir_not_writable_fmt: &'static str, // args: path, error
    pub download_failed: &'static str,
    pub download_failed_fmt: &'static str, // args: error
    pub checksum_failed_fmt: &'static str, // args: error
    pub chmod_failed_fmt: &'static str,    // args: error
    pub update_temp_file_missing: &'static str,
    pub replace_binary_failed_fmt: &'static str, // args: error
    pub windows_sha256_missing: &'static str,
    pub windows_installer_run_failed_fmt: &'static str, // args: error
    pub windows_installer_failed_fmt: &'static str,     // args: status
    pub localappdata_missing: &'static str,
    pub target_status_failed_fmt: &'static str, // args: label, path, error, command
    pub target_status_no_response_fmt: &'static str, // args: label, path, command
    pub target_client_socket_no_response_fmt: &'static str, // args: label, path, command
    pub server_listening_status_unavailable_fmt: &'static str, // args: command
    pub sessions_list_failed_fmt: &'static str, // args: error
    pub sessions_must_stop_noninteractive: &'static str,
    pub prompt_flush_failed_fmt: &'static str, // args: error
    pub prompt_read_failed_fmt: &'static str,  // args: error
    pub server_connect_failed_fmt: &'static str, // args: error
    pub server_write_timeout_fmt: &'static str, // args: action, error
    pub server_read_timeout_fmt: &'static str, // args: action, error
    pub server_send_failed_fmt: &'static str,  // args: action, error
    pub server_finish_failed_fmt: &'static str, // args: action, error
    pub server_flush_failed_fmt: &'static str, // args: action, error
    pub server_response_read_failed_fmt: &'static str, // args: action, error
    pub server_response_empty_fmt: &'static str, // args: action
    pub server_response_invalid_fmt: &'static str, // args: error
    pub server_action_failed_fmt: &'static str, // args: action, error
    pub shutdown_confirm_failed_fmt: &'static str, // args: path, error
    pub server_still_responding_fmt: &'static str, // args: path, seconds
    pub post_handoff_status_failed_fmt: &'static str, // args: error
    pub handoff_no_compatible_server_fmt: &'static str, // args: path, seconds
}

pub struct Texts {
    pub chrome: ChromeTexts,
    pub onboarding: OnboardingTexts,
    pub context_menu: ContextMenuTexts,
    pub global_menu: GlobalMenuTexts,
    pub keybinds: KeybindTexts,
    pub overlays: OverlayTexts,
    pub dialogs: DialogTexts,
    pub worktree: WorktreeTexts,
    pub settings: SettingsTexts,
    pub sidebar: SidebarTexts,
    pub status: StatusTexts,
    pub mode_bar: ModeBarTexts,
    pub notify: NotifyTexts,
    pub mobile: MobileTexts,
    pub update: UpdateTexts,
    pub endpoint: EndpointTexts,
    pub cli_help: CliHelpTexts,
    pub cli_output: CliOutputTexts,
    pub cli_errors: CliErrorTexts,
}

/// Runtime placeholder substitution for table-held format templates:
/// `fill("{n} 个窗格", &[("n", "3")])` -> "3 个窗格". Rust's `format!` only
/// accepts literal templates, so translated templates are filled manually.
pub fn fill(template: &str, args: &[(&str, &str)]) -> String {
    let mut output = template.to_owned();
    for (name, value) in args {
        output = output.replace(&format!("{{{name}}}"), value);
    }
    output
}

/// Display name for a canonical theme value. English shows the canonical
/// value; zh-CN maps to a friendly name and falls back to the value.
pub fn theme_display_name(canonical: &str) -> &'static str {
    if lang() == Lang::En {
        return canonical_theme_fallback(canonical);
    }
    zh_cn::THEME_DISPLAY
        .iter()
        .find(|(value, _)| *value == canonical)
        .map(|(_, display)| *display)
        .unwrap_or_else(|| canonical_theme_fallback(canonical))
}

fn canonical_theme_fallback(canonical: &str) -> &'static str {
    crate::config::THEME_NAMES
        .iter()
        .find(|name| **name == canonical)
        .copied()
        .unwrap_or("")
}

pub fn texts() -> &'static Texts {
    texts_for(lang())
}

pub fn texts_for(lang: Lang) -> &'static Texts {
    match lang {
        Lang::ZhCn => &zh_cn::TEXTS,
        Lang::En => &en::TEXTS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lang_deserialize_falls_back_to_default_on_unknown_value() {
        let loaded: crate::config::Config =
            toml::from_str("language = \"fr\"").expect("unknown language must not fail config");
        assert_eq!(loaded.language, Lang::ZhCn);

        let loaded: crate::config::Config =
            toml::from_str("language = \"en\"").expect("known language parses");
        assert_eq!(loaded.language, Lang::En);
    }

    #[test]
    fn lang_parses_config_values_and_rejects_others() {
        assert_eq!(Lang::parse("zh-CN"), Some(Lang::ZhCn));
        assert_eq!(Lang::parse(" en "), Some(Lang::En));
        assert_eq!(Lang::parse("zh"), None);
        assert_eq!(Lang::parse(""), None);
    }

    #[test]
    fn default_language_is_zh_cn() {
        let _guard = lang_guard(Lang::ZhCn);
        assert_eq!(Lang::default(), Lang::ZhCn);
        assert_eq!(lang(), Lang::ZhCn);
    }

    #[test]
    fn lang_guard_restores_previous_language() {
        let _outer = lang_guard(Lang::ZhCn);
        {
            let _inner = lang_guard(Lang::En);
            assert_eq!(lang(), Lang::En);
        }
        assert_eq!(lang(), Lang::ZhCn);
    }

    #[test]
    fn texts_switch_with_language() {
        assert_eq!(texts_for(Lang::En).chrome.close_button, " esc close ");
        assert_eq!(texts_for(Lang::ZhCn).chrome.close_button, " esc 关闭 ");
        assert_eq!(texts_for(Lang::En).chrome.continue_button, " ↵ continue ");
        assert_eq!(texts_for(Lang::ZhCn).chrome.continue_button, " ↵ 继续 ");
    }

    #[test]
    fn init_early_prefers_env_over_config() {
        let _guard = lang_guard(Lang::ZhCn);
        // Without HERDR_LANG set the config peek decides; this environment
        // may not have a config file, so only assert the env precedence.
        std::env::set_var(LANG_ENV_VAR, "en");
        init_early();
        assert_eq!(lang(), Lang::En);
        std::env::remove_var(LANG_ENV_VAR);
    }
}
