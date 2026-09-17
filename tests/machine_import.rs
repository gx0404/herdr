#![cfg(unix)]

//! End-to-end smoke tests for `machine import` and `machine add --from-config`
//! using a temporary HOME with a fixture SSH config. The `add --from-config`
//! happy path drives a fake `ssh` binary; no real network or server is used.

use std::fs;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::time::Duration;

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

const CONFIG: &str = "\
Host web
    HostName web.internal
    User deploy
    Port 2222
    IdentityFile ~/.ssh/web
    IdentitiesOnly yes
    ProxyJump bastion
    ServerAliveInterval 45

Host bastion
    HostName bastion.internal

Host *.internal
    User ops
";

fn test_root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "herdr-machine-import-{}-{name}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join(".ssh")).unwrap();
    root
}

fn app_dir_name() -> &'static str {
    if cfg!(debug_assertions) {
        "herdr-dev"
    } else {
        "herdr"
    }
}

fn write_config(root: &Path, content: &str) {
    fs::write(root.join(".ssh").join("config"), content).unwrap();
}

fn base_command(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_herdr"));
    command.env("HOME", root);
    command.env("XDG_CONFIG_HOME", root.join("config"));
    command.env("XDG_STATE_HOME", root.join("state"));
    command.env("XDG_RUNTIME_DIR", root);
    command.env("HERDR_LANG", "en");
    for name in [
        "HERDR_ENV",
        "HERDR_SESSION",
        "HERDR_SOCKET_PATH",
        "HERDR_CLIENT_SOCKET_PATH",
        "HERDR_REMOTE_BINARY",
        "HERDR_CONFIG_PATH",
    ] {
        command.env_remove(name);
    }
    command
}

fn run(root: &Path, args: &[&str]) -> std::process::Output {
    let mut command = base_command(root);
    command.args(args);
    command.output().unwrap()
}

fn catalog_json(root: &Path) -> serde_json::Value {
    let path = root
        .join("state")
        .join(app_dir_name())
        .join("client")
        .join("endpoints.json");
    let content = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("catalog {} unreadable: {error}", path.display()));
    serde_json::from_str(&content).unwrap()
}

fn saved_profiles(root: &Path) -> Vec<serde_json::Value> {
    catalog_json(root)["ssh"].as_array().unwrap().clone()
}

