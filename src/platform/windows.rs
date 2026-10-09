use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet, VecDeque},
    ffi::{c_void, OsStr},
    mem::{size_of, MaybeUninit},
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    path::PathBuf,
    ptr::{copy_nonoverlapping, null_mut},
    sync::{
        atomic::{AtomicU32, AtomicU64, Ordering as AtomicOrdering},
        Arc, LazyLock, Mutex, OnceLock,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

mod client_stream;
#[cfg(test)]
pub(crate) use client_stream::client_transport_process_sample;
pub(crate) use client_stream::{
    client_stream_control, finish_client_stream, prepare_server_client_stream,
    read_client_handshake, write_client_stream, ClientStreamControl, ServerClientStream,
};
mod clipboard_image;
mod command_search;
mod config_backup;
mod local_socket;
pub(crate) use local_socket::{connect_local_pipe, windows_pipe_names};
mod notifications;
pub(super) mod persist_files;
pub(crate) use notifications::{
    foreground_desktop_notification_host, maybe_activate_desktop_notification,
    show_actionable_desktop_notification, show_desktop_notification,
};

static ALLOW_UNELEVATED_CLIENTS: OnceLock<bool> = OnceLock::new();

pub(crate) fn allow_unelevated_clients() {
    let _ = ALLOW_UNELEVATED_CLIENTS.set(true);
}

pub(crate) fn probe_local_server(path: &std::path::Path) -> std::io::Result<()> {
    connect_local_pipe(path, Some(Instant::now() + Duration::from_millis(500)))
        .map(|_| ())
        .map_err(local_server_connection_error)
}

pub(crate) fn local_server_security_descriptor(
) -> std::io::Result<interprocess::os::windows::security_descriptor::SecurityDescriptor> {
    use windows_sys::Win32::Security::{
        GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut token = null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let mut elevation = TOKEN_ELEVATION::default();
    let mut needed = 0;
    if unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenElevation,
            (&mut elevation as *mut TOKEN_ELEVATION).cast(),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut needed,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    let allow_unelevated = ALLOW_UNELEVATED_CLIENTS.get().copied().unwrap_or(false);
    let integrity = if elevation.TokenIsElevated != 0 && !allow_unelevated {
        "HI"
    } else {
        "ME"
    };
    // The account DACL alone cannot distinguish ordinary and elevated clients.
    // The integrity label blocks both reading and writing from lower levels.
    user_security_descriptor("GRGW", &format!("S:(ML;;NRNW;;;{integrity})"))
}

fn user_security_descriptor(
    access: &str,
    integrity_label: &str,
) -> std::io::Result<interprocess::os::windows::security_descriptor::SecurityDescriptor> {
    use interprocess::os::windows::security_descriptor::SecurityDescriptor;
    use widestring::{U16CStr, U16CString};
    use windows_sys::Win32::Security::{
        Authorization::ConvertSidToStringSidW, GetTokenInformation, TokenUser, TOKEN_QUERY,
        TOKEN_USER,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut token = null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let mut needed = 0;
    unsafe { GetTokenInformation(token.as_raw_handle(), TokenUser, null_mut(), 0, &mut needed) };
    if needed == 0 {
        return Err(std::io::Error::last_os_error());
    }
    // usize storage keeps TOKEN_USER aligned and its trailing SID alive.
    let mut buffer = vec![0usize; (needed as usize).div_ceil(size_of::<usize>())];
    if unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buffer.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: GetTokenInformation initialized the aligned TOKEN_USER and SID.
    let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    let mut sid = null_mut();
    if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut sid) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let sid_text = unsafe { U16CStr::from_ptr_str(sid) }.to_string_lossy();
    unsafe { LocalFree(sid.cast()) };
    // Use the account SID rather than the elevated token's Administrators owner.
    let sddl = U16CString::from_str(format!(
        "D:P(A;;GA;;;SY)(A;;{access};;;{sid_text}){integrity_label}"
    ))
    .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidInput, err))?;
    SecurityDescriptor::deserialize(&sddl)
}

pub(crate) fn local_server_connection_error(error: std::io::Error) -> std::io::Error {
    if error.kind() != std::io::ErrorKind::PermissionDenied {
        return error;
    }
    std::io::Error::new(
        error.kind(),
        format!(
            "For an elevated server, use an administrator terminal. Sharing with ordinary \
             clients requires stopping it there and restarting its `herdr server` command \
             with --allow-unelevated-clients (closes panes). {error}"
        ),
    )
}

pub(crate) use command_search::{
    cli_child_path_platform, cli_invocation_platform, command_availability_fallback_platform,
    command_file_candidates_platform, command_search_dirs_platform,
};
#[cfg(test)]
pub(crate) use command_search::{set_test_command_search_environment, CommandSearchEnvironment};

/// 让出 stdout：把标准输出句柄换成 `NUL` 并关闭原句柄（管道写端）。statusline 回调回放完
/// stdin 后调用，之后本进程再等上报也不会拖住管道另一端读 EOF。Rust 的 `std::io::stdout()`
/// 每次写都重新取标准句柄，换掉后写入落到 `NUL`。
pub(crate) fn detach_stdout() -> std::io::Result<()> {
    use std::os::windows::io::IntoRawHandle as _;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Console::{GetStdHandle, SetStdHandle, STD_OUTPUT_HANDLE};

    let null = std::fs::OpenOptions::new().write(true).open("NUL")?;
    let previous = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
    // 新句柄成为进程的标准输出，随进程退出释放。
    if unsafe { SetStdHandle(STD_OUTPUT_HANDLE, null.into_raw_handle()) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    if !previous.is_null() && previous != INVALID_HANDLE_VALUE {
        unsafe { CloseHandle(previous) };
    }
    Ok(())
}

/// 关机时 Windows shell 自报的可疑退出码。
///
/// Windows 没有 `128 + signal` 语义（其对应量 `0xC000013A` 由
/// `classify_child_exit` 单列），所以只认最泛用的失败码 `1`：shell 在会话结束
/// 时被拆掉 ConPTY 往往就是这个码。代价只是多做一次会话检查点
/// （HSR-04 / 上游 #4320）。
fn exit_code_suspects_host_shutdown(code: u32) -> bool {
    code == 1
}

pub(crate) fn windows_virtual_terminal_input_active() -> bool {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, ENABLE_VIRTUAL_TERMINAL_INPUT, STD_INPUT_HANDLE,
    };

    let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return false;
    }
    let mut mode = 0;
    (unsafe { GetConsoleMode(handle, &mut mode) } != 0) && mode & ENABLE_VIRTUAL_TERMINAL_INPUT != 0
}

pub(crate) fn classify_child_exit(status: &portable_pty::ExitStatus) -> super::ChildExitReason {
    // STATUS_CONTROL_C_EXIT is reported without a Unix signal by portable-pty.
    if status.exit_code() == 0xC000013A {
        super::ChildExitReason::Interrupted
    } else if exit_code_suspects_host_shutdown(status.exit_code()) {
        // 关机时 ConPTY 下的 shell 同样只留一个可疑退出码。
        super::ChildExitReason::SuspectedInterruption
    } else {
        super::ChildExitReason::Exited
    }
}

pub(crate) fn host_shutdown_in_progress() -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_SHUTTINGDOWN};

    // This describes the current Windows session, including logoff. Child exit
    // codes alone cannot distinguish host shutdown from ordinary process failure.
    unsafe { GetSystemMetrics(SM_SHUTTINGDOWN) != 0 }
}

pub(crate) struct RemoteBridgeWake;

impl RemoteBridgeWake {
    pub(crate) fn new() -> std::io::Result<Self> {
        Ok(Self)
    }

    pub(crate) fn cancel(&self) -> std::io::Result<()> {
        // The named-pipe reader checks its cancellation flag between peeks.
        Ok(())
    }

    pub(crate) fn wait(&self, _stream: &crate::ipc::LocalStream) -> std::io::Result<()> {
        // Synchronous named pipes still use peek-before-read polling on Windows.
        std::thread::sleep(Duration::from_millis(1));
        Ok(())
    }
}

pub(crate) fn wait_client_stream_readable(
    _stream: &crate::ipc::LocalStream,
) -> std::io::Result<()> {
    // Sync named pipes have no read timeout. The caller peeks before each read and checks its
    // cancellation flag between polls, including when a frame arrives in several fragments.
    std::thread::sleep(Duration::from_millis(2));
    Ok(())
}

pub(crate) fn forward_remote_bridge_stdio(
    stream: crate::ipc::LocalStream,
    _idle_timeout: bool,
) -> std::io::Result<()> {
    use interprocess::TryClone as _;
    use std::sync::atomic::{AtomicBool, Ordering};

    let mut stdout = std::io::stdout().lock();
    let mut socket_to_stdout = stream.try_clone()?;
    let mut stdin_to_socket = stream;
    let upload_done = Arc::new(AtomicBool::new(false));
    let upload_done_worker = Arc::clone(&upload_done);
    let _upload = std::thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let _ = copy_flush(&mut stdin, &mut stdin_to_socket);
        upload_done_worker.store(true, Ordering::Release);
    });

    let mut buffer = [0_u8; 16 * 1024];
    while !upload_done.load(Ordering::Acquire) {
        match crate::ipc::poll_local_stream_read_count(&mut socket_to_stdout, &mut buffer)? {
            crate::ipc::LocalStreamReadCount::Data(read) => {
                std::io::Write::write_all(&mut stdout, &buffer[..read])?;
                std::io::Write::flush(&mut stdout)?;
            }
            crate::ipc::LocalStreamReadCount::Pending => {
                std::thread::sleep(Duration::from_millis(1));
            }
            crate::ipc::LocalStreamReadCount::Closed => break,
        }
    }
    Ok(())
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

pub(super) fn read_terminal_grid_size() -> std::io::Result<(u16, u16)> {
    crossterm::terminal::size()
}

pub(crate) fn replace_file(
    source: &std::path::Path,
    destination: &std::path::Path,
) -> std::io::Result<()> {
    // std::fs::rename on Windows renames through an open handle
    // (SetFileInformationByHandle / FileRenameInfoEx with POSIX replace
    // semantics): concurrent writers replacing the same destination serialize
    // correctly. Raw MoveFileExW(MOVEFILE_REPLACE_EXISTING) instead fails
    // transiently with ERROR_ACCESS_DENIED in that scenario, which made
    // concurrent client-state stores (e.g. chrome preferences) lose writers.
    std::fs::rename(source, destination)?;
    // Durability previously came from MOVEFILE_WRITE_THROUGH; flush the parent
    // directory so the rename itself is durable before returning.
    flush_parent_directory(destination)
}

fn flush_parent_directory(path: &std::path::Path) -> std::io::Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    flush_directory(parent)
}

pub(super) fn flush_directory(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{FlushFileBuffers, FILE_FLAG_BACKUP_SEMANTICS};

    // FlushFileBuffers requires a handle with write access; a read-only
    // directory handle fails with ERROR_ACCESS_DENIED.
    let directory = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    if unsafe { FlushFileBuffers(directory.as_raw_handle() as _) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

pub(crate) fn config_file_link_count(path: &std::path::Path) -> std::io::Result<u64> {
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };
    let file = std::fs::File::open(path)?;
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(u64::from(info.nNumberOfLinks))
}

pub(crate) fn create_config_temporary(
    path: &std::path::Path,
    private: bool,
) -> std::io::Result<std::fs::File> {
    if !private {
        return std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path);
    }
    use interprocess::os::windows::security_descriptor::AsSecurityDescriptorExt as _;
    use windows_sys::Win32::{
        Foundation::GENERIC_WRITE,
        Storage::FileSystem::{
            CreateFileW, CREATE_NEW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_DELETE, FILE_SHARE_READ,
            FILE_SHARE_WRITE,
        },
    };
    let descriptor = user_security_descriptor("GA", "")?;
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: null_mut(),
        bInheritHandle: 0,
    };
    descriptor.write_to_security_attributes(&mut attributes);
    let path = extended_length_path(path)?;
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            &attributes,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    // CreateFileW returned an owned handle; File closes it exactly once.
    Ok(unsafe { std::fs::File::from_raw_handle(handle) })
}

pub(crate) fn write_config_temporary(
    source: Option<&std::path::Path>,
    temporary: &std::path::Path,
    contents: &[u8],
) -> std::io::Result<()> {
    use std::io::Write;
    if source.is_some() {
        // If preparation finds an existing file, leave it to the recovery-backed
        // path instead of applying replacement-file permissions.
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "config appeared while preparing a new file; retry the update",
        ));
    }
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(temporary)?;
    output.write_all(contents)?;
    output.sync_all()
}

pub(crate) fn check_config_write_target(target: &std::path::Path) -> std::io::Result<()> {
    config_backup::check_recovery(target)
}

pub(crate) fn write_existing_config(
    target: &std::path::Path,
    contents: &[u8],
) -> std::io::Result<bool> {
    config_backup::write_existing(target, contents)
}

#[cfg(test)]
fn config_security_descriptor(
    path: &std::path::Path,
    information: windows_sys::Win32::Security::OBJECT_SECURITY_INFORMATION,
) -> std::io::Result<Vec<u8>> {
    use windows_sys::Win32::Security::GetFileSecurityW;
    let path = extended_length_path(path)?;
    let mut needed = 0;
    unsafe { GetFileSecurityW(path.as_ptr(), information, null_mut(), 0, &mut needed) };
    if needed == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut descriptor = vec![0_u8; needed as usize];
    if unsafe {
        GetFileSecurityW(
            path.as_ptr(),
            information,
            descriptor.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(descriptor)
}

#[cfg(test)]
fn config_security_sddl(
    descriptor: &mut [u8],
    information: windows_sys::Win32::Security::OBJECT_SECURITY_INFORMATION,
) -> std::io::Result<Vec<u16>> {
    use windows_sys::Win32::Security::{
        Authorization::{ConvertSecurityDescriptorToStringSecurityDescriptorW, SDDL_REVISION_1},
        SACL_SECURITY_INFORMATION,
    };
    let mut text = null_mut();
    if unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            descriptor.as_mut_ptr().cast(),
            SDDL_REVISION_1,
            // Only labels were queried from the SACL. Serialize that returned
            // SACL too; this does not request audit access to either file.
            information | SACL_SECURITY_INFORMATION,
            &mut text,
            null_mut(),
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    let result = unsafe { widestring::U16CStr::from_ptr_str(text) }
        .as_slice()
        .to_vec();
    unsafe { LocalFree(text.cast()) };
    Ok(result)
}

pub(crate) fn set_default_plugin_pane_pwd(
    _env: &mut Vec<(String, String)>,
    _cwd: &std::path::Path,
) {
}

#[cfg(target_pointer_width = "64")]
use windows_sys::{
    Wdk::System::Threading::ProcessWow64Information, Win32::System::Kernel::STRING32,
};

use windows_sys::{
    Wdk::System::Threading::ProcessCommandLineInformation,
    Wdk::System::Threading::{NtQueryInformationProcess, ProcessBasicInformation},
    Win32::{
        Foundation::{
            CloseHandle, GlobalFree, LocalFree, FILETIME, HANDLE, HWND, INVALID_HANDLE_VALUE,
            MAX_PATH, NTSTATUS, STATUS_BUFFER_OVERFLOW, STATUS_BUFFER_TOO_SMALL,
            STATUS_INFO_LENGTH_MISMATCH, STATUS_SUCCESS, UNICODE_STRING,
        },
        Globalization::{CompareStringOrdinal, CSTR_EQUAL, CSTR_GREATER_THAN, CSTR_LESS_THAN},
        Security::SECURITY_ATTRIBUTES,
        Storage::FileSystem::CreateDirectoryW,
        System::{
            Console::GetConsoleWindow,
            DataExchange::{
                CloseClipboard, CountClipboardFormats, EmptyClipboard, EnumClipboardFormats,
                GetClipboardData, GetClipboardOwner, GetClipboardSequenceNumber, OpenClipboard,
                RegisterClipboardFormatW, SetClipboardData,
            },
            Diagnostics::{
                Debug::ReadProcessMemory,
                ToolHelp::{
                    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, Thread32First,
                    Thread32Next, PROCESSENTRY32W, TH32CS_SNAPPROCESS, TH32CS_SNAPTHREAD,
                    THREADENTRY32,
                },
            },
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob,
                JobObjectExtendedLimitInformation, QueryInformationJobObject,
                SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            },
            Memory::{
                GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, VirtualQueryEx, GMEM_MOVEABLE,
                MEMORY_BASIC_INFORMATION,
            },
            Ole::{CF_DIB, CF_DIBV5, CF_LOCALE, CF_OEMTEXT, CF_TEXT, CF_UNICODETEXT},
            SystemInformation::GetSystemTimeAsFileTime,
            Threading::{
                CreateEventW, GetCurrentProcess, GetExitCodeProcess, GetProcessTimes,
                IsWow64Process2, OpenEventW, OpenProcess, OpenThread, QueryFullProcessImageNameW,
                ResumeThread, TerminateProcess, CREATE_NO_WINDOW, CREATE_SUSPENDED,
                DETACHED_PROCESS, PROCESS_BASIC_INFORMATION, PROCESS_QUERY_INFORMATION,
                PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE, PROCESS_VM_READ,
                SYNCHRONIZATION_SYNCHRONIZE, THREAD_SUSPEND_RESUME,
            },
        },
        UI::{
            Input::{
                Ime::ImmGetDefaultIMEWnd,
                KeyboardAndMouse::{
                    GetKeyboardLayout, SendInput, ToUnicodeEx, INPUT, INPUT_0, INPUT_KEYBOARD,
                    KEYBDINPUT, KEYEVENTF_KEYUP,
                },
            },
            Shell::{CommandLineToArgvW, ShellExecuteW},
            WindowsAndMessaging::{
                GetForegroundWindow, GetWindowThreadProcessId, SendMessageTimeoutW,
                SMTO_ABORTIFHUNG, WM_IME_CONTROL,
            },
        },
    },
};

use super::{
    ClipboardImage, ForegroundJob, ProcessLineage, ProcessParentEntry, ProcessSessionId,
    ProcessSessionMember, Signal,
};

const STILL_ACTIVE: u32 = 259;
/// At least one unidentified detection tick (500 ms): every pane observing within the same tick
/// window shares one Toolhelp snapshot instead of taking its own.
const FOREGROUND_PROCESS_SNAPSHOT_CACHE_TTL: Duration = Duration::from_millis(500);
/// Safety recheck for a quiet agent-less pane: output (and the acquisition window after it)
/// triggers the next observation at once; without output the process tree is re-read at this
/// pace instead of on every detection tick.
const QUIET_PANE_PROCESS_RECHECK: Duration = Duration::from_secs(2);
const AGENT_CLASSIFICATION_CACHE_CAPACITY: usize = 4_096;
const AGENT_CLASSIFICATION_CACHE_RETENTION: Duration = Duration::from_secs(60);
/// How long a "not an agent" verdict made without the command line (access denied, or a process
/// still starting) is reused: an agent started through a runtime (`node.exe`, `bun.exe`) is only
/// recognisable from its command line, so the read is retried soon.
const AGENT_CLASSIFICATION_UNREAD_TTL: Duration = Duration::from_secs(1);
const FOREGROUND_SELECTION_RECHECK: Duration = Duration::from_secs(5);
const FOREGROUND_SELECTION_CACHE_CAPACITY: usize = 1_024;
const FOREGROUND_SELECTION_CACHE_RETENTION: Duration = Duration::from_secs(60);
const PANE_RUNTIME_MARKER_ENV_VAR: &str = "HERDR_PANE_RUNTIME_ID";

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default)]
struct ProcessInspectionCounts {
    snapshots: u64,
    opens: u64,
    command_reads: u64,
    creation_queries: u64,
    parent_queries: u64,
    image_queries: u64,
}

#[cfg(test)]
thread_local! {
    static PROCESS_INSPECTION_COUNTS: std::cell::RefCell<ProcessInspectionCounts> =
        std::cell::RefCell::new(ProcessInspectionCounts::default());
}

/// Native processor architecture of the Windows host as an
/// `IMAGE_FILE_MACHINE_*` value. `IsWow64Process2` reports the native machine
/// even when an x64 Herdr runs under emulation on Windows ARM64, which the
/// build target alone cannot reveal.
pub(crate) fn native_machine_type() -> u16 {
    let mut process_machine = 0u16;
    let mut native_machine = 0u16;
    // SAFETY: `GetCurrentProcess` returns a pseudo-handle and both out-pointers
    // are valid for the duration of the call.
    let ok = unsafe {
        IsWow64Process2(
            GetCurrentProcess(),
            &mut process_machine,
            &mut native_machine,
        )
    };
    if ok != 0 && native_machine != 0 {
        native_machine
    } else {
        process_machine
    }
}

pub(crate) fn terminal_title_for_presentation(title: &str) -> &str {
    title.strip_prefix("Administrator: ").unwrap_or(title)
}

pub(crate) fn prepare_paste_text_for_pty_platform(text: String) -> String {
    text.replace("\r\n", "\n").replace('\n', "\r\n")
}

pub(crate) fn normalize_cwd_for_launch_platform(path: &std::path::Path) -> PathBuf {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::path::{Component, Prefix};
    use windows_sys::Win32::{
        Foundation::INVALID_HANDLE_VALUE,
        Storage::FileSystem::{FindClose, FindFirstFileW, WIN32_FIND_DATAW},
    };

    fn stored_name(path: &std::path::Path) -> Option<std::ffi::OsString> {
        let input = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let mut data = std::mem::MaybeUninit::<WIN32_FIND_DATAW>::uninit();
        let handle = unsafe { FindFirstFileW(input.as_ptr(), data.as_mut_ptr()) };
        if handle == INVALID_HANDLE_VALUE {
            return None;
        }
        let data = unsafe { data.assume_init() };
        unsafe { FindClose(handle) };
        let len = data
            .cFileName
            .iter()
            .position(|&ch| ch == 0)
            .unwrap_or(data.cFileName.len());
        Some(std::ffi::OsString::from_wide(&data.cFileName[..len]))
    }

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => match prefix.kind() {
                Prefix::Disk(drive) => {
                    normalized.push(format!("{}:", char::from(drive).to_ascii_uppercase()))
                }
                _ => normalized.push(prefix.as_os_str()),
            },
            Component::Normal(name) => {
                let candidate = normalized.join(name);
                normalized.push(stored_name(&candidate).unwrap_or_else(|| name.to_os_string()));
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

pub(crate) fn plugin_runtime_path_platform(path: &std::path::Path) -> PathBuf {
    use std::os::windows::ffi::OsStrExt;

    let Some(candidate) = standard_windows_path(path) else {
        return path.to_path_buf();
    };
    // Rust can canonicalize a long standard path by adding its own verbatim prefix, but native
    // process consumers still need the original prefix when the plugin root exceeds MAX_PATH.
    if candidate.join("").as_os_str().encode_wide().count() >= MAX_PATH as usize {
        return path.to_path_buf();
    }
    match candidate.canonicalize() {
        Ok(canonical) if canonical == path => candidate,
        _ => path.to_path_buf(),
    }
}

fn standard_windows_path(path: &std::path::Path) -> Option<PathBuf> {
    use std::path::{Component, Prefix};

    let mut components = path.components();
    let Component::Prefix(prefix) = components.next()? else {
        return None;
    };
    let mut candidate = match prefix.kind() {
        Prefix::VerbatimDisk(drive) => PathBuf::from(format!("{}:", char::from(drive))),
        Prefix::VerbatimUNC(server, share) => {
            let mut candidate = PathBuf::from(r"\\");
            candidate.push(server);
            candidate.push(share);
            candidate
        }
        _ => return None,
    };
    candidate.push(components.as_path());
    Some(candidate)
}

/// Resolves against the current foreground layout because asynchronous console
/// records do not retain the layout that was active when the key was pressed.
pub(crate) fn resolve_base_printable_key(vk: u16, scan: u16) -> Option<char> {
    // SAFETY: the foreground window and its layout are owned by Win32.
    let layout = unsafe {
        let thread_id = GetWindowThreadProcessId(GetForegroundWindow(), null_mut());
        GetKeyboardLayout(thread_id)
    };
    resolve_base_printable_key_in_layout(vk, scan, layout)
}

pub(crate) fn resolve_base_printable_key_in_layout(
    vk: u16,
    scan: u16,
    layout: windows_sys::Win32::UI::Input::KeyboardAndMouse::HKL,
) -> Option<char> {
    // SAFETY: Win32 owns the handles; the fixed buffers match the API lengths.
    unsafe {
        let key_state = [0u8; 256];
        let mut output = [0u16; 2];
        let written = ToUnicodeEx(
            vk.into(),
            scan.into(),
            key_state.as_ptr(),
            output.as_mut_ptr(),
            output.len() as i32,
            0x4,
            layout,
        );
        // A negative result identifies a dead key; its spacing accent is still
        // the key's identity. Flag 0x4 above keeps composition state unchanged.
        let units = output.get(..usize::try_from(written.unsigned_abs()).ok()?)?;
        let mut chars = char::decode_utf16(units.iter().copied());
        let ch = chars.next()?.ok()?;
        (chars.next().is_none() && !ch.is_control()).then_some(ch)
    }
}

const MAX_PROCESS_ENVIRONMENT_BYTES: usize = 256 * 1024;
const PROCESS_ENVIRONMENT_READ_CHUNK_BYTES: usize = 16 * 1024;
const PROCESS_RUNTIME_MARKER_CACHE_CAPACITY: usize = 1_024;
const PROCESS_RUNTIME_MARKER_CACHE_RETENTION: Duration = Duration::from_secs(60);
const PROCESS_RUNTIME_MARKER_NEGATIVE_TTL: Duration = Duration::from_secs(1);

static NEXT_PANE_RUNTIME_MARKER: AtomicU64 = AtomicU64::new(1);
static PROCESS_RUNTIME_MARKER_CACHE: LazyLock<Mutex<HashMap<u32, CachedProcessRuntimeMarker>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static GIT_BASH_PROCESS_CACHE: LazyLock<Mutex<HashMap<u32, CachedGitBashProcess>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(crate) fn remote_ssh_config_paths() -> super::RemoteSshConfigPaths {
    super::RemoteSshConfigPaths {
        user_config: super::remote_ssh_user_config_path(),
        system_config: std::env::var_os("PROGRAMDATA")
            .map(PathBuf::from)
            .map(|dir| dir.join("ssh").join("ssh_config")),
        multiplexing: false,
    }
}

pub(crate) fn default_known_hosts_path() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .map(|home| home.join(".ssh").join("known_hosts"))
}

pub(crate) fn create_remote_ssh_config_dir(_control_socket_name: &str) -> std::io::Result<PathBuf> {
    let base = super::ensure_remote_private_temp_base()?;
    for attempt in 0..100 {
        let dir = base.join(format!("ssh-{}-{attempt}", std::process::id()));
        match create_remote_private_dir(&dir) {
            Ok(()) => return Ok(dir),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "failed to create private herdr ssh config directory",
    ))
}

pub(crate) fn create_remote_ssh_config_file(
    path: &std::path::Path,
) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

/// 复用只在支持 SSH 多路复用的平台上有意义；Windows 的
/// `remote_ssh_config_paths().multiplexing` 为 false（没有 ControlPath），
/// 这里退回一次性唯一目录。共享通道把它留到进程结束（不删），进程退出后由下一个
/// 进程的 `sweep_stale_remote_private_entries_platform` 清理。
pub(crate) fn reusable_remote_ssh_config_dir(
    _key: &str,
    control_socket_name: &str,
) -> std::io::Result<PathBuf> {
    create_remote_ssh_config_dir(control_socket_name)
}

/// 覆写受管 ssh 配置：共享目录里同一档案的配置每次操作都会重写，
/// `create_new` 会 AlreadyExists。
pub(crate) fn write_remote_ssh_config_file(
    path: &std::path::Path,
) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
}

pub(crate) fn create_remote_private_dir(path: &std::path::Path) -> std::io::Result<()> {
    use interprocess::os::windows::security_descriptor::{
        AsSecurityDescriptorExt as _, SecurityDescriptor,
    };
    use widestring::U16CString;

    let sddl = U16CString::from_str("D:P(A;OICI;GA;;;SY)(A;OICI;GA;;;OW)")
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidInput, err))?;
    let security_descriptor = SecurityDescriptor::deserialize(&sddl)?;
    let mut security_attributes = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(u32::MAX),
        lpSecurityDescriptor: null_mut(),
        bInheritHandle: 0,
    };
    security_descriptor.write_to_security_attributes(&mut security_attributes);
    let path = extended_length_path(path)?;
    if unsafe { CreateDirectoryW(path.as_ptr(), &security_attributes) } != 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn extended_length_path(path: &std::path::Path) -> std::io::Result<Vec<u16>> {
    use std::os::windows::ffi::OsStrExt as _;

    let path = std::path::absolute(path)?;
    let wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
    let mut extended = if wide.starts_with(&[b'\\' as u16, b'\\' as u16, b'?' as u16, b'\\' as u16])
        || wide.starts_with(&[b'\\' as u16, b'\\' as u16, b'.' as u16, b'\\' as u16])
    {
        wide
    } else if wide.starts_with(&[b'\\' as u16, b'\\' as u16]) {
        "\\\\?\\UNC\\"
            .encode_utf16()
            .chain(wide.into_iter().skip(2))
            .collect()
    } else {
        "\\\\?\\".encode_utf16().chain(wide).collect()
    };
    extended.push(0);
    Ok(extended)
}

pub(crate) fn remote_private_temp_base() -> PathBuf {
    crate::config::state_dir().join("remote")
}

/// 端点路径只在即将绑定前计算：顺带清理已退出进程留下的陈旧端点标记文件（每个基准每进程
/// 一次），只用普通 saved 连接、从不建受管配置目录的用户也不会越积越多。
pub(crate) fn remote_bridge_endpoint_path(_readable_name: &str, short_name: &str) -> PathBuf {
    let base = remote_private_temp_base();
    sweep_stale_remote_private_entries_platform(&base);
    base.join(short_name)
}

/// 私有目录基准在 herdr 自己的状态目录里，没有系统清理：进程被杀或异常退出时不走 Drop，
/// 共享通道目录按设计活到进程结束也不删，这些项会一直留着。每个基准每进程只扫一次——
/// 陈旧项只来自已退出的进程，之后再退出的进程留下的项由下一个用到远程功能的 herdr 进程
/// 收拾，开销只落在每个进程第一次建私有目录项时。
pub(crate) fn sweep_stale_remote_private_entries_platform(base: &std::path::Path) {
    static SWEPT_BASES: LazyLock<Mutex<HashSet<PathBuf>>> =
        LazyLock::new(|| Mutex::new(HashSet::new()));
    let first_use = SWEPT_BASES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(base.to_path_buf());
    if first_use {
        sweep_stale_remote_private_entries(base);
    }
}

/// 删掉 `base` 下属主进程已确定退出的私有目录项，返回删掉的项数。尽力而为：失败只记日志，
/// 不影响正在进行的远程操作。只认名字逐字匹配（`remote_private_entry`）且类型对得上的项，
/// 符号链接与 junction 一律不碰也不跟随；本进程、仍在运行或查不清的进程的项都保留。
fn sweep_stale_remote_private_entries(base: &std::path::Path) -> usize {
    let entries = match std::fs::read_dir(base) {
        Ok(entries) => entries,
        Err(err) => {
            if err.kind() != std::io::ErrorKind::NotFound {
                tracing::debug!(
                    base = %base.display(),
                    %err,
                    "could not scan remote private dir for stale entries"
                );
            }
            return 0;
        }
    };
    let current_pid = std::process::id();
    let mut exited_owners = HashMap::new();
    let mut removed = 0;
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                tracing::debug!(
                    base = %base.display(),
                    %err,
                    "could not read remote private dir entry"
                );
                continue;
            }
        };
        let name = entry.file_name();
        let Some((pid, kind)) = name.to_str().and_then(remote_private_entry) else {
            continue;
        };
        if pid == current_pid
            || !*exited_owners
                .entry(pid)
                .or_insert_with(|| process_has_exited(pid))
        {
            continue;
        }
        // 目录项自带的类型不跟随重解析点：junction 与符号链接既不算 dir 也不算 file。
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        let result = match kind {
            RemotePrivateEntryKind::Dir if file_type.is_dir() => std::fs::remove_dir_all(&path),
            RemotePrivateEntryKind::File if file_type.is_file() => std::fs::remove_file(&path),
            _ => continue,
        };
        match result {
            Ok(()) => removed += 1,
            Err(err) => tracing::debug!(
                path = %path.display(),
                %err,
                "could not remove stale remote private entry"
            ),
        }
    }
    if removed > 0 {
        tracing::debug!(
            base = %base.display(),
            removed,
            "removed stale remote private entries"
        );
    }
    removed
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RemotePrivateEntryKind {
    Dir,
    File,
}

/// herdr 在 `remote_private_temp_base()` 下按属主 pid 命名的目录项：返回属主 pid 与种类。名字
/// 必须逐字对应创建处的格式，否则 `None`（不清理）；创建处改名时这里要一起改。
fn remote_private_entry(name: &str) -> Option<(u32, RemotePrivateEntryKind)> {
    // `create_remote_ssh_config_dir`：`ssh-<pid>-<attempt>`。
    if let Some(rest) = name.strip_prefix("ssh-") {
        let (pid, attempt) = rest.split_once('-')?;
        canonical_decimal::<u32>(attempt)?;
        return Some((canonical_decimal(pid)?, RemotePrivateEntryKind::Dir));
    }
    // `remote::attach::private_download_dir`：`herdr-remote-<pid>-<asset key>-<attempt>`。
    if let Some(rest) = name.strip_prefix("herdr-remote-") {
        let (pid, rest) = rest.split_once('-')?;
        let (asset_key, attempt) = rest.rsplit_once('-')?;
        canonical_decimal::<u32>(attempt)?;
        if !is_remote_asset_key(asset_key) {
            return None;
        }
        return Some((canonical_decimal(pid)?, RemotePrivateEntryKind::Dir));
    }
    // 桥接端点短名（命名管道旁的标记文件）：`remote::attach::local_forward_socket_path` 的
    // `herdr-r-<pid>-<目标前缀>-<hash>.sock`，`remote::saved` 的 `herdr-s-` / `herdr-api-`
    // `<pid>-<档案 id 前 16 位>.sock`，`remote::askpass` 的 `herdr-ap-<pid>-<序号>.sock`。
    let (tag, rest) = name
        .strip_prefix("herdr-")?
        .strip_suffix(".sock")?
        .split_once('-')?;
    let (pid, rest) = rest.split_once('-')?;
    let rest_matches = match tag {
        "r" => rest.rsplit_once('-').is_some_and(|(target, hash)| {
            is_remote_target_prefix(target) && is_lower_hex(hash, 16)
        }),
        "s" | "api" => is_lower_hex(rest, 16),
        "ap" => canonical_decimal::<u64>(rest).is_some(),
        _ => false,
    };
    if !rest_matches {
        return None;
    }
    Some((canonical_decimal(pid)?, RemotePrivateEntryKind::File))
}

/// 供创建处的测试核对：它们产出的名字能被陈旧项清理认出属主。
#[cfg(test)]
pub(crate) fn remote_private_entry_owner(name: &str) -> Option<u32> {
    remote_private_entry(name).map(|(pid, _)| pid)
}

/// 与 `format!("{n}")` 的输出逐字一致的十进制数：非空、只有数字、没有多余的前导零。
fn canonical_decimal<T: std::str::FromStr>(value: &str) -> Option<T> {
    let canonical = !value.is_empty()
        && value.bytes().all(|byte| byte.is_ascii_digit())
        && (value == "0" || !value.starts_with('0'));
    canonical.then(|| value.parse().ok()).flatten()
}

/// 下载资产的 key（`linux-x86_64`、`windows-installer`）：小写字母、数字与 `_`，`-` 分段。
fn is_remote_asset_key(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('-')
        && !value.ends_with('-')
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
}

/// 本地转发端点名里目标的前 8 个字符（已按 `sanitize_path_component` 清洗）。
fn is_remote_target_prefix(value: &str) -> bool {
    value.len() <= 8
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn is_lower_hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// 属主进程是否确定已退出（见 `process_state`）。拒绝访问等查不清的情形按仍在运行处理——
/// `process_exists` 把打不开一律当成不存在，删目录不能沿用：宁可留下陈旧项，也不删活进程的
/// 私有目录。
fn process_has_exited(pid: u32) -> bool {
    process_state(pid) == ProcessState::Exited
}

pub(crate) fn remote_reattach_program(program: &str) -> String {
    let path = std::env::current_exe()
        .ok()
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| PathBuf::from(program));
    format!(
        "& {}",
        remote_reattach_argument(&path.display().to_string())
    )
}

pub(crate) fn remote_reattach_argument(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// Encode native or targeted semantic Win32 input for a compatible ConPTY destination.
pub(crate) fn encode_windows_conpty_fallback(key: &crate::input::TerminalKey) -> Option<Vec<u8>> {
    use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};

    let (virtual_key_code, virtual_scan_code, unicode, control_key_state) =
        if let Some(record) = key.windows_record() {
            (
                record.virtual_key_code,
                record.virtual_scan_code,
                record.unicode,
                record.control_key_state,
            )
        } else if key.code == KeyCode::Esc
            && key.modifiers.is_empty()
            && key.kind == KeyEventKind::Press
            && key.vt_bytes().is_none()
        {
            return Some(b"\x1b[27;1;27;1;0;1_\x1b[27;1;27;0;0;1_".to_vec());
        } else if key.code == KeyCode::Enter && key.modifiers == KeyModifiers::SHIFT {
            (13, 28, 13, 16)
        } else {
            return None;
        };
    let unicode = ctrl_letter_control_character(virtual_key_code, unicode, control_key_state);
    let key_down = key.kind != KeyEventKind::Release;
    let repeat_count = if key_down { key.repeat_count.max(1) } else { 1 };

    Some(
        format!(
            "\x1b[{virtual_key_code};{virtual_scan_code};{unicode};{};{control_key_state};{repeat_count}_",
            u8::from(key_down),
        )
        .into_bytes(),
    )
}

