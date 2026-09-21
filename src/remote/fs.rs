//! Saved-profile remote filesystem access over the OpenSSH `sftp` client.
//!
//! Every operation runs `sftp -b -` (batch on stdin) against the profile's
//! target with the same managed ssh config as the endpoint connection and
//! port forwards: user config includes first, profile directives as the
//! fallback, multiplexing when the platform offers it, and the
//! non-interactive option family (BatchMode, strict host key, keepalives,
//! connect timeout). sftp was chosen over ssh exec one-liners
//! (`ls`/`stat`/`base64`): the batch protocol is binary-safe, identical on
//! POSIX and Windows OpenSSH servers, and free of remote shell quoting and
//! GNU/BSD tool differences.
//!
//! Two quirks shape the implementation. First, sftp has no `stat` command,
//! so metadata comes from parsing OpenSSH's long-listing format. Second,
//! escape handling inside batch quotes is inconsistent across commands and
//! OpenSSH versions (verified against 8.2: `cd` keeps `\x` literally, `get`
//! consumes it), so `\` and `"` are rejected at validation and every
//! argument is a plainly quoted literal — quoted arguments do not glob on
//! the commands we use, listings run as `cd <dir>` + bare `ls -la` (`cd`
//! never globs), and exact names are matched client-side after a listing.

use std::io::{self, Write as _};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use super::attach::{
    apply_managed_channel_options, apply_noninteractive_ssh_options, write_managed_ssh_config,
    ManagedSshConfig,
};
use crate::client::endpoint::SavedSshEndpoint;

/// Size cap for `read_small_file`/`write_small_file` payloads.
pub(crate) const MAX_SMALL_FILE_BYTES: u64 = 1024 * 1024;

const SFTP_COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_LISTING_BYTES: usize = 16 * 1024 * 1024;
const MAX_STATUS_OUTPUT_BYTES: usize = 8 * 1024;
const MAX_SFTP_STDERR_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RemoteEntryKind {
    File,
    Directory,
    Symlink,
    Other,
}

impl RemoteEntryKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Directory => "directory",
            Self::Symlink => "symlink",
            Self::Other => "other",
        }
    }
}

/// One `ls -la` entry. `modified` is the server-rendered column verbatim
/// (for example `Sep 17 10:03`); parsing server locales is deliberately out
/// of scope. Symlink names containing " -> " are indistinguishable from the
/// link annotation, exactly like in `ls` output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemoteDirEntry {
    pub(crate) name: String,
    pub(crate) kind: RemoteEntryKind,
    pub(crate) size: u64,
    pub(crate) mode: Option<u32>,
    pub(crate) link_target: Option<String>,
    pub(crate) modified: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemoteFileStat {
    pub(crate) kind: RemoteEntryKind,
    pub(crate) size: u64,
    pub(crate) mode: Option<u32>,
    pub(crate) modified: String,
    pub(crate) link_target: Option<String>,
}

impl RemoteDirEntry {
    fn into_stat(self) -> RemoteFileStat {
        RemoteFileStat {
            kind: self.kind,
            size: self.size,
            mode: self.mode,
            modified: self.modified,
            link_target: self.link_target,
        }
    }
}

/// Remote filesystem bound to one saved profile. Holds the managed ssh
/// config alive; dropping it removes the config directory like the other
/// saved-machine paths.
pub(crate) struct RemoteFs {
    target: String,
    identity_file: Option<String>,
    config: ManagedSshConfig,
}

impl RemoteFs {
    pub(crate) fn connect(profile: &SavedSshEndpoint) -> io::Result<Self> {
        let profile_options = super::saved::saved_profile_ssh_options(profile)?;
        let mut config = write_managed_ssh_config(profile_options.as_ref())?;
        // 一次性 sftp 通道不建控制主连接：每个操作都会写一份新的受管配置
        // （ControlPath 因此每次不同），`ControlMaster=auto` + `ControlPersist`
        // 会让每个操作在后台留下一个永不退出的 ssh master（HERDR-MACH-004；
        // man ssh_config：`ControlPersist yes` = 永久驻留）。没有共享
        // ControlPath 时复用本就为零，这里显式关掉，与 `machine exec`、
        // 端口转发等一次性通道同口径。
        config.options.control_path = None;
        Ok(Self {
            target: profile.target.clone(),
            identity_file: profile.identity_file.first().cloned(),
            config,
        })
    }