#[test]
fn import_yes_imports_hosts_and_resolves_jump_references() {
    let root = test_root("happy");
    write_config(&root, CONFIG);

    let output = run(&root, &["machine", "import", "--yes", "--group", "prod"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("imported web"), "{stdout}");
    assert!(stdout.contains("imported bastion"), "{stdout}");
    assert!(
        stdout.contains("skipped *.internal") && stdout.contains("wildcard host pattern"),
        "{stdout}"
    );

    let profiles = saved_profiles(&root);
    assert_eq!(profiles.len(), 2, "{profiles:?}");
    // The jump host is stored before its dependents.
    assert_eq!(profiles[0]["label"], "bastion");
    assert_eq!(profiles[1]["label"], "web");
    let bastion_id = profiles[0]["id"].as_str().unwrap();
    let web = &profiles[1];
    assert_eq!(web["target"], "web.internal");
    assert_eq!(web["user"], "deploy");
    assert_eq!(web["port"], 2222);
    assert_eq!(web["identity_file"], serde_json::json!(["~/.ssh/web"]));
    assert_eq!(web["identities_only"], true);
    assert_eq!(web["server_alive_interval"], 45);
    assert_eq!(web["group"], "prod");
    assert_eq!(
        web["proxy_jump"],
        serde_json::json!([{ "profile": bastion_id }]),
        "same-batch ProxyJump resolves to a profile reference"
    );

    // A second import is idempotent: everything is skipped and reported.
    let output = run(&root, &["machine", "import", "--yes"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(
        stdout.contains("a saved machine already uses this label"),
        "{stdout}"
    );
    assert!(stdout.contains("imported 0 machine(s)"), "{stdout}");
    let saved = saved_profiles(&root);
    assert_eq!(saved.len(), 2, "no duplicates were added");

    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn import_without_yes_requires_a_terminal() {
    let root = test_root("non-tty");
    write_config(&root, CONFIG);

    let output = run(&root, &["machine", "import"]);
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("stdin is not a terminal") && stderr.contains("--yes"),
        "{stderr}"
    );
    assert!(!root
        .join("state")
        .join(app_dir_name())
        .join("client")
        .join("endpoints.json")
        .exists());

    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn import_host_filter_limits_the_batch_and_keeps_hops_literal() {
    let root = test_root("host-filter");
    write_config(&root, CONFIG);

    let output = run(&root, &["machine", "import", "--yes", "--host", "web"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    let profiles = saved_profiles(&root);
    assert_eq!(profiles.len(), 1, "{profiles:?}");
    assert_eq!(profiles[0]["label"], "web");
    assert_eq!(
        profiles[0]["proxy_jump"],
        serde_json::json!([{ "target": "bastion" }]),
        "a hop outside the filtered batch stays a literal target"
    );

    let output = run(&root, &["machine", "import", "--yes", "--host", "absent"]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("no hosts"), "{stderr}");

    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn import_missing_file_and_missing_config_report_cleanly() {
    let root = test_root("missing");
    let output = run(&root, &["machine", "import", "--yes"]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("no SSH config found at"), "{stderr}");

    let output = run(
        &root,
        &[
            "machine",
            "import",
            "--yes",
            "--file",
            &root.join("absent").to_string_lossy(),
        ],
    );
    assert_eq!(output.status.code(), Some(1));
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn import_interactive_selection_picks_numbered_hosts() {
    let root = test_root("interactive");
    write_config(&root, CONFIG);

    let pair = native_pty_system().openpty(PtySize::default()).unwrap();
    let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_herdr"));
    command.args(["machine", "import"]);
    command.env("HOME", &root);
    command.env("XDG_CONFIG_HOME", root.join("config"));
    command.env("XDG_STATE_HOME", root.join("state"));
    command.env("XDG_RUNTIME_DIR", &root);
    command.env("HERDR_LANG", "en");
    let mut child = pair.slave.spawn_command(command).unwrap();
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().unwrap();
    let mut writer = pair.master.take_writer().unwrap();
    let (tx, rx) = mpsc::channel();
    let reading = std::thread::spawn(move || {
        let mut buffer = [0; 4096];
        while let Ok(len) = reader.read(&mut buffer) {
            if len == 0 || tx.send(buffer[..len].to_vec()).is_err() {
                break;
            }
        }
    });
    let mut output = String::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let mut answered = false;
    while let Ok(bytes) = rx.recv_timeout(Duration::from_secs(20)) {
        output.push_str(&String::from_utf8_lossy(&bytes));
        if output.contains("import which hosts?") {
            writer.write_all(b"2\n").unwrap();
            writer.flush().unwrap();
            answered = true;
            break;
        }
        assert!(std::time::Instant::now() < deadline, "timed out: {output}");
    }
    assert!(answered, "selection prompt never appeared: {output}");
    while let Ok(bytes) = rx.recv_timeout(Duration::from_secs(20)) {
        output.push_str(&String::from_utf8_lossy(&bytes));
    }
    let status = child.wait().unwrap();
    drop(writer);
    drop(pair.master);
    reading.join().unwrap();

    assert!(status.success(), "{output}");
    assert!(output.contains("found 2 host(s)"), "{output}");
    let profiles = saved_profiles(&root);
    assert_eq!(profiles.len(), 1, "{profiles:?}");
    // "2" selects the second listed host; the jump host sorts first.
    assert_eq!(profiles[0]["label"], "web", "{output}");
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn add_from_config_rejects_unknown_hosts_and_conflicts() {
    let root = test_root("from-config-errors");
    write_config(&root, CONFIG);

    let output = run(&root, &["machine", "add", "--from-config", "ghost"]);
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("host 'ghost' not found"), "{stderr}");

    let output = run(&root, &["machine", "add", "--from-config", "*.internal"]);
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("wildcards"), "{stderr}");

    let output = run(
        &root,
        &["machine", "add", "--from-config", "web", "--user", "dev"],
    );
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("cannot be combined"), "{stderr}");

    fs::remove_dir_all(&root).unwrap();
}

// Fake ssh: enough of the prepare flow to prove `machine add --from-config`
// resolves the host and renders its directives into the managed SSH config
// (captured to $FAKE_ROOT/used-config). The flow necessarily stops at the
// endpoint handshake probe, which needs a real remote server; the repo's
// existing `machine add` coverage (tests/machine_setup.rs) likewise stops
// before a successful save, so the profile must remain unsaved here.
const FAKE_SSH: &str = r#"#!/bin/sh
echo "$*" >>"$FAKE_ROOT/ssh-log"
for arg do
    if [ "$arg" = '-F' ]; then take_config=yes; continue; fi
    if [ "$take_config" = yes ]; then cp "$arg" "$FAKE_ROOT/used-config"; take_config=no; fi
    last=$arg
done
if [ "$last" = 'command -v herdr' ]; then
    echo /home/remote/.local/bin/herdr
    exit 0
fi
case "$last" in
    'tee '*) cat >/dev/null; exit 0 ;;
esac
case "$last" in
    *'herdr-remote-output-ready:1'*) script=$last ;;
    *) script=$(cat) ;;
esac
printf '\n%s\n' 'herdr-remote-output-ready:1'
case "$script" in
    *'uname -s'*) uname -s; uname -m ;;
    *'version='*) echo /home/remote/.local/bin/herdr ;;
    *'status client --json'*) printf '%s\n' "$FAKE_CLIENT_STATUS" ;;
    *'status server --json'*)
        echo '{"running":true,"version":"0.8.2","capabilities":{"live_handoff":true,"detached_server_daemon":true,"endpoint_protocol_generation":1,"surface_interest":true,"health_check":true}}' ;;
    *'remote-client-bridge'*) echo bridge >>"$FAKE_ROOT/actions" ;;
    *'mkdir -p'*) printf '/fake/tmp\000/fake/herdr\000' ;;
    *'chmod 755'*) echo install >>"$FAKE_ROOT/actions" ;;
    *'command -v herdr'*) echo /home/remote/.local/bin/herdr ;;
    *) echo "unexpected fake ssh script: $script" >&2; exit 1 ;;
