//! Saved-profile SSH port forwarding.
//!
//! Each rule runs as an independent `ssh -N -L/-R/-D` child process that
//! reuses the managed ssh config (user config include plus profile fallback
//! directives), exactly like the endpoint bridge — and like the bridge it
//! never multiplexes: a shared control master would keep a torn-down rule's
//! listener (and the mux process itself) alive past the rule's lifetime.
//! Forwards are non-interactive (BatchMode), fail visibly instead of hanging
//! (`ExitOnForwardFailure`), and die with the connection; the owning endpoint
//! supervisor rebuilds them after every successful reconnect.

use std::collections::HashMap;
use std::io;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use super::attach::{
    apply_managed_ssh_options, apply_noninteractive_ssh_options, write_managed_ssh_config,
    ManagedSshConfig, ManagedSshOptions,
};
use crate::client::endpoint::{PortForwardKind, PortForwardRule, ProfileId, SavedSshEndpoint};

/// Pre-check bind address for local/dynamic rules without one. ssh itself
/// binds `localhost`; the deterministic pre-check uses IPv4 loopback and
/// `ExitOnForwardFailure` catches anything racy or IPv6-only.
const DEFAULT_LOCAL_BIND_ADDRESS: &str = "127.0.0.1";
const MONITOR_POLL_INTERVAL: Duration = Duration::from_millis(25);
const MAX_FORWARD_STDERR_BYTES: usize = 8 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PortForwardPhase {
    Active,
    Failed,
}

/// Snapshot of one rule's runtime state for endpoint/UI queries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PortForwardStatus {
    pub(crate) rule: PortForwardRule,
    pub(crate) phase: PortForwardPhase,
    pub(crate) detail: Option<String>,
}

/// One running forwarding child. Dropping it stops the child; the monitor
/// thread records the exit reason into the shared state first.
pub(crate) struct PortForward {
    rule: PortForwardRule,
    state: Arc<Mutex<PortForwardState>>,
    stop: Arc<AtomicBool>,
    monitor: Option<JoinHandle<()>>,
}

struct PortForwardState {
    phase: PortForwardPhase,
    detail: Option<String>,
}

type ForwardStarter = fn(
    rule: &PortForwardRule,
    target: &str,
    options: Option<&ManagedSshOptions>,
) -> io::Result<PortForward>;

impl PortForward {
    fn start(
        rule: &PortForwardRule,
        target: &str,
        options: Option<&ManagedSshOptions>,
    ) -> io::Result<Self> {
        ensure_listen_address_available(rule)?;
        let mut child = forward_command(rule, target, options)
            .spawn()
            .map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("failed to start SSH port forward: {error}"),
                )
            })?;
        let state = Arc::new(Mutex::new(PortForwardState {
            phase: PortForwardPhase::Active,
            detail: None,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let monitor = {
            let state = Arc::clone(&state);
            let stop = Arc::clone(&stop);
            thread::spawn(move || monitor_forward(&mut child, &state, &stop))
        };
        Ok(Self {
            rule: rule.clone(),
            state,
            stop,
            monitor: Some(monitor),
        })
    }

    fn is_active(&self) -> bool {
        lock_forward_state(&self.state).phase == PortForwardPhase::Active
    }

    fn status(&self) -> PortForwardStatus {
        let state = lock_forward_state(&self.state);
        PortForwardStatus {
            rule: self.rule.clone(),
            phase: state.phase,
            detail: state.detail.clone(),
        }
    }
}

impl Drop for PortForward {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(monitor) = self.monitor.take() {
            let _ = monitor.join();
        }
    }
}

