//! ZCode（智谱 Z.ai 桌面应用，开源仓库 `zai-org/ZCode`）的本地用量来源：只读聚合本机
//! `~/.zcode/cli/db/db.sqlite` 的 `model_usage` 表，按主任务 / 子 agent 拆分最近 24 h 的
//! token，零凭据。
//!
//! # 边界
//!
//! - 只有本地统计：ZCode 的远端额度端点要用它自己存的 OAuth/JWT 鉴权，本来源**不查询**
//!   远端额度，不读任何凭据文件，也没有相应的配置键。
//! - 绝不执行 `zcode` 命令（`--version` / `--help` 都会拉起 Electron 窗口）：来源登记的
//!   `command` 是实际起的 `sqlite3`。
//! - 安装判定 = 库文件存在（`installed`）；库不存在时账号不出现。HOME 由调用方注入
//!   （生产取 `home_dir`，测试传临时目录），不读真实 `~/.zcode`。
//! - 只读聚合行数与合计，不读 `error_message`、`raw_usage_json` 等可能带正文的列。
//!
//! # 读取方式
//!
//! 与 `server::agent_activity::zcode` 同一口径：系统 `sqlite3 -readonly -batch`，`-init`
//! 指向空设备以屏蔽 `~/.sqliterc`，`.timeout 2000` 等锁，行由 SQL 的 `json_object()` 拼成
//! JSON（本机 sqlite3 3.31.1 没有 `-json`）；时限取探测时限与 5 s 的较小者。子进程走本目录
//! 的 `transport::capture_raw`（进程组隔离、环境清洗、两路输出各 ≤ 2 MiB），不另起一套
//! 进程管理。一条聚合语句，走 `model_usage_started_model_idx` 只扫窗口内的行；聚合不带
//! GROUP BY 恒出一行，行首的 `'t'` 标签就是哨兵——表或列缺失时 sqlite3 只往 stderr 报错、
//! 不出这一行。
//!
//! # 本机取证（2026-09-23，只读核对表名 / 列名 / 枚举取值与聚合数值，未读任何正文）
//!
//! ZCode 3.14.3（CLI 引擎 0.16.9），迁移停在 `0022_backfilled_session_reasoning`。
//! `model_usage` 每次模型请求一行（重试各占一行），相关列：`session_id`、
//! `query_source`（实测 `main_turn` / `subagent` / `session_title` / `compact`）、`agent`
//! （`zcode-agent` / `zcode-Explore` / `zcode-general-purpose` / `zcode-vision` /
//! `zcode-flash`）、`task_type`（`interactive` / `subagent_child`，与 `query_source` 的
//! subagent 一一对应）、`status`（`running` / `completed` / `error` / `cancelled`）、
//! `started_at`（毫秒）、`tool_call_count`、`input_tokens`（含缓存读取）、`output_tokens`、
//! `reasoning_tokens`、`cache_creation_input_tokens`、`cache_read_input_tokens`、
//! `provider_total_tokens`（可空）、`computed_total_tokens`（非空；全表逐行等于
//! `input_tokens + output_tokens`）。`tool_usage` 的行数与 `sum(tool_call_count)` 相等，
//! 所以工具调用次数只读 `model_usage` 一张表。24 h 窗口的聚合走索引，实测毫秒级。
//!
//! # 口径
//!
//! - 子 agent 行：`query_source = 'subagent'` 或 `task_type = 'subagent_child'`；其余
//!   （`main_turn` / `session_title` / `compact` 与未来的新来源）都算主任务，主任务 + 子
//!   agent 恒等于合计。
//! - 每行 token 取 `computed_total_tokens`，为空时退回 `input_tokens + output_tokens`。
//! - 子 agent 数 = 窗口内有子 agent 行的不同 `session_id` 数。

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

use super::metric_texts;
use super::registry::Provider;
use super::transport::{self, QueryError};
use crate::api::schema::{ObservationStatus, UsageMetric};
use crate::config::UsageAccountConfig;

