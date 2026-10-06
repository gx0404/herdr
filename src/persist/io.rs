use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use tracing::warn;

use super::snapshot::{
    parse_history_snapshot, parse_snapshot, snapshot_file_version, SessionHistorySnapshot,
    SessionSnapshot, SNAPSHOT_VERSION,
};

pub(super) fn session_path() -> PathBuf {
    crate::session::data_dir().join("session.json")
}

fn session_history_path() -> PathBuf {
    crate::session::data_dir().join("session-history.json")
}

// Canonicalization requires an existing target; a dangling link must still
// resolve to its destination on the first save.
fn resolve_write_target(path: &Path) -> io::Result<PathBuf> {
    let mut current = path.to_path_buf();
    let mut followed = 0;
    loop {
        let meta = match std::fs::symlink_metadata(&current) {
            Ok(meta) => meta,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(current),
            Err(err) => return Err(err),
        };
        if !meta.file_type().is_symlink() {
            return if meta.is_file() {
                Ok(current)
            } else {
                Err(io::Error::other("session path is not a regular file"))
            };
        }
        if followed == 40 {
            return Err(io::Error::other("session symlink chain exceeds 40 links"));
        }
        followed += 1;
        let link = std::fs::read_link(&current)?;
        current = if link.is_absolute() {
            link
        } else {
            current
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(link)
        };
    }
}

#[derive(Debug)]
pub(super) struct SaveError {
    pub(super) replaced: bool,
    phase: &'static str,
    source: io::Error,
    cleanup: Option<io::Error>,
}

impl std::fmt::Display for SaveError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} failed (replaced={}): {}",
            self.phase, self.replaced, self.source
        )?;
        if let Some(cleanup) = &self.cleanup {
            write!(formatter, "; temporary cleanup failed: {cleanup}")?;
        }
        Ok(())
    }
}

impl std::error::Error for SaveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

pub(super) fn save_to_path(path: &Path, snapshot: &SessionSnapshot) -> Result<(), SaveError> {
    save_json_to_path(path, snapshot)
}

fn save_json_to_path<T: serde::Serialize>(path: &Path, snapshot: &T) -> Result<(), SaveError> {
    let json = serde_json::to_string_pretty(snapshot).map_err(|err| SaveError {
        replaced: false,
        phase: "save.serialize",
        source: err.into(),
        cleanup: None,
    })?;
    save_bytes_to_path(path, json.as_bytes())
}

fn save_bytes_to_path(path: &Path, bytes: &[u8]) -> Result<(), SaveError> {
    static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(0);
    let mut phase = "save.resolve";
    let mut replaced = false;
    let mut temporary: Option<(File, PathBuf)> = None;
    let result = (|| -> io::Result<()> {
        let target = resolve_write_target(path)?;
        phase = "save.source";
        let source = match File::options().read(true).write(true).open(&target) {
            Ok(source) => {
                crate::platform::check_persist_source(&source)?;
                if source.metadata()?.permissions().readonly() {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "session target is readonly",
                    ));
                }
                Some(source)
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => None,
            Err(err) => return Err(err),
        };
        let directory = target
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        phase = "save.create_parent";
        std::fs::create_dir_all(directory)?;
        phase = "save.create_temporary";
        for _ in 0..128 {
            let sequence = NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed);
            let candidate = directory.join(format!(
                ".herdr-persist-{}-{sequence}.tmp",
                std::process::id()
            ));
            #[cfg(test)]
            test_io::check(phase, &candidate)?;
            match crate::platform::create_persist_temporary(&candidate) {
                Ok(file) => {
                    temporary = Some((file, candidate));
                    break;
                }
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(err) => return Err(err),
            }
        }
        let (output, pending) = temporary.as_mut().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "could not allocate session temporary file after 128 attempts",
            )
        })?;
        phase = "save.metadata";
        if let Some(source) = &source {
            crate::platform::prepare_persist_metadata(source, output)?;
        }
        phase = "save.write";
        #[cfg(test)]
        test_io::check(phase, &target)?;
        output.write_all(bytes)?;
        phase = "save.file_sync";
        #[cfg(test)]
        test_io::check(phase, &target)?;
        output.sync_all()?;
        phase = "save.rename";
        #[cfg(test)]
        test_io::check(phase, &target)?;
        std::fs::rename(&*pending, &target)?;
        replaced = true;
        phase = "save.directory_sync";
        #[cfg(test)]
        test_io::check(phase, &target)?;
        crate::platform::sync_directory(directory)
    })();
    result.map_err(|source| {
        let cleanup = if !replaced {
            temporary.as_ref().and_then(|(file, pending)| {
                #[cfg(test)]
                if let Err(err) = test_io::check("save.cleanup", pending) {
                    return Some(err);
                }
                crate::platform::discard_persist_temporary(file, pending).err()
            })
        } else {
            None
        };
        SaveError {
            replaced,
            phase,
            source,
            cleanup,
        }
    })
}

