use std::path::{Path, PathBuf};

/// 宿主关机/重启时 POSIX shell 自报的可疑退出码。
///
/// POSIX shell 用 `128 + signal` 表示被信号打断（`129` = SIGHUP、`143` =
/// SIGTERM），而装了 `trap` 处理的 shell（含登录 shell 与多数 agent CLI 包装）
/// 往往直接 `exit 1`。这些退出码与「用户敲了一条失败命令后退出」无法区分，
/// 所以统一按可疑处理：代价只是多做一次会话检查点，收益是主机重启时不会把
/// `session.json` 清空（HSR-04 / 上游 #4320）。
///
/// `128 + signal` 是 unix 专属语义，所以码表留在本文件；Windows 自己维护一份
/// （见 `docs/AGENT_RULES/platform.md`：OS 专属行为只进 `src/platform/<os>.rs`）。
pub(crate) fn exit_code_suspects_host_shutdown(code: u32) -> bool {
    matches!(code, 1 | 129 | 143)
}

pub(crate) struct ChildPipe(std::fs::File);

impl ChildPipe {
    pub(crate) fn new_reader(pipe: impl Into<std::os::fd::OwnedFd>) -> std::io::Result<Self> {
        use std::os::fd::AsRawFd as _;
        let file = std::fs::File::from(pipe.into());
        let fd = file.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self(file))
    }

    pub(crate) fn new_writer(pipe: impl Into<std::os::fd::OwnedFd>) -> std::io::Result<Self> {
        Self::new_reader(pipe)
    }

    pub(crate) fn wait(&self, writing: bool) -> std::io::Result<()> {
        use std::os::fd::AsRawFd as _;
        let mut descriptor = libc::pollfd {
            fd: self.0.as_raw_fd(),
            events: if writing { libc::POLLOUT } else { libc::POLLIN },
            revents: 0,
        };
        if unsafe { libc::poll(&mut descriptor, 1, 50) } < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
        Ok(())
    }
}

impl std::io::Read for ChildPipe {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        std::io::Read::read(&mut self.0, buffer)
    }
}

impl std::io::Write for ChildPipe {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        std::io::Write::write(&mut self.0, buffer)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(crate) fn classify_child_exit(status: &portable_pty::ExitStatus) -> super::ChildExitReason {
    if status.signal().is_some() {
        super::ChildExitReason::Interrupted
    } else if exit_code_suspects_host_shutdown(status.exit_code()) {
        // 捕获 SIGHUP/SIGTERM 后自行退出的 shell 只留下退出码，没有信号。
        super::ChildExitReason::SuspectedInterruption
    } else {
        super::ChildExitReason::Exited
    }
}

/// pane 注入 `SSH_AUTH_SOCK` 前的活性判定（HOST-INT-01）：路径必须是一个存在、
/// 属主为当前 euid 的 unix socket。宿主终端重启后旧 agent socket 会消失（路径悬
/// 空），终端内置的 agent 代理还可能换用新的 `agent.<pid>` 路径，所以每次 spawn 都要
/// 重查，不能缓存结果。
pub(crate) fn ssh_auth_sock_path_is_live(path: &Path) -> bool {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};

    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    metadata.file_type().is_socket() && metadata.uid() == unsafe { libc::geteuid() }
}

pub(crate) fn read_fd(fd: std::os::fd::RawFd, data: &mut [u8]) -> std::io::Result<usize> {
    let result = unsafe { libc::read(fd, data.as_mut_ptr().cast(), data.len()) };
    if result < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(result as usize)
    }
}

/// The tty's erase character (VERASE) on stdin, when one is set.
pub(super) fn terminal_erase_byte() -> Option<u8> {
    let mut termios: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut termios) } != 0 {
        return None;
    }
    let erase = termios.c_cc[libc::VERASE];
    // 0 (Linux) and 0xff (BSD) disable the special character.
    (erase != 0 && erase != 0xff).then_some(erase)
}

pub(crate) fn poll_fd_readable(fd: std::os::fd::RawFd, timeout_ms: i32) -> std::io::Result<bool> {
    let mut descriptor = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let result = unsafe { libc::poll(&mut descriptor, 1, timeout_ms) };
    if result < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(result > 0)
    }
}

pub(crate) type ServerClientStream = crate::ipc::LocalStream;

#[derive(Debug)]
pub(crate) struct ClientStreamControl(ServerClientStream);

impl ClientStreamControl {
    pub(crate) fn shutdown(&self) {
        let _ = shutdown_client_stream(&self.0);
    }

    pub(crate) fn set_deadline(&self, _deadline: std::time::Instant) {}
}

pub(crate) fn prepare_server_client_stream(
    stream: ServerClientStream,
    write_timeout: std::time::Duration,
) -> std::io::Result<ServerClientStream> {
    use interprocess::local_socket::traits::Stream as _;
    stream.set_nonblocking(false)?;
    stream.set_send_timeout(Some(write_timeout))?;
    Ok(stream)
}

pub(crate) fn client_stream_control(
    stream: &ServerClientStream,
) -> std::io::Result<std::sync::Arc<ClientStreamControl>> {
    use interprocess::TryClone as _;
    Ok(std::sync::Arc::new(ClientStreamControl(
        stream.try_clone()?,
    )))
}

pub(crate) fn read_client_handshake(
    stream: &mut ServerClientStream,
    data: &mut [u8],
    deadline: std::time::Instant,
) -> std::io::Result<usize> {
    use interprocess::local_socket::traits::Stream as _;
    use std::io::Read as _;
    let remaining = deadline
        .checked_duration_since(std::time::Instant::now())
        .ok_or(std::io::ErrorKind::TimedOut)?;
    stream.set_recv_timeout(Some(remaining))?;
    stream.read(data)
}

