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

use crate::config::test_dirs::{override_home_dir, override_search_path};
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

/// 调用方必须已持有 `integration_env_lock()`：这里会清掉集成路径相关的环境变量。
fn unique_base() -> PathBuf {
    clear_integration_path_env();
    // 时间戳在并发的测试线程间会撞，再带进程内序号。
    std::env::temp_dir().join(format!(
        "herdr-integration-install-test-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        crate::config::test_dirs::unique_id()
    ))
}

#[cfg(windows)]
#[test]
fn home_dir_uses_userprofile_when_home_is_missing() {
    // 用假的环境验证回退顺序：删掉进程级 HOME 会被同进程并发的测试看到。
    let profile = std::ffi::OsString::from(r"C:\Users\herdr-profile");
    let env = |key: &str| (key == "USERPROFILE").then(|| profile.clone());
    assert_eq!(home_dir_from_env(env).unwrap(), PathBuf::from(&profile));

    let with_home = |key: &str| match key {
        "HOME" => Some(std::ffi::OsString::from(r"C:\herdr-home")),
        "USERPROFILE" => Some(profile.clone()),
        _ => None,
    };
    assert_eq!(
        home_dir_from_env(with_home).unwrap(),
        PathBuf::from(r"C:\herdr-home")
    );
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
    let _path = override_search_path(&bin);

    fs::write(bin.join("pi.cmd"), "@echo off\r\n").unwrap();
    fs::write(bin.join("opencode.cmd"), "@echo off\r\n").unwrap();
    fs::write(bin.join("kimi.exe"), "").unwrap();

    assert!(integration_target_available(IntegrationTarget::Pi));
    assert!(integration_target_available(IntegrationTarget::Opencode));
    assert!(integration_target_available(IntegrationTarget::Kimi));

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
    let _home = override_home_dir(&base);
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

    let _ = fs::remove_dir_all(base);
}

#[test]
fn command_availability_search_path_override_is_scoped() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let bin = base.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let executable = bin.join(if cfg!(windows) {
        "herdr-merge-test-agent.cmd"
    } else {
        "herdr-merge-test-agent"
    });
    fs::write(&executable, "").unwrap();
    make_executable(&executable).unwrap();
    let original_path = std::env::var_os("PATH");
    let _path = override_search_path(&bin);
    assert!(command_available("herdr-merge-test-agent"));
    {
        let _empty = override_search_path("");
        assert!(!command_available("herdr-merge-test-agent"));
    }
    assert!(command_available("herdr-merge-test-agent"));
    assert_eq!(std::env::var_os("PATH"), original_path);
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
    let _path = override_search_path(&bin);

    let command = bin.join("claude");
    fs::write(&command, "#!/bin/sh\n").unwrap();
    fs::set_permissions(&command, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(!command_available("claude"));

    fs::set_permissions(&command, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(command_available("claude"));

    let _ = fs::remove_dir_all(base);
}

#[test]
#[cfg(windows)]
fn command_available_finds_windows_command_shims_on_path() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let bin = base.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let _path = override_search_path(&bin);

    fs::write(bin.join("claude.cmd"), "@echo off\r\n").unwrap();
    assert!(command_available("claude"));

    fs::write(bin.join("codex.exe"), "").unwrap();
    assert!(command_available("codex"));

    assert!(!command_available("missing-agent"));

    let _ = fs::remove_dir_all(base);
}

/// A long-running server's PATH misses CLIs installed later, registry-only PATH updates and
/// desktop-bundled CLIs; availability must still see them (fake profile, injected registry
/// PATH, no process PATH).
#[test]
#[cfg(windows)]
fn command_available_searches_windows_install_locations_outside_path() {
    struct Pinned;
    impl Drop for Pinned {
        fn drop(&mut self) {
            crate::platform::set_test_command_search_environment(None);
        }
    }

    let _lock = integration_env_lock();
    let base = unique_base();
    let write = |relative: &str| {
        let path = base.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "").unwrap();
    };
    write(r"registry-bin\kimi.cmd");
    write(r"roaming\npm\opencode.cmd");
    write(r"local\Microsoft\WinGet\Links\pi.exe");
    write(r"local\OpenAI\Codex\bin\faa963e871dd422c\codex.exe");
    write(
        r"local\Packages\Claude_pzs8sxrjxfjjc\LocalCache\Roaming\Claude\claude-code\2.1.284\claude.exe",
    );
    fs::create_dir_all(base.join("profile")).unwrap();

    let environment = crate::platform::CommandSearchEnvironment {
        registry_path: vec![base.join("registry-bin").into_os_string()],
        user_profile: Some(base.join("profile")),
        app_data: Some(base.join("roaming")),
        local_app_data: Some(base.join("local")),
        ..Default::default()
    };
    crate::platform::set_test_command_search_environment(Some(environment));
    let _pinned = Pinned;

    for command in ["kimi", "opencode", "pi", "codex", "claude"] {
        assert!(command_available(command), "{command}");
    }
    for target in [
        crate::api::schema::IntegrationTarget::Claude,
        crate::api::schema::IntegrationTarget::Codex,
        crate::api::schema::IntegrationTarget::Kimi,
        crate::api::schema::IntegrationTarget::Opencode,
        crate::api::schema::IntegrationTarget::Pi,
    ] {
        assert!(integration_target_available(target), "{target:?}");
    }
    assert!(!command_available("missing-agent"));

    crate::platform::set_test_command_search_environment(Some(
        crate::platform::CommandSearchEnvironment::default(),
    ));
    assert!(
        !command_available("kimi"),
        "nothing is found without a PATH or install locations"
    );
    let _ = fs::remove_dir_all(base);
}

