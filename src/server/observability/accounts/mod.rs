mod http;
mod parse;
mod persistence;
mod registry;
mod transport;
pub(crate) use transport::run_probe_helper;

use std::collections::HashMap;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use super::Reply;
use crate::api::schema::*;
use crate::config::{AccountUsageConfig, UsageAccountConfig};

enum Command {
    Request { request: Box<Request>, reply: Reply },
    Release(u64),
}

struct Task {
    generation: u64,
    account: UsageAccountConfig,
    timeout: Duration,
}

struct CacheEntry {
    snapshot: AccountUsageSnapshot,
    requested_at: Option<Instant>,
    in_flight: bool,
    generation: u64,
    failures: u32,
    callback: bool,
    retry_after: Option<Instant>,
}

pub(super) struct Service {
    commands: mpsc::SyncSender<Command>,
}

impl Service {
    pub fn start() -> std::io::Result<Self> {
        let (commands, input) = mpsc::sync_channel(64);
        let (tasks, task_rx) = mpsc::sync_channel::<Task>(32);
        let task_rx = Arc::new(Mutex::new(task_rx));
        let (results, completed) = mpsc::channel::<(u64, AccountUsageSnapshot)>();
        for index in 0..2 {
            let input = task_rx.clone();
            let output = results.clone();
            std::thread::Builder::new()
                .name(format!("herdr-account-query-{index}"))
                .spawn(move || loop {
                    let task = match input.lock() {
                        Ok(receiver) => receiver.recv(),
                        Err(_) => break,
                    };
                    let Ok(task) = task else {
                        break;
                    };
                    let snapshot = query(&task.account, task.timeout);
                    if output.send((task.generation, snapshot)).is_err() {
                        break;
                    }
                })?;
        }
        std::thread::Builder::new()
            .name("herdr-account-usage".into())
            .spawn(move || {
                let mut config = crate::config::Config::load().config.account_usage;
                let mut accounts = configured_accounts(&config);
                let mut saved = persistence::load();
                let mut cache = accounts
                    .iter()
                    .map(|account| {
                        (
                            account.id.clone(),
                            CacheEntry {
                                snapshot: AccountUsageSnapshot {
                                    account_identity: saved.identities.get(&account.id).cloned(),
                                    ..empty_snapshot(account)
                                },
                                requested_at: None,
                                in_flight: false,
                                generation: 0,
                                failures: 0,
                                callback: false,
                                retry_after: None,
                            },
                        )
                    })
                    .collect::<HashMap<_, _>>();
                let mut bindings = std::mem::take(&mut saved.bindings);
                let mut subscribers = HashMap::<String, (Option<u64>, UsageParams, Reply)>::new();
                let mut next_subscription = 1_u64;
                let mut next_query = 1_u64;
                let mut reload_at = Instant::now() + Duration::from_secs(5);
                loop {
                    if Instant::now() >= reload_at {
                        reload_at = Instant::now() + Duration::from_secs(5);
                        let loaded = crate::config::Config::load().config.account_usage;
                        if loaded != config {
                            let updated = configured_accounts(&loaded);
                            for account in &updated {
                                if accounts.iter().find(|old| old.id == account.id) != Some(account)
                                {
                                    bindings.retain(|_, id| id != &account.id);
                                    cache.insert(
                                        account.id.clone(),
                                        CacheEntry {
                                            snapshot: empty_snapshot(account),
                                            requested_at: None,
                                            in_flight: false,
                                            generation: 0,
                                            failures: 0,
                                            callback: false,
                                            retry_after: None,
                                        },
                                    );
                                }
                            }
                            cache.retain(|id, _| updated.iter().any(|account| &account.id == id));
                            bindings.retain(|_, id| cache.contains_key(id));
                            accounts = updated;
                            config = loaded;
                        }
                    }
                    while let Ok((generation, mut snapshot)) = completed.try_recv() {
                        if let Some(entry) = cache.get_mut(&snapshot.account_id) {
                            if entry.generation != generation {
                                continue;
                            }
                            entry.in_flight = false;
                            if invalidate_changed_identity(
                                &entry.snapshot,
                                &mut snapshot,
                                &mut bindings,
                            ) {
                                entry.snapshot.metrics.clear();
                            }
                            merge_result(entry, snapshot);
                            if let Some(identity) = &entry.snapshot.account_identity {
                                saved
                                    .identities
                                    .insert(entry.snapshot.account_id.clone(), identity.clone());
                            }
                            saved.bindings.clone_from(&bindings);
                            persistence::store(&saved);
                            let value = &entry.snapshot;
                            subscribers.retain(|_, (_, params, reply)| {
                                if !reply.alive() {
                                    return false;
                                }
                                if matches_account(params, value, &bindings) {
                                    reply.event(
                                        "account.usage.updated",
                                        serde_json::json!({"accounts":[value]}),
                                    )
                                } else {
                                    true
                                }
                            });
                        }
                    }
                    let command = match input.recv_timeout(Duration::from_millis(100)) {
                        Ok(command) => command,
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            subscribers.retain(|_, (_, _, reply)| reply.alive());
                            for (_, params, _) in subscribers.values() {
                                request_accounts(
                                    params,
                                    false,
                                    &accounts,
                                    &config,
                                    &bindings,
                                    &mut cache,
                                    &tasks,
                                    &mut next_query,
                                );
                            }
                            continue;
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    };
                    let Command::Request { request, reply } = command else {
                        if let Command::Release(id) = command {
                            subscribers.retain(|_, (owner, _, _)| *owner != Some(id));
                        }
                        continue;
                    };
                    let id = request.id;
                    let manual_refresh = matches!(&request.method, Method::AccountUsageRefresh(_));
                    let result = match request.method {
                        Method::AccountUsageProviders(_) => {
                            Ok(ResponseResult::AccountUsageProviders {
                                providers: registry::PROVIDERS
                                    .iter()
                                    .map(|p| UsageProviderInfo {
                                        agent: p.agent.into(),
                                        label: p.label.into(),
                                        source_url: p.source.into(),
                                        method: p.method.into(),
                                        account_scope: p.scope.into(),
                                        minimum_interval_seconds: match p.query {
                                            registry::Query::Callback => 0,
                                            registry::Query::Portal => {
                                                config.api_refresh_seconds.max(60)
                                            }
                                            _ => config.cli_refresh_seconds.max(300),
                                        },
                                        configured_accounts: accounts
                                            .iter()
                                            .filter(|a| a.agent == p.agent)
                                            .map(|a| a.id.clone())
                                            .collect(),
                                    })
                                    .collect(),
                            })
                        }
                        Method::AccountUsageGet(params) | Method::AccountUsageRefresh(params) => {
                            // 连续悬浮与手动刷新都复用同账号的进行中任务和最短间隔。
                            request_accounts(
                                &params,
                                manual_refresh,
                                &accounts,
                                &config,
                                &bindings,
                                &mut cache,
                                &tasks,
                                &mut next_query,
                            );
                            let unbound =
                                params.pane_id.is_some() && !binding_matches(&params, &bindings);
                            let selection = UsageParams {
                                pane_id: if unbound {
                                    None
                                } else {
                                    params.pane_id.clone()
                                },
                                ..params.clone()
                            };
                            let mut values = cache
                                .values()
                                .map(|entry| entry.snapshot.clone())
                                .filter(|value| matches_account(&selection, value, &bindings))
                                .collect::<Vec<_>>();
                            values.sort_by(|a, b| a.account_id.cmp(&b.account_id));
                            if unbound {
                                for value in &mut values {
                                    value.status = ObservationStatus::NeedsBinding;
                                    value.metrics.clear();
                                    value.message = Some(
                                        "请先确认此 Agent 使用的账号；不会按厂商品牌猜测账号"
                                            .into(),
                                    );
                                }
                            }
                            Ok(ResponseResult::AccountUsage { accounts: values })
                        }
                        Method::AccountBindingSet(params) => {
                            if !accounts
                                .iter()
                                .any(|account| account.id == params.account_id)
                            {
                                Err(("unknown_account", "未找到配置的账号".into()))
                            } else if bindings.len() >= 1024 || params.pane_id.len() > 256 {
                                Err(("invalid_binding", "账号绑定超出限制".into()))
                            } else {
                                bindings.insert(params.pane_id.clone(), params.account_id.clone());
                                saved.bindings.clone_from(&bindings);
                                persistence::store(&saved);
                                Ok(ResponseResult::AccountBinding {
                                    pane_id: params.pane_id,
                                    account_id: params.account_id,
                                })
                            }
                        }
                        Method::AccountUsageSubscribe(params) => {
                            subscribers.retain(|_, (_, _, reply)| reply.alive());
                            if subscribers.len() >= 256 {
                                reply.response(
                                    &id,
                                    Err(("subscription_limit", "账号订阅数量超过限制".into())),
                                );
                                continue;
                            }
                            let subscription_id = format!("usage-{next_subscription}");
                            next_subscription = next_subscription.saturating_add(1);
                            subscribers.insert(
                                subscription_id.clone(),
                                (reply.owner(), params.clone(), reply.clone()),
                            );
                            request_accounts(
                                &params,
                                false,
                                &accounts,
                                &config,
                                &bindings,
                                &mut cache,
                                &tasks,
                                &mut next_query,
                            );
                            Ok(ResponseResult::ObservationSubscription {
                                subscription_id,
                                active: true,
                            })
                        }
                        Method::AccountUsageUnsubscribe(params) => {
                            let subscription_id = params.subscription_id.unwrap_or_default();
                            if subscribers
                                .get(&subscription_id)
                                .is_some_and(|(owner, _, _)| *owner == reply.owner())
                            {
                                subscribers.remove(&subscription_id);
                            }
                            Ok(ResponseResult::ObservationSubscription {
                                subscription_id,
                                active: false,
                            })
                        }
                        Method::AccountUsageIntegration(params) => {
                            if let Some(account) = accounts
                                .iter()
                                .find(|account| account.id == params.account_id)
                            {
                                crate::integration::configure_usage(account, params.enabled)
                                    .map(|_| ResponseResult::Ok {})
                                    .map_err(|error| {
                                        ("usage_integration_failed", error.to_string())
                                    })
                            } else {
                                Err(("unknown_account", "未找到配置的账号".into()))
                            }
                        }
                        Method::AccountUsageReport(mut params) => {
                            if params.account_id.is_empty() {
                                params.account_id = params
                                    .pane_id
                                    .as_ref()
                                    .and_then(|pane| bindings.get(pane))
                                    .cloned()
                                    .unwrap_or_default();
                            }
                            if let Some(payload) = params.official_payload.take() {
                                let agent = params.agent.as_deref().and_then(registry::provider);
                                if agent.is_none()
                                    || !accounts.iter().any(|account| {
                                        account.id == params.account_id
                                            && agent
                                                .is_some_and(|agent| account.agent == agent.agent)
                                    })
                                {
                                    reply.response(
                                        &id,
                                        Err((
                                            "usage_binding_required",
                                            "请先在账号用量页面为此窗格绑定对应厂商账号".into(),
                                        )),
                                    );
                                    continue;
                                }
                                params.snapshot.metrics = match agent.map(|provider| provider.agent)
                                {
                                    Some("claude") => parse::claude(&payload),
                                    Some("antigravity") => parse::antigravity(&payload),
                                    Some("kimi") => parse::kimi(&payload),
                                    Some("codex") => parse::codex(&payload),
                                    Some(
                                        "pi" | "qwen" | "maki" | "mastracode" | "opencode" | "omp",
                                    ) => parse::structured(&payload, "session"),
                                    _ => Vec::new(),
                                };
                                params.snapshot.message = None;
                                if agent.is_some_and(|provider| provider.agent == "antigravity") {
                                    params.snapshot.account_identity = payload
                                        .get("email")
                                        .and_then(serde_json::Value::as_str)
                                        .filter(|id| {
                                            !id.is_empty()
                                                && id.len() <= 256
                                                && !id.chars().any(char::is_control)
                                        })
                                        .map(|id| id.to_ascii_lowercase());
                                    params.snapshot.plan = payload
                                        .get("plan_tier")
                                        .and_then(serde_json::Value::as_str)
                                        .map(str::to_owned);
                                }
                            }
                            if !cache.contains_key(&params.account_id)
                                || !parse::validate(&params.snapshot.metrics)
                                || params.pane_id.as_ref().is_some_and(|pane| {
                                    bindings.get(pane) != Some(&params.account_id)
                                })
                            {
                                Err(("invalid_usage_report", "账号用量报告无效".into()))
                            } else {
                                let mut snapshot = params.snapshot;
                                snapshot.account_id = params.account_id.clone();
                                snapshot.observed_at_ms = super::now_ms();
                                if let Some(account) =
                                    accounts.iter().find(|a| a.id == params.account_id)
                                {
                                    snapshot.agent = account.agent.clone();
                                    snapshot.provider = account.provider.clone();
                                    snapshot.auth_mode = account.auth_mode.clone();
                                    snapshot.account_label = account.label.clone();
                                    snapshot.source_url = registry::provider(&account.agent)
                                        .map(|p| p.source)
                                        .unwrap_or_default()
                                        .into();
                                }
                                snapshot.source = "官方 CLI 回调".into();
                                snapshot.status = if snapshot.metrics.is_empty() {
                                    ObservationStatus::Unavailable
                                } else {
                                    ObservationStatus::Ready
                                };
                                if snapshot.account_identity.is_none() {
                                    snapshot.account_identity = cache
                                        .get(&params.account_id)
                                        .and_then(|entry| entry.snapshot.account_identity.clone());
                                }
                                if let Some(entry) = cache.get_mut(&params.account_id) {
                                    invalidate_changed_identity(
                                        &entry.snapshot,
                                        &mut snapshot,
                                        &mut bindings,
                                    );
                                    if let Some(identity) = &snapshot.account_identity {
                                        saved
                                            .identities
                                            .insert(params.account_id.clone(), identity.clone());
                                    }
                                    saved.bindings.clone_from(&bindings);
                                    persistence::store(&saved);
                                    entry.generation = next_query;
                                    next_query = next_query.saturating_add(1);
                                    entry.in_flight = false;
                                    entry.callback = true;
                                    entry.failures = 0;
                                    entry.snapshot = snapshot;
                                    subscribers.retain(|_, (_, params, reply)| {
                                        reply.alive()
                                            && (!matches_account(
                                                params,
                                                &entry.snapshot,
                                                &bindings,
                                            ) || reply.event(
                                                "account.usage.updated",
                                                serde_json::json!({"accounts":[&entry.snapshot]}),
                                            ))
                                    });
                                }
                                Ok(ResponseResult::Ok {})
                            }
                        }
                        _ => Err(("unsupported_method", "不支持的账号请求".into())),
                    };
                    reply.response(&id, result);
                }
            })?;
        Ok(Self { commands })
    }

