//! Codex CLI 的活动来源适配器：从 `<config>/sessions/YYYY/MM/DD/rollout-*.jsonl`
//! （`<config>` 跟随 `CODEX_HOME`，缺省 `<home>/.codex`）读出以本 pane 线程为祖先的
//! 子线程（`thread_spawn` 子 agent、guardian 审核等），`read` 按字节游标分页返回
//! 子线程自己的 rollout（JSONL 原文）。
//!
//! # 本机取证（codex-cli 0.155.1，2026-09-22，267 个 rollout；只看 type 与键名）
//!
//! - 目录：`sessions/<YYYY>/<MM>/<DD>/rollout-<YYYY-MM-DDTHH-MM-SS>-<thread id>.jsonl`。
//!   文件名时间与日期目录是**本地时间**（267/267 与 UUIDv7 毫秒之差恰为时区偏移），
//!   记录内 `timestamp` 才是 UTC RFC3339；压缩变体后缀 `.jsonl.zst`（二进制字符串
//!   `compressed rollout reader` / `archived_sessions`），本适配器不解压。
//! - 每个文件第 0 行必是自己的 `session_meta`（267/267，字节偏移 0，长度 < 64 KiB）；
//!   子线程第 1 行是父线程 `session_meta` 的副本（138 例），其后跟父历史前缀——
//!   所以只读第 0 行。
//! - `session_meta.payload` 键：`id`、`timestamp`、`cwd`、`cli_version`、`source`、
//!   `thread_source`、`parent_thread_id`、`agent_nickname`、`agent_role`、`agent_path`、
//!   `forked_from_id`、`history_mode`、`multi_agent_version`、
//!   `subagent_history_start_ordinal`、`base_instructions`、`git`、`originator`、
//!   `model_provider`、`context_window`、`runtime_workspace_roots`、`session_id`。
//!   取值集合：`thread_source` ∈ user | subagent | guardian_review；`source` ∈ `"cli"` |
//!   `{"subagent":{"thread_spawn":{parent_thread_id, depth, agent_path, agent_nickname,
//!   agent_role}}}` | `{"subagent":{"other":"guardian"}}`（app-server schema 另列
//!   review / compact / memory_consolidation 字符串变体，本机未见）；`agent_role` 见
//!   worker / explorer / default / 自定义名 / null；`agent_nickname` 与 `agent_path`
//!   可为 null；`depth` 见 1、2（无深度上限承诺，树按任意深度建）。
//! - 生命周期（子线程自己的文件）：`event_msg` 的 `task_started{turn_id, started_at}` →
//!   `task_complete{turn_id, started_at, completed_at, duration_ms, error,
//!   last_agent_message}` 或 `turn_aborted{reason: "interrupted", started_at,
//!   completed_at, duration_ms}`；时间是 unix 秒。164 个子线程里最后一个标记距 EOF
//!   < 16 KiB 的 159 个，其余 4 个是 `task_started` 之后 ≥ 256 KiB 的进行中输出——
//!   所以只看尾窗口：窗口里没有标记且文件比窗口大即视为进行中。
//! - 父线程侧 `item_completed.item.type == "SubAgentActivity"`（PascalCase；键
//!   `agent_thread_id`、`agent_path`、`kind` ∈ started | interacted | interrupted |
//!   completed，`interacted` 占 86%）只在 12/19 个父文件里出现，不作状态来源。
//! - app-server（`codex app-server --listen stdio:// -c mcp_servers={}` 实连两次，
//!   第二次 275 个 rollout）：`initialize` 1.4 s；`thread/list` 无参返回
//!   `{backwardsCursor, data, nextCursor}`，25 条全是 `source: "cli"`、
//!   `status.type: "notLoaded"`、顶层 `threadSource: null`；`sourceKinds` 只接受
//!   cli | vscode | exec | appServer | subAgent | subAgentReview | subAgentCompact |
//!   subAgentThreadSpawn | subAgentOther | unknown，传 `["subAgentThreadSpawn"]` 用时
//!   5.8 s 返回 **0 条**（盘上 160+ 个子线程）。`thread/read {threadId,
//!   includeTurns:false}` 对子线程可用：线程键 `id`、`parentThreadId`、`agentNickname`、
//!   `agentRole`、`threadSource: "subagent"`、`source.subAgent.thread_spawn{
//!   parent_thread_id, depth, agent_path, agent_nickname, agent_role}`（外层 camelCase、
//!   内层 snake_case）、`status`、`createdAt`、`updatedAt`、`recencyAt`、`cwd`、`path`、
//!   `preview`、`name`、`model`、`turns` 等，顶层没有 `agentPath`；值与 rollout 首行
//!   逐字段一致。短命进程只收到 `remoteControl/status/changed` 一条通知。
//!   结论：app-server 既列不出子线程也给不出状态，本适配器不起 app-server，树与
//!   状态全部来自 rollout（与任务书「首选 app-server」的偏离，见交付说明）。
//! - 计划条目：rollout 只持久化 `item_completed.item.type == "Plan"`（键 `type` /
//!   `id` / `text`，20 个文件），275 个文件零条 `plan_update` / `update_plan`；
//!   `turn/plan/updated` 只是 app-server 通知，短命进程收不到 → 本版本不产出 Todo
//!   节点。
//! - 钩子（二进制 `HookEventsToml` 字段表）：0.155.1 的 `hooks.json` 认 PreToolUse |
//!   PermissionRequest | PostToolUse | PreCompact | PostCompact | SessionStart |
//!   SessionEnd | SubagentStart | SubagentStop | Interrupt，需 `[features] hooks = true`
//!   （herdr 安装 codex 集成时已写入）。内嵌 schema：`SubagentStart` 输入必填
//!   `agent_id`、`agent_type`、`cwd`、`hook_event_name`、`model`、`permission_mode`、
//!   `session_id`、`transcript_path`、`turn_id`；`SubagentStop` 另有
//!   `agent_transcript_path`、`last_assistant_message`、`stop_hook_active`。
//!   `agent_id` 即子线程 id，可直接作本适配器的节点 id。

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{ActivitySource, ContentChunk, SourceContext, SourceError};
use crate::api::schema::{
    AgentActivityContentFormat, AgentActivityKind, AgentActivityNode, AgentActivityStatus,
};

pub(super) struct Codex;

