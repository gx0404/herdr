use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, Once, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use sysinfo::{Process, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

static PID_REGISTRY: OnceLock<Mutex<HashSet<u32>>> = OnceLock::new();
static RUNTIME_DIR_REGISTRY: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
static REAPER_REGISTRY: OnceLock<Mutex<HashMap<PathBuf, Child>>> = OnceLock::new();
static INIT: Once = Once::new();
static CLEANUP_GUARD: OnceLock<CleanupGuard> = OnceLock::new();
const SANDBOX_OWNER_MARKER: &str = ".herdr-test-sandbox-owner";
const REAPER_OWNER_ENV: &str = "HERDR_TEST_REAPER_OWNER";
const REAPER_ROOT_ENV: &str = "HERDR_TEST_REAPER_ROOT";
const REAPER_DRIVER_ROOT_ENV: &str = "HERDR_TEST_REAPER_DRIVER_ROOT";
const SANDBOX_ENV_KEYS: &[&str] = &[
    "HOME",
    "XDG_CONFIG_HOME",
    "XDG_STATE_HOME",
    "XDG_RUNTIME_DIR",
    "XDG_CACHE_HOME",
    "HERDR_CONFIG_PATH",
    "HERDR_SOCKET_PATH",
    "HERDR_CLIENT_SOCKET_PATH",
];
pub const CURRENT_PROTOCOL: u32 = 23;
pub const CURRENT_ENDPOINT_PROTOCOL_GENERATION: u32 = 1;
pub const SERVER_MESSAGE_SERVER_SHUTDOWN: u32 = 3;
pub const SERVER_MESSAGE_ENDPOINT_CONTROL: u32 = 20;
pub const SERVER_MESSAGE_PANE_SURFACE: u32 = 13;
pub const SERVER_MESSAGE_SEMANTIC_NOTIFICATION: u32 = 14;
pub const SERVER_MESSAGE_PANE_SURFACE_PATCH: u32 = 19;
const CLIENT_MESSAGE_CLIENT_SHELL_PANE_INPUT: u32 = 13;
const CLIENT_MESSAGE_CLIENT_SHELL_FOCUS: u32 = 18;
const CLIENT_MESSAGE_ENDPOINT_CONTROL: u32 = 20;

/// 外层环境里能把 herdr 子进程引回开发机真实目录的变量，HOME / XDG_CONFIG_HOME /
/// XDG_STATE_HOME 的隔离管不到它们：`HERDR_CONFIG_PATH` 直接指定配置文件；其余改写 agent
/// 集成目录（`~/.claude`、`~/.codex`、`~/.kimi-code`、`~/.pi/agent`，opencode 的数据目录跟随
/// `XDG_DATA_HOME`），`integration install` 往里写、server 的活动树适配器从里读。拉起 herdr
/// 的助手都清掉它们，让这些位置落回用例自己的配置目录与 HOME。
pub const INHERITED_DIR_OVERRIDES: &[&str] = &[
    "HERDR_CONFIG_PATH",
    "CLAUDE_CONFIG_DIR",
    "CODEX_HOME",
    "KIMI_CODE_HOME",
    "PI_CODING_AGENT_DIR",
    "XDG_DATA_HOME",
];

/// 被测二进制在配置目录下使用的应用目录名（`config::app_dir_name`）：debug 构建是
/// `herdr-dev`，release 构建是 `herdr`。测试与被测二进制按同一 profile 构建，所以跟随本
/// crate 的 `debug_assertions`；写死任一名字，另一种 profile 下配置会被静默忽略、断言会去查
/// 不存在的路径。
pub fn app_dir_name() -> &'static str {
    if cfg!(debug_assertions) {
        "herdr-dev"
    } else {
        "herdr"
    }
}

pub fn register_spawned_herdr_pid(pid: Option<u32>) {
    let Some(pid) = pid else {
        return;
    };

    ensure_cleanup_hooks();
    let mut registry = pid_registry_lock();
    registry.insert(pid);
}

pub fn unregister_spawned_herdr_pid(pid: Option<u32>) {
    let Some(pid) = pid else {
        return;
    };

    if let Some(registry) = PID_REGISTRY.get() {
        let mut guard = registry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.remove(&pid);
    }
}

pub fn register_runtime_dir(path: &Path) {
    ensure_cleanup_hooks();

    let _ = fs::create_dir_all(path);
    let owner = std::process::id().to_string();

    let mut runtime_dirs = runtime_dir_registry_lock();
    runtime_dirs.insert(path.to_path_buf());
    drop(runtime_dirs);

    let Some(root) = path.parent().map(Path::to_path_buf) else {
        return;
    };
    let _ = fs::write(root.join(SANDBOX_OWNER_MARKER), owner);
    register_sandbox_reaper(&root);
}