    pub fn submit(&self, request: Request, reply: Reply) {
        let id = request.id.clone();
        if self
            .commands
            .try_send(Command::Request {
                request: Box::new(request),
                reply: reply.clone(),
            })
            .is_err()
        {
            reply.response(
                &id,
                Err(("server_busy", "账号查询服务繁忙，请稍后重试".into())),
            );
        }
    }

    pub fn release(&self, client_id: u64) {
        let _ = self.commands.try_send(Command::Release(client_id));
    }
}

fn configured_accounts(config: &AccountUsageConfig) -> Vec<UsageAccountConfig> {
    let mut accounts = config.accounts.clone();
    for account in &mut accounts {
        if let Some(provider) = registry::provider(&account.agent) {
            account.agent = provider.agent.into();
        }
    }
    for provider in registry::PROVIDERS {
        if !accounts
            .iter()
            .any(|account| account.agent == provider.agent)
        {
            accounts.push(UsageAccountConfig {
                id: format!("{}:default", provider.agent),
                label: format!("{} 默认登录", provider.label),
                agent: provider.agent.into(),
                provider: provider.agent.into(),
                auth_mode: "cli".into(),
                ..Default::default()
            });
        }
    }
    accounts.truncate(128);
    accounts
}

fn empty_snapshot(account: &UsageAccountConfig) -> AccountUsageSnapshot {
    AccountUsageSnapshot {
        account_id: account.id.clone(),
        account_label: if account.label.is_empty() {
            account.id.clone()
        } else {
            account.label.clone()
        },
        agent: account.agent.clone(),
        provider: account.provider.clone(),
        auth_mode: account.auth_mode.clone(),
        status: ObservationStatus::Warming,
        source_url: registry::provider(&account.agent)
            .map(|p| p.source)
            .unwrap_or_default()
            .into(),
        message: Some("尚未查询".into()),
        ..Default::default()
    }
}

