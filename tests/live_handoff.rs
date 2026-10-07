#![cfg(unix)]

pub mod support;

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use support::{
    app_dir_name, cleanup_test_base, client_shell_handshake, register_runtime_dir,
    register_spawned_herdr_pid, send_client_shell_shift_enter, unregister_spawned_herdr_pid,
    wait_for_client_shell_bootstrap, wait_for_message_variant, wait_for_socket,
    INHERITED_DIR_OVERRIDES, SERVER_MESSAGE_ENDPOINT_CONTROL, SERVER_MESSAGE_SERVER_SHUTDOWN,
};

struct SpawnedHerdr {
    _master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
}

struct RequestError {
    retryable: bool,
    message: String,
}

impl Drop for SpawnedHerdr {
    fn drop(&mut self) {
        let pid = self.child.process_id();
        let _ = self.child.kill();
        unregister_spawned_herdr_pid(pid);
    }
}

fn test_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn unique_test_dir() -> PathBuf {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    PathBuf::from(format!("/tmp/hlh-{}-{n}", std::process::id()))
}

const TEST_CONFIG: &str = "onboarding = false\n";

/// 把用例配置写到被测二进制读取的应用目录（`support::app_dir_name`）；写到别的目录会被
/// 静默忽略。
fn write_config(config_home: &Path, config: &str) {
    let app_dir = config_home.join(app_dir_name());
    fs::create_dir_all(&app_dir).unwrap();
    fs::write(app_dir.join("config.toml"), config).unwrap();
}

fn spawn_server(config_home: &Path, runtime_dir: &Path, api_socket: &Path) -> SpawnedHerdr {
    spawn_server_with_env(config_home, runtime_dir, api_socket, &[])
}

fn spawn_server_with_env(
    config_home: &Path,
    runtime_dir: &Path,
    api_socket: &Path,
    extra_env: &[(&str, &str)],
) -> SpawnedHerdr {
    spawn_server_with_config_and_env(config_home, runtime_dir, api_socket, TEST_CONFIG, extra_env)
}

fn spawn_server_with_config_and_env(
    config_home: &Path,
    runtime_dir: &Path,
    api_socket: &Path,
    config: &str,
    extra_env: &[(&str, &str)],
) -> SpawnedHerdr {
    write_config(config_home, config);
    fs::create_dir_all(runtime_dir).unwrap();

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
    // 状态目录也显式隔离（与 spawn_default_session_server 同一处）：不设时 state_dir 回退到
    // 平台目录（Windows 取 %LOCALAPPDATA%，不随 HOME 走），外层继承的 XDG_STATE_HOME 也会越过隔离。
    cmd.env("XDG_STATE_HOME", runtime_dir.join("state"));
    cmd.env("XDG_RUNTIME_DIR", runtime_dir);
    cmd.env("HERDR_SOCKET_PATH", api_socket);
    cmd.env(
        "HERDR_CLIENT_SOCKET_PATH",
        runtime_dir.join("herdr-client.sock"),
    );
    cmd.env("SHELL", "/bin/sh");
    // 宿主在 herdr 窗格内跑测试时会注入 HERDR_STARTUP_CWD：server 会据此预建
    // 启动工作区，破坏用例的工作区/pane 假设。
    cmd.env_remove("HERDR_STARTUP_CWD");
    for key in INHERITED_DIR_OVERRIDES {
        cmd.env_remove(key);
    }
    for (key, value) in extra_env {
        cmd.env(key, value);
    }

    let child = pair.slave.spawn_command(cmd).unwrap();
    register_spawned_herdr_pid(child.process_id());
    SpawnedHerdr {
        _master: pair.master,
        child,
    }
}

fn spawn_named_session_server(
    config_home: &Path,
    runtime_dir: &Path,
    session_name: &str,
) -> SpawnedHerdr {
    write_config(config_home, TEST_CONFIG);
    fs::create_dir_all(runtime_dir).unwrap();

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
    cmd.env("XDG_STATE_HOME", runtime_dir.join("state"));
    cmd.env("XDG_RUNTIME_DIR", runtime_dir);
    cmd.env("HERDR_SESSION", session_name);
    cmd.env_remove("HERDR_SOCKET_PATH");
    cmd.env_remove("HERDR_CLIENT_SOCKET_PATH");
    cmd.env("SHELL", "/bin/sh");
    // 宿主在 herdr 窗格内跑测试时会注入 HERDR_STARTUP_CWD：server 会据此预建
    // 启动工作区，破坏用例的工作区/pane 假设。
    cmd.env_remove("HERDR_STARTUP_CWD");
    for key in INHERITED_DIR_OVERRIDES {
        cmd.env_remove(key);
    }

    let child = pair.slave.spawn_command(cmd).unwrap();
    register_spawned_herdr_pid(child.process_id());
    SpawnedHerdr {
        _master: pair.master,
        child,
    }
}

fn spawn_default_session_server(config_home: &Path, runtime_dir: &Path) -> SpawnedHerdr {
    write_config(config_home, TEST_CONFIG);
    fs::create_dir_all(runtime_dir).unwrap();

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
    cmd.env("XDG_STATE_HOME", runtime_dir.join("state"));
    cmd.env_remove("HERDR_SESSION");
    cmd.env_remove("HERDR_SOCKET_PATH");
    cmd.env_remove("HERDR_CLIENT_SOCKET_PATH");
    cmd.env("SHELL", "/bin/sh");
    // 宿主在 herdr 窗格内跑测试时会注入 HERDR_STARTUP_CWD：server 会据此预建
    // 启动工作区，破坏用例的工作区/pane 假设。
    cmd.env_remove("HERDR_STARTUP_CWD");
    for key in INHERITED_DIR_OVERRIDES {
        cmd.env_remove(key);
    }

    let child = pair.slave.spawn_command(cmd).unwrap();
    register_spawned_herdr_pid(child.process_id());
    SpawnedHerdr {
        _master: pair.master,
        child,
    }
}

fn spawn_server_with_args_and_socket_env(
    config_home: &Path,
    runtime_dir: &Path,
    session_name: Option<&str>,
    api_socket_env: Option<&Path>,
    client_socket_env: Option<&Path>,
) -> SpawnedHerdr {
    write_config(config_home, TEST_CONFIG);
    fs::create_dir_all(runtime_dir).unwrap();

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_herdr"));
    if let Some(session_name) = session_name {
        cmd.arg("--session");
        cmd.arg(session_name);
    }
    cmd.arg("server");
    // HOME 隔离到测试目录：server 的活动树适配器（zcode 等）会按 HOME 读开发机上
    // 真实的 CLI 数据，把外部会话塞进快照，让用例随开发机状态漂移。
    let home = runtime_dir.join("home");
    let _ = fs::create_dir_all(&home);
    cmd.env("HOME", &home);
    cmd.env("XDG_CONFIG_HOME", config_home);
    cmd.env("XDG_STATE_HOME", runtime_dir.join("state"));
    cmd.env("XDG_RUNTIME_DIR", runtime_dir);
    cmd.env_remove("HERDR_SESSION");
    if let Some(api_socket_env) = api_socket_env {
        cmd.env("HERDR_SOCKET_PATH", api_socket_env);
    } else {
        cmd.env_remove("HERDR_SOCKET_PATH");
    }
    if let Some(client_socket_env) = client_socket_env {
        cmd.env("HERDR_CLIENT_SOCKET_PATH", client_socket_env);
    } else {
        cmd.env_remove("HERDR_CLIENT_SOCKET_PATH");
    }
    cmd.env("SHELL", "/bin/sh");
    // 宿主在 herdr 窗格内跑测试时会注入 HERDR_STARTUP_CWD：server 会据此预建
    // 启动工作区，破坏用例的工作区/pane 假设。
    cmd.env_remove("HERDR_STARTUP_CWD");
    for key in INHERITED_DIR_OVERRIDES {
        cmd.env_remove(key);
    }

    let child = pair.slave.spawn_command(cmd).unwrap();
    register_spawned_herdr_pid(child.process_id());
    SpawnedHerdr {
        _master: pair.master,
        child,
    }
}

fn try_request(
    socket_path: &Path,
    request: serde_json::Value,
) -> Result<serde_json::Value, RequestError> {
    let mut stream = UnixStream::connect(socket_path).map_err(|err| RequestError {
        retryable: true,
        message: format!("connect {}: {err}", socket_path.display()),
    })?;
    let request_text = request.to_string();
    stream
        .write_all(request_text.as_bytes())
        .map_err(|err| RequestError {
            retryable: true,
            message: format!("write request to {}: {err}", socket_path.display()),
        })?;
    stream.write_all(b"\n").map_err(|err| RequestError {
        retryable: true,
        message: format!("write newline to {}: {err}", socket_path.display()),
    })?;
    stream.flush().map_err(|err| RequestError {
        retryable: true,
        message: format!("flush request to {}: {err}", socket_path.display()),
    })?;
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .map_err(|err| RequestError {
            retryable: true,
            message: format!("read response from {}: {err}", socket_path.display()),
        })?;
    if line.is_empty() {
        return Err(RequestError {
            retryable: true,
            message: format!(
                "empty response from {} for request {request_text}",
                socket_path.display()
            ),
        });
    }
    serde_json::from_str(&line).map_err(|err| RequestError {
        retryable: false,
        message: format!(
            "parse response from {} for request {request_text}: {err}; response was {line:?}",
            socket_path.display()
        ),
    })
}

fn request(socket_path: &Path, request: serde_json::Value) -> serde_json::Value {
    try_request(socket_path, request).unwrap_or_else(|err| panic!("{}", err.message))
}

fn assert_ok(response: serde_json::Value) {
    assert!(
        response.get("result").is_some(),
        "api request failed: {response}"
    );
}

