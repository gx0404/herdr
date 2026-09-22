//! agent 活动树的来源适配器：各 CLI 把自己的子 agent / 任务 / 待办 / 后台进程
//! 读成统一的 `AgentActivityNode`，外部来源（不属于任何 pane 的会话）另经
//! `discover_external` 给出。适配器内只有同步纯函数（入参含 `home`，可用临时
//! 目录单测）；后台线程 runtime、限频与刷新调度写在本文件，I/O 全在后台线程。
//!
//! 数据流：钩子经 `pane.report_agent_activity` 把提示放进 `AppState` 的收件箱 →
//! [`Service::tick`]（server 主循环每轮调用，内部按 [`SCHEDULER_PASS_INTERVAL`]
//! 限流）取走提示并按触发条件挑出到期的 pane → 后台线程调适配器 → 结果经
//! `AppEvent::{AgentActivityRefreshed, ExternalAgentsRefreshed}` 回主线程落库。
//! `agent.activity.read` / `agent.external.list` 也在同一后台线程执行，响应异步
//! 返回（JSON API 走请求自带的应答通道，客户端端点走 `ServerEvent`）。

mod claude;
mod codex;
mod kimi;
mod opencode;
// seam-stub(adapter-pi)：pi 的入口 `discover_from_hint` / `read_from_hint` 不走
// trait，runtime 接通前整个子模块无调用方；接通提交删除。
#[allow(dead_code)]
mod pi;
mod zcode;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::api::schema::{
    AgentActivityContent, AgentActivityContentFormat, AgentActivityNode, ErrorBody, ErrorResponse,
    ExternalAgentInfo, Method, Request, ResponseResult, SuccessResponse,
};
use crate::app::state::AgentActivitySubject;
use crate::events::AppEvent;
use crate::layout::PaneId;
use crate::server::client_transport::ServerEvent;

/// 一次发现 / 读取的上下文。`home` 由调用方注入，测试传临时目录。
// seam-stub(adapters)：字段只有适配器读取，六个适配器目前都是空壳；首个读取
// 上下文的适配器车道落地后删除。
#[allow(dead_code)]
pub(crate) struct SourceContext<'a> {
    /// 规范化的 agent 名：`"claude"`、`"codex"` 等。
    pub agent: &'a str,
    pub session: Option<&'a crate::agent_resume::AgentSessionRef>,
    pub cwd: Option<&'a Path>,
    pub home: &'a Path,
    pub now_ms: u64,
}

/// 一个节点的内容片段；`next_cursor` 对调用方不透明。
// seam-stub(adapters)：只有适配器会构造内容片段，六个适配器目前都回
// `Unsupported`；首个实现 `read` 的适配器车道落地后删除。
#[allow(dead_code)]
pub(crate) struct ContentChunk {
    pub format: AgentActivityContentFormat,
    pub text: String,
    pub next_cursor: Option<String>,
    pub eof: bool,
    pub truncated: bool,
}

#[derive(Debug)]
pub(crate) enum SourceError {
    /// 该来源不提供此能力（含尚未实现的适配器）。
    Unsupported,
    /// 来源此刻不可读（文件被占用、数据库被锁）；稍后重试。
    // seam-stub(adapters)：以下三个变体只由适配器构造；适配器落地后删除。
    #[allow(dead_code)]
    Unavailable,
    /// 来源内容不是预期格式；附说明，不 panic。
    #[allow(dead_code)]
    Malformed(String),
    #[allow(dead_code)]
    Io(std::io::Error),
}

impl SourceError {
    /// 对外的错误码与说明（`agent.activity.read` / `agent.external.list` 的应答）。
    fn code_and_message(&self) -> (&'static str, String) {
        match self {
            Self::Unsupported => (NOT_IMPLEMENTED_CODE, NOT_IMPLEMENTED_MESSAGE.into()),
            Self::Unavailable => (
                "activity_unavailable",
                "agent activity source is temporarily unavailable".into(),
            ),
            Self::Malformed(detail) => (
                "activity_malformed",
                format!("agent activity source is malformed: {detail}"),
            ),
            Self::Io(error) => ("activity_io_error", error.to_string()),
        }
    }
}

impl std::fmt::Display for SourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported => f.write_str("unsupported"),
            Self::Unavailable => f.write_str("unavailable"),
            Self::Malformed(detail) => write!(f, "malformed: {detail}"),
            Self::Io(error) => write!(f, "io: {error}"),
        }
    }
}

pub(crate) trait ActivitySource: Send + Sync {
    /// 来源 id，与 `source_for` 的 agent 名一致。
    fn id(&self) -> &'static str;

    /// 该会话下的活动节点。缺文件 / 缺字段一律降级为空或部分结果，绝不 panic。
    fn discover(&self, cx: &SourceContext<'_>) -> Result<Vec<AgentActivityNode>, SourceError>;

    /// 读一个节点的内容片段；`cursor` 对调用方不透明。
    fn read(
        &self,
        cx: &SourceContext<'_>,
        node_id: &str,
        cursor: Option<&str>,
        max_bytes: usize,
    ) -> Result<ContentChunk, SourceError>;

    /// 仅外部来源实现；pane 型来源用默认空实现。
    fn discover_external(
        &self,
        _home: &Path,
        _now_ms: u64,
    ) -> Result<Vec<ExternalAgentInfo>, SourceError> {
        Ok(Vec::new())
    }
}

/// 按规范化 agent 名找适配器。
pub(crate) fn source_for(agent: &str) -> Option<&'static dyn ActivitySource> {
    match agent {
        "claude" => Some(&claude::Claude),
        "codex" => Some(&codex::Codex),
        "kimi" => Some(&kimi::Kimi),
        "opencode" => Some(&opencode::OpenCode),
        "pi" => Some(&pi::Pi),
        "zcode" => Some(&zcode::ZCode),
        _ => None,
    }
}

/// 会给出外部条目（不属于任何 pane 的会话）的来源。
pub(crate) fn external_sources() -> &'static [&'static dyn ActivitySource] {
    &[&zcode::ZCode]
}

/// 需要 server 上下文、由本模块处理的方法。
pub(crate) fn handles(method: &Method) -> bool {
    matches!(
        method,
        Method::AgentActivityRead(_) | Method::AgentExternalList(_)
    )
}

/// 来源未实现（适配器回 `Unsupported`）时的应答。
pub(crate) const NOT_IMPLEMENTED_CODE: &str = "not_implemented";
pub(crate) const NOT_IMPLEMENTED_MESSAGE: &str = "agent activity is not implemented by this server";

// ---------------------------------------------------------------------------
// 调度参数
// ---------------------------------------------------------------------------

