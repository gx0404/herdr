use super::*;
use crate::config::test_dirs::isolate_dirs;
use std::fs;

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

#[test]
fn windows_codex_native_tar_limits_adaptation_to_exact_installer_arguments() {
    let dirs = isolate_dirs("codex-tar-contract");
    fs::create_dir_all(dirs.state_dir()).unwrap();
    let archive = dirs.state_dir().join("中文 & () ! archive.tar.gz");
    fs::write(&archive, b"archive bytes").unwrap();
    let output = dirs.state_dir().join("中文 & () ! output");
    let args = vec![
        "-xzf".into(),
        archive.into_os_string(),
        "-C".into(),
        output.clone().into_os_string(),
    ];
    let command = extraction_command(&args).unwrap();
    assert_eq!(command.get_args().collect::<Vec<_>>(), ["-xzf", "-"]);
    assert_eq!(command.get_current_dir(), Some(output.as_path()));
    for args in [
        vec!["--version"],
        vec!["-tf", "archive.tar.gz"],
        vec!["-xf", "archive.tar", "-C", "output"],
        vec!["-xzf", "archive.tar.gz", "-C", "output", "payload.txt"],
        vec!["-xzf", "archive.tar.gz", "-C", "output", "-C", "other"],
        vec!["-xzf", "archive.tar.gz", "-C", "output", "--unknown"],
        vec!["-xzf", "archive.tar.gz"],
    ] {
        let args: Vec<OsString> = args.into_iter().map(Into::into).collect();
        let command = extraction_command(&args).unwrap();
        assert_eq!(command.get_args().collect::<Vec<_>>(), args);
        assert_eq!(command.get_current_dir(), None);
    }
    let stdin_args = ["-xzf", "-", "-C", "relative output"].map(Into::into);
    let command = extraction_command(&stdin_args).unwrap();
    assert_eq!(command.get_args().collect::<Vec<_>>(), ["-xzf", "-"]);
    assert_eq!(
        command.get_current_dir(),
        Some(Path::new("relative output"))
    );
}