// 快照的来源（`UsageProbeTexts::source_zcode_local`）与 Ready 快照的说明（本地统计，非账号
// 额度，远端额度不查询：`UsageNoticeTexts::zcode_local`）都按 server 语言取。

const WINDOW_HOURS: u64 = 24;
const WINDOW_MS: u64 = WINDOW_HOURS * 60 * 60 * 1000;
/// 统计行的哨兵标签。
const ROW_TAG: &str = "zcode_usage";
/// 交给 `-init` 的空文件：不读用户的 `~/.sqliterc`（它可能改掉输出模式）。
const SQLITE_EMPTY_INIT: &str = if cfg!(windows) { "NUL" } else { "/dev/null" };
/// 整个 sqlite3 子进程的时限上限（与活动树适配器同口径）。
const SQLITE_TIMEOUT: Duration = Duration::from_secs(5);
/// 遇到写锁时 sqlite3 自己等待的上限（WAL 下读者一般不会被挡）。
const SQLITE_BUSY_TIMEOUT_MS: u32 = 2_000;
/// 进 debug 日志的 stderr 尾部上限。
const STDERR_LOG_BYTES: usize = 512;

/// 子 agent 行的判据与每行 token 的取值（见模块文档「口径」）。
const SUBAGENT_ROW: &str = "(query_source = 'subagent' or task_type = 'subagent_child')";
const ROW_TOKENS: &str =
    "coalesce(computed_total_tokens, coalesce(input_tokens, 0) + coalesce(output_tokens, 0))";

/// 快照 `message` 里的固定说明（文档终审 D7）：按 server 的界面语言取；sqlite3 的报错原文
/// 只进 debug 日志，不进文案。
fn texts() -> &'static crate::i18n::UsageProbeTexts {
    super::probe_texts()
}

