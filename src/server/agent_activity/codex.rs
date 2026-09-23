//! Codex CLI 的活动来源适配器：从 `<config>/sessions/YYYY/MM/DD/rollout-*.jsonl`
//! （`<config>` 跟随 `CODEX_HOME`，缺省 `<home>/.codex`）读出以本 pane 线程为祖先的
//! 子线程（`thread_spawn` 子 agent、guardian 审核等），`read` 按字节游标分页返回
//! 子线程 rollout 的可读转写（文本，见下文「内容转写」）。
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
//!
//! # 内容转写（codex-cli 0.156.1，2026-09-23 本机 12 个 rollout，含真机探针的 1 个
//! 子线程；只看类型、键名与枚举取值，另做二进制只读字符串检索）
//!
//! - 每条记录是 `{ordinal, timestamp, type, payload}`，`ordinal` 等于 0 起的行号；
//!   子线程自己的 `session_meta` 带 `subagent_history_start_ordinal`，它之前的记录
//!   （第 1 行起的父 meta 副本与父历史前缀，含用户给父线程的原始提示）不属于子线程，
//!   转写跳过。早期版本没有 `ordinal` 时不跳。
//! - 顶层类型：`session_meta`、`turn_context`、`world_state`、`token_usage_record`、
//!   `inter_agent_communication_metadata` 是簿记，不进转写；二进制里另有
//!   `inter_agent_communication`、`compacted`、`realtime_item`。
//! - `response_item`：`message`（role developer / user / assistant；developer 是
//!   给模型的指令，不进转写；assistant 带 `phase` commentary / final_answer）、
//!   `agent_message`（`author` / `recipient`，正文 `input_text` 是任务头，负载在
//!   `encrypted_content` 里加密）、`reasoning`（本机 43 条的 `summary` 全为空，只有
//!   加密内容）、`function_call`（`arguments` 是 JSON 字符串；`spawn_agent` 的
//!   `message` 同样加密，明文只有 `task_name`）、`custom_tool_call`（`exec` 的 `input`
//!   是自由文本脚本）、`function_call_output` / `custom_tool_call_output`（`output` 是
//!   字符串或 `input_text` 块数组）；二进制里另有 `local_shell_call`、
//!   `web_search_call`、`image_generation_call`、`tool_search_*`、`compaction*` 等。
//! - `event_msg`：`item_completed`（Reasoning / CommandExecution / AgentMessage /
//!   UserMessage / SubAgentActivity / FileChange，都是 `response_item` 的镜像，只有
//!   Plan 条目只以它落盘）、`token_count`（遥测，含 rate_limits）、`task_started` /
//!   `thread_settings_applied`（簿记）、`task_complete`、`turn_aborted`。
//! - 转写口径与 opencode 一致：`user: …`、助手正文原样、`(thinking)`、`[工具名] 参数
//!   摘要` 加缩进的输出前几行、`[task complete] 最后一条消息`；认不出的类型只留一行
//!   `[类型名]`，不整条倒出 JSON。

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
/// `read` 的默认页与页大小区间。
const DEFAULT_READ_BYTES: usize = 64 * 1024;
const MIN_READ_BYTES: usize = 256;
const MAX_READ_BYTES: usize = 4 * 1024 * 1024;
/// 一页最多扫这么多原始字节：隐藏的簿记记录（meta、world_state 动辄几十 KB）太多时
/// 先交这一页，游标停在已扫到的位置，续读接着往后。
const MAX_PAGE_SCAN_BYTES: u64 = 8 * 1024 * 1024;
/// 单条记录的读取上限；更长的整条跳过、留一行占位，不把它读进内存。
const MAX_RECORD_BYTES: u64 = 16 * 1024 * 1024;
/// 消息正文（user / assistant / 协作消息）的字符上限。
const MESSAGE_CHARS: usize = 2000;
/// 工具调用摘要、结束消息等单行摘要的字符上限。
const SUMMARY_CHARS: usize = 160;
/// 工具输出只显示前几行，其余只计数。
const OUTPUT_LINES: usize = 5;
/// 工具输出每行的字符上限。
const OUTPUT_LINE_CHARS: usize = 200;
/// 类型名、工具名的字符上限。
const NAME_CHARS: usize = 60;
/// `function_call` 参数里拿来当摘要的键，按优先级；都没有就给截断的紧凑 JSON。
const CALL_ARGUMENT_KEYS: [&str; 8] = [
    "cmd",
    "command",
    "task_name",
    "path",
    "file_path",
    "query",
    "pattern",
    "url",
];
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
        read_transcript_page(&path, cursor, max_bytes)
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
    /// `subagent_history_start_ordinal`：子线程自己的历史从这个 ordinal 开始，之前是
    /// 继承的父历史前缀。
    history_start: Option<u64>,
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
        history_start: payload
            .get("subagent_history_start_ordinal")
            .and_then(Value::as_u64),
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