pub(crate) fn finish_client_stream(stream: &mut ServerClientStream) -> std::io::Result<()> {
    shutdown_client_stream(stream)
}

pub(crate) fn shutdown_client_stream(stream: &crate::ipc::LocalStream) -> std::io::Result<()> {
    let crate::ipc::LocalStream::UdSocket(stream) = stream;
    stream.inner().shutdown(std::net::Shutdown::Both)
}

pub(crate) struct ClientStreamReader<'a>(pub(crate) &'a mut crate::ipc::LocalStream);

impl std::io::Read for ClientStreamReader<'_> {
    fn read(&mut self, data: &mut [u8]) -> std::io::Result<usize> {
        use std::os::fd::AsRawFd as _;

        loop {
            match self.0.read(data) {
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    let crate::ipc::LocalStream::UdSocket(stream) = &*self.0;
                    let mut descriptor = libc::pollfd {
                        fd: stream.inner().as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    // Sleep until input or shutdown, without polling quiet observers.
                    if unsafe { libc::poll(&mut descriptor, 1, -1) } < 0 {
                        let error = std::io::Error::last_os_error();
                        if error.kind() != std::io::ErrorKind::Interrupted {
                            return Err(error);
                        }
                    }
                }
                result => return result,
            }
        }
    }
}

/// HSR-12：`MSG_PEEK` 窥探 Unix 流当前可读的字节数，且不消费数据。
///
/// 阻塞语义与普通 `read` 一致（不传 `MSG_DONTWAIT`，因此连接的 `SO_RCVTIMEO`
/// 照常生效）；返回 0 表示对端已关闭写端。handoff 的行式读取据此只在「当前
/// 可读窗口里还没有换行」时才整块消费，避免逐字节 `read(2)` 的十万级系统调用。
pub(crate) fn peek_unix_stream(
    stream: &std::os::unix::net::UnixStream,
    buffer: &mut [u8],
) -> std::io::Result<usize> {
    use std::os::fd::AsRawFd as _;

    let peeked = unsafe {
        libc::recv(
            stream.as_raw_fd(),
            buffer.as_mut_ptr().cast::<libc::c_void>(),
            buffer.len(),
            libc::MSG_PEEK,
        )
    };
    if peeked < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(peeked as usize)
}

pub(crate) fn write_client_stream(
    stream: &crate::ipc::LocalStream,
    mut data: &[u8],
) -> std::io::Result<()> {
    use std::io::{self, Write as _};
    use std::os::fd::AsRawFd as _;
    use std::time::Instant;

    let crate::ipc::LocalStream::UdSocket(socket) = stream;
    let mut socket = socket.inner();
    let Some(timeout) = socket.write_timeout()? else {
        return socket.write_all(data);
    };
    let timed_out = || {
        // Dropping the writer clone alone would leave the reader blocked.
        let _ = shutdown_client_stream(stream);
        io::Error::new(io::ErrorKind::TimedOut, "client stopped receiving output")
    };
    let mut progress = Instant::now();
    while !data.is_empty() {
        match socket.write(data) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(written) => {
                data = &data[written..];
                progress = Instant::now();
                continue;
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(error),
        }
        let remaining = timeout
            .checked_sub(progress.elapsed())
            .ok_or_else(timed_out)?;
        let mut descriptor = libc::pollfd {
            fd: socket.as_raw_fd(),
            events: libc::POLLOUT,
            revents: 0,
        };
        let wait_ms = remaining.as_millis().clamp(1, i32::MAX as u128) as i32;
        let ready = unsafe { libc::poll(&mut descriptor, 1, wait_ms) };
        if ready == 0 {
            return Err(timed_out());
        }
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
    Ok(())
}

pub(crate) fn wait_client_stream_readable(
    stream: &crate::ipc::LocalStream,
    timeout: std::time::Duration,
) -> std::io::Result<()> {
    use std::os::fd::{AsFd as _, AsRawFd as _};
    let crate::ipc::LocalStream::UdSocket(stream) = stream;
    let mut descriptor = libc::pollfd {
        fd: stream.as_fd().as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // Bound cancellation latency without polling idle connections hundreds of times per second.
    let wait_ms = timeout.as_millis().clamp(1, 100) as i32;
    let result = unsafe { libc::poll(&mut descriptor, 1, wait_ms) };
    if result < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
    Ok(())
}

pub(crate) fn forward_remote_bridge_stdio(
    stream: crate::ipc::LocalStream,
    idle_timeout: bool,
) -> std::io::Result<()> {
    forward_remote_bridge_stdio_with_timeout(
        stream,
        idle_timeout.then_some(super::remote_bridge::IDLE_TIMEOUT),
    )
}

pub(super) fn forward_remote_bridge_stdio_with_timeout(
    stream: crate::ipc::LocalStream,
    idle_timeout: Option<std::time::Duration>,
) -> std::io::Result<()> {
    use super::remote_bridge::{Activity, TrackedIo};
    use interprocess::TryClone as _;

    let activity = idle_timeout.map(Activity::start).transpose()?;
    let mut stdout = TrackedIo::new(std::io::stdout().lock(), activity.clone());
    let mut socket_to_stdout = TrackedIo::new(stream.try_clone()?, activity.clone());
    let mut stdin_to_socket = stream;
    let _upload = std::thread::spawn(move || {
        let mut stdin = TrackedIo::new(std::io::stdin(), activity.clone());
        let _ = copy_flush(
            &mut stdin,
            &mut TrackedIo::new(&mut stdin_to_socket, activity),
        );
        let crate::ipc::LocalStream::UdSocket(stream) = stdin_to_socket;
        let _ = stream.inner().shutdown(std::net::Shutdown::Write);
    });
    copy_flush(&mut socket_to_stdout, &mut stdout)
}

fn copy_flush<R: std::io::Read, W: std::io::Write>(
    reader: &mut R,
    writer: &mut W,
) -> std::io::Result<()> {
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let read = match reader.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(read) => read,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        };
        writer.write_all(&buffer[..read])?;
        writer.flush()?;
    }
}

