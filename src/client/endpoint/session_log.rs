//! Profile-driven session logging for saved SSH machines.
//!
//! The hot render path has no safe hook inside this module tree, so the
//! mechanism is snapshot-based: one background worker per logged profile
//! polls the machine's panes through its JSON API bridge and appends a
//! snapshot whenever the pane revision changes. All disk I/O is funneled
//! through a single writer thread fed by a bounded channel, so a slow disk
//! or a flooding pane never blocks a caller; when the queue is full the
//! snapshot is dropped and counted against its profile. A future
//! frame-pipeline tap can reuse [`SessionLogQueue::append`] directly without
//! touching the writer.
//!
//! Files rotate at the profile's size cap: the current file is moved to
//! `<file>.1` (atomically, through the platform replace) and a fresh file
//! starts. Logs are local client state, created with private permissions.

use std::collections::HashMap;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::catalog::{SavedSshEndpoint, SessionLogProfile};
use super::ProfileId;

pub(crate) const DEFAULT_PATH_TEMPLATE: &str = "{machine}/{date}-{pane}.log";
pub(crate) const DEFAULT_DUMP_INTERVAL_SECS: u16 = 30;
pub(crate) const DEFAULT_MAX_LOG_BYTES: u64 = 16 * 1024 * 1024;
const SNAPSHOT_LINES: u32 = 200;
const WORKER_STOP_POLL: Duration = Duration::from_millis(100);
const WRITER_QUEUE_DEPTH: usize = 256;
const WRITER_FLUSH_INTERVAL: Duration = Duration::from_millis(250);
const WRITER_FLUSH_BYTES: usize = 64 * 1024;

const TEMPLATE_VARIABLES: [&str; 5] = ["date", "machine", "machine-id", "pane", "session"];

/// Save-time template check: balanced braces and known variables only.
pub(crate) fn validate_path_template(template: &str) -> Result<(), String> {
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        if rest[..open].contains('}') {
            return Err("session log path template has an unbalanced '}'".into());
        }
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            return Err("session log path template has an unbalanced '{'".into());
        };
        let name = &after[..close];
        if !TEMPLATE_VARIABLES.contains(&name) {
            return Err(format!(
                "session log path template variable {{{name}}} is not supported; use one of {}",
                TEMPLATE_VARIABLES
                    .map(|name| format!("{{{name}}}"))
                    .join(", ")
            ));
        }
        rest = &after[close + 1..];
    }
    if rest.contains('}') {
        return Err("session log path template has an unbalanced '}'".into());
    }
    Ok(())
}

fn slugify(value: &str, fallback: &str) -> String {
    let slug: String = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
                ch
            } else {
                '-'
            }
        })
        .collect();
    let slug = slug.trim_matches(|ch| ch == '-' || ch == '.');
    if slug.is_empty() {
        fallback.to_string()
    } else {
        slug.to_string()
    }
}

/// Renders a profile's log path template for one pane. Relative templates
/// resolve under `<state dir>/session-logs/`; absolute templates are used
/// as-is. The rendered path must not contain `..` components or control
/// characters. `date` is a parameter so rendering stays pure and testable.
pub(crate) fn render_log_path(
    template: Option<&str>,
    profile: &SavedSshEndpoint,
    pane_id: &str,
    date: time::Date,
) -> Result<PathBuf, String> {
    let template = template.unwrap_or(DEFAULT_PATH_TEMPLATE);
    validate_path_template(template)?;
    let date = format!(
        "{:04}-{:02}-{:02}",
        date.year(),
        date.month() as u8,
        date.day()
    );
    let machine_id = profile.id.as_str();
    let values = [
        ("date", date),
        ("machine", slugify(&profile.label, "machine")),
        ("machine-id", machine_id[..8].to_string()),
        ("pane", pane_id.replace(':', "-")),
        ("session", slugify(&profile.session, "session")),
    ];
    let mut rendered = template.to_string();
    for (name, value) in values {
        rendered = rendered.replace(&format!("{{{name}}}"), &value);
    }
    if rendered.is_empty() || rendered.chars().any(char::is_control) {
        return Err("session log path renders to an empty or invalid path".into());
    }
    let path = PathBuf::from(&rendered);
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err("session log path must not contain '..' components".into());
    };
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(crate::config::state_dir().join("session-logs").join(path))
    }
}

