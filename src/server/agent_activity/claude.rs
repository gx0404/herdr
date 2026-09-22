//! Claude Code 的活动来源适配器：子 agent 转录树、workflow journal 增强与主
//! 转录尾部的待办条目。
//!
//! # 本机取证（claude 2.1.278，只读核对目录结构与键名，未读取任何对话正文）
//!
//! 转录布局（`<home>/.claude/projects/<项目 slug>/` 下）：
//!
//! - `<session-uuid>.jsonl` 是主转录；同名目录 `<session-uuid>/` 放派生数据。
//! - `<session-uuid>/subagents/agent-<agentId>.jsonl` 是官方文档写的扁平布局；
//!   本机实测另有一层 `<session-uuid>/subagents/workflows/wf_*/agent-*.jsonl`
//!   （227 个实测样本里两种布局同时存在）→ 一律递归扫。
//! - 每个 `agent-<agentId>.jsonl` 旁有实测存在、但计划未登记的同名边车
//!   `agent-<agentId>.meta.json`，键与取值集合（227 个样本全覆盖）：
//!   `agentType`（`Explore` / `Plan` / `general-purpose` / `workflow-subagent`）、
//!   `description`（自由文本，即派生时的标题）、`model`（`fable` / `haiku` /
//!   `opus` / `sonnet`）、`requestShape`（`background` / `foreground`）、
//!   `requestNonInteractive`(bool)、`spawnDepth`(int)、`spawnedWithWorktree`(bool)、
//!   `toolUseId`(str)、`workflowPhase`(str)、`worktreePath`(str)。
//!   它是本适配器取 label / agent_type 的首选，比解析转录首行便宜得多。
//! - `<session-uuid>/subagents/workflows/wf_*/journal.jsonl`（未文档化内部格式）
//!   实测 16 份、392 行，`type` 只出现 `launched` / `started` / `result` /
//!   `failed`；`started` 带 `agentId` / `key` / `label` / `phase`，`result` 带
//!   `agentId` / `key` / `result`，`failed` 带 `agentId` / `key`，`launched` 只有
//!   `type`。`phase` 的实测取值集合：`Base` / `Critic` / `Design` / `Final` /
//!   `Fix` / `Implement` / `Inventory` / `Map` / `Research` / `Review` / `Verify`。
//!   `result` 的值是各 workflow 自定义的对象或字符串，没有稳定形状 → 只当作
//!   「该 agent 已结束」的标记，不解析内容。整份 journal 解析失败静默降级。
//!
//! 子 agent 转录行（实测 40 个文件、15 053 行，0 条坏行）：顶层键 `agentId`、
//! `type`、`timestamp`、`uuid`、`parentUuid`、`isSidechain`、`sessionId`、`cwd`、
//! `gitBranch`、`slug`、`version`、`message`、`attributionAgent`、`promptId`、
//! `toolUseResult` 等；`type` 只出现 `user` / `attachment` / `assistant`，**没有**
//! `result` 之类的结束记录 → 结束与否只能靠 journal 或时间窗判定。
//! `attributionAgent` 是子 agent 的**类型**字符串（实测 `Explore` / `Plan` /
//! `general-purpose`），不是 id；token 用量在 `message.usage`，键为
//! `input_tokens` / `output_tokens` / `cache_creation_input_tokens` /
//! `cache_read_input_tokens` 等。`timestamp` 是 RFC3339 毫秒 UTC。
//! `message.content` 既可能是字符串，也可能是块数组（块 `type` 实测 `text` /
//! `thinking` / `tool_use` / `tool_result`）。
//!
//! 主转录里派生子 agent 的工具名实测是 `Agent`（输入键 `description` /
//! `subagent_type` / `model` / `prompt`），不是 `Task`；workflow 派生的子 agent 在
//! 父转录里只有一条 `Workflow` 工具调用 → 反查父转录得不到完整树，目录扫描是
//! 主力。
//!
//! 待办：**本机 25 份主转录里没有任何 `TodoWrite` / `TaskCreate` 调用**，形状取自
//! 2.1.278 二进制内嵌的输入 schema（只读字符串检索）：`TodoWrite` 的
//! `input.todos[]` 为 `{content, status, activeForm}`，`status` ∈ `pending` /
//! `in_progress` / `completed`，本适配器取尾部窗口里最后一次写入；夹具按该形状
//! 手写。另一套 Tasks 系统（`TaskCreate` 输入 `{subject, description,
//! activeForm}`，任务 id 只出现在工具结果文本 `Task #<id> created successfully`
//! 里，状态靠后续 `TaskUpdate {taskId, status}`）**本版不重建**：它的持久化目录
//! `<config>/tasks/<listId>/<id>.json` 的 listId 在本机 4 个样本里都对不上任何
//! 会话 id，没有可靠的会话→清单映射；`TaskCreated` / `TaskCompleted` 钩子只负责
//! 触发刷新信号。

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{ActivitySource, ContentChunk, SourceContext, SourceError};
use crate::agent_resume::AgentSessionRefKind;
use crate::api::schema::{
    AgentActivityContentFormat, AgentActivityKind, AgentActivityNode, AgentActivityStatus,
};