pub(crate) struct RemoteBridgeWake {
    reader: std::os::unix::net::UnixStream,
    writer: std::os::unix::net::UnixStream,
}

impl RemoteBridgeWake {
    pub(crate) fn new() -> std::io::Result<Self> {
        let (reader, writer) = std::os::unix::net::UnixStream::pair()?;
        Ok(Self { reader, writer })
    }

    pub(crate) fn cancel(&self) -> std::io::Result<()> {
        // EOF stays readable, including when cancellation precedes the wait.
        self.writer.shutdown(std::net::Shutdown::Write)
    }

    pub(crate) fn wait(&self, stream: &crate::ipc::LocalStream) -> std::io::Result<()> {
        use std::os::fd::{AsFd as _, AsRawFd as _};
        let crate::ipc::LocalStream::UdSocket(stream) = stream;
        let mut descriptors = [
            libc::pollfd {
                fd: stream.as_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: self.reader.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        loop {
            // SAFETY: both descriptors remain borrowed and the array has two entries.
            if unsafe { libc::poll(descriptors.as_mut_ptr(), 2, -1) } >= 0 {
                return Ok(());
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
}

pub(super) fn read_terminal_grid_size() -> std::io::Result<(u16, u16)> {
    crossterm::terminal::window_size().map(|size| (size.columns, size.rows))
}

fn set_sigpipe_disposition(handler: libc::sighandler_t) {
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = handler;
    unsafe {
        libc::sigemptyset(&mut action.sa_mask);
        // Rust starts with SIGPIPE ignored. If this best-effort transition
        // fails, stdout retains the existing Rust behavior.
        libc::sigaction(libc::SIGPIPE, &action, std::ptr::null_mut());
    }
}

extern "C" fn discard_signal(_signal: libc::c_int) {}

/// The server must outlive the terminal or shell that launched it, like tmux.
/// Installed at process start so SIGHUP cannot stop the server before its event
/// loop runs. A handler, unlike SIG_IGN, resets to the default on exec, so
/// child processes keep normal hangup behavior.
pub(crate) fn ignore_server_hangup() {
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = discard_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
    action.sa_flags = libc::SA_RESTART;
    unsafe {
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(libc::SIGHUP, &action, std::ptr::null_mut());
    }
}

/// Routes SIGINT and SIGTERM to `on_quit`, and logs SIGHUP without stopping.
pub(crate) fn spawn_server_signal_monitor(
    on_quit: impl Fn(super::ServerQuitSignal) + Send + Sync + 'static,
) {
    use tokio::signal::unix::{signal, SignalKind};
    use tracing::{info, warn};

    let on_quit = std::sync::Arc::new(on_quit);
    for (kind, quit_signal) in [
        (SignalKind::interrupt(), super::ServerQuitSignal::Interrupt),
        (SignalKind::terminate(), super::ServerQuitSignal::Terminate),
    ] {
        match signal(kind) {
            Ok(mut stream) => {
                let on_quit = on_quit.clone();
                tokio::spawn(async move {
                    while stream.recv().await.is_some() {
                        on_quit(quit_signal);
                    }
                });
            }
            Err(err) => warn!(%err, signal = %quit_signal, "failed to install server stop handler"),
        }
    }
    match signal(SignalKind::hangup()) {
        Ok(mut stream) => {
            tokio::spawn(async move {
                while stream.recv().await.is_some() {
                    info!("ignoring SIGHUP; server keeps running");
                }
            });
        }
        Err(err) => warn!(%err, "failed to install SIGHUP handler"),
    }
}

/// Describes the process on the other end of a local socket, for logs.
pub(crate) fn local_stream_peer_description(stream: &crate::ipc::LocalStream) -> Option<String> {
    use std::os::fd::{AsFd as _, AsRawFd as _};

    let crate::ipc::LocalStream::UdSocket(socket) = stream;
    let pid = super::socket_peer_pid(socket.as_fd().as_raw_fd())?;
    let Some((name, parent)) = super::process_name_and_parent(pid) else {
        return Some(format!("pid {pid}"));
    };
    let mut description = format!("pid {pid} ({name})");
    if parent > 1 {
        description.push_str(&format!(", parent pid {parent}"));
        if let Some((parent_name, _)) = super::process_name_and_parent(parent) {
            description.push_str(&format!(" ({parent_name})"));
        }
    }
    Some(description)
}

pub(crate) fn begin_cli_output() {
    set_sigpipe_disposition(libc::SIG_DFL);
}

pub(crate) fn end_cli_output() {
    set_sigpipe_disposition(libc::SIG_IGN);
}

pub(crate) fn remote_ssh_config_paths() -> super::RemoteSshConfigPaths {
    super::RemoteSshConfigPaths {
        user_config: super::remote_ssh_user_config_path(),
        system_config: Some(PathBuf::from("/etc/ssh/ssh_config")),
        multiplexing: true,
    }
}

pub(crate) fn default_known_hosts_path() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".ssh").join("known_hosts"))
}

pub(crate) fn create_remote_ssh_config_dir(control_socket_name: &str) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;

    let mut bases = vec![std::env::temp_dir()];
    let short_tmp = PathBuf::from("/tmp");
    if bases.first() != Some(&short_tmp) {
        bases.push(short_tmp);
    }

    let mut last_error = None;
    let mut path_fits = false;
    for base in bases {
        for attempt in 0..100 {
            let dir = base.join(format!("herdr-ssh-{}-{attempt}", std::process::id()));
            if !fits_unix_socket_path(&dir.join(control_socket_name)) {
                continue;
            }
            path_fits = true;
            match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
                Ok(()) => return Ok(dir),
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(err) => {
                    last_error = Some(err);
                    break;
                }
            }
        }
    }

