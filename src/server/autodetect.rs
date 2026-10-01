//! Auto-detect launch behavior for the `herdr` command.
//!
//! When the user runs `herdr` with no subcommand:
//! 1. Check whether a server is running: Unix connects to the client socket, Windows
//!    reads the server status from the JSON API and keeps it for the compatibility check
//! 2. If no server → spawn one as a background daemon → wait until its client socket
//!    accepts connections (up to 15s)
//! 3. Attach as a client to the server

use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use tracing::info;

use super::socket_paths::client_socket_path;

/// Maximum time to wait for the server's client socket to become ready
/// after spawning the server process.
const SERVER_READY_TIMEOUT: Duration = Duration::from_secs(15);

/// Poll interval when waiting for the server socket to appear.
const SOCKET_POLL_INTERVAL: Duration = Duration::from_millis(50);
/// START-03：冷启动最初一段用细粒度轮询——server 通常在几十毫秒内就绪，
/// 50 ms 粒度会平白多等一个周期。超出该窗口后退回 `SOCKET_POLL_INTERVAL`
/// （避免长时间空转）。
const SOCKET_POLL_INTERVAL_FAST: Duration = Duration::from_millis(5);
const SOCKET_POLL_FAST_WINDOW: Duration = Duration::from_millis(500);

/// Timeout for checking the stable JSON API before attaching to the binary protocol socket.
const STATUS_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

/// Private daemon-start hint used to seed a fresh headless server from the
/// directory where the user ran `herdr`.
pub(crate) const STARTUP_CWD_ENV_VAR: &str = "HERDR_STARTUP_CWD";

// ---------------------------------------------------------------------------
// Server detection
// ---------------------------------------------------------------------------

/// Checks whether a herdr server is listening on the client socket at `socket_path`.
///
/// Connecting succeeds only while a server listens. A missing socket file, or a stale
/// one left by a crashed server (connect returns `ConnectionRefused`), means no server
/// is running. The server sees the probe as a client that disconnects before its
/// handshake.
#[cfg(not(windows))]
fn is_server_listening_at(socket_path: &Path) -> bool {
    if !socket_path.exists() {
        return false;
    }

    match crate::ipc::connect_local_stream(socket_path) {
        Ok(_) => true,
        Err(err)
            if matches!(
                err.kind(),
                io::ErrorKind::ConnectionRefused | io::ErrorKind::TimedOut
            ) =>
        {
            // Socket file exists but nobody is listening — stale socket.
            false
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            // Socket file disappeared between exists() and connect().
            false
        }
        Err(err) => {
            // Other errors (permission denied, etc.) — assume not listening.
            tracing::warn!(err = %err, "unexpected error checking server socket");
            false
        }
    }
}

#[cfg(windows)]
fn read_server_status() -> io::Result<Option<crate::api::RuntimeStatus>> {
    crate::api::read_runtime_status_at(&crate::api::socket_path(), STATUS_REQUEST_TIMEOUT)
}

/// A running server found by [`find_running_server`].
pub(crate) struct RunningServer {
    /// Status read while detecting the server. Windows detects through the status API,
    /// so callers reuse it instead of pinging again.
    status: Option<crate::api::RuntimeStatus>,
}

impl RunningServer {
    /// The status read during detection, or a status read now within `timeout`.
    pub(crate) fn into_status(
        self,
        timeout: Duration,
    ) -> io::Result<Option<crate::api::RuntimeStatus>> {
        match self.status {
            Some(status) => Ok(Some(status)),
            None => crate::api::read_runtime_status_at(&crate::api::socket_path(), timeout),
        }
    }
}

/// Finds a running server: Unix connects to the client socket at `socket_path`,
/// Windows reads the server status from the JSON API.
pub(crate) fn find_running_server(socket_path: &Path) -> Option<RunningServer> {
    #[cfg(windows)]
    {
        let _ = socket_path;
        read_server_status()
            .ok()
            .flatten()
            .map(|status| RunningServer {
                status: Some(status),
            })
    }

    #[cfg(not(windows))]
    {
        is_server_listening_at(socket_path).then_some(RunningServer { status: None })
    }
}

fn validate_running_server_compatibility(
    saved_federation: bool,
    server: RunningServer,
) -> io::Result<()> {
    validate_server_status_compatibility(
        server.into_status(STATUS_REQUEST_TIMEOUT)?,
        saved_federation,
    )
}

