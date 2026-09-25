//! 解析厂商公开结果。没有确切含义的数字不作为账号额度显示。

use super::metric_texts;
use crate::api::schema::UsageMetric;
use crate::i18n::UsageMetricTexts;
use serde_json::Value;
use std::collections::HashMap;

fn finite(value: Option<&Value>) -> Option<f64> {
    value
        .and_then(|v| v.as_f64().or_else(|| v.as_str()?.parse().ok()))
        .filter(|v| v.is_finite() && *v >= 0.0)
}

fn timestamp(value: Option<&Value>) -> Option<u64> {
    let value = value?;
    if let Some(number) = value.as_u64() {
        return Some(if number > 100_000_000_000 {
            number / 1000
        } else {
            number
        });
    }
    if let Some(text) = value.as_str() {
        if let Ok(number) = text.parse::<u64>() {
            return Some(number);
        }
        return time::OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339)
            .ok()
            .and_then(|date| u64::try_from(date.unix_timestamp()).ok());
    }
    None
}

fn first_number(value: &Value, names: &[&str]) -> Option<f64> {
    names.iter().find_map(|name| finite(value.get(*name)))
}

fn window(id: String, label: String, value: &Value, scope: &str) -> Option<UsageMetric> {
    // id / label 可能由厂商 JSON 的键或 `label` 字段拼出，unit 直接取自 JSON：进入指标前统一
    // 清洗控制字符，让单条脏字段不至于在 `retain_valid` 处被丢弃。
    let id = clean_field(&id);
    let label = clean_field(&label);
    let percent = first_number(
        value,
        &[
            "usedPercent",
            "used_percentage",
            "used_percent",
            "utilization",
        ],
    )
    .or_else(|| {
        // Kimi 2.x reports `usedRatio` on a 0–1 scale.
        first_number(value, &["usedRatio"]).map(|ratio| ratio.clamp(0.0, 1.0) * 100.0)
    });
    let used = first_number(value, &["used", "used_amount", "usage", "consumed"]);
    let limit = first_number(value, &["limit", "total", "total_amount"]);
    let remaining = first_number(value, &["remaining", "remaining_amount", "limit_remaining"]);
    if percent.is_none() && used.is_none() && limit.is_none() && remaining.is_none() {
        return None;
    }
    let unit = value
        .get("unit")
        .and_then(Value::as_str)
        .map(clean_field)
        .filter(|unit| !unit.is_empty())
        .unwrap_or_else(|| {
            if percent.is_some() && used.is_none() {
                "%".into()
            } else {
                metric_texts().quota_unit.into()
            }
        });
    let percentage = percent.or_else(|| {
        used.zip(limit)
            .filter(|(_, total)| *total > 0.0)
            .map(|(used, total)| used / total * 100.0)
    });
    Some(UsageMetric {
        id,
        label,
        scope: scope.into(),
        unit,
        used,
        limit,
        remaining,
        used_percent: percentage,
        resets_at: ["resetsAt", "resets_at", "resetAt", "reset_time", "reset_at"]
            .iter()
            .find_map(|key| timestamp(value.get(*key))),
        window_seconds: value
            .get("windowDurationMins")
            .and_then(Value::as_u64)
            .map(|v| v.saturating_mul(60))
            .or_else(|| value.get("window_seconds").and_then(Value::as_u64)),
        ..Default::default()
    })
}

pub(super) fn codex(value: &Value) -> Vec<UsageMetric> {
    let mut metrics = Vec::new();
    let buckets = value.get("rateLimitsByLimitId").and_then(Value::as_object);
    let fallback = value.get("rateLimits");
    let entries = if let Some(buckets) = buckets.filter(|b| !b.is_empty()) {
        buckets
            .iter()
            .map(|(id, value)| (id.as_str(), value))
            .collect::<Vec<_>>()
    } else {
        fallback
            .map(|value| vec![("codex", value)])
            .unwrap_or_default()
    };
    for (id, bucket) in entries {
        let name = bucket
            .get("limitName")
            .and_then(Value::as_str)
            .unwrap_or(id);
        let labels = metric_texts();
        for (key, label) in [
            ("primary", labels.quota_primary),
            ("secondary", labels.quota_secondary),
        ] {
            if let Some(metric) = window(
                format!("{id}/{key}"),
                format!("{name} · {label}"),
                &bucket[key],
                "account",
            ) {
                metrics.push(metric);
            }
        }
        if let Some(balance) = bucket
            .pointer("/credits/balance")
            .filter(|v| v.is_string() || v.is_number())
        {
            metrics.push(UsageMetric {
                id: clean_field(&format!("{id}/credits")),
                label: labels.credits.into(),
                unit: "credits".into(),
                scope: "account".into(),
                amount_decimal: Some(decimal(balance)),
                ..Default::default()
            });
        }
    }
    // The backend count is authoritative: details may be omitted, empty, or capped.
    if let Some(count) = value
        .pointer("/rateLimitResetCredits/availableCount")
        .and_then(Value::as_i64)
        .filter(|count| *count >= 0)
    {
        let exact = count <= 1_i64 << 53;
        metrics.push(UsageMetric {
            id: "rate_limit_reset/available".into(),
            label: metric_texts().codex_reset_credits_available.into(),
            scope: "account".into(),
            unit: "resets".into(),
            remaining: exact.then_some(count as f64),
            text_value: (!exact).then(|| format!("{count} resets")),
            ..Default::default()
        });
    }
    codex_official_usage(&value["officialUsage"], &mut metrics);
    metrics
}

/// 官方 0.157 `AccountTokenUsageSummary` 的已知计数；不是额度窗口，也不以每日桶
/// 之和替代 lifetime。null/非法值缺席，0 则保留。超过 f64 精确整数范围时用文本保真。
fn codex_count(id: String, label: String, unit: &str, count: i64) -> UsageMetric {
    let exact_float = count <= 1_i64 << 53;
    UsageMetric {
        id,
        label,
        scope: "account".into(),
        unit: unit.into(),
        used: exact_float.then_some(count as f64),
        text_value: (!exact_float).then(|| format!("{count} {unit}")),
        ..Default::default()
    }
}

fn codex_official_usage(value: &Value, metrics: &mut Vec<UsageMetric>) {
    let labels = metric_texts();
    for (key, id, label, unit) in [
        (
            "lifetimeTokens",
            "lifetime_tokens",
            labels.codex_lifetime_tokens,
            "tokens",
        ),
        (
            "peakDailyTokens",
            "peak_daily_tokens",
            labels.codex_peak_daily_tokens,
            "tokens",
        ),
        (
            "longestRunningTurnSec",
            "longest_running_turn",
            labels.codex_longest_turn,
            "seconds",
        ),
        (
            "currentStreakDays",
            "current_streak",
            labels.codex_current_streak,
            "days",
        ),
        (
            "longestStreakDays",
            "longest_streak",
            labels.codex_longest_streak,
            "days",
        ),
    ] {
        if let Some(count) = value["summary"][key].as_i64().filter(|count| *count >= 0) {
            metrics.push(codex_count(
                format!("usage/{id}"),
                label.into(),
                unit,
                count,
            ));
        }
    }
    // 日期去重且只保留最近 90 桶；有界地图避免大量历史占满通用 128 指标预算。
    // startDate 是官方日期标签，不擅自换算成本机时区或推定为完整计费周期。
    const MAX_DAILY_BUCKETS: usize = 90;
    let mut daily = std::collections::BTreeMap::new();
    if let Some(buckets) = value["dailyUsageBuckets"].as_array() {
        for bucket in buckets {
            let Some(date) = bucket["startDate"]
                .as_str()
                .filter(|date| codex_usage_date(date))
            else {
                continue;
            };
            let Some(tokens) = bucket["tokens"].as_i64().filter(|tokens| *tokens >= 0) else {
                continue;
            };
            daily.entry(date).or_insert(tokens);
            if daily.len() > MAX_DAILY_BUCKETS {
                daily.pop_first();
            }
        }
    }
    metrics.extend(daily.into_iter().rev().map(|(date, tokens)| {
        codex_count(
            format!("usage/daily/{date}"),
            format!("{} · {date}", labels.codex_daily_tokens),
            "tokens",
            tokens,
        )
    }));
}

fn codex_usage_date(date: &str) -> bool {
    let bytes = date.as_bytes();
    if bytes.len() != 10
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || !bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| index == 4 || index == 7 || byte.is_ascii_digit())
    {
        return false;
    }
    let valid = || {
        let year = date[..4].parse::<i32>().ok()?;
        let month = time::Month::try_from(date[5..7].parse::<u8>().ok()?).ok()?;
        let day = date[8..].parse::<u8>().ok()?;
        time::Date::from_calendar_date(year, month, day).ok()
    };
    valid().is_some()
}

/// 普通套餐使用许可是独立的后端事实；百分比与重置时间不能推断该许可。
/// false 不代表额外 credits / reserve 都不可用，更不改变查询结果的 Ready 状态。
pub(super) fn codex_usage_notice(value: &Value) -> Option<&'static str> {
    value["ordinaryUsageAllowed"].as_bool().map(|allowed| {
        let texts = super::notices();
        if allowed {
            texts.codex_ordinary_usage_allowed
        } else {
            texts.codex_ordinary_usage_blocked
        }
    })
}

/// 从指标文案表里取一个标签：表按 server 的界面语言选定，常量表里只存取法。
pub(super) type MetricLabel = fn(&UsageMetricTexts) -> &'static str;

/// claude 的账号额度窗口（官方 statusline `rate_limits` 的键）与标签（按 server 语言取）。
pub(super) const CLAUDE_RATE_LIMIT_WINDOWS: [(&str, MetricLabel); 3] = [
    ("five_hour", |labels| labels.quota_5h),
    ("seven_day", |labels| labels.quota_weekly),
    ("spend_limit", |labels| labels.quota_spend),
];

/// 沿用的过期窗口在 `text_value` 里的明示：官方 statusline 会在窗口过了 `resets_at` 之后把它
/// 从 JSON 里去掉，缺席不是归零。指标保留上次的 `used_percent` 与已经过去的 `resets_at`
/// （客户端也可据「`resets_at` 不晚于当前时间」自行判定）。
pub(super) fn claude_stale_window_text() -> &'static str {
    metric_texts().stale_window
}

/// 沿用窗口的上限（秒）：过了 `resets_at` 的从 `resets_at` 起算，没有 `resets_at` 的从最近
/// 一次出现在报文里起算。超过最长的窗口周期（7 天）后上次值已无参考意义。
const CLAUDE_STALE_WINDOW_MAX_AGE_SECS: u64 = 7 * 24 * 60 * 60;

/// `context_window` 的数值为 `null`（会话首次 API 调用之前、`/compact` 之后）时的明示：未知
/// 不是 0，数值字段保持 `None`。
pub(super) fn claude_context_pending_text() -> &'static str {
    metric_texts().claude_context_pending
}

/// 官方 statusline JSON → 指标：账号额度窗口（`rate_limits`，`scope = account`）在前，本会话
/// 的费用 / 时长 / 上下文（`cost`、`context_window`，`scope = session`）在后。
///
/// - `spend_limit.used_percentage` 超限后可大于 100，原样保留、不截断。
/// - 会话级指标刻意不写 `used_percent`、也不成对写 `used` + `limit`：客户端把这两种形态都
///   当作账号额度压力，而上下文占用不是账号额度。百分比放在 `unit = "%"` 指标的 `used` 里。
/// - 多个会话共用一个账号时，会话级指标反映最近一次上报的那个会话。
pub(super) fn claude(value: &Value) -> Vec<UsageMetric> {
    let limits = value.get("rate_limits").unwrap_or(value);
    let mut metrics = CLAUDE_RATE_LIMIT_WINDOWS
        .into_iter()
        .filter_map(|(key, label)| {
            let label = label(metric_texts());
            window(key.into(), label.into(), &limits[key], "account")
        })
        .collect::<Vec<_>>();
    metrics.extend(claude_session(value));
    metrics
}

/// claude 的账号额度窗口指标（`CLAUDE_RATE_LIMIT_WINDOWS` 之一、`scope = account`）。
pub(super) fn is_claude_window(metric: &UsageMetric) -> bool {
    metric.scope == "account"
        && CLAUDE_RATE_LIMIT_WINDOWS
            .iter()
            .any(|(id, _)| metric.id == *id)
}

/// 补回本次报文里缺席的额度窗口：官方 statusline 在窗口过了 `resets_at` 后把它去掉，会话的
/// 首个 API 响应之前也整段缺省——两种缺席都不是归零。上次的窗口原样沿用；`resets_at` 已过
/// 的标为过期（`CLAUDE_STALE_WINDOW_TEXT`），过期超过 `CLAUDE_STALE_WINDOW_MAX_AGE_SECS`
/// 的不再沿用。没有 `resets_at` 的窗口无从判断是否已重置：按 `last_seen`（窗口 id → 最近
/// 一次真正出现在报文里的秒级时刻）限期，超过同一上限或时刻未知的不再沿用，免得被无限期
/// 沿用。结果里额度窗口按固定顺序排在会话级指标之前。
pub(super) fn claude_retain_missing_windows(
    fresh: Vec<UsageMetric>,
    previous: &[UsageMetric],
    last_seen: &HashMap<String, u64>,
    now_secs: u64,
) -> Vec<UsageMetric> {
    let (mut windows, session): (Vec<_>, Vec<_>) = fresh.into_iter().partition(is_claude_window);
    for carried in previous.iter().filter(|metric| is_claude_window(metric)) {
        if windows.iter().any(|metric| metric.id == carried.id) {
            continue;
        }
        let mut carried = carried.clone();
        match carried.resets_at {
            Some(resets_at) if resets_at <= now_secs => {
                if now_secs - resets_at > CLAUDE_STALE_WINDOW_MAX_AGE_SECS {
                    continue;
                }
                carried.text_value = Some(claude_stale_window_text().into());
            }
            Some(_) => {}
            None => {
                let expired = last_seen.get(&carried.id).is_none_or(|seen| {
                    now_secs.saturating_sub(*seen) > CLAUDE_STALE_WINDOW_MAX_AGE_SECS
                });
                if expired {
                    continue;
                }
            }
        }
        windows.push(carried);
    }
    let order = |metric: &UsageMetric| {
        CLAUDE_RATE_LIMIT_WINDOWS
            .iter()
            .position(|(id, _)| metric.id == *id)
    };
    windows.sort_by_key(order);
    windows.extend(session);
    windows
}

