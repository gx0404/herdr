//! ZCode（智谱 Z.ai 桌面应用，开源仓库 `zai-org/ZCode`）的外部来源适配器：把桌面
//! 会话读成不属于任何 pane 的外部条目（`ExternalAgentInfo`）及其活动树，并按节点
//! 分页读出子 agent 的内容。ZCode 不跑在 pane 里，所以 `discover` 恒为
//! `Unsupported`，只实现 `discover_external` 与 `read`。
//!
//! # 读取方式
//!
//! - 会话树来自 `<home>/.zcode/cli/db/db.sqlite`（本机约 600 MB，WAL 模式）。不新增
//!   Rust 依赖：起一个短命的系统 `sqlite3` 子进程（`-readonly -batch`，限时 5 s，
//!   `.timeout` 2 s 等锁，`-init` 指向空设备以屏蔽 `~/.sqliterc`），每行输出由 SQL 的
//!   `json_object()` 拼成一个 JSON 对象。**不用 `-json`**：本机 sqlite3 3.31.1
//!   没有这个选项（3.33 起才有），`json_object()` 新旧版本都能用。
//! - 没装 ZCode（没有库文件）→ 空列表；没装 sqlite3、库打不开、会话查询失败或超时
//!   → `SourceError::Unavailable`，外部分组显示「暂不可读」。
//! - 一次子进程依次跑四条语句：会话树（必需）→ 待办 → 轮次统计 → 迁移版本（后三条
//!   可选）。sqlite3 对命令行给出的 SQL 遇错即停，所以可选语句排在后面，且每条都
//!   以 `union all` 一行哨兵收尾：哨兵在 = 该语句跑通；缺了只降级那一块。
//!   `schema_migration` 出现未核实的新迁移只记 debug 日志，照常尽力解析。
//! - 只查近期：根会话限 `parent_id is null`（走 `session_parent_idx`）、未归档、
//!   `time_updated` 在最近 72 h 内，按其倒序取前 10 个；子树用递归 CTE 沿
//!   `parent_id` 下行（同样走 `session_parent_idx`），深度 ≤ 8、总行数 ≤ 300。
//!   本机实测该查询 63 ms。
//! - 子 agent 节点的详情读 `agents/<父会话 id>/<agentId>/metadata.json`；内容读同
//!   目录的 `transcript.jsonl`（结构化事件流，格式化成可读文本），没有就退
//!   `output.txt`。树以 DB 的 `parent_id` 为准；metadata 缺失、坏 JSON 或字段类型
//!   不对时只丢那几项，节点照样出现。
//! - `~/.zcode/cli/agents` 的变化由骨架的低频轮询驱动，本适配器不做 inotify。
//!
//! # 映射
//!
//! - 外部条目 = 一个近期根会话，`external_id` = `zcode:<会话 id>`。最新一轮
//!   `turn_usage` 是 running、或有子 agent 在跑，且整棵树 2 h 内有动静 → Working；
//!   否则 Idle（桌面会话没有「已完成未查看」的概念）。
//! - 子会话 → 节点，id 就是会话 id。`subagent_child` → Subagent；其余
//!   （`selection_side_chat` 与未来的新类型）→ Unknown，`agent_type` 记原始
//!   `task_type`。状态以 metadata 的 `status` 为准（stopped / cancelled / killed 按
//!   Failed 呈现、摘要写明原因），没有 metadata 退回最新一轮；running 超过 2 h 没有
//!   任何动静 → Unknown，摘要 `stale`。
//! - 待办：根会话的全部，外加仍在跑的子 agent 的，id 为 `todo:<会话 id>:<position>`。
//! - `read` 只对子 agent 节点可用（其余节点 `Unsupported`）；游标是 `t:<偏移>`
//!   （转录）或 `o:<偏移>`（output.txt）。读到末尾时 `eof=true` 只表示「暂时读完」，
//!   `next_cursor` 仍给出当前位置供跟随续读；转录里不以换行结尾的半截末行不消费，
//!   游标留在行首。
//!
//! # 本机取证（2026-09-22，只读核对结构、键名与枚举取值，未读取任何对话正文）
//!
//! 版本：deb 包 `zcode 3.14.3-7762`（`dpkg -s`），当天 14:37 已启动；内置 CLI 引擎
//! `0.16.9`（`~/.zcode/cli/log/*.jsonl` 的 `context.version`，打包源
//! `/opt/ZCode/resources/glm/zcode.cjs`）。启动后迁移仍停在
//! `0022_backfilled_session_reasoning`（`schema_migration.id`；其 `app_version` 列记
//! 的是 CLI 引擎版本 0.2.0…0.16.5，不是桌面版本）。
//!
//! `session` 表（541 行）键：`id`、`project_id`、`workspace_id`、`parent_id`、
//! `slug`、`directory`、`path`、`title`、`version`（CLI 引擎版本）、
//! `time_created` / `time_updated` / `time_archived`（毫秒）、`task_type`、
//! `title_source`、`trace_id` 等。`task_type` 实测：`interactive`（149，全是根）、
//! `subagent_child`（391，父全是 interactive）、`selection_side_chat`（1，父是
//! interactive）。本机没有超过一层的嵌套，但目录规则允许（见下），树按任意深度建。
//! `session_task_link` 仍是 0 行（勿用）。
//!
//! id 规则（391/391 吻合）：子会话 id = `sess_subagent_` + `agentId`，`agentId` 形如
//! `agent_<uuid>`、就是目录名 → `agents/<parentSessionId>/agent_<uuid>/`，其中
//! `parentSessionId` 与 DB 的 `parent_id` 相同。
//!
//! `metadata.json`（391 份，0 份坏）键：`agentId`、`childSessionId`、
//! `parentSessionId`、`parentToolUseId`、`profileId`（= `profileSnapshot.name`）、
//! `profileSnapshot{name,color,description,source,systemPrompt,tools,
//! injectAgentsMd,modelSelection,path}`、`description`（≤ 40 字符）、`prompt`、`cwd`、
//! `workspaceRoot`、`status`、`createdAt` / `updatedAt` / `completedAt`（RFC3339 毫秒
//! UTC）、`totalDurationMs`、`totalTokens`、`totalToolUseCount`、`usage{inputTokens,
//! outputTokens,cacheReadTokens,cacheWriteTokens,reasoningTokens,totalTokens}`、
//! `error`（字符串）、`metadataFile` / `outputFile` / `taskOutputFile` /
//! `transcriptFile`（绝对路径）。`status` 实测 `completed` / `failed` / `running`；
//! `profileSnapshot.name` 实测 `Explore` / `general-purpose` / `vision` / `flash`。
//! 唯一一份 `running` 是 8 月中断留下的残留（目录里只有 metadata.json），所以
//! running 要做陈旧判定。0.16.9 引擎包里另有写 `status: "stopped"` 的路径（后台
//! agent 被停），该路径只写 agentId / childSessionId / completedAt / description /
//! outputFile / parentSessionId / parentToolUseId / profileId / prompt / status /
//! taskOutputFile / updatedAt，没有 createdAt / profileSnapshot；续跑路径另加
//! `resumedAt` / `resumedFromMessageId`。metadata 只在开始与结束时各写一次，
//! `updatedAt` 不是心跳。
//!
//! `transcript.jsonl` 只在 CLI 引擎 ≤ 0.16.3 的运行里出现（190/391）；0.16.5 与
//! 0.16.9 的运行一份都没有，0.16.9 包里也不再有 `transcriptFile` 键 → 当前版本
//! 实际只能读 `output.txt`（完成或失败时一次性写入，与 `task.output` 同内容）。
//! 转录行顶层键 `id`、`sessionId`、`turnId`、`type`、`timestamp`、`traceId`、
//! `sequenceNumber`（不单调）、`payload`；`type` 实测 11 种：`turn_started`、
//! `turn_complete`、`model_request`（单行可达 2 MB，含完整上下文）、
//! `model_streaming`（占 96% 行数）、`model_complete`、`model_network_status`、
//! `tool_call_scheduled`、`tool_batch_complete`、`streaming_tool_ledger_updated`、
//! `stream_recovery_anchor_created`、`checkpoint_created`。计划里列的三个父链字段名
//! （`parentToolUseId` / `parentToolCallId` / `_meta.claudeCode.parentToolUseId`）在
//! 175 万行转录里一个都没出现，metadata 只有 `parentToolUseId`；它们指向父会话里
//! 派生该子 agent 的工具调用，不是父会话，建树不需要，本适配器不读。
//!
//! `turn_usage`（每轮一行，主键 `(session_id, turn_id)`）的 `status` ∈ `running` /
//! `completed` / `error` / `cancelled`（schema 的 check 约束；本机当时 0 行
//! running），用来判断会话是否在跑；`todo` 表（`session_id`、`content`、`status`、
//! `priority`、`position`）的 `status` 实测 `pending` / `in_progress` / `completed`。

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::Value;

use super::{ActivitySource, ContentChunk, SourceContext, SourceError};
use crate::api::schema::{
    AgentActivityContentFormat, AgentActivityKind, AgentActivityNode, AgentActivityStatus,
    AgentStatus, ExternalAgentInfo,
};

pub(super) struct ZCode;

const SOURCE_ID: &str = "zcode";
const SQLITE_PROGRAM: &str = "sqlite3";
/// 交给 `-init` 的空文件：不读用户的 `~/.sqliterc`（它可能改掉输出模式）。给空串在
/// 新版 sqlite3 上会往 stderr 报 "cannot open"，所以用空设备。
const SQLITE_EMPTY_INIT: &str = if cfg!(windows) { "NUL" } else { "/dev/null" };
/// 整个 sqlite3 子进程的时限；超时即杀掉并视为暂不可读。
const SQLITE_TIMEOUT: Duration = Duration::from_secs(5);
const SQLITE_POLL: Duration = Duration::from_millis(10);
/// 遇到写锁时 sqlite3 自己等待的上限（WAL 下读者一般不会被挡）。
const SQLITE_BUSY_TIMEOUT_MS: u32 = 2_000;
const MAX_QUERY_OUTPUT_BYTES: u64 = 8 * 1024 * 1024;
const MAX_STDERR_BYTES: u64 = 4 * 1024;

/// 只列最近这段时间里动过的根会话。
const RECENT_WINDOW_MS: u64 = 72 * 60 * 60 * 1000;
const ROOT_LIMIT: usize = 10;
const MAX_TREE_DEPTH: usize = 8;
const TREE_ROW_LIMIT: usize = 300;
/// running 超过这么久没有任何动静就当作中断残留。
const STALE_RUNNING_MS: u64 = 2 * 60 * 60 * 1000;
/// 本机核实过的最新迁移序号（`0022_backfilled_session_reasoning`）。
const LATEST_VERIFIED_MIGRATION: u32 = 22;

