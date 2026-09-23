//! 有界官方 CLI 查询。辅助进程没有工作 pane 身份，也不会发送模型任务。

use std::collections::{HashMap, VecDeque};
use std::io::{self, BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::{mpsc, Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::parse::{self, ProbeBlocker};
use super::registry::{self, Provider};
use crate::api::schema::{ObservationStatus, UsageMetric};
use crate::config::UsageAccountConfig;
use crate::platform::UsageProbeExit;
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
/// 退出形态（退出码或终止信号）；不做任何状态分类，交给调用方「先看内容再看退出码」。
pub(super) struct Captured {
    pub exit: UsageProbeExit,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Captured {
    fn success(&self) -> bool {
        self.exit.success()
    }

    fn stdout_blank(&self) -> bool {
        self.stdout.iter().all(u8::is_ascii_whitespace)
    }

    /// 两路输出是否像命令行用法错误 / 帮助文本（flag 或子命令不受支持）。被信号终止的
    /// 进程不算——那是运行环境的问题。
    fn usage_error(&self) -> bool {
        if self.exit.signal.is_some() {
            return false;
        }
        [&self.stderr, &self.stdout].into_iter().any(|bytes| {
            let text = String::from_utf8_lossy(tail_bytes(bytes, STDERR_TAIL));
            parse::cli_usage_error(&text) || parse::cli_help_output(&text)
        })
    }
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

/// 启动非交互查询并读完 stdout / stderr 直到子进程退出。stderr 由独立线程排空：不排空时
/// 话多的 CLI 会卡在管道上永不退出；两路都有 `MAX_OUTPUT` 上限。`zcode_local` 也经这里起
/// 系统 `sqlite3`（`provider.command`），共用同一套进程隔离与环境清洗。
pub(super) fn capture_raw(
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
    let exit = loop {
        if let Some(exit) = crate::platform::usage_probe_exit(&mut child.child)
            .map_err(|_| (ObservationStatus::Error, "无法获取查询结果".into()))?
        {
            break exit;
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
        exit,
        stdout,
        stderr,
    })
}

/// 帮助文本捕获：yargs 族（opencode）把 `--help` 打到 stderr，所以 stdout 与 stderr 合并。
/// 判定见 `settle_help`。真实查询不走这里，stderr 不进入解析载荷。
fn capture_help(
    provider: &Provider,
    account: &UsageAccountConfig,
    args: &[&str],
    timeout: Duration,
) -> Result<String, QueryError> {
    let captured = capture_raw(provider, account, args, timeout)?;
    settle_help(provider, &captured)
}

/// `--help` 捕获的判定（纯函数）——先看内容再看退出码：合并后的文本是帮助
/// （`parse::cli_help_output`）就是结论，个别 CLI 的 `--help` 以非零退出也无妨；非零退出且
/// 内容不是帮助（node shim 的 warning / 异常栈、EACCES、代理报错）按 `classify_failure` 归为
/// 失败，不能冒充帮助被缓存成「子命令不存在」的终态；正常退出但内容不像帮助（oclif 之类
/// 无冒号标题的形态）原样返回，由 `cached_help` 决定不缓存；两路都空白按退出形态分类。
fn settle_help(provider: &Provider, captured: &Captured) -> Result<String, QueryError> {
    let mut merged = captured.stdout.clone();
    merged.extend_from_slice(&captured.stderr);
    if merged.iter().all(u8::is_ascii_whitespace) {
        if !captured.success() {
            return Err(classify_failure(provider, captured));
        }
        return Err((
            ObservationStatus::Unsupported,
            "官方 CLI 没有输出帮助文本，无法确认用量子命令".into(),
        ));
    }
    let text = String::from_utf8_lossy(&merged).into_owned();
    if !captured.success() && !parse::cli_help_output(&text) {
        return Err(classify_failure(provider, captured));
    }
    Ok(text)
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

/// 失败退出的分类——先看内容再看退出码：被信号终止是运行环境问题（transient）；两路都空白
/// 是「无机器可读输出」（版本旧）；用法错误 / 帮助文本是「flag 或子命令不受支持」——这条排在
/// 登录证据之前，与 `classify_auth_status` 一致：帮助文本里列出的 `auth login` 子命令不是
/// 未登录证据；然后 stdout / stderr 有登录失败证据才是 `NotAuthenticated`；其余归 transient
/// `Error` 并带退出码。判定跑在未脱敏的原文上（脱敏会把长令牌样式的串整段遮掉，证据可能
/// 就在其中）；只有 debug 日志与文案摘要用脱敏后的文本。
pub(super) fn classify_failure(provider: &Provider, captured: &Captured) -> QueryError {
    let stderr_raw = String::from_utf8_lossy(tail_bytes(&captured.stderr, STDERR_TAIL));
    let stdout_raw = String::from_utf8_lossy(tail_bytes(&captured.stdout, STDERR_TAIL));
    let stderr_tail = sanitize(&stderr_raw);
    debug_stderr_tail(provider, &stderr_tail, "官方 CLI 非零退出");
    if let Some(signal) = captured.exit.signal {
        return (
            ObservationStatus::Error,
            format!("官方 CLI 被信号 {signal} 终止（未得到结论），稍后自动重试"),
        );
    }
    let code = captured.exit.code.unwrap_or(-1);
    if captured.stdout_blank() && stderr_tail.is_empty() {
        return (
            ObservationStatus::Unsupported,
            format!("此版本官方 CLI 未提供机器可读的用量输出（退出码 {code}），请升级 CLI 或查看官方页面"),
        );
    }
    let summary = summary_line(&stderr_tail);
    if captured.usage_error() {
        let detail = if summary.is_empty() {
            String::new()
        } else {
            format!("：{summary}")
        };
        return (
            ObservationStatus::Unsupported,
            format!("当前版本官方 CLI 不支持此用量查询参数（用法错误{detail}），请升级 CLI 或查看官方页面"),
        );
    }
    if parse::auth_evidence(&stderr_raw) || parse::auth_evidence(&stdout_raw) {
        return (
            ObservationStatus::NotAuthenticated,
            "官方 CLI 报告未登录，请先在正常会话完成登录".into(),
        );
    }
    let message = if summary.is_empty() {
        format!("官方 CLI 查询失败（退出码 {code}），稍后自动重试")
    } else {
        format!("官方 CLI 查询失败（退出码 {code}）：{summary}")
    };
    (ObservationStatus::Error, message)
}

/// ANSI 转义序列（CSI / OSC / 单字节 ESC 序列）；`parse::clean_field` 与 `sanitize` 共用。
pub(super) fn ansi_pattern() -> Option<&'static regex::Regex> {
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

/// 帮助文本缓存的有效期；CLI 二进制的文件戳变化时立即失效。
const HELP_TTL: Duration = Duration::from_secs(60 * 60);
/// 每次帮助预检的时限上限：帮助文本应当立刻返回，慢就是 CLI 本身有问题。
const HELP_TIMEOUT: Duration = Duration::from_secs(3);

/// `(agent, 子命令)` → `(CLI 指纹, 缓存时间, 帮助文本)`。
type HelpCache =
    HashMap<(&'static str, Option<&'static str>), (Option<registry::FileStamp>, Instant, String)>;

fn help_cache() -> &'static Mutex<HelpCache> {
    static CACHE: OnceLock<Mutex<HelpCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 缓存命中判定（纯函数）：指纹相同且未超过 `HELP_TTL`。
fn help_cache_fresh(
    cached: &(Option<registry::FileStamp>, Instant, String),
    stamp: &Option<registry::FileStamp>,
    now: Instant,
) -> bool {
    cached.0 == *stamp && now.duration_since(cached.1) < HELP_TTL
}

/// `<cli> [sub] --help` 的文本：按 `(agent, sub)` 缓存，CLI 指纹（路径 + mtime + 大小）未变
/// 且未超过 `HELP_TTL` 时不再起进程——帮助文本只随二进制变化。只缓存确实是帮助文本
/// （`parse::cli_help_output`）的成功结果：失败不缓存，正常退出但形态不明的文本也不缓存，
/// 避免一次偶发失败被当成「子命令不存在」的终态重复一小时。
fn cached_help(
    provider: &Provider,
    account: &UsageAccountConfig,
    sub: Option<&'static str>,
    timeout: Duration,
) -> Result<String, QueryError> {
    let stamp = registry::command_stamp(provider.command);
    let key = (provider.agent, sub);
    let now = Instant::now();
    if let Ok(cache) = help_cache().lock() {
        if let Some(cached) = cache.get(&key) {
            if help_cache_fresh(cached, &stamp, now) {
                return Ok(cached.2.clone());
            }
        }
    }
    let args = sub.into_iter().chain(["--help"]).collect::<Vec<_>>();
    let text = capture_help(provider, account, &args, timeout)?;
    if parse::cli_help_output(&text) {
        if let Ok(mut cache) = help_cache().lock() {
            cache.insert(key, (stamp, now, text.clone()));
        }
    }
    Ok(text)
}

/// 顶层帮助文本是否列出了子命令（`<cli> <sub>` 或缩进的 `<sub>` 行）。
pub(super) fn help_lists_subcommand(help: &str, cli: &str, sub: &str) -> bool {
    help.lines().any(|line| {
        let line = line.trim();
        let line = line.strip_prefix(cli).unwrap_or(line).trim_start();
        line.strip_prefix(sub)
            .is_some_and(|tail| tail.is_empty() || tail.starts_with(char::is_whitespace))
    })
}

/// flag 行的 flag 列：行首到描述开始（第一处连续两个空格或制表符）为止。
fn flag_column(line: &str) -> &str {
    let end = line
        .find("  ")
        .into_iter()
        .chain(line.find('\t'))
        .min()
        .unwrap_or(line.len());
    &line[..end]
}

/// 子命令级帮助文本是否列出了 `args` 里的每个 `--flag`：只看 flag 行（以 `-x` / `--flag`
/// 开头）的 flag 列，描述文字里提到的 `--json`（「use --json for machine output」）不算；
/// 词边界：前后不能紧邻字母数字或 `-` / `_`，`--json` 不命中 `--jsonl` / `--json-lines`。
/// 没有 `--flag` 的形态恒为 true。
pub(super) fn help_lists_flags(help: &str, args: &[&str]) -> bool {
    args.iter().filter(|arg| arg.starts_with("--")).all(|flag| {
        help.lines()
            .map(str::trim_start)
            .filter(|line| parse::is_flag_line(line))
            .map(flag_column)
            .any(|column| {
                column.match_indices(*flag).any(|(at, _)| {
                    let before = column[..at].chars().next_back();
                    let after = column[at + flag.len()..].chars().next();
                    before.is_none_or(|ch| !(ch.is_alphanumeric() || ch == '-'))
                        && after.is_none_or(|ch| !(ch.is_alphanumeric() || matches!(ch, '-' | '_')))
                })
            })
    })
}

/// 非交互子命令查询的结果：解析出的指标与实际选用的参数形态（回退时与登记的 `args` 不同）。
#[derive(Debug)]
pub(super) struct QueryOutcome {
    pub metrics: Vec<UsageMetric>,
    pub args: &'static [&'static str],
}

/// 非交互查询的解析回调：`(stdout 文本, 实际选用的参数形态)` → 指标或解析器自己的结论。
type QueryParser<'a> =
    &'a mut dyn FnMut(&str, &'static [&'static str]) -> Result<Vec<UsageMetric>, QueryError>;

/// 「先看内容再看退出码」的判定结果。
enum Settled {
    Done(Result<Vec<UsageMetric>, QueryError>),
    /// 用法错误（flag / 子命令不受支持）：调用方尚有回退形态时再试一次。
    UsageError(QueryError),
    /// 进程自己以非零退出且没有产出指标（不是用法错误，也不是被信号终止）：首选形态是
    /// 数据库查询时，这可能只是旧库缺列之类的 schema 差异，回退形态走另一条读取路径，调用方
    /// 尚有回退形态时再试一次；没有回退形态时就是结论。
    Failed(QueryError),
}

/// 先看内容再看退出码（纯函数，便于表驱动测试）：stdout 能解析出指标就是结论——个别 CLI
/// 打完结果以非零退出；否则失败退出按 `classify_failure` 分类，其中用法错误与进程自身的
/// 失败退出分别标出以便回退（被信号终止是运行环境的问题，不回退）；正常退出但没有指标就把
/// 解析结果原样交给调用方（空 → 「无已验证字段」）。
fn settle_query(
    provider: &Provider,
    captured: &Captured,
    args: &'static [&'static str],
    parse: QueryParser<'_>,
) -> Settled {
    let parsed =
        (!captured.stdout_blank()).then(|| parse(&String::from_utf8_lossy(&captured.stdout), args));
    match parsed {
        Some(Ok(metrics)) if !metrics.is_empty() => Settled::Done(Ok(metrics)),
        parsed if !captured.success() => {
            let error = classify_failure(provider, captured);
            // 解析器自己的结论不比退出码更可信：进程都失败了。
            drop(parsed);
            if captured.usage_error() {
                Settled::UsageError(error)
            } else if captured.exit.signal.is_some() {
                Settled::Done(Err(error))
            } else {
                Settled::Failed(error)
            }
        }
        parsed => Settled::Done(parsed.unwrap_or_else(|| Ok(Vec::new()))),
    }
}

/// 子命令级帮助预检的最小时限：低于它就跳过预检，直接按登记形态运行。
const HELP_MIN_BUDGET: Duration = Duration::from_millis(250);

/// 顶层帮助预检的时限：`HELP_TIMEOUT` 封顶，且不超过总预算的一半——真正的查询至少保留一半
/// 预算（`probe_timeout_seconds` 取下限 5 s 且 npm shim 冷启动时，两次预检不能吃光预算）。
fn help_budget(timeout: Duration) -> Duration {
    HELP_TIMEOUT.min(timeout / 2)
}

/// 子命令级帮助预检可用的时限（纯函数）：`remaining` 扣除为查询保留的 `timeout / 2` 后再以
/// `help_budget` 封顶；不足 `HELP_MIN_BUDGET` 时返回 `None`（跳过预检）。
fn sub_help_budget(timeout: Duration, remaining: Duration) -> Option<Duration> {
    let budget = remaining
        .saturating_sub(timeout / 2)
        .min(help_budget(timeout));
    (budget >= HELP_MIN_BUDGET).then_some(budget)
}

/// 形态选择（纯函数）：顶层帮助未列出首选子命令时，回退形态的子命令若被列出就直接选回退
/// 形态（旧版 CLI 没有首选子命令），否则终态 `Unsupported`；有回退形态且子命令级帮助可用但
/// 未列出首选 `--flag` → 直接选回退形态、不再保留回退（不浪费一次必然失败的调用）；其余选
/// 首选形态并保留回退（拿不到子命令帮助不作结论，留给运行结果决定）。返回
/// `(选定形态, 仍可回退的形态)`。
fn choose_args(
    top_help: &str,
    sub_help: Option<&str>,
    command: &str,
    args: &'static [&'static str],
    fallback_args: Option<&'static [&'static str]>,
) -> Result<(&'static [&'static str], Option<&'static [&'static str]>), QueryError> {
    let Some(sub) = args.first().copied() else {
        return Err((ObservationStatus::Unsupported, "未配置官方子命令".into()));
    };
    if !help_lists_subcommand(top_help, command, sub) {
        let alternative = fallback_args.filter(|alternative| {
            alternative
                .first()
                .is_some_and(|other| help_lists_subcommand(top_help, command, other))
        });
        return alternative.map(|alternative| (alternative, None)).ok_or((
            ObservationStatus::Unsupported,
            "当前 CLI 帮助中没有此官方用量子命令，请更新 CLI 或使用官方页面".into(),
        ));
    }
    match (fallback_args, sub_help) {
        (Some(alternative), Some(help)) if !help_lists_flags(help, args) => Ok((alternative, None)),
        _ => Ok((args, fallback_args)),
    }
}

