#![cfg(target_os = "linux")]

use std::io::{BufRead, Read, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const ROOT_PREFIX: &str = "hhs-";
const OWNER_MARKER: &str = ".owner";
const ROOT_ENV: &str = "HERDR_HOST_SHUTDOWN_ROOT";
const REAPER_OWNER_ENV: &str = "HERDR_HOST_SHUTDOWN_REAPER_OWNER";
const REAPER_ROOT_ENV: &str = "HERDR_HOST_SHUTDOWN_REAPER_ROOT";
const REAPER_DRIVER_ROOT_ENV: &str = "HERDR_HOST_SHUTDOWN_REAPER_DRIVER_ROOT";
const HOST_SHUTDOWN_TEST_NAME: &str = "host_shutdown_saves_layout_before_releasing_delay_lock";

/// 被测二进制使用的应用目录名，规则同 `tests/support/mod.rs::app_dir_name`（本文件不引入
/// support）：debug 构建是 `herdr-dev`，release 构建是 `herdr`。
fn app_dir_name() -> &'static str {
    if cfg!(debug_assertions) {
        "herdr-dev"
    } else {
        "herdr"
    }
}

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct LoginManager {
    peer: Arc<Mutex<Option<UnixStream>>>,
}

#[zbus::interface(name = "org.freedesktop.login1.Manager")]
impl LoginManager {
    fn inhibit(
        &self,
        what: &str,
        _who: &str,
        _why: &str,
        mode: &str,
    ) -> zbus::fdo::Result<zbus::zvariant::OwnedFd> {
        assert_eq!((what, mode), ("shutdown", "delay"));
        let (lock, peer) = UnixStream::pair().unwrap();
        peer.set_nonblocking(true).unwrap();
        *self.peer.lock().unwrap() = Some(peer);
        Ok(std::os::fd::OwnedFd::from(lock).into())
    }

    #[zbus(property)]
    fn preparing_for_shutdown(&self) -> bool {
        false
    }
}

fn private_bus(address: &str, root: &Path) -> ChildGuard {
    let mut child = ChildGuard(
        Command::new("dbus-daemon")
            .args([
                "--session",
                "--nofork",
                "--print-address=1",
                "--address",
                address,
            ])
            .env(ROOT_ENV, root)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut ready = String::new();
    std::io::BufReader::new(child.0.stdout.take().unwrap())
        .read_line(&mut ready)
        .unwrap();
    child
}

async fn login_service(address: &str, peer: Arc<Mutex<Option<UnixStream>>>) -> zbus::Connection {
    zbus::connection::Builder::address(address)
        .unwrap()
        .name("org.freedesktop.login1")
        .unwrap()
        .serve_at("/org/freedesktop/login1", LoginManager { peer })
        .unwrap()
        .build()
        .await
        .unwrap()
}

async fn wait_for_inhibitor(peer: &Mutex<Option<UnixStream>>) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while peer.lock().unwrap().is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

fn api(socket: &Path, method: &str, params: serde_json::Value) -> serde_json::Value {
    let mut stream = UnixStream::connect(socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    writeln!(
        stream,
        "{}",
        serde_json::json!({"id":"test", "method":method, "params":params})
    )
    .unwrap();
    let mut line = String::new();
    std::io::BufReader::new(stream)
        .read_line(&mut line)
        .unwrap();
    let response: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert!(response.get("error").is_none(), "{response}");
    response
}

fn is_host_shutdown_root(root: &Path) -> bool {
    root.parent() == Some(Path::new("/var/tmp"))
        && root
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with(ROOT_PREFIX))
        && root.join(OWNER_MARKER).is_file()
}

fn path_is_under(path: &[u8], root: &[u8]) -> bool {
    path.strip_prefix(root)
        .is_some_and(|rest| rest.first().is_none_or(|byte| *byte == b'/'))
}

fn process_belongs_to_root(proc_dir: &Path, root: &[u8]) -> bool {
    std::fs::read(proc_dir.join("environ")).is_ok_and(|environ| {
        environ.split(|byte| *byte == 0).any(|var| {
            var.strip_prefix(ROOT_ENV.as_bytes()).is_some_and(|value| {
                value.first().is_some_and(|byte| *byte == b'=') && path_is_under(&value[1..], root)
            })
        })
    })
}

fn kill_sandbox_processes(root: &Path, spare: &[u32]) -> usize {
    let needle = root.to_string_lossy();
    let needle = needle.as_bytes();
    let self_pid = std::process::id();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return 0;
    };
    let mut killed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Ok(pid) = name.to_string_lossy().parse::<u32>() else {
            continue;
        };
        if pid == self_pid || spare.contains(&pid) {
            continue;
        }
        if process_belongs_to_root(&entry.path(), needle) {
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
            killed += 1;
        }
    }
    killed
}