/// 读取用的 home 目录：先 `HOME` 再 `USERPROFILE`，不依赖平台 cfg。与
/// `server::agent_activity::home_dir` 同一口径；那边是私有函数，这里复制最小实现，不为一行
/// 改动去动另一条车道的可见性。
pub(super) fn home_dir() -> Option<PathBuf> {
    ["HOME", "USERPROFILE"]
        .into_iter()
        .filter_map(std::env::var_os)
        .find(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// `<home>/.zcode/cli/db/db.sqlite`。
pub(super) fn database_path(home: &Path) -> PathBuf {
    home.join(".zcode").join("cli").join("db").join("db.sqlite")
}

/// 安装判定：注入的 home 下有 ZCode 的数据库文件。没有 home 视为未安装。
pub(super) fn installed(home: Option<&Path>) -> bool {
    home.is_some_and(|home| database_path(home).is_file())
}

/// 最近 24 h 的聚合查询：只读、单条语句、不取任何文本列。`now_ms` 由调用方给出，窗口下界
/// 以整数字面量拼进 SQL（`u64`，无注入面）。
pub(super) fn usage_sql(now_ms: u64) -> String {
    let since = now_ms.saturating_sub(WINDOW_MS);
    format!(
        "select json_object('t', '{ROW_TAG}', \
         'main_tokens', coalesce(sum(case when {SUBAGENT_ROW} then 0 else {ROW_TOKENS} end), 0), \
         'subagent_tokens', coalesce(sum(case when {SUBAGENT_ROW} then {ROW_TOKENS} else 0 end), 0), \
         'tool_uses', coalesce(sum(tool_call_count), 0), \
         'subagents', count(distinct case when {SUBAGENT_ROW} then session_id end)) \
         from model_usage where started_at >= {since}"
    )
}

/// 一次聚合的结果；字段缺失、不是有限非负数时为 `None`，只丢对应的指标。
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub(super) struct Totals {
    pub main_tokens: Option<f64>,
    pub subagent_tokens: Option<f64>,
    pub tool_uses: Option<f64>,
    pub subagents: Option<f64>,
}

/// 从 sqlite3 的输出里找带哨兵标签的统计行；非 JSON 行（旧版 sqlite3 的报错回显等）跳过。
/// 没有这一行 ⇒ `None`（查询没跑通）。
pub(super) fn parse_output(stdout: &str) -> Option<Totals> {
    let row = stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
        .find(|value| value.get("t").and_then(Value::as_str) == Some(ROW_TAG))?;
    let count = |key: &str| {
        row.get(key)
            .and_then(Value::as_f64)
            .filter(|value| value.is_finite() && *value >= 0.0)
    };
    Some(Totals {
        main_tokens: count("main_tokens"),
        subagent_tokens: count("subagent_tokens"),
        tool_uses: count("tool_uses"),
        subagents: count("subagents"),
    })
}

/// 聚合结果 → 指标（id 约定供客户端卡片匹配）。全部 `scope = local`：本地统计，不是账号
/// 额度；统计窗口单独一条 `session/window_hours`，只有它带 `window_seconds`（详情栏的
/// 「窗口」列不重复）。一条用量都没有时连窗口也不给。
pub(super) fn metrics(totals: &Totals) -> Vec<UsageMetric> {
    let local = |id: &str, label: &str, unit: &str| UsageMetric {
        id: id.into(),
        label: label.into(),
        unit: unit.into(),
        scope: "local".into(),
        ..Default::default()
    };
    let total = totals
        .main_tokens
        .zip(totals.subagent_tokens)
        .map(|(main, subagents)| main + subagents);
    let labels = metric_texts();
    let mut metrics = Vec::new();
    for (value, id, label, unit) in [
        (
            totals.main_tokens,
            "session/tokens/main",
            labels.tokens_main,
            "tokens",
        ),
        (
            totals.subagent_tokens,
            "session/tokens/subagents",
            labels.tokens_subagents,
            "tokens",
        ),
        (total, "session/tokens/total", labels.tokens_total, "tokens"),
        (
            totals.tool_uses,
            "session/tool_uses",
            labels.tool_uses,
            "count",
        ),
        (
            totals.subagents,
            "session/subagents",
            labels.subagents,
            "count",
        ),
    ] {
        if let Some(value) = value {
            metrics.push(UsageMetric {
                used: Some(value),
                ..local(id, label, unit)
            });
        }
    }
    if !metrics.is_empty() {
        metrics.push(UsageMetric {
            used: Some(WINDOW_HOURS as f64),
            text_value: Some(format!("{WINDOW_HOURS}h")),
            window_seconds: Some(WINDOW_HOURS * 60 * 60),
            ..local("session/window_hours", labels.stats_window, "hours")
        });
    }
    metrics
}

/// 一次探测：定位注入 home 下的库 → 起 sqlite3 只读聚合 → 指标。错误语义：
/// - 配了 `profile_dir` → `Unsupported`（没有经过验证的独立数据目录选择方式）；
/// - 没有 home / 库文件 / 路径不是 UTF-8、sqlite3 不在 PATH → `Unavailable`；
/// - 库在但查询失败（超时、锁、坏文件、缺表缺列、没有统计行）→ `Error`：沿用上次样本
///   标为缓存，并按失败次数指数退避。
pub(super) fn probe(
    provider: &Provider,
    account: &UsageAccountConfig,
    home: Option<&Path>,
    timeout: Duration,
    now_ms: u64,
) -> Result<Vec<UsageMetric>, QueryError> {
    if account.profile_dir.is_some() {
        return Err((
            ObservationStatus::Unsupported,
            texts().zcode_profile_unsupported.into(),
        ));
    }
    let home = home.ok_or_else(|| {
        (
            ObservationStatus::Unavailable,
            texts().zcode_no_home.to_owned(),
        )
    })?;
    let database = database_path(home);
    if !database.is_file() {
        return Err((
            ObservationStatus::Unavailable,
            texts().zcode_no_database.into(),
        ));
    }
    let database = database.to_str().ok_or_else(|| {
        (
            ObservationStatus::Unavailable,
            texts().zcode_non_utf8_path.to_owned(),
        )
    })?;
    let busy = format!(".timeout {SQLITE_BUSY_TIMEOUT_MS}");
    let sql = usage_sql(now_ms);
    let args = [
        "-init",
        SQLITE_EMPTY_INIT,
        "-readonly",
        "-batch",
        "-list",
        "-noheader",
        "-cmd",
        &busy,
        database,
        &sql,
    ];
    let captured = transport::capture_raw(provider, account, &args, timeout.min(SQLITE_TIMEOUT))
        .map_err(|(status, message)| {
            tracing::debug!(
                event = "account.probe.zcode_local",
                subsystem = "account_usage",
                outcome = "error",
                %message,
                "zcode 本地库查询未完成"
            );
            if status == ObservationStatus::Unavailable {
                (
                    ObservationStatus::Unavailable,
                    texts().zcode_sqlite_missing.to_owned(),
                )
            } else {
                (
                    ObservationStatus::Error,
                    texts().zcode_query_failed.to_owned(),
                )
            }
        })?;
    match parse_output(&String::from_utf8_lossy(&captured.stdout)) {
        Some(totals) => {
            let metrics = metrics(&totals);
            if metrics.is_empty() {
                Err((ObservationStatus::Error, texts().zcode_unparsable.into()))
            } else {
                Ok(metrics)
            }
        }
        None => Err(classify_failure(&captured.stderr)),
    }
}

/// 没有统计行时按 sqlite3 的报错归类；报错原文可能带路径，只以脱敏尾部进 debug 日志，
/// 文案一律用固定说明。
fn classify_failure(stderr: &[u8]) -> QueryError {
    let tail = &stderr[stderr.len().saturating_sub(STDERR_LOG_BYTES)..];
    let text = String::from_utf8_lossy(tail).to_ascii_lowercase();
    let message = if text.contains("no such table") || text.contains("no such column") {
        texts().zcode_schema_mismatch
    } else if text.contains("database is locked") || text.contains("database is busy") {
        texts().zcode_database_busy
    } else if text.contains("not a database")
        || text.contains("malformed")
        || text.contains("unable to open")
    {
        texts().zcode_database_unreadable
    } else {
        texts().zcode_no_result
    };
    tracing::debug!(
        event = "account.probe.zcode_local",
        subsystem = "account_usage",
        outcome = "error",
        stderr_tail = %transport::sanitize(&String::from_utf8_lossy(tail)),
        "zcode 本地库查询没有返回统计行"
    );
    (ObservationStatus::Error, message.into())
}

#[cfg(test)]
mod tests {
    //! 夹具在 `tests/fixtures/usage/zcode/`：`schema.sql` 逐字取自本机 schema（只有表结构），
    //! `rows.sql` 是手写的脱敏行（全部编造，预期值写在文件头）。端到端用例用真实 sqlite3
    //! 建库；HOME 一律是临时目录，不读真实 `~/.zcode`。

    use std::io::Write;
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    /// 夹具时间基准：2026-09-21T14:13:20Z。
    const NOW: u64 = 1_790_000_000_000;

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/usage/zcode")
                .join(name),
        )
        .expect("读取夹具")
    }

    /// 测试用临时 home，析构时删除。
    struct TempHome(PathBuf);

    impl TempHome {
        fn new(tag: &str) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "herdr-zcode-usage-{tag}-{}-{}",
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

        /// 在 home 下按 ZCode 的布局建库，灌入 `sql`。
        fn build_db(&self, sql: &str) -> PathBuf {
            let db = database_path(self.path());
            std::fs::create_dir_all(db.parent().expect("库目录")).expect("建库目录");
            let mut child = Command::new("sqlite3")
                .arg(&db)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .expect("启动 sqlite3");
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
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// 端到端用例要真实的 sqlite3。Linux / macOS 构建机都自带；只有 Windows 构建机允许
    /// 缺省跳过，其余平台缺它直接失败，免得整族静默跳过后报绿。
    fn sqlite3_available() -> bool {
        let available = Command::new("sqlite3")
            .arg("-version")
            .output()
            .is_ok_and(|output| output.status.success());
        if !available {
            if !cfg!(windows) {
                panic!("sqlite3 不在 PATH 上：zcode 本地用量端到端用例需要它");
            }
            eprintln!("跳过：Windows 构建机没有 sqlite3");
        }
        available
    }

    fn zcode() -> &'static Provider {
        super::super::registry::provider("zcode").expect("zcode 已登记")
    }

    /// 探测用的程序换成 `program`（缺失 / 卡死的假 sqlite3）；`Provider.command` 是
    /// `&'static str`，测试里泄漏一次路径串。
    fn provider_running(program: &str) -> Provider {
        let base = zcode();
        Provider {
            agent: base.agent,
            label: base.label,
            command: Box::leak(program.to_owned().into_boxed_str()),
            source: base.source,
            method: base.method,
            scope: base.scope,
            query: base.query,
        }
    }

    fn account() -> UsageAccountConfig {
        UsageAccountConfig {
            id: "zcode:default".into(),
            label: "ZCode".into(),
            agent: "zcode".into(),
            provider: "zcode".into(),
            auth_mode: "local".into(),
            ..Default::default()
        }
    }

    fn used(metrics: &[UsageMetric], id: &str) -> Option<f64> {
        metrics
            .iter()
            .find(|metric| metric.id == id)
            .and_then(|metric| metric.used)
    }

    // --- 查询与解析 -----------------------------------------------------------

    #[test]
    fn usage_sql_is_a_read_only_aggregate_over_the_last_24_hours() {
        let sql = usage_sql(NOW);
        assert!(sql.starts_with("select json_object("));
        assert!(
            sql.ends_with(&format!("where started_at >= {}", NOW - 86_400_000)),
            "{sql}"
        );
        let upper = sql.to_ascii_uppercase();
        for forbidden in [
            "INSERT",
            "UPDATE",
            "DELETE",
            "DROP",
            "ALTER",
            "PRAGMA",
            "ATTACH",
            "CREATE",
            ";",
            // 可能带正文的列一律不读。
            "ERROR_MESSAGE",
            "RAW_USAGE_JSON",
            "PROVIDER_METADATA_JSON",
            "TITLE",
            "CONTENT",
        ] {
            assert!(!upper.contains(forbidden), "查询不得包含 {forbidden}");
        }
        assert_eq!(upper.matches("FROM MODEL_USAGE").count(), 1, "只读一张表");
        assert!(
            usage_sql(1_000).ends_with("where started_at >= 0"),
            "时钟异常时下界饱和到 0"
        );
    }

    #[test]
    fn query_output_parsing_needs_the_tagged_row_and_skips_noise() {
        let totals = parse_output(
            "Error: stray diagnostics echoed by an old sqlite3\n\
             {\"t\":\"other\",\"main_tokens\":1}\n\
             not json at all\n\
             {\"t\":\"zcode_usage\",\"main_tokens\":41027.0,\"subagent_tokens\":24200,\"tool_uses\":18.0,\"subagents\":3}\n",
        )
        .expect("找到统计行");
        assert_eq!(
            totals,
            Totals {
                main_tokens: Some(41_027.0),
                subagent_tokens: Some(24_200.0),
                tool_uses: Some(18.0),
                subagents: Some(3.0),
            }
        );
        assert_eq!(parse_output(""), None, "空输出：查询没跑通");
        assert_eq!(
            parse_output("Error: no such table: model_usage\n{\"t\":\"other\"}\n"),
            None,
            "没有哨兵行就不是结果"
        );
    }

    /// 坏值（负数、字符串、null、缺字段）只丢对应的指标，合计缺一边就不给。
    #[test]
    fn bad_values_drop_only_their_own_metric() {
        let totals = parse_output(
            "{\"t\":\"zcode_usage\",\"main_tokens\":-5,\"subagent_tokens\":\"12\",\"tool_uses\":null,\"subagents\":2}",
        )
        .expect("找到统计行");
        assert_eq!(
            totals,
            Totals {
                subagents: Some(2.0),
                ..Default::default()
            }
        );
        let kept = metrics(&totals);
        assert_eq!(
            kept.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["session/subagents", "session/window_hours"]
        );
        assert!(
            metrics(&Totals::default()).is_empty(),
            "一条用量都没有时连统计窗口也不给"
        );
    }

    #[test]
    fn metrics_split_main_and_subagent_tokens_into_local_statistics() {
        let metrics = metrics(&Totals {
            main_tokens: Some(41_027.0),
            subagent_tokens: Some(24_200.0),
            tool_uses: Some(18.0),
            subagents: Some(3.0),
        });
        assert_eq!(
            metrics
                .iter()
                .map(|m| (m.id.as_str(), m.unit.as_str(), m.used))
                .collect::<Vec<_>>(),
            vec![
                ("session/tokens/main", "tokens", Some(41_027.0)),
                ("session/tokens/subagents", "tokens", Some(24_200.0)),
                ("session/tokens/total", "tokens", Some(65_227.0)),
                ("session/tool_uses", "count", Some(18.0)),
                ("session/subagents", "count", Some(3.0)),
                ("session/window_hours", "hours", Some(24.0)),
            ]
        );
        assert!(
            metrics.iter().all(|metric| metric.scope == "local"),
            "本地统计，不是账号额度"
        );
        assert!(
            metrics.iter().all(|metric| metric.used_percent.is_none()
                && metric.limit.is_none()
                && metric.resets_at.is_none()),
            "没有额度语义：不给百分比、上限与重置时间"
        );
        let window = metrics.last().expect("统计窗口");
        assert_eq!(window.text_value.as_deref(), Some("24h"));
        assert_eq!(window.window_seconds, Some(86_400));
        assert_eq!(
            metrics
                .iter()
                .filter(|metric| metric.window_seconds.is_some())
                .count(),
            1,
            "只有窗口指标带 window_seconds"
        );
        assert!(super::super::parse::validate(&metrics), "{metrics:#?}");
    }

    // --- 安装判定与 HOME 隔离 --------------------------------------------------

    #[test]
    fn installation_follows_the_database_file_under_the_injected_home() {
        let home = TempHome::new("installed");
        assert!(!installed(None), "没有 home 视为未安装");
        assert!(!installed(Some(home.path())));
        let db = database_path(home.path());
        assert_eq!(
            db,
            home.path()
                .join(".zcode")
                .join("cli")
                .join("db")
                .join("db.sqlite")
        );
        std::fs::create_dir_all(&db).expect("同名目录");
        assert!(!installed(Some(home.path())), "同名目录不算库文件");
        std::fs::remove_dir(&db).expect("删目录");
        std::fs::write(&db, b"").expect("写库文件");
        assert!(installed(Some(home.path())));
    }

    /// 探测只看注入的 home：没有库就是 Unavailable，不会退到真实 `~/.zcode`；配了
    /// `profile_dir` 是 Unsupported。这些分支都不起任何进程（程序名指向不存在的命令）。
    #[test]
    fn probe_stays_inside_the_injected_home() {
        let home = TempHome::new("no-db");
        let missing = provider_running("herdr-test-no-such-sqlite3");
        let timeout = Duration::from_secs(5);
        assert_eq!(
            probe(&missing, &account(), Some(home.path()), timeout, NOW),
            Err((
                ObservationStatus::Unavailable,
                texts().zcode_no_database.into()
            ))
        );
        assert_eq!(
            probe(&missing, &account(), None, timeout, NOW),
            Err((ObservationStatus::Unavailable, texts().zcode_no_home.into()))
        );
        let with_profile = UsageAccountConfig {
            profile_dir: Some(home.path().to_path_buf()),
            ..account()
        };
        assert_eq!(
            probe(&missing, &with_profile, Some(home.path()), timeout, NOW),
            Err((
                ObservationStatus::Unsupported,
                texts().zcode_profile_unsupported.into()
            ))
        );
    }

    #[test]
    fn a_missing_sqlite3_is_unavailable_not_an_error() {
        let home = TempHome::new("no-sqlite");
        let db = database_path(home.path());
        std::fs::create_dir_all(db.parent().expect("库目录")).expect("建库目录");
        std::fs::write(&db, b"not really a database").expect("写假库");
        assert_eq!(
            probe(
                &provider_running("herdr-test-no-such-sqlite3"),
                &account(),
                Some(home.path()),
                Duration::from_secs(5),
                NOW,
            ),
            Err((
                ObservationStatus::Unavailable,
                texts().zcode_sqlite_missing.into()
            ))
        );
    }

    /// 卡住的 sqlite3 到点就被杀掉，按查询失败回报，不拖住查询线程。
    #[cfg(unix)]
    #[test]
    fn a_hung_sqlite3_is_killed_at_the_deadline() {
        use std::os::unix::fs::PermissionsExt;
        // 只有这个 unix 用例计时；放模块级会在 Windows 测试目标上成为未使用导入。
        use std::time::Instant;

        let home = TempHome::new("hung");
        let db = database_path(home.path());
        std::fs::create_dir_all(db.parent().expect("库目录")).expect("建库目录");
        std::fs::write(&db, b"").expect("写库文件");
        let program = home.path().join("fake-sqlite3");
        // exec：不留孙进程攥着管道。
        std::fs::write(&program, "#!/bin/sh\nexec sleep 30\n").expect("写假 sqlite3");
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))
            .expect("设可执行位");
        let started = Instant::now();
        let result = probe(
            &provider_running(program.to_str().expect("临时路径是 UTF-8")),
            &account(),
            Some(home.path()),
            Duration::from_millis(200),
            NOW,
        );
        assert_eq!(
            result,
            Err((ObservationStatus::Error, texts().zcode_query_failed.into()))
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "超时后必须立刻收尾：{:?}",
            started.elapsed()
        );
    }

    // --- 端到端（真实 sqlite3） ------------------------------------------------

    #[test]
    fn the_fixture_database_yields_the_expected_totals() {
        if !sqlite3_available() {
            return;
        }
        let home = TempHome::new("fixture");
        home.build_db(&format!(
            "{}\n{}",
            fixture("schema.sql"),
            fixture("rows.sql")
        ));
        let metrics = probe(
            zcode(),
            &account(),
            Some(home.path()),
            Duration::from_secs(20),
            NOW,
        )
        .expect("夹具库可读");
        for (id, expected) in [
            ("session/tokens/main", 41_027.0),
            ("session/tokens/subagents", 24_200.0),
            ("session/tokens/total", 65_227.0),
            ("session/tool_uses", 18.0),
            ("session/subagents", 3.0),
            ("session/window_hours", 24.0),
        ] {
            assert_eq!(used(&metrics, id), Some(expected), "{id}");
        }
        // 窗口跟着 now 走：晚 23 h 再查，窗口下界落在 NOW - 1 h，只剩那之后的行
        // （主任务 21500 + 5400 + 1000，子 agent 会话 02 / 03）。
        let later = probe(
            zcode(),
            &account(),
            Some(home.path()),
            Duration::from_secs(20),
            NOW + 23 * 60 * 60 * 1000,
        )
        .expect("夹具库可读");
        assert_eq!(used(&later, "session/tokens/main"), Some(27_900.0));
        assert_eq!(used(&later, "session/tokens/subagents"), Some(5_900.0));
        assert_eq!(used(&later, "session/tool_uses"), Some(6.0));
        assert_eq!(used(&later, "session/subagents"), Some(2.0));
    }

    #[test]
    fn an_empty_database_reports_zero_usage() {
        if !sqlite3_available() {
            return;
        }
        let home = TempHome::new("empty");
        home.build_db(&fixture("schema.sql"));
        let metrics = probe(
            zcode(),
            &account(),
            Some(home.path()),
            Duration::from_secs(20),
            NOW,
        )
        .expect("空库可读");
        assert_eq!(metrics.len(), 6);
        assert!(metrics
            .iter()
            .filter(|metric| metric.id != "session/window_hours")
            .all(|metric| metric.used == Some(0.0)));
    }

    #[test]
    fn a_missing_table_or_column_or_a_corrupt_file_is_a_query_failure() {
        if !sqlite3_available() {
            return;
        }
        let run = |home: &TempHome| {
            probe(
                zcode(),
                &account(),
                Some(home.path()),
                Duration::from_secs(20),
                NOW,
            )
        };
        let no_table = TempHome::new("no-table");
        no_table.build_db("CREATE TABLE session (id text primary key);");
        assert_eq!(
            run(&no_table),
            Err((
                ObservationStatus::Error,
                texts().zcode_schema_mismatch.into()
            ))
        );

        let no_column = TempHome::new("no-column");
        no_column.build_db(
            "CREATE TABLE model_usage (id text primary key, session_id text not null, \
             query_source text not null, task_type text, started_at integer not null, \
             tool_call_count integer not null default 0);",
        );
        assert_eq!(
            run(&no_column),
            Err((
                ObservationStatus::Error,
                texts().zcode_schema_mismatch.into()
            ))
        );

        let corrupt = TempHome::new("corrupt");
        let db = database_path(corrupt.path());
        std::fs::create_dir_all(db.parent().expect("库目录")).expect("建库目录");
        std::fs::write(&db, [0x5a_u8; 8192]).expect("写坏文件");
        assert_eq!(
            run(&corrupt),
            Err((
                ObservationStatus::Error,
                texts().zcode_database_unreadable.into()
            ))
        );
    }

    #[test]
    fn sqlite_errors_map_to_fixed_messages_without_echoing_stderr() {
        for (stderr, expected) in [
            ("Error: no such table: model_usage", texts().zcode_schema_mismatch),
            ("Error: no such column: computed_total_tokens", texts().zcode_schema_mismatch),
            ("Error: database is locked", texts().zcode_database_busy),
            ("Error: file is not a database", texts().zcode_database_unreadable),
            (
                "Error: unable to open database \"/home/someone/.zcode/cli/db/db.sqlite\": unable to open database file",
                texts().zcode_database_unreadable,
            ),
            ("", texts().zcode_no_result),
        ] {
            assert_eq!(
                classify_failure(stderr.as_bytes()),
                (ObservationStatus::Error, expected.to_owned()),
                "{stderr}"
            );
        }
    }

    /// 文档终审 D7：本地库说明与指标名按 server 的界面语言给出——英文界面不含 CJK，中文
    /// 界面是中文；sqlite3 的报错原文不进文案。
    #[test]
    fn local_database_notes_follow_the_interface_language() {
        use crate::i18n::{has_cjk, lang_guard, Lang};
        let totals = Totals {
            main_tokens: Some(10.0),
            subagent_tokens: Some(5.0),
            tool_uses: Some(3.0),
            subagents: Some(1.0),
        };
        for (lang, chinese) in [(Lang::En, false), (Lang::ZhCn, true)] {
            let _guard = lang_guard(lang);
            for stderr in [
                "Error: no such table: model_usage",
                "Error: database is locked",
                "Error: file is not a database",
                "",
            ] {
                let (_, message) = classify_failure(stderr.as_bytes());
                assert!(!message.contains("Error:"), "{message}");
                assert_eq!(has_cjk(&message), chinese, "{lang:?}: {message}");
            }
            for metric in metrics(&totals) {
                assert_eq!(
                    has_cjk(&metric.label),
                    chinese,
                    "{lang:?}: {}",
                    metric.label
                );
            }
        }
    }
}
