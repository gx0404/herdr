//! Pane-local Codex launch policy. Resolve the pane's PATH at invocation time;
//! only a successful bounded capability probe can add `--no-daemon`.

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

pub(crate) const ENTRY: &str = "--internal-codex-launch";
const SHIM_DIR: &str = "HERDR_CODEX_SHIM_DIR";
const ACTIVE: &str = "HERDR_CODEX_LAUNCH_ACTIVE";
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const OUTPUT_LIMIT: usize = 64 * 1024;

/// Dispatch before the ordinary UTF-8 CLI parser, preserving native argv.
pub(crate) fn dispatch(argv: &[OsString]) -> Option<io::Result<()>> {
    let shim = argv.first().and_then(|arg| Path::new(arg).file_stem()) == Some(OsStr::new("codex"));
    let explicit = argv.get(1).is_some_and(|arg| arg == ENTRY);
    (shim || explicit).then(|| launch(&argv[if shim { 1 } else { 2 }..]))
}

fn in_pane() -> bool {
    std::env::var_os(crate::HERDR_ENV_VAR).as_deref() == Some(OsStr::new(crate::HERDR_ENV_VALUE))
        && std::env::var_os(super::HERDR_PANE_ID_ENV_VAR).is_some_and(|id| !id.is_empty())
}

fn launch(args: &[OsString]) -> io::Result<()> {
    if std::env::var_os(ACTIVE).is_some() {
        return Err(io::Error::other(
            "recursive Codex launcher invocation; check PATH wrappers",
        ));
    }
    let executable = resolve_executable(
        std::env::var_os("PATH").as_deref().unwrap_or_default(),
        std::env::var_os(SHIM_DIR).as_deref().map(Path::new),
        &std::env::current_exe()?,
    )?;
    let mut args = args.to_vec();
    if in_pane() && interactive(&args) {
        match supports_no_daemon(&executable, PROBE_TIMEOUT, OUTPUT_LIMIT) {
            Ok(true) => args.insert(0, "--no-daemon".into()),
            Ok(false) => {}
            Err(reason) => eprintln!(
                "herdr: Codex foreground isolation is unknown ({reason}); launching unchanged once"
            ),
        }
    }
    let mut command = crate::platform::codex_launch::command(&executable, &args)?;
    // Wrappers resolving `codex` again must not reenter our shim. Only remove
    // Herdr directories; preserve the order of the pane's real PATH entries.
    command.env(
        "PATH",
        path_without_shims(std::env::var_os("PATH").as_deref().unwrap_or_default())?,
    );
    command.env_remove(ACTIVE);
    crate::platform::codex_launch::run(command)
}

/// Return an execution-only argv; callers retain their original public/persisted plan.
pub(crate) fn managed_argv(argv: &[String]) -> io::Result<Vec<String>> {
    if argv.first().is_none_or(|program| program != "codex") {
        return Ok(argv.to_vec());
    }
    let executable = crate::platform::launch_executable()?;
    let executable = executable.into_os_string().into_string().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Herdr executable path is not UTF-8",
        )
    })?;
    Ok([vec![executable, ENTRY.into()], argv[1..].to_vec()].concat())
}

/// Only new panes get the shim. Shell startup files may subsequently replace PATH.
pub(crate) fn apply_pane_env(command: &mut portable_pty::CommandBuilder) {
    command.env_remove(ACTIVE);
    if command.get_env(crate::HERDR_ENV_VAR) != Some(OsStr::new(crate::HERDR_ENV_VALUE))
        || command
            .get_env(super::HERDR_PANE_ID_ENV_VAR)
            .is_none_or(OsStr::is_empty)
    {
        return;
    }
    static DIRECTORY: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    let directory = DIRECTORY.get_or_init(|| {
        crate::platform::launch_executable()
            .and_then(|executable| crate::platform::codex_launch::install_shim(&executable))
            .map_err(|error| error.to_string())
    });
    let Ok(directory) = directory else {
        tracing::warn!("Codex PATH shim unavailable; managed launches still use the launcher");
        return;
    };
    let path = command.get_env("PATH").unwrap_or_default();
    let paths = std::iter::once(directory.clone()).chain(std::env::split_paths(path));
    match std::env::join_paths(paths) {
        Ok(path) => {
            command.env("PATH", path);
            command.env(SHIM_DIR, directory);
        }
        Err(_) => tracing::warn!("Codex PATH shim unavailable: invalid PATH"),
    }
}