fn matches_account(
    params: &UsageParams,
    value: &AccountUsageSnapshot,
    bindings: &HashMap<String, String>,
) -> bool {
    if params.pane_id.is_some() && !binding_matches(params, bindings) {
        return false;
    }
    let bound = params
        .account_id
        .as_ref()
        .or_else(|| params.pane_id.as_ref().and_then(|pane| bindings.get(pane)));
    if params.pane_id.is_some() && bound.is_none() {
        return false;
    }
    bound.is_none_or(|id| id == &value.account_id)
        && params
            .agent
            .as_ref()
            .is_none_or(|agent| registry::provider(agent).is_some_and(|p| p.agent == value.agent))
}

fn binding_matches(params: &UsageParams, bindings: &HashMap<String, String>) -> bool {
    params
        .pane_id
        .as_ref()
        .and_then(|pane| bindings.get(pane))
        .is_some_and(|bound| {
            params
                .account_id
                .as_ref()
                .is_none_or(|selected| selected == bound)
        })
}

fn request_accounts(
    params: &UsageParams,
    manual: bool,
    accounts: &[UsageAccountConfig],
    config: &AccountUsageConfig,
    bindings: &HashMap<String, String>,
    cache: &mut HashMap<String, CacheEntry>,
    tasks: &mpsc::SyncSender<Task>,
    next_query: &mut u64,
) {
    if !config.enabled {
        return;
    }
    // 默认总览只读缓存；用户选定厂商/账号后再发起查询，避免批量启动 CLI。
    if params.agent.is_none() && params.account_id.is_none() && params.pane_id.is_none() {
        return;
    }
    if params.pane_id.is_some()
        && params.account_id.is_none()
        && !params
            .pane_id
            .as_ref()
            .is_some_and(|pane| bindings.contains_key(pane))
    {
        return;
    }
    for account in accounts {
        let Some(entry) = cache.get_mut(&account.id) else {
            continue;
        };
        if !matches_account(params, &entry.snapshot, bindings) || entry.in_flight {
            continue;
        }
        if entry
            .retry_after
            .is_some_and(|until| Instant::now() < until)
        {
            continue;
        }
        if entry.callback {
            if super::now_ms().saturating_sub(entry.snapshot.observed_at_ms) > 300_000 {
                entry.snapshot.status = ObservationStatus::Stale;
                entry.snapshot.message = Some("等待官方 CLI 下一次回调".into());
            }
            continue;
        }
        if config
            .disabled_providers
            .iter()
            .any(|agent| agent == &account.agent)
        {
            entry.snapshot.status = ObservationStatus::Unavailable;
            entry.snapshot.message = Some("已在设置中关闭此厂商".into());
            continue;
        }
        let seconds = if account.auth_mode == "api" || account.credential_env.is_some() {
            config.api_refresh_seconds.max(60)
        } else {
            config.cli_refresh_seconds.max(300)
        };
        let backoff = seconds
            .saturating_mul(1_u64 << entry.failures.min(5))
            .min(3600);
        if entry.requested_at.is_some_and(|at| {
            at.elapsed() < Duration::from_secs(if manual { seconds } else { backoff })
        }) {
            continue;
        }
        if !manual
            && matches!(
                entry.snapshot.status,
                ObservationStatus::PermissionDenied
                    | ObservationStatus::NotAuthenticated
                    | ObservationStatus::Unsupported
            )
        {
            continue;
        }
        let generation = *next_query;
        *next_query = next_query.saturating_add(1);
        let task = Task {
            generation,
            account: account.clone(),
            timeout: Duration::from_secs(config.probe_timeout_seconds.clamp(5, 30)),
        };
        if tasks.try_send(task).is_ok() {
            entry.generation = generation;
            entry.in_flight = true;
            entry.requested_at = Some(Instant::now());
            entry.snapshot.status = if entry.snapshot.metrics.is_empty() {
                ObservationStatus::Warming
            } else {
                ObservationStatus::Stale
            };
            entry.snapshot.message = Some("正在读取官方用量".into());
        }
    }
}

