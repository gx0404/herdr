pub(super) use std::fs;
pub(super) use std::io::{BufRead, BufReader, Write};
pub(super) use std::os::unix::net::{UnixListener, UnixStream};
pub(super) use std::path::{Path, PathBuf};
pub(super) use std::process::{Command, Stdio};
pub(super) use std::thread;
pub(super) use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub(super) use crate::support::{
    cleanup_test_base, register_runtime_dir, register_spawned_herdr_pid,
    unregister_spawned_herdr_pid, CURRENT_PROTOCOL,
};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

pub(super) const WORKTREE_BOOTSTRAP_MANAGED_COMPONENT: &str =
    "example.worktree-bootstrap-ef876653ffc3";

pub(super) fn unique_test_dir() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    PathBuf::from(format!("/tmp/hcli-{}-{nanos}", std::process::id()))
}

/// 等「终会成立」的条件用的与负载无关的宽上限（N21，做法同 `93621e2c`）：负载
/// 25–35 时起进程、线程调度都可能被拖慢好几秒，固定的短时限会误报。条件一满足
/// 立即往下走，只在真的失败时才等满。
pub(super) const LOADED_WAIT: Duration = Duration::from_secs(30);

/// 设成非空且不是 `0` 时，[`TestDirGuard`] 不删测试目录、只在 stderr 报路径，
/// 方便用例失败后进去看现场。
pub(super) const KEEP_TEST_DIRS_ENV: &str = "HERDR_TEST_KEEP_DIRS";

/// 测试目录守卫：离开作用域时（含断言失败的 panic 展开）删掉整个测试目录。
/// 给不拉起 server 的用例用——它们只经 `run_named_cli*` 顺手建出
/// `runtime/home`，以前没有收尾，每次全量都在 /tmp 留一个空目录（N21）。拉起
/// server 的用例仍走 `cleanup_test_base`：要先按 runtime 目录收掉 server 再删。
pub(super) struct TestDirGuard {
    path: PathBuf,
    keep: bool,
}

impl TestDirGuard {
    pub(super) fn new(path: &Path) -> Self {
        let keep = keep_test_dirs(std::env::var_os(KEEP_TEST_DIRS_ENV).as_deref());
        Self::with_keep(path, keep)
    }

    fn with_keep(path: &Path, keep: bool) -> Self {
        Self {
            path: path.to_path_buf(),
            keep,
        }
    }
}