    if let Some(err) = last_error {
        return Err(err);
    }
    let message = if path_fits {
        "failed to create private herdr ssh config directory"
    } else {
        "SSH control socket path exceeds the Unix socket length limit"
    };
    Err(std::io::Error::new(
        if path_fits {
            std::io::ErrorKind::AlreadyExists
        } else {
            std::io::ErrorKind::InvalidInput
        },
        message,
    ))
}

/// 可复用的受管配置目录：同一个 `key` 得到同一个路径，多个 herdr 进程因此共用
/// 一个 ssh ControlPath（ControlMaster 复用，HERDR-MACH-003）。已存在的目录必须
/// 是本用户的私有目录（属主 + 0700，且不是符号链接）才复用；校验不过就退回一次性
/// 唯一目录——宁可放弃复用，也不用别人控制的目录装控制 socket。
pub(crate) fn reusable_remote_ssh_config_dir(
    key: &str,
    control_socket_name: &str,
) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;

    let name = format!(
        "herdr-ssh-{}-{:016x}",
        current_uid(),
        stable_key_digest(key)
    );
    let mut bases = vec![std::env::temp_dir()];
    let short_tmp = PathBuf::from("/tmp");
    if bases.first() != Some(&short_tmp) {
        bases.push(short_tmp);
    }

    for base in bases {
        let dir = base.join(&name);
        if !fits_unix_socket_path(&dir.join(control_socket_name)) {
            continue;
        }
        match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => return Ok(dir),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                if private_owned_dir(&dir) {
                    return Ok(dir);
                }
                break;
            }
            Err(err) => return Err(err),
        }
    }
    create_remote_ssh_config_dir(control_socket_name)
}

/// 目录是否为本用户的私有目录：`symlink_metadata` 不跟随符号链接，避免被别人
/// 用符号链接把控制 socket 引到别处。
fn private_owned_dir(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;

    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return false;
    };
    metadata.is_dir() && metadata.uid() == current_uid() && metadata.mode() & 0o077 == 0
}

fn current_uid() -> u32 {
    // SAFETY: geteuid 只读当前进程的有效用户 id，没有额外前置条件。
    unsafe { libc::geteuid() }
}

/// key 的稳定摘要：目录名要在不同进程、不同 herdr 版本之间保持一致，因此不用
/// 标准库的哈希实现（其算法不保证稳定），改用 FNV-1a（这里只需要防碰撞）。
fn stable_key_digest(key: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in key.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

pub(crate) fn create_remote_ssh_config_file(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

/// 覆写受管 ssh 配置：共享目录里同一档案的配置每次操作都会重写，
/// `create_new` 会 AlreadyExists。
pub(crate) fn write_remote_ssh_config_file(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
}

pub(crate) fn create_remote_private_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;

    std::fs::DirBuilder::new().mode(0o700).create(path)
}

pub(crate) fn remote_private_temp_base() -> PathBuf {
    std::env::temp_dir()
}

pub(crate) fn remote_bridge_endpoint_path(readable_name: &str, short_name: &str) -> PathBuf {
    remote_bridge_endpoint_path_under(&std::env::temp_dir(), readable_name, short_name)
}

/// 临时目录由调用方给出的 `remote_bridge_endpoint_path`：测试用超长的目录验证回退，不必改
/// 进程级的 `TMPDIR`（同一进程里并发的测试会看到）。
pub(crate) fn remote_bridge_endpoint_path_under(
    tmp: &Path,
    readable_name: &str,
    short_name: &str,
) -> PathBuf {
    let readable = tmp.join(readable_name);
    if fits_unix_socket_path(&readable) {
        return readable;
    }
    let short = tmp.join(short_name);
    if fits_unix_socket_path(&short) {
        return short;
    }
    PathBuf::from("/tmp").join(short_name)
}

pub(crate) fn remote_reattach_program(program: &str) -> String {
    shell_quote(if program.is_empty() { "herdr" } else { program })
}

pub(crate) fn remote_reattach_argument(value: &str) -> String {
    shell_quote(value)
}

fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value.chars().all(|ch| {
            ch.is_ascii_alphanumeric()
                || matches!(
                    ch,
                    '@' | '%' | '_' | '+' | '=' | ':' | ',' | '.' | '/' | '-'
                )
        })
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn fits_unix_socket_path(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;

    path.as_os_str().as_bytes().len() <= 103
}

/// The machine's node name, as shown by tmux's `#h`.
pub(crate) fn hostname() -> Option<String> {
    let mut buffer = [0_u8; 256];
    let result =
        unsafe { libc::gethostname(buffer.as_mut_ptr().cast::<libc::c_char>(), buffer.len()) };
    if result != 0 {
        return None;
    }
    let end = buffer
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(buffer.len());
    let name = String::from_utf8_lossy(&buffer[..end]).into_owned();
    (!name.is_empty()).then_some(name)
}

pub(crate) fn local_datetime() -> Option<time::PrimitiveDateTime> {
    let mut timestamp: libc::time_t = 0;
    if unsafe { libc::time(&mut timestamp) } == -1 {
        return None;
    }
    let mut local: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&timestamp, &mut local) }.is_null() {
        return None;
    }
    datetime_from_tm(&local)
}

