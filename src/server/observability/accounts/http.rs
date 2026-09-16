//! 仅调用已登记的官方只读接口；认证只在所属主机内组装。

use super::{parse, transport::QueryError};
use crate::api::schema::{ObservationStatus, UsageMetric};
use crate::config::UsageAccountConfig;
use reqwest::blocking::Client;
use serde_json::{json, Value};
use std::time::Duration;

fn identifier(value: Option<&str>) -> Result<&str, QueryError> {
    let value = value
        .filter(|v| {
            !v.is_empty()
                && v.len() <= 128
                && v.bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
        })
        .ok_or_else(|| {
            (
                ObservationStatus::NeedsBinding,
                "请配置官方计费账号的组织或用户标识".into(),
            )
        })?;
    Ok(value)
}

pub(super) fn query(
    account: &UsageAccountConfig,
    timeout: Duration,
) -> Result<Vec<UsageMetric>, QueryError> {
    let provider = if account.provider.is_empty() {
        account.agent.as_str()
    } else {
        account.provider.as_str()
    };
    let env = account.credential_env.as_deref().ok_or_else(|| {
        (
            ObservationStatus::NotAuthenticated,
            "请配置凭据环境变量引用；不要填写密钥本身".into(),
        )
    })?;
    let credential = std::env::var(env)
        .ok()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            (
                ObservationStatus::NotAuthenticated,
                "server 未取得所配置的凭据环境变量".into(),
            )
        })?;
    let client = Client::builder()
        .timeout(timeout)
        .connect_timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| (ObservationStatus::Error, "无法初始化官方接口连接".into()))?;
    let today = time::OffsetDateTime::now_utc().date().to_string();
    let yesterday = (time::OffsetDateTime::now_utc() - time::Duration::days(1))
        .date()
        .to_string();
    let start_time = time::OffsetDateTime::now_utc()
        .replace_time(time::Time::MIDNIGHT)
        .unix_timestamp();
    let base = match provider {
        "openai" | "codex" => "https://api.openai.com",
        "anthropic" | "claude" => "https://api.anthropic.com",
        "moonshot" | "kimi-api" => "https://api.moonshot.cn",
        "openrouter" => "https://openrouter.ai",
        "cursor" => "https://api.cursor.com",
        "github" | "github-copilot" => "https://api.github.com",
        "devin" => "https://api.devin.ai",
        "factory" | "droid" => "https://api.factory.ai",
        "cline" => "https://api.cline.bot",
        "amp" => "https://ampcode.com",
        "xai" | "grok" => "https://management-api.x.ai",
        "kimi" => account.base_url.as_deref().ok_or_else(|| {
            (
                ObservationStatus::NeedsBinding,
                "请指定 Kimi 官方本地 server 地址，或选择 CLI 查询".into(),
            )
        })?,
        _ => {
            return Err((
                ObservationStatus::Unsupported,
                "未登记此计费厂商的官方查询接口".into(),
            ))
        }
    };
    let base = account
        .base_url
        .as_deref()
        .unwrap_or(base)
        .trim_end_matches('/');
    validate_base(provider, base)?;
    let (path, body) = match provider {
        "openai" | "codex" => (format!("/v1/organization/usage/completions?start_time={start_time}&bucket_width=1d&limit=1"), None),
        "anthropic" | "claude" => (format!("/v1/organizations/usage_report/messages?starting_at={today}T00%3A00%3A00Z&bucket_width=1d&limit=1"), None),
        "moonshot" | "kimi-api" => ("/v1/users/me/balance".into(), None),
        "kimi" => ("/api/v1/oauth/usage".into(), None),
        "openrouter" => (if account.billing_scope.as_deref() == Some("account") { "/api/v1/credits" } else { "/api/v1/key" }.into(), None),
        "cursor" => ("/teams/spend".into(), Some(json!({"searchTerm":account.account_user.as_deref().filter(|value| value.contains('@')).unwrap_or(""),"page":1,"pageSize":100}))),
        "github" | "github-copilot" => {
            let target = if account.billing_scope.as_deref() == Some("enterprise") {
                format!("enterprises/{}", identifier(account.organization.as_deref())?)
            } else if let Some(org) = account.organization.as_deref() {
                format!("organizations/{}", identifier(Some(org))?)
            } else { format!("users/{}", identifier(account.account_user.as_deref())?) };
            (format!("/{target}/settings/billing/ai_credit/usage{}", if account.organization.is_some() { account.account_user.as_deref().map(|user| identifier(Some(user)).map(|user| format!("?user={user}"))).transpose()?.unwrap_or_default() } else { String::new() }), None)
        }
        "devin" => (format!("/v3/enterprise/consumption/daily/users/{}", identifier(account.account_user.as_deref())?), None),
        "factory" | "droid" => (format!("/api/v1/analytics/cost/me/query?queryKey=headline_daily&startDate={yesterday}&endDate={yesterday}"), None),
        "cline" => (if let Some(org) = account.organization.as_deref() { format!("/api/v1/organizations/{}/balance", identifier(Some(org))?) } else { format!("/api/v1/users/{}/balance", identifier(account.account_user.as_deref())?) }, None),
        "amp" => (format!("/api/v2/workspace/analytics/daily-usage?lookbackDays=1{}", account.account_user.as_deref().map(|user| identifier(Some(user)).map(|user| format!("&userID={user}"))).transpose()?.unwrap_or_default()), None),
        "xai" | "grok" => (format!("/v1/billing/teams/{}/prepaid/balance", identifier(account.organization.as_deref())?), None),
        _ => return Err((ObservationStatus::Unsupported, "未登记此查询".into())),
    };
    let deadline = std::time::Instant::now() + timeout;
    let read = |path: &str, body: Option<Value>| {
        read_json(
            &client,
            base,
            path,
            body,
            provider,
            &credential,
            account,
            deadline,
        )
    };
    let mut value = read(&path, body)?;
    if provider == "cursor" {
        let selected = account.account_user.as_deref().ok_or_else(|| {
            (
                ObservationStatus::NeedsBinding,
                "Cursor 管理员报表需要指定准确的 userId 或邮箱".into(),
            )
        })?;
        for page in 2..=20 {
            if !cursor(&value, Some(selected)).is_empty() {
                break;
            }
            if value
                .get("teamMemberSpend")
                .and_then(Value::as_array)
                .is_none_or(|items| items.len() < 100)
            {
                break;
            }
            value = read(
                "/teams/spend",
                Some(
                    json!({"searchTerm":if selected.contains('@') { selected } else { "" },"page":page,"pageSize":100}),
                ),
            )?;
        }
    }
    let mut metrics = match provider {
        "moonshot" | "kimi-api" => parse::moonshot_balance(
            &value,
            if reqwest::Url::parse(base)
                .ok()
                .and_then(|url| url.host_str().map(str::to_owned))
                .as_deref()
                == Some("api.moonshot.ai")
            {
                "USD"
            } else {
                "CNY"
            },
        ),
        "kimi" => parse::kimi(&value),
        "openrouter" => parse::openrouter(&value),
        "devin" => number_metric(&value, "total_acus", "查询期间 ACU 消耗", "ACU", "account")
            .into_iter()
            .collect(),
        "github" | "github-copilot" => github(&value),
        "cursor" => cursor(&value, account.account_user.as_deref()),
        "factory" | "droid" => factory(&value),
        "cline" => credits(&value, "Cline credits"),
        "amp" => amp(&value, account.account_user.as_deref()),
        "xai" | "grok" => xai_balance(&value),
        _ => parse::structured(&value, "organization"),
    };
    if matches!(provider, "openai" | "codex" | "anthropic" | "claude") {
        let cost_path = if matches!(provider, "openai" | "codex") {
            format!("/v1/organization/costs?start_time={start_time}&bucket_width=1d&limit=1")
        } else {
            format!("/v1/organizations/cost_report?starting_at={today}T00%3A00%3A00Z&bucket_width=1d&limit=1")
        };
        if let Ok(costs) = read(&cost_path, None) {
            metrics.extend(cost_report(&costs, provider));
        }
    }
    metrics.truncate(128);
    if metrics.is_empty() {
        return Err((
            ObservationStatus::Unsupported,
            "官方响应未包含已验证的用量字段；未推算额度".into(),
        ));
    }
    Ok(metrics)
}