impl Drop for TestDirGuard {
    fn drop(&mut self) {
        if self.keep {
            eprintln!(
                "{KEEP_TEST_DIRS_ENV} is set; keeping test dir {}",
                self.path.display()
            );
            return;
        }
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn keep_test_dirs(value: Option<&std::ffi::OsStr>) -> bool {
    value.is_some_and(|value| !value.is_empty() && value != "0")
}

/// 写 insteadOf 离线重定向配置并返回 GIT_CONFIG_GLOBAL 路径。
/// git >= 2.32 读 GIT_CONFIG_GLOBAL；更老的 git（如 Ubuntu 20.04 的 2.25）忽略该
/// 变量、改读 HOME/.gitconfig，因此同内容双写，调用方须把 HOME 指向测试目录。
pub(super) fn write_offline_git_config(
    base: &Path,
    source_repo: &Path,
    remote_url: &str,
) -> PathBuf {
    let content = format!(
        "[url \"file://{}\"]\n    insteadOf = {remote_url}\n",
        source_repo.display()
    );
    let git_config = base.join("gitconfig");
    fs::write(&git_config, &content).unwrap();
    fs::write(base.join(".gitconfig"), &content).unwrap();
    git_config
}

pub(super) fn managed_github_plugin_dir(config_home: &Path) -> PathBuf {
    config_home.join("herdr-dev").join("plugins").join("github")
}

pub(super) fn path_missing_or_empty(path: &Path) -> bool {
    match fs::read_dir(path) {
        Ok(mut entries) => entries.next().is_none(),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => true,
        Err(err) => panic!("failed to read {}: {err}", path.display()),
    }
}

pub(super) fn run_git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .status()
        .unwrap();
    assert!(
        status.success(),
        "git command failed: git -C {} {}",
        repo.display(),
        args.join(" ")
    );
}

pub(super) fn create_committed_repo(path: &Path) {
    fs::create_dir_all(path).unwrap();
    run_git(path, &["init", "--quiet"]);
    run_git(path, &["config", "user.email", "herdr@example.invalid"]);
    run_git(path, &["config", "user.name", "Herdr Test"]);
    fs::write(path.join("README.md"), "test\n").unwrap();
    run_git(path, &["add", "README.md"]);
    run_git(path, &["commit", "--quiet", "-m", "initial"]);
}

pub(super) struct SpawnedHerdr {
    _master: Box<dyn MasterPty + Send>,
    pub(super) child: Box<dyn Child + Send + Sync>,
}

pub(super) struct SpawnedServerProcess {
    child: std::process::Child,
}

impl Drop for SpawnedServerProcess {
    fn drop(&mut self) {
        let pid = self.child.id();
        let _ = self.child.kill();
        let _ = self.child.wait();
        unregister_spawned_herdr_pid(Some(pid));
    }
}

impl Drop for SpawnedHerdr {
    fn drop(&mut self) {
        let pid = self.child.process_id();
        let _ = self.child.kill();

        if let Some(pid) = pid {
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                let mut status = 0;
                let result =
                    unsafe { libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG) };
                if result == pid as libc::pid_t || result == -1 {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }

            unregister_spawned_herdr_pid(Some(pid));
        }
    }
}

pub(super) fn cleanup_spawned_herdr(spawned: SpawnedHerdr, base: PathBuf) {
    drop(spawned);
    cleanup_test_base(&base);
}

pub(super) fn wait_for_socket(path: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() && std::os::unix::net::UnixStream::connect(path).is_ok() {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("socket did not appear at {}", path.display());
}

pub(super) fn spawn_herdr(
    config_home: &Path,
    runtime_dir: &Path,
    socket_path: &Path,
) -> SpawnedHerdr {
    spawn_herdr_with_config(
        config_home,
        runtime_dir,
        socket_path,
        None,
        "onboarding = false\n",
    )
}

pub(super) fn spawn_herdr_with_pane_history(
    config_home: &Path,
    runtime_dir: &Path,
    socket_path: &Path,
) -> SpawnedHerdr {
    spawn_herdr_with_config(
        config_home,
        runtime_dir,
        socket_path,
        None,
        "onboarding = false\n[experimental]\npane_history = true\n",
    )
}

pub(super) fn app_dir_name() -> &'static str {
    if cfg!(debug_assertions) {
        "herdr-dev"
    } else {
        "herdr"
    }
}

pub(super) fn named_session_socket(config_home: &Path, session: &str) -> PathBuf {
    config_home
        .join(app_dir_name())
        .join("sessions")
        .join(session)
        .join("herdr.sock")
}

pub(super) fn spawn_named_server(
    config_home: &Path,
    runtime_dir: &Path,
    session: &str,
) -> SpawnedServerProcess {
    fs::create_dir_all(config_home.join(app_dir_name())).unwrap();
    fs::create_dir_all(runtime_dir).unwrap();
    register_runtime_dir(runtime_dir);
    fs::write(
        config_home.join(app_dir_name()).join("config.toml"),
        "onboarding = false\n",
    )
    .unwrap();

    // HOME 隔离到测试目录：server 的活动树适配器（zcode 等）会按 HOME 读开发机上
    // 真实的 CLI 数据，把外部会话塞进快照，让用例随开发机状态漂移。
    let home = runtime_dir.join("home");
    let _ = fs::create_dir_all(&home);

    let mut command = Command::new(env!("CARGO_BIN_EXE_herdr"));
    command
        .args(["--session", session, "server"])
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", config_home)
        .env("XDG_RUNTIME_DIR", runtime_dir)
        .env_remove("HERDR_SOCKET_PATH")
        .env_remove("HERDR_CLIENT_SOCKET_PATH")
        .env_remove("HERDR_ENV")
        // 宿主在 herdr 窗格内跑测试时会注入 HERDR_STARTUP_CWD；server 会据此预建启动工作区，破坏用例的零工作区假设。
        .env_remove("HERDR_STARTUP_CWD")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    let child = command.spawn().unwrap();
    register_spawned_herdr_pid(Some(child.id()));
    SpawnedServerProcess { child }
}