/// 一次 `discover` 最多打开的 rollout 文件数（每个只读首行）。
const MAX_ROLLOUTS_SCANNED: usize = 2048;
/// `session_meta` 行的读取上限（含 `base_instructions`；本机最大不到 64 KiB）。
const MAX_META_LINE_BYTES: u64 = 1024 * 1024;
/// 状态推断只看文件尾部这么多字节。
const STATUS_TAIL_BYTES: u64 = 256 * 1024;
/// `read` 的默认页与页上限。
const DEFAULT_READ_BYTES: usize = 64 * 1024;
const MAX_READ_BYTES: usize = 4 * 1024 * 1024;
/// 树深度上限，只用来防父链成环。
const MAX_TREE_DEPTH: usize = 32;
const ROLLOUT_PREFIX: &str = "rollout-";
/// 文件名里 `YYYY-MM-DDTHH-MM-SS-` 的长度。
const ROLLOUT_STAMP_LEN: usize = 20;
const ROLLOUT_SUFFIX: &str = ".jsonl";
const COMPRESSED_SUFFIX: &str = ".jsonl.zst";

impl ActivitySource for Codex {
    fn id(&self) -> &'static str {
        "codex"
    }

    fn discover(&self, cx: &SourceContext<'_>) -> Result<Vec<AgentActivityNode>, SourceError> {
        // codex 只能按线程 id 定位会话：钩子还没上报 id 时不支持发现。
        let Some(root_id) = root_thread_id(cx) else {
            return Err(SourceError::Unsupported);
        };
        let sessions = sessions_dir(cx);
        if !sessions.is_dir() {
            return Ok(Vec::new());
        }
        // 子线程在父线程之后创建；目录按本地日期分桶，UTC 前一天起扫对任何时区都安全。
        let min_day = uuid_v7_millis(&root_id)
            .map(|millis| millis.saturating_sub(86_400_000))
            .and_then(utc_day);
        let metas = scan_metas(&sessions, min_day).map_err(SourceError::Io)?;
        let mut nodes = build_tree(&root_id, &metas);
        for node in &mut nodes {
            let Some(meta) = metas.iter().find(|meta| meta.id == node.id) else {
                continue;
            };
            let lifecycle = lifecycle_from_tail(&meta.path, STATUS_TAIL_BYTES).unwrap_or_else(|error| {
                tracing::debug!(path = %meta.path.display(), %error, "codex rollout 尾部不可读，状态记未知");
                Lifecycle::Unreadable
            });
            apply_lifecycle(node, &lifecycle);
        }
        Ok(nodes)
    }

    fn read(
        &self,
        cx: &SourceContext<'_>,
        node_id: &str,
        cursor: Option<&str>,
        max_bytes: usize,
    ) -> Result<ContentChunk, SourceError> {
        // 节点 id 本身就是子线程 id，定位不依赖会话；但没有会话引用的 pane 根本不会有
        // 节点可读，统一口径下同样报不支持。
        if root_thread_id(cx).is_none() {
            return Err(SourceError::Unsupported);
        }
        if !is_plausible_thread_id(node_id) {
            return Err(SourceError::Malformed(format!(
                "codex node id 不像线程 id：{node_id:?}"
            )));
        }
        let sessions = sessions_dir(cx);
        let path = find_rollout(&sessions, node_id)?;
        read_page(&path, cursor, max_bytes)
    }
}

// ---- 会话定位 ----

/// 会话库根目录，本适配器所有「按配置目录拼路径」的唯一出口：runtime 解析好的
/// 配置目录（`SourceContext::agent_config_dir`，跟随 `CODEX_HOME`，口径同
/// `integration::env::codex_dir`）优先，缺省才回退 `<home>/.codex`。给了配置目录
/// 就只认它、不再回头找 home。
fn sessions_dir(cx: &SourceContext<'_>) -> PathBuf {
    match cx.agent_config_dir {
        Some(config_dir) => config_dir.join("sessions"),
        None => cx.home.join(".codex").join("sessions"),
    }
}

/// pane 的根线程 id：钩子上报的是 id；给的是路径时从文件名取。
fn root_thread_id(cx: &SourceContext<'_>) -> Option<String> {
    use crate::agent_resume::AgentSessionRefKind;
    let session = cx.session?;
    match session.kind {
        AgentSessionRefKind::Id => {
            is_plausible_thread_id(&session.value).then(|| session.value.clone())
        }
        AgentSessionRefKind::Path => Path::new(&session.value)
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(rollout_file_id)
            .map(|(id, _)| id.to_string()),
    }
}

/// 线程 id 只会是 UUID 一类的字符；这里也顺手挡住路径成分。
fn is_plausible_thread_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// 从 `rollout-<stamp>-<id>.jsonl[.zst]` 取 id；返回 `(id, 是否压缩)`。
fn rollout_file_id(name: &str) -> Option<(&str, bool)> {
    let rest = name.strip_prefix(ROLLOUT_PREFIX)?;
    let (stem, compressed) = if let Some(stem) = rest.strip_suffix(COMPRESSED_SUFFIX) {
        (stem, true)
    } else {
        (rest.strip_suffix(ROLLOUT_SUFFIX)?, false)
    };
    if stem.len() <= ROLLOUT_STAMP_LEN || !stem.is_char_boundary(ROLLOUT_STAMP_LEN) {
        return None;
    }
    let (stamp, id) = stem.split_at(ROLLOUT_STAMP_LEN);
    if !stamp.ends_with('-') || !is_plausible_thread_id(id) {
        return None;
    }
    Some((id, compressed))
}

/// UUIDv7 的高 48 位是 unix 毫秒；不是 v7 时返回 `None`。
fn uuid_v7_millis(id: &str) -> Option<u64> {
    let hex: String = id.chars().filter(|ch| *ch != '-').collect();
    if hex.len() != 32 || hex.as_bytes()[12] != b'7' {
        return None;
    }
    u64::from_str_radix(&hex[..12], 16).ok()
}

/// unix 毫秒对应的 UTC 日期 `(年, 月, 日)`。
fn utc_day(millis: u64) -> Option<(u32, u32, u32)> {
    let seconds = i64::try_from(millis / 1000).ok()?;
    let date = time::OffsetDateTime::from_unix_timestamp(seconds)
        .ok()?
        .date();
    Some((
        u32::try_from(date.year()).ok()?,
        u32::from(u8::from(date.month())),
        u32::from(date.day()),
    ))
}

fn numeric_dir_name(entry: &fs::DirEntry) -> Option<u32> {
    let name = entry.file_name();
    let name = name.to_str()?;
    if name.is_empty() || name.len() > 4 || !name.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    name.parse().ok()
}

