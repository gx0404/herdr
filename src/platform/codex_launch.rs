//! Native shim creation, foreground process replacement and probe supervision.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};

const MARKER: &str = ".herdr-codex-launch-v1";
#[cfg(unix)]
const LAUNCHER: &str = "codex";
#[cfg(not(unix))]
const LAUNCHER: &str = "codex.exe";

pub(crate) fn is_shim(executable: &Path) -> bool {
    executable
        .parent()
        .is_some_and(|parent| parent.join(MARKER).is_file())
}

/// Returns a private directory whose `codex` launcher runs `executable`.
///
/// Windows (and every test build) shares one directory per executable identity instead of
/// creating one per process: across volumes every directory held a full copy of the binary,
/// and even a hard link pins each replaced binary on disk. `shared` covers reuse and cleanup.
/// Unix production keeps a private per-process symlink directory: it costs nothing and the OS
/// cleans the temp dir. Existing panes can outlive a replaced server, so a launch path is only
/// removed once nothing holds or runs it and it has gone unused for a stale age.
pub(crate) fn install_shim(executable: &Path) -> io::Result<PathBuf> {
    #[cfg(any(test, not(unix)))]
    match shared::install_for_process(executable) {
        Ok(directory) => return Ok(directory),
        Err(error) => tracing::warn!(
            %error,
            "shared Codex shim unavailable; creating a per-process shim"
        ),
    }
    create_process_shim(&std::env::temp_dir(), "herdr-codex-", executable)
}