pub fn unregister_runtime_dir(path: &Path) {
    if let Some(registry) = RUNTIME_DIR_REGISTRY.get() {
        let mut guard = registry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.remove(path);
    }
}

#[cfg(target_os = "linux")]
pub fn herdr_server_pids_for_runtime_dir(runtime_dir: &Path) -> std::io::Result<Vec<u32>> {
    let mut pids = Vec::new();
    for pid in iter_worktree_server_pids()? {
        let Some(process_runtime_dir) = process_runtime_dir(pid)? else {
            continue;
        };
        if process_runtime_dir == runtime_dir {
            pids.push(pid);
        }
    }
    pids.sort_unstable();
    Ok(pids)
}

pub fn cleanup_test_base(base: &Path) {
    let runtime_dir = base.join("runtime");
    let runtime_dirs = HashSet::from([runtime_dir.clone()]);

    terminate_servers_for_runtime_dirs(&runtime_dirs);
    unregister_runtime_dir(&runtime_dir);
    unregister_sandbox_reaper(base);
    let _ = fs::remove_dir_all(base);
}

pub fn wait_for_socket(path: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() && UnixStream::connect(path).is_ok() {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("socket did not appear at {}", path.display());
}

pub fn wait_for_file(path: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("file did not appear at {}", path.display());
}

fn encode_varint_u32(v: u32) -> Vec<u8> {
    if v < 251 {
        vec![v as u8]
    } else if v < 65536 {
        let mut buf = vec![251u8];
        buf.extend_from_slice(&(v as u16).to_le_bytes());
        buf
    } else {
        let mut buf = vec![252u8];
        buf.extend_from_slice(&v.to_le_bytes());
        buf
    }
}

fn encode_varint_u16(v: u16) -> Vec<u8> {
    if v < 251 {
        vec![v as u8]
    } else {
        let mut buf = vec![251u8];
        buf.extend_from_slice(&v.to_le_bytes());
        buf
    }
}

fn frame_message(payload: &[u8]) -> Vec<u8> {
    let len = payload.len() as u32;
    let mut framed = len.to_le_bytes().to_vec();
    framed.extend_from_slice(payload);
    framed
}

fn decode_varint_u32(payload: &[u8], offset: usize) -> Result<(u32, usize), String> {
    if offset >= payload.len() {
        return Err("payload too short for varint".into());
    }
    let first_byte = payload[offset];
    match first_byte {
        0..=250 => Ok((first_byte as u32, 1)),
        251 => {
            if offset + 3 > payload.len() {
                return Err("payload too short for u16 varint".into());
            }
            let v = u16::from_le_bytes(
                payload[offset + 1..offset + 3]
                    .try_into()
                    .map_err(|e: std::array::TryFromSliceError| e.to_string())?,
            );
            Ok((v as u32, 3))
        }
        252 => {
            if offset + 5 > payload.len() {
                return Err("payload too short for u32 varint".into());
            }
            let v = u32::from_le_bytes(
                payload[offset + 1..offset + 5]
                    .try_into()
                    .map_err(|e: std::array::TryFromSliceError| e.to_string())?,
            );
            Ok((v, 5))
        }
        _ => Err(format!("unsupported varint tag: {first_byte}")),
    }
}

fn encode_varint_enum(variant_idx: u32, fields: &[&[u8]]) -> Vec<u8> {
    let mut buf = encode_varint_u32(variant_idx);
    for field in fields {
        buf.extend_from_slice(field);
    }
    buf
}

fn encode_string(value: &str) -> Vec<u8> {
    let mut encoded = encode_varint_u32(value.len() as u32);
    encoded.extend_from_slice(value.as_bytes());
    encoded
}

fn decode_string(payload: &[u8], offset: &mut usize) -> Result<String, String> {
    let (len, consumed) = decode_varint_u32(payload, *offset)?;
    *offset += consumed;
    let len = len as usize;
    if *offset + len > payload.len() {
        return Err("payload too short for string content".into());
    }
    let value = String::from_utf8(payload[*offset..*offset + len].to_vec())
        .map_err(|err| err.to_string())?;
    *offset += len;
    Ok(value)
}

fn decode_welcome(payload: &[u8]) -> Result<(u32, Option<String>), String> {
    let mut offset = 0;
    let (variant, consumed) = decode_varint_u32(payload, offset)?;
    offset += consumed;
    if variant != 0 {
        return Err(format!(
            "expected Welcome (variant 0), got variant {variant}"
        ));
    }

    let (version, consumed) = decode_varint_u32(payload, offset)?;
    offset += consumed;

    let (_encoding, consumed) = decode_varint_u32(payload, offset)?;
    offset += consumed;

    if offset >= payload.len() {
        return Err("payload too short for Option tag".into());
    }
    let option_tag = payload[offset];
    offset += 1;

    let error = if option_tag == 1 {
        let (str_len, consumed) = decode_varint_u32(payload, offset)?;
        offset += consumed;
        let str_len = str_len as usize;
        if offset + str_len > payload.len() {
            return Err("payload too short for string content".into());
        }
        Some(
            String::from_utf8(payload[offset..offset + str_len].to_vec())
                .map_err(|e| e.to_string())?,
        )
    } else {
        None
    };

    Ok((version, error))
}

fn read_handshake_response(
    stream: &mut UnixStream,
    hello_payload: &[u8],
) -> Result<Vec<u8>, String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    stream
        .write_all(&frame_message(hello_payload))
        .map_err(|e| e.to_string())?;
    stream.flush().map_err(|e| e.to_string())?;

    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).map_err(|e| e.to_string())?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > 2 * 1024 * 1024 {
        return Err(format!("oversized response: {len}"));
    }
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload).map_err(|e| e.to_string())?;
    Ok(payload)
}