pub(crate) fn status_commands_supported() -> bool {
    true
}

pub(crate) fn configure_status_command(process: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;

    process.process_group(0);
}

pub(crate) struct StatusCommandGuard {
    process_group_id: Option<i32>,
}

impl StatusCommandGuard {
    pub(crate) fn from_std_child(child: &std::process::Child) -> std::io::Result<Self> {
        let process_group_id =
            i32::try_from(child.id()).map_err(|_| std::io::Error::other("任务进程 ID 超出范围"))?;
        Ok(Self {
            process_group_id: Some(process_group_id),
        })
    }

    pub(crate) fn new(child: &tokio::process::Child) -> std::io::Result<Self> {
        let process_id = child
            .id()
            .ok_or_else(|| std::io::Error::other("status command has no process id"))?;
        let process_group_id = i32::try_from(process_id)
            .map_err(|_| std::io::Error::other("status command process id exceeds i32"))?;
        Ok(Self {
            process_group_id: Some(process_group_id),
        })
    }
}

impl StatusCommandGuard {
    pub(crate) fn terminate(&mut self) {
        if let Some(process_group_id) = self.process_group_id.take() {
            // The command was spawned as this process group's leader. Killing the
            // group also cleans up background descendants on completion/cancellation.
            unsafe {
                libc::kill(-process_group_id, libc::SIGKILL);
            }
        }
    }
}

impl Drop for StatusCommandGuard {
    fn drop(&mut self) {
        self.terminate();
    }
}

fn datetime_from_tm(value: &libc::tm) -> Option<time::PrimitiveDateTime> {
    let month = time::Month::try_from(u8::try_from(value.tm_mon + 1).ok()?).ok()?;
    let date = time::Date::from_calendar_date(
        value.tm_year + 1900,
        month,
        u8::try_from(value.tm_mday).ok()?,
    )
    .ok()?;
    let time = time::Time::from_hms(
        u8::try_from(value.tm_hour).ok()?,
        u8::try_from(value.tm_min).ok()?,
        u8::try_from(value.tm_sec).ok()?,
    )
    .ok()?;
    Some(time::PrimitiveDateTime::new(date, time))
}

