mod actions;
mod claude_settings;
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
pub(crate) use types::{IntegrationRecommendation, IntegrationStatus, IntegrationStatusKind};

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
const CLAUDE_INTEGRATION_VERSION: u32 = 10;
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
const CODEX_INTEGRATION_VERSION: u32 = 8;
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
const KIMI_INTEGRATION_VERSION: u32 = 7;
const KIMI_CONFIG_BLOCK_BEGIN: &str = "# >>> herdr kimi integration";
const KIMI_CONFIG_BLOCK_END: &str = "# <<< herdr kimi integration";
const KIMI_MIN_VERSION: &str = "0.14.0";
const KIMI_ASK_USER_QUESTION_MATCHER: &str = "^AskUserQuestion$";
const KIMI_OTHER_TOOL_MATCHER: &str = "^(?!AskUserQuestion$).*$";
const KIMI_HOOK_EVENTS: [(&str, Option<&str>, &str); 12] = [
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
    ("Interrupt", None, "idle"),
];
const OPENCODE_PLUGIN_INSTALL_NAME: &str = "herdr-agent-state.js";
const OPENCODE_PLUGIN_ASSET: &str = include_str!("assets/opencode/herdr-agent-state.js");
const OPENCODE_TUI_PLUGIN_INSTALL_NAME: &str = "herdr-tui-session.js";
const OPENCODE_TUI_PLUGIN_SPEC: &str = "./herdr-tui-session.js";
const OPENCODE_TUI_PLUGIN_ASSET: &str = include_str!("assets/opencode/herdr-tui-session.js");
const OPENCODE_V2_TUI_PLUGIN_DIR: &str = "herdr-opencode";
const OPENCODE_V2_TUI_PLUGIN_SPEC: &str = "./herdr-opencode";
const OPENCODE_V2_TUI_PLUGIN_ASSET: &str = include_str!("assets/opencode/tui.js");
const OPENCODE_INTEGRATION_VERSION: u32 = 12;
const INTEGRATION_VERSION_MARKER: &str = "HERDR_INTEGRATION_VERSION=";

pub(crate) const INSTALL_WARNING_PREFIX: &str = "warning:";

#[cfg(test)]
mod tests;
