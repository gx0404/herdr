#![cfg(unix)]

//! Real-machine end-to-end regression for the saved-machine (SSH)
//! productization, driven against a user-level OpenSSH `sshd` on 127.0.0.1.
//! Everything (two fake HOMEs, the sshd itself, known_hosts, herdr sessions,
//! forwarded ports, log files) lives under one temporary root that is
//! removed on drop; the user's real `~/.ssh` and default Herdr session are
//! never touched.
//!
//! Covered chains:
//!
//! 1. port-forward rules written to the catalog (`machine forward add`) are
//!    picked up by the running client's catalog watcher and applied by the
//!    endpoint supervisor / `PortForwardManager`: initial rules at connect,
//!    rule edits while the connection stays up, the 0 -> N rules
//!    transition, and rebuild after an sshd outage + recovery.
//! 2. broadcast fan-out across two saved machines plus Local through
//!    `broadcast send`: one-pane-per-endpoint registration dedup, per-target
//!    failure reporting, and the disabled/empty safety gates.
//! 3. profile session logs of a real remote pane: interval dumps land in
//!    the rendered path template, size rotation produces `<file>.1`, and
//!    `machine log dump` runs one synchronous cycle.
//! 4. `machine import` regression for configs with wildcard hosts,
//!    multi-hop ProxyJump and multiple IdentityFile directives (a separate
//!    test that needs no sshd).

use std::fs;
use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

const HERDR: &str = env!("CARGO_BIN_EXE_herdr");
const SSHD_CANDIDATES: &[&str] = &["/usr/sbin/sshd", "/usr/bin/sshd", "/sbin/sshd"];
const SERVICE_BODY: &[u8] = b"E2E-SVC-OK";

/// Environment variables inherited from an enclosing Herdr session that must
/// never leak into the isolated client/remote environments.
const SCRUB_ENV: &[&str] = &[
    "HERDR_ENV",
    "HERDR_SESSION",
    "HERDR_SOCKET_PATH",
    "HERDR_CLIENT_SOCKET_PATH",
    "HERDR_WORKSPACE_ID",
    "HERDR_TAB_ID",
    "HERDR_PANE_ID",
    "HERDR_REMOTE_BINARY",
    "HERDR_BIN_PATH",
    "HERDR_STARTUP_CWD",
];

fn app_dir_name() -> &'static str {
    if cfg!(debug_assertions) {
        "herdr-dev"
    } else {
        "herdr"
    }
}

fn log(phase: &str, message: &str) {
    eprintln!("[ssh-e2e {phase}] {message}");
}

fn free_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
        .expect("bind ephemeral port")
        .local_addr()
        .expect("local addr")
        .port()
}

fn on_path(tool: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(tool).is_file()))
}

fn sshd_path() -> Option<PathBuf> {
    SSHD_CANDIDATES
        .iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
        .or_else(|| on_path("sshd").then(|| PathBuf::from("sshd")))
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn expect_code(output: &Output, code: i32, what: &str) {
    assert_eq!(
        output.status.code(),
        Some(code),
        "{what}: exit {:?}\nstdout: {}\nstderr: {}",
        output.status.code(),
        stdout(output),
        stderr(output)
    );
}

fn wait_until(what: &str, timeout: Duration, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    loop {
        if check() {
            log("wait", &format!("{what}: satisfied"));
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(300));
    }
}

/// One isolated environment side (client or remote): a fake HOME plus the
/// XDG directories, with every inherited Herdr variable scrubbed.
struct Side {
    root: PathBuf,
    config_path: Option<PathBuf>,
}

impl Side {
    fn new(root: &Path, name: &str, config_path: Option<PathBuf>) -> Self {
        let root = root.join(name);
        for dir in [
            "home",
            "config",
            "state",
            "run",
            "cache",
            "home/.ssh",
            "home/.local/bin",
        ] {
            fs::create_dir_all(root.join(dir)).expect("create side dirs");
        }
        Self { root, config_path }
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    fn state_dir(&self) -> PathBuf {
        self.root.join("state")
    }

    fn command(&self, session: Option<&str>, args: &[&str]) -> Command {
        let mut command = Command::new(HERDR);
        command
            .args(args)
            .env("HOME", self.home())
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.state_dir())
            .env("XDG_RUNTIME_DIR", self.root.join("run"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("HERDR_LANG", "en")
            .env("TERM", "xterm-256color")
            .stdin(Stdio::null());
        for name in SCRUB_ENV {
            command.env_remove(name);
        }
        if let Some(name) = session {
            command.env("HERDR_SESSION", name);
        }
        match &self.config_path {
            Some(path) => {
                command.env("HERDR_CONFIG_PATH", path);
            }
            None => {
                command.env_remove("HERDR_CONFIG_PATH");
            }
        }
        command
    }

    fn run(&self, session: Option<&str>, args: &[&str]) -> Output {
        self.command(session, args)
            .output()
            .unwrap_or_else(|error| panic!("run herdr {args:?}: {error}"))
    }

    fn catalog_json(&self) -> serde_json::Value {
        let path = self
            .state_dir()
            .join(app_dir_name())
            .join("client")
            .join("endpoints.json");
        let content = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("catalog {} unreadable: {error}", path.display()));
        serde_json::from_str(&content).expect("catalog is valid json")
    }

    fn profile_id(&self, label: &str) -> String {
        self.catalog_json()["ssh"]
            .as_array()
            .expect("ssh profiles array")
            .iter()
            .find(|profile| profile["label"] == label)
            .unwrap_or_else(|| panic!("profile {label} saved"))
            .get("id")
            .and_then(serde_json::Value::as_str)
            .expect("profile id")
            .to_owned()
    }

    /// First pane id of the given session, addressed through this side's env.
    fn first_pane_id(&self, session: &str) -> String {
        let output = self.run(Some(session), &["pane", "list"]);
        expect_code(&output, 0, "pane list");
        let response: serde_json::Value =
            serde_json::from_str(&stdout(&output)).expect("pane list json");
        response
            .pointer("/result/panes/0/pane_id")
            .and_then(serde_json::Value::as_str)
            .expect("at least one pane")
            .to_owned()
    }

    fn pane_wait_output(&self, session: &str, pane: &str, marker: &str, timeout_ms: u32) -> Output {
        self.run(
            Some(session),
            &[
                "pane",
                "wait-output",
                pane,
                "--match",
                marker,
                "--timeout",
                &timeout_ms.to_string(),
            ],
        )
    }
}

/// Minimal HTTP service on 127.0.0.1 acting as the port-forward target.
fn start_service(port: u16, stop: Arc<AtomicBool>) {
    let listener = TcpListener::bind(("127.0.0.1", port)).expect("bind forward target service");
    listener.set_nonblocking(true).expect("nonblocking service");
    thread::spawn(move || {
        while !stop.load(Ordering::Acquire) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let mut buf = [0_u8; 1024];
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                    let _ = stream.read(&mut buf);
                    let _ = stream.write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            SERVICE_BODY.len()
                        )
                        .as_bytes(),
                    );
                    let _ = stream.write_all(SERVICE_BODY);
                }
                Err(_) => thread::sleep(Duration::from_millis(25)),
            }
        }
    });
}

