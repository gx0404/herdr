//! 有界官方 CLI 查询。辅助进程没有工作 pane 身份，也不会发送模型任务。

use std::io::{self, BufRead, Read, Write};
use std::process::{Child, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use super::registry::Provider;
use crate::api::schema::ObservationStatus;
use crate::config::UsageAccountConfig;
use serde_json::{json, Value};

pub(super) type QueryError = (ObservationStatus, String);
const MAX_OUTPUT: usize = 2 * 1024 * 1024;

struct ChildGuard {
    child: Child,
    guard: crate::platform::UsageProbeGuard,
    readers: Vec<std::thread::JoinHandle<()>>,
}
impl ChildGuard {
    fn new(mut child: Child) -> Result<Self, QueryError> {
        let guard = crate::platform::UsageProbeGuard::new(&child).map_err(|_| {
            let _ = child.kill();
            let _ = child.wait();
            (ObservationStatus::Error, "无法隔离官方查询进程".into())
        })?;
        Ok(Self {
            child,
            guard,
            readers: Vec::new(),
        })
    }
}
impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.guard.terminate();
        crate::platform::terminate_usage_probe(&mut self.child);
        for reader in self.readers.drain(..) {
            let _ = reader.join();
        }
    }
}

struct ProbeDirectory(std::path::PathBuf);
impl ProbeDirectory {
    fn new() -> Result<Self, QueryError> {
        static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "herdr-usage-{}-{}-{sequence}",
            std::process::id(),
            crate::server::observability::now_ms()
        ));
        std::fs::create_dir(&path)
            .map_err(|_| (ObservationStatus::Error, "无法创建隔离查询目录".into()))?;
        Ok(Self(path))
    }
}
impl Drop for ProbeDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn profile_variable(agent: &str) -> Option<&'static str> {
    match agent {
        "codex" => Some("CODEX_HOME"),
        "claude" => Some("CLAUDE_CONFIG_DIR"),
        "kimi" => Some("KIMI_CODE_HOME"),
        "qodercli" => Some("QODER_CONFIG_DIR"),
        "hermes" => Some("HERMES_HOME"),
        "pi" => Some("PI_CODING_AGENT_DIR"),
        _ => None,
    }
}