pub fn client_handshake(
    stream: &mut UnixStream,
    version: u32,
    cols: u16,
    rows: u16,
) -> Result<(u32, Option<String>), String> {
    let hello_payload = encode_varint_enum(
        0,
        &[
            &encode_varint_u32(version),
            &encode_varint_u16(cols),
            &encode_varint_u16(rows),
            &encode_varint_u32(8),  // cell_width_px
            &encode_varint_u32(16), // cell_height_px
            &[0],                   // pixel_mouse = false
        ],
    );
    let response = read_handshake_response(stream, &hello_payload)?;
    decode_welcome(&response)
}

pub fn client_shell_handshake(
    stream: &mut UnixStream,
    endpoint_generation: u32,
    surface_cols: u16,
    surface_rows: u16,
) -> Result<(u32, Option<String>), String> {
    let data = serde_json::json!({
        "generation": endpoint_generation,
        "cell_width_px": 8,
        "cell_height_px": 16,
        "surface_size": {"cols": surface_cols, "rows": surface_rows},
        "pixel_mouse": false,
        "direct_graphics": false,
        "endpoint_keybindings": false,
        "mouse_capture": false,
        "snapshot_codecs": ["shell.snapshot.v1"],
        "surface_codecs": ["shell.surface.v1"],
        "input_codecs": ["shell.input.semantic.v1"],
        "blob_codecs": ["shell.blob.v1"]
    })
    .to_string();
    let hello_payload = encode_varint_enum(
        CLIENT_MESSAGE_ENDPOINT_CONTROL,
        &[&encode_string("endpoint.hello.v1"), &encode_string(&data)],
    );
    let response = read_handshake_response(stream, &hello_payload)?;
    let mut offset = 0;
    let (variant, consumed) = decode_varint_u32(&response, offset)?;
    offset += consumed;
    if variant != SERVER_MESSAGE_ENDPOINT_CONTROL {
        return Err(format!(
            "expected EndpointControl (variant {SERVER_MESSAGE_ENDPOINT_CONTROL}), got variant {variant}"
        ));
    }
    let kind = decode_string(&response, &mut offset)?;
    if kind != "endpoint.welcome.v1" {
        return Err(format!("expected endpoint.welcome.v1, got {kind}"));
    }
    let data = decode_string(&response, &mut offset)?;
    let value: serde_json::Value = serde_json::from_str(&data).map_err(|err| err.to_string())?;
    let generation = value["generation"]
        .as_u64()
        .ok_or_else(|| "endpoint welcome omitted generation".to_owned())?
        as u32;
    let error = value["error"]
        .as_object()
        .and_then(|error| error.get("message"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    Ok((generation, error))
}

pub fn read_server_message(stream: &mut UnixStream) -> Result<(u32, Vec<u8>), String> {
    let mut len_buf = [0u8; 4];
    stream
        .read_exact(&mut len_buf)
        .map_err(|e| format!("read length prefix: {e}"))?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > 2 * 1024 * 1024 {
        return Err(format!("oversized frame: {len} bytes"));
    }
    if len == 0 {
        return Err("zero-length frame".into());
    }

    let mut payload = vec![0u8; len];
    stream
        .read_exact(&mut payload)
        .map_err(|e| format!("read payload: {e}"))?;

    let (variant, consumed) = decode_varint_u32(&payload, 0)?;
    Ok((variant, payload[consumed..].to_vec()))
}

pub fn send_client_shell_shift_enter(stream: &mut UnixStream, pane_id: &str) -> Result<(), String> {
    let mut payload = encode_varint_u32(CLIENT_MESSAGE_CLIENT_SHELL_PANE_INPUT);
    payload.extend_from_slice(&encode_varint_u32(pane_id.len() as u32));
    payload.extend_from_slice(pane_id.as_bytes());
    payload.extend_from_slice(&encode_varint_u32(1)); // one pane input event
    payload.extend_from_slice(&encode_varint_u32(0)); // Key
    payload.extend_from_slice(&encode_varint_u32(1)); // Enter
    payload.push(1); // Shift
    payload.extend_from_slice(&encode_varint_u32(0)); // Press
    payload.extend_from_slice(&encode_varint_u16(1));
    payload.push(0); // no shifted codepoint
    payload.push(0); // no generated text
    payload.push(0); // does not track release
    payload.push(0); // no physical key id
    payload.push(0); // no Windows key record

    stream
        .write_all(&frame_message(&payload))
        .map_err(|e| format!("write client shell key: {e}"))?;
    stream
        .flush()
        .map_err(|e| format!("flush client shell key: {e}"))
}

pub fn send_client_shell_focus(stream: &mut UnixStream, focused: bool) -> Result<(), String> {
    let mut payload = encode_varint_u32(CLIENT_MESSAGE_CLIENT_SHELL_FOCUS);
    payload.push(u8::from(focused));
    stream
        .write_all(&frame_message(&payload))
        .map_err(|e| format!("write client shell focus: {e}"))?;
    stream
        .flush()
        .map_err(|e| format!("flush client shell focus: {e}"))
}

pub fn send_detach(stream: &mut UnixStream) -> Result<(), String> {
    let detach_payload = encode_varint_u32(4);
    let framed = frame_message(&detach_payload);
    stream
        .write_all(&framed)
        .map_err(|e| format!("write detach: {e}"))?;
    stream.flush().map_err(|e| format!("flush detach: {e}"))?;
    Ok(())
}

pub fn drain_messages(stream: &mut UnixStream) {
    stream
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    while read_server_message(stream).is_ok() {}
    stream.set_read_timeout(None).unwrap();
}

pub fn wait_until<F>(timeout: Duration, interval: Duration, mut predicate: F) -> bool
where
    F: FnMut() -> bool,
{
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate() {
            return true;
        }
        thread::sleep(interval);
    }
    predicate()
}

pub fn wait_for_message_variant(
    stream: &mut UnixStream,
    timeout: Duration,
    variant: u32,
) -> Result<bool, String> {
    wait_for_message_variants(stream, timeout, &[variant])
}

pub fn wait_for_message_variants(
    stream: &mut UnixStream,
    timeout: Duration,
    variants: &[u32],
) -> Result<bool, String> {
    let read_timeout = Some(Duration::from_millis(200));
    // Darwin can reject resetting the timeout after peer closure with queued data.
    if stream.read_timeout().map_err(|e| e.to_string())? != read_timeout {
        stream
            .set_read_timeout(read_timeout)
            .map_err(|e| e.to_string())?;
    }
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match read_server_message(stream) {
            Ok((got, _)) if variants.contains(&got) => return Ok(true),
            Ok(_) => continue,
            Err(_) => continue,
        }
    }
    Ok(false)
}