/// 一页的上限：渲染文本的字节预算、单页扫描的原始字节、单条记录的原始字节。
#[derive(Clone, Copy, Debug)]
struct PageLimits {
    budget: usize,
    scan_bytes: u64,
    record_bytes: u64,
}

/// 游标是 rollout 的字节偏移，页尾总在记录边界上；每条记录渲染成可读转写（见
/// [`render_record`]），隐藏的记录照样推进游标。`next_cursor` 总是给出（含 `eof`），
/// `eof` 只表示这次已读到文件末尾；末尾不以换行结尾的半截记录不消费，游标停在它的
/// 行首，跟随增长时用同一游标再读。单条渲染结果比整页预算还大时截在字符边界并标记
/// `truncated`。
fn read_transcript_page(
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
        max_bytes.clamp(MIN_READ_BYTES, MAX_READ_BYTES)
    };
    render_page(
        path,
        offset,
        PageLimits {
            budget,
            scan_bytes: MAX_PAGE_SCAN_BYTES,
            record_bytes: MAX_RECORD_BYTES,
        },
    )
}

fn render_page(path: &Path, offset: u64, limits: PageLimits) -> Result<ContentChunk, SourceError> {
    // 子线程开头继承的父历史前缀：ordinal 小于它的记录不属于这个子线程。
    let history_start = read_meta(path).and_then(|meta| meta.history_start);
    let mut file = fs::File::open(path).map_err(SourceError::Io)?;
    let length = file.metadata().map_err(SourceError::Io)?.len();
    let offset = offset.min(length);
    file.seek(SeekFrom::Start(offset))
        .map_err(SourceError::Io)?;
    let mut reader = BufReader::new(file);
    let mut position = offset;
    let mut text = String::new();
    let mut truncated = false;
    // 停在还没写完的末条记录上：这一次已经读到头了。
    let mut waiting = false;
    let mut raw = Vec::new();
    while text.len() < limits.budget && position - offset < limits.scan_bytes {
        let line_start = position;
        raw.clear();
        let read = (&mut reader)
            .take(limits.record_bytes)
            .read_until(b'\n', &mut raw)
            .map_err(SourceError::Io)?;
        if read == 0 {
            break;
        }
        let rendered = if raw.last() == Some(&b'\n') {
            position += read as u64;
            render_record(&String::from_utf8_lossy(&raw), history_start)
        } else if (read as u64) < limits.record_bytes {
            // 没有换行 = 这条还在写：不消费，游标留在行首，写完再渲染。
            waiting = true;
            break;
        } else {
            // 超长记录：不读进内存，跳到下一个换行；还没写完就同样等下一次。
            let (skipped, complete) = skip_line(&mut reader).map_err(SourceError::Io)?;
            if !complete {
                waiting = true;
                break;
            }
            position += read as u64 + skipped;
            Some("[oversized record]".to_string())
        };
        let Some(rendered) = rendered else {
            continue;
        };
        let remaining = limits.budget.saturating_sub(text.len());
        if rendered.len() + 1 > remaining {
            if !text.is_empty() {
                // 本页装不下就整条留给下一页：游标退回行首，内容不丢。
                position = line_start;
                break;
            }
            // 单条本身超过整页预算，只能截到字符边界并标记。
            const MARK: &str = " …\n";
            let end = floor_char_boundary(
                rendered.as_bytes(),
                limits.budget.saturating_sub(MARK.len()),
            );
            text.push_str(&rendered[..end]);
            text.push_str(MARK);
            truncated = true;
            break;
        }
        text.push_str(&rendered);
        text.push('\n');
    }
    Ok(ContentChunk {
        format: AgentActivityContentFormat::Text,
        text,
        next_cursor: Some(position.to_string()),
        eof: waiting || position >= length,
        truncated,
    })
}

/// 跳过当前行余下的部分（不留在内存里）。返回跳过的字节数与是否遇到了换行。
fn skip_line(reader: &mut impl BufRead) -> io::Result<(u64, bool)> {
    let mut skipped = 0u64;
    loop {
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            return Ok((skipped, false));
        }
        if let Some(newline) = buffer.iter().position(|byte| *byte == b'\n') {
            reader.consume(newline + 1);
            return Ok((skipped + newline as u64 + 1, true));
        }
        let length = buffer.len();
        reader.consume(length);
        skipped += length as u64;
    }
}