/// 非交互子命令查询。顺序：
/// 1. 顶层 `--help`（缓存）确认子命令存在（首选子命令缺失而回退子命令存在时直接用回退形态）；
/// 2. 有回退形态时再看子命令级 `--help`（预算允许时）：首选 `--flag` 未列出就直接用回退形态；
/// 3. 运行选定形态并 `settle_query`：用法错误或进程自身失败、且尚有回退形态时再试一次回退。
pub(super) fn capture_query(
    provider: &Provider,
    account: &UsageAccountConfig,
    args: &'static [&'static str],
    fallback_args: Option<&'static [&'static str]>,
    timeout: Duration,
    parse: impl FnMut(&str, &'static [&'static str]) -> Result<Vec<UsageMetric>, QueryError>,
) -> Result<QueryOutcome, QueryError> {
    run_query(
        provider,
        args,
        fallback_args,
        timeout,
        |sub, budget| cached_help(provider, account, sub, budget),
        |chosen, budget| capture_raw(provider, account, chosen, budget),
        parse,
    )
}

/// `capture_query` 的编排本体：帮助文本与进程捕获经 `help` / `run` 注入，便于不起进程地
/// 表驱动测试「预检选型 → 首选形态 → 一次回退」的状态机与 `QueryOutcome::args` 的取值。
/// 首选与回退都失败时返回最后一次尝试的错误（回退形态的失败更能反映当前可重试与否）。
fn run_query(
    provider: &Provider,
    args: &'static [&'static str],
    fallback_args: Option<&'static [&'static str]>,
    timeout: Duration,
    mut help: impl FnMut(Option<&'static str>, Duration) -> Result<String, QueryError>,
    mut run: impl FnMut(&'static [&'static str], Duration) -> Result<Captured, QueryError>,
    mut parse: impl FnMut(&str, &'static [&'static str]) -> Result<Vec<UsageMetric>, QueryError>,
) -> Result<QueryOutcome, QueryError> {
    let deadline = Instant::now() + timeout;
    let remaining = || {
        deadline
            .saturating_duration_since(Instant::now())
            .max(Duration::from_millis(1))
    };
    let Some(sub) = args.first().copied() else {
        return Err((ObservationStatus::Unsupported, "未配置官方子命令".into()));
    };
    let top_help = help(None, help_budget(timeout))?;
    let sub_help = fallback_args
        .and_then(|_| sub_help_budget(timeout, remaining()))
        .and_then(|budget| help(Some(sub), budget).ok());
    let (mut chosen, mut fallback) = choose_args(
        &top_help,
        sub_help.as_deref(),
        provider.command,
        args,
        fallback_args,
    )?;
    if chosen != args {
        tracing::debug!(
            event = "account.probe.fallback",
            subsystem = "account_usage",
            outcome = "fallback",
            agent = provider.agent,
            reason = "not_in_help",
            "帮助文本未列出首选子命令或 flag，改用回退形态"
        );
    }
    loop {
        let captured = run(chosen, remaining())?;
        let (error, reason) = match settle_query(provider, &captured, chosen, &mut parse) {
            Settled::Done(result) => {
                return result.map(|metrics| QueryOutcome {
                    metrics,
                    args: chosen,
                })
            }
            Settled::UsageError(error) => (error, "usage_error"),
            Settled::Failed(error) => (error, "query_failed"),
        };
        match fallback.take() {
            Some(alternative) => {
                tracing::debug!(
                    event = "account.probe.fallback",
                    subsystem = "account_usage",
                    outcome = "fallback",
                    agent = provider.agent,
                    reason,
                    "首选形态失败，改用回退形态"
                );
                chosen = alternative;
            }
            None => return Err(error),
        }
    }
}