/// `sessions/YYYY/MM/DD` 日期目录，按日期从新到旧；`min_day` 之前的跳过。
fn day_dirs(sessions: &Path, min_day: Option<(u32, u32, u32)>) -> io::Result<Vec<PathBuf>> {
    let mut days = Vec::new();
    for year in fs::read_dir(sessions)?.filter_map(Result::ok) {
        let Some(year_number) = numeric_dir_name(&year) else {
            continue;
        };
        let Ok(months) = fs::read_dir(year.path()) else {
            continue;
        };
        for month in months.filter_map(Result::ok) {
            let Some(month_number) = numeric_dir_name(&month) else {
                continue;
            };
            let Ok(day_entries) = fs::read_dir(month.path()) else {
                continue;
            };
            for day in day_entries.filter_map(Result::ok) {
                let Some(day_number) = numeric_dir_name(&day) else {
                    continue;
                };
                let key = (year_number, month_number, day_number);
                if min_day.is_some_and(|min| key < min) {
                    continue;
                }
                days.push((key, day.path()));
            }
        }
    }
    days.sort_by_key(|(key, _)| std::cmp::Reverse(*key));
    Ok(days.into_iter().map(|(_, path)| path).collect())
}

// ---- session_meta ----

/// 子线程来源；`source` 未识别的形态落 `Unknown`，节点照样保留。
#[derive(Debug, Clone, PartialEq, Eq)]
enum ThreadOrigin {
    /// 顶层线程（`"cli"` / `"vscode"` 等字符串来源），不会成为节点。
    TopLevel,
    /// `{"subagent":{"thread_spawn":{…}}}`：AgentControl 派生的子 agent。
    ThreadSpawn,
    /// `{"subagent":"review"}` 一类字符串变体，或 `{"subagent":{"other":"guardian"}}`。
    Named(String),
    Unknown,
}

#[derive(Debug, Clone)]
struct RolloutMeta {
    id: String,
    parent_id: Option<String>,
    path: PathBuf,
    started_at_ms: Option<u64>,
    origin: ThreadOrigin,
    nickname: Option<String>,
    role: Option<String>,
    agent_path: Option<String>,
}

/// 只读首行；任何一步失败都只是跳过这个文件。
fn read_meta(path: &Path) -> Option<RolloutMeta> {
    let file = fs::File::open(path).ok()?;
    let mut line = Vec::new();
    let mut reader = BufReader::new(file).take(MAX_META_LINE_BYTES);
    let length = reader.read_until(b'\n', &mut line).ok()?;
    if length == 0 || (length as u64 >= MAX_META_LINE_BYTES && !line.ends_with(b"\n")) {
        return None;
    }
    let record: Value = serde_json::from_slice(&line).ok()?;
    if record.get("type").and_then(Value::as_str) != Some("session_meta") {
        return None;
    }
    parse_meta(record.get("payload")?, path)
}

fn parse_meta(payload: &Value, path: &Path) -> Option<RolloutMeta> {
    let id = payload.get("id").and_then(Value::as_str)?;
    if !is_plausible_thread_id(id) {
        return None;
    }
    let source = payload.get("source");
    let spawn = source
        .and_then(|source| source.get("subagent"))
        .and_then(|subagent| subagent.get("thread_spawn"));
    let origin = match source {
        None => ThreadOrigin::Unknown,
        Some(Value::String(_)) => ThreadOrigin::TopLevel,
        Some(Value::Object(object)) => match object.get("subagent") {
            Some(Value::String(name)) => ThreadOrigin::Named(name.clone()),
            Some(Value::Object(subagent)) if subagent.contains_key("thread_spawn") => {
                ThreadOrigin::ThreadSpawn
            }
            Some(Value::Object(subagent)) => subagent
                .get("other")
                .and_then(Value::as_str)
                .map(|name| ThreadOrigin::Named(name.to_string()))
                .unwrap_or(ThreadOrigin::Unknown),
            _ => ThreadOrigin::Unknown,
        },
        Some(_) => ThreadOrigin::Unknown,
    };
    let parent_id = payload
        .get("parent_thread_id")
        .and_then(Value::as_str)
        .or_else(|| {
            spawn
                .and_then(|spawn| spawn.get("parent_thread_id"))
                .and_then(Value::as_str)
        })
        .filter(|parent| is_plausible_thread_id(parent))
        .map(str::to_string);
    let text = |name: &str| -> Option<String> {
        payload
            .get(name)
            .and_then(Value::as_str)
            .or_else(|| {
                spawn
                    .and_then(|spawn| spawn.get(name))
                    .and_then(Value::as_str)
            })
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    let started_at_ms = payload
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(rfc3339_millis)
        .or_else(|| uuid_v7_millis(id));
    Some(RolloutMeta {
        id: id.to_string(),
        parent_id,
        path: path.to_path_buf(),
        started_at_ms,
        origin,
        nickname: text("agent_nickname"),
        role: text("agent_role"),
        agent_path: text("agent_path"),
    })
}

fn rfc3339_millis(text: &str) -> Option<u64> {
    let date =
        time::OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339).ok()?;
    let nanos = date.unix_timestamp_nanos();
    u64::try_from(nanos / 1_000_000).ok()
}

/// 读 `min_day` 起每个日期目录里 rollout 的首行；从新到旧，超过预算就停。
fn scan_metas(sessions: &Path, min_day: Option<(u32, u32, u32)>) -> io::Result<Vec<RolloutMeta>> {
    let mut metas = Vec::new();
    let mut budget = MAX_ROLLOUTS_SCANNED;
    for day in day_dirs(sessions, min_day)? {
        let Ok(entries) = fs::read_dir(&day) else {
            continue;
        };
        let mut files: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .and_then(rollout_file_id)
                    .is_some_and(|(_, compressed)| !compressed)
            })
            .collect();
        files.sort();
        for path in files.into_iter().rev() {
            if budget == 0 {
                tracing::debug!(
                    limit = MAX_ROLLOUTS_SCANNED,
                    "codex rollout 扫描达到预算上限"
                );
                return Ok(metas);
            }
            budget -= 1;
            if let Some(meta) = read_meta(&path) {
                metas.push(meta);
            }
        }
    }
    Ok(metas)
}

// ---- 建树 ----

