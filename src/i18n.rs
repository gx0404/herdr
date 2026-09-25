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

/// Test helper: whether `text` has CJK ideographs or full-width punctuation —
/// the English interface must not show any (docs audit D7).
#[cfg(test)]
pub fn has_cjk(text: &str) -> bool {
    text.chars().any(|ch| {
        matches!(
            ch,
            '\u{3000}'..='\u{303f}'
                | '\u{3400}'..='\u{4dbf}'
                | '\u{4e00}'..='\u{9fff}'
                | '\u{ff00}'..='\u{ffef}'
        )
    })
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
    pub popup_title: &'static str,
}

pub struct OnboardingTexts {
    pub subtitle: &'static str,
    pub description: [&'static str; 3],
    pub prefix_suffix: &'static str,
    pub help_suffix: &'static str,
    pub next: &'static str,
}

pub struct ContextMenuTexts {
    pub copy_selection: &'static str,
    pub rename: &'static str,
    pub close: &'static str,
    pub close_group: &'static str,
    pub close_pane: &'static str,
    pub new_worktree: &'static str,
    pub open_worktree: &'static str,
    pub delete_worktree: &'static str,
    pub collapse_group: &'static str,
    pub new_tab: &'static str,
    pub rename_pane: &'static str,
    pub clear_pane_name: &'static str,
    pub swap_with_focused: &'static str,
    pub split_right: &'static str,
    pub split_down: &'static str,
    pub zoom: &'static str,
    pub send_right_clicks: &'static str,
    pub manage_machines: &'static str,
    pub edit_machine: &'static str,
    pub reconnect_machine: &'static str,
    pub switch_machine: &'static str,
    pub enable_machine: &'static str,
    pub remove_machine: &'static str,
    pub copy_machine_fix_command: &'static str,
}

pub struct GlobalMenuTexts {
    pub resize_hint: &'static str,
    pub main_menu: &'static str,
    pub command_search: &'static str,
    pub categories: [&'static str; 7],
    pub back: &'static str,
    pub settings: &'static str,
    pub machines: &'static str,
    pub notifications: &'static str,
    pub keybinds: &'static str,
    pub reload_config: &'static str,
    pub update_ready: &'static str,
    pub whats_new: &'static str,
    pub detach: &'static str,
    // Command palette (fuzzy search over every available action).
    pub search_hint: &'static str,
    pub recent: &'static str,
    pub no_matches: &'static str,
    pub footer_run: &'static str,
    pub footer_select: &'static str,
    pub footer_close: &'static str,
    pub machine_connect_fmt: &'static str,  // args: label
    pub machine_switch_fmt: &'static str,   // args: label
    pub machine_edit_fmt: &'static str,     // args: label
    pub machine_enable_fmt: &'static str,   // args: label
    pub machine_rename_fmt: &'static str,   // args: label
    pub machine_remove_fmt: &'static str,   // args: label
    pub machine_copy_fix_fmt: &'static str, // args: label
    pub machine_import: &'static str,
    pub snippets: &'static str,
    pub snippet_run: &'static str,
    pub scene_save: &'static str,
    pub scene_restore: &'static str,
    pub broadcast: &'static str,
}

/// Notification history overlay (global menu → notifications).
pub struct HistoryTexts {
    pub title: &'static str,
    pub empty: &'static str,
    pub footer: &'static str,
    pub just_now: &'static str,
    pub minutes_ago_fmt: &'static str, // args: n
    pub hours_ago_fmt: &'static str,   // args: n
    pub days_ago_fmt: &'static str,    // args: n
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
    pub manage_machines: &'static str,
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
    pub clear_pane: &'static str,
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
    pub link_hints: &'static str,
    pub custom_command: &'static str,
}

pub struct OverlayTexts {
    pub update_ready: &'static str,
    pub release_preview_title: &'static str,
    pub whats_new_in_release: &'static str,
    pub product_announcement_preview: &'static str,
    pub product_announcement: &'static str,
    pub keybinds_title: &'static str,
    pub hint_scroll: &'static str,
    pub footer_close: &'static str,
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
    pub link_hints_no_links: &'static str,
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
    pub force_confirm_hint: &'static str,
    pub removing: &'static str,
    pub delete_anyway: &'static str,
    pub remove: &'static str,
    pub branch_required: &'static str,
    pub start_from_parent: &'static str,
    pub not_a_worktree_checkout: &'static str,
    pub no_worktrees_found: &'static str,
    pub unexpected_result: &'static str,
    pub prunable_cannot_open: &'static str,
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
    /// Integrations 分区专用页脚：与 `integrations_hint` 用同一套动词
    /// （Enter 安装选中），不重用通用 `footer` 的「应用」措辞——两者原本
    /// 各写各的，Enter 的效果在头部与页脚间自相矛盾（冒烟 L7）。
    pub footer_integrations: &'static str,
    /// Appended to the footer in the Integrations section: `a` installs every
    /// pending target without a confirmation, so it belongs in the key hints.
    pub footer_install_all: &'static str,
    pub integrations: &'static str,
    pub integrations_hint: &'static str,
    pub loading: &'static str,
    pub no_targets: &'static str,
    pub state_installed: &'static str,
    pub state_update_available: &'static str,
    pub state_available: &'static str,
    pub state_not_found: &'static str,
    pub installing: &'static str,
    /// 安装进行中按 Esc 的反馈：不静默吞键（TOOL-21）。
    pub install_in_progress: &'static str,
    /// One-shot feedback when the selected integration row needs no install.
    /// Kept apart from `integration_messages`, which carries the endpoint's
    /// install report and must survive this hint.
    pub nothing_to_install: &'static str,
    pub unexpected_list_result: &'static str,
    pub unexpected_install_result: &'static str,
}

/// 监控 → 账号 / 设置页（本批新增文案；存量 `tr()` 的迁移留批次四）。
pub struct MonitorTexts {
    /// 账号页工具栏首个 chip：回到跨厂商总览。
    pub all_providers: &'static str,
    /// 折叠标记后的说明：还有 N 个厂商放不下。
    pub more_fmt: &'static str, // args: n
    pub callback_toggle: &'static str,
    pub settings_button: &'static str,
    pub format_dashboard: &'static str,
    pub format_table: &'static str,
    pub position_hover: &'static str,
    pub position_page: &'static str,
    pub position_both: &'static str,
    pub scope_api_key: &'static str,
    pub updated_ago_fmt: &'static str, // args: age
    pub never_updated: &'static str,
    pub resets_in_fmt: &'static str, // args: span
    /// 汇总行账号数（n ≥ 2）；单账号用 `account_one`，避免英文「1 accounts」。
    pub accounts_fmt: &'static str, // args: n
    pub account_one: &'static str,
    pub summary_ready_fmt: &'static str,     // args: n
    pub summary_loading_fmt: &'static str,   // args: n
    pub summary_attention_fmt: &'static str, // args: n
    pub summary_failed_fmt: &'static str,    // args: n
    pub col_reset: &'static str,
    pub col_freshness: &'static str,
    pub col_usage: &'static str,
    pub detail_title: &'static str,
    pub detail_auth: &'static str,
    pub detail_provider: &'static str,
    pub detail_source: &'static str,
    pub detail_window: &'static str,
    pub detail_updated: &'static str,
    pub detail_docs: &'static str,
    pub detail_history: &'static str,
    pub detail_history_waiting: &'static str,
    pub detail_history_fmt: &'static str,
    pub detail_none: &'static str,
    pub section_monitor: &'static str,
    pub section_usage: &'static str,
    pub section_cards: &'static str,
    pub section_alerts: &'static str,
    pub section_providers: &'static str,
    pub section_devices: &'static str,
    pub hover_delay: &'static str,
    pub api_refresh: &'static str,
    pub cli_refresh: &'static str,
    pub interactive_probe: &'static str,
    /// 设置页只读行的来源标注：这些值来自本机 config.toml，连远端 endpoint 时与实际
    /// 生效值无关，文案必须说明「本机」。
    pub config_value_hint: &'static str,
    pub hover_closed_hint: &'static str,
    /// 官方回调开关没有作用对象（多账号且未选）时的禁用态标注与动作提示。
    pub select_account_first: &'static str,
    pub on: &'static str,
    pub off: &'static str,
    // ---- 页签与页脚 ----
    pub tab_system: &'static str,
    pub tab_accounts: &'static str,
    /// 第三个页签：监控偏好（不再叫「设置」，与全局设置浮层区分）。
    pub tab_preferences: &'static str,
    pub hint_refresh: &'static str,
    pub hint_pause: &'static str,
    pub hint_resume: &'static str,
    pub hint_close: &'static str,
    // ---- 系统页 ----
    pub edit_layout: &'static str,
    pub edit_layout_done: &'static str,
    /// 编辑布局模式的页脚说明（↑↓ 键）。
    pub edit_layout_hint: &'static str,
    pub uptime_fmt: &'static str,       // args: span
    pub interval_fmt: &'static str,     // args: ms
    pub logical_cpus_fmt: &'static str, // args: n
    pub mem_used: &'static str,
    pub swap: &'static str,
    pub cache_fmt: &'static str, // args: size
    pub gpu_util: &'static str,
    pub vram: &'static str,
    pub col_mount: &'static str,
    pub col_device: &'static str,
    pub col_used: &'static str,
    pub col_total: &'static str,
    pub col_percent: &'static str,
    pub no_disks: &'static str,
    pub net_idle_one: &'static str,
    pub net_idle_folded_fmt: &'static str, // args: n
    pub temp_avg_fmt: &'static str,        // args: avg
    pub proc_col_pid: &'static str,
    pub proc_col_name: &'static str,
    pub proc_col_cpu: &'static str,
    pub proc_col_mem: &'static str,
    pub no_matching_processes: &'static str,
    // ---- 监控偏好页 ----
    pub chart_glyphs: &'static str,
    pub glyph_braille: &'static str,
    pub glyph_blocks: &'static str,
    pub glyph_ascii: &'static str,
    pub sampling_interval: &'static str,
    pub card_height: &'static str,
    pub history_range: &'static str,
    pub alerts_enabled: &'static str,
    pub usage_enabled: &'static str,
    pub usage_format: &'static str,
    pub usage_position: &'static str,
    pub restore_config: &'static str,
    pub clear_overrides: &'static str,
    pub no_overrides: &'static str,
    pub alert_rule_fmt: &'static str, // args: metric
    pub alert_duration: &'static str,
    pub alert_cooldown: &'static str,
    /// 告警规则的指标显示名（偏好页告警行与资源告警通知共用）：配置里的
    /// `cpu` / `memory` / `gpu` / `disk` 是 id，不直接给人看。
    pub alert_metric_cpu: &'static str,
    pub alert_metric_memory: &'static str,
    pub alert_metric_gpu: &'static str,
    pub alert_metric_disk: &'static str,
    // ---- 账号页厂商卡片 ----
    /// 账号卡片（厂商专属）：额度窗口短标签。
    pub quota_5h: &'static str,
    pub quota_weekly: &'static str,
    pub quota_spend: &'static str,
    pub quota_primary: &'static str,
    pub quota_secondary: &'static str,
    pub quota_7d: &'static str,
    pub quota_monthly: &'static str,
    pub quota_monthly_code: &'static str,
    pub quota_plan: &'static str,
    /// codex 主 / 次窗口知道长度时的标签（`5h` / `7d`）。
    pub window_fmt: &'static str, // args: span
    /// 额度窗口已过重置时间、沿用上次值时的说明。
    pub window_expired: &'static str,
    /// 余额与钱包。
    pub credits: &'static str,
    pub balance_available: &'static str,
    pub balance_voucher: &'static str,
    pub balance_cash: &'static str,
    pub section_extra_usage: &'static str,
    pub extra_balance: &'static str,
    pub extra_total: &'static str,
    pub extra_month_used: &'static str,
    pub extra_month_cap: &'static str,
    /// 会话与本地统计。
    pub cost: &'static str,
    pub duration: &'static str,
    pub api_duration: &'static str,
    pub context: &'static str,
    pub context_tokens: &'static str,
    pub context_window: &'static str,
    pub sessions: &'static str,
    pub subagents: &'static str,
    pub section_tokens: &'static str,
    pub tokens_input: &'static str,
    pub tokens_output: &'static str,
    pub tokens_reasoning: &'static str,
    pub tokens_cache_read: &'static str,
    pub tokens_cache_write: &'static str,
    pub tokens_total: &'static str,
    pub tokens_main: &'static str,
    pub tokens_subagents: &'static str,
    pub tool_uses: &'static str,
    pub stats_window: &'static str,
    pub model: &'static str,
    /// 卡片徽标：本机统计 / 会话统计不是账号额度。
    pub local_stats_badge: &'static str,
    pub session_stats_badge: &'static str,
    /// 空态与未知值。
    pub no_usage_data: &'static str,
    pub no_session_data: &'static str,
    pub no_data_yet: &'static str,
    pub no_accounts: &'static str,
}

/// 服务端写进账号用量快照 `message` 的固定说明（文档终审 D2）。服务端按自己的语言
/// 生成（`texts()`，与 toast、通知同一口径，随 config.toml 的 `language` 同步）；客户端
/// 显示前经 [`localize_usage_notice`] 对照各语言的原文，认得的换成界面语言——远端机器
/// 或 `HERDR_LANG` 不同的 server 发来的说明也跟着界面语言走；认不出的原样显示。
pub struct UsageNoticeTexts {
    pub signed_in: &'static str,
    /// `claude auth status` 的输出解析不了时的登录态。
    pub login_unknown: &'static str,
    /// 官方回调已接入、等待第一次回调。
    pub callback_on_fmt: &'static str, // args: login
    /// 官方回调未接入：指引写账号页上「官方回调」开关的真实名字。
    pub callback_off_fmt: &'static str, // args: login
    /// 读不出官方回调的接入态（settings.json 无法解析等）。
    pub callback_unknown_fmt: &'static str, // args: login
    /// 交互探测的失败原因接在等待说明之后时的分隔语。
    pub interactive_failure_sep: &'static str,
    /// 支持官方回调的厂商停在目录信任对话、且没有稳定探测目录时的提示。
    pub trust_callback_hint: &'static str,
    /// 支持官方回调的厂商停在登录对话时的提示。
    pub sign_in_callback_hint: &'static str,
    /// 交互探测停在目录信任对话、有稳定探测目录时（claude 的生产路径）的提示：给出可以
    /// 照抄的目录与命令。带参数，客户端按模板反解后换成界面语言（见 `templates`）。
    pub trust_dir_fmt: &'static str, // args: dir, command
    /// 不支持官方回调的厂商停在目录信任对话、且没有稳定探测目录时的提示。
    pub trust_hint: &'static str,
    /// 不支持官方回调的厂商停在登录对话时的提示。
    pub sign_in_hint: &'static str,
    /// 交互探测在截止时间前没等到就绪提示或用量输出。
    pub probe_timeout: &'static str,
    // ---- 账号卡与表格「说明」里的固定说明（文档终审 D7）----
    /// 冷条目还没有查询过。
    pub not_queried_yet: &'static str,
    /// 该厂商在设置里被关闭。
    pub provider_disabled: &'static str,
    /// 官方回调超过闩锁时长没有更新，显示缓存样本。
    pub callback_stale: &'static str,
    /// 集成扩展（pi）超过闩锁时长没有推送，显示缓存的会话统计。
    pub extension_push_stale: &'static str,
    /// 未绑定窗格且无法推断账号时的占位。
    pub binding_placeholder: &'static str,
    /// claude 登录预检报告未登录。
    pub claude_signed_out: &'static str,
    /// pi 还没有推送会话用量。
    pub pi_waiting: &'static str,
    /// 官方输出里没有已验证的用量字段。
    pub no_verified_fields: &'static str,
    pub codex_ordinary_usage_allowed: &'static str,
    pub codex_ordinary_usage_blocked: &'static str,
    /// 官方账号身份变化、绑定已撤销。
    pub identity_changed: &'static str,
    /// zcode 本地统计卡的来源声明。
    pub zcode_local: &'static str,
    /// 按唯一账号自动绑定。
    pub auto_bound: &'static str,
    /// 官方回调暂无额度字段。
    pub no_quota: &'static str,
    /// 集成扩展的推送暂无用量字段。
    pub no_push_usage: &'static str,
    /// 上报被拒：窗格还没有绑定该厂商的账号（`account.usage.report` 的错误说明）。
    pub binding_required: &'static str,
    pub invalid_report: &'static str,
    /// 本机数据库轮询型来源（zcode）收到上报时的拒绝说明。
    pub local_source_report: &'static str,
    pub binding_candidates_fmt: &'static str, // args: message, candidates
    /// 候选账号之间的分隔符。
    pub candidate_separator: &'static str,
    pub binding_no_quota_fmt: &'static str, // args: message, account
}

/// 说明表里的一条说明：固定文案按下标，claude 的等待说明按登录态与回调接入态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UsageNotice {
    Fixed(usize),
    ClaudeWaiting {
        login_known: bool,
        callback: Option<bool>,
    },
}

const CLAUDE_WAITING_VARIANTS: [(bool, Option<bool>); 6] = [
    (true, Some(true)),
    (true, Some(false)),
    (true, None),
    (false, Some(true)),
    (false, Some(false)),
    (false, None),
];

impl UsageNoticeTexts {
    /// claude「已登录（或登录态未知）、等待官方回调」的占位说明。
    pub fn claude_waiting(&self, login_known: bool, callback: Option<bool>) -> String {
        let login = if login_known {
            self.signed_in
        } else {
            self.login_unknown
        };
        let template = match callback {
            Some(true) => self.callback_on_fmt,
            Some(false) => self.callback_off_fmt,
            None => self.callback_unknown_fmt,
        };
        fill(template, &[("login", login)])
    }

