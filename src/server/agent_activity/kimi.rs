//! Kimi Code 的活动来源适配器：子 agent、后台任务与待办。
//!
//! # 取证基线
//!
//! Kimi Code 2.0.2（`kimi --version`），2026-09-22 本机只读核对：189 个会话、1253
//! 个 agent、1340 个任务文件；只核对目录结构、键名与枚举取值，不读正文。数据根是
//! `$KIMI_CODE_HOME`，缺省 `~/.kimi-code`。
//!
//! - `session_index.jsonl`：每行 `{sessionId, sessionDir, workDir}`，均为字符串。
//!   `sessionId` 形如 `session_<uuid>`；`sessionDir` 是绝对路径，末段等于
//!   `sessionId`（189/189）。
//! - 会话目录 `sessions/wd_<basename(workDir)>_<sha256(workDir) 前 12 位>/
//!   session_<uuid>/`（哈希规则 189/189 吻合）。
//! - `state.json` 两代格式：旧版无 `version`，带 `workDir` 与 ISO 字符串
//!   `createdAt`；`version: 2` 带 `id`（= `sessionId`）、`cwd` 与毫秒整数
//!   `createdAt`。`agents` 的键实测只有 `main` 与 `agent-<序号>`，值为
//!   `{type: "main"|"sub", homedir, parentAgentId, labels?: {parentAgentId,
//!   profileName, swarmItem}, swarmItem?}`。`profileName` 实测 `coder`/`explore`，
//!   只有 198/1064 个子 agent 带；`parentAgentId` 实测为 `main` 或 `agent-<序号>`，
//!   深度 0/1/2 = 189/1044/20，**不假设深度上限**；9 处 `labels.parentAgentId`
//!   与顶层不一致，以顶层为准。
//! - `agents/<agentId>/wire.jsonl`：逐事件流，单行实测可达 1.4 MB。首行
//!   `{type: "metadata", protocol_version: "1.4"|"1.5", created_at: <ms>}`，其余每行
//!   `{type, time: <ms>, …}`。本适配器只认：开始类 `turn.prompt`、`turn.steer`、
//!   `agent.turn.started`、`prompt.accepted`；结束类
//!   `turn.ended{reason: completed|failed|cancelled}`、
//!   `agent.turn.ended{outcome: done|failed|aborted}`、
//!   `prompt.completed{reason: completed|failed}`、`prompt.aborted`、
//!   `turn.cancel{reason: user_cancelled|aborted}`；活动类
//!   `context.append_loop_event`、`llm.request`、`context.append_message`；待办
//!   `tools.update_store{key: "todo", value: [{title, status}]}`（整表覆盖，`status`
//!   实测 `pending|in_progress|done`）。
//! - `agents/<agentId>/tasks/<taskId>.json`：`taskId` = `<kind>-<8 位 [0-9a-z]>`，
//!   与文件名一致（1340/1340）。`kind` 实测 `process|agent|question`；`status` 六态
//!   `completed|failed|killed|timed_out|lost|running`；`startedAt`/`endedAt` 为毫秒
//!   整数（运行中 `endedAt` 为 null）；另有 `description`、`command`、`pid`、
//!   `exitCode`、`detached`、`timeoutMs`、`stopReason`、`stopCode`。`kind: agent`
//!   另有 `agentId`（154/154 指向 `state.json` 里父 agent 即任务属主的子 agent，同一
//!   `agentId` 可有多次运行）与 `subagentType`（`coder|explore|agent`）。同级
//!   `<taskId>/output.log` 是 UTF-8 文本输出，可能不存在（121/1340 缺）。
//! - 只有后台子 agent 有任务文件：1064 个子 agent 里 954 个没有，它们的状态只能从
//!   自身 `wire.jsonl` 尾部推断（见 `foreground_status`）。
//!
//! # 节点
//!
//! 主 agent（`main`）就是 pane 本身，不出节点；它的直接子项 `parent_id` 为空。
//! - `agent:<agentId>`：子 agent（`Subagent`）。父子关系取 `state.json`，`agent_type`
//!   取 `profileName`，状态取最近一次 `kind: agent` 任务；没有任务的前台子 agent 由
//!   wire 尾部推断。`read` 返回该 agent 的 `wire.jsonl`（`Jsonl`）。
//! - `task:<taskId>`：`process` → `Background`，`question` → `Task`（运行中即
//!   `Blocked`），找不到所指子 agent 的 `agent` 任务 → `Subagent`。`read` 返回
//!   `output.log`（`Text`）。
//! - `todo:<agentId>:<序号>`：待办（`Todo`），没有可读内容。
//!
//! 状态映射：`running` → Running；`completed` → Done；`failed`/`timed_out` →
//! Failed；`killed` 与取消/中止 → Done（摘要注明，实测多为主动收尾而非出错）；`lost`
//! 与未知取值 → Unknown。
//!
//! # 读取与游标
//!
//! 游标是十进制字节偏移，`next_cursor` 总会给出（含 `eof` 时），跟随方拿它轮询即可
//! 读到文件增长；文件变短（被重写）时从头重读并置 `truncated`。`Jsonl` 只按整行切
//! 片：单行超过 `max_bytes` 时只给前缀、跳过该行其余部分并置 `truncated`；尚未写完
//! 的末行留到下次。`Text` 优先在换行处切片，不切断 UTF-8 字符。
//!
//! 「没内容」分两种，不可混：会话目录还没落盘 → `Unavailable`（稍后重试）；会话在、
//! 节点在、只是内容文件还没生成 → 空片段且 `eof`（确实还没有正文）。
//!
//! # 定位会话
//!
//! 优先用 pane 上报的会话引用（钩子 `SessionStart` 报的 `session_id`）：先查
//! `session_index.jsonl`，再在各 `wd_*` 桶下找同名目录；引用给了却找不到时 `discover`
//! 回空树、`read` 回 `Unavailable`，不退回 cwd（会话可能还没落盘）。没有引用时按
//! cwd：索引里 `workDir` 相同的会话与
//! 按哈希规则算出的桶下的会话里，取最近活动的一个——同一目录多个 kimi 同时运行时
//! 可能认错，钩子上报引用后即以引用为准。两者都没有时返回 `Unsupported`。
//!
//! # 钩子
//!
//! herdr 的 kimi 钩子（`src/integration/assets/kimi/`）在 `SubagentStart`、
//! `SubagentStop`、`Notification`（`task.*`）与 `PostToolUse`（`TodoList`）上发
//! `pane.report_agent_activity`：`hint` 为钩子事件名，非子 agent 任务另带
//! `node_id = "task:<taskId>"`。钩子只是触发器，树以本适配器读到的文件为准；后台任务
//! 开始没有钩子事件（`TaskStarted` 超出 `KIMI_MIN_VERSION` 的事件枚举，见
//! `crate::integration::KIMI_HOOK_EVENTS`），靠轮询兜底。

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{ActivitySource, ContentChunk, SourceContext, SourceError};
use crate::agent_resume::AgentSessionRefKind;
use crate::api::schema::{
    AgentActivityContentFormat, AgentActivityKind, AgentActivityNode, AgentActivityStatus,
};

pub(super) struct Kimi;

const DEFAULT_ROOT_AGENT_ID: &str = "main";

const AGENT_NODE_PREFIX: &str = "agent:";
const TASK_NODE_PREFIX: &str = "task:";
const TODO_NODE_PREFIX: &str = "todo:";

/// wire 尾部扫描窗口：主 agent 只为找待办，窗口放大；子 agent 同时找生命周期与待办。
const ROOT_TAIL_BYTES: u64 = 1024 * 1024;
const SUB_TAIL_BYTES: u64 = 128 * 1024;
/// 首行 `metadata` 先读这么多，没读到换行再放宽到上限。
const HEAD_PROBE_BYTES: u64 = 4 * 1024;
const HEAD_BYTES: u64 = 64 * 1024;
/// 只在行首这么多字节里找 `"type"`：大多数记录 `type` 是第一个键。
const TYPE_PROBE_BYTES: usize = 512;
/// `type` 不在行首时，只对不超过该长度的行做完整解析。
const FULL_PARSE_LINE_BYTES: usize = 64 * 1024;
/// 前台子 agent 没有结束记录时，wire 在这段时间内有写入才算运行中。
const ACTIVE_WINDOW_MS: u64 = 5 * 60 * 1000;

const MAX_STATE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_TASK_BYTES: u64 = 1024 * 1024;
const MAX_INDEX_BYTES: u64 = 32 * 1024 * 1024;
const MAX_TASKS_PER_AGENT: usize = 512;
const MAX_TODOS_PER_AGENT: usize = 200;
const MAX_ID_LEN: usize = 128;
const MAX_LABEL_CHARS: usize = 120;
const MAX_SUMMARY_CHARS: usize = 160;

/// `read` 的 `max_bytes` 为 0 时的默认值与上下限。下限保证至少能前进一个 UTF-8 字符。
const DEFAULT_READ_BYTES: usize = 64 * 1024;
const MIN_READ_BYTES: usize = 4;
const MAX_READ_BYTES: usize = 1024 * 1024;
/// 跳过超长单行时向前找换行的上限；超过即跳到文件末尾。
const MAX_LINE_SKIP_BYTES: u64 = 64 * 1024 * 1024;

const START_RECORDS: [&str; 4] = [
    "turn.prompt",
    "turn.steer",
    "agent.turn.started",
    "prompt.accepted",
];
const END_RECORDS: [&str; 5] = [
    "turn.ended",
    "agent.turn.ended",
    "prompt.completed",
    "prompt.aborted",
    "turn.cancel",
];
const ACTIVITY_RECORDS: [&str; 3] = [
    "context.append_loop_event",
    "llm.request",
    "context.append_message",
];
const TODO_STORE_RECORD: &str = "tools.update_store";
const TODO_STORE_KEY: &str = "todo";

impl ActivitySource for Kimi {
    fn id(&self) -> &'static str {
        "kimi"
    }

    fn discover(&self, cx: &SourceContext<'_>) -> Result<Vec<AgentActivityNode>, SourceError> {
        discover_in(&kimi_root(cx), cx)
    }

    fn read(
        &self,
        cx: &SourceContext<'_>,
        node_id: &str,
        cursor: Option<&str>,
        max_bytes: usize,
    ) -> Result<ContentChunk, SourceError> {
        read_in(&kimi_root(cx), cx, node_id, cursor, max_bytes)
    }
}

/// Kimi 数据根，本适配器所有「按配置目录拼路径」的唯一出口：runtime 解析好的
/// 配置目录（`SourceContext::agent_config_dir`，跟随 `KIMI_CODE_HOME` 并展开开头
/// 的 `~`，口径同 `integration::env::kimi_dir`）优先，缺省才回退
/// `<home>/.kimi-code`。给了配置目录就只认它、不再回头找 home。
fn kimi_root(cx: &SourceContext<'_>) -> PathBuf {
    match cx.agent_config_dir {
        Some(config_dir) => config_dir.to_path_buf(),
        None => cx.home.join(".kimi-code"),
    }
}

fn discover_in(root: &Path, cx: &SourceContext<'_>) -> Result<Vec<AgentActivityNode>, SourceError> {
    let Some(session_dir) = locate_session(root, cx)? else {
        return Ok(Vec::new());
    };
    Ok(build_tree(
        &session_dir,
        cx.now_ms,
        cx.session
            .is_some_and(|session| session.kind == AgentSessionRefKind::Path),
    ))
}