fn query(account: &UsageAccountConfig, timeout: Duration) -> AccountUsageSnapshot {
    let mut snapshot = empty_snapshot(account);
    let result = if account.auth_mode == "api" || account.credential_env.is_some() {
        snapshot.source = "官方 API".into();
        http::query(account, timeout)
    } else if let Some(provider) = registry::provider(&account.agent) {
        match provider.query {
            registry::Query::Codex => {
                snapshot.source = "Codex App Server".into();
                transport::codex(provider, account, timeout).map(|(identity, value)| {
                    snapshot.account_identity = identity
                        .pointer("/account/id")
                        .or_else(|| identity.pointer("/account/email"))
                        .and_then(serde_json::Value::as_str)
                        .filter(|value| value.len() <= 256)
                        .map(str::to_owned);
                    snapshot.plan = identity
                        .pointer("/account/planType")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned);
                    parse::codex(&value)
                })
            }
            registry::Query::Kimi => {
                snapshot.source = "Kimi 官方本地 Server API".into();
                transport::kimi(provider, account, timeout).map(|(identity, usage)| {
                    snapshot.account_identity = identity
                        .pointer("/data/userInfo/userId")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned);
                    snapshot.plan = identity
                        .pointer("/data/userInfo/userLevelName")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned);
                    parse::kimi(&usage)
                })
            }
            registry::Query::Json(args) => {
                snapshot.source = provider.method.into();
                transport::capture_query(provider, account, args, timeout).and_then(|text| {
                    if provider.agent == "letta" { return Ok(parse::letta(&text)); }
                    if let Ok(value) = serde_json::from_str(&text) {
                        if provider.agent == "omp" { parse::omp(&value, account) } else { Ok(parse::structured(&value, provider.scope)) }
                    } else { Ok(parse::screen(&text, provider.scope)) }
                })
            }
            registry::Query::Interactive(command) => {
                snapshot.source = format!("官方 CLI {command}");
                transport::interactive(provider, account, command, timeout)
                    .map(|text| parse::screen(&text, provider.scope))
            }
            registry::Query::Callback if provider.agent == "antigravity" => Err((ObservationStatus::NeedsBinding, "请将官方 statusline JSON 接入 herdr api usage-report --agent antigravity；等待 quota 字段".into())),
            registry::Query::Callback => Err((
                ObservationStatus::NeedsBinding,
                "此工具的会话统计不代表账号额度；请绑定实际计费厂商，或启用官方用量回调".into(),
            )),
            registry::Query::Portal => Err((
                ObservationStatus::NeedsBinding,
                "请配置官方报表接口的凭据引用与账号范围；官方页面入口可随时打开".into(),
            )),
        }
    } else {
        Err((
            ObservationStatus::Unsupported,
            "未登记此 Agent 的官方查询方案".into(),
        ))
    };
    match result {
        Ok(metrics) if !metrics.is_empty() && parse::validate(&metrics) => {
            snapshot.metrics = metrics;
            snapshot.status = ObservationStatus::Ready;
            snapshot.message = None;
        }
        Ok(_) => {
            snapshot.status = ObservationStatus::Unsupported;
            snapshot.message = Some("官方输出没有已验证的用量字段；未推算账号剩余额度".into());
        }
        Err((status, message)) => {
            snapshot.status = status;
            snapshot.message = Some(message);
        }
    }
    snapshot.observed_at_ms = super::now_ms();
    snapshot
}

