//! Synthetic executables only: never consult Codex credentials or sessions.

use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

struct Sandbox(PathBuf);

impl Sandbox {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "herdr-codex-launch-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        std::fs::create_dir(path.join("bin")).unwrap();
        std::fs::create_dir(path.join("home")).unwrap();
        let executable = path.join("bin/codex");
        std::fs::write(
            &executable,
            r#"#!/usr/bin/python3
import os, sys, time
if sys.argv[1:] == ['--help']:
    with open(os.environ['PROBES'], 'ab') as file: file.write(b'probe\n')
    mode = os.environ.get('MOCK', 'new')
    if mode == 'hang': time.sleep(30)
    if mode == 'inherited':
        if os.fork() == 0: time.sleep(30)
        sys.exit(0)
    if mode == 'large': print('x' * 70000); sys.exit(0)
    if mode == 'failed': print('--no-daemon'); sys.exit(2)
    if mode == 'old': print('--no-daemonize'); sys.exit(0)
    print('Options:\n  --no-daemon  Run locally')
    sys.exit(0)
with open(os.environ['RUNS'], 'ab') as file: file.write(b'run\n')
sys.stdout.buffer.write(b'\0'.join(os.fsencode(a) for a in sys.argv[1:]) + b'\0')
sys.stderr.write('mock cwd=' + os.getcwd() + '\n')
sys.stderr.write('mock pid=' + str(os.getpid()) + '\n')
if 'INPUT_FILE' in os.environ:
    with open(os.environ['INPUT_FILE'], 'wb') as file: file.write(sys.stdin.buffer.read())
if 'PANE_REPORT' in os.environ:
    with open(os.environ['PANE_REPORT'], 'w') as file:
        file.write(os.environ['HERDR_PANE_ID'] if '--no-daemon' in sys.argv else 'shared-daemon-pane')
sys.exit(int(os.environ.get('EXIT_CODE', '0')))
"#,
        )
        .unwrap();
        std::fs::set_permissions(executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self(path)
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_herdr"));
        command
            .arg("--internal-codex-launch")
            .env_clear()
            .env("HOME", self.0.join("home"))
            .env("CODEX_HOME", self.0.join("home/codex"))
            .env("PATH", self.0.join("bin"))
            .env("HERDR_ENV", "1")
            .env("HERDR_PANE_ID", "pane:test")
            .env("RUNS", self.0.join("runs"))
            .env("PROBES", self.0.join("probes"))
            .current_dir(&self.0)
            .stdin(Stdio::null());
        command
    }

    fn run(&self, args: &[&str], mode: &str) -> Output {
        self.command()
            .args(args)
            .env("MOCK", mode)
            .output()
            .unwrap()
    }