pub fn wait_for_client_shell_bootstrap(
    stream: &mut UnixStream,
    timeout: Duration,
) -> Result<(), String> {
    stream
        .set_read_timeout(Some(Duration::from_millis(200)))
        .map_err(|e| e.to_string())?;
    let deadline = Instant::now() + timeout;
    let mut saw_snapshot = false;
    while Instant::now() < deadline {
        match read_server_message(stream) {
            Ok((SERVER_MESSAGE_ENDPOINT_CONTROL, payload)) => {
                let mut offset = 0;
                if decode_string(&payload, &mut offset).as_deref() == Ok("shell.snapshot.v1") {
                    saw_snapshot = true;
                }
            }
            Ok((SERVER_MESSAGE_PANE_SURFACE, _)) if saw_snapshot => return Ok(()),
            Ok((SERVER_MESSAGE_PANE_SURFACE, _)) => {
                return Err("client shell pane surface arrived before its snapshot".into());
            }
            Ok(_) | Err(_) => {}
        }
    }
    Err(format!(
        "timed out waiting for client shell {}",
        if saw_snapshot {
            "pane surface"
        } else {
            "snapshot"
        }
    ))
}

pub fn wait_for_disconnect(stream: &mut UnixStream, timeout: Duration) -> Result<bool, String> {
    stream.set_nonblocking(true).map_err(|e| e.to_string())?;
    let deadline = Instant::now() + timeout;
    let mut idle_since = None;
    let result = loop {
        match read_server_message(stream) {
            Ok(_) => idle_since = None,
            Err(err)
                if err.to_ascii_lowercase().contains("would block")
                    || err.contains("Resource temporarily unavailable") =>
            {
                let idle_started = *idle_since.get_or_insert_with(Instant::now);
                if idle_started.elapsed() >= Duration::from_millis(200) {
                    break Ok(true);
                }
            }
            Err(_) => break Ok(true),
        }
        if Instant::now() >= deadline {
            break Ok(false);
        }
        thread::sleep(Duration::from_millis(25));
    };
    let _ = stream.set_nonblocking(false);
    result
}

