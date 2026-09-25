//! pi 的活动来源适配器：扩展类工具的执行（含官方 `subagent` 范例派生的子
//! agent）。树不落任何文件，由 herdr 自带扩展写成快照、经
//! `pane.report_agent_activity` 的 `hint` 上报；本适配器只解析该快照。
//!
//! # 本机取证（pi 0.87.0，`@earendil-works/pi-coding-agent`）
//!
//! 只读查看了安装目录里的类型定义（`dist/core/extensions/types.d.ts`、
//! `dist/core/source-info.d.ts`、`dist/core/tools/index.d.ts`）、编译产物
//! （`dist/core/agent-session.js`、`pi-agent-core/dist/agent-loop.js`）、文档
//! 与范例。本机从未运行过 pi（没有 `~/.pi`），没有读取任何会话内容。
//!
//! - pi 不内置子 agent / 计划 / 待办 / 后台任务，全部由扩展提供。官方范例
//!   `examples/extensions/subagent/index.ts` 以 `pi --mode json -p --no-session`
//!   派生子进程 → 子代理不写会话文件；会话 JSONL
//!   （`~/.pi/agent/sessions/--<cwd>--/<时间>_<会话 id>.jsonl`，可被
//!   `PI_CODING_AGENT_DIR` / `PI_CODING_AGENT_SESSION_DIR` 改写）里的
//!   `id` / `parentId` 是对话分支树，不是子 agent 树。
//! - 扩展事件：`tool_execution_start {toolCallId, toolName, args}`、
//!   `tool_execution_update {toolCallId, toolName, args, partialResult}`、
//!   `tool_execution_end {toolCallId, toolName, result, isError}`；
//!   `partialResult` / `result` 形如 `{content: [{type: "text", text}], details}`。
//!   pi 在工具循环里 await 扩展处理函数，扩展不得在其中等 socket。
//! - 事件上的 `isError` 只在 `execute` 抛异常（或 `tool_result` 钩子改写）时为
//!   真；官方 subagent 范例把失败写成返回值里的 `isError: true`，事件上仍是
//!   `false` → 扩展两处都看。
//! - `pi.getAllTools()` 的 `sourceInfo.source`：内置工具为 `"builtin"`（路径
//!   `<builtin:名>`），SDK 工具为 `"sdk"`，扩展工具为扩展来源；内置工具名
//!   `allToolNames` = `read` / `bash` / `powershell` / `edit` / `write` / `grep` /
//!   `find` / `ls`。扩展可以同名覆盖内置工具，扩展侧按名字排除它们。
//! - 官方 subagent 范例的 `details` = `{mode, agentScope, projectAgentsDir,
//!   results[]}`，`mode` ∈ `single` / `parallel` / `chain`；`results[]` 元素键：
//!   `agent`、`agentSource`（`user` / `project` / `unknown`）、`task`、`exitCode`、
//!   `messages`、`stderr`、`usage`、`model`、`stopReason`、`errorMessage`、
//!   `step`（仅 chain）。`exitCode` 只有并行占位的 `-1` 在运行期可信：流式期间
//!   恒为 `0`，进程退出后才是真实退出码 → 扩展改看最后一条 assistant 消息的
//!   `stopReason`（pi-ai `StopReason`：`pending` / `stop` / `length` / `toolUse` /
//!   `error` / `aborted` / `deferred`）。
//! - `ctx.mode` ∈ `tui` / `rpc` / `json` / `print`；herdr 扩展只在 `tui` 上报，
//!   `json` 子进程即使加载了扩展也不上报 → 子节点一律由父进程侧写。
//!
//! # hint 快照格式（`herdr.activity.snapshot`，version 1）
//!
//! 写入方是 `src/integration/assets/pi/herdr-agent-state.ts`，真实样本见
//! `tests/fixtures/agent-activity/pi/snapshot-extension.json`（bun 契约测试与本
//! 文件的单测共用）。之后的版本只增字段，不改语义：
//!
//! ```text
//! { "type": "herdr.activity.snapshot", "version": 1,
//!   "session_path"?: string, "session_id"?: string,
//!   "nodes": [{ "id", "parent_id"?, "kind", "label", "status", "agent_type"?,
//!               "summary"?, "started_at_ms"?, "ended_at_ms"?,
//!               "output"?: { "text", "start", "format" } }] }
//! ```
//!
//! - 根节点 `id` = `toolCallId`；并行 / 链式 subagent 的每个委派 agent 是子节点
//!   `<toolCallId>/<results 下标>`；扩展写出的 `kind` 只有 `subagent` / `task`，
//!   `status` 只有 `pending` / `running` / `done` / `failed`，未知值落 `Unknown`。
//! - `output` 是该节点流式 update 文本的追加日志的尾部：`start` 是 `text` 在日志
//!   里的 UTF-8 字节偏移（头部被丢掉的字节数），`format` ∈ `markdown` / `text`。
//!   `read_from_hint` 的游标是日志里的绝对字节偏移（十进制字符串），因此跨快照
//!   有效；游标落到已丢弃的头部时从保留窗口开头读并置 `truncated`。
//! - `session_path` / `session_id` 与 pane 当前会话引用同类且不同 → 快照属于
//!   上一个会话，树按空处理。
//!
//! # 接入点
//!
//! server 缓存该 pane 最近到达的一份 hint（超过 1 MiB 的不缓存，保留上一份）：
//! 按到达先后覆盖，不比较 `seq`，也不先解析（`AgentActivityStore::store_hint`），
//! 刷新时经 `SourceContext::latest_hint` 交给适配器。runtime 对 pi 不走 trait
//! （trait 的 `discover` / `read` 如实回 `Unsupported`），直接调
//! [`discover_from_hint`] / [`read_from_hint`]；还没有 hint 时发现回空树、读取回
//! `Unavailable`（`agent_activity::discover_nodes` / `read_node`）。解析失败时由
//! 调用方保留上一份树。

