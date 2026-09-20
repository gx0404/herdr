//! Platform-specific process and filesystem operations.
//!
//! Centralizes OS-dependent behavior behind a clean boundary so core
//! modules don't scatter `#[cfg]` branches through product logic.

#[cfg(target_os = "linux")]
#[path = "linux/monitoring.rs"]
mod monitoring;
#[cfg(windows)]
#[path = "windows/monitoring.rs"]
mod monitoring;
#[cfg(not(any(target_os = "linux", windows)))]
#[path = "monitoring_fallback.rs"]
mod monitoring;
pub(crate) use monitoring::monitor_cpu_inventory;
pub(crate) use monitoring::terminate_usage_pty;
pub(crate) use monitoring::usage_probe_needs_job_helper;
pub(crate) use monitoring::MonitoredProcess;
pub(crate) use monitoring::{configure_usage_probe_command, terminate_usage_probe};
pub(crate) use monitoring::{monitor_environment, process_instance_token, NativeGpuCollector};
pub(crate) use monitoring::{strip_usage_statusline, usage_statusline_pipeline};
pub(crate) use monitoring::{usage_probe_exit, UsageProbeGuard};

/// statusline 包装串里的不变标记。各平台的包装形态都嵌入它（POSIX sh 是首行注释，Windows
/// 在 `-EncodedCommand` 脚本首行），识别与剥离按标记而不是按整串相等比对：包装形态升级后
/// 旧版本写下的包装仍能被识别与解除。`v1` 是标记版本，形态不兼容时新增标记并保留旧标记
/// 的解析。
pub(crate) const USAGE_STATUSLINE_MARKER: &str = "# herdr-usage v1";

/// herdr 用量回调在包装串里的调用片段；各平台从它后面取厂商名。
pub(crate) const USAGE_REPORT_INVOCATION: &str = "api usage-report";

/// herdr 写下的 Windows 包装串前缀（`-EncodedCommand` 形态）。POSIX 平台上遇到它说明 settings
/// 是从 Windows 同步来的：按「无法识别的 herdr 回调」处理，而不是当成自定义渲染器再包一层。
pub(crate) const POWERSHELL_ENCODED_COMMAND_PREFIX: &str =
    "powershell.exe -NoLogo -NoProfile -NonInteractive -EncodedCommand ";

/// 文本（包装串或其解码后的脚本）是否带 herdr 包装的特征：标记行、herdr 的 Windows 编码前缀，
/// 或回调调用与 `HERDR_ENV` / `HERDR_BIN_PATH` 环境判定同时出现。用户按文档手写的
/// `herdr api usage-report --agent <agent>` 没有这些特征，按自定义渲染器处理（启用时接在
/// 管道末端、解除时不动）。带特征却剥不出包装的命令才是「无法识别的 herdr 回调」。
pub(crate) fn looks_like_usage_wrapper(text: &str) -> bool {
    text.starts_with(USAGE_STATUSLINE_MARKER)
        || text.starts_with(POWERSHELL_ENCODED_COMMAND_PREFIX)
        || (text.contains(USAGE_REPORT_INVOCATION)
            && (text.contains(crate::HERDR_ENV_VAR) || text.contains("HERDR_BIN_PATH")))
}

/// `unrecognized_usage_statusline_error` 的固定文案，调用方与测试按它识别这类错误。
pub(crate) const UNRECOGNIZED_USAGE_STATUSLINE: &str =
    "statusLine 已含无法识别的 herdr 用量回调（其它平台或版本的形态），请手动清理后重试";

/// 命令含 `api usage-report` 却不是本平台可识别的 herdr 包装（其它平台或更早版本的形态、
/// 手工改过的包装、别的厂商的回调）时的错误：调用方据此提示手动清理，不再盲目再包一层或
/// 静默放过。
pub(crate) fn unrecognized_usage_statusline_error() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        UNRECOGNIZED_USAGE_STATUSLINE,
    )
}

/// 从包装串（或其解码后的脚本）里取出 `api usage-report --agent <agent>` 的厂商名；`None`
/// 表示不含 herdr 回调调用。厂商名到空白或 shell / PowerShell 分隔符为止，各平台共用。
pub(crate) fn usage_statusline_agent(text: &str) -> Option<&str> {
    let rest = text
        .split_once(USAGE_REPORT_INVOCATION)?
        .1
        .strip_prefix(" --agent ")?;
    let end = rest
        .find(|c: char| c.is_whitespace() || matches!(c, ';' | ')' | '}' | '|' | '"' | '\'' | '&'))
        .unwrap_or(rest.len());
    let agent = &rest[..end];
    (!agent.is_empty()).then_some(agent)
}

