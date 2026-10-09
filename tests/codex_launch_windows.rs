#![cfg(windows)]

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

fn success(output: &std::process::Output) {
    if !output.status.success() {
        use windows_sys::Win32::Globalization::{GetACP, GetOEMCP};
        use windows_sys::Win32::System::Console::{GetConsoleCP, GetConsoleOutputCP};

        let code_pages = unsafe { (GetACP(), GetOEMCP(), GetConsoleCP(), GetConsoleOutputCP()) };
        eprintln!("ACP/OEMCP/ConsoleCP/ConsoleOutputCP: {code_pages:?}");
        if let Some(system) = std::env::var_os("SystemRoot") {
            let native = PathBuf::from(system).join("System32/tar.exe");
            match Command::new(&native).arg("--version").output() {
                Ok(version) => eprintln!(
                    "{native:?} --version: status={}\nstdout={}\nstderr={}",
                    version.status,
                    String::from_utf8_lossy(&version.stdout),
                    String::from_utf8_lossy(&version.stderr),
                ),
                Err(error) => eprintln!("{native:?} --version: {error}"),
            }
        }
    }
    assert!(
        output.status.success(),
        "status={}\nstdout={}\nstderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

struct Sandbox(PathBuf);

impl Sandbox {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "herdr-codex-native-entry-中文 & (x) !-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        let sandbox = Self(root);
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
            .command(&system.join("System32/tar.exe"))
            .current_dir(&source)
            .args(["-czf", "fixture.tar.gz", "payload.txt"])
            .output()
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
            "\"%HERDR_TEST_POWERSHELL%\" -NoLogo -NoProfile -NonInteractive -Command \"$report = @{path=$env:PATH; arg1=$env:HERDR_TEST_ARG1; arg2=$env:HERDR_TEST_ARG2; arg3=$env:HERDR_TEST_ARG3; active=$env:HERDR_CODEX_LAUNCH_ACTIVE}; [IO.File]::WriteAllText($env:HERDR_TEST_REPORT, ($report | ConvertTo-Json)); tar -xzf $env:HERDR_TEST_ARCHIVE -C $env:HERDR_TEST_OUTPUT; exit $LASTEXITCODE\"\r\n",
            "exit /b %errorlevel%\r\n"
        )).unwrap();
        sandbox
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
        let deadline = Instant::now() + Duration::from_secs(30);
        while fs::remove_dir_all(&self.0).is_err() && self.0.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
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
    let output = command
        .args(["--internal-codex-launch", "exec", "中文 prompt"])
        .output()
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
    let output = command.args(["resume", "中文 session"]).output().unwrap();
    success(&output);
    let report = sandbox.report();
    assert_eq!(report["arg1"], "--no-daemon");
    assert_eq!(report["arg2"], "resume");
    assert_eq!(report["arg3"], "中文 session");
    assert_eq!(report["path"].as_str().unwrap(), configured);
    assert_eq!(fs::read_dir(shim.parent().unwrap()).unwrap().count(), 1);
    let output = command
        .env("HERDR_TEST_ARCHIVE", sandbox.0.join("missing.tar.gz"))
        .output()
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