pub fn cleanup_registered_herdr_pids() {
    // PIDs are retained for API compatibility, but never used as ownership
    // evidence: a reused PID must not make the cleanup target a user process.
    {
        let mut registry = pid_registry_lock();
        registry.clear();
    }

    let runtime_dirs: HashSet<PathBuf> = {
        let mut runtime_dirs = runtime_dir_registry_lock();
        runtime_dirs.drain().collect()
    };
    terminate_servers_for_runtime_dirs(&runtime_dirs);

    let reapers: Vec<(PathBuf, Child)> = {
        let mut registry = reaper_registry_lock();
        registry.drain().collect()
    };
    for (root, reaper) in reapers {
        let reaper_pid = reaper.id();
        cleanup_sandbox_root(&root, &[reaper_pid]);
        stop_reaper(reaper);
        let _ = fs::remove_dir_all(root);
    }
}

fn ensure_cleanup_hooks() {
    INIT.call_once(|| {
        sweep_stale_sandboxes();

        let _ = CLEANUP_GUARD.set(CleanupGuard);

        let previous_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |panic_info| {
            cleanup_registered_herdr_pids();
            previous_hook(panic_info);
        }));

        let _ = ctrlc::set_handler(|| {
            cleanup_registered_herdr_pids();
            std::process::exit(130);
        });

        unsafe {
            libc::atexit(run_atexit_cleanup);
        }
    });
}

fn pid_registry_lock() -> std::sync::MutexGuard<'static, HashSet<u32>> {
    PID_REGISTRY
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn runtime_dir_registry_lock() -> std::sync::MutexGuard<'static, HashSet<PathBuf>> {
    RUNTIME_DIR_REGISTRY
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn reaper_registry_lock() -> std::sync::MutexGuard<'static, HashMap<PathBuf, Child>> {
    REAPER_REGISTRY
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn register_sandbox_reaper(root: &Path) {
    if !is_sandbox_root(root) {
        return;
    }

    let mut reapers = reaper_registry_lock();
    if reapers.contains_key(root) {
        return;
    }

    if let Some(child) = spawn_sandbox_reaper(root) {
        reapers.insert(root.to_path_buf(), child);
    }
}

fn stop_reaper(mut reaper: Child) {
    if reaper.try_wait().ok().flatten().is_none() {
        let _ = reaper.kill();
        let _ = reaper.wait();
    }
}

fn unregister_sandbox_reaper(root: &Path) {
    let reaper = REAPER_REGISTRY.get().and_then(|registry| {
        registry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(root)
    });
    if let Some(reaper) = reaper {
        stop_reaper(reaper);
    }
}

fn reaper_pid_for_root(root: &Path) -> Option<u32> {
    REAPER_REGISTRY.get().and_then(|registry| {
        registry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(root)
            .map(Child::id)
    })
}

fn terminate_servers_for_runtime_dirs(runtime_dirs: &HashSet<PathBuf>) {
    for runtime_dir in runtime_dirs {
        let Some(root) = runtime_dir.parent() else {
            continue;
        };
        let spare = reaper_pid_for_root(root).into_iter().collect::<Vec<_>>();
        cleanup_sandbox_root(root, &spare);
    }
}

fn sandbox_parent_is_allowed(parent: Option<&Path>) -> bool {
    let Some(parent) = parent else {
        return false;
    };
    parent == std::env::temp_dir() || (cfg!(unix) && parent == Path::new("/tmp"))
}

fn is_sandbox_root(root: &Path) -> bool {
    root.is_absolute()
        && sandbox_parent_is_allowed(root.parent())
        && fs::symlink_metadata(root).is_ok_and(|metadata| metadata.file_type().is_dir())
        && root.join(SANDBOX_OWNER_MARKER).is_file()
}