    /// Directory listing without `.`/`..`. Entries whose line does not match
    /// OpenSSH's long format are skipped; if nothing parses at all the
    /// listing is rejected as invalid data (non-OpenSSH server).
    pub(crate) fn list_dir(&self, path: &str) -> io::Result<Vec<RemoteDirEntry>> {
        Ok(self
            .listing(path)?
            .into_iter()
            .filter(|entry| entry.name != "." && entry.name != "..")
            .collect())
    }

    /// Metadata for one path. sftp has no `stat` command, so this lists the
    /// parent and matches the basename; a trailing-slash or root path reports
    /// a synthetic directory stat. Symlinks are not followed (lstat
    /// semantics, matching the listing).
    pub(crate) fn stat(&self, path: &str) -> io::Result<RemoteFileStat> {
        let path = validate_remote_path(path)?;
        let trimmed = path.trim_end_matches('/');
        if trimmed.is_empty() {
            return Ok(RemoteFileStat {
                kind: RemoteEntryKind::Directory,
                size: 0,
                mode: None,
                modified: String::new(),
                link_target: None,
            });
        }
        let (parent, base) = split_remote_path(trimmed);
        self.listing(parent)?
            .into_iter()
            .find(|entry| entry.name == base)
            .map(RemoteDirEntry::into_stat)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no such remote file or directory: {path}"),
                )
            })
    }

    /// Reads a remote file of at most [`MAX_SMALL_FILE_BYTES`]. The size is
    /// checked against the remote stat first and the downloaded bytes again,
    /// so a file growing mid-transfer is still rejected. The exact stat
    /// match pins the identity of what `get` retrieves.
    pub(crate) fn read_small_file(&self, path: &str) -> io::Result<Vec<u8>> {
        let path = validate_remote_path(path)?;
        let stat = self.stat(path)?;
        if stat.kind == RemoteEntryKind::Directory {
            return Err(io::Error::new(
                io::ErrorKind::IsADirectory,
                format!("remote path is a directory: {path}"),
            ));
        }
        if stat.size > MAX_SMALL_FILE_BYTES {
            return Err(too_large(stat.size));
        }
        let temp = LocalTempFile::create()?;
        self.run_batch(
            &format!(
                "get {} {}",
                quote_batch_path(path),
                quote_batch_path(&temp.path.to_string_lossy())
            ),
            MAX_STATUS_OUTPUT_BYTES,
        )?;
        let data = std::fs::read(&temp.path)?;
        if data.len() as u64 > MAX_SMALL_FILE_BYTES {
            return Err(too_large(data.len() as u64));
        }
        Ok(data)
    }

    /// Writes at most [`MAX_SMALL_FILE_BYTES`] to a remote path atomically:
    /// upload to a sibling temporary name, then `rename` over the target.
    /// When the target exists with a known mode, that mode is restored with
    /// `chmod` afterwards (sftp would otherwise apply the server umask). A
    /// failed upload best-effort removes the temporary name.
    pub(crate) fn write_small_file(&self, path: &str, data: &[u8]) -> io::Result<()> {
        let path = validate_remote_path(path)?;
        if data.len() as u64 > MAX_SMALL_FILE_BYTES {
            return Err(too_large(data.len() as u64));
        }
        let existing_mode = self.stat(path).ok().and_then(|stat| stat.mode);
        let temp = LocalTempFile::create()?;
        std::fs::write(&temp.path, data)?;
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let remote_tmp = format!("{path}.herdr-tmp-{}-{sequence}", std::process::id());
        let mut script = format!(
            "put {} {}\nrename {} {}\n",
            quote_batch_path(&temp.path.to_string_lossy()),
            quote_batch_path(&remote_tmp),
            quote_batch_path(&remote_tmp),
            quote_batch_path(path),
        );
        if let Some(mode) = existing_mode {
            script.push_str(&format!(
                "chmod {:o} {}\n",
                mode & 0o7777,
                quote_batch_path(path)
            ));
        }
        match self.run_batch(&script, MAX_STATUS_OUTPUT_BYTES) {
            Ok(_) => Ok(()),
            Err(error) => {
                let _ = self.run_batch(
                    &format!("rm {}", quote_batch_path(&remote_tmp)),
                    MAX_STATUS_OUTPUT_BYTES,
                );
                Err(error)
            }
        }
    }

    pub(crate) fn mkdir(&self, path: &str, parents: bool) -> io::Result<()> {
        let path = validate_remote_path(path)?;
        let trimmed = path.trim_end_matches('/');
        if trimmed.is_empty() {
            return Ok(());
        }
        if !parents {
            self.run_batch(
                &format!("mkdir {}", quote_batch_path(trimmed)),
                MAX_STATUS_OUTPUT_BYTES,
            )?;
            return Ok(());
        }
        let mut current = String::new();
        if trimmed.starts_with('/') {
            current.push('/');
        }
        for component in trimmed.split('/').filter(|component| !component.is_empty()) {
            if !current.is_empty() && !current.ends_with('/') {
                current.push('/');
            }
            current.push_str(component);
            match self.stat(&current) {
                Ok(stat) if stat.kind == RemoteEntryKind::Directory => {}
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::NotADirectory,
                        format!("remote path exists and is not a directory: {current}"),
                    ));
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    self.run_batch(
                        &format!("mkdir {}", quote_batch_path(&current)),
                        MAX_STATUS_OUTPUT_BYTES,
                    )?;
                }
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    pub(crate) fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        let from = validate_remote_path(from)?;
        let to = validate_remote_path(to)?;
        self.run_batch(
            &format!("rename {} {}", quote_batch_path(from), quote_batch_path(to)),
            MAX_STATUS_OUTPUT_BYTES,
        )?;
        Ok(())
    }

    /// Deletes a file, symlink, or directory. Directories require
    /// `recursive`; recursion deletes entries depth-first and never follows
    /// symlinks. Deleting the remote root is refused outright.
    pub(crate) fn delete(&self, path: &str, recursive: bool) -> io::Result<()> {
        let path = validate_remote_path(path)?;
        let trimmed = path.trim_end_matches('/');
        if trimmed.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "refusing to delete the remote root",
            ));
        }
        let stat = self.stat(trimmed)?;
        if stat.kind != RemoteEntryKind::Directory {
            self.run_batch(
                &format!("rm {}", quote_batch_path(trimmed)),
                MAX_STATUS_OUTPUT_BYTES,
            )?;
            return Ok(());
        }
        if !recursive {
            self.run_batch(
                &format!("rmdir {}", quote_batch_path(trimmed)),
                MAX_STATUS_OUTPUT_BYTES,
            )?;
            return Ok(());
        }
        for entry in self.list_dir(trimmed)? {
            self.delete(&format!("{trimmed}/{}", entry.name), true)?;
        }
        self.run_batch(
            &format!("rmdir {}", quote_batch_path(trimmed)),
            MAX_STATUS_OUTPUT_BYTES,
        )?;
        Ok(())
    }

    /// `cd` does not glob and a bare `ls` uses no pattern, so this lists
    /// directories with any name safely.
    fn listing(&self, path: &str) -> io::Result<Vec<RemoteDirEntry>> {
        let path = validate_remote_path(path)?;
        let output = self.run_batch(
            &format!("cd {}\nls -la", quote_batch_path(path)),
            MAX_LISTING_BYTES,
        )?;
        if output.stdout.len() >= MAX_LISTING_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "remote directory listing exceeds the size limit",
            ));
        }
        parse_long_listing(&output.stdout)
    }

    fn run_batch(&self, script: &str, stdout_limit: usize) -> io::Result<Output> {
        let mut command = Command::new("sftp");
        apply_managed_channel_options(&mut command, Some(&self.config.options));
        apply_noninteractive_ssh_options(
            &mut command,
            self.config.options.server_alive_interval,
            self.config.options.server_alive_count_max,
            self.config.options.strict_host_key_checking,
        );
        command
            .arg("-b")
            .arg("-")
            .arg(&self.target)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = super::process::spawn(&mut command).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("failed to start the sftp client: {error}"),
            )
        })?;
        if let Some(mut stdin) = child.stdin.take() {
            // A dead ssh makes this write fail; the exit status and stderr
            // carry the real error, so the write result is ignored here.
            let _ = stdin.write_all(script.as_bytes());
        }
        let output = super::process::wait_with_output_timeout_bounded(
            child,
            SFTP_COMMAND_TIMEOUT,
            stdout_limit,
            MAX_SFTP_STDERR_BYTES,
        )?;
        if output.status.success() {
            return Ok(output);
        }
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let message = if stderr.is_empty() {
            format!("sftp exited with {}", output.status)
        } else {
            stderr
        };
        let error = io::Error::new(
            fs_error_kind(&message),
            format!("remote file operation failed: {message}"),
        );
        if is_fs_level_error(&message) {
            return Err(error);
        }
        // Transport/auth/host-key failures keep the structured connection
        // classification so supervisors react to the kind, not the text.
        Err(super::error::classify_and_wrap(
            error,
            &self.target,
            self.identity_file.clone(),
        ))
    }
}

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);