pub(super) fn run_named_cli(
    config_home: &Path,
    runtime_dir: &Path,
    args: &[&str],
) -> std::process::Output {
    run_named_cli_with_socket_override(config_home, runtime_dir, args, None)
}

pub(super) fn run_named_cli_with_socket_override(
    config_home: &Path,
    runtime_dir: &Path,
    args: &[&str],
    socket_override: Option<&Path>,
) -> std::process::Output {
    run_named_cli_with_env_and_socket_override(config_home, runtime_dir, args, &[], socket_override)
}

pub(super) fn run_named_cli_with_env(
    config_home: &Path,
    runtime_dir: &Path,
    args: &[&str],
    envs: &[(&str, &Path)],
) -> std::process::Output {
    run_named_cli_with_env_and_socket_override(config_home, runtime_dir, args, envs, None)
}

pub(super) fn run_named_cli_with_env_and_socket_override(
    config_home: &Path,
    runtime_dir: &Path,
    args: &[&str],
    envs: &[(&str, &Path)],
    socket_override: Option<&Path>,
) -> std::process::Output {
    // HOME 隔离到测试目录：server 的活动树适配器（zcode 等）会按 HOME 读开发机上
    // 真实的 CLI 数据，把外部会话塞进快照，让用例随开发机状态漂移。
    let home = runtime_dir.join("home");
    let _ = fs::create_dir_all(&home);

    let mut command = Command::new(env!("CARGO_BIN_EXE_herdr"));
    command
        .args(args)
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", config_home)
        .env("XDG_RUNTIME_DIR", runtime_dir)
        // Human-readable CLI stdout is localized (zh-CN default); pin English
        // so assertions on non-JSON output stay stable. JSON output is
        // byte-identical across languages.
        .env("HERDR_LANG", "en")
        .env_remove("HERDR_CLIENT_SOCKET_PATH")
        .env_remove("HERDR_ENV")
        // 宿主在 herdr 窗格内跑测试时会注入 HERDR_STARTUP_CWD；server 会据此预建
        // 启动工作区，破坏用例的零工作区假设。
        .env_remove("HERDR_STARTUP_CWD");
    for (key, value) in envs {
        command.env(key, value);
    }
    if let Some(socket_override) = socket_override {
        command.env("HERDR_SOCKET_PATH", socket_override);
    } else {
        command.env_remove("HERDR_SOCKET_PATH");
    }
    command.output().unwrap()
}

