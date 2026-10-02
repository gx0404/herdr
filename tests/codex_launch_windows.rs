#![cfg(windows)]

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

struct Sandbox(PathBuf);

impl Sandbox {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "herdr-codex-native-entry-{}-{}",
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
            "source 中文",
            "output 中文",
        ] {
            fs::create_dir(sandbox.0.join(directory)).unwrap();
        }
        fs::write(
            sandbox.0.join("source 中文/payload.txt"),
            b"launcher extraction\n",
        )
        .unwrap();
        let system = PathBuf::from(std::env::var_os("SystemRoot").unwrap());
        let archive = Command::new(system.join("System32/tar.exe"))
            .arg("-czf")
            .arg(sandbox.0.join("archive 中文.tar.gz"))
            .arg("-C")
            .arg(sandbox.0.join("source 中文"))
            .arg("payload.txt")
            .output()
            .unwrap();
        assert!(archive.status.success(), "{archive:?}");
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
        let git = PathBuf::from(std::env::var_os("ProgramFiles").unwrap()).join("Git/usr/bin");
        assert!(git.join("tar.exe").is_file(), "Git GNU tar required");
        let mut command = Command::new(executable);
        command
            .env_clear()
            .env("SystemRoot", &system)
            .env("ComSpec", system.join("System32").join("cmd.exe"))
            .env("PATHEXT", ".COM;.EXE;.BAT;.CMD")
            .env(
                "PATH",
                std::env::join_paths([self.0.join("bin"), git, system.join("System32")]).unwrap(),
            )
            .env("HOME", self.0.join("home"))
            .env("USERPROFILE", self.0.join("home"))
            .env("TEMP", self.0.join("temp"))
            .env("TMP", self.0.join("temp"))
            .env("XDG_CONFIG_HOME", self.0.join("config"))
            .env("XDG_STATE_HOME", self.0.join("state"))
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
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while fs::remove_dir_all(&self.0).is_err() && self.0.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
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
    assert!(output.status.success(), "{output:?}");
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
    assert!(output.status.success(), "{output:?}");
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