/// 一次 discover 产出的节点总上限；超出后按路径序丢弃其余子 agent。
const MAX_NODES: usize = 256;
/// 单个子 agent 转录的扫描字节上限。
const MAX_TRANSCRIPT_BYTES: u64 = 4 * 1024 * 1024;
/// 一次 discover 扫描全部子 agent 转录的总字节预算。
const MAX_DISCOVER_BYTES: u64 = 8 * 1024 * 1024;
/// 单份 journal.jsonl 的读取上限。
const MAX_JOURNAL_BYTES: u64 = 1024 * 1024;
/// 主转录只扫尾部这么多字节找最后一次待办写入。
const MAX_MAIN_TAIL_BYTES: u64 = 256 * 1024;
/// 单个 meta 边车的读取上限。
const MAX_META_BYTES: u64 = 64 * 1024;
/// subagents 目录的递归深度上限。
const MAX_WALK_DEPTH: usize = 8;
/// label 与 summary 的字符上限。
const MAX_LABEL_CHARS: usize = 120;
/// 待办条目上限。
const MAX_TODOS: usize = 32;
/// 无 journal 记录时，最后一条转录早于这个窗口就不再算运行中。
const RUNNING_WINDOW_MS: u64 = 120_000;
/// read 的分页预算区间。
const MIN_READ_BYTES: usize = 1024;
const MAX_READ_BYTES: usize = 1024 * 1024;

/// 主转录里整份覆写待办清单的工具名（输入形状见模块文档）。
const TODO_TOOL_NAME: &str = "TodoWrite";

pub(super) struct Claude;

impl ActivitySource for Claude {
    fn id(&self) -> &'static str {
        "claude"
    }

    fn discover(&self, cx: &SourceContext<'_>) -> Result<Vec<AgentActivityNode>, SourceError> {
        match locate_session(cx) {
            Located::NoSession => Err(SourceError::Unsupported),
            Located::NotFound => Ok(Vec::new()),
            Located::Found(dir) => Ok(build_tree(&dir, cx.now_ms)),
        }
    }

    fn read(
        &self,
        cx: &SourceContext<'_>,
        node_id: &str,
        cursor: Option<&str>,
        max_bytes: usize,
    ) -> Result<ContentChunk, SourceError> {
        let session_dir = match locate_session(cx) {
            Located::NoSession => return Err(SourceError::Unsupported),
            Located::NotFound => return Err(SourceError::Unavailable),
            Located::Found(dir) => dir,
        };
        let Some(file) = find_subagent_file(&session_dir, node_id) else {
            // 分组节点与待办节点没有可读内容，未知 id 同样落这里。
            return Err(SourceError::Unavailable);
        };
        let offset = match cursor {
            Some(cursor) => cursor.parse::<u64>().map_err(|_| {
                SourceError::Malformed(format!("cursor is not an offset: {cursor}"))
            })?,
            None => 0,
        };
        read_transcript_page(&file, offset, max_bytes)
    }
}

// ---------------------------------------------------------------------------
// 会话定位
// ---------------------------------------------------------------------------

enum Located {
    /// 没有会话引用，本来源无从发现。
    NoSession,
    /// 有会话引用但磁盘上还没有对应目录（会话刚起、或转录被清理）。
    NotFound,
    Found(PathBuf),
}

/// 由会话引用定位 `<项目 slug>/<session-uuid>/` 目录。
///
/// `agent_resume::session_ref_from_report` 对 claude 只保留 `Id`（`transcript_path`
/// 被丢弃），所以 `Id` 分支要在 `<home>/.claude/projects/*/` 下按会话 id 找目录；
/// `Path` 分支兼容日后直接给转录路径的情况。
fn locate_session(cx: &SourceContext<'_>) -> Located {
    let Some(session) = cx.session else {
        return Located::NoSession;
    };
    let candidate = match session.kind {
        AgentSessionRefKind::Path => session_dir_from_transcript(Path::new(&session.value)),
        AgentSessionRefKind::Id => find_session_dir_by_id(cx.home, &session.value),
    };
    match candidate {
        Some(dir) if dir.is_dir() => Located::Found(dir),
        _ => Located::NotFound,
    }
}

/// `<...>/<session-uuid>.jsonl` → `<...>/<session-uuid>/`；已经是目录则原样返回。
fn session_dir_from_transcript(path: &Path) -> Option<PathBuf> {
    if path.is_dir() {
        return Some(path.to_path_buf());
    }
    let parent = path.parent()?;
    let stem = path.file_stem()?.to_str()?;
    if stem.is_empty() {
        return None;
    }
    Some(parent.join(stem))
}

/// 在 `<home>/.claude/projects/*/` 下找名为会话 id 的目录。项目 slug 由 cwd 推导的
/// 规则并不可靠（路径里的 `-` 与分隔符会混淆），直接逐个项目目录探测更稳。
fn find_session_dir_by_id(home: &Path, session_id: &str) -> Option<PathBuf> {
    if session_id.is_empty() || session_id.contains(['/', '\\']) || session_id.contains("..") {
        return None;
    }
    let projects = home.join(".claude").join("projects");
    let mut matches: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(&projects).ok()?.flatten() {
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let candidate = entry.path().join(session_id);
        if candidate.is_dir() {
            matches.push(candidate);
        }
    }
    // 同一会话 id 理论上只属于一个项目；真出现多份就取字典序最小的，保证确定性。
    matches.sort();
    matches.into_iter().next()
}

// ---------------------------------------------------------------------------
// 目录扫描
// ---------------------------------------------------------------------------

struct SubagentFile {
    agent_id: String,
    path: PathBuf,
    /// 相对 session 目录、以 `/` 分隔，用作 `content_ref`。
    rel: String,
    /// 位于 `subagents/workflows/<name>/` 下时的 workflow 目录名。
    workflow: Option<String>,
}