const SUBAGENT_SESSION_PREFIX: &str = "sess_subagent_";
const SUBAGENT_TASK_TYPE: &str = "subagent_child";
const METADATA_FILE: &str = "metadata.json";
const TRANSCRIPT_FILE: &str = "transcript.jsonl";
const OUTPUT_FILE: &str = "output.txt";
const MAX_METADATA_BYTES: u64 = 256 * 1024;
const MAX_ID_LEN: usize = 128;

const LABEL_CHARS: usize = 120;
const SUMMARY_CHARS: usize = 160;
const TODO_CHARS: usize = 200;
const TOOL_DETAIL_CHARS: usize = 160;
const ROOT_FALLBACK_LABEL: &str = "ZCode session";
const SUBAGENT_FALLBACK_LABEL: &str = "subagent";

/// `read` 的单页预算上下限（字节）。
const MIN_READ_BYTES: usize = 256;
const MAX_READ_BYTES: usize = 1024 * 1024;
/// 单次 `read` 最多扫过的转录字节数：流式增量行不产出文本，不设上限会一口气
/// 扫完几十 MB 的文件。
const SCAN_BUDGET_BYTES: u64 = 8 * 1024 * 1024;
/// 单行最多留在内存里的字节数；更长的行只数长度、不缓存。
const LINE_KEEP_BYTES: usize = 1024 * 1024;
/// 在行首这么多字节里嗅探 `"type":"…"`，命中跳过类型就不做整行 JSON 解析。
const TYPE_SNIFF_BYTES: usize = 512;
/// 不产出可读文本的转录事件：流式增量、完整请求上下文与内部账本。
const SKIPPED_EVENTS: &[&str] = &[
    "model_streaming",
    "model_request",
    "streaming_tool_ledger_updated",
    "stream_recovery_anchor_created",
    "checkpoint_created",
];

impl ActivitySource for ZCode {
    fn id(&self) -> &'static str {
        SOURCE_ID
    }

    fn discover(&self, _cx: &SourceContext<'_>) -> Result<Vec<AgentActivityNode>, SourceError> {
        // ZCode 是桌面应用，不跑在任何 pane 里；它的树只经 `discover_external` 给出。
        Err(SourceError::Unsupported)
    }

    fn read(
        &self,
        cx: &SourceContext<'_>,
        node_id: &str,
        cursor: Option<&str>,
        max_bytes: usize,
    ) -> Result<ContentChunk, SourceError> {
        let session_hint = cx.session.map(|session| {
            let value = session.value.as_str();
            value
                .strip_prefix(SOURCE_ID)
                .and_then(|rest| rest.strip_prefix(':'))
                .unwrap_or(value)
        });
        read_node(
            &agents_dir(cx.home),
            session_hint,
            node_id,
            cursor,
            max_bytes,
        )
    }

    fn discover_external(
        &self,
        home: &Path,
        now_ms: u64,
    ) -> Result<Vec<ExternalAgentInfo>, SourceError> {
        discover_sessions(&db_path(home), &agents_dir(home), SQLITE_PROGRAM, now_ms)
    }
}

fn data_root(home: &Path) -> PathBuf {
    home.join(".zcode").join("cli")
}

fn db_path(home: &Path) -> PathBuf {
    data_root(home).join("db").join("db.sqlite")
}

fn agents_dir(home: &Path) -> PathBuf {
    data_root(home).join("agents")
}

/// 会话 id / agentId 只允许 ASCII 字母数字与 `-` `_`，拼路径前一律过这道闸，
/// 杜绝 `..` 与分隔符。
fn is_safe_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_ID_LEN
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

// ---------------------------------------------------------------------------
// 发现：sqlite3 子进程 → 快照 → 外部条目
// ---------------------------------------------------------------------------

fn discover_sessions(
    db: &Path,
    agents: &Path,
    sqlite: &str,
    now_ms: u64,
) -> Result<Vec<ExternalAgentInfo>, SourceError> {
    if !db.is_file() {
        // 没装或从没启动过 ZCode：没有外部会话，不算不可读。
        return Ok(Vec::new());
    }
    let output = run_sqlite(sqlite, db, &tree_statements(now_ms), SQLITE_TIMEOUT)?;
    let snapshot = parse_query_output(&output.stdout);
    if !snapshot.sessions_ok {
        tracing::debug!(stderr = %output.stderr, "zcode: session query failed, source unreadable");
        return Err(SourceError::Unavailable);
    }
    note_degradations(&snapshot, &output.stderr);
    Ok(build_external_agents(&snapshot, agents, now_ms))
}

fn note_degradations(snapshot: &DbSnapshot, stderr: &str) {
    if !snapshot.todos_ok || !snapshot.turns_ok {
        tracing::debug!(
            todos = snapshot.todos_ok,
            turns = snapshot.turns_ok,
            stderr,
            "zcode: optional queries failed, activity degraded"
        );
    }
    match snapshot.migration.as_deref().map(migration_number) {
        Some(Some(number)) if number <= LATEST_VERIFIED_MIGRATION => {}
        Some(Some(number)) => tracing::debug!(
            migration = number,
            "zcode: database schema is newer than the verified one, parsing best-effort"
        ),
        Some(None) | None => {
            tracing::debug!("zcode: database schema version is unknown, parsing best-effort")
        }
    }
}

/// `0022_backfilled_session_reasoning` → 22。
fn migration_number(id: &str) -> Option<u32> {
    id.split('_').next()?.parse().ok()
}

/// 四条语句，顺序即降级顺序：会话树必需，其余可选（见模块文档）。
fn tree_statements(now_ms: u64) -> [String; 4] {
    let since = now_ms.saturating_sub(RECENT_WINDOW_MS);
    let tree = format!(
        "with recursive roots(id) as (select id from session where parent_id is null \
         and time_archived is null and time_updated >= {since} \
         order by time_updated desc limit {ROOT_LIMIT}), \
         tree(id, root_id, depth) as (select id, id, 0 from roots union all \
         select s.id, t.root_id, t.depth + 1 from session s join tree t on s.parent_id = t.id \
         where t.depth < {MAX_TREE_DEPTH} limit {TREE_ROW_LIMIT}) "
    );
    [
        format!(
            "{tree}select json_object('t', 's', 'id', s.id, 'root', t.root_id, \
             'depth', t.depth, 'parent', s.parent_id, 'task_type', s.task_type, \
             'title', substr(s.title, 1, {LABEL_CHARS}), 'directory', s.directory, \
             'created', s.time_created, 'updated', s.time_updated) \
             from tree t join session s on s.id = t.id \
             union all select json_object('t', 's_end')"
        ),
        format!(
            "{tree}select json_object('t', 'd', 'id', d.session_id, 'pos', d.position, \
             'content', substr(d.content, 1, {TODO_CHARS}), 'status', d.status) \
             from todo d where d.session_id in (select id from tree) \
             union all select json_object('t', 'd_end')"
        ),
        format!(
            "{tree}select json_object('t', 'u', 'id', u.session_id, \
             'last', max(coalesce(u.completed_at, u.started_at)), \
             'last_status', (select x.status from turn_usage x \
             where x.session_id = u.session_id order by x.started_at desc limit 1)) \
             from turn_usage u where u.session_id in (select id from tree) \
             group by u.session_id \
             union all select json_object('t', 'u_end')"
        ),
        "select json_object('t', 'v', 'id', max(id)) from schema_migration".to_string(),
    ]
}

struct SqliteOutput {
    stdout: String,
    stderr: String,
}

fn run_sqlite(
    program: &str,
    db: &Path,
    statements: &[String],
    timeout: Duration,
) -> Result<SqliteOutput, SourceError> {
    let mut command = Command::new(program);
    command
        .arg("-init")
        .arg(SQLITE_EMPTY_INIT)
        .arg("-readonly")
        .arg("-batch")
        .arg("-list")
        .arg("-noheader")
        .arg("-cmd")
        .arg(format!(".timeout {SQLITE_BUSY_TIMEOUT_MS}"))
        .arg(db)
        .args(statements)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::platform::configure_background_command(&mut command);
    let mut child = command.spawn().map_err(|error| {
        tracing::debug!(%error, program, "zcode: cannot start sqlite3");
        SourceError::Unavailable
    })?;
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        terminate(&mut child);
        return Err(SourceError::Unavailable);
    };
    // 读线程起不来（系统线程耗尽）同样只是暂不可读：先杀子进程，已起的读线程
    // 随管道关闭自然结束。
    let stdout = match spawn_capped_reader(stdout, MAX_QUERY_OUTPUT_BYTES) {
        Ok(handle) => handle,
        Err(error) => {
            tracing::debug!(%error, "zcode: cannot start the sqlite3 reader thread");
            terminate(&mut child);
            return Err(SourceError::Unavailable);
        }
    };
    let stderr = match spawn_capped_reader(stderr, MAX_STDERR_BYTES) {
        Ok(handle) => handle,
        Err(error) => {
            tracing::debug!(%error, "zcode: cannot start the sqlite3 reader thread");
            terminate(&mut child);
            let _ = join_reader(stdout);
            return Err(SourceError::Unavailable);
        }
    };
    let deadline = Instant::now() + timeout;
    let finished = loop {
        match child.try_wait() {
            Ok(Some(_)) => break true,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(SQLITE_POLL),
            Ok(None) | Err(_) => {
                terminate(&mut child);
                break false;
            }
        }
    };
    let stdout = join_reader(stdout);
    let stderr = join_reader(stderr);
    if !finished {
        tracing::debug!("zcode: sqlite3 timed out or could not be waited on");
        return Err(SourceError::Unavailable);
    }
    // 非零退出不一定致命：可选语句失败也会非零，交给哨兵判定。
    Ok(SqliteOutput { stdout, stderr })
}

