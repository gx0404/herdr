//! Where agent CLIs are looked up on Windows.
//!
//! A long-running server keeps the `PATH` it started with, so CLIs installed later, installers
//! that only update the registry `PATH`, and CLIs bundled with desktop apps (Codex, Claude
//! Code) stay invisible to a plain `PATH` walk. The search keeps the process `PATH` first,
//! then adds the current registry `PATH` and well-known per-user install locations. File
//! names follow `PATHEXT` order, restricted to types `CreateProcess` can start, with `.ps1`
//! shims last.

use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use windows_sys::Win32::Foundation::{ERROR_MORE_DATA, ERROR_SUCCESS};
use windows_sys::Win32::System::Registry::{
    RegGetValueW, HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_NOEXPAND, RRF_RT_REG_EXPAND_SZ,
    RRF_RT_REG_SZ,
};

#[cfg(test)]
mod tests;

const DEFAULT_PATHEXT: &str = ".COM;.EXE;.BAT;.CMD;.VBS;.VBE;.JS;.JSE;.WSF;.WSH;.MSC";
/// `PATHEXT` entries a CLI shim can use (batch files run through `cmd.exe`); script-host
/// types such as `.vbs` or `.js` are never agent CLI entry points.
const LAUNCHABLE_EXTENSIONS: [&str; 4] = [".com", ".exe", ".bat", ".cmd"];
/// Tried even when a customised `PATHEXT` omits them, so no historical match is lost.
const REQUIRED_EXTENSIONS: [&str; 3] = [".exe", ".cmd", ".bat"];
/// npm and pnpm also write PowerShell shims; they only win when nothing else exists.
const POWERSHELL_EXTENSION: &str = ".ps1";

const USER_ENVIRONMENT_KEY: &str = "Environment";
const MACHINE_ENVIRONMENT_KEY: &str =
    r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment";
/// Nesting limit for `%VARIABLE%` values that reference other registry variables.
const EXPANSION_DEPTH: u8 = 4;
/// Package family name of the Claude desktop app's MSIX package, matched exactly so look-alike
/// package folders never contribute a `claude.exe`; its virtualised roaming folder holds the
/// bundled Claude Code builds.
const CLAUDE_MSIX_PACKAGE_FAMILY: &str = "Claude_pzs8sxrjxfjjc";
/// PowerShell shims run through Windows PowerShell, non-interactively and with the
/// `-ExecutionPolicy Bypass` herdr's own hook scripts use (`integration::command`): the default
/// client policy refuses every script, while a policy set by Group Policy still wins over the
/// flag.
const POWERSHELL_SCRIPT_HOST: &str = "powershell.exe";
const POWERSHELL_SCRIPT_ARGS: [&str; 6] = [
    "-NoLogo",
    "-NoProfile",
    "-NonInteractive",
    "-ExecutionPolicy",
    "Bypass",
    "-File",
];

/// Inputs of the search. Production reads them from the process environment and the registry
/// ([`CommandSearchEnvironment::from_system`]); tests inject temporary directories.
#[derive(Debug, Clone, Default)]
pub(crate) struct CommandSearchEnvironment {
    pub(crate) process_path: Option<OsString>,
    /// Current registry `Path` values, machine first then user (the order Windows composes a
    /// logon `PATH` in), with `%VARIABLE%` references expanded.
    pub(crate) registry_path: Vec<OsString>,
    /// Tests pin `PATHEXT` together with the rest; production reads it on every lookup.
    #[cfg(test)]
    pub(crate) path_ext: Option<OsString>,
    pub(crate) user_profile: Option<PathBuf>,
    pub(crate) app_data: Option<PathBuf>,
    pub(crate) local_app_data: Option<PathBuf>,
    pub(crate) pnpm_home: Option<PathBuf>,
    pub(crate) bun_install: Option<PathBuf>,
    pub(crate) volta_home: Option<PathBuf>,
    pub(crate) scoop: Option<PathBuf>,
}

