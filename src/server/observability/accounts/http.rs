//! 仅调用已登记的官方只读接口；认证只在所属主机内组装。

use super::{metric_texts, parse, probe_texts, transport::QueryError};
use crate::api::schema::{ObservationStatus, UsageMetric};
use crate::config::UsageAccountConfig;
use reqwest::blocking::Client;
use serde_json::Value;
use std::time::Duration;

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
            probe_texts().api_credential_env_required.into(),
        )
    })?;
    let credential = std::env::var(env)
        .ok()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            (
                ObservationStatus::NotAuthenticated,
                probe_texts().api_credential_missing.into(),
            )
        })?;
    let client = Client::builder()
        .timeout(timeout)
        .connect_timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| {
            (
                ObservationStatus::Error,
                probe_texts().api_client_failed.into(),
            )
        })?;
    let today = time::OffsetDateTime::now_utc().date().to_string();
    let start_time = time::OffsetDateTime::now_utc()
        .replace_time(time::Time::MIDNIGHT)
        .unix_timestamp();
    let base = match provider {
        "openai" | "codex" => "https://api.openai.com",
        "anthropic" | "claude" => "https://api.anthropic.com",
        "moonshot" | "kimi-api" => "https://api.moonshot.cn",
        "openrouter" => "https://openrouter.ai",
        "kimi" => account.base_url.as_deref().ok_or_else(|| {
            (
                ObservationStatus::NeedsBinding,
                probe_texts().api_kimi_base_required.into(),
            )
        })?,
        _ => {
            return Err((
                ObservationStatus::Unsupported,
                probe_texts().api_provider_unsupported.into(),
            ))
        }
    };
    let base = account
        .base_url
        .as_deref()
        .unwrap_or(base)
        .trim_end_matches('/');
    validate_base(provider, base)?;
    let path = match provider {
        "openai" | "codex" => format!("/v1/organization/usage/completions?start_time={start_time}&bucket_width=1d&limit=1"),
        "anthropic" | "claude" => format!("/v1/organizations/usage_report/messages?starting_at={today}T00%3A00%3A00Z&bucket_width=1d&limit=1"),
        "moonshot" | "kimi-api" => "/v1/users/me/balance".into(),
        "kimi" => "/api/v1/oauth/usage".into(),
        "openrouter" => if account.billing_scope.as_deref() == Some("account") { "/api/v1/credits" } else { "/api/v1/key" }.into(),
        _ => {
            return Err((
                ObservationStatus::Unsupported,
                probe_texts().api_query_unsupported.into(),
            ))
        }
    };
    let deadline = std::time::Instant::now() + timeout;
    let read = |path: &str| {
        read_json(
            &client,
            base,
            path,
            provider,
            &credential,
            account,
            deadline,
        )
    };
    let value = read(&path)?;
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
        _ => parse::structured(&value, "organization"),
    };
    if matches!(provider, "openai" | "codex" | "anthropic" | "claude") {
        let cost_path = if matches!(provider, "openai" | "codex") {
            format!("/v1/organization/costs?start_time={start_time}&bucket_width=1d&limit=1")
        } else {
            format!("/v1/organizations/cost_report?starting_at={today}T00%3A00%3A00Z&bucket_width=1d&limit=1")
        };
        if let Ok(costs) = read(&cost_path) {
            metrics.extend(cost_report(&costs, provider));
        }
    }
    metrics.truncate(128);
    if metrics.is_empty() {
        return Err((
            ObservationStatus::Unsupported,
            probe_texts().api_no_verified_fields.into(),
        ));
    }
    Ok(metrics)
}

fn read_json(
    client: &Client,
    base: &str,
    path: &str,
    provider: &str,
    credential: &str,
    account: &UsageAccountConfig,
    deadline: std::time::Instant,
) -> Result<Value, QueryError> {
    let mut request = client
        .get(format!("{base}{path}"))
        .header("Accept", "application/json")
        .header("User-Agent", concat!("herdr/", env!("CARGO_PKG_VERSION")));
    request = match provider {
        "anthropic" | "claude" => request
            .header("x-api-key", credential)
            .header("anthropic-version", "2023-06-01"),
        _ => request.bearer_auth(credential),
    };
    if matches!(provider, "openai" | "codex") {
        if let Some(org) = &account.organization {
            request = request.header("OpenAI-Organization", org);
        }
    }
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    if remaining.is_zero() {
        return Err((ObservationStatus::Error, probe_texts().api_deadline.into()));
    }
    let response = request.timeout(remaining).send().map_err(|_| {
        (
            ObservationStatus::Error,
            probe_texts().api_connect_failed.into(),
        )
    })?;
    let status = response.status();
    if !status.is_success() {
        let kind = status_kind(status.as_u16());
        let retry = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .map(|seconds| {
                crate::i18n::fill(
                    probe_texts().api_retry_after_fmt,
                    &[("seconds", &seconds.clamp(1, 3600).to_string())],
                )
            })
            .unwrap_or_default();
        return Err((
            kind,
            crate::i18n::fill(
                probe_texts().api_http_status_fmt,
                &[("status", &status.as_u16().to_string()), ("retry", &retry)],
            ),
        ));
    }
    if response
        .content_length()
        .is_some_and(|length| length > 2 * 1024 * 1024)
    {
        return Err((ObservationStatus::Error, probe_texts().api_too_large.into()));
    }
    let mut data = Vec::new();
    std::io::Read::take(response, 2 * 1024 * 1024 + 1)
        .read_to_end(&mut data)
        .map_err(|_| {
            (
                ObservationStatus::Error,
                probe_texts().api_unreadable.into(),
            )
        })?;
    if data.len() > 2 * 1024 * 1024 {
        return Err((ObservationStatus::Error, probe_texts().api_too_large.into()));
    }
    let value: Value = serde_json::from_slice(&data).map_err(|_| {
        (
            ObservationStatus::Error,
            probe_texts().api_unrecognized.into(),
        )
    })?;
    Ok(value)
}