fn wait_for_api(socket_path: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    let mut last_error = String::new();
    while Instant::now() < deadline {
        match try_request(
            socket_path,
            serde_json::json!({"id":"test:ping","method":"ping","params":{}}),
        ) {
            Ok(response) if response.get("result").is_some() => return,
            Ok(response) => panic!("api ping returned non-success response: {response}"),
            Err(err) if !err.retryable => panic!("{}", err.message),
            Err(err) => {
                last_error = err.message;
            }
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!(
        "api did not become ready at {}; last error: {last_error}",
        socket_path.display()
    );
}

fn write_plugin_manifest(root: &Path, plugin_id: &str) {
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("herdr-plugin.toml"),
        format!(
            r#"id = "{plugin_id}"
name = "Live handoff test"
version = "0.1.0"
min_herdr_version = "0.6.10"
platforms = ["linux", "macos", "windows"]
"#
        ),
    )
    .unwrap();
}

fn link_plugin(socket_path: &Path, root: &Path) {
    assert_ok(request(
        socket_path,
        serde_json::json!({
            "id": "test:plugin:link",
            "method": "plugin.link",
            "params": {"path": root, "enabled": true}
        }),
    ));
}

fn listed_plugin_ids(socket_path: &Path) -> Vec<String> {
    let response = request(
        socket_path,
        serde_json::json!({"id":"test:plugin:list","method":"plugin.list","params":{}}),
    );
    assert_ok(response.clone());
    response["result"]["plugins"]
        .as_array()
        .unwrap()
        .iter()
        .map(|plugin| plugin["plugin_id"].as_str().unwrap().to_string())
        .collect()
}

fn saved_plugin_ids(registry_path: &Path) -> Vec<String> {
    let mut ids =
        serde_json::from_str::<Vec<serde_json::Value>>(&fs::read_to_string(registry_path).unwrap())
            .unwrap()
            .into_iter()
            .map(|plugin| plugin["plugin_id"].as_str().unwrap().to_string())
            .collect::<Vec<_>>();
    ids.sort();
    ids
}

fn wait_for_output(socket_path: &Path, pane_id: &str, needle: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut last_text = String::new();
    let mut last_response = serde_json::Value::Null;
    while Instant::now() < deadline {
        let response = request(
            socket_path,
            serde_json::json!({
                "id": "test:pane:read",
                "method": "pane.read",
                "params": {
                    "pane_id": pane_id,
                    "source": "visible",
                    "lines": 20,
                    "format": "text",
                    "strip_ansi": true
                }
            }),
        );
        last_response = response.clone();
        let text = response["result"]["read"]["text"]
            .as_str()
            .unwrap_or_default();
        last_text = text.to_string();
        if text.contains(needle) {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!(
        "pane output did not contain {needle:?}; last text was {last_text:?}; last response was {last_response}"
    );
}

fn wait_for_file_contains(path: &Path, needle: &str, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    let mut last_text = String::new();
    while Instant::now() < deadline {
        if let Ok(text) = fs::read_to_string(path) {
            last_text = text;
            if last_text.contains(needle) {
                return last_text;
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!(
        "{} did not contain {needle:?}; last text was {last_text:?}",
        path.display()
    );
}

fn wait_for_pid_marker(path: &Path, timeout: Duration) -> u32 {
    // Shell redirection creates the file before echo writes the PID. Wait for
    // the newline too, so a partially written PID cannot be accepted.
    let text = wait_for_file_contains(path, "\n", timeout);
    text.lines()
        .next()
        .and_then(|line| line.split_whitespace().last())
        .and_then(|pid| pid.parse().ok())
        .filter(|pid| *pid > 0)
        .unwrap_or_else(|| panic!("invalid PID marker at {}: {text:?}", path.display()))
}

#[test]
fn pid_marker_waits_for_complete_line() {
    let base = unique_test_dir();
    fs::create_dir_all(&base).unwrap();
    let marker = base.join("child.pid");
    // Keep each incomplete marker unchanged throughout the wait. In particular,
    // READY 12 must time out rather than return a truncated but parseable PID.
    for partial in ["", "READY ", "READY 12"] {
        fs::write(&marker, partial).unwrap();
        let timeout = Duration::from_millis(100);
        let started = Instant::now();
        let panic = std::panic::catch_unwind(|| wait_for_pid_marker(&marker, timeout))
            .expect_err("incomplete marker should time out");
        assert!(
            started.elapsed() >= timeout,
            "marker {partial:?} failed early"
        );
        let message = panic.downcast_ref::<String>().expect("timeout diagnostic");
        assert_eq!(
            message,
            &format!(
                "{} did not contain {:?}; last text was {partial:?}",
                marker.display(),
                "\n"
            )
        );
    }
    fs::write(&marker, "READY 1234\n").unwrap();
    assert_eq!(wait_for_pid_marker(&marker, Duration::from_secs(1)), 1234);
    fs::remove_dir_all(base).unwrap();
}

#[cfg(target_os = "linux")]
fn server_ptmx_fd_count(pid: u32) -> usize {
    let Ok(entries) = fs::read_dir(format!("/proc/{pid}/fd")) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| fs::read_link(entry.path()).ok())
        // ptmx master node: /dev/ptmx or /dev/pts/ptmx (devpts); slaves /dev/pts/<N> excluded.
        .filter(|target| target == Path::new("/dev/ptmx") || target == Path::new("/dev/pts/ptmx"))
        .count()
}

#[cfg(target_os = "macos")]
fn server_ptmx_fd_count(pid: u32) -> usize {
    let Ok(output) = std::process::Command::new("lsof")
        .args(["-nP", "-p", &pid.to_string()])
        .output()
    else {
        return 0;
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| line.contains("/dev/ptmx"))
        .count()
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn wait_for_server_ptmx_fd_count(pid: u32, expected: usize, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    let mut last_count = 0;
    while Instant::now() < deadline {
        last_count = server_ptmx_fd_count(pid);
        if last_count == expected {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("server pid {pid} had {last_count} ptmx master fds; expected {expected}");
}

#[cfg(target_os = "linux")]
fn wait_for_replacement_server_pid(runtime_dir: &Path, old_pid: u32, timeout: Duration) -> u32 {
    let deadline = Instant::now() + timeout;
    let mut last_pids = Vec::new();
    while Instant::now() < deadline {
        last_pids = support::herdr_server_pids_for_runtime_dir(runtime_dir).unwrap_or_default();
        if let Some(pid) = last_pids.iter().copied().find(|pid| *pid != old_pid) {
            return pid;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!(
        "replacement server for {} did not appear; last pids: {:?}",
        runtime_dir.display(),
        last_pids
    );
}

#[cfg(target_os = "macos")]
fn wait_for_replacement_server_pid(_runtime_dir: &Path, old_pid: u32, timeout: Duration) -> u32 {
    let handoff_socket_pattern = format!("herdr-handoff-{old_pid}.sock");
    let deadline = Instant::now() + timeout;
    let mut last_stdout = String::new();
    while Instant::now() < deadline {
        if let Ok(output) = std::process::Command::new("pgrep")
            .args(["-af", &handoff_socket_pattern])
            .output()
        {
            last_stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            for line in last_stdout.lines() {
                let Some(pid_text) = line.split_whitespace().next() else {
                    continue;
                };
                let Ok(pid) = pid_text.parse::<u32>() else {
                    continue;
                };
                if pid != old_pid {
                    return pid;
                }
            }
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!(
        "replacement server for {} did not appear; last pgrep output: {}",
        _runtime_dir.display(),
        last_stdout
    );
}

fn unused_local_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

const HTTP_RESPONSE_LIMIT: usize = 64 * 1024;
const DIAGNOSTIC_LIMIT: usize = 128 * 1024;

#[derive(Debug)]
struct HttpFailure {
    operation: &'static str,
    kind: std::io::ErrorKind,
    message: String,
    response: String,
}

#[derive(Debug)]
struct HttpWaitFailure {
    attempts: usize,
    errors: std::collections::BTreeMap<String, usize>,
    last: HttpFailure,
}

fn remaining(deadline: Instant) -> std::io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::TimedOut, "absolute deadline elapsed")
        })
}

fn http_response(port: u16, deadline: Instant) -> Result<String, HttpFailure> {
    let mut bytes = Vec::new();
    let mut operation = "connect";
    let result = (|| -> std::io::Result<()> {
        let address = ([127, 0, 0, 1], port).into();
        let mut stream = TcpStream::connect_timeout(&address, remaining(deadline)?)?;
        operation = "write";
        let mut request = &b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"[..];
        while !request.is_empty() {
            stream.set_write_timeout(Some(remaining(deadline)?))?;
            match stream.write(request) {
                Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
                Ok(count) => request = &request[count..],
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        operation = "read";
        loop {
            stream.set_read_timeout(Some(remaining(deadline)?))?;
            let mut buffer = [0; 4096];
            let available = (HTTP_RESPONSE_LIMIT + 1 - bytes.len()).min(buffer.len());
            match stream.read(&mut buffer[..available]) {
                Ok(0) => break,
                Ok(count) => bytes.extend_from_slice(&buffer[..count]),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
            if bytes.len() > HTTP_RESPONSE_LIMIT {
                operation = "response-limit";
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "HTTP response exceeded 64 KiB",
                ));
            }
        }
        remaining(deadline)?;
        operation = "decode";
        std::str::from_utf8(&bytes)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        Ok(())
    })();
    let response =
        String::from_utf8_lossy(&bytes[..bytes.len().min(HTTP_RESPONSE_LIMIT)]).into_owned();
    result
        .map(|()| response.clone())
        .map_err(|error| HttpFailure {
            operation,
            kind: error.kind(),
            message: error.to_string(),
            response,
        })
}

fn poll_http_contains(
    port: u16,
    needle: &str,
    timeout: Duration,
) -> Result<String, HttpWaitFailure> {
    let deadline = Instant::now() + timeout;
    let mut failure = HttpWaitFailure {
        attempts: 0,
        errors: std::collections::BTreeMap::new(),
        last: HttpFailure {
            operation: "deadline",
            kind: std::io::ErrorKind::TimedOut,
            message: "no attempt within budget".into(),
            response: String::new(),
        },
    };
    while remaining(deadline).is_ok() {
        failure.attempts += 1;
        failure.last = match http_response(port, deadline) {
            Ok(response) if response.contains(needle) => return Ok(response),
            Ok(response) => HttpFailure {
                operation: "match",
                kind: std::io::ErrorKind::InvalidData,
                message: "response did not contain expected payload".into(),
                response,
            },
            Err(error) => error,
        };
        *failure
            .errors
            .entry(format!(
                "{}:{:?}",
                failure.last.operation, failure.last.kind
            ))
            .or_default() += 1;
        if let Ok(budget) = remaining(deadline) {
            thread::sleep(budget.min(Duration::from_millis(50)));
        }
    }
    Err(failure)
}

fn posix_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\"'\"'"))
}

const PYTHON_HTTP_FIXTURE: &str = r#"import http.server
import socketserver
import sys

class LoopbackHTTPServer(http.server.ThreadingHTTPServer):
    def server_bind(self):
        socketserver.TCPServer.server_bind(self)
        self.server_name = "127.0.0.1"
        self.server_port = self.server_address[1]

if __name__ == "__main__":
    with LoopbackHTTPServer(("127.0.0.1", int(sys.argv[1])), http.server.SimpleHTTPRequestHandler) as server:
        print(f"Serving HTTP on 127.0.0.1 port {server.server_port}", flush=True)
        server.serve_forever()
"#;

fn python_http_command(web_root: &Path, port: u16) -> String {
    let script = web_root.join("handoff_http.py");
    fs::write(&script, PYTHON_HTTP_FIXTURE).unwrap();
    let script = posix_quote(&script.canonicalize().unwrap().to_string_lossy());
    let log = posix_quote(&web_root.join("startup.stdout").to_string_lossy());
    let err = posix_quote(&web_root.join("startup.stderr").to_string_lossy());
    let probe = posix_quote("import os, sys; print('sys.executable=' + sys.executable); print('sys.version=' + sys.version); print('cwd=' + os.getcwd()); print('probe_pid=' + str(os.getpid())); print('import http.server: starting', flush=True); import http.server; print('import http.server: ok', flush=True)");
    let launch = posix_quote(&format!(
        "printf 'server_pid=%s\\n' \"$$\" >> {log}; exec python3 {script} {port}"
    ));
    format!(
        "{{ pwd; command -v python3; python3 -c {probe}; printf 'probe_exit=%s\\n' \"$?\"; }} > {log} 2> {err}; sh -c {launch}; printf 'server_exit=%s\\n' \"$?\" >> {log}"
    )
}

struct HttpContext<'a> {
    test: &'a str,
    phase: &'a str,
    session: &'a str,
    socket: &'a Path,
    pane: &'a str,
    web_root: &'a Path,
    config_home: &'a Path,
}

fn diagnostic_pane_read(socket: &Path, pane: &str) -> std::io::Result<Vec<u8>> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    #[cfg(target_os = "macos")]
    {
        address.sun_len = std::mem::size_of_val(&address) as u8;
    }
    let path = socket.as_os_str().as_bytes();
    if path.len() >= address.sun_path.len() || path.contains(&0) {
        return Err(std::io::ErrorKind::InvalidInput.into());
    }
    for (destination, source) in address.sun_path.iter_mut().zip(path) {
        *destination = *source as libc::c_char;
    }
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let stream = unsafe { UnixStream::from_raw_fd(fd) };
    if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    stream.set_nonblocking(true)?;
    let connected = unsafe {
        libc::connect(
            fd,
            (&address as *const libc::sockaddr_un).cast(),
            std::mem::size_of_val(&address) as libc::socklen_t,
        )
    };
    if connected < 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINPROGRESS) {
            return Err(error);
        }
    }
    let wait = |events| -> std::io::Result<()> {
        loop {
            let mut descriptor = libc::pollfd {
                fd: stream.as_raw_fd(),
                events,
                revents: 0,
            };
            let milliseconds = remaining(deadline)?
                .as_millis()
                .max(1)
                .min(i32::MAX as u128) as i32;
            let ready = unsafe { libc::poll(&mut descriptor, 1, milliseconds) };
            if ready > 0 {
                return remaining(deadline).map(|_| ());
            }
            if ready < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
        }
    };
    wait(libc::POLLOUT)?;
    if let Some(error) = stream.take_error()? {
        return Err(error);
    }
    let request = format!(
        "{}\n",
        serde_json::json!({
            "id": "test:http:diagnostic", "method": "pane.read",
            "params": {"pane_id": pane, "source": "visible", "lines": 40, "format": "text", "strip_ansi": true}
        })
    );
    let mut pending = request.as_bytes();
    while !pending.is_empty() {
        wait(libc::POLLOUT)?;
        match (&stream).write(pending) {
            Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
            Ok(count) => pending = &pending[count..],
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(error),
        }
    }
    let mut response = Vec::new();
    loop {
        wait(libc::POLLIN)?;
        let mut buffer = [0; 4096];
        match (&stream).read(&mut buffer) {
            Ok(0) => return Ok(response),
            Ok(count) => {
                let count = count.min(DIAGNOSTIC_LIMIT - response.len());
                response.extend_from_slice(&buffer[..count]);
                if response.contains(&b'\n') || response.len() == DIAGNOSTIC_LIMIT {
                    return Ok(response);
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(error),
        }
    }
}

fn diagnostic_file(path: &Path) -> std::io::Result<Vec<u8>> {
    use std::io::{Seek, SeekFrom};
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(std::io::ErrorKind::InvalidInput.into());
    }
    file.seek(SeekFrom::Start(
        metadata.len().saturating_sub(DIAGNOSTIC_LIMIT as u64),
    ))?;
    let mut bytes = Vec::new();
    file.take(DIAGNOSTIC_LIMIT as u64).read_to_end(&mut bytes)?;
    Ok(bytes)
}

#[cfg(any(target_os = "macos", test))]
fn diagnostic_command(
    command: &mut std::process::Command,
    deadline: Instant,
) -> std::io::Result<(Option<std::process::ExitStatus>, Vec<u8>)> {
    use std::os::fd::OwnedFd;
    use std::process::Stdio;
    remaining(deadline)?;
    let (mut reader, writer) = UnixStream::pair()?;
    reader.set_nonblocking(true)?;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::from(OwnedFd::from(writer.try_clone()?)))
        .stderr(Stdio::from(OwnedFd::from(writer)))
        .spawn()?;
    let mut output = Vec::new();
    loop {
        let status = match child.try_wait() {
            Ok(status) => status,
            Err(error) => {
                let _ = child.kill();
                thread::Builder::new()
                    .spawn(move || {
                        let _ = child.wait();
                    })
                    .map_err(|reap_error| {
                        std::io::Error::other(format!("{error}; reaper unavailable: {reap_error}"))
                    })?;
                return Err(error);
            }
        };
        for _ in 0..32 {
            let mut buffer = [0; 4096];
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    output.extend_from_slice(&buffer[..count.min(DIAGNOSTIC_LIMIT - output.len())])
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        if status.is_some() {
            return Ok((status, output));
        }
        let Ok(budget) = remaining(deadline) else {
            let _ = child.kill();
            // Reaping must not renew the expired evidence deadline.
            thread::Builder::new()
                .spawn(move || {
                    let _ = child.wait();
                })
                .map_err(|error| {
                    std::io::Error::other(format!(
                        "deadline exhausted; reaper unavailable: {error}"
                    ))
                })?;
            return Ok((None, output));
        };
        thread::sleep(budget.min(Duration::from_millis(10)));
    }
}

#[cfg(any(target_os = "macos", test))]
fn python_process_identity(ps: &str, cwd: &str, pid: u32, port: u16, web_root: &Path) -> bool {
    let fields: Vec<_> = ps.split_whitespace().collect();
    let executable = fields
        .get(7)
        .and_then(|name| Path::new(name).file_name())
        .and_then(|name| name.to_str());
    let python = matches!(executable, Some("Python" | "python3"));
    fields.len() == 10
        && fields[0] == pid.to_string()
        && python
        && Path::new(fields[8]) == web_root.join("handoff_http.py")
        && fields[9] == port.to_string()
        && cwd.lines().any(|line| line == format!("p{pid}"))
        && cwd
            .lines()
            .any(|line| line == format!("n{}", web_root.display()))
}

#[cfg(target_os = "macos")]
fn capture_python_process(
    context: &HttpContext<'_>,
    port: u16,
    directory: &Path,
) -> std::io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(3);
    let startup = diagnostic_file(&context.web_root.join("startup.stdout"))?;
    let startup = String::from_utf8_lossy(&startup);
    let pids: Vec<_> = startup
        .lines()
        .filter_map(|line| line.strip_prefix("server_pid="))
        .collect();
    let pid = pids
        .first()
        .filter(|_| {
            pids.len() == 1 && !startup.lines().any(|line| line.starts_with("server_exit="))
        })
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|pid| *pid > 0 && *pid <= i32::MAX as u32)
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "unavailable: missing/ambiguous/or exited PID marker",
            )
        })?;
    let pid_text = pid.to_string();
    let web_root = context.web_root.canonicalize()?;
    let mut captures = Vec::new();
    let mut run = |name: &str, executable: &str, args: &[&str]| -> std::io::Result<String> {
        let (status, output) = diagnostic_command(
            std::process::Command::new(executable)
                .env("LC_ALL", "C")
                .args(args),
            deadline,
        )
        .map_err(|error| std::io::Error::other(format!("unavailable: {name}: {error}")))?;
        let text = String::from_utf8_lossy(&output).into_owned();
        captures.push((
            name.to_string(),
            output,
            format!("exit={status:?}; deadline_exhausted={}\n", status.is_none()),
        ));
        if !status.is_some_and(|status| status.success()) {
            return Err(std::io::Error::other(format!(
                "unavailable: {name} exit={status:?}"
            )));
        }
        Ok(text)
    };
    let ps_args = ["-ww", "-p", &pid_text, "-o", "pid=,stat=,lstart=,command="];
    let cwd_args = ["-nP", "-a", "-p", &pid_text, "-d", "cwd", "-Fpn"];
    let ps = run("python-ps.txt", "/bin/ps", &ps_args)?;
    let cwd = run("python-cwd.txt", "/usr/sbin/lsof", &cwd_args)?;
    if !python_process_identity(&ps, &cwd, pid, port, &web_root) {
        return Err(std::io::Error::other(
            "unavailable: Python command/port/private cwd identity not confirmed",
        ));
    }
    let _ = run(
        "python-tcp.txt",
        "/usr/sbin/lsof",
        &["-nP", "-a", "-p", &pid_text, "-i", &format!("TCP:{port}")],
    );
    let current_ps = run("python-ps-recheck.txt", "/bin/ps", &ps_args)?;
    let current_cwd = run("python-cwd-recheck.txt", "/usr/sbin/lsof", &cwd_args)?;
    if ps
        .split_whitespace()
        .skip(2)
        .ne(current_ps.split_whitespace().skip(2))
        || !python_process_identity(&current_ps, &current_cwd, pid, port, &web_root)
    {
        return Err(std::io::Error::other(
            "unavailable: process identity changed before sampling",
        ));
    }
    let raw_sample = context.web_root.join("python-native-sample.raw");
    let result = run(
        "python-sample-command.txt",
        "/usr/bin/sample",
        &[&pid_text, "1", "10", "-file", &raw_sample.to_string_lossy()],
    );
    for (name, output, status) in captures {
        fs::write(directory.join(&name), output)?;
        fs::write(directory.join(format!("{name}.status")), status)?;
    }
    let bytes = diagnostic_file(&raw_sample)
        .unwrap_or_else(|error| format!("capture unavailable: {error}\n").into_bytes());
    fs::write(directory.join("python-native-sample.txt"), bytes)?;
    result.map(|_| ())
}

