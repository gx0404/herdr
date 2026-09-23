//! Claude Code 的活动来源适配器：子 agent 转录树、workflow journal 增强与主
//! 转录尾部的待办条目。
//!
//! # 本机取证（claude 2.1.278，只读核对目录结构与键名，未读取任何对话正文）
//!
//! 转录布局（`<config>/projects/<项目 slug>/` 下；`<config>` 是 Claude Code 的配置
//! 目录，跟随 `CLAUDE_CONFIG_DIR`，缺省 `<home>/.claude`）：
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
//! - `<session-uuid>/workflows/wf_<id>.json`（同为未文档化内部格式，实测 236 份，
//!   与 `subagents/workflows/wf_<id>/` 按目录名一一对应，但只在 workflow 结束后
//!   才出现：一个会话里 12 个 wf 目录只有 9 份）：顶层键 `workflowName`（人类
//!   可读名）、`status`（实测只有 `completed` / `killed`）、`startTime`(ms)、
//!   `durationMs`、`agentCount`、`totalTokens`、`totalToolCalls`、`summary`、
//!   `phases`、`logs`、`script` 等。本适配器只取名字、状态、起止时间与 token
//!   总数给分组节点；workflow 已结束时，其下仍被判为运行中 / 未知的子 agent
//!   随之落到结束态（`killed` 的 workflow 不会再写 journal 的 `result`）。
//!
//! 子 agent 转录行（实测 40 个文件、15 053 行，0 条坏行）：顶层键 `agentId`、
//! `type`、`timestamp`、`uuid`、`parentUuid`、`isSidechain`、`sessionId`、`cwd`、
//! `gitBranch`、`slug`、`version`、`message`、`attributionAgent`、`promptId`、
//! `toolUseResult` 等；`type` 只出现 `user` / `attachment` / `assistant`，**没有**
//! `result` 之类的结束记录 → 结束与否靠 journal、父 agent 转录里的结束通知（见下文
//! 「后台子 agent 的结束通知」）或时间窗判定。
//! `attributionAgent` 是子 agent 的**类型**字符串（实测 `Explore` / `Plan` /
//! `general-purpose`），不是 id；token 用量在 `message.usage`，键为
//! `input_tokens` / `output_tokens` / `cache_creation_input_tokens` /
//! `cache_read_input_tokens` 等。`timestamp` 是 RFC3339 毫秒 UTC。
//! `message.content` 既可能是字符串，也可能是块数组（块 `type` 实测 `text` /
//! `thinking` / `tool_use` / `tool_result`）。
//!
//! **一次 API 响应会被按内容块拆成多行 `assistant` 记录**：同一响应的各行共用
//! 同一个 `message.id` 并逐行重复整份 `message.usage`。本机 60 份子 agent 转录
//! 实测 4207 条 assistant 行只对应 2236 个 `message.id`（58/60 份文件存在重复
//! id）；同 id 各行的 `input_tokens` 完全相同、`output_tokens` 单调不减，取最后
//! 一行即该响应的最终用量。逐行相加会把 input 放大约 1.8 倍、output 放大约
//! 1.1%，消息数放大近一倍 → 本适配器按 id 归并。同 id 的行实测总是连续出现
//! （80 份文件 3244 个 id 组，0 例非连续重现），所以只需记住上一个 id。
//! 实测 assistant 行 100% 带 `message.id`；`user` 行带 `message` 但既无 `id`
//! 也无 `usage`；`attachment` 行没有 `message`。
//! `input_tokens` **不含**缓存：本机同一批样本里去重后 `input_tokens` 合计
//! 5544，而 `cache_read_input_tokens` 合计 4.37 亿、`cache_creation_input_tokens`
//! 合计 751 万 → 摘要里的 `in` 取三者之和才有意义。
//!
//! 转录文件实测 321/321 以换行结尾，没有换行的末行只会出现在正在写入的瞬间 →
//! 分页读不消费半截末行，游标留在行首等下一次读到完整行。
//!
//! 主转录里派生子 agent 的工具名实测是 `Agent`（输入键 `description` /
//! `subagent_type` / `model` / `prompt`），不是 `Task`；workflow 派生的子 agent 在
//! 父转录里只有一条 `Workflow` 工具调用 → 反查父转录得不到完整树，目录扫描是
//! 主力。
//!
//! # 后台子 agent 的结束通知（claude 2.1.280）
//!
//! 取证：真机探针会话 1 份 + 本机 241 份主转录的结构统计（只看键名、标签名与
//! 状态取值，不读正文）+ 二进制只读字符串检索。
//!
//! - `Agent` 工具默认后台派生：本机 38 个非 workflow 子 agent 的 meta 边车全是
//!   `requestShape: background`（workflow 下的 935 个是 `foreground`，由 journal
//!   管）；父转录里的工具结果 `toolUseResult` 是 `{agentId, status:
//!   "async_launched", isAsync, description, prompt, outputFile, …}`。
//! - 结束靠 Claude Code 写给父 agent 的**结束通知**：`<task-notification>` 包着的
//!   类 XML 正文，含一个或多个 `<task-id>`（后台子 agent 的就是它的 agentId）与
//!   `<status>`，另有 `<tool-use-id>` / `<output-file>` / `<summary>` / `<note>` /
//!   `<result>` / `<usage>`，自由文本经 XML 转义。`<status>` 的取值（二进制）：
//!   `completed`、`failed`、`killed`（被用户或主 agent 停掉）、`stopped`（上一个
//!   进程退出时没跑完的孤儿，重启后汇总成一条、带多个 task-id）、`blocked`；本机
//!   实测只见 completed / failed。没有 task-id 的汇总通知（「N background commands
//!   completed」）与 task-id 对不上子 agent 的（后台命令、workflow）一律忽略。
//! - 只认三种投递记录：`queue-operation` 且 `operation == "enqueue"`（`content` 是
//!   正文，时刻最接近结束；随后的 `dequeue` / `remove` 不带正文）；`origin.kind ==
//!   "task-notification"` 的 `user` 行（`message.content` 是正文）；回合中投递的
//!   `attachment.type == "queued_command"`（`attachment.prompt` 是正文）。工具说明
//!   里的格式示例（`prompt_snapshot` 附件）、agent 读到的含通知字样的文件内容都不算。
//! - 时序：本机 38 个有转录的通知对象，通知全部晚于子 agent 的最后一条转录（中位
//!   48 ms、最多 1.3 s）。同一 task-id 可以多次通知（子 agent 被 SendMessage 唤起
//!   续跑后再次结束），所以通知之后转录又有活动（超过宽限）视为续跑，回到时间窗
//!   判定。
//! - 位置：父 agent 是主会话时通知在主转录里——只扫尾部窗口，逐行先按开标签预筛、
//!   命中才解析（本机主转录中位 270 KB、p99 21.8 MB），窗口外的旧子 agent 落回
//!   时间窗判定；嵌套派生的通知在父子 agent 的转录里，随转录扫描顺带收集。
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

