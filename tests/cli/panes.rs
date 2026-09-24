use super::harness::*;

#[test]
fn pane_close_only_removes_the_target_tab_when_other_tabs_exist() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("herdr.sock");

    let herdr = spawn_herdr(&config_home, &runtime_dir, &socket_path);
    wait_for_socket(&socket_path, Duration::from_secs(5));

    let created = run_cli(
        &socket_path,
        &["workspace", "create", "--cwd", base.to_str().unwrap()],
    );
    assert!(created.status.success());
    let created_json: serde_json::Value = serde_json::from_slice(&created.stdout).unwrap();
    let workspace_id = created_json["result"]["workspace"]["workspace_id"]
        .as_str()
        .unwrap()
        .to_string();

    let created_tab = run_cli(
        &socket_path,
        &["tab", "create", "--workspace", &workspace_id],
    );
    assert!(created_tab.status.success());
    let created_tab_json: serde_json::Value = serde_json::from_slice(&created_tab.stdout).unwrap();
    let second_root_pane_id = created_tab_json["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();

    let closed = run_cli(&socket_path, &["pane", "close", &second_root_pane_id]);
    assert!(closed.status.success());
    let closed_json: serde_json::Value = serde_json::from_slice(&closed.stdout).unwrap();
    assert_eq!(closed_json["result"]["type"], "ok");

    let workspaces = run_cli(&socket_path, &["workspace", "list"]);
    assert!(workspaces.status.success());
    let workspaces_json: serde_json::Value = serde_json::from_slice(&workspaces.stdout).unwrap();
    assert_eq!(
        workspaces_json["result"]["workspaces"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        workspaces_json["result"]["workspaces"][0]["workspace_id"],
        workspace_id
    );

    let tabs = run_cli(&socket_path, &["tab", "list", "--workspace", &workspace_id]);
    assert!(tabs.status.success());
    let tabs_json: serde_json::Value = serde_json::from_slice(&tabs.stdout).unwrap();
    assert_eq!(tabs_json["result"]["tabs"].as_array().unwrap().len(), 1);

    cleanup_spawned_herdr(herdr, base);
}

#[test]
fn pane_close_removes_the_workspace_when_it_closes_the_last_pane() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("herdr.sock");

    let herdr = spawn_herdr(&config_home, &runtime_dir, &socket_path);
    wait_for_socket(&socket_path, Duration::from_secs(5));

    let created = run_cli(
        &socket_path,
        &["workspace", "create", "--cwd", base.to_str().unwrap()],
    );
    assert!(created.status.success());
    let created_json: serde_json::Value = serde_json::from_slice(&created.stdout).unwrap();
    let root_pane_id = created_json["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();

    let closed = run_cli(&socket_path, &["pane", "close", &root_pane_id]);
    assert!(closed.status.success());
    let closed_json: serde_json::Value = serde_json::from_slice(&closed.stdout).unwrap();
    assert_eq!(closed_json["result"]["type"], "ok");

    let workspaces = run_cli(&socket_path, &["workspace", "list"]);
    assert!(workspaces.status.success());
    let workspaces_json: serde_json::Value = serde_json::from_slice(&workspaces.stdout).unwrap();
    assert!(workspaces_json["result"]["workspaces"]
        .as_array()
        .unwrap()
        .is_empty());

    cleanup_spawned_herdr(herdr, base);
}

#[test]
fn pane_run_read_and_wait_commands_work() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("herdr.sock");

    let herdr = spawn_herdr(&config_home, &runtime_dir, &socket_path);
    wait_for_socket(&socket_path, Duration::from_secs(5));

    send_request(
        &socket_path,
        &format!(
            r#"{{"id":"req_cli_1","method":"workspace.create","params":{{"cwd":"{}","focus":true}}}}"#,
            base.display()
        ),
    );
    let create = run_cli(
        &socket_path,
        &[
            "pane",
            "run",
            "1-1",
            "echo alpha && echo beta && printf 'ready\\n'",
        ],
    );
    assert!(create.status.success());

    let started = Instant::now();
    let waited = run_cli(
        &socket_path,
        &[
            "pane",
            "wait-output",
            "1-1",
            "--match",
            "ready",
            "--source",
            "recent",
            "--lines",
            "40",
            "--timeout",
            "5000",
        ],
    );
    let elapsed = started.elapsed();
    assert!(
        waited.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&waited.stderr)
    );
    assert!(
        elapsed < Duration::from_millis(500),
        "already-matching wait took {elapsed:?}"
    );
    let waited_json: serde_json::Value = serde_json::from_slice(&waited.stdout).unwrap();
    assert_eq!(waited_json["result"]["type"], "output_matched");

    let read = run_cli(
        &socket_path,
        &["pane", "read", "1-1", "--source", "recent", "--lines", "40"],
    );
    assert!(read.status.success());
    let text = String::from_utf8(read.stdout).unwrap();
    assert!(text.contains("alpha"));
    assert!(text.contains("ready"));

    cleanup_spawned_herdr(herdr, base);
}