/// 官方 HTTP 接口非 2xx 状态 → 观测状态：401 未登录、403 无权限、404 接口不存在（终态），
/// 其余（429 / 5xx / 网关错误）是可重试的瞬时错误。CLI 本地服务（kimi web）与远程 API
/// 共用这一张表。
pub(super) fn status_kind(status: u16) -> ObservationStatus {
    match status {
        401 => ObservationStatus::NotAuthenticated,
        403 => ObservationStatus::PermissionDenied,
        404 => ObservationStatus::Unsupported,
        _ => ObservationStatus::Error,
    }
}

fn validate_base(provider: &str, base: &str) -> Result<(), QueryError> {
    let url = reqwest::Url::parse(base).map_err(|_| {
        (
            ObservationStatus::NeedsBinding,
            probe_texts().api_base_invalid.into(),
        )
    })?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err((
            ObservationStatus::NeedsBinding,
            probe_texts().api_base_has_credentials.into(),
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
        _ => false,
    };
    if allowed && matches!(url.path(), "" | "/") {
        Ok(())
    } else {
        Err((
            ObservationStatus::Unsupported,
            probe_texts().api_base_unregistered.into(),
        ))
    }
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
                    label: metric_texts().cost_report.into(),
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
    fn http_status_mapping_separates_terminal_from_transient() {
        assert_eq!(status_kind(401), ObservationStatus::NotAuthenticated);
        assert_eq!(status_kind(403), ObservationStatus::PermissionDenied);
        assert_eq!(status_kind(404), ObservationStatus::Unsupported);
        for transient in [400, 408, 429, 500, 502, 503] {
            assert_eq!(
                status_kind(transient),
                ObservationStatus::Error,
                "{transient}"
            );
        }
    }

    /// 范围外厂商的报表接口已移除：既没有登记的主机，也不会把凭据发出去。
    #[test]
    fn retired_billing_providers_have_no_registered_host() {
        for (provider, base) in [
            ("cursor", "https://api.cursor.com"),
            ("github", "https://api.github.com"),
            ("devin", "https://api.devin.ai"),
            ("factory", "https://api.factory.ai"),
            ("cline", "https://api.cline.bot"),
            ("amp", "https://ampcode.com"),
            ("xai", "https://management-api.x.ai"),
        ] {
            let (status, _) = validate_base(provider, base).expect_err(provider);
            assert_eq!(status, ObservationStatus::Unsupported, "{provider}");
        }
        for (provider, base) in [
            ("anthropic", "https://api.anthropic.com"),
            ("moonshot", "https://api.moonshot.ai"),
            ("openrouter", "https://openrouter.ai"),
        ] {
            assert!(validate_base(provider, base).is_ok(), "{provider}");
        }
    }

    #[test]
    fn cost_reports_keep_the_vendor_amount_and_currency() {
        let openai = serde_json::json!({"data":[{"results":[{"amount":{"value":"1.25","currency":"usd"}}]}]});
        let values = cost_report(&openai, "openai");
        assert_eq!(values[0].amount_decimal.as_deref(), Some("1.25"));
        assert_eq!(values[0].unit, "USD");
        let anthropic = serde_json::json!({"data":[{"results":[{"amount":"140.05"}]}]});
        let values = cost_report(&anthropic, "anthropic");
        assert_eq!(values[0].amount_decimal.as_deref(), Some("140.05"));
        assert_eq!(values[0].unit, "USD cents");
        assert!(cost_report(&serde_json::json!({}), "openai").is_empty());
    }

    /// 文档终审 D7：官方接口查询被拒的说明按 server 的界面语言给出——英文界面不含 CJK，
    /// 中文界面是中文。都在发请求之前拒绝，不碰网络。
    #[test]
    fn api_rejections_follow_the_interface_language() {
        use crate::i18n::{has_cjk, lang_guard, Lang};
        let account = UsageAccountConfig {
            id: "codex:api".into(),
            agent: "codex".into(),
            auth_mode: "api".into(),
            ..Default::default()
        };
        for (lang, chinese) in [(Lang::En, false), (Lang::ZhCn, true)] {
            let _guard = lang_guard(lang);
            let mut messages = vec![query(&account, Duration::from_secs(1)).unwrap_err().1];
            for base in [
                "not a url",
                "https://user:secret@api.openai.com",
                "https://evil.example",
            ] {
                messages.push(validate_base("openai", base).unwrap_err().1);
            }
            for message in &messages {
                assert_eq!(has_cjk(message), chinese, "{lang:?}: {message}");
            }
        }
    }
}
