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