/// app-server 响应队列容量：探针只等 4 个响应，通知不入队，32 槽足够。
const RPC_QUEUE_CAPACITY: usize = 32;

/// 一行 JSON-RPC 是否是响应：带 `id` 且没有 `method`。server→client 的请求（如
/// `execCommandApproval`）同样带 id，且 id 空间由对方分配、通常也从小整数起，会与探针自用的
/// 1..5 撞号；把它当响应会得到 `result` 缺失的 `Null`，进而误判成「未登录」。通知没有 id。
fn rpc_response(value: &Value) -> bool {
    value.get("id").is_some() && value.get("method").is_none()
}

/// app-server 响应队列：只收响应行（`rpc_response`；通知与对方的请求不入队），满时丢最旧
/// 而不是让读线程退出；EOF 后标记关闭，让等待方区分「超时」与「进程已退出」。
struct LineQueue {
    lines: Mutex<(VecDeque<Value>, bool)>,
    ready: Condvar,
}

impl LineQueue {
    fn new() -> Self {
        Self {
            lines: Mutex::new((VecDeque::new(), false)),
            ready: Condvar::new(),
        }
    }

    fn push(&self, value: Value) {
        if !rpc_response(&value) {
            return;
        }
        if let Ok(mut guard) = self.lines.lock() {
            if guard.0.len() >= RPC_QUEUE_CAPACITY {
                guard.0.pop_front();
            }
            guard.0.push_back(value);
            self.ready.notify_all();
        }
    }

    fn close(&self) {
        if let Ok(mut guard) = self.lines.lock() {
            guard.1 = true;
            self.ready.notify_all();
        }
    }

    /// 取下一行；队列空且已关闭 → `Disconnected`，到期 → `Timeout`。
    fn pop(&self, deadline: Instant) -> Result<Value, mpsc::RecvTimeoutError> {
        let mut guard = self
            .lines
            .lock()
            .map_err(|_| mpsc::RecvTimeoutError::Disconnected)?;
        loop {
            if let Some(value) = guard.0.pop_front() {
                return Ok(value);
            }
            if guard.1 {
                return Err(mpsc::RecvTimeoutError::Disconnected);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(mpsc::RecvTimeoutError::Timeout);
            }
            let (next, _) = self
                .ready
                .wait_timeout(guard, remaining)
                .map_err(|_| mpsc::RecvTimeoutError::Disconnected)?;
            guard = next;
        }
    }
}

/// JSON-RPC 错误 → 观测状态，按 `error.message` 判定：实测 codex 对未知方法回 `-32600`
/// `unknown variant …`（不是标准的 `-32601`），错误码只作辅助；登录失败证据 →
/// `NotAuthenticated`（调用方可带 `refreshToken` 重试一次）；其余是 transient `Error`。
pub(super) fn classify_rpc_error(error: &Value) -> QueryError {
    let code = error.get("code").and_then(Value::as_i64);
    let message = error.get("message").and_then(Value::as_str).unwrap_or("");
    let lower = message.to_lowercase();
    let summary = summary_line(&sanitize(message));
    let detail = if summary.is_empty() {
        String::new()
    } else {
        format!("：{summary}")
    };
    if code == Some(-32601)
        || lower.contains("unknown variant")
        || lower.contains("method not found")
        || lower.contains("unknown method")
    {
        return (
            ObservationStatus::Unsupported,
            format!("当前 Codex 版本或登录方式不支持此账号查询{detail}"),
        );
    }
    if parse::auth_evidence(message) {
        return (
            ObservationStatus::NotAuthenticated,
            format!("Codex 报告未登录，请先使用官方登录流程{detail}"),
        );
    }
    (
        ObservationStatus::Error,
        format!("Codex 账号查询失败{detail}，稍后自动重试"),
    )
}

struct Rpc<'a> {
    provider: &'a Provider,
    child: ChildGuard,
    lines: Arc<LineQueue>,
    /// app-server 的 stderr 尾部（≤ `STDERR_TAIL`），进程提前退出时作诊断摘要。
    stderr: Arc<Mutex<Vec<u8>>>,
    deadline: Instant,
}

impl Rpc<'_> {
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
            let value = match self.lines.pop(self.deadline) {
                Ok(value) => value,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    return Err((
                        ObservationStatus::Error,
                        "Codex app-server 未在时限内响应，稍后自动重试".into(),
                    ))
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => return Err(self.disconnected()),
            };
            if value.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = value.get("error") {
                return Err(classify_rpc_error(error));
            }
            return Ok(value.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    /// 进程在响应前退出：stderr 尾部脱敏后进 debug 日志，摘要进文案。
    fn disconnected(&self) -> QueryError {
        let tail = self
            .stderr
            .lock()
            .map(|bytes| sanitize(&String::from_utf8_lossy(tail_bytes(&bytes, STDERR_TAIL))))
            .unwrap_or_default();
        debug_stderr_tail(self.provider, &tail, "Codex app-server 提前退出");
        let summary = summary_line(&tail);
        (
            ObservationStatus::Error,
            if summary.is_empty() {
                "Codex app-server 提前退出（未得到响应），稍后自动重试".into()
            } else {
                format!("Codex app-server 提前退出：{summary}")
            },
        )
    }
}