enum LogCommand {
    Append {
        path: PathBuf,
        bytes: Vec<u8>,
        max_bytes: u64,
    },
}

/// Drop counters for the shared writer queue, split per profile so each
/// machine's detail card shows its own loss instead of a fleet-wide total.
/// The mutex is only taken on the drop path (queue full), never on a
/// successful append.
#[derive(Default)]
struct SessionLogDrops {
    by_profile: std::sync::Mutex<HashMap<ProfileId, u64>>,
}

impl SessionLogDrops {
    fn record(&self, profile: &ProfileId) -> u64 {
        let mut counts = self
            .by_profile
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let count = counts.entry(profile.clone()).or_insert(0);
        *count = count.saturating_add(1);
        *count
    }

    fn get(&self, profile: &ProfileId) -> u64 {
        self.by_profile
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(profile)
            .copied()
            .unwrap_or(0)
    }
}

/// Cheap cloneable producer side of the log writer. `append` never blocks:
/// a full queue drops the payload and counts it against the profile.
#[derive(Clone)]
pub(crate) struct SessionLogQueue {
    sender: mpsc::SyncSender<LogCommand>,
    dropped: Arc<SessionLogDrops>,
}

impl SessionLogQueue {
    pub(crate) fn append(
        &self,
        profile: &ProfileId,
        path: PathBuf,
        bytes: Vec<u8>,
        max_bytes: u64,
    ) {
        match self.sender.try_send(LogCommand::Append {
            path,
            bytes,
            max_bytes,
        }) {
            Ok(()) => {}
            Err(mpsc::TrySendError::Full(_)) => {
                let dropped = self.dropped.record(profile);
                if dropped == 1 || dropped.is_multiple_of(1024) {
                    tracing::warn!(dropped, profile = %profile, "session log queue is full; dropping snapshots");
                }
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {}
        }
    }

    /// Snapshots dropped for one profile because the writer queue was full.
    /// Status surface for that machine's detail card (mirrored through the
    /// supervisor).
    pub(crate) fn dropped_for(&self, profile: &ProfileId) -> u64 {
        self.dropped.get(profile)
    }
}

struct PendingLog {
    bytes: Vec<u8>,
    max_bytes: u64,
}

/// Single background writer: batches appends per file, flushes on an
/// interval or batch size, rotates at the per-file cap. Dropping the writer
/// signals shutdown, flushes, and joins the thread.
pub(crate) struct SessionLogWriter {
    queue: SessionLogQueue,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl SessionLogWriter {
    pub(crate) fn start() -> Self {
        let (sender, receiver) = mpsc::sync_channel(WRITER_QUEUE_DEPTH);
        let dropped = Arc::new(SessionLogDrops::default());
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker_shutdown = Arc::clone(&shutdown);
        let thread = thread::Builder::new()
            .name("session-log-writer".into())
            .spawn(move || run_writer(receiver, worker_shutdown));
        match thread {
            Ok(thread) => Self {
                queue: SessionLogQueue { sender, dropped },
                shutdown,
                thread: Some(thread),
            },
            Err(error) => {
                // No writer thread: the queue drains into nothing and
                // appends become counted drops instead of failing the client.
                tracing::warn!(%error, "session log writer thread failed to start");
                Self {
                    queue: SessionLogQueue { sender, dropped },
                    shutdown,
                    thread: None,
                }
            }
        }
    }

    pub(crate) fn queue(&self) -> SessionLogQueue {
        self.queue.clone()
    }
}

impl Drop for SessionLogWriter {
    fn drop(&mut self) {
        // Queue clones keep the channel alive, so an explicit flag — not
        // disconnect — is the shutdown signal.
        self.shutdown.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run_writer(receiver: mpsc::Receiver<LogCommand>, shutdown: Arc<AtomicBool>) {
    let mut pending: HashMap<PathBuf, PendingLog> = HashMap::new();
    let mut pending_bytes = 0_usize;
    loop {
        if shutdown.load(Ordering::Acquire) {
            // Drain what was already queued, then flush one last time.
            while let Ok(LogCommand::Append {
                path,
                bytes,
                max_bytes,
            }) = receiver.try_recv()
            {
                pending
                    .entry(path)
                    .or_insert_with(|| PendingLog {
                        bytes: Vec::new(),
                        max_bytes,
                    })
                    .bytes
                    .extend_from_slice(&bytes);
            }
            flush_all(&mut pending, &mut pending_bytes);
            return;
        }
        match receiver.recv_timeout(WRITER_FLUSH_INTERVAL) {
            Ok(LogCommand::Append {
                path,
                bytes,
                max_bytes,
            }) => {
                pending_bytes += bytes.len();
                pending
                    .entry(path)
                    .or_insert_with(|| PendingLog {
                        bytes: Vec::new(),
                        max_bytes,
                    })
                    .bytes
                    .extend_from_slice(&bytes);
                if pending_bytes >= WRITER_FLUSH_BYTES {
                    flush_all(&mut pending, &mut pending_bytes);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => flush_all(&mut pending, &mut pending_bytes),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                flush_all(&mut pending, &mut pending_bytes);
                break;
            }
        }
    }
}

fn flush_all(pending: &mut HashMap<PathBuf, PendingLog>, pending_bytes: &mut usize) {
    for (path, entry) in pending.iter_mut() {
        if let Err(error) = flush_file(path, entry) {
            tracing::warn!(%error, path = %path.display(), "session log write failed");
        }
    }
    pending.clear();
    *pending_bytes = 0;
}

fn flush_file(path: &Path, entry: &mut PendingLog) -> io::Result<()> {
    if entry.bytes.is_empty() {
        return Ok(());
    }
    let bytes = std::mem::take(&mut entry.bytes);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let existing = std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
    if existing > 0 && existing.saturating_add(bytes.len() as u64) > entry.max_bytes {
        let mut rotated = path.as_os_str().to_owned();
        rotated.push(".1");
        crate::platform::replace_file(path, Path::new(&rotated))?;
    }
    if !path.exists() {
        crate::platform::create_private_state_file(path)?;
    }
    let mut file = std::fs::OpenOptions::new().append(true).open(path)?;
    io::Write::write_all(&mut file, &bytes)
}

type WorkerStarter =
    fn(&SavedSshEndpoint, Arc<AtomicBool>, SessionLogQueue) -> io::Result<JoinHandle<()>>;

struct WorkerEntry {
    stop: Arc<AtomicBool>,
    /// Kept so the thread outlives the entry; never joined. A worker can be
    /// inside a blocking ssh probe for tens of seconds, and `sync` runs on
    /// the client loop — joining there would stall the UI. The flag makes
    /// the thread exit after its in-flight operation; its bridge tears down
    /// on drop.
    #[allow(dead_code)]
    thread: Option<JoinHandle<()>>,
}

impl WorkerEntry {
    fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

impl Drop for WorkerEntry {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Owns one snapshot worker per logged profile, following the endpoint
/// supervisor's profile lifecycle: `sync` starts workers for newly logged
/// profiles, restarts them when the log configuration changes, and stops
/// them when the profile retires or logging turns off.
pub(crate) struct SessionLogManager {
    writer: SessionLogWriter,
    workers: HashMap<ProfileId, WorkerEntry>,
    configs: HashMap<ProfileId, SessionLogProfile>,
    starter: WorkerStarter,
}

impl Default for SessionLogManager {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionLogManager {
    pub(crate) fn new() -> Self {
        Self {
            writer: SessionLogWriter::start(),
            workers: HashMap::new(),
            configs: HashMap::new(),
            starter: spawn_session_log_worker,
        }
    }

    #[cfg(test)]
    fn with_starter(starter: WorkerStarter) -> Self {
        Self {
            starter,
            ..Self::new()
        }
    }

    pub(crate) fn sync(&mut self, profiles: &[SavedSshEndpoint]) {
        let wanted: HashMap<ProfileId, SessionLogProfile> = profiles
            .iter()
            .filter(|profile| profile.enabled)
            .filter_map(|profile| {
                let config = profile.session_log.clone()?;
                config.enabled.then_some((profile.id.clone(), config))
            })
            .collect();
        self.workers.retain(|id, entry| {
            let keep = wanted.contains_key(id);
            if !keep {
                entry.stop();
            }
            keep
        });
        self.configs.retain(|id, _| wanted.contains_key(id));
        for profile in profiles {
            let Some(config) = wanted.get(&profile.id) else {
                continue;
            };
            if self.configs.get(&profile.id) == Some(config)
                && self.workers.contains_key(&profile.id)
            {
                continue;
            }
            if let Some(entry) = self.workers.get_mut(&profile.id) {
                entry.stop();
            }
            let stop = Arc::new(AtomicBool::new(false));
            let thread = match (self.starter)(profile, Arc::clone(&stop), self.writer.queue()) {
                Ok(thread) => thread,
                Err(error) => {
                    tracing::warn!(%error, profile = %profile.label, "session log worker failed to start");
                    continue;
                }
            };
            self.workers.insert(
                profile.id.clone(),
                WorkerEntry {
                    stop,
                    thread: Some(thread),
                },
            );
            self.configs.insert(profile.id.clone(), config.clone());
        }
    }

    pub(crate) fn stop_all(&mut self) {
        for (_, mut entry) in self.workers.drain() {
            entry.stop();
        }
        self.configs.clear();
    }

    /// Snapshots dropped for one profile because the shared writer queue was
    /// full. Surfaced by that machine's detail card.
    pub(crate) fn dropped_for(&self, profile: &ProfileId) -> u64 {
        self.writer.queue.dropped_for(profile)
    }

    #[cfg(test)]
    fn worker_count(&self) -> usize {
        self.workers.len()
    }
}

fn spawn_session_log_worker(
    profile: &SavedSshEndpoint,
    stop: Arc<AtomicBool>,
    queue: SessionLogQueue,
) -> io::Result<JoinHandle<()>> {
    let profile = profile.clone();
    thread::Builder::new()
        .name(format!("session-log-{}", &profile.id.as_str()[..8]))
        .spawn(move || session_log_worker(&profile, &stop, &queue))
}

fn session_log_worker(profile: &SavedSshEndpoint, stop: &Arc<AtomicBool>, queue: &SessionLogQueue) {
    let Some(config) = profile.session_log.clone() else {
        return;
    };
    let interval = Duration::from_secs(
        config
            .dump_interval_secs
            .unwrap_or(DEFAULT_DUMP_INTERVAL_SECS)
            .into(),
    );
    let max_bytes = config.max_bytes.unwrap_or(DEFAULT_MAX_LOG_BYTES);
    let mut bridge: Option<crate::remote::SavedSshApiBridge> = None;
    let mut revisions: HashMap<String, u64> = HashMap::new();
    loop {
        if stop.load(Ordering::Acquire) {
            return;
        }
        if bridge.is_none() {
            match crate::remote::SavedSshApiBridge::start(profile) {
                Ok(started) => bridge = Some(started),
                Err(error) => {
                    tracing::debug!(%error, profile = %profile.label, "session log bridge failed to start");
                    wait_interruptible(interval, stop);
                    continue;
                }
            }
        }
        let Some(active) = bridge.as_ref() else {
            continue;
        };
        let client = crate::api::client::ApiClient::for_target(
            crate::api::client::ConnectionTarget::SocketPath(active.socket_path().to_owned()),
        );
        match dump_snapshots(&client, profile, &config, max_bytes, &mut revisions, queue) {
            Ok(_) => wait_interruptible(interval, stop),
            Err(error) => {
                tracing::debug!(%error, profile = %profile.label, "session log snapshot cycle failed; rebuilding the bridge");
                bridge = None;
                revisions.clear();
                wait_interruptible(interval, stop);
            }
        }
    }
}

fn wait_interruptible(duration: Duration, stop: &Arc<AtomicBool>) {
    let deadline = Instant::now() + duration;
    loop {
        if stop.load(Ordering::Acquire) {
            return;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return;
        }
        thread::sleep(remaining.min(WORKER_STOP_POLL));
    }
}

/// One snapshot cycle: list the machine's panes, read the recent text of
/// each, and append a snapshot for panes whose revision advanced. Per-pane
/// failures are skipped so one broken pane cannot stop the cycle; a list
/// failure fails the cycle (the bridge is rebuilt by the caller). Returns
/// the number of appended snapshots.
fn dump_snapshots(
    client: &crate::api::client::ApiClient,
    profile: &SavedSshEndpoint,
    config: &SessionLogProfile,
    max_bytes: u64,
    revisions: &mut HashMap<String, u64>,
    queue: &SessionLogQueue,
) -> io::Result<usize> {
    let request_id = || format!("session-log:{}", profile.id.as_str());
    let list = request_json(
        client,
        &crate::api::schema::Request {
            id: request_id(),
            method: crate::api::schema::Method::PaneList(
                crate::api::schema::PaneListParams::default(),
            ),
        },
    )?;
    let pane_ids = list
        .pointer("/result/panes")
        .and_then(serde_json::Value::as_array)
        .map(|panes| {
            panes
                .iter()
                .filter_map(|pane| pane.get("pane_id")?.as_str().map(str::to_owned))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let now = time::OffsetDateTime::now_utc();
    let mut appended = 0_usize;
    for pane_id in pane_ids {
        let read = match request_json(
            client,
            &crate::api::schema::Request {
                id: request_id(),
                method: crate::api::schema::Method::PaneRead(crate::api::schema::PaneReadParams {
                    pane_id: pane_id.clone(),
                    source: crate::api::schema::ReadSource::RecentUnwrapped,
                    lines: Some(SNAPSHOT_LINES),
                    format: crate::api::schema::ReadFormat::Text,
                    strip_ansi: true,
                    intent: crate::api::schema::ReadIntent::Passive,
                }),
            },
        ) {
            Ok(read) => read,
            Err(error) => {
                tracing::debug!(%error, pane = %pane_id, "session log pane read failed; skipping");
                continue;
            }
        };
        let revision = read
            .pointer("/result/read/revision")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        if revisions.get(&pane_id) == Some(&revision) {
            continue;
        }
        revisions.insert(pane_id.clone(), revision);
        let Some(text) = read
            .pointer("/result/read/text")
            .and_then(serde_json::Value::as_str)
        else {
            continue;
        };
        let path = match render_log_path(
            config.path_template.as_deref(),
            profile,
            &pane_id,
            now.date(),
        ) {
            Ok(path) => path,
            Err(error) => {
                tracing::warn!(%error, profile = %profile.label, "session log path template failed to render");
                continue;
            }
        };
        let timestamp = now
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_else(|_| date_fallback(now.date()));
        let mut bytes = format!(
            "--- herdr session snapshot {timestamp} machine {} pane {pane_id} revision {revision} ---\n",
            profile.label
        )
        .into_bytes();
        bytes.extend_from_slice(text.as_bytes());
        if !text.is_empty() && !text.ends_with('\n') {
            bytes.push(b'\n');
        }
        queue.append(&profile.id, path, bytes, max_bytes);
        appended += 1;
    }
    Ok(appended)
}

/// Runs one snapshot cycle synchronously, for CLI verification and scripts:
/// builds the machine's API bridge, appends one snapshot per changed pane,
/// and flushes the writer before returning the number of appended
/// snapshots. Logging must be enabled on the profile.
pub(crate) fn dump_session_log_once(profile: &SavedSshEndpoint) -> io::Result<usize> {
    let Some(config) = profile.session_log.clone() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "session logging is not configured for this machine",
        ));
    };
    if !config.enabled {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "session logging is disabled for this machine; enable it first",
        ));
    }
    let bridge = crate::remote::SavedSshApiBridge::start(profile)?;
    let client = crate::api::client::ApiClient::for_target(
        crate::api::client::ConnectionTarget::SocketPath(bridge.socket_path().to_owned()),
    );
    let writer = SessionLogWriter::start();
    let max_bytes = config.max_bytes.unwrap_or(DEFAULT_MAX_LOG_BYTES);
    let mut revisions = HashMap::new();
    let appended = dump_snapshots(
        &client,
        profile,
        &config,
        max_bytes,
        &mut revisions,
        &writer.queue(),
    )?;
    drop(writer);
    Ok(appended)
}

fn date_fallback(date: time::Date) -> String {
    format!(
        "{:04}-{:02}-{:02}",
        date.year(),
        date.month() as u8,
        date.day()
    )
}

fn request_json(
    client: &crate::api::client::ApiClient,
    request: &crate::api::schema::Request,
) -> io::Result<serde_json::Value> {
    let response = client.request_value(request).map_err(|error| match error {
        crate::api::client::ApiClientError::Io(error) => error,
        other => io::Error::other(other),
    })?;
    if let Some(error) = response.get("error") {
        let message = error
            .get("message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("remote request failed");
        return Err(io::Error::other(message.to_string()));
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::endpoint::SshProfileOptions;

    fn profile_with_log(session_log: SessionLogProfile) -> SavedSshEndpoint {
        let mut profile = SavedSshEndpoint::with_options(
            "Build Machine",
            "build.example",
            "agents",
            SshProfileOptions::default(),
        )
        .expect("valid profile");
        profile.session_log = Some(session_log);
        profile
    }

    fn date() -> time::Date {
        time::Date::from_calendar_date(2026, time::Month::September, 17).expect("valid date")
    }

    #[test]
    fn template_validation_rejects_unknown_variables_and_unbalanced_braces() {
        assert!(validate_path_template("{machine}/{date}-{pane}.log").is_ok());
        assert!(validate_path_template("plain.log").is_ok());
        for bad in ["{host}.log", "{pane.log", "pane}.log", "{machine"] {
            assert!(validate_path_template(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn render_log_path_expands_every_variable_under_the_state_root() {
        let profile = profile_with_log(SessionLogProfile::default());
        let path = render_log_path(
            Some("{machine}/{machine-id}/{session}/{date}-{pane}.log"),
            &profile,
            "w1:p2",
            date(),
        )
        .expect("valid template");
        let rendered = path.to_string_lossy();
        assert!(rendered.contains("Build-Machine"), "{rendered}");
        assert!(rendered.contains(&profile.id.as_str()[..8]), "{rendered}");
        assert!(rendered.contains("agents"), "{rendered}");
        assert!(rendered.contains("2026-09-17-w1-p2.log"), "{rendered}");
        assert!(path.starts_with(crate::config::state_dir().join("session-logs")));
    }

    #[test]
    fn render_log_path_keeps_absolute_templates_and_rejects_traversal() {
        let profile = profile_with_log(SessionLogProfile::default());
        let path = render_log_path(
            Some("/tmp/herdr-test/{pane}.log"),
            &profile,
            "w1:p1",
            date(),
        )
        .expect("absolute template");
        assert_eq!(path, PathBuf::from("/tmp/herdr-test/w1-p1.log"));
        assert!(
            render_log_path(Some("../{pane}.log"), &profile, "w1:p1", date()).is_err(),
            "parent traversal must be rejected"
        );
        assert!(render_log_path(Some("a/../../{pane}.log"), &profile, "w1:p1", date()).is_err());
    }

    #[test]
    fn render_log_path_defaults_to_the_machine_date_pane_shape() {
        let profile = profile_with_log(SessionLogProfile::default());
        let path = render_log_path(None, &profile, "w3:p1", date()).expect("default template");
        assert!(path.to_string_lossy().ends_with(&format!(
            "Build-Machine{}2026-09-17-w3-p1.log",
            std::path::MAIN_SEPARATOR
        )));
    }

    #[test]
    fn slugify_maps_unsafe_characters_and_falls_back() {
        assert_eq!(slugify("Build Machine #2", "x"), "Build-Machine--2");
        assert_eq!(slugify("构建机", "machine"), "machine");
        assert_eq!(slugify("...hidden", "x"), "hidden");
    }

    #[test]
    fn writer_batches_rotates_and_flushes_on_drop() {
        let dir =
            std::env::temp_dir().join(format!("herdr-session-log-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let log = dir.join("pane.log");
        let profile = profile_with_log(SessionLogProfile::default());
        {
            let writer = SessionLogWriter::start();
            let queue = writer.queue();
            queue.append(&profile.id, log.clone(), b"first\n".to_vec(), 1024);
            queue.append(&profile.id, log.clone(), b"second\n".to_vec(), 1024);
        }
        assert_eq!(std::fs::read(&log).expect("log file"), b"first\nsecond\n");

        let writer = SessionLogWriter::start();
        let queue = writer.queue();
        // 40 bytes present; appending 90 with a 100-byte cap rotates first.
        queue.append(&profile.id, log.clone(), vec![b'x'; 90], 100);
        drop(writer);
        let rotated = dir.join("pane.log.1");
        assert_eq!(
            std::fs::read(&rotated).expect("rotated file"),
            b"first\nsecond\n"
        );
        assert_eq!(std::fs::read(&log).expect("fresh file"), vec![b'x'; 90]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn writer_queue_is_bounded_and_counts_drops_per_profile() {
        let (sender, _receiver) = mpsc::sync_channel(1);
        let queue = SessionLogQueue {
            sender,
            dropped: Arc::new(SessionLogDrops::default()),
        };
        let profile_a = ProfileId::parse("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").expect("id");
        let profile_b = ProfileId::parse("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").expect("id");
        let path = PathBuf::from("x");
        // Depth 1: the first append fills the queue, the rest drop.
        queue.append(&profile_a, path.clone(), vec![1], 10);
        queue.append(&profile_a, path.clone(), vec![2], 10);
        queue.append(&profile_a, path.clone(), vec![3], 10);
        queue.append(&profile_b, path.clone(), vec![4], 10);
        queue.append(&profile_b, path, vec![5], 10);
        assert_eq!(queue.dropped_for(&profile_a), 2);
        assert_eq!(queue.dropped_for(&profile_b), 2);
        assert_eq!(
            queue.dropped_for(&ProfileId::parse("cccccccccccccccccccccccccccccccc").expect("id")),
            0
        );
    }

    fn fake_starter(
        _profile: &SavedSshEndpoint,
        stop: Arc<AtomicBool>,
        _queue: SessionLogQueue,
    ) -> io::Result<JoinHandle<()>> {
        Ok(thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(5));
            }
        }))
    }

    #[test]
    fn manager_sync_starts_restarts_and_stops_workers_with_the_config() {
        let logging = SessionLogProfile {
            enabled: true,
            path_template: None,
            max_bytes: None,
            dump_interval_secs: None,
        };
        let profile = profile_with_log(logging.clone());
        let mut manager = SessionLogManager::with_starter(fake_starter);
        manager.sync(std::slice::from_ref(&profile));
        assert_eq!(manager.worker_count(), 1);
        // Idempotent while the config is unchanged.
        manager.sync(std::slice::from_ref(&profile));
        assert_eq!(manager.worker_count(), 1);
        // A config change restarts the worker.
        let mut updated = profile.clone();
        updated.session_log = Some(SessionLogProfile {
            enabled: true,
            path_template: Some("{pane}.log".into()),
            ..logging
        });
        manager.sync(std::slice::from_ref(&updated));
        assert_eq!(manager.worker_count(), 1);
        // Turning logging off (or removing the profile) stops the worker.
        manager.sync(&[]);
        assert_eq!(manager.worker_count(), 0);
        let mut disabled = profile;
        disabled.session_log = Some(SessionLogProfile {
            enabled: false,
            ..logging
        });
        manager.sync(&[disabled]);
        assert_eq!(manager.worker_count(), 0);
        manager.stop_all();
    }

    #[test]
    fn stopping_a_worker_never_joins_its_thread() {
        // A worker parked in a blocking operation must not stall `sync` on
        // the client loop.
        let logging = SessionLogProfile {
            enabled: true,
            ..SessionLogProfile::default()
        };
        let profile = profile_with_log(logging);
        let mut manager = SessionLogManager::with_starter(|_profile, stop, _queue| {
            Ok(thread::spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    thread::sleep(Duration::from_secs(60));
                }
            }))
        });
        let started = Instant::now();
        manager.sync(std::slice::from_ref(&profile));
        manager.sync(&[]);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn unused_manager_drops_cleanly() {
        let manager = SessionLogManager::with_starter(fake_starter);
        drop(manager);
    }
}
