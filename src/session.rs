use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use interprocess::local_socket::traits::Stream as _;

use crate::ipc::LocalStream;

pub const SESSION_ENV_VAR: &str = "HERDR_SESSION";
pub const DEFAULT_SESSION_NAME: &str = "default";

const MAX_SESSION_NAME_LEN: usize = 64;
const STOP_WAIT_TIMEOUT: Duration = Duration::from_secs(15);
const STOP_WAIT_POLL: Duration = Duration::from_millis(25);
const MIN_SOCKET_TIMEOUT: Duration = Duration::from_millis(1);

static EXPLICIT_SESSION_REQUESTED: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SessionInfo {
    pub name: String,
    pub default: bool,
    pub running: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connection_error: Option<String>,
    pub socket_path: String,
    pub session_dir: String,
}

pub fn configure_from_args(args: &[String]) -> Result<Vec<String>, String> {
    let mut cleaned = Vec::with_capacity(args.len());
    if let Some(program) = args.first() {
        cleaned.push(program.clone());
    }

    if args.get(1).map(String::as_str) == Some("session")
        && args.get(2).map(String::as_str) == Some("attach")
    {
        if matches!(
            args.get(3).map(String::as_str),
            Some("help" | "--help" | "-h")
        ) {
            return Ok(args.to_vec());
        }
        let Some(name) = args.get(3) else {
            return Err("usage: herdr session attach <name>".to_string());
        };
        if args.len() != 4 {
            return Err("usage: herdr session attach <name>".to_string());
        }
        apply_explicit_name(name)?;
        return Ok(cleaned);
    }

    let mut requested_session = None;
    let mut index = 1;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            cleaned.extend_from_slice(&args[index..]);
            break;
        }
        if arg == "--session" {
            let Some(value) = args.get(index + 1) else {
                return Err("missing value for --session".to_string());
            };
            requested_session = Some(value.clone());
            index += 2;
            continue;
        }
        if let Some(value) = arg.strip_prefix("--session=") {
            requested_session = Some(value.to_string());
            index += 1;
            continue;
        }

        cleaned.push(arg.clone());
        index += 1;
    }

    if let Some(session) = requested_session {
        apply_explicit_name(&session)?;
    } else if std::env::var_os(crate::api::SOCKET_PATH_ENV_VAR).is_some() {
        EXPLICIT_SESSION_REQUESTED.store(false, Ordering::Relaxed);
    } else if let Ok(session) = std::env::var(SESSION_ENV_VAR) {
        if normalize_name(&session)?.is_none() {
            std::env::remove_var(SESSION_ENV_VAR);
        }
        EXPLICIT_SESSION_REQUESTED.store(false, Ordering::Relaxed);
    } else {
        EXPLICIT_SESSION_REQUESTED.store(false, Ordering::Relaxed);
    }

    Ok(cleaned)
}

pub fn active_name() -> Option<String> {
    std::env::var(SESSION_ENV_VAR)
        .ok()
        .filter(|name| name != DEFAULT_SESSION_NAME)
        .filter(|name| validate_name(name).is_ok())
}

pub fn local_attach_command() -> String {
    match active_name() {
        Some(name) => format!("herdr session attach {name}"),
        None => "herdr".to_string(),
    }
}

pub fn local_stop_command() -> String {
    stop_command_for(active_name().as_deref())
}

pub fn stop_command_for(name: Option<&str>) -> String {
    match name {
        Some(name) => format!("herdr session stop {name}"),
        None => "herdr server stop".to_string(),
    }
}

pub fn restart_after_update_guidance(stop_command: &str, attach_command: Option<&str>) -> String {
    let restart = match attach_command {
        Some(command) => format!("Run `{stop_command}`, then run `{command}` again."),
        None => format!("Run `{stop_command}`, then restart Herdr with the same socket override."),
    };
    format!(
        "Stop the old server to use the new version.\nStopping exits pane processes.\n{restart}"
    )
}

pub fn active_restart_after_update_guidance() -> String {
    if !explicit_session_requested() {
        if let Ok(socket_path) = std::env::var(crate::api::SOCKET_PATH_ENV_VAR) {
            return restart_after_update_guidance(
                &format!(
                    "{}={} herdr server stop",
                    crate::api::SOCKET_PATH_ENV_VAR,
                    socket_path
                ),
                None,
            );
        }
    }

    restart_after_update_guidance(&local_stop_command(), Some(&local_attach_command()))
}

pub fn explicit_session_requested() -> bool {
    EXPLICIT_SESSION_REQUESTED.load(Ordering::Relaxed)
}

#[cfg(test)]
pub(crate) fn clear_explicit_session_for_test() {
    EXPLICIT_SESSION_REQUESTED.store(false, Ordering::Relaxed);
}

pub fn data_dir() -> PathBuf {
    data_dir_for(active_name().as_deref())
}

pub fn data_dir_for(name: Option<&str>) -> PathBuf {
    let config_dir = crate::config::config_dir();
    match name {
        Some(name) => config_dir.join("sessions").join(name),
        None => config_dir,
    }
}

pub fn api_socket_path_for(name: Option<&str>) -> PathBuf {
    data_dir_for(name).join("herdr.sock")
}

pub fn active_api_socket_path() -> PathBuf {
    if explicit_session_requested() {
        return api_socket_path_for(active_name().as_deref());
    }
    if let Ok(path) = std::env::var(crate::api::SOCKET_PATH_ENV_VAR) {
        return PathBuf::from(path);
    }
    api_socket_path_for(active_name().as_deref())
}

pub fn client_socket_path_for(name: Option<&str>) -> PathBuf {
    data_dir_for(name).join("herdr-client.sock")
}

pub fn list_sessions() -> std::io::Result<Vec<SessionInfo>> {
    let mut sessions = vec![session_info(None)];
    let sessions_dir = crate::config::config_dir().join("sessions");
    let entries = match std::fs::read_dir(&sessions_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(sessions),
        Err(err) => return Err(err),
    };

    let mut names = Vec::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if name != DEFAULT_SESSION_NAME && validate_name(&name).is_ok() {
            names.push(name);
        }
    }
    names.sort();
    sessions.extend(names.iter().map(|name| session_info(Some(name))));
    Ok(sessions)
}