#[test]
fn wait_output_matches_recent_unwrapped_text() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("herdr.sock");

    let herdr = spawn_herdr(&config_home, &runtime_dir, &socket_path);
    wait_for_socket(&socket_path, Duration::from_secs(5));

    let created = run_cli(
        &socket_path,
        &["workspace", "create", "--cwd", base.to_str().unwrap()],
    );
    assert!(created.status.success());

    let token = "WRAP_WAIT_TEST_ABCDEFGHIJKLMNOPQRSTUVWXYZ_0123456789_ABCDEFGHIJKLMNOPQRSTUVWXYZ_0123456789";
    let script = base.join("emit-long-token.sh");
    std::fs::write(&script, format!("#!/bin/sh\nprintf '%s\\n' '{token}'\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();
    }

    let run = run_cli(
        &socket_path,
        &["pane", "run", "1-1", &format!("sh {}", script.display())],
    );
    assert!(run.status.success());

    let waited = run_cli(
        &socket_path,
        &[
            "pane",
            "wait-output",
            "1-1",
            "--match",
            token,
            "--source",
            "recent",
            "--lines",
            "80",
            "--timeout",
            "5000",
        ],
    );
    assert!(
        waited.status.success(),
        "stderr: {} stdout: {}",
        String::from_utf8_lossy(&waited.stderr),
        String::from_utf8_lossy(&waited.stdout)
    );

    let read = run_cli(
        &socket_path,
        &[
            "pane",
            "read",
            "1-1",
            "--source",
            "recent-unwrapped",
            "--lines",
            "80",
        ],
    );
    assert!(read.status.success());
    let text = String::from_utf8(read.stdout).unwrap();
    assert!(text.contains(token));

    cleanup_spawned_herdr(herdr, base);
}

#[test]
fn closing_pane_terminates_processes_inside_it() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("herdr.sock");

    let herdr = spawn_herdr(&config_home, &runtime_dir, &socket_path);
    wait_for_socket(&socket_path, Duration::from_secs(5));

    let created = run_cli(
        &socket_path,
        &["workspace", "create", "--cwd", base.to_str().unwrap()],
    );
    assert!(created.status.success());

    let split = run_cli(
        &socket_path,
        &["pane", "split", "1-1", "--direction", "right"],
    );
    assert!(split.status.success());
    let split_json: serde_json::Value = serde_json::from_slice(&split.stdout).unwrap();
    let pane_id = split_json["result"]["pane"]["pane_id"].as_str().unwrap();

    let pid_file = base.join("pane-close.pid");
    let command = format!(
        "python3 -c 'import os,time,pathlib; pathlib.Path(r\"{}\").write_text(str(os.getpid())); time.sleep(1000)'",
        pid_file.display()
    );
    let ran = run_cli(&socket_path, &["pane", "run", pane_id, &command]);
    assert!(
        ran.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&ran.stderr)
    );

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && !pid_file.exists() {
        thread::sleep(Duration::from_millis(25));
    }
    assert!(pid_file.exists(), "pid file was not created");

    let pid = wait_for_pid_file(&pid_file, Duration::from_secs(3)).unwrap_or_else(|err| {
        panic!("failed to read pane child pid: {err}");
    });
    assert!(process_exists(pid), "child process was not running");

    let closed = run_cli(&socket_path, &["pane", "close", pane_id]);
    assert!(
        closed.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&closed.stderr)
    );
    assert!(
        wait_for_pid_exit(pid, Duration::from_secs(3)),
        "process {pid} survived pane close"
    );

    cleanup_spawned_herdr(herdr, base);
}