/// An extensionless shell shim (npm and Unix-style installers write one) shows the CLI is
/// installed when no launchable file sits next to it; it only counts after every launchable
/// candidate in every search directory, and an explicit extension has no such fallback.
#[test]
#[cfg(windows)]
fn command_available_counts_extensionless_windows_shims_as_installed() {
    struct Pinned;
    impl Drop for Pinned {
        fn drop(&mut self) {
            crate::platform::set_test_command_search_environment(None);
        }
    }

    let _lock = integration_env_lock();
    let base = unique_base();
    let bin = base.join("bin");
    fs::create_dir_all(&bin).unwrap();
    crate::platform::set_test_command_search_environment(Some(
        crate::platform::CommandSearchEnvironment {
            process_path: Some(bin.clone().into_os_string()),
            ..Default::default()
        },
    ));
    let _pinned = Pinned;

    assert!(!command_available("pi"));
    fs::write(bin.join("pi"), "#!/bin/sh\n").unwrap();
    assert!(command_available("pi"));
    assert!(integration_target_available(
        crate::api::schema::IntegrationTarget::Pi
    ));
    assert!(!command_available("pi.exe"));
    fs::create_dir_all(bin.join("claude")).unwrap();
    assert!(
        !command_available("claude"),
        "a directory named like the CLI is not a shim"
    );
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
    let _home = override_home_dir(&home);
    let _path = override_search_path("");

    assert!(integration_target_available(
        crate::api::schema::IntegrationTarget::Codex
    ));

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
    let original_codex_home = std::env::var_os(CODEX_HOME_ENV_VAR);
    let _home = override_home_dir(&home);
    let _path = override_search_path("");
    std::env::remove_var(CODEX_HOME_ENV_VAR);

    assert!(integration_target_available(
        crate::api::schema::IntegrationTarget::Codex
    ));

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
    let original_codex_home = std::env::var_os(CODEX_HOME_ENV_VAR);
    let _path = override_search_path("");
    std::env::remove_var(CODEX_HOME_ENV_VAR);

    for segment in [".volta/bin", ".bun/bin", ".local/share/pnpm"] {
        let home = base.join(segment.replace(['/', '.'], "_"));
        let bin = home.join(segment);
        fs::create_dir_all(&bin).unwrap();
        let binary = bin.join(codex_executable_name());
        fs::write(&binary, "").unwrap();
        make_executable(&binary).unwrap();
        let _home = override_home_dir(&home);

        assert!(
            integration_target_available(crate::api::schema::IntegrationTarget::Codex),
            "codex should be available via {segment}"
        );
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
    let original_codex_home = std::env::var_os(CODEX_HOME_ENV_VAR);
    let _home = override_home_dir(&home);
    let _path = override_search_path("");
    std::env::remove_var(CODEX_HOME_ENV_VAR);
    std::env::set_var("APPDATA", base.join("appdata"));

    assert!(integration_target_available(
        crate::api::schema::IntegrationTarget::Codex
    ));

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
    let original_codex_home = std::env::var_os(CODEX_HOME_ENV_VAR);
    let _home = override_home_dir(&home);
    let _path = override_search_path("");
    std::env::remove_var(CODEX_HOME_ENV_VAR);

    assert!(!integration_target_available(
        crate::api::schema::IntegrationTarget::Codex
    ));

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
    let _home = override_home_dir(&home);
    let _path = override_search_path("");

    let codex = integration_recommendations()
        .into_iter()
        .find(|recommendation| {
            recommendation.target == crate::api::schema::IntegrationTarget::Codex
        })
        .expect("codex recommendation should be present");

    assert!(codex.available);
    assert_eq!(codex.state, IntegrationStatusKind::NotInstalled);
    assert!(codex.needs_install());

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
    let _home = override_home_dir(&home);

    let path = install_pi().unwrap();
    let content = fs::read_to_string(&path).unwrap();

    assert_eq!(path, ext_dir.join(PI_EXTENSION_INSTALL_NAME));
    assert_eq!(content, PI_EXTENSION_ASSET);

    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_pi_creates_extensions_dir_when_agent_dir_exists() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let agent_dir = home.join(".pi/agent");
    fs::create_dir_all(&agent_dir).unwrap();
    let _home = override_home_dir(&home);

    let path = install_pi().unwrap();

    assert_eq!(
        path,
        agent_dir.join("extensions").join(PI_EXTENSION_INSTALL_NAME)
    );
    assert!(path.is_file());

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
    let _home = override_home_dir(&home);
    std::env::set_var(PI_CODING_AGENT_DIR_ENV_VAR, "~/custom-pi-agent");

    let path = install_pi().unwrap();

    assert_eq!(path, ext_dir.join(PI_EXTENSION_INSTALL_NAME));

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
    let _home = override_home_dir(&home);

    let result = uninstall_pi().unwrap();

    assert_eq!(
        result.extension_path,
        ext_dir.join(PI_EXTENSION_INSTALL_NAME)
    );
    assert!(result.removed_extension);
    assert!(!result.extension_path.exists());

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
    let _home = override_home_dir(&home);

    let outdated = outdated_installed_integrations();

    assert_eq!(outdated.len(), 1);
    assert_eq!(
        outdated[0].target,
        crate::api::schema::IntegrationTarget::Pi
    );
    assert_eq!(outdated[0].path, extension_path);
    assert_eq!(outdated[0].installed_version, None);
    assert_eq!(outdated[0].expected_version, PI_INTEGRATION_VERSION);

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
    let _home = override_home_dir(&home);

    let outdated = outdated_installed_integrations();

    assert_eq!(outdated.len(), 1);
    assert_eq!(
        outdated[0].target,
        crate::api::schema::IntegrationTarget::Pi
    );
    assert_eq!(outdated[0].path, extension_path);
    assert_eq!(outdated[0].installed_version, Some(4));
    assert_eq!(outdated[0].expected_version, PI_INTEGRATION_VERSION);

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
    let _home = override_home_dir(&home);

    assert!(outdated_installed_integrations().is_empty());

    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_pi_errors_when_extension_dir_missing() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    fs::create_dir_all(&home).unwrap();
    let _home = override_home_dir(&home);

    let err = install_pi().unwrap_err().to_string();

    assert!(err.contains("pi extension directory not found"));

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
    let _home = override_home_dir(&home);

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
    let _home = override_home_dir(&home);

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
    let _home = override_home_dir(&home);

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
    let _home = override_home_dir(&home);

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
    let _home = override_home_dir(&home);

    let statuses = installed_integration_statuses();
    let claude = statuses
        .iter()
        .find(|status| status.target == crate::api::schema::IntegrationTarget::Claude)
        .unwrap();

    assert_eq!(claude.path, hook_path);
    assert_eq!(claude.installed_version, Some(2));
    assert_eq!(claude.expected_version, CLAUDE_INTEGRATION_VERSION);
    assert_eq!(claude.state, IntegrationStatusKind::Outdated);

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
    let _home = override_home_dir(&home);

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

    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_claude_errors_when_claude_dir_missing() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    fs::create_dir_all(&home).unwrap();
    let _home = override_home_dir(&home);

    let err = install_claude().unwrap_err().to_string();

    assert!(err.contains("claude directory not found"));

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
    let _home = override_home_dir(&home);

    let statuses = installed_integration_statuses();
    let codex = statuses
        .iter()
        .find(|status| status.target == crate::api::schema::IntegrationTarget::Codex)
        .unwrap();

    assert_eq!(codex.path, hook_path);
    assert_eq!(codex.installed_version, Some(2));
    assert_eq!(codex.expected_version, CODEX_INTEGRATION_VERSION);
    assert_eq!(codex.state, IntegrationStatusKind::Outdated);

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
    let _home = override_home_dir(&home);

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
    assert!(hooks["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap()
        .contains(" working"));
    assert!(hooks["hooks"].get("PreToolUse").is_none());
    assert!(hooks["hooks"].get("PermissionRequest").is_none());
    assert!(hooks["hooks"]["Stop"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap()
        .contains(" idle"));
    assert!(hooks["hooks"]["Interrupt"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap()
        .contains(" idle"));
    assert!(config.contains("model = \"gpt-5.4\""));
    assert!(config.contains("[features]"));
    assert!(config.contains("hooks = true"));
    assert!(!config.contains("codex_hooks"));

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
    let _home = override_home_dir(&home);

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
    assert_eq!(
        hooks["hooks"]["UserPromptSubmit"].as_array().unwrap().len(),
        1
    );
    assert!(hooks["hooks"].get("PreToolUse").is_none());
    assert!(hooks["hooks"].get("PermissionRequest").is_none());
    assert_eq!(hooks["hooks"]["Stop"].as_array().unwrap().len(), 1);
    assert_eq!(hooks["hooks"]["Interrupt"].as_array().unwrap().len(), 1);
    assert_eq!(config.matches("hooks = true").count(), 1);
    assert!(!config.contains("codex_hooks"));
    assert!(config.contains("other = true"));

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
    let _home = override_home_dir(&home);

    install_codex().unwrap();

    let config = fs::read_to_string(codex_dir.join("config.toml")).unwrap();

    assert!(config.contains("[profiles.work.features]\nhooks = false\ncodex_hooks = false"));
    assert!(config.contains("[features]\nhooks = true\nother = true"));

    let _ = fs::remove_dir_all(base);
}

#[cfg(test)]
fn assert_codex_feature_edit_preserves_config(features: &str) {
    let prefix = r#"# User settings
model = "gpt-5.4"
profile = "work"

"#;
    let suffix = r#"
# Profile overrides and hook trust must remain untouched.
[profiles.work.features]
hooks = false
codex_hooks = false

[hooks.state."user:session_start:0:0"]
trusted_hash = "keep-me"
"#;
    let content = format!("{prefix}{features}{suffix}");
    let mut expected: toml::Value =
        toml::from_str(&content).expect("input Codex config must be valid TOML");
    expected["features"]
        .as_table_mut()
        .unwrap()
        .insert("hooks".to_string(), toml::Value::Boolean(true));

    let updated = super::config_edit::build_codex_config_with_hooks(&content)
        .expect("valid Codex config must be editable");
    let parsed: toml::Value = toml::from_str(&updated).unwrap_or_else(|error| {
        panic!("updated Codex config must be valid TOML: {error}\n{updated}")
    });
    assert_eq!(parsed["features"]["hooks"].as_bool(), Some(true));
    assert_eq!(parsed, expected, "only top-level features.hooks may change");
    assert!(
        updated.starts_with(prefix),
        "user settings must be preserved"
    );
    assert!(
        updated.ends_with(suffix),
        "profile overrides and hook trust must be preserved"
    );

    let repeated = super::config_edit::build_codex_config_with_hooks(&updated)
        .expect("repeated Codex config update must succeed");
    let reparsed: toml::Value =
        toml::from_str(&repeated).expect("repeated Codex config update must remain valid TOML");
    assert_eq!(reparsed, expected);
    assert_eq!(repeated, updated, "Codex config edits must be idempotent");
}

#[test]
fn build_codex_config_with_hooks_accepts_quoted_hook_keys() {
    for key in ["hooks", "\"hooks\"", "'hooks'"] {
        let features = format!("[features]\n{key} = false\nother = true\n");
        assert_codex_feature_edit_preserves_config(&features);
    }
}

#[test]
fn build_codex_config_with_hooks_accepts_quoted_feature_tables() {
    for table in ["\"features\"", "'features'"] {
        let features = format!("[{table}]\nhooks = false\nother = true\n");
        assert_codex_feature_edit_preserves_config(&features);
    }
}

#[test]
fn build_codex_config_with_hooks_accepts_dotted_and_inline_features() {
    for features in [
        "features.hooks = false\nfeatures.other = true\n",
        "features = { hooks = false, other = true }\n",
        "features.other = true\n",
        "features = { other = true }\n",
        "[features]\nother = true\n",
        "[features.future]\nflag = true\n",
    ] {
        assert_codex_feature_edit_preserves_config(features);
    }
}

#[test]
fn build_codex_config_with_hooks_preserves_comments_and_migrates_deprecated_flags() {
    for (input, expected) in [
        (
            "['features'] # table\n# user note\n'codex_hooks'\t= false # migration note\nother = true\n",
            "['features'] # table\n# user note\nhooks\t= true # migration note\nother = true\n",
        ),
        (
            "[features]\n# old note\ncodex_hooks = false # keep this note\n\"hooks\" = false # hook note\nother = true\n",
            "[features]\n# old note\n # keep this note\n\"hooks\" = true # hook note\nother = true\n",
        ),
        (
            "features.'codex_hooks' = false # old note\nmodel = 'keep'\nfeatures.hooks = false # hook note\n",
            " # old note\nmodel = 'keep'\nfeatures.hooks = true # hook note\n",
        ),
        (
            "features = { codex_hooks = false, hooks = false, other = true } # table note\n",
            "features = {  hooks = true, other = true } # table note\n",
        ),
        (
            "features = { other = true, hooks = false, 'codex_hooks' = false } # table note\n",
            "features = { other = true, hooks = true } # table note\n",
        ),
        (
            "features = { 'codex_hooks' = false, other = true } # table note\n",
            "features = { hooks = true, other = true } # table note\n",
        ),
        (
            "model = 'keep'\r\n[\"features\"] # table note\r\n\"hooks\" = false # hook note\r\n",
            "model = 'keep'\r\n[\"features\"] # table note\r\n\"hooks\" = true # hook note\r\n",
        ),
        (
            "[features]\nhooks = false # no final newline",
            "[features]\nhooks = true # no final newline",
        ),
        (
            "instructions = '''\n[features]\nhooks = false\ncodex_hooks = false\n'''\n[features]\n'hooks' = false\n",
            "instructions = '''\n[features]\nhooks = false\ncodex_hooks = false\n'''\n[features]\n'hooks' = true\n",
        ),
    ] {
        let mut expected_value: toml::Value = toml::from_str(input).unwrap();
        let features = expected_value["features"].as_table_mut().unwrap();
        features.remove("codex_hooks");
        features.insert("hooks".to_string(), toml::Value::Boolean(true));
        let updated = super::config_edit::build_codex_config_with_hooks(input)
            .unwrap_or_else(|error| panic!("{error}\n{input}"));
        assert_eq!(updated, expected, "{input}");
        assert_eq!(toml::from_str::<toml::Value>(&updated).unwrap(), expected_value);
        assert_eq!(
            super::config_edit::build_codex_config_with_hooks(&updated).unwrap(),
            updated
        );
    }
    for input in ["", "# user note\n", "model = 'keep'", "model = 'keep'\n"] {
        let mut expected: toml::Table = toml::from_str(input).unwrap();
        expected.insert(
            "features".to_string(),
            toml::Value::Table(toml::Table::from_iter([(
                "hooks".to_string(),
                toml::Value::Boolean(true),
            )])),
        );
        let updated = super::config_edit::build_codex_config_with_hooks(input).unwrap();
        assert!(updated.contains(input));
        assert_eq!(toml::from_str::<toml::Table>(&updated).unwrap(), expected);
        assert_eq!(
            super::config_edit::build_codex_config_with_hooks(&updated).unwrap(),
            updated
        );
    }
}

#[cfg(test)]
fn assert_codex_install_rejection_preserves_files(
    config: Option<&str>,
    hooks: Option<&str>,
    installed: bool,
) -> std::io::Error {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let codex_dir = home.join(".codex");
    fs::create_dir_all(&codex_dir).unwrap();
    let _home = override_home_dir(&home);
    if let Some(config) = config {
        fs::write(codex_dir.join("config.toml"), config).unwrap();
    }
    if let Some(hooks) = hooks {
        fs::write(codex_dir.join("hooks.json"), hooks).unwrap();
    }
    if installed {
        for name in [CODEX_HOOK_INSTALL_NAME, "herdr-agent-state.sh"] {
            fs::write(codex_dir.join(name), format!("previous asset: {name}\n")).unwrap();
        }
    }
    let snapshot = || {
        fs::read_dir(&codex_dir)
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                (entry.file_name(), fs::read(entry.path()).unwrap())
            })
            .collect::<std::collections::BTreeMap<_, _>>()
    };
    let before = snapshot();
    let result = install_codex();
    let after = snapshot();
    fs::remove_dir_all(&base).unwrap();
    assert_eq!(
        after, before,
        "rejection must not create, replace or remove files"
    );
    result.expect_err("invalid Codex settings must reject installation")
}

#[test]
fn install_codex_rejects_invalid_or_unsafe_config_before_writing_files() {
    for config in [
        "[features\n",
        "[features]\nhooks = false\n\"hooks\" = true\n",
        "features = false\n",
        "features = [{ hooks = false }]\n",
        "[[features]]\nhooks = false\n",
        "[features]\nhooks = 'false'\n",
        "[features]\nhooks = []\n",
        "[features.hooks]\ncustom = true\n",
        "[features]\ncodex_hooks = 'false'\n",
        "[features.codex_hooks]\ncustom = true\n",
        "features.other = true\nmodel = 'keep'\nfeatures.future = false\n",
    ] {
        for (hooks, installed) in [(None, false), (Some("{ \"hooks\": {} }\n"), true)] {
            let error =
                assert_codex_install_rejection_preserves_files(Some(config), hooks, installed);
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidData, "{config}");
            assert!(error.to_string().contains("config.toml"), "{error}");
        }
    }
}

#[test]
fn install_codex_rejects_invalid_hooks_before_writing_files() {
    for hooks in [
        "{",
        "[]",
        r#"{"hooks": false}"#,
        r#"{"hooks": {"SessionStart": {}}}"#,
        r#"{"hooks": {"Interrupt": {}}}"#,
    ] {
        for (config, installed) in [(None, false), (Some("[features]\nhooks = false\n"), true)] {
            let error =
                assert_codex_install_rejection_preserves_files(config, Some(hooks), installed);
            assert!(!error.to_string().is_empty());
        }
    }
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
            ]}],
            "Interrupt": [{"hooks": [{"type": "command", "command": format!("bash '{}' idle", hook_path.display()), "timeout": 10}]}]
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
    let _home = override_home_dir(&home);

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
    assert!(hooks["hooks"].get("Interrupt").is_none());
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

    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_codex_errors_when_config_dir_missing() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    fs::create_dir_all(&home).unwrap();
    let _home = override_home_dir(&home);

    let err = install_codex().unwrap_err().to_string();

    assert!(err.contains("codex config directory not found"));

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
    let _home = override_home_dir(&home);

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
            "UserPromptSubmit" => "user_prompt_submit",
            "Stop" => "stop",
            "SubagentStart" => "subagent_start",
            "SubagentStop" => "subagent_stop",
            // Interrupt/SessionEnd：codex 不为其持久化信任状态（hash 为 None）。
            "Interrupt" | "SessionEnd" => continue,
            other => panic!("unmapped codex hook event: {other}"),
        };
        let Some(hash) = super::codex_trust::codex_hook_trust_hash(event, &command, 10) else {
            continue;
        };
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
    let _home = override_home_dir(&home);
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
    let _home = override_home_dir(&home);

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
    let _home = override_home_dir(&home);

    install_kimi().unwrap();
    install_kimi().unwrap();

    let config = fs::read_to_string(kimi_dir.join("config.toml")).unwrap();
    let hooks = kimi_config_hooks(&config);

    assert_eq!(config.matches(KIMI_CONFIG_BLOCK_BEGIN).count(), 1);
    assert_eq!(config.matches(KIMI_CONFIG_BLOCK_END).count(), 1);
    assert_eq!(hooks.len(), KIMI_HOOK_EVENTS.len());

    let _ = fs::remove_dir_all(base);
}

