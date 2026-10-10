//! 活动节点状态的统一归一表：各家来源的原始状态取值 → 现有的节点状态
//! [`AgentActivityStatus`]。不新增枚举变体，语义如下：
//!
//! - 等待（`pending` / `queued`）→ `Pending`；运行（`running` / `in_progress`）→
//!   `Running`；运行中的工作停下等用户（`blocked`）→ `Blocked`（计划条目没有这一状态）。
//! - 完成（`completed` / `done` / `success`）→ `Done`。
//! - 出错（`error` / `failed`）、被杀（`killed`）、被停（`stopped`）、中止（`aborted` /
//!   `interrupted`）、超时（`timed_out`）→ `Failed`：工作没有做完。
//! - 取消看语境（[`StatusScope`]）：计划条目（待办、计划步骤）被撤下是计划内的调整，
//!   → `Done`；运行中的工作（子 agent、任务、后台进程、回合）被取消等同中止，→ `Failed`。
//! - `lost` → `Unknown`；认不出的取值不在表里（[`normalize`] 回 `None`），由调用方决定
//!   落 `Unknown` 还是另作推断。
//!
//! 归一后丢掉的信息（被杀、超时、取消……）放在 [`Normalized::note`]，适配器把它写进
//! 节点摘要。

use crate::api::schema::AgentActivityStatus;

/// 原始状态出现的语境。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusScope {
    /// 计划条目：待办、计划步骤。被取消算计划内的结束。
    Plan,
    /// 运行中的工作：子 agent、任务、后台进程、回合。被取消算中止。
    Run,
}

/// 一个原始状态的归一结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Normalized {
    pub status: AgentActivityStatus,
    /// 归一时丢掉的原因（`killed`、`timed out` 等），写进节点摘要。
    pub note: Option<&'static str>,
}

struct Rule {
    raw: &'static [&'static str],
    /// `None` = 两种语境都适用。
    scope: Option<StatusScope>,
    status: AgentActivityStatus,
    note: Option<&'static str>,
}

const fn rule(
    raw: &'static [&'static str],
    scope: Option<StatusScope>,
    status: AgentActivityStatus,
    note: Option<&'static str>,
) -> Rule {
    Rule {
        raw,
        scope,
        status,
        note,
    }
}

/// 归一表。原始取值先规范成小写、`-` 与空格换成 `_`（见 [`canonical`]）。
const RULES: &[Rule] = &[
    rule(
        &["pending", "queued", "waiting"],
        None,
        AgentActivityStatus::Pending,
        None,
    ),
    rule(
        &["running", "in_progress", "started", "active"],
        None,
        AgentActivityStatus::Running,
        None,
    ),
    // 「停下等用户」只说运行中的工作；计划条目没有这一状态，写了也不猜。
    rule(
        &["blocked"],
        Some(StatusScope::Run),
        AgentActivityStatus::Blocked,
        None,
    ),
    rule(
        &[
            "completed",
            "complete",
            "done",
            "success",
            "succeeded",
            "finished",
        ],
        None,
        AgentActivityStatus::Done,
        None,
    ),
    rule(
        &["cancelled", "canceled", "user_cancelled"],
        Some(StatusScope::Plan),
        AgentActivityStatus::Done,
        Some("cancelled"),
    ),
    rule(
        &["cancelled", "canceled", "user_cancelled"],
        Some(StatusScope::Run),
        AgentActivityStatus::Failed,
        Some("cancelled"),
    ),
    rule(
        &["failed", "failure", "error", "errored"],
        None,
        AgentActivityStatus::Failed,
        None,
    ),
    rule(
        &["killed"],
        None,
        AgentActivityStatus::Failed,
        Some("killed"),
    ),
    rule(
        &["stopped"],
        None,
        AgentActivityStatus::Failed,
        Some("stopped"),
    ),
    rule(
        &["aborted"],
        None,
        AgentActivityStatus::Failed,
        Some("aborted"),
    ),
    rule(
        &["interrupted"],
        None,
        AgentActivityStatus::Failed,
        Some("interrupted"),
    ),
    rule(
        &["timed_out", "timeout"],
        None,
        AgentActivityStatus::Failed,
        Some("timed out"),
    ),
    rule(&["lost"], None, AgentActivityStatus::Unknown, Some("lost")),
];