#[test]
fn closing_a_pane_returns_before_the_signal_ladder_finishes() {
    // HSR-01：信号阶梯曾在事件循环内同步执行，关一个赖着不退的 pane 会把整个 server
    // 冻住最多 750 ms。阶梯移到 reaper 线程后，pane.close 必须在进程还活着时就返回，
    // 期间其它 API 照常响应，最终进程仍被 SIGKILL 收掉、不泄漏。
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("herdr.sock");

    let herdr = spawn_herdr(&config_home, &runtime_dir, &socket_path);
    wait_for_socket(&socket_path, Duration::from_secs(5));

    let created = run_cli(
        &socket_path,
        &["workspace", "create", "--cwd", base.to_str().unwrap()],
    );
    assert!(created.status.success());

    let split = run_cli(
        &socket_path,
        &["pane", "split", "1-1", "--direction", "right"],
    );
    assert!(split.status.success());
    let split_json: serde_json::Value = serde_json::from_slice(&split.stdout).unwrap();
    let pane_id = split_json["result"]["pane"]["pane_id"].as_str().unwrap();

    let pid_file = base.join("pane-close-stubborn.pid");
    let command = format!(
        "python3 -c 'import os,signal,time,pathlib; signal.signal(signal.SIGHUP, signal.SIG_IGN); signal.signal(signal.SIGTERM, signal.SIG_IGN); pathlib.Path(r\"{}\").write_text(str(os.getpid())); time.sleep(1000)'",
        pid_file.display()
    );
    let ran = run_cli(&socket_path, &["pane", "run", pane_id, &command]);
    assert!(
        ran.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&ran.stderr)
    );

    let pid = wait_for_pid_file(&pid_file, Duration::from_secs(5)).unwrap_or_else(|err| {
        panic!("failed to read pane child pid: {err}");
    });
    assert!(process_exists(pid), "child process was not running");

    // 基线：同一条 CLI 链路（起进程 + 连 socket + 请求 + 响应）在本机的往返成本。
    // 用相对基线而不是绝对毫秒做断言，负载高的机器上基线同样变慢，不会假红。
    let baseline_started = Instant::now();
    let listed_before = run_cli(&socket_path, &["workspace", "list"]);
    let baseline = baseline_started.elapsed();
    assert!(
        listed_before.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&listed_before.stderr)
    );

    let close_started = Instant::now();
    let closed = run_cli(&socket_path, &["pane", "close", pane_id]);
    let close_elapsed = close_started.elapsed();
    assert!(
        closed.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&closed.stderr)
    );
    // 同步阶梯要多花整条阶梯（3×250 ms）；这里只允许比基线多出不到一个首级宽限。
    let first_grace = Duration::from_millis(250);
    assert!(
        close_elapsed < baseline + first_grace,
        "pane close took {close_elapsed:?} against a {baseline:?} baseline: \
         it still waits for the signal ladder before answering"
    );
    if close_elapsed < first_grace {
        // 实测落在首级宽限内，SIGKILL 还没来得及发：进程必然还在。
        assert!(
            process_exists(pid),
            "pane close waited for the whole signal ladder before answering"
        );
    }

    // 阶梯还在跑，事件循环必须照常服务其它请求。
    let listed = run_cli(&socket_path, &["workspace", "list"]);
    assert!(
        listed.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&listed.stderr)
    );

    assert!(
        wait_for_pid_exit(pid, Duration::from_secs(5)),
        "process {pid} survived pane close"
    );

    cleanup_spawned_herdr(herdr, base);
}

