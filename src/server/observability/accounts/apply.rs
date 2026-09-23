//! `account.usage.report` 与 `account.binding.set` 的纯函数落地：解析官方回调与 herdr
//! 集成扩展的推送、校验绑定并写入缓存。这里不做 I/O：持久化、订阅推送与日志由 `mod.rs` 的服务
//! 循环负责。

use std::collections::{HashMap, VecDeque};

use super::{invalidate_changed_identity, parse, registry, CacheEntry};
use crate::api::schema::*;
use crate::config::UsageAccountConfig;

/// 被拒 (pane, agent) 待办队列的容量；超出时淘汰最早的条目。
const PENDING_CAPACITY: usize = 64;
/// 待办条目的保留时长：pane 早已关闭的待办不再有意义。
const PENDING_TTL_MS: u64 = 60 * 60 * 1000;
/// 同一 (pane, agent, 错误码) 重复被拒时，再次按 info 级别记录日志的最小间隔。
const REJECTION_LOG_INTERVAL_MS: u64 = 10 * 60 * 1000;
/// 拒绝日志限速表的容量；条目按时间淘汰，这里只是防御性的上限。
const REJECTION_LOG_CAPACITY: usize = 64;
/// 绑定表上限：`account.binding.set` 与自动绑定共用同一真源。
pub(super) const MAX_BINDINGS: usize = 1024;
pub(super) const MAX_PANE_ID_LEN: usize = 256;
/// 日志字段里未知 agent 别名的最大长度；已知别名记规范化后的厂商名。
const MAX_AGENT_LOG_LEN: usize = 64;

const BINDING_REQUIRED_MESSAGE: &str = "请先在账号用量页面为此窗格绑定对应厂商账号";
const INVALID_REPORT_MESSAGE: &str = "账号用量报告无效";
const AUTO_BOUND_MESSAGE: &str = "已按唯一账号自动绑定";
const NO_QUOTA_MESSAGE: &str =
    "官方回调暂无额度字段（statusline 未提供 rate_limits），等待下一次回调或探测";
const NO_PUSH_USAGE_MESSAGE: &str = "集成扩展的推送暂无用量字段，等待下一次推送";
/// 本机数据库轮询型来源（zcode）收到上报时的拒绝说明。
const LOCAL_SOURCE_REPORT_MESSAGE: &str = "此来源只由 server 只读本机数据库取数，不接受上报";
const CALLBACK_SOURCE: &str = "官方 CLI 回调";
const EXTENSION_PUSH_SOURCE: &str = "herdr 集成扩展推送 · 会话统计，非账号额度";

/// `apply_report` 的只读输入。
#[derive(Clone, Copy)]
pub(super) struct Context<'a> {
    pub accounts: &'a [UsageAccountConfig],
    /// `account_usage.enabled`：关闭时不自动绑定，未绑定 pane 沿用拒绝路径。
    pub enabled: bool,
    pub now_ms: u64,
}

/// 一条被拒绝的未绑定回调；后续响应的 `pending_binding` 字段从这里取数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PendingBinding {
    pub pane_id: String,
    pub agent: String,
    pub candidates: Vec<String>,
    pub rejected_at_ms: u64,
}

/// 已按 info 记录过的拒绝，用于限速。
struct LoggedRejection {
    pane_id: String,
    agent: String,
    code: &'static str,
    logged_at_ms: u64,
}

/// 被拒回调的记账：未绑定 pane 的待办队列 + 全部拒绝出口共用的日志限速表。
#[derive(Default)]
pub(super) struct Rejections {
    pending: VecDeque<PendingBinding>,
    logged: VecDeque<LoggedRejection>,
}

impl From<&PendingBinding> for UsagePendingBinding {
    fn from(pending: &PendingBinding) -> Self {
        Self {
            pane_id: pending.pane_id.clone(),
            agent: pending.agent.clone(),
            candidates: pending.candidates.clone(),
            rejected_at_ms: pending.rejected_at_ms,
        }
    }
}

impl Rejections {
    /// 记一条待办：按 (pane, agent) 去重，按容量与时长淘汰。
    pub(super) fn record_pending(
        &mut self,
        pane_id: &str,
        agent: &str,
        candidates: Vec<String>,
        now_ms: u64,
    ) {
        if pane_id.len() > MAX_PANE_ID_LEN {
            // 这样的 pane 永远绑不上（`bind_pane` 同样拒绝），不值得占队列。
            return;
        }
        self.pending
            .retain(|item| now_ms.saturating_sub(item.rejected_at_ms) < PENDING_TTL_MS);
        self.pending
            .retain(|item| !(item.pane_id == pane_id && item.agent == agent));
        self.pending.push_back(PendingBinding {
            pane_id: pane_id.into(),
            agent: agent.into(),
            candidates,
            rejected_at_ms: now_ms,
        });
        while self.pending.len() > PENDING_CAPACITY {
            self.pending.pop_front();
        }
    }

    /// 绑定落地后移除该 pane 的待办条目。
    pub(super) fn forget_pane(&mut self, pane_id: &str) {
        self.pending.retain(|item| item.pane_id != pane_id);
    }

    /// 供响应侧取用的待办：指定 pane 时只看该 pane，否则取该 agent 最近一条；过期条目不算。
    pub(super) fn pending_for(
        &self,
        pane_id: Option<&str>,
        agent: &str,
        now_ms: u64,
    ) -> Option<&PendingBinding> {
        self.pending.iter().rev().find(|item| {
            item.agent == agent
                && pane_id.is_none_or(|pane| item.pane_id == pane)
                && now_ms.saturating_sub(item.rejected_at_ms) < PENDING_TTL_MS
        })
    }

    /// 同一 (pane, agent, 错误码) 在限速窗口内只按 info 记一次；返回本次是否该记 info。
    fn should_log(&mut self, origin: &Origin, code: &'static str, now_ms: u64) -> bool {
        self.logged
            .retain(|item| now_ms.saturating_sub(item.logged_at_ms) < REJECTION_LOG_INTERVAL_MS);
        if self.logged.iter().any(|item| {
            item.code == code && item.pane_id == origin.pane_id && item.agent == origin.agent
        }) {
            return false;
        }
        self.logged.push_back(LoggedRejection {
            pane_id: origin.pane_id.clone(),
            agent: origin.agent.clone(),
            code,
            logged_at_ms: now_ms,
        });
        while self.logged.len() > REJECTION_LOG_CAPACITY {
            self.logged.pop_front();
        }
        true
    }

    /// 组装一次拒绝：错误码经过限速表决定 `repeated`，日志字段取自 `origin`。
    fn reject(
        &mut self,
        origin: &Origin,
        code: &'static str,
        message: String,
        now_ms: u64,
    ) -> Rejected {
        let repeated = !self.should_log(origin, code, now_ms);
        Rejected {
            code,
            message,
            repeated,
            pane_id: origin.pane_id.clone(),
            agent: origin.agent.clone(),
        }
    }

    #[cfg(test)]
    pub(super) fn pending(&self) -> impl Iterator<Item = &PendingBinding> {
        self.pending.iter()
    }
}

/// 拒绝日志与限速用的来源键：pane 截断、agent 规范化，两者都不带原始报文。
struct Origin {
    pane_id: String,
    agent: String,
}

impl Origin {
    fn new(
        pane_id: Option<&str>,
        agent: Option<&str>,
        provider: Option<&registry::Provider>,
    ) -> Self {
        Self {
            pane_id: pane_id
                .unwrap_or_default()
                .chars()
                .take(MAX_PANE_ID_LEN)
                .collect(),
            agent: provider
                .map(|provider| provider.agent.to_owned())
                .unwrap_or_else(|| {
                    agent
                        .unwrap_or_default()
                        .chars()
                        .take(MAX_AGENT_LOG_LEN)
                        .collect()
                }),
        }
    }
}