/// 毫秒时长的紧凑文案：`1h02m` / `12m05s` / `45s`。
fn duration_text(ms: f64) -> String {
    let seconds = (ms / 1000.0).round() as u64;
    if seconds >= 3600 {
        format!("{}h{:02}m", seconds / 3600, seconds % 3600 / 60)
    } else if seconds >= 60 {
        format!("{}m{:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}s")
    }
}

/// 百分比文案：整数不带小数，其余保留一位。
fn percent_text(percent: f64) -> String {
    if percent.fract() == 0.0 {
        format!("{percent:.0}%")
    } else {
        format!("{percent:.1}%")
    }
}

/// 官方 statusline 的会话级字段：`cost.*` 与 `context_window.*`。字段缺失就不产出对应指标；
/// `context_window` 在场但数值为 `null` 时照常产出指标、数值留 `None` 并给出明示文案。
fn claude_session(value: &Value) -> Vec<UsageMetric> {
    let mut metrics = Vec::new();
    let labels = metric_texts();
    let session = |id: &str, label: &str, unit: &str| UsageMetric {
        id: id.into(),
        label: label.into(),
        unit: unit.into(),
        scope: "session".into(),
        ..Default::default()
    };
    if let Some(cost) = value
        .pointer("/cost/total_cost_usd")
        .filter(|cost| finite(Some(cost)).is_some())
    {
        metrics.push(UsageMetric {
            // 厂商这一项也是逐次调用累加出来的浮点和：按金额规整，不原样透出尾差。
            amount_decimal: Some(money_decimal(cost)),
            ..session("cost/total_cost_usd", labels.session_cost_estimate, "USD")
        });
    }
    for (key, label) in [
        ("total_duration_ms", labels.session_duration),
        ("total_api_duration_ms", labels.session_api_duration),
    ] {
        if let Some(ms) = finite(value.pointer(&format!("/cost/{key}"))) {
            metrics.push(UsageMetric {
                used: Some(ms),
                text_value: Some(duration_text(ms)),
                ..session(&format!("cost/{key}"), label, "ms")
            });
        }
    }
    let Some(context) = value
        .get("context_window")
        .filter(|context| context.is_object())
    else {
        return metrics;
    };
    // `used_percentage` / `remaining_percentage` 在会话早期可为 null。
    let used_percentage = finite(context.get("used_percentage"));
    metrics.push(UsageMetric {
        used: used_percentage,
        remaining: finite(context.get("remaining_percentage")),
        text_value: Some(
            used_percentage.map_or_else(|| claude_context_pending_text().into(), percent_text),
        ),
        ..session("context_window/used_percentage", labels.context_used, "%")
    });
    // `current_usage` 在首次 API 调用前与 `/compact` 后为 null；口径与官方 `used_percentage`
    // 一致，只计输入侧（input + cache 创建 + cache 读取），不含输出。
    let current = context
        .get("current_usage")
        .filter(|usage| usage.is_object());
    let input_tokens = current.map(|usage| {
        [
            "input_tokens",
            "cache_creation_input_tokens",
            "cache_read_input_tokens",
        ]
        .into_iter()
        .filter_map(|key| finite(usage.get(key)))
        .sum::<f64>()
    });
    metrics.push(UsageMetric {
        used: input_tokens,
        text_value: input_tokens
            .is_none()
            .then(|| claude_context_pending_text().into()),
        ..session(
            "context_window/current_usage",
            labels.context_tokens_input,
            "tokens",
        )
    });
    if let Some(size) = finite(context.get("context_window_size")) {
        metrics.push(UsageMetric {
            used: Some(size),
            ..session(
                "context_window/context_window_size",
                labels.context_window,
                "tokens",
            )
        });
    }
    metrics
}

pub(super) fn kimi(value: &Value) -> Vec<UsageMetric> {
    let value = value.get("data").unwrap_or(value);
    let labels = metric_texts();
    let mut metrics = Vec::new();
    if let Some(metric) = window(
        "summary".into(),
        labels.quota_plan.into(),
        &value["summary"],
        "account",
    ) {
        metrics.push(metric);
    }
    // Kimi 2.x packs quota windows under `quota.usages` keyed by window id.
    if let Some(usages) = value.pointer("/quota/usages").and_then(Value::as_object) {
        for (key, window_value) in usages {
            // 标签与官方 TUI 的用量面板一致：monthCode 是月度额度里 Code 占用的那一部分。
            let label = match key.as_str() {
                "limit5h" => labels.quota_5h,
                "limit7d" => labels.quota_7d,
                "monthTotal" => labels.quota_monthly,
                "monthCode" => labels.quota_monthly_code,
                other => other,
            };
            if let Some(metric) = window(key.clone(), label.into(), window_value, "account") {
                metrics.push(metric);
            }
        }
    }
    if let Some(limits) = value.get("limits").and_then(Value::as_array) {
        for (index, limit) in limits.iter().enumerate() {
            let label = limit
                .get("name")
                .or_else(|| limit.get("label"))
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| {
                    crate::i18n::fill(labels.quota_window_fmt, &[("n", &(index + 1).to_string())])
                });
            let key = limit
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| format!("window-{index}"));
            if let Some(metric) = window(key, label, limit, "account") {
                metrics.push(metric);
            }
        }
    }
    for (key, label) in [
        ("available_balance", labels.balance_available),
        ("voucher_balance", labels.balance_voucher),
        ("cash_balance", labels.balance_cash),
    ] {
        if let Some(amount) = value.get(key).filter(|v| finite(Some(v)).is_some()) {
            metrics.push(UsageMetric {
                id: key.into(),
                label: label.into(),
                unit: "CNY".into(),
                scope: "account".into(),
                amount_decimal: Some(decimal(amount)),
                ..Default::default()
            });
        }
    }
    metrics.extend(kimi_extra_usage(value.pointer("/quota/extraUsage")));
    if let Some(wallet) = value.get("extra_usage").filter(|wallet| wallet.is_object()) {
        if let Some(currency) = wallet.get("currency").and_then(Value::as_str) {
            for (key, label) in [
                ("balance_cents", labels.extra_balance),
                ("monthly_used_cents", labels.extra_month_used),
                ("monthly_charge_limit_cents", labels.extra_month_cap),
            ] {
                if key == "monthly_charge_limit_cents"
                    && wallet
                        .get("monthly_charge_limit_enabled")
                        .and_then(Value::as_bool)
                        != Some(true)
                {
                    continue;
                }
                if let Some(amount) = wallet
                    .get(key)
                    .filter(|amount| finite(Some(amount)).is_some())
                {
                    metrics.push(UsageMetric {
                        id: key.into(),
                        label: label.into(),
                        scope: "account".into(),
                        unit: clean_field(&format!("{currency} cents")),
                        amount_decimal: Some(decimal(amount)),
                        ..Default::default()
                    });
                }
            }
        }
    }
    metrics
}

/// 整数「分」→ 主单位的十进制文本（`12345` → `123.45`），不经浮点往返。
fn cents_decimal(cents: u64) -> String {
    format!("{}.{:02}", cents / 100, cents % 100)
}

/// Kimi Code 2.x 的 `quota.extraUsage`（超额按量的加油包钱包，可为 `null`）：金额类指标，单位
/// 是币种，不折算成百分比。字段名取自 2.0.2 的 `boosterWalletInfoSchema`：`totalCents` 是
/// 加油包总额、`balanceCents` 是余额、`monthlyUsedCents` 是本月按量费用；
/// `monthlyChargeLimitCents` 只在 `monthlyChargeLimitEnabled` 且大于 0 时才是真实上限（官方
/// 面板同一判据）。server API 官方标注为 experimental：任何字段缺失或形状不符都只跳过该项；
/// 缺 `currency` 时按官方实现的缺省取 `USD`。
fn kimi_extra_usage(wallet: Option<&Value>) -> Vec<UsageMetric> {
    let Some(wallet) = wallet.filter(|wallet| wallet.is_object()) else {
        return Vec::new();
    };
    let currency = wallet
        .get("currency")
        .and_then(Value::as_str)
        .map(clean_field)
        .filter(|currency| !currency.is_empty())
        .unwrap_or_else(|| "USD".into());
    let cents = |key: &str| wallet.get(key).and_then(Value::as_u64);
    let limit_enabled = wallet
        .get("monthlyChargeLimitEnabled")
        .and_then(Value::as_bool)
        == Some(true);
    let labels = metric_texts();
    [
        (
            "extra_usage/balance",
            labels.extra_balance,
            cents("balanceCents"),
        ),
        ("extra_usage/total", labels.extra_total, cents("totalCents")),
        (
            "extra_usage/monthly_used",
            labels.extra_month_used,
            cents("monthlyUsedCents"),
        ),
        (
            "extra_usage/monthly_limit",
            labels.extra_month_cap,
            cents("monthlyChargeLimitCents").filter(|limit| limit_enabled && *limit > 0),
        ),
    ]
    .into_iter()
    .filter_map(|(id, label, cents)| {
        Some(UsageMetric {
            id: id.into(),
            label: label.into(),
            unit: currency.clone(),
            scope: "account".into(),
            amount_decimal: Some(cents_decimal(cents?)),
            ..Default::default()
        })
    })
    .collect()
}

pub(super) fn moonshot_balance(value: &Value, currency: &str) -> Vec<UsageMetric> {
    let value = value.get("data").unwrap_or(value);
    let labels = metric_texts();
    [
        ("available_balance", labels.balance_available),
        ("voucher_balance", labels.balance_voucher),
        ("cash_balance", labels.balance_cash),
    ]
    .into_iter()
    .filter_map(|(id, label)| {
        let amount = value.get(id)?;
        let decimal = decimal(amount);
        if !decimal.parse::<f64>().ok()?.is_finite() {
            return None;
        }
        Some(UsageMetric {
            id: id.into(),
            label: label.into(),
            unit: currency.into(),
            scope: "account".into(),
            amount_decimal: Some(decimal),
            ..Default::default()
        })
    })
    .collect()
}

pub(super) fn openrouter(value: &Value) -> Vec<UsageMetric> {
    let value = value.get("data").unwrap_or(value);
    let labels = metric_texts();
    let mut metrics = Vec::new();
    for (key, label) in [
        ("usage", labels.key_usage),
        ("usage_daily", labels.key_usage_daily),
        ("usage_weekly", labels.key_usage_weekly),
        ("usage_monthly", labels.key_usage_monthly),
        ("limit_remaining", labels.key_limit_remaining),
        ("total_credits", labels.account_total_credits),
        ("total_usage", labels.account_total_usage),
    ] {
        if let Some(amount) = value.get(key).filter(|v| finite(Some(v)).is_some()) {
            metrics.push(UsageMetric {
                id: key.into(),
                label: label.into(),
                unit: "USD".into(),
                scope: if key.starts_with("total_") {
                    "account"
                } else {
                    "api_key"
                }
                .into(),
                amount_decimal: Some(decimal(amount)),
                ..Default::default()
            });
        }
    }
    metrics
}

pub(super) fn structured(value: &Value, scope: &str) -> Vec<UsageMetric> {
    let mut result = Vec::new();
    visit_structured(value, scope, "", 0, &mut result);
    result.truncate(128);
    result
}

fn visit_structured(
    value: &Value,
    scope: &str,
    path: &str,
    depth: usize,
    output: &mut Vec<UsageMetric>,
) {
    if depth > 8 || output.len() >= 128 {
        return;
    }
    if let Some(items) = value.as_array() {
        for (index, child) in items.iter().take(128).enumerate() {
            visit_structured(child, scope, &format!("{path}/{index}"), depth + 1, output);
        }
    } else if let Some(object) = value.as_object() {
        let label = object
            .get("label")
            .or_else(|| object.get("name"))
            .and_then(Value::as_str)
            .unwrap_or(path);
        if let Some(metric) = window(path.into(), label.into(), value, scope) {
            output.push(metric);
        }
        for (key, child) in object {
            if key == "tokens"
                || key.ends_with("Tokens")
                || key.ends_with("_tokens")
                || matches!(
                    key.as_str(),
                    "input" | "output" | "cacheRead" | "cacheWrite"
                )
            {
                if let Some(count) = finite(Some(child)) {
                    output.push(UsageMetric {
                        id: clean_field(&format!("{path}/{key}")),
                        label: clean_field(key),
                        unit: "tokens".into(),
                        scope: scope.into(),
                        used: Some(count),
                        ..Default::default()
                    });
                    continue;
                }
            }
            if matches!(
                key.as_str(),
                "totalCost" | "total_cost" | "total_cost_usd" | "cost_usd" | "balance"
            ) && finite(Some(child)).is_some()
            {
                output.push(UsageMetric {
                    id: clean_field(&format!("{path}/{key}")),
                    label: clean_field(key),
                    unit: if key == "balance" {
                        metric_texts().balance_unit
                    } else {
                        "USD"
                    }
                    .into(),
                    scope: scope.into(),
                    amount_decimal: Some(decimal(child)),
                    ..Default::default()
                });
            } else if child.is_array() || child.is_object() {
                visit_structured(child, scope, &format!("{path}/{key}"), depth + 1, output);
            }
        }
    }
}

pub(super) fn decimal(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string())
}

/// 金额的十进制表示。客户端把 `amount_decimal` 原样渲染，所以浮点合计的 IEEE754 尾差
/// （`0.36 + 0.04` = `0.39999999999999997`）不能透出去：数值按 6 位小数规整后去掉尾随 0，
/// 比任何单次调用的单价都细。厂商给的字符串是它自己的精确十进制表示，原样保留。
///
/// pi 扩展自 v10 起在推送前就已规整；这里是对更早版本已装扩展（herdr 只提示过期、不会
/// 自动改写用户的扩展文件）与厂商浮点合计的兜底。
pub(super) fn money_decimal(value: &Value) -> String {
    let Some(amount) = value.as_f64().filter(|amount| amount.is_finite()) else {
        return decimal(value);
    };
    let rendered = format!("{amount:.6}");
    // `{:.6}` 必定带小数点，先去尾随 0 再去小数点不会吃掉整数部分。
    rendered
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_owned()
}