fn preserve_http_failure(
    context: &HttpContext<'_>,
    port: u16,
    needle: &str,
    failure: &HttpWaitFailure,
) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).canonicalize()?;
    let mut parent = root.clone();
    for component in ["target", "ci-release-evidence", "handoff-diagnostics"] {
        parent.push(component);
        match fs::create_dir(&parent) {
            Ok(()) => (),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(error) => return Err(error),
        }
        if !parent.canonicalize()?.starts_with(&root) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "diagnostic directory escaped source root",
            ));
        }
    }
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let directory = parent.join(format!(
        "{}-{timestamp}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    fs::DirBuilder::new().mode(0o700).create(&directory)?;
    let sha = std::env::var("GITHUB_SHA")
        .unwrap_or_else(|_| "not provided (see CI checkout receipt)".into());
    let summary = format!("test={}\nphase={}\nsession={}\nsocket={}\npane={}\nport={port}\nneedle={needle:?}\nsource_root={}\nGITHUB_SHA={sha}\nattempts={}\nerrors={:?}\nlast_operation={}\nlast_kind={:?}\nlast_message={}\nlast_response={:?}\n", context.test, context.phase, context.session, context.socket.display(), context.pane, root.display(), failure.attempts, failure.errors, failure.last.operation, failure.last.kind, failure.last.message, failure.last.response);
    fs::write(directory.join("http-failure.txt"), summary)?;
    let capture = |name: &str, result: std::io::Result<Vec<u8>>| -> std::io::Result<()> {
        let bytes =
            result.unwrap_or_else(|error| format!("capture unavailable: {error}\n").into_bytes());
        fs::write(directory.join(name), bytes)
    };
    capture(
        "python-stdout-stderr-pane.json",
        diagnostic_pane_read(context.socket, context.pane),
    )?;
    for name in ["startup.stdout", "startup.stderr"] {
        capture(name, diagnostic_file(&context.web_root.join(name)))?;
    }
    for (label, data_dir) in [
        ("default", context.config_home.join(app_dir_name())),
        (
            "work",
            context
                .config_home
                .join(app_dir_name())
                .join("sessions/work"),
        ),
    ] {
        for suffix in ["", ".1", ".2"] {
            capture(
                &format!("{label}-herdr-server.log{suffix}"),
                diagnostic_file(&data_dir.join(format!("herdr-server.log{suffix}"))),
            )?;
        }
    }
    #[cfg(target_os = "macos")]
    capture(
        "python-process-capture.txt",
        capture_python_process(context, port, &directory).map(|()| b"capture completed\n".to_vec()),
    )?;
    Ok(directory)
}

fn wait_for_http_contains(
    port: u16,
    needle: &str,
    timeout: Duration,
    context: &HttpContext<'_>,
) -> String {
    match poll_http_contains(port, needle, timeout) {
        Ok(response) => response,
        Err(failure) => {
            let evidence = preserve_http_failure(context, port, needle, &failure);
            panic!("http server on port {port} did not return {needle:?}; test={} phase={} session={}; attempts={} errors={:?}; last operation={} kind={:?} message={}; last response was {:?}; evidence={evidence:?}", context.test, context.phase, context.session, failure.attempts, failure.errors, failure.last.operation, failure.last.kind, failure.last.message, failure.last.response);
        }
    }
}

mod http_helper_tests {
    use super::*;

    #[test]
    fn process_identity_requires_exact_command_port_and_private_cwd() {
        let ps = "123 S Tue Oct 6 19:17:38 2026 /usr/local/bin/python3 /private/tmp/hlh-1/web/handoff_http.py 49421\n";
        let cwd = "p123\nn/private/tmp/hlh-1/web\n";
        let root = Path::new("/private/tmp/hlh-1/web");
        assert!(python_process_identity(ps, cwd, 123, 49421, root));
        for wrong in [
            ps.replace("123 S", "124 S"),
            ps.replace("49421", "49422"),
            ps.replace("python3", "not-python3"),
            ps.replace("handoff_http.py", "different.py"),
            ps.replace("hlh-1", "hlh-2"),
            format!("{ps} extra"),
        ] {
            assert!(!python_process_identity(&wrong, cwd, 123, 49421, root));
        }
        assert!(!python_process_identity(
            ps,
            "p123\nn/private/tmp/another/web\n",
            123,
            49421,
            root
        ));
        assert!(!python_process_identity(
            ps,
            "p124\nn/private/tmp/hlh-1/web\n",
            123,
            49421,
            root
        ));
    }

    #[test]
    fn diagnostic_subprocess_has_bounded_output_and_shared_deadline() {
        let started = Instant::now();
        let deadline = started + Duration::from_millis(150);
        let (status, output) = diagnostic_command(
            std::process::Command::new("/bin/sh").args([
                "-c",
                "printf start; printf error >&2; while :; do printf 0123456789; done",
            ]),
            deadline,
        )
        .unwrap();
        assert!(status.is_none());
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(output.starts_with(b"starterror"));
        assert!(output.len() <= DIAGNOSTIC_LIMIT);
        assert_eq!(
            diagnostic_command(&mut std::process::Command::new("/does-not-exist"), deadline)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::TimedOut
        );
        let (status, output) = diagnostic_command(
            std::process::Command::new("/bin/sh").args(["-c", "printf done; exit 7"]),
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(status.and_then(|status| status.code()), Some(7));
        assert_eq!(output, b"done");
    }

    fn mock_http(reply: impl FnOnce(TcpStream) + Send + 'static) -> (u16, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let worker = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        stream
                            .set_write_timeout(Some(Duration::from_secs(1)))
                            .unwrap();
                        let request_deadline = Instant::now() + Duration::from_secs(1);
                        let mut request = [0; 128];
                        let mut received = 0;
                        while !request[..received].ends_with(b"\r\n\r\n") {
                            assert!(received < request.len(), "mock request header too large");
                            stream
                                .set_read_timeout(Some(remaining(request_deadline).unwrap()))
                                .unwrap();
                            let count = match stream.read(&mut request[received..]) {
                                Ok(count) => count,
                                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {
                                    continue
                                }
                                Err(error) => panic!("mock request read: {error}"),
                            };
                            assert!(count > 0, "mock request closed before complete headers");
                            received += count;
                        }
                        reply(stream);
                        return;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "mock received no request");
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("mock accept: {error}"),
                }
            }
        });
        (port, worker)
    }

    #[test]
    fn mock_waits_for_fragmented_headers_before_reply_and_close() {
        let (port, worker) = mock_http(|mut stream| stream.write_all(&[0xff]).unwrap());
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        stream.write_all(b"G").unwrap();
        let error = stream
            .read(&mut [0; 1])
            .expect_err("mock replied before headers completed");
        assert!(matches!(
            error.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ));
        stream
            .write_all(b"ET / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
            .unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).unwrap();
        worker.join().unwrap();
        assert_eq!(response, [0xff]);
    }

    #[test]
    fn successful_response_keeps_payload_gate() {
        let (port, worker) = mock_http(|mut stream| {
            stream
                .write_all(b"HTTP/1.0 200 OK\r\n\r\nexpected-payload")
                .unwrap();
        });
        assert!(
            poll_http_contains(port, "expected-payload", Duration::from_secs(1))
                .unwrap()
                .contains("expected-payload")
        );
        worker.join().unwrap();
    }

    #[test]
    fn refused_connection_records_attempts_and_operation() {
        let failure =
            poll_http_contains(unused_local_port(), "expected", Duration::from_millis(100))
                .unwrap_err();
        assert!(failure.attempts > 0);
        assert_eq!(failure.errors.values().sum::<usize>(), failure.attempts);
        assert_eq!(failure.last.operation, "connect");
        assert_eq!(failure.last.kind, std::io::ErrorKind::ConnectionRefused);
        assert!(!failure.last.message.is_empty());
    }

    #[test]
    fn zero_budget_never_connects() {
        let failure = poll_http_contains(0, "expected", Duration::ZERO).unwrap_err();
        assert_eq!(failure.attempts, 0);
        assert_eq!(failure.last.operation, "deadline");
    }

    #[test]
    fn stalled_and_dribbling_response_share_absolute_deadline() {
        for dribble in [false, true] {
            let (port, worker) = mock_http(move |mut stream| {
                if dribble {
                    for _ in 0..60 {
                        if stream.write_all(b"x").is_err() {
                            break;
                        }
                        thread::sleep(Duration::from_millis(10));
                    }
                } else {
                    thread::sleep(Duration::from_millis(600));
                }
            });
            let started = Instant::now();
            let failure =
                poll_http_contains(port, "expected", Duration::from_millis(150)).unwrap_err();
            let elapsed = started.elapsed();
            worker.join().unwrap();
            assert!(
                elapsed < Duration::from_millis(500),
                "renewed deadline: {elapsed:?}"
            );
            assert_eq!(failure.last.operation, "read");
            assert!(matches!(
                failure.last.kind,
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
            ));
            assert_eq!(failure.attempts, 1);
        }
    }

    #[test]
    fn oversized_response_is_bounded_and_invalid_utf8_is_reported() {
        for (bytes, operation) in [
            (vec![b'x'; HTTP_RESPONSE_LIMIT + 4096], "response-limit"),
            (vec![0xff], "decode"),
        ] {
            let (port, worker) = mock_http(move |mut stream| {
                let _ = stream.write_all(&bytes);
            });
            let failure = http_response(port, Instant::now() + Duration::from_secs(1)).unwrap_err();
            worker.join().unwrap();
            assert_eq!(failure.operation, operation);
            assert_eq!(failure.kind, std::io::ErrorKind::InvalidData);
            assert!(failure.response.len() <= HTTP_RESPONSE_LIMIT);
        }
    }

    #[test]
    fn wrong_payload_is_not_success() {
        let (port, worker) = mock_http(|mut stream| {
            stream.write_all(b"HTTP/1.0 200 OK\r\n\r\nwrong").unwrap();
        });
        let failure = poll_http_contains(port, "expected", Duration::from_millis(150)).unwrap_err();
        worker.join().unwrap();
        assert_eq!(failure.errors.get("match:InvalidData"), Some(&1));
    }

    #[test]
    fn posix_quote_preserves_special_characters() {
        for value in ["", "plain", "a'b c/$HOME;$(false)\nend"] {
            let output = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(format!("printf '%s' {}", posix_quote(value)))
                .output()
                .unwrap();
            assert!(output.status.success());
            assert_eq!(output.stdout, value.as_bytes());
        }
    }

    #[test]
    fn python_fixture_serves_payload_without_reverse_dns() {
        let base = unique_test_dir();
        fs::create_dir_all(&base).unwrap();
        fs::write(base.join("handoff_http.py"), PYTHON_HTTP_FIXTURE).unwrap();
        fs::write(
            base.join("index.html"),
            "hello-from-python-before-and-after",
        )
        .unwrap();
        let check = r#"import http.client, runpy, socket, sys, threading
module = runpy.run_path("handoff_http.py")
def reject_fqdn(frame, event, arg):
    if event == "call" and frame.f_code is socket.getfqdn.__code__:
        raise AssertionError("unexpected getfqdn call")
sys.setprofile(reject_fqdn)
try:
    socket.getfqdn("127.0.0.1")
except AssertionError:
    pass
else:
    raise AssertionError("getfqdn guard did not fire")
sys.setprofile(reject_fqdn)
threading.setprofile(reject_fqdn)
with module["LoopbackHTTPServer"](("127.0.0.1", 0), module["http"].server.SimpleHTTPRequestHandler) as server:
    assert server.server_name == "127.0.0.1"
    assert server.server_port == server.socket.getsockname()[1] != 0
    server.timeout = 1
    worker = threading.Thread(target=server.handle_request)
    worker.start()
    connection = http.client.HTTPConnection("127.0.0.1", server.server_port, timeout=1)
    try:
        connection.request("GET", "/")
        response = connection.getresponse()
        assert response.status == 200
        assert response.read() == b"hello-from-python-before-and-after"
    finally:
        connection.close()
    worker.join(1)
    assert not worker.is_alive()
print("fixture-no-reverse-dns-ok")
"#;
        let (status, output) = diagnostic_command(
            std::process::Command::new("python3")
                .args(["-c", check])
                .current_dir(&base),
            Instant::now() + Duration::from_secs(10),
        )
        .unwrap();
        fs::remove_dir_all(&base).unwrap();
        assert!(
            status.is_some_and(|status| status.success()),
            "{}",
            String::from_utf8_lossy(&output)
        );
        assert!(String::from_utf8_lossy(&output).contains("fixture-no-reverse-dns-ok"));
    }

    #[test]
    fn startup_logs_quote_paths_and_leave_http_output_in_pane() {
        use std::os::unix::fs::PermissionsExt;
        let base = unique_test_dir();
        let web_root = base.join("web' $HOME; quoted");
        let bin = base.join("bin");
        fs::create_dir_all(&web_root).unwrap();
        fs::create_dir_all(&bin).unwrap();
        let python = bin.join("python3");
        fs::write(&python, "#!/bin/sh\nif [ \"$1\" = -c ]; then printf 'mock-probe\\n'; exit 0; fi\nprintf 'http-stdout:%s\\n' \"$*\"\nprintf 'http-stderr\\n' >&2\nexit 7\n").unwrap();
        fs::set_permissions(&python, fs::Permissions::from_mode(0o700)).unwrap();
        let output = std::process::Command::new("/bin/sh")
            .args(["-c", &python_http_command(&web_root, 12345)])
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .current_dir(&web_root)
            .output()
            .unwrap();
        let startup = fs::read_to_string(web_root.join("startup.stdout")).unwrap();
        let stderr = fs::read(web_root.join("startup.stderr")).unwrap();
        let script = web_root.join("handoff_http.py").canonicalize().unwrap();
        fs::remove_dir_all(&base).unwrap();
        assert!(output.status.success());
        assert_eq!(
            output.stdout,
            format!("http-stdout:{} 12345\n", script.display()).as_bytes()
        );
        assert_eq!(output.stderr, b"http-stderr\n");
        assert!(stderr.is_empty());
        assert!(startup.contains(&web_root.to_string_lossy().to_string()));
        assert!(startup.contains(&python.to_string_lossy().to_string()));
        assert!(startup.contains("probe_exit=0\n"));
        assert!(startup.contains("server_pid="));
        assert!(startup.contains("server_exit=7\n"));
    }

    #[test]
    fn pane_capture_stall_is_bounded() {
        use std::os::unix::net::UnixListener;
        let base = unique_test_dir();
        fs::create_dir_all(&base).unwrap();
        let socket = base.join("diagnostic.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let started = Instant::now();
        let result = diagnostic_pane_read(&socket, "pane:1");
        let elapsed = started.elapsed();
        drop(listener);
        fs::remove_dir_all(&base).unwrap();
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::TimedOut);
        assert!(
            elapsed < Duration::from_secs(2),
            "pane capture exceeded deadline: {elapsed:?}"
        );
    }

    #[test]
    fn evidence_survives_sandbox_cleanup_and_log_reads_are_bounded() {
        let base = unique_test_dir();
        let web_root = base.join("web' quoted");
        fs::create_dir_all(&web_root).unwrap();
        fs::write(web_root.join("startup.stdout"), "synthetic startup\n").unwrap();
        fs::write(
            web_root.join("startup.stderr"),
            vec![b'e'; DIAGNOSTIC_LIMIT + 100],
        )
        .unwrap();
        let failure = poll_http_contains(0, "expected", Duration::ZERO).unwrap_err();
        let directory = preserve_http_failure(
            &HttpContext {
                test: "synthetic-http-diagnostics",
                phase: "post-handoff",
                session: "work",
                socket: &base.join("missing.sock"),
                pane: "pane:1",
                web_root: &web_root,
                config_home: &base.join("config"),
            },
            0,
            "expected",
            &failure,
        )
        .unwrap();
        fs::remove_dir_all(&base).unwrap();
        let summary = fs::read_to_string(directory.join("http-failure.txt")).unwrap();
        let stdout = fs::read_to_string(directory.join("startup.stdout")).unwrap();
        let stderr = fs::read(directory.join("startup.stderr")).unwrap();
        let pane = fs::read_to_string(directory.join("python-stdout-stderr-pane.json")).unwrap();
        fs::remove_dir_all(directory).unwrap();
        assert!(summary.contains("phase=post-handoff\nsession=work"));
        assert!(summary.contains("last_operation=deadline"));
        assert_eq!(stdout, "synthetic startup\n");
        assert_eq!(stderr.len(), DIAGNOSTIC_LIMIT);
        assert!(pane.contains("capture unavailable"));
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn live_server_holds_one_pty_master_fd_per_pane() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");

    let spawned = spawn_server(&config_home, &runtime_dir, &api_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    register_runtime_dir(&runtime_dir);
    let server_pid = spawned
        .child
        .process_id()
        .expect("test server should expose pid");
    wait_for_server_ptmx_fd_count(server_pid, 0, Duration::from_secs(5));

    let created = request(
        &api_socket,
        serde_json::json!({
            "id": "test:workspace:create",
            "method": "workspace.create",
            "params": {"cwd": "/tmp", "focus": true}
        }),
    );
    let pane_id = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();
    wait_for_server_ptmx_fd_count(server_pid, 1, Duration::from_secs(5));

    let second = request(
        &api_socket,
        serde_json::json!({
            "id": "test:pane:split-second",
            "method": "pane.split",
            "params": {
                "target_pane_id": pane_id,
                "direction": "right",
                "focus": true
            }
        }),
    );
    assert_ok(second.clone());
    let second_pane_id = second["result"]["pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();
    wait_for_server_ptmx_fd_count(server_pid, 2, Duration::from_secs(5));

    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:pane:split-third",
            "method": "pane.split",
            "params": {
                "target_pane_id": second_pane_id,
                "direction": "down",
                "focus": true
            }
        }),
    ));
    wait_for_server_ptmx_fd_count(server_pid, 3, Duration::from_secs(5));

    assert_ok(request(
        &api_socket,
        serde_json::json!({"id":"test:handoff","method":"server.live_handoff","params":{}}),
    ));
    let replacement_pid =
        wait_for_replacement_server_pid(&runtime_dir, server_pid, Duration::from_secs(10));
    wait_for_api(&api_socket, Duration::from_secs(10));
    wait_for_server_ptmx_fd_count(replacement_pid, 3, Duration::from_secs(5));

    let _ = request(
        &api_socket,
        serde_json::json!({"id":"test:stop","method":"server.stop","params":{}}),
    );
    drop(spawned);
    cleanup_test_base(&base);
}