pub(crate) fn set_default_plugin_pane_pwd(env: &mut Vec<(String, String)>, cwd: &std::path::Path) {
    if !env.iter().any(|(key, _)| key == "PWD") {
        env.push(("PWD".to_string(), cwd.display().to_string()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_signal_monitor_survives_hangup_and_reports_terminate() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            spawn_server_signal_monitor(move |signal| {
                let _ = tx.send(signal);
            });

            // An unhandled SIGHUP would end the test process here.
            assert_eq!(unsafe { libc::kill(libc::getpid(), libc::SIGHUP) }, 0);
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            assert!(rx.try_recv().is_err());

            assert_eq!(unsafe { libc::kill(libc::getpid(), libc::SIGTERM) }, 0);
            let signal = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
                .await
                .unwrap();
            assert_eq!(signal, Some(super::super::ServerQuitSignal::Terminate));
        });
    }

    #[test]
    fn plugin_pane_pwd_defaults_to_cwd_without_overriding_explicit_env() {
        let cwd = Path::new("/plugin-cwd");
        let mut derived = vec![("OTHER".to_string(), "value".to_string())];
        set_default_plugin_pane_pwd(&mut derived, cwd);
        assert!(derived.contains(&("PWD".to_string(), "/plugin-cwd".to_string())));

        let mut explicit = vec![("PWD".to_string(), "/caller-pwd".to_string())];
        set_default_plugin_pane_pwd(&mut explicit, cwd);
        assert_eq!(explicit, [("PWD".to_string(), "/caller-pwd".to_string())]);
    }

    #[test]
    fn remote_ssh_config_dir_rejects_overlong_control_socket_name() {
        let err = create_remote_ssh_config_dir(&"x".repeat(200)).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    /// 目录名要在进程之间稳定（下一个 herdr 进程靠它接到同一个 ssh master），
    /// 所以摘要算法一旦变化就会让既有 master 失去复用；这里钉住算法与取值。
    #[test]
    fn reusable_dir_key_digest_is_stable() {
        assert_eq!(stable_key_digest("herdr"), 0xe4e1_6546_1418_32fe);
    }

    /// 测试用唯一 key：pid 区分进程，`test_dirs::unique_id` 区分 `cargo test` 同一进程里的测试。
    fn unique_test_key(label: &str) -> String {
        format!(
            "{label}-{}-{}",
            std::process::id(),
            crate::config::test_dirs::unique_id()
        )
    }

    #[test]
    fn reusable_remote_ssh_config_dir_reuses_one_private_dir_per_key() {
        use std::os::unix::fs::MetadataExt;

        // 目录建在 `$TMPDIR` 下：持环境锁，两次解析之间不会有测试改掉 TMPDIR。
        let _env = crate::config::test_config_env_lock().lock().unwrap();
        let key = unique_test_key("test-reuse");
        let first = reusable_remote_ssh_config_dir(&key, "ctl").expect("first");
        let second = reusable_remote_ssh_config_dir(&key, "ctl").expect("second");
        let other =
            reusable_remote_ssh_config_dir(&format!("{key}-other"), "ctl").expect("other key");

        assert_eq!(first, second, "同 key 复用同一个目录");
        assert_ne!(first, other, "不同 key 不共享目录");
        assert_eq!(
            std::fs::metadata(&first).expect("metadata").mode() & 0o077,
            0,
            "复用目录必须是 0700"
        );

        let _ = std::fs::remove_dir_all(&first);
        let _ = std::fs::remove_dir_all(&other);
    }

    #[test]
    fn reusable_remote_ssh_config_dir_falls_back_for_non_private_dir() {
        use std::os::unix::fs::PermissionsExt;

        let _env = crate::config::test_config_env_lock().lock().unwrap();
        let key = unique_test_key("test-fallback");
        let existing = reusable_remote_ssh_config_dir(&key, "ctl").expect("first");
        std::fs::set_permissions(&existing, std::fs::Permissions::from_mode(0o777))
            .expect("loosen permissions");

        let fallback = reusable_remote_ssh_config_dir(&key, "ctl").expect("fallback");
        assert_ne!(fallback, existing, "非私有目录不复用，退回唯一目录");
        assert!(fallback.is_dir(), "退回目录仍然可用");

        let _ = std::fs::remove_dir_all(&existing);
        let _ = std::fs::remove_dir_all(&fallback);
    }
}

/// 已移除的用量回调在 POSIX 平台（Linux 与 unix 回退平台）写下的 statusline 包装：首行是标记
/// 注释，第二行是回调子 shell；带原渲染命令时以 ` | (` 收尾，其后是渲染命令，最后一行是 `)`。
/// 返回原渲染命令（可能为空串）；`Ok(None)` 表示命令不是 herdr 包装（自定义渲染器、用户手写的
/// `herdr api usage-report …` 或空）。识别顺序：标记形态 → 标记之前的旧模板（按整串比对）→
/// 带 herdr 包装特征却都不匹配（其它平台 / 版本 / 厂商的形态、手工改过的包装）时报可识别错误，
/// 调用方据此原样保留。
pub(crate) fn unwrap_legacy_statusline(
    agent: &str,
    command: &str,
) -> std::io::Result<Option<String>> {
    if let Some(body) = command
        .strip_prefix(super::LEGACY_STATUSLINE_MARKER)
        .and_then(|rest| rest.strip_prefix('\n'))
    {
        return unwrap_marked_legacy_statusline(agent, body).map(Some);
    }
    if command == unmarked_legacy_callback_command(agent, false) {
        return Ok(Some(String::new()));
    }
    let legacy_pipe = format!(
        "{}{STATUSLINE_PIPE_OPEN}",
        unmarked_legacy_callback_command(agent, true)
    );
    if let Some(original) = command
        .strip_prefix(legacy_pipe.as_str())
        .and_then(|rest| rest.strip_suffix(STATUSLINE_PIPE_CLOSE))
    {
        return Ok(Some(original.to_owned()));
    }
    if super::looks_like_legacy_statusline_wrapper(command) {
        return Err(super::unrecognized_legacy_statusline_error());
    }
    Ok(None)
}

const STATUSLINE_PIPE_OPEN: &str = " | (\n";
const STATUSLINE_PIPE_CLOSE: &str = "\n)";

/// 标记之后的正文：首行是回调子 shell；带渲染命令时首行以 ` | (` 收尾、其余是渲染命令、
/// 最后一行是 `)`。首行里的厂商名必须与 `agent` 一致，否则是别的厂商的回调。
fn unwrap_marked_legacy_statusline(agent: &str, body: &str) -> std::io::Result<String> {
    let (callback, original) = match body.split_once('\n') {
        Some((first, rest)) => {
            let callback = first
                .strip_suffix(STATUSLINE_PIPE_OPEN.trim_end_matches('\n'))
                .ok_or_else(super::unrecognized_legacy_statusline_error)?;
            let original = rest
                .strip_suffix(STATUSLINE_PIPE_CLOSE)
                .ok_or_else(super::unrecognized_legacy_statusline_error)?;
            (callback, original)
        }
        None => (body, ""),
    };
    if super::legacy_statusline_agent(callback) != Some(agent) {
        return Err(super::unrecognized_legacy_statusline_error());
    }
    Ok(original.to_owned())
}

/// 标记之前的旧模板（不带标记、不 `exec`，逐字保留）：旧 settings 按整串比对识别。
fn unmarked_legacy_callback_command(agent: &str, passthrough: bool) -> String {
    let args = if passthrough { " --passthrough" } else { "" };
    let otherwise = if passthrough { "cat" } else { ":" };
    format!("(if [ \"${{HERDR_ENV:-}}\" = 1 ] && [ -n \"${{HERDR_BIN_PATH:-}}\" ]; then \"$HERDR_BIN_PATH\" api usage-report --agent {agent}{args}; else {otherwise}; fi)")
}

/// 旧版 herdr 的带标记包装（逐字复现当时的生成逻辑）：只给测试构造任意渲染命令的输入，
/// 生产代码不再生成包装。
#[cfg(test)]
pub(crate) fn marked_legacy_statusline_wrapper(agent: &str, original: &str) -> String {
    let marker = super::LEGACY_STATUSLINE_MARKER;
    if original.is_empty() {
        format!("{marker}\n(if [ \"${{HERDR_ENV:-}}\" = 1 ] && [ -n \"${{HERDR_BIN_PATH:-}}\" ]; then exec \"$HERDR_BIN_PATH\" api usage-report --agent {agent} --passthrough >/dev/null; else :; fi)")
    } else {
        format!("{marker}\n(if [ \"${{HERDR_ENV:-}}\" = 1 ] && [ -n \"${{HERDR_BIN_PATH:-}}\" ]; then exec \"$HERDR_BIN_PATH\" api usage-report --agent {agent} --passthrough; else exec cat; fi){STATUSLINE_PIPE_OPEN}{original}{STATUSLINE_PIPE_CLOSE}")
    }
}

fn validate_shared_ssh_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::symlink_metadata(dir)?;
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o7777 != 0o700
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "SSH control directory must be owned by the current user, mode 0700, and not a symlink",
        ));
    }
    Ok(())
}