struct LocalTempFile {
    path: PathBuf,
}

impl LocalTempFile {
    fn create() -> io::Result<Self> {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("herdr-fs-{}-{sequence}", std::process::id()));
        // 0600 from creation; sftp truncates and rewrites the file itself.
        crate::platform::create_private_state_file(&path)?;
        Ok(Self { path })
    }
}

impl Drop for LocalTempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn too_large(size: u64) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("remote file is {size} bytes; the small-file limit is {MAX_SMALL_FILE_BYTES}"),
    )
}

fn validate_remote_path(path: &str) -> io::Result<&str> {
    if path.is_empty()
        || path
            .bytes()
            .any(|byte| byte == 0 || byte == b'\n' || byte == b'\r')
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "remote path must not be empty or contain NUL/newline characters",
        ));
    }
    if path.starts_with('-') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "remote paths must not start with '-'; use an absolute path or ./name",
        ));
    }
    // Backslash and double-quote escape handling inside sftp batch quotes is
    // inconsistent across commands and OpenSSH versions (verified against
    // 8.2), so these characters are rejected rather than mis-sent.
    if path.contains(['\\', '"']) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "remote paths containing '\\' or '\"' are not supported",
        ));
    }
    Ok(path)
}

fn split_remote_path(path: &str) -> (&str, &str) {
    match path.rsplit_once('/') {
        Some(("", base)) => ("/", base),
        Some((parent, base)) => (parent, base),
        None => (".", path),
    }
}