// ---- 转写渲染 ----

/// 把一条 rollout 记录渲染成转写文本；簿记与遥测记录、继承的父历史前缀返回
/// `None`。认不出的类型只留一行类型名，不整条倒出 JSON。这是用户自己的会话内容，
/// 只进本地内容片段，不进日志。
fn render_record(line: &str, history_start: Option<u64>) -> Option<String> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let Ok(record) = serde_json::from_str::<Value>(line) else {
        return Some("[unparsable line]".to_string());
    };
    if let (Some(start), Some(ordinal)) =
        (history_start, record.get("ordinal").and_then(Value::as_u64))
    {
        if ordinal < start {
            return None;
        }
    }
    let payload = record.get("payload");
    match record.get("type").and_then(Value::as_str)? {
        "response_item" => render_response_item(payload?),
        "event_msg" => render_event(payload?),
        "compacted" => Some("[compacted]".to_string()),
        // 会话元数据、回合上下文、世界状态、用量记录、协作元数据：簿记，不进转写。
        "session_meta"
        | "turn_context"
        | "world_state"
        | "token_usage_record"
        | "inter_agent_communication_metadata" => None,
        other => Some(type_line(other)),
    }
}

fn render_response_item(payload: &Value) -> Option<String> {
    match payload.get("type").and_then(Value::as_str)? {
        "message" => render_message(payload),
        "agent_message" => Some(render_agent_message(payload)),
        "reasoning" => Some(render_reasoning(payload)),
        "function_call" => Some(call_line(
            &tool_name(payload),
            arguments_summary(payload.get("arguments")),
        )),
        "custom_tool_call" => Some(call_line(
            &tool_name(payload),
            payload
                .get("input")
                .and_then(Value::as_str)
                .and_then(single_line),
        )),
        "function_call_output" | "custom_tool_call_output" => render_output(payload.get("output")),
        "local_shell_call" => Some(call_line(
            "shell",
            payload
                .get("action")
                .and_then(|action| action.get("command"))
                .and_then(words),
        )),
        "web_search_call" => Some(call_line(
            "web_search",
            payload.get("action").and_then(|action| {
                ["query", "queries", "url", "pattern"]
                    .iter()
                    .find_map(|key| action.get(*key).and_then(words))
            }),
        )),
        "image_generation_call" => Some(call_line(
            "image_generation",
            payload
                .get("revised_prompt")
                .and_then(Value::as_str)
                .and_then(single_line),
        )),
        other => Some(type_line(other)),
    }
}

/// user / assistant 的对话正文；developer / system 是给模型的指令，不进转写。
fn render_message(payload: &Value) -> Option<String> {
    let role = payload.get("role").and_then(Value::as_str);
    if !matches!(role, Some("user" | "assistant")) {
        return None;
    }
    let text = content_text(payload.get("content"));
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if role == Some("assistant") {
        return Some(clean_text(text, MESSAGE_CHARS, true));
    }
    // `<environment_context>…</environment_context>` 这类内部包装只留标签名。
    if let Some(tag) = wrapper_tag(text) {
        return Some(format!("[{tag}]"));
    }
    Some(format!("user: {}", clean_text(text, MESSAGE_CHARS, true)))
}

/// agent 之间的协作消息：发送方 + 明文部分，负载加密时注明。
fn render_agent_message(payload: &Value) -> String {
    let mut line = match payload
        .get("author")
        .and_then(Value::as_str)
        .map(collapse)
        .filter(|author| !author.is_empty())
    {
        Some(author) => format!("[message from {}]", clip_chars(&author, NAME_CHARS)),
        None => "[message]".to_string(),
    };
    let mut parts = Vec::new();
    let mut encrypted = false;
    for block in payload
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match block.get("type").and_then(Value::as_str) {
            Some("input_text" | "output_text" | "text") => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    let text = collapse(text);
                    if !text.is_empty() {
                        parts.push(text);
                    }
                }
            }
            Some("encrypted_content") => encrypted = true,
            _ => {}
        }
    }
    if !parts.is_empty() {
        line.push(' ');
        line.push_str(&clip_chars(&parts.join(" "), MESSAGE_CHARS));
    }
    if encrypted {
        line.push_str(" (encrypted)");
    }
    line
}

/// 推理：有摘要就带上，只有加密内容时只留标记。
fn render_reasoning(payload: &Value) -> String {
    let summary: Vec<String> = payload
        .get("summary")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .map(collapse)
        .filter(|text| !text.is_empty())
        .collect();
    if summary.is_empty() {
        "(thinking)".to_string()
    } else {
        format!(
            "(thinking) {}",
            clip_chars(&summary.join(" "), MESSAGE_CHARS)
        )
    }
}

