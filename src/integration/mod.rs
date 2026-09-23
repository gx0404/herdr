mod actions;
mod claude_settings;
mod codex_trust;
mod command;
mod config_edit;
mod config_file;
mod env;
mod file_ops;
mod opencode_config;
mod registry;
mod targets;
mod types;
mod usage;
mod version;
pub(crate) use usage::configure as configure_usage;
pub(crate) use usage::settings_path as usage_settings_path;
pub(crate) use usage::statusline_enabled as usage_statusline_enabled;
pub(crate) use usage::supports_extension_push as usage_supports_extension_push;
pub(crate) use usage::supports_statusline as usage_supports_statusline;

pub(crate) use actions::{install_target, uninstall_target};
#[cfg(test)]
pub(crate) use env::integration_env_lock;
pub(crate) use env::{
    apply_pane_base_env, claude_dir, claude_state_file, codex_dir, kimi_dir, opencode_data_dir,
    HERDR_PANE_ID_ENV_VAR, HERDR_TAB_ID_ENV_VAR, HERDR_WORKSPACE_ID_ENV_VAR,
};
pub(crate) use registry::{
    codex_layout_binary_path, command_available, command_path_candidates, executable_file_exists,
    installed_integration_statuses, integration_recommendations, integration_target_available,
    integration_target_label, print_outdated_update_notice, retired_integration_error,
};
pub(crate) use types::{
    IntegrationRecommendation, IntegrationStatus, IntegrationStatusKind, IntegrationStatusNote,
};