/// Creates `<root>/<prefix><pid>-<attempt>` holding the marker and a launcher for `target`.
fn create_process_shim(root: &Path, prefix: &str, target: &Path) -> io::Result<PathBuf> {
    for attempt in 0..100 {
        let directory = root.join(format!("{prefix}{}-{attempt}", std::process::id()));
        match super::create_remote_private_dir(&directory) {
            Ok(()) => {
                // Marker first: a directory that a crashed process left half-built still
                // carries it, so a later sweep can collect it.
                let result = std::fs::write(directory.join(MARKER), b"v1\n")
                    .and_then(|()| install_link(target, &directory));
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
fn install_link(target: &Path, directory: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, directory.join(LAUNCHER))
}

#[cfg(not(unix))]
fn install_link(target: &Path, directory: &Path) -> io::Result<()> {
    let launcher = directory.join(LAUNCHER);
    std::fs::hard_link(target, &launcher)
        .or_else(|_| std::fs::copy(target, launcher).map(|_| ()))?;
    #[cfg(windows)]
    native_tools::install(directory)?;
    Ok(())
}

#[cfg(windows)]
mod native_tools;

pub(crate) fn dispatch_native_tool(argv: &[OsString]) -> Option<io::Result<()>> {
    #[cfg(windows)]
    {
        native_tools::dispatch(argv)
    }
    #[cfg(not(windows))]
    {
        let _ = argv;
        None
    }
}

pub(crate) fn configure_child(command: &mut Command) -> io::Result<()> {
    #[cfg(windows)]
    {
        native_tools::configure(command)
    }
    #[cfg(not(windows))]
    {
        let _ = command;
        Ok(())
    }
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

/// Shims shared by every process that runs the same executable: one directory per executable
/// identity (a digest of its canonical path, length and modification time) in a private base.
///
/// A directory is built in a private per-process sibling and renamed into place, so readers only
/// ever see complete shims; racing installers adopt the winner's directory, and a damaged one is
/// rebuilt, or bypassed by keeping the sibling. Each process sweeps stale shims once in the
/// background. On Windows a process leases its shim before validating it and keeps the lease
/// while it lives, and a shim is only removed under a claim, which no lease allows: a shim that
/// validated under a lease cannot vanish while its process may still hand it to new panes. A
/// process that cannot lease the shared shim uses a per-process one, which sweeps keep while
/// the process lives.
#[cfg(any(test, not(unix)))]
mod shared {
    use std::io;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use sha2::{Digest as _, Sha256};

    use super::{LAUNCHER, MARKER};

    const HOUR: Duration = Duration::from_secs(60 * 60);
    /// Release grace period after a shim's last use, for users that no lease shows: pane
    /// processes that outlived their server.
    const WEEK: Duration = Duration::from_secs(7 * 24 * 60 * 60);
    /// Development builds are rebuilt many times a day and every rebuild is a new identity
    /// (usually a full copy, as `target/` rarely shares a volume with the state dir); a day
    /// still covers panes that outlive a dev server.
    const DAY: Duration = Duration::from_secs(24 * 60 * 60);
    /// Removals per sweep: a large backlog is worked off over several process starts.
    const SWEEP_LIMIT: usize = 64;

    pub(super) fn install_for_process(executable: &Path) -> io::Result<PathBuf> {
        static SWEEP: std::sync::Once = std::sync::Once::new();
        let store = Store::for_process();
        let installed = store.install(executable, SystemTime::now())?;
        hold(installed.lease);
        let identity = installed.identity;
        SWEEP.call_once(move || store.sweep_in_background(identity));
        Ok(installed.directory)
    }

    #[cfg(windows)]
    pub(super) fn directory_for_current_process() -> io::Result<PathBuf> {
        let executable = std::env::current_exe()?;
        if let Some((directory, lease)) = lease_running_shim(&executable)? {
            refresh(&directory, SystemTime::now());
            hold(Some(lease));
            return Ok(directory);
        }
        install_for_process(&executable)
    }

    #[cfg(windows)]
    fn lease_running_shim(executable: &Path) -> io::Result<Option<(PathBuf, Lease)>> {
        if !super::is_shim(executable) {
            return Ok(None);
        }
        let directory = executable
            .parent()
            .ok_or_else(|| io::Error::other("Codex shim has no parent directory"))?;
        let lease = Source::inspect(executable)?
            .lease_valid(directory)
            .ok_or_else(|| {
                io::Error::other("running Codex shim is incomplete or cannot be leased")
            })?;
        Ok(Some((directory.to_path_buf(), lease)))
    }

    /// A shim ready to hand to panes.
    struct Installed {
        directory: PathBuf,
        /// This executable's identity name, which sweeps must keep.
        identity: String,
        /// Always present for the shared shim; a per-process shim may go without, as sweeps
        /// keep it while its owner lives.
        lease: Option<Lease>,
    }

    /// Where shared shims live and when leftovers count as abandoned.
    struct Store {
        /// Identity directories and per-process `<pid>-<attempt>` siblings.
        base: PathBuf,
        /// Holds the pre-identity `herdr-codex-<pid>-<attempt>` shims; production only.
        legacy_root: Option<PathBuf>,
        stale_age: Duration,
        alive: fn(u32) -> bool,
    }

    impl Store {
        fn for_process() -> Self {
            let alive = crate::platform::process_exists;
            if cfg!(test) {
                // Test binaries never create or sweep production shims, and the copy of a
                // rebuilt test binary is collected within the hour.
                Self {
                    base: std::env::temp_dir().join("herdr-unit-codex-shim"),
                    legacy_root: None,
                    stale_age: HOUR,
                    alive,
                }
            } else {
                // The per-user state dir rather than the temp dir: a reused launcher must not
                // come from a directory that other users can write to (a shared TEMP), and
                // temp cleaners leave it intact.
                Self {
                    base: crate::config::state_dir().join("codex-shims"),
                    legacy_root: Some(std::env::temp_dir()),
                    stale_age: if cfg!(debug_assertions) { DAY } else { WEEK },
                    alive,
                }
            }
        }

        /// Returns the shared shim under a lease, or a per-process shim when the shared one
        /// cannot be used or leased.
        fn install(&self, executable: &Path, now: SystemTime) -> io::Result<Installed> {
            let source = Source::inspect(executable)?;
            ensure_private_dir(&self.base)?;
            let identity = self.base.join(&source.name);
            let installed = |directory, lease| Installed {
                directory,
                identity: source.name.clone(),
                lease,
            };
            if let Some(lease) = source.lease_valid(&identity) {
                refresh(&identity, now);
                return Ok(installed(identity, Some(lease)));
            }
            let staged = super::create_process_shim(&self.base, "", &source.path)?;
            let mut directory = publish(&source, staged, &identity, now);
            if directory == identity {
                if let Some(lease) = source.lease_valid(&identity) {
                    return Ok(installed(identity, Some(lease)));
                }
                // A sweep elsewhere claimed it for removal after it was published.
                tracing::warn!(
                    path = %identity.display(),
                    "could not lease the shared Codex shim; using a per-process shim"
                );
                directory = super::create_process_shim(&self.base, "", &source.path)?;
            }
            // Sweeps keep a per-process shim while its owner lives; a lease also covers a
            // liveness check that errs.
            let lease = Lease::take(&directory).ok();
            Ok(installed(directory, lease))
        }

        /// Removes stale shims, best effort and at most `SWEEP_LIMIT` per call. Candidates are
        /// directories named exactly as this module names them: identity directories other
        /// than `keep`, and per-process ones whose owner has exited. A candidate goes only if
        /// it carries the marker, the marker is older than the stale age, no live process
        /// holds the shim and its launcher is not running. Reparse points are left alone.
        fn sweep(&self, keep: &str, now: SystemTime) -> usize {
            let roots = std::iter::once((&self.base, ""))
                .chain(self.legacy_root.iter().map(|root| (root, "herdr-codex-")));
            let mut removed = 0;
            for (root, prefix) in roots {
                let entries = match std::fs::read_dir(root) {
                    Ok(entries) => entries,
                    Err(error) => {
                        if error.kind() != io::ErrorKind::NotFound {
                            tracing::debug!(
                                root = %root.display(),
                                %error,
                                "could not scan for stale Codex shims"
                            );
                        }
                        continue;
                    }
                };
                for entry in entries.filter_map(Result::ok) {
                    if removed == SWEEP_LIMIT {
                        return removed;
                    }
                    let name = entry.file_name();
                    let Some(name) = name.to_str() else {
                        continue;
                    };
                    let candidate = match process_entry(name, prefix) {
                        Some(pid) => !(self.alive)(pid),
                        None => prefix.is_empty() && identity_name(name) && name != keep,
                    };
                    if candidate && self.collect(&entry.path(), now) {
                        removed += 1;
                    }
                }
            }
            removed
        }

        fn collect(&self, directory: &Path, now: SystemTime) -> bool {
            let stale = std::fs::symlink_metadata(directory)
                .is_ok_and(|metadata| plain_dir(&metadata))
                && std::fs::symlink_metadata(directory.join(MARKER))
                    .ok()
                    .filter(plain_file)
                    .and_then(|marker| marker.modified().ok())
                    .and_then(|modified| now.duration_since(modified).ok())
                    .is_some_and(|age| age > self.stale_age);
            if !stale {
                return false;
            }
            match remove_shim(directory) {
                Ok(()) => true,
                Err(error) => {
                    tracing::debug!(
                        path = %directory.display(),
                        %error,
                        "could not remove stale Codex shim"
                    );
                    false
                }
            }
        }

        fn sweep_in_background(self, keep: String) {
            let spawned = std::thread::Builder::new()
                .name("herdr-codex-shim-sweep".into())
                .spawn(move || {
                    let removed = self.sweep(&keep, SystemTime::now());
                    if removed > 0 {
                        tracing::debug!(removed, "removed stale Codex shims");
                    }
                });
            if let Err(error) = spawned {
                tracing::debug!(%error, "could not start the Codex shim sweep");
            }
        }
    }

    /// The executable behind a shim and the identity name every process running it derives.
    struct Source {
        /// Canonical, so every spelling of one executable shares a directory.
        path: PathBuf,
        len: u64,
        name: String,
    }

    impl Source {
        fn inspect(executable: &Path) -> io::Result<Self> {
            let path = executable.canonicalize()?;
            let metadata = std::fs::metadata(&path)?;
            let (after_epoch, offset) = match metadata.modified()?.duration_since(UNIX_EPOCH) {
                Ok(offset) => (1_u8, offset),
                Err(error) => (0, error.duration()),
            };
            let bytes = path.as_os_str().as_encoded_bytes();
            let digest = Sha256::new()
                .chain_update(MARKER)
                .chain_update((bytes.len() as u64).to_le_bytes())
                .chain_update(bytes)
                .chain_update(metadata.len().to_le_bytes())
                .chain_update([after_epoch])
                .chain_update(offset.as_secs().to_le_bytes())
                .chain_update(offset.subsec_nanos().to_le_bytes())
                .finalize();
            let name = digest[..16]
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            Ok(Self {
                path,
                len: metadata.len(),
                name,
            })
        }

        /// Leases `directory` first and then checks it: under the lease no sweep can remove it,
        /// so it stays valid. A lease on an invalid shim is released again, since it would keep
        /// `make_way` from clearing the damage.
        fn lease_valid(&self, directory: &Path) -> Option<Lease> {
            let lease = Lease::take(directory).ok()?;
            self.matches(directory).then_some(lease)
        }

        /// Whether `directory` is a complete shim for this executable.
        fn matches(&self, directory: &Path) -> bool {
            #[cfg(windows)]
            if !super::native_tools::matches(directory) {
                return false;
            }
            std::fs::symlink_metadata(directory).is_ok_and(|metadata| plain_dir(&metadata))
                && marked(directory)
                && self.launches(&directory.join(LAUNCHER))
        }

        #[cfg(unix)]
        fn launches(&self, launcher: &Path) -> bool {
            std::fs::read_link(launcher).is_ok_and(|target| target == self.path)
                && std::fs::metadata(launcher).is_ok_and(|metadata| metadata.len() == self.len)
        }

        /// A hard link or a complete copy has the executable's length; a truncated or foreign
        /// file does not.
        #[cfg(not(unix))]
        fn launches(&self, launcher: &Path) -> bool {
            std::fs::symlink_metadata(launcher)
                .is_ok_and(|metadata| plain_file(&metadata) && metadata.len() == self.len)
        }
    }

    /// Renames a complete staged shim into place. Racing installers contend for one atomic
    /// rename (it fails onto a non-empty directory); the losers discard their copy and adopt
    /// the winner's directory.
    fn publish(source: &Source, staged: PathBuf, identity: &Path, now: SystemTime) -> PathBuf {
        // Only onto a free path: on Windows the rename would replace a foreign file there.
        if make_way(source, identity) && std::fs::rename(&staged, identity).is_ok() {
            return identity.to_path_buf();
        }
        if source.matches(identity) {
            discard(&staged);
            refresh(identity, now);
            return identity.to_path_buf();
        }
        tracing::warn!(
            path = %identity.display(),
            "shared Codex shim is unusable; keeping a per-process shim"
        );
        staged
    }

    /// Whether the identity path is free, after clearing what is not worth keeping there: an
    /// empty directory, or a damaged shim of ours that no live process holds (a lease blocks
    /// the claim that `remove_shim` needs). Anything else, including a valid shim, stays.
    fn make_way(source: &Source, identity: &Path) -> bool {
        let gone = || {
            std::fs::symlink_metadata(identity)
                .is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
        };
        match std::fs::symlink_metadata(identity) {
            Err(error) => error.kind() == io::ErrorKind::NotFound,
            Ok(metadata) if !plain_dir(&metadata) => false,
            Ok(_) => {
                std::fs::remove_dir(identity).is_ok()
                    || (marked(identity)
                        && !source.matches(identity)
                        && remove_shim(identity).is_ok())
                    // A sweep elsewhere may just have finished removing it.
                    || gone()
            }
        }
    }

    /// Launchers run from the base, so an existing base is only accepted as a private
    /// directory.
    fn ensure_private_dir(base: &Path) -> io::Result<()> {
        if let Some(parent) = base.parent() {
            std::fs::create_dir_all(parent)?;
        }
        match crate::platform::create_remote_private_dir(base) {
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                if private_dir(base) {
                    Ok(())
                } else {
                    Err(io::Error::other(format!(
                        "{} is not a private directory",
                        base.display()
                    )))
                }
            }
            result => result,
        }
    }

    #[cfg(unix)]
    fn private_dir(path: &Path) -> bool {
        use std::os::unix::fs::MetadataExt as _;
        // SAFETY: geteuid only reads this process's effective user id.
        let uid = unsafe { libc::geteuid() };
        std::fs::symlink_metadata(path).is_ok_and(|metadata| {
            plain_dir(&metadata) && metadata.uid() == uid && metadata.mode() & 0o077 == 0
        })
    }

    /// Both bases sit in per-user directories (the state dir, or the test base in the temp
    /// dir), so only require a real directory here.
    #[cfg(not(unix))]
    fn private_dir(path: &Path) -> bool {
        std::fs::symlink_metadata(path).is_ok_and(|metadata| plain_dir(&metadata))
    }

    fn marked(directory: &Path) -> bool {
        std::fs::symlink_metadata(directory.join(MARKER))
            .is_ok_and(|metadata| plain_file(&metadata))
    }

    fn plain_dir(metadata: &std::fs::Metadata) -> bool {
        metadata.is_dir() && !reparse_point(metadata)
    }

    fn plain_file(metadata: &std::fs::Metadata) -> bool {
        metadata.is_file() && !reparse_point(metadata)
    }

    /// Every reparse point, including the kinds that `is_symlink` does not report.
    #[cfg(windows)]
    fn reparse_point(metadata: &std::fs::Metadata) -> bool {
        use std::os::windows::fs::MetadataExt as _;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }

    #[cfg(not(windows))]
    fn reparse_point(metadata: &std::fs::Metadata) -> bool {
        metadata.file_type().is_symlink()
    }

    fn identity_name(name: &str) -> bool {
        name.len() == 32
            && name
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    }

    /// Owner pid of `<prefix><pid>-<attempt>` exactly as `create_process_shim` names it.
    fn process_entry(name: &str, prefix: &str) -> Option<u32> {
        let (pid, attempt) = name.strip_prefix(prefix)?.split_once('-')?;
        decimal(attempt)?;
        decimal(pid).filter(|pid| *pid != 0)
    }

    /// A number exactly as `format!` writes it: digits only, no leading zero.
    fn decimal(value: &str) -> Option<u32> {
        let canonical = !value.is_empty()
            && value.bytes().all(|byte| byte.is_ascii_digit())
            && (value == "0" || !value.starts_with('0'));
        canonical.then(|| value.parse().ok()).flatten()
    }

    /// Removes a shim that no live process holds: the claim fails while any lease is held and
    /// shuts out new leases meanwhile. The launcher goes first, since Windows refuses to delete
    /// a running one and the shim must then stay intact and marked for a later sweep; the
    /// directory goes once the marker is gone and the claim released.
    fn remove_shim(directory: &Path) -> io::Result<()> {
        {
            let _claim = Claim::take(directory)?;
            remove_file_if_present(&directory.join(LAUNCHER))?;
            remove_file_if_present(&directory.join(MARKER))?;
        }
        std::fs::remove_dir_all(directory)
    }

    fn remove_file_if_present(path: &Path) -> io::Result<()> {
        match std::fs::remove_file(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            result => result,
        }
    }

    fn discard(staged: &Path) {
        if let Err(error) = remove_shim(staged) {
            tracing::debug!(
                path = %staged.display(),
                %error,
                "could not discard staged Codex shim"
            );
        }
    }

    /// Reuse counts as use: sweeps measure staleness from the marker's mtime.
    fn refresh(directory: &Path, now: SystemTime) {
        let result = std::fs::OpenOptions::new()
            .write(true)
            .open(directory.join(MARKER))
            .and_then(|marker| marker.set_modified(now));
        if let Err(error) = result {
            tracing::debug!(
                path = %directory.display(),
                %error,
                "could not refresh Codex shim marker"
            );
        }
    }

    /// Keeps a lease for the process lifetime: the process may hand the shim to new panes at
    /// any time.
    fn hold(lease: Option<Lease>) {
        static LEASES: std::sync::Mutex<Vec<Lease>> = std::sync::Mutex::new(Vec::new());
        if let Some(lease) = lease {
            LEASES
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(lease);
        }
    }

    /// A shared hold on a shim's marker (open without delete sharing): while any lease lives,
    /// every `Claim` fails, so no process removes the shim or deletes its marker.
    struct Lease {
        #[cfg(windows)]
        _marker: std::fs::File,
    }

    /// An exclusive hold on a shim's marker for removing the shim: it fails while any lease is
    /// held, and leases fail until it is dropped. Only delete sharing is left, for the marker
    /// itself to go.
    struct Claim {
        #[cfg(windows)]
        _marker: std::fs::File,
    }

    #[cfg(windows)]
    impl Lease {
        fn take(directory: &Path) -> io::Result<Self> {
            use std::os::windows::fs::OpenOptionsExt as _;
            use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
            let marker = std::fs::OpenOptions::new()
                .read(true)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
                .open(directory.join(MARKER))?;
            Ok(Self { _marker: marker })
        }
    }

    #[cfg(windows)]
    impl Claim {
        fn take(directory: &Path) -> io::Result<Self> {
            use std::os::windows::fs::OpenOptionsExt as _;
            use windows_sys::Win32::Storage::FileSystem::{DELETE, FILE_SHARE_DELETE};
            let marker = std::fs::OpenOptions::new()
                .access_mode(DELETE)
                .share_mode(FILE_SHARE_DELETE)
                .open(directory.join(MARKER))?;
            Ok(Self { _marker: marker })
        }
    }

    /// Outside Windows only test processes share shims, and they are short-lived.
    #[cfg(not(windows))]
    impl Lease {
        fn take(_directory: &Path) -> io::Result<Self> {
            Ok(Self {})
        }
    }

    #[cfg(not(windows))]
    impl Claim {
        fn take(_directory: &Path) -> io::Result<Self> {
            Ok(Self {})
        }
    }

    #[cfg(test)]
    mod tests {
        use std::fs;

        use super::*;
        use crate::config::test_dirs::{isolate_dirs, IsolatedDirs};

        const LIVE: u32 = 4242;
        const DEAD: u32 = 4343;
        const KEEP: &str = "0123456789abcdef0123456789abcdef";
        const OTHER: &str = "fedcba9876543210fedcba9876543210";

        struct Fixture {
            _dirs: IsolatedDirs,
            root: PathBuf,
            executable: PathBuf,
            store: Store,
        }

        fn fixture(name: &str) -> Fixture {
            let dirs = isolate_dirs(name);
            let root = dirs.state_dir().to_path_buf();
            let legacy_root = root.join("temp");
            fs::create_dir_all(&legacy_root).unwrap();
            let executable = root.join("herdr.exe");
            fs::write(&executable, b"fake herdr executable").unwrap();
            let store = Store {
                base: root.join("shims"),
                legacy_root: Some(legacy_root),
                stale_age: HOUR,
                alive: |pid| pid == LIVE,
            };
            Fixture {
                _dirs: dirs,
                root,
                executable,
                store,
            }
        }

        /// Whole seconds survive a round trip through any file system's timestamps.
        fn now() -> SystemTime {
            let elapsed = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
            UNIX_EPOCH + Duration::from_secs(elapsed.as_secs())
        }

        fn set_modified(path: &Path, modified: SystemTime) {
            fs::OpenOptions::new()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(modified)
                .unwrap();
        }

        /// A shim-shaped directory whose marker was last refreshed `age` before `now`.
        fn plant(directory: &Path, now: SystemTime, age: Duration) {
            fs::create_dir_all(directory).unwrap();
            fs::write(directory.join(LAUNCHER), b"launcher").unwrap();
            fs::write(directory.join(MARKER), b"v1\n").unwrap();
            set_modified(&directory.join(MARKER), now - age);
        }

        fn names(directory: &Path) -> Vec<String> {
            let mut names: Vec<_> = fs::read_dir(directory)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .collect();
            names.sort();
            names
        }

        fn remove_launcher(directory: &Path) {
            fs::remove_file(directory.join(LAUNCHER)).unwrap();
        }

        /// Unlinks first: the launcher may be a hard link to the fixture executable.
        fn replace_launcher(directory: &Path) {
            remove_launcher(directory);
            plant_foreign_launcher(directory);
        }

        /// A launcher that does not run the fixture executable.
        #[cfg(unix)]
        fn plant_foreign_launcher(directory: &Path) {
            std::os::unix::fs::symlink("/bin/sh", directory.join(LAUNCHER)).unwrap();
        }

        /// Shorter than the executable, like a copy cut short.
        #[cfg(not(unix))]
        fn plant_foreign_launcher(directory: &Path) {
            fs::write(directory.join(LAUNCHER), b"fake").unwrap();
        }

        fn empty_directory(directory: &Path) {
            fs::remove_dir_all(directory).unwrap();
            fs::create_dir(directory).unwrap();
        }

        #[cfg(unix)]
        fn link_dir(link: &Path, target: &Path) {
            std::os::unix::fs::symlink(target, link).unwrap();
        }

        /// A junction: unlike a directory symlink, it needs no privilege to create.
        #[cfg(not(unix))]
        fn link_dir(link: &Path, target: &Path) {
            let status = std::process::Command::new("cmd.exe")
                .args(["/D", "/Q", "/C", "mklink", "/J"])
                .arg(link)
                .arg(target)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap();
            assert!(status.success());
        }

        #[test]
        fn identity_follows_the_canonical_executable_and_changes_with_length_or_mtime() {
            let fixture = fixture("codex-shim-identity");
            let executable = &fixture.executable;
            let identity = |path: &Path| Source::inspect(path).unwrap().name;
            let modified = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
            set_modified(executable, modified);
            let original = identity(executable);
            assert!(identity_name(&original), "{original}");
            let respelled = fixture.root.join("temp").join("..").join("herdr.exe");
            assert_eq!(identity(&respelled), original);

            fs::write(executable, b"fake herdr executable, rebuilt").unwrap();
            set_modified(executable, modified);
            let longer = identity(executable);
            fs::write(executable, b"fake herdr executable").unwrap();
            set_modified(executable, modified + Duration::from_secs(1));
            let touched = identity(executable);
            assert_ne!(longer, original);
            assert_ne!(touched, original);
            assert_ne!(touched, longer);

            set_modified(executable, modified);
            assert_eq!(identity(executable), original);
            let elsewhere = fixture.root.join("other.exe");
            fs::copy(executable, &elsewhere).unwrap();
            set_modified(&elsewhere, modified);
            assert_ne!(identity(&elsewhere), original);
        }

        #[test]
        fn installs_reuse_the_identity_directory_without_rebuilding_it() {
            let fixture = fixture("codex-shim-reuse");
            let store = &fixture.store;
            let now = now();
            let first = store.install(&fixture.executable, now).unwrap();
            let (directory, identity) = (first.directory.clone(), first.identity.clone());
            assert_eq!(directory, store.base.join(&identity));
            assert!(first.lease.is_some());
            assert!(Source::inspect(&fixture.executable)
                .unwrap()
                .matches(&directory));
            assert!(super::super::is_shim(&directory.join(LAUNCHER)));
            // A rebuilt directory would lose this file.
            fs::write(directory.join("sentinel"), b"").unwrap();
            set_modified(&directory.join(MARKER), now - HOUR);

            let later = now + Duration::from_secs(60);
            let again = store.install(&fixture.executable, later).unwrap();
            assert_eq!((&again.directory, &again.identity), (&directory, &identity));
            assert!(again.lease.is_some());
            assert!(directory.join("sentinel").is_file());
            assert_eq!(
                fs::metadata(directory.join(MARKER))
                    .unwrap()
                    .modified()
                    .unwrap(),
                later
            );
            assert_eq!(names(&store.base), [identity]);
        }

        #[test]
        fn concurrent_installs_converge_on_one_valid_directory() {
            let fixture = fixture("codex-shim-race");
            let (store, executable) = (&fixture.store, &fixture.executable);
            let now = now();
            let barrier = std::sync::Barrier::new(8);
            let results: Vec<_> = std::thread::scope(|scope| {
                let installs: Vec<_> = (0..8)
                    .map(|_| {
                        scope.spawn(|| {
                            barrier.wait();
                            let installed = store.install(executable, now).unwrap();
                            let leased = installed.lease.is_some();
                            (installed.directory, installed.identity, leased)
                        })
                    })
                    .collect();
                installs
                    .into_iter()
                    .map(|install| install.join().unwrap())
                    .collect()
            });
            let (directory, identity, leased) = &results[0];
            assert!(
                results.iter().all(|result| result == &results[0]),
                "{results:?}"
            );
            assert!(leased);
            assert!(Source::inspect(executable).unwrap().matches(directory));
            assert_eq!(names(&store.base), [identity.as_str()]);
        }

        #[test]
        fn a_losing_installer_discards_its_copy_and_adopts_the_winner() {
            let fixture = fixture("codex-shim-loser");
            let store = &fixture.store;
            let now = now();
            let source = Source::inspect(&fixture.executable).unwrap();
            ensure_private_dir(&store.base).unwrap();
            let staged = super::super::create_process_shim(&store.base, "", &source.path).unwrap();
            let winner = store.install(&fixture.executable, now).unwrap().directory;
            fs::write(winner.join("sentinel"), b"").unwrap();

            assert_eq!(publish(&source, staged.clone(), &winner, now), winner);
            assert!(!staged.exists());
            assert!(winner.join("sentinel").is_file());
            assert_eq!(names(&store.base), [source.name]);
        }

        #[test]
        fn damaged_identity_directories_are_never_reused() {
            let fixture = fixture("codex-shim-damaged");
            let store = &fixture.store;
            let now = now();
            let source = Source::inspect(&fixture.executable).unwrap();
            let identity = store.base.join(&source.name);
            // Our marker beside a missing, truncated or foreign launcher, or an emptied
            // directory: rebuilt in place. Install leases the shim before checking it, so this
            // also shows that it releases its own lease, which would block the removal.
            let damages = [
                ("missing launcher", remove_launcher as fn(&Path)),
                ("foreign launcher", replace_launcher),
                ("empty directory", empty_directory),
            ];
            for (damage, apply) in damages {
                assert_eq!(
                    store.install(&fixture.executable, now).unwrap().directory,
                    identity
                );
                apply(&identity);
                assert!(!source.matches(&identity), "{damage}");
                let repaired = store.install(&fixture.executable, now).unwrap();
                assert_eq!(repaired.directory, identity, "{damage}");
                assert!(repaired.lease.is_some(), "{damage}");
                assert!(source.matches(&identity), "{damage}");
            }

            // Without our marker, not a directory, or a link (even to a valid shim): not
            // provably ours, so it stays, and a complete per-process shim serves instead.
            fs::remove_dir_all(&identity).unwrap();
            fs::create_dir(&identity).unwrap();
            fs::write(identity.join(LAUNCHER), b"foreign").unwrap();
            let unmarked = store.install(&fixture.executable, now).unwrap().directory;
            assert_eq!(fs::read(identity.join(LAUNCHER)).unwrap(), b"foreign");
            assert!(!identity.join(MARKER).exists());
            fs::remove_dir_all(&identity).unwrap();
            fs::write(&identity, b"foreign").unwrap();
            let file = store.install(&fixture.executable, now).unwrap().directory;
            assert_eq!(fs::read(&identity).unwrap(), b"foreign");
            fs::remove_file(&identity).unwrap();
            let elsewhere =
                super::super::create_process_shim(&fixture.root, "elsewhere-", &source.path)
                    .unwrap();
            link_dir(&identity, &elsewhere);
            let link = store.install(&fixture.executable, now).unwrap().directory;
            assert!(fs::symlink_metadata(&identity).is_ok_and(|metadata| !metadata.is_dir()));
            assert!(source.matches(&elsewhere));
            for fallback in [&unmarked, &file, &link] {
                assert_eq!(fallback.parent(), Some(store.base.as_path()));
                let name = fallback.file_name().and_then(|name| name.to_str()).unwrap();
                assert_eq!(process_entry(name, ""), Some(std::process::id()));
                assert!(source.matches(fallback), "{}", fallback.display());
            }
        }

        #[test]
        fn sweep_removes_only_stale_unused_shims_named_and_marked_as_ours() {
            let fixture = fixture("codex-shim-sweep");
            let store = &fixture.store;
            let (base, legacy) = (&store.base, store.legacy_root.clone().unwrap());
            let now = now();
            let (stale, fresh) = (2 * HOUR, Duration::from_secs(60));
            let removed = [
                base.join(OTHER),
                base.join(format!("{DEAD}-0")),
                legacy.join(format!("herdr-codex-{DEAD}-0")),
            ];
            let kept = [
                // This process's identity, and a shim that was used recently.
                (base.join(KEEP), stale),
                (base.join("00000000000000000000000000000001"), fresh),
                // A live owner, or a recent use.
                (base.join(format!("{LIVE}-0")), stale),
                (base.join(format!("{DEAD}-1")), fresh),
                (legacy.join(format!("herdr-codex-{LIVE}-0")), stale),
                (legacy.join(format!("herdr-codex-{DEAD}-1")), fresh),
                // Not named exactly as this module names directories in that place.
                (base.join("0123456789ABCDEF0123456789ABCDEF"), stale),
                (base.join("0123456789abcdef0123456789abcde"), stale),
                (base.join(format!("{DEAD}-x")), stale),
                (base.join(format!("0{DEAD}-0")), stale),
                (base.join("0-0"), stale),
                (base.join(format!("herdr-codex-{DEAD}-0")), stale),
                (legacy.join(format!("herdr-codex-{DEAD}-01")), stale),
                (
                    legacy.join(format!("herdr-codex-launch-test-{DEAD}-0")),
                    stale,
                ),
                (legacy.join(OTHER), stale),
                (legacy.join(format!("{DEAD}-0")), stale),
            ];
            for directory in &removed {
                plant(directory, now, stale);
            }
            for (directory, age) in &kept {
                plant(directory, now, *age);
            }
            // Named as ours, but without the marker, not a directory, or a link.
            let unmarked = [
                base.join("ffffffffffffffffffffffffffffffff"),
                base.join(format!("{DEAD}-2")),
                legacy.join(format!("herdr-codex-{DEAD}-2")),
            ];
            for directory in &unmarked {
                fs::create_dir_all(directory).unwrap();
                fs::write(directory.join(LAUNCHER), b"launcher").unwrap();
            }
            let file = base.join("eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee");
            fs::write(&file, b"").unwrap();
            let outside = fixture.root.join("outside");
            plant(&outside, now, stale);
            let link = base.join("dddddddddddddddddddddddddddddddd");
            link_dir(&link, &outside);

            assert_eq!(store.sweep(KEEP, now), removed.len());
            for directory in &removed {
                assert!(!directory.exists(), "{}", directory.display());
            }
            let kept = kept.iter().map(|(directory, _)| directory);
            for directory in kept.chain(&unmarked) {
                assert!(
                    directory.join(LAUNCHER).is_file(),
                    "{}",
                    directory.display()
                );
            }
            assert!(file.is_file());
            assert!(fs::symlink_metadata(&link).is_ok());
            assert!(outside.join(LAUNCHER).is_file() && outside.join(MARKER).is_file());
            assert_eq!(store.sweep(KEEP, now), 0);
        }

        #[test]
        fn one_sweep_removes_a_bounded_number_of_shims() {
            let fixture = fixture("codex-shim-bounded");
            let store = &fixture.store;
            let now = now();
            for attempt in 0..SWEEP_LIMIT + 5 {
                plant(&store.base.join(format!("{DEAD}-{attempt}")), now, 2 * HOUR);
            }
            assert_eq!(store.sweep(KEEP, now), SWEEP_LIMIT);
            assert_eq!(store.sweep(KEEP, now), 5);
            assert!(names(&store.base).is_empty());
        }

        /// A lease (a live process that may still hand the shim to panes) or a running
        /// launcher keeps a stale shim intact and marked until released; a damaged shim that
        /// is still leased is bypassed instead of rebuilt.
        #[cfg(windows)]
        #[test]
        fn leased_or_running_shims_stay_intact_until_released() {
            use std::process::{Command, Stdio};

            let fixture = fixture("codex-shim-busy");
            let store = &fixture.store;
            let now = now();
            let leased_shim = store.base.join(OTHER);
            plant(&leased_shim, now, 2 * HOUR);
            let other_lease = Lease::take(&leased_shim).unwrap();
            let running = store.base.join(format!("{DEAD}-0"));
            plant(&running, now, 2 * HOUR);
            fs::remove_file(running.join(LAUNCHER)).unwrap();
            let system = std::env::var_os("SystemRoot")
                .map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
            fs::copy(
                system.join("System32").join("PING.EXE"),
                running.join(LAUNCHER),
            )
            .unwrap();
            let mut child = Command::new(running.join(LAUNCHER))
                .args(["-n", "30", "127.0.0.1"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();

            let source = Source::inspect(&fixture.executable).unwrap();
            let identity = store.base.join(&source.name);
            assert_eq!(
                store.install(&fixture.executable, now).unwrap().directory,
                identity
            );
            remove_launcher(&identity);
            let identity_lease = Lease::take(&identity).unwrap();
            let bypass = store.install(&fixture.executable, now).unwrap().directory;
            assert_ne!(bypass, identity);
            assert!(source.matches(&bypass));
            assert!(identity.join(MARKER).is_file());

            assert_eq!(store.sweep(&source.name, now), 0);
            for shim in [&leased_shim, &running] {
                assert!(
                    shim.join(LAUNCHER).is_file() && shim.join(MARKER).is_file(),
                    "{}",
                    shim.display()
                );
            }

            drop(other_lease);
            drop(identity_lease);
            child.kill().unwrap();
            child.wait().unwrap();
            assert_eq!(
                store.install(&fixture.executable, now).unwrap().directory,
                identity
            );
            assert!(source.matches(&identity));
            // The image of an exited process can stay mapped for a moment.
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            let mut removed = store.sweep(&source.name, now);
            while removed < 2 {
                assert!(std::time::Instant::now() < deadline, "removed {removed}");
                std::thread::sleep(Duration::from_millis(50));
                removed += store.sweep(&source.name, now);
            }
            assert!(!leased_shim.exists() && !running.exists());
        }

        /// The protocol rests on these share modes: leases coexist, a claim and a lease exclude
        /// each other whichever comes first, and the claimer can still delete the marker.
        #[cfg(windows)]
        #[test]
        fn leases_coexist_and_exclude_claims_either_way() {
            let fixture = fixture("codex-shim-share-modes");
            let shim = fixture.store.base.join(OTHER);
            plant(&shim, now(), Duration::ZERO);
            let leases = [Lease::take(&shim).unwrap(), Lease::take(&shim).unwrap()];
            assert!(Claim::take(&shim).is_err());
            drop(leases);
            let claim = Claim::take(&shim).unwrap();
            assert!(Lease::take(&shim).is_err());
            remove_file_if_present(&shim.join(MARKER)).unwrap();
            drop(claim);
            assert!(!shim.join(MARKER).exists());
        }

        /// Install leases the shim before validating it, so a sweep elsewhere that comes after
        /// the lease cannot remove the shim this process hands out, however stale its marker.
        #[cfg(windows)]
        #[test]
        fn sweeps_cannot_remove_a_shim_that_install_leased() {
            let fixture = fixture("codex-shim-leased");
            let store = &fixture.store;
            let now = now();
            let installed = store.install(&fixture.executable, now).unwrap();
            let directory = installed.directory.clone();
            // As if last reused long ago by a process that still runs.
            set_modified(&directory.join(MARKER), now - 2 * HOUR);
            // A sweep in a process that runs another executable.
            assert_eq!(store.sweep(KEEP, now), 0);
            assert!(Source::inspect(&fixture.executable)
                .unwrap()
                .matches(&directory));

            drop(installed);
            assert_eq!(store.sweep(KEEP, now), 1);
            assert!(!directory.exists());
        }

        /// A sweep elsewhere that claimed the shim before this process could lease it is about
        /// to remove it: install must not hand it out, and uses a per-process shim meanwhile.
        #[cfg(windows)]
        #[test]
        fn install_never_hands_out_a_shim_that_a_sweep_has_claimed() {
            let fixture = fixture("codex-shim-claimed");
            let store = &fixture.store;
            let now = now();
            let source = Source::inspect(&fixture.executable).unwrap();
            let identity = store.install(&fixture.executable, now).unwrap().directory;
            let claim = Claim::take(&identity).unwrap();

            let installed = store.install(&fixture.executable, now).unwrap();
            assert_ne!(installed.directory, identity);
            assert!(installed.lease.is_some());
            assert!(source.matches(&installed.directory));
            let name = installed
                .directory
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap();
            assert_eq!(process_entry(name, ""), Some(std::process::id()));

            // Once the sweep has removed it, the next install rebuilds the shared shim.
            drop(claim);
            remove_shim(&identity).unwrap();
            assert_eq!(
                store.install(&fixture.executable, now).unwrap().directory,
                identity
            );
            assert!(source.matches(&identity));
        }

        #[cfg(windows)]
        #[test]
        fn native_tools_share_the_running_shim_lease_and_are_swept_with_it() {
            let fixture = fixture("codex-tools-lifecycle");
            let now = now();
            let installed = fixture.store.install(&fixture.executable, now).unwrap();
            let shim = installed.directory.clone();
            let tools = shim.join("native-tools");
            assert_eq!(names(&tools), ["tar.cmd"]);
            assert!(super::super::native_tools::matches(&shim));
            let (running_directory, lease) =
                lease_running_shim(&shim.join(LAUNCHER)).unwrap().unwrap();
            assert_eq!(running_directory, shim);
            assert_eq!(
                names(&fixture.store.base),
                std::slice::from_ref(&installed.identity)
            );
            drop(installed);
            set_modified(&shim.join(MARKER), now - 2 * HOUR);
            assert_eq!(fixture.store.sweep(KEEP, now), 0);
            assert!(tools.join("tar.cmd").is_file());
            drop(lease);
            assert_eq!(fixture.store.sweep(KEEP, now), 1);
            assert!(!tools.exists());
        }

        #[cfg(windows)]
        #[test]
        fn damaged_native_tools_are_rebuilt_or_bypassed_under_an_existing_lease() {
            let fixture = fixture("codex-tools-repair");
            let now = now();
            let installed = fixture.store.install(&fixture.executable, now).unwrap();
            let shim = installed.directory.clone();
            fs::write(shim.join("native-tools/tar.cmd"), b"damaged").unwrap();
            let bypass = fixture.store.install(&fixture.executable, now).unwrap();
            assert_ne!(bypass.directory, shim);
            assert!(super::super::native_tools::matches(&bypass.directory));
            assert!(lease_running_shim(&shim.join(LAUNCHER)).is_err());
            drop(installed);
            let repaired = fixture.store.install(&fixture.executable, now).unwrap();
            assert_eq!(repaired.directory, shim);
            assert!(super::super::native_tools::matches(&shim));
        }

        #[test]
        fn test_processes_share_their_own_short_lived_base() {
            let store = Store::for_process();
            assert_eq!(
                store.base,
                std::env::temp_dir().join("herdr-unit-codex-shim")
            );
            assert_eq!(store.legacy_root, None);
            assert_eq!(store.stale_age, HOUR);
            assert!((store.alive)(std::process::id()));
        }
    }
}