impl CommandSearchEnvironment {
    /// Process variables win; variables created after this process started (for example
    /// `PNPM_HOME` written by an installer) are read from the registry.
    pub(crate) fn from_system() -> Self {
        let directory = |name: &str| {
            system_variable(OsStr::new(name), 0)
                .filter(|value| !value.is_empty())
                .map(|value| PathBuf::from(OsString::from_wide(&value)))
        };
        let registry_path = [
            (HKEY_LOCAL_MACHINE, MACHINE_ENVIRONMENT_KEY),
            (HKEY_CURRENT_USER, USER_ENVIRONMENT_KEY),
        ]
        .into_iter()
        .filter_map(|(root, key)| registry_string(root, key, OsStr::new("Path")))
        .map(|raw| expand_environment_strings(&raw, &mut |name| system_variable(name, 1)))
        .collect();
        Self {
            process_path: std::env::var_os("PATH"),
            registry_path,
            #[cfg(test)]
            path_ext: std::env::var_os("PATHEXT"),
            user_profile: directory("USERPROFILE"),
            app_data: directory("APPDATA"),
            local_app_data: directory("LOCALAPPDATA"),
            pnpm_home: directory("PNPM_HOME"),
            bun_install: directory("BUN_INSTALL"),
            volta_home: directory("VOLTA_HOME"),
            scoop: directory("SCOOP"),
        }
    }
}

#[cfg(test)]
thread_local! {
    static TEST_ENVIRONMENT: std::cell::RefCell<Option<CommandSearchEnvironment>> =
        const { std::cell::RefCell::new(None) };
}

/// Pins the search inputs for the calling test thread; `None` restores the hermetic default
/// (process `PATH` only, no registry and no per-user locations of the machine running tests).
#[cfg(test)]
pub(crate) fn set_test_command_search_environment(environment: Option<CommandSearchEnvironment>) {
    TEST_ENVIRONMENT.with(|slot| *slot.borrow_mut() = environment);
}

fn current_environment() -> CommandSearchEnvironment {
    #[cfg(test)]
    {
        TEST_ENVIRONMENT
            .with(|slot| slot.borrow().clone())
            .unwrap_or_else(|| CommandSearchEnvironment {
                process_path: std::env::var_os("PATH"),
                path_ext: std::env::var_os("PATHEXT"),
                ..CommandSearchEnvironment::default()
            })
    }
    #[cfg(not(test))]
    {
        CommandSearchEnvironment::from_system()
    }
}

fn current_path_ext() -> Option<OsString> {
    #[cfg(test)]
    if let Some(path_ext) = TEST_ENVIRONMENT.with(|slot| {
        slot.borrow()
            .as_ref()
            .map(|environment| environment.path_ext.clone())
    }) {
        return path_ext;
    }
    std::env::var_os("PATHEXT")
}

pub(crate) fn command_search_dirs_platform(command: &str) -> Vec<PathBuf> {
    command_search_dirs_in(&current_environment(), command)
}

pub(crate) fn command_file_candidates_platform(dir: &Path, command: &str) -> Vec<PathBuf> {
    command_file_candidates_in(dir, command, current_path_ext().as_deref())
}

/// Availability only, after every launchable candidate failed: an extensionless file (a POSIX
/// shell shim from npm or a Unix-style installer) still shows the CLI is installed, but
/// `CreateProcess` cannot start it, so it is never used for execution.
pub(crate) fn command_availability_fallback_platform(dir: &Path, command: &str) -> Option<PathBuf> {
    Path::new(command)
        .extension()
        .is_none()
        .then(|| dir.join(command))
}

/// How to start a resolved CLI: PowerShell shims through `powershell.exe -File`, everything
/// else directly.
pub(crate) fn cli_invocation_platform(executable: &Path) -> (OsString, Vec<OsString>) {
    let powershell_script = executable
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case(&POWERSHELL_EXTENSION[1..]));
    if !powershell_script {
        return (executable.as_os_str().to_os_string(), Vec::new());
    }
    let mut args = POWERSHELL_SCRIPT_ARGS
        .iter()
        .map(OsString::from)
        .collect::<Vec<_>>();
    args.push(executable.as_os_str().to_os_string());
    (OsString::from(POWERSHELL_SCRIPT_HOST), args)
}