    /// 快照 `message` 里没有参数的固定说明，按下标与各语言对齐。只进上报应答的拒绝
    /// 说明（`binding_required` 等）不在其中：客户端不显示它们。
    fn fixed(&self) -> [&'static str; 20] {
        [
            self.trust_callback_hint,
            self.sign_in_callback_hint,
            self.trust_hint,
            self.sign_in_hint,
            self.probe_timeout,
            self.not_queried_yet,
            self.provider_disabled,
            self.callback_stale,
            self.extension_push_stale,
            self.binding_placeholder,
            self.claude_signed_out,
            self.pi_waiting,
            self.no_verified_fields,
            self.codex_ordinary_usage_allowed,
            self.codex_ordinary_usage_blocked,
            self.identity_changed,
            self.zcode_local,
            self.auto_bound,
            self.no_quota,
            self.no_push_usage,
        ]
    }

    /// 快照 `message` 里带参数的说明模板，按下标与各语言对齐：客户端按原语言的模板反解
    /// 出参数，再用界面语言的同一模板重填。
    fn templates(&self) -> [&'static str; 1] {
        [self.trust_dir_fmt]
    }

    fn render(&self, notice: UsageNotice) -> std::borrow::Cow<'static, str> {
        match notice {
            UsageNotice::Fixed(index) => {
                std::borrow::Cow::Borrowed(self.fixed().get(index).copied().unwrap_or_default())
            }
            UsageNotice::ClaudeWaiting {
                login_known,
                callback,
            } => std::borrow::Cow::Owned(self.claude_waiting(login_known, callback)),
        }
    }
}

/// 各语言下每条说明的原文（进程内算一次）。
fn usage_notice_catalog() -> &'static [(Lang, UsageNotice, String)] {
    static CATALOG: std::sync::OnceLock<Vec<(Lang, UsageNotice, String)>> =
        std::sync::OnceLock::new();
    CATALOG.get_or_init(|| {
        let mut entries = Vec::new();
        for lang in [Lang::En, Lang::ZhCn] {
            let texts = &texts_for(lang).usage_notice;
            for (index, text) in texts.fixed().iter().enumerate() {
                entries.push((lang, UsageNotice::Fixed(index), (*text).to_owned()));
            }
            for (login_known, callback) in CLAUDE_WAITING_VARIANTS {
                entries.push((
                    lang,
                    UsageNotice::ClaudeWaiting {
                        login_known,
                        callback,
                    },
                    texts.claude_waiting(login_known, callback),
                ));
            }
        }
        entries
    })
}

/// 把服务端生成的账号用量说明换成当前界面语言：整条认得的直接换；等待说明后面拼了
/// 交互探测失败原因的，两段各自换（失败原因认不出就原样保留）；认不出的原样返回。
pub fn localize_usage_notice(message: &str) -> std::borrow::Cow<'_, str> {
    let target = &texts().usage_notice;
    let current = lang();
    let catalog = usage_notice_catalog();
    if let Some((lang, notice, _)) = catalog.iter().find(|(_, _, text)| text == message) {
        return if *lang == current {
            std::borrow::Cow::Borrowed(message)
        } else {
            target.render(*notice)
        };
    }
    if let Some(localized) = localize_templated_notice(message, current) {
        return localized;
    }
    for (lang, notice, text) in catalog {
        if !matches!(notice, UsageNotice::ClaudeWaiting { .. }) {
            continue;
        }
        let separator = texts_for(*lang).usage_notice.interactive_failure_sep;
        let Some(failure) = message
            .strip_prefix(text.as_str())
            .and_then(|rest| rest.strip_prefix(separator))
        else {
            continue;
        };
        if *lang == current {
            return std::borrow::Cow::Borrowed(message);
        }
        return std::borrow::Cow::Owned(format!(
            "{}{}{}",
            target.render(*notice),
            target.interactive_failure_sep,
            localize_usage_notice(failure)
        ));
    }
    std::borrow::Cow::Borrowed(message)
}

/// 带参数的说明（`UsageNoticeTexts::templates`）：按各语言的模板反解出参数；与界面同语言
/// 的原样返回，否则用界面语言的同一模板重填。哪个模板都对不上返回 `None`。
fn localize_templated_notice(message: &str, current: Lang) -> Option<std::borrow::Cow<'_, str>> {
    for lang in [Lang::En, Lang::ZhCn] {
        for (index, template) in texts_for(lang).usage_notice.templates().iter().enumerate() {
            let Some(args) = unfill(template, message) else {
                continue;
            };
            if lang == current {
                return Some(std::borrow::Cow::Borrowed(message));
            }
            let target = texts_for(current).usage_notice.templates()[index];
            return Some(std::borrow::Cow::Owned(fill(target, &args)));
        }
    }
    None
}

/// [`fill`] 的逆运算：按模板的字面段切开 `message`，依次取出各占位（`{name}`）的值。字面段
/// 对不上、或两个占位之间没有字面段（切分有歧义）时返回 `None`；值里又出现后一个字面段时按
/// 第一次出现切分。
fn unfill<'t, 'm>(template: &'t str, message: &'m str) -> Option<Vec<(&'t str, &'m str)>> {
    let mut literals = Vec::new();
    let mut names = Vec::new();
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        let close = open + rest[open..].find('}')?;
        literals.push(&rest[..open]);
        names.push(&rest[open + 1..close]);
        rest = &rest[close + 1..];
    }
    if names.is_empty() {
        return (message == template).then(Vec::new);
    }
    let mut body = message.strip_prefix(literals[0])?.strip_suffix(rest)?;
    let mut values = Vec::with_capacity(names.len());
    for (index, name) in names.iter().enumerate() {
        match literals.get(index + 1) {
            Some(&"") => return None,
            Some(separator) => {
                let (value, remainder) = body.split_once(separator)?;
                values.push((*name, value));
                body = remainder;
            }
            None => values.push((*name, body)),
        }
    }
    Some(values)
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
    /// `wt_*` 是「打开 worktree」浮层（`client::shell::state::ClientWorktreeOpenEntry::
    /// status_label`）的一组状态标签，历史上归在侧栏文案下；侧栏 worktree 行并不显示它们。
    pub wt_open: &'static str,
    pub wt_detached: &'static str,
    pub wt_root: &'static str,
    pub wt_prunable: &'static str,
    pub sort_grouped: &'static str,
    pub no_matching_agents: &'static str,
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
    pub broadcast_fmt: &'static str, // args: count
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
    pub new_workspace: &'static str,
    pub new_tab: &'static str,
    pub close: &'static str,
    pub not_ready_fmt: &'static str,
}

pub struct UpdateTexts {
    pub install_run_fmt: &'static str,
    pub install_nix: &'static str,
}

