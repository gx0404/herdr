//! `herdr api usage-report --passthrough` 的进程级契约：statusline 管道里的 herdr 必须逐字节
//! 透传 stdin、stderr 保持为空、上报 fire-and-forget（不等 server 响应就退出）。socket 一律
//! 指向隔离路径，绝不能碰到运行测试的那个 herdr 会话。
//!
//! 包装串（写进用户 settings.json 的 statusLine 命令）在这里按字面量钉住并交给真实的 bash
//! 执行：单测只能断言字符串形态，原渲染器何时读到 EOF 只有真实 shell 能证明。

use super::harness::*;

/// 写进 Claude Code settings.json 的管道形态（`platform::usage_statusline_pipeline` 的输出，
/// 与 `src/platform/unix_common.rs` 的单测逐字一致）：回调子 shell `exec` 成 herdr，原渲染命令
/// 在管道末端的分组里整体读同一份 stdin。
fn claude_statusline_pipeline(renderer: &str) -> String {
    format!(
        "# herdr-usage v1\n(if [ \"${{HERDR_ENV:-}}\" = 1 ] && [ -n \"${{HERDR_BIN_PATH:-}}\" ]; then exec \"$HERDR_BIN_PATH\" api usage-report --agent claude --passthrough; else exec cat; fi) | (\n{renderer}\n)"
    )
}

/// 没有原渲染命令时的独立形态：同样走 `--passthrough`（热路径 + fire-and-forget + 静默），
/// 回放的 stdout 丢弃，statusline 不显示任何内容。
const CLAUDE_STATUSLINE_STANDALONE: &str = "# herdr-usage v1\n(if [ \"${HERDR_ENV:-}\" = 1 ] && [ -n \"${HERDR_BIN_PATH:-}\" ]; then exec \"$HERDR_BIN_PATH\" api usage-report --agent claude --passthrough >/dev/null; else :; fi)";

/// 子进程会不会读 stdin：决定写 stdin 时对 `BrokenPipe` 的容忍度。
#[derive(Clone, Copy, PartialEq, Eq)]
enum StdinUse {
    /// 包装串一定读 stdin（透传给 herdr 或 `exec cat`）：写失败就是缺陷。
    Read,
    /// 包装串按设计不读 stdin（herdr 之外的独立形态只跑 `:`）：bash 可能在测试写入前
    /// 就退出、关掉管道读端，写入得到 `BrokenPipe` 属于预期。
    MayIgnore,
}

/// 用真实 bash 跑包装串（Claude Code 用 shell 执行 statusLine 命令）；`herdr_env` 决定是否
/// 模拟在 herdr 会话内（`HERDR_ENV=1` + `HERDR_BIN_PATH` 指向测试二进制）。返回子进程与
/// 写完 stdin 的时刻。
fn spawn_statusline_shell(
    command: &str,
    herdr_env: bool,
    socket_path: Option<&Path>,
    input: &[u8],
) -> (std::process::Child, Instant) {
    let mut child = spawn_statusline_process(command, herdr_env, socket_path);
    feed_statusline_stdin(&mut child, input, StdinUse::Read);
    (child, Instant::now())
}

/// 只起进程、不写 stdin：调用方按 [`StdinUse`] 自己喂输入。
fn spawn_statusline_process(
    command: &str,
    herdr_env: bool,
    socket_path: Option<&Path>,
) -> std::process::Child {
    let mut shell = Command::new("bash");
    shell.arg("-c").arg(command);
    shell.env("HERDR_LANG", "en");
    shell.env("HERDR_PANE_ID", "w1:p1");
    shell.env_remove("HERDR_SESSION");
    shell.env_remove("HERDR_CLIENT_SOCKET_PATH");
    if herdr_env {
        shell.env("HERDR_ENV", "1");
        shell.env("HERDR_BIN_PATH", env!("CARGO_BIN_EXE_herdr"));
    } else {
        shell.env_remove("HERDR_ENV");
        // 宿主在 herdr 窗格内跑测试时会注入 HERDR_STARTUP_CWD：server 会据此预建启动工作区，破坏用例的工作区/pane 假设。
        shell.env_remove("HERDR_STARTUP_CWD");
        shell.env_remove("HERDR_BIN_PATH");
    }
    match socket_path {
        Some(path) => shell.env("HERDR_SOCKET_PATH", path),
        None => shell.env_remove("HERDR_SOCKET_PATH"),
    };
    shell
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    shell.spawn().unwrap()
}