#[test]
fn closing_a_pane_kills_background_processes_after_its_shell_exits() {
    // HSR-01 的竞态：关闭 pane 会先关 PTY master，shell 随即退出并在几毫秒内被 wait
    // 回收。终止阶梯跑在后台线程上，等它动手时 shell 的进程表条目往往已经消失——只有
    // 凭投递时刻快照的会话锚点，才能找到仍留在会话里的后台进程。按 child pid 现扫会话
    // 会扫出空集合，后台进程被永久泄漏。
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("herdr.sock");

    let herdr = spawn_herdr(&config_home, &runtime_dir, &socket_path);
    wait_for_socket(&socket_path, Duration::from_secs(5));

    let created = run_cli(
        &socket_path,
        &["workspace", "create", "--cwd", base.to_str().unwrap()],
    );
    assert!(created.status.success());

    let split = run_cli(
        &socket_path,
        &["pane", "split", "1-1", "--direction", "right"],
    );
    assert!(split.status.success());
    let split_json: serde_json::Value = serde_json::from_slice(&split.stdout).unwrap();
    let pane_id = split_json["result"]["pane"]["pane_id"].as_str().unwrap();

    let pid_file = base.join("pane-close-background.pid");
    // 后台运行且忽略 SIGHUP：shell 退出不会带走它，只有阶梯的 SIGTERM 能收掉它。
    let command = format!(
        "python3 -c 'import os,signal,time,pathlib; signal.signal(signal.SIGHUP, signal.SIG_IGN); pathlib.Path(r\"{}\").write_text(str(os.getpid())); time.sleep(1000)' &",
        pid_file.display()
    );
    let ran = run_cli(&socket_path, &["pane", "run", pane_id, &command]);
    assert!(
        ran.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&ran.stderr)
    );

    let pid = wait_for_pid_file(&pid_file, Duration::from_secs(5)).unwrap_or_else(|err| {
        panic!("failed to read background pid: {err}");
    });
    assert!(process_exists(pid), "background process was not running");

    let closed = run_cli(&socket_path, &["pane", "close", pane_id]);
    assert!(
        closed.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&closed.stderr)
    );

    assert!(
        wait_for_pid_exit(pid, Duration::from_secs(5)),
        "background process {pid} survived pane close"
    );

    cleanup_spawned_herdr(herdr, base);
}

#[test]
fn closing_workspace_terminates_processes_inside_it() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("herdr.sock");

    let herdr = spawn_herdr(&config_home, &runtime_dir, &socket_path);
    wait_for_socket(&socket_path, Duration::from_secs(5));

    let created = run_cli(
        &socket_path,
        &["workspace", "create", "--cwd", base.to_str().unwrap()],
    );
    assert!(created.status.success());

    let pid_file = base.join("workspace-close.pid");
    let command = format!(
        "python3 -c 'import os,time,pathlib; pathlib.Path(r\"{}\").write_text(str(os.getpid())); time.sleep(1000)'",
        pid_file.display()
    );
    let ran = run_cli(&socket_path, &["pane", "run", "1-1", &command]);
    assert!(
        ran.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&ran.stderr)
    );

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && !pid_file.exists() {
        thread::sleep(Duration::from_millis(25));
    }
    assert!(pid_file.exists(), "pid file was not created");

    let pid = wait_for_pid_file(&pid_file, Duration::from_secs(3)).unwrap_or_else(|err| {
        panic!("failed to read pane child pid: {err}");
    });
    assert!(process_exists(pid), "child process was not running");

    let closed = run_cli(&socket_path, &["workspace", "close", "1"]);
    assert!(
        closed.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&closed.stderr)
    );
    assert!(
        wait_for_pid_exit(pid, Duration::from_secs(3)),
        "process {pid} survived workspace close"
    );

    cleanup_spawned_herdr(herdr, base);
}