fn read_json(
    client: &Client,
    base: &str,
    path: &str,
    body: Option<Value>,
    provider: &str,
    credential: &str,
    account: &UsageAccountConfig,
    deadline: std::time::Instant,
) -> Result<Value, QueryError> {
    let mut request = if let Some(body) = body {
        client.post(format!("{base}{path}")).json(&body)
    } else {
        client.get(format!("{base}{path}"))
    };
    request = request
        .header("Accept", "application/json")
        .header("User-Agent", concat!("herdr/", env!("CARGO_PKG_VERSION")));
    request = match provider {
        "anthropic" | "claude" => request
            .header("x-api-key", credential)
            .header("anthropic-version", "2023-06-01"),
        "cursor" => request.basic_auth(credential, Some("")),
        _ => request.bearer_auth(credential),
    };
    if matches!(provider, "github" | "github-copilot") {
        request = request.header("X-GitHub-Api-Version", "2026-03-10");
    }
    if matches!(provider, "openai" | "codex") {
        if let Some(org) = &account.organization {
            request = request.header("OpenAI-Organization", org);
        }
    }
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    if remaining.is_zero() {
        return Err((ObservationStatus::Error, "官方查询达到总时限".into()));
    }
    let response = request
        .timeout(remaining)
        .send()
        .map_err(|_| (ObservationStatus::Error, "官方接口连接失败或超时".into()))?;
    let status = response.status();
    if !status.is_success() {
        let kind = match status.as_u16() {
            401 => ObservationStatus::NotAuthenticated,
            403 => ObservationStatus::PermissionDenied,
            404 => ObservationStatus::Unsupported,
            _ => ObservationStatus::Error,
        };
        let retry = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .map(|seconds| format!("；retry_after={}", seconds.clamp(1, 3600)))
            .unwrap_or_default();
        return Err((
            kind,
            format!(
                "官方查询返回 HTTP {}；请检查账号、地区和报表权限{retry}",
                status.as_u16()
            ),
        ));
    }
    if response
        .content_length()
        .is_some_and(|length| length > 2 * 1024 * 1024)
    {
        return Err((ObservationStatus::Error, "官方查询结果超过大小限制".into()));
    }
    let mut data = Vec::new();
    std::io::Read::take(response, 2 * 1024 * 1024 + 1)
        .read_to_end(&mut data)
        .map_err(|_| (ObservationStatus::Error, "官方查询返回内容无法读取".into()))?;
    if data.len() > 2 * 1024 * 1024 {
        return Err((ObservationStatus::Error, "官方查询结果超过大小限制".into()));
    }
    let value: Value = serde_json::from_slice(&data).map_err(|_| {
        (
            ObservationStatus::Error,
            "官方接口返回了无法识别的数据".into(),
        )
    })?;
    Ok(value)
}