/// 把 Claude Code 写给 statusLine 命令的 JSON 写进子进程 stdin 并关闭写端。子进程按设计
/// 不读 stdin 时（[`StdinUse::MayIgnore`]），它先退出造成的 `BrokenPipe` 不算失败；其它
/// 写入错误照样让用例失败。
fn feed_statusline_stdin(child: &mut std::process::Child, input: &[u8], usage: StdinUse) {
    let mut stdin = child.stdin.take().unwrap();
    match stdin.write_all(input) {
        Ok(()) => {}
        Err(error)
            if usage == StdinUse::MayIgnore && error.kind() == std::io::ErrorKind::BrokenPipe => {}
        Err(error) => panic!("写 statusline 命令的 stdin 失败：{error}"),
    }
    drop(stdin);
}

/// 把监听 socket 的 backlog 压到 0 并塞进一个未被 accept 的连接：之后的 `connect()` 会
/// 阻塞（非阻塞形态得到 `EAGAIN`），模拟「server 忙、来不及 accept」。返回占位连接，drop
/// 即释放。
fn saturate_listener(listener: &UnixListener, path: &Path) -> UnixStream {
    use std::os::fd::AsRawFd as _;
    assert_eq!(
        unsafe { libc::listen(listener.as_raw_fd(), 0) },
        0,
        "listen(0) failed: {}",
        std::io::Error::last_os_error()
    );
    let pending = UnixStream::connect(path).unwrap();
    assert!(
        connect_would_block(path),
        "前置条件不成立：backlog 未填满，connect 不会阻塞"
    );
    pending
}

/// 非阻塞 connect 一次：backlog 满时 Linux 返回 `EAGAIN`。
fn connect_would_block(path: &Path) -> bool {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};
    use std::os::unix::ffi::OsStrExt as _;
    let fd = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        )
    };
    assert!(fd >= 0, "socket(): {}", std::io::Error::last_os_error());
    let socket = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let bytes = path.as_os_str().as_bytes();
    assert!(bytes.len() < address.sun_path.len(), "socket 路径过长");
    for (slot, byte) in address.sun_path.iter_mut().zip(bytes) {
        *slot = *byte as libc::c_char;
    }
    let result = unsafe {
        libc::connect(
            socket.as_raw_fd(),
            std::ptr::addr_of!(address).cast::<libc::sockaddr>(),
            std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
        )
    };
    result < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::WouldBlock
}

fn run_usage_report(
    socket_path: &Path,
    pane_id: Option<&str>,
    args: &[&str],
    input: &[u8],
) -> (std::process::Output, Duration) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_herdr"));
    command.args(["api", "usage-report"]);
    command.args(args);
    command.env("HERDR_SOCKET_PATH", socket_path);
    command.env("HERDR_LANG", "en");
    command.env_remove("HERDR_SESSION");
    match pane_id {
        Some(pane_id) => command.env("HERDR_PANE_ID", pane_id),
        None => command.env_remove("HERDR_PANE_ID"),
    };
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let started = Instant::now();
    let mut child = command.spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let payload = input.to_vec();
    let writer = thread::spawn(move || {
        stdin.write_all(&payload).unwrap();
    });
    let output = child.wait_with_output().unwrap();
    let elapsed = started.elapsed();
    writer.join().unwrap();
    (output, elapsed)
}