/// Ctrl+A..Z records must carry their C0 control character (`vk - 0x40`, e.g. `0x03` for
/// Ctrl+C) like conhost produces: MSYS2/Cygwin programs only raise SIGINT (and other
/// termios specials) from the record's character field, while ConPTY raises the native
/// Ctrl+C event from the key code alone. A host, key remapper or IME that reports the plain
/// letter or no character would otherwise leave MSYS programs uninterruptible. AltGr
/// (Ctrl+Alt) and existing control characters are left untouched.
fn ctrl_letter_control_character(
    virtual_key_code: u16,
    unicode: u16,
    control_key_state: u32,
) -> u16 {
    use windows_sys::Win32::System::Console::{
        LEFT_ALT_PRESSED, LEFT_CTRL_PRESSED, RIGHT_ALT_PRESSED, RIGHT_CTRL_PRESSED,
    };

    let letter = (u16::from(b'A')..=u16::from(b'Z')).contains(&virtual_key_code);
    let ctrl_only = control_key_state & (LEFT_CTRL_PRESSED | RIGHT_CTRL_PRESSED) != 0
        && control_key_state & (LEFT_ALT_PRESSED | RIGHT_ALT_PRESSED) == 0;
    let control_character = (0x01..=0x1f).contains(&unicode);
    if letter && ctrl_only && !control_character {
        virtual_key_code - 0x40
    } else {
        unicode
    }
}

#[derive(Debug)]
struct CachedProcessSnapshot {
    built_at: Instant,
    snapshot: Arc<ProcessSnapshot>,
}

#[derive(Debug)]
struct ProcessSnapshotCache {
    cached: Option<CachedProcessSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProcessSignature {
    pid: u32,
    parent_pid: u32,
    name: String,
}

impl ProcessSignature {
    fn from_entry(entry: &WindowsProcessEntry) -> Self {
        Self {
            pid: entry.pid,
            parent_pid: entry.parent_pid,
            name: entry.name.clone(),
        }
    }

    fn matches(&self, entry: Option<&WindowsProcessEntry>) -> bool {
        entry.is_some_and(|entry| {
            self.pid == entry.pid && self.parent_pid == entry.parent_pid && self.name == entry.name
        })
    }
}

#[derive(Debug, Clone)]
enum ProcessIdentity {
    Handle(Arc<OwnedHandle>),
    #[cfg(test)]
    Stub {
        running: bool,
        creation_time: Option<u64>,
    },
}

impl ProcessIdentity {
    fn open(pid: u32) -> Option<Self> {
        #[cfg(test)]
        PROCESS_INSPECTION_COUNTS.with_borrow_mut(|counts| counts.opens += 1);
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            return None;
        }
        let handle = unsafe { OwnedHandle::from_raw_handle(handle.cast()) };
        Some(Self::Handle(Arc::new(handle)))
    }

    fn handle(&self) -> Option<HANDLE> {
        match self {
            Self::Handle(handle) => Some(handle.as_raw_handle().cast()),
            #[cfg(test)]
            Self::Stub { .. } => None,
        }
    }

    fn running(&self) -> bool {
        match self {
            Self::Handle(handle) => {
                let mut exit_code = 0;
                let read = unsafe {
                    GetExitCodeProcess(handle.as_raw_handle().cast(), &mut exit_code) != 0
                };
                read && exit_code == STILL_ACTIVE
            }
            #[cfg(test)]
            Self::Stub { running, .. } => *running,
        }
    }

    fn creation_time(&self) -> Option<u64> {
        match self {
            Self::Handle(handle) => process_creation_time(handle.as_raw_handle().cast()),
            #[cfg(test)]
            Self::Stub { creation_time, .. } => *creation_time,
        }
    }

    fn matches_observation(&self, observation: &ProcessObservation) -> bool {
        match (self, &observation.identity) {
            (Self::Handle(handle), Self::Handle(observed)) if Arc::ptr_eq(handle, observed) => true,
            _ => self.creation_time() == Some(observation.created),
        }
    }
}

#[derive(Debug)]
struct CachedForegroundSelection {
    shell: ProcessSignature,
    selected: ProcessSignature,
    descendants: Vec<ProcessSignature>,
    descendant_identities: Vec<ProcessIdentity>,
    shell_identity: ProcessIdentity,
    selected_identity: ProcessIdentity,
    observations: Vec<(ProcessSignature, Arc<ProcessObservation>)>,
    job: ForegroundJob,
    verified_at: Instant,
    last_used: Instant,
}

#[derive(Debug, Default)]
struct ForegroundSelectionCache {
    entries: HashMap<u32, CachedForegroundSelection>,
}

#[derive(Debug)]
struct CachedProcessRuntimeMarker {
    creation_time: u64,
    marker: Option<String>,
    cached_at: Instant,
    last_used: Instant,
}

#[derive(Debug)]
struct CachedGitBashProcess {
    creation_time: u64,
    is_git_bash: bool,
    last_used: Instant,
}

/// Whether one process instance (pid + creation time) looks like an agent. Most descendants of
/// a pane shell (subshells, prompt helpers such as gitstatusd) live across many snapshots; their
/// command lines are read and classified once instead of on every snapshot.
#[derive(Debug)]
struct CachedAgentClassification {
    creation_time: u64,
    identifies_agent: bool,
    /// The verdict is final: an agent was found, or the command line was read. A "not an agent"
    /// verdict without a command line expires after `AGENT_CLASSIFICATION_UNREAD_TTL`.
    settled: bool,
    cached_at: Instant,
    last_used: Instant,
}

type AgentClassificationCache = HashMap<u32, CachedAgentClassification>;

static FOREGROUND_PROCESS_SNAPSHOT_CACHE: Mutex<ProcessSnapshotCache> =
    Mutex::new(ProcessSnapshotCache { cached: None });
static FOREGROUND_SELECTION_CACHE: LazyLock<Mutex<ForegroundSelectionCache>> =
    LazyLock::new(|| Mutex::new(ForegroundSelectionCache::default()));
#[cfg(not(test))]
static AGENT_CLASSIFICATION_CACHE: LazyLock<Mutex<AgentClassificationCache>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
// Tests reuse fake pids and creation times across cases; each test thread gets its own cache.
#[cfg(test)]
thread_local! {
    static AGENT_CLASSIFICATION_CACHE: std::cell::RefCell<AgentClassificationCache> =
        std::cell::RefCell::new(HashMap::new());
}

fn with_agent_classification_cache<T>(
    action: impl FnOnce(&mut AgentClassificationCache) -> T,
) -> T {
    #[cfg(not(test))]
    {
        let mut cache = AGENT_CLASSIFICATION_CACHE
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        action(&mut cache)
    }
    #[cfg(test)]
    {
        AGENT_CLASSIFICATION_CACHE.with(|cache| action(&mut cache.borrow_mut()))
    }
}

fn cached_agent_classification(pid: u32, creation_time: u64) -> Option<bool> {
    with_agent_classification_cache(|cache| {
        let cached = cache.get_mut(&pid)?;
        if cached.creation_time != creation_time
            || (!cached.settled && cached.cached_at.elapsed() >= AGENT_CLASSIFICATION_UNREAD_TTL)
        {
            return None;
        }
        cached.last_used = Instant::now();
        Some(cached.identifies_agent)
    })
}

fn remember_agent_classification(
    pid: u32,
    creation_time: u64,
    identifies_agent: bool,
    command_line_read: bool,
) {
    with_agent_classification_cache(|cache| {
        if cache.len() >= AGENT_CLASSIFICATION_CACHE_CAPACITY {
            cache.retain(|_, cached| {
                cached.last_used.elapsed() < AGENT_CLASSIFICATION_CACHE_RETENTION
            });
            if cache.len() >= AGENT_CLASSIFICATION_CACHE_CAPACITY {
                cache.clear();
            }
        }
        let now = Instant::now();
        cache.insert(
            pid,
            CachedAgentClassification {
                creation_time,
                identifies_agent,
                settled: identifies_agent || command_line_read,
                cached_at: now,
                last_used: now,
            },
        );
    });
}

/// Whether a quiet agent-less pane last observed at `last_observation` re-reads the process tree
/// at `now`. Rechecks happen once per process-wide slot of `QUIET_PANE_PROCESS_RECHECK` rather
/// than that long after each pane's own last observation: every such pane then observes in the
/// first detection tick of the slot, inside one lifetime of the shared process snapshot
/// (`FOREGROUND_PROCESS_SNAPSHOT_CACHE_TTL`), so they all reuse a single Toolhelp snapshot.
pub(crate) fn quiet_pane_process_recheck_due(last_observation: Instant, now: Instant) -> bool {
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    process_recheck_slot_changed(*ORIGIN.get_or_init(Instant::now), last_observation, now)
}

fn process_recheck_slot_changed(origin: Instant, last_observation: Instant, now: Instant) -> bool {
    let slot = |at: Instant| {
        at.saturating_duration_since(origin).as_millis() / QUIET_PANE_PROCESS_RECHECK.as_millis()
    };
    slot(now) != slot(last_observation)
}

pub(crate) fn should_draw_host_cursor_by_default() -> bool {
    true
}

pub(crate) fn should_query_host_terminal_palette() -> bool {
    false
}

/// The machine's node name, as shown by tmux's `#h`.
pub(crate) fn hostname() -> Option<String> {
    std::env::var("COMPUTERNAME")
        .ok()
        .filter(|name| !name.is_empty())
}

pub(crate) fn local_datetime() -> Option<time::PrimitiveDateTime> {
    let mut timestamp: libc::time_t = 0;
    if unsafe { libc::time(&mut timestamp) } == -1 {
        return None;
    }
    let mut local: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_s(&mut local, &timestamp) } != 0 {
        return None;
    }
    let month = time::Month::try_from(u8::try_from(local.tm_mon + 1).ok()?).ok()?;
    let date = time::Date::from_calendar_date(
        local.tm_year + 1900,
        month,
        u8::try_from(local.tm_mday).ok()?,
    )
    .ok()?;
    let time = time::Time::from_hms(
        u8::try_from(local.tm_hour).ok()?,
        u8::try_from(local.tm_min).ok()?,
        u8::try_from(local.tm_sec).ok()?,
    )
    .ok()?;
    Some(time::PrimitiveDateTime::new(date, time))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WindowsProcessCommand {
    creation_time: Option<u64>,
    argv0: Option<String>,
    argv: Option<Vec<String>>,
    cmdline: Option<String>,
}

#[cfg(test)]
#[derive(Debug)]
struct ObservationReaderStub {
    parent_pid: Option<u32>,
    created: Option<u64>,
    image: String,
    command: WindowsProcessCommand,
    observations: AtomicU32,
    commands: AtomicU32,
}

#[derive(Debug)]
struct ProcessObservation {
    identity: ProcessIdentity,
    parent_pid: Option<u32>,
    created: u64,
    image: Option<String>,
}

impl ProcessObservation {
    fn same_metadata(&self, other: &Self) -> bool {
        self.created == other.created
            && self.parent_pid == other.parent_pid
            && self.image == other.image
    }

    fn name(&self) -> &str {
        self.image
            .as_deref()
            .and_then(|image| std::path::Path::new(image).file_name())
            .and_then(OsStr::to_str)
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone)]
struct WindowsProcessEntry {
    pid: u32,
    parent_pid: u32,
    name: String,
    command: OnceLock<WindowsProcessCommand>,
    observation: OnceLock<Option<Arc<ProcessObservation>>>,
    #[cfg(test)]
    reader: Option<Arc<ObservationReaderStub>>,
}

impl WindowsProcessEntry {
    fn new(pid: u32, parent_pid: u32, name: String) -> Self {
        Self {
            pid,
            parent_pid,
            name,
            command: OnceLock::new(),
            observation: OnceLock::new(),
            #[cfg(test)]
            reader: None,
        }
    }

    fn observation(&self) -> Option<&Arc<ProcessObservation>> {
        self.observation
            .get_or_init(|| {
                #[cfg(test)]
                if let Some(reader) = &self.reader {
                    reader.observations.fetch_add(1, AtomicOrdering::Relaxed);
                    return Some(Arc::new(ProcessObservation {
                        identity: ProcessIdentity::Stub {
                            running: true,
                            creation_time: reader.created,
                        },
                        parent_pid: reader.parent_pid,
                        created: reader.created?,
                        image: Some(reader.image.clone()),
                    }));
                }
                let identity = ProcessIdentity::open(self.pid)?;
                let process = identity.handle()?;
                let created = identity.creation_time()?;
                let parent_pid = process_basic_information(process)
                    .and_then(|basic| u32::try_from(basic.InheritedFromUniqueProcessId).ok());
                let image = process_executable_path(process);
                Some(Arc::new(ProcessObservation {
                    identity,
                    parent_pid,
                    created,
                    image,
                }))
            })
            .as_ref()
    }

    fn observed_name(&self) -> &str {
        self.observation()
            .map_or("", |observation| observation.name())
    }

    fn command(&self) -> &WindowsProcessCommand {
        self.command.get_or_init(|| {
            let Some(observation) = self.observation() else {
                return WindowsProcessCommand::from_cmdline("", None, None);
            };
            let command = self.read_command(observation);
            if command.creation_time == Some(observation.created) {
                command
            } else {
                WindowsProcessCommand::from_cmdline(
                    observation.name(),
                    Some(observation.created),
                    None,
                )
            }
        })
    }

    fn read_command(&self, observation: &ProcessObservation) -> WindowsProcessCommand {
        #[cfg(test)]
        if let Some(reader) = &self.reader {
            reader.commands.fetch_add(1, AtomicOrdering::Relaxed);
            return reader.command.clone();
        }
        read_process_command(self.pid, observation)
    }

    fn creation_time(&self) -> Option<u64> {
        self.observation().map(|observation| observation.created)
    }
}

#[derive(Debug)]
struct ProcessSnapshot {
    entries: Vec<WindowsProcessEntry>,
    entry_by_pid: HashMap<u32, usize>,
    children_by_parent: HashMap<u32, Vec<usize>>,
    agent_indices: OnceLock<Vec<usize>>,
}

impl ProcessSnapshot {
    fn new(entries: Vec<WindowsProcessEntry>) -> Self {
        let mut entry_by_pid = HashMap::with_capacity(entries.len());
        let mut children_by_parent = HashMap::<u32, Vec<usize>>::new();
        for (index, entry) in entries.iter().enumerate() {
            entry_by_pid.insert(entry.pid, index);
            children_by_parent
                .entry(entry.parent_pid)
                .or_default()
                .push(index);
        }
        Self {
            entries,
            entry_by_pid,
            children_by_parent,
            agent_indices: OnceLock::new(),
        }
    }

    fn entry(&self, pid: u32) -> Option<&WindowsProcessEntry> {
        self.entry_by_pid
            .get(&pid)
            .map(|&index| &self.entries[index])
    }

    /// 快照里父 pid 记为 `pid` 的进程。父进程退出后 pid 会被复用，这只是候选。
    fn child_pids(&self, pid: u32) -> impl Iterator<Item = u32> + '_ {
        self.children_by_parent
            .get(&pid)
            .into_iter()
            .flatten()
            .map(|&index| self.entries[index].pid)
    }

    fn descendant_signatures(&self, root_pid: u32) -> Vec<ProcessSignature> {
        let mut signatures = descendant_entries(root_pid, self)
            .into_iter()
            .map(ProcessSignature::from_entry)
            .collect::<Vec<_>>();
        signatures.sort_unstable_by_key(|entry| entry.pid);
        signatures
    }

    fn agent_indices(&self) -> &[usize] {
        self.agent_indices.get_or_init(|| {
            self.entries
                .iter()
                .enumerate()
                .filter_map(|(index, entry)| process_entry_identifies_agent(entry).then_some(index))
                .collect()
        })
    }
}

pub fn raise_server_nofile_limit() {}

pub(crate) fn apply_pane_runtime_marker_platform(command: &mut portable_pty::CommandBuilder) {
    if command_uses_git_bash(command) {
        command.env(PANE_RUNTIME_MARKER_ENV_VAR, next_pane_runtime_marker());
    }
}

fn next_pane_runtime_marker() -> String {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let counter = NEXT_PANE_RUNTIME_MARKER.fetch_add(1, AtomicOrdering::Relaxed);
    format!("{:x}-{timestamp:x}-{counter:x}", std::process::id())
}

fn raw_command_shell(comspec: Option<std::ffi::OsString>) -> std::ffi::OsString {
    comspec
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| r"C:\Windows\System32\cmd.exe".into())
}

pub(crate) fn interactive_shell_command(argv: &[String], shell_name: &str) -> Option<String> {
    let shell_name = shell_name.to_ascii_lowercase();
    let powershell = shell_name.contains("powershell") || shell_name.contains("pwsh");
    let script = powershell_agent_script(argv)?;
    if powershell {
        Some(script)
    } else {
        Some(cmd_encoded_powershell_command(&script))
    }
}

fn powershell_agent_script(argv: &[String]) -> Option<String> {
    let (program, args) = argv.split_first()?;
    if args.is_empty() {
        return Some(format!("& {}", super::quote_powershell_arg(program)));
    }

    let powershell_args = args
        .iter()
        .map(|arg| super::quote_powershell_arg(arg))
        .collect::<Vec<_>>()
        .join(" ");
    let command_line = args
        .iter()
        .map(|arg| super::quote_windows_command_line_arg(arg))
        .collect::<Vec<_>>()
        .join(" ");
    Some(format!(
        "if((Get-Command {} -ErrorAction SilentlyContinue).CommandType -eq 'ExternalScript'){{& {} {}}}else{{Start-Process -FilePath {} -ArgumentList {} -NoNewWindow -Wait}}",
        super::quote_powershell_arg(program),
        super::quote_powershell_arg(program),
        powershell_args,
        super::quote_powershell_arg(program),
        super::quote_powershell_arg(&command_line),
    ))
}

fn cmd_encoded_powershell_command(script: &str) -> String {
    use base64::Engine as _;

    let utf16 = script
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    let encoded = base64::engine::general_purpose::STANDARD.encode(utf16);
    format!("powershell.exe -NoLogo -NoProfile -EncodedCommand {encoded}")
}

pub(crate) fn detached_custom_command_process_platform(command: &str) -> std::process::Command {
    detached_custom_command_process_with_comspec(command, std::env::var_os("ComSpec"))
}

pub(crate) fn status_commands_supported() -> bool {
    true
}

pub(crate) fn configure_status_command(process: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;

    // The process must not run before it is assigned to the kill-on-close job.
    process.creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED);
}

pub(crate) struct StatusCommandGuard {
    job: usize,
}

impl StatusCommandGuard {
    pub(crate) fn new(child: &tokio::process::Child) -> std::io::Result<Self> {
        Self::for_process(child.raw_handle(), child.id())
    }

    pub(crate) fn from_std_child(child: &std::process::Child) -> std::io::Result<Self> {
        use std::os::windows::io::AsRawHandle;
        Self::for_process(Some(child.as_raw_handle()), Some(child.id()))
    }

    fn for_process(
        process: Option<std::os::windows::io::RawHandle>,
        process_id: Option<u32>,
    ) -> std::io::Result<Self> {
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if job.is_null() {
            return Err(std::io::Error::last_os_error());
        }

        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let limits_size = match u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>()) {
            Ok(size) => size,
            Err(_) => {
                unsafe {
                    CloseHandle(job);
                }
                return Err(std::io::Error::other("job limits size exceeds u32"));
            }
        };
        if unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                std::ptr::from_ref(&limits).cast(),
                limits_size,
            )
        } == 0
        {
            let error = std::io::Error::last_os_error();
            unsafe {
                CloseHandle(job);
            }
            return Err(error);
        }

        let Some(process) = process else {
            unsafe {
                CloseHandle(job);
            }
            return Err(std::io::Error::other(
                "status command has no process handle",
            ));
        };
        if unsafe { AssignProcessToJobObject(job, process.cast()) } == 0 {
            let error = std::io::Error::last_os_error();
            unsafe {
                CloseHandle(job);
            }
            return Err(error);
        }
        if let Err(error) = resume_suspended_process(process_id) {
            unsafe {
                CloseHandle(job);
            }
            return Err(error);
        }

        Ok(Self { job: job as usize })
    }
}

fn resume_suspended_process(process_id: Option<u32>) -> std::io::Result<()> {
    let process_id =
        process_id.ok_or_else(|| std::io::Error::other("status command has no process id"))?;
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }

    let result = (|| {
        let mut entry: THREADENTRY32 = unsafe { std::mem::zeroed() };
        entry.dwSize = u32::try_from(size_of::<THREADENTRY32>())
            .map_err(|_| std::io::Error::other("thread entry size exceeds u32"))?;
        if unsafe { Thread32First(snapshot, &mut entry) } == 0 {
            return Err(std::io::Error::last_os_error());
        }

        loop {
            if entry.th32OwnerProcessID == process_id {
                let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
                if thread.is_null() {
                    return Err(std::io::Error::last_os_error());
                }
                let resume_result = unsafe { ResumeThread(thread) };
                let resume_error = (resume_result == u32::MAX).then(std::io::Error::last_os_error);
                unsafe {
                    CloseHandle(thread);
                }
                if let Some(error) = resume_error {
                    return Err(error);
                }
                return Ok(());
            }
            if unsafe { Thread32Next(snapshot, &mut entry) } == 0 {
                return Err(std::io::Error::other(
                    "status command primary thread was not found",
                ));
            }
        }
    })();

    unsafe {
        CloseHandle(snapshot);
    }
    result
}

impl StatusCommandGuard {
    pub(crate) fn terminate(&mut self) {
        if self.job != 0 {
            // KILL_ON_JOB_CLOSE terminates the shell and every descendant still in
            // the job, including on task cancellation and config reload.
            unsafe {
                CloseHandle(self.job as HANDLE);
            }
            self.job = 0;
        }
    }
}

impl Drop for StatusCommandGuard {
    fn drop(&mut self) {
        self.terminate();
    }
}

fn detached_custom_command_process_with_comspec(
    command: &str,
    comspec: Option<std::ffi::OsString>,
) -> std::process::Command {
    use std::os::windows::process::CommandExt;

    let mut process = std::process::Command::new(raw_command_shell(comspec));
    process.arg("/d").arg("/c").raw_arg(command);
    process
}

pub(crate) fn pane_custom_command_pty_builder_platform(
    command: &str,
) -> portable_pty::CommandBuilder {
    pane_custom_command_pty_builder_with_comspec(command, std::env::var_os("ComSpec"))
}

fn pane_custom_command_pty_builder_with_comspec(
    command: &str,
    comspec: Option<std::ffi::OsString>,
) -> portable_pty::CommandBuilder {
    let mut builder = portable_pty::CommandBuilder::new(raw_command_shell(comspec));
    builder.arg("/d");
    builder.arg("/c");
    builder.raw_arg(command);
    builder
}

pub(crate) fn scrollback_editor_argv(path: &std::path::Path) -> std::io::Result<Vec<String>> {
    let editor = std::env::var("VISUAL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            std::env::var("EDITOR")
                .ok()
                .filter(|value| !value.trim().is_empty())
        });
    scrollback_editor_argv_with_env(path, editor.as_deref())
}

fn scrollback_editor_argv_with_env(
    path: &std::path::Path,
    editor: Option<&str>,
) -> std::io::Result<Vec<String>> {
    let mut argv = match editor.filter(|value| !value.trim().is_empty()) {
        Some(editor) => command_line_to_argv(editor).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("failed to parse editor command {editor:?}"),
            )
        })?,
        None => vec!["notepad.exe".to_string()],
    };
    if argv.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "editor command must not be empty",
        ));
    }
    argv.push(path.display().to_string());
    Ok(argv)
}

pub(crate) fn configure_background_command_platform(command: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;

    command.creation_flags(CREATE_NO_WINDOW);
}

pub fn launch_server_daemon_command(command: &mut std::process::Command) -> std::io::Result<u32> {
    if current_job_kills_processes_on_close()? {
        launch_server_daemon_with_wmi(command)
    } else {
        command.spawn().map(|child| child.id())
    }
}

fn launch_server_daemon_with_wmi(command: &std::process::Command) -> std::io::Result<u32> {
    // WMI resolves the class from this Rust type name, including CIM casing.
    #[allow(non_camel_case_types)]
    #[derive(serde::Deserialize)]
    struct Win32_Process;

    // WMI serializes this embedded object using the matching CIM class name.
    #[allow(non_camel_case_types)]
    #[derive(serde::Serialize)]
    struct Win32_ProcessStartup {
        #[serde(rename = "CreateFlags")]
        create_flags: u32,
        #[serde(rename = "EnvironmentVariables")]
        environment_variables: Vec<String>,
    }

    #[derive(serde::Serialize)]
    struct CreateInput {
        #[serde(rename = "CommandLine")]
        command_line: String,
        #[serde(rename = "CurrentDirectory")]
        current_directory: String,
        #[serde(rename = "ProcessStartupInformation")]
        process_startup_information: Win32_ProcessStartup,
    }

    #[derive(serde::Deserialize)]
    struct CreateOutput {
        #[serde(rename = "ProcessId")]
        process_id: Option<u32>,
        #[serde(rename = "ReturnValue")]
        return_value: u32,
    }

    let current_directory = command
        .get_current_dir()
        .map(std::path::Path::to_path_buf)
        .map(Ok)
        .unwrap_or_else(std::env::current_dir)?;
    let input = CreateInput {
        command_line: windows_command_line(command)?,
        current_directory: unicode_windows_value(
            &current_directory.into_os_string(),
            "working directory",
        )?,
        process_startup_information: Win32_ProcessStartup {
            create_flags: DETACHED_PROCESS,
            environment_variables: effective_command_environment(command)?,
        },
    };

    let connection = wmi::WMIConnection::new()
        .map_err(|err| std::io::Error::other(format!("failed to connect to WMI: {err}")))?;
    let output: CreateOutput = connection
        .exec_class_method::<Win32_Process, _>("Create", &input)
        .map_err(|err| std::io::Error::other(format!("WMI Win32_Process.Create failed: {err}")))?;
    if output.return_value != 0 {
        return Err(std::io::Error::other(format!(
            "WMI Win32_Process.Create returned error {}",
            output.return_value
        )));
    }
    output.process_id.ok_or_else(|| {
        std::io::Error::other("WMI Win32_Process.Create succeeded without a process id")
    })
}

fn windows_command_line(command: &std::process::Command) -> std::io::Result<String> {
    std::iter::once(command.get_program())
        .chain(command.get_args())
        .map(|value| {
            unicode_windows_value(value, "server command argument")
                .map(|value| super::quote_windows_command_line_arg(&value))
        })
        .collect::<std::io::Result<Vec<_>>>()
        .map(|parts| parts.join(" "))
}

fn effective_command_environment(command: &std::process::Command) -> std::io::Result<Vec<String>> {
    let mut environment = std::env::vars_os()
        .map(|(key, value)| {
            Ok((
                unicode_windows_value(&key, "inherited environment variable name")?,
                unicode_windows_value(&value, "inherited environment variable value")?,
            ))
        })
        .collect::<std::io::Result<Vec<(String, String)>>>()?;
    for (key, value) in command.get_envs() {
        let key = unicode_windows_value(key, "environment variable name")?;
        environment.retain(|(inherited, _)| windows_environment_key_cmp(inherited, &key).is_ne());
        if let Some(value) = value {
            environment.push((
                key,
                unicode_windows_value(value, "environment variable value")?,
            ));
        }
    }
    environment.sort_unstable_by(|(left, _), (right, _)| windows_environment_key_cmp(left, right));
    Ok(environment
        .into_iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect())
}

fn windows_environment_key_cmp(left: &str, right: &str) -> Ordering {
    let left_wide: Vec<u16> = left.encode_utf16().collect();
    let right_wide: Vec<u16> = right.encode_utf16().collect();
    // SAFETY: both pointers remain valid for the call and lengths count UTF-16 units.
    match unsafe {
        CompareStringOrdinal(
            left_wide.as_ptr(),
            left_wide.len() as i32,
            right_wide.as_ptr(),
            right_wide.len() as i32,
            1,
        )
    } {
        CSTR_LESS_THAN => Ordering::Less,
        CSTR_EQUAL => Ordering::Equal,
        CSTR_GREATER_THAN => Ordering::Greater,
        _ => left.cmp(right),
    }
}

fn unicode_windows_value(value: &OsStr, label: &str) -> std::io::Result<String> {
    value.to_str().map(str::to_owned).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{label} is not valid Unicode"),
        )
    })
}