pub struct EndpointTexts {
    pub local_unavailable: &'static str,
    pub saved_machines_fmt: &'static str, // args: error
    pub st_connecting: &'static str,
    pub st_online: &'static str,
    pub st_reconnecting: &'static str,
    pub st_attention: &'static str,
    pub st_disabled: &'static str,
    pub unknown_endpoint: &'static str,
    pub offline_hint_fmt: &'static str, // args: label, status
    pub workspace_unavailable_hint: &'static str,
    pub custom_command_unavailable: &'static str,
    pub notice_server_timed_out: &'static str,
    pub notice_timeout_body_fmt: &'static str, // args: method
    pub notice_action_interrupted: &'static str,
    pub notice_server_unavailable: &'static str,
    pub notice_action_rejected: &'static str,
    pub notice_action_unavailable: &'static str,
    pub notice_action_not_applicable: &'static str,
    pub notice_unsupported_method_fmt: &'static str, // args: method
    pub notice_paste_rejected: &'static str,
    pub notice_endpoint_unavailable: &'static str,
    pub notice_cancelled_body: &'static str,
    /// 另一台机器上的窗格关闭需要确认（会连带关闭 worktree 分组）：没法替那台机器
    /// 的工作区弹确认框，写明下一步——切到那台机器上再关闭。
    pub notice_remote_close_needs_confirmation_fmt: &'static str, // args: label
    pub unexpected_selection_result: &'static str,
    pub unexpected_link_result: &'static str,
    pub unexpected_copy_motion_result: &'static str,
    pub unexpected_copy_search_result: &'static str,
    pub unexpected_config_reload_result: &'static str,
}

/// Texts for the in-TUI machine manager: the machines overlay (list, detail,
/// add/edit form), its bootstrap progress, and the sidebar affordances.
/// `*_fmt` entries are `fill` templates; placeholders are documented inline.
pub struct MachinesTexts {
    pub title: &'static str,
    pub search_hint: &'static str,
    /// local/remote 转发的目标必填项提示（动态转发的这几个字段不参与）。
    pub forward_target_host_required: &'static str,
    pub forward_target_port_required: &'static str,
    pub count_fmt: &'static str, // args: count
    pub empty: &'static str,
    pub empty_hint: &'static str,
    /// 有已保存的机器、但过滤后没有匹配时的空状态标题。
    pub no_matches: &'static str,
    // Footer hint labels (rendered as key caps by `render_key_hints`).
    pub hint_select: &'static str,
    pub hint_scroll: &'static str,
    pub hint_details: &'static str,
    pub hint_add: &'static str,
    pub hint_import: &'static str,
    pub hint_filter: &'static str,
    pub hint_close: &'static str,
    pub hint_back: &'static str,
    pub hint_edit: &'static str,
    pub hint_rename: &'static str,
    pub hint_reconnect: &'static str,
    pub hint_review: &'static str,
    pub hint_toggle_enabled: &'static str,
    pub hint_remove: &'static str,
    pub hint_forwards: &'static str,
    pub hint_copy_fix: &'static str,
    pub hint_fields: &'static str,
    pub hint_change: &'static str,
    pub hint_confirm: &'static str,
    pub hint_toggle: &'static str,
    pub hint_all_none: &'static str,
    pub hint_continue: &'static str,
    // Detail field labels.
    pub detail_id: &'static str,
    pub detail_target: &'static str,
    pub detail_session: &'static str,
    pub detail_status: &'static str,
    pub detail_server_version: &'static str,
    pub detail_enabled: &'static str,
    pub detail_group: &'static str,
    pub detail_tags: &'static str,
    pub detail_color: &'static str,
    pub detail_port: &'static str,
    pub detail_user: &'static str,
    pub detail_identity_files: &'static str,
    pub detail_identities_only: &'static str,
    pub detail_identity_agent: &'static str,
    pub detail_strict_host_key: &'static str,
    pub detail_proxy_jump: &'static str,
    pub detail_forward_agent: &'static str,
    pub detail_server_alive_interval: &'static str,
    pub detail_server_alive_count_max: &'static str,
    pub detail_control_persist: &'static str,
    pub detail_remote_command: &'static str,
    pub detail_last_error: &'static str,
    pub value_not_set: &'static str,
    pub fix_hint: &'static str,
    // Detail action buttons.
    pub add_button: &'static str,
    pub copied_fix_command: &'static str,
    // Remove confirmation.
    pub remove_title_fmt: &'static str, // args: label
    pub remove_detail: &'static str,
    // Add/edit form.
    pub add_title: &'static str,
    pub edit_title: &'static str,
    /// 重命名浮层（机器页 `R`、侧栏右键「重命名」）的标题。
    pub rename_title: &'static str,
    pub field_label: &'static str,
    pub hint_identity_files: &'static str,
    pub hint_proxy_jump: &'static str,
    pub hint_color: &'static str,
    pub choice_default: &'static str,
    pub choice_yes: &'static str,
    pub choice_no: &'static str,
    pub save_button: &'static str,
    pub confirm_install_note: &'static str,
    pub confirm_auth_note: &'static str,
    // Remote bootstrap progress.
    pub progress_detect: &'static str,
    pub progress_install: &'static str,
    pub progress_start: &'static str,
    pub progress_verify: &'static str,
    // Port forwards: detail section and the rules editor page.
    pub detail_port_forwards: &'static str,
    pub forward_status_active: &'static str,
    pub forward_status_failed: &'static str,
    pub forward_waiting: &'static str,
    pub forwards_title_fmt: &'static str, // args: label
    pub forward_none: &'static str,
    pub forward_none_hint: &'static str,
    pub forward_add_title: &'static str,
    pub forward_field_kind: &'static str,
    pub forward_field_listen_port: &'static str,
    pub forward_field_bind_address: &'static str,
    pub forward_field_target_host: &'static str,
    pub forward_field_target_port: &'static str,
    pub forward_saved: &'static str,
    pub forward_removed: &'static str,
    pub forward_remove_confirm_fmt: &'static str, // args: rule
    pub forward_remove_cancelled: &'static str,
    /// 武装与确认之间规则表被外部改写时的取消提示。
    pub forward_remove_stale: &'static str,
    // SSH config import wizard.
    pub import_title: &'static str,
    pub import_step_discover: &'static str,
    pub import_step_select: &'static str,
    pub import_step_done: &'static str,
    pub import_no_config: &'static str,
    pub import_read_failed_fmt: &'static str, // args: error
    pub import_no_hosts_fmt: &'static str,    // args: path
    pub import_warnings_fmt: &'static str,    // args: count
    pub import_skip_header: &'static str,
    pub import_ready_header: &'static str,
    pub import_notes_fmt: &'static str, // args: count
    pub import_include_wildcards: &'static str,
    pub import_group_label: &'static str,
    pub import_selected_fmt: &'static str, // args: selected, total
    pub import_scroll_position_fmt: &'static str, // args: start, total
    pub import_result_imported: &'static str,
    pub import_result_skipped: &'static str,
    /// 结果页里未勾选主机的跳过原因。
    pub import_skip_unselected: &'static str,
    pub import_result_failed: &'static str,
    pub import_failed_hint: &'static str,
    pub import_summary_fmt: &'static str, // args: imported, skipped, failed
    pub import_connect_note: &'static str,
    // Session log: detail-card block and the edit-form fields.
    pub detail_session_log: &'static str,
    pub session_log_dropped_fmt: &'static str, // args: count
    pub field_session_log_enabled: &'static str,
    pub field_session_log_path: &'static str,
    pub field_session_log_max_bytes: &'static str,
    pub field_session_log_interval: &'static str,
    pub hint_session_log_path: &'static str,
    // Detail entries into the broadcast set manager and the file browser.
    pub hint_broadcast: &'static str,
    pub hint_browse_files: &'static str,
}

/// Texts for the broadcast target-set manager: the overlay listing the
/// registered panes, the machine/pane pickers, and the gate toggle. The
/// persisted set and its validation live in `endpoint::broadcast`; the
/// always-on input indicator is `ModeBarTexts::broadcast_fmt`.
/// `*_fmt` entries are `fill` templates; placeholders are documented inline.
pub struct BroadcastTexts {
    pub title: &'static str,
    pub gate_enabled: &'static str,
    pub gate_disabled: &'static str,
    pub count_fmt: &'static str, // args: count
    pub empty: &'static str,
    pub empty_hint: &'static str,
    pub enable_button: &'static str,
    pub disable_button: &'static str,
    pub add_button: &'static str,
    pub remove_button: &'static str,
    pub clear_button: &'static str,
    pub enabled_message: &'static str,
    pub disabled_message: &'static str,
    pub cleared_message: &'static str,
    pub added_fmt: &'static str,   // args: machine, pane
    pub removed_fmt: &'static str, // args: machine, pane
    /// 目标列表被外部改动后按 x 的提示（不暴露 CLI 的编号文案，TOOL-23）。
    pub target_missing: &'static str,
    pub duplicate_fmt: &'static str, // args: machine
    pub pick_machine_title: &'static str,
    pub pick_pane_title_fmt: &'static str, // args: label
    pub picker_offline_fmt: &'static str,  // args: label
    pub picker_no_panes_fmt: &'static str, // args: label
    pub notice_failed_fmt: &'static str,   // args: label, error
    // Footer hint labels (rendered as key caps by `render_key_hints`).
    pub hint_select: &'static str,
    pub hint_gate: &'static str,
    pub hint_add: &'static str,
    pub hint_remove: &'static str,
    pub hint_clear: &'static str,
    pub hint_close: &'static str,
    pub hint_back: &'static str,
}

/// Texts for the remote file browser of one saved machine: directory
/// listing, read-only small-file viewer, and the download/upload/mkdir/
/// rename/delete operations. Operations run on worker threads through
/// `remote::RemoteFs`; results arrive as client loop events.
/// `*_fmt` entries are `fill` templates; placeholders are documented inline.
pub struct MachineFilesTexts {
    pub title_fmt: &'static str, // args: label
    /// 下载目标已存在时的拒绝文案（args: path，HERDR-MACH-020）。
    pub download_exists_fmt: &'static str,
    pub search_hint: &'static str,
    pub count_fmt: &'static str, // args: count
    pub loading: &'static str,
    pub empty: &'static str,
    pub error_fmt: &'static str, // args: error
    pub kind_directory: &'static str,
    pub kind_file: &'static str,
    pub kind_symlink: &'static str,
    pub kind_other: &'static str,
    pub downloaded_fmt: &'static str,     // args: path
    pub uploaded_fmt: &'static str,       // args: path
    pub created_fmt: &'static str,        // args: path
    pub renamed_fmt: &'static str,        // args: path
    pub deleted_fmt: &'static str,        // args: path
    pub confirm_delete_fmt: &'static str, // args: path
    pub confirm_delete_recursive: &'static str,
    pub prompt_download_fmt: &'static str, // args: name
    pub prompt_upload: &'static str,
    pub prompt_mkdir: &'static str,
    pub prompt_rename_fmt: &'static str, // args: name
    pub viewer_title_fmt: &'static str,  // args: path
    // Action buttons.
    pub up_button: &'static str,
    pub refresh_button: &'static str,
    pub download_button: &'static str,
    pub upload_button: &'static str,
    pub mkdir_button: &'static str,
    pub rename_button: &'static str,
    pub delete_button: &'static str,
    // Footer hint labels (rendered as key caps by `render_key_hints`).
    pub hint_select: &'static str,
    pub hint_open: &'static str,
    pub hint_up: &'static str,
    pub hint_filter: &'static str,
    pub hint_confirm: &'static str,
    pub hint_back: &'static str,
    pub hint_scroll: &'static str,
}

