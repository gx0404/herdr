use std::fs;
use std::io::{self, Read};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

#[cfg(unix)]
use interprocess::local_socket::traits::Stream as _;

pub(crate) type LocalListener = interprocess::local_socket::Listener;
pub(crate) type LocalStream = interprocess::local_socket::Stream;

pub(crate) enum LocalStreamRead {
    Data,
    Pending,
    Closed,
}

pub(crate) enum LocalStreamReadCount {
    Data(usize),
    Pending,
    Closed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SocketFileIdentity {
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
    #[cfg(windows)]
    marker: Vec<u8>,
}

pub(crate) fn connect_local_stream(path: &Path) -> io::Result<LocalStream> {
    #[cfg(unix)]
    {
        use interprocess::local_socket::{prelude::*, GenericFilePath};

        let name = path.to_fs_name::<GenericFilePath>()?;
        LocalStream::connect(name)
    }

    #[cfg(windows)]
    {
        crate::platform::connect_local_pipe(path, None)
            .map_err(crate::platform::local_server_connection_error)
    }
}

pub(crate) fn bind_local_listener(path: &Path) -> io::Result<LocalListener> {
    #[cfg(unix)]
    {
        use interprocess::local_socket::{prelude::*, GenericFilePath, ListenerOptions};

        let name = path.to_fs_name::<GenericFilePath>()?;
        ListenerOptions::new()
            .name(name)
            .reclaim_name(false)
            .create_sync()
    }

    #[cfg(windows)]
    {
        let listener = bind_windows_pipe_listener(
            path,
            Some(crate::platform::local_server_security_descriptor()?),
        )?;
        fs::write(path, windows_socket_marker())?;
        Ok(listener)
    }
}

/// Buffer size of every named-pipe instance a Windows local listener creates.
///
/// A nonblocking pipe write that neither fits the free buffer nor meets a pending read
/// writes nothing, and the polling readers peek before reading instead of posting one.
/// interprocess's 512-byte default therefore limited nonblocking writers to 512-byte
/// writes, and blocking writers waited for every read; larger buffers let whole frames
/// through while the peer is between polls.
#[cfg(windows)]
pub(crate) const WINDOWS_PIPE_BUFFER_BYTES: u32 = 1024 * 1024;

/// Creates the local listener for `path` with [`WINDOWS_PIPE_BUFFER_BYTES`] buffers.
///
/// interprocess exposes pipe buffer sizes only on its raw named-pipe listener, so the
/// raw listener is built first (claiming the name as the first instance) and then moved
/// into a local-socket listener created under a unique placeholder name, whose own pipe
/// instance is closed right away. Every later instance comes from the moved listener's
/// options and gets the same buffers.
#[cfg(windows)]
fn bind_windows_pipe_listener(
    path: &Path,
    security_descriptor: Option<interprocess::os::windows::security_descriptor::SecurityDescriptor>,
) -> io::Result<LocalListener> {
    use interprocess::local_socket::{prelude::*, GenericNamespaced, ListenerOptions};
    use interprocess::os::windows::named_pipe::{pipe_mode, PipeListenerOptions};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_PLACEHOLDER: AtomicU64 = AtomicU64::new(0);

    let mut options = PipeListenerOptions::new();
    options.path = std::borrow::Cow::Owned(crate::platform::windows_pipe_names(path)?.open);
    options.input_buffer_size_hint = WINDOWS_PIPE_BUFFER_BYTES;
    options.output_buffer_size_hint = WINDOWS_PIPE_BUFFER_BYTES;
    options.security_descriptor = security_descriptor;
    let pipe_listener = options.create_duplex::<pipe_mode::Bytes>()?;

    let placeholder = format!(
        "herdr-listener-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0),
        NEXT_PLACEHOLDER.fetch_add(1, Ordering::Relaxed)
    );
    let mut listener = ListenerOptions::new()
        .name(placeholder.to_ns_name::<GenericNamespaced>()?)
        .reclaim_name(false)
        .create_sync()?;
    let LocalListener::NamedPipe(named_pipe) = &mut listener;
    drop(std::mem::replace(named_pipe.inner_mut(), pipe_listener));
    Ok(listener)
}

/// Reports whether a Windows local listener at `path` has a pipe instance waiting for a
/// client, without connecting to it: a probe connection would reach the server as a
/// client that hangs up right after connecting. Unix sockets offer no equivalent;
/// callers connect instead.
#[cfg(windows)]
pub(crate) fn local_listener_accepting(path: &Path) -> io::Result<bool> {
    // The marker is written after the pipe exists, so its absence means not bound yet.
    if !path.exists() {
        return Ok(false);
    }
    let name = crate::platform::windows_pipe_names(path)?.wait;
    // WaitNamedPipeW reports a listening instance without opening it. It times out while
    // every instance is busy; callers poll again rather than queue behind a dead server.
    if unsafe { windows_sys::Win32::System::Pipes::WaitNamedPipeW(name.as_ptr(), 1) } != 0 {
        return Ok(true);
    }
    let err = io::Error::last_os_error();
    match err.kind() {
        io::ErrorKind::NotFound | io::ErrorKind::TimedOut => Ok(false),
        _ => Err(err),
    }
}

pub(crate) fn prepare_socket_path(
    path: &Path,
    busy_message: impl FnOnce(&Path) -> String,
) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    if !path.exists() {
        return Ok(());
    }

    match connect_local_stream(path) {
        Ok(_) => {
            return Err(io::Error::new(io::ErrorKind::AddrInUse, busy_message(path)));
        }
        Err(err) if stale_socket_connect_error(err.kind()) => {}
        Err(err) => return Err(err),
    }

