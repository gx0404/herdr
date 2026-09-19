mod apply;
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
                let mut rejections = apply::Rejections::default();
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
                            // 待办里的候选账号与限速键都可能已失效，随账号清单一起重来。
                            rejections = apply::Rejections::default();
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
                            tracing::debug!(
                                event = "account.probe.complete",
                                subsystem = "account_usage",
                                outcome = "completed",
                                account_id = %snapshot.account_id,
                                status = ?snapshot.status,
                                metrics = snapshot.metrics.len(),
                                generation,
                                "账号用量探测完成"
                            );
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
                                        installed: Some(registry::provider_installed(p)),
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
                                        "请先确认此 Agent 使用的账号；该厂商只有一个账号时会在收到官方回调时自动绑定，多个账号请手动选择"
                                            .into(),
                                    );
                                }
                            }
                            Ok(ResponseResult::AccountUsage { accounts: values })
                        }
                        Method::AccountBindingSet(params) => apply::bind_pane(
                            &params.pane_id,
                            &params.account_id,
                            &accounts,
                            &mut bindings,
                            &mut rejections,
                        )
                        .map(|()| {
                            saved.bindings.clone_from(&bindings);
                            persistence::store(&saved);
                            ResponseResult::AccountBinding {
                                pane_id: params.pane_id,
                                account_id: params.account_id,
                            }
                        }),
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
                        Method::AccountUsageReport(params) => {
                            match apply::apply_report(
                                params,
                                apply::Context {
                                    accounts: &accounts,
                                    enabled: config.enabled,
                                    now_ms: super::now_ms(),
                                },
                                &mut bindings,
                                &mut cache,
                                &mut rejections,
                                &mut next_query,
                            ) {
                                Ok(accepted) => {
                                    if let Some(pane) = &accepted.auto_bound_pane {
                                        tracing::info!(
                                            event = "account.report.auto_bind",
                                            subsystem = "account_usage",
                                            outcome = "ok",
                                            pane_id = %pane,
                                            account_id = %accepted.account_id,
                                            "未绑定窗格按唯一账号自动绑定"
                                        );
                                    }
                                    if commit_accepted(
                                        &accepted,
                                        &cache,
                                        &bindings,
                                        &mut saved,
                                        &mut subscribers,
                                    ) {
                                        persistence::store(&saved);
                                    }
                                    Ok(ResponseResult::Ok {})
                                }
                                Err(rejected) => {
                                    // 只记错误码与定位字段（pane 截断、agent 规范化），不落
                                    // message 与原始报文；同一 (pane, agent, code) 的重复拒绝
                                    // 降级为 debug。
                                    if rejected.repeated {
                                        tracing::debug!(
                                            event = "account.report.reject",
                                            subsystem = "account_usage",
                                            outcome = "error",
                                            error_code = rejected.code,
                                            pane_id = %rejected.pane_id,
                                            agent = %rejected.agent,
                                            repeated = true,
                                            "官方回调被拒绝"
                                        );
                                    } else {
                                        tracing::info!(
                                            event = "account.report.reject",
                                            subsystem = "account_usage",
                                            outcome = "error",
                                            error_code = rejected.code,
                                            pane_id = %rejected.pane_id,
                                            agent = %rejected.agent,
                                            repeated = false,
                                            "官方回调被拒绝"
                                        );
                                    }
                                    Err((rejected.code, rejected.message))
                                }
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

/// 报告被接受后的调用方职责：记住公开身份、同步绑定快照并推送给匹配的订阅者。
/// 返回是否有内容需要持久化（缓存里找不到该账号时为 false）。
fn commit_accepted(
    accepted: &apply::Accepted,
    cache: &HashMap<String, CacheEntry>,
    bindings: &HashMap<String, String>,
    saved: &mut persistence::Saved,
    subscribers: &mut HashMap<String, (Option<u64>, UsageParams, Reply)>,
) -> bool {
    let Some(entry) = cache.get(&accepted.account_id) else {
        return false;
    };
    if let Some(identity) = &entry.snapshot.account_identity {
        saved
            .identities
            .insert(accepted.account_id.clone(), identity.clone());
    }
    saved.bindings.clone_from(bindings);
    tracing::debug!(
        event = "account.report.accept",
        subsystem = "account_usage",
        outcome = "ok",
        account_id = %accepted.account_id,
        status = ?entry.snapshot.status,
        metrics = entry.snapshot.metrics.len(),
        "官方回调已写入账号用量"
    );
    let value = &entry.snapshot;
    subscribers.retain(|_, (_, params, reply)| {
        reply.alive()
            && (!matches_account(params, value, bindings)
                || reply.event(
                    "account.usage.updated",
                    serde_json::json!({"accounts":[value]}),
                ))
    });
    true
}

fn configured_accounts(config: &AccountUsageConfig) -> Vec<UsageAccountConfig> {
    let mut accounts = config.accounts.clone();
    for account in &mut accounts {
        if let Some(provider) = registry::provider(&account.agent) {
            account.agent = provider.agent.into();
        }
    }
    // Only installed CLIs get an implicit default account, so the usage page
    // reflects what this host can actually query instead of the full
    // registry. User-configured accounts are always kept.
    for provider in registry::PROVIDERS {
        if !registry::provider_installed(provider) {
            continue;
        }
        if !accounts
            .iter()
            .any(|account| account.agent == provider.agent)
        {
            accounts.push(UsageAccountConfig {
                id: format!("{}:default", provider.agent),
                label: provider.label.into(),
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
        probe_skipped(None, "disabled", manual);
        return;
    }
    // 总览（未选定厂商/账号）也发起自动查询：账号清单已只含本机已安装厂商，
    // 查询频率由下方 requested_at/退避控制，用户显式选择仍可立即查询。
    if params.pane_id.is_some()
        && params.account_id.is_none()
        && !params
            .pane_id
            .as_ref()
            .is_some_and(|pane| bindings.contains_key(pane))
    {
        probe_skipped(None, "unbound_pane", manual);
        return;
    }
    for account in accounts {
        let Some(entry) = cache.get_mut(&account.id) else {
            continue;
        };
        if !matches_account(params, &entry.snapshot, bindings) {
            continue;
        }
        if entry.in_flight {
            probe_skipped(Some(&account.id), "in_flight", manual);
            continue;
        }
        if entry
            .retry_after
            .is_some_and(|until| Instant::now() < until)
        {
            probe_skipped(Some(&account.id), "retry_after", manual);
            continue;
        }
        if entry.callback {
            if super::now_ms().saturating_sub(entry.snapshot.observed_at_ms) > 300_000 {
                entry.snapshot.status = ObservationStatus::Stale;
                entry.snapshot.message = Some("等待官方 CLI 下一次回调".into());
            }
            probe_skipped(Some(&account.id), "callback", manual);
            continue;
        }
        if config
            .disabled_providers
            .iter()
            .any(|agent| agent == &account.agent)
        {
            entry.snapshot.status = ObservationStatus::Unavailable;
            entry.snapshot.message = Some("已在设置中关闭此厂商".into());
            probe_skipped(Some(&account.id), "disabled_provider", manual);
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
            probe_skipped(Some(&account.id), "interval", manual);
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
            probe_skipped(Some(&account.id), "terminal_status", manual);
            continue;
        }
        // generation 只在任务真正入队后消耗，队列满时缓存条目与计数都保持不变。
        let generation = *next_query;
        let task = Task {
            generation,
            account: account.clone(),
            timeout: Duration::from_secs(config.probe_timeout_seconds.clamp(5, 30)),
        };
        if tasks.try_send(task).is_ok() {
            *next_query = next_query.saturating_add(1);
            entry.generation = generation;
            entry.in_flight = true;
            entry.requested_at = Some(Instant::now());
            entry.snapshot.status = if entry.snapshot.metrics.is_empty() {
                ObservationStatus::Warming
            } else {
                ObservationStatus::Stale
            };
            entry.snapshot.message = Some("正在读取官方用量".into());
            tracing::debug!(
                event = "account.probe.dispatch",
                subsystem = "account_usage",
                outcome = "started",
                account_id = %account.id,
                agent = %account.agent,
                generation,
                manual,
                "派发账号用量探测"
            );
        } else {
            probe_skipped(Some(&account.id), "queue_full", manual);
        }
    }
}

/// 闸门跳过只记 trace：订阅者驱动的空转 tick 每 100 ms 走一遍这些分支。
/// 全局闸门（disabled / unbound_pane）不针对具体账号，不带 `account_id` 字段。
fn probe_skipped(account_id: Option<&str>, gate: &'static str, manual: bool) {
    let event = "account.probe.skip";
    let subsystem = "account_usage";
    let outcome = "skipped";
    let message = "账号用量探测被闸门跳过";
    if let Some(account_id) = account_id {
        tracing::trace!(
            event,
            subsystem,
            outcome,
            account_id,
            gate,
            manual,
            "{message}"
        );
    } else {
        tracing::trace!(event, subsystem, outcome, gate, manual, "{message}");
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
        // Hermetic account fixture: implicit defaults now only cover
        // installed CLIs, which must not decide this test's outcome.
        let account = UsageAccountConfig {
            id: "pi:default".into(),
            label: "Pi".into(),
            agent: "pi".into(),
            provider: "pi".into(),
            auth_mode: "cli".into(),
            ..Default::default()
        };
        let mut cache = HashMap::from([(
            account.id.clone(),
            CacheEntry {
                snapshot: AccountUsageSnapshot {
                    metrics: vec![UsageMetric {
                        used: Some(12.0),
                        ..Default::default()
                    }],
                    observed_at_ms: super::super::now_ms(),
                    ..empty_snapshot(&account)
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
        let config = AccountUsageConfig::default();
        let accounts = vec![account.clone()];
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

    /// `request_accounts` 一道闸门的表驱动用例：只描述输入差异与期望结局。
    struct Gate {
        name: &'static str,
        manual: bool,
        params: UsageParams,
        bindings: Vec<(&'static str, &'static str)>,
        enabled: bool,
        disabled_providers: Vec<&'static str>,
        prepare: fn(&mut CacheEntry),
        /// 预先塞满任务通道，模拟 `queue_full` 闸门。
        queue_full: bool,
        enqueued: bool,
        status: Option<ObservationStatus>,
        message: Option<&'static str>,
    }

    fn gate(name: &'static str) -> Gate {
        Gate {
            name,
            manual: false,
            params: UsageParams {
                account_id: Some("claude:default".into()),
                ..Default::default()
            },
            bindings: Vec::new(),
            enabled: true,
            disabled_providers: Vec::new(),
            prepare: |_| {},
            queue_full: false,
            enqueued: true,
            status: Some(ObservationStatus::Warming),
            message: Some("正在读取官方用量"),
        }
    }

    fn fresh_entry(account: &UsageAccountConfig) -> CacheEntry {
        CacheEntry {
            snapshot: empty_snapshot(account),
            requested_at: None,
            in_flight: false,
            generation: 0,
            failures: 0,
            callback: false,
            retry_after: None,
        }
    }

    #[test]
    fn request_accounts_gates_are_each_covered_by_a_contrasting_case() {
        let account = UsageAccountConfig {
            id: "claude:default".into(),
            label: "Claude Code".into(),
            agent: "claude".into(),
            provider: "claude".into(),
            auth_mode: "cli".into(),
            ..Default::default()
        };
        let cases = vec![
            gate("合法输入被接受"),
            Gate {
                enabled: false,
                enqueued: false,
                message: Some("尚未查询"),
                ..gate("account_usage.enabled=false 时一个探测都不发")
            },
            Gate {
                params: UsageParams {
                    pane_id: Some("pane-1".into()),
                    ..Default::default()
                },
                enqueued: false,
                message: Some("尚未查询"),
                ..gate("未绑定 pane 且未指定账号时早退")
            },
            Gate {
                params: UsageParams {
                    pane_id: Some("pane-1".into()),
                    ..Default::default()
                },
                bindings: vec![("pane-1", "claude:default")],
                ..gate("对照：已绑定 pane 正常派发")
            },
            Gate {
                params: UsageParams {
                    agent: Some("kimi".into()),
                    ..Default::default()
                },
                enqueued: false,
                message: Some("尚未查询"),
                ..gate("厂商不匹配的账号被过滤")
            },
            Gate {
                prepare: |entry| entry.in_flight = true,
                enqueued: false,
                message: Some("尚未查询"),
                ..gate("进行中的探测不重复派发")
            },
            Gate {
                prepare: |entry| entry.retry_after = Some(Instant::now() + Duration::from_secs(60)),
                enqueued: false,
                message: Some("尚未查询"),
                ..gate("retry_after 未到不派发")
            },
            Gate {
                manual: true,
                prepare: |entry| entry.retry_after = Some(Instant::now() + Duration::from_secs(60)),
                enqueued: false,
                message: Some("尚未查询"),
                ..gate("retry_after 未到即使手动也不派发")
            },
            Gate {
                prepare: |entry| {
                    entry.callback = true;
                    entry.snapshot.status = ObservationStatus::Ready;
                    entry.snapshot.message = None;
                    entry.snapshot.observed_at_ms = super::super::now_ms();
                },
                enqueued: false,
                status: Some(ObservationStatus::Ready),
                message: None,
                ..gate("新鲜回调不回落探测")
            },
            Gate {
                prepare: |entry| {
                    entry.callback = true;
                    entry.snapshot.status = ObservationStatus::Ready;
                    entry.snapshot.observed_at_ms = super::super::now_ms().saturating_sub(400_000);
                },
                enqueued: false,
                status: Some(ObservationStatus::Stale),
                message: Some("等待官方 CLI 下一次回调"),
                ..gate("过期回调改标 Stale 但仍不探测")
            },
            Gate {
                manual: true,
                prepare: |entry| {
                    entry.callback = true;
                    entry.snapshot.status = ObservationStatus::Ready;
                    entry.snapshot.message = None;
                    entry.snapshot.observed_at_ms = super::super::now_ms();
                },
                enqueued: false,
                status: Some(ObservationStatus::Ready),
                message: None,
                ..gate("对照：回调闩锁连手动刷新也拦")
            },
            Gate {
                queue_full: true,
                enqueued: false,
                message: Some("尚未查询"),
                ..gate("任务队列满时不派发、不消耗 generation、不置 in_flight")
            },
            Gate {
                disabled_providers: vec!["claude"],
                enqueued: false,
                status: Some(ObservationStatus::Unavailable),
                message: Some("已在设置中关闭此厂商"),
                ..gate("设置里关闭的厂商不派发")
            },
            Gate {
                prepare: |entry| entry.requested_at = Some(Instant::now()),
                enqueued: false,
                message: Some("尚未查询"),
                ..gate("requested_at 刚刷新过时不派发")
            },
            Gate {
                manual: true,
                prepare: |entry| entry.requested_at = Some(Instant::now()),
                enqueued: false,
                message: Some("尚未查询"),
                ..gate("手动刷新也遵守最短间隔")
            },
            Gate {
                prepare: |entry| entry.snapshot.status = ObservationStatus::NotAuthenticated,
                enqueued: false,
                status: Some(ObservationStatus::NotAuthenticated),
                message: Some("尚未查询"),
                ..gate("NotAuthenticated 终态非手动永不入队")
            },
            Gate {
                prepare: |entry| entry.snapshot.status = ObservationStatus::PermissionDenied,
                enqueued: false,
                status: Some(ObservationStatus::PermissionDenied),
                message: Some("尚未查询"),
                ..gate("PermissionDenied 终态非手动永不入队")
            },
            Gate {
                prepare: |entry| entry.snapshot.status = ObservationStatus::Unsupported,
                enqueued: false,
                status: Some(ObservationStatus::Unsupported),
                message: Some("尚未查询"),
                ..gate("Unsupported 终态非手动永不入队")
            },
            Gate {
                manual: true,
                prepare: |entry| entry.snapshot.status = ObservationStatus::NotAuthenticated,
                ..gate("对照：手动刷新穿透终态")
            },
            Gate {
                prepare: |entry| {
                    entry.snapshot.metrics = vec![UsageMetric {
                        used: Some(1.0),
                        ..Default::default()
                    }];
                },
                status: Some(ObservationStatus::Stale),
                ..gate("已有样本时派发中标 Stale 而非 Warming")
            },
        ];
        for case in cases {
            let accounts = vec![account.clone()];
            let config = AccountUsageConfig {
                enabled: case.enabled,
                disabled_providers: case
                    .disabled_providers
                    .iter()
                    .map(|agent| agent.to_string())
                    .collect(),
                ..Default::default()
            };
            let bindings = case
                .bindings
                .iter()
                .map(|(pane, id)| (pane.to_string(), id.to_string()))
                .collect::<HashMap<_, _>>();
            let mut entry = fresh_entry(&account);
            (case.prepare)(&mut entry);
            let mut cache = HashMap::from([(account.id.clone(), entry)]);
            let (tasks, input) = mpsc::sync_channel(1);
            if case.queue_full {
                let placeholder = Task {
                    generation: 0,
                    account: account.clone(),
                    timeout: Duration::from_secs(5),
                };
                assert!(tasks.try_send(placeholder).is_ok(), "{}: 预填", case.name);
            }
            let mut next_query = 5;
            request_accounts(
                &case.params,
                case.manual,
                &accounts,
                &config,
                &bindings,
                &mut cache,
                &tasks,
                &mut next_query,
            );
            let entry = &cache[&account.id];
            if case.queue_full {
                let placeholder = input.try_recv().ok();
                assert_eq!(
                    placeholder.map(|task| task.generation),
                    Some(0),
                    "{}: 先取出预填任务",
                    case.name
                );
                assert!(!entry.in_flight, "{}: 队列满不置 in_flight", case.name);
                assert!(
                    entry.requested_at.is_none(),
                    "{}: 队列满不记 requested_at",
                    case.name
                );
            }
            let task = input.try_recv().ok();
            assert_eq!(task.is_some(), case.enqueued, "{}: 入队", case.name);
            if case.enqueued {
                let task = task.unwrap_or_else(|| panic!("{}: 缺少任务", case.name));
                assert_eq!(task.generation, 5, "{}: generation", case.name);
                assert_eq!(task.account.id, account.id, "{}: 账号", case.name);
                assert_eq!(next_query, 6, "{}: next_query", case.name);
                assert!(entry.in_flight, "{}: in_flight", case.name);
                assert!(entry.requested_at.is_some(), "{}: requested_at", case.name);
                assert_eq!(entry.generation, 5, "{}: 缓存 generation", case.name);
            } else {
                assert_eq!(next_query, 5, "{}: next_query 不变", case.name);
                assert_eq!(entry.generation, 0, "{}: 未入队不改 generation", case.name);
            }
            if let Some(status) = case.status {
                assert_eq!(entry.snapshot.status, status, "{}: status", case.name);
            }
            assert_eq!(
                entry.snapshot.message.as_deref(),
                case.message,
                "{}: message",
                case.name
            );
        }
    }

    #[test]
    fn accepted_reports_are_committed_to_saved_state_and_pushed_to_subscribers() {
        let account = UsageAccountConfig {
            id: "claude:default".into(),
            label: "Claude Code".into(),
            agent: "claude".into(),
            provider: "claude".into(),
            auth_mode: "cli".into(),
            ..Default::default()
        };
        let accounts = vec![account.clone()];
        let mut entry = fresh_entry(&account);
        entry.snapshot.account_identity = Some("me@example.test".into());
        let mut cache = HashMap::from([(account.id.clone(), entry)]);
        let mut bindings = HashMap::new();
        let mut rejections = apply::Rejections::default();
        let mut next_query = 1;
        let accepted = apply::apply_report(
            UsageReportParams {
                account_id: String::new(),
                pane_id: Some("wT:p9".into()),
                agent: Some("claude".into()),
                official_payload: Some(
                    serde_json::json!({"rate_limits": {"five_hour": {"used_percentage": 42}}}),
                ),
                snapshot: Default::default(),
            },
            apply::Context {
                accounts: &accounts,
                enabled: true,
                now_ms: super::super::now_ms(),
            },
            &mut bindings,
            &mut cache,
            &mut rejections,
            &mut next_query,
        )
        .unwrap_or_else(|rejected| panic!("被拒: {}", rejected.code));
        assert_eq!(accepted.auto_bound_pane.as_deref(), Some("wT:p9"));

        let (bound_sender, bound_receiver) = mpsc::channel();
        let (other_sender, other_receiver) = mpsc::channel();
        let mut subscribers = HashMap::from([
            (
                "usage-1".to_string(),
                (
                    None,
                    UsageParams {
                        pane_id: Some("wT:p9".into()),
                        ..Default::default()
                    },
                    Reply::Api {
                        sender: bound_sender,
                        active: None,
                        latest: None,
                    },
                ),
            ),
            (
                "usage-2".to_string(),
                (
                    None,
                    UsageParams {
                        agent: Some("kimi".into()),
                        ..Default::default()
                    },
                    Reply::Api {
                        sender: other_sender,
                        active: None,
                        latest: None,
                    },
                ),
            ),
        ]);
        let mut saved = persistence::Saved::default();
        assert!(commit_accepted(
            &accepted,
            &cache,
            &bindings,
            &mut saved,
            &mut subscribers
        ));
        // 自动写入的绑定与公开身份进入待持久化状态。
        assert_eq!(
            saved.bindings.get("wT:p9").map(String::as_str),
            Some("claude:default")
        );
        assert_eq!(
            saved.identities.get("claude:default").map(String::as_str),
            Some("me@example.test")
        );
        // 只有匹配该 pane 的订阅者收到事件，订阅本身都保留。
        let text = bound_receiver.try_recv().unwrap_or_default();
        let event: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
        assert_eq!(event["event"], "account.usage.updated");
        assert_eq!(event["data"]["accounts"][0]["account_id"], "claude:default");
        assert_eq!(event["data"]["accounts"][0]["status"], "ready");
        assert!(other_receiver.try_recv().is_err(), "不匹配的订阅者不收事件");
        assert_eq!(subscribers.len(), 2);

        // 缓存里没有该账号时不落盘。
        let missing = apply::Accepted {
            account_id: "nope".into(),
            auto_bound_pane: None,
        };
        assert!(!commit_accepted(
            &missing,
            &cache,
            &bindings,
            &mut saved,
            &mut subscribers
        ));
    }
}
