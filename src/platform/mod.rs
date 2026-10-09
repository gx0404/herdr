//! Platform-specific process and filesystem operations.
//!
//! Centralizes OS-dependent behavior behind a clean boundary so core
//! modules don't scatter `#[cfg]` branches through product logic.

pub(crate) mod codex_launch;

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

/// `unrecognized_usage_statusline_error` 的说明（按调用进程的界面语言取，文档终审 D7）；调用方
/// 与测试按它识别这类错误。
pub(crate) fn unrecognized_usage_statusline_message() -> &'static str {
    crate::i18n::texts().usage_probe.statusline_unrecognized
}

/// 命令含 `api usage-report` 却不是本平台可识别的 herdr 包装（其它平台或更早版本的形态、
/// 手工改过的包装、别的厂商的回调）时的错误：调用方据此提示手动清理，不再盲目再包一层或
/// 静默放过。
pub(crate) fn unrecognized_usage_statusline_error() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        unrecognized_usage_statusline_message(),
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

#[cfg(unix)]
pub(crate) mod ssh_agent;

pub(crate) struct HostShutdownMonitor {
    task: Option<tokio::task::JoinHandle<()>>,
}

impl HostShutdownMonitor {
    pub(crate) fn start(
        requested: std::sync::Arc<std::sync::atomic::AtomicBool>,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        let task = monitor_host_shutdown(requested, wake);
        Self { task }
    }
}

impl Drop for HostShutdownMonitor {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn monitor_host_shutdown(
    _requested: std::sync::Arc<std::sync::atomic::AtomicBool>,
    _wake: impl Fn() + Send + Sync + 'static,
) -> Option<tokio::task::JoinHandle<()>> {
    None
}

#[cfg(not(windows))]
pub(crate) fn host_shutdown_in_progress() -> bool {
    false
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

/// A request from outside the process to stop the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ServerQuitSignal {
    #[cfg(unix)]
    Interrupt,
    #[cfg(unix)]
    Terminate,
    #[cfg(not(unix))]
    ConsoleControl,
}

impl std::fmt::Display for ServerQuitSignal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            #[cfg(unix)]
            Self::Interrupt => "SIGINT",
            #[cfg(unix)]
            Self::Terminate => "SIGTERM",
            #[cfg(not(unix))]
            Self::ConsoleControl => "console control event",
        })
    }
}

#[cfg(not(unix))]
pub(crate) fn spawn_server_signal_monitor(
    on_quit: impl Fn(ServerQuitSignal) + Send + Sync + 'static,
) {
    if let Err(err) = ctrlc::set_handler(move || on_quit(ServerQuitSignal::ConsoleControl)) {
        tracing::warn!(%err, "failed to install server stop handler");
    }
}

#[cfg(not(unix))]
pub(crate) fn ignore_server_hangup() {}

#[cfg(not(unix))]
pub(crate) fn local_stream_peer_description(_stream: &crate::ipc::LocalStream) -> Option<String> {
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Hangup,
    Terminate,
    Kill,
}

/// 一棵 pane 进程树的锚点：Unix 上是会话 id（session leader 退出后仍然有效），Windows
/// 没有会话语义、用 pane 自己的 child 进程实例代指这棵树。
///
/// 终止阶梯在事件循环里对 child 快照一次锚点（[`process_session_id`]，单次廉价查询），
/// 之后后台线程凭锚点枚举成员（`session_members_batch`）。这样即便 child 在投递到
/// 执行之间被 `wait` 回收、进程表条目消失，会话里的孙进程仍然找得到，也不会因为 pid
/// 被复用而误伤无关进程（HSR-01）。
///
/// Windows 的 pid 在进程退出、最后一个句柄关闭后就可能被复用，单靠 pid 认不出原来那个进程，
/// 所以锚点还带着两个时刻（FILETIME，100 ns 刻度）：
///
/// - `instance`：根进程的创建时间。枚举时 pid 当前的主人创建时间对得上才算根进程本身。
/// - `captured`：快照锚点的时刻。读它时根进程还占着这个 pid，此后拿到这个 pid 的进程（连同
///   它的子进程）都创建于这一刻之后；根进程核对不上时（已退出且 pid 已空出，或 pid 已换了
///   主人），只有创建于 `[instance, captured)` 之间、父 pid 指向它的进程才认作根进程留下的孤儿。
///
/// Unix 上两者恒为 0：会话 id 在会话还有成员时不会被复用，不需要核对。同一个根进程两次快照
/// 的 `captured` 不同，比较是否同一个根用 [`ProcessSessionId::same_root`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProcessSessionId {
    /// Unix：会话 id。Windows：pane 根进程（child）的 pid。
    pub id: i64,
    pub instance: u64,
    pub captured: u64,
}

