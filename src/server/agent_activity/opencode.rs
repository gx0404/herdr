//! OpenCode 的活动来源适配器：子会话树（`task` 工具派生的 subagent 会话）与待办，
//! 经官方只读入口 `opencode db "<sql>" --format json` 读取，不直接开库。
//!
//! # 本机取证（主安装 1.17.20 只读；1.18.31 取证副本在 XDG 全隔离目录下运行）
//!
//! 数据库 `<XDG_DATA_HOME|~/.local/share>/opencode/opencode.db`（`opencode db path`
//! 两版一致；隔离目录下 1.18.31 首次运行即建库，`sqlite_master` 里 20 张表，含
//! `session` / `message` / `part` / `todo` / `permission`）。本机 41 个会话、1141 条
//! 消息、5920 个 part，只核对了列名、键名与枚举取值，没有读取任何正文。
//!
//! - `session` 表两版列集合相同（`pragma_table_info` 逐列核对；只是列序不同）：
//!   `id`（`ses_` + 26 字符，41/41）、
//!   `project_id`、`workspace_id`、`parent_id`、`slug`、`directory`、`path`、`title`、
//!   `version`、`share_url`、`summary_*`、`metadata`、`cost`(real)、`tokens_input` /
//!   `tokens_output` / `tokens_reasoning` / `tokens_cache_read` / `tokens_cache_write`、
//!   `revert`、`permission`、`agent`、`model`、`time_created` / `time_updated`
//!   （毫秒）、`time_compacting`、`time_archived`。**没有状态列**。
//!   `model` 是 JSON 文本 `{providerID, id, variant?}`：1.17.20 主安装库上
//!   `json_each` 枚举出的键集合恰是这三个，`$.id` 与 `$.providerID` 是 text、
//!   `variant` 可缺；模型名走 `$.id`（25/25 非空），**不是 `$.modelID`**
//!   （0/25——`modelID` 是 `message.data` 的顶层键，不是 `session.model` 的）。
//!   [`MODEL_ID_PATH`] 之外保留 [`MODEL_ID_FALLBACK_PATH`] 只为兼容异版。
//!   本机 26/41 带 `parent_id`，深度只有 0/1，但
//!   上游明确不设深度上限 → 本适配器递归查询、深度只受 [`MAX_TREE_DEPTH`] 保护。
//!   `agent` 实测 null 16 个，其余是自由文本（`explore`、`librarian` 与第三方 agent
//!   名，含零宽字符）。子会话标题由二进制拼成 `<描述> (@<agent> subagent)`。
//! - 会话状态从最后一条消息推断：`message.data` 是 JSON 文本，键 `role`
//!   （`user` / `assistant`）、`time.created`、`time.completed`（assistant 行
//!   1141/1141 有）、`finish`（实测 `stop` 1023 / `tool-calls` 73 / `length` 2 /
//!   null）、`error`（对象，18 条）。assistant 行另带 `system`（约 47 KB 的系统提示）
//!   → 只用 `json_extract` 取键，绝不整列取回。
//! - `part.data.type` 实测：`tool` 2225、`step-start` 1104、`step-finish` 1098、
//!   `reasoning` 787、`text` 690、`compaction` 13、`file` 3。`tool` 部件带 `tool`、
//!   `callID`、`state{status: pending|running|completed|error, input, output, title,
//!   time{start,end}}`；`text` / `reasoning` 带 `text` 与可选 `time{start,end}`。
//! - `todo` 表：`session_id`、`content`、`status`、`priority`、`position`、
//!   `time_created`、`time_updated`；本机 0 行，形状按 1.18.31 隔离库里写入的合成行与
//!   二进制内嵌 schema 说明（`status` ∈ `pending` / `in_progress` / `completed` /
//!   `cancelled`，`priority` ∈ `high` / `medium` / `low`）。
//! - `opencode db` 只读命令：JSON 数组写到 stdout；SQL 出错退出码 1、stderr
//!   `Error: ...`；`--pure`（不加载外部插件）两版都有；支持 `json_extract` /
//!   `json_valid` 与递归 CTE。**stdout 是管道时，超过 64 KiB 的输出会在进程退出时被
//!   丢掉尾部**（1.17.20：80 KB 纯 ASCII 输出稳定截到 65536 字节，110 行 part 查询 8/8
//!   截断；≤ 62 KB 单次输出 100% 完整。1.18.31 隔离复现：递归 CTE 生成 500 / 900 /
//!   1500 / 3000 行，经管道一律恰好 65536 字节，重定向到文件则 406896 字节完整）
//!   → 每条查询的输出都按行数与 `substr` 限在 40 KiB 以内，解析失败且不以 `]` 结尾
//!   视为截断（`Unavailable`，稍后重试）。
//! - `opencode export <id>` 同样受该截断影响：本机最大的两个会话 3/3 次被截到
//!   48–58 KB，且每条 assistant 消息都附带整份系统提示 → **内容读取不用 export**，改
//!   按 `part` 行分页查询（见 [`read_with`]）。会话不存在时 export 退出码 1。
//! - 插件加载器两版相同（二进制字符串逐段核对）：加载函数先取模块 `default`；不是
//!   对象时 strict 模式抛 `must default export an object with server()`，detect 模式
//!   放行；是对象但不带 `id` / `server` / `tui` 任一键时退回旧路径——遍历全部导出、
//!   每个必须是函数或带 `server` 函数的对象；带任一键则只调 `default.server()`，
//!   具名导出不再执行。核验称「1.18.x 只认 `export default`」对 1.17.20 同样成立，
//!   herdr 资产自 #3757 起已是默认导出。会话行映射 `parentID: parent_id ?? undefined`
//!   （两版同一段代码）→ 根会话事件的 `info.parentID` 键存在但值为 `undefined`，
//!   「所有会话都带 parentID」只在「键存在」的意义上成立。事件名两版一致：
//!   `session.created` / `session.updated` / `session.deleted`（`{sessionID, info}`）、
//!   `session.status`（`{sessionID, status: {type: idle|busy|retry}}`）、`session.idle`
//!   （已弃用）、`permission.asked` / `permission.replied`、`todo.updated`
//!   （`{sessionID, todos}`）。
//!
//! # 键名漂移补验
//!
//! 上面的列名、别名与 json 路径是版本相关事实，单测喂的是「SQL 跑完之后」的行
//! JSON，覆盖不到 SQL 本身。升级 opencode 后（或怀疑键名漂移时）在装有 opencode
//! 的机器上跑只读探针，它把 [`tree_sql`] / [`todo_sql`] / [`parts_sql`] 原样打到
//! 本机库上，核对别名全集与各 json 路径仍能解析，并逐行核对 `model` 非空的会话都
//! 取得出模型名：
//!
//! ```text
//! cargo nextest run --run-ignored only -E 'test(live_schema_probe)'
//! ```
//!
//! 探针只读、只看键名与非空计数，从不取回或打印任何正文。
//!
//! # 节点
//!
//! 根会话就是 pane 本身，不出节点；它的直接子项 `parent_id` 为空。
//! - `session:<会话 id>`：子会话（`Subagent`）。`agent_type` 取 `agent` 列（缺省从标题
//!   的 `(@name subagent)` 后缀取），`summary` 是模型、本地费用与 token 用量（本地
//!   统计，非账号额度）。状态：最后一条 assistant 消息带 `error` → Failed，摘要以
//!   `错误名: 说明` 开头；带 `time.completed` → Done；否则（或最后一条是 user）→
//!   Running，但 `time_updated` 超过 [`RUNNING_STALE_MS`] 不再变化时降为 Unknown；
//!   没有消息 → Pending；`time_archived` 非空 → Done。`read` 返回该会话的部件流
//!   （`Text`）。
//!
//!   `error` 的形状按 1.18.32 捆绑源码核实：`{name, data}`，`name` ∈ `APIError` /
//!   `ProviderAuthError` / `UnknownError` / `MessageOutputLengthError` /
//!   `MessageAbortedError` / `StructuredOutputError` / `ContextOverflowError` /
//!   `ContentFilterError`，除 `MessageOutputLengthError` 外 `data.message` 都是字符串；
//!   `APIError` 另带响应头与响应体 → 只取 `name` 与截断后的 `data.message`（另在 SQL 里
//!   算原长，截断过的说明显示时以省略号结尾）。**库里看不出
//!   的失败**：默认模型已不在提供商目录时（`ProviderModelNotFoundError`），
//!   `SessionPrompt.getModel` 只经 Bus 发不落库的 `session.error`（没有 `durable`
//!   标记，不进 `event` 表），随即在建 assistant 消息之前退出——库里最后一条仍是 user
//!   消息、没有任何错误字段，节点只能按上面的时间规则从 Running 降为 Unknown。
//! - `todo:<会话 id>:<position>`：待办（`Todo`），没有可读内容；`pending` → Pending、
//!   `in_progress` → Running、`completed` / `cancelled` → Done（摘要注明 cancelled）。
//!
//! # 读取与游标
//!
//! 游标是已消费的 part 行数（十进制），每页 `LIMIT n+1 OFFSET 游标`，按 `max_bytes`
//! 逐行消费；`next_cursor` 总会给出（含 `eof`），跟随方拿它续读即可看到新部件。末尾
//! 连续的生成中部件（运行中的工具、未结束的文本）不消费，留到下次读到完整内容；
//! 夹在已完成部件之前的生成中部件照常消费，免得一个卡死的工具让游标永远停住。
//! 单个部件的文本按 [`PART_TEXT_CLIP_CHARS`] 截断并置 `truncated`。
//!
//! 「没内容」分两种：数据库文件还没生成、或会话不在库里 → `Unavailable`；会话在、
//! 只是还没有部件 → 空片段且 `eof`。
//!
//! # 钩子
//!
//! herdr 的 opencode 插件（`src/integration/assets/opencode/herdr-agent-state.js`）在
//! `session.created` / `session.updated` / `session.status` / `session.idle` /
//! `session.deleted` / `todo.updated` 上发 `pane.report_agent_activity`（`hint` 为事件
//! 名；子会话的这些事件同样上报；单槽合并——派发前又来的事件只保留最后一个事件名）。
//! 钩子只是触发器，树以本适配器查到的库为准。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Map, Value};

use super::{ActivitySource, ContentChunk, SourceContext, SourceError};
use crate::api::schema::{
    AgentActivityContentFormat, AgentActivityKind, AgentActivityNode, AgentActivityStatus,
};

pub(super) struct OpenCode;

/// 官方 CLI 名；与用量服务商注册表里 opencode 的 `command` 一致，按 PATH 解析。
const OPENCODE_COMMAND: &str = "opencode";
const DATABASE_FILE: &str = "opencode.db";

const SESSION_NODE_PREFIX: &str = "session:";
const TODO_NODE_PREFIX: &str = "todo:";

/// 一次树查询取回的会话行数：每行约 500 B，48 行约 24 KiB，远低于管道截断阈值。
const SESSION_ROWS_PER_QUERY: usize = 48;
/// 一次 discover 最多收录的会话数（含根）。
const MAX_SESSIONS: usize = 256;
/// 递归查询的深度保护：不是产品上限，只防 `parent_id` 成环时无限递归。
const MAX_TREE_DEPTH: u64 = 128;
/// 一次 discover 最多收录的待办行数。
const MAX_TODO_ROWS: usize = 128;
/// 标题 / 待办内容在 SQL 里先截到这么多字符，再由 `clean_line` 裁到显示上限。
const SQL_TITLE_CHARS: usize = 160;
const MAX_LABEL_CHARS: usize = 120;
const MAX_SUMMARY_CHARS: usize = 160;
/// 失败子会话的错误说明在 SQL 里先截到这么多字符（APIError 等还带响应体，永不取回），
/// 再由 `clean_line` 裁到 [`MAX_ERROR_CHARS`]，给摘要里的模型与用量留出位置。
const SQL_ERROR_CHARS: usize = 120;
const MAX_ERROR_CHARS: usize = 100;
const MAX_ID_LEN: usize = 64;