/// 报告被接受：缓存已更新，调用方负责持久化与订阅推送。
pub(super) struct Accepted {
    pub account_id: String,
    /// 本次按唯一候选自动写入绑定的 pane。
    pub auto_bound_pane: Option<String>,
    /// 缓存是否真的被改写；报文合法但没有额度字段时为 false，调用方不推送不落盘。
    pub updated: bool,
}

/// 报告被拒绝：`code` 直接作为 API 错误码返回。
pub(super) struct Rejected {
    pub code: &'static str,
    pub message: String,
    /// 同一 (pane, agent, code) 在限速窗口内已记录过拒绝日志，调用方降级日志级别。
    pub repeated: bool,
    /// 供日志定位的 pane（截断）与规范化后的 agent。
    pub pane_id: String,
    pub agent: String,
}

fn binding_required_message(candidates: &[String]) -> String {
    if candidates.is_empty() {
        BINDING_REQUIRED_MESSAGE.into()
    } else {
        format!(
            "{BINDING_REQUIRED_MESSAGE}；候选账号：{}",
            candidates.join("、")
        )
    }
}

/// 把一次 `account.usage.report` 落到缓存：解析官方报文、校验绑定并更新条目。
///
/// 未绑定 pane 且 agent 在 `accounts` 中恰好只有一个账号时自动写入绑定并接受；
/// 多个候选才拒绝并在 message 附候选列表，同时记入待办队列。
pub(super) fn apply_report(
    mut params: UsageReportParams,
    context: Context<'_>,
    bindings: &mut HashMap<String, String>,
    cache: &mut HashMap<String, CacheEntry>,
    rejections: &mut Rejections,
    next_query: &mut u64,
) -> Result<Accepted, Rejected> {
    let Context {
        accounts,
        enabled,
        now_ms,
    } = context;
    let provider = params.agent.as_deref().and_then(registry::provider);
    let origin = Origin::new(params.pane_id.as_deref(), params.agent.as_deref(), provider);
    if params.account_id.is_empty() {
        params.account_id = params
            .pane_id
            .as_ref()
            .and_then(|pane| bindings.get(pane))
            .cloned()
            .unwrap_or_default();
    }
    // 本机数据库轮询型来源（zcode）不跑在 pane 里、也没有推送：外来上报会置回调闩锁、把
    // 本地轮询压住，也不该为它记绑定待办。报文的 agent 与目标账号的 agent 两头都判。
    let target = accounts
        .iter()
        .find(|account| account.id == params.account_id)
        .and_then(|account| registry::provider(&account.agent));
    if provider
        .into_iter()
        .chain(target)
        .any(|provider| !registry::accepts_reports(provider))
    {
        return Err(rejections.reject(
            &origin,
            "invalid_usage_report",
            LOCAL_SOURCE_REPORT_MESSAGE.into(),
            now_ms,
        ));
    }
    // 唯一候选自动绑定：这里只选定候选；绑定要等报文解析出额度、全部校验通过后
    // 才写入，被拒的报文不得留下绑定。账号用量开关关闭时沿用旧的拒绝路径。
    let mut auto_bind_pane: Option<String> = None;
    // pi 扩展报文里的当前服务商：账号配置没有钉死计费厂商时写进快照的 `provider`。
    let mut reported_provider: Option<String> = None;
    if params.account_id.is_empty() && enabled {
        if let (Some(pane), Some(provider)) = (params.pane_id.as_deref(), provider) {
            let candidates = accounts
                .iter()
                .filter(|account| account.agent == provider.agent)
                .map(|account| account.id.clone())
                .collect::<Vec<_>>();
            match candidates.as_slice() {
                [only]
                    if params.official_payload.is_some()
                        && bindings.len() < MAX_BINDINGS
                        && pane.len() <= MAX_PANE_ID_LEN =>
                {
                    params.account_id = only.clone();
                    auto_bind_pane = Some(pane.to_owned());
                }
                _ => {
                    let message = binding_required_message(&candidates);
                    rejections.record_pending(pane, provider.agent, candidates, now_ms);
                    return Err(rejections.reject(
                        &origin,
                        "usage_binding_required",
                        message,
                        now_ms,
                    ));
                }
            }
        }
    }
    if let Some(payload) = params.official_payload.take() {
        if !provider.is_some_and(|provider| {
            accounts
                .iter()
                .any(|account| account.id == params.account_id && account.agent == provider.agent)
        }) {
            return Err(rejections.reject(
                &origin,
                "usage_binding_required",
                BINDING_REQUIRED_MESSAGE.into(),
                now_ms,
            ));
        }
        params.snapshot.metrics = match provider.map(|provider| provider.agent) {
            Some("claude") => {
                let fresh = parse::claude(&payload);
                // 官方 statusline 会把过了 `resets_at` 的窗口从 JSON 里去掉，会话首个响应之前
                // 也整段缺省：缺席不是归零，沿用该账号上次的窗口（过期的标明）。报文本身什么
                // 都没解析出来时不沿用，免得用旧数据刷新观测时间。
                match cache.get(&params.account_id) {
                    Some(entry) if !fresh.is_empty() => parse::claude_retain_missing_windows(
                        fresh,
                        &entry.snapshot.metrics,
                        now_ms / 1000,
                    ),
                    _ => fresh,
                }
            }
            Some("kimi") => parse::kimi(&payload),
            Some("codex") => parse::codex(&payload),
            Some("pi") => {
                reported_provider = parse::pi_provider_model(&payload).0;
                parse::pi(&payload)
            }
            Some("opencode") => parse::structured(&payload, "session"),
            _ => Vec::new(),
        };
        // 官方 statusline 回调接管账号靠的是账号额度：报文只有会话级指标（旧版 statusline 无
        // `rate_limits`、API key 登录，或会话首个响应之前且没有可沿用的窗口）时按「暂无额度
        // 字段」处理——不自动绑定、不置回调闩锁，否则该账号不再回落探测。
        if provider.is_some_and(registry::supports_callback)
            && !params
                .snapshot
                .metrics
                .iter()
                .any(|metric| metric.scope == "account")
        {
            params.snapshot.metrics.clear();
        }
        params.snapshot.message = None;
    }
    if let (Some(pane), Some(provider)) = (auto_bind_pane.as_deref(), provider) {
        if params.snapshot.metrics.is_empty() {
            // 报文里解析不出额度字段（如旧版 statusline 无 rate_limits）：不写绑定、不置
            // callback，否则该账号会被回调闩锁卡在 Unavailable 且不再探测。留给用户显式绑定。
            let candidates = vec![params.account_id.clone()];
            let message = format!(
                "{BINDING_REQUIRED_MESSAGE}；官方报文暂无额度字段，未按唯一账号 {} 自动绑定",
                params.account_id
            );
            rejections.record_pending(pane, provider.agent, candidates, now_ms);
            return Err(rejections.reject(&origin, "usage_binding_required", message, now_ms));
        }
    }
    // 自动绑定的 pane 此刻尚未写入绑定表，绑定一致性检查只针对既有绑定。
    let bound_elsewhere = auto_bind_pane.is_none()
        && params
            .pane_id
            .as_ref()
            .is_some_and(|pane| bindings.get(pane) != Some(&params.account_id));
    // 过滤语义：不合规的单条指标只丢它自己，其余照常接受；报文里有指标但没有一条合规才是
    // 无效上报（整份拒收会让主路径的合法额度因一条脏 label 全部丢失）。
    let had_metrics = !params.snapshot.metrics.is_empty();
    let dropped = parse::retain_valid(&mut params.snapshot.metrics);
    if dropped > 0 {
        tracing::debug!(
            event = "account.report.metrics_dropped",
            subsystem = "account_usage",
            outcome = "dropped",
            account = %params.account_id,
            dropped,
            "丢弃回调上报里不合规的用量指标"
        );
    }
    let all_invalid = had_metrics && params.snapshot.metrics.is_empty();
    if !cache.contains_key(&params.account_id) || all_invalid || bound_elsewhere {
        return Err(rejections.reject(
            &origin,
            "invalid_usage_report",
            INVALID_REPORT_MESSAGE.into(),
            now_ms,
        ));
    }
    if params.snapshot.metrics.is_empty() {
        // 报文合法但没有额度字段（如旧版 statusline 无 rate_limits）：不覆盖缓存、不置回调
        // 闩锁——否则账号会被钉在 Unavailable 且不再探测。只在账号还没有任何结论（Warming
        // 且无样本）时留一句说明；终态/错误的诊断信息不被替换，避免 status 与 message 矛盾。
        if let Some(entry) = cache.get_mut(&params.account_id) {
            if entry.snapshot.status == ObservationStatus::Warming
                && entry.snapshot.metrics.is_empty()
            {
                entry.snapshot.message = Some(
                    if provider.is_some_and(registry::supports_extension_push) {
                        NO_PUSH_USAGE_MESSAGE
                    } else {
                        NO_QUOTA_MESSAGE
                    }
                    .into(),
                );
            }
        }
        return Ok(Accepted {
            account_id: params.account_id,
            auto_bound_pane: None,
            updated: false,
        });
    }
    let mut snapshot = params.snapshot;
    snapshot.account_id = params.account_id.clone();
    snapshot.observed_at_ms = now_ms;
    if let Some(account) = accounts.iter().find(|a| a.id == params.account_id) {
        snapshot.agent = account.agent.clone();
        snapshot.provider = account.provider.clone();
        snapshot.auth_mode = account.auth_mode.clone();
        snapshot.account_label = account.label.clone();
        snapshot.source_url = registry::provider(&account.agent)
            .map(|p| p.source)
            .unwrap_or_default()
            .into();
    }
    // 多服务商 CLI（pi）的实际计费厂商随会话变化：账号配置没有钉死时取扩展报上来的当前
    // 服务商，供客户端把这份会话统计关联到对应订阅账号的额度，而不是重复计量。
    // 「没有钉死」有两种形态：`provider` 留空，或等于 agent 名——`configured_accounts` 给
    // 隐式默认账号预填的就是 agent 名（`pi:default` 的 provider 是 "pi"），那不是计费厂商，
    // 只判空会让绝大多数用户（装完 pi 直接用默认账号）永远拿不到关联。
    if snapshot.provider.is_empty() || provider.is_some_and(|p| snapshot.provider == p.agent) {
        if let Some(reported) = reported_provider {
            snapshot.provider = reported;
        }
    }
    snapshot.source = if provider.is_some_and(registry::supports_extension_push) {
        EXTENSION_PUSH_SOURCE
    } else {
        CALLBACK_SOURCE
    }
    .into();
    snapshot.status = ObservationStatus::Ready;
    if snapshot.account_identity.is_none() {
        snapshot.account_identity = cache
            .get(&params.account_id)
            .and_then(|entry| entry.snapshot.account_identity.clone());
    }
    // 上面已确认缓存含该账号；这里的 else 只是避免 unwrap。
    let Some(entry) = cache.get_mut(&params.account_id) else {
        return Err(rejections.reject(
            &origin,
            "invalid_usage_report",
            INVALID_REPORT_MESSAGE.into(),
            now_ms,
        ));
    };
    let identity_changed = invalidate_changed_identity(&entry.snapshot, &mut snapshot, bindings);
    let auto_bound_pane = match auto_bind_pane {
        // 官方身份已变：该账号的绑定刚被整体撤销，不再写入也不宣称「已自动绑定」。
        Some(_) if identity_changed => None,
        Some(pane) => {
            bindings.insert(pane.clone(), params.account_id.clone());
            rejections.forget_pane(&pane);
            snapshot.message = Some(AUTO_BOUND_MESSAGE.into());
            Some(pane)
        }
        None => None,
    };
    entry.generation = *next_query;
    *next_query = next_query.saturating_add(1);
    entry.in_flight = false;
    entry.queued = false;
    // 官方回调接管该账号：一个闩锁周期内不回落探测，成功一次即清零失败与终态；回调能到
    // 说明不再被交互探测的目录信任对话阻塞，旁路标记一并清掉。
    entry.callback_until_ms = Some(now_ms.saturating_add(super::CALLBACK_LATCH_MS));
    entry.failures = 0;
    entry.terminal = None;
    entry.trust_required = false;
    entry.snapshot = snapshot;
    Ok(Accepted {
        account_id: params.account_id,
        auto_bound_pane,
        updated: true,
    })
}