impl ProcessSessionId {
    /// 两个锚点是否指向同一个根进程实例（不比较快照时刻）。
    pub fn same_root(self, other: Self) -> bool {
        self.id == other.id && self.instance == other.instance
    }
}

/// 终止阶梯里的一个会话成员：pid 加上枚举时读到的进程实例标记。
///
/// 成员集合在阶梯开始前枚举一次，之后要等几百毫秒才升级信号；其间成员可能已经退出、pid 被
/// 别的进程拿走。Windows 上 `instance` 是成员的创建时间，判活与发信号都先按它核对，pid 换了
/// 主人就当成员已退出、不发信号。Unix 上恒为 0、不核对，行为与按 pid 发信号相同。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ProcessSessionMember {
    pub pid: u32,
    pub instance: u64,
}

/// 一次扫描取出整批会话的成员，桶与 `sessions` 一一对应。Unix 成员不带实例标记。
#[cfg(not(windows))]
pub(crate) fn session_members_batch(
    sessions: &[ProcessSessionId],
) -> Vec<Vec<ProcessSessionMember>> {
    session_processes_batch(sessions)
        .into_iter()
        .map(|pids| {
            pids.into_iter()
                .map(|pid| ProcessSessionMember { pid, instance: 0 })
                .collect()
        })
        .collect()
}

#[cfg(not(windows))]
pub(crate) fn signal_session_members(members: &[ProcessSessionMember], signal: Signal) {
    let pids: Vec<u32> = members.iter().map(|member| member.pid).collect();
    signal_processes(&pids, signal);
}

/// 成员是否仍需等待它退出，语义同 `process_alive_excluding_zombies`。
#[cfg(not(windows))]
pub(crate) fn session_member_alive(member: ProcessSessionMember) -> bool {
    process_alive_excluding_zombies(member.pid)
}

/// server 启动时调用：脱离控制台的 daemon 向 pane 进程树的枚举表明身份，免得随拉起它的
/// pane 一起被终止。Unix 的 daemon 用 setsid 脱离了 pane 的会话，什么都不用做。
#[cfg(not(windows))]
pub(crate) fn announce_detached_server_daemon() {}

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
pub(crate) use unix_common::{classify_child_exit, poll_fd_readable, read_fd};

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

pub(crate) fn normalize_cwd_for_launch(path: &std::path::Path) -> std::path::PathBuf {
    normalize_cwd_for_launch_platform(path)
}