    if let Err(err) = fs::remove_file(path) {
        if err.kind() != io::ErrorKind::NotFound {
            return Err(err);
        }
    }

    Ok(())
}

fn stale_socket_connect_error(kind: io::ErrorKind) -> bool {
    matches!(
        kind,
        io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound | io::ErrorKind::TimedOut
    ) || (cfg!(windows) && kind == io::ErrorKind::WouldBlock)
}

pub(crate) fn local_stream_peer_closed(stream: &mut LocalStream) -> io::Result<bool> {
    probe_stream_closed(stream)
}

pub(crate) fn set_local_stream_polling(stream: &mut LocalStream, enabled: bool) -> io::Result<()> {
    #[cfg(unix)]
    {
        stream.set_nonblocking(enabled)
    }

    #[cfg(windows)]
    {
        let _ = (stream, enabled);
        Ok(())
    }
}

/// Binds a listener for private terminal traffic. Unix callers restrict the
/// socket file after binding; Windows must set the named-pipe DACL at creation.
pub(crate) fn bind_private_local_listener(path: &Path) -> io::Result<LocalListener> {
    #[cfg(unix)]
    {
        bind_local_listener(path)
    }

    #[cfg(windows)]
    {
        use interprocess::os::windows::security_descriptor::SecurityDescriptor;
        use widestring::U16CString;

        let sddl = U16CString::from_str("D:P(A;;GA;;;SY)(A;;GA;;;OW)")
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;
        let security_descriptor = SecurityDescriptor::deserialize(&sddl)?;
        let listener = bind_windows_pipe_listener(path, Some(security_descriptor))?;
        fs::write(path, windows_socket_marker())?;
        Ok(listener)
    }
}

pub(crate) fn poll_local_stream_read(
    stream: &mut LocalStream,
    buf: &mut [u8],
) -> io::Result<LocalStreamRead> {
    match poll_local_stream_read_count(stream, buf)? {
        LocalStreamReadCount::Data(read) => {
            let _ = read;
            Ok(LocalStreamRead::Data)
        }
        LocalStreamReadCount::Pending => Ok(LocalStreamRead::Pending),
        LocalStreamReadCount::Closed => Ok(LocalStreamRead::Closed),
    }
}

pub(crate) fn poll_local_stream_read_count(
    stream: &mut LocalStream,
    buf: &mut [u8],
) -> io::Result<LocalStreamReadCount> {
    #[cfg(unix)]
    {
        match stream.read(buf) {
            Ok(0) => Ok(LocalStreamReadCount::Closed),
            Ok(read) => Ok(LocalStreamReadCount::Data(read)),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                Ok(LocalStreamReadCount::Pending)
            }
            Err(err) => Err(err),
        }
    }

    #[cfg(windows)]
    {
        match windows_named_pipe_available(stream)? {
            None => Ok(LocalStreamReadCount::Closed),
            Some(0) => Ok(LocalStreamReadCount::Pending),
            Some(_) => match stream.read(buf) {
                Ok(0) => Ok(LocalStreamReadCount::Closed),
                Ok(read) => Ok(LocalStreamReadCount::Data(read)),
                Err(err) if is_connection_closed_error(&err) => Ok(LocalStreamReadCount::Closed),
                Err(err) => Err(err),
            },
        }
    }
}

/// 探测对端是否已关闭，且**不消费**任何字节。此前这里用 `read()`，读到 1 字节
/// 就判连接关闭（`Ok(_) => Ok(true)`），于是流式连接上追加的换行/心跳既被吞掉
/// 又会掐断 `events.subscribe` / `*.subscribe` / `*.wait` 流（HSR-07）。
///
/// 两步判定：
/// 1. `poll(POLLIN, 0)`：挂断/出错即算关闭——即便内核缓冲里还有残留数据也要
///    算关闭，否则"写一个字节再断开"的客户端会让流式连接的探测永远报活着，
///    订阅线程与 fd 泄漏。
/// 2. 有可读数据时用 `recv(MSG_PEEK|MSG_DONTWAIT)` 区分真数据与 EOF：
///    `>0` = 未关闭且字节原样留在内核缓冲里，`0` = 对端已关闭写端。
///
/// 「不消费字节」这一点两平台一致，**挂断判定尚未对齐**：windows 分支只看
/// `PeekNamedPipe` 的 available，缓冲里还有字节时一律报"未关闭"，所以第 1 步
/// 修掉的"写完再断开"泄漏在 windows 上仍然存在。补齐挂断判定需要 windows
/// 宿主实测，留给平台窗口（见 `probe_stream_closed` 的 windows 分支）。
#[cfg(unix)]
fn probe_stream_closed(stream: &mut LocalStream) -> io::Result<bool> {
    use std::os::fd::{AsFd, AsRawFd};

    let LocalStream::UdSocket(socket) = stream;
    let fd = socket.as_fd().as_raw_fd();

    let mut poll_fd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let polled = unsafe { libc::poll(&mut poll_fd, 1, 0) };
    if polled < 0 {
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::Interrupted {
            return Ok(false);
        }
        if is_connection_closed_error(&err) {
            return Ok(true);
        }
        return Err(err);
    }
    if polled == 0 {
        return Ok(false);
    }
    if poll_fd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
        return Ok(true);
    }
    if poll_fd.revents & libc::POLLIN == 0 {
        return Ok(false);
    }

    let mut probe = [0u8; 1];
    // MSG_DONTWAIT 让探测本身不阻塞，因此不必再切换连接的阻塞模式。
    let peeked = unsafe {
        libc::recv(
            fd,
            probe.as_mut_ptr().cast::<libc::c_void>(),
            probe.len(),
            libc::MSG_PEEK | libc::MSG_DONTWAIT,
        )
    };
    if peeked > 0 {
        return Ok(false);
    }
    if peeked == 0 {
        return Ok(true);
    }

    let err = io::Error::last_os_error();
    if matches!(
        err.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
    ) {
        return Ok(false);
    }
    if is_connection_closed_error(&err) {
        return Ok(true);
    }
    Err(err)
}