    fn runs(&self) -> usize {
        std::fs::read_to_string(self.0.join("runs"))
            .unwrap_or_default()
            .lines()
            .count()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn args(output: &Output) -> Vec<OsString> {
    output
        .stdout
        .strip_suffix(&[0])
        .unwrap_or(&output.stdout)
        .split(|byte| *byte == 0)
        .map(|bytes| OsString::from_vec(bytes.to_vec()))
        .collect()
}

#[test]
fn codex_launcher_preserves_native_argv_cwd_stdio_and_exit_code() {
    let sandbox = Sandbox::new();
    let values = [
        OsString::from("resume"),
        OsString::from("id ' \" $() ;"),
        OsString::from_vec(vec![0xff, b'x']),
    ];
    let output = sandbox
        .command()
        .args(&values)
        .env("EXIT_CODE", "23")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(23));
    assert_eq!(
        args(&output),
        [vec![OsString::from("--no-daemon")], values.to_vec()].concat()
    );
    assert!(output
        .stderr
        .windows(sandbox.0.as_os_str().as_bytes().len())
        .any(|bytes| bytes == sandbox.0.as_os_str().as_bytes()));
    assert_eq!(sandbox.runs(), 1);
}

#[test]
fn codex_launcher_old_and_noninteractive_and_remote_are_passthrough() {
    for values in [
        vec!["exec", "x"],
        vec!["review"],
        vec!["app-server"],
        vec!["mcp-server"],
        vec!["login"],
        vec!["agents"],
        vec!["queue"],
        vec!["resume", "id", "--remote=x"],
        vec!["--no-daemon", "resume", "id"],
    ] {
        let sandbox = Sandbox::new();
        let output = sandbox.run(&values, "new");
        assert!(output.status.success());
        assert_eq!(
            args(&output),
            values.iter().map(OsString::from).collect::<Vec<_>>()
        );
        assert!(!sandbox.0.join("probes").exists());
    }
    let sandbox = Sandbox::new();
    assert_eq!(
        args(&sandbox.run(&["resume", "id"], "old")),
        ["resume", "id"]
    );
}

#[test]
fn codex_launcher_0157_all_noninteractive_commands_are_passthrough() {
    // Independent CLI contract from rust-v0.157.0 cli/src/main.rs::Subcommand,
    // including hidden commands, platform-specific App, and aliases e/a/cloud-tasks.
    // mcp-server is retained for older Codex; help is Clap's generated command.
    let commands = [
        "agents",
        "tcp-tunnel",
        "exec",
        "e",
        "review",
        "login",
        "logout",
        "mcp",
        "plugin",
        "app-server",
        "remote-control",
        "app",
        "completion",
        "update",
        "doctor",
        "sandbox",
        "debug",
        "execpolicy",
        "apply",
        "a",
        "queue",
        "archive",
        "delete",
        "migrate-rollouts",
        "unarchive",
        "cloud",
        "cloud-tasks",
        "responses-api-proxy",
        "stdio-to-uds",
        "exec-server",
        "features",
        "mcp-server",
        "help",
    ];
    let mut failures = Vec::new();
    for command in commands {
        for prefix in [vec![], vec!["-c", "test=value"], vec!["--model=mock-model"]] {
            let sandbox = Sandbox::new();
            let values = [prefix, vec![command]].concat();
            let output = sandbox.run(&values, "new");
            let expected = values.iter().map(OsString::from).collect::<Vec<_>>();
            if !output.status.success()
                || args(&output) != expected
                || sandbox.0.join("probes").exists()
            {
                failures.push(format!("{values:?}"));
            }
            assert_eq!(sandbox.runs(), 1);
        }
    }
    assert!(
        failures.is_empty(),
        "noninteractive commands were probed or changed: {failures:?}"
    );
}

#[test]
fn codex_launcher_command_words_in_prompt_or_option_values_stay_interactive() {
    for values in [
        vec!["--", "archive"],
        vec!["--", "--internal-codex-launch"],
        vec!["help me archive this project"],
        vec!["-m", "doctor", "prompt"],
        vec!["-c", "alias=exec-server", "prompt"],
        vec!["resume", "archive"],
        vec!["fork", "delete"],
    ] {
        let sandbox = Sandbox::new();
        let output = sandbox.run(&values, "new");
        let expected = [vec!["--no-daemon"], values].concat();
        assert!(output.status.success());
        assert_eq!(
            args(&output),
            expected.iter().map(OsString::from).collect::<Vec<_>>()
        );
        assert_eq!(sandbox.runs(), 1);
    }
}

#[test]
fn codex_launcher_probe_failures_are_bounded_and_launch_only_once() {
    for mode in ["failed", "large", "hang", "inherited"] {
        let sandbox = Sandbox::new();
        let started = Instant::now();
        let output = sandbox.run(&["resume", "id"], mode);
        assert!(started.elapsed() < Duration::from_secs(6), "{mode}");
        assert!(output.status.success(), "{mode}");
        assert_eq!(args(&output), ["resume", "id"]);
        assert!(String::from_utf8_lossy(&output.stderr).contains("isolation is unknown"));
        assert_eq!(sandbox.runs(), 1);
    }
}

#[test]
fn codex_launcher_outside_pane_does_not_probe_or_modify_args() {
    for marker in ["HERDR_ENV", "HERDR_PANE_ID"] {
        let sandbox = Sandbox::new();
        let output = sandbox
            .command()
            .env_remove(marker)
            .arg("prompt")
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(args(&output), ["prompt"]);
        assert!(!sandbox.0.join("probes").exists());
    }
}

#[test]
fn codex_launcher_argv0_shim_skips_itself_and_resolves_current_path() {
    let sandbox = Sandbox::new();
    let shim = sandbox.0.join("shim");
    std::fs::create_dir(&shim).unwrap();
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_herdr"), shim.join("codex")).unwrap();
    let mut command = sandbox.command();
    // Command::new sets argv0 from its executable; use a separate command to
    // actually enter through the symlink rather than the internal CLI flag.
    let mut shim_command = Command::new(shim.join("codex"));
    shim_command
        .env_clear()
        .envs(
            command
                .get_envs()
                .filter_map(|(key, value)| value.map(|value| (key, value))),
        )
        .current_dir(&sandbox.0)
        .env(
            "PATH",
            std::env::join_paths([&shim, &sandbox.0.join("bin")]).unwrap(),
        )
        .arg("fork")
        .arg("id");
    let output = shim_command.output().unwrap();
    assert!(output.status.success());
    assert_eq!(args(&output), ["--no-daemon", "fork", "id"]);
    command.env("PATH", &shim);
    let failed = command.output().unwrap();
    assert!(!failed.status.success());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("not found outside"));
    assert_eq!(sandbox.runs(), 1);
}

#[test]
fn codex_launcher_rejects_probe_wrapper_recursion() {
    let sandbox = Sandbox::new();
    let output = sandbox
        .command()
        .env("HERDR_CODEX_LAUNCH_ACTIVE", "1")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(sandbox.runs(), 0);
    assert!(String::from_utf8_lossy(&output.stderr).contains("recursive Codex"));
}

#[test]
fn codex_launcher_exec_keeps_process_identity_and_stdin() {
    use std::io::Write;
    let sandbox = Sandbox::new();
    let input = sandbox.0.join("input");
    let mut child = sandbox
        .command()
        .arg("prompt")
        .env("INPUT_FILE", &input)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"opaque stdin\xff\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains(&format!("mock pid={pid}\n")));
    assert_eq!(std::fs::read(input).unwrap(), b"opaque stdin\xff\n");
}

#[test]
fn codex_launcher_two_panes_do_not_reuse_simulated_daemon_environment() {
    let sandbox = Sandbox::new();
    for pane in ["pane:first", "pane:second"] {
        let report = sandbox.0.join("report");
        let output = sandbox
            .command()
            .env("HERDR_PANE_ID", pane)
            .env("PANE_REPORT", &report)
            .arg("prompt")
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(std::fs::read_to_string(report).unwrap(), pane);
    }
    assert_eq!(sandbox.runs(), 2);
}