fn invalidate_changed_identity(
    previous: &AccountUsageSnapshot,
    next: &mut AccountUsageSnapshot,
    bindings: &mut HashMap<String, String>,
) -> bool {
    if previous
        .account_identity
        .as_ref()
        .zip(next.account_identity.as_ref())
        .is_none_or(|(old, new)| old == new)
    {
        return false;
    }
    bindings.retain(|_, account| account != &next.account_id);
    next.metrics.clear();
    next.status = ObservationStatus::NeedsBinding;
    next.message = Some("官方账号身份已改变，请重新确认窗格绑定".into());
    true
}

fn merge_result(entry: &mut CacheEntry, snapshot: AccountUsageSnapshot) {
    entry.retry_after = snapshot
        .message
        .as_deref()
        .and_then(|message| message.rsplit_once("retry_after=")?.1.parse::<u64>().ok())
        .map(|seconds| Instant::now() + Duration::from_secs(seconds.min(3600)));
    if snapshot.status == ObservationStatus::Error && !entry.snapshot.metrics.is_empty() {
        entry.failures = entry.failures.saturating_add(1);
        entry.snapshot.status = ObservationStatus::Stale;
        entry.snapshot.message = snapshot.message;
    } else {
        entry.failures = if snapshot.status == ObservationStatus::Error {
            entry.failures.saturating_add(1)
        } else {
            0
        };
        entry.snapshot = snapshot;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_switch_revokes_only_that_accounts_bindings_and_metrics() {
        let previous = AccountUsageSnapshot {
            account_id: "work".into(),
            account_identity: Some("a@example.test".into()),
            ..Default::default()
        };
        let mut next = AccountUsageSnapshot {
            account_id: "work".into(),
            account_identity: Some("b@example.test".into()),
            metrics: vec![UsageMetric {
                used: Some(12.0),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut bindings = HashMap::from([
            ("pane-a".into(), "work".into()),
            ("pane-b".into(), "personal".into()),
        ]);
        assert!(invalidate_changed_identity(
            &previous,
            &mut next,
            &mut bindings
        ));
        assert!(!bindings.contains_key("pane-a"));
        assert_eq!(bindings["pane-b"], "personal");
        assert!(next.metrics.is_empty());
        assert_eq!(next.status, ObservationStatus::NeedsBinding);
        assert!(!matches_account(
            &UsageParams {
                pane_id: Some("pane-a".into()),
                account_id: Some("work".into()),
                ..Default::default()
            },
            &next,
            &bindings
        ));
    }

    #[test]
    fn configured_accounts_replace_only_the_matching_default() {
        let config = AccountUsageConfig {
            accounts: vec![UsageAccountConfig {
                id: "work".into(),
                agent: "codex".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let accounts = configured_accounts(&config);
        assert_eq!(
            accounts
                .iter()
                .filter(|account| account.agent == "codex")
                .count(),
            1
        );
        assert!(!accounts.iter().any(|account| account.agent == "muse"));
    }

    #[test]
    fn explicit_binding_prevents_cross_account_cache_reads() {
        let mut bindings = HashMap::new();
        bindings.insert("pane-a".into(), "work".into());
        let params = UsageParams {
            pane_id: Some("pane-a".into()),
            ..Default::default()
        };
        assert!(matches_account(
            &params,
            &AccountUsageSnapshot {
                account_id: "work".into(),
                ..Default::default()
            },
            &bindings
        ));
        assert!(!matches_account(
            &params,
            &AccountUsageSnapshot {
                account_id: "personal".into(),
                ..Default::default()
            },
            &bindings
        ));
        let unbound = UsageParams {
            pane_id: Some("unbound".into()),
            ..Default::default()
        };
        assert!(!matches_account(
            &unbound,
            &AccountUsageSnapshot {
                account_id: "work".into(),
                ..Default::default()
            },
            &bindings
        ));
    }

    #[test]
    fn transient_failure_keeps_sample_age_and_can_recover() {
        let mut entry = CacheEntry {
            snapshot: AccountUsageSnapshot {
                status: ObservationStatus::Ready,
                observed_at_ms: 12,
                metrics: vec![UsageMetric {
                    used: Some(7.0),
                    ..Default::default()
                }],
                ..Default::default()
            },
            requested_at: None,
            in_flight: false,
            generation: 1,
            failures: 0,
            callback: false,
            retry_after: None,
        };
        merge_result(
            &mut entry,
            AccountUsageSnapshot {
                status: ObservationStatus::Error,
                observed_at_ms: 99,
                message: Some("HTTP 429；retry_after=60".into()),
                ..Default::default()
            },
        );
        assert_eq!(entry.snapshot.observed_at_ms, 12);
        assert_eq!(entry.snapshot.metrics[0].used, Some(7.0));
        assert_eq!(entry.snapshot.status, ObservationStatus::Stale);
        assert!(entry.retry_after.is_some());
        merge_result(
            &mut entry,
            AccountUsageSnapshot {
                status: ObservationStatus::Ready,
                observed_at_ms: 100,
                metrics: vec![UsageMetric {
                    used: Some(8.0),
                    ..Default::default()
                }],
                ..Default::default()
            },
        );
        assert_eq!(entry.snapshot.observed_at_ms, 100);
        assert_eq!(entry.failures, 0);
        assert!(entry.retry_after.is_none());
    }

    #[test]
    fn callback_reports_are_not_overwritten_by_placeholder_queries() {
        let config = AccountUsageConfig::default();
        let accounts = configured_accounts(&config);
        let account = accounts
            .iter()
            .find(|account| account.agent == "pi")
            .unwrap();
        let mut cache = HashMap::from([(
            account.id.clone(),
            CacheEntry {
                snapshot: AccountUsageSnapshot {
                    metrics: vec![UsageMetric {
                        used: Some(12.0),
                        ..Default::default()
                    }],
                    observed_at_ms: super::super::now_ms(),
                    ..empty_snapshot(account)
                },
                requested_at: None,
                in_flight: false,
                generation: 1,
                failures: 0,
                callback: true,
                retry_after: None,
            },
        )]);
        let (tasks, input) = mpsc::sync_channel(1);
        request_accounts(
            &UsageParams {
                account_id: Some(account.id.clone()),
                ..Default::default()
            },
            true,
            &accounts,
            &config,
            &HashMap::new(),
            &mut cache,
            &tasks,
            &mut 2,
        );
        assert!(input.try_recv().is_err());
        assert_eq!(cache[&account.id].snapshot.metrics[0].used, Some(12.0));
    }
}