/// 持续把管道尾部（最多 `STDERR_TAIL` 字节）存进 `sink`；孙进程持有管道时读取不会返回，
/// 所以线程不 join，靠子进程退出后管道关闭自然结束。
fn spawn_tail_reader(mut pipe: impl Read + Send + 'static, sink: Arc<Mutex<Vec<u8>>>) {
    std::thread::spawn(move || {
        let mut bytes = vec![0_u8; 4096];
        while let Ok(length) = pipe.read(&mut bytes) {
            if length == 0 {
                break;
            }
            if let Ok(mut tail) = sink.lock() {
                tail.extend_from_slice(&bytes[..length]);
                let excess = tail.len().saturating_sub(STDERR_TAIL);
                if excess > 0 {
                    tail.drain(..excess);
                }
            }
        }
    });
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
        .stderr(Stdio::piped())
        .spawn()
        .map_err(spawn_error)?;
    let mut child = ChildGuard::new(child)?;
    let stdout = child
        .child
        .stdout
        .take()
        .ok_or_else(|| (ObservationStatus::Error, "RPC 输出不可用".into()))?;
    let stderr = Arc::new(Mutex::new(Vec::new()));
    if let Some(pipe) = child.child.stderr.take() {
        spawn_tail_reader(pipe, stderr.clone());
    }
    let lines = Arc::new(LineQueue::new());
    let queue = lines.clone();
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
            // 只有响应进队列（`LineQueue::push` 过滤）；通知与对方的请求不占槽位。
            if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
                queue.push(value);
            }
        }
        queue.close();
    });
    child.readers.push(reader);
    let mut rpc = Rpc {
        provider,
        child,
        lines,
        stderr,
        deadline: Instant::now() + timeout,
    };
    rpc.send(&json!({"id":1,"method":"initialize","params":{"clientInfo":{"name":"herdr_usage","version":env!("CARGO_PKG_VERSION")}}}))?;
    rpc.receive(1)?;
    rpc.send(&json!({"method":"initialized","params":{}}))?;
    rpc.send(&json!({"id":2,"method":"account/read","params":{"refreshToken":false}}))?;
    let identity = match rpc.receive(2) {
        Ok(identity) => identity,
        // 只读探针默认不刷新令牌；令牌可能只是过期，报未登录时带 refreshToken 重试一次。
        Err((ObservationStatus::NotAuthenticated, message)) => {
            rpc.send(&json!({"id":5,"method":"account/read","params":{"refreshToken":true}}))?;
            rpc.receive(5).map_err(|(_, retry)| {
                (
                    ObservationStatus::NotAuthenticated,
                    format!("{message}；刷新令牌后仍失败：{retry}"),
                )
            })?
        }
        Err(error) => return Err(error),
    };
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
    let statusline = crate::integration::usage_supports_statusline(provider.agent);
    match blocker {
        ProbeBlocker::Trust => InteractiveError {
            status: ObservationStatus::Error,
            message: match stable_dir {
                Some(dir) => format!(
                    "需在 CLI 中确认目录信任：在终端运行 `cd {} && {}` 并选择「Yes, I trust this folder」一次；herdr 不会代为应答",
                    dir.display(),
                    provider.command
                ),
                None if statusline => crate::i18n::texts()
                    .usage_notice
                    .trust_callback_hint
                    .into(),
                None => "需在 CLI 中确认目录信任；herdr 不会代为应答，请先在正常会话完成一次确认".into(),
            },
            trust_required: true,
        },
        ProbeBlocker::SignIn => InteractiveError {
            status: ObservationStatus::NotAuthenticated,
            message: if statusline {
                crate::i18n::texts()
                    .usage_notice
                    .sign_in_callback_hint
                    .into()
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
                    let _ = input.write_all(b"/help\r");
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

/// 本地服务启动重试上限：端口预分配到子进程 bind 之间有竞争窗口，子进程未打印 banner
/// 就退出、或在子预算内等不到 banner 时换端口重试。
const KIMI_START_ATTEMPTS: u32 = 3;

/// 临时访问令牌：只活在查询函数内，不写入配置、日志、报告或 URL。Drop 时清零的只是这一份
/// 拷贝；banner 读取线程的 channel 缓冲、`from_utf8_lossy` 可能产生的副本与 reqwest 请求头
/// 里的副本不在覆盖范围——这不是完整的内存卫生保证，只是不让令牌在本结构里多活一会儿。
struct Secret(Vec<u8>);

impl Secret {
    fn as_str(&self) -> &str {
        std::str::from_utf8(&self.0).unwrap_or("")
    }

    /// 就地清零；Drop 走同一条路径。
    fn zeroize(&mut self) {
        self.0.fill(0);
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        self.zeroize();
    }
}

fn kimi_banner_pattern() -> Option<&'static regex::Regex> {
    static PATTERN: OnceLock<Option<regex::Regex>> = OnceLock::new();
    PATTERN
        .get_or_init(|| {
            regex::Regex::new(r"http://127\.0\.0\.1:(\d+)/#token=([A-Za-z0-9_-]+)").ok()
        })
        .as_ref()
}

/// 已就绪的 `kimi web` 本地服务：子进程与探测目录随之存活。
struct KimiService {
    _child: ChildGuard,
    _directory: ProbeDirectory,
    base: String,
    token: Secret,
}

enum KimiStartError {
    /// 换端口再试（bind 竞争、进程提前退出且无明确原因）。
    Retry(QueryError),
    /// 有明确结论（版本不支持 / 未登录 / 输出异常），不再重试。
    Fatal(QueryError),
}

/// 启动一次 `kimi web` 并等到 banner 里的地址与令牌；`deadline` 是本次尝试的子预算（见
/// `kimi_attempts`），到期未见 banner 归 `Retry`（换端口再试）。
fn kimi_start(
    provider: &Provider,
    account: &UsageAccountConfig,
    deadline: Instant,
) -> Result<KimiService, KimiStartError> {
    let directory = ProbeDirectory::new().map_err(KimiStartError::Fatal)?;
    let listener =
        std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).map_err(|_| {
            KimiStartError::Retry((
                ObservationStatus::Unavailable,
                "无法分配本地查询端口".into(),
            ))
        })?;
    let port = listener
        .local_addr()
        .map_err(|_| KimiStartError::Fatal((ObservationStatus::Error, "无法读取本地端口".into())))?
        .port();
    drop(listener);
    let child = command(provider, account, &directory)
        .map_err(KimiStartError::Fatal)?
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
        .map_err(|error| KimiStartError::Fatal(spawn_error(error)))?;
    let mut child = ChildGuard::new(child).map_err(KimiStartError::Fatal)?;
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
    let pattern = kimi_banner_pattern().ok_or_else(|| {
        KimiStartError::Fatal((ObservationStatus::Error, "本地服务地址解析失败".into()))
    })?;
    // banner 含临时令牌：取出后立即清零。
    let mut banner = Vec::new();
    let result = loop {
        let bytes = match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            Ok(bytes) => bytes,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                break Err(KimiStartError::Retry((
                    ObservationStatus::Unavailable,
                    "Kimi 本地服务未就绪（等待 banner 超时）；请确认版本支持 kimi web".into(),
                )));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let text = String::from_utf8_lossy(tail_bytes(&banner, STDERR_TAIL)).into_owned();
                break Err(kimi_exit_before_banner(provider, &text));
            }
        };
        banner.extend_from_slice(&bytes);
        if banner.len() > MAX_OUTPUT {
            break Err(KimiStartError::Fatal((
                ObservationStatus::Error,
                "Kimi 启动输出过大".into(),
            )));
        }
        let text = String::from_utf8_lossy(&banner);
        if let Some(found) = pattern.captures(&text) {
            let base = format!("http://127.0.0.1:{}", &found[1]);
            let token = Secret(found[2].as_bytes().to_vec());
            break Ok((base, token));
        }
    };
    banner.fill(0);
    let (base, token) = result?;
    Ok(KimiService {
        _child: child,
        _directory: directory,
        base,
        token,
    })
}

/// `kimi web` 在打印 banner 前退出时的分档（纯函数，输入是输出尾部原文）：用法错误 / 帮助
/// 文本 → 版本不支持（`Fatal`，`Unsupported`）；登录证据 → `Fatal`，`NotAuthenticated`；其余
/// （端口竞争、无明确原因）→ `Retry`，脱敏摘要进文案。判定跑在原文上，只有日志与文案用
/// 脱敏文本。
fn kimi_exit_before_banner(provider: &Provider, text: &str) -> KimiStartError {
    let sanitized = sanitize(text);
    debug_stderr_tail(provider, &sanitized, "kimi web 在 banner 前退出");
    if parse::cli_usage_error(text) || parse::cli_help_output(text) {
        return KimiStartError::Fatal((
            ObservationStatus::Unsupported,
            "此版本 Kimi CLI 不支持 kimi web；请升级 CLI".into(),
        ));
    }
    if parse::auth_evidence(text) {
        return KimiStartError::Fatal((
            ObservationStatus::NotAuthenticated,
            "Kimi CLI 报告未登录，请先在正常会话完成登录".into(),
        ));
    }
    let summary = summary_line(&sanitized);
    KimiStartError::Retry((
        ObservationStatus::Unavailable,
        if summary.is_empty() {
            "Kimi 本地服务提前退出（可能是端口竞争）".into()
        } else {
            format!("Kimi 本地服务提前退出：{summary}")
        },
    ))
}

/// 启动重试的编排（启动函数注入，便于不起进程地测试）：最多 `KIMI_START_ATTEMPTS` 次，每次
/// 分到「剩余预算 ÷ 剩余次数」的子预算——等待 banner 超时也能换端口再试，而不是第一次就吃
/// 掉全部预算；`Fatal` 立即终止；`Retry` 在总预算用尽时停止并返回最后一次的错误。第一次
/// 尝试总会执行（预算已耗尽时也给出真实错误而非泛化文案）。
fn kimi_attempts<T>(
    deadline: Instant,
    mut start: impl FnMut(u32, Instant) -> Result<T, KimiStartError>,
) -> Result<T, QueryError> {
    let mut last_error = None;
    for attempt in 1..=KIMI_START_ATTEMPTS {
        let now = Instant::now();
        let left = KIMI_START_ATTEMPTS - attempt + 1;
        let slice = deadline.saturating_duration_since(now) / left;
        match start(attempt, now + slice) {
            Ok(service) => return Ok(service),
            Err(KimiStartError::Fatal(error)) => return Err(error),
            Err(KimiStartError::Retry(error)) => {
                last_error = Some(error);
                if Instant::now() >= deadline {
                    break;
                }
            }
        }
    }
    Err(last_error.unwrap_or((
        ObservationStatus::Unavailable,
        "Kimi 本地服务未就绪；请确认版本支持 kimi web".into(),
    )))
}

/// 本地服务的 HTTP 状态 → 文案；状态分类与远程官方接口共用 `http::status_kind`。
fn kimi_status_error(status: u16) -> QueryError {
    let kind = super::http::status_kind(status);
    let message = match kind {
        ObservationStatus::NotAuthenticated => {
            format!("Kimi 官方查询需要有效登录（HTTP {status}），请先在正常会话完成登录")
        }
        ObservationStatus::PermissionDenied => {
            format!("Kimi 账号无权访问用量接口（HTTP {status}）")
        }
        ObservationStatus::Unsupported => {
            format!("此版本 Kimi CLI 没有该用量接口（HTTP {status}），请升级 CLI")
        }
        _ => format!("Kimi 本地服务返回 HTTP {status}，稍后自动重试"),
    };
    (kind, message)
}