pub fn session_info(name: Option<&str>) -> SessionInfo {
    let default = name.is_none();
    let display_name = name.unwrap_or(DEFAULT_SESSION_NAME).to_string();
    let socket_path = api_socket_path_for(name);
    let session_dir = data_dir_for(name);
    let connection = crate::ipc::connect_local_stream(&socket_path);
    let running = connection.is_ok();
    let connection_error = connection
        .err()
        .filter(|error| !crate::cli::server_not_running_error(error))
        .map(|error| error.to_string());
    SessionInfo {
        name: display_name,
        default,
        running,
        connection_error,
        socket_path: socket_path.display().to_string(),
        session_dir: session_dir.display().to_string(),
    }
}

pub fn parse_target_name(name: &str) -> Result<Option<String>, String> {
    normalize_name(name)
}

pub fn stop_session(name: Option<&str>) -> Result<SessionInfo, String> {
    stop_session_with_timeout(name, STOP_WAIT_TIMEOUT)
}

pub(crate) fn stop_active_server() -> Result<(), String> {
    let socket_path = active_api_socket_path();
    let client_socket_path = crate::server::socket_paths::client_socket_path();
    stop_socket_with_timeout(
        socket_path.clone(),
        vec![socket_path, client_socket_path],
        STOP_WAIT_TIMEOUT,
        "server",
    )
}

fn stop_session_with_timeout(name: Option<&str>, timeout: Duration) -> Result<SessionInfo, String> {
    let socket_path = api_socket_path_for(name);
    let client_socket_path = client_socket_path_for(name);
    let label = format!("session {}", name.unwrap_or(DEFAULT_SESSION_NAME));
    stop_socket_with_timeout(
        socket_path.clone(),
        vec![socket_path, client_socket_path],
        timeout,
        &label,
    )?;
    Ok(session_info(name))
}

fn stop_socket_with_timeout(
    socket_path: PathBuf,
    stopped_socket_paths: Vec<PathBuf>,
    timeout: Duration,
    label: &str,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    let request = serde_json::json!({
        "id": "cli:session:stop",
        "method": "server.stop",
        "params": {}
    });
    let stream = crate::ipc::connect_local_stream(&socket_path).map_err(|err| {
        format!(
            "{label} is not running or cannot be reached at {}: {err}",
            socket_path.display()
        )
    })?;
    let stop_response = send_stop_request(stream, &request, deadline)?;
    if let Some(response) = stop_response {
        if let Some(error) = response.get("error") {
            return Err(error.to_string());
        }
    }
    let reachable = wait_until_stopped_until(&stopped_socket_paths, deadline)
        .map_err(|error| format!("{label} cannot determine whether shutdown completed: {error}"))?;
    if !reachable.is_empty() {
        return Err(format!(
            "{label} did not stop within {}ms; sockets are still reachable at {}",
            timeout.as_millis(),
            reachable
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    Ok(())
}

pub fn delete_session(name: &str) -> Result<SessionInfo, String> {
    if name == DEFAULT_SESSION_NAME {
        return Err("deleting the default session is not supported".to_string());
    }
    validate_name(name)?;
    let dir = exact_session_dir_for_delete(name)?.unwrap_or_else(|| data_dir_for(Some(name)));
    ensure_session_stopped(name, &dir)?;

    let info = SessionInfo {
        name: name.to_string(),
        default: false,
        running: false,
        connection_error: None,
        socket_path: dir.join("herdr.sock").display().to_string(),
        session_dir: dir.display().to_string(),
    };
    let Some(confirmed_dir) = exact_session_dir_for_delete(name)? else {
        return Ok(info);
    };
    if confirmed_dir != dir {
        return Err(format!(
            "session {name} changed while checking deletion; retry"
        ));
    }
    ensure_session_stopped(name, &dir)?;

    match std::fs::remove_dir_all(&dir) {
        Ok(()) => Ok(info),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(info),
        Err(err) => Err(err.to_string()),
    }
}

fn ensure_session_stopped(name: &str, dir: &Path) -> Result<(), String> {
    for socket_path in [dir.join("herdr.sock"), dir.join("herdr-client.sock")] {
        match is_running_at(&socket_path) {
            Ok(true) => {
                return Err(format!(
                    "session {name} is running; stop it before deleting"
                ));
            }
            Ok(false) => {}
            Err(error) => {
                return Err(format!(
                    "session {name} cannot be safely deleted; cannot determine whether {} is stopped: {error}",
                    socket_path.display()
                ));
            }
        }
    }
    Ok(())
}

fn session_directory_exists_for_delete(path: &Path) -> Result<bool, String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => Ok(true),
        Ok(_) => Err(format!(
            "refusing to delete session data through non-directory or symbolic link at {}",
            path.display()
        )),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(format!("cannot inspect {}: {err}", path.display())),
    }
}

fn exact_session_dir_for_delete(name: &str) -> Result<Option<PathBuf>, String> {
    let sessions_dir = crate::config::config_dir().join("sessions");
    if !session_directory_exists_for_delete(&sessions_dir)? {
        return Ok(None);
    }
    let entries = match std::fs::read_dir(&sessions_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.to_string()),
    };
    for entry in entries {
        let entry = entry.map_err(|err| err.to_string())?;
        if entry.file_name() == std::ffi::OsStr::new(name) {
            let path = entry.path();
            return Ok(session_directory_exists_for_delete(&path)?.then_some(path));
        }
    }

    // A path lookup alone can resolve a different spelling on case-insensitive
    // filesystems. Never probe its socket or delete it without an exact entry.
    match std::fs::symlink_metadata(sessions_dir.join(name)) {
        Ok(_) => Err(format!(
            "session {name} does not match an exact session name; use the spelling shown by `herdr session list`"
        )),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.to_string()),
    }
}

fn send_stop_request(
    mut stream: LocalStream,
    request: &serde_json::Value,
    deadline: Instant,
) -> Result<Option<serde_json::Value>, String> {
    let Some(write_timeout) = socket_timeout_until(deadline) else {
        return Ok(None);
    };
    if let Err(err) = stream.set_send_timeout(Some(write_timeout)) {
        if !stop_timeout_error_allows_wait(&err) {
            return Err(err.to_string());
        }
    }

    let response = send_stop_request_inner(&mut stream, request, deadline);
    match response {
        Ok(Some(line)) => serde_json::from_str(&line)
            .map(Some)
            .map_err(|err| err.to_string()),
        Ok(None) => Ok(None),
        Err(err) if stop_request_error_allows_wait(&err) => Ok(None),
        Err(err) => Err(err.to_string()),
    }
}

