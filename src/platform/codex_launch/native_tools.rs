use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

const DIRECTORY: &str = "native-tools";
// A .cmd is visible to PowerShell, but not bare tar lookup in MSYS Bash/Zsh.
// This forwards the installer's file-path arguments, not arbitrary cmd syntax.
const TAR: &[u8] = b"@echo off\r\nsetlocal EnableExtensions DisableDelayedExpansion\r\nset \"ERRORLEVEL=\"\r\n\"%SystemRoot%\\System32\\tar.exe\" %*\r\nexit /b %errorlevel%\r\n";

pub(super) fn install(shim: &Path) -> io::Result<()> {
    let directory = shim.join(DIRECTORY);
    crate::platform::create_remote_private_dir(&directory)?;
    std::fs::write(directory.join("tar.cmd"), TAR)
}

pub(super) fn matches(shim: &Path) -> bool {
    let directory = shim.join(DIRECTORY);
    std::fs::symlink_metadata(&directory).is_ok_and(|metadata| plain(&metadata, true))
        && std::fs::symlink_metadata(directory.join("tar.cmd"))
            .is_ok_and(|metadata| plain(&metadata, false))
        && std::fs::read(directory.join("tar.cmd")).is_ok_and(|bytes| bytes == TAR)
        && std::fs::read_dir(directory).is_ok_and(|entries| {
            let mut entries = entries;
            entries
                .next()
                .is_some_and(|entry| entry.is_ok_and(|entry| entry.file_name() == "tar.cmd"))
                && entries.next().is_none()
        })
}

fn plain(metadata: &std::fs::Metadata, directory: bool) -> bool {
    use std::os::windows::fs::MetadataExt as _;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0
        && if directory {
            metadata.is_dir()
        } else {
            metadata.is_file()
        }
}

fn environment(command: &Command, key: &str) -> Option<OsString> {
    command
        .get_envs()
        .find(|(name, _)| name.eq_ignore_ascii_case(key))
        .map_or_else(|| std::env::var_os(key), |(_, value)| value.map(Into::into))
}

fn system_tar(command: &Command) -> io::Result<PathBuf> {
    let system = environment(command, "SystemRoot")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "SystemRoot is missing"))?;
    let tar = Path::new(&system).join("System32").join("tar.exe");
    if !tar.is_absolute() || !tar.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("Windows native tar is unavailable: {}", tar.display()),
        ));
    }
    Ok(tar)
}

pub(super) fn configure(command: &mut Command) -> io::Result<()> {
    system_tar(command)?;
    let shim = super::shared::directory_for_current_process()?;
    let directory = shim.join(DIRECTORY);
    let path = environment(command, "PATH").unwrap_or_default();
    let canonical = directory.canonicalize()?;
    let entries = std::env::split_paths(&path).filter(|entry| {
        entry != &directory && entry.canonicalize().ok().as_ref() != Some(&canonical)
    });
    let path = std::env::join_paths(std::iter::once(directory.clone()).chain(entries))
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid Codex child PATH"))?;
    command.env("PATH", path);
    Ok(())
}

#[cfg(test)]
mod tests;