fn assert_passthrough(output: &std::process::Output, input: &[u8]) {
    assert!(
        output.stderr.is_empty(),
        "statusline 管道里的 stderr 必须为空：{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout.len(), input.len(), "stdout 长度与 stdin 不同");
    assert!(output.stdout == input, "stdout 与 stdin 逐字节不同");
}

#[test]
fn usage_report_passthrough_binary_keeps_stdout_byte_exact_and_stderr_empty() {
    let base = unique_test_dir();
    fs::create_dir_all(&base).unwrap();
    // socket 指向不存在的路径：上报连不上也必须静默、退出码 0。
    let socket_path = base.join("missing.sock");
    // 无尾随换行、含非 ASCII 与 NUL：原渲染器读到的必须与 Claude Code 写入的完全一致。
    let mut input = b"{\"model\":{\"display_name\":\"Opus\"},\"cwd\":\"/tmp/\xe8\xb7\xaf\xe5\xbe\x84\",\"x\":\"".to_vec();
    input.extend_from_slice(&[0, 1, 2]);
    input.extend_from_slice(b"\"}");
    let (output, _) = run_usage_report(
        &socket_path,
        Some("w1:p1"),
        &["--agent", "claude", "--passthrough"],
        &input,
    );
    assert_passthrough(&output, &input);
    assert_eq!(output.status.code(), Some(0));
    let _ = fs::remove_dir_all(base);
}

#[test]
fn usage_report_passthrough_binary_streams_more_than_one_mebibyte() {
    let base = unique_test_dir();
    fs::create_dir_all(&base).unwrap();
    let socket_path = base.join("missing.sock");
    let mut input = b"{\"pad\":\"".to_vec();
    input.resize(1024 * 1024 + 64 * 1024, b'x');
    input.extend_from_slice(b"\"}");
    let (output, _) = run_usage_report(
        &socket_path,
        Some("w1:p1"),
        &["--agent", "claude", "--passthrough"],
        &input,
    );
    assert_passthrough(&output, &input);
    assert_eq!(output.status.code(), Some(0));
    let _ = fs::remove_dir_all(base);
}

#[test]
fn usage_report_passthrough_binary_replays_invalid_json_and_bad_options() {
    let base = unique_test_dir();
    fs::create_dir_all(&base).unwrap();
    let socket_path = base.join("missing.sock");
    let input = b"not json \xff\xfe";
    let (output, _) = run_usage_report(
        &socket_path,
        Some("w1:p1"),
        &["--agent", "claude", "--passthrough"],
        input,
    );
    assert_passthrough(&output, input);
    assert_eq!(output.status.code(), Some(0), "坏 JSON 只透传");

    let input = b"{\"model\":{}}";
    let (output, _) = run_usage_report(
        &socket_path,
        Some("w1:p1"),
        &["--agent", "claude", "--passthrough", "--bogus"],
        input,
    );
    assert_passthrough(&output, input);
    assert_eq!(output.status.code(), Some(2), "参数错误仍先回放 stdout");
    let _ = fs::remove_dir_all(base);
}

/// 假 server：接一个连接、读一行请求、然后既不回复也不关连接。
fn accept_one_request(
    listener: UnixListener,
    hold: Duration,
) -> thread::JoinHandle<Option<String>> {
    thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            match listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    let mut line = String::new();
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    reader.read_line(&mut line).unwrap();
                    thread::sleep(hold);
                    drop(stream);
                    return Some(line);
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(err) => panic!("accept failed: {err}"),
            }
        }
        None
    })
}

#[test]
fn usage_report_passthrough_binary_returns_before_the_server_replies() {
    let base = unique_test_dir();
    fs::create_dir_all(&base).unwrap();
    let socket_path = base.join("herdr.sock");
    let listener = UnixListener::bind(&socket_path).unwrap();
    let hold = Duration::from_secs(1);
    let server = accept_one_request(listener, hold);
    // 旧实现会等 server 响应直到 500 ms 收超时才退出；fire-and-forget 只写不读，本机 Unix
    // socket 连接加写入远低于这个门槛。
    let budget = Duration::from_millis(500);

    let input = b"{\"model\":{\"display_name\":\"Opus\"},\"rate_limits\":{}}";
    let (output, elapsed) = run_usage_report(
        &socket_path,
        Some("w1:p1"),
        &["--agent", "claude", "--passthrough"],
        input,
    );
    assert_passthrough(&output, input);
    assert_eq!(output.status.code(), Some(0));
    assert!(
        elapsed < budget,
        "server 迟迟不回复时 CLI 也应写完请求就退出，实际耗时 {elapsed:?}"
    );

    let line = server.join().unwrap().expect("server 应收到一条请求");
    let request: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(request["method"], "account.usage.report");
    assert_eq!(request["params"]["agent"], "claude");
    assert_eq!(request["params"]["pane_id"], "w1:p1");
    assert_eq!(
        request["params"]["official_payload"]["model"]["display_name"],
        "Opus"
    );
    let _ = fs::remove_dir_all(base);
}

#[test]
fn usage_report_passthrough_binary_skips_socket_without_pane() {
    let base = unique_test_dir();
    fs::create_dir_all(&base).unwrap();
    let socket_path = base.join("herdr.sock");
    let listener = UnixListener::bind(&socket_path).unwrap();
    listener.set_nonblocking(true).unwrap();

    let input = b"{\"model\":{}}";
    let (output, _) = run_usage_report(
        &socket_path,
        None,
        &["--agent", "claude", "--passthrough"],
        input,
    );
    assert_passthrough(&output, input);
    assert_eq!(output.status.code(), Some(0));
    // 进程已退出：此刻不该有任何连接进来。
    match listener.accept() {
        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {}
        Ok(_) => panic!("不在 pane 里、也没指定账号时不应连接 server"),
        Err(err) => panic!("accept failed: {err}"),
    }
    let _ = fs::remove_dir_all(base);
}