#[test]
fn uninstall_kimi_removes_hook_and_config_block_preserves_other_hooks() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let kimi_dir = home.join(".kimi-code");
    fs::create_dir_all(&kimi_dir).unwrap();
    let _home = override_home_dir(&home);

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

    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_kimi_errors_when_config_dir_missing() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    fs::create_dir_all(&home).unwrap();
    let _home = override_home_dir(&home);

    let err = install_kimi().unwrap_err().to_string();

    assert!(err.contains("kimi code config directory not found"));

    let _ = fs::remove_dir_all(base);
}

fn kimi_audit_retry<T>(
    mut operation: impl FnMut() -> std::io::Result<T>,
    deadline: std::time::Instant,
    mut wait: impl FnMut(std::time::Duration),
) -> std::io::Result<T> {
    loop {
        let error = match operation() {
            Ok(value) => return Ok(value),
            Err(error) => error,
        };
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if !cfg!(windows) || !matches!(error.raw_os_error(), Some(32 | 33)) || remaining.is_zero() {
            return Err(error);
        }
        wait(remaining.min(std::time::Duration::from_millis(10)));
        if std::time::Instant::now() >= deadline {
            return Err(error);
        }
    }
}

fn kimi_audit_read(
    path: &Path,
    deadline: std::time::Instant,
    wait: impl FnMut(std::time::Duration),
) -> std::io::Result<Vec<u8>> {
    kimi_audit_retry(|| fs::read(path), deadline, wait)
}