fn terminate(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn spawn_capped_reader<R>(reader: R, cap: u64) -> std::io::Result<JoinHandle<Vec<u8>>>
where
    R: Read + Send + 'static,
{
    std::thread::Builder::new()
        .name("zcode-sqlite-output".to_string())
        .spawn(move || {
            let mut reader = reader;
            let mut kept = Vec::new();
            let _ = (&mut reader).take(cap).read_to_end(&mut kept);
            // 超出上限的部分照读照丢，子进程才不会卡在写满的管道上。
            let _ = std::io::copy(&mut reader, &mut std::io::sink());
            kept
        })
}

fn join_reader(handle: JoinHandle<Vec<u8>>) -> String {
    let bytes = handle.join().unwrap_or_default();
    String::from_utf8_lossy(&bytes).into_owned()
}

#[derive(Debug, Default)]
struct DbSnapshot {
    sessions: Vec<SessionRow>,
    sessions_ok: bool,
    todos: Vec<TodoRow>,
    todos_ok: bool,
    turns: HashMap<String, TurnStats>,
    turns_ok: bool,
    migration: Option<String>,
}

#[derive(Debug)]
struct SessionRow {
    id: String,
    root: String,
    depth: u64,
    parent: Option<String>,
    task_type: String,
    title: String,
    directory: Option<String>,
    created_ms: Option<u64>,
    updated_ms: Option<u64>,
}

#[derive(Debug)]
struct TodoRow {
    session: String,
    position: u64,
    content: String,
    status: String,
}

/// 一个会话的轮次统计。只看**最新一轮**的状态：更早的轮次若是崩溃留下的
/// running，不能把现在空闲的会话说成在跑。
#[derive(Debug, Default)]
struct TurnStats {
    last_ms: Option<u64>,
    last_status: Option<String>,
}

impl TurnStats {
    fn running(&self) -> bool {
        self.last_status.as_deref() == Some("running")
    }
}

/// 逐行解析查询输出；非 JSON 行（例如 sqlite3 的报错回显）与缺关键字段的行跳过。
fn parse_query_output(stdout: &str) -> DbSnapshot {
    let mut snapshot = DbSnapshot::default();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match value.get("t").and_then(Value::as_str) {
            Some("s") => snapshot.sessions.extend(session_row(&value)),
            Some("s_end") => snapshot.sessions_ok = true,
            Some("d") => snapshot.todos.extend(todo_row(&value)),
            Some("d_end") => snapshot.todos_ok = true,
            Some("u") => {
                if let Some(id) = safe_str(&value, "id") {
                    snapshot.turns.insert(
                        id.to_string(),
                        TurnStats {
                            last_ms: value.get("last").and_then(Value::as_u64),
                            last_status: value
                                .get("last_status")
                                .and_then(Value::as_str)
                                .map(str::to_string),
                        },
                    );
                }
            }
            Some("u_end") => snapshot.turns_ok = true,
            Some("v") => {
                snapshot.migration = value.get("id").and_then(Value::as_str).map(str::to_string);
            }
            _ => {}
        }
    }
    snapshot
}

fn safe_str<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| is_safe_id(text))
}

fn session_row(value: &Value) -> Option<SessionRow> {
    Some(SessionRow {
        id: safe_str(value, "id")?.to_string(),
        root: safe_str(value, "root")?.to_string(),
        depth: value.get("depth").and_then(Value::as_u64)?,
        parent: safe_str(value, "parent").map(str::to_string),
        task_type: value
            .get("task_type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        title: value
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        directory: value
            .get("directory")
            .and_then(Value::as_str)
            .filter(|dir| !dir.is_empty())
            .map(str::to_string),
        created_ms: value.get("created").and_then(Value::as_u64),
        updated_ms: value.get("updated").and_then(Value::as_u64),
    })
}

fn todo_row(value: &Value) -> Option<TodoRow> {
    Some(TodoRow {
        session: safe_str(value, "id")?.to_string(),
        position: value.get("pos").and_then(Value::as_u64)?,
        content: value.get("content").and_then(Value::as_str)?.to_string(),
        status: value
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    })
}

fn build_external_agents(
    snapshot: &DbSnapshot,
    agents: &Path,
    now_ms: u64,
) -> Vec<ExternalAgentInfo> {
    let mut children: HashMap<&str, Vec<&SessionRow>> = HashMap::new();
    for row in snapshot.sessions.iter().filter(|row| row.depth > 0) {
        children.entry(row.root.as_str()).or_default().push(row);
    }
    let mut out: Vec<ExternalAgentInfo> = snapshot
        .sessions
        .iter()
        .filter(|row| row.depth == 0 && row.id == row.root)
        .map(|root| {
            let rows = children.remove(root.id.as_str()).unwrap_or_default();
            external_agent(root, rows, snapshot, agents, now_ms)
        })
        .collect();
    out.sort_by(|a, b| {
        b.updated_at_ms
            .cmp(&a.updated_at_ms)
            .then_with(|| a.external_id.cmp(&b.external_id))
    });
    out
}

fn external_agent(
    root: &SessionRow,
    mut rows: Vec<&SessionRow>,
    snapshot: &DbSnapshot,
    agents: &Path,
    now_ms: u64,
) -> ExternalAgentInfo {
    rows.sort_by(|a, b| {
        a.created_ms
            .cmp(&b.created_ms)
            .then_with(|| a.id.cmp(&b.id))
    });
    let members: HashSet<&str> = rows.iter().map(|row| row.id.as_str()).collect();
    let root_turns = snapshot.turns.get(&root.id);
    let mut last_activity = latest([root.updated_ms, root_turns.and_then(|t| t.last_ms)]);
    let mut nodes = Vec::with_capacity(rows.len());
    let mut live_subagents: HashSet<&str> = HashSet::new();
    for row in &rows {
        let turns = snapshot.turns.get(&row.id);
        last_activity = last_activity.max(latest([row.updated_ms, turns.and_then(|t| t.last_ms)]));
        let parent_id = row
            .parent
            .as_deref()
            .filter(|parent| *parent != root.id && members.contains(parent))
            .map(str::to_string);
        let node = if row.task_type == SUBAGENT_TASK_TYPE {
            subagent_node(row, parent_id, agents, turns, now_ms)
        } else {
            other_session_node(row, parent_id, turns)
        };
        if node.status == AgentActivityStatus::Running && node.kind == AgentActivityKind::Subagent {
            live_subagents.insert(row.id.as_str());
        }
        nodes.push(node);
    }

    // 待办：根会话的全部，外加仍在跑的子 agent 的；已结束子 agent 的待办只是噪音。
    let mut todos: Vec<&TodoRow> = snapshot
        .todos
        .iter()
        .filter(|todo| todo.session == root.id || live_subagents.contains(todo.session.as_str()))
        .collect();
    todos.sort_by(|a, b| {
        (a.session != root.id, &a.session, a.position).cmp(&(
            b.session != root.id,
            &b.session,
            b.position,
        ))
    });
    nodes.extend(todos.into_iter().map(|todo| todo_node(todo, &root.id)));

    let working = (root_turns.is_some_and(TurnStats::running) || !live_subagents.is_empty())
        && now_ms.saturating_sub(last_activity) <= STALE_RUNNING_MS;
    ExternalAgentInfo {
        external_id: format!("{SOURCE_ID}:{}", root.id),
        source: SOURCE_ID.to_string(),
        agent_status: if working {
            AgentStatus::Working
        } else {
            AgentStatus::Idle
        },
        label: first_line(&root.title)
            .map(|title| clip_chars(title, LABEL_CHARS))
            .unwrap_or_else(|| ROOT_FALLBACK_LABEL.to_string()),
        readable: true,
        agent: Some(SOURCE_ID.to_string()),
        cwd: root.directory.clone(),
        updated_at_ms: (last_activity > 0).then_some(last_activity),
        activity: nodes,
    }
}

fn latest<const N: usize>(values: [Option<u64>; N]) -> u64 {
    values.into_iter().flatten().max().unwrap_or(0)
}

/// 旁支对话与未来新增的会话类型：种类未知，`agent_type` 记原始 `task_type`。
fn other_session_node(
    row: &SessionRow,
    parent_id: Option<String>,
    turns: Option<&TurnStats>,
) -> AgentActivityNode {
    AgentActivityNode {
        id: row.id.clone(),
        kind: AgentActivityKind::Unknown,
        label: first_line(&row.title)
            .map(|title| clip_chars(title, LABEL_CHARS))
            .unwrap_or_else(|| row.task_type.clone()),
        status: turns.map_or(AgentActivityStatus::Unknown, turn_status),
        parent_id,
        agent_type: (!row.task_type.is_empty()).then(|| row.task_type.clone()),
        content_ref: None,
        summary: None,
        started_at_ms: row.created_ms,
        ended_at_ms: None,
    }
}

fn subagent_node(
    row: &SessionRow,
    parent_id: Option<String>,
    agents: &Path,
    turns: Option<&TurnStats>,
    now_ms: u64,
) -> AgentActivityNode {
    let dir = subagent_dir(agents, row);
    let meta = dir.as_deref().map(read_metadata).unwrap_or_default();
    let mut status = match meta.status.as_deref() {
        Some(raw) => metadata_status(raw),
        None => turns.map_or(AgentActivityStatus::Unknown, turn_status),
    };
    let last_seen = latest([
        row.updated_ms,
        meta.updated_ms,
        turns.and_then(|turns| turns.last_ms),
    ]);
    let stale = status == AgentActivityStatus::Running
        && now_ms.saturating_sub(last_seen) > STALE_RUNNING_MS;
    if stale {
        status = AgentActivityStatus::Unknown;
    }
    let finished = matches!(
        status,
        AgentActivityStatus::Done | AgentActivityStatus::Failed
    );
    AgentActivityNode {
        id: row.id.clone(),
        kind: AgentActivityKind::Subagent,
        label: meta
            .description
            .as_deref()
            .and_then(first_line)
            .or_else(|| first_line(&row.title))
            .or(meta.profile_name.as_deref())
            .map(|label| clip_chars(label, LABEL_CHARS))
            .unwrap_or_else(|| SUBAGENT_FALLBACK_LABEL.to_string()),
        status,
        parent_id,
        agent_type: meta
            .profile_name
            .clone()
            .or_else(|| meta.profile_id.clone()),
        content_ref: dir
            .as_deref()
            .filter(|dir| has_content(dir))
            .map(|_| row.id.clone()),
        summary: if stale {
            Some("stale".to_string())
        } else {
            subagent_summary(&meta)
        },
        started_at_ms: meta.created_ms.or(row.created_ms),
        ended_at_ms: finished.then_some(meta.completed_ms).flatten(),
    }
}

fn todo_node(todo: &TodoRow, root_id: &str) -> AgentActivityNode {
    AgentActivityNode {
        id: format!("todo:{}:{}", todo.session, todo.position),
        kind: AgentActivityKind::Todo,
        label: first_line(&todo.content)
            .map(|content| clip_chars(content, TODO_CHARS))
            .unwrap_or_default(),
        status: match todo.status.as_str() {
            "pending" => AgentActivityStatus::Pending,
            "in_progress" => AgentActivityStatus::Running,
            "completed" => AgentActivityStatus::Done,
            _ => AgentActivityStatus::Unknown,
        },
        parent_id: (todo.session != root_id).then(|| todo.session.clone()),
        ..AgentActivityNode::default()
    }
}

fn metadata_status(raw: &str) -> AgentActivityStatus {
    match raw {
        "running" => AgentActivityStatus::Running,
        "completed" => AgentActivityStatus::Done,
        // stopped 是 0.16.9 后台 agent 被停的写法；没做完就按失败呈现，摘要注明原因。
        "failed" | "stopped" | "cancelled" | "killed" => AgentActivityStatus::Failed,
        "pending" | "queued" => AgentActivityStatus::Pending,
        _ => AgentActivityStatus::Unknown,
    }
}

fn turn_status(turns: &TurnStats) -> AgentActivityStatus {
    match turns.last_status.as_deref() {
        Some("running") => AgentActivityStatus::Running,
        Some("completed") => AgentActivityStatus::Done,
        Some("error" | "cancelled") => AgentActivityStatus::Failed,
        _ => AgentActivityStatus::Unknown,
    }
}

/// `agents/<父会话 id>/<agentId>/`；id 不合规则时没有目录可找。
fn subagent_dir(agents: &Path, row: &SessionRow) -> Option<PathBuf> {
    let agent_id = row
        .id
        .strip_prefix(SUBAGENT_SESSION_PREFIX)
        .filter(|id| is_safe_id(id))?;
    let parent = row.parent.as_deref()?;
    Some(agents.join(parent).join(agent_id))
}

fn has_content(dir: &Path) -> bool {
    dir.join(TRANSCRIPT_FILE).is_file() || dir.join(OUTPUT_FILE).is_file()
}

#[derive(Debug, Default)]
struct AgentMetadata {
    status: Option<String>,
    description: Option<String>,
    profile_name: Option<String>,
    profile_id: Option<String>,
    created_ms: Option<u64>,
    updated_ms: Option<u64>,
    completed_ms: Option<u64>,
    total_tokens: Option<u64>,
    tool_uses: Option<u64>,
    duration_ms: Option<u64>,
    error: Option<String>,
}

/// 读 metadata.json；缺文件、超大、坏 JSON 都退回空结构，字段类型不对只丢该字段。
fn read_metadata(dir: &Path) -> AgentMetadata {
    let path = dir.join(METADATA_FILE);
    let Ok(file) = File::open(&path) else {
        return AgentMetadata::default();
    };
    let mut text = String::new();
    if file
        .take(MAX_METADATA_BYTES + 1)
        .read_to_string(&mut text)
        .is_err()
        || text.len() as u64 > MAX_METADATA_BYTES
    {
        tracing::debug!(path = %path.display(), "zcode: metadata unreadable or too large");
        return AgentMetadata::default();
    }
    match serde_json::from_str::<Value>(&text) {
        Ok(value) => parse_metadata(&value),
        Err(error) => {
            tracing::debug!(path = %path.display(), %error, "zcode: metadata is not JSON");
            AgentMetadata::default()
        }
    }
}

fn parse_metadata(value: &Value) -> AgentMetadata {
    let text = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .filter(|text| !text.trim().is_empty())
            .map(str::to_string)
    };
    let number = |key: &str| value.get(key).and_then(Value::as_u64);
    let time = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .and_then(parse_rfc3339_ms)
    };
    let profile = value.get("profileSnapshot");
    AgentMetadata {
        status: text("status"),
        description: text("description"),
        profile_name: profile
            .and_then(|profile| profile.get("name"))
            .and_then(Value::as_str)
            .filter(|name| !name.trim().is_empty())
            .map(str::to_string),
        profile_id: text("profileId"),
        created_ms: time("createdAt"),
        updated_ms: time("updatedAt"),
        completed_ms: time("completedAt"),
        total_tokens: number("totalTokens").or_else(|| {
            value
                .get("usage")
                .and_then(|usage| usage.get("totalTokens"))
                .and_then(Value::as_u64)
        }),
        tool_uses: number("totalToolUseCount"),
        duration_ms: number("totalDurationMs"),
        error: match value.get("error") {
            Some(Value::String(error)) => Some(error.clone()),
            Some(Value::Object(error)) => error
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_string),
            _ => None,
        },
    }
}