/// Quotes one path for an sftp batch line. Quoted arguments are literal
/// (no glob expansion) in every position we use, and validation has
/// already rejected `"` and `\`, whose escape semantics vary across
/// commands and OpenSSH versions — so quoting is a plain wrap.
fn quote_batch_path(path: &str) -> String {
    format!("\"{path}\"")
}

/// Whether the sftp stderr describes a server-side file operation failure
/// (`Couldn't ...`/`Can't ...`) rather than a connection-level failure.
fn is_fs_level_error(message: &str) -> bool {
    message
        .lines()
        .any(|line| line.starts_with("Couldn't") || line.starts_with("Can't"))
}

fn fs_error_kind(message: &str) -> io::ErrorKind {
    let lower = message.to_ascii_lowercase();
    if lower.contains("no such file") || lower.contains("not found") {
        io::ErrorKind::NotFound
    } else if lower.contains("permission denied") {
        io::ErrorKind::PermissionDenied
    } else if lower.contains("not a directory") {
        io::ErrorKind::NotADirectory
    } else if lower.contains("is a directory") {
        io::ErrorKind::IsADirectory
    } else {
        io::ErrorKind::Other
    }
}

fn parse_long_listing(stdout: &[u8]) -> io::Result<Vec<RemoteDirEntry>> {
    let text = String::from_utf8_lossy(stdout);
    let mut entries = Vec::new();
    let mut skipped = 0_usize;
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match parse_long_entry(line) {
            Some(entry) => entries.push(entry),
            None => skipped += 1,
        }
    }
    if entries.is_empty() && skipped > 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "remote listing is not in OpenSSH long format",
        ));
    }
    Ok(entries)
}