pub(super) fn save_history_to_path(
    path: &Path,
    history: Option<&SessionHistorySnapshot>,
) -> io::Result<()> {
    match history {
        Some(history) => {
            save_json_to_path(path, history).map_err(|err| io::Error::new(err.source.kind(), err))
        }
        None => clear_path(path),
    }
}

#[cfg(all(test, windows))]
pub(crate) fn assert_legacy_save_rejected(path: &Path) {
    use std::cell::Cell;
    use std::rc::Rc;

    let writes = Rc::new(Cell::new(0));
    let observed = writes.clone();
    let err = test_io::with(
        Box::new(move |phase, _| {
            if phase == "save.write" {
                observed.set(observed.get() + 1);
            }
            Ok(())
        }),
        || save_bytes_to_path(path, b"replacement"),
    )
    .unwrap_err();
    assert_eq!(err.phase, "save.metadata", "{err:?}");
    assert_eq!(err.source.kind(), io::ErrorKind::Unsupported, "{err:?}");
    assert!(!err.replaced);
    assert!(err.cleanup.is_none(), "{err:?}");
    assert_eq!(writes.get(), 0);
    assert_eq!(std::fs::read(path).unwrap(), b"original");
    assert_eq!(
        std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
        1
    );
}

#[cfg(test)]
pub(super) mod test_io {
    use std::cell::RefCell;
    use std::io;
    use std::path::Path;

    type Hook = Box<dyn FnMut(&str, &Path) -> io::Result<()>>;

    thread_local! {
        static HOOK: RefCell<Option<Hook>> = const { RefCell::new(None) };
    }

    pub(in crate::persist) fn check(phase: &str, path: &Path) -> io::Result<()> {
        HOOK.with(|slot| match slot.borrow_mut().as_mut() {
            Some(hook) => hook(phase, path),
            None => Ok(()),
        })
    }

    pub(in crate::persist) fn with<R>(hook: Hook, run: impl FnOnce() -> R) -> R {
        struct Restore(Option<Hook>);
        impl Drop for Restore {
            fn drop(&mut self) {
                HOOK.with(|slot| *slot.borrow_mut() = self.0.take());
            }
        }
        let _restore = Restore(HOOK.with(|slot| slot.replace(Some(hook))));
        run()
    }
}

pub(super) fn clear_path(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

pub fn clear_history() {
    let path = session_history_path();
    if let Err(err) = clear_path(&path) {
        crate::logging::session_clear_failed(&path, &err.to_string());
    }
}

pub fn load() -> Option<SessionSnapshot> {
    let path = session_path();
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!(
                event = "persist.restore", subsystem = "persist", outcome = "missing",
                path = %path.display(), "session file is missing"
            );
            return None;
        }
        Err(err) => {
            warn!(
                event = "persist.restore", subsystem = "persist", outcome = "read_error",
                path = %path.display(), err = %err, "failed to read session file"
            );
            return None;
        }
    };
    match parse_snapshot(&content) {
        Ok(snapshot) => Some(snapshot),
        Err(err) => {
            if let Some(version) = snapshot_file_version(&content) {
                if version > SNAPSHOT_VERSION {
                    warn!(
                        event = "persist.restore", subsystem = "persist", outcome = "unsupported_version",
                        path = %path.display(), file_version = version, supported = SNAPSHOT_VERSION,
                        "session file is from a newer herdr version, ignoring"
                    );
                    return None;
                }
            }
            warn!(
                event = "persist.restore", subsystem = "persist", outcome = "parse_error",
                path = %path.display(), err = %err, "failed to parse session file, ignoring"
            );
            None
        }
    }
}