/// `screen` 的两条正则：每次探测都会调用，编译一次即可。
fn screen_patterns() -> Option<&'static (regex::Regex, regex::Regex)> {
    static PATTERNS: std::sync::OnceLock<Option<(regex::Regex, regex::Regex)>> =
        std::sync::OnceLock::new();
    PATTERNS
        .get_or_init(|| {
            let percent = regex::Regex::new(
                r"(?i)(\d+(?:\.\d+)?)\s*%\s*(used|remaining|left|已用|剩余)?",
            )
            .ok()?;
            let credits = regex::Regex::new(
                r"(?i)(\d+(?:\.\d+)?)\s*/\s*(\d+(?:\.\d+)?)\s*(credits?|tokens?|requests?|额度|积分)",
            )
            .ok()?;
            Some((percent, credits))
        })
        .as_ref()
}

pub(super) fn screen(text: &str, scope: &str) -> Vec<UsageMetric> {
    let Some((percent, credits)) = screen_patterns() else {
        return Vec::new();
    };
    let mut metrics = Vec::new();
    let mut seen = std::collections::HashSet::new();
    // 屏幕 / CLI 行先去掉 ANSI 与控制字符（NO_COLOR 未必被尊重、对齐用的制表符很常见），
    // 关键字判定、数字匹配与 label 都用清洗后的行。
    for line in text
        .lines()
        .map(clean_field)
        .filter(|line| !line.is_empty())
    {
        let line = line.as_str();
        let lower = line.to_lowercase();
        // 会话上下文百分比不是账号额度，绝不放进账号仪表。
        if lower.contains("context") || lower.contains("上下文") || lower.contains("loading") {
            continue;
        }
        let is_remaining =
            lower.contains("remaining") || lower.contains("left") || lower.contains("剩余");
        let is_used = lower.contains("used")
            || lower.contains("consumed")
            || lower.contains("已用")
            || lower.contains("已使用");
        // 没有明确方向或同时含两种口径时，不猜测百分比含义。
        if is_remaining == is_used {
            continue;
        }
        if let Some(found) = credits.captures(line) {
            let reported = found[1].parse::<f64>().ok();
            let total = found[2].parse::<f64>().ok();
            let used = if is_remaining {
                reported.zip(total).map(|(r, t)| (t - r).max(0.0))
            } else {
                reported
            };
            if used
                .zip(total)
                .is_none_or(|(u, t)| !u.is_finite() || !t.is_finite() || t <= 0.0)
            {
                continue;
            }
            let label = line.chars().take(120).collect::<String>();
            if seen.insert(label.clone()) {
                metrics.push(UsageMetric {
                    id: format!("cli-{}", metrics.len()),
                    label,
                    scope: scope.into(),
                    unit: found[3].to_owned(),
                    used,
                    limit: total,
                    remaining: is_remaining.then_some(reported).flatten(),
                    used_percent: used.zip(total).map(|(u, t)| u / t * 100.0),
                    ..Default::default()
                });
            }
        } else if let Some(found) = percent.captures(line) {
            let Some(value) = found[1]
                .parse::<f64>()
                .ok()
                .filter(|v| v.is_finite() && *v >= 0.0)
            else {
                continue;
            };
            let label = line.chars().take(120).collect::<String>();
            if seen.insert(label.clone()) {
                metrics.push(UsageMetric {
                    id: format!("cli-{}", metrics.len()),
                    label,
                    scope: scope.into(),
                    unit: "%".into(),
                    used_percent: Some(if is_remaining {
                        (100.0 - value).max(0.0)
                    } else {
                        value
                    }),
                    ..Default::default()
                });
            }
        }
    }
    metrics.truncate(64);
    metrics
}

/// 文本字段是否干净：无控制字符（含制表符与 ANSI 转义的 ESC），进入事件与持久化的文本
/// 不能携带终端画面里的排版控制。
fn clean_text(text: &str) -> bool {
    !text.chars().any(char::is_control)
}

/// 生产者侧的文本清洗：去 ANSI 序列、去控制字符（制表符等空白控制折叠为空格）、折叠连续
/// 空白、去首尾空白。所有由 CLI 行或厂商 JSON 拼出的 id / label / unit / text_value 都先经
/// 这里，再进 `retain_valid`——脏字段是清洗对象，不是否决整份快照的理由。
pub(super) fn clean_field(text: &str) -> String {
    let stripped = match super::transport::ansi_pattern() {
        Some(pattern) => pattern.replace_all(text, ""),
        None => std::borrow::Cow::Borrowed(text),
    };
    stripped
        .chars()
        .filter(|ch| ch.is_whitespace() || !ch.is_control())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// 指标数量上限：超出的部分被截掉。
const MAX_METRICS: usize = 128;

/// 单条指标是否合规：文本字段长度与控制字符、数值有限非负、金额文本只含数字字符。
fn metric_valid(metric: &UsageMetric) -> bool {
    metric.id.len() <= 256
        && clean_text(&metric.id)
        && metric
            .text_value
            .as_ref()
            .is_none_or(|value| value.len() <= 512 && clean_text(value))
        && metric.label.len() <= 512
        && clean_text(&metric.label)
        && metric.unit.len() <= 64
        && clean_text(&metric.unit)
        && metric.scope.len() <= 64
        && clean_text(&metric.scope)
        && [
            metric.used,
            metric.limit,
            metric.remaining,
            metric.used_percent,
        ]
        .into_iter()
        .flatten()
        .all(|v| v.is_finite() && v >= 0.0)
        && metric.amount_decimal.as_ref().is_none_or(|v| {
            v.len() <= 128
                && v.bytes()
                    .all(|c| c.is_ascii_digit() || matches!(c, b'.' | b'-' | b'+' | b'e' | b'E'))
        })
}

/// 过滤语义的校验：丢弃不合规的单条指标、截到 `MAX_METRICS` 条，返回丢弃的条数。一条脏
/// 指标只丢它自己，不连带否决整份探测结果或回调上报（否则 CLI 路径会把账号推进
/// `Unsupported` 终态、回调路径会整份拒收）。
pub(super) fn retain_valid(metrics: &mut Vec<UsageMetric>) -> usize {
    let before = metrics.len();
    metrics.retain(metric_valid);
    metrics.truncate(MAX_METRICS);
    before - metrics.len()
}

/// 全部合规且不超上限：测试断言用；生产路径一律走过滤语义的 `retain_valid`。
#[cfg(test)]
pub(super) fn validate(metrics: &[UsageMetric]) -> bool {
    metrics.len() <= MAX_METRICS && metrics.iter().all(metric_valid)
}

/// 公开账号身份（邮箱 / 用户 id）进入快照、事件与持久化前的统一清洗：去首尾空白、非空、
/// ≤ 256 字节、无控制字符。所有厂商探测与回调上报共用这一条规则（`persistence::store`
/// 的加载守门与之一致）。
pub(super) fn sanitize_identity(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|text| !text.is_empty() && text.len() <= 256 && clean_text(text))
        .map(str::to_owned)
}

/// 非交互输出是否是一份命令行帮助（yargs / commander 族在 flag 不受支持时直接打印用法，
/// 未必带 `unknown option` 字样）。判据：出现 `Options:` / `Commands:` / `Flags:` 这类
/// 选项列表标题行；或 `Usage:` 标题行**同时**伴有 flag 列表行（以 `-x` / `--flag` 开头的
/// 行）。`Usage:` 单独不成立——真实的用量输出也可能用 `Usage:` 作小标题。只认标题行，不认
/// `--help` 子串——错误提示里的「run … --help」不是帮助文本。
pub(super) fn cli_help_output(text: &str) -> bool {
    let mut usage_heading = false;
    let mut flag_line = false;
    for line in text.lines() {
        let trimmed = line.trim();
        let lower = trimmed.to_lowercase();
        if [
            "options:",
            "commands:",
            "positionals:",
            "arguments:",
            "flags:",
        ]
        .iter()
        .any(|heading| lower.starts_with(heading))
        {
            return true;
        }
        usage_heading |= lower.starts_with("usage:");
        flag_line |= is_flag_line(trimmed);
    }
    usage_heading && flag_line
}

/// 行是否以 `-x` / `--flag` 开头（帮助文本的 flag 列表行）；`- 项目` 之类的列表符号不算。
pub(super) fn is_flag_line(trimmed: &str) -> bool {
    let mut chars = trimmed.chars();
    chars.next() == Some('-')
        && chars
            .next()
            .is_some_and(|ch| ch == '-' || ch.is_ascii_alphabetic())
}

/// `opencode stats` 人类可读输出（1.17.x）里一行统计的定义。
struct StatsRow {
    /// CLI 打印的行名（精确匹配）。
    name: &'static str,
    /// 指标 id。
    id: &'static str,
    /// 标签（按 server 语言取）。
    label: MetricLabel,
    /// 单位；`USD` 的行是金额，其余是计数。
    unit: &'static str,
}

const fn stats_row(
    name: &'static str,
    id: &'static str,
    label: MetricLabel,
    unit: &'static str,
) -> StatsRow {
    StatsRow {
        name,
        id,
        label,
        unit,
    }
}

/// `opencode stats` 已知行：只认 `OVERVIEW` 与 `COST & TOKENS` 两张表；工具用量表
/// （`TOOL USAGE`）不是用量指标，不解析。
const OPENCODE_STATS_ROWS: &[StatsRow] = &[
    stats_row("Sessions", "sessions", |t| t.sessions, "sessions"),
    stats_row("Messages", "messages", |t| t.messages, "messages"),
    stats_row("Days", "days", |t| t.stats_days, "days"),
    stats_row("Total Cost", "total_cost", |t| t.total_cost, "USD"),
    stats_row(
        "Avg Cost/Day",
        "avg_cost_per_day",
        |t| t.avg_cost_per_day,
        "USD",
    ),
    stats_row(
        "Avg Tokens/Session",
        "avg_tokens_per_session",
        |t| t.avg_tokens_per_session,
        "tokens",
    ),
    stats_row(
        "Median Tokens/Session",
        "median_tokens_per_session",
        |t| t.median_tokens_per_session,
        "tokens",
    ),
    stats_row("Input", "input_tokens", |t| t.tokens_input, "tokens"),
    stats_row("Output", "output_tokens", |t| t.tokens_output, "tokens"),
    stats_row(
        "Cache Read",
        "cache_read_tokens",
        |t| t.tokens_cache_read,
        "tokens",
    ),
    stats_row(
        "Cache Write",
        "cache_write_tokens",
        |t| t.tokens_cache_write,
        "tokens",
    ),
];

/// 解析 `1,323` / `3.6M` / `690.3K` / `$0.88` 这类展示数字：千分位逗号、`K/M/B` 后缀与
/// 货币符号。返回 `(数值, 原始小数文本)`：没有倍率后缀时原样保留展示精度的文本（金额不经
/// 浮点往返）；带 `K/M/B` 后缀时文本是 `None`——剥掉后缀的数字串不是真实数值，不能当金额。
fn display_number(text: &str) -> Option<(f64, Option<String>)> {
    let text = text.trim().trim_start_matches('$').replace(',', "");
    let (digits, multiplier) = match text.chars().last()? {
        'K' | 'k' => (&text[..text.len() - 1], 1_000.0),
        'M' | 'm' => (&text[..text.len() - 1], 1_000_000.0),
        'B' | 'b' => (&text[..text.len() - 1], 1_000_000_000.0),
        _ => (text.as_str(), 1.0),
    };
    if digits.is_empty()
        || !digits
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'.')
    {
        return None;
    }
    let value = digits.parse::<f64>().ok()?;
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    let literal = (multiplier == 1.0).then(|| digits.to_owned());
    Some((value * multiplier, literal))
}

/// `opencode stats`（无 `--json` 的回退形态）框线表解析。输出是本地会话的累计统计，不是
/// 账号额度：`scope` 固定为 `local`，客户端按 `scope` 分区显示（分区标题由客户端渲染，
/// label 只保留指标本体）。只认识 `OVERVIEW` 与 `COST & TOKENS` 两张表里的已知行；未知
/// 行与 `TOOL USAGE` 跳过。金额行带 `K/M/B` 后缀时按乘算后的数值格式化（两位小数）。
pub(super) fn opencode_stats(text: &str) -> Vec<UsageMetric> {
    let mut metrics = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for line in text.lines().map(clean_field) {
        let body = line.trim_matches(|ch: char| ch.is_whitespace() || "│┃|".contains(ch));
        let Some((name, value)) = body.rsplit_once(char::is_whitespace) else {
            continue;
        };
        let name = name.trim();
        let Some(row) = OPENCODE_STATS_ROWS.iter().find(|row| row.name == name) else {
            continue;
        };
        let Some((number, literal)) = display_number(value) else {
            continue;
        };
        if !seen.insert(row.id) {
            continue;
        }
        let mut metric = UsageMetric {
            id: row.id.into(),
            label: (row.label)(metric_texts()).into(),
            unit: row.unit.into(),
            scope: "local".into(),
            ..Default::default()
        };
        if row.unit == "USD" {
            metric.amount_decimal = Some(literal.unwrap_or_else(|| format!("{number:.2}")));
        } else {
            metric.used = Some(number);
        }
        metrics.push(metric);
    }
    metrics
}

/// `opencode db <query> --format json` 的输出（`registry::OPENCODE_SESSION_TOTALS_SQL` 的
/// 单行聚合，`JSON.stringify(rows)` 形态的数组）→ 本地会话统计。与 `opencode_stats` 一样不是
/// 账号额度：`scope` 固定为 `local`。stdout 里 JSON 数组之前若夹杂日志行，取首个 `[` 到末个
/// `]` 之间再解析一次；列缺失只跳过该项。
pub(super) fn opencode_sessions(text: &str) -> Vec<UsageMetric> {
    let trimmed = text.trim();
    let rows = serde_json::from_str::<Value>(trimmed).ok().or_else(|| {
        let (start, end) = (trimmed.find('[')?, trimmed.rfind(']')?);
        serde_json::from_str::<Value>(trimmed.get(start..=end)?).ok()
    });
    let Some(row) = rows
        .as_ref()
        .and_then(Value::as_array)
        .and_then(|rows| rows.first())
        .filter(|row| row.is_object())
    else {
        return Vec::new();
    };
    let local = |id: &str, label: &str, unit: &str| UsageMetric {
        id: id.into(),
        label: label.into(),
        unit: unit.into(),
        scope: "local".into(),
        ..Default::default()
    };
    let labels = metric_texts();
    let mut metrics = Vec::new();
    for (column, id, label, unit) in [
        ("sessions", "sessions", labels.sessions, "sessions"),
        (
            "child_sessions",
            "child_sessions",
            labels.subagent_sessions,
            "sessions",
        ),
    ] {
        if let Some(count) = finite(row.get(column)) {
            metrics.push(UsageMetric {
                used: Some(count),
                ..local(id, label, unit)
            });
        }
    }
    if let Some(cost) = finite(row.get("cost")) {
        metrics.push(UsageMetric {
            // SQLite 的 REAL 合计带浮点尾差；费用按 4 位小数展示。
            amount_decimal: Some(format!("{cost:.4}")),
            ..local("total_cost", labels.total_cost, "USD")
        });
    }
    for (column, id, label) in [
        ("tokens_input", "input_tokens", labels.tokens_input),
        ("tokens_output", "output_tokens", labels.tokens_output),
        (
            "tokens_reasoning",
            "reasoning_tokens",
            labels.tokens_reasoning,
        ),
        (
            "tokens_cache_read",
            "cache_read_tokens",
            labels.tokens_cache_read,
        ),
        (
            "tokens_cache_write",
            "cache_write_tokens",
            labels.tokens_cache_write,
        ),
    ] {
        if let Some(count) = finite(row.get(column)) {
            metrics.push(UsageMetric {
                used: Some(count),
                ..local(id, label, "tokens")
            });
        }
    }
    metrics
}