/// 用量探测子进程的退出形态。平台差异（Unix 的信号终止、Windows 只有退出码）在
/// `usage_probe_exit` 里收敛，核心模块只看两个事实：正常退出的退出码，或终止它的信号。
/// 被信号终止（OOM、外部 kill、崩溃）不是 CLI 的结论，调用方应归为可重试的瞬时错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct UsageProbeExit {
    /// 正常退出时的退出码；被信号终止时为 `None`。
    pub code: Option<i32>,
    /// 终止信号编号（仅 Unix 有意义）；正常退出时为 `None`。
    pub signal: Option<i32>,
}

impl UsageProbeExit {
    /// 退出码 0 才算成功；被信号终止不算。
    pub(crate) fn success(self) -> bool {
        self.code == Some(0)
    }

    /// 由 `std::process::ExitStatus` 换算：非 Linux 平台的 `try_wait` 路径共用（Linux 用
    /// `waitid` 的 siginfo 直接构造）。
    #[cfg(not(target_os = "linux"))]
    pub(crate) fn from_status(status: std::process::ExitStatus) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            if let Some(signal) = status.signal() {
                return Self {
                    code: None,
                    signal: Some(signal),
                };
            }
        }
        Self {
            code: status.code(),
            signal: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForegroundProcess {
    pub pid: u32,
    pub name: String,
    pub argv0: Option<String>,
    pub argv: Option<Vec<String>>,
    pub cmdline: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForegroundJob {
    pub process_group_id: u32,
    pub processes: Vec<ForegroundProcess>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Hangup,
    Terminate,
    Kill,
}

/// Why a pane runtime ended, before application persistence policy is applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildExitReason {
    Exited,
    Interrupted,
    /// 退出码看起来是宿主关机/重启打断的结果，但无法与正常退出区分。
    /// 按打断处理，只影响持久化检查点策略；哪些退出码算可疑由各平台的
    /// `classify_child_exit` 自己决定。
    SuspectedInterruption,
    /// Imported runtimes have no child wait handle in the replacement server.
    #[cfg(unix)]
    Handoff,
    WaitFailed,
}

impl ChildExitReason {
    pub(crate) fn requires_session_checkpoint(self) -> bool {
        match self {
            Self::Interrupted | Self::SuspectedInterruption => true,
            #[cfg(unix)]
            Self::Handoff => true,
            _ => false,
        }
    }
}

#[cfg(unix)]
pub(crate) use unix_common::classify_child_exit;

/// 没有平台实现时只认「正常退出」：可疑退出码表是 OS 专属语义，各
/// `src/platform/<os>.rs` 自己维护（见 `docs/AGENT_RULES/platform.md`）。
#[cfg(not(any(unix, windows)))]
pub(crate) fn classify_child_exit(_status: &portable_pty::ExitStatus) -> ChildExitReason {
    ChildExitReason::Exited
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn launch_executable() -> std::io::Result<std::path::PathBuf> {
    std::env::current_exe()
}

pub(crate) fn detached_custom_command_process(command: &str) -> std::process::Command {
    let mut process = detached_custom_command_process_platform(command);
    configure_background_command(&mut process);
    process
}

pub(crate) fn pane_custom_command_pty_builder(command: &str) -> portable_pty::CommandBuilder {
    pane_custom_command_pty_builder_platform(command)
}

pub(crate) fn apply_pane_runtime_marker(command: &mut portable_pty::CommandBuilder) {
    apply_pane_runtime_marker_platform(command);
}

pub(crate) fn prepare_paste_text_for_pty(text: String) -> String {
    prepare_paste_text_for_pty_platform(text)
}

pub(crate) fn plugin_runtime_path(path: &std::path::Path) -> std::path::PathBuf {
    plugin_runtime_path_platform(path)
}

#[cfg(not(windows))]
fn plugin_runtime_path_platform(path: &std::path::Path) -> std::path::PathBuf {
    path.to_path_buf()
}

#[cfg(not(windows))]
fn prepare_paste_text_for_pty_platform(text: String) -> String {
    text
}

#[cfg(not(windows))]
pub(crate) fn terminal_title_for_presentation(title: &str) -> &str {
    title
}

#[cfg(not(windows))]
fn apply_pane_runtime_marker_platform(_command: &mut portable_pty::CommandBuilder) {}

pub(crate) fn configure_background_command(command: &mut std::process::Command) {
    configure_background_command_platform(command);
}

#[cfg(not(windows))]
fn configure_background_command_platform(_command: &mut std::process::Command) {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PlatformCapabilities {
    pub(crate) live_handoff: bool,
    pub(crate) direct_terminal_attach: bool,
    pub(crate) preserve_legacy_doubled_escape_input: bool,
}

pub(crate) const fn capabilities() -> PlatformCapabilities {
    PlatformCapabilities {
        live_handoff: cfg!(unix),
        direct_terminal_attach: cfg!(unix),
        preserve_legacy_doubled_escape_input: cfg!(target_os = "macos"),
    }
}

pub(crate) fn terminal_grid_size() -> std::io::Result<(u16, u16)> {
    #[cfg(unix)]
    let (cols, rows) = unix_common::read_terminal_grid_size()?;
    #[cfg(windows)]
    let (cols, rows) = windows::read_terminal_grid_size()?;
    #[cfg(not(any(unix, windows)))]
    let (cols, rows) = fallback::read_terminal_grid_size()?;

    if cols == 0 || rows == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "terminal reported a zero-sized grid",
        ));
    }
    Ok((cols, rows))
}