#[test]
fn workspace_ids_and_public_pane_ids_are_stable() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("herdr.sock");

    let herdr = spawn_herdr(&config_home, &runtime_dir, &socket_path);
    wait_for_socket(&socket_path, Duration::from_secs(5));

    let ws1_json = run_cli_json(
        &socket_path,
        &["workspace", "create", "--cwd", base.to_str().unwrap()],
    );
    let ws1_id = ws1_json["result"]["workspace"]["workspace_id"]
        .as_str()
        .unwrap()
        .to_string();

    let split_12_json = run_cli_json(
        &socket_path,
        &["pane", "split", "1-1", "--direction", "right", "--no-focus"],
    );
    assert_eq!(
        split_12_json["result"]["pane"]["pane_id"],
        format!("{ws1_id}:p2")
    );

    let split_13_json = run_cli_json(
        &socket_path,
        &["pane", "split", "1-1", "--direction", "down", "--no-focus"],
    );
    assert_eq!(
        split_13_json["result"]["pane"]["pane_id"],
        format!("{ws1_id}:p3")
    );

    let ws2_json = run_cli_json(
        &socket_path,
        &["workspace", "create", "--cwd", "/tmp", "--no-focus"],
    );
    let ws2_id = ws2_json["result"]["workspace"]["workspace_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(ws2_id, ws1_id);

    let ws2_focus = run_cli(&socket_path, &["workspace", "focus", &ws2_id]);
    assert!(
        ws2_focus.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&ws2_focus.stderr)
    );

    let ws2_split_json = run_cli_json(
        &socket_path,
        &["pane", "split", "2-1", "--direction", "right", "--no-focus"],
    );
    assert_eq!(
        ws2_split_json["result"]["pane"]["pane_id"],
        format!("{ws2_id}:p2")
    );

    let ws3_json = run_cli_json(
        &socket_path,
        &["workspace", "create", "--cwd", "/", "--no-focus"],
    );
    let ws3_id = ws3_json["result"]["workspace"]["workspace_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(ws3_id, ws1_id);
    assert_ne!(ws3_id, ws2_id);

    let close_ws2 = run_cli(&socket_path, &["workspace", "close", &ws2_id]);
    assert!(
        close_ws2.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&close_ws2.stderr)
    );

    let workspaces_json = run_cli_json(&socket_path, &["workspace", "list"]);
    let ids: Vec<String> = workspaces_json["result"]["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .map(|ws| ws["workspace_id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids, vec![ws1_id.clone(), ws3_id.clone()]);

    let new_ws_json = run_cli_json(
        &socket_path,
        &["workspace", "create", "--cwd", "/var/tmp", "--no-focus"],
    );
    let new_ws_id = new_ws_json["result"]["workspace"]["workspace_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(new_ws_id, ws1_id);
    assert_ne!(new_ws_id, ws2_id);
    assert_ne!(new_ws_id, ws3_id);

    let ws3_panes_json = run_cli_json(&socket_path, &["pane", "list", "--workspace", &ws3_id]);
    assert_eq!(
        ws3_panes_json["result"]["panes"][0]["pane_id"],
        format!("{ws3_id}:p1")
    );

    let close_middle = run_cli(&socket_path, &["pane", "close", &format!("{ws1_id}-2")]);
    assert!(
        close_middle.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&close_middle.stderr)
    );

    let ws1_panes_json = run_cli_json(&socket_path, &["pane", "list", "--workspace", &ws1_id]);
    let pane_ids: Vec<String> = ws1_panes_json["result"]["panes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|pane| pane["pane_id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        pane_ids,
        vec![format!("{ws1_id}:p1"), format!("{ws1_id}:p3")]
    );

    let closed_lookup = run_cli(&socket_path, &["pane", "get", &format!("{ws1_id}:p2")]);
    assert!(
        !closed_lookup.status.success(),
        "closed pane id should not retarget: {}",
        String::from_utf8_lossy(&closed_lookup.stdout)
    );

    let split_14_json = run_cli_json(
        &socket_path,
        &[
            "pane",
            "split",
            &format!("{ws1_id}:p1"),
            "--direction",
            "right",
            "--no-focus",
        ],
    );
    assert_eq!(
        split_14_json["result"]["pane"]["pane_id"],
        format!("{ws1_id}:p4")
    );

    cleanup_spawned_herdr(herdr, base);
}