fn reap_root(root: &Path) {
    if !is_host_shutdown_root(root) {
        return;
    }
    for _ in 0..5 {
        if kill_sandbox_processes(root, &[]) == 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = std::fs::remove_dir_all(root);
}

fn owner_is_live(pid: u32) -> bool {
    let alive = unsafe { libc::kill(pid as libc::pid_t, 0) } == 0
        || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH);
    if !alive {
        return false;
    }
    let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
        return true;
    };
    cmdline
        .windows(b"host_shutdown".len())
        .any(|window| window == b"host_shutdown")
}

fn sweep_stale_roots() {
    let Ok(entries) = std::fs::read_dir("/var/tmp") else {
        return;
    };
    for entry in entries.flatten() {
        let root = entry.path();
        if !is_host_shutdown_root(&root) {
            continue;
        }
        let Some(owner) = std::fs::read_to_string(root.join(OWNER_MARKER))
            .ok()
            .and_then(|pid| pid.trim().parse::<u32>().ok())
        else {
            continue;
        };
        if owner == std::process::id() || owner_is_live(owner) {
            continue;
        }
        reap_root(&root);
    }
}

fn spawn_orphan_reaper(root: &Path) -> Child {
    let mut command = Command::new(std::env::current_exe().expect("test binary path"));
    command
        .args([
            "--exact",
            HOST_SHUTDOWN_TEST_NAME,
            "--ignored",
            "--nocapture",
        ])
        .env(REAPER_OWNER_ENV, std::process::id().to_string())
        .env(REAPER_ROOT_ENV, root)
        .env_remove(ROOT_ENV)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.spawn().expect("spawn orphan reaper")
}

fn run_as_orphan_reaper() -> bool {
    let (Some(owner), Some(root)) = (
        std::env::var(REAPER_OWNER_ENV)
            .ok()
            .and_then(|pid| pid.parse::<libc::pid_t>().ok()),
        std::env::var_os(REAPER_ROOT_ENV).map(PathBuf::from),
    ) else {
        return false;
    };
    if !is_host_shutdown_root(&root) {
        return false;
    }
    while unsafe { libc::getppid() } == owner {
        std::thread::sleep(Duration::from_millis(100));
    }
    reap_root(&root);
    true
}