fn lock_forward_state(state: &Arc<Mutex<PortForwardState>>) -> MutexGuard<'_, PortForwardState> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn monitor_forward(
    child: &mut Child,
    state: &Arc<Mutex<PortForwardState>>,
    stop: &Arc<AtomicBool>,
) {
    let drain = child.stderr.take().map(|stderr| {
        thread::spawn(move || super::process::read_to_end_bounded(stderr, MAX_FORWARD_STDERR_BYTES))
    });
    let mut exited = None;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                exited = Some(status);
                break;
            }
            Ok(None) => {}
            Err(error) => {
                set_forward_state(
                    state,
                    Some(format!("SSH port forward monitor failed: {error}")),
                );
                break;
            }
        }
        if stop.load(Ordering::Acquire) {
            let _ = child.kill();
            let _ = child.wait();
            break;
        }
        thread::sleep(MONITOR_POLL_INTERVAL);
    }
    let stderr_tail = drain
        .and_then(|drain| drain.join().ok())
        .and_then(|result| result.ok())
        .map(|bytes| String::from_utf8_lossy(&bytes).trim().to_owned())
        .filter(|tail| !tail.is_empty());
    if let Some(status) = exited {
        let detail = match stderr_tail {
            Some(tail) => format!("SSH port forward ended ({status}): {tail}"),
            None => format!("SSH port forward ended ({status})"),
        };
        set_forward_state(state, Some(detail));
    }
}

fn set_forward_state(state: &Arc<Mutex<PortForwardState>>, detail: Option<String>) {
    let mut state = lock_forward_state(state);
    state.phase = PortForwardPhase::Failed;
    state.detail = detail;
}

/// Local and dynamic rules listen on this machine; fail fast with a
/// structured `AddrInUse` instead of an opaque ssh exit. Remote rules listen
/// on the far side, so their bind failures surface through the monitor.
fn ensure_listen_address_available(rule: &PortForwardRule) -> io::Result<()> {
    let bind_address = match rule.kind {
        PortForwardKind::Local | PortForwardKind::Dynamic => rule
            .bind_address
            .as_deref()
            .unwrap_or(DEFAULT_LOCAL_BIND_ADDRESS),
        PortForwardKind::Remote => return Ok(()),
    };
    let listener =
        std::net::TcpListener::bind((bind_address, rule.listen_port)).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "SSH port forward cannot listen on {bind_address}:{}: {error}",
                    rule.listen_port
                ),
            )
        })?;
    drop(listener);
    Ok(())
}

fn ssh_flag(kind: PortForwardKind) -> &'static str {
    match kind {
        PortForwardKind::Local => "-L",
        PortForwardKind::Remote => "-R",
        PortForwardKind::Dynamic => "-D",
    }
}

fn forward_spec(rule: &PortForwardRule) -> String {
    let bind = rule.bind_address.as_deref();
    let listen = match bind {
        Some(bind) => format!("{bind}:{}", rule.listen_port),
        None => rule.listen_port.to_string(),
    };
    match rule.kind {
        PortForwardKind::Local | PortForwardKind::Remote => {
            let target_host = rule.target_host.as_deref().unwrap_or_default();
            let target_port = rule.target_port.unwrap_or_default();
            format!("{listen}:{target_host}:{target_port}")
        }
        PortForwardKind::Dynamic => listen,
    }
}

fn forward_command(
    rule: &PortForwardRule,
    target: &str,
    options: Option<&ManagedSshOptions>,
) -> Command {
    let mut command = Command::new("ssh");
    apply_managed_ssh_options(&mut command, options);
    apply_noninteractive_ssh_options(
        &mut command,
        options.and_then(|options| options.server_alive_interval),
        options.and_then(|options| options.server_alive_count_max),
        options.and_then(|options| options.strict_host_key_checking),
    );
    command
        .arg("-o")
        .arg("ExitOnForwardFailure=yes")
        .arg("-N")
        .arg("-T")
        .arg(ssh_flag(rule.kind))
        .arg(forward_spec(rule))
        .arg(target)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    command
}

/// Tracks the desired and running forwards of every connected SSH endpoint.
/// `rebuild` restarts a profile's whole set after a successful (re)connect;
/// `reconcile` applies catalog rule edits in place while the connection
/// stays up; `stop` tears a set down when the endpoint retires.
pub(crate) struct PortForwardManager {
    sets: HashMap<ProfileId, ForwardSet>,
    starter: ForwardStarter,
}

struct ForwardSet {
    // Field order matters: entries (children) drop before the managed config
    // removes the directory holding the config file they were started with.
    entries: Vec<ForwardEntry>,
    config: Option<ManagedSshConfig>,
}

struct ForwardEntry {
    rule: PortForwardRule,
    state: ForwardEntryState,
}