fn render_event(payload: &Value) -> Option<String> {
    match payload.get("type").and_then(Value::as_str)? {
        "task_complete" => Some(
            match payload.get("error").filter(|error| !error.is_null()) {
                Some(error) => tagged_line("[task failed]", Some(&error_message(error))),
                None => tagged_line(
                    "[task complete]",
                    payload.get("last_agent_message").and_then(Value::as_str),
                ),
            },
        ),
        "turn_aborted" => Some(tagged_line(
            "[turn aborted]",
            payload.get("reason").and_then(Value::as_str),
        )),
        "error" | "stream_error" => Some(tagged_line(
            "[error]",
            payload.get("message").and_then(Value::as_str),
        )),
        "context_compacted" => Some("[context compacted]".to_string()),
        "item_completed" => render_plan(payload.get("item")?),
        // 遥测、回合起始与设置、以及和 response_item 重复的镜像事件：不进转写。
        "token_count"
        | "task_started"
        | "thread_settings_applied"
        | "session_configured"
        | "item_started"
        | "user_message"
        | "agent_message"
        | "agent_reasoning"
        | "agent_reasoning_raw_content"
        | "agent_reasoning_section_break" => None,
        other => Some(type_line(other)),
    }
}

/// 计划只以 `item_completed` 落盘；其余完成条目都是 response_item 的镜像。
fn render_plan(item: &Value) -> Option<String> {
    if item.get("type").and_then(Value::as_str) != Some("Plan") {
        return None;
    }
    let mut out = "[plan]".to_string();
    push_output_lines(
        &mut out,
        item.get("text").and_then(Value::as_str).unwrap_or_default(),
    );
    Some(out)
}

/// 工具输出：去掉开头空行后的前几行缩进显示，其余只计数；没有内容时不出行。
fn render_output(output: Option<&Value>) -> Option<String> {
    let mut text = String::new();
    let mut append = |part: &str| {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(part);
    };
    match output? {
        Value::String(output) => append(output),
        Value::Array(blocks) => {
            for block in blocks {
                match block.get("type").and_then(Value::as_str) {
                    Some("input_text" | "output_text" | "text") => {
                        if let Some(output) = block.get("text").and_then(Value::as_str) {
                            append(output);
                        }
                    }
                    Some("input_image" | "image") => append("[image]"),
                    _ => {}
                }
            }
        }
        // 旧版的 `{content, success}` 形态。
        Value::Object(fields) => {
            if let Some(output) = fields.get("content").and_then(Value::as_str) {
                append(output);
            }
        }
        _ => {}
    }
    let mut out = String::new();
    push_output_lines(&mut out, &text);
    // 去掉 push_output_lines 补在最前面的换行。
    out.strip_prefix('\n').map(str::to_string)
}

/// 把输出的前 [`OUTPUT_LINES`] 行（去掉开头空行、ANSI 序列与控制字符，逐行截断）
/// 以缩进行接在 `out` 后面，其余只计数；末尾空行不计，中间的空行不补缩进。
fn push_output_lines(out: &mut String, text: &str) {
    let mut shown: Vec<String> = Vec::new();
    let mut hidden = 0usize;
    let mut trailing_blank = 0usize;
    for raw in text.lines() {
        if shown.len() < OUTPUT_LINES {
            let line = clean_text(raw, OUTPUT_LINE_CHARS, false);
            let blank = line.trim().is_empty();
            if !(shown.is_empty() && blank) {
                shown.push(if blank { String::new() } else { line });
            }
            continue;
        }
        // 显示满之后只数行，不再逐字符清洗。
        if raw.trim().is_empty() {
            trailing_blank += 1;
        } else {
            hidden += trailing_blank + 1;
            trailing_blank = 0;
        }
    }
    if hidden == 0 {
        while shown.last().is_some_and(String::is_empty) {
            shown.pop();
        }
    } else {
        shown.push(format!("… +{hidden} lines"));
    }
    for line in shown {
        out.push('\n');
        if !line.is_empty() {
            out.push_str("    ");
            out.push_str(&line);
        }
    }
}

/// `[名字] 摘要`；没有摘要时只留名字。
fn call_line(name: &str, summary: Option<String>) -> String {
    match summary {
        Some(summary) => format!("[{name}] {summary}"),
        None => format!("[{name}]"),
    }
}

/// `标签 单行摘要`；没有内容时只留标签。
fn tagged_line(tag: &str, text: Option<&str>) -> String {
    match text.and_then(single_line) {
        Some(summary) => format!("{tag} {summary}"),
        None => tag.to_string(),
    }
}