fn command(
    provider: &Provider,
    account: &UsageAccountConfig,
    directory: &ProbeDirectory,
) -> Result<std::process::Command, QueryError> {
    if account.profile_dir.is_some() && profile_variable(provider.agent).is_none() {
        return Err((
            ObservationStatus::Unsupported,
            "此 CLI 未配置经过验证的独立 profile 选择方式".into(),
        ));
    }
    let mut command = crate::noninteractive_process::command(provider.command);
    crate::platform::configure_usage_probe_command(&mut command);
    command
        .current_dir(&directory.0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    for variable in [
        "HERDR_ENV",
        "HERDR_SOCKET_PATH",
        "HERDR_CLIENT_SOCKET_PATH",
        "HERDR_PANE_ID",
        "HERDR_TERMINAL_ID",
        "HERDR_WORKSPACE_ID",
        "HERDR_TAB_ID",
        "HERDR_SESSION",
    ] {
        command.env_remove(variable);
    }
    command.env("HERDR_USAGE_PROBE", "1").env("NO_COLOR", "1");
    if let Some((variable, path)) =
        profile_variable(provider.agent).zip(account.profile_dir.as_ref())
    {
        command.env(variable, path);
    }
    Ok(command)
}

fn spawn_error(error: io::Error) -> QueryError {
    if error.kind() == io::ErrorKind::NotFound {
        (
            ObservationStatus::Unavailable,
            "此主机未安装对应官方 CLI，或 server 的 PATH 中不可用".into(),
        )
    } else {
        (ObservationStatus::Error, "无法启动官方 CLI 查询".into())
    }
}

pub(super) fn capture(
    provider: &Provider,
    account: &UsageAccountConfig,
    args: &[&str],
    timeout: Duration,
) -> Result<String, QueryError> {
    let deadline = Instant::now() + timeout;
    let directory = ProbeDirectory::new()?;
    let mut child = ChildGuard::new(
        command(provider, account, &directory)?
            .args(args)
            .spawn()
            .map_err(spawn_error)?,
    )?;
    let stdout = child
        .child
        .stdout
        .take()
        .ok_or_else(|| (ObservationStatus::Error, "查询输出不可用".into()))?;
    let (sender, receiver) = mpsc::sync_channel(1);
    let reader = std::thread::spawn(move || {
        let mut output = Vec::new();
        let result = stdout
            .take((MAX_OUTPUT + 1) as u64)
            .read_to_end(&mut output)
            .map(|_| output);
        let _ = sender.send(result);
    });
    child.readers.push(reader);
    let output = receiver
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(|_| (ObservationStatus::Error, "官方 CLI 查询超时".into()))?
        .map_err(|_| (ObservationStatus::Error, "无法读取官方 CLI 输出".into()))?;
    if output.len() > MAX_OUTPUT {
        return Err((
            ObservationStatus::Error,
            "官方 CLI 返回内容超过安全上限".into(),
        ));
    }
    let status = loop {
        if let Some(status) = crate::platform::usage_probe_exit(&mut child.child)
            .map_err(|_| (ObservationStatus::Error, "无法获取查询结果".into()))?
        {
            break status;
        }
        if Instant::now() >= deadline {
            return Err((ObservationStatus::Error, "官方 CLI 查询结束超时".into()));
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    if !status {
        return Err((
            ObservationStatus::NotAuthenticated,
            "官方 CLI 查询失败，请检查登录状态或查询权限".into(),
        ));
    }
    Ok(String::from_utf8_lossy(&output).into_owned())
}

pub(super) fn capture_query(
    provider: &Provider,
    account: &UsageAccountConfig,
    args: &[&str],
    timeout: Duration,
) -> Result<String, QueryError> {
    let started = Instant::now();
    let help = capture(
        provider,
        account,
        &["--help"],
        timeout.min(Duration::from_secs(3)),
    )?;
    let Some(command) = args.first() else {
        return Err((ObservationStatus::Unsupported, "未配置官方子命令".into()));
    };
    let advertised = help.lines().any(|line| {
        let line = line.trim();
        let line = line
            .strip_prefix(provider.command)
            .unwrap_or(line)
            .trim_start();
        line.strip_prefix(command)
            .is_some_and(|tail| tail.is_empty() || tail.starts_with(char::is_whitespace))
    });
    if !advertised {
        return Err((
            ObservationStatus::Unsupported,
            "当前 CLI 帮助中没有此官方用量子命令，请更新 CLI 或使用官方页面".into(),
        ));
    }
    capture(
        provider,
        account,
        args,
        timeout
            .saturating_sub(started.elapsed())
            .max(Duration::from_millis(1)),
    )
}

struct Rpc {
    child: ChildGuard,
    lines: mpsc::Receiver<Value>,
    deadline: Instant,
}

impl Rpc {
    fn send(&mut self, value: &Value) -> Result<(), QueryError> {
        let input = self
            .child
            .child
            .stdin
            .as_mut()
            .ok_or_else(|| (ObservationStatus::Error, "RPC 输入已关闭".into()))?;
        writeln!(input, "{value}")
            .and_then(|_| input.flush())
            .map_err(|_| (ObservationStatus::Error, "RPC 输入失败".into()))
    }

    fn receive(&self, id: u64) -> Result<Value, QueryError> {
        loop {
            let remaining = self.deadline.saturating_duration_since(Instant::now());
            let value = self
                .lines
                .recv_timeout(remaining)
                .map_err(|_| (ObservationStatus::Error, "Codex 账号查询超时".into()))?;
            if value.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if value.get("error").is_some() {
                return Err((
                    ObservationStatus::Unsupported,
                    "当前 Codex 版本或登录方式不支持此账号查询".into(),
                ));
            }
            return Ok(value.get("result").cloned().unwrap_or(Value::Null));
        }
    }
}

pub(super) fn codex(
    provider: &Provider,
    account: &UsageAccountConfig,
    timeout: Duration,
) -> Result<(Value, Value), QueryError> {
    let directory = ProbeDirectory::new()?;
    let child = command(provider, account, &directory)?
        .args(["app-server", "--listen", "stdio://", "-c", "mcp_servers={}"])
        .stdin(Stdio::piped())
        .spawn()
        .map_err(spawn_error)?;
    let mut child = ChildGuard::new(child)?;
    let stdout = child
        .child
        .stdout
        .take()
        .ok_or_else(|| (ObservationStatus::Error, "RPC 输出不可用".into()))?;
    let (sender, lines) = mpsc::sync_channel(32);
    let reader = std::thread::spawn(move || {
        let mut reader = io::BufReader::new(stdout);
        let mut total = 0_usize;
        loop {
            let mut bytes = Vec::new();
            let read = (&mut reader)
                .take((MAX_OUTPUT.saturating_sub(total) + 1) as u64)
                .read_until(b'\n', &mut bytes);
            let Ok(length) = read else {
                break;
            };
            total = total.saturating_add(length);
            if length == 0 || total > MAX_OUTPUT {
                break;
            }
            if let Ok(value) = serde_json::from_slice(&bytes) {
                if sender.try_send(value).is_err() {
                    break;
                }
            }
        }
    });
    child.readers.push(reader);
    let mut rpc = Rpc {
        child,
        lines,
        deadline: Instant::now() + timeout,
    };
    rpc.send(&json!({"id":1,"method":"initialize","params":{"clientInfo":{"name":"herdr_usage","version":env!("CARGO_PKG_VERSION")}}}))?;
    rpc.receive(1)?;
    rpc.send(&json!({"method":"initialized","params":{}}))?;
    rpc.send(&json!({"id":2,"method":"account/read","params":{"refreshToken":false}}))?;
    let identity = rpc.receive(2)?;
    if identity.get("account").is_none_or(Value::is_null) {
        return Err((
            ObservationStatus::NotAuthenticated,
            "请先使用 Codex 官方登录流程".into(),
        ));
    }
    rpc.send(&json!({"id":3,"method":"account/rateLimits/read"}))?;
    let mut limits = rpc.receive(3)?;
    // 新版支持更细的附加用量；老版本只保留已经取得的窗口数据。
    rpc.send(&json!({"id":4,"method":"account/usage/read"}))?;
    if let Ok(usage) = rpc.receive(4) {
        limits["officialUsage"] = usage;
    }
    Ok((identity, limits))
}

#[derive(serde::Serialize, serde::Deserialize)]
struct ProbeRequest {
    account: UsageAccountConfig,
    timeout_ms: u64,
}

pub(crate) fn run_probe_helper() -> io::Result<()> {
    let mut bytes = Vec::new();
    std::io::stdin()
        .take(MAX_OUTPUT as u64 + 1)
        .read_to_end(&mut bytes)?;
    let params: ProbeRequest =
        serde_json::from_slice(&bytes).map_err(|_| io::Error::other("查询辅助进程参数无效"))?;
    let provider = super::registry::provider(&params.account.agent)
        .ok_or_else(|| io::Error::other("未知官方 CLI"))?;
    let super::registry::Query::Interactive(query) = provider.query else {
        return Err(io::Error::other("此厂商不使用交互查询"));
    };
    let result = interactive_direct(
        provider,
        &params.account,
        query,
        Duration::from_millis(params.timeout_ms.clamp(1000, 30_000)),
    );
    serde_json::to_writer(std::io::stdout(), &result).map_err(io::Error::other)
}

pub(super) fn interactive(
    provider: &Provider,
    account: &UsageAccountConfig,
    query: &str,
    timeout: Duration,
) -> Result<String, QueryError> {
    if !crate::platform::usage_probe_needs_job_helper() {
        return interactive_direct(provider, account, query, timeout);
    }
    // ConPTY 内的 CLI 由提前加入 Windows Job 的辅助进程启动；根进程先退出也不会失去后代。
    let directory = ProbeDirectory::new()?;
    let executable = std::env::current_exe()
        .map_err(|_| (ObservationStatus::Error, "无法定位查询辅助进程".into()))?;
    // 重新建立命令，避免把 CLI 路径当成 herdr 的参数。
    let mut command = std::process::Command::new(executable);
    crate::platform::configure_usage_probe_command(&mut command);
    command
        .arg("--internal-usage-probe")
        .current_dir(&directory.0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    for variable in [
        "HERDR_ENV",
        "HERDR_SOCKET_PATH",
        "HERDR_CLIENT_SOCKET_PATH",
        "HERDR_PANE_ID",
        "HERDR_TERMINAL_ID",
        "HERDR_WORKSPACE_ID",
        "HERDR_TAB_ID",
        "HERDR_SESSION",
    ] {
        command.env_remove(variable);
    }
    let mut child = ChildGuard::new(command.spawn().map_err(spawn_error)?)?;
    if let Some(mut input) = child.child.stdin.take() {
        serde_json::to_writer(
            &mut input,
            &ProbeRequest {
                account: account.clone(),
                timeout_ms: timeout.as_millis().min(30_000) as u64,
            },
        )
        .map_err(|_| (ObservationStatus::Error, "无法配置隔离查询".into()))?;
        input
            .flush()
            .map_err(|_| (ObservationStatus::Error, "无法发送隔离查询".into()))?;
    }
    let stdout = child
        .child
        .stdout
        .take()
        .ok_or_else(|| (ObservationStatus::Error, "辅助查询输出不可用".into()))?;
    let (sender, receiver) = mpsc::sync_channel(1);
    child.readers.push(std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdout
            .take(MAX_OUTPUT as u64 + 1)
            .read_to_end(&mut bytes)
            .map(|_| bytes);
        let _ = sender.try_send(result);
    }));
    let bytes = receiver
        .recv_timeout(timeout + Duration::from_secs(2))
        .map_err(|_| (ObservationStatus::Error, "隔离 CLI 查询超时".into()))?
        .map_err(|_| (ObservationStatus::Error, "无法读取隔离查询".into()))?;
    if bytes.len() > MAX_OUTPUT {
        return Err((ObservationStatus::Error, "隔离查询结果过大".into()));
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| (ObservationStatus::Error, "隔离查询结果无效".into()))?
}

fn help_lists(screen: &str, query: &str) -> bool {
    let command = query.split_whitespace().next().unwrap_or(query);
    screen.lines().any(|line| {
        let line = line.trim_start_matches(|ch: char| ch.is_whitespace() || "│┃├└─•*".contains(ch));
        line.strip_prefix(command)
            .is_some_and(|rest| rest.starts_with(char::is_whitespace) || rest.starts_with('—'))
    })
}

fn probe_ready(
    agent: &str,
    terminal: &crate::ghostty::Terminal,
    render: &mut crate::ghostty::RenderState,
    detected: bool,
) -> bool {
    if render.update(terminal).is_err() {
        return false;
    }
    let Ok(cursor) = render.cursor() else {
        return false;
    };
    let Some(position) = cursor.viewport.filter(|_| cursor.visible) else {
        return false;
    };
    let Ok(row) = terminal.read_text_viewport(
        (0, u32::from(position.y)),
        (179, u32::from(position.y)),
        true,
    ) else {
        return false;
    };
    let plain = row.trim_matches(|ch: char| ch.is_whitespace() || "│┃".contains(ch));
    match agent {
        "gemini" => {
            plain.starts_with('>')
                && plain.contains("Type your message or @path/to/file")
                && position.x < 10
        }
        "hermes" | "grok" => {
            plain.ends_with('❯')
                && plain.chars().count() <= 64
                && usize::from(position.x) <= plain.chars().count() + 3
        }
        "claude" => detected && plain == "❯" && position.x < 8,
        _ => false,
    }
}

fn interactive_direct(
    provider: &Provider,
    account: &UsageAccountConfig,
    query: &str,
    timeout: Duration,
) -> Result<String, QueryError> {
    if provider.agent == "qodercli" {
        return Err((ObservationStatus::Unsupported, "Qoder 官方提供 /usage，但没有稳定的自动输入就绪契约；请在 CLI 中查看或绑定实际计费 provider".into()));
    }
    let directory = ProbeDirectory::new()?;
    if account.profile_dir.is_some() && profile_variable(provider.agent).is_none() {
        return Err((
            ObservationStatus::Unsupported,
            "尚无此 CLI 的安全 profile 查询方式".into(),
        ));
    }
    let pty = portable_pty::native_pty_system()
        .openpty(portable_pty::PtySize {
            rows: 80,
            cols: 180,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|_| (ObservationStatus::Unavailable, "无法创建隔离终端".into()))?;
    let mut builder = portable_pty::CommandBuilder::new(provider.command);
    if provider.agent == "hermes" {
        builder.arg("--cli");
    }
    builder.cwd(&directory.0);
    builder.env("TERM", "xterm-256color");
    builder.env("HERDR_USAGE_PROBE", "1");
    for variable in [
        "HERDR_ENV",
        "HERDR_SOCKET_PATH",
        "HERDR_CLIENT_SOCKET_PATH",
        "HERDR_PANE_ID",
        "HERDR_TERMINAL_ID",
        "HERDR_WORKSPACE_ID",
        "HERDR_TAB_ID",
        "HERDR_SESSION",
    ] {
        builder.env_remove(variable);
    }
    if let Some((variable, path)) =
        profile_variable(provider.agent).zip(account.profile_dir.as_ref())
    {
        builder.env(variable, path);
    }
    let mut child = pty.slave.spawn_command(builder).map_err(|_| {
        (
            ObservationStatus::Unavailable,
            "无法启动隔离 CLI，请检查安装和平台支持".into(),
        )
    })?;
    drop(pty.slave);
    let mut reader_handle = None;
    let result = (|| {
        let writer = pty
            .master
            .take_writer()
            .map_err(|_| (ObservationStatus::Error, "隔离终端输入不可用".into()))?;
        let writer = Arc::new(Mutex::new(writer));
        let mut reader = pty
            .master
            .try_clone_reader()
            .map_err(|_| (ObservationStatus::Error, "隔离终端输出不可用".into()))?;
        let (sender, receiver) = mpsc::sync_channel(32);
        reader_handle = Some(std::thread::spawn(move || {
            let mut total = 0_usize;
            loop {
                let mut bytes = vec![0_u8; 8192];
                let Ok(read) = reader.read(&mut bytes) else {
                    break;
                };
                if read == 0 {
                    break;
                }
                total = total.saturating_add(read);
                if total > MAX_OUTPUT {
                    break;
                }
                bytes.truncate(read);
                if sender.try_send(bytes).is_err() {
                    break;
                }
            }
        }));
        let mut terminal = crate::ghostty::Terminal::new(180, 80, MAX_OUTPUT)
            .map_err(|_| (ObservationStatus::Error, "无法读取隔离 CLI 画面".into()))?;
        let terminal_writer = writer.clone();
        terminal
            .set_write_pty_callback(move |bytes| {
                if let Ok(mut writer) = terminal_writer.lock() {
                    let _ = writer.write_all(bytes);
                    let _ = writer.flush();
                }
            })
            .map_err(|_| (ObservationStatus::Error, "隔离终端协议初始化失败".into()))?;
        let deadline = Instant::now() + timeout;
        let agent = crate::detect::parse_agent_label(provider.agent);
        let mut render_state = crate::ghostty::RenderState::new()
            .map_err(|_| (ObservationStatus::Error, "隔离终端游标不可用".into()))?;
        let mut stage = 0;
        let mut last_output = Instant::now();
        let mut screen = String::new();
        loop {
            if Instant::now() >= deadline {
                return Err((
                    ObservationStatus::Error,
                    "官方用量查询超时；未发送模型任务".into(),
                ));
            }
            match receiver.recv_timeout(Duration::from_millis(100)) {
                Ok(bytes) => {
                    terminal.write(&bytes);
                    screen = terminal
                        .read_text_viewport((0, 0), (179, 79), true)
                        .unwrap_or_default();
                    last_output = Instant::now();
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err((ObservationStatus::Unavailable, "官方 CLI 已退出查询".into()))
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            let lower = screen.to_lowercase();
            if [
                "sign in",
                "log in",
                "login required",
                "trust this",
                "trust the files",
                "permission required",
                "选择登录",
                "请登录",
            ]
            .iter()
            .any(|text| lower.contains(text))
            {
                return Err((
                    ObservationStatus::NotAuthenticated,
                    "官方 CLI 需要登录或信任确认，请先在正常会话完成设置".into(),
                ));
            }
            let detection = crate::detect::detect_agent(agent, &screen);
            let ready = probe_ready(
                provider.agent,
                &terminal,
                &mut render_state,
                detection.visible_idle,
            ) && !detection.visible_blocker
                && !detection.visible_working;
            if stage == 0 && ready && last_output.elapsed() >= Duration::from_millis(200) {
                if let Ok(mut input) = writer.lock() {
                    let _ = input.write_all(if provider.agent == "kiro" {
                        b"/help --legacy\r"
                    } else {
                        b"/help\r"
                    });
                    let _ = input.flush();
                }
                stage = 1;
                screen.clear();
                last_output = Instant::now();
            } else if stage == 1
                && help_lists(&screen, query)
                && last_output.elapsed() >= Duration::from_millis(200)
            {
                if let Ok(mut input) = writer.lock() {
                    let _ = input.write_all(b"\x1b");
                    let _ = input.flush();
                }
                stage = 2;
                last_output = Instant::now();
            } else if stage == 2 && ready && last_output.elapsed() >= Duration::from_millis(200) {
                if let Ok(mut input) = writer.lock() {
                    let _ = input.write_all(format!("{query}\r").as_bytes());
                    let _ = input.flush();
                }
                stage = 3;
                last_output = Instant::now();
                screen.clear();
            } else if stage == 3
                && last_output.elapsed() >= Duration::from_millis(700)
                && !super::parse::screen(&screen, provider.scope).is_empty()
            {
                return Ok(screen);
            }
        }
    })();
    crate::platform::terminate_usage_pty(&mut *child);
    drop(pty.master);
    if let Some(reader) = reader_handle {
        let _ = reader.join();
    }
    result
}

pub(super) fn kimi(
    provider: &Provider,
    account: &UsageAccountConfig,
    timeout: Duration,
) -> Result<(Value, Value), QueryError> {
    let directory = ProbeDirectory::new()?;
    let deadline = Instant::now() + timeout;
    let listener =
        std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).map_err(|_| {
            (
                ObservationStatus::Unavailable,
                "无法分配本地查询端口".into(),
            )
        })?;
    let port = listener
        .local_addr()
        .map_err(|_| (ObservationStatus::Error, "无法读取本地端口".into()))?
        .port();
    drop(listener);
    let child = command(provider, account, &directory)?
        .args([
            "web",
            "--host",
            "127.0.0.1",
            "--port",
            &port.to_string(),
            "--no-open",
        ])
        .stderr(Stdio::piped())
        .spawn()
        .map_err(spawn_error)?;
    let mut child = ChildGuard::new(child)?;
    let (sender, receiver) = mpsc::sync_channel(32);
    let mut pipes: Vec<Box<dyn Read + Send>> = Vec::new();
    if let Some(stdout) = child.child.stdout.take() {
        pipes.push(Box::new(stdout));
    }
    if let Some(stderr) = child.child.stderr.take() {
        pipes.push(Box::new(stderr));
    }
    for mut pipe in pipes {
        let sender = sender.clone();
        child.readers.push(std::thread::spawn(move || {
            let mut total = 0;
            loop {
                let mut bytes = vec![0; 4096];
                let Ok(length) = pipe.read(&mut bytes) else {
                    break;
                };
                total += length;
                if length == 0 || total > MAX_OUTPUT {
                    break;
                }
                bytes.truncate(length);
                if sender.try_send(bytes).is_err() {
                    break;
                }
            }
        }));
    }
    drop(sender);
    let pattern = regex::Regex::new(r"http://127\.0\.0\.1:(\d+)/#token=([A-Za-z0-9_-]+)")
        .map_err(|_| (ObservationStatus::Error, "本地服务地址解析失败".into()))?;
    let mut banner = String::new();
    let (base, token) = loop {
        let bytes = receiver
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|_| {
                (
                    ObservationStatus::Unavailable,
                    "Kimi 本地服务未就绪；请确认版本支持 kimi web".into(),
                )
            })?;
        banner.push_str(&String::from_utf8_lossy(&bytes));
        if banner.len() > MAX_OUTPUT {
            return Err((ObservationStatus::Error, "Kimi 启动输出过大".into()));
        }
        if let Some(found) = pattern.captures(&banner) {
            break (
                format!("http://127.0.0.1:{}", &found[1]),
                found[2].to_owned(),
            );
        }
    };
    // 临时访问令牌仅留在此函数内，不写入配置、日志、报告或 URL。
    let client = reqwest::blocking::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| (ObservationStatus::Error, "无法连接本地官方服务".into()))?;
    let read = |path: &str| -> Result<Value, QueryError> {
        let response = client
            .get(format!("{base}{path}"))
            .bearer_auth(&token)
            .timeout(
                deadline
                    .saturating_duration_since(Instant::now())
                    .max(Duration::from_millis(1)),
            )
            .send()
            .map_err(|_| (ObservationStatus::Error, "Kimi 官方查询连接失败".into()))?;
        if !response.status().is_success() {
            return Err((
                ObservationStatus::NotAuthenticated,
                "Kimi 官方查询需要有效登录或新版 CLI".into(),
            ));
        }
        let mut bytes = Vec::new();
        response
            .take(MAX_OUTPUT as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| (ObservationStatus::Error, "Kimi 官方响应读取失败".into()))?;
        if bytes.len() > MAX_OUTPUT {
            return Err((ObservationStatus::Error, "Kimi 响应过大".into()));
        }
        let value: Value = serde_json::from_slice(&bytes).map_err(|_| {
            (
                ObservationStatus::Unsupported,
                "当前 Kimi 接口格式不受支持".into(),
            )
        })?;
        if value.get("code").and_then(Value::as_i64) != Some(0)
            || value.pointer("/data/kind").and_then(Value::as_str) == Some("error")
        {
            return Err((
                ObservationStatus::NotAuthenticated,
                "Kimi 官方账号查询未成功，请检查登录状态".into(),
            ));
        }
        Ok(value)
    };
    let identity = read("/api/v1/oauth/userinfo")?;
    let usage = read("/api/v1/oauth/usage")?;
    Ok((identity, usage))
}