fn validate_server_status_compatibility(
    status: Option<crate::api::RuntimeStatus>,
    saved_federation: bool,
) -> io::Result<()> {
    let Some(status) = status else {
        return Err(io::Error::other(format!(
            "a herdr server is listening, but its status API is unavailable.\n\n{}\nIf that fails, stop the old server process manually.",
            crate::session::active_restart_after_update_guidance()
        )));
    };

    let capabilities = status.capabilities.as_ref();
    let endpoint_generation =
        capabilities.and_then(|capabilities| capabilities.endpoint_protocol_generation);
    let surface_interest = capabilities.is_some_and(|capabilities| capabilities.surface_interest);
    if endpoint_generation == Some(crate::protocol::endpoint::ENDPOINT_PROTOCOL_GENERATION)
        && (!saved_federation || surface_interest)
    {
        return Ok(());
    }

    let requirement = if saved_federation && !surface_interest {
        "saved SSH machines require surface lifecycle support"
    } else {
        "the stable endpoint generation is incompatible"
    };
    Err(io::Error::other(format!(
        "This session needs one final server update before Herdr can attach ({requirement}).\n\nserver: v{} endpoint generation {}\nclient: v{} endpoint generation {}\n\n{}",
        status.version.as_deref().unwrap_or("unknown"),
        endpoint_generation
            .map(|value| value.to_string())
            .unwrap_or_else(|| "unavailable".to_string()),
        crate::build_info::version(),
        crate::protocol::endpoint::ENDPOINT_PROTOCOL_GENERATION,
        crate::session::active_restart_after_update_guidance()
    )))
}

// ---------------------------------------------------------------------------
// Server spawning
// ---------------------------------------------------------------------------

/// Spawns the herdr server as a background daemon process.
///
/// The server process is fully detached:
/// - Runs in its own session (setsid) so it survives the client exiting
/// - Stdin/stdout/stderr are redirected to /dev/null
/// - Inherits relevant environment variables (`XDG_CONFIG_HOME`, `HERDR_SESSION`,
///   socket overrides, etc.), except inherited socket overrides are cleared when
///   this CLI invocation explicitly selected a session.
///
/// Returns the PID of the spawned server process.
pub fn spawn_server_daemon() -> io::Result<u32> {
    let exe = std::env::current_exe().map_err(|err| {
        io::Error::new(
            err.kind(),
            format!("failed to determine herdr executable path: {err}"),
        )
    })?;

    info!(exe = %exe.display(), "spawning server daemon");

    let mut command = build_server_daemon_command(exe);

    let pid =
        crate::platform::launch_server_daemon_command(&mut command).map_err(|err: io::Error| {
            io::Error::new(err.kind(), format!("failed to spawn herdr server: {err}"))
        })?;
    info!(pid, "server daemon spawned");

    Ok(pid)
}

fn build_server_daemon_command(exe: PathBuf) -> Command {
    let mut command = Command::new(&exe);
    command
        .arg("server")
        // Redirect stdio to /dev/null
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    crate::platform::detach_server_daemon_command(&mut command);

    match std::env::current_dir() {
        Ok(cwd) => {
            command.env(STARTUP_CWD_ENV_VAR, cwd);
        }
        Err(_) => {
            command.env_remove(STARTUP_CWD_ENV_VAR);
        }
    }

    if crate::session::explicit_session_requested() {
        command
            .env_remove(crate::api::SOCKET_PATH_ENV_VAR)
            .env_remove("HERDR_CLIENT_SOCKET_PATH");
    }

    command
}

// ---------------------------------------------------------------------------
// Socket readiness
// ---------------------------------------------------------------------------

/// Waits for the server's client socket to become ready for connections.
///
/// Polls the socket path at regular intervals until the listener accepts
/// connections or the timeout elapses: Unix connects, Windows checks for a
/// listening pipe instance without connecting. Returns an error if the server
/// doesn't become ready within the timeout.
pub fn wait_for_server_socket(socket_path: &Path, timeout: Duration) -> io::Result<()> {
    let started = std::time::Instant::now();
    let deadline = started + timeout;

    while std::time::Instant::now() < deadline {
        #[cfg(windows)]
        if crate::ipc::local_listener_accepting(socket_path)? {
            info!(path = %socket_path.display(), "server client pipe listening");
            return Ok(());
        }

        #[cfg(not(windows))]
        if is_server_listening_at(socket_path) {
            info!(path = %socket_path.display(), "server socket ready");
            return Ok(());
        }
        let interval = if started.elapsed() < SOCKET_POLL_FAST_WINDOW {
            SOCKET_POLL_INTERVAL_FAST
        } else {
            SOCKET_POLL_INTERVAL
        };
        std::thread::sleep(interval);
    }

    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        format!(
            "server did not become ready within {}s (socket: {}). The background server may still be starting; try `herdr` again, or check {}",
            timeout.as_secs(),
            socket_path.display(),
            crate::session::data_dir().join("herdr-server.log").display()
        ),
    ))
}