pub fn load_history() -> Option<SessionHistorySnapshot> {
    let path = session_history_path();
    if !path.exists() {
        return None;
    }
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(err) => {
            warn!(err = %err, "failed to read session history file");
            return None;
        }
    };
    match parse_history_snapshot(&content) {
        Ok(snapshot) => Some(snapshot),
        Err(err) => {
            if let Some(version) = snapshot_file_version(&content) {
                if version > SNAPSHOT_VERSION {
                    warn!(
                        file_version = version,
                        supported = SNAPSHOT_VERSION,
                        "session history file is from a newer herdr version, ignoring"
                    );
                    return None;
                }
            }
            warn!(err = %err, "failed to parse session history file, ignoring");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persist::snapshot::{
        PaneHistorySnapshot, TabHistorySnapshot, WorkspaceHistorySnapshot,
    };

    /// 唯一临时根下的 `session.json` 路径；析构时删掉整个根（含断言失败的 panic 展开），
    /// 测试不留残留。
    struct TempSessionPath(PathBuf);

    impl std::ops::Deref for TempSessionPath {
        type Target = Path;

        fn deref(&self) -> &Path {
            &self.0
        }
    }

    impl AsRef<Path> for TempSessionPath {
        fn as_ref(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempSessionPath {
        fn drop(&mut self) {
            if let Some(root) = self.0.parent() {
                let _ = std::fs::remove_dir_all(root);
            }
        }
    }

    fn temp_session_path(name: &str) -> TempSessionPath {
        // 时间戳在并发的测试线程间会撞，再带进程内序号。
        let unique = format!(
            "herdr-session-tests-{}-{}-{}-{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            crate::config::test_dirs::unique_id()
        );
        TempSessionPath(std::env::temp_dir().join(unique).join("session.json"))
    }

    fn temp_session_paths(name: &str) -> (TempSessionPath, PathBuf) {
        let session = temp_session_path(name);
        let history = session.with_file_name("session-history.json");
        (session, history)
    }

    fn empty_snapshot() -> SessionSnapshot {
        SessionSnapshot {
            version: SNAPSHOT_VERSION,
            workspaces: vec![],
            active: None,
            selected: 0,
            sidebar_width: Some(26),
            sidebar_section_split: Some(0.5),
            collapsed_space_keys: std::collections::HashSet::new(),
        }
    }

    fn history_snapshot(secret: &str) -> SessionHistorySnapshot {
        SessionHistorySnapshot {
            version: SNAPSHOT_VERSION,
            layout_fingerprint: None,
            workspaces: vec![WorkspaceHistorySnapshot {
                tabs: vec![TabHistorySnapshot {
                    panes: std::collections::HashMap::from([(
                        0,
                        PaneHistorySnapshot {
                            ansi: secret.to_string(),
                            lines: 1,
                        },
                    )]),
                }],
            }],
        }
    }

    #[test]
    fn fixed_temporary_file_is_not_overwritten_or_removed() {
        let path = temp_session_path("foreign-fixed-temp-file");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let foreign = path.with_extension("json.tmp");
        std::fs::write(&foreign, b"foreign temporary contents").unwrap();

        save_to_path(&path, &empty_snapshot()).unwrap();

        assert_eq!(
            std::fs::read(&foreign).unwrap(),
            b"foreign temporary contents"
        );
        assert!(parse_snapshot(&std::fs::read_to_string(&path).unwrap()).is_ok());
    }

    #[test]
    fn fixed_temporary_directory_does_not_block_save() {
        let path = temp_session_path("foreign-fixed-temp-directory");
        let foreign = path.with_extension("json.tmp");
        std::fs::create_dir_all(&foreign).unwrap();
        std::fs::write(foreign.join("keep"), b"foreign").unwrap();

        save_to_path(&path, &empty_snapshot()).unwrap();

        assert_eq!(std::fs::read(foreign.join("keep")).unwrap(), b"foreign");
        assert!(parse_snapshot(&std::fs::read_to_string(&path).unwrap()).is_ok());
    }

    #[test]
    fn exclusive_temporary_collisions_are_bounded_and_never_cleaned() {
        use std::cell::RefCell;
        use std::rc::Rc;

        for collisions in [1, 128] {
            let path = temp_session_path("temporary-collisions");
            let owned = Rc::new(RefCell::new(Vec::new()));
            let occupied = owned.clone();
            let result = test_io::with(
                Box::new(move |phase, candidate| {
                    if phase == "save.create_temporary" && occupied.borrow().len() < collisions {
                        std::fs::write(candidate, b"foreign")?;
                        occupied.borrow_mut().push(candidate.to_path_buf());
                    }
                    Ok(())
                }),
                || save_to_path(&path, &empty_snapshot()),
            );
            if collisions == 128 {
                let err = result.unwrap_err();
                assert_eq!(err.phase, "save.create_temporary");
                assert_eq!(err.source.kind(), io::ErrorKind::AlreadyExists);
                assert!(!err.replaced);
                assert!(!path.exists());
            } else {
                result.unwrap();
                assert!(path.exists());
            }
            assert_eq!(owned.borrow().len(), collisions);
            for foreign in owned.borrow().iter() {
                assert_eq!(std::fs::read(foreign).unwrap(), b"foreign");
            }
        }
    }

    #[test]
    fn temporary_errors_other_than_collision_are_not_retried() {
        use std::cell::Cell;
        use std::rc::Rc;

        let path = temp_session_path("temporary-denied");
        let attempts = Rc::new(Cell::new(0));
        let count = attempts.clone();
        let err = test_io::with(
            Box::new(move |phase, _| {
                if phase == "save.create_temporary" {
                    count.set(count.get() + 1);
                    return Err(io::ErrorKind::PermissionDenied.into());
                }
                Ok(())
            }),
            || save_to_path(&path, &empty_snapshot()),
        )
        .unwrap_err();
        assert_eq!(attempts.get(), 1);
        assert_eq!(err.source.kind(), io::ErrorKind::PermissionDenied);
        assert!(!err.replaced);
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            0
        );
    }

    #[test]
    fn save_failures_report_the_phase_and_exact_commit_state() {
        for existed in [false, true] {
            for phase in [
                "save.write",
                "save.file_sync",
                "save.rename",
                "save.directory_sync",
            ] {
                let path = temp_session_path("save-stages");
                if existed {
                    save_bytes_to_path(&path, b"original bytes").unwrap();
                }
                let expected = serde_json::to_vec_pretty(&empty_snapshot()).unwrap();
                let err = test_io::with(
                    Box::new(move |current, _| {
                        if current == phase {
                            Err(io::Error::other("injected persistence failure"))
                        } else {
                            Ok(())
                        }
                    }),
                    || save_to_path(&path, &empty_snapshot()),
                )
                .unwrap_err();
                let replaced = phase == "save.directory_sync";
                let diagnostic = || {
                    format!(
                        "path={} existed={existed} injected_phase={phase} actual_phase={} \
                         expected_replaced={replaced} error={err} debug={err:?} \
                         source_kind={:?} source_raw_os_error={:?} \
                         cleanup_kind={:?} cleanup_raw_os_error={:?}",
                        path.display(),
                        err.phase,
                        err.source.kind(),
                        err.source.raw_os_error(),
                        err.cleanup.as_ref().map(io::Error::kind),
                        err.cleanup.as_ref().and_then(io::Error::raw_os_error),
                    )
                };
                assert_eq!(err.replaced, replaced, "{}", diagnostic());
                assert_eq!(err.phase, phase, "{}", diagnostic());
                assert_eq!(err.source.to_string(), "injected persistence failure");
                assert!(err.to_string().contains(&format!("replaced={replaced}")));
                assert!(err.cleanup.is_none(), "{err}");
                if replaced {
                    assert_eq!(std::fs::read(&path).unwrap(), expected);
                } else if existed {
                    assert_eq!(std::fs::read(&path).unwrap(), b"original bytes");
                } else {
                    assert!(!path.exists());
                }
                let expected_count = usize::from(existed || replaced);
                assert_eq!(
                    std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
                    expected_count
                );
            }
        }
    }

    #[test]
    fn partial_write_failure_discards_only_the_owned_temporary() {
        let path = temp_session_path("partial-write");
        save_bytes_to_path(&path, b"original").unwrap();
        let foreign = path.with_extension("json.tmp");
        std::fs::write(&foreign, b"foreign").unwrap();
        let mut pending = PathBuf::new();
        let err = test_io::with(
            Box::new(move |phase, candidate| {
                if phase == "save.create_temporary" {
                    pending = candidate.to_path_buf();
                } else if phase == "save.write" {
                    std::fs::write(&pending, b"partial JSON prefix")?;
                    return Err(io::Error::other("injected partial write failure"));
                }
                Ok(())
            }),
            || save_to_path(&path, &empty_snapshot()),
        )
        .unwrap_err();
        assert!(!err.replaced);
        assert_eq!(err.phase, "save.write");
        assert_eq!(err.source.to_string(), "injected partial write failure");
        assert!(err.cleanup.is_none());
        assert_eq!(std::fs::read(&path).unwrap(), b"original");
        assert_eq!(std::fs::read(&foreign).unwrap(), b"foreign");
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            2
        );
    }

    #[test]
    fn temporary_cleanup_failure_is_reported_without_touching_the_target() {
        let path = temp_session_path("cleanup-error");
        save_bytes_to_path(&path, b"original").unwrap();
        let err = test_io::with(
            Box::new(|phase, _| {
                if matches!(phase, "save.write" | "save.cleanup") {
                    Err(io::Error::other(format!("injected {phase}")))
                } else {
                    Ok(())
                }
            }),
            || save_to_path(&path, &empty_snapshot()),
        )
        .unwrap_err();
        assert!(!err.replaced);
        assert!(err.to_string().contains("temporary cleanup failed"));
        assert_eq!(err.phase, "save.write");
        assert_eq!(err.source.to_string(), "injected save.write");
        assert_eq!(
            err.cleanup.as_ref().unwrap().to_string(),
            "injected save.cleanup"
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"original");
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            2
        );
    }

    #[test]
    fn readonly_target_is_not_replaced() {
        let path = temp_session_path("readonly-target");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"original").unwrap();
        let original = std::fs::metadata(&path).unwrap().permissions();
        let mut readonly = original.clone();
        readonly.set_readonly(true);
        std::fs::set_permissions(&path, readonly).unwrap();
        let result = save_to_path(&path, &empty_snapshot());
        std::fs::set_permissions(&path, original).unwrap();
        assert!(!result.unwrap_err().replaced);
        assert_eq!(std::fs::read(&path).unwrap(), b"original");
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1
        );
    }

    #[test]
    fn resolver_rejects_directories_and_metadata_errors() {
        let path = temp_session_path("invalid-target");
        std::fs::create_dir_all(&path).unwrap();
        assert!(resolve_write_target(&path).is_err());
        let regular = path.join("regular");
        std::fs::write(&regular, b"original").unwrap();
        let child = regular.join("child.json");
        let metadata_error = std::fs::symlink_metadata(&child).unwrap_err();
        if metadata_error.kind() != io::ErrorKind::NotFound {
            assert!(resolve_write_target(&child).is_err());
        }
        assert!(save_to_path(&child, &empty_snapshot()).is_err());
        assert_eq!(std::fs::read(regular).unwrap(), b"original");
    }

    #[cfg(unix)]
    #[test]
    fn resolver_accepts_exactly_forty_links_and_rejects_a_loop() {
        let path = temp_session_path("link-boundary");
        let directory = path.parent().unwrap();
        std::fs::create_dir_all(directory).unwrap();
        for index in 0..41 {
            std::os::unix::fs::symlink(
                format!("link-{}", index + 1),
                directory.join(format!("link-{index}")),
            )
            .unwrap();
        }
        assert_eq!(
            resolve_write_target(&directory.join("link-1")).unwrap(),
            directory.join("link-41")
        );
        assert!(resolve_write_target(&directory.join("link-0")).is_err());
        std::os::unix::fs::symlink("loop", directory.join("loop")).unwrap();
        assert!(resolve_write_target(&directory.join("loop")).is_err());
    }

    #[test]
    fn concurrent_large_json_saves_only_expose_complete_versions() {
        use std::sync::{Arc, Barrier};

        let path = temp_session_path("concurrent-json");
        let payloads: Vec<String> = (b'a'..=b'd')
            .map(|byte| char::from(byte).to_string().repeat(256 * 1024))
            .collect();
        save_json_to_path(&path, &payloads[0]).unwrap();
        let barrier = Arc::new(Barrier::new(payloads.len() + 1));
        std::thread::scope(|scope| {
            let handles: Vec<_> = payloads
                .iter()
                .map(|payload| {
                    let path = path.to_path_buf();
                    let barrier = barrier.clone();
                    scope.spawn(move || {
                        barrier.wait();
                        for _ in 0..8 {
                            save_json_to_path(&path, payload).unwrap();
                        }
                    })
                })
                .collect();
            barrier.wait();
            loop {
                let bytes = std::fs::read(&path).unwrap();
                let observed: String = serde_json::from_slice(&bytes).unwrap();
                assert!(payloads.contains(&observed));
                if handles
                    .iter()
                    .all(std::thread::ScopedJoinHandle::is_finished)
                {
                    break;
                }
            }
            for handle in handles {
                handle.join().unwrap();
            }
        });
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1
        );
    }

    #[test]
    fn save_to_paths_writes_pane_history_only_to_history_file() {
        let (session_path, history_path) = temp_session_paths("split-history");

        save_to_path(&session_path, &empty_snapshot()).unwrap();
        save_history_to_path(&history_path, Some(&history_snapshot("split-secret"))).unwrap();

        let session = std::fs::read_to_string(&session_path).unwrap();
        let history = std::fs::read_to_string(&history_path).unwrap();
        assert!(!session.contains("split-secret"));
        assert!(!session.contains("history"));
        assert!(history.contains("split-secret"));
    }

    #[test]
    fn save_to_paths_removes_stale_history_when_history_is_disabled() {
        let (session_path, history_path) = temp_session_paths("clear-history");
        save_to_path(&session_path, &empty_snapshot()).unwrap();
        save_history_to_path(&history_path, Some(&history_snapshot("stale-secret"))).unwrap();

        save_history_to_path(&history_path, None).unwrap();

        assert!(session_path.exists());
        assert!(!history_path.exists());
    }

    #[test]
    fn clear_path_removes_existing_session_file() {
        let path = temp_session_path("clear-existing");
        save_to_path(&path, &empty_snapshot()).unwrap();

        clear_path(&path).unwrap();

        assert!(!path.exists());
    }

    #[test]
    fn clear_path_ignores_missing_session_file() {
        let path = temp_session_path("clear-missing");

        clear_path(&path).unwrap();

        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn save_to_path_preserves_existing_symlink() {
        let target = temp_session_path("symlink-target");
        let link = target.with_file_name("link.json");
        save_to_path(&target, &empty_snapshot()).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let mut snap = empty_snapshot();
        snap.selected = 7;
        save_to_path(&link, &snap).unwrap();

        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        let parsed = parse_snapshot(&std::fs::read_to_string(&target).unwrap()).unwrap();
        assert_eq!(parsed.selected, 7);
    }

    #[cfg(unix)]
    #[test]
    fn save_to_path_writes_through_dangling_symlink() {
        let target = temp_session_path("dangling-target");
        let link = target.with_file_name("link.json");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        save_to_path(&link, &empty_snapshot()).unwrap();

        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(target.exists());
    }

    #[cfg(unix)]
    #[test]
    fn save_to_path_resolves_relative_symlink() {
        let session = temp_session_path("relative-symlink");
        let dir = session.parent().unwrap();
        std::fs::create_dir_all(dir).unwrap();
        let target = dir.join("real.json");
        let link = dir.join("link.json");
        std::os::unix::fs::symlink("real.json", &link).unwrap();

        save_to_path(&link, &empty_snapshot()).unwrap();

        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(target.exists());
    }
}