fn parse_rfc3339_ms(text: &str) -> Option<u64> {
    let time =
        time::OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339).ok()?;
    u64::try_from(time.unix_timestamp_nanos() / 1_000_000).ok()
}

fn subagent_summary(meta: &AgentMetadata) -> Option<String> {
    match meta.status.as_deref() {
        Some("failed") => {
            return meta
                .error
                .as_deref()
                .and_then(first_line)
                .map(|error| clip_chars(error, SUMMARY_CHARS));
        }
        Some(raw @ ("stopped" | "cancelled" | "killed")) => return Some(raw.to_string()),
        _ => {}
    }
    let mut parts = Vec::new();
    if let Some(tokens) = meta.total_tokens {
        parts.push(format!("{} tokens", format_count(tokens)));
    }
    if let Some(tools) = meta.tool_uses {
        parts.push(format!("{tools} tools"));
    }
    if let Some(duration) = meta.duration_ms {
        parts.push(format_duration(duration));
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

fn format_count(value: u64) -> String {
    match value {
        0..=999 => value.to_string(),
        1_000..=999_999 => format!("{:.1}k", value as f64 / 1_000.0),
        _ => format!("{:.1}M", value as f64 / 1_000_000.0),
    }
}

fn format_duration(ms: u64) -> String {
    let seconds = ms / 1_000;
    match seconds {
        0 => format!("{ms}ms"),
        1..=59 => format!("{seconds}s"),
        60..=3_599 => format!("{}m {}s", seconds / 60, seconds % 60),
        _ => format!("{}h {}m", seconds / 3_600, (seconds % 3_600) / 60),
    }
}

fn first_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).find(|line| !line.is_empty())
}

fn clip_chars(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_string(),
    }
}

// ---------------------------------------------------------------------------
// 内容分页读取
// ---------------------------------------------------------------------------

/// 游标对调用方不透明：`t:<偏移>` 指转录，`o:<偏移>` 指 output.txt。
#[derive(Debug, PartialEq, Eq)]
enum Cursor {
    Start,
    Transcript(u64),
    Output(u64),
}

fn parse_cursor(cursor: Option<&str>) -> Result<Cursor, SourceError> {
    let Some(cursor) = cursor else {
        return Ok(Cursor::Start);
    };
    let malformed = || SourceError::Malformed(format!("unrecognized zcode cursor: {cursor}"));
    let (tag, offset) = cursor.split_once(':').ok_or_else(malformed)?;
    let offset = offset.parse::<u64>().map_err(|_| malformed())?;
    match tag {
        "t" => Ok(Cursor::Transcript(offset)),
        "o" => Ok(Cursor::Output(offset)),
        _ => Err(malformed()),
    }
}

fn read_node(
    agents: &Path,
    session_hint: Option<&str>,
    node_id: &str,
    cursor: Option<&str>,
    max_bytes: usize,
) -> Result<ContentChunk, SourceError> {
    let cursor = parse_cursor(cursor)?;
    // 只有子 agent 节点有内容；待办、旁支对话等节点本来就不提供读取。
    let Some(agent_id) = node_id.strip_prefix(SUBAGENT_SESSION_PREFIX) else {
        return Err(SourceError::Unsupported);
    };
    if !is_safe_id(agent_id) {
        return Err(SourceError::Malformed(format!(
            "invalid zcode node id: {node_id}"
        )));
    }
    // 形状对但找不到目录：已被清理，或 id 本就不存在。
    let Some(dir) = locate_agent_dir(agents, session_hint, agent_id) else {
        return Err(SourceError::Unavailable);
    };
    let budget = max_bytes.clamp(MIN_READ_BYTES, MAX_READ_BYTES);
    let transcript = dir.join(TRANSCRIPT_FILE);
    let output = dir.join(OUTPUT_FILE);
    match cursor {
        Cursor::Start if transcript.is_file() => read_transcript_page(&transcript, 0, budget),
        Cursor::Start if output.is_file() => read_text_page(&output, 0, budget),
        // 还没有任何输出（仍在跑，或当前版本不写转录且尚未结束）。两个文件都不在，
        // 给不出有意义的位置：下一次仍从头读。
        Cursor::Start => Ok(empty_chunk(AgentActivityContentFormat::Text, None)),
        Cursor::Transcript(offset) => read_transcript_page(&transcript, offset, budget),
        Cursor::Output(offset) => read_text_page(&output, offset, budget),
    }
}

/// 先按会话提示直取；嵌套子 agent 的目录挂在直接父会话下，外部条目只给得出根
/// 会话 → 再在 agents 下扫一层。
fn locate_agent_dir(agents: &Path, session_hint: Option<&str>, agent_id: &str) -> Option<PathBuf> {
    if let Some(session) = session_hint.filter(|session| is_safe_id(session)) {
        let dir = agents.join(session).join(agent_id);
        if dir.is_dir() {
            return Some(dir);
        }
    }
    std::fs::read_dir(agents)
        .ok()?
        .flatten()
        .map(|entry| entry.path().join(agent_id))
        .find(|dir| dir.is_dir())
}

/// 空页。`next_cursor` 由调用方给出：能定位就原样回传当前位置（供跟随续读，
/// 且不让游标倒退），只有连文件都不存在时才是 `None`。
fn empty_chunk(format: AgentActivityContentFormat, next_cursor: Option<String>) -> ContentChunk {
    ContentChunk {
        format,
        text: String::new(),
        next_cursor,
        eof: true,
        truncated: false,
    }
}

fn open_content(path: &Path) -> Result<(File, u64), SourceError> {
    let file = File::open(path).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => SourceError::Unavailable,
        _ => SourceError::Io(error),
    })?;
    let len = file.metadata().map_err(SourceError::Io)?.len();
    Ok((file, len))
}

fn read_transcript_page(
    path: &Path,
    offset: u64,
    budget: usize,
) -> Result<ContentChunk, SourceError> {
    let (file, len) = open_content(path)?;
    if offset >= len {
        return Ok(empty_chunk(
            AgentActivityContentFormat::Text,
            Some(format!("t:{offset}")),
        ));
    }
    let mut reader = BufReader::new(file);
    reader
        .seek(SeekFrom::Start(offset))
        .map_err(SourceError::Io)?;

    let mut position = offset;
    let mut scanned = 0_u64;
    let mut text = String::new();
    let mut truncated = false;
    let mut drained = false;
    let mut line = Vec::new();
    while text.len() < budget && scanned < SCAN_BUDGET_BYTES {
        let line_start = position;
        line.clear();
        let raw =
            read_line_capped(&mut reader, &mut line, LINE_KEEP_BYTES).map_err(SourceError::Io)?;
        if raw.consumed == 0 {
            drained = true;
            break;
        }
        if !raw.terminated {
            // 没有换行符结尾 = 这一行 ZCode 还在写。不消费：游标退回行首，下一次
            // 读到完整行再渲染，免得半截 JSON 被静默丢掉。
            position = line_start;
            drained = true;
            break;
        }
        position = position.saturating_add(raw.consumed as u64);
        scanned = scanned.saturating_add(raw.consumed as u64);
        let Some(entry) = render_transcript_line(&line, raw.overflow) else {
            continue;
        };
        let remaining = budget.saturating_sub(text.len());
        if entry.len() + 1 > remaining {
            if !text.is_empty() {
                // 本页装不下就整条留给下一页：游标退回行首，内容不丢。
                position = line_start;
                break;
            }
            // 单条本身超过整页预算，只能截到字符边界并标记。
            text.push_str(clip_bytes(&entry, budget.saturating_sub(5)));
            text.push_str(" …\n");
            truncated = true;
            break;
        }
        text.push_str(&entry);
        text.push('\n');
    }

    // `drained` 覆盖「半截末行不消费」这种 position 还没到 len 的读完。
    let eof = drained || position >= len;
    Ok(ContentChunk {
        format: AgentActivityContentFormat::Text,
        text,
        next_cursor: Some(format!("t:{position}")),
        eof,
        truncated,
    })
}

/// 读一行的结果：消耗的字节数、是否超长截断、是否读到了行尾换行符。
struct RawLine {
    consumed: usize,
    overflow: bool,
    terminated: bool,
}

