mod apply;
mod http;
mod parse;
mod persistence;
mod registry;
mod transport;
pub(crate) use transport::run_probe_helper;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use super::Reply;
use crate::api::schema::*;
use crate::config::{AccountUsageConfig, UsageAccountConfig};

/// 显式刷新的防抖：用户切换/刷新对同一账号最多每 10 s 真正探测一次（与厂商间隔无关，
/// 响应里的 `next_allowed_at_ms` 是唯一口径）。
const MANUAL_DEBOUNCE: Duration = Duration::from_secs(10);
/// 终态（未登录 / 不支持 / 无权限）自动重试的慢 TTL 起点与上限。
const TERMINAL_RETRY_BASE: Duration = Duration::from_secs(10 * 60);
const TERMINAL_RETRY_CAP: Duration = Duration::from_secs(60 * 60);
/// 终态自动重试次数上限：用尽后只有显式刷新、凭据或 CLI 变化能再触发探测
/// （claude 交互探测会真起 PTY，不能无限自动重试）。
const TERMINAL_AUTO_RETRY_LIMIT: u32 = 6;
/// 官方回调闩锁时长：期内不回落探测；到期后快照降为缓存态并允许探测，但只有 Ready 结果
/// 能覆盖回调快照。
pub(super) const CALLBACK_LATCH_MS: u64 = 15 * 60 * 1000;
/// 回调闩锁到期后写入快照的说明；样本与采样时间原样保留。
const CALLBACK_STALE_MESSAGE: &str =
    "官方回调已超过 15 分钟未更新，显示的是缓存样本；等待下一次回调";
/// 配置文件戳的复查周期；已安装厂商与终态指纹的复查对齐 `registry::AVAILABILITY_TTL`。
const RELOAD_INTERVAL: Duration = Duration::from_secs(5);
/// 隐式默认账号（`<agent>:default`）在官方 CLI 暂时检测不到时的保留时长：PATH 抖动、
/// `npm i -g` 升级窗口都在这个量级内恢复；期满只移出清单，绑定与公开身份不剪枝。
const IMPLICIT_ACCOUNT_GRACE: Duration = Duration::from_secs(5 * 60);

enum Command {
    Request { request: Box<Request>, reply: Reply },
    Release(u64),
}

struct Task {
    generation: u64,
    account: UsageAccountConfig,
    timeout: Duration,
}

/// 查询线程回报：开始执行（清 `queued`）与完成。快照装箱，避免枚举随其体积膨胀。
enum Outcome {
    Started(u64),
    Completed(u64, Box<AccountUsageSnapshot>),
}

/// 终态保持：慢 TTL 与进入终态时的探测指纹。它是缓存事实，与展示用的快照状态解耦——
/// 回调闩锁下的回落探测失败时快照仍显示回调数据，但保持照样推进并管住自动重试。
#[derive(Debug, Clone, PartialEq, Eq)]
struct TerminalHold {
    /// 连续终态次数，决定 TTL 的指数。
    streak: u32,
    /// 自动重试的最早时间；`None` 表示自动重试已用尽。
    until: Option<Instant>,
    /// 进入终态时的 CLI / 凭据指纹；变化即自愈。由服务循环在探测完成后填入。
    fingerprint: Option<registry::ProbeFingerprint>,
}

struct CacheEntry {
    snapshot: AccountUsageSnapshot,
    /// 最近一次真正派发探测的时刻：自动间隔与显式防抖都以它为基准。
    requested_at: Option<Instant>,
    in_flight: bool,
    /// 已入队但查询线程尚未开始执行。
    queued: bool,
    generation: u64,
    failures: u32,
    /// 官方回调闩锁的截止时间（墙钟毫秒）；`None` 表示当前快照来自探测。
    callback_until_ms: Option<u64>,
    /// 厂商要求的退避（HTTP 429），显式刷新也不豁免。
    retry_after: Option<Instant>,
    terminal: Option<TerminalHold>,
    /// 最近一次派发探测的墙钟时间；0 表示从未探测。
    attempted_at_ms: u64,
    /// 最近一次显式刷新到达的墙钟时间，含被防抖的请求。
    manual_requested_at_ms: Option<u64>,
    /// 官方 CLI 在等待用户确认目录信任。
    /// TODO(Claude 探测分类)：目前没有写点、恒为 false；Claude 交互探测把 trust/permission
    /// 关键字拆成可重试态时在 `complete` 里按探测结果填入。
    trust_required: bool,
}

impl CacheEntry {
    fn new(snapshot: AccountUsageSnapshot) -> Self {
        Self {
            snapshot,
            requested_at: None,
            in_flight: false,
            queued: false,
            generation: 0,
            failures: 0,
            callback_until_ms: None,
            retry_after: None,
            terminal: None,
            attempted_at_ms: 0,
            manual_requested_at_ms: None,
            trust_required: false,
        }
    }

    /// 冷条目：沿用已保存的公开身份，登录切换检测才有对照基线。
    fn cold(account: &UsageAccountConfig, saved: &persistence::Saved) -> Self {
        Self::new(AccountUsageSnapshot {
            account_identity: saved.identities.get(&account.id).cloned(),
            ..empty_snapshot(account)
        })
    }

    /// 当前快照由官方回调写入（闩锁未清，不论是否过期）。
    fn callback_latched(&self) -> bool {
        self.callback_until_ms.is_some()
    }
}

type Subscribers = HashMap<String, (Option<u64>, UsageParams, Reply)>;

/// 一次 `account.usage.*` 请求解析出的目标：`request` 用于派发探测，`matching` 用于筛选
/// 返回/推送的账号。未绑定 pane 在该 agent 只有一个账号时按唯一候选推断（不写绑定）；
/// 歧义时 `placeholder` 为真，返回 NeedsBinding 占位且不探测。
struct Selection {
    request: UsageParams,
    matching: UsageParams,
    inferred: bool,
    placeholder: bool,
}

/// 服务循环持有的全部共享状态；请求分派与订阅推送在 `Service::start` 的循环里。
struct ServiceState {
    config: AccountUsageConfig,
    /// 配置文件的 (mtime, 大小)：变化才重新解析。
    config_stamp: Option<(Option<SystemTime>, u64)>,
    accounts: Vec<UsageAccountConfig>,
    cache: HashMap<String, CacheEntry>,
    bindings: HashMap<String, String>,
    saved: persistence::Saved,
    /// 上次真正落盘的内容，供脏检查。
    persisted: Option<persistence::Saved>,
    /// 绑定与公开身份的持久化文件。
    state_path: PathBuf,
    rejections: apply::Rejections,
    next_query: u64,
    /// 隐式默认账号首次检测不到官方 CLI 的时刻，宽限期内保留在清单里。
    missing_since: HashMap<String, Instant>,
    /// 上次按已安装厂商重算账号清单的时刻。
    scanned_at: Option<Instant>,
}

impl ServiceState {
    fn load() -> Self {
        let loaded = crate::config::Config::load();
        let load_failed = loaded
            .diagnostics
            .iter()
            .any(|diagnostic| crate::config::is_config_load_failure(diagnostic));
        let config = loaded.config.account_usage;
        let accounts = configured_accounts(&config, &registry::provider_installed);
        let state_path = persistence::path();
        let mut saved = persistence::load(&state_path);
        let persisted = Some(saved.clone());
        // 身份剪枝只看「账号是否还可能存在」（显式配置 + 注册表默认 id），不看当次是否检测到
        // CLI；配置本身没读出来时更不能按默认值剪枝。
        if !load_failed {
            saved.prune_identities(|id| known_account_id(&config, id));
        }
        let cache = accounts
            .iter()
            .map(|account| (account.id.clone(), CacheEntry::cold(account, &saved)))
            .collect::<HashMap<_, _>>();
        let bindings = std::mem::take(&mut saved.bindings);
        Self {
            config,
            config_stamp: config_stamp(),
            accounts,
            cache,
            bindings,
            saved,
            persisted,
            state_path,
            rejections: apply::Rejections::default(),
            next_query: 1,
            missing_since: HashMap::new(),
            scanned_at: Some(Instant::now()),
        }
    }

    /// 配置文件 mtime/大小变化才重新解析；解析失败保留上一份有效配置（不触发账号增删与
    /// 绑定剪枝）。账号清单在配置变化时立即重算，否则按 `AVAILABILITY_TTL` 周期随已安装
    /// 厂商复算；`installed` 是已安装判定（生产走 `registry::provider_installed`）。
    fn reload(&mut self, installed: &dyn Fn(&registry::Provider) -> bool, now: Instant) {
        let stamp = config_stamp();
        let mut removed_explicit = HashSet::new();
        let mut config_changed = false;
        if stamp != self.config_stamp {
            self.config_stamp = stamp;
            let loaded = crate::config::Config::load();
            if loaded
                .diagnostics
                .iter()
                .any(|diagnostic| crate::config::is_config_load_failure(diagnostic))
            {
                tracing::warn!(
                    event = "account.config.reload",
                    subsystem = "account_usage",
                    outcome = "error",
                    "配置文件无法解析，账号用量沿用上一份有效配置"
                );
            } else if loaded.config.account_usage != self.config {
                let previous = explicit_ids(&self.config);
                self.config = loaded.config.account_usage;
                removed_explicit = &previous - &explicit_ids(&self.config);
                config_changed = true;
            }
        }
        let scan_due = self
            .scanned_at
            .is_none_or(|at| now.saturating_duration_since(at) >= registry::AVAILABILITY_TTL);
        if !config_changed && !scan_due {
            return;
        }
        self.scanned_at = Some(now);
        let updated = configured_accounts(&self.config, installed);
        if updated != self.accounts || !removed_explicit.is_empty() {
            self.apply_accounts(updated, &removed_explicit, now);
        }
    }