/// 模型名在 `session.model` 里的路径：1.17.20 实测 `{providerID, id, variant?}`，
/// 模型名是 `$.id`。
const MODEL_ID_PATH: &str = "$.id";
/// 兼容回退：`$.modelID` 在实测的两个版本里都不是 `session.model` 的键（它是
/// `message.data` 的顶层键），只为其他版本万一改名而保留。
const MODEL_ID_FALLBACK_PATH: &str = "$.modelID";

/// 单个部件的文本在 SQL 里截到这么多字符（CJK 最多 3 字节 / 字符）。
const PART_TEXT_CLIP_CHARS: usize = 400;
/// 工具输出只给这么多字符的预览。
const TOOL_OUTPUT_CLIP_CHARS: usize = 200;
/// 一页取回的部件行数区间：24 行 × 最坏 1.8 KiB ≈ 43 KiB，仍在截断阈值之下。
const MIN_PARTS_PER_PAGE: usize = 4;
const MAX_PARTS_PER_PAGE: usize = 24;
/// `max_bytes` 为 0 时的默认预算。
const DEFAULT_READ_BYTES: usize = 64 * 1024;
/// 每行部件的估算字节数，用于由 `max_bytes` 推算取行数。
const ESTIMATED_PART_BYTES: usize = 1024;

/// 最后一条消息没有结束、且会话这么久不再更新时，不再算运行中。
const RUNNING_STALE_MS: u64 = 30 * 60_000;

/// CLI 子进程的总时限与输出上限；超时即杀。
const CLI_TIMEOUT: Duration = Duration::from_secs(20);
const CLI_POLL_INTERVAL: Duration = Duration::from_millis(20);
const MAX_CLI_OUTPUT_BYTES: usize = 1024 * 1024;

/// 子进程里不该继承的 pane 环境：CLI 若装了 herdr 插件，不能把查询进程当成 pane 上报。
const PANE_ENV_VARS: [&str; 8] = [
    "HERDR_ENV",
    "HERDR_SOCKET_PATH",
    "HERDR_CLIENT_SOCKET_PATH",
    "HERDR_PANE_ID",
    "HERDR_TERMINAL_ID",
    "HERDR_WORKSPACE_ID",
    "HERDR_TAB_ID",
    "HERDR_SESSION",
];

impl ActivitySource for OpenCode {
    fn id(&self) -> &'static str {
        "opencode"
    }

    fn discover(&self, cx: &SourceContext<'_>) -> Result<Vec<AgentActivityNode>, SourceError> {
        discover_with(cx, &OfficialCli, xdg_data_home())
    }

    fn read(
        &self,
        cx: &SourceContext<'_>,
        node_id: &str,
        cursor: Option<&str>,
        max_bytes: usize,
    ) -> Result<ContentChunk, SourceError> {
        read_with(
            cx,
            &OfficialCli,
            xdg_data_home(),
            node_id,
            cursor,
            max_bytes,
        )
    }
}

/// 执行 `opencode db <sql> --format json` 并返回 stdout 文本；测试注入夹具。
trait DbQuery {
    fn query(&self, sql: &str) -> Result<String, SourceError>;
}

struct OfficialCli;

impl DbQuery for OfficialCli {
    fn query(&self, sql: &str) -> Result<String, SourceError> {
        run_db_query(sql)
    }
}

fn xdg_data_home() -> Option<OsString> {
    std::env::var_os("XDG_DATA_HOME")
}

/// 数据目录：`XDG_DATA_HOME/opencode`（开头的 `~` 按 `home` 展开）优先，否则
/// `<home>/.local/share/opencode`，与 `integration::env::opencode_data_dir` 同口径。
fn data_dir(home: &Path, xdg_data_home: Option<OsString>) -> PathBuf {
    match xdg_data_home.filter(|value| !value.is_empty()) {
        Some(value) => {
            let path = PathBuf::from(value);
            let expanded = match path.strip_prefix("~") {
                Ok(rest) => home.join(rest),
                Err(_) => path,
            };
            expanded.join("opencode")
        }
        None => home.join(".local").join("share").join("opencode"),
    }
}

fn database_exists(home: &Path, xdg_data_home: Option<OsString>) -> bool {
    data_dir(home, xdg_data_home).join(DATABASE_FILE).is_file()
}