/// `PeekNamedPipe` 不消费字节，这点与 unix 分支的 `MSG_PEEK` 一致。
///
/// 但挂断判定与 unix 分支不同：这里只要 available > 0 就报"未关闭"，
/// 因此"客户端写完再断开"时探测会一直报活着，订阅线程与句柄不会回收
/// （unix 分支已用 `poll(POLLIN, 0)` 的 `POLLHUP/POLLERR/POLLNVAL` 修掉）。
/// 补齐需要 `PeekNamedPipe` 返回 `ERROR_BROKEN_PIPE`/`ERROR_PIPE_NOT_CONNECTED`
/// 时无条件判关闭，且要在 windows 宿主上实测验证，故推迟到平台窗口处理。
#[cfg(windows)]
fn probe_stream_closed(stream: &mut LocalStream) -> io::Result<bool> {
    Ok(windows_named_pipe_available(stream)?.is_none())
}

#[cfg(windows)]
fn windows_named_pipe_available(stream: &mut LocalStream) -> io::Result<Option<u32>> {
    use std::os::windows::io::{AsHandle, AsRawHandle};

    let LocalStream::NamedPipe(pipe) = stream;
    let mut available = 0;
    let ok = unsafe {
        windows_sys::Win32::System::Pipes::PeekNamedPipe(
            pipe.as_handle().as_raw_handle(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            &mut available,
            std::ptr::null_mut(),
        )
    };
    if ok != 0 {
        return Ok(Some(available));
    }

    let err = io::Error::last_os_error();
    if is_connection_closed_error(&err) || windows_named_pipe_closed_error(&err) {
        return Ok(None);
    }
    Err(err)
}

pub(crate) fn is_connection_closed_error(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::NotConnected
            | io::ErrorKind::UnexpectedEof
            | io::ErrorKind::WriteZero
    )
}

#[cfg(windows)]
fn windows_named_pipe_closed_error(err: &io::Error) -> bool {
    matches!(err.raw_os_error(), Some(6 | 109 | 232 | 233))
}

pub(crate) fn socket_file_identity(path: &Path) -> io::Result<SocketFileIdentity> {
    #[cfg(windows)]
    {
        Ok(SocketFileIdentity {
            marker: fs::read(path)?,
        })
    }

    #[cfg(unix)]
    {
        let metadata = fs::metadata(path)?;
        Ok(SocketFileIdentity {
            dev: metadata.dev(),
            ino: metadata.ino(),
        })
    }
}

pub(crate) fn remove_socket_file_if_owned(
    path: &Path,
    identity: &SocketFileIdentity,
) -> io::Result<()> {
    let current = match socket_file_identity(path) {
        Ok(current) => current,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };

    if current != *identity {
        return Ok(());
    }

    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

#[cfg(windows)]
fn windows_socket_marker() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("{}:{now}", std::process::id())
}

#[cfg(unix)]
pub(crate) fn restrict_socket_permissions(path: &Path, mode: u32) -> io::Result<()> {
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(mode);
    fs::set_permissions(path, permissions)
}