fn http_ok(port: u16) -> bool {
    let address = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let Ok(mut stream) = TcpStream::connect_timeout(&address, Duration::from_millis(700)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(900)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(700)));
    if stream
        .write_all(b"GET / HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n")
        .is_err()
    {
        return false;
    }
    let mut body = Vec::new();
    let mut buf = [0_u8; 1024];
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => body.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
        if body.windows(SERVICE_BODY.len()).any(|w| w == SERVICE_BODY) {
            return true;
        }
        if Instant::now() > deadline {
            break;
        }
    }
    body.windows(SERVICE_BODY.len()).any(|w| w == SERVICE_BODY)
}

fn refused(port: u16) -> bool {
    let address = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    match TcpStream::connect_timeout(&address, Duration::from_millis(500)) {
        Err(error) => error.kind() == std::io::ErrorKind::ConnectionRefused,
        Ok(_) => false,
    }
}

fn spawn_sshd(sshd: &Path, config: &Path, log_file: &Path) -> Child {
    let log = fs::File::create(log_file).expect("sshd log file");
    Command::new(sshd)
        .arg("-D")
        .arg("-e")
        .arg("-f")
        .arg(config)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log))
        .process_group(0)
        .spawn()
        .expect("spawn sshd")
}

/// SIGKILLs the child's whole process group: per-connection sshd children
/// must die with their master, or the outage simulation would keep existing
/// sessions alive.
fn kill_process_group(child: &mut Child) {
    // SAFETY: the negative PID targets only this child's private process
    // group (spawned with `process_group(0)`).
    unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) };
    let _ = child.wait();
}

/// `/proc/<pid>/stat` start time (field 22), used as a pid-reuse fence when
/// killing recorded pids later.
fn process_starttime(pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after = stat.rfind(')')?;
    // Fields after `comm`: index 0 is state (field 3), so starttime (field
    // 22) lands at index 19.
    stat[after + 1..].split_whitespace().nth(19)?.parse().ok()
}

fn process_ppid(pid: u32) -> Option<u32> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after = stat.rfind(')')?;
    stat[after + 1..].split_whitespace().nth(1)?.parse().ok()
}

fn descendants_of(root_pid: u32) -> Vec<u32> {
    let mut links = Vec::new();
    if let Ok(entries) = fs::read_dir("/proc") {
        for entry in entries.flatten() {
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
                continue;
            };
            if let Some(ppid) = process_ppid(pid) {
                links.push((pid, ppid));
            }
        }
    }
    let mut descendants = Vec::new();
    let mut frontier = vec![root_pid];
    while let Some(parent) = frontier.pop() {
        for (pid, ppid) in &links {
            if *ppid == parent && !descendants.contains(pid) {
                descendants.push(*pid);
                frontier.push(*pid);
            }
        }
    }
    descendants
}

/// sshd moves every accepted connection into its own session/process group,
/// so killing only the master's group leaves live sessions (and their
/// clients, including `ssh -N -L` forwards) completely unaffected. Snapshot
/// the master's descendants first, kill the master's group, then kill each
/// recorded connection process with a pid-reuse fence.
fn kill_sshd_tree(child: &mut Child) {
    let victims: Vec<(u32, Option<u64>)> = descendants_of(child.id())
        .into_iter()
        .map(|pid| (pid, process_starttime(pid)))
        .collect();
    kill_process_group(child);
    for (pid, starttime) in victims {
        if process_starttime(pid) == starttime {
            // SAFETY: the pid was recorded from /proc moments ago and its
            // start time still matches, so it is the same process.
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
        }
    }
}

/// Best-effort sweep of stray processes (ssh control masters, bridges,
/// remote servers) whose command line references the test root.
fn kill_processes_referencing(root: &Path) {
    let needle = root.as_os_str().as_bytes().to_vec();
    let self_pid = std::process::id();
    let Ok(entries) = fs::read_dir("/proc") else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Ok(pid) = name.to_string_lossy().parse::<u32>() else {
            continue;
        };
        if pid == self_pid {
            continue;
        }
        let Ok(cmdline) = fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        if cmdline
            .windows(needle.len())
            .any(|window| window == needle.as_slice())
        {
            // SAFETY: the PID was just read from /proc and matched the
            // unique per-test root path.
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
        }
    }
}

struct Tui {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    master: Box<dyn portable_pty::MasterPty + Send>,
    reader: Option<thread::JoinHandle<()>>,
    output: Arc<Mutex<String>>,
}