fn read_in(
    root: &Path,
    cx: &SourceContext<'_>,
    node_id: &str,
    cursor: Option<&str>,
    max_bytes: usize,
) -> Result<ContentChunk, SourceError> {
    let node = NodeRef::parse(node_id).ok_or(SourceError::Unsupported)?;
    let format = match node {
        NodeRef::Agent(_) => AgentActivityContentFormat::Jsonl,
        NodeRef::Task(_) => AgentActivityContentFormat::Text,
        // 待办没有正文。
        NodeRef::Todo => return Err(SourceError::Unsupported),
    };
    let offset = parse_cursor(cursor)?;
    let max_bytes = effective_max_bytes(max_bytes);
    // 会话目录还没落盘：`Unavailable` 让调用方稍后重试，不能装成「读完了」。
    let Some(session_dir) = locate_session(root, cx)? else {
        return Err(SourceError::Unavailable);
    };
    if cx
        .session
        .is_some_and(|session| session.kind == AgentSessionRefKind::Path)
        && !session_metadata_matches(
            &session_dir,
            read_json_file(&session_dir.join("state.json"), MAX_STATE_BYTES).as_ref(),
        )
    {
        return Err(SourceError::Unavailable);
    }
    match node {
        NodeRef::Agent(agent_id) => {
            let path = session_dir.join("agents").join(agent_id).join("wire.jsonl");
            read_jsonl_chunk(&path, offset, max_bytes)
        }
        NodeRef::Task(task_id) => match find_task_output(&session_dir, task_id) {
            Some(path) => read_text_chunk(&path, offset, max_bytes),
            None => Ok(empty_chunk(format, offset)),
        },
        NodeRef::Todo => Err(SourceError::Unsupported),
    }
}

// ---------------------------------------------------------------------------
// 节点 id