fn current_process_is_in_job() -> std::io::Result<bool> {
    let mut in_job = 0;
    // SAFETY: `in_job` is a valid writable BOOL for the duration of the call.
    if unsafe { IsProcessInJob(GetCurrentProcess(), null_mut(), &mut in_job) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(in_job != 0)
}

fn current_job_kills_processes_on_close() -> std::io::Result<bool> {
    if !current_process_is_in_job()? {
        return Ok(false);
    }

    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    // SAFETY: `limits` is writable and its exact buffer size is supplied.
    if unsafe {
        QueryInformationJobObject(
            null_mut(),
            JobObjectExtendedLimitInformation,
            &mut limits as *mut _ as *mut c_void,
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            null_mut(),
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(limits.BasicLimitInformation.LimitFlags & JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE != 0)
}

pub fn detach_server_daemon_command(command: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;

    command.creation_flags(DETACHED_PROCESS);
}

pub fn current_process_is_detached_server_daemon() -> bool {
    if !unsafe { GetConsoleWindow() }.is_null() {
        return false;
    }

    // Job membership alone does not tie the daemon lifetime to its launcher.
    matches!(current_job_kills_processes_on_close(), Ok(false))
}

/// 本进程挂着的 server daemon 标记，握到进程退出（见 [`announce_detached_server_daemon`]）。
static SERVER_DAEMON_MARKER: OnceLock<OwnedHandle> = OnceLock::new();

/// 脱离控制台的 server daemon 在整个生命周期里挂一个命名事件，名字带 pid 与进程创建时间。
///
/// 普通启动时 daemon 的父进程就是拉起它的客户端：客户端若跑在某个 pane 里，daemon 就挂在这个
/// pane 的进程树下，关 pane 时会连同它下面整个嵌套会话一起被终止。Unix 的 daemon 用 setsid
/// 脱离会话，不受影响；这里用标记让 pane 进程树的枚举认出 daemon、把它连同子树留下。daemon
/// 退出时内核关掉句柄、标记随之消失；名字带创建时间，pid 被复用后也对不上，别人也无法替未来的
/// daemon 预先占名。
pub(crate) fn announce_detached_server_daemon() {
    if !current_process_is_detached_server_daemon() {
        return;
    }
    let Some(instance) = process_creation_time(unsafe { GetCurrentProcess() }) else {
        tracing::warn!(
            err = %std::io::Error::last_os_error(),
            "server daemon could not read its creation time; pane trees may still contain it"
        );
        return;
    };
    let name = server_daemon_marker_name(ProcessSessionMember {
        pid: std::process::id(),
        instance,
    });
    let marker = unsafe { CreateEventW(std::ptr::null(), 1, 0, name.as_ptr()) };
    if marker.is_null() {
        tracing::warn!(
            err = %std::io::Error::last_os_error(),
            "server daemon could not announce itself; closing the pane that started it may stop it"
        );
        return;
    }
    let _ = SERVER_DAEMON_MARKER.set(unsafe { OwnedHandle::from_raw_handle(marker) });
}

fn server_daemon_marker_name(member: ProcessSessionMember) -> Vec<u16> {
    format!(
        "Local\\herdr-server-daemon-{}-{}",
        member.pid, member.instance
    )
    .encode_utf16()
    .chain(std::iter::once(0))
    .collect()
}

/// 这个（已核对身份的）进程是否挂着 server daemon 标记。只有「名字不存在」才算没有：拒绝访问
/// （daemon 以别的用户或更高权限运行）等错误按存在处理，多留一个进程的代价远小于误杀 daemon
/// 丢掉整个嵌套会话。
fn server_daemon_marker_exists(member: ProcessSessionMember) -> bool {
    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND};

    let name = server_daemon_marker_name(member);
    let marker = unsafe { OpenEventW(SYNCHRONIZATION_SYNCHRONIZE, 0, name.as_ptr()) };
    if !marker.is_null() {
        drop(unsafe { OwnedHandle::from_raw_handle(marker) });
        return true;
    }
    let error = std::io::Error::last_os_error().raw_os_error();
    let missing = [ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND]
        .iter()
        .any(|&code| error == i32::try_from(code).ok());
    !missing
}

pub fn foreground_job(child_pid: u32) -> Option<ForegroundJob> {
    select_pane_foreground_job_cached(child_pid)
}

pub(crate) fn available_pane_shell(child_pid: u32) -> Option<String> {
    let snapshot = ProcessSnapshot::new(snapshot_processes());
    available_pane_shell_from_snapshot(child_pid, &snapshot)
}

/// Periodic detection-loop check: the snapshot shared with other panes may show the shell still
/// busy, but an apparently idle shell is confirmed against a fresh snapshot, so a command that
/// started after the shared snapshot is never missed. The confirming snapshot replaces the
/// shared one, so the other panes of the same round reuse it.
pub(crate) fn pane_shell_is_idle(child_pid: u32) -> bool {
    pane_shell_is_idle_in(
        child_pid,
        &FOREGROUND_PROCESS_SNAPSHOT_CACHE,
        snapshot_processes,
    )
}

fn pane_shell_is_idle_in(
    child_pid: u32,
    cache: &Mutex<ProcessSnapshotCache>,
    build: impl Fn() -> Vec<WindowsProcessEntry>,
) -> bool {
    let snapshot = |max_age| {
        cache
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .snapshot(max_age, &build)
    };
    available_pane_shell_from_snapshot(child_pid, &snapshot(FOREGROUND_PROCESS_SNAPSHOT_CACHE_TTL))
        .is_some()
        && available_pane_shell_from_snapshot(child_pid, &snapshot(Duration::ZERO)).is_some()
}

fn available_pane_shell_from_snapshot(
    child_pid: u32,
    snapshot: &ProcessSnapshot,
) -> Option<String> {
    let shell = snapshot.entry(child_pid)?.observation()?;
    if !super::is_pane_shell_process_name(shell.name()) {
        return None;
    }
    let busy_or_unknown = snapshot.child_pids(child_pid).any(|pid| {
        snapshot.entry(pid).is_none_or(|child| {
            child.observation().is_none_or(|observation| {
                observation.parent_pid.is_none_or(|parent| {
                    parent == child_pid && observation.created >= shell.created
                })
            })
        })
    });
    (!busy_or_unknown).then(|| shell.name().to_owned())
}

pub fn foreground_group_leader_job(process_group_id: u32) -> Option<ForegroundJob> {
    let snapshot = cached_foreground_processes();
    let entry = snapshot.entry(process_group_id)?;
    Some(ForegroundJob {
        process_group_id,
        processes: vec![foreground_process_from_entry(entry)],
    })
}

pub fn foreground_process_group_id(child_pid: u32) -> Option<u32> {
    select_pane_foreground_job_cached(child_pid).map(|job| job.process_group_id)
}

pub fn process_cwd(pid: u32) -> Option<PathBuf> {
    let process = ProcessHandle::open(pid, PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ)?;
    process_cwd_from_handle(process.0)
}

pub(crate) fn pane_process_cwd(child_pid: u32) -> Option<PathBuf> {
    let process = ProcessHandle::open(
        child_pid,
        PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ,
    )?;
    let executable = PathBuf::from(process_executable_path(process.0)?);
    if executable.with_extension("shim").is_file() {
        // A Scoop shim keeps its launch cwd while the real shell can change directories.
        // Never replace the saved shell directory with that launcher directory on exit.
        let snapshot = cached_foreground_processes();
        let parent = snapshot.entry(child_pid)?.observation()?;
        if !process_matches_cwd_observation(process.0, parent) {
            return None;
        }
        let shell = shim_shell_entry(child_pid, &snapshot)?;
        let cwd = process_cwd_for_observation(shell.pid, shell.observation()?)?;
        return parent.identity.running().then_some(cwd);
    }
    process_cwd_from_handle(process.0)
}

fn process_cwd_from_handle(process: HANDLE) -> Option<PathBuf> {
    read_unicode_string(process, process_cwd_descriptor(process)?)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

fn process_cwd_descriptor(process: HANDLE) -> Option<UNICODE_STRING> {
    #[cfg(target_pointer_width = "64")]
    {
        let mut peb32_address = 0_usize;
        // SAFETY: the output is a writable ULONG_PTR with its exact buffer size.
        let status = unsafe {
            NtQueryInformationProcess(
                process,
                ProcessWow64Information,
                (&mut peb32_address as *mut usize).cast(),
                size_of::<usize>() as u32,
                null_mut(),
            )
        };
        if status != STATUS_SUCCESS as NTSTATUS {
            return None;
        }
        if peb32_address != 0 {
            // WoW64's native PEB can report C:\Windows while the x86 shell uses
            // another directory. Read its own pointer-width layout instead.
            let peb = read_process_value::<Peb32>(process, peb32_address as *const c_void)?;
            let parameters = read_process_value::<ProcessCwdParameters32>(
                process,
                peb.process_parameters as usize as *const c_void,
            )?;
            let cwd = parameters.current_directory;
            return Some(UNICODE_STRING {
                Length: cwd.Length,
                MaximumLength: cwd.MaximumLength,
                Buffer: cwd.Buffer as usize as *mut u16,
            });
        }
    }
    Some(read_process_parameters(process)?.current_directory.dos_path)
}

fn process_matches_cwd_observation(process: HANDLE, observation: &ProcessObservation) -> bool {
    observation.identity.running()
        && process_creation_time(process) == Some(observation.created)
        && observation.parent_pid.is_some()
        && process_basic_information(process)
            .and_then(|basic| u32::try_from(basic.InheritedFromUniqueProcessId).ok())
            == observation.parent_pid
        && observation.image.is_some()
        && process_executable_path(process) == observation.image
}

fn process_cwd_for_observation(pid: u32, observation: &ProcessObservation) -> Option<PathBuf> {
    let process = ProcessHandle::open(pid, PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ)?;
    if !process_matches_cwd_observation(process.0, observation) {
        return None;
    }
    let cwd = process_cwd_from_handle(process.0)?;
    observation.identity.running().then_some(cwd)
}

fn shim_shell_entry(child_pid: u32, snapshot: &ProcessSnapshot) -> Option<&WindowsProcessEntry> {
    let parent = snapshot.entry(child_pid)?;
    if !parent.observation()?.identity.running() {
        return None;
    }
    let mut shell = None;
    for &index in snapshot.children_by_parent.get(&child_pid)? {
        let entry = &snapshot.entries[index];
        let observation = entry.observation()?;
        if !observation.identity.running()
            || !verified_parent_child(parent, entry)
            || observation.name().is_empty()
        {
            return None;
        }
        if super::is_pane_shell_process_name(observation.name()) {
            if shell.is_some() {
                return None;
            }
            shell = Some(entry);
        }
    }
    shell
}

fn select_pane_foreground_job_cached(shell_pid: u32) -> Option<ForegroundJob> {
    let snapshot = cached_foreground_processes();
    let (job, retry_with_fresh_snapshot) =
        select_pane_foreground_job_from_snapshot(shell_pid, &snapshot)?;
    if !retry_with_fresh_snapshot {
        return Some(job);
    }

    let snapshot = fresh_foreground_processes();
    select_pane_foreground_job_from_snapshot(shell_pid, &snapshot).map(|(job, _)| job)
}

fn select_pane_foreground_job_from_snapshot(
    shell_pid: u32,
    snapshot: &ProcessSnapshot,
) -> Option<(ForegroundJob, bool)> {
    if let Some(job) = FOREGROUND_SELECTION_CACHE
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .get(shell_pid, snapshot)
    {
        return Some((job, false));
    }

    let job = select_pane_foreground_job_from_snapshot_uncached(shell_pid, snapshot)?;
    let cached = prepare_cached_foreground_selection(shell_pid, snapshot, &job);
    let retry_with_fresh_snapshot = job.process_group_id != shell_pid && cached.is_none();
    FOREGROUND_SELECTION_CACHE
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .remember(shell_pid, cached);
    Some((job, retry_with_fresh_snapshot))
}

fn select_pane_foreground_job_from_snapshot_uncached(
    shell_pid: u32,
    snapshot: &ProcessSnapshot,
) -> Option<ForegroundJob> {
    select_pane_foreground_job_from_snapshot_with_runtime_inspection(
        shell_pid,
        snapshot,
        process_is_git_bash,
        process_runtime_marker,
    )
}

fn select_pane_foreground_job_from_snapshot_with_runtime_inspection(
    shell_pid: u32,
    snapshot: &ProcessSnapshot,
    shell_is_git_bash: impl FnOnce(&WindowsProcessEntry) -> bool,
    mut runtime_marker: impl FnMut(&WindowsProcessEntry) -> Option<String>,
) -> Option<ForegroundJob> {
    let entries = &snapshot.entries;
    let shell = snapshot.entry(shell_pid)?;
    let descendants = descendant_entries(shell_pid, snapshot);
    let mut candidates = Vec::new();
    for entry in std::iter::once(shell).chain(descendants) {
        if process_entry_identifies_agent(entry) {
            candidates.push(entry);
        }
    }

    if let Some(selected) = select_topmost_agent_chain_candidate(&candidates, snapshot) {
        return Some(foreground_job_from_entry(selected));
    }
    if !candidates.is_empty() || !shell_is_git_bash(shell) {
        return Some(foreground_job_from_entry(shell));
    }

    let escaped_agent_indices = snapshot.agent_indices();
    if escaped_agent_indices.is_empty() {
        return Some(foreground_job_from_entry(shell));
    }

    let Some(shell_runtime_marker) = pane_runtime_marker(shell, &mut runtime_marker) else {
        return Some(foreground_job_from_entry(shell));
    };
    let matching_candidates: Vec<_> = escaped_agent_indices
        .iter()
        .map(|&index| &entries[index])
        .filter(|entry| {
            carries_pane_runtime_marker(entry, &shell_runtime_marker, &mut runtime_marker)
        })
        .collect();
    let selected =
        select_topmost_agent_chain_candidate(&matching_candidates, snapshot).unwrap_or(shell);
    Some(foreground_job_from_entry(selected))
}

#[cfg(test)]
fn select_pane_foreground_job(
    shell_pid: u32,
    entries: &[WindowsProcessEntry],
) -> Option<ForegroundJob> {
    select_pane_foreground_job_from_snapshot_uncached(
        shell_pid,
        &ProcessSnapshot::new(entries.to_vec()),
    )
}

/// Git Bash can start agents outside the pane shell's process tree. Those
/// belong to the pane when they carry the runtime marker the shell got.
fn pane_runtime_marker(
    shell: &WindowsProcessEntry,
    runtime_marker: &mut impl FnMut(&WindowsProcessEntry) -> Option<String>,
) -> Option<String> {
    runtime_marker(shell).filter(|marker| !marker.is_empty())
}

fn carries_pane_runtime_marker(
    entry: &WindowsProcessEntry,
    pane_marker: &str,
    runtime_marker: &mut impl FnMut(&WindowsProcessEntry) -> Option<String>,
) -> bool {
    runtime_marker(entry).as_deref() == Some(pane_marker)
}

/// Whether foreground selection could pick `pid` for this pane: a descendant
/// of the pane shell, or an escaped Git Bash agent with the pane's marker.
fn process_belongs_to_pane(
    shell_pid: u32,
    pid: u32,
    snapshot: &ProcessSnapshot,
    shell_is_git_bash: impl FnOnce(&WindowsProcessEntry) -> bool,
    mut runtime_marker: impl FnMut(&WindowsProcessEntry) -> Option<String>,
) -> bool {
    if process_is_ancestor(shell_pid, pid, snapshot) {
        return true;
    }
    let (Some(shell), Some(entry)) = (snapshot.entry(shell_pid), snapshot.entry(pid)) else {
        return false;
    };
    shell_is_git_bash(shell)
        && process_entry_identifies_agent(entry)
        && pane_runtime_marker(shell, &mut runtime_marker).is_some_and(|pane_marker| {
            carries_pane_runtime_marker(entry, &pane_marker, &mut runtime_marker)
        })
}

/// Creation time of `pid`. It tells a process apart from a later one that
/// reuses its pid.
pub fn process_start_token(pid: u32) -> Option<u64> {
    let snapshot = cached_foreground_processes();
    let observation = snapshot.entry(pid)?.observation()?;
    observation
        .identity
        .running()
        .then_some(observation.created)
}

/// Returns `pid` while that same process, matched by its creation time, is
/// still running for the pane shell `shell_pid`. Windows has no job control,
/// so the process stands in for its own group.
pub fn live_pane_process_group(shell_pid: u32, pid: u32, start_token: u64) -> Option<u32> {
    live_pane_process_group_from_snapshot(
        shell_pid,
        pid,
        start_token,
        &cached_foreground_processes(),
        process_is_git_bash,
        process_runtime_marker,
    )
}

fn live_pane_process_group_from_snapshot(
    shell_pid: u32,
    pid: u32,
    start_token: u64,
    snapshot: &ProcessSnapshot,
    shell_is_git_bash: impl FnOnce(&WindowsProcessEntry) -> bool,
    runtime_marker: impl FnMut(&WindowsProcessEntry) -> Option<String>,
) -> Option<u32> {
    let shell = snapshot.entry(shell_pid)?.observation()?;
    let process = snapshot.entry(pid)?.observation()?;
    if !shell.identity.running() || !process.identity.running() || process.created != start_token {
        return None;
    }
    process_belongs_to_pane(shell_pid, pid, snapshot, shell_is_git_bash, runtime_marker)
        .then_some(pid)
}

fn process_entry_identifies_agent(entry: &WindowsProcessEntry) -> bool {
    if crate::detect::identify_agent(entry.observed_name()).is_some() {
        return true;
    }
    let Some(creation_time) = entry.creation_time() else {
        return process_command_identifies_agent(entry);
    };
    if let Some(identifies_agent) = cached_agent_classification(entry.pid, creation_time) {
        return identifies_agent;
    }
    let identifies_agent = process_command_identifies_agent(entry);
    let command = entry.command();
    if command.creation_time == Some(creation_time) {
        remember_agent_classification(
            entry.pid,
            creation_time,
            identifies_agent,
            command.cmdline.is_some(),
        );
    }
    identifies_agent
}

fn process_command_identifies_agent(entry: &WindowsProcessEntry) -> bool {
    crate::detect::identify_agent_in_job(&foreground_job_from_entry(entry)).is_some()
}

fn foreground_job_from_entry(entry: &WindowsProcessEntry) -> ForegroundJob {
    ForegroundJob {
        process_group_id: entry.pid,
        processes: vec![foreground_process_from_entry(entry)],
    }
}

fn select_topmost_agent_chain_candidate<'a>(
    candidates: &[&'a WindowsProcessEntry],
    snapshot: &ProcessSnapshot,
) -> Option<&'a WindowsProcessEntry> {
    if candidates.is_empty() {
        return None;
    }

    candidates.iter().copied().find(|entry| {
        candidates.iter().all(|other| {
            entry.pid == other.pid || process_is_ancestor(entry.pid, other.pid, snapshot)
        })
    })
}

fn verified_parent_child(parent: &WindowsProcessEntry, child: &WindowsProcessEntry) -> bool {
    let Some(parent_observation) = parent.observation() else {
        return false;
    };
    parent.pid == child.parent_pid
        && child.observation().is_some_and(|child_observation| {
            child_observation.parent_pid == Some(parent.pid)
                && parent_observation.created <= child_observation.created
        })
}

fn process_is_ancestor(ancestor_pid: u32, descendant_pid: u32, snapshot: &ProcessSnapshot) -> bool {
    let mut current = descendant_pid;
    let mut visited = HashSet::new();
    while visited.insert(current) {
        let Some(child) = snapshot.entry(current) else {
            return false;
        };
        let Some(parent) = snapshot.entry(child.parent_pid) else {
            return false;
        };
        if !verified_parent_child(parent, child) {
            return false;
        }
        if parent.pid == ancestor_pid {
            return true;
        }
        current = parent.pid;
    }

    false
}

fn descendant_entries(root_pid: u32, snapshot: &ProcessSnapshot) -> Vec<&WindowsProcessEntry> {
    let mut output = Vec::new();
    let Some(root) = snapshot.entry(root_pid) else {
        return output;
    };
    let mut queue = VecDeque::from([root]);
    let mut visited = HashSet::from([root_pid]);
    while let Some(parent) = queue.pop_front() {
        if let Some(next) = snapshot.children_by_parent.get(&parent.pid) {
            for &index in next {
                let child = &snapshot.entries[index];
                if visited.insert(child.pid) && verified_parent_child(parent, child) {
                    output.push(child);
                    queue.push_back(child);
                }
            }
        }
    }
    output
}

fn foreground_process_from_entry(entry: &WindowsProcessEntry) -> super::ForegroundProcess {
    let command = entry.command();
    super::ForegroundProcess {
        pid: entry.pid,
        name: entry.observed_name().to_owned(),
        argv0: command.argv0.clone(),
        argv: command.argv.clone(),
        cmdline: command.cmdline.clone(),
    }
}

pub(super) fn snapshot_multiplexer_client_lineages(
    peer: &ProcessLineage,
) -> Option<Vec<ProcessLineage>> {
    snapshot_multiplexer_client_lineages_with(peer, cached_foreground_processes)
}

fn snapshot_multiplexer_client_lineages_with(
    peer: &ProcessLineage,
    snapshot: impl FnOnce() -> Arc<ProcessSnapshot>,
) -> Option<Vec<ProcessLineage>> {
    super::process_lineage::multiplexer_client_lineages_matching(peer, |matches| {
        let snapshot = snapshot();
        if snapshot.entries.is_empty() {
            return None;
        }
        snapshot
            .entries
            .iter()
            .filter(|entry| matches(entry.pid, &entry.name))
            .map(|entry| {
                matches(entry.pid, entry.observation()?.name())
                    .then(|| process_lineage_from_snapshot(entry.pid, &snapshot))?
            })
            .collect()
    })
}

/// 复用 Toolhelp 快照并核验父子创建时间；身份未知或父 PID 已复用时截断为不完整的链。
pub(crate) fn process_lineage(pid: u32) -> Option<ProcessLineage> {
    process_lineage_from_snapshot(pid, &cached_foreground_processes())
}

fn process_lineage_from_snapshot(pid: u32, snapshot: &ProcessSnapshot) -> Option<ProcessLineage> {
    let mut child_created = None;
    super::walk_process_lineage(pid, |pid| {
        let entry = snapshot.entry(pid)?;
        let _parent_pin = snapshot
            .entry(entry.parent_pid)
            .and_then(WindowsProcessEntry::observation);
        let observation = entry.observation()?;
        if observation.parent_pid != Some(entry.parent_pid)
            || child_created.is_some_and(|child| observation.created > child)
        {
            return None;
        }
        child_created = Some(observation.created);
        Some(ProcessParentEntry {
            pid: entry.pid,
            parent_pid: entry.parent_pid,
            name: observation.name().to_owned(),
        })
    })
}

/// 命名管道对端（客户端）进程的 pid（`GetNamedPipeClientProcessId`，经 interprocess 的
/// `peer_creds`）。
pub(crate) fn peer_process_id(stream: &crate::ipc::LocalStream) -> Option<u32> {
    use interprocess::local_socket::traits::StreamCommon as _;

    stream.peer_creds().ok()?.pid().filter(|pid| *pid != 0)
}

fn snapshot_processes() -> Vec<WindowsProcessEntry> {
    #[cfg(test)]
    PROCESS_INSPECTION_COUNTS.with_borrow_mut(|counts| counts.snapshots += 1);
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Vec::new();
    }
    let _snapshot = ProcessHandle(snapshot);

    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut output = Vec::new();
    let mut ok = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
    while ok {
        output.push(WindowsProcessEntry::new(
            entry.th32ProcessID,
            entry.th32ParentProcessID,
            nul_terminated_utf16_to_string(&entry.szExeFile),
        ));
        ok = unsafe { Process32NextW(snapshot, &mut entry) } != 0;
    }
    output
}

fn cached_foreground_processes() -> Arc<ProcessSnapshot> {
    let mut cache = FOREGROUND_PROCESS_SNAPSHOT_CACHE
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    cache.snapshot(FOREGROUND_PROCESS_SNAPSHOT_CACHE_TTL, snapshot_processes)
}

fn fresh_foreground_processes() -> Arc<ProcessSnapshot> {
    let mut cache = FOREGROUND_PROCESS_SNAPSHOT_CACHE
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    cache.snapshot(Duration::ZERO, snapshot_processes)
}

fn prepare_cached_foreground_selection(
    shell_pid: u32,
    snapshot: &ProcessSnapshot,
    job: &ForegroundJob,
) -> Option<CachedForegroundSelection> {
    if !CachedForegroundSelection::can_cache(shell_pid, snapshot, job) {
        return None;
    }
    let identity = |pid| Some(snapshot.entry(pid)?.observation()?.identity.clone());
    let shell_identity = identity(shell_pid)?;
    let selected_identity = identity(job.process_group_id)?;
    let descendants = snapshot.descendant_signatures(shell_pid);
    let descendant_identities = descendants
        .iter()
        .map(|entry| identity(entry.pid))
        .collect::<Option<Vec<_>>>()?;
    CachedForegroundSelection::from_snapshot_with_identities(
        shell_pid,
        snapshot,
        job,
        descendants,
        descendant_identities,
        shell_identity,
        selected_identity,
    )
}

impl CachedForegroundSelection {
    fn can_cache(shell_pid: u32, snapshot: &ProcessSnapshot, job: &ForegroundJob) -> bool {
        // Idle native shells cannot use Git Bash's escaped-agent fallback.
        // Keep reevaluating unknown children; a first child invalidates topology.
        job.process_group_id != shell_pid
            || (snapshot.entry(shell_pid).is_some_and(|shell| {
                ["cmd.exe", "powershell.exe", "pwsh.exe"]
                    .iter()
                    .any(|name| shell.observed_name().eq_ignore_ascii_case(name))
            }) && !snapshot.children_by_parent.contains_key(&shell_pid))
    }

    fn from_snapshot_with_identities(
        shell_pid: u32,
        snapshot: &ProcessSnapshot,
        job: &ForegroundJob,
        descendants: Vec<ProcessSignature>,
        descendant_identities: Vec<ProcessIdentity>,
        shell_identity: ProcessIdentity,
        selected_identity: ProcessIdentity,
    ) -> Option<Self> {
        if !Self::can_cache(shell_pid, snapshot, job) {
            return None;
        }
        let shell_entry = snapshot.entry(shell_pid)?;
        if !shell_identity.matches_observation(shell_entry.observation()?) {
            return None;
        }
        let shell = ProcessSignature::from_entry(shell_entry);
        let selected_entry = snapshot.entry(job.process_group_id)?;
        if !selected_identity.matches_observation(selected_entry.observation()?) {
            return None;
        }
        let selected = ProcessSignature::from_entry(selected_entry);
        let descendants_match_identities = descendants.len() == descendant_identities.len()
            && descendants
                .iter()
                .zip(&descendant_identities)
                .all(|(signature, identity)| {
                    snapshot
                        .entry(signature.pid)
                        .and_then(WindowsProcessEntry::observation)
                        .is_some_and(|observation| identity.matches_observation(observation))
                });
        if !descendants_match_identities
            || !shell_identity.running()
            || !selected_identity.running()
            || !descendant_identities.iter().all(ProcessIdentity::running)
        {
            return None;
        }
        let observations = std::iter::once(&shell)
            .chain(std::iter::once(&selected))
            .chain(&descendants)
            .map(|signature| {
                Some((
                    signature.clone(),
                    Arc::clone(snapshot.entry(signature.pid)?.observation()?),
                ))
            })
            .collect::<Option<Vec<_>>>()?;
        let now = Instant::now();
        Some(Self {
            shell,
            selected,
            descendants,
            descendant_identities,
            shell_identity,
            selected_identity,
            observations,
            job: job.clone(),
            verified_at: now,
            last_used: now,
        })
    }
}

impl ForegroundSelectionCache {
    fn get(&mut self, shell_pid: u32, snapshot: &ProcessSnapshot) -> Option<ForegroundJob> {
        if let Some(cached) = self.entries.get_mut(&shell_pid) {
            let pinned = cached.verified_at.elapsed() < FOREGROUND_SELECTION_RECHECK
                && cached.shell_identity.running()
                && cached.selected_identity.running()
                && cached
                    .descendant_identities
                    .iter()
                    .all(ProcessIdentity::running)
                && cached.shell.matches(snapshot.entry(shell_pid))
                && cached
                    .selected
                    .matches(snapshot.entry(cached.job.process_group_id))
                && cached
                    .descendants
                    .iter()
                    .all(|entry| entry.matches(snapshot.entry(entry.pid)));
            let metadata_matches = pinned
                && cached.observations.iter().all(|(signature, observation)| {
                    snapshot.entry(signature.pid).is_some_and(|entry| {
                        entry.observation.get().is_none_or(|current| {
                            current
                                .as_ref()
                                .is_some_and(|current| current.same_metadata(observation))
                        })
                    })
                });
            if metadata_matches {
                // Share independently verified live pins; a cache hit still requires matching topology.
                let observations_shared =
                    cached.observations.iter().all(|(signature, observation)| {
                        snapshot.entry(signature.pid).is_some_and(|entry| {
                            entry
                                .observation
                                .get_or_init(|| Some(Arc::clone(observation)))
                                .as_ref()
                                .is_some_and(|current| current.same_metadata(observation))
                        })
                    });
                if observations_shared {
                    let mut descendants = descendant_entries(shell_pid, snapshot);
                    descendants.sort_unstable_by_key(|entry| entry.pid);
                    let topology_matches = descendants
                        .iter()
                        .map(|entry| entry.pid)
                        .eq(cached.descendants.iter().map(|entry| entry.pid));
                    if topology_matches {
                        cached.last_used = Instant::now();
                        return Some(cached.job.clone());
                    }
                }
            }
        }
        self.entries.remove(&shell_pid);
        None
    }

    fn remember(&mut self, shell_pid: u32, cached: Option<CachedForegroundSelection>) {
        let Some(cached) = cached else {
            self.entries.remove(&shell_pid);
            return;
        };
        self.entries
            .retain(|_, cached| cached.last_used.elapsed() < FOREGROUND_SELECTION_CACHE_RETENTION);
        if self.entries.len() >= FOREGROUND_SELECTION_CACHE_CAPACITY {
            self.entries.clear();
        }
        self.entries.insert(shell_pid, cached);
    }

    #[cfg(test)]
    fn remember_for_test(
        &mut self,
        shell_pid: u32,
        snapshot: &ProcessSnapshot,
        job: &ForegroundJob,
    ) {
        let identity = |pid| ProcessIdentity::Stub {
            running: true,
            creation_time: snapshot
                .entry(pid)
                .and_then(WindowsProcessEntry::creation_time),
        };
        let descendants = snapshot.descendant_signatures(shell_pid);
        let descendant_identities = descendants
            .iter()
            .map(|entry| identity(entry.pid))
            .collect();
        let cached = CachedForegroundSelection::from_snapshot_with_identities(
            shell_pid,
            snapshot,
            job,
            descendants,
            descendant_identities,
            identity(shell_pid),
            identity(job.process_group_id),
        );
        self.remember(shell_pid, cached);
    }
}

impl ProcessSnapshotCache {
    fn snapshot(
        &mut self,
        max_age: Duration,
        build: impl FnOnce() -> Vec<WindowsProcessEntry>,
    ) -> Arc<ProcessSnapshot> {
        if let Some(cached) = &self.cached {
            if cached.built_at.elapsed() < max_age {
                return Arc::clone(&cached.snapshot);
            }
        }

        let snapshot = Arc::new(ProcessSnapshot::new(build()));
        self.cached = Some(CachedProcessSnapshot {
            built_at: Instant::now(),
            snapshot: Arc::clone(&snapshot),
        });
        snapshot
    }
}

fn read_process_command(pid: u32, observation: &ProcessObservation) -> WindowsProcessCommand {
    let cmdline = observation
        .identity
        .handle()
        .and_then(read_process_command_line)
        .or_else(|| read_process_command_fallback(pid, observation));
    WindowsProcessCommand::from_cmdline(observation.name(), Some(observation.created), cmdline)
}

fn read_process_command_fallback(pid: u32, observation: &ProcessObservation) -> Option<String> {
    let process = ProcessHandle::open(pid, PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ)?;
    if process_creation_time(process.0) != Some(observation.created) {
        return None;
    }
    read_process_parameters(process.0)
        .and_then(|parameters| read_unicode_string(process.0, parameters.command_line))
}

impl WindowsProcessCommand {
    fn from_cmdline(name: &str, creation_time: Option<u64>, cmdline: Option<String>) -> Self {
        let argv = cmdline.as_deref().and_then(command_line_to_argv);
        let argv0 = argv
            .as_ref()
            .and_then(|argv| argv.first().cloned())
            .or_else(|| (!name.is_empty()).then(|| name.to_string()));
        Self {
            creation_time,
            argv0,
            argv,
            cmdline,
        }
    }
}

fn process_is_git_bash(entry: &WindowsProcessEntry) -> bool {
    let Some(observation) = entry.observation() else {
        return false;
    };
    let pid = entry.pid;
    let creation_time = observation.created;
    {
        let mut cache = GIT_BASH_PROCESS_CACHE
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        if let Some(cached) = cache.get_mut(&pid) {
            if cached.creation_time == creation_time {
                cached.last_used = Instant::now();
                return cached.is_git_bash;
            }
        }
    }

    let is_git_bash = observation
        .image
        .as_deref()
        .is_some_and(|path| is_git_bash_executable_path(std::path::Path::new(path)));
    let mut cache = GIT_BASH_PROCESS_CACHE
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    if cache.len() >= PROCESS_RUNTIME_MARKER_CACHE_CAPACITY {
        cache.retain(|_, cached| {
            cached.last_used.elapsed() < PROCESS_RUNTIME_MARKER_CACHE_RETENTION
        });
        if cache.len() >= PROCESS_RUNTIME_MARKER_CACHE_CAPACITY {
            cache.clear();
        }
    }
    cache.insert(
        pid,
        CachedGitBashProcess {
            creation_time,
            is_git_bash,
            last_used: Instant::now(),
        },
    );
    is_git_bash
}

fn process_executable_path(process: HANDLE) -> Option<String> {
    #[cfg(test)]
    PROCESS_INSPECTION_COUNTS.with_borrow_mut(|counts| counts.image_queries += 1);
    let mut path = vec![0_u16; 32_768];
    let mut len = path.len() as u32;
    if unsafe { QueryFullProcessImageNameW(process, 0, path.as_mut_ptr(), &mut len) } == 0 {
        return None;
    }
    String::from_utf16(&path[..len as usize]).ok()
}

fn command_uses_git_bash(command: &portable_pty::CommandBuilder) -> bool {
    let Some(program) = command.get_argv().first() else {
        return false;
    };
    let path = std::path::Path::new(program);
    if path.is_absolute() {
        return is_git_bash_executable_path(path);
    }
    if program.to_string_lossy().contains(['/', '\\']) {
        return false;
    }

    let Some(file_name) = path.file_name().and_then(OsStr::to_str) else {
        return false;
    };
    let candidate_name = if file_name.eq_ignore_ascii_case("bash") {
        "bash.exe"
    } else if file_name.eq_ignore_ascii_case("bash.exe") {
        file_name
    } else {
        return false;
    };
    let search_path = command
        .get_env("PATH")
        .map(OsStr::to_os_string)
        .or_else(|| std::env::var_os("PATH"));
    search_path.is_some_and(|search_path| {
        std::env::split_paths(&search_path)
            .map(|directory| directory.join(candidate_name))
            .find(|candidate| candidate.is_file())
            .is_some_and(|candidate| is_git_bash_executable_path(&candidate))
    })
}

fn is_git_bash_executable_path(path: &std::path::Path) -> bool {
    let Some(file_name) = path.file_name().and_then(OsStr::to_str) else {
        return false;
    };
    if !file_name.eq_ignore_ascii_case("bash.exe") || !path.is_absolute() || !path.is_file() {
        return false;
    }

    let Some(bin_dir) = path.parent() else {
        return false;
    };
    if !bin_dir
        .file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| name.eq_ignore_ascii_case("bin"))
    {
        return false;
    }

    let Some(mut root) = bin_dir.parent() else {
        return false;
    };
    if root
        .file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| name.eq_ignore_ascii_case("usr"))
    {
        let Some(parent) = root.parent() else {
            return false;
        };
        root = parent;
    }

    root.join("usr").join("bin").join("msys-2.0.dll").is_file()
        && root.join("cmd").join("git.exe").is_file()
}

fn process_runtime_marker(entry: &WindowsProcessEntry) -> Option<String> {
    let observation = entry.observation()?;
    let pid = entry.pid;
    let creation_time = observation.created;
    {
        let mut cache = PROCESS_RUNTIME_MARKER_CACHE
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        if let Some(cached) = cache.get_mut(&pid) {
            if cached.creation_time == creation_time
                && (cached.marker.is_some()
                    || cached.cached_at.elapsed() < PROCESS_RUNTIME_MARKER_NEGATIVE_TTL)
            {
                cached.last_used = Instant::now();
                return cached.marker.clone();
            }
        }
    }

    let process = ProcessHandle::open(pid, PROCESS_QUERY_INFORMATION | PROCESS_VM_READ)?;
    if process_creation_time(process.0) != Some(creation_time) {
        return None;
    }
    let marker = process_runtime_marker_from_handle(process.0)?;
    let mut cache = PROCESS_RUNTIME_MARKER_CACHE
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    if cache.len() >= PROCESS_RUNTIME_MARKER_CACHE_CAPACITY {
        cache.retain(|_, cached| {
            cached.last_used.elapsed() < PROCESS_RUNTIME_MARKER_CACHE_RETENTION
        });
        if cache.len() >= PROCESS_RUNTIME_MARKER_CACHE_CAPACITY {
            cache.clear();
        }
    }
    cache.insert(
        pid,
        CachedProcessRuntimeMarker {
            creation_time,
            marker: marker.clone(),
            cached_at: Instant::now(),
            last_used: Instant::now(),
        },
    );
    marker
}

fn process_creation_time(process: HANDLE) -> Option<u64> {
    #[cfg(test)]
    PROCESS_INSPECTION_COUNTS.with_borrow_mut(|counts| counts.creation_queries += 1);
    let mut creation_time = FILETIME::default();
    let mut exit_time = FILETIME::default();
    let mut kernel_time = FILETIME::default();
    let mut user_time = FILETIME::default();
    if unsafe {
        GetProcessTimes(
            process,
            &mut creation_time,
            &mut exit_time,
            &mut kernel_time,
            &mut user_time,
        )
    } == 0
    {
        return None;
    }
    Some(filetime_ticks(creation_time))
}

fn filetime_ticks(time: FILETIME) -> u64 {
    (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime)
}

fn process_runtime_marker_from_handle(process: HANDLE) -> Option<Option<String>> {
    let parameters = read_process_parameters(process)?;
    let environment = read_process_environment(process, parameters.environment)?;
    Some(environment_variable_from_utf16(
        &environment,
        PANE_RUNTIME_MARKER_ENV_VAR,
    ))
}

fn read_process_environment(process: HANDLE, address: *const c_void) -> Option<Vec<u16>> {
    if address.is_null() {
        return None;
    }

    let mut memory = MaybeUninit::<MEMORY_BASIC_INFORMATION>::uninit();
    let queried = unsafe {
        VirtualQueryEx(
            process,
            address,
            memory.as_mut_ptr(),
            size_of::<MEMORY_BASIC_INFORMATION>(),
        )
    };
    if queried == 0 {
        return None;
    }
    let memory = unsafe { memory.assume_init() };
    let address = address as usize;
    let base = memory.BaseAddress as usize;
    let offset = address.checked_sub(base)?;
    let available = memory.RegionSize.checked_sub(offset)?;
    let read_len = available.min(MAX_PROCESS_ENVIRONMENT_BYTES);
    if read_len < size_of::<u16>() {
        return None;
    }

    let max_units = read_len / size_of::<u16>();
    let chunk_units = PROCESS_ENVIRONMENT_READ_CHUNK_BYTES / size_of::<u16>();
    let mut environment = Vec::new();
    while environment.len() < max_units {
        let unit_count = (max_units - environment.len()).min(chunk_units);
        let chunk_bytes = unit_count * size_of::<u16>();
        let mut chunk = vec![0_u16; unit_count];
        let mut bytes_read = 0;
        let offset = environment.len().checked_mul(size_of::<u16>())?;
        let chunk_address = address.checked_add(offset)?;
        if unsafe {
            ReadProcessMemory(
                process,
                chunk_address as *const c_void,
                chunk.as_mut_ptr().cast::<c_void>(),
                chunk_bytes,
                &mut bytes_read,
            )
        } == 0
        {
            break;
        }
        chunk.truncate(bytes_read / size_of::<u16>());
        if chunk.is_empty() {
            break;
        }
        environment.extend_from_slice(&chunk);
        if let Some(end) = environment
            .windows(2)
            .position(|pair| pair == [0, 0])
            .map(|index| index + 2)
        {
            environment.truncate(end);
            return Some(environment);
        }
        if bytes_read < chunk_bytes {
            break;
        }
    }
    None
}

fn environment_variable_from_utf16(environment: &[u16], name: &str) -> Option<String> {
    for variable in environment.split(|unit| *unit == 0) {
        if variable.is_empty() {
            break;
        }
        let Some(separator) = variable.iter().position(|unit| *unit == u16::from(b'=')) else {
            continue;
        };
        let Ok(variable_name) = String::from_utf16(&variable[..separator]) else {
            continue;
        };
        if variable_name.eq_ignore_ascii_case(name) {
            return String::from_utf16(&variable[separator + 1..]).ok();
        }
    }
    None
}

/// Read a process command line with only `PROCESS_QUERY_LIMITED_INFORMATION`.
///
/// `ProcessCommandLineInformation` has been available since Windows 8.1.
/// Prefer it over walking the target PEB, which additionally requires
/// `PROCESS_VM_READ` access that hardened runtimes and security products deny.
///
/// Returns `None` for a process without a stored command line; the caller then
/// tries the PEB path before giving up.
fn read_process_command_line(process: HANDLE) -> Option<String> {
    #[cfg(test)]
    PROCESS_INSPECTION_COUNTS.with_borrow_mut(|counts| counts.command_reads += 1);
    let mut required = 0_u32;
    // SAFETY: a null buffer with length 0 only asks for the required size, and
    // `required` is a valid out-pointer for the duration of the call.
    let status = unsafe {
        NtQueryInformationProcess(
            process,
            ProcessCommandLineInformation,
            null_mut(),
            0,
            &mut required,
        )
    };
    // A process with no command line is already handled as a miss below.
    if status != STATUS_BUFFER_TOO_SMALL
        && status != STATUS_INFO_LENGTH_MISMATCH
        && status != STATUS_BUFFER_OVERFLOW
    {
        return None;
    }

    // `required` is already at least a UNICODE_STRING sized buffer.
    let mut buffer = vec![0_u8; required as usize];
    for _ in 0..2 {
        // SAFETY: `buffer` is `required` bytes and both pointers are valid for
        // the call; the kernel writes the length back into `required`.
        let status = unsafe {
            NtQueryInformationProcess(
                process,
                ProcessCommandLineInformation,
                buffer.as_mut_ptr().cast(),
                required,
                &mut required,
            )
        };
        // These three statuses all mean the command line grew between the probe
        // and the read. Some data was written; retry once with the larger buffer
        // the call just reported. They are negative as `NTSTATUS`, so they must
        // be checked before the failure test below.
        let grew = status == STATUS_BUFFER_OVERFLOW
            || status == STATUS_BUFFER_TOO_SMALL
            || status == STATUS_INFO_LENGTH_MISMATCH;
        if grew {
            buffer = vec![0_u8; required as usize];
            continue;
        }
        if status < 0 {
            return None;
        }
        break;
    }

    // SAFETY: on success the kernel wrote a UNICODE_STRING followed by its
    // UTF-16 contents into `buffer`. A `Vec<u8>` only guarantees byte
    // alignment, so read the header unaligned.
    let unicode = unsafe { buffer.as_ptr().cast::<UNICODE_STRING>().read_unaligned() };
    let length = usize::from(unicode.Length);
    // A short command line leaves `Length` inside the header itself; guard
    // against reading a malformed header as string data.
    if length == 0 || !length.is_multiple_of(2) {
        return None;
    }
    let string_offset = size_of::<UNICODE_STRING>();
    if string_offset + length > buffer.len() {
        return None;
    }
    let units = buffer[string_offset..string_offset + length]
        .chunks_exact(2)
        .map(|unit| u16::from_ne_bytes([unit[0], unit[1]]))
        .collect::<Vec<_>>();
    String::from_utf16(&units)
        .ok()
        .filter(|command_line| !command_line.is_empty())
}

fn process_basic_information(process: HANDLE) -> Option<PROCESS_BASIC_INFORMATION> {
    #[cfg(test)]
    PROCESS_INSPECTION_COUNTS.with_borrow_mut(|counts| counts.parent_queries += 1);
    let mut basic_info = MaybeUninit::<PROCESS_BASIC_INFORMATION>::uninit();
    let status = unsafe {
        NtQueryInformationProcess(
            process,
            ProcessBasicInformation,
            basic_info.as_mut_ptr().cast::<c_void>(),
            size_of::<PROCESS_BASIC_INFORMATION>() as u32,
            null_mut(),
        )
    };
    if status != STATUS_SUCCESS as NTSTATUS {
        return None;
    }
    Some(unsafe { basic_info.assume_init() })
}

fn read_process_parameters(process: HANDLE) -> Option<RtlUserProcessParameters> {
    let basic_info = process_basic_information(process)?;
    if basic_info.PebBaseAddress.is_null() {
        return None;
    }

    let peb = read_process_value::<Peb>(process, basic_info.PebBaseAddress.cast::<c_void>())?;
    if peb.process_parameters.is_null() {
        return None;
    }

    read_process_value::<RtlUserProcessParameters>(process, peb.process_parameters.cast())
}

fn command_line_to_argv(command_line: &str) -> Option<Vec<String>> {
    let wide: Vec<u16> = command_line
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut argc = 0;
    let argv_ptr = unsafe { CommandLineToArgvW(wide.as_ptr(), &mut argc) };
    if argv_ptr.is_null() || argc <= 0 {
        return None;
    }

    let argv_slice = unsafe { std::slice::from_raw_parts(argv_ptr, argc as usize) };
    let mut argv = Vec::with_capacity(argc as usize);
    for &arg in argv_slice {
        if arg.is_null() {
            continue;
        }
        let mut len = 0;
        unsafe {
            while *arg.add(len) != 0 {
                len += 1;
            }
            argv.push(String::from_utf16_lossy(std::slice::from_raw_parts(
                arg, len,
            )));
        }
    }
    unsafe {
        LocalFree(argv_ptr.cast());
    }
    Some(argv)
}

fn nul_terminated_utf16_to_string(buffer: &[u16]) -> String {
    let len = buffer
        .iter()
        .position(|&value| value == 0)
        .unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..len])
}

/// pane 进程树的锚点：pane 自己的 child pid 加上它的创建时间与快照时刻（见
/// [`ProcessSessionId`]）。Windows 没有 Unix 的会话语义，进程树按父子关系枚举。
///
/// 只认句柄当下指向的进程：调用方要确认这个 pid 那一刻仍属于自己的子进程（还没被 `wait`
/// 回收、句柄还握着），否则读到的可能是复用了 pid 的无关进程。已退出但句柄未释放的进程照样
/// 给出锚点，它留下的孤儿仍归这棵树。
pub fn process_session_id(pid: u32) -> Option<ProcessSessionId> {
    let process = ProcessHandle::open(pid, PROCESS_QUERY_LIMITED_INFORMATION)?;
    let instance = process_creation_time(process.0)?;
    // 必须在握着句柄时读时钟：这一刻 pid 还属于根进程，之后才可能被复用。用粗粒度时钟：它
    // 不晚于此刻的精确时间，进程创建时间无论是精确值还是时钟节拍值，之后创建的进程都不早于它。
    let mut now = FILETIME::default();
    unsafe { GetSystemTimeAsFileTime(&mut now) };
    Some(ProcessSessionId {
        id: i64::from(pid),
        instance,
        captured: filetime_ticks(now),
    })
}

/// 一次进程快照取出整批进程树的成员，返回与 `sessions` 一一对应的桶：关 N 个 pane 只拍
/// 一次快照，而不是 N 次。成员按 [`verified_session_members`] 的规则逐个核对身份。
pub(crate) fn session_members_batch(
    sessions: &[ProcessSessionId],
) -> Vec<Vec<ProcessSessionMember>> {
    if sessions.is_empty() {
        return Vec::new();
    }

    let snapshot = ProcessSnapshot::new(snapshot_processes());
    sessions
        .iter()
        .map(|session| {
            verified_session_members(*session, &snapshot, inspect_process, is_herdr_server_daemon)
        })
        .collect()
}

/// 以 `root_pid` 为根的进程树成员（根进程在前）。调用方必须还握着根进程的句柄（尚未 `wait`
/// 的子进程），否则 pid 可能已经属于别的进程。
pub(crate) fn process_tree_members(root_pid: u32) -> Vec<ProcessSessionMember> {
    let Some(session) = process_session_id(root_pid) else {
        return Vec::new();
    };
    let snapshot = ProcessSnapshot::new(snapshot_processes());
    verified_session_members(session, &snapshot, inspect_process, is_herdr_server_daemon)
}

