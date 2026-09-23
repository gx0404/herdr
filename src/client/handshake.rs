use std::io;
#[cfg(unix)]
use std::io::IsTerminal as _;
use std::time::{Duration, Instant};

use interprocess::local_socket::traits::Stream as _;
use tracing::info;

use crate::ipc::LocalStream;
use crate::protocol::endpoint::{
    EndpointClientHello, EndpointServerWelcome, BLOB_CODEC_V1, ENDPOINT_HELLO_KIND,
    ENDPOINT_PROTOCOL_GENERATION, ENDPOINT_WELCOME_KIND, INPUT_CODEC_V1, SNAPSHOT_CODEC_V1,
    SURFACE_CODEC_V1,
};
use crate::protocol::{
    self, ClientMessage, RenderEncoding, ServerMessage, MAX_FRAME_SIZE, PROTOCOL_VERSION,
};

#[cfg(unix)]
use super::terminal_setup::is_ssh_session;
use super::{shell, ClientError};

/// Time to wait for the server's Welcome reply during the handshake.
///
/// A local client talks to an already-connected server, so 5s is plenty. The
/// remote bridge client (`herdr --remote`) sits behind a fresh per-attach ssh
/// connection whose cold-connect (TCP + key exchange + auth) happens inside this
/// window; on a high-latency link that easily exceeds 5s, so it gets a far
/// larger budget. See issue #753.
pub(super) const LOCAL_HANDSHAKE_READ_TIMEOUT: Duration = Duration::from_secs(5);
pub(super) const REMOTE_HANDSHAKE_READ_TIMEOUT: Duration = Duration::from_secs(60);

pub(super) fn is_remote_client_process() -> bool {
    std::env::var(crate::remote::REMOTE_KEYBINDINGS_ENV_VAR).is_ok()
}

pub(super) fn client_shell_keybinding_source() -> shell::ClientShellKeybindingSource {
    match std::env::var(crate::remote::REMOTE_KEYBINDINGS_ENV_VAR)
        .ok()
        .as_deref()
    {
        Some("server") => shell::ClientShellKeybindingSource::Endpoint,
        Some(_) => shell::ClientShellKeybindingSource::RemoteLocal,
        None => shell::ClientShellKeybindingSource::Local,
    }
}

pub(super) fn handshake_read_timeout() -> Duration {
    if is_remote_client_process() {
        return REMOTE_HANDSHAKE_READ_TIMEOUT;
    }
    LOCAL_HANDSHAKE_READ_TIMEOUT
}

#[cfg(any(unix, test))]
pub(super) fn direct_graphics_profile_values(
    term_program: &str,
    term: &str,
    kitty_window: bool,
    blocked_transport: bool,
    terminals: bool,
) -> bool {
    let supported = term_program.eq_ignore_ascii_case("ghostty")
        || term_program.eq_ignore_ascii_case("wezterm")
        || matches!(term, "xterm-ghostty" | "xterm-kitty" | "xterm-wezterm")
        || kitty_window;
    supported && !blocked_transport && terminals
}

#[cfg(unix)]
fn direct_graphics_profile_allowed() -> bool {
    let term_program = std::env::var("TERM_PROGRAM").unwrap_or_default();
    let term = std::env::var("TERM").unwrap_or_default();
    direct_graphics_profile_values(
        &term_program,
        &term,
        std::env::var_os("KITTY_WINDOW_ID").is_some(),
        is_remote_client_process()
            || is_ssh_session()
            || std::env::var_os("TMUX").is_some()
            || std::env::var_os("STY").is_some(),
        io::stdin().is_terminal() && io::stdout().is_terminal(),
    )
}

#[cfg(not(unix))]
fn direct_graphics_profile_allowed() -> bool {
    false
}

