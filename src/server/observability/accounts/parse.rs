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
    );
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
        resets_at: ["resetsAt", "resets_at", "reset_time", "reset_at"]
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
}