/// Texts for the SSH connection recovery dialogs: the host-key trust (TOFU)
/// confirmation, the host-key-changed blocker, the authentication guide, the
/// askpass password prompt, the reconnect banner, and the machine detail
/// card's structured failure classification. `*_fmt` entries are `fill`
/// templates; placeholders are documented inline. Secrets never appear here:
/// prompt text comes from ssh and answers are never templated into strings.
pub struct MachineAuthTexts {
    // Shared dialog lines.
    pub host_fmt: &'static str,        // args: host
    pub key_type_fmt: &'static str,    // args: type
    pub fingerprint_fmt: &'static str, // args: fingerprint
    pub fingerprint_unavailable: &'static str,
    pub working: &'static str,
    pub failed_fmt: &'static str, // args: error
    pub close_button: &'static str,
    // Unknown host key (trust on first use).
    pub tofu_title: &'static str,
    pub tofu_question: &'static str,
    pub tofu_verify_hint: &'static str,
    pub trust_remember_button: &'static str,
    pub trust_once_button: &'static str,
    pub abort_button: &'static str,
    pub trusted_fmt: &'static str, // args: count
    /// 添加表单（测试连接）路径信任后的说明：没有已保存的机器，不会重连。
    pub trusted_retest_fmt: &'static str, // args: count
    // Changed host key (hard blocker).
    pub changed_title: &'static str,
    pub changed_warning: &'static str,
    pub changed_reinstall_hint: &'static str,
    pub changed_mitm_hint: &'static str,
    pub remove_retry_button: &'static str,
    /// 添加表单（测试连接）路径的「移除」按钮：没有已保存的机器，按下只清掉旧
    /// 记录、并不重试（随后提示关闭对话框重新测试），按钮不写「重试」。
    pub remove_button: &'static str,
    pub removed: &'static str,
    /// 添加表单（测试连接）路径移除旧记录后的说明：不会重连，需重新测试。
    pub removed_retest: &'static str,
    // Authentication guide.
    pub auth_title: &'static str,
    pub auth_methods_fmt: &'static str,  // args: methods
    pub auth_identity_fmt: &'static str, // args: path
    pub auth_no_identity: &'static str,
    pub auth_hint: &'static str,
    pub auth_interactive_button: &'static str,
    pub auth_precollect_button: &'static str,
    pub auth_copy_fix_button: &'static str,
    pub auth_copied_fix: &'static str,
    pub precollected_fmt: &'static str, // args: count
    // Askpass password/passphrase prompt.
    pub password_title: &'static str,
    pub password_submit_button: &'static str,
    pub password_cancel_button: &'static str,
    pub password_hidden_note: &'static str,
    pub passphrase_agent_hint: &'static str,
    pub auth_success: &'static str,
    pub auth_success_wizard: &'static str,
    pub auth_failed_fmt: &'static str, // args: error
    pub copy_ssh_add_button: &'static str,
    pub copied_ssh_add: &'static str,
    // Reconnect banner.
    pub banner_reconnecting_fmt: &'static str, // args: label, attempt, seconds
    pub banner_retry_button: &'static str,
    pub banner_give_up_button: &'static str,
    // Detail card failure classification and next-step guidance.
    pub detail_failure: &'static str,
    pub kind_dns: &'static str,
    pub kind_timeout: &'static str,
    pub kind_auth_denied: &'static str,
    pub kind_host_key_unknown: &'static str,
    pub kind_host_key_changed: &'static str,
    pub kind_auth_required: &'static str,
    pub kind_remote_install_required: &'static str,
    pub kind_remote_install_failed: &'static str,
    pub kind_protocol: &'static str,
    pub kind_other: &'static str,
    pub next_host_key_unknown: &'static str,
    pub next_host_key_changed: &'static str,
    pub next_auth_required: &'static str,
    pub next_auth_denied: &'static str,
    pub next_install: &'static str,
    pub next_protocol: &'static str,
    pub next_retry: &'static str,
    // Dialog keyboard footers (rendered as key caps by `render_key_hints`).
    pub hint_trust: &'static str,
    pub hint_trust_once: &'static str,
    pub hint_abort: &'static str,
    pub hint_remove_retry: &'static str,
    /// 添加表单路径的页脚提示，同 `remove_button`。
    pub hint_remove: &'static str,
    pub hint_interactive: &'static str,
    pub hint_precollect: &'static str,
    pub hint_copy_fix: &'static str,
    pub hint_close: &'static str,
}

/// Texts for the snippet overlay: the library list, the edit form, the run
/// flow (target picker, variable form, confirmation), and the history view.
/// `*_fmt` entries are `fill` templates; placeholders are documented inline.
pub struct SnippetsTexts {
    pub title: &'static str,
    pub run_pick_title: &'static str,
    pub search_hint: &'static str,
    pub count_fmt: &'static str, // args: count
    pub empty: &'static str,
    pub empty_hint: &'static str,
    // List action buttons.
    pub new_button: &'static str,
    pub run_button: &'static str,
    pub edit_button: &'static str,
    pub delete_button: &'static str,
    pub history_button: &'static str,
    // New/edit form.
    pub new_title: &'static str,
    pub edit_title: &'static str,
    pub field_label: &'static str,
    pub field_command: &'static str,
    pub field_description: &'static str,
    pub field_variables: &'static str,
    pub field_tags: &'static str,
    pub hint_variables: &'static str,
    pub hint_tags: &'static str,
    pub saved_message: &'static str,
    pub removed_message: &'static str,
    pub delete_title_fmt: &'static str, // args: label
    pub delete_detail: &'static str,
    // Run flow.
    pub run_title_fmt: &'static str, // args: label
    pub target_title: &'static str,
    pub target_current_fmt: &'static str, // args: pane
    pub target_pick_pane: &'static str,
    pub target_machines: &'static str,
    pub pane_picker_title: &'static str,
    pub picker_empty: &'static str,
    pub machines_picker_title: &'static str,
    /// 每台机器一个 pane 的模式下，选中集合为空时按 Enter 的提示。
    pub machines_picker_empty_selection: &'static str,
    /// 端点投影重建时被丢弃的在途运行：按失败收尾的记账文案（独立复审 中-3）。
    pub run_interrupted: &'static str,
    pub variables_title: &'static str,
    pub confirm_title: &'static str,
    pub confirm_command: &'static str,
    pub confirm_targets_fmt: &'static str, // args: count
    pub confirm_press_enter: &'static str,
    pub run_now_button: &'static str,
    /// 确认执行步的提示行：执行键是 y（或 ctrl+↵），普通回车不执行（C-02）。
    pub run_confirm_hint: &'static str,
    // Per-target outcomes and the completion toast.
    pub target_offline_fmt: &'static str,  // args: label
    pub target_no_pane_fmt: &'static str,  // args: label
    pub run_summary_fmt: &'static str,     // args: label, ok, total
    pub run_failed_body_fmt: &'static str, // args: failures
    pub run_ok_body: &'static str,
    // History view.
    pub history_title: &'static str,
    pub history_empty: &'static str,
    pub history_sent: &'static str,
    pub history_failed: &'static str,
    // Footer hint labels (rendered as key caps by `render_key_hints`).
    pub hint_select: &'static str,
    pub hint_run: &'static str,
    pub hint_new: &'static str,
    pub hint_edit: &'static str,
    pub hint_delete: &'static str,
    pub hint_history: &'static str,
    pub hint_close: &'static str,
    pub hint_back: &'static str,
    pub hint_toggle: &'static str,
    pub hint_all_none: &'static str,
    pub hint_continue: &'static str,
    pub hint_fields: &'static str,
    pub hint_confirm: &'static str,
}

/// Scene snapshots overlay: named captures of the current working scene
/// (enabled machine set, active machine/workspace, sidebar chrome), stored
/// in a client-local `scene-snapshots.json` next to the endpoint catalog.
/// `*_fmt` entries are `fill` templates; placeholders are documented inline.
pub struct ScenesTexts {
    pub title: &'static str,
    pub count_fmt: &'static str, // args: count
    pub empty: &'static str,
    pub empty_hint: &'static str,
    pub machines_count_fmt: &'static str, // args: count
    // List action buttons.
    pub save_button: &'static str,
    pub restore_button: &'static str,
    pub rename_button: &'static str,
    pub delete_button: &'static str,
    pub confirm_save_button: &'static str,
    pub confirm_delete_button: &'static str,
    pub cancel_button: &'static str,
    // Restore option toggle (checkbox row in the list view).
    pub toggle_disable_others: &'static str,
    // Save / rename forms.
    pub save_title: &'static str,
    pub rename_title: &'static str,
    pub field_name: &'static str,
    pub field_note: &'static str,
    pub default_name_fmt: &'static str, // args: n
    pub name_required: &'static str,
    pub name_too_long: &'static str,
    /// Rename collision: another scene already carries the new name.
    pub name_duplicate: &'static str,
    // Delete confirmation.
    pub delete_title_fmt: &'static str, // args: name
    pub delete_detail: &'static str,
    // Restore confirmation (only shown when the restore would disable machines).
    pub restore_confirm_title_fmt: &'static str, // args: name
    /// Lead-in above the per-machine list of the restore confirmation.
    pub restore_confirm_detail: &'static str,
    /// Overflow line when the list does not fit the modal.
    pub restore_confirm_more_fmt: &'static str, // args: count
    /// The confirmation key differs from the list's restore key on purpose.
    pub restore_confirm_hint: &'static str,
    pub confirm_restore_button: &'static str,
    // Footer hint labels (rendered as key caps by `render_key_hints`).
    pub hint_select: &'static str,
    pub hint_restore: &'static str,
    pub hint_save: &'static str,
    pub hint_rename: &'static str,
    pub hint_delete: &'static str,
    pub hint_toggle: &'static str,
    pub hint_close: &'static str,
    pub hint_back: &'static str,
    pub hint_confirm: &'static str,
    pub hint_fields: &'static str,
    // Completion toasts and degraded-restore details.
    pub notice_saved_fmt: &'static str,    // args: name
    pub notice_restored_fmt: &'static str, // args: name
    pub notice_renamed_fmt: &'static str,  // args: name
    pub notice_deleted_fmt: &'static str,  // args: name
    pub restore_ok_body: &'static str,
    pub restore_missing_fmt: &'static str, // args: names
    pub restore_offline_fmt: &'static str, // args: label
    pub load_failed_fmt: &'static str,     // args: error
}

/// Agents 面板（统一树）与 agent 行右键菜单的文案。
/// `*_fmt` entries are `fill` templates; placeholders are documented inline.
pub struct AgentPanelTexts {
    pub sort_launch: &'static str,
    pub external_group: &'static str,
    pub external_unreadable: &'static str,
    /// 属主在运行、有运行中的节点时的徽标。
    pub activity_badge_running_fmt: &'static str, // args: running
    /// 属主在运行、没有运行中的节点但有已结束（完成 + 失败）的节点时的徽标。
    pub activity_badge_finished_fmt: &'static str, // args: n
    /// 活动摘要里还有没下发的活跃节点（点击打开活动窗口）。
    pub activity_more_fmt: &'static str, // args: n
    /// 活动摘要末尾「已完成 · 失败」行的两段，为 0 的段不画，两段之间用 ` · `。
    pub activity_done_fmt: &'static str, // args: n
    pub activity_failed_fmt: &'static str, // args: n
    pub menu_focus: &'static str,
    pub menu_view_activity: &'static str,
    /// 重命名的是 agent 所在的 pane（`pane.rename`，改 pane 标签），文案照实写
    /// 「重命名窗格」，与 pane 右键菜单同名。
    pub menu_rename: &'static str,
    pub menu_usage: &'static str,
    pub menu_bind_account: &'static str,
    pub menu_close: &'static str,
    /// agent 用量悬停卡的标题（指针悬浮与右键「用量」钉住的是同一张卡）。
    pub usage_card_title_fmt: &'static str, // args: agent
    /// 钉住的用量卡标题栏右侧的标记。
    pub usage_pinned: &'static str,
}

/// 「Agent 活动」二级窗口的文案；节点种类 / 状态的标签 Agents 面板也读。
/// `*_fmt` entries are `fill` templates; placeholders are documented inline.
pub struct AgentActivityTexts {
    pub title_fmt: &'static str, // args: name
    pub tree_title: &'static str,
    pub content_title: &'static str,
    pub empty_tree: &'static str,
    pub empty_content: &'static str,
    pub loading: &'static str,
    pub read_failed_fmt: &'static str, // args: error
    pub unsupported: &'static str,
    pub external_read_only: &'static str,
    pub truncated: &'static str,
    pub follow_on: &'static str,
    pub follow_off: &'static str,
    pub hint_select: &'static str,
    pub hint_follow: &'static str,
    pub hint_close: &'static str,
    /// 页脚：`r` 重新读取树与内容。
    pub hint_refresh: &'static str,
    /// 页脚：`←→` 折叠 / 展开树节点。
    pub hint_collapse: &'static str,
    /// 页脚：`Tab` 在活动树与内容之间切换（窄窗口只显示其中一列）。
    pub hint_switch_column: &'static str,
    /// 标题栏按钮（后接跟随状态的 `●` / `○`）。
    pub follow_button: &'static str,
    pub refresh_button: &'static str,
    /// 未跟随时属主的活动摘要变了：标题旁的提示。
    pub updates_badge: &'static str,
    /// 已选节点读完但没有任何输出。
    pub no_output: &'static str,
    pub kind_subagent: &'static str,
    pub kind_task: &'static str,
    pub kind_todo: &'static str,
    pub kind_background: &'static str,
    pub kind_unknown: &'static str,
    pub status_pending: &'static str,
    pub status_running: &'static str,
    pub status_blocked: &'static str,
    pub status_done: &'static str,
    pub status_failed: &'static str,
    pub status_unknown: &'static str,
}