#[cfg(not(windows))]
fn normalize_cwd_for_launch_platform(path: &std::path::Path) -> std::path::PathBuf {
    path.to_path_buf()
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

/// Ordered directories that may hold the agent CLI `command` (integration and account usage
/// availability, usage probes). Every platform searches the process `PATH` first; Windows adds
/// the current registry `PATH` and known per-user install locations, because a long-running
/// server keeps the `PATH` it started with (`windows/command_search.rs`).
pub(crate) fn command_search_dirs(command: &str) -> Vec<std::path::PathBuf> {
    #[cfg(test)]
    if let Some(paths) = crate::config::test_dirs::search_path() {
        return std::env::split_paths(&paths).collect();
    }
    command_search_dirs_platform(command)
}

/// Files to try for `command` inside one search directory, in lookup order (Windows: `PATHEXT`
/// order with `.ps1` last; elsewhere the bare name).
pub(crate) fn command_file_candidates(
    dir: &std::path::Path,
    command: &str,
) -> Vec<std::path::PathBuf> {
    command_file_candidates_platform(dir, command)
}

/// First candidate across [`command_search_dirs`] that `accept` takes, in search order: the
/// executable to start for `command`.
pub(crate) fn find_command(
    command: &str,
    accept: impl Fn(&std::path::Path) -> bool,
) -> Option<std::path::PathBuf> {
    command_search_dirs(command).into_iter().find_map(|dir| {
        command_file_candidates(&dir, command)
            .into_iter()
            .find(|path| accept(path))
    })
}

/// Whether `command` is installed: a candidate [`find_command`] would start, or, as the last
/// resort and for availability only, a platform fallback name that cannot be started directly
/// (Windows: an extensionless shell shim).
pub(crate) fn command_installed(command: &str, accept: impl Fn(&std::path::Path) -> bool) -> bool {
    let dirs = command_search_dirs(command);
    dirs.iter().any(|dir| {
        command_file_candidates(dir, command)
            .iter()
            .any(|path| accept(path))
    }) || dirs
        .iter()
        .filter_map(|dir| command_availability_fallback_platform(dir, command))
        .any(|path| accept(&path))
}

/// Program and leading arguments that start a resolved CLI executable (Windows runs PowerShell
/// shims through `powershell.exe -File`).
pub(crate) fn cli_invocation(
    executable: &std::path::Path,
) -> (std::ffi::OsString, Vec<std::ffi::OsString>) {
    cli_invocation_platform(executable)
}

/// `PATH` for CLIs the server starts on the user's behalf (usage probes): Windows adds the
/// current registry `PATH` entries the long-running server lacks, so interpreters installed
/// after it started resolve; `None` keeps the inherited `PATH`.
pub(crate) fn cli_child_path() -> Option<std::ffi::OsString> {
    cli_child_path_platform()
}

#[cfg(not(windows))]
fn command_search_dirs_platform(_command: &str) -> Vec<std::path::PathBuf> {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).collect())
        .unwrap_or_default()
}

#[cfg(not(windows))]
fn command_file_candidates_platform(
    dir: &std::path::Path,
    command: &str,
) -> Vec<std::path::PathBuf> {
    vec![dir.join(command)]
}

#[cfg(not(windows))]
fn command_availability_fallback_platform(
    _dir: &std::path::Path,
    _command: &str,
) -> Option<std::path::PathBuf> {
    None
}

#[cfg(not(windows))]
fn cli_invocation_platform(
    executable: &std::path::Path,
) -> (std::ffi::OsString, Vec<std::ffi::OsString>) {
    (executable.as_os_str().to_os_string(), Vec::new())
}

