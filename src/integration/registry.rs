use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::env::*;

/// 退役变体的标签占位：它们没有独立标签表，错误与日志里用
/// `IntegrationTarget::wire_name` 指名（见 `retired_integration_error`）。
pub(crate) const RETIRED_INTEGRATION_TARGET_LABEL: &str = "retired";

pub(crate) fn integration_target_label(
    target: crate::api::schema::IntegrationTarget,
) -> &'static str {
    match target {
        crate::api::schema::IntegrationTarget::Pi => "pi",
        crate::api::schema::IntegrationTarget::Claude => "claude",
        crate::api::schema::IntegrationTarget::Codex => "codex",
        crate::api::schema::IntegrationTarget::Kimi => "kimi",
        crate::api::schema::IntegrationTarget::Opencode => "opencode",
        // 统一退役分支：冻结枚举里其余变体（含上游日后追加的）都落到这里。
        _ => RETIRED_INTEGRATION_TARGET_LABEL,
    }
}

pub(crate) fn integration_target_command(
    target: crate::api::schema::IntegrationTarget,
) -> &'static str {
    integration_target_command_names(target)
        .first()
        .copied()
        .unwrap_or(RETIRED_INTEGRATION_TARGET_LABEL)
}

pub(crate) fn integration_target_command_names(
    target: crate::api::schema::IntegrationTarget,
) -> &'static [&'static str] {
    match target {
        crate::api::schema::IntegrationTarget::Pi => &["pi"],
        crate::api::schema::IntegrationTarget::Claude => &["claude"],
        crate::api::schema::IntegrationTarget::Codex => &["codex"],
        crate::api::schema::IntegrationTarget::Kimi => &["kimi"],
        crate::api::schema::IntegrationTarget::Opencode => &["opencode"],
        // 统一退役分支：退役变体没有可探测的命令，永远不可用。
        _ => &[],
    }
}

/// 官方集成在所有平台都受支持；退役变体在任何平台都不受支持，因此不进
/// `integration.list`、`herdr integration status` 与更新提示。
pub(crate) fn integration_target_supported(target: crate::api::schema::IntegrationTarget) -> bool {
    !target.is_retired()
}

/// 退役集成的统一错误：install / uninstall 与 API 入口都返回它，旧客户端按名字传来
/// 的退役 target 因此得到明确的「已退役」而不是反序列化失败。口径见
/// `docs/AGENT_RULES/README.md` 的「fork 已删除的集成」。
pub(crate) fn retired_integration_error(
    action: &'static str,
    target: crate::api::schema::IntegrationTarget,
) -> io::Error {
    let name = target.wire_name();
    tracing::warn!(
        event = "integration.retired",
        subsystem = "integration",
        action,
        target = %name,
        "retired integration target requested"
    );
    io::Error::other(crate::i18n::fill(
        crate::i18n::texts()
            .cli_errors
            .integration_target_retired_fmt,
        &[("target", &name)],
    ))
}

pub(crate) fn integration_target_available(target: crate::api::schema::IntegrationTarget) -> bool {
    if !integration_target_supported(target) {
        return false;
    }

    integration_target_command_names(target)
        .iter()
        .any(|command| command_available(command))
        || integration_target_install_layout_available(target)
}

pub(crate) fn integration_target_install_layout_available(
    target: crate::api::schema::IntegrationTarget,
) -> bool {
    match target {
        crate::api::schema::IntegrationTarget::Codex => codex_install_layout_available(),
        _ => false,
    }
}

pub(crate) fn command_available(command: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|dir| {
        command_path_candidates(&dir, command)
            .into_iter()
            .any(|path| executable_file_exists(&path))
    })
}