/// 统一菜单组件（全局菜单 / 右键菜单 / 子菜单）与工作台顶栏、「调整布局」
/// 页脚提示的文案。
pub struct MenuTexts {
    pub arrange_layout: &'static str,
    /// 「调整布局」模式下页脚最左侧的状态标签。
    pub arrange_hint: &'static str,
    /// 「调整布局」模式页脚键位提示的动作名（键帽不翻译）。
    pub arrange_focus: &'static str,
    pub arrange_resize: &'static str,
    pub arrange_move: &'static str,
    pub arrange_maximize: &'static str,
    pub arrange_done: &'static str,
    pub unavailable_suffix: &'static str,
    pub submenu_view: &'static str,
    pub monitor: &'static str,
    pub monitor_short: &'static str,
    pub close_monitor: &'static str,
    pub lock_layout: &'static str,
}

/// 机器表单（快速添加、分组、预览、连接测试与校验）的文案。
/// `*_fmt` entries are `fill` templates; placeholders are documented inline.
pub struct MachineFormTexts {
    pub quick_label: &'static str,
    pub quick_placeholder: &'static str,
    pub quick_parsed_fmt: &'static str, // args: n
    pub quick_parse_failed: &'static str,
    pub group_connection: &'static str,
    pub group_auth: &'static str,
    pub group_session: &'static str,
    pub group_advanced: &'static str,
    pub preview_title: &'static str,
    pub test_connection: &'static str,
    pub test_running: &'static str,
    pub test_passed: &'static str,
    pub test_failed_fmt: &'static str, // args: step
    pub err_required: &'static str,
    pub err_port: &'static str,
    pub err_host: &'static str,
    pub err_duplicate_name: &'static str,
    /// 导入 Select 步骤的清单表头提示。
    pub import_select_hint: &'static str,
    /// 预览里字段校验失败时追加在值后面的标记（配合标红，不只靠颜色区分，L15）。
    pub preview_invalid_suffix: &'static str,
    /// 目标字段的占位文字。
    pub target_placeholder: &'static str,
    /// 页脚：快速输入聚焦时 Enter 的含义。
    pub hint_fill: &'static str,
    /// 页脚：测试运行中 Esc 的含义。
    pub cancel_test: &'static str,
    /// 测试失败为主机密钥问题时的恢复入口。
    pub review_host_key: &'static str,
    /// 测试通过后的提醒：测试不保存。
    pub test_not_saved: &'static str,
    /// 保存 / 测试被字段校验拦下时的提示。
    pub fix_fields: &'static str,
    /// 测试连接的确认条：预先授权的后果（安装 / 更新、停止远端 server）。
    pub test_confirm_note: &'static str,
    /// 确认条上开始测试的页脚项。
    pub test_confirm_start: &'static str,
    /// 确认条上的取消。
    pub prompt_cancel: &'static str,
    /// 有改动时离开表单的确认条。
    pub discard_prompt: &'static str,
    /// 放弃确认条上的「放弃更改」。
    pub discard_confirm: &'static str,
    /// 放弃确认条上的「继续编辑」。
    pub keep_editing: &'static str,
    pub saved_fmt: &'static str, // args: label
}

/// 运行期提示与错误里原来只有中文的文案（文档终审 D7）：选区与阅读快照、连接、进程、
/// 显卡与监控服务等。服务端产出的条目按 server 进程的界面语言给出（与 toast、通知同一
/// 口径，随 config.toml 的 `language` 同步）。
pub struct RuntimeMessageTexts {
    // 选区与阅读快照（客户端）。
    pub selection_history_unreadable: &'static str,
    pub selection_changed_before_copy: &'static str,
    pub selection_resized: &'static str,
    pub selection_capture_failed: &'static str,
    pub selection_copy_mismatch: &'static str,
    pub selection_timed_out: &'static str,
    // 连接（客户端）。
    pub views_update_failed: &'static str,
    pub connection_settings_changed: &'static str,
    pub host_key_review_required: &'static str,
    pub terminal_size_not_ready: &'static str,
    pub connection_cancelled: &'static str,
    pub handshake_timed_out: &'static str,
    // 阅读快照（服务端 `pane.text_snapshot.*` 的错误说明）。
    pub snapshot_expired: &'static str,
    pub snapshot_capacity: &'static str,
    pub snapshot_ids_exhausted: &'static str,
    pub snapshot_viewport_unreadable: &'static str,
    pub snapshot_rows_out_of_range: &'static str,
    pub snapshot_selection_out_of_range: &'static str,
    pub snapshot_invalid_request: &'static str,
    pub snapshot_pane_gone: &'static str,
    pub snapshot_capture_failed: &'static str,
    pub snapshot_needs_server: &'static str,
    /// 只有 server 或客户端连接上下文才能处理的 API 方法落到了 App 自身。
    pub server_context_required: &'static str,
    // 进程详情与结束进程（服务端 `system.process.*` 的错误说明）。
    pub process_request_unsupported: &'static str,
    pub process_busy: &'static str,
    pub process_host_changed: &'static str,
    pub process_changed_refresh: &'static str,
    pub process_changed: &'static str,
    pub process_gone: &'static str,
    pub process_pid_reused: &'static str,
    pub process_identity_changed: &'static str,
    pub process_confirm_first: &'static str,
    pub process_confirm_expired: &'static str,
    pub process_confirm_mismatch: &'static str,
    pub process_protected: &'static str,
    // 显卡、主机与监控服务（服务端）。
    pub gpu_driver_timeout: &'static str,
    pub gpu_utilization_unavailable: &'static str,
    /// 系统取不到主机名时概要行的占位。
    pub hostname_unknown: &'static str,
    pub monitor_subscription_limit: &'static str,
    pub monitor_request_unsupported: &'static str,
    pub monitor_busy: &'static str,
    pub monitor_unavailable: &'static str,
    /// 账号用量请求指向的窗格已不存在。
    pub observation_pane_gone: &'static str,
    pub observation_start_failed_fmt: &'static str, // args: error
}

/// 平台层（`src/platform/<os>`）产出、会进入 API 应答或监控快照的文案（文档终审 D7），
/// 按调用进程（server）的界面语言给出。每条只由对应平台的实现读取。
// 任一目标编译时，别的平台的条目都没有读者；测试构建里 `tests::platform_messages` 穷尽
// 读取全部条目。
#[cfg_attr(not(test), allow(dead_code))]
pub struct PlatformMessageTexts {
    // Linux（`/proc` 与 sysfs）。
    pub process_status_invalid: &'static str,
    pub process_start_missing: &'static str,
    pub process_start_invalid: &'static str,
    pub process_name_missing: &'static str,
    pub gpu_utilization_unexposed: &'static str,
    pub environment_wsl: &'static str,
    pub environment_container: &'static str,
    pub environment_linux: &'static str,
    // Windows。
    pub process_force_required: &'static str,
    pub gpu_counters_pending: &'static str,
    pub environment_windows: &'static str,
    // 其余平台（回退实现）。
    pub process_handle_unsupported: &'static str,
}

/// 账号用量里 herdr 自己写的说明（文档终审 D7）：探测失败写进快照 `message` 的文案、详情
/// 面板的来源（`source`），以及账号请求与官方回调接入被拒时的错误说明。按 server 的界面语言
/// 生成（与 toast、通知同一口径）；厂商原文（CLI 的 stderr 摘要、接口返回的内容）经
/// `detail_fmt` 等原样拼在后面，不翻译。`*_fmt` 是 `fill` 模板，占位写在行尾。
pub struct UsageProbeTexts {
    // ---- 官方 CLI 进程与输出 ----
    pub probe_isolation_failed: &'static str,
    pub probe_dir_failed: &'static str,
    pub stable_dir_failed: &'static str,
    pub profile_unsupported: &'static str,
    pub cli_missing: &'static str,
    pub cli_start_failed: &'static str,
    pub output_unavailable: &'static str,
    pub cli_timed_out: &'static str,
    pub cli_output_unreadable: &'static str,
    pub cli_output_too_large: &'static str,
    pub result_unavailable: &'static str,
    pub cli_exit_timed_out: &'static str,
    // ---- 失败分类（厂商原文摘要经 `detail_fmt` 拼在后面） ----
    pub cli_help_empty: &'static str,
    pub cli_signed_out: &'static str,
    pub cli_signaled_fmt: &'static str,        // args: signal
    pub cli_no_usage_output_fmt: &'static str, // args: code
    pub cli_usage_error_fmt: &'static str,     // args: detail
    /// 接在说明后的厂商原文摘要。
    pub detail_fmt: &'static str, // args: summary
    pub cli_failed_fmt: &'static str,          // args: code
    pub cli_failed_summary_fmt: &'static str,  // args: code, summary
    pub no_subcommand: &'static str,
    pub subcommand_missing: &'static str,
    pub claude_auth_status_unsupported: &'static str,
    // ---- Codex app-server ----
    pub codex_unsupported_fmt: &'static str, // args: detail
    pub codex_signed_out_fmt: &'static str,  // args: detail
    pub codex_failed_fmt: &'static str,      // args: detail
    pub rpc_input_closed: &'static str,
    pub rpc_input_failed: &'static str,
    pub rpc_output_unavailable: &'static str,
    pub codex_timed_out: &'static str,
    pub codex_exited: &'static str,
    pub codex_exited_fmt: &'static str,         // args: summary
    pub token_refresh_failed_fmt: &'static str, // args: message, retry
    pub codex_sign_in_first: &'static str,
    // ---- 交互探测（隔离终端与 Windows 辅助进程） ----
    pub helper_missing: &'static str,
    pub helper_config_failed: &'static str,
    pub helper_send_failed: &'static str,
    pub helper_output_unavailable: &'static str,
    pub isolated_timed_out: &'static str,
    pub isolated_unreadable: &'static str,
    pub isolated_too_large: &'static str,
    pub isolated_invalid: &'static str,
    pub interactive_profile_unsupported: &'static str,
    pub pty_create_failed: &'static str,
    pub pty_spawn_failed: &'static str,
    pub pty_input_unavailable: &'static str,
    pub pty_output_unavailable: &'static str,
    pub pty_screen_unreadable: &'static str,
    pub pty_protocol_failed: &'static str,
    pub pty_cursor_unavailable: &'static str,
    pub pty_cli_exited: &'static str,
    // ---- Kimi 本地服务（kimi web） ----
    pub kimi_port_failed: &'static str,
    pub kimi_port_unreadable: &'static str,
    pub kimi_address_invalid: &'static str,
    pub kimi_banner_timeout: &'static str,
    pub kimi_banner_too_large: &'static str,
    pub kimi_web_unsupported: &'static str,
    pub kimi_signed_out: &'static str,
    pub kimi_exited: &'static str,
    pub kimi_exited_fmt: &'static str, // args: summary
    pub kimi_not_ready: &'static str,
    pub kimi_http_signed_out_fmt: &'static str, // args: status
    pub kimi_http_forbidden_fmt: &'static str,  // args: status
    pub kimi_http_missing_fmt: &'static str,    // args: status
    pub kimi_http_error_fmt: &'static str,      // args: status
    pub kimi_connect_failed: &'static str,
    pub kimi_request_failed: &'static str,
    pub kimi_response_unreadable: &'static str,
    pub kimi_response_too_large: &'static str,
    pub kimi_format_unsupported: &'static str,
    pub kimi_query_failed: &'static str,
    // ---- 官方 HTTP 接口（`auth_mode = "api"`） ----
    pub api_credential_env_required: &'static str,
    pub api_credential_missing: &'static str,
    pub api_client_failed: &'static str,
    pub api_kimi_base_required: &'static str,
    pub api_provider_unsupported: &'static str,
    pub api_query_unsupported: &'static str,
    pub api_no_verified_fields: &'static str,
    pub api_deadline: &'static str,
    pub api_connect_failed: &'static str,
    pub api_http_status_fmt: &'static str, // args: status, retry
    pub api_retry_after_fmt: &'static str, // args: seconds
    pub api_too_large: &'static str,
    pub api_unreadable: &'static str,
    pub api_unrecognized: &'static str,
    pub api_base_invalid: &'static str,
    pub api_base_has_credentials: &'static str,
    pub api_base_unregistered: &'static str,
    // ---- ZCode 本地数据库（只读外部来源） ----
    pub zcode_no_home: &'static str,
    pub zcode_no_database: &'static str,
    pub zcode_non_utf8_path: &'static str,
    pub zcode_profile_unsupported: &'static str,
    pub zcode_sqlite_missing: &'static str,
    pub zcode_query_failed: &'static str,
    pub zcode_schema_mismatch: &'static str,
    pub zcode_database_busy: &'static str,
    pub zcode_database_unreadable: &'static str,
    pub zcode_no_result: &'static str,
    pub zcode_unparsable: &'static str,
    // ---- 快照来源（详情面板的「来源」） ----
    pub source_official_api: &'static str,
    pub source_kimi_local_api: &'static str,
    pub source_cli_fmt: &'static str, // args: command
    pub source_claude_callback: &'static str,
    pub source_local_stats_fmt: &'static str, // args: command
    pub source_extension_push: &'static str,
    pub source_cli_callback: &'static str,
    pub source_extension_push_stats: &'static str,
    pub source_zcode_local: &'static str,
    // ---- 账号请求与探测方案 ----
    pub callback_session_stats: &'static str,
    pub agent_unsupported: &'static str,
    pub subscription_limit: &'static str,
    pub unknown_account: &'static str,
    pub binding_limit: &'static str,
    pub request_unsupported: &'static str,
    pub busy: &'static str,
    // ---- 官方回调（statusline）接入：`account.usage.integration` 的错误说明 ----
    pub statusline_unsupported_provider: &'static str,
    pub statusline_unknown_provider: &'static str,
    pub statusline_retired: &'static str,
    pub settings_invalid_json: &'static str,
    pub settings_empty: &'static str,
    pub settings_not_object: &'static str,
    pub statusline_not_object: &'static str,
    pub statusline_create_failed: &'static str,
    pub statusline_invalid: &'static str,
    pub statusline_not_command: &'static str,
    pub statusline_unrecognized: &'static str,
}