#[cfg(not(windows))]
pub fn launch_server_daemon_command(command: &mut std::process::Command) -> std::io::Result<u32> {
    command.spawn().map(|child| child.id())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn detach_server_daemon_command(command: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;

    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// Detaches a child from the controlling terminal so OpenSSH cannot read
/// prompts from `/dev/tty` and must use its `SSH_ASKPASS` helper. Older
/// OpenSSH releases without `SSH_ASKPASS_REQUIRE` only consult the helper
/// when no controlling terminal exists; releases that honor `force` are
/// unaffected. Windows OpenSSH has no `/dev/tty` concept and needs nothing.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn detach_child_from_controlling_terminal(command: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;

    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn detach_child_from_controlling_terminal(_command: &mut std::process::Command) {}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn current_process_is_detached_server_daemon() -> bool {
    unsafe { libc::getsid(0) == libc::getpid() }
}

/// Raised by the SIGWINCH handler, consumed by the host resize watcher.
#[cfg(unix)]
static TERMINAL_RESIZE_SIGNALLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(unix)]
extern "C" fn record_terminal_resize_signal(_signal: libc::c_int) {
    TERMINAL_RESIZE_SIGNALLED.store(true, std::sync::atomic::Ordering::Release);
}

/// Records SIGWINCH events that size polling can miss.
#[cfg(unix)]
pub(crate) fn watch_terminal_resize_signal() {
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction =
        record_terminal_resize_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
    // Keep blocking stdin and socket reads from failing with EINTR.
    action.sa_flags = libc::SA_RESTART;
    unsafe {
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(libc::SIGWINCH, &action, std::ptr::null_mut());
    }
}

#[cfg(not(unix))]
pub(crate) fn watch_terminal_resize_signal() {}

/// Returns whether a terminal size change was signalled since the last call.
#[cfg(unix)]
pub(crate) fn take_terminal_resize_signal() -> bool {
    TERMINAL_RESIZE_SIGNALLED.swap(false, std::sync::atomic::Ordering::AcqRel)
}

/// Windows relies on size polling.
#[cfg(not(unix))]
pub(crate) fn take_terminal_resize_signal() -> bool {
    false
}

#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardCommand {
    pub program: &'static str,
    pub args: &'static [&'static str],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardImage {
    pub bytes: Vec<u8>,
    pub extension: &'static str,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LimitedRead {
    Empty,
    Complete(Vec<u8>),
    Oversized,
}

pub(crate) fn read_limited_reader(
    mut reader: impl std::io::Read,
    max_bytes: usize,
) -> std::io::Result<LimitedRead> {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 8192];

    while bytes.len() < max_bytes {
        let remaining = max_bytes - bytes.len();
        let read_len = remaining.min(buffer.len());
        let bytes_read = match reader.read(&mut buffer[..read_len]) {
            Ok(bytes_read) => bytes_read,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        };
        if bytes_read == 0 {
            return if bytes.is_empty() {
                Ok(LimitedRead::Empty)
            } else {
                Ok(LimitedRead::Complete(bytes))
            };
        }
        bytes.extend_from_slice(&buffer[..bytes_read]);
    }

    let mut sentinel = [0_u8; 1];
    loop {
        return match reader.read(&mut sentinel) {
            Ok(0) if bytes.is_empty() => Ok(LimitedRead::Empty),
            Ok(0) => Ok(LimitedRead::Complete(bytes)),
            Ok(_) => Ok(LimitedRead::Oversized),
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => Err(err),
        };
    }
}