#[test]
fn pane_shell_gets_herdr_socket_and_pane_env() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("herdr.sock");

    let herdr = spawn_herdr(&config_home, &runtime_dir, &socket_path);
    wait_for_socket(&socket_path, Duration::from_secs(5));

    let created = send_request(
        &socket_path,
        &format!(
            r#"{{"id":"req_env_1","method":"workspace.create","params":{{"cwd":"{}","focus":true}}}}"#,
            base.display()
        ),
    );
    let pane_id = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();

    let env_capture = base.join("pane-env.txt");
    let ran = run_cli(
        &socket_path,
        &[
            "pane",
            "run",
            "1-1",
            &format!(
                "printf '%s\\n%s\\n' \"$HERDR_SOCKET_PATH\" \"$HERDR_PANE_ID\" > {}",
                env_capture.display()
            ),
        ],
    );
    assert!(ran.status.success());

    let deadline = Instant::now() + Duration::from_secs(3);
    let mut text = String::new();
    while Instant::now() < deadline {
        if env_capture.exists() {
            text = fs::read_to_string(&env_capture).unwrap();
            if text.contains(&socket_path.display().to_string()) && text.contains(&pane_id) {
                break;
            }
        }
        thread::sleep(Duration::from_millis(25));
    }
    assert!(env_capture.exists(), "env capture file was not created");
    assert!(
        text.contains(&socket_path.display().to_string()),
        "env file was: {text:?}"
    );
    assert!(text.contains(&pane_id), "env file was: {text:?}");

    cleanup_spawned_herdr(herdr, base);
}

#[test]
fn pane_agent_reports_accept_options_before_pane() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("herdr.sock");

    let herdr = spawn_herdr(&config_home, &runtime_dir, &socket_path);
    wait_for_socket(&socket_path, Duration::from_secs(5));

    let created = send_request(
        &socket_path,
        &format!(
            r#"{{"id":"req_agent_report_1","method":"workspace.create","params":{{"cwd":"{}","focus":true}}}}"#,
            base.display()
        ),
    );
    let pane_id = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();

    let state_report = run_cli(
        &socket_path,
        &[
            "pane",
            "report-agent",
            "--source=custom:cli-test",
            "--agent=cli-test",
            "--state=working",
            "--message=parsing=works",
            &pane_id,
        ],
    );
    assert!(
        state_report.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&state_report.stderr)
    );
    assert!(state_report.stdout.is_empty());
    assert!(state_report.stderr.is_empty());

    let agent = run_cli_json(&socket_path, &["agent", "get", &pane_id]);
    assert_eq!(agent["result"]["agent"]["agent"], "cli-test");
    assert_eq!(agent["result"]["agent"]["agent_status"], "working");

    let session_report = run_cli(
        &socket_path,
        &[
            "pane",
            "report-agent-session",
            "--source=custom:cli-test",
            "--agent=cli-test",
            "--seq=1",
            "--agent-session-id=session=1",
            "--session-start-source=startup",
            &pane_id,
        ],
    );
    assert!(
        session_report.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&session_report.stderr)
    );
    assert!(session_report.stdout.is_empty());
    assert!(session_report.stderr.is_empty());

    cleanup_spawned_herdr(herdr, base);
}