fn next_field<'a>(rest: &mut &'a str) -> Option<&'a str> {
    *rest = rest.trim_start();
    if rest.is_empty() {
        return None;
    }
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let field = &rest[..end];
    *rest = &rest[end..];
    Some(field)
}

/// Parses one OpenSSH sftp long-listing line:
/// `<mode> <nlink> <user> <group> <size> <mon> <day> <time|year> <name>`.
/// The name keeps its inner spaces; a symlink's ` -> target` suffix is
/// split off.
fn parse_long_entry(line: &str) -> Option<RemoteDirEntry> {
    let mut rest = line;
    let mode_field = next_field(&mut rest)?;
    if mode_field.len() != 10 {
        return None;
    }
    let _nlink = next_field(&mut rest)?;
    let _user = next_field(&mut rest)?;
    let _group = next_field(&mut rest)?;
    let size: u64 = next_field(&mut rest)?.parse().ok()?;
    let month = next_field(&mut rest)?;
    let day = next_field(&mut rest)?;
    let time = next_field(&mut rest)?;
    let name = rest.trim();
    if name.is_empty() {
        return None;
    }
    let kind = match mode_field.as_bytes()[0] {
        b'-' => RemoteEntryKind::File,
        b'd' => RemoteEntryKind::Directory,
        b'l' => RemoteEntryKind::Symlink,
        _ => RemoteEntryKind::Other,
    };
    let (name, link_target) = if kind == RemoteEntryKind::Symlink {
        match name.split_once(" -> ") {
            Some((name, target)) => (name, Some(target.to_string())),
            None => (name, None),
        }
    } else {
        (name, None)
    };
    Some(RemoteDirEntry {
        name: name.to_string(),
        kind,
        size,
        mode: parse_mode(mode_field),
        link_target,
        modified: format!("{month} {day} {time}"),
    })
}