#[cfg(windows)]
fn kimi_audit_open_exclusive(
    path: &Path,
    deadline: std::time::Instant,
    wait: impl FnMut(std::time::Duration),
) -> std::io::Result<fs::File> {
    use std::os::windows::fs::OpenOptionsExt;

    kimi_audit_retry(
        || {
            fs::OpenOptions::new()
                .read(true)
                .write(true)
                .share_mode(0)
                .open(path)
        },
        deadline,
        wait,
    )
}

type KimiAuditSnapshot = std::collections::BTreeMap<PathBuf, Option<Vec<u8>>>;

#[track_caller]
fn kimi_audit_snapshot(root: &Path) -> KimiAuditSnapshot {
    let caller = std::panic::Location::caller();
    kimi_audit_snapshot_for(root, 0, &format!("{}:{}", caller.file(), caller.line()))
}

fn kimi_audit_snapshot_for(root: &Path, case_index: usize, phase: &str) -> KimiAuditSnapshot {
    kimi_audit_snapshot_after_enumeration(root, case_index, phase, |_| {})
        .unwrap_or_else(|error| panic!("{error}"))
}

fn kimi_audit_snapshot_after_enumeration(
    root: &Path,
    case_index: usize,
    phase: &str,
    mut after_enumeration: impl FnMut(&Path),
) -> Result<KimiAuditSnapshot, String> {
    let mut snapshot = KimiAuditSnapshot::new();
    let mut pending = vec![root.to_path_buf()];
    let mut enumerated = Vec::new();
    let describe = |operation: &str, path: &Path, names: &[PathBuf], error: std::io::Error| {
        format!(
            "cannot snapshot: case_index={case_index}, phase={phase}, operation={operation}, path={}, enumerated={names:?}, os_code={:?}, error={error:?}",
            path.display(),
            error.raw_os_error()
        )
    };
    while let Some(dir) = pending.pop() {
        let entries =
            fs::read_dir(&dir).map_err(|error| describe("read_dir", &dir, &enumerated, error))?;
        let mut listed = Vec::new();
        for entry in entries {
            let entry =
                entry.map_err(|error| describe("read_dir_entry", &dir, &enumerated, error))?;
            let path = entry.path();
            enumerated.push(path.strip_prefix(root).unwrap().to_path_buf());
            let kind = entry
                .file_type()
                .map_err(|error| describe("file_type", &path, &enumerated, error))?;
            listed.push((path, kind));
        }
        after_enumeration(&dir);
        for (path, kind) in listed {
            let relative = path.strip_prefix(root).unwrap().to_path_buf();
            if kind.is_dir() {
                snapshot.insert(relative, None);
                pending.push(path);
            } else {
                if !kind.is_file() {
                    return Err(describe(
                        "file_type",
                        &path,
                        &enumerated,
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "unexpected fixture type",
                        ),
                    ));
                }
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
                let contents = kimi_audit_read(&path, deadline, std::thread::sleep)
                    .map_err(|error| describe("read", &path, &enumerated, error))?;
                snapshot.insert(relative, Some(contents));
            }
        }
    }
    Ok(snapshot)
}

#[test]
fn audit_p0_kimi_snapshot_rejects_file_removed_after_enumeration() {
    let dir = crate::config::test_dirs::TempDir::new("audit-kimi-disappeared");
    let path = dir.join("CONFIG.TOML.HERDR-BACKUP.tmp");
    fs::write(&path, b"private fixture content").unwrap();
    let mut barriers = 0;
    let error = kimi_audit_snapshot_after_enumeration(&dir, 7, "after-install", |listed_dir| {
        assert_eq!(listed_dir, dir.path());
        barriers += 1;
        fs::remove_file(&path).unwrap();
    })
    .unwrap_err();
    assert_eq!(barriers, 1);
    for expected in [
        "case_index=7",
        "phase=after-install",
        "operation=read,",
        "enumerated=[",
        "CONFIG.TOML.HERDR-BACKUP.tmp",
        "NotFound",
    ] {
        assert!(error.contains(expected), "{error}");
    }
    assert!(error.contains(&format!(
        "os_code={:?}",
        fs::read(&path).unwrap_err().raw_os_error()
    )));
    assert!(!error.contains("private fixture content"));
}

#[test]
fn audit_p0_kimi_snapshot_includes_persistent_tmp_in_comparison() {
    let dir = crate::config::test_dirs::TempDir::new("audit-kimi-persistent-tmp");
    let relative = PathBuf::from("CONFIG.TOML.HERDR-BACKUP.tmp");
    let path = dir.join(&relative);
    fs::write(&path, b"before\0\xff").unwrap();
    let before = kimi_audit_snapshot_for(&dir, 0, "before-change");
    assert_eq!(before.get(&relative), Some(&Some(b"before\0\xff".to_vec())));
    assert_eq!(kimi_audit_snapshot_for(&dir, 0, "unchanged"), before);
    fs::write(&path, b"after\r\n").unwrap();
    let after = kimi_audit_snapshot_for(&dir, 0, "after-change");
    assert_eq!(after.get(&relative), Some(&Some(b"after\r\n".to_vec())));
    assert_ne!(after, before);
}

#[cfg(windows)]
struct KimiAuditFileLock {
    path: PathBuf,
    file: Option<fs::File>,
    byte_locked: bool,
}

#[cfg(windows)]
impl KimiAuditFileLock {
    fn unlock(&mut self) -> std::io::Result<()> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::{Storage::FileSystem::UnlockFileEx, System::IO::OVERLAPPED};

        if self.byte_locked {
            let file = self.file.as_ref().expect("byte lock must retain its file");
            let mut overlapped = OVERLAPPED::default();
            if unsafe { UnlockFileEx(file.as_raw_handle(), 0, 1, 0, &mut overlapped) } == 0 {
                return Err(std::io::Error::last_os_error());
            }
            self.byte_locked = false;
        }
        Ok(())
    }

    fn release(mut self) {
        self.unlock()
            .unwrap_or_else(|error| panic!("cannot unlock {}: {error:?}", self.path.display()));
        drop(self.file.take());
    }
}