/// 归一一个原始状态；认不出的取值为 `None`。
pub fn normalize(raw: &str, scope: StatusScope) -> Option<Normalized> {
    let raw = canonical(raw);
    RULES
        .iter()
        .find(|rule| {
            rule.scope.is_none_or(|only| only == scope) && rule.raw.contains(&raw.as_str())
        })
        .map(|rule| Normalized {
            status: rule.status,
            note: rule.note,
        })
}

/// 归一后的状态；认不出的取值落 `Unknown`。
pub fn status_or_unknown(raw: Option<&str>, scope: StatusScope) -> AgentActivityStatus {
    raw.and_then(|raw| normalize(raw, scope))
        .map_or(AgentActivityStatus::Unknown, |normalized| normalized.status)
}

/// 去掉首尾空白、ASCII 小写，`-` 与空格换成 `_`：`In Progress`、`timed-out` 与
/// `in_progress`、`timed_out` 视为同一取值。
fn canonical(raw: &str) -> String {
    raw.trim()
        .chars()
        .map(|ch| match ch {
            '-' | ' ' => '_',
            other => other.to_ascii_lowercase(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use AgentActivityStatus::{Blocked, Done, Failed, Pending, Running, Unknown};
    use StatusScope::{Plan, Run};

    fn to(status: AgentActivityStatus, note: Option<&'static str>) -> Option<Normalized> {
        Some(Normalized { status, note })
    }

    #[test]
    fn the_table_maps_each_raw_status_to_a_node_status() {
        let cases: &[(&str, StatusScope, Option<Normalized>)] = &[
            ("pending", Run, to(Pending, None)),
            ("queued", Run, to(Pending, None)),
            ("running", Run, to(Running, None)),
            ("in_progress", Plan, to(Running, None)),
            ("blocked", Run, to(Blocked, None)),
            ("blocked", Plan, None),
            ("completed", Run, to(Done, None)),
            ("done", Plan, to(Done, None)),
            ("success", Run, to(Done, None)),
            ("failed", Run, to(Failed, None)),
            ("error", Run, to(Failed, None)),
            ("killed", Run, to(Failed, Some("killed"))),
            ("stopped", Run, to(Failed, Some("stopped"))),
            ("aborted", Run, to(Failed, Some("aborted"))),
            ("interrupted", Run, to(Failed, Some("interrupted"))),
            ("timed_out", Run, to(Failed, Some("timed out"))),
            ("lost", Run, to(Unknown, Some("lost"))),
            // 取消看语境：计划内撤下的条目算结束，运行中的工作被取消算中止。
            ("cancelled", Plan, to(Done, Some("cancelled"))),
            ("cancelled", Run, to(Failed, Some("cancelled"))),
            ("user_cancelled", Run, to(Failed, Some("cancelled"))),
            ("canceled", Plan, to(Done, Some("cancelled"))),
            // 写法差异：大小写、连字符、空格、首尾空白。
            (" In Progress ", Plan, to(Running, None)),
            ("TIMED-OUT", Run, to(Failed, Some("timed out"))),
            // 认不出的取值不猜。
            ("paused", Run, None),
            ("unknown", Run, None),
            ("", Plan, None),
        ];
        for (raw, scope, expected) in cases {
            assert_eq!(normalize(raw, *scope), *expected, "{raw:?} in {scope:?}");
        }
    }

    #[test]
    fn unrecognized_or_missing_statuses_fall_back_to_unknown() {
        assert_eq!(status_or_unknown(Some("completed"), Plan), Done);
        assert_eq!(status_or_unknown(Some("paused"), Run), Unknown);
        assert_eq!(status_or_unknown(None, Run), Unknown);
    }
}