#[cfg(target_os = "linux")]
#[test]
fn live_handoff_unknown_pane_exit_preserves_session_on_shutdown() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");

    let spawned = spawn_server(&config_home, &runtime_dir, &api_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    register_runtime_dir(&runtime_dir);

    let created = request(
        &api_socket,
        serde_json::json!({
            "id": "test:workspace:create",
            "method": "workspace.create",
            "params": {"cwd": "/tmp", "focus": true}
        }),
    );
    let pane_id = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .expect("root pane id")
        .to_string();
    let old_pid = spawned.child.process_id().expect("old server pid");

    assert_ok(request(
        &api_socket,
        serde_json::json!({"id":"test:handoff","method":"server.live_handoff","params":{}}),
    ));
    let replacement_pid =
        wait_for_replacement_server_pid(&runtime_dir, old_pid, Duration::from_secs(10));
    drop(spawned);
    wait_for_api(&api_socket, Duration::from_secs(10));

    let process_info = request(
        &api_socket,
        serde_json::json!({
            "id": "test:process-info",
            "method": "pane.process_info",
            "params": {"pane_id": pane_id}
        }),
    );
    let shell_pid = process_info["result"]["process_info"]["shell_pid"]
        .as_u64()
        .expect("shell pid") as libc::pid_t;
    assert_eq!(unsafe { libc::kill(shell_pid, libc::SIGHUP) }, 0);

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let panes = request(
            &api_socket,
            serde_json::json!({"id":"test:panes","method":"pane.list","params":{}}),
        );
        if panes["result"]["panes"]
            .as_array()
            .is_some_and(Vec::is_empty)
        {
            break;
        }
        assert!(Instant::now() < deadline, "handoff pane was not removed");
        thread::sleep(Duration::from_millis(20));
    }

    assert_ok(request(
        &api_socket,
        serde_json::json!({"id":"test:stop","method":"server.stop","params":{}}),
    ));
    let deadline = Instant::now() + Duration::from_secs(5);
    while Path::new(&format!("/proc/{replacement_pid}")).exists() {
        assert!(Instant::now() < deadline, "replacement server did not stop");
        thread::sleep(Duration::from_millis(20));
    }

    let session: serde_json::Value = serde_json::from_slice(
        &fs::read(config_home.join(app_dir_name()).join("session.json")).expect("saved session"),
    )
    .expect("valid session json");
    assert_eq!(session["workspaces"].as_array().map(Vec::len), Some(1));
    assert_eq!(
        session["workspaces"][0]["tabs"][0]["panes"]
            .as_object()
            .map(serde_json::Map::len),
        Some(1)
    );

    cleanup_test_base(&base);
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn live_handoff_carries_more_panes_than_one_scm_rights_message() {
    const PANES: usize = 70;

    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");

    let spawned = spawn_server(&config_home, &runtime_dir, &api_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    register_runtime_dir(&runtime_dir);
    let server_pid = spawned
        .child
        .process_id()
        .expect("test server should expose pid");

    let created = request(
        &api_socket,
        serde_json::json!({
            "id": "test:workspace:create",
            "method": "workspace.create",
            "params": {"cwd": "/tmp", "focus": true}
        }),
    );
    let workspace_id = created["result"]["workspace"]["workspace_id"]
        .as_str()
        .unwrap()
        .to_string();

    // One pane per tab keeps the layout shallow, so this exercises the fd
    // transfer rather than the depth of a single split tree.
    for index in 1..PANES {
        assert_ok(request(
            &api_socket,
            serde_json::json!({
                "id": format!("test:tab:create-{index}"),
                "method": "tab.create",
                "params": {"workspace_id": workspace_id, "focus": false}
            }),
        ));
    }
    wait_for_server_ptmx_fd_count(server_pid, PANES, Duration::from_secs(60));

    assert_ok(request(
        &api_socket,
        serde_json::json!({"id":"test:handoff","method":"server.live_handoff","params":{}}),
    ));
    let replacement_pid =
        wait_for_replacement_server_pid(&runtime_dir, server_pid, Duration::from_secs(30));
    wait_for_api(&api_socket, Duration::from_secs(30));
    wait_for_server_ptmx_fd_count(replacement_pid, PANES, Duration::from_secs(30));

    let panes = request(
        &api_socket,
        serde_json::json!({"id":"test:pane:list","method":"pane.list","params":{}}),
    );
    assert_eq!(
        panes["result"]["panes"].as_array().map(Vec::len),
        Some(PANES),
        "replacement server should report every pane after handoff"
    );

    let _ = request(
        &api_socket,
        serde_json::json!({"id":"test:stop","method":"server.stop","params":{}}),
    );
    drop(spawned);
    cleanup_test_base(&base);
}

#[test]
fn live_handoff_preserves_named_session_socket_paths() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let session_dir = config_home.join(app_dir_name()).join("sessions/work");
    let api_socket = session_dir.join("herdr.sock");
    let client_socket = session_dir.join("herdr-client.sock");

    let spawned = spawn_named_session_server(&config_home, &runtime_dir, "work");
    wait_for_socket(&api_socket, Duration::from_secs(10));
    register_runtime_dir(&runtime_dir);

    assert_ok(request(
        &api_socket,
        serde_json::json!({"id":"test:handoff","method":"server.live_handoff","params":{}}),
    ));
    drop(spawned);
    wait_for_api(&api_socket, Duration::from_secs(10));
    wait_for_socket(&client_socket, Duration::from_secs(5));
    assert!(
        !config_home.join(app_dir_name()).join("herdr.sock").exists(),
        "named handoff unexpectedly bound the default session API socket"
    );

    let _ = request(
        &api_socket,
        serde_json::json!({"id":"test:stop","method":"server.stop","params":{}}),
    );
    cleanup_test_base(&base);
}