use std::collections::{HashMap, HashSet};

use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

use super::{ActivitySource, ContentChunk, SourceContext, SourceError};
use crate::agent_resume::{AgentSessionRef, AgentSessionRefKind};
use crate::api::schema::AgentActivityContentFormat;
use crate::api::schema::AgentActivityNode;

/// 快照的类型标记，与扩展侧 `ACTIVITY_HINT_TYPE` 一致。
const HINT_TYPE: &str = "herdr.activity.snapshot";
/// 本适配器理解的版本；更高版本按「只增字段」约定尽力解析。
const HINT_VERSION: u64 = 1;
/// 单个 hint 的字节上限，与 API 单请求上限一致。
const MAX_HINT_BYTES: usize = 1024 * 1024;
/// 一次快照最多取的节点数（扩展侧上限 8 个根 × 9，远小于此）。
const MAX_NODES: usize = 256;
const MAX_ID_CHARS: usize = 256;
const MAX_LABEL_CHARS: usize = 200;
const MAX_TEXT_FIELD_CHARS: usize = 240;

pub(super) struct Pi;

impl ActivitySource for Pi {
    fn id(&self) -> &'static str {
        "pi"
    }

    /// pi 的树只存在于扩展上报的 hint 里；runtime 直接调 [`discover_from_hint`]
    /// （见模块文档「接入点」），不经此入口。
    fn discover(&self, _cx: &SourceContext<'_>) -> Result<Vec<AgentActivityNode>, SourceError> {
        Err(SourceError::Unsupported)
    }

    /// 同 `discover`：内容随快照上报，入口是 [`read_from_hint`]。
    fn read(
        &self,
        _cx: &SourceContext<'_>,
        _node_id: &str,
        _cursor: Option<&str>,
        _max_bytes: usize,
    ) -> Result<ContentChunk, SourceError> {
        Err(SourceError::Unsupported)
    }
}

/// 由一次 hint 快照发现活动节点，按父先子后排列。快照属于别的会话时回空树；
/// hint 不是本格式或不是 JSON 时回 `Malformed`（调用方保留上一份树）。
pub(super) fn discover_from_hint(
    cx: &SourceContext<'_>,
    hint: &str,
) -> Result<Vec<AgentActivityNode>, SourceError> {
    let snapshot = parse_snapshot(hint)?;
    if !snapshot.belongs_to(cx.session) {
        return Ok(Vec::new());
    }
    Ok(snapshot.nodes.into_iter().map(|entry| entry.node).collect())
}

/// 按字节游标读一个节点的输出日志。节点不在快照里（已被扩展裁掉）或快照属于
/// 别的会话时回 `Unavailable`。
pub(super) fn read_from_hint(
    cx: &SourceContext<'_>,
    hint: &str,
    node_id: &str,
    cursor: Option<&str>,
    max_bytes: usize,
) -> Result<ContentChunk, SourceError> {
    let snapshot = parse_snapshot(hint)?;
    if !snapshot.belongs_to(cx.session) {
        return Err(SourceError::Unavailable);
    }
    let entry = snapshot
        .nodes
        .iter()
        .find(|entry| entry.node.id == node_id)
        .ok_or(SourceError::Unavailable)?;
    let requested = cursor.map(parse_cursor).transpose()?;
    Ok(match &entry.output {
        Some(output) => read_output(output, requested, max_bytes),
        None => ContentChunk {
            format: AgentActivityContentFormat::Text,
            text: String::new(),
            next_cursor: None,
            eof: true,
            truncated: false,
        },
    })
}

struct Snapshot {
    session_path: Option<String>,
    session_id: Option<String>,
    nodes: Vec<SnapshotNode>,
}

impl Snapshot {
    /// 快照没带同类会话引用、或 pane 还没有会话引用时无法比对，按属于处理。
    fn belongs_to(&self, session: Option<&AgentSessionRef>) -> bool {
        let Some(session) = session else {
            return true;
        };
        let reported = match session.kind {
            AgentSessionRefKind::Path => self.session_path.as_deref(),
            AgentSessionRefKind::Id => self.session_id.as_deref(),
        };
        reported.is_none_or(|value| value == session.value)
    }
}

struct SnapshotNode {
    node: AgentActivityNode,
    output: Option<NodeOutput>,
}

struct NodeOutput {
    text: String,
    /// `text` 在该节点输出日志里的起始字节偏移。
    start: u64,
    format: AgentActivityContentFormat,
}