/// B-10 的核心目标：server 忙（connect 阻塞）时原渲染器也必须立刻读到 EOF——回调进程写完
/// stdout 就让出管道写端，再去等上报；断言的是渲染器读到 EOF 的时刻，而不是字符串里有没有
/// `exec`。对照：不让出 fd 1 时渲染器要等到回调进程放弃上报（250 ms）才读到 EOF。
#[test]
fn usage_statusline_pipeline_releases_the_renderer_before_the_report_settles() {
    let base = unique_test_dir();
    fs::create_dir_all(&base).unwrap();
    let socket_path = base.join("herdr.sock");
    let listener = UnixListener::bind(&socket_path).unwrap();
    let _pending = saturate_listener(&listener, &socket_path);

    let input = b"{\"model\":{\"display_name\":\"Opus\"}}";
    let pipeline = claude_statusline_pipeline("cat >/dev/null; echo renderer-eof");
    let (mut child, stdin_closed) =
        spawn_statusline_shell(&pipeline, true, Some(&socket_path), input);
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut stderr = child.stderr.take().unwrap();
    let stderr_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut stderr, &mut bytes).unwrap();
        bytes
    });
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    let renderer_eof = Instant::now();
    let status = child.wait().unwrap();
    let shell_exit = Instant::now();
    let stderr = stderr_reader.join().unwrap();

    assert_eq!(line, "renderer-eof\n");
    assert!(
        stderr.is_empty(),
        "stderr 必须为空：{}",
        String::from_utf8_lossy(&stderr)
    );
    assert_eq!(status.code(), Some(0));
    let eof_after = renderer_eof - stdin_closed;
    let exit_after_eof = shell_exit - renderer_eof;
    // 回调进程被阻塞的 connect 拖住约 250 ms 才退出；渲染器的 EOF 必须明显早于这个时刻。
    assert!(
        exit_after_eof >= Duration::from_millis(100),
        "渲染器读到 EOF（stdin 关闭后 {eof_after:?}）与回调进程退出之间只差 {exit_after_eof:?}：\
         回调进程仍持有管道写端，原渲染器被上报等待拖住"
    );
    drop(listener);
    let _ = fs::remove_dir_all(base);
}

/// 包装串在 herdr 之外退化为 `exec cat`，在 herdr 之内（哪怕 server 不在）由回调透传：两种
/// 情况下原渲染器读到的都必须是 Claude Code 写入的原始字节。
#[test]
fn usage_statusline_pipeline_passes_input_through_inside_and_outside_herdr() {
    let base = unique_test_dir();
    fs::create_dir_all(&base).unwrap();
    let input = b"{\"model\":{\"display_name\":\"Opus\"},\"cwd\":\"/tmp/\xe8\xb7\xaf\"}";
    let pipeline = claude_statusline_pipeline("cat");
    for herdr_env in [false, true] {
        let (child, _) = spawn_statusline_shell(
            &pipeline,
            herdr_env,
            Some(&base.join("missing.sock")),
            input,
        );
        let output = child.wait_with_output().unwrap();
        assert!(
            output.stderr.is_empty(),
            "herdr_env={herdr_env}: stderr 必须为空：{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.stdout == input,
            "herdr_env={herdr_env}: 渲染器读到的字节与输入不同"
        );
        assert_eq!(output.status.code(), Some(0));
    }
    let _ = fs::remove_dir_all(base);
}