#[test]
fn pane_read_rejects_invalid_value_with_usage_error() {
    // Invalid option values fail as CLI usage errors before any server
    // contact: exit 2 with the plain parser message, not the old
    // `Error: Custom { ... }` io::Error wrapper from main.
    let socket_path = Path::new("/tmp/herdr-cli-invalid-values-no-server.sock");

    let read = run_cli(socket_path, &["pane", "read", "w1:p1", "--source", "bogus"]);
    assert_eq!(read.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&read.stderr);
    assert!(stderr.contains("invalid read source: bogus"));
    assert!(!stderr.contains("Error: Custom"));
}

/// RL12：`pane process-info` 能查任何窗格，前台进程命令行里疑似凭据的值要打码，
/// 程序名与参数结构原样保留。
#[test]
fn pane_process_info_redacts_credentials_in_foreground_command_lines() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("herdr.sock");

    let herdr = spawn_herdr(&config_home, &runtime_dir, &socket_path);
    wait_for_socket(&socket_path, LOADED_WAIT);
    let created = run_cli_json(
        &socket_path,
        &["workspace", "create", "--cwd", base.to_str().unwrap()],
    );
    let pane_id = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();

    // `sleep; :` 让 sh 留在前台当 sleep 的父进程，argv 里带着手写的假凭据。
    let ready = base.join("probe-ready");
    let script = "touch \"$0\"; sleep 60; :";
    let command = format!(
        "/bin/sh -c '{script}' '{}' --token=fake-s3cr3t-1 --api-key fake-s3cr3t-2 -H 'Authorization: Bearer fake-s3cr3t-3' OPENAI_API_KEY=fake-s3cr3t-4 https://user:fake-s3cr3t-5@example.invalid/repo",
        ready.display()
    );
    let ran = run_cli(&socket_path, &["pane", "run", &pane_id, &command]);
    assert!(
        ran.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&ran.stderr)
    );
    assert!(
        wait_until(LOADED_WAIT, Duration::from_millis(25), || ready.exists()),
        "probe command did not start"
    );

    // 前台进程组可能晚一拍才换成这条命令：轮询到它出现为止。
    let deadline = Instant::now() + LOADED_WAIT;
    let (stdout, probe) = loop {
        let output = run_cli(&socket_path, &["pane", "process-info", "--pane", &pane_id]);
        assert!(
            output.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        let info: serde_json::Value = serde_json::from_str(&stdout).unwrap();
        let probe = info["result"]["process_info"]["foreground_processes"]
            .as_array()
            .and_then(|processes| {
                processes.iter().find(|process| {
                    process["argv"]
                        .as_array()
                        .is_some_and(|argv| argv.iter().any(|arg| arg == "--api-key"))
                })
            })
            .cloned();
        if let Some(probe) = probe {
            break (stdout, probe);
        }
        assert!(
            Instant::now() < deadline,
            "probe never showed up as a foreground process: {stdout}"
        );
        thread::sleep(Duration::from_millis(50));
    };

    assert!(
        !stdout.contains("fake-s3cr3t"),
        "credential leaked: {stdout}"
    );
    let ready_arg = ready.display().to_string();
    let expected = [
        "/bin/sh",
        "-c",
        script,
        ready_arg.as_str(),
        "--token=[REDACTED]",
        "--api-key",
        "[REDACTED]",
        "-H",
        "Authorization: Bearer [REDACTED]",
        "OPENAI_API_KEY=[REDACTED]",
        "https://user:[REDACTED]@example.invalid/repo",
    ];
    assert_eq!(probe["argv"], serde_json::json!(expected), "{probe}");
    assert_eq!(probe["cmdline"], expected.join(" "), "{probe}");

    cleanup_spawned_herdr(herdr, base);
}