#[test]
fn orphan_reaper_reclaims_sigkilled_driver() {
    let root = PathBuf::from(format!(
        "/var/tmp/{ROOT_PREFIX}{}-driver-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let mut driver = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "orphan_reaper_sigkill_driver", "--nocapture"])
        .env(REAPER_DRIVER_ROOT_ENV, &root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let status = driver.wait().unwrap();
    assert!(!status.success(), "the reaper driver must be SIGKILLed");

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while root.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !root.exists(),
        "orphan reaper left sandbox {}",
        root.display()
    );
}

#[test]
#[allow(
    clippy::zombie_processes,
    reason = "The owner deliberately exits via SIGKILL with live children to test external reaper cleanup"
)]
fn orphan_reaper_sigkill_driver() {
    let Some(root) = std::env::var_os(REAPER_DRIVER_ROOT_ENV).map(PathBuf::from) else {
        return;
    };
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join(OWNER_MARKER), std::process::id().to_string()).unwrap();
    let _child = Command::new("sh")
        .args(["-c", "sleep 60"])
        .env(ROOT_ENV, &root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _reaper = spawn_orphan_reaper(&root);
    unsafe { libc::kill(std::process::id() as libc::pid_t, libc::SIGKILL) };
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires dbus-daemon; exercises a private bus and disposable named server, not host shutdown"]
async fn host_shutdown_saves_layout_before_releasing_delay_lock() {
    if run_as_orphan_reaper() {
        return;
    }
    sweep_stale_roots();
    let base = std::path::PathBuf::from(format!(
        "/var/tmp/{ROOT_PREFIX}{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&base).unwrap();
    std::fs::write(base.join(OWNER_MARKER), std::process::id().to_string()).unwrap();
    let mut reaper = spawn_orphan_reaper(&base);
    let address = format!("unix:path={}", base.join("bus").display());
    let mut bus = private_bus(&address, &base);
    let peer = Arc::new(Mutex::new(None));
    let mut service = login_service(&address, peer.clone()).await;
    let session_dir = base.join(app_dir_name()).join("sessions/shutdown");
    let socket = session_dir.join("herdr.sock");
    let config = base.join("config.toml");
    std::fs::write(&config, "onboarding = false\n[experimental]\nallow_nested = true\n[terminal]\ndefault_shell = \"/bin/sh\"\n").unwrap();
    // server 会在三个工作区里起 shell：HOME 也放进沙箱，shell 的 rc 文件与历史、按 HOME
    // 找数据的活动树适配器都落在沙箱里，碰不到开发机真实的主目录。
    let home = base.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let mut server = ChildGuard(
        Command::new(env!("CARGO_BIN_EXE_herdr"))
            .args(["--session", "shutdown", "server"])
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", &base)
            .env("XDG_STATE_HOME", &base)
            .env("XDG_RUNTIME_DIR", &base)
            .env(ROOT_ENV, &base)
            .env("HERDR_CONFIG_PATH", &config)
            .env_remove("HERDR_SOCKET_PATH")
            .env("DBUS_SYSTEM_BUS_ADDRESS", address.trim())
            .env_remove("HERDR_CLIENT_SOCKET_PATH")
            .env_remove("HERDR_SESSION")
            .env_remove("HERDR_WORKSPACE_ID")
            .env_remove("HERDR_TAB_ID")
            .env_remove("HERDR_PANE_ID")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    tokio::time::timeout(Duration::from_secs(10), async {
        while !socket.exists() || peer.lock().unwrap().is_none() {
            assert!(
                server.0.try_wait().unwrap().is_none(),
                "server exited during startup"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    // Neither a login1 owner change nor a bus restart may leave a stale inhibitor.
    peer.lock().unwrap().take();
    service
        .release_name("org.freedesktop.login1")
        .await
        .unwrap();
    drop(service);
    service = login_service(&address, peer.clone()).await;
    wait_for_inhibitor(&peer).await;
    peer.lock().unwrap().take();
    drop(service);
    drop(bus);
    if base.join("bus").exists() {
        std::fs::remove_file(base.join("bus")).unwrap();
    }
    bus = private_bus(&address, &base);
    service = login_service(&address, peer.clone()).await;
    wait_for_inhibitor(&peer).await;

    for label in ["one", "two", "three"] {
        api(
            &socket,
            "workspace.create",
            serde_json::json!({"cwd":base,"label":label,"focus":true}),
        );
    }
    service
        .emit_signal(
            None::<&str>,
            "/org/freedesktop/login1",
            "org.freedesktop.login1.Manager",
            "PrepareForShutdown",
            &true,
        )
        .await
        .unwrap();
    let mut peer = peer.lock().unwrap().take().unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match peer.read(&mut [0]) {
                Ok(0) => break,
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {}
                other => panic!("unexpected inhibitor state: {other:?}"),
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let saved = session_dir.join("session.json");
    let layout: serde_json::Value = serde_json::from_slice(&std::fs::read(saved).unwrap()).unwrap();
    assert_eq!(layout["workspaces"].as_array().unwrap().len(), 3);
    assert_eq!(layout["workspaces"][2]["custom_name"], "three");
    tokio::time::timeout(Duration::from_secs(5), async {
        while server.0.try_wait().unwrap().is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    drop(server);
    drop(service);
    drop(bus);
    reap_root(&base);
    let _ = reaper.kill();
    let _ = reaper.wait();
    let _ = std::fs::remove_dir_all(base);
}