fn send_stop_request_inner(
    stream: &mut LocalStream,
    request: &serde_json::Value,
    deadline: Instant,
) -> std::io::Result<Option<String>> {
    stream.write_all(request.to_string().as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    let Some(read_timeout) = socket_timeout_until(deadline) else {
        return Ok(None);
    };
    if let Err(err) = stream.set_recv_timeout(Some(read_timeout)) {
        if stop_timeout_error_allows_wait(&err) {
            return Ok(None);
        }
        return Err(err);
    }

    let mut line = String::new();
    let bytes_read = BufReader::new(stream).read_line(&mut line)?;
    if bytes_read == 0 {
        return Ok(None);
    }
    Ok(Some(line))
}

fn stop_timeout_error_allows_wait(err: &std::io::Error) -> bool {
    err.kind() == std::io::ErrorKind::InvalidInput
        || (cfg!(windows) && err.kind() == std::io::ErrorKind::Unsupported)
}

fn stop_request_error_allows_wait(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::UnexpectedEof
            | std::io::ErrorKind::NotConnected
            | std::io::ErrorKind::TimedOut
            | std::io::ErrorKind::WouldBlock
    )
}

fn is_running_at(socket_path: &Path) -> std::io::Result<bool> {
    match crate::ipc::connect_local_stream(socket_path) {
        Ok(_) => Ok(true),
        Err(err) if crate::cli::server_not_running_error(&err) => Ok(false),
        Err(err) => Err(err),
    }
}

fn wait_until_stopped_until(
    socket_paths: &[PathBuf],
    deadline: Instant,
) -> Result<Vec<PathBuf>, String> {
    loop {
        let reachable = reachable_socket_paths(socket_paths)?;
        if reachable.is_empty() || Instant::now() >= deadline {
            return Ok(reachable);
        }
        std::thread::sleep(STOP_WAIT_POLL.min(time_until(deadline)));
    }
}

fn reachable_socket_paths(socket_paths: &[PathBuf]) -> Result<Vec<PathBuf>, String> {
    let mut reachable = Vec::new();
    for path in socket_paths {
        if is_running_at(path)
            .map_err(|err| format!("cannot determine server state at {}: {err}", path.display()))?
        {
            reachable.push(path.clone());
        }
    }
    Ok(reachable)
}

fn time_until(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

fn socket_timeout_until(deadline: Instant) -> Option<Duration> {
    socket_timeout_from_remaining(time_until(deadline))
}

fn socket_timeout_from_remaining(remaining: Duration) -> Option<Duration> {
    if remaining.is_zero() {
        return None;
    }
    Some(remaining.max(MIN_SOCKET_TIMEOUT))
}

pub fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("session name cannot be empty".to_string());
    }
    if name.len() > MAX_SESSION_NAME_LEN {
        return Err(format!(
            "session name cannot be longer than {MAX_SESSION_NAME_LEN} bytes"
        ));
    }
    if name == "." || name == ".." {
        return Err("session name cannot be . or ..".to_string());
    }
    if !name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(
            "session name may only contain ASCII letters, numbers, '.', '_' and '-'".to_string(),
        );
    }
    Ok(())
}

fn apply_explicit_name(name: &str) -> Result<(), String> {
    let session = normalize_name(name)?;
    if let Some(session) = session {
        std::env::set_var(SESSION_ENV_VAR, session);
    } else {
        std::env::remove_var(SESSION_ENV_VAR);
    }
    EXPLICIT_SESSION_REQUESTED.store(true, Ordering::Relaxed);
    Ok(())
}