/// 递归收集 `subagents/**/agent-*.jsonl`，按相对路径排序保证结果确定。
fn collect_subagent_files(session_dir: &Path) -> Vec<SubagentFile> {
    let mut found = Vec::new();
    let mut stack = vec![(session_dir.join("subagents"), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_dir() {
                if depth < MAX_WALK_DEPTH {
                    stack.push((path, depth + 1));
                }
                continue;
            }
            if !kind.is_file() {
                continue;
            }
            let Some(agent_id) = agent_id_from_file_name(&path) else {
                continue;
            };
            let Some(rel) = relative_slashed(session_dir, &path) else {
                continue;
            };
            let workflow = workflow_of(&rel);
            found.push(SubagentFile {
                agent_id,
                path,
                rel,
                workflow,
            });
        }
    }
    found.sort_by(|left, right| left.rel.cmp(&right.rel));
    found
}

/// `agent-<id>.jsonl` → `<id>`。
fn agent_id_from_file_name(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let id = name.strip_prefix("agent-")?.strip_suffix(".jsonl")?;
    (!id.is_empty()).then(|| id.to_string())
}

fn relative_slashed(base: &Path, path: &Path) -> Option<String> {
    let mut parts = Vec::new();
    for component in path.strip_prefix(base).ok()?.components() {
        match component {
            std::path::Component::Normal(part) => parts.push(part.to_str()?),
            _ => return None,
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

fn workflow_of(rel: &str) -> Option<String> {
    let mut parts = rel.split('/');
    (parts.next()? == "subagents" && parts.next()? == "workflows")
        .then(|| parts.next().map(str::to_string))
        .flatten()
}

fn find_subagent_file(session_dir: &Path, agent_id: &str) -> Option<PathBuf> {
    collect_subagent_files(session_dir)
        .into_iter()
        .find(|file| file.agent_id == agent_id)
        .map(|file| file.path)
}

// ---------------------------------------------------------------------------
// meta 边车与 journal
// ---------------------------------------------------------------------------

#[derive(Default)]
struct SubagentMeta {
    description: Option<String>,
    agent_type: Option<String>,
    model: Option<String>,
    phase: Option<String>,
}

fn read_meta(transcript: &Path) -> SubagentMeta {
    let Some(name) = transcript.file_name().and_then(|name| name.to_str()) else {
        return SubagentMeta::default();
    };
    let Some(stem) = name.strip_suffix(".jsonl") else {
        return SubagentMeta::default();
    };
    let path = transcript.with_file_name(format!("{stem}.meta.json"));
    let Some(text) = read_capped(&path, MAX_META_BYTES) else {
        return SubagentMeta::default();
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return SubagentMeta::default();
    };
    SubagentMeta {
        description: trimmed_string(value.get("description")),
        agent_type: trimmed_string(value.get("agentType")),
        model: trimmed_string(value.get("model")),
        phase: trimmed_string(value.get("workflowPhase")),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum JournalState {
    Started,
    Done,
    Failed,
}

#[derive(Default)]
struct JournalRecord {
    label: Option<String>,
    phase: Option<String>,
    state: Option<JournalState>,
}

/// 解析一份 workflow journal。未文档化格式：未知 `type` 忽略，坏行跳过，整份
/// 读不动就当没有。
fn read_journal(path: &Path) -> BTreeMap<String, JournalRecord> {
    let mut records: BTreeMap<String, JournalRecord> = BTreeMap::new();
    let Some(text) = read_capped(path, MAX_JOURNAL_BYTES) else {
        return records;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(agent_id) = trimmed_string(value.get("agentId")) else {
            continue;
        };
        let record = records.entry(agent_id).or_default();
        match value.get("type").and_then(Value::as_str) {
            Some("started") => {
                record.state.get_or_insert(JournalState::Started);
                if let Some(label) = trimmed_string(value.get("label")) {
                    record.label = Some(label);
                }
                if let Some(phase) = trimmed_string(value.get("phase")) {
                    record.phase = Some(phase);
                }
            }
            // `result` 的载荷是各 workflow 自定义的，只取「已结束」这一个事实。
            Some("result") => record.state = Some(JournalState::Done),
            Some("failed") => record.state = Some(JournalState::Failed),
            _ => {}
        }
    }
    records
}

// ---------------------------------------------------------------------------
// 转录扫描
// ---------------------------------------------------------------------------

#[derive(Default)]
struct TranscriptScan {
    started_at_ms: Option<u64>,
    last_at_ms: Option<u64>,
    messages: u32,
    input_tokens: u64,
    output_tokens: u64,
    first_user_text: Option<String>,
    attribution: Option<String>,
}

/// 单趟流式扫描一个子 agent 转录，顺带把预算消耗记回 `budget`。坏行只跳过。
fn scan_transcript(path: &Path, budget: &mut u64) -> TranscriptScan {
    let mut scan = TranscriptScan::default();
    let Ok(file) = File::open(path) else {
        return scan;
    };
    let mut reader = BufReader::new(file);
    let mut consumed = 0u64;
    let mut raw = Vec::new();
    loop {
        if consumed >= MAX_TRANSCRIPT_BYTES || consumed >= *budget {
            break;
        }
        raw.clear();
        match reader.read_until(b'\n', &mut raw) {
            Ok(0) | Err(_) => break,
            Ok(read) => consumed = consumed.saturating_add(read as u64),
        }
        let line = String::from_utf8_lossy(&raw);
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if let Some(stamp) = rfc3339_ms(value.get("timestamp")) {
            scan.started_at_ms.get_or_insert(stamp);
            scan.last_at_ms = Some(stamp);
        }
        if scan.attribution.is_none() {
            scan.attribution = trimmed_string(value.get("attributionAgent"));
        }
        let Some(message) = value.get("message").filter(|value| value.is_object()) else {
            continue;
        };
        scan.messages = scan.messages.saturating_add(1);
        if let Some(usage) = message.get("usage") {
            scan.input_tokens = scan
                .input_tokens
                .saturating_add(non_negative(usage.get("input_tokens")));
            scan.output_tokens = scan
                .output_tokens
                .saturating_add(non_negative(usage.get("output_tokens")));
        }
        if scan.first_user_text.is_none()
            && message.get("role").and_then(Value::as_str) == Some("user")
        {
            scan.first_user_text = plain_text(message.get("content")).map(|text| clip(&text));
        }
    }
    *budget = budget.saturating_sub(consumed);
    scan
}

// ---------------------------------------------------------------------------
// 树构建
// ---------------------------------------------------------------------------

fn build_tree(session_dir: &Path, now_ms: u64) -> Vec<AgentActivityNode> {
    let files = collect_subagent_files(session_dir);

    // 每个 workflow 目录读一次 journal；解析失败留空映射，只是少了增强。
    let mut journals: BTreeMap<String, BTreeMap<String, JournalRecord>> = BTreeMap::new();
    for workflow in files.iter().filter_map(|file| file.workflow.as_ref()) {
        if !journals.contains_key(workflow) {
            let path = session_dir
                .join("subagents")
                .join("workflows")
                .join(workflow)
                .join("journal.jsonl");
            journals.insert(workflow.clone(), read_journal(&path));
        }
    }

    let mut nodes: Vec<AgentActivityNode> = Vec::new();
    // workflow / phase 分组节点在 `nodes` 里的下标，用来回填统计。
    let mut group_index: BTreeMap<String, usize> = BTreeMap::new();
    let mut group_tally: BTreeMap<String, Tally> = BTreeMap::new();
    let mut budget = MAX_DISCOVER_BYTES;

    for file in &files {
        if nodes.len() >= MAX_NODES {
            break;
        }
        let meta = read_meta(&file.path);
        let journal = file
            .workflow
            .as_ref()
            .and_then(|workflow| journals.get(workflow))
            .and_then(|records| records.get(&file.agent_id));
        let scan = scan_transcript(&file.path, &mut budget);

        let status = subagent_status(journal.and_then(|record| record.state), &scan, now_ms);
        let parent_id = match &file.workflow {
            None => None,
            Some(workflow) => {
                let workflow_id = format!("wf:{workflow}");
                ensure_group(
                    &mut nodes,
                    &mut group_index,
                    &mut group_tally,
                    &workflow_id,
                    workflow,
                    None,
                );
                let phase = meta
                    .phase
                    .clone()
                    .or_else(|| journal.and_then(|record| record.phase.clone()));
                match phase {
                    None => Some(workflow_id),
                    Some(phase) => {
                        let phase_id = format!("phase:{workflow}:{phase}");
                        ensure_group(
                            &mut nodes,
                            &mut group_index,
                            &mut group_tally,
                            &phase_id,
                            &phase,
                            Some(workflow_id),
                        );
                        Some(phase_id)
                    }
                }
            }
        };

        // 分组统计要一路记到祖先，面板才能在折叠时看出运行中数量。
        let mut ancestor = parent_id.clone();
        while let Some(id) = ancestor {
            let tally = group_tally.entry(id.clone()).or_default();
            tally.add(status);
            ancestor = group_index
                .get(&id)
                .and_then(|index| nodes.get(*index))
                .and_then(|node| node.parent_id.clone());
        }

        let agent_type = meta.agent_type.clone().or_else(|| scan.attribution.clone());
        nodes.push(AgentActivityNode {
            id: file.agent_id.clone(),
            kind: AgentActivityKind::Subagent,
            label: subagent_label(&file.agent_id, &meta, journal, &scan),
            status,
            parent_id,
            agent_type,
            content_ref: Some(file.rel.clone()),
            summary: subagent_summary(&meta, &scan),
            started_at_ms: scan.started_at_ms,
            ended_at_ms: (status != AgentActivityStatus::Running)
                .then_some(scan.last_at_ms)
                .flatten(),
        });
    }

    for (id, tally) in &group_tally {
        let Some(node) = group_index.get(id).and_then(|index| nodes.get_mut(*index)) else {
            continue;
        };
        node.status = tally.status();
        node.summary = Some(tally.summary());
    }

    nodes.extend(read_todos(session_dir));
    nodes
}

#[derive(Default)]
struct Tally {
    total: u32,
    running: u32,
    failed: u32,
    done: u32,
}

impl Tally {
    fn add(&mut self, status: AgentActivityStatus) {
        self.total = self.total.saturating_add(1);
        match status {
            AgentActivityStatus::Running | AgentActivityStatus::Pending => {
                self.running = self.running.saturating_add(1);
            }
            AgentActivityStatus::Failed => self.failed = self.failed.saturating_add(1),
            AgentActivityStatus::Done => self.done = self.done.saturating_add(1),
            _ => {}
        }
    }

    fn status(&self) -> AgentActivityStatus {
        if self.running > 0 {
            AgentActivityStatus::Running
        } else if self.failed > 0 {
            AgentActivityStatus::Failed
        } else if self.done > 0 && self.done == self.total {
            AgentActivityStatus::Done
        } else {
            AgentActivityStatus::Unknown
        }
    }

    fn summary(&self) -> String {
        format!("{}/{} running", self.running, self.total)
    }
}

fn ensure_group(
    nodes: &mut Vec<AgentActivityNode>,
    index: &mut BTreeMap<String, usize>,
    tally: &mut BTreeMap<String, Tally>,
    id: &str,
    label: &str,
    parent_id: Option<String>,
) {
    if index.contains_key(id) {
        return;
    }
    index.insert(id.to_string(), nodes.len());
    tally.entry(id.to_string()).or_default();
    nodes.push(AgentActivityNode {
        id: id.to_string(),
        kind: AgentActivityKind::Task,
        label: clip(label),
        status: AgentActivityStatus::Unknown,
        parent_id,
        ..AgentActivityNode::default()
    });
}

/// journal 有记录时以它为准；没有就靠「最后一条转录距今多久」判定，判不出来落
/// `Unknown` 而不是猜 `Done`——子 agent 转录里没有任何结束记录（本机实测）。
fn subagent_status(
    journal: Option<JournalState>,
    scan: &TranscriptScan,
    now_ms: u64,
) -> AgentActivityStatus {
    match journal {
        Some(JournalState::Done) => return AgentActivityStatus::Done,
        Some(JournalState::Failed) => return AgentActivityStatus::Failed,
        Some(JournalState::Started) => return AgentActivityStatus::Running,
        None => {}
    }
    match scan.last_at_ms {
        Some(last) if now_ms.saturating_sub(last) <= RUNNING_WINDOW_MS => {
            AgentActivityStatus::Running
        }
        Some(_) => AgentActivityStatus::Unknown,
        None => AgentActivityStatus::Pending,
    }
}

fn subagent_label(
    agent_id: &str,
    meta: &SubagentMeta,
    journal: Option<&JournalRecord>,
    scan: &TranscriptScan,
) -> String {
    meta.description
        .clone()
        .or_else(|| journal.and_then(|record| record.label.clone()))
        .or_else(|| scan.first_user_text.clone())
        .or_else(|| meta.agent_type.clone())
        .or_else(|| scan.attribution.clone())
        .map(|label| clip(&label))
        .unwrap_or_else(|| agent_id.to_string())
}

fn subagent_summary(meta: &SubagentMeta, scan: &TranscriptScan) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(model) = &meta.model {
        parts.push(clip(model));
    }
    if scan.messages > 0 {
        parts.push(format!("{} msg", scan.messages));
    }
    if scan.input_tokens > 0 {
        parts.push(format!("in {}", compact_count(scan.input_tokens)));
    }
    if scan.output_tokens > 0 {
        parts.push(format!("out {}", compact_count(scan.output_tokens)));
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

// ---------------------------------------------------------------------------
// 主转录尾部的待办条目
// ---------------------------------------------------------------------------

/// 扫主转录尾部，取**最后一次**待办写入作为当前清单。整份主转录可以有几十 MB，
/// 只读尾部窗口；窗口里没有写入就当没有待办。
fn read_todos(session_dir: &Path) -> Vec<AgentActivityNode> {
    let Some(path) = main_transcript_path(session_dir) else {
        return Vec::new();
    };
    let Some(text) = read_tail(&path, MAX_MAIN_TAIL_BYTES) else {
        return Vec::new();
    };
    let mut latest: Option<Vec<Value>> = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(blocks) = value
            .get("message")
            .and_then(|message| message.get("content"))
            .and_then(Value::as_array)
        else {
            continue;
        };
        for block in blocks {
            if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                continue;
            }
            if block.get("name").and_then(Value::as_str) != Some(TODO_TOOL_NAME) {
                continue;
            }
            if let Some(items) = block
                .get("input")
                .and_then(|input| input.get("todos"))
                .and_then(Value::as_array)
            {
                latest = Some(items.clone());
            }
        }
    }
    let Some(items) = latest else {
        return Vec::new();
    };
    items
        .iter()
        .take(MAX_TODOS)
        .enumerate()
        .map(|(index, item)| AgentActivityNode {
            id: format!("todo:{index}"),
            kind: AgentActivityKind::Todo,
            label: trimmed_string(item.get("content"))
                .or_else(|| trimmed_string(item.get("activeForm")))
                .map(|label| clip(&label))
                .unwrap_or_default(),
            status: todo_status(item.get("status").and_then(Value::as_str)),
            ..AgentActivityNode::default()
        })
        .collect()
}

fn todo_status(value: Option<&str>) -> AgentActivityStatus {
    match value {
        Some("completed") => AgentActivityStatus::Done,
        Some("in_progress") => AgentActivityStatus::Running,
        Some("pending") => AgentActivityStatus::Pending,
        _ => AgentActivityStatus::Unknown,
    }
}

fn main_transcript_path(session_dir: &Path) -> Option<PathBuf> {
    let parent = session_dir.parent()?;
    let name = session_dir.file_name()?.to_str()?;
    Some(parent.join(format!("{name}.jsonl")))
}

// ---------------------------------------------------------------------------
// 内容分页读取
// ---------------------------------------------------------------------------

fn read_transcript_page(
    path: &Path,
    offset: u64,
    max_bytes: usize,
) -> Result<ContentChunk, SourceError> {
    let file = File::open(path).map_err(SourceError::Io)?;
    let len = file.metadata().map_err(SourceError::Io)?.len();
    if offset >= len {
        return Ok(ContentChunk {
            format: AgentActivityContentFormat::Text,
            text: String::new(),
            next_cursor: None,
            eof: true,
            truncated: false,
        });
    }

    let budget = max_bytes.clamp(MIN_READ_BYTES, MAX_READ_BYTES);
    let mut reader = BufReader::new(file);
    reader
        .seek(SeekFrom::Start(offset))
        .map_err(SourceError::Io)?;

    let mut position = offset;
    let mut text = String::new();
    let mut truncated = false;
    let mut raw = Vec::new();
    while text.len() < budget {
        let line_start = position;
        raw.clear();
        match reader.read_until(b'\n', &mut raw) {
            Ok(0) => break,
            Ok(read) => position = position.saturating_add(read as u64),
            Err(error) => return Err(SourceError::Io(error)),
        }
        let Some(rendered) = render_transcript_line(&String::from_utf8_lossy(&raw)) else {
            continue;
        };
        let remaining = budget.saturating_sub(text.len());
        if rendered.len() + 1 > remaining {
            if !text.is_empty() {
                // 本页装不下就整行留给下一页：游标退回行首，内容不丢。
                position = line_start;
                break;
            }
            // 单行本身超过整页预算，只能截到字符边界并标记。
            text.push_str(clip_bytes(&rendered, budget.saturating_sub(2)));
            text.push_str(" …\n");
            truncated = true;
            break;
        }
        text.push_str(&rendered);
        text.push('\n');
    }

    let eof = position >= len;
    Ok(ContentChunk {
        format: AgentActivityContentFormat::Text,
        text,
        next_cursor: (!eof).then(|| position.to_string()),
        eof,
        truncated,
    })
}

/// 把一条转录行折叠成可读的一行：角色 + 文本摘要，工具调用只留名字。
fn render_transcript_line(line: &str) -> Option<String> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return Some("[unparsable line]".to_string());
    };
    let role = match value.get("type").and_then(Value::as_str) {
        Some("user") => "user",
        Some("assistant") => "assistant",
        // attachment 等派生行对阅读没有价值，直接跳过。
        _ => return None,
    };
    let content = value
        .get("message")
        .and_then(|message| message.get("content"));
    let body = render_content(content);
    Some(format!("[{role}] {body}"))
}

fn render_content(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => collapse(text),
        Some(Value::Array(blocks)) => {
            let rendered: Vec<String> = blocks.iter().map(render_block).collect();
            rendered
                .into_iter()
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join(" ")
        }
        _ => String::new(),
    }
}

fn render_block(block: &Value) -> String {
    match block.get("type").and_then(Value::as_str) {
        Some("text") => block
            .get("text")
            .and_then(Value::as_str)
            .map(collapse)
            .unwrap_or_default(),
        Some("thinking") => "[thinking]".to_string(),
        Some("tool_use") => {
            let name = block.get("name").and_then(Value::as_str).unwrap_or("tool");
            format!("[tool {}]", collapse(name))
        }
        Some("tool_result") => "[tool result]".to_string(),
        Some(other) => format!("[{}]", collapse(other)),
        None => String::new(),
    }
}

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------

fn read_capped(path: &Path, max_bytes: u64) -> Option<String> {
    use std::io::Read;
    let file = File::open(path).ok()?;
    let mut buffer = Vec::new();
    file.take(max_bytes).read_to_end(&mut buffer).ok()?;
    Some(String::from_utf8_lossy(&buffer).into_owned())
}

/// 读文件尾部窗口，并丢掉第一条可能被截断的行。
fn read_tail(path: &Path, max_bytes: u64) -> Option<String> {
    use std::io::Read;
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(max_bytes);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut buffer = Vec::new();
    file.take(max_bytes).read_to_end(&mut buffer).ok()?;
    let text = String::from_utf8_lossy(&buffer).into_owned();
    if start == 0 {
        return Some(text);
    }
    Some(match text.split_once('\n') {
        Some((_, rest)) => rest.to_string(),
        None => String::new(),
    })
}

fn trimmed_string(value: Option<&Value>) -> Option<String> {
    let text = value?.as_str()?.trim();
    (!text.is_empty()).then(|| text.to_string())
}

fn non_negative(value: Option<&Value>) -> u64 {
    value.and_then(Value::as_u64).unwrap_or(0)
}

fn rfc3339_ms(value: Option<&Value>) -> Option<u64> {
    let text = value?.as_str()?;
    let stamp =
        time::OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339).ok()?;
    u64::try_from(stamp.unix_timestamp_nanos() / 1_000_000).ok()
}