/// 根会话 id：没有会话引用 → `Unsupported`；不是 opencode 会话 id 的形状 → `Malformed`
/// （它要拼进 SQL，只放行 `[A-Za-z0-9_-]`）。
fn root_session_id(cx: &SourceContext<'_>) -> Result<String, SourceError> {
    let session = cx.session.ok_or(SourceError::Unsupported)?;
    let id = session.value.trim();
    if !valid_id(id) {
        return Err(SourceError::Malformed(format!(
            "session reference is not an opencode session id: {id}"
        )));
    }
    Ok(id.to_owned())
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_LEN
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

// ---------------------------------------------------------------------------
// 发现
// ---------------------------------------------------------------------------

fn discover_with(
    cx: &SourceContext<'_>,
    db: &dyn DbQuery,
    xdg_data_home: Option<OsString>,
) -> Result<Vec<AgentActivityNode>, SourceError> {
    let root = root_session_id(cx)?;
    if !database_exists(cx.home, xdg_data_home) {
        return Ok(Vec::new());
    }
    let sessions = fetch_sessions(db, &root)?;
    if sessions.is_empty() {
        // 根会话尚未落库（刚创建）或已被删除：空树，不报错。
        return Ok(Vec::new());
    }
    // 待办是增强信息：查询失败（老版本没有 todo 表等）只丢待办，树照常给出。
    let todos = match fetch_todos(db, &sessions) {
        Ok(todos) => todos,
        Err(error) => {
            tracing::debug!(
                ?error,
                "opencode todo query failed; tree continues without todos"
            );
            Vec::new()
        }
    };
    Ok(build_tree(&root, sessions, &todos, cx.now_ms))
}

struct SessionRow {
    id: String,
    parent_id: Option<String>,
    depth: u64,
    agent: Option<String>,
    model_id: Option<String>,
    title: Option<String>,
    title_len: Option<u64>,
    time_created: Option<u64>,
    time_updated: Option<u64>,
    time_archived: Option<u64>,
    cost: f64,
    tokens_input: u64,
    tokens_output: u64,
    tokens_reasoning: u64,
    last_role: Option<String>,
    last_completed: Option<u64>,
    last_error: bool,
    /// 最后一条消息 `error.name`（如 `APIError`）与 `error.data.message`（SQL 里已截断，
    /// 截断前的字符数另见 `last_error_message_len`）。
    last_error_name: Option<String>,
    last_error_message: Option<String>,
    last_error_message_len: Option<u64>,
}

struct TodoRow {
    session_id: String,
    position: u64,
    status: Option<String>,
    priority: Option<String>,
    content: Option<String>,
    content_len: Option<u64>,
    time_created: Option<u64>,
    time_updated: Option<u64>,
}

/// 从根会话递归取整棵子会话树，按深度 / 创建时间排序，分页直到取完或达上限。
fn fetch_sessions(db: &dyn DbQuery, root: &str) -> Result<Vec<SessionRow>, SourceError> {
    let mut sessions: Vec<SessionRow> = Vec::new();
    let mut seen = HashSet::new();
    let mut offset = 0;
    loop {
        let rows = parse_rows(&db.query(&tree_sql(root, offset))?)?;
        let fetched = rows.len();
        for row in rows {
            if sessions.len() >= MAX_SESSIONS {
                break;
            }
            // 单行缺 id 只丢它自己；成环产生的重复行以先出现的为准。
            let Some(session) = session_row(&row) else {
                continue;
            };
            if seen.insert(session.id.clone()) {
                sessions.push(session);
            }
        }
        if fetched < SESSION_ROWS_PER_QUERY || sessions.len() >= MAX_SESSIONS {
            break;
        }
        offset += SESSION_ROWS_PER_QUERY;
    }
    sessions.sort_by_key(|session| session.depth);
    Ok(sessions)
}

/// 从 `session.model` 列（`column` 是它在查询里的写法）取模型名的表达式：
/// 先走实测路径，再走兼容回退；列不是合法 JSON 时为 null。
fn model_id_sql(column: &str) -> String {
    format!(
        "CASE WHEN json_valid({column}) THEN COALESCE(\
           json_extract({column}, '{MODEL_ID_PATH}'), \
           json_extract({column}, '{MODEL_ID_FALLBACK_PATH}')\
         ) END"
    )
}

fn tree_sql(root: &str, offset: usize) -> String {
    let model_id = model_id_sql("s.model");
    format!(
        "WITH RECURSIVE tree(id, depth) AS (\
           SELECT id, 0 FROM session WHERE id = '{root}' \
           UNION ALL \
           SELECT s.id, t.depth + 1 FROM session s JOIN tree t ON s.parent_id = t.id \
           WHERE t.depth < {MAX_TREE_DEPTH}\
         ) \
         SELECT s.id, s.parent_id, t.depth, s.agent, \
           {model_id} AS model_id, \
           substr(s.title, 1, {SQL_TITLE_CHARS}) AS title, length(s.title) AS title_len, \
           s.time_created, s.time_updated, s.time_archived, \
           s.cost, s.tokens_input, s.tokens_output, s.tokens_reasoning, \
           CASE WHEN json_valid(lm.data) THEN json_extract(lm.data, '$.role') END AS last_role, \
           CASE WHEN json_valid(lm.data) THEN json_extract(lm.data, '$.time.completed') END AS last_completed, \
           CASE WHEN json_valid(lm.data) THEN json_type(lm.data, '$.error') END AS last_error, \
           CASE WHEN json_valid(lm.data) THEN json_extract(lm.data, '$.error.name') END AS last_error_name, \
           CASE WHEN json_valid(lm.data) THEN substr(json_extract(lm.data, '$.error.data.message'), 1, {SQL_ERROR_CHARS}) END AS last_error_message, \
           CASE WHEN json_valid(lm.data) THEN length(json_extract(lm.data, '$.error.data.message')) END AS last_error_message_len \
         FROM tree t JOIN session s ON s.id = t.id \
         LEFT JOIN message lm ON lm.id = (\
           SELECT id FROM message WHERE session_id = s.id \
           ORDER BY time_created DESC, id DESC LIMIT 1\
         ) \
         ORDER BY t.depth, s.time_created, s.id \
         LIMIT {SESSION_ROWS_PER_QUERY} OFFSET {offset}"
    )
}

fn session_row(row: &Map<String, Value>) -> Option<SessionRow> {
    let id = identifier(row.get("id"))?;
    Some(SessionRow {
        parent_id: identifier(row.get("parent_id")),
        depth: row.get("depth").and_then(non_negative_integer).unwrap_or(0),
        agent: text_field(row, "agent"),
        model_id: text_field(row, "model_id"),
        title: text_field(row, "title"),
        title_len: row.get("title_len").and_then(non_negative_integer),
        time_created: row.get("time_created").and_then(non_negative_integer),
        time_updated: row.get("time_updated").and_then(non_negative_integer),
        time_archived: row.get("time_archived").and_then(non_negative_integer),
        cost: row
            .get("cost")
            .and_then(Value::as_f64)
            .filter(|cost| cost.is_finite() && *cost >= 0.0)
            .unwrap_or(0.0),
        tokens_input: row
            .get("tokens_input")
            .and_then(non_negative_integer)
            .unwrap_or(0),
        tokens_output: row
            .get("tokens_output")
            .and_then(non_negative_integer)
            .unwrap_or(0),
        tokens_reasoning: row
            .get("tokens_reasoning")
            .and_then(non_negative_integer)
            .unwrap_or(0),
        last_role: text_field(row, "last_role"),
        last_completed: row.get("last_completed").and_then(non_negative_integer),
        // `json_type` 给出 `object` / `null` 等类型名；有错误对象即视为失败。
        last_error: row
            .get("last_error")
            .and_then(Value::as_str)
            .is_some_and(|kind| kind != "null"),
        last_error_name: text_field(row, "last_error_name"),
        last_error_message: text_field(row, "last_error_message"),
        last_error_message_len: row
            .get("last_error_message_len")
            .and_then(non_negative_integer),
        id,
    })
}

fn fetch_todos(db: &dyn DbQuery, sessions: &[SessionRow]) -> Result<Vec<TodoRow>, SourceError> {
    if sessions.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<String> = sessions
        .iter()
        .map(|session| format!("'{}'", session.id))
        .collect();
    let rows = parse_rows(&db.query(&todo_sql(&ids.join(", ")))?)?;
    Ok(rows.iter().filter_map(todo_row).collect())
}

fn todo_sql(quoted_ids: &str) -> String {
    format!(
        "SELECT session_id, position, status, priority, \
           substr(content, 1, {SQL_TITLE_CHARS}) AS content, length(content) AS content_len, \
           time_created, time_updated \
         FROM todo WHERE session_id IN ({quoted_ids}) \
         ORDER BY session_id, position LIMIT {MAX_TODO_ROWS}"
    )
}

fn todo_row(row: &Map<String, Value>) -> Option<TodoRow> {
    let session_id = identifier(row.get("session_id"))?;
    let position = row.get("position").and_then(non_negative_integer)?;
    Some(TodoRow {
        session_id,
        position,
        status: text_field(row, "status"),
        priority: text_field(row, "priority"),
        content: text_field(row, "content"),
        content_len: row.get("content_len").and_then(non_negative_integer),
        time_created: row.get("time_created").and_then(non_negative_integer),
        time_updated: row.get("time_updated").and_then(non_negative_integer),
    })
}

/// 父先子后：会话行已按深度排序，父会话总在子会话之前；每个会话的待办紧随其后。
fn build_tree(
    root: &str,
    sessions: Vec<SessionRow>,
    todos: &[TodoRow],
    now_ms: u64,
) -> Vec<AgentActivityNode> {
    let known: HashSet<&str> = sessions.iter().map(|session| session.id.as_str()).collect();
    let mut todos_by_session: HashMap<&str, BTreeMap<u64, &TodoRow>> = HashMap::new();
    for todo in todos {
        if known.contains(todo.session_id.as_str()) {
            // 同一位置重复时以先出现的为准。
            todos_by_session
                .entry(todo.session_id.as_str())
                .or_default()
                .entry(todo.position)
                .or_insert(todo);
        }
    }

    let mut nodes = Vec::with_capacity(sessions.len() + todos.len());
    for session in &sessions {
        let is_root = session.id == root;
        let owner = (!is_root).then(|| session_node_id(&session.id));
        if !is_root {
            // 父会话是根、缺失（被上限截掉）或指向自己：挂到根。
            let parent = session
                .parent_id
                .as_deref()
                .filter(|parent| *parent != root && *parent != session.id && known.contains(parent))
                .map(session_node_id);
            nodes.push(session_node(session, parent, now_ms));
        }
        if let Some(todos) = todos_by_session.get(session.id.as_str()) {
            for todo in todos.values() {
                nodes.push(todo_node(todo, owner.clone()));
            }
        }
    }
    nodes
}

fn session_node_id(session_id: &str) -> String {
    format!("{SESSION_NODE_PREFIX}{session_id}")
}

fn session_node(session: &SessionRow, parent_id: Option<String>, now_ms: u64) -> AgentActivityNode {
    let id = session_node_id(&session.id);
    let (label, title_agent) = split_title(session);
    let status = session_status(session, now_ms);
    let ended_at_ms = match status {
        AgentActivityStatus::Done | AgentActivityStatus::Failed => session
            .time_archived
            .or(session.last_completed)
            .or(session.time_updated),
        _ => None,
    };
    AgentActivityNode {
        kind: AgentActivityKind::Subagent,
        label: label.unwrap_or_else(|| session.id.clone()),
        status,
        parent_id,
        agent_type: session
            .agent
            .as_deref()
            .and_then(|agent| clean_line(agent, MAX_LABEL_CHARS))
            .or(title_agent),
        content_ref: Some(id.clone()),
        summary: session_summary(session, status),
        started_at_ms: session.time_created,
        ended_at_ms,
        id,
    }
}

/// 标题 `<描述> (@<agent> subagent)` 拆成描述与 agent 名；不是这个形状就整个当标题。
fn split_title(session: &SessionRow) -> (Option<String>, Option<String>) {
    let Some(title) = session.title.as_deref() else {
        return (None, None);
    };
    let clipped = session
        .title_len
        .is_some_and(|len| len > title.chars().count() as u64);
    let trimmed = title.trim_end();
    if !clipped {
        if let Some(head) = trimmed.strip_suffix(" subagent)") {
            if let Some((description, agent)) = head.rsplit_once(" (@") {
                if !agent.is_empty() && !agent.contains(' ') {
                    return (
                        clean_line(description, MAX_LABEL_CHARS),
                        clean_line(agent, MAX_LABEL_CHARS),
                    );
                }
            }
        }
    }
    let mut label = clean_line(title, MAX_LABEL_CHARS);
    if clipped {
        label = label.map(|label| clipped_line(label, MAX_LABEL_CHARS));
    }
    (label, None)
}

fn session_status(session: &SessionRow, now_ms: u64) -> AgentActivityStatus {
    if session.time_archived.is_some() {
        return AgentActivityStatus::Done;
    }
    let recently_active = session
        .time_updated
        .is_some_and(|updated| now_ms.saturating_sub(updated) < RUNNING_STALE_MS);
    match session.last_role.as_deref() {
        None => {
            if recently_active {
                AgentActivityStatus::Pending
            } else {
                AgentActivityStatus::Unknown
            }
        }
        Some("assistant") => {
            if session.last_error {
                AgentActivityStatus::Failed
            } else if session.last_completed.is_some() {
                AgentActivityStatus::Done
            } else if recently_active {
                AgentActivityStatus::Running
            } else {
                AgentActivityStatus::Unknown
            }
        }
        Some("user") => {
            if recently_active {
                AgentActivityStatus::Running
            } else {
                AgentActivityStatus::Unknown
            }
        }
        Some(_) => AgentActivityStatus::Unknown,
    }
}

/// 摘要：失败时先写错误，然后是模型、本地费用与 token 用量（本地统计，不是账号额度）。
fn session_summary(session: &SessionRow, status: AgentActivityStatus) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if status == AgentActivityStatus::Failed {
        parts.extend(error_summary(session));
    }
    if let Some(model) = session
        .model_id
        .as_deref()
        .and_then(|model| clean_line(model, MAX_LABEL_CHARS))
    {
        parts.push(model);
    }
    if session.cost > 0.0 {
        parts.push(format!("${:.2}", session.cost));
    }
    if session.tokens_input > 0 || session.tokens_output > 0 {
        parts.push(format!(
            "{} in · {} out",
            format_tokens(session.tokens_input),
            format_tokens(session.tokens_output)
        ));
    }
    if session.tokens_reasoning > 0 {
        parts.push(format!(
            "{} reasoning",
            format_tokens(session.tokens_reasoning)
        ));
    }
    if status == AgentActivityStatus::Done && session.time_archived.is_some() {
        parts.push("archived".to_owned());
    }
    if parts.is_empty() {
        return None;
    }
    clean_line(&parts.join(" · "), MAX_SUMMARY_CHARS)
}

/// 失败原因：`错误名: 说明`；只有其一时只写其一。说明压成单行并截到
/// [`MAX_ERROR_CHARS`]，免得一条长报错把模型与用量挤出摘要；在 SQL 里已被截断的说明
/// 总以省略号结尾（按 `last_error_message_len` 判定）。
fn error_summary(session: &SessionRow) -> Option<String> {
    let name = session
        .last_error_name
        .as_deref()
        .and_then(|name| clean_line(name, MAX_LABEL_CHARS));
    let clipped = session
        .last_error_message_len
        .zip(session.last_error_message.as_deref())
        .is_some_and(|(len, message)| len > message.chars().count() as u64);
    let message = session
        .last_error_message
        .as_deref()
        .and_then(|message| clean_line(message, MAX_ERROR_CHARS))
        .map(|message| {
            if clipped {
                clipped_line(message, MAX_ERROR_CHARS)
            } else {
                message
            }
        });
    match (name, message) {
        (Some(name), Some(message)) => Some(format!("{name}: {message}")),
        (name, message) => name.or(message),
    }
}

fn format_tokens(count: u64) -> String {
    if count < 1_000 {
        count.to_string()
    } else if count < 1_000_000 {
        format!("{:.1}k", count as f64 / 1_000.0)
    } else {
        format!("{:.1}M", count as f64 / 1_000_000.0)
    }
}

fn todo_node(todo: &TodoRow, parent_id: Option<String>) -> AgentActivityNode {
    let id = format!("{TODO_NODE_PREFIX}{}:{}", todo.session_id, todo.position);
    let (status, cancelled) = match todo.status.as_deref() {
        Some("pending") => (AgentActivityStatus::Pending, false),
        Some("in_progress") => (AgentActivityStatus::Running, false),
        Some("completed") => (AgentActivityStatus::Done, false),
        Some("cancelled") => (AgentActivityStatus::Done, true),
        _ => (AgentActivityStatus::Unknown, false),
    };
    let mut label = todo
        .content
        .as_deref()
        .and_then(|content| clean_line(content, MAX_LABEL_CHARS));
    if todo
        .content_len
        .zip(todo.content.as_deref())
        .is_some_and(|(len, content)| len > content.chars().count() as u64)
    {
        label = label.map(|label| clipped_line(label, MAX_LABEL_CHARS));
    }
    let mut summary: Vec<String> = Vec::new();
    if cancelled {
        summary.push("cancelled".to_owned());
    }
    if let Some(priority) = todo
        .priority
        .as_deref()
        .and_then(|priority| clean_line(priority, MAX_LABEL_CHARS))
    {
        summary.push(priority);
    }
    AgentActivityNode {
        kind: AgentActivityKind::Todo,
        label: label.unwrap_or_else(|| id.clone()),
        status,
        parent_id,
        agent_type: None,
        content_ref: None,
        summary: (!summary.is_empty()).then(|| summary.join(" · ")),
        started_at_ms: todo.time_created,
        ended_at_ms: (status == AgentActivityStatus::Done)
            .then_some(todo.time_updated)
            .flatten(),
        id,
    }
}

// ---------------------------------------------------------------------------
// 读取
// ---------------------------------------------------------------------------

fn read_with(
    cx: &SourceContext<'_>,
    db: &dyn DbQuery,
    xdg_data_home: Option<OsString>,
    node_id: &str,
    cursor: Option<&str>,
    max_bytes: usize,
) -> Result<ContentChunk, SourceError> {
    root_session_id(cx)?;
    // 待办节点与未知 id 没有可读内容。
    let Some(session_id) = node_id.strip_prefix(SESSION_NODE_PREFIX) else {
        return Err(SourceError::Unavailable);
    };
    if !valid_id(session_id) {
        return Err(SourceError::Malformed(format!(
            "node id is not an opencode session node: {node_id}"
        )));
    }
    if !database_exists(cx.home, xdg_data_home) {
        return Err(SourceError::Unavailable);
    }
    let offset = cursor.map(parse_cursor).transpose()?.unwrap_or(0);
    let budget = if max_bytes == 0 {
        DEFAULT_READ_BYTES
    } else {
        max_bytes
    };
    let per_page = (budget / ESTIMATED_PART_BYTES).clamp(MIN_PARTS_PER_PAGE, MAX_PARTS_PER_PAGE);
    let rows = parse_rows(&db.query(&parts_sql(session_id, offset, per_page + 1))?)?;
    if rows.is_empty() && offset == 0 && !session_exists(db, session_id)? {
        return Err(SourceError::Unavailable);
    }
    let parts: Vec<PartRow> = rows.iter().map(part_row).collect();
    Ok(render_page(&parts, offset, per_page, budget))
}