#[test]
fn live_handoff_ignores_leaked_default_socket_env_for_named_session() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let default_session_dir = config_home.join(app_dir_name());
    let default_api_socket = default_session_dir.join("herdr.sock");
    let default_client_socket = default_session_dir.join("herdr-client.sock");
    let work_session_dir = config_home.join(app_dir_name()).join("sessions/work");
    let work_api_socket = work_session_dir.join("herdr.sock");
    let work_client_socket = work_session_dir.join("herdr-client.sock");

    let default_spawned = spawn_default_session_server(&config_home, &runtime_dir);
    wait_for_socket(&default_api_socket, Duration::from_secs(10));
    register_runtime_dir(&runtime_dir);

    let work_spawned = spawn_server_with_args_and_socket_env(
        &config_home,
        &runtime_dir,
        Some("work"),
        Some(&default_api_socket),
        Some(&default_client_socket),
    );
    wait_for_socket(&work_api_socket, Duration::from_secs(10));

    assert_ok(request(
        &work_api_socket,
        serde_json::json!({"id":"test:handoff","method":"server.live_handoff","params":{}}),
    ));
    drop(work_spawned);
    wait_for_api(&default_api_socket, Duration::from_secs(10));
    wait_for_api(&work_api_socket, Duration::from_secs(10));
    wait_for_socket(&work_client_socket, Duration::from_secs(5));

    let _ = request(
        &work_api_socket,
        serde_json::json!({"id":"test:stop-work","method":"server.stop","params":{}}),
    );
    let _ = request(
        &default_api_socket,
        serde_json::json!({"id":"test:stop-default","method":"server.stop","params":{}}),
    );
    drop(default_spawned);
    cleanup_test_base(&base);
}