/// Converts an `ls`-style mode string (`-rw-r--r--`) into permission bits,
/// including suid/sgid/sticky. Non-standard shapes yield `None`.
fn parse_mode(field: &str) -> Option<u32> {
    let bytes = field.as_bytes();
    if bytes.len() != 10 {
        return None;
    }
    let mut mode = 0_u32;
    for (index, &byte) in bytes[1..].iter().enumerate() {
        let bit = 8 - index as u32;
        let set = matches!(
            (index % 3, byte),
            (0, b'r') | (1, b'w') | (2, b'x' | b's' | b't')
        );
        if set {
            mode |= 1 << bit;
        }
    }
    if matches!(bytes[3], b's' | b'S') {
        mode |= 0o4000;
    }
    if matches!(bytes[6], b's' | b'S') {
        mode |= 0o2000;
    }
    if matches!(bytes[9], b't' | b'T') {
        mode |= 0o1000;
    }
    Some(mode)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// HERDR-MACH-004：一次性 sftp 通道不建控制主连接。每个操作都会新建受管
    /// 配置（ControlPath 因此每次不同），留着 `ControlMaster=auto` +
    /// `ControlPersist` 只会让每个操作在后台留下一个永不退出的 ssh master。
    #[test]
    fn one_off_sftp_channels_do_not_keep_a_control_master() {
        let profile =
            SavedSshEndpoint::new("fs-test", "user@example.invalid", "default").expect("profile");
        let fs = RemoteFs::connect(&profile).expect("connect");
        assert!(
            fs.config.options.control_path.is_none(),
            "一次性通道不带控制路径"
        );
    }

    #[test]
    fn quote_batch_path_wraps_in_double_quotes() {
        assert_eq!(quote_batch_path("/var/log/a b"), "\"/var/log/a b\"");
        assert_eq!(quote_batch_path("a*b?[c]"), "\"a*b?[c]\"");
    }

    #[test]
    fn validate_remote_path_rejects_injection_shapes() {
        assert!(validate_remote_path("/var/log").is_ok());
        assert!(validate_remote_path("relative/dir").is_ok());
        assert!(validate_remote_path("star*file?.txt").is_ok());
        for bad in [
            "",
            "a\nb",
            "a\rb",
            "-la",
            "--help",
            "back\\slash",
            "quote\"d",
        ] {
            assert!(
                validate_remote_path(bad).is_err(),
                "path must be rejected: {bad:?}"
            );
        }
    }

    #[test]
    fn split_remote_path_handles_roots_and_relatives() {
        assert_eq!(split_remote_path("/var/log"), ("/var", "log"));
        assert_eq!(split_remote_path("/file"), ("/", "file"));
        assert_eq!(split_remote_path("file"), (".", "file"));
        assert_eq!(split_remote_path("a/b/c"), ("a/b", "c"));
    }

    #[test]
    fn long_listing_parses_openssh_lines() {
        let stdout = b"-rw-r--r--    1 dev      dev          4096 Sep 17 10:03 plain.txt\ndrwxr-xr-x    3 dev      dev          4096 Sep 16  2025 dir with spaces\nlrwxrwxrwx    1 dev      dev            11 Jan  1  1970 link -> /target/x\n-rwsr-xr-x    1 root     root        84080 Feb  2  2024 suid tool\n";
        let entries = parse_long_listing(stdout).expect("valid listing");
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[0].name, "plain.txt");
        assert_eq!(entries[0].kind, RemoteEntryKind::File);
        assert_eq!(entries[0].size, 4096);
        assert_eq!(entries[0].mode, Some(0o644));
        assert_eq!(entries[0].modified, "Sep 17 10:03");
        assert_eq!(entries[1].name, "dir with spaces");
        assert_eq!(entries[1].kind, RemoteEntryKind::Directory);
        assert_eq!(entries[2].name, "link");
        assert_eq!(entries[2].kind, RemoteEntryKind::Symlink);
        assert_eq!(entries[2].link_target.as_deref(), Some("/target/x"));
        assert_eq!(entries[3].name, "suid tool");
        assert_eq!(entries[3].mode, Some(0o4755));
    }

    #[test]
    fn long_listing_rejects_unparseable_output_only_when_nothing_parsed() {
        let error = parse_long_listing(b"total 0\nrandom garbage\n").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        // A header line alongside valid entries is skipped, not fatal.
        let entries =
            parse_long_listing(b"total 1\n-rw-r--r--    1 a  a  1 Jan  1  1970 f\n").unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "f");
    }

    #[test]
    fn fs_level_detection_separates_server_failures_from_transport() {
        assert!(is_fs_level_error(
            "Couldn't stat remote file: No such file or directory"
        ));
        assert!(is_fs_level_error("Can't ls: \"/x\" not found"));
        assert!(!is_fs_level_error(
            "user@host: Permission denied (publickey)."
        ));
        assert!(!is_fs_level_error("Host key verification failed."));
    }

    #[test]
    fn fs_error_kind_maps_server_messages() {
        assert_eq!(
            fs_error_kind("Couldn't stat remote file: No such file or directory"),
            io::ErrorKind::NotFound
        );
        assert_eq!(
            fs_error_kind("Couldn't open remote file: Permission denied"),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            fs_error_kind("Couldn't read directory: Not a directory"),
            io::ErrorKind::NotADirectory
        );
        assert_eq!(
            fs_error_kind("Couldn't create directory: Failure"),
            io::ErrorKind::Other
        );
    }
}