pub(crate) fn cli_child_path_platform() -> Option<OsString> {
    child_path_in(&current_environment())
}

/// The process `PATH` followed by the current registry `PATH` entries it lacks, so a CLI the
/// server starts finds interpreters installed after the server started (npm `.cmd` shims run
/// `node`). `None` when nothing is missing and the inherited `PATH` can stay.
pub(crate) fn child_path_in(environment: &CommandSearchEnvironment) -> Option<OsString> {
    let process_entries = environment
        .process_path
        .as_deref()
        .map(|path| std::env::split_paths(path).collect::<Vec<_>>())
        .unwrap_or_default();
    let mut seen = process_entries
        .iter()
        .map(|dir| path_key(dir))
        .collect::<std::collections::HashSet<_>>();
    let missing = environment
        .registry_path
        .iter()
        .flat_map(std::env::split_paths)
        .filter(|dir| dir.is_absolute() && seen.insert(path_key(dir)))
        .collect::<Vec<_>>();
    if missing.is_empty() {
        return None;
    }
    std::env::join_paths(process_entries.into_iter().chain(missing)).ok()
}

/// Case-insensitive identity of a directory spelling, ignoring trailing separators.
fn path_key(dir: &Path) -> String {
    dir.to_string_lossy()
        .trim_end_matches(['\\', '/'])
        .to_lowercase()
}

/// Search order: process `PATH`, registry `PATH`, existing per-user install locations, then
/// the newest CLI bundled with a desktop app. Duplicates (case-insensitive) and relative
/// entries are dropped.
pub(crate) fn command_search_dirs_in(
    environment: &CommandSearchEnvironment,
    command: &str,
) -> Vec<PathBuf> {
    let mut dirs = SearchDirs::default();
    for path in environment
        .process_path
        .iter()
        .chain(environment.registry_path.iter())
    {
        for dir in std::env::split_paths(path) {
            dirs.push(dir);
        }
    }
    for dir in known_install_dirs(environment) {
        if dir.is_dir() {
            dirs.push(dir);
        }
    }
    for dir in bundled_command_dirs(environment, command) {
        dirs.push(dir);
    }
    dirs.entries
}

#[derive(Default)]
struct SearchDirs {
    entries: Vec<PathBuf>,
    seen: std::collections::HashSet<String>,
}

impl SearchDirs {
    fn push(&mut self, dir: PathBuf) {
        if dir.is_absolute() && self.seen.insert(path_key(&dir)) {
            self.entries.push(dir);
        }
    }
}

fn known_install_dirs(environment: &CommandSearchEnvironment) -> Vec<PathBuf> {
    let profile = environment.user_profile.as_deref();
    let local = environment.local_app_data.as_deref();
    let rooted =
        |configured: &Option<PathBuf>, fallback: Option<PathBuf>| configured.clone().or(fallback);
    let mut dirs = Vec::new();
    if let Some(profile) = profile {
        dirs.push(profile.join(".local").join("bin"));
        dirs.push(profile.join(".kimi-code").join("bin"));
    }
    if let Some(app_data) = &environment.app_data {
        dirs.push(app_data.join("npm"));
    }
    dirs.extend(rooted(
        &environment.pnpm_home,
        local.map(|local| local.join("pnpm")),
    ));
    dirs.extend(
        rooted(
            &environment.bun_install,
            profile.map(|profile| profile.join(".bun")),
        )
        .map(|root| root.join("bin")),
    );
    dirs.extend(
        rooted(
            &environment.volta_home,
            local.map(|local| local.join("Volta")),
        )
        .map(|root| root.join("bin")),
    );
    dirs.extend(
        rooted(
            &environment.scoop,
            profile.map(|profile| profile.join("scoop")),
        )
        .map(|root| root.join("shims")),
    );
    if let Some(local) = local {
        dirs.push(local.join("Microsoft").join("WinGet").join("Links"));
    }
    dirs
}