#[test]
fn live_handoff_preserves_client_socket_env_without_api_socket_env() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = config_home.join(app_dir_name()).join("herdr.sock");
    let client_socket = runtime_dir.join("custom-client.sock");

    let spawned = spawn_server_with_args_and_socket_env(
        &config_home,
        &runtime_dir,
        None,
        None,
        Some(&client_socket),
    );
    wait_for_socket(&api_socket, Duration::from_secs(10));
    wait_for_socket(&client_socket, Duration::from_secs(10));
    register_runtime_dir(&runtime_dir);

    assert_ok(request(
        &api_socket,
        serde_json::json!({"id":"test:handoff","method":"server.live_handoff","params":{}}),
    ));
    drop(spawned);
    wait_for_api(&api_socket, Duration::from_secs(10));
    wait_for_socket(&client_socket, Duration::from_secs(5));

    let _ = request(
        &api_socket,
        serde_json::json!({"id":"test:stop","method":"server.stop","params":{}}),
    );
    cleanup_test_base(&base);
}

#[test]
fn live_handoff_preserves_installed_plugins() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = config_home.join(app_dir_name()).join("herdr.sock");
    let registry_path = config_home.join(app_dir_name()).join("plugins.json");
    let existing_plugin = base.join("plugins/existing");
    let added_plugin = base.join("plugins/added");
    write_plugin_manifest(&existing_plugin, "test.live-handoff-existing");
    write_plugin_manifest(&added_plugin, "test.live-handoff-added");

    let spawned = spawn_default_session_server(&config_home, &runtime_dir);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    register_runtime_dir(&runtime_dir);

    link_plugin(&api_socket, &existing_plugin);
    assert_eq!(
        listed_plugin_ids(&api_socket),
        ["test.live-handoff-existing"]
    );

    assert_ok(request(
        &api_socket,
        serde_json::json!({"id":"test:handoff","method":"server.live_handoff","params":{}}),
    ));
    drop(spawned);
    wait_for_api(&api_socket, Duration::from_secs(10));

    assert_eq!(
        listed_plugin_ids(&api_socket),
        ["test.live-handoff-existing"]
    );
    link_plugin(&api_socket, &added_plugin);
    assert_eq!(
        saved_plugin_ids(&registry_path),
        ["test.live-handoff-added", "test.live-handoff-existing"]
    );

    let _ = request(
        &api_socket,
        serde_json::json!({"id":"test:stop","method":"server.stop","params":{}}),
    );
    cleanup_test_base(&base);
}

#[test]
fn live_handoff_preserves_pane_process_io() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");
    let marker = base.join("child.pid");
    let second_marker = base.join("second-child.pid");
    let hup_marker = base.join("hup");
    let second_hup_marker = base.join("second-hup");
    let received_marker = base.join("received");
    let second_received_marker = base.join("second-received");

    let spawned = spawn_server(&config_home, &runtime_dir, &api_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    register_runtime_dir(&runtime_dir);

    let created = request(
        &api_socket,
        serde_json::json!({
            "id": "test:workspace:create",
            "method": "workspace.create",
            "params": {"cwd": "/tmp", "focus": true}
        }),
    );
    let pane_id = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();
    let split = request(
        &api_socket,
        serde_json::json!({
            "id": "test:pane:split",
            "method": "pane.split",
            "params": {
                "target_pane_id": pane_id,
                "direction": "right",
                "focus": false
            }
        }),
    );
    assert_ok(split.clone());
    let second_pane_id = split["result"]["pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();

    let command = format!(
        "sh -c 'echo READY $$ > {}; trap \"echo HUP >> {}\" HUP; while read line; do echo got:$line; echo got:$line >> {}; done'",
        marker.display(),
        hup_marker.display(),
        received_marker.display()
    );
    let second_command = format!(
        "sh -c 'echo SECOND_READY $$ > {}; trap \"echo HUP >> {}\" HUP; while read line; do echo second:$line; echo second:$line >> {}; done'",
        second_marker.display(),
        second_hup_marker.display(),
        second_received_marker.display()
    );
    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:pane:run",
            "method": "pane.send_input",
            "params": {"pane_id": pane_id, "text": command, "keys": ["Enter"]}
        }),
    ));
    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:second-pane:run",
            "method": "pane.send_input",
            "params": {"pane_id": second_pane_id, "text": second_command, "keys": ["Enter"]}
        }),
    ));
    let child_pid = wait_for_pid_marker(&marker, Duration::from_secs(5));
    let second_child_pid = wait_for_pid_marker(&second_marker, Duration::from_secs(5));
    assert_eq!(unsafe { libc::kill(child_pid as libc::pid_t, 0) }, 0);
    assert_eq!(unsafe { libc::kill(second_child_pid as libc::pid_t, 0) }, 0);

    let endpoint_generation = support::CURRENT_ENDPOINT_PROTOCOL_GENERATION;
    let mut client_stream = UnixStream::connect(&client_socket).unwrap();
    let (server_generation, error) =
        client_shell_handshake(&mut client_stream, endpoint_generation, 54, 23).unwrap();
    assert_eq!(server_generation, endpoint_generation);
    assert!(error.is_none(), "client shell handshake failed: {error:?}");
    assert!(
        wait_for_message_variant(
            &mut client_stream,
            Duration::from_secs(5),
            SERVER_MESSAGE_ENDPOINT_CONTROL,
        )
        .unwrap(),
        "client shell should receive a complete snapshot before handoff"
    );

    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:pane:before-log",
            "method": "pane.send_input",
            "params": {"pane_id": pane_id, "text": "before_replay", "keys": ["Enter"]}
        }),
    ));
    wait_for_output(&api_socket, &pane_id, "got:before_replay");

    assert_ok(request(
        &api_socket,
        serde_json::json!({"id":"test:handoff","method":"server.live_handoff","params":{}}),
    ));
    assert!(
        wait_for_message_variant(
            &mut client_stream,
            Duration::from_secs(5),
            SERVER_MESSAGE_SERVER_SHUTDOWN,
        )
        .unwrap(),
        "connected client shell should receive live-handoff shutdown"
    );
    drop(spawned);
    thread::sleep(Duration::from_millis(300));
    wait_for_api(&api_socket, Duration::from_secs(10));
    wait_for_socket(&client_socket, Duration::from_secs(5));
    assert_eq!(unsafe { libc::kill(child_pid as libc::pid_t, 0) }, 0);
    assert_eq!(unsafe { libc::kill(second_child_pid as libc::pid_t, 0) }, 0);
    assert!(
        !hup_marker.exists(),
        "pane process received HUP during handoff"
    );
    assert!(
        !second_hup_marker.exists(),
        "second pane process received HUP during handoff"
    );
    wait_for_output(&api_socket, &pane_id, "got:before_replay");

    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:pane:send",
            "method": "pane.send_input",
            "params": {"pane_id": pane_id, "text": "after-handoff", "keys": ["Enter"]}
        }),
    ));
    wait_for_file_contains(
        &received_marker,
        "got:after-handoff",
        Duration::from_secs(5),
    );
    wait_for_output(&api_socket, &pane_id, "got:after-handoff");
    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:second-pane:send",
            "method": "pane.send_input",
            "params": {"pane_id": second_pane_id, "text": "after-handoff-second", "keys": ["Enter"]}
        }),
    ));
    wait_for_file_contains(
        &second_received_marker,
        "second:after-handoff-second",
        Duration::from_secs(5),
    );
    wait_for_output(&api_socket, &second_pane_id, "second:after-handoff-sec");

    let mut reattached_shell = UnixStream::connect(&client_socket).unwrap();
    let (server_generation, error) = client_shell_handshake(
        &mut reattached_shell,
        support::CURRENT_ENDPOINT_PROTOCOL_GENERATION,
        54,
        23,
    )
    .unwrap();
    assert_eq!(
        server_generation,
        support::CURRENT_ENDPOINT_PROTOCOL_GENERATION
    );
    assert!(error.is_none(), "reattached client shell failed: {error:?}");
    wait_for_client_shell_bootstrap(&mut reattached_shell, Duration::from_secs(5))
        .expect("fresh client shell should receive restored snapshot before pane content");

    let _ = request(
        &api_socket,
        serde_json::json!({"id":"test:stop","method":"server.stop","params":{}}),
    );
    let _ = client_socket;
    cleanup_test_base(&base);
}

#[test]
fn live_handoff_preserves_keyboard_protocol_for_client_input() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");
    let script = base.join("read-raw.py");
    let ready_marker = base.join("keyboard-ready");
    let received_marker = base.join("keyboard-received");

    fs::create_dir_all(&base).unwrap();
    fs::write(
        &script,
        format!(
            r#"import os
import pathlib
import select
import sys
import tty

sys.stdout.buffer.write(b"\x1b[>5u")
sys.stdout.flush()
pathlib.Path({ready:?}).write_text("ready")
tty.setraw(sys.stdin.fileno())
ready_fds, _, _ = select.select([sys.stdin.fileno()], [], [], 5)
data = os.read(sys.stdin.fileno(), 32) if ready_fds else b""
pathlib.Path({received:?}).write_text(data.hex())
"#,
            ready = ready_marker.display().to_string(),
            received = received_marker.display().to_string()
        ),
    )
    .unwrap();

    let spawned = spawn_server(&config_home, &runtime_dir, &api_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    register_runtime_dir(&runtime_dir);

    let created = request(
        &api_socket,
        serde_json::json!({
            "id": "test:workspace:create",
            "method": "workspace.create",
            "params": {"cwd": "/tmp", "focus": true}
        }),
    );
    let pane_id = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:pane:run",
            "method": "pane.send_input",
            "params": {"pane_id": pane_id, "text": format!("python3 {}", script.display()), "keys": ["Enter"]}
        }),
    ));
    support::wait_for_file(&ready_marker, Duration::from_secs(5));

    assert_ok(request(
        &api_socket,
        serde_json::json!({"id":"test:handoff","method":"server.live_handoff","params":{}}),
    ));
    drop(spawned);
    wait_for_api(&api_socket, Duration::from_secs(10));
    wait_for_socket(&client_socket, Duration::from_secs(5));

    let mut client_stream = UnixStream::connect(&client_socket).unwrap();
    let (server_generation, error) = client_shell_handshake(
        &mut client_stream,
        support::CURRENT_ENDPOINT_PROTOCOL_GENERATION,
        54,
        23,
    )
    .unwrap();
    assert_eq!(
        server_generation,
        support::CURRENT_ENDPOINT_PROTOCOL_GENERATION
    );
    assert!(error.is_none(), "client shell handshake failed: {error:?}");
    wait_for_client_shell_bootstrap(&mut client_stream, Duration::from_secs(5))
        .expect("client shell should receive restored state before sending input");
    send_client_shell_shift_enter(&mut client_stream, &pane_id).unwrap();

    wait_for_file_contains(&received_marker, "1b5b31333b3275", Duration::from_secs(5));

    let _ = request(
        &api_socket,
        serde_json::json!({"id":"test:stop","method":"server.stop","params":{}}),
    );
    cleanup_test_base(&base);
}