/// 把一次 `account.binding.set` 落到绑定表：校验账号与上限，成功后清掉该 pane 的待办。
pub(super) fn bind_pane(
    pane_id: &str,
    account_id: &str,
    accounts: &[UsageAccountConfig],
    bindings: &mut HashMap<String, String>,
    rejections: &mut Rejections,
) -> Result<(), (&'static str, String)> {
    if !accounts.iter().any(|account| account.id == account_id) {
        return Err(("unknown_account", "未找到配置的账号".into()));
    }
    if bindings.len() >= MAX_BINDINGS || pane_id.len() > MAX_PANE_ID_LEN {
        return Err(("invalid_binding", "账号绑定超出限制".into()));
    }
    bindings.insert(pane_id.into(), account_id.into());
    rejections.forget_pane(pane_id);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::empty_snapshot;
    use super::*;
    use crate::config::AccountUsageConfig;
    use serde_json::json;

    const NOW_MS: u64 = 1_700_000_000_000;

    fn account(id: &str, agent: &str) -> UsageAccountConfig {
        UsageAccountConfig {
            id: id.into(),
            label: format!("{agent} 账号"),
            agent: agent.into(),
            provider: agent.into(),
            auth_mode: "cli".into(),
            ..Default::default()
        }
    }

    fn cache_for(accounts: &[UsageAccountConfig]) -> HashMap<String, CacheEntry> {
        accounts
            .iter()
            .map(|account| (account.id.clone(), CacheEntry::new(empty_snapshot(account))))
            .collect()
    }

    fn claude_payload() -> serde_json::Value {
        json!({
            "rate_limits": {
                "five_hour": {"used_percentage": 42, "resets_at": 1_700_000_000},
                "seven_day": {"used_percentage": 10}
            }
        })
    }

    fn report(pane: Option<&str>, account_id: &str) -> UsageReportParams {
        UsageReportParams {
            account_id: account_id.into(),
            pane_id: pane.map(Into::into),
            agent: Some("claude".into()),
            official_payload: Some(claude_payload()),
            snapshot: Default::default(),
        }
    }

    struct Fixture {
        accounts: Vec<UsageAccountConfig>,
        bindings: HashMap<String, String>,
        cache: HashMap<String, CacheEntry>,
        rejections: Rejections,
        next_query: u64,
        enabled: bool,
        now_ms: u64,
    }

    impl Fixture {
        fn new(accounts: Vec<UsageAccountConfig>) -> Self {
            let cache = cache_for(&accounts);
            Self {
                accounts,
                bindings: HashMap::new(),
                cache,
                rejections: Rejections::default(),
                next_query: 7,
                enabled: true,
                now_ms: NOW_MS,
            }
        }

        fn apply(&mut self, params: UsageReportParams) -> Result<Accepted, Rejected> {
            apply_report(
                params,
                Context {
                    accounts: &self.accounts,
                    enabled: self.enabled,
                    now_ms: self.now_ms,
                },
                &mut self.bindings,
                &mut self.cache,
                &mut self.rejections,
                &mut self.next_query,
            )
        }

        fn bind(&mut self, pane: &str, account_id: &str) -> Result<(), &'static str> {
            bind_pane(
                pane,
                account_id,
                &self.accounts,
                &mut self.bindings,
                &mut self.rejections,
            )
            .map_err(|(code, _)| code)
        }

        fn pending_panes(&self) -> Vec<&str> {
            self.rejections
                .pending()
                .map(|item| item.pane_id.as_str())
                .collect()
        }
    }

    fn code(result: &Result<Accepted, Rejected>) -> &'static str {
        match result {
            Ok(_) => "ok",
            Err(rejected) => rejected.code,
        }
    }

    fn rejected(result: Result<Accepted, Rejected>) -> Rejected {
        match result {
            Ok(accepted) => panic!("不应接受：账号 {}", accepted.account_id),
            Err(rejected) => rejected,
        }
    }

    // ---- characterization：抽出纯函数前后的行为保持一致 ----

    #[test]
    fn bound_pane_report_is_accepted_and_marks_callback() {
        let mut fixture = Fixture::new(vec![account("claude:default", "claude")]);
        fixture
            .bindings
            .insert("pane-1".into(), "claude:default".into());
        let result = fixture.apply(report(Some("pane-1"), ""));
        let accepted = result.unwrap_or_else(|rejected| panic!("被拒: {}", rejected.code));
        assert_eq!(accepted.account_id, "claude:default");
        assert!(accepted.auto_bound_pane.is_none());
        let entry = &fixture.cache["claude:default"];
        assert!(entry.callback_latched());
        assert_eq!(
            entry.callback_until_ms,
            Some(NOW_MS + super::super::CALLBACK_LATCH_MS)
        );
        assert!(!entry.in_flight);
        assert_eq!(entry.failures, 0);
        assert_eq!(entry.generation, 7);
        assert_eq!(fixture.next_query, 8);
        assert_eq!(entry.snapshot.status, ObservationStatus::Ready);
        assert_eq!(entry.snapshot.metrics.len(), 2);
        assert_eq!(entry.snapshot.metrics[0].used_percent, Some(42.0));
        assert_eq!(entry.snapshot.observed_at_ms, NOW_MS);
        assert_eq!(entry.snapshot.source, CALLBACK_SOURCE);
        assert_eq!(entry.snapshot.agent, "claude");
        assert_eq!(entry.snapshot.message, None);
        assert!(fixture.pending_panes().is_empty());
    }

    #[test]
    fn accepted_report_clears_the_trust_required_flag_left_by_a_probe() {
        let mut fixture = Fixture::new(vec![account("claude:default", "claude")]);
        fixture
            .bindings
            .insert("pane-1".into(), "claude:default".into());
        if let Some(entry) = fixture.cache.get_mut("claude:default") {
            entry.trust_required = true;
            entry.failures = 3;
        }
        let result = fixture.apply(report(Some("pane-1"), ""));
        assert!(result.is_ok(), "{}", code(&result));
        let entry = &fixture.cache["claude:default"];
        assert!(
            !entry.trust_required,
            "官方回调成功即证明不再被信任对话阻塞"
        );
        assert_eq!(entry.failures, 0);
        assert!(entry.terminal.is_none());
    }

    #[test]
    fn explicit_account_that_differs_from_the_binding_is_invalid() {
        let mut fixture = Fixture::new(vec![
            account("claude:work", "claude"),
            account("claude:home", "claude"),
        ]);
        fixture
            .bindings
            .insert("pane-1".into(), "claude:home".into());
        let result = fixture.apply(report(Some("pane-1"), "claude:work"));
        assert_eq!(code(&result), "invalid_usage_report");
        assert!(!fixture.cache["claude:work"].callback_latched());
        assert!(!fixture.cache["claude:home"].callback_latched());
        assert_eq!(fixture.bindings["pane-1"], "claude:home");
    }

    #[test]
    fn binding_to_an_account_of_another_agent_requires_rebinding() {
        let mut fixture = Fixture::new(vec![
            account("claude:default", "claude"),
            account("kimi:default", "kimi"),
        ]);
        fixture
            .bindings
            .insert("pane-1".into(), "kimi:default".into());
        let result = fixture.apply(report(Some("pane-1"), ""));
        assert_eq!(code(&result), "usage_binding_required");
        assert!(!fixture.cache["kimi:default"].callback_latched());
    }

    #[test]
    fn unknown_agent_or_missing_pane_requires_binding() {
        let mut fixture = Fixture::new(vec![account("claude:default", "claude")]);
        let mut unknown = report(Some("pane-1"), "");
        unknown.agent = Some("no-such-agent".into());
        assert_eq!(code(&fixture.apply(unknown)), "usage_binding_required");

        let rejected = rejected(fixture.apply(report(None, "")));
        assert_eq!(rejected.code, "usage_binding_required");
        assert_eq!(rejected.message, BINDING_REQUIRED_MESSAGE);
        assert!(fixture.bindings.is_empty());
    }

    #[test]
    fn one_invalid_metric_is_dropped_and_the_rest_of_the_report_is_accepted() {
        let mut fixture = Fixture::new(vec![account("claude:default", "claude")]);
        fixture
            .bindings
            .insert("pane-1".into(), "claude:default".into());
        let mut params = report(Some("pane-1"), "");
        // seven_day 的 unit 超长（不合规），five_hour 合规：只丢前者，不整份拒收。
        params.official_payload = Some(json!({
            "rate_limits": {
                "five_hour": {"used_percentage": 42},
                "seven_day": {"used_percentage": 10, "unit": "x".repeat(65)}
            }
        }));
        let result = fixture.apply(params);
        assert_eq!(code(&result), "ok");
        let entry = &fixture.cache["claude:default"];
        assert_eq!(
            entry.snapshot.metrics.len(),
            1,
            "{:#?}",
            entry.snapshot.metrics
        );
        assert_eq!(entry.snapshot.metrics[0].id, "five_hour");
        assert!(entry.callback_latched());
        // 厂商 JSON 里带制表符的 unit 会在解析时清洗，而不是被拒。
        let mut params = report(Some("pane-1"), "");
        params.official_payload = Some(json!({
            "rate_limits": {"five_hour": {"used_percentage": 42, "unit": "req\tuests"}}
        }));
        let result = fixture.apply(params);
        assert_eq!(code(&result), "ok");
        assert_eq!(
            fixture.cache["claude:default"].snapshot.metrics[0].unit,
            "req uests"
        );
    }

    #[test]
    fn invalid_metrics_are_rejected_after_binding_checks() {
        let mut fixture = Fixture::new(vec![account("claude:default", "claude")]);
        fixture
            .bindings
            .insert("pane-1".into(), "claude:default".into());
        let mut params = report(Some("pane-1"), "");
        params.official_payload = Some(json!({
            "rate_limits": {"five_hour": {"used_percentage": 42, "unit": "x".repeat(65)}}
        }));
        assert_eq!(code(&fixture.apply(params)), "invalid_usage_report");
        assert!(!fixture.cache["claude:default"].callback_latched());
    }

    #[test]
    fn explicit_account_without_pane_is_accepted_without_binding() {
        let mut fixture = Fixture::new(vec![account("claude:default", "claude")]);
        let result = fixture.apply(report(None, "claude:default"));
        assert_eq!(code(&result), "ok");
        assert!(fixture.bindings.is_empty());
        assert!(fixture.cache["claude:default"].callback_latched());
    }

    #[test]
    fn empty_payload_report_keeps_the_cached_snapshot_and_does_not_latch_callback() {
        // B-6：报文没有 rate_limits 时接受但不改写缓存；没有样本时只留一句说明。
        let mut fixture = Fixture::new(vec![account("claude:default", "claude")]);
        let mut params = report(None, "claude:default");
        params.official_payload = Some(json!({"cost": {"total_cost_usd": 1.5}}));
        let accepted = fixture
            .apply(params.clone())
            .unwrap_or_else(|rejected| panic!("被拒: {}", rejected.code));
        assert!(!accepted.updated);
        let entry = &fixture.cache["claude:default"];
        assert!(!entry.callback_latched());
        assert!(entry.snapshot.metrics.is_empty());
        assert_eq!(entry.snapshot.status, ObservationStatus::Warming);
        assert_eq!(entry.snapshot.message.as_deref(), Some(NO_QUOTA_MESSAGE));
        assert_eq!(fixture.next_query, 7, "未改写缓存不消耗 generation");

        // 已有真实额度时，什么都解析不出来的报文连 message 都不碰。
        assert_eq!(code(&fixture.apply(report(None, "claude:default"))), "ok");
        let mut blank = report(None, "claude:default");
        blank.official_payload = Some(json!({"model": {"display_name": "Opus"}}));
        let accepted = fixture
            .apply(blank)
            .unwrap_or_else(|rejected| panic!("被拒: {}", rejected.code));
        assert!(!accepted.updated, "不拿旧数据刷新观测时间");
        let entry = &fixture.cache["claude:default"];
        assert!(entry.callback_latched());
        assert_eq!(entry.snapshot.status, ObservationStatus::Ready);
        assert_eq!(entry.snapshot.metrics.len(), 2);
        assert_eq!(entry.snapshot.message, None);

        // 只带会话级字段的报文（没有 rate_limits）不会把已有额度清零：窗口沿用，会话级指标更新。
        let accepted = fixture
            .apply(params)
            .unwrap_or_else(|rejected| panic!("被拒: {}", rejected.code));
        assert!(accepted.updated);
        let entry = &fixture.cache["claude:default"];
        assert_eq!(
            entry
                .snapshot
                .metrics
                .iter()
                .map(|metric| metric.id.as_str())
                .collect::<Vec<_>>(),
            vec!["five_hour", "seven_day", "cost/total_cost_usd"]
        );
        assert_eq!(entry.snapshot.metrics[0].used_percent, Some(42.0));
        assert_eq!(entry.snapshot.status, ObservationStatus::Ready);
        assert_eq!(entry.snapshot.message, None);
    }

    #[test]
    fn empty_payload_report_never_rewrites_a_terminal_diagnostic() {
        // 已处于「需要登录」等终态的账号收到无额度字段的回调：status 与 message 都不动，
        // 否则会出现「需要登录 + 官方回调暂无额度字段」互相矛盾的一行。
        let mut fixture = Fixture::new(vec![account("claude:default", "claude")]);
        if let Some(entry) = fixture.cache.get_mut("claude:default") {
            entry.snapshot.status = ObservationStatus::NotAuthenticated;
            entry.snapshot.message = Some("需要登录".into());
        }
        let mut params = report(None, "claude:default");
        params.official_payload = Some(json!({"cost": {"total_cost_usd": 1.5}}));
        assert_eq!(code(&fixture.apply(params)), "ok");
        let entry = &fixture.cache["claude:default"];
        assert_eq!(entry.snapshot.status, ObservationStatus::NotAuthenticated);
        assert_eq!(entry.snapshot.message.as_deref(), Some("需要登录"));
        assert!(!entry.callback_latched());
    }

    // ---- 唯一候选自动绑定 ----

    #[test]
    fn unbound_pane_with_a_single_candidate_is_auto_bound() {
        let mut fixture = Fixture::new(vec![
            account("claude:default", "claude"),
            account("kimi:default", "kimi"),
        ]);
        let result = fixture.apply(report(Some("wT:p9"), ""));
        let accepted = result.unwrap_or_else(|rejected| panic!("被拒: {}", rejected.code));
        assert_eq!(accepted.account_id, "claude:default");
        assert_eq!(accepted.auto_bound_pane.as_deref(), Some("wT:p9"));
        assert_eq!(fixture.bindings["wT:p9"], "claude:default");
        let entry = &fixture.cache["claude:default"];
        assert!(entry.callback_latched());
        assert_eq!(entry.snapshot.status, ObservationStatus::Ready);
        assert_eq!(entry.snapshot.metrics.len(), 2);
        assert_eq!(entry.snapshot.message.as_deref(), Some(AUTO_BOUND_MESSAGE));
        assert!(!fixture.cache["kimi:default"].callback_latched());
        assert!(fixture.pending_panes().is_empty());

        // 下一次回调已能直接命中绑定，不再标自动绑定。
        let result = fixture.apply(report(Some("wT:p9"), ""));
        let accepted = result.unwrap_or_else(|rejected| panic!("被拒: {}", rejected.code));
        assert!(accepted.auto_bound_pane.is_none());
        assert_eq!(fixture.cache["claude:default"].snapshot.message, None);
    }

    #[test]
    fn auto_binding_clears_the_pending_entry_of_that_pane() {
        let mut fixture = Fixture::new(vec![account("claude:default", "claude")]);
        let mut params = report(Some("wT:p9"), "");
        params.official_payload = None;
        assert_eq!(code(&fixture.apply(params)), "usage_binding_required");
        assert_eq!(fixture.pending_panes(), vec!["wT:p9"]);
        assert_eq!(code(&fixture.apply(report(Some("wT:p9"), ""))), "ok");
        assert!(fixture.pending_panes().is_empty());
    }

    #[test]
    fn unbound_pane_with_several_candidates_lists_them_and_queues_the_pane() {
        let mut fixture = Fixture::new(vec![
            account("claude:work", "claude"),
            account("claude:home", "claude"),
        ]);
        let rejected = rejected(fixture.apply(report(Some("wT:p9"), "")));
        assert_eq!(rejected.code, "usage_binding_required");
        assert_eq!(
            rejected.message,
            format!("{BINDING_REQUIRED_MESSAGE}；候选账号：claude:work、claude:home")
        );
        assert!(!rejected.repeated);
        assert!(fixture.bindings.is_empty());
        assert!(!fixture.cache["claude:work"].callback_latched());
        assert!(!fixture.cache["claude:home"].callback_latched());
        let pending = fixture.rejections.pending().collect::<Vec<_>>();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].pane_id, "wT:p9");
        assert_eq!(pending[0].agent, "claude");
        assert_eq!(pending[0].candidates, vec!["claude:work", "claude:home"]);
        assert_eq!(pending[0].rejected_at_ms, NOW_MS);

        // 同一 pane 重复被拒：仍拒绝，但标记为重复以便日志限速。
        let again = super::tests::rejected(fixture.apply(report(Some("wT:p9"), "")));
        assert!(again.repeated);
        assert_eq!(fixture.rejections.pending().count(), 1);
    }

    #[test]
    fn agent_without_configured_accounts_is_rejected_without_candidates() {
        let mut fixture = Fixture::new(vec![account("kimi:default", "kimi")]);
        let rejected = rejected(fixture.apply(report(Some("wT:p9"), "")));
        assert_eq!(rejected.code, "usage_binding_required");
        assert_eq!(rejected.message, BINDING_REQUIRED_MESSAGE);
        assert!(fixture.bindings.is_empty());
        let pending = fixture.rejections.pending().collect::<Vec<_>>();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].candidates.is_empty());
    }

    #[test]
    fn explicit_account_never_triggers_auto_binding() {
        let mut fixture = Fixture::new(vec![account("claude:default", "claude")]);
        let result = fixture.apply(report(Some("wT:p9"), "claude:default"));
        assert_eq!(code(&result), "invalid_usage_report");
        assert!(fixture.bindings.is_empty());
    }

    #[test]
    fn auto_binding_respects_binding_table_limits() {
        let mut fixture = Fixture::new(vec![account("claude:default", "claude")]);
        for index in 0..MAX_BINDINGS {
            fixture
                .bindings
                .insert(format!("pane-{index}"), "claude:default".into());
        }
        assert_eq!(
            code(&fixture.apply(report(Some("wT:p9"), ""))),
            "usage_binding_required"
        );
        assert!(!fixture.bindings.contains_key("wT:p9"));

        let mut fixture = Fixture::new(vec![account("claude:default", "claude")]);
        let long_pane = "p".repeat(MAX_PANE_ID_LEN + 1);
        assert_eq!(
            code(&fixture.apply(report(Some(&long_pane), ""))),
            "usage_binding_required"
        );
        assert!(fixture.bindings.is_empty());
        // 绑不上的超长 pane 也不进待办队列。
        assert!(fixture.pending_panes().is_empty());
    }

    // ---- 自动绑定的门槛：只有能解析出额度的官方回调才配得上一条绑定 ----

    #[test]
    fn auto_binding_requires_an_official_payload() {
        let mut fixture = Fixture::new(vec![account("claude:default", "claude")]);
        let mut params = report(Some("wT:p9"), "");
        params.official_payload = None;
        let rejected = rejected(fixture.apply(params));
        assert_eq!(rejected.code, "usage_binding_required");
        assert!(fixture.bindings.is_empty());
        assert!(!fixture.cache["claude:default"].callback_latched());
        assert_eq!(fixture.pending_panes(), vec!["wT:p9"]);
    }

    #[test]
    fn auto_binding_requires_parsed_metrics() {
        // claude 旧版 statusline 没有 rate_limits：不写绑定、不置 callback，等用户显式绑定，
        // 否则该账号会被 callback 闩锁卡在 Unavailable 且再也不探测。
        let mut fixture = Fixture::new(vec![account("claude:default", "claude")]);
        let mut params = report(Some("wT:p9"), "");
        params.official_payload = Some(json!({"cost": {"total_cost_usd": 1.5}}));
        let rejected = rejected(fixture.apply(params));
        assert_eq!(rejected.code, "usage_binding_required");
        assert!(
            rejected.message.contains("暂无额度字段"),
            "message 应说明原因: {}",
            rejected.message
        );
        assert!(fixture.bindings.is_empty());
        let entry = &fixture.cache["claude:default"];
        assert!(!entry.callback_latched());
        assert_eq!(entry.snapshot.status, ObservationStatus::Warming);
        let pending = fixture.rejections.pending().collect::<Vec<_>>();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].candidates, vec!["claude:default"]);
        assert_eq!(fixture.next_query, 7);
    }

    #[test]
    fn rejected_report_never_leaves_an_auto_binding_behind() {
        let mut fixture = Fixture::new(vec![account("claude:default", "claude")]);
        let mut params = report(Some("wT:p9"), "");
        params.official_payload = Some(json!({
            "rate_limits": {"five_hour": {"used_percentage": 42, "unit": "x".repeat(65)}}
        }));
        assert_eq!(code(&fixture.apply(params)), "invalid_usage_report");
        assert!(fixture.bindings.is_empty());
        assert!(!fixture.cache["claude:default"].callback_latched());
    }

    #[test]
    fn identity_change_revokes_the_auto_binding_before_it_is_reported() {
        let mut fixture = Fixture::new(vec![account("claude:default", "claude")]);
        if let Some(entry) = fixture.cache.get_mut("claude:default") {
            entry.snapshot.account_identity = Some("a@example.test".into());
        }
        let mut params = report(Some("wT:p9"), "");
        params.snapshot.account_identity = Some("b@example.test".into());
        let accepted = fixture
            .apply(params)
            .unwrap_or_else(|rejected| panic!("被拒: {}", rejected.code));
        // 身份已变：绑定被撤销，不得再宣称「已自动绑定」。
        assert!(accepted.auto_bound_pane.is_none());
        assert!(fixture.bindings.is_empty());
        let entry = &fixture.cache["claude:default"];
        assert_eq!(entry.snapshot.status, ObservationStatus::NeedsBinding);
        assert_ne!(entry.snapshot.message.as_deref(), Some(AUTO_BOUND_MESSAGE));
        assert_eq!(
            entry.snapshot.account_identity.as_deref(),
            Some("b@example.test")
        );
    }

    // ---- claude：缺席的额度窗口不是归零 ----

    fn metric<'a>(entry: &'a CacheEntry, id: &str) -> &'a UsageMetric {
        entry
            .snapshot
            .metrics
            .iter()
            .find(|metric| metric.id == id)
            .unwrap_or_else(|| panic!("缺少指标 {id}: {:#?}", entry.snapshot.metrics))
    }

    #[test]
    fn claude_window_dropped_after_its_reset_keeps_the_last_value_as_stale() {
        let mut fixture = Fixture::new(vec![account("claude:default", "claude")]);
        fixture
            .bindings
            .insert("pane-1".into(), "claude:default".into());
        let now_secs = NOW_MS / 1000;
        let mut first = report(Some("pane-1"), "");
        first.official_payload = Some(json!({
            "cost": {"total_cost_usd": 0.5},
            "rate_limits": {
                "five_hour": {"used_percentage": 88, "resets_at": now_secs + 60},
                "seven_day": {"used_percentage": 30, "resets_at": now_secs + 86_400}
            }
        }));
        assert_eq!(code(&fixture.apply(first)), "ok");

        // 两分钟后：five_hour 已过 resets_at，官方 JSON 里不再有它。
        fixture.now_ms = NOW_MS + 120_000;
        let mut second = report(Some("pane-1"), "");
        second.official_payload = Some(json!({
            "cost": {"total_cost_usd": 0.75},
            "rate_limits": {"seven_day": {"used_percentage": 31, "resets_at": now_secs + 86_400}}
        }));
        assert_eq!(code(&fixture.apply(second)), "ok");
        let entry = &fixture.cache["claude:default"];
        let five_hour = metric(entry, "five_hour");
        assert_eq!(five_hour.used_percent, Some(88.0), "保留上次值，不是 0");
        assert_eq!(
            five_hour.text_value.as_deref(),
            Some(parse::CLAUDE_STALE_WINDOW_TEXT)
        );
        assert_eq!(metric(entry, "seven_day").used_percent, Some(31.0));
        assert_eq!(metric(entry, "seven_day").text_value, None);
        assert_eq!(
            metric(entry, "cost/total_cost_usd")
                .amount_decimal
                .as_deref(),
            Some("0.75"),
            "会话级指标取本次报文"
        );
        assert_eq!(entry.snapshot.status, ObservationStatus::Ready);
        assert_eq!(entry.snapshot.observed_at_ms, NOW_MS + 120_000);
    }

    /// 另一个会话在首个 API 响应之前上报：`rate_limits` 整段缺省，但该账号已有未过期的窗口
    /// ——原样沿用、不标过期，会话级指标照常更新。
    #[test]
    fn claude_report_without_rate_limits_keeps_the_unexpired_windows() {
        let mut fixture = Fixture::new(vec![account("claude:default", "claude")]);
        fixture
            .bindings
            .insert("pane-1".into(), "claude:default".into());
        let mut first = report(Some("pane-1"), "");
        first.official_payload = Some(json!({"rate_limits": {
            "five_hour": {"used_percentage": 42, "resets_at": NOW_MS / 1000 + 3600}
        }}));
        assert_eq!(code(&fixture.apply(first)), "ok");
        let mut second = report(Some("pane-1"), "");
        second.official_payload = Some(json!({
            "cost": {"total_cost_usd": 0},
            "context_window": {"context_window_size": 200000, "used_percentage": null, "current_usage": null}
        }));
        let accepted = fixture
            .apply(second)
            .unwrap_or_else(|rejected| panic!("被拒: {}", rejected.code));
        assert!(accepted.updated);
        let entry = &fixture.cache["claude:default"];
        assert_eq!(metric(entry, "five_hour").used_percent, Some(42.0));
        assert_eq!(metric(entry, "five_hour").text_value, None, "未过期不标记");
        assert_eq!(
            metric(entry, "context_window/used_percentage").used,
            None,
            "首次请求前上下文未知，不是 0"
        );
    }

    // ---- pi：herdr 扩展推送 ----

    fn pi_report(pane: &str, payload: serde_json::Value) -> UsageReportParams {
        UsageReportParams {
            account_id: String::new(),
            pane_id: Some(pane.into()),
            agent: Some("pi".into()),
            official_payload: Some(payload),
            snapshot: Default::default(),
        }
    }

    /// pi 的隐式默认账号，由 `configured_accounts` 按生产路径产出：它给默认账号预填的
    /// `provider` 是 agent 名（"pi"），不是空串。测试必须用这个形状——手工把 `provider`
    /// 置空会绕开绝大多数用户实际走的默认路径，让「扩展报上来的服务商写不进快照」这类
    /// 回归被测试掩盖。
    fn pi_default_account() -> UsageAccountConfig {
        let account =
            super::super::configured_accounts(&AccountUsageConfig::default(), &|provider| {
                provider.agent == "pi"
            })
            .into_iter()
            .find(|account| account.agent == "pi")
            .expect("已安装的 pi 必须补出隐式默认账号");
        assert_eq!(account.id, "pi:default");
        assert_eq!(
            account.provider, "pi",
            "默认账号的 provider 预填 agent 名；本测试依赖这个真实形状"
        );
        account
    }

    fn pi_payload() -> serde_json::Value {
        json!({
            "source": "herdr:pi",
            "provider": "anthropic",
            "model": "claude-sonnet-4-5",
            "context": {"tokens": 45000, "percent": 22.5, "context_window": 200000},
            "tokens": {"input": 1000, "output": 200, "cache_read": 3000, "cache_write": 40, "total": 4240},
            "cost_usd": 0.42
        })
    }

    #[test]
    fn pi_extension_push_is_accepted_as_session_statistics() {
        let mut fixture = Fixture::new(vec![pi_default_account()]);
        let accepted = fixture
            .apply(pi_report("wT:p3", pi_payload()))
            .unwrap_or_else(|rejected| panic!("被拒: {}", rejected.code));
        assert_eq!(accepted.account_id, "pi:default");
        assert_eq!(
            accepted.auto_bound_pane.as_deref(),
            Some("wT:p3"),
            "唯一的 pi 账号：自动绑定"
        );
        let entry = &fixture.cache["pi:default"];
        assert_eq!(entry.snapshot.status, ObservationStatus::Ready);
        assert_eq!(entry.snapshot.source, EXTENSION_PUSH_SOURCE);
        assert!(entry.snapshot.source.contains("非账号额度"));
        assert_eq!(
            entry.snapshot.provider, "anthropic",
            "账号未钉死计费厂商：取扩展报上来的当前服务商"
        );
        assert!(entry
            .snapshot
            .metrics
            .iter()
            .all(|metric| metric.scope == "session"));
        assert_eq!(
            metric(entry, "session/model").text_value.as_deref(),
            Some("anthropic/claude-sonnet-4-5")
        );
        assert_eq!(metric(entry, "context/percent").used, Some(22.5));
        assert!(entry.callback_latched(), "推送接管该账号，占位探测让位");
    }

    /// 扩展（`integration/assets/pi/herdr-agent-state.ts`）写到 socket 上的整条请求：与 bun 契约
    /// 测试断言的形状逐字段一致，这里钉住它能被反序列化并落成指标。
    #[test]
    fn pi_extension_wire_request_deserializes_and_applies() {
        let request: Request = serde_json::from_value(json!({
            "id": "herdr:pi:usage:1:abc",
            "method": "account.usage.report",
            "params": {
                "agent": "pi",
                "pane_id": "wT:p3",
                "official_payload": {
                    "source": "herdr:pi",
                    "version": 1,
                    "provider": "openai",
                    "model": "gpt-5",
                    "context": {"tokens": null, "percent": null, "context_window": 400000},
                    "tokens": {"input": 10, "output": 2, "cache_read": 0, "cache_write": 0, "total": 12},
                    "cost_usd": 0.0012
                }
            }
        }))
        .expect("扩展的请求形状必须是合法的 account.usage.report");
        let Method::AccountUsageReport(params) = request.method else {
            panic!("方法名必须是 account.usage.report");
        };
        let mut fixture = Fixture::new(vec![pi_default_account()]);
        assert_eq!(code(&fixture.apply(params)), "ok");
        let entry = &fixture.cache["pi:default"];
        assert_eq!(entry.snapshot.provider, "openai");
        assert_eq!(metric(entry, "context/tokens").used, None);
        assert_eq!(
            metric(entry, "session/cost_usd").amount_decimal.as_deref(),
            Some("0.0012")
        );
    }

    #[test]
    fn pi_push_keeps_a_configured_billing_provider_and_survives_compaction() {
        // 用户显式钉死计费厂商：从默认账号出发只改 `provider`，与上面两个测试共用同一形状。
        let mut pi_account = pi_default_account();
        pi_account.provider = "openrouter".into();
        let mut fixture = Fixture::new(vec![pi_account]);
        fixture.bindings.insert("wT:p3".into(), "pi:default".into());
        let mut payload = pi_payload();
        payload["context"] = json!({"tokens": null, "percent": null, "context_window": 200000});
        assert_eq!(code(&fixture.apply(pi_report("wT:p3", payload))), "ok");
        let entry = &fixture.cache["pi:default"];
        assert_eq!(
            entry.snapshot.provider, "openrouter",
            "账号配置钉死的计费厂商优先"
        );
        let tokens = metric(entry, "context/tokens");
        assert_eq!(tokens.used, None, "压缩后未知，不是 0");
        assert_eq!(
            tokens.text_value.as_deref(),
            Some(parse::PI_CONTEXT_PENDING_TEXT)
        );
        assert_eq!(metric(entry, "session/tokens/total").used, Some(4240.0));
    }

    #[test]
    fn pi_push_without_usage_fields_does_not_take_over_the_account() {
        let mut fixture = Fixture::new(vec![account("pi:default", "pi")]);
        fixture.bindings.insert("wT:p3".into(), "pi:default".into());
        let accepted = fixture
            .apply(pi_report(
                "wT:p3",
                json!({"provider": "anthropic", "model": "x"}),
            ))
            .unwrap_or_else(|rejected| panic!("被拒: {}", rejected.code));
        assert!(!accepted.updated);
        let entry = &fixture.cache["pi:default"];
        assert!(!entry.callback_latched());
        assert_eq!(
            entry.snapshot.message.as_deref(),
            Some(NO_PUSH_USAGE_MESSAGE)
        );
    }

    /// 范围外厂商的上报一律按未知 agent 处理：不解析、不自动绑定。
    #[test]
    fn reports_from_retired_providers_are_not_parsed() {
        let mut fixture = Fixture::new(vec![account("claude:default", "claude")]);
        let mut params = report(Some("wT:p9"), "");
        params.agent = Some("antigravity".into());
        params.official_payload = Some(json!({"quota": {"weekly": {"remaining_fraction": 0.5}}}));
        let rejected = rejected(fixture.apply(params));
        assert_eq!(rejected.code, "usage_binding_required");
        assert!(fixture.bindings.is_empty());
        assert!(!fixture.cache["claude:default"].callback_latched());
    }

    /// zcode 只由 server 只读本机数据库取数：按 agent 或按目标账号指向它的上报一律拒收，
    /// 不自动绑定、不记待办、不置闩锁，缓存里的本地统计原样保留。
    #[test]
    fn reports_for_the_local_database_source_are_refused() {
        let mut fixture = Fixture::new(vec![
            account("zcode:default", "zcode"),
            account("claude:default", "claude"),
        ]);
        let before = fixture.cache["zcode:default"].snapshot.clone();

        // 未绑定 pane、唯一候选、带官方报文：本来会自动绑定的形态。
        let mut by_agent = report(Some("wT:p9"), "");
        by_agent.agent = Some("zcode".into());
        let refused = rejected(fixture.apply(by_agent));
        assert_eq!(refused.code, "invalid_usage_report");
        assert_eq!(refused.message, LOCAL_SOURCE_REPORT_MESSAGE);

        // 显式指向 zcode 账号、只带快照指标（不经官方报文解析）。
        let mut by_account = report(None, "zcode:default");
        by_account.agent = None;
        by_account.official_payload = None;
        by_account.snapshot.metrics = vec![UsageMetric {
            id: "session/tokens/total".into(),
            label: "合计 token".into(),
            unit: "tokens".into(),
            scope: "local".into(),
            used: Some(1.0),
            ..Default::default()
        }];
        assert_eq!(code(&fixture.apply(by_account)), "invalid_usage_report");

        // 经已有绑定指向 zcode 账号，报文却自称 claude：目标账号一侧同样拒收。
        fixture
            .bindings
            .insert("pane-z".into(), "zcode:default".into());
        assert_eq!(
            code(&fixture.apply(report(Some("pane-z"), ""))),
            "invalid_usage_report"
        );

        let entry = &fixture.cache["zcode:default"];
        assert!(!entry.callback_latched());
        assert_eq!(entry.snapshot, before);
        assert_eq!(fixture.bindings.len(), 1, "只有测试自己写的那条绑定");
        assert!(fixture.pending_panes().is_empty());
        // 其它厂商不受影响。
        fixture
            .bindings
            .insert("pane-1".into(), "claude:default".into());
        assert_eq!(code(&fixture.apply(report(Some("pane-1"), ""))), "ok");
    }

    #[test]
    fn disabled_account_usage_never_auto_binds() {
        let mut fixture = Fixture::new(vec![account("claude:default", "claude")]);
        fixture.enabled = false;
        let rejected = rejected(fixture.apply(report(Some("wT:p9"), "")));
        assert_eq!(rejected.code, "usage_binding_required");
        assert_eq!(rejected.message, BINDING_REQUIRED_MESSAGE);
        assert!(fixture.bindings.is_empty());
        assert!(fixture.pending_panes().is_empty());
        // 已显式绑定的窗格不受开关影响，回调照常接受。
        fixture
            .bindings
            .insert("pane-1".into(), "claude:default".into());
        assert_eq!(code(&fixture.apply(report(Some("pane-1"), ""))), "ok");
    }

    // ---- 拒绝日志限速覆盖全部出口 ----

    #[test]
    fn every_rejection_is_rate_limited_per_pane_agent_and_code() {
        let mut fixture = Fixture::new(vec![
            account("claude:default", "claude"),
            account("kimi:default", "kimi"),
        ]);
        fixture
            .bindings
            .insert("pane-1".into(), "kimi:default".into());
        // 绑到别厂商账号：首次 info，其后 repeated。
        let first = rejected(fixture.apply(report(Some("pane-1"), "")));
        assert_eq!(first.code, "usage_binding_required");
        assert!(!first.repeated);
        assert_eq!(first.pane_id, "pane-1");
        assert_eq!(first.agent, "claude");
        assert!(rejected(fixture.apply(report(Some("pane-1"), ""))).repeated);
        // 不同错误码各自独立限速。
        let invalid = rejected(fixture.apply(report(Some("pane-1"), "claude:default")));
        assert_eq!(invalid.code, "invalid_usage_report");
        assert!(!invalid.repeated);
        assert!(rejected(fixture.apply(report(Some("pane-1"), "claude:default"))).repeated);
        // 无 pane 的报文同样限速；agent 别名记规范化后的厂商名。
        let mut params = report(None, "");
        params.agent = Some("Claude Code".into());
        let first = rejected(fixture.apply(params.clone()));
        assert!(!first.repeated);
        assert_eq!(first.pane_id, "");
        assert_eq!(first.agent, "claude");
        assert!(rejected(fixture.apply(params)).repeated);
        // 未知 agent 别名截断后记录，不带原文。
        let mut params = report(Some("pane-2"), "");
        params.agent = Some("x".repeat(100));
        assert_eq!(
            rejected(fixture.apply(params)).agent.len(),
            MAX_AGENT_LOG_LEN
        );
        // 超过限速间隔后重新按 info 记录。
        fixture.now_ms += REJECTION_LOG_INTERVAL_MS;
        assert!(!rejected(fixture.apply(report(Some("pane-1"), ""))).repeated);
    }

    // ---- 显式绑定 ----

    #[test]
    fn explicit_binding_validates_account_and_limits_then_clears_pending() {
        let mut fixture = Fixture::new(vec![
            account("claude:work", "claude"),
            account("claude:home", "claude"),
        ]);
        assert_eq!(
            code(&fixture.apply(report(Some("wT:p9"), ""))),
            "usage_binding_required"
        );
        assert_eq!(fixture.pending_panes(), vec!["wT:p9"]);
        assert_eq!(fixture.bind("wT:p9", "nope"), Err("unknown_account"));
        let long_pane = "p".repeat(MAX_PANE_ID_LEN + 1);
        assert_eq!(
            fixture.bind(&long_pane, "claude:work"),
            Err("invalid_binding")
        );
        assert_eq!(fixture.pending_panes(), vec!["wT:p9"]);
        assert_eq!(fixture.bind("wT:p9", "claude:work"), Ok(()));
        assert_eq!(fixture.bindings["wT:p9"], "claude:work");
        assert!(fixture.pending_panes().is_empty());
        assert_eq!(code(&fixture.apply(report(Some("wT:p9"), ""))), "ok");

        for index in 0..MAX_BINDINGS {
            fixture
                .bindings
                .insert(format!("pane-{index}"), "claude:work".into());
        }
        assert_eq!(
            fixture.bind("another", "claude:home"),
            Err("invalid_binding")
        );
    }

    // ---- 记账结构自身 ----

    #[test]
    fn pending_queue_is_bounded_and_deduplicated() {
        let mut rejections = Rejections::default();
        for index in 0..(PENDING_CAPACITY + 10) {
            rejections.record_pending(&format!("pane-{index}"), "claude", vec![], NOW_MS);
        }
        assert_eq!(rejections.pending().count(), PENDING_CAPACITY);
        assert!(rejections.pending().all(|item| item.pane_id != "pane-0"));
        assert!(rejections.pending().any(|item| item.pane_id == "pane-73"));

        rejections.record_pending("pane-73", "claude", vec!["a".into()], NOW_MS + 1);
        let entries = rejections
            .pending()
            .filter(|item| item.pane_id == "pane-73")
            .collect::<Vec<_>>();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].candidates, vec!["a"]);
        assert_eq!(entries[0].rejected_at_ms, NOW_MS + 1);
        // 同一 pane 的不同 agent 各自独立。
        rejections.record_pending("pane-73", "kimi", vec![], NOW_MS + 3);
        assert_eq!(
            rejections
                .pending()
                .filter(|item| item.pane_id == "pane-73")
                .count(),
            2
        );
        rejections.forget_pane("pane-73");
        assert!(rejections.pending().all(|item| item.pane_id != "pane-73"));
    }

    #[test]
    fn pending_queue_drops_entries_older_than_ttl() {
        let mut rejections = Rejections::default();
        rejections.record_pending("old", "claude", vec![], NOW_MS);
        rejections.record_pending("fresh", "claude", vec![], NOW_MS + PENDING_TTL_MS);
        let panes = rejections
            .pending()
            .map(|item| item.pane_id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(panes, vec!["fresh"]);
    }

    #[test]
    fn rejection_log_is_rate_limited_per_key_and_bounded() {
        let mut rejections = Rejections::default();
        let origin = Origin::new(Some("pane-1"), Some("claude"), None);
        assert!(rejections.should_log(&origin, "a", NOW_MS));
        assert!(!rejections.should_log(&origin, "a", NOW_MS + 1));
        assert!(rejections.should_log(&origin, "b", NOW_MS + 1));
        let other = Origin::new(Some("pane-2"), Some("claude"), None);
        assert!(rejections.should_log(&other, "a", NOW_MS + 1));
        assert!(rejections.should_log(&origin, "a", NOW_MS + REJECTION_LOG_INTERVAL_MS));

        let mut rejections = Rejections::default();
        for index in 0..(REJECTION_LOG_CAPACITY + 5) {
            let origin = Origin::new(Some(&format!("pane-{index}")), Some("claude"), None);
            assert!(rejections.should_log(&origin, "a", NOW_MS));
        }
        assert_eq!(rejections.logged.len(), REJECTION_LOG_CAPACITY);
        // 最早的键被淘汰后会重新记录，容量只是防御性上限。
        let first = Origin::new(Some("pane-0"), Some("claude"), None);
        assert!(rejections.should_log(&first, "a", NOW_MS));
    }
}