/// CLIs shipped inside desktop apps: the Codex app unpacks `codex.exe` into a per-build
/// directory under `%LOCALAPPDATA%\OpenAI\Codex\bin`, the Claude app keeps versioned Claude
/// Code builds under `%APPDATA%\Claude\claude-code` (the MSIX package redirects that folder
/// into its `LocalCache`).
fn bundled_command_dirs(environment: &CommandSearchEnvironment, command: &str) -> Vec<PathBuf> {
    if command.eq_ignore_ascii_case("codex") {
        return environment
            .local_app_data
            .as_ref()
            .and_then(|local| {
                newest_modified_dir_with(
                    &local.join("OpenAI").join("Codex").join("bin"),
                    "codex.exe",
                )
            })
            .into_iter()
            .collect();
    }
    if command.eq_ignore_ascii_case("claude") {
        let mut roots = Vec::new();
        if let Some(app_data) = &environment.app_data {
            roots.push(app_data.join("Claude").join("claude-code"));
        }
        if let Some(local) = &environment.local_app_data {
            roots.push(
                local
                    .join("Packages")
                    .join(CLAUDE_MSIX_PACKAGE_FAMILY)
                    .join("LocalCache")
                    .join("Roaming")
                    .join("Claude")
                    .join("claude-code"),
            );
        }
        return roots
            .iter()
            .filter_map(|root| newest_version_dir_with(root, "claude.exe"))
            .max_by(|left, right| left.0.cmp(&right.0))
            .map(|(_, dir)| dir)
            .into_iter()
            .collect();
    }
    Vec::new()
}

fn subdirs_with(root: &Path, executable: &str) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|dir| dir.join(executable).is_file())
        .collect()
}