/// 从根线程出发按 `parent_thread_id` 逐层收集；直接子线程 `parent_id` 为空，更深的
/// 指向父节点。同层按创建时间再按 id 排序。
fn build_tree(root_id: &str, metas: &[RolloutMeta]) -> Vec<AgentActivityNode> {
    let mut children: HashMap<&str, Vec<&RolloutMeta>> = HashMap::new();
    for meta in metas {
        if meta.origin == ThreadOrigin::TopLevel || meta.id == root_id {
            continue;
        }
        if let Some(parent) = meta.parent_id.as_deref() {
            children.entry(parent).or_default().push(meta);
        }
    }
    for siblings in children.values_mut() {
        // 创建时间未知的排最后，再按 id 稳定。
        siblings.sort_by_key(|meta| {
            (
                meta.started_at_ms.is_none(),
                meta.started_at_ms,
                meta.id.clone(),
            )
        });
    }
    let mut nodes = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    seen.insert(root_id);
    let mut queue: VecDeque<(&str, usize)> = VecDeque::new();
    queue.push_back((root_id, 0));
    while let Some((parent, depth)) = queue.pop_front() {
        if depth >= MAX_TREE_DEPTH {
            continue;
        }
        let Some(siblings) = children.get(parent) else {
            continue;
        };
        for meta in siblings {
            if !seen.insert(meta.id.as_str()) {
                continue;
            }
            nodes.push(node_from_meta(
                meta,
                (depth > 0).then(|| parent.to_string()),
            ));
            queue.push_back((meta.id.as_str(), depth + 1));
        }
    }
    nodes
}

fn node_from_meta(meta: &RolloutMeta, parent_id: Option<String>) -> AgentActivityNode {
    let kind = match &meta.origin {
        ThreadOrigin::ThreadSpawn => AgentActivityKind::Subagent,
        ThreadOrigin::Named(_) => AgentActivityKind::Background,
        ThreadOrigin::TopLevel | ThreadOrigin::Unknown => AgentActivityKind::Unknown,
    };
    let agent_type = meta.role.clone().or_else(|| match &meta.origin {
        ThreadOrigin::Named(name) => Some(name.clone()),
        _ => None,
    });
    AgentActivityNode {
        id: meta.id.clone(),
        kind,
        label: label_for(meta),
        status: AgentActivityStatus::Unknown,
        parent_id,
        agent_type,
        content_ref: Some(meta.id.clone()),
        summary: meta.agent_path.clone(),
        started_at_ms: meta.started_at_ms,
        ended_at_ms: None,
    }
}

/// 显示名回退链：昵称 → 角色 → `agent_path` 末段 → 来源名 → 线程 id。
fn label_for(meta: &RolloutMeta) -> String {
    if let Some(nickname) = &meta.nickname {
        return nickname.clone();
    }
    if let Some(role) = &meta.role {
        return role.clone();
    }
    if let Some(segment) = meta
        .agent_path
        .as_deref()
        .and_then(|path| path.rsplit('/').find(|segment| !segment.trim().is_empty()))
    {
        return segment.to_string();
    }
    if let ThreadOrigin::Named(name) = &meta.origin {
        return name.clone();
    }
    meta.id.clone()
}

// ---- 状态 ----

#[derive(Debug, Clone, PartialEq, Eq)]
enum Lifecycle {
    /// 文件里还没有任何回合标记。
    Idle,
    /// 最后一个标记是 `task_started`，或尾窗口里全是进行中的输出。
    Running,
    Completed {
        ended_at_ms: Option<u64>,
        error: Option<String>,
    },
    Aborted {
        ended_at_ms: Option<u64>,
        reason: Option<String>,
    },
    Unreadable,
}

/// 只看尾窗口的回合标记（见文件头「生命周期」）。
fn lifecycle_from_tail(path: &Path, tail_bytes: u64) -> io::Result<Lifecycle> {
    let mut file = fs::File::open(path)?;
    let length = file.metadata()?.len();
    let start = length.saturating_sub(tail_bytes);
    file.seek(SeekFrom::Start(start))?;
    let mut buffer = Vec::with_capacity((length - start) as usize);
    file.read_to_end(&mut buffer)?;
    let mut window: &[u8] = &buffer;
    if start > 0 {
        // 窗口起点多半落在一行中间，丢掉半行。
        window = match window.iter().position(|byte| *byte == b'\n') {
            Some(newline) => &window[newline + 1..],
            None => &[],
        };
    }
    let mut last = None;
    for line in window.split(|byte| *byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        let Ok(record) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        if record.get("type").and_then(Value::as_str) != Some("event_msg") {
            continue;
        }
        let Some(payload) = record.get("payload") else {
            continue;
        };
        let ended_at_ms = || unix_seconds_to_millis(payload.get("completed_at"));
        match payload.get("type").and_then(Value::as_str) {
            Some("task_started") => last = Some(Lifecycle::Running),
            Some("task_complete") => {
                let error = payload.get("error").filter(|error| !error.is_null());
                last = Some(Lifecycle::Completed {
                    ended_at_ms: ended_at_ms(),
                    error: error.map(error_message),
                });
            }
            Some("turn_aborted") => {
                last = Some(Lifecycle::Aborted {
                    ended_at_ms: ended_at_ms(),
                    reason: payload
                        .get("reason")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                });
            }
            _ => {}
        }
    }
    Ok(match last {
        Some(lifecycle) => lifecycle,
        // 窗口被一个回合的输出填满而没有任何标记：这一回合还没结束。
        None if start > 0 => Lifecycle::Running,
        None => Lifecycle::Idle,
    })
}

fn unix_seconds_to_millis(value: Option<&Value>) -> Option<u64> {
    let seconds = value?.as_u64()?;
    seconds.checked_mul(1000)
}

fn error_message(error: &Value) -> String {
    error
        .get("message")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| error.as_str().map(str::to_string))
        .unwrap_or_else(|| error.to_string())
}

fn apply_lifecycle(node: &mut AgentActivityNode, lifecycle: &Lifecycle) {
    match lifecycle {
        Lifecycle::Idle => node.status = AgentActivityStatus::Pending,
        Lifecycle::Running => node.status = AgentActivityStatus::Running,
        Lifecycle::Completed {
            ended_at_ms,
            error: None,
        } => {
            node.status = AgentActivityStatus::Done;
            node.ended_at_ms = *ended_at_ms;
        }
        Lifecycle::Completed {
            ended_at_ms,
            error: Some(message),
        } => {
            node.status = AgentActivityStatus::Failed;
            node.ended_at_ms = *ended_at_ms;
            node.summary = Some(message.clone());
        }
        Lifecycle::Aborted {
            ended_at_ms,
            reason,
        } => {
            node.status = AgentActivityStatus::Failed;
            node.ended_at_ms = *ended_at_ms;
            node.summary = Some(reason.clone().unwrap_or_else(|| "interrupted".to_string()));
        }
        Lifecycle::Unreadable => node.status = AgentActivityStatus::Unknown,
    }
}