const PI_EXTENSION_INSTALL_NAME: &str = "herdr-agent-state.ts";
const PI_EXTENSION_ASSET: &str = include_str!("assets/pi/herdr-agent-state.ts");
const PI_INTEGRATION_VERSION: u32 = 10;
const CLAUDE_HOOK_INSTALL_NAME: &str = if cfg!(windows) {
    "herdr-agent-state.ps1"
} else {
    "herdr-agent-state.sh"
};
const CLAUDE_HOOK_ASSET: &str = if cfg!(windows) {
    include_str!("assets/claude/herdr-agent-state.ps1")
} else {
    include_str!("assets/claude/herdr-agent-state.sh")
};
const CLAUDE_INTEGRATION_VERSION: u32 = 11;
const CODEX_HOOK_INSTALL_NAME: &str = if cfg!(windows) {
    "herdr-agent-state.ps1"
} else {
    "herdr-agent-state.sh"
};
const CODEX_HOOK_ASSET: &str = if cfg!(windows) {
    include_str!("assets/codex/herdr-agent-state.ps1")
} else {
    include_str!("assets/codex/herdr-agent-state.sh")
};
const CODEX_INTEGRATION_VERSION: u32 = 9;
const KIMI_HOOK_INSTALL_NAME: &str = if cfg!(windows) {
    "herdr-agent-state.ps1"
} else {
    "herdr-agent-state.sh"
};
const KIMI_HOOK_ASSET: &str = if cfg!(windows) {
    include_str!("assets/kimi/herdr-agent-state.ps1")
} else {
    include_str!("assets/kimi/herdr-agent-state.sh")
};
const KIMI_INTEGRATION_VERSION: u32 = 8;
const KIMI_CONFIG_BLOCK_BEGIN: &str = "# >>> herdr kimi integration";
const KIMI_CONFIG_BLOCK_END: &str = "# <<< herdr kimi integration";
const KIMI_MIN_VERSION: &str = "0.14.0";
const KIMI_ASK_USER_QUESTION_MATCHER: &str = "^AskUserQuestion$";
const KIMI_OTHER_TOOL_MATCHER: &str = "^(?!AskUserQuestion$).*$";
/// Kimi 的 `Notification` 以通知类型作 matcher 值，后台任务收尾是 `task.<status>`。
const KIMI_TASK_NOTIFICATION_MATCHER: &str = "^task\\.";
const KIMI_TODO_TOOL_MATCHER: &str = "^TodoList$";
/// 末尾四条 `activity` 只给活动树发「有变化」信号（`pane.report_agent_activity`）。
///
/// **本表每个事件名的最低 Kimi 版本必须 `<= KIMI_MIN_VERSION`。** Kimi 按固定枚举
/// 校验 `[[hooks]]`（`HookDefSchema` 是 `strict` 的 `event: enum(...)`），未知事件名
/// 让**整份 `config.toml` 判为非法**而不只是丢掉那一条：本表其余生命周期钩子会一起
/// 失效。两份枚举都会校验同一份 config：agent-core-v2 的 20 项与 node-sdk 的 16 项，
/// 按较窄的那份取交集。`TaskStarted` 只在 20 项里，因此暂不订阅——后台任务开始靠
/// `Notification`（`task.*`，实测通知类型是 `task.<终态>`）与轮询兜底；待
/// `KIMI_MIN_VERSION` 提到含 `TaskStarted` 的版本后再加回。
///
/// 回合的三种收尾各发一个事件且互斥：正常结束 `Stop`、用户打断 `Interrupt`、回合出错
/// `StopFailure`（provider 报错、鉴权失败等，载荷是 `errorType` / `errorMessage`）。
/// 三者都要映射到 `idle`，否则出错的回合会让 pane 停在 working 直到 kimi 退出。
/// `StopFailure` 与 `SessionEnd` 自官方仓库最早的 tag（0.2.0）起就在枚举里，
/// 0.14.0 的 legacy 引擎也会在回合出错时触发 `StopFailure`，无需按版本门控。
/// `SessionEnd` 不订阅：它只在 kimi 退出时触发，而 herdr 观察到 agent 进程退出时
/// 本就会收回该 pane 的 hook 权威。
const KIMI_HOOK_EVENTS: [(&str, Option<&str>, &str); 17] = [
    ("SessionStart", None, "session"),
    ("UserPromptSubmit", None, "working"),
    ("PreToolUse", Some(KIMI_OTHER_TOOL_MATCHER), "working"),
    (
        "PreToolUse",
        Some(KIMI_ASK_USER_QUESTION_MATCHER),
        "blocked",
    ),
    (
        "PostToolUse",
        Some(KIMI_ASK_USER_QUESTION_MATCHER),
        "working",
    ),
    (
        "PostToolUseFailure",
        Some(KIMI_ASK_USER_QUESTION_MATCHER),
        "working",
    ),
    ("SubagentStart", None, "working"),
    ("PreCompact", None, "working"),
    ("PermissionRequest", None, "blocked"),
    ("PermissionResult", None, "working"),
    ("Stop", None, "idle"),
    ("StopFailure", None, "idle"),
    ("Interrupt", None, "idle"),
    ("SubagentStart", None, "activity"),
    ("SubagentStop", None, "activity"),
    (
        "Notification",
        Some(KIMI_TASK_NOTIFICATION_MATCHER),
        "activity",
    ),
    ("PostToolUse", Some(KIMI_TODO_TOOL_MATCHER), "activity"),
];
const OPENCODE_PLUGIN_INSTALL_NAME: &str = "herdr-agent-state.js";
const OPENCODE_PLUGIN_ASSET: &str = include_str!("assets/opencode/herdr-agent-state.js");
const OPENCODE_TUI_PLUGIN_INSTALL_NAME: &str = "herdr-tui-session.js";
const OPENCODE_TUI_PLUGIN_SPEC: &str = "./herdr-tui-session.js";
const OPENCODE_TUI_PLUGIN_ASSET: &str = include_str!("assets/opencode/herdr-tui-session.js");
const OPENCODE_V2_TUI_PLUGIN_DIR: &str = "herdr-opencode";
const OPENCODE_V2_TUI_PLUGIN_SPEC: &str = "./herdr-opencode";
const OPENCODE_V2_TUI_PLUGIN_ASSET: &str = include_str!("assets/opencode/tui.js");
const OPENCODE_INTEGRATION_VERSION: u32 = 13;
const INTEGRATION_VERSION_MARKER: &str = "HERDR_INTEGRATION_VERSION=";

pub(crate) const INSTALL_WARNING_PREFIX: &str = "warning:";

#[cfg(test)]
mod tests;
