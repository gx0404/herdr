use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use super::{SessionHistorySnapshot, SessionSnapshot};

#[derive(PartialEq, Eq)]
struct RecoveryContents {
    length: u64,
    sha256: [u8; 32],
}

fn transfer_recovery(
    source: &mut impl io::Read,
    output: &mut impl io::Write,
) -> io::Result<RecoveryContents> {
    let mut digest = Sha256::new();
    let mut length = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = match source.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        };
        output.write_all(&buffer[..read])?;
        digest.update(&buffer[..read]);
        length = length
            .checked_add(read as u64)
            .ok_or_else(|| io::Error::other("recovery length overflow"))?;
    }
    Ok(RecoveryContents {
        length,
        sha256: digest.finalize().into(),
    })
}

struct PendingRecovery {
    path: PathBuf,
    owned: File,
    contents: RecoveryContents,
}

impl PendingRecovery {
    fn verify_file(&self, file: &mut File) -> io::Result<()> {
        use std::io::Seek;

        crate::platform::check_persist_source(file)?;
        let before = file.metadata()?;
        file.rewind()?;
        let read = transfer_recovery(file, &mut io::sink());
        let rewind = file.rewind();
        let contents = read?;
        rewind?;
        let after = file.metadata()?;
        if contents != self.contents
            || before.len() != after.len()
            || before.modified()? != after.modified()?
        {
            return Err(io::Error::other(
                "pending recovery content mismatch; safe re-backup or manual inspection required",
            ));
        }
        Ok(())
    }

    fn open_verified(&self) -> io::Result<File> {
        let owned = self.owned.metadata()?;
        if !owned.is_file()
            || owned.len() != self.contents.length
            || !std::fs::symlink_metadata(&self.path)?.is_file()
        {
            return Err(io::Error::other(
                "pending recovery is no longer a complete regular file",
            ));
        }
        let mut file = File::open(&self.path)?;
        if !crate::platform::same_persist_file(&self.owned, &file)? {
            return Err(io::Error::other(
                "pending recovery identity mismatch; safe re-backup or manual inspection required",
            ));
        }
        self.verify_file(&mut file)?;
        Ok(file)
    }

    fn verify_source(&self, path: &Path, source: &mut File) -> io::Result<()> {
        self.verify_file(source)?;
        self.verify_file(&mut File::open(path)?)
    }
}

#[derive(Default)]
struct RecoveryDirectory {
    confirmed: bool,
    pending: Option<PendingRecovery>,
}

impl RecoveryDirectory {
    fn confirm_existing(
        &mut self,
        directory: &Path,
        existing: &[(u128, PathBuf)],
        mut source: Option<(&Path, &mut File)>,
    ) -> io::Result<Option<PathBuf>> {
        let mut phase = "recovery.retry_validate";
        let result = (|| -> io::Result<Option<PathBuf>> {
            let mut verified = self
                .pending
                .as_ref()
                .map(PendingRecovery::open_verified)
                .transpose()?;
            if let (Some(pending), Some((path, source))) = (&self.pending, source.as_mut()) {
                pending.verify_source(path, source)?;
            }
            if !self.confirmed && (!existing.is_empty() || self.pending.is_some()) {
                #[cfg(test)]
                let probe = self
                    .pending
                    .as_ref()
                    .map_or(directory, |pending| pending.path.as_path());
                phase = "recovery.retry_directory_sync";
                #[cfg(test)]
                super::io::test_io::check(phase, probe)?;
                crate::platform::sync_directory(directory)?;
                phase = "recovery.retry_parent_sync";
                #[cfg(test)]
                super::io::test_io::check(phase, probe)?;
                crate::platform::sync_directory(
                    directory
                        .parent()
                        .filter(|parent| !parent.as_os_str().is_empty())
                        .unwrap_or_else(|| Path::new(".")),
                )?;
            }
            phase = "recovery.retry_validate";
            if let Some(pending) = &self.pending {
                if let Some(file) = &mut verified {
                    pending.verify_file(file)?;
                }
                pending.open_verified()?;
                if let Some((path, source)) = source.as_mut() {
                    pending.verify_source(path, source)?;
                }
            }
            self.confirmed = true;
            Ok(self.pending.as_ref().map(|pending| pending.path.clone()))
        })();
        if result.is_err() {
            self.confirmed = false;
        }
        result.map_err(|err| {
            io::Error::new(err.kind(), format!("{phase} failed (durable=false): {err}"))
        })
    }
}

/// Shared by autosave, pane-exit checkpoints, and shutdown.
pub(crate) struct SessionWriter {
    path: PathBuf,
    protect_unloaded: bool,
    backup_sync: RecoveryDirectory,
    snapshot_sync: RecoveryDirectory,
}

impl SessionWriter {
    pub(crate) fn new(protect_unloaded: bool) -> Self {
        Self {
            path: super::io::session_path(),
            protect_unloaded,
            backup_sync: RecoveryDirectory::default(),
            snapshot_sync: RecoveryDirectory::default(),
        }
    }

    fn preserve_unloaded(&mut self) -> io::Result<()> {
        if self.protect_unloaded && preserve_existing(&self.path, &mut self.backup_sync)? {
            self.protect_unloaded = false;
        }
        Ok(())
    }

    fn preserve_snapshot_history(&mut self) {
        if let Err(err) = preserve_snapshot_history(&self.path, &mut self.snapshot_sync) {
            tracing::warn!(
                event = "persist.snapshot", outcome = "error", path = %self.path.display(),
                err = %err, "failed to preserve session snapshot"
            );
        }
    }

    pub(crate) fn save(
        &mut self,
        snapshot: &SessionSnapshot,
        history: Option<&SessionHistorySnapshot>,
    ) {
        if let Err(err) = self.preserve_unloaded() {
            crate::logging::session_save_failed(&self.path, &err.to_string());
            return;
        }
        self.preserve_snapshot_history();
        if let Err(err) = super::io::save_to_path(&self.path, snapshot) {
            if err.replaced {
                self.protect_unloaded = false;
            }
            crate::logging::session_save_failed(&self.path, &err.to_string());
            return;
        }
        // Optional history failure must not reclassify our committed layout as unloaded.
        self.protect_unloaded = false;
        self.preserve_snapshot_history();
        let history_path = self.path.with_file_name("session-history.json");
        if let Err(err) = super::io::save_history_to_path(&history_path, history) {
            crate::logging::session_save_failed(&history_path, &err.to_string());
        }
        crate::logging::session_saved(&self.path, snapshot.workspaces.len());
    }

    pub(crate) fn clear(&mut self) {
        let result = self.preserve_unloaded().and_then(|()| {
            self.preserve_snapshot_history();
            super::io::clear_path(&self.path)
        });
        if let Err(err) = result {
            crate::logging::session_clear_failed(&self.path, &err.to_string());
            return;
        }
        let history_path = self.path.with_file_name("session-history.json");
        if let Err(err) = super::io::clear_path(&history_path) {
            crate::logging::session_clear_failed(&history_path, &err.to_string());
        }
        crate::logging::session_cleared(&self.path);
    }
}