    /// 把新的账号清单并入状态。两种「消失」区别对待：
    /// - 用户在配置里显式删除的账号（`removed_explicit`）：删缓存、剪绑定与公开身份并落盘；
    /// - 隐式默认账号只是官方 CLI 暂时检测不到：宽限期内保留在清单里，期满只移出清单，
    ///   绑定与身份都不剪（指向未知 id 的绑定在读路径被 `matches_account` 惰性过滤）。
    ///
    /// 已有账号只在影响探测的字段变化时重建条目并撤销其绑定；只改 label 就地更新。
    fn apply_accounts(
        &mut self,
        updated: Vec<UsageAccountConfig>,
        removed_explicit: &HashSet<String>,
        now: Instant,
    ) {
        let previous = std::mem::take(&mut self.accounts);
        let mut next = Vec::with_capacity(updated.len());
        for account in updated {
            match previous.iter().find(|old| old.id == account.id) {
                Some(old) if probe_config_changed(old, &account) => {
                    // 探测目标变了：旧样本与绑定都不再可信；公开身份沿用已保存的值。
                    self.bindings.retain(|_, id| id != &account.id);
                    self.cache
                        .insert(account.id.clone(), CacheEntry::cold(&account, &self.saved));
                }
                Some(old) if old.label != account.label => {
                    if let Some(entry) = self.cache.get_mut(&account.id) {
                        entry.snapshot.account_label = label_of(&account);
                    }
                }
                Some(_) => {}
                None => {
                    // 新账号；宽限期内回来的隐式账号缓存仍在，直接复用。
                    if !self.cache.contains_key(&account.id) {
                        self.cache
                            .insert(account.id.clone(), CacheEntry::cold(&account, &self.saved));
                    }
                }
            }
            self.missing_since.remove(&account.id);
            next.push(account);
        }
        for old in previous.iter() {
            if next.iter().any(|account| account.id == old.id) {
                continue;
            }
            if removed_explicit.contains(&old.id) {
                self.cache.remove(&old.id);
                self.bindings.retain(|_, id| id != &old.id);
                self.saved.identities.remove(&old.id);
                self.missing_since.remove(&old.id);
                continue;
            }
            let since = *self.missing_since.entry(old.id.clone()).or_insert(now);
            if now.saturating_duration_since(since) < IMPLICIT_ACCOUNT_GRACE {
                next.push(old.clone());
            } else {
                tracing::info!(
                    event = "account.retire",
                    subsystem = "account_usage",
                    outcome = "ok",
                    account_id = %old.id,
                    "官方 CLI 持续未检测到，隐式默认账号移出清单（绑定与身份保留）"
                );
                self.cache.remove(&old.id);
                self.missing_since.remove(&old.id);
            }
        }
        if next != previous {
            // 待办里的候选账号与限速键都可能已失效，随账号清单一起重来。
            self.rejections = apply::Rejections::default();
        }
        self.accounts = next;
        // 脏检查保证只有绑定/身份真的变了才写盘：检测抖动不会固化到磁盘。
        self.persist();
    }

    /// 同步绑定快照并按脏检查落盘。
    fn persist(&mut self) -> bool {
        self.saved.bindings.clone_from(&self.bindings);
        persistence::store_if_changed(&self.state_path, &self.saved, &mut self.persisted)
    }

    /// 终态条目按当前 CLI / 凭据指纹自愈：登录、升级或换路径后允许重新探测。
    fn heal_terminal_entries(&mut self) {
        for account in &self.accounts {
            let Some(entry) = self.cache.get_mut(&account.id) else {
                continue;
            };
            if entry.terminal.is_none() {
                continue;
            }
            let Some(provider) = registry::provider(&account.agent) else {
                continue;
            };
            let current = registry::probe_fingerprint(provider, account);
            if heal_terminal(entry, &current) {
                tracing::info!(
                    event = "account.probe.heal",
                    subsystem = "account_usage",
                    outcome = "ok",
                    account_id = %account.id,
                    status = ?entry.snapshot.status,
                    "CLI 或凭据已变化，终态账号允许重新探测"
                );
            }
        }
    }

    /// 官方回调超过闩锁时长没有更新：快照降为 Stale（样本与采样时间保留），直到下一次回调
    /// 或成功的回落探测替换它。返回被改写的账号 id，供调用方推送给订阅者。
    fn age_out_callbacks(&mut self, now_ms: u64) -> Vec<String> {
        age_out_callbacks(&mut self.cache, now_ms)
    }

    /// 处理一次查询线程回报；返回是否并入了新结果（调用方据此决定是否落盘）。
    /// 这里不做 I/O 以外的持久化，落盘由循环统一按脏检查执行。
    fn complete(&mut self, outcome: Outcome, subscribers: &mut Subscribers) -> bool {
        let (generation, mut snapshot) = match outcome {
            Outcome::Started(generation) => {
                for entry in self.cache.values_mut() {
                    if entry.in_flight && entry.generation == generation {
                        entry.queued = false;
                    }
                }
                return false;
            }
            Outcome::Completed(generation, snapshot) => (generation, *snapshot),
        };
        let account_id = snapshot.account_id.clone();
        {
            let Some(entry) = self.cache.get_mut(&account_id) else {
                return false;
            };
            if entry.generation != generation {
                return false;
            }
            entry.in_flight = false;
            entry.queued = false;
            tracing::debug!(
                event = "account.probe.complete",
                subsystem = "account_usage",
                outcome = "completed",
                account_id = %account_id,
                status = ?snapshot.status,
                metrics = snapshot.metrics.len(),
                generation,
                "账号用量探测完成"
            );
            if invalidate_changed_identity(&entry.snapshot, &mut snapshot, &mut self.bindings) {
                entry.snapshot.metrics.clear();
                // 绑定已整体撤销，回调快照随之作废，避免闩锁把账号钉死。
                entry.callback_until_ms = None;
            }
            merge_result(entry, snapshot, Instant::now());
            if let Some(hold) = &mut entry.terminal {
                hold.fingerprint = self
                    .accounts
                    .iter()
                    .find(|account| account.id == account_id)
                    .and_then(|account| {
                        registry::provider(&account.agent)
                            .map(|provider| registry::probe_fingerprint(provider, account))
                    });
            }
            if let Some(identity) = &entry.snapshot.account_identity {
                self.saved
                    .identities
                    .insert(account_id.clone(), identity.clone());
            }
        }
        if let Some(entry) = self.cache.get(&account_id) {
            notify_subscribers(subscribers, &entry.snapshot, &self.bindings);
        }
        true
    }

    fn request(&mut self, params: &UsageParams, manual: bool, tasks: &mpsc::SyncSender<Task>) {
        request_accounts(
            params,
            manual,
            &self.accounts,
            &self.config,
            &self.bindings,
            &mut self.cache,
            tasks,
            &mut self.next_query,
        );
    }

    /// 解析请求目标：读路径（`usage`）与订阅落库共用，保证同一请求在两条通道看到同一
    /// 组账号。
    fn select(&self, params: &UsageParams) -> Selection {
        let unbound = params.pane_id.is_some() && !binding_matches(params, &self.bindings);
        let inferred = if unbound {
            infer_binding(params, &self.accounts)
        } else {
            None
        };
        let matching = UsageParams {
            agent: params.agent.clone(),
            account_id: inferred.clone().or_else(|| params.account_id.clone()),
            pane_id: if unbound {
                None
            } else {
                params.pane_id.clone()
            },
        };
        let placeholder = unbound && inferred.is_none();
        Selection {
            // 未绑定且无法推断时沿用原参数：`request_accounts` 的 unbound_pane 闸门保证不探测。
            request: if placeholder {
                params.clone()
            } else {
                matching.clone()
            },
            matching,
            inferred: inferred.is_some(),
            placeholder,
        }
    }

    /// `account.usage.get|refresh`：未绑定 pane 在该 agent 只有一个账号时按唯一候选返回
    /// 数据并标 `binding_inferred`（不写绑定）；歧义时沿用 NeedsBinding 占位且不探测。
    fn usage(
        &mut self,
        params: &UsageParams,
        manual: bool,
        tasks: &mpsc::SyncSender<Task>,
    ) -> ResponseResult {
        let selection = self.select(params);
        let now_ms = super::now_ms();
        self.age_out_callbacks(now_ms);
        self.request(&selection.request, manual, tasks);
        let now = Instant::now();
        let mut values = self
            .accounts
            .iter()
            .filter_map(|account| {
                let entry = self.cache.get(&account.id)?;
                if !matches_account(&selection.matching, &entry.snapshot, &self.bindings) {
                    return None;
                }
                let mut refresh = refresh_state(entry, account, now, now_ms);
                refresh.binding_inferred = selection.inferred;
                refresh.pending_binding = self
                    .rejections
                    .pending_for(params.pane_id.as_deref(), &account.agent, now_ms)
                    .map(UsagePendingBinding::from);
                let mut snapshot = entry.snapshot.clone();
                if selection.placeholder {
                    snapshot.status = ObservationStatus::NeedsBinding;
                    snapshot.metrics.clear();
                    snapshot.message = Some(
                        "请先确认此 Agent 使用的账号；该厂商只有一个账号时会在收到官方回调时自动绑定，多个账号请手动选择"
                            .into(),
                    );
                }
                Some((snapshot, refresh))
            })
            .collect::<Vec<_>>();
        values.sort_by(|a, b| a.0.account_id.cmp(&b.0.account_id));
        let (accounts, refresh): (Vec<_>, Vec<_>) = values.into_iter().unzip();
        ResponseResult::AccountUsage {
            accounts,
            refresh: Some(refresh),
        }
    }
}

pub(super) struct Service {
    commands: mpsc::SyncSender<Command>,
}