/// pi 上下文用量未知（压缩之后、下一次响应之前 `tokens` / `percent` 为 null）时的明示。
pub(super) fn pi_context_pending_text() -> &'static str {
    metric_texts().pi_context_pending
}

/// pi 报文里的当前服务商 / 模型（`provider`、`model`，扩展已按 `responseModel` 优先取值）。
/// 供快照的 `provider` 字段与 `session/model` 指标共用。
pub(super) fn pi_provider_model(value: &Value) -> (Option<String>, Option<String>) {
    let text = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .map(clean_field)
            .filter(|text| !text.is_empty() && text.len() <= 128)
    };
    (text("provider"), text("model"))
}

/// herdr 的 pi 扩展（`integration/assets/pi/herdr-agent-state.ts`）推送的会话用量报文 → 指标。
/// 全部是会话级统计（`scope = session`），不是账号额度：pi 是多服务商 CLI，额度属于底层
/// 订阅账号，这里只带上当前 `provider/model` 供客户端关联，不重复计量。
///
/// - `context.tokens` / `context.percent` 在压缩之后、下一次响应之前为 null：数值留 `None`
///   并给出明示文案，与 0 区分。
/// - 费用只认扩展合计好的数值字段 `cost_usd`。pi 的会话条目里 `usage.cost` 是对象
///   （`{input, output, …, total}`），RPC `get_session_stats` 的 `cost` 才是数值——两种形态由
///   扩展各自取数，这里不接受对象形态，避免把对象当数值。数值经 `money_decimal` 规整，
///   旧版已装扩展推上来的浮点尾差不会原样显示。
/// - 与 claude 的会话级指标同理：不写 `used_percent`、不成对写 `used` + `limit`。
pub(super) fn pi(value: &Value) -> Vec<UsageMetric> {
    let labels = metric_texts();
    let session = |id: &str, label: &str, unit: &str| UsageMetric {
        id: id.into(),
        label: label.into(),
        unit: unit.into(),
        scope: "session".into(),
        ..Default::default()
    };
    let mut metrics = Vec::new();
    if let Some(context) = value.get("context").filter(|context| context.is_object()) {
        let percent = finite(context.get("percent"));
        metrics.push(UsageMetric {
            used: percent,
            text_value: Some(
                percent.map_or_else(|| pi_context_pending_text().into(), percent_text),
            ),
            ..session("context/percent", labels.context_used, "%")
        });
        let tokens = finite(context.get("tokens"));
        metrics.push(UsageMetric {
            used: tokens,
            text_value: tokens.is_none().then(|| pi_context_pending_text().into()),
            ..session("context/tokens", labels.context_tokens, "tokens")
        });
        if let Some(size) = finite(context.get("context_window")) {
            metrics.push(UsageMetric {
                used: Some(size),
                ..session("context/context_window", labels.context_window, "tokens")
            });
        }
    }
    if let Some(cost) = value
        .get("cost_usd")
        .filter(|cost| cost.is_number() && finite(Some(cost)).is_some())
    {
        metrics.push(UsageMetric {
            amount_decimal: Some(money_decimal(cost)),
            ..session("session/cost_usd", labels.session_cost, "USD")
        });
    }
    if let Some(tokens) = value.get("tokens").filter(|tokens| tokens.is_object()) {
        for (key, label) in [
            ("input", labels.tokens_input),
            ("output", labels.tokens_output),
            ("cache_read", labels.tokens_cache_read),
            ("cache_write", labels.tokens_cache_write),
            ("total", labels.tokens_total),
        ] {
            if let Some(count) = finite(tokens.get(key)) {
                metrics.push(UsageMetric {
                    used: Some(count),
                    ..session(&format!("session/tokens/{key}"), label, "tokens")
                });
            }
        }
    }
    // 只有在报文确实带了用量时才附上模型标签：单独一条模型名不是用量。
    if !metrics.is_empty() {
        let (provider, model) = pi_provider_model(value);
        let tag = match (provider, model) {
            (Some(provider), Some(model)) => Some(format!("{provider}/{model}")),
            (None, Some(model)) => Some(model),
            (Some(provider), None) => Some(provider),
            (None, None) => None,
        };
        if let Some(tag) = tag {
            metrics.push(UsageMetric {
                text_value: Some(tag),
                ..session("session/model", labels.current_model, "")
            });
        }
    }
    metrics
}

/// `claude auth status --json` 的非交互登录预检结果。只取登录判定与公开身份，不含令牌。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ClaudeAuthStatus {
    pub logged_in: bool,
    /// 登录邮箱（小写、≤256、无控制字符）；用作账号身份，切换账号时触发绑定失效。
    pub email: Option<String>,
    /// `subscriptionType`（team / pro / max …），作为套餐展示。
    pub subscription: Option<String>,
}

/// 解析 `claude auth status --json` 的 stdout。未登录时 CLI 以退出码 1 结束但 JSON 仍
/// 合法，所以调用方必须先解析 stdout 再看退出码。2.1.x 经 Ink 渲染输出，非 TTY 下按 80 列
/// 换行可能把长路径行折断、前后也可能夹杂非 JSON 行：整段解析失败时退回到「首个 `{` 到
/// 末个 `}` 之间去掉换行与缩进」再解析，仍失败则只按键提取需要的三个字段。缺少 `loggedIn`
/// 布尔值时视为不可解析。
pub(super) fn claude_auth_status(text: &str) -> Option<ClaudeAuthStatus> {
    let value = lenient_json_object(text)?;
    let logged_in = value.get("loggedIn")?.as_bool()?;
    let text_field = |name: &str, limit: usize| {
        value
            .get(name)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty() && s.len() <= limit && !s.chars().any(char::is_control))
            .map(str::to_owned)
    };
    Some(ClaudeAuthStatus {
        logged_in,
        email: text_field("email", 256).map(|email| email.to_ascii_lowercase()),
        subscription: text_field("subscriptionType", 64),
    })
}