// ---- 内容读取 ----

/// 只列目录、不打开文件；同一 id 同时有明文与 `.zst` 时取明文。会话库或该线程的
/// rollout 还没生成时报 `Unavailable`（稍后重试），只剩压缩变体时报 `Unsupported`。
fn find_rollout(sessions: &Path, id: &str) -> Result<PathBuf, SourceError> {
    if !sessions.is_dir() {
        return Err(SourceError::Unavailable);
    }
    // 线程 id 是 UUIDv7 时只需从它创建那天（UTC 前一天起，见 `discover`）往后找。
    let min_day = uuid_v7_millis(id)
        .map(|millis| millis.saturating_sub(86_400_000))
        .and_then(utc_day);
    let mut compressed = None;
    for day in day_dirs(sessions, min_day).map_err(SourceError::Io)? {
        let Ok(entries) = fs::read_dir(&day) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let name = entry.file_name();
            let Some((file_id, is_compressed)) = name.to_str().and_then(rollout_file_id) else {
                continue;
            };
            if file_id != id {
                continue;
            }
            if is_compressed {
                compressed = Some(entry.path());
            } else {
                return Ok(entry.path());
            }
        }
    }
    if compressed.is_some() {
        return Err(SourceError::Unsupported);
    }
    tracing::debug!(thread = id, "codex 线程还没有 rollout 文件");
    Err(SourceError::Unavailable)
}

/// 游标是字节偏移；页尾对齐到最后一个换行，单行超过页大小时截在字符边界并标记
/// `truncated`。`next_cursor` 总是给出（含 `eof`），`eof` 只表示这次已读到文件末尾；
/// 末尾不以换行结尾的半截行不消费，游标停在它的行首，跟随增长时用同一游标再读。
fn read_page(
    path: &Path,
    cursor: Option<&str>,
    max_bytes: usize,
) -> Result<ContentChunk, SourceError> {
    let offset = match cursor {
        None => 0,
        Some(cursor) => cursor.trim().parse::<u64>().map_err(|_| {
            SourceError::Malformed(format!("codex 内容游标不是字节偏移：{cursor:?}"))
        })?,
    };
    let budget = if max_bytes == 0 {
        DEFAULT_READ_BYTES
    } else {
        max_bytes.min(MAX_READ_BYTES)
    };
    let mut file = fs::File::open(path).map_err(SourceError::Io)?;
    let length = file.metadata().map_err(SourceError::Io)?.len();
    let offset = offset.min(length);
    file.seek(SeekFrom::Start(offset))
        .map_err(SourceError::Io)?;
    let mut buffer = Vec::with_capacity(budget.min((length - offset) as usize));
    (&mut file)
        .take(budget as u64)
        .read_to_end(&mut buffer)
        .map_err(SourceError::Io)?;
    let read = buffer.len();
    let at_end = offset + (read as u64) >= length;
    let mut truncated = false;
    let end = match buffer.iter().rposition(|byte| *byte == b'\n') {
        // 页尾对齐到最后一个换行；文件末尾的半截行留给下一次。
        Some(newline) => newline + 1,
        // 整页只是尾部的半截行：不消费，等它写完。
        None if at_end => 0,
        // 一行比页还长：截在字符边界，继续读能拼回原文。
        None => {
            truncated = true;
            floor_char_boundary(&buffer, read)
        }
    };
    let next = offset + end as u64;
    Ok(ContentChunk {
        format: AgentActivityContentFormat::Jsonl,
        text: String::from_utf8_lossy(&buffer[..end]).into_owned(),
        next_cursor: Some(next.to_string()),
        eof: at_end,
        truncated,
    })
}