impl Service {
    pub fn start() -> std::io::Result<Self> {
        let (commands, input) = mpsc::sync_channel(64);
        let (tasks, task_rx) = mpsc::sync_channel::<Task>(32);
        let task_rx = Arc::new(Mutex::new(task_rx));
        let (results, completed) = mpsc::channel::<Outcome>();
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
                    if output.send(Outcome::Started(task.generation)).is_err() {
                        break;
                    }
                    let snapshot = query(&task.account, task.timeout);
                    if output
                        .send(Outcome::Completed(task.generation, Box::new(snapshot)))
                        .is_err()
                    {
                        break;
                    }
                })?;
        }
        std::thread::Builder::new()
            .name("herdr-account-usage".into())
            .spawn(move || {
                let mut state = ServiceState::load();
                let mut subscribers = Subscribers::new();
                let mut next_subscription = 1_u64;
                let mut reload_at = Instant::now() + RELOAD_INTERVAL;
                let mut heal_at = Instant::now() + registry::AVAILABILITY_TTL;
                loop {
                    let now = Instant::now();
                    if now >= reload_at {
                        reload_at = now + RELOAD_INTERVAL;
                        state.reload(&registry::provider_installed, now);
                    }
                    if now >= heal_at {
                        // 指纹计算走缓存的 PATH 解析 + 几次 stat，按扫描 TTL 而不是 5 s 跑。
                        heal_at = now + registry::AVAILABILITY_TTL;
                        state.heal_terminal_entries();
                    }
                    let mut merged = false;
                    while let Ok(outcome) = completed.try_recv() {
                        merged |= state.complete(outcome, &mut subscribers);
                    }
                    if merged {
                        state.persist();
                    }
                    // 回调闩锁到期的账号降为缓存态，并像探测完成一样推送给订阅者；放在命令
                    // 处理之前，读路径与事件通道看到的是同一次改写。
                    for id in state.age_out_callbacks(super::now_ms()) {
                        if let Some(entry) = state.cache.get(&id) {
                            notify_subscribers(&mut subscribers, &entry.snapshot, &state.bindings);
                        }
                    }
                    let command = match input.recv_timeout(Duration::from_millis(100)) {
                        Ok(command) => command,
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            subscribers.retain(|_, (_, _, reply)| reply.alive());
                            let wanted = subscribers
                                .values()
                                .map(|(_, params, _)| params.clone())
                                .collect::<Vec<_>>();
                            for params in &wanted {
                                state.request(params, false, &tasks);
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
                                        minimum_interval_seconds: minimum_interval_seconds(
                                            Some(p),
                                            false,
                                            &state.config,
                                        ),
                                        configured_accounts: state
                                            .accounts
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
                            // 连续悬浮与显式刷新都复用同账号的进行中任务；显式刷新只受
                            // 短防抖与厂商退避约束。
                            Ok(state.usage(&params, manual_refresh, &tasks))
                        }
                        Method::AccountBindingSet(params) => apply::bind_pane(
                            &params.pane_id,
                            &params.account_id,
                            &state.accounts,
                            &mut state.bindings,
                            &mut state.rejections,
                        )
                        .map(|()| {
                            state.persist();
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
                            // 订阅参数与 `usage()` 走同一次解析：未绑定 pane 的唯一候选推断
                            // 在事件通道同样生效，否则 get 能拿到的数据推送永远收不到。
                            let params = state.select(&params).request;
                            let subscription_id = format!("usage-{next_subscription}");
                            next_subscription = next_subscription.saturating_add(1);
                            subscribers.insert(
                                subscription_id.clone(),
                                (reply.owner(), params.clone(), reply.clone()),
                            );
                            state.request(&params, false, &tasks);
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
                            if let Some(account) = state
                                .accounts
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
                                    accounts: &state.accounts,
                                    enabled: state.config.enabled,
                                    now_ms: super::now_ms(),
                                },
                                &mut state.bindings,
                                &mut state.cache,
                                &mut state.rejections,
                                &mut state.next_query,
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
                                    if accepted.updated
                                        && commit_accepted(
                                            &accepted,
                                            &state.cache,
                                            &state.bindings,
                                            &mut state.saved,
                                            &mut subscribers,
                                        )
                                    {
                                        state.persist();
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

/// 把一份快照推送给匹配的订阅者；断开的订阅者顺手清掉。探测完成、回调接受与闩锁到期
/// 三条路径共用，事件通道与读路径对同一账号给出同一份数据。
fn notify_subscribers(
    subscribers: &mut Subscribers,
    value: &AccountUsageSnapshot,
    bindings: &HashMap<String, String>,
) {
    subscribers.retain(|_, (_, params, reply)| {
        reply.alive()
            && (!matches_account(params, value, bindings)
                || reply.event(
                    "account.usage.updated",
                    serde_json::json!({"accounts":[value]}),
                ))
    });
}

/// 报告被接受后的调用方职责：记住公开身份、同步绑定快照并推送给匹配的订阅者。
/// 返回是否有内容需要持久化（缓存里找不到该账号时为 false）。
fn commit_accepted(
    accepted: &apply::Accepted,
    cache: &HashMap<String, CacheEntry>,
    bindings: &HashMap<String, String>,
    saved: &mut persistence::Saved,
    subscribers: &mut Subscribers,
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
    notify_subscribers(subscribers, &entry.snapshot, bindings);
    true
}

/// 官方回调超过闩锁时长没有更新的账号降为 Stale；返回被改写的账号 id。
/// 只改状态与说明，样本、采样时间与闩锁本身都保留：下一次回调或成功的回落探测会替换。
fn age_out_callbacks(cache: &mut HashMap<String, CacheEntry>, now_ms: u64) -> Vec<String> {
    let mut aged = Vec::new();
    for (id, entry) in cache.iter_mut() {
        let expired = entry.callback_until_ms.is_some_and(|until| now_ms >= until);
        if expired && entry.snapshot.status == ObservationStatus::Ready {
            entry.snapshot.status = ObservationStatus::Stale;
            entry.snapshot.message = Some(CALLBACK_STALE_MESSAGE.into());
            aged.push(id.clone());
        }
    }
    aged.sort();
    aged
}

/// 配置文件的 (mtime, 大小)；文件不存在时为 `None`。mtime 不可用的文件系统只比大小。
fn config_stamp() -> Option<(Option<SystemTime>, u64)> {
    let metadata = std::fs::metadata(crate::config::config_path()).ok()?;
    Some((metadata.modified().ok(), metadata.len()))
}

/// 用户在 TOML 里显式配置的账号 id。
fn explicit_ids(config: &AccountUsageConfig) -> HashSet<String> {
    config
        .accounts
        .iter()
        .map(|account| account.id.clone())
        .collect()
}

/// 账号 id 是否仍可能存在：显式配置的 id，或任一注册厂商的隐式默认 id。
/// 不看当次是否检测到 CLI，持久化的身份/绑定剪枝只能用这个判据。
fn known_account_id(config: &AccountUsageConfig, id: &str) -> bool {
    config.accounts.iter().any(|account| account.id == id)
        || registry::PROVIDERS
            .iter()
            .any(|provider| id == format!("{}:default", provider.agent))
}

/// 影响探测目标的字段是否变化（label 以外的全部字段）。
fn probe_config_changed(old: &UsageAccountConfig, new: &UsageAccountConfig) -> bool {
    let strip = |account: &UsageAccountConfig| UsageAccountConfig {
        label: String::new(),
        ..account.clone()
    };
    strip(old) != strip(new)
}

fn label_of(account: &UsageAccountConfig) -> String {
    if account.label.is_empty() {
        account.id.clone()
    } else {
        account.label.clone()
    }
}

/// 账号清单：用户配置的账号（agent 规范化、按 id 去重保留首个）加上本机已安装厂商的隐式
/// 默认账号。`installed` 由调用方注入，测试不依赖本机 PATH。
fn configured_accounts(
    config: &AccountUsageConfig,
    installed: &dyn Fn(&registry::Provider) -> bool,
) -> Vec<UsageAccountConfig> {
    let mut accounts = Vec::with_capacity(config.accounts.len());
    for account in &config.accounts {
        if accounts
            .iter()
            .any(|seen: &UsageAccountConfig| seen.id == account.id)
        {
            continue;
        }
        let mut account = account.clone();
        if let Some(provider) = registry::provider(&account.agent) {
            account.agent = provider.agent.into();
        }
        accounts.push(account);
    }
    // Only installed CLIs get an implicit default account, so the usage page
    // reflects what this host can actually query instead of the full
    // registry. User-configured accounts are always kept.
    for provider in registry::PROVIDERS {
        if !installed(provider) {
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
        account_label: label_of(account),
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

/// 未绑定 pane 的读路径推断：只在请求指明 agent、未指定账号、且该 agent 恰好一个账号时
/// 给出候选。这里不写绑定——绑定只由官方回调（唯一候选）或用户显式确认写入。
fn infer_binding(params: &UsageParams, accounts: &[UsageAccountConfig]) -> Option<String> {
    if params.account_id.is_some() {
        return None;
    }
    let provider = params.agent.as_deref().and_then(registry::provider)?;
    let mut candidates = accounts
        .iter()
        .filter(|account| account.agent == provider.agent)
        .map(|account| &account.id);
    match (candidates.next(), candidates.next()) {
        (Some(only), None) => Some(only.clone()),
        _ => None,
    }
}

/// 账号是否走官方 API（而非本机 CLI）查询。
fn uses_api(account: &UsageAccountConfig) -> bool {
    account.auth_mode == "api" || account.credential_env.is_some()
}

/// 对外播报的最小刷新间隔（秒）——`account.usage.providers` 的 `minimum_interval_seconds`
/// 与自动轮询间隔的唯一真源。API 凭据与报表接口按 `api_refresh_seconds`（≥60）；回调型厂商
/// 没有可轮询的官方接口，播报 0；其余 CLI 探测按 `cli_refresh_seconds`（≥300）。
fn minimum_interval_seconds(
    provider: Option<&registry::Provider>,
    api: bool,
    config: &AccountUsageConfig,
) -> u64 {
    if api {
        return config.api_refresh_seconds.max(60);
    }
    match provider.map(|provider| provider.query) {
        Some(registry::Query::Callback) => 0,
        Some(registry::Query::Portal) => config.api_refresh_seconds.max(60),
        _ => config.cli_refresh_seconds.max(300),
    }
}

/// 账号的自动轮询间隔（秒）：即播报值；回调型厂商的占位探测按 CLI 周期，避免 0 间隔空转。
fn refresh_interval(account: &UsageAccountConfig, config: &AccountUsageConfig) -> u64 {
    let provider = registry::provider(&account.agent);
    match minimum_interval_seconds(provider, uses_api(account), config) {
        0 => config.cli_refresh_seconds.max(300),
        seconds => seconds,
    }
}

fn is_terminal(status: ObservationStatus) -> bool {
    matches!(
        status,
        ObservationStatus::PermissionDenied
            | ObservationStatus::NotAuthenticated
            | ObservationStatus::Unsupported
    )
}

/// 再次得到终态时推进慢 TTL：10 min 起、每次翻倍、1 h 顶；超过重试上限后不再自动重试。
fn next_terminal_hold(previous: Option<&TerminalHold>, now: Instant) -> TerminalHold {
    let streak = previous.map_or(0, |hold| hold.streak).saturating_add(1);
    let until = (streak <= TERMINAL_AUTO_RETRY_LIMIT).then(|| {
        let exponent = streak.saturating_sub(1).min(6);
        now + TERMINAL_RETRY_BASE
            .saturating_mul(1_u32 << exponent)
            .min(TERMINAL_RETRY_CAP)
    });
    TerminalHold {
        streak,
        until,
        fingerprint: previous.and_then(|hold| hold.fingerprint.clone()),
    }
}

/// 指纹与进入终态时不同 ⇒ 撤销保持。指纹尚未采集时不判定。只撤销保持、不清
/// `requested_at`：自动间隔仍是下限，指纹抖动最多让账号每个周期多探测一次；要立即探测走
/// 显式刷新。
fn heal_terminal(entry: &mut CacheEntry, current: &registry::ProbeFingerprint) -> bool {
    let changed = entry
        .terminal
        .as_ref()
        .and_then(|hold| hold.fingerprint.as_ref())
        .is_some_and(|recorded| recorded != current);
    if changed {
        entry.terminal = None;
    }
    changed
}

/// Instant → 墙钟毫秒（相对当前时刻换算，过去的时刻取 `now_ms`）。
fn wall_clock_ms(now: Instant, now_ms: u64, at: Instant) -> u64 {
    let ahead = at.saturating_duration_since(now).as_millis();
    now_ms.saturating_add(ahead.min(u128::from(u64::MAX)) as u64)
}

/// 与快照并行的刷新状态；`binding_inferred` 与 `pending_binding` 由调用方按请求补齐。
fn refresh_state(
    entry: &CacheEntry,
    account: &UsageAccountConfig,
    now: Instant,
    now_ms: u64,
) -> UsageRefreshState {
    let debounce_until = entry.requested_at.map(|at| at + MANUAL_DEBOUNCE);
    let retry_after = entry.retry_after.filter(|until| *until > now);
    let next_allowed = [debounce_until, retry_after]
        .into_iter()
        .flatten()
        .filter(|until| *until > now)
        .max();
    UsageRefreshState {
        account_id: entry.snapshot.account_id.clone(),
        in_flight: entry.in_flight,
        queued: entry.queued,
        requested_at_ms: entry.manual_requested_at_ms,
        attempted_at_ms: (entry.attempted_at_ms > 0).then_some(entry.attempted_at_ms),
        next_allowed_at_ms: next_allowed.map(|at| wall_clock_ms(now, now_ms, at)),
        retry_after_ms: retry_after.map(|at| wall_clock_ms(now, now_ms, at)),
        binding_inferred: false,
        trust_required: entry.trust_required,
        callback_only: registry::provider(&account.agent)
            .is_some_and(|provider| matches!(provider.query, registry::Query::Callback)),
        pending_binding: None,
    }
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
    // 总览（未选定厂商/账号）也发起自动查询：账号清单已只含本机已安装厂商，
    // 查询频率由下方 requested_at/退避控制，用户显式选择仍可立即查询。
    if params.pane_id.is_some()
        && params.account_id.is_none()
        && !params
            .pane_id
            .as_ref()
            .is_some_and(|pane| bindings.contains_key(pane))
    {
        // 未绑定 pane 不对应任何账号，显式刷新也无处登记：响应以 NeedsBinding 占位说明。
        probe_skipped(None, "unbound_pane", manual);
        return;
    }
    let now = Instant::now();
    let now_ms = super::now_ms();
    for account in accounts {
        let Some(entry) = cache.get_mut(&account.id) else {
            continue;
        };
        if !matches_account(params, &entry.snapshot, bindings) {
            continue;
        }
        // 显式刷新先登记再过闸门：被拦下的请求也能在响应里解释「为什么点了没反应」。
        if manual {
            entry.manual_requested_at_ms = Some(now_ms);
        }
        if !config.enabled {
            probe_skipped(Some(&account.id), "disabled", manual);
            continue;
        }
        if entry.in_flight {
            probe_skipped(Some(&account.id), "in_flight", manual);
            continue;
        }
        if entry.retry_after.is_some_and(|until| now < until) {
            probe_skipped(Some(&account.id), "retry_after", manual);
            continue;
        }
        let provider = registry::provider(&account.agent);
        if entry.callback_latched() {
            // 回调型厂商没有可回落的探测（只会得到占位）；其余厂商在闩锁新鲜时不回落，
            // 显式刷新可穿透；到期后允许回落，但只有 Ready 结果能覆盖（见 merge_result）。
            let callback_only =
                provider.is_some_and(|p| matches!(p.query, registry::Query::Callback));
            let fresh = entry.callback_until_ms.is_some_and(|until| now_ms < until);
            if callback_only || (fresh && !manual) {
                probe_skipped(Some(&account.id), "callback", manual);
                continue;
            }
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
        // 显式刷新只受短防抖约束；自动轮询按失败次数指数退避。
        let wait = if manual {
            MANUAL_DEBOUNCE
        } else {
            let seconds = refresh_interval(account, config);
            Duration::from_secs(
                seconds
                    .saturating_mul(1_u64 << entry.failures.min(5))
                    .min(3600),
            )
        };
        if entry
            .requested_at
            .is_some_and(|at| now.saturating_duration_since(at) < wait)
        {
            probe_skipped(
                Some(&account.id),
                if manual { "debounce" } else { "interval" },
                manual,
            );
            continue;
        }
        // 终态保持以缓存事实（`entry.terminal`）为判据，而不是可见状态：回调闩锁下的回落
        // 探测失败时快照仍显示回调数据，保持照样管住自动重试。
        if !manual {
            match &entry.terminal {
                Some(TerminalHold { until: None, .. }) => {
                    probe_skipped(Some(&account.id), "terminal_exhausted", manual);
                    continue;
                }
                Some(TerminalHold {
                    until: Some(until), ..
                }) if now < *until => {
                    probe_skipped(Some(&account.id), "terminal_hold", manual);
                    continue;
                }
                // TTL 到期或保持已被自愈撤销：允许重新探测。
                _ => {}
            }
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
            entry.queued = true;
            entry.requested_at = Some(now);
            entry.attempted_at_ms = now_ms;
            // 派发不改 status/message：「正在读取」由响应侧的 in_flight 表达。
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
/// 全局闸门（unbound_pane）不针对具体账号，不带 `account_id` 字段。
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

/// 把一次探测结果并入缓存。`observed_at_ms` 只在拿到真实额度（Ready）时更新；
/// 失败只推进 failures / 终态保持，并保留已有样本或回调快照。
fn merge_result(entry: &mut CacheEntry, snapshot: AccountUsageSnapshot, now: Instant) {
    entry.retry_after = snapshot
        .message
        .as_deref()
        .and_then(|message| message.rsplit_once("retry_after=")?.1.parse::<u64>().ok())
        .map(|seconds| now + Duration::from_secs(seconds.min(3600)));
    if snapshot.status == ObservationStatus::Ready {
        entry.failures = 0;
        entry.terminal = None;
        // 探测拿到了真实额度：回调闩锁让位，直到下一次官方回调再接管。
        entry.callback_until_ms = None;
        entry.snapshot = snapshot;
        return;
    }
    entry.failures = if snapshot.status == ObservationStatus::Error {
        entry.failures.saturating_add(1)
    } else {
        0
    };
    entry.terminal =
        is_terminal(snapshot.status).then(|| next_terminal_hold(entry.terminal.as_ref(), now));
    if entry.callback_latched() {
        // 回落探测没有拿到额度：官方回调的快照更可信，只记录这次尝试。
        return;
    }
    if snapshot.status == ObservationStatus::Error && !entry.snapshot.metrics.is_empty() {
        entry.snapshot.status = ObservationStatus::Stale;
        entry.snapshot.message = snapshot.message;
        return;
    }
    let observed_at_ms = entry.snapshot.observed_at_ms;
    entry.snapshot = AccountUsageSnapshot {
        observed_at_ms,
        ..snapshot
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claude_account() -> UsageAccountConfig {
        UsageAccountConfig {
            id: "claude:default".into(),
            label: "Claude Code".into(),
            agent: "claude".into(),
            provider: "claude".into(),
            auth_mode: "cli".into(),
            ..Default::default()
        }
    }

    fn fresh_entry(account: &UsageAccountConfig) -> CacheEntry {
        CacheEntry::new(empty_snapshot(account))
    }

    fn snapshot(status: ObservationStatus) -> AccountUsageSnapshot {
        AccountUsageSnapshot {
            account_id: "claude:default".into(),
            status,
            observed_at_ms: 999,
            message: Some("探测结果".into()),
            ..Default::default()
        }
    }

    fn ready_snapshot(used: f64, observed_at_ms: u64) -> AccountUsageSnapshot {
        AccountUsageSnapshot {
            status: ObservationStatus::Ready,
            observed_at_ms,
            metrics: vec![UsageMetric {
                used: Some(used),
                ..Default::default()
            }],
            ..snapshot(ObservationStatus::Ready)
        }
    }

    /// 测试用的服务状态：账号清单、冷缓存与临时持久化路径；不依赖本机 PATH 与配置文件。
    fn test_state(config: AccountUsageConfig, accounts: Vec<UsageAccountConfig>) -> ServiceState {
        let cache = accounts
            .iter()
            .map(|account| (account.id.clone(), fresh_entry(account)))
            .collect();
        let state_path = std::env::temp_dir()
            .join(format!(
                "herdr-usage-state-{}-{}",
                std::process::id(),
                super::super::now_ms()
            ))
            .join("state.json");
        ServiceState {
            config,
            config_stamp: None,
            accounts,
            cache,
            bindings: HashMap::new(),
            saved: persistence::Saved::default(),
            persisted: None,
            state_path,
            rejections: apply::Rejections::default(),
            next_query: 1,
            missing_since: HashMap::new(),
            scanned_at: None,
        }
    }

    fn cleanup(state: &ServiceState) {
        if let Some(dir) = state.state_path.parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    /// 派发一次并返回是否入队。
    fn dispatch(entry: &mut CacheEntry, manual: bool) -> bool {
        let account = claude_account();
        let mut cache = HashMap::from([(
            account.id.clone(),
            std::mem::replace(entry, fresh_entry(&account)),
        )]);
        let (tasks, input) = mpsc::sync_channel(1);
        request_accounts(
            &UsageParams {
                account_id: Some(account.id.clone()),
                ..Default::default()
            },
            manual,
            std::slice::from_ref(&account),
            &AccountUsageConfig::default(),
            &HashMap::new(),
            &mut cache,
            &tasks,
            &mut 1,
        );
        if let Some(restored) = cache.remove(&account.id) {
            *entry = restored;
        }
        input.try_recv().is_ok()
    }

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
        let accounts = configured_accounts(&config, &|_| true);
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
    fn configured_accounts_dedupe_user_ids_and_only_add_installed_defaults() {
        let config = AccountUsageConfig {
            accounts: vec![
                UsageAccountConfig {
                    id: "work".into(),
                    label: "first".into(),
                    agent: "Claude Code".into(),
                    ..Default::default()
                },
                UsageAccountConfig {
                    id: "work".into(),
                    label: "dup".into(),
                    agent: "codex".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let accounts = configured_accounts(&config, &|provider| provider.agent == "kimi");
        assert_eq!(
            accounts.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(),
            vec!["work", "kimi:default"],
            "同 id 保留首个；只有已安装厂商补隐式默认账号"
        );
        assert_eq!(accounts[0].label, "first");
        assert_eq!(accounts[0].agent, "claude", "别名规范化");
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
        let mut entry = CacheEntry::new(ready_snapshot(7.0, 12));
        entry.generation = 1;
        let now = Instant::now();
        merge_result(
            &mut entry,
            AccountUsageSnapshot {
                status: ObservationStatus::Error,
                observed_at_ms: 99,
                message: Some("HTTP 429；retry_after=60".into()),
                ..Default::default()
            },
            now,
        );
        assert_eq!(entry.snapshot.observed_at_ms, 12);
        assert_eq!(entry.snapshot.metrics[0].used, Some(7.0));
        assert_eq!(entry.snapshot.status, ObservationStatus::Stale);
        assert_eq!(entry.failures, 1);
        assert!(entry.retry_after.is_some());
        merge_result(&mut entry, ready_snapshot(8.0, 100), now);
        assert_eq!(entry.snapshot.observed_at_ms, 100);
        assert_eq!(entry.failures, 0);
        assert!(entry.retry_after.is_none());
    }

    // ---- B-6：失败不更新 observed_at，派发不改状态 ----

    #[test]
    fn failed_probes_keep_observed_at_and_only_ready_results_update_it() {
        let now = Instant::now();
        let mut entry = CacheEntry::new(ready_snapshot(7.0, 12));
        // 终态：快照被替换（旧额度不再可信），但 observed_at 仍是最后一次成功的时间。
        merge_result(
            &mut entry,
            snapshot(ObservationStatus::NotAuthenticated),
            now,
        );
        assert_eq!(entry.snapshot.status, ObservationStatus::NotAuthenticated);
        assert!(entry.snapshot.metrics.is_empty());
        assert_eq!(entry.snapshot.observed_at_ms, 12);
        assert_eq!(entry.snapshot.message.as_deref(), Some("探测结果"));
        // 从未成功过的账号失败后 observed_at 仍为 0。
        let mut cold = fresh_entry(&claude_account());
        merge_result(&mut cold, snapshot(ObservationStatus::Error), now);
        assert_eq!(cold.snapshot.observed_at_ms, 0);
        assert_eq!(cold.snapshot.status, ObservationStatus::Error);
        merge_result(&mut cold, ready_snapshot(1.0, 500), now);
        assert_eq!(cold.snapshot.observed_at_ms, 500);
    }

    // ---- B-1：终态慢 TTL、重试上限与指纹自愈 ----

    #[test]
    fn terminal_results_hold_with_a_slow_ttl_and_stop_auto_retrying_after_the_limit() {
        let now = Instant::now();
        let mut entry = fresh_entry(&claude_account());
        let expected_minutes = [10_u64, 20, 40, 60, 60, 60];
        for (index, minutes) in expected_minutes.iter().enumerate() {
            merge_result(
                &mut entry,
                snapshot(ObservationStatus::NotAuthenticated),
                now,
            );
            let hold = entry.terminal.as_ref().expect("终态应留下保持记录");
            assert_eq!(hold.streak, index as u32 + 1);
            let until = hold.until.expect("上限内仍有自动重试时间");
            assert_eq!(
                until.duration_since(now),
                Duration::from_secs(minutes * 60),
                "第 {} 次终态的 TTL",
                index + 1
            );
            assert!(!dispatch(&mut entry, false), "TTL 内不自动重试");
            assert!(dispatch(&mut entry, true), "显式刷新穿透终态");
            // 探测派发后模拟完成，让下一轮不受 in_flight 与防抖影响。
            entry.in_flight = false;
            entry.requested_at = None;
        }
        merge_result(
            &mut entry,
            snapshot(ObservationStatus::NotAuthenticated),
            now,
        );
        let hold = entry.terminal.as_ref().expect("终态保持");
        assert_eq!(hold.streak, 7);
        assert!(hold.until.is_none(), "超过上限后自动重试用尽");
        assert!(!dispatch(&mut entry, false));
        assert!(dispatch(&mut entry, true), "用尽后显式刷新仍可探测");
        entry.in_flight = false;
        entry.requested_at = None;
        // 成功一次即清零。
        merge_result(&mut entry, ready_snapshot(1.0, 5), now);
        assert!(entry.terminal.is_none());
        merge_result(&mut entry, snapshot(ObservationStatus::Unsupported), now);
        assert_eq!(entry.terminal.as_ref().map(|hold| hold.streak), Some(1));
        // 非终态的失败（瞬时 Error）也结束保持，转由指数退避管辖。
        merge_result(&mut entry, snapshot(ObservationStatus::Error), now);
        assert!(entry.terminal.is_none());
        assert_eq!(entry.failures, 1);
    }

    #[test]
    fn expired_terminal_hold_allows_one_automatic_retry() {
        let now = Instant::now();
        let mut entry = fresh_entry(&claude_account());
        merge_result(
            &mut entry,
            snapshot(ObservationStatus::PermissionDenied),
            now,
        );
        // 把保持截止时间拨到「此刻」：随后的派发发生在它之后，视为 TTL 到期。
        if let Some(hold) = &mut entry.terminal {
            hold.until = Some(now);
        }
        assert!(dispatch(&mut entry, false), "TTL 到期后自动重试一次");
        assert!(entry.in_flight);
    }

    #[test]
    fn terminal_hold_heals_when_the_cli_or_credential_fingerprint_changes() {
        let now = Instant::now();
        let mut entry = fresh_entry(&claude_account());
        merge_result(
            &mut entry,
            snapshot(ObservationStatus::NotAuthenticated),
            now,
        );
        entry.requested_at = Some(now);
        let recorded = registry::ProbeFingerprint {
            credentials: vec![None],
            ..Default::default()
        };
        // 指纹尚未采集：不判定。
        assert!(!heal_terminal(&mut entry, &recorded));
        if let Some(hold) = &mut entry.terminal {
            hold.fingerprint = Some(recorded.clone());
        }
        assert!(!heal_terminal(&mut entry, &recorded), "指纹未变不自愈");
        assert!(entry.terminal.is_some());
        assert!(!dispatch(&mut entry, false));

        let changed = registry::ProbeFingerprint {
            credentials: vec![Some(("/tmp/creds".into(), None, 3))],
            ..Default::default()
        };
        assert!(heal_terminal(&mut entry, &changed));
        assert!(entry.terminal.is_none());
        assert_eq!(entry.snapshot.status, ObservationStatus::NotAuthenticated);
        // 自愈只撤销保持：`requested_at + 自动间隔` 仍是下限，指纹抖动最多每个周期重试一次。
        assert!(entry.requested_at.is_some(), "自愈不清 requested_at");
        assert!(!dispatch(&mut entry, false), "刚探测过：自愈后仍等自动间隔");
        entry.requested_at = Some(now - Duration::from_secs(400));
        assert!(dispatch(&mut entry, false), "间隔到期后自愈的账号重新探测");
        assert!(!heal_terminal(&mut entry, &changed), "没有保持时不判定");
    }

    // ---- B-2：显式刷新的防抖与穿透 ----

    #[test]
    fn minimum_interval_is_the_single_source_for_advertising_and_polling() {
        let config = AccountUsageConfig::default();
        let claude = registry::provider("claude");
        let cursor = registry::provider("cursor");
        let pi = registry::provider("pi");
        // 播报值：交互/JSON 探测按 CLI 周期，报表接口按 API 周期，回调型为 0。
        assert_eq!(minimum_interval_seconds(claude, false, &config), 300);
        assert_eq!(minimum_interval_seconds(cursor, false, &config), 60);
        assert_eq!(minimum_interval_seconds(pi, false, &config), 0);
        assert_eq!(minimum_interval_seconds(claude, true, &config), 60);
        // 自动轮询取同一真源；回调型厂商的占位探测按 CLI 周期而不是 0。
        let account = |agent: &str, auth_mode: &str| UsageAccountConfig {
            agent: agent.into(),
            auth_mode: auth_mode.into(),
            ..Default::default()
        };
        assert_eq!(refresh_interval(&account("cursor", "cli"), &config), 60);
        assert_eq!(refresh_interval(&account("pi", "cli"), &config), 300);
        assert_eq!(refresh_interval(&account("claude", "api"), &config), 60);
        assert_eq!(refresh_interval(&claude_account(), &config), 300);
        // 显式刷新防抖是固定 10 s，与厂商间隔无关。
        assert_eq!(MANUAL_DEBOUNCE, Duration::from_secs(10));
    }

    #[test]
    fn manual_refresh_bypasses_exponential_backoff_and_terminal_states_but_respects_debounce() {
        let now = Instant::now();
        let mut entry = fresh_entry(&claude_account());
        // 三次失败：自动退避 300 s × 8；同时处于终态保持。
        entry.failures = 3;
        merge_result(
            &mut entry,
            snapshot(ObservationStatus::NotAuthenticated),
            now,
        );
        entry.failures = 3;
        entry.requested_at = Some(now - Duration::from_secs(40));
        assert!(!dispatch(&mut entry, false), "自动轮询受退避与终态双重拦截");
        assert!(entry.manual_requested_at_ms.is_none());
        assert!(
            dispatch(&mut entry, true),
            "40 s 前探测过：显式刷新已过 10 s 防抖"
        );
        assert!(entry.manual_requested_at_ms.is_some());
        assert!(entry.in_flight && entry.queued);
        assert!(entry.attempted_at_ms > 0);

        let mut entry = fresh_entry(&claude_account());
        entry.requested_at = Some(now - Duration::from_secs(5));
        assert!(!dispatch(&mut entry, true), "5 s 前刚探测：显式刷新被防抖");
        assert!(entry.manual_requested_at_ms.is_some(), "被防抖的请求也登记");
        let state = refresh_state(&entry, &claude_account(), now, 1_000_000);
        let next = state.next_allowed_at_ms.expect("被防抖时给出下次允许时间");
        assert!(
            (1_004_000..=1_005_100).contains(&next),
            "≈ now + 5 s，实际 {next}"
        );

        // 厂商退避（HTTP 429）显式刷新也不豁免。
        let mut entry = fresh_entry(&claude_account());
        entry.retry_after = Some(now + Duration::from_secs(60));
        assert!(!dispatch(&mut entry, true));
    }

    // ---- B-7：回调闩锁的生命周期 ----

    #[test]
    fn expired_callback_latch_falls_back_to_probes_but_only_ready_results_overwrite() {
        let now = Instant::now();
        let now_ms = super::super::now_ms();
        let mut entry = CacheEntry::new(ready_snapshot(12.0, now_ms));
        entry.callback_until_ms = Some(now_ms + CALLBACK_LATCH_MS);
        assert!(!dispatch(&mut entry, false), "闩锁新鲜：不回落探测");
        assert!(dispatch(&mut entry, true), "显式刷新可穿透闩锁");
        entry.in_flight = false;
        entry.requested_at = None;

        entry.callback_until_ms = Some(now_ms.saturating_sub(1));
        assert!(dispatch(&mut entry, false), "闩锁过期：允许回落探测");
        entry.in_flight = false;
        entry.requested_at = None;
        // 回落失败：回调快照原样保留，只推进终态保持。
        merge_result(
            &mut entry,
            snapshot(ObservationStatus::NotAuthenticated),
            now,
        );
        assert_eq!(entry.snapshot.status, ObservationStatus::Ready);
        assert_eq!(entry.snapshot.metrics[0].used, Some(12.0));
        assert_eq!(entry.snapshot.observed_at_ms, now_ms);
        assert!(entry.callback_latched());
        assert!(entry.terminal.is_some());
        assert!(
            !dispatch(&mut entry, false),
            "回落失败进入终态保持：慢 TTL 内不再自动回落，即使可见状态仍是回调写入的 Ready"
        );
        assert!(dispatch(&mut entry, true), "显式刷新仍可穿透");
        entry.in_flight = false;
        entry.requested_at = None;
        // 连续失败直到自动重试用尽：此后即使闩锁早已过期也不再自动派发。
        for _ in 0..TERMINAL_AUTO_RETRY_LIMIT {
            merge_result(
                &mut entry,
                snapshot(ObservationStatus::NotAuthenticated),
                now,
            );
        }
        assert!(entry
            .terminal
            .as_ref()
            .is_some_and(|hold| hold.until.is_none()));
        assert!(!dispatch(&mut entry, false), "自动重试用尽后不再派发");
        assert!(dispatch(&mut entry, true), "用尽后显式刷新仍可探测");
        entry.in_flight = false;
        entry.requested_at = None;
        // 回落成功：探测接管，闩锁清空。
        merge_result(&mut entry, ready_snapshot(3.0, now_ms + 1), now);
        assert_eq!(entry.snapshot.metrics[0].used, Some(3.0));
        assert!(!entry.callback_latched());
        assert!(entry.terminal.is_none());
    }

    #[test]
    fn callback_only_providers_never_fall_back_to_probes() {
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
        let mut entry = CacheEntry::new(AccountUsageSnapshot {
            metrics: vec![UsageMetric {
                used: Some(12.0),
                ..Default::default()
            }],
            observed_at_ms: 1,
            ..empty_snapshot(&account)
        });
        entry.generation = 1;
        // 早已过期的闩锁 + 显式刷新：回调型厂商依旧不探测，占位不会覆盖回调数据。
        entry.callback_until_ms = Some(1);
        let mut cache = HashMap::from([(account.id.clone(), entry)]);
        let (tasks, input) = mpsc::sync_channel(1);
        request_accounts(
            &UsageParams {
                account_id: Some(account.id.clone()),
                ..Default::default()
            },
            true,
            std::slice::from_ref(&account),
            &AccountUsageConfig::default(),
            &HashMap::new(),
            &mut cache,
            &tasks,
            &mut 2,
        );
        assert!(input.try_recv().is_err());
        assert_eq!(cache[&account.id].snapshot.metrics[0].used, Some(12.0));
    }

    // ---- 响应侧并行结构 ----

    #[test]
    fn refresh_state_exposes_progress_debounce_and_retry_after() {
        let now = Instant::now();
        let account = claude_account();
        let cold = fresh_entry(&account);
        let state = refresh_state(&cold, &account, now, 1_000);
        assert_eq!(
            state,
            UsageRefreshState {
                account_id: "claude:default".into(),
                ..Default::default()
            }
        );

        let mut entry = fresh_entry(&account);
        entry.in_flight = true;
        entry.queued = true;
        entry.requested_at = Some(now);
        entry.attempted_at_ms = 900;
        entry.manual_requested_at_ms = Some(950);
        entry.retry_after = Some(now + Duration::from_secs(60));
        entry.trust_required = true;
        let state = refresh_state(&entry, &account, now, 1_000);
        assert!(state.in_flight && state.queued && state.trust_required);
        assert_eq!(state.attempted_at_ms, Some(900));
        assert_eq!(state.requested_at_ms, Some(950));
        assert_eq!(state.retry_after_ms, Some(61_000));
        // 防抖 10 s 与退避 60 s 取更晚者。
        assert_eq!(state.next_allowed_at_ms, Some(61_000));
        entry.retry_after = Some(now - Duration::from_secs(1));
        let state = refresh_state(&entry, &account, now, 1_000);
        assert_eq!(state.retry_after_ms, None, "已过期的退避不再暴露");
        assert_eq!(state.next_allowed_at_ms, Some(11_000));
        assert!(!state.binding_inferred && state.pending_binding.is_none());
        assert!(!state.callback_only, "claude 有可回落的探测");

        // 回调型厂商：显式刷新不会产生新数据，客户端据此禁用刷新动作。
        let pi = UsageAccountConfig {
            id: "pi:default".into(),
            agent: "pi".into(),
            ..Default::default()
        };
        let state = refresh_state(&fresh_entry(&pi), &pi, now, 1_000);
        assert!(state.callback_only);
    }

    #[test]
    fn unbound_pane_with_a_single_candidate_is_inferred_without_writing_a_binding() {
        let accounts = vec![
            claude_account(),
            UsageAccountConfig {
                id: "kimi:default".into(),
                agent: "kimi".into(),
                ..Default::default()
            },
        ];
        let params = |agent: Option<&str>, account_id: Option<&str>| UsageParams {
            agent: agent.map(Into::into),
            account_id: account_id.map(Into::into),
            pane_id: Some("wT:p9".into()),
        };
        assert_eq!(
            infer_binding(&params(Some("Claude Code"), None), &accounts).as_deref(),
            Some("claude:default"),
            "别名也能定位唯一候选"
        );
        assert_eq!(infer_binding(&params(None, None), &accounts), None);
        assert_eq!(
            infer_binding(&params(Some("claude"), Some("claude:default")), &accounts),
            None,
            "显式账号不走推断"
        );
        assert_eq!(
            infer_binding(&params(Some("no-such"), None), &accounts),
            None
        );
        let two = vec![
            claude_account(),
            UsageAccountConfig {
                id: "claude:home".into(),
                agent: "claude".into(),
                ..Default::default()
            },
        ];
        assert_eq!(infer_binding(&params(Some("claude"), None), &two), None);
    }

    #[test]
    fn usage_requests_return_refresh_state_and_infer_unbound_single_candidates() {
        let (tasks, input) = mpsc::sync_channel(4);
        let account = claude_account();
        let mut state = test_state(AccountUsageConfig::default(), vec![account.clone()]);
        // 未绑定 pane + 唯一候选：返回真实数据并探测，标 binding_inferred，不写绑定。
        let result = state.usage(
            &UsageParams {
                agent: Some("claude".into()),
                pane_id: Some("wT:p9".into()),
                ..Default::default()
            },
            false,
            &tasks,
        );
        let ResponseResult::AccountUsage { accounts, refresh } = result else {
            panic!("not account_usage");
        };
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].status, ObservationStatus::Warming);
        let refresh = refresh.expect("响应带并行结构");
        assert_eq!(refresh.len(), 1);
        assert_eq!(refresh[0].account_id, "claude:default");
        assert!(refresh[0].binding_inferred);
        assert!(refresh[0].in_flight && refresh[0].queued);
        assert!(input.try_recv().is_ok(), "唯一候选被探测");
        assert!(state.bindings.is_empty());

        // 未绑定 pane 且无法推断（不带 agent）：占位 NeedsBinding、不探测。
        let result = state.usage(
            &UsageParams {
                pane_id: Some("wT:p9".into()),
                ..Default::default()
            },
            false,
            &tasks,
        );
        let ResponseResult::AccountUsage { accounts, refresh } = result else {
            panic!("not account_usage");
        };
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].status, ObservationStatus::NeedsBinding);
        assert!(!refresh.expect("并行结构")[0].binding_inferred);
        assert!(input.try_recv().is_err());

        // 被拒的回调留下待办：同 agent 的账号带上 pending_binding。
        let rejected_at_ms = super::super::now_ms();
        state.rejections.record_pending(
            "wT:p9",
            "claude",
            vec!["claude:default".into()],
            rejected_at_ms,
        );
        let result = state.usage(
            &UsageParams {
                agent: Some("claude".into()),
                ..Default::default()
            },
            false,
            &tasks,
        );
        let ResponseResult::AccountUsage { refresh, .. } = result else {
            panic!("not account_usage");
        };
        let pending = refresh.expect("并行结构")[0]
            .pending_binding
            .clone()
            .expect("待办绑定");
        assert_eq!(pending.pane_id, "wT:p9");
        assert_eq!(pending.agent, "claude");
        assert_eq!(pending.candidates, vec!["claude:default"]);
        assert_eq!(pending.rejected_at_ms, rejected_at_ms);
    }

    #[test]
    fn unparsable_config_keeps_accounts_and_bindings_until_it_parses_again() {
        let _guard = crate::config::test_config_env_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = std::env::temp_dir().join(format!(
            "herdr-usage-config-{}-{}",
            std::process::id(),
            super::super::now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let explicit = UsageAccountConfig {
            id: "claude:work".into(),
            label: "工作".into(),
            agent: "claude".into(),
            provider: "claude".into(),
            auth_mode: "cli".into(),
            ..Default::default()
        };
        let config = AccountUsageConfig {
            accounts: vec![explicit.clone()],
            ..Default::default()
        };
        let mut state = test_state(config.clone(), vec![explicit.clone()]);
        state.bindings.insert("pane-1".into(), "claude:work".into());
        state
            .saved
            .identities
            .insert("claude:work".into(), "me@example.test".into());
        let now = Instant::now();

        // 真实的坏 TOML 走真实的 Config::load：账号、绑定与身份一律不动。
        std::fs::write(&path, "[account_usage\nenabled = ").unwrap();
        std::env::set_var(crate::config::CONFIG_PATH_ENV_VAR, &path);
        state.reload(&|_| false, now);
        assert_eq!(state.config, config, "解析失败沿用上一份配置");
        assert_eq!(state.accounts, vec![explicit.clone()]);
        assert_eq!(
            state.bindings.get("pane-1").map(String::as_str),
            Some("claude:work")
        );
        assert!(state.saved.identities.contains_key("claude:work"));

        // 合法 TOML 但显式删掉了该账号：这才是剪枝绑定与身份的唯一路径。
        std::fs::write(&path, "[account_usage]\nenabled = true\n").unwrap();
        state.reload(&|_| false, now + registry::AVAILABILITY_TTL);
        assert!(state.config.accounts.is_empty());
        assert!(state.accounts.is_empty());
        assert!(state.bindings.is_empty(), "显式删除的账号剪掉绑定");
        assert!(!state.saved.identities.contains_key("claude:work"));

        std::env::remove_var(crate::config::CONFIG_PATH_ENV_VAR);
        cleanup(&state);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn transient_cli_detection_loss_keeps_bindings_identities_and_cache() {
        let account = claude_account();
        let mut state = test_state(AccountUsageConfig::default(), vec![account.clone()]);
        state
            .bindings
            .insert("pane-1".into(), "claude:default".into());
        state
            .saved
            .identities
            .insert("claude:default".into(), "me@example.test".into());
        if let Some(entry) = state.cache.get_mut("claude:default") {
            entry.snapshot = ready_snapshot(7.0, 12);
        }
        let now = Instant::now();
        let none = HashSet::new();

        // 检测抖动：这一轮清单里没有 claude。宽限期内账号仍在清单里，缓存/绑定/身份都不动。
        state.apply_accounts(Vec::new(), &none, now);
        assert_eq!(state.accounts, vec![account.clone()]);
        assert!(state.bindings.contains_key("pane-1"));
        assert!(state.saved.identities.contains_key("claude:default"));
        assert_eq!(
            state.cache["claude:default"].snapshot.status,
            ObservationStatus::Ready
        );
        // 宽限期内恢复：缓存样本原样复用，不重建、不重新探测。
        state.apply_accounts(vec![account.clone()], &none, now + Duration::from_secs(60));
        assert_eq!(
            state.cache["claude:default"].snapshot.metrics[0].used,
            Some(7.0)
        );
        assert!(state.missing_since.is_empty());

        // 持续检测不到超过宽限期：移出清单与缓存，但绑定与身份保留（读路径惰性过滤）。
        state.apply_accounts(Vec::new(), &none, now + Duration::from_secs(100));
        assert_eq!(state.accounts.len(), 1, "宽限期内");
        state.apply_accounts(
            Vec::new(),
            &none,
            now + Duration::from_secs(100) + IMPLICIT_ACCOUNT_GRACE,
        );
        assert!(state.accounts.is_empty());
        assert!(!state.cache.contains_key("claude:default"));
        assert!(
            state.bindings.contains_key("pane-1"),
            "绑定不因检测消失而剪枝"
        );
        assert!(state.saved.identities.contains_key("claude:default"));
        assert!(
            !matches_account(
                &UsageParams {
                    pane_id: Some("pane-1".into()),
                    ..Default::default()
                },
                &ready_snapshot(1.0, 1),
                &state.bindings
            ) || state.accounts.is_empty()
        );

        // 只有用户在配置里显式删除的账号才剪枝绑定与身份。
        let explicit = UsageAccountConfig {
            id: "claude:work".into(),
            agent: "claude".into(),
            provider: "claude".into(),
            auth_mode: "cli".into(),
            ..Default::default()
        };
        state.apply_accounts(vec![explicit.clone()], &none, now);
        state.bindings.insert("pane-2".into(), "claude:work".into());
        state
            .saved
            .identities
            .insert("claude:work".into(), "work@example.test".into());
        let removed = HashSet::from(["claude:work".to_string()]);
        state.apply_accounts(Vec::new(), &removed, now);
        assert!(!state.bindings.contains_key("pane-2"));
        assert!(!state.saved.identities.contains_key("claude:work"));
        assert!(!state.cache.contains_key("claude:work"));
        assert!(
            state.bindings.contains_key("pane-1"),
            "其他账号的绑定不受影响"
        );
        cleanup(&state);
    }

    #[test]
    fn account_config_changes_keep_the_saved_identity_and_label_edits_stay_in_place() {
        let account = claude_account();
        let mut state = test_state(AccountUsageConfig::default(), vec![account.clone()]);
        state
            .saved
            .identities
            .insert("claude:default".into(), "me@example.test".into());
        state
            .bindings
            .insert("pane-1".into(), "claude:default".into());
        if let Some(entry) = state.cache.get_mut("claude:default") {
            entry.snapshot = AccountUsageSnapshot {
                account_identity: Some("me@example.test".into()),
                ..ready_snapshot(7.0, 12)
            };
        }
        let now = Instant::now();
        let none = HashSet::new();

        // 只改 label：就地更新展示名，样本与绑定都不动。
        let relabeled = UsageAccountConfig {
            label: "我的 Claude".into(),
            ..account.clone()
        };
        state.apply_accounts(vec![relabeled.clone()], &none, now);
        let entry = &state.cache["claude:default"];
        assert_eq!(entry.snapshot.account_label, "我的 Claude");
        assert_eq!(entry.snapshot.metrics[0].used, Some(7.0));
        assert!(state.bindings.contains_key("pane-1"), "label 变化不剪绑定");

        // 改 profile_dir：探测目标变了，重建条目并撤销绑定，但公开身份沿用已保存的值，
        // 下一次探测才能检测出登录切换。
        let moved = UsageAccountConfig {
            profile_dir: Some(std::path::PathBuf::from("/tmp/other-profile")),
            ..relabeled.clone()
        };
        state.apply_accounts(vec![moved.clone()], &none, now);
        let entry = &state.cache["claude:default"];
        assert!(entry.snapshot.metrics.is_empty());
        assert_eq!(entry.snapshot.status, ObservationStatus::Warming);
        assert_eq!(
            entry.snapshot.account_identity.as_deref(),
            Some("me@example.test")
        );
        assert!(!state.bindings.contains_key("pane-1"));
        assert_eq!(state.accounts, vec![moved]);
        cleanup(&state);
    }

    // ---- 回调闩锁到期：快照降为缓存态 ----

    #[test]
    fn expired_callback_latch_marks_the_snapshot_as_cached_until_fresh_data_arrives() {
        let now_ms = super::super::now_ms();
        let account = claude_account();
        let mut entry = CacheEntry::new(ready_snapshot(12.0, now_ms - 1));
        entry.callback_until_ms = Some(now_ms + 1_000);
        let mut cache = HashMap::from([(account.id.clone(), entry)]);
        assert!(
            age_out_callbacks(&mut cache, now_ms).is_empty(),
            "闩锁新鲜不动"
        );
        if let Some(entry) = cache.get_mut(&account.id) {
            entry.callback_until_ms = Some(now_ms - 1);
        }
        assert_eq!(
            age_out_callbacks(&mut cache, now_ms),
            vec!["claude:default".to_string()]
        );
        let entry = &cache[&account.id];
        assert_eq!(entry.snapshot.status, ObservationStatus::Stale);
        assert_eq!(entry.snapshot.metrics[0].used, Some(12.0), "样本保留");
        assert_eq!(entry.snapshot.observed_at_ms, now_ms - 1, "采样时间保留");
        assert_eq!(
            entry.snapshot.message.as_deref(),
            Some(CALLBACK_STALE_MESSAGE)
        );
        assert!(
            entry.callback_latched(),
            "闩锁本身保留：回落探测仍只允许 Ready 覆盖"
        );
        assert!(
            age_out_callbacks(&mut cache, now_ms).is_empty(),
            "只改写一次"
        );

        // 读路径看到的也是缓存态，且闩锁过期允许回落探测；回落成功后恢复 Ready。
        let mut state = test_state(AccountUsageConfig::default(), vec![account.clone()]);
        state.cache = cache;
        let (tasks, input) = mpsc::sync_channel(4);
        let ResponseResult::AccountUsage { accounts, .. } = state.usage(
            &UsageParams {
                account_id: Some(account.id.clone()),
                ..Default::default()
            },
            false,
            &tasks,
        ) else {
            panic!("not account_usage");
        };
        assert_eq!(accounts[0].status, ObservationStatus::Stale);
        assert!(input.try_recv().is_ok(), "闩锁过期允许回落探测");
        let entry = state.cache.get_mut(&account.id).expect("缓存条目");
        // 回落失败：仍是缓存态，回调样本不被占位覆盖。
        merge_result(
            entry,
            snapshot(ObservationStatus::NotAuthenticated),
            Instant::now(),
        );
        assert_eq!(entry.snapshot.status, ObservationStatus::Stale);
        assert_eq!(entry.snapshot.metrics[0].used, Some(12.0));
        merge_result(entry, ready_snapshot(3.0, now_ms + 5), Instant::now());
        assert_eq!(entry.snapshot.status, ObservationStatus::Ready);
        assert!(!entry.callback_latched());
        cleanup(&state);
    }

    // ---- 订阅参数与读路径同一份解析 ----

    #[test]
    fn subscriptions_are_normalized_like_reads_so_inferred_accounts_receive_events() {
        let kimi = UsageAccountConfig {
            id: "kimi:default".into(),
            agent: "kimi".into(),
            ..Default::default()
        };
        let mut state = test_state(AccountUsageConfig::default(), vec![claude_account(), kimi]);
        let unbound = UsageParams {
            agent: Some("claude".into()),
            pane_id: Some("wT:p9".into()),
            account_id: None,
        };
        let selection = state.select(&unbound);
        assert!(selection.inferred && !selection.placeholder);
        assert_eq!(
            selection.request.account_id.as_deref(),
            Some("claude:default")
        );
        assert_eq!(selection.request.pane_id, None);
        assert_eq!(selection.request, selection.matching);
        let value = &state.cache["claude:default"].snapshot;
        assert!(matches_account(&selection.request, value, &state.bindings));
        assert!(
            !matches_account(&unbound, value, &state.bindings),
            "原参数在事件通道永远不匹配：这正是要归一化的原因"
        );

        // 歧义（不带 agent）：占位；派发沿用原参数，匹配时清掉未绑定 pane。
        let ambiguous = UsageParams {
            pane_id: Some("wT:p9".into()),
            ..Default::default()
        };
        let selection = state.select(&ambiguous);
        assert!(selection.placeholder && !selection.inferred);
        assert_eq!(selection.request, ambiguous);
        assert_eq!(selection.matching.pane_id, None);

        // 已绑定 pane：原样。
        state.bindings.insert("wT:p9".into(), "kimi:default".into());
        let bound = UsageParams {
            pane_id: Some("wT:p9".into()),
            ..Default::default()
        };
        let selection = state.select(&bound);
        assert!(!selection.inferred && !selection.placeholder);
        assert_eq!(selection.request, bound);
        assert_eq!(selection.matching, bound);
        cleanup(&state);
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
        /// 显式刷新是否被登记到 `manual_requested_at_ms`；`None` 表示与 `manual` 相同。
        registered: Option<bool>,
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
            // 派发不再改写 message：冷条目始终是「尚未查询」。
            message: Some("尚未查询"),
            registered: None,
        }
    }

    fn hold(entry: &mut CacheEntry, status: ObservationStatus, until: Option<Instant>) {
        entry.snapshot.status = status;
        entry.terminal = Some(TerminalHold {
            streak: 1,
            until,
            fingerprint: None,
        });
    }

    #[test]
    fn request_accounts_gates_are_each_covered_by_a_contrasting_case() {
        let account = claude_account();
        let cases = vec![
            gate("合法输入被接受"),
            Gate {
                enabled: false,
                enqueued: false,
                ..gate("account_usage.enabled=false 时一个探测都不发")
            },
            Gate {
                manual: true,
                enabled: false,
                enqueued: false,
                ..gate("显式刷新在 enabled=false 时不派发但仍登记")
            },
            Gate {
                manual: true,
                params: UsageParams {
                    pane_id: Some("pane-1".into()),
                    ..Default::default()
                },
                enqueued: false,
                registered: Some(false),
                ..gate("显式刷新遇到未绑定 pane：不对应任何账号，不派发也无处登记")
            },
            Gate {
                params: UsageParams {
                    pane_id: Some("pane-1".into()),
                    ..Default::default()
                },
                enqueued: false,
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
                ..gate("厂商不匹配的账号被过滤")
            },
            Gate {
                prepare: |entry| entry.in_flight = true,
                enqueued: false,
                ..gate("进行中的探测不重复派发")
            },
            Gate {
                prepare: |entry| entry.retry_after = Some(Instant::now() + Duration::from_secs(60)),
                enqueued: false,
                ..gate("retry_after 未到不派发")
            },
            Gate {
                manual: true,
                prepare: |entry| entry.retry_after = Some(Instant::now() + Duration::from_secs(60)),
                enqueued: false,
                ..gate("retry_after 未到即使手动也不派发")
            },
            Gate {
                prepare: |entry| {
                    entry.callback_until_ms = Some(super::super::now_ms() + CALLBACK_LATCH_MS);
                    entry.snapshot.status = ObservationStatus::Ready;
                    entry.snapshot.message = None;
                },
                enqueued: false,
                status: Some(ObservationStatus::Ready),
                message: None,
                ..gate("新鲜回调不回落探测")
            },
            Gate {
                prepare: |entry| {
                    entry.callback_until_ms = Some(super::super::now_ms().saturating_sub(1));
                    entry.snapshot.status = ObservationStatus::Ready;
                    entry.snapshot.message = None;
                },
                status: Some(ObservationStatus::Ready),
                message: None,
                ..gate("过期回调回落探测且不改状态")
            },
            Gate {
                manual: true,
                prepare: |entry| {
                    entry.callback_until_ms = Some(super::super::now_ms() + CALLBACK_LATCH_MS);
                    entry.snapshot.status = ObservationStatus::Ready;
                    entry.snapshot.message = None;
                },
                status: Some(ObservationStatus::Ready),
                message: None,
                ..gate("对照：显式刷新穿透新鲜回调闩锁")
            },
            Gate {
                prepare: |entry| {
                    entry.callback_until_ms = Some(super::super::now_ms().saturating_sub(1));
                    entry.snapshot.status = ObservationStatus::Ready;
                    entry.snapshot.message = None;
                    entry.terminal = Some(TerminalHold {
                        streak: 1,
                        until: Some(Instant::now() + Duration::from_secs(600)),
                        fingerprint: None,
                    });
                },
                enqueued: false,
                status: Some(ObservationStatus::Ready),
                message: None,
                ..gate("闩锁过期但终态保持未到期：不回落探测（保持以缓存事实为准，不看可见状态）")
            },
            Gate {
                manual: true,
                prepare: |entry| {
                    entry.callback_until_ms = Some(super::super::now_ms().saturating_sub(1));
                    entry.snapshot.status = ObservationStatus::Ready;
                    entry.snapshot.message = None;
                    entry.terminal = Some(TerminalHold {
                        streak: 1,
                        until: Some(Instant::now() + Duration::from_secs(600)),
                        fingerprint: None,
                    });
                },
                status: Some(ObservationStatus::Ready),
                message: None,
                ..gate("对照：显式刷新穿透过期闩锁下的终态保持")
            },
            Gate {
                prepare: |entry| {
                    entry.callback_until_ms = Some(super::super::now_ms().saturating_sub(1));
                    entry.snapshot.status = ObservationStatus::Ready;
                    entry.snapshot.message = None;
                    entry.terminal = Some(TerminalHold {
                        streak: 7,
                        until: None,
                        fingerprint: None,
                    });
                },
                enqueued: false,
                status: Some(ObservationStatus::Ready),
                message: None,
                ..gate("闩锁过期且自动重试已用尽：不回落探测")
            },
            Gate {
                queue_full: true,
                enqueued: false,
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
                ..gate("requested_at 刚刷新过时不派发")
            },
            Gate {
                manual: true,
                prepare: |entry| entry.requested_at = Some(Instant::now()),
                enqueued: false,
                ..gate("显式刷新在防抖窗口内不派发")
            },
            Gate {
                manual: true,
                prepare: |entry| {
                    entry.requested_at = Some(Instant::now() - Duration::from_secs(40));
                },
                ..gate("显式刷新过了防抖即派发（不等 300 s）")
            },
            Gate {
                prepare: |entry| {
                    entry.requested_at = Some(Instant::now() - Duration::from_secs(40));
                },
                enqueued: false,
                ..gate("对照：自动轮询 40 s 内不派发")
            },
            Gate {
                prepare: |entry| {
                    entry.requested_at = Some(Instant::now() - Duration::from_secs(40));
                    entry.failures = 1;
                },
                manual: true,
                ..gate("显式刷新无视失败退避")
            },
            Gate {
                prepare: |entry| {
                    hold(
                        entry,
                        ObservationStatus::NotAuthenticated,
                        Some(Instant::now() + Duration::from_secs(600)),
                    );
                },
                enqueued: false,
                status: Some(ObservationStatus::NotAuthenticated),
                ..gate("NotAuthenticated 终态 TTL 内不入队")
            },
            Gate {
                prepare: |entry| {
                    hold(
                        entry,
                        ObservationStatus::PermissionDenied,
                        Some(Instant::now() + Duration::from_secs(600)),
                    );
                },
                enqueued: false,
                status: Some(ObservationStatus::PermissionDenied),
                ..gate("PermissionDenied 终态 TTL 内不入队")
            },
            Gate {
                prepare: |entry| {
                    hold(
                        entry,
                        ObservationStatus::Unsupported,
                        Some(Instant::now() + Duration::from_secs(600)),
                    );
                },
                enqueued: false,
                status: Some(ObservationStatus::Unsupported),
                ..gate("Unsupported 终态 TTL 内不入队")
            },
            Gate {
                prepare: |entry| {
                    hold(
                        entry,
                        ObservationStatus::Unsupported,
                        Some(Instant::now() - Duration::from_secs(1)),
                    );
                },
                status: Some(ObservationStatus::Unsupported),
                ..gate("终态 TTL 到期后自动重试")
            },
            Gate {
                prepare: |entry| hold(entry, ObservationStatus::NotAuthenticated, None),
                enqueued: false,
                status: Some(ObservationStatus::NotAuthenticated),
                ..gate("自动重试用尽后不再入队")
            },
            Gate {
                manual: true,
                prepare: |entry| hold(entry, ObservationStatus::NotAuthenticated, None),
                status: Some(ObservationStatus::NotAuthenticated),
                ..gate("对照：手动刷新穿透终态")
            },
            Gate {
                prepare: |entry| entry.snapshot.status = ObservationStatus::NotAuthenticated,
                status: Some(ObservationStatus::NotAuthenticated),
                ..gate("终态但保持已被自愈撤销：允许探测")
            },
            Gate {
                prepare: |entry| {
                    entry.snapshot.status = ObservationStatus::Ready;
                    entry.snapshot.message = None;
                    entry.snapshot.metrics = vec![UsageMetric {
                        used: Some(1.0),
                        ..Default::default()
                    }];
                },
                status: Some(ObservationStatus::Ready),
                message: None,
                ..gate("已有样本时派发不改 status/message")
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
                assert_eq!(entry.attempted_at_ms, 0, "{}: 队列满不记尝试", case.name);
            }
            let task = input.try_recv().ok();
            assert_eq!(task.is_some(), case.enqueued, "{}: 入队", case.name);
            if case.enqueued {
                let task = task.unwrap_or_else(|| panic!("{}: 缺少任务", case.name));
                assert_eq!(task.generation, 5, "{}: generation", case.name);
                assert_eq!(task.account.id, account.id, "{}: 账号", case.name);
                assert_eq!(next_query, 6, "{}: next_query", case.name);
                assert!(entry.in_flight, "{}: in_flight", case.name);
                assert!(entry.queued, "{}: queued", case.name);
                assert!(entry.requested_at.is_some(), "{}: requested_at", case.name);
                assert!(entry.attempted_at_ms > 0, "{}: attempted_at_ms", case.name);
                assert_eq!(entry.generation, 5, "{}: 缓存 generation", case.name);
            } else {
                assert_eq!(next_query, 5, "{}: next_query 不变", case.name);
                assert_eq!(entry.generation, 0, "{}: 未入队不改 generation", case.name);
                assert!(!entry.queued, "{}: 未入队不置 queued", case.name);
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
            // 指向该账号的显式刷新无论是否被闸门拦下都被登记；未绑定 pane 不对应账号，无处登记。
            assert_eq!(
                entry.manual_requested_at_ms.is_some(),
                case.registered.unwrap_or(case.manual),
                "{}: 显式刷新登记",
                case.name
            );
        }
    }

    #[test]
    fn accepted_reports_are_committed_to_saved_state_and_pushed_to_subscribers() {
        let account = claude_account();
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
        assert!(accepted.updated);

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
            updated: true,
        };
        assert!(!commit_accepted(
            &missing,
            &cache,
            &bindings,
            &mut saved,
            &mut subscribers
        ));
    }

    #[test]
    fn worker_start_clears_queued_and_completion_persists_identity() {
        let account = claude_account();
        let mut entry = fresh_entry(&account);
        entry.in_flight = true;
        entry.queued = true;
        entry.generation = 3;
        let mut state = test_state(AccountUsageConfig::default(), vec![account.clone()]);
        state.cache.insert(account.id.clone(), entry);
        state.next_query = 4;
        let mut subscribers = Subscribers::new();
        state.complete(Outcome::Started(2), &mut subscribers);
        assert!(
            state.cache[&account.id].queued,
            "旧 generation 的开始通知被忽略"
        );
        state.complete(Outcome::Started(3), &mut subscribers);
        assert!(!state.cache[&account.id].queued);
        assert!(state.cache[&account.id].in_flight);
        // 旧 generation 的完成同样被忽略。
        state.complete(
            Outcome::Completed(2, Box::new(ready_snapshot(1.0, 1))),
            &mut subscribers,
        );
        assert!(state.cache[&account.id].in_flight);
        state.complete(
            Outcome::Completed(
                3,
                Box::new(AccountUsageSnapshot {
                    account_identity: Some("me@example.test".into()),
                    ..ready_snapshot(2.0, 2)
                }),
            ),
            &mut subscribers,
        );
        let entry = &state.cache[&account.id];
        assert!(!entry.in_flight);
        assert_eq!(entry.snapshot.metrics[0].used, Some(2.0));
        assert_eq!(
            state
                .saved
                .identities
                .get("claude:default")
                .map(String::as_str),
            Some("me@example.test")
        );
    }
}