enum NodeRef<'a> {
    Agent(&'a str),
    Task(&'a str),
    Todo,
}

impl<'a> NodeRef<'a> {
    /// 解析调用方给的节点 id。id 片段只允许 `[A-Za-z0-9_-]`，杜绝路径穿越。
    fn parse(node_id: &'a str) -> Option<Self> {
        if let Some(id) = node_id.strip_prefix(AGENT_NODE_PREFIX) {
            return valid_id(id).then_some(Self::Agent(id));
        }
        if let Some(id) = node_id.strip_prefix(TASK_NODE_PREFIX) {
            return valid_id(id).then_some(Self::Task(id));
        }
        let rest = node_id.strip_prefix(TODO_NODE_PREFIX)?;
        let (agent_id, index) = rest.rsplit_once(':')?;
        (valid_id(agent_id) && index.parse::<usize>().is_ok()).then_some(Self::Todo)
    }
}

fn agent_node_id(agent_id: &str) -> String {
    format!("{AGENT_NODE_PREFIX}{agent_id}")
}

fn task_node_id(task_id: &str) -> String {
    format!("{TASK_NODE_PREFIX}{task_id}")
}

fn todo_node_id(agent_id: &str, index: usize) -> String {
    format!("{TODO_NODE_PREFIX}{agent_id}:{index}")
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_LEN
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

// ---------------------------------------------------------------------------
// 定位会话

fn locate_session(root: &Path, cx: &SourceContext<'_>) -> Result<Option<PathBuf>, SourceError> {
    if let Some(session) = cx.session {
        return Ok(match session.kind {
            AgentSessionRefKind::Id => session_dir_by_id(root, &session.value),
            AgentSessionRefKind::Path => session_dir_by_path(Path::new(&session.value)),
        });
    }
    if let Some(cwd) = cx.cwd {
        return Ok(session_dir_by_cwd(root, cwd));
    }
    Err(SourceError::Unsupported)
}

/// 会话 id 规范成目录名：`session_<uuid>` 原样，裸 uuid 补前缀；非法字符一律拒绝。
fn session_dir_name(id: &str) -> Option<String> {
    crate::agent_resume::kimi_session_dir_name(id.trim())
}

fn session_dir_by_id(root: &Path, id: &str) -> Option<PathBuf> {
    let dir_name = session_dir_name(id)?;
    let trimmed = id.trim();
    for entry in read_session_index(root) {
        let matches = entry
            .session_id
            .as_deref()
            .is_some_and(|session_id| session_id == dir_name || session_id == trimmed);
        if !matches {
            continue;
        }
        if let Some(dir) = entry.session_dir.filter(|dir| dir.is_dir()) {
            return Some(dir);
        }
    }
    // 索引缺失、过期或指向别处：在各工作目录桶下找同名会话目录。
    list_dirs(&root.join("sessions"))
        .into_iter()
        .map(|(_, bucket)| bucket.join(&dir_name))
        .find(|candidate| candidate.is_dir())
}

fn session_dir_by_path(path: &Path) -> Option<PathBuf> {
    (path.join("state.json").is_file() || path.join("agents").is_dir()).then(|| path.to_path_buf())
}

fn session_dir_by_cwd(root: &Path, cwd: &Path) -> Option<PathBuf> {
    let mut forms = vec![cwd.to_path_buf()];
    if let Ok(canonical) = fs::canonicalize(cwd) {
        if canonical != cwd {
            forms.push(canonical);
        }
    }

    let mut candidates: Vec<PathBuf> = Vec::new();
    for entry in read_session_index(root) {
        let same_dir = entry
            .work_dir
            .as_deref()
            .is_some_and(|work_dir| forms.iter().any(|form| Path::new(work_dir) == form));
        if !same_dir {
            continue;
        }
        if let Some(dir) = entry.session_dir.filter(|dir| dir.is_dir()) {
            candidates.push(dir);
        }
    }

    let buckets = list_dirs(&root.join("sessions"));
    for form in &forms {
        let Some(text) = form.to_str() else {
            continue;
        };
        let suffix = format!("_{}", work_dir_hash(text));
        for (name, bucket) in &buckets {
            if !(name.starts_with("wd_") && name.ends_with(&suffix)) {
                continue;
            }
            candidates.extend(
                list_dirs(bucket)
                    .into_iter()
                    .filter(|(session, _)| session.starts_with("session_"))
                    .map(|(_, dir)| dir),
            );
        }
    }

    candidates.sort();
    candidates.dedup();
    candidates
        .into_iter()
        .map(|dir| (session_activity_ms(&dir), dir))
        .max_by(|(a_time, a_dir), (b_time, b_dir)| {
            a_time.cmp(b_time).then_with(|| a_dir.cmp(b_dir))
        })
        .map(|(_, dir)| dir)
}

/// kimi 工作目录桶名里的哈希：`sha256(workDir)` 的前 12 位十六进制。
fn work_dir_hash(work_dir: &str) -> String {
    Sha256::digest(work_dir.as_bytes())
        .iter()
        .take(6)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// 会话最近活动时间：主 agent wire 与 `state.json` 的较晚修改时间。
fn session_activity_ms(session_dir: &Path) -> Option<u64> {
    [
        session_dir
            .join("agents")
            .join(DEFAULT_ROOT_AGENT_ID)
            .join("wire.jsonl"),
        session_dir.join("state.json"),
    ]
    .iter()
    .filter_map(|path| fs::metadata(path).ok().and_then(|meta| modified_ms(&meta)))
    .max()
}

struct IndexEntry {
    session_id: Option<String>,
    session_dir: Option<PathBuf>,
    work_dir: Option<String>,
}

fn read_session_index(root: &Path) -> Vec<IndexEntry> {
    let path = root.join("session_index.jsonl");
    let Some(bytes) = read_capped_file(&path, MAX_INDEX_BYTES) else {
        return Vec::new();
    };
    bytes
        .split(|byte| *byte == b'\n')
        .filter_map(|line| {
            let line = line.trim_ascii();
            if line.is_empty() {
                return None;
            }
            let value: Value = serde_json::from_slice(line).ok()?;
            Some(IndexEntry {
                session_id: str_field(&value, "sessionId").map(str::to_string),
                session_dir: str_field(&value, "sessionDir").map(PathBuf::from),
                work_dir: str_field(&value, "workDir").map(str::to_string),
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// 活动树

#[derive(Default)]
struct AgentMeta {
    parent: Option<String>,
    profile: Option<String>,
    swarm_item: Option<String>,
    is_main: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TaskKind {
    Agent,
    Process,
    Question,
    Unknown,
}

impl TaskKind {
    fn parse(kind: Option<&str>, task_id: &str) -> Self {
        // 缺 `kind` 时按 `taskId` 的 `<kind>-` 前缀推断。
        let kind = kind.or_else(|| task_id.split_once('-').map(|(prefix, _)| prefix));
        match kind {
            Some("agent") => Self::Agent,
            Some("process" | "bash") => Self::Process,
            Some("question") => Self::Question,
            _ => Self::Unknown,
        }
    }
}

struct TaskMeta {
    owner: String,
    id: String,
    kind: TaskKind,
    status: Option<String>,
    description: Option<String>,
    command: Option<String>,
    agent_id: Option<String>,
    subagent_type: Option<String>,
    started_at_ms: Option<u64>,
    ended_at_ms: Option<u64>,
    exit_code: Option<i64>,
    stop_reason: Option<String>,
    stop_code: Option<String>,
    has_output: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum LifecycleOutcome {
    Started,
    Ended(AgentActivityStatus, Option<&'static str>),
}

struct Lifecycle {
    outcome: LifecycleOutcome,
    time_ms: Option<u64>,
    /// 这条生命周期记录之后又出现了活动类记录（agent 仍在干活）。
    activity_after: bool,
}

struct TodoItem {
    title: Option<String>,
    status: Option<String>,
}

#[derive(Default)]
struct WireSummary {
    exists: bool,
    created_at_ms: Option<u64>,
    modified_ms: Option<u64>,
    lifecycle: Option<Lifecycle>,
    /// `None` = 窗口内没有待办记录；`Some(空)` = 待办被清空。
    todos: Option<Vec<TodoItem>>,
}

/// 同一父节点下的排序键：待办在前（按表内顺序），其余按开始时间、再按 id。
struct Placed {
    node: AgentActivityNode,
    is_todo: bool,
    index: usize,
}

fn build_tree(session_dir: &Path, now_ms: u64, reported_path: bool) -> Vec<AgentActivityNode> {
    let agents_dir = session_dir.join("agents");
    let metadata = read_json_file(&session_dir.join("state.json"), MAX_STATE_BYTES);
    if reported_path && !session_metadata_matches(session_dir, metadata.as_ref()) {
        return Vec::new();
    }
    let state = read_state_agents(metadata.as_ref());
    let root = root_agent_id(&state);

    let agent_dirs: BTreeMap<String, PathBuf> = list_dirs(&agents_dir)
        .into_iter()
        .filter(|(name, _)| valid_id(name))
        .collect();
    let mut agent_ids: BTreeSet<String> = state.keys().cloned().collect();
    agent_ids.extend(agent_dirs.keys().cloned());
    let parents = resolve_parents(&state, &agent_ids, &root);

    let tasks: Vec<TaskMeta> = agent_dirs
        .iter()
        .flat_map(|(owner, dir)| read_tasks(dir, owner))
        .collect();
    let mut agent_runs: HashMap<&str, Vec<&TaskMeta>> = HashMap::new();
    for task in &tasks {
        if task.kind != TaskKind::Agent {
            continue;
        }
        if let Some(agent_id) = task.agent_id.as_deref() {
            if agent_id != root && agent_ids.contains(agent_id) {
                agent_runs.entry(agent_id).or_default().push(task);
            }
        }
    }

    let owner_parent = |owner: &str| -> Option<String> {
        (owner != root && agent_ids.contains(owner)).then(|| agent_node_id(owner))
    };
    let mut children: BTreeMap<Option<String>, Vec<Placed>> = BTreeMap::new();
    let mut place =
        |parent: Option<String>, node: AgentActivityNode, is_todo: bool, index: usize| {
            children.entry(parent).or_default().push(Placed {
                node,
                is_todo,
                index,
            });
        };

    for agent_id in agent_ids.iter().filter(|id| **id != root) {
        let wire = agent_dirs
            .get(agent_id)
            .map(|dir| scan_wire(&dir.join("wire.jsonl"), SUB_TAIL_BYTES, true))
            .unwrap_or_default();
        let runs = agent_runs
            .get(agent_id.as_str())
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let parent = parents
            .get(agent_id)
            .cloned()
            .flatten()
            .map(|id| agent_node_id(&id));
        let node = subagent_node(
            agent_id,
            state.get(agent_id),
            runs,
            &wire,
            parent.clone(),
            now_ms,
        );
        place(parent, node, false, 0);
        for (index, node) in todo_nodes(agent_id, Some(agent_node_id(agent_id)), &wire) {
            place(Some(agent_node_id(agent_id)), node, true, index);
        }
    }

    if let Some(dir) = agent_dirs.get(&root) {
        let wire = scan_wire(&dir.join("wire.jsonl"), ROOT_TAIL_BYTES, false);
        for (index, node) in todo_nodes(&root, None, &wire) {
            place(None, node, true, index);
        }
    }

    for task in &tasks {
        let merged = task.kind == TaskKind::Agent
            && task
                .agent_id
                .as_deref()
                .is_some_and(|agent_id| agent_runs.contains_key(agent_id));
        if merged {
            continue;
        }
        let parent = owner_parent(&task.owner);
        place(parent.clone(), task_node(task, parent), false, 0);
    }

    emit_preorder(children)
}

// Reported directories must retain their session identity when read later.
// The legacy state schema has workDir but no version/id; v2 always has id.
fn session_metadata_matches(session_dir: &Path, state: Option<&Value>) -> bool {
    let Some(state) = state.and_then(Value::as_object) else {
        return false;
    };
    if let Some(id) = state.get("id") {
        id.as_str()
            .is_some_and(|id| Some(id) == session_dir.file_name().and_then(|name| name.to_str()))
    } else {
        !state.contains_key("version") && state.get("workDir").is_some_and(Value::is_string)
    }
}

fn read_state_agents(state: Option<&Value>) -> BTreeMap<String, AgentMeta> {
    let mut agents = BTreeMap::new();
    let Some(state) = state else {
        return agents;
    };
    let Some(entries) = state.get("agents").and_then(Value::as_object) else {
        return agents;
    };
    for (id, entry) in entries {
        if !valid_id(id) {
            continue;
        }
        let labels = entry.get("labels");
        let label_field = |key: &str| labels.and_then(|labels| str_field(labels, key));
        agents.insert(
            id.clone(),
            AgentMeta {
                parent: str_field(entry, "parentAgentId")
                    .or_else(|| label_field("parentAgentId"))
                    .filter(|parent| valid_id(parent))
                    .map(str::to_string),
                profile: label_field("profileName")
                    .or_else(|| str_field(entry, "profileName"))
                    .and_then(|profile| one_line(profile, MAX_LABEL_CHARS)),
                swarm_item: label_field("swarmItem")
                    .or_else(|| str_field(entry, "swarmItem"))
                    .map(str::to_string),
                is_main: str_field(entry, "type") == Some("main"),
            },
        );
    }
    agents
}

fn root_agent_id(state: &BTreeMap<String, AgentMeta>) -> String {
    if state.contains_key(DEFAULT_ROOT_AGENT_ID) {
        return DEFAULT_ROOT_AGENT_ID.to_string();
    }
    state
        .iter()
        .find(|(_, meta)| meta.is_main)
        .map(|(id, _)| id.clone())
        .unwrap_or_else(|| DEFAULT_ROOT_AGENT_ID.to_string())
}

/// 每个非根 agent 的父 agent；`None` = 挂在 pane 下。父缺失、指向根或自身、或成环时
/// 一律挂到顶层，保证结果无环且不丢节点。
fn resolve_parents(
    state: &BTreeMap<String, AgentMeta>,
    agent_ids: &BTreeSet<String>,
    root: &str,
) -> HashMap<String, Option<String>> {
    let mut parents: HashMap<String, Option<String>> = agent_ids
        .iter()
        .filter(|id| id.as_str() != root)
        .map(|id| {
            let parent = state
                .get(id)
                .and_then(|meta| meta.parent.clone())
                .filter(|parent| {
                    parent.as_str() != root && parent != id && agent_ids.contains(parent)
                });
            (id.clone(), parent)
        })
        .collect();

    // 逐个沿父链上溯；回到起点即在环上，剪掉起点的父边。
    for id in agent_ids.iter().filter(|id| id.as_str() != root) {
        let mut current = parents.get(id).cloned().flatten();
        let mut steps = 0usize;
        while let Some(parent) = current {
            if parent == *id {
                parents.insert(id.clone(), None);
                break;
            }
            steps += 1;
            if steps > agent_ids.len() {
                break;
            }
            current = parents.get(&parent).cloned().flatten();
        }
    }
    parents
}

fn read_tasks(owner_dir: &Path, owner: &str) -> Vec<TaskMeta> {
    let tasks_dir = owner_dir.join("tasks");
    let Ok(entries) = fs::read_dir(&tasks_dir) else {
        return Vec::new();
    };
    let mut files: Vec<(Option<SystemTime>, String, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(task_id) = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix(".json"))
        else {
            continue;
        };
        if !valid_id(task_id) || !path.is_file() {
            continue;
        }
        let modified = entry.metadata().ok().and_then(|meta| meta.modified().ok());
        files.push((modified, task_id.to_string(), path));
    }
    if files.len() > MAX_TASKS_PER_AGENT {
        // 任务过多时只保留最近修改的一批。
        files.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        files.truncate(MAX_TASKS_PER_AGENT);
    }
    files
        .into_iter()
        .filter_map(|(_, task_id, path)| parse_task(&path, &tasks_dir, owner, task_id))
        .collect()
}

fn parse_task(path: &Path, tasks_dir: &Path, owner: &str, task_id: String) -> Option<TaskMeta> {
    let value = read_json_file(path, MAX_TASK_BYTES)?;
    if !value.is_object() {
        tracing::debug!(path = %path.display(), "kimi activity: task file is not a JSON object");
        return None;
    }
    let has_output = tasks_dir.join(&task_id).join("output.log").is_file();
    Some(TaskMeta {
        owner: owner.to_string(),
        kind: TaskKind::parse(str_field(&value, "kind"), &task_id),
        status: str_field(&value, "status").map(str::to_string),
        description: str_field(&value, "description").map(str::to_string),
        command: str_field(&value, "command").map(str::to_string),
        agent_id: str_field(&value, "agentId")
            .filter(|agent_id| valid_id(agent_id))
            .map(str::to_string),
        subagent_type: str_field(&value, "subagentType")
            .and_then(|kind| one_line(kind, MAX_LABEL_CHARS)),
        started_at_ms: ms_field(&value, "startedAt"),
        ended_at_ms: ms_field(&value, "endedAt"),
        exit_code: value.get("exitCode").and_then(Value::as_i64),
        stop_reason: str_field(&value, "stopReason").map(str::to_string),
        stop_code: str_field(&value, "stopCode").map(str::to_string),
        has_output,
        id: task_id,
    })
}

fn subagent_node(
    agent_id: &str,
    meta: Option<&AgentMeta>,
    runs: &[&TaskMeta],
    wire: &WireSummary,
    parent_id: Option<String>,
    now_ms: u64,
) -> AgentActivityNode {
    let latest = runs.iter().copied().max_by(|a, b| {
        a.started_at_ms
            .cmp(&b.started_at_ms)
            .then_with(|| a.id.cmp(&b.id))
    });
    let agent_type = meta
        .and_then(|meta| meta.profile.clone())
        .or_else(|| latest.and_then(|task| task.subagent_type.clone()));
    let label = latest
        .and_then(|task| task.description.as_deref())
        .and_then(|description| one_line(description, MAX_LABEL_CHARS))
        .or_else(|| {
            meta.and_then(|meta| meta.swarm_item.as_deref())
                .and_then(|item| one_line(item, MAX_LABEL_CHARS))
        })
        .unwrap_or_else(|| agent_id.to_string());

    let (status, started_at_ms, ended_at_ms, summary) = match latest {
        Some(task) => {
            let (status, summary) = task_status(task);
            let started = runs.iter().filter_map(|run| run.started_at_ms).min();
            (status, started, task.ended_at_ms, summary)
        }
        None => {
            let (status, ended, summary) = foreground_status(wire, now_ms);
            (status, wire.created_at_ms, ended, summary)
        }
    };

    AgentActivityNode {
        id: agent_node_id(agent_id),
        kind: AgentActivityKind::Subagent,
        label,
        status,
        parent_id,
        agent_type,
        content_ref: wire.exists.then(|| format!("agents/{agent_id}/wire.jsonl")),
        summary,
        started_at_ms,
        ended_at_ms,
    }
}

/// 没有任务文件的前台子 agent：最后一条生命周期记录是结束类且其后没有新活动 → 按
/// 结束原因；否则 wire 最近有写入 → Running，久未写入 → Unknown（多半已结束但没留
/// 结束记录，实测约三成，不猜）。
fn foreground_status(
    wire: &WireSummary,
    now_ms: u64,
) -> (AgentActivityStatus, Option<u64>, Option<String>) {
    if let Some(lifecycle) = &wire.lifecycle {
        if let LifecycleOutcome::Ended(status, note) = lifecycle.outcome {
            if !lifecycle.activity_after {
                return (status, lifecycle.time_ms, note.map(str::to_string));
            }
        }
    }
    let fresh = wire
        .modified_ms
        .is_some_and(|modified| now_ms > 0 && now_ms.saturating_sub(modified) <= ACTIVE_WINDOW_MS);
    if wire.exists && fresh {
        (AgentActivityStatus::Running, None, None)
    } else {
        (AgentActivityStatus::Unknown, None, None)
    }
}

fn task_node(task: &TaskMeta, parent_id: Option<String>) -> AgentActivityNode {
    let (status, summary) = task_status(task);
    let kind = match task.kind {
        TaskKind::Process => AgentActivityKind::Background,
        TaskKind::Question => AgentActivityKind::Task,
        TaskKind::Agent => AgentActivityKind::Subagent,
        TaskKind::Unknown => AgentActivityKind::Unknown,
    };
    let label = task
        .description
        .as_deref()
        .and_then(|description| one_line(description, MAX_LABEL_CHARS))
        .or_else(|| {
            task.command
                .as_deref()
                .and_then(|command| one_line(command, MAX_LABEL_CHARS))
        })
        .unwrap_or_else(|| task.id.clone());
    AgentActivityNode {
        id: task_node_id(&task.id),
        kind,
        label,
        status,
        parent_id,
        agent_type: (task.kind == TaskKind::Agent)
            .then(|| task.subagent_type.clone())
            .flatten(),
        content_ref: task
            .has_output
            .then(|| format!("agents/{}/tasks/{}/output.log", task.owner, task.id)),
        summary,
        started_at_ms: task.started_at_ms,
        ended_at_ms: task.ended_at_ms,
    }
}

fn task_status(task: &TaskMeta) -> (AgentActivityStatus, Option<String>) {
    let raw = task.status.as_deref();
    let (status, mut summary) = match raw {
        Some("running") if task.kind == TaskKind::Question => (AgentActivityStatus::Blocked, None),
        Some("running") => (AgentActivityStatus::Running, None),
        Some("pending" | "queued") => (AgentActivityStatus::Pending, None),
        Some("completed") => (AgentActivityStatus::Done, None),
        Some("failed") => (AgentActivityStatus::Failed, None),
        Some("timed_out") => (AgentActivityStatus::Failed, Some("timed out".to_string())),
        Some("killed") => (AgentActivityStatus::Done, Some("killed".to_string())),
        Some("lost") => (AgentActivityStatus::Unknown, Some("lost".to_string())),
        Some(other) => (
            AgentActivityStatus::Unknown,
            one_line(other, MAX_LABEL_CHARS).map(|other| format!("status {other}")),
        ),
        None => (AgentActivityStatus::Unknown, None),
    };
    if matches!(raw, Some("completed" | "failed")) {
        if let Some(code) = task.exit_code.filter(|code| *code != 0) {
            summary = Some(format!("exit {code}"));
        }
    }
    let settled = !matches!(
        status,
        AgentActivityStatus::Running | AgentActivityStatus::Pending | AgentActivityStatus::Blocked
    );
    let reason = task
        .stop_reason
        .as_deref()
        .or(task.stop_code.as_deref())
        .and_then(|reason| one_line(reason, MAX_SUMMARY_CHARS));
    if let Some(reason) = reason.filter(|_| settled) {
        summary = Some(match summary {
            Some(summary) => format!("{summary}: {reason}"),
            None => reason,
        });
    }
    (
        status,
        summary.map(|summary| truncate_chars(&summary, MAX_SUMMARY_CHARS)),
    )
}

fn todo_nodes(
    agent_id: &str,
    parent_id: Option<String>,
    wire: &WireSummary,
) -> Vec<(usize, AgentActivityNode)> {
    let Some(todos) = &wire.todos else {
        return Vec::new();
    };
    todos
        .iter()
        .take(MAX_TODOS_PER_AGENT)
        .enumerate()
        .map(|(index, todo)| {
            let status = match todo.status.as_deref() {
                Some("pending") => AgentActivityStatus::Pending,
                Some("in_progress") => AgentActivityStatus::Running,
                Some("done" | "completed") => AgentActivityStatus::Done,
                _ => AgentActivityStatus::Unknown,
            };
            let node = AgentActivityNode {
                id: todo_node_id(agent_id, index),
                kind: AgentActivityKind::Todo,
                label: todo
                    .title
                    .as_deref()
                    .and_then(|title| one_line(title, MAX_LABEL_CHARS))
                    .unwrap_or_else(|| format!("todo {}", index + 1)),
                status,
                parent_id: parent_id.clone(),
                ..AgentActivityNode::default()
            };
            (index, node)
        })
        .collect()
}

/// 父节点先于子节点的前序输出；同级按 `Placed` 的排序键，结果确定。
fn emit_preorder(mut children: BTreeMap<Option<String>, Vec<Placed>>) -> Vec<AgentActivityNode> {
    for siblings in children.values_mut() {
        siblings.sort_by(compare_placed);
    }
    let mut out = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut stack = vec![children.remove(&None).unwrap_or_default().into_iter()];
    while let Some(level) = stack.last_mut() {
        let Some(placed) = level.next() else {
            stack.pop();
            continue;
        };
        if !seen.insert(placed.node.id.clone()) {
            continue;
        }
        let key = Some(placed.node.id.clone());
        out.push(placed.node);
        if let Some(kids) = children.remove(&key) {
            stack.push(kids.into_iter());
        }
    }
    // 父节点没有出现的兜底（按构造不应发生）：挂到顶层追加，不丢节点。
    for rest in children.into_values() {
        for mut placed in rest {
            if seen.insert(placed.node.id.clone()) {
                placed.node.parent_id = None;
                out.push(placed.node);
            }
        }
    }
    out
}

fn compare_placed(a: &Placed, b: &Placed) -> Ordering {
    b.is_todo
        .cmp(&a.is_todo)
        .then_with(|| a.index.cmp(&b.index))
        .then_with(|| {
            let a_start = a.node.started_at_ms.unwrap_or(u64::MAX);
            let b_start = b.node.started_at_ms.unwrap_or(u64::MAX);
            a_start.cmp(&b_start)
        })
        .then_with(|| a.node.id.cmp(&b.node.id))
}

// ---------------------------------------------------------------------------
// wire.jsonl 摘要

/// 读 wire 的首行 `metadata`（可选）与尾部窗口：找最后一条生命周期记录与最后一次待办
/// 覆盖。I/O 上限 = `HEAD_BYTES + tail_bytes`，与文件总长无关。
fn scan_wire(path: &Path, tail_bytes: u64, want_head: bool) -> WireSummary {
    let mut summary = WireSummary::default();
    let Ok(mut file) = File::open(path) else {
        return summary;
    };
    let Ok(meta) = file.metadata() else {
        return summary;
    };
    if !meta.is_file() {
        return summary;
    }
    summary.exists = true;
    summary.modified_ms = modified_ms(&meta);
    let len = meta.len();

    if want_head {
        summary.created_at_ms = read_created_at(&mut file, len);
    }

    let start = len.saturating_sub(tail_bytes);
    let Ok(tail) = read_range(&mut file, start, tail_bytes, len) else {
        return summary;
    };
    let mut window = tail.as_slice();
    if start > 0 {
        // 窗口起点多半落在行中间，丢掉这段残行。
        window = match window.iter().position(|byte| *byte == b'\n') {
            Some(newline) => &window[newline + 1..],
            None => &[],
        };
    }

    let mut activity_after = false;
    for line in window.rsplit(|byte| *byte == b'\n') {
        if summary.lifecycle.is_some() && summary.todos.is_some() {
            break;
        }
        let line = line.trim_ascii();
        if line.is_empty() {
            continue;
        }
        let Some(record) = record_type(line) else {
            continue;
        };
        if summary.lifecycle.is_none() {
            if ACTIVITY_RECORDS.contains(&record.as_str()) {
                activity_after = true;
                continue;
            }
            if let Some(lifecycle) = parse_lifecycle(&record, line, activity_after) {
                summary.lifecycle = Some(lifecycle);
                continue;
            }
        }
        if summary.todos.is_none() && record == TODO_STORE_RECORD {
            summary.todos = parse_todo_store(line);
        }
    }
    summary
}

/// 首行 `metadata.created_at`。首行实测约 70 字节：先读一小段，没读到换行才放宽到
/// `HEAD_BYTES`。
fn read_created_at(file: &mut File, len: u64) -> Option<u64> {
    let mut head = read_range(file, 0, HEAD_PROBE_BYTES, len).ok()?;
    if !head.contains(&b'\n') && (head.len() as u64) < len {
        head = read_range(file, 0, HEAD_BYTES, len).ok()?;
    }
    let line = head.split(|byte| *byte == b'\n').next()?;
    let value: Value = serde_json::from_slice(line.trim_ascii()).ok()?;
    (str_field(&value, "type") == Some("metadata"))
        .then(|| ms_field(&value, "created_at"))
        .flatten()
}

/// 取一行记录的顶层 `type`。先在行首找（绝大多数记录 `type` 是第一个键），找不到且
/// 行不长时完整解析；超长且行首没有 `type` 的行放弃。
fn record_type(line: &[u8]) -> Option<String> {
    let probe = &line[..line.len().min(TYPE_PROBE_BYTES)];
    if let Some(found) = probe_type_value(probe) {
        return Some(found);
    }
    if line.len() > FULL_PARSE_LINE_BYTES {
        return None;
    }
    let value: Value = serde_json::from_slice(line).ok()?;
    str_field(&value, "type").map(str::to_string)
}

/// 在字节片里找第一个 `"type"` 键的字符串值（允许冒号两侧有空白，不支持转义）。
/// 只用于快速分类；命中生命周期 / 待办时调用方会完整解析复核。
fn probe_type_value(probe: &[u8]) -> Option<String> {
    const KEY: &[u8] = b"\"type\"";
    let position = probe.windows(KEY.len()).position(|window| window == KEY)?;
    let mut rest = probe[position + KEY.len()..].trim_ascii_start();
    rest = rest.strip_prefix(b":")?.trim_ascii_start();
    rest = rest.strip_prefix(b"\"")?;
    let end = rest
        .iter()
        .position(|byte| *byte == b'"' || *byte == b'\\')?;
    if rest[end] != b'"' {
        return None;
    }
    std::str::from_utf8(&rest[..end]).ok().map(str::to_string)
}

fn parse_lifecycle(record: &str, line: &[u8], activity_after: bool) -> Option<Lifecycle> {
    let is_start = START_RECORDS.contains(&record);
    if !is_start && !END_RECORDS.contains(&record) {
        return None;
    }
    // 快速分类可能命中嵌套对象里的 `type`，完整解析复核顶层。
    let value: Value = serde_json::from_slice(line).ok()?;
    if str_field(&value, "type") != Some(record) {
        return None;
    }
    let outcome = if is_start {
        LifecycleOutcome::Started
    } else {
        let (status, note) = lifecycle_end(record, &value);
        LifecycleOutcome::Ended(status, note)
    };
    Some(Lifecycle {
        outcome,
        time_ms: ms_field(&value, "time"),
        activity_after,
    })
}

fn lifecycle_end(record: &str, value: &Value) -> (AgentActivityStatus, Option<&'static str>) {
    match record {
        "prompt.aborted" => return (AgentActivityStatus::Done, Some("aborted")),
        "turn.cancel" => return (AgentActivityStatus::Done, Some("cancelled")),
        _ => {}
    }
    let reason = str_field(value, "reason").or_else(|| str_field(value, "outcome"));
    match reason {
        Some("completed" | "done") => (AgentActivityStatus::Done, None),
        Some("failed" | "error") => (AgentActivityStatus::Failed, None),
        Some("cancelled" | "user_cancelled") => (AgentActivityStatus::Done, Some("cancelled")),
        Some("aborted") => (AgentActivityStatus::Done, Some("aborted")),
        _ => (AgentActivityStatus::Unknown, None),
    }
}

fn parse_todo_store(line: &[u8]) -> Option<Vec<TodoItem>> {
    let value: Value = serde_json::from_slice(line).ok()?;
    if str_field(&value, "type") != Some(TODO_STORE_RECORD)
        || str_field(&value, "key") != Some(TODO_STORE_KEY)
    {
        return None;
    }
    let items = value.get("value")?.as_array()?;
    Some(
        items
            .iter()
            .filter(|item| item.is_object())
            .map(|item| TodoItem {
                title: str_field(item, "title").map(str::to_string),
                status: str_field(item, "status").map(str::to_string),
            })
            .collect(),
    )
}

// ---------------------------------------------------------------------------
// 内容分页

fn parse_cursor(cursor: Option<&str>) -> Result<u64, SourceError> {
    match cursor {
        None => Ok(0),
        Some(cursor) => cursor.trim().parse::<u64>().map_err(|_| {
            SourceError::Malformed(format!("invalid kimi activity cursor {cursor:?}"))
        }),
    }
}

fn effective_max_bytes(max_bytes: usize) -> usize {
    if max_bytes == 0 {
        DEFAULT_READ_BYTES
    } else {
        max_bytes.clamp(MIN_READ_BYTES, MAX_READ_BYTES)
    }
}

fn find_task_output(session_dir: &Path, task_id: &str) -> Option<PathBuf> {
    list_dirs(&session_dir.join("agents"))
        .into_iter()
        .map(|(_, dir)| dir.join("tasks").join(task_id).join("output.log"))
        .find(|path| path.is_file())
}

fn empty_chunk(format: AgentActivityContentFormat, offset: u64) -> ContentChunk {
    ContentChunk {
        format,
        text: String::new(),
        next_cursor: Some(offset.to_string()),
        eof: true,
        truncated: false,
    }
}

fn chunk(
    format: AgentActivityContentFormat,
    bytes: &[u8],
    next: u64,
    eof: bool,
    truncated: bool,
) -> ContentChunk {
    ContentChunk {
        format,
        text: String::from_utf8_lossy(bytes).into_owned(),
        next_cursor: Some(next.to_string()),
        eof,
        truncated,
    }
}

/// 打开要分页读取的文件；不存在（或不是普通文件）时返回 `None`，由调用方降级为空片段。
fn open_for_read(path: &Path) -> Result<Option<(File, u64)>, SourceError> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(SourceError::Io(error)),
    };
    let meta = file.metadata().map_err(SourceError::Io)?;
    if !meta.is_file() {
        return Ok(None);
    }
    Ok(Some((file, meta.len())))
}

/// 游标越过文件长度说明文件被截断重写：从头读，并在片段上标 `truncated`。
fn resolve_start(offset: u64, len: u64) -> (u64, bool) {
    if offset > len {
        (0, true)
    } else {
        (offset, false)
    }
}

fn read_text_chunk(
    path: &Path,
    offset: u64,
    max_bytes: usize,
) -> Result<ContentChunk, SourceError> {
    let format = AgentActivityContentFormat::Text;
    let Some((mut file, len)) = open_for_read(path)? else {
        return Ok(empty_chunk(format, offset));
    };
    let (start, restarted) = resolve_start(offset, len);
    let buf = read_range(&mut file, start, max_bytes as u64, len).map_err(SourceError::Io)?;
    let reached_end = start + buf.len() as u64 >= len;
    let cut = if reached_end {
        utf8_prefix_len(&buf)
    } else {
        match buf.iter().rposition(|byte| *byte == b'\n') {
            Some(newline) => newline + 1,
            None => utf8_prefix_len(&buf),
        }
    };
    Ok(chunk(
        format,
        &buf[..cut],
        start + cut as u64,
        reached_end,
        restarted,
    ))
}

fn read_jsonl_chunk(
    path: &Path,
    offset: u64,
    max_bytes: usize,
) -> Result<ContentChunk, SourceError> {
    let format = AgentActivityContentFormat::Jsonl;
    let Some((mut file, len)) = open_for_read(path)? else {
        return Ok(empty_chunk(format, offset));
    };
    let (start, restarted) = resolve_start(offset, len);
    let buf = read_range(&mut file, start, max_bytes as u64, len).map_err(SourceError::Io)?;
    let reached_end = start + buf.len() as u64 >= len;

    if let Some(newline) = buf.iter().rposition(|byte| *byte == b'\n') {
        let mut cut = newline + 1;
        // 已到文件尾且最后一段是完整 JSON（只是没写换行）：一并给出。
        if reached_end && is_complete_json(&buf[cut..]) {
            cut = buf.len();
        }
        return Ok(chunk(
            format,
            &buf[..cut],
            start + cut as u64,
            reached_end,
            restarted,
        ));
    }

    if reached_end {
        if is_complete_json(&buf) {
            return Ok(chunk(format, &buf, len, true, restarted));
        }
        // 末行还没写完：留到下次，游标不动。
        return Ok(chunk(format, &[], start, true, restarted));
    }

    // 单行超过 max_bytes：给前缀，跳过这一行剩下的部分。
    let prefix = utf8_prefix_len(&buf);
    match find_line_end(&mut file, start + buf.len() as u64, len).map_err(SourceError::Io)? {
        Some(next) => Ok(chunk(format, &buf[..prefix], next, next >= len, true)),
        // 超长末行尚未写完：等它写完。
        None => Ok(chunk(format, &[], start, true, restarted)),
    }
}

/// 从 `from` 起找下一个换行，返回其后的位置；到文件尾都没有换行返回 `None`。超过
/// `MAX_LINE_SKIP_BYTES` 仍没有换行时直接跳到文件尾，保证调用方总能前进。
fn find_line_end(file: &mut File, from: u64, len: u64) -> io::Result<Option<u64>> {
    const BLOCK: u64 = 64 * 1024;
    let mut position = from;
    while position < len {
        if position - from > MAX_LINE_SKIP_BYTES {
            return Ok(Some(len));
        }
        let block = read_range(file, position, BLOCK, len)?;
        if block.is_empty() {
            break;
        }
        if let Some(newline) = block.iter().position(|byte| *byte == b'\n') {
            return Ok(Some(position + newline as u64 + 1));
        }
        position += block.len() as u64;
    }
    Ok(None)
}

fn is_complete_json(bytes: &[u8]) -> bool {
    let bytes = bytes.trim_ascii();
    !bytes.is_empty() && serde_json::from_slice::<serde::de::IgnoredAny>(bytes).is_ok()
}

/// 不切断末尾 UTF-8 字符的最长前缀；中间的非法字节交给有损解码。
fn utf8_prefix_len(bytes: &[u8]) -> usize {
    let len = bytes.len();
    for index in (len.saturating_sub(4)..len).rev() {
        let byte = bytes[index];
        if byte & 0xC0 == 0x80 {
            continue;
        }
        let width = match byte {
            0x00..=0x7F => 1,
            0xC0..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF7 => 4,
            _ => 1,
        };
        return if index + width > len { index } else { len };
    }
    len
}

// ---------------------------------------------------------------------------
// 文件与 JSON 小工具

/// 从 `start` 起最多读 `max` 字节（不超过 `len`）。
fn read_range(file: &mut File, start: u64, max: u64, len: u64) -> io::Result<Vec<u8>> {
    let want = max.min(len.saturating_sub(start));
    let mut buf = Vec::with_capacity(usize::try_from(want).unwrap_or(0));
    if want == 0 {
        return Ok(buf);
    }
    file.seek(SeekFrom::Start(start))?;
    file.take(want).read_to_end(&mut buf)?;
    Ok(buf)
}

fn read_capped_file(path: &Path, max_bytes: u64) -> Option<Vec<u8>> {
    let meta = fs::metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    if meta.len() > max_bytes {
        tracing::debug!(path = %path.display(), len = meta.len(), "kimi activity: file too large, skipped");
        return None;
    }
    match fs::read(path) {
        Ok(bytes) => Some(bytes),
        Err(error) => {
            tracing::debug!(path = %path.display(), %error, "kimi activity: unreadable file skipped");
            None
        }
    }
}

fn read_json_file(path: &Path, max_bytes: u64) -> Option<Value> {
    let bytes = read_capped_file(path, max_bytes)?;
    match serde_json::from_slice(&bytes) {
        Ok(value) => Some(value),
        Err(error) => {
            tracing::debug!(path = %path.display(), %error, "kimi activity: malformed JSON skipped");
            None
        }
    }
}

/// 名字合法（UTF-8）的子目录，按名排序。
fn list_dirs(dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut dirs: Vec<(String, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let name = entry.file_name().into_string().ok()?;
            path.is_dir().then_some((name, path))
        })
        .collect();
    dirs.sort();
    dirs
}

fn modified_ms(meta: &fs::Metadata) -> Option<u64> {
    let modified = meta.modified().ok()?;
    let since_epoch = modified.duration_since(UNIX_EPOCH).ok()?;
    u64::try_from(since_epoch.as_millis()).ok()
}

fn str_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)?
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty())
}

/// 毫秒时间戳字段：只认非负数字（整数或有限浮点）。
fn ms_field(value: &Value, key: &str) -> Option<u64> {
    let number = value.get(key)?.as_number()?;
    number.as_u64().or_else(|| {
        number
            .as_f64()
            .filter(|float| float.is_finite() && *float >= 0.0)
            .map(|float| float as u64)
    })
}

/// 第一行非空文本，控制字符换成空格（防止终端转义序列混进标签），按字符数截断。
fn one_line(text: &str, max_chars: usize) -> Option<String> {
    let line = text.lines().map(str::trim).find(|line| !line.is_empty())?;
    let cleaned: String = line
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect();
    let cleaned = cleaned.trim();
    (!cleaned.is_empty()).then(|| truncate_chars(cleaned, max_chars))
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut truncated: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    truncated.push('…');
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_resume::AgentSessionRef;

    const SESSION_ID: &str = "session_0f0f0f0f-1111-4222-8333-444455556666";
    const BROKEN_SESSION_ID: &str = "session_0b0b0b0b-aaaa-4bbb-8ccc-ddddeeeeffff";
    /// 远未来的时钟：「wire 最近有写入」恒为假，结果与夹具检出时间无关。
    const FAR_FUTURE_MS: u64 = u64::MAX;

    fn fixture_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/agent-activity/kimi/home/.kimi-code")
    }

    fn fixture_session_dir() -> PathBuf {
        fixture_root()
            .join("sessions/wd_demo_111b1182b4b0")
            .join(SESSION_ID)
    }

    fn context<'a>(
        home: &'a Path,
        session: Option<&'a AgentSessionRef>,
        cwd: Option<&'a Path>,
        now_ms: u64,
    ) -> SourceContext<'a> {
        SourceContext {
            codex_cache: None,
            agent: "kimi",
            session,
            cwd,
            home,
            now_ms,
            agent_config_dir: None,
            latest_hint: None,
        }
    }

    fn discover_by_id(root: &Path, id: &str, now_ms: u64) -> Vec<AgentActivityNode> {
        let session = AgentSessionRef::id(id).expect("合法会话 id");
        discover_in(root, &context(root, Some(&session), None, now_ms)).expect("discover")
    }

    fn ids(nodes: &[AgentActivityNode]) -> Vec<&str> {
        nodes.iter().map(|node| node.id.as_str()).collect()
    }

    fn node<'a>(nodes: &'a [AgentActivityNode], id: &str) -> &'a AgentActivityNode {
        nodes
            .iter()
            .find(|node| node.id == id)
            .unwrap_or_else(|| panic!("缺节点 {id}"))
    }

    fn read_all(
        root: &Path,
        cx: &SourceContext<'_>,
        node_id: &str,
        max_bytes: usize,
    ) -> Vec<ContentChunk> {
        let mut chunks = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..10_000 {
            let chunk = read_in(root, cx, node_id, cursor.as_deref(), max_bytes).expect("read");
            let eof = chunk.eof;
            cursor = chunk.next_cursor.clone();
            chunks.push(chunk);
            if eof {
                return chunks;
            }
        }
        panic!("分页没有收敛");
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0);
            let path = std::env::temp_dir().join(format!(
                "herdr-kimi-activity-{name}-{}-{nanos}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("建临时目录");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write_file(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().expect("有父目录")).expect("建目录");
        fs::write(path, content).expect("写文件");
    }

    fn set_mtime(path: &Path, ms: u64) {
        let time = UNIX_EPOCH + std::time::Duration::from_millis(ms);
        File::options()
            .write(true)
            .open(path)
            .and_then(|file| file.set_modified(time))
            .expect("设置修改时间");
    }

    #[test]
    fn explicit_kimi_path_rejects_foreign_metadata_and_never_falls_back() {
        let tmp = TempDir::new("path-identity");
        let directory = tmp.path().join(SESSION_ID);
        let wire = directory.join("agents/agent-1/wire.jsonl");
        write_file(&wire, "{\"type\":\"fixture\"}\n");
        let session = AgentSessionRef::path(directory.to_string_lossy()).unwrap();
        let root = fixture_root();
        let cx = context(
            &root,
            Some(&session),
            Some(Path::new("/synthetic/project")),
            FAR_FUTURE_MS,
        );
        for metadata in [
            serde_json::json!({"version": 2, "id": "session_other", "agents": {}}),
            serde_json::json!({"version": 2, "agents": {}}),
        ] {
            write_file(&directory.join("state.json"), &metadata.to_string());
            assert!(discover_in(&root, &cx).unwrap().is_empty());
            assert!(matches!(
                read_in(&root, &cx, "agent:agent-1", None, 1024),
                Err(SourceError::Unavailable)
            ));
        }
        for metadata in [
            serde_json::json!({"version": 2, "id": SESSION_ID, "agents": {}}),
            serde_json::json!({"workDir": "/synthetic/project", "agents": {}}),
        ] {
            write_file(&directory.join("state.json"), &metadata.to_string());
            assert!(!discover_in(&root, &cx).unwrap().is_empty());
            assert!(!read_in(&root, &cx, "agent:agent-1", None, 1024)
                .unwrap()
                .text
                .is_empty());
        }
        fs::remove_dir_all(&directory).unwrap();
        assert!(discover_in(&root, &cx).unwrap().is_empty());
        assert!(matches!(
            read_in(&root, &cx, "agent:agent-1", None, 1024),
            Err(SourceError::Unavailable)
        ));
    }

    #[test]
    fn discovers_the_fixture_tree_in_parent_first_order() {
        let nodes = discover_by_id(&fixture_root(), SESSION_ID, FAR_FUTURE_MS);
        assert_eq!(
            ids(&nodes),
            [
                "todo:main:0",
                "todo:main:1",
                "todo:main:2",
                "todo:main:3",
                "task:bash-cccc3333",
                "agent:agent-1",
                "agent:agent-2",
                "todo:agent-2:0",
                "task:bash-iiii9999",
                "agent:agent-3",
                "agent:agent-4",
                "task:bash-aaaa1111",
                "task:bash-eeee5555",
                "task:bash-bbbb2222",
                "agent:agent-5",
                "task:agent-orph0001",
                "task:question-qqqq1111",
                "agent:agent-8",
                "task:bash-oooo0000",
                "agent:agent-6",
                "agent:agent-7",
                "task:bash-empty000",
                "task:bash-zzzz9999",
            ]
        );
        // 父节点总在子节点之前，且每个 parent_id 都指向树里的节点。
        for (index, entry) in nodes.iter().enumerate() {
            if let Some(parent) = &entry.parent_id {
                let parent_index = nodes
                    .iter()
                    .position(|candidate| &candidate.id == parent)
                    .unwrap_or_else(|| panic!("{} 的父 {parent} 不在树里", entry.id));
                assert!(parent_index < index, "{} 出现在父节点之前", entry.id);
            }
        }
    }

    #[test]
    fn subagents_follow_state_json_parents_without_a_depth_limit() {
        let nodes = discover_by_id(&fixture_root(), SESSION_ID, FAR_FUTURE_MS);

        let explore = node(&nodes, "agent:agent-1");
        assert_eq!(explore.kind, AgentActivityKind::Subagent);
        assert_eq!(explore.parent_id, None);
        assert_eq!(explore.agent_type.as_deref(), Some("explore"));
        assert_eq!(explore.label, "agent-1");
        assert_eq!(explore.status, AgentActivityStatus::Done);
        assert_eq!(explore.started_at_ms, Some(1_790_000_110_000));
        assert_eq!(explore.ended_at_ms, Some(1_790_000_115_000));
        assert_eq!(
            explore.content_ref.as_deref(),
            Some("agents/agent-1/wire.jsonl")
        );

        // 后台子 agent：状态取最近一次运行，开始时间取最早一次，类型取 subagentType。
        let background = node(&nodes, "agent:agent-2");
        assert_eq!(background.status, AgentActivityStatus::Running);
        assert_eq!(background.label, "review diff");
        assert_eq!(background.agent_type.as_deref(), Some("coder"));
        assert_eq!(background.started_at_ms, Some(1_790_000_150_000));
        assert_eq!(background.ended_at_ms, None);

        // 深度 2 与 3；顶层 parentAgentId 优先于 labels 里的旧值。
        let nested = node(&nodes, "agent:agent-3");
        assert_eq!(nested.parent_id.as_deref(), Some("agent:agent-2"));
        assert_eq!(nested.agent_type.as_deref(), Some("coder"));
        assert_eq!(nested.status, AgentActivityStatus::Unknown);
        let deeper = node(&nodes, "agent:agent-4");
        assert_eq!(deeper.parent_id.as_deref(), Some("agent:agent-3"));
        assert_eq!(deeper.status, AgentActivityStatus::Failed);
        assert_eq!(deeper.ended_at_ms, Some(1_790_000_731_000));

        // 只有 labels 的父、swarmItem 作标签、`type` 不是首键的结束记录。
        let swarm = node(&nodes, "agent:agent-5");
        assert_eq!(swarm.parent_id, None);
        assert_eq!(swarm.label, "Check the parser module");
        assert_eq!(swarm.status, AgentActivityStatus::Done);
        assert_eq!(swarm.summary.as_deref(), Some("aborted"));

        // 父不存在、字段类型错、只有目录：都挂顶层，状态未知，不丢节点。
        for id in ["agent:agent-6", "agent:agent-7", "agent:agent-8"] {
            let orphan = node(&nodes, id);
            assert_eq!(orphan.parent_id, None, "{id}");
            assert_eq!(orphan.status, AgentActivityStatus::Unknown, "{id}");
        }
        assert_eq!(node(&nodes, "agent:agent-6").content_ref, None);
        assert!(nodes.iter().all(|entry| entry.id != "agent:main"));
        assert!(nodes.iter().all(|entry| !entry.id.contains("..")));
    }

    #[test]
    fn tasks_map_kind_status_and_summary() {
        let nodes = discover_by_id(&fixture_root(), SESSION_ID, FAR_FUTURE_MS);

        let tests = node(&nodes, "task:bash-aaaa1111");
        assert_eq!(tests.kind, AgentActivityKind::Background);
        assert_eq!(tests.status, AgentActivityStatus::Done);
        assert_eq!(tests.label, "cargo test");
        assert_eq!(tests.summary, None);
        assert_eq!(tests.started_at_ms, Some(1_790_000_400_000));
        assert_eq!(tests.ended_at_ms, Some(1_790_000_460_000));
        assert_eq!(
            tests.content_ref.as_deref(),
            Some("agents/main/tasks/bash-aaaa1111/output.log")
        );

        let dev = node(&nodes, "task:bash-bbbb2222");
        assert_eq!(dev.status, AgentActivityStatus::Running);
        assert_eq!(dev.label, "npm run dev");
        assert_eq!(dev.content_ref, None);

        let killed = node(&nodes, "task:bash-cccc3333");
        assert_eq!(killed.status, AgentActivityStatus::Done);
        assert_eq!(
            killed.summary.as_deref(),
            Some("killed: verification finished")
        );

        let lint = node(&nodes, "task:bash-eeee5555");
        assert_eq!(lint.status, AgentActivityStatus::Failed);
        assert_eq!(lint.summary.as_deref(), Some("exit 2"));

        let orphan = node(&nodes, "task:agent-orph0001");
        assert_eq!(orphan.kind, AgentActivityKind::Subagent);
        assert_eq!(orphan.status, AgentActivityStatus::Failed);
        assert_eq!(orphan.summary.as_deref(), Some("timed out"));
        assert_eq!(orphan.agent_type.as_deref(), Some("explore"));

        let question = node(&nodes, "task:question-qqqq1111");
        assert_eq!(question.kind, AgentActivityKind::Task);
        assert_eq!(question.status, AgentActivityStatus::Blocked);

        // 未知枚举值、非数字时间：落 Unknown，不 panic。
        let odd = node(&nodes, "task:bash-zzzz9999");
        assert_eq!(odd.kind, AgentActivityKind::Unknown);
        assert_eq!(odd.status, AgentActivityStatus::Unknown);
        assert_eq!(odd.summary.as_deref(), Some("status exploded"));
        assert_eq!(odd.started_at_ms, None);
        assert_eq!(odd.label, "bash-zzzz9999");

        // 空对象：种类按 id 前缀推断，其余字段缺省。
        let empty = node(&nodes, "task:bash-empty000");
        assert_eq!(empty.kind, AgentActivityKind::Background);
        assert_eq!(empty.status, AgentActivityStatus::Unknown);
        assert_eq!(empty.label, "bash-empty000");

        let lost = node(&nodes, "task:bash-oooo0000");
        assert_eq!(lost.parent_id.as_deref(), Some("agent:agent-8"));
        assert_eq!(lost.status, AgentActivityStatus::Unknown);
        assert_eq!(lost.summary.as_deref(), Some("lost"));

        // 坏 JSON 任务被跳过；已并入子 agent 的运行不重复出节点。
        for hidden in [
            "task:bash-dddd4444",
            "task:agent-k2k2k2k2",
            "task:agent-k3k3k3k3",
        ] {
            assert!(nodes.iter().all(|entry| entry.id != hidden), "{hidden}");
        }
    }

    #[test]
    fn todos_come_from_the_latest_todo_store_record() {
        let nodes = discover_by_id(&fixture_root(), SESSION_ID, FAR_FUTURE_MS);
        let todos: Vec<_> = nodes
            .iter()
            .filter(|entry| entry.kind == AgentActivityKind::Todo && entry.parent_id.is_none())
            .map(|entry| (entry.label.as_str(), entry.status))
            .collect();
        assert_eq!(
            todos,
            [
                ("Write parser tests", AgentActivityStatus::Done),
                // 控制字符被换成空格，只取第一行。
                ("Fix [31m color", AgentActivityStatus::Running),
                ("Ship it", AgentActivityStatus::Pending),
                ("todo 4", AgentActivityStatus::Unknown),
            ]
        );
        let child_todo = node(&nodes, "todo:agent-2:0");
        assert_eq!(child_todo.parent_id.as_deref(), Some("agent:agent-2"));
        assert_eq!(child_todo.label, "Read diff");
        assert_eq!(child_todo.status, AgentActivityStatus::Done);
        assert_eq!(child_todo.content_ref, None);
    }

    #[test]
    fn session_can_be_located_by_bare_id_or_by_cwd() {
        let root = fixture_root();
        let expected = ids(&discover_by_id(&root, SESSION_ID, FAR_FUTURE_MS))
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();

        let bare = discover_by_id(&root, "0f0f0f0f-1111-4222-8333-444455556666", FAR_FUTURE_MS);
        assert_eq!(ids(&bare), expected);

        let cwd = Path::new("/work/demo");
        let by_cwd =
            discover_in(&root, &context(&root, None, Some(cwd), FAR_FUTURE_MS)).expect("discover");
        assert_eq!(ids(&by_cwd), expected);

        let by_path = AgentSessionRef::path(fixture_session_dir().to_string_lossy().into_owned())
            .expect("绝对路径");
        let nodes = discover_in(&root, &context(&root, Some(&by_path), None, FAR_FUTURE_MS))
            .expect("discover");
        assert_eq!(ids(&nodes), expected);
    }

    #[test]
    fn missing_sessions_degrade_to_an_empty_tree() {
        let root = fixture_root();
        for id in ["session_missing", "../../etc", "session_0f0f/../x"] {
            assert!(discover_by_id(&root, id, FAR_FUTURE_MS).is_empty(), "{id}");
        }
        let nowhere = Path::new("/work/nowhere");
        let nodes = discover_in(&root, &context(&root, None, Some(nowhere), FAR_FUTURE_MS))
            .expect("discover");
        assert!(nodes.is_empty());

        // kimi 数据根整个不存在。
        let empty = TempDir::new("missing-root");
        let absent = empty.path().join(".kimi-code");
        assert!(discover_by_id(&absent, SESSION_ID, FAR_FUTURE_MS).is_empty());
    }

    #[test]
    fn a_context_without_session_or_cwd_is_unsupported() {
        let home = std::env::temp_dir();
        let cx = context(&home, None, None, 0);
        assert!(matches!(Kimi.discover(&cx), Err(SourceError::Unsupported)));
        assert!(matches!(
            Kimi.read(&cx, "agent:agent-1", None, 1024),
            Err(SourceError::Unsupported)
        ));
    }

    #[test]
    fn a_broken_state_json_still_yields_directory_agents_and_tasks() {
        let nodes = discover_by_id(&fixture_root(), BROKEN_SESSION_ID, FAR_FUTURE_MS);
        assert_eq!(ids(&nodes), ["task:bash-bbbb0001", "agent:agent-1"]);
        assert_eq!(node(&nodes, "agent:agent-1").parent_id, None);
        assert_eq!(
            node(&nodes, "task:bash-bbbb0001").status,
            AgentActivityStatus::Done
        );
    }

    #[test]
    fn a_foreground_subagent_is_running_only_while_its_wire_is_fresh() {
        let wire = fixture_session_dir().join("agents/agent-3/wire.jsonl");
        let modified = modified_ms(&fs::metadata(&wire).expect("wire")).expect("mtime");
        let fresh = discover_by_id(&fixture_root(), SESSION_ID, modified + 1_000);
        assert_eq!(
            node(&fresh, "agent:agent-3").status,
            AgentActivityStatus::Running
        );
        // 有结束记录的不受新鲜度影响。
        assert_eq!(
            node(&fresh, "agent:agent-1").status,
            AgentActivityStatus::Done
        );
        let stale = discover_by_id(&fixture_root(), SESSION_ID, modified + ACTIVE_WINDOW_MS + 1);
        assert_eq!(
            node(&stale, "agent:agent-3").status,
            AgentActivityStatus::Unknown
        );
    }

    #[test]
    fn activity_after_an_end_record_means_the_agent_kept_working() {
        let temp = TempDir::new("activity-after");
        let path = temp.path().join("wire.jsonl");
        write_file(
            &path,
            concat!(
                "{\"type\":\"metadata\",\"created_at\":1}\n",
                "{\"type\":\"turn.ended\",\"reason\":\"completed\",\"time\":2}\n",
                "{\"type\":\"context.append_loop_event\",\"event\":{\"type\":\"step.begin\"},\"time\":3}\n",
                "{\"type\":\"usage.record\",\"time\":4}\n",
            ),
        );
        set_mtime(&path, 10_000);
        let wire = scan_wire(&path, SUB_TAIL_BYTES, true);
        assert_eq!(wire.created_at_ms, Some(1));
        let lifecycle = wire.lifecycle.as_ref().expect("生命周期");
        assert!(lifecycle.activity_after);
        assert_eq!(
            foreground_status(&wire, 10_500).0,
            AgentActivityStatus::Running
        );
        assert_eq!(
            foreground_status(&wire, FAR_FUTURE_MS).0,
            AgentActivityStatus::Unknown
        );
    }

    #[test]
    fn created_at_is_read_from_a_first_line_longer_than_the_probe() {
        let temp = TempDir::new("long-head");
        let path = temp.path().join("wire.jsonl");
        let pad = "p".repeat(HEAD_PROBE_BYTES as usize + 100);
        write_file(
            &path,
            &format!(
                "{{\"type\":\"metadata\",\"pad\":\"{pad}\",\"created_at\":42}}\n{{\"type\":\"x\"}}\n"
            ),
        );
        assert_eq!(
            scan_wire(&path, SUB_TAIL_BYTES, true).created_at_ms,
            Some(42)
        );

        // 首行不是 metadata：不取时间。
        write_file(&path, "{\"type\":\"turn.prompt\",\"created_at\":42}\n");
        assert_eq!(scan_wire(&path, SUB_TAIL_BYTES, true).created_at_ms, None);
    }

    #[test]
    fn the_tail_window_bounds_what_the_wire_scan_can_see() {
        let temp = TempDir::new("tail-window");
        let path = temp.path().join("wire.jsonl");
        let filler = format!(
            "{{\"type\":\"usage.record\",\"pad\":\"{}\",\"time\":5}}\n",
            "x".repeat(4096)
        );
        write_file(
            &path,
            &format!(
                "{}{}{}",
                "{\"type\":\"metadata\",\"created_at\":1}\n",
                "{\"type\":\"turn.ended\",\"reason\":\"failed\",\"time\":2}\n",
                filler.repeat(4)
            ),
        );
        // 窗口够大：看得到结束记录。
        let wide = scan_wire(&path, 1024 * 1024, false);
        assert!(matches!(
            wide.lifecycle.as_ref().map(|lifecycle| lifecycle.outcome),
            Some(LifecycleOutcome::Ended(AgentActivityStatus::Failed, None))
        ));
        // 窗口只覆盖末尾填充行：没有生命周期记录，首部也不读。
        let narrow = scan_wire(&path, 6000, false);
        assert!(narrow.lifecycle.is_none());
        assert!(narrow.todos.is_none());
        assert_eq!(narrow.created_at_ms, None);
    }

    #[test]
    fn deep_chains_and_parent_cycles_keep_every_node_once() {
        let temp = TempDir::new("deep-chain");
        let session_dir = temp.path().join("session_deep");
        let mut agents = serde_json::Map::new();
        agents.insert("main".into(), serde_json::json!({"type": "main"}));
        for depth in 1..=40 {
            let parent = if depth == 1 {
                "main".to_string()
            } else {
                format!("agent-{}", depth - 1)
            };
            agents.insert(
                format!("agent-{depth}"),
                serde_json::json!({"type": "sub", "parentAgentId": parent}),
            );
        }
        agents.insert(
            "agent-50".into(),
            serde_json::json!({"type": "sub", "parentAgentId": "agent-51"}),
        );
        agents.insert(
            "agent-51".into(),
            serde_json::json!({"type": "sub", "parentAgentId": "agent-50"}),
        );
        agents.insert(
            "agent-60".into(),
            serde_json::json!({"type": "sub", "parentAgentId": "agent-60"}),
        );
        write_file(
            &session_dir.join("state.json"),
            &serde_json::json!({ "version": 2, "id": "session_deep", "agents": agents })
                .to_string(),
        );

        let session =
            AgentSessionRef::path(session_dir.to_string_lossy().into_owned()).expect("绝对路径");
        let root = temp.path().join(".kimi-code");
        let nodes = discover_in(&root, &context(&root, Some(&session), None, FAR_FUTURE_MS))
            .expect("discover");

        assert_eq!(nodes.len(), 43);
        let unique: HashSet<&str> = ids(&nodes).into_iter().collect();
        assert_eq!(unique.len(), 43);
        assert_eq!(node(&nodes, "agent:agent-1").parent_id, None);
        for depth in 2..=40 {
            assert_eq!(
                node(&nodes, &format!("agent:agent-{depth}"))
                    .parent_id
                    .as_deref(),
                Some(format!("agent:agent-{}", depth - 1).as_str())
            );
        }
        // 深链按前序连续输出。
        let order = ids(&nodes);
        let first = order
            .iter()
            .position(|id| *id == "agent:agent-1")
            .expect("链首");
        let chain: Vec<String> = (1..=40)
            .map(|depth| format!("agent:agent-{depth}"))
            .collect();
        let expected: Vec<&str> = chain.iter().map(String::as_str).collect();
        assert_eq!(order[first..first + 40].to_vec(), expected);
        // 环被剪开：至少一端挂顶层；自指挂顶层。
        let cycle_roots = ["agent:agent-50", "agent:agent-51"]
            .iter()
            .filter(|id| node(&nodes, id).parent_id.is_none())
            .count();
        assert!(cycle_roots >= 1);
        assert_eq!(node(&nodes, "agent:agent-60").parent_id, None);
    }

    #[test]
    fn the_session_index_is_preferred_and_cwd_picks_the_latest_session() {
        let temp = TempDir::new("index");
        let root = temp.path().join(".kimi-code");
        // 索引指向 sessions/ 之外的目录：按引用找会话时以索引为准。
        let relocated = temp.path().join("elsewhere").join("session_relocated");
        write_file(
            &relocated.join("agents/main/tasks/bash-rrrr0001.json"),
            "{\"kind\":\"process\",\"status\":\"running\"}",
        );

        let project = temp.path().join("project");
        fs::create_dir_all(&project).expect("项目目录");
        let project_text = project.to_string_lossy().into_owned();
        let bucket = root
            .join("sessions")
            .join(format!("wd_project_{}", work_dir_hash(&project_text)));
        let older = bucket.join("session_older");
        let newer = bucket.join("session_newer");
        for (dir, task, mtime) in [
            (&older, "bash-old00001", 1_000_000),
            (&newer, "bash-new00001", 2_000_000),
        ] {
            write_file(
                &dir.join(format!("agents/main/tasks/{task}.json")),
                "{\"kind\":\"process\",\"status\":\"completed\"}",
            );
            let wire = dir.join("agents/main/wire.jsonl");
            write_file(&wire, "{\"type\":\"metadata\",\"created_at\":1}\n");
            set_mtime(&wire, mtime);
        }
        let index = [
            serde_json::json!({
                "sessionId": "session_relocated",
                "sessionDir": relocated.to_string_lossy(),
                "workDir": "/somewhere/else",
            }),
            serde_json::json!({
                "sessionId": "session_older",
                "sessionDir": older.to_string_lossy(),
                "workDir": project_text,
            }),
        ]
        .iter()
        .map(|entry| format!("{entry}\n"))
        .collect::<String>();
        write_file(&root.join("session_index.jsonl"), &index);

        let by_id = discover_by_id(&root, "session_relocated", FAR_FUTURE_MS);
        assert_eq!(ids(&by_id), ["task:bash-rrrr0001"]);

        // cwd：索引只登记了旧会话，哈希桶里还有更新的会话，取最近活动的那个。
        let by_cwd = discover_in(&root, &context(&root, None, Some(&project), FAR_FUTURE_MS))
            .expect("discover");
        assert_eq!(ids(&by_cwd), ["task:bash-new00001"]);
    }

    #[test]
    fn task_output_pages_as_text_without_splitting_characters() {
        let root = fixture_root();
        let session = AgentSessionRef::id(SESSION_ID).expect("id");
        let cx = context(&root, Some(&session), None, FAR_FUTURE_MS);
        let expected = fs::read_to_string(
            fixture_session_dir().join("agents/main/tasks/bash-aaaa1111/output.log"),
        )
        .expect("输出");

        for max_bytes in [5, 16, 24, 4096] {
            let chunks = read_all(&root, &cx, "task:bash-aaaa1111", max_bytes);
            let text: String = chunks.iter().map(|chunk| chunk.text.as_str()).collect();
            assert_eq!(text, expected, "max_bytes={max_bytes}");
            assert!(!text.contains('\u{FFFD}'));
            for chunk in &chunks {
                assert_eq!(chunk.format, AgentActivityContentFormat::Text);
                assert!(chunk.text.len() <= max_bytes.max(MIN_READ_BYTES));
                assert!(!chunk.truncated);
                assert!(chunk.next_cursor.is_some());
            }
            if max_bytes >= 24 {
                // 预算放得下最长一行时只在换行处切。
                for chunk in &chunks[..chunks.len() - 1] {
                    assert!(chunk.text.ends_with('\n'), "{:?}", chunk.text);
                }
            }
        }

        // eof 之后继续拿游标读：空片段，仍给游标（跟随用）。
        let last = read_all(&root, &cx, "task:bash-aaaa1111", 4096);
        let cursor = last.last().and_then(|chunk| chunk.next_cursor.clone());
        let again =
            read_in(&root, &cx, "task:bash-aaaa1111", cursor.as_deref(), 4096).expect("read");
        assert!(again.text.is_empty() && again.eof);
        assert_eq!(again.next_cursor, cursor);

        // max_bytes 为 0 用默认预算，一次读完。
        let whole = read_in(&root, &cx, "task:bash-aaaa1111", None, 0).expect("read");
        assert_eq!(whole.text, expected);
        assert!(whole.eof);

        // 并入子 agent 的运行也能按 task id 读到输出。
        let run = read_in(&root, &cx, "task:agent-k2k2k2k2", None, 4096).expect("read");
        assert_eq!(run.text, "first review: no blocking issues");
    }

    #[test]
    fn agent_wire_pages_as_whole_jsonl_lines() {
        let root = fixture_root();
        let session = AgentSessionRef::id(SESSION_ID).expect("id");
        let cx = context(&root, Some(&session), None, FAR_FUTURE_MS);
        let expected = fs::read_to_string(fixture_session_dir().join("agents/agent-1/wire.jsonl"))
            .expect("wire");

        let chunks = read_all(&root, &cx, "agent:agent-1", 150);
        assert!(chunks.len() > 1);
        let text: String = chunks.iter().map(|chunk| chunk.text.as_str()).collect();
        assert_eq!(text, expected);
        for chunk in &chunks {
            assert_eq!(chunk.format, AgentActivityContentFormat::Jsonl);
            assert!(!chunk.truncated);
            for line in chunk.text.lines() {
                serde_json::from_str::<Value>(line).expect("整行 JSON");
            }
        }
    }

    #[test]
    fn oversized_and_unfinished_jsonl_lines() {
        let temp = TempDir::new("jsonl-edges");
        let root = temp.path().join(".kimi-code");
        let session_dir = root.join("sessions/wd_x_000000000000/session_edges");
        let wire = session_dir.join("agents/agent-9/wire.jsonl");
        let short = "{\"type\":\"a\",\"time\":1}\n";
        let long = format!("{{\"type\":\"b\",\"pad\":\"{}\"}}\n", "y".repeat(300));
        let tail = "{\"type\":\"c\",\"time\":3}\n";
        write_file(&wire, &format!("{short}{long}{tail}{{\"type\":\"turn"));
        let session = AgentSessionRef::id("session_edges").expect("id");
        let cx = context(&root, Some(&session), None, FAR_FUTURE_MS);
        let read =
            |cursor: Option<&str>| read_in(&root, &cx, "agent:agent-9", cursor, 64).expect("read");

        let first = read(None);
        assert_eq!(first.text, short);
        assert!(!first.eof && !first.truncated);

        // 超长行：只给前缀，跳到下一行开头。
        let second = read(first.next_cursor.as_deref());
        assert!(second.truncated);
        assert_eq!(second.text.len(), 64);
        assert!(long.starts_with(&second.text));
        let after_long = (short.len() + long.len()).to_string();
        assert_eq!(second.next_cursor.as_deref(), Some(after_long.as_str()));

        // 末行没写完：只给完整行，游标停在残行前。
        let third = read(second.next_cursor.as_deref());
        assert_eq!(third.text, tail);
        assert!(third.eof);
        let waiting = read(third.next_cursor.as_deref());
        assert!(waiting.text.is_empty() && waiting.eof);
        assert_eq!(waiting.next_cursor, third.next_cursor);

        // 残行写完后从同一游标读到它。
        let mut file = File::options().append(true).open(&wire).expect("追加");
        std::io::Write::write_all(&mut file, b".ended\",\"time\":4}").expect("写");
        let finished = read(third.next_cursor.as_deref());
        assert_eq!(finished.text, "{\"type\":\"turn.ended\",\"time\":4}");
        assert!(finished.eof);

        // 游标越过文件长度（文件被重写）：从头读并标 truncated。
        let restarted = read(Some("999999"));
        assert!(restarted.truncated);
        assert_eq!(restarted.text, short);
    }

    #[test]
    fn read_rejects_foreign_ids_and_bad_cursors() {
        let root = fixture_root();
        let session = AgentSessionRef::id(SESSION_ID).expect("id");
        let cx = context(&root, Some(&session), None, FAR_FUTURE_MS);
        for foreign in [
            "node",
            "agent:",
            "task:../../etc/passwd",
            "agent:agent-1/../main",
            "todo:main:0",
            "todo:main:x",
        ] {
            assert!(
                matches!(
                    read_in(&root, &cx, foreign, None, 1024),
                    Err(SourceError::Unsupported)
                ),
                "{foreign}"
            );
        }
        assert!(matches!(
            read_in(&root, &cx, "task:bash-aaaa1111", Some("abc"), 1024),
            Err(SourceError::Malformed(_))
        ));

        // 节点存在但没有内容文件：空片段而不是报错。
        let no_output = read_in(&root, &cx, "task:bash-cccc3333", None, 1024).expect("read");
        assert!(no_output.text.is_empty() && no_output.eof);
        let no_wire = read_in(&root, &cx, "agent:agent-6", None, 1024).expect("read");
        assert_eq!(no_wire.format, AgentActivityContentFormat::Jsonl);
        assert!(no_wire.text.is_empty() && no_wire.eof);

        // 会话目录还没落盘：`Unavailable`，不能是「空且已读完」。
        let absent = AgentSessionRef::id("session_absent").expect("id");
        let absent_cx = context(&root, Some(&absent), None, FAR_FUTURE_MS);
        assert!(matches!(
            read_in(&root, &absent_cx, "agent:main", None, 1024),
            Err(SourceError::Unavailable)
        ));
    }

    #[test]
    fn task_status_maps_all_six_kimi_states() {
        let task = |kind: TaskKind, status: &str| TaskMeta {
            owner: "main".into(),
            id: "bash-t0000000".into(),
            kind,
            status: Some(status.into()),
            description: None,
            command: None,
            agent_id: None,
            subagent_type: None,
            started_at_ms: None,
            ended_at_ms: None,
            exit_code: None,
            stop_reason: None,
            stop_code: None,
            has_output: false,
        };
        let cases = [
            ("running", AgentActivityStatus::Running, None),
            ("completed", AgentActivityStatus::Done, None),
            ("failed", AgentActivityStatus::Failed, None),
            ("killed", AgentActivityStatus::Done, Some("killed")),
            ("timed_out", AgentActivityStatus::Failed, Some("timed out")),
            ("lost", AgentActivityStatus::Unknown, Some("lost")),
        ];
        for (raw, status, summary) in cases {
            let (mapped, text) = task_status(&task(TaskKind::Process, raw));
            assert_eq!(mapped, status, "{raw}");
            assert_eq!(text.as_deref(), summary, "{raw}");
        }
        assert_eq!(
            task_status(&task(TaskKind::Question, "running")).0,
            AgentActivityStatus::Blocked
        );
        let mut coded = task(TaskKind::Agent, "failed");
        coded.stop_code = Some("provider.api_error".into());
        assert_eq!(task_status(&coded).1.as_deref(), Some("provider.api_error"));
        let mut running = task(TaskKind::Process, "running");
        running.stop_reason = Some("not yet".into());
        assert_eq!(task_status(&running).1, None);
    }

    #[test]
    fn record_type_probe_handles_spacing_nesting_and_escapes() {
        assert_eq!(
            probe_type_value(br#"{"type" : "turn.ended","time":1}"#).as_deref(),
            Some("turn.ended")
        );
        assert_eq!(probe_type_value(br#"{"type":"a\"b"}"#), None);
        assert_eq!(probe_type_value(br#"{"time":1}"#), None);
        // 嵌套对象里的 `type` 先出现时，完整解析复核会拒绝它。
        let nested = br#"{"message":{"type":"turn.ended"},"type":"agent.message.appended"}"#;
        assert_eq!(record_type(nested).as_deref(), Some("turn.ended"));
        assert!(parse_lifecycle("turn.ended", nested, false).is_none());
    }

    #[test]
    fn utf8_prefix_never_splits_a_character() {
        let text = "a完✓".as_bytes();
        for cut in 0..=text.len() {
            let prefix = &text[..cut];
            let len = utf8_prefix_len(prefix);
            assert!(std::str::from_utf8(&prefix[..len]).is_ok(), "cut={cut}");
            assert!(cut - len < 4);
        }
        assert_eq!(utf8_prefix_len(&[0xFF, 0xFE]), 2);
    }

    /// `KIMI_CODE_HOME` 把数据根挪出 home 时（环境变量与 `~` 的解析归 runtime 的
    /// `agent_config_dir`）：给了配置目录就用它，home 下没有 `.kimi-code` 也能找到
    /// 会话；不给则回退 `<home>/.kimi-code`；给了就只认它，不回退 home。
    #[test]
    fn the_runtime_config_dir_is_followed_instead_of_home() {
        let empty_home = TempDir::new("config-dir");
        let root = fixture_root();
        let session = AgentSessionRef::id(SESSION_ID).expect("合法会话 id");

        let relocated = SourceContext {
            codex_cache: None,
            agent_config_dir: Some(&root),
            ..context(empty_home.path(), Some(&session), None, FAR_FUTURE_MS)
        };
        let nodes = Kimi.discover(&relocated).expect("配置目录下的会话可发现");
        assert!(!nodes.is_empty());
        assert_eq!(nodes, discover_by_id(&root, SESSION_ID, FAR_FUTURE_MS));
        let page = Kimi
            .read(&relocated, "agent:agent-1", None, 64 * 1024)
            .expect("配置目录下的 wire 可读");
        assert!(!page.text.is_empty());

        // 不给配置目录：回退 `<home>/.kimi-code`，空 home 下什么都没有。
        let fallback = context(empty_home.path(), Some(&session), None, FAR_FUTURE_MS);
        assert!(Kimi.discover(&fallback).expect("缺目录不报错").is_empty());
        assert!(matches!(
            Kimi.read(&fallback, "agent:agent-1", None, 1024),
            Err(SourceError::Unavailable)
        ));

        // 给了配置目录就只认它：home 下明明有数据也不回头找。
        let fixture_home = root.parent().expect("夹具数据根有父目录").to_path_buf();
        let nowhere = empty_home.path().join("nowhere");
        let pinned = SourceContext {
            codex_cache: None,
            agent_config_dir: Some(&nowhere),
            ..context(&fixture_home, Some(&session), None, FAR_FUTURE_MS)
        };
        assert!(Kimi.discover(&pinned).expect("缺目录不报错").is_empty());
    }

    #[test]
    fn work_dir_hash_matches_the_kimi_bucket_rule() {
        // 夹具桶名由 `sha256("/work/demo")[:12]` 独立算出（Python hashlib）。
        assert_eq!(work_dir_hash("/work/demo"), "111b1182b4b0");
        assert_eq!(work_dir_hash("/work/broken"), "df7dd69c03fc");
    }
}
