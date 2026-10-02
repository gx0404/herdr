use super::*;

fn argv(values: &[&str]) -> Vec<OsString> {
    values.iter().map(OsString::from).collect()
}

#[test]
fn codex_interactive_policy_keeps_subcommands_and_remote_unchanged() {
    for values in [
        vec![],
        vec!["resume", "id"],
        vec!["fork", "id"],
        vec!["-c", "key=review", "resume", "id"],
        vec!["--model=x", "prompt"],
        vec!["--", "--remote"],
        vec!["a prompt", "review"],
        vec!["resume", "review"],
    ] {
        assert!(interactive(&argv(&values)), "{values:?}");
    }
    for values in [
        vec!["exec"],
        vec!["e"],
        vec!["review"],
        vec!["app-server"],
        vec!["mcp-server"],
        vec!["login"],
        vec!["agents"],
        vec!["queue"],
        vec!["resume", "id", "--remote", "address"],
        vec!["--remote=address"],
        vec!["--no-daemon"],
        vec!["fork", "id", "--no-daemon"],
        vec!["--unknown-option", "value"],
        vec!["-c", "key=value", "exec"],
        vec!["--help"],
        vec!["resume", "--help"],
    ] {
        assert!(!interactive(&argv(&values)), "{values:?}");
    }
}

#[test]
fn codex_capability_matches_complete_help_flag() {
    assert!(help_has_flag(b"Options:\n  --no-daemon  Run locally\n"));
    assert!(help_has_flag(b"[--no-daemon]"));
    assert!(!help_has_flag(
        b"--no-daemonize --no-daemon=false --other-no-daemon"
    ));
}

#[test]
fn codex_managed_argv_does_not_modify_saved_plan() {
    let original = vec!["codex".into(), "resume".into(), "session-id".into()];
    let managed = managed_argv(&original).unwrap();
    assert_eq!(&managed[1..], &[ENTRY, "resume", "session-id"]);
    assert_eq!(original, ["codex", "resume", "session-id"]);
    assert_eq!(managed_argv(&["claude".into()]).unwrap(), ["claude"]);
}

#[test]
fn codex_pane_environment_requires_both_markers() {
    for (herdr, pane) in [(false, false), (true, false), (false, true)] {
        let mut command = portable_pty::CommandBuilder::new("sh");
        command.env_clear();
        command.env("PATH", "/test/path");
        if herdr {
            command.env(crate::HERDR_ENV_VAR, crate::HERDR_ENV_VALUE);
        }
        if pane {
            command.env(super::super::HERDR_PANE_ID_ENV_VAR, "pane:1");
        }
        command.env(ACTIVE, "1");
        apply_pane_env(&mut command);
        assert_eq!(command.get_env("PATH"), Some(OsStr::new("/test/path")));
        assert!(command.get_env(SHIM_DIR).is_none());
        assert!(command.get_env(ACTIVE).is_none());
    }
}

#[test]
fn codex_pane_shim_precedes_path_and_can_be_resolved_without_recursion() {
    let mut command = portable_pty::CommandBuilder::new("sh");
    command.env_clear();
    command.env("PATH", "/test/path");
    command.env(crate::HERDR_ENV_VAR, crate::HERDR_ENV_VALUE);
    command.env(super::super::HERDR_PANE_ID_ENV_VAR, "pane:1");
    apply_pane_env(&mut command);
    let path = command.get_env("PATH").unwrap();
    let directory = Path::new(command.get_env(SHIM_DIR).unwrap());
    assert_eq!(
        std::env::split_paths(path).next().as_deref(),
        Some(directory)
    );
    assert_eq!(path_without_shims(path).unwrap(), OsStr::new("/test/path"));
    assert_eq!(
        resolve_executable(path, Some(directory), &std::env::current_exe().unwrap())
            .unwrap_err()
            .kind(),
        io::ErrorKind::NotFound
    );
}

#[test]
fn codex_pane_shim_is_the_shared_one_of_the_unit_test_base() {
    let mut command = portable_pty::CommandBuilder::new("sh");
    command.env_clear();
    command.env(crate::HERDR_ENV_VAR, crate::HERDR_ENV_VALUE);
    command.env(super::super::HERDR_PANE_ID_ENV_VAR, "pane:1");
    apply_pane_env(&mut command);
    let directory = Path::new(command.get_env(SHIM_DIR).unwrap());
    // Test processes share one shim per test binary there, never a per-process copy in the
    // temp root or a production shim.
    assert_eq!(
        directory.parent().and_then(Path::file_name),
        Some(OsStr::new("herdr-unit-codex-shim"))
    );
    assert!(crate::platform::codex_launch::is_shim(
        &directory.join("codex")
    ));
}

#[cfg(windows)]
#[test]
fn codex_native_tools_survive_shim_cleanup_and_preserve_command() {
    let _lock = crate::config::test_config_env_lock();
    let dirs = crate::config::test_dirs::isolate_dirs("codex-child-tools");
    let own = std::env::current_exe().unwrap();
    let shim = crate::platform::codex_launch::install_shim(&own).unwrap();
    let real = dirs.state_dir().join("real tools");
    let tail = dirs.state_dir().join("tail");
    let original = std::env::join_paths([&real, &shim, &tail, &real]).unwrap();
    let cleaned = path_without_shims(&original).unwrap();
    let process_path = std::env::var_os("PATH");
    for values in [
        vec!["resume", "中文 session"],
        vec!["update"],
        vec!["exec", "two words"],
    ] {
        let args = argv(&values);
        let mut command = crate::platform::codex_launch::command(&own, &args).unwrap();
        command.env("PATH", &cleaned).env_remove(ACTIVE);
        crate::platform::codex_launch::configure_child(&mut command).unwrap();
        let child_path = |command: &std::process::Command| {
            command
                .get_envs()
                .find(|(key, _)| *key == "PATH")
                .and_then(|(_, value)| value)
                .unwrap()
                .to_os_string()
        };
        let configured = child_path(&command);
        let paths: Vec<_> = std::env::split_paths(&configured).collect();
        assert_eq!(
            paths,
            [
                shim.join("native-tools"),
                real.clone(),
                tail.clone(),
                real.clone()
            ]
        );
        assert!(paths[0].join("tar.cmd").is_file());
        assert!(!crate::platform::codex_launch::is_shim(
            &paths[0].join("codex")
        ));
        assert_eq!(path_without_shims(&configured).unwrap(), configured);
        crate::platform::codex_launch::configure_child(&mut command).unwrap();
        assert_eq!(child_path(&command), configured);
        assert_eq!(command.get_program(), own.as_os_str());
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            args.iter().map(OsString::as_os_str).collect::<Vec<_>>()
        );
        assert!(command
            .get_envs()
            .any(|(key, value)| key == ACTIVE && value.is_none()));
    }
    assert_eq!(std::env::var_os("PATH"), process_path);
}

#[cfg(not(windows))]
#[test]
fn codex_native_tools_leave_non_windows_commands_unchanged() {
    let mut command = std::process::Command::new("codex");
    command.arg("update").env("PATH", "/custom/tools:/usr/bin");
    let original = format!("{command:?}");
    crate::platform::codex_launch::configure_child(&mut command).unwrap();
    assert_eq!(format!("{command:?}"), original);
}
