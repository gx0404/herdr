//! 账号卡片的匹配表：`(agent, scope, metric.id)` → 卡片槽位。这是客户端识别厂商
//! 指标的唯一一处，id 约定对齐服务端 `server::observability::accounts::parse`（只读
//! 对照，客户端不复制解析逻辑）。schema 是冻结的通用 `UsageMetric`，厂商差异只编码在
//! id / scope 字符串里；表里没有的指标由卡片按通用行兜底，数据不会被藏掉。

use crate::api::schema::UsageMetric;
use crate::i18n::MonitorTexts;

/// id 的匹配方式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum IdMatch {
    Exact(&'static str),
    /// 以此结尾：codex 的 `{bucket}/primary` 等按桶拼出的 id。
    Suffix(&'static str),
}

impl IdMatch {
    fn matches(self, id: &str) -> bool {
        match self {
            Self::Exact(exact) => id == exact,
            Self::Suffix(suffix) => id.len() > suffix.len() && id.ends_with(suffix),
        }
    }
}

/// 一条指标在厂商卡片里的角色。标签由客户端按界面语言给出（服务端 `label` 按 server 的
/// 语言生成，只在通用兜底行里原样显示）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Slot {
    // ---- 额度窗口（meter：阈值色 + 窗口刻度，可溢出）----
    Quota5h,
    QuotaWeekly,
    QuotaSpend,
    QuotaPrimary,
    QuotaSecondary,
    Quota7d,
    QuotaMonthly,
    QuotaMonthlyCode,
    QuotaPlan,
    // ---- 余额 / 钱包（只画金额）----
    Credits,
    BalanceAvailable,
    BalanceVoucher,
    BalanceCash,
    ExtraBalance,
    ExtraTotal,
    ExtraMonthUsed,
    ExtraMonthCap,
    // ---- 会话 / 本地统计 ----
    Cost,
    Duration,
    ApiDuration,
    ContextPercent,
    ContextTokens,
    ContextWindow,
    Sessions,
    SubagentSessions,
    TokensInput,
    TokensOutput,
    TokensReasoning,
    TokensCacheRead,
    TokensCacheWrite,
    TokensTotal,
    TokensMain,
    TokensSubagents,
    ToolUses,
    Subagents,
    WindowHours,
    Model,
}

/// 槽位的数值形态：决定画 meter 还是数值项，以及数字怎么格式化。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SlotKind {
    /// 账号额度窗口：`used_percent`（或 `used/limit`）画 gauge。
    Quota,
    /// 会话上下文占用：百分比放在 `unit = "%"` 的 `used` 里，也画 gauge。
    Percent,
    /// 金额：`amount_decimal` + 币种。
    Money,
    /// token 数：紧凑写法（`1.2M`）。
    Tokens,
    /// 计数：整数。
    Count,
    /// 服务端给好的文本（时长 / 模型 / 统计窗口）。
    Text,
}

impl Slot {
    pub(super) fn kind(self) -> SlotKind {
        match self {
            Self::Quota5h
            | Self::QuotaWeekly
            | Self::QuotaSpend
            | Self::QuotaPrimary
            | Self::QuotaSecondary
            | Self::Quota7d
            | Self::QuotaMonthly
            | Self::QuotaMonthlyCode
            | Self::QuotaPlan => SlotKind::Quota,
            Self::ContextPercent => SlotKind::Percent,
            Self::Credits
            | Self::BalanceAvailable
            | Self::BalanceVoucher
            | Self::BalanceCash
            | Self::ExtraBalance
            | Self::ExtraTotal
            | Self::ExtraMonthUsed
            | Self::ExtraMonthCap
            | Self::Cost => SlotKind::Money,
            Self::ContextTokens
            | Self::ContextWindow
            | Self::TokensInput
            | Self::TokensOutput
            | Self::TokensReasoning
            | Self::TokensCacheRead
            | Self::TokensCacheWrite
            | Self::TokensTotal
            | Self::TokensMain
            | Self::TokensSubagents => SlotKind::Tokens,
            Self::Sessions | Self::SubagentSessions | Self::ToolUses | Self::Subagents => {
                SlotKind::Count
            }
            Self::Duration | Self::ApiDuration | Self::WindowHours | Self::Model => SlotKind::Text,
        }
    }