fn resolve_executable(path: &OsStr, shim: Option<&Path>, own: &Path) -> io::Result<PathBuf> {
    let own = own.canonicalize()?;
    let shim = shim.and_then(|path| path.canonicalize().ok());
    for directory in std::env::split_paths(path) {
        if shim
            .as_ref()
            .is_some_and(|shim| directory.canonicalize().ok().as_ref() == Some(shim))
        {
            continue;
        }
        for candidate in super::command_path_candidates(&directory, "codex") {
            if !super::executable_file_exists(&candidate) {
                continue;
            }
            let canonical = candidate.canonicalize()?;
            if canonical == own
                || crate::platform::codex_launch::same_file(&canonical, &own)
                || crate::platform::codex_launch::is_shim(&candidate)
            {
                continue;
            }
            // Keep the selected PATH spelling: npm shims can depend on $0.
            return std::path::absolute(candidate);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "Codex executable not found outside the Herdr shim; check pane PATH",
    ))
}

fn path_without_shims(path: &OsStr) -> io::Result<OsString> {
    std::env::join_paths(
        std::env::split_paths(path)
            .filter(|directory| !crate::platform::codex_launch::is_shim(&directory.join("codex"))),
    )
    .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid pane PATH"))
}

/// Conservative CLI classification: unknown options are passed through. Values
/// of known options and prompt text are never scanned as subcommands or flags.
fn interactive(args: &[OsString]) -> bool {
    let mut index = 0;
    let mut positional = false;
    while let Some(arg) = args.get(index) {
        let Some(arg) = arg.to_str() else {
            positional = true;
            index += 1;
            continue;
        };
        match arg {
            "--" => return true,
            "--remote" | "--no-daemon" | "--help" | "-h" | "--version" | "-V" => return false,
            "-c" | "--config" | "--enable" | "--disable" | "-i" | "--image" | "-m" | "--model"
            | "--local-provider" | "-p" | "--profile" | "-s" | "--sandbox" | "-a"
            | "--ask-for-approval" | "-C" | "--cd" | "--add-dir" => index += 2,
            "--oss"
            | "--full-auto"
            | "--dangerously-bypass-approvals-and-sandbox"
            | "--search"
            | "--no-alt-screen"
            | "--last"
            | "--all" => index += 1,
            "resume" | "fork" if !positional => {
                positional = true;
                index += 1;
            }
            value if value.starts_with("--remote=") || value.starts_with("--no-daemon=") => {
                return false
            }
            value if value.starts_with('-') => {
                let known_assignment = value.split_once('=').is_some_and(|(key, _)| {
                    matches!(
                        key,
                        "--config"
                            | "--enable"
                            | "--disable"
                            | "--image"
                            | "--model"
                            | "--local-provider"
                            | "--profile"
                            | "--sandbox"
                            | "--ask-for-approval"
                            | "--cd"
                            | "--add-dir"
                    )
                });
                if !known_assignment {
                    return false;
                }
                index += 1;
            }
            // rust-v0.157.0 codex-rs/cli/src/main.rs::Subcommand, including
            // hidden/platform commands and aliases. Keep legacy mcp-server and
            // Clap-generated help. Future bare commands cannot be distinguished
            // from prompts by argv alone; update this contract for new releases.
            "agents"
            | "tcp-tunnel"
            | "exec"
            | "e"
            | "review"
            | "login"
            | "logout"
            | "mcp"
            | "plugin"
            | "app-server"
            | "remote-control"
            | "app"
            | "completion"
            | "update"
            | "doctor"
            | "sandbox"
            | "debug"
            | "execpolicy"
            | "apply"
            | "a"
            | "queue"
            | "archive"
            | "delete"
            | "migrate-rollouts"
            | "unarchive"
            | "cloud"
            | "cloud-tasks"
            | "responses-api-proxy"
            | "stdio-to-uds"
            | "exec-server"
            | "features"
            | "mcp-server"
            | "help"
                if !positional =>
            {
                return false
            }
            _ => {
                // Options can follow the positional prompt/session ID. Consume
                // that positional once, then continue checking explicit flags.
                index += 1;
                positional = true;
            }
        }
    }
    true
}