/// 收到钩子提示后，同一 pane 两次刷新的最小间隔（限频）。
pub(crate) const HINT_MIN_INTERVAL: Duration = Duration::from_secs(1);
/// agent 处于 Working 时的低频轮询间隔。
pub(crate) const WORKING_POLL_INTERVAL: Duration = Duration::from_secs(5);
/// 二级窗口跟随（`agent.activity.read` 带 `follow = true`）时的树刷新间隔。
pub(crate) const FOLLOW_INTERVAL: Duration = Duration::from_secs(1);
/// 一次跟随读取让该 pane 保持跟随的时长；客户端持续读取即持续跟随。
pub(crate) const FOLLOW_TTL: Duration = Duration::from_secs(10);
/// 外部来源的轮询间隔（只在有客户端连接时轮询）。
pub(crate) const EXTERNAL_POLL_INTERVAL: Duration = Duration::from_secs(10);
/// 后台任务的结果迟迟不回（事件丢失）时，视为已结束的超时。
pub(crate) const IN_FLIGHT_TIMEOUT: Duration = Duration::from_secs(30);
/// 调度遍历 pane 的最小间隔；有新提示或被推迟的提示到期时提前。
pub(crate) const SCHEDULER_PASS_INTERVAL: Duration = Duration::from_secs(1);
/// `agent.activity.read` 未给 `max_bytes` 时的内容上限。
pub(crate) const DEFAULT_READ_BYTES: usize = 64 * 1024;
/// `agent.activity.read` 的内容上限（大于它的 `max_bytes` 被压到它）。
pub(crate) const MAX_READ_BYTES: usize = 512 * 1024;
const JOB_QUEUE_CAPACITY: usize = 64;
/// 与 `client_commands` 的端点应答分块一致。
const ENDPOINT_RESPONSE_CHUNK_BYTES: usize = 512 * 1024;

// ---------------------------------------------------------------------------
// 调度（纯逻辑，时钟由调用方注入）
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct PaneSchedule {
    last_started: Option<Instant>,
    in_flight_since: Option<Instant>,
    hinted: bool,
    follow_until: Option<Instant>,
    was_working: bool,
    seen_pass: u64,
}

/// 活动树刷新调度：决定哪个 pane 何时交给后台发现。触发条件：
///
/// - 首次见到该 agent：立即发现一次；
/// - 钩子提示：立即刷，但同一 pane 两次刷新至少隔 [`HINT_MIN_INTERVAL`]；
/// - Working：每 [`WORKING_POLL_INTERVAL`] 轮询一次；Working 结束时补刷一次；
/// - 跟随：[`FOLLOW_TTL`] 内每 [`FOLLOW_INTERVAL`] 刷一次。
///
/// 同一 pane 同时最多一个在途任务；结果回来（或 [`IN_FLIGHT_TIMEOUT`] 过去）才放行
/// 下一次，期间到达的提示保留到放行后。
#[derive(Debug, Default)]
pub(crate) struct Scheduler {
    panes: HashMap<PaneId, PaneSchedule>,
    pass: u64,
    next_pass: Option<Instant>,
    external_last_started: Option<Instant>,
    external_in_flight_since: Option<Instant>,
}

impl Scheduler {
    pub(crate) fn note_hint(&mut self, pane_id: PaneId) {
        self.panes.entry(pane_id).or_default().hinted = true;
    }

    pub(crate) fn note_follow(&mut self, pane_id: PaneId, now: Instant) {
        self.panes.entry(pane_id).or_default().follow_until = Some(now + FOLLOW_TTL);
        self.next_pass = None;
    }

    /// 本轮是否该遍历 pane：有未取走的提示时立即，否则按下一次到期时刻。
    pub(crate) fn pass_due(&self, now: Instant, hints_pending: bool) -> bool {
        hints_pending || self.next_pass.is_none_or(|at| now >= at)
    }

    pub(crate) fn begin_pass(&mut self, now: Instant) {
        self.pass = self.pass.wrapping_add(1);
        self.next_pass = Some(now + SCHEDULER_PASS_INTERVAL);
    }

    /// 对一个持有 agent 的 pane 做决定；到期返回真并记为在途。
    pub(crate) fn should_refresh(&mut self, pane_id: PaneId, working: bool, now: Instant) -> bool {
        let pass = self.pass;
        let entry = self.panes.entry(pane_id).or_default();
        entry.seen_pass = pass;
        if entry.was_working && !working {
            // Working 结束：补刷一次，拿到任务的最终状态。
            entry.hinted = true;
        }
        entry.was_working = working;
        if entry
            .in_flight_since
            .is_some_and(|since| now.saturating_duration_since(since) < IN_FLIGHT_TIMEOUT)
        {
            return false;
        }
        entry.in_flight_since = None;
        let following = entry.follow_until.is_some_and(|until| now < until);
        if !following {
            entry.follow_until = None;
        }
        let due = match entry.last_started {
            None => true,
            Some(at) => {
                let elapsed = now.saturating_duration_since(at);
                (entry.hinted && elapsed >= HINT_MIN_INTERVAL)
                    || (following && elapsed >= FOLLOW_INTERVAL)
                    || (working && elapsed >= WORKING_POLL_INTERVAL)
            }
        };
        if due {
            entry.hinted = false;
            entry.last_started = Some(now);
            entry.in_flight_since = Some(now);
            return true;
        }
        if entry.hinted {
            // 被限频推迟的提示：到期时刻提前下一轮遍历。
            if let Some(at) = entry.last_started {
                let deadline = at + HINT_MIN_INTERVAL;
                self.next_pass = Some(self.next_pass.map_or(deadline, |next| next.min(deadline)));
            }
        }
        false
    }

    /// 结束一轮遍历：本轮没见到的 pane（已关闭 / 不再持有 agent / 没有来源适配器）
    /// 丢弃其调度记录。
    pub(crate) fn end_pass(&mut self) {
        let pass = self.pass;
        self.panes.retain(|_, entry| entry.seen_pass == pass);
    }