    /// 固定长度额度窗口的名义长度（秒）。服务端只在厂商报文带窗口长度时写
    /// `window_seconds`（目前只有 codex 的 `windowDurationMins`）；claude 的
    /// `five_hour` / `seven_day` 与 kimi 的 `limit5h` / `limit7d` 的长度写在 id
    /// 里，报文不带，由这里补上，窗口进度刻度与按节奏预警才画得出来。月度、
    /// 消费额度与套餐额度的周期不固定，不给名义长度。
    pub(super) fn nominal_window_secs(self) -> Option<u64> {
        match self {
            Self::Quota5h => Some(5 * 3600),
            Self::QuotaWeekly | Self::Quota7d => Some(7 * 86_400),
            _ => None,
        }
    }

    /// 界面语言下的短标签。codex 的主 / 次窗口知道窗口长度时由卡片改写成
    /// 「5h 窗口」，这里给的是不知道长度时的兜底。
    pub(super) fn label(self, texts: &MonitorTexts) -> &'static str {
        match self {
            Self::Quota5h => texts.quota_5h,
            Self::QuotaWeekly => texts.quota_weekly,
            Self::QuotaSpend => texts.quota_spend,
            Self::QuotaPrimary => texts.quota_primary,
            Self::QuotaSecondary => texts.quota_secondary,
            Self::Quota7d => texts.quota_7d,
            Self::QuotaMonthly => texts.quota_monthly,
            Self::QuotaMonthlyCode => texts.quota_monthly_code,
            Self::QuotaPlan => texts.quota_plan,
            Self::Credits => texts.credits,
            Self::BalanceAvailable => texts.balance_available,
            Self::BalanceVoucher => texts.balance_voucher,
            Self::BalanceCash => texts.balance_cash,
            Self::ExtraBalance => texts.extra_balance,
            Self::ExtraTotal => texts.extra_total,
            Self::ExtraMonthUsed => texts.extra_month_used,
            Self::ExtraMonthCap => texts.extra_month_cap,
            Self::Cost => texts.cost,
            Self::Duration => texts.duration,
            Self::ApiDuration => texts.api_duration,
            Self::ContextPercent => texts.context,
            Self::ContextTokens => texts.context_tokens,
            Self::ContextWindow => texts.context_window,
            Self::Sessions => texts.sessions,
            Self::SubagentSessions | Self::Subagents => texts.subagents,
            Self::TokensInput => texts.tokens_input,
            Self::TokensOutput => texts.tokens_output,
            Self::TokensReasoning => texts.tokens_reasoning,
            Self::TokensCacheRead => texts.tokens_cache_read,
            Self::TokensCacheWrite => texts.tokens_cache_write,
            Self::TokensTotal => texts.tokens_total,
            Self::TokensMain => texts.tokens_main,
            Self::TokensSubagents => texts.tokens_subagents,
            Self::ToolUses => texts.tool_uses,
            Self::WindowHours => texts.stats_window,
            Self::Model => texts.model,
        }
    }
}

/// 匹配表的一行。`scope` 为 `None` 时不看 scope（zcode 的本地统计由并行车道
/// 产出，计数项的 scope 以它为准，这里放宽）。
pub(super) struct SlotRule {
    pub agent: &'static str,
    pub scope: Option<&'static str>,
    pub id: IdMatch,
    pub slot: Slot,
}

const fn exact(agent: &'static str, scope: &'static str, id: &'static str, slot: Slot) -> SlotRule {
    SlotRule {
        agent,
        scope: Some(scope),
        id: IdMatch::Exact(id),
        slot,
    }
}

const fn suffix(
    agent: &'static str,
    scope: &'static str,
    id: &'static str,
    slot: Slot,
) -> SlotRule {
    SlotRule {
        agent,
        scope: Some(scope),
        id: IdMatch::Suffix(id),
        slot,
    }
}

const fn any_scope(agent: &'static str, id: &'static str, slot: Slot) -> SlotRule {
    SlotRule {
        agent,
        scope: None,
        id: IdMatch::Exact(id),
        slot,
    }
}

