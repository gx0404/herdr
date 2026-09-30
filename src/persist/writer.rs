use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::{SessionHistorySnapshot, SessionSnapshot};

/// Shared by autosave, pane-exit checkpoints, and shutdown.
pub(crate) struct SessionWriter {
    path: PathBuf,
    protect_unloaded: bool,
}

impl SessionWriter {
    pub(crate) fn new(protect_unloaded: bool) -> Self {
        Self {
            path: super::io::session_path(),
            protect_unloaded,
        }
    }

    fn preserve_unloaded(&mut self) -> io::Result<()> {
        if self.protect_unloaded && preserve_existing(&self.path)? {
            self.protect_unloaded = false;
        }
        Ok(())
    }

    fn preserve_snapshot_history(&self) {
        if let Err(err) = preserve_snapshot_history(&self.path) {
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
        let result = self.preserve_unloaded().and_then(|()| {
            self.preserve_snapshot_history();
            super::io::save_to_path(&self.path, snapshot)
        });
        if let Err(err) = result {
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

fn preserve_snapshot_history(path: &Path) -> io::Result<()> {
    let directory = path.with_file_name("session-snapshots");
    let existing = match recovery_files(&directory) {
        Ok(files) => files,
        Err(err) if err.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(err) => return Err(err),
    };
    if let Some((_, latest)) = existing.last() {
        let modified = std::fs::metadata(latest)?.modified()?;
        if SystemTime::now()
            .duration_since(modified)
            .is_ok_and(|age| age < SNAPSHOT_INTERVAL)
        {
            return Ok(());
        }
    }
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
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
    preserve_existing_in(path, "session-snapshots", SNAPSHOT_LIMIT)?;
    Ok(())
}

fn preserve_existing(path: &Path) -> io::Result<bool> {
    preserve_existing_in(path, "session-backups", 3)
}

fn preserve_existing_in(path: &Path, directory_name: &str, keep: usize) -> io::Result<bool> {
    let mut source = match File::open(path) {
        Ok(file) => file,
        // Recheck on the next mutation until a fresh session is actually saved.
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err),
    };
    if !source.metadata()?.is_file() {
        return Err(io::Error::other("session path is not a regular file"));
    }
    let directory = path.with_file_name(directory_name);
    std::fs::create_dir_all(&directory)?;
    let older = recovery_files(&directory)?;
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
        match copy_recovery(&mut source, &backup) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
        tracing::info!(
            event = "persist.backup",
            subsystem = "persist",
            outcome = "ok",
            path = %path.display(),
            backup_path = %backup.display(),
            "preserved session recovery copy"
        );
        if let Err(err) = prune_backups(&older, keep) {
            if directory_name == "session-snapshots" {
                std::fs::remove_file(&backup)?;
                return Err(err);
            }
            tracing::warn!(
                event = "persist.backup", subsystem = "persist", outcome = "prune_error",
                path = %directory.display(), err = %err, "failed to prune session recovery copies"
            );
        }
        return Ok(true);
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate session recovery copy",
    ))
}

fn copy_recovery(source: &mut impl io::Read, backup: &Path) -> io::Result<()> {
    let directory = backup.parent().unwrap_or_else(|| Path::new("."));
    let pending = backup.with_extension("pending");
    let mut output = crate::platform::create_config_temporary(&pending, true)?;
    let mut published = false;
    let result = (|| {
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
        io::copy(source, &mut output)?;
        output.sync_all()?;
        drop(output);
        std::fs::rename(&pending, backup)?;
        published = true;
        crate::platform::sync_parent_directory(directory)?;
        crate::platform::sync_parent_directory(directory.parent().unwrap_or_else(|| Path::new(".")))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(pending);
        if published {
            let _ = std::fs::remove_file(backup);
        }
    }
    result
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
        }
    }

    fn snapshot() -> SessionSnapshot {
        serde_json::from_str(include_str!(
            "../../tests/fixtures/session/current-herdr-session.json"
        ))
        .unwrap()
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
        let mut writer = writer(true);
        std::fs::write(&writer.path, b"original").unwrap();
        let temporary = writer.path.with_extension("json.tmp");
        std::fs::create_dir(&temporary).unwrap();
        writer.save(&snapshot(), None);
        writer.save(&snapshot(), None);
        assert_eq!(std::fs::read(&writer.path).unwrap(), b"original");
        std::fs::remove_dir(&temporary).unwrap();
        writer.save(&snapshot(), None);
        assert_eq!(backups(&writer), vec![b"original".to_vec()]);
        remove_test_dir(&writer);
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
            copy_recovery(&mut source, &backup)
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