/// 账号用量指标的名称、单位与文字值（快照里的 `label` / `unit` / `text_value`），按 server 的
/// 界面语言生成（文档终审 D7）。客户端的厂商卡片与表格对认得的指标另用界面语言的槽位名
/// （`MonitorTexts`），认不出的指标原样显示这里的名称。
pub struct UsageMetricTexts {
    // ---- 额度窗口 ----
    pub quota_primary: &'static str,
    pub quota_secondary: &'static str,
    pub credits: &'static str,
    pub quota_5h: &'static str,
    pub quota_weekly: &'static str,
    pub quota_spend: &'static str,
    pub quota_plan: &'static str,
    pub quota_7d: &'static str,
    pub quota_monthly: &'static str,
    pub quota_monthly_code: &'static str,
    pub quota_window_fmt: &'static str, // args: n
    /// 厂商没给单位时的计量单位。
    pub quota_unit: &'static str,
    // ---- 余额与费用 ----
    pub balance_available: &'static str,
    pub balance_voucher: &'static str,
    pub balance_cash: &'static str,
    /// 厂商没给币种的余额单位。
    pub balance_unit: &'static str,
    pub extra_balance: &'static str,
    pub extra_total: &'static str,
    pub extra_month_used: &'static str,
    pub extra_month_cap: &'static str,
    pub key_usage: &'static str,
    pub key_usage_daily: &'static str,
    pub key_usage_weekly: &'static str,
    pub key_usage_monthly: &'static str,
    pub key_limit_remaining: &'static str,
    pub account_total_credits: &'static str,
    pub account_total_usage: &'static str,
    pub cost_report: &'static str,
    // ---- Codex 官方账号使用统计（非额度） ----
    pub codex_lifetime_tokens: &'static str,
    pub codex_peak_daily_tokens: &'static str,
    pub codex_longest_turn: &'static str,
    pub codex_current_streak: &'static str,
    pub codex_longest_streak: &'static str,
    pub codex_daily_tokens: &'static str,
    // ---- 会话统计 ----
    pub session_cost_estimate: &'static str,
    pub session_duration: &'static str,
    pub session_api_duration: &'static str,
    pub context_used: &'static str,
    pub context_tokens_input: &'static str,
    pub context_tokens: &'static str,
    pub context_window: &'static str,
    pub session_cost: &'static str,
    pub current_model: &'static str,
    // ---- 本地统计（opencode / zcode） ----
    pub sessions: &'static str,
    pub subagent_sessions: &'static str,
    pub messages: &'static str,
    pub stats_days: &'static str,
    pub total_cost: &'static str,
    pub avg_cost_per_day: &'static str,
    pub avg_tokens_per_session: &'static str,
    pub median_tokens_per_session: &'static str,
    pub tokens_input: &'static str,
    pub tokens_output: &'static str,
    pub tokens_reasoning: &'static str,
    pub tokens_cache_read: &'static str,
    pub tokens_cache_write: &'static str,
    pub tokens_total: &'static str,
    pub tokens_main: &'static str,
    pub tokens_subagents: &'static str,
    pub tool_uses: &'static str,
    pub subagents: &'static str,
    pub stats_window: &'static str,
    // ---- 文字值（`text_value`） ----
    /// claude 沿用的过期额度窗口。
    pub stale_window: &'static str,
    /// claude 上下文用量未知。
    pub claude_context_pending: &'static str,
    /// pi 上下文用量未知。
    pub pi_context_pending: &'static str,
}

/// `[monitor]` 与 `[account_usage]` 配置的诊断（`herdr config check` 与界面的配置诊断共用）：
/// 按调用进程的界面语言给出，同一份诊断里不再中英混杂（T1 服务端审查轻 3）。
pub struct MonitorConfigTexts {
    pub interval_invalid: &'static str,
    pub history_invalid: &'static str,
    pub account_id_invalid: &'static str,
    pub credential_env_invalid: &'static str,
    pub account_user_deprecated_fmt: &'static str, // args: account
}

/// 远端连接层（`src/remote`）写给用户看的错误说明（文档终审 D7）：主机密钥核对、远端安装
/// 上传与远端任务。随调用进程（客户端）的界面语言；ssh / scp 的原始输出原样接在后面。
pub struct RemoteTexts {
    // ---- 主机密钥核对（known_hosts） ----
    pub ssh_config_unreadable: &'static str,
    pub ssh_config_no_hostname: &'static str,
    pub ssh_config_bad_port: &'static str,
    pub ssh_host_unparsable: &'static str,
    pub host_alias_not_single: &'static str,
    pub ssh_home_missing: &'static str,
    pub known_hosts_ambiguous: &'static str,
    pub known_hosts_token_unsupported: &'static str,
    pub known_hosts_disabled: &'static str,
    pub host_key_proxied: &'static str,
    pub ssh_config_changed_review: &'static str,
    pub host_key_mismatch: &'static str,
    pub known_hosts_unwritable: &'static str,
    pub host_key_remove_failed: &'static str,
    pub ssh_config_changed_confirm: &'static str,
    // ---- 临时信任配置与远端安装上传 ----
    pub trust_config_failed: &'static str,
    pub trust_config_no_dir: &'static str,
    pub install_upload_failed: &'static str,
    pub scp_upload_failed: &'static str,
    // ---- 远端任务 ----
    pub task_cancelled: &'static str,
    pub task_input_closed: &'static str,
    pub upload_timed_out: &'static str,
    pub task_output_timed_out: &'static str,
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
    pub api_usage_report_about: &'static str,
    pub api_usage_report_agent_help: &'static str,
    pub api_usage_report_account_help: &'static str,
    pub api_usage_report_passthrough_help: &'static str,
    pub api_schema_about: &'static str,
    pub api_activity_read_about: &'static str,
    pub api_activity_read_agent_help: &'static str,
    pub api_activity_read_external_help: &'static str,
    pub api_activity_read_node_help: &'static str,
    pub api_activity_read_cursor_help: &'static str,
    pub api_activity_read_max_bytes_help: &'static str,
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
    /// `machine add --label`：给了目标时必填，`--from-config` 时缺省取 HOST。
    pub machine_add_label_help: &'static str,
    pub machine_remote_session_help: &'static str,
    pub machine_group_help: &'static str,
    pub machine_tag_help: &'static str,
    pub machine_color_help: &'static str,
    pub machine_port_help: &'static str,
    pub machine_user_help: &'static str,
    pub machine_identity_file_help: &'static str,
    pub machine_identities_only_help: &'static str,
    pub machine_identity_agent_help: &'static str,
    pub machine_strict_host_key_checking_help: &'static str,
    pub machine_proxy_jump_help: &'static str,
    pub machine_forward_agent_help: &'static str,
    pub machine_server_alive_interval_help: &'static str,
    pub machine_server_alive_count_max_help: &'static str,
    pub machine_control_persist_help: &'static str,
    pub machine_remote_command_help: &'static str,
    pub machine_rename_about: &'static str,
    pub machine_remove_about: &'static str,
    pub machine_enable_about: &'static str,
    pub machine_disable_about: &'static str,
    pub machine_from_config_help: &'static str,
    pub machine_import_about: &'static str,
    pub machine_import_file_help: &'static str,
    pub machine_import_host_help: &'static str,
    pub machine_import_yes_help: &'static str,
    pub machine_import_group_help: &'static str,
    pub machine_import_include_wildcards_help: &'static str,
    pub machine_forward_about: &'static str,
    pub machine_forward_list_about: &'static str,
    pub machine_forward_add_about: &'static str,
    pub machine_forward_kind_help: &'static str,
    pub machine_forward_listen_port_help: &'static str,
    pub machine_forward_bind_address_help: &'static str,
    pub machine_forward_target_host_help: &'static str,
    pub machine_forward_target_port_help: &'static str,
    pub machine_forward_remove_about: &'static str,
    pub machine_fs_about: &'static str,
    pub machine_fs_profile_help: &'static str,
    pub machine_fs_operation_help: &'static str,
    pub machine_fs_args_help: &'static str,
    pub machine_log_about: &'static str,
    pub machine_log_action_help: &'static str,
    pub machine_log_args_help: &'static str,
    pub machine_status_about: &'static str,
    pub machine_exec_about: &'static str,
    pub machine_exec_command_help: &'static str,
    pub broadcast_about: &'static str,
    pub broadcast_status_about: &'static str,
    pub broadcast_enable_about: &'static str,
    pub broadcast_disable_about: &'static str,
    pub broadcast_add_about: &'static str,
    pub broadcast_remove_about: &'static str,
    pub broadcast_clear_about: &'static str,
    pub broadcast_send_about: &'static str,
    pub snippet_about: &'static str,
    pub snippet_list_about: &'static str,
    pub snippet_add_about: &'static str,
    pub snippet_remove_about: &'static str,
    pub snippet_run_about: &'static str,
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
    pub machine_forward_none: &'static str,
    pub machine_forward_added_fmt: &'static str, // args: kind, rule, id
    pub machine_forward_updated_fmt: &'static str, // args: id
    pub machine_forward_removed_fmt: &'static str, // args: kind, rule, id
    pub machine_import_found_fmt: &'static str,  // args: count, path
    pub machine_import_prompt: &'static str,
    pub machine_import_skipped_fmt: &'static str, // args: label, reason
    pub machine_imported_fmt: &'static str,       // args: label, id
    pub machine_import_note_fmt: &'static str,    // args: note
    pub machine_import_summary_fmt: &'static str, // args: imported, skipped, failed
    pub machine_import_connect_note: &'static str,
    pub machine_log_enabled_fmt: &'static str,  // args: id
    pub machine_log_disabled_fmt: &'static str, // args: id
    pub machine_log_dumped_fmt: &'static str,   // args: count
    pub broadcast_enabled: &'static str,
    pub broadcast_disabled: &'static str,
    pub broadcast_registered_fmt: &'static str, // args: machine, pane
    pub broadcast_removed_fmt: &'static str,    // args: number, pane
    pub broadcast_cleared: &'static str,
    pub broadcast_no_targets: &'static str,
    pub broadcast_sent_fmt: &'static str, // args: machine, pane
    pub broadcast_failed_fmt: &'static str, // args: machine, pane, error
    pub snippet_none_saved: &'static str,
    pub snippet_saved_fmt: &'static str,    // args: id
    pub snippet_removed_fmt: &'static str,  // args: id
    pub snippet_run_sent_fmt: &'static str, // args: machine, pane
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
    pub integration_codex_hooks_need_review_note: &'static str,
    pub integration_codex_hooks_disabled_note: &'static str,
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
    pub machine_option_specified_twice_fmt: &'static str, // args: option
    pub unknown_option_or_argument_fmt: &'static str, // args: option
    pub remote_session_specified_twice: &'static str,
    pub label_specified_twice: &'static str,
    pub label_required: &'static str,
    pub machine_not_saved_fmt: &'static str, // args: error
    pub machine_prepared_not_saved_fmt: &'static str, // args: error
    pub machine_profile_not_found_fmt: &'static str, // args: id
    // src/cli/machine.rs — `machine add --from-config`
    pub machine_add_from_config_no_config: &'static str,
    pub machine_add_from_config_not_found_fmt: &'static str, // args: alias, path
    pub machine_add_from_config_wildcard_fmt: &'static str,  // args: label
    pub machine_add_from_config_with_target: &'static str,
    pub machine_add_from_config_with_options: &'static str,
    pub machine_config_note_fmt: &'static str, // args: note
    // src/cli/machine.rs — `machine import`
    pub machine_import_usage: &'static str,
    pub machine_import_no_config_path: &'static str,
    pub machine_import_config_not_found_fmt: &'static str, // args: path
    pub machine_import_config_read_failed_fmt: &'static str, // args: path, error
    pub machine_import_config_warning_fmt: &'static str,   // args: origin, line, message
    pub machine_import_no_matching_hosts_fmt: &'static str, // args: path
    pub machine_import_no_hosts_fmt: &'static str,         // args: path
    pub machine_import_cancelled: &'static str,
    pub machine_import_not_a_terminal: &'static str,
    pub machine_import_failed_fmt: &'static str, // args: label, error
    pub machine_import_selection_empty: &'static str,
    pub machine_import_selection_reversed_fmt: &'static str, // args: part
    pub machine_import_selection_range_fmt: &'static str,    // args: part, count
    // src/remote/ssh_config.rs — batch import skip reasons
    pub import_skip_wildcard: &'static str,
    pub import_skip_label_exists: &'static str,
    pub import_skip_target_exists_fmt: &'static str, // args: target
    pub import_skip_batch_label: &'static str,
    pub import_skip_batch_target_fmt: &'static str, // args: target
    // src/cli/machine.rs — `machine forward`
    pub machine_forward_usage: &'static str,
    pub machine_forward_kind_invalid_fmt: &'static str, // args: value
    pub machine_forward_kind_required: &'static str,
    pub machine_forward_listen_port_required: &'static str,
    pub machine_forward_invalid_profile_fmt: &'static str, // args: error
    pub machine_forward_rule_number_invalid_fmt: &'static str, // args: number
    pub machine_forward_rule_number_unknown_fmt: &'static str, // args: id, count, number
    // src/cli/machine_fs.rs / machine_log.rs / machine_status.rs / machine_exec.rs
    pub machine_fs_usage: &'static str,
    pub machine_fs_local_not_file_fmt: &'static str, // args: path
    pub machine_fs_too_large_fmt: &'static str,      // args: size, limit
    pub machine_log_usage: &'static str,
    pub machine_status_usage: &'static str,
    pub machine_exec_usage: &'static str,
    pub machine_exec_command_required: &'static str,
    // src/cli/broadcast.rs
    pub broadcast_usage: &'static str,
    pub broadcast_gate_disabled: &'static str,
    pub broadcast_gate_empty: &'static str,
    pub broadcast_invalid_number_fmt: &'static str, // args: value
    // src/cli/snippet.rs
    pub snippet_usage: &'static str,
    pub snippet_label_required: &'static str,
    pub snippet_command_required: &'static str,
    pub snippet_not_found_fmt: &'static str, // args: selector
    pub snippet_run_pane_twice: &'static str,
    pub snippet_run_target_required: &'static str,
    pub snippet_run_pane_required_fmt: &'static str, // args: machine
    pub snippet_run_protocol_mismatch: &'static str,
    pub snippet_run_server_not_running: &'static str,
    pub snippet_run_request_failed: &'static str,
    pub snippet_run_failed_fmt: &'static str, // args: machine, pane, error
    pub snippet_history_store_failed_fmt: &'static str, // args: error