fn owner_pid_from_sandbox_marker(root: &Path) -> Option<u32> {
    fs::read_to_string(root.join(SANDBOX_OWNER_MARKER))
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn spawn_sandbox_reaper(root: &Path) -> Option<Child> {
    let current_exe = std::env::current_exe().ok()?;
    let mut command = Command::new(current_exe);
    command
        .args([
            "--exact",
            "support::sandbox_reaper_entrypoint",
            "--nocapture",
        ])
        .env(REAPER_OWNER_ENV, std::process::id().to_string())
        .env(REAPER_ROOT_ENV, root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(
            windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP
                | windows_sys::Win32::System::Threading::CREATE_NO_WINDOW
                | windows_sys::Win32::System::Threading::DETACHED_PROCESS,
        );
    }

    command.spawn().ok()
}

fn wait_for_sandbox_owner_exit(owner: u32) {
    #[cfg(unix)]
    {
        while unsafe { libc::getppid() } == owner as libc::pid_t {
            thread::sleep(Duration::from_millis(100));
        }
    }

    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            OpenProcess, WaitForSingleObject, INFINITE, PROCESS_SYNCHRONIZE,
        };
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, owner) };
        if !handle.is_null() {
            unsafe {
                WaitForSingleObject(handle, INFINITE);
                CloseHandle(handle);
            }
        }
    }
}

fn run_sandbox_reaper(owner: u32, root: &Path) {
    if !is_sandbox_root(root) || owner == std::process::id() {
        return;
    }

    wait_for_sandbox_owner_exit(owner);
    if root.join(SANDBOX_OWNER_MARKER).exists() {
        cleanup_sandbox_root(root, &[]);
        let _ = fs::remove_dir_all(root);
    }
}