esac
"#;

#[test]
fn add_from_config_registers_the_configured_directives() {
    let root = test_root("from-config-happy");
    write_config(
        &root,
        "\
Host web
    HostName web.internal
    User deploy
    Port 2222
    IdentityFile ~/.ssh/web
    IdentitiesOnly yes
    ProxyJump bastion.example
    ForwardAgent yes
    ServerAliveInterval 45
    ControlPersist 10m
",
    );
    fs::create_dir_all(root.join("bin")).unwrap();
    fs::create_dir_all(root.join("config").join(app_dir_name())).unwrap();
    fs::write(root.join("bin").join("ssh"), FAKE_SSH).unwrap();
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(
            root.join("bin").join("ssh"),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
    }
    fs::write(
        root.join("config").join(app_dir_name()).join("config.toml"),
        "onboarding = false\n[remote]\nmanage_ssh_config = false\n",
    )
    .unwrap();
    let status = Command::new(env!("CARGO_BIN_EXE_herdr"))
        .args(["status", "client", "--json"])
        .output()
        .unwrap();
    assert!(status.status.success());

    let mut command = base_command(&root);
    command.args(["machine", "add", "--from-config", "web"]);
    command.env(
        "PATH",
        format!("{}:/usr/bin:/bin", root.join("bin").display()),
    );
    command.env("FAKE_ROOT", &root);
    command.env(
        "FAKE_CLIENT_STATUS",
        String::from_utf8(status.stdout).unwrap(),
    );
    let output = command.output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Setup runs against the resolved HostName, not the alias.
    let ssh_log = fs::read_to_string(root.join("ssh-log")).unwrap();
    assert!(ssh_log.contains(" web.internal "), "{ssh_log}");
    assert!(!ssh_log.contains(" -T web "), "{ssh_log}");

    // The profile's connection fields reach the managed SSH config used by
    // the setup probes.
    let used_config = fs::read_to_string(root.join("used-config")).unwrap();
    for directive in [
        "Port 2222",
        "User deploy",
        "IdentityFile \"~/.ssh/web\"",
        "IdentitiesOnly yes",
        "ProxyJump \"bastion.example\"",
        "ForwardAgent yes",
        "ServerAliveInterval 45",
        "ControlPersist 10m",
    ] {
        assert!(
            used_config.contains(directive),
            "missing `{directive}` in:\n{used_config}"
        );
    }

    // The endpoint handshake probe needs a real remote server, so the fake
    // flow ends here: prepare fails and nothing is saved.
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("machine was not saved"), "{stderr}");
    let catalog = root
        .join("state")
        .join(app_dir_name())
        .join("client")
        .join("endpoints.json");
    let saved = fs::read_to_string(&catalog).unwrap_or_default();
    assert!(
        !saved.contains("web.internal"),
        "failed setup must not save the machine: {saved}"
    );
    fs::remove_dir_all(&root).unwrap();
}
