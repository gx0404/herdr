#![cfg(windows)]

use std::cell::Cell;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read};
use std::mem::size_of;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicAccountingInformation,
    JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
    TerminateJobObject, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::Threading::{
    GetProcessIdOfThread, OpenThread, ResumeThread, CREATE_SUSPENDED,
    THREAD_QUERY_LIMITED_INFORMATION, THREAD_SUSPEND_RESUME,
};

fn success(output: &std::process::Output) {
    if !output.status.success() {
        use windows_sys::Win32::Globalization::{GetACP, GetOEMCP};
        use windows_sys::Win32::System::Console::{GetConsoleCP, GetConsoleOutputCP};

        let code_pages = unsafe { (GetACP(), GetOEMCP(), GetConsoleCP(), GetConsoleOutputCP()) };
        eprintln!("ACP/OEMCP/ConsoleCP/ConsoleOutputCP: {code_pages:?}");
    }
    assert!(
        output.status.success(),
        "status={}\nstdout={}\nstderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

struct Sandbox(PathBuf, Cell<bool>);

impl Sandbox {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "herdr-codex-native-entry-中文 & (x) !-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        let sandbox = Self(root, Cell::new(false));
        for directory in [
            "bin",
            "home",
            "temp",
            "config",
            "state",
            "appdata",
            "localappdata",
            "data",
            "cache",
            "runtime",
            "source 中文",
            "output 中文",
        ] {
            fs::create_dir(sandbox.0.join(directory)).unwrap();
        }
        fs::write(
            sandbox.0.join("bash-env.sh"),
            concat!(
                "export TEMP=\"$HERDR_TEST_TEMP\" TMP=\"$HERDR_TEST_TEMP\"\n",
                "TMPDIR=$(cygpath -u -- \"$HERDR_TEST_TEMP\") || exit 1\n",
                "export TMPDIR\n",
            ),
        )
        .unwrap();
        fs::write(
            sandbox.0.join("source 中文/payload.txt"),
            b"launcher extraction\n",
        )
        .unwrap();
        let system = PathBuf::from(std::env::var_os("SystemRoot").unwrap());
        let source = sandbox.0.join("source 中文");
        let archive = sandbox
            .phase_output(
                "archive-create",
                sandbox
                    .command(&system.join("System32/tar.exe"))
                    .current_dir(&source)
                    .args(["-czf", "fixture.tar.gz", "payload.txt"]),
            )
            .unwrap();
        success(&archive);
        fs::rename(
            source.join("fixture.tar.gz"),
            sandbox.0.join("archive 中文.tar.gz"),
        )
        .unwrap();
        fs::write(sandbox.0.join("bin/codex.cmd"), concat!(
            "@echo off\r\nsetlocal DisableDelayedExpansion\r\n",
            "if \"%~1\"==\"--help\" (echo Options: --no-daemon & exit /b 0)\r\n",
            "set \"HERDR_TEST_ARG1=%~1\"\r\nset \"HERDR_TEST_ARG2=%~2\"\r\nset \"HERDR_TEST_ARG3=%~3\"\r\n",
            "\"%HERDR_TEST_POWERSHELL%\" -NoLogo -NoProfile -NonInteractive -Command \"",
            "[Console]::Error.WriteLine('codex-fixture: powershell-start'); ",
            "$report = @{path=$env:PATH; tar_path=(Get-Command tar).Path; arg1=$env:HERDR_TEST_ARG1; arg2=$env:HERDR_TEST_ARG2; arg3=$env:HERDR_TEST_ARG3; active=$env:HERDR_CODEX_LAUNCH_ACTIVE}; ",
            "[IO.File]::WriteAllText($env:HERDR_TEST_REPORT, ($report | ConvertTo-Json)); ",
            "[Console]::Error.WriteLine('codex-fixture: report-written'); ",
            "tar -xzf $env:HERDR_TEST_ARCHIVE -C $env:HERDR_TEST_OUTPUT; ",
            "[Console]::Error.WriteLine(('codex-fixture: tar-returned exit={0}' -f $LASTEXITCODE)); exit $LASTEXITCODE\"\r\n",
            "exit /b %errorlevel%\r\n"
        )).unwrap();
        sandbox
    }

    fn phase_output(&self, phase: &str, command: &mut Command) -> io::Result<Output> {
        let started = Instant::now();
        let stdout = self.0.join(format!("{phase}.stdout"));
        let stderr = self.0.join(format!("{phase}.stderr"));
        let mut job = None;
        let mut child = None;
        let mut root_status = None;
        let mut active = None;
        eprintln!("phase={phase} started; sandbox={}", self.0.display());
        let result = (|| {
            // Regular files avoid waiting for inherited pipe handles to reach EOF.
            command
                .stdin(Stdio::null())
                .stdout(fs::File::create_new(&stdout)?)
                .stderr(fs::File::create_new(&stderr)?)
                .creation_flags(CREATE_SUSPENDED);
            let job = job.insert(private_job()?);
            let child = child.insert(command.spawn()?);
            if unsafe { AssignProcessToJobObject(job.as_raw_handle(), child.as_raw_handle()) } == 0
            {
                return Err(io::Error::last_os_error());
            }
            resume_child(child)?;
            loop {
                root_status = child.try_wait()?;
                active = Some(active_processes(job)?);
                if let Some(status) = root_status.filter(|_| active == Some(0)) {
                    return Ok(Output {
                        status,
                        stdout: bounded_file(&stdout)?,
                        stderr: bounded_file(&stderr)?,
                    });
                }
                if started.elapsed() >= Duration::from_secs(60) {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "phase exceeded 60s",
                    ));
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        })();
        match result {
            Ok(output) => {
                eprintln!(
                    "phase={phase} elapsed={:?}; root={}; active_processes=0",
                    started.elapsed(),
                    output.status,
                );
                if !output.status.success() {
                    eprintln!("{}", self.phase_diagnostics(&stdout, &stderr));
                }
                Ok(output)
            }
            Err(error) => {
                self.1.set(true);
                let cleanup = match (child.as_mut(), job.as_ref()) {
                    (Some(child), Some(job)) => stop_phase(child, job),
                    _ => Ok(()),
                };
                // Closing the non-inherited job is also the kill-tree fallback.
                drop(job);
                Err(io::Error::new(error.kind(), format!(
                    "phase={phase} elapsed={:?}; root={root_status:?}; active_processes={active:?}; error={error}; cleanup={cleanup:?}; sandbox={}\n{}",
                    started.elapsed(), self.0.display(), self.phase_diagnostics(&stdout, &stderr),
                )))
            }
        }
    }

    fn phase_diagnostics(&self, stdout: &Path, stderr: &Path) -> String {
        [stdout, stderr, &self.0.join("report.json")]
            .into_iter()
            .map(|path| {
                let text = match bounded_file(path) {
                    Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                    Err(error) => format!("<unavailable: {error}>"),
                };
                format!("{} (latest available):\n{text}", path.display())
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn command(&self, executable: &Path) -> Command {
        let system = PathBuf::from(std::env::var_os("SystemRoot").unwrap());
        let mut command = Command::new(executable);
        command
            .env_clear()
            .env("SystemRoot", &system)
            .env("ComSpec", system.join("System32").join("cmd.exe"))
            .env("PATHEXT", ".COM;.EXE;.BAT;.CMD")
            .env(
                "PATH",
                std::env::join_paths([self.0.join("bin"), git_bin(), system.join("System32")])
                    .unwrap(),
            )
            .env("HOME", self.0.join("home"))
            .env("USERPROFILE", self.0.join("home"))
            .env("ZDOTDIR", self.0.join("home"))
            .env("APPDATA", self.0.join("appdata"))
            .env("LOCALAPPDATA", self.0.join("localappdata"))
            .env("TEMP", self.0.join("temp"))
            .env("TMP", self.0.join("temp"))
            .env("TMPDIR", self.0.join("temp"))
            .env("HERDR_TEST_TEMP", self.0.join("temp"))
            .env("BASH_ENV", self.0.join("bash-env.sh"))
            .env("XDG_CONFIG_HOME", self.0.join("config"))
            .env("XDG_STATE_HOME", self.0.join("state"))
            .env("XDG_DATA_HOME", self.0.join("data"))
            .env("XDG_CACHE_HOME", self.0.join("cache"))
            .env("XDG_RUNTIME_DIR", self.0.join("runtime"))
            .env("PSModuleAnalysisCachePath", self.0.join("cache/powershell"))
            .env(
                "HERDR_TEST_POWERSHELL",
                system.join("System32/WindowsPowerShell/v1.0/powershell.exe"),
            )
            .env("HERDR_TEST_ARCHIVE", self.0.join("archive 中文.tar.gz"))
            .env("HERDR_TEST_OUTPUT", self.0.join("output 中文"))
            .env("HERDR_TEST_REPORT", self.0.join("report.json"))
            .current_dir(&self.0);
        command
    }

    fn report(&self) -> serde_json::Value {
        serde_json::from_slice(&fs::read(self.0.join("report.json")).unwrap()).unwrap()
    }

    fn powershell(&self, script: &str) -> Command {
        let system = PathBuf::from(std::env::var_os("SystemRoot").unwrap());
        let mut command =
            self.command(&system.join("System32/WindowsPowerShell/v1.0/powershell.exe"));
        command.args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            script,
        ]);
        command
    }

    fn native_tools(&self) -> PathBuf {
        success(
            &self
                .command(Path::new(env!("CARGO_BIN_EXE_herdr")))
                .args(["--internal-codex-launch", "exec"])
                .output()
                .unwrap(),
        );
        let report = self.report();
        let tools = std::env::split_paths(report["path"].as_str().unwrap())
            .next()
            .unwrap();
        assert_eq!(tools.file_name().unwrap(), "native-tools");
        assert!(tools.join("tar.cmd").is_file());
        assert_eq!(fs::read_dir(&tools).unwrap().count(), 1);
        assert!(!tools.join(".herdr-codex-launch-v1").exists());
        fs::remove_file(self.0.join("output 中文/payload.txt")).unwrap();
        tools
    }
}

fn git_bin() -> PathBuf {
    let program_files = std::env::var_os("ProgramFiles").map(PathBuf::from);
    let inherited = std::env::var_os("PATH").unwrap_or_default();
    program_files
        .into_iter()
        .map(|root| root.join("Git/usr/bin"))
        .chain(std::env::split_paths(&inherited))
        .find(|root| {
            ["bash.exe", "tar.exe", "find.exe", "sort.exe"]
                .iter()
                .all(|name| root.join(name).is_file())
        })
        .expect("Git Bash and GNU tools are required for the Windows tar regression")
}

fn prepend_tools(command: &mut Command, tools: &Path) {
    let path = command
        .get_envs()
        .find(|(key, _)| *key == "PATH")
        .unwrap()
        .1
        .unwrap()
        .to_os_string();
    command.env(
        "PATH",
        std::env::join_paths(
            std::iter::once(tools.to_path_buf()).chain(std::env::split_paths(&path)),
        )
        .unwrap(),
    );
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        if self.1.get() {
            eprintln!("preserving failed phase sandbox: {}", self.0.display());
            return;
        }
        let deadline = Instant::now() + Duration::from_secs(30);
        while fs::remove_dir_all(&self.0).is_err() && self.0.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

fn bounded_file(path: &Path) -> io::Result<Vec<u8>> {
    if !fs::symlink_metadata(path)?.is_file() {
        return Err(io::Error::other("diagnostic path is not a regular file"));
    }
    let mut file = fs::File::open(path)?;
    let mut bytes = vec![0; 64 * 1024];
    let mut length = 0;
    while length < bytes.len() {
        let read = file.read(&mut bytes[length..])?;
        if read == 0 {
            break;
        }
        length += read;
    }
    bytes.truncate(length);
    if file.metadata()?.len() > length as u64 {
        bytes.extend_from_slice(b"\n<diagnostic truncated at 64 KiB>\n");
    }
    Ok(bytes)
}

fn private_job() -> io::Result<OwnedHandle> {
    let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    let job = unsafe { OwnedHandle::from_raw_handle(handle) };
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    if unsafe {
        SetInformationJobObject(
            job.as_raw_handle(),
            JobObjectExtendedLimitInformation,
            std::ptr::from_ref(&limits).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(job)
}

fn active_processes(job: &OwnedHandle) -> io::Result<u32> {
    let mut accounting = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
    if unsafe {
        QueryInformationJobObject(
            job.as_raw_handle(),
            JobObjectBasicAccountingInformation,
            std::ptr::from_mut(&mut accounting).cast(),
            size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(accounting.ActiveProcesses)
}

fn resume_child(child: &Child) -> io::Result<()> {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot) };
    let mut entry = THREADENTRY32 {
        dwSize: size_of::<THREADENTRY32>() as u32,
        ..Default::default()
    };
    if unsafe { Thread32First(snapshot.as_raw_handle(), &mut entry) } == 0 {
        return Err(io::Error::last_os_error());
    }
    loop {
        if entry.th32OwnerProcessID == child.id() {
            let thread = unsafe {
                OpenThread(
                    THREAD_SUSPEND_RESUME | THREAD_QUERY_LIMITED_INFORMATION,
                    0,
                    entry.th32ThreadID,
                )
            };
            if thread.is_null() {
                return Err(io::Error::last_os_error());
            }
            let thread = unsafe { OwnedHandle::from_raw_handle(thread) };
            if unsafe { GetProcessIdOfThread(thread.as_raw_handle()) } != child.id() {
                return Err(io::Error::other("created child thread identity changed"));
            }
            if unsafe { ResumeThread(thread.as_raw_handle()) } == u32::MAX {
                return Err(io::Error::last_os_error());
            }
            return Ok(());
        }
        if unsafe { Thread32Next(snapshot.as_raw_handle(), &mut entry) } == 0 {
            return Err(io::Error::other(
                "created child primary thread was not found",
            ));
        }
    }
}

fn stop_phase(child: &mut Child, job: &OwnedHandle) -> io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let job_error =
        (unsafe { TerminateJobObject(job.as_raw_handle(), 1) } == 0).then(io::Error::last_os_error);
    // Assignment failure leaves a suspended child outside the job. Use its handle.
    let root_error = child.kill().err();
    loop {
        let root = child.try_wait();
        let active = active_processes(job);
        if matches!(root, Ok(Some(_))) && matches!(active, Ok(0)) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(io::ErrorKind::TimedOut, format!(
                "cleanup exceeded 5s; root={root:?}; active_processes={active:?}; terminate_job={job_error:?}; kill_root={root_error:?}",
            )));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn check_runtime_environment(git_bash: bool) {
    let sandbox = Sandbox::new();
    let probe = sandbox.0.join("environment.ps1");
    fs::write(
        &probe,
        r#"
$ErrorActionPreference = 'Stop'
$report = @{}
foreach ($name in @('HOME', 'USERPROFILE', 'ZDOTDIR', 'APPDATA', 'LOCALAPPDATA', 'TEMP', 'TMP', 'TMPDIR', 'XDG_CONFIG_HOME', 'XDG_STATE_HOME', 'XDG_DATA_HOME', 'XDG_CACHE_HOME', 'XDG_RUNTIME_DIR', 'PSModuleAnalysisCachePath', 'BASH_ENV')) {
    $report[$name] = [Environment]::GetEnvironmentVariable($name)
}
$report.native_temp = [IO.Path]::GetTempPath()
$report.native_temp_file = [IO.Path]::GetTempFileName()
$report.bash_temp_file = $env:HERDR_TEST_BASH_TEMP
[IO.File]::WriteAllText($env:HERDR_TEST_REPORT, ($report | ConvertTo-Json))
"#,
    )
    .unwrap();
    let mut command = if git_bash {
        let mut command = sandbox.command(&git_bin().join("bash.exe"));
        command.args([
            "--noprofile",
            "--norc",
            "-c",
            r#"set -eu
file=$(mktemp)
export HERDR_TEST_BASH_TEMP="$(cygpath -w -- "$file")"
"$HERDR_TEST_POWERSHELL" -NoLogo -NoProfile -NonInteractive -File "$HERDR_TEST_ENV_PROBE"
"#,
        ]);
        command
    } else {
        sandbox.powershell("& $env:HERDR_TEST_ENV_PROBE")
    };
    success(
        &command
            .env("HERDR_TEST_ENV_PROBE", &probe)
            .output()
            .unwrap(),
    );
    let report = sandbox.report();
    for (name, relative) in [
        ("HOME", "home"),
        ("USERPROFILE", "home"),
        ("ZDOTDIR", "home"),
        ("APPDATA", "appdata"),
        ("LOCALAPPDATA", "localappdata"),
        ("TEMP", "temp"),
        ("TMP", "temp"),
        ("TMPDIR", "temp"),
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_STATE_HOME", "state"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_CACHE_HOME", "cache"),
        ("XDG_RUNTIME_DIR", "runtime"),
        ("native_temp", "temp"),
        ("BASH_ENV", "bash-env.sh"),
    ] {
        let actual = Path::new(report[name].as_str().unwrap());
        assert_eq!(
            actual.canonicalize().unwrap(),
            sandbox.0.join(relative).canonicalize().unwrap(),
            "{name}: {report}"
        );
    }
    let cache = Path::new(report["PSModuleAnalysisCachePath"].as_str().unwrap());
    assert_eq!(cache, sandbox.0.join("cache/powershell"));
    for name in std::iter::once("native_temp_file").chain(git_bash.then_some("bash_temp_file")) {
        let file = Path::new(report[name].as_str().unwrap());
        assert!(file.is_file(), "{name}: {report}");
        assert_eq!(
            file.parent().unwrap().canonicalize().unwrap(),
            sandbox.0.join("temp").canonicalize().unwrap(),
            "{name}: {report}"
        );
    }
}

#[test]
fn windows_codex_sandbox_confines_powershell_runtime_environment() {
    check_runtime_environment(false);
}

#[test]
fn windows_codex_sandbox_confines_git_bash_runtime_environment() {
    check_runtime_environment(true);
}

#[test]
fn windows_codex_launch_real_entry_and_shared_shim_extract_without_user_profiles() {
    let sandbox = Sandbox::new();
    let mut command = sandbox.command(Path::new(env!("CARGO_BIN_EXE_herdr")));
    let original = command
        .get_envs()
        .find(|(key, _)| *key == "PATH")
        .unwrap()
        .1
        .unwrap()
        .to_os_string();
    let output = sandbox
        .phase_output(
            "entry-exec",
            command.args(["--internal-codex-launch", "exec", "中文 prompt"]),
        )
        .unwrap();
    success(&output);
    assert_eq!(
        fs::read(sandbox.0.join("output 中文/payload.txt")).unwrap(),
        b"launcher extraction\n"
    );
    let report = sandbox.report();
    assert_eq!(report["arg1"], "exec");
    assert_eq!(report["arg2"], "中文 prompt");
    assert!(report["active"].is_null());
    let configured = OsString::from(report["path"].as_str().unwrap());
    let mut entries = std::env::split_paths(&configured);
    let tools = entries.next().unwrap();
    assert_eq!(tools.file_name().unwrap(), "native-tools");
    assert_eq!(std::env::join_paths(entries).unwrap(), original);
    let shim = tools.parent().unwrap();
    assert!(shim.join(".herdr-codex-launch-v1").is_file());
    let mut command = sandbox.command(&shim.join("codex.exe"));
    command
        .env("HERDR_ENV", "1")
        .env("HERDR_PANE_ID", "pane:test")
        .env(
            "PATH",
            std::env::join_paths(
                std::iter::once(shim.to_path_buf()).chain(std::env::split_paths(&configured)),
            )
            .unwrap(),
        );
    let output = sandbox
        .phase_output("shim-resume", command.args(["resume", "中文 session"]))
        .unwrap();
    success(&output);
    let report = sandbox.report();
    assert_eq!(report["arg1"], "--no-daemon");
    assert_eq!(report["arg2"], "resume");
    assert_eq!(report["arg3"], "中文 session");
    assert_eq!(report["path"].as_str().unwrap(), configured);
    assert_eq!(fs::read_dir(shim.parent().unwrap()).unwrap().count(), 1);
    let output = sandbox
        .phase_output(
            "shim-missing-archive",
            command.env("HERDR_TEST_ARCHIVE", sandbox.0.join("missing.tar.gz")),
        )
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(output.status.code(), Some(1));
}

#[test]
fn windows_codex_launch_reports_unfixed_tools_and_preserves_recursion_guard() {
    let sandbox = Sandbox::new();
    let state = sandbox.0.join("state/herdr-dev");
    fs::create_dir_all(&state).unwrap();
    fs::write(state.join("codex-shims"), b"not a directory").unwrap();
    let mut command = sandbox.command(Path::new(env!("CARGO_BIN_EXE_herdr")));
    command.args(["--internal-codex-launch", "update"]);
    let output = command.output().unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Windows update tools unavailable"),
        "{stderr}"
    );
    assert!(
        stderr.contains("built-in updates may still fail"),
        "{stderr}"
    );
    assert!(!sandbox.report()["path"]
        .as_str()
        .unwrap()
        .contains("native-tools"));
    fs::remove_file(sandbox.0.join("report.json")).unwrap();
    let output = command
        .env("HERDR_CODEX_LAUNCH_ACTIVE", "1")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("recursive Codex"));
    assert!(!sandbox.0.join("report.json").exists());
}

#[test]
fn windows_codex_native_tar_real_entry_preserves_paths_stdin_failures_and_passthrough() {
    let sandbox = Sandbox::new();
    let product = Path::new(env!("CARGO_BIN_EXE_herdr"));
    let invoke = |args: &[&str]| {
        let mut command = sandbox.command(product);
        command.arg("--internal-native-tar").args(args);
        command
    };
    let output = invoke(&["-xzf", "archive 中文.tar.gz", "-C", "output 中文"])
        .output()
        .unwrap();
    success(&output);
    assert_eq!(
        fs::read(sandbox.0.join("output 中文/payload.txt")).unwrap(),
        b"launcher extraction\n"
    );
    fs::remove_file(sandbox.0.join("output 中文/payload.txt")).unwrap();
    let output = invoke(&["-xzf", "-", "-C", "output 中文"])
        .stdin(fs::File::open(sandbox.0.join("archive 中文.tar.gz")).unwrap())
        .output()
        .unwrap();
    success(&output);
    assert_eq!(
        fs::read(sandbox.0.join("output 中文/payload.txt")).unwrap(),
        b"launcher extraction\n"
    );
    fs::write(sandbox.0.join("broken.tar.gz"), b"broken archive").unwrap();
    for args in [
        ["-xzf", "missing 中文.tar.gz", "-C", "output 中文"],
        ["-xzf", "broken.tar.gz", "-C", "output 中文"],
        ["-xzf", "archive 中文.tar.gz", "-C", "missing 中文"],
        ["-xzf", "archive 中文.tar.gz", "-C", "broken.tar.gz"],
    ] {
        let output = invoke(&args).output().unwrap();
        assert!(!output.status.success(), "{args:?}");
        assert!(!output.stderr.is_empty(), "{args:?}");
    }
    fs::copy(
        sandbox.0.join("archive 中文.tar.gz"),
        sandbox.0.join("fixture.tar.gz"),
    )
    .unwrap();
    let native = PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32/tar.exe");
    fs::create_dir(sandbox.0.join("out")).unwrap();
    for args in [
        vec!["--version"],
        vec!["-tf", "fixture.tar.gz"],
        vec!["-xzf", "fixture.tar.gz", "-C", "out", "absent-entry"],
        vec!["-xzf", "fixture.tar.gz", "-C", "out", "--unknown-option"],
        vec!["-xzf", "fixture.tar.gz", "-C", "out", "-C", "missing"],
    ] {
        let expected = sandbox.command(&native).args(&args).output().unwrap();
        let actual = invoke(&args).output().unwrap();
        assert_eq!(actual.status.code(), expected.status.code(), "{args:?}");
        assert_eq!(actual.stdout, expected.stdout, "{args:?}");
        assert_eq!(actual.stderr, expected.stderr, "{args:?}");
    }
}

#[test]
fn windows_codex_native_tar_installer_extracts_windows_paths_and_preserves_exit_code() {
    let sandbox = Sandbox::new();
    let tools = sandbox.native_tools();
    let mut command = sandbox.powershell(
        "tar -xzf $env:HERDR_TEST_ARCHIVE -C $env:HERDR_TEST_OUTPUT; exit $LASTEXITCODE",
    );
    command.env("ERRORLEVEL", "99");
    assert!(
        !command.output().unwrap().status.success(),
        "GNU tar must reproduce the installer failure"
    );
    prepend_tools(&mut command, &tools);
    success(&command.output().unwrap());
    assert_eq!(
        fs::read(sandbox.0.join("output 中文/payload.txt")).unwrap(),
        b"launcher extraction\n"
    );

    let mut lookup = sandbox.powershell("if ((Get-Command tar).Path -ne $env:HERDR_TEST_TAR) { exit 91 }; tar --version; exit $LASTEXITCODE");
    prepend_tools(&mut lookup, &tools);
    lookup.env("HERDR_TEST_TAR", tools.join("tar.cmd"));
    let version = lookup.output().unwrap();
    success(&version);
    assert!(String::from_utf8_lossy(&version.stdout).contains("bsdtar"));

    let native = PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32/tar.exe");
    let missing = sandbox.0.join("missing 中文.tar.gz");
    let native_failure = sandbox
        .command(&native)
        .arg("-xzf")
        .arg(&missing)
        .arg("-C")
        .arg(sandbox.0.join("output 中文"))
        .output()
        .unwrap();
    let forwarded_failure = command
        .env("HERDR_TEST_ARCHIVE", &missing)
        .output()
        .unwrap();
    assert!(!forwarded_failure.status.success());
    assert_eq!(
        forwarded_failure.status.code(),
        native_failure.status.code()
    );
    let malformed = sandbox.0.join("broken archive.tar.gz");
    fs::write(&malformed, b"not an archive").unwrap();
    assert!(!command
        .env("HERDR_TEST_ARCHIVE", &malformed)
        .output()
        .unwrap()
        .status
        .success());
}

#[test]
fn windows_codex_native_tar_documents_powershell_batch_argument_boundaries() {
    let sandbox = Sandbox::new();
    let tools = sandbox.native_tools();
    let root = Path::new("boundary space");
    fs::create_dir(sandbox.0.join(root)).unwrap();
    let archive = root.join("archive.tar");
    fs::copy(
        sandbox.0.join("archive 中文.tar.gz"),
        sandbox.0.join(&archive),
    )
    .unwrap();
    let native = PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32/tar.exe");
    let mut direct = sandbox.powershell("& $env:HERDR_TEST_NATIVE -xf $env:HERDR_TEST_ARCHIVE -C $env:HERDR_TEST_OUTPUT; exit $LASTEXITCODE");
    let mut forwarded = sandbox.powershell(
        "tar -xf $env:HERDR_TEST_ARCHIVE -C $env:HERDR_TEST_OUTPUT; exit $LASTEXITCODE",
    );
    for command in [&mut direct, &mut forwarded] {
        command
            .env("HERDR_TEST_NATIVE", &native)
            .env("HERDR_TEST_ARCHIVE", &archive)
            .env("HERDR_TEST_OUTPUT", format!("{}\\", root.display()));
    }
    prepend_tools(&mut forwarded, &tools);
    let native_trailing = direct.output().unwrap();
    let batch_trailing = forwarded.output().unwrap();
    assert!(!native_trailing.status.success());
    assert_eq!(batch_trailing.status.code(), native_trailing.status.code());

    // cmd expands %NAME% even in quoted arguments; this is not a general argv proxy.
    let percent = root.join("%HERDR_TEST_EXPAND%.tar");
    fs::copy(sandbox.0.join(&archive), sandbox.0.join(&percent)).unwrap();
    for command in [&mut direct, &mut forwarded] {
        command
            .env("HERDR_TEST_OUTPUT", root)
            .env("HERDR_TEST_ARCHIVE", &percent)
            .env("HERDR_TEST_EXPAND", "expanded");
    }
    success(&direct.output().unwrap());
    assert_eq!(
        fs::read(sandbox.0.join(root).join("payload.txt")).unwrap(),
        b"launcher extraction\n"
    );
    assert!(!forwarded.output().unwrap().status.success());
}

fn check_gnu_shell(sandbox: &Sandbox, shell: &Path, flags: &[&str]) {
    let tools = sandbox.native_tools();
    let script = "command -v tar; command -v find; command -v sort; tar --version; find --version; sort --version";
    let mut command = sandbox.command(shell);
    command.args(flags).arg(script).env(
        "PATH",
        std::env::join_paths([shell.parent().unwrap().to_path_buf(), git_bin()]).unwrap(),
    );
    let before = command.output().unwrap();
    success(&before);
    prepend_tools(&mut command, &tools);
    let after = command.output().unwrap();
    success(&after);
    assert_eq!(before.stdout, after.stdout);
    let text = String::from_utf8_lossy(&after.stdout);
    for expected in ["GNU tar", "GNU findutils", "GNU coreutils"] {
        assert!(text.contains(expected), "{text}");
    }
}

#[test]
fn windows_codex_native_tar_keeps_git_bash_gnu_tools() {
    check_gnu_shell(
        &Sandbox::new(),
        &git_bin().join("bash.exe"),
        &["--noprofile", "--norc", "-c"],
    );
}

#[test]
fn windows_codex_native_tar_keeps_available_gx_zsh_gnu_tools() {
    let zsh = std::env::var_os("HERDR_TEST_GX_ZSH")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("LOCALAPPDATA").map(|root| {
                PathBuf::from(root).join("Programs/GXShell/runtime/msys64/usr/bin/zsh.exe")
            })
        });
    let Some(zsh) = zsh.filter(|path| path.is_file()) else {
        eprintln!("GX Zsh unavailable; set HERDR_TEST_GX_ZSH to qualify this optional runtime");
        return;
    };
    check_gnu_shell(&Sandbox::new(), &zsh, &["-f", "-c"]);
}
