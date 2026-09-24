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

/// pane 注入 `SSH_AUTH_SOCK` 前的活性判定（WEZ-INT-01）：路径必须是一个存在、
/// 属主为当前 euid 的 unix socket。宿主终端重启后旧 agent socket 会消失（路径悬
/// 空），wezterm 这类代理还会新建 `agent.<pid>` 路径，所以每次 spawn 都要重查，
/// 不能缓存结果。
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

pub(crate) fn wait_client_stream_readable(stream: &crate::ipc::LocalStream) -> std::io::Result<()> {
    use std::os::fd::{AsFd as _, AsRawFd as _};
    let crate::ipc::LocalStream::UdSocket(stream) = stream;
    let mut descriptor = libc::pollfd {
        fd: stream.as_fd().as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // Bound cancellation latency without polling idle connections hundreds of times per second.
    let result = unsafe { libc::poll(&mut descriptor, 1, 100) };
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

pub(crate) fn begin_cli_output() {
    set_sigpipe_disposition(libc::SIG_DFL);
}

pub(crate) fn end_cli_output() {
    set_sigpipe_disposition(libc::SIG_IGN);
}

/// 让出 stdout：把 fd 1 换成 `/dev/null`。statusline 回调回放完 stdin 后调用——本进程是管道
/// 写端的最后持有者（包装串已 `exec` 掉子 shell），换掉 fd 1 后原渲染器立刻读到 EOF，本进程
/// 再等上报也不会拖住它。之后经 `std::io::stdout()` 的写入落到 `/dev/null`。
pub(crate) fn detach_stdout() -> std::io::Result<()> {
    use std::os::fd::AsRawFd as _;
    let null = std::fs::OpenOptions::new().write(true).open("/dev/null")?;
    if unsafe { libc::dup2(null.as_raw_fd(), libc::STDOUT_FILENO) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

pub(crate) fn remote_ssh_config_paths() -> super::RemoteSshConfigPaths {
    super::RemoteSshConfigPaths {
        user_config: std::env::var_os("HOME")
            .map(PathBuf::from)
            .map(|home| home.join(".ssh").join("config")),
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
    let tmp = std::env::temp_dir();
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

    #[test]
    fn reusable_remote_ssh_config_dir_reuses_one_private_dir_per_key() {
        use std::os::unix::fs::MetadataExt;

        let key = format!("test-reuse-{}", std::process::id());
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

        let key = format!("test-fallback-{}", std::process::id());
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

/// POSIX sh 形态的 statusline 包装（Linux 与 unix 回退平台共用）：首行是标记注释；第二行
/// 的子 shell 在 herdr 环境里 `exec` 成 `herdr api usage-report`，环境外 `exec cat` 原样
/// 透传——`exec` 让子 shell 不再持有管道写端，回调进程回放完就能让出它（`detach_stdout`），
/// 原渲染命令立刻读到 EOF；`original` 非空时接在管道末端并用子 shell 分组，使多条命令的
/// 渲染脚本整体读到同一份 stdin。`original` 为空时只留回调本身：同样走 `--passthrough`
/// （热路径、fire-and-forget、静默），回放丢到 `/dev/null`，statusline 不显示任何内容。
pub(crate) fn usage_statusline_pipeline(agent: &str, original: &str) -> std::io::Result<String> {
    let marker = super::USAGE_STATUSLINE_MARKER;
    if original.is_empty() {
        Ok(format!(
            "{marker}\n{}",
            usage_callback_command(agent, false)
        ))
    } else {
        Ok(format!(
            "{marker}\n{}{STATUSLINE_PIPE_OPEN}{original}{STATUSLINE_PIPE_CLOSE}",
            usage_callback_command(agent, true)
        ))
    }
}

/// 从 statusline 命令里剥离 herdr 用量回调，返回原渲染命令（可能为空串）；`Ok(None)` 表示
/// 命令不是 herdr 包装（自定义渲染器、用户手写的 `herdr api usage-report …` 或空）。识别
/// 顺序：标记形态 → 标记之前的旧模板（按整串比对）→ 带 herdr 包装特征却都不匹配（其它
/// 平台 / 版本 / 厂商的形态、手工改过的包装）时报可识别错误。
pub(crate) fn strip_usage_statusline(
    agent: &str,
    command: &str,
) -> std::io::Result<Option<String>> {
    if let Some(body) = command
        .strip_prefix(super::USAGE_STATUSLINE_MARKER)
        .and_then(|rest| rest.strip_prefix('\n'))
    {
        return strip_marked_usage_statusline(agent, body).map(Some);
    }
    if command == legacy_usage_callback_command(agent, false) {
        return Ok(Some(String::new()));
    }
    let legacy_pipe = format!(
        "{}{STATUSLINE_PIPE_OPEN}",
        legacy_usage_callback_command(agent, true)
    );
    if let Some(original) = command
        .strip_prefix(legacy_pipe.as_str())
        .and_then(|rest| rest.strip_suffix(STATUSLINE_PIPE_CLOSE))
    {
        return Ok(Some(original.to_owned()));
    }
    if super::looks_like_usage_wrapper(command) {
        return Err(super::unrecognized_usage_statusline_error());
    }
    Ok(None)
}

const STATUSLINE_PIPE_OPEN: &str = " | (\n";
const STATUSLINE_PIPE_CLOSE: &str = "\n)";

/// 标记之后的正文：首行是回调子 shell；带渲染命令时首行以 ` | (` 收尾、其余是渲染命令、
/// 最后一行是 `)`。首行里的厂商名必须与 `agent` 一致，否则是别的厂商的回调。
fn strip_marked_usage_statusline(agent: &str, body: &str) -> std::io::Result<String> {
    let (callback, original) = match body.split_once('\n') {
        Some((first, rest)) => {
            let callback = first
                .strip_suffix(STATUSLINE_PIPE_OPEN.trim_end_matches('\n'))
                .ok_or_else(super::unrecognized_usage_statusline_error)?;
            let original = rest
                .strip_suffix(STATUSLINE_PIPE_CLOSE)
                .ok_or_else(super::unrecognized_usage_statusline_error)?;
            (callback, original)
        }
        None => (body, ""),
    };
    if super::usage_statusline_agent(callback) != Some(agent) {
        return Err(super::unrecognized_usage_statusline_error());
    }
    Ok(original.to_owned())
}

/// 回调子 shell：`piped` 为 true 时 herdr 回放 stdin 给管道末端的渲染命令、herdr 之外
/// `exec cat` 直通；为 false（独立形态）时回放丢弃、herdr 之外什么都不做。
fn usage_callback_command(agent: &str, piped: bool) -> String {
    let (redirect, otherwise) = if piped {
        ("", "exec cat")
    } else {
        (" >/dev/null", ":")
    };
    format!("(if [ \"${{HERDR_ENV:-}}\" = 1 ] && [ -n \"${{HERDR_BIN_PATH:-}}\" ]; then exec \"$HERDR_BIN_PATH\" api usage-report --agent {agent} --passthrough{redirect}; else {otherwise}; fi)")
}

/// 标记之前的旧模板（不带标记、不 `exec`）：只用于识别与解除既有 settings，不再生成。
fn legacy_usage_callback_command(agent: &str, passthrough: bool) -> String {
    let args = if passthrough { " --passthrough" } else { "" };
    let otherwise = if passthrough { "cat" } else { ":" };
    format!("(if [ \"${{HERDR_ENV:-}}\" = 1 ] && [ -n \"${{HERDR_BIN_PATH:-}}\" ]; then \"$HERDR_BIN_PATH\" api usage-report --agent {agent}{args}; else {otherwise}; fi)")
}

#[cfg(test)]
mod usage_statusline_tests {
    use super::*;

    /// 旧二进制写进用户 settings.json 的模板（逐字保留），新二进制必须仍能识别与解除。
    const LEGACY_PIPELINE: &str = "(if [ \"${HERDR_ENV:-}\" = 1 ] && [ -n \"${HERDR_BIN_PATH:-}\" ]; then \"$HERDR_BIN_PATH\" api usage-report --agent claude --passthrough; else cat; fi) | (\nbash /home/xyz/.claude/statusline-command.sh\n)";
    const LEGACY_STANDALONE: &str = "(if [ \"${HERDR_ENV:-}\" = 1 ] && [ -n \"${HERDR_BIN_PATH:-}\" ]; then \"$HERDR_BIN_PATH\" api usage-report --agent claude; else :; fi)";

    /// 写进用户 settings.json 的形态逐字钉住（`tests/cli/usage_report.rs` 用同一字面量交给
    /// 真实 bash 验证 EOF 时机与透传）：两处形态必须一起改。
    #[test]
    fn usage_statusline_pipeline_embeds_marker_and_execs_the_callback() {
        let standalone = usage_statusline_pipeline("claude", "").unwrap();
        assert_eq!(
            standalone,
            "# herdr-usage v1\n(if [ \"${HERDR_ENV:-}\" = 1 ] && [ -n \"${HERDR_BIN_PATH:-}\" ]; then exec \"$HERDR_BIN_PATH\" api usage-report --agent claude --passthrough >/dev/null; else :; fi)",
            "独立形态也走 --passthrough（热路径 + fire-and-forget + 静默），回放丢弃"
        );

        let piped = usage_statusline_pipeline("claude", "bash ~/.claude/statusline.sh").unwrap();
        assert_eq!(
            piped,
            "# herdr-usage v1\n(if [ \"${HERDR_ENV:-}\" = 1 ] && [ -n \"${HERDR_BIN_PATH:-}\" ]; then exec \"$HERDR_BIN_PATH\" api usage-report --agent claude --passthrough; else exec cat; fi) | (\nbash ~/.claude/statusline.sh\n)"
        );
        assert_eq!(
            piped.lines().count(),
            4,
            "标记行、回调行、渲染命令行、收尾括号行"
        );
    }

    #[test]
    fn strip_usage_statusline_round_trips_marked_pipelines() {
        for original in ["", "bash ~/.claude/statusline.sh", "a; b\nc | d\n)\n(e)"] {
            let wrapped = usage_statusline_pipeline("claude", original).unwrap();
            assert_eq!(
                strip_usage_statusline("claude", &wrapped)
                    .unwrap()
                    .as_deref(),
                Some(original),
                "{original:?}"
            );
        }
        let wrapped = usage_statusline_pipeline("antigravity", "python custom.py").unwrap();
        assert_eq!(
            strip_usage_statusline("antigravity", &wrapped)
                .unwrap()
                .as_deref(),
            Some("python custom.py")
        );
        assert_eq!(
            strip_usage_statusline("claude", &wrapped)
                .unwrap_err()
                .to_string(),
            crate::platform::unrecognized_usage_statusline_message(),
            "别的厂商的回调是可识别错误，不能当成自定义渲染器再包一层"
        );
    }

    #[test]
    fn strip_usage_statusline_still_recognizes_legacy_templates() {
        assert_eq!(
            strip_usage_statusline("claude", LEGACY_PIPELINE)
                .unwrap()
                .as_deref(),
            Some("bash /home/xyz/.claude/statusline-command.sh")
        );
        assert_eq!(
            strip_usage_statusline("claude", LEGACY_STANDALONE)
                .unwrap()
                .as_deref(),
            Some("")
        );
        assert_eq!(
            strip_usage_statusline("antigravity", LEGACY_PIPELINE)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::InvalidData,
            "旧模板按整串比对，厂商不同不算本厂商的包装"
        );
    }

    #[test]
    fn strip_usage_statusline_reports_unrecognized_callbacks_and_ignores_custom_commands() {
        assert_eq!(strip_usage_statusline("claude", "").unwrap(), None);
        assert_eq!(
            strip_usage_statusline("claude", "python custom.py").unwrap(),
            None
        );
        // 文档推荐的手写集成没有 herdr 包装特征：按自定义渲染器处理，启用 / 解除都不报错。
        for hand_written in [
            "herdr api usage-report --agent claude",
            "tee >(herdr api usage-report --agent claude) | bash status.sh",
        ] {
            assert_eq!(
                strip_usage_statusline("claude", hand_written).unwrap(),
                None,
                "{hand_written:?}"
            );
        }
        // 从 Windows 同步来的 herdr 包装、手工改过的 POSIX 包装：带特征却剥不下来，可识别错误。
        let windows = "powershell.exe -NoLogo -NoProfile -NonInteractive -EncodedCommand AAAA";
        assert_eq!(
            strip_usage_statusline("claude", windows)
                .unwrap_err()
                .to_string(),
            crate::platform::unrecognized_usage_statusline_message()
        );
        let edited = "(if [ \"${HERDR_ENV:-}\" = 1 ]; then \"$HERDR_BIN_PATH\" api usage-report --agent claude --passthrough; else cat; fi) | (\nbash x.sh\n)";
        assert_eq!(
            strip_usage_statusline("claude", edited).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
        // 标记在但正文被手工改坏：同样是可识别错误，而不是静默按自定义命令处理。
        let broken = "# herdr-usage v1\n(if true; then exec herdr api usage-report --agent claude --passthrough; fi) | (\nbash x.sh";
        assert!(strip_usage_statusline("claude", broken).is_err());
    }
}