pub(crate) fn command_path_candidates(dir: &Path, command: &str) -> Vec<PathBuf> {
    let base = dir.join(command);

    #[cfg(not(windows))]
    {
        vec![base]
    }

    #[cfg(windows)]
    {
        if Path::new(command).extension().is_some() {
            return vec![base];
        }

        let mut candidates = vec![base];
        for extension in [".exe", ".cmd", ".bat", ".ps1"] {
            candidates.push(dir.join(format!("{command}{extension}")));
        }
        candidates
    }
}

pub(crate) fn executable_file_exists(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }

    #[cfg(not(unix))]
    {
        true
    }
}

/// Codex availability must not depend on the server process inheriting an
/// interactive-shell PATH: detached servers (daemon/GUI launch) miss the
/// version-manager bin dirs where npm installs land, so both the native
/// standalone layout and the npm-managed layouts are checked by path.
pub(crate) fn codex_install_layout_available() -> bool {
    codex_layout_binary_path().is_some()
}

/// Absolute path to a codex binary found outside PATH (standalone release or
/// version-manager layout), preferring the newest version directory. Usage
/// probes spawn this path directly when the server PATH cannot see codex.
pub(crate) fn codex_layout_binary_path() -> Option<PathBuf> {
    codex_standalone_binary_path()
        .or_else(codex_nvm_binary_path)
        .or_else(|| {
            let home = home_dir().ok()?;
            [".volta/bin", ".bun/bin", ".local/share/pnpm"]
                .iter()
                .find_map(|segment| codex_binary_in_dir(&home.join(segment)))
        })
        .or_else(codex_windows_npm_binary_path)
}

fn codex_standalone_binary_path() -> Option<PathBuf> {
    let releases_dir = codex_dir()
        .ok()?
        .join("packages")
        .join("standalone")
        .join("releases");
    newest_versioned_binary(&releases_dir, |dir| codex_binary_in_dir(&dir.join("bin")))
}

fn codex_nvm_binary_path() -> Option<PathBuf> {
    let home = home_dir().ok()?;
    newest_versioned_binary(&home.join(".nvm").join("versions").join("node"), |dir| {
        codex_binary_in_dir(&dir.join("bin"))
    })
}

/// Picks the codex binary from the highest-versioned entry directory that
/// contains one (`v24.18.0` beats `v24.9.0` numerically, not lexically).
fn newest_versioned_binary(
    entries_dir: &Path,
    binary: impl Fn(&Path) -> Option<PathBuf>,
) -> Option<PathBuf> {
    let entries = fs::read_dir(entries_dir).ok()?;
    let mut best: Option<(Vec<u64>, PathBuf)> = None;
    for entry in entries.filter_map(Result::ok) {
        let Some(path) = binary(&entry.path()) else {
            continue;
        };
        let version = version_sort_key(&entry.file_name().to_string_lossy());
        if best.as_ref().is_none_or(|(key, _)| version > *key) {
            best = Some((version, path));
        }
    }
    best.map(|(_, path)| path)
}

fn version_sort_key(name: &str) -> Vec<u64> {
    name.chars()
        .map(|ch| if ch.is_ascii_digit() { ch } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .filter_map(|part| part.parse().ok())
        .collect()
}

fn codex_binary_in_dir(dir: &Path) -> Option<PathBuf> {
    command_path_candidates(dir, "codex")
        .into_iter()
        .find(|path| executable_file_exists(path))
}

#[cfg(windows)]
fn codex_windows_npm_binary_path() -> Option<PathBuf> {
    std::env::var_os("APPDATA")
        .filter(|value| !value.is_empty())
        .and_then(|appdata| codex_binary_in_dir(&Path::new(&appdata).join("npm")))
}

#[cfg(not(windows))]
fn codex_windows_npm_binary_path() -> Option<PathBuf> {
    None
}

#[cfg(test)]
pub(crate) fn codex_executable_name() -> &'static str {
    if cfg!(windows) {
        "codex.exe"
    } else {
        "codex"
    }
}