fn cleanup_sandbox_root(root: &Path, spare: &[u32]) {
    if !is_sandbox_root(root) {
        return;
    }

    for _ in 0..5 {
        if kill_sandbox_processes(root, spare) == 0 {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn sweep_stale_sandboxes() {
    let mut parents = vec![std::env::temp_dir()];
    if cfg!(unix) {
        let unix_parent = PathBuf::from("/tmp");
        if !parents.contains(&unix_parent) {
            parents.push(unix_parent);
        }
    }

    for parent in parents {
        let Ok(entries) = fs::read_dir(parent) else {
            continue;
        };

        for entry in entries.flatten() {
            let root = entry.path();
            if !is_sandbox_root(&root) || owner_pid_from_sandbox_marker(&root).is_none() {
                continue;
            }
            let owner = owner_pid_from_sandbox_marker(&root).unwrap_or_default();
            if owner == std::process::id() || process_exists(owner as libc::pid_t) {
                continue;
            }

            eprintln!(
                "[herdr-test-reaper] sweeping stale sandbox {}",
                root.display()
            );
            cleanup_sandbox_root(&root, &[]);
            let _ = fs::remove_dir_all(root);
        }
    }
}

fn path_is_under(path: &Path, root: &Path) -> bool {
    path == root || path.strip_prefix(root).is_ok()
}

fn environment_entry_belongs_to_root(entry: &OsStr, root: &Path) -> bool {
    let entry = entry.to_string_lossy();
    let Some(equal) = entry.find('=') else {
        return false;
    };
    let key = &entry[..equal];
    if !SANDBOX_ENV_KEYS.contains(&key) {
        return false;
    }
    path_is_under(Path::new(&entry[equal + 1..]), root)
}

fn executable_belongs_to_root(exe: Option<&Path>, root: &Path) -> bool {
    exe.is_some_and(|exe| path_is_under(exe, root))
}

fn process_belongs_to_root(process: &Process, root: &Path) -> bool {
    process
        .environ()
        .iter()
        .any(|entry| environment_entry_belongs_to_root(entry, root))
        || executable_belongs_to_root(process.exe(), root)
}

fn kill_sandbox_processes(root: &Path, spare: &[u32]) -> usize {
    let self_pid = std::process::id();
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing()
            .with_environ(UpdateKind::Always)
            .with_exe(UpdateKind::Always),
    );

    system
        .processes()
        .iter()
        .filter_map(|(pid, process)| {
            let pid = pid.as_u32();
            if pid == self_pid || spare.contains(&pid) || !process_belongs_to_root(process, root) {
                return None;
            }
            process.kill().then_some(pid)
        })
        .count()
}

#[cfg(target_os = "linux")]
fn iter_worktree_server_pids() -> std::io::Result<Vec<u32>> {
    let own_pid = std::process::id();
    let mut pids = Vec::new();

    let proc_entries = match fs::read_dir("/proc") {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };

    for entry in proc_entries {
        let entry = entry?;
        let file_name = entry.file_name();
        let Some(pid) = file_name.to_str().and_then(|name| name.parse::<u32>().ok()) else {
            continue;
        };

        if pid == own_pid {
            continue;
        }

        if is_test_herdr_server_process(pid) {
            pids.push(pid);
        }
    }

    Ok(pids)
}

#[cfg(target_os = "linux")]
fn is_test_herdr_server_process(pid: u32) -> bool {
    let Some(exe_path) = proc_link_target(pid, "exe") else {
        return false;
    };

    if !is_test_herdr_binary(&exe_path) {
        return false;
    }

    let Ok(cmdline) = read_cmdline(pid) else {
        return false;
    };

    cmdline.iter().any(|arg| arg == "server")
}

#[cfg(target_os = "linux")]
fn proc_link_target(pid: u32, link: &str) -> Option<PathBuf> {
    fs::read_link(format!("/proc/{pid}/{link}")).ok()
}

#[cfg(target_os = "linux")]
fn read_cmdline(pid: u32) -> std::io::Result<Vec<String>> {
    let cmdline = fs::read(format!("/proc/{pid}/cmdline"))?;
    Ok(cmdline
        .split(|byte| *byte == 0)
        .filter(|chunk| !chunk.is_empty())
        .map(|chunk| String::from_utf8_lossy(chunk).to_string())
        .collect())
}

#[cfg(target_os = "linux")]
fn process_runtime_dir(pid: u32) -> std::io::Result<Option<PathBuf>> {
    let environ = fs::read(format!("/proc/{pid}/environ"))?;

    let mut socket_path: Option<PathBuf> = None;

    for entry in environ.split(|byte| *byte == 0) {
        if entry.is_empty() {
            continue;
        }

        let kv = String::from_utf8_lossy(entry);
        if let Some(value) = kv.strip_prefix("XDG_RUNTIME_DIR=") {
            return Ok(Some(PathBuf::from(value)));
        }

        if let Some(value) = kv.strip_prefix("HERDR_SOCKET_PATH=") {
            socket_path = Some(PathBuf::from(value));
        }
    }

    Ok(socket_path.and_then(|path| path.parent().map(Path::to_path_buf)))
}

fn is_test_herdr_binary(path: &Path) -> bool {
    // /proc resolves executable symlinks. Match only this Cargo build, including
    // custom target directories; binary identity alone never grants ownership.
    static TEST_BINARY: OnceLock<Option<PathBuf>> = OnceLock::new();
    TEST_BINARY
        .get_or_init(|| fs::canonicalize(env!("CARGO_BIN_EXE_herdr")).ok())
        .as_deref()
        .is_some_and(|binary| path == binary)
}

extern "C" fn run_atexit_cleanup() {
    cleanup_registered_herdr_pids();
}

struct CleanupGuard;

impl Drop for CleanupGuard {
    fn drop(&mut self) {
        cleanup_registered_herdr_pids();
    }
}

#[cfg(unix)]
fn process_exists(pid: libc::pid_t) -> bool {
    let result = unsafe { libc::kill(pid, 0) };
    if result == 0 {
        true
    } else {
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

#[cfg(windows)]
fn process_exists(pid: libc::pid_t) -> bool {
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[sysinfo::Pid::from_u32(pid as u32)]),
        true,
        ProcessRefreshKind::nothing(),
    );
    system.process(sysinfo::Pid::from_u32(pid as u32)).is_some()
}

#[cfg(all(test, unix))]
#[test]
#[allow(
    clippy::zombie_processes,
    reason = "The owner deliberately exits via SIGKILL with a live child to test external reaper cleanup"
)]
fn sandbox_reaper_sigkill_driver() {
    let Ok(root) = std::env::var(REAPER_DRIVER_ROOT_ENV) else {
        return;
    };
    let root = PathBuf::from(root);
    let runtime = root.join("runtime");
    fs::create_dir_all(&runtime).unwrap();
    register_runtime_dir(&runtime);

    let helper = Command::new("sh")
        .args(["-c", "sleep 60"])
        .env("HOME", root.join("home"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("HERDR_CONFIG_PATH", root.join("config.toml"))
        .env("HERDR_SOCKET_PATH", runtime.join("herdr.sock"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    fs::write(root.join("helper.pid"), helper.id().to_string()).unwrap();
    fs::write(root.join("ready"), "ready").unwrap();

    unsafe {
        libc::kill(libc::getpid(), libc::SIGKILL);
    }
}

#[cfg(test)]
#[test]
fn sandbox_reaper_entrypoint() {
    let (Ok(owner), Some(root)) = (
        std::env::var(REAPER_OWNER_ENV),
        std::env::var_os(REAPER_ROOT_ENV),
    ) else {
        return;
    };
    let Ok(owner) = owner.parse::<u32>() else {
        return;
    };
    run_sandbox_reaper(owner, &PathBuf::from(root));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_root(label: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "herdr-reaper-{label}-{}-{unique}",
            std::process::id()
        ))
    }

    fn marked_root(label: &str) -> PathBuf {
        let root = unique_root(label);
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join(SANDBOX_OWNER_MARKER),
            std::process::id().to_string(),
        )
        .unwrap();
        root
    }

    #[test]
    fn sandbox_root_boundary_rejects_prefix_collision() {
        let root = Path::new("/tmp/herdr-reaper-root-123");
        assert!(path_is_under(root, Path::new("/tmp/herdr-reaper-root-123")));
        assert!(path_is_under(
            Path::new("/tmp/herdr-reaper-root-123/runtime"),
            root
        ));
        assert!(!path_is_under(
            Path::new("/tmp/herdr-reaper-root-1234/runtime"),
            root
        ));
    }

    #[test]
    fn sandbox_ownership_requires_environment_or_executable_evidence() {
        let root = marked_root("ownership");
        let path = format!("PATH={}/bin", root.display());
        assert!(!environment_entry_belongs_to_root(OsStr::new(&path), &root));

        let path = format!("HOME={}/home", root.display());
        assert!(environment_entry_belongs_to_root(OsStr::new(&path), &root));
        assert!(executable_belongs_to_root(
            Some(&root.join("bin/herdr")),
            &root
        ));
        assert!(!executable_belongs_to_root(
            Some(Path::new("/tmp/herdr-reaper-other/herdr")),
            &root
        ));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn sandbox_ownership_rejects_cmdline_only_match() {
        let root = marked_root("cmdline-only");
        let command = format!("COMMAND=tail -f {}/log", root.display());
        assert!(!environment_entry_belongs_to_root(
            OsStr::new(&command),
            &root
        ));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unmarked_sandbox_is_never_swept() {
        let root = unique_root("unmarked");
        fs::create_dir_all(&root).unwrap();
        assert!(!is_sandbox_root(&root));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn live_owner_protects_sandbox_from_stale_sweep() {
        let root = marked_root("live-owner");
        assert_eq!(
            owner_pid_from_sandbox_marker(&root),
            Some(std::process::id())
        );
        assert!(process_exists(std::process::id() as libc::pid_t));
        sweep_stale_sandboxes();
        assert!(root.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn stale_sweep_reclaims_root_after_reaper_loss() {
        let root = unique_root("stale");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join(SANDBOX_OWNER_MARKER),
            2_000_000_000_u32.to_string(),
        )
        .unwrap();
        let mut helper = Command::new("sh")
            .args(["-c", "sleep 60"])
            .env("HOME", root.join("home"))
            .env("XDG_RUNTIME_DIR", root.join("runtime"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        sweep_stale_sandboxes();
        let _ = helper.kill();
        let _ = helper.wait();
        assert!(
            !root.exists(),
            "stale sweep left sandbox {}",
            root.display()
        );
    }

    #[cfg(unix)]
    #[test]
    fn orphan_reaper_reclaims_sigkilled_driver_sandbox() {
        let root = unique_root("sigkilled-driver");
        let mut driver = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "support::sandbox_reaper_sigkill_driver",
                "--nocapture",
            ])
            .env(REAPER_DRIVER_ROOT_ENV, &root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let status = driver.wait().unwrap();
        assert!(!status.success(), "the driver must be SIGKILLed");

        let deadline = Instant::now() + Duration::from_secs(10);
        while root.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(50));
        }
        assert!(
            !root.exists(),
            "orphan reaper left sandbox {}",
            root.display()
        );
    }

    #[test]
    fn test_binary_matcher_accepts_cargo_test_binary() {
        let binary = std::fs::canonicalize(env!("CARGO_BIN_EXE_herdr"))
            .expect("Cargo-built binary must exist");
        assert!(
            is_test_herdr_binary(&binary),
            "Cargo-built binary should be considered test-owned regardless of target directory"
        );
    }

    #[test]
    fn test_binary_matcher_rejects_other_binaries() {
        let nested_build = Path::new(env!("CARGO_MANIFEST_DIR")).join("other/target/debug/herdr");
        let sibling_build = Path::new(env!("CARGO_BIN_EXE_herdr"))
            .parent()
            .unwrap()
            .join("other-build/herdr");
        for binary in [
            Path::new("/home/can/.local/bin/herdr"),
            Path::new("/tmp/other-checkout/target/debug/herdr"),
            nested_build.as_path(),
            sibling_build.as_path(),
        ] {
            assert!(
                !is_test_herdr_binary(binary),
                "other binaries must not be considered test-owned: {}",
                binary.display()
            );
        }
    }
}