fn spawn_tui(client: &Side, session: &str, cwd: &Path) -> Tui {
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 30,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");
    let mut command = CommandBuilder::new(HERDR);
    command.args(["--session", session]);
    command.env("HOME", client.home());
    command.env("XDG_CONFIG_HOME", client.root.join("config"));
    command.env("XDG_STATE_HOME", client.state_dir());
    command.env("XDG_RUNTIME_DIR", client.root.join("run"));
    command.env("XDG_CACHE_HOME", client.root.join("cache"));
    command.env("HERDR_LANG", "en");
    command.env("TERM", "xterm-256color");
    if let Some(path) = &client.config_path {
        command.env("HERDR_CONFIG_PATH", path);
    }
    for name in SCRUB_ENV {
        command.env_remove(name);
    }
    command.cwd(cwd);
    let child = pair.slave.spawn_command(command).expect("spawn nested TUI");
    drop(pair.slave);
    let mut master_reader = pair.master.try_clone_reader().expect("pty reader");
    let output = Arc::new(Mutex::new(String::new()));
    let sink = Arc::clone(&output);
    let reader = thread::spawn(move || {
        let mut buf = [0_u8; 8192];
        loop {
            match master_reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let mut output = sink.lock().expect("tui output buffer");
                    output.push_str(&String::from_utf8_lossy(&buf[..n]));
                    let len = output.len();
                    if len > 64 * 1024 {
                        output.drain(..len - 64 * 1024);
                    }
                }
            }
        }
    });
    Tui {
        child,
        master: pair.master,
        reader: Some(reader),
        output,
    }
}

struct Rig {
    root: PathBuf,
    sshd_binary: PathBuf,
    sshd_config: PathBuf,
    sshd_log: PathBuf,
    sshd: Option<Child>,
    tui: Option<Tui>,
    service_stop: Arc<AtomicBool>,
    client: Side,
    remote: Side,
}

impl Rig {
    fn start_sshd(&mut self) {
        assert!(self.sshd.is_none(), "sshd already running");
        let mut child = spawn_sshd(&self.sshd_binary, &self.sshd_config, &self.sshd_log);
        // The listener is ready once the pid file exists; give it a moment.
        wait_until("sshd listener", Duration::from_secs(10), || {
            child.try_wait().expect("sshd try_wait").is_none()
                && self
                    .sshd_config
                    .parent()
                    .map(|dir| dir.join("sshd.pid").exists())
                    .unwrap_or(false)
        });
        self.sshd = Some(child);
    }

