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
        use interprocess::local_socket::{prelude::*, GenericNamespaced, ListenerOptions};

        let name = path.to_string_lossy().to_string();
        let name = name.to_ns_name::<GenericNamespaced>()?;
        let listener = ListenerOptions::new()
            .name(name)
            .reclaim_name(false)
            .create_sync()?;
        fs::write(path, windows_socket_marker())?;
        Ok(listener)
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
        use interprocess::local_socket::{prelude::*, GenericNamespaced, ListenerOptions};
        use interprocess::os::windows::local_socket::ListenerOptionsExt as _;
        use interprocess::os::windows::security_descriptor::SecurityDescriptor;
        use widestring::U16CString;

        let sddl = U16CString::from_str("D:P(A;;GA;;;SY)(A;;GA;;;OW)")
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;
        let security_descriptor = SecurityDescriptor::deserialize(&sddl)?;
        let name = path.to_string_lossy().to_string();
        let name = name.to_ns_name::<GenericNamespaced>()?;
        let listener = ListenerOptions::new()
            .name(name)
            .reclaim_name(false)
            .security_descriptor(security_descriptor)
            .create_sync()?;
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