fn validate_base(provider: &str, base: &str) -> Result<(), QueryError> {
    let url = reqwest::Url::parse(base)
        .map_err(|_| (ObservationStatus::NeedsBinding, "查询地址无效".into()))?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err((
            ObservationStatus::NeedsBinding,
            "查询地址不能携带凭据或查询参数".into(),
        ));
    }
    let host = url.host_str().unwrap_or_default();
    let allowed = match provider {
        "kimi" => url.scheme() == "http" && matches!(host, "127.0.0.1" | "[::1]" | "::1"),
        "openai" | "codex" => url.scheme() == "https" && host == "api.openai.com",
        "anthropic" | "claude" => url.scheme() == "https" && host == "api.anthropic.com",
        "moonshot" | "kimi-api" => {
            url.scheme() == "https" && matches!(host, "api.moonshot.cn" | "api.moonshot.ai")
        }
        "openrouter" => url.scheme() == "https" && host == "openrouter.ai",
        "cursor" => url.scheme() == "https" && host == "api.cursor.com",
        "github" | "github-copilot" => url.scheme() == "https" && host == "api.github.com",
        "devin" => url.scheme() == "https" && host == "api.devin.ai",
        "factory" | "droid" => url.scheme() == "https" && host == "api.factory.ai",
        "cline" => url.scheme() == "https" && host == "api.cline.bot",
        "amp" => url.scheme() == "https" && host == "ampcode.com",
        "xai" | "grok" => url.scheme() == "https" && host == "management-api.x.ai",
        _ => false,
    };
    if allowed && matches!(url.path(), "" | "/") {
        Ok(())
    } else {
        Err((
            ObservationStatus::Unsupported,
            "未登记此官方计费服务地址，未发送凭据".into(),
        ))
    }
}

fn number_metric(
    value: &Value,
    key: &str,
    label: &str,
    unit: &str,
    scope: &str,
) -> Option<UsageMetric> {
    let used = value
        .get(key)?
        .as_f64()
        .filter(|v| v.is_finite() && *v >= 0.0)?;
    Some(UsageMetric {
        id: key.into(),
        label: label.into(),
        unit: unit.into(),
        scope: scope.into(),
        used: Some(used),
        ..Default::default()
    })
}