/// 已核对身份（句柄还握着）的候选是不是 herdr 自己的 server daemon：挂着标记的（见
/// [`announce_detached_server_daemon`]），或者还不会挂标记的旧版、上游构建——映像与命令行
/// 按 [`is_legacy_server_daemon_command`] 认，都从同一个句柄读。
fn is_herdr_server_daemon(member: ProcessSessionMember, process: &ProcessHandle) -> bool {
    if server_daemon_marker_exists(member) {
        return true;
    }
    let Some(image) = process_executable_path(process.0) else {
        return false;
    };
    // 先看映像名再读命令行：绝大多数候选第一步就排除，不多读一次进程信息。
    if !image_file_name_starts_with_herdr(&image) {
        return false;
    }
    read_process_command_line(process.0)
        .is_some_and(|command_line| is_legacy_server_daemon_command(&image, &command_line))
}

fn image_file_name_starts_with_herdr(image_path: &str) -> bool {
    std::path::Path::new(image_path)
        .file_name()
        .and_then(OsStr::to_str)
        .and_then(|name| name.get(..5))
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("herdr"))
}

/// 没挂标记的 server daemon 的保守认法：映像文件名以 `herdr` 开头（不分大小写），命令行恰好是
/// `<exe> server`——`build_server_daemon_command` 拉起 daemon 的形态：只有两个参数，第二个是
/// `server`，第一个指向同一个映像文件（按文件名比，不分大小写，路径写法不同也认）。别的子命令、
/// 多余或缺少的参数都不算：宁可漏认（照常终止），也不放过别的程序。在 PowerShell 里前台敲的
/// `herdr server` 形态相同也会被留下，它照样收到控制台关闭事件。
fn is_legacy_server_daemon_command(image_path: &str, command_line: &str) -> bool {
    if !image_file_name_starts_with_herdr(image_path) {
        return false;
    }
    let Some(argv) = command_line_to_argv(command_line) else {
        return false;
    };
    let [program, subcommand] = argv.as_slice() else {
        return false;
    };
    let file_name = |path: &str| {
        std::path::Path::new(path)
            .file_name()
            .and_then(OsStr::to_str)
            .map(str::to_owned)
    };
    subcommand == "server"
        && file_name(program)
            .zip(file_name(image_path))
            .is_some_and(|(program, image)| program.eq_ignore_ascii_case(&image))
}

/// 通过同一个进程句柄读到的身份：父 pid 与创建时间都属于这个句柄指向的进程对象。握着 `pin`
/// 期间这个 pid 不会被复用（生产里是句柄本身，纯逻辑测试里是测试替身）。
struct InspectedProcess<P> {
    parent_pid: u32,
    created: u64,
    pin: P,
}

fn inspect_process(pid: u32) -> Option<InspectedProcess<ProcessHandle>> {
    let process = ProcessHandle::open(pid, PROCESS_QUERY_LIMITED_INFORMATION)?;
    let created = process_creation_time(process.0)?;
    let basic = process_basic_information(process.0)?;
    let parent_pid = u32::try_from(basic.InheritedFromUniqueProcessId).ok()?;
    Some(InspectedProcess {
        parent_pid,
        created,
        pin: process,
    })
}

/// 一个已核对进程的子进程，创建时间须落在的范围：`[since, before)`，`before` 为空表示不设上限。
struct ChildWindow {
    since: u64,
    before: Option<u64>,
}

impl ChildWindow {
    fn contains(&self, created: u64) -> bool {
        created >= self.since && self.before.is_none_or(|before| created < before)
    }
}

/// 核对一棵 pane 进程树的成员（根进程在前）。
///
/// 快照的 `th32ParentProcessID` 只用来发现候选：父进程退出后 pid 会被复用，快照里挂在某个 pid
/// 下的进程未必是它当前主人的子进程。候选的父 pid 与创建时间都经 `inspect` 从它自己的句柄读出，
/// 读不出来的进程不算成员（也就永远不会收到信号），也不再往下找。
///
/// - 根进程：pid 当前主人的创建时间等于 `session.instance` 才算根进程本身。
/// - 已核对的成员握着句柄，其 pid 在核对子进程期间不会被复用；父 pid 指向它、且创建不早于它
///   的进程只能是它创建的，算成员。早于它创建的是 pid 上一任主人留下的子进程，排除。
/// - 根进程核对不上（已退出且 pid 已空出、pid 已换了主人，或读不出来）：根进程不算成员。
///   锚点时刻根进程还占着 pid，此后拿到这个 pid 的进程及其子进程都创建于 `session.captured`
///   之后，排除；只认创建于 `[instance, captured)`、父 pid 指向它的孤儿，再从孤儿往下照常
///   核对。中间一级已经退出的进程（启动器先退出）接不上，留在原处。
/// - 已退出但进程对象还被句柄引用的根进程 pid 不会被复用，照常算根进程本身。
/// - herdr 自己的 server daemon（`is_server_daemon` 拿核对过的身份与还握着的句柄认）连同它下面
///   的整个嵌套会话都不算成员：它不该随拉起它的那个 pane 一起终止。根进程不查：pane 的 shell
///   跑在 ConPTY 里、有控制台，不会是脱离控制台的 daemon。
fn verified_session_members<P>(
    session: ProcessSessionId,
    snapshot: &ProcessSnapshot,
    mut inspect: impl FnMut(u32) -> Option<InspectedProcess<P>>,
    is_server_daemon: impl Fn(ProcessSessionMember, &P) -> bool,
) -> Vec<ProcessSessionMember> {
    let Ok(root_pid) = u32::try_from(session.id) else {
        return Vec::new();
    };
    if root_pid == 0 {
        return Vec::new();
    }

    let mut members = Vec::new();
    let mut visited = HashSet::from([root_pid]);
    // 待展开的进程、它的子进程须落在的创建时间范围，以及它的句柄：句柄一直留到子进程核对完。
    let mut queue = VecDeque::new();
    match inspect(root_pid).filter(|root| root.created == session.instance) {
        Some(root) => {
            members.push(ProcessSessionMember {
                pid: root_pid,
                instance: root.created,
            });
            let window = ChildWindow {
                since: root.created,
                before: None,
            };
            queue.push_back((root_pid, window, Some(root.pin)));
        }
        None => {
            let window = ChildWindow {
                since: session.instance,
                before: Some(session.captured),
            };
            queue.push_back((root_pid, window, None));
        }
    }

    while let Some((pid, window, _pin)) = queue.pop_front() {
        for child in snapshot.child_pids(pid) {
            if !visited.insert(child) {
                continue;
            }
            let Some(found) = inspect(child) else {
                continue;
            };
            if found.parent_pid != pid || !window.contains(found.created) {
                continue;
            }
            let member = ProcessSessionMember {
                pid: child,
                instance: found.created,
            };
            if is_server_daemon(member, &found.pin) {
                tracing::debug!(
                    pid = child,
                    "leaving a herdr server daemon in a pane process tree running with its sessions"
                );
                continue;
            }
            members.push(member);
            let window = ChildWindow {
                since: found.created,
                before: None,
            };
            queue.push_back((child, window, Some(found.pin)));
        }
    }
    members
}

/// 给会话成员发信号，只有 `Kill` 真的终止进程（`TerminateProcess`）。`Hangup` 什么都不做：关掉
/// ConPTY 时控制台里的进程已经收到 CTRL_CLOSE_EVENT。`Terminate` 也什么都不做：Windows 没有可以
/// 捕获的终止请求，提前硬杀只会砍掉宽限期——处理关闭事件的控制台程序（agent CLI 保存会话、git
/// 释放 `index.lock`）多等一级，图形界面程序本来就收不到任何通知；硬杀落在与 Unix SIGKILL 相同
/// 的那一级。
pub(crate) fn signal_session_members(members: &[ProcessSessionMember], signal: Signal) {
    if signal != Signal::Kill {
        return;
    }
    for member in members {
        terminate_session_member(*member);
    }
}

/// 核对身份与终止用同一个句柄：pid 已换了主人、创建时间读不出来，都不终止。
fn terminate_session_member(member: ProcessSessionMember) {
    if member.pid == std::process::id() {
        return;
    }
    let access = PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION;
    let Some(process) = ProcessHandle::open(member.pid, access) else {
        tracing::debug!(
            pid = member.pid,
            err = %std::io::Error::last_os_error(),
            "pane process could not be opened for termination"
        );
        return;
    };
    let created = process_creation_time(process.0);
    if created != Some(member.instance) {
        tracing::debug!(
            pid = member.pid,
            expected = member.instance,
            found = ?created,
            "pane process id now names another process; not terminating it"
        );
        return;
    }
    if unsafe { TerminateProcess(process.0, 1) } == 0 {
        tracing::debug!(
            pid = member.pid,
            err = %std::io::Error::last_os_error(),
            "failed to terminate pane process"
        );
    }
}

/// 成员是否仍需等待它退出：pid 已换了主人（创建时间对不上或读不出来）就算原来的成员已退出。
/// 核对与判活是两次打开，其间若刚好被复用只会多等一轮，发信号前还会再核对一次。
pub(crate) fn session_member_alive(member: ProcessSessionMember) -> bool {
    let instance_matches = ProcessHandle::open(member.pid, PROCESS_QUERY_LIMITED_INFORMATION)
        .and_then(|process| process_creation_time(process.0))
        == Some(member.instance);
    instance_matches && process_alive_excluding_zombies(member.pid)
}

/// 进程在进程对象 signaled 之前都算存在。Windows 先公布退出码，再结束其余线程、关掉句柄表
/// （cwd、打开的文件）、释放地址空间，最后才把进程对象置为 signaled：只看退出码会在句柄仍被
/// 占着时就报已退出，pane 收尾后紧接着删 worktree 目录因此失败。拿不到 `SYNCHRONIZE` 时（拒绝
/// 访问等）沿用原来只读退出码的判定，打不开仍按不存在处理。
pub fn process_exists(pid: u32) -> bool {
    match process_state(pid) {
        ProcessState::Running => true,
        ProcessState::Exited => false,
        ProcessState::Unknown => process_exit_code_is_still_active(pid),
    }
}

/// 进程是否仍在运行、需要继续等它退出。
///
/// Windows 没有僵尸进程，[`process_exists`] 本身已满足终止阶梯的语义（HSR-02）；它等到进程
/// 对象 signaled，阶梯因此不会在句柄表释放前就把 pane 当成已退出。
pub fn process_alive_excluding_zombies(pid: u32) -> bool {
    process_exists(pid)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProcessState {
    /// 进程对象还没 signaled：在运行，或正在退出、句柄表还没释放完。
    Running,
    /// 进程对象已 signaled，或 pid 根本不存在。
    Exited,
    /// 拿不到带 `SYNCHRONIZE` 的句柄（拒绝访问等），或 pid 为 0：查不清。
    Unknown,
}

fn process_state(pid: u32) -> ProcessState {
    use windows_sys::Win32::Foundation::{ERROR_INVALID_PARAMETER, WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{WaitForSingleObject, PROCESS_SYNCHRONIZE};

    if pid == 0 {
        return ProcessState::Unknown;
    }
    let access = PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE;
    let handle = unsafe { OpenProcess(access, 0, pid) };
    if handle.is_null() {
        let missing = std::io::Error::last_os_error().raw_os_error()
            == i32::try_from(ERROR_INVALID_PARAMETER).ok();
        return if missing {
            ProcessState::Exited
        } else {
            ProcessState::Unknown
        };
    }
    let process = ProcessHandle(handle);
    match unsafe { WaitForSingleObject(process.0, 0) } {
        WAIT_TIMEOUT => ProcessState::Running,
        WAIT_OBJECT_0 => ProcessState::Exited,
        _ => ProcessState::Unknown,
    }
}

/// 只读退出码的旧判定：`SYNCHRONIZE` 被拒时的退路。
fn process_exit_code_is_still_active(pid: u32) -> bool {
    let Some(process) = ProcessHandle::open(pid, PROCESS_QUERY_LIMITED_INFORMATION) else {
        return false;
    };

    let mut exit_code = 0;
    let ok = unsafe { GetExitCodeProcess(process.0, &mut exit_code) } != 0;
    ok && exit_code == STILL_ACTIVE
}

static LAST_CLIPBOARD_WRITE_SEQUENCE: AtomicU32 = AtomicU32::new(0);

pub fn write_clipboard(bytes: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return false;
    };
    if text.contains('\0') {
        return false;
    }
    let mut utf16: Vec<u16> = text.encode_utf16().collect();
    utf16.push(0);
    let Some(byte_len) = utf16.len().checked_mul(size_of::<u16>()) else {
        return false;
    };

    unsafe {
        let owner = GetConsoleWindow();
        if owner.is_null() || OpenClipboard(owner) == 0 {
            return false;
        }
        let _clipboard = ClipboardGuard;

        if EmptyClipboard() == 0 {
            return false;
        }

        let memory = GlobalAlloc(GMEM_MOVEABLE, byte_len);
        if memory.is_null() {
            return false;
        }

        let locked = GlobalLock(memory);
        if locked.is_null() {
            GlobalFree(memory);
            return false;
        }
        copy_nonoverlapping(utf16.as_ptr(), locked.cast::<u16>(), utf16.len());
        GlobalUnlock(memory);

        if SetClipboardData(CF_UNICODETEXT as u32, memory).is_null() {
            GlobalFree(memory);
            return false;
        }

        // Closing may generate additional text formats and advance the sequence.
        drop(_clipboard);
        // Read the sequence first: a later writer must not become our last write.
        let sequence = GetClipboardSequenceNumber();
        let sequence = if GetClipboardOwner() == owner {
            sequence
        } else {
            0
        };
        LAST_CLIPBOARD_WRITE_SEQUENCE.store(sequence, AtomicOrdering::Relaxed);
        true
    }
}

pub fn read_clipboard_text() -> Option<String> {
    None
}

/// Whether the system clipboard currently holds exactly this text.
///
/// Returns `None` when the clipboard changed since our last write, cannot be read,
/// or has non-text formats.
/// Kept separate from [`read_clipboard_text`] so unsupported modal paste on
/// Windows is unchanged.
pub fn clipboard_text_matches(bytes: &[u8]) -> Option<bool> {
    let current = read_clipboard_unicode_text()?;
    Some(clipboard_text_equals(&current, bytes))
}

fn clipboard_text_equals(current: &str, bytes: &[u8]) -> bool {
    let Ok(payload) = std::str::from_utf8(bytes) else {
        return false;
    };
    normalized_clipboard_newlines(payload) == normalized_clipboard_newlines(current)
}

fn normalized_clipboard_newlines(text: &str) -> std::borrow::Cow<'_, str> {
    if text.contains("\r\n") {
        std::borrow::Cow::Owned(text.replace("\r\n", "\n"))
    } else {
        std::borrow::Cow::Borrowed(text)
    }
}

fn plain_text_clipboard_format(format: u32) -> bool {
    format == CF_UNICODETEXT as u32
        || format == CF_TEXT as u32
        || format == CF_OEMTEXT as u32
        || format == CF_LOCALE as u32
}

fn read_clipboard_unicode_text() -> Option<String> {
    const MAX_CLIPBOARD_TEXT_BYTES: usize = 1024 * 1024;

    for attempt in 0..10 {
        if unsafe { OpenClipboard(null_mut()) } != 0 {
            let _clipboard = ClipboardGuard;
            let sequence = unsafe { GetClipboardSequenceNumber() };
            if sequence == 0
                || sequence != LAST_CLIPBOARD_WRITE_SEQUENCE.load(AtomicOrdering::Relaxed)
            {
                return None;
            }
            let format_count = unsafe { CountClipboardFormats() };
            if format_count <= 0 {
                return None;
            }
            let mut format = 0;
            for _ in 0..format_count {
                format = unsafe { EnumClipboardFormats(format) };
                if format == 0 || !plain_text_clipboard_format(format) {
                    return None;
                }
            }
            let bytes = clipboard_global_bytes(CF_UNICODETEXT as u32, MAX_CLIPBOARD_TEXT_BYTES)?;
            let units = bytes
                .chunks_exact(2)
                .map(|pair| u16::from_ne_bytes([pair[0], pair[1]]))
                .take_while(|unit| *unit != 0);
            return String::from_utf16(&units.collect::<Vec<_>>()).ok();
        }
        if attempt < 9 {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    None
}

pub fn open_url(url: &str) -> std::io::Result<Option<std::process::Child>> {
    let operation = wide_null("open");
    let url = wide_null(url);
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            operation.as_ptr(),
            url.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
        )
    };
    if result as isize > 32 {
        Ok(None)
    } else {
        Err(std::io::Error::other(format!(
            "failed to open URL with ShellExecuteW: code {}",
            result as isize
        )))
    }
}