#[cfg(windows)]
impl Drop for KimiAuditFileLock {
    fn drop(&mut self) {
        let result = self.unlock();
        drop(self.file.take());
        if !std::thread::panicking() {
            result
                .unwrap_or_else(|error| panic!("cannot unlock {}: {error:?}", self.path.display()));
        }
    }
}

#[cfg(windows)]
fn kimi_audit_lock_file(path: &Path, code: i32) -> KimiAuditFileLock {
    use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
    use windows_sys::Win32::{
        Storage::FileSystem::{LockFileEx, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY},
        System::IO::OVERLAPPED,
    };

    let mut options = fs::OpenOptions::new();
    options.read(true).write(true);
    if code == 32 {
        options.share_mode(0);
    }
    let mut held = KimiAuditFileLock {
        path: path.to_path_buf(),
        file: Some(options.open(path).unwrap()),
        byte_locked: false,
    };
    if code == 33 {
        let file = held.file.as_ref().unwrap();
        let mut overlapped = OVERLAPPED::default();
        assert_ne!(
            unsafe {
                LockFileEx(
                    file.as_raw_handle(),
                    LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
                    0,
                    1,
                    0,
                    &mut overlapped,
                )
            },
            0,
            "{}: {:?}",
            path.display(),
            std::io::Error::last_os_error()
        );
        held.byte_locked = true;
    }
    assert_eq!(fs::read(path).unwrap_err().raw_os_error(), Some(code));
    held
}

#[cfg(windows)]
#[test]
fn audit_p0_kimi_snapshot_read_recovers_after_windows_lock_release() {
    use std::time::{Duration, Instant};

    let dir = crate::config::test_dirs::TempDir::new("audit-kimi-read");
    let root = dir.path().to_path_buf();
    fs::create_dir(root.join("nested")).unwrap();
    fs::write(root.join("user-hook"), b"untouched\r\n").unwrap();
    for code in [32, 33] {
        let path = root.join("nested").join(format!("locked-{code}"));
        let contents = b"full\0bytes\r\n\xff";
        fs::write(&path, contents).unwrap();
        let before = kimi_audit_snapshot(&root);
        let held = kimi_audit_lock_file(&path, code);
        std::thread::scope(|scope| {
            let (release, wait_release) = std::sync::mpsc::channel();
            let (released, wait_released) = std::sync::mpsc::channel();
            let holder = scope.spawn(move || {
                let signal = wait_release.recv_timeout(Duration::from_secs(30));
                held.release();
                if signal.is_ok() {
                    released.send(()).unwrap();
                }
            });
            let mut waits = 0;
            let result = kimi_audit_read(&path, Instant::now() + Duration::from_secs(30), |_| {
                waits += 1;
                if waits == 1 {
                    release.send(()).unwrap();
                    wait_released.recv_timeout(Duration::from_secs(30)).unwrap();
                }
            });
            drop(release);
            holder.join().unwrap();
            assert_eq!(
                result.unwrap_or_else(|error| panic!("{}: {error:?}", path.display())),
                contents
            );
            assert!(waits > 0, "the locked read must enter the retry path");
        });
        assert_eq!(kimi_audit_snapshot(&root), before);
    }
    drop(dir);
    assert!(
        !root.exists(),
        "temporary tree must be cleaned after handles close"
    );
}

#[cfg(windows)]
#[test]
fn audit_p0_kimi_snapshot_read_stops_at_deadline_for_persistent_windows_locks() {
    use std::time::{Duration, Instant};

    let dir = crate::config::test_dirs::TempDir::new("audit-kimi-deadline");
    let root = dir.path().to_path_buf();
    let path = root.join("locked-file");
    fs::write(&path, b"unchanged").unwrap();
    for code in [32, 33] {
        let held = kimi_audit_lock_file(&path, code);
        let deadline = Instant::now() + Duration::from_millis(50);
        let error = kimi_audit_read(&path, deadline, std::thread::sleep).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(code));
        assert!(
            Instant::now() >= deadline,
            "sharing errors must wait until the deadline"
        );
        assert!(Instant::now() - deadline < Duration::from_secs(5));
        let failure = std::panic::catch_unwind(|| kimi_audit_snapshot(&root)).unwrap_err();
        let message = failure.downcast_ref::<String>().unwrap();
        assert!(message.contains("cannot snapshot"));
        assert!(message.contains(&path.display().to_string()));
        assert!(message.contains(&format!("code: {code}")));
        held.release();
        assert_eq!(fs::read(&path).unwrap(), b"unchanged");
    }
    drop(dir);
    assert!(!root.exists());
}

#[cfg(windows)]
#[test]
fn audit_p0_kimi_snapshot_lock_fixture_releases_during_unwind() {
    let dir = crate::config::test_dirs::TempDir::new("audit-kimi-unwind");
    let root = dir.path().to_path_buf();
    let path = root.join("locked-file");
    fs::write(&path, b"unchanged").unwrap();
    for code in [32, 33] {
        let acquired = std::sync::atomic::AtomicBool::new(false);
        let failure = std::panic::catch_unwind(|| {
            let _held = kimi_audit_lock_file(&path, code);
            acquired.store(true, std::sync::atomic::Ordering::Relaxed);
            panic!("exercise fixture cleanup during unwind");
        });
        assert!(acquired.load(std::sync::atomic::Ordering::Relaxed));
        assert!(failure.is_err());
        assert_eq!(fs::read(&path).unwrap(), b"unchanged");
    }
    drop(dir);
    assert!(!root.exists());
}

#[test]
fn audit_p0_kimi_snapshot_read_does_not_retry_other_errors() {
    let dir = crate::config::test_dirs::TempDir::new("audit-kimi-read-errors");
    let root = dir.path().to_path_buf();
    for path in [root.join("missing"), root.clone()] {
        let expected = fs::read(&path).unwrap_err();
        let error = kimi_audit_read(
            &path,
            std::time::Instant::now() + std::time::Duration::from_secs(30),
            |_| panic!("must not retry non-sharing error for {}", path.display()),
        )
        .unwrap_err();
        assert_eq!(error.raw_os_error(), expected.raw_os_error());
        assert_eq!(error.kind(), expected.kind());
    }
    drop(dir);
    assert!(!root.exists());
}

#[cfg(windows)]
#[test]
fn audit_p0_kimi_exclusive_probe_waits_for_shared_reader_release() {
    use std::os::windows::fs::OpenOptionsExt;
    use std::time::{Duration, Instant};

    let dir = crate::config::test_dirs::TempDir::new("audit-kimi-shared-reader");
    let root = dir.path().to_path_buf();
    let path = root.join("config.toml");
    fs::write(&path, b"unchanged\r\n").unwrap();
    let before = kimi_audit_snapshot(&root);
    let reader = fs::File::open(&path).unwrap();
    assert_eq!(
        fs::OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(0)
            .open(&path)
            .unwrap_err()
            .raw_os_error(),
        Some(32)
    );
    assert_eq!(fs::read(&path).unwrap(), b"unchanged\r\n");
    std::thread::scope(|scope| {
        let (release, wait_release) = std::sync::mpsc::channel();
        let (released, wait_released) = std::sync::mpsc::channel();
        let holder = scope.spawn(move || {
            let signal = wait_release.recv_timeout(Duration::from_secs(30));
            drop(reader);
            if signal.is_ok() {
                released.send(()).unwrap();
            }
        });
        let mut waits = 0;
        let result =
            kimi_audit_open_exclusive(&path, Instant::now() + Duration::from_secs(30), |_| {
                waits += 1;
                if waits == 1 {
                    release.send(()).unwrap();
                    wait_released.recv_timeout(Duration::from_secs(30)).unwrap();
                }
            });
        drop(release);
        holder.join().unwrap();
        drop(result.unwrap_or_else(|error| panic!("{}: {error:?}", path.display())));
        assert!(
            waits > 0,
            "the shared reader must block the exclusive probe"
        );
    });
    assert_eq!(kimi_audit_snapshot(&root), before);
    drop(dir);
    assert!(!root.exists());
}