/// `index` 若落在一个多字节字符中间，退到该字符的起点；否则原样返回。
fn floor_char_boundary(bytes: &[u8], index: usize) -> usize {
    let index = index.min(bytes.len());
    let start = index.saturating_sub(4);
    let Some(lead_position) = (start..index)
        .rev()
        .find(|&i| (bytes[i] & 0b1100_0000) != 0b1000_0000)
    else {
        // 最近 4 字节全是续字节：畸形输入，交给 `from_utf8_lossy` 兜底。
        return start;
    };
    let lead = bytes[lead_position];
    let width = if lead < 0x80 {
        1
    } else if lead >= 0xF0 {
        4
    } else if lead >= 0xE0 {
        3
    } else if lead >= 0xC0 {
        2
    } else {
        1
    };
    if lead_position + width <= index {
        index
    } else {
        lead_position
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_resume::AgentSessionRef;

    const ROOT: &str = "01a0c6d7-6500-7000-8000-0000000000a1";
    const CHILD_A: &str = "01a0c6d8-4f60-7000-8000-0000000000a2";
    const CHILD_B: &str = "01a0c6d9-39c0-7000-8000-0000000000a3";
    const CHILD_C: &str = "01a0c6da-2420-7000-8000-0000000000a4";
    const GRANDCHILD: &str = "01a0c6db-0e80-7000-8000-0000000000a5";
    const GREAT_GRANDCHILD: &str = "01a0c6db-f8e0-7000-8000-0000000000a6";
    const GUARDIAN: &str = "01a0c6dc-e340-7000-8000-0000000000a7";
    const UNKNOWN_SOURCE: &str = "01a0c6df-a260-7000-8000-0000000000b1";
    const ERRORED: &str = "01a0c6e0-8cc0-7000-8000-0000000000b2";
    const IDLE: &str = "01a0c6e1-7720-7000-8000-0000000000b3";
    const COMPRESSED: &str = "01a0c6e2-6180-7000-8000-0000000000b4";
    const LEGACY: &str = "legacy-child-0001";

    fn fixture_home() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("agent-activity")
            .join("codex")
            .join("tree")
    }

    fn context<'a>(home: &'a Path, session: Option<&'a AgentSessionRef>) -> SourceContext<'a> {
        SourceContext {
            agent: "codex",
            session,
            cwd: None,
            home,
            now_ms: 1_790_043_600_000,
            agent_config_dir: None,
            latest_hint: None,
        }
    }

    fn discover(home: &Path, session: Option<&AgentSessionRef>) -> Vec<AgentActivityNode> {
        Codex
            .discover(&context(home, session))
            .expect("夹具目录可读")
    }

    fn node<'a>(nodes: &'a [AgentActivityNode], id: &str) -> &'a AgentActivityNode {
        nodes
            .iter()
            .find(|node| node.id == id)
            .unwrap_or_else(|| panic!("树里应有 {id}"))
    }

    #[test]
    fn discover_builds_the_tree_from_rollout_first_lines() {
        let home = fixture_home();
        let session = AgentSessionRef::id(ROOT).expect("合法 id");
        let nodes = discover(&home, Some(&session));

        let ids: Vec<&str> = nodes.iter().map(|node| node.id.as_str()).collect();
        // 第一层按创建时间，缺时间的排最后；再逐层 BFS。
        assert_eq!(
            ids,
            [
                CHILD_A,
                CHILD_B,
                CHILD_C,
                GUARDIAN,
                UNKNOWN_SOURCE,
                ERRORED,
                IDLE,
                LEGACY,
                GRANDCHILD,
                GREAT_GRANDCHILD,
            ]
        );
        // 顶层线程、坏文件、非 meta 首行、孤儿、压缩变体、早于根的文件都不在树里。
        assert!(!ids.iter().any(|id| id.ends_with("a8")
            || id.ends_with("a9")
            || id.ends_with("b4")
            || id.ends_with("b5")
            || id.ends_with("b6")
            || id.ends_with("b7")));

        let child_a = node(&nodes, CHILD_A);
        assert_eq!(child_a.kind, AgentActivityKind::Subagent);
        assert_eq!(child_a.label, "Ada");
        assert_eq!(child_a.agent_type.as_deref(), Some("worker"));
        assert_eq!(child_a.parent_id, None);
        assert_eq!(child_a.status, AgentActivityStatus::Done);
        assert_eq!(child_a.started_at_ms, Some(1_790_042_460_000));
        assert_eq!(child_a.ended_at_ms, Some(1_790_042_490_000));
        assert_eq!(child_a.summary.as_deref(), Some("/root/fix_login"));
        assert_eq!(child_a.content_ref.as_deref(), Some(CHILD_A));

        let child_b = node(&nodes, CHILD_B);
        assert_eq!(child_b.label, "explorer", "没有昵称时退到角色");
        assert_eq!(child_b.status, AgentActivityStatus::Running);
        assert_eq!(child_b.ended_at_ms, None);

        let child_c = node(&nodes, CHILD_C);
        assert_eq!(
            child_c.label, "audit_deb",
            "没有昵称与角色时退到 agent_path 末段"
        );
        assert_eq!(child_c.agent_type, None);
        assert_eq!(child_c.status, AgentActivityStatus::Failed);
        assert_eq!(child_c.summary.as_deref(), Some("interrupted"));
        assert_eq!(child_c.ended_at_ms, Some(1_790_042_600_000));

        let grandchild = node(&nodes, GRANDCHILD);
        assert_eq!(grandchild.parent_id.as_deref(), Some(CHILD_B));
        assert_eq!(grandchild.label, "Bo");
        let great = node(&nodes, GREAT_GRANDCHILD);
        assert_eq!(great.parent_id.as_deref(), Some(GRANDCHILD));
        assert_eq!(great.label, GREAT_GRANDCHILD, "全空时显示线程 id");
        assert_eq!(great.status, AgentActivityStatus::Running);

        let guardian = node(&nodes, GUARDIAN);
        assert_eq!(guardian.kind, AgentActivityKind::Background);
        assert_eq!(guardian.label, "guardian");
        assert_eq!(guardian.agent_type.as_deref(), Some("guardian"));
        assert_eq!(guardian.status, AgentActivityStatus::Done);

        let unknown = node(&nodes, UNKNOWN_SOURCE);
        assert_eq!(unknown.kind, AgentActivityKind::Unknown);
        assert_eq!(unknown.label, UNKNOWN_SOURCE);
        assert_eq!(
            unknown.status,
            AgentActivityStatus::Done,
            "未知事件类型与坏 JSON 行都被跳过"
        );

        let errored = node(&nodes, ERRORED);
        assert_eq!(errored.status, AgentActivityStatus::Failed);
        assert_eq!(errored.summary.as_deref(), Some("model refused (fake)"));
        assert_eq!(errored.ended_at_ms, Some(1_790_043_030_000));

        let idle = node(&nodes, IDLE);
        assert_eq!(idle.status, AgentActivityStatus::Pending);

        let legacy = node(&nodes, LEGACY);
        assert_eq!(legacy.kind, AgentActivityKind::Unknown);
        assert_eq!(legacy.label, LEGACY);
        assert_eq!(legacy.started_at_ms, None);
        assert_eq!(legacy.agent_type, None);
        assert_eq!(legacy.summary, None);
        assert_eq!(legacy.status, AgentActivityStatus::Pending);
    }

    #[test]
    fn discover_degrades_without_session_or_sessions_dir() {
        let home = fixture_home();
        assert!(matches!(
            Codex.discover(&context(&home, None)),
            Err(SourceError::Unsupported)
        ));

        let missing = home.join("no-such-home");
        let session = AgentSessionRef::id(ROOT).expect("合法 id");
        assert!(discover(&missing, Some(&session)).is_empty());

        let stranger =
            AgentSessionRef::id("01a0c6fe-0000-7000-8000-00000000000e").expect("合法 id");
        assert!(
            discover(&home, Some(&stranger)).is_empty(),
            "无子线程的根给空树"
        );

        // 孤儿夹具 b7 的父线程作根：树里只有它，顶层线程与其它人的子线程都不混进来。
        let orphan_parent =
            AgentSessionRef::id("01a0c6ff-0000-7000-8000-0000000000ff").expect("合法 id");
        let nodes = discover(&home, Some(&orphan_parent));
        assert_eq!(nodes.len(), 1);
        assert!(nodes[0].id.ends_with("b7"));
        assert_eq!(nodes[0].status, AgentActivityStatus::Running);
    }

    #[test]
    fn discover_accepts_a_rollout_path_as_the_session_ref() {
        let home = fixture_home();
        let path = home
            .join(".codex/sessions/2026/09/22")
            .join(format!("rollout-2026-09-22T10-00-00-{ROOT}.jsonl"));
        let session = AgentSessionRef::path(path.to_string_lossy().into_owned()).expect("合法路径");
        let nodes = discover(&home, Some(&session));
        assert_eq!(nodes.len(), 10);
    }

    /// 每个测试自己的临时 home（仓库不带 tempfile 依赖）；用完删掉。
    fn unique_temp_home(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "herdr-codex-activity-{name}-{}-{nanos}",
            std::process::id()
        ))
    }

    /// 把夹具树整棵复制到 `to`。
    fn copy_tree(from: &Path, to: &Path) {
        fs::create_dir_all(to).expect("建目标目录");
        for entry in fs::read_dir(from).expect("夹具目录可读") {
            let entry = entry.expect("夹具目录项可读");
            let target = to.join(entry.file_name());
            if entry.file_type().expect("夹具目录项类型可读").is_dir() {
                copy_tree(&entry.path(), &target);
            } else {
                fs::copy(entry.path(), &target).expect("复制夹具文件");
            }
        }
    }

    /// `CODEX_HOME` 把配置目录挪出 home 时：runtime 给的配置目录优先，home 下没有
    /// `.codex` 也能找到并读到子线程；给了配置目录就只认它，不回退 home。
    #[test]
    fn a_relocated_codex_home_is_followed_instead_of_home() {
        let temp = unique_temp_home("codex-home");
        let codex_home = temp.join("opt").join("codex-home");
        copy_tree(&fixture_home().join(".codex"), &codex_home);
        let empty_home = temp.join("home");
        fs::create_dir_all(&empty_home).expect("建空 home");
        let session = AgentSessionRef::id(ROOT).expect("合法 id");

        let relocated = SourceContext {
            agent_config_dir: Some(&codex_home),
            ..context(&empty_home, Some(&session))
        };
        let nodes = Codex.discover(&relocated).expect("配置目录下的会话库可读");
        assert_eq!(nodes.len(), 10);
        assert_eq!(
            nodes,
            discover(&fixture_home(), Some(&session)),
            "与默认布局同一棵树"
        );
        let expected = fs::read_to_string(
            fixture_home()
                .join(".codex/sessions/2026/09/22")
                .join(format!("rollout-2026-09-22T10-01-00-{CHILD_A}.jsonl")),
        )
        .expect("夹具可读");
        let page = Codex
            .read(&relocated, CHILD_A, None, MAX_READ_BYTES)
            .expect("配置目录下的 rollout 可读");
        assert_eq!(page.text, expected);

        // 不给配置目录：回退 `<home>/.codex`，空 home 下是空树，读取稍后重试。
        assert!(discover(&empty_home, Some(&session)).is_empty());
        assert!(matches!(
            Codex.read(&context(&empty_home, Some(&session)), CHILD_A, None, 300),
            Err(SourceError::Unavailable)
        ));

        // 给了配置目录就只认它：home 下明明有数据也不回头找。
        let home = fixture_home();
        let nowhere = temp.join("nowhere");
        let pinned = SourceContext {
            agent_config_dir: Some(&nowhere),
            ..context(&home, Some(&session))
        };
        assert!(Codex.discover(&pinned).expect("缺目录不报错").is_empty());

        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn read_pages_through_a_child_rollout_by_byte_cursor() {
        let home = fixture_home();
        let session = AgentSessionRef::id(ROOT).expect("合法 id");
        let cx = context(&home, Some(&session));
        let path = home
            .join(".codex/sessions/2026/09/22")
            .join(format!("rollout-2026-09-22T10-01-00-{CHILD_A}.jsonl"));
        let expected = fs::read_to_string(&path).expect("夹具可读");

        let whole = Codex
            .read(&cx, CHILD_A, None, MAX_READ_BYTES)
            .expect("整页可读");
        assert_eq!(whole.format, AgentActivityContentFormat::Jsonl);
        assert_eq!(whole.text, expected);
        assert!(whole.eof);
        assert!(!whole.truncated);
        assert_eq!(
            whole.next_cursor.as_deref(),
            Some(expected.len().to_string().as_str())
        );

        // 按 800 字节分页（夹具最长行 747 字节）：页尾对齐换行，拼起来等于原文。
        let mut cursor: Option<String> = None;
        let mut pages = 0;
        let mut joined = String::new();
        loop {
            let chunk = Codex
                .read(&cx, CHILD_A, cursor.as_deref(), 800)
                .expect("分页可读");
            assert!(!chunk.truncated);
            assert!(chunk.text.is_empty() || chunk.text.ends_with('\n'));
            joined.push_str(&chunk.text);
            pages += 1;
            if chunk.eof {
                break;
            }
            cursor = chunk.next_cursor;
            assert!(pages < 1000, "分页不收敛");
        }
        assert!(pages > 2);
        assert_eq!(joined, expected);

        // 页比一行还小：截断标记为真，继续读仍能拼出原文。
        let mut cursor: Option<String> = None;
        let mut joined = String::new();
        let mut truncated_pages = 0;
        loop {
            let chunk = Codex
                .read(&cx, CHILD_A, cursor.as_deref(), 16)
                .expect("小页可读");
            assert!(chunk.text.len() <= 16);
            truncated_pages += usize::from(chunk.truncated);
            joined.push_str(&chunk.text);
            if chunk.eof {
                break;
            }
            cursor = chunk.next_cursor;
        }
        assert!(truncated_pages > 0);
        assert_eq!(joined, expected);

        // 游标越过文件末尾（文件被截断或跟随时）：空页、eof、游标夹回长度。
        let beyond = Codex
            .read(&cx, CHILD_A, Some("999999999"), 300)
            .expect("越界游标不报错");
        assert!(beyond.text.is_empty());
        assert!(beyond.eof);
        assert_eq!(
            beyond.next_cursor.as_deref(),
            Some(expected.len().to_string().as_str())
        );

        // max_bytes = 0 用默认页。
        let default_page = Codex.read(&cx, CHILD_A, None, 0).expect("默认页可读");
        assert_eq!(default_page.text, expected);
    }

    #[test]
    fn read_rejects_bad_cursors_unknown_nodes_and_compressed_rollouts() {
        let home = fixture_home();
        let session = AgentSessionRef::id(ROOT).expect("合法 id");
        let cx = context(&home, Some(&session));
        // 无会话引用：与 discover 一样报不支持。
        assert!(matches!(
            Codex.read(&context(&home, None), CHILD_A, None, 300),
            Err(SourceError::Unsupported)
        ));
        assert!(matches!(
            Codex.read(&cx, CHILD_A, Some("not-a-number"), 300),
            Err(SourceError::Malformed(_))
        ));
        assert!(matches!(
            Codex.read(&cx, "../etc/passwd", None, 300),
            Err(SourceError::Malformed(_))
        ));
        // 线程 id 合法但 rollout 还没落盘：稍后重试。
        assert!(matches!(
            Codex.read(&cx, "01a0c6ff-0000-7000-8000-0000000000ff", None, 300),
            Err(SourceError::Unavailable)
        ));
        assert!(matches!(
            Codex.read(&cx, COMPRESSED, None, 300),
            Err(SourceError::Unsupported)
        ));
        // 会话库目录还没生成：同样是稍后重试。
        let missing = home.join("no-such-home");
        assert!(matches!(
            Codex.read(&context(&missing, Some(&session)), CHILD_A, None, 300),
            Err(SourceError::Unavailable)
        ));
    }

    #[test]
    fn read_leaves_a_trailing_partial_line_for_the_next_follow_up() {
        let home = unique_temp_home("partial-line");
        let day = home.join(".codex/sessions/2026/09/22");
        fs::create_dir_all(&day).expect("临时目录可建");
        let path = day.join(format!("rollout-2026-09-22T10-01-00-{CHILD_A}.jsonl"));
        let head =
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"x\"}}\n{\"type\":\"turn_context\"}\n";
        let partial = "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_st";
        fs::write(&path, format!("{head}{partial}")).expect("临时文件可写");
        let session = AgentSessionRef::id(ROOT).expect("合法 id");
        let cx = context(&home, Some(&session));

        // 半截行不消费：游标停在它的行首，eof 表示暂时读完。
        let first = Codex.read(&cx, CHILD_A, None, 4096).expect("可读");
        assert_eq!(first.text, head);
        assert!(first.eof);
        assert!(!first.truncated);
        assert_eq!(
            first.next_cursor.as_deref(),
            Some(head.len().to_string().as_str())
        );

        // 同一游标再读：还是空页、同一游标。
        let waiting = Codex
            .read(&cx, CHILD_A, first.next_cursor.as_deref(), 4096)
            .expect("可读");
        assert!(waiting.text.is_empty());
        assert!(waiting.eof);
        assert!(!waiting.truncated);
        assert_eq!(waiting.next_cursor, first.next_cursor);

        // 页比半截行还短：读不到文件末尾就分不清「半截」与「超长」，按超长行截断
        // 消费；最后一片到了末尾仍会等换行。
        let tiny = Codex
            .read(&cx, CHILD_A, first.next_cursor.as_deref(), 8)
            .expect("可读");
        assert_eq!(tiny.text, &partial[..8]);
        assert!(tiny.truncated);
        assert!(!tiny.eof);
        assert_eq!(
            tiny.next_cursor.as_deref(),
            Some((head.len() + 8).to_string().as_str())
        );

        // 这一行写完并追加新行后，从同一游标续读拿到完整内容。
        let rest = "arted\"}}\n{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\"}}\n";
        {
            use std::io::Write;
            let mut file = fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("可追加");
            file.write_all(rest.as_bytes()).expect("可写");
        }
        let followed = Codex
            .read(&cx, CHILD_A, first.next_cursor.as_deref(), 4096)
            .expect("可读");
        assert_eq!(followed.text, format!("{partial}{rest}"));
        assert!(followed.eof);
        assert_eq!(
            followed.next_cursor.as_deref(),
            Some(
                (head.len() + partial.len() + rest.len())
                    .to_string()
                    .as_str()
            )
        );

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn lifecycle_tail_window_treats_marker_free_tails_as_running() {
        let day = fixture_home().join(".codex/sessions/2026/09/22");
        let child_a = day.join(format!("rollout-2026-09-22T10-01-00-{CHILD_A}.jsonl"));
        let child_b = day.join(format!("rollout-2026-09-22T10-02-00-{CHILD_B}.jsonl"));
        let idle = day.join(format!("rollout-2026-09-22T10-11-00-{IDLE}.jsonl"));

        assert_eq!(
            lifecycle_from_tail(&child_a, STATUS_TAIL_BYTES).expect("可读"),
            Lifecycle::Completed {
                ended_at_ms: Some(1_790_042_490_000),
                error: None,
            }
        );
        // 窗口小到装不下最后一个标记，而文件比窗口大：视为回合进行中。
        assert_eq!(
            lifecycle_from_tail(&child_b, 64).expect("可读"),
            Lifecycle::Running
        );
        // 整个文件都在窗口里且没有标记：还没开始。
        assert_eq!(
            lifecycle_from_tail(&idle, STATUS_TAIL_BYTES).expect("可读"),
            Lifecycle::Idle
        );
        assert!(lifecycle_from_tail(&day.join("missing.jsonl"), 64).is_err());
    }

    #[test]
    fn rollout_file_names_and_uuid_v7_timestamps_parse() {
        assert_eq!(
            rollout_file_id(&format!("rollout-2026-09-22T10-01-00-{CHILD_A}.jsonl")),
            Some((CHILD_A, false))
        );
        assert_eq!(
            rollout_file_id(&format!(
                "rollout-2026-09-22T10-12-00-{COMPRESSED}.jsonl.zst"
            )),
            Some((COMPRESSED, true))
        );
        assert_eq!(
            rollout_file_id("rollout-2026-09-22T10-14-00-legacy-child-0001.jsonl"),
            Some(("legacy-child-0001", false))
        );
        assert_eq!(rollout_file_id("rollout-2026-09-22T10-14-00-.jsonl"), None);
        assert_eq!(
            rollout_file_id("rollout-2026-09-22T10-14-00-a/b.jsonl"),
            None
        );
        assert_eq!(rollout_file_id("notes.jsonl"), None);
        assert_eq!(rollout_file_id("rollout-short.jsonl"), None);

        assert_eq!(uuid_v7_millis(ROOT), Some(1_790_042_400_000));
        assert_eq!(uuid_v7_millis(LEGACY), None);
        assert_eq!(
            uuid_v7_millis("01a0c6d7-6500-4000-8000-0000000000a1"),
            None,
            "只认版本 7"
        );
        assert_eq!(utc_day(1_790_042_400_000), Some((2026, 9, 22)));
        assert_eq!(utc_day(0), Some((1970, 1, 1)));
    }

    #[test]
    fn floor_char_boundary_keeps_multibyte_chars_whole() {
        let text = "a中b";
        let bytes = text.as_bytes();
        assert_eq!(floor_char_boundary(bytes, 1), 1);
        assert_eq!(floor_char_boundary(bytes, 2), 1, "落在「中」中间退到起点");
        assert_eq!(floor_char_boundary(bytes, 3), 1);
        assert_eq!(floor_char_boundary(bytes, 4), 4);
        assert_eq!(floor_char_boundary(bytes, 5), 5);
        assert_eq!(floor_char_boundary(bytes, 99), 5);
        assert_eq!(floor_char_boundary(&[0x80, 0x80, 0x80, 0x80, 0x80], 5), 1);
    }
}
