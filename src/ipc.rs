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
        use interprocess::local_socket::{prelude::*, GenericNamespaced};

        let name = path.to_string_lossy().to_string();
        let name = name.to_ns_name::<GenericNamespaced>()?;
        LocalStream::connect(name)
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
        let listener = bind_windows_pipe_listener(path, None)?;
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

#[cfg(windows)]
fn windows_pipe_path(path: &Path) -> io::Result<widestring::U16CString> {
    // Same `\\.\pipe\` + name mapping as interprocess's `GenericNamespaced`.
    widestring::U16CString::from_str(format!(r"\\.\pipe\{}", path.to_string_lossy()))
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))
}

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
    options.path = std::borrow::Cow::Owned(windows_pipe_path(path)?);
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

/// The pipe name for `path` as `CreateNamedPipeW` and `CreateFileW` register and look it
/// up: they canonicalise `.`/`..` segments, repeated separators and trailing dots, while
/// `WaitNamedPipeW` compares the name as given.
#[cfg(windows)]
fn windows_canonical_pipe_path(path: &Path) -> io::Result<widestring::U16CString> {
    use windows_sys::Win32::Storage::FileSystem::GetFullPathNameW;

    let name = windows_pipe_path(path)?;
    let mut capacity = name.len() + 1;
    loop {
        let mut buffer = vec![0_u16; capacity];
        let size = u32::try_from(buffer.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "pipe name is too long"))?;
        let written = unsafe {
            GetFullPathNameW(
                name.as_ptr(),
                size,
                buffer.as_mut_ptr(),
                std::ptr::null_mut(),
            )
        } as usize;
        if written == 0 {
            return Err(io::Error::last_os_error());
        }
        if written < buffer.len() {
            buffer.truncate(written);
            return widestring::U16CString::from_vec(buffer)
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err));
        }
        // Too small: `written` is the required size including the terminator.
        capacity = written;
    }
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
    let name = windows_canonical_pipe_path(path)?;
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