fn parse_cursor(cursor: &str) -> Result<u64, SourceError> {
    cursor
        .trim()
        .parse()
        .map_err(|_| SourceError::Malformed(format!("cursor is not a row offset: {cursor}")))
}

fn parts_sql(session_id: &str, offset: u64, limit: usize) -> String {
    format!(
        "SELECT p.id, \
           CASE WHEN json_valid(m.data) THEN json_extract(m.data, '$.role') END AS role, \
           CASE WHEN json_valid(m.data) THEN json_extract(m.data, '$.time.completed') END AS message_completed, \
           json_extract(p.data, '$.type') AS type, \
           json_extract(p.data, '$.tool') AS tool, \
           json_extract(p.data, '$.state.status') AS tool_status, \
           substr(json_extract(p.data, '$.state.title'), 1, {SQL_TITLE_CHARS}) AS tool_title, \
           substr(json_extract(p.data, '$.state.output'), 1, {TOOL_OUTPUT_CLIP_CHARS}) AS tool_output, \
           length(json_extract(p.data, '$.state.output')) AS tool_output_len, \
           json_extract(p.data, '$.time.end') AS part_end, \
           substr(json_extract(p.data, '$.text'), 1, {PART_TEXT_CLIP_CHARS}) AS text, \
           length(json_extract(p.data, '$.text')) AS text_len, \
           substr(json_extract(p.data, '$.filename'), 1, {SQL_TITLE_CHARS}) AS filename \
         FROM part p LEFT JOIN message m ON m.id = p.message_id \
         WHERE p.session_id = '{session_id}' AND json_valid(p.data) \
         ORDER BY p.time_created, p.id LIMIT {limit} OFFSET {offset}"
    )
}

fn session_exists(db: &dyn DbQuery, session_id: &str) -> Result<bool, SourceError> {
    let output = db.query(&format!(
        "SELECT 1 AS present FROM session WHERE id = '{session_id}' LIMIT 1"
    ))?;
    Ok(!parse_rows(&output)?.is_empty())
}

struct PartRow {
    role: Option<String>,
    message_completed: bool,
    kind: Option<String>,
    tool: Option<String>,
    tool_status: Option<String>,
    tool_title: Option<String>,
    tool_output: Option<String>,
    tool_output_clipped: bool,
    part_ended: bool,
    text: Option<String>,
    text_clipped: bool,
    filename: Option<String>,
}

fn part_row(row: &Map<String, Value>) -> PartRow {
    let text = row.get("text").and_then(Value::as_str).map(str::to_owned);
    let tool_output = row
        .get("tool_output")
        .and_then(Value::as_str)
        .map(str::to_owned);
    PartRow {
        role: text_field(row, "role"),
        message_completed: row
            .get("message_completed")
            .and_then(non_negative_integer)
            .is_some(),
        kind: text_field(row, "type"),
        tool: text_field(row, "tool"),
        tool_status: text_field(row, "tool_status"),
        tool_title: text_field(row, "tool_title"),
        tool_output_clipped: clipped(row.get("tool_output_len"), tool_output.as_deref()),
        tool_output,
        part_ended: row.get("part_end").and_then(non_negative_integer).is_some(),
        text_clipped: clipped(row.get("text_len"), text.as_deref()),
        text,
        filename: text_field(row, "filename"),
    }
}

fn clipped(full_len: Option<&Value>, text: Option<&str>) -> bool {
    match (full_len.and_then(non_negative_integer), text) {
        (Some(len), Some(text)) => len > text.chars().count() as u64,
        _ => false,
    }
}

impl PartRow {
    /// 仍在生成：运行中的工具，或所属 assistant 消息未结束且部件本身没有结束时间。
    fn in_progress(&self) -> bool {
        match self.kind.as_deref() {
            Some("tool") => matches!(
                self.tool_status.as_deref(),
                Some("pending") | Some("running")
            ),
            Some("text") | Some("reasoning") => {
                self.role.as_deref() == Some("assistant")
                    && !self.message_completed
                    && !self.part_ended
            }
            _ => false,
        }
    }

    fn clipped(&self) -> bool {
        self.text_clipped || self.tool_output_clipped
    }

    /// 渲染成一段文本；步骤标记等没有可读内容的部件返回 `None`。
    fn render(&self) -> Option<String> {
        let kind = self.kind.as_deref()?;
        match kind {
            "text" => {
                let text = self.text.as_deref().map(clean_content)?;
                let text = with_ellipsis(text, self.text_clipped);
                Some(if self.role.as_deref() == Some("user") {
                    format!("user: {text}")
                } else {
                    text
                })
            }
            "reasoning" => {
                let text = self.text.as_deref().map(clean_content)?;
                Some(format!(
                    "(thinking) {}",
                    with_ellipsis(text, self.text_clipped)
                ))
            }
            "tool" => {
                let name = self
                    .tool
                    .as_deref()
                    .and_then(|tool| clean_line(tool, MAX_LABEL_CHARS))
                    .unwrap_or_else(|| "tool".to_owned());
                let mut line = format!("[{name}]");
                if let Some(title) = self
                    .tool_title
                    .as_deref()
                    .and_then(|title| clean_line(title, MAX_LABEL_CHARS))
                {
                    line.push(' ');
                    line.push_str(&title);
                }
                match self.tool_status.as_deref() {
                    Some("completed") | None => {}
                    Some(status) => {
                        if let Some(status) = clean_line(status, MAX_LABEL_CHARS) {
                            line.push_str(&format!(" ({status})"));
                        }
                    }
                }
                if let Some(output) = self
                    .tool_output
                    .as_deref()
                    .map(clean_content)
                    .filter(|output| !output.trim().is_empty())
                {
                    line.push('\n');
                    line.push_str(&indent(&with_ellipsis(output, self.tool_output_clipped)));
                }
                Some(line)
            }
            "step-start" | "step-finish" => None,
            "file" => Some(
                match self
                    .filename
                    .as_deref()
                    .and_then(|name| clean_line(name, MAX_LABEL_CHARS))
                {
                    Some(name) => format!("[file] {name}"),
                    None => "[file]".to_owned(),
                },
            ),
            other => clean_line(other, MAX_LABEL_CHARS).map(|kind| format!("[{kind}]")),
        }
    }
}

fn with_ellipsis(text: String, clipped: bool) -> String {
    if clipped {
        format!("{text}…")
    } else {
        text
    }
}

fn indent(text: &str) -> String {
    text.lines()
        .map(|line| format!("    {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// 按字节预算逐行消费，至少消费一行保证前进；末尾连续的生成中部件留到下次。
fn render_page(parts: &[PartRow], offset: u64, per_page: usize, budget: usize) -> ContentChunk {
    let more = parts.len() > per_page;
    let mut available = parts.len().min(per_page);
    if !more {
        while available > 0 && parts[available - 1].in_progress() {
            available -= 1;
        }
    }
    let mut text = String::new();
    let mut consumed = 0;
    let mut truncated = false;
    for part in &parts[..available] {
        let rendered = part.render().map(|line| format!("{line}\n"));
        let added = rendered.as_ref().map_or(0, String::len);
        if consumed > 0 && text.len() + added > budget {
            break;
        }
        if let Some(rendered) = rendered {
            text.push_str(&rendered);
        }
        truncated |= part.clipped();
        consumed += 1;
    }
    let eof = !more && consumed == available;
    ContentChunk {
        format: AgentActivityContentFormat::Text,
        text,
        next_cursor: Some((offset + consumed as u64).to_string()),
        eof,
        truncated,
    }
}

// ---------------------------------------------------------------------------
// CLI 与解析
// ---------------------------------------------------------------------------

/// 运行 `opencode db <sql> --format json --pure`（不加载外部插件，含 herdr 自己的）。
/// 找不到 CLI → `Io`；超时、非零退出、输出超限 → `Unavailable`。
fn run_db_query(sql: &str) -> Result<String, SourceError> {
    let mut command = Command::new(OPENCODE_COMMAND);
    crate::platform::configure_background_command(&mut command);
    command
        .args(["db", sql, "--format", "json", "--pure"])
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    for variable in PANE_ENV_VARS {
        command.env_remove(variable);
    }
    let mut child = command.spawn().map_err(SourceError::Io)?;
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(SourceError::Unavailable);
    };
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdout
            .take(MAX_CLI_OUTPUT_BYTES as u64 + 1)
            .read_to_end(&mut bytes);
        result.map(|_| bytes)
    });
    let deadline = Instant::now() + CLI_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(SourceError::Io(error));
            }
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            tracing::debug!("opencode db query timed out after {:?}", CLI_TIMEOUT);
            return Err(SourceError::Unavailable);
        }
        std::thread::sleep(CLI_POLL_INTERVAL);
    };
    let bytes = match reader.join() {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(error)) => return Err(SourceError::Io(error)),
        Err(_) => return Err(SourceError::Unavailable),
    };
    if !status.success() {
        tracing::debug!(?status, "opencode db query failed");
        return Err(SourceError::Unavailable);
    }
    if bytes.len() > MAX_CLI_OUTPUT_BYTES {
        return Err(SourceError::Unavailable);
    }
    String::from_utf8(bytes)
        .map_err(|_| SourceError::Malformed("opencode db output is not UTF-8".into()))
}

/// `--format json` 的输出是对象数组；非对象元素丢弃。解析失败时，不以 `]` 结尾说明
/// 被管道截断（`Unavailable`，重试即可），否则是格式问题（`Malformed`）。
fn parse_rows(output: &str) -> Result<Vec<Map<String, Value>>, SourceError> {
    let trimmed = output.trim();
    match serde_json::from_str::<Value>(trimmed) {
        Ok(Value::Array(items)) => Ok(items
            .into_iter()
            .filter_map(|item| match item {
                Value::Object(row) => Some(row),
                _ => None,
            })
            .collect()),
        Ok(_) => Err(SourceError::Malformed(
            "opencode db output is not a JSON array".into(),
        )),
        Err(error) => {
            if trimmed.ends_with(']') {
                Err(SourceError::Malformed(format!(
                    "opencode db output is not JSON: {error}"
                )))
            } else {
                tracing::debug!(%error, "opencode db output looks truncated; retrying later");
                Err(SourceError::Unavailable)
            }
        }
    }
}

fn text_field(row: &Map<String, Value>, key: &str) -> Option<String> {
    row.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn identifier(value: Option<&Value>) -> Option<String> {
    let text = value?.as_str()?.trim();
    valid_id(text).then(|| text.to_owned())
}

/// 毫秒时间戳与计数：接受非负整数与非负有限浮点（截断）。
fn non_negative_integer(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| {
        value
            .as_f64()
            .filter(|number| number.is_finite() && *number >= 0.0)
            .map(|number| number as u64)
    })
}