fn credits(value: &Value, unit: &str) -> Vec<UsageMetric> {
    let mut metrics = Vec::new();
    fn walk(value: &Value, unit: &str, path: &str, depth: usize, metrics: &mut Vec<UsageMetric>) {
        if depth > 6 || metrics.len() > 128 {
            return;
        }
        if let Some(object) = value.as_object() {
            for (key, child) in object {
                if matches!(
                    key.as_str(),
                    "balance" | "totalCredits" | "total_credits" | "creditsUsed" | "credits_used"
                ) && (child.is_number()
                    || child.as_str().is_some_and(|v| v.parse::<f64>().is_ok()))
                {
                    metrics.push(UsageMetric {
                        id: format!("{path}/{key}"),
                        label: key.clone(),
                        unit: unit.into(),
                        scope: "account".into(),
                        amount_decimal: Some(parse::decimal(child)),
                        ..Default::default()
                    });
                } else {
                    walk(child, unit, &format!("{path}/{key}"), depth + 1, metrics);
                }
            }
        } else if let Some(array) = value.as_array() {
            for (index, child) in array.iter().take(128).enumerate() {
                walk(child, unit, &format!("{path}/{index}"), depth + 1, metrics);
            }
        }
    }
    walk(value, unit, "", 0, &mut metrics);
    metrics
}

fn amp(value: &Value, user: Option<&str>) -> Vec<UsageMetric> {
    let mut metrics = Vec::new();
    for day in value
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let date = day.get("date").and_then(Value::as_str).unwrap_or("");
        for item in day
            .get("users")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let id = item
                .pointer("/user/id")
                .and_then(Value::as_str)
                .unwrap_or("");
            if user.is_some_and(|user| user != id) {
                continue;
            }
            if let Some(usage) = item
                .pointer("/metrics/usage")
                .filter(|value| value.is_number())
            {
                metrics.push(UsageMetric {
                    id: format!("{date}/{id}/usage"),
                    label: format!("{date} · {id}"),
                    amount_decimal: Some(parse::decimal(usage)),
                    unit: "USD".into(),
                    scope: "workspace_user".into(),
                    ..Default::default()
                });
            }
        }
    }
    // BYOK 的 estimatedProviderCostAtListPriceUSD 是估算价，不放入实际消费。
    metrics
}

fn cost_report(value: &Value, provider: &str) -> Vec<UsageMetric> {
    let mut metrics = Vec::new();
    for (index, bucket) in value
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        for (result_index, result) in bucket
            .get("results")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
        {
            let (amount, unit) = if matches!(provider, "openai" | "codex") {
                (
                    result.pointer("/amount/value"),
                    result
                        .pointer("/amount/currency")
                        .and_then(Value::as_str)
                        .unwrap_or("usd")
                        .to_ascii_uppercase(),
                )
            } else {
                (result.get("amount"), "USD cents".into())
            };
            if let Some(amount) = amount.filter(|value| {
                value.is_number()
                    || value
                        .as_str()
                        .is_some_and(|text| text.parse::<f64>().is_ok())
            }) {
                metrics.push(UsageMetric {
                    id: format!("cost/{index}/{result_index}"),
                    label: "当日官方费用报表".into(),
                    unit,
                    scope: "organization".into(),
                    amount_decimal: Some(parse::decimal(amount)),
                    ..Default::default()
                });
            }
        }
    }
    metrics
}

fn github(value: &Value) -> Vec<UsageMetric> {
    let mut metrics = Vec::new();
    for (index, item) in value
        .get("usageItems")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .take(40)
        .enumerate()
    {
        let sku = item.get("sku").and_then(Value::as_str).unwrap_or("Copilot");
        let model = item.get("model").and_then(Value::as_str).unwrap_or("");
        let unit = item
            .get("unitType")
            .and_then(Value::as_str)
            .unwrap_or("credits");
        for (key, label) in [
            ("grossQuantity", "实际消耗"),
            ("netQuantity", "折扣后计费数量"),
        ] {
            if let Some(used) = item
                .get(key)
                .and_then(Value::as_f64)
                .filter(|value| value.is_finite() && *value >= 0.0)
            {
                metrics.push(UsageMetric {
                    id: format!("billing-{index}-{key}"),
                    label: format!("{sku} {model} · {label}"),
                    unit: unit.into(),
                    scope: "billing_account".into(),
                    used: Some(used),
                    ..Default::default()
                });
            }
        }
        if let Some(amount) = item.get("netAmount") {
            metrics.push(UsageMetric {
                id: format!("billing-{index}-cost"),
                label: format!("{sku} {model} · 实际费用"),
                unit: "USD".into(),
                scope: "billing_account".into(),
                amount_decimal: Some(parse::decimal(amount)),
                ..Default::default()
            });
        }
    }
    metrics
}