#[cfg(windows)]
pub(crate) fn restrict_socket_permissions(_path: &Path, _mode: u32) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use interprocess::local_socket::traits::Listener as _;
    use std::path::PathBuf;

    #[test]
    fn stale_socket_connect_errors_keep_unix_would_block_strict() {
        assert!(stale_socket_connect_error(io::ErrorKind::ConnectionRefused));
        assert!(stale_socket_connect_error(io::ErrorKind::NotFound));
        assert!(stale_socket_connect_error(io::ErrorKind::TimedOut));
        assert_eq!(
            stale_socket_connect_error(io::ErrorKind::WouldBlock),
            cfg!(windows)
        );
    }

    #[cfg(windows)]
    fn pipe_path_at_utf16_length(root: &Path, leaf: &str, units: usize, fill: &str) -> PathBuf {
        let raw_len = |path: &Path| {
            format!(r"\\.\pipe\{}", path.to_string_lossy())
                .encode_utf16()
                .count()
        };
        let mut directory = root.to_path_buf();
        let mut remaining = units.checked_sub(raw_len(&directory.join(leaf))).unwrap();
        let fill_units = fill.encode_utf16().count();
        while remaining != 0 {
            assert!(remaining >= 2);
            let mut take = remaining.min(64);
            if remaining - take == 1 {
                take -= 1;
            }
            let component_units = take - 1;
            directory.push(format!(
                "{}{}",
                fill.repeat(component_units / fill_units),
                "x".repeat(component_units % fill_units)
            ));
            remaining = units - raw_len(&directory.join(leaf));
        }
        let path = directory.join(leaf);
        assert_eq!(raw_len(&path), units);
        path
    }

    #[cfg(windows)]
    fn exchange_pipe_bytes(client: &mut LocalStream, server: &mut LocalStream) {
        use interprocess::local_socket::traits::Stream as _;
        use std::io::Write as _;
        use std::time::{Duration, Instant};

        client.set_nonblocking(true).unwrap();
        server.set_nonblocking(true).unwrap();
        let transfer = |sender: &mut LocalStream, receiver: &mut LocalStream, payload: &[u8]| {
            assert_eq!(sender.write(payload).unwrap(), payload.len());
            let deadline = Instant::now() + Duration::from_secs(30);
            let mut received = vec![0; payload.len()];
            let mut offset = 0;
            while offset != received.len() {
                assert!(Instant::now() < deadline, "bounded pipe echo read");
                match poll_local_stream_read_count(receiver, &mut received[offset..]).unwrap() {
                    LocalStreamReadCount::Data(count) => offset += count,
                    LocalStreamReadCount::Pending => std::thread::sleep(Duration::from_millis(1)),
                    LocalStreamReadCount::Closed => panic!("pipe closed during echo"),
                }
            }
            assert_eq!(received, payload);
            // Receipt is confirmed above; avoid asynchronous linger delaying the next bind.
            let LocalStream::NamedPipe(pipe) = sender;
            pipe.inner().assume_flushed();
        };
        transfer(client, server, b"client-to-server");
        transfer(server, client, b"server-to-client");
    }

    #[cfg(windows)]
    fn long_pipe_round_trip(private: bool) {
        let root = crate::config::test_dirs::TempDir::new("long-pipe");
        let bind = if private {
            bind_private_local_listener
        } else {
            bind_local_listener
        };
        for units in [259, 260, 273, 420] {
            for (kind, fill) in [("ascii", "p"), ("bmp", "管"), ("supplementary", "🦀")] {
                let base = root.join(format!("{units}-{kind}"));
                let leaf = if private {
                    "herdr-client.sock"
                } else {
                    "herdr.sock"
                };
                let path = pipe_path_at_utf16_length(&base, leaf, units, fill);
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(&path, b"filesystem marker succeeds").unwrap();
                assert_eq!(fs::read(&path).unwrap(), b"filesystem marker succeeds");
                fs::remove_file(&path).unwrap();
                let listener = bind(&path).unwrap_or_else(|error| panic!("long pipe bind private={private} raw_units={units} raw_error={:?}: {error}", error.raw_os_error()));
                let original = socket_file_identity(&path).unwrap();
                assert!(
                    bind(&path).is_err(),
                    "first-instance ownership must reject a second listener"
                );
                assert_eq!(socket_file_identity(&path).unwrap(), original);
                assert!(local_listener_accepting(&path).unwrap());
                for _ in 0..2 {
                    let mut client = connect_local_stream(&path).unwrap();
                    let mut server = listener.accept().unwrap();
                    for stream in [&client, &server] {
                        let (out_size, in_size) = named_pipe_buffer_sizes(stream);
                        assert!(
                            out_size >= WINDOWS_PIPE_BUFFER_BYTES
                                && in_size >= WINDOWS_PIPE_BUFFER_BYTES
                        );
                    }
                    exchange_pipe_bytes(&mut client, &mut server);
                }
                drop(listener);
                let rebound = bind(&path).expect("all pipe instances released before rebind");
                let current = socket_file_identity(&path).unwrap();
                assert_ne!(original, current);
                remove_socket_file_if_owned(&path, &original).unwrap();
                assert_eq!(socket_file_identity(&path).unwrap(), current);
                drop(rebound);
                remove_socket_file_if_owned(&path, &current).unwrap();
                assert!(!path.exists());
                println!("long-pipe private={private} raw_utf16={units} kind={kind} instances=2 bidirectional_echo=PASS rebind=PASS marker_identity=PASS");
            }
        }
        let path = root.path().to_path_buf();
        drop(root);
        assert!(!path.exists(), "owned long-path fixture must be removed");
    }

    #[cfg(windows)]
    #[test]
    fn windows_long_public_pipe_round_trip() {
        long_pipe_round_trip(false);
    }

    #[cfg(windows)]
    #[test]
    fn windows_long_private_pipe_round_trip() {
        long_pipe_round_trip(true);
    }

    #[cfg(windows)]
    fn accept_pipe_ready(listener: &LocalListener) -> LocalStream {
        use interprocess::local_socket::ListenerNonblockingMode;
        use std::time::{Duration, Instant};
        listener
            .set_nonblocking(ListenerNonblockingMode::Accept)
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            match listener.accept() {
                Ok(stream) => return stream,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "bounded listener accept");
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("accept failed: {error}"),
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_pipe_short_legacy_interoperability_is_bidirectional() {
        use interprocess::local_socket::{prelude::*, GenericNamespaced, ListenerOptions};
        let root = crate::config::test_dirs::TempDir::new("legacy-pipe");
        for legacy_listener in [false, true] {
            let path = root.join(format!("legacy-{legacy_listener}.sock"));
            let name = path.to_string_lossy().to_string();
            let listener = if legacy_listener {
                ListenerOptions::new()
                    .name(name.as_str().to_ns_name::<GenericNamespaced>().unwrap())
                    .reclaim_name(false)
                    .create_sync()
                    .unwrap()
            } else {
                bind_local_listener(&path).unwrap()
            };
            let mut client = if legacy_listener {
                connect_local_stream(&path).unwrap()
            } else {
                LocalStream::connect(name.as_str().to_ns_name::<GenericNamespaced>().unwrap())
                    .unwrap()
            };
            let mut server = accept_pipe_ready(&listener);
            exchange_pipe_bytes(&mut client, &mut server);
            println!("legacy-interop legacy_listener={legacy_listener} bidirectional_echo=PASS");
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_pipe_aliases_share_bind_connect_probe_and_readiness() {
        use interprocess::local_socket::ListenerNonblockingMode;
        let root = crate::config::test_dirs::TempDir::new("pipe-alias");
        fs::create_dir_all(root.join("case")).unwrap();
        fs::create_dir_all(root.join("unused")).unwrap();
        fs::create_dir_all(root.join("a".repeat(190))).unwrap();
        fs::create_dir_all(root.join("b".repeat(190))).unwrap();
        let canonical = root.join("case/endpoint.sock");
        let aliases = [
            root.join(".").join("case/endpoint.sock"),
            root.join("unused/../case/endpoint.sock"),
            PathBuf::from(format!("{}\\\\case\\endpoint.sock", root.display())),
            PathBuf::from(canonical.to_string_lossy().replace('\\', "/")),
            PathBuf::from(canonical.to_string_lossy().to_ascii_uppercase()),
            root.join("a".repeat(190))
                .join("..")
                .join("b".repeat(190))
                .join("..")
                .join("case/endpoint.sock"),
        ];
        for alias in aliases {
            let listener = bind_local_listener(&alias).unwrap();
            listener
                .set_nonblocking(ListenerNonblockingMode::Accept)
                .unwrap();
            for name in [&alias, &canonical] {
                assert!(local_listener_accepting(name).unwrap());
                assert!(
                    matches!(listener.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock)
                );
                let mut client = connect_local_stream(name).unwrap();
                let mut server = accept_pipe_ready(&listener);
                exchange_pipe_bytes(&mut client, &mut server);
            }
            crate::platform::probe_local_server(&alias).unwrap();
            drop(listener);
            fs::remove_file(&canonical).unwrap();
            println!("pipe-alias raw_utf16={} canonical={} echo=PASS readiness_nonconnecting=PASS probe=PASS", format!(r"\\.\pipe\{}", alias.to_string_lossy()).encode_utf16().count(), canonical.display());
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_pipe_busy_probe_has_one_deadline_and_recovers_on_release() {
        use std::time::{Duration, Instant};
        let root = crate::config::test_dirs::TempDir::new("busy-long-pipe");
        let path = pipe_path_at_utf16_length(root.path(), "herdr.sock", 420, "管");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let listener = bind_local_listener(&path).unwrap();
        let occupied = connect_local_stream(&path).unwrap();
        let started = Instant::now();
        let error = crate::platform::probe_local_server(&path).unwrap_err();
        let elapsed = started.elapsed();
        assert_eq!(
            error.kind(),
            io::ErrorKind::TimedOut,
            "busy must not become absence"
        );
        assert!(
            elapsed >= Duration::from_millis(400) && elapsed < Duration::from_secs(2),
            "absolute 500ms probe budget: {elapsed:?}"
        );
        assert!(!local_listener_accepting(&path).unwrap());
        let (ready_tx, ready) = std::sync::mpsc::channel();
        let (done_tx, done) = std::sync::mpsc::channel();
        let probe_path = path.clone();
        let worker = std::thread::spawn(move || {
            ready_tx.send(()).unwrap();
            let result = crate::platform::probe_local_server(&probe_path);
            done_tx.send(result).unwrap();
        });
        ready.recv_timeout(Duration::from_secs(30)).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        assert!(matches!(
            done.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        let server = accept_pipe_ready(&listener);
        let result = done.recv_timeout(Duration::from_secs(30)).unwrap();
        worker.join().unwrap();
        result.expect("availability releases the canonical legacy wait");
        drop(server);
        drop(occupied);
        drop(listener);
        println!("pipe-busy raw_utf16=420 timeout_kind={:?} elapsed_ms={} release_probe=PASS worker_joined=true", error.kind(), elapsed.as_millis());
    }

    #[cfg(windows)]
    #[test]
    fn windows_pipe_invalid_names_and_marker_failure_preserve_errors_and_release() {
        let root = crate::config::test_dirs::TempDir::new("pipe-error");
        let absent = root.join("absent.sock");
        for error in [
            connect_local_stream(&absent).unwrap_err(),
            crate::platform::probe_local_server(&absent).unwrap_err(),
        ] {
            assert_eq!(error.kind(), io::ErrorKind::NotFound);
        }
        let invalid = root.join("invalid\0pipe.sock");
        assert_eq!(
            connect_local_stream(&invalid).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            crate::platform::probe_local_server(&invalid)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert!(
            matches!(crate::platform::windows_pipe_names(Path::new("")), Err(error) if error.kind() == io::ErrorKind::InvalidInput)
        );
        for private in [false, true] {
            let base = root.join(format!("private-{private}"));
            let path = pipe_path_at_utf16_length(&base, "marker.sock", 420, "p");
            fs::create_dir_all(&path).unwrap();
            let bind = if private {
                bind_private_local_listener
            } else {
                bind_local_listener
            };
            assert!(bind(&path).is_err(), "marker path is a directory");
            fs::remove_dir(&path).unwrap();
            let listener =
                bind(&path).expect("failed marker publication must close the pipe instance");
            let mut client = connect_local_stream(&path).unwrap();
            let mut server = accept_pipe_ready(&listener);
            exchange_pipe_bytes(&mut client, &mut server);
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_pipe_long_sessions_and_endpoint_suffixes_do_not_collide() {
        let root = crate::config::test_dirs::TempDir::new("pipe-isolation");
        let parent = pipe_path_at_utf16_length(root.path(), "seed.sock", 390, "🦀")
            .parent()
            .unwrap()
            .to_path_buf();
        let paths = [
            parent.join("session-a/herdr.sock"),
            parent.join("session-a/herdr-client.sock"),
            parent.join("session-b/herdr.sock"),
        ];
        let listeners = paths
            .iter()
            .enumerate()
            .map(|(index, path)| {
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                if index == 1 {
                    bind_private_local_listener(path)
                } else {
                    bind_local_listener(path)
                }
                .unwrap()
            })
            .collect::<Vec<_>>();
        for (path, listener) in paths.iter().zip(&listeners) {
            let mut client = connect_local_stream(path).unwrap();
            let mut server = accept_pipe_ready(listener);
            exchange_pipe_bytes(&mut client, &mut server);
        }
        println!("long-pipe distinct_sessions=2 distinct_suffixes=2 listeners=3 echo=PASS");
    }

    #[cfg(windows)]
    #[test]
    fn private_named_pipe_accepts_same_user() {
        use std::io::Write as _;

        let path = temp_socket_marker_path("private-pipe");
        let _ = fs::remove_file(&path);
        let listener = bind_private_local_listener(&path).unwrap();
        let mut client = connect_local_stream(&path).unwrap();
        let mut server = listener.accept().unwrap();
        client.write_all(b"remote").unwrap();

        let mut buffer = [0_u8; 16];
        assert!(matches!(
            poll_local_stream_read_count(&mut server, &mut buffer).unwrap(),
            LocalStreamReadCount::Data(6)
        ));
        assert_eq!(&buffer[..6], b"remote");

        drop(client);
        drop(server);
        drop(listener);
        let _ = fs::remove_file(path);
    }

    #[cfg(windows)]
    #[test]
    fn remove_socket_file_if_owned_compares_windows_marker_contents() {
        let path = temp_socket_marker_path("same-len-marker");
        let _ = fs::remove_file(&path);

        fs::write(&path, b"marker-aa").expect("write first marker");
        let identity = socket_file_identity(&path).expect("read first identity");
        fs::write(&path, b"marker-bb").expect("replace with same-length marker");

        remove_socket_file_if_owned(&path, &identity).expect("remove owned marker");

        assert!(path.exists(), "same-length replacement marker must survive");

        let _ = fs::remove_file(&path);
    }

    #[cfg(windows)]
    #[test]
    fn idle_named_pipe_peer_is_not_treated_as_closed() {
        let path = temp_socket_marker_path("idle-pipe");
        let listener = bind_local_listener(&path).unwrap();
        let _client = connect_local_stream(&path).unwrap();
        let mut server = listener.accept().unwrap();

        assert!(!local_stream_peer_closed(&mut server).unwrap());

        let _ = fs::remove_file(path);
    }

    #[cfg(windows)]
    #[test]
    fn disconnected_named_pipe_peer_is_treated_as_closed() {
        let path = temp_socket_marker_path("disconnected-pipe");
        let listener = bind_local_listener(&path).unwrap();
        let client = connect_local_stream(&path).unwrap();
        let mut server = listener.accept().unwrap();

        drop(client);

        assert!(local_stream_peer_closed(&mut server).unwrap());

        let _ = fs::remove_file(path);
    }

    #[cfg(windows)]
    fn named_pipe_buffer_sizes(stream: &LocalStream) -> (u32, u32) {
        use std::os::windows::io::{AsHandle, AsRawHandle};

        let LocalStream::NamedPipe(pipe) = stream;
        let (mut out_size, mut in_size) = (0, 0);
        let ok = unsafe {
            windows_sys::Win32::System::Pipes::GetNamedPipeInfo(
                pipe.as_handle().as_raw_handle(),
                std::ptr::null_mut(),
                &mut out_size,
                &mut in_size,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(ok, 0, "GetNamedPipeInfo: {}", io::Error::last_os_error());
        (out_size, in_size)
    }

    #[cfg(windows)]
    #[test]
    fn named_pipe_listeners_give_every_instance_large_buffers() {
        for (name, private) in [("buffers-public", false), ("buffers-private", true)] {
            let path = temp_socket_marker_path(name);
            let _ = fs::remove_file(&path);
            let listener = if private {
                bind_private_local_listener(&path)
            } else {
                bind_local_listener(&path)
            }
            .unwrap();
            // The first instance is created with the listener; later ones by each accept.
            for _ in 0..2 {
                let client = connect_local_stream(&path).unwrap();
                let server = listener.accept().unwrap();
                for stream in [&client, &server] {
                    let (out_size, in_size) = named_pipe_buffer_sizes(stream);
                    assert!(
                        out_size >= WINDOWS_PIPE_BUFFER_BYTES
                            && in_size >= WINDOWS_PIPE_BUFFER_BYTES,
                        "{name}: pipe buffers are {out_size}/{in_size} bytes"
                    );
                }
            }
            drop(listener);
            let _ = fs::remove_file(path);
        }
    }

    #[cfg(windows)]
    #[test]
    fn nonblocking_write_is_buffered_while_the_peer_has_no_read_pending() {
        use interprocess::local_socket::traits::Stream as _;
        use std::io::Write as _;

        let path = temp_socket_marker_path("nonblocking-buffered-write");
        let _ = fs::remove_file(&path);
        let listener = bind_private_local_listener(&path).unwrap();
        let mut client = connect_local_stream(&path).unwrap();
        let mut server = listener.accept().unwrap();
        client.set_nonblocking(true).unwrap();

        // With 512-byte pipe buffers this write reports 0 bytes until the peer reads.
        let chunk = vec![0x5a_u8; 64 * 1024];
        assert_eq!(client.write(&chunk).unwrap(), chunk.len());

        let mut received = vec![0_u8; chunk.len()];
        server.read_exact(&mut received).unwrap();
        assert_eq!(received, chunk);
        drop(client);
        drop(server);
        drop(listener);
        let _ = fs::remove_file(path);
    }

    #[cfg(windows)]
    #[test]
    fn megabyte_frames_round_trip_between_blocking_and_polling_ends() {
        use interprocess::local_socket::traits::Stream as _;
        use std::io::Write as _;
        use std::time::{Duration, Instant};

        let path = temp_socket_marker_path("megabyte-frames");
        let _ = fs::remove_file(&path);
        let listener = bind_local_listener(&path).unwrap();
        let mut client = connect_local_stream(&path).unwrap();
        let server = listener.accept().unwrap();
        let frame: Vec<u8> = (0..3 * 1024 * 1024)
            .map(|index| (index % 251) as u8)
            .collect();
        let deadline = Instant::now() + Duration::from_secs(30);

        // Server to client: a blocking writer and a reader that peeks before reading.
        let outgoing = frame.clone();
        let writer = std::thread::spawn(move || {
            let mut server = server;
            server.write_all(&outgoing).unwrap();
            server
        });
        let mut received = vec![0_u8; frame.len()];
        let mut filled = 0;
        while filled < received.len() {
            assert!(
                Instant::now() < deadline,
                "polling reader stalled at {filled} bytes"
            );
            match poll_local_stream_read_count(&mut client, &mut received[filled..]).unwrap() {
                LocalStreamReadCount::Data(read) => filled += read,
                LocalStreamReadCount::Pending => std::thread::sleep(Duration::from_millis(2)),
                LocalStreamReadCount::Closed => panic!("server closed the pipe"),
            }
        }
        assert!(received == frame, "server-to-client frame corrupted");
        let mut server = writer.join().unwrap();

        // Client to server: nonblocking 64 KiB writes into a blocking reader.
        client.set_nonblocking(true).unwrap();
        let expected = frame.len();
        let reader = std::thread::spawn(move || {
            let mut incoming = vec![0_u8; expected];
            server.read_exact(&mut incoming).unwrap();
            incoming
        });
        let mut remaining = frame.as_slice();
        while !remaining.is_empty() {
            assert!(Instant::now() < deadline, "nonblocking writer stalled");
            let chunk = &remaining[..remaining.len().min(64 * 1024)];
            match client.write(chunk) {
                Ok(0) => std::thread::sleep(Duration::from_millis(2)),
                Ok(written) => remaining = &remaining[written..],
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(2))
                }
                Err(err) => panic!("nonblocking write failed: {err}"),
            }
        }
        assert!(
            reader.join().unwrap() == frame,
            "client-to-server frame corrupted"
        );
        drop(client);
        drop(listener);
        let _ = fs::remove_file(path);
    }

    #[cfg(windows)]
    #[test]
    fn polling_reader_on_an_idle_pipe_notices_stop_promptly() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use std::time::{Duration, Instant};

        let path = temp_socket_marker_path("reader-stop");
        let _ = fs::remove_file(&path);
        let listener = bind_private_local_listener(&path).unwrap();
        let client = connect_local_stream(&path).unwrap();
        let _server = listener.accept().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let reader_stop = stop.clone();
        let (stopped_tx, stopped_rx) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut client = client;
            let mut buffer = [0_u8; 64];
            while !reader_stop.load(Ordering::Acquire) {
                let read = poll_local_stream_read_count(&mut client, &mut buffer).unwrap();
                assert!(matches!(read, LocalStreamReadCount::Pending));
                crate::platform::wait_client_stream_readable(&client).unwrap();
            }
            stopped_tx.send(Instant::now()).unwrap();
        });

        std::thread::sleep(Duration::from_millis(50));
        assert!(
            stopped_rx.try_recv().is_err(),
            "an idle pipe keeps the reader polling"
        );
        let requested = Instant::now();
        stop.store(true, Ordering::Release);
        let stopped = stopped_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the reader never blocks in the kernel, so it sees the stop flag");
        assert!(stopped.duration_since(requested) < Duration::from_secs(5));
        reader.join().unwrap();
        drop(listener);
        let _ = fs::remove_file(path);
    }

    #[cfg(windows)]
    #[test]
    fn listener_probe_finds_listeners_bound_at_non_canonical_paths() {
        let dir = std::env::temp_dir();
        let tag = format!("herdr-probe-names-{}", std::process::id());
        for (label, path) in [
            ("dot", dir.join(".").join(format!("{tag}-dot.sock"))),
            (
                "dotdot",
                dir.join("missing-dir")
                    .join("..")
                    .join(format!("{tag}-dotdot.sock")),
            ),
            (
                "doubled separator",
                PathBuf::from(format!(
                    "{}\\\\{tag}-doubled.sock",
                    dir.to_string_lossy().trim_end_matches('\\')
                )),
            ),
        ] {
            let _ = fs::remove_file(&path);
            let listener = bind_local_listener(&path).unwrap();
            assert!(
                local_listener_accepting(&path).unwrap(),
                "{label}: the probe must find a listener bound at {}",
                path.display()
            );
            let client = connect_local_stream(&path).unwrap();
            let server = listener.accept().unwrap();
            drop(server);
            drop(client);
            drop(listener);
            let _ = fs::remove_file(path);
        }
    }

    #[cfg(windows)]
    #[test]
    fn listener_probe_waits_while_every_pipe_instance_is_busy() {
        let path = temp_socket_marker_path("listener-busy");
        let _ = fs::remove_file(&path);
        let listener = bind_local_listener(&path).unwrap();
        assert!(local_listener_accepting(&path).unwrap());

        // A connected client occupies the only instance until the server accepts it.
        let client = connect_local_stream(&path).unwrap();
        assert!(!local_listener_accepting(&path).unwrap());

        let server = listener.accept().unwrap();
        assert!(local_listener_accepting(&path).unwrap());

        drop(server);
        drop(client);
        drop(listener);
        let _ = fs::remove_file(path);
    }

    #[cfg(windows)]
    #[test]
    fn listener_probe_does_not_consume_a_pipe_instance() {
        use interprocess::local_socket::ListenerNonblockingMode;

        let path = temp_socket_marker_path("listener-probe");
        let _ = fs::remove_file(&path);
        assert!(!local_listener_accepting(&path).unwrap());

        let listener = bind_local_listener(&path).unwrap();
        listener
            .set_nonblocking(ListenerNonblockingMode::Accept)
            .unwrap();
        assert!(local_listener_accepting(&path).unwrap());
        assert!(local_listener_accepting(&path).unwrap());
        let pending = listener.accept();
        assert!(
            matches!(&pending, Err(err) if err.kind() == io::ErrorKind::WouldBlock),
            "the probe must not leave a connection behind"
        );

        let client = connect_local_stream(&path).unwrap();
        let server = listener.accept().unwrap();

        drop(server);
        drop(client);
        drop(listener);
        assert!(!local_listener_accepting(&path).unwrap());
        let _ = fs::remove_file(path);
    }

    /// HSR-07 回归：流式连接上对端追加的字节既不能被探测消费，也不能被
    /// 误判成连接关闭，否则 `events.subscribe` 之类的长流会被一条心跳掐断。
    #[cfg(unix)]
    #[test]
    fn pending_unix_peer_bytes_are_peeked_without_closing_the_stream() {
        use std::io::Write as _;
        use std::time::{Duration, Instant};

        let path = temp_socket_marker_path("peek-pending");
        let _ = fs::remove_file(&path);
        let listener = bind_local_listener(&path).expect("bind listener");
        let mut client = connect_local_stream(&path).expect("connect client");
        let mut server = listener.accept().expect("accept client");

        assert!(
            !local_stream_peer_closed(&mut server).expect("probe idle peer"),
            "空闲连接不得被判为关闭"
        );

        client.write_all(b"ping\n").expect("write heartbeat");
        client.flush().expect("flush heartbeat");

        for _ in 0..5 {
            assert!(
                !local_stream_peer_closed(&mut server).expect("probe peer with pending bytes"),
                "对端写入的字节不得被判为连接关闭"
            );
        }

        set_local_stream_polling(&mut server, true).expect("enable polling");
        let mut buffer = [0_u8; 16];
        let deadline = Instant::now() + Duration::from_secs(2);
        let read = loop {
            match poll_local_stream_read_count(&mut server, &mut buffer).expect("read peer bytes") {
                LocalStreamReadCount::Data(read) => break read,
                LocalStreamReadCount::Pending => {
                    assert!(Instant::now() < deadline, "等待对端字节超时");
                    std::thread::sleep(Duration::from_millis(10));
                }
                LocalStreamReadCount::Closed => panic!("连接不应关闭"),
            }
        };
        set_local_stream_polling(&mut server, false).expect("disable polling");
        assert_eq!(&buffer[..read], b"ping\n", "探测不得消费任何字节");

        drop(client);
        drop(server);
        drop(listener);
        let _ = fs::remove_file(path);
    }

    /// 对端写完字节再断开时必须判为关闭：否则残留数据会让探测永远报"活着"，
    /// 流式连接的线程与 fd 永久泄漏。
    #[cfg(unix)]
    #[test]
    fn disconnected_unix_peer_with_pending_bytes_is_treated_as_closed() {
        use std::io::Write as _;

        let path = temp_socket_marker_path("peek-disconnected-pending");
        let _ = fs::remove_file(&path);
        let listener = bind_local_listener(&path).expect("bind listener");
        let mut client = connect_local_stream(&path).expect("connect client");
        let mut server = listener.accept().expect("accept client");

        client.write_all(b"ping\n").expect("write heartbeat");
        client.flush().expect("flush heartbeat");
        drop(client);

        assert!(
            local_stream_peer_closed(&mut server).expect("probe disconnected peer"),
            "对端断开必须判为关闭，残留数据不得让探测一直报活着"
        );

        drop(server);
        drop(listener);
        let _ = fs::remove_file(path);
    }

    #[cfg(unix)]
    #[test]
    fn disconnected_unix_peer_is_treated_as_closed() {
        let path = temp_socket_marker_path("peek-disconnected");
        let _ = fs::remove_file(&path);
        let listener = bind_local_listener(&path).expect("bind listener");
        let client = connect_local_stream(&path).expect("connect client");
        let mut server = listener.accept().expect("accept client");

        drop(client);

        assert!(
            local_stream_peer_closed(&mut server).expect("probe disconnected peer"),
            "对端断开必须判为关闭"
        );

        drop(server);
        drop(listener);
        let _ = fs::remove_file(path);
    }

    fn temp_socket_marker_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("herdr-{name}-{}.sock", std::process::id()))
    }
}