/// 独立形态（用户原本没有 statusLine）同样是每次刷新都跑的热路径：stdout 必须为空（否则
/// Claude Code 会把报文当状态栏文字显示）、stderr 为空、退出码 0，server 不回复时也不等。
#[test]
fn usage_statusline_standalone_wrapper_stays_silent_and_does_not_wait_for_the_server() {
    let base = unique_test_dir();
    fs::create_dir_all(&base).unwrap();
    let socket_path = base.join("herdr.sock");
    let listener = UnixListener::bind(&socket_path).unwrap();
    let server = accept_one_request(listener, Duration::from_secs(1));

    let input = b"{\"model\":{\"display_name\":\"Opus\"}}";
    let (child, started) = spawn_statusline_shell(
        CLAUDE_STATUSLINE_STANDALONE,
        true,
        Some(&socket_path),
        input,
    );
    let output = child.wait_with_output().unwrap();
    let elapsed = started.elapsed();
    assert!(
        output.stdout.is_empty(),
        "独立形态不得向 statusline 输出任何内容：{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        output.stderr.is_empty(),
        "stderr 必须为空：{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.status.code(), Some(0));
    assert!(
        elapsed < Duration::from_millis(500),
        "server 不回复时独立形态也应写完请求就退出，实际耗时 {elapsed:?}"
    );
    let line = server.join().unwrap().expect("server 应收到一条请求");
    let request: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(request["method"], "account.usage.report");
    assert_eq!(request["params"]["agent"], "claude");

    // herdr 之外：什么都不做，也不读 stdin。bash 可能抢在写入前退出（负载高时常见），
    // 写入端的 BrokenPipe 由 `StdinUse::MayIgnore` 兜住。
    let mut child = spawn_statusline_process(CLAUDE_STATUSLINE_STANDALONE, false, None);
    feed_statusline_stdin(&mut child, input, StdinUse::MayIgnore);
    let output = child.wait_with_output().unwrap();
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    assert_eq!(output.status.code(), Some(0));
    let _ = fs::remove_dir_all(base);
}

/// D15：herdr 之外的独立形态按设计不读 stdin，bash 可能在测试写入前就退出并关掉管道
/// 读端——负载高时这正是上面用例偶发 BrokenPipe 的时序。这里先等 bash 退出再写，把它
/// 钉成必现：写入失败不算缺陷，输出与退出码照常断言。
#[test]
fn usage_statusline_standalone_wrapper_outside_herdr_tolerates_a_closed_stdin() {
    let input = b"{\"model\":{\"display_name\":\"Opus\"}}";
    let mut child = spawn_statusline_process(CLAUDE_STATUSLINE_STANDALONE, false, None);
    let deadline = Instant::now() + Duration::from_secs(10);
    while child.try_wait().unwrap().is_none() {
        assert!(
            Instant::now() < deadline,
            "herdr 之外的独立形态只跑 `:`，bash 应立即退出"
        );
        thread::sleep(Duration::from_millis(5));
    }
    feed_statusline_stdin(&mut child, input, StdinUse::MayIgnore);
    let output = child.wait_with_output().unwrap();
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    assert_eq!(output.status.code(), Some(0));
}

/// 热路径跳过了会话参数解析，但 socket 归属必须与完整路径一致：只设 `HERDR_SESSION`（不设
/// `HERDR_SOCKET_PATH`）时上报落到 `sessions/<name>/herdr.sock`；`--session` 写在前面走完整
/// 路径，落点相同。
#[test]
fn usage_report_passthrough_binary_reports_to_the_named_session_socket() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let socket_path = named_session_socket(&config_home, "work");
    fs::create_dir_all(socket_path.parent().unwrap()).unwrap();
    let input = b"{\"model\":{\"display_name\":\"Opus\"}}";
    let shapes: [&[&str]; 2] = [
        &["api", "usage-report", "--agent", "claude", "--passthrough"],
        &[
            "--session",
            "work",
            "api",
            "usage-report",
            "--agent",
            "claude",
            "--passthrough",
        ],
    ];
    // HOME 隔离到测试目录：passthrough 上报进程也是 herdr 子进程，按同一约定固定 HOME，
    // 避免读到开发机上真实的 CLI 数据。
    let home = base.join("home");
    fs::create_dir_all(&home).unwrap();
    for args in shapes {
        let listener = UnixListener::bind(&socket_path).unwrap();
        let server = accept_one_request(listener, Duration::ZERO);
        let output = Command::new(env!("CARGO_BIN_EXE_herdr"))
            .args(args)
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", &config_home)
            .env("HERDR_SESSION", "work")
            .env("HERDR_PANE_ID", "w1:p1")
            .env("HERDR_LANG", "en")
            .env_remove("HERDR_SOCKET_PATH")
            .env_remove("HERDR_CLIENT_SOCKET_PATH")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                child.stdin.take().unwrap().write_all(input)?;
                child.wait_with_output()
            })
            .unwrap();
        assert_passthrough(&output, input);
        assert_eq!(output.status.code(), Some(0), "{args:?}");
        let line = server
            .join()
            .unwrap()
            .unwrap_or_else(|| panic!("{args:?}: 请求没有落到 {}", socket_path.display()));
        let request: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(request["method"], "account.usage.report");
        assert_eq!(request["params"]["pane_id"], "w1:p1");
        fs::remove_file(&socket_path).unwrap();
    }
    let _ = fs::remove_dir_all(base);
}
