use super::*;
use crate::config::test_dirs::{isolate_dirs, IsolatedDirs};
use std::fs;
use std::process::Output;

fn powershell(dirs: &IsolatedDirs, script: &str) -> Command {
    let system = PathBuf::from(std::env::var_os("SystemRoot").unwrap());
    let mut command = Command::new(system.join("System32/WindowsPowerShell/v1.0/powershell.exe"));
    command
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            script,
        ])
        .env("HOME", dirs.home_dir())
        .env("USERPROFILE", dirs.home_dir())
        .env("ZDOTDIR", dirs.home_dir())
        .env_remove("BASH_ENV")
        .env_remove("ENV");
    command
}

fn success(output: Output) -> Output {
    assert!(output.status.success(), "{output:?}");
    output
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

fn contaminated_path() -> OsString {
    let system = PathBuf::from(std::env::var_os("SystemRoot").unwrap());
    // Do not inherit the user's temporary tar.cmd workaround.
    std::env::join_paths([git_bin(), system.join("System32"), system]).unwrap()
}

#[test]
fn windows_codex_native_tar_installer_extracts_windows_paths_and_preserves_exit_code() {
    let _lock = crate::config::test_config_env_lock();
    let dirs = isolate_dirs("codex-tar-extract");
    let root = dirs.state_dir().join("中文 更新包 & (x) !");
    let source = root.join("source files");
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("payload.txt"), b"native tar round trip\n").unwrap();
    let archive = root.join("archive 中文.tar.gz");
    let native = system_tar(&Command::new("codex")).unwrap();
    success(
        Command::new(&native)
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(&source)
            .arg("payload.txt")
            .output()
            .unwrap(),
    );
    let output_dir = root.join("output 中文");
    fs::create_dir(&output_dir).unwrap();
    let script = "tar -xzf $env:HERDR_TEST_ARCHIVE -C $env:HERDR_TEST_OUTPUT; exit $LASTEXITCODE";
    let mut command = powershell(&dirs, script);
    command
        .env("PATH", contaminated_path())
        .env("HERDR_TEST_ARCHIVE", &archive)
        .env("HERDR_TEST_OUTPUT", &output_dir)
        .env("ERRORLEVEL", "99");
    assert!(
        !command.output().unwrap().status.success(),
        "GNU tar must reproduce the installer failure"
    );
    configure(&mut command).unwrap();
    success(command.output().unwrap());
    assert_eq!(
        fs::read(output_dir.join("payload.txt")).unwrap(),
        b"native tar round trip\n"
    );

    let mut lookup = powershell(&dirs, "if ((Get-Command tar).Path -ne $env:HERDR_TEST_TAR) { exit 91 }; tar --version; exit $LASTEXITCODE");
    lookup.env("PATH", contaminated_path());
    configure(&mut lookup).unwrap();
    let tools = std::env::split_paths(&environment(&lookup, "PATH").unwrap())
        .next()
        .unwrap();
    lookup.env("HERDR_TEST_TAR", tools.join("tar.cmd"));
    let version = success(lookup.output().unwrap());
    assert!(String::from_utf8_lossy(&version.stdout).contains("bsdtar"));
    assert_eq!(fs::read_dir(&tools).unwrap().count(), 1);
    assert!(!super::super::is_shim(&tools.join("codex")));

    let missing = root.join("missing 中文.tar.gz");
    let native_failure = Command::new(&native)
        .arg("-xzf")
        .arg(&missing)
        .arg("-C")
        .arg(&output_dir)
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
    let malformed = root.join("broken archive.tar.gz");
    fs::write(&malformed, b"not an archive").unwrap();
    command.env("HERDR_TEST_ARCHIVE", &malformed);
    assert!(!command.output().unwrap().status.success());
}