/// Build directories have opaque names; the most recently written executable is the newest.
fn newest_modified_dir_with(root: &Path, executable: &str) -> Option<PathBuf> {
    subdirs_with(root, executable)
        .into_iter()
        .map(|dir| {
            let modified = std::fs::metadata(dir.join(executable))
                .and_then(|metadata| metadata.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            (modified, dir)
        })
        .max()
        .map(|(_, dir)| dir)
}

/// Version directories compare numerically (`2.1.100` is newer than `2.1.99`).
fn newest_version_dir_with(root: &Path, executable: &str) -> Option<(Vec<u64>, PathBuf)> {
    subdirs_with(root, executable)
        .into_iter()
        .map(|dir| {
            let version = dir
                .file_name()
                .map(|name| version_sort_key(&name.to_string_lossy()))
                .unwrap_or_default();
            (version, dir)
        })
        .max()
}

fn version_sort_key(name: &str) -> Vec<u64> {
    name.split(|ch: char| !ch.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse().ok())
        .collect()
}

pub(crate) fn command_file_candidates_in(
    dir: &Path,
    command: &str,
    path_ext: Option<&OsStr>,
) -> Vec<PathBuf> {
    if Path::new(command).extension().is_some() {
        return vec![dir.join(command)];
    }
    executable_extensions(path_ext)
        .into_iter()
        .map(|extension| dir.join(format!("{command}{extension}")))
        .collect()
}

/// `PATHEXT` order limited to launchable types, the historical `.exe`/`.cmd`/`.bat` set when
/// `PATHEXT` omits it, and `.ps1` last.
pub(crate) fn executable_extensions(path_ext: Option<&OsStr>) -> Vec<String> {
    let listed = path_ext
        .and_then(OsStr::to_str)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(DEFAULT_PATHEXT);
    let mut extensions: Vec<String> = Vec::new();
    for extension in listed.split(';') {
        let extension = extension
            .trim()
            .trim_start_matches('.')
            .to_ascii_lowercase();
        if extension.is_empty() {
            continue;
        }
        let extension = format!(".{extension}");
        if LAUNCHABLE_EXTENSIONS.contains(&extension.as_str()) && !extensions.contains(&extension) {
            extensions.push(extension);
        }
    }
    for extension in REQUIRED_EXTENSIONS {
        if !extensions.iter().any(|listed| listed == extension) {
            extensions.push(extension.to_string());
        }
    }
    extensions.push(POWERSHELL_EXTENSION.to_string());
    extensions
}

/// A variable for `%NAME%` expansion: the process environment first, then the current user
/// and machine environment keys (their `REG_EXPAND_SZ` values expanded in turn).
fn system_variable(name: &OsStr, depth: u8) -> Option<Vec<u16>> {
    if let Some(value) = std::env::var_os(name) {
        return Some(value.encode_wide().collect());
    }
    if depth >= EXPANSION_DEPTH {
        return None;
    }
    [
        (HKEY_CURRENT_USER, USER_ENVIRONMENT_KEY),
        (HKEY_LOCAL_MACHINE, MACHINE_ENVIRONMENT_KEY),
    ]
    .into_iter()
    .find_map(|(root, key)| registry_string(root, key, name))
    .map(|raw| {
        expand_environment_strings(&raw, &mut |inner| system_variable(inner, depth + 1))
            .encode_wide()
            .collect()
    })
}

/// `ExpandEnvironmentStrings` semantics with an injectable lookup: `%NAME%` is replaced by its
/// value; for an unknown name only the opening `%` is kept literally and scanning resumes right
/// after it, so its closing `%` may open the next reference.
pub(crate) fn expand_environment_strings(
    raw: &[u16],
    lookup: &mut dyn FnMut(&OsStr) -> Option<Vec<u16>>,
) -> OsString {
    const PERCENT: u16 = b'%' as u16;
    let mut expanded = Vec::with_capacity(raw.len());
    let mut index = 0;
    while let Some(&unit) = raw.get(index) {
        if unit == PERCENT {
            let rest = &raw[index + 1..];
            if let Some(end) = rest.iter().position(|&unit| unit == PERCENT) {
                let name = &rest[..end];
                let valid =
                    !name.is_empty() && !name.iter().any(|&unit| unit == 0 || unit == b'=' as u16);
                if let Some(value) = valid.then(|| lookup(&OsString::from_wide(name))).flatten() {
                    expanded.extend_from_slice(&value);
                    index += end + 2;
                    continue;
                }
            }
        }
        expanded.push(unit);
        index += 1;
    }
    OsString::from_wide(&expanded)
}

/// A `REG_SZ`/`REG_EXPAND_SZ` value, unexpanded, without its terminating NULs.
fn registry_string(root: HKEY, key: &str, value: &OsStr) -> Option<Vec<u16>> {
    let key = super::wide_null(key);
    let value = value
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let flags = RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | RRF_NOEXPAND;
    let mut size = 0_u32;
    // SAFETY: both names are NUL-terminated; a null data pointer only asks for the size.
    let status = unsafe {
        RegGetValueW(
            root,
            key.as_ptr(),
            value.as_ptr(),
            flags,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size,
        )
    };
    if status != ERROR_SUCCESS || size == 0 {
        return None;
    }
    // The value can grow between the size probe and the read.
    for _ in 0..3 {
        let mut buffer = vec![0_u16; (size as usize).div_ceil(2) + 1];
        let mut bytes = u32::try_from(buffer.len() * 2).ok()?;
        // SAFETY: `buffer` holds `bytes` writable bytes for the duration of the call.
        let status = unsafe {
            RegGetValueW(
                root,
                key.as_ptr(),
                value.as_ptr(),
                flags,
                std::ptr::null_mut(),
                buffer.as_mut_ptr().cast(),
                &mut bytes,
            )
        };
        match status {
            ERROR_SUCCESS => {
                buffer.truncate((bytes as usize / 2).min(buffer.len()));
                while buffer.last() == Some(&0) {
                    buffer.pop();
                }
                return Some(buffer);
            }
            ERROR_MORE_DATA => size = bytes,
            _ => return None,
        }
    }
    None
}
