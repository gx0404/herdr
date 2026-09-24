use std::path::PathBuf;

#[derive(Debug)]
pub(crate) struct ClaudeInstallPaths {
    pub hook_path: PathBuf,
    pub settings_path: PathBuf,
}

#[derive(Debug)]
pub(crate) struct CodexInstallPaths {
    pub hook_path: PathBuf,
    pub hooks_path: PathBuf,
    pub config_path: PathBuf,
}

#[derive(Debug)]
pub(crate) struct KimiInstallPaths {
    pub hook_path: PathBuf,
    pub config_path: PathBuf,
}

#[derive(Debug)]
pub(crate) struct OpenCodeInstallPaths {
    pub plugin_path: PathBuf,
    pub tui_plugin_path: PathBuf,
    pub tui_config_path: PathBuf,
    pub cli_config_path: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IntegrationStatus {
    pub target: crate::api::schema::IntegrationTarget,
    pub path: PathBuf,
    pub state: IntegrationStatusKind,
    pub installed_version: Option<u32>,
    pub expected_version: u32,
    /// 版本之外还需要用户处理的事，`herdr integration status` 另起一行提示。
    pub note: Option<IntegrationStatusNote>,
}

/// 集成装好了但宿主还没让它生效的情形。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IntegrationStatusNote {
    /// codex 还没信任 herdr 的钩子（或信任后钩子改过）：下次启动会弹「Hooks need
    /// review」，信任前钩子不运行。
    CodexHooksNeedReview,
    /// 用户在 codex 里停用了 herdr 的钩子。
    CodexHooksDisabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IntegrationStatusKind {
    NotInstalled,
    Current,
    Outdated,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IntegrationRecommendation {
    pub target: crate::api::schema::IntegrationTarget,
    pub label: &'static str,
    pub command: &'static str,
    pub available: bool,
    pub path: PathBuf,
    pub state: IntegrationStatusKind,
}

impl IntegrationRecommendation {
    pub fn needs_install(&self) -> bool {
        self.state == IntegrationStatusKind::Outdated
            || (self.available && self.state == IntegrationStatusKind::NotInstalled)
    }
}

#[derive(Debug)]
pub(crate) struct PiUninstallResult {
    pub extension_path: PathBuf,
    pub removed_extension: bool,
}

#[derive(Debug)]
pub(crate) struct ClaudeUninstallResult {
    pub hook_path: PathBuf,
    pub settings_path: PathBuf,
    pub removed_hook_file: bool,
    pub updated_settings: bool,
}

#[derive(Debug)]
pub(crate) struct CodexUninstallResult {
    pub hook_path: PathBuf,
    pub hooks_path: PathBuf,
    pub config_path: PathBuf,
    pub removed_hook_file: bool,
    pub updated_hooks: bool,
}

#[derive(Debug)]
pub(crate) struct KimiUninstallResult {
    pub hook_path: PathBuf,
    pub config_path: PathBuf,
    pub removed_hook_file: bool,
    pub updated_config: bool,
}

#[derive(Debug)]
pub(crate) struct OpenCodeUninstallResult {
    pub plugin_path: PathBuf,
    pub tui_plugin_path: PathBuf,
    pub removed_plugin: bool,
    pub removed_tui_plugin: bool,
    pub updated_tui_configs: Vec<PathBuf>,
}