    // src/cli/api.rs
    pub api_schema_usage: &'static str,
    pub api_snapshot_usage: &'static str,
    pub api_usage_report_usage: &'static str,
    pub api_activity_read_usage: &'static str,
    pub api_activity_read_no_pane_id: &'static str,
    pub usage_report_too_large: &'static str,
    pub usage_report_needs_json: &'static str,

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
    pub integration_target_retired_fmt: &'static str, // args: target
    pub integration_targets_supported: &'static str,

    // src/integration (registry notice + install support check)
    pub integrations_need_updating_fmt: &'static str, // args: instructions
    pub integration_instructions_run_fmt: &'static str, // args: command
    pub integration_instructions_run_list_fmt: &'static str, // args: commands, last

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
    pub monitor: MonitorTexts,
    pub usage_notice: UsageNoticeTexts,
    pub usage_probe: UsageProbeTexts,
    pub usage_metric: UsageMetricTexts,
    pub monitor_config: MonitorConfigTexts,
    pub remote: RemoteTexts,
    pub sidebar: SidebarTexts,
    pub status: StatusTexts,
    pub mode_bar: ModeBarTexts,
    pub notify: NotifyTexts,
    pub history: HistoryTexts,
    pub mobile: MobileTexts,
    pub update: UpdateTexts,
    pub endpoint: EndpointTexts,
    pub machines: MachinesTexts,
    pub machine_auth: MachineAuthTexts,
    pub snippets: SnippetsTexts,
    pub scenes: ScenesTexts,
    pub broadcast: BroadcastTexts,
    pub machine_files: MachineFilesTexts,
    pub agent_panel: AgentPanelTexts,
    pub agent_activity: AgentActivityTexts,
    pub menu: MenuTexts,
    pub machine_form: MachineFormTexts,
    pub runtime: RuntimeMessageTexts,
    pub platform: PlatformMessageTexts,
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

    /// 文档终审 D2：claude 等待说明与交互探测提示的指引，写账号页上「官方回调」开关在
    /// 该语言下的真实名字，不再指向「监控 → 设置」；英文表里不留中文。
    #[test]
    fn usage_notices_point_at_the_accounts_page_callback_toggle() {
        for lang in [Lang::En, Lang::ZhCn] {
            let texts = texts_for(lang);
            let notices = &texts.usage_notice;
            let mut guided: Vec<String> = [(true, Some(false)), (false, Some(false)), (true, None)]
                .into_iter()
                .map(|(login_known, callback)| notices.claude_waiting(login_known, callback))
                .collect();
            guided.push(notices.trust_callback_hint.to_owned());
            guided.push(notices.sign_in_callback_hint.to_owned());
            for notice in &guided {
                assert!(
                    notice.contains(texts.monitor.callback_toggle),
                    "{lang:?}: {notice}"
                );
                assert!(
                    notice.contains(texts.monitor.tab_accounts),
                    "{lang:?}: {notice}"
                );
                assert!(!notice.contains("监控 → 设置"), "{lang:?}: {notice}");
            }
        }
        let en = &texts_for(Lang::En).usage_notice;
        for (login_known, callback) in CLAUDE_WAITING_VARIANTS {
            let notice = en.claude_waiting(login_known, callback);
            assert!(notice.is_ascii(), "{notice}");
        }
        assert!(en.trust_callback_hint.is_ascii() && en.sign_in_callback_hint.is_ascii());
        assert!(en.interactive_failure_sep.is_ascii());
    }

    /// 同一语言里每条说明的原文互不相同，否则按原文反查会认错条目。
    #[test]
    fn usage_notice_catalog_is_unambiguous_per_language() {
        for lang in [Lang::En, Lang::ZhCn] {
            let texts: Vec<&str> = usage_notice_catalog()
                .iter()
                .filter(|(entry_lang, _, _)| *entry_lang == lang)
                .map(|(_, _, text)| text.as_str())
                .collect();
            let unique: std::collections::HashSet<&str> = texts.iter().copied().collect();
            assert_eq!(unique.len(), texts.len(), "{lang:?}: {texts:?}");
            assert!(texts.iter().all(|text| !text.is_empty()), "{lang:?}");
        }
        let en = &texts_for(Lang::En).usage_notice;
        assert!(
            en.fixed().iter().all(|text| text.is_ascii()),
            "英文表里不留中文"
        );
    }

    /// 服务端按它自己的语言写说明，客户端按界面语言显示：整条认得的直接换；等待说明后
    /// 拼了交互探测失败原因的两段各自换，原因认不出就原样保留；认不出的整条原样，且与
    /// 界面同语言时不重新分配。
    #[test]
    fn usage_notices_follow_the_client_language_whatever_the_server_wrote() {
        for server in [Lang::En, Lang::ZhCn] {
            for ui in [Lang::En, Lang::ZhCn] {
                let from = &texts_for(server).usage_notice;
                let to = &texts_for(ui).usage_notice;
                let _guard = lang_guard(ui);
                for (login_known, callback) in CLAUDE_WAITING_VARIANTS {
                    let waiting = from.claude_waiting(login_known, callback);
                    let expected = to.claude_waiting(login_known, callback);
                    assert_eq!(
                        localize_usage_notice(&waiting),
                        expected,
                        "{server:?} → {ui:?}"
                    );

                    let with_hint = format!(
                        "{waiting}{}{}",
                        from.interactive_failure_sep, from.trust_callback_hint
                    );
                    assert_eq!(
                        localize_usage_notice(&with_hint),
                        format!(
                            "{expected}{}{}",
                            to.interactive_failure_sep, to.trust_callback_hint
                        ),
                        "{server:?} → {ui:?}"
                    );
                    let with_detail = format!(
                        "{waiting}{}probe dir /x needs trust",
                        from.interactive_failure_sep
                    );
                    assert_eq!(
                        localize_usage_notice(&with_detail),
                        format!(
                            "{expected}{}probe dir /x needs trust",
                            to.interactive_failure_sep
                        ),
                        "{server:?} → {ui:?}"
                    );
                }
                for (source, target) in from.fixed().iter().zip(to.fixed()) {
                    assert_eq!(localize_usage_notice(source), target, "{server:?} → {ui:?}");
                }
            }
        }
        let _guard = lang_guard(Lang::En);
        let english = texts_for(Lang::En)
            .usage_notice
            .claude_waiting(true, Some(false));
        assert!(matches!(
            localize_usage_notice(&english),
            std::borrow::Cow::Borrowed(_)
        ));
        for unknown in ["HTTP 429；retry_after=60", "", "Signed in"] {
            assert_eq!(localize_usage_notice(unknown), unknown);
        }
    }

    /// 审查中级：生产路径上带目录的信任提示是模板句。中文 server 写的「等待说明 + 分隔语 +
    /// 带目录的信任句」在英文界面整句不含中文，目录与命令原样保留；反过来同理；与界面同
    /// 语言时原样返回。
    #[test]
    fn templated_trust_notice_follows_the_client_language() {
        let dir = "/home/u/.local/state/herdr/probe/claude default";
        for server in [Lang::En, Lang::ZhCn] {
            for ui in [Lang::En, Lang::ZhCn] {
                let from = &texts_for(server).usage_notice;
                let to = &texts_for(ui).usage_notice;
                let _guard = lang_guard(ui);
                let args = [("dir", dir), ("command", "claude")];
                let trust = fill(from.trust_dir_fmt, &args);
                let expected = fill(to.trust_dir_fmt, &args);
                assert_eq!(
                    localize_usage_notice(&trust),
                    expected,
                    "{server:?} → {ui:?}"
                );
                let waiting = from.claude_waiting(true, Some(false));
                let composite = format!("{waiting}{}{trust}", from.interactive_failure_sep);
                let localized = localize_usage_notice(&composite);
                assert_eq!(
                    localized,
                    format!(
                        "{}{}{expected}",
                        to.claude_waiting(true, Some(false)),
                        to.interactive_failure_sep
                    ),
                    "{server:?} → {ui:?}"
                );
                assert!(localized.contains(&format!("cd {dir} && claude")));
                if ui == Lang::En {
                    assert!(!has_cjk(&localized), "{localized}");
                }
                if server == ui {
                    assert!(matches!(
                        localize_usage_notice(&trust),
                        std::borrow::Cow::Borrowed(_)
                    ));
                }
            }
        }
    }