#[derive(Debug, Clone)]
pub(crate) struct RemoteSshConfigPaths {
    pub(crate) user_config: Option<std::path::PathBuf>,
    pub(crate) system_config: Option<std::path::PathBuf>,
    pub(crate) multiplexing: bool,
}

/// Home directory used to expand a leading `~` in SSH client config paths
/// (`Include`, `IdentityFile`). Matches the per-OS home source already used by
/// `remote_ssh_config_paths`; only the variable selection is OS-specific, so a
/// pure strategy constant keeps both branches compiling on every target.
pub(crate) fn ssh_config_home_dir() -> Option<std::path::PathBuf> {
    let non_empty = |key: &str| std::env::var_os(key).filter(|value| !value.is_empty());
    if cfg!(windows) {
        non_empty("USERPROFILE")
            .or_else(|| match (non_empty("HOMEDRIVE"), non_empty("HOMEPATH")) {
                (Some(drive), Some(path)) => {
                    let mut home = std::path::PathBuf::from(drive);
                    home.push(path);
                    Some(home.into_os_string())
                }
                _ => None,
            })
            .or_else(|| non_empty("HOME"))
            .map(std::path::PathBuf::from)
    } else {
        non_empty("HOME").map(std::path::PathBuf::from)
    }
}

pub(crate) const REMOTE_BRIDGE_IDLE_TIMEOUT_SUPPORTED: bool =
    cfg!(any(target_os = "linux", target_os = "macos"));

#[cfg(unix)]
mod remote_bridge;
#[cfg(all(test, unix))]
mod remote_bridge_tests;
#[cfg(unix)]
mod unix_common;
#[cfg(unix)]
pub(crate) use unix_common::{
    begin_cli_output, default_known_hosts_path, detach_stdout, end_cli_output,
    forward_remote_bridge_stdio, RemoteBridgeWake,
};

mod client_state;
pub(crate) use client_state::{create_private_state_file, replace_file, sync_parent_directory};

#[cfg(not(unix))]
pub(crate) fn begin_cli_output() {}

#[cfg(not(unix))]
pub(crate) fn end_cli_output() {}

/// 没有 stdout 句柄语义的平台：让不出去，调用方退回到等待上报结束。
#[cfg(not(any(unix, windows)))]
pub(crate) fn detach_stdout() -> std::io::Result<()> {
    Ok(())
}

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::*;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::*;

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
pub use windows::*;

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
mod fallback;
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub use fallback::*;

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn available_pane_shell_from_job(child_pid: u32, job: ForegroundJob) -> Option<String> {
    if job.process_group_id != child_pid
        || job.processes.iter().any(|process| process.pid != child_pid)
    {
        return None;
    }
    job.processes
        .into_iter()
        .find(|process| process.pid == child_pid)
        .map(|process| process.name)
        .filter(|name| is_pane_shell_process_name(name))
}

fn normalized_process_name(name: &str) -> String {
    name.rsplit(['/', '\\'])
        .next()
        .unwrap_or(name)
        .trim_start_matches('-')
        .trim_end_matches(".exe")
        .to_ascii_lowercase()
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn is_powershell_process_name(name: &str) -> bool {
    matches!(
        normalized_process_name(name).as_str(),
        "pwsh" | "powershell"
    )
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn interactive_unix_shell_command(
    argv: &[String],
    shell_name: &str,
    quote_posix_arg: fn(&str) -> String,
) -> Option<String> {
    let quote = if is_powershell_process_name(shell_name) {
        quote_powershell_arg
    } else {
        quote_posix_arg
    };
    let mut parts = argv.iter();
    let mut command = quote(parts.next()?);
    for part in parts {
        command.push(' ');
        command.push_str(&quote(part));
    }
    Some(command)
}

pub(crate) fn quote_powershell_arg(value: &str) -> String {
    if !value.is_empty()
        && !value.starts_with('-')
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'_' | b'-' | b'.' | b'/' | b':' | b'+' | b'=')
        })
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "''"))
}