#[test]
fn live_handoff_preserves_modify_other_keys_for_client_input() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");
    let script = base.join("read-raw.py");
    let ready_marker = base.join("modify-ready");
    let received_marker = base.join("modify-received");

    fs::create_dir_all(&base).unwrap();
    fs::write(
        &script,
        format!(
            r#"import os
import pathlib
import select
import sys
import tty

sys.stdout.buffer.write(b"\x1b[>4;2m")
sys.stdout.flush()
pathlib.Path({ready:?}).write_text("ready")
tty.setraw(sys.stdin.fileno())
ready_fds, _, _ = select.select([sys.stdin.fileno()], [], [], 5)
data = os.read(sys.stdin.fileno(), 32) if ready_fds else b""
pathlib.Path({received:?}).write_text(data.hex())
"#,
            ready = ready_marker.display().to_string(),
            received = received_marker.display().to_string()
        ),
    )
    .unwrap();

    let spawned = spawn_server(&config_home, &runtime_dir, &api_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    register_runtime_dir(&runtime_dir);

    let created = request(
        &api_socket,
        serde_json::json!({
            "id": "test:workspace:create",
            "method": "workspace.create",
            "params": {"cwd": "/tmp", "focus": true}
        }),
    );
    let pane_id = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:pane:run",
            "method": "pane.send_input",
            "params": {"pane_id": pane_id, "text": format!("python3 {}", script.display()), "keys": ["Enter"]}
        }),
    ));
    support::wait_for_file(&ready_marker, Duration::from_secs(5));

    assert_ok(request(
        &api_socket,
        serde_json::json!({"id":"test:handoff","method":"server.live_handoff","params":{}}),
    ));
    drop(spawned);
    wait_for_api(&api_socket, Duration::from_secs(10));
    wait_for_socket(&client_socket, Duration::from_secs(5));

    let mut client_stream = UnixStream::connect(&client_socket).unwrap();
    let (server_generation, error) = client_shell_handshake(
        &mut client_stream,
        support::CURRENT_ENDPOINT_PROTOCOL_GENERATION,
        54,
        23,
    )
    .unwrap();
    assert_eq!(
        server_generation,
        support::CURRENT_ENDPOINT_PROTOCOL_GENERATION
    );
    assert!(error.is_none(), "client shell handshake failed: {error:?}");
    wait_for_client_shell_bootstrap(&mut client_stream, Duration::from_secs(5))
        .expect("client shell should receive restored state before sending input");
    send_client_shell_shift_enter(&mut client_stream, &pane_id).unwrap();

    wait_for_file_contains(
        &received_marker,
        "1b5b32373b323b31337e",
        Duration::from_secs(5),
    );

    let _ = request(
        &api_socket,
        serde_json::json!({"id":"test:stop","method":"server.stop","params":{}}),
    );
    cleanup_test_base(&base);
}

#[test]
fn live_handoff_accepts_canonical_pane_id_from_child_env() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let pane_id_marker = base.join("pane-id");

    let spawned = spawn_server(&config_home, &runtime_dir, &api_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    register_runtime_dir(&runtime_dir);

    let created = request(
        &api_socket,
        serde_json::json!({
            "id": "test:workspace:create",
            "method": "workspace.create",
            "params": {"cwd": "/tmp", "focus": true}
        }),
    );
    let pane_id = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:pane:print-id",
            "method": "pane.send_input",
            "params": {"pane_id": pane_id, "text": format!("printf '%s' \"$HERDR_PANE_ID\" > {}", pane_id_marker.display()), "keys": ["Enter"]}
        }),
    ));
    let old_pane_id = wait_for_file_contains(&pane_id_marker, &pane_id, Duration::from_secs(5));
    assert!(
        old_pane_id == pane_id,
        "unexpected pane id from env: {old_pane_id:?}"
    );

    assert_ok(request(
        &api_socket,
        serde_json::json!({"id":"test:handoff","method":"server.live_handoff","params":{}}),
    ));
    drop(spawned);
    wait_for_api(&api_socket, Duration::from_secs(10));

    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:old-pane-report",
            "method": "pane.report_agent",
            "params": {
                "pane_id": old_pane_id,
                "source": "handoff-test",
                "agent": "pi",
                "state": "working"
            }
        }),
    ));
    let agents = request(
        &api_socket,
        serde_json::json!({"id":"test:agent-list","method":"agent.list","params":{}}),
    );
    let found = agents["result"]["agents"]
        .as_array()
        .unwrap()
        .iter()
        .any(|agent| {
            agent["agent"].as_str() == Some("pi")
                && agent["agent_status"].as_str() == Some("working")
        });
    assert!(
        found,
        "old pane id report did not update restored pane: {agents}"
    );

    let _ = request(
        &api_socket,
        serde_json::json!({"id":"test:stop","method":"server.stop","params":{}}),
    );
    cleanup_test_base(&base);
}

#[test]
fn live_handoff_keeps_unmanaged_agent_name_bound_to_saved_session() {
    use std::os::unix::fs::PermissionsExt;

    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let old_session = base.join("old-session.jsonl");
    let new_session = base.join("new-session.jsonl");
    let started_marker = base.join("agent-started");
    let fake_pi = base.join("pi");
    fs::create_dir_all(&base).unwrap();
    fs::write(
        &fake_pi,
        format!(
            "#!/bin/sh\nexport HERDR_AGENT=pi\necho started > {}\n/bin/sleep 30\n:\n",
            started_marker.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&fake_pi, fs::Permissions::from_mode(0o755)).unwrap();
    // 测试进程自己扮演 pi 扩展发 `herdr:pi` 上报，它不在窗格进程树里：关掉上报来源校验。
    let spawned = spawn_server_with_config_and_env(
        &config_home,
        &runtime_dir,
        &api_socket,
        "onboarding = false\n[server]\nverify_report_process = false\n",
        &[],
    );
    wait_for_socket(&api_socket, Duration::from_secs(10));
    register_runtime_dir(&runtime_dir);
    let created = request(
        &api_socket,
        serde_json::json!({
            "id": "test:workspace:create",
            "method": "workspace.create",
            "params": {"cwd": "/tmp", "focus": true}
        }),
    );
    let pane_id = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:pane:start-agent",
            "method": "pane.send_input",
            "params": {"pane_id": pane_id, "text": fake_pi, "keys": ["Enter"]}
        }),
    ));
    support::wait_for_file(&started_marker, Duration::from_secs(5));
    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:agent:session",
            "method": "pane.report_agent_session",
            "params": {
                "pane_id": pane_id,
                "source": "herdr:pi",
                "agent": "pi",
                "seq": 1,
                "agent_session_path": old_session,
                "session_start_source": "startup"
            }
        }),
    ));
    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:agent:report",
            "method": "pane.report_agent",
            "params": {
                "pane_id": pane_id,
                "source": "herdr:pi",
                "agent": "pi",
                "state": "idle",
                "seq": 2,
                "agent_session_path": old_session
            }
        }),
    ));
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let response = request(
            &api_socket,
            serde_json::json!({
                "id": "test:agent:wait-for-process",
                "method": "agent.get",
                "params": {"target": pane_id}
            }),
        );
        if response.get("result").is_some() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "agent process was not detected: {response}"
        );
        thread::sleep(Duration::from_millis(25));
    }
    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:agent:rename",
            "method": "agent.rename",
            "params": {"target": pane_id, "name": "reviewer"}
        }),
    ));

    assert_ok(request(
        &api_socket,
        serde_json::json!({"id":"test:handoff","method":"server.live_handoff","params":{}}),
    ));
    drop(spawned);
    wait_for_api(&api_socket, Duration::from_secs(10));

    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:agent:new-session",
            "method": "pane.report_agent_session",
            "params": {
                "pane_id": pane_id,
                "source": "herdr:pi",
                "agent": "pi",
                "seq": 3,
                "agent_session_path": new_session,
                "session_start_source": "new"
            }
        }),
    ));
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let old_name = request(
            &api_socket,
            serde_json::json!({
                "id": "test:agent:get-old-name",
                "method": "agent.get",
                "params": {"target": "reviewer"}
            }),
        );
        if old_name["error"]["code"] == "agent_not_found" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "old session alias was not cleared: {old_name}"
        );
        thread::sleep(Duration::from_millis(25));
    }

    let _ = request(
        &api_socket,
        serde_json::json!({"id":"test:stop","method":"server.stop","params":{}}),
    );
    cleanup_test_base(&base);
}

#[test]
fn live_handoff_keeps_agent_started_pane_after_agent_exits() {
    use std::os::unix::fs::PermissionsExt;

    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let started_marker = base.join("agent-started");
    let exited_marker = base.join("agent-exited");
    let ready_marker = base.join("shell-ready");
    let shell_marker = base.join("shell-after-agent");
    let bin = base.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let delayed_shell = bin.join("delayed-shell");
    fs::write(&delayed_shell, "#!/bin/sh\n/bin/sleep 0.4\nexec /bin/sh\n").unwrap();
    fs::set_permissions(&delayed_shell, fs::Permissions::from_mode(0o755)).unwrap();
    let fake_pi = bin.join("pi");
    fs::write(
        &fake_pi,
        format!(
            "#!/bin/sh\nexport HERDR_AGENT=pi\necho started > {}\n/bin/sleep 1\necho exited > {}\n",
            started_marker.display(),
            exited_marker.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&fake_pi, fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:/bin:/usr/bin", bin.display());

    let spawned = spawn_server_with_env(
        &config_home,
        &runtime_dir,
        &api_socket,
        &[
            ("PATH", path.as_str()),
            ("SHELL", delayed_shell.to_str().unwrap()),
        ],
    );
    wait_for_socket(&api_socket, Duration::from_secs(10));
    register_runtime_dir(&runtime_dir);
    let workspace = request(
        &api_socket,
        serde_json::json!({
            "id": "test:workspace-create",
            "method": "workspace.create",
            "params": { "cwd": "/tmp", "focus": false }
        }),
    );
    assert_ok(workspace.clone());
    let pane_id = workspace["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();

    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:shell-ready",
            "method": "pane.send_input",
            "params": {
                "pane_id": pane_id,
                "text": format!("printf ready > {}", ready_marker.display()),
                "keys": ["Enter"]
            }
        }),
    ));
    // Creation acknowledges the PTY, not an idle interactive shell. A real
    // shell command must execute before this raw agent.start request.
    support::wait_for_file(&ready_marker, Duration::from_secs(5));

    let started = request(
        &api_socket,
        serde_json::json!({
            "id": "test:agent-start",
            "method": "agent.start",
            "params": {
                "name": "handoff-agent",
                "kind": "pi",
                "pane_id": pane_id,
                "timeout_ms": 5000
            }
        }),
    );
    assert_ok(started);
    support::wait_for_file(&started_marker, Duration::from_secs(5));

    assert_ok(request(
        &api_socket,
        serde_json::json!({"id":"test:handoff","method":"server.live_handoff","params":{}}),
    ));
    drop(spawned);
    wait_for_api(&api_socket, Duration::from_secs(10));
    support::wait_for_file(&exited_marker, Duration::from_secs(5));
    thread::sleep(Duration::from_millis(300));

    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:pane:shell-after-agent",
            "method": "pane.send_input",
            "params": {"pane_id": pane_id, "text": format!("echo alive > {}", shell_marker.display()), "keys": ["Enter"]}
        }),
    ));
    support::wait_for_file(&shell_marker, Duration::from_secs(5));

    let _ = request(
        &api_socket,
        serde_json::json!({"id":"test:stop","method":"server.stop","params":{}}),
    );
    cleanup_test_base(&base);
}

