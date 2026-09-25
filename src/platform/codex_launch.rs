//! Native shim creation, foreground process replacement and probe supervision.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};

const MARKER: &str = ".herdr-codex-launch-v1";

pub(crate) fn is_shim(executable: &Path) -> bool {
    executable
        .parent()
        .is_some_and(|parent| parent.join(MARKER).is_file())
}

/// One private directory per server process. Existing panes can outlive a
/// replaced server, so their launch path must not be removed during handoff.
pub(crate) fn install_shim(executable: &Path) -> io::Result<PathBuf> {
    for attempt in 0..100 {
        let directory =
            std::env::temp_dir().join(format!("herdr-codex-{}-{attempt}", std::process::id()));
        match super::create_remote_private_dir(&directory) {
            Ok(()) => {
                let result = install_link(executable, &directory)
                    .and_then(|()| std::fs::write(directory.join(MARKER), b"v1\n"));
                if let Err(error) = result {
                    let _ = std::fs::remove_dir_all(&directory);
                    return Err(error);
                }
                return Ok(directory);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::other(
        "no private Codex shim directory available",
    ))
}

#[cfg(unix)]
fn install_link(executable: &Path, directory: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(executable, directory.join("codex"))
}

#[cfg(not(unix))]
fn install_link(executable: &Path, directory: &Path) -> io::Result<()> {
    let target = directory.join("codex.exe");
    std::fs::hard_link(executable, &target)
        .or_else(|_| std::fs::copy(executable, target).map(|_| ()))
}

pub(crate) fn command(executable: &Path, args: &[OsString]) -> io::Result<Command> {
    #[cfg(windows)]
    if executable
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("ps1"))
    {
        let mut command = Command::new("powershell.exe");
        command
            .args(["-NoLogo", "-NoProfile", "-File"])
            .arg(executable)
            .args(args);
        return Ok(command);
    }
    // Rust's Windows Command handles native argv quoting, including its safe
    // cmd.exe encoding for batch files; never interpolate a `%*` shim string.
    let mut command = Command::new(executable);
    command.args(args);
    Ok(command)
}

#[cfg(unix)]
pub(crate) fn run(mut command: Command) -> io::Result<()> {
    use std::os::unix::process::CommandExt;
    Err(command.exec())
}

#[cfg(not(unix))]
pub(crate) fn run(mut command: Command) -> io::Result<()> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Console::{
            SetConsoleCtrlHandler, CTRL_BREAK_EVENT, CTRL_C_EVENT,
        };
        unsafe extern "system" fn handle(event: u32) -> i32 {
            // The child shares this console and receives the event itself.
            // This callback is not inherited; only keep the waiting shim alive.
            i32::from(matches!(event, CTRL_C_EVENT | CTRL_BREAK_EVENT))
        }
        if unsafe { SetConsoleCtrlHandler(Some(handle), 1) } == 0 {
            return Err(io::Error::last_os_error());
        }
    }
    let status = command.status()?;
    std::process::exit(status.code().unwrap_or(1));
}

pub(crate) fn configure_probe(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(not(unix))]
    super::configure_usage_probe_command(command);
}

pub(crate) fn stop_probe(child: &mut Child) {
    #[cfg(unix)]
    if child.id() > 1 {
        // A private process group contains only the --help probe and wrappers.
        unsafe {
            libc::kill(-(child.id() as i32), libc::SIGKILL);
        }
    }
    super::terminate_usage_probe(child);
}

#[cfg(unix)]
pub(crate) fn probe_succeeded(child: &mut Child) -> io::Result<Option<bool>> {
    // Do not reap the group leader before stop_probe: its PID must remain
    // reserved until the private process group has been terminated (also macOS).
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    if unsafe {
        libc::waitid(
            libc::P_PID,
            child.id(),
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    if unsafe { info.si_pid() } == 0 {
        return Ok(None);
    }
    Ok(Some(
        info.si_code == libc::CLD_EXITED && unsafe { info.si_status() } == 0,
    ))
}

#[cfg(not(unix))]
pub(crate) fn probe_succeeded(child: &mut Child) -> io::Result<Option<bool>> {
    child
        .try_wait()
        .map(|status| status.map(|status| status.success()))
}

/// Poll a single-reader pipe without blocking on inherited writer handles.
#[cfg(unix)]
pub(crate) fn read_probe_pipe<P: std::io::Read + std::os::fd::AsRawFd>(
    pipe: &mut P,
    buffer: &mut [u8],
) -> io::Result<Option<usize>> {
    if !super::poll_fd_readable(pipe.as_raw_fd(), 0)? {
        return Ok(None);
    }
    pipe.read(buffer).map(Some)
}

#[cfg(windows)]
pub(crate) fn read_probe_pipe<P: std::io::Read + std::os::windows::io::AsRawHandle>(
    pipe: &mut P,
    buffer: &mut [u8],
) -> io::Result<Option<usize>> {
    use windows_sys::Win32::Foundation::{ERROR_BROKEN_PIPE, ERROR_PIPE_NOT_CONNECTED};
    use windows_sys::Win32::System::Pipes::PeekNamedPipe;
    let mut available = 0;
    let success = unsafe {
        PeekNamedPipe(
            pipe.as_raw_handle(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            &mut available,
            std::ptr::null_mut(),
        )
    };
    if success == 0 {
        let error = io::Error::last_os_error();
        return if matches!(
            error.raw_os_error().map(|code| code as u32),
            Some(ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED)
        ) {
            Ok(Some(0))
        } else {
            Err(error)
        };
    }
    if available == 0 {
        return Ok(None);
    }
    let count = buffer.len().min(available as usize);
    pipe.read(&mut buffer[..count]).map(Some)
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn read_probe_pipe<P: std::io::Read>(
    _pipe: &mut P,
    _buffer: &mut [u8],
) -> io::Result<Option<usize>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "probe pipes unsupported",
    ))
}

#[cfg(unix)]
pub(crate) fn same_file(left: &Path, right: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (left.metadata(), right.metadata()) {
        (Ok(left), Ok(right)) => left.dev() == right.dev() && left.ino() == right.ino(),
        _ => false,
    }
}

#[cfg(windows)]
pub(crate) fn same_file(left: &Path, right: &Path) -> bool {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };
    fn identity(path: &Path) -> Option<(u32, u32, u32)> {
        let file = std::fs::File::open(path).ok()?;
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
            return None;
        }
        Some((
            info.dwVolumeSerialNumber,
            info.nFileIndexHigh,
            info.nFileIndexLow,
        ))
    }
    match (identity(left), identity(right)) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn same_file(_left: &Path, _right: &Path) -> bool {
    false
}