pub(crate) fn quote_windows_command_line_arg(value: &str) -> String {
    if !value.is_empty()
        && !value
            .chars()
            .any(|ch| matches!(ch, ' ' | '\t' | '\n' | '\x0b' | '"'))
    {
        return value.to_string();
    }

    let mut quoted = String::from("\"");
    let mut backslashes = 0;
    for ch in value.chars() {
        if ch == '\\' {
            backslashes += 1;
            continue;
        }
        if ch == '"' {
            quoted.push_str(&"\\".repeat(backslashes * 2 + 1));
        } else {
            quoted.push_str(&"\\".repeat(backslashes));
        }
        backslashes = 0;
        quoted.push(ch);
    }
    quoted.push_str(&"\\".repeat(backslashes * 2));
    quoted.push('"');
    quoted
}

pub(crate) fn is_pane_shell_process_name(name: &str) -> bool {
    let normalized = normalized_process_name(name);
    matches!(
        normalized.as_str(),
        "sh" | "bash"
            | "dash"
            | "zsh"
            | "fish"
            | "ksh"
            | "mksh"
            | "csh"
            | "tcsh"
            | "elvish"
            | "xonsh"
            | "nu"
            | "pwsh"
            | "powershell"
            | "cmd"
    )
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn process_agent_hint(_pid: u32) -> Option<crate::detect::Agent> {
    None
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn parse_agent_env_hint(environ: &[u8]) -> Option<crate::detect::Agent> {
    for record in environ.split(|&byte| byte == 0) {
        let Some(value) = record.strip_prefix(b"HERDR_AGENT=") else {
            continue;
        };
        return crate::detect::parse_agent_label(std::str::from_utf8(value).ok()?);
    }
    None
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
#[derive(Debug)]
pub(crate) struct InputSourceRestore;

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub(crate) fn switch_to_ascii_input_source() -> Option<InputSourceRestore> {
    None
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub(crate) fn pump_input_source_runloop() {}

/// Switches the host keyboard input source while prefix mode is active.
///
/// `App` drives this through a trait so the prefix-mode transitions can be
/// tested with a fake, without touching the real macOS APIs or leaking a
/// platform-specific restore type into `App`.
pub(crate) trait PrefixInputSource {
    /// Switch to an ASCII-capable input source for prefix commands. No-op if
    /// the current source is already ASCII-capable, the platform is
    /// unsupported, or the switch fails. Calling it again before `restore`
    /// keeps the source saved by the first call.
    fn switch_to_ascii(&mut self);

    /// Restore whatever `switch_to_ascii` saved. No-op if nothing was switched.
    fn restore(&mut self);
}

/// Production [`PrefixInputSource`] backed by the per-platform API.
#[derive(Default)]
pub(crate) struct RealPrefixInputSource {
    restore: Option<InputSourceRestore>,
}

impl PrefixInputSource for RealPrefixInputSource {
    fn switch_to_ascii(&mut self) {
        if self.restore.is_none() {
            // Drain pending input-source-change notifications so the read below is fresh (see
            // `pump_input_source_runloop`); a no-op on non-macOS.
            pump_input_source_runloop();
            self.restore = switch_to_ascii_input_source();
        }
    }

    fn restore(&mut self) {
        let _ = self.restore.take();
    }
}

#[cfg(all(test, any(unix, windows)))]
#[test]
fn child_exit_classification_only_checkpoints_interruptions() {
    for code in [0, 2, 130, 255, 0xC0000005] {
        let reason = classify_child_exit(&portable_pty::ExitStatus::with_exit_code(code));
        assert_eq!(reason, ChildExitReason::Exited, "exit code {code:#x}");
        assert!(!reason.requires_session_checkpoint());
    }
    #[cfg(windows)]
    let status = portable_pty::ExitStatus::with_exit_code(0xC000013A);
    #[cfg(not(windows))]
    let status = portable_pty::ExitStatus::with_signal("Terminated: 15");
    assert_eq!(classify_child_exit(&status), ChildExitReason::Interrupted);
    assert!(classify_child_exit(&status).requires_session_checkpoint());
    #[cfg(unix)]
    assert!(ChildExitReason::Handoff.requires_session_checkpoint());
    assert!(!ChildExitReason::WaitFailed.requires_session_checkpoint());
}

/// 跨平台可测试契约：`1` 是 unix 与 windows 都认可的可疑退出码，`0`/`2` 不是。
/// `128 + signal` 那半张码表是 unix 语义，由 `unix_common` 的测试守。
#[cfg(all(test, any(unix, windows)))]
#[test]
fn exit_code_one_is_a_suspected_interruption_on_every_platform() {
    let reason = classify_child_exit(&portable_pty::ExitStatus::with_exit_code(1));
    assert_eq!(reason, ChildExitReason::SuspectedInterruption);
    assert!(reason.requires_session_checkpoint());
}

/// 主机重启时 shell 捕获 SIGHUP/SIGTERM 后自报 `128 + signal`：必须触发会话
/// 检查点，否则 pane 逐个移除会把 session.json 清空（HSR-04 / 上游 #4320）。
#[cfg(all(test, unix))]
#[test]
fn unix_signal_exit_codes_are_classified_as_suspected_interruptions() {
    for code in [129, 143] {
        let reason = classify_child_exit(&portable_pty::ExitStatus::with_exit_code(code));
        assert_eq!(
            reason,
            ChildExitReason::SuspectedInterruption,
            "exit code {code}"
        );
        assert!(reason.requires_session_checkpoint(), "exit code {code}");
    }
    assert!(!unix_common::exit_code_suspects_host_shutdown(0));
    assert!(!unix_common::exit_code_suspects_host_shutdown(2));
    assert!(unix_common::exit_code_suspects_host_shutdown(1));
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn terminal_resize_signal_is_recorded_once_per_delivery() {
        watch_terminal_resize_signal();
        assert!(!take_terminal_resize_signal());

        unsafe {
            libc::raise(libc::SIGWINCH);
        }

        assert!(take_terminal_resize_signal());
        assert!(!take_terminal_resize_signal());
    }

    #[test]
    fn pane_shell_process_names_reject_exec_replacement_programs() {
        for shell in ["bash", "-zsh", "/bin/fish", "pwsh", "powershell.exe"] {
            assert!(is_pane_shell_process_name(shell), "{shell}");
        }
        for program in ["vim", "nvim", "cargo", "test-runner", "opencode"] {
            assert!(!is_pane_shell_process_name(program), "{program}");
        }
    }

    #[test]
    fn detached_custom_command_preserves_unix_login_shell_flag() {
        let cmd = detached_custom_command_process("echo hello");
        assert_eq!(cmd.get_program(), std::ffi::OsStr::new("/bin/sh"));
        assert_eq!(
            cmd.get_args().collect::<Vec<_>>(),
            [
                std::ffi::OsStr::new("-lc"),
                std::ffi::OsStr::new("echo hello")
            ]
        );
    }

    #[test]
    fn pane_custom_command_builder_preserves_unix_shell_flag() {
        let expected: Vec<std::ffi::OsString> =
            vec!["/bin/sh".into(), "-c".into(), "echo hello".into()];
        assert_eq!(
            pane_custom_command_pty_builder("echo hello").get_argv(),
            &expected
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn parse_agent_env_hint_accepts_known_agents() {
        assert_eq!(
            parse_agent_env_hint(b"PATH=/bin\0HERDR_AGENT=claude\0TERM=xterm\0"),
            Some(crate::detect::Agent::Claude)
        );
        assert_eq!(
            parse_agent_env_hint(b"HERDR_AGENT=codex"),
            Some(crate::detect::Agent::Codex)
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn parse_agent_env_hint_ignores_missing_or_unknown_agents() {
        assert_eq!(parse_agent_env_hint(b"PATH=/bin\0TERM=xterm\0"), None);
        assert_eq!(parse_agent_env_hint(b"HERDR_AGENT=not-an-agent\0"), None);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn interactive_shell_command_quotes_for_posix_and_powershell() {
        let argv = vec![
            "pi".into(),
            String::new(),
            "two words".into(),
            "a'b".into(),
            "$HOME".into(),
            "semi;colon".into(),
            "@options".into(),
        ];
        assert_eq!(
            interactive_shell_command(&argv, "bash").as_deref(),
            Some("pi '' 'two words' 'a'\\''b' '$HOME' 'semi;colon' @options")
        );
        assert_eq!(
            interactive_shell_command(&argv, "pwsh").as_deref(),
            Some("pi '' 'two words' 'a''b' '$HOME' 'semi;colon' '@options'")
        );
    }

    #[test]
    fn read_limited_reader_returns_complete_data_under_limit() {
        let input = std::io::Cursor::new(b"image".to_vec());
        assert_eq!(
            read_limited_reader(input, 16).expect("limited read"),
            LimitedRead::Complete(b"image".to_vec())
        );
    }

    #[test]
    fn read_limited_reader_returns_empty_for_empty_input() {
        let input = std::io::Cursor::new(Vec::<u8>::new());
        assert_eq!(
            read_limited_reader(input, 16).expect("limited read"),
            LimitedRead::Empty
        );
    }

    #[test]
    fn read_limited_reader_accepts_data_exactly_at_limit() {
        let input = std::io::Cursor::new(b"four".to_vec());
        assert_eq!(
            read_limited_reader(input, 4).expect("limited read"),
            LimitedRead::Complete(b"four".to_vec())
        );
    }

    #[test]
    fn read_limited_reader_rejects_data_over_limit() {
        let input = std::io::Cursor::new(b"oversized".to_vec());
        assert_eq!(
            read_limited_reader(input, 4).expect("limited read"),
            LimitedRead::Oversized
        );
    }

    #[test]
    fn read_limited_reader_retries_interrupted_reads() {
        struct InterruptedOnce {
            interrupted: bool,
            inner: std::io::Cursor<Vec<u8>>,
        }

        impl std::io::Read for InterruptedOnce {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                if !self.interrupted {
                    self.interrupted = true;
                    return Err(std::io::ErrorKind::Interrupted.into());
                }
                self.inner.read(buffer)
            }
        }

        let input = InterruptedOnce {
            interrupted: false,
            inner: std::io::Cursor::new(b"image".to_vec()),
        };
        assert_eq!(
            read_limited_reader(input, 16).expect("limited read"),
            LimitedRead::Complete(b"image".to_vec())
        );
    }

    /// 厂商名解析是各平台包装串识别的共同判据：到空白或 shell / PowerShell 分隔符为止，
    /// 没有 `--agent` 或厂商名为空都视为不含回调。
    #[test]
    fn usage_statusline_agent_stops_at_shell_delimiters() {
        assert_eq!(
            usage_statusline_agent(
                "exec \"$HERDR_BIN_PATH\" api usage-report --agent claude --passthrough; else"
            ),
            Some("claude")
        );
        assert_eq!(
            usage_statusline_agent(
                "\"$HERDR_BIN_PATH\" api usage-report --agent antigravity; else :; fi)"
            ),
            Some("antigravity")
        );
        assert_eq!(
            usage_statusline_agent("& $env:HERDR_BIN_PATH api usage-report --agent claude}else{}"),
            Some("claude")
        );
        assert_eq!(
            usage_statusline_agent("herdr api usage-report --agent codex"),
            Some("codex")
        );
        assert_eq!(
            usage_statusline_agent("herdr api usage-report --passthrough"),
            None
        );
        assert_eq!(
            usage_statusline_agent("herdr api usage-report --agent "),
            None
        );
        assert_eq!(usage_statusline_agent("python custom.py"), None);
    }

    #[test]
    fn usage_wrapper_features_distinguish_herdr_wrappers_from_hand_written_commands() {
        assert!(looks_like_usage_wrapper(
            "# herdr-usage v1\n(if true; then :; fi)"
        ));
        assert!(looks_like_usage_wrapper(
            "powershell.exe -NoLogo -NoProfile -NonInteractive -EncodedCommand AAAA"
        ));
        assert!(looks_like_usage_wrapper(
            "(if [ \"${HERDR_ENV:-}\" = 1 ]; then \"$HERDR_BIN_PATH\" api usage-report --agent claude; fi)"
        ));
        assert!(looks_like_usage_wrapper(
            "if($env:HERDR_ENV -eq '1'){& $env:HERDR_BIN_PATH api usage-report --agent claude}"
        ));
        // 文档推荐的手写集成：不是 herdr 写下的包装，按自定义渲染器处理。
        assert!(!looks_like_usage_wrapper(
            "herdr api usage-report --agent claude"
        ));
        assert!(!looks_like_usage_wrapper(
            "cat | herdr api usage-report --agent claude --passthrough | bash status.sh"
        ));
        assert!(!looks_like_usage_wrapper(
            "powershell.exe -NoLogo -EncodedCommand AAAA api usage-report --agent claude"
        ));
        assert!(!looks_like_usage_wrapper("python custom.py"));
        assert!(!looks_like_usage_wrapper(""));
    }
}