use std::collections::{BTreeMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{ActivitySource, ContentChunk, SourceContext, SourceError};
use crate::agent_resume::AgentSessionRefKind;
use crate::api::schema::{
    AgentActivityContentFormat, AgentActivityKind, AgentActivityNode, AgentActivityStatus,
};

/// 一次 discover 最多收录的子 agent 数；超出后保留最近修改的那批（分组与待办
/// 节点另计，各自有界）。
const MAX_NODES: usize = 256;
/// 单个子 agent 转录的完整扫描字节上限。
const MAX_TRANSCRIPT_BYTES: u64 = 4 * 1024 * 1024;
/// 一次 discover 完整扫描子 agent 转录的总字节预算，按最近修改优先分配。本机
/// 一个会话就有 227 个约 1 MB 的转录，全扫不现实；预算外的转录只读头部。
const MAX_DISCOVER_BYTES: u64 = 8 * 1024 * 1024;
/// 预算外的转录只读这么多头部字节，取开始时间与首条用户文本。
const HEAD_SCAN_BYTES: u64 = 64 * 1024;
/// 单份 journal.jsonl 的读取上限。
const MAX_JOURNAL_BYTES: u64 = 1024 * 1024;
/// 单份 workflow 状态文件的读取上限（实测最大约 675 KB，含脚本与日志）。
const MAX_WORKFLOW_STATE_BYTES: u64 = 2 * 1024 * 1024;
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
/// 既无 journal 终态也无结束通知时，最后一条转录早于这个窗口就不再算运行中。
const RUNNING_WINDOW_MS: u64 = 120_000;
/// 主转录只扫尾部这么多字节找结束通知（模块文档「后台子 agent 的结束通知」）。
const MAX_NOTIFICATION_SCAN_BYTES: u64 = 4 * 1024 * 1024;
/// 结束通知之后子 agent 转录又有活动、且晚于最后一条通知超过这个宽限，视为被唤起
/// 续跑。宽限吸收被停掉的瞬间在通知之后补写的收尾行。
const RESUME_GRACE_MS: u64 = 5_000;
/// 结束通知正文的开闭标签。
const NOTIFICATION_OPEN: &str = "<task-notification>";
const NOTIFICATION_CLOSE: &str = "</task-notification>";
/// journal 只记了开始时的宽限：workflow 被杀或崩溃且没写状态文件时不会补
/// `result`，转录这么久不动就不再算运行中。取得宽，给长时间静默的工具调用留余量。
const STALE_STARTED_MS: u64 = 30 * 60_000;
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
/// 被丢弃），所以 `Id` 分支要在 [`projects_root`] 的各项目目录下按会话 id 找目录；
/// `Path` 分支兼容日后直接给转录路径的情况。
fn locate_session(cx: &SourceContext<'_>) -> Located {
    let Some(session) = cx.session else {
        return Located::NoSession;
    };
    let candidate = match session.kind {
        AgentSessionRefKind::Path => session_dir_from_transcript(Path::new(&session.value)),
        AgentSessionRefKind::Id => find_session_dir_by_id(&projects_root(cx), &session.value),
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

/// 转录根目录，本适配器所有「按配置目录拼路径」的唯一出口：runtime 解析好的
/// 配置目录（`SourceContext::agent_config_dir`，跟随 `CLAUDE_CONFIG_DIR`）优先，
/// 缺省才回退 `<home>/.claude`。给了配置目录就只认它、不再回头找 home，与 Claude
/// Code 自己的口径一致。
fn projects_root(cx: &SourceContext<'_>) -> PathBuf {
    match cx.agent_config_dir {
        Some(config_dir) => config_dir.join("projects"),
        None => cx.home.join(".claude").join("projects"),
    }
}

/// 在 `<projects>/*/` 下找名为会话 id 的目录。项目 slug 由 cwd 推导的规则并不
/// 可靠（路径里的 `-` 与分隔符会混淆），直接逐个项目目录探测更稳。
fn find_session_dir_by_id(projects: &Path, session_id: &str) -> Option<PathBuf> {
    if session_id.is_empty() || session_id.contains(['/', '\\']) || session_id.contains("..") {
        return None;
    }
    let mut matches: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(projects).ok()?.flatten() {
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
    /// 文件最后修改时间；决定扫描预算的分配顺序，也是头部扫描时的最后活动时间。
    modified_ms: Option<u64>,
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
            let modified_ms = entry
                .metadata()
                .ok()
                .and_then(|metadata| metadata.modified().ok())
                .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
                .and_then(|elapsed| u64::try_from(elapsed.as_millis()).ok());
            found.push(SubagentFile {
                agent_id,
                path,
                rel,
                workflow,
                modified_ms,
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

/// 超过节点上限时保留**最近修改**的那批，再按相对路径排回来保持顺序确定。
///
/// 直接按相对路径截断会让排在后面的目录（如 `subagents/workflows/wf_zzz/`）整批
/// 消失，而那往往正是刚派生、最该看见的一批。缺修改时间的排最后先丢。
fn retain_recent(files: &mut Vec<SubagentFile>, limit: usize) {
    if files.len() <= limit {
        return;
    }
    files.sort_by(|left, right| {
        right
            .modified_ms
            .cmp(&left.modified_ms)
            .then_with(|| left.rel.cmp(&right.rel))
    });
    files.truncate(limit);
    files.sort_by(|left, right| left.rel.cmp(&right.rel));
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

/// workflow 结束后写出的状态文件里本适配器用到的部分。
#[derive(Default)]
struct WorkflowState {
    name: Option<String>,
    status: Option<AgentActivityStatus>,
    started_at_ms: Option<u64>,
    ended_at_ms: Option<u64>,
    total_tokens: Option<u64>,
}

/// 读 `<session>/workflows/<workflow>.json`。文件不存在（workflow 还在跑）、过大
/// 或不是预期形状都返回 `None`，分组节点退回目录名与子节点统计。
fn read_workflow_state(session_dir: &Path, workflow: &str) -> Option<WorkflowState> {
    let path = session_dir
        .join("workflows")
        .join(format!("{workflow}.json"));
    if std::fs::metadata(&path).ok()?.len() > MAX_WORKFLOW_STATE_BYTES {
        return None;
    }
    let text = read_capped(&path, MAX_WORKFLOW_STATE_BYTES)?;
    let value = serde_json::from_str::<Value>(&text).ok()?;
    if !value.is_object() {
        return None;
    }
    let started_at_ms = value.get("startTime").and_then(Value::as_u64);
    let duration_ms = value.get("durationMs").and_then(Value::as_u64);
    Some(WorkflowState {
        name: trimmed_string(value.get("workflowName")),
        status: match value.get("status").and_then(Value::as_str) {
            Some("completed") => Some(AgentActivityStatus::Done),
            Some("killed" | "failed" | "error") => Some(AgentActivityStatus::Failed),
            Some("running") => Some(AgentActivityStatus::Running),
            // 未知取值不猜，交给子节点统计。
            _ => None,
        },
        started_at_ms,
        ended_at_ms: started_at_ms
            .zip(duration_ms)
            .map(|(start, duration)| start.saturating_add(duration)),
        total_tokens: value.get("totalTokens").and_then(Value::as_u64),
    })
}

// ---------------------------------------------------------------------------
// 后台子 agent 的结束通知
// ---------------------------------------------------------------------------

/// 结束通知里的 `<status>`（取值见模块文档）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NotifiedStatus {
    Completed,
    Failed,
    /// `killed`（被用户或主 agent 停掉）与 `stopped`（上一个进程退出时的孤儿）。
    Stopped,
    /// `blocked`：停下来等用户处理，不是终态；之后续跑会写出新的转录活动。
    Blocked,
}

impl NotifiedStatus {
    fn parse(raw: &str) -> Option<Self> {
        match raw.trim() {
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            "killed" | "stopped" => Some(Self::Stopped),
            "blocked" => Some(Self::Blocked),
            // 未知取值不猜，交给时间窗。
            _ => None,
        }
    }

    fn activity_status(self) -> AgentActivityStatus {
        match self {
            Self::Completed => AgentActivityStatus::Done,
            Self::Failed | Self::Stopped => AgentActivityStatus::Failed,
            Self::Blocked => AgentActivityStatus::Blocked,
        }
    }
}

/// 一条带状态的结束通知投递记录。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Notification {
    status: NotifiedStatus,
    at_ms: u64,
}

/// 按 task-id（后台子 agent 的 agentId）归集的投递记录，按转录顺序。
type Notifications = BTreeMap<String, Vec<Notification>>;

/// 一个子 agent 由结束通知得出的终局。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Settled {
    status: NotifiedStatus,
    ended_at_ms: u64,
}

/// 由投递记录与子 agent 转录的最后活动时间得出终局；没有记录，或通知之后转录又有
/// 活动（晚于最后一条通知超过 [`RESUME_GRACE_MS`]，即被唤起续跑）时返回 `None`。
///
/// 状态取最后一条记录的。结束时间取「本轮」（不早于最后活动减宽限）的第一条记录
/// ——入队时刻最接近真正的结束；子 agent 在通知之后还补写了收尾行时取最后活动。
fn settle(records: &[Notification], last_activity_ms: Option<u64>) -> Option<Settled> {
    // 同一时刻的多条取后出现的（转录只追加）。
    let latest = records
        .iter()
        .enumerate()
        .max_by_key(|(index, record)| (record.at_ms, *index))
        .map(|(_, record)| *record)?;
    if last_activity_ms.is_some_and(|last| last > latest.at_ms.saturating_add(RESUME_GRACE_MS)) {
        return None;
    }
    let floor = last_activity_ms.map_or(0, |last| last.saturating_sub(RESUME_GRACE_MS));
    let first = records
        .iter()
        .map(|record| record.at_ms)
        .filter(|at| *at >= floor)
        .min()
        .unwrap_or(latest.at_ms);
    Some(Settled {
        status: latest.status,
        ended_at_ms: last_activity_ms.map_or(first, |last| first.max(last)),
    })
}

/// 扫主转录尾部 `window` 字节里的结束通知，只留 `known` 里的 task-id。逐行先按开
/// 标签做子串预筛，命中的行才解析 JSON：几 MB 的窗口也只是一次顺序读。
fn read_notifications(session_dir: &Path, window: u64, known: &HashSet<&str>) -> Notifications {
    let mut notifications = Notifications::new();
    let Some(path) = main_transcript_path(session_dir) else {
        return notifications;
    };
    let Ok(mut file) = File::open(&path) else {
        return notifications;
    };
    let len = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
    let start = len.saturating_sub(window);
    // 从窗口起点的前一字节读起并丢掉第一段：起点落在行中间时丢的是半行，恰好在行首
    // 时丢的只是上一行的换行符，完整的行一条不丢。
    let skip_first = start > 0;
    if file
        .seek(SeekFrom::Start(start - u64::from(skip_first)))
        .is_err()
    {
        return notifications;
    }
    let mut reader = BufReader::new(std::io::Read::take(
        file,
        window.saturating_add(u64::from(skip_first)),
    ));
    let mut skip = skip_first;
    let mut raw = Vec::new();
    loop {
        raw.clear();
        match reader.read_until(b'\n', &mut raw) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if std::mem::take(&mut skip) {
            continue;
        }
        let line = String::from_utf8_lossy(&raw);
        if !line.contains(NOTIFICATION_OPEN) {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        let Some(stamp) = rfc3339_ms(value.get("timestamp")) else {
            continue;
        };
        collect_notifications(&value, stamp, &mut |id, notification| {
            if known.contains(id) {
                notifications
                    .entry(id.to_string())
                    .or_default()
                    .push(notification);
            }
        });
    }
    notifications
}

/// 一条转录记录若是结束通知的投递记录，把其中每条带状态的通知按 task-id 交给
/// `sink`。
fn collect_notifications(value: &Value, at_ms: u64, sink: &mut dyn FnMut(&str, Notification)) {
    let Some(carrier) = notification_carrier(value) else {
        return;
    };
    match carrier {
        Value::String(text) => parse_notification_text(text, at_ms, sink),
        Value::Array(blocks) => {
            for text in blocks
                .iter()
                .filter_map(|block| block.get("text").and_then(Value::as_str))
            {
                parse_notification_text(text, at_ms, sink);
            }
        }
        _ => {}
    }
}

/// 只认三种投递记录（模块文档），返回装着通知正文的字段。
fn notification_carrier(value: &Value) -> Option<&Value> {
    fn str_field<'a>(object: &'a Value, key: &str) -> Option<&'a str> {
        object.get(key).and_then(Value::as_str)
    }
    match str_field(value, "type")? {
        "queue-operation" => (str_field(value, "operation") == Some("enqueue"))
            .then(|| value.get("content"))
            .flatten(),
        "user" => {
            let kind = value
                .get("origin")
                .and_then(|origin| str_field(origin, "kind"));
            (kind == Some("task-notification"))
                .then(|| {
                    value
                        .get("message")
                        .and_then(|message| message.get("content"))
                })
                .flatten()
        }
        "attachment" => value
            .get("attachment")
            .filter(|attachment| str_field(attachment, "type") == Some("queued_command"))
            .and_then(|attachment| attachment.get("prompt")),
        _ => None,
    }
}

/// 从正文里逐个取出 `<task-notification>` 块；没有可识别 `<status>` 的块（汇总、
/// 「已被唤起」之类的提示）跳过，块里的每个 `<task-id>` 各记一条。
fn parse_notification_text(text: &str, at_ms: u64, sink: &mut dyn FnMut(&str, Notification)) {
    let mut rest = text;
    while let Some(open) = rest.find(NOTIFICATION_OPEN) {
        let body = &rest[open + NOTIFICATION_OPEN.len()..];
        let (body, next) = match body.find(NOTIFICATION_CLOSE) {
            Some(close) => (&body[..close], &body[close + NOTIFICATION_CLOSE.len()..]),
            None => (body, ""),
        };
        rest = next;
        let Some(status) = tag_values(body, "status")
            .first()
            .and_then(|raw| NotifiedStatus::parse(raw))
        else {
            continue;
        };
        for id in tag_values(body, "task-id") {
            let id = id.trim();
            if !id.is_empty() {
                sink(id, Notification { status, at_ms });
            }
        }
    }
}

/// `<name>值</name>` 的全部取值，按出现顺序；缺闭标签的残段不算。
fn tag_values<'a>(body: &'a str, name: &str) -> Vec<&'a str> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let mut values = Vec::new();
    let mut rest = body;
    while let Some(start) = rest.find(&open) {
        let after = &rest[start + open.len()..];
        let Some(end) = after.find(&close) else {
            break;
        };
        values.push(&after[..end]);
        rest = &after[end + close.len()..];
    }
    values
}

// ---------------------------------------------------------------------------
// 转录扫描
// ---------------------------------------------------------------------------

#[derive(Default)]
struct TranscriptScan {
    started_at_ms: Option<u64>,
    last_at_ms: Option<u64>,
    /// 消息条数：同一 `message.id` 的多行只算一条（模块文档「一次 API 响应…」）。
    messages: u32,
    /// 输入 token 合计，含缓存创建与缓存命中；裸 `input_tokens` 只是零头。
    input_tokens: u64,
    output_tokens: u64,
    first_user_text: Option<String>,
    attribution: Option<String>,
    /// 是否完整扫到了文件末尾；只有这时消息数与 token 才是全量。
    complete: bool,
    /// 投递给这个子 agent 的结束通知（它自己派的后台子 agent 结束了），按 task-id。
    notifications: Vec<(String, Notification)>,
}

/// 按 `message.id` 归并同一次 API 响应拆出的多行：同 id 只算一条消息，用量取该组
/// 最后一行（同 id 各行的 usage 是重复的整份快照，见模块文档）。同 id 的行实测
/// 总是连续出现，所以只记住上一组，不用集合，逐行零分配。
#[derive(Default)]
struct UsageMerge {
    open_id: Option<String>,
    open_input: u64,
    open_output: u64,
}

impl UsageMerge {
    /// 收下一条带 `message` 的记录；`id` 为 `None`（实测 `user` 行）的自成一条。
    fn push(&mut self, scan: &mut TranscriptScan, id: Option<&str>, input: u64, output: u64) {
        match id {
            Some(id) => {
                if self.open_id.as_deref() != Some(id) {
                    self.flush(scan);
                    self.open_id = Some(id.to_string());
                }
                self.open_input = input;
                self.open_output = output;
            }
            None => {
                self.flush(scan);
                scan.messages = scan.messages.saturating_add(1);
                scan.input_tokens = scan.input_tokens.saturating_add(input);
                scan.output_tokens = scan.output_tokens.saturating_add(output);
            }
        }
    }

    /// 结算当前这一组；扫描结束时必须调用一次，否则最后一组会丢。
    fn flush(&mut self, scan: &mut TranscriptScan) {
        if self.open_id.take().is_none() {
            return;
        }
        scan.messages = scan.messages.saturating_add(1);
        scan.input_tokens = scan.input_tokens.saturating_add(self.open_input);
        scan.output_tokens = scan.output_tokens.saturating_add(self.open_output);
        self.open_input = 0;
        self.open_output = 0;
    }
}

/// 一条记录的 (输入, 输出) token。输入含缓存创建与缓存命中——裸 `input_tokens`
/// 在本机样本里只占总输入的万分之一量级，单报它会严重低估。
fn usage_tokens(message: &Value) -> (u64, u64) {
    let Some(usage) = message.get("usage") else {
        return (0, 0);
    };
    let input = non_negative(usage.get("input_tokens"))
        .saturating_add(non_negative(usage.get("cache_creation_input_tokens")))
        .saturating_add(non_negative(usage.get("cache_read_input_tokens")));
    (input, non_negative(usage.get("output_tokens")))
}

/// 流式扫描一个子 agent 转录，坏行只跳过。
///
/// `budget` 还有余量时做完整扫描（单文件上限 `MAX_TRANSCRIPT_BYTES`）并扣减预算；
/// 预算用尽后只读 `HEAD_SCAN_BYTES` 头部取开始时间与首条用户文本，拿到就停，
/// 不扣预算。没扫到末尾时最后活动时间取文件修改时间，免得把还在写的转录误判
/// 为早已停止。
fn scan_transcript(path: &Path, modified_ms: Option<u64>, budget: &mut u64) -> TranscriptScan {
    let mut scan = TranscriptScan::default();
    let Ok(file) = File::open(path) else {
        return scan;
    };
    let file_len = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
    let full = *budget > 0;
    let limit = if full {
        MAX_TRANSCRIPT_BYTES.min(*budget)
    } else {
        HEAD_SCAN_BYTES
    };
    let mut reader = BufReader::new(std::io::Read::take(file, limit));
    let mut consumed = 0u64;
    let mut raw = Vec::new();
    let mut merge = UsageMerge::default();
    loop {
        if !full && scan.started_at_ms.is_some() && scan.first_user_text.is_some() {
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
            collect_notifications(&value, stamp, &mut |id, notification| {
                scan.notifications.push((id.to_string(), notification));
            });
        }
        if scan.attribution.is_none() {
            scan.attribution = trimmed_string(value.get("attributionAgent"));
        }
        let Some(message) = value.get("message").filter(|value| value.is_object()) else {
            continue;
        };
        let (input, output) = usage_tokens(message);
        let message_id = message
            .get("id")
            .and_then(Value::as_str)
            .or_else(|| value.get("requestId").and_then(Value::as_str))
            .or_else(|| value.get("uuid").and_then(Value::as_str));
        merge.push(&mut scan, message_id, input, output);
        if scan.first_user_text.is_none()
            && message.get("role").and_then(Value::as_str) == Some("user")
        {
            scan.first_user_text = plain_text(message.get("content")).map(|text| clip(&text));
        }
    }
    merge.flush(&mut scan);
    if full {
        *budget = budget.saturating_sub(consumed);
    }
    scan.complete = full && consumed >= file_len;
    if !scan.complete {
        scan.last_at_ms = scan.last_at_ms.max(modified_ms);
    }
    scan
}

// ---------------------------------------------------------------------------
// 树构建
// ---------------------------------------------------------------------------

fn build_tree(session_dir: &Path, now_ms: u64) -> Vec<AgentActivityNode> {
    let mut files = collect_subagent_files(session_dir);
    retain_recent(&mut files, MAX_NODES);

    // 每个 workflow 目录读一次 journal 与状态文件；读不动就只是少了增强。
    let mut journals: BTreeMap<String, BTreeMap<String, JournalRecord>> = BTreeMap::new();
    let mut workflow_states: BTreeMap<String, Option<WorkflowState>> = BTreeMap::new();
    for workflow in files.iter().filter_map(|file| file.workflow.as_ref()) {
        if !journals.contains_key(workflow) {
            let path = session_dir
                .join("subagents")
                .join("workflows")
                .join(workflow)
                .join("journal.jsonl");
            journals.insert(workflow.clone(), read_journal(&path));
            workflow_states.insert(workflow.clone(), read_workflow_state(session_dir, workflow));
        }
    }

    // 完整扫描的预算先给最近修改的转录：正在跑的子 agent 最需要准确的用量与
    // 时间，早已结束的只读头部也够用。
    let mut scan_order: Vec<usize> = (0..files.len()).collect();
    scan_order.sort_by_key(|&index| std::cmp::Reverse(files[index].modified_ms));
    let mut scans: Vec<TranscriptScan> = Vec::new();
    scans.resize_with(files.len(), TranscriptScan::default);
    let mut budget = MAX_DISCOVER_BYTES;
    for index in scan_order {
        let file = &files[index];
        scans[index] = scan_transcript(&file.path, file.modified_ms, &mut budget);
    }

    // 结束通知：主会话派的在主转录里，嵌套派生的在父子 agent 的转录里（上面的扫描
    // 顺带收了）。只留对得上本会话子 agent 的，后台命令与 workflow 的通知不占内存。
    let known: HashSet<&str> = files.iter().map(|file| file.agent_id.as_str()).collect();
    let mut notifications = read_notifications(session_dir, MAX_NOTIFICATION_SCAN_BYTES, &known);
    for (id, notification) in scans.iter().flat_map(|scan| &scan.notifications) {
        if known.contains(id.as_str()) {
            notifications
                .entry(id.clone())
                .or_default()
                .push(*notification);
        }
    }

    let mut nodes: Vec<AgentActivityNode> = Vec::new();
    // workflow / phase 分组节点在 `nodes` 里的下标，用来回填统计。
    let mut group_index: BTreeMap<String, usize> = BTreeMap::new();
    let mut group_tally: BTreeMap<String, Tally> = BTreeMap::new();

    for (file, scan) in files.iter().zip(&scans) {
        let meta = read_meta(&file.path);
        let journal = file
            .workflow
            .as_ref()
            .and_then(|workflow| journals.get(workflow))
            .and_then(|records| records.get(&file.agent_id));
        let workflow_state = file
            .workflow
            .as_ref()
            .and_then(|workflow| workflow_states.get(workflow))
            .and_then(Option::as_ref);

        let journal_state = journal.and_then(|record| record.state);
        // journal 的终态记录最权威（workflow 子 agent 不发结束通知）；否则结束通知给出
        // 终局，这是后台子 agent 唯一可靠的结束信号；都没有才按时间窗判定。
        let settled = match journal_state {
            Some(JournalState::Done | JournalState::Failed) => None,
            _ => notifications
                .get(&file.agent_id)
                .and_then(|records| settle(records, scan.last_at_ms)),
        };
        let mut status = match settled {
            Some(settled) => settled.status.activity_status(),
            None => subagent_status(journal_state, scan, now_ms),
        };
        // workflow 已结束：还挂着「运行中 / 未知」的子 agent 不可能再跑，随之落到
        // 结束态（被 kill 的 workflow 不会再给子 agent 写 journal 的 result）。
        if let Some(terminal) = workflow_state
            .and_then(|state| state.status)
            .filter(|status| {
                matches!(
                    status,
                    AgentActivityStatus::Done | AgentActivityStatus::Failed
                )
            })
        {
            if !matches!(
                status,
                AgentActivityStatus::Done | AgentActivityStatus::Failed
            ) {
                status = terminal;
            }
        }
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
        let mut summary = subagent_summary(&meta, scan);
        if settled.is_some_and(|settled| settled.status == NotifiedStatus::Stopped) {
            // 被停掉与出错都呈现为失败，摘要注明是停掉的。
            summary = Some(match summary {
                Some(summary) => format!("stopped · {summary}"),
                None => "stopped".to_string(),
            });
        }
        nodes.push(AgentActivityNode {
            id: file.agent_id.clone(),
            kind: AgentActivityKind::Subagent,
            label: subagent_label(&file.agent_id, &meta, journal, scan),
            status,
            parent_id,
            agent_type,
            content_ref: Some(file.rel.clone()),
            summary,
            started_at_ms: scan.started_at_ms,
            ended_at_ms: match settled {
                Some(settled) => Some(settled.ended_at_ms),
                None => (status != AgentActivityStatus::Running)
                    .then_some(scan.last_at_ms)
                    .flatten(),
            },
        });
    }

    for (id, tally) in &group_tally {
        let Some(node) = group_index.get(id).and_then(|index| nodes.get_mut(*index)) else {
            continue;
        };
        node.status = tally.status();
        node.summary = Some(tally.summary());
    }

    // workflow 状态文件给分组节点补人类可读名、终态、起止时间与 token 总数。
    for (workflow, state) in &workflow_states {
        let Some(state) = state else {
            continue;
        };
        let Some(node) = group_index
            .get(&format!("wf:{workflow}"))
            .and_then(|index| nodes.get_mut(*index))
        else {
            continue;
        };
        if let Some(name) = &state.name {
            node.label = clip(name);
        }
        if let Some(status) = state.status {
            node.status = status;
        }
        node.started_at_ms = state.started_at_ms;
        node.ended_at_ms = state.ended_at_ms;
        if let Some(tokens) = state.total_tokens {
            let tally = node.summary.take().unwrap_or_default();
            node.summary = Some(if tally.is_empty() {
                format!("{} tokens", compact_count(tokens))
            } else {
                format!("{tally} · {} tokens", compact_count(tokens))
            });
        }
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

/// journal 有终态记录时以它为准；只记了开始的，转录久未变化就不再算运行中；
/// 没有 journal 就靠「最后一条转录距今多久」判定。判不出来落 `Unknown` 而不是猜
/// `Done`——子 agent 转录里没有任何结束记录（本机实测）。
fn subagent_status(
    journal: Option<JournalState>,
    scan: &TranscriptScan,
    now_ms: u64,
) -> AgentActivityStatus {
    match journal {
        Some(JournalState::Done) => return AgentActivityStatus::Done,
        Some(JournalState::Failed) => return AgentActivityStatus::Failed,
        Some(JournalState::Started) => {
            return match scan.last_at_ms {
                Some(last) if now_ms.saturating_sub(last) > STALE_STARTED_MS => {
                    AgentActivityStatus::Unknown
                }
                _ => AgentActivityStatus::Running,
            };
        }
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
    // 没扫完的转录只有部分计数，宁可不报也不报错数。
    if scan.complete {
        if scan.messages > 0 {
            parts.push(format!("{} msg", scan.messages));
        }
        if scan.input_tokens > 0 {
            parts.push(format!("in {}", compact_count(scan.input_tokens)));
        }
        if scan.output_tokens > 0 {
            parts.push(format!("out {}", compact_count(scan.output_tokens)));
        }
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
            // eof 只表示「暂时读完」，游标照给，二级窗口拿它续读跟随新内容；
            // 文件被截断（len < offset）时收回新末尾，否则永远追不上。
            next_cursor: Some(offset.min(len).to_string()),
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
    let mut drained = false;
    let mut raw = Vec::new();
    while text.len() < budget {
        let line_start = position;
        raw.clear();
        match reader.read_until(b'\n', &mut raw) {
            Ok(0) => {
                drained = true;
                break;
            }
            Ok(read) => position = position.saturating_add(read as u64),
            Err(error) => return Err(SourceError::Io(error)),
        }
        if raw.last() != Some(&b'\n') {
            // 没有换行符结尾 = 这一行还在写。不消费：游标退回行首，下一次读到
            // 完整行再渲染，免得把半截 JSON 渲染成 `[unparsable line]`。
            position = line_start;
            drained = true;
            break;
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

    // `drained` 覆盖「半截末行不消费」这种 position 还没到 len 的读完。
    let eof = drained || position >= len;
    Ok(ContentChunk {
        format: AgentActivityContentFormat::Text,
        text,
        next_cursor: Some(position.to_string()),
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
            agent_config_dir: None,
            latest_hint: None,
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

    /// 每个测试自己的临时目录（仓库不带 tempfile 依赖），作用域结束删掉。
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0);
            let path = std::env::temp_dir().join(format!(
                "herdr-claude-activity-{name}-{}-{nanos}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("建临时目录");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// 把夹具树整棵复制到 `to`。
    fn copy_tree(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).expect("建目标目录");
        for entry in std::fs::read_dir(from).expect("夹具目录可读") {
            let entry = entry.expect("夹具目录项可读");
            let target = to.join(entry.file_name());
            if entry.file_type().expect("夹具目录项类型可读").is_dir() {
                copy_tree(&entry.path(), &target);
            } else {
                std::fs::copy(entry.path(), &target).expect("复制夹具文件");
            }
        }
    }

    /// `CLAUDE_CONFIG_DIR` 把配置目录挪出 home 时：runtime 给的配置目录优先，home 下
    /// 没有 `.claude` 也能找到并读到会话；给了配置目录就只认它，不回退 home。
    #[test]
    fn a_relocated_config_dir_is_followed_instead_of_home() {
        let temp = TempDir::new("config-dir");
        let config_dir = temp.path().join("profiles").join("work");
        copy_tree(&fixture_home().join(".claude"), &config_dir);
        let empty_home = temp.path().join("home");
        std::fs::create_dir_all(&empty_home).expect("建空 home");
        let session = session_ref(SESSION_ID);

        let relocated = SourceContext {
            agent_config_dir: Some(&config_dir),
            ..context(&empty_home, Some(&session), FIXTURE_LAST_MS)
        };
        let nodes = Claude.discover(&relocated).expect("配置目录下的会话可发现");
        assert!(!nodes.is_empty());
        assert_eq!(
            nodes,
            discover(SESSION_ID, FIXTURE_LAST_MS),
            "与默认布局同一棵树"
        );

        let home = fixture_home();
        let default_cx = context(&home, Some(&session), FIXTURE_LAST_MS);
        let expected = Claude
            .read(&default_cx, "a0000000000000001", None, 64 * 1024)
            .expect("默认布局可读");
        let page = Claude
            .read(&relocated, "a0000000000000001", None, 64 * 1024)
            .expect("配置目录下的转录可读");
        assert!(!page.text.is_empty());
        assert_eq!(page.text, expected.text);

        // 不给配置目录：回退 `<home>/.claude`，空 home 下什么都没有。
        let fallback = context(&empty_home, Some(&session), FIXTURE_LAST_MS);
        assert!(Claude.discover(&fallback).expect("缺目录不报错").is_empty());
        assert!(matches!(
            Claude.read(&fallback, "a0000000000000001", None, 4096),
            Err(SourceError::Unavailable)
        ));

        // 给了配置目录就只认它：home 下明明有数据也不回头找。
        let nowhere = temp.path().join("nowhere");
        let pinned = SourceContext {
            agent_config_dir: Some(&nowhere),
            ..context(&home, Some(&session), FIXTURE_LAST_MS)
        };
        assert!(Claude.discover(&pinned).expect("缺目录不报错").is_empty());
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
                "a0000000000000006",
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
        // 夹具里一次响应拆成 3 行 assistant，共用一个 message.id 并重复同一份
        // usage：消息数与 token 都不得翻倍（逐行相加会得到 4 msg / in 3.7k /
        // out 710）。
        assert_eq!(
            first.summary.as_deref(),
            Some("sonnet · 2 msg · in 1.2k · out 340")
        );
    }

    #[test]
    fn the_node_cap_keeps_the_most_recently_modified_subagents() {
        fn file(rel: &str, modified_ms: Option<u64>) -> SubagentFile {
            SubagentFile {
                agent_id: rel.to_string(),
                path: PathBuf::from(rel),
                rel: rel.to_string(),
                workflow: None,
                modified_ms,
            }
        }

        // 按相对路径排在最后、但刚写过的那批必须留下；缺修改时间的先丢。
        let mut files = vec![
            file("subagents/a", Some(10)),
            file("subagents/b", Some(40)),
            file("subagents/c", None),
            file("subagents/workflows/wf_z/d", Some(30)),
        ];
        retain_recent(&mut files, 2);
        assert_eq!(
            files
                .iter()
                .map(|file| file.rel.as_str())
                .collect::<Vec<_>>(),
            ["subagents/b", "subagents/workflows/wf_z/d"],
            "保留最近修改的两个，并排回相对路径序"
        );

        // 未超限时一个不动。
        let mut few = vec![file("subagents/b", Some(1)), file("subagents/a", Some(9))];
        retain_recent(&mut few, 2);
        assert_eq!(
            few.iter().map(|file| file.rel.as_str()).collect::<Vec<_>>(),
            ["subagents/b", "subagents/a"]
        );
    }

    #[test]
    fn a_missing_sidecar_falls_back_to_the_transcript() {
        let nodes = discover(SESSION_ID, FIXTURE_LAST_MS);
        let second = node(&nodes, "a0000000000000002");
        // 无 meta.json、无 journal → label 取首条用户文本，类型取 attributionAgent。
        assert_eq!(second.label, "check the config reference");
        assert_eq!(second.agent_type.as_deref(), Some("Plan"));
        // 输入合计含缓存创建与缓存命中：10 + 300 + 4000 = 4310。只看裸
        // input_tokens 会报成 10，与真实用量差几个数量级。
        assert_eq!(second.summary.as_deref(), Some("2 msg · in 4.3k"));
    }

    #[test]
    fn repeated_usage_rows_sharing_a_message_id_are_merged() {
        let path = fixture_home().join(
            ".claude/projects/-tmp-demo-project/5f000000-0000-4000-8000-000000000001/subagents/agent-a0000000000000001.jsonl",
        );
        let mut budget = MAX_DISCOVER_BYTES;
        let scan = scan_transcript(&path, None, &mut budget);
        assert!(scan.complete);
        // 1 条 user + 1 组 assistant（3 行同 id）= 2 条消息。
        assert_eq!(scan.messages, 2);
        // 同 id 各行的 usage 是重复快照：input 只取一次，output 取组内最后一行。
        assert_eq!(scan.input_tokens, 1234);
        assert_eq!(scan.output_tokens, 340);

        // 没有 message.id 的行（实测 user 行）各算一条，用量直接计入；它也会
        // 结束上一组，之后同名 id 重新开组。
        let mut scan = TranscriptScan::default();
        let mut merge = UsageMerge::default();
        merge.push(&mut scan, Some("m1"), 10, 1);
        merge.push(&mut scan, Some("m1"), 10, 2);
        merge.push(&mut scan, None, 5, 5);
        merge.push(&mut scan, Some("m1"), 10, 3);
        merge.flush(&mut scan);
        assert_eq!(
            (scan.messages, scan.input_tokens, scan.output_tokens),
            (3, 25, 10)
        );
        // 重复 flush 不会重复计数。
        merge.flush(&mut scan);
        assert_eq!(scan.messages, 3);
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
    fn a_finished_workflow_names_its_group_and_settles_its_children() {
        let nodes = discover(SESSION_ID, FIXTURE_LAST_MS);
        let workflow = node(&nodes, "wf:wf_demo-0002");
        assert_eq!(workflow.label, "demo-pipeline");
        assert_eq!(workflow.status, AgentActivityStatus::Done);
        assert_eq!(workflow.started_at_ms, Some(1_789_900_000_000));
        assert_eq!(workflow.ended_at_ms, Some(1_789_900_060_000));
        assert_eq!(
            workflow.summary.as_deref(),
            Some("0/1 running · 12.3k tokens")
        );
        // a5 无 journal、按时间窗本应是 Unknown；workflow 已完成，随之落 Done。
        let child = node(&nodes, "a0000000000000005");
        assert_eq!(child.status, AgentActivityStatus::Done);
        assert_eq!(child.ended_at_ms, Some(1_789_900_000_000));
    }

    #[test]
    fn a_malformed_workflow_state_falls_back_to_the_directory_name() {
        let nodes = discover(SESSION_ID, FIXTURE_LAST_MS);
        // wf_demo-0001.json 是截断的坏文件：名字退回目录名，状态退回子节点统计。
        let workflow = node(&nodes, "wf:wf_demo-0001");
        assert_eq!(workflow.label, "wf_demo-0001");
        assert_eq!(workflow.status, AgentActivityStatus::Running);
        assert_eq!(workflow.started_at_ms, None);
    }

    #[test]
    fn an_exhausted_scan_budget_falls_back_to_the_head_and_file_time() {
        let path = fixture_home().join(
            ".claude/projects/-tmp-demo-project/5f000000-0000-4000-8000-000000000001/subagents/agent-a0000000000000001.jsonl",
        );
        let later = FIXTURE_LAST_MS + 3_600_000;

        // 预算用尽：只读头部，拿到开始时间与首条用户文本就停，不扣预算。
        let mut budget = 0;
        let head = scan_transcript(&path, Some(later), &mut budget);
        assert_eq!(budget, 0);
        assert!(!head.complete);
        assert_eq!(head.started_at_ms, Some(1_790_071_440_000));
        assert_eq!(
            head.first_user_text.as_deref(),
            Some("map the render hot path")
        );
        // 没扫到末尾时最后活动时间取文件修改时间。
        assert_eq!(head.last_at_ms, Some(later));
        assert_eq!(
            subagent_summary(&SubagentMeta::default(), &head),
            None,
            "部分计数不进摘要"
        );

        // 预算只够半行：扣光预算，同样标记为未扫完。
        let mut budget = 100;
        let partial = scan_transcript(&path, Some(later), &mut budget);
        assert_eq!(budget, 0);
        assert!(!partial.complete);
        assert_eq!(partial.last_at_ms, Some(later));

        // 预算充足：完整扫描，按内容时间而不是文件时间。
        let mut budget = MAX_DISCOVER_BYTES;
        let full = scan_transcript(&path, Some(later), &mut budget);
        assert!(full.complete);
        assert_eq!(full.last_at_ms, Some(FIXTURE_LAST_MS));
        assert!(budget < MAX_DISCOVER_BYTES);
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

    /// 后台（异步）子 agent 的会话夹具：主转录里有结束通知的各种投递记录。
    const ASYNC_SESSION_ID: &str = "5f000000-0000-4000-8000-000000000002";
    /// 该夹具的 2026-09-23T10:00:00.000Z；各子 agent 的最后一条转录都在 10:00:25 之前。
    const ASYNC_BASE_MS: u64 = 1_790_157_600_000;

    /// 冒烟 M1（claude-04 / 09 / 15）：后台子 agent 早已结束，只看时间窗时 120 s 内
    /// 一直算运行中、过窗落「未知」；主转录里的结束通知才是它的终局。
    #[test]
    fn async_subagents_settle_from_their_task_notifications() {
        // 10:01:00：每个子 agent 的最后一条转录都还在 120 s 窗口内。
        let nodes = discover(ASYNC_SESSION_ID, ASYNC_BASE_MS + 60_000);

        // 入队 → 出队 → user 行投递的 completed：完成，结束时间取最后活动之后的第一条
        // 通知（入队时刻），不是 user 行的投递时刻。
        let completed = node(&nodes, "b0000000000000001");
        assert_eq!(completed.status, AgentActivityStatus::Done);
        assert_eq!(completed.ended_at_ms, Some(ASYNC_BASE_MS + 4_500));
        assert_eq!(completed.started_at_ms, Some(ASYNC_BASE_MS + 1_000));

        // 回合中投递（入队 → 移出 → queued_command 附件）的 failed；通知之后宽限内
        // 补写的收尾行不算被唤起，结束时间取那条收尾行。
        let failed = node(&nodes, "b0000000000000002");
        assert_eq!(failed.status, AgentActivityStatus::Failed);
        assert_eq!(failed.ended_at_ms, Some(ASYNC_BASE_MS + 7_000));

        // 重启后的孤儿汇总：一条通知里多个 task-id、状态 stopped → 失败并注明停止。
        let stopped = node(&nodes, "b0000000000000003");
        assert_eq!(stopped.status, AgentActivityStatus::Failed);
        assert_eq!(stopped.ended_at_ms, Some(ASYNC_BASE_MS + 30_000));
        assert!(
            stopped
                .summary
                .as_deref()
                .is_some_and(|summary| summary.starts_with("stopped")),
            "{:?}",
            stopped.summary
        );

        // 嵌套：b1 自己派的后台子 agent，结束通知投递在 b1 的转录里。
        let nested = node(&nodes, "b0000000000000006");
        assert_eq!(nested.status, AgentActivityStatus::Done);
        assert_eq!(nested.ended_at_ms, Some(ASYNC_BASE_MS + 3_300));
    }

    #[test]
    fn subagents_without_a_final_notification_keep_the_freshness_window() {
        let fresh = discover(ASYNC_SESSION_ID, ASYNC_BASE_MS + 60_000);
        // b4 没有结束通知；工具说明里的通知格式示例点了它的名也不算。
        let silent = node(&fresh, "b0000000000000004");
        assert_eq!(silent.status, AgentActivityStatus::Running);
        assert_eq!(silent.ended_at_ms, None);
        // b5 收到 completed 之后又被 SendMessage 唤起：通知之后的活动晚于宽限，
        // 回到时间窗判定。
        let resumed = node(&fresh, "b0000000000000005");
        assert_eq!(resumed.status, AgentActivityStatus::Running);
        assert_eq!(resumed.ended_at_ms, None);

        // 10:05:00 过窗：没有终局的两个落「未知」，有终局的不受时间窗影响。
        let stale = discover(ASYNC_SESSION_ID, ASYNC_BASE_MS + 300_000);
        assert_eq!(
            node(&stale, "b0000000000000004").status,
            AgentActivityStatus::Unknown
        );
        assert_eq!(
            node(&stale, "b0000000000000005").status,
            AgentActivityStatus::Unknown
        );
        assert_eq!(
            node(&stale, "b0000000000000001").status,
            AgentActivityStatus::Done
        );
    }

    #[test]
    fn settling_prefers_the_latest_run_and_its_first_notification() {
        let record = |status, at_ms| Notification { status, at_ms };
        use NotifiedStatus::{Blocked, Completed, Failed, Stopped};
        assert_eq!(settle(&[], Some(10)), None);
        // 没有转录时间：取第一条记录作结束时间。
        assert_eq!(
            settle(&[record(Completed, 100), record(Completed, 105)], None),
            Some(Settled {
                status: Completed,
                ended_at_ms: 100
            })
        );
        // 续跑后再次结束：状态取最后一条，结束时间取第二轮的入队时刻。
        let twice = [
            record(Completed, 8_500),
            record(Completed, 8_510),
            record(Failed, 30_000),
            record(Failed, 30_010),
        ];
        assert_eq!(
            settle(&twice, Some(29_500)),
            Some(Settled {
                status: Failed,
                ended_at_ms: 30_000
            })
        );
        // 宽限边界：恰好晚 5 s 仍算收尾，再晚 1 ms 才算续跑。
        let once = [record(Stopped, 1_000)];
        assert!(settle(&once, Some(1_000 + RESUME_GRACE_MS)).is_some());
        assert_eq!(settle(&once, Some(1_001 + RESUME_GRACE_MS)), None);
        // 同一时刻的两条取后出现的。
        assert_eq!(
            settle(&[record(Blocked, 50), record(Completed, 50)], Some(40))
                .map(|settled| settled.status),
            Some(Completed)
        );

        assert_eq!(NotifiedStatus::parse(" killed "), Some(Stopped));
        assert_eq!(NotifiedStatus::parse("stopped"), Some(Stopped));
        assert_eq!(NotifiedStatus::parse("blocked"), Some(Blocked));
        assert_eq!(NotifiedStatus::parse("paused"), None);
        assert_eq!(Blocked.activity_status(), AgentActivityStatus::Blocked);
        assert_eq!(Stopped.activity_status(), AgentActivityStatus::Failed);
    }

    #[test]
    fn notification_text_parsing_stays_total_on_odd_input() {
        let mut seen: Vec<(String, NotifiedStatus)> = Vec::new();
        let text = "noise <task-notification><task-id>a1</task-id><status>completed</status>\
                    </task-notification> between <task-notification>\n<summary>2 background \
                    commands completed</summary>\n<status>completed</status>\n</task-notification>\
                    <task-notification><task-id> a2 </task-id><task-id></task-id>\
                    <status>failed</status><result>x &lt;task-id&gt;a9&lt;/task-id&gt;</result>\
                    </task-notification><task-notification><task-id>a3</task-id>\
                    <summary>resumed</summary></task-notification><task-notification>\
                    <task-id>a4</task-id><status>killed";
        parse_notification_text(text, 7, &mut |id, notification| {
            assert_eq!(notification.at_ms, 7);
            seen.push((id.to_string(), notification.status));
        });
        // 汇总（无 task-id）、「已被唤起」（无 status）、转义过的正文与空 id 都不产出；
        // 缺闭标签的末块照样按已有字段解析。
        assert_eq!(
            seen,
            [
                ("a1".to_string(), NotifiedStatus::Completed),
                ("a2".to_string(), NotifiedStatus::Failed),
            ]
        );
        assert_eq!(tag_values("<x>1</x><x>2", "x"), ["1"]);
        assert!(tag_values("", "x").is_empty());

        // 只认三种投递记录。
        let carrier = |line: &str| {
            let value: Value = serde_json::from_str(line).expect("测试行是 JSON");
            notification_carrier(&value).is_some()
        };
        assert!(carrier(
            r#"{"type":"queue-operation","operation":"enqueue","content":"x"}"#
        ));
        assert!(!carrier(
            r#"{"type":"queue-operation","operation":"remove","content":"x"}"#
        ));
        assert!(carrier(
            r#"{"type":"user","origin":{"kind":"task-notification"},"message":{"content":"x"}}"#
        ));
        assert!(!carrier(
            r#"{"type":"user","origin":{"kind":"human"},"message":{"content":"x"}}"#
        ));
        assert!(carrier(
            r#"{"type":"attachment","attachment":{"type":"queued_command","prompt":"x"}}"#
        ));
        assert!(!carrier(
            r#"{"type":"attachment","attachment":{"type":"prompt_snapshot","tools":[]}}"#
        ));
        assert!(!carrier(
            r#"{"type":"assistant","message":{"content":"x"}}"#
        ));
    }

    #[test]
    fn the_main_transcript_is_only_scanned_within_its_tail_window() {
        let temp = TempDir::new("notification-window");
        let session_dir = temp.path().join("s1");
        std::fs::create_dir_all(&session_dir).expect("建会话目录");
        let row = |id: &str, second: u32| {
            format!(
                r#"{{"type":"queue-operation","operation":"enqueue","timestamp":"2026-09-23T10:00:{second:02}.000Z","content":"<task-notification><task-id>{id}</task-id><status>completed</status></task-notification>"}}"#
            ) + "\n"
        };
        let (old, cut, kept) = (row("c1", 1), row("c2", 2), row("c3", 3));
        std::fs::write(temp.path().join("s1.jsonl"), format!("{old}{cut}{kept}"))
            .expect("写主转录");
        let known: HashSet<&str> = ["c1", "c2", "c3"].into_iter().collect();
        let ids = |window: u64| {
            read_notifications(&session_dir, window, &known)
                .into_keys()
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(u64::MAX), ["c1", "c2", "c3"]);
        // 窗口起点恰好是行首：那一行完整保留。
        assert_eq!(ids((cut.len() + kept.len()) as u64), ["c2", "c3"]);
        // 起点落在行中间：那半行丢掉，不会被当成坏行以外的任何东西。
        assert_eq!(ids((cut.len() + kept.len() - 1) as u64), ["c3"]);
        // 只收已知的子 agent。
        let only: HashSet<&str> = ["c2"].into_iter().collect();
        assert_eq!(
            read_notifications(&session_dir, u64::MAX, &only)
                .into_keys()
                .collect::<Vec<_>>(),
            ["c2"]
        );
        // 没有主转录：空。
        assert!(read_notifications(&temp.path().join("nope"), u64::MAX, &known).is_empty());
    }

    #[test]
    fn a_journal_started_agent_goes_stale_when_its_transcript_stops() {
        // a3 的 journal 只有 started；转录最后一条在 10:00。
        let started_ms = 1_790_071_200_000;
        let within = discover(SESSION_ID, started_ms + STALE_STARTED_MS);
        assert_eq!(
            node(&within, "a0000000000000003").status,
            AgentActivityStatus::Running
        );
        // 超过宽限仍无动静：workflow 多半已被杀，不再算运行中，分组也不再计入。
        let stale = discover(SESSION_ID, started_ms + STALE_STARTED_MS + 1);
        assert_eq!(
            node(&stale, "a0000000000000003").status,
            AgentActivityStatus::Unknown
        );
        assert_eq!(
            node(&stale, "wf:wf_demo-0001").summary.as_deref(),
            Some("0/2 running")
        );
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
        assert!(!page.truncated);
        // eof 只表示「暂时读完」：游标照给，二级窗口拿它续读跟随新内容。
        let cursor = page.next_cursor.clone().expect("eof 也要给出游标");
        let again = Claude
            .read(&cx, "a0000000000000001", Some(&cursor), MIN_READ_BYTES)
            .expect("用 eof 游标续读");
        assert!(again.text.is_empty(), "{}", again.text);
        assert!(again.eof);
        assert_eq!(again.next_cursor.as_deref(), Some(cursor.as_str()));

        let beyond = Claude
            .read(&cx, "a0000000000000001", Some("999999"), MIN_READ_BYTES)
            .expect("越界游标回空页");
        assert!(beyond.eof);
        assert!(beyond.text.is_empty());
        // 文件比游标短（被截断）时游标收回新末尾，否则永远追不上。
        assert_eq!(beyond.next_cursor.as_deref(), Some(cursor.as_str()));
    }

    #[test]
    fn a_half_written_tail_line_is_never_consumed() {
        let home = fixture_home();
        let session = session_ref(SESSION_ID);
        let cx = context(&home, Some(&session), FIXTURE_LAST_MS);
        let path = home.join(
            ".claude/projects/-tmp-demo-project/5f000000-0000-4000-8000-000000000001/subagents/agent-a0000000000000006.jsonl",
        );
        let raw = std::fs::read(&path).expect("夹具可读");
        assert_ne!(raw.last(), Some(&b'\n'), "夹具末行必须没有换行");
        let complete = raw
            .iter()
            .rposition(|byte| *byte == b'\n')
            .expect("夹具有完整行")
            + 1;

        let page = Claude
            .read(&cx, "a0000000000000006", None, MIN_READ_BYTES)
            .expect("可读");
        assert_eq!(page.text, "[user] follow the tail\n[assistant] tailing\n");
        assert!(!page.text.contains("[unparsable line]"), "{}", page.text);
        assert!(page.eof);
        // 游标停在半截行的行首，等它写完再消费。
        assert_eq!(
            page.next_cursor.as_deref(),
            Some(complete.to_string().as_str())
        );
    }

    #[test]
    fn a_completed_tail_line_is_picked_up_by_the_eof_cursor() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        let path = std::env::temp_dir().join(format!(
            "herdr-claude-activity-tail-{}-{nanos}.jsonl",
            std::process::id()
        ));
        let row = |text: &str| {
            format!(
                r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"text","text":"{text}"}}]}}}}"#
            )
        };
        // 末行已经是合法 JSON，但还没写换行 → 仍然不能消费，它可能还在长。
        std::fs::write(&path, format!("{}\n{}", row("done"), row("half"))).expect("写临时转录");
        let first = read_transcript_page(&path, 0, MIN_READ_BYTES).expect("首页可读");
        assert_eq!(first.text, "[assistant] done\n");
        assert!(first.eof);
        let cursor: u64 = first
            .next_cursor
            .expect("eof 也要给出游标")
            .parse()
            .expect("游标是字节偏移");

        std::fs::write(&path, format!("{}\n{}\n", row("done"), row("half"))).expect("补齐末行");
        let second = read_transcript_page(&path, cursor, MIN_READ_BYTES).expect("续读");
        assert_eq!(second.text, "[assistant] half\n", "不重不漏地接上");
        assert!(second.eof);
        let _ = std::fs::remove_file(&path);
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
                assert!(page.next_cursor.is_some(), "eof 也要给出续读游标");
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