#[cfg(windows)]
#[test]
fn audit_p0_kimi_exclusive_probe_rejects_persistent_occupancy_and_other_errors() {
    use std::time::{Duration, Instant};

    let dir = crate::config::test_dirs::TempDir::new("audit-kimi-probe-errors");
    let root = dir.path().to_path_buf();
    let path = root.join("config.toml");
    fs::write(&path, b"unchanged\r\n").unwrap();
    let reader = fs::File::open(&path).unwrap();
    let deadline = Instant::now() + Duration::from_millis(50);
    let error = kimi_audit_open_exclusive(&path, deadline, std::thread::sleep).unwrap_err();
    assert_eq!(error.raw_os_error(), Some(32));
    assert!(Instant::now() >= deadline);
    assert!(Instant::now() - deadline < Duration::from_secs(5));
    drop(reader);
    drop(
        kimi_audit_open_exclusive(
            &path,
            Instant::now() + Duration::from_secs(2),
            std::thread::sleep,
        )
        .unwrap(),
    );
    for path in [root.join("missing"), root.clone()] {
        let error =
            kimi_audit_open_exclusive(&path, Instant::now() + Duration::from_secs(30), |_| {
                panic!("must not retry non-sharing error for {}", path.display())
            })
            .unwrap_err();
        assert!(!matches!(error.raw_os_error(), Some(32 | 33)));
    }
    assert_eq!(fs::read(&path).unwrap(), b"unchanged\r\n");
    drop(dir);
    assert!(!root.exists());
}

#[cfg(windows)]
#[test]
fn audit_p0_kimi_install_uninstall_leave_no_persistent_file_occupancy() {
    let _lock = crate::config::test_config_env_lock().lock().unwrap();
    let dirs = crate::config::test_dirs::isolate_dirs("audit-kimi-handles");
    std::env::remove_var(KIMI_CODE_HOME_ENV_VAR);
    let dir = dirs.home_dir().join(".kimi-code");
    fs::create_dir_all(&dir).unwrap();
    let config = dir.join("config.toml");
    fs::write(&config, "default_model = 'moonshot'\n").unwrap();
    let root = dirs.home_dir().parent().unwrap().to_path_buf();
    let assert_accessible = |path: &Path| {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        drop(
            kimi_audit_open_exclusive(path, deadline, std::thread::sleep).unwrap_or_else(|error| {
                panic!(
                    "cannot access exclusively before deadline {}: {error:?}",
                    path.display()
                )
            }),
        );
    };
    for _ in 0..2 {
        let installed = install_kimi().expect("install must retain its ordinary error boundary");
        assert_accessible(&installed.config_path);
        assert_accessible(&installed.hook_path);
        assert!(!dir.join("config.toml.herdr-backup").exists());
        assert!(!dir.join("config.toml.herdr-backup.pending").exists());
    }
    for _ in 0..2 {
        let result = uninstall_kimi().expect("uninstall must retain its ordinary error boundary");
        assert_accessible(&result.config_path);
        assert!(!result.hook_path.exists());
    }
    drop(dirs);
    assert!(!root.exists());
}