/// 把宿主环境的 SSH agent socket 带给 server（WEZ-INT-01 自愈链）：server 是 detached
/// 进程，它继承的 `SSH_AUTH_SOCK` 可能已随宿主终端重启失效；server 只在上报值通过属主
/// 与 socket 类型校验后才用它做 pane 环境兜底。
fn client_ssh_auth_sock_env() -> Option<String> {
    std::env::var("SSH_AUTH_SOCK")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

#[derive(Debug)]
pub(super) struct HandshakeResult {
    pub(super) encoding: RenderEncoding,
    pub(super) endpoint_methods: Option<Vec<String>>,
    pub(super) endpoint_capabilities: Option<Vec<String>>,
    /// Server-reported Herdr version on the stable endpoint path, when offered.
    pub(super) server_version: Option<String>,
}

pub(crate) fn probe_endpoint_negotiation(
    stream: &mut LocalStream,
) -> io::Result<super::endpoint::EndpointNegotiation> {
    let handshake = do_handshake(
        stream,
        80,
        24,
        0,
        0,
        false,
        Some(crate::protocol::ClientSurfaceSize { cols: 80, rows: 24 }),
        false,
        false,
        false,
        None,
    )
    .map_err(io::Error::other)?;
    Ok(super::endpoint::EndpointNegotiation::new(
        handshake.endpoint_methods.unwrap_or_default(),
        handshake.endpoint_capabilities.unwrap_or_default(),
    ))
}

/// Performs the client→server handshake.
///
/// Direct terminal clients retain the same-install private protocol. Client-owned
/// shells use the stable endpoint generation and negotiate whole codecs without
/// comparing Herdr build versions.
pub(super) fn do_handshake(
    stream: &mut LocalStream,
    cols: u16,
    rows: u16,
    cell_width_px: u32,
    cell_height_px: u32,
    exact_cell_size: bool,
    shell_surface_size: Option<crate::protocol::ClientSurfaceSize>,
    endpoint_keybindings: bool,
    mouse_capture: bool,
    surface_active: bool,
    cancel: Option<&crate::remote::TaskCancellation>,
) -> Result<HandshakeResult, ClientError> {
    stream
        .set_nonblocking(false)
        .map_err(ClientError::ConnectionFailed)?;

    let endpoint_shell = shell_surface_size.is_some();
    let hello = if let Some(surface_size) = shell_surface_size {
        let hello = EndpointClientHello {
            generation: ENDPOINT_PROTOCOL_GENERATION,
            cell_width_px,
            cell_height_px,
            surface_size,
            pixel_mouse: exact_cell_size && cfg!(unix),
            direct_graphics: exact_cell_size
                && cell_width_px > 0
                && cell_height_px > 0
                && direct_graphics_profile_allowed(),
            endpoint_keybindings,
            mouse_capture,
            surface_active,
            surface_reuse: true,
            snapshot_codecs: vec![SNAPSHOT_CODEC_V1.into()],
            surface_codecs: vec![SURFACE_CODEC_V1.into()],
            input_codecs: vec![INPUT_CODEC_V1.into()],
            blob_codecs: vec![BLOB_CODEC_V1.into()],
            ssh_auth_sock: client_ssh_auth_sock_env(),
        };
        ClientMessage::EndpointControl {
            kind: ENDPOINT_HELLO_KIND.into(),
            data: serde_json::to_string(&hello).map_err(|error| {
                ClientError::ConnectionFailed(io::Error::new(io::ErrorKind::InvalidData, error))
            })?,
        }
    } else {
        ClientMessage::TerminalHello {
            version: PROTOCOL_VERSION,
            cols,
            rows,
            cell_width_px,
            cell_height_px,
            pixel_mouse: exact_cell_size && cfg!(unix),
        }
    };
    protocol::write_message(stream, &hello)
        .map_err(|e| ClientError::ConnectionFailed(io::Error::other(e.to_string())))?;

    let read_timeout = if endpoint_shell && !surface_active {
        REMOTE_HANDSHAKE_READ_TIMEOUT
    } else {
        handshake_read_timeout()
    };
    let welcome = read_handshake_welcome(stream, read_timeout, cancel)?;

    if endpoint_shell {
        let ServerMessage::EndpointControl { kind, data } = welcome else {
            return Err(ClientError::Protocol(protocol::FramingError::Io(
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "server does not support the stable Herdr endpoint protocol; update this machine",
                ),
            )));
        };
        if kind != ENDPOINT_WELCOME_KIND {
            return Err(ClientError::Protocol(protocol::FramingError::Io(
                io::Error::new(io::ErrorKind::InvalidData, "expected endpoint welcome"),
            )));
        }
        let welcome: EndpointServerWelcome = serde_json::from_str(&data).map_err(|error| {
            ClientError::Protocol(protocol::FramingError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid endpoint welcome: {error}"),
            )))
        })?;
        if let Some(error) = welcome.error {
            return Err(ClientError::HandshakeRejected {
                version: welcome.generation,
                error: error.message,
            });
        }
        if welcome.generation != ENDPOINT_PROTOCOL_GENERATION
            || welcome.snapshot_codec != SNAPSHOT_CODEC_V1
            || welcome.surface_codec != SURFACE_CODEC_V1
            || welcome.input_codec != INPUT_CODEC_V1
            || welcome.blob_codec != BLOB_CODEC_V1
        {
            return Err(ClientError::HandshakeRejected {
                version: welcome.generation,
                error: "server has no compatible endpoint core; update this machine".into(),
            });
        }
        info!(
            generation = welcome.generation,
            server_version = %welcome.server_version,
            "endpoint handshake succeeded"
        );
        return Ok(HandshakeResult {
            encoding: RenderEncoding::SemanticFrame,
            endpoint_methods: Some(welcome.methods),
            endpoint_capabilities: Some(welcome.capabilities),
            server_version: Some(welcome.server_version),
        });
    }

    match welcome {
        ServerMessage::Welcome {
            version,
            encoding,
            error,
        } => {
            if let Some(error) = error {
                return Err(ClientError::HandshakeRejected { version, error });
            }
            info!(version, ?encoding, "handshake succeeded");
            Ok(HandshakeResult {
                encoding,
                endpoint_methods: None,
                endpoint_capabilities: None,
                server_version: None,
            })
        }
        _ => Err(ClientError::Protocol(protocol::FramingError::Io(
            io::Error::new(io::ErrorKind::InvalidData, "expected Welcome message"),
        ))),
    }
}