enum ForwardEntryState {
    Running(PortForward),
    Failed(String),
}

impl Default for PortForwardManager {
    fn default() -> Self {
        Self::new()
    }
}

impl PortForwardManager {
    pub(crate) fn new() -> Self {
        Self {
            sets: HashMap::new(),
            starter: PortForward::start,
        }
    }

    /// Restarts every forward of a profile. Called by the endpoint supervisor
    /// after a connection (re)establishes; also stops forwards of profiles
    /// whose rules were all removed.
    pub(crate) fn rebuild(&mut self, profile: &SavedSshEndpoint) {
        self.stop(&profile.id);
        if profile.port_forwards.is_empty() {
            return;
        }
        let profile_options = match super::saved::saved_profile_ssh_options(profile) {
            Ok(options) => options,
            Err(error) => {
                self.insert_config_failure(profile, format!("invalid SSH options: {error}"));
                return;
            }
        };
        let config = match write_managed_ssh_config(profile_options.as_ref()) {
            Ok(mut config) => {
                // Forward children are long-lived independent processes (see
                // the module docs): no control master for this set.
                config.options.control_path = None;
                Some(config)
            }
            Err(error) => {
                self.insert_config_failure(
                    profile,
                    format!("failed to write managed ssh config: {error}"),
                );
                return;
            }
        };
        let options = config.as_ref().map(|config| config.options.clone());
        let starter = self.starter;
        let entries = profile
            .port_forwards
            .iter()
            .map(|rule| spawn_entry(starter, rule, &profile.target, options.as_ref()))
            .collect();
        self.sets
            .insert(profile.id.clone(), ForwardSet { entries, config });
    }

    /// Applies rule edits for a profile whose connection stays up: removed
    /// rules stop, added rules start, failed rules that are still wanted get
    /// a fresh attempt, and healthy forwards keep running. A profile with no
    /// tracked set — it connected with zero rules, or its set was torn down
    /// when the rules were emptied — gets a full rebuild, exactly like a
    /// fresh connect, so the 0 -> N transition never waits for a reconnect.
    pub(crate) fn reconcile(&mut self, profile: &SavedSshEndpoint) {
        if profile.port_forwards.is_empty() {
            self.stop(&profile.id);
            return;
        }
        let Some(set) = self.sets.get_mut(&profile.id) else {
            self.rebuild(profile);
            return;
        };
        let starter = self.starter;
        let options = set.config.as_ref().map(|config| config.options.clone());
        set.entries
            .retain(|entry| profile.port_forwards.contains(&entry.rule));
        for rule in &profile.port_forwards {
            match set.entries.iter_mut().find(|entry| &entry.rule == rule) {
                Some(entry) if matches!(&entry.state, ForwardEntryState::Running(forward) if forward.is_active()) =>
                    {}
                Some(entry) => {
                    *entry = spawn_entry(starter, rule, &profile.target, options.as_ref());
                }
                None => set.entries.push(spawn_entry(
                    starter,
                    rule,
                    &profile.target,
                    options.as_ref(),
                )),
            }
        }
    }

    pub(crate) fn stop(&mut self, profile_id: &ProfileId) {
        self.sets.remove(profile_id);
    }

    pub(crate) fn stop_all(&mut self) {
        self.sets.clear();
    }