fn assert_kimi_audit_marker_rejection(action: &str, run: impl Fn() -> std::io::Result<()>) {
    let _lock = crate::config::test_config_env_lock().lock().unwrap();
    std::env::remove_var(KIMI_CODE_HOME_ENV_VAR);
    let begin = KIMI_CONFIG_BLOCK_BEGIN;
    let end = KIMI_CONFIG_BLOCK_END;
    let mut failures = Vec::new();
    for (case, markers) in [
        ("missing end", vec![begin]),
        ("missing begin", vec![end]),
        ("reversed markers", vec![end, begin]),
        ("nested markers", vec![begin, begin, end, end]),
        ("duplicate begin", vec![begin, begin, end]),
        ("duplicate end", vec![begin, end, end]),
        ("duplicate blocks", vec![begin, end, begin, end]),
    ] {
        for existing_hooks in [false, true] {
            let dirs = crate::config::test_dirs::isolate_dirs("audit-kimi-reject");
            let dir = dirs.home_dir().join(".kimi-code");
            fs::create_dir_all(&dir).unwrap();
            assert_eq!(kimi_dir().unwrap(), dir);
            let content = format!(
                "# user header\r\ndefault_model = 'keep'\r\n{}\r\n[models.user]\r\nprovider = 'keep'\r\n# user tail\r\n",
                markers.join("\r\n")
            );
            toml::from_str::<toml::Value>(&content)
                .expect("broken marker fixtures must still be valid TOML");
            fs::write(dir.join("config.toml"), content).unwrap();
            if existing_hooks {
                let hooks = dir.join("hooks");
                fs::create_dir_all(&hooks).unwrap();
                for name in [
                    "herdr-agent-state.sh",
                    "herdr-agent-state.ps1",
                    "user-hook.txt",
                ] {
                    fs::write(
                        hooks.join(name),
                        format!("# HERDR_INTEGRATION_ID=kimi\n# previous asset: {name}\n"),
                    )
                    .unwrap();
                }
            }
            let root = dirs.home_dir().parent().unwrap();
            let before = kimi_audit_snapshot(root);
            let result = run();
            let after = kimi_audit_snapshot(root);
            let label = format!("{action}: {case}, existing_hooks={existing_hooks}");
            if after != before {
                failures.push(format!(
                    "{label}: modified the private root before rejection"
                ));
            }
            match result {
                Ok(()) => failures.push(format!("{label}: accepted damaged markers")),
                Err(error) => {
                    if error.kind() != std::io::ErrorKind::InvalidData
                        || !error.to_string().contains("config.toml")
                    {
                        failures.push(format!("{label}: unexpected rejection: {error:?}"));
                    }
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn audit_p0_kimi_install_rejects_damaged_markers_before_any_write() {
    assert_kimi_audit_marker_rejection("install", || install_kimi().map(|_| ()));
}

#[test]
fn audit_p0_kimi_uninstall_rejects_damaged_markers_before_any_write() {
    assert_kimi_audit_marker_rejection("uninstall", || uninstall_kimi().map(|_| ()));
}

fn kimi_audit_pseudo_marker_configs() -> Vec<String> {
    let begin = KIMI_CONFIG_BLOCK_BEGIN;
    let end = KIMI_CONFIG_BLOCK_END;
    let paired = format!("before\n{begin}\nkeep this text\n{end}\nafter");
    [
        format!("instructions = \"\"\"\n{paired}\n\"\"\""),
        format!("instructions = '''\n{paired}\n'''"),
        format!("notes = [{{ text = \"\"\"\n{begin}\nkeep this text\n\"\"\" }}]"),
        format!("notes = {{ text = '''\n{end}\nkeep this text\n''' }}"),
    ]
    .into_iter()
    .map(|value| {
        format!(
            "# user header\ndefault_model = 'moonshot'\n{value}\n[models.user]\nprovider = 'keep'\n# user tail\n"
        )
    })
    .collect()
}

#[test]
fn audit_p0_kimi_text_helpers_preserve_markers_inside_multiline_strings() {
    let hook_path = Path::new("hooks").join(KIMI_HOOK_INSTALL_NAME);
    for content in kimi_audit_pseudo_marker_configs() {
        let expected: toml::Table = toml::from_str(&content).unwrap();
        assert_eq!(
            super::config_edit::remove_kimi_config_block(&content).unwrap(),
            content,
            "marker-shaped string data is not a managed comment block"
        );
        let updated =
            super::config_edit::build_kimi_config_with_hooks(&content, &hook_path).unwrap();
        assert!(
            updated.starts_with(&content),
            "user text must remain byte-exact"
        );
        let mut parsed: toml::Table = toml::from_str(&updated).unwrap();
        let hooks = parsed.remove("hooks").unwrap();
        assert_eq!(hooks.as_array().unwrap().len(), KIMI_HOOK_EVENTS.len());
        assert_eq!(parsed, expected, "only managed hooks may be added");
        assert_eq!(
            super::config_edit::remove_kimi_config_block(&updated).unwrap(),
            content
        );
    }
}

#[test]
fn audit_p0_kimi_install_uninstall_preserve_multiline_string_markers() {
    let _lock = crate::config::test_config_env_lock().lock().unwrap();
    std::env::remove_var(KIMI_CODE_HOME_ENV_VAR);
    for content in kimi_audit_pseudo_marker_configs() {
        let dirs = crate::config::test_dirs::isolate_dirs("audit-kimi-string");
        let dir = dirs.home_dir().join(".kimi-code");
        fs::create_dir_all(&dir).unwrap();
        assert_eq!(kimi_dir().unwrap(), dir);
        let config_path = dir.join("config.toml");
        fs::write(&config_path, &content).unwrap();
        let expected: toml::Table = toml::from_str(&content).unwrap();
        let root = dirs.home_dir().parent().unwrap();
        let before = kimi_audit_snapshot(root);

        let uninstalled = uninstall_kimi().expect("string markers must not reject uninstall");
        assert!(!uninstalled.updated_config);
        assert!(!uninstalled.removed_hook_file);
        assert_eq!(kimi_audit_snapshot(root), before);

        let install_result = install_kimi();
        let installed = install_result.expect("string markers must not reject install");
        let updated = fs::read_to_string(&config_path).unwrap();
        assert!(updated.starts_with(&content));
        let mut parsed: toml::Table = toml::from_str(&updated).unwrap();
        let hooks = parsed.remove("hooks").unwrap();
        assert_eq!(hooks.as_array().unwrap().len(), KIMI_HOOK_EVENTS.len());
        assert_eq!(parsed, expected);
        for (event, matcher, action) in KIMI_HOOK_EVENTS {
            assert_kimi_hook(&updated, &installed.hook_path, event, matcher, action);
        }

        let uninstalled = uninstall_kimi().unwrap();
        assert!(uninstalled.updated_config);
        assert!(uninstalled.removed_hook_file);
        assert_eq!(fs::read(&config_path).unwrap(), content.as_bytes());
        assert!(!installed.hook_path.exists());
    }
}

fn assert_kimi_audit_roundtrip(newline: &str) {
    let _lock = crate::config::test_config_env_lock().lock().unwrap();
    let dirs = crate::config::test_dirs::isolate_dirs("audit-kimi-roundtrip");
    std::env::remove_var(KIMI_CODE_HOME_ENV_VAR);
    let dir = dirs.home_dir().join(".kimi-code");
    let hooks_dir = dir.join("hooks");
    fs::create_dir_all(&hooks_dir).unwrap();
    assert_eq!(kimi_dir().unwrap(), dir);
    let user_hook = hooks_dir.join("user-hook.txt");
    fs::write(&user_hook, b"untouched user hook\r\n").unwrap();
    let prefix = "# user header\ndefault_model = 'moonshot' # keep model\n\n[[hooks]]\nevent = 'Notification'\nmatcher = 'user.custom'\ncommand = 'echo keep'\ntimeout = 3 # keep timeout\n"
        .replace('\n', newline);
    let suffix =
        "[models.user]\nprovider = 'keep' # keep provider\n# user tail\n".replace('\n', newline);
    let original = format!("{prefix}{suffix}");
    let content = format!(
        "{prefix}{KIMI_CONFIG_BLOCK_BEGIN}{newline}[[hooks]]{newline}event = 'Stop'{newline}command = 'echo previous-managed'{newline}timeout = 10{newline}{KIMI_CONFIG_BLOCK_END}{newline}{suffix}"
    );
    toml::from_str::<toml::Value>(&content).unwrap();
    let expected: toml::Value = toml::from_str(&original).unwrap();
    let config_path = dir.join("config.toml");
    fs::write(&config_path, content).unwrap();

    let install_result = install_kimi();
    let installed = install_result.expect("one paired block must be replaceable");
    let first = fs::read_to_string(&config_path).unwrap();
    assert!(
        first.starts_with(&prefix),
        "user prefix must remain byte-exact"
    );
    assert!(
        first.contains(&suffix),
        "user suffix must remain byte-exact"
    );
    assert_eq!(first.matches(KIMI_CONFIG_BLOCK_BEGIN).count(), 1);
    assert_eq!(first.matches(KIMI_CONFIG_BLOCK_END).count(), 1);
    let mut parsed: toml::Value = toml::from_str(&first).unwrap();
    let hooks = parsed["hooks"].as_array_mut().unwrap();
    assert_eq!(hooks.len(), KIMI_HOOK_EVENTS.len() + 1);
    hooks.truncate(1);
    assert_eq!(
        parsed, expected,
        "unrelated TOML semantics must survive install"
    );
    for (event, matcher, action) in KIMI_HOOK_EVENTS {
        assert_kimi_hook(&first, &installed.hook_path, event, matcher, action);
    }
    let root = dirs.home_dir().parent().unwrap();
    let after_install = kimi_audit_snapshot(root);
    install_kimi().expect("repeated install must succeed");
    assert_eq!(kimi_audit_snapshot(root), after_install);

    let removed = uninstall_kimi().expect("one paired block must be removable");
    assert!(removed.updated_config);
    assert!(removed.removed_hook_file);
    assert!(!installed.hook_path.exists());
    assert_eq!(fs::read(&config_path).unwrap(), original.as_bytes());
    assert_eq!(fs::read(&user_hook).unwrap(), b"untouched user hook\r\n");
    let after_uninstall = kimi_audit_snapshot(root);
    let repeated = uninstall_kimi().expect("repeated uninstall must succeed");
    assert!(!repeated.updated_config);
    assert!(!repeated.removed_hook_file);
    assert_eq!(kimi_audit_snapshot(root), after_uninstall);
}

#[test]
fn audit_p0_kimi_paired_block_roundtrip_is_idempotent_and_preserves_user_config() {
    assert_kimi_audit_roundtrip("\n");
}

#[test]
fn audit_p0_kimi_paired_block_roundtrip_preserves_crlf_and_comments() {
    assert_kimi_audit_roundtrip("\r\n");
}

fn assert_kimi_audit_config_rejected(content: &str, run: impl Fn() -> std::io::Result<()>) {
    let _lock = crate::config::test_config_env_lock().lock().unwrap();
    std::env::remove_var(KIMI_CODE_HOME_ENV_VAR);
    for existing_hooks in [false, true] {
        let dirs = crate::config::test_dirs::isolate_dirs("audit-kimi-candidate");
        let dir = dirs.home_dir().join(".kimi-code");
        fs::create_dir_all(&dir).unwrap();
        assert_eq!(kimi_dir().unwrap(), dir);
        let config_path = dir.join("config.toml");
        fs::write(&config_path, content).unwrap();
        if existing_hooks {
            let hooks = dir.join("hooks");
            fs::create_dir_all(&hooks).unwrap();
            for name in ["herdr-agent-state.sh", "herdr-agent-state.ps1"] {
                fs::write(
                    hooks.join(name),
                    format!("# HERDR_INTEGRATION_ID=kimi\n# previous asset: {name}\n"),
                )
                .unwrap();
            }
        }
        let root = dirs.home_dir().parent().unwrap();
        let before = kimi_audit_snapshot(root);
        let result = run();
        assert_eq!(kimi_audit_snapshot(root), before);
        let error = result.expect_err("unsafe config must be rejected before writes");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error
            .to_string()
            .contains(&config_path.display().to_string()));
        assert!(!format!("{error:?}").contains("audit-kimi-private-token"));
    }
}

#[test]
fn audit_p0_kimi_invalid_or_unsafe_candidates_are_rejected_without_leaking_config() {
    let begin = KIMI_CONFIG_BLOCK_BEGIN;
    let end = KIMI_CONFIG_BLOCK_END;
    for content in [
        format!("values = [\n{begin}\n1\n{end}\n, 2\n]\n"),
        format!("{begin}\n[models.user]\ntoken = 'audit-kimi-private-token'\n{end}\n"),
        format!("[[hooks]]\nevent = 'Notification'\n{begin}\ncommand = 'audit-kimi-private-token'\n{end}\n"),
        format!("{begin}\n[[hooks]]\nevent = 'Stop'\ncommand = 'echo old'\n{end}\n[hooks.user]\ntoken = 'audit-kimi-private-token'\n"),
    ] {
        toml::from_str::<toml::Table>(&content).expect("source TOML is valid before the edit");
        assert_kimi_audit_config_rejected(&content, || install_kimi().map(|_| ()));
        assert_kimi_audit_config_rejected(&content, || uninstall_kimi().map(|_| ()));
    }
    let invalid = "token = 'audit-kimi-private-token'\ninvalid = ['audit-kimi-private-token'\n";
    assert!(toml::from_str::<toml::Table>(invalid).is_err());
    assert_kimi_audit_config_rejected(invalid, || install_kimi().map(|_| ()));
    assert_kimi_audit_config_rejected(invalid, || uninstall_kimi().map(|_| ()));
}

#[test]
fn audit_p0_kimi_install_rejects_conflicting_hook_shapes_before_assets() {
    for content in [
        "hooks = []\n",
        "hooks = [{ event = 'Notification', command = 'audit-kimi-private-token' }]\n",
        "hooks = 'audit-kimi-private-token'\n",
        "[hooks]\ncommand = 'audit-kimi-private-token'\n",
    ] {
        toml::from_str::<toml::Table>(content).unwrap();
        assert_kimi_audit_config_rejected(content, || install_kimi().map(|_| ()));
    }
}

#[test]
fn audit_p0_kimi_unmarked_configs_preserve_whitespace_and_remain_idempotent() {
    let _lock = crate::config::test_config_env_lock().lock().unwrap();
    std::env::remove_var(KIMI_CODE_HOME_ENV_VAR);
    for (case_index, content) in [
        "",
        "# user comments only\r\n\r\n",
        "default_model = 'moonshot'\r\n# 用户注释\r\n\r\n\r\n",
        "custom = { score = nan, nested = [+nan, -nan] }\n",
    ]
    .into_iter()
    .enumerate()
    {
        let dirs = crate::config::test_dirs::isolate_dirs("audit-kimi-unmarked");
        let dir = dirs.home_dir().join(".kimi-code");
        fs::create_dir_all(&dir).unwrap();
        assert_eq!(kimi_dir().unwrap(), dir);
        let config_path = dir.join("config.toml");
        fs::write(&config_path, content).unwrap();
        let root = dirs.home_dir().parent().unwrap();
        let snapshot = |phase| kimi_audit_snapshot_for(root, case_index, phase);
        let before = snapshot("before-uninstall");
        let absent = uninstall_kimi().unwrap();
        assert!(!absent.updated_config);
        assert!(!absent.removed_hook_file);
        assert_eq!(snapshot("after-absent-uninstall"), before);

        install_kimi().unwrap();
        let first = snapshot("after-install");
        install_kimi().unwrap();
        assert_eq!(snapshot("after-repeated-install"), first);
        let removed = uninstall_kimi().unwrap();
        assert!(removed.updated_config);
        assert!(removed.removed_hook_file);
        assert_eq!(fs::read(&config_path).unwrap(), content.as_bytes());
        let after = snapshot("after-uninstall");
        let repeated = uninstall_kimi().unwrap();
        assert!(!repeated.updated_config);
        assert!(!repeated.removed_hook_file);
        assert_eq!(snapshot("after-repeated-uninstall"), after);
    }

    let content = "default_model = 'moonshot' # no final newline";
    let hook_path = Path::new("hooks").join(KIMI_HOOK_INSTALL_NAME);
    assert_eq!(
        super::config_edit::remove_kimi_config_block(content).unwrap(),
        content
    );
    let installed = super::config_edit::build_kimi_config_with_hooks(content, &hook_path).unwrap();
    assert!(installed.starts_with(content));
    assert_eq!(
        super::config_edit::build_kimi_config_with_hooks(&installed, &hook_path).unwrap(),
        installed
    );
    let removed = super::config_edit::remove_kimi_config_block(&installed).unwrap();
    assert_eq!(
        toml::from_str::<toml::Table>(&removed).unwrap(),
        toml::from_str::<toml::Table>(content).unwrap()
    );
}

#[test]
fn audit_p0_kimi_uninstall_missing_config_creates_no_files_or_directories() {
    let _lock = crate::config::test_config_env_lock().lock().unwrap();
    std::env::remove_var(KIMI_CODE_HOME_ENV_VAR);
    for directory_exists in [false, true] {
        let dirs = crate::config::test_dirs::isolate_dirs("audit-kimi-missing");
        let dir = dirs.home_dir().join(".kimi-code");
        if directory_exists {
            fs::create_dir_all(&dir).unwrap();
        }
        assert_eq!(kimi_dir().unwrap(), dir);
        let root = dirs.home_dir().parent().unwrap();
        let before = kimi_audit_snapshot(root);
        for _ in 0..2 {
            let result = uninstall_kimi().unwrap();
            assert!(!result.updated_config);
            assert!(!result.removed_hook_file);
            assert!(!result.config_path.exists());
            assert_eq!(kimi_audit_snapshot(root), before);
        }
    }
}

#[test]
fn install_opencode_writes_server_and_tui_plugins() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let opencode_dir = home.join(".config/opencode");
    fs::create_dir_all(&opencode_dir).unwrap();
    let _home = override_home_dir(&home);

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
    let _home = override_home_dir(&home);
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
    let _home = override_home_dir(&home);

    let installed = install_opencode().unwrap();

    assert!(installed.cli_config_path.is_none());
    assert!(!opencode_dir.join("cli.json").exists());
    assert!(opencode_dir
        .join(OPENCODE_V2_TUI_PLUGIN_DIR)
        .join("tui.js")
        .is_file());

    let _ = fs::remove_dir_all(base);
}

#[test]
fn opencode_v2_install_status_and_uninstall_preserve_cli_preferences() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let dir = home.join(".config/opencode");
    fs::create_dir_all(&dir).unwrap();
    let _home = override_home_dir(&home);
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
    let _ = fs::remove_dir_all(base);
}

#[test]
fn opencode_hard_link_rejection_precedes_install_and_uninstall_asset_changes() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let dir = home.join(".config/opencode");
    fs::create_dir_all(dir.join("plugins")).unwrap();
    let _home = override_home_dir(&home);
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
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn opencode_json_config_validation_precedes_asset_changes() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let dir = home.join(".config/opencode");
    fs::create_dir_all(dir.join("plugins")).unwrap();
    let _home = override_home_dir(&home);
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
    let _home = override_home_dir(&home);
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
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn opencode_invalid_cli_config_does_not_overwrite_existing_plugins() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let dir = home.join(".config/opencode");
    fs::create_dir_all(dir.join("plugins")).unwrap();
    let _home = override_home_dir(&home);
    let plugin = dir.join("plugins").join(OPENCODE_PLUGIN_INSTALL_NAME);
    fs::write(&plugin, "previous integration").unwrap();
    fs::write(dir.join("cli.json"), r#"{"plugins":{}}"#).unwrap();
    assert!(install_opencode().is_err());
    assert_eq!(fs::read_to_string(plugin).unwrap(), "previous integration");
    assert!(!dir.join("tui.jsonc").exists());
    let _ = fs::remove_dir_all(base);
}

#[test]
fn opencode_status_requires_the_tui_plugin_and_config_entry() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let opencode_dir = home.join(".config/opencode");
    fs::create_dir_all(&opencode_dir).unwrap();
    let _home = override_home_dir(&home);
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

    let _ = fs::remove_dir_all(base);
}

#[test]
fn uninstall_opencode_removes_plugins_and_managed_tui_config_entry() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    let opencode_dir = home.join(".config/opencode");
    fs::create_dir_all(&opencode_dir).unwrap();
    let _home = override_home_dir(&home);
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
    let _home = override_home_dir(&home);

    let err = install_opencode().unwrap_err().to_string();

    assert!(err.contains("plugin list"));
    assert!(!opencode_dir
        .join("plugins")
        .join(OPENCODE_PLUGIN_INSTALL_NAME)
        .exists());
    assert!(!opencode_dir.join(OPENCODE_TUI_PLUGIN_INSTALL_NAME).exists());

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
    let _home = override_home_dir(&home);

    let err = uninstall_opencode().unwrap_err().to_string();

    assert!(err.contains("failed to parse OpenCode TUI config"));
    assert!(!plugin_path.exists());
    assert!(!tui_plugin_path.exists());
    assert_eq!(
        serde_json::from_str::<Value>(&fs::read_to_string(json_path).unwrap()).unwrap(),
        json!({"plugin":["other"]})
    );

    let _ = fs::remove_dir_all(base);
}

#[test]
fn install_opencode_errors_when_config_dir_missing() {
    let _lock = integration_env_lock();
    let base = unique_base();
    let home = base.join("home");
    fs::create_dir_all(&home).unwrap();
    let _home = override_home_dir(&home);

    let err = install_opencode().unwrap_err().to_string();

    assert!(err.contains("opencode config directory not found"));

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