const SNAPSHOT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15 * 60);
const SNAPSHOT_LIMIT: usize = 48;

fn preserve_snapshot_history(path: &Path, sync: &mut RecoveryDirectory) -> io::Result<()> {
    let directory = path.with_file_name("session-snapshots");
    let existing = match recovery_files(&directory) {
        Ok(files) => files,
        Err(err) if err.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(err) => return Err(err),
    };
    if let Some(backup) = sync.confirm_existing(&directory, &existing, None)? {
        finish_recovery(
            path,
            "session-snapshots",
            &backup,
            &existing,
            SNAPSHOT_LIMIT,
            sync,
        )?;
    } else if existing.len() > SNAPSHOT_LIMIT {
        prune_backups(&existing, SNAPSHOT_LIMIT + 1)?;
    }
    if let Some((_, latest)) = existing.last() {
        let modified = std::fs::metadata(latest)?.modified()?;
        if SystemTime::now()
            .duration_since(modified)
            .is_ok_and(|age| age < SNAPSHOT_INTERVAL)
        {
            return Ok(());
        }
    }
    let mut source = match File::open(path) {
        Ok(source) => source,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    crate::platform::check_persist_source(&source)?;
    let mut bytes = Vec::new();
    io::Read::read_to_end(&mut source, &mut bytes)?;
    let Ok(snapshot) = serde_json::from_slice::<SessionSnapshot>(&bytes) else {
        return Ok(());
    };
    if snapshot.version > super::snapshot::SNAPSHOT_VERSION || snapshot.workspaces.is_empty() {
        return Ok(());
    }
    if let Some((_, latest)) = existing.last() {
        let previous_bytes = std::fs::read(latest)?;
        if let Ok(previous) = serde_json::from_slice::<SessionSnapshot>(&previous_bytes) {
            if super::snapshot::layout_fingerprint(&snapshot).is_some_and(|fingerprint| {
                super::snapshot::layout_fingerprint(&previous).as_ref() == Some(&fingerprint)
            }) {
                return Ok(());
            }
        }
    }
    preserve_existing_in(path, "session-snapshots", SNAPSHOT_LIMIT, sync)?;
    Ok(())
}

fn preserve_existing(path: &Path, sync: &mut RecoveryDirectory) -> io::Result<bool> {
    preserve_existing_in(path, "session-backups", 3, sync)
}

fn preserve_existing_in(
    path: &Path,
    directory_name: &str,
    keep: usize,
    sync: &mut RecoveryDirectory,
) -> io::Result<bool> {
    use std::io::Seek;

    let mut source = match File::open(path) {
        Ok(file) => file,
        // Recheck on the next mutation until a fresh session is actually saved.
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err),
    };
    crate::platform::check_persist_source(&source)?;
    let directory = path.with_file_name(directory_name);
    std::fs::create_dir_all(&directory)?;
    let older = recovery_files(&directory)?;
    if let Some(backup) = sync.confirm_existing(&directory, &older, Some((path, &mut source)))? {
        return finish_recovery(path, directory_name, &backup, &older, keep, sync);
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    // Keep creation order even when the wall clock moves backwards.
    let timestamp = match older.last() {
        Some((previous, _)) => now.max(
            previous
                .checked_add(1)
                .ok_or_else(|| io::Error::other("session recovery sequence exhausted"))?,
        ),
        None => now,
    };
    for sequence in 0..128 {
        let backup = directory.join(format!(
            "session-{timestamp:039}-{}-{sequence}.json",
            std::process::id()
        ));
        source.rewind()?;
        match copy_recovery(&mut source, &backup, sync) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
        sync.confirm_existing(&directory, &older, Some((path, &mut source)))?;
        return finish_recovery(path, directory_name, &backup, &older, keep, sync);
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate session recovery copy",
    ))
}

fn finish_recovery(
    path: &Path,
    directory_name: &str,
    backup: &Path,
    existing: &[(u128, PathBuf)],
    keep: usize,
    sync: &mut RecoveryDirectory,
) -> io::Result<bool> {
    let older: Vec<_> = existing
        .iter()
        .filter(|(_, candidate)| candidate != backup)
        .cloned()
        .collect();
    if let Err(err) = prune_backups(&older, keep) {
        if directory_name == "session-snapshots" {
            return Err(err);
        }
        let directory = path.with_file_name(directory_name);
        tracing::warn!(
            event = "persist.backup", subsystem = "persist", outcome = "prune_error",
            path = %directory.display(), err = %err, "failed to prune session recovery copies"
        );
    }
    sync.pending = None;
    tracing::info!(
        event = "persist.backup", subsystem = "persist", outcome = "ok",
        path = %path.display(), backup_path = %backup.display(),
        "preserved session recovery copy"
    );
    Ok(true)
}

fn copy_recovery(
    source: &mut impl io::Read,
    backup: &Path,
    sync: &mut RecoveryDirectory,
) -> io::Result<()> {
    let directory = backup
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let pending = backup.with_extension("pending");
    let mut output = crate::platform::create_persist_temporary(&pending)?;
    let mut published = false;
    let mut phase = "recovery.check_destination";
    let result = (|| -> io::Result<()> {
        match std::fs::symlink_metadata(backup) {
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "recovery copy already exists",
                ))
            }
        }
        phase = "recovery.copy";
        #[cfg(test)]
        super::io::test_io::check(phase, backup)?;
        let contents = transfer_recovery(source, &mut output)?;
        phase = "recovery.file_sync";
        #[cfg(test)]
        super::io::test_io::check(phase, backup)?;
        output.sync_all()?;
        let owned = output.try_clone()?;
        phase = "recovery.publish";
        #[cfg(test)]
        super::io::test_io::check(phase, backup)?;
        // This primitive must never return Err after installing the destination.
        crate::platform::publish_persist_recovery(&pending, backup)?;
        published = true;
        sync.confirmed = false;
        sync.pending = Some(PendingRecovery {
            path: backup.to_path_buf(),
            owned,
            contents,
        });
        phase = "recovery.directory_sync";
        #[cfg(test)]
        super::io::test_io::check(phase, backup)?;
        crate::platform::sync_directory(directory)?;
        phase = "recovery.parent_sync";
        #[cfg(test)]
        super::io::test_io::check(phase, backup)?;
        crate::platform::sync_directory(
            directory
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new(".")),
        )?;
        sync.confirmed = true;
        Ok(())
    })();
    if let Err(error) = result {
        let mut cleanup = Vec::new();
        if !published {
            if let Err(err) = crate::platform::discard_persist_temporary(&output, &pending) {
                cleanup.push(format!("{}: {err}", pending.display()));
            }
        }
        let kind = if published || !cleanup.is_empty() {
            io::ErrorKind::Other
        } else {
            error.kind()
        };
        let mut message = format!("{phase} failed (published={published}, durable=false): {error}");
        if published {
            message.push_str("; published recovery retained for directory-sync retry");
        }
        if !cleanup.is_empty() {
            message.push_str(&format!(
                "; recovery cleanup failed: {}",
                cleanup.join("; ")
            ));
        }
        return Err(io::Error::new(kind, message));
    }
    Ok(())
}