fn cursor(value: &Value, expected: Option<&str>) -> Vec<UsageMetric> {
    let mut metrics = Vec::new();
    for item in value
        .get("teamMemberSpend")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .take(128)
    {
        let Some(id) = item.get("userId").and_then(Value::as_str) else {
            continue;
        };
        if expected.is_some_and(|wanted| {
            wanted != id
                && !item
                    .get("email")
                    .and_then(Value::as_str)
                    .is_some_and(|email| email.eq_ignore_ascii_case(wanted))
        }) {
            continue;
        }
        for (key, label) in [
            ("spendCents", "按量超额消费"),
            ("overallSpendCents", "含套餐内使用的总量"),
        ] {
            if let Some(amount) = item.get(key).filter(|v| v.is_number() || v.is_string()) {
                metrics.push(UsageMetric {
                    id: format!("{id}/{key}"),
                    label: format!("{id} · {label}"),
                    unit: "USD cents".into(),
                    scope: "organization_member".into(),
                    amount_decimal: Some(parse::decimal(amount)),
                    ..Default::default()
                });
            }
        }
    }
    metrics
}

fn factory(value: &Value) -> Vec<UsageMetric> {
    value
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|row| {
            let amount = row.get("fsc")?;
            if !amount.is_number() && !amount.is_string() {
                return None;
            }
            let date = row.get("date").and_then(Value::as_str)?;
            Some(UsageMetric {
                id: format!("fsc/{date}"),
                label: format!("{date} 已结算用量"),
                unit: "Factory Standard Credits".into(),
                scope: "authenticated_user".into(),
                amount_decimal: Some(parse::decimal(amount)),
                ..Default::default()
            })
        })
        .collect()
}

fn xai_balance(value: &Value) -> Vec<UsageMetric> {
    let Some(value) = value
        .pointer("/total/val")
        .and_then(Value::as_str)
        .filter(|s| s.parse::<f64>().is_ok_and(f64::is_finite))
    else {
        return Vec::new();
    };
    let prepaid = value.starts_with('-') || value.parse::<f64>().ok() == Some(0.0);
    vec![UsageMetric {
        id: "prepaid-balance".into(),
        label: if prepaid {
            "可用预付余额"
        } else {
            "待结算账本金额"
        }
        .into(),
        unit: "USD cents".into(),
        scope: "team".into(),
        amount_decimal: Some(value.trim_start_matches('-').into()),
        ..Default::default()
    }]
}

use std::io::Read;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_only_go_to_registered_official_hosts() {
        assert!(validate_base("openai", "https://api.openai.com").is_ok());
        assert!(validate_base("openai", "https://api.openai.com.attacker.invalid").is_err());
        assert!(validate_base("openai", "http://api.openai.com").is_err());
        assert!(validate_base("kimi", "http://127.0.0.1:58627").is_ok());
        assert!(validate_base("kimi", "http://192.168.1.2:58627").is_err());
        assert!(validate_base("openai", "https://secret@api.openai.com").is_err());
    }

    #[test]
    fn vendor_amounts_preserve_their_documented_meaning() {
        let values = factory(&json!({"data":[{"date":"2026-09-15","fsc":12.5}]}));
        assert_eq!(values[0].amount_decimal.as_deref(), Some("12.5"));
        assert!(factory(&json!({"data":[{"date":"2026-09-15"}]})).is_empty());
        assert_eq!(
            xai_balance(&json!({"total":{"val":"-1000"}}))[0]
                .amount_decimal
                .as_deref(),
            Some("1000")
        );
        assert!(xai_balance(&json!({})).is_empty());
        let payload = json!({"teamMemberSpend":[{"userId":"a","email":"a@example.test","spendCents":0,"overallSpendCents":140.05},{"userId":"b","spendCents":200}]});
        let values = cursor(&payload, Some("a"));
        assert_eq!(values.len(), 2);
        assert_eq!(values[0].id, "a/spendCents");
        assert_eq!(values[1].amount_decimal.as_deref(), Some("140.05"));
        assert!(cursor(&payload, Some("missing")).is_empty());
    }
}
