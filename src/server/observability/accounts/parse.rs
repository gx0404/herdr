//! 解析厂商公开结果。没有确切含义的数字不作为账号额度显示。

use crate::api::schema::UsageMetric;
use serde_json::Value;

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
    let unit = value.get("unit").and_then(Value::as_str).unwrap_or(
        if percent.is_some() && used.is_none() {
            "%"
        } else {
            "额度单位"
        },
    );
    let percentage = percent.or_else(|| {
        used.zip(limit)
            .filter(|(_, total)| *total > 0.0)
            .map(|(used, total)| used / total * 100.0)
    });
    Some(UsageMetric {
        id,
        label,
        scope: scope.into(),
        unit: unit.into(),
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
        for (key, label) in [("primary", "主要额度"), ("secondary", "次级额度")] {
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
                id: format!("{id}/credits"),
                label: "额外余额".into(),
                unit: "credits".into(),
                scope: "account".into(),
                amount_decimal: Some(decimal(balance)),
                ..Default::default()
            });
        }
    }
    metrics
}

pub(super) fn claude(value: &Value) -> Vec<UsageMetric> {
    let limits = value.get("rate_limits").unwrap_or(value);
    [
        ("five_hour", "5 小时额度"),
        ("seven_day", "每周额度"),
        ("spend_limit", "网关消费额度"),
    ]
    .into_iter()
    .filter_map(|(key, label)| window(key.into(), label.into(), &limits[key], "account"))
    .collect()
}

pub(super) fn antigravity(value: &Value) -> Vec<UsageMetric> {
    value
        .get("quota")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter_map(|(id, quota)| {
            let remaining =
                finite(quota.get("remaining_fraction")).filter(|value| *value <= 1.0)?;
            Some(UsageMetric {
                id: id.clone(),
                label: id.clone(),
                unit: "%".into(),
                scope: "account".into(),
                used_percent: Some((1.0 - remaining) * 100.0),
                resets_at: timestamp(quota.get("reset_time")),
                ..Default::default()
            })
        })
        .collect()
}

pub(super) fn omp(
    value: &Value,
    account: &crate::config::UsageAccountConfig,
) -> Result<Vec<UsageMetric>, super::transport::QueryError> {
    let reports = value
        .get("reports")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|report| {
            let provider = report.get("provider").and_then(Value::as_str);
            (account.provider == "omp"
                || account.provider.is_empty()
                || provider == Some(account.provider.as_str()))
                && account.account_user.as_ref().is_none_or(|selected| {
                    ["email", "accountId", "projectId"].iter().any(|key| {
                        report
                            .get("metadata")
                            .and_then(|meta| meta.get(*key))
                            .and_then(Value::as_str)
                            == Some(selected.as_str())
                    })
                })
                && account.organization.as_ref().is_none_or(|org| {
                    report.pointer("/metadata/orgId").and_then(Value::as_str) == Some(org.as_str())
                })
        })
        .collect::<Vec<_>>();
    if reports.len() > 1 {
        return Err((
            crate::api::schema::ObservationStatus::NeedsBinding,
            "OMP 有多个计费账号，请配置 provider、account_user 和需要的 organization".into(),
        ));
    }
    let mut result = Vec::new();
    for report in reports {
        let provider = report
            .get("provider")
            .and_then(Value::as_str)
            .unwrap_or("OMP");
        for limit in report
            .get("limits")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let amount = &limit["amount"];
            let id = limit.get("id").and_then(Value::as_str).unwrap_or("");
            let label = limit.get("label").and_then(Value::as_str).unwrap_or(id);
            let mut metric = window(
                format!("{provider}/{id}"),
                format!("{provider} · {label}"),
                amount,
                "account",
            )
            .unwrap_or_else(|| UsageMetric {
                id: format!("{provider}/{id}"),
                label: format!("{provider} · {label}"),
                unit: amount
                    .get("unit")
                    .and_then(Value::as_str)
                    .unwrap_or("额度单位")
                    .into(),
                scope: "account".into(),
                ..Default::default()
            });
            metric.used_percent = finite(amount.get("usedFraction"))
                .map(|value| value * 100.0)
                .or_else(|| {
                    finite(amount.get("remainingFraction"))
                        .filter(|value| *value <= 1.0)
                        .map(|value| (1.0 - value) * 100.0)
                })
                .or(metric.used_percent);
            metric.resets_at = limit
                .pointer("/window/resetsAt")
                .and_then(Value::as_u64)
                .map(|value| value / 1000);
            metric.window_seconds = limit
                .pointer("/window/durationMs")
                .and_then(Value::as_u64)
                .map(|value| value / 1000);
            if metric.used_percent.is_some() || metric.used.is_some() || metric.remaining.is_some()
            {
                result.push(metric);
            }
        }
    }
    Ok(result)
}

