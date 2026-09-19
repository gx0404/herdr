//! 有界官方 CLI 查询。辅助进程没有工作 pane 身份，也不会发送模型任务。

use std::io::{self, BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use super::parse::{self, ProbeBlocker};
use super::registry::Provider;
use crate::api::schema::ObservationStatus;
use crate::config::UsageAccountConfig;
use serde_json::{json, Value};

pub(super) type QueryError = (ObservationStatus, String);
const MAX_OUTPUT: usize = 2 * 1024 * 1024;
/// 非零退出时保留并记进 debug 日志的 stderr 尾部长度。
const STDERR_TAIL: usize = 2 * 1024;
/// 进入状态文案的 stderr 摘要长度上限（字符）。
const SUMMARY_CHARS: usize = 120;

/// 交互探测失败：状态与文案之外还带「停在目录信任对话」的判定，供缓存条目暴露
/// `trust_required`。也是 Windows 辅助进程回传的 JSON 形态。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub(super) struct InteractiveError {
    pub status: ObservationStatus,
    pub message: String,
    #[serde(default)]
    pub trust_required: bool,
}

impl From<QueryError> for InteractiveError {
    fn from((status, message): QueryError) -> Self {
        Self {
            status,
            message,
            trust_required: false,
        }
    }
}

/// 一次有界的非交互捕获：stdout 全量（≤ `MAX_OUTPUT`）、stderr 全量（≤ `MAX_OUTPUT`）与
/// 退出是否成功；不做任何状态分类，交给调用方「先看内容再看退出码」。
pub(super) struct Captured {
    pub success: bool,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

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

/// 探测工作目录：默认是一次性临时目录（Drop 时删除）；claude 的交互探测用稳定目录
/// （`stable`），Claude Code 的目录信任按路径持久化在用户自己的状态文件里，固定目录让用户
/// 只需在 CLI 里确认一次即可复用，Drop 时保留。
struct ProbeDirectory {
    path: PathBuf,
    persistent: bool,
}
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
        Ok(Self {
            path,
            persistent: false,
        })
    }

    fn stable(path: &Path) -> Result<Self, QueryError> {
        std::fs::create_dir_all(path)
            .map_err(|_| (ObservationStatus::Error, "无法创建稳定探测目录".into()))?;
        Ok(Self {
            path: path.to_path_buf(),
            persistent: true,
        })
    }
}
impl Drop for ProbeDirectory {
    fn drop(&mut self) {
        if !self.persistent {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

/// claude 交互探测的稳定目录：`<state_dir>/account-usage/probe/<账号 id>`（id 只保留字母
/// 数字与 `-_`）。herdr 不会代为应答信任对话，也不写用户的官方状态文件；文案里给出这个
/// 路径，用户在自己的 CLI 中确认一次即可。
pub(super) fn stable_probe_dir(account: &UsageAccountConfig) -> PathBuf {
    let mut safe = account
        .id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>();
    if safe.is_empty() {
        safe.push_str("default");
    }
    crate::config::state_dir()
        .join("account-usage")
        .join("probe")
        .join(safe)
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

/// Layout binaries are npm shims (`#!/usr/bin/env node`); prepending the
/// layout's bin dir lets the shebang interpreter resolve even when the
/// detached server PATH cannot see the version-manager directory.
fn prepend_layout_path(
    existing: &std::ffi::OsStr,
    dir: &std::path::Path,
) -> Option<std::ffi::OsString> {
    std::env::join_paths(std::iter::once(dir.to_path_buf()).chain(std::env::split_paths(existing)))
        .ok()
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
    // Detached servers may not inherit version-manager PATHs; fall back to
    // the layout-resolved binary for CLIs installed outside PATH.
    let layout_fallback = if crate::integration::command_available(provider.command) {
        None
    } else if provider.command == "codex" {
        crate::integration::codex_layout_binary_path()
    } else {
        None
    };
    let program = match &layout_fallback {
        Some(path) => std::borrow::Cow::Owned(path.to_string_lossy().into_owned()),
        None => std::borrow::Cow::Borrowed(provider.command),
    };
    let mut command = crate::noninteractive_process::command(program.as_ref());
    if let Some(dir) = layout_fallback.as_ref().and_then(|path| path.parent()) {
        if let Some(prefixed) =
            prepend_layout_path(&std::env::var_os("PATH").unwrap_or_default(), dir)
        {
            command.env("PATH", prefixed);
        }
    }
    crate::platform::configure_usage_probe_command(&mut command);
    command
        .current_dir(&directory.path)
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
    capture_output(provider, account, args, timeout, false)
}

/// 启动非交互查询并读完 stdout / stderr 直到子进程退出。stderr 由独立线程排空：不排空时
/// 话多的 CLI 会卡在管道上永不退出；两路都有 `MAX_OUTPUT` 上限。
fn capture_raw(
    provider: &Provider,
    account: &UsageAccountConfig,
    args: &[&str],
    timeout: Duration,
) -> Result<Captured, QueryError> {
    let deadline = Instant::now() + timeout;
    let directory = ProbeDirectory::new()?;
    let mut configured = command(provider, account, &directory)?;
    configured.stderr(Stdio::piped());
    let mut child = ChildGuard::new(configured.args(args).spawn().map_err(spawn_error)?)?;
    let stdout = child
        .child
        .stdout
        .take()
        .ok_or_else(|| (ObservationStatus::Error, "查询输出不可用".into()))?;
    let (sender, receiver) = mpsc::sync_channel(1);
    child.readers.push(std::thread::spawn(move || {
        let mut output = Vec::new();
        let result = stdout
            .take((MAX_OUTPUT + 1) as u64)
            .read_to_end(&mut output)
            .map(|_| output);
        let _ = sender.send(result);
    }));
    let (stderr_sender, stderr_receiver) = mpsc::sync_channel(1);
    match child.child.stderr.take() {
        // 不放进 `readers`：数据经 channel 取回，线程本身不 join——孙进程继承并持有 stderr
        // 管道时 `read_to_end` 不会返回，join 会把整条查询工作线程挂住。
        Some(stderr) => {
            std::thread::spawn(move || {
                let mut output = Vec::new();
                let _ = stderr
                    .take((MAX_OUTPUT + 1) as u64)
                    .read_to_end(&mut output);
                let _ = stderr_sender.try_send(output);
            });
        }
        None => {
            let _ = stderr_sender.try_send(Vec::new());
        }
    }
    let stdout = receiver
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(|_| (ObservationStatus::Error, "官方 CLI 查询超时".into()))?
        .map_err(|_| (ObservationStatus::Error, "无法读取官方 CLI 输出".into()))?;
    if stdout.len() > MAX_OUTPUT {
        return Err((
            ObservationStatus::Error,
            "官方 CLI 返回内容超过安全上限".into(),
        ));
    }
    let success = loop {
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
    // 子进程已退出，stderr 很快到 EOF；孙进程仍持有管道时最多等 500 ms 后按空处理，
    // 读取线程则被放弃（见上），不会拖住后续探测。
    let mut stderr = stderr_receiver
        .recv_timeout(Duration::from_millis(500))
        .unwrap_or_default();
    stderr.truncate(MAX_OUTPUT);
    Ok(Captured {
        success,
        stdout,
        stderr,
    })
}

/// `merge_stderr` also collects stderr into the output — yargs-family CLIs
/// (opencode) print `--help` to stderr, so only the subcommand precheck
/// uses it; the real query keeps stderr out of the parsed payload.
pub(super) fn capture_output(
    provider: &Provider,
    account: &UsageAccountConfig,
    args: &[&str],
    timeout: Duration,
    merge_stderr: bool,
) -> Result<String, QueryError> {
    let Captured {
        success,
        mut stdout,
        stderr,
    } = capture_raw(provider, account, args, timeout)?;
    if !success {
        return Err(classify_failure(provider, &stdout, &stderr));
    }
    if merge_stderr {
        stdout.extend_from_slice(&stderr);
    }
    Ok(String::from_utf8_lossy(&stdout).into_owned())
}

/// Claude 非交互登录预检：`claude auth status --json`，只读地复用真实登录态。分类见
/// `classify_auth_status`。
pub(super) fn claude_auth_status(
    provider: &Provider,
    account: &UsageAccountConfig,
    timeout: Duration,
) -> Result<Option<parse::ClaudeAuthStatus>, QueryError> {
    let captured = capture_raw(provider, account, &["auth", "status", "--json"], timeout)?;
    classify_auth_status(provider, &captured)
}

/// `auth status --json` 输出的分类——先看内容再看退出码：stdout 解析出 JSON（未登录时 CLI
/// 退出码为 1 但 JSON 仍合法）就是结论；否则 stderr 是用法错误（旧版 CLI 没有该子命令 /
/// flag）→ 终态 `Unsupported`；有登录失败证据 → `NotAuthenticated`；其余「登录态未知」返回
/// `Ok(None)`，不作终态——claude 的主路径（statusline 回调）本就不依赖它。
pub(super) fn classify_auth_status(
    provider: &Provider,
    captured: &Captured,
) -> Result<Option<parse::ClaudeAuthStatus>, QueryError> {
    if let Some(status) = parse::claude_auth_status(&String::from_utf8_lossy(&captured.stdout)) {
        return Ok(Some(status));
    }
    let stderr_raw = String::from_utf8_lossy(tail_bytes(&captured.stderr, STDERR_TAIL));
    let stdout_raw = String::from_utf8_lossy(tail_bytes(&captured.stdout, STDERR_TAIL));
    debug_stderr_tail(provider, &sanitize(&stderr_raw), "auth status 输出不可解析");
    if parse::cli_usage_error(&stderr_raw) {
        return Err((
            ObservationStatus::Unsupported,
            "此版本 Claude Code 没有 auth status --json；请升级 CLI 或启用官方 statusline 上报"
                .into(),
        ));
    }
    if parse::auth_evidence(&stderr_raw) || parse::auth_evidence(&stdout_raw) {
        return Err((
            ObservationStatus::NotAuthenticated,
            "官方 CLI 报告未登录，请先在正常会话完成登录".into(),
        ));
    }
    Ok(None)
}

fn tail_bytes(bytes: &[u8], limit: usize) -> &[u8] {
    &bytes[bytes.len().saturating_sub(limit)..]
}

/// 脱敏后的 stderr 尾部只进 debug 日志。
fn debug_stderr_tail(provider: &Provider, sanitized_tail: &str, message: &str) {
    if !sanitized_tail.is_empty() {
        tracing::debug!(
            event = "account.probe.stderr",
            subsystem = "account_usage",
            outcome = "error",
            agent = provider.agent,
            stderr_tail = %sanitized_tail,
            "{message}"
        );
    }
}

/// 非零退出的分类——先看内容再看退出码：stdout / stderr 有登录失败证据才是 `NotAuthenticated`；
/// 两路都空白是「无机器可读输出」；其余归 transient `Error`。判定跑在未脱敏的原文上
/// （脱敏会把长令牌样式的串整段遮掉，证据可能就在其中）；只有 debug 日志与文案摘要用脱敏
/// 后的文本。
pub(super) fn classify_failure(provider: &Provider, stdout: &[u8], stderr: &[u8]) -> QueryError {
    let stderr_raw = String::from_utf8_lossy(tail_bytes(stderr, STDERR_TAIL));
    let stdout_raw = String::from_utf8_lossy(tail_bytes(stdout, STDERR_TAIL));
    let stderr_tail = sanitize(&stderr_raw);
    debug_stderr_tail(provider, &stderr_tail, "官方 CLI 非零退出");
    if stdout.iter().all(u8::is_ascii_whitespace) && stderr_tail.is_empty() {
        return (
            ObservationStatus::Unsupported,
            "此版本官方 CLI 未提供机器可读的用量输出，请升级 CLI 或查看官方页面".into(),
        );
    }
    if parse::auth_evidence(&stderr_raw) || parse::auth_evidence(&stdout_raw) {
        return (
            ObservationStatus::NotAuthenticated,
            "官方 CLI 报告未登录，请先在正常会话完成登录".into(),
        );
    }
    let summary = summary_line(&stderr_tail);
    let message = if summary.is_empty() {
        "官方 CLI 查询失败（退出码非零），稍后自动重试".into()
    } else {
        format!("官方 CLI 查询失败：{summary}")
    };
    (ObservationStatus::Error, message)
}

fn ansi_pattern() -> Option<&'static regex::Regex> {
    static PATTERN: std::sync::OnceLock<Option<regex::Regex>> = std::sync::OnceLock::new();
    PATTERN
        .get_or_init(|| {
            // CSI 序列、OSC 序列（BEL 或 ST 结尾）与其余单字节 ESC 序列。
            regex::Regex::new(
                r"\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b[@-Z\\-_]",
            )
            .ok()
        })
        .as_ref()
}

fn secret_patterns() -> &'static [regex::Regex] {
    static PATTERNS: std::sync::OnceLock<Vec<regex::Regex>> = std::sync::OnceLock::new();
    PATTERNS.get_or_init(|| {
        [
            // `Bearer xxx` / `token=xxx` 之类的显式凭据。
            r"(?i)\b(bearer|token|key|secret|password)([=: ]+)[A-Za-z0-9._\-]{8,}",
            // 令牌样式的长串（API key、JWT 片段）：≥32 且同时含字母与数字（见 `looks_like_token`），
            // 纯字母的长单词序列、`pkg-name-with-dashes` 之类的普通诊断信息不遮。
            r"[A-Za-z0-9_\-]{32,}",
            // 邮箱：账号身份不进日志与文案。
            r"[^\s@]+@[^\s@]+\.[^\s@]+",
        ]
        .iter()
        .filter_map(|pattern| regex::Regex::new(pattern).ok())
        .collect()
    })
}

/// 长串是否像令牌：同时含字母与数字（API key、JWT、hash 都满足；`unauthorized-request`
/// 这类连字符单词不满足）。
fn looks_like_token(text: &str) -> bool {
    text.chars().any(|ch| ch.is_ascii_digit()) && text.chars().any(|ch| ch.is_ascii_alphabetic())
}

/// 脱敏：去 ANSI 与控制字符、按行折叠空白并丢弃空行、遮蔽令牌样式的长串与邮箱。
/// 保留换行，便于取最后一行作为摘要。
pub(super) fn sanitize(text: &str) -> String {
    let stripped = match ansi_pattern() {
        Some(pattern) => pattern.replace_all(text, ""),
        None => std::borrow::Cow::Borrowed(text),
    };
    let mut lines = Vec::new();
    for line in stripped.lines() {
        // 制表符等空白也是控制字符，留给下面的空白折叠处理。
        let plain = line
            .chars()
            .filter(|ch| ch.is_whitespace() || !ch.is_control())
            .collect::<String>();
        let collapsed = plain.split_whitespace().collect::<Vec<_>>().join(" ");
        if !collapsed.is_empty() {
            lines.push(collapsed);
        }
    }
    let mut joined = lines.join("\n");
    for pattern in secret_patterns() {
        joined = pattern
            .replace_all(&joined, |captures: &regex::Captures| {
                let whole = captures.get(0).map_or("", |m| m.as_str());
                match captures.get(2) {
                    Some(separator) => format!(
                        "{}{}[redacted]",
                        captures.get(1).map_or("", |m| m.as_str()),
                        separator.as_str()
                    ),
                    // 无分隔符的裸长串：只遮令牌样式的；邮箱模式含 `@` 恒遮。
                    None if whole.contains('@') || looks_like_token(whole) => "[redacted]".into(),
                    None => whole.to_owned(),
                }
            })
            .into_owned();
    }
    joined
}

/// 已脱敏文本的最后一行（错误通常最后打印），截到 `SUMMARY_CHARS` 个字符。
fn summary_line(sanitized: &str) -> String {
    let Some(last) = sanitized.lines().next_back() else {
        return String::new();
    };
    if last.chars().count() <= SUMMARY_CHARS {
        return last.to_owned();
    }
    let mut cut = last.chars().take(SUMMARY_CHARS).collect::<String>();
    cut.push('…');
    cut
}

pub(super) fn capture_query(
    provider: &Provider,
    account: &UsageAccountConfig,
    args: &[&str],
    timeout: Duration,
) -> Result<String, QueryError> {
    let started = Instant::now();
    let help = capture_output(
        provider,
        account,
        &["--help"],
        timeout.min(Duration::from_secs(3)),
        true,
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

/// Windows 辅助进程的参数。`interactive_probe` 是 `[account_usage] interactive_probe` 的
/// 副本：claude 的交互探测在辅助进程侧也要过这道闸门，不只依赖调用方；旧 payload 缺省为
/// false。`probe_dir` 是 claude 的稳定探测目录。
#[derive(serde::Serialize, serde::Deserialize)]
struct ProbeRequest {
    account: UsageAccountConfig,
    timeout_ms: u64,
    #[serde(default)]
    interactive_probe: bool,
    #[serde(default)]
    probe_dir: Option<PathBuf>,
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
    let Some(query) = super::registry::interactive_fallback(provider) else {
        return Err(io::Error::other("此厂商不使用交互查询"));
    };
    if provider.agent == "claude" && !params.interactive_probe {
        return Err(io::Error::other(
            "claude 交互探测未在 account_usage.interactive_probe 中开启",
        ));
    }
    let result = interactive_direct(
        provider,
        &params.account,
        query,
        Duration::from_millis(params.timeout_ms.clamp(1000, 30_000)),
        params.probe_dir.as_deref(),
    );
    serde_json::to_writer(std::io::stdout(), &result).map_err(io::Error::other)
}

/// 交互探测：`stable_dir` 是 claude 的稳定探测目录（其它厂商为 `None`，用一次性临时目录）。
/// claude 的调用方只在 `interactive_probe` 开启且显式刷新时到达这里。
pub(super) fn interactive(
    provider: &Provider,
    account: &UsageAccountConfig,
    query: &str,
    timeout: Duration,
    stable_dir: Option<&Path>,
) -> Result<String, InteractiveError> {
    if !crate::platform::usage_probe_needs_job_helper() {
        return interactive_direct(provider, account, query, timeout, stable_dir);
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
        .current_dir(&directory.path)
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
                // 调用方已过 `interactive_probe && manual` 闸门；辅助进程侧再校验一次。
                interactive_probe: provider.agent != "claude" || stable_dir.is_some(),
                probe_dir: stable_dir.map(Path::to_path_buf),
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
        return Err((ObservationStatus::Error, "隔离查询结果过大".into()).into());
    }
    serde_json::from_slice::<Result<String, InteractiveError>>(&bytes)
        .map_err(|_| (ObservationStatus::Error, "隔离查询结果无效".into()))?
}

/// 阻塞对话的判定 → 探测错误：登录组是终态；信任组是可重试的 transient `Error` 并置
/// `trust_required`。herdr 不会替用户应答信任对话（那属代用户授权，且会写用户的官方状态
/// 文件）；有稳定探测目录时文案给出确切路径，用户在自己的 CLI 里确认一次即可复用。
pub(super) fn blocker_error(
    provider: &Provider,
    blocker: ProbeBlocker,
    stable_dir: Option<&Path>,
) -> InteractiveError {
    let statusline = matches!(provider.agent, "claude" | "antigravity");
    match blocker {
        ProbeBlocker::Trust => InteractiveError {
            status: ObservationStatus::Error,
            message: match stable_dir {
                Some(dir) => format!(
                    "需在 CLI 中确认目录信任：在终端运行 `cd {} && {}` 并选择「Yes, I trust this folder」一次；herdr 不会代为应答",
                    dir.display(),
                    provider.command
                ),
                None if statusline => "需在 CLI 中确认目录信任；herdr 不会代为应答。建议在 监控 → 设置 启用官方 statusline 上报获取用量".into(),
                None => "需在 CLI 中确认目录信任；herdr 不会代为应答，请先在正常会话完成一次确认".into(),
            },
            trust_required: true,
        },
        ProbeBlocker::SignIn => InteractiveError {
            status: ObservationStatus::NotAuthenticated,
            message: if statusline {
                "官方 CLI 需要登录；也可在 监控 → 设置 启用官方 statusline 上报获取用量".into()
            } else {
                "官方 CLI 需要登录，请先在正常会话完成登录".into()
            },
            trust_required: false,
        },
    }
}

/// 交互探测在截止时间前没等到就绪提示或用量输出：transient，不进终态集合。
fn probe_timeout() -> InteractiveError {
    InteractiveError {
        status: ObservationStatus::Error,
        message: "官方用量查询超时；未发送模型任务".into(),
        trust_required: false,
    }
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
    stable_dir: Option<&Path>,
) -> Result<String, InteractiveError> {
    if provider.agent == "qodercli" {
        return Err((ObservationStatus::Unsupported, "Qoder 官方提供 /usage，但没有稳定的自动输入就绪契约；请在 CLI 中查看或绑定实际计费 provider".into()).into());
    }
    let directory = match stable_dir {
        Some(path) => ProbeDirectory::stable(path)?,
        None => ProbeDirectory::new()?,
    };
    if account.profile_dir.is_some() && profile_variable(provider.agent).is_none() {
        return Err((
            ObservationStatus::Unsupported,
            "尚无此 CLI 的安全 profile 查询方式".into(),
        )
            .into());
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
    builder.cwd(&directory.path);
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
        InteractiveError::from((
            ObservationStatus::Unavailable,
            "无法启动隔离 CLI，请检查安装和平台支持".to_owned(),
        ))
    })?;
    drop(pty.slave);
    let mut reader_handle = None;
    let result = (|| -> Result<String, InteractiveError> {
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
                return Err(probe_timeout());
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
                    return Err(
                        (ObservationStatus::Unavailable, "官方 CLI 已退出查询".into()).into(),
                    )
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            // 登录 / 信任对话按行级前缀判定（`/help` 列表里的 `Sign in with…` 不算），
            // 信任对话不自动应答。
            if let Some(blocker) = parse::interactive_blocker(&screen) {
                return Err(blocker_error(provider, blocker, stable_dir));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_fallback_prefixes_its_bin_dir_onto_the_child_path() {
        let existing = std::env::join_paths(["/usr/bin", "/bin"]).expect("join fixture path");
        let dir = std::path::PathBuf::from("/home/x/.nvm/versions/node/v24/bin");
        let prefixed = prepend_layout_path(&existing, &dir).expect("prepend layout path");
        let split: Vec<_> = std::env::split_paths(&prefixed).collect();
        assert_eq!(
            split.first(),
            Some(&dir),
            "the layout bin dir must come first so `#!/usr/bin/env node` resolves"
        );
        assert_eq!(split[1], std::path::Path::new("/usr/bin"));
        assert_eq!(split[2], std::path::Path::new("/bin"));
    }

    fn claude() -> &'static Provider {
        super::super::registry::provider("claude").expect("claude 已登记")
    }

    #[test]
    fn nonzero_exit_without_auth_evidence_is_a_transient_error_with_a_sanitized_summary() {
        let stderr = b"\x1b[31mError:\x1b[0m Unknown argument: --json\nRun opencode stats --help\n";
        let (status, message) = classify_failure(claude(), b"", stderr);
        assert_eq!(
            status,
            ObservationStatus::Error,
            "无登录证据的失败可自动重试"
        );
        assert!(
            message.contains("Run opencode stats --help"),
            "最后一行进摘要：{message}"
        );
        assert!(!message.contains("\x1b"), "ANSI 已脱敏：{message}");

        // 只有退出码、两路都空白：无机器可读输出。
        let (status, _) = classify_failure(claude(), b"  \n", b"");
        assert_eq!(status, ObservationStatus::Unsupported);

        // 没有 stderr 但 stdout 有内容：不再一律判「需要登录」。
        let (status, _) = classify_failure(claude(), b"{\"partial\":true}", b"");
        assert_eq!(status, ObservationStatus::Error);
    }

    #[test]
    fn nonzero_exit_with_auth_evidence_stays_not_authenticated() {
        let stderr = b"Not logged in. Run claude auth login to authenticate.\n";
        let (status, _) = classify_failure(claude(), b"", stderr);
        assert_eq!(status, ObservationStatus::NotAuthenticated);
        let (status, _) = classify_failure(claude(), b"error: unauthorized (401)", b"");
        assert_eq!(status, ObservationStatus::NotAuthenticated);
        // 证据藏在会被脱敏整段遮掉的长串里：判定必须跑在原文上。
        let (status, message) = classify_failure(
            claude(),
            b"",
            b"Unauthorized-request-id-0123456789abcdef0123\n",
        );
        assert_eq!(status, ObservationStatus::NotAuthenticated, "{message}");
    }

    #[test]
    fn auth_status_classification_prefers_stdout_json_over_the_exit_code() {
        let captured = |success: bool, stdout: &str, stderr: &str| Captured {
            success,
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        };
        // 未登录：退出码 1 但 JSON 合法 → 以 JSON 为准，不是 classify_failure 的分支。
        let status = classify_auth_status(
            claude(),
            &captured(false, "{\"loggedIn\":false,\"authMethod\":\"none\"}\n", ""),
        )
        .expect("JSON 优先")
        .expect("已解析");
        assert!(!status.logged_in);
        // 已登录且退出码 0。
        let status = classify_auth_status(
            claude(),
            &captured(
                true,
                "{\"loggedIn\":true,\"email\":\"Me@Example.test\"}",
                "",
            ),
        )
        .expect("JSON 优先")
        .expect("已解析");
        assert!(status.logged_in);
        assert_eq!(status.email.as_deref(), Some("me@example.test"));
        // Ink 折行 + 前后夹杂提示行：仍解析。
        let wrapped = "Checking…\n{\n  \"loggedIn\": true,\n  \"configDirectory\": \"/very/long/pa\nth/.claude\"\n}\n";
        assert!(classify_auth_status(claude(), &captured(true, wrapped, ""))
            .expect("折行仍解析")
            .is_some_and(|status| status.logged_in));
        // 旧版 CLI 没有子命令 / flag：用法错误 → 终态 Unsupported，带升级 / 回调指引。
        let (status, message) = classify_auth_status(
            claude(),
            &captured(false, "", "error: unknown option '--json'\n"),
        )
        .expect_err("用法错误");
        assert_eq!(status, ObservationStatus::Unsupported);
        assert!(message.contains("statusline"), "{message}");
        let (status, _) = classify_auth_status(
            claude(),
            &captured(false, "", "error: unknown command 'status'\n"),
        )
        .expect_err("用法错误");
        assert_eq!(status, ObservationStatus::Unsupported);
        // 纯文本登录证据 → NotAuthenticated。
        let (status, _) = classify_auth_status(
            claude(),
            &captured(false, "", "Not logged in. Run claude auth login\n"),
        )
        .expect_err("登录证据");
        assert_eq!(status, ObservationStatus::NotAuthenticated);
        // 退出码 0 但不是 JSON、非零退出但无用法 / 登录证据、两路空白：登录态未知，不作终态。
        for (success, stdout, stderr) in [
            (true, "Logged in as someone\n", ""),
            (false, "", "segfault while starting\n"),
            (false, "", ""),
        ] {
            assert_eq!(
                classify_auth_status(claude(), &captured(success, stdout, stderr))
                    .expect("不作终态"),
                None,
                "{stdout:?} {stderr:?}"
            );
        }
    }

    #[test]
    fn sanitize_strips_escapes_and_redacts_credentials() {
        let raw = "\x1b[1mfailed\x1b[0m\r\n  Authorization: Bearer sk-ant-api03-abcdefghijklmnop \n\nuser me@example.test\ttoken=abcdefgh12345678\n\x07";
        let clean = sanitize(raw);
        assert_eq!(
            clean,
            "failed\nAuthorization: Bearer [redacted]\nuser [redacted] token=[redacted]"
        );
        // 令牌样式（字母 + 数字）的长串遮；纯字母 / 连字符单词序列与短 hash 保留。
        let token = format!("prefix {}", "a1".repeat(20));
        assert_eq!(sanitize(&token), "prefix [redacted]");
        let hash = format!("sha {}", "0123456789abcdef".repeat(4));
        assert_eq!(sanitize(&hash), "sha [redacted]", "hex hash 含字母与数字");
        let words = format!("prefix {}", "a".repeat(40));
        assert_eq!(sanitize(&words), words, "纯字母长串不是令牌");
        let diagnostic = "Unauthorized-request-id-was-not-a-token-string\n";
        assert_eq!(sanitize(diagnostic).trim(), diagnostic.trim());
        assert_eq!(sanitize(""), "");
        let wide = "x".repeat(SUMMARY_CHARS + 10);
        let summary = summary_line(&wide);
        assert_eq!(summary.chars().count(), SUMMARY_CHARS + 1);
        assert!(summary.ends_with('…'));
    }

    #[test]
    fn trust_screen_is_retryable_and_sign_in_screen_is_terminal() {
        let gemini = super::super::registry::provider("gemini").expect("gemini 已登记");
        let trust = parse::interactive_blocker(" ❯ 1. Yes, I trust this folder\n   2. No, exit\n")
            .expect("信任对话");
        let error = blocker_error(claude(), trust, None);
        assert_eq!(
            error.status,
            ObservationStatus::Error,
            "信任态可重试，不进终态集合"
        );
        assert!(error.trust_required);
        assert!(error.message.contains("需在 CLI 中确认目录信任"));
        assert!(error.message.contains("statusline"), "claude 给回调指引");
        assert!(!blocker_error(gemini, trust, None)
            .message
            .contains("statusline"));
        // 有稳定探测目录时给出可执行的确切路径：用户在自己的 CLI 里确认一次即可复用。
        let dir = std::path::Path::new("/state/account-usage/probe/claude-default");
        let error = blocker_error(claude(), trust, Some(dir));
        assert!(error.trust_required);
        assert!(
            error
                .message
                .contains("cd /state/account-usage/probe/claude-default && claude"),
            "{}",
            error.message
        );

        let sign_in = parse::interactive_blocker(
            " Select login method:\n ❯ 1. Claude account with subscription\n",
        )
        .expect("登录对话");
        let error = blocker_error(claude(), sign_in, None);
        assert_eq!(error.status, ObservationStatus::NotAuthenticated);
        assert!(!error.trust_required);

        // 空屏：没有阻塞对话，只能等到截止时间，归 transient。
        assert_eq!(parse::interactive_blocker(""), None);
        let timeout = probe_timeout();
        assert_eq!(timeout.status, ObservationStatus::Error);
        assert!(!timeout.trust_required);
    }

    #[test]
    fn helper_result_json_defaults_trust_required_for_older_payloads() {
        let text = serde_json::to_string(&Err::<String, _>(blocker_error(
            claude(),
            ProbeBlocker::Trust,
            None,
        )))
        .unwrap();
        let decoded: Result<String, InteractiveError> = serde_json::from_str(&text).unwrap();
        assert!(decoded.unwrap_err().trust_required);
        let legacy: Result<String, InteractiveError> =
            serde_json::from_str("{\"Err\":{\"status\":\"error\",\"message\":\"x\"}}").unwrap();
        assert!(!legacy.unwrap_err().trust_required);
        let ok: Result<String, InteractiveError> =
            serde_json::from_str("{\"Ok\":\"screen\"}").unwrap();
        assert_eq!(ok.unwrap(), "screen");
    }

    #[test]
    fn helper_request_defaults_gate_claude_off_for_older_payloads() {
        let legacy: ProbeRequest = serde_json::from_str(
            "{\"account\":{\"id\":\"claude:default\",\"agent\":\"claude\"},\"timeout_ms\":5000}",
        )
        .unwrap();
        assert!(
            !legacy.interactive_probe,
            "旧 payload：claude 交互探测默认关闭"
        );
        assert_eq!(legacy.probe_dir, None);
        let account = UsageAccountConfig {
            id: "claude:work/中文".into(),
            agent: "claude".into(),
            ..Default::default()
        };
        let dir = stable_probe_dir(&account);
        assert!(dir.ends_with(std::path::Path::new("account-usage/probe/claude-work---")));
        assert_eq!(
            stable_probe_dir(&UsageAccountConfig::default())
                .file_name()
                .and_then(|name| name.to_str()),
            Some("default")
        );
    }
}
