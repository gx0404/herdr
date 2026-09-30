use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use super::*;

struct TempRoot(PathBuf);

impl TempRoot {
    fn new(label: &str) -> Self {
        let stamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "herdr-command-search-{label}-{}-{stamp}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("create temp root");
        Self(root)
    }

    fn dir(&self, relative: &str) -> PathBuf {
        let dir = self.0.join(relative);
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn file(&self, relative: &str) -> PathBuf {
        let path = self.0.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create temp parent");
        }
        fs::write(&path, b"").expect("write fake executable");
        path
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn wide(value: &str) -> Vec<u16> {
    OsStr::new(value).encode_wide().collect()
}

fn lowercase(paths: &[PathBuf]) -> Vec<String> {
    paths
        .iter()
        .map(|path| path.to_string_lossy().to_lowercase())
        .collect()
}

#[test]
fn executable_extensions_follow_pathext_with_powershell_last() {
    assert_eq!(
        executable_extensions(Some(OsStr::new(
            ".COM;.EXE;.BAT;.CMD;.VBS;.VBE;.JS;.JSE;.WSF;.WSH;.MSC"
        ))),
        [".com", ".exe", ".bat", ".cmd", ".ps1"],
        "script-host types are never agent CLI entry points"
    );
    assert_eq!(
        executable_extensions(Some(OsStr::new(".CMD;.PS1;.EXE"))),
        [".cmd", ".exe", ".bat", ".ps1"],
        "PATHEXT order wins, .ps1 moves last and missing historical types are appended"
    );
    assert_eq!(
        executable_extensions(Some(OsStr::new(" exe ; ;CMD"))),
        [".exe", ".cmd", ".bat", ".ps1"]
    );
    assert_eq!(
        executable_extensions(None),
        executable_extensions(Some(OsStr::new(DEFAULT_PATHEXT)))
    );
    assert_eq!(
        executable_extensions(Some(OsStr::new("   "))),
        executable_extensions(None)
    );
}

#[test]
fn command_file_candidates_use_pathext_order_and_keep_explicit_extensions() {
    let dir = Path::new(r"C:\tools");
    assert_eq!(
        command_file_candidates_in(dir, "claude", Some(OsStr::new(".CMD;.EXE"))),
        [
            dir.join("claude.cmd"),
            dir.join("claude.exe"),
            dir.join("claude.bat"),
            dir.join("claude.ps1"),
        ]
    );
    assert_eq!(
        command_file_candidates_in(dir, "sqlite3.exe", None),
        [dir.join("sqlite3.exe")]
    );
}

#[test]
fn extensionless_shims_only_count_for_availability() {
    let dir = Path::new(r"C:\tools");
    assert_eq!(
        command_availability_fallback_platform(dir, "pi"),
        Some(dir.join("pi"))
    );
    assert_eq!(
        command_availability_fallback_platform(dir, "sqlite3.exe"),
        None,
        "an explicit extension has no fallback"
    );
    assert!(
        !command_file_candidates_in(dir, "pi", None).contains(&dir.join("pi")),
        "CreateProcess cannot start a POSIX shim, so it is never a launch candidate"
    );
}

#[test]
fn powershell_shims_start_through_the_script_host() {
    for shim in [
        r"C:\Users\me\AppData\Roaming\npm\claude.ps1",
        r"C:\tools\CLAUDE.PS1",
    ] {
        let (program, args) = cli_invocation_platform(Path::new(shim));
        assert_eq!(program, OsString::from(POWERSHELL_SCRIPT_HOST));
        assert_eq!(
            args,
            POWERSHELL_SCRIPT_ARGS
                .iter()
                .map(OsString::from)
                .chain([OsString::from(shim)])
                .collect::<Vec<_>>()
        );
        assert_eq!(args[args.len() - 2], OsString::from("-File"));
    }
    for direct in [
        r"C:\Users\me\AppData\Roaming\npm\claude.cmd",
        r"C:\tools\codex.exe",
    ] {
        let (program, args) = cli_invocation_platform(Path::new(direct));
        assert_eq!(program, OsString::from(direct));
        assert!(args.is_empty(), "{direct} starts directly");
    }
}

#[test]
fn child_path_appends_only_the_registry_entries_the_process_path_lacks() {
    let environment = CommandSearchEnvironment {
        process_path: Some(OsString::from(r"C:\Windows\System32;C:\tools")),
        registry_path: vec![
            // Another spelling (case, trailing backslash) of a process entry, a relative entry.
            OsString::from(r"c:\windows\system32\;C:\Program Files\nodejs\;relative"),
            OsString::from(r"C:\Users\me\AppData\Roaming\npm;C:\TOOLS"),
        ],
        ..CommandSearchEnvironment::default()
    };
    let path = child_path_in(&environment).expect("registry entries are missing");
    assert_eq!(
        std::env::split_paths(&path).collect::<Vec<_>>(),
        [
            PathBuf::from(r"C:\Windows\System32"),
            PathBuf::from(r"C:\tools"),
            PathBuf::from(r"C:\Program Files\nodejs\"),
            PathBuf::from(r"C:\Users\me\AppData\Roaming\npm"),
        ]
    );

    let complete = CommandSearchEnvironment {
        registry_path: vec![OsString::from(r"C:\WINDOWS\system32\;C:\tools\")],
        ..environment
    };
    assert_eq!(
        child_path_in(&complete),
        None,
        "the inherited PATH stays when nothing is missing"
    );
    assert_eq!(child_path_in(&CommandSearchEnvironment::default()), None);
}

#[test]
fn expand_environment_strings_substitutes_known_variables_and_keeps_unknown_ones() {
    let mut lookup = |name: &OsStr| match name.to_str() {
        Some("USERPROFILE") => Some(wide(r"C:\Users\me")),
        Some("PNPM_HOME") => Some(wide(r"C:\Users\me\AppData\Local\pnpm")),
        Some("EMPTY") => Some(Vec::new()),
        _ => None,
    };
    let expand = |raw: &str, lookup: &mut dyn FnMut(&OsStr) -> Option<Vec<u16>>| {
        expand_environment_strings(&wide(raw), lookup)
    };
    assert_eq!(
        expand(r"%USERPROFILE%\.local\bin;%PNPM_HOME%", &mut lookup),
        OsString::from(r"C:\Users\me\.local\bin;C:\Users\me\AppData\Local\pnpm")
    );
    assert_eq!(
        expand(r"%MISSING%\bin;50%;%%;a%EMPTY%b", &mut lookup),
        OsString::from(r"%MISSING%\bin;50%;%%;ab")
    );
    assert_eq!(
        expand(r"%MISSING%USERPROFILE%\x", &mut lookup),
        OsString::from(r"%MISSINGC:\Users\me\x"),
        "the closing % of an unknown name can open the next reference"
    );
    assert_eq!(
        expand(r"C:\no\variables", &mut lookup),
        OsString::from(r"C:\no\variables")
    );
}

#[test]
fn search_dirs_put_process_path_first_then_registry_path_then_known_locations() {
    let root = TempRoot::new("order");
    let profile = root.dir("profile");
    let app_data = root.dir("roaming");
    let local = root.dir("local");
    let process_bin = root.dir("process-bin");
    let registry_bin = root.dir("registry-bin");
    let local_bin = root.dir(r"profile\.local\bin");
    let kimi_bin = root.dir(r"profile\.kimi-code\bin");
    let npm = root.dir(r"roaming\npm");
    let pnpm = root.dir(r"local\pnpm");
    let bun = root.dir(r"profile\.bun\bin");
    let volta = root.dir(r"local\Volta\bin");
    let scoop = root.dir(r"profile\scoop\shims");
    let winget = root.dir(r"local\Microsoft\WinGet\Links");

    let environment = CommandSearchEnvironment {
        process_path: Some(std::env::join_paths([&process_bin]).expect("join")),
        registry_path: vec![
            // Duplicates of the process PATH (in another spelling) and relative entries drop.
            OsString::from(format!(
                "{};relative\\bin;{}",
                process_bin.to_string_lossy().to_uppercase(),
                registry_bin.display()
            )),
            OsString::from(format!("{}\\", registry_bin.display())),
        ],
        user_profile: Some(profile),
        app_data: Some(app_data),
        local_app_data: Some(local),
        ..CommandSearchEnvironment::default()
    };
    assert_eq!(
        lowercase(&command_search_dirs_in(&environment, "kimi")),
        lowercase(&[
            process_bin,
            registry_bin,
            local_bin,
            kimi_bin,
            npm,
            pnpm,
            bun,
            volta,
            scoop,
            winget,
        ])
    );
}

#[test]
fn search_dirs_skip_missing_locations_and_honour_package_manager_roots() {
    let root = TempRoot::new("roots");
    let profile = root.dir("profile");
    let local = root.dir("local");
    let pnpm_home = root.dir("custom-pnpm");
    let bun_bin = root.dir(r"custom-bun\bin");
    let volta_bin = root.dir(r"custom-volta\bin");
    let scoop_shims = root.dir(r"custom-scoop\shims");
    // Default locations exist but configured roots take their place.
    root.dir(r"local\pnpm");
    root.dir(r"profile\.bun\bin");

    let environment = CommandSearchEnvironment {
        user_profile: Some(profile),
        local_app_data: Some(local),
        pnpm_home: Some(pnpm_home.clone()),
        bun_install: Some(root.0.join("custom-bun")),
        volta_home: Some(root.0.join("custom-volta")),
        scoop: Some(root.0.join("custom-scoop")),
        ..CommandSearchEnvironment::default()
    };
    assert_eq!(
        lowercase(&command_search_dirs_in(&environment, "pi")),
        lowercase(&[pnpm_home, bun_bin, volta_bin, scoop_shims]),
        "absent .local/.kimi-code/npm/WinGet directories are not searched"
    );
    assert!(command_search_dirs_in(&CommandSearchEnvironment::default(), "pi").is_empty());
}

#[test]
fn bundled_codex_uses_the_most_recently_written_desktop_build() {
    let root = TempRoot::new("codex");
    let local = root.dir("local");
    let older = root.file(r"local\OpenAI\Codex\bin\0ddb895c950eaeba\codex.exe");
    let newer = root.file(r"local\OpenAI\Codex\bin\faa963e871dd422c\codex.exe");
    root.file(r"local\OpenAI\Codex\bin\1111111111111111\rg.exe");
    let now = SystemTime::now();
    let set_modified = |path: &Path, at: SystemTime| {
        fs::File::options()
            .write(true)
            .open(path)
            .and_then(|file| file.set_modified(at))
            .expect("set modified time");
    };
    set_modified(&older, now - Duration::from_secs(3600));
    set_modified(&newer, now);

    let environment = CommandSearchEnvironment {
        local_app_data: Some(local),
        ..CommandSearchEnvironment::default()
    };
    let expected = newer
        .parent()
        .map(Path::to_path_buf)
        .into_iter()
        .collect::<Vec<_>>();
    assert_eq!(command_search_dirs_in(&environment, "codex"), expected);
    assert_eq!(command_search_dirs_in(&environment, "CODEX"), expected);
    assert!(
        command_search_dirs_in(&environment, "claude").is_empty(),
        "the Codex bundle is only searched for codex"
    );

    set_modified(&older, now + Duration::from_secs(60));
    assert_eq!(
        command_search_dirs_in(&environment, "codex"),
        older
            .parent()
            .map(Path::to_path_buf)
            .into_iter()
            .collect::<Vec<_>>()
    );
}

#[test]
fn bundled_claude_uses_the_highest_version_across_roaming_and_msix_copies() {
    let root = TempRoot::new("claude");
    let app_data = root.dir("roaming");
    let local = root.dir("local");
    root.file(r"roaming\Claude\claude-code\2.1.99\claude.exe");
    let msix = root.file(
        r"local\Packages\Claude_pzs8sxrjxfjjc\LocalCache\Roaming\Claude\claude-code\2.1.284\claude.exe",
    );
    // A newer version directory without the executable (an interrupted update) is skipped.
    root.dir(r"roaming\Claude\claude-code\2.2.0");
    root.dir(r"local\Packages\Microsoft.WindowsTerminal_8wekyb3d8bbwe");

    let environment = CommandSearchEnvironment {
        app_data: Some(app_data.clone()),
        local_app_data: Some(local),
        ..CommandSearchEnvironment::default()
    };
    assert_eq!(
        command_search_dirs_in(&environment, "claude"),
        msix.parent()
            .map(Path::to_path_buf)
            .into_iter()
            .collect::<Vec<_>>()
    );

    let roaming = root.file(r"roaming\Claude\claude-code\2.1.1000\claude.exe");
    assert_eq!(
        command_search_dirs_in(&environment, "claude"),
        roaming
            .parent()
            .map(Path::to_path_buf)
            .into_iter()
            .collect::<Vec<_>>(),
        "versions compare numerically across both copies"
    );
    let only_roaming = CommandSearchEnvironment {
        app_data: Some(app_data),
        ..CommandSearchEnvironment::default()
    };
    assert_eq!(
        command_search_dirs_in(&only_roaming, "claude"),
        roaming
            .parent()
            .map(Path::to_path_buf)
            .into_iter()
            .collect::<Vec<_>>()
    );
}

/// Only the Claude app's published package family is trusted; look-alike package folders never
/// contribute a `claude.exe`, however new their version directory.
#[test]
fn bundled_claude_only_trusts_the_published_msix_package_family() {
    let root = TempRoot::new("claude-family");
    let local = root.dir("local");
    for family in ["Claude_0123456789abc", "ClaudeBeta_pzs8sxrjxfjjc", "claude"] {
        root.file(&format!(
            r"local\Packages\{family}\LocalCache\Roaming\Claude\claude-code\9.9.9\claude.exe"
        ));
    }
    let environment = CommandSearchEnvironment {
        local_app_data: Some(local),
        ..CommandSearchEnvironment::default()
    };
    assert!(command_search_dirs_in(&environment, "claude").is_empty());

    let genuine = root.file(
        r"local\Packages\Claude_pzs8sxrjxfjjc\LocalCache\Roaming\Claude\claude-code\2.1.284\claude.exe",
    );
    assert_eq!(
        command_search_dirs_in(&environment, "claude"),
        genuine
            .parent()
            .map(Path::to_path_buf)
            .into_iter()
            .collect::<Vec<_>>()
    );
}

struct PinnedEnvironment;

impl PinnedEnvironment {
    fn new(environment: CommandSearchEnvironment) -> Self {
        set_test_command_search_environment(Some(environment));
        Self
    }
}

impl Drop for PinnedEnvironment {
    fn drop(&mut self) {
        set_test_command_search_environment(None);
    }
}

#[test]
fn test_threads_search_only_the_process_path_unless_an_environment_is_pinned() {
    let root = TempRoot::new("override");
    let bin = root.dir("bin");
    set_test_command_search_environment(None);
    let hermetic = command_search_dirs_platform("claude");
    let process_path = std::env::var_os("PATH").unwrap_or_default();
    assert!(hermetic
        .iter()
        .all(|dir| std::env::split_paths(&process_path).any(|entry| entry == *dir)));

    let _pinned = PinnedEnvironment::new(CommandSearchEnvironment {
        registry_path: vec![bin.clone().into_os_string()],
        path_ext: Some(OsString::from(".EXE")),
        ..CommandSearchEnvironment::default()
    });
    assert_eq!(
        command_search_dirs_platform("claude"),
        std::slice::from_ref(&bin)
    );
    assert_eq!(
        command_file_candidates_platform(&bin, "claude"),
        [
            bin.join("claude.exe"),
            bin.join("claude.cmd"),
            bin.join("claude.bat"),
            bin.join("claude.ps1"),
        ]
    );
}

/// The machine environment key always carries `Path` (System32 at minimum), so the registry
/// read and its `%SystemRoot%` expansion are exercised on every Windows host. Directories are
/// compared by their canonical form: a host may spell System32 with a trailing backslash or an
/// 8.3 short name.
#[test]
fn system_environment_reads_the_current_registry_path() {
    let environment = CommandSearchEnvironment::from_system();
    assert!(
        !environment.registry_path.is_empty(),
        "HKLM Environment\\Path must be readable"
    );
    let system32 = std::env::var_os("SystemRoot")
        .map(|root| PathBuf::from(root).join("System32"))
        .and_then(|dir| fs::canonicalize(dir).ok())
        .expect("SystemRoot is set on Windows and System32 exists");
    let without_process_path = CommandSearchEnvironment {
        process_path: None,
        ..environment
    };
    let dirs = command_search_dirs_in(&without_process_path, "cmd");
    assert!(
        dirs.iter()
            .any(|dir| fs::canonicalize(dir).is_ok_and(|dir| dir == system32)),
        "registry PATH (expanded) reaches System32: {dirs:?}"
    );
    assert!(
        dirs.iter().all(|dir| !dir
            .to_string_lossy()
            .to_ascii_lowercase()
            .contains("%systemroot%")),
        "no unexpanded %SystemRoot% entries: {dirs:?}"
    );
}

/// Read-only check of this machine's real agent CLI locations (registry PATH, desktop app
/// bundles): `cargo nextest run --run-ignored only real_machine_agent_cli_locations`.
#[test]
#[ignore = "inspects the developer machine; run manually"]
fn real_machine_agent_cli_locations() {
    let environment = CommandSearchEnvironment::from_system();
    for command in ["codex", "claude", "kimi", "opencode", "pi", "sqlite3"] {
        let found = command_search_dirs_in(&environment, command)
            .into_iter()
            .find_map(|dir| {
                command_file_candidates_in(&dir, command, environment.path_ext.as_deref())
                    .into_iter()
                    .find(|path| path.is_file())
            });
        println!("{command}: {found:?}");
    }
    let _pinned = PinnedEnvironment::new(environment);
    for target in crate::api::schema::IntegrationTarget::ALL {
        println!(
            "integration {target:?} available: {}",
            crate::integration::integration_target_available(target)
        );
    }
}