/// 匹配表。每段注明对应的 `parse.rs` 生产者；改服务端 id 必须同步这里与
/// `tests::ids_follow_the_server_parsers`。
pub(super) const SLOT_RULES: &[SlotRule] = &[
    // claude —— `parse::CLAUDE_RATE_LIMIT_WINDOWS`（account）与 `claude_session`（session）。
    exact("claude", "account", "five_hour", Slot::Quota5h),
    exact("claude", "account", "seven_day", Slot::QuotaWeekly),
    exact("claude", "account", "spend_limit", Slot::QuotaSpend),
    exact("claude", "session", "cost/total_cost_usd", Slot::Cost),
    exact(
        "claude",
        "session",
        "cost/total_duration_ms",
        Slot::Duration,
    ),
    exact(
        "claude",
        "session",
        "cost/total_api_duration_ms",
        Slot::ApiDuration,
    ),
    exact(
        "claude",
        "session",
        "context_window/used_percentage",
        Slot::ContextPercent,
    ),
    exact(
        "claude",
        "session",
        "context_window/current_usage",
        Slot::ContextTokens,
    ),
    exact(
        "claude",
        "session",
        "context_window/context_window_size",
        Slot::ContextWindow,
    ),
    // codex —— `parse::codex`：每个限额桶 `{bucket}/primary|secondary|credits`。
    suffix("codex", "account", "/primary", Slot::QuotaPrimary),
    suffix("codex", "account", "/secondary", Slot::QuotaSecondary),
    suffix("codex", "account", "/credits", Slot::Credits),
    // kimi —— `parse::kimi`（`quota.usages` 窗口、`summary`、余额、两种额外用量钱包）
    // 与 `parse::moonshot_balance`（API 余额，同一组 id）。
    exact("kimi", "account", "limit5h", Slot::Quota5h),
    exact("kimi", "account", "limit7d", Slot::Quota7d),
    exact("kimi", "account", "monthTotal", Slot::QuotaMonthly),
    exact("kimi", "account", "monthCode", Slot::QuotaMonthlyCode),
    exact("kimi", "account", "summary", Slot::QuotaPlan),
    exact(
        "kimi",
        "account",
        "available_balance",
        Slot::BalanceAvailable,
    ),
    exact("kimi", "account", "voucher_balance", Slot::BalanceVoucher),
    exact("kimi", "account", "cash_balance", Slot::BalanceCash),
    exact("kimi", "account", "extra_usage/balance", Slot::ExtraBalance),
    exact("kimi", "account", "extra_usage/total", Slot::ExtraTotal),
    exact(
        "kimi",
        "account",
        "extra_usage/monthly_used",
        Slot::ExtraMonthUsed,
    ),
    exact(
        "kimi",
        "account",
        "extra_usage/monthly_limit",
        Slot::ExtraMonthCap,
    ),
    exact("kimi", "account", "balance_cents", Slot::ExtraBalance),
    exact(
        "kimi",
        "account",
        "monthly_used_cents",
        Slot::ExtraMonthUsed,
    ),
    exact(
        "kimi",
        "account",
        "monthly_charge_limit_cents",
        Slot::ExtraMonthCap,
    ),
    // opencode —— `parse::opencode_sessions` / `opencode_stats`（scope 固定 local）。
    exact("opencode", "local", "sessions", Slot::Sessions),
    exact(
        "opencode",
        "local",
        "child_sessions",
        Slot::SubagentSessions,
    ),
    exact("opencode", "local", "total_cost", Slot::Cost),
    exact("opencode", "local", "input_tokens", Slot::TokensInput),
    exact("opencode", "local", "output_tokens", Slot::TokensOutput),
    exact(
        "opencode",
        "local",
        "reasoning_tokens",
        Slot::TokensReasoning,
    ),
    exact(
        "opencode",
        "local",
        "cache_read_tokens",
        Slot::TokensCacheRead,
    ),
    exact(
        "opencode",
        "local",
        "cache_write_tokens",
        Slot::TokensCacheWrite,
    ),
    // pi —— `parse::pi`（扩展推送，scope 固定 session）。
    exact("pi", "session", "context/percent", Slot::ContextPercent),
    exact("pi", "session", "context/tokens", Slot::ContextTokens),
    exact(
        "pi",
        "session",
        "context/context_window",
        Slot::ContextWindow,
    ),
    exact("pi", "session", "session/cost_usd", Slot::Cost),
    exact("pi", "session", "session/tokens/input", Slot::TokensInput),
    exact("pi", "session", "session/tokens/output", Slot::TokensOutput),
    exact(
        "pi",
        "session",
        "session/tokens/cache_read",
        Slot::TokensCacheRead,
    ),
    exact(
        "pi",
        "session",
        "session/tokens/cache_write",
        Slot::TokensCacheWrite,
    ),
    exact("pi", "session", "session/tokens/total", Slot::TokensTotal),
    exact("pi", "session", "session/model", Slot::Model),
    // zcode —— 本地用量车道的约定（ZCode 桌面版数据库的本地统计，远端额度不查询）。
    any_scope("zcode", "session/tokens/main", Slot::TokensMain),
    any_scope("zcode", "session/tokens/subagents", Slot::TokensSubagents),
    any_scope("zcode", "session/tokens/total", Slot::TokensTotal),
    any_scope("zcode", "session/tool_uses", Slot::ToolUses),
    any_scope("zcode", "session/subagents", Slot::Subagents),
    any_scope("zcode", "session/window_hours", Slot::WindowHours),
];