fn recovery_files(directory: &Path) -> io::Result<Vec<(u128, PathBuf)>> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            if let Some(timestamp) = entry.file_name().to_str().and_then(recovery_timestamp) {
                files.push((timestamp, entry.path()));
            }
        }
    }
    files.sort();
    Ok(files)
}

fn prune_backups(older: &[(u128, PathBuf)], keep: usize) -> io::Result<()> {
    // The new copy is durable before any of the previous copies are removed.
    let mut remaining = older.len().saturating_sub(keep.saturating_sub(1));
    let mut failure = None;
    for (_, path) in older {
        if remaining == 0 {
            return Ok(());
        }
        match std::fs::remove_file(path) {
            Ok(()) => remaining -= 1,
            Err(err) => failure = Some(err),
        }
    }
    if remaining > 0 {
        if let Some(err) = failure {
            return Err(err);
        }
    }
    Ok(())
}

fn recovery_timestamp(name: &str) -> Option<u128> {
    let fields = name.strip_prefix("session-")?.strip_suffix(".json")?;
    let fields: Vec<_> = fields.split('-').collect();
    if fields.len() == 3
        && fields[0].len() == 39
        && fields
            .iter()
            .all(|field| !field.is_empty() && field.bytes().all(|byte| byte.is_ascii_digit()))
    {
        fields[0].parse().ok()
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn writer(protect_unloaded: bool) -> SessionWriter {
        // 时间戳在并发的测试线程间会撞，再带进程内序号。
        let directory = std::env::temp_dir().join(format!(
            "herdr-session-recovery-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            crate::config::test_dirs::unique_id()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        SessionWriter {
            path: directory.join("session.json"),
            protect_unloaded,
            backup_sync: RecoveryDirectory::default(),
            snapshot_sync: RecoveryDirectory::default(),
        }
    }

    fn snapshot() -> SessionSnapshot {
        serde_json::from_str(include_str!(
            "../../tests/fixtures/session/current-herdr-session.json"
        ))
        .unwrap()
    }

    fn isolated_writer(
        protect_unloaded: bool,
    ) -> (SessionWriter, crate::config::test_dirs::TempDir) {
        let directory = crate::config::test_dirs::TempDir::new("persist-writer");
        let writer = SessionWriter {
            path: directory.join("session.json"),
            protect_unloaded,
            backup_sync: RecoveryDirectory::default(),
            snapshot_sync: RecoveryDirectory::default(),
        };
        (writer, directory)
    }

    fn with_failure<R>(phase: &'static str, run: impl FnOnce() -> R) -> R {
        super::super::io::test_io::with(
            Box::new(move |current, _| {
                let retry = current.strip_prefix("recovery.retry_");
                if current == phase
                    || retry.is_some_and(|suffix| phase.strip_prefix("recovery.") == Some(suffix))
                {
                    Err(io::Error::other("injected persistence failure"))
                } else {
                    Ok(())
                }
            }),
            run,
        )
    }

    fn capture_logs(run: impl FnOnce()) -> String {
        #[derive(Clone, Default)]
        struct Buffer(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

        impl io::Write for Buffer {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Buffer {
            type Writer = Buffer;

            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        let buffer = Buffer::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buffer.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .finish();
        tracing::subscriber::with_default(subscriber, run);
        let bytes = buffer.0.lock().unwrap().clone();
        String::from_utf8(bytes).unwrap()
    }

    /// Windows 上杀毒软件会在新文件落盘后短暂打开它扫描：这期间读它报共享冲突（os error
    /// 32），删掉的文件也要等扫描放手才从目录里消失（删除挂起）。测试在几毫秒内接连改同一批
    /// 文件，所以观察目录与文件时等到状态稳定（有时限），不只看一眼。
    const SETTLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
    const SETTLE_POLL: std::time::Duration = std::time::Duration::from_millis(10);

    /// 收尾删测试目录：删不掉就稍等重试；到时限仍删不掉只是临时目录残留，不算测试失败。
    fn remove_test_dir(writer: &SessionWriter) {
        let directory = writer.path.parent().unwrap();
        let deadline = std::time::Instant::now() + SETTLE_TIMEOUT;
        while std::fs::remove_dir_all(directory).is_err()
            && directory.exists()
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(SETTLE_POLL);
        }
    }

    /// 目录条目数等到 `expected`（被修剪的副本在扫描放手前仍列在目录里）；到时限返回最后
    /// 看到的数，由调用方断言。
    fn settled_entry_count(directory: &Path, expected: usize) -> usize {
        let deadline = std::time::Instant::now() + SETTLE_TIMEOUT;
        loop {
            let count = std::fs::read_dir(directory).unwrap().count();
            if count == expected || std::time::Instant::now() >= deadline {
                return count;
            }
            std::thread::sleep(SETTLE_POLL);
        }
    }

    fn backups(writer: &SessionWriter) -> Vec<Vec<u8>> {
        let directory = writer.path.with_file_name("session-backups");
        if !directory.exists() {
            return Vec::new();
        }
        let deadline = std::time::Instant::now() + SETTLE_TIMEOUT;
        loop {
            let mut entries: Vec<_> = std::fs::read_dir(&directory)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect();
            entries.sort();
            // 读不开的条目要么正被扫描，要么是删除挂起、稍后就消失：整体重新列一遍再读。
            match entries.iter().map(std::fs::read).collect::<io::Result<_>>() {
                Ok(contents) => return contents,
                Err(err) if std::time::Instant::now() >= deadline => {
                    panic!("failed to read recovery copies: {err}")
                }
                Err(_) => std::thread::sleep(SETTLE_POLL),
            }
        }
    }

    fn snapshots(writer: &SessionWriter) -> Vec<(u128, PathBuf)> {
        recovery_files(&writer.path.with_file_name("session-snapshots")).unwrap()
    }

    #[test]
    fn snapshot_survives_exit_bursts_clears_and_writer_restarts() {
        let mut writer = writer(false);
        let original = snapshot();
        writer.save(&original, None);
        let files = snapshots(&writer);
        assert_eq!(files.len(), 1);
        let saved = std::fs::read(&files[0].1).unwrap();
        for i in 0..100 {
            let mut shrinking = snapshot();
            shrinking.workspaces[0].custom_name = Some(format!("remaining pane {i}"));
            writer.save(&shrinking, None);
            writer = SessionWriter {
                path: writer.path.clone(),
                protect_unloaded: false,
                backup_sync: RecoveryDirectory::default(),
                snapshot_sync: RecoveryDirectory::default(),
            };
        }
        writer.clear();
        assert!(
            !writer.path.exists(),
            "intentional clear must still persist"
        );
        assert_eq!(snapshots(&writer), files);
        assert_eq!(std::fs::read(&files[0].1).unwrap(), saved);
        assert!(backups(&writer).is_empty());
        remove_test_dir(&writer);
    }

    #[test]
    fn snapshot_history_is_bounded_and_does_not_rotate_identical_layouts() {
        let mut writer = writer(false);
        let directory = writer.path.with_file_name("session-snapshots");
        std::fs::create_dir(&directory).unwrap();
        let manual = directory.join("my-layout.json");
        std::fs::write(&manual, b"manual").unwrap();
        for i in 0..SNAPSHOT_LIMIT {
            std::fs::write(
                directory.join(format!("session-{i:039}-1-0.json")),
                b"old snapshot",
            )
            .unwrap();
        }
        for (_, path) in snapshots(&writer) {
            File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(UNIX_EPOCH))
                .unwrap();
        }
        writer.save(&snapshot(), None);
        assert_eq!(snapshots(&writer).len(), SNAPSHOT_LIMIT);
        assert!(!directory
            .join(format!("session-{:039}-1-0.json", 0))
            .exists());
        assert!(manual.exists());

        for (_, path) in snapshots(&writer) {
            std::fs::remove_file(path).unwrap();
        }
        let old = directory.join(format!("session-{:039}-1-0.json", 1));
        let saved = std::fs::read(&writer.path).unwrap();
        let reordered: serde_json::Value = serde_json::from_slice(&saved).unwrap();
        let equivalent = serde_json::to_vec(&reordered).unwrap();
        assert_ne!(saved, equivalent);
        std::fs::write(&old, equivalent).unwrap();
        File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(UNIX_EPOCH))
            .unwrap();
        writer.save(&snapshot(), None);
        assert_eq!(snapshots(&writer), vec![(1, old)]);
        remove_test_dir(&writer);
    }

    #[test]
    fn snapshot_cadence_recovers_after_clock_rollback_and_restart() {
        let mut writer = writer(false);
        writer.save(&snapshot(), None);
        let file = snapshots(&writer).pop().unwrap().1;
        File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_times(
                std::fs::FileTimes::new()
                    .set_modified(SystemTime::now() + std::time::Duration::from_secs(86400)),
            )
            .unwrap();
        let mut changed = snapshot();
        changed.workspaces[0].custom_name = Some("after clock rollback".into());
        writer.save(&changed, None);
        assert_eq!(snapshots(&writer).len(), 2);
        writer = SessionWriter {
            path: writer.path.clone(),
            protect_unloaded: false,
            backup_sync: RecoveryDirectory::default(),
            snapshot_sync: RecoveryDirectory::default(),
        };
        changed.workspaces[0].custom_name = Some("after restart".into());
        writer.save(&changed, None);
        assert_eq!(
            snapshots(&writer).len(),
            2,
            "new mtime restores cadence across restart"
        );
        remove_test_dir(&writer);
    }

    #[test]
    fn pruning_continues_past_an_undeletable_entry() {
        let writer = writer(false);
        let locked = writer.path.with_file_name("undeletable");
        std::fs::create_dir(&locked).unwrap();
        let removable = writer.path.with_file_name("removable");
        std::fs::write(&removable, b"old").unwrap();
        assert!(prune_backups(&[(1, locked.clone()), (2, removable.clone())], 2).is_ok());
        assert!(locked.exists());
        assert!(!removable.exists());
        assert!(prune_backups(&[(1, locked)], 1).is_err());
        remove_test_dir(&writer);
    }

    #[test]
    fn snapshot_failure_does_not_block_primary_save_and_clear() {
        let mut writer = writer(false);
        std::fs::write(writer.path.with_file_name("session-snapshots"), b"blocked").unwrap();
        writer.save(&snapshot(), None);
        assert!(writer.path.exists());
        writer.clear();
        assert!(!writer.path.exists());
        remove_test_dir(&writer);
    }

    #[test]
    fn healthy_and_fresh_sessions_save_and_clear_without_backups() {
        for protect_unloaded in [false, true] {
            let mut writer = writer(protect_unloaded);
            if !protect_unloaded {
                super::super::io::save_to_path(&writer.path, &snapshot()).unwrap();
            }
            writer.save(&snapshot(), None);
            assert!(!writer.protect_unloaded);
            assert!(writer.path.exists());
            writer.save(&snapshot(), None);
            writer.clear();
            assert!(!writer.path.exists());
            assert!(backups(&writer).is_empty());
            remove_test_dir(&writer);
        }
    }

    #[test]
    fn failed_recovery_blocks_mutations_then_retries_preserving_exact_bytes_once() {
        let original = b"invalid utf8 \xff";
        let mut writer = writer(true);
        std::fs::write(&writer.path, original).unwrap();
        let history_path = writer.path.with_file_name("session-history.json");
        std::fs::write(&history_path, b"history").unwrap();
        let directory = writer.path.with_file_name("session-backups");
        std::fs::write(&directory, b"blocks recovery").unwrap();
        writer.save(&snapshot(), None);
        writer.clear();
        assert!(writer.protect_unloaded);
        assert_eq!(std::fs::read(&writer.path).unwrap(), original);
        assert_eq!(std::fs::read(&history_path).unwrap(), b"history");

        std::fs::remove_file(&directory).unwrap();
        writer.save(&snapshot(), None);
        assert!(!writer.protect_unloaded);
        writer.save(&snapshot(), None);
        writer.clear();
        assert!(!writer.path.exists());
        assert_eq!(backups(&writer), vec![original.to_vec()]);
        remove_test_dir(&writer);
    }

    #[test]
    fn optional_history_failure_does_not_block_later_layout_saves() {
        let mut writer = writer(true);
        let history = writer.path.with_file_name("session-history.json");
        std::fs::create_dir(&history).unwrap();
        std::fs::write(
            writer.path.with_file_name("session-backups"),
            b"unavailable",
        )
        .unwrap();
        writer.save(&snapshot(), None);
        assert!(
            !writer.protect_unloaded,
            "structural session was saved successfully"
        );
        let mut changed = snapshot();
        changed.workspaces[0].custom_name = Some("latest layout".into());
        writer.save(&changed, None);
        let saved: SessionSnapshot =
            serde_json::from_slice(&std::fs::read(&writer.path).unwrap()).unwrap();
        assert_eq!(
            saved.workspaces[0].custom_name.as_deref(),
            Some("latest layout")
        );
        remove_test_dir(&writer);
    }

    #[test]
    fn pruning_leaves_user_named_recovery_files_alone() {
        let mut writer = writer(true);
        let directory = writer.path.with_file_name("session-backups");
        std::fs::create_dir(&directory).unwrap();
        let manual = directory.join("session-000-manual.json");
        std::fs::write(&manual, b"manual recovery copy").unwrap();
        for i in 0..5u8 {
            writer.protect_unloaded = true;
            std::fs::write(&writer.path, [i]).unwrap();
            writer.save(&snapshot(), None);
        }
        assert_eq!(std::fs::read(manual).unwrap(), b"manual recovery copy");
        assert_eq!(settled_entry_count(&directory, 4), 4);
        remove_test_dir(&writer);
    }

    #[test]
    fn first_clear_preserves_an_unloaded_file_even_after_an_earlier_missing_clear() {
        let mut writer = writer(true);
        writer.clear();
        assert!(writer.protect_unloaded);
        std::fs::write(&writer.path, b"late layout").unwrap();
        writer.clear();
        assert!(!writer.path.exists());
        assert_eq!(backups(&writer), vec![b"late layout".to_vec()]);
        remove_test_dir(&writer);
    }

    #[test]
    fn repeated_failed_saves_do_not_replace_a_completed_recovery_copy() {
        for phase in ["save.write", "save.file_sync", "save.rename"] {
            let (mut writer, _directory) = isolated_writer(true);
            std::fs::write(&writer.path, b"original").unwrap();
            let history = writer.path.with_file_name("session-history.json");
            std::fs::write(&history, b"history").unwrap();
            let logs = capture_logs(|| {
                with_failure(phase, || {
                    writer.save(&snapshot(), None);
                    assert!(!writer.protect_unloaded);
                    writer.save(&snapshot(), None);
                });
            });
            assert!(logs.contains(phase));
            assert!(logs.contains("replaced=false"));
            assert!(!logs.contains("session saved"));
            assert_eq!(std::fs::read(&writer.path).unwrap(), b"original");
            assert_eq!(std::fs::read(&history).unwrap(), b"history");
            writer.save(&snapshot(), None);
            assert_eq!(backups(&writer), vec![b"original".to_vec()]);
        }
    }

    #[test]
    fn missing_original_precommit_failure_still_protects_a_late_file() {
        for phase in ["save.write", "save.file_sync", "save.rename"] {
            let (mut writer, _directory) = isolated_writer(true);
            let history = writer.path.with_file_name("session-history.json");
            std::fs::write(&history, b"history").unwrap();
            with_failure(phase, || writer.save(&snapshot(), None));
            assert!(writer.protect_unloaded);
            assert!(!writer.path.exists());
            assert_eq!(std::fs::read(&history).unwrap(), b"history");
            assert!(backups(&writer).is_empty());
            std::fs::write(&writer.path, b"late original").unwrap();
            writer.save(&snapshot(), None);
            assert!(!writer.protect_unloaded);
            assert_eq!(backups(&writer), vec![b"late original".to_vec()]);
        }
    }

    #[test]
    fn committed_directory_sync_failure_keeps_layout_and_skips_history() {
        for existed in [false, true] {
            let (mut writer, _directory) = isolated_writer(true);
            if existed {
                std::fs::write(&writer.path, b"unloaded original").unwrap();
            }
            let history = writer.path.with_file_name("session-history.json");
            std::fs::write(&history, b"history must not be cleared").unwrap();
            let logs = capture_logs(|| {
                with_failure("save.directory_sync", || writer.save(&snapshot(), None));
            });
            assert!(!writer.protect_unloaded);
            assert_eq!(
                std::fs::read(&writer.path).unwrap(),
                serde_json::to_vec_pretty(&snapshot()).unwrap()
            );
            assert_eq!(
                std::fs::read(&history).unwrap(),
                b"history must not be cleared"
            );
            assert!(!writer.path.with_file_name("session-snapshots").exists());
            assert!(logs.contains("save.directory_sync"));
            assert!(logs.contains("replaced=true"));
            assert!(logs.contains("failed to save session"));
            assert!(!logs.contains("session saved"));
            writer.save(&snapshot(), None);
            assert_eq!(backups(&writer).len(), usize::from(existed));
        }
    }

    #[test]
    fn optional_history_write_failure_never_reprotects_the_layout() {
        let (mut writer, _directory) = isolated_writer(true);
        let history_path = writer.path.with_file_name("session-history.json");
        std::fs::write(&history_path, b"old history").unwrap();
        let failed_path = history_path.clone();
        let history = SessionHistorySnapshot {
            version: super::super::snapshot::SNAPSHOT_VERSION,
            layout_fingerprint: None,
            workspaces: vec![],
        };
        let logs = capture_logs(|| {
            super::super::io::test_io::with(
                Box::new(move |phase, path| {
                    if phase == "save.write" && path == failed_path {
                        Err(io::Error::other("history unavailable"))
                    } else {
                        Ok(())
                    }
                }),
                || writer.save(&snapshot(), Some(&history)),
            );
        });
        assert!(!writer.protect_unloaded);
        assert_eq!(std::fs::read(&history_path).unwrap(), b"old history");
        assert!(logs.contains("history unavailable"));
        assert!(logs.contains("session saved"));
        let mut changed = snapshot();
        changed.workspaces[0].custom_name = Some("next layout".into());
        writer.save(&changed, Some(&history));
        assert_eq!(
            std::fs::read(&writer.path).unwrap(),
            serde_json::to_vec_pretty(&changed).unwrap()
        );
        assert!(backups(&writer).is_empty());
    }

    #[test]
    fn recovery_failures_preserve_old_copies_and_block_save_and_clear() {
        for phase in [
            "recovery.copy",
            "recovery.file_sync",
            "recovery.publish",
            "recovery.directory_sync",
            "recovery.parent_sync",
        ] {
            let (mut writer, _root) = isolated_writer(true);
            let directory = writer.path.with_file_name("session-backups");
            std::fs::create_dir(&directory).unwrap();
            for index in 0..3u8 {
                std::fs::write(
                    directory.join(format!("session-{:039}-1-0.json", index)),
                    [index],
                )
                .unwrap();
            }
            let older = recovery_files(&directory).unwrap();
            writer
                .backup_sync
                .confirm_existing(&directory, &older, None)
                .unwrap();
            std::fs::write(&writer.path, b"unloaded \xff").unwrap();
            let history = writer.path.with_file_name("session-history.json");
            std::fs::write(&history, b"history").unwrap();
            let logs = capture_logs(|| {
                with_failure(phase, || {
                    writer.save(&snapshot(), None);
                    writer.clear();
                });
            });
            assert!(writer.protect_unloaded);
            assert_eq!(std::fs::read(&writer.path).unwrap(), b"unloaded \xff");
            assert_eq!(std::fs::read(&history).unwrap(), b"history");
            for (index, (_, path)) in older.iter().enumerate() {
                assert_eq!(std::fs::read(path).unwrap(), vec![index as u8]);
            }
            let published = matches!(phase, "recovery.directory_sync" | "recovery.parent_sync");
            let expected = 3 + usize::from(published);
            assert_eq!(settled_entry_count(&directory, expected), expected);
            assert_eq!(writer.backup_sync.pending.is_some(), published);
            assert!(logs.contains(phase));
            assert!(logs.contains("durable=false"));
            assert!(logs.contains(&format!("published={published}")));
            assert!(!logs.contains("preserved session recovery copy"));
            assert!(!logs.contains("session saved"));
        }
    }

    #[test]
    fn periodic_recovery_sync_failure_never_prunes_old_snapshots() {
        use std::cell::RefCell;
        use std::rc::Rc;

        for phase in ["recovery.directory_sync", "recovery.parent_sync"] {
            let (mut writer, _root) = isolated_writer(false);
            let directory = writer.path.with_file_name("session-snapshots");
            std::fs::create_dir(&directory).unwrap();
            for index in 0..SNAPSHOT_LIMIT {
                let path = directory.join(format!("session-{index:039}-1-0.json"));
                std::fs::write(&path, b"old snapshot").unwrap();
                File::options()
                    .write(true)
                    .open(&path)
                    .unwrap()
                    .set_times(std::fs::FileTimes::new().set_modified(UNIX_EPOCH))
                    .unwrap();
            }
            std::fs::write(
                &writer.path,
                serde_json::to_vec_pretty(&snapshot()).unwrap(),
            )
            .unwrap();
            let older = recovery_files(&directory).unwrap();
            writer
                .snapshot_sync
                .confirm_existing(&directory, &older, None)
                .unwrap();
            let err = with_failure(phase, || {
                preserve_snapshot_history(&writer.path, &mut writer.snapshot_sync)
            })
            .unwrap_err();
            assert!(err.to_string().contains("durable=false"));
            assert!(err.to_string().contains("retained"));
            assert!(!writer.snapshot_sync.confirmed);
            let published = writer.snapshot_sync.pending.as_ref().unwrap().path.clone();
            assert_eq!(
                std::fs::read(&published).unwrap(),
                std::fs::read(&writer.path).unwrap()
            );
            let failed_files = recovery_files(&directory).unwrap();
            assert_eq!(failed_files.len(), SNAPSHOT_LIMIT + 1);
            for _ in 0..3 {
                with_failure(phase, || writer.save(&snapshot(), None));
                assert_eq!(recovery_files(&directory).unwrap(), failed_files);
                assert!(!writer.snapshot_sync.confirmed);
                for (_, path) in &older {
                    assert_eq!(std::fs::read(path).unwrap(), b"old snapshot");
                }
            }
            let attempts = Rc::new(RefCell::new(Vec::new()));
            let observed = attempts.clone();
            super::super::io::test_io::with(
                Box::new(move |phase, _| {
                    if phase.starts_with("recovery.") {
                        observed.borrow_mut().push(phase.to_string());
                    }
                    Ok(())
                }),
                || {
                    writer.save(&snapshot(), None);
                    writer.save(&snapshot(), None);
                },
            );
            assert_eq!(
                *attempts.borrow(),
                [
                    "recovery.retry_directory_sync",
                    "recovery.retry_parent_sync"
                ]
            );
            assert!(writer.snapshot_sync.confirmed);
            assert!(writer.snapshot_sync.pending.is_none());
            assert_eq!(snapshots(&writer).len(), SNAPSHOT_LIMIT);
            assert!(published.exists());
            assert!(!older[0].1.exists());
        }
    }

    #[test]
    fn published_recovery_sync_failure_never_deletes_a_replaced_name() {
        let (writer, _root) = isolated_writer(true);
        let backup = writer
            .path
            .with_file_name("session-000000000000000000000000000000000000001-1-0.json");
        let moved = backup.with_extension("retained");
        let mut sync = RecoveryDirectory::default();
        let err = super::super::io::test_io::with(
            Box::new(|phase, backup| {
                if phase == "recovery.directory_sync" {
                    std::fs::rename(backup, backup.with_extension("retained"))?;
                    std::fs::write(backup, b"foreign replacement")?;
                    return Err(io::Error::other("directory sync failed after name swap"));
                }
                Ok(())
            }),
            || {
                copy_recovery(
                    &mut io::Cursor::new(b"own complete bytes"),
                    &backup,
                    &mut sync,
                )
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("published=true, durable=false"));
        assert!(err.to_string().contains("retained"));
        assert_eq!(std::fs::read(&backup).unwrap(), b"foreign replacement");
        assert_eq!(std::fs::read(&moved).unwrap(), b"own complete bytes");
        assert!(!sync.confirmed);
        assert_eq!(
            sync.pending.as_ref().map(|pending| pending.path.as_path()),
            Some(backup.as_path())
        );
    }

    #[test]
    fn pending_recovery_replacement_never_releases_unloaded_protection() {
        for deleted in [false, true] {
            for during_sync in [false, true] {
                let (mut writer, _root) = isolated_writer(true);
                std::fs::write(&writer.path, b"snapshot A").unwrap();
                let history = writer.path.with_file_name("session-history.json");
                std::fs::write(&history, b"history").unwrap();
                with_failure("recovery.directory_sync", || writer.save(&snapshot(), None));
                let backup = writer.backup_sync.pending.as_ref().unwrap().path.clone();
                let replace = move |path: &Path| -> io::Result<()> {
                    if deleted {
                        std::fs::remove_file(path)
                    } else {
                        std::fs::rename(path, path.with_extension("retained"))?;
                        std::fs::write(path, b"snapshot X")
                    }
                };
                if !during_sync {
                    replace(&backup).unwrap();
                }
                let logs = capture_logs(|| {
                    if during_sync {
                        super::super::io::test_io::with(
                            Box::new(move |phase, path| {
                                if phase == "recovery.retry_directory_sync" {
                                    replace(path)?;
                                }
                                Ok(())
                            }),
                            || writer.save(&snapshot(), None),
                        );
                    } else {
                        writer.save(&snapshot(), None);
                    }
                    writer.clear();
                });
                assert!(writer.protect_unloaded);
                assert!(!writer.backup_sync.confirmed);
                assert!(writer.backup_sync.pending.is_some());
                assert_eq!(std::fs::read(&writer.path).unwrap(), b"snapshot A");
                assert_eq!(std::fs::read(&history).unwrap(), b"history");
                if !deleted {
                    assert_eq!(std::fs::read(&backup).unwrap(), b"snapshot X");
                    assert_eq!(
                        std::fs::read(backup.with_extension("retained")).unwrap(),
                        b"snapshot A"
                    );
                }
                assert!(logs.contains("recovery.retry_validate"));
                assert!(!logs.contains("preserved session recovery copy"));
                assert!(!logs.contains("session saved"));
                assert!(!logs.contains("session cleared"));
                drop(writer);
            }
        }
    }

    #[test]
    fn pending_recovery_equal_bytes_replacement_keeps_unloaded_protection() {
        for during_sync in [false, true] {
            let (mut writer, _root) = isolated_writer(true);
            std::fs::write(&writer.path, b"snapshot A").unwrap();
            let history = writer.path.with_file_name("session-history.json");
            std::fs::write(&history, b"history").unwrap();
            with_failure("recovery.directory_sync", || writer.save(&snapshot(), None));
            let backup = writer.backup_sync.pending.as_ref().unwrap().path.clone();
            let replace = |path: &Path| -> io::Result<()> {
                let replacement = path.with_extension("replacement");
                std::fs::write(&replacement, b"snapshot A")?;
                std::fs::rename(replacement, path)
            };
            if !during_sync {
                replace(&backup).unwrap();
            }
            let logs = capture_logs(|| {
                if during_sync {
                    super::super::io::test_io::with(
                        Box::new(move |phase, path| {
                            if phase == "recovery.retry_directory_sync" {
                                replace(path)?;
                            }
                            Ok(())
                        }),
                        || writer.save(&snapshot(), None),
                    );
                } else {
                    writer.save(&snapshot(), None);
                }
                writer.clear();
            });
            assert!(writer.protect_unloaded);
            assert!(!writer.backup_sync.confirmed);
            assert!(writer.backup_sync.pending.is_some());
            assert_eq!(std::fs::read(&writer.path).unwrap(), b"snapshot A");
            assert_eq!(std::fs::read(&history).unwrap(), b"history");
            assert_eq!(std::fs::read(&backup).unwrap(), b"snapshot A");
            assert!(logs.contains("pending recovery identity mismatch"));
            assert!(!logs.contains("preserved session recovery copy"));
            assert!(!logs.contains("session saved"));
            assert!(!logs.contains("session cleared"));
            drop(writer);
        }
    }

    #[test]
    fn pending_recovery_does_not_acknowledge_a_changed_source() {
        for during_sync in [false, true] {
            let (mut writer, _root) = isolated_writer(true);
            std::fs::write(&writer.path, b"snapshot A").unwrap();
            let history = writer.path.with_file_name("session-history.json");
            std::fs::write(&history, b"history").unwrap();
            with_failure("recovery.directory_sync", || writer.save(&snapshot(), None));
            let backup = writer.backup_sync.pending.as_ref().unwrap().path.clone();
            let source = writer.path.clone();
            let replace = move || -> io::Result<()> {
                let replacement = source.with_extension("replacement");
                std::fs::write(&replacement, b"snapshot B")?;
                std::fs::rename(replacement, &source)
            };
            if !during_sync {
                replace().unwrap();
            }
            let logs = capture_logs(|| {
                if during_sync {
                    super::super::io::test_io::with(
                        Box::new(move |phase, _| {
                            if phase == "recovery.retry_directory_sync" {
                                replace()?;
                            }
                            Ok(())
                        }),
                        || writer.save(&snapshot(), None),
                    );
                } else {
                    writer.save(&snapshot(), None);
                }
                writer.clear();
            });
            assert!(writer.protect_unloaded);
            assert!(!writer.backup_sync.confirmed);
            assert!(writer.backup_sync.pending.is_some());
            assert_eq!(std::fs::read(&writer.path).unwrap(), b"snapshot B");
            assert_eq!(std::fs::read(&history).unwrap(), b"history");
            assert_eq!(std::fs::read(&backup).unwrap(), b"snapshot A");
            assert_eq!(backups(&writer), vec![b"snapshot A".to_vec()]);
            assert!(logs.contains("safe re-backup or manual inspection required"));
            assert!(!logs.contains("preserved session recovery copy"));
            assert!(!logs.contains("session saved"));
            assert!(!logs.contains("session cleared"));
            drop(writer);
        }
    }

    #[test]
    fn unloaded_retry_confirms_the_same_copy_before_releasing_protection() {
        use std::cell::RefCell;
        use std::rc::Rc;

        for phase in ["recovery.directory_sync", "recovery.parent_sync"] {
            let (mut writer, _root) = isolated_writer(true);
            std::fs::write(&writer.path, b"original unloaded bytes").unwrap();
            let history = writer.path.with_file_name("session-history.json");
            std::fs::write(&history, b"history").unwrap();
            with_failure(phase, || writer.save(&snapshot(), None));
            let pending = writer.backup_sync.pending.as_ref().unwrap().path.clone();
            if phase == "recovery.parent_sync" {
                let equivalent = writer.path.with_extension("equivalent");
                std::fs::write(&equivalent, b"original unloaded bytes").unwrap();
                std::fs::rename(equivalent, &writer.path).unwrap();
            }
            for _ in 0..3 {
                with_failure(phase, || writer.save(&snapshot(), None));
                assert!(writer.protect_unloaded);
                assert_eq!(backups(&writer), vec![b"original unloaded bytes".to_vec()]);
                assert_eq!(
                    std::fs::read(&writer.path).unwrap(),
                    b"original unloaded bytes"
                );
                assert_eq!(std::fs::read(&history).unwrap(), b"history");
            }
            let events = Rc::new(RefCell::new(Vec::new()));
            let observed = events.clone();
            super::super::io::test_io::with(
                Box::new(move |phase, _| {
                    observed.borrow_mut().push(phase.to_string());
                    Ok(())
                }),
                || writer.save(&snapshot(), None),
            );
            let events = events.borrow();
            assert_eq!(
                &events[..2],
                [
                    "recovery.retry_directory_sync",
                    "recovery.retry_parent_sync"
                ]
            );
            assert!(!writer.protect_unloaded);
            assert!(writer.backup_sync.pending.is_none());
            assert!(pending.exists());
            assert_eq!(backups(&writer), vec![b"original unloaded bytes".to_vec()]);
        }
    }

    #[test]
    fn restarted_writer_confirms_existing_snapshots_once_before_cadence() {
        use std::cell::RefCell;
        use std::rc::Rc;

        let (mut writer, _root) = isolated_writer(false);
        with_failure("recovery.directory_sync", || writer.save(&snapshot(), None));
        let files = snapshots(&writer);
        assert_eq!(files.len(), 1);
        let mut restarted = SessionWriter {
            path: writer.path.clone(),
            protect_unloaded: false,
            backup_sync: RecoveryDirectory::default(),
            snapshot_sync: RecoveryDirectory::default(),
        };
        let mut changed = snapshot();
        changed.workspaces[0].custom_name = Some("after restart".into());
        let attempts = Rc::new(RefCell::new(Vec::new()));
        let failed = attempts.clone();
        super::super::io::test_io::with(
            Box::new(move |phase, _| {
                if phase.starts_with("recovery.") {
                    failed.borrow_mut().push(phase.to_string());
                    return Err(io::Error::other("restart sync unavailable"));
                }
                Ok(())
            }),
            || restarted.save(&changed, None),
        );
        assert!(!attempts.borrow().is_empty());
        assert!(attempts
            .borrow()
            .iter()
            .all(|phase| phase == "recovery.retry_directory_sync"));
        assert!(!restarted.snapshot_sync.confirmed);
        assert_eq!(snapshots(&restarted), files);
        attempts.borrow_mut().clear();
        let succeeded = attempts.clone();
        super::super::io::test_io::with(
            Box::new(move |phase, _| {
                if phase.starts_with("recovery.") {
                    succeeded.borrow_mut().push(phase.to_string());
                }
                Ok(())
            }),
            || {
                restarted.save(&changed, None);
                restarted.save(&changed, None);
            },
        );
        assert_eq!(
            *attempts.borrow(),
            [
                "recovery.retry_directory_sync",
                "recovery.retry_parent_sync"
            ]
        );
        assert_eq!(snapshots(&restarted), files);
        assert!(restarted.snapshot_sync.confirmed);
    }

    #[test]
    fn missing_source_does_not_require_an_usable_backup_directory() {
        let (mut writer, _root) = isolated_writer(true);
        std::fs::write(writer.path.with_file_name("session-backups"), b"blocked").unwrap();
        with_failure("recovery.retry_directory_sync", || writer.clear());
        assert!(writer.protect_unloaded);
        assert!(!writer.path.exists());
        with_failure("recovery.retry_directory_sync", || {
            writer.save(&snapshot(), None)
        });
        assert!(!writer.protect_unloaded);
        assert!(writer.path.exists());
        assert_eq!(
            std::fs::read(writer.path.with_file_name("session-backups")).unwrap(),
            b"blocked"
        );
    }

    #[test]
    fn recovery_publish_collision_rewinds_the_source_before_retry() {
        use std::cell::Cell;
        use std::rc::Rc;

        let (writer, _root) = isolated_writer(true);
        let original = b"complete recovery bytes \xff";
        std::fs::write(&writer.path, original).unwrap();
        let attempts = Rc::new(Cell::new(0));
        let count = attempts.clone();
        assert!(super::super::io::test_io::with(
            Box::new(move |phase, backup| {
                if phase == "recovery.publish" {
                    count.set(count.get() + 1);
                    if count.get() == 1 {
                        std::fs::write(backup, b"foreign complete backup")?;
                    }
                }
                Ok(())
            }),
            || preserve_existing(&writer.path, &mut RecoveryDirectory::default()),
        )
        .unwrap());
        assert_eq!(attempts.get(), 2);
        assert_eq!(
            backups(&writer),
            vec![b"foreign complete backup".to_vec(), original.to_vec()]
        );
    }

    #[test]
    fn recovery_collisions_do_not_remove_foreign_pending_or_complete_files() {
        for pending_collision in [false, true] {
            let (writer, _root) = isolated_writer(true);
            let backup = writer
                .path
                .with_file_name("session-000000000000000000000000000000000000001-1-0.json");
            let foreign = if pending_collision {
                backup.with_extension("pending")
            } else {
                backup.clone()
            };
            std::fs::write(&foreign, b"foreign").unwrap();
            let err = copy_recovery(
                &mut io::Cursor::new(b"new recovery"),
                &backup,
                &mut RecoveryDirectory::default(),
            )
            .unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
            assert_eq!(std::fs::read(&foreign).unwrap(), b"foreign");
            assert_eq!(settled_entry_count(writer.path.parent().unwrap(), 1), 1);
        }
    }

    #[test]
    fn recovery_publish_collisions_stop_after_128_without_removing_foreign_files() {
        let (writer, _root) = isolated_writer(true);
        std::fs::write(&writer.path, b"original").unwrap();
        let err = super::super::io::test_io::with(
            Box::new(|phase, backup| {
                if phase == "recovery.publish" {
                    std::fs::write(backup, b"foreign")?;
                }
                Ok(())
            }),
            || preserve_existing(&writer.path, &mut RecoveryDirectory::default()),
        )
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&writer.path).unwrap(), b"original");
        let directory = writer.path.with_file_name("session-backups");
        assert_eq!(settled_entry_count(&directory, 128), 128);
        let copies = backups(&writer);
        assert_eq!(copies.len(), 128);
        assert!(copies.iter().all(|bytes| bytes == b"foreign"));
    }

    #[test]
    fn readonly_source_can_still_be_preserved_as_private_raw_bytes() {
        let (writer, _root) = isolated_writer(true);
        std::fs::write(&writer.path, b"readonly \xff").unwrap();
        let original = std::fs::metadata(&writer.path).unwrap().permissions();
        let mut readonly = original.clone();
        readonly.set_readonly(true);
        std::fs::set_permissions(&writer.path, readonly).unwrap();
        let result = preserve_existing(&writer.path, &mut RecoveryDirectory::default());
        std::fs::set_permissions(&writer.path, original).unwrap();
        assert!(result.unwrap());
        assert_eq!(backups(&writer), vec![b"readonly \xff".to_vec()]);
    }

    #[test]
    fn interrupted_copy_is_not_published_as_a_recovery_file() {
        use std::io::Read;
        struct Interrupted;
        impl Read for Interrupted {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                panic!("interrupt the copy after writing its prefix");
            }
        }
        let writer = writer(true);
        let backup = writer
            .path
            .with_file_name("session-000000000000000000000000000000000000001-1-0.json");
        let mut source = io::Cursor::new(b"partial").chain(Interrupted);
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            copy_recovery(&mut source, &backup, &mut RecoveryDirectory::default())
        }))
        .is_err());
        assert!(
            !backup.exists(),
            "an interrupted copy must not look complete"
        );
        assert_eq!(
            std::fs::read(backup.with_extension("pending")).unwrap(),
            b"partial"
        );
        assert!(recovery_files(writer.path.parent().unwrap())
            .unwrap()
            .is_empty());
        remove_test_dir(&writer);
    }

    #[test]
    fn recovery_order_survives_clock_rollback() {
        let mut writer = writer(true);
        let directory = writer.path.with_file_name("session-backups");
        std::fs::create_dir(&directory).unwrap();
        let future = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
            + 1_000_000_000_000_000;
        for i in 0..2u8 {
            std::fs::write(
                directory.join(format!("session-{:039}-1-0.json", future + u128::from(i))),
                [i],
            )
            .unwrap();
        }
        for i in 2..4u8 {
            writer.protect_unloaded = true;
            std::fs::write(&writer.path, [i]).unwrap();
            writer.save(&snapshot(), None);
        }
        assert_eq!(backups(&writer), vec![vec![1], vec![2], vec![3]]);
        remove_test_dir(&writer);
    }

    #[test]
    fn recovery_keeps_three_copies_and_healthy_saves_do_not_rotate_them() {
        let mut writer = writer(true);
        for i in 0..5u8 {
            writer.protect_unloaded = true;
            std::fs::write(&writer.path, [i]).unwrap();
            writer.save(&snapshot(), None);
        }
        assert_eq!(backups(&writer), vec![vec![2], vec![3], vec![4]]);
        writer.save(&snapshot(), None);
        writer.clear();
        assert_eq!(backups(&writer), vec![vec![2], vec![3], vec![4]]);
        remove_test_dir(&writer);
    }

    #[cfg(unix)]
    #[test]
    fn dangling_symlink_allows_first_save_and_late_target_is_preserved() {
        use std::os::unix::{fs::symlink, fs::PermissionsExt};
        for late_target in [false, true] {
            let mut writer = writer(true);
            let target = writer.path.with_file_name("target.json");
            symlink("target.json", &writer.path).unwrap();
            if late_target {
                std::fs::write(&target, b"late layout").unwrap();
            }
            writer.save(&snapshot(), None);
            assert!(std::fs::symlink_metadata(&writer.path)
                .unwrap()
                .file_type()
                .is_symlink());
            assert!(target.exists());
            if late_target {
                assert_eq!(backups(&writer), vec![b"late layout".to_vec()]);
                let backup = std::fs::read_dir(writer.path.with_file_name("session-backups"))
                    .unwrap()
                    .next()
                    .unwrap()
                    .unwrap()
                    .path();
                assert_eq!(
                    std::fs::metadata(backup).unwrap().permissions().mode() & 0o777,
                    0o600
                );
            } else {
                assert!(backups(&writer).is_empty());
            }
            writer.clear();
            assert!(target.exists());
            remove_test_dir(&writer);
        }
    }
}