pub(crate) fn installed_integration_statuses() -> Vec<super::IntegrationStatus> {
    integration_specs()
        .into_iter()
        .filter_map(|(target, path, expected_version)| {
            if !integration_target_supported(target) {
                return None;
            }
            let mut status = integration_status_at(target, path.ok()?, expected_version);
            status.note = integration_status_note(&status);
            Some(status)
        })
        .collect()
}

/// 只在 `herdr integration status` 这条路径上读宿主配置：推荐列表（设置页、API）
/// 不需要这项，不为它多做文件 I/O。
fn integration_status_note(
    status: &super::IntegrationStatus,
) -> Option<super::IntegrationStatusNote> {
    if status.target != crate::api::schema::IntegrationTarget::Codex
        || status.state == super::IntegrationStatusKind::NotInstalled
    {
        return None;
    }
    let dir = codex_dir().ok()?;
    let summary = super::codex_trust::codex_hooks_trust_summary(
        &dir.join("hooks.json"),
        &dir.join("config.toml"),
        &super::targets::codex_managed_hooks(&status.path),
    );
    if summary.needs_review {
        Some(super::IntegrationStatusNote::CodexHooksNeedReview)
    } else if summary.disabled {
        Some(super::IntegrationStatusNote::CodexHooksDisabled)
    } else {
        None
    }
}

pub(crate) fn integration_recommendations() -> Vec<super::IntegrationRecommendation> {
    integration_specs()
        .into_iter()
        .filter_map(|(target, path, expected_version)| {
            if !integration_target_supported(target) {
                return None;
            }
            let path = path.ok()?;
            let status = integration_status_at(target, path.clone(), expected_version);
            Some(super::IntegrationRecommendation {
                target,
                label: integration_target_label(target),
                command: integration_target_command(target),
                available: integration_target_available(target)
                    || status.state != super::IntegrationStatusKind::NotInstalled,
                path,
                state: status.state,
            })
        })
        .collect()
}

pub(crate) fn outdated_installed_integrations() -> Vec<super::IntegrationStatus> {
    installed_integration_statuses()
        .into_iter()
        .filter(|status| status.state == super::IntegrationStatusKind::Outdated)
        .collect()
}

/// `integration.list`、状态与更新提示的来源：只登记官方集成，顺序同
/// `IntegrationTarget::ALL`。
fn integration_specs() -> [(
    crate::api::schema::IntegrationTarget,
    io::Result<PathBuf>,
    u32,
); 5] {
    [
        (
            crate::api::schema::IntegrationTarget::Pi,
            pi_extension_dir().map(|dir| dir.join(super::PI_EXTENSION_INSTALL_NAME)),
            super::PI_INTEGRATION_VERSION,
        ),
        (
            crate::api::schema::IntegrationTarget::Claude,
            claude_dir().map(|dir| dir.join("hooks").join(super::CLAUDE_HOOK_INSTALL_NAME)),
            super::CLAUDE_INTEGRATION_VERSION,
        ),
        (
            crate::api::schema::IntegrationTarget::Codex,
            codex_dir().map(|dir| dir.join(super::CODEX_HOOK_INSTALL_NAME)),
            super::CODEX_INTEGRATION_VERSION,
        ),
        (
            crate::api::schema::IntegrationTarget::Kimi,
            kimi_dir().map(|dir| dir.join("hooks").join(super::KIMI_HOOK_INSTALL_NAME)),
            super::KIMI_INTEGRATION_VERSION,
        ),
        (
            crate::api::schema::IntegrationTarget::Opencode,
            opencode_dir().map(|dir| {
                dir.join("plugins")
                    .join(super::OPENCODE_PLUGIN_INSTALL_NAME)
            }),
            super::OPENCODE_INTEGRATION_VERSION,
        ),
    ]
}