/// 读一行（含换行符），只在 `out` 里留前 `cap` 字节。文件末尾没有换行符时
/// `terminated` 为 false，由调用方决定是否消费这半截行。
fn read_line_capped<R: BufRead>(
    reader: &mut R,
    out: &mut Vec<u8>,
    cap: usize,
) -> std::io::Result<RawLine> {
    let mut consumed = 0;
    let mut overflow = false;
    loop {
        let (used, done) = {
            let buf = reader.fill_buf()?;
            if buf.is_empty() {
                return Ok(RawLine {
                    consumed,
                    overflow,
                    terminated: false,
                });
            }
            let (chunk, done) = match buf.iter().position(|byte| *byte == b'\n') {
                Some(end) => (&buf[..=end], true),
                None => (buf, false),
            };
            let room = cap.saturating_sub(out.len());
            if chunk.len() > room {
                overflow = true;
            }
            out.extend_from_slice(&chunk[..chunk.len().min(room)]);
            (chunk.len(), done)
        };
        reader.consume(used);
        consumed += used;
        if done {
            return Ok(RawLine {
                consumed,
                overflow,
                terminated: true,
            });
        }
    }
}

/// 在行首嗅探顶层 `"type":"…"`（实测键序 id → sessionId → turnId → type，type 总在
/// 前 200 字节内）。只用来跳过已知的无文本事件，嗅错了最多多解析一行。
fn sniff_event_type(head: &[u8]) -> Option<&str> {
    const NEEDLE: &[u8] = b"\"type\":\"";
    let head = &head[..head.len().min(TYPE_SNIFF_BYTES)];
    let start = head
        .windows(NEEDLE.len())
        .position(|window| window == NEEDLE)?
        + NEEDLE.len();
    let rest = &head[start..];
    let end = rest.iter().position(|byte| *byte == b'"')?;
    std::str::from_utf8(&rest[..end]).ok()
}

/// 把一条转录事件折成可读文本；无文本事件、坏行与未知类型返回 `None`。
fn render_transcript_line(raw: &[u8], overflow: bool) -> Option<String> {
    let sniffed = sniff_event_type(raw);
    if sniffed.is_some_and(|kind| SKIPPED_EVENTS.contains(&kind)) {
        return None;
    }
    if overflow {
        // 超长行没留全，不解析；只对会产出文本的已知事件留个占位。
        return sniffed
            .filter(|kind| {
                matches!(
                    *kind,
                    "model_complete" | "turn_started" | "tool_call_scheduled"
                )
            })
            .map(|kind| format!("[{kind} entry too large to show]"));
    }
    let line = std::str::from_utf8(raw).ok()?.trim();
    if line.is_empty() {
        return None;
    }
    let value = serde_json::from_str::<Value>(line).ok()?;
    let payload = value.get("payload").unwrap_or(&Value::Null);
    let text = |key: &str| payload.get(key).and_then(Value::as_str).unwrap_or_default();
    let number = |key: &str| payload.get(key).and_then(Value::as_u64);
    match value.get("type").and_then(Value::as_str)? {
        "turn_started" => {
            let heading = match number("turnNumber") {
                Some(turn) => format!("── turn {turn} ──"),
                None => "── turn ──".to_string(),
            };
            let input = text("input").trim();
            Some(if input.is_empty() {
                heading
            } else {
                format!("{heading}\n{input}")
            })
        }
        "tool_call_scheduled" => {
            let name = Some(text("toolName").trim())
                .filter(|name| !name.is_empty())
                .unwrap_or("tool");
            Some(match tool_detail(payload.get("input")) {
                Some(detail) => format!("→ {name}: {detail}"),
                None => format!("→ {name}"),
            })
        }
        "tool_batch_complete" => {
            let errors = number("errorCount").unwrap_or(0);
            (errors > 0).then(|| {
                let total = errors + number("successCount").unwrap_or(0);
                format!("✗ {errors} of {total} tool calls failed")
            })
        }
        "model_complete" => {
            let content = text("content").trim();
            (!content.is_empty()).then(|| content.to_string())
        }
        "model_network_status" => {
            let problem = Some(text("reason"))
                .filter(|reason| !reason.is_empty())
                .or_else(|| Some(text("errorCode")).filter(|code| !code.is_empty()))?;
            Some(match (number("attempt"), number("maxAttempts")) {
                (Some(attempt), Some(max)) => {
                    format!("! model request retry: {problem} (attempt {attempt}/{max})")
                }
                _ => format!("! model request retry: {problem}"),
            })
        }
        "turn_complete" => {
            let mut parts = vec!["■ turn complete".to_string()];
            let result = text("resultType");
            if !result.is_empty() {
                parts.push(result.to_string());
            }
            if let Some(tools) = number("toolCallCount") {
                parts.push(format!("{tools} tool calls"));
            }
            if let Some(duration) = number("duration") {
                parts.push(format_duration(duration));
            }
            Some(parts.join(" · "))
        }
        _ => None,
    }
}

/// 工具输入里挑一个最能说明意图的字段。
fn tool_detail(input: Option<&Value>) -> Option<String> {
    const KEYS: &[&str] = &[
        "description",
        "command",
        "file_path",
        "pattern",
        "query",
        "url",
        "skill",
        "title",
        "prompt",
    ];
    let input = input?;
    let detail = KEYS
        .iter()
        .find_map(|key| input.get(*key).and_then(Value::as_str).and_then(first_line));
    match detail {
        Some(detail) => Some(clip_chars(detail, TOOL_DETAIL_CHARS)),
        None => input
            .get("todos")
            .and_then(Value::as_array)
            .map(|todos| format!("{} items", todos.len())),
    }
}

fn clip_bytes(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut cut = max_bytes;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    &text[..cut]
}

/// output.txt 按字节分页，切口落在 UTF-8 字符边界上。
fn read_text_page(path: &Path, offset: u64, budget: usize) -> Result<ContentChunk, SourceError> {
    let (mut file, len) = open_content(path)?;
    if offset >= len {
        return Ok(empty_chunk(
            AgentActivityContentFormat::Markdown,
            Some(format!("o:{offset}")),
        ));
    }
    file.seek(SeekFrom::Start(offset))
        .map_err(SourceError::Io)?;
    let mut buf = Vec::with_capacity(budget.min(usize::try_from(len - offset).unwrap_or(budget)));
    file.take(budget as u64)
        .read_to_end(&mut buf)
        .map_err(SourceError::Io)?;
    let cut = match std::str::from_utf8(&buf) {
        Ok(_) => buf.len(),
        // 末尾只差半个字符：留给下一页。
        Err(error) if error.error_len().is_none() && error.valid_up_to() > 0 => error.valid_up_to(),
        // 真正的坏字节（或游标落在字符中间）：按替换字符吞下，保证前进。
        Err(_) => buf.len(),
    };
    let text = String::from_utf8_lossy(&buf[..cut]).into_owned();
    let position = offset.saturating_add(cut as u64);
    let eof = position >= len;
    Ok(ContentChunk {
        format: AgentActivityContentFormat::Markdown,
        text,
        next_cursor: Some(format!("o:{position}")),
        eof,
        truncated: false,
    })
}

#[cfg(test)]
mod tests {
    //! 夹具在 `tests/fixtures/agent-activity/zcode/`：`db.sql` 是按本机 schema 手写的
    //! 脱敏库，`query-output.jsonl` 是对它跑 `tree_statements(NOW)` 的真实输出（让
    //! 没有 sqlite3 的构建机也能测建树逻辑），`home/.zcode/cli/agents/**` 是
    //! metadata.json / transcript.jsonl / output.txt 样本。

    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::agent_resume::AgentSessionRef;

    /// 夹具时间基准：2026-09-21T14:13:20Z。
    const NOW: u64 = 1_790_000_000_000;
    const ROOT_A: &str = "sess_a0000000-0000-4000-8000-000000000001";
    const ROOT_B: &str = "sess_b0000000-0000-4000-8000-000000000002";
    const SIDE_CHAT: &str = "sess_e0000000-0000-4000-8000-000000000005";
    const FUTURE_STEP: &str = "sess_f0000000-0000-4000-8000-000000000006";

    /// `sub("01")` → `sess_subagent_agent_10000000-0000-4000-8000-000000000001`。
    fn sub(suffix: &str) -> String {
        format!("sess_subagent_agent_10000000-0000-4000-8000-0000000000{suffix}")
    }