fn parse_snapshot(hint: &str) -> Result<Snapshot, SourceError> {
    if hint.len() > MAX_HINT_BYTES {
        return Err(SourceError::Malformed(format!(
            "activity snapshot exceeds {MAX_HINT_BYTES} bytes"
        )));
    }
    let value: Value = serde_json::from_str(hint)
        .map_err(|error| SourceError::Malformed(format!("activity hint is not JSON: {error}")))?;
    let Value::Object(envelope) = value else {
        return Err(SourceError::Malformed(
            "activity hint is not a JSON object".into(),
        ));
    };
    if envelope.get("type").and_then(Value::as_str) != Some(HINT_TYPE) {
        return Err(SourceError::Malformed(format!(
            "activity hint is not a {HINT_TYPE}"
        )));
    }
    if envelope
        .get("version")
        .and_then(Value::as_u64)
        .is_none_or(|version| version < HINT_VERSION)
    {
        return Err(SourceError::Malformed(
            "unsupported activity snapshot version".into(),
        ));
    }

    let mut seen = HashSet::new();
    let mut nodes = Vec::new();
    for raw in envelope
        .get("nodes")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        if nodes.len() == MAX_NODES {
            break;
        }
        // 单个坏节点只丢它自己；同 id 以先出现的为准。
        let Some(entry) = parse_node(raw) else {
            continue;
        };
        if seen.insert(entry.node.id.clone()) {
            nodes.push(entry);
        }
    }

    Ok(Snapshot {
        session_path: session_field(&envelope, "session_path"),
        session_id: session_field(&envelope, "session_id"),
        nodes: into_tree_order(nodes),
    })
}

/// 会话引用原样比对，不做任何清洗（路径里可以有连续空格）。
fn session_field(envelope: &Map<String, Value>, key: &str) -> Option<String> {
    envelope
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn parse_node(raw: &Value) -> Option<SnapshotNode> {
    let object = raw.as_object()?;
    let id = identifier(object.get("id"))?;
    let output = object
        .get("output")
        .and_then(Value::as_object)
        .map(parse_output);
    let node = AgentActivityNode {
        label: clean_line(object.get("label"), MAX_LABEL_CHARS).unwrap_or_else(|| id.clone()),
        kind: enum_field(object, "kind"),
        status: enum_field(object, "status"),
        parent_id: identifier(object.get("parent_id")),
        agent_type: clean_line(object.get("agent_type"), MAX_TEXT_FIELD_CHARS),
        content_ref: output.as_ref().map(|_| id.clone()),
        summary: clean_line(object.get("summary"), MAX_TEXT_FIELD_CHARS),
        started_at_ms: object.get("started_at_ms").and_then(non_negative_integer),
        ended_at_ms: object.get("ended_at_ms").and_then(non_negative_integer),
        id,
    };
    Some(SnapshotNode { node, output })
}

fn parse_output(output: &Map<String, Value>) -> NodeOutput {
    NodeOutput {
        text: output
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        start: output
            .get("start")
            .and_then(non_negative_integer)
            .unwrap_or(0),
        // 缺省是纯文本；给了但不认识的格式落 `Unknown`。
        format: output
            .get("format")
            .map_or(AgentActivityContentFormat::Text, enum_value),
    }
}

/// 非字符串或不认识的取值都落该枚举的默认值（`Unknown`）。
fn enum_field<T: DeserializeOwned + Default>(object: &Map<String, Value>, key: &str) -> T {
    object.get(key).map_or_else(T::default, enum_value)
}

fn enum_value<T: DeserializeOwned + Default>(value: &Value) -> T {
    match value {
        Value::String(_) => serde_json::from_value(value.clone()).unwrap_or_default(),
        _ => T::default(),
    }
}

fn identifier(value: Option<&Value>) -> Option<String> {
    let text = value?.as_str()?.trim();
    (!text.is_empty()
        && text.chars().count() <= MAX_ID_CHARS
        && !text.chars().any(char::is_control))
    .then(|| text.to_owned())
}

/// 毫秒时间戳与字节偏移：接受非负整数与非负有限浮点（截断）。
fn non_negative_integer(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| {
        value
            .as_f64()
            .filter(|number| number.is_finite() && *number >= 0.0)
            .map(|number| number as u64)
    })
}

/// 单行展示文本：控制字符与连续空白压成一个空格，超长截断并以省略号结尾。
fn clean_line(value: Option<&Value>, max_chars: usize) -> Option<String> {
    let mut line = String::new();
    let mut pending_space = false;
    for ch in value?.as_str()?.chars() {
        if ch.is_control() || ch.is_whitespace() {
            pending_space = !line.is_empty();
            continue;
        }
        if pending_space {
            line.push(' ');
            pending_space = false;
        }
        line.push(ch);
    }
    if line.is_empty() {
        return None;
    }
    if line.chars().count() > max_chars {
        let mut clipped: String = line.chars().take(max_chars.saturating_sub(1)).collect();
        clipped.push('…');
        return Some(clipped);
    }
    Some(line)
}

/// 内容片段只保留换行与制表符两种控制字符；游标按原始字节计，不受影响。
fn clean_content(text: &str) -> String {
    text.chars()
        .filter(|ch| !ch.is_control() || matches!(ch, '\n' | '\t'))
        .collect()
}