pub fn read_clipboard_image() -> Option<ClipboardImage> {
    for attempt in 0..10 {
        if unsafe { OpenClipboard(null_mut()) } != 0 {
            let _clipboard = ClipboardGuard;
            if let Some(bytes) = read_registered_png_clipboard() {
                return Some(ClipboardImage {
                    bytes,
                    extension: "png",
                });
            }
            for format in [CF_DIBV5 as u32, CF_DIB as u32] {
                if let Some(bytes) =
                    clipboard_global_bytes(format, clipboard_image::MAX_CLIPBOARD_ALLOCATION)
                {
                    if let Some(bytes) = clipboard_image::dib_to_png(&bytes) {
                        return Some(ClipboardImage {
                            bytes,
                            extension: "png",
                        });
                    }
                }
            }
            return None;
        }
        if attempt < 9 {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    None
}

fn read_registered_png_clipboard() -> Option<Vec<u8>> {
    static PNG_FORMAT: LazyLock<u32> = LazyLock::new(|| {
        let name = wide_null("PNG");
        unsafe { RegisterClipboardFormatW(name.as_ptr()) }
    });
    if *PNG_FORMAT == 0 {
        return None;
    }
    let bytes = clipboard_global_bytes(
        *PNG_FORMAT,
        crate::protocol::MAX_CLIPBOARD_IMAGE_PAYLOAD + 64 * 1024,
    )?;
    clipboard_image::validated_png(&bytes)
}

fn clipboard_global_bytes(format: u32, max_bytes: usize) -> Option<Vec<u8>> {
    let handle = unsafe { GetClipboardData(format) };
    if handle.is_null() {
        return None;
    }
    let data = unsafe { GlobalLock(handle) };
    if data.is_null() {
        return None;
    }
    let size = unsafe { GlobalSize(handle) };
    if size == 0 || size > max_bytes {
        unsafe {
            GlobalUnlock(handle);
        }
        return None;
    }
    let mut bytes = vec![0_u8; size];
    unsafe {
        copy_nonoverlapping(data.cast::<u8>(), bytes.as_mut_ptr(), size);
        GlobalUnlock(handle);
    }
    Some(bytes)
}

fn wide_null(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

struct ProcessHandle(HANDLE);

struct ClipboardGuard;

impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        unsafe {
            CloseClipboard();
        }
    }
}

impl ProcessHandle {
    fn open(pid: u32, access: u32) -> Option<Self> {
        if pid == 0 {
            return None;
        }
        #[cfg(test)]
        PROCESS_INSPECTION_COUNTS.with_borrow_mut(|counts| counts.opens += 1);
        let handle = unsafe { OpenProcess(access, 0, pid) };
        (!handle.is_null()).then_some(Self(handle))
    }
}

impl Drop for ProcessHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Peb {
    reserved1: [u8; 2],
    being_debugged: u8,
    reserved2: [u8; 1],
    reserved3: [*mut c_void; 2],
    ldr: *mut c_void,
    process_parameters: *mut RtlUserProcessParameters,
}

// Prefixes of the x86 PEB and RTL_USER_PROCESS_PARAMETERS through the fields
// needed for cwd; remote pointers must stay 32-bit on a 64-bit reader.
#[cfg(target_pointer_width = "64")]
#[repr(C)]
#[derive(Clone, Copy)]
struct Peb32 {
    reserved: [u32; 4],
    process_parameters: u32,
}

#[cfg(target_pointer_width = "64")]
#[repr(C)]
#[derive(Clone, Copy)]
struct ProcessCwdParameters32 {
    reserved: [u32; 9],
    current_directory: STRING32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CurDir {
    dos_path: UNICODE_STRING,
    handle: HANDLE,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RtlUserProcessParameters {
    maximum_length: u32,
    length: u32,
    flags: u32,
    debug_flags: u32,
    console_handle: HANDLE,
    console_flags: u32,
    standard_input: HANDLE,
    standard_output: HANDLE,
    standard_error: HANDLE,
    current_directory: CurDir,
    dll_path: UNICODE_STRING,
    image_path_name: UNICODE_STRING,
    command_line: UNICODE_STRING,
    environment: *mut c_void,
}

fn read_process_value<T: Copy>(process: HANDLE, address: *const c_void) -> Option<T> {
    if address.is_null() {
        return None;
    }

    let mut value = MaybeUninit::<T>::uninit();
    let mut bytes_read = 0;
    let ok = unsafe {
        ReadProcessMemory(
            process,
            address,
            value.as_mut_ptr().cast::<c_void>(),
            size_of::<T>(),
            &mut bytes_read,
        )
    } != 0;

    (ok && bytes_read == size_of::<T>()).then(|| unsafe { value.assume_init() })
}

fn read_unicode_string(process: HANDLE, unicode: UNICODE_STRING) -> Option<String> {
    if unicode.Buffer.is_null() || unicode.Length == 0 || !unicode.Length.is_multiple_of(2) {
        return None;
    }

    let char_len = usize::from(unicode.Length / 2);
    let mut buffer = vec![0_u16; char_len];
    let mut bytes_read = 0;
    let ok = unsafe {
        ReadProcessMemory(
            process,
            unicode.Buffer.cast::<c_void>(),
            buffer.as_mut_ptr().cast::<c_void>(),
            usize::from(unicode.Length),
            &mut bytes_read,
        )
    } != 0;

    if !ok || bytes_read != usize::from(unicode.Length) {
        return None;
    }

    String::from_utf16(&buffer).ok()
}

// Prefix-mode ASCII input source support (see `switch_ascii_input_source_in_prefix`).
//
// Windows IMEs live in the terminal-emulator process, not in herdr. Empirically:
//   - `WM_IME_CONTROL` / `IMC_GETOPENSTATUS` reads whether the IME is open
//     (composing native characters) reliably across the process boundary (this
//     is what kren-select uses), so we detect state with it. The read goes
//     through `SendMessageTimeoutW` (`SMTO_ABORTIFHUNG`) so a hung host process
//     cannot block us indefinitely.
//   - Writing the state back (`IMC_SETOPENSTATUS` / `IMC_SETCONVERSIONMODE`)
//     changes the flag value but does NOT affect real input in terminal/TSF
//     hosts, so we cannot switch by writing the mode.
//   - `ImmGetContext` on the foreground window returns null across the process
//     boundary, so the ImmGetOpenStatus/ImmSetOpenStatus path is unavailable.
// Therefore we switch the way kren-select does: inject the IME toggle key with
// `SendInput`, which reaches the foreground input queue like a real keypress.
//
// The toggle key is language-specific, so we pick it from the foreground
// keyboard layout's language id. Only Korean is mapped today; other IMEs are
// detected and left untouched (a no-op) rather than toggled with the wrong key.

/// `WM_IME_CONTROL` sub-command that reads whether the IME is open, i.e.
/// composing native characters. This is `IMC_GETOPENSTATUS` (0x0005); for the
/// Korean IME "open" is exactly the Hangul state and "closed" is English/ASCII
/// direct input, which is the state we detect and toggle.
const IMC_GETOPENSTATUS: usize = 0x0005;

/// Virtual key that toggles Hangul/English on Korean IMEs.
const VK_HANGUL: u16 = 0x15;

/// Primary language id (low 10 bits of a LANGID) for Korean.
const LANG_KOREAN: u32 = 0x12;

/// Whether the IME reports itself open, i.e. composing native characters
/// (Hangul for the Korean IME). `IMC_GETOPENSTATUS` returns nonzero when the
/// IME is open and zero when it is in direct English/ASCII input.
fn ime_open(open_status: isize) -> bool {
    open_status != 0
}

/// Timeout (ms) for the cross-process IME open-status read. Short enough that a
/// hung terminal never freezes prefix-mode entry/exit.
const IME_STATUS_READ_TIMEOUT_MS: u32 = 200;

/// Reads the IME open status (`IMC_GETOPENSTATUS`) with a bounded timeout.
///
/// `WM_IME_CONTROL` crosses into the terminal-emulator process, and a plain
/// `SendMessageW` would block herdr's client thread until that process responds
/// (indefinitely if it is hung). `SendMessageTimeoutW` with `SMTO_ABORTIFHUNG`
/// caps the wait; on timeout or failure this returns `None` and callers leave
/// the IME untouched rather than blocking or guessing.
fn read_ime_open_status(ime_hwnd: HWND) -> Option<isize> {
    let mut result: usize = 0;
    // SAFETY: `ime_hwnd` is a non-null IME window from `ImmGetDefaultIMEWnd`, and
    // `result` is a valid out-pointer for the message's `DWORD_PTR` result.
    let ret = unsafe {
        SendMessageTimeoutW(
            ime_hwnd,
            WM_IME_CONTROL,
            IMC_GETOPENSTATUS,
            0,
            SMTO_ABORTIFHUNG,
            IME_STATUS_READ_TIMEOUT_MS,
            &mut result,
        )
    };
    if ret == 0 {
        // Timed out or failed; do not block or assume a state.
        return None;
    }
    Some(result as isize)
}

/// The IME toggle key for a keyboard layout language id, or `None` when the
/// language's toggle key is not known. `langid` is the full LANGID (LOWORD of
/// an `HKL`); the primary language is its low 10 bits.
///
/// Only Korean is mapped: `VK_HANGUL` is the Hangul/English toggle. Japanese
/// (half/full-width) and Chinese use different keys per IME, so they return
/// `None` and are left untouched instead of toggled incorrectly.
fn toggle_key_for_language(langid: u32) -> Option<u16> {
    match langid & 0x3FF {
        LANG_KOREAN => Some(VK_HANGUL),
        _ => None,
    }
}

/// Builds the key-down then key-up `INPUT` pair for `vk`.
fn key_tap_inputs(vk: u16) -> [INPUT; 2] {
    let key_event = |flags| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    [key_event(0), key_event(KEYEVENTF_KEYUP)]
}

/// Injects a key-down then key-up for `vk` via `SendInput`.
///
/// Returns `true` when the key-down was queued and the IME may have toggled.
/// Thin wrapper over [`send_vk_tap_with`] that plugs in the real `SendInput`;
/// the injection policy lives there so it can be unit-tested without the OS.
fn send_vk_tap(vk: u16) -> bool {
    send_vk_tap_with(vk, |events| {
        // SAFETY: `events` outlives the call; its `INPUT_KEYBOARD` entries have
        // the `ki` union variant fully initialized, which is the variant
        // SendInput reads for keyboard input. `size_of::<INPUT>()` is the
        // required `cbSize`.
        unsafe {
            SendInput(
                events.len() as u32,
                events.as_ptr(),
                size_of::<INPUT>() as i32,
            )
        }
    })
}

/// Core key-tap logic with the raw event injector abstracted behind `inject`,
/// which returns how many of the passed events it actually queued. This keeps
/// the success / partial-injection / total-failure branches unit-testable
/// without touching the real `SendInput`.
///
/// `SendInput` returns how many events it queued; a short count means injection
/// was blocked (e.g. by UIPI). Returns `true` whenever the key-down was queued,
/// because the IME may have toggled and callers must retain restoration state.
/// When only the key-down landed, the key-up is retried so the key is not left
/// logically held down.
fn send_vk_tap_with(vk: u16, mut inject: impl FnMut(&[INPUT]) -> u32) -> bool {
    let inputs = key_tap_inputs(vk);
    let sent = inject(&inputs);
    if sent as usize == inputs.len() {
        return true;
    }

    if sent == 1 {
        // The key-down landed and may already have toggled the IME. Retry the
        // dropped key-up, but report that restoration state is still required.
        let key_up = [inputs[1]];
        let up_sent = inject(&key_up);
        tracing::warn!(
            vk,
            sent,
            expected = inputs.len(),
            key_up_retry_sent = up_sent,
            "SendInput dropped the IME toggle key-up; retried key-up"
        );
        return true;
    }

    tracing::warn!(
        vk,
        sent,
        expected = inputs.len(),
        "SendInput did not inject the IME toggle key tap"
    );
    false
}

pub(crate) fn pump_input_source_runloop() {}

/// Switch the foreground window's IME to ASCII-capable input for prefix mode.
///
/// Returns `None` (nothing to restore) when there is no foreground IME, the
/// keyboard language has no known toggle key, or the IME is already
/// ASCII-capable, matching the macOS contract.
pub(crate) fn switch_to_ascii_input_source() -> Option<InputSourceRestore> {
    // SAFETY: all calls are Win32 UI functions invoked on the client's main
    // thread. Every HWND is null-checked before use; `fg_thread` is a thread id
    // (not a handle) used only as `GetKeyboardLayout` input, where 0 harmlessly
    // falls back to the calling thread's layout.
    unsafe {
        let fg = GetForegroundWindow();
        if fg.is_null() {
            return None;
        }

        // Pick the toggle key for the foreground keyboard language. Unknown
        // languages (Japanese, Chinese, ...) are left untouched.
        let fg_thread = GetWindowThreadProcessId(fg, null_mut());
        let langid = (GetKeyboardLayout(fg_thread) as usize as u32) & 0xFFFF;
        let Some(toggle_vk) = toggle_key_for_language(langid) else {
            tracing::debug!(
                langid = format!("{langid:#06x}"),
                "prefix IME switch: no toggle key for keyboard language, leaving IME as-is"
            );
            return None;
        };

        // Detect the open (Hangul) state via the bounded read path.
        let ime_hwnd = ImmGetDefaultIMEWnd(fg);
        if ime_hwnd.is_null() {
            return None;
        }
        let Some(open) = read_ime_open_status(ime_hwnd) else {
            tracing::debug!("prefix IME switch skipped: IME open-status read timed out");
            return None;
        };
        if !ime_open(open) {
            // Already in English/ASCII input; nothing to switch or restore.
            return None;
        }

        // The bounded cross-process status read can take long enough for focus
        // to change. Recheck immediately before using the global input queue.
        if GetForegroundWindow() != fg {
            tracing::debug!("prefix IME switch skipped: foreground window changed");
            return None;
        }

        // Toggle to ASCII by injecting the language's IME toggle key. Only arm
        // restoration when the toggle actually landed, so we never try to
        // restore a switch that never happened.
        if !send_vk_tap(toggle_vk) {
            tracing::warn!(
                langid = format!("{langid:#06x}"),
                "prefix IME switch: toggle injection failed, leaving IME as-is"
            );
            return None;
        }
        tracing::debug!(
            langid = format!("{langid:#06x}"),
            "switched host IME to ASCII for prefix mode"
        );
        Some(InputSourceRestore {
            toggle_vk,
            origin_hwnd: fg as isize,
        })
    }
}

/// Restores the native (Hangul) IME state that was active before prefix mode.
///
/// Only constructed by [`switch_to_ascii_input_source`] after it successfully
/// toggled the IME to English/ASCII. Dropping it re-injects the same toggle key
/// to go back, but only after two guards, so restoration never fights the user
/// or another application:
///   - the same window that was switched must still be focused, otherwise the
///     toggle would land on whatever app the user moved to;
///   - the IME must still be in English (our switch still in effect), otherwise
///     the user manually returned to Hangul during prefix mode and we must leave
///     their choice alone.
///
/// `origin_hwnd` stores the foreground window at switch time as raw pointer bits
/// (`isize`, not `HWND`) so the guard stays `Send` when parked in the client's
/// prefix-input state across `.await` points.
#[derive(Debug)]
pub(crate) struct InputSourceRestore {
    toggle_vk: u16,
    origin_hwnd: isize,
}

impl Drop for InputSourceRestore {
    fn drop(&mut self) {
        // SAFETY: all calls are Win32 UI functions invoked on the client's main
        // thread. Every HWND is null-checked before use.
        unsafe {
            // Guard 1: only restore if the window we switched is still focused,
            // so the toggle never lands on a different application.
            let fg = GetForegroundWindow();
            if fg.is_null() || fg as isize != self.origin_hwnd {
                tracing::debug!(
                    "prefix IME restore skipped: foreground window changed since switch"
                );
                return;
            }

            // Guard 2: only restore if the IME is still in English (our switch is
            // still in effect). If the user manually switched back to Hangul
            // during prefix mode, leave their choice untouched.
            let ime_hwnd = ImmGetDefaultIMEWnd(fg);
            if ime_hwnd.is_null() {
                return;
            }
            let Some(open) = read_ime_open_status(ime_hwnd) else {
                tracing::debug!("prefix IME restore skipped: IME open-status read timed out");
                return;
            };
            if ime_open(open) {
                tracing::debug!("prefix IME restore skipped: IME already back to native input");
                return;
            }

            // The bounded cross-process status read can take long enough for
            // focus to change. Recheck immediately before using SendInput.
            if GetForegroundWindow() != fg {
                tracing::debug!("prefix IME restore skipped: foreground window changed");
                return;
            }

            if send_vk_tap(self.toggle_vk) {
                tracing::debug!("restored host IME after prefix mode");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        process::{Command, Stdio},
        sync::Arc,
        thread,
        time::{Duration, Instant},
    };

    use windows_sys::Win32::System::Console::{
        AllocConsole, FreeConsole, GetConsoleProcessList, GetConsoleWindow,
    };

    /// 测试用唯一临时路径：pid 区分进程，`test_dirs::unique_id` 区分 `cargo test` 同一进程里
    /// 并发的测试（只靠 pid 或时间戳会撞名）。
    fn unique_temp_path(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "{label}-{}-{}",
            std::process::id(),
            crate::config::test_dirs::unique_id()
        ))
    }

    fn comspec() -> std::ffi::OsString {
        std::env::var_os("ComSpec").unwrap_or_else(|| r"C:\Windows\System32\cmd.exe".into())
    }

    /// 已退出、但句柄还握在手里的子进程：pid 在测试期间不会被复用给别的进程。
    fn exited_child() -> std::process::Child {
        let mut child = Command::new(comspec())
            .args(["/D", "/Q", "/C", "exit 0"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn short-lived child");
        child.wait().expect("wait for short-lived child");
        child
    }

    #[test]
    fn remote_private_entry_names_match_their_creators_exactly() {
        use super::RemotePrivateEntryKind::{Dir, File};

        for (name, expected) in [
            ("ssh-4242-0", Some((4242, Dir))),
            ("ssh-4242-17", Some((4242, Dir))),
            ("herdr-remote-4242-linux-x86_64-0", Some((4242, Dir))),
            ("herdr-remote-4242-windows-installer-3", Some((4242, Dir))),
            (
                "herdr-r-4242-user-exa-0123456789abcdef.sock",
                Some((4242, File)),
            ),
            ("herdr-r-4242--0123456789abcdef.sock", Some((4242, File))),
            ("herdr-s-4242-0123456789abcdef.sock", Some((4242, File))),
            ("herdr-api-4242-fedcba9876543210.sock", Some((4242, File))),
            ("herdr-ap-4242-7.sock", Some((4242, File))),
            // 与创建处的格式不逐字一致：一律不认。
            ("ssh-4242", None),
            ("ssh-4242-", None),
            ("ssh-4242-x", None),
            ("ssh-04242-0", None),
            ("ssh-4242-00", None),
            ("ssh-4242-0-old", None),
            ("ssh-99999999999-0", None),
            ("herdr-remote-4242-linux-x86_64", None),
            ("herdr-remote-4242-Linux-x86_64-0", None),
            ("herdr-remote-4242--0", None),
            ("herdr-remote-4242-linux-x86_64-0.tmp", None),
            ("herdr-remote-4242-dev-default.sock", None),
            ("herdr-r-4242-user-0123456789abcde.sock", None),
            ("herdr-r-4242-longtarget-0123456789abcdef.sock", None),
            ("herdr-r-4242-user-0123456789ABCDEF.sock", None),
            ("herdr-s-4242-0123456789abcdef", None),
            ("herdr-s-4242-0123456789abcdef.sock.bak", None),
            (
                "herdr-api-ssh-4242-0123456789abcdef0123456789abcdef.sock",
                None,
            ),
            ("herdr-ssh-4242-0123456789abcdef0123456789abcdef.sock", None),
            ("herdr-askpass-4242-7.sock", None),
            ("herdr-ap-4242-07.sock", None),
            ("herdr-x-4242-7.sock", None),
            ("known_hosts", None),
        ] {
            assert_eq!(super::remote_private_entry(name), expected, "{name}");
        }
    }

    /// 陈旧私有目录项清理只删属主进程确定已退出、名字逐字匹配且类型对得上的项：本进程与仍在
    /// 运行（或查不清）的进程的项、名字不匹配的项、类型不符的项都留着；junction 既不删也不
    /// 跟随进去删目标。
    #[test]
    fn stale_remote_private_entries_of_exited_processes_are_swept() {
        let dirs = crate::config::test_dirs::isolate_dirs("remote-sweep");
        let base = dirs.state_dir().join("remote");
        fs::create_dir_all(&base).expect("create remote base");
        let exited = exited_child();
        let dead = exited.id();
        let live = std::process::id();
        assert!(super::process_has_exited(dead));
        assert!(!super::process_has_exited(live));

        let stale_dirs = [
            format!("ssh-{dead}-0"),
            format!("ssh-{dead}-1"),
            format!("herdr-remote-{dead}-linux-x86_64-0"),
            format!("herdr-remote-{dead}-windows-installer-2"),
        ];
        let stale_files = [
            format!("herdr-r-{dead}-user-exa-0123456789abcdef.sock"),
            format!("herdr-s-{dead}-0123456789abcdef.sock"),
            format!("herdr-api-{dead}-0123456789abcdef.sock"),
            format!("herdr-ap-{dead}-7.sock"),
        ];
        let kept_dirs = [
            format!("ssh-{live}-0"),
            format!("herdr-remote-{live}-linux-x86_64-0"),
            // System 进程（pid 4）一直在运行，打不开时也按仍在运行处理。
            "ssh-4-0".to_owned(),
            "ssh-0-0".to_owned(),
            format!("ssh-{dead}-old"),
            format!("ssh-0{dead}-0"),
            // 名字是端点的格式，类型却是目录。
            format!("herdr-ap-{dead}-8.sock"),
        ];
        let kept_files = [
            format!("herdr-ap-{live}-7.sock"),
            format!("herdr-s-{dead}-0123.sock"),
            format!("herdr-ap-{dead}-7.sock.bak"),
            // 名字是受管配置目录的格式，类型却是文件。
            format!("ssh-{dead}-2"),
            "known_hosts".to_owned(),
        ];
        for name in stale_dirs.iter().chain(&kept_dirs) {
            fs::create_dir(base.join(name)).expect("create fixture dir");
            fs::write(base.join(name).join("config"), b"Host *\n").expect("write fixture file");
        }
        for name in stale_files.iter().chain(&kept_files) {
            fs::write(base.join(name), b"").expect("create fixture file");
        }
        let outside = dirs.state_dir().join("outside");
        fs::create_dir_all(&outside).expect("create junction target");
        fs::write(outside.join("keep"), b"keep").expect("write junction target file");
        let junction = base.join(format!("ssh-{dead}-3"));
        let status = Command::new(comspec())
            .args(["/D", "/Q", "/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&outside)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("run mklink");
        assert!(status.success(), "create junction fixture");

        assert_eq!(
            super::sweep_stale_remote_private_entries(&base),
            stale_dirs.len() + stale_files.len()
        );
        for name in stale_dirs.iter().chain(&stale_files) {
            assert!(
                !base.join(name).exists(),
                "stale entry must be removed: {name}"
            );
        }
        for name in kept_dirs.iter().chain(&kept_files) {
            assert!(base.join(name).exists(), "entry must be kept: {name}");
        }
        assert!(
            fs::symlink_metadata(&junction).is_ok(),
            "junction must be kept"
        );
        assert!(
            outside.join("keep").is_file(),
            "junction target must not be touched"
        );
        drop(exited);
    }

    /// 建私有目录项时顺带清理（每个基准每进程一次）：之后才出现的陈旧项留给下一个进程，
    /// 开销不落在每次操作上。
    #[test]
    fn creating_a_private_dir_sweeps_its_base_once() {
        let _dirs = crate::config::test_dirs::isolate_dirs("remote-sweep-once");
        let base = super::remote_private_temp_base();
        fs::create_dir_all(&base).expect("create remote base");
        let exited = exited_child();
        let dead = exited.id();
        let stale = base.join(format!("ssh-{dead}-0"));
        fs::create_dir(&stale).expect("create stale dir");

        let created = super::create_remote_ssh_config_dir("ctl").expect("create ssh config dir");
        assert!(created.starts_with(&base), "{}", created.display());
        assert!(!stale.exists(), "the first private dir sweeps its base");

        let later = base.join(format!("herdr-ap-{dead}-1.sock"));
        fs::write(&later, b"").expect("create later stale endpoint");
        let endpoint = super::remote_bridge_endpoint_path("readable.sock", "herdr-ap-0-0.sock");
        assert!(endpoint.starts_with(&base), "{}", endpoint.display());
        assert!(later.exists(), "a base is swept only once per process");
        drop(exited);
    }

    /// 只用桥接端点、从不建受管配置目录的基准，也在第一次算端点路径时清理。
    #[test]
    fn bridge_endpoint_path_sweeps_stale_endpoints_on_first_use() {
        let _dirs = crate::config::test_dirs::isolate_dirs("remote-sweep-endpoint");
        let base = super::remote_private_temp_base();
        fs::create_dir_all(&base).expect("create remote base");
        let exited = exited_child();
        let stale = base.join(format!("herdr-s-{}-0123456789abcdef.sock", exited.id()));
        fs::write(&stale, b"").expect("create stale endpoint");

        let endpoint = super::remote_bridge_endpoint_path("readable.sock", "herdr-ap-0-0.sock");
        assert!(endpoint.starts_with(&base), "{}", endpoint.display());
        assert!(!stale.exists(), "stale endpoint marker must be removed");
        drop(exited);
    }

    /// 真实子进程：运行中算活着；退出并被 wait 之后（句柄仍握在手里，pid 不会被复用）算
    /// 已退出。
    #[test]
    fn process_liveness_follows_a_real_child_until_it_exits() {
        let mut child = Command::new("ping")
            .args(["-n", "30", "127.0.0.1"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn long-running child");
        let pid = child.id();
        assert!(super::process_exists(pid));
        assert!(super::process_alive_excluding_zombies(pid));
        assert!(!super::process_has_exited(pid));

        child.kill().expect("kill child");
        child.wait().expect("wait for child");
        assert!(!super::process_exists(pid));
        assert!(!super::process_alive_excluding_zombies(pid));
        assert!(super::process_has_exited(pid));
    }

    /// 退出码恰好等于 `STILL_ACTIVE`（259）的进程已经退出：只看退出码会把它永远当成活着，
    /// 按进程对象是否 signaled 判定才对。
    #[test]
    fn process_that_exited_with_the_still_active_code_is_gone() {
        let mut child = Command::new(comspec())
            .args(["/D", "/Q", "/C", "exit 259"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn child");
        let status = child.wait().expect("wait for child");
        assert_eq!(status.code(), Some(259));
        assert!(!super::process_exists(child.id()));
        assert!(!super::process_alive_excluding_zombies(child.id()));
        assert!(super::process_has_exited(child.id()));
    }

    const TEARDOWN_TEST_CHILD_ENV: &str = "HERDR_TEST_PROCESS_TEARDOWN_CHILD";
    const TEARDOWN_TEST_CHILD_READY: &str = "herdr-teardown-child-ready";

    /// 存活判定报「已退出」时进程的句柄表已经释放：它占着的 cwd 立刻删得掉。Windows 先公布
    /// 退出码、再逐个结束线程、关句柄表，最后才把进程对象置为 signaled；只看退出码会在 cwd
    /// 仍被占着时就报已退出（pane 收尾后紧接着删 worktree 目录因此失败）。子进程是本测试
    /// 二进制自己，起几百个线程把这段窗口拉长到毫秒级。
    #[test]
    fn a_process_reported_gone_no_longer_holds_its_cwd() {
        if std::env::var_os(TEARDOWN_TEST_CHILD_ENV).is_some() {
            for _ in 0..400 {
                let _ = thread::Builder::new()
                    .stack_size(64 * 1024)
                    .spawn(|| thread::sleep(Duration::from_secs(60)));
            }
            println!("{TEARDOWN_TEST_CHILD_READY}");
            thread::sleep(Duration::from_secs(60));
            return;
        }

        let dirs = crate::config::test_dirs::isolate_dirs("process-cwd-release");
        let cwd = dirs.state_dir().join("cwd");
        fs::create_dir_all(&cwd).expect("create cwd fixture");
        let mut child = Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "platform::windows::tests::a_process_reported_gone_no_longer_holds_its_cwd",
                "--nocapture",
            ])
            .env(TEARDOWN_TEST_CHILD_ENV, "1")
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn teardown child");
        let stdout = child.stdout.take().expect("teardown child stdout");
        let ready = std::io::BufRead::lines(std::io::BufReader::new(stdout))
            .map_while(Result::ok)
            .any(|line| line.contains(TEARDOWN_TEST_CHILD_READY));
        if !ready {
            let _ = child.kill();
            let _ = child.wait();
            panic!("teardown child did not start");
        }
        let pid = child.id();
        child.kill().expect("kill child");
        let deadline = Instant::now() + Duration::from_secs(10);
        while super::process_exists(pid) {
            assert!(Instant::now() < deadline, "killed child never went away");
            thread::yield_now();
        }
        let removed = fs::remove_dir(&cwd);
        child.wait().expect("wait for child");
        removed.expect("the cwd of a process reported gone must be removable");
    }

    #[test]
    fn process_liveness_rejects_pid_zero_and_missing_pids() {
        for pid in [0, 4_294_967_292] {
            assert!(!super::process_exists(pid), "{pid}");
            assert!(!super::process_alive_excluding_zombies(pid), "{pid}");
        }
        assert!(super::process_exists(std::process::id()));
        assert!(!super::process_has_exited(0));
        assert!(super::process_has_exited(4_294_967_292));
    }

    /// 测试结束（含 panic）时终止登记过的进程，不留下孤儿。
    struct KillOnDrop(Vec<super::ProcessSessionMember>);

    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            super::signal_session_members(&self.0, super::Signal::Kill);
        }
    }

    fn spawn_ping() -> std::process::Child {
        Command::new("ping")
            .args(["-n", "60", "127.0.0.1"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn ping")
    }

    fn live_member(pid: u32) -> super::ProcessSessionMember {
        let session = super::process_session_id(pid).expect("anchor for a live process");
        super::ProcessSessionMember {
            pid,
            instance: session.instance,
        }
    }

    fn wait_until_members_exit(members: &[super::ProcessSessionMember]) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while let Some(alive) = members
            .iter()
            .find(|member| super::session_member_alive(**member))
        {
            assert!(
                Instant::now() < deadline,
                "process {} never exited",
                alive.pid
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// 身份核对不过的成员收不到信号：创建时间对不上（pid 已换了主人）、不带实例标记、或者是
    /// 本进程自己。`Hangup` 与 `Terminate` 在 Windows 上什么都不发，只有 `Kill` 真的终止进程。
    #[test]
    fn only_verified_session_members_are_terminated() {
        let mut child = spawn_ping();
        let genuine = live_member(child.id());
        let _cleanup = KillOnDrop(vec![genuine]);
        let recycled = super::ProcessSessionMember {
            instance: genuine.instance + 1,
            ..genuine
        };
        let untracked = super::ProcessSessionMember {
            instance: 0,
            ..genuine
        };
        let this_process = live_member(std::process::id());

        assert!(super::session_member_alive(genuine));
        assert!(!super::session_member_alive(recycled));
        assert!(!super::session_member_alive(untracked));

        super::signal_session_members(&[genuine], super::Signal::Hangup);
        super::signal_session_members(&[genuine], super::Signal::Terminate);
        super::signal_session_members(&[recycled, untracked, this_process], super::Signal::Kill);
        thread::sleep(Duration::from_millis(200));
        assert!(
            child.try_wait().expect("poll child").is_none(),
            "neither Hangup, Terminate nor an unverified member may terminate the process"
        );

        super::signal_session_members(&[genuine], super::Signal::Kill);
        let status = child.wait().expect("wait for child");
        assert_eq!(status.code(), Some(1));
        assert!(!super::session_member_alive(genuine));
    }

    const ORPHAN_ROOT_ENV: &str = "HERDR_TEST_PANE_TREE_ORPHAN_ROOT";
    const ORPHAN_ROOT_READY: &str = "herdr-pane-tree-orphan-root-ready";

    /// 根进程（pane shell）先退出、本测试已释放它的句柄：它退出前拉起的子进程（好比
    /// `start notepad` 之后 shell 先收到关闭事件退出）仍凭锚点归这棵树，并且真的被终止。根进程
    /// 对象若还被别人（例如杀毒软件）引用，它作为已退出的成员列出；否则走孤儿规则。
    #[test]
    fn orphans_of_an_exited_pane_root_are_still_terminated() {
        if std::env::var_os(ORPHAN_ROOT_ENV).is_some() {
            // 留下孤儿正是本测试要造的局面：根进程退出时故意不等它。Windows 没有僵尸进程，丢掉
            // `Child` 只是关掉句柄，测试最后会终止它。
            #[allow(clippy::zombie_processes)]
            let orphan = spawn_ping();
            println!("{ORPHAN_ROOT_READY} {}", orphan.id());
            // 测试关掉 stdin 后才退出，ping 留下成为孤儿。
            let _ = std::io::Read::read_to_end(&mut std::io::stdin(), &mut Vec::new());
            return;
        }

        let mut root = Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "platform::windows::tests::orphans_of_an_exited_pane_root_are_still_terminated",
                "--nocapture",
            ])
            .env(ORPHAN_ROOT_ENV, "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn pane root");
        let mut stdout = std::io::BufReader::new(root.stdout.take().expect("pane root stdout"));
        let orphan_pid = std::io::BufRead::lines(&mut stdout)
            .map_while(Result::ok)
            .find_map(|line| {
                line.strip_prefix(ORPHAN_ROOT_READY)
                    .and_then(|pid| pid.trim().parse::<u32>().ok())
            });
        let Some(orphan_pid) = orphan_pid else {
            let _ = root.kill();
            let _ = root.wait();
            panic!("pane root did not start its child");
        };
        let orphan = live_member(orphan_pid);
        let _cleanup = KillOnDrop(vec![orphan]);
        // 让锚点时刻落在孤儿创建之后的时钟节拍里（粗粒度时钟一拍最长约 16 ms）。
        thread::sleep(Duration::from_millis(50));
        let session = super::process_session_id(root.id()).expect("anchor for the live root");

        // 不读到 EOF：ping 继承了根进程的 stdout 管道，要等它退出才会 EOF。
        drop(stdout);
        drop(root.stdin.take());
        root.wait().expect("wait for pane root");
        let root_pid = root.id();
        // 释放根进程句柄：此后 pid 可能被复用，只剩锚点认得原来的根进程。
        drop(root);

        let members = super::session_members_batch(&[session])
            .into_iter()
            .next()
            .unwrap_or_default();
        assert!(
            members.contains(&orphan),
            "the orphan of the exited root must stay reachable: {members:?}"
        );
        assert!(
            members
                .iter()
                .filter(|member| member.pid == root_pid)
                .all(|member| !super::session_member_alive(*member)),
            "the root has exited: {members:?}"
        );
        super::signal_session_members(&members, super::Signal::Kill);
        wait_until_members_exit(&members);
    }

    const DAEMON_TREE_ENV: &str = "HERDR_TEST_PANE_TREE_DAEMON";
    const DAEMON_TREE_READY: &str = "herdr-pane-tree-daemon-ready";

    fn relaunch_daemon_tree_test(role: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().expect("test executable"));
        command
            .args([
                "--exact",
                "platform::windows::tests::herdr_server_daemons_survive_the_pane_that_started_them",
                "--nocapture",
            ])
            .env(DAEMON_TREE_ENV, role)
            .stdin(Stdio::null())
            .stderr(Stdio::null());
        command
    }

    /// 把本进程换进一个关闭时不终止进程的作业，返回作业句柄（握到进程退出）。测试运行器可能用
    /// 关闭即终止的作业包住测试进程，那样就不算脱离的 daemon；生产里 daemon 在这种作业里时会改走
    /// WMI 拉起，根本不在 pane 的进程树下。
    fn enter_non_killing_job() -> std::os::windows::io::OwnedHandle {
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

        let job = unsafe { super::CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        assert!(!job.is_null(), "create test job");
        let job = unsafe { OwnedHandle::from_raw_handle(job) };
        assert_ne!(
            unsafe {
                super::AssignProcessToJobObject(job.as_raw_handle(), super::GetCurrentProcess())
            },
            0,
            "assign to test job"
        );
        job
    }

    /// pane 里的 herdr 客户端拉起的 server daemon（脱离控制台、挂着标记）不随这个 pane 一起终止：
    /// 它连同它下面的嵌套会话都留着，同一个 pane 里的其它进程照常终止。普通进程没有标记，daemon
    /// 退出后标记随之消失。
    #[test]
    fn herdr_server_daemons_survive_the_pane_that_started_them() {
        match std::env::var(DAEMON_TREE_ENV).as_deref() {
            Ok("client") => {
                // daemon 本来就要比拉起它的客户端活得久，这里故意不等它。
                #[allow(clippy::zombie_processes)]
                let daemon = {
                    let mut command = relaunch_daemon_tree_test("daemon");
                    command.stdout(Stdio::null());
                    super::detach_server_daemon_command(&mut command);
                    command.spawn().expect("spawn server daemon")
                };
                let mut sibling = spawn_ping();
                println!("{DAEMON_TREE_READY} {} {}", daemon.id(), sibling.id());
                thread::sleep(Duration::from_secs(60));
                let _ = sibling.kill();
                let _ = sibling.wait();
                return;
            }
            Ok("daemon") => {
                use std::os::windows::process::CommandExt;

                let _job = enter_non_killing_job();
                super::announce_detached_server_daemon();
                // daemon 自己的 pane。daemon 没有控制台，不让 ping 弹出窗口。
                let mut nested = Command::new("ping")
                    .args(["-n", "60", "127.0.0.1"])
                    .creation_flags(super::CREATE_NO_WINDOW)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .expect("spawn nested pane process");
                thread::sleep(Duration::from_secs(60));
                let _ = nested.kill();
                let _ = nested.wait();
                return;
            }
            _ => {}
        }

        let mut client = relaunch_daemon_tree_test("client")
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn pane client");
        let stdout = client.stdout.take().expect("pane client stdout");
        let ready = std::io::BufRead::lines(std::io::BufReader::new(stdout))
            .map_while(Result::ok)
            .find_map(|line| {
                let mut pids = line.strip_prefix(DAEMON_TREE_READY)?.split_whitespace();
                let daemon = pids.next()?.parse::<u32>().ok()?;
                let sibling = pids.next()?.parse::<u32>().ok()?;
                Some((daemon, sibling))
            });
        let Some((daemon_pid, sibling_pid)) = ready else {
            let _ = client.kill();
            let _ = client.wait();
            panic!("pane client did not start the daemon");
        };
        let client_member = live_member(client.id());
        let daemon = live_member(daemon_pid);
        let sibling = live_member(sibling_pid);
        let mut cleanup = KillOnDrop(vec![client_member, sibling, daemon]);

        // 等 daemon 挂上标记、拉起自己的 pane。
        let deadline = Instant::now() + Duration::from_secs(30);
        let nested_session = loop {
            let daemon_tree = super::process_tree_members(daemon_pid);
            if super::server_daemon_marker_exists(daemon) && daemon_tree.len() >= 2 {
                break daemon_tree;
            }
            assert!(
                Instant::now() < deadline,
                "daemon never announced itself: {daemon_tree:?}"
            );
            thread::sleep(Duration::from_millis(20));
        };
        cleanup.0.extend(nested_session.iter().copied());
        assert!(nested_session.contains(&daemon));
        for plain in [client_member, sibling, live_member(std::process::id())] {
            assert!(
                !super::server_daemon_marker_exists(plain),
                "process {} is not a server daemon",
                plain.pid
            );
        }
        // 生产判定读真实进程的映像与命令行：客户端的映像同样以 herdr 开头，但命令行不是
        // `<exe> server`，不算 daemon。
        let open = |member: super::ProcessSessionMember| {
            super::ProcessHandle::open(member.pid, super::PROCESS_QUERY_LIMITED_INFORMATION)
                .expect("open live process")
        };
        assert!(super::is_herdr_server_daemon(daemon, &open(daemon)));
        for plain in [client_member, sibling] {
            assert!(
                !super::is_herdr_server_daemon(plain, &open(plain)),
                "process {} is not a server daemon",
                plain.pid
            );
        }

        let session = super::process_session_id(client.id()).expect("anchor for the pane root");
        let members = super::session_members_batch(&[session])
            .into_iter()
            .next()
            .unwrap_or_default();
        assert!(
            members.contains(&client_member) && members.contains(&sibling),
            "the pane's own processes are members: {members:?}"
        );
        assert!(
            nested_session.iter().all(|kept| !members.contains(kept)),
            "the daemon and its sessions are not pane members: {members:?}"
        );
        super::signal_session_members(&members, super::Signal::Kill);
        wait_until_members_exit(&members);
        assert!(
            nested_session
                .iter()
                .all(|kept| super::session_member_alive(*kept)),
            "closing the pane must leave the daemon and its sessions running"
        );
        let _ = client.wait();

        super::signal_session_members(&nested_session, super::Signal::Kill);
        wait_until_members_exit(&nested_session);
        assert!(
            !super::server_daemon_marker_exists(daemon),
            "the marker goes away with the daemon"
        );
    }

    #[test]
    fn local_resources_authorize_account_without_admin_rights() {
        use interprocess::local_socket::traits::Listener as _;
        use std::io::{Read, Write};
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
        use windows_sys::Win32::Security::{
            CreateRestrictedToken, GetTokenInformation, ImpersonateLoggedOnUser, RevertToSelf,
            TokenUser, DISABLE_MAX_PRIVILEGE, LUA_TOKEN, SID_AND_ATTRIBUTES, TOKEN_DUPLICATE,
            TOKEN_QUERY, TOKEN_USER,
        };
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

        let path =
            std::env::temp_dir().join(format!("herdr-user-pipe-{}.sock", std::process::id()));
        let listener = crate::ipc::bind_local_listener(&path).unwrap();
        let mut raw_token = std::ptr::null_mut();
        assert_ne!(
            unsafe {
                OpenProcessToken(
                    GetCurrentProcess(),
                    TOKEN_DUPLICATE | TOKEN_QUERY,
                    &mut raw_token,
                )
            },
            0
        );
        let token = unsafe { OwnedHandle::from_raw_handle(raw_token) };
        let mut restricted = std::ptr::null_mut();
        assert_ne!(
            unsafe {
                CreateRestrictedToken(
                    token.as_raw_handle(),
                    DISABLE_MAX_PRIVILEGE | LUA_TOKEN,
                    0,
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                    &mut restricted,
                )
            },
            0
        );
        let restricted = unsafe { OwnedHandle::from_raw_handle(restricted) };
        let private_path = path.with_extension("private");
        super::create_config_temporary(&private_path, true)
            .unwrap()
            .write_all(b"recovery")
            .unwrap();
        assert_ne!(
            unsafe { ImpersonateLoggedOnUser(restricted.as_raw_handle()) },
            0
        );
        let connection = crate::ipc::connect_local_stream(&path);
        let private_read = fs::read(&private_path);
        let private_write = fs::write(&private_path, b"updated");
        let reverted = unsafe { RevertToSelf() };
        assert_ne!(reverted, 0);
        assert_eq!(private_read.unwrap(), b"recovery");
        private_write.unwrap();
        let mut client = connection.expect("the account SID must work without admin membership");
        let mut server = listener.accept().unwrap();
        client.write_all(b"account").unwrap();
        let mut received = [0; 7];
        server.read_exact(&mut received).unwrap();
        assert_eq!(&received, b"account");

        // Removing the account SID must not leave access through Everyone or
        // another ordinary group. This exercises the real DACL access check.
        let mut size = 0;
        unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenUser,
                std::ptr::null_mut(),
                0,
                &mut size,
            )
        };
        let mut user = vec![0usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
        assert_ne!(
            unsafe {
                GetTokenInformation(
                    token.as_raw_handle(),
                    TokenUser,
                    user.as_mut_ptr().cast(),
                    size,
                    &mut size,
                )
            },
            0
        );
        let user = unsafe { &*user.as_ptr().cast::<TOKEN_USER>() };
        let disabled = SID_AND_ATTRIBUTES {
            Sid: user.User.Sid,
            Attributes: 0,
        };
        let mut without_account = std::ptr::null_mut();
        assert_ne!(
            unsafe {
                CreateRestrictedToken(
                    token.as_raw_handle(),
                    DISABLE_MAX_PRIVILEGE | LUA_TOKEN,
                    1,
                    &disabled,
                    0,
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                    &mut without_account,
                )
            },
            0
        );
        let without_account = unsafe { OwnedHandle::from_raw_handle(without_account) };
        assert_ne!(
            unsafe { ImpersonateLoggedOnUser(without_account.as_raw_handle()) },
            0
        );
        let denied = crate::ipc::connect_local_stream(&path);
        let private_denied = fs::read(&private_path);
        let reverted = unsafe { RevertToSelf() };
        assert_ne!(reverted, 0);
        assert_eq!(
            denied.unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            private_denied.unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert_ne!(
            unsafe { ImpersonateLoggedOnUser(restricted.as_raw_handle()) },
            0
        );
        let removed = fs::remove_file(private_path);
        let reverted = unsafe { RevertToSelf() };
        assert_ne!(reverted, 0);
        removed.expect("the account must be able to remove its private recovery files");
        drop(client);
        drop(server);
        drop(listener);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn clipboard_text_equals_normalizes_line_endings() {
        assert!(super::clipboard_text_equals("hello", b"hello"));
        assert!(super::clipboard_text_equals("a\r\nb", b"a\nb"));
        assert!(super::clipboard_text_equals("a\nb", b"a\r\nb"));
        assert!(!super::clipboard_text_equals("hello ", b"hello"));
        assert!(!super::clipboard_text_equals("hello", b"world"));
        assert!(!super::clipboard_text_equals("hello", &[0xff]));
        assert!(!super::clipboard_text_equals("a\rb", b"a\nb"));
    }

    #[test]
    fn clipboard_format_check_rejects_rich_content() {
        for format in [
            super::CF_UNICODETEXT,
            super::CF_TEXT,
            super::CF_OEMTEXT,
            super::CF_LOCALE,
        ] {
            assert!(super::plain_text_clipboard_format(format as u32));
        }
        assert!(!super::plain_text_clipboard_format(super::CF_DIB as u32));
        assert!(!super::plain_text_clipboard_format(0xC000));
    }

    #[test]
    fn windows_standard_plugin_runtime_paths_drop_only_disk_and_unc_verbatim_prefixes() {
        assert_eq!(
            super::standard_windows_path(std::path::Path::new(r"\\?\C:\plugins\example")),
            Some(std::path::PathBuf::from(r"C:\plugins\example"))
        );
        assert_eq!(
            super::standard_windows_path(std::path::Path::new(
                r"\\?\UNC\server\share\plugins\example"
            )),
            Some(std::path::PathBuf::from(r"\\server\share\plugins\example"))
        );
        assert_eq!(
            super::standard_windows_path(std::path::Path::new(
                r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}\plugins"
            )),
            None
        );
    }

    #[test]
    fn windows_plugin_runtime_path_keeps_extended_path_when_normal_form_is_not_equivalent() {
        let path = std::path::PathBuf::from(format!(
            r"\\?\C:\herdr-missing-plugin-runtime-path-{}",
            std::process::id()
        ));
        assert_eq!(super::plugin_runtime_path_platform(&path), path);
    }

    #[test]
    fn windows_plugin_runtime_path_keeps_verbatim_root_beyond_max_path() {
        use std::os::windows::ffi::OsStrExt;

        let base = unique_temp_path("herdr-plugin-runtime-path-limit-test");
        fs::create_dir_all(&base).expect("create test base");
        let extended_base = base.canonicalize().expect("canonicalize test base");
        let normal_base = super::standard_windows_path(&extended_base)
            .expect("test base has a standard drive path");
        let normal_base_len = normal_base.as_os_str().encode_wide().count();
        let root_at_length = |length| {
            let component_len = length - normal_base_len - 1;
            let path = extended_base.join("é".repeat(component_len));
            fs::create_dir(&path).expect("create length-boundary test root");
            path.canonicalize()
                .expect("canonicalize length-boundary test root")
        };

        let at_limit = root_at_length(windows_sys::Win32::Foundation::MAX_PATH as usize - 2);
        let at_limit_normal =
            super::standard_windows_path(&at_limit).expect("convert root at MAX_PATH boundary");
        assert_eq!(
            super::plugin_runtime_path_platform(&at_limit),
            at_limit_normal
        );

        let beyond_limit = root_at_length(windows_sys::Win32::Foundation::MAX_PATH as usize - 1);
        assert_eq!(
            super::plugin_runtime_path_platform(&beyond_limit),
            beyond_limit
        );

        fs::remove_dir_all(base).expect("remove test directory");
    }

    #[test]
    fn paste_text_uses_windows_line_endings() {
        assert_eq!(
            super::prepare_paste_text_for_pty_platform("one\ntwo\r\nthree\rfour".to_owned()),
            "one\r\ntwo\r\nthree\rfour"
        );
    }

    #[test]
    fn private_remote_directory_supports_long_paths() {
        let base = unique_temp_path("herdr-private-remote-dir-test");
        fs::create_dir_all(&base).expect("create test base");
        let private = base.join("x".repeat(240));

        super::create_remote_private_dir(&private).expect("create private long-path directory");
        fs::write(private.join("probe"), b"ok").expect("write inherited private file");

        fs::remove_dir_all(base).expect("remove test directory");
    }

    #[test]
    fn windows_conpty_native_encoder_uses_canonical_phase_and_repeat_count() {
        let key = crate::input::TerminalKey::new(
            crossterm::event::KeyCode::Char('7'),
            crossterm::event::KeyModifiers::CONTROL,
        )
        .with_windows_record(crate::input::WindowsKeyRecord {
            key_down: true,
            repeat_count: 3,
            virtual_key_code: 0x37,
            virtual_scan_code: 0x08,
            unicode: 0,
            control_key_state: 0x0008,
        });

        assert_eq!(
            super::encode_windows_conpty_fallback(&key),
            Some(b"\x1b[55;8;0;1;8;3_".to_vec())
        );
        let mut release = key.with_kind(crossterm::event::KeyEventKind::Release);
        release.repeat_count = 3;
        assert_eq!(
            super::encode_windows_conpty_fallback(&release),
            Some(b"\x1b[55;8;0;0;8;1_".to_vec())
        );
    }

    #[test]
    fn windows_conpty_native_encoder_preserves_semantic_escape_fallback() {
        let escape = crate::input::TerminalKey::new(
            crossterm::event::KeyCode::Esc,
            crossterm::event::KeyModifiers::empty(),
        );

        assert_eq!(
            super::encode_windows_conpty_fallback(&escape),
            Some(b"\x1b[27;1;27;1;0;1_\x1b[27;1;27;0;0;1_".to_vec())
        );
        assert_eq!(
            super::encode_windows_conpty_fallback(
                &escape
                    .clone()
                    .with_kind(crossterm::event::KeyEventKind::Repeat),
            ),
            None
        );
        assert_eq!(
            super::encode_windows_conpty_fallback(
                &escape
                    .clone()
                    .with_kind(crossterm::event::KeyEventKind::Release),
            ),
            None
        );
        assert_eq!(
            super::encode_windows_conpty_fallback(&escape.clone().with_vt_bytes(vec![27])),
            None
        );
        assert_eq!(
            super::encode_windows_conpty_fallback(&crate::input::TerminalKey::new(
                crossterm::event::KeyCode::Esc,
                crossterm::event::KeyModifiers::ALT,
            ),),
            None
        );
    }

    #[test]
    fn windows_conpty_native_encoder_preserves_semantic_shift_enter_fallback() {
        let shift_enter = crate::input::TerminalKey::new(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::SHIFT,
        );

        assert_eq!(
            super::encode_windows_conpty_fallback(&shift_enter),
            Some(b"\x1b[13;28;13;1;16;1_".to_vec())
        );
        assert_eq!(
            super::encode_windows_conpty_fallback(&crate::input::TerminalKey::new(
                crossterm::event::KeyCode::Enter,
                crossterm::event::KeyModifiers::empty(),
            )),
            None
        );
    }

    fn native_key(
        virtual_key_code: u16,
        virtual_scan_code: u16,
        unicode: u16,
        control_key_state: u32,
    ) -> crate::input::TerminalKey {
        crate::input::TerminalKey::new(
            crossterm::event::KeyCode::Char('c'),
            crossterm::event::KeyModifiers::CONTROL,
        )
        .with_windows_record(crate::input::WindowsKeyRecord {
            key_down: true,
            repeat_count: 1,
            virtual_key_code,
            virtual_scan_code,
            unicode,
            control_key_state,
        })
    }

    #[test]
    fn windows_conpty_native_encoder_gives_ctrl_letters_their_control_character() {
        // A host that reports the plain letter (or no character) for Ctrl+C would leave
        // MSYS programs, which read SIGINT from the character field, uninterruptible.
        for unicode in [0x63, 0x43, 0] {
            let key = native_key(0x43, 46, unicode, 0x0008);
            assert_eq!(
                super::encode_windows_conpty_fallback(&key),
                Some(b"\x1b[67;46;3;1;8;1_".to_vec()),
                "unicode {unicode:#x}"
            );
            assert_eq!(
                super::encode_windows_conpty_fallback(
                    &key.with_kind(crossterm::event::KeyEventKind::Release)
                ),
                Some(b"\x1b[67;46;3;0;8;1_".to_vec()),
                "release, unicode {unicode:#x}"
            );
        }
        assert_eq!(
            super::encode_windows_conpty_fallback(&native_key(0x5A, 44, 0x7A, 0x0004 | 0x0010)),
            Some(b"\x1b[90;44;26;1;20;1_".to_vec()),
            "right Ctrl with Shift maps Z to 0x1a"
        );
        assert_eq!(
            super::encode_windows_conpty_fallback(&native_key(0x41, 30, 0x61, 0x0008 | 0x0020)),
            Some(b"\x1b[65;30;1;1;40;1_".to_vec()),
            "lock-key flags do not block the rewrite"
        );
    }

    #[test]
    fn windows_conpty_native_encoder_keeps_control_characters_and_non_ctrl_letters() {
        assert_eq!(
            super::encode_windows_conpty_fallback(&native_key(0x43, 46, 3, 0x0008)),
            Some(b"\x1b[67;46;3;1;8;1_".to_vec()),
            "an existing control character is forwarded unchanged"
        );
        assert_eq!(
            super::encode_windows_conpty_fallback(&native_key(0x51, 16, 0x40, 0x0009)),
            Some(b"\x1b[81;16;64;1;9;1_".to_vec()),
            "AltGr (Right Alt + Left Ctrl) keeps the layout character"
        );
        assert_eq!(
            super::encode_windows_conpty_fallback(&native_key(0x43, 46, 0x63, 0x0002 | 0x0008)),
            Some(b"\x1b[67;46;99;1;10;1_".to_vec()),
            "Ctrl+Alt keeps the character"
        );
        assert_eq!(
            super::encode_windows_conpty_fallback(&native_key(0x43, 46, 0x43, 0x0010)),
            Some(b"\x1b[67;46;67;1;16;1_".to_vec()),
            "Shift without Ctrl keeps the character"
        );
        assert_eq!(
            super::encode_windows_conpty_fallback(&native_key(0xBA, 39, 0x3B, 0x0008)),
            Some(b"\x1b[186;39;59;1;8;1_".to_vec()),
            "non-letter keys are not rewritten"
        );
    }

    #[test]
    fn powershell_agent_command_omits_argument_list_when_no_arguments_are_passed() {
        let argv = vec!["opencode".into()];

        assert_eq!(
            super::interactive_shell_command(&argv, "powershell.exe").as_deref(),
            Some("& opencode")
        );
    }

    #[test]
    fn cmd_agent_command_encodes_edge_arguments_without_cmd_expansion() {
        use base64::Engine as _;

        assert_eq!(super::super::quote_powershell_arg("@options"), "'@options'");
        let argv = vec![
            "pi".into(),
            String::new(),
            "two words".into(),
            "100%".into(),
            "wow!".into(),
            "a'b".into(),
            "--model".into(),
        ];
        let command = super::interactive_shell_command(&argv, "cmd.exe").unwrap();
        let encoded = command.split_whitespace().last().unwrap();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .unwrap();
        let utf16 = bytes
            .chunks_exact(2)
            .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
            .collect::<Vec<_>>();
        assert_eq!(
            String::from_utf16(&utf16).unwrap(),
            "if((Get-Command pi -ErrorAction SilentlyContinue).CommandType -eq 'ExternalScript'){& pi '' 'two words' '100%' 'wow!' 'a''b' '--model'}else{Start-Process -FilePath pi -ArgumentList '\"\" \"two words\" 100% wow! a''b --model' -NoNewWindow -Wait}"
        );
    }

    #[test]
    fn windows_shells_round_trip_agent_arguments_through_a_real_command() {
        let _lock = crate::integration::integration_env_lock();
        let base = unique_temp_path("herdr-agent-argv");
        fs::create_dir_all(&base).unwrap();
        let helper = base.join("pi.cmd");
        fs::write(
            &helper,
            "@echo off\r\n>\"%HERDR_ARGV_CAPTURE%\" (\r\necho(%~1\r\necho(%~2\r\necho(%~3\r\necho(%~4\r\necho(%~5\r\necho(%~6\r\necho(%~7\r\n)\r\n",
        )
        .unwrap();
        let argv = vec![
            "pi".into(),
            String::new(),
            "two words".into(),
            "100%".into(),
            "wow!".into(),
            "a'b".into(),
            "@options".into(),
            "--model".into(),
        ];
        let inherited_path = std::env::var_os("PATH").unwrap_or_default();
        let path = format!("{};{}", base.display(), inherited_path.to_string_lossy());
        let run_command = |shell: &str, command: &str, capture: &std::path::Path| {
            let mut process = if shell == "cmd.exe" {
                let mut process = Command::new("cmd.exe");
                process.args(["/d", "/c", command]);
                process
            } else {
                let mut process = Command::new("powershell.exe");
                process.args(["-NoLogo", "-NoProfile", "-Command", command]);
                process
            };
            process
                .env("PATH", &path)
                .env("HERDR_ARGV_CAPTURE", capture)
                .env("PSExecutionPolicyPreference", "Bypass")
                .status()
                .unwrap()
        };

        for shell in ["powershell.exe", "cmd.exe"] {
            let no_args_capture = base.join(format!("{shell}-no-args.txt"));
            let no_args_command = super::interactive_shell_command(&["pi".into()], shell).unwrap();
            let status = run_command(shell, &no_args_command, &no_args_capture);
            assert!(status.success(), "{shell} argument-free command failed");
            assert_eq!(
                fs::read_to_string(no_args_capture)
                    .unwrap()
                    .replace("\r\n", "\n"),
                "\n\n\n\n\n\n\n"
            );

            let capture = base.join(format!("{shell}.txt"));
            let command = super::interactive_shell_command(&argv, shell).unwrap();
            let status = run_command(shell, &command, &capture);
            assert!(status.success(), "{shell} command failed");
            assert_eq!(
                fs::read_to_string(capture).unwrap().replace("\r\n", "\n"),
                "\ntwo words\n100%\nwow!\na'b\n@options\n--model\n"
            );
        }

        fs::remove_file(helper).unwrap();
        fs::write(
            base.join("pi.ps1"),
            "Set-Content -LiteralPath $env:HERDR_ARGV_CAPTURE -Value @(\"$($args[0])\", \"$($args[1])\", \"$($args[2])\", \"$($args[3])\", \"$($args[4])\", \"$($args[5])\", \"$($args[6])\")\r\n",
        )
        .unwrap();
        for shell in ["powershell.exe", "cmd.exe"] {
            let capture = base.join(format!("{shell}-ps1.txt"));
            let command = super::interactive_shell_command(&argv, shell).unwrap();
            let status = run_command(shell, &command, &capture);
            assert!(status.success(), "{shell} PowerShell script command failed");
            assert_eq!(
                fs::read_to_string(capture).unwrap().replace("\r\n", "\n"),
                "\ntwo words\n100%\nwow!\na'b\n@options\n--model\n"
            );
        }

        let _ = fs::remove_dir_all(base);
    }

    const CONSOLE_TEST_CHILD_ENV: &str = "HERDR_TEST_CONSOLE_CHILD_MODE";
    const CONSOLE_TEST_PARENT_PID_ENV: &str = "HERDR_TEST_CONSOLE_PARENT_PID";
    const WMI_DAEMON_TEST_CHILD_ENV: &str = "HERDR_TEST_WMI_DAEMON_CHILD";

    #[test]
    fn windows_daemon_readiness_checks_job_limits_and_console() {
        const CHILD_ENV: &str = "HERDR_TEST_DAEMON_READINESS_CHILD";
        if std::env::var_os(CHILD_ENV).is_some() {
            use super::*;

            let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            assert!(!job.is_null(), "create test job");
            let job = unsafe { OwnedHandle::from_raw_handle(job) };
            assert_ne!(
                unsafe { AssignProcessToJobObject(job.as_raw_handle(), GetCurrentProcess()) },
                0,
                "assign child to test job"
            );
            assert!(current_process_is_in_job().unwrap());
            assert!(
                current_process_is_detached_server_daemon(),
                "a console-free process in a non-killing job must be ready"
            );

            for (flags, ready) in [(JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, false), (0, true)] {
                let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
                limits.BasicLimitInformation.LimitFlags = flags;
                assert_ne!(
                    unsafe {
                        SetInformationJobObject(
                            job.as_raw_handle(),
                            JobObjectExtendedLimitInformation,
                            std::ptr::from_ref(&limits).cast(),
                            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                        )
                    },
                    0,
                    "set test job limits"
                );
                assert_eq!(current_process_is_detached_server_daemon(), ready);
            }
            assert_ne!(unsafe { AllocConsole() }, 0, "allocate test console");
            assert!(!current_process_is_detached_server_daemon());
            assert_ne!(unsafe { FreeConsole() }, 0, "release test console");
            return;
        }

        let mut child = Command::new(std::env::current_exe().unwrap());
        child
            .args([
                "--exact",
                "platform::windows::tests::windows_daemon_readiness_checks_job_limits_and_console",
                "--nocapture",
            ])
            .env(CHILD_ENV, "1");
        super::detach_server_daemon_command(&mut child);
        let output = child.output().expect("run daemon readiness child");
        assert!(
            output.status.success(),
            "daemon readiness child failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn windows_environment_keys_use_unicode_case_insensitive_ordering() {
        assert_eq!(
            super::windows_environment_key_cmp("hérdr", "HÉRDR"),
            std::cmp::Ordering::Equal
        );
    }

    #[test]
    fn windows_wmi_daemon_preserves_environment_and_working_directory() {
        if let Some(capture) = std::env::var_os(WMI_DAEMON_TEST_CHILD_ENV) {
            let cwd = std::env::current_dir().expect("WMI daemon test working directory");
            fs::write(
                capture,
                format!(
                    "{}\n{}\n{}",
                    cwd.display(),
                    unsafe { GetConsoleWindow() }.is_null(),
                    super::current_process_is_detached_server_daemon()
                ),
            )
            .expect("write WMI daemon test capture");
            return;
        }

        let base = unique_temp_path("herdr-wmi-daemon-test");
        fs::create_dir_all(&base).unwrap();
        let capture = base.join("capture.txt");
        let test_exe = std::env::current_exe().expect("resolve test executable");
        let mut child = Command::new(test_exe);
        child
            .arg("windows_wmi_daemon_preserves_environment_and_working_directory")
            .current_dir(&base)
            .env(WMI_DAEMON_TEST_CHILD_ENV, &capture)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        let pid = super::launch_server_daemon_with_wmi(&child)
            .expect("launch detached process through WMI");
        assert_ne!(pid, 0, "WMI returned an invalid process id");

        let expected = format!("{}\ntrue\ntrue", base.display());
        // WMI 拉起在负载下要好几秒；宽上限只在真失败时才等满。
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if fs::read_to_string(&capture).is_ok_and(|captured| captured == expected) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "WMI daemon child did not write the expected capture"
            );
            thread::sleep(Duration::from_millis(50));
        }
        // 子进程写完捕获文件后才退出，退出前一直以 base 为工作目录：有界重试直到放开。
        crate::config::test_dirs::remove_dir_eventually(&base);
    }

    fn console_process_ids() -> Vec<u32> {
        let mut process_ids = vec![0; 8];
        loop {
            let count = unsafe {
                GetConsoleProcessList(process_ids.as_mut_ptr(), process_ids.len() as u32)
            } as usize;
            if count == 0 {
                return Vec::new();
            }
            if count <= process_ids.len() {
                process_ids.truncate(count);
                return process_ids;
            }
            process_ids.resize(count, 0);
        }
    }

    #[test]
    fn windows_background_and_server_daemon_commands_do_not_have_consoles() {
        if let Some(mode) = std::env::var_os(CONSOLE_TEST_CHILD_ENV) {
            assert!(
                unsafe { GetConsoleWindow() }.is_null(),
                "{} child opened or inherited a console window",
                mode.to_string_lossy()
            );
            let parent_pid = std::env::var(CONSOLE_TEST_PARENT_PID_ENV)
                .expect("console test parent pid")
                .parse::<u32>()
                .expect("numeric console test parent pid");
            assert!(
                !console_process_ids().contains(&parent_pid),
                "{} child inherited the parent console",
                mode.to_string_lossy()
            );
            return;
        }

        let allocated_console = if console_process_ids().is_empty() {
            assert_ne!(unsafe { AllocConsole() }, 0, "allocate test console");
            true
        } else {
            false
        };

        let parent_pid = std::process::id().to_string();
        let test_exe = std::env::current_exe().expect("resolve test executable");
        type ConfigureCommand = fn(&mut Command);
        let configurations: [(&str, ConfigureCommand); 2] = [
            ("background", super::configure_background_command_platform),
            ("server daemon", super::detach_server_daemon_command),
        ];
        for (mode, configure) in configurations {
            let mut child = Command::new(&test_exe);
            child
                .arg("windows_background_and_server_daemon_commands_do_not_have_consoles")
                .env(CONSOLE_TEST_CHILD_ENV, mode)
                .env(CONSOLE_TEST_PARENT_PID_ENV, &parent_pid)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            configure(&mut child);

            let status = child.status().expect("spawn console isolation test child");
            assert!(
                status.success(),
                "{mode} child opened or inherited a console"
            );
        }

        let command = format!(
            r#""{}" windows_background_and_server_daemon_commands_do_not_have_consoles"#,
            test_exe.display()
        );
        let status = crate::platform::detached_custom_command_process(&command)
            .env(CONSOLE_TEST_CHILD_ENV, "detached custom command descendant")
            .env(CONSOLE_TEST_PARENT_PID_ENV, &parent_pid)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("spawn detached custom command test child");
        assert!(
            status.success(),
            "detached custom command descendant opened or inherited a console"
        );

        if allocated_console {
            unsafe {
                FreeConsole();
            }
        }
    }

    fn argv_strings(argv: &[std::ffi::OsString]) -> Vec<String> {
        argv.iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn pane_custom_command_uses_cmd() {
        let builder = super::pane_custom_command_pty_builder_with_comspec(
            "echo hello",
            Some(r"C:\Windows\System32\cmd.exe".into()),
        );

        assert_eq!(
            argv_strings(builder.get_argv()),
            [r"C:\Windows\System32\cmd.exe", "/d", "/c"]
        );
    }

    #[test]
    fn detached_custom_command_uses_cmd() {
        let expected_shell = std::env::var_os("ComSpec")
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| r"C:\Windows\System32\cmd.exe".into())
            .to_string_lossy()
            .into_owned();

        let process = super::detached_custom_command_process_platform("echo hello");

        assert_eq!(process.get_program().to_string_lossy(), expected_shell);
        assert_eq!(
            process
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            ["/d", "/c", "echo hello"]
        );
    }

    #[test]
    fn custom_command_falls_back_when_comspec_is_empty() {
        let builder =
            super::pane_custom_command_pty_builder_with_comspec("echo hello", Some("".into()));

        assert_eq!(
            argv_strings(builder.get_argv()),
            [r"C:\Windows\System32\cmd.exe", "/d", "/c"]
        );
    }

    #[test]
    fn detached_custom_command_preserves_quoted_command_tail() {
        let path = unique_temp_path("herdr-raw-command-quotes").with_extension("txt");
        let command = format!(r#"echo "hi" > "{}""#, path.display());

        let status = super::detached_custom_command_process_platform(&command)
            .status()
            .expect("spawn raw command");

        assert!(status.success(), "{status:?}");
        let content = std::fs::read_to_string(&path).expect("read command output");
        let _ = std::fs::remove_file(&path);
        assert!(content.contains(r#""hi""#), "{content:?}");
        assert!(!content.contains(r#"\"hi\""#), "{content:?}");
    }

    #[test]
    fn windows_process_cwd_reads_normalized_child_launch_directory() {
        use std::path::PathBuf;

        let cwd = unique_temp_path("Herdr-Cwd-Case");
        let name = cwd
            .file_name()
            .expect("cwd fixture name")
            .to_string_lossy()
            .into_owned();
        fs::create_dir_all(&cwd).expect("create cwd fixture");
        let cwd = super::normalize_cwd_for_launch_platform(&cwd);
        let launch_cwd = cwd.with_file_name(name.to_ascii_lowercase());

        // 直接起 ping（不经 cmd /C）：kill 掉的就是占着 cwd 的进程，fixture 目录才删得掉；
        // 经 cmd 时 ping 孙进程还会占着目录十秒，目录删不掉、留在临时目录里。
        let mut ping = Command::new("ping")
            .args(["-n", "11", "127.0.0.1"])
            .current_dir(super::normalize_cwd_for_launch_platform(&launch_cwd))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn ping");
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut direct_observed = None;
        while Instant::now() < deadline {
            direct_observed = super::process_cwd(ping.id());
            if direct_observed.as_ref().and_then(|path| path.file_name()) == Some(name.as_ref()) {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
        let _ = ping.kill();
        let _ = ping.wait();

        let changed = cwd.join("Changed");
        fs::create_dir(&changed).expect("create changed cwd");
        let windows = PathBuf::from(std::env::var_os("SystemRoot").expect("Windows SystemRoot"));
        let shells = [
            windows.join("System32").join("cmd.exe"),
            #[cfg(target_pointer_width = "64")]
            windows.join("SysWOW64").join("cmd.exe"),
        ];
        let mut observations = Vec::new();
        for shell in shells {
            let mut child = Command::new(&shell)
                .args(["/D", "/Q", "/K"])
                .current_dir(super::normalize_cwd_for_launch_platform(&launch_cwd))
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn cmd");
            let observe = |expected: &PathBuf| {
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    let observed = super::process_cwd(child.id());
                    if observed.as_ref() == Some(expected) || Instant::now() >= deadline {
                        break observed;
                    }
                    thread::sleep(Duration::from_millis(20));
                }
            };
            let initial = observe(&cwd);
            use std::io::Write;
            writeln!(
                child.stdin.as_ref().unwrap(),
                "cd /d \"{}\"\r",
                changed.display()
            )
            .expect("change cmd cwd");
            let after_cd = observe(&changed);
            let pane_cwd = super::pane_process_cwd(child.id());
            let _ = child.kill();
            let _ = child.wait();
            observations.push((shell, initial, after_cd, pane_cwd));
        }
        let _ = fs::remove_dir_all(&cwd);
        assert_eq!(
            direct_observed.as_ref().and_then(|path| path.file_name()),
            Some(name.as_ref())
        );
        for (shell, initial, after_cd, pane_cwd) in observations {
            assert_eq!(initial, Some(cwd.clone()), "{} launch cwd", shell.display());
            assert_eq!(
                after_cd,
                Some(changed.clone()),
                "{} live cd",
                shell.display()
            );
            assert_eq!(pane_cwd, Some(changed.clone()), "ordinary shell owns cwd");
        }
    }

    #[test]
    fn windows_shim_cwd_uses_only_an_unambiguous_direct_shell() {
        let cases = [
            ("cmdx.exe", vec![(11, 10, "cmd.exe")], Some(11)),
            ("pwsh.exe", vec![(11, 10, "pwsh.exe")], Some(11)),
            (
                "cmdx.exe",
                vec![
                    (11, 10, "cmd.exe"),
                    (12, 11, "pwsh.exe"),
                    (13, 11, "node.exe"),
                ],
                Some(11),
            ),
            (
                "cmdx.exe",
                vec![(11, 10, "node.exe"), (12, 11, "cmd.exe")],
                None,
            ),
            ("cmdx.exe", vec![], None),
            (
                "cmdx.exe",
                vec![(11, 10, "cmd.exe"), (12, 10, "pwsh.exe")],
                None,
            ),
        ];
        for (root_name, children, expected) in cases {
            let mut entries = vec![test_entry(10, 1, root_name, &[root_name])];
            entries.extend(
                children
                    .iter()
                    .map(|&(pid, parent, name)| test_entry(pid, parent, name, &[name])),
            );
            let snapshot = super::ProcessSnapshot::new(entries);
            assert_eq!(
                super::shim_shell_entry(10, &snapshot).map(|entry| entry.pid),
                expected,
                "{root_name}: {children:?}"
            );
        }
    }

    #[test]
    fn windows_shim_cwd_rejects_stale_or_unknown_observations() {
        for (parent, created, image) in [
            (Some(77), Some(200), "cmd.exe"),
            (None, Some(200), "cmd.exe"),
            (Some(10), Some(50), "cmd.exe"),
            (Some(10), None, "cmd.exe"),
            (Some(10), Some(200), "worker.exe"),
            (Some(10), Some(200), ""),
        ] {
            let mut child =
                observation_entry((20, 10, "cmd.exe"), (10, created, image), created, None);
            Arc::get_mut(child.reader.as_mut().unwrap())
                .unwrap()
                .parent_pid = parent;
            let snapshot = super::ProcessSnapshot::new(vec![
                test_entry_with_creation_time(10, 0, "shim.exe", &[], Some(100)),
                child,
            ]);
            assert!(
                super::shim_shell_entry(10, &snapshot).is_none(),
                "parent={parent:?}, created={created:?}, image={image}"
            );
        }
        for unreadable in [10, 30] {
            let snapshot = super::ProcessSnapshot::new(vec![
                test_entry_with_creation_time(
                    10,
                    0,
                    "shim.exe",
                    &[],
                    (unreadable != 10).then_some(100),
                ),
                test_entry_with_creation_time(20, 10, "cmd.exe", &[], Some(200)),
                test_entry_with_creation_time(
                    30,
                    10,
                    "worker.exe",
                    &[],
                    (unreadable != 30).then_some(300),
                ),
            ]);
            assert!(super::shim_shell_entry(10, &snapshot).is_none());
        }
    }

    #[test]
    fn windows_shim_cwd_uses_actual_names_and_reuses_pinned_observations() {
        for panes in [1, 16] {
            let mut entries = Vec::new();
            for pane in 0..panes {
                let pid = 100 + pane * 10;
                entries.push(test_entry(pid, 0, "shim.exe", &[]));
                entries.push(observation_entry(
                    (pid + 1, pid, "worker.exe"),
                    (pid, Some(u64::from(pid + 1)), "cmd.exe"),
                    None,
                    None,
                ));
                entries.push(test_entry(pid + 2, pid, "node.exe", &[]));
            }
            let snapshot = super::ProcessSnapshot::new(entries);
            for _ in 0..3 {
                for pane in 0..panes {
                    let pid = 100 + pane * 10;
                    assert_eq!(
                        super::shim_shell_entry(pid, &snapshot).map(|entry| entry.pid),
                        Some(pid + 1)
                    );
                }
            }
            let pins: Vec<_> = snapshot
                .entries
                .iter()
                .map(|entry| {
                    let reader = entry.reader.as_ref().unwrap();
                    assert_eq!(reader.observations.load(super::AtomicOrdering::Relaxed), 1);
                    assert_eq!(reader.commands.load(super::AtomicOrdering::Relaxed), 0);
                    Arc::downgrade(entry.observation().unwrap())
                })
                .collect();
            drop(snapshot);
            assert!(pins.iter().all(|pin| pin.upgrade().is_none()));
        }
        let snapshot = super::ProcessSnapshot::new(vec![
            test_entry(10, 0, "shim.exe", &[]),
            test_entry(20, 10, "cmd.exe", &[]),
            observation_entry(
                (30, 10, "worker.exe"),
                (10, Some(30), "pwsh.exe"),
                None,
                None,
            ),
        ]);
        assert!(super::shim_shell_entry(10, &snapshot).is_none());
    }

    #[test]
    fn windows_shim_cwd_native_read_checks_reopened_identity_and_releases_pins() {
        let mut child = ObservationTestChild::spawn();
        let pid = child.0.id();
        let parent_pid = std::process::id();
        let snapshot = super::ProcessSnapshot::new(vec![
            super::WindowsProcessEntry::new(parent_pid, 0, "shim.exe".into()),
            super::WindowsProcessEntry::new(pid, parent_pid, "worker.exe".into()),
        ]);
        let shell = super::shim_shell_entry(parent_pid, &snapshot).unwrap();
        let observation = shell.observation().unwrap();
        let super::ProcessIdentity::Handle(handle) = &observation.identity else {
            panic!("native identity required");
        };
        let pin = Arc::downgrade(handle);
        super::PROCESS_INSPECTION_COUNTS.with_borrow_mut(|counts| *counts = Default::default());
        let cwd = super::process_cwd_for_observation(pid, observation).unwrap();
        assert!(cwd.is_absolute());
        assert_eq!(
            super::PROCESS_INSPECTION_COUNTS.with_borrow(|counts| counts.opens),
            1
        );
        assert_eq!(Arc::strong_count(handle), 1);
        for mismatch in 0..5 {
            let mut wrong = super::ProcessObservation {
                identity: observation.identity.clone(),
                parent_pid: observation.parent_pid,
                created: observation.created,
                image: observation.image.clone(),
            };
            match mismatch {
                0 => wrong.created += 1,
                1 => wrong.parent_pid = Some(parent_pid + 1),
                2 => wrong.image = Some("different.exe".into()),
                3 => wrong.parent_pid = None,
                _ => wrong.image = None,
            }
            assert!(super::process_cwd_for_observation(pid, &wrong).is_none());
        }
        assert!(super::process_cwd_for_observation(0, observation).is_none());
        child.0.kill().unwrap();
        child.0.wait().unwrap();
        assert!(super::shim_shell_entry(parent_pid, &snapshot).is_none());
        assert!(super::process_cwd_for_observation(pid, observation).is_none());
        drop(snapshot);
        assert!(pin.upgrade().is_none());
    }

    #[test]
    fn windows_process_environment_reads_runtime_marker() {
        let shell =
            std::env::var_os("ComSpec").unwrap_or_else(|| r"C:\Windows\System32\cmd.exe".into());
        let mut child = Command::new(shell)
            .args(["/D", "/Q", "/C", "ping -n 11 127.0.0.1 > NUL"])
            .env(super::PANE_RUNTIME_MARKER_ENV_VAR, "pane-test")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn cmd");

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut observed = None;
        while Instant::now() < deadline {
            observed = super::process_runtime_marker(&super::WindowsProcessEntry::new(
                child.id(),
                std::process::id(),
                "cmd.exe".into(),
            ));
            if observed.as_deref() == Some("pane-test") {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }

        let _ = child.kill();
        let _ = child.wait();

        assert_eq!(observed.as_deref(), Some("pane-test"));
    }

    #[test]
    fn windows_process_command_line_reads_live_process_with_limited_access() {
        // The point of the fix: the command line must be readable from a handle
        // that does not request `PROCESS_VM_READ`. Verification against a
        // process that actually denies that access needs a hardened host, which
        // this suite cannot provide.
        let handle = super::ProcessHandle::open(
            std::process::id(),
            super::PROCESS_QUERY_LIMITED_INFORMATION,
        )
        .expect("open self with limited access");

        let command_line =
            super::read_process_command_line(handle.0).expect("command line must be readable");
        assert!(
            !command_line.is_empty(),
            "command line for the test process must not be empty"
        );
    }

    #[test]
    fn windows_process_command_line_reads_spawned_process_marker() {
        let shell =
            std::env::var_os("ComSpec").unwrap_or_else(|| r"C:\Windows\System32\cmd.exe".into());
        // `rem` keeps the marker inside cmd.exe's own command line without
        // becoming a target for `ping`, so the process stays alive for the read.
        let mut child = Command::new(shell)
            .args([
                "/D",
                "/Q",
                "/C",
                "ping -n 11 127.0.0.1 > NUL & rem unique-cmdline-marker",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn cmd");

        let command_line =
            super::ProcessHandle::open(child.id(), super::PROCESS_QUERY_LIMITED_INFORMATION)
                .and_then(|process| super::read_process_command_line(process.0));

        let _ = child.kill();
        let _ = child.wait();

        let command_line = command_line.expect("command line must be readable");
        assert!(
            command_line.contains("unique-cmdline-marker"),
            "unexpected command line: {command_line}"
        );
    }

    #[test]
    fn windows_process_tree_selects_direct_agent_descendant() {
        let entries = vec![
            test_entry(10, 1, "powershell.exe", &["powershell.exe"]),
            test_entry(20, 10, "codex.exe", &["codex.exe"]),
        ];
        let snapshot = super::ProcessSnapshot::new(entries);

        let job = super::select_pane_foreground_job_from_snapshot_with_runtime_inspection(
            10,
            &snapshot,
            |_| panic!("Git Bash fallback must not run after normal detection succeeds"),
            |_| panic!("runtime marker must not be read after normal detection succeeds"),
        )
        .unwrap();

        assert_eq!(job.process_group_id, 20);
        assert_eq!(job.processes.len(), 1);
        assert_eq!(job.processes[0].name, "codex.exe");
    }

    #[test]
    fn windows_process_tree_still_inspects_unusual_escaped_argv0() {
        let snapshot = super::ProcessSnapshot::new(vec![
            test_entry(10, 1, "bash.exe", &[r"C:\Program Files\Git\bin\bash.exe"]),
            test_entry(20, 99, "launcher.exe", &["codex.exe"]),
        ]);

        let job = super::select_pane_foreground_job_from_snapshot_with_runtime_inspection(
            10,
            &snapshot,
            |_| true,
            |_| Some("pane-a".to_string()),
        )
        .unwrap();

        assert_eq!(job.process_group_id, 20);
        assert_eq!(job.processes[0].name, "launcher.exe");
    }

    #[test]
    fn windows_process_tree_still_inspects_unusual_descendant_argv0() {
        let entries = vec![
            test_entry(10, 1, "powershell.exe", &["powershell.exe"]),
            test_entry(20, 10, "launcher.exe", &["codex.exe"]),
        ];

        let job = super::select_pane_foreground_job(10, &entries).unwrap();

        assert_eq!(job.process_group_id, 20);
        assert_eq!(job.processes[0].name, "launcher.exe");
    }

    #[test]
    fn windows_process_tree_shares_snapshot_candidates_across_git_bash_panes() {
        let snapshot = super::ProcessSnapshot::new(vec![
            test_entry(10, 1, "bash.exe", &[r"C:\Program Files\Git\bin\bash.exe"]),
            test_entry(
                11,
                10,
                "bash.exe",
                &[r"C:\Program Files\Git\usr\bin\bash.exe"],
            ),
            test_entry(12, 1, "bash.exe", &[r"C:\Program Files\Git\bin\bash.exe"]),
            test_entry(
                20,
                99,
                "sh.exe",
                &[r"C:\Program Files\Git\usr\bin\sh.exe", "/c/npm/codex"],
            ),
            test_entry(
                30,
                20,
                "node.exe",
                &[
                    r"C:\Program Files\nodejs\node.exe",
                    r"C:\Users\user\AppData\Roaming\npm\node_modules\@openai\codex\bin\codex.js",
                ],
            ),
            test_entry(
                40,
                30,
                "codex.exe",
                &[r"C:\npm\node_modules\@openai\codex\bin\codex.exe"],
            ),
            test_entry(50, 98, "claude.exe", &["claude.exe"]),
        ]);
        let mut inspected = Vec::new();
        assert!(snapshot.agent_indices.get().is_none());
        let marker = |entry: &super::WindowsProcessEntry| match entry.pid {
            12 | 50 => Some("pane-b".to_string()),
            _ => Some("pane-a".to_string()),
        };

        let first = super::select_pane_foreground_job_from_snapshot_with_runtime_inspection(
            10,
            &snapshot,
            |_| true,
            |entry| {
                inspected.push(entry.pid);
                marker(entry)
            },
        )
        .unwrap();
        let indices = snapshot.agent_indices.get().unwrap();
        let second = super::select_pane_foreground_job_from_snapshot_with_runtime_inspection(
            12,
            &snapshot,
            |_| true,
            marker,
        )
        .unwrap();

        assert_eq!(first.process_group_id, 20);
        assert_eq!(first.processes[0].name, "sh.exe");
        assert_eq!(second.process_group_id, 50);
        assert_eq!(indices, &[3, 4, 5, 6]);
        assert!(std::ptr::eq(indices, snapshot.agent_indices.get().unwrap()));
        assert_eq!(inspected, vec![10, 20, 30, 40, 50]);
    }

    #[test]
    fn windows_process_tree_skips_runtime_inspection_for_non_git_bash_shell() {
        let entries = vec![
            test_entry(10, 1, "powershell.exe", &["powershell.exe"]),
            test_entry(20, 99, "codex.exe", &["codex.exe"]),
        ];
        let snapshot = super::ProcessSnapshot::new(entries);

        let job = super::select_pane_foreground_job_from_snapshot_with_runtime_inspection(
            10,
            &snapshot,
            |_| false,
            |_| panic!("runtime marker must not be read for non-Git-Bash panes"),
        )
        .unwrap();

        assert_eq!(job.process_group_id, 10);
    }

    #[test]
    fn windows_process_tree_skips_runtime_inspection_without_agent_candidate() {
        let entries = vec![
            test_entry(10, 1, "bash.exe", &[r"C:\Program Files\Git\bin\bash.exe"]),
            test_entry(20, 99, "git.exe", &["git.exe", "status"]),
        ];
        let snapshot = super::ProcessSnapshot::new(entries);

        let job = super::select_pane_foreground_job_from_snapshot_with_runtime_inspection(
            10,
            &snapshot,
            |_| true,
            |_| panic!("runtime marker must not be read without an agent candidate"),
        )
        .unwrap();

        assert_eq!(job.process_group_id, 10);
    }

    #[test]
    fn windows_process_tree_rejects_missing_or_empty_shell_runtime_marker() {
        let entries = vec![
            test_entry(10, 1, "bash.exe", &[r"C:\Program Files\Git\bin\bash.exe"]),
            test_entry(20, 99, "codex.exe", &["codex.exe"]),
        ];
        let snapshot = super::ProcessSnapshot::new(entries);

        for shell_marker in [None, Some(String::new())] {
            let job = super::select_pane_foreground_job_from_snapshot_with_runtime_inspection(
                10,
                &snapshot,
                |_| true,
                |entry| {
                    if entry.pid == 10 {
                        shell_marker.clone()
                    } else {
                        Some("pane-a".to_string())
                    }
                },
            )
            .unwrap();

            assert_eq!(job.process_group_id, 10);
        }
    }

    #[test]
    fn windows_process_tree_rejects_runtime_marker_from_another_pane() {
        let entries = vec![
            test_entry(10, 1, "bash.exe", &[r"C:\Program Files\Git\bin\bash.exe"]),
            test_entry(20, 99, "codex.exe", &["codex.exe"]),
        ];
        let snapshot = super::ProcessSnapshot::new(entries);

        let job = super::select_pane_foreground_job_from_snapshot_with_runtime_inspection(
            10,
            &snapshot,
            |_| true,
            |entry| Some(if entry.pid == 10 { "pane-a" } else { "pane-b" }.to_string()),
        )
        .unwrap();

        assert_eq!(job.process_group_id, 10);
        assert_eq!(job.processes[0].name, "bash.exe");
    }

    #[test]
    fn windows_held_agent_must_still_belong_to_the_pane() {
        let entries = vec![
            test_entry(10, 1, "bash.exe", &[r"C:\Program Files\Git\bin\bash.exe"]),
            test_entry(11, 10, "claude.exe", &["claude.exe"]),
            test_entry(20, 99, "codex.exe", &["codex.exe"]),
            test_entry(30, 98, "vim.exe", &["vim.exe"]),
            test_entry(50, 77, "claude.exe", &["claude.exe"]),
        ];
        let snapshot = super::ProcessSnapshot::new(entries);
        let marker = |pane: &'static str| {
            move |entry: &super::WindowsProcessEntry| {
                Some(if entry.pid == 10 { "pane-a" } else { pane }.to_string())
            }
        };
        let belongs = |pid, git_bash, pane| {
            super::process_belongs_to_pane(10, pid, &snapshot, |_| git_bash, marker(pane))
        };

        assert!(belongs(11, false, "pane-b"), "descendant of the shell");
        assert!(
            belongs(20, true, "pane-a"),
            "escaped agent with the pane marker"
        );
        assert!(!belongs(20, true, "pane-b"), "marker from another pane");
        assert!(
            !belongs(20, false, "pane-a"),
            "escape only applies to Git Bash"
        );
        assert!(!belongs(30, true, "pane-a"), "escaped non-agent process");
        assert!(
            !belongs(40, true, "pane-a"),
            "process gone from the snapshot"
        );
        assert!(
            !belongs(50, false, "pane-a"),
            "agent whose parent chain no longer reaches the shell"
        );
        assert!(
            !super::process_belongs_to_pane(60, 11, &snapshot, |_| true, marker("pane-a")),
            "pane shell gone"
        );
    }

    #[test]
    fn windows_held_agent_liveness_uses_pinned_observations_at_scale() {
        for pane_count in [1, 16] {
            let mut entries = Vec::new();
            for pane in 0..pane_count {
                let shell = 100 + pane * 2;
                entries.push(test_entry(shell, 0, "pwsh.exe", &["pwsh.exe"]));
                entries.push(test_entry(shell + 1, shell, "claude.exe", &["claude.exe"]));
            }
            let readers: Vec<_> = entries
                .iter()
                .map(|entry| entry.reader.clone().unwrap())
                .collect();
            let snapshot = super::ProcessSnapshot::new(entries);
            for _ in 0..3 {
                for pane in 0..pane_count {
                    let shell = 100 + pane * 2;
                    let pid = shell + 1;
                    assert_eq!(
                        super::live_pane_process_group_from_snapshot(
                            shell,
                            pid,
                            u64::from(pid),
                            &snapshot,
                            |_| false,
                            |_| None,
                        ),
                        Some(pid)
                    );
                    assert_eq!(
                        super::live_pane_process_group_from_snapshot(
                            shell,
                            pid,
                            u64::from(pid) + 1,
                            &snapshot,
                            |_| false,
                            |_| None,
                        ),
                        None
                    );
                }
            }
            for reader in readers {
                assert_eq!(reader.observations.load(super::AtomicOrdering::Relaxed), 1);
                assert_eq!(reader.commands.load(super::AtomicOrdering::Relaxed), 0);
            }
            let observations: Vec<_> = snapshot
                .entries
                .iter()
                .map(|entry| Arc::downgrade(entry.observation().unwrap()))
                .collect();
            drop(snapshot);
            assert!(observations
                .iter()
                .all(|observation| observation.upgrade().is_none()));
        }
    }

    #[test]
    fn windows_held_agent_rejects_exited_or_replaced_snapshot_instances() {
        for (shell_running, agent_running) in [(true, false), (false, true)] {
            let entries = vec![
                test_entry(10, 0, "pwsh.exe", &["pwsh.exe"]),
                test_entry(20, 10, "claude.exe", &["claude.exe"]),
            ];
            for (entry, running) in entries.iter().zip([shell_running, agent_running]) {
                entry
                    .observation
                    .set(Some(Arc::new(super::ProcessObservation {
                        identity: super::ProcessIdentity::Stub {
                            running,
                            creation_time: Some(u64::from(entry.pid)),
                        },
                        parent_pid: Some(entry.parent_pid),
                        created: u64::from(entry.pid),
                        image: Some(entry.name.clone()),
                    })))
                    .unwrap();
            }
            let snapshot = super::ProcessSnapshot::new(entries);
            assert_eq!(
                super::live_pane_process_group_from_snapshot(
                    10,
                    20,
                    20,
                    &snapshot,
                    |_| false,
                    |_| None
                ),
                None
            );
        }
        let snapshot = super::ProcessSnapshot::new(vec![
            observation_entry(
                (10, 0, "pwsh.exe"),
                (0, Some(30), "pwsh.exe"),
                Some(30),
                None,
            ),
            test_entry(20, 10, "claude.exe", &["claude.exe"]),
        ]);
        assert_eq!(
            super::live_pane_process_group_from_snapshot(
                10,
                20,
                20,
                &snapshot,
                |_| false,
                |_| None
            ),
            None,
            "a reused shell PID cannot inherit an older agent"
        );
    }

    #[test]
    fn windows_process_tree_rejects_ambiguous_runtime_marker_candidates() {
        let entries = vec![
            test_entry(10, 1, "bash.exe", &[r"C:\Program Files\Git\bin\bash.exe"]),
            test_entry(20, 99, "codex.exe", &["codex.exe"]),
            test_entry(30, 98, "claude.exe", &["claude.exe"]),
        ];
        let snapshot = super::ProcessSnapshot::new(entries);

        let job = super::select_pane_foreground_job_from_snapshot_with_runtime_inspection(
            10,
            &snapshot,
            |_| true,
            |_| Some("pane-a".to_string()),
        )
        .unwrap();

        assert_eq!(job.process_group_id, 10);
        assert_eq!(job.processes[0].name, "bash.exe");
    }

    /// Long-lived shell descendants are classified once per process instance: the verdict is
    /// keyed by pid + creation time, so a reused pid is classified afresh.
    #[test]
    fn windows_agent_classification_is_cached_per_process_instance() {
        let worker =
            test_entry_with_creation_time(30, 10, "node.exe", &["node.exe", "worker.js"], Some(7));
        assert!(!super::process_entry_identifies_agent(&worker));
        assert_eq!(super::cached_agent_classification(30, 7), Some(false));

        let reused = test_entry_with_creation_time(
            30,
            10,
            "node.exe",
            &[
                r"C:\Program Files\nodejs\node.exe",
                r"C:\Users\user\AppData\Roaming\npm\node_modules\@openai\codex\bin\codex.js",
            ],
            Some(8),
        );
        assert!(super::process_entry_identifies_agent(&reused));
        assert_eq!(super::cached_agent_classification(30, 8), Some(true));
        assert_eq!(super::cached_agent_classification(30, 7), None);

        let unidentified =
            test_entry_with_creation_time(31, 10, "node.exe", &["node.exe", "worker.js"], None);
        assert!(!super::process_entry_identifies_agent(&unidentified));
        assert!(
            !super::with_agent_classification_cache(|cache| cache.contains_key(&31)),
            "an instance without a creation time is never cached"
        );
        assert!(super::process_entry_identifies_agent(&test_entry(
            32,
            10,
            "claude.exe",
            &["claude.exe"]
        )));
    }

    /// A "not an agent" verdict made without the command line is only reused briefly: the next
    /// read after `AGENT_CLASSIFICATION_UNREAD_TTL` may show a runtime-launched agent. Verdicts
    /// from a readable command line stay cached.
    #[test]
    fn windows_agent_classification_retries_unreadable_command_lines() {
        let age = |pid: u32| {
            super::with_agent_classification_cache(|cache| {
                let cached = cache.get_mut(&pid).expect("verdict cached");
                cached.cached_at = cached
                    .cached_at
                    .checked_sub(super::AGENT_CLASSIFICATION_UNREAD_TTL)
                    .expect("instant before the TTL");
            });
        };

        let unreadable = test_entry_without_cmdline(40, 10, "node.exe", 9);
        assert!(!super::process_entry_identifies_agent(&unreadable));
        assert_eq!(
            super::cached_agent_classification(40, 9),
            Some(false),
            "reused within the TTL"
        );
        age(40);
        assert_eq!(
            super::cached_agent_classification(40, 9),
            None,
            "an unreadable verdict expires"
        );
        let readable = test_entry_with_creation_time(
            40,
            10,
            "node.exe",
            &[
                r"C:\Program Files\nodejs\node.exe",
                r"C:\Users\user\AppData\Roaming\npm\node_modules\@openai\codex\bin\codex.js",
            ],
            Some(9),
        );
        assert!(super::process_entry_identifies_agent(&readable));
        age(40);
        assert_eq!(super::cached_agent_classification(40, 9), Some(true));

        let worker =
            test_entry_with_creation_time(41, 10, "node.exe", &["node.exe", "worker.js"], Some(3));
        assert!(!super::process_entry_identifies_agent(&worker));
        age(41);
        assert_eq!(
            super::cached_agent_classification(41, 3),
            Some(false),
            "a verdict from a readable command line is settled"
        );
    }

    /// The periodic idle check answers "busy" from the shared snapshot without taking another,
    /// and confirms "idle" against a fresh snapshot that then replaces the shared one.
    #[test]
    fn pane_shell_idle_check_confirms_idle_with_a_fresh_shared_snapshot() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let idle = || vec![test_entry(10, 1, "zsh.exe", &["zsh.exe"])];
        let busy = || {
            vec![
                test_entry(10, 1, "zsh.exe", &["zsh.exe"]),
                test_entry(20, 10, "sleep.exe", &["sleep.exe", "30"]),
            ]
        };
        let shared = |entries: Vec<super::WindowsProcessEntry>| {
            std::sync::Mutex::new(super::ProcessSnapshotCache {
                cached: Some(super::CachedProcessSnapshot {
                    // Inside the shared TTL however slowly the test runs.
                    built_at: Instant::now() + Duration::from_secs(60),
                    snapshot: Arc::new(super::ProcessSnapshot::new(entries)),
                }),
            })
        };
        let cached_shell_is_idle = |cache: &std::sync::Mutex<super::ProcessSnapshotCache>| {
            let cache = cache.lock().expect("cache lock");
            let cached = cache.cached.as_ref().expect("snapshot cached");
            super::available_pane_shell_from_snapshot(10, &cached.snapshot).is_some()
        };

        let builds = AtomicUsize::new(0);
        let counted = |entries: fn() -> Vec<super::WindowsProcessEntry>| {
            let builds = &builds;
            move || {
                builds.fetch_add(1, Ordering::SeqCst);
                entries()
            }
        };

        let cache = shared(busy());
        assert!(!super::pane_shell_is_idle_in(10, &cache, counted(idle)));
        assert_eq!(
            builds.load(Ordering::SeqCst),
            0,
            "busy in the shared snapshot"
        );

        let cache = shared(idle());
        assert!(!super::pane_shell_is_idle_in(10, &cache, counted(busy)));
        assert_eq!(builds.load(Ordering::SeqCst), 1, "idle is confirmed live");
        assert!(
            !cached_shell_is_idle(&cache),
            "the confirming snapshot is shared"
        );

        let cache = shared(idle());
        assert!(super::pane_shell_is_idle_in(10, &cache, counted(idle)));
        assert_eq!(builds.load(Ordering::SeqCst), 2);
        assert!(cached_shell_is_idle(&cache));
    }

    /// Quiet panes recheck at the first tick of each shared slot, whatever their own phase, so
    /// all of them fall inside one snapshot lifetime.
    #[test]
    fn quiet_agentless_rechecks_line_up_on_shared_slots() {
        let origin = Instant::now();
        let at = |millis| origin + Duration::from_millis(millis);
        let slot = super::QUIET_PANE_PROCESS_RECHECK.as_millis() as u64;
        let due = super::process_recheck_slot_changed;

        // Two panes whose last observations sit early and late in the same slot.
        for last in [at(100), at(slot - 100)] {
            assert!(!due(origin, last, at(slot - 1)));
            assert!(due(origin, last, at(slot)));
            assert!(due(origin, last, at(slot + 499)));
        }
        // Once observed in the new slot, the next recheck waits for the following one.
        assert!(!due(origin, at(slot + 200), at(2 * slot - 1)));
        assert!(due(origin, at(slot + 200), at(2 * slot)));
        // An observation time before the origin counts as slot zero.
        assert!(!due(at(500), origin, at(600)));
    }

    /// Detection scale profile: `cargo test --release --locked --bin herdr
    /// detection_scale_profile -- --ignored --nocapture --test-threads=1`. Spawns `count` idle
    /// pane-like trees (a `cmd.exe` shell with a long-running child) and times one detection
    /// round, every pane observing its foreground group, with the shared snapshot expired
    /// between rounds as it is between 500 ms ticks.
    #[test]
    #[ignore = "profiling; spawns real process trees"]
    fn detection_scale_profile_windows_foreground_observation() {
        for count in [1_usize, 15, 30] {
            let mut trees = Vec::new();
            for _ in 0..count {
                let mut command = Command::new("cmd.exe");
                command
                    .args(["/d", "/c", "ping -n 120 127.0.0.1 >nul"])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                super::configure_status_command(&mut command);
                let child = command.spawn().expect("spawn pane-like process tree");
                let guard = super::StatusCommandGuard::from_std_child(&child)
                    .expect("kill-on-close job for the tree");
                trees.push((child.id(), guard, child));
            }
            thread::sleep(Duration::from_millis(1500));
            let mut samples = Vec::new();
            for round in 0..12 {
                thread::sleep(Duration::from_millis(600));
                let started = Instant::now();
                for (pid, _, _) in &trees {
                    std::hint::black_box(super::foreground_process_group_id(*pid));
                }
                if round >= 2 {
                    samples.push(started.elapsed().as_micros());
                }
            }
            samples.sort_unstable();
            eprintln!(
                "detection_scale panes={count} round_median_us={} round_max_us={}",
                samples[samples.len() / 2],
                samples[samples.len() - 1]
            );
        }
    }

    #[test]
    #[ignore = "isolated 1/15/118-shell process-inspection profile"]
    fn windows_process_inspection_profile() {
        struct Shell {
            child: Box<dyn portable_pty::Child + Send + Sync>,
            pty: Option<portable_pty::PtyPair>,
            reader: Option<thread::JoinHandle<()>>,
        }
        impl Drop for Shell {
            fn drop(&mut self) {
                let _ = self.child.kill();
                let _ = self.child.wait();
                self.pty.take();
                if let Some(reader) = self.reader.take() {
                    let _ = reader.join();
                }
            }
        }

        fn cpu_time() -> Duration {
            let mut creation = super::FILETIME::default();
            let mut exit = super::FILETIME::default();
            let mut kernel = super::FILETIME::default();
            let mut user = super::FILETIME::default();
            assert_ne!(
                unsafe {
                    super::GetProcessTimes(
                        super::GetCurrentProcess(),
                        &mut creation,
                        &mut exit,
                        &mut kernel,
                        &mut user,
                    )
                },
                0
            );
            let ticks = |time: super::FILETIME| {
                (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime)
            };
            Duration::from_nanos((ticks(kernel) + ticks(user)) * 100)
        }

        let shell =
            std::path::PathBuf::from(std::env::var_os("SystemRoot").expect("Windows SystemRoot"))
                .join("System32")
                .join("cmd.exe");
        for panes in [1, 15, 118] {
            let mut shells = Vec::new();
            for _ in 0..panes {
                let pty = portable_pty::native_pty_system()
                    .openpty(portable_pty::PtySize {
                        rows: 24,
                        cols: 80,
                        pixel_width: 0,
                        pixel_height: 0,
                    })
                    .expect("open isolated fixed-geometry PTY");
                let mut command = portable_pty::CommandBuilder::new(&shell);
                command.args(["/D", "/Q", "/K"]);
                let child = pty
                    .slave
                    .spawn_command(command)
                    .expect("spawn isolated idle shell");
                let mut reader = pty
                    .master
                    .try_clone_reader()
                    .expect("clone profile PTY reader");
                shells.push(Shell {
                    child,
                    pty: Some(pty),
                    reader: Some(thread::spawn(move || {
                        let _ = std::io::copy(&mut reader, &mut std::io::sink());
                    })),
                });
            }
            thread::sleep(Duration::from_millis(300));
            let snapshot = super::ProcessSnapshot::new(super::snapshot_processes());
            for shell in &shells {
                assert!(
                    super::descendant_entries(shell.child.process_id().unwrap(), &snapshot)
                        .is_empty()
                );
            }
            for sample in 0..3 {
                super::FOREGROUND_PROCESS_SNAPSHOT_CACHE
                    .lock()
                    .unwrap()
                    .cached = None;
                super::FOREGROUND_SELECTION_CACHE
                    .lock()
                    .unwrap()
                    .entries
                    .clear();
                super::PROCESS_INSPECTION_COUNTS
                    .with_borrow_mut(|counts| *counts = super::ProcessInspectionCounts::default());
                let started = Instant::now();
                let cpu_started = cpu_time();
                let mut inspection_time = Duration::ZERO;
                for poll in 0..20 {
                    let next_poll = started + Duration::from_millis(poll * 500);
                    thread::sleep(next_poll.saturating_duration_since(Instant::now()));
                    let inspecting = Instant::now();
                    for shell in &mut shells {
                        assert!(shell.child.try_wait().unwrap().is_none());
                        let pid = shell.child.process_id().unwrap();
                        let job = super::foreground_job(pid).expect("live shell job");
                        assert_eq!(job.process_group_id, pid);
                    }
                    inspection_time += inspecting.elapsed();
                }
                let cpu = cpu_time() - cpu_started;
                let counts = super::PROCESS_INSPECTION_COUNTS.with_borrow(|counts| *counts);
                println!(
                    "panes={panes} sample={sample} polls=20 snapshots={} opens={} command_reads={} inspection_ms={:.3} cpu_ms={:.3} elapsed_ms={:.3} creation_queries={} parent_queries={} image_queries={}",
                    counts.snapshots,
                    counts.opens,
                    counts.command_reads,
                    inspection_time.as_secs_f64() * 1000.0,
                    cpu.as_secs_f64() * 1000.0,
                    started.elapsed().as_secs_f64() * 1000.0,
                    counts.creation_queries,
                    counts.parent_queries,
                    counts.image_queries,
                );
            }
        }
    }

    #[test]
    fn windows_foreground_process_snapshot_is_shared_within_ttl() {
        let mut cache = super::ProcessSnapshotCache { cached: None };
        let mut builds = 0;
        let mut first_build_completed_at = None;

        let first = cache.snapshot(Duration::from_secs(60), || {
            builds += 1;
            let entries = vec![test_entry(10, 1, "powershell.exe", &["powershell.exe"])];
            first_build_completed_at = Some(Instant::now());
            entries
        });
        assert!(cache.cached.as_ref().unwrap().built_at >= first_build_completed_at.unwrap());
        let second = cache.snapshot(Duration::from_secs(60), || {
            builds += 1;
            Vec::new()
        });
        let refreshed = cache.snapshot(Duration::ZERO, || {
            builds += 1;
            vec![test_entry(20, 1, "pwsh.exe", &["pwsh.exe"])]
        });

        assert!(Arc::ptr_eq(&first, &second));
        assert!(!Arc::ptr_eq(&second, &refreshed));
        assert_eq!(builds, 2);
        assert_eq!(refreshed.entries[0].pid, 20);
    }

    #[test]
    fn windows_foreground_selection_cache_reuses_live_agent_and_invalidates_changes() {
        let snapshot = super::ProcessSnapshot::new(vec![
            test_entry(10, 1, "powershell.exe", &["powershell.exe"]),
            test_entry(20, 10, "codex.exe", &["codex.exe"]),
        ]);
        let job = super::foreground_job_from_entry(snapshot.entry(20).unwrap());
        let mut cache = super::ForegroundSelectionCache::default();
        cache.remember_for_test(10, &snapshot, &job);

        assert_eq!(cache.get(10, &snapshot), Some(job.clone()));

        cache.entries.get_mut(&10).unwrap().selected_identity = super::ProcessIdentity::Stub {
            running: false,
            creation_time: None,
        };
        assert_eq!(cache.get(10, &snapshot), None);

        cache.remember_for_test(10, &snapshot, &job);
        let overlap = super::ProcessSnapshot::new(vec![
            test_entry(10, 1, "powershell.exe", &["powershell.exe"]),
            test_entry(20, 10, "codex.exe", &["codex.exe"]),
            test_entry(30, 10, "claude.exe", &["claude.exe"]),
        ]);
        assert_eq!(cache.get(10, &overlap), None);

        cache.remember_for_test(10, &snapshot, &job);
        let changed = super::ProcessSnapshot::new(vec![
            test_entry(10, 1, "powershell.exe", &["powershell.exe"]),
            test_entry(20, 10, "git.exe", &["git.exe"]),
        ]);
        assert_eq!(cache.get(10, &changed), None);

        cache.remember_for_test(10, &snapshot, &job);
        cache.entries.get_mut(&10).unwrap().verified_at = Instant::now()
            .checked_sub(super::FOREGROUND_SELECTION_RECHECK + Duration::from_secs(1))
            .unwrap();
        assert_eq!(cache.get(10, &snapshot), None);
    }

    #[test]
    fn windows_foreground_selection_cache_rejects_initialized_unknown_parent_same_size_topology() {
        let original = super::ProcessSnapshot::new(vec![
            test_entry(10, 0, "pwsh.exe", &["pwsh.exe"]),
            test_entry(20, 10, "codex.exe", &["codex.exe"]),
        ]);
        let job = super::foreground_job_from_entry(original.entry(20).unwrap());
        let mut cache = super::ForegroundSelectionCache::default();
        cache.remember_for_test(10, &original, &job);
        assert_eq!(cache.entries.get(&10).unwrap().descendants.len(), 1);
        let mut child = observation_entry(
            (20, 10, "codex.exe"),
            (10, Some(20), "codex.exe"),
            Some(20),
            Some("codex.exe"),
        );
        Arc::get_mut(child.reader.as_mut().unwrap())
            .unwrap()
            .parent_pid = None;
        let fresh = super::ProcessSnapshot::new(vec![
            test_entry(10, 0, "pwsh.exe", &["pwsh.exe"]),
            child,
            test_entry(30, 10, "claude.exe", &["claude.exe"]),
        ]);
        assert_eq!(
            fresh.entry(20).unwrap().observation().unwrap().parent_pid,
            None
        );
        assert_eq!(cache.get(10, &fresh), None);
    }

    #[test]
    fn windows_foreground_selection_cache_rejects_initialized_image_conflict_before_sharing() {
        let original = super::ProcessSnapshot::new(vec![
            test_entry(10, 0, "pwsh.exe", &["pwsh.exe"]),
            test_entry(20, 10, "codex.exe", &["codex.exe"]),
        ]);
        let job = super::foreground_job_from_entry(original.entry(20).unwrap());
        let mut cache = super::ForegroundSelectionCache::default();
        cache.remember_for_test(10, &original, &job);
        let fresh = super::ProcessSnapshot::new(vec![
            test_entry(10, 0, "pwsh.exe", &["pwsh.exe"]),
            observation_entry(
                (20, 10, "codex.exe"),
                (10, Some(20), "worker.exe"),
                Some(20),
                None,
            ),
        ]);
        assert_eq!(
            fresh.entry(20).unwrap().observation().unwrap().name(),
            "worker.exe"
        );
        assert!(fresh.entry(10).unwrap().observation.get().is_none());
        assert_eq!(cache.get(10, &fresh), None);
        assert!(
            fresh.entry(10).unwrap().observation.get().is_none(),
            "existing metadata conflicts are checked before any pin is shared"
        );
    }

    #[test]
    fn windows_foreground_selection_cache_rejects_added_or_replaced_descendants() {
        let original = super::ProcessSnapshot::new(vec![
            test_entry(10, 0, "pwsh.exe", &["pwsh.exe"]),
            test_entry(20, 10, "codex.exe", &["codex.exe"]),
        ]);
        let job = super::foreground_job_from_entry(original.entry(20).unwrap());
        for retains_previous in [true, false] {
            let mut entries = vec![
                test_entry(10, 0, "pwsh.exe", &["pwsh.exe"]),
                test_entry(30, 10, "claude.exe", &["claude.exe"]),
            ];
            if retains_previous {
                entries.push(test_entry(20, 10, "codex.exe", &["codex.exe"]));
            }
            let fresh = super::ProcessSnapshot::new(entries);
            let mut cache = super::ForegroundSelectionCache::default();
            cache.remember_for_test(10, &original, &job);
            assert_eq!(
                cache.get(10, &fresh),
                None,
                "retains_previous={retains_previous}"
            );
        }
    }

    #[test]
    fn windows_foreground_selection_cache_accepts_initialized_matching_metadata_and_reordered_tree()
    {
        let entries = vec![
            test_entry(10, 0, "pwsh.exe", &["pwsh.exe"]),
            test_entry(20, 10, "codex.exe", &["codex.exe"]),
            test_entry(30, 10, "worker.exe", &["worker.exe"]),
        ];
        let original = super::ProcessSnapshot::new(entries.clone());
        let job = super::foreground_job_from_entry(original.entry(20).unwrap());
        let mut cache = super::ForegroundSelectionCache::default();
        cache.remember_for_test(10, &original, &job);
        let fresh = super::ProcessSnapshot::new(entries.into_iter().rev().collect());
        for entry in &fresh.entries {
            assert!(entry.observation().is_some());
        }
        let reads = fresh
            .entries
            .iter()
            .map(|entry| {
                entry
                    .reader
                    .as_ref()
                    .unwrap()
                    .observations
                    .load(super::AtomicOrdering::Relaxed)
            })
            .collect::<Vec<_>>();
        assert_eq!(cache.get(10, &fresh), Some(job));
        for (entry, reads) in fresh.entries.iter().zip(reads) {
            let reader = entry.reader.as_ref().unwrap();
            assert_eq!(
                reader.observations.load(super::AtomicOrdering::Relaxed),
                reads
            );
            assert_eq!(
                reader.commands.load(super::AtomicOrdering::Relaxed),
                u32::from(entry.pid == 20)
            );
        }
    }

    #[test]
    fn windows_foreground_selection_cache_rejects_reused_descendant_pid() {
        let original = super::ProcessSnapshot::new(vec![
            test_entry(10, 1, "powershell.exe", &["powershell.exe"]),
            test_entry(20, 10, "codex.exe", &["codex.exe"]),
            test_entry(30, 10, "node.exe", &["node.exe", "worker.js"]),
        ]);
        let job = super::foreground_job_from_entry(original.entry(20).unwrap());
        let mut cache = super::ForegroundSelectionCache::default();
        cache.remember_for_test(10, &original, &job);

        let cached = cache.entries.get_mut(&10).unwrap();
        let reused_index = cached
            .descendants
            .iter()
            .position(|entry| entry.pid == 30)
            .unwrap();
        cached.descendant_identities[reused_index] = super::ProcessIdentity::Stub {
            running: false,
            creation_time: None,
        };
        let replacement = super::ProcessSnapshot::new(vec![
            test_entry(10, 1, "powershell.exe", &["powershell.exe"]),
            test_entry(20, 10, "codex.exe", &["codex.exe"]),
            test_entry(30, 10, "node.exe", &["node.exe", "codex.js"]),
        ]);

        assert_eq!(cache.get(10, &replacement), None);
    }

    #[test]
    fn windows_foreground_selection_cache_rejects_identity_change_before_insertion() {
        let snapshot = super::ProcessSnapshot::new(vec![
            test_entry_with_creation_time(10, 1, "powershell.exe", &["powershell.exe"], Some(1)),
            test_entry_with_creation_time(20, 10, "codex.exe", &["codex.exe"], Some(2)),
            test_entry_with_creation_time(30, 10, "node.exe", &["node.exe", "worker.js"], Some(3)),
        ]);
        let job = super::foreground_job_from_entry(snapshot.entry(20).unwrap());
        let descendants = snapshot.descendant_signatures(10);
        let descendant_identities = vec![
            super::ProcessIdentity::Stub {
                running: true,
                creation_time: Some(2),
            },
            super::ProcessIdentity::Stub {
                running: true,
                creation_time: Some(4),
            },
        ];
        let mut cache = super::ForegroundSelectionCache::default();

        let cached = super::CachedForegroundSelection::from_snapshot_with_identities(
            10,
            &snapshot,
            &job,
            descendants,
            descendant_identities,
            super::ProcessIdentity::Stub {
                running: true,
                creation_time: Some(1),
            },
            super::ProcessIdentity::Stub {
                running: true,
                creation_time: Some(2),
            },
        );
        cache.remember(10, cached);

        assert!(cache.entries.is_empty());
    }

    #[test]
    fn windows_foreground_selection_cache_accepts_limited_information_identity() {
        let snapshot = super::ProcessSnapshot::new(vec![
            test_entry_without_cmdline(10, 1, "powershell.exe", 1),
            test_entry_without_cmdline(20, 10, "codex.exe", 2),
        ]);
        let job = super::foreground_job_from_entry(snapshot.entry(20).unwrap());
        let cached = super::CachedForegroundSelection::from_snapshot_with_identities(
            10,
            &snapshot,
            &job,
            snapshot.descendant_signatures(10),
            vec![super::ProcessIdentity::Stub {
                running: true,
                creation_time: Some(2),
            }],
            super::ProcessIdentity::Stub {
                running: true,
                creation_time: Some(1),
            },
            super::ProcessIdentity::Stub {
                running: true,
                creation_time: Some(2),
            },
        );

        assert!(cached.is_some());
        assert_eq!(job.processes[0].argv0.as_deref(), Some("codex.exe"));
        assert!(job.processes[0].cmdline.is_none());
    }

    #[test]
    fn windows_foreground_selection_cache_prunes_unused_entries_on_insertion() {
        let first_snapshot = super::ProcessSnapshot::new(vec![
            test_entry(10, 1, "powershell.exe", &["powershell.exe"]),
            test_entry(20, 10, "codex.exe", &["codex.exe"]),
        ]);
        let first_job = super::foreground_job_from_entry(first_snapshot.entry(20).unwrap());
        let mut cache = super::ForegroundSelectionCache::default();
        cache.remember_for_test(10, &first_snapshot, &first_job);
        cache.entries.get_mut(&10).unwrap().last_used = Instant::now()
            .checked_sub(super::FOREGROUND_SELECTION_CACHE_RETENTION + Duration::from_secs(1))
            .unwrap();

        let second_snapshot = super::ProcessSnapshot::new(vec![
            test_entry(11, 1, "powershell.exe", &["powershell.exe"]),
            test_entry(21, 11, "claude.exe", &["claude.exe"]),
        ]);
        let second_job = super::foreground_job_from_entry(second_snapshot.entry(21).unwrap());
        cache.remember_for_test(11, &second_snapshot, &second_job);

        assert!(!cache.entries.contains_key(&10));
        assert!(cache.entries.contains_key(&11));
    }

    #[test]
    fn windows_foreground_selection_cache_retains_idle_native_shell_until_launch_or_exit() {
        for name in ["cmd.exe", "powershell.exe", "PWSH.EXE"] {
            let snapshot = super::ProcessSnapshot::new(vec![test_entry(10, 1, name, &[name])]);
            let shell = super::foreground_job_from_entry(snapshot.entry(10).unwrap());
            let mut cache = super::ForegroundSelectionCache::default();
            cache.remember_for_test(10, &snapshot, &shell);
            assert_eq!(cache.get(10, &snapshot), Some(shell.clone()));

            let launched = super::ProcessSnapshot::new(vec![
                test_entry(10, 1, name, &[name]),
                test_entry(20, 10, "codex.exe", &["codex.exe"]),
            ]);
            assert_eq!(cache.get(10, &launched), None);
            let agent = super::select_pane_foreground_job_from_snapshot_with_runtime_inspection(
                10,
                &launched,
                |_| panic!("direct child must bypass escaped-agent inspection"),
                |_| panic!("direct child must bypass runtime markers"),
            )
            .unwrap();
            assert_eq!(agent.process_group_id, 20);
            cache.remember_for_test(10, &launched, &agent);
            assert_eq!(cache.get(10, &snapshot), None);

            cache.remember_for_test(10, &snapshot, &shell);
            assert_eq!(cache.get(10, &snapshot), Some(shell.clone()));
            let short_lived_child = super::ProcessSnapshot::new(vec![
                test_entry(10, 1, name, &[name]),
                test_entry(21, 10, "git.exe", &["git.exe"]),
            ]);
            assert_eq!(cache.get(10, &short_lived_child), None);
            cache.remember_for_test(10, &snapshot, &shell);
            cache.entries.get_mut(&10).unwrap().shell_identity = super::ProcessIdentity::Stub {
                running: false,
                creation_time: None,
            };
            // An identical fresh signature must not hide shell exit/PID reuse.
            assert_eq!(cache.get(10, &snapshot), None);

            cache.remember_for_test(10, &snapshot, &shell);
            cache.entries.get_mut(&10).unwrap().verified_at = Instant::now()
                .checked_sub(super::FOREGROUND_SELECTION_RECHECK + Duration::from_secs(1))
                .unwrap();
            assert_eq!(cache.get(10, &snapshot), None);
        }
    }

    #[test]
    fn windows_foreground_selection_cache_keeps_escaped_and_unknown_children_fresh() {
        for entries in [
            vec![test_entry(10, 1, "bash.exe", &["bash.exe"])],
            vec![test_entry(10, 1, "launcher.exe", &["launcher.exe"])],
            vec![
                test_entry(10, 1, "pwsh.exe", &["pwsh.exe"]),
                test_entry(20, 10, "node.exe", &["node.exe", "worker.js"]),
            ],
        ] {
            let snapshot = super::ProcessSnapshot::new(entries);
            let shell = super::foreground_job_from_entry(snapshot.entry(10).unwrap());
            let mut cache = super::ForegroundSelectionCache::default();
            cache.remember_for_test(10, &snapshot, &shell);
            assert_eq!(cache.get(10, &snapshot), None);
        }
    }

    #[test]
    fn windows_foreground_selection_cache_retains_live_escaped_agent() {
        let snapshot = super::ProcessSnapshot::new(vec![
            test_entry(10, 1, "bash.exe", &["bash.exe"]),
            test_entry(20, 99, "launcher.exe", &["codex.exe"]),
        ]);
        let escaped = super::foreground_job_from_entry(snapshot.entry(20).unwrap());
        let mut cache = super::ForegroundSelectionCache::default();

        cache.remember_for_test(10, &snapshot, &escaped);

        assert_eq!(cache.get(10, &snapshot), Some(escaped.clone()));
        cache.entries.get_mut(&10).unwrap().selected_identity = super::ProcessIdentity::Stub {
            running: false,
            creation_time: None,
        };
        assert_eq!(cache.get(10, &snapshot), None);
    }

    #[test]
    fn windows_process_tree_selects_wrapped_agent_descendant() {
        let entries = vec![
            test_entry(10, 1, "cmd.exe", &["cmd.exe"]),
            test_entry(
                20,
                10,
                "node.exe",
                &[
                    "node.exe",
                    "C:\\Users\\herdr\\AppData\\Roaming\\npm\\node_modules\\codex\\bin\\codex.js",
                ],
            ),
        ];

        let job = super::select_pane_foreground_job(10, &entries).unwrap();

        assert_eq!(job.process_group_id, 20);
        assert_eq!(job.processes[0].name, "node.exe");
    }

    #[test]
    fn windows_process_tree_selects_cmd_wrapped_agent_descendant() {
        let entries = vec![
            test_entry(10, 1, "powershell.exe", &["powershell.exe"]),
            test_entry(
                20,
                10,
                "cmd.exe",
                &[
                    "cmd.exe",
                    "/D",
                    "/S",
                    "/C",
                    "C:\\Users\\herdr\\AppData\\Roaming\\npm\\codex.cmd --model gpt-5",
                ],
            ),
        ];

        let job = super::select_pane_foreground_job(10, &entries).unwrap();

        assert_eq!(job.process_group_id, 20);
        assert_eq!(job.processes[0].name, "cmd.exe");
    }

    #[test]
    fn windows_process_tree_selects_topmost_codex_process_in_single_agent_chain() {
        let entries = vec![
            test_entry(10, 1, "powershell.exe", &["powershell.exe"]),
            test_entry(
                20,
                10,
                "node.exe",
                &[
                    "node.exe",
                    "C:\\Users\\herdr\\AppData\\Roaming\\npm\\node_modules\\@openai\\codex\\bin\\codex.js",
                ],
            ),
            test_entry(
                30,
                20,
                "codex.exe",
                &["C:\\Users\\herdr\\AppData\\Roaming\\npm\\node_modules\\@openai\\codex\\node_modules\\@openai\\codex-win32-x64\\vendor\\x86_64-pc-windows-msvc\\bin\\codex.exe"],
            ),
            test_entry(40, 30, "node_repl.exe", &["node_repl.exe"]),
            test_entry(
                50,
                40,
                "codex.exe",
                &["codex.exe", "app-server", "--listen", "stdio://"],
            ),
        ];

        let job = super::select_pane_foreground_job(10, &entries).unwrap();

        assert_eq!(job.process_group_id, 20);
        assert_eq!(job.processes[0].name, "node.exe");
    }

    #[test]
    fn windows_process_tree_keeps_topmost_agent_over_different_agent_descendant() {
        let entries = vec![
            test_entry(10, 1, "powershell.exe", &["powershell.exe"]),
            test_entry(20, 10, "claude.exe", &["claude.exe"]),
            test_entry(
                30,
                20,
                "cmd.exe",
                &["cmd.exe", "/D", "/S", "/C", "codex mcp-server"],
            ),
        ];

        let job = super::select_pane_foreground_job(10, &entries).unwrap();

        assert_eq!(job.process_group_id, 20);
        assert_eq!(job.processes[0].name, "claude.exe");
    }

    #[test]
    fn windows_process_tree_keeps_root_agent_over_agent_descendant() {
        let entries = vec![
            test_entry(10, 1, "claude.exe", &["claude.exe"]),
            test_entry(
                20,
                10,
                "cmd.exe",
                &["cmd.exe", "/D", "/S", "/C", "codex mcp-server"],
            ),
        ];

        let job = super::select_pane_foreground_job(10, &entries).unwrap();

        assert_eq!(job.process_group_id, 10);
        assert_eq!(job.processes[0].name, "claude.exe");
    }

    #[test]
    fn windows_process_tree_returns_shell_for_same_agent_siblings() {
        let entries = vec![
            test_entry(10, 1, "powershell.exe", &["powershell.exe"]),
            test_entry(20, 10, "codex.exe", &["codex.exe"]),
            test_entry(30, 10, "codex.exe", &["codex.exe"]),
        ];

        let job = super::select_pane_foreground_job(10, &entries).unwrap();

        assert_eq!(job.process_group_id, 10);
        assert_eq!(job.processes[0].name, "powershell.exe");
    }

    #[test]
    fn windows_process_tree_returns_shell_for_plain_descendant() {
        let entries = vec![
            test_entry(10, 1, "powershell.exe", &["powershell.exe"]),
            test_entry(20, 10, "git.exe", &["git.exe", "status"]),
        ];

        let job = super::select_pane_foreground_job(10, &entries).unwrap();

        assert_eq!(job.process_group_id, 10);
        assert_eq!(job.processes[0].name, "powershell.exe");
    }

    #[test]
    fn windows_shell_is_available_only_without_descendants() {
        let shell_only = super::ProcessSnapshot::new(vec![test_entry(
            10,
            1,
            "powershell.exe",
            &["powershell.exe"],
        )]);
        assert_eq!(
            super::available_pane_shell_from_snapshot(10, &shell_only).as_deref(),
            Some("powershell.exe")
        );

        let busy = super::ProcessSnapshot::new(vec![
            test_entry(10, 1, "powershell.exe", &["powershell.exe"]),
            test_entry(20, 10, "git.exe", &["git.exe", "status"]),
        ]);
        assert_eq!(super::available_pane_shell_from_snapshot(10, &busy), None);

        let replaced =
            super::ProcessSnapshot::new(vec![test_entry(10, 1, "vim.exe", &["vim.exe"])]);
        assert_eq!(
            super::available_pane_shell_from_snapshot(10, &replaced),
            None
        );
    }

    #[test]
    fn windows_process_tree_returns_shell_for_multiple_agent_descendants() {
        let entries = vec![
            test_entry(10, 1, "powershell.exe", &["powershell.exe"]),
            test_entry(20, 10, "codex.exe", &["codex.exe"]),
            test_entry(30, 10, "claude.exe", &["claude.exe"]),
        ];

        let job = super::select_pane_foreground_job(10, &entries).unwrap();

        assert_eq!(job.process_group_id, 10);
        assert_eq!(job.processes[0].name, "powershell.exe");
    }

    /// 纯逻辑地核对一棵 pane 进程树。`snapshot` 是 Toolhelp 快照里的 (pid, 父 pid)；`handles`
    /// 是经进程句柄读到的 (pid, 父 pid, 创建时间)，不在表里表示打不开或读不出创建时间。
    fn verified_members(
        session: super::ProcessSessionId,
        snapshot: &[(u32, u32)],
        handles: &[(u32, u32, u64)],
    ) -> Vec<(u32, u64)> {
        verified_members_with_daemons(session, snapshot, handles, &[], &[])
    }

    /// 同上，另给出 daemon 线索：`markers` 里的 (pid, 创建时间) 挂着 server daemon 标记；
    /// `commands` 是经句柄读到的 (pid, 映像路径, 命令行)，没列出的进程两者都为空。认法与生产
    /// 相同：标记，或旧版 daemon 的映像与命令行。
    fn verified_members_with_daemons(
        session: super::ProcessSessionId,
        snapshot: &[(u32, u32)],
        handles: &[(u32, u32, u64)],
        markers: &[(u32, u64)],
        commands: &[(u32, &str, &str)],
    ) -> Vec<(u32, u64)> {
        let snapshot = super::ProcessSnapshot::new(
            snapshot
                .iter()
                .map(|&(pid, parent)| test_entry(pid, parent, "process.exe", &["process.exe"]))
                .collect(),
        );
        let handles: std::collections::HashMap<u32, (u32, u64)> = handles
            .iter()
            .map(|&(pid, parent, created)| (pid, (parent, created)))
            .collect();
        let commands: std::collections::HashMap<u32, (&str, &str)> = commands
            .iter()
            .map(|&(pid, image, command_line)| (pid, (image, command_line)))
            .collect();
        let mut members: Vec<(u32, u64)> = super::verified_session_members(
            session,
            &snapshot,
            |pid| {
                handles
                    .get(&pid)
                    .map(|&(parent_pid, created)| super::InspectedProcess {
                        parent_pid,
                        created,
                        pin: commands.get(&pid).copied().unwrap_or(("", "")),
                    })
            },
            |member, &(image, command_line): &(&str, &str)| {
                markers.contains(&(member.pid, member.instance))
                    || super::is_legacy_server_daemon_command(image, command_line)
            },
        )
        .into_iter()
        .map(|member| (member.pid, member.instance))
        .collect();
        members.sort_unstable();
        members
    }

    #[test]
    fn pane_tree_members_leave_herdr_server_daemons_and_their_sessions_running() {
        // 20 是 pane 里的 herdr 客户端，30 是它拉起的 server daemon，35 是 daemon 自己的 pane，
        // 37 是那个 pane 里的程序。daemon 连同整个嵌套会话都留着，客户端与 40 照常终止。
        let members = verified_members_with_daemons(
            ROOT_ANCHOR,
            &[(10, 1), (20, 10), (30, 20), (35, 30), (37, 35), (40, 10)],
            &[
                (10, 1, 100),
                (20, 10, 110),
                (30, 20, 120),
                (35, 30, 130),
                (37, 35, 140),
                (40, 10, 150),
            ],
            &[(30, 120)],
            &[],
        );
        assert_eq!(members, vec![(10, 100), (20, 110), (40, 150)]);
    }

    #[test]
    fn pane_tree_members_leave_orphaned_herdr_server_daemons_running() {
        // 根进程已退出：孤儿里的 daemon（20）及其子进程同样留着，别的孤儿照常算成员。
        let members = verified_members_with_daemons(
            ROOT_ANCHOR,
            &[(20, 10), (25, 20), (30, 10)],
            &[(20, 10, 150), (25, 20, 160), (30, 10, 170)],
            &[(20, 150)],
            &[],
        );
        assert_eq!(members, vec![(30, 170)]);
    }

    #[test]
    fn pane_tree_members_ignore_a_daemon_marker_of_another_process_instance() {
        // 标记按核对过的 pid 与创建时间认：pid 30 当前的主人创建于 120，创建于 90 的旧 daemon
        // 的标记与它无关。
        let members = verified_members_with_daemons(
            ROOT_ANCHOR,
            &[(10, 1), (30, 10)],
            &[(10, 1, 100), (30, 10, 120)],
            &[(30, 90)],
            &[],
        );
        assert_eq!(members, vec![(10, 100), (30, 120)]);
    }

    #[test]
    fn pane_tree_members_leave_legacy_herdr_server_daemons_running() {
        // 30 是旧版 herdr 拉起的 server daemon：不挂标记，映像与命令行就是 `<exe> server`。它连同
        // 它的 pane（35）都留着；同一个映像的客户端（20）与带多余参数的 40 照常终止。
        let image = r"C:\Users\me\.herdr\packages\standalone\releases\0.8.0\herdr.exe";
        let client = format!(r#""{image}""#);
        let daemon = format!(r#""{image}" server"#);
        let extra = format!(r#""{image}" server --verbose"#);
        let members = verified_members_with_daemons(
            ROOT_ANCHOR,
            &[(10, 1), (20, 10), (30, 20), (35, 30), (40, 10)],
            &[
                (10, 1, 100),
                (20, 10, 110),
                (30, 20, 120),
                (35, 30, 130),
                (40, 10, 140),
            ],
            &[],
            &[
                (20, image, &client),
                (30, image, &daemon),
                (40, image, &extra),
            ],
        );
        assert_eq!(members, vec![(10, 100), (20, 110), (40, 140)]);
    }

    #[test]
    fn legacy_server_daemons_need_a_herdr_image_and_exactly_the_server_subcommand() {
        let image = r"C:\Users\me\AppData\Local\Programs\Herdr\bin\herdr.exe";
        let quoted = r#""C:\Users\me\AppData\Local\Programs\Herdr\bin\herdr.exe""#;
        for (image, command_line, expected) in [
            (image, format!("{quoted} server"), true),
            // 引号、多余空白与路径写法（按文件名、不分大小写比）的差异都认。
            (
                image,
                r"C:\Users\me\AppData\Local\Programs\Herdr\bin\herdr.exe   server".to_owned(),
                true,
            ),
            (image, r#""D:\copy\HERDR.EXE" server"#.to_owned(), true),
            (
                r"D:\src\herdr\target\debug\Herdr-Dev.EXE",
                r#""D:\src\herdr\target\debug\herdr-dev.exe" server"#.to_owned(),
                true,
            ),
            // 别的子命令、多余或缺少的参数都不算。
            (image, quoted.to_owned(), false),
            (image, format!("{quoted} Server"), false),
            (
                image,
                format!("{quoted} server --handoff-import pipe token"),
                false,
            ),
            (
                image,
                format!("{quoted} api usage-report --agent claude"),
                false,
            ),
            (image, String::new(), false),
            // argv[0] 指向别的程序，或映像名不以 herdr 开头。
            (image, r#""C:\tools\node.exe" server"#.to_owned(), false),
            (
                r"C:\tools\node.exe",
                r#""C:\tools\node.exe" server"#.to_owned(),
                false,
            ),
            (
                r"C:\tools\notherdr.exe",
                r#""C:\tools\notherdr.exe" server"#.to_owned(),
                false,
            ),
        ] {
            assert_eq!(
                super::is_legacy_server_daemon_command(image, &command_line),
                expected,
                "{image} | {command_line}"
            );
        }
    }

    /// pid 10 的根进程创建于 100，锚点快照于 500。
    const ROOT_ANCHOR: super::ProcessSessionId = super::ProcessSessionId {
        id: 10,
        instance: 100,
        captured: 500,
    };

    #[test]
    fn pane_tree_members_follow_parents_created_no_later_than_their_children() {
        let members = verified_members(
            ROOT_ANCHOR,
            &[(10, 1), (20, 10), (30, 20), (40, 10), (50, 1)],
            &[
                (10, 1, 100),
                (20, 10, 150),
                (30, 20, 200),
                // 与根进程同一时刻创建（时钟精度内）仍算它的子进程。
                (40, 10, 100),
                (50, 1, 120),
            ],
        );
        assert_eq!(members, vec![(10, 100), (20, 150), (30, 200), (40, 100)]);
    }

    #[test]
    fn pane_tree_members_skip_children_older_than_their_parent() {
        // 20 与 35 早于父进程创建：它们的父 pid 属于上一任主人，不是这棵树的成员，也不从
        // 它们往下找。
        let members = verified_members(
            ROOT_ANCHOR,
            &[(10, 1), (20, 10), (25, 20), (30, 10), (35, 30)],
            &[
                (10, 1, 100),
                (20, 10, 50),
                (25, 20, 60),
                (30, 10, 120),
                (35, 30, 110),
            ],
        );
        assert_eq!(members, vec![(10, 100), (30, 120)]);
    }

    #[test]
    fn pane_tree_members_skip_processes_whose_identity_cannot_be_read() {
        // 20 打不开或读不出创建时间：不算成员（永远不会收到信号），它下面的 30 也接不上。
        let members = verified_members(
            ROOT_ANCHOR,
            &[(10, 1), (20, 10), (30, 20), (40, 10)],
            &[(10, 1, 100), (30, 20, 200), (40, 10, 130)],
        );
        assert_eq!(members, vec![(10, 100), (40, 130)]);
    }

    #[test]
    fn pane_tree_members_reject_a_candidate_whose_pid_changed_owner_after_the_snapshot() {
        // 快照说 20 是根进程的子进程，但读句柄时 pid 20 已属于别的父进程创建的新进程。
        let members = verified_members(
            ROOT_ANCHOR,
            &[(10, 1), (20, 10)],
            &[(10, 1, 100), (20, 77, 300)],
        );
        assert_eq!(members, vec![(10, 100)]);
    }

    #[test]
    fn pane_tree_members_keep_orphans_of_an_exited_root() {
        // 根进程已退出，pid 10 已空出。只认锚点之前由它创建的孤儿（及孤儿的后代）；创建于
        // 锚点时刻及之后的，可能是 pid 10 的下一任主人（已退出）留下的，排除；早于根进程的
        // 是上一任主人留下的，同样排除。
        let members = verified_members(
            ROOT_ANCHOR,
            &[(20, 10), (25, 20), (30, 10), (40, 10), (60, 10)],
            &[
                (20, 10, 150),
                (25, 20, 160),
                (30, 10, 520),
                (40, 10, 500),
                (60, 10, 50),
            ],
        );
        assert_eq!(members, vec![(20, 150), (25, 160)]);
    }

    #[test]
    fn pane_tree_members_exclude_a_recycled_root_and_its_new_children() {
        // pid 10 已属于创建于 600 的新进程：它本身和它的子进程（30）都不算成员，原根进程的
        // 孤儿（20）仍然算。
        let members = verified_members(
            ROOT_ANCHOR,
            &[(10, 4), (20, 10), (30, 10), (35, 30)],
            &[(10, 4, 600), (20, 10, 150), (30, 10, 650), (35, 30, 700)],
        );
        assert_eq!(members, vec![(20, 150)]);
    }

    #[test]
    fn pane_tree_members_stop_at_parent_cycles() {
        let members = verified_members(
            ROOT_ANCHOR,
            &[(10, 30), (20, 10), (30, 20)],
            &[(10, 30, 100), (20, 10, 110), (30, 20, 120)],
        );
        assert_eq!(members, vec![(10, 100), (20, 110), (30, 120)]);
    }

    #[test]
    fn pane_tree_members_reject_anchors_without_a_usable_root_pid() {
        for id in [0, -1, i64::from(u32::MAX) + 1] {
            let session = super::ProcessSessionId { id, ..ROOT_ANCHOR };
            assert!(
                verified_members(session, &[(20, 0)], &[(0, 0, 100), (20, 0, 150)]).is_empty(),
                "{id}"
            );
        }
    }

    #[test]
    fn windows_process_tree_ignores_pid_reuse_cycles() {
        let entries = vec![
            test_entry(10, 30, "powershell.exe", &["powershell.exe"]),
            test_entry(20, 10, "codex.exe", &["codex.exe"]),
            test_entry(30, 20, "node.exe", &["node.exe"]),
        ];

        let snapshot = super::ProcessSnapshot::new(entries);
        let descendants = super::descendant_entries(10, &snapshot);

        assert_eq!(
            descendants
                .iter()
                .map(|entry| entry.pid)
                .collect::<Vec<_>>(),
            vec![20, 30]
        );
    }

    #[test]
    fn windows_process_tree_rejects_children_older_than_their_recorded_parent() {
        let entries = vec![
            test_entry_with_creation_time(10, 1, "shell.exe", &["shell.exe"], Some(100)),
            test_entry_with_creation_time(20, 10, "old.exe", &["old.exe"], Some(90)),
            test_entry_with_creation_time(21, 20, "worker.exe", &["worker.exe"], Some(130)),
            test_entry_with_creation_time(30, 10, "child.exe", &["child.exe"], Some(120)),
            test_entry_with_creation_time(40, 30, "old.exe", &["old.exe"], Some(110)),
            test_entry_with_creation_time(41, 40, "worker.exe", &["worker.exe"], Some(160)),
            test_entry_with_creation_time(50, 30, "child.exe", &["child.exe"], Some(140)),
        ];
        let snapshot = super::ProcessSnapshot::new(entries);

        assert_eq!(
            super::descendant_entries(10, &snapshot)
                .iter()
                .map(|entry| entry.pid)
                .collect::<Vec<_>>(),
            vec![30, 50],
            "reused parent PIDs must not admit older children or their subtrees"
        );
    }

    #[test]
    fn windows_process_tree_excludes_branches_with_unknown_identity() {
        for (root_created, child_created, expected) in [
            (None, Some(200), vec![]),
            (Some(100), None, vec![40]),
            (None, None, vec![]),
        ] {
            let snapshot = super::ProcessSnapshot::new(vec![
                test_entry_with_creation_time(10, 1, "shell.exe", &["shell.exe"], root_created),
                test_entry_with_creation_time(20, 10, "child.exe", &["child.exe"], child_created),
                test_entry_with_creation_time(30, 20, "worker.exe", &["worker.exe"], Some(300)),
                test_entry_with_creation_time(40, 10, "child.exe", &["child.exe"], Some(400)),
            ]);

            assert_eq!(
                super::descendant_entries(10, &snapshot)
                    .iter()
                    .map(|entry| entry.pid)
                    .collect::<Vec<_>>(),
                expected,
                "unknown creation times cannot establish ancestry: root={root_created:?}, child={child_created:?}"
            );
        }

        let snapshot = super::ProcessSnapshot::new(vec![
            test_entry_with_creation_time(20, 10, "child.exe", &["child.exe"], Some(200)),
            test_entry_with_creation_time(30, 20, "worker.exe", &["worker.exe"], Some(300)),
        ]);
        assert!(
            super::descendant_entries(10, &snapshot).is_empty(),
            "a missing root cannot establish ancestry from its PID alone"
        );
    }

    #[test]
    fn windows_multiplexer_lineages_preserve_unknown_identity_from_one_snapshot() {
        for (parent_created, client_created, descends) in [
            (Some(100), Some(200), Some(true)),
            (Some(250), Some(200), None),
            (None, Some(200), None),
            (Some(100), None, None),
        ] {
            let snapshot = Arc::new(super::ProcessSnapshot::new(vec![
                test_entry(1, 0, "init", &["init"]),
                test_entry_with_creation_time(10, 1, "pwsh.exe", &["pwsh.exe"], parent_created),
                test_entry_with_creation_time(20, 10, "tmux: client", &["tmux"], client_created),
                test_entry(30, 1, "tmux: server", &["tmux"]),
                test_entry(40, 30, "hook.exe", &["hook.exe"]),
            ]));
            let peer = super::process_lineage_from_snapshot(40, &snapshot).unwrap();
            let mut reads = 0;
            let clients = super::snapshot_multiplexer_client_lineages_with(&peer, || {
                reads += 1;
                Arc::clone(&snapshot)
            });
            assert_eq!(reads, 1);
            if client_created.is_none() {
                assert_eq!(
                    clients, None,
                    "an unreadable client is not an absent client"
                );
            } else {
                let clients = clients.unwrap();
                assert_eq!(clients.len(), 1);
                assert_eq!(clients[0].descends_from(10), descends);
            }
        }
        let ordinary = super::ProcessLineage {
            processes: vec![super::ProcessParentEntry {
                pid: 10,
                parent_pid: 0,
                name: "pwsh.exe".into(),
            }],
            complete: true,
        };
        assert_eq!(
            super::snapshot_multiplexer_client_lineages_with(&ordinary, || panic!("no scan")),
            Some(Vec::new())
        );
    }

    #[test]
    fn windows_identity_walks_share_one_snapshot_for_one_and_sixteen_panes() {
        for panes in [1, 16] {
            let mut builds = 0;
            let mut cache = super::ProcessSnapshotCache { cached: None };
            let snapshot = cache.snapshot(Duration::ZERO, || {
                builds += 1;
                (0..panes)
                    .flat_map(|index| {
                        let shell = 10 + index * 10;
                        [
                            test_entry(shell, 0, "pwsh.exe", &["pwsh.exe"]),
                            test_entry(shell + 1, shell, "codex.exe", &["codex.exe"]),
                        ]
                    })
                    .collect()
            });
            for index in 0..panes {
                let shared = cache.snapshot(Duration::MAX, || {
                    panic!("identity walks must reuse the retained snapshot")
                });
                assert!(Arc::ptr_eq(&snapshot, &shared));
                let shell = 10 + index * 10;
                assert_eq!(
                    super::descendant_entries(shell, &shared)
                        .iter()
                        .map(|entry| entry.pid)
                        .collect::<Vec<_>>(),
                    vec![shell + 1]
                );
                let lineage = super::process_lineage_from_snapshot(shell + 1, &shared).unwrap();
                assert!(lineage.complete);
                assert_eq!(lineage.descends_from(shell), Some(true));
                assert!(super::process_is_ancestor(shell, shell + 1, &shared));
                assert_eq!(
                    super::available_pane_shell_from_snapshot(shell, &shared),
                    None
                );
            }
            assert_eq!(builds, 1, "{panes} panes share one process-table build");
        }
    }

    #[test]
    fn windows_process_lineage_stops_at_reused_or_unknown_parent() {
        for (parent_created, complete) in [
            (Some(100), true),
            (Some(200), true),
            (Some(250), false),
            (None, false),
        ] {
            let snapshot = super::ProcessSnapshot::new(vec![
                test_entry_with_creation_time(1, 0, "init.exe", &["init.exe"], Some(1)),
                test_entry_with_creation_time(10, 1, "shell.exe", &["shell.exe"], parent_created),
                test_entry_with_creation_time(20, 10, "child.exe", &["child.exe"], Some(200)),
                test_entry_with_creation_time(30, 20, "hook.exe", &["hook.exe"], Some(300)),
            ]);
            let lineage = super::process_lineage_from_snapshot(30, &snapshot).unwrap();
            assert_eq!(lineage.complete, complete, "{parent_created:?}");
            assert_eq!(
                lineage
                    .processes
                    .iter()
                    .map(|entry| entry.pid)
                    .collect::<Vec<_>>(),
                if complete {
                    vec![30, 20, 10, 1]
                } else {
                    vec![30, 20]
                }
            );
            assert_eq!(lineage.descends_from(20), Some(true));
            assert_eq!(lineage.descends_from(10), complete.then_some(true));
            assert_eq!(lineage.descends_from(99), complete.then_some(false));
            assert_eq!(super::process_is_ancestor(10, 30, &snapshot), complete);
        }
        let snapshot = super::ProcessSnapshot::new(vec![test_entry_with_creation_time(
            10,
            0,
            "shell.exe",
            &["shell.exe"],
            None,
        )]);
        assert_eq!(super::process_lineage_from_snapshot(10, &snapshot), None);
        assert_eq!(super::process_lineage_from_snapshot(99, &snapshot), None);
        assert_eq!(super::process_lineage_from_snapshot(0, &snapshot), None);
    }

    #[test]
    fn windows_shell_idle_check_rejects_unknown_identities() {
        for (shell_created, child_created, available) in [
            (None, Some(200), false),
            (Some(100), None, false),
            (None, None, false),
            (Some(100), Some(100), false),
            (Some(100), Some(200), false),
            (Some(100), Some(50), true),
        ] {
            let snapshot = super::ProcessSnapshot::new(vec![
                test_entry_with_creation_time(10, 1, "pwsh.exe", &["pwsh.exe"], shell_created),
                test_entry_with_creation_time(20, 10, "child.exe", &["child.exe"], child_created),
            ]);
            assert_eq!(
                super::available_pane_shell_from_snapshot(10, &snapshot).as_deref(),
                available.then_some("pwsh.exe"),
                "shell={shell_created:?}, child={child_created:?}"
            );
        }
        let snapshot = super::ProcessSnapshot::new(vec![test_entry_with_creation_time(
            10,
            1,
            "pwsh.exe",
            &["pwsh.exe"],
            None,
        )]);
        assert_eq!(
            super::available_pane_shell_from_snapshot(10, &snapshot),
            None
        );
    }

    #[test]
    fn windows_process_tree_returns_shell_when_candidate_parent_chain_cycles() {
        let entries = vec![
            test_entry(10, 40, "powershell.exe", &["powershell.exe"]),
            test_entry(20, 10, "codex.exe", &["codex.exe"]),
            test_entry(30, 10, "codex.exe", &["codex.exe"]),
            test_entry(40, 10, "node.exe", &["node.exe"]),
        ];

        let job = super::select_pane_foreground_job(10, &entries).unwrap();

        assert_eq!(job.process_group_id, 10);
        assert_eq!(job.processes[0].name, "powershell.exe");
    }

    #[test]
    fn scrollback_editor_argv_uses_editor_env_and_appends_path() {
        let path = std::path::Path::new(r"C:\Users\User\AppData\Local\Temp\herdr scrollback.txt");
        let argv = super::scrollback_editor_argv_with_env(
            path,
            Some(r#""C:\Program Files\Microsoft VS Code\Code.exe" --wait"#),
        )
        .unwrap();

        assert_eq!(argv[0], r"C:\Program Files\Microsoft VS Code\Code.exe");
        assert_eq!(argv[1], "--wait");
        assert_eq!(argv[2], path.display().to_string());
    }

    #[test]
    fn scrollback_editor_argv_falls_back_to_notepad() {
        let path = std::path::Path::new(r"C:\Temp\herdr-scrollback.txt");
        let argv = super::scrollback_editor_argv_with_env(path, None).unwrap();

        assert_eq!(
            argv,
            vec!["notepad.exe".to_string(), path.display().to_string()]
        );
    }

    fn observation_entry(
        candidate: (u32, u32, &str),
        actual: (u32, Option<u64>, &str),
        command_created: Option<u64>,
        cmdline: Option<&str>,
    ) -> super::WindowsProcessEntry {
        let (pid, parent, name) = candidate;
        let mut entry = super::WindowsProcessEntry::new(pid, parent, name.into());
        entry.reader = Some(Arc::new(super::ObservationReaderStub {
            parent_pid: Some(actual.0),
            created: actual.1,
            image: actual.2.into(),
            command: super::WindowsProcessCommand::from_cmdline(
                actual.2,
                command_created,
                cmdline.map(str::to_owned),
            ),
            observations: super::AtomicU32::new(0),
            commands: super::AtomicU32::new(0),
        }));
        entry
    }

    #[test]
    fn windows_observation_identity_rejects_replaced_parent_relation() {
        let snapshot = super::ProcessSnapshot::new(vec![
            observation_entry(
                (10, 0, "pwsh.exe"),
                (0, Some(100), "pwsh.exe"),
                Some(100),
                None,
            ),
            observation_entry(
                (20, 10, "codex.exe"),
                (77, Some(300), "worker.exe"),
                Some(300),
                Some("worker.exe"),
            ),
        ]);
        let reader = snapshot.entry(20).unwrap().reader.as_ref().unwrap();
        assert_eq!(reader.parent_pid, Some(77));
        assert_eq!(reader.image, "worker.exe");
        assert!(super::descendant_entries(10, &snapshot).is_empty());
        assert!(!super::process_is_ancestor(10, 20, &snapshot));
        assert_ne!(
            super::process_lineage_from_snapshot(20, &snapshot)
                .and_then(|lineage| lineage.descends_from(10)),
            Some(true)
        );
        assert_eq!(
            super::available_pane_shell_from_snapshot(10, &snapshot).as_deref(),
            Some("pwsh.exe")
        );
    }

    #[test]
    fn windows_observation_identity_unknown_parent_is_not_verified_or_idle() {
        let mut child = observation_entry(
            (20, 10, "worker.exe"),
            (10, Some(200), "worker.exe"),
            Some(200),
            None,
        );
        Arc::get_mut(child.reader.as_mut().unwrap())
            .unwrap()
            .parent_pid = None;
        let snapshot = super::ProcessSnapshot::new(vec![
            test_entry(10, 0, "pwsh.exe", &["pwsh.exe"]),
            child,
            test_entry(30, 10, "codex.exe", &["codex.exe"]),
        ]);
        assert!(!super::process_is_ancestor(10, 20, &snapshot));
        assert_eq!(super::process_lineage_from_snapshot(20, &snapshot), None);
        assert_eq!(
            super::descendant_entries(10, &snapshot)
                .iter()
                .map(|entry| entry.pid)
                .collect::<Vec<_>>(),
            vec![30]
        );
        assert!(super::available_pane_shell_from_snapshot(10, &snapshot).is_none());
    }

    #[test]
    fn windows_observation_identity_command_cannot_replace_instance() {
        let entry = observation_entry(
            (20, 10, "node.exe"),
            (10, Some(150), "node.exe"),
            Some(300),
            Some("node.exe codex.js"),
        );
        assert_eq!(entry.creation_time(), Some(150));
        let command = entry.command();
        assert!(
            command.cmdline.is_none(),
            "a command from another instance cannot be published"
        );
        assert_eq!(entry.creation_time(), Some(150));
        assert!(!super::process_entry_identifies_agent(&entry));
    }

    #[test]
    fn windows_observation_identity_does_not_publish_old_agent_name() {
        let entry = observation_entry(
            (20, 10, "codex.exe"),
            (10, Some(300), "worker.exe"),
            Some(300),
            Some("worker.exe"),
        );
        assert!(!super::process_entry_identifies_agent(&entry));
        assert_eq!(
            super::foreground_process_from_entry(&entry).name,
            "worker.exe"
        );
    }

    #[test]
    fn windows_observation_identity_accepts_valid_siblings_and_unreadable_command() {
        for created in [100, 300] {
            let snapshot = super::ProcessSnapshot::new(vec![
                observation_entry(
                    (10, 0, "pwsh.exe"),
                    (0, Some(100), "pwsh.exe"),
                    Some(100),
                    None,
                ),
                observation_entry(
                    (20, 10, "worker.exe"),
                    (10, Some(created), "codex.exe"),
                    Some(created),
                    None,
                ),
                observation_entry((30, 10, "worker.exe"), (10, None, "worker.exe"), None, None),
            ]);
            assert_eq!(
                super::descendant_entries(10, &snapshot)
                    .iter()
                    .map(|entry| entry.pid)
                    .collect::<Vec<_>>(),
                vec![20]
            );
            assert!(super::process_is_ancestor(10, 20, &snapshot));
            assert_eq!(
                super::process_lineage_from_snapshot(20, &snapshot)
                    .unwrap()
                    .descends_from(10),
                Some(true)
            );
            assert!(super::process_entry_identifies_agent(
                snapshot.entry(20).unwrap()
            ));
            assert!(super::available_pane_shell_from_snapshot(10, &snapshot).is_none());
        }
    }

    #[test]
    fn windows_observation_identity_shared_panes_reuse_reads_and_release_pins() {
        for panes in [1, 16] {
            let entries = || {
                (0..panes)
                    .flat_map(|index| {
                        let shell = 100 + index * 10;
                        [
                            test_entry(shell, 0, "pwsh.exe", &["pwsh.exe"]),
                            test_entry(shell + 1, shell, "codex.exe", &["codex.exe"]),
                        ]
                    })
                    .collect()
            };
            let snapshot = super::ProcessSnapshot::new(entries());
            let mut cache = super::ForegroundSelectionCache::default();
            let mut pins = Vec::new();
            for index in 0..panes {
                let shell = 100 + index * 10;
                for _ in 0..3 {
                    assert!(super::process_is_ancestor(shell, shell + 1, &snapshot));
                    assert_eq!(
                        super::process_lineage_from_snapshot(shell + 1, &snapshot)
                            .unwrap()
                            .descends_from(shell),
                        Some(true)
                    );
                }
                for pid in [shell, shell + 1] {
                    let entry = snapshot.entry(pid).unwrap();
                    let reader = entry.reader.as_ref().unwrap();
                    assert_eq!(reader.observations.load(super::AtomicOrdering::Relaxed), 1);
                    assert_eq!(
                        reader.commands.load(super::AtomicOrdering::Relaxed),
                        0,
                        "ancestry never reads command lines"
                    );
                    pins.push(Arc::downgrade(entry.observation().unwrap()));
                }
                let job = super::foreground_job_from_entry(snapshot.entry(shell + 1).unwrap());
                let cached =
                    super::prepare_cached_foreground_selection(shell, &snapshot, &job).unwrap();
                cache.remember(shell, Some(cached));
                assert_eq!(
                    snapshot
                        .entry(shell)
                        .unwrap()
                        .reader
                        .as_ref()
                        .unwrap()
                        .commands
                        .load(super::AtomicOrdering::Relaxed),
                    0,
                    "cache admission reuses identity without reading commands"
                );
            }
            let refreshed = super::ProcessSnapshot::new(entries());
            drop(snapshot);
            assert!(pins.iter().all(|pin| pin.upgrade().is_some()));
            for index in 0..panes {
                let shell = 100 + index * 10;
                assert_eq!(
                    cache.get(shell, &refreshed).unwrap().process_group_id,
                    shell + 1
                );
                for pid in [shell, shell + 1] {
                    let reader = refreshed.entry(pid).unwrap().reader.as_ref().unwrap();
                    assert_eq!(
                        reader.observations.load(super::AtomicOrdering::Relaxed),
                        0,
                        "live cached pins avoid native reinspection"
                    );
                    assert_eq!(reader.commands.load(super::AtomicOrdering::Relaxed), 0);
                }
            }
            cache.entries.clear();
            assert!(pins.iter().all(|pin| pin.upgrade().is_some()));
            drop(refreshed);
            assert!(
                pins.iter().all(|pin| pin.upgrade().is_none()),
                "all observation pins are released with their last owner"
            );
        }
    }

    #[test]
    fn windows_observation_identity_lazy_unknown_and_command_read_once() {
        let entry = test_entry(10, 0, "node.exe", &["node.exe", "worker.js"]);
        let reader = Arc::clone(entry.reader.as_ref().unwrap());
        assert_eq!(reader.observations.load(super::AtomicOrdering::Relaxed), 0);
        for _ in 0..3 {
            assert_eq!(entry.creation_time(), Some(10));
            assert!(entry.command().cmdline.is_some());
            assert_eq!(entry.observed_name(), "node.exe");
        }
        assert_eq!(reader.observations.load(super::AtomicOrdering::Relaxed), 1);
        assert_eq!(reader.commands.load(super::AtomicOrdering::Relaxed), 1);
        let unknown = test_entry_with_creation_time(20, 10, "codex.exe", &["codex.exe"], None);
        for _ in 0..3 {
            assert_eq!(unknown.creation_time(), None);
            assert!(!super::process_entry_identifies_agent(&unknown));
        }
        let reader = unknown.reader.as_ref().unwrap();
        assert_eq!(reader.observations.load(super::AtomicOrdering::Relaxed), 1);
        assert_eq!(reader.commands.load(super::AtomicOrdering::Relaxed), 0);
    }

    #[test]
    fn windows_observation_identity_git_bash_agent_index_shared_at_scale() {
        for panes in [1, 16] {
            super::with_agent_classification_cache(|cache| cache.clear());
            let snapshot = super::ProcessSnapshot::new(
                (0..panes)
                    .flat_map(|index| {
                        let shell = 100 + index * 10;
                        [
                            test_entry(shell, 0, "bash.exe", &["bash.exe"]),
                            test_entry(shell + 1, 99, "node.exe", &["node.exe", "codex.js"]),
                            test_entry(shell + 2, shell + 1, "codex.exe", &["codex.exe"]),
                        ]
                    })
                    .collect(),
            );
            let mut indices: Option<&Vec<usize>> = None;
            for index in 0..panes {
                let shell = 100 + index * 10;
                let job = super::select_pane_foreground_job_from_snapshot_with_runtime_inspection(
                    shell,
                    &snapshot,
                    |_| true,
                    |entry| Some((entry.pid / 10).to_string()),
                )
                .unwrap();
                assert_eq!(job.process_group_id, shell + 1);
                let current = snapshot.agent_indices.get().unwrap();
                if let Some(previous) = indices {
                    assert!(std::ptr::eq(previous, current));
                }
                indices = Some(current);
            }
            assert_eq!(indices.unwrap().len(), panes as usize * 2);
            for entry in &snapshot.entries {
                let reader = entry.reader.as_ref().unwrap();
                assert_eq!(reader.observations.load(super::AtomicOrdering::Relaxed), 1);
                assert_eq!(
                    reader.commands.load(super::AtomicOrdering::Relaxed),
                    u32::from(entry.pid % 10 != 2)
                );
            }
        }
    }

    struct ObservationTestChild(std::process::Child);

    impl ObservationTestChild {
        fn spawn() -> Self {
            let shell = std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap())
                .join("System32")
                .join("cmd.exe");
            let mut command = Command::new(shell);
            command
                .args(["/D", "/Q", "/K"])
                .env(
                    super::PANE_RUNTIME_MARKER_ENV_VAR,
                    "observation-native-marker",
                )
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            super::configure_status_command(&mut command);
            Self(command.spawn().unwrap())
        }
    }

    impl Drop for ObservationTestChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn windows_observation_identity_native_exited_before_inspection_is_not_cached() {
        let mut child = ObservationTestChild::spawn();
        let pid = child.0.id();
        child.0.kill().unwrap();
        child.0.wait().unwrap();
        let entry = super::WindowsProcessEntry::new(pid, std::process::id(), "cmd.exe".into());
        assert!(!entry.observation().unwrap().identity.running());
        let job = super::foreground_job_from_entry(&entry);
        let snapshot = super::ProcessSnapshot::new(vec![entry]);
        assert!(super::prepare_cached_foreground_selection(pid, &snapshot, &job).is_none());
    }

    #[test]
    fn windows_observation_identity_native_same_handle_limited_and_fallback() {
        let child = ObservationTestChild::spawn();
        let entry =
            super::WindowsProcessEntry::new(child.0.id(), std::process::id(), "codex.exe".into());
        super::PROCESS_INSPECTION_COUNTS.with_borrow_mut(|counts| *counts = Default::default());
        let observation = entry.observation().unwrap();
        let handle = observation.identity.handle().unwrap();
        assert_eq!(observation.parent_pid, Some(std::process::id()));
        assert_eq!(
            Some(observation.created),
            super::process_creation_time(handle)
        );
        assert_eq!(observation.image, super::process_executable_path(handle));
        assert_eq!(observation.name().to_ascii_lowercase(), "cmd.exe");
        let limited = entry.command().cmdline.as_ref().unwrap();
        assert!(limited.contains("/K"));
        let counts = super::PROCESS_INSPECTION_COUNTS.with_borrow(|counts| *counts);
        assert_eq!(
            counts.opens, 1,
            "limited query reuses the observation handle"
        );
        assert_eq!(counts.command_reads, 1);
        let fallback = super::read_process_command_fallback(child.0.id(), observation).unwrap();
        assert_eq!(fallback, *limited);
        let wrong = super::ProcessObservation {
            identity: observation.identity.clone(),
            parent_pid: observation.parent_pid,
            created: observation.created + 1,
            image: observation.image.clone(),
        };
        assert!(super::read_process_command_fallback(child.0.id(), &wrong).is_none());
        assert_eq!(
            super::process_runtime_marker(&entry).as_deref(),
            Some("observation-native-marker")
        );
        assert_eq!(
            super::foreground_process_from_entry(&entry)
                .name
                .to_ascii_lowercase(),
            "cmd.exe"
        );
        let pid = entry.pid;
        let job = super::foreground_job_from_entry(&entry);
        let snapshot = super::ProcessSnapshot::new(vec![entry]);
        super::PROCESS_INSPECTION_COUNTS.with_borrow_mut(|counts| *counts = Default::default());
        let cached = super::prepare_cached_foreground_selection(pid, &snapshot, &job).unwrap();
        let mut cache = super::ForegroundSelectionCache::default();
        cache.remember(pid, Some(cached));
        let refreshed = super::ProcessSnapshot::new(vec![super::WindowsProcessEntry::new(
            pid,
            std::process::id(),
            "codex.exe".into(),
        )]);
        assert_eq!(cache.get(pid, &refreshed), Some(job.clone()));
        let counts = super::PROCESS_INSPECTION_COUNTS.with_borrow(|counts| *counts);
        assert_eq!(counts.opens, 0);
        assert_eq!(counts.command_reads, 0);
        assert_eq!(counts.creation_queries, 0);
        assert_eq!(counts.parent_queries, 0);
        assert_eq!(counts.image_queries, 0);

        let initialized = super::ProcessSnapshot::new(vec![super::WindowsProcessEntry::new(
            pid,
            std::process::id(),
            "codex.exe".into(),
        )]);
        let observed = initialized.entry(pid).unwrap().observation().unwrap();
        assert!(observed.same_metadata(snapshot.entry(pid).unwrap().observation().unwrap()));
        super::PROCESS_INSPECTION_COUNTS.with_borrow_mut(|counts| *counts = Default::default());
        assert_eq!(cache.get(pid, &initialized), Some(job));
        let counts = super::PROCESS_INSPECTION_COUNTS.with_borrow(|counts| *counts);
        assert_eq!(counts.opens, 0);
        assert_eq!(counts.command_reads, 0);
        assert_eq!(counts.creation_queries, 0);
        assert_eq!(counts.parent_queries, 0);
        assert_eq!(counts.image_queries, 0);
    }

    #[test]
    fn windows_observation_identity_native_exit_and_last_handle_release() {
        let mut child = ObservationTestChild::spawn();
        let entry =
            super::WindowsProcessEntry::new(child.0.id(), std::process::id(), "cmd.exe".into());
        let observation = entry.observation().unwrap();
        let created = observation.created;
        let super::ProcessIdentity::Handle(handle) = &observation.identity else {
            panic!("native handle");
        };
        let weak = Arc::downgrade(handle);
        let job = super::foreground_job_from_entry(&entry);
        let pid = entry.pid;
        let snapshot = super::ProcessSnapshot::new(vec![entry]);
        let cached = super::prepare_cached_foreground_selection(pid, &snapshot, &job).unwrap();
        drop(snapshot);
        assert!(weak.upgrade().is_some());
        child.0.kill().unwrap();
        child.0.wait().unwrap();
        assert!(!cached.shell_identity.running());
        assert_eq!(cached.shell_identity.creation_time(), Some(created));
        drop(cached);
        assert!(weak.upgrade().is_none());
    }

    fn test_entry(
        pid: u32,
        parent_pid: u32,
        name: &str,
        argv: &[&str],
    ) -> super::WindowsProcessEntry {
        // Ordinary tree fixtures use stable synthetic identities; tests for unreadable identities
        // call `test_entry_with_creation_time(..., None)` explicitly.
        test_entry_with_creation_time(pid, parent_pid, name, argv, Some(u64::from(pid)))
    }

    fn test_entry_without_cmdline(
        pid: u32,
        parent_pid: u32,
        name: &str,
        creation_time: u64,
    ) -> super::WindowsProcessEntry {
        observation_entry(
            (pid, parent_pid, name),
            (parent_pid, Some(creation_time), name),
            Some(creation_time),
            None,
        )
    }

    fn test_entry_with_creation_time(
        pid: u32,
        parent_pid: u32,
        name: &str,
        argv: &[&str],
        creation_time: Option<u64>,
    ) -> super::WindowsProcessEntry {
        let mut entry = observation_entry(
            (pid, parent_pid, name),
            (parent_pid, creation_time, name),
            creation_time,
            None,
        );
        Arc::get_mut(entry.reader.as_mut().unwrap())
            .unwrap()
            .command = super::WindowsProcessCommand {
            creation_time,
            argv0: argv.first().map(|value| (*value).to_string()),
            argv: Some(argv.iter().map(|value| (*value).to_string()).collect()),
            cmdline: Some(argv.join(" ")),
        };
        entry
    }

    #[test]
    fn process_environment_variable_parser_reads_case_insensitive_marker() {
        let environment: Vec<u16> = "PATH=C:\\Windows\0herdr_pane_runtime_id=pane-a\0\0"
            .encode_utf16()
            .collect();

        assert_eq!(
            super::environment_variable_from_utf16(
                &environment,
                super::PANE_RUNTIME_MARKER_ENV_VAR,
            )
            .as_deref(),
            Some("pane-a")
        );
    }

    #[test]
    fn pane_runtime_markers_are_distinct() {
        let first = super::next_pane_runtime_marker();
        let second = super::next_pane_runtime_marker();

        assert_ne!(first, second);
    }

    #[test]
    fn pane_runtime_marker_is_added_only_to_git_bash_environment() {
        let root = std::env::temp_dir().join(format!(
            "herdr-git-bash-test-{}",
            super::next_pane_runtime_marker()
        ));
        fs::create_dir_all(root.join("bin")).expect("create Git Bash bin fixture");
        fs::create_dir_all(root.join("usr").join("bin")).expect("create Git Bash usr/bin fixture");
        fs::create_dir_all(root.join("cmd")).expect("create Git Bash cmd fixture");
        fs::write(root.join("bin").join("bash.exe"), []).expect("create Bash fixture");
        fs::write(root.join("usr").join("bin").join("msys-2.0.dll"), [])
            .expect("create MSYS runtime fixture");
        fs::write(root.join("cmd").join("git.exe"), []).expect("create Git fixture");

        let mut git_bash = portable_pty::CommandBuilder::new(root.join("bin").join("bash.exe"));
        super::apply_pane_runtime_marker_platform(&mut git_bash);
        let mut path_resolved_git_bash = portable_pty::CommandBuilder::new("bash.exe");
        path_resolved_git_bash.env("PATH", root.join("bin"));
        super::apply_pane_runtime_marker_platform(&mut path_resolved_git_bash);
        let mut cmd = portable_pty::CommandBuilder::new("cmd.exe");
        super::apply_pane_runtime_marker_platform(&mut cmd);

        assert!(git_bash
            .get_env(super::PANE_RUNTIME_MARKER_ENV_VAR)
            .is_some_and(|value| !value.is_empty()));
        assert!(path_resolved_git_bash
            .get_env(super::PANE_RUNTIME_MARKER_ENV_VAR)
            .is_some_and(|value| !value.is_empty()));
        assert!(cmd.get_env(super::PANE_RUNTIME_MARKER_ENV_VAR).is_none());
        fs::remove_dir_all(root).expect("remove Git Bash fixture");
    }

    #[test]
    fn ime_open_reflects_open_status() {
        // IMC_GETOPENSTATUS returns nonzero when the IME is open (Hangul
        // composing) and zero for direct English/ASCII input.
        assert!(super::ime_open(1));
        assert!(!super::ime_open(0));
        // Any nonzero value is treated as open, not just 1.
        assert!(super::ime_open(2));
    }

    #[test]
    fn toggle_key_maps_korean_and_ignores_other_languages() {
        // Korean (0x0412) -> Hangul/English toggle.
        assert_eq!(
            super::toggle_key_for_language(0x0412),
            Some(super::VK_HANGUL)
        );
        // Korean with a different sublanguage still resolves by primary id.
        assert_eq!(
            super::toggle_key_for_language(0x0812),
            Some(super::VK_HANGUL)
        );
        // Japanese (0x0411) and Chinese (0x0804) have no mapped key yet.
        assert_eq!(super::toggle_key_for_language(0x0411), None);
        assert_eq!(super::toggle_key_for_language(0x0804), None);
        // English (0x0409): nothing to toggle.
        assert_eq!(super::toggle_key_for_language(0x0409), None);
    }

    #[test]
    fn send_vk_tap_reports_success_when_full_tap_is_queued() {
        let mut calls = 0;
        let ok = super::send_vk_tap_with(super::VK_HANGUL, |events| {
            calls += 1;
            events.len() as u32
        });
        assert!(ok, "a fully queued tap is reported as success");
        assert_eq!(calls, 1, "a clean tap needs no retry");
    }

    #[test]
    fn send_vk_tap_retries_keyup_and_reports_toggle_on_partial_injection() {
        let mut calls = 0;
        let mut retry_len = 0;
        let mut retry_is_keyup = false;
        let ok = super::send_vk_tap_with(super::VK_HANGUL, |events| {
            calls += 1;
            if calls == 1 {
                // Only the key-down is queued; the key-up is dropped.
                1
            } else {
                retry_len = events.len();
                // SAFETY: keyboard inputs, so reading the `ki` union is valid.
                retry_is_keyup =
                    unsafe { events[0].Anonymous.ki.dwFlags } == super::KEYEVENTF_KEYUP;
                events.len() as u32
            }
        });
        assert!(ok, "the queued key-down may have toggled the IME");
        assert_eq!(calls, 2, "the dropped key-up is retried exactly once");
        assert_eq!(retry_len, 1, "only the key-up is retried");
        assert!(retry_is_keyup, "the retry injects the key-up event");
    }

    #[test]
    fn send_vk_tap_reports_toggle_when_keyup_retry_fails() {
        let mut calls = 0;
        let ok = super::send_vk_tap_with(super::VK_HANGUL, |_events| {
            calls += 1;
            if calls == 1 {
                1
            } else {
                0
            }
        });
        assert!(ok, "the queued key-down may have toggled the IME");
        assert_eq!(calls, 2, "the dropped key-up is retried exactly once");
    }

    #[test]
    fn send_vk_tap_reports_failure_without_retry_when_nothing_is_queued() {
        let mut calls = 0;
        let ok = super::send_vk_tap_with(super::VK_HANGUL, |_events| {
            calls += 1;
            0
        });
        assert!(!ok, "a fully blocked tap is reported as failure");
        assert_eq!(
            calls, 1,
            "nothing was queued, so there is no key-up to retry"
        );
    }

    #[test]
    fn key_tap_inputs_emit_keydown_then_keyup() {
        let inputs = super::key_tap_inputs(super::VK_HANGUL);
        // SAFETY: both entries are keyboard inputs, so reading the `ki` union is valid.
        unsafe {
            assert_eq!(inputs[0].r#type, super::INPUT_KEYBOARD);
            assert_eq!(inputs[0].Anonymous.ki.wVk, super::VK_HANGUL);
            assert_eq!(inputs[0].Anonymous.ki.dwFlags, 0, "first event is key-down");
            assert_eq!(inputs[1].r#type, super::INPUT_KEYBOARD);
            assert_eq!(inputs[1].Anonymous.ki.wVk, super::VK_HANGUL);
            assert_eq!(
                inputs[1].Anonymous.ki.dwFlags,
                super::KEYEVENTF_KEYUP,
                "second event is key-up"
            );
        }
    }
}