// ---------------------------------------------------------------------------
// Auto-detect launch
// ---------------------------------------------------------------------------

/// Performs auto-detect launch: check for server, spawn if needed, then
/// attach as a thin client.
///
/// This is the entry point called from `main.rs` when the user runs `herdr`
/// without a subcommand.
///
/// Flow:
/// 1. Check if a server is listening on the client socket
/// 2. If no server → spawn server daemon → wait for socket readiness
/// 3. Run the thin client (which connects to the server)
///
/// CFG-01：`startup_config` 是调用方（main）已经加载好的配置；启动路径原先会在
/// 这里再 `Config::load()` 一次（第二次解析 + 诊断）。
pub fn auto_detect_launch(
    saved_federation: bool,
    startup_config: crate::config::LoadedConfig,
) -> io::Result<()> {
    // The client requires terminal geometry before it can attach. Reject an
    // unusable terminal before socket lookup creates directories or starts a daemon.
    crate::platform::terminal_grid_size().map_err(|err| {
        io::Error::new(
            err.kind(),
            format!("cannot attach without a usable terminal: {err}; run inside a terminal"),
        )
    })?;
    let socket_path = client_socket_path();
    info!(path = %socket_path.display(), "auto-detect launch starting");

    let startup = match find_running_server(&socket_path) {
        Some(server) => {
            info!("server already running, attaching as client");
            if saved_federation {
                Ok(())
            } else {
                validate_running_server_compatibility(false, server)
            }
        }
        None => {
            info!("no server running, spawning server daemon");
            spawn_server_daemon()
                .and_then(|_| wait_for_server_socket(&socket_path, SERVER_READY_TIMEOUT))
        }
    };
    if let Err(error) = startup {
        if !saved_federation {
            return Err(error);
        }
        tracing::warn!(%error, "Local startup failed; keeping saved machines available");
    }

    // Now attach as a thin client.
    crate::client::run_client_with_startup_config(startup_config)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Uses the crate-wide lock and restores process environment variables on drop.
#[cfg(test)]
fn env_lock() -> &'static crate::config::TestEnvLock {
    crate::config::test_config_env_lock()
}

#[cfg(test)]
mod status_tests {
    use super::*;

    fn status(
        endpoint_protocol_generation: Option<u32>,
        surface_interest: bool,
    ) -> crate::api::RuntimeStatus {
        crate::api::RuntimeStatus {
            version: Some("0.9.2".into()),
            protocol: Some(crate::protocol::PROTOCOL_VERSION),
            capabilities: Some(crate::api::schema::ServerCapabilities {
                live_handoff: false,
                detached_server_daemon: true,
                endpoint_protocol_generation,
                surface_interest,
                health_check: true,
                ssh_agent_registration: false,
            }),
        }
    }

    fn compatible_status() -> crate::api::RuntimeStatus {
        status(
            Some(crate::protocol::endpoint::ENDPOINT_PROTOCOL_GENERATION),
            true,
        )
    }