fn normalize_name(name: &str) -> Result<Option<String>, String> {
    if name == DEFAULT_SESSION_NAME {
        return Ok(None);
    }
    validate_name(name)?;
    Ok(Some(name.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use interprocess::local_socket::traits::Listener as _;
    // 会话名、socket 覆盖是进程全局的环境变量：与全 crate 共用一把测试环境锁。
    fn env_lock() -> &'static crate::config::TestEnvLock {
        crate::config::test_config_env_lock()
    }

    #[cfg(windows)]
    #[test]
    fn session_discovery_distinguishes_denied_running_and_stopped_servers() {
        use interprocess::local_socket::{prelude::*, GenericNamespaced, ListenerOptions};
        use interprocess::os::windows::local_socket::ListenerOptionsExt as _;
        use interprocess::os::windows::security_descriptor::SecurityDescriptor;
        use widestring::U16CString;

        let _guard = env_lock().lock().unwrap();
        let config_home =
            std::env::temp_dir().join(format!("herdr-session-denied-{}", std::process::id()));
        let previous_config_home = std::env::var_os("XDG_CONFIG_HOME");
        std::env::set_var("XDG_CONFIG_HOME", &config_home);
        let denied_path = api_socket_path_for(Some("denied"));
        let running_path = api_socket_path_for(Some("running"));
        std::fs::create_dir_all(denied_path.parent().unwrap()).unwrap();
        std::fs::create_dir_all(running_path.parent().unwrap()).unwrap();

        // A SYSTEM-only pipe reproduces access denial without requiring elevation in CI.
        let sddl = U16CString::from_str("D:P(A;;GA;;;SY)").unwrap();
        let denied_listener = ListenerOptions::new()
            .name(
                denied_path
                    .to_string_lossy()
                    .to_ns_name::<GenericNamespaced>()
                    .unwrap(),
            )
            .security_descriptor(SecurityDescriptor::deserialize(&sddl).unwrap())
            .create_sync()
            .unwrap();
        let running_listener = crate::ipc::bind_local_listener(&running_path).unwrap();
        let error = crate::ipc::connect_local_stream(&denied_path).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);

        let sessions = list_sessions().unwrap();
        let denied = sessions
            .iter()
            .find(|session| session.name == "denied")
            .unwrap();
        assert!(!denied.running);
        assert_eq!(denied.connection_error, Some(error.to_string()));
        let running = sessions
            .iter()
            .find(|session| session.name == "running")
            .unwrap();
        assert!(running.running);
        assert!(running.connection_error.is_none());
        let stopped = sessions.iter().find(|session| session.default).unwrap();
        assert!(!stopped.running);
        assert!(stopped.connection_error.is_none());
        for session in [running, stopped] {
            assert!(serde_json::to_value(session)
                .unwrap()
                .get("connection_error")
                .is_none());
        }
        assert!(serde_json::to_value(denied).unwrap()["connection_error"].is_string());

        drop(denied_listener);
        drop(running_listener);
        match previous_config_home {
            Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
        std::fs::remove_dir_all(config_home).unwrap();
    }

    #[cfg(unix)]
    fn unique_test_path(name: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        // 时间戳在并发的测试线程间会撞，再带进程内序号。socket 直接建在这个路径：放短的
        // /tmp，macOS 的 $TMPDIR 本身就近 50 字节，加上名字会超出 sun_path（104 字节）。
        let id = crate::config::test_dirs::unique_id();
        std::path::PathBuf::from("/tmp")
            .join(format!("herdr-{name}-{}-{nanos}-{id}", std::process::id()))
    }

    #[cfg(unix)]
    fn local_stream_pair(name: &str) -> (LocalStream, LocalStream, std::path::PathBuf) {
        let path = unique_test_path(name);
        let listener = crate::ipc::bind_local_listener(&path).unwrap();
        let client = crate::ipc::connect_local_stream(&path).unwrap();
        let server = listener.accept().unwrap();
        (client, server, path)
    }

    #[test]
    fn stop_wait_timeout_allows_slow_graceful_shutdown() {
        assert_eq!(STOP_WAIT_TIMEOUT, Duration::from_secs(15));
    }

    #[test]
    fn stop_request_errors_wait_for_socket_state() {
        for kind in [
            std::io::ErrorKind::BrokenPipe,
            std::io::ErrorKind::ConnectionReset,
            std::io::ErrorKind::UnexpectedEof,
            std::io::ErrorKind::NotConnected,
            std::io::ErrorKind::TimedOut,
            std::io::ErrorKind::WouldBlock,
        ] {
            let err = std::io::Error::from(kind);
            assert!(stop_request_error_allows_wait(&err), "{kind:?}");
        }
    }

    #[test]
    fn stop_timeout_invalid_input_waits_for_socket_state() {
        let err = std::io::Error::from(std::io::ErrorKind::InvalidInput);

        assert!(stop_timeout_error_allows_wait(&err));
    }

    #[test]
    fn stop_timeout_unsupported_waits_for_socket_state_only_on_windows() {
        let err = std::io::Error::from(std::io::ErrorKind::Unsupported);

        assert_eq!(stop_timeout_error_allows_wait(&err), cfg!(windows));
    }

    #[test]
    fn socket_timeouts_are_never_zero_duration() {
        assert_eq!(socket_timeout_from_remaining(Duration::ZERO), None);
        assert_eq!(
            socket_timeout_from_remaining(Duration::from_nanos(1)),
            Some(MIN_SOCKET_TIMEOUT)
        );
        assert_eq!(
            socket_timeout_from_remaining(Duration::from_millis(10)),
            Some(Duration::from_millis(10))
        );
    }

    #[cfg(unix)]
    #[test]
    fn stop_request_empty_response_waits_for_socket_state() {
        let (client, server, _path) = local_stream_pair("stop-empty-response");
        let handle = std::thread::spawn(move || {
            let mut request = String::new();
            let _ = BufReader::new(server).read_line(&mut request);
            request
        });
        let request = serde_json::json!({
            "id": "cli:session:stop",
            "method": "server.stop",
            "params": {}
        });

        assert_eq!(
            send_stop_request(
                client,
                &request,
                Instant::now() + Duration::from_millis(100)
            )
            .unwrap(),
            None
        );
        assert!(handle.join().unwrap().contains("server.stop"));
    }

    #[cfg(unix)]
    #[test]
    fn stop_session_times_out_when_socket_stays_open_without_response() {
        // 只要私有目录：线程本地隔离，不改进程环境变量。
        let _dirs = crate::config::test_dirs::isolate_dirs("stop-open");
        let session_name = "silent";
        let socket_path = api_socket_path_for(Some(session_name));
        std::fs::create_dir_all(socket_path.parent().unwrap()).unwrap();
        let _ = std::fs::remove_file(&socket_path);
        let listener = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let keep_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let keep_running_for_thread = keep_running.clone();
        let handle = std::thread::spawn(move || {
            let mut held_streams = Vec::new();
            while keep_running_for_thread.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        if let Ok(reader_stream) = stream.try_clone() {
                            let mut request = String::new();
                            match BufReader::new(reader_stream).read_line(&mut request) {
                                Ok(0) => continue,
                                Ok(_) if request.contains("server.stop") => {
                                    held_streams.push(stream)
                                }
                                Ok(_) => {}
                                Err(_) => continue,
                            }
                        }
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        let err = stop_session_with_timeout(Some(session_name), Duration::from_millis(75))
            .expect_err("silent session should fail after timeout");

        assert!(err.contains("did not stop"), "{err}");
        keep_running.store(false, Ordering::Relaxed);
        handle.join().unwrap();
    }

    #[test]
    fn configure_from_args_removes_global_session_option() {
        let _guard = env_lock().lock().unwrap();
        std::env::remove_var(SESSION_ENV_VAR);
        clear_explicit_session_for_test();
        let args = vec![
            "herdr".to_string(),
            "--session".to_string(),
            "work".to_string(),
            "workspace".to_string(),
            "list".to_string(),
        ];

        let cleaned = configure_from_args(&args).unwrap();

        assert_eq!(std::env::var(SESSION_ENV_VAR).as_deref(), Ok("work"));
        assert!(explicit_session_requested());
        assert_eq!(cleaned, vec!["herdr", "workspace", "list"]);
        std::env::remove_var(SESSION_ENV_VAR);
        clear_explicit_session_for_test();
    }

    #[test]
    fn configure_from_args_accepts_equals_form() {
        let _guard = env_lock().lock().unwrap();
        std::env::remove_var(SESSION_ENV_VAR);
        clear_explicit_session_for_test();
        let args = vec![
            "herdr".to_string(),
            "server".to_string(),
            "stop".to_string(),
            "--session=api".to_string(),
        ];

        let cleaned = configure_from_args(&args).unwrap();

        assert_eq!(std::env::var(SESSION_ENV_VAR).as_deref(), Ok("api"));
        assert!(explicit_session_requested());
        assert_eq!(cleaned, vec!["herdr", "server", "stop"]);
        std::env::remove_var(SESSION_ENV_VAR);
        clear_explicit_session_for_test();
    }

    #[test]
    fn configure_from_args_preserves_child_session_option_after_separator() {
        let _guard = env_lock().lock().unwrap();
        std::env::remove_var(SESSION_ENV_VAR);
        clear_explicit_session_for_test();
        let args = vec![
            "herdr".to_string(),
            "agent".to_string(),
            "start".to_string(),
            "repro".to_string(),
            "--".to_string(),
            "/bin/echo".to_string(),
            "--session".to_string(),
            "child-session".to_string(),
        ];

        let cleaned = configure_from_args(&args).unwrap();

        assert_eq!(cleaned, args);
        assert!(std::env::var(SESSION_ENV_VAR).is_err());
        assert!(!explicit_session_requested());
    }

    #[test]
    fn configure_from_args_preserves_child_session_equals_option_after_separator() {
        let _guard = env_lock().lock().unwrap();
        std::env::remove_var(SESSION_ENV_VAR);
        clear_explicit_session_for_test();
        let args = vec![
            "herdr".to_string(),
            "agent".to_string(),
            "start".to_string(),
            "repro".to_string(),
            "--".to_string(),
            "/bin/echo".to_string(),
            "--session=child-session".to_string(),
        ];

        let cleaned = configure_from_args(&args).unwrap();

        assert_eq!(cleaned, args);
        assert!(std::env::var(SESSION_ENV_VAR).is_err());
        assert!(!explicit_session_requested());
    }

    #[test]
    fn configure_from_args_rewrites_session_attach_to_default_launch() {
        let _guard = env_lock().lock().unwrap();
        std::env::set_var(SESSION_ENV_VAR, "bad/name");
        std::env::set_var(crate::api::SOCKET_PATH_ENV_VAR, "/tmp/inherited.sock");
        clear_explicit_session_for_test();
        let args = vec![
            "herdr".to_string(),
            "session".to_string(),
            "attach".to_string(),
            "work".to_string(),
        ];

        let cleaned = configure_from_args(&args).unwrap();

        assert_eq!(std::env::var(SESSION_ENV_VAR).as_deref(), Ok("work"));
        assert!(explicit_session_requested());
        assert_eq!(cleaned, vec!["herdr"]);
        std::env::remove_var(SESSION_ENV_VAR);
        std::env::remove_var(crate::api::SOCKET_PATH_ENV_VAR);
        clear_explicit_session_for_test();
    }

    #[test]
    fn configure_from_args_leaves_session_attach_help_for_cli_dispatch() {
        let _guard = env_lock().lock().unwrap();
        std::env::remove_var(SESSION_ENV_VAR);
        clear_explicit_session_for_test();
        let args = vec![
            "herdr".to_string(),
            "session".to_string(),
            "attach".to_string(),
            "-h".to_string(),
        ];

        let cleaned = configure_from_args(&args).unwrap();

        assert_eq!(cleaned, args);
        assert!(!explicit_session_requested());
    }

    #[test]
    fn configure_from_args_maps_default_session_name_to_default_path() {
        let _guard = env_lock().lock().unwrap();
        let dirs = crate::config::test_dirs::isolate_dirs("session-default");
        std::env::set_var(SESSION_ENV_VAR, "work");
        clear_explicit_session_for_test();
        std::env::set_var(crate::api::SOCKET_PATH_ENV_VAR, "/tmp/inherited.sock");
        let args = vec![
            "herdr".to_string(),
            "--session".to_string(),
            DEFAULT_SESSION_NAME.to_string(),
            "workspace".to_string(),
            "list".to_string(),
        ];

        let cleaned = configure_from_args(&args).unwrap();

        assert_eq!(cleaned, vec!["herdr", "workspace", "list"]);
        assert!(std::env::var(SESSION_ENV_VAR).is_err());
        assert!(explicit_session_requested());
        assert_eq!(
            active_api_socket_path(),
            dirs.config_dir().join("herdr.sock")
        );
        std::env::remove_var(SESSION_ENV_VAR);
        clear_explicit_session_for_test();
        std::env::remove_var(crate::api::SOCKET_PATH_ENV_VAR);
    }

    #[test]
    fn env_session_does_not_mark_session_explicit() {
        let _guard = env_lock().lock().unwrap();
        std::env::set_var(SESSION_ENV_VAR, "env-session");
        EXPLICIT_SESSION_REQUESTED.store(true, Ordering::Relaxed);
        let args = vec![
            "herdr".to_string(),
            "workspace".to_string(),
            "list".to_string(),
        ];

        let cleaned = configure_from_args(&args).unwrap();

        assert_eq!(cleaned, vec!["herdr", "workspace", "list"]);
        assert_eq!(std::env::var(SESSION_ENV_VAR).as_deref(), Ok("env-session"));
        assert!(!explicit_session_requested());
        std::env::remove_var(SESSION_ENV_VAR);
    }

    #[test]
    fn env_default_session_name_uses_default_path() {
        let _guard = env_lock().lock().unwrap();
        let dirs = crate::config::test_dirs::isolate_dirs("env-session-default");
        std::env::remove_var(crate::api::SOCKET_PATH_ENV_VAR);
        std::env::set_var(SESSION_ENV_VAR, DEFAULT_SESSION_NAME);
        EXPLICIT_SESSION_REQUESTED.store(true, Ordering::Relaxed);
        let args = vec![
            "herdr".to_string(),
            "workspace".to_string(),
            "list".to_string(),
        ];

        let cleaned = configure_from_args(&args).unwrap();

        assert_eq!(cleaned, vec!["herdr", "workspace", "list"]);
        assert!(std::env::var(SESSION_ENV_VAR).is_err());
        assert!(!explicit_session_requested());
        assert_eq!(
            active_api_socket_path(),
            dirs.config_dir().join("herdr.sock")
        );
        std::env::remove_var(SESSION_ENV_VAR);
        std::env::remove_var(crate::api::SOCKET_PATH_ENV_VAR);
        clear_explicit_session_for_test();
    }

    #[test]
    fn local_attach_command_uses_default_launch_for_default_session() {
        let _guard = env_lock().lock().unwrap();
        std::env::remove_var(SESSION_ENV_VAR);

        assert_eq!(local_attach_command(), "herdr");
    }

    #[test]
    fn local_attach_command_uses_session_attach_for_named_session() {
        let _guard = env_lock().lock().unwrap();
        std::env::set_var(SESSION_ENV_VAR, "work");

        assert_eq!(local_attach_command(), "herdr session attach work");

        std::env::remove_var(SESSION_ENV_VAR);
    }

    #[test]
    fn local_stop_command_uses_server_stop_for_default_session() {
        let _guard = env_lock().lock().unwrap();
        std::env::remove_var(SESSION_ENV_VAR);

        assert_eq!(local_stop_command(), "herdr server stop");

        std::env::remove_var(SESSION_ENV_VAR);
    }

    #[test]
    fn local_stop_command_uses_session_stop_for_named_session() {
        let _guard = env_lock().lock().unwrap();
        std::env::set_var(SESSION_ENV_VAR, "work");

        assert_eq!(local_stop_command(), "herdr session stop work");

        std::env::remove_var(SESSION_ENV_VAR);
    }

    #[test]
    fn restart_after_update_guidance_names_stop_and_attach_commands() {
        assert_eq!(
            restart_after_update_guidance(
                "herdr session stop work",
                Some("herdr session attach work")
            ),
            "Stop the old server to use the new version.\nStopping exits pane processes.\nRun `herdr session stop work`, then run `herdr session attach work` again."
        );
    }

    #[test]
    fn active_restart_after_update_guidance_respects_socket_override() {
        let _guard = env_lock().lock().unwrap();
        std::env::set_var(crate::api::SOCKET_PATH_ENV_VAR, "/tmp/custom-herdr.sock");
        std::env::remove_var(SESSION_ENV_VAR);
        clear_explicit_session_for_test();

        assert_eq!(
            active_restart_after_update_guidance(),
            "Stop the old server to use the new version.\nStopping exits pane processes.\nRun `HERDR_SOCKET_PATH=/tmp/custom-herdr.sock herdr server stop`, then restart Herdr with the same socket override."
        );

        std::env::remove_var(crate::api::SOCKET_PATH_ENV_VAR);
    }

    #[test]
    fn explicit_session_socket_ignores_inherited_socket_override() {
        let _guard = env_lock().lock().unwrap();
        let dirs = crate::config::test_dirs::isolate_dirs("session-precedence");
        std::env::set_var(SESSION_ENV_VAR, "work");
        EXPLICIT_SESSION_REQUESTED.store(true, Ordering::Relaxed);
        std::env::set_var(crate::api::SOCKET_PATH_ENV_VAR, "/tmp/inherited.sock");

        let path = active_api_socket_path();

        assert_eq!(
            path,
            dirs.config_dir()
                .join("sessions")
                .join("work")
                .join("herdr.sock")
        );
        std::env::remove_var(SESSION_ENV_VAR);
        clear_explicit_session_for_test();
        std::env::remove_var(crate::api::SOCKET_PATH_ENV_VAR);
    }

    #[test]
    fn env_socket_override_wins_without_explicit_session() {
        let _guard = env_lock().lock().unwrap();
        std::env::set_var(SESSION_ENV_VAR, "work");
        clear_explicit_session_for_test();
        std::env::set_var(crate::api::SOCKET_PATH_ENV_VAR, "/tmp/explicit.sock");

        assert_eq!(
            active_api_socket_path(),
            PathBuf::from("/tmp/explicit.sock")
        );

        std::env::remove_var(SESSION_ENV_VAR);
        clear_explicit_session_for_test();
        std::env::remove_var(crate::api::SOCKET_PATH_ENV_VAR);
    }

    #[test]
    fn env_socket_override_skips_invalid_env_session_validation_without_explicit_session() {
        let _guard = env_lock().lock().unwrap();
        std::env::set_var(SESSION_ENV_VAR, "bad/name");
        clear_explicit_session_for_test();
        std::env::set_var(crate::api::SOCKET_PATH_ENV_VAR, "/tmp/herdr.sock");
        let args = vec![
            "herdr".to_string(),
            "workspace".to_string(),
            "list".to_string(),
        ];

        let cleaned = configure_from_args(&args).unwrap();

        assert_eq!(cleaned, vec!["herdr", "workspace", "list"]);
        assert!(!explicit_session_requested());
        assert_eq!(active_api_socket_path(), PathBuf::from("/tmp/herdr.sock"));
        assert_eq!(std::env::var(SESSION_ENV_VAR).as_deref(), Ok("bad/name"));

        std::env::remove_var(SESSION_ENV_VAR);
        clear_explicit_session_for_test();
        std::env::remove_var(crate::api::SOCKET_PATH_ENV_VAR);
    }

    #[cfg(unix)]
    #[test]
    fn stop_session_fails_when_socket_remains_reachable_after_timeout() {
        // 只要私有目录：线程本地隔离，不改进程环境变量。
        let _dirs = crate::config::test_dirs::isolate_dirs("stop-slow");
        let session_name = "slow";
        let socket_path = api_socket_path_for(Some(session_name));
        std::fs::create_dir_all(socket_path.parent().unwrap()).unwrap();
        let _ = std::fs::remove_file(&socket_path);
        let listener = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let keep_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let keep_running_for_thread = keep_running.clone();
        let handle = std::thread::spawn(move || {
            while keep_running_for_thread.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        if let Ok(reader_stream) = stream.try_clone() {
                            let mut request = String::new();
                            match BufReader::new(reader_stream).read_line(&mut request) {
                                Ok(0) => continue,
                                Ok(_) if request.trim().is_empty() => continue,
                                Ok(_) => {}
                                Err(_) => continue,
                            }
                        }
                        let _ = stream.write_all(b"{\"id\":\"cli:session:stop\",\"result\":{}}\n");
                        let _ = stream.flush();
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        let err = stop_session_with_timeout(Some(session_name), Duration::from_millis(75))
            .expect_err("still-running session should fail");

        assert!(err.contains("did not stop"), "{err}");
        assert!(
            err.contains(socket_path.to_string_lossy().as_ref()),
            "{err}"
        );
        keep_running.store(false, Ordering::Relaxed);
        handle.join().unwrap();
    }

    #[test]
    fn invalid_names_are_rejected() {
        let _guard = env_lock().lock().unwrap();
        assert!(validate_name("../prod").is_err());
        assert!(validate_name("").is_err());
        assert!(validate_name("work session").is_err());
    }

    #[test]
    fn parse_default_target_name_maps_to_default_session() {
        assert_eq!(parse_target_name(DEFAULT_SESSION_NAME).unwrap(), None);
        assert_eq!(parse_target_name("work").unwrap(), Some("work".to_string()));
    }

    #[test]
    fn delete_default_session_is_rejected() {
        assert!(delete_session(DEFAULT_SESSION_NAME).is_err());
    }

    #[test]
    fn delete_session_preserves_running_data_and_deletes_after_listener_stops() {
        let _dirs = crate::config::test_dirs::isolate_dirs("delete-running");
        let session_name = "running";
        let session_dir = data_dir_for(Some(session_name));
        let socket_path = api_socket_path_for(Some(session_name));
        let sentinel = session_dir.join("scrollback").join("pane-1.txt");
        std::fs::create_dir_all(sentinel.parent().unwrap()).unwrap();
        std::fs::write(&sentinel, b"saved pane history\n").unwrap();
        let listener = crate::ipc::bind_local_listener(&socket_path).unwrap();

        let result = delete_session(session_name);

        assert!(
            session_dir.is_dir(),
            "running session data was removed: {result:?}"
        );
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"saved pane history\n");
        assert_eq!(
            result.unwrap_err(),
            format!("session {session_name} is running; stop it before deleting")
        );

        drop(listener);
        for path in [socket_path, client_socket_path_for(Some(session_name))] {
            let error = crate::ipc::connect_local_stream(&path).unwrap_err();
            assert!(
                matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                ),
                "the stopped endpoint must be absent or refuse connections: {error}"
            );
        }

        let info = delete_session(session_name).unwrap();

        assert_eq!(info.name, session_name);
        assert!(!info.running);
        assert!(info.connection_error.is_none());
        assert!(
            !session_dir.exists(),
            "stopped session data must be deleted"
        );
    }

    #[cfg(windows)]
    #[test]
    fn delete_session_preserves_data_when_endpoint_access_is_denied() {
        use interprocess::local_socket::{prelude::*, GenericNamespaced, ListenerOptions};
        use interprocess::os::windows::local_socket::ListenerOptionsExt as _;
        use interprocess::os::windows::security_descriptor::SecurityDescriptor;
        use widestring::U16CString;

        for (endpoint, marker_present) in [
            ("herdr.sock", true),
            ("herdr-client.sock", true),
            ("herdr.sock", false),
            ("herdr-client.sock", false),
        ] {
            let _dirs = crate::config::test_dirs::isolate_dirs("delete-denied");
            let session_name = "denied";
            let session_dir = data_dir_for(Some(session_name));
            let socket_path = session_dir.join(endpoint);
            let sentinel = session_dir.join("scrollback").join("pane-1.txt");
            std::fs::create_dir_all(sentinel.parent().unwrap()).unwrap();
            std::fs::write(&sentinel, b"saved pane history\n").unwrap();
            let sddl = U16CString::from_str("D:P(D;;GA;;;WD)").unwrap();
            let _listener = ListenerOptions::new()
                .name(
                    socket_path
                        .to_string_lossy()
                        .to_ns_name::<GenericNamespaced>()
                        .unwrap(),
                )
                .security_descriptor(SecurityDescriptor::deserialize(&sddl).unwrap())
                .create_sync()
                .unwrap();
            if marker_present {
                std::fs::write(&socket_path, b"test named-pipe marker").unwrap();
            }
            let error = crate::ipc::connect_local_stream(&socket_path).unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
            assert_eq!(
                is_running_at(&socket_path).unwrap_err().kind(),
                std::io::ErrorKind::PermissionDenied
            );
            let socket_paths = [
                api_socket_path_for(Some(session_name)),
                client_socket_path_for(Some(session_name)),
            ];
            for deadline in [Instant::now(), Instant::now() + Duration::from_secs(5)] {
                let wait_error = wait_until_stopped_until(&socket_paths, deadline)
                    .expect_err("access denial must not report a stopped server");
                assert!(wait_error.contains(socket_path.to_string_lossy().as_ref()));
                assert!(wait_error.contains(&error.to_string()));
            }

            let result = delete_session(session_name);

            assert!(
                session_dir.is_dir(),
                "access denial on {endpoint} must preserve session data: {result:?}"
            );
            assert_eq!(std::fs::read(&sentinel).unwrap(), b"saved pane history\n");
            let delete_error = result.expect_err("access denial must not report deletion success");
            assert!(delete_error.contains(socket_path.to_string_lossy().as_ref()));
            assert!(delete_error.contains(&error.to_string()));
        }
    }

    #[test]
    fn delete_session_preserves_data_while_only_client_endpoint_is_running() {
        let _dirs = crate::config::test_dirs::isolate_dirs("delete-client");
        let session_name = "client-only";
        let session_dir = data_dir_for(Some(session_name));
        let sentinel = session_dir.join("scrollback").join("pane-1.txt");
        std::fs::create_dir_all(sentinel.parent().unwrap()).unwrap();
        std::fs::write(&sentinel, b"saved pane history\n").unwrap();
        let _listener =
            crate::ipc::bind_local_listener(&client_socket_path_for(Some(session_name))).unwrap();
        let error =
            crate::ipc::connect_local_stream(&api_socket_path_for(Some(session_name))).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);

        let result = delete_session(session_name);

        assert!(
            session_dir.is_dir(),
            "a live client endpoint must preserve session data: {result:?}"
        );
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"saved pane history\n");
        assert_eq!(
            result.unwrap_err(),
            format!("session {session_name} is running; stop it before deleting")
        );
    }

    #[cfg(windows)]
    #[test]
    fn delete_and_stop_wait_detect_running_pipes_without_markers() {
        for (endpoint, check_wait) in [
            ("herdr.sock", false),
            ("herdr-client.sock", false),
            ("herdr.sock", true),
            ("herdr-client.sock", true),
        ] {
            let _dirs = crate::config::test_dirs::isolate_dirs("no-marker");
            let session_name = "running";
            let session_dir = data_dir_for(Some(session_name));
            let socket_path = session_dir.join(endpoint);
            let sentinel = session_dir.join("keep");
            std::fs::create_dir_all(&session_dir).unwrap();
            std::fs::write(&sentinel, b"do not remove").unwrap();
            let _listener = crate::ipc::bind_local_listener(&socket_path).unwrap();
            std::fs::remove_file(&socket_path).unwrap();

            if check_wait {
                let socket_paths = [
                    api_socket_path_for(Some(session_name)),
                    client_socket_path_for(Some(session_name)),
                ];
                assert_eq!(
                    wait_until_stopped_until(&socket_paths, Instant::now()).unwrap(),
                    vec![socket_path]
                );
            } else {
                assert_eq!(
                    delete_session(session_name).unwrap_err(),
                    format!("session {session_name} is running; stop it before deleting")
                );
            }
            assert_eq!(std::fs::read(&sentinel).unwrap(), b"do not remove");
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_long_sessions_without_markers_preserve_live_data() {
        for (endpoint, check_wait) in [
            ("herdr.sock", false),
            ("herdr-client.sock", false),
            ("herdr.sock", true),
            ("herdr-client.sock", true),
        ] {
            let dirs = crate::config::test_dirs::isolate_dirs("long-no-marker");
            let deep = dirs
                .config_dir()
                .join("a".repeat(80))
                .join("管".repeat(80))
                .join("🦀".repeat(40))
                .join("z".repeat(80));
            crate::config::test_dirs::set_config_dir(deep);
            let session_name = "running";
            let session_dir = data_dir_for(Some(session_name));
            let socket_path = session_dir.join(endpoint);
            let units = format!(r"\\.\pipe\{}", socket_path.to_string_lossy())
                .encode_utf16()
                .count();
            assert!(units > 420);
            let sentinel = session_dir.join("keep");
            std::fs::create_dir_all(&session_dir).unwrap();
            std::fs::write(&sentinel, b"do not remove").unwrap();
            let listener = crate::ipc::bind_local_listener(&socket_path).unwrap();
            std::fs::remove_file(&socket_path).unwrap();
            assert!(
                !crate::ipc::local_listener_accepting(&socket_path).unwrap(),
                "readiness keeps its marker gate"
            );
            if check_wait {
                let paths = [
                    api_socket_path_for(Some(session_name)),
                    client_socket_path_for(Some(session_name)),
                ];
                assert_eq!(
                    wait_until_stopped_until(&paths, Instant::now()).unwrap(),
                    vec![socket_path.clone()]
                );
            } else {
                assert_eq!(
                    delete_session(session_name).unwrap_err(),
                    format!("session {session_name} is running; stop it before deleting")
                );
            }
            assert_eq!(std::fs::read(&sentinel).unwrap(), b"do not remove");
            drop(listener);
            let removed = delete_session(session_name).unwrap();
            assert!(!removed.running);
            assert!(!session_dir.exists());
            println!("long-session endpoint={endpoint} raw_utf16={units} marker=false stop_wait={check_wait} live_preserved=PASS stopped_deleted=PASS");
        }
    }

    #[test]
    fn delete_session_refuses_linked_session_directories() {
        #[cfg(windows)]
        let _guard = env_lock().lock().unwrap();

        for link_sessions_root in [false, true] {
            let dirs = crate::config::test_dirs::isolate_dirs("delete-link");
            let sessions_dir = dirs.config_dir().join("sessions");
            let outside = dirs.state_dir().join("outside");
            let sentinel = outside.join("linked").join("keep");
            std::fs::create_dir_all(sentinel.parent().unwrap()).unwrap();
            std::fs::write(&sentinel, b"do not remove").unwrap();
            let (target, link) = if link_sessions_root {
                (outside, sessions_dir)
            } else {
                (outside.join("linked"), sessions_dir.join("linked"))
            };
            std::fs::create_dir_all(link.parent().unwrap()).unwrap();
            #[cfg(unix)]
            std::os::unix::fs::symlink(&target, &link).unwrap();
            #[cfg(windows)]
            {
                let output = std::process::Command::new(
                    std::env::var_os("COMSPEC").unwrap_or_else(|| "cmd.exe".into()),
                )
                .args(["/D", "/Q", "/C", "mklink", "/J"])
                .arg(&link)
                .arg(&target)
                .stdin(std::process::Stdio::null())
                .output()
                .unwrap();
                assert!(output.status.success(), "create junction: {output:?}");
            }

            let result = delete_session("linked");

            assert_eq!(std::fs::read(&sentinel).unwrap(), b"do not remove");
            assert!(std::fs::symlink_metadata(&link).is_ok());
            let error = result.unwrap_err();
            assert!(error.contains("symbolic link"), "{error}");
            assert!(error.contains(link.to_string_lossy().as_ref()), "{error}");
        }
    }

    #[test]
    fn stop_wait_distinguishes_absent_endpoints_from_invalid_probe_paths() {
        let dirs = crate::config::test_dirs::isolate_dirs("wait-probe");
        let absent = dirs.config_dir().join("absent.sock");
        assert!(!is_running_at(&absent).unwrap());
        assert!(wait_until_stopped_until(&[absent], Instant::now())
            .unwrap()
            .is_empty());

        let invalid = dirs.config_dir().join("invalid\0endpoint");
        let probe_error = is_running_at(&invalid).unwrap_err();
        assert_eq!(probe_error.kind(), std::io::ErrorKind::InvalidInput);
        for deadline in [Instant::now(), Instant::now() + Duration::from_secs(5)] {
            let error = wait_until_stopped_until(std::slice::from_ref(&invalid), deadline)
                .expect_err("invalid probe paths must not report a stopped server");
            assert!(
                error.contains(invalid.to_string_lossy().as_ref()),
                "{error}"
            );
            assert!(error.contains(&probe_error.to_string()), "{error}");
        }
    }

    #[test]
    fn list_sessions_skips_reserved_default_directory() {
        // 列会话只看配置目录，不读会话相关的环境变量：线程本地隔离即可，不必持锁。
        let dirs = crate::config::test_dirs::isolate_dirs("session-list");
        let sessions_dir = dirs.config_dir().join("sessions");
        std::fs::create_dir_all(sessions_dir.join(DEFAULT_SESSION_NAME)).unwrap();
        std::fs::create_dir_all(sessions_dir.join("work")).unwrap();

        let sessions = list_sessions().unwrap();
        let names: Vec<_> = sessions
            .iter()
            .map(|session| session.name.as_str())
            .collect();

        assert_eq!(names, vec![DEFAULT_SESSION_NAME, "work"]);
    }
}