/// 修复父子关系并排成父先子后：父节点缺失或指向自己的挂到根；环上最先出现的
/// 节点切断回边成为根，挂在环下的节点保留原父子关系。深度不设上限，遍历是迭代的。
fn into_tree_order(mut nodes: Vec<SnapshotNode>) -> Vec<SnapshotNode> {
    let index: HashMap<String, usize> = nodes
        .iter()
        .enumerate()
        .map(|(position, entry)| (entry.node.id.clone(), position))
        .collect();
    for entry in &mut nodes {
        let dangling = entry
            .node
            .parent_id
            .as_ref()
            .is_some_and(|parent| *parent == entry.node.id || !index.contains_key(parent));
        if dangling {
            entry.node.parent_id = None;
        }
    }
    let parents: Vec<Option<usize>> = nodes
        .iter()
        .map(|entry| {
            entry
                .node
                .parent_id
                .as_ref()
                .and_then(|id| index.get(id))
                .copied()
        })
        .collect();
    let mut children = vec![Vec::new(); nodes.len()];
    for (position, parent) in parents.iter().enumerate() {
        if let Some(parent) = *parent {
            children[parent].push(position);
        }
    }

    let roots: Vec<usize> = (0..nodes.len())
        .filter(|&position| parents[position].is_none())
        .collect();
    let mut visited = vec![false; nodes.len()];
    let mut order = Vec::with_capacity(nodes.len());
    let mut cut = Vec::new();
    let mut path_step = vec![usize::MAX; nodes.len()];
    let mut path = Vec::new();
    // 第二轮只会遇到环上（或挂在环下）的节点：从根出发的一轮已访问全部可达节点，
    // 未访问节点的父节点也必然未访问，沿父链上溯终会回到自己走过的环。
    for start in roots.into_iter().chain(0..nodes.len()) {
        if visited[start] {
            continue;
        }
        let start = if parents[start].is_some() {
            let head = cycle_head(&parents, start, &mut path_step, &mut path);
            cut.push(head);
            head
        } else {
            start
        };
        let mut stack = vec![start];
        while let Some(position) = stack.pop() {
            if visited[position] {
                continue;
            }
            visited[position] = true;
            order.push(position);
            stack.extend(
                children[position]
                    .iter()
                    .rev()
                    .copied()
                    .filter(|&child| !visited[child]),
            );
        }
    }
    for position in cut {
        nodes[position].node.parent_id = None;
    }

    let mut slots: Vec<Option<SnapshotNode>> = nodes.into_iter().map(Some).collect();
    order
        .into_iter()
        .filter_map(|position| slots[position].take())
        .collect()
}

/// 从 `start` 沿父链上溯直到走回已走过的节点，返回该环上快照位置最小（最先出现）
/// 的节点。`path_step` / `path` 是调用方复用的暂存区，返回前复原。
fn cycle_head(
    parents: &[Option<usize>],
    start: usize,
    path_step: &mut [usize],
    path: &mut Vec<usize>,
) -> usize {
    let mut position = start;
    let cycle_from = loop {
        if path_step[position] != usize::MAX {
            break path_step[position];
        }
        path_step[position] = path.len();
        path.push(position);
        match parents[position] {
            Some(parent) => position = parent,
            // 上溯到了根：调用约定下不会发生，按「本节点即切断点」兜底。
            None => break path.len() - 1,
        }
    };
    let head = path[cycle_from..].iter().copied().min().unwrap_or(start);
    for &step in path.iter() {
        path_step[step] = usize::MAX;
    }
    path.clear();
    head
}

fn parse_cursor(cursor: &str) -> Result<u64, SourceError> {
    cursor
        .trim()
        .parse()
        .map_err(|_| SourceError::Malformed(format!("cursor is not an offset: {cursor}")))
}