/// 取消息内容里的第一段纯文本，用作缺 label 时的兜底标题。
fn plain_text(content: Option<&Value>) -> Option<String> {
    match content? {
        Value::String(text) => {
            let text = collapse(text);
            (!text.is_empty()).then_some(text)
        }
        Value::Array(blocks) => blocks.iter().find_map(|block| {
            (block.get("type").and_then(Value::as_str) == Some("text"))
                .then(|| trimmed_string(block.get("text")).map(|text| collapse(&text)))
                .flatten()
                .filter(|text| !text.is_empty())
        }),
        _ => None,
    }
}

/// 折叠换行与连续空白，保证标题与内容行都是单行。
fn collapse(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    for ch in text.chars() {
        if ch.is_whitespace() || ch.is_control() {
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(ch);
    }
    out
}

fn clip(text: &str) -> String {
    let mut out: String = text.chars().take(MAX_LABEL_CHARS).collect();
    if out.chars().count() < text.chars().count() {
        out.push('…');
    }
    out
}

/// 按字节上限截断到字符边界。
fn clip_bytes(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn compact_count(value: u64) -> String {
    if value < 1_000 {
        value.to_string()
    } else if value < 1_000_000 {
        format!("{}.{}k", value / 1_000, (value % 1_000) / 100)
    } else {
        format!("{}.{}M", value / 1_000_000, (value % 1_000_000) / 100_000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_resume::AgentSessionRef;

    const SESSION_ID: &str = "5f000000-0000-4000-8000-000000000001";
    /// 夹具里最后一条时间戳是 2026-09-22T10:05:00.000Z。
    const FIXTURE_LAST_MS: u64 = 1_790_071_500_000;

    fn fixture_home() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/agent-activity/claude/home")
    }

    fn context<'a>(
        home: &'a Path,
        session: Option<&'a AgentSessionRef>,
        now_ms: u64,
    ) -> SourceContext<'a> {
        SourceContext {
            agent: "claude",
            session,
            cwd: None,
            home,
            now_ms,
        }
    }

    fn session_ref(id: &str) -> AgentSessionRef {
        AgentSessionRef::id(id).expect("夹具会话 id 合法")
    }

    fn discover(id: &str, now_ms: u64) -> Vec<AgentActivityNode> {
        let home = fixture_home();
        let session = session_ref(id);
        Claude
            .discover(&context(&home, Some(&session), now_ms))
            .expect("夹具会话可发现")
    }

    fn node<'a>(nodes: &'a [AgentActivityNode], id: &str) -> &'a AgentActivityNode {
        nodes
            .iter()
            .find(|node| node.id == id)
            .unwrap_or_else(|| panic!("缺少节点 {id}"))
    }

    #[test]
    fn without_a_session_reference_the_source_reports_unsupported() {
        let home = fixture_home();
        let cx = context(&home, None, FIXTURE_LAST_MS);
        assert!(matches!(
            Claude.discover(&cx),
            Err(SourceError::Unsupported)
        ));
        assert!(matches!(
            Claude.read(&cx, "a0000000000000001", None, 4096),
            Err(SourceError::Unsupported)
        ));
    }

    #[test]
    fn a_missing_session_directory_degrades_to_an_empty_tree() {
        let home = fixture_home();
        let session = session_ref("5f000000-0000-4000-8000-00000000dead");
        let cx = context(&home, Some(&session), FIXTURE_LAST_MS);
        assert!(Claude.discover(&cx).expect("缺目录不报错").is_empty());
        assert!(matches!(
            Claude.read(&cx, "a0000000000000001", None, 4096),
            Err(SourceError::Unavailable)
        ));
    }

    #[test]
    fn flat_and_nested_subagents_are_both_discovered() {
        let nodes = discover(SESSION_ID, FIXTURE_LAST_MS);
        let ids: Vec<&str> = nodes
            .iter()
            .filter(|node| node.kind == AgentActivityKind::Subagent)
            .map(|node| node.id.as_str())
            .collect();
        // 扁平布局、workflows/wf_* 与再深一层的嵌套都要收进来。
        assert_eq!(
            ids,
            [
                "a0000000000000001",
                "a0000000000000002",
                "a0000000000000003",
                "a0000000000000004",
                "a0000000000000005",
            ]
        );
        assert_eq!(
            node(&nodes, "a0000000000000005").parent_id.as_deref(),
            Some("wf:wf_demo-0002"),
        );
    }

    #[test]
    fn meta_sidecars_supply_label_agent_type_and_model() {
        let nodes = discover(SESSION_ID, FIXTURE_LAST_MS);
        let first = node(&nodes, "a0000000000000001");
        assert_eq!(first.label, "map the render hot path");
        assert_eq!(first.agent_type.as_deref(), Some("Explore"));
        let summary = first.summary.clone().expect("有用量摘要");
        assert!(summary.starts_with("sonnet · 2 msg"), "{summary}");
        assert!(summary.contains("in 1.2k"), "{summary}");
        assert!(summary.contains("out 340"), "{summary}");
    }

    #[test]
    fn a_missing_sidecar_falls_back_to_the_transcript() {
        let nodes = discover(SESSION_ID, FIXTURE_LAST_MS);
        let second = node(&nodes, "a0000000000000002");
        // 无 meta.json、无 journal → label 取首条用户文本，类型取 attributionAgent。
        assert_eq!(second.label, "check the config reference");
        assert_eq!(second.agent_type.as_deref(), Some("Plan"));
    }

    #[test]
    fn broken_json_lines_and_missing_fields_never_drop_the_tree() {
        let nodes = discover(SESSION_ID, FIXTURE_LAST_MS);
        // a0000000000000002 的转录里混有坏行、缺 timestamp 与未知 type 的行。
        let second = node(&nodes, "a0000000000000002");
        assert_eq!(second.started_at_ms, Some(1_790_071_200_000));
        assert!(second.content_ref.is_some());
        assert_eq!(nodes.iter().filter(|node| node.label.is_empty()).count(), 0);
    }

    #[test]
    fn journal_records_drive_status_label_and_phase_grouping() {
        let nodes = discover(SESSION_ID, FIXTURE_LAST_MS);
        // started 无 result → 运行中；failed → 失败。
        let third = node(&nodes, "a0000000000000003");
        assert_eq!(third.status, AgentActivityStatus::Running);
        assert_eq!(
            node(&nodes, "a0000000000000004").status,
            AgentActivityStatus::Failed
        );
        // a3 无 meta 边车：label 与 phase 都来自 journal。
        assert_eq!(third.label, "design:w2");
        assert_eq!(
            third.parent_id.as_deref(),
            Some("phase:wf_demo-0001:Design")
        );
        // a4 有 meta 边车：phase 来自 workflowPhase，label 来自 description。
        let fourth = node(&nodes, "a0000000000000004");
        assert_eq!(fourth.label, "critic:w2");
        assert_eq!(
            fourth.parent_id.as_deref(),
            Some("phase:wf_demo-0001:Critic")
        );

        let phase = node(&nodes, "phase:wf_demo-0001:Design");
        assert_eq!(phase.kind, AgentActivityKind::Task);
        assert_eq!(phase.parent_id.as_deref(), Some("wf:wf_demo-0001"));
        let workflow = node(&nodes, "wf:wf_demo-0001");
        assert_eq!(workflow.parent_id, None);
        assert_eq!(workflow.status, AgentActivityStatus::Running);
        assert_eq!(workflow.summary.as_deref(), Some("1/2 running"));
    }

    #[test]
    fn an_unknown_journal_type_is_ignored_without_losing_the_agent() {
        let nodes = discover(SESSION_ID, FIXTURE_LAST_MS);
        // journal 里 a0000000000000004 之后有一条 type="paused" 的未知记录。
        assert!(nodes.iter().any(|node| node.id == "a0000000000000004"));
    }

    #[test]
    fn without_a_journal_the_status_comes_from_the_freshness_window() {
        let fresh = discover(SESSION_ID, FIXTURE_LAST_MS);
        assert_eq!(
            node(&fresh, "a0000000000000001").status,
            AgentActivityStatus::Running
        );
        assert_eq!(node(&fresh, "a0000000000000001").ended_at_ms, None);

        let stale = discover(SESSION_ID, FIXTURE_LAST_MS + RUNNING_WINDOW_MS + 1);
        let node = node(&stale, "a0000000000000001");
        assert_eq!(node.status, AgentActivityStatus::Unknown);
        assert_eq!(node.ended_at_ms, Some(FIXTURE_LAST_MS));
    }

    #[test]
    fn todo_entries_come_from_the_tail_of_the_main_transcript() {
        let nodes = discover(SESSION_ID, FIXTURE_LAST_MS);
        let todos: Vec<(&str, AgentActivityStatus)> = nodes
            .iter()
            .filter(|node| node.kind == AgentActivityKind::Todo)
            .map(|node| (node.label.as_str(), node.status))
            .collect();
        // 只取最后一次写入；未知 status 落 Unknown。
        assert_eq!(
            todos,
            [
                ("rebuild the kb", AgentActivityStatus::Done),
                ("wire the adapter", AgentActivityStatus::Running),
                ("review the diff", AgentActivityStatus::Unknown),
            ]
        );
    }

    #[test]
    fn read_renders_roles_and_folds_tool_calls() {
        let home = fixture_home();
        let session = session_ref(SESSION_ID);
        let cx = context(&home, Some(&session), FIXTURE_LAST_MS);

        let page = Claude
            .read(&cx, "a0000000000000001", None, MIN_READ_BYTES)
            .expect("首页可读");
        assert_eq!(page.format, AgentActivityContentFormat::Text);
        assert!(page.text.starts_with("[user] map the render hot path"));
        assert!(page.text.contains("[assistant]"), "{}", page.text);
        assert!(page.text.contains("[thinking]"), "{}", page.text);
        assert!(page.text.contains("[tool Read]"), "{}", page.text);
        // attachment 行不进内容，工具入参被折叠掉。
        assert!(!page.text.contains("file_history"), "{}", page.text);
        assert!(!page.text.contains("/tmp/demo.rs"), "{}", page.text);
        assert!(page.eof);
        assert!(page.next_cursor.is_none());
        assert!(!page.truncated);

        let beyond = Claude
            .read(&cx, "a0000000000000001", Some("999999"), MIN_READ_BYTES)
            .expect("越界游标回空页");
        assert!(beyond.eof);
        assert!(beyond.text.is_empty());
    }

    #[test]
    fn read_paginates_a_long_transcript_without_repeating_lines() {
        let home = fixture_home();
        let session = session_ref(SESSION_ID);
        let cx = context(&home, Some(&session), FIXTURE_LAST_MS);

        let mut cursor: Option<String> = None;
        let mut text = String::new();
        let mut pages = 0;
        let mut truncated_pages = 0;
        loop {
            let page = Claude
                .read(&cx, "a0000000000000005", cursor.as_deref(), MIN_READ_BYTES)
                .expect("深层嵌套的转录可读");
            text.push_str(&page.text);
            pages += 1;
            truncated_pages += usize::from(page.truncated);
            assert!(pages < 64, "分页没有收敛");
            if page.eof {
                assert!(page.next_cursor.is_none());
                break;
            }
            cursor = page.next_cursor.clone();
            assert!(cursor.is_some(), "未到 eof 必须给出下一个游标");
        }
        assert!(pages >= 2, "夹具应当跨页，实际 {pages} 页");
        // 页边界上装不下的整行留给下一页，一条都不能丢。
        assert_eq!(text.matches("[user] step 01").count(), 1);
        assert_eq!(text.matches("step 24 processed").count(), 1);
        assert_eq!(text.matches("[tool Grep]").count(), 24);
        // 只有「单行本身超过整页预算」的末行会被截断。
        assert_eq!(truncated_pages, 1);
        assert!(text.contains("overlong tail"));
        assert!(text.contains('…'));
    }

    #[test]
    fn read_rejects_a_bad_cursor_and_an_unknown_node() {
        let home = fixture_home();
        let session = session_ref(SESSION_ID);
        let cx = context(&home, Some(&session), FIXTURE_LAST_MS);
        assert!(matches!(
            Claude.read(&cx, "a0000000000000001", Some("not-an-offset"), 4096),
            Err(SourceError::Malformed(_))
        ));
        assert!(matches!(
            Claude.read(&cx, "wf:wf_demo-0001", None, 4096),
            Err(SourceError::Unavailable)
        ));
    }

    #[test]
    fn helpers_stay_total_on_odd_input() {
        assert_eq!(collapse(" a\n\tb  c "), "a b c");
        assert_eq!(clip_bytes("中文", 4), "中");
        assert_eq!(clip_bytes("中文", 0), "");
        assert_eq!(compact_count(999), "999");
        assert_eq!(compact_count(1_234), "1.2k");
        assert_eq!(compact_count(12_345_678), "12.3M");
        assert_eq!(
            workflow_of("subagents/workflows/wf_x/agent-a.jsonl").as_deref(),
            Some("wf_x")
        );
        assert_eq!(workflow_of("subagents/agent-a.jsonl"), None);
        assert_eq!(
            agent_id_from_file_name(Path::new("/tmp/agent-a1.jsonl")).as_deref(),
            Some("a1")
        );
        assert_eq!(
            agent_id_from_file_name(Path::new("/tmp/journal.jsonl")),
            None
        );
        assert_eq!(
            render_transcript_line("{not json"),
            Some("[unparsable line]".into())
        );
        assert_eq!(render_transcript_line("   "), None);
    }
}