pub(crate) struct PinnedSshControlSocket {
    pub(crate) alias: PathBuf,
    original: PathBuf,
    dev: u64,
    ino: u64,
}

impl PinnedSshControlSocket {
    pub(crate) fn new(path: &Path) -> std::io::Result<Self> {
        use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt};
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT_PIN: AtomicU64 = AtomicU64::new(0);

        let parent = path.parent().ok_or(std::io::ErrorKind::InvalidInput)?;
        validate_shared_ssh_dir(parent)?;
        let metadata = std::fs::symlink_metadata(path)?;
        if !metadata.file_type().is_socket() || metadata.uid() != unsafe { libc::geteuid() } {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "SSH control path must be a socket owned by the current user",
            ));
        }
        let directory = parent.join(format!(
            "r-{}-{}",
            std::process::id(),
            NEXT_PIN.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::DirBuilder::new().mode(0o700).create(&directory)?;
        let pinned = Self {
            alias: directory.join("ctl"),
            original: path.to_owned(),
            dev: metadata.dev(),
            ino: metadata.ino(),
        };
        // A separate name pins the old inode; a replacement cannot receive its stop request.
        std::fs::hard_link(path, &pinned.alias)?;
        let linked = std::fs::symlink_metadata(&pinned.alias)?;
        if (linked.dev(), linked.ino()) != (pinned.dev, pinned.ino) {
            return Err(std::io::ErrorKind::NotFound.into());
        }
        Ok(pinned)
    }

    pub(crate) fn is_current(&self) -> std::io::Result<bool> {
        use std::os::unix::fs::MetadataExt;
        match std::fs::symlink_metadata(&self.original) {
            Ok(metadata) => Ok((metadata.dev(), metadata.ino()) == (self.dev, self.ino)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }
}

impl Drop for PinnedSshControlSocket {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.alias);
        if let Some(directory) = self.alias.parent() {
            let _ = std::fs::remove_dir(directory);
        }
    }
}

#[cfg(test)]
mod legacy_statusline_tests {
    use super::*;

    /// 旧二进制写进用户 settings.json 的带标记形态（逐字钉住）：独立形态与带渲染命令的管道形态。
    const MARKED_STANDALONE: &str = "# herdr-usage v1\n(if [ \"${HERDR_ENV:-}\" = 1 ] && [ -n \"${HERDR_BIN_PATH:-}\" ]; then exec \"$HERDR_BIN_PATH\" api usage-report --agent claude --passthrough >/dev/null; else :; fi)";
    const MARKED_PIPELINE: &str = "# herdr-usage v1\n(if [ \"${HERDR_ENV:-}\" = 1 ] && [ -n \"${HERDR_BIN_PATH:-}\" ]; then exec \"$HERDR_BIN_PATH\" api usage-report --agent claude --passthrough; else exec cat; fi) | (\nbash ~/.claude/statusline.sh\n)";
    /// 标记之前的旧模板（逐字保留），新二进制必须仍能识别与剥离。
    const LEGACY_PIPELINE: &str = "(if [ \"${HERDR_ENV:-}\" = 1 ] && [ -n \"${HERDR_BIN_PATH:-}\" ]; then \"$HERDR_BIN_PATH\" api usage-report --agent claude --passthrough; else cat; fi) | (\nbash /home/xyz/.claude/statusline-command.sh\n)";
    const LEGACY_STANDALONE: &str = "(if [ \"${HERDR_ENV:-}\" = 1 ] && [ -n \"${HERDR_BIN_PATH:-}\" ]; then \"$HERDR_BIN_PATH\" api usage-report --agent claude; else :; fi)";

    #[test]
    fn pinned_marked_wrappers_unwrap_to_the_original_renderer() {
        assert_eq!(
            marked_legacy_statusline_wrapper("claude", ""),
            MARKED_STANDALONE
        );
        assert_eq!(
            marked_legacy_statusline_wrapper("claude", "bash ~/.claude/statusline.sh"),
            MARKED_PIPELINE
        );
        assert_eq!(
            unwrap_legacy_statusline("claude", MARKED_STANDALONE)
                .unwrap()
                .as_deref(),
            Some("")
        );
        assert_eq!(
            unwrap_legacy_statusline("claude", MARKED_PIPELINE)
                .unwrap()
                .as_deref(),
            Some("bash ~/.claude/statusline.sh")
        );
    }