#[test]
fn windows_codex_native_tar_documents_powershell_batch_argument_boundaries() {
    let _lock = crate::config::test_config_env_lock();
    let dirs = isolate_dirs("codex-tar-boundaries");
    let root = dirs.state_dir().join("boundary space");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("payload.txt"), b"payload").unwrap();
    let archive = root.join("archive.tar");
    let native = system_tar(&Command::new("codex")).unwrap();
    success(
        Command::new(&native)
            .arg("-cf")
            .arg(&archive)
            .arg("-C")
            .arg(&root)
            .arg("payload.txt")
            .output()
            .unwrap(),
    );
    let mut direct = powershell(
        &dirs,
        "& $env:HERDR_TEST_NATIVE -xf $env:HERDR_TEST_ARCHIVE -C $env:HERDR_TEST_OUTPUT; exit $LASTEXITCODE",
    );
    direct
        .env("HERDR_TEST_NATIVE", &native)
        .env("HERDR_TEST_ARCHIVE", &archive)
        .env("HERDR_TEST_OUTPUT", format!("{}\\", root.display()));
    let mut forwarded = powershell(
        &dirs,
        "tar -xf $env:HERDR_TEST_ARCHIVE -C $env:HERDR_TEST_OUTPUT; exit $LASTEXITCODE",
    );
    forwarded
        .envs(
            direct
                .get_envs()
                .filter_map(|(key, value)| value.map(|value| (key, value))),
        )
        .env("PATH", contaminated_path());
    configure(&mut forwarded).unwrap();
    let native_trailing = direct.output().unwrap();
    let batch_trailing = forwarded.output().unwrap();
    assert!(!native_trailing.status.success());
    assert_eq!(batch_trailing.status.code(), native_trailing.status.code());

    // cmd expands %NAME% even in quoted arguments; this is not a general argv proxy.
    let percent = root.join("%HERDR_TEST_EXPAND%.tar");
    fs::copy(&archive, &percent).unwrap();
    for command in [&mut direct, &mut forwarded] {
        command
            .env("HERDR_TEST_OUTPUT", &root)
            .env("HERDR_TEST_ARCHIVE", &percent)
            .env("HERDR_TEST_EXPAND", "expanded");
    }
    success(direct.output().unwrap());
    assert!(!forwarded.output().unwrap().status.success());
}

fn check_gnu_shell(dirs: &IsolatedDirs, shell: &Path, flags: &[&str]) {
    let script = "command -v tar; command -v find; command -v sort; tar --version; find --version; sort --version";
    let mut command = Command::new(shell);
    command
        .args(flags)
        .arg(script)
        .env("HOME", dirs.home_dir())
        .env("USERPROFILE", dirs.home_dir())
        .env("ZDOTDIR", dirs.home_dir())
        .env_remove("BASH_ENV")
        .env_remove("ENV")
        .env(
            "PATH",
            std::env::join_paths([shell.parent().unwrap().to_path_buf(), git_bin()]).unwrap(),
        );
    let before = success(command.output().unwrap());
    configure(&mut command).unwrap();
    let after = success(command.output().unwrap());
    assert_eq!(before.stdout, after.stdout);
    let text = String::from_utf8_lossy(&after.stdout);
    for expected in ["GNU tar", "GNU findutils", "GNU coreutils"] {
        assert!(text.contains(expected), "{text}");
    }
}

#[test]
fn windows_codex_native_tar_keeps_git_bash_gnu_tools() {
    let _lock = crate::config::test_config_env_lock();
    let dirs = isolate_dirs("codex-tar-bash");
    check_gnu_shell(
        &dirs,
        &git_bin().join("bash.exe"),
        &["--noprofile", "--norc", "-c"],
    );
}

#[test]
fn windows_codex_native_tar_keeps_available_gx_zsh_gnu_tools() {
    let _lock = crate::config::test_config_env_lock();
    let dirs = isolate_dirs("codex-tar-zsh");
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
    check_gnu_shell(&dirs, &zsh, &["-f", "-c"]);
}

#[test]
fn windows_codex_native_tar_missing_system_tool_leaves_command_unchanged() {
    let dirs = isolate_dirs("codex-tar-missing");
    let mut command = Command::new("codex");
    command
        .env("PATH", "first;second")
        .env("SystemRoot", dirs.state_dir());
    let before = format!("{command:?}");
    let error = configure(&mut command).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::NotFound);
    assert!(error
        .to_string()
        .contains("Windows native tar is unavailable"));
    assert_eq!(format!("{command:?}"), before);
    command.env_remove("SystemRoot");
    assert!(configure(&mut command)
        .unwrap_err()
        .to_string()
        .contains("SystemRoot is missing"));
}

#[test]
fn windows_codex_native_tar_creation_failure_and_damage_are_not_accepted() {
    let dirs = isolate_dirs("codex-tar-damage");
    let shim = dirs.state_dir();
    fs::create_dir_all(shim).unwrap();
    fs::write(shim.join(DIRECTORY), b"foreign file").unwrap();
    assert!(install(shim).is_err());
    assert_eq!(fs::read(shim.join(DIRECTORY)).unwrap(), b"foreign file");
    fs::remove_file(shim.join(DIRECTORY)).unwrap();
    install(shim).unwrap();
    assert!(matches(shim));
    fs::write(shim.join(DIRECTORY).join("find.exe"), b"unexpected").unwrap();
    assert!(!matches(shim));
    fs::remove_file(shim.join(DIRECTORY).join("find.exe")).unwrap();
    fs::write(shim.join(DIRECTORY).join("tar.cmd"), b"damaged").unwrap();
    assert!(!matches(shim));
}