fn read_output(output: &NodeOutput, requested: Option<u64>, max_bytes: usize) -> ContentChunk {
    let text = output.text.as_str();
    let end = output.start.saturating_add(text.len() as u64);
    // 游标早于保留窗口：头部已被扩展丢弃，从窗口开头读。
    let (position, truncated) = match requested {
        None => (output.start, output.start > 0),
        Some(offset) if offset < output.start => (output.start, true),
        Some(offset) => (offset.min(end), false),
    };
    let mut from = usize::try_from(position - output.start)
        .unwrap_or(text.len())
        .min(text.len());
    // 不是本适配器签发的游标可能落在字符中间：前移到下一个字符边界。
    while !text.is_char_boundary(from) {
        from += 1;
    }
    let mut to = from.saturating_add(max_bytes).min(text.len());
    while to > from && !text.is_char_boundary(to) {
        to -= 1;
    }
    if to == from {
        // 预算不足一个字符：整字符给出，保证分页前进。
        to += text[from..].chars().next().map_or(0, char::len_utf8);
    }
    let next = output.start.saturating_add(to as u64);
    let eof = next >= end;
    ContentChunk {
        format: output.format,
        text: clean_content(&text[from..to]),
        next_cursor: (!eof).then(|| next.to_string()),
        eof,
        truncated,
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::api::schema::{AgentActivityKind, AgentActivityStatus};

    const EXTENSION_SESSION_PATH: &str = "/home/user/.pi/agent/sessions/--home-user-demo--/2026-09-22T10-00-00-000Z_5f000000-0000-4000-8000-000000000001.jsonl";
    const EXTENSION_SESSION_ID: &str = "5f000000-0000-4000-8000-000000000001";

    fn fixture(name: &str) -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/agent-activity/pi")
            .join(name);
        std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("读取夹具 {}：{error}", path.display()))
    }

    fn home() -> PathBuf {
        std::env::temp_dir()
    }

    fn context<'a>(home: &'a Path, session: Option<&'a AgentSessionRef>) -> SourceContext<'a> {
        SourceContext {
            codex_cache: None,
            agent: "pi",
            session,
            cwd: None,
            home,
            now_ms: 1_726_990_000_000,
            agent_config_dir: None,
            latest_hint: None,
        }
    }

    fn discover(hint: &str) -> Vec<AgentActivityNode> {
        let home = home();
        discover_from_hint(&context(&home, None), hint).expect("快照可解析")
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

    fn read(hint: &str, node_id: &str, cursor: Option<&str>, max_bytes: usize) -> ContentChunk {
        let home = home();
        read_from_hint(&context(&home, None), hint, node_id, cursor, max_bytes).expect("节点可读")
    }

    /// 从头按页读到 eof，返回拼接文本、每页游标与首页的 truncated。
    fn read_all(hint: &str, node_id: &str, max_bytes: usize) -> (String, Vec<u64>, bool) {
        let mut text = String::new();
        let mut cursors = Vec::new();
        let mut cursor: Option<String> = None;
        let mut first_truncated = None;
        for _ in 0..10_000 {
            let page = read(hint, node_id, cursor.as_deref(), max_bytes);
            first_truncated.get_or_insert(page.truncated);
            text.push_str(&page.text);
            if page.eof {
                assert!(page.next_cursor.is_none(), "eof 时不再给游标");
                return (text, cursors, first_truncated.unwrap_or(false));
            }
            let next = page.next_cursor.expect("未到 eof 必须给出下一个游标");
            cursors.push(next.parse().expect("游标是十进制偏移"));
            cursor = Some(next);
        }
        panic!("分页没有收敛");
    }

    #[test]
    fn trait_entry_points_stay_unsupported_without_a_hint_cache() {
        let home = home();
        let session = AgentSessionRef::id(EXTENSION_SESSION_ID).expect("合法会话 id");
        let cx = context(&home, Some(&session));
        assert_eq!(Pi.id(), "pi");
        assert!(matches!(Pi.discover(&cx), Err(SourceError::Unsupported)));
        assert!(matches!(
            Pi.read(&cx, "call_par", None, 1024),
            Err(SourceError::Unsupported)
        ));
    }

    #[test]
    fn extension_snapshot_becomes_a_parent_first_tree() {
        let nodes = discover(&fixture("snapshot-extension.json"));
        assert_eq!(
            ids(&nodes),
            [
                "call_par",
                "call_par/0",
                "call_par/1",
                "call_par/2",
                "call_ws",
                "call_notes"
            ]
        );

        let parallel = node(&nodes, "call_par");
        assert_eq!(parallel.kind, AgentActivityKind::Subagent);
        assert_eq!(parallel.status, AgentActivityStatus::Done);
        assert_eq!(parallel.label, "subagent parallel (3 tasks)");
        assert_eq!(parallel.parent_id, None);
        assert_eq!(parallel.started_at_ms, Some(1_000));
        assert_eq!(parallel.ended_at_ms, Some(3_000));
        assert_eq!(parallel.summary.as_deref(), Some("Parallel: 1/3 succeeded"));
        assert_eq!(parallel.content_ref.as_deref(), Some("call_par"));

        let reviewer = node(&nodes, "call_par/1");
        assert_eq!(reviewer.parent_id.as_deref(), Some("call_par"));
        assert_eq!(reviewer.kind, AgentActivityKind::Subagent);
        assert_eq!(reviewer.status, AgentActivityStatus::Failed);
        assert_eq!(reviewer.agent_type.as_deref(), Some("reviewer"));
        assert_eq!(reviewer.label, "reviewer: Review the login flow");

        let search = node(&nodes, "call_ws");
        assert_eq!(search.kind, AgentActivityKind::Task);
        assert_eq!(search.status, AgentActivityStatus::Failed);
        assert_eq!(search.agent_type, None);

        let notes = node(&nodes, "call_notes");
        assert_eq!(notes.status, AgentActivityStatus::Running);
        assert_eq!(notes.ended_at_ms, None);
        assert_eq!(notes.label, "notes 发布检查清单");
    }

    /// 扩展写到 socket 上的整条请求（形状与 bun 契约测试一致）：能反序列化成
    /// `pane.report_agent_activity`，其 `hint` 就是本适配器读的快照。
    #[test]
    fn extension_wire_request_carries_a_readable_snapshot() {
        use crate::api::schema::{Method, Request};

        let hint = fixture("snapshot-extension.json");
        let request: Request = serde_json::from_value(serde_json::json!({
            "id": "herdr:pi:activity:1726990000000:abc",
            "method": "pane.report_agent_activity",
            "params": {
                "pane_id": "wT:p3",
                "source": "herdr:pi",
                "agent": "pi",
                "hint": hint,
                "seq": 1_726_990_000_000_001_u64
            }
        }))
        .expect("扩展的请求形状必须是合法的 pane.report_agent_activity");
        let Method::PaneReportAgentActivity(params) = request.method else {
            panic!("方法名必须是 pane.report_agent_activity");
        };
        assert_eq!(params.agent, "pi");
        assert_eq!(params.seq, Some(1_726_990_000_000_001));
        assert_eq!(params.node_id, None);
        let nodes = discover(params.hint.as_deref().expect("hint 携带快照"));
        assert_eq!(nodes.len(), 6);
    }

    #[test]
    fn snapshot_of_another_session_reads_as_an_empty_tree() {
        let hint = fixture("snapshot-extension.json");
        let home = home();
        // 夹具里的会话路径是 Unix 形态；`AgentSessionRef::path` 按本平台校验绝对路径
        // （Windows 要盘符），这里比对的只是字符串，直接构造引用。
        let path_ref = |value: &str| AgentSessionRef {
            kind: crate::agent_resume::AgentSessionRefKind::Path,
            value: value.to_owned(),
        };
        let same_path = path_ref(EXTENSION_SESSION_PATH);
        let same_id = AgentSessionRef::id(EXTENSION_SESSION_ID).expect("合法 id");
        let other_path = path_ref("/home/user/.pi/agent/sessions/--x--/other.jsonl");
        let other_id =
            AgentSessionRef::id("5f000000-0000-4000-8000-00000000ffff").expect("合法 id");

        for session in [&same_path, &same_id] {
            let nodes = discover_from_hint(&context(&home, Some(session)), &hint).expect("可解析");
            assert_eq!(nodes.len(), 6);
        }
        for session in [&other_path, &other_id] {
            let cx = context(&home, Some(session));
            assert!(discover_from_hint(&cx, &hint).expect("可解析").is_empty());
            assert!(matches!(
                read_from_hint(&cx, &hint, "call_par", None, 64),
                Err(SourceError::Unavailable)
            ));
        }
        // 快照没带会话引用：无从比对，按属于处理。
        let bare = fixture("snapshot-missing-fields.json");
        let nodes = discover_from_hint(&context(&home, Some(&other_id)), &bare).expect("可解析");
        assert_eq!(nodes.len(), 5);
    }

    #[test]
    fn missing_fields_degrade_to_defaults() {
        let hint = fixture("snapshot-missing-fields.json");
        let nodes = discover(&hint);
        assert_eq!(
            ids(&nodes),
            [
                "call_min",
                "call_child",
                "call_label_only",
                "call_output_no_text",
                "call_output_bad"
            ]
        );

        let minimal = node(&nodes, "call_min");
        assert_eq!(minimal.label, "call_min", "缺 label 退回 id");
        assert_eq!(minimal.kind, AgentActivityKind::Unknown);
        assert_eq!(minimal.status, AgentActivityStatus::Unknown);
        assert_eq!(minimal.started_at_ms, None);
        assert_eq!(minimal.content_ref, None);

        let child = node(&nodes, "call_child");
        assert_eq!(child.parent_id.as_deref(), Some("call_min"));
        assert_eq!(child.status, AgentActivityStatus::Running);

        // output 缺 text：可读、为空、头部已丢弃。
        assert_eq!(
            node(&nodes, "call_output_no_text").content_ref.as_deref(),
            Some("call_output_no_text")
        );
        let page = read(&hint, "call_output_no_text", None, 64);
        assert_eq!(page.text, "");
        assert!(page.eof);
        assert!(page.truncated);
        assert_eq!(page.format, AgentActivityContentFormat::Text);

        // output 不是对象：当作没有内容，节点仍在。
        assert_eq!(node(&nodes, "call_output_bad").content_ref, None);
        let empty = read(&hint, "call_output_bad", None, 64);
        assert!(empty.eof && empty.text.is_empty() && !empty.truncated);

        // 顶层缺 nodes：空树而不是错误。
        assert!(discover(r#"{"type":"herdr.activity.snapshot","version":1}"#).is_empty());
    }

    #[test]
    fn unknown_enum_values_fall_back_to_unknown() {
        let hint = fixture("snapshot-unknown-values.json");
        let nodes = discover(&hint);
        let workflow = node(&nodes, "call_a");
        assert_eq!(workflow.kind, AgentActivityKind::Unknown);
        assert_eq!(workflow.status, AgentActivityStatus::Unknown);
        assert_eq!(
            read(&hint, "call_a", None, 64).format,
            AgentActivityContentFormat::Unknown
        );

        let numbers = node(&nodes, "call_b");
        assert_eq!(numbers.kind, AgentActivityKind::Unknown);
        assert_eq!(numbers.status, AgentActivityStatus::Unknown);

        // 扩展今天不写、但 schema 认识的取值照常映射。
        let background = node(&nodes, "call_c");
        assert_eq!(background.kind, AgentActivityKind::Background);
        assert_eq!(background.status, AgentActivityStatus::Blocked);
    }

    #[test]
    fn bad_nodes_are_skipped_and_the_tree_is_repaired() {
        let nodes = discover(&fixture("snapshot-bad-nodes.json"));
        assert_eq!(
            ids(&nodes),
            [
                "call_root",
                "call_first_child",
                "call_root/0",
                "call_late_child",
                "call_orphan",
                "call_self",
                "call_cycle_a",
                "call_cycle_b"
            ]
        );

        let root = node(&nodes, "call_root");
        assert_eq!(
            root.label, "subagent parallel (2 tasks)",
            "同 id 以先出现的为准"
        );
        assert_eq!(root.started_at_ms, None, "非数字时间戳丢弃");
        assert_eq!(root.ended_at_ms, None, "负数时间戳丢弃");
        let scout = node(&nodes, "call_root/0");
        assert_eq!(scout.started_at_ms, Some(1_726_990_000_123));
        assert_eq!(scout.ended_at_ms, Some(1_726_990_005_000));
        assert_eq!(
            node(&nodes, "call_late_child").parent_id.as_deref(),
            Some("call_root/0")
        );

        assert_eq!(
            node(&nodes, "call_orphan").parent_id,
            None,
            "父节点缺失挂到根"
        );
        assert_eq!(node(&nodes, "call_self").parent_id, None, "指向自己挂到根");
        assert_eq!(
            node(&nodes, "call_cycle_a").parent_id,
            None,
            "环在最先出现处切断"
        );
        assert_eq!(
            node(&nodes, "call_cycle_b").parent_id.as_deref(),
            Some("call_cycle_a")
        );
    }

    /// 挂在环下的节点先于环出现时，只切断环上最先出现的节点；挂在下面的节点保留
    /// 原父节点，不被当成切断点挂到根。
    #[test]
    fn a_cycle_is_cut_at_its_first_node_and_keeps_what_hangs_below() {
        let snapshot = |nodes: &[String]| {
            format!(
                "{{\"type\":\"herdr.activity.snapshot\",\"version\":1,\"nodes\":[{}]}}",
                nodes.join(",")
            )
        };
        let entry = |id: &str, parent: &str| {
            format!(
                "{{\"id\":\"{id}\",\"kind\":\"subagent\",\"status\":\"running\",\"parent_id\":\"{parent}\"}}"
            )
        };
        let parents = |nodes: &[AgentActivityNode]| {
            nodes
                .iter()
                .map(|node| (node.id.clone(), node.parent_id.clone()))
                .collect::<Vec<_>>()
        };
        let some = |id: &str| Some(id.to_owned());

        // below 挂在 a 下，先于环 a ⇄ b 出现。
        let nodes = discover(&snapshot(&[
            entry("below", "a"),
            entry("a", "b"),
            entry("b", "a"),
        ]));
        assert_eq!(
            parents(&nodes),
            [
                ("a".to_owned(), None),
                ("below".to_owned(), some("a")),
                ("b".to_owned(), some("a")),
            ],
            "环在最先出现的 a 处切断，below 仍挂在 a 下"
        );

        // 环上最先出现的是 b（a 在它之后）：切断 b，a 与挂在 b 下的 x 都保留父节点。
        let nodes = discover(&snapshot(&[
            entry("x", "b"),
            entry("b", "a"),
            entry("a", "b"),
        ]));
        assert_eq!(
            parents(&nodes),
            [
                ("b".to_owned(), None),
                ("x".to_owned(), some("b")),
                ("a".to_owned(), some("b")),
            ]
        );

        // 更长的链挂在三节点环下：只有环头变成根，其余父子关系不变且父先子后。
        let nodes = discover(&snapshot(&[
            entry("leaf", "mid"),
            entry("mid", "c2"),
            entry("c1", "c3"),
            entry("c2", "c1"),
            entry("c3", "c2"),
        ]));
        let roots: Vec<&str> = nodes
            .iter()
            .filter(|node| node.parent_id.is_none())
            .map(|node| node.id.as_str())
            .collect();
        assert_eq!(roots, ["c1"]);
        assert_eq!(node(&nodes, "mid").parent_id, some("c2"));
        assert_eq!(node(&nodes, "leaf").parent_id, some("mid"));
        assert_eq!(node(&nodes, "c3").parent_id, some("c2"));
        let position = |id: &str| {
            nodes
                .iter()
                .position(|node| node.id == id)
                .expect("节点在快照里")
        };
        for node in &nodes {
            if let Some(parent) = &node.parent_id {
                assert!(
                    position(parent) < position(&node.id),
                    "父先子后：{}",
                    node.id
                );
            }
        }
    }

    #[test]
    fn every_hint_line_parses_on_its_own() {
        let home = home();
        let cx = context(&home, None);
        let outcomes: Vec<Result<usize, String>> = fixture("hints.jsonl")
            .lines()
            .map(|line| match discover_from_hint(&cx, line) {
                Ok(nodes) => Ok(nodes.len()),
                Err(SourceError::Malformed(reason)) => Err(reason),
                Err(other) => panic!("意外的错误 {other:?}"),
            })
            .collect();
        assert_eq!(outcomes.len(), 9);
        assert_eq!(outcomes[0], Ok(1));
        for (line, outcome) in outcomes.iter().enumerate().skip(1).take(6) {
            assert!(
                outcome.is_err(),
                "第 {} 行应判为坏 hint：{outcome:?}",
                line + 1
            );
        }
        assert!(outcomes[1]
            .as_ref()
            .is_err_and(|reason| reason.contains("not JSON")));
        assert!(outcomes[3]
            .as_ref()
            .is_err_and(|reason| reason.contains("herdr.activity.snapshot")));
        assert!(outcomes[4]
            .as_ref()
            .is_err_and(|reason| reason.contains("version")));
        // 更高版本只增字段：照常解析，多出来的键忽略。
        assert_eq!(outcomes[7], Ok(1));
        assert_eq!(outcomes[8], Ok(0));

        let later = fixture("hints.jsonl")
            .lines()
            .nth(7)
            .map(str::to_owned)
            .expect("第 8 行");
        assert_eq!(discover(&later)[0].status, AgentActivityStatus::Done);
    }

    #[test]
    fn deep_nesting_keeps_every_level_parent_first() {
        let nodes = discover(&fixture("snapshot-deep.json"));
        let expected: Vec<String> = (0..64).map(|level| format!("call_d{level:02}")).collect();
        assert_eq!(ids(&nodes), expected);
        for pair in nodes.windows(2) {
            assert_eq!(pair[1].parent_id.as_deref(), Some(pair[0].id.as_str()));
        }

        // 超过节点上限：按出现顺序取前 256 个，父节点被截掉的那一层挂到根。
        let chain: Vec<String> = (0..300)
            .rev()
            .map(|level| {
                let parent = if level == 0 {
                    String::new()
                } else {
                    format!(r#","parent_id":"n{}""#, level - 1)
                };
                format!(r#"{{"id":"n{level}","kind":"subagent","status":"running"{parent}}}"#)
            })
            .collect();
        let hint = format!(
            r#"{{"type":"herdr.activity.snapshot","version":1,"nodes":[{}]}}"#,
            chain.join(",")
        );
        let nodes = discover(&hint);
        assert_eq!(nodes.len(), MAX_NODES);
        assert_eq!(nodes[0].id, "n44");
        assert_eq!(nodes[0].parent_id, None);
        assert_eq!(nodes[MAX_NODES - 1].id, "n299");
    }

    #[test]
    fn read_pages_the_log_by_byte_cursor() {
        let hint = fixture("snapshot-extension.json");
        let snapshot: Value = serde_json::from_str(&hint).expect("JSON");
        let notes = snapshot["nodes"]
            .as_array()
            .and_then(|nodes| nodes.iter().find(|node| node["id"] == "call_notes"))
            .expect("call_notes");
        let expected = notes["output"]["text"].as_str().expect("text");
        let start = notes["output"]["start"].as_u64().expect("start");
        assert_eq!(start, 718);

        // 页预算不是 3 的倍数：CJK 字符永远不被切开，拼回来与原文一致。
        let (text, cursors, truncated) = read_all(&hint, "call_notes", 100);
        assert_eq!(text, expected);
        assert!(truncated, "日志头部已丢弃，首页必须标 truncated");
        assert!(cursors.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(cursors
            .iter()
            .all(|&cursor| cursor > start && cursor < start + expected.len() as u64));

        // 游标是绝对偏移：从中间某个游标续读等于原文对应后缀。
        let middle = cursors[cursors.len() / 2];
        let page = read(&hint, "call_notes", Some(&middle.to_string()), 1 << 20);
        assert!(page.eof);
        assert!(!page.truncated);
        assert_eq!(page.text, &expected[(middle - start) as usize..]);

        // 预算小于一个字符也会整字符前进。
        let tiny = read(&hint, "call_notes", None, 1);
        assert_eq!(tiny.text.chars().count(), 1);
        assert!(tiny.next_cursor.is_some());

        // markdown 格式照传。
        assert_eq!(
            read(&hint, "call_par/0", None, 4096).format,
            AgentActivityContentFormat::Markdown
        );
    }

    #[test]
    fn read_recovers_from_stale_cursors_and_reports_missing_nodes() {
        let hint = fixture("snapshot-extension.json");
        let home = home();
        let cx = context(&home, None);

        // 游标早于保留窗口（头部被丢掉）：从窗口开头读并标 truncated。
        let stale = read(&hint, "call_notes", Some("10"), 32);
        assert!(stale.truncated);
        assert_eq!(stale.text, read(&hint, "call_notes", None, 32).text);

        // 游标越过末尾：空页 eof。
        let beyond = read(&hint, "call_notes", Some("999999"), 32);
        assert!(beyond.eof);
        assert!(beyond.text.is_empty());
        assert!(beyond.next_cursor.is_none());

        // 落在字符中间的游标前移到下一个字符边界。
        let inside = read(&hint, "call_notes", Some("719"), 3);
        assert_eq!(inside.text.chars().count(), 1);

        assert!(matches!(
            read_from_hint(&cx, &hint, "call_notes", Some("abc"), 32),
            Err(SourceError::Malformed(_))
        ));
        assert!(matches!(
            read_from_hint(&cx, &hint, "call_gone", None, 32),
            Err(SourceError::Unavailable)
        ));
        assert!(matches!(
            read_from_hint(&cx, "tool_execution_end", "call_notes", None, 32),
            Err(SourceError::Malformed(_))
        ));
    }

    #[test]
    fn display_text_is_single_line_and_content_drops_control_characters() {
        let long_label = "x".repeat(500);
        let hint = serde_json::json!({
            "type": "herdr.activity.snapshot",
            "version": 1,
            "nodes": [{
                "id": "call_x",
                "kind": "task",
                "status": "running",
                "label": format!("line one\n\u{1b}[31mline\ttwo {long_label}"),
                "summary": "  \u{7}\n  ",
                "output": { "text": "a\u{7}b\tc\r\nd\u{1b}e", "start": 0, "format": "text" }
            }]
        })
        .to_string();
        let nodes = discover(&hint);
        let label = &nodes[0].label;
        assert!(label.starts_with("line one [31mline two x"));
        assert_eq!(label.chars().count(), MAX_LABEL_CHARS);
        assert!(label.ends_with('…'));
        assert!(!label.chars().any(char::is_control));
        assert_eq!(nodes[0].summary, None, "只有空白的摘要按缺失处理");

        let page = read(&hint, "call_x", None, 1024);
        assert_eq!(page.text, "ab\tc\nde");
        assert!(page.eof);
    }

    #[test]
    fn oversized_or_non_object_hints_are_rejected() {
        let home = home();
        let cx = context(&home, None);
        let oversized = format!(
            r#"{{"type":"herdr.activity.snapshot","version":1,"pad":"{}"}}"#,
            "x".repeat(MAX_HINT_BYTES)
        );
        assert!(matches!(
            discover_from_hint(&cx, &oversized),
            Err(SourceError::Malformed(reason)) if reason.contains("exceeds")
        ));
        assert!(matches!(
            discover_from_hint(&cx, "\"just a string\""),
            Err(SourceError::Malformed(_))
        ));
        assert!(matches!(
            discover_from_hint(&cx, ""),
            Err(SourceError::Malformed(_))
        ));
    }
}