    #[test]
    fn marked_wrappers_round_trip_any_renderer() {
        for original in ["", "bash ~/.claude/statusline.sh", "a; b\nc | d\n)\n(e)"] {
            let wrapped = marked_legacy_statusline_wrapper("claude", original);
            assert_eq!(
                unwrap_legacy_statusline("claude", &wrapped)
                    .unwrap()
                    .as_deref(),
                Some(original),
                "{original:?}"
            );
        }
        let wrapped = marked_legacy_statusline_wrapper("antigravity", "python custom.py");
        assert_eq!(
            unwrap_legacy_statusline("antigravity", &wrapped)
                .unwrap()
                .as_deref(),
            Some("python custom.py")
        );
        assert_eq!(
            unwrap_legacy_statusline("claude", &wrapped)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::InvalidData,
            "别的厂商的回调是可识别错误，不能当成自定义渲染器"
        );
    }

    #[test]
    fn unmarked_legacy_templates_still_unwrap() {
        assert_eq!(
            unwrap_legacy_statusline("claude", LEGACY_PIPELINE)
                .unwrap()
                .as_deref(),
            Some("bash /home/xyz/.claude/statusline-command.sh")
        );
        assert_eq!(
            unwrap_legacy_statusline("claude", LEGACY_STANDALONE)
                .unwrap()
                .as_deref(),
            Some("")
        );
        assert_eq!(
            unwrap_legacy_statusline("antigravity", LEGACY_PIPELINE)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::InvalidData,
            "旧模板按整串比对，厂商不同不算本厂商的包装"
        );
    }

    #[test]
    fn unrecognized_wrappers_are_errors_and_custom_commands_are_left_alone() {
        assert_eq!(unwrap_legacy_statusline("claude", "").unwrap(), None);
        assert_eq!(
            unwrap_legacy_statusline("claude", "python custom.py").unwrap(),
            None
        );
        // 用户手写的命令没有 herdr 包装特征：按自定义渲染器处理。
        for hand_written in [
            "herdr api usage-report --agent claude",
            "tee >(herdr api usage-report --agent claude) | bash status.sh",
        ] {
            assert_eq!(
                unwrap_legacy_statusline("claude", hand_written).unwrap(),
                None,
                "{hand_written:?}"
            );
        }
        // 从 Windows 同步来的 herdr 包装、手工改过的 POSIX 包装：带特征却剥不下来，可识别错误。
        let windows = "powershell.exe -NoLogo -NoProfile -NonInteractive -EncodedCommand AAAA";
        assert_eq!(
            unwrap_legacy_statusline("claude", windows)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::InvalidData
        );
        let edited = "(if [ \"${HERDR_ENV:-}\" = 1 ]; then \"$HERDR_BIN_PATH\" api usage-report --agent claude --passthrough; else cat; fi) | (\nbash x.sh\n)";
        assert_eq!(
            unwrap_legacy_statusline("claude", edited)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::InvalidData
        );
        // 标记在但正文被手工改坏：同样是可识别错误，而不是静默按自定义命令处理。
        let broken = "# herdr-usage v1\n(if true; then exec herdr api usage-report --agent claude --passthrough; fi) | (\nbash x.sh";
        assert!(unwrap_legacy_statusline("claude", broken).is_err());
    }
}

#[cfg(test)]
mod pinned_socket_tests {
    use super::*;
    #[test]
    fn pinned_ssh_socket_connects_old_listener_after_original_is_replaced() {
        use std::io::{Read as _, Write as _};
        use std::os::unix::net::{UnixListener, UnixStream};

        let directory = create_remote_ssh_config_dir("ctl").unwrap();
        let path = directory.join("ctl");
        let old = UnixListener::bind(&path).unwrap();
        let pinned = PinnedSshControlSocket::new(&path).unwrap();
        assert!(pinned.is_current().unwrap());
        std::fs::remove_file(&path).unwrap();
        let replacement = UnixListener::bind(&path).unwrap();
        assert!(!pinned.is_current().unwrap());
        let mut client = UnixStream::connect(&pinned.alias).unwrap();
        let (mut server, _) = old.accept().unwrap();
        server.write_all(b"old").unwrap();
        let mut reply = [0; 3];
        client.read_exact(&mut reply).unwrap();
        assert_eq!(&reply, b"old");
        let alias = pinned.alias.clone();
        drop(pinned);
        assert!(!alias.exists());
        let _client = UnixStream::connect(&path).unwrap();
        replacement.accept().unwrap();
        drop((old, replacement));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn ssh_socket_pin_rejects_files_symlinks_and_public_parent() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        use std::os::unix::net::UnixListener;

        let directory = create_remote_ssh_config_dir("ctl").unwrap();
        let file = directory.join("file");
        std::fs::write(&file, b"not a socket").unwrap();
        assert!(PinnedSshControlSocket::new(&file).is_err());
        let path = directory.join("ctl");
        let _listener = UnixListener::bind(&path).unwrap();
        let link = directory.join("link");
        symlink(&path, &link).unwrap();
        assert!(PinnedSshControlSocket::new(&link).is_err());
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(PinnedSshControlSocket::new(&path).is_err());
        std::fs::remove_dir_all(directory).unwrap();
    }
}

#[cfg(test)]
mod shared_socket_tests {
    use super::*;
    #[test]
    fn shared_ssh_directory_rejects_symlinks_and_public_modes() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let dir = create_remote_ssh_config_dir("ctl").unwrap();
        let link = dir.join("link");
        symlink(&dir, &link).unwrap();
        assert_eq!(
            validate_shared_ssh_dir(&link).unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            validate_shared_ssh_dir(&dir).unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