fn kimi_read(service: &KimiService, deadline: Instant) -> Result<(Value, Value), QueryError> {
    let client = reqwest::blocking::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| (ObservationStatus::Error, "无法连接本地官方服务".into()))?;
    let read = |path: &str| -> Result<Value, QueryError> {
        let response = client
            .get(format!("{}{path}", service.base))
            .bearer_auth(service.token.as_str())
            .timeout(
                deadline
                    .saturating_duration_since(Instant::now())
                    .max(Duration::from_millis(1)),
            )
            .send()
            .map_err(|_| (ObservationStatus::Error, "Kimi 官方查询连接失败".into()))?;
        if !response.status().is_success() {
            return Err(kimi_status_error(response.status().as_u16()));
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

pub(super) fn kimi(
    provider: &Provider,
    account: &UsageAccountConfig,
    timeout: Duration,
) -> Result<(Value, Value), QueryError> {
    let deadline = Instant::now() + timeout;
    let service = kimi_attempts(deadline, |attempt, attempt_deadline| {
        let result = kimi_start(provider, account, attempt_deadline);
        if let Err(KimiStartError::Retry(error)) = &result {
            tracing::debug!(
                event = "account.probe.retry",
                subsystem = "account_usage",
                outcome = "retry",
                agent = provider.agent,
                attempt,
                reason = %error.1,
                "kimi web 启动未就绪"
            );
        }
        result
    })?;
    kimi_read(&service, deadline)
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

    fn opencode() -> &'static Provider {
        super::super::registry::provider("opencode").expect("opencode 已登记")
    }

    /// 以退出码 `code` 结束的一次捕获。
    fn exited(code: i32, stdout: &[u8], stderr: &[u8]) -> Captured {
        Captured {
            exit: UsageProbeExit {
                code: Some(code),
                signal: None,
            },
            stdout: stdout.to_vec(),
            stderr: stderr.to_vec(),
        }
    }

    /// 被信号 `signal` 终止的一次捕获。
    fn signaled(signal: i32, stdout: &[u8], stderr: &[u8]) -> Captured {
        Captured {
            exit: UsageProbeExit {
                code: None,
                signal: Some(signal),
            },
            stdout: stdout.to_vec(),
            stderr: stderr.to_vec(),
        }
    }

    /// opencode 1.17.20 对 `stats --json` 的真实 stderr（yargs 直接打印子命令用法，没有
    /// `unknown option` 字样）。
    const OPENCODE_STATS_JSON_STDERR: &str = "\
opencode stats

show token usage and cost statistics

Options:
  -h, --help        show help                                                              [boolean]
  -v, --version     show version number                                                    [boolean]
      --print-logs  print logs to stderr                                                   [boolean]
      --days        show stats for the last N days (default: all time)                      [number]
      --project     filter by project (default: all projects, empty string: current project)[string]
";

    #[test]
    fn nonzero_exit_without_auth_evidence_is_a_transient_error_with_a_sanitized_summary() {
        let stderr =
            b"\x1b[31mError:\x1b[0m connection reset by peer\nRetrying failed after 3 attempts\n";
        let (status, message) = classify_failure(claude(), &exited(1, b"", stderr));
        assert_eq!(
            status,
            ObservationStatus::Error,
            "无登录证据的失败可自动重试"
        );
        assert!(
            message.contains("Retrying failed after 3 attempts"),
            "最后一行进摘要：{message}"
        );
        assert!(message.contains("退出码 1"), "{message}");
        assert!(!message.contains("\x1b"), "ANSI 已脱敏：{message}");

        // 只有退出码、两路都空白：无机器可读输出（版本旧）。
        let (status, message) = classify_failure(claude(), &exited(2, b"  \n", b""));
        assert_eq!(status, ObservationStatus::Unsupported);
        assert!(message.contains("退出码 2"), "{message}");

        // 没有 stderr 但 stdout 有内容：不再一律判「需要登录」。
        let (status, _) = classify_failure(claude(), &exited(1, b"{\"partial\":true}", b""));
        assert_eq!(status, ObservationStatus::Error);
    }

    #[test]
    fn signal_termination_is_transient_whatever_the_output_says() {
        let signaled = signaled(9, b"", b"Not logged in\n");
        let (status, message) = classify_failure(claude(), &signaled);
        assert_eq!(
            status,
            ObservationStatus::Error,
            "被信号终止不是 CLI 的结论，不得钉成终态"
        );
        assert!(message.contains("信号 9"), "{message}");
        assert!(!signaled.usage_error());
        assert!(!signaled.success());
        assert!(exited(0, b"", b"").success());
        assert!(!exited(1, b"", b"").success());
    }

    #[test]
    fn usage_error_or_help_output_means_the_flag_is_unsupported() {
        // yargs：只打印用法。
        let captured = exited(1, b"", OPENCODE_STATS_JSON_STDERR.as_bytes());
        assert!(captured.usage_error());
        let (status, message) = classify_failure(opencode(), &captured);
        assert_eq!(status, ObservationStatus::Unsupported, "{message}");
        assert!(message.contains("不支持此用量查询参数"), "{message}");
        assert!(!message.contains("升级 CLI 或查看官方页面。"), "{message}");
        // commander：明确的 unknown option。
        let captured = exited(1, b"", b"error: unknown option '--json'\n");
        assert!(captured.usage_error());
        let (status, message) = classify_failure(opencode(), &captured);
        assert_eq!(status, ObservationStatus::Unsupported);
        assert!(message.contains("unknown option"), "{message}");
        // 普通错误不是用法错误。
        assert!(!exited(1, b"", b"segfault\n").usage_error());
    }

    #[test]
    fn nonzero_exit_with_auth_evidence_stays_not_authenticated() {
        let stderr = b"Not logged in. Run claude auth login to authenticate.\n";
        let (status, _) = classify_failure(claude(), &exited(1, b"", stderr));
        assert_eq!(status, ObservationStatus::NotAuthenticated);
        let (status, _) = classify_failure(claude(), &exited(1, b"error: unauthorized (401)", b""));
        assert_eq!(status, ObservationStatus::NotAuthenticated);
        // 证据藏在会被脱敏整段遮掉的长串里：判定必须跑在原文上。
        let (status, message) = classify_failure(
            claude(),
            &exited(1, b"", b"Unauthorized-request-id-0123456789abcdef0123\n"),
        );
        assert_eq!(status, ObservationStatus::NotAuthenticated, "{message}");
    }

    #[test]
    fn help_text_listing_auth_login_is_a_usage_error_not_missing_login() {
        // 用法错误的帮助屏里列出 `auth login` 子命令（极常见）：先判用法错误，与
        // classify_auth_status 的顺序一致，不能被裸关键字 `auth login` 判成未登录终态。
        let help = b"error: unknown option '--json'\n\nUsage: foo usage [options]\n\nCommands:\n  foo auth login   Sign in\n  foo usage        Show usage\n\nOptions:\n  -h, --help  display help\n";
        let (status, message) = classify_failure(claude(), &exited(1, b"", help));
        assert_eq!(status, ObservationStatus::Unsupported, "{message}");
        assert!(message.contains("用法错误"), "{message}");
        // 只有帮助标题、没有 unknown option 字样：同样是用法错误。
        let help = b"Usage: foo usage [options]\n\nOptions:\n  -h, --help\n\nRun `foo auth login` first.\n";
        let (status, _) = classify_failure(claude(), &exited(1, b"", help));
        assert_eq!(status, ObservationStatus::Unsupported);
        // 真实用量输出用 `Usage:` 作小标题、部分输出后非零退出：可重试的 Error，不是终态。
        let (status, message) = classify_failure(
            claude(),
            &exited(1, b"Usage: 1,234 / 10,000 credits\n", b""),
        );
        assert_eq!(status, ObservationStatus::Error, "{message}");
        assert!(!exited(1, b"Usage: 1,234 / 10,000 credits\n", b"").usage_error());
    }

    #[test]
    fn help_capture_is_settled_by_content_before_exit_code() {
        // exit=1 + stderr 是异常栈：失败，不能当帮助。
        let stack = b"node:internal/modules/cjs/loader:1228\n  throw err;\n  ^\nError: Cannot find module 'yargs'\n";
        let (status, message) =
            settle_help(opencode(), &exited(1, b"", stack)).expect_err("异常栈不是帮助");
        assert_eq!(status, ObservationStatus::Error, "{message}");
        // exit=1 + stderr 是 yargs 用法：帮助文本。
        let text = settle_help(
            opencode(),
            &exited(1, b"", OPENCODE_STATS_JSON_STDERR.as_bytes()),
        )
        .expect("yargs 用法就是帮助");
        assert!(text.contains("--print-logs"));
        assert!(parse::cli_help_output(&text), "可缓存");
        // exit=0 + 不像帮助的文本（oclif 无冒号标题）：原样返回但不可缓存。
        let text = settle_help(opencode(), &exited(0, b"USAGE\n  $ opencode stats\n", b""))
            .expect("正常退出原样返回");
        assert!(!parse::cli_help_output(&text));
        // 两路都空白：非零按退出形态分类，正常退出是「没有帮助文本」。
        let (status, _) = settle_help(opencode(), &exited(1, b"", b"")).expect_err("空白");
        assert_eq!(status, ObservationStatus::Unsupported);
        let (status, message) = settle_help(opencode(), &exited(0, b" \n", b"")).expect_err("空白");
        assert_eq!(status, ObservationStatus::Unsupported);
        assert!(message.contains("没有输出帮助文本"), "{message}");
        // 被信号终止：transient。
        let (status, _) = settle_help(opencode(), &signaled(9, b"", b"partial")).expect_err("信号");
        assert_eq!(status, ObservationStatus::Error);
    }

    #[test]
    fn help_precheck_reads_subcommands_and_flags_with_word_boundaries() {
        // 顶层帮助：yargs 形态（`opencode stats  描述`）与缩进列表形态。
        let top = "Commands:\n  opencode stats               show token usage and cost statistics\n  opencode auth  manage credentials\n";
        assert!(help_lists_subcommand(top, "opencode", "stats"));
        assert!(!help_lists_subcommand(top, "opencode", "stat"));
        assert!(!help_lists_subcommand(top, "opencode", "usage"));
        assert!(help_lists_subcommand(
            "  usage    Show usage\n",
            "kimi",
            "usage"
        ));
        assert!(!help_lists_subcommand(
            "  usages   Show usage\n",
            "kimi",
            "usage"
        ));
        // opencode 1.17.20 / 1.18.31 的 `db --help`：首选形态的 `--format` 被列出，SQL 位置参数
        // 不是 flag、不参与判定。
        assert!(help_lists_subcommand(OPENCODE_TOP_HELP, "opencode", "db"));
        assert!(help_lists_subcommand(
            OPENCODE_TOP_HELP,
            "opencode",
            "stats"
        ));
        assert!(help_lists_flags(OPENCODE_DB_HELP, OPENCODE_DB_ARGS));
        assert!(!help_lists_flags(
            OPENCODE_STATS_JSON_STDERR,
            OPENCODE_DB_ARGS
        ));
        // 子命令帮助：1.17.20 的 stats 没有 --json。
        assert!(!help_lists_flags(
            OPENCODE_STATS_JSON_STDERR,
            &["stats", "--json"]
        ));
        assert!(help_lists_flags(
            OPENCODE_STATS_JSON_STDERR,
            &["stats", "--days"]
        ));
        assert!(
            help_lists_flags(OPENCODE_STATS_JSON_STDERR, &["stats"]),
            "无 flag 恒真"
        );
        // 词边界：--json 不命中 --jsonl / --json-lines / --no-json 的子串… 除非确实列出。
        assert!(!help_lists_flags("  --jsonl  lines\n", &["x", "--json"]));
        assert!(!help_lists_flags(
            "  --json-lines  lines\n",
            &["x", "--json"]
        ));
        assert!(help_lists_flags(
            "  -j, --json  Output JSON\n",
            &["x", "--json"]
        ));
        assert!(help_lists_flags("  --json=<fmt>\n", &["x", "--json"]));
        assert!(help_lists_flags("  --json\n", &["x", "--json"]));
        assert!(help_lists_flags(
            "  --json, -j\tOutput JSON\n",
            &["x", "--json"]
        ));
        assert!(!help_lists_flags("", &["x", "--json"]));
        // 多个 flag 必须全部列出。
        assert!(!help_lists_flags("  --json\n", &["x", "--json", "--all"]));
        // 只看 flag 行的 flag 列：描述文字里提到的 --json 不算。
        assert!(!help_lists_flags(
            "  --format  output format (use --json for machine output)\n",
            &["x", "--json"]
        ));
        assert!(!help_lists_flags(
            "Use --json to get machine output.\n",
            &["x", "--json"]
        ));
        assert_eq!(flag_column("-h, --help        show help"), "-h, --help");
        assert_eq!(flag_column("--json=<fmt>"), "--json=<fmt>");
        assert_eq!(flag_column("--json\tdesc  more"), "--json");
    }

    /// opencode 顶层帮助（1.17.20 / 1.18.31 的相关行）：`db` 与 `stats` 都在。
    const OPENCODE_TOP_HELP: &str = "Commands:\n  opencode stats   show token usage and cost statistics\n  opencode db      database tools\n";
    /// opencode `db --help`（1.17.20 与 1.18.31 逐字一致的选项段）。
    const OPENCODE_DB_HELP: &str = "opencode db\n\ndatabase tools\n\nCommands:\n  opencode db [query]     open an interactive sqlite3 shell or run a query  [default]\n  opencode db path        print the database path\n\nOptions:\n  -h, --help        show help  [boolean]\n      --format      Output format  [string] [choices: \"json\", \"tsv\"] [default: \"tsv\"]\n";
    /// 首选形态：SQL 用占位串，编排逻辑不关心查询体。
    const OPENCODE_DB_ARGS: &[&str] = &["db", "SELECT 1", "--format", "json"];
    const OPENCODE_STATS_ARGS: &[&str] = &["stats"];

    /// 一次注入的捕获：`(args, 退出码, stdout, stderr)`。
    type Scripted = (&'static [&'static str], i32, &'static str, &'static str);

    /// 用注入的帮助文本与脚本化的进程结果跑 `run_query`，返回结果与实际起过的进程参数。
    fn scripted_query(
        top_help: &str,
        sub_help: Result<&str, QueryError>,
        script: &[Scripted],
        fallback: bool,
    ) -> (
        Result<QueryOutcome, QueryError>,
        Vec<&'static [&'static str]>,
    ) {
        let fallback_args: Option<&'static [&'static str]> =
            fallback.then_some(OPENCODE_STATS_ARGS);
        let mut ran = Vec::new();
        let mut script = script.iter();
        let result = run_query(
            opencode(),
            OPENCODE_DB_ARGS,
            fallback_args,
            Duration::from_secs(20),
            |sub, budget| {
                assert!(budget <= HELP_TIMEOUT, "帮助预检不超过封顶时限");
                match sub {
                    None => Ok(top_help.to_owned()),
                    Some("db") => sub_help.clone().map(str::to_owned),
                    Some(other) => panic!("意外的子命令帮助 {other}"),
                }
            },
            |chosen, _| {
                ran.push(chosen);
                let (expected, code, stdout, stderr) = script
                    .next()
                    .unwrap_or_else(|| panic!("多余的进程 {chosen:?}"));
                assert_eq!(*expected, chosen, "进程参数顺序");
                Ok(exited(*code, stdout.as_bytes(), stderr.as_bytes()))
            },
            |text, chosen| {
                Ok(if chosen.first() == Some(&"db") {
                    parse::opencode_sessions(text)
                } else {
                    parse::opencode_stats(text)
                })
            },
        );
        (result, ran)
    }

    #[test]
    fn query_orchestration_prechecks_help_then_falls_back_once() {
        let db_args = OPENCODE_DB_ARGS;
        let stats_args = OPENCODE_STATS_ARGS;
        let rows = "[\n  {\n    \"sessions\": 41,\n    \"cost\": 0.5\n  }\n]\n";
        let table = "│Sessions   41 │\n";
        let unavailable = || Err((ObservationStatus::Error, "no sub help".to_owned()));

        // (a) 首选形态成功：args 保持首选，子命令帮助列出了 --format 所以不回退。
        let (result, ran) = scripted_query(
            OPENCODE_TOP_HELP,
            Ok(OPENCODE_DB_HELP),
            &[(db_args, 0, rows, "")],
            true,
        );
        let outcome = result.expect("首选成功");
        assert_eq!(outcome.args, db_args);
        assert_eq!(ran, vec![db_args]);
        assert_eq!(outcome.metrics[0].id, "sessions");
        assert_eq!(outcome.metrics[0].used, Some(41.0));

        // (b) 子命令帮助未列出 --format：直接选回退形态，只起一次进程，args 透出为回退形态。
        let (result, ran) = scripted_query(
            OPENCODE_TOP_HELP,
            Ok("opencode db\n\nOptions:\n  -h, --help  show help\n"),
            &[(stats_args, 0, table, "")],
            true,
        );
        let outcome = result.expect("回退形态成功");
        assert_eq!(outcome.args, stats_args);
        assert_eq!(ran, vec![stats_args]);
        assert_eq!(outcome.metrics[0].id, "sessions");

        // (c) 旧版 CLI 没有 db 子命令但有 stats：不起必然失败的首选进程，直接用回退形态。
        let (result, ran) = scripted_query(
            "Commands:\n  opencode stats  show token usage and cost statistics\n",
            Err((ObservationStatus::Error, "不该被调用".into())),
            &[(stats_args, 0, table, "")],
            true,
        );
        assert_eq!(result.expect("回退形态成功").args, stats_args);
        assert_eq!(ran, vec![stats_args]);

        // (d) 子命令帮助取不到：先跑首选，用法错误后回退一次。
        let (result, ran) = scripted_query(
            OPENCODE_TOP_HELP,
            unavailable(),
            &[
                (db_args, 1, "", "error: unknown option '--format'\n"),
                (stats_args, 0, table, ""),
            ],
            true,
        );
        let outcome = result.expect("回退成功");
        assert_eq!(outcome.args, stats_args);
        assert_eq!(ran, vec![db_args, stats_args]);

        // (e) 查询本身失败（旧库缺列）：不是用法错误，同样回退一次。
        let (result, ran) = scripted_query(
            OPENCODE_TOP_HELP,
            Ok(OPENCODE_DB_HELP),
            &[
                (
                    db_args,
                    1,
                    "",
                    "SQLiteError: no such column: tokens_input\n",
                ),
                (stats_args, 0, table, ""),
            ],
            true,
        );
        assert_eq!(result.expect("回退成功").args, stats_args);
        assert_eq!(ran, vec![db_args, stats_args]);

        // (f) 回退也失败：返回最后一次（回退形态）的错误——它更能反映当前可否重试。
        let (result, ran) = scripted_query(
            OPENCODE_TOP_HELP,
            unavailable(),
            &[
                (db_args, 1, "", "SQLiteError: no such table: session\n"),
                (stats_args, 1, "", "database is locked\n"),
            ],
            true,
        );
        let (status, message) = result.expect_err("两次都失败");
        assert_eq!(status, ObservationStatus::Error, "{message}");
        assert!(message.contains("database is locked"), "{message}");
        assert_eq!(ran.len(), 2);
        // 回退形态也是用法错误：终态 Unsupported，不再有第三次。
        let (result, ran) = scripted_query(
            OPENCODE_TOP_HELP,
            unavailable(),
            &[
                (db_args, 1, "", "error: unknown option '--format'\n"),
                (stats_args, 1, "", "error: unknown command 'stats'\n"),
            ],
            true,
        );
        assert_eq!(
            result.expect_err("用法错误").0,
            ObservationStatus::Unsupported
        );
        assert_eq!(ran.len(), 2);

        // (g) 顶层帮助既没有首选也没有回退子命令：终态，不起查询进程。
        let (result, ran) = scripted_query("Commands:\n  opencode run\n", Ok(""), &[], true);
        let (status, message) = result.expect_err("子命令缺失");
        assert_eq!(status, ObservationStatus::Unsupported);
        assert!(message.contains("没有此官方用量子命令"), "{message}");
        assert!(ran.is_empty());

        // (h) 没有回退形态：不查子命令帮助，失败直接是结论。
        let (result, ran) = scripted_query(
            OPENCODE_TOP_HELP,
            Err((ObservationStatus::Error, "不该被调用".into())),
            &[(db_args, 1, "", "error: unknown option '--format'\n")],
            false,
        );
        assert_eq!(
            result.expect_err("用法错误").0,
            ObservationStatus::Unsupported
        );
        assert_eq!(ran, vec![db_args]);

        // (i) 顶层帮助本身失败（偶发）：透传 transient 错误，不起查询进程。
        let result = run_query(
            opencode(),
            OPENCODE_DB_ARGS,
            Some(OPENCODE_STATS_ARGS),
            Duration::from_secs(20),
            |_, _| Err((ObservationStatus::Error, "官方 CLI 查询超时".into())),
            |chosen, _| panic!("不应起进程 {chosen:?}"),
            |_, _| Ok(Vec::new()),
        );
        assert_eq!(result.expect_err("帮助失败").0, ObservationStatus::Error);
    }

    #[test]
    fn choose_args_is_a_pure_table() {
        let top = OPENCODE_TOP_HELP;
        let args = OPENCODE_DB_ARGS;
        let plain = OPENCODE_STATS_ARGS;
        /// `(顶层帮助, 子命令帮助, 回退形态, 期望的 (选定, 保留回退) 或状态)`。
        type Case = (
            &'static str,
            Option<&'static str>,
            Option<&'static [&'static str]>,
            Result<(&'static [&'static str], Option<&'static [&'static str]>), ObservationStatus>,
        );
        let cases: &[Case] = &[
            // 子命令帮助未列出 flag → 回退形态、不再保留回退。
            (
                top,
                Some("Options:\n  -h, --help  show help\n"),
                Some(plain),
                Ok((plain, None)),
            ),
            // 子命令帮助列出了 flag → 首选并保留回退。
            (
                top,
                Some(OPENCODE_DB_HELP),
                Some(plain),
                Ok((args, Some(plain))),
            ),
            // 拿不到子命令帮助 → 首选并保留回退。
            (top, None, Some(plain), Ok((args, Some(plain)))),
            // 无回退形态 → 首选，子命令帮助不参与。
            (
                top,
                Some("Options:\n  -h, --help  show help\n"),
                None,
                Ok((args, None)),
            ),
            // 顶层帮助没有首选子命令、但有回退子命令 → 直接回退，不再保留回退。
            (
                "Commands:\n  opencode stats  desc\n",
                None,
                Some(plain),
                Ok((plain, None)),
            ),
            // 顶层帮助没有首选子命令，也没有回退形态 → 终态。
            (
                "Commands:\n  opencode stats  desc\n",
                None,
                None,
                Err(ObservationStatus::Unsupported),
            ),
            // 顶层帮助两个子命令都没有 → 终态。
            (
                "Commands:\n  opencode run\n",
                None,
                Some(plain),
                Err(ObservationStatus::Unsupported),
            ),
        ];
        for (index, (top, sub, fallback, expected)) in cases.iter().enumerate() {
            let actual =
                choose_args(top, *sub, "opencode", args, *fallback).map_err(|(status, _)| status);
            assert_eq!(actual, *expected, "用例 {index}");
        }
        assert_eq!(
            choose_args(top, None, "opencode", &[], None)
                .expect_err("空 args")
                .0,
            ObservationStatus::Unsupported
        );
    }

    #[test]
    fn help_prechecks_leave_at_least_half_the_budget_for_the_query() {
        // 5 s 下限：顶层预检 ≤ 2.5 s。
        assert_eq!(
            help_budget(Duration::from_secs(5)),
            Duration::from_millis(2500)
        );
        assert_eq!(help_budget(Duration::from_secs(20)), HELP_TIMEOUT);
        // 子命令级预检只用「为查询保留一半」之外的剩余，不足 250 ms 跳过。
        let five = Duration::from_secs(5);
        assert_eq!(
            sub_help_budget(five, five),
            Some(Duration::from_millis(2500))
        );
        assert_eq!(
            sub_help_budget(five, Duration::from_millis(2700)),
            None,
            "冷启动吃掉顶层预检后跳过子命令预检"
        );
        assert_eq!(
            sub_help_budget(five, Duration::from_millis(2800)),
            Some(Duration::from_millis(300))
        );
        let twenty = Duration::from_secs(20);
        assert_eq!(sub_help_budget(twenty, twenty), Some(HELP_TIMEOUT));
        assert_eq!(
            sub_help_budget(twenty, Duration::from_secs(11)),
            Some(Duration::from_secs(1))
        );
        assert_eq!(sub_help_budget(twenty, Duration::from_secs(10)), None);
        assert_eq!(sub_help_budget(twenty, Duration::ZERO), None);
    }

    #[test]
    fn settle_query_prefers_parsed_content_and_flags_failures_for_fallback() {
        let stats: &'static [&'static str] = &["stats"];
        let mut parse = |text: &str, _: &'static [&'static str]| {
            Ok(if text.contains("Sessions") {
                vec![UsageMetric {
                    id: "sessions".into(),
                    used: Some(1.0),
                    ..Default::default()
                }]
            } else {
                Vec::new()
            })
        };
        // 先看内容：stdout 可解析就是结论，即使退出码非零。
        let captured = exited(1, b"|Sessions  1 |\n", b"warning: deprecated\n");
        let Settled::Done(Ok(metrics)) = settle_query(opencode(), &captured, stats, &mut parse)
        else {
            panic!("有内容即结论");
        };
        assert_eq!(metrics.len(), 1);
        // 无内容 + 用法错误 → 可回退。
        let captured = exited(1, b"", OPENCODE_STATS_JSON_STDERR.as_bytes());
        assert!(matches!(
            settle_query(opencode(), &captured, stats, &mut parse),
            Settled::UsageError((ObservationStatus::Unsupported, _))
        ));
        // 无内容 + 进程自身失败（不是用法错误）→ 单独标出，调用方有回退形态时再试一次。
        let captured = exited(1, b"", b"SQLiteError: no such column: tokens_input\n");
        assert!(matches!(
            settle_query(opencode(), &captured, stats, &mut parse),
            Settled::Failed((ObservationStatus::Error, _))
        ));
        // 被信号终止 → transient，不回退。
        let killed = signaled(15, b"", OPENCODE_STATS_JSON_STDERR.as_bytes());
        assert!(matches!(
            settle_query(opencode(), &killed, stats, &mut parse),
            Settled::Done(Err((ObservationStatus::Error, _)))
        ));
        // 正常退出但解析不出：空指标交给调用方（→ 无已验证字段）。
        let captured = exited(0, b"nothing here\n", b"");
        assert!(matches!(
            settle_query(opencode(), &captured, stats, &mut parse),
            Settled::Done(Ok(metrics)) if metrics.is_empty()
        ));
        // 正常退出、stdout 空白：同样是空指标。
        let captured = exited(0, b"  \n", b"");
        assert!(matches!(
            settle_query(opencode(), &captured, stats, &mut parse),
            Settled::Done(Ok(metrics)) if metrics.is_empty()
        ));
        // 解析器自己的结论（NeedsBinding）在正常退出时原样透传。
        let mut needs_binding = |_: &str, _: &'static [&'static str]| {
            Err((ObservationStatus::NeedsBinding, "多个账号".to_owned()))
        };
        let captured = exited(0, b"{}", b"");
        assert!(matches!(
            settle_query(opencode(), &captured, stats, &mut needs_binding),
            Settled::Done(Err((ObservationStatus::NeedsBinding, _)))
        ));
    }

    #[test]
    fn help_cache_expires_on_ttl_or_binary_change() {
        let stamp = Some((PathBuf::from("/bin/opencode"), None, 10));
        let now = Instant::now();
        let cached = (stamp.clone(), now, "help".to_owned());
        assert!(help_cache_fresh(&cached, &stamp, now));
        assert!(help_cache_fresh(
            &cached,
            &stamp,
            now + HELP_TTL - Duration::from_secs(1)
        ));
        assert!(
            !help_cache_fresh(&cached, &stamp, now + HELP_TTL),
            "TTL 到期"
        );
        let upgraded = Some((PathBuf::from("/bin/opencode"), None, 11));
        assert!(!help_cache_fresh(&cached, &upgraded, now), "二进制变化");
        assert!(!help_cache_fresh(&cached, &None, now), "CLI 消失");
    }

    #[test]
    fn rpc_errors_are_classified_by_message_and_the_queue_keeps_responses_only() {
        // 实测：未知方法回 -32600 `unknown variant`，不是 -32601。
        let (status, message) = classify_rpc_error(
            &json!({"code":-32600,"message":"unknown variant `account/usage/read`, expected one of `initialize`, …"}),
        );
        assert_eq!(status, ObservationStatus::Unsupported, "{message}");
        assert!(message.contains("unknown variant"), "{message}");
        let (status, _) = classify_rpc_error(&json!({"code":-32601,"message":"Method not found"}));
        assert_eq!(status, ObservationStatus::Unsupported);
        let (status, message) = classify_rpc_error(
            &json!({"code":-32600,"message":"authentication required: run codex login"}),
        );
        assert_eq!(status, ObservationStatus::NotAuthenticated, "{message}");
        let (status, message) = classify_rpc_error(
            &json!({"code":-32000,"message":"upstream 503 from api.openai.com"}),
        );
        assert_eq!(status, ObservationStatus::Error, "{message}");
        assert!(message.contains("稍后自动重试"), "{message}");
        let (status, message) = classify_rpc_error(&json!({"code":-32000}));
        assert_eq!(status, ObservationStatus::Error);
        assert!(
            !message.contains("："),
            "无 message 时不带空摘要：{message}"
        );
        // 脱敏：错误里的令牌不进文案。
        let (_, message) =
            classify_rpc_error(&json!({"message":format!("token={} rejected", "a1".repeat(20))}));
        assert!(message.contains("[redacted]"), "{message}");

        // 队列：满时丢最旧；关闭后区分 Disconnected 与 Timeout。
        let queue = LineQueue::new();
        for id in 0..(RPC_QUEUE_CAPACITY as u64 + 4) {
            queue.push(json!({"id":id}));
        }
        let first = queue
            .pop(Instant::now() + Duration::from_millis(10))
            .expect("有响应");
        assert_eq!(
            first.get("id").and_then(Value::as_u64),
            Some(4),
            "最旧的 4 条被丢弃"
        );
        for _ in 1..RPC_QUEUE_CAPACITY {
            queue.pop(Instant::now()).expect("剩余响应");
        }
        assert_eq!(
            queue.pop(Instant::now() + Duration::from_millis(20)),
            Err(mpsc::RecvTimeoutError::Timeout)
        );
        queue.close();
        assert_eq!(
            queue.pop(Instant::now() + Duration::from_secs(1)),
            Err(mpsc::RecvTimeoutError::Disconnected)
        );
        // 关闭后仍先交出已入队的响应。
        let queue = LineQueue::new();
        queue.push(json!({"id":7}));
        queue.close();
        assert!(queue.pop(Instant::now()).is_ok());
        assert_eq!(
            queue.pop(Instant::now()),
            Err(mpsc::RecvTimeoutError::Disconnected)
        );
        // 对方的请求（带 id 也带 method）与通知（无 id）都不是响应，不得与自用 id 撞号。
        let queue = LineQueue::new();
        queue.push(json!({"id":2,"method":"execCommandApproval","params":{}}));
        queue.push(json!({"method":"notify","params":{}}));
        assert_eq!(
            queue.pop(Instant::now() + Duration::from_millis(10)),
            Err(mpsc::RecvTimeoutError::Timeout),
            "server→client 请求不入队"
        );
        queue.push(json!({"id":2,"result":{"account":{"id":"me"}}}));
        assert!(queue.pop(Instant::now()).is_ok());
        assert!(rpc_response(&json!({"id":1,"error":{"code":-32600}})));
        assert!(!rpc_response(&json!({"id":1,"method":"x"})));
        assert!(!rpc_response(&json!({"result":{}})));
    }

    #[test]
    fn kimi_exit_before_banner_is_classified_into_fatal_and_retry() {
        let kimi = super::super::registry::provider("kimi").expect("kimi 已登记");
        // 用法错误 / 帮助文本 → 版本不支持，不重试。
        for text in [
            "error: unknown command 'web'\n",
            "Usage: kimi [options] [command]\n\nOptions:\n  -h, --help\n",
        ] {
            assert!(
                matches!(
                    kimi_exit_before_banner(kimi, text),
                    KimiStartError::Fatal((ObservationStatus::Unsupported, _))
                ),
                "{text:?}"
            );
        }
        // 登录证据 → 未登录，不重试。
        assert!(matches!(
            kimi_exit_before_banner(kimi, "Not logged in. Run kimi login first.\n"),
            KimiStartError::Fatal((ObservationStatus::NotAuthenticated, _))
        ));
        // 普通噪声 / 空输出 → 可重试，摘要脱敏进文案。
        match kimi_exit_before_banner(kimi, "\x1b[31mbind: address already in use\x1b[0m\n") {
            KimiStartError::Retry((ObservationStatus::Unavailable, message)) => {
                assert!(message.contains("address already in use"), "{message}");
                assert!(!message.contains("\x1b"), "{message}");
            }
            _ => panic!("端口竞争应可重试"),
        }
        assert!(matches!(
            kimi_exit_before_banner(kimi, ""),
            KimiStartError::Retry((ObservationStatus::Unavailable, _))
        ));
    }

    #[test]
    fn kimi_start_attempts_split_the_budget_and_stop_on_fatal_or_deadline() {
        // 三次 Retry：每次分到「剩余 ÷ 剩余次数」的子预算，用尽后返回最后一次的错误。
        let start = Instant::now();
        let deadline = start + Duration::from_secs(30);
        let mut seen = Vec::new();
        let result: Result<(), QueryError> =
            kimi_attempts(deadline, |attempt, attempt_deadline| {
                seen.push((attempt, attempt_deadline));
                Err(KimiStartError::Retry((
                    ObservationStatus::Unavailable,
                    format!("第 {attempt} 次"),
                )))
            });
        assert_eq!(result.expect_err("三次都未就绪").1, "第 3 次");
        assert_eq!(
            seen.iter().map(|(a, _)| *a).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        // 第一次只拿约 1/3（等待 banner 超时也能换端口再试），最后一次拿到全部剩余。
        assert!(
            seen[0].1 <= start + Duration::from_secs(11),
            "{:?}",
            seen[0].1 - start
        );
        assert!(
            seen[0].1 >= start + Duration::from_secs(9),
            "{:?}",
            seen[0].1 - start
        );
        assert!(seen[2].1 >= deadline - Duration::from_millis(50));
        assert!(seen
            .iter()
            .all(|(_, d)| *d <= deadline + Duration::from_millis(1)));
        // Fatal 立即终止。
        let mut calls = 0;
        let result: Result<(), QueryError> = kimi_attempts(deadline, |_, _| {
            calls += 1;
            Err(KimiStartError::Fatal((
                ObservationStatus::NotAuthenticated,
                "未登录".into(),
            )))
        });
        assert_eq!(
            result.expect_err("终态").0,
            ObservationStatus::NotAuthenticated
        );
        assert_eq!(calls, 1);
        // Retry 后成功。
        let mut calls = 0;
        let result = kimi_attempts(deadline, |attempt, _| {
            calls += 1;
            if attempt == 1 {
                Err(KimiStartError::Retry((
                    ObservationStatus::Unavailable,
                    "竞争".into(),
                )))
            } else {
                Ok("service")
            }
        });
        assert_eq!(result.expect("第二次成功"), "service");
        assert_eq!(calls, 2);
        // 总预算已过：第一次仍执行（给出真实错误），Retry 后不再重试。
        let mut calls = 0;
        let result: Result<(), QueryError> = kimi_attempts(
            Instant::now() - Duration::from_secs(1),
            |_, attempt_deadline| {
                calls += 1;
                assert!(attempt_deadline <= Instant::now());
                Err(KimiStartError::Retry((
                    ObservationStatus::Unavailable,
                    "晚了".into(),
                )))
            },
        );
        assert_eq!(result.expect_err("预算用尽").1, "晚了");
        assert_eq!(calls, 1);
    }

    #[test]
    fn secret_zeroing_blanks_its_buffer_and_http_statuses_follow_the_shared_table() {
        // 只断言 `zeroize` 对这一份缓冲的行为（Drop 走同一路径）；清零后的内存无法安全观测，
        // 也不承诺 channel 缓冲与 HTTP 头里的副本。
        let mut secret = Secret(b"tok-123".to_vec());
        assert_eq!(secret.as_str(), "tok-123");
        secret.zeroize();
        assert_eq!(secret.as_str(), "\0\0\0\0\0\0\0");
        drop(secret);
        assert_eq!(
            kimi_status_error(401).0,
            ObservationStatus::NotAuthenticated
        );
        assert_eq!(
            kimi_status_error(403).0,
            ObservationStatus::PermissionDenied
        );
        assert_eq!(kimi_status_error(404).0, ObservationStatus::Unsupported);
        for transient in [429, 500, 502] {
            let (status, message) = kimi_status_error(transient);
            assert_eq!(status, ObservationStatus::Error, "{transient}");
            assert!(message.contains(&transient.to_string()), "{message}");
        }
        let pattern = kimi_banner_pattern().expect("banner 正则");
        let found = pattern
            .captures("Kimi web UI: http://127.0.0.1:58627/#token=abc_DEF-9\n")
            .expect("banner");
        assert_eq!(&found[1], "58627");
        assert_eq!(&found[2], "abc_DEF-9");
    }

    #[test]
    fn auth_status_classification_prefers_stdout_json_over_the_exit_code() {
        let captured = |success: bool, stdout: &str, stderr: &str| {
            exited(
                if success { 0 } else { 1 },
                stdout.as_bytes(),
                stderr.as_bytes(),
            )
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
        let kimi = super::super::registry::provider("kimi").expect("kimi 已登记");
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
        // 文档终审 D2：指引写账号页上「官方回调」开关的名字，不再指向「监控 → 设置」。
        assert!(
            error.message.contains("在账号页打开「官方回调」"),
            "claude 给回调指引：{}",
            error.message
        );
        assert!(!error.message.contains("监控 → 设置"));
        assert!(
            !blocker_error(kimi, trust, None)
                .message
                .contains("官方回调"),
            "没有 statusline 回调的厂商不给这条指引"
        );
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
        assert!(
            error.message.contains("在账号页打开「官方回调」"),
            "{}",
            error.message
        );

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