/// 单行展示文本：控制字符与连续空白压成一个空格，超长截断并以省略号结尾。
fn clean_line(text: &str, max_chars: usize) -> Option<String> {
    let mut line = String::new();
    let mut pending_space = false;
    for ch in text.chars() {
        if ch.is_control() || ch.is_whitespace() || is_format_char(ch) {
            pending_space = !line.is_empty() && !is_format_char(ch);
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
    Some(ensure_ellipsis(line, max_chars))
}

/// 零宽字符：第三方 agent 名里实测有 U+200B，展示时去掉。
fn is_format_char(ch: char) -> bool {
    matches!(ch, '\u{200B}'..='\u{200F}' | '\u{FEFF}' | '\u{2060}')
}

/// 超过上限截断并以省略号结尾；已经以省略号结尾且在上限内则原样返回。
fn ensure_ellipsis(line: String, max_chars: usize) -> String {
    if line.chars().count() > max_chars {
        let mut clipped: String = line.chars().take(max_chars.saturating_sub(1)).collect();
        clipped.push('…');
        return clipped;
    }
    if line.ends_with('…') {
        return line;
    }
    if line.chars().count() == max_chars {
        let mut clipped: String = line.chars().take(max_chars.saturating_sub(1)).collect();
        clipped.push('…');
        return clipped;
    }
    line
}

/// 上游（SQL 的 `substr`）已截断的一行：保证以省略号结尾且不超过 `max_chars`。清洗
/// 压掉连续空白后这一行可能短于上限，这时 [`ensure_ellipsis`] 不补省略号，截断的痕迹
/// 就丢了。
fn clipped_line(line: String, max_chars: usize) -> String {
    if line.ends_with('…') {
        return ensure_ellipsis(line, max_chars);
    }
    let mut clipped: String = line.chars().take(max_chars.saturating_sub(1)).collect();
    clipped.push('…');
    clipped
}

/// 内容片段只保留换行与制表符两种控制字符。
fn clean_content(text: &str) -> String {
    text.chars()
        .filter(|ch| !ch.is_control() || matches!(ch, '\n' | '\t'))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::agent_resume::AgentSessionRef;

    const ROOT: &str = "ses_root0000000000000000000000";
    const NOW_MS: u64 = 1_726_990_000_000;

    fn fixture_path(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/agent-activity/opencode")
            .join(name)
    }

    fn fixture(name: &str) -> String {
        fs::read_to_string(fixture_path(name))
            .unwrap_or_else(|error| panic!("读取夹具 {name}：{error}"))
    }

    /// 带数据库文件的临时 home；`Drop` 时删除。
    struct Home {
        path: PathBuf,
    }

    impl Home {
        fn with_database() -> Self {
            let home = Self::empty();
            let dir = data_dir(&home.path, None);
            fs::create_dir_all(&dir).expect("数据目录可创建");
            fs::write(dir.join(DATABASE_FILE), b"").expect("数据库占位文件可写");
            home
        }

        fn empty() -> Self {
            let path = std::env::temp_dir().join(format!(
                "herdr-opencode-activity-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("系统时钟在纪元之后")
                    .as_nanos()
            ));
            fs::create_dir_all(&path).expect("临时 home 可创建");
            Self { path }
        }
    }

    impl Drop for Home {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    /// 按 SQL 片段应答的假 CLI：`(SQL 必须包含的片段, 应答)`，先匹配先用；记录全部 SQL。
    struct FakeDb {
        answers: Vec<(&'static str, Result<String, &'static str>)>,
        queries: RefCell<Vec<String>>,
    }

    impl FakeDb {
        fn new(answers: Vec<(&'static str, Result<String, &'static str>)>) -> Self {
            Self {
                answers,
                queries: RefCell::new(Vec::new()),
            }
        }

        fn queries(&self) -> Vec<String> {
            self.queries.borrow().clone()
        }
    }

    impl DbQuery for FakeDb {
        fn query(&self, sql: &str) -> Result<String, SourceError> {
            self.queries.borrow_mut().push(sql.to_owned());
            let (_, answer) = self
                .answers
                .iter()
                .find(|(needle, _)| sql.contains(needle))
                .unwrap_or_else(|| panic!("没有为该 SQL 准备应答：{sql}"));
            match answer {
                Ok(output) => Ok(output.clone()),
                Err("unavailable") => Err(SourceError::Unavailable),
                Err(reason) => Err(SourceError::Malformed((*reason).to_owned())),
            }
        }
    }

    fn context<'a>(home: &'a Path, session: Option<&'a AgentSessionRef>) -> SourceContext<'a> {
        SourceContext {
            agent: "opencode",
            session,
            cwd: None,
            home,
            now_ms: NOW_MS,
            agent_config_dir: None,
            latest_hint: None,
        }
    }

    fn normal_db() -> FakeDb {
        FakeDb::new(vec![
            ("WITH RECURSIVE", Ok(fixture("sessions-normal.json"))),
            ("FROM todo", Ok(fixture("todos-normal.json"))),
        ])
    }

    fn discover(db: &FakeDb) -> Vec<AgentActivityNode> {
        let home = Home::with_database();
        let session = AgentSessionRef::id(ROOT).expect("合法会话 id");
        discover_with(&context(&home.path, Some(&session)), db, None).expect("树可发现")
    }

    fn read(
        db: &FakeDb,
        node_id: &str,
        cursor: Option<&str>,
        max_bytes: usize,
    ) -> Result<ContentChunk, SourceError> {
        let home = Home::with_database();
        let session = AgentSessionRef::id(ROOT).expect("合法会话 id");
        read_with(
            &context(&home.path, Some(&session)),
            db,
            None,
            node_id,
            cursor,
            max_bytes,
        )
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

    #[test]
    fn trait_entry_points_need_a_session_reference() {
        let home = Home::with_database();
        let cx = context(&home.path, None);
        assert_eq!(OpenCode.id(), "opencode");
        assert!(matches!(
            OpenCode.discover(&cx),
            Err(SourceError::Unsupported)
        ));
        assert!(matches!(
            OpenCode.read(&cx, "session:ses_x", None, 1024),
            Err(SourceError::Unsupported)
        ));
        let db = normal_db();
        assert!(matches!(
            discover_with(&cx, &db, None),
            Err(SourceError::Unsupported)
        ));
        assert!(db.queries().is_empty(), "没有会话引用时不得启动 CLI");

        // 会话引用要拼进 SQL：不是 opencode id 形状一律拒绝，不启动 CLI。
        let odd = AgentSessionRef::id("ses_x' OR 1=1 --").expect("引用本身合法");
        assert!(matches!(
            discover_with(&context(&home.path, Some(&odd)), &db, None),
            Err(SourceError::Malformed(_))
        ));
        assert!(db.queries().is_empty());
    }

    #[test]
    fn missing_database_reads_as_empty_tree_and_unavailable_content() {
        let home = Home::empty();
        let session = AgentSessionRef::id(ROOT).expect("合法会话 id");
        let cx = context(&home.path, Some(&session));
        let db = normal_db();
        assert!(discover_with(&cx, &db, None).expect("空树").is_empty());
        assert!(matches!(
            read_with(&cx, &db, None, "session:ses_child", None, 1024),
            Err(SourceError::Unavailable)
        ));
        assert!(db.queries().is_empty(), "库文件不存在时不得启动 CLI");

        // XDG_DATA_HOME 覆盖数据目录，`~` 按 home 展开。
        let custom = home.path.join("xdg");
        fs::create_dir_all(custom.join("opencode")).expect("目录可创建");
        fs::write(custom.join("opencode").join(DATABASE_FILE), b"").expect("可写");
        assert_eq!(
            data_dir(&home.path, Some(OsString::from("~/xdg"))),
            custom.join("opencode")
        );
        assert!(database_exists(&home.path, Some(custom.clone().into())));
        assert!(!database_exists(&home.path, Some(OsString::from(""))));
        assert!(!database_exists(&home.path, None));
    }

    #[test]
    fn session_tree_is_parent_first_with_status_from_last_message() {
        let db = normal_db();
        let nodes = discover(&db);
        assert_eq!(
            ids(&nodes),
            [
                "todo:ses_root0000000000000000000000:0",
                "todo:ses_root0000000000000000000000:1",
                "todo:ses_root0000000000000000000000:2",
                "session:ses_done000000000000000000000",
                "todo:ses_done000000000000000000000:0",
                "session:ses_running0000000000000000000",
                "session:ses_failed00000000000000000000",
                "session:ses_pending0000000000000000000",
                "session:ses_archived000000000000000000",
                "session:ses_stale000000000000000000000",
                "session:ses_grand00000000000000000000",
                "session:ses_great00000000000000000000",
            ]
        );

        let done = node(&nodes, "session:ses_done000000000000000000000");
        assert_eq!(done.kind, AgentActivityKind::Subagent);
        assert_eq!(done.status, AgentActivityStatus::Done);
        assert_eq!(done.label, "Explore the docs");
        assert_eq!(done.agent_type.as_deref(), Some("explore"));
        assert_eq!(done.parent_id, None, "根的直接子会话挂在顶层");
        assert_eq!(
            done.content_ref.as_deref(),
            Some("session:ses_done000000000000000000000")
        );
        assert_eq!(done.started_at_ms, Some(1_726_989_000_000));
        assert_eq!(done.ended_at_ms, Some(1_726_989_060_000));
        assert_eq!(
            done.summary.as_deref(),
            Some("claude-sonnet-4 · $0.12 · 23.8k in · 3.9k out")
        );

        let running = node(&nodes, "session:ses_running0000000000000000000");
        assert_eq!(running.status, AgentActivityStatus::Running);
        assert_eq!(running.ended_at_ms, None);
        assert_eq!(
            running.agent_type.as_deref(),
            Some("Sisyphus - Ultraworker"),
            "agent 列优先于标题后缀，零宽字符去掉"
        );
        assert_eq!(running.label, "Refactor the parser");

        let failed = node(&nodes, "session:ses_failed00000000000000000000");
        assert_eq!(failed.status, AgentActivityStatus::Failed);
        assert_eq!(failed.ended_at_ms, Some(1_726_989_900_000));
        assert_eq!(
            failed.agent_type.as_deref(),
            Some("librarian"),
            "agent 列缺失时从标题后缀取"
        );

        let pending = node(&nodes, "session:ses_pending0000000000000000000");
        assert_eq!(pending.status, AgentActivityStatus::Pending);
        assert_eq!(pending.summary, None, "没有模型与用量时不给摘要");
        assert_eq!(
            pending.label, "Untitled child",
            "不是 subagent 形状的标题整个当标题"
        );

        let archived = node(&nodes, "session:ses_archived000000000000000000");
        assert_eq!(archived.status, AgentActivityStatus::Done);
        assert_eq!(archived.ended_at_ms, Some(1_726_989_990_000));
        assert!(archived
            .summary
            .as_deref()
            .is_some_and(|summary| summary.ends_with("archived")));

        let stale = node(&nodes, "session:ses_stale000000000000000000000");
        assert_eq!(
            stale.status,
            AgentActivityStatus::Unknown,
            "最后一条是 user 且很久没更新：不再算运行中"
        );

        let grand = node(&nodes, "session:ses_grand00000000000000000000");
        assert_eq!(
            grand.parent_id.as_deref(),
            Some("session:ses_done000000000000000000000")
        );
        let great = node(&nodes, "session:ses_great00000000000000000000");
        assert_eq!(
            great.parent_id.as_deref(),
            Some("session:ses_grand00000000000000000000")
        );

        let root_todo = node(&nodes, "todo:ses_root0000000000000000000000:0");
        assert_eq!(root_todo.kind, AgentActivityKind::Todo);
        assert_eq!(root_todo.status, AgentActivityStatus::Running);
        assert_eq!(root_todo.label, "Write the parser tests");
        assert_eq!(root_todo.summary.as_deref(), Some("high"));
        assert_eq!(root_todo.parent_id, None);
        assert_eq!(root_todo.content_ref, None);
        assert_eq!(root_todo.ended_at_ms, None);
        let completed = node(&nodes, "todo:ses_root0000000000000000000000:1");
        assert_eq!(completed.status, AgentActivityStatus::Done);
        assert_eq!(completed.ended_at_ms, Some(1_726_989_500_000));
        let pending_todo = node(&nodes, "todo:ses_root0000000000000000000000:2");
        assert_eq!(pending_todo.status, AgentActivityStatus::Pending);
        let child_todo = node(&nodes, "todo:ses_done000000000000000000000:0");
        assert_eq!(
            child_todo.parent_id.as_deref(),
            Some("session:ses_done000000000000000000000")
        );
        assert_eq!(child_todo.status, AgentActivityStatus::Done);
        assert_eq!(child_todo.summary.as_deref(), Some("cancelled · low"));

        // 待办查询只问树里的会话，id 全部单引号包裹。
        let queries = db.queries();
        assert_eq!(queries.len(), 2);
        assert!(queries[0].contains(&format!("WHERE id = '{ROOT}'")));
        assert!(queries[0].contains("LIMIT 48 OFFSET 0"));
        assert!(queries[1].contains(&format!("'{ROOT}', 'ses_done000000000000000000000'")));
    }

    #[test]
    fn root_session_absent_from_database_reads_as_empty_tree() {
        let db = FakeDb::new(vec![
            ("WITH RECURSIVE", Ok("[]\n".into())),
            ("FROM part", Ok("[]\n".into())),
            ("SELECT 1 AS present", Ok("[]\n".into())),
        ]);
        assert!(discover(&db).is_empty());
        assert_eq!(db.queries().len(), 1, "根不在库里就不再查待办");
        assert!(matches!(
            read(&db, "session:ses_missing000000000000000000", None, 1024),
            Err(SourceError::Unavailable)
        ));

        // 会话在、只是还没有部件：空片段且 eof，游标仍给出。
        let empty = FakeDb::new(vec![
            ("FROM part", Ok("[]\n".into())),
            ("SELECT 1 AS present", Ok(r#"[{"present":1}]"#.into())),
        ]);
        let page =
            read(&empty, "session:ses_empty00000000000000000000", None, 1024).expect("会话存在");
        assert!(page.eof && page.text.is_empty() && !page.truncated);
        assert_eq!(page.next_cursor.as_deref(), Some("0"));
    }

    #[test]
    fn missing_fields_degrade_to_defaults() {
        let db = FakeDb::new(vec![
            (
                "WITH RECURSIVE",
                Ok(fixture("sessions-missing-fields.json")),
            ),
            ("FROM todo", Ok(fixture("todos-missing-fields.json"))),
        ]);
        let nodes = discover(&db);
        assert_eq!(
            ids(&nodes),
            [
                "session:ses_bare0000000000000000000000",
                "session:ses_orphan00000000000000000000",
                "session:ses_self0000000000000000000000",
                "todo:ses_self0000000000000000000000:3",
                "session:ses_child000000000000000000000",
            ]
        );

        let bare = node(&nodes, "session:ses_bare0000000000000000000000");
        assert_eq!(
            bare.label, "ses_bare0000000000000000000000",
            "缺标题退回 id"
        );
        assert_eq!(bare.agent_type, None);
        assert_eq!(bare.summary, None);
        assert_eq!(bare.started_at_ms, None);
        assert_eq!(
            bare.status,
            AgentActivityStatus::Unknown,
            "既没有消息也没有更新时间"
        );

        assert_eq!(
            node(&nodes, "session:ses_orphan00000000000000000000").parent_id,
            None,
            "父会话不在结果里挂到根"
        );
        assert_eq!(
            node(&nodes, "session:ses_self0000000000000000000000").parent_id,
            None,
            "指向自己挂到根"
        );
        let child = node(&nodes, "session:ses_child000000000000000000000");
        assert_eq!(
            child.parent_id.as_deref(),
            Some("session:ses_bare0000000000000000000000"),
            "深度乱序的行也按深度排回父先子后"
        );
        assert_eq!(child.summary.as_deref(), Some("1 in · 0 out"));

        let todo = node(&nodes, "todo:ses_self0000000000000000000000:3");
        assert_eq!(
            todo.label, "todo:ses_self0000000000000000000000:3",
            "缺内容退回 id"
        );
        assert_eq!(todo.status, AgentActivityStatus::Unknown);
        assert_eq!(todo.summary, None);
    }

    #[test]
    fn unknown_enum_values_fall_back_to_unknown() {
        let db = FakeDb::new(vec![
            (
                "WITH RECURSIVE",
                Ok(fixture("sessions-unknown-values.json")),
            ),
            ("FROM todo", Ok(fixture("todos-unknown-values.json"))),
        ]);
        let nodes = discover(&db);
        let system = node(&nodes, "session:ses_system00000000000000000000");
        assert_eq!(system.status, AgentActivityStatus::Unknown, "未知 role");
        assert_eq!(
            system.summary.as_deref(),
            Some("2 in · 3 out"),
            "费用不是数字时忽略"
        );
        let odd = node(&nodes, "session:ses_negative000000000000000000");
        assert_eq!(odd.started_at_ms, None, "负数时间戳丢弃");
        assert_eq!(odd.summary, None, "负数 token 按 0");
        assert_eq!(
            odd.status,
            AgentActivityStatus::Failed,
            "error 是对象即失败，不看 finish 的取值"
        );

        let weird = node(&nodes, "todo:ses_system00000000000000000000:0");
        assert_eq!(weird.status, AgentActivityStatus::Unknown);
        assert_eq!(
            weird.summary.as_deref(),
            Some("urgent"),
            "未知优先级原样展示"
        );
        let numeric = node(&nodes, "todo:ses_system00000000000000000000:1");
        assert_eq!(numeric.status, AgentActivityStatus::Unknown, "非字符串状态");
        assert_eq!(numeric.summary, None);
    }

    /// 复验 N15：子会话最后一条 assistant 消息带 `error` 时节点落 failed，摘要以错误名
    /// 与说明开头，排在模型与用量之前。`error` 的形状按 opencode 1.18.32 捆绑源码核实
    /// （`{name, data: {message?}}`，见模块文档），夹具是手写的行，不来自任何真实会话。
    /// 说明压成单行、过长截断带省略号；没有说明时只写错误名，没有错误名时只写说明。
    #[test]
    fn failed_sessions_lead_their_summary_with_the_error() {
        let db = FakeDb::new(vec![
            ("WITH RECURSIVE", Ok(fixture("sessions-errors.json"))),
            ("FROM todo", Ok("[]".to_owned())),
        ]);
        let nodes = discover(&db);

        let api = node(&nodes, "session:ses_apierror000000000000000000");
        assert_eq!(api.status, AgentActivityStatus::Failed);
        assert_eq!(
            api.summary.as_deref(),
            Some("APIError: Rate limit exceeded, retry after 60s · glm-4.7 · $0.01 · 1.2k in · 0 out")
        );
        assert_eq!(api.ended_at_ms, Some(1_726_989_060_000));

        let auth = node(&nodes, "session:ses_autherror00000000000000000");
        assert_eq!(
            auth.status,
            AgentActivityStatus::Failed,
            "没有 time.completed 也按错误落 failed"
        );
        assert_eq!(
            auth.summary.as_deref(),
            Some("ProviderAuthError: Invalid API key · glm-4.7")
        );

        let length = node(&nodes, "session:ses_lengthcap00000000000000000");
        assert_eq!(length.status, AgentActivityStatus::Failed);
        assert_eq!(
            length.summary.as_deref(),
            Some("MessageOutputLengthError · glm-4.7 · 900 in · 8.2k out"),
            "没有说明的错误只写错误名"
        );

        let noisy = node(&nodes, "session:ses_noisyerror000000000000000");
        assert_eq!(noisy.status, AgentActivityStatus::Failed);
        let summary = noisy.summary.as_deref().expect("失败节点带摘要");
        assert!(
            summary.starts_with("UnknownError: upstream said: 502 Bad Gateway while streaming"),
            "控制字符与换行压成空格：{summary}"
        );
        assert!(
            summary.ends_with("… · glm-4.7"),
            "过长的说明截断带省略号，模型仍在：{summary}"
        );
        assert!(!summary.contains('\n') && !summary.contains('\u{7}'));

        let nameless = node(&nodes, "session:ses_namelesserror0000000000000");
        assert_eq!(nameless.status, AgentActivityStatus::Failed);
        assert_eq!(nameless.summary.as_deref(), Some("no name on this error"));

        // 真机 N15 的形态（默认模型已不在提供商目录）：opencode 1.18.32 在
        // `SessionPrompt.getModel` 里只经 Bus 发不落库的 `session.error`，然后在建
        // assistant 消息之前退出，库里最后一条仍是 user 消息、没有任何错误字段。
        // 库里看不出失败，只能按最后活动时间：30 分钟内算运行中，之后降为未知。
        let gone = node(&nodes, "session:ses_modelgone0000000000000000");
        assert_eq!(gone.status, AgentActivityStatus::Running);
        assert_eq!(gone.summary.as_deref(), Some("glm-4.6"));
    }

    /// 审查轻 5：错误说明在 SQL 里先截到 [`SQL_ERROR_CHARS`]、再由 `clean_line` 裁到
    /// [`MAX_ERROR_CHARS`]；压掉连续空白后不足显示上限时，截断的痕迹（省略号）曾经丢掉。
    /// 现在按 SQL 给出的原长判定截断，截过就以省略号结尾；标题（SQL 截 160、显示 120）
    /// 同理。没截过、或旧形状的行没有原长时照旧不加。
    #[test]
    fn text_clipped_in_sql_keeps_its_ellipsis_after_whitespace_collapses() {
        let spaced_error = format!("upstream{}timed out", " ".repeat(103));
        assert_eq!(spaced_error.chars().count(), SQL_ERROR_CHARS);
        let spaced_title = format!("Fix{}the flaky tests", " ".repeat(142));
        assert_eq!(spaced_title.chars().count(), SQL_TITLE_CHARS);
        let failed = |id: &str, message: &str, message_len: Option<u64>| {
            serde_json::json!({
                "id": id, "parent_id": ROOT, "depth": 1, "model_id": "glm-4.7",
                "title": "t", "title_len": 1, "time_updated": NOW_MS,
                "last_role": "assistant", "last_completed": NOW_MS,
                "last_error": "object", "last_error_name": "UnknownError",
                "last_error_message": message, "last_error_message_len": message_len,
            })
        };
        let sessions = serde_json::json!([
            {"id": ROOT, "depth": 0},
            failed("ses_clipped00000000000000000000", &spaced_error, Some(4000)),
            failed("ses_whole0000000000000000000000", "short and whole", Some(15)),
            failed("ses_oldshape000000000000000000", "no length column", None),
            {
                "id": "ses_title0000000000000000000000", "parent_id": ROOT, "depth": 1,
                "title": spaced_title, "title_len": 900, "time_updated": NOW_MS,
            },
        ])
        .to_string();
        let db = FakeDb::new(vec![
            ("WITH RECURSIVE", Ok(sessions)),
            ("FROM todo", Ok("[]".into())),
        ]);
        let nodes = discover(&db);

        let clipped = node(&nodes, "session:ses_clipped00000000000000000000");
        assert_eq!(
            clipped.summary.as_deref(),
            Some("UnknownError: upstream timed out… · glm-4.7")
        );
        let whole = node(&nodes, "session:ses_whole0000000000000000000000");
        assert_eq!(
            whole.summary.as_deref(),
            Some("UnknownError: short and whole · glm-4.7")
        );
        let old = node(&nodes, "session:ses_oldshape000000000000000000");
        assert_eq!(
            old.summary.as_deref(),
            Some("UnknownError: no length column · glm-4.7")
        );
        let title = node(&nodes, "session:ses_title0000000000000000000000");
        assert_eq!(title.label, "Fix the flaky tests…");
        assert!(
            tree_sql(ROOT, 0).contains(
                "length(json_extract(lm.data, '$.error.data.message')) END AS last_error_message_len"
            ),
            "原长在 SQL 里算，说明本身仍只取回截断后的一段"
        );
    }

    #[test]
    fn tree_sql_clips_the_last_error_instead_of_fetching_it_whole() {
        let sql = tree_sql("ses_abc", 0);
        assert!(
            sql.contains("json_extract(lm.data, '$.error.name') END AS last_error_name"),
            "{sql}"
        );
        assert!(
            sql.contains(&format!(
                "substr(json_extract(lm.data, '$.error.data.message'), 1, {SQL_ERROR_CHARS}) END AS last_error_message"
            )),
            "错误说明在 SQL 里先截断，响应体等大字段永不取回：{sql}"
        );
        assert!(!sql.contains("responseBody"));
    }

    #[test]
    fn broken_cli_output_is_reported_without_a_tree() {
        let home = Home::with_database();
        let session = AgentSessionRef::id(ROOT).expect("合法会话 id");
        let cx = context(&home.path, Some(&session));

        // 管道截断：不以 `]` 结尾 → 稍后重试。
        let truncated = FakeDb::new(vec![("WITH RECURSIVE", Ok(fixture("db-truncated.txt")))]);
        assert!(matches!(
            discover_with(&cx, &truncated, None),
            Err(SourceError::Unavailable)
        ));
        // 默认 tsv 形态（没带 --format json 的输出）同样按截断处理。
        let tsv = FakeDb::new(vec![("WITH RECURSIVE", Ok(fixture("db-not-json.txt")))]);
        assert!(matches!(
            discover_with(&cx, &tsv, None),
            Err(SourceError::Unavailable)
        ));
        // 完整但不是数组 / 不是 JSON。
        let object = FakeDb::new(vec![("WITH RECURSIVE", Ok(r#"{"rows":[]}"#.into()))]);
        assert!(matches!(
            discover_with(&cx, &object, None),
            Err(SourceError::Malformed(_))
        ));
        let junk = FakeDb::new(vec![("WITH RECURSIVE", Ok("[1, 2, oops]".into()))]);
        assert!(matches!(
            discover_with(&cx, &junk, None),
            Err(SourceError::Malformed(_))
        ));
        // CLI 自身失败原样透传。
        let failing = FakeDb::new(vec![("WITH RECURSIVE", Err("unavailable"))]);
        assert!(matches!(
            discover_with(&cx, &failing, None),
            Err(SourceError::Unavailable)
        ));

        // 数组里的坏元素只丢它自己：非对象、缺 id、id 带引号。
        let mixed = FakeDb::new(vec![
            ("WITH RECURSIVE", Ok(fixture("sessions-bad-rows.json"))),
            ("FROM todo", Ok("[]".into())),
        ]);
        let nodes = discover_with(&cx, &mixed, None).expect("坏行不影响整棵树");
        assert_eq!(ids(&nodes), ["session:ses_good0000000000000000000000"]);

        // 待办查询失败只丢待办。
        let todo_failure = FakeDb::new(vec![
            ("WITH RECURSIVE", Ok(fixture("sessions-normal.json"))),
            ("FROM todo", Err("unavailable")),
        ]);
        let nodes = discover_with(&cx, &todo_failure, None).expect("树照常");
        assert_eq!(nodes.len(), 8);
        assert!(nodes
            .iter()
            .all(|node| node.kind == AgentActivityKind::Subagent));
    }

    #[test]
    fn deep_nesting_keeps_every_level_parent_first() {
        let db = FakeDb::new(vec![
            ("WITH RECURSIVE", Ok(fixture("sessions-deep.json"))),
            ("FROM todo", Ok("[]".into())),
        ]);
        let nodes = discover(&db);
        let expected: Vec<String> = (1..=40)
            .map(|level| format!("session:ses_deep{level:02}00000000000000000000"))
            .collect();
        assert_eq!(ids(&nodes), expected);
        assert_eq!(nodes[0].parent_id, None);
        for pair in nodes.windows(2) {
            assert_eq!(pair[1].parent_id.as_deref(), Some(pair[0].id.as_str()));
        }
    }

    /// 超过一页的树分多次查询取回；超过总上限的层级被截掉后，其子会话挂到根。
    #[test]
    fn large_trees_are_fetched_page_by_page() {
        fn row(index: usize, parent: Option<usize>) -> String {
            let depth = usize::from(parent.is_some());
            let parent = match parent {
                Some(parent) => format!("\"ses_wide{parent:03}0000000000000000000\""),
                None => "null".into(),
            };
            format!(
                r#"{{"id":"ses_wide{index:03}0000000000000000000","parent_id":{parent},"depth":{depth},"title":"child {index}","time_created":{created},"time_updated":{created}}}"#,
                created = 1_726_989_000_000_u64 + index as u64
            )
        }
        let mut rows = vec![format!(
            r#"{{"id":"{ROOT}","parent_id":null,"depth":0,"title":"root","time_created":1726989000000,"time_updated":1726989000000}}"#
        )];
        rows.extend(
            (1..70)
                .map(|index| row(index, Some(0)).replace("ses_wide0000000000000000000000", ROOT)),
        );
        let page =
            |from: usize, to: usize| format!("[{}]", rows[from..to.min(rows.len())].join(","));
        let db = FakeDb::new(vec![
            ("OFFSET 0", Ok(page(0, 48))),
            ("OFFSET 48", Ok(page(48, 96))),
            ("FROM todo", Ok("[]".into())),
        ]);
        let nodes = discover(&db);
        assert_eq!(nodes.len(), 69);
        assert_eq!(db.queries().len(), 3, "两页树查询 + 一次待办查询");
        assert!(nodes.iter().all(|node| node.parent_id.is_none()));

        // 超过 256 个会话：只取前 256 个，后面的查询不再发。
        let many: Vec<String> =
            std::iter::once(rows[0].clone())
                .chain((1..400).map(|index| {
                    row(index, Some(0)).replace("ses_wide0000000000000000000000", ROOT)
                }))
                .collect();
        let mut answers: Vec<(&'static str, Result<String, &'static str>)> = Vec::new();
        for (offset, needle) in [
            (0, "OFFSET 0"),
            (48, "OFFSET 48"),
            (96, "OFFSET 96"),
            (144, "OFFSET 144"),
            (192, "OFFSET 192"),
            (240, "OFFSET 240"),
            (288, "OFFSET 288"),
        ] {
            answers.push((
                needle,
                Ok(format!("[{}]", many[offset..offset + 48].join(","))),
            ));
        }
        answers.push(("FROM todo", Ok("[]".into())));
        let db = FakeDb::new(answers);
        let nodes = discover(&db);
        assert_eq!(nodes.len(), MAX_SESSIONS - 1, "根不出节点");
        assert_eq!(db.queries().len(), 7, "取满 256 个后停止翻页");
    }

    #[test]
    fn read_pages_parts_by_row_cursor() {
        let db = FakeDb::new(vec![
            ("OFFSET 0", Ok(fixture("parts-page-1.json"))),
            ("OFFSET 4", Ok(fixture("parts-page-2.json"))),
            ("OFFSET 6", Ok("[]".into())),
        ]);
        let node_id = "session:ses_done000000000000000000000";
        // 4 KiB 预算 → 每页 4 行（多取 1 行探测是否还有）。
        let first = read(&db, node_id, None, 4096).expect("首页");
        assert!(!first.eof);
        assert_eq!(first.next_cursor.as_deref(), Some("4"));
        assert_eq!(first.format, AgentActivityContentFormat::Text);
        assert_eq!(
            first.text,
            "user: Find the docs\n(thinking) Look under docs/ first\n[grep] grep docs\n    docs/README.md\n"
        );
        assert!(!first.truncated);
        assert!(db.queries()[0].contains("LIMIT 5 OFFSET 0"));

        let second = read(&db, node_id, Some("4"), 4096).expect("次页");
        assert!(second.eof);
        assert_eq!(second.next_cursor.as_deref(), Some("6"));
        assert_eq!(second.text, "Found docs/README.md\n[file] notes.md\n");

        // eof 后拿着游标续读：空页、游标不动。
        let third = read(&db, node_id, Some("6"), 4096).expect("续读");
        assert!(third.eof && third.text.is_empty());
        assert_eq!(third.next_cursor.as_deref(), Some("6"));
        assert_eq!(db.queries().len(), 3, "有部件就不再查会话是否存在");
    }

    #[test]
    fn read_leaves_an_in_progress_tail_for_the_next_page() {
        let db = FakeDb::new(vec![("FROM part", Ok(fixture("parts-in-progress.json")))]);
        let node_id = "session:ses_running0000000000000000000";
        let page = read(&db, node_id, None, 64 * 1024).expect("可读");
        assert!(page.eof, "暂时读完");
        assert_eq!(
            page.next_cursor.as_deref(),
            Some("2"),
            "末尾连续的生成中部件（运行中的工具 + 流式文本）都不消费"
        );
        assert_eq!(
            page.text,
            "user: Run the suite…\n[bash] cargo build\n    warning: unused…\n"
        );
        assert!(page.truncated, "用户文本与工具输出都被 SQL 截断");

        // 卡在中间的运行中工具不挡路：后面已有完成的部件时照常消费。
        let db = FakeDb::new(vec![("FROM part", Ok(fixture("parts-stuck-tool.json")))]);
        let page = read(&db, node_id, None, 64 * 1024).expect("可读");
        assert!(page.eof);
        assert_eq!(page.next_cursor.as_deref(), Some("3"));
        assert_eq!(
            page.text,
            "user: Run it\n[bash] sleep (running)\nDone anyway\n"
        );
        assert!(!page.truncated);

        // 只剩生成中的部件：空页、游标不动，仍是 eof。
        let db = FakeDb::new(vec![("FROM part", Ok(fixture("parts-streaming.json")))]);
        let page = read(&db, node_id, None, 64 * 1024).expect("可读");
        assert!(page.eof && page.text.is_empty());
        assert_eq!(page.next_cursor.as_deref(), Some("0"));
    }

    #[test]
    fn read_honours_the_byte_budget_and_always_advances() {
        let db = FakeDb::new(vec![("FROM part", Ok(fixture("parts-page-1.json")))]);
        let node_id = "session:ses_done000000000000000000000";
        // 预算装不下第二行：只消费一行，未到 eof。
        let page = read(&db, node_id, None, 24).expect("可读");
        assert_eq!(page.text, "user: Find the docs\n");
        assert_eq!(page.next_cursor.as_deref(), Some("1"));
        assert!(!page.eof);
        assert!(
            db.queries()[0].contains("LIMIT 5 OFFSET 0"),
            "预算再小也至少取 4 行"
        );

        // 预算为 0 用默认值：一页最多 24 行。
        let _ = read(&db, node_id, None, 0);
        assert!(db.queries()[1].contains("LIMIT 25 OFFSET 0"));
        let _ = read(&db, node_id, None, 1 << 20);
        assert!(db.queries()[2].contains("LIMIT 25 OFFSET 0"), "上限 24 行");
    }

    #[test]
    fn read_rejects_bad_cursors_and_unreadable_nodes() {
        let db = FakeDb::new(vec![("FROM part", Ok("[]".into()))]);
        assert!(matches!(
            read(
                &db,
                "session:ses_done000000000000000000000",
                Some("abc"),
                32
            ),
            Err(SourceError::Malformed(_))
        ));
        assert!(matches!(
            read(&db, "todo:ses_done000000000000000000000:0", None, 32),
            Err(SourceError::Unavailable)
        ));
        assert!(matches!(
            read(&db, "session:ses_x'; DROP TABLE part; --", None, 32),
            Err(SourceError::Malformed(_))
        ));
        assert!(matches!(
            read(&db, "", None, 32),
            Err(SourceError::Unavailable)
        ));
        assert!(db.queries().is_empty(), "参数不合法不启动 CLI");

        let truncated = FakeDb::new(vec![("FROM part", Ok(fixture("db-truncated.txt")))]);
        assert!(matches!(
            read(
                &truncated,
                "session:ses_done000000000000000000000",
                None,
                32
            ),
            Err(SourceError::Unavailable)
        ));
    }

    #[test]
    fn display_text_is_single_line_and_content_keeps_only_newlines_and_tabs() {
        let long = "x".repeat(300);
        let sessions = format!(
            r#"[{{"id":"{ROOT}","depth":0}},
              {{"id":"ses_long0000000000000000000000","parent_id":"{ROOT}","depth":1,
                "title":"line one\n\u001b[31mline\ttwo {long}","title_len":400,
                "agent":"  \u0007\n  ","time_updated":{NOW_MS}}}]"#
        );
        let parts = r#"[{"id":"prt_1","role":"assistant","message_completed":1,"type":"text","text":"a\u0007b\tc\r\nd\u001be","text_len":9}]"#;
        let db = FakeDb::new(vec![
            ("WITH RECURSIVE", Ok(sessions)),
            ("FROM todo", Ok("[]".into())),
            ("FROM part", Ok(parts.into())),
        ]);
        let nodes = discover(&db);
        let label = &nodes[0].label;
        assert!(label.starts_with("line one [31mline two x"));
        assert_eq!(label.chars().count(), MAX_LABEL_CHARS);
        assert!(label.ends_with('…'));
        assert!(!label.chars().any(char::is_control));
        assert_eq!(nodes[0].agent_type, None, "只有空白的 agent 名按缺失处理");

        let page = read(&db, "session:ses_long0000000000000000000000", None, 1024).expect("可读");
        assert_eq!(page.text, "ab\tc\nde\n");
        assert!(page.eof);
    }

    #[test]
    fn sql_only_embeds_validated_identifiers() {
        assert!(valid_id("ses_0123456789abcdefABCDEF_-"));
        assert!(!valid_id(""));
        assert!(!valid_id("ses_x y"));
        assert!(!valid_id("ses_x'"));
        assert!(!valid_id(&"a".repeat(MAX_ID_LEN + 1)));
        let sql = tree_sql("ses_abc", 96);
        assert!(sql.contains("WHERE id = 'ses_abc'"));
        assert!(sql.contains("t.depth < 128"));
        assert!(sql.ends_with("LIMIT 48 OFFSET 96"));
        assert!(
            !sql.contains("s.model AS"),
            "model 列只取模型名，不整列取回"
        );
        assert!(
            sql.contains("json_extract(s.model, '$.id')")
                && sql.contains("json_extract(s.model, '$.modelID')"),
            "模型名走实测的 $.id，$.modelID 只作回退"
        );
        let parts = parts_sql("ses_abc", 7, 5);
        assert!(parts.contains("p.session_id = 'ses_abc'"));
        assert!(parts.ends_with("LIMIT 5 OFFSET 7"));
        assert!(!parts.contains("'$.system'"), "永不取回系统提示");
    }

    // -----------------------------------------------------------------------
    // 只读键名探针（默认不跑，见文件头「键名漂移补验」）
    // -----------------------------------------------------------------------

    /// 探针把查询包成聚合时用的行上限：聚合后只有计数进 stdout，不受管道截断影响。
    const PROBE_ROW_LIMIT: usize = 1_000_000;

    /// 取一行的键集合（排序）；行值当场丢弃，正文不外泄。
    fn alias_set(db: &dyn DbQuery, sql: &str) -> Option<Vec<String>> {
        let output = db
            .query(&format!("SELECT * FROM ({sql}) LIMIT 1"))
            .expect("包装查询可执行");
        let rows = parse_rows(&output).expect("包装查询返回 JSON 数组");
        rows.first().map(|row| {
            let mut keys: Vec<String> = row.keys().cloned().collect();
            keys.sort();
            keys
        })
    }

    /// 把查询包成聚合，只取 `row_total` 与各列的非空计数；行值永远不进 stdout。
    fn non_null_counts(db: &dyn DbQuery, sql: &str, columns: &[&str]) -> BTreeMap<String, u64> {
        let projection: Vec<String> = columns
            .iter()
            .map(|column| format!("COUNT(\"{column}\") AS \"{column}\""))
            .collect();
        let output = db
            .query(&format!(
                "SELECT COUNT(*) AS row_total, {} FROM ({sql})",
                projection.join(", ")
            ))
            .expect("聚合查询可执行");
        let rows = parse_rows(&output).expect("聚合查询返回 JSON 数组");
        let row = rows.first().expect("聚合查询恰好一行");
        row.iter()
            .filter_map(|(key, value)| {
                non_negative_integer(value).map(|count| (key.clone(), count))
            })
            .collect()
    }

    fn count_of(counts: &BTreeMap<String, u64>, column: &str) -> u64 {
        counts.get(column).copied().unwrap_or(0)
    }

    /// 取一列名为 `id` 的标量。
    fn scalar_id(db: &dyn DbQuery, sql: &str) -> Option<String> {
        let output = db.query(sql).expect("查询可执行");
        let rows = parse_rows(&output).expect("查询返回 JSON 数组");
        identifier(rows.first().and_then(|row| row.get("id")))
    }

    /// 把三条查询原样打到本机真实的 opencode 库上：核对列别名全集，并确认每条
    /// `json_extract` 路径在真实数据上仍能解析（路径写错不会报错，只会全为 null，
    /// 喂夹具的单测看不见——这正是 `$.modelID` 曾经漏网的原因）。
    ///
    /// 只读；只看键名与非空计数，不取回也不打印任何正文。升级 opencode 后手动跑：
    /// `cargo nextest run --run-ignored only -E 'test(live_schema_probe)'`。
    #[test]
    #[ignore = "需要本机 opencode 与真实会话库；升级 opencode 后手动核对键名漂移"]
    fn live_schema_probe_keeps_aliases_and_json_paths() {
        let home = PathBuf::from(std::env::var_os("HOME").expect("HOME 已设置"));
        assert!(
            database_exists(&home, xdg_data_home()),
            "本机没有 opencode 库：装好 opencode 并跑过一次会话再来"
        );
        let db = OfficialCli;

        // 树：挑一个带 model 且有消息的会话当根（子会话最多的优先，覆盖的行更多），
        // 别名与路径一次查清。
        let root = scalar_id(
            &db,
            "SELECT s.id AS id FROM session s WHERE json_valid(s.model) \
             AND EXISTS (SELECT 1 FROM message m WHERE m.session_id = s.id) \
             ORDER BY (SELECT COUNT(*) FROM session c WHERE c.parent_id = s.id) DESC, \
             s.time_updated DESC LIMIT 1",
        )
        .expect("库里要有带 model 与消息的会话");
        let tree = tree_sql(&root, 0);
        assert_eq!(
            alias_set(&db, &tree).expect("根会话至少有自己一行"),
            [
                "agent",
                "cost",
                "depth",
                "id",
                "last_completed",
                "last_error",
                "last_error_message",
                "last_error_message_len",
                "last_error_name",
                "last_role",
                "model_id",
                "parent_id",
                "time_archived",
                "time_created",
                "time_updated",
                "title",
                "title_len",
                "tokens_input",
                "tokens_output",
                "tokens_reasoning",
            ],
            "session 查询的列别名漂移了"
        );
        let counts = non_null_counts(&db, &tree, &["model_id", "last_role", "title"]);
        assert!(
            count_of(&counts, "model_id") > 0,
            "model_id 全为 null：session.model 里的模型名路径漂移了（实测是 $.id）"
        );
        // 逐行：树查询结果里 `session.model` 非空的每一行都要解析出模型名。
        let with_model = non_null_counts(
            &db,
            &format!(
                "SELECT t.model_id FROM ({tree}) t JOIN session s ON s.id = t.id \
                 WHERE s.model IS NOT NULL"
            ),
            &["model_id"],
        );
        assert!(
            count_of(&with_model, "row_total") > 0,
            "选中的根会话带 model，树查询却没有带 model 的行"
        );
        assert_eq!(
            count_of(&with_model, "model_id"),
            count_of(&with_model, "row_total"),
            "树查询里有 model 非空、model_id 却为 null 的行：模型名路径漂移了"
        );
        // 全库：同一表达式对所有 model 非空的会话都能取出模型名（异构行也算漂移）。
        let whole_db = non_null_counts(
            &db,
            &format!(
                "SELECT {} AS model_id FROM session WHERE model IS NOT NULL",
                model_id_sql("model")
            ),
            &["model_id"],
        );
        assert_eq!(
            count_of(&whole_db, "model_id"),
            count_of(&whole_db, "row_total"),
            "库里有 model 非空却取不出模型名的会话：session.model 的键名漂移了"
        );
        assert!(
            count_of(&counts, "last_role") > 0,
            "last_role 全为 null：message.data 的 $.role 路径漂移了"
        );
        // 全库：带错误对象的消息都要取得出错误名（摘要里的失败原因靠它）。库里一条
        // 带错误的消息都没有时两边都是 0，只证明查询能跑。
        let errors = non_null_counts(
            &db,
            "SELECT json_extract(data, '$.error.name') AS error_name FROM message \
             WHERE json_valid(data) AND json_type(data, '$.error') = 'object'",
            &["error_name"],
        );
        assert_eq!(
            count_of(&errors, "error_name"),
            count_of(&errors, "row_total"),
            "有错误对象却取不出 $.error.name：message.data 的错误形状漂移了"
        );

        // 待办：优先挑真有待办的会话核对别名；库里一条都没有时，至少确认这条 SQL
        // 能在真实 schema 上跑通（列名写错会直接报错）。
        let todo_owner = scalar_id(
            &db,
            "SELECT session_id AS id FROM todo GROUP BY session_id \
             ORDER BY COUNT(*) DESC LIMIT 1",
        );
        let todo = todo_sql(&format!(
            "'{}'",
            todo_owner.as_deref().unwrap_or(root.as_str())
        ));
        match alias_set(&db, &todo) {
            Some(keys) => assert_eq!(
                keys,
                [
                    "content",
                    "content_len",
                    "position",
                    "priority",
                    "session_id",
                    "status",
                    "time_created",
                    "time_updated",
                ],
                "todo 查询的列别名漂移了"
            ),
            None => assert!(
                todo_owner.is_none(),
                "选中的会话有待办却查不到行：todo 查询的过滤条件漂移了"
            ),
        }

        // 部件：挑部件最多的会话，别名看一行、路径看整段。
        let chatty = scalar_id(
            &db,
            "SELECT session_id AS id FROM part WHERE json_valid(data) \
             GROUP BY session_id ORDER BY COUNT(*) DESC LIMIT 1",
        )
        .expect("库里要有部件");
        assert_eq!(
            alias_set(&db, &parts_sql(&chatty, 0, MAX_PARTS_PER_PAGE)).expect("该会话有部件"),
            [
                "filename",
                "id",
                "message_completed",
                "part_end",
                "role",
                "text",
                "text_len",
                "tool",
                "tool_output",
                "tool_output_len",
                "tool_status",
                "tool_title",
                "type",
            ],
            "part 查询的列别名漂移了"
        );
        let columns = [
            "type",
            "role",
            "message_completed",
            "tool",
            "tool_status",
            "tool_title",
            "tool_output",
            "part_end",
            "text",
        ];
        let counts = non_null_counts(&db, &parts_sql(&chatty, 0, PROBE_ROW_LIMIT), &columns);
        assert_eq!(
            count_of(&counts, "type"),
            count_of(&counts, "row_total"),
            "有部件的 $.type 解析不出来：类型路径漂移了"
        );
        for column in columns {
            assert!(
                count_of(&counts, column) > 0,
                "{column} 在整段会话里全为 null：对应的 json 路径漂移了"
            );
        }
    }
}