    #[test]
    fn status_from_detection_is_reused_without_another_ping() {
        let _guard = env_lock().lock().unwrap();
        // A second ping would find no status API here and fail the check.
        let missing = std::env::temp_dir().join(format!(
            "herdr-autodetect-no-api-{}.sock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&missing);
        std::env::set_var(crate::api::SOCKET_PATH_ENV_VAR, &missing);

        let reused = validate_running_server_compatibility(
            false,
            RunningServer {
                status: Some(compatible_status()),
            },
        );
        let read_again =
            validate_running_server_compatibility(false, RunningServer { status: None });

        std::env::remove_var(crate::api::SOCKET_PATH_ENV_VAR);
        assert!(reused.is_ok(), "unexpected error: {reused:?}");
        let error = read_again.unwrap_err().to_string();
        assert!(
            error.contains("status API is unavailable"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn status_compatibility_requires_the_stable_endpoint_generation() {
        assert!(validate_server_status_compatibility(Some(compatible_status()), false).is_ok());
        assert!(validate_server_status_compatibility(Some(compatible_status()), true).is_ok());

        let missing = validate_server_status_compatibility(None, false)
            .unwrap_err()
            .to_string();
        assert!(missing.contains("status API is unavailable"), "{missing}");

        let old = validate_server_status_compatibility(Some(status(None, false)), false)
            .unwrap_err()
            .to_string();
        assert!(
            old.contains("the stable endpoint generation is incompatible"),
            "{old}"
        );

        let without_surface_interest = validate_server_status_compatibility(
            Some(status(
                Some(crate::protocol::endpoint::ENDPOINT_PROTOCOL_GENERATION),
                false,
            )),
            true,
        )
        .unwrap_err()
        .to_string();
        assert!(
            without_surface_interest.contains("saved SSH machines require surface lifecycle"),
            "{without_surface_interest}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_readiness_wait_leaves_no_probe_connection_behind() {
        use interprocess::local_socket::traits::Listener as _;
        use interprocess::local_socket::ListenerNonblockingMode;

        let path = std::env::temp_dir().join(format!(
            "herdr-autodetect-ready-{}-{}.sock",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_file(&path);
        let timed_out = wait_for_server_socket(&path, Duration::from_millis(50)).unwrap_err();
        assert_eq!(timed_out.kind(), io::ErrorKind::TimedOut);

        let listener = crate::ipc::bind_local_listener(&path).unwrap();
        listener
            .set_nonblocking(ListenerNonblockingMode::Accept)
            .unwrap();
        wait_for_server_socket(&path, Duration::from_secs(2)).unwrap();

        let pending = listener.accept();
        assert!(
            matches!(&pending, Err(err) if err.kind() == io::ErrorKind::WouldBlock),
            "readiness must not reach the server as a client"
        );
        drop(listener);
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;

    // socket 覆盖、会话名与 XDG 目录是进程全局的环境变量：与全 crate 共用一把测试环境锁，
    // 放锁时自动还原。
    fn env_lock() -> &'static crate::config::TestEnvLock {
        crate::config::test_config_env_lock()
    }

    fn unique_test_dir(name: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::path::PathBuf::from(format!(
            "/tmp/ha-{name}-{}-{nanos}-{}",
            std::process::id(),
            crate::config::test_dirs::unique_id()
        ))
    }

    #[test]
    fn is_server_listening_returns_false_for_nonexistent_path() {
        let dir = unique_test_dir("nonexistent");
        let path = dir.join("s.sock");
        assert!(!is_server_listening_at(&path));
    }

    #[test]
    fn server_daemon_command_clears_socket_overrides_for_explicit_session() {
        let _guard = env_lock().lock().unwrap();
        std::env::set_var(crate::api::SOCKET_PATH_ENV_VAR, "/tmp/inherited.sock");
        std::env::set_var("HERDR_CLIENT_SOCKET_PATH", "/tmp/inherited-client.sock");
        std::env::remove_var(crate::session::SESSION_ENV_VAR);
        crate::session::clear_explicit_session_for_test();
        let args = vec![
            "herdr".to_string(),
            "--session".to_string(),
            "work".to_string(),
        ];
        crate::session::configure_from_args(&args).unwrap();

        let command = build_server_daemon_command(PathBuf::from("/tmp/herdr-test"));
        let envs: Vec<_> = command.get_envs().collect();

        assert!(envs.iter().any(|(key, value)| {
            *key == OsStr::new(crate::api::SOCKET_PATH_ENV_VAR) && value.is_none()
        }));
        assert!(envs.iter().any(|(key, value)| {
            *key == OsStr::new("HERDR_CLIENT_SOCKET_PATH") && value.is_none()
        }));
        crate::session::clear_explicit_session_for_test();
    }

    #[test]
    fn server_daemon_command_passes_current_dir_as_startup_cwd() {
        let expected = std::env::current_dir().unwrap();
        let command = build_server_daemon_command(PathBuf::from("/tmp/herdr-test"));
        let envs: Vec<_> = command.get_envs().collect();

        assert!(envs.iter().any(|(key, value)| {
            *key == OsStr::new(STARTUP_CWD_ENV_VAR) && value == &Some(expected.as_os_str())
        }));
    }

    #[test]
    fn is_server_listening_returns_true_for_live_socket() {
        let dir = unique_test_dir("live");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.sock");

        let _listener = UnixListener::bind(&path).unwrap();
        assert!(is_server_listening_at(&path));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn is_server_listening_returns_false_for_stale_socket() {
        let dir = unique_test_dir("stale");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.sock");

        // Create a socket and immediately drop the listener.
        // This leaves a stale socket file with nobody listening.
        {
            let _listener = UnixListener::bind(&path).unwrap();
        }

        // The socket file exists but nobody is listening.
        assert!(!is_server_listening_at(&path));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn is_server_listening_returns_false_when_listener_dropped() {
        let dir = unique_test_dir("dropped");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.sock");

        // Bind and immediately drop the listener.
        drop(UnixListener::bind(&path).unwrap());

        // Socket is stale — should return false.
        assert!(!is_server_listening_at(&path));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn wait_for_server_socket_succeeds_immediately() {
        let dir = unique_test_dir("wait-ok");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.sock");

        let _listener = UnixListener::bind(&path).unwrap();

        // Should succeed immediately (socket is already ready).
        let result = wait_for_server_socket(&path, Duration::from_millis(100));
        assert!(result.is_ok());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn wait_for_server_socket_times_out() {
        let dir = unique_test_dir("wait-timeout");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.sock");

        // No listener — should time out.
        let result = wait_for_server_socket(&path, Duration::from_millis(50));
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn wait_for_server_socket_succeeds_after_delay() {
        let dir = unique_test_dir("wait-delay");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.sock");

        // Spawn a thread that will create the listener after a short delay.
        let path_clone = path.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            let _listener = UnixListener::bind(&path_clone).unwrap();
            // Keep the listener alive for a bit.
            std::thread::sleep(Duration::from_secs(1));
        });

        // Wait with a generous timeout — should succeed.
        let result = wait_for_server_socket(&path, Duration::from_secs(2));
        assert!(result.is_ok());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn read_server_status_at_reads_ping_response() {
        let dir = unique_test_dir("status");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("api.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut request)
                .unwrap();
            assert!(request.contains("ping"));
            stream
                .write_all(
                    b"{\"id\":\"autodetect:server:status\",\"result\":{\"type\":\"pong\",\"version\":\"0.5.5\",\"protocol\":2}}\n",
                )
                .unwrap();
            stream.flush().unwrap();
        });

        let status = crate::api::read_runtime_status_at(&path, Duration::from_millis(200))
            .unwrap()
            .unwrap();
        let _ = handle.join();
        assert_eq!(status.version.as_deref(), Some("0.5.5"));
        assert_eq!(status.protocol, Some(2));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn validate_running_server_compatibility_fails_when_status_api_missing() {
        let _guard = env_lock().lock().unwrap();
        let dir = unique_test_dir("missing-api");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("api.sock");
        std::env::set_var(crate::api::SOCKET_PATH_ENV_VAR, &path);

        let err = validate_running_server_compatibility(false, RunningServer { status: None })
            .unwrap_err();

        assert!(
            err.to_string().contains("status API is unavailable"),
            "unexpected error: {err}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn validate_running_server_compatibility_names_session_commands_for_protocol_mismatch() {
        let _guard = env_lock().lock().unwrap();
        let dir = unique_test_dir("named-protocol");
        std::env::set_var("XDG_CONFIG_HOME", &dir);
        std::env::set_var(crate::session::SESSION_ENV_VAR, "work");
        std::env::remove_var(crate::api::SOCKET_PATH_ENV_VAR);
        crate::session::clear_explicit_session_for_test();
        let path = crate::session::api_socket_path_for(Some("work"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let listener = UnixListener::bind(&path).unwrap();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut request)
                .unwrap();
            assert!(request.contains("ping"));
            let body = format!(
                "{{\"id\":\"autodetect:server:status\",\"result\":{{\"type\":\"pong\",\"version\":\"0.5.5\",\"protocol\":{}}}}}\n",
                crate::protocol::PROTOCOL_VERSION + 1
            );
            stream.write_all(body.as_bytes()).unwrap();
            stream.flush().unwrap();
        });

        let err = validate_running_server_compatibility(false, RunningServer { status: None })
            .unwrap_err();
        let message = err.to_string();

        let _ = handle.join();
        assert!(
            message.contains("Stop the old server to use the new version"),
            "unexpected error: {message}"
        );
        assert!(
            message.contains("Run `herdr session stop work`"),
            "unexpected error: {message}"
        );
        assert!(
            message.contains("then run `herdr session attach work` again"),
            "unexpected error: {message}"
        );
        crate::session::clear_explicit_session_for_test();
        let _ = std::fs::remove_dir_all(dir);
    }
}