pub(crate) fn integration_update_instructions(
    targets: &[crate::api::schema::IntegrationTarget],
) -> String {
    let errors = &crate::i18n::texts().cli_errors;
    let commands: Vec<String> = targets
        .iter()
        .map(|target| {
            format!(
                "`herdr integration install {}`",
                integration_target_label(*target)
            )
        })
        .collect();

    match commands.as_slice() {
        [] => String::new(),
        [command] => crate::i18n::fill(
            errors.integration_instructions_run_fmt,
            &[("command", command)],
        ),
        [rest @ .., last] => crate::i18n::fill(
            errors.integration_instructions_run_list_fmt,
            &[("commands", &rest.join(", ")), ("last", last)],
        ),
    }
}

pub(crate) fn print_outdated_update_notice() -> bool {
    let outdated = outdated_installed_integrations();
    if outdated.is_empty() {
        return false;
    }

    let targets = outdated
        .iter()
        .map(|integration| integration.target)
        .collect::<Vec<_>>();
    eprintln!(
        "{}",
        crate::i18n::fill(
            crate::i18n::texts()
                .cli_errors
                .integrations_need_updating_fmt,
            &[("instructions", &integration_update_instructions(&targets))]
        )
        .replace('`', "")
    );
    true
}

fn opencode_tui_integration_is_valid(plugin_path: &Path, expected_version: u32) -> bool {
    let Some(config_dir) = plugin_path.parent().and_then(Path::parent) else {
        return false;
    };
    let tui_plugin_path = config_dir.join(super::OPENCODE_TUI_PLUGIN_INSTALL_NAME);
    let tui_plugin_current = fs::read_to_string(tui_plugin_path)
        .ok()
        .and_then(|content| parse_integration_version(&content))
        .is_some_and(|version| version >= expected_version);
    tui_plugin_current
        && super::opencode_config::tui_plugin_is_configured(
            config_dir,
            super::OPENCODE_TUI_PLUGIN_SPEC,
        )
        && (!config_dir.join("cli.json").exists()
            || (super::opencode_config::cli_plugin_is_configured(
                config_dir,
                super::OPENCODE_V2_TUI_PLUGIN_SPEC,
            ) && fs::read_to_string(
                config_dir
                    .join(super::OPENCODE_V2_TUI_PLUGIN_DIR)
                    .join("tui.js"),
            )
            .ok()
            .and_then(|content| parse_integration_version(&content))
            .is_some_and(|version| version >= expected_version)))
}

fn integration_state_for_path(
    path: &Path,
    expected_version: u32,
) -> (super::IntegrationStatusKind, Option<u32>) {
    if !path.is_file() {
        return (super::IntegrationStatusKind::NotInstalled, None);
    }

    let installed_version = fs::read_to_string(path)
        .ok()
        .and_then(|content| parse_integration_version(&content));
    let state = if installed_version.is_some_and(|version| version >= expected_version) {
        super::IntegrationStatusKind::Current
    } else {
        super::IntegrationStatusKind::Outdated
    };

    (state, installed_version)
}

pub(crate) fn integration_status_at(
    target: crate::api::schema::IntegrationTarget,
    path: PathBuf,
    expected_version: u32,
) -> super::IntegrationStatus {
    let (mut state, installed_version) = integration_state_for_path(&path, expected_version);

    if target == crate::api::schema::IntegrationTarget::Opencode
        && state == super::IntegrationStatusKind::Current
        && !opencode_tui_integration_is_valid(&path, expected_version)
    {
        state = super::IntegrationStatusKind::Outdated;
    }

    super::IntegrationStatus {
        target,
        path,
        state,
        installed_version,
        expected_version,
        note: None,
    }
}

pub(crate) fn parse_integration_version(content: &str) -> Option<u32> {
    content.lines().find_map(|line| {
        let marker_line = line
            .trim()
            .trim_start_matches('/')
            .trim_start_matches('#')
            .trim();
        marker_line
            .strip_prefix(super::INTEGRATION_VERSION_MARKER)?
            .trim()
            .parse()
            .ok()
    })
}
