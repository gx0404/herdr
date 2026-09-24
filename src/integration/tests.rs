use super::command::*;
use super::env::*;
use super::file_ops::*;
use super::registry::*;
use super::targets::*;
#[cfg(windows)]
use super::test_support::symlink_file;
use super::types::*;
use super::version::*;
use super::*;

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

#[test]
fn extract_version_triple_parses_common_outputs() {
    assert_eq!(extract_version_triple("0.14.0"), Some((0, 14, 0)));
    assert_eq!(extract_version_triple("v1.2.3"), Some((1, 2, 3)));
    assert_eq!(
        extract_version_triple("kimi-code 0.14.0 (linux/x64)"),
        Some((0, 14, 0))
    );
    assert_eq!(extract_version_triple("0.14"), Some((0, 14, 0)));
    assert_eq!(extract_version_triple("0.14.1-beta.2"), Some((0, 14, 1)));
    assert_eq!(extract_version_triple("no version here"), None);
    assert_eq!(extract_version_triple(""), None);
}

#[test]
fn extract_version_triple_orders_versions() {
    let old = extract_version_triple("0.12.1").unwrap();
    let min = extract_version_triple(KIMI_MIN_VERSION).unwrap();
    let new = extract_version_triple("0.15.0").unwrap();
    assert!(old < min);
    assert!(min <= min);
    assert!(min < new);
}

#[test]
fn agent_version_requirement_only_set_for_kimi() {
    let requirement = agent_version_requirement(crate::api::schema::IntegrationTarget::Kimi)
        .expect("kimi must have a version requirement");
    assert_eq!(requirement.binary, "kimi");
    assert_eq!(requirement.min_version, KIMI_MIN_VERSION);
    assert!(agent_version_requirement(crate::api::schema::IntegrationTarget::Claude).is_none());
    assert!(agent_version_requirement(crate::api::schema::IntegrationTarget::Codex).is_none());
}

#[test]
fn enforce_agent_version_warns_when_binary_missing() {
    let requirement = AgentVersionRequirement {
        label: "kimi code",
        binary: "herdr-test-binary-that-does-not-exist",
        args: &["--version"],
        min_version: "0.14.0",
    };
    let warning = enforce_agent_version(&requirement)
        .expect("missing binary must not fail the install")
        .expect("missing binary must produce a warning");
    assert!(warning.contains("could not run"));
    assert!(warning.contains("0.14.0"));
}

#[cfg(unix)]
#[test]
fn enforce_agent_version_rejects_old_version() {
    let requirement = AgentVersionRequirement {
        label: "kimi code",
        binary: "echo",
        args: &["0.12.1"],
        min_version: "0.14.0",
    };
    let err = enforce_agent_version(&requirement).expect_err("old version must fail the install");
    let message = err.to_string();
    assert!(message.contains("0.12.1"));
    assert!(message.contains("0.14.0"));
    assert!(message.contains("upgrade"));
}

#[cfg(unix)]
#[test]
fn enforce_agent_version_accepts_current_version() {
    let requirement = AgentVersionRequirement {
        label: "kimi code",
        binary: "echo",
        args: &["0.14.0"],
        min_version: "0.14.0",
    };
    let result =
        enforce_agent_version(&requirement).expect("matching version must not fail the install");
    assert!(result.is_none(), "matching version must not warn");
}

fn clear_integration_path_env() {
    std::env::remove_var(PI_CODING_AGENT_DIR_ENV_VAR);
    std::env::remove_var(CLAUDE_CONFIG_DIR_ENV_VAR);
    std::env::remove_var(CODEX_HOME_ENV_VAR);
    std::env::remove_var(KIMI_CODE_HOME_ENV_VAR);
    std::env::remove_var("XDG_CONFIG_HOME");
    std::env::remove_var("XDG_STATE_HOME");
    #[cfg(windows)]
    std::env::remove_var("APPDATA");
}

fn kimi_hook_command(hook_path: &Path, action: &str) -> String {
    hook_command(hook_path, Some(action))
}