/// 宽松地从 CLI 输出里取出一个 JSON 对象：整段 → 去掉行结构的 `{…}` 片段 → 逐键提取。
fn lenient_json_object(text: &str) -> Option<Value> {
    let trimmed = text.trim();
    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        if value.is_object() {
            return Some(value);
        }
    }
    let start = trimmed.find('{')?;
    let end = trimmed.rfind('}')?;
    if end <= start {
        return None;
    }
    let body = &trimmed[start..=end];
    // JSON 词法上行间空白无意义；被折断的字符串值去掉换行与缩进后重新接上。
    let joined = body.lines().map(str::trim).collect::<Vec<_>>().concat();
    if let Ok(value) = serde_json::from_str::<Value>(&joined) {
        if value.is_object() {
            return Some(value);
        }
    }
    let mut object = serde_json::Map::new();
    let logged_in = regex::Regex::new(r#""loggedIn"\s*:\s*(true|false)"#).ok()?;
    let flag = logged_in.captures(&joined)?.get(1)?.as_str() == "true";
    object.insert("loggedIn".into(), Value::Bool(flag));
    for key in ["email", "subscriptionType"] {
        let pattern = regex::Regex::new(&format!(r#""{key}"\s*:\s*"([^"]*)""#)).ok()?;
        if let Some(found) = pattern.captures(&joined).and_then(|c| c.get(1)) {
            object.insert(key.into(), Value::String(found.as_str().to_owned()));
        }
    }
    Some(Value::Object(object))
}

/// 交互探测画面上阻塞探测的官方对话框分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ProbeBlocker {
    /// 登录 / 选择登录方式：账号未登录，属于终态。
    SignIn,
    /// 目录信任确认：需用户在 CLI 中确认一次，探测可重试。
    Trust,
}

/// 登录组关键字（小写、行首词边界锚定）。前半是 claude 2.x 登录画面的标题 / 提示行，后半是
/// 通用措辞（CLI 改版后的兜底）。以 `/` 开头的斜杠命令行（`/login  Sign in with …`）在判定前被整行跳过，所以这里可以
/// 保留 `sign in` / `log in` 这类短语而不误判 `/help` 列表。
const SIGN_IN_LINE_PREFIXES: &[&str] = &[
    "select login method",
    "how do you want to sign in",
    "log in to claude",
    "sign in to claude",
    "please run /login",
    "run /login",
    "sign in",
    "log in",
    "log into",
    "login",
    "please sign in",
    "please log in",
    "you must sign in",
    "you must log in",
    "you need to sign in",
    "you need to log in",
    "not logged in",
    "not authenticated",
    "login required",
    "authentication required",
    "session expired",
    "your session has expired",
    "选择登录",
    "请登录",
    "未登录",
];

/// 信任组关键字（小写、行首词边界锚定），只锚定信任对话专属文案：Claude Code 2.x 的
/// `Quick safety check` / `Yes, I trust this folder` / `Yes, trust it`。`Accessing workspace:`
/// 是工作区横幅、`permission required` 是通用措辞，都不能单独作为判据。
const TRUST_LINE_PREFIXES: &[&str] = &[
    "quick safety check",
    "yes, i trust this folder",
    "yes, trust it",
];

/// 去掉一行前面的边框、项目符号、单选标记、光标与 `1.` / `2)` 之类的选项序号，只保留文案本体。
fn strip_line_decoration(line: &str) -> &str {
    let line =
        line.trim_start_matches(|ch: char| ch.is_whitespace() || "│┃├└─•*>❯●○◉◯".contains(ch));
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 && digits <= 2 {
        if let Some(rest) = line[digits..]
            .strip_prefix('.')
            .or_else(|| line[digits..].strip_prefix(')'))
        {
            return rest.trim_start();
        }
    }
    line
}

/// 行首词边界匹配：`prefix` 之后必须是行尾或非字母数字（`log in` 不命中 `log into`，
/// `login` 不命中 `logins`）；中文短语没有词边界概念，命中即算。
fn starts_with_phrase(line: &str, prefix: &str) -> bool {
    let Some(rest) = line.strip_prefix(prefix) else {
        return false;
    };
    if !prefix.ends_with(|ch: char| ch.is_ascii_alphanumeric()) {
        return true;
    }
    rest.chars().next().is_none_or(|ch| !ch.is_alphanumeric())
}

/// 逐行判定画面是否停在登录或信任对话框上：行级词边界前缀匹配，斜杠命令行整行跳过，
/// 两组同时出现时以信任为准（信任对话可重试、不会把账号钉进「需要登录」终态）。返回
/// `None` 表示没有阻塞对话。
pub(super) fn interactive_blocker(screen: &str) -> Option<ProbeBlocker> {
    let mut verdict = None;
    for line in screen.lines() {
        let plain = strip_line_decoration(line).to_lowercase();
        if plain.is_empty() || plain.starts_with('/') {
            continue;
        }
        if TRUST_LINE_PREFIXES
            .iter()
            .any(|prefix| starts_with_phrase(&plain, prefix))
        {
            return Some(ProbeBlocker::Trust);
        }
        if verdict.is_none()
            && SIGN_IN_LINE_PREFIXES
                .iter()
                .any(|prefix| starts_with_phrase(&plain, prefix))
        {
            verdict = Some(ProbeBlocker::SignIn);
        }
    }
    verdict
}

/// 非交互输出（stderr / stdout）里是否有登录失败证据：短错误文本，子串匹配即可。
pub(super) fn auth_evidence(text: &str) -> bool {
    let lower = text.to_lowercase();
    [
        "not logged in",
        "login required",
        "please log in",
        "not authenticated",
        "authentication required",
        "unauthorized",
        "invalid api key",
        "run /login",
        "auth login",
        "请登录",
        "未登录",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

/// 非交互输出里是否是命令行用法错误（旧版 CLI 缺子命令或 flag）：commander / yargs 族的
/// `unknown command` / `unknown option` / `unrecognized` 等措辞。
pub(super) fn cli_usage_error(text: &str) -> bool {
    let lower = text.to_lowercase();
    [
        "unknown command",
        "unknown option",
        "unknown argument",
        // clap 4：`error: unexpected argument '--json' found`。
        "unexpected argument",
        "unrecognized",
        "too many arguments",
        "invalid option",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Claude Code 2.1.x 在未信任目录启动时的画面（文案取自二进制字符串）。
    const CLAUDE_TRUST_SCREEN: &str = "\
 ╭──────────────────────────────────────────────────────────────╮
 │ Accessing workspace: /tmp/herdr-usage-4242-1-1                 │
 │                                                                │
 │ Quick safety check: Is this a project you created or one you   │
 │ trust? (Like your own code, a well-known open source project,  │
 │ or work from your team). If not, take a moment to review what  │
 │ is in this folder.                                             │
 │                                                                │
 │ ❯ 1. Yes, I trust this folder                                  │
 │   2. No, exit                                                  │
 ╰──────────────────────────────────────────────────────────────╯
";

    /// 未登录时的欢迎画面。
    const CLAUDE_SIGN_IN_SCREEN: &str = "\
 Welcome to Claude Code

 Select login method:

 ❯ 1. Claude account with subscription
   2. Anthropic Console account
";

    /// `/help` 列表：命令说明里带 `Sign in` 字样，不是登录对话。
    const CLAUDE_HELP_SCREEN: &str = "\
 /login    Sign in with your Anthropic account
 /logout   Sign out from your Anthropic account
 /usage    Show plan usage limits

 ❯
";

    #[test]
    fn claude_auth_status_parses_signed_in_and_signed_out_json() {
        let signed_in = claude_auth_status(
            "{\"loggedIn\":true,\"authMethod\":\"claude.ai\",\"email\":\"Me@Example.test\",\"orgId\":\"o\",\"subscriptionType\":\"team\"}\n",
        )
        .expect("已登录 JSON");
        assert!(signed_in.logged_in);
        assert_eq!(signed_in.email.as_deref(), Some("me@example.test"));
        assert_eq!(signed_in.subscription.as_deref(), Some("team"));

        // 未登录：CLI 退出码为 1，但 stdout 仍是合法 JSON，必须先解析。
        let signed_out = claude_auth_status("{\"loggedIn\":false,\"authMethod\":\"none\"}")
            .expect("未登录 JSON");
        assert!(!signed_out.logged_in);
        assert_eq!(signed_out.email, None);
        assert_eq!(signed_out.subscription, None);

        assert_eq!(claude_auth_status(""), None);
        assert_eq!(claude_auth_status("Not logged in"), None);
        assert_eq!(
            claude_auth_status("{\"email\":\"x\"}"),
            None,
            "缺 loggedIn 视为不可解析"
        );
        let control = claude_auth_status("{\"loggedIn\":true,\"email\":\"a\\u0007b\"}").unwrap();
        assert_eq!(control.email, None, "控制字符不作为身份");
    }

    #[test]
    fn claude_auth_status_survives_ink_wrapping_and_surrounding_lines() {
        // Ink 非 TTY 渲染：多行缩进 JSON、长路径行按 80 列折断、前后夹杂提示行。
        let wrapped = "\
Checking auth status...
{
  \"loggedIn\": true,
  \"authMethod\": \"claude.ai\",
  \"apiProvider\": \"firstParty\",
  \"configDirectory\": \"/home/someone-with-a-really-long-user-name/.config/claude-code-profil
es/work/.claude\",
  \"email\": \"Me@Example.test\",
  \"subscriptionType\": \"max\"
}
Done.
";
        let status = claude_auth_status(wrapped).expect("折行 JSON 仍可解析");
        assert!(status.logged_in);
        assert_eq!(status.email.as_deref(), Some("me@example.test"));
        assert_eq!(status.subscription.as_deref(), Some("max"));
        // 折断落在结构位置（引号被拆开）时按键提取仍能拿到登录判定。
        let broken = "{\n  \"loggedIn\": false,\n  \"configDirectory\": \"/x\n\"y\",\n  \"email\": \"a@b.test\"\n}";
        let status = claude_auth_status(broken).expect("逐键提取");
        assert!(!status.logged_in);
        assert_eq!(status.email.as_deref(), Some("a@b.test"));
        assert_eq!(claude_auth_status("prefix { not json } suffix"), None);
    }

    #[test]
    fn trust_dialog_is_retryable_and_sign_in_dialog_is_terminal() {
        assert_eq!(
            interactive_blocker(CLAUDE_TRUST_SCREEN),
            Some(ProbeBlocker::Trust)
        );
        assert_eq!(
            interactive_blocker(CLAUDE_SIGN_IN_SCREEN),
            Some(ProbeBlocker::SignIn)
        );
        // 只剩选项行（对话头部已滚出视口）也能识别，且序号与光标不影响判定。
        assert_eq!(
            interactive_blocker("   2) Yes, trust it\n"),
            Some(ProbeBlocker::Trust)
        );
        // 两组同时可见时以可重试的信任态为准。
        let both = format!("{CLAUDE_SIGN_IN_SCREEN}\n{CLAUDE_TRUST_SCREEN}");
        assert_eq!(interactive_blocker(&both), Some(ProbeBlocker::Trust));
        // 工作区横幅单独出现不算信任对话（已信任目录的正常启动也可能打印它）。
        assert_eq!(
            interactive_blocker(" Accessing workspace: /home/me/project\n ❯\n"),
            None
        );
        // 通用「permission required」不是目录信任对话。
        assert_eq!(
            interactive_blocker(" Permission required: allow tool Bash?\n"),
            None
        );
    }

    /// 通用登录措辞是 CLI 改版后的兜底：不依赖某一版的确切标题行。
    #[test]
    fn generic_sign_in_phrases_are_recognized() {
        for line in [
            "Please sign in to continue",
            "You need to sign in",
            "Authentication required",
            "You must log in first",
            "Login required: run `claude auth login`",
            "Log in to your account",
            "Not authenticated. Run `claude auth login` to continue.",
        ] {
            assert_eq!(
                interactive_blocker(line),
                Some(ProbeBlocker::SignIn),
                "{line}"
            );
        }
    }

    #[test]
    fn help_listing_and_idle_prompt_are_not_mistaken_for_login_dialogs() {
        assert_eq!(interactive_blocker(CLAUDE_HELP_SCREEN), None);
        assert_eq!(interactive_blocker(""), None);
        assert_eq!(interactive_blocker(" ❯ \n"), None);
        // 行中间出现的裸子串不算：只有行首锚定才命中。
        assert_eq!(
            interactive_blocker("Tip: run /login to sign in with another account\n"),
            None
        );
        assert_eq!(
            interactive_blocker("Some text about trust this folder later in a sentence\n"),
            None
        );
        // 词边界：已登录状态行与相近单词不命中。
        for line in [
            "Logged in as me@example.test",
            "Logins today: 3",
            "Logbook: 3 entries",
            "Signing in… please wait",
            "> Type your message or @path/to/file",
        ] {
            assert_eq!(interactive_blocker(line), None, "{line}");
        }
    }

    #[test]
    fn auth_evidence_matches_login_errors_only() {
        assert!(auth_evidence(
            "Error: Not logged in. Run claude auth login to authenticate."
        ));
        assert!(auth_evidence("HTTP 401 Unauthorized"));
        assert!(!auth_evidence("Unknown argument: --json"));
        assert!(!auth_evidence(""));
        // 用法错误：旧版 CLI 缺子命令 / flag。
        assert!(cli_usage_error("error: unknown command 'status'"));
        assert!(cli_usage_error("error: unknown option '--json'"));
        assert!(cli_usage_error("Unrecognized arguments: --json"));
        assert!(
            cli_usage_error("error: unexpected argument '--json' found"),
            "clap 4 措辞"
        );
        assert!(!cli_usage_error("Not logged in"));
        assert!(!cli_usage_error(""));
    }

    #[test]
    fn producers_strip_control_characters_so_dirty_lines_still_validate() {
        // 屏幕 / CLI 行：制表符对齐 + 未被尊重的 NO_COLOR（ANSI）。
        let screen_text = "\x1b[1mWeekly\x1b[0m\t20% used\nCredits used\t30/100 credits\n";
        let metrics = screen(screen_text, "account");
        assert_eq!(metrics.len(), 2, "{metrics:#?}");
        assert!(validate(&metrics), "清洗后整份通过：{metrics:#?}");
        assert_eq!(metrics[0].label, "Weekly 20% used");
        assert_eq!(metrics[0].used_percent, Some(20.0));
        assert_eq!(metrics[1].label, "Credits used 30/100 credits");
        assert_eq!(metrics[1].limit, Some(100.0));
        // 厂商 JSON：label 与对象键带制表符 / 转义。
        let value = json!({
            "buckets": {"week\tly": {"label": "Week\x1bly\tquota", "usedPercent": 40, "unit": "%\t"}},
            "input\tTokens": 12,
            "totalCost": "1.5"
        });
        let metrics = structured(&value, "session");
        assert!(validate(&metrics), "{metrics:#?}");
        assert!(metrics.iter().all(|metric| !metric.id.contains('\t')
            && !metric.label.contains('\t')
            && !metric.unit.contains('\t')));
        let bucket = metrics
            .iter()
            .find(|metric| metric.used_percent == Some(40.0))
            .expect("窗口指标");
        assert_eq!(bucket.label, "Weekly quota");
        assert_eq!(bucket.id, "/buckets/week ly");
        assert_eq!(bucket.unit, "%");
        // codex 的 limitName 同样来自 JSON。
        let codex_metrics = codex(
            &json!({"rateLimitsByLimitId": {"co\tdex": {"limitName": "Co\x07dex", "primary": {"usedPercent": 5}, "credits": {"balance": "1.0"}}}}),
        );
        assert!(validate(&codex_metrics), "{codex_metrics:#?}");
        assert_eq!(codex_metrics[0].label, "Codex · 主要额度");
        assert_eq!(codex_metrics[1].id, "co dex/credits");
        // opencode 框线表带 ANSI。
        let stats = opencode_stats("│\x1b[1mSessions\x1b[0m\t\t41 │\n");
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].used, Some(41.0));
        assert!(validate(&stats));
        // pi 扩展报文里的服务商 / 模型名。
        let pi_metrics =
            pi(&json!({"provider": "anth\tropic", "model": "son\x1bnet", "cost_usd": 0.5}));
        assert!(validate(&pi_metrics), "{pi_metrics:#?}");
        assert_eq!(
            pi_metrics
                .last()
                .and_then(|metric| metric.text_value.as_deref()),
            Some("anth ropic/sonnet")
        );
        assert_eq!(clean_field("  a \t\x1b[31m b\r\n c  "), "a b c");
        assert_eq!(clean_field("\t"), "");
    }

    #[test]
    fn retain_valid_drops_only_the_dirty_metrics() {
        let clean = UsageMetric {
            id: "cli-0".into(),
            label: "Weekly 20% used".into(),
            unit: "%".into(),
            scope: "account".into(),
            used_percent: Some(20.0),
            ..Default::default()
        };
        let mut dirty = clean.clone();
        dirty.id = "cli-1".into();
        dirty.label = "Daily\t50%".into();
        let mut nan = clean.clone();
        nan.id = "cli-2".into();
        nan.used_percent = Some(f64::NAN);
        let mut metrics = vec![clean.clone(), dirty, nan];
        assert_eq!(retain_valid(&mut metrics), 2, "只丢两条脏指标");
        assert_eq!(metrics, vec![clean.clone()]);
        // 全部合规时不动；超过上限时截断并计入丢弃数。
        let mut metrics = vec![clean.clone(); 3];
        assert_eq!(retain_valid(&mut metrics), 0);
        assert_eq!(metrics.len(), 3);
        let mut metrics = vec![clean; MAX_METRICS + 5];
        assert_eq!(retain_valid(&mut metrics), 5);
        assert_eq!(metrics.len(), MAX_METRICS);
        assert!(validate(&metrics));
        // 全脏 → 空。
        let mut metrics = vec![UsageMetric {
            unit: "x".repeat(65),
            ..Default::default()
        }];
        assert_eq!(retain_valid(&mut metrics), 1);
        assert!(metrics.is_empty());
    }

    /// opencode 1.17.20 `stats`（无 `--json`）的真实输出节选（NO_COLOR=1）。
    const OPENCODE_STATS_SCREEN: &str = "\
┌────────────────────────────────────────────────────────┐
│                       OVERVIEW                         │
├────────────────────────────────────────────────────────┤
│Sessions                                             41 │
│Messages                                          1,323 │
│Days                                                100 │
└────────────────────────────────────────────────────────┘

┌────────────────────────────────────────────────────────┐
│                    COST & TOKENS                       │
├────────────────────────────────────────────────────────┤
│Total Cost                                        $0.88 │
│Avg Cost/Day                                      $0.01 │
│Avg Tokens/Session                                 3.6M │
│Median Tokens/Session                            690.3K │
│Input                                             11.9M │
│Output                                           631.5K │
│Cache Read                                       132.9M │
│Cache Write                                      267.9K │
└────────────────────────────────────────────────────────┘


┌────────────────────────────────────────────────────────┐
│                      TOOL USAGE                        │
├────────────────────────────────────────────────────────┤
│ bash               ████████████████████ 1308 (58.8%)   │
│ read               ██████               435 (19.6%)    │
│ filesystem_edit_.. █                     19 ( 0.9%)    │
└────────────────────────────────────────────────────────┘
";

    #[test]
    fn opencode_stats_table_becomes_local_session_metrics() {
        let metrics = opencode_stats(OPENCODE_STATS_SCREEN);
        assert_eq!(metrics.len(), 11, "{metrics:#?}");
        assert!(validate(&metrics));
        assert!(
            metrics.iter().all(|metric| metric.scope == "local"),
            "本地会话统计不是账号额度：分区信号是结构化的 scope"
        );
        assert!(
            metrics
                .iter()
                .all(|metric| !metric.label.contains("本地会话统计")),
            "label 只留指标本体，分区标题由客户端按 scope 渲染"
        );
        let by_id = |id: &str| {
            metrics
                .iter()
                .find(|metric| metric.id == id)
                .unwrap_or_else(|| panic!("缺 {id}"))
        };
        assert_eq!(by_id("sessions").used, Some(41.0));
        assert_eq!(by_id("sessions").label, "会话数");
        assert_eq!(by_id("messages").used, Some(1323.0), "千分位逗号");
        assert_eq!(by_id("days").used, Some(100.0));
        let cost = by_id("total_cost");
        assert_eq!(cost.amount_decimal.as_deref(), Some("0.88"), "金额保留文本");
        assert_eq!(cost.unit, "USD");
        assert_eq!(cost.used, None);
        assert_eq!(
            by_id("avg_cost_per_day").amount_decimal.as_deref(),
            Some("0.01")
        );
        assert_eq!(
            by_id("avg_tokens_per_session").used,
            Some(3_600_000.0),
            "M 后缀"
        );
        assert_eq!(
            by_id("median_tokens_per_session").used,
            Some(690_300.0),
            "K 后缀"
        );
        assert_eq!(by_id("input_tokens").used, Some(11_900_000.0));
        assert_eq!(by_id("output_tokens").used, Some(631_500.0));
        assert_eq!(by_id("cache_read_tokens").used, Some(132_900_000.0));
        assert_eq!(by_id("cache_write_tokens").used, Some(267_900.0));
        assert_eq!(by_id("input_tokens").unit, "tokens");
        // 工具用量表与百分比不是用量指标；通用画面解析也不应从中臆造账号额度。
        assert!(metrics.iter().all(|metric| !metric.id.contains("bash")));
        assert!(screen(OPENCODE_STATS_SCREEN, "local").is_empty());
        // 空输出 / 帮助文本 / 无关文本 → 空。
        assert!(opencode_stats("").is_empty());
        assert!(opencode_stats("Options:\n  -h, --help  show help\n").is_empty());
        assert!(
            opencode_stats("│Total Cost      n/a │").is_empty(),
            "非数字不臆造"
        );
        // 同名行只取第一次出现。
        let twice = "│Sessions   1 │\n│Sessions   2 │\n";
        let metrics = opencode_stats(twice);
        assert_eq!(metrics.len(), 1);
        assert_eq!(metrics[0].used, Some(1.0));
        // 金额行用紧凑记数：按乘算后的数值格式化，不能把剥掉后缀的 `1.2` 当成金额。
        let compact = "│Total Cost      $1.2K │\n│Avg Cost/Day   $1.3M │\n";
        let metrics = opencode_stats(compact);
        assert_eq!(metrics.len(), 2);
        assert_eq!(metrics[0].amount_decimal.as_deref(), Some("1200.00"));
        assert_eq!(metrics[1].amount_decimal.as_deref(), Some("1300000.00"));
        assert!(validate(&metrics));
    }

    #[test]
    fn display_numbers_accept_separators_suffixes_and_currency() {
        assert_eq!(display_number("1,323"), Some((1323.0, Some("1323".into()))));
        assert_eq!(display_number("$0.88"), Some((0.88, Some("0.88".into()))));
        assert_eq!(display_number("41"), Some((41.0, Some("41".into()))));
        // 带倍率后缀：数值已乘算，原始文本不再提供（`3.6` 不是 3.6M 的金额）。
        assert_eq!(display_number("3.6M"), Some((3_600_000.0, None)));
        assert_eq!(display_number("690.3K"), Some((690_300.0, None)));
        assert_eq!(display_number("2B"), Some((2_000_000_000.0, None)));
        assert_eq!(display_number("$1.2K"), Some((1200.0, None)));
        for bad in ["", "$", "M", "n/a", "-1", "1.2.3M", "1e5", "--"] {
            assert_eq!(display_number(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn identity_sanitizer_and_help_detector_share_one_rule() {
        assert_eq!(
            sanitize_identity(Some("  Me@Example.test ")),
            Some("Me@Example.test".into()),
            "只去首尾空白，不改大小写（厂商 id 大小写有意义）"
        );
        assert_eq!(sanitize_identity(None), None);
        assert_eq!(sanitize_identity(Some("   ")), None);
        assert_eq!(sanitize_identity(Some("a\u{7}b")), None, "控制字符");
        assert_eq!(sanitize_identity(Some("a\tb")), None, "制表符也是控制字符");
        assert_eq!(
            sanitize_identity(Some(&"x".repeat(256))).map(|v| v.len()),
            Some(256)
        );
        assert_eq!(sanitize_identity(Some(&"x".repeat(257))), None);

        // yargs 在 flag 不受支持时只打印用法，没有 `unknown option` 字样。
        let yargs = "opencode stats\n\nshow token usage and cost statistics\n\nOptions:\n  -h, --help  show help  [boolean]\n";
        assert!(cli_help_output(yargs));
        assert!(cli_help_output(
            "Usage: tool profile [options]\n\n  -h, --help  display help\n"
        ));
        assert!(cli_help_output("Commands:\n  tool profile  show profile\n"));
        assert!(
            cli_help_output(
                "Error: unknown flag\nUsage:\n  tool profile [flags]\n\nFlags:\n  -h, --help\n"
            ),
            "cobra 形态"
        );
        assert!(!cli_help_output("Not logged in\n"));
        assert!(
            !cli_help_output("Unauthorized. Run `foo auth login --help` for details\n"),
            "错误提示里的 --help 不是帮助文本"
        );
        // `Usage:` 单独不成立：真实用量输出也会用它作小标题。
        assert!(!cli_help_output("Usage: 1,234 / 10,000 credits\n"));
        assert!(
            !cli_help_output("Usage:\n- 5h window: 20%\n- weekly: 40%\n"),
            "列表符号 `- ` 不是 flag 行"
        );
        assert!(
            !cli_help_output("Usage: tool profile [options]\n"),
            "无 flag 行"
        );
        assert!(!cli_help_output(OPENCODE_STATS_SCREEN), "框线表不是帮助");
        assert!(!cli_help_output(""));
    }

    #[test]
    fn validate_rejects_control_characters_in_every_text_field() {
        let clean = UsageMetric {
            id: "cli-0".into(),
            label: "Weekly 20% used".into(),
            unit: "%".into(),
            scope: "account".into(),
            used_percent: Some(20.0),
            ..Default::default()
        };
        assert!(validate(std::slice::from_ref(&clean)));
        for (field, value) in [
            ("id", "cli\u{1b}[0m"),
            ("label", "Weekly\u{7}20%"),
            ("unit", "%\r"),
            ("scope", "acc\u{0}ount"),
        ] {
            let mut metric = clean.clone();
            match field {
                "id" => metric.id = value.into(),
                "label" => metric.label = value.into(),
                "unit" => metric.unit = value.into(),
                _ => metric.scope = value.into(),
            }
            assert!(!validate(&[metric]), "{field} 含控制字符必须被拒");
        }
        // 制表符也是控制字符：终端画面里的对齐空白不能进入 label。
        let mut tab = clean.clone();
        tab.label = "Weekly\t20%".into();
        assert!(!validate(&[tab]));
    }

    #[test]
    fn codex_reset_credit_count_is_global_and_not_a_balance_or_percentage() {
        for details in [
            Value::Null,
            json!([]),
            json!([{"id":"one","status":"available"}]),
        ] {
            let metrics = codex(
                &json!({"rateLimitsByLimitId":{"codex":{"credits":{"balance":"12"}},"other":{"primary":{"usedPercent":50}}}, "rateLimitResetCredits":{"availableCount":3,"credits":details}}),
            );
            let reset: Vec<_> = metrics
                .iter()
                .filter(|metric| metric.id == "rate_limit_reset/available")
                .collect();
            assert_eq!(reset.len(), 1);
            assert_eq!(reset[0].remaining, Some(3.0));
            assert!(
                reset[0].limit.is_none()
                    && reset[0].used_percent.is_none()
                    && reset[0].used.is_none()
                    && reset[0].amount_decimal.is_none()
            );
            assert!(validate(&metrics));
        }
    }

    #[test]
    fn codex_reset_credit_count_preserves_zero_and_large_integers_and_ignores_invalid_counts() {
        for count in [json!(0), json!(1_i64 << 53), json!(i64::MAX)] {
            let metrics = codex(
                &json!({"ordinaryUsageAllowed":false,"rateLimitResetCredits":{"availableCount":count,"credits":null}}),
            );
            assert_eq!(metrics.len(), 1);
            assert!(validate(&metrics));
            let value = count.as_i64().unwrap();
            if value <= 1_i64 << 53 {
                assert_eq!(metrics[0].remaining, Some(value as f64));
            } else {
                assert!(metrics[0].remaining.is_none());
                assert_eq!(metrics[0].text_value, Some(format!("{value} resets")));
            }
            assert!(
                metrics[0].used.is_none()
                    && metrics[0].used_percent.is_none()
                    && metrics[0].limit.is_none()
            );
        }
        for count in [Value::Null, json!(-1), json!(1.5), json!("2"), json!(true)] {
            assert!(codex(&json!({"rateLimitResetCredits":{"availableCount":count,"credits":[{"status":"available"}]}})).is_empty());
        }
        for data in [
            json!({}),
            json!({"rateLimitResetCredits":null}),
            json!({"rateLimitResetCredits":{"credits":[{"status":"available"}]}}),
        ] {
            assert!(codex(&data).is_empty());
        }
    }

    #[test]
    fn codex_uses_named_buckets_once_and_preserves_reset_windows() {
        let bucket = json!({"primary":{"usedPercent":25,"windowDurationMins":300,"resetsAt":1700000000},"secondary":null});
        let data = json!({"rateLimits":bucket,"rateLimitsByLimitId":{"codex":bucket}});
        let metrics = codex(&data);
        assert_eq!(metrics.len(), 1);
        assert_eq!(metrics[0].used_percent, Some(25.0));
        assert_eq!(metrics[0].window_seconds, Some(18000));
        assert_eq!(metrics[0].resets_at, Some(1700000000));
    }

    #[test]
    fn codex_official_usage_preserves_summary_and_daily_units_without_quota_pressure() {
        let metrics = codex(&json!({"officialUsage": {
            "summary": {"lifetimeTokens":123456,"peakDailyTokens":9876,
                "longestRunningTurnSec":321,"currentStreakDays":0,"longestStreakDays":17,
                "futureField":999},
            "dailyUsageBuckets":[{"startDate":"2026-09-24","tokens":42},
                {"startDate":"2026-09-25","tokens":0}],
            "threadUsage":{"totalTokens":999999}
        }}));
        assert_eq!(metrics.len(), 7);
        for (id, unit, expected) in [
            ("usage/lifetime_tokens", "tokens", 123456.0),
            ("usage/peak_daily_tokens", "tokens", 9876.0),
            ("usage/longest_running_turn", "seconds", 321.0),
            ("usage/current_streak", "days", 0.0),
            ("usage/longest_streak", "days", 17.0),
            ("usage/daily/2026-09-25", "tokens", 0.0),
        ] {
            let metric = metrics.iter().find(|metric| metric.id == id).expect(id);
            assert_eq!(metric.unit, unit);
            assert_eq!(metric.used, Some(expected));
            assert_eq!(metric.scope, "account");
        }
        assert!(metrics
            .iter()
            .all(|metric| !counts_as_quota_pressure(metric)));
        assert!(validate(&metrics));
    }

    #[test]
    fn codex_official_usage_rejects_unknown_invalid_and_duplicate_daily_values() {
        assert!(
            codex(&json!({"officialUsage":{"summary":null,"dailyUsageBuckets":null}})).is_empty()
        );
        let metrics = codex(&json!({"officialUsage": {
            "summary":{"lifetimeTokens":null,"peakDailyTokens":-1,
                "longestRunningTurnSec":1.5,"currentStreakDays":"7","longestStreakDays":true},
            "dailyUsageBuckets":[{"startDate":"2026-02-30","tokens":1},
                {"startDate":"2026-09-24","tokens":-1},
                {"startDate":"2026-09-25","tokens":4},
                {"startDate":"2026-09-25","tokens":4},
                {"startDate":"2026-9-26","tokens":3},
                {"startDate":"2026-09-27","tokens":null}]
        }}));
        assert_eq!(metrics.len(), 1);
        assert_eq!(metrics[0].used, Some(4.0), "重复日期不累计");
        assert!(
            codex(&json!({"ordinaryUsageAllowed":false})).is_empty(),
            "权限不是额度数值，不能推断或伪造为指标"
        );
    }

    #[test]
    fn codex_official_daily_usage_is_bounded_recent_first_and_large_integers_stay_exact() {
        let start = time::Date::from_calendar_date(2026, time::Month::January, 1).expect("date");
        let buckets = (0..100)
            .map(|day| {
                json!({
                    "startDate":(start + time::Duration::days(day)).to_string(),"tokens":day
                })
            })
            .collect::<Vec<_>>();
        let metrics = codex(&json!({"officialUsage":{
            "summary":{"lifetimeTokens":i64::MAX}, "dailyUsageBuckets":buckets
        }}));
        assert_eq!(metrics.len(), 91);
        assert_eq!(metrics[0].used, None, "不经f64舍入官方大整数");
        assert_eq!(
            metrics[0].text_value.as_deref(),
            Some("9223372036854775807 tokens")
        );
        assert_eq!(metrics[1].id, "usage/daily/2026-04-10");
        assert_eq!(metrics[90].id, "usage/daily/2026-01-11");
        assert!(validate(&metrics));
    }

    /// 客户端把 `used_percent` 与成对的 `used` + `limit` 都当作账号额度压力：会话级指标不得
    /// 落进这两种形态。
    fn counts_as_quota_pressure(metric: &UsageMetric) -> bool {
        metric.used_percent.is_some() || (metric.used.is_some() && metric.limit.is_some())
    }

    #[test]
    fn missing_claude_quota_never_becomes_zero_and_context_is_never_account_quota() {
        let metrics = claude(&json!({"context_window":{"used_percentage":80}}));
        assert!(!metrics.is_empty(), "上下文作为会话级指标保留");
        assert!(
            metrics.iter().all(|metric| metric.scope == "session"),
            "没有 rate_limits 就没有账号额度指标：{metrics:#?}"
        );
        assert!(
            !metrics.iter().any(counts_as_quota_pressure),
            "上下文占用不是账号额度，不得进入额度压力口径：{metrics:#?}"
        );
        assert!(claude(&json!({})).is_empty());
        assert!(claude(&json!({"model":{"display_name":"Opus"}})).is_empty());
    }

    /// `spend_limit.used_percentage` 超限后可大于 100（官方文档：above 100 once you exceed the
    /// limit）：解析与校验都不得截断。
    #[test]
    fn claude_spend_limit_overage_is_not_truncated() {
        let mut metrics = claude(
            &json!({"rate_limits":{"spend_limit":{"used_percentage":162.8,"resets_at":1700000000}}}),
        );
        assert_eq!(metrics.len(), 1);
        assert_eq!(metrics[0].id, "spend_limit");
        assert_eq!(metrics[0].used_percent, Some(162.8));
        assert_eq!(metrics[0].resets_at, Some(1700000000));
        assert_eq!(retain_valid(&mut metrics), 0, "超过 100 的百分比是合规指标");
        assert_eq!(metrics[0].used_percent, Some(162.8));
    }

    /// 官方 statusline 的完整形态（字段取自 code.claude.com/docs/en/statusline 的示例）。
    #[test]
    fn claude_statusline_yields_quota_windows_then_session_cost_and_context() {
        let metrics = claude(&json!({
            "session_id": "abc",
            "cost": {
                "total_cost_usd": 0.01234,
                "total_duration_ms": 3_725_000,
                "total_api_duration_ms": 2300,
                "total_lines_added": 156,
                "total_lines_removed": 23
            },
            "context_window": {
                "total_input_tokens": 15234,
                "total_output_tokens": 4521,
                "context_window_size": 200000,
                "used_percentage": 8,
                "remaining_percentage": 92,
                "current_usage": {
                    "input_tokens": 8500,
                    "output_tokens": 1200,
                    "cache_creation_input_tokens": 5000,
                    "cache_read_input_tokens": 2000
                }
            },
            "rate_limits": {
                "five_hour": {"used_percentage": 23.5, "resets_at": 1738425600},
                "seven_day": {"used_percentage": 41.2, "resets_at": 1738857600}
            }
        }));
        assert!(validate(&metrics), "{metrics:#?}");
        let ids = metrics
            .iter()
            .map(|metric| metric.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            vec![
                "five_hour",
                "seven_day",
                "cost/total_cost_usd",
                "cost/total_duration_ms",
                "cost/total_api_duration_ms",
                "context_window/used_percentage",
                "context_window/current_usage",
                "context_window/context_window_size",
            ],
            "账号额度在前，会话级指标在后"
        );
        let by_id = |id: &str| metrics.iter().find(|metric| metric.id == id).unwrap();
        assert_eq!(by_id("five_hour").scope, "account");
        assert_eq!(by_id("five_hour").used_percent, Some(23.5));
        for metric in metrics.iter().skip(2) {
            assert_eq!(metric.scope, "session", "{}", metric.id);
            assert!(!counts_as_quota_pressure(metric), "{}", metric.id);
        }
        let cost = by_id("cost/total_cost_usd");
        assert_eq!(cost.amount_decimal.as_deref(), Some("0.01234"));
        assert_eq!(cost.unit, "USD");
        let duration = by_id("cost/total_duration_ms");
        assert_eq!(duration.used, Some(3_725_000.0));
        assert_eq!(duration.text_value.as_deref(), Some("1h02m"));
        assert_eq!(
            by_id("cost/total_api_duration_ms").text_value.as_deref(),
            Some("2s")
        );
        let percent = by_id("context_window/used_percentage");
        assert_eq!(percent.used, Some(8.0));
        assert_eq!(percent.remaining, Some(92.0));
        assert_eq!(percent.text_value.as_deref(), Some("8%"));
        let tokens = by_id("context_window/current_usage");
        assert_eq!(
            tokens.used,
            Some(15_500.0),
            "与官方 used_percentage 同口径：input + cache 创建 + cache 读取，不含 output"
        );
        assert_eq!(tokens.text_value, None);
        assert_eq!(
            by_id("context_window/context_window_size").used,
            Some(200_000.0)
        );
    }

    /// `current_usage` / `used_percentage` 为 null（首次 API 调用前、`/compact` 后）是「未知」，
    /// 必须与真实的 0 区分：数值留 `None` 并给出明示，而不是 `Some(0.0)`。
    #[test]
    fn claude_null_context_is_unknown_not_zero() {
        let pending = claude(&json!({"context_window": {
            "context_window_size": 200000,
            "used_percentage": null,
            "remaining_percentage": null,
            "current_usage": null
        }}));
        let by_id = |metrics: &[UsageMetric], id: &str| {
            metrics
                .iter()
                .find(|metric| metric.id == id)
                .cloned()
                .unwrap()
        };
        let percent = by_id(&pending, "context_window/used_percentage");
        assert_eq!(percent.used, None);
        assert_eq!(percent.remaining, None);
        assert_eq!(
            percent.text_value.as_deref(),
            Some(claude_context_pending_text())
        );
        let tokens = by_id(&pending, "context_window/current_usage");
        assert_eq!(tokens.used, None);
        assert_eq!(
            tokens.text_value.as_deref(),
            Some(claude_context_pending_text())
        );
        assert_eq!(
            by_id(&pending, "context_window/context_window_size").used,
            Some(200_000.0),
            "窗口大小已知，照常给出"
        );
        assert!(validate(&pending));

        let zero = claude(&json!({"context_window": {
            "used_percentage": 0,
            "remaining_percentage": 100,
            "current_usage": {"input_tokens": 0, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0}
        }}));
        let percent = by_id(&zero, "context_window/used_percentage");
        assert_eq!(percent.used, Some(0.0), "真实的 0 保留为 0");
        assert_eq!(percent.text_value.as_deref(), Some("0%"));
        let tokens = by_id(&zero, "context_window/current_usage");
        assert_eq!(tokens.used, Some(0.0));
        assert_eq!(tokens.text_value, None);
        assert_ne!(
            by_id(&pending, "context_window/current_usage"),
            tokens,
            "未知与 0 不是同一个指标值"
        );
    }

    fn claude_window(id: &str, percent: f64, resets_at: u64) -> UsageMetric {
        UsageMetric {
            id: id.into(),
            label: id.into(),
            unit: "%".into(),
            scope: "account".into(),
            used_percent: Some(percent),
            resets_at: Some(resets_at),
            ..Default::default()
        }
    }

    /// 官方 statusline 在窗口过了 `resets_at` 后把它从 JSON 里去掉：缺席不是归零，上次值沿用
    /// 并标为过期；尚未到 `resets_at` 的缺席（会话首个响应之前）原样沿用、不标过期。
    #[test]
    fn claude_window_that_disappears_is_retained_not_zeroed() {
        let now = 1_700_010_000;
        let previous = vec![
            claude_window("five_hour", 87.5, now - 60),
            claude_window("seven_day", 41.0, now + 86_400),
            UsageMetric {
                id: "cost/total_cost_usd".into(),
                scope: "session".into(),
                unit: "USD".into(),
                amount_decimal: Some("9.99".into()),
                ..Default::default()
            },
        ];
        // five_hour 已过重置时间，从报文里消失；seven_day 仍在。
        let fresh = claude(&json!({
            "cost": {"total_cost_usd": 1.5},
            "rate_limits": {"seven_day": {"used_percentage": 42.0, "resets_at": now + 86_400}}
        }));
        let unseen = HashMap::new();
        let merged = claude_retain_missing_windows(fresh, &previous, &unseen, now);
        let ids = merged
            .iter()
            .map(|metric| metric.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            vec!["five_hour", "seven_day", "cost/total_cost_usd"],
            "窗口按固定顺序排在会话级指标之前；会话级指标只取本次报文"
        );
        let five_hour = &merged[0];
        assert_eq!(five_hour.used_percent, Some(87.5), "保留上次值，不是 0");
        assert_eq!(five_hour.resets_at, Some(now - 60));
        assert_eq!(
            five_hour.text_value.as_deref(),
            Some(claude_stale_window_text())
        );
        assert_eq!(merged[1].used_percent, Some(42.0), "在场的窗口用本次报文");
        assert_eq!(merged[1].text_value, None);
        assert_eq!(merged[2].amount_decimal.as_deref(), Some("1.5"));
        assert!(validate(&merged));

        // 过期标记幂等：再缺席一次仍是同一条过期指标。
        let again = claude_retain_missing_windows(
            claude(&json!({"cost": {"total_cost_usd": 1.6}})),
            &merged,
            &unseen,
            now + 30,
        );
        assert_eq!(again[0], merged[0]);
        assert_eq!(again[1].used_percent, Some(42.0));
        assert_eq!(
            again[1].text_value, None,
            "尚未到 resets_at 的缺席（rate_limits 整段缺省）不算过期"
        );

        // 新窗口的数据回来后取代过期值。
        let recovered = claude_retain_missing_windows(
            claude(
                &json!({"rate_limits": {"five_hour": {"used_percentage": 1.0, "resets_at": now + 18_000}}}),
            ),
            &again,
            &unseen,
            now + 60,
        );
        assert_eq!(recovered[0].used_percent, Some(1.0));
        assert_eq!(recovered[0].text_value, None);

        // 过期太久的上次值不再沿用。
        let ancient = claude_retain_missing_windows(
            Vec::new(),
            &[claude_window(
                "five_hour",
                87.5,
                now - CLAUDE_STALE_WINDOW_MAX_AGE_SECS - 1,
            )],
            &unseen,
            now,
        );
        assert!(ancient.is_empty());
    }

    /// 没有 `resets_at` 的窗口无从判断是否已重置：沿用按最近一次出现在报文里的时刻限期，
    /// 不能被无限期沿用；出现时刻未知的不沿用。
    #[test]
    fn claude_window_without_resets_at_is_retained_only_within_the_max_age() {
        let now = 1_700_010_000;
        let previous = vec![UsageMetric {
            resets_at: None,
            ..claude_window("spend_limit", 162.8, 0)
        }];
        let fresh = || {
            claude(&json!({
                "rate_limits": {"five_hour": {"used_percentage": 5.0, "resets_at": now + 3600}}
            }))
        };
        let ids = |metrics: &[UsageMetric]| {
            metrics
                .iter()
                .map(|metric| metric.id.clone())
                .collect::<Vec<_>>()
        };

        let seen = HashMap::from([("spend_limit".to_string(), now - 60)]);
        let kept = claude_retain_missing_windows(fresh(), &previous, &seen, now);
        assert_eq!(ids(&kept), vec!["five_hour", "spend_limit"]);
        assert_eq!(kept[1].used_percent, Some(162.8), "上限内原样沿用");
        assert_eq!(
            kept[1].text_value, None,
            "没有 resets_at 就不能宣称已过重置时间"
        );

        let at_limit = HashMap::from([(
            "spend_limit".to_string(),
            now - CLAUDE_STALE_WINDOW_MAX_AGE_SECS,
        )]);
        assert_eq!(
            ids(&claude_retain_missing_windows(
                fresh(),
                &previous,
                &at_limit,
                now
            )),
            vec!["five_hour", "spend_limit"],
            "恰好到上限仍沿用"
        );

        let expired = HashMap::from([(
            "spend_limit".to_string(),
            now - CLAUDE_STALE_WINDOW_MAX_AGE_SECS - 1,
        )]);
        assert_eq!(
            ids(&claude_retain_missing_windows(
                fresh(),
                &previous,
                &expired,
                now
            )),
            vec!["five_hour"],
            "超过上限不再沿用"
        );
        assert_eq!(
            ids(&claude_retain_missing_windows(
                fresh(),
                &previous,
                &HashMap::new(),
                now
            )),
            vec!["five_hour"],
            "出现时刻未知的不沿用"
        );
    }

    #[test]
    fn cli_context_is_not_mistaken_for_account_quota() {
        let values = screen(
            "Context 70%\nWeekly 20% used\n5h 70% remaining\nCredits used 30/100 credits",
            "account",
        );
        assert_eq!(values.len(), 3);
        assert_eq!(values[1].used_percent, Some(30.0));
        assert_eq!(values[2].limit, Some(100.0));
    }

    #[test]
    fn remaining_before_or_after_number_is_never_reported_as_used() {
        for line in [
            "Remaining: 80%",
            "80% of quota remaining",
            "剩余 80%",
            "80/100 credits remaining",
        ] {
            let metrics = screen(line, "account");
            assert_eq!(metrics.len(), 1, "{line}");
            assert_eq!(metrics[0].used_percent, Some(20.0), "{line}");
        }
        assert!(screen("Weekly 80%", "account").is_empty());
    }

    #[test]
    fn balances_preserve_decimal_units_and_key_scope() {
        let metrics =
            openrouter(&json!({"data":{"usage_monthly":"12.000001","limit_remaining":null}}));
        assert_eq!(metrics.len(), 1);
        assert_eq!(metrics[0].amount_decimal.as_deref(), Some("12.000001"));
        assert_eq!(metrics[0].scope, "api_key");
    }

    #[test]
    fn moonshot_regions_keep_currency_and_debt() {
        let balance = json!({"data":{"available_balance":"12.123456","cash_balance":"-0.025","voucher_balance":3}});
        for currency in ["CNY", "USD"] {
            let metrics = moonshot_balance(&balance, currency);
            assert_eq!(metrics.len(), 3);
            assert!(metrics.iter().all(|metric| metric.unit == currency));
            assert_eq!(
                metrics
                    .iter()
                    .find(|metric| metric.id == "cash_balance")
                    .unwrap()
                    .amount_decimal
                    .as_deref(),
                Some("-0.025")
            );
        }
    }

    #[test]
    fn kimi_v2_quota_usages_windows_become_percent_metrics() {
        let values = kimi(&json!({
            "code": 0,
            "data": {
                "kind": "ok",
                "quota": {
                    "usages": {
                        "limit5h": {"usedRatio": 0.25, "resetAt": "2026-09-18T10:50:54Z"},
                        "limit7d": {"usedRatio": 0.665726, "resetAt": "2026-09-23T13:50:55Z"}
                    },
                    "extraUsage": null
                }
            }
        }));
        assert_eq!(values.len(), 2);
        let five_hours = values
            .iter()
            .find(|metric| metric.id == "limit5h")
            .expect("5h window");
        assert_eq!(five_hours.used_percent, Some(25.0));
        assert!(five_hours.resets_at.is_some());
        let seven_days = values
            .iter()
            .find(|metric| metric.id == "limit7d")
            .expect("7d window");
        assert!((seven_days.used_percent.unwrap() - 66.5726).abs() < 0.01);
    }

    /// 2.0.2 的 `managedQuotaUsagesSchema` 还有 monthTotal / monthCode 两个窗口：必须有中文
    /// 标签，不能把裸键名显示出来。
    #[test]
    fn kimi_monthly_windows_are_labelled() {
        let values = kimi(&json!({"data": {"kind": "ok", "quota": {"usages": {
            "monthTotal": {"usedRatio": 0.5, "resetAt": "2026-10-01T00:00:00Z"},
            "monthCode": {"usedRatio": 0.125}
        }}}}));
        let label = |id: &str| {
            values
                .iter()
                .find(|metric| metric.id == id)
                .map(|metric| metric.label.clone())
                .unwrap()
        };
        assert_eq!(label("monthTotal"), "月度额度");
        assert_eq!(label("monthCode"), "月度额度 · Code 部分");
        assert!(values.iter().all(|metric| metric.label != metric.id));
        let percent = |id: &str| {
            values
                .iter()
                .find(|metric| metric.id == id)
                .and_then(|metric| metric.used_percent)
        };
        assert_eq!(percent("monthTotal"), Some(50.0));
        assert_eq!(percent("monthCode"), Some(12.5));
    }

    /// `quota.extraUsage`（超额按量的钱包）→ 金额类指标：单位是币种，分 → 主单位不经浮点，
    /// 不产出百分比；月上限只在开启且大于 0 时才是真实上限。
    #[test]
    fn kimi_extra_usage_becomes_amount_metrics_not_percentages() {
        let values = kimi(&json!({"data": {"kind": "ok", "quota": {
            "usages": {"limit5h": {"usedRatio": 0.25}},
            "extraUsage": {
                "balanceCents": 12345,
                "totalCents": 20000,
                "monthlyChargeLimitEnabled": true,
                "monthlyChargeLimitCents": 5000,
                "monthlyUsedCents": 705,
                "currency": "CNY"
            }
        }}}));
        assert!(validate(&values), "{values:#?}");
        let amount = |id: &str| {
            values
                .iter()
                .find(|metric| metric.id == id)
                .map(|metric| (metric.amount_decimal.clone().unwrap(), metric.unit.clone()))
        };
        assert_eq!(
            amount("extra_usage/balance"),
            Some(("123.45".into(), "CNY".into()))
        );
        assert_eq!(
            amount("extra_usage/total"),
            Some(("200.00".into(), "CNY".into()))
        );
        assert_eq!(
            amount("extra_usage/monthly_used"),
            Some(("7.05".into(), "CNY".into()))
        );
        assert_eq!(
            amount("extra_usage/monthly_limit"),
            Some(("50.00".into(), "CNY".into()))
        );
        for metric in values
            .iter()
            .filter(|metric| metric.id.starts_with("extra_usage/"))
        {
            assert_eq!(metric.scope, "account");
            assert!(
                metric.used_percent.is_none() && metric.used.is_none() && metric.limit.is_none(),
                "金额类指标不折算百分比：{metric:#?}"
            );
        }

        // 上限未开启或为 0：不是真实上限，不展示。
        for wallet in [
            json!({"balanceCents": 1, "monthlyChargeLimitEnabled": false, "monthlyChargeLimitCents": 5000}),
            json!({"balanceCents": 1, "monthlyChargeLimitEnabled": true, "monthlyChargeLimitCents": 0}),
        ] {
            let values = kimi(&json!({"quota": {"usages": {}, "extraUsage": wallet}}));
            assert!(values
                .iter()
                .all(|metric| metric.id != "extra_usage/monthly_limit"));
            assert_eq!(values[0].unit, "USD", "缺 currency 按官方实现的缺省");
        }
    }

    /// server API 官方标注 experimental：extraUsage 为 null、缺字段或形状不符都只跳过，不影响
    /// 额度窗口。
    #[test]
    fn kimi_extra_usage_tolerates_missing_and_malformed_fields() {
        for extra in [
            json!(null),
            json!("soon"),
            json!([]),
            json!({}),
            json!({"balanceCents": "12", "totalCents": -5, "monthlyUsedCents": 1.5, "currency": 7}),
        ] {
            let values = kimi(&json!({"quota": {
                "usages": {"limit7d": {"usedRatio": 0.5}},
                "extraUsage": extra
            }}));
            assert_eq!(values.len(), 1, "{values:#?}");
            assert_eq!(values[0].id, "limit7d");
        }
        let partial = kimi(&json!({"quota": {"extraUsage": {"monthlyUsedCents": 9}}}));
        assert_eq!(partial.len(), 1);
        assert_eq!(partial[0].id, "extra_usage/monthly_used");
        assert_eq!(partial[0].amount_decimal.as_deref(), Some("0.09"));
    }

    /// `opencode db <query> --format json` 打印 `JSON.stringify(rows, null, 2)`：单行聚合 →
    /// 本地会话统计，`scope` 固定 `local`（不是账号额度）。
    #[test]
    fn opencode_session_totals_become_local_session_metrics() {
        let text = "[\n  {\n    \"sessions\": 41,\n    \"child_sessions\": 7,\n    \"cost\": 0.8800000000000001,\n    \"tokens_input\": 690300,\n    \"tokens_output\": 52100,\n    \"tokens_reasoning\": 1200,\n    \"tokens_cache_read\": 3600000,\n    \"tokens_cache_write\": 0\n  }\n]\n";
        let metrics = opencode_sessions(text);
        assert!(validate(&metrics), "{metrics:#?}");
        assert!(
            metrics.iter().all(|metric| metric.scope == "local"),
            "本地统计，非账号额度"
        );
        assert!(metrics
            .iter()
            .all(|metric| metric.used_percent.is_none() && metric.limit.is_none()));
        let by_id = |id: &str| metrics.iter().find(|metric| metric.id == id).unwrap();
        assert_eq!(by_id("sessions").used, Some(41.0));
        assert_eq!(by_id("child_sessions").used, Some(7.0));
        assert_eq!(
            by_id("total_cost").amount_decimal.as_deref(),
            Some("0.8800")
        );
        assert_eq!(by_id("total_cost").unit, "USD");
        assert_eq!(by_id("input_tokens").used, Some(690_300.0));
        assert_eq!(by_id("reasoning_tokens").used, Some(1200.0));
        assert_eq!(by_id("cache_write_tokens").used, Some(0.0));
        // 与框线表回退形态共用 id，切换数据源后客户端看到的是同一组指标。
        for id in ["sessions", "total_cost", "input_tokens", "output_tokens"] {
            assert!(OPENCODE_STATS_ROWS.iter().any(|row| row.id == id), "{id}");
        }

        // JSON 前夹杂日志行、缺列、空结果、非 JSON 都不 panic。
        let noisy = format!("Performing one time database migration...\n{text}");
        assert_eq!(opencode_sessions(&noisy).len(), metrics.len());
        assert_eq!(opencode_sessions("[{\"sessions\": 3}]").len(), 1);
        assert!(opencode_sessions("[]").is_empty());
        assert!(opencode_sessions("{}").is_empty());
        assert!(opencode_sessions("sessions\tcost\n41\t0.88\n").is_empty());
        assert!(opencode_sessions("").is_empty());
    }

    /// herdr pi 扩展的推送报文 → 会话级指标，并带上当前 provider/model。
    #[test]
    fn pi_push_payload_becomes_session_metrics_with_the_current_model() {
        let payload = json!({
            "source": "herdr:pi",
            "provider": "anthropic",
            "model": "claude-sonnet-4-5",
            "context": {"tokens": 45_000, "percent": 22.5, "context_window": 200_000},
            "tokens": {"input": 1000, "output": 200, "cache_read": 3000, "cache_write": 40, "total": 4240},
            "cost_usd": 0.4213
        });
        let metrics = pi(&payload);
        assert!(validate(&metrics), "{metrics:#?}");
        assert!(metrics.iter().all(|metric| metric.scope == "session"));
        assert!(
            !metrics.iter().any(counts_as_quota_pressure),
            "会话统计不进账号额度压力口径"
        );
        let by_id = |id: &str| metrics.iter().find(|metric| metric.id == id).unwrap();
        assert_eq!(by_id("context/percent").used, Some(22.5));
        assert_eq!(
            by_id("context/percent").text_value.as_deref(),
            Some("22.5%")
        );
        assert_eq!(by_id("context/tokens").used, Some(45_000.0));
        assert_eq!(by_id("context/context_window").used, Some(200_000.0));
        assert_eq!(
            by_id("session/cost_usd").amount_decimal.as_deref(),
            Some("0.4213")
        );
        assert_eq!(by_id("session/tokens/cache_read").used, Some(3000.0));
        assert_eq!(by_id("session/tokens/total").used, Some(4240.0));
        assert_eq!(
            by_id("session/model").text_value.as_deref(),
            Some("anthropic/claude-sonnet-4-5")
        );
        assert_eq!(
            pi_provider_model(&payload),
            (Some("anthropic".into()), Some("claude-sonnet-4-5".into()))
        );
        // 没有任何用量字段时，单独一条模型名不算用量。
        assert!(pi(&json!({"provider": "anthropic", "model": "x"})).is_empty());
        assert!(pi(&json!({})).is_empty());
    }

    /// 压缩之后、下一次响应之前 `tokens` / `percent` 为 null：未知不是 0。
    #[test]
    fn pi_null_context_after_compaction_is_unknown_not_zero() {
        let pending =
            pi(&json!({"context": {"tokens": null, "percent": null, "context_window": 200_000}}));
        let by_id = |metrics: &[UsageMetric], id: &str| {
            metrics
                .iter()
                .find(|metric| metric.id == id)
                .cloned()
                .unwrap()
        };
        for id in ["context/percent", "context/tokens"] {
            let metric = by_id(&pending, id);
            assert_eq!(metric.used, None, "{id}");
            assert_eq!(
                metric.text_value.as_deref(),
                Some(pi_context_pending_text()),
                "{id}"
            );
        }
        assert_eq!(
            by_id(&pending, "context/context_window").used,
            Some(200_000.0)
        );
        assert!(validate(&pending));

        let zero = pi(&json!({"context": {"tokens": 0, "percent": 0, "context_window": 200_000}}));
        assert_eq!(by_id(&zero, "context/percent").used, Some(0.0));
        assert_eq!(
            by_id(&zero, "context/percent").text_value.as_deref(),
            Some("0%")
        );
        assert_eq!(by_id(&zero, "context/tokens").used, Some(0.0));
        assert_eq!(by_id(&zero, "context/tokens").text_value, None);
        assert_ne!(
            by_id(&pending, "context/tokens"),
            by_id(&zero, "context/tokens")
        );
    }

    /// pi 的会话条目里 `usage.cost` 是对象（`{input, output, cacheRead, cacheWrite, total}`），
    /// RPC `get_session_stats` 的 `cost` 是数值。服务端只认扩展合计好的数值 `cost_usd`：对象形态
    /// 不被当成数值，别的键名也不会被误读成费用。
    #[test]
    fn pi_cost_is_only_read_from_the_numeric_cost_usd_field() {
        let cost_of = |payload: Value| {
            pi(&payload)
                .into_iter()
                .find(|metric| metric.id == "session/cost_usd")
                .and_then(|metric| metric.amount_decimal)
        };
        let context = json!({"tokens": 1, "percent": 1, "context_window": 10});
        assert_eq!(
            cost_of(json!({"context": context, "cost_usd": 1.25})).as_deref(),
            Some("1.25")
        );
        assert_eq!(
            cost_of(json!({"context": context, "cost_usd": 0})).as_deref(),
            Some("0")
        );
        // 对象形态（会话条目口径）与字符串都不接受。
        for malformed in [
            json!({"input": 0.1, "output": 0.2, "cacheRead": 0.0, "cacheWrite": 0.0, "total": 0.3}),
            json!("1.25"),
            json!(null),
            json!(-1),
        ] {
            assert_eq!(
                cost_of(json!({"context": context, "cost_usd": malformed})),
                None,
                "{malformed}"
            );
        }
        // RPC / 会话条目的原始键名不是本报文的契约。
        assert_eq!(cost_of(json!({"context": context, "cost": 1.25})), None);
        assert_eq!(
            cost_of(json!({"context": context, "cost": {"total": 1.25}})),
            None
        );
    }

    /// 金额类指标原样渲染给用户，所以浮点合计的尾差必须在这里收口：6 位小数够细
    /// （比任何单次调用的单价都细），又不会把 `0.39999999999999997` 透出去。
    #[test]
    fn money_decimal_rounds_float_tails_but_keeps_vendor_strings() {
        // 0.36 + 0.04 在 IEEE754 里就是这个值：pi v9 及更早的已装扩展会原样推上来。
        assert_eq!(money_decimal(&json!(0.399_999_999_999_999_97)), "0.4");
        assert_eq!(money_decimal(&json!(0.1 + 0.2)), "0.3");
        assert_eq!(money_decimal(&json!(0)), "0");
        assert_eq!(money_decimal(&json!(0.0012)), "0.0012");
        assert_eq!(money_decimal(&json!(12)), "12");
        assert_eq!(money_decimal(&json!(1.5)), "1.5");
        // 六位以下的差额仍然保留，不会被抹成整数。
        assert_eq!(money_decimal(&json!(0.000_001)), "0.000001");
        // 厂商给的字符串是它自己的精确十进制表示，不做浮点往返。
        assert_eq!(money_decimal(&json!("12.3400")), "12.3400");
        assert_eq!(money_decimal(&json!(null)), "null");
    }

    /// 同一条尾差走完整解析路径：pi 与 claude 的费用指标都不得把它透给客户端。
    #[test]
    fn session_cost_metrics_never_expose_float_tails() {
        let pi_cost = pi(&json!({"cost_usd": 0.399_999_999_999_999_97}))
            .into_iter()
            .find(|metric| metric.id == "session/cost_usd")
            .and_then(|metric| metric.amount_decimal);
        assert_eq!(pi_cost.as_deref(), Some("0.4"));
        let claude_cost = claude(&json!({"cost": {"total_cost_usd": 0.399_999_999_999_999_97}}))
            .into_iter()
            .find(|metric| metric.id == "cost/total_cost_usd")
            .and_then(|metric| metric.amount_decimal);
        assert_eq!(claude_cost.as_deref(), Some("0.4"));
    }

    /// 文档终审 D7：指标名称、默认单位与文字值按 server 的界面语言给出——英文界面不含
    /// CJK，中文界面是中文；指标 id 不随语言变化（客户端按 id 认槽位）。
    #[test]
    fn metric_labels_follow_the_interface_language() {
        use crate::i18n::{has_cjk, lang_guard, Lang};
        let statusline = serde_json::json!({
            "rate_limits": {"five_hour": {"used_percentage": 12}},
            "cost": {"total_cost_usd": 0.5, "total_duration_ms": 1000},
            "context_window": {"used_percentage": null, "current_usage": null},
        });
        let kimi_usage = serde_json::json!({
            "limits": [{"used": 3, "limit": 10}],
            "available_balance": 1,
        });
        let stats = "│ Sessions   12 │\n│ Messages  340 │\n│ Avg Cost/Day  $0.25 │";
        let pi_push =
            serde_json::json!({"context": {"percent": null, "tokens": null}, "model": "m"});
        for (lang, chinese) in [(Lang::En, false), (Lang::ZhCn, true)] {
            let _guard = lang_guard(lang);
            let mut metrics = claude(&statusline);
            metrics.extend(kimi(&kimi_usage));
            metrics.extend(opencode_stats(stats));
            metrics.extend(pi(&pi_push));
            metrics.extend(openrouter(&serde_json::json!({"usage": 1.5})));
            assert!(metrics.len() >= 10, "{metrics:?}");
            for metric in &metrics {
                assert_eq!(
                    has_cjk(&metric.label),
                    chinese,
                    "{lang:?}: {}",
                    metric.label
                );
                if let Some(text) = metric.text_value.as_deref().filter(|text| has_cjk(text)) {
                    assert!(chinese, "{lang:?}: {text}");
                }
            }
            let window = metrics
                .iter()
                .find(|metric| metric.id == "window-0")
                .expect("没有单位的额度窗口");
            assert_eq!(has_cjk(&window.unit), chinese, "{lang:?}: {}", window.unit);
            assert!(metrics.iter().any(|metric| metric.id == "five_hour"));
        }
    }
}