pub(super) fn letta(text: &str) -> Vec<UsageMetric> {
    if !text.contains("# Letta usage overview") {
        return Vec::new();
    }
    let mut result = Vec::new();
    for line in text.lines().map(str::trim) {
        if let Some(value) = line
            .strip_prefix("* Balance: ")
            .and_then(|value| value.strip_suffix(" credits"))
        {
            if let Ok(remaining) = value.parse::<f64>() {
                result.push(UsageMetric {
                    id: "balance".into(),
                    label: "Letta credits".into(),
                    unit: "credits".into(),
                    scope: "account".into(),
                    remaining: Some(remaining),
                    ..Default::default()
                });
            }
        }
        if let Some(value) = line.strip_prefix("* Bucket (full/high/medium/low/empty): ") {
            result.push(UsageMetric {
                id: "quota-bucket".into(),
                label: "letta/* quota".into(),
                unit: "state".into(),
                scope: "account".into(),
                text_value: Some(value.chars().take(120).collect()),
                ..Default::default()
            });
        }
        if let Some(value) = line
            .strip_prefix("* Quota Window End: ")
            .filter(|value| *value != "Unavailable")
        {
            if let Some(metric) = result.iter_mut().find(|metric| metric.id == "quota-bucket") {
                metric.resets_at = timestamp(Some(&Value::String(value.into())));
            }
        }
    }
    result
}

pub(super) fn kimi(value: &Value) -> Vec<UsageMetric> {
    let value = value.get("data").unwrap_or(value);
    let mut metrics = Vec::new();
    if let Some(metric) = window(
        "summary".into(),
        "套餐额度".into(),
        &value["summary"],
        "account",
    ) {
        metrics.push(metric);
    }
    // Kimi 2.x packs quota windows under `quota.usages` keyed by window id.
    if let Some(usages) = value.pointer("/quota/usages").and_then(Value::as_object) {
        for (key, window_value) in usages {
            let label = match key.as_str() {
                "limit5h" => "5 小时额度",
                "limit7d" => "7 天额度",
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
                .unwrap_or_else(|| format!("额度窗口 {}", index + 1));
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
        ("available_balance", "可用余额"),
        ("voucher_balance", "代金券余额"),
        ("cash_balance", "现金余额"),
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
    if let Some(wallet) = value.get("extra_usage").filter(|wallet| wallet.is_object()) {
        if let Some(currency) = wallet.get("currency").and_then(Value::as_str) {
            for (key, label) in [
                ("balance_cents", "额外用量余额"),
                ("monthly_used_cents", "本月额外用量费用"),
                ("monthly_charge_limit_cents", "每月额外用量上限"),
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
                        unit: format!("{currency} cents"),
                        amount_decimal: Some(decimal(amount)),
                        ..Default::default()
                    });
                }
            }
        }
    }
    metrics
}