    /// 该 pane 的后台任务已结束（成功或失败）。期间到达过提示则让下一轮立即遍历。
    pub(crate) fn finish(&mut self, pane_id: PaneId) {
        if let Some(entry) = self.panes.get_mut(&pane_id) {
            entry.in_flight_since = None;
            if entry.hinted {
                self.next_pass = None;
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn in_flight(&self, pane_id: PaneId) -> bool {
        self.panes
            .get(&pane_id)
            .is_some_and(|entry| entry.in_flight_since.is_some())
    }

    /// 外部来源是否到了轮询时刻；到期返回真并记为在途。
    pub(crate) fn external_due(&mut self, now: Instant) -> bool {
        if self
            .external_in_flight_since
            .is_some_and(|since| now.saturating_duration_since(since) < IN_FLIGHT_TIMEOUT)
        {
            return false;
        }
        self.external_in_flight_since = None;
        let due = self
            .external_last_started
            .is_none_or(|at| now.saturating_duration_since(at) >= EXTERNAL_POLL_INTERVAL);
        if due {
            self.external_last_started = Some(now);
            self.external_in_flight_since = Some(now);
        }
        due
    }

    pub(crate) fn finish_external(&mut self) {
        self.external_in_flight_since = None;
    }
}

// ---------------------------------------------------------------------------
// 应答
// ---------------------------------------------------------------------------

/// 后台请求的应答通道。只在后台线程上使用（端点应答用阻塞发送）。
pub(crate) enum Reply {
    /// JSON API：请求自带的应答通道。
    Api(mpsc::Sender<String>),
    /// 客户端端点：经 server 事件通道转成 `ClientShellEndpointResponseChunk`。
    Endpoint {
        client_id: u64,
        boot_id: String,
        events: tokio::sync::mpsc::Sender<ServerEvent>,
    },
}

impl Reply {
    fn send(&self, id: &str, result: Result<ResponseResult, (&'static str, String)>) {
        let text = match result {
            Ok(result) => serde_json::to_string(&SuccessResponse {
                id: id.into(),
                result,
            }),
            Err((code, message)) => serde_json::to_string(&ErrorResponse {
                id: id.into(),
                error: ErrorBody {
                    code: code.into(),
                    message,
                },
            }),
        };
        let text = text.unwrap_or_else(|error| {
            crate::server::client_commands::error_response(
                id.into(),
                "serialization_error",
                error.to_string(),
            )
        });
        match self {
            Self::Api(sender) => {
                let _ = sender.send(text);
            }
            Self::Endpoint {
                client_id,
                boot_id,
                events,
            } => {
                let bytes = text.into_bytes();
                let count = bytes.len().div_ceil(ENDPOINT_RESPONSE_CHUNK_BYTES).max(1);
                for index in 0..count {
                    let start = index * ENDPOINT_RESPONSE_CHUNK_BYTES;
                    let end = (start + ENDPOINT_RESPONSE_CHUNK_BYTES).min(bytes.len());
                    let event = ServerEvent::ObservationResponse {
                        client_id: *client_id,
                        boot_id: boot_id.clone(),
                        message: crate::protocol::ServerMessage::ClientShellEndpointResponseChunk {
                            boot_id: boot_id.clone(),
                            request_id: id.into(),
                            final_chunk: index + 1 == count,
                            data: bytes[start..end].to_vec(),
                        },
                    };
                    if events.blocking_send(event).is_err() {
                        return;
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 后台线程
// ---------------------------------------------------------------------------

/// 注入点：测试用假适配器替换注册表。
#[derive(Clone, Copy)]
pub(crate) struct Sources {
    pub source_for: fn(&str) -> Option<&'static dyn ActivitySource>,
    pub external: fn() -> &'static [&'static dyn ActivitySource],
}

impl Sources {
    pub(crate) const REGISTERED: Self = Self {
        source_for,
        external: external_sources,
    };
}

enum ReadTarget {
    Pane {
        pane_id: PaneId,
        subject: AgentActivitySubject,
    },
    External {
        session: crate::agent_resume::AgentSessionRef,
    },
}

struct ReadJob {
    request_id: String,
    source: &'static dyn ActivitySource,
    target: ReadTarget,
    node_id: Option<String>,
    cursor: Option<String>,
    max_bytes: usize,
    reply: Reply,
}

enum Job {
    Discover {
        pane_id: PaneId,
        source: &'static dyn ActivitySource,
        subject: AgentActivitySubject,
    },
    DiscoverExternal {
        sources: &'static [&'static dyn ActivitySource],
    },
    Read(Box<ReadJob>),
    ExternalList {
        request_id: String,
        sources: &'static [&'static dyn ActivitySource],
        reply: Reply,
    },
}

struct Runtime {
    jobs: mpsc::SyncSender<Job>,
}

impl Runtime {
    fn start(
        events: tokio::sync::mpsc::Sender<AppEvent>,
        home: Option<PathBuf>,
    ) -> std::io::Result<Self> {
        let (jobs, queue) = mpsc::sync_channel(JOB_QUEUE_CAPACITY);
        std::thread::Builder::new()
            .name("herdr-agent-activity".into())
            .spawn(move || {
                let worker = Worker { events, home };
                while let Ok(job) = queue.recv() {
                    if !worker.run(job) {
                        break;
                    }
                }
            })?;
        Ok(Self { jobs })
    }

    fn submit(&self, job: Job) -> Result<(), Job> {
        self.jobs.try_send(job).map_err(|error| match error {
            mpsc::TrySendError::Full(job) | mpsc::TrySendError::Disconnected(job) => job,
        })
    }
}

struct Worker {
    events: tokio::sync::mpsc::Sender<AppEvent>,
    /// `None` = 取系统 home；测试注入临时目录。
    home: Option<PathBuf>,
}

impl Worker {
    /// 执行一个任务。返回 false 表示 app 事件通道已关闭（server 退出），线程随之结束。
    fn run(&self, job: Job) -> bool {
        let Some(home) = self.home.clone().or_else(home_dir) else {
            return self.run_without_home(job);
        };
        let now_ms = crate::server::observability::now_ms();
        match job {
            Job::Discover {
                pane_id,
                source,
                subject,
            } => {
                let result = source
                    .discover(&pane_context(&subject, &home, now_ms))
                    .map_err(|error| error.to_string());
                self.send_event(AppEvent::AgentActivityRefreshed { pane_id, result })
            }
            Job::DiscoverExternal { sources } => {
                for source in sources {
                    let result = source
                        .discover_external(&home, now_ms)
                        .map_err(|error| error.to_string());
                    if !self.send_event(AppEvent::ExternalAgentsRefreshed {
                        source: source.id().to_owned(),
                        result,
                    }) {
                        return false;
                    }
                }
                true
            }
            Job::Read(job) => self.read(*job, &home, now_ms),
            Job::ExternalList {
                request_id,
                sources,
                reply,
            } => {
                let mut agents = Vec::new();
                let mut first_error: Option<SourceError> = None;
                let mut any_supported = false;
                for source in sources {
                    match source.discover_external(&home, now_ms) {
                        Ok(found) => {
                            any_supported = true;
                            agents.extend(found.iter().cloned().map(|mut agent| {
                                agent.source = source.id().to_owned();
                                agent
                            }));
                            if !self.send_event(AppEvent::ExternalAgentsRefreshed {
                                source: source.id().to_owned(),
                                result: Ok(found),
                            }) {
                                return false;
                            }
                        }
                        Err(SourceError::Unsupported) => {}
                        Err(error) => {
                            any_supported = true;
                            if first_error.is_none() {
                                first_error = Some(error);
                            }
                        }
                    }
                }
                agents.sort_by(|left, right| {
                    left.source
                        .cmp(&right.source)
                        .then_with(|| left.external_id.cmp(&right.external_id))
                });
                let result = if !any_supported {
                    Err(SourceError::Unsupported.code_and_message())
                } else if agents.is_empty() && first_error.is_some() {
                    Err(first_error
                        .map(|error| error.code_and_message())
                        .unwrap_or_else(|| SourceError::Unavailable.code_and_message()))
                } else {
                    Ok(ResponseResult::ExternalAgentList { agents })
                };
                reply.send(&request_id, result);
                true
            }
        }
    }

    /// 取不到 home 目录：发现任务按失败回报（释放在途名额），请求回 `activity_unavailable`。
    fn run_without_home(&self, job: Job) -> bool {
        const NO_HOME: &str = "home directory is not available";
        match job {
            Job::Discover { pane_id, .. } => self.send_event(AppEvent::AgentActivityRefreshed {
                pane_id,
                result: Err(NO_HOME.into()),
            }),
            Job::DiscoverExternal { sources } => sources.iter().all(|source| {
                self.send_event(AppEvent::ExternalAgentsRefreshed {
                    source: source.id().to_owned(),
                    result: Err(NO_HOME.into()),
                })
            }),
            Job::Read(job) => {
                job.reply.send(
                    &job.request_id,
                    Err(("activity_unavailable", NO_HOME.into())),
                );
                true
            }
            Job::ExternalList {
                request_id, reply, ..
            } => {
                reply.send(&request_id, Err(("activity_unavailable", NO_HOME.into())));
                true
            }
        }
    }

    fn read(&self, job: ReadJob, home: &Path, now_ms: u64) -> bool {
        let ReadJob {
            request_id,
            source,
            target,
            node_id,
            cursor,
            max_bytes,
            reply,
        } = job;
        let external_agent = source.id();
        let cx = match &target {
            ReadTarget::Pane { subject, .. } => pane_context(subject, home, now_ms),
            ReadTarget::External { session } => SourceContext {
                agent: external_agent,
                session: Some(session),
                cwd: None,
                home,
                now_ms,
            },
        };
        let result = match node_id {
            None => source.discover(&cx).map(|nodes| (nodes, None)),
            Some(node_id) => source
                .read(&cx, &node_id, cursor.as_deref(), max_bytes)
                .map(|chunk| {
                    let content = AgentActivityContent {
                        node_id,
                        format: chunk.format,
                        text: chunk.text,
                        eof: chunk.eof,
                        truncated: chunk.truncated,
                        next_cursor: chunk.next_cursor,
                    };
                    (Vec::new(), Some(content))
                }),
        };
        let mut alive = true;
        if let (ReadTarget::Pane { pane_id, .. }, Ok((nodes, None))) = (&target, &result) {
            // 读整棵树顺带刷新落库，让快照与本次应答一致。
            alive = self.send_event(AppEvent::AgentActivityRefreshed {
                pane_id: *pane_id,
                result: Ok(nodes.clone()),
            });
        }
        reply.send(
            &request_id,
            result
                .map(|(nodes, content)| ResponseResult::AgentActivity { nodes, content })
                .map_err(|error| error.code_and_message()),
        );
        alive
    }

    fn send_event(&self, event: AppEvent) -> bool {
        self.events.blocking_send(event).is_ok()
    }
}

fn pane_context<'a>(
    subject: &'a AgentActivitySubject,
    home: &'a Path,
    now_ms: u64,
) -> SourceContext<'a> {
    SourceContext {
        agent: &subject.agent,
        session: subject.session.as_ref(),
        cwd: subject.cwd.as_deref(),
        home,
        now_ms,
    }
}

/// 适配器读取用的 home 目录（不依赖平台 cfg：Unix 取 `HOME`，Windows 取
/// `USERPROFILE`）。
fn home_dir() -> Option<PathBuf> {
    ["HOME", "USERPROFILE"]
        .into_iter()
        .filter_map(std::env::var_os)
        .find(|value| !value.is_empty())
        .map(PathBuf::from)
}

// ---------------------------------------------------------------------------
// 服务：server 主循环持有
// ---------------------------------------------------------------------------

/// server 主循环持有的活动树服务：调度 + 惰性启动的后台线程 + 投影脏标记。
pub(crate) struct Service {
    scheduler: Scheduler,
    runtime: Option<Runtime>,
    events: tokio::sync::mpsc::Sender<AppEvent>,
    sources: Sources,
    home: Option<PathBuf>,
    /// 活动树 / 外部条目落库后投影变了、尚未随快照下发。主循环据此安排一次只刷
    /// 投影的渲染，真正同步到客户端后才清除（渲染被节流推迟时不会丢）。
    projection_dirty: bool,
}

impl Service {
    pub(crate) fn new(events: tokio::sync::mpsc::Sender<AppEvent>) -> Self {
        Self::with_sources(events, Sources::REGISTERED, None)
    }

    pub(crate) fn with_sources(
        events: tokio::sync::mpsc::Sender<AppEvent>,
        sources: Sources,
        home: Option<PathBuf>,
    ) -> Self {
        Self {
            scheduler: Scheduler::default(),
            runtime: None,
            events,
            sources,
            home,
            projection_dirty: false,
        }
    }

    /// 调度一轮（主循环每轮调用；内部按 [`SCHEDULER_PASS_INTERVAL`] 限流，有新提示
    /// 时立即）：取走提示、挑出到期的 pane 提交后台发现、按需轮询外部来源、做过期
    /// 清理。`external_demand` 为假（没有客户端连接）时不轮询外部来源。
    pub(crate) fn tick(
        &mut self,
        state: &mut crate::app::state::AppState,
        now: Instant,
        external_demand: bool,
    ) {
        if !self
            .scheduler
            .pass_due(now, state.agent_activity.has_hints())
        {
            return;
        }
        let scheduler = &mut self.scheduler;
        state
            .agent_activity
            .drain_hints(|pane_id| scheduler.note_hint(pane_id));
        scheduler.begin_pass(now);
        let source_for = self.sources.source_for;
        let mut due = Vec::new();
        state.for_each_agent_pane(|pane_id, agent, working| {
            if let Some(source) = source_for(agent) {
                if scheduler.should_refresh(pane_id, working, now) {
                    due.push((pane_id, source));
                }
            }
        });
        scheduler.end_pass();
        for (pane_id, source) in due {
            let submitted = state
                .agent_activity_subject(pane_id)
                .is_some_and(|subject| {
                    self.submit(Job::Discover {
                        pane_id,
                        source,
                        subject,
                    })
                    .is_ok()
                });
            if !submitted {
                self.scheduler.finish(pane_id);
            }
        }
        let external = (self.sources.external)();
        if external_demand && !external.is_empty() && self.scheduler.external_due(now) {
            let submitted = self
                .submit(Job::DiscoverExternal { sources: external })
                .is_ok();
            if !submitted {
                self.scheduler.finish_external();
            }
        }
        if state.expire_agent_activity(now) {
            self.projection_dirty = true;
        }
    }

    /// 一个 pane 的后台发现结果已回主线程；`projection_changed` 为落库后投影是否变化。
    pub(crate) fn pane_refreshed(&mut self, pane_id: PaneId, projection_changed: bool) {
        self.scheduler.finish(pane_id);
        self.projection_dirty |= projection_changed;
    }

    /// 一个外部来源的后台发现结果已回主线程。
    pub(crate) fn external_refreshed(&mut self, projection_changed: bool) {
        self.scheduler.finish_external();
        self.projection_dirty |= projection_changed;
    }

    pub(crate) fn projection_dirty(&self) -> bool {
        self.projection_dirty
    }

    /// 投影已同步给客户端（快照重建路径跑过）。
    pub(crate) fn projection_synced(&mut self) {
        self.projection_dirty = false;
    }

    /// 受理 `agent.activity.read` / `agent.external.list`：主线程校验参数并解析
    /// 目标，读取在后台线程执行，经 `reply` 异步应答。返回错误时调用方自行同步
    /// 应答（此时 `reply` 已被丢弃）。
    pub(crate) fn submit_request(
        &mut self,
        app: &crate::app::App,
        request: Request,
        reply: Reply,
        now: Instant,
    ) -> Result<(), (&'static str, String)> {
        let job = match request.method {
            Method::AgentActivityRead(params) => {
                if params.node_id.as_deref().is_some_and(str::is_empty) {
                    return Err(("invalid_params", "node_id must not be empty".into()));
                }
                if params.max_bytes == Some(0) {
                    return Err(("invalid_params", "max_bytes must be positive".into()));
                }
                let max_bytes = params
                    .max_bytes
                    .map_or(DEFAULT_READ_BYTES, |bytes| {
                        usize::try_from(bytes).unwrap_or(MAX_READ_BYTES)
                    })
                    .min(MAX_READ_BYTES);
                let (source, target) = match (params.pane_id, params.external_id) {
                    (Some(pane), None) => self.resolve_pane_target(app, &pane)?,
                    (None, Some(external_id)) => self.resolve_external_target(&external_id)?,
                    _ => {
                        return Err((
                            "invalid_params",
                            "exactly one of pane_id or external_id is required".into(),
                        ))
                    }
                };
                if params.follow {
                    if let ReadTarget::Pane { pane_id, .. } = &target {
                        self.scheduler.note_follow(*pane_id, now);
                    }
                }
                Job::Read(Box::new(ReadJob {
                    request_id: request.id,
                    source,
                    target,
                    node_id: params.node_id,
                    cursor: params.cursor,
                    max_bytes,
                    reply,
                }))
            }
            Method::AgentExternalList(_) => Job::ExternalList {
                request_id: request.id,
                sources: (self.sources.external)(),
                reply,
            },
            _ => return Err(("unsupported_method", "not an agent activity method".into())),
        };
        self.submit(job).map_err(|error| match error {
            SubmitError::Busy => (
                "server_busy",
                "agent activity reader is busy, retry later".into(),
            ),
            SubmitError::Unavailable(message) => ("server_unavailable", message),
        })
    }

    fn resolve_pane_target(
        &self,
        app: &crate::app::App,
        pane: &str,
    ) -> Result<(&'static dyn ActivitySource, ReadTarget), (&'static str, String)> {
        let (_, pane_id) = app
            .parse_pane_id(pane)
            .ok_or_else(|| ("pane_not_found", format!("pane {pane} not found")))?;
        let subject = app.state.agent_activity_subject(pane_id).ok_or_else(|| {
            (
                "agent_not_found",
                format!("pane {pane} does not currently host an agent"),
            )
        })?;
        let source = (self.sources.source_for)(&subject.agent)
            .ok_or_else(|| (NOT_IMPLEMENTED_CODE, NOT_IMPLEMENTED_MESSAGE.to_owned()))?;
        Ok((source, ReadTarget::Pane { pane_id, subject }))
    }

    fn resolve_external_target(
        &self,
        external_id: &str,
    ) -> Result<(&'static dyn ActivitySource, ReadTarget), (&'static str, String)> {
        let (source_id, session_id) = external_id
            .split_once(':')
            .filter(|(source, session)| !source.is_empty() && !session.is_empty())
            .ok_or_else(|| {
                (
                    "invalid_params",
                    "external_id must look like <source>:<session id>".to_owned(),
                )
            })?;
        let source = (self.sources.external)()
            .iter()
            .copied()
            .find(|source| source.id() == source_id)
            .ok_or_else(|| {
                (
                    "agent_not_found",
                    format!("external source {source_id} is not known to this server"),
                )
            })?;
        let session = crate::agent_resume::AgentSessionRef::id(session_id).ok_or_else(|| {
            (
                "invalid_params",
                "external_id carries an invalid session id".to_owned(),
            )
        })?;
        Ok((source, ReadTarget::External { session }))
    }

    fn submit(&mut self, job: Job) -> Result<(), SubmitError> {
        if self.runtime.is_none() {
            match Runtime::start(self.events.clone(), self.home.clone()) {
                Ok(runtime) => self.runtime = Some(runtime),
                Err(error) => {
                    tracing::warn!(%error, "failed to start agent activity worker");
                    return Err(SubmitError::Unavailable(format!(
                        "failed to start agent activity worker: {error}"
                    )));
                }
            }
        }
        let Some(runtime) = &self.runtime else {
            return Err(SubmitError::Unavailable(
                "agent activity worker is not running".into(),
            ));
        };
        match runtime.submit(job) {
            Ok(()) => Ok(()),
            Err(_job) => {
                tracing::debug!("agent activity queue is full; dropping job");
                Err(SubmitError::Busy)
            }
        }
    }
}

enum SubmitError {
    Busy,
    Unavailable(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::{AgentActivityKind, AgentActivityStatus, AgentStatus};
    use crate::detect::{Agent, AgentState};

    #[test]
    fn every_registered_source_answers_to_its_own_id() {
        for agent in ["claude", "codex", "kimi", "opencode", "pi", "zcode"] {
            let source = source_for(agent).expect("已预注册的来源");
            assert_eq!(source.id(), agent);
        }
        assert!(source_for("unknown-agent").is_none());
        assert_eq!(
            external_sources()
                .iter()
                .map(|source| source.id())
                .collect::<Vec<_>>(),
            ["zcode"]
        );
    }

    #[test]
    fn stub_sources_report_unsupported_without_panicking() {
        let home = std::env::temp_dir();
        for agent in ["claude", "codex", "kimi", "opencode", "pi", "zcode"] {
            let source = source_for(agent).expect("已预注册的来源");
            let cx = SourceContext {
                agent,
                session: None,
                cwd: None,
                home: &home,
                now_ms: 0,
            };
            assert!(matches!(
                source.discover(&cx),
                Err(SourceError::Unsupported)
            ));
            assert!(matches!(
                source.read(&cx, "node", None, 1024),
                Err(SourceError::Unsupported)
            ));
        }
    }

    #[test]
    fn handles_only_the_server_context_activity_methods() {
        use crate::api::schema::{AgentActivityReadParams, EmptyParams};
        assert!(handles(&Method::AgentActivityRead(
            AgentActivityReadParams::default()
        )));
        assert!(handles(&Method::AgentExternalList(EmptyParams::default())));
        assert!(!handles(&Method::AgentList(EmptyParams::default())));
    }

    // -----------------------------------------------------------------------
    // 调度：时钟全部由测试注入（固定起点 + 偏移），不 sleep。
    // -----------------------------------------------------------------------

    fn pane(raw: u32) -> PaneId {
        PaneId::from_raw(raw)
    }

    fn secs(value: f64) -> Duration {
        Duration::from_secs_f64(value)
    }

    /// 走一轮完整遍历（begin → 逐 pane 决定 → end），返回到期的 pane。
    fn pass(scheduler: &mut Scheduler, now: Instant, panes: &[(PaneId, bool)]) -> Vec<PaneId> {
        scheduler.begin_pass(now);
        let due = panes
            .iter()
            .filter(|(pane_id, working)| scheduler.should_refresh(*pane_id, *working, now))
            .map(|(pane_id, _)| *pane_id)
            .collect();
        scheduler.end_pass();
        due
    }

    #[test]
    fn scheduler_discovers_once_on_first_sight_then_waits_for_a_trigger() {
        let t0 = Instant::now();
        let mut scheduler = Scheduler::default();
        let idle = pane(1);
        assert_eq!(pass(&mut scheduler, t0, &[(idle, false)]), [idle]);
        scheduler.finish(idle);
        // 空闲、无提示、无跟随：之后不再刷新。
        for offset in [1.0, 5.0, 60.0] {
            assert!(pass(&mut scheduler, t0 + secs(offset), &[(idle, false)]).is_empty());
        }
    }

    #[test]
    fn scheduler_rate_limits_hints_to_one_refresh_per_second() {
        let t0 = Instant::now();
        let mut scheduler = Scheduler::default();
        let agent = pane(1);
        assert_eq!(pass(&mut scheduler, t0, &[(agent, false)]), [agent]);
        scheduler.finish(agent);

        // 0.3 s 后的提示被限频推迟，并把下一轮遍历提前到 1 s 到期时刻。
        scheduler.note_hint(agent);
        assert!(pass(&mut scheduler, t0 + secs(0.3), &[(agent, false)]).is_empty());
        assert!(!scheduler.pass_due(t0 + secs(0.9), false));
        assert!(scheduler.pass_due(t0 + secs(1.0), false));
        assert_eq!(
            pass(&mut scheduler, t0 + secs(1.0), &[(agent, false)]),
            [agent]
        );
        scheduler.finish(agent);
        // 提示已消费：没有新提示就不再刷。
        assert!(pass(&mut scheduler, t0 + secs(2.5), &[(agent, false)]).is_empty());

        // 间隔已满 1 s 的提示立即刷。
        scheduler.note_hint(agent);
        assert!(scheduler.pass_due(t0 + secs(2.6), true));
        assert_eq!(
            pass(&mut scheduler, t0 + secs(2.6), &[(agent, false)]),
            [agent]
        );
    }

    #[test]
    fn scheduler_polls_working_agents_every_five_seconds_only() {
        let t0 = Instant::now();
        let mut scheduler = Scheduler::default();
        let working = pane(1);
        let idle = pane(2);
        let panes = [(working, true), (idle, false)];
        assert_eq!(pass(&mut scheduler, t0, &panes), [working, idle]);
        scheduler.finish(working);
        scheduler.finish(idle);
        assert!(pass(&mut scheduler, t0 + secs(4.9), &panes).is_empty());
        assert_eq!(pass(&mut scheduler, t0 + secs(5.0), &panes), [working]);
        scheduler.finish(working);
        assert!(pass(&mut scheduler, t0 + secs(9.0), &panes).is_empty());
        assert_eq!(pass(&mut scheduler, t0 + secs(10.0), &panes), [working]);
    }

    #[test]
    fn scheduler_refreshes_once_more_when_work_ends() {
        let t0 = Instant::now();
        let mut scheduler = Scheduler::default();
        let agent = pane(1);
        assert_eq!(pass(&mut scheduler, t0, &[(agent, true)]), [agent]);
        scheduler.finish(agent);
        // Working → Idle：补刷一次拿最终状态（仍受 1 s 限频）。
        assert!(pass(&mut scheduler, t0 + secs(0.5), &[(agent, false)]).is_empty());
        assert_eq!(
            pass(&mut scheduler, t0 + secs(1.0), &[(agent, false)]),
            [agent]
        );
        scheduler.finish(agent);
        assert!(pass(&mut scheduler, t0 + secs(7.0), &[(agent, false)]).is_empty());
    }

    #[test]
    fn scheduler_keeps_one_job_in_flight_per_pane_and_replays_hints_after_it() {
        let t0 = Instant::now();
        let mut scheduler = Scheduler::default();
        let agent = pane(1);
        assert_eq!(pass(&mut scheduler, t0, &[(agent, true)]), [agent]);
        assert!(scheduler.in_flight(agent));
        // 在途期间：提示与轮询都不重复提交。
        scheduler.note_hint(agent);
        assert!(pass(&mut scheduler, t0 + secs(6.0), &[(agent, true)]).is_empty());
        // 结果回来：保留的提示让下一轮立即遍历并刷新。
        scheduler.finish(agent);
        assert!(!scheduler.in_flight(agent));
        assert!(scheduler.pass_due(t0 + secs(6.1), false));
        assert_eq!(
            pass(&mut scheduler, t0 + secs(6.1), &[(agent, true)]),
            [agent]
        );
        // 结果事件丢失：超时后放行。
        assert!(pass(&mut scheduler, t0 + secs(20.0), &[(agent, true)]).is_empty());
        assert_eq!(
            pass(
                &mut scheduler,
                t0 + secs(6.1) + IN_FLIGHT_TIMEOUT,
                &[(agent, true)]
            ),
            [agent]
        );
    }

    #[test]
    fn scheduler_follows_a_pane_every_second_until_the_ttl_lapses() {
        let t0 = Instant::now();
        let mut scheduler = Scheduler::default();
        let agent = pane(1);
        assert_eq!(pass(&mut scheduler, t0, &[(agent, false)]), [agent]);
        scheduler.finish(agent);
        scheduler.note_follow(agent, t0 + secs(0.5));
        assert!(scheduler.pass_due(t0 + secs(0.5), false));
        let mut refreshed = Vec::new();
        for tenth in 5..=140 {
            let now = t0 + secs(f64::from(tenth) / 10.0);
            if !pass(&mut scheduler, now, &[(agent, false)]).is_empty() {
                refreshed.push(tenth);
                scheduler.finish(agent);
            }
        }
        // 跟随窗口 0.5 s + 10 s：每满 1 s 刷一次，窗口过后停止。
        assert_eq!(refreshed, [10, 20, 30, 40, 50, 60, 70, 80, 90, 100]);
    }

    #[test]
    fn scheduler_forgets_panes_that_no_longer_host_an_agent() {
        let t0 = Instant::now();
        let mut scheduler = Scheduler::default();
        let agent = pane(1);
        assert_eq!(pass(&mut scheduler, t0, &[(agent, false)]), [agent]);
        assert!(pass(&mut scheduler, t0 + secs(1.0), &[]).is_empty());
        assert!(!scheduler.in_flight(agent));
        // 重新出现的 agent 视为首次发现。
        assert_eq!(
            pass(&mut scheduler, t0 + secs(2.0), &[(agent, false)]),
            [agent]
        );
    }

    #[test]
    fn scheduler_polls_external_sources_on_their_own_cadence() {
        let t0 = Instant::now();
        let mut scheduler = Scheduler::default();
        assert!(scheduler.external_due(t0));
        assert!(!scheduler.external_due(t0 + secs(11.0)), "在途");
        scheduler.finish_external();
        assert!(scheduler.external_due(t0 + secs(11.0)));
        scheduler.finish_external();
        assert!(!scheduler.external_due(t0 + secs(20.0)));
        assert!(scheduler.external_due(t0 + secs(21.0)));
    }

    // -----------------------------------------------------------------------
    // 后台线程与请求受理：假适配器
    // -----------------------------------------------------------------------

    fn node(id: &str, parent: Option<&str>, status: AgentActivityStatus) -> AgentActivityNode {
        AgentActivityNode {
            id: id.into(),
            kind: AgentActivityKind::Subagent,
            label: format!("task {id}"),
            status,
            parent_id: parent.map(str::to_owned),
            ..AgentActivityNode::default()
        }
    }

    /// 能读的假来源：树两层，节点内容回显调用参数。
    struct FakeTree;

    impl ActivitySource for FakeTree {
        fn id(&self) -> &'static str {
            "claude"
        }

        fn discover(&self, cx: &SourceContext<'_>) -> Result<Vec<AgentActivityNode>, SourceError> {
            assert_eq!(cx.agent, "claude");
            Ok(vec![
                node("a", None, AgentActivityStatus::Running),
                node("a.1", Some("a"), AgentActivityStatus::Done),
            ])
        }

        fn read(
            &self,
            cx: &SourceContext<'_>,
            node_id: &str,
            cursor: Option<&str>,
            max_bytes: usize,
        ) -> Result<ContentChunk, SourceError> {
            if node_id == "missing" {
                return Err(SourceError::Malformed("no such node".into()));
            }
            Ok(ContentChunk {
                format: AgentActivityContentFormat::Text,
                text: format!(
                    "{}|{node_id}|{}|{max_bytes}",
                    cx.session.map_or("-", |session| session.value.as_str()),
                    cursor.unwrap_or("-")
                ),
                next_cursor: Some("next".into()),
                eof: false,
                truncated: false,
            })
        }
    }

    /// 外部来源：一个会话，内容读取回显会话 id。
    struct FakeExternal;

    impl ActivitySource for FakeExternal {
        fn id(&self) -> &'static str {
            "zcode"
        }

        fn discover(&self, cx: &SourceContext<'_>) -> Result<Vec<AgentActivityNode>, SourceError> {
            let session = cx.session.map(|session| session.value.clone());
            Ok(vec![node(
                &session.unwrap_or_default(),
                None,
                AgentActivityStatus::Running,
            )])
        }

        fn read(
            &self,
            _cx: &SourceContext<'_>,
            _node_id: &str,
            _cursor: Option<&str>,
            _max_bytes: usize,
        ) -> Result<ContentChunk, SourceError> {
            Err(SourceError::Unsupported)
        }

        fn discover_external(
            &self,
            _home: &Path,
            now_ms: u64,
        ) -> Result<Vec<ExternalAgentInfo>, SourceError> {
            Ok(vec![ExternalAgentInfo {
                external_id: "zcode:s-1".into(),
                source: String::new(),
                agent_status: AgentStatus::Working,
                label: "external session".into(),
                readable: true,
                agent: None,
                cwd: None,
                updated_at_ms: Some(now_ms),
                activity: Vec::new(),
            }])
        }
    }

    fn fake_source_for(agent: &str) -> Option<&'static dyn ActivitySource> {
        match agent {
            "claude" => Some(&FakeTree),
            // 已注册但未实现：走真实的空壳。
            "codex" => source_for("codex"),
            _ => None,
        }
    }

    fn fake_external() -> &'static [&'static dyn ActivitySource] {
        &[&FakeExternal]
    }

    const FAKE_SOURCES: Sources = Sources {
        source_for: fake_source_for,
        external: fake_external,
    };

    fn fake_service() -> (Service, tokio::sync::mpsc::Receiver<AppEvent>) {
        let (events, received) = tokio::sync::mpsc::channel(32);
        let service = Service::with_sources(events, FAKE_SOURCES, Some(std::env::temp_dir()));
        (service, received)
    }

    fn recv_event(received: &mut tokio::sync::mpsc::Receiver<AppEvent>) -> AppEvent {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match received.try_recv() {
                Ok(event) => return event,
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("没有等到活动事件：{error:?}"),
            }
        }
    }

    fn app_with_agent(agent: Option<Agent>) -> (crate::app::App, PaneId, String) {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = crate::app::App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state
            .workspaces
            .push(crate::workspace::Workspace::test_new("activity"));
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        if let Some(agent) = agent {
            app.handle_internal_event(AppEvent::StateChanged {
                pane_id,
                agent: Some(agent),
                state: AgentState::Working,
                visible_blocker: false,
                visible_working: false,
                process_exited: false,
                observed_at: Instant::now(),
            });
        }
        let public = app.public_pane_id(0, pane_id).expect("公开 pane id");
        (app, pane_id, public)
    }

    fn read_request(params: crate::api::schema::AgentActivityReadParams) -> Request {
        Request {
            id: "req-1".into(),
            method: Method::AgentActivityRead(params),
        }
    }

    fn submit_api(
        service: &mut Service,
        app: &crate::app::App,
        request: Request,
    ) -> Result<serde_json::Value, (&'static str, String)> {
        let (sender, replies) = mpsc::channel();
        service.submit_request(app, request, Reply::Api(sender), Instant::now())?;
        let text = replies
            .recv_timeout(Duration::from_secs(5))
            .expect("后台线程应答");
        Ok(serde_json::from_str(&text).expect("应答是 JSON"))
    }

    #[test]
    fn tick_submits_discovery_and_the_result_comes_back_as_an_app_event() {
        let (mut service, mut received) = fake_service();
        let (mut app, pane_id, _) = app_with_agent(Some(Agent::Claude));
        let t0 = Instant::now();
        service.tick(&mut app.state, t0, false);
        let AppEvent::AgentActivityRefreshed {
            pane_id: refreshed,
            result,
        } = recv_event(&mut received)
        else {
            panic!("应为活动树刷新事件");
        };
        assert_eq!(refreshed, pane_id);
        assert_eq!(
            result
                .expect("假来源可读")
                .iter()
                .map(|node| node.id.as_str())
                .collect::<Vec<_>>(),
            ["a", "a.1"]
        );
        // 结果未回主线程前（在途）不重复提交，即使有提示。
        app.state.agent_activity.note_hint(pane_id);
        service.tick(&mut app.state, t0 + secs(2.0), false);
        assert!(received.try_recv().is_err());
        assert!(!app.state.agent_activity.has_hints(), "提示已被取走");
        // 回主线程后，保留的提示触发下一次刷新。
        service.pane_refreshed(pane_id, true);
        assert!(service.projection_dirty());
        service.tick(&mut app.state, t0 + secs(2.1), false);
        assert!(matches!(
            recv_event(&mut received),
            AppEvent::AgentActivityRefreshed { .. }
        ));
    }

    #[test]
    fn tick_skips_panes_without_an_agent_or_without_a_source() {
        let (mut service, mut received) = fake_service();
        let (mut shell, _, _) = app_with_agent(None);
        service.tick(&mut shell.state, Instant::now(), false);
        // pi 没有（假）来源适配器：不提交。
        let (mut pi, _, _) = app_with_agent(Some(Agent::Pi));
        service.tick(&mut pi.state, Instant::now() + secs(1.0), false);
        std::thread::sleep(Duration::from_millis(50));
        assert!(received.try_recv().is_err());
    }

    #[test]
    fn tick_polls_external_sources_only_with_demand() {
        let (mut service, mut received) = fake_service();
        let (mut app, _, _) = app_with_agent(None);
        let t0 = Instant::now();
        service.tick(&mut app.state, t0, false);
        std::thread::sleep(Duration::from_millis(50));
        assert!(received.try_recv().is_err(), "无客户端时不轮询外部来源");
        service.tick(&mut app.state, t0 + secs(1.0), true);
        let AppEvent::ExternalAgentsRefreshed { source, result } = recv_event(&mut received) else {
            panic!("应为外部来源刷新事件");
        };
        assert_eq!(source, "zcode");
        assert_eq!(result.expect("假外部来源可读")[0].external_id, "zcode:s-1");
    }

    #[test]
    fn activity_read_validates_its_target_before_touching_the_worker() {
        use crate::api::schema::AgentActivityReadParams;
        let (mut service, _received) = fake_service();
        let (app, _, public) = app_with_agent(Some(Agent::Claude));
        let (shell, _, shell_pane) = app_with_agent(None);
        let cases: Vec<(&crate::app::App, AgentActivityReadParams, &str)> = vec![
            (&app, AgentActivityReadParams::default(), "invalid_params"),
            (
                &app,
                AgentActivityReadParams {
                    pane_id: Some(public.clone()),
                    external_id: Some("zcode:s-1".into()),
                    ..AgentActivityReadParams::default()
                },
                "invalid_params",
            ),
            (
                &app,
                AgentActivityReadParams {
                    pane_id: Some(public.clone()),
                    node_id: Some(String::new()),
                    ..AgentActivityReadParams::default()
                },
                "invalid_params",
            ),
            (
                &app,
                AgentActivityReadParams {
                    pane_id: Some(public.clone()),
                    max_bytes: Some(0),
                    ..AgentActivityReadParams::default()
                },
                "invalid_params",
            ),
            (
                &app,
                AgentActivityReadParams {
                    external_id: Some("no-colon".into()),
                    ..AgentActivityReadParams::default()
                },
                "invalid_params",
            ),
            (
                &app,
                AgentActivityReadParams {
                    pane_id: Some("w999:p999".into()),
                    ..AgentActivityReadParams::default()
                },
                "pane_not_found",
            ),
            (
                &shell,
                AgentActivityReadParams {
                    pane_id: Some(shell_pane),
                    ..AgentActivityReadParams::default()
                },
                "agent_not_found",
            ),
            (
                &app,
                AgentActivityReadParams {
                    external_id: Some("gemini:s-1".into()),
                    ..AgentActivityReadParams::default()
                },
                "agent_not_found",
            ),
        ];
        for (app, params, code) in cases {
            let described = format!("{params:?}");
            let (sender, _replies) = mpsc::channel();
            let error = service
                .submit_request(
                    app,
                    read_request(params),
                    Reply::Api(sender),
                    Instant::now(),
                )
                .expect_err("非法参数必须在主线程拒绝");
            assert_eq!(error.0, code, "{described}");
        }
        assert!(service.runtime.is_none(), "拒绝的请求不启动后台线程");

        // 合法输入被受理（防「全部拒绝」的假安全）。
        let response = submit_api(
            &mut service,
            &app,
            read_request(AgentActivityReadParams {
                pane_id: Some(public),
                ..AgentActivityReadParams::default()
            }),
        )
        .expect("合法请求被受理");
        assert_eq!(response["result"]["type"], "agent_activity");
    }

    #[test]
    fn activity_read_returns_the_tree_or_one_node_and_refreshes_the_store() {
        use crate::api::schema::AgentActivityReadParams;
        let (mut service, mut received) = fake_service();
        let (app, pane_id, public) = app_with_agent(Some(Agent::Claude));

        let tree = submit_api(
            &mut service,
            &app,
            read_request(AgentActivityReadParams {
                pane_id: Some(public.clone()),
                follow: true,
                ..AgentActivityReadParams::default()
            }),
        )
        .expect("受理");
        assert_eq!(tree["id"], "req-1");
        assert_eq!(tree["result"]["nodes"][1]["parent_id"], "a");
        assert!(tree["result"].get("content").is_none());
        // 读整棵树顺带刷新落库。
        assert!(matches!(
            recv_event(&mut received),
            AppEvent::AgentActivityRefreshed { pane_id: refreshed, result: Ok(nodes) }
                if refreshed == pane_id && nodes.len() == 2
        ));

        let content = submit_api(
            &mut service,
            &app,
            read_request(AgentActivityReadParams {
                pane_id: Some(public.clone()),
                node_id: Some("a.1".into()),
                cursor: Some("c0".into()),
                max_bytes: Some(u32::MAX),
                ..AgentActivityReadParams::default()
            }),
        )
        .expect("受理");
        assert_eq!(content["result"]["nodes"], serde_json::json!([]));
        assert_eq!(content["result"]["content"]["node_id"], "a.1");
        assert_eq!(
            content["result"]["content"]["text"],
            format!("-|a.1|c0|{MAX_READ_BYTES}"),
            "max_bytes 被压到上限"
        );
        assert_eq!(content["result"]["content"]["next_cursor"], "next");

        let failed = submit_api(
            &mut service,
            &app,
            read_request(AgentActivityReadParams {
                pane_id: Some(public),
                node_id: Some("missing".into()),
                ..AgentActivityReadParams::default()
            }),
        )
        .expect("受理");
        assert_eq!(failed["error"]["code"], "activity_malformed");
    }

    #[test]
    fn activity_read_of_an_unimplemented_source_answers_not_implemented() {
        use crate::api::schema::AgentActivityReadParams;
        let (mut service, _received) = fake_service();
        let (app, _, public) = app_with_agent(Some(Agent::Codex));
        let response = submit_api(
            &mut service,
            &app,
            read_request(AgentActivityReadParams {
                pane_id: Some(public),
                ..AgentActivityReadParams::default()
            }),
        )
        .expect("受理");
        assert_eq!(response["error"]["code"], NOT_IMPLEMENTED_CODE);
    }

    #[test]
    fn external_read_and_list_go_through_the_external_sources() {
        use crate::api::schema::{AgentActivityReadParams, EmptyParams};
        let (mut service, mut received) = fake_service();
        let (app, _, _) = app_with_agent(None);
        let tree = submit_api(
            &mut service,
            &app,
            read_request(AgentActivityReadParams {
                external_id: Some("zcode:s-1".into()),
                ..AgentActivityReadParams::default()
            }),
        )
        .expect("受理");
        assert_eq!(tree["result"]["nodes"][0]["id"], "s-1");

        let list = submit_api(
            &mut service,
            &app,
            Request {
                id: "req-2".into(),
                method: Method::AgentExternalList(EmptyParams::default()),
            },
        )
        .expect("受理");
        assert_eq!(list["id"], "req-2");
        assert_eq!(list["result"]["type"], "external_agent_list");
        assert_eq!(list["result"]["agents"][0]["source"], "zcode");
        assert!(matches!(
            recv_event(&mut received),
            AppEvent::ExternalAgentsRefreshed { source, result: Ok(agents) }
                if source == "zcode" && agents.len() == 1
        ));
    }

    #[test]
    fn external_list_with_only_stub_sources_answers_not_implemented() {
        use crate::api::schema::EmptyParams;
        fn stub_external() -> &'static [&'static dyn ActivitySource] {
            struct Stub;
            impl ActivitySource for Stub {
                fn id(&self) -> &'static str {
                    "stub"
                }
                fn discover(
                    &self,
                    _cx: &SourceContext<'_>,
                ) -> Result<Vec<AgentActivityNode>, SourceError> {
                    Err(SourceError::Unsupported)
                }
                fn read(
                    &self,
                    _cx: &SourceContext<'_>,
                    _node_id: &str,
                    _cursor: Option<&str>,
                    _max_bytes: usize,
                ) -> Result<ContentChunk, SourceError> {
                    Err(SourceError::Unsupported)
                }
                fn discover_external(
                    &self,
                    _home: &Path,
                    _now_ms: u64,
                ) -> Result<Vec<ExternalAgentInfo>, SourceError> {
                    Err(SourceError::Unsupported)
                }
            }
            &[&Stub]
        }
        let (events, _received) = tokio::sync::mpsc::channel(8);
        let mut service = Service::with_sources(
            events,
            Sources {
                source_for: fake_source_for,
                external: stub_external,
            },
            Some(std::env::temp_dir()),
        );
        let (app, _, _) = app_with_agent(None);
        let response = submit_api(
            &mut service,
            &app,
            Request {
                id: "req-3".into(),
                method: Method::AgentExternalList(EmptyParams::default()),
            },
        )
        .expect("受理");
        assert_eq!(response["error"]["code"], NOT_IMPLEMENTED_CODE);
    }

    #[test]
    fn endpoint_replies_travel_as_response_chunks_for_the_requesting_client() {
        use crate::api::schema::AgentActivityReadParams;
        let (mut service, _received) = fake_service();
        let (app, _, public) = app_with_agent(Some(Agent::Claude));
        let (server_events, mut server_received) = tokio::sync::mpsc::channel(8);
        service
            .submit_request(
                &app,
                read_request(AgentActivityReadParams {
                    pane_id: Some(public),
                    node_id: Some("a".into()),
                    ..AgentActivityReadParams::default()
                }),
                Reply::Endpoint {
                    client_id: 7,
                    boot_id: "boot".into(),
                    events: server_events,
                },
                Instant::now(),
            )
            .expect("受理");
        let deadline = Instant::now() + Duration::from_secs(5);
        let event = loop {
            match server_received.try_recv() {
                Ok(event) => break event,
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("没有等到端点应答：{error:?}"),
            }
        };
        let ServerEvent::ObservationResponse {
            client_id,
            boot_id,
            message:
                crate::protocol::ServerMessage::ClientShellEndpointResponseChunk {
                    request_id,
                    final_chunk,
                    data,
                    ..
                },
        } = event
        else {
            panic!("应为端点应答分块");
        };
        assert_eq!((client_id, boot_id.as_str()), (7, "boot"));
        assert_eq!(request_id, "req-1");
        assert!(final_chunk);
        let response: serde_json::Value = serde_json::from_slice(&data).expect("JSON");
        assert_eq!(response["result"]["content"]["node_id"], "a");
    }
}