#[cfg(not(windows))]
fn cli_child_path_platform() -> Option<std::ffi::OsString> {
    None
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

/// The byte the host terminal sends for Backspace according to the tty's erase
/// setting (e.g. `^H` for MobaXterm and PuTTY-style terminals), when known.
pub(crate) fn terminal_erase_byte() -> Option<u8> {
    #[cfg(unix)]
    return unix_common::terminal_erase_byte();
    #[cfg(not(unix))]
    None
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

#[cfg(not(target_os = "macos"))]
pub(crate) fn prepare_server_process(_handoff_import: bool) -> std::io::Result<bool> {
    Ok(false)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn detach_server_daemon_command(command: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;

    #[cfg(target_os = "macos")]
    macos::configure_server_daemon_context(command);

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

/// User-level SSH client config path shared by the per-OS
/// `remote_ssh_config_paths`; only the variable selection is OS-specific
/// (`USERPROFILE` vs `HOME`). Tests pin a thread-local home
/// (`config::test_dirs::set_home_dir`) instead of mutating the process
/// environment, which would race sibling tests under `cargo test`.
pub(crate) fn remote_ssh_user_config_path() -> Option<std::path::PathBuf> {
    #[cfg(test)]
    if let Some(home) = crate::config::test_dirs::home_dir() {
        return Some(home.join(".ssh").join("config"));
    }
    let key = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(key)
        .map(std::path::PathBuf::from)
        .map(|home| home.join(".ssh").join("config"))
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

/// 取 `remote_private_temp_base()` 并确保它存在：在其下新建私有目录项（受管 ssh 配置目录、
/// 下载目录）之前调用。Windows 的基准在 herdr 自己的状态目录里，顺带清理已退出进程留下的
/// 私有目录项（见 `sweep_stale_remote_private_entries_platform`）。
pub(crate) fn ensure_remote_private_temp_base() -> std::io::Result<std::path::PathBuf> {
    let base = remote_private_temp_base();
    std::fs::create_dir_all(&base)?;
    sweep_stale_remote_private_entries_platform(&base);
    Ok(base)
}

/// unix（及其余非 Windows 平台）的基准是系统临时目录（`$TMPDIR` / `/tmp`），不扫：系统
/// 会清理（开机清空、systemd-tmpfiles / macOS 定期清理）；它是所有程序共用的大目录，逐项
/// 扫描放在用户操作路径上不便宜；容器共享 `/tmp` 时本 pid 命名空间里查不到的 pid 不代表
/// 属主已退出；跨进程复用的共享通道目录（`herdr-ssh-<uid>-<摘要>`）也不按 pid 命名。
#[cfg(not(windows))]
fn sweep_stale_remote_private_entries_platform(_base: &std::path::Path) {}

pub(crate) const REMOTE_BRIDGE_IDLE_TIMEOUT_SUPPORTED: bool =
    cfg!(any(target_os = "linux", target_os = "macos"));

#[cfg(unix)]
mod remote_bridge;
#[cfg(all(test, unix))]
mod remote_bridge_tests;
#[cfg(unix)]
mod unix_common;
#[cfg(unix)]
pub(crate) mod unix_image_files;
#[cfg(all(test, unix))]
pub(crate) use unix_common::remote_bridge_endpoint_path_under;
#[cfg(unix)]
pub(crate) use unix_common::{
    begin_cli_output, default_known_hosts_path, detach_stdout, end_cli_output,
    forward_remote_bridge_stdio, ignore_server_hangup, local_stream_peer_description,
    peek_unix_stream, spawn_server_signal_monitor, ssh_auth_sock_path_is_live, RemoteBridgeWake,
};

/// Windows 的 SSH agent 是命名管道（Win32-OpenSSH 服务），不走文件系统 socket 路径；
/// 保留继承语义，不做路径活性判定。
#[cfg(not(unix))]
pub(crate) fn ssh_auth_sock_path_is_live(_path: &std::path::Path) -> bool {
    true
}

mod client_state;
pub(crate) use client_state::{create_private_state_file, replace_file, sync_parent_directory};
mod persist_files;
pub(crate) use persist_files::{
    check_persist_source, create_persist_temporary, discard_persist_temporary,
    prepare_persist_metadata, publish_persist_recovery, same_persist_file, sync_directory,
};

mod process_lineage;
pub(crate) use process_lineage::{
    multiplexer_client_lineages, walk_process_lineage, ProcessLineage, ProcessParentEntry,
};

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

#[cfg(not(windows))]
pub(crate) use process_cwd as pane_process_cwd;

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
mod fallback;
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub use fallback::*;

/// Detection-loop process queries block long enough on this platform (Windows: Toolhelp
/// snapshots and per-process handle queries) to run on the blocking pool instead of the async
/// workers; elsewhere they are cheap procfs/sysctl reads and stay inline.
pub(crate) const PROCESS_QUERIES_BLOCK: bool = cfg!(windows);

/// Whether a pane shell sits at its prompt with nothing running, for periodic detection checks
/// (Windows answers "busy" from a snapshot shared across panes and confirms "idle" live).
#[cfg(not(windows))]
pub(crate) fn pane_shell_is_idle(child_pid: u32) -> bool {
    available_pane_shell(child_pid).is_some()
}

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

    #[cfg(target_os = "linux")]
    #[test]
    fn live_pane_process_group_rejects_processes_outside_the_pane_session() {
        use std::os::unix::process::CommandExt;

        let mut detached = std::process::Command::new("sleep");
        detached.arg("30");
        // SAFETY: setsid is async-signal-safe and touches only the child.
        unsafe {
            detached.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        let mut detached = detached.spawn().expect("spawn detached");
        let token = process_start_token(detached.id()).expect("start token");
        let mut gone = std::process::Command::new("true").spawn().expect("spawn");
        let gone_pid = gone.id();
        gone.wait().expect("reap");

        assert_eq!(
            live_pane_process_group(std::process::id(), detached.id(), token),
            None,
            "a live process in another terminal session"
        );
        assert_eq!(
            live_pane_process_group(gone_pid, detached.id(), token),
            None,
            "a pane shell that is gone"
        );
        let _ = detached.kill();
        let _ = detached.wait();
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn live_pane_process_group_follows_the_agent_process_not_its_job() {
        use std::os::unix::process::CommandExt;

        let shell_pid = std::process::id();
        let mut wrapper = std::process::Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .expect("spawn wrapper");
        let job = wrapper.id();
        let mut agent = std::process::Command::new("sleep")
            .arg("30")
            .process_group(job as i32)
            .spawn()
            .expect("spawn agent");
        let agent_pid = agent.id();
        let token = process_start_token(agent_pid).expect("agent start token");
        let wrapper_token = process_start_token(job).expect("wrapper start token");

        assert_eq!(
            live_pane_process_group(shell_pid, agent_pid, token),
            Some(job)
        );
        assert_eq!(
            live_pane_process_group(shell_pid, agent_pid, token + 1),
            None,
            "a reused pid has a different start token"
        );
        unsafe {
            libc::kill(agent_pid as libc::pid_t, libc::SIGSTOP);
        }
        assert_eq!(
            live_pane_process_group(shell_pid, agent_pid, token),
            Some(job)
        );

        unsafe {
            libc::kill(agent_pid as libc::pid_t, libc::SIGKILL);
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while live_pane_process_group(shell_pid, agent_pid, token).is_some()
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(
            live_pane_process_group(shell_pid, agent_pid, token),
            None,
            "an unreaped agent must not count as alive while its wrapper lives"
        );
        assert_eq!(
            live_pane_process_group(shell_pid, job, wrapper_token),
            Some(job)
        );
        agent.wait().expect("reap agent");
        assert_eq!(live_pane_process_group(shell_pid, agent_pid, token), None);
        let _ = wrapper.kill();
        let _ = wrapper.wait();
    }

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

    #[cfg(unix)]
    #[test]
    fn ssh_auth_sock_path_is_live_requires_an_owned_socket() {
        // 隔离的临时根：路径唯一（pid + 进程级计数器），测试失败时也随根目录删掉。
        let dirs = crate::config::test_dirs::isolate_dirs("sock-live");
        let dir = dirs.state_dir().to_path_buf();
        std::fs::create_dir_all(&dir).expect("create test dir");

        assert!(!ssh_auth_sock_path_is_live(&dir.join("missing")));
        let regular = dir.join("regular");
        std::fs::write(&regular, b"x").expect("write regular file");
        assert!(!ssh_auth_sock_path_is_live(&regular));
        assert!(!ssh_auth_sock_path_is_live(&dir));
        let socket = dir.join("agent.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&socket).expect("bind socket");
        assert!(ssh_auth_sock_path_is_live(&socket));

        std::fs::remove_dir_all(&dir).expect("cleanup");
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

    #[cfg(target_os = "linux")]
    #[test]
    fn server_daemon_detach_creates_new_session() {
        let mut command = std::process::Command::new("sh");
        command.arg("-c").arg(
            r#"sid=$(ps -o sid= -p $$ | tr -d ' ')
test "$sid" = "$$"
"#,
        );
        detach_server_daemon_command(&mut command);

        let status = command.status().unwrap();
        assert!(
            status.success(),
            "detached server child should be its own session leader"
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
