#![cfg(unix)]

use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

fn closed_pipe_writer() -> Stdio {
    let mut fds = [-1; 2];
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    // SAFETY: pipe initialized both descriptors, and each is assigned to one
    // OwnedFd so it is closed exactly once.
    let read_fd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let write_fd = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    drop(read_fd);
    Stdio::from(write_fd)
}

fn run_with_closed_stdout(args: &[&str]) -> Output {
    // 配置/状态目录指向本次调用专属的临时目录：不设时 CLI 会读开发机真实的 herdr 目录。
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let root = std::env::temp_dir().join(format!(
        "herdr-broken-pipe-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let output = Command::new(env!("CARGO_BIN_EXE_herdr"))
        .args(args)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_STATE_HOME", root.join("state"))
        .stdout(closed_pipe_writer())
        .stderr(Stdio::piped())
        .output()
        .expect("run herdr CLI");
    let _ = std::fs::remove_dir_all(&root);
    output
}

fn assert_quiet_sigpipe(output: Output) {
    assert_eq!(output.status.signal(), Some(libc::SIGPIPE));
    assert!(
        output.stderr.is_empty(),
        "closed stdout should not emit an error or panic: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn cli_exits_via_sigpipe_without_panicking_when_stdout_closes() {
    assert_quiet_sigpipe(run_with_closed_stdout(&["api", "schema", "--json"]));
}

#[test]
fn terminal_help_uses_cli_sigpipe_behavior_before_attach_starts() {
    assert_quiet_sigpipe(run_with_closed_stdout(&["terminal", "attach", "--help"]));
}

#[test]
fn completion_direct_writer_uses_cli_sigpipe_behavior() {
    assert_quiet_sigpipe(run_with_closed_stdout(&["completion", "bash"]));
}