pub(super) fn run_named_cli_json(
    config_home: &Path,
    runtime_dir: &Path,
    args: &[&str],
) -> serde_json::Value {
    let output = run_named_cli(config_home, runtime_dir, args);
    assert!(
        output.status.success(),
        "command failed: herdr {}\nstatus: {:?}\nstderr: {}\nstdout: {}",
        args.join(" "),
        output.status.code(),
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

pub(super) fn spawn_herdr_with_path(
    config_home: &Path,
    runtime_dir: &Path,
    socket_path: &Path,
    path_override: Option<&Path>,
) -> SpawnedHerdr {
    spawn_herdr_with_config(
        config_home,
        runtime_dir,
        socket_path,
        path_override,
        "onboarding = false\n",
    )
}

pub(super) fn spawn_herdr_with_config(
    config_home: &Path,
    runtime_dir: &Path,
    socket_path: &Path,
    path_override: Option<&Path>,
    config_toml: &str,
) -> SpawnedHerdr {
    fs::create_dir_all(config_home.join(app_dir_name())).unwrap();
    fs::create_dir_all(runtime_dir).unwrap();
    register_runtime_dir(runtime_dir);
    fs::write(
        config_home.join(app_dir_name()).join("config.toml"),
        config_toml,
    )
    .unwrap();

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_herdr"));
    cmd.arg("server");
    // HOME 隔离到测试目录：server 的活动树适配器（zcode 等）会按 HOME 读开发机上
    // 真实的 CLI 数据，把外部会话塞进快照，让用例随开发机状态漂移。
    let home = runtime_dir.join("home");
    let _ = fs::create_dir_all(&home);
    cmd.env("HOME", &home);
    cmd.env("XDG_CONFIG_HOME", config_home);
    cmd.env("XDG_RUNTIME_DIR", runtime_dir);
    cmd.env("HERDR_SOCKET_PATH", socket_path);
    cmd.env_remove("HERDR_CLIENT_SOCKET_PATH");
    cmd.env("SHELL", "/bin/sh");
    cmd.env_remove("HERDR_ENV");
    // 宿主在 herdr 窗格内跑测试时会注入 HERDR_STARTUP_CWD；server 会据此预建
    // 启动工作区，破坏用例的零工作区假设。
    cmd.env_remove("HERDR_STARTUP_CWD");
    if let Some(path) = path_override {
        cmd.env("PATH", path);
    }

    let child = pair.slave.spawn_command(cmd).unwrap();
    register_spawned_herdr_pid(child.process_id());
    SpawnedHerdr {
        _master: pair.master,
        child,
    }
}

pub(super) fn run_cli(socket_path: &Path, args: &[&str]) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_herdr"));
    command.args(args);
    command.env("HERDR_SOCKET_PATH", socket_path);
    // See run_named_cli_with_env_and_socket_override: pin English for the
    // localized human-readable output path.
    command.env("HERDR_LANG", "en");
    command.output().unwrap()
}

pub(super) fn run_cli_in_dir(
    socket_path: &Path,
    args: &[&str],
    current_dir: &Path,
) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_herdr"));
    command.args(args);
    command.current_dir(current_dir);
    command.env("HERDR_SOCKET_PATH", socket_path);
    command.env("HERDR_LANG", "en");
    command.output().unwrap()
}

pub(super) fn pane_topology_snapshot(list_response: &serde_json::Value) -> Vec<serde_json::Value> {
    list_response["result"]["panes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|pane| {
            serde_json::json!({
                "pane_id": pane["pane_id"],
                "terminal_id": pane["terminal_id"],
                "workspace_id": pane["workspace_id"],
                "tab_id": pane["tab_id"],
                "focused": pane["focused"],
            })
        })
        .collect()
}

pub(super) fn run_cli_json(socket_path: &Path, args: &[&str]) -> serde_json::Value {
    let output = run_cli(socket_path, args);
    parse_cli_json_output(args, output)
}

pub(super) fn run_cli_json_in_dir(
    socket_path: &Path,
    args: &[&str],
    current_dir: &Path,
) -> serde_json::Value {
    let output = run_cli_in_dir(socket_path, args, current_dir);
    parse_cli_json_output(args, output)
}