#[test]
fn live_handoff_keeps_shell_pane_after_foreground_process_exits() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let started_marker = base.join("foreground-started");
    let exited_marker = base.join("foreground-exited");
    let shell_marker = base.join("shell-after-foreground");

    let spawned = spawn_server(&config_home, &runtime_dir, &api_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    register_runtime_dir(&runtime_dir);

    let created = request(
        &api_socket,
        serde_json::json!({
            "id": "test:workspace:create",
            "method": "workspace.create",
            "params": {"cwd": "/tmp", "focus": true}
        }),
    );
    let pane_id = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();
    let command = format!(
        "sh -c 'echo started > {}; sleep 1; echo exited > {}'",
        started_marker.display(),
        exited_marker.display()
    );
    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:pane:run-foreground",
            "method": "pane.send_input",
            "params": {"pane_id": pane_id, "text": command, "keys": ["Enter"]}
        }),
    ));
    support::wait_for_file(&started_marker, Duration::from_secs(5));

    assert_ok(request(
        &api_socket,
        serde_json::json!({"id":"test:handoff","method":"server.live_handoff","params":{}}),
    ));
    drop(spawned);
    wait_for_api(&api_socket, Duration::from_secs(10));
    support::wait_for_file(&exited_marker, Duration::from_secs(5));

    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:pane:shell-after-foreground",
            "method": "pane.send_input",
            "params": {"pane_id": pane_id, "text": format!("echo alive > {}", shell_marker.display()), "keys": ["Enter"]}
        }),
    ));
    support::wait_for_file(&shell_marker, Duration::from_secs(5));

    let _ = request(
        &api_socket,
        serde_json::json!({"id":"test:stop","method":"server.stop","params":{}}),
    );
    cleanup_test_base(&base);
}

#[test]
fn live_handoff_preserves_python_http_server() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");
    let web_root = base.join("web");
    fs::create_dir_all(&web_root).unwrap();
    fs::write(
        web_root.join("index.html"),
        "hello-from-python-before-and-after",
    )
    .unwrap();
    let port = unused_local_port();

    let spawned = spawn_server(&config_home, &runtime_dir, &api_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    register_runtime_dir(&runtime_dir);

    let created = request(
        &api_socket,
        serde_json::json!({
            "id": "test:workspace:create",
            "method": "workspace.create",
            "params": {"cwd": web_root, "focus": true}
        }),
    );
    let pane_id = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();
    let mut context = HttpContext {
        test: "live_handoff_preserves_python_http_server",
        phase: "pre-handoff",
        session: "default",
        socket: &api_socket,
        pane: &pane_id,
        web_root: &web_root,
        config_home: &config_home,
    };

    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:pane:run-python",
            "method": "pane.send_input",
            "params": {
                "pane_id": pane_id,
                "text": python_http_command(&web_root, port),
                "keys": ["Enter"]
            }
        }),
    ));
    wait_for_http_contains(
        port,
        "hello-from-python-before-and-after",
        Duration::from_secs(10),
        &context,
    );

    assert_ok(request(
        &api_socket,
        serde_json::json!({"id":"test:handoff","method":"server.live_handoff","params":{}}),
    ));
    drop(spawned);
    wait_for_api(&api_socket, Duration::from_secs(10));
    context.phase = "post-handoff";
    wait_for_http_contains(
        port,
        "hello-from-python-before-and-after",
        Duration::from_secs(10),
        &context,
    );

    let _ = request(
        &api_socket,
        serde_json::json!({"id":"test:stop","method":"server.stop","params":{}}),
    );
    let _ = client_socket;
    cleanup_test_base(&base);
}

#[test]
fn live_handoff_preserves_http_servers_across_multiple_sessions() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let sessions = [
        (None, config_home.join(app_dir_name()).join("herdr.sock")),
        (
            Some("work"),
            config_home
                .join(app_dir_name())
                .join("sessions/work/herdr.sock"),
        ),
    ];
    let mut spawned = Vec::new();
    let mut ports = Vec::new();

    for (session_name, api_socket) in &sessions {
        let label = session_name.unwrap_or("default");
        let web_root = base.join(format!("web-{label}"));
        fs::create_dir_all(&web_root).unwrap();
        fs::write(web_root.join("index.html"), format!("hello-from-{label}")).unwrap();
        let port = unused_local_port();
        let server = if let Some(session_name) = session_name {
            spawn_named_session_server(&config_home, &runtime_dir, session_name)
        } else {
            spawn_default_session_server(&config_home, &runtime_dir)
        };
        wait_for_socket(api_socket, Duration::from_secs(10));
        let created = request(
            api_socket,
            serde_json::json!({
                "id": "test:workspace:create",
                "method": "workspace.create",
                "params": {"cwd": web_root, "focus": true}
            }),
        );
        let pane_id = created["result"]["root_pane"]["pane_id"]
            .as_str()
            .unwrap()
            .to_string();
        assert_ok(request(
            api_socket,
            serde_json::json!({
                "id": "test:pane:run-python",
                "method": "pane.send_input",
                "params": {
                    "pane_id": pane_id,
                    "text": python_http_command(&web_root, port),
                    "keys": ["Enter"]
                }
            }),
        ));
        wait_for_http_contains(
            port,
            &format!("hello-from-{label}"),
            Duration::from_secs(10),
            &HttpContext {
                test: "live_handoff_preserves_http_servers_across_multiple_sessions",
                phase: "pre-handoff",
                session: label,
                socket: api_socket,
                pane: &pane_id,
                web_root: &web_root,
                config_home: &config_home,
            },
        );
        spawned.push(server);
        ports.push((port, label, api_socket, pane_id, web_root));
    }
    register_runtime_dir(&runtime_dir);

    for (_session_name, api_socket) in &sessions {
        assert_ok(request(
            api_socket,
            serde_json::json!({"id":"test:handoff","method":"server.live_handoff","params":{}}),
        ));
    }
    drop(spawned);

    for (_session_name, api_socket) in &sessions {
        wait_for_api(api_socket, Duration::from_secs(10));
    }
    for (port, label, api_socket, pane_id, web_root) in ports {
        wait_for_http_contains(
            port,
            &format!("hello-from-{label}"),
            Duration::from_secs(10),
            &HttpContext {
                test: "live_handoff_preserves_http_servers_across_multiple_sessions",
                phase: "post-handoff",
                session: label,
                socket: api_socket,
                pane: &pane_id,
                web_root: &web_root,
                config_home: &config_home,
            },
        );
    }

    for (_session_name, api_socket) in &sessions {
        let _ = request(
            api_socket,
            serde_json::json!({"id":"test:stop","method":"server.stop","params":{}}),
        );
    }
    cleanup_test_base(&base);
}

#[test]
fn live_handoff_bad_expected_protocol_rolls_back_old_server() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let marker = base.join("child.pid");
    let received_marker = base.join("received");

    let spawned = spawn_server(&config_home, &runtime_dir, &api_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    register_runtime_dir(&runtime_dir);

    let created = request(
        &api_socket,
        serde_json::json!({
            "id": "test:workspace:create",
            "method": "workspace.create",
            "params": {"cwd": "/tmp", "focus": true}
        }),
    );
    let pane_id = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();
    let command = format!(
        "sh -c 'echo READY $$ > {}; while read line; do echo got:$line; echo got:$line >> {}; done'",
        marker.display(),
        received_marker.display()
    );
    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:pane:run",
            "method": "pane.send_input",
            "params": {"pane_id": pane_id, "text": command, "keys": ["Enter"]}
        }),
    ));
    let child_pid = wait_for_pid_marker(&marker, Duration::from_secs(5));

    let failed = request(
        &api_socket,
        serde_json::json!({
            "id": "test:bad-handoff",
            "method": "server.live_handoff",
            "params": {"expected_protocol": 999999}
        }),
    );
    assert!(
        failed.get("error").is_some(),
        "bad protocol handoff should fail: {failed}"
    );
    wait_for_api(&api_socket, Duration::from_secs(5));
    assert_eq!(unsafe { libc::kill(child_pid as libc::pid_t, 0) }, 0);

    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:pane:send-after-failed-handoff",
            "method": "pane.send_input",
            "params": {"pane_id": pane_id, "text": "after-failed-handoff", "keys": ["Enter"]}
        }),
    ));
    wait_for_file_contains(
        &received_marker,
        "got:after-failed-handoff",
        Duration::from_secs(5),
    );
    wait_for_output(&api_socket, &pane_id, "got:after-failed-handoff");

    let _ = request(
        &api_socket,
        serde_json::json!({"id":"test:stop","method":"server.stop","params":{}}),
    );
    drop(spawned);
    cleanup_test_base(&base);
}

fn live_handoff_import_failure_rolls_back_old_server_at(failure_point: &str) {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");
    let marker = base.join("child.pid");
    let received_marker = base.join("received");

    let spawned = spawn_server_with_env(
        &config_home,
        &runtime_dir,
        &api_socket,
        &[("HERDR_TEST_HANDOFF_IMPORT_FAIL", failure_point)],
    );
    wait_for_socket(&api_socket, Duration::from_secs(10));
    register_runtime_dir(&runtime_dir);

    let created = request(
        &api_socket,
        serde_json::json!({
            "id": "test:workspace:create",
            "method": "workspace.create",
            "params": {"cwd": "/tmp", "focus": true}
        }),
    );
    let pane_id = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();
    let command = format!(
        "sh -c 'echo READY $$ > {}; while read line; do echo got:$line; echo got:$line >> {}; done'",
        marker.display(),
        received_marker.display()
    );
    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:pane:run",
            "method": "pane.send_input",
            "params": {"pane_id": pane_id, "text": command, "keys": ["Enter"]}
        }),
    ));
    let child_pid = wait_for_pid_marker(&marker, Duration::from_secs(5));

    let failed = request(
        &api_socket,
        serde_json::json!({"id":"test:handoff-fail","method":"server.live_handoff","params":{}}),
    );
    assert!(
        failed.get("error").is_some(),
        "{failure_point} handoff should fail: {failed}"
    );
    wait_for_api(&api_socket, Duration::from_secs(10));
    wait_for_socket(&client_socket, Duration::from_secs(5));
    assert_eq!(unsafe { libc::kill(child_pid as libc::pid_t, 0) }, 0);

    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:pane:send-after-import-failure",
            "method": "pane.send_input",
            "params": {"pane_id": pane_id, "text": failure_point, "keys": ["Enter"]}
        }),
    ));
    wait_for_file_contains(
        &received_marker,
        &format!("got:{failure_point}"),
        Duration::from_secs(5),
    );

    let _ = request(
        &api_socket,
        serde_json::json!({"id":"test:stop","method":"server.stop","params":{}}),
    );
    drop(spawned);
    cleanup_test_base(&base);
}

#[test]
fn live_handoff_after_restored_failure_rolls_back_old_server() {
    live_handoff_import_failure_rolls_back_old_server_at("after_restored");
}