    #[test]
    fn unfill_reverses_fill_and_rejects_other_text() {
        let template = "run `cd {dir} && {command}` once";
        let args = [("dir", "/a b"), ("command", "claude")];
        assert_eq!(
            unfill(template, &fill(template, &args)),
            Some(args.to_vec())
        );
        assert_eq!(unfill(template, "run `cd /a b` once"), None);
        assert_eq!(unfill(template, "something else"), None);
        assert_eq!(unfill("{a}{b}", "xy"), None, "相邻占位无法切分");
        assert_eq!(unfill("plain", "plain"), Some(Vec::new()));
        assert_eq!(unfill("plain", "plain!"), None);
    }

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

    /// 冒烟 L11：术语统一为「窗格」——中文文案里不应该混用英文单词
    /// "pane" 指代同一个概念（`test_confirm_note` 曾经写「全部 pane
    /// 进程」，与仓库里其它地方一律用「窗格」不一致）。
    #[test]
    fn zh_cn_test_confirm_note_uses_the_pane_translation_consistently() {
        let note = zh_cn::TEXTS.machine_form.test_confirm_note;
        assert!(
            !note.to_lowercase().contains("pane"),
            "中文文案不该混用英文单词 pane：{note}"
        );
        assert!(note.contains("窗格"), "术语要用「窗格」：{note}");
    }

    /// 一张文案表的全部条目：解构不带 `..`，新增字段不列进来就编译不过。
    macro_rules! all_entries {
        ($table:expr, $ty:ident { $($field:ident),* $(,)? }) => {{
            let $ty { $($field),* } = $table;
            vec![$(*$field),*]
        }};
    }

    /// `RuntimeMessageTexts` 的全部条目。
    fn runtime_messages(t: &RuntimeMessageTexts) -> Vec<&'static str> {
        all_entries!(
            t,
            RuntimeMessageTexts {
                selection_history_unreadable,
                selection_changed_before_copy,
                selection_resized,
                selection_capture_failed,
                selection_copy_mismatch,
                selection_timed_out,
                views_update_failed,
                connection_settings_changed,
                host_key_review_required,
                terminal_size_not_ready,
                connection_cancelled,
                handshake_timed_out,
                snapshot_expired,
                snapshot_capacity,
                snapshot_ids_exhausted,
                snapshot_viewport_unreadable,
                snapshot_rows_out_of_range,
                snapshot_selection_out_of_range,
                snapshot_invalid_request,
                snapshot_pane_gone,
                snapshot_capture_failed,
                snapshot_needs_server,
                server_context_required,
                process_request_unsupported,
                process_busy,
                process_host_changed,
                process_changed_refresh,
                process_changed,
                process_gone,
                process_pid_reused,
                process_identity_changed,
                process_confirm_first,
                process_confirm_expired,
                process_confirm_mismatch,
                process_protected,
                gpu_driver_timeout,
                gpu_utilization_unavailable,
                hostname_unknown,
                monitor_subscription_limit,
                monitor_request_unsupported,
                monitor_busy,
                monitor_unavailable,
                observation_pane_gone,
                observation_start_failed_fmt,
            }
        )
    }

    /// `PlatformMessageTexts` 的全部条目：任一目标编译时只有本平台的条目有读者（见结构体
    /// 上的 `allow`），测试构建里在这里穷尽读取。
    fn platform_messages(t: &PlatformMessageTexts) -> Vec<&'static str> {
        all_entries!(
            t,
            PlatformMessageTexts {
                process_status_invalid,
                process_start_missing,
                process_start_invalid,
                process_name_missing,
                gpu_utilization_unexposed,
                environment_wsl,
                environment_container,
                environment_linux,
                process_force_required,
                gpu_counters_pending,
                environment_windows,
                process_handle_unsupported,
            }
        )
    }

    /// `UsageProbeTexts` 的全部条目。
    fn usage_probe_texts(t: &UsageProbeTexts) -> Vec<&'static str> {
        all_entries!(
            t,
            UsageProbeTexts {
                probe_isolation_failed,
                probe_dir_failed,
                stable_dir_failed,
                profile_unsupported,
                cli_missing,
                cli_start_failed,
                output_unavailable,
                cli_timed_out,
                cli_output_unreadable,
                cli_output_too_large,
                result_unavailable,
                cli_exit_timed_out,
                cli_help_empty,
                cli_signed_out,
                cli_signaled_fmt,
                cli_no_usage_output_fmt,
                cli_usage_error_fmt,
                detail_fmt,
                cli_failed_fmt,
                cli_failed_summary_fmt,
                no_subcommand,
                subcommand_missing,
                claude_auth_status_unsupported,
                codex_unsupported_fmt,
                codex_signed_out_fmt,
                codex_failed_fmt,
                rpc_input_closed,
                rpc_input_failed,
                rpc_output_unavailable,
                codex_timed_out,
                codex_exited,
                codex_exited_fmt,
                token_refresh_failed_fmt,
                codex_sign_in_first,
                helper_missing,
                helper_config_failed,
                helper_send_failed,
                helper_output_unavailable,
                isolated_timed_out,
                isolated_unreadable,
                isolated_too_large,
                isolated_invalid,
                interactive_profile_unsupported,
                pty_create_failed,
                pty_spawn_failed,
                pty_input_unavailable,
                pty_output_unavailable,
                pty_screen_unreadable,
                pty_protocol_failed,
                pty_cursor_unavailable,
                pty_cli_exited,
                kimi_port_failed,
                kimi_port_unreadable,
                kimi_address_invalid,
                kimi_banner_timeout,
                kimi_banner_too_large,
                kimi_web_unsupported,
                kimi_signed_out,
                kimi_exited,
                kimi_exited_fmt,
                kimi_not_ready,
                kimi_http_signed_out_fmt,
                kimi_http_forbidden_fmt,
                kimi_http_missing_fmt,
                kimi_http_error_fmt,
                kimi_connect_failed,
                kimi_request_failed,
                kimi_response_unreadable,
                kimi_response_too_large,
                kimi_format_unsupported,
                kimi_query_failed,
                api_credential_env_required,
                api_credential_missing,
                api_client_failed,
                api_kimi_base_required,
                api_provider_unsupported,
                api_query_unsupported,
                api_no_verified_fields,
                api_deadline,
                api_connect_failed,
                api_http_status_fmt,
                api_retry_after_fmt,
                api_too_large,
                api_unreadable,
                api_unrecognized,
                api_base_invalid,
                api_base_has_credentials,
                api_base_unregistered,
                zcode_no_home,
                zcode_no_database,
                zcode_non_utf8_path,
                zcode_profile_unsupported,
                zcode_sqlite_missing,
                zcode_query_failed,
                zcode_schema_mismatch,
                zcode_database_busy,
                zcode_database_unreadable,
                zcode_no_result,
                zcode_unparsable,
                source_official_api,
                source_kimi_local_api,
                source_cli_fmt,
                source_claude_callback,
                source_local_stats_fmt,
                source_extension_push,
                source_cli_callback,
                source_extension_push_stats,
                source_zcode_local,
                callback_session_stats,
                agent_unsupported,
                subscription_limit,
                unknown_account,
                binding_limit,
                request_unsupported,
                busy,
                statusline_unsupported_provider,
                statusline_unknown_provider,
                statusline_retired,
                settings_invalid_json,
                settings_empty,
                settings_not_object,
                statusline_not_object,
                statusline_create_failed,
                statusline_invalid,
                statusline_not_command,
                statusline_unrecognized,
            }
        )
    }

    /// `UsageMetricTexts` 的全部条目。
    fn usage_metric_texts(t: &UsageMetricTexts) -> Vec<&'static str> {
        all_entries!(
            t,
            UsageMetricTexts {
                quota_primary,
                quota_secondary,
                credits,
                quota_5h,
                quota_weekly,
                quota_spend,
                quota_plan,
                quota_7d,
                quota_monthly,
                quota_monthly_code,
                quota_window_fmt,
                quota_unit,
                balance_available,
                balance_voucher,
                balance_cash,
                balance_unit,
                extra_balance,
                extra_total,
                extra_month_used,
                extra_month_cap,
                key_usage,
                key_usage_daily,
                key_usage_weekly,
                key_usage_monthly,
                key_limit_remaining,
                account_total_credits,
                account_total_usage,
                cost_report,
                codex_lifetime_tokens,
                codex_peak_daily_tokens,
                codex_longest_turn,
                codex_current_streak,
                codex_longest_streak,
                codex_daily_tokens,
                session_cost_estimate,
                session_duration,
                session_api_duration,
                context_used,
                context_tokens_input,
                context_tokens,
                context_window,
                session_cost,
                current_model,
                sessions,
                subagent_sessions,
                messages,
                stats_days,
                total_cost,
                avg_cost_per_day,
                avg_tokens_per_session,
                median_tokens_per_session,
                tokens_input,
                tokens_output,
                tokens_reasoning,
                tokens_cache_read,
                tokens_cache_write,
                tokens_total,
                tokens_main,
                tokens_subagents,
                tool_uses,
                subagents,
                stats_window,
                stale_window,
                claude_context_pending,
                pi_context_pending,
            }
        )
    }

    /// `MonitorConfigTexts` 的全部条目。
    fn monitor_config_texts(t: &MonitorConfigTexts) -> Vec<&'static str> {
        all_entries!(
            t,
            MonitorConfigTexts {
                interval_invalid,
                history_invalid,
                account_id_invalid,
                credential_env_invalid,
                account_user_deprecated_fmt,
            }
        )
    }

    /// `RemoteTexts` 的全部条目。
    fn remote_texts(t: &RemoteTexts) -> Vec<&'static str> {
        all_entries!(
            t,
            RemoteTexts {
                ssh_config_unreadable,
                ssh_config_no_hostname,
                ssh_config_bad_port,
                ssh_host_unparsable,
                host_alias_not_single,
                ssh_home_missing,
                known_hosts_ambiguous,
                known_hosts_token_unsupported,
                known_hosts_disabled,
                host_key_proxied,
                ssh_config_changed_review,
                host_key_mismatch,
                known_hosts_unwritable,
                host_key_remove_failed,
                ssh_config_changed_confirm,
                trust_config_failed,
                trust_config_no_dir,
                install_upload_failed,
                scp_upload_failed,
                task_cancelled,
                task_input_closed,
                upload_timed_out,
                task_output_timed_out,
            }
        )
    }

    /// 两张表逐条对照：英文表每条都不含 CJK 字符，中文表每条都有译文（不是照抄英文）。
    fn assert_translated(en: &[&str], zh: &[&str]) {
        assert_eq!(en.len(), zh.len());
        for (en, zh) in en.iter().zip(zh) {
            assert!(!en.is_empty() && !has_cjk(en), "英文表混入中文：{en}");
            assert!(has_cjk(zh), "中文表缺译文：{zh}");
        }
    }

    /// 文档终审 D7：运行期提示原来只有中文。英文表的每一条都不含 CJK 字符，中文表
    /// 每一条都有译文（不是照抄英文）。
    #[test]
    fn runtime_messages_are_translated_in_both_tables() {
        assert_translated(
            &runtime_messages(&en::TEXTS.runtime),
            &runtime_messages(&zh_cn::TEXTS.runtime),
        );
        assert_translated(
            &platform_messages(&en::TEXTS.platform),
            &platform_messages(&zh_cn::TEXTS.platform),
        );
        assert_translated(
            &usage_probe_texts(&en::TEXTS.usage_probe),
            &usage_probe_texts(&zh_cn::TEXTS.usage_probe),
        );
        assert_translated(
            &usage_metric_texts(&en::TEXTS.usage_metric),
            &usage_metric_texts(&zh_cn::TEXTS.usage_metric),
        );
        assert_translated(
            &monitor_config_texts(&en::TEXTS.monitor_config),
            &monitor_config_texts(&zh_cn::TEXTS.monitor_config),
        );
        assert_translated(
            &remote_texts(&en::TEXTS.remote),
            &remote_texts(&zh_cn::TEXTS.remote),
        );
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