pub(super) fn parse_cli_json_output(
    args: &[&str],
    output: std::process::Output,
) -> serde_json::Value {
    assert!(
        output.status.success(),
        "command failed: herdr {}\nstatus: {:?}\nstderr: {}\nstdout: {}",
        args.join(" "),
        output.status.code(),
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );

    serde_json::from_slice(&output.stdout).unwrap_or_else(|err| {
        panic!(
            "failed to parse JSON response for `herdr {}`: {}\nstdout: {}\nstderr: {}",
            args.join(" "),
            err,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

pub(super) fn wait_until(
    timeout: Duration,
    interval: Duration,
    mut condition: impl FnMut() -> bool,
) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        thread::sleep(interval);
    }
    false
}

pub(super) fn pane_read_recent_contains(socket_path: &Path, pane_id: &str, expected: &str) -> bool {
    let output = run_cli(
        socket_path,
        &["pane", "read", pane_id, "--source", "recent"],
    );
    if !output.status.success() {
        return false;
    }
    String::from_utf8_lossy(&output.stdout).contains(expected)
}

pub(super) fn process_exists(pid: u32) -> bool {
    let result = unsafe { libc::kill(pid as i32, 0) };
    if result == 0 {
        true
    } else {
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

pub(super) fn wait_for_pid_exit(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !process_exists(pid) {
            return true;
        }
        thread::sleep(Duration::from_millis(25));
    }
    !process_exists(pid)
}

/// pid 文件内容要保持不变这么久才算写完。
const STABLE_PID_CONTENT_WINDOW: Duration = Duration::from_millis(250);

pub(super) fn wait_for_pid_file(pid_file: &Path, timeout: Duration) -> Result<u32, String> {
    let deadline = Instant::now() + timeout;
    let mut last_contents = String::new();
    let mut stable_candidate: Option<(String, u32, Instant)> = None;

    while Instant::now() < deadline {
        if let Ok(contents) = fs::read_to_string(pid_file) {
            let trimmed = contents.trim().to_string();
            last_contents = contents;

            if let Ok(pid) = trimmed.parse::<u32>() {
                match &stable_candidate {
                    Some((candidate_text, candidate_pid, stable_since))
                        if candidate_text == &trimmed && *candidate_pid == pid =>
                    {
                        if stable_since.elapsed() >= STABLE_PID_CONTENT_WINDOW {
                            return Ok(pid);
                        }
                    }
                    _ => {
                        stable_candidate = Some((trimmed, pid, Instant::now()));
                    }
                }
            } else {
                stable_candidate = None;
            }
        }

        thread::sleep(Duration::from_millis(25));
    }

    Err(format!(
        "pid file {} did not contain stable parseable pid before timeout; last contents={:?}",
        pid_file.display(),
        last_contents
    ))
}

#[test]
fn wait_for_pid_file_retries_until_pid_is_written() {
    let base = unique_test_dir();
    fs::create_dir_all(&base).unwrap();
    let pid_file = base.join("delayed.pid");
    fs::write(&pid_file, "").unwrap();

    let writer = thread::spawn({
        let pid_file = pid_file.clone();
        move || {
            thread::sleep(Duration::from_millis(100));
            fs::write(pid_file, "424242\n").unwrap();
        }
    });

    let pid = wait_for_pid_file(&pid_file, Duration::from_secs(2)).unwrap();
    assert_eq!(pid, 424242);

    writer.join().unwrap();
    cleanup_test_base(&base);
}

#[test]
fn wait_for_pid_file_errors_when_file_never_contains_pid() {
    let base = unique_test_dir();
    fs::create_dir_all(&base).unwrap();
    let pid_file = base.join("empty.pid");
    fs::write(&pid_file, "").unwrap();

    let err = wait_for_pid_file(&pid_file, Duration::from_millis(150)).unwrap_err();
    assert!(
        err.contains("did not contain stable parseable pid"),
        "unexpected error: {err}"
    );

    cleanup_test_base(&base);
}

#[test]
fn wait_for_pid_file_rejects_unparseable_partial_write_until_stable_contents() {
    let base = unique_test_dir();
    fs::create_dir_all(&base).unwrap();
    let pid_file = base.join("partial-race.pid");
    // 调用前就落盘半截内容：helper 第一次读到的必然是不可解析的 `pid=`。
    fs::write(&pid_file, "pid=").unwrap();

    let writer = thread::spawn({
        let pid_file = pid_file.clone();
        move || {
            thread::sleep(Duration::from_millis(40));
            fs::write(&pid_file, "pid=424242").unwrap();
            thread::sleep(Duration::from_millis(40));
            // 先记时刻再写完整内容：helper 最早也只能在这之后读到可解析的 pid。
            let complete_written_at = Instant::now();
            fs::write(&pid_file, "424242\n").unwrap();
            complete_written_at
        }
    });

    let pid = wait_for_pid_file(&pid_file, LOADED_WAIT).unwrap();
    let returned_at = Instant::now();
    let complete_written_at = writer.join().unwrap();
    assert_eq!(pid, 424242);
    // N21：以前从调用 helper 起量「≥300 ms」，负载下主线程在 spawn 之后被晚调度、
    // 写线程已经写完时，起点落在完整内容之后，量出来不足 300 ms 而误报。改为从完整
    // 内容落盘时刻量起：helper 看到它稳定满一个窗口才返回，与调度快慢无关；若它
    // 接受了半截内容，会在完整内容之前返回，差值为零同样失败。
    let waited = returned_at.saturating_duration_since(complete_written_at);
    assert!(
        waited >= STABLE_PID_CONTENT_WINDOW,
        "helper should wait for stable complete contents, returned {waited:?} after the complete write"
    );

    cleanup_test_base(&base);
}

#[test]
fn test_dir_guard_removes_the_test_dir_unless_asked_to_keep_it() {
    let removed = unique_test_dir();
    fs::create_dir_all(removed.join("runtime").join("home")).unwrap();
    drop(TestDirGuard::with_keep(&removed, false));
    assert!(!removed.exists(), "guard left {}", removed.display());

    let kept = unique_test_dir();
    fs::create_dir_all(kept.join("runtime").join("home")).unwrap();
    drop(TestDirGuard::with_keep(&kept, true));
    assert!(
        kept.join("runtime").join("home").is_dir(),
        "kept dir was removed"
    );
    fs::remove_dir_all(&kept).unwrap();

    assert!(!keep_test_dirs(None));
    assert!(!keep_test_dirs(Some(std::ffi::OsStr::new(""))));
    assert!(!keep_test_dirs(Some(std::ffi::OsStr::new("0"))));
    assert!(keep_test_dirs(Some(std::ffi::OsStr::new("1"))));
}

pub(super) fn send_request(socket_path: &Path, json: &str) -> serde_json::Value {
    let mut stream = UnixStream::connect(socket_path).unwrap();
    stream.write_all(json.as_bytes()).unwrap();
    stream.write_all(b"\n").unwrap();
    stream.flush().unwrap();

    let mut line = String::new();
    let mut reader = BufReader::new(stream);
    reader.read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

pub(super) fn write_fake_pong(
    stream: &mut UnixStream,
    request: &serde_json::Value,
    version: &str,
    protocol: u32,
) {
    writeln!(
        stream,
        "{}",
        serde_json::json!({
            "id": request["id"],
            "result": {
                "type": "pong",
                "version": version,
                "protocol": protocol,
                "capabilities": {
                    "live_handoff": true,
                    "detached_server_daemon": true
                }
            }
        })
    )
    .unwrap();
    stream.flush().unwrap();
}

pub(super) fn accept_fake_cli_operation(listener: &UnixListener) -> (UnixStream, String) {
    loop {
        let (mut stream, _) = listener.accept().unwrap();
        let mut line = String::new();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        reader.read_line(&mut line).unwrap();
        let request: serde_json::Value = serde_json::from_str(&line).unwrap();
        if request["method"] != "ping" {
            return (stream, line);
        }

        write_fake_pong(
            &mut stream,
            &request,
            "different-build-same-protocol",
            CURRENT_PROTOCOL,
        );
    }
}