/// 指标在 `agent` 的卡片里的槽位；表里没有返回 `None`（通用行兜底）。
pub(super) fn slot_of(agent: &str, metric: &UsageMetric) -> Option<Slot> {
    SLOT_RULES
        .iter()
        .find(|rule| {
            rule.agent == agent
                && rule.scope.is_none_or(|scope| scope == metric.scope)
                && rule.id.matches(&metric.id)
        })
        .map(|rule| rule.slot)
}

/// codex 限额桶 id：`{bucket}/primary` → `bucket`；不是按桶拼出的 id 返回 `None`。
pub(super) fn codex_bucket(id: &str) -> Option<&str> {
    ["/primary", "/secondary", "/credits"]
        .into_iter()
        .find_map(|suffix| id.strip_suffix(suffix))
        .filter(|bucket| !bucket.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metric(id: &str, scope: &str) -> UsageMetric {
        UsageMetric {
            id: id.into(),
            scope: scope.into(),
            ..Default::default()
        }
    }

    /// 钉住与服务端 `parse.rs` 的 id 约定：这些 id 由 `parse::{claude, codex, kimi,
    /// opencode_sessions, opencode_stats, pi}` 与 zcode 本地用量车道产出。服务端改
    /// id 时这里必须一起改，否则厂商卡片会悄悄退回通用行。
    #[test]
    fn ids_follow_the_server_parsers() {
        let cases: &[(&str, &str, &str, Slot)] = &[
            ("claude", "account", "five_hour", Slot::Quota5h),
            ("claude", "account", "seven_day", Slot::QuotaWeekly),
            ("claude", "account", "spend_limit", Slot::QuotaSpend),
            ("claude", "session", "cost/total_cost_usd", Slot::Cost),
            (
                "claude",
                "session",
                "cost/total_duration_ms",
                Slot::Duration,
            ),
            (
                "claude",
                "session",
                "cost/total_api_duration_ms",
                Slot::ApiDuration,
            ),
            (
                "claude",
                "session",
                "context_window/used_percentage",
                Slot::ContextPercent,
            ),
            (
                "claude",
                "session",
                "context_window/current_usage",
                Slot::ContextTokens,
            ),
            (
                "claude",
                "session",
                "context_window/context_window_size",
                Slot::ContextWindow,
            ),
            ("codex", "account", "codex/primary", Slot::QuotaPrimary),
            (
                "codex",
                "account",
                "codex_other/secondary",
                Slot::QuotaSecondary,
            ),
            ("codex", "account", "codex/credits", Slot::Credits),
            ("kimi", "account", "limit5h", Slot::Quota5h),
            ("kimi", "account", "limit7d", Slot::Quota7d),
            ("kimi", "account", "monthTotal", Slot::QuotaMonthly),
            ("kimi", "account", "monthCode", Slot::QuotaMonthlyCode),
            ("kimi", "account", "summary", Slot::QuotaPlan),
            (
                "kimi",
                "account",
                "available_balance",
                Slot::BalanceAvailable,
            ),
            ("kimi", "account", "voucher_balance", Slot::BalanceVoucher),
            ("kimi", "account", "cash_balance", Slot::BalanceCash),
            ("kimi", "account", "extra_usage/balance", Slot::ExtraBalance),
            ("kimi", "account", "extra_usage/total", Slot::ExtraTotal),
            (
                "kimi",
                "account",
                "extra_usage/monthly_used",
                Slot::ExtraMonthUsed,
            ),
            (
                "kimi",
                "account",
                "extra_usage/monthly_limit",
                Slot::ExtraMonthCap,
            ),
            ("kimi", "account", "balance_cents", Slot::ExtraBalance),
            (
                "kimi",
                "account",
                "monthly_used_cents",
                Slot::ExtraMonthUsed,
            ),
            (
                "kimi",
                "account",
                "monthly_charge_limit_cents",
                Slot::ExtraMonthCap,
            ),
            ("opencode", "local", "sessions", Slot::Sessions),
            (
                "opencode",
                "local",
                "child_sessions",
                Slot::SubagentSessions,
            ),
            ("opencode", "local", "total_cost", Slot::Cost),
            ("opencode", "local", "input_tokens", Slot::TokensInput),
            ("opencode", "local", "output_tokens", Slot::TokensOutput),
            (
                "opencode",
                "local",
                "reasoning_tokens",
                Slot::TokensReasoning,
            ),
            (
                "opencode",
                "local",
                "cache_read_tokens",
                Slot::TokensCacheRead,
            ),
            (
                "opencode",
                "local",
                "cache_write_tokens",
                Slot::TokensCacheWrite,
            ),
            ("pi", "session", "context/percent", Slot::ContextPercent),
            ("pi", "session", "context/tokens", Slot::ContextTokens),
            (
                "pi",
                "session",
                "context/context_window",
                Slot::ContextWindow,
            ),
            ("pi", "session", "session/cost_usd", Slot::Cost),
            ("pi", "session", "session/tokens/input", Slot::TokensInput),
            ("pi", "session", "session/tokens/output", Slot::TokensOutput),
            (
                "pi",
                "session",
                "session/tokens/cache_read",
                Slot::TokensCacheRead,
            ),
            (
                "pi",
                "session",
                "session/tokens/cache_write",
                Slot::TokensCacheWrite,
            ),
            ("pi", "session", "session/tokens/total", Slot::TokensTotal),
            ("pi", "session", "session/model", Slot::Model),
            ("zcode", "local", "session/tokens/main", Slot::TokensMain),
            (
                "zcode",
                "local",
                "session/tokens/subagents",
                Slot::TokensSubagents,
            ),
            ("zcode", "local", "session/tokens/total", Slot::TokensTotal),
            ("zcode", "local", "session/tool_uses", Slot::ToolUses),
            ("zcode", "local", "session/subagents", Slot::Subagents),
            ("zcode", "local", "session/window_hours", Slot::WindowHours),
        ];
        let mut hit = vec![false; SLOT_RULES.len()];
        for (agent, scope, id, slot) in cases {
            let metric = metric(id, scope);
            assert_eq!(slot_of(agent, &metric), Some(*slot), "{agent} {scope} {id}");
            let rule = SLOT_RULES
                .iter()
                .position(|rule| {
                    rule.agent == *agent
                        && rule.scope.is_none_or(|scope| scope == metric.scope)
                        && rule.id.matches(id)
                })
                .expect("命中的规则");
            hit[rule] = true;
        }
        // 表里每一行都被上面的清单覆盖：新增匹配规则必须同时钉进清单。
        let missed = SLOT_RULES
            .iter()
            .zip(&hit)
            .filter(|(_, hit)| !**hit)
            .map(|(rule, _)| format!("{} {:?}", rule.agent, rule.id))
            .collect::<Vec<_>>();
        assert!(missed.is_empty(), "未钉进清单的规则: {missed:?}");
    }

    /// 名义窗口长度只给长度写在 id 里的固定窗口：5 小时 = 18000 秒，每周 / 7 天
    /// = 604800 秒；codex 主 / 次窗口以服务端的 `window_seconds` 为准，月度 / 消费
    /// 额度 / 套餐额度周期不固定。
    #[test]
    fn fixed_windows_have_a_nominal_length() {
        let nominal = |agent: &str, id: &str| {
            slot_of(agent, &metric(id, "account")).and_then(Slot::nominal_window_secs)
        };
        assert_eq!(nominal("claude", "five_hour"), Some(18_000));
        assert_eq!(nominal("claude", "seven_day"), Some(604_800));
        assert_eq!(nominal("kimi", "limit5h"), Some(18_000));
        assert_eq!(nominal("kimi", "limit7d"), Some(604_800));
        for (agent, id) in [
            ("claude", "spend_limit"),
            ("codex", "codex/primary"),
            ("codex", "codex/secondary"),
            ("kimi", "monthTotal"),
            ("kimi", "monthCode"),
            ("kimi", "summary"),
        ] {
            assert_eq!(nominal(agent, id), None, "{agent} {id}");
        }
    }

    #[test]
    fn scope_agent_and_suffix_boundaries_are_respected() {
        // scope 不符：claude 的 API 账号（organization）同名 id 不进额度槽。
        assert_eq!(
            slot_of("claude", &metric("five_hour", "organization")),
            None
        );
        // 别家的同名 id 不串台。
        assert_eq!(slot_of("codex", &metric("five_hour", "account")), None);
        assert_eq!(slot_of("unknown", &metric("sessions", "local")), None);
        // 后缀规则要求真有桶名前缀。
        assert_eq!(slot_of("codex", &metric("/primary", "account")), None);
        // zcode 放宽 scope。
        assert_eq!(
            slot_of("zcode", &metric("session/tool_uses", "session")),
            Some(Slot::ToolUses)
        );
        assert_eq!(codex_bucket("codex/primary"), Some("codex"));
        assert_eq!(codex_bucket("gpt 5/credits"), Some("gpt 5"));
        assert_eq!(codex_bucket("/primary"), None);
        assert_eq!(codex_bucket("five_hour"), None);
    }
}