    fn fixture_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/agent-activity/zcode")
    }

    fn fixture_home() -> PathBuf {
        fixture_dir().join("home")
    }

    fn fixture_agents() -> PathBuf {
        agents_dir(&fixture_home())
    }

    fn transcript_path(parent: &str, suffix: &str) -> PathBuf {
        fixture_agents()
            .join(parent)
            .join(format!("agent_10000000-0000-4000-8000-0000000000{suffix}"))
            .join(TRANSCRIPT_FILE)
    }

    fn recorded_agents() -> Vec<ExternalAgentInfo> {
        let text = std::fs::read_to_string(fixture_dir().join("query-output.jsonl"))
            .expect("读取录制的查询输出");
        let snapshot = parse_query_output(&text);
        assert!(snapshot.sessions_ok && snapshot.todos_ok && snapshot.turns_ok);
        build_external_agents(&snapshot, &fixture_agents(), NOW)
    }

    fn agent<'a>(agents: &'a [ExternalAgentInfo], root: &str) -> &'a ExternalAgentInfo {
        let external_id = format!("zcode:{root}");
        agents
            .iter()
            .find(|agent| agent.external_id == external_id)
            .unwrap_or_else(|| panic!("缺少外部条目 {external_id}"))
    }

    fn node<'a>(nodes: &'a [AgentActivityNode], id: &str) -> &'a AgentActivityNode {
        nodes
            .iter()
            .find(|node| node.id == id)
            .unwrap_or_else(|| panic!("缺少节点 {id}"))
    }

    fn context<'a>(home: &'a Path, session: Option<&'a AgentSessionRef>) -> SourceContext<'a> {
        SourceContext {
            agent: "zcode",
            session,
            cwd: None,
            home,
            now_ms: NOW,
        }
    }

    /// 测试用临时目录，析构时删除。
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "herdr-zcode-{tag}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("建临时目录");
            Self(dir)
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

    /// 端到端用例要真实的 sqlite3。Linux / macOS 构建机都自带；只有 Windows 构建机
    /// 允许缺省跳过，其余平台缺它直接失败，免得整族静默跳过后报绿。
    fn sqlite3_available() -> bool {
        let available = Command::new(SQLITE_PROGRAM)
            .arg("-version")
            .output()
            .is_ok_and(|output| output.status.success());
        if !available {
            if !cfg!(windows) {
                panic!("sqlite3 不在 PATH 上：zcode 端到端用例需要它");
            }
            eprintln!("跳过：Windows 构建机没有 sqlite3");
        }
        available
    }

    fn build_db(dir: &Path, sql: &str) -> PathBuf {
        let db = dir.join("db.sqlite");
        let mut child = Command::new(SQLITE_PROGRAM)
            .arg(&db)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("启动 sqlite3");
        // 一个事务、不落盘同步：机器忙时逐条 fsync 能拖到秒级。
        let script = format!("PRAGMA synchronous = OFF;\nBEGIN;\n{sql}\nCOMMIT;\n");
        child
            .stdin
            .take()
            .expect("sqlite3 stdin")
            .write_all(script.as_bytes())
            .expect("写入建库 SQL");
        let output = child.wait_with_output().expect("等待 sqlite3");
        assert!(
            output.status.success(),
            "建库失败：{}",
            String::from_utf8_lossy(&output.stderr)
        );
        db
    }

    fn fixture_sql() -> String {
        std::fs::read_to_string(fixture_dir().join("db.sql")).expect("读取夹具 SQL")
    }

    // --- 发现 ---------------------------------------------------------------

    #[test]
    fn pane_discovery_is_unsupported() {
        let home = fixture_home();
        assert!(matches!(
            ZCode.discover(&context(&home, None)),
            Err(SourceError::Unsupported)
        ));
    }

    #[test]
    fn recent_root_sessions_become_external_agents() {
        let agents = recorded_agents();
        let ids: Vec<&str> = agents.iter().map(|a| a.external_id.as_str()).collect();
        // 5 天前的 OLD 与已归档的根不在窗口里；A 比 B 新，排前面。
        assert_eq!(ids, [format!("zcode:{ROOT_A}"), format!("zcode:{ROOT_B}")]);

        let a = agent(&agents, ROOT_A);
        assert_eq!(a.source, "zcode");
        assert_eq!(a.agent.as_deref(), Some("zcode"));
        assert_eq!(a.label, "Refactor the parser");
        assert_eq!(a.cwd.as_deref(), Some("/work/demo"));
        assert!(a.readable);
        // 最后一轮 running 且刚有动静 → 在跑。
        assert_eq!(a.agent_status, AgentStatus::Working);
        // 取整棵树里最新的动静：S3 会话 NOW - 30 s。
        assert_eq!(a.updated_at_ms, Some(NOW - 30_000));

        let b = agent(&agents, ROOT_B);
        assert_eq!(b.label, ROOT_FALLBACK_LABEL, "空标题退回兜底文案");
        assert_eq!(b.cwd.as_deref(), Some("/work/other"));
    }

    #[test]
    fn subagents_carry_metadata_and_nest_to_any_depth() {
        let agents = recorded_agents();
        let nodes = &agent(&agents, ROOT_A).activity;

        let s1 = node(nodes, &sub("01"));
        assert_eq!(s1.kind, AgentActivityKind::Subagent);
        assert_eq!(s1.label, "Map parser entry points");
        assert_eq!(s1.agent_type.as_deref(), Some("Explore"));
        assert_eq!(s1.status, AgentActivityStatus::Done);
        assert_eq!(s1.parent_id, None, "直属根会话的节点是顶层");
        assert_eq!(s1.content_ref.as_deref(), Some(sub("01").as_str()));
        assert_eq!(
            s1.summary.as_deref(),
            Some("12.3k tokens · 8 tools · 10m 0s")
        );
        assert_eq!(s1.started_at_ms, Some(NOW - 3_000_000));
        assert_eq!(s1.ended_at_ms, Some(NOW - 2_400_000));

        // 三层嵌套：S1 → S8 → S9，目录分别挂在各自的直接父会话下。
        let s8 = node(nodes, &sub("08"));
        assert_eq!(s8.parent_id.as_deref(), Some(sub("01").as_str()));
        assert_eq!(s8.status, AgentActivityStatus::Done);
        assert_eq!(s8.summary.as_deref(), Some("950 tokens · 3 tools · 1m 40s"));
        assert!(s8.content_ref.is_some());
        let s9 = node(nodes, &sub("09"));
        assert_eq!(s9.parent_id.as_deref(), Some(sub("08").as_str()));
        assert_eq!(s9.agent_type.as_deref(), Some("flash"));
        assert_eq!(s9.summary.as_deref(), Some("30s"));
        assert_eq!(s9.content_ref, None, "只有 metadata、没有内容文件");

        let s2 = node(nodes, &sub("02"));
        assert_eq!(s2.status, AgentActivityStatus::Failed);
        assert_eq!(s2.summary.as_deref(), Some("Model provider returned 429"));
        assert_eq!(s2.ended_at_ms, Some(NOW - 2_800_000));

        let s3 = node(nodes, &sub("03"));
        assert_eq!(s3.status, AgentActivityStatus::Running);
        assert_eq!(s3.ended_at_ms, None);
        assert_eq!(s3.content_ref, None, "仍在跑、还没有输出");

        // 0.16.9 的 stopped 写法：缺 createdAt / profileSnapshot。
        let s7 = node(nodes, &sub("07"));
        assert_eq!(s7.status, AgentActivityStatus::Failed);
        assert_eq!(s7.summary.as_deref(), Some("stopped"));
        assert_eq!(s7.label, "Watch the build");
        assert_eq!(s7.agent_type.as_deref(), Some("general-purpose"));
        assert_eq!(s7.started_at_ms, Some(NOW - 1_600_000), "退回 DB 创建时间");
        assert_eq!(s7.ended_at_ms, Some(NOW - 1_500_000));

        // 会话节点按创建时间排，待办跟在后面。
        let order: Vec<&str> = nodes.iter().map(|node| node.id.as_str()).collect();
        let expected_sessions = [
            sub("01"),
            sub("02"),
            sub("08"),
            sub("09"),
            sub("04"),
            sub("05"),
            sub("06"),
            sub("07"),
            SIDE_CHAT.to_string(),
            FUTURE_STEP.to_string(),
            sub("03"),
        ];
        assert_eq!(order[..expected_sessions.len()], expected_sessions);
    }

    #[test]
    fn missing_or_broken_metadata_only_degrades_that_node() {
        let agents = recorded_agents();
        let nodes = &agent(&agents, ROOT_A).activity;

        // S4 没有目录：标题与状态退回 DB（最后一轮 completed）。
        let s4 = node(nodes, &sub("04"));
        assert_eq!(s4.kind, AgentActivityKind::Subagent);
        assert_eq!(s4.label, "Check lints");
        assert_eq!(s4.status, AgentActivityStatus::Done);
        assert_eq!(s4.agent_type, None);
        assert_eq!(s4.content_ref, None);
        assert_eq!(s4.summary, None);
        assert_eq!(s4.started_at_ms, Some(NOW - 2_000_000));

        // S5 缺字段、未知状态、字段类型不对：只丢这些项。
        let s5 = node(nodes, &sub("05"));
        assert_eq!(s5.status, AgentActivityStatus::Unknown);
        assert_eq!(s5.label, "Draft summary");
        assert_eq!(s5.agent_type, None);
        assert_eq!(s5.summary, None);

        // S6 metadata 是坏 JSON：节点照样出现。
        let s6 = node(nodes, &sub("06"));
        assert_eq!(s6.label, "Inspect fixtures");
        assert_eq!(s6.status, AgentActivityStatus::Unknown);
        assert_eq!(s6.content_ref, None);
    }

    #[test]
    fn unknown_session_types_and_todo_states_fall_back_to_unknown() {
        let agents = recorded_agents();
        let nodes = &agent(&agents, ROOT_A).activity;

        let side = node(nodes, SIDE_CHAT);
        assert_eq!(side.kind, AgentActivityKind::Unknown);
        assert_eq!(side.agent_type.as_deref(), Some("selection_side_chat"));
        assert_eq!(side.label, "Explain this selection");
        assert_eq!(side.parent_id, None);
        let future = node(nodes, FUTURE_STEP);
        assert_eq!(future.kind, AgentActivityKind::Unknown);
        assert_eq!(future.agent_type.as_deref(), Some("workflow_step"));

        let todos: Vec<(&str, AgentActivityStatus, Option<&str>)> = nodes
            .iter()
            .filter(|node| node.kind == AgentActivityKind::Todo)
            .map(|node| (node.label.as_str(), node.status, node.parent_id.as_deref()))
            .collect();
        let s3 = sub("03");
        assert_eq!(
            todos,
            [
                (
                    "Map the parser entry points",
                    AgentActivityStatus::Done,
                    None
                ),
                ("Split the tokenizer", AgentActivityStatus::Running, None),
                ("Add regression tests", AgentActivityStatus::Pending, None),
                ("Tidy the docs", AgentActivityStatus::Unknown, None),
                // 仍在跑的 S3 的待办挂在它下面；已完成的 S1 的待办不列。
                (
                    "Scan call sites",
                    AgentActivityStatus::Running,
                    Some(s3.as_str())
                ),
            ]
        );
        assert!(nodes
            .iter()
            .any(|node| node.id == format!("todo:{ROOT_A}:1")));
    }

    #[test]
    fn stale_running_is_not_reported_as_working() {
        let agents = recorded_agents();
        let b = agent(&agents, ROOT_B);
        assert_eq!(
            b.agent_status,
            AgentStatus::Idle,
            "5 小时前崩掉的 running 轮次"
        );
        let s10 = node(&b.activity, &sub("0a"));
        assert_eq!(s10.status, AgentActivityStatus::Unknown);
        assert_eq!(s10.summary.as_deref(), Some("stale"));

        // 同一份快照放回当时的时刻：两者都还算活着。
        let text = std::fs::read_to_string(fixture_dir().join("query-output.jsonl"))
            .expect("读取录制的查询输出");
        let then = NOW - 18_000_000 + 60_000;
        let agents = build_external_agents(&parse_query_output(&text), &fixture_agents(), then);
        let b = agent(&agents, ROOT_B);
        assert_eq!(b.agent_status, AgentStatus::Working);
        assert_eq!(
            node(&b.activity, &sub("0a")).status,
            AgentActivityStatus::Running
        );
    }

    #[test]
    fn query_output_parsing_skips_noise_and_needs_the_session_sentinel() {
        let text = [
            "Error: stray diagnostics echoed by an old sqlite3",
            "",
            "{\"t\":\"s\",\"id\":\"../../etc\",\"root\":\"x\",\"depth\":0}",
            "{\"t\":\"s\",\"id\":\"sess_ok\",\"root\":\"sess_ok\",\"depth\":\"zero\"}",
            "{\"t\":\"s\",\"id\":\"sess_ok\",\"root\":\"sess_ok\",\"depth\":0,\"title\":7}",
            "[1,2,3]",
            "{\"t\":\"d\",\"id\":\"sess_ok\",\"pos\":0}",
            "{\"t\":\"u\",\"id\":\"sess_ok\",\"last_status\":\"running\"}",
            "{\"t\":\"u\",\"id\":\"sess_done\",\"last_status\":\"completed\",\"last\":\"x\"}",
            "{\"t\":\"mystery\"}",
        ]
        .join("\n");
        let snapshot = parse_query_output(&text);
        assert_eq!(snapshot.sessions.len(), 1, "坏 id、坏 depth 的行丢掉");
        assert_eq!(snapshot.sessions[0].title, "", "字段类型不对只丢该字段");
        assert!(snapshot.todos.is_empty(), "缺 content 的待办丢掉");
        assert!(snapshot
            .turns
            .get("sess_ok")
            .is_some_and(TurnStats::running));
        let done = snapshot.turns.get("sess_done").expect("轮次行保留");
        assert!(!done.running());
        assert_eq!(done.last_ms, None, "类型不对的时间只丢该字段");
        assert_eq!(turn_status(done), AgentActivityStatus::Done);
        assert!(!snapshot.sessions_ok && !snapshot.todos_ok && !snapshot.turns_ok);
        assert_eq!(snapshot.migration, None);

        let text = format!("{text}\n{{\"t\":\"s_end\"}}\n{{\"t\":\"v\",\"id\":\"0099_future\"}}");
        let snapshot = parse_query_output(&text);
        assert!(snapshot.sessions_ok);
        assert_eq!(
            snapshot.migration.as_deref().and_then(migration_number),
            Some(99)
        );
    }

    #[test]
    fn without_a_database_there_are_no_external_agents() {
        let home = TempDir::new("no-db");
        assert!(ZCode
            .discover_external(home.path(), NOW)
            .expect("没装 ZCode 不算不可读")
            .is_empty());
    }

    #[test]
    fn a_missing_sqlite3_makes_the_source_unavailable() {
        let dir = TempDir::new("no-sqlite");
        let db = dir.path().join("db.sqlite");
        std::fs::write(&db, b"not really a database").expect("写假库");
        assert!(matches!(
            discover_sessions(&db, &fixture_agents(), "herdr-test-no-such-sqlite3", NOW),
            Err(SourceError::Unavailable)
        ));
    }

    /// 卡住的 sqlite3 到点就被杀掉、读线程随之收尾，不会拖住后台线程。
    #[cfg(unix)]
    #[test]
    fn a_hung_sqlite3_is_killed_at_the_deadline() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new("hung");
        let program = dir.path().join("fake-sqlite3");
        // exec：不留孙进程攥着管道。
        std::fs::write(&program, "#!/bin/sh\nexec sleep 30\n").expect("写假 sqlite3");
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))
            .expect("设可执行位");
        let started = Instant::now();
        let result = run_sqlite(
            program.to_str().expect("临时路径是 UTF-8"),
            &dir.path().join("db.sqlite"),
            &[],
            Duration::from_millis(200),
        );
        assert!(matches!(result, Err(SourceError::Unavailable)));
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "超时后必须立刻收尾：{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn the_fixture_database_yields_the_recorded_tree() {
        if !sqlite3_available() {
            return;
        }
        let dir = TempDir::new("fixture-db");
        let db = build_db(dir.path(), &fixture_sql());
        let found =
            discover_sessions(&db, &fixture_agents(), SQLITE_PROGRAM, NOW).expect("夹具库可读");
        assert_eq!(found, recorded_agents());

        // 本机没核实过的新迁移：照常解析，不报错。
        let db = build_db(
            dir.path(),
            "INSERT INTO schema_migration VALUES ('0099_future_change', 'x', '9.9.9', 1);",
        );
        let found =
            discover_sessions(&db, &fixture_agents(), SQLITE_PROGRAM, NOW).expect("新迁移只降级");
        assert_eq!(found, recorded_agents());
    }

    #[test]
    fn missing_optional_tables_only_degrade() {
        if !sqlite3_available() {
            return;
        }
        let dir = TempDir::new("optional-tables");
        let db = build_db(
            dir.path(),
            &format!(
                "CREATE TABLE session (id text primary key, parent_id text, directory text not null, \
                 title text not null, time_created integer not null, time_updated integer not null, \
                 time_archived integer, task_type text not null default 'interactive');
                 CREATE INDEX session_parent_idx on session(parent_id);
                 INSERT INTO session VALUES ('{ROOT_A}', NULL, '/work/demo', 'Only sessions', {}, {}, NULL, 'interactive');
                 INSERT INTO session VALUES ('{}', '{ROOT_A}', '/work/demo', 'Explore: map parser', {}, {}, NULL, 'subagent_child');",
                NOW - 1_000,
                NOW - 1_000,
                sub("01"),
                NOW - 900,
                NOW - 800,
            ),
        );
        let found = discover_sessions(&db, &fixture_agents(), SQLITE_PROGRAM, NOW)
            .expect("缺 todo / turn_usage / schema_migration 只降级");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].label, "Only sessions");
        assert_eq!(found[0].agent_status, AgentStatus::Idle);
        assert_eq!(found[0].activity.len(), 1);
        // metadata 仍按目录读到（夹具 agents 里的 S1）。
        assert_eq!(found[0].activity[0].status, AgentActivityStatus::Done);
    }

    #[test]
    fn an_unrecognized_schema_or_corrupt_file_is_unavailable() {
        if !sqlite3_available() {
            return;
        }
        let dir = TempDir::new("bad-schema");
        let db = build_db(
            dir.path(),
            "CREATE TABLE session (id text primary key, parent_id text, title text);",
        );
        assert!(matches!(
            discover_sessions(&db, &fixture_agents(), SQLITE_PROGRAM, NOW),
            Err(SourceError::Unavailable)
        ));

        let corrupt = dir.path().join("corrupt.sqlite");
        std::fs::write(&corrupt, [0x5a_u8; 8192]).expect("写坏文件");
        assert!(matches!(
            discover_sessions(&corrupt, &fixture_agents(), SQLITE_PROGRAM, NOW),
            Err(SourceError::Unavailable)
        ));
    }

    // --- 内容读取 -------------------------------------------------------------

    const S1_TRANSCRIPT_TEXT: &str = "── turn 1 ──\n\
        Find where the parser is constructed.\n\
        ! model request retry: rate_limited (attempt 1/3)\n\
        → Bash: Search parser\n\
        → Read: src/parser.rs\n\
        ✗ 1 of 2 tool calls failed\n\
        The parser is built in src/parser.rs::Parser::new.\n\
        ■ turn complete · success · 2 tool calls · 10m 0s\n";

    #[test]
    fn read_renders_the_transcript_and_skips_noise() {
        let home = fixture_home();
        let session = AgentSessionRef::id(format!("zcode:{ROOT_A}")).expect("合法会话引用");
        let chunk = ZCode
            .read(&context(&home, Some(&session)), &sub("01"), None, 64 * 1024)
            .expect("可读");
        assert_eq!(chunk.format, AgentActivityContentFormat::Text);
        assert_eq!(chunk.text, S1_TRANSCRIPT_TEXT);
        assert!(chunk.eof);
        assert!(!chunk.truncated);
        // eof 只表示「暂时读完」：游标仍指向当前末尾，供二级窗口跟随续读。
        let len = std::fs::metadata(transcript_path(ROOT_A, "01"))
            .expect("转录可读")
            .len();
        let at_end = format!("t:{len}");
        assert_eq!(chunk.next_cursor.as_deref(), Some(at_end.as_str()));
        // 拿它原地再读一次：空页，游标不倒退也不前进。
        let again = ZCode
            .read(
                &context(&home, Some(&session)),
                &sub("01"),
                Some(&at_end),
                64 * 1024,
            )
            .expect("原地续读");
        assert!(again.text.is_empty() && again.eof);
        assert_eq!(again.next_cursor.as_deref(), Some(at_end.as_str()));
    }

    #[test]
    fn read_finds_the_node_with_any_session_hint() {
        let home = fixture_home();
        let bare = AgentSessionRef::id(ROOT_A).expect("合法会话引用");
        let unrelated = AgentSessionRef::id(ROOT_B).expect("合法会话引用");
        for session in [None, Some(&bare), Some(&unrelated)] {
            let chunk = ZCode
                .read(&context(&home, session), &sub("01"), None, 64 * 1024)
                .expect("扫目录也能找到");
            assert_eq!(chunk.text, S1_TRANSCRIPT_TEXT);
        }
        // 嵌套节点挂在 S1 的会话目录下，只凭根会话提示也要找得到。
        let chunk = ZCode
            .read(&context(&home, Some(&bare)), &sub("08"), None, 64 * 1024)
            .expect("嵌套节点可读");
        assert_eq!(chunk.format, AgentActivityContentFormat::Markdown);
        assert_eq!(
            chunk.text,
            "The tokenizer is driven by Lexer::next_token.\n"
        );
        assert!(chunk.eof);
    }

    #[test]
    fn transcript_pages_concatenate_without_repeats() {
        let path = transcript_path(ROOT_A, "01");
        let mut pages = Vec::new();
        let mut offset = 0;
        loop {
            let chunk = read_transcript_page(&path, offset, 60).expect("分页可读");
            assert!(!chunk.truncated, "每条都放得进 60 字节的页");
            assert!(chunk.text.len() <= 60);
            pages.push(chunk.text);
            if chunk.eof {
                // eof 也给游标：位置停在已消费的末尾。
                let Ok(Cursor::Transcript(next)) = parse_cursor(chunk.next_cursor.as_deref())
                else {
                    panic!("eof 也要给出转录游标：{:?}", chunk.next_cursor);
                };
                assert_eq!(next, std::fs::metadata(&path).expect("转录可读").len());
                break;
            }
            match parse_cursor(chunk.next_cursor.as_deref()).expect("游标可解析") {
                Cursor::Transcript(next) => {
                    assert!(next > offset, "游标必须前进");
                    offset = next;
                }
                other => panic!("转录游标类型不对：{other:?}"),
            }
        }
        assert!(pages.len() > 4, "应分成多页：{pages:?}");
        assert_eq!(pages.concat(), S1_TRANSCRIPT_TEXT);

        // 走 trait 时太小的预算会被抬到下限，仍然分页。
        let home = fixture_home();
        let chunk = ZCode
            .read(&context(&home, None), &sub("01"), None, 1)
            .expect("可读");
        assert!(!chunk.eof && chunk.text.len() <= MIN_READ_BYTES);
        assert!(chunk
            .next_cursor
            .as_deref()
            .is_some_and(|c| c.starts_with("t:")));
    }

    #[test]
    fn output_pages_split_on_char_boundaries() {
        let path = fixture_agents()
            .join(ROOT_A)
            .join("agent_10000000-0000-4000-8000-000000000001")
            .join(OUTPUT_FILE);
        let expected = std::fs::read_to_string(&path).expect("读取 output.txt");
        let mut pages = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let offset = match parse_cursor(cursor.as_deref()).expect("游标可解析") {
                Cursor::Start => 0,
                Cursor::Output(offset) => offset,
                other => panic!("output 游标类型不对：{other:?}"),
            };
            // 7 字节的页必然切到三字节汉字中间。
            let chunk = read_text_page(&path, offset, 7).expect("分页可读");
            assert_eq!(chunk.format, AgentActivityContentFormat::Markdown);
            assert!(!chunk.text.contains('\u{fffd}'), "不得切坏字符");
            pages.push(chunk.text);
            if chunk.eof {
                break;
            }
            cursor = chunk.next_cursor;
        }
        assert_eq!(pages.concat(), expected);

        // 从中途的 output 游标续读。
        let home = fixture_home();
        let chunk = ZCode
            .read(&context(&home, None), &sub("01"), Some("o:2"), 4096)
            .expect("按游标续读");
        assert_eq!(chunk.text, expected[2..]);
    }

    #[test]
    fn a_growing_file_is_followed_from_the_cursor_returned_at_eof() {
        let dir = TempDir::new("follow");
        let path = dir.path().join(TRANSCRIPT_FILE);
        std::fs::write(
            &path,
            transcript_line("model_complete", "{\"content\":\"first\"}"),
        )
        .expect("写转录");

        let first = read_transcript_page(&path, 0, 4096).expect("可读");
        assert_eq!(first.text, "first\n");
        assert!(first.eof, "暂时读完");
        let Ok(Cursor::Transcript(offset)) = parse_cursor(first.next_cursor.as_deref()) else {
            panic!("eof 也要给出转录游标：{:?}", first.next_cursor);
        };

        // 子 agent 还在跑，转录又长了一条：拿上一轮游标续读，只看到新增内容。
        let mut file = File::options().append(true).open(&path).expect("追加打开");
        file.write_all(transcript_line("model_complete", "{\"content\":\"second\"}").as_bytes())
            .expect("追加转录");
        drop(file);
        let second = read_transcript_page(&path, offset, 4096).expect("续读");
        assert_eq!(second.text, "second\n", "不重复也不丢");
        assert!(second.eof);

        // output.txt 同理。
        let output = dir.path().join(OUTPUT_FILE);
        std::fs::write(&output, "alpha\n").expect("写输出");
        let out_first = read_text_page(&output, 0, 4096).expect("可读");
        assert_eq!(out_first.text, "alpha\n");
        assert!(out_first.eof);
        let Ok(Cursor::Output(out_offset)) = parse_cursor(out_first.next_cursor.as_deref()) else {
            panic!("eof 也要给出输出游标：{:?}", out_first.next_cursor);
        };
        let mut file = File::options()
            .append(true)
            .open(&output)
            .expect("追加打开");
        file.write_all(b"beta\n").expect("追加输出");
        drop(file);
        let out_second = read_text_page(&output, out_offset, 4096).expect("续读");
        assert_eq!(out_second.text, "beta\n");
        assert!(out_second.eof);
    }

    #[test]
    fn an_unterminated_final_line_is_left_for_the_next_read() {
        let dir = TempDir::new("partial-line");
        let path = dir.path().join(TRANSCRIPT_FILE);
        let complete = transcript_line("model_complete", "{\"content\":\"done\"}");
        let pending = transcript_line("model_complete", "{\"content\":\"still writing\"}");
        // 读取正好撞上 ZCode 写到一半的那一行（全 ASCII，随便切）。
        let (head, tail) = pending.split_at(pending.len() / 2);
        std::fs::write(&path, format!("{complete}{head}")).expect("写半截转录");

        let page = read_transcript_page(&path, 0, 4096).expect("可读");
        assert_eq!(page.text, "done\n", "半截行既不渲染也不消费");
        assert!(page.eof, "暂时读完");
        let at_line_start = format!("t:{}", complete.len());
        assert_eq!(page.next_cursor.as_deref(), Some(at_line_start.as_str()));

        // 补齐后按该游标续读：完整条目出现一次。
        let mut file = File::options().append(true).open(&path).expect("追加打开");
        file.write_all(tail.as_bytes()).expect("补齐半截行");
        drop(file);
        let Ok(Cursor::Transcript(offset)) = parse_cursor(page.next_cursor.as_deref()) else {
            panic!("游标可解析：{:?}", page.next_cursor);
        };
        let resumed = read_transcript_page(&path, offset, 4096).expect("续读");
        assert_eq!(resumed.text, "still writing\n");
        assert!(resumed.eof);
    }

    #[test]
    fn nodes_without_content_and_bad_requests_degrade() {
        let home = fixture_home();
        let cx = context(&home, None);
        // 仍在跑、还没有任何输出：空页而不是报错。两个文件都不在，给不出位置。
        let chunk = ZCode.read(&cx, &sub("03"), None, 4096).expect("空内容可读");
        assert!(chunk.text.is_empty() && chunk.eof && chunk.next_cursor.is_none());
        // 游标越过文件末尾：空页，游标原样回传，不倒退。
        let chunk = ZCode
            .read(&cx, &sub("01"), Some("t:999999"), 4096)
            .expect("越界游标");
        assert!(chunk.text.is_empty() && chunk.eof);
        assert_eq!(chunk.next_cursor.as_deref(), Some("t:999999"));

        // 子 agent 形状的 id 但目录不在：没有目录（S4）或本就不存在。
        for node_id in [
            sub("04"),
            "sess_subagent_agent_ffffffff-0000-4000-8000-000000000000".to_string(),
        ] {
            assert!(
                matches!(
                    ZCode.read(&cx, &node_id, None, 4096),
                    Err(SourceError::Unavailable)
                ),
                "{node_id}"
            );
        }
        // 待办、旁支对话等节点本来就不提供内容。
        for node_id in [format!("todo:{ROOT_A}:0"), SIDE_CHAT.to_string()] {
            assert!(
                matches!(
                    ZCode.read(&cx, &node_id, None, 4096),
                    Err(SourceError::Unsupported)
                ),
                "{node_id}"
            );
        }
        // 拼路径前就拦下越界 id。
        assert!(matches!(
            ZCode.read(&cx, "sess_subagent_../../etc", None, 4096),
            Err(SourceError::Malformed(_))
        ));
        for cursor in ["garbage", "x:1", "t:abc", "o:-1"] {
            assert!(
                matches!(
                    ZCode.read(&cx, &sub("01"), Some(cursor), 4096),
                    Err(SourceError::Malformed(_))
                ),
                "{cursor}"
            );
        }
    }

    fn transcript_line(kind: &str, payload: &str) -> String {
        format!(
            "{{\"id\":\"evt\",\"sessionId\":\"sess\",\"turnId\":\"turn\",\"type\":\"{kind}\",\
             \"timestamp\":\"2026-09-21T13:00:00.000Z\",\"traceId\":\"trace\",\
             \"sequenceNumber\":1,\"payload\":{payload}}}\n"
        )
    }

    #[test]
    fn an_entry_larger_than_the_page_is_clipped_on_a_char_boundary() {
        let dir = TempDir::new("clip");
        let path = dir.path().join(TRANSCRIPT_FILE);
        let content = "解析".repeat(200);
        std::fs::write(
            &path,
            transcript_line("model_complete", &format!("{{\"content\":\"{content}\"}}")),
        )
        .expect("写转录");
        let chunk = read_transcript_page(&path, 0, MIN_READ_BYTES).expect("可读");
        assert!(chunk.truncated);
        assert!(chunk.eof, "唯一一行已消费");
        assert!(chunk.text.len() <= MIN_READ_BYTES);
        assert!(chunk.text.ends_with(" …\n"));
        assert!(chunk.text.starts_with("解析解析"));
    }

    #[test]
    fn oversized_lines_are_skipped_or_marked_without_parsing() {
        let dir = TempDir::new("oversized");
        let path = dir.path().join(TRANSCRIPT_FILE);
        let padding = "x".repeat(LINE_KEEP_BYTES + 512 * 1024);
        let mut body = String::new();
        body.push_str(&transcript_line(
            "model_request",
            &format!("{{\"messages\":[{{\"role\":\"user\",\"content\":\"{padding}\"}}]}}"),
        ));
        body.push_str(&transcript_line(
            "model_complete",
            &format!("{{\"content\":\"{padding}\"}}"),
        ));
        body.push_str(&transcript_line(
            "turn_complete",
            "{\"resultType\":\"success\",\"toolCallCount\":0,\"duration\":1500}",
        ));
        std::fs::write(&path, body).expect("写转录");
        let chunk = read_transcript_page(&path, 0, 64 * 1024).expect("可读");
        assert_eq!(
            chunk.text,
            "[model_complete entry too large to show]\n\
             ■ turn complete · success · 0 tool calls · 1s\n"
        );
        assert!(chunk.eof);
    }

    #[test]
    fn a_single_read_scans_a_bounded_number_of_bytes() {
        let dir = TempDir::new("scan-budget");
        let path = dir.path().join(TRANSCRIPT_FILE);
        let delta = "y".repeat(100 * 1024);
        let streaming = transcript_line(
            "model_streaming",
            &format!("{{\"delta\":\"{delta}\",\"kind\":\"text_delta\",\"done\":false}}"),
        );
        let mut file = File::create(&path).expect("建转录");
        let lines = (SCAN_BUDGET_BYTES as usize / streaming.len()) + 8;
        for _ in 0..lines {
            file.write_all(streaming.as_bytes()).expect("写流式行");
        }
        file.write_all(transcript_line("model_complete", "{\"content\":\"done\"}").as_bytes())
            .expect("写结尾");
        drop(file);

        let first = read_transcript_page(&path, 0, 4096).expect("可读");
        assert!(first.text.is_empty());
        assert!(!first.eof, "扫描预算用尽但未到末尾");
        let Ok(Cursor::Transcript(offset)) = parse_cursor(first.next_cursor.as_deref()) else {
            panic!("缺少续读游标");
        };
        assert!(offset >= SCAN_BUDGET_BYTES && offset < SCAN_BUDGET_BYTES + streaming.len() as u64);
        let second = read_transcript_page(&path, offset, 4096).expect("续读");
        assert_eq!(second.text, "done\n");
        assert!(second.eof);
    }

    #[test]
    fn helpers_format_and_map_consistently() {
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(12_345), "12.3k");
        assert_eq!(format_count(2_500_000), "2.5M");
        assert_eq!(format_duration(250), "250ms");
        assert_eq!(format_duration(45_000), "45s");
        assert_eq!(format_duration(3_723_000), "1h 2m");
        assert_eq!(clip_chars("解析器入口", 3), "解析器…");
        assert_eq!(clip_chars("short", 10), "short");
        assert_eq!(clip_bytes("解析", 4), "解");
        assert_eq!(
            migration_number("0022_backfilled_session_reasoning"),
            Some(22)
        );
        assert_eq!(migration_number("garbage"), None);
        assert_eq!(parse_rfc3339_ms("2026-09-21T14:13:20.000Z"), Some(NOW));
        assert_eq!(parse_rfc3339_ms("yesterday"), None);
        assert_eq!(metadata_status("stopped"), AgentActivityStatus::Failed);
        assert_eq!(metadata_status("brand-new"), AgentActivityStatus::Unknown);
        assert_eq!(
            sniff_event_type(transcript_line("model_streaming", "{}").as_bytes()),
            Some("model_streaming")
        );
        assert!(!is_safe_id("../x") && !is_safe_id("") && is_safe_id("sess_a-b_c"));
    }
}