    pub(crate) fn status(&self, profile_id: &ProfileId) -> Vec<PortForwardStatus> {
        self.sets
            .get(profile_id)
            .map(|set| {
                set.entries
                    .iter()
                    .map(|entry| match &entry.state {
                        ForwardEntryState::Running(forward) => forward.status(),
                        ForwardEntryState::Failed(detail) => PortForwardStatus {
                            rule: entry.rule.clone(),
                            phase: PortForwardPhase::Failed,
                            detail: Some(detail.clone()),
                        },
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn insert_config_failure(&mut self, profile: &SavedSshEndpoint, detail: String) {
        self.sets.insert(
            profile.id.clone(),
            ForwardSet {
                entries: profile
                    .port_forwards
                    .iter()
                    .map(|rule| ForwardEntry {
                        rule: rule.clone(),
                        state: ForwardEntryState::Failed(detail.clone()),
                    })
                    .collect(),
                config: None,
            },
        );
    }
}

fn spawn_entry(
    starter: ForwardStarter,
    rule: &PortForwardRule,
    target: &str,
    options: Option<&ManagedSshOptions>,
) -> ForwardEntry {
    match starter(rule, target, options) {
        Ok(forward) => ForwardEntry {
            rule: rule.clone(),
            state: ForwardEntryState::Running(forward),
        },
        Err(error) => ForwardEntry {
            rule: rule.clone(),
            state: ForwardEntryState::Failed(error.to_string()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::endpoint::SshProfileOptions;

    fn rule(kind: PortForwardKind, listen_port: u16) -> PortForwardRule {
        let (target_host, target_port) = match kind {
            PortForwardKind::Local | PortForwardKind::Remote => {
                (Some("127.0.0.1".to_string()), Some(80))
            }
            PortForwardKind::Dynamic => (None, None),
        };
        PortForwardRule {
            kind,
            bind_address: None,
            listen_port,
            target_host,
            target_port,
        }
    }

    fn profile_with_forwards(port_forwards: Vec<PortForwardRule>) -> SavedSshEndpoint {
        let mut profile = SavedSshEndpoint::with_options(
            "build",
            "build.example",
            "default",
            SshProfileOptions::default(),
        )
        .expect("valid profile");
        profile.port_forwards = port_forwards;
        profile
    }

    fn starter_succeeds(
        rule: &PortForwardRule,
        _target: &str,
        _options: Option<&ManagedSshOptions>,
    ) -> io::Result<PortForward> {
        Ok(PortForward {
            rule: rule.clone(),
            state: Arc::new(Mutex::new(PortForwardState {
                phase: PortForwardPhase::Active,
                detail: None,
            })),
            stop: Arc::new(AtomicBool::new(false)),
            monitor: None,
        })
    }

    fn starter_fails(
        _rule: &PortForwardRule,
        _target: &str,
        _options: Option<&ManagedSshOptions>,
    ) -> io::Result<PortForward> {
        Err(io::Error::other("no ssh here"))
    }

    fn manager_with(starter: ForwardStarter) -> PortForwardManager {
        PortForwardManager {
            sets: HashMap::new(),
            starter,
        }
    }

    #[test]
    fn forward_spec_covers_all_kinds_with_and_without_bind() {
        let mut local = rule(PortForwardKind::Local, 8080);
        assert_eq!(forward_spec(&local), "8080:127.0.0.1:80");
        local.bind_address = Some("0.0.0.0".into());
        assert_eq!(forward_spec(&local), "0.0.0.0:8080:127.0.0.1:80");

        let remote = rule(PortForwardKind::Remote, 9000);
        assert_eq!(forward_spec(&remote), "9000:127.0.0.1:80");

        let mut dynamic = rule(PortForwardKind::Dynamic, 1080);
        assert_eq!(forward_spec(&dynamic), "1080");
        dynamic.bind_address = Some("127.0.0.1".into());
        assert_eq!(forward_spec(&dynamic), "127.0.0.1:1080");
    }

    #[test]
    fn forward_command_is_noninteractive_and_fails_on_forward_failure() {
        let command = forward_command(&rule(PortForwardKind::Local, 8080), "build.example", None);
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(command.get_program(), "ssh");
        for expected in [
            "BatchMode=yes",
            "NumberOfPasswordPrompts=0",
            "ExitOnForwardFailure=yes",
        ] {
            assert!(args.iter().any(|arg| arg == expected), "{args:?}");
        }
        let flag = args.iter().position(|arg| arg == "-L").expect("-L present");
        assert_eq!(args[flag + 1], "8080:127.0.0.1:80");
        assert_eq!(args.last().map(String::as_str), Some("build.example"));
        assert!(args.contains(&"-N".to_string()));
        assert!(args.contains(&"-T".to_string()));
    }

    #[test]
    fn occupied_local_listen_port_is_a_structured_error_before_spawn() {
        let occupied = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind probe");
        let port = occupied.local_addr().expect("probe address").port();
        let error = ensure_listen_address_available(&rule(PortForwardKind::Local, port))
            .expect_err("occupied port must fail");
        assert_eq!(error.kind(), io::ErrorKind::AddrInUse, "{error}");
        assert!(error.to_string().contains(&port.to_string()), "{error}");

        let error = ensure_listen_address_available(&rule(PortForwardKind::Dynamic, port))
            .expect_err("occupied dynamic port must fail");
        assert_eq!(error.kind(), io::ErrorKind::AddrInUse, "{error}");

        // Remote rules listen on the far side: no local pre-check.
        assert!(ensure_listen_address_available(&rule(PortForwardKind::Remote, port)).is_ok());
    }

    #[test]
    fn rebuild_records_spawn_failures_and_status_reports_them() {
        let profile = profile_with_forwards(vec![
            rule(PortForwardKind::Local, 18080),
            rule(PortForwardKind::Dynamic, 11080),
        ]);
        let mut manager = manager_with(starter_fails);
        manager.rebuild(&profile);

        let statuses = manager.status(&profile.id);
        assert_eq!(statuses.len(), 2);
        for status in &statuses {
            assert_eq!(status.phase, PortForwardPhase::Failed);
            assert_eq!(status.detail.as_deref(), Some("no ssh here"));
        }

        manager.stop(&profile.id);
        assert!(manager.status(&profile.id).is_empty());
    }

    #[test]
    fn reconcile_keeps_healthy_forwards_and_restarts_failed_ones() {
        let mut wanted = rule(PortForwardKind::Local, 18081);
        wanted.bind_address = Some("10.0.0.1".into());
        let profile = profile_with_forwards(vec![wanted.clone()]);
        let mut manager = manager_with(starter_succeeds);
        manager.rebuild(&profile);
        assert_eq!(manager.status(&profile.id).len(), 1);

        // Unchanged rules keep their running forward; a failed entry with the
        // same rule is restarted.
        let set = manager.sets.get_mut(&profile.id).expect("tracked set");
        let failed = Arc::new(Mutex::new(PortForwardState {
            phase: PortForwardPhase::Failed,
            detail: Some("died".into()),
        }));
        set.entries.clear();
        set.entries.push(ForwardEntry {
            rule: wanted.clone(),
            state: ForwardEntryState::Running(PortForward {
                rule: wanted.clone(),
                state: failed,
                stop: Arc::new(AtomicBool::new(false)),
                monitor: None,
            }),
        });
        manager.reconcile(&profile);
        let statuses = manager.status(&profile.id);
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].phase, PortForwardPhase::Active);

        // Adding a rule starts it; removing stops it; empty rule list stops all.
        let mut updated = profile_with_forwards(Vec::new());
        updated.id = profile.id.clone();
        updated.port_forwards = vec![wanted.clone(), rule(PortForwardKind::Dynamic, 11081)];
        manager.reconcile(&updated);
        assert_eq!(manager.status(&profile.id).len(), 2);
        manager.reconcile(&updated);
        assert_eq!(manager.status(&profile.id).len(), 2);
        let mut emptied = updated.clone();
        emptied.port_forwards = Vec::new();
        manager.reconcile(&emptied);
        assert!(manager.status(&profile.id).is_empty());
    }

    #[test]
    fn reconcile_rebuilds_when_the_profile_has_no_tracked_set() {
        // The connection came up with zero rules (or the set was torn down
        // when the rules were emptied): a freshly added rule starts the
        // whole set instead of waiting for the next reconnect.
        let profile = profile_with_forwards(vec![rule(PortForwardKind::Local, 18082)]);
        let mut manager = manager_with(starter_succeeds);
        manager.reconcile(&profile);
        let statuses = manager.status(&profile.id);
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].phase, PortForwardPhase::Active);
    }

    #[test]
    fn rebuild_never_multiplexes_forward_children() {
        // A shared control master would keep a torn-down rule's listener
        // (and the mux process itself) alive past the rule's lifetime.
        let profile = profile_with_forwards(vec![rule(PortForwardKind::Local, 18083)]);
        let mut manager = manager_with(starter_succeeds);
        manager.rebuild(&profile);
        let set = manager.sets.get(&profile.id).expect("tracked set");
        let config = set.config.as_ref().expect("managed config");
        assert!(config.options.control_path.is_none());
    }
}