fn kimi_config_hooks(config: &str) -> Vec<toml::Value> {
    let parsed: toml::Value = toml::from_str(config).unwrap();
    parsed
        .get("hooks")
        .and_then(toml::Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn assert_kimi_hook(
    config: &str,
    hook_path: &Path,
    event: &str,
    matcher: Option<&str>,
    action: &str,
) {
    let command = kimi_hook_command(hook_path, action);
    let hooks = kimi_config_hooks(config);
    assert!(
        hooks.iter().any(|hook| {
            hook.get("event").and_then(toml::Value::as_str) == Some(event)
                && hook.get("matcher").and_then(toml::Value::as_str) == matcher
                && hook.get("command").and_then(toml::Value::as_str) == Some(command.as_str())
                && hook.get("timeout").and_then(toml::Value::as_integer) == Some(10)
        }),
        "missing kimi hook for {event} ({matcher:?}) -> {action}"
    );
}

fn unique_base() -> PathBuf {
    clear_integration_path_env();
    std::env::temp_dir().join(format!(
        "herdr-integration-install-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

#[cfg(windows)]
#[test]
fn home_dir_uses_userprofile_when_home_is_missing() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let previous_home = std::env::var_os("HOME");
    let previous_userprofile = std::env::var_os("USERPROFILE");
    std::env::remove_var("HOME");
    std::env::set_var("USERPROFILE", &base);

    assert_eq!(home_dir().unwrap(), base);

    if let Some(home) = previous_home {
        std::env::set_var("HOME", home);
    }
    if let Some(userprofile) = previous_userprofile {
        std::env::set_var("USERPROFILE", userprofile);
    } else {
        std::env::remove_var("USERPROFILE");
    }
}

#[cfg(windows)]
#[test]
fn windows_supports_every_official_integration() {
    use crate::api::schema::IntegrationTarget;

    for target in IntegrationTarget::ALL {
        assert!(integration_target_supported(target), "{target:?}");
    }
}

#[cfg(windows)]
#[test]
fn windows_availability_includes_native_integrations() {
    use crate::api::schema::IntegrationTarget;

    let _lock = integration_env_lock();
    let base = unique_base();
    let bin = base.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let original_path = std::env::var_os("PATH");
    std::env::set_var("PATH", &bin);

    fs::write(bin.join("pi.cmd"), "@echo off\r\n").unwrap();
    fs::write(bin.join("opencode.cmd"), "@echo off\r\n").unwrap();
    fs::write(bin.join("kimi.exe"), "").unwrap();

    assert!(integration_target_available(IntegrationTarget::Pi));
    assert!(integration_target_available(IntegrationTarget::Opencode));
    assert!(integration_target_available(IntegrationTarget::Kimi));

    if let Some(path) = original_path {
        std::env::set_var("PATH", path);
    } else {
        std::env::remove_var("PATH");
    }
    let _ = fs::remove_dir_all(base);
}

/// 冻结枚举的全部 serde 名（generation-1 契约，见 `tests/fixtures/
/// endpoint-method-shapes-v1.json` 的 `integration.install` 摘要）。退役测试用它
/// 枚举「旧客户端可能传来的每一个名字」。
const FROZEN_INTEGRATION_TARGET_WIRE_NAMES: [&str; 17] = [
    "pi",
    "omp",
    "claude",
    "codex",
    "copilot",
    "devin",
    "droid",
    "kimi",
    "opencode",
    "kilo",
    "hermes",
    "qodercli",
    "qwen",
    "cursor",
    "mastracode",
    "antigravity_cli",
    "grok",
];

#[test]
fn frozen_integration_targets_still_deserialize_and_split_into_official_and_retired() {
    use crate::api::schema::IntegrationTarget;

    let mut official = Vec::new();
    for name in FROZEN_INTEGRATION_TARGET_WIRE_NAMES {
        // 旧客户端按名字发来的 target 必须仍能反序列化：枚举变体一个都不能删。
        let target: IntegrationTarget = serde_json::from_value(json!(name))
            .unwrap_or_else(|err| panic!("{name} 不再能反序列化: {err}"));
        assert_eq!(target.wire_name(), name);
        assert_eq!(IntegrationTarget::from_wire_name(name), Some(target));
        if !target.is_retired() {
            official.push(name);
        }
    }
    assert_eq!(official, ["pi", "claude", "codex", "kimi", "opencode"]);
    assert_eq!(
        IntegrationTarget::ALL.map(integration_target_label),
        ["pi", "claude", "codex", "kimi", "opencode"]
    );
}

#[test]
fn retired_integration_targets_get_a_retired_error_and_stay_out_of_listings() {
    use crate::api::schema::IntegrationTarget;

    let _lock = integration_env_lock();
    let base = unique_base();
    let original_home = std::env::var_os("HOME");
    std::env::set_var("HOME", &base);
    clear_integration_path_env();

    for name in FROZEN_INTEGRATION_TARGET_WIRE_NAMES {
        let target = IntegrationTarget::from_wire_name(name).unwrap();
        if !target.is_retired() {
            continue;
        }
        assert!(!integration_target_supported(target), "{name}");
        assert!(!integration_target_available(target), "{name}");
        for (lang, marker) in [
            (crate::i18n::Lang::En, "retired"),
            (crate::i18n::Lang::ZhCn, "退役"),
        ] {
            let _lang = crate::i18n::lang_guard(lang);
            for (action, result) in [
                ("install", install_target(target)),
                ("uninstall", uninstall_target(target)),
            ] {
                let message = result
                    .expect_err(&format!("{action} {name} 必须失败"))
                    .to_string();
                assert!(
                    message.contains(name) && message.contains(marker),
                    "{action} {name}: {message}"
                );
            }
        }
    }
    // 退役错误不得在 HOME 下留下任何文件。
    assert!(!base.exists() || fs::read_dir(&base).unwrap().next().is_none());

    let listed: Vec<_> = integration_recommendations()
        .into_iter()
        .map(|recommendation| recommendation.target)
        .collect();
    assert_eq!(listed, IntegrationTarget::ALL);
    assert!(installed_integration_statuses()
        .iter()
        .all(|status| !status.target.is_retired()));

    if let Some(home) = original_home {
        std::env::set_var("HOME", home);
    } else {
        std::env::remove_var("HOME");
    }
    let _ = fs::remove_dir_all(base);
}

#[test]
#[cfg(unix)]
fn command_available_requires_executable_file_on_path() {
    use std::os::unix::fs::PermissionsExt;

    let _lock = integration_env_lock();
    let base = unique_base();
    let bin = base.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let original_path = std::env::var_os("PATH");
    std::env::set_var("PATH", &bin);

    let command = bin.join("claude");
    fs::write(&command, "#!/bin/sh\n").unwrap();
    fs::set_permissions(&command, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(!command_available("claude"));

    fs::set_permissions(&command, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(command_available("claude"));

    if let Some(path) = original_path {
        std::env::set_var("PATH", path);
    } else {
        std::env::remove_var("PATH");
    }
    let _ = fs::remove_dir_all(base);
}

#[test]
#[cfg(windows)]
fn command_available_finds_windows_command_shims_on_path() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let bin = base.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let original_path = std::env::var_os("PATH");
    std::env::set_var("PATH", &bin);

    fs::write(bin.join("claude.cmd"), "@echo off\r\n").unwrap();
    assert!(command_available("claude"));

    fs::write(bin.join("codex.exe"), "").unwrap();
    assert!(command_available("codex"));

    assert!(!command_available("missing-agent"));

    if let Some(path) = original_path {
        std::env::set_var("PATH", path);
    } else {
        std::env::remove_var("PATH");
    }
    let _ = fs::remove_dir_all(base);
}

#[test]
fn codex_availability_finds_standalone_binary_under_codex_home() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let bin = home
        .join(".codex/packages/standalone/releases/0.137.0-test")
        .join("bin");
    fs::create_dir_all(&bin).unwrap();
    let binary = bin.join(codex_executable_name());
    fs::write(&binary, "").unwrap();
    make_executable(&binary).unwrap();
    let original_home = std::env::var_os("HOME");
    let original_path = std::env::var_os("PATH");
    std::env::set_var("HOME", &home);
    std::env::set_var("PATH", "");

    assert!(integration_target_available(
        crate::api::schema::IntegrationTarget::Codex
    ));

    if let Some(home) = original_home {
        std::env::set_var("HOME", home);
    } else {
        std::env::remove_var("HOME");
    }
    if let Some(path) = original_path {
        std::env::set_var("PATH", path);
    } else {
        std::env::remove_var("PATH");
    }
    let _ = fs::remove_dir_all(base);
}

#[test]
fn codex_availability_finds_nvm_managed_npm_binary() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let bin = home.join(".nvm/versions/node/v24.0.0").join("bin");
    fs::create_dir_all(&bin).unwrap();
    let binary = bin.join(codex_executable_name());
    fs::write(&binary, "").unwrap();
    make_executable(&binary).unwrap();
    let original_home = std::env::var_os("HOME");
    let original_path = std::env::var_os("PATH");
    let original_codex_home = std::env::var_os(CODEX_HOME_ENV_VAR);
    std::env::set_var("HOME", &home);
    std::env::set_var("PATH", "");
    std::env::remove_var(CODEX_HOME_ENV_VAR);

    assert!(integration_target_available(
        crate::api::schema::IntegrationTarget::Codex
    ));

    if let Some(home) = original_home {
        std::env::set_var("HOME", home);
    } else {
        std::env::remove_var("HOME");
    }
    if let Some(path) = original_path {
        std::env::set_var("PATH", path);
    } else {
        std::env::remove_var("PATH");
    }
    if let Some(codex_home) = original_codex_home {
        std::env::set_var(CODEX_HOME_ENV_VAR, codex_home);
    } else {
        std::env::remove_var(CODEX_HOME_ENV_VAR);
    }
    let _ = fs::remove_dir_all(base);
}

#[test]
fn codex_availability_finds_js_package_manager_global_bins() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let original_home = std::env::var_os("HOME");
    let original_path = std::env::var_os("PATH");
    let original_codex_home = std::env::var_os(CODEX_HOME_ENV_VAR);
    std::env::set_var("PATH", "");
    std::env::remove_var(CODEX_HOME_ENV_VAR);

    for segment in [".volta/bin", ".bun/bin", ".local/share/pnpm"] {
        let home = base.join(segment.replace(['/', '.'], "_"));
        let bin = home.join(segment);
        fs::create_dir_all(&bin).unwrap();
        let binary = bin.join(codex_executable_name());
        fs::write(&binary, "").unwrap();
        make_executable(&binary).unwrap();
        std::env::set_var("HOME", &home);

        assert!(
            integration_target_available(crate::api::schema::IntegrationTarget::Codex),
            "codex should be available via {segment}"
        );
    }

    if let Some(home) = original_home {
        std::env::set_var("HOME", home);
    } else {
        std::env::remove_var("HOME");
    }
    if let Some(path) = original_path {
        std::env::set_var("PATH", path);
    } else {
        std::env::remove_var("PATH");
    }
    if let Some(codex_home) = original_codex_home {
        std::env::set_var(CODEX_HOME_ENV_VAR, codex_home);
    } else {
        std::env::remove_var(CODEX_HOME_ENV_VAR);
    }
    let _ = fs::remove_dir_all(base);
}

#[test]
#[cfg(windows)]
fn codex_availability_finds_windows_npm_global_shim() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let npm_bin = base.join("appdata").join("npm");
    fs::create_dir_all(&npm_bin).unwrap();
    fs::write(npm_bin.join("codex.cmd"), "@echo off\r\n").unwrap();
    let original_home = std::env::var_os("HOME");
    let original_path = std::env::var_os("PATH");
    let original_codex_home = std::env::var_os(CODEX_HOME_ENV_VAR);
    std::env::set_var("HOME", &home);
    std::env::set_var("PATH", "");
    std::env::remove_var(CODEX_HOME_ENV_VAR);
    std::env::set_var("APPDATA", base.join("appdata"));

    assert!(integration_target_available(
        crate::api::schema::IntegrationTarget::Codex
    ));

    if let Some(home) = original_home {
        std::env::set_var("HOME", home);
    } else {
        std::env::remove_var("HOME");
    }
    if let Some(path) = original_path {
        std::env::set_var("PATH", path);
    } else {
        std::env::remove_var("PATH");
    }
    if let Some(codex_home) = original_codex_home {
        std::env::set_var(CODEX_HOME_ENV_VAR, codex_home);
    } else {
        std::env::remove_var(CODEX_HOME_ENV_VAR);
    }
    let _ = fs::remove_dir_all(base);
}

#[test]
fn codex_availability_stays_false_without_path_or_layout() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    fs::create_dir_all(&home).unwrap();
    let original_home = std::env::var_os("HOME");
    let original_path = std::env::var_os("PATH");
    let original_codex_home = std::env::var_os(CODEX_HOME_ENV_VAR);
    std::env::set_var("HOME", &home);
    std::env::set_var("PATH", "");
    std::env::remove_var(CODEX_HOME_ENV_VAR);

    assert!(!integration_target_available(
        crate::api::schema::IntegrationTarget::Codex
    ));

    if let Some(home) = original_home {
        std::env::set_var("HOME", home);
    } else {
        std::env::remove_var("HOME");
    }
    if let Some(path) = original_path {
        std::env::set_var("PATH", path);
    } else {
        std::env::remove_var("PATH");
    }
    if let Some(codex_home) = original_codex_home {
        std::env::set_var(CODEX_HOME_ENV_VAR, codex_home);
    } else {
        std::env::remove_var(CODEX_HOME_ENV_VAR);
    }
    let _ = fs::remove_dir_all(base);
}

#[test]
fn integration_recommendations_mark_standalone_codex_available() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let bin = home
        .join(".codex/packages/standalone/releases/0.137.0-test")
        .join("bin");
    fs::create_dir_all(&bin).unwrap();
    let binary = bin.join(codex_executable_name());
    fs::write(&binary, "").unwrap();
    make_executable(&binary).unwrap();
    let original_home = std::env::var_os("HOME");
    let original_path = std::env::var_os("PATH");
    std::env::set_var("HOME", &home);
    std::env::set_var("PATH", "");

    let codex = integration_recommendations()
        .into_iter()
        .find(|recommendation| {
            recommendation.target == crate::api::schema::IntegrationTarget::Codex
        })
        .expect("codex recommendation should be present");

    assert!(codex.available);
    assert_eq!(codex.state, IntegrationStatusKind::NotInstalled);
    assert!(codex.needs_install());

    if let Some(home) = original_home {
        std::env::set_var("HOME", home);
    } else {
        std::env::remove_var("HOME");
    }
    if let Some(path) = original_path {
        std::env::set_var("PATH", path);
    } else {
        std::env::remove_var("PATH");
    }
    let _ = fs::remove_dir_all(base);
}