    fn stop_sshd(&mut self) {
        if let Some(mut child) = self.sshd.take() {
            kill_sshd_tree(&mut child);
        }
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let mut tui_pid = None;
        if let Some(mut tui) = self.tui.take() {
            tui_pid = tui.child.process_id();
            if let Some(pid) = tui_pid {
                // SAFETY: the negative PID targets only the nested TUI's
                // pty session process group.
                unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGKILL) };
            }
            let _ = tui.child.kill();
            let _ = tui.child.wait();
            drop(tui.master);
            if let Some(reader) = tui.reader.take() {
                let _ = reader.join();
            }
        }
        // Stop the herdr servers before removing their state: the nested
        // client session and the two remote sessions are detached daemons.
        for (side, session) in [
            (&self.client, "e2etest"),
            (&self.remote, "e2ea"),
            (&self.remote, "e2eb"),
        ] {
            let _ = side.run(None, &["session", "stop", session]);
        }
        self.stop_sshd();
        self.service_stop.store(true, Ordering::Release);
        kill_processes_referencing(&self.root);
        sweep_managed_ssh_configs(&self.root, tui_pid);
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Removes `/tmp/herdr-ssh-<pid>-*` managed-config leftovers of the test's
/// own processes and any whose config references the test root. A SIGKILLed
/// client cannot run the `ManagedSshConfigDir` destructors, so the rig
/// sweeps them here instead of leaving ssh configs behind.
fn sweep_managed_ssh_configs(root: &Path, tui_pid: Option<u32>) {
    let needle = root.as_os_str().as_bytes().to_vec();
    let tui_prefix = tui_pid.map(|pid| format!("herdr-ssh-{pid}-"));
    let Ok(entries) = fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("herdr-ssh-") {
            continue;
        }
        let owned_by_tui = tui_prefix
            .as_ref()
            .is_some_and(|prefix| name.starts_with(prefix));
        let references_root = fs::read(entry.path().join("config"))
            .map(|content| {
                content
                    .windows(needle.len())
                    .any(|window| window == needle.as_slice())
            })
            .unwrap_or(false);
        if owned_by_tui || references_root {
            if entry.path().is_dir() {
                let _ = fs::remove_dir_all(entry.path());
            } else {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
}

fn write_file(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent dir");
    }
    fs::write(path, content).expect("write file");
}

fn tool_or_skip(tool: &str) -> bool {
    if on_path(tool) {
        true
    } else {
        eprintln!("[ssh-e2e] SKIP: required tool `{tool}` not found on PATH");
        false
    }
}

#[test]
fn sshd_real_machine_end_to_end() {
    let Some(sshd_binary) = sshd_path() else {
        eprintln!("[ssh-e2e] SKIP: no OpenSSH sshd available on this machine");
        return;
    };
    for tool in ["ssh", "ssh-keygen", "ssh-keyscan"] {
        if !tool_or_skip(tool) {
            return;
        }
    }

    let phase = "setup";
    let root = std::env::temp_dir().join(format!("herdr-ssh-e2e-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("create e2e root");

    let user = std::env::var("USER").expect("USER is set");
    let sshd_port = free_port();
    let service_port = free_port();
    let forward_a_initial = free_port();
    let forward_a_edited = free_port();
    let forward_a_refilled = free_port();
    let forward_b_first = free_port();

    // Keys and sshd configuration.
    let keys_dir = root.join("keys");
    fs::create_dir_all(&keys_dir).expect("keys dir");
    let sshd_dir = root.join("sshd");
    fs::create_dir_all(&sshd_dir).expect("sshd dir");
    let client_key = keys_dir.join("id_ed25519");
    for args in [
        vec![
            "-t",
            "ed25519",
            "-N",
            "",
            "-C",
            "herdr-e2e-client",
            "-f",
            &client_key.to_string_lossy(),
        ],
        vec![
            "-t",
            "ed25519",
            "-N",
            "",
            "-C",
            "herdr-e2e-host",
            "-f",
            &root.join("sshd/ssh_host_ed25519_key").to_string_lossy(),
        ],
    ] {
        let output = Command::new("ssh-keygen")
            .args(&args)
            .output()
            .expect("ssh-keygen");
        assert!(output.status.success(), "ssh-keygen: {}", stderr(&output));
    }
    fs::copy(
        keys_dir.join("id_ed25519.pub"),
        keys_dir.join("authorized_keys"),
    )
    .expect("authorized_keys");

    let client_config_path = root.join("client-config.toml");
    write_file(
        &client_config_path,
        "onboarding = false\n[experimental]\nallow_nested = true\n",
    );
    let client = Side::new(&root, "client", Some(client_config_path));
    let remote = Side::new(&root, "remote", None);

    // The "remote" machine gets the same build installed as a remote herdr.
    let remote_herdr = remote.home().join(".local/bin/herdr");
    if fs::hard_link(HERDR, &remote_herdr).is_err() {
        fs::copy(HERDR, &remote_herdr).expect("install remote herdr binary");
    }
    // An empty remote HOME makes interactive zsh stop at the
    // zsh-newuser-install wizard, which silently consumes typed input and
    // would freeze the pane revision the session-log and broadcast
    // scenarios depend on; a stub .zshrc skips it deterministically.
    write_file(&remote.home().join(".zshrc"), "# herdr ssh e2e\n");

    let sshd_config = sshd_dir.join("sshd_config");
    write_file(
        &sshd_config,
        &format!(
            "Port {sshd_port}\n\
             ListenAddress 127.0.0.1\n\
             HostKey {sshd_dir}/ssh_host_ed25519_key\n\
             PidFile {sshd_dir}/sshd.pid\n\
             AuthorizedKeysFile {keys_dir}/authorized_keys\n\
             PasswordAuthentication no\n\
             ChallengeResponseAuthentication no\n\
             UsePAM no\n\
             StrictModes no\n\
             AllowTcpForwarding yes\n\
             AllowAgentForwarding yes\n\
             X11Forwarding no\n\
             PermitTunnel no\n\
             Subsystem sftp internal-sftp\n\
             LogLevel VERBOSE\n\
             SetEnv HOME={remote_home} \
             XDG_CONFIG_HOME={remote}/config \
             XDG_STATE_HOME={remote}/state \
             XDG_RUNTIME_DIR={remote}/run \
             XDG_CACHE_HOME={remote}/cache \
             PATH={remote_home}/.local/bin:/usr/local/bin:/usr/bin:/bin\n",
            sshd_dir = sshd_dir.display(),
            keys_dir = keys_dir.display(),
            remote = remote.root.display(),
            remote_home = remote.home().display(),
        ),
    );

    // The client ssh config feeds `machine add --from-config`. Real ssh
    // resolves `~` through the passwd database, so a fake HOME alone cannot
    // relocate known_hosts: every herdr-driven ssh runs `-F <managed>` and
    // the managed config includes this user config, so pin the known_hosts
    // path here (this also keeps the real ~/.ssh completely untouched).
    let known_hosts = client.home().join(".ssh/known_hosts");
    write_file(
        &client.home().join(".ssh/config"),
        &format!(
            "Host e2e-a\n\
             \x20   HostName 127.0.0.1\n\
             \x20   Port {sshd_port}\n\
             \x20   User {user}\n\
             \x20   IdentityFile {client_key}\n\
             \x20   IdentitiesOnly yes\n\
             \n\
             Host *\n\
             \x20   UserKnownHostsFile {known_hosts}\n",
            client_key = client_key.display(),
            known_hosts = known_hosts.display(),
        ),
    );

    let service_stop = Arc::new(AtomicBool::new(false));
    start_service(service_port, Arc::clone(&service_stop));

    let mut rig = Rig {
        root: root.clone(),
        sshd_binary,
        sshd_config,
        sshd_log: sshd_dir.join("sshd.log"),
        sshd: None,
        tui: None,
        service_stop,
        client,
        remote,
    };
    rig.start_sshd();
    log(phase, &format!("sshd up on 127.0.0.1:{sshd_port}"));

    // Pre-seed known_hosts: saved profiles default to StrictHostKeyChecking=yes.
    let scan = Command::new("ssh-keyscan")
        .args(["-T", "5", "-p", &sshd_port.to_string(), "127.0.0.1"])
        .output()
        .expect("ssh-keyscan");
    assert!(scan.status.success(), "ssh-keyscan: {}", stderr(&scan));
    write_file(
        &rig.client.home().join(".ssh/known_hosts"),
        &String::from_utf8_lossy(&scan.stdout),
    );

    // --- Machine registration ------------------------------------------
    let phase = "machine-add";
    let add_a = rig.client.run(
        None,
        &[
            "machine",
            "add",
            "--from-config",
            "e2e-a",
            "--label",
            "machine-a",
            "--remote-session",
            "e2ea",
        ],
    );
    expect_code(&add_a, 0, "machine add --from-config e2e-a");
    let add_b = rig.client.run(
        None,
        &[
            "machine",
            "add",
            "127.0.0.1",
            "--label",
            "machine-b",
            "--port",
            &sshd_port.to_string(),
            "--user",
            &user,
            "--identity-file",
            &client_key.to_string_lossy(),
            "--identities-only",
            "--remote-session",
            "e2eb",
        ],
    );
    expect_code(&add_b, 0, "machine add machine-b");
    let id_a = rig.client.profile_id("machine-a");
    let id_b = rig.client.profile_id("machine-b");
    log(phase, &format!("machine-a={id_a} machine-b={id_b}"));

    // `add --from-config` resolved the alias into structured profile fields.
    let catalog = rig.client.catalog_json();
    let profile_a = catalog["ssh"]
        .as_array()
        .expect("profiles")
        .iter()
        .find(|profile| profile["label"] == "machine-a")
        .expect("machine-a profile");
    assert_eq!(profile_a["target"], "127.0.0.1");
    assert_eq!(profile_a["user"], user.as_str());
    assert_eq!(profile_a["port"], sshd_port);
    assert_eq!(
        profile_a["identity_file"],
        serde_json::json!([client_key.to_string_lossy()])
    );
    assert_eq!(profile_a["session"], "e2ea");

    // Remote sessions started by the prepare flow are reachable directly.
    for session in ["e2ea", "e2eb"] {
        let list = rig.remote.run(None, &["session", "list", "--json"]);
        expect_code(&list, 0, "remote session list");
        assert!(
            stdout(&list).contains(session),
            "remote session {session} running: {}",
            stdout(&list)
        );
    }

    // --- Scenario 1: port forwards via catalog watcher -> reconcile -----
    // 1a. A rule saved before the client connects is built at connect time.
    let phase = "forward-initial";
    let add_rule = rig.client.run(
        None,
        &[
            "machine",
            "forward",
            "add",
            &id_a,
            "--kind",
            "local",
            "--listen-port",
            &forward_a_initial.to_string(),
            "--target-host",
            "127.0.0.1",
            "--target-port",
            &service_port.to_string(),
        ],
    );
    expect_code(&add_rule, 0, "machine forward add initial");

    rig.tui = Some(spawn_tui(&rig.client, "e2etest", &root));
    let tui_deadline = Instant::now() + Duration::from_secs(45);
    loop {
        if let Some(status) = rig
            .tui
            .as_mut()
            .and_then(|tui| tui.child.try_wait().ok().flatten())
        {
            let tail = rig
                .tui
                .as_ref()
                .map(|tui| tui.output.lock().expect("tui output").clone())
                .unwrap_or_default();
            panic!("nested TUI exited early ({status}); output tail:\n{tail}");
        }
        if rig
            .client
            .run(Some("e2etest"), &["pane", "list"])
            .status
            .success()
        {
            break;
        }
        assert!(
            Instant::now() < tui_deadline,
            "timed out waiting for the nested client session"
        );
        thread::sleep(Duration::from_millis(400));
    }
    log("setup", "nested TUI client session is up");
    wait_until(
        "initial forward serves through ssh",
        Duration::from_secs(60),
        || http_ok(forward_a_initial),
    );
    log(
        phase,
        "initial rule active: GET via forwarded port succeeded",
    );

    // 1b. Rule edits while the connection stays up: old stops, new starts.
    let phase = "forward-reconcile";
    let remove_rule = rig
        .client
        .run(None, &["machine", "forward", "remove", &id_a, "1"]);
    expect_code(&remove_rule, 0, "machine forward remove");
    let add_edited = rig.client.run(
        None,
        &[
            "machine",
            "forward",
            "add",
            &id_a,
            "--kind",
            "local",
            "--listen-port",
            &forward_a_edited.to_string(),
            "--target-host",
            "127.0.0.1",
            "--target-port",
            &service_port.to_string(),
        ],
    );
    expect_code(&add_edited, 0, "machine forward add edited");
    wait_until("removed forward refuses", Duration::from_secs(20), || {
        refused(forward_a_initial)
    });
    wait_until("edited forward serves", Duration::from_secs(30), || {
        http_ok(forward_a_edited)
    });
    let list = rig
        .client
        .run(None, &["machine", "forward", "list", &id_a, "--json"]);
    expect_code(&list, 0, "machine forward list");
    let rules: serde_json::Value = serde_json::from_str(&stdout(&list)).expect("forward list json");
    let rules = rules.as_array().expect("rules array");
    assert_eq!(rules.len(), 1, "exactly one rule after edits: {rules:?}");
    assert_eq!(rules[0]["listen_port"], forward_a_edited);
    log(
        phase,
        "edit while online: old listener stopped, new listener serves",
    );

    // 1c. The 0 -> N transition: a machine whose connection came up with no
    // (or no longer any) rules must still start a freshly added rule.
    let phase = "forward-zero-to-n";
    let remove_last = rig
        .client
        .run(None, &["machine", "forward", "remove", &id_a, "1"]);
    expect_code(&remove_last, 0, "machine forward remove last");
    wait_until("emptied rule set refuses", Duration::from_secs(20), || {
        refused(forward_a_edited)
    });
    let refill = rig.client.run(
        None,
        &[
            "machine",
            "forward",
            "add",
            &id_a,
            "--kind",
            "local",
            "--listen-port",
            &forward_a_refilled.to_string(),
            "--target-host",
            "127.0.0.1",
            "--target-port",
            &service_port.to_string(),
        ],
    );
    expect_code(&refill, 0, "machine forward add refill");
    wait_until(
        "refilled rule after empty set serves",
        Duration::from_secs(30),
        || http_ok(forward_a_refilled),
    );
    let add_b_rule = rig.client.run(
        None,
        &[
            "machine",
            "forward",
            "add",
            &id_b,
            "--kind",
            "local",
            "--listen-port",
            &forward_b_first.to_string(),
            "--target-host",
            "127.0.0.1",
            "--target-port",
            &service_port.to_string(),
        ],
    );
    expect_code(&add_b_rule, 0, "machine forward add first rule for B");
    wait_until(
        "first-ever rule of online machine serves",
        Duration::from_secs(30),
        || http_ok(forward_b_first),
    );
    log(phase, "0 -> N transitions start forwards while online");

    // 1d. Outage + recovery: kill sshd, forwards die; restart sshd, the
    // supervisor reconnects and rebuilds every rule.
    let phase = "forward-recovery";
    rig.stop_sshd();
    wait_until("forward dies with sshd", Duration::from_secs(30), || {
        refused(forward_a_refilled) && refused(forward_b_first)
    });
    log(phase, "sshd killed; forwards refused connections");
    rig.start_sshd();
    wait_until(
        "forwards rebuilt after sshd recovery",
        Duration::from_secs(120),
        || http_ok(forward_a_refilled) && http_ok(forward_b_first),
    );
    let status = rig
        .client
        .run(None, &["machine", "status", &id_a, "--json"]);
    expect_code(&status, 0, "machine status");
    let report: serde_json::Value = serde_json::from_str(&stdout(&status)).expect("status json");
    assert_eq!(
        report["connection"], "online",
        "machine-a back online after recovery: {report}"
    );
    assert_eq!(report["forwards"].as_array().map(Vec::len), Some(1));
    log(phase, "reconnect rebuilt forwards; machine status online");

    // --- Scenario 2: broadcast fan-out over real SSH bridges ------------
    let phase = "broadcast";
    let pane_a = rig.remote.first_pane_id("e2ea");
    let pane_b = rig.remote.first_pane_id("e2eb");
    let pane_local = rig.client.first_pane_id("e2etest");
    log(
        phase,
        &format!("panes: a={pane_a} b={pane_b} local={pane_local}"),
    );

    // Registration dedup: one pane per endpoint.
    let add_target_a = rig.client.run(
        None,
        &["broadcast", "add", "--machine", &id_a, "--pane", &pane_a],
    );
    expect_code(&add_target_a, 0, "broadcast add machine-a");
    let dup = rig.client.run(
        None,
        &["broadcast", "add", "--machine", &id_a, "--pane", "w9:p9"],
    );
    assert_eq!(
        dup.status.code(),
        Some(2),
        "second pane on the same endpoint must be rejected: {}",
        stderr(&dup)
    );
    let add_target_b = rig.client.run(
        None,
        &["broadcast", "add", "--machine", &id_b, "--pane", &pane_b],
    );
    expect_code(&add_target_b, 0, "broadcast add machine-b");
    let add_local = rig
        .client
        .run(None, &["broadcast", "add", "--pane", &pane_local]);
    expect_code(&add_local, 0, "broadcast add local");
    let status = rig.client.run(None, &["broadcast", "status", "--json"]);
    expect_code(&status, 0, "broadcast status");
    let set: serde_json::Value = serde_json::from_str(&stdout(&status)).expect("status json");
    assert_eq!(set["enabled"], false);
    assert_eq!(set["targets"].as_array().map(Vec::len), Some(3));

    // Safety gates: disabled set, then empty set.
    let gated = rig
        .client
        .run(Some("e2etest"), &["broadcast", "send", "echo gated"]);
    assert_eq!(
        gated.status.code(),
        Some(2),
        "disabled set must refuse to send: {}",
        stderr(&gated)
    );
    let enable = rig.client.run(None, &["broadcast", "enable"]);
    expect_code(&enable, 0, "broadcast enable");
    let clear = rig.client.run(None, &["broadcast", "clear"]);
    expect_code(&clear, 0, "broadcast clear");
    let empty = rig
        .client
        .run(Some("e2etest"), &["broadcast", "send", "echo gated"]);
    assert_eq!(
        empty.status.code(),
        Some(2),
        "empty set must refuse to send: {}",
        stderr(&empty)
    );
    for args in [
        vec!["broadcast", "add", "--machine", &id_a, "--pane", &pane_a],
        vec!["broadcast", "add", "--machine", &id_b, "--pane", &pane_b],
        vec!["broadcast", "add", "--pane", &pane_local],
    ] {
        let output = rig.client.run(None, &args);
        expect_code(&output, 0, "broadcast re-add");
    }

    // Fan-out to all three panes.
    let marker = format!("CAST-{}", std::process::id());
    let send = rig.client.run(
        Some("e2etest"),
        &["broadcast", "send", "--json", "echo", &marker],
    );
    expect_code(&send, 0, "broadcast send fan-out");
    let rows: serde_json::Value = serde_json::from_str(&stdout(&send)).expect("send rows json");
    let rows = rows.as_array().expect("rows array");
    assert_eq!(rows.len(), 3, "one outcome row per target: {rows:?}");
    assert!(
        rows.iter().all(|row| row["success"] == true),
        "all targets succeed: {rows:?}"
    );
    for (side, session, pane) in [
        (&rig.remote, "e2ea", &pane_a),
        (&rig.remote, "e2eb", &pane_b),
        (&rig.client, "e2etest", &pane_local),
    ] {
        let wait = side.pane_wait_output(session, pane, &marker, 15_000);
        expect_code(&wait, 0, &format!("marker reaches pane {pane}"));
    }
    log(phase, "fan-out reached remote panes and the local pane");

    // Per-target failure reporting: one broken pane fails alone.
    let remove_b = rig.client.run(None, &["broadcast", "remove", "2"]);
    expect_code(&remove_b, 0, "broadcast remove machine-b target");
    let bad = rig.client.run(
        None,
        &["broadcast", "add", "--machine", &id_b, "--pane", "w9:p9"],
    );
    expect_code(&bad, 0, "broadcast add broken pane");
    let send = rig.client.run(
        Some("e2etest"),
        &[
            "broadcast",
            "send",
            "--json",
            "echo",
            &format!("{marker}-2"),
        ],
    );
    assert_eq!(
        send.status.code(),
        Some(1),
        "one failing target flips the exit code: {}",
        stderr(&send)
    );
    let rows: serde_json::Value = serde_json::from_str(&stdout(&send)).expect("send rows json");
    let rows = rows.as_array().expect("rows array");
    assert_eq!(rows.len(), 3);
    let failures: Vec<_> = rows.iter().filter(|row| row["success"] == false).collect();
    assert_eq!(failures.len(), 1, "exactly one failing target: {rows:?}");
    assert_eq!(failures[0]["pane_id"], "w9:p9");
    assert!(
        failures[0]["error"].as_str().is_some_and(|e| !e.is_empty()),
        "failure row carries the error: {rows:?}"
    );
    let remove_bad = rig.client.run(None, &["broadcast", "remove", "3"]);
    expect_code(&remove_bad, 0, "broadcast remove broken target");
    let add_b_back = rig.client.run(
        None,
        &["broadcast", "add", "--machine", &id_b, "--pane", &pane_b],
    );
    expect_code(&add_b_back, 0, "broadcast restore machine-b target");
    log(
        phase,
        "failing target reported once; healthy targets still succeed",
    );

    // --- Scenario 3: session logs of a real remote pane -----------------
    let phase = "session-log";
    let logs_dir = root.join("logs");
    let template = format!("{}/{{machine}}-{{pane}}.log", logs_dir.display());
    let log_file = logs_dir.join(format!("machine-a-{}.log", pane_a.replace(':', "-")));
    let rotated = log_file.with_file_name(format!(
        "{}.1",
        log_file.file_name().expect("log name").to_string_lossy()
    ));

    // Produce the markers first: the logging worker's first snapshot then
    // deterministically contains them (its change detection keys on the
    // pane-read revision, which tracks the pane content sequence).
    let run_marker = rig
        .remote
        .run(Some("e2ea"), &["pane", "run", &pane_a, "echo LOGMARK-1"]);
    expect_code(&run_marker, 0, "produce remote output");
    for index in 2..=10 {
        let run = rig.remote.run(
            Some("e2ea"),
            &[
                "pane",
                "run",
                &pane_a,
                &format!("echo LOGMARK-{index}-{}", "X".repeat(280)),
            ],
        );
        expect_code(&run, 0, "produce more remote output");
        thread::sleep(Duration::from_millis(300));
    }

    let log_on = rig.client.run(
        None,
        &[
            "machine",
            "log",
            &id_a,
            "on",
            "--path",
            &template,
            "--max-bytes",
            "4096",
            "--interval",
            "5",
        ],
    );
    expect_code(&log_on, 0, "machine log on");
    let show = rig
        .client
        .run(None, &["machine", "log", &id_a, "show", "--json"]);
    expect_code(&show, 0, "machine log show");
    let shown: serde_json::Value = serde_json::from_str(&stdout(&show)).expect("show json");
    assert_eq!(shown["enabled"], true);
    assert_eq!(shown["max_bytes"], 4096);
    assert_eq!(shown["dump_interval_secs"], 5);
    assert_eq!(shown["path_template"], template.as_str());

    wait_until(
        "interval dump lands in the rendered log path",
        Duration::from_secs(30),
        || {
            fs::read_to_string(&log_file)
                .map(|content| content.contains("LOGMARK-1"))
                .unwrap_or(false)
        },
    );
    let content = fs::read_to_string(&log_file).expect("log file");
    assert!(
        content.contains("herdr session snapshot"),
        "snapshot header present: {}",
        &content[..content.len().min(400)]
    );
    log(phase, "interval dump contains remote pane output");

    // One-shot synchronous dump via the CLI appends the current pane state.
    let run_oneshot = rig.remote.run(
        Some("e2ea"),
        &["pane", "run", &pane_a, "echo LOGMARK-ONESHOT"],
    );
    expect_code(&run_oneshot, 0, "produce one-shot marker");
    thread::sleep(Duration::from_millis(500));
    let dump = rig.client.run(None, &["machine", "log", &id_a, "dump"]);
    expect_code(&dump, 0, "machine log dump");
    wait_until(
        "one-shot dump contains the marker",
        Duration::from_secs(10),
        || {
            fs::read_to_string(&log_file)
                .map(|content| content.contains("LOGMARK-ONESHOT"))
                .unwrap_or(false)
        },
    );
    log(phase, "machine log dump appended a snapshot synchronously");

    // Rotation at max-bytes: every one-shot dump appends a full snapshot, so
    // a few dumps push the file past the cap and produce `<file>.1`.
    let mut rotated_ok = false;
    for _ in 0..6 {
        let dump = rig.client.run(None, &["machine", "log", &id_a, "dump"]);
        expect_code(&dump, 0, "machine log dump for rotation");
        if fs::metadata(&rotated)
            .map(|meta| meta.len() > 0)
            .unwrap_or(false)
        {
            rotated_ok = true;
            break;
        }
        thread::sleep(Duration::from_millis(400));
    }
    assert!(rotated_ok, "size rotation must produce {rotated:?}");
    assert!(
        fs::metadata(&log_file)
            .map(|meta| meta.len() > 0)
            .unwrap_or(false),
        "current log file keeps the fresh snapshots"
    );
    log(phase, "rotation produced <log>.1");

    // Interval dumps keep capturing subsequent output: the pane-read API
    // reports the real content sequence as `revision`, so the worker's
    // per-pane change detection fires when new output arrives.
    let run_followup = rig.remote.run(
        Some("e2ea"),
        &["pane", "run", &pane_a, "echo LOGMARK-FOLLOWUP"],
    );
    expect_code(&run_followup, 0, "produce follow-up marker");
    wait_until(
        "interval dump captures output produced after the first cycle",
        Duration::from_secs(30),
        || {
            fs::read_to_string(&log_file)
                .map(|content| content.contains("LOGMARK-FOLLOWUP"))
                .unwrap_or(false)
        },
    );
    log(phase, "interval dump keeps capturing subsequent output");

    // --- Broadcast against unreachable machines (final, before teardown) -
    let phase = "broadcast-outage";
    rig.stop_sshd();
    let send = rig.client.run(
        Some("e2etest"),
        &[
            "broadcast",
            "send",
            "--json",
            "echo",
            &format!("{marker}-3"),
        ],
    );
    assert_eq!(
        send.status.code(),
        Some(1),
        "unreachable machines fail the send: {}",
        stderr(&send)
    );
    let rows: serde_json::Value = serde_json::from_str(&stdout(&send)).expect("send rows json");
    let rows = rows.as_array().expect("rows array");
    assert_eq!(rows.len(), 3);
    let failed_machines: Vec<_> = rows
        .iter()
        .filter(|row| row["success"] == false)
        .map(|row| row["machine"].as_str().expect("machine name").to_owned())
        .collect();
    assert_eq!(
        failed_machines.len(),
        2,
        "one failure row per unreachable machine (deduplicated): {rows:?}"
    );
    let mut sorted = failed_machines.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), 2, "no duplicate machine failures: {rows:?}");
    let local_row = rows
        .iter()
        .find(|row| row["machine"] == "local")
        .expect("local row");
    assert_eq!(
        local_row["success"], true,
        "local delivery unaffected by ssh outage: {rows:?}"
    );
    log(
        phase,
        "ssh outage: per-machine failures, local target unaffected",
    );

    log("done", "all scenarios passed");
}

#[test]
fn ssh_config_import_wildcards_multihop_and_multi_identity() {
    let phase = "import";
    let root = std::env::temp_dir().join(format!("herdr-ssh-import-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let client = Side::new(&root, "client", None);

    let config_path = root.join("ssh_config");
    write_file(
        &config_path,
        "Host *.internal\n\
         \x20   User ops\n\
         \n\
         Host jump\n\
         \x20   HostName jump.internal\n\
         \x20   User jumper\n\
         \x20   Port 2201\n\
         \x20   IdentityFile ~/.ssh/jump_ed25519\n\
         \n\
         Host app\n\
         \x20   HostName app.internal\n\
         \x20   User deploy\n\
         \x20   Port 2202\n\
         \x20   IdentityFile ~/.ssh/app_primary\n\
         \x20   IdentityFile ~/.ssh/app_fallback\n\
         \x20   IdentitiesOnly yes\n\
         \x20   ProxyJump jump,10.0.0.1\n\
         \n\
         Host db\n\
         \x20   HostName db.internal\n\
         \x20   ProxyJump app\n",
    );

    let output = client.run(
        None,
        &[
            "machine",
            "import",
            "--yes",
            "--file",
            &config_path.to_string_lossy(),
        ],
    );
    expect_code(&output, 0, "machine import");
    let out = stdout(&output);
    assert!(out.contains("imported jump"), "{out}");
    assert!(out.contains("imported app"), "{out}");
    assert!(out.contains("imported db"), "{out}");
    assert!(
        out.contains("skipped *.internal") && out.contains("wildcard host pattern"),
        "{out}"
    );

    let catalog = client.catalog_json();
    let profiles = catalog["ssh"].as_array().expect("profiles array");
    assert_eq!(profiles.len(), 3, "{profiles:?}");
    // Dependency order: jump hosts are stored before their dependents.
    assert_eq!(profiles[0]["label"], "jump");
    assert_eq!(profiles[1]["label"], "app");
    assert_eq!(profiles[2]["label"], "db");
    let jump_id = profiles[0]["id"].as_str().expect("jump id");
    let app_id = profiles[1]["id"].as_str().expect("app id");

    let jump = &profiles[0];
    assert_eq!(jump["target"], "jump.internal");
    assert_eq!(jump["user"], "jumper");
    assert_eq!(jump["port"], 2201);
    assert_eq!(
        jump["identity_file"],
        serde_json::json!(["~/.ssh/jump_ed25519"])
    );

    let app = &profiles[1];
    assert_eq!(app["target"], "app.internal");
    assert_eq!(app["user"], "deploy");
    assert_eq!(app["port"], 2202);
    // Multiple IdentityFile directives append in order.
    assert_eq!(
        app["identity_file"],
        serde_json::json!(["~/.ssh/app_primary", "~/.ssh/app_fallback"])
    );
    assert_eq!(app["identities_only"], true);
    // Multi-hop ProxyJump: same-batch alias resolves to a profile
    // reference; anything else stays a literal target.
    assert_eq!(
        app["proxy_jump"],
        serde_json::json!([{ "profile": jump_id }, { "target": "10.0.0.1" }]),
        "mixed hops: {app}"
    );
    // A chained reference resolves transitively through the batch.
    assert_eq!(
        profiles[2]["proxy_jump"],
        serde_json::json!([{ "profile": app_id }]),
        "db jumps via the app profile"
    );
    log(
        phase,
        "wildcards skipped; multi-hop and multi-identity mapped",
    );

    // A second import is idempotent.
    let output = client.run(
        None,
        &[
            "machine",
            "import",
            "--yes",
            "--file",
            &config_path.to_string_lossy(),
        ],
    );
    expect_code(&output, 0, "machine import idempotent");
    let out = stdout(&output);
    assert!(out.contains("imported 0 machine(s)"), "{out}");
    assert!(
        client.catalog_json()["ssh"].as_array().map(Vec::len) == Some(3),
        "no duplicates after re-import"
    );
    log(phase, "re-import skipped everything");

    kill_processes_referencing(&root);
    let _ = fs::remove_dir_all(&root);
}