pub(super) fn moonshot_balance(value: &Value, currency: &str) -> Vec<UsageMetric> {
    let value = value.get("data").unwrap_or(value);
    [
        ("available_balance", "可用余额"),
        ("voucher_balance", "代金券余额"),
        ("cash_balance", "现金余额"),
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
    let mut metrics = Vec::new();
    for (key, label) in [
        ("usage", "此密钥累计费用"),
        ("usage_daily", "此密钥今日费用"),
        ("usage_weekly", "此密钥本周费用"),
        ("usage_monthly", "此密钥本月费用"),
        ("limit_remaining", "此密钥剩余预算"),
        ("total_credits", "账号累计充值"),
        ("total_usage", "账号累计消费"),
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
                        id: format!("{path}/{key}"),
                        label: key.clone(),
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
                    id: format!("{path}/{key}"),
                    label: key.clone(),
                    unit: if key == "balance" {
                        "厂商余额单位"
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

pub(super) fn screen(text: &str, scope: &str) -> Vec<UsageMetric> {
    let Ok(percent) =
        regex::Regex::new(r"(?i)(\d+(?:\.\d+)?)\s*%\s*(used|remaining|left|已用|剩余)?")
    else {
        return Vec::new();
    };
    let Ok(credits) = regex::Regex::new(
        r"(?i)(\d+(?:\.\d+)?)\s*/\s*(\d+(?:\.\d+)?)\s*(credits?|tokens?|requests?|额度|积分)",
    ) else {
        return Vec::new();
    };
    let mut metrics = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
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

pub(super) fn validate(metrics: &[UsageMetric]) -> bool {
    metrics.len() <= 128
        && metrics.iter().all(|metric| {
            metric.id.len() <= 256
                && metric
                    .text_value
                    .as_ref()
                    .is_none_or(|value| value.len() <= 512 && !value.chars().any(char::is_control))
                && metric.label.len() <= 512
                && metric.unit.len() <= 64
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
                        && v.bytes().all(|c| {
                            c.is_ascii_digit() || matches!(c, b'.' | b'-' | b'+' | b'e' | b'E')
                        })
                })
        })
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

/// 登录组关键字（小写、行首词边界锚定）。前半是各厂商登录画面的标题 / 提示行（claude 2.x、
/// Gemini CLI 认证对话），后半是通用措辞，覆盖 gemini / grok / hermes 等共用本判定的厂商。
/// 以 `/` 开头的斜杠命令行（`/login  Sign in with …`）在判定前被整行跳过，所以这里可以
/// 保留 `sign in` / `log in` 这类短语而不误判 `/help` 列表。
const SIGN_IN_LINE_PREFIXES: &[&str] = &[
    "select login method",
    "how do you want to sign in",
    "how would you like to authenticate",
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
/// `Quick safety check` / `Yes, I trust this folder` / `Yes, trust it`，以及 Gemini CLI 的
/// 目录信任对话标题。`Accessing workspace:` 是工作区横幅、`permission required` 是通用措辞，
/// 都不能单独作为判据。
const TRUST_LINE_PREFIXES: &[&str] = &[
    "quick safety check",
    "yes, i trust this folder",
    "yes, trust it",
    "do you trust this folder",
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

    /// Gemini CLI 首次启动的认证对话（文案按官方文档 geminicli.com/docs/get-started/
    /// authentication 编写；待真机快照校正）。
    const GEMINI_AUTH_SCREEN: &str = "\
 ╭───────────────────────────────────────────────────────╮
 │ How would you like to authenticate for this project?  │
 │                                                       │
 │ ● 1. Login with Google                                │
 │   2. Use Gemini API Key                               │
 │   3. Vertex AI                                        │
 │                                                       │
 │ (Use Enter to select)                                 │
 ╰───────────────────────────────────────────────────────╯
";

    /// Gemini CLI 的目录信任对话（文案按官方文档 trusted-folders 编写；待真机快照校正）。
    const GEMINI_TRUST_SCREEN: &str = "\
 ╭───────────────────────────────────────────────────────╮
 │ Do you trust this folder?                             │
 │ Trusting a folder allows Gemini to execute commands.  │
 │ ● 1. Trust folder                                     │
 │   2. Trust parent folder                              │
 │   3. Don't trust                                      │
 ╰───────────────────────────────────────────────────────╯
";

    /// Grok CLI 未登录提示（通用措辞，待真机快照校正）。
    const GROK_SIGN_IN_SCREEN: &str = "\
 You must log in first.
 Run /login to authenticate with your xAI account.
 ❯
";

    /// Hermes 未登录提示（通用措辞，待真机快照校正）。
    const HERMES_SIGN_IN_SCREEN: &str = "\
 Not authenticated. Run `hermes auth login` to continue.
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

    #[test]
    fn login_screens_of_other_interactive_vendors_are_recognized() {
        assert_eq!(
            interactive_blocker(GEMINI_AUTH_SCREEN),
            Some(ProbeBlocker::SignIn),
            "gemini 认证对话"
        );
        assert_eq!(
            interactive_blocker(GEMINI_TRUST_SCREEN),
            Some(ProbeBlocker::Trust),
            "gemini 目录信任对话"
        );
        assert_eq!(
            interactive_blocker(GROK_SIGN_IN_SCREEN),
            Some(ProbeBlocker::SignIn),
            "grok 登录提示"
        );
        assert_eq!(
            interactive_blocker(HERMES_SIGN_IN_SCREEN),
            Some(ProbeBlocker::SignIn),
            "hermes 登录提示"
        );
        for line in [
            "Please sign in to continue",
            "You need to sign in",
            "Authentication required",
            "You must log in first",
            "Sign in with Google to get started",
            "Login required: run `grok auth`",
            "Log in to your account",
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
        assert!(!cli_usage_error("Not logged in"));
        assert!(!cli_usage_error(""));
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
    fn missing_claude_quota_never_becomes_zero_and_overage_is_retained() {
        assert!(claude(&json!({"context_window":{"used_percentage":80}})).is_empty());
        let metrics = claude(
            &json!({"rate_limits":{"spend_limit":{"used_percentage":110,"resets_at":1700000000}}}),
        );
        assert_eq!(metrics[0].used_percent, Some(110.0));
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
    fn omp_same_email_in_two_organizations_requires_scope() {
        let value = json!({"reports":[{"provider":"anthropic","metadata":{"email":"user@example.test","orgId":"a"},"limits":[]},{"provider":"anthropic","metadata":{"email":"user@example.test","orgId":"b"},"limits":[]}]});
        let mut account = crate::config::UsageAccountConfig {
            provider: "anthropic".into(),
            account_user: Some("user@example.test".into()),
            ..Default::default()
        };
        assert_eq!(
            omp(&value, &account).unwrap_err().0,
            crate::api::schema::ObservationStatus::NeedsBinding
        );
        account.organization = Some("a".into());
        assert!(omp(&value, &account).is_ok());
    }

    #[test]
    fn official_statusline_ignores_context_and_converts_remaining_fraction() {
        let values = antigravity(
            &json!({"context_window":{"used_percentage":90},"quota":{"weekly":{"remaining_fraction":0.75,"reset_time":"2026-09-17T00:00:00Z"}}}),
        );
        assert_eq!(values.len(), 1);
        assert_eq!(values[0].used_percent, Some(25.0));
        assert!(values[0].resets_at.is_some());
        assert!(antigravity(&json!({"quota":{"broken":{"remaining_fraction":2}}})).is_empty());
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
}