#[test]
fn integration_recommendation_installs_available_or_outdated_targets() {
    let mut recommendation = IntegrationRecommendation {
        target: crate::api::schema::IntegrationTarget::Claude,
        label: "claude",
        command: "claude",
        available: false,
        path: PathBuf::from("/tmp/herdr-agent-state.sh"),
        state: IntegrationStatusKind::NotInstalled,
    };
    assert!(!recommendation.needs_install());

    recommendation.available = true;
    assert!(recommendation.needs_install());

    recommendation.available = false;
    recommendation.state = IntegrationStatusKind::Outdated;
    assert!(recommendation.needs_install());

    recommendation.available = true;
    recommendation.state = IntegrationStatusKind::Current;
    assert!(!recommendation.needs_install());
}

#[test]
fn install_pi_writes_embedded_asset_to_pi_extensions_dir() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let ext_dir = home.join(".pi/agent/extensions");
    fs::create_dir_all(&ext_dir).unwrap();
    std::env::set_var("HOME", &home);

    let path = install_pi().unwrap();
    let content = fs::read_to_string(&path).unwrap();

    assert_eq!(path, ext_dir.join(PI_EXTENSION_INSTALL_NAME));
    assert_eq!(content, PI_EXTENSION_ASSET);

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_pi_creates_extensions_dir_when_agent_dir_exists() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let agent_dir = home.join(".pi/agent");
    fs::create_dir_all(&agent_dir).unwrap();
    std::env::set_var("HOME", &home);

    let path = install_pi().unwrap();

    assert_eq!(
        path,
        agent_dir.join("extensions").join(PI_EXTENSION_INSTALL_NAME)
    );
    assert!(path.is_file());

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_pi_uses_pi_coding_agent_dir_env() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let agent_dir = base.join("custom-pi-agent");
    let ext_dir = agent_dir.join("extensions");
    fs::create_dir_all(&ext_dir).unwrap();
    std::env::set_var(PI_CODING_AGENT_DIR_ENV_VAR, &agent_dir);

    let path = install_pi().unwrap();

    assert_eq!(path, ext_dir.join(PI_EXTENSION_INSTALL_NAME));

    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_pi_expands_tilde_in_pi_coding_agent_dir_env() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let ext_dir = home.join("custom-pi-agent/extensions");
    fs::create_dir_all(&ext_dir).unwrap();
    std::env::set_var("HOME", &home);
    std::env::set_var(PI_CODING_AGENT_DIR_ENV_VAR, "~/custom-pi-agent");

    let path = install_pi().unwrap();

    assert_eq!(path, ext_dir.join(PI_EXTENSION_INSTALL_NAME));

    std::env::remove_var("HOME");
    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}