fn tool_name(payload: &Value) -> String {
    payload
        .get("name")
        .and_then(Value::as_str)
        .map(collapse)
        .filter(|name| !name.is_empty())
        .map(|name| clip_chars(&name, NAME_CHARS))
        .unwrap_or_else(|| "tool".to_string())
}

fn type_line(kind: &str) -> String {
    let name = collapse(kind);
    if name.is_empty() {
        return "[record]".to_string();
    }
    format!("[{}]", clip_chars(&name, NAME_CHARS))
}

/// `function_call` 的参数摘要：参数（JSON 字符串）里按 [`CALL_ARGUMENT_KEYS`] 取第一个
/// 有值的键；都没有就给截断的紧凑 JSON，不是 JSON 就当自由文本。
fn arguments_summary(arguments: Option<&Value>) -> Option<String> {
    let parsed = match arguments? {
        Value::String(text) => match serde_json::from_str::<Value>(text) {
            Ok(parsed) => parsed,
            Err(_) => return single_line(text),
        },
        other => other.clone(),
    };
    let Value::Object(fields) = &parsed else {
        return words(&parsed);
    };
    if let Some(summary) = CALL_ARGUMENT_KEYS
        .iter()
        .find_map(|key| fields.get(*key).and_then(words))
    {
        return Some(summary);
    }
    if fields.is_empty() {
        return None;
    }
    serde_json::to_string(&parsed)
        .ok()
        .and_then(|json| single_line(&json))
}

/// 字符串，或字符串数组（命令行参数）用空格连起来，折成单行摘要。
fn words(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => single_line(text),
        Value::Array(items) => {
            let joined: Vec<&str> = items.iter().filter_map(Value::as_str).collect();
            single_line(&joined.join(" "))
        }
        _ => None,
    }
}

/// 折成单行并截到 [`SUMMARY_CHARS`]；空的返回 `None`。
fn single_line(text: &str) -> Option<String> {
    let line = collapse(text);
    (!line.is_empty()).then(|| clip_chars(&line, SUMMARY_CHARS))
}