// 逐段轮询同时保留 framing 的读取进度；Windows 同步 pipe 不支持接收超时。
fn read_handshake_welcome(
    stream: &mut LocalStream,
    timeout: Duration,
    cancel: Option<&crate::remote::TaskCancellation>,
) -> Result<ServerMessage, ClientError> {
    struct Reader<'a> {
        stream: &'a mut LocalStream,
        deadline: Instant,
        cancel: Option<&'a crate::remote::TaskCancellation>,
    }
    impl io::Read for Reader<'_> {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            loop {
                if self
                    .cancel
                    .is_some_and(crate::remote::TaskCancellation::is_cancelled)
                {
                    return Err(io::Error::new(
                        io::ErrorKind::ConnectionAborted,
                        crate::i18n::texts().runtime.connection_cancelled,
                    ));
                }
                if Instant::now() >= self.deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        crate::i18n::texts().runtime.handshake_timed_out,
                    ));
                }
                match crate::ipc::poll_local_stream_read_count(self.stream, buffer)? {
                    crate::ipc::LocalStreamReadCount::Data(count) => return Ok(count),
                    crate::ipc::LocalStreamReadCount::Closed => return Ok(0),
                    crate::ipc::LocalStreamReadCount::Pending => {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                }
            }
        }
    }
    stream
        .set_nonblocking(true)
        .map_err(ClientError::ConnectionFailed)?;
    let welcome = protocol::read_message(
        &mut Reader {
            stream,
            deadline: Instant::now() + timeout,
            cancel,
        },
        MAX_FRAME_SIZE,
    );
    let restored = stream.set_nonblocking(false);
    let welcome = welcome?;
    restored.map_err(ClientError::ConnectionFailed)?;
    Ok(welcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use interprocess::local_socket::traits::Listener as _;
    use std::io::Write as _;

    fn pair() -> (LocalStream, LocalStream, std::path::PathBuf) {
        let name = std::env::temp_dir().join(format!(
            "herdr-handshake-{}-{}.sock",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let listener = crate::ipc::bind_local_listener(&name).unwrap();
        let connecting = name.clone();
        let client =
            std::thread::spawn(move || crate::ipc::connect_local_stream(&connecting).unwrap());
        let server = listener.accept().unwrap();
        (client.join().unwrap(), server, name)
    }

    #[test]
    fn cancelled_fragmented_handshake_returns_without_waiting_for_peer() {
        let (mut client, mut server, path) = pair();
        // 只有帧头的一部分到达；取消不能丢失进度后重新阻塞。
        server.write_all(&[1, 0]).unwrap();
        let cancel = crate::remote::TaskCancellation::default();
        let worker_cancel = cancel.clone();
        let worker = std::thread::spawn(move || {
            read_handshake_welcome(&mut client, Duration::from_secs(60), Some(&worker_cancel))
        });
        std::thread::sleep(Duration::from_millis(10));
        cancel.cancel();
        let started = Instant::now();
        assert!(
            matches!(worker.join().unwrap(), Err(ClientError::ConnectionLost(error)) if error.kind() == io::ErrorKind::ConnectionAborted)
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        drop(server);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn idle_handshake_obeys_deadline_without_platform_socket_timeouts() {
        let (mut client, server, path) = pair();
        assert!(
            matches!(read_handshake_welcome(&mut client, Duration::from_millis(10), None), Err(ClientError::ConnectionLost(error)) if error.kind() == io::ErrorKind::TimedOut)
        );
        drop((client, server));
        let _ = std::fs::remove_file(path);
    }
}