#[test]
fn uninstall_pi_removes_embedded_extension_when_present() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let ext_dir = home.join(".pi/agent/extensions");
    fs::create_dir_all(&ext_dir).unwrap();
    fs::write(ext_dir.join(PI_EXTENSION_INSTALL_NAME), PI_EXTENSION_ASSET).unwrap();
    std::env::set_var("HOME", &home);

    let result = uninstall_pi().unwrap();

    assert_eq!(
        result.extension_path,
        ext_dir.join(PI_EXTENSION_INSTALL_NAME)
    );
    assert!(result.removed_extension);
    assert!(!result.extension_path.exists());

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn outdated_integrations_treat_missing_version_marker_as_legacy() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let ext_dir = home.join(".pi/agent/extensions");
    fs::create_dir_all(&ext_dir).unwrap();
    let extension_path = ext_dir.join(PI_EXTENSION_INSTALL_NAME);
    fs::write(&extension_path, "// installed by herdr\n").unwrap();
    std::env::set_var("HOME", &home);

    let outdated = outdated_installed_integrations();

    assert_eq!(outdated.len(), 1);
    assert_eq!(
        outdated[0].target,
        crate::api::schema::IntegrationTarget::Pi
    );
    assert_eq!(outdated[0].path, extension_path);
    assert_eq!(outdated[0].installed_version, None);
    assert_eq!(outdated[0].expected_version, PI_INTEGRATION_VERSION);

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn outdated_integrations_detect_previous_pi_version() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let ext_dir = home.join(".pi/agent/extensions");
    fs::create_dir_all(&ext_dir).unwrap();
    let extension_path = ext_dir.join(PI_EXTENSION_INSTALL_NAME);
    fs::write(
        &extension_path,
        "// HERDR_INTEGRATION_ID=pi\n// HERDR_INTEGRATION_VERSION=4\n",
    )
    .unwrap();
    std::env::set_var("HOME", &home);

    let outdated = outdated_installed_integrations();

    assert_eq!(outdated.len(), 1);
    assert_eq!(
        outdated[0].target,
        crate::api::schema::IntegrationTarget::Pi
    );
    assert_eq!(outdated[0].path, extension_path);
    assert_eq!(outdated[0].installed_version, Some(4));
    assert_eq!(outdated[0].expected_version, PI_INTEGRATION_VERSION);

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn outdated_integrations_accept_current_version_marker() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let ext_dir = home.join(".pi/agent/extensions");
    fs::create_dir_all(&ext_dir).unwrap();
    fs::write(ext_dir.join(PI_EXTENSION_INSTALL_NAME), PI_EXTENSION_ASSET).unwrap();
    std::env::set_var("HOME", &home);

    assert!(outdated_installed_integrations().is_empty());

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_pi_errors_when_extension_dir_missing() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    fs::create_dir_all(&home).unwrap();
    std::env::set_var("HOME", &home);

    let err = install_pi().unwrap_err().to_string();

    assert!(err.contains("pi extension directory not found"));

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_claude_writes_hook_and_updates_settings() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let claude_dir = home.join(".claude");
    fs::create_dir_all(&claude_dir).unwrap();
    fs::write(
        claude_dir.join("settings.json"),
        r#"{"permissions":{"allow":["Read"]},"hooks":{}}"#,
    )
    .unwrap();
    std::env::set_var("HOME", &home);

    let installed = install_claude().unwrap();
    let hook_content = fs::read_to_string(&installed.hook_path).unwrap();
    let settings: Value =
        serde_json::from_str(&fs::read_to_string(&installed.settings_path).unwrap()).unwrap();

    assert_eq!(
        installed.hook_path,
        claude_dir.join("hooks").join(CLAUDE_HOOK_INSTALL_NAME)
    );
    assert_eq!(hook_content, CLAUDE_HOOK_ASSET);
    assert!(settings["permissions"]["allow"].is_array());
    assert_eq!(
        settings["hooks"]["SessionStart"][0]["matcher"],
        "^(startup|resume|clear|compact|fork)$"
    );
    assert!(settings["hooks"]["SessionStart"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap()
        .contains(" session"));
    assert!(settings["hooks"].get("UserPromptSubmit").is_none());
    assert!(settings["hooks"].get("PreToolUse").is_none());
    assert!(settings["hooks"].get("PermissionRequest").is_none());
    assert!(settings["hooks"].get("PostToolUse").is_none());
    assert!(settings["hooks"].get("PostToolUseFailure").is_none());
    // SubagentStop 只挂活动信号钩子（Windows 上不装），旧的 working 动作不得复活。
    assert!(settings["hooks"]["SubagentStop"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|group| group["hooks"].as_array().into_iter().flatten())
        .all(|hook| hook["command"]
            .as_str()
            .is_some_and(|command| command.ends_with(" activity"))));
    assert!(settings["hooks"].get("Stop").is_none());
    assert!(settings["hooks"].get("SessionEnd").is_none());

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_claude_uses_claude_config_dir_env() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let claude_dir = base.join("custom-claude");
    fs::create_dir_all(&claude_dir).unwrap();
    std::env::set_var(CLAUDE_CONFIG_DIR_ENV_VAR, &claude_dir);

    let installed = install_claude().unwrap();

    assert_eq!(installed.settings_path, claude_dir.join("settings.json"));
    assert_eq!(
        installed.hook_path,
        claude_dir.join("hooks").join(CLAUDE_HOOK_INSTALL_NAME)
    );

    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_claude_is_idempotent_for_hook_entries() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let claude_dir = home.join(".claude");
    fs::create_dir_all(&claude_dir).unwrap();
    std::env::set_var("HOME", &home);

    install_claude().unwrap();
    install_claude().unwrap();

    let settings: Value =
        serde_json::from_str(&fs::read_to_string(claude_dir.join("settings.json")).unwrap())
            .unwrap();
    assert_eq!(
        settings["hooks"]["SessionStart"].as_array().unwrap().len(),
        1
    );
    assert!(settings["hooks"].get("UserPromptSubmit").is_none());
    assert!(settings["hooks"].get("PreToolUse").is_none());
    assert!(settings["hooks"].get("PermissionRequest").is_none());
    assert!(settings["hooks"].get("PostToolUse").is_none());
    assert!(settings["hooks"].get("PostToolUseFailure").is_none());
    // 重复安装不叠加活动信号钩子（Windows 上不装）。
    for event in [
        "SubagentStart",
        "SubagentStop",
        "TaskCreated",
        "TaskCompleted",
    ] {
        assert!(
            settings["hooks"][event].as_array().map_or(0, Vec::len) <= 1,
            "{event}"
        );
    }
    assert!(settings["hooks"].get("Stop").is_none());
    assert!(settings["hooks"].get("SessionEnd").is_none());

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_claude_removes_deprecated_completion_hooks_and_preserves_user_hooks() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let claude_dir = home.join(".claude");
    let hooks_dir = claude_dir.join("hooks");
    fs::create_dir_all(&hooks_dir).unwrap();
    let hook_path = hooks_dir.join(CLAUDE_HOOK_INSTALL_NAME);
    let settings = serde_json::json!({
        "hooks": {
            "PostToolUse": [{
                "matcher": "*",
                "hooks": [
                    {"type": "command", "command": format!("bash '{}' working", hook_path.display()), "timeout": 10},
                    {"type": "command", "command": "echo keep-post", "timeout": 10}
                ]
            }],
            "PostToolUseFailure": [{
                "matcher": "*",
                "hooks": [
                    {"type": "command", "command": format!("bash '{}' working", hook_path.display()), "timeout": 10},
                    {"type": "command", "command": "echo keep-failure", "timeout": 10}
                ]
            }],
            "SubagentStop": [{
                "matcher": "*",
                "hooks": [
                    {"type": "command", "command": format!("bash '{}' working", hook_path.display()), "timeout": 10},
                    {"type": "command", "command": "echo keep-subagent", "timeout": 10}
                ]
            }],
            "SessionEnd": [{
                "matcher": "*",
                "hooks": [
                    {"type": "command", "command": format!("bash '{}' release", hook_path.display()), "timeout": 10},
                    {"type": "command", "command": "echo keep-session-end", "timeout": 10}
                ]
            }]
        }
    });
    fs::write(
        claude_dir.join("settings.json"),
        serde_json::to_string(&settings).unwrap(),
    )
    .unwrap();
    std::env::set_var("HOME", &home);

    install_claude().unwrap();

    let settings: Value =
        serde_json::from_str(&fs::read_to_string(claude_dir.join("settings.json")).unwrap())
            .unwrap();
    assert_eq!(
        settings["hooks"]["PostToolUse"][0]["hooks"][0]["command"],
        "echo keep-post"
    );
    assert_eq!(
        settings["hooks"]["PostToolUseFailure"][0]["hooks"][0]["command"],
        "echo keep-failure"
    );
    assert_eq!(
        settings["hooks"]["SubagentStop"][0]["hooks"][0]["command"],
        "echo keep-subagent"
    );
    assert_eq!(
        settings["hooks"]["SessionEnd"][0]["hooks"][0]["command"],
        "echo keep-session-end"
    );
    assert!(settings["hooks"].get("UserPromptSubmit").is_none());
    assert!(settings["hooks"].get("PreToolUse").is_none());
    assert!(settings["hooks"].get("Stop").is_none());

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn claude_v9_integration_status_is_outdated_until_reinstalled() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let claude_hooks_dir = home.join(".claude").join("hooks");
    fs::create_dir_all(&claude_hooks_dir).unwrap();
    let hook_path = claude_hooks_dir.join(CLAUDE_HOOK_INSTALL_NAME);
    fs::write(
        &hook_path,
        "#!/bin/sh\n# HERDR_INTEGRATION_ID=claude\n# HERDR_INTEGRATION_VERSION=9\n",
    )
    .unwrap();
    std::env::set_var("HOME", &home);

    let statuses = installed_integration_statuses();
    let claude = statuses
        .iter()
        .find(|status| status.target == crate::api::schema::IntegrationTarget::Claude)
        .unwrap();

    assert_eq!(claude.path, hook_path);
    assert_eq!(claude.installed_version, Some(9));
    assert_eq!(claude.expected_version, CLAUDE_INTEGRATION_VERSION);
    assert_eq!(claude.state, IntegrationStatusKind::Outdated);

    install_claude().unwrap();
    let status = integration_status_at(
        crate::api::schema::IntegrationTarget::Claude,
        hook_path,
        CLAUDE_INTEGRATION_VERSION,
    );
    assert_eq!(status.installed_version, Some(CLAUDE_INTEGRATION_VERSION));
    assert_eq!(status.state, IntegrationStatusKind::Current);

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn claude_v2_integration_status_is_outdated() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let claude_hooks_dir = home.join(".claude").join("hooks");
    fs::create_dir_all(&claude_hooks_dir).unwrap();
    let hook_path = claude_hooks_dir.join(CLAUDE_HOOK_INSTALL_NAME);
    fs::write(
        &hook_path,
        "#!/bin/sh\n# HERDR_INTEGRATION_ID=claude\n# HERDR_INTEGRATION_VERSION=2\n",
    )
    .unwrap();
    std::env::set_var("HOME", &home);

    let statuses = installed_integration_statuses();
    let claude = statuses
        .iter()
        .find(|status| status.target == crate::api::schema::IntegrationTarget::Claude)
        .unwrap();

    assert_eq!(claude.path, hook_path);
    assert_eq!(claude.installed_version, Some(2));
    assert_eq!(claude.expected_version, CLAUDE_INTEGRATION_VERSION);
    assert_eq!(claude.state, IntegrationStatusKind::Outdated);

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn uninstall_claude_removes_herdr_hooks_and_preserves_others() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let claude_dir = home.join(".claude");
    let hooks_dir = claude_dir.join("hooks");
    fs::create_dir_all(&hooks_dir).unwrap();
    let hook_path = hooks_dir.join(CLAUDE_HOOK_INSTALL_NAME);
    fs::write(&hook_path, CLAUDE_HOOK_ASSET).unwrap();
    let settings = serde_json::json!({
        "hooks": {
            "SessionStart": [{
                "matcher": "*",
                "hooks": [{"type": "command", "command": format!("bash '{}' idle", hook_path.display()), "timeout": 10}]
            }],
            "UserPromptSubmit": [{
                "matcher": "*",
                "hooks": [
                    {"type": "command", "command": format!("bash '{}' working", hook_path.display()), "timeout": 10},
                    {"type": "command", "command": "echo keep", "timeout": 10}
                ]
            }],
            "PermissionRequest": [{
                "matcher": "*",
                "hooks": [{"type": "command", "command": format!("bash '{}' blocked", hook_path.display()), "timeout": 10}]
            }],
            "PostToolUse": [{
                "matcher": "*",
                "hooks": [{"type": "command", "command": format!("bash '{}' working", hook_path.display()), "timeout": 10}]
            }],
            "PostToolUseFailure": [{
                "matcher": "*",
                "hooks": [{"type": "command", "command": format!("bash '{}' working", hook_path.display()), "timeout": 10}]
            }],
            "SubagentStop": [{
                "matcher": "*",
                "hooks": [{"type": "command", "command": format!("bash '{}' working", hook_path.display()), "timeout": 10}]
            }],
            "Stop": [{
                "matcher": "*",
                "hooks": [{"type": "command", "command": format!("bash '{}' idle", hook_path.display()), "timeout": 10}]
            }],
            "SessionEnd": [{
                "matcher": "*",
                "hooks": [{"type": "command", "command": format!("bash '{}' release", hook_path.display()), "timeout": 10}]
            }]
        }
    });
    fs::write(
        claude_dir.join("settings.json"),
        serde_json::to_string(&settings).unwrap(),
    )
    .unwrap();
    std::env::set_var("HOME", &home);

    let result = uninstall_claude().unwrap();
    let settings: Value =
        serde_json::from_str(&fs::read_to_string(claude_dir.join("settings.json")).unwrap())
            .unwrap();

    assert!(result.removed_hook_file);
    assert!(result.updated_settings);
    assert!(!result.hook_path.exists());
    assert_eq!(
        settings["hooks"]["UserPromptSubmit"][0]["hooks"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        settings["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"],
        "echo keep"
    );
    assert!(settings["hooks"].get("PermissionRequest").is_none());
    assert!(settings["hooks"].get("SessionStart").is_none());
    assert!(settings["hooks"].get("PostToolUse").is_none());
    assert!(settings["hooks"].get("PostToolUseFailure").is_none());
    assert!(settings["hooks"].get("SubagentStop").is_none());
    assert!(settings["hooks"].get("Stop").is_none());
    assert!(settings["hooks"].get("SessionEnd").is_none());

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_claude_errors_when_claude_dir_missing() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    fs::create_dir_all(&home).unwrap();
    std::env::set_var("HOME", &home);

    let err = install_claude().unwrap_err().to_string();

    assert!(err.contains("claude directory not found"));

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn codex_v2_integration_status_is_outdated() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let codex_dir = home.join(".codex");
    fs::create_dir_all(&codex_dir).unwrap();
    let hook_path = codex_dir.join(CODEX_HOOK_INSTALL_NAME);
    fs::write(
        &hook_path,
        "#!/bin/sh\n# HERDR_INTEGRATION_ID=codex\n# HERDR_INTEGRATION_VERSION=2\n",
    )
    .unwrap();
    std::env::set_var("HOME", &home);

    let statuses = installed_integration_statuses();
    let codex = statuses
        .iter()
        .find(|status| status.target == crate::api::schema::IntegrationTarget::Codex)
        .unwrap();

    assert_eq!(codex.path, hook_path);
    assert_eq!(codex.installed_version, Some(2));
    assert_eq!(codex.expected_version, CODEX_INTEGRATION_VERSION);
    assert_eq!(codex.state, IntegrationStatusKind::Outdated);

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_codex_writes_hook_and_updates_hooks_and_config() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let codex_dir = home.join(".codex");
    fs::create_dir_all(&codex_dir).unwrap();
    fs::write(codex_dir.join("config.toml"), "model = \"gpt-5.4\"\n").unwrap();
    std::env::set_var("HOME", &home);

    let installed = install_codex().unwrap();
    let hook_content = fs::read_to_string(&installed.hook_path).unwrap();
    let hooks: Value =
        serde_json::from_str(&fs::read_to_string(&installed.hooks_path).unwrap()).unwrap();
    let config = fs::read_to_string(&installed.config_path).unwrap();

    assert_eq!(installed.hook_path, codex_dir.join(CODEX_HOOK_INSTALL_NAME));
    assert_eq!(installed.hooks_path, codex_dir.join("hooks.json"));
    assert_eq!(installed.config_path, codex_dir.join("config.toml"));
    assert_eq!(hook_content, CODEX_HOOK_ASSET);
    assert!(hooks["hooks"]["SessionStart"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap()
        .contains(" session"));
    // 活动钩子只在 Unix 安装（Windows 资产没有活动信号通道），且不带 matcher。
    for event in ["SubagentStart", "SubagentStop"] {
        if cfg!(windows) {
            assert!(hooks["hooks"].get(event).is_none(), "{event}");
        } else {
            assert!(
                hooks["hooks"][event][0]["hooks"][0]["command"]
                    .as_str()
                    .unwrap()
                    .contains(" activity"),
                "{event}"
            );
            assert!(hooks["hooks"][event][0].get("matcher").is_none(), "{event}");
        }
    }
    assert!(hooks["hooks"].get("UserPromptSubmit").is_none());
    assert!(hooks["hooks"].get("PreToolUse").is_none());
    assert!(hooks["hooks"].get("PermissionRequest").is_none());
    assert!(hooks["hooks"].get("Stop").is_none());
    assert!(config.contains("model = \"gpt-5.4\""));
    assert!(config.contains("[features]"));
    assert!(config.contains("hooks = true"));
    assert!(!config.contains("codex_hooks"));

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_codex_uses_codex_home_env() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let codex_dir = base.join("custom-codex");
    fs::create_dir_all(&codex_dir).unwrap();
    fs::write(codex_dir.join("config.toml"), "model = \"gpt-5.4\"\n").unwrap();
    std::env::set_var(CODEX_HOME_ENV_VAR, &codex_dir);

    let installed = install_codex().unwrap();

    assert_eq!(installed.hook_path, codex_dir.join(CODEX_HOOK_INSTALL_NAME));
    assert_eq!(installed.hooks_path, codex_dir.join("hooks.json"));
    assert_eq!(installed.config_path, codex_dir.join("config.toml"));

    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_codex_is_idempotent_for_hook_entries_and_feature_flag() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let codex_dir = home.join(".codex");
    fs::create_dir_all(&codex_dir).unwrap();
    fs::write(
        codex_dir.join("config.toml"),
        "[features]\ncodex_hooks = false\nother = true\n",
    )
    .unwrap();
    std::env::set_var("HOME", &home);

    install_codex().unwrap();
    install_codex().unwrap();

    let hooks: Value =
        serde_json::from_str(&fs::read_to_string(codex_dir.join("hooks.json")).unwrap()).unwrap();
    let config = fs::read_to_string(codex_dir.join("config.toml")).unwrap();

    assert_eq!(hooks["hooks"]["SessionStart"].as_array().unwrap().len(), 1);
    if !cfg!(windows) {
        assert_eq!(hooks["hooks"]["SubagentStart"].as_array().unwrap().len(), 1);
        assert_eq!(hooks["hooks"]["SubagentStop"].as_array().unwrap().len(), 1);
    }
    assert!(hooks["hooks"].get("UserPromptSubmit").is_none());
    assert!(hooks["hooks"].get("PreToolUse").is_none());
    assert!(hooks["hooks"].get("PermissionRequest").is_none());
    assert!(hooks["hooks"].get("Stop").is_none());
    assert_eq!(config.matches("hooks = true").count(), 1);
    assert!(!config.contains("codex_hooks"));
    assert!(config.contains("other = true"));

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_codex_only_migrates_top_level_feature_flags() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let codex_dir = home.join(".codex");
    fs::create_dir_all(&codex_dir).unwrap();
    fs::write(
            codex_dir.join("config.toml"),
            "profile = \"work\"\n\n[profiles.work.features]\nhooks = false\ncodex_hooks = false\n\n[features]\ncodex_hooks = true\nother = true\n",
        )
        .unwrap();
    std::env::set_var("HOME", &home);

    install_codex().unwrap();

    let config = fs::read_to_string(codex_dir.join("config.toml")).unwrap();

    assert!(config.contains("[profiles.work.features]\nhooks = false\ncodex_hooks = false"));
    assert!(config.contains("[features]\nhooks = true\nother = true"));

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn uninstall_codex_removes_herdr_hooks_and_leaves_config_alone() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let codex_dir = home.join(".codex");
    fs::create_dir_all(&codex_dir).unwrap();
    let hook_path = codex_dir.join(CODEX_HOOK_INSTALL_NAME);
    fs::write(&hook_path, CODEX_HOOK_ASSET).unwrap();
    let hooks = serde_json::json!({
        "hooks": {
            "SessionStart": [{"hooks": [{"type": "command", "command": format!("bash '{}' idle", hook_path.display()), "timeout": 10}]}],
            "UserPromptSubmit": [{"hooks": [
                {"type": "command", "command": format!("bash '{}' working", hook_path.display()), "timeout": 10},
                {"type": "command", "command": "echo keep", "timeout": 10}
            ]}],
            "PreToolUse": [{"hooks": [{"type": "command", "command": format!("bash '{}' working", hook_path.display()), "timeout": 10}]}],
            "PermissionRequest": [{"hooks": [{"type": "command", "command": format!("bash '{}' blocked", hook_path.display()), "timeout": 10}]}],
            "Stop": [{"hooks": [{"type": "command", "command": format!("bash '{}' idle", hook_path.display()), "timeout": 10}]}],
            "SubagentStart": [{"hooks": [{"type": "command", "command": format!("bash '{}' activity", hook_path.display()), "timeout": 10}]}],
            "SubagentStop": [{"hooks": [
                {"type": "command", "command": format!("bash '{}' activity", hook_path.display()), "timeout": 10},
                {"type": "command", "command": "echo keep-stop", "timeout": 10}
            ]}]
        }
    });
    fs::write(
        codex_dir.join("hooks.json"),
        serde_json::to_string(&hooks).unwrap(),
    )
    .unwrap();
    fs::write(
        codex_dir.join("config.toml"),
        "[features]\nhooks = true\nother = true\n",
    )
    .unwrap();
    std::env::set_var("HOME", &home);

    let result = uninstall_codex().unwrap();
    let hooks: Value =
        serde_json::from_str(&fs::read_to_string(codex_dir.join("hooks.json")).unwrap()).unwrap();
    let config = fs::read_to_string(codex_dir.join("config.toml")).unwrap();

    assert!(result.removed_hook_file);
    assert!(result.updated_hooks);
    assert!(!result.hook_path.exists());
    assert!(hooks["hooks"].get("SessionStart").is_none());
    assert!(hooks["hooks"].get("PreToolUse").is_none());
    assert!(hooks["hooks"].get("PermissionRequest").is_none());
    assert!(hooks["hooks"].get("Stop").is_none());
    assert!(hooks["hooks"].get("SubagentStart").is_none());
    assert_eq!(
        hooks["hooks"]["SubagentStop"][0]["hooks"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        hooks["hooks"]["SubagentStop"][0]["hooks"][0]["command"],
        "echo keep-stop"
    );
    assert_eq!(
        hooks["hooks"]["UserPromptSubmit"][0]["hooks"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        hooks["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"],
        "echo keep"
    );
    assert!(config.contains("hooks = true"));
    assert!(config.contains("other = true"));

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_codex_errors_when_config_dir_missing() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    fs::create_dir_all(&home).unwrap();
    std::env::set_var("HOME", &home);

    let err = install_codex().unwrap_err().to_string();

    assert!(err.contains("codex config directory not found"));

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

/// 冒烟 M5①：codex 只运行用户信任过的钩子。装完 herdr 的钩子后首启会弹
/// 「Hooks need review」，选「Continue without trusting」钩子就静默失效；此前安装
/// 输出与状态都不提这件事。
#[test]
fn install_codex_tells_the_user_to_trust_the_hooks_until_codex_does() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let codex_dir = home.join(".codex");
    fs::create_dir_all(&codex_dir).unwrap();
    fs::write(codex_dir.join("config.toml"), "model = \"gpt-5.4\"\n").unwrap();
    std::env::set_var("HOME", &home);

    let codex_status = || {
        installed_integration_statuses()
            .into_iter()
            .find(|status| status.target == crate::api::schema::IntegrationTarget::Codex)
            .unwrap()
    };

    let messages = install_target(crate::api::schema::IntegrationTarget::Codex).unwrap();
    assert!(
        messages
            .iter()
            .any(|message| message.contains("Hooks need review")
                && message.contains("Trust all and continue")),
        "{messages:?}"
    );
    // 两条提示按「先动作、后后果」相邻输出。
    let hints = super::actions::CODEX_HOOKS_REVIEW_HINTS.map(str::to_owned);
    assert!(
        messages.windows(hints.len()).any(|window| window == hints),
        "{messages:?}"
    );
    assert_eq!(
        codex_status().note,
        Some(IntegrationStatusNote::CodexHooksNeedReview)
    );

    // 用户在 codex 里选了「Trust all and continue」：codex 按「hooks.json 路径:事件:
    // 组序号:钩子序号」写下各钩子的哈希。重装不再提示，状态也不再带提示行。
    let hooks_path = codex_dir.join("hooks.json");
    let hook_path = codex_dir.join(CODEX_HOOK_INSTALL_NAME);
    let mut config = fs::read_to_string(codex_dir.join("config.toml")).unwrap();
    for (event, command) in codex_managed_hooks(&hook_path) {
        let label = match event {
            "SessionStart" => "session_start",
            "SubagentStart" => "subagent_start",
            _ => "subagent_stop",
        };
        let hash = super::codex_trust::codex_hook_trust_hash(event, &command, 10).unwrap();
        config.push_str(&format!(
            "\n[hooks.state.{}]\ntrusted_hash = \"{hash}\"\n",
            super::config_edit::toml_basic_string(&format!("{}:{label}:0:0", hooks_path.display()))
        ));
    }
    fs::write(codex_dir.join("config.toml"), &config).unwrap();

    let messages = install_target(crate::api::schema::IntegrationTarget::Codex).unwrap();
    assert!(
        !messages
            .iter()
            .any(|message| message.contains("Hooks need review")),
        "{messages:?}"
    );
    assert_eq!(codex_status().note, None);

    // 用户随后在 codex 的 /hooks 里停用了它们。
    fs::write(
        codex_dir.join("config.toml"),
        config.replace("trusted_hash", "enabled = false\ntrusted_hash"),
    )
    .unwrap();
    assert_eq!(
        codex_status().note,
        Some(IntegrationStatusNote::CodexHooksDisabled)
    );

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

/// 冒烟 M5① 的对抗审查（中）：设置页集成页把安装消息逐条单行显示、按宽度硬截断
/// （80 列终端下消息区只有 74 列），原先 221 列的单行提示在那里看不到该选哪一项。
/// 每条信任提示不超过 70 列，要做的选择写在第一条。
#[test]
fn codex_hook_trust_hints_fit_the_settings_message_column() {
    use unicode_width::UnicodeWidthStr;

    let [review, consequence] = super::actions::CODEX_HOOKS_REVIEW_HINTS;
    for hint in [
        review,
        consequence,
        super::actions::CODEX_HOOKS_DISABLED_HINT,
    ] {
        assert!(UnicodeWidthStr::width(hint) <= 70, "{hint:?} 超过 70 列");
    }
    assert!(
        review.contains("Hooks need review") && review.contains("Trust all and continue"),
        "{review}"
    );
    assert!(
        consequence.contains("Continue without trusting"),
        "{consequence}"
    );
}

/// 更新提示（`herdr update` 之后、`status --outdated-only`）只看版本：信任提示行只在
/// `herdr integration status` 里算，不为它在每次更新后去读 codex 的配置。
#[test]
fn outdated_notice_leaves_codex_hook_trust_to_the_status_command() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let codex_dir = home.join(".codex");
    fs::create_dir_all(&codex_dir).unwrap();
    std::env::set_var("HOME", &home);
    install_codex().unwrap();
    fs::write(
        codex_dir.join(CODEX_HOOK_INSTALL_NAME),
        "#!/bin/sh\n# HERDR_INTEGRATION_ID=codex\n# HERDR_INTEGRATION_VERSION=2\n",
    )
    .unwrap();
    let is_codex =
        |status: &IntegrationStatus| status.target == crate::api::schema::IntegrationTarget::Codex;

    let outdated = outdated_installed_integrations();
    let codex = outdated.iter().find(|status| is_codex(status)).unwrap();
    assert_eq!(codex.state, IntegrationStatusKind::Outdated);
    assert_eq!(codex.note, None);

    let statuses = installed_integration_statuses();
    let codex = statuses.iter().find(|status| is_codex(status)).unwrap();
    assert_eq!(
        codex.note,
        Some(IntegrationStatusNote::CodexHooksNeedReview)
    );

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_kimi_writes_hook_and_updates_config() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let kimi_dir = home.join(".kimi-code");
    fs::create_dir_all(&kimi_dir).unwrap();
    fs::write(
            kimi_dir.join("config.toml"),
            "default_model = \"moonshot\"\n\n[[hooks]]\nevent = \"Notification\"\nmatcher = \"task.completed\"\ncommand = \"echo keep\"\ntimeout = 3\n",
        )
        .unwrap();
    std::env::set_var("HOME", &home);

    let installed = install_kimi().unwrap();
    let hook_content = fs::read_to_string(&installed.hook_path).unwrap();
    let config = fs::read_to_string(&installed.config_path).unwrap();
    let hooks = kimi_config_hooks(&config);

    assert_eq!(
        installed.hook_path,
        kimi_dir.join("hooks").join(KIMI_HOOK_INSTALL_NAME)
    );
    assert_eq!(installed.config_path, kimi_dir.join("config.toml"));
    assert_eq!(hook_content, KIMI_HOOK_ASSET);
    assert_eq!(hooks.len(), KIMI_HOOK_EVENTS.len() + 1);
    assert!(config.contains("default_model = \"moonshot\""));
    assert!(config.contains("command = \"echo keep\""));
    assert!(config.contains(KIMI_CONFIG_BLOCK_BEGIN));
    assert!(config.contains(KIMI_CONFIG_BLOCK_END));
    for (event, matcher, action) in KIMI_HOOK_EVENTS {
        assert_kimi_hook(&config, &installed.hook_path, event, matcher, action);
    }

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn kimi_question_hooks_report_blocked_until_the_question_finishes() {
    assert!(KIMI_HOOK_EVENTS.contains(&(
        "PreToolUse",
        Some(KIMI_ASK_USER_QUESTION_MATCHER),
        "blocked",
    )));
    assert!(KIMI_HOOK_EVENTS.contains(&(
        "PostToolUse",
        Some(KIMI_ASK_USER_QUESTION_MATCHER),
        "working",
    )));
    assert!(KIMI_HOOK_EVENTS.contains(&(
        "PostToolUseFailure",
        Some(KIMI_ASK_USER_QUESTION_MATCHER),
        "working",
    )));
    assert!(KIMI_HOOK_EVENTS.contains(&("PreToolUse", Some(KIMI_OTHER_TOOL_MATCHER), "working",)));
}

/// 冒烟 M3：Kimi 的回合有三种收尾，各自只发一个事件——正常结束发 `Stop`、用户打断发
/// `Interrupt`、回合出错（真机里是 provider 401）发 `StopFailure`。钩子是 kimi 的完整
/// 生命周期权威，屏幕检测不再兜底，漏掉任何一种都会让 pane 停在 working 直到退出。
#[test]
fn kimi_every_turn_ending_event_reports_idle() {
    for event in ["Stop", "Interrupt", "StopFailure"] {
        assert!(
            KIMI_HOOK_EVENTS.contains(&(event, None, "idle")),
            "kimi turn-ending event {event} must report idle"
        );
    }
}

#[test]
fn install_kimi_uses_kimi_code_home_env() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let kimi_dir = base.join("custom-kimi");
    fs::create_dir_all(&kimi_dir).unwrap();
    std::env::set_var(KIMI_CODE_HOME_ENV_VAR, &kimi_dir);

    let installed = install_kimi().unwrap();

    assert_eq!(
        installed.hook_path,
        kimi_dir.join("hooks").join(KIMI_HOOK_INSTALL_NAME)
    );
    assert_eq!(installed.config_path, kimi_dir.join("config.toml"));

    clear_integration_path_env();
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_kimi_is_idempotent_for_config_block() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let kimi_dir = home.join(".kimi-code");
    fs::create_dir_all(&kimi_dir).unwrap();
    std::env::set_var("HOME", &home);

    install_kimi().unwrap();
    install_kimi().unwrap();

    let config = fs::read_to_string(kimi_dir.join("config.toml")).unwrap();
    let hooks = kimi_config_hooks(&config);

    assert_eq!(config.matches(KIMI_CONFIG_BLOCK_BEGIN).count(), 1);
    assert_eq!(config.matches(KIMI_CONFIG_BLOCK_END).count(), 1);
    assert_eq!(hooks.len(), KIMI_HOOK_EVENTS.len());

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn uninstall_kimi_removes_hook_and_config_block_preserves_other_hooks() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let kimi_dir = home.join(".kimi-code");
    fs::create_dir_all(&kimi_dir).unwrap();
    std::env::set_var("HOME", &home);

    let installed = install_kimi().unwrap();
    fs::write(
            &installed.config_path,
            format!(
                "default_model = \"moonshot\"\n\n[[hooks]]\nevent = \"Notification\"\ncommand = \"echo keep\"\n\n{}",
                fs::read_to_string(&installed.config_path).unwrap()
            ),
        )
        .unwrap();

    let result = uninstall_kimi().unwrap();
    let config = fs::read_to_string(kimi_dir.join("config.toml")).unwrap();
    let hooks = kimi_config_hooks(&config);

    assert!(result.removed_hook_file);
    assert!(result.updated_config);
    assert!(!result.hook_path.exists());
    assert!(config.contains("default_model = \"moonshot\""));
    assert!(config.contains("command = \"echo keep\""));
    assert!(!config.contains(KIMI_CONFIG_BLOCK_BEGIN));
    assert!(!config.contains(KIMI_CONFIG_BLOCK_END));
    assert_eq!(hooks.len(), 1);
    assert_eq!(
        hooks[0].get("event").and_then(toml::Value::as_str),
        Some("Notification")
    );

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_kimi_errors_when_config_dir_missing() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    fs::create_dir_all(&home).unwrap();
    std::env::set_var("HOME", &home);

    let err = install_kimi().unwrap_err().to_string();

    assert!(err.contains("kimi code config directory not found"));

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_opencode_writes_server_and_tui_plugins() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let opencode_dir = home.join(".config/opencode");
    fs::create_dir_all(&opencode_dir).unwrap();
    std::env::set_var("HOME", &home);

    let installed = install_opencode().unwrap();

    assert_eq!(
        installed.plugin_path,
        opencode_dir
            .join("plugins")
            .join(OPENCODE_PLUGIN_INSTALL_NAME)
    );
    assert_eq!(
        fs::read_to_string(&installed.plugin_path).unwrap(),
        OPENCODE_PLUGIN_ASSET
    );
    assert_eq!(
        installed.tui_plugin_path,
        opencode_dir.join(OPENCODE_TUI_PLUGIN_INSTALL_NAME)
    );
    assert_eq!(
        fs::read_to_string(&installed.tui_plugin_path).unwrap(),
        OPENCODE_TUI_PLUGIN_ASSET
    );
    assert_eq!(installed.tui_config_path, opencode_dir.join("tui.jsonc"));
    let tui_config: Value =
        serde_json::from_str(&fs::read_to_string(&installed.tui_config_path).unwrap()).unwrap();
    assert_eq!(tui_config["plugin"], json!([OPENCODE_TUI_PLUGIN_SPEC]));
    let cli_config_path = installed
        .cli_config_path
        .expect("cli.json should be created when OpenCode has nothing to migrate");
    assert_eq!(cli_config_path, opencode_dir.join("cli.json"));
    let cli_config: Value =
        serde_json::from_str(&fs::read_to_string(&cli_config_path).unwrap()).unwrap();
    assert_eq!(cli_config["plugins"], json!([OPENCODE_V2_TUI_PLUGIN_SPEC]));

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[cfg(unix)]
#[test]
fn opencode_reuses_json_registration_in_symlinked_config_directory() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let dotfiles = base.join("dotfiles");
    let dir = home.join(".config/opencode");
    fs::create_dir_all(home.join(".config")).unwrap();
    fs::create_dir_all(&dotfiles).unwrap();
    std::os::unix::fs::symlink(&dotfiles, &dir).unwrap();
    std::env::set_var("HOME", &home);
    let json_path = dir.join("tui.json");
    let original = "{\n  // User preferences\n  \"theme\":\"system\",\n  \"plugin\":[\"other\",[\"./herdr-tui-session.js\",{\"enabled\":true}]]\n}\n";
    fs::write(&json_path, original).unwrap();

    for _ in 0..2 {
        let installed = install_opencode().unwrap();
        assert_eq!(installed.tui_config_path, json_path);
        assert!(installed.cli_config_path.is_none());
        assert!(!dir.join("tui.jsonc").exists());
        assert_eq!(fs::read_to_string(&json_path).unwrap(), original);
        assert_eq!(
            integration_status_at(
                crate::api::schema::IntegrationTarget::Opencode,
                installed.plugin_path,
                OPENCODE_INTEGRATION_VERSION,
            )
            .state,
            IntegrationStatusKind::Current
        );
    }

    // Older installs may have registered the same plugin in both files.
    let jsonc_path = dir.join("tui.jsonc");
    fs::write(
        &jsonc_path,
        r#"{"plugin":["./herdr-tui-session.js","another"]}"#,
    )
    .unwrap();
    assert_eq!(install_opencode().unwrap().tui_config_path, jsonc_path);
    let result = uninstall_opencode().unwrap();
    assert_eq!(
        result.updated_tui_configs,
        vec![jsonc_path.clone(), json_path.clone()]
    );
    let json = fs::read_to_string(&json_path).unwrap();
    assert!(json.contains("// User preferences"));
    assert!(!json.contains("herdr-tui-session.js"));
    assert!(json.contains("\"other\""));
    assert!(json.contains("\"system\""));
    assert_eq!(
        serde_json::from_str::<Value>(&fs::read_to_string(jsonc_path).unwrap()).unwrap(),
        json!({"plugin":["another"]})
    );
    assert!(uninstall_opencode().unwrap().updated_tui_configs.is_empty());
    assert_eq!(fs::read_link(&dir).unwrap(), dotfiles);
    assert!(!result.plugin_path.exists());
    assert!(!result.tui_plugin_path.exists());
    std::env::remove_var("HOME");
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn opencode_install_defers_v2_registration_while_migration_pending() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let opencode_dir = home.join(".config/opencode");
    fs::create_dir_all(&opencode_dir).unwrap();
    fs::write(opencode_dir.join("tui.json"), "{}").unwrap();
    std::env::set_var("HOME", &home);

    let installed = install_opencode().unwrap();

    assert!(installed.cli_config_path.is_none());
    assert!(!opencode_dir.join("cli.json").exists());
    assert!(opencode_dir
        .join(OPENCODE_V2_TUI_PLUGIN_DIR)
        .join("tui.js")
        .is_file());

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn opencode_v2_install_status_and_uninstall_preserve_cli_preferences() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let dir = home.join(".config/opencode");
    fs::create_dir_all(&dir).unwrap();
    std::env::set_var("HOME", &home);
    let cli = dir.join("cli.json");
    fs::write(
        &cli,
        r#"{"theme":{"name":"catppuccin"},"plugins":["other"]}"#,
    )
    .unwrap();
    let installed = install_opencode().unwrap();
    assert_eq!(installed.cli_config_path, Some(cli.clone()));
    let status = || {
        integration_status_at(
            crate::api::schema::IntegrationTarget::Opencode,
            installed.plugin_path.clone(),
            OPENCODE_INTEGRATION_VERSION,
        )
        .state
    };
    assert_eq!(status(), IntegrationStatusKind::Current);
    let entry = dir.join(OPENCODE_V2_TUI_PLUGIN_DIR).join("tui.js");
    assert_eq!(
        fs::read_to_string(&entry).unwrap(),
        OPENCODE_V2_TUI_PLUGIN_ASSET
    );
    fs::remove_file(&entry).unwrap();
    assert_eq!(status(), IntegrationStatusKind::Outdated);
    install_opencode().unwrap();
    super::opencode_config::remove_cli_plugin(&dir, OPENCODE_V2_TUI_PLUGIN_SPEC).unwrap();
    assert_eq!(status(), IntegrationStatusKind::Outdated);
    install_opencode().unwrap();
    uninstall_opencode().unwrap();
    assert!(!entry.exists());
    assert_eq!(
        serde_json::from_str::<Value>(&fs::read_to_string(cli).unwrap()).unwrap(),
        json!({"theme":{"name":"catppuccin"},"plugins":["other"]})
    );
    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn opencode_hard_link_rejection_precedes_install_and_uninstall_asset_changes() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let dir = home.join(".config/opencode");
    fs::create_dir_all(dir.join("plugins")).unwrap();
    std::env::set_var("HOME", &home);
    let plugin = dir.join("plugins").join(OPENCODE_PLUGIN_INSTALL_NAME);
    fs::write(&plugin, "previous integration").unwrap();
    let config = dir.join("cli.json");
    let original = r#"{"plugins":["./herdr-opencode"],"theme":"system"}"#;
    fs::write(&config, original).unwrap();
    let alias = base.join("linked-config");
    fs::hard_link(&config, &alias).unwrap();
    let target = crate::api::schema::IntegrationTarget::Opencode;
    for error in [
        install_target(target).unwrap_err(),
        uninstall_target(target).unwrap_err(),
    ] {
        assert!(error.to_string().contains("multiple hard links"));
        assert!(error.to_string().contains("cli.json"));
    }
    assert_eq!(fs::read_to_string(&plugin).unwrap(), "previous integration");
    assert_eq!(fs::read_to_string(&alias).unwrap(), original);
    assert_eq!(crate::platform::config_file_link_count(&config).unwrap(), 2);
    assert!(!dir.join("tui.jsonc").exists());
    assert!(!dir.join(OPENCODE_V2_TUI_PLUGIN_DIR).exists());
    std::env::remove_var("HOME");
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn opencode_json_config_validation_precedes_asset_changes() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let dir = home.join(".config/opencode");
    fs::create_dir_all(dir.join("plugins")).unwrap();
    std::env::set_var("HOME", &home);
    let plugin = dir.join("plugins").join(OPENCODE_PLUGIN_INSTALL_NAME);
    fs::write(&plugin, "previous integration").unwrap();
    let config = dir.join("tui.json");
    fs::write(&config, r#"{"plugin":{}}"#).unwrap();
    assert!(install_opencode()
        .unwrap_err()
        .to_string()
        .contains("plugin list"));
    assert_eq!(fs::read_to_string(&plugin).unwrap(), "previous integration");
    let original = r#"{"plugin":["./herdr-tui-session.js"]}"#;
    fs::write(&config, original).unwrap();
    let alias = base.join("linked-config");
    fs::hard_link(&config, &alias).unwrap();
    for error in [
        install_opencode().unwrap_err(),
        uninstall_opencode().unwrap_err(),
    ] {
        assert!(error.to_string().contains("multiple hard links"));
        assert!(error.to_string().contains("tui.json"));
    }
    assert_eq!(fs::read_to_string(&plugin).unwrap(), "previous integration");
    assert_eq!(fs::read_to_string(&alias).unwrap(), original);
    assert!(!dir.join("tui.jsonc").exists());
    assert!(!dir.join(OPENCODE_TUI_PLUGIN_INSTALL_NAME).exists());
    std::env::remove_var("HOME");
    fs::remove_dir_all(base).unwrap();
}

#[cfg(windows)]
#[test]
fn opencode_recovery_copy_blocks_retry_before_parsing_or_asset_changes() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let dir = home.join(".config/opencode");
    fs::create_dir_all(dir.join("plugins")).unwrap();
    std::env::set_var("HOME", &home);
    let plugin = dir.join("plugins").join(OPENCODE_PLUGIN_INSTALL_NAME);
    fs::write(&plugin, "previous integration").unwrap();
    let config = dir.join("cli.json");
    let backup = dir.join("cli.json.herdr-backup");
    let original = r#"{"plugins":["./herdr-opencode"],"theme":"system"}"#;
    fs::write(&backup, original).unwrap();
    let target = crate::api::schema::IntegrationTarget::Opencode;
    for contents in [Some("{"), Some(original), None] {
        if let Some(contents) = contents {
            fs::write(&config, contents).unwrap();
        } else {
            fs::remove_file(&config).unwrap();
        }
        for error in [
            install_target(target).unwrap_err(),
            uninstall_target(target).unwrap_err(),
        ] {
            assert!(error.to_string().contains("recovery copy"), "{error}");
            assert!(error.to_string().contains("cli.json.herdr-backup"));
        }
        assert_eq!(fs::read_to_string(&backup).unwrap(), original);
        if contents.is_none() {
            assert!(!config.exists());
        }
    }
    // A symlink invocation must discover the referent's recovery copy too.
    let referent = base.join("preferences.json");
    fs::write(&referent, "{").unwrap();
    let linked_backup = base.join("preferences.json.herdr-backup");
    fs::rename(&backup, &linked_backup).unwrap();
    if symlink_file(&referent, &config) {
        let link_before = fs::read_link(&config).unwrap();
        let error = install_target(target).unwrap_err();
        assert!(error.to_string().contains("preferences.json.herdr-backup"));
        assert_eq!(fs::read_link(&config).unwrap(), link_before);
    }
    assert_eq!(fs::read_to_string(&plugin).unwrap(), "previous integration");
    assert!(!dir.join("tui.jsonc").exists());
    assert!(!dir.join(OPENCODE_V2_TUI_PLUGIN_DIR).exists());
    std::env::remove_var("HOME");
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn opencode_invalid_cli_config_does_not_overwrite_existing_plugins() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let dir = home.join(".config/opencode");
    fs::create_dir_all(dir.join("plugins")).unwrap();
    std::env::set_var("HOME", &home);
    let plugin = dir.join("plugins").join(OPENCODE_PLUGIN_INSTALL_NAME);
    fs::write(&plugin, "previous integration").unwrap();
    fs::write(dir.join("cli.json"), r#"{"plugins":{}}"#).unwrap();
    assert!(install_opencode().is_err());
    assert_eq!(fs::read_to_string(plugin).unwrap(), "previous integration");
    assert!(!dir.join("tui.jsonc").exists());
    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn opencode_status_requires_the_tui_plugin_and_config_entry() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let opencode_dir = home.join(".config/opencode");
    fs::create_dir_all(&opencode_dir).unwrap();
    std::env::set_var("HOME", &home);
    let installed = install_opencode().unwrap();
    let status = || {
        integration_status_at(
            crate::api::schema::IntegrationTarget::Opencode,
            installed.plugin_path.clone(),
            OPENCODE_INTEGRATION_VERSION,
        )
        .state
    };

    assert_eq!(status(), IntegrationStatusKind::Current);
    fs::remove_file(&installed.tui_plugin_path).unwrap();
    assert_eq!(status(), IntegrationStatusKind::Outdated);
    fs::write(&installed.tui_plugin_path, OPENCODE_TUI_PLUGIN_ASSET).unwrap();
    super::opencode_config::remove_tui_plugin(&opencode_dir, OPENCODE_TUI_PLUGIN_SPEC).unwrap();
    assert_eq!(status(), IntegrationStatusKind::Outdated);

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn uninstall_opencode_removes_plugins_and_managed_tui_config_entry() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let opencode_dir = home.join(".config/opencode");
    fs::create_dir_all(&opencode_dir).unwrap();
    std::env::set_var("HOME", &home);
    let installed = install_opencode().unwrap();

    let result = uninstall_opencode().unwrap();

    assert!(result.removed_plugin);
    assert!(result.removed_tui_plugin);
    assert_eq!(
        result.updated_tui_configs,
        vec![installed.tui_config_path.clone()]
    );
    assert!(!result.plugin_path.exists());
    assert!(!result.tui_plugin_path.exists());
    assert!(installed.tui_config_path.exists());
    let tui_config: Value =
        serde_json::from_str(&fs::read_to_string(&installed.tui_config_path).unwrap()).unwrap();
    assert_eq!(tui_config, json!({}));
    assert_eq!(installed.plugin_path, result.plugin_path);

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_opencode_invalid_tui_config_does_not_write_plugins() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let opencode_dir = home.join(".config/opencode");
    fs::create_dir_all(&opencode_dir).unwrap();
    fs::write(opencode_dir.join("tui.jsonc"), r#"{"plugin":{}}"#).unwrap();
    std::env::set_var("HOME", &home);

    let err = install_opencode().unwrap_err().to_string();

    assert!(err.contains("plugin list"));
    assert!(!opencode_dir
        .join("plugins")
        .join(OPENCODE_PLUGIN_INSTALL_NAME)
        .exists());
    assert!(!opencode_dir.join(OPENCODE_TUI_PLUGIN_INSTALL_NAME).exists());

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn uninstall_opencode_removes_plugins_when_tui_config_is_invalid() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let opencode_dir = home.join(".config/opencode");
    let plugins_dir = opencode_dir.join("plugins");
    fs::create_dir_all(&plugins_dir).unwrap();
    let plugin_path = plugins_dir.join(OPENCODE_PLUGIN_INSTALL_NAME);
    let tui_plugin_path = opencode_dir.join(OPENCODE_TUI_PLUGIN_INSTALL_NAME);
    fs::write(&plugin_path, OPENCODE_PLUGIN_ASSET).unwrap();
    fs::write(&tui_plugin_path, OPENCODE_TUI_PLUGIN_ASSET).unwrap();
    fs::write(opencode_dir.join("tui.jsonc"), "{\"plugin\":").unwrap();
    let json_path = opencode_dir.join("tui.json");
    fs::write(
        &json_path,
        r#"{"plugin":["./herdr-tui-session.js","other"]}"#,
    )
    .unwrap();
    std::env::set_var("HOME", &home);

    let err = uninstall_opencode().unwrap_err().to_string();

    assert!(err.contains("failed to parse OpenCode TUI config"));
    assert!(!plugin_path.exists());
    assert!(!tui_plugin_path.exists());
    assert_eq!(
        serde_json::from_str::<Value>(&fs::read_to_string(json_path).unwrap()).unwrap(),
        json!({"plugin":["other"]})
    );

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_opencode_errors_when_config_dir_missing() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    fs::create_dir_all(&home).unwrap();
    std::env::set_var("HOME", &home);

    let err = install_opencode().unwrap_err().to_string();

    assert!(err.contains("opencode config directory not found"));

    std::env::remove_var("HOME");
    let _ = fs::remove_dir_all(base);
}

#[test]
fn bundled_integration_asset_versions_match_expected_versions() {
    for (name, asset, expected_version) in [
        ("pi", PI_EXTENSION_ASSET, PI_INTEGRATION_VERSION),
        ("claude", CLAUDE_HOOK_ASSET, CLAUDE_INTEGRATION_VERSION),
        ("codex", CODEX_HOOK_ASSET, CODEX_INTEGRATION_VERSION),
        ("kimi", KIMI_HOOK_ASSET, KIMI_INTEGRATION_VERSION),
        (
            "opencode",
            OPENCODE_PLUGIN_ASSET,
            OPENCODE_INTEGRATION_VERSION,
        ),
    ] {
        assert_eq!(
            parse_integration_version(asset),
            Some(expected_version),
            "{name} asset version must match its integration version constant"
        );
    }
}

#[test]
fn process_owned_integration_assets_do_not_report_release() {
    for (name, asset) in [("pi", PI_EXTENSION_ASSET), ("kimi", KIMI_HOOK_ASSET)] {
        assert!(
            !asset.contains("pane.release_agent"),
            "{name} process exit should own lifecycle release"
        );
    }
}

#[test]
fn pi_extension_refreshes_session_ref_before_agent_start_state() {
    let agent_start = PI_EXTENSION_ASSET
        .find("pi.on(\"agent_start\", (_event, ctx)")
        .expect("pi extension should receive agent_start context");
    let handler = &PI_EXTENSION_ASSET[agent_start..];
    let update_session = handler
        .find("updateSessionRef(ctx);")
        .expect("pi extension should refresh the active session on agent_start");
    let report_session = handler
        .find("void reportSession();")
        .expect("pi extension should report the refreshed session before state");
    let publish_state = handler
        .find("publishState();")
        .expect("pi extension should publish working state after refreshing session");

    assert!(update_session < report_session);
    assert!(report_session < publish_state);
}