/// 块数组里的文本块用换行连起来，图片留占位；字符串原样。
fn content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| match block.get("type").and_then(Value::as_str) {
                Some("input_text" | "output_text" | "text") => block
                    .get("text")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                Some("input_image" | "image") => Some("[image]".to_string()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// `<tag>…</tag>` 形式的内部包装（`<environment_context>`、`<user_instructions>` 等），
/// 返回标签名。
fn wrapper_tag(text: &str) -> Option<&str> {
    let rest = text.strip_prefix('<')?;
    let name = &rest[..rest.find('>')?];
    let plausible = !name.is_empty()
        && name.len() <= NAME_CHARS
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-');
    (plausible && text.ends_with(&format!("</{name}>"))).then_some(name)
}

/// 折叠换行与连续空白，保证摘要是单行。
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

/// 取前 `max_chars` 个字符，超出以省略号结尾。
fn clip_chars(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let mut out: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        out.push('…');
    }
    out
}

/// 去掉 ANSI 转义序列与控制字符（制表符留给客户端展开；`keep_newlines` 时保留换行
/// 并去掉行尾空白），取前 `max_chars` 个字符，超出以省略号结尾。逐字符处理，读到
/// 上限就停。
fn clean_text(text: &str, max_chars: usize, keep_newlines: bool) -> String {
    let mut out = String::new();
    let mut kept = 0usize;
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            // CSI（ESC [ … 终止字节 0x40–0x7e）整段丢；其余 ESC 连同下一个字符丢。
            if chars.peek() == Some(&'[') {
                chars.next();
                for next in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&next) {
                        break;
                    }
                }
            } else {
                chars.next();
            }
            continue;
        }
        let keep = ch == '\t' || (keep_newlines && ch == '\n');
        if ch.is_control() && !keep {
            continue;
        }
        if kept == max_chars {
            out.push('…');
            break;
        }
        if ch == '\n' {
            out.truncate(out.trim_end_matches([' ', '\t']).len());
        }
        out.push(ch);
        kept += 1;
    }
    out.truncate(out.trim_end().len());
    out
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
    /// 0.156.1 结构的根线程与它的子线程（昵称 Noor，任务 /root/print_probe）。
    const PROBE_ROOT: &str = "01a0c6f0-0000-7000-8000-0000000000c1";
    const PROBE_CHILD: &str = "01a0c6f0-2710-7000-8000-0000000000c2";

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
        let expected = Codex
            .read(
                &context(&fixture_home(), Some(&session)),
                CHILD_A,
                None,
                MAX_READ_BYTES,
            )
            .expect("默认布局可读");
        let page = Codex
            .read(&relocated, CHILD_A, None, MAX_READ_BYTES)
            .expect("配置目录下的 rollout 可读");
        assert!(!page.text.is_empty());
        assert_eq!(page.text, expected.text);

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
    fn read_pages_through_a_child_transcript_by_byte_cursor() {
        let home = fixture_home();
        let session = AgentSessionRef::id(PROBE_ROOT).expect("合法 id");
        let cx = context(&home, Some(&session));
        let path = home
            .join(".codex/sessions/2026/09/22")
            .join(format!("rollout-2026-09-22T10-30-05-{PROBE_CHILD}.jsonl"));
        let length = fs::metadata(&path).expect("夹具可读").len().to_string();

        let whole = Codex
            .read(&cx, PROBE_CHILD, None, MAX_READ_BYTES)
            .expect("整页可读");
        assert!(whole.eof);
        assert!(!whole.truncated);
        assert_eq!(whole.next_cursor.as_deref(), Some(length.as_str()));

        // 按最小页分页：页尾总在记录边界上，一条都不重不漏，拼起来等于整页。
        let mut cursor: Option<String> = None;
        let mut pages = 0;
        let mut joined = String::new();
        loop {
            let chunk = Codex
                .read(&cx, PROBE_CHILD, cursor.as_deref(), MIN_READ_BYTES)
                .expect("分页可读");
            assert!(!chunk.truncated);
            assert!(chunk.text.len() <= MIN_READ_BYTES);
            assert!(chunk.text.is_empty() || chunk.text.ends_with('\n'));
            joined.push_str(&chunk.text);
            pages += 1;
            if chunk.eof {
                break;
            }
            cursor = chunk.next_cursor;
            assert!(pages < 1000, "分页不收敛");
        }
        assert!(pages > 2, "应当跨页，实际 {pages} 页");
        assert_eq!(joined, whole.text);

        // 过小的页按最小页算。
        let tiny = Codex.read(&cx, PROBE_CHILD, None, 16).expect("小页可读");
        let minimum = Codex
            .read(&cx, PROBE_CHILD, None, MIN_READ_BYTES)
            .expect("最小页可读");
        assert_eq!(tiny.text, minimum.text);

        // 游标越过文件末尾（文件被截断或跟随时）：空页、eof、游标夹回长度。
        let beyond = Codex
            .read(&cx, PROBE_CHILD, Some("999999999"), 300)
            .expect("越界游标不报错");
        assert!(beyond.text.is_empty());
        assert!(beyond.eof);
        assert_eq!(beyond.next_cursor.as_deref(), Some(length.as_str()));

        // max_bytes = 0 用默认页。
        let default_page = Codex.read(&cx, PROBE_CHILD, None, 0).expect("默认页可读");
        assert_eq!(default_page.text, whole.text);
    }

    #[test]
    fn page_limits_bound_oversized_records_and_long_hidden_runs() {
        let home = unique_temp_home("page-limits");
        let day = home.join(".codex/sessions/2026/09/22");
        fs::create_dir_all(&day).expect("临时目录可建");
        let path = day.join(format!("rollout-2026-09-22T10-01-00-{CHILD_A}.jsonl"));
        let message = |text: &str| {
            format!(
                r#"{{"type":"response_item","payload":{{"type":"message","role":"assistant","content":[{{"type":"output_text","text":"{text}"}}]}}}}"#
            ) + "\n"
        };
        let hidden = r#"{"type":"event_msg","payload":{"type":"token_count"}}"#.to_string() + "\n";
        let big = message(&"a".repeat(600));
        let small = message("small");
        let limits = |budget, scan_bytes, record_bytes| PageLimits {
            budget,
            scan_bytes,
            record_bytes,
        };

        // 单条渲染结果比整页还大：截在页内并标记，游标越过这一条。
        fs::write(&path, format!("{big}{small}")).expect("写临时 rollout");
        let page = render_page(&path, 0, limits(256, u64::MAX, u64::MAX)).expect("可读");
        assert!(page.truncated);
        assert!(page.text.ends_with(" …\n"), "{}", page.text);
        assert!(page.text.len() <= 256);
        assert_eq!(
            page.next_cursor.as_deref(),
            Some(big.len().to_string().as_str())
        );
        assert!(!page.eof);
        let rest =
            render_page(&path, big.len() as u64, limits(256, u64::MAX, u64::MAX)).expect("可读");
        assert_eq!(rest.text, "small\n");
        assert!(rest.eof);

        // 超过单条上限的记录不读进内存：整条跳过，留一行占位。
        let cap = 150;
        assert!(small.len() < cap && big.len() > cap);
        let page = render_page(&path, 0, limits(4096, u64::MAX, cap as u64)).expect("可读");
        assert_eq!(page.text, "[oversized record]\nsmall\n");
        assert!(page.eof);

        // 还没写完的超长记录不消费，游标停在它的行首。
        fs::write(&path, format!("{small}{}", "b".repeat(2 * cap))).expect("写半截超长行");
        let page = render_page(&path, 0, limits(4096, u64::MAX, cap as u64)).expect("可读");
        assert_eq!(page.text, "small\n");
        assert!(page.eof);
        assert_eq!(
            page.next_cursor.as_deref(),
            Some(small.len().to_string().as_str())
        );

        // 一页扫的原始字节有上限：隐藏记录再多也先交页，游标记在扫到的位置。
        fs::write(&path, format!("{}{small}", hidden.repeat(10))).expect("写隐藏记录");
        let cap = (hidden.len() * 3) as u64;
        let page = render_page(&path, 0, limits(4096, cap, u64::MAX)).expect("可读");
        assert!(page.text.is_empty());
        assert!(!page.eof);
        assert_eq!(page.next_cursor.as_deref(), Some(cap.to_string().as_str()));

        let _ = fs::remove_dir_all(&home);
    }

    /// 冒烟 M6（codex-13 / 17）：活动窗口右列是原始 rollout JSON，探针输出埋在
    /// JSON 字段里，token_count / rate_limits 这类遥测整条倒出来。
    #[test]
    fn read_renders_a_readable_transcript_instead_of_raw_rollout_json() {
        let home = fixture_home();
        let session = AgentSessionRef::id(PROBE_ROOT).expect("合法 id");
        let cx = context(&home, Some(&session));
        let page = Codex.read(&cx, PROBE_CHILD, None, 64 * 1024).expect("可读");
        assert_eq!(page.format, AgentActivityContentFormat::Text);
        assert_eq!(
            page.text,
            "[message from /root] Message Type: NEW_TASK Task name: /root/print_probe \
             Sender: /root Payload: (encrypted)\n\
             (thinking)\n\
             [exec] const result = await tools.exec_command({cmd:\"echo fake-probe\"}); \
             text(result.output);\n\
             \x20   Script completed\n\
             \x20   Wall time 0.1 seconds\n\
             \x20   Output:\n\
             \x20   fake-probe\n\
             [shell] ls -la\n\
             \x20   total 0\n\
             \x20   drwx fake .\n\
             [wait] {\"cell_id\":\"7\",\"yield_time_ms\":500}\n\
             \x20   still waiting\n\
             fake-probe\n\
             [plan]\n\
             \x20   1. print the probe\n\
             \x20   2. report back\n\
             [mystery_event]\n\
             [future_record]\n\
             [hologram_call]\n\
             [task complete] fake-probe\n\
             [unparsable line]\n"
        );
        // 继承的父历史前缀、开发者指令与遥测都不进转写。
        assert!(!page.text.contains("inherited"), "{}", page.text);
        assert!(!page.text.contains("developer notes"), "{}", page.text);
        assert!(!page.text.contains("rate_limits"), "{}", page.text);
        assert!(page.eof);
        assert!(!page.truncated);
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
        let head = "{\"type\":\"session_meta\",\"payload\":{\"id\":\"x\"}}\n\
                    {\"type\":\"turn_context\"}\n\
                    {\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\
                    \"content\":[{\"type\":\"input_text\",\"text\":\"go\"}]}}\n";
        let partial = "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\
                       \"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"hal";
        fs::write(&path, format!("{head}{partial}")).expect("临时文件可写");
        let session = AgentSessionRef::id(ROOT).expect("合法 id");
        let cx = context(&home, Some(&session));

        // 半截行不消费：游标停在它的行首，eof 表示暂时读完。簿记记录照样推进游标。
        let first = Codex.read(&cx, CHILD_A, None, 4096).expect("可读");
        assert_eq!(first.text, "user: go\n");
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

        // 这一行写完并追加新行后，从同一游标续读拿到完整内容，不重不漏。
        let rest =
            "f done\"}]}}\n{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\"}}\n";
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
        assert_eq!(followed.text, "half done\n[task complete]\n");
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
    fn record_rendering_stays_total_on_odd_input() {
        let render = |line: &str| render_record(line, None);
        let item = |payload: &str| format!(r#"{{"type":"response_item","payload":{payload}}}"#);
        let event = |payload: &str| format!(r#"{{"type":"event_msg","payload":{payload}}}"#);

        // 内部包装只留标签名；developer 指令与空消息不进转写。
        assert_eq!(
            render(&item(
                r#"{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>\n  <cwd>/w</cwd>\n</environment_context>"}]}"#
            ))
            .as_deref(),
            Some("[environment_context]")
        );
        assert_eq!(
            render(&item(
                r#"{"type":"message","role":"developer","content":[{"type":"input_text","text":"rules"}]}"#
            )),
            None
        );
        assert_eq!(
            render(&item(
                r#"{"type":"message","role":"assistant","content":[]}"#
            )),
            None
        );
        // 多行正文保留换行、去掉 ANSI 与行尾空白；图片留占位。
        assert_eq!(
            render(&item(
                r#"{"type":"message","role":"user","content":[{"type":"input_text","text":"line one  \r\n\u001b[1mline two\u001b[0m"},{"type":"input_image","image_url":"x"}]}"#
            ))
            .as_deref(),
            Some("user: line one\nline two\n[image]")
        );
        // 推理摘要、协作消息缺字段、各类工具调用与非 JSON 参数。
        assert_eq!(
            render(&item(
                r#"{"type":"reasoning","summary":[{"type":"summary_text","text":"weigh  options"}]}"#
            ))
            .as_deref(),
            Some("(thinking) weigh options")
        );
        assert_eq!(
            render(&item(r#"{"type":"agent_message","content":"odd"}"#)).as_deref(),
            Some("[message]")
        );
        assert_eq!(
            render(&item(
                r#"{"type":"local_shell_call","action":{"type":"exec","command":["git","status"]}}"#
            ))
            .as_deref(),
            Some("[shell] git status")
        );
        assert_eq!(
            render(&item(
                r#"{"type":"web_search_call","action":{"type":"search","query":"rust  bufread"}}"#
            ))
            .as_deref(),
            Some("[web_search] rust bufread")
        );
        assert_eq!(
            render(&item(
                r#"{"type":"function_call","name":"apply","arguments":"not json at all"}"#
            ))
            .as_deref(),
            Some("[apply] not json at all")
        );
        assert_eq!(
            render(&item(r#"{"type":"function_call","arguments":"{}"}"#)).as_deref(),
            Some("[tool]")
        );
        // 输出：旧版 {content, success} 形态、图片块、空输出。
        assert_eq!(
            render(&item(
                r#"{"type":"function_call_output","output":{"content":"ok\n","success":true}}"#
            ))
            .as_deref(),
            Some("    ok")
        );
        assert_eq!(
            render(&item(
                r#"{"type":"custom_tool_call_output","output":[{"type":"input_image","image_url":"x"}]}"#
            ))
            .as_deref(),
            Some("    [image]")
        );
        assert_eq!(
            render(&item(r#"{"type":"function_call_output","output":"\n\n"}"#)),
            None
        );
        // 生命周期事件与错误。
        assert_eq!(
            render(&event(
                r#"{"type":"task_complete","error":{"message":"model refused"},"last_agent_message":null}"#
            ))
            .as_deref(),
            Some("[task failed] model refused")
        );
        assert_eq!(
            render(&event(
                r#"{"type":"task_complete","last_agent_message":null}"#
            ))
            .as_deref(),
            Some("[task complete]")
        );
        assert_eq!(
            render(&event(r#"{"type":"turn_aborted","reason":"interrupted"}"#)).as_deref(),
            Some("[turn aborted] interrupted")
        );
        assert_eq!(
            render(&event(r#"{"type":"error","message":"stream closed"}"#)).as_deref(),
            Some("[error] stream closed")
        );
        // 镜像事件与非计划的完成条目不进转写。
        assert_eq!(
            render(&event(r#"{"type":"agent_message","message":"dup"}"#)),
            None
        );
        assert_eq!(
            render(&event(
                r#"{"type":"item_completed","item":{"type":"AgentMessage","content":[]}}"#
            )),
            None
        );
        // 没有类型、空类型名与空行。
        assert_eq!(render(r#"{"payload":{}}"#), None);
        assert_eq!(render(r#"{"type":"   "}"#).as_deref(), Some("[record]"));
        assert_eq!(render("   "), None);
        // 继承前缀按 ordinal 跳过；没有 ordinal 的不跳。
        let prefixed = r#"{"ordinal":3,"type":"response_item","payload":{"type":"message","role":"user","content":"old"}}"#;
        assert_eq!(render_record(prefixed, Some(4)), None);
        assert_eq!(
            render_record(prefixed, Some(3)).as_deref(),
            Some("user: old")
        );
        assert_eq!(wrapper_tag("<a b>x</a b>"), None);
        assert_eq!(wrapper_tag("<ctx>open only"), None);
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