fn supports_no_daemon(
    executable: &Path,
    timeout: Duration,
    limit: usize,
) -> Result<bool, &'static str> {
    let mut command = crate::platform::codex_launch::command(executable, &["--help".into()])
        .map_err(|_| "probe command unavailable")?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env(ACTIVE, "1");
    crate::platform::codex_launch::configure_probe(&mut command);
    let mut child = command.spawn().map_err(|_| "probe could not start")?;
    let mut guard = match crate::platform::UsageProbeGuard::new(&child) {
        Ok(guard) => guard,
        Err(_) => {
            crate::platform::codex_launch::stop_probe(&mut child);
            return Err("probe supervision unavailable");
        }
    };
    let mut stdout = child.stdout.take().ok_or("probe stdout unavailable")?;
    let mut stderr = child.stderr.take().ok_or("probe stderr unavailable")?;
    let start = Instant::now();
    let mut outputs = [Vec::new(), Vec::new()];
    let mut closed = [false; 2];
    let mut buffer = [0; 4096];
    let result = loop {
        let reads = [
            if closed[0] {
                Ok(None)
            } else {
                crate::platform::codex_launch::read_probe_pipe(&mut stdout, &mut buffer)
                    .map(|count| count.map(|count| buffer[..count].to_vec()))
            },
            if closed[1] {
                Ok(None)
            } else {
                crate::platform::codex_launch::read_probe_pipe(&mut stderr, &mut buffer)
                    .map(|count| count.map(|count| buffer[..count].to_vec()))
            },
        ];
        let mut unreadable = false;
        for (index, read) in reads.into_iter().enumerate() {
            match read {
                Ok(Some(bytes)) if bytes.is_empty() => closed[index] = true,
                Ok(Some(bytes)) => outputs[index].extend(bytes),
                Ok(None) => {}
                Err(_) => unreadable = true,
            }
        }
        if unreadable {
            break Err("probe output unreadable");
        }
        if outputs.iter().map(Vec::len).sum::<usize>() > limit {
            break Err("probe output limit exceeded");
        }
        if closed == [true, true] {
            match crate::platform::codex_launch::probe_succeeded(&mut child) {
                Ok(Some(true)) => break Ok(outputs.iter().any(|bytes| help_has_flag(bytes))),
                Ok(Some(_)) => break Err("probe exited unsuccessfully"),
                Ok(None) => {}
                Err(_) => break Err("probe exit status unavailable"),
            }
        }
        if start.elapsed() >= timeout {
            break Err("probe timed out");
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    // Polling owns both read handles: timeout closes them without waiting on
    // inherited writers, and no reader threads can linger after cancellation.
    guard.terminate();
    crate::platform::codex_launch::stop_probe(&mut child);
    result
}

fn help_has_flag(bytes: &[u8]) -> bool {
    bytes
        .split(|byte| byte.is_ascii_whitespace() || matches!(byte, b',' | b'[' | b']'))
        .any(|word| word == b"--no-daemon")
}

#[cfg(test)]
mod tests;
