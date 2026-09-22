//! 监控与账号界面的客户端状态；网络和设备访问均通过公开 API。

mod render;
use super::*;
use crate::api::schema::*;
use crate::config::{AccountUsageConfig, MonitorConfig, UsageDisplayFormat, UsageDisplayPosition};
use crossterm::event::{KeyCode, MouseButton, MouseEvent, MouseEventKind};
use std::time::{Duration, Instant};

/// 订阅生效期间的低频兜底轮询：事件通道是合并丢帧语义，偶发丢帧靠它补齐。
const SUBSCRIBED_POLL: Duration = Duration::from_secs(30);
/// 订阅被拒 / 不可用后的退避：到期前只走自适应轮询。
const SUBSCRIBE_RETRY: Duration = Duration::from_secs(30);
/// 退订请求失败（超时 / 传输错误）后的重试间隔；重试次数有上界。
const UNSUBSCRIBE_RETRY: Duration = Duration::from_secs(2);
/// 同一订阅的退订最多尝试次数：超过即放弃（服务端会在连接断开时释放）。
const MAX_UNSUBSCRIBE_ATTEMPTS: u8 = 3;
/// 服务端探测在途时的轮询间隔（旧 server 不带刷新状态时保持 2 秒）。
const IN_FLIGHT_POLL: Duration = Duration::from_millis(500);
const IDLE_POLL: Duration = Duration::from_secs(2);
/// 「N 秒后可刷新」的可信上限：端点时钟与本机偏差超过它时当作未知，不锁按钮。
const MAX_REFRESH_WAIT_SECS: u64 = 60;
/// 总览逐厂商扇出的厂商数上限：超过即回落一次整体请求（响应侧仍按本机关闭
/// 过滤），逐厂商请求数不随注册表规模线性增长（乘法性能路径：频率 × 基数）。
const MAX_USAGE_FAN_OUT: usize = 8;
/// 总览等待厂商列表的上限：列表请求发出超过它仍未到达（响应丢失 / 连接未断）
/// 即回落整体请求，用量轮询不因列表停摆。
const PROVIDERS_WAIT: Duration = Duration::from_secs(1);
/// 厂商列表请求失败后的重试间隔（正常节流为 5 分钟），失败不每 tick 重发。
const PROVIDERS_RETRY: Duration = Duration::from_secs(10);

pub(super) fn zh() -> bool {
    crate::i18n::lang().as_str().starts_with("zh")
}

pub(super) fn tr(en: &'static str, zh_text: &'static str) -> &'static str {
    if zh() {
        zh_text
    } else {
        en
    }
}

/// 可作为绑定候选 / 悬浮目标的 agent：排除 herdr 自身的 agent（muse）。悬浮层
/// 判据与账号页 pane 选择器共用同一处，避免两个入口漂移。
pub(super) fn is_bindable_agent(name: &str) -> bool {
    !name.eq_ignore_ascii_case("muse")
}

/// Whether the accounts surface lists a provider: hidden only when the
/// server explicitly reports the CLI missing and no account is explicitly
/// configured for it. `None` (older server) counts as unknown and stays
/// listed per the generation-1 endpoint contract.
pub(super) fn provider_listed(provider: &UsageProviderInfo) -> bool {
    provider.installed != Some(false) || !provider.configured_accounts.is_empty()
}

/// 监控面板内的页面（tab）；可持久化为客户端偏好，因此 wire 名固定为 snake_case。
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Page {
    Monitor,
    Accounts,
    Settings,
}

/// 系统页迷你图 / 条形的字形档位，持久化为客户端偏好（wire 名固定 snake_case）。
/// 默认盲文；宿主字体缺盲文时切方块，连方块都缺时切 ASCII。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ChartGlyphsPreference {
    #[default]
    Braille,
    Blocks,
    Ascii,
}

impl ChartGlyphsPreference {
    /// kit 迷你图用的字形集。
    pub(super) fn chart(self) -> crate::ui::kit::braille_chart::ChartGlyphs {
        use crate::ui::kit::braille_chart::ChartGlyphs;
        match self {
            Self::Braille => ChartGlyphs::Braille,
            Self::Blocks => ChartGlyphs::Blocks,
            Self::Ascii => ChartGlyphs::Ascii,
        }
    }

    /// 条形 / 开关 / 表格排序标记是否降级为 ASCII：只有第三档。
    pub(super) fn ascii(self) -> bool {
        matches!(self, Self::Ascii)
    }
}

#[derive(Clone, Debug)]
pub(super) enum Purpose {
    Metrics,
    Providers,
    /// 页面作用域的用量请求；`agent` 是请求带的厂商：总览逐厂商请求时为
    /// `Some(x)`（响应只替换该厂商的账号），`None` 是整体请求（响应整体替换）。
    Usage {
        agent: Option<String>,
    },
    /// 悬浮层自己的用量请求：带 hover 的 pane，落到 `hover_scope`，永不写页面；
    /// `agent` 语义同 `Usage`。
    HoverUsage {
        agent: Option<String>,
    },
    Binding,
    /// 悬浮层发起的绑定：发往 hover 的端点，回流只刷新悬浮层。
    HoverBinding,
    /// 官方回调开关；`bind_after` 是启用成功后顺手绑定的 (pane, account)。
    Integration {
        bind_after: Option<(String, String)>,
    },
    /// 悬浮层发起的官方回调开关：发往 hover 的端点。
    HoverIntegration {
        bind_after: Option<(String, String)>,
    },
    /// 页面作用域的 `account.usage.subscribe` / `unsubscribe`：不绑定页面代际
    /// （订阅按参数与 (端点, boot) 归属，由 tick 对账），只受 boot 校验。
    Subscribe,
    Unsubscribe,
    Process,
    Terminate,
}

impl Purpose {
    /// 在途去重键：同键请求在途时不重复发。逐厂商的用量请求带厂商后缀
    /// （`usage:<agent>` / `hover_usage:<agent>`），各厂商互不阻塞。
    fn key(&self) -> std::borrow::Cow<'static, str> {
        use std::borrow::Cow;
        match self {
            Self::Metrics => Cow::Borrowed("metrics"),
            Self::Providers => Cow::Borrowed("providers"),
            Self::Usage { agent: None } => Cow::Borrowed(Self::USAGE_KEY),
            Self::Usage { agent: Some(agent) } => {
                Cow::Owned(format!("{}:{agent}", Self::USAGE_KEY))
            }
            Self::HoverUsage { agent: None } => Cow::Borrowed(Self::HOVER_USAGE_KEY),
            Self::HoverUsage { agent: Some(agent) } => {
                Cow::Owned(format!("{}:{agent}", Self::HOVER_USAGE_KEY))
            }
            Self::Binding => Cow::Borrowed("binding"),
            Self::HoverBinding => Cow::Borrowed("hover_binding"),
            Self::Integration { .. } => Cow::Borrowed("integration"),
            Self::HoverIntegration { .. } => Cow::Borrowed("hover_integration"),
            Self::Subscribe => Cow::Borrowed("subscribe"),
            Self::Unsubscribe => Cow::Borrowed("unsubscribe"),
            Self::Process => Cow::Borrowed("process"),
            Self::Terminate => Cow::Borrowed("terminate"),
        }
    }

    const USAGE_KEY: &'static str = "usage";
    const HOVER_USAGE_KEY: &'static str = "hover_usage";

    /// 某作用域用量请求的键前缀：页面 `usage`、悬浮层 `hover_usage`。
    fn usage_key_prefix(hover: bool) -> &'static str {
        if hover {
            Self::HOVER_USAGE_KEY
        } else {
            Self::USAGE_KEY
        }
    }

    /// `key` 是否是某作用域的用量请求键（整体请求或任一厂商的逐厂商请求）。
    fn is_usage_key(key: &str, hover: bool) -> bool {
        let prefix = Self::usage_key_prefix(hover);
        key == prefix
            || key
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.starts_with(':'))
    }

    /// 悬浮层作用域的请求：目标端点与代际都取自 `hover_scope`。
    fn is_hover(&self) -> bool {
        matches!(
            self,
            Self::HoverUsage { .. } | Self::HoverBinding | Self::HoverIntegration { .. }
        )
    }

    /// 随页面代际作废的请求：既不是悬浮层作用域，也不是订阅生命周期。
    fn page_scoped(&self) -> bool {
        !self.is_hover() && !matches!(self, Self::Subscribe | Self::Unsubscribe)
    }

    /// 键的主体：逐厂商后缀（`:<agent>`）之前的部分。
    fn key_stem(key: &str) -> &str {
        key.split_once(':').map_or(key, |(stem, _)| stem)
    }

    /// 悬浮层作用域的请求键（含逐厂商的 `hover_usage:<agent>`）。按闭集显式匹配，
    /// 与 `is_hover` 的变体集合一一对应（`key_scope_flags_match_the_variants` 守门），
    /// 不按前缀猜：误判会让在途请求不被作废、响应落进错误作用域。
    fn is_hover_key(key: &str) -> bool {
        matches!(
            Self::key_stem(key),
            Self::HOVER_USAGE_KEY | "hover_binding" | "hover_integration"
        )
    }

    /// 不随页面代际作废的请求键：悬浮层作用域 + 订阅生命周期（与 `page_scoped`
    /// 的补集一一对应）。
    fn is_page_independent_key(key: &str) -> bool {
        Self::is_hover_key(key) || matches!(Self::key_stem(key), "subscribe" | "unsubscribe")
    }
}

#[cfg(test)]
mod key_tests {
    use super::Purpose;

    /// 每个变体的样本：新增变体时这里的穷举 match 会编译失败，提醒同步键判定。
    fn samples() -> Vec<Purpose> {
        let variants = [
            Purpose::Metrics,
            Purpose::Providers,
            Purpose::Usage { agent: None },
            Purpose::Usage {
                agent: Some("claude".into()),
            },
            Purpose::HoverUsage { agent: None },
            Purpose::HoverUsage {
                agent: Some("codex".into()),
            },
            Purpose::Binding,
            Purpose::HoverBinding,
            Purpose::Integration { bind_after: None },
            Purpose::HoverIntegration { bind_after: None },
            Purpose::Subscribe,
            Purpose::Unsubscribe,
            Purpose::Process,
            Purpose::Terminate,
        ];
        for variant in &variants {
            match variant {
                Purpose::Metrics
                | Purpose::Providers
                | Purpose::Usage { .. }
                | Purpose::HoverUsage { .. }
                | Purpose::Binding
                | Purpose::HoverBinding
                | Purpose::Integration { .. }
                | Purpose::HoverIntegration { .. }
                | Purpose::Subscribe
                | Purpose::Unsubscribe
                | Purpose::Process
                | Purpose::Terminate => {}
            }
        }
        variants.into()
    }

    /// 字符串键的作用域判定必须与枚举侧的 `is_hover` / `page_scoped` 一致。
    #[test]
    fn key_scope_flags_match_the_variants() {
        for purpose in samples() {
            let key = purpose.key();
            assert_eq!(
                Purpose::is_hover_key(&key),
                purpose.is_hover(),
                "{key}: 悬浮层键判定与变体不一致"
            );
            assert_eq!(
                Purpose::is_page_independent_key(&key),
                !purpose.page_scoped(),
                "{key}: 页面代际判定与变体不一致"
            );
            let hover = purpose.is_hover();
            assert_eq!(
                Purpose::is_usage_key(&key, hover),
                matches!(purpose, Purpose::Usage { .. } | Purpose::HoverUsage { .. }),
                "{key}: 用量键判定与变体不一致"
            );
        }
        assert!(
            !Purpose::is_hover_key("hover_something_new"),
            "未知前缀不算悬浮层"
        );
        assert!(!Purpose::is_usage_key("usage_extra", false));
        assert!(Purpose::is_usage_key("usage:claude", false));
        assert!(!Purpose::is_usage_key("usage:claude", true));
    }
}

/// `observation_request` 的结局：调用方据此决定强意图刷新标志的去留。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RequestOutcome {
    /// 请求已发出。
    Sent,
    /// 同类请求在途：保留标志稍后重试。
    Busy,
    /// 端点离线 / 未宣告该方法 / 无快照：该动作当前不可用，标志应清除。
    Unavailable,
}

/// `scope_usage_requests` 一轮扇出的结局：三个标志互不排斥（一部分厂商发出、
/// 另一部分被在途请求挡下是常态），调用方据此决定强意图刷新的去留。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct FanOut {
    /// 至少一个厂商的请求已发出。
    sent: bool,
    /// 至少一个厂商被同键在途请求挡下：强意图须保留并稍后只补发它们。
    busy: bool,
    /// 端点级不可用（离线 / 未宣告方法 / 无快照），对所有厂商一样。
    unavailable: bool,
}

/// `usage_targets` 的结果：某作用域这一轮向哪些厂商发用量请求。
#[derive(Clone, Debug, PartialEq, Eq)]
enum UsageTargets {
    /// 逐个发请求的厂商；`None` 是整体请求（服务端按它自己的 TOML 过滤）。
    Agents(Vec<Option<String>>),
    /// 选中的厂商（或总览里所有已列出的厂商）已在本机设置关闭：不发请求。
    Disabled,
    /// 总览态但厂商列表里没有任何已列出的厂商（此主机既没装受支持的 agent CLI
    /// 也没配置账号）：不发请求；与 `Disabled` 分开，文案不能把用户指去设置页。
    NoProviders,
    /// 总览态且厂商列表刚发出、仍在途：等列表到达再逐厂商发（到达即唤醒）。
    AwaitProviders,
}

/// 设置页动作触到的偏好键：只回写用户改动的那一个键，其余键保持 None（继续跟随
/// config.toml）。否则勾掉一个厂商会把四个 usage_* 键一次性固化成影子值，之后
/// config.toml 的修改就再也照不进来（F05/F06）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PreferenceKey {
    Monitor,
    UsageEnabled,
    UsageFormat,
    UsagePosition,
    DisabledProviders,
    HoverDelay,
    /// 系统页迷你图 / 条形的字形档位（`monitor_chart_glyphs`）。
    ChartGlyphs,
    /// 「恢复配置文件值」：不写任何键，只把已清空的偏好落盘。
    RestoreUsage,
}

impl PreferenceKey {
    fn for_action(action: &Action) -> Option<Self> {
        Some(match action {
            Action::Interval(_)
            | Action::Metric(_)
            | Action::CardMove(..)
            | Action::CardSize(_)
            | Action::HistoryRange(_)
            | Action::Device(_)
            | Action::ToggleAlerts
            | Action::AlertThreshold(..)
            | Action::AlertDuration(..)
            | Action::AlertCooldown(..) => Self::Monitor,
            Action::UsageEnabled => Self::UsageEnabled,
            Action::UsageFormat(_) => Self::UsageFormat,
            Action::UsagePosition(_) => Self::UsagePosition,
            Action::ProviderEnabled(_) => Self::DisabledProviders,
            Action::HoverDelay(_) => Self::HoverDelay,
            Action::ChartGlyphs(_) => Self::ChartGlyphs,
            Action::RestoreUsagePreferences => Self::RestoreUsage,
            _ => return None,
        })
    }
}

/// 在固定档位表里按方向取相邻档：`delta > 0` 取「大于当前值的最小档」，否则取
/// 「小于当前值的最大档」，越过末档回绕。配置文件里的非档位值（如 300 ms）
/// 第一次点击落到相邻档，不跳档。
fn step_ladder<T: Copy + PartialOrd>(ladder: &[T], current: T, delta: i8) -> T {
    let next = if delta < 0 {
        ladder
            .iter()
            .rev()
            .copied()
            .find(|step| *step < current)
            .or_else(|| ladder.last().copied())
    } else {
        ladder
            .iter()
            .copied()
            .find(|step| *step > current)
            .or_else(|| ladder.first().copied())
    };
    next.unwrap_or(current)
}

/// 设置页步进器的档位表。
const INTERVAL_STEPS: [u64; 4] = [500, 1000, 2000, 5000];
const CARD_HEIGHT_STEPS: [u16; 4] = [7, 10, 16, 24];
const HISTORY_STEPS: [u16; 5] = [1, 5, 15, 30, 60];
const ALERT_DURATION_STEPS: [u64; 4] = [10, 30, 60, 300];
const ALERT_COOLDOWN_STEPS: [u64; 3] = [60, 300, 900];

#[derive(Clone, Debug)]
pub(super) enum Action {
    Page(Page),
    Close,
    Pause,
    /// 系统页「编辑布局」模式开关：卡片的 ↑↓ 按钮只在该模式下出现，↑↓ 键移动
    /// 选中的卡片。
    EditLayout,
    Configure,
    Refresh,
    ToggleAlerts,
    /// 采样间隔步进（±1 档），持久化为客户端偏好；下同。
    Interval(i8),
    Metric(String),
    UsageEnabled,
    /// 用量样式：设置页的分段控件直接设值，账号页工具栏传「另一个」。
    UsageFormat(UsageDisplayFormat),
    UsagePosition(UsageDisplayPosition),
    /// 悬浮延时步进（200 / 400 / 800 / 1200 / 2000 ms 档位），持久化为客户端偏好。
    HoverDelay(i8),
    /// 系统页迷你图 / 条形的字形档位。
    ChartGlyphs(ChartGlyphsPreference),
    /// 设置页「恢复配置文件值」：清掉 usage_* 的本机覆盖，重新跟随 config.toml。
    RestoreUsagePreferences,
    /// 回到跨厂商总览（账号页首个 chip / 再点已选厂商 chip）。
    Overview,
    Provider(String),
    Account(String),
    CycleAccount,
    Bind,
    /// 把当前聚焦的 pane（须运行所选厂商的 agent）绑定到作用域内的账号。
    BindFocused,
    /// 账号页 pane 选择器：在运行所选厂商 agent 的 pane 之间前后切换。
    CyclePane(isize),
    /// 一键绑定：服务端 `pending_binding` 给出的 (pane, account)。
    BindTo(String, String),
    Source,
    UsageIntegration(bool),
    Process(ProcessIdentity),
    CancelProcess,
    Terminate(bool),
    ConfirmProcess,
    /// 进程表按列排序（表头点击）。
    SortProcesses(ProcessSort),
    FilterProcesses,
    Card(String),
    Core(usize),
    CardMove(String, isize),
    CardSize(i8),
    HistoryRange(i8),
    ProviderEnabled(String),
    Device(String),
    /// 第 n 条告警规则的阈值 / 持续 / 冷却步进（±1 档）。
    AlertThreshold(usize, i8),
    AlertDuration(usize, i8),
    AlertCooldown(usize, i8),
}

/// 每个账号保留的用量历史样本数（右栏 sparkline 的窗口）。
pub(super) const USAGE_HISTORY_SAMPLES: usize = 120;

/// 每个网络接口保留的速率样本数（网络卡堆叠迷你图的窗口；按默认 1 s 采样约
/// 两分钟），也是渲染期栈上拷贝缓冲的上限。
pub(super) const NET_HISTORY_SAMPLES: usize = 128;

/// 一次账号用量采样：`percent` 取该账号各窗口里已用比例最高的一档（「最紧的
/// 那一档」是用户扫一眼 sparkline 想看的压力信号）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct UsageSample {
    pub at_ms: u64,
    pub percent: f32,
}

/// 账号的压力百分比：各窗口 `used/limit`（或 `used_percent`）里最高的一档；
/// 没有额度字段（如只报请求数）的账号不产生样本。
fn usage_pressure_percent(account: &AccountUsageSnapshot) -> Option<f32> {
    account
        .metrics
        .iter()
        .filter_map(super::observability::render::metric_percent)
        .reduce(f32::max)
}

/// 追加一条样本：`observed_at_ms` 未前进（同一份快照被重复投递）时跳过，
/// 超出上限丢最旧的。
fn push_usage_sample(samples: &mut VecDeque<UsageSample>, at_ms: u64, percent: f32) {
    if samples.back().is_some_and(|sample| sample.at_ms >= at_ms) {
        return;
    }
    if samples.len() >= USAGE_HISTORY_SAMPLES {
        samples.pop_front();
    }
    samples.push_back(UsageSample { at_ms, percent });
}

fn record_usage_history(
    history: &mut HashMap<String, VecDeque<UsageSample>>,
    accounts: &[AccountUsageSnapshot],
) {
    for account in accounts {
        let Some(percent) = usage_pressure_percent(account) else {
            continue;
        };
        push_usage_sample(
            history.entry(account.account_id.clone()).or_default(),
            account.observed_at_ms,
            percent,
        );
    }
}

#[derive(Clone)]
pub(super) struct HistoryPoint {
    pub at: u64,
    pub cpu: Option<f32>,
    pub memory: Option<f32>,
    pub cores: Vec<Option<f32>>,
}

/// 悬浮层的目标：Agents 面板里某个 pane 的 agent（按 pane 查该厂商用量）。
#[derive(Clone, Debug, PartialEq)]
pub(super) enum HoverTarget {
    Agent {
        endpoint_id: ClientEndpointId,
        pane: String,
        agent: String,
    },
}

pub(super) struct Hover {
    pub target: HoverTarget,
    pub anchor: Rect,
    pub since: Instant,
    pub visible: bool,
    pub leave_at: Option<Instant>,
    /// 钉住的浮层：指针离开不再关闭，Esc / 浮层外点击才关闭。目前没有入口会
    /// 置位，留给「键盘钉住单 agent 用量卡」。
    pub pinned: bool,
}

/// 悬浮层独立的数据作用域：厂商 / pane / 端点 / 账号快照 / epoch / 刷新标志 /
/// 滚动位置 / 轮询时刻，与账号页的 `selected_*` / `accounts` / `epoch` /
/// `account_scroll` / `next_usage` 互不污染。页面轮询永不带这里的 pane；悬浮层
/// 出现、消失都不改写页面选择，也不提前唤醒页面轮询。
pub(super) struct HoverScope {
    pub provider: Option<String>,
    pub pane: Option<String>,
    pub endpoint: Option<ClientEndpointId>,
    pub accounts: Vec<AccountUsageSnapshot>,
    /// 与 `accounts` 按 `account_id` 对齐的服务端刷新状态（旧 server 为空）。
    pub refresh_states: Vec<UsageRefreshState>,
    /// 悬浮层请求的代际：作用域复位即推进，旧响应按此丢弃。
    pub epoch: u64,
    /// 排队中的强意图刷新（悬浮层首次可见 / 点「刷新」/ 绑定回流）；请求真正
    /// 发出后才清除，被在途请求挡下时保留并 200 ms 重试。
    pub refresh: bool,
    /// 悬浮层的 `account.usage.refresh` 在途；响应到达即清除。
    manual_in_flight: bool,
    /// 本轮强意图刷新已发出 `refresh` 的厂商（语义同 `State::manual_sent`）。
    manual_sent: Vec<Option<String>>,
    /// 悬浮层自己的账号列表滚动位置。
    pub scroll: usize,
    /// 悬浮层下一次轮询时刻，与页面的 `next_usage` 各自独立。
    next_usage: Instant,
    /// 悬浮层最近一次用量请求的发出时刻：响应说探测在途时以它为基准收紧轮询。
    sent_at: Option<Instant>,
}

impl Default for HoverScope {
    fn default() -> Self {
        Self {
            provider: None,
            pane: None,
            endpoint: None,
            accounts: Vec::new(),
            refresh_states: Vec::new(),
            epoch: 0,
            refresh: false,
            manual_in_flight: false,
            manual_sent: Vec::new(),
            scroll: 0,
            next_usage: Instant::now(),
            sent_at: None,
        }
    }
}

/// 「悬浮延时」设置行的档位（毫秒），`Action::HoverDelay` 按表循环。
const HOVER_DELAY_STEPS: [u64; 5] = [200, 400, 800, 1200, 2000];

/// 服务端确认的页面订阅：id、参数与创建它的 (端点, boot)。退订必须发回同一
/// 端点——订阅 id 只在创建它的 server 上有意义。
pub(super) struct ActiveSubscription {
    pub id: String,
    pub params: UsageParams,
    pub endpoint: ClientEndpointId,
    pub boot_id: String,
}

/// 待退订的旧订阅：定向发往创建它的端点；失败有界重试，端点离线 / boot 变化
/// 时服务端已随连接释放，直接放弃。
struct RetiringSubscription {
    id: String,
    endpoint: ClientEndpointId,
    boot_id: String,
    attempts: u8,
}

/// 页面作用域订阅（`account.usage.subscribe`）的生命周期；悬浮层不订阅。
/// 订阅归属于创建它的 (端点, boot) 与连接：活动端点 / boot 变化时旧订阅进入
/// 退订队列（发回旧端点），连接断开时服务端已释放、直接丢弃。
#[derive(Default)]
pub(super) struct UsageSubscription {
    /// 服务端确认的订阅；`None` = 未订阅。
    pub active: Option<ActiveSubscription>,
    /// 已发出、等待确认的订阅：参数与目标端点（boot 由响应路径按端点校验）。
    requested: Option<(UsageParams, ClientEndpointId)>,
    /// 待退订的旧订阅（作用域变化 / 页面隐藏 / 端点切换后），一次退一个，
    /// 收到 `active:false` 确认才出队。
    retire: VecDeque<RetiringSubscription>,
    /// 订阅被拒 / 端点不可用后的退避截止：到期前只走自适应轮询。
    retry_at: Option<Instant>,
    /// 退订失败后的重试时刻。
    unsubscribe_retry_at: Option<Instant>,
}

impl UsageSubscription {
    /// 参数是否覆盖页面当前的 (厂商, 账号) 选择。
    fn covers(params: &UsageParams, provider: Option<&str>, account: Option<&str>) -> bool {
        params.agent.as_deref() == provider && params.account_id.as_deref() == account
    }

    /// 把已确认的订阅移入退订队列（保留其归属端点）。
    fn retire_active(&mut self) {
        if let Some(active) = self.active.take() {
            self.retire.push_back(RetiringSubscription {
                id: active.id,
                endpoint: active.endpoint,
                boot_id: active.boot_id,
                attempts: 0,
            });
        }
    }

    /// 页面作用域换了 (端点, boot)：已确认的订阅排队退订（发回旧端点），等待
    /// 确认的订阅在响应到达时再排队退订；退避复位。退订队列原样保留。
    fn detach(&mut self) {
        self.retire_active();
        self.requested = None;
        self.retry_at = None;
    }

    /// 某端点的连接断开：服务端已随连接释放该端点上的订阅，本地相应条目直接
    /// 丢弃（不再退订）。返回是否丢掉了在途的订阅 / 退订请求。
    fn endpoint_disconnected(&mut self, endpoint: &ClientEndpointId) -> (bool, bool) {
        let subscribe_in_flight = self
            .requested
            .as_ref()
            .is_some_and(|(_, owner)| owner == endpoint);
        let unsubscribe_in_flight = self
            .retire
            .front()
            .is_some_and(|entry| &entry.endpoint == endpoint);
        if self
            .active
            .as_ref()
            .is_some_and(|active| &active.endpoint == endpoint)
        {
            self.active = None;
        }
        if subscribe_in_flight {
            self.requested = None;
        }
        self.retire.retain(|entry| &entry.endpoint != endpoint);
        if unsubscribe_in_flight {
            self.unsubscribe_retry_at = None;
        }
        (subscribe_in_flight, unsubscribe_in_flight)
    }
}

/// 排队待发的绑定（官方回调启用成功后顺手绑定）：响应处理期无法发请求，由
/// 下一次 tick 发出。`endpoint` 是应答回调的主机——pane id 只在它上面有意义，
/// 派发时目标主机已变则放弃。
struct QueuedBinding {
    pane_id: String,
    account_id: String,
    hover: bool,
    endpoint: ClientEndpointId,
}

impl HoverScope {
    /// 悬浮层处于「刷新中」：强意图刷新已排队或正在途中（语义与页面的
    /// `State::refreshing` 对齐）。
    pub(super) fn refreshing(&self) -> bool {
        self.refresh || self.manual_in_flight
    }

    /// 悬浮层的强意图刷新：下一次 tick 立即发 `account.usage.refresh`。
    pub(super) fn request_refresh(&mut self) {
        self.refresh = true;
        self.next_usage = Instant::now();
    }
}

pub(super) struct ProcessDialog {
    pub process: ProcessMetric,
    pub force: bool,
    pub confirm: bool,
    pub pending: bool,
}

#[derive(Default)]
struct AlertState {
    since: Option<u64>,
    last_fired: Option<u64>,
    armed: bool,
}

/// `State::paint` 一次绘制的结果；hits/rect 由调用方通过 `commit_paint` 写回。
pub(super) struct Painted {
    /// 供 kitty 图片 occlusion 使用的覆盖区：进程对话框存在时只有对话框，否则
    /// 页面与悬浮层各占一块（空块由 `Occlusion::cover` 忽略）。两块分开给出而
    /// 不合成外接矩形：经典布局下页面铺在 pane 区、悬浮层锚在侧栏 agent 行旁，
    /// 外接矩形会把中间没被盖住的区域也算成覆盖。
    pub covered: [Rect; 2],
    /// 页面（或进程对话框）的命中区。
    pub hits: Vec<(Rect, Action)>,
    /// 悬浮层自己的命中区，与页面命中区分开记录（浮层独占内部输入）。
    pub hover_hits: Vec<(Rect, Action)>,
    pub page_rect: Rect,
    pub hover_rect: Rect,
    pub dialog: bool,
}

pub(super) struct State {
    /// 当前拥有键盘输入的页面：经典布局下即打开的页面；停靠工作台下仅当
    /// 监控 / 账号面板聚焦时为 `Some`，与 `dock.focused` 同步（不在渲染期改写）。
    pub page: Option<Page>,
    /// 用户在监控面板里选中的 tab；焦点回到终端后面板仍画这个 tab，并持久化。
    pub monitor_tab: Page,
    pub monitor: MonitorConfig,
    pub usage: AccountUsageConfig,
    pub metrics: Option<Box<SystemMetricsSnapshot>>,
    pub history: VecDeque<HistoryPoint>,
    /// 账号用量历史样本（按账号 id）：右栏 sparkline 的数据源。只在快照的
    /// `observed_at_ms` 前进时追加（轮询响应与订阅事件都走这里），每个账号保留
    /// 最近 `USAGE_HISTORY_SAMPLES` 个点。
    pub usage_history: HashMap<String, VecDeque<UsageSample>>,
    pub providers: Vec<UsageProviderInfo>,
    /// 账号页的账号快照（页面作用域）。
    pub accounts: Vec<AccountUsageSnapshot>,
    pub selected_provider: Option<String>,
    pub selected_account: Option<String>,
    /// 页面作用域里待绑定的 pane：由 pane 选择器（`Action::CyclePane`）、「绑定到
    /// 聚焦 pane」（`Action::BindFocused`）与一键绑定（`Action::BindTo`）写入，只供
    /// `Action::Bind` 使用，页面轮询不带它；悬浮层从不写它。
    pub selected_pane: Option<String>,
    /// `selected_pane` 在绑定行里的显示名（agent 名 · pane id）。
    pub selected_pane_label: Option<String>,
    /// 与页面 `accounts` 按 `account_id` 对齐的服务端刷新状态（旧 server 为空）：
    /// 探测在途 / 防抖截止 / 目录信任 / 推断绑定 / 待办绑定。
    pub refresh_states: Vec<UsageRefreshState>,
    /// 页面作用域的用量订阅，见 `UsageSubscription`。
    pub subscription: UsageSubscription,
    pub hover: Option<Hover>,
    /// 悬浮层作用域，见 `HoverScope`。
    pub hover_scope: HoverScope,
    /// usage_* 偏好键里是否有本机覆盖（`ClientChromePreferences::usage_overridden`
    /// 的镜像，供设置页「恢复配置文件值」决定是否可点）；随偏好写入 / 重载刷新。
    pub usage_overridden: bool,
    /// 官方回调启用成功后排队的绑定，下一次 tick 发出。
    queued_binding: Option<QueuedBinding>,
    /// 订阅推送在呈现面可见时到达：下一次 tick 重绘一次（事件不逐帧 compose）。
    event_repaint: bool,
    /// 上一 tick 页面作用域（账号页）是否可见：由不可见变可见时
    /// 冷启动一次 get，不等订阅期间的低频兜底轮询。
    page_seen: bool,
    /// 页面作用域的 `account.usage.refresh` 在途；响应到达即清除。与
    /// `refresh_usage`（排队）一起构成 `refreshing()`。
    manual_in_flight: bool,
    pub hover_rect: Rect,
    pub hover_hits: Vec<(Rect, Action)>,
    pub page_rect: Rect,
    /// 本帧全部命中区：前 `page_hits` 项是页面（或进程对话框）的，其后是悬浮层
    /// 的（既有契约：浮层按钮可从总表列举）。
    pub hits: Vec<(Rect, Action)>,
    /// `hits` 里页面命中区的数量：键盘 Tab / Enter 只在 `hits[..page_hits]` 内
    /// 循环，浮层命中区只服务鼠标——否则页面与浮层同帧时 Tab 到末尾会把
    /// 高亮夹在最后一个页面控件上、Enter 却触发浮层的「打开页面」。
    pub page_hits: usize,
    pub selected_hit: usize,
    pub selected_card: Option<String>,
    pub selected_core: Option<usize>,
    /// 系统页「编辑布局」模式：卡片带 ↑↓ 按钮，↑↓ 键移动选中的卡片；离开
    /// 系统页或关闭面板即退出。
    pub layout_editing: bool,
    /// 系统页迷你图 / 条形的字形档位。
    pub chart_glyphs: ChartGlyphsPreference,
    /// 每个网络接口最近的 (收, 发) 速率样本（B/s）：网络卡堆叠迷你图与空闲
    /// 判定的数据源；随主机 boot 变化清空，接口消失即丢弃。
    pub net_history: HashMap<String, VecDeque<(f32, f32)>>,
    pub card_scroll: HashMap<String, usize>,
    pub account_scroll: usize,
    /// 系统页卡片列表的滚动位置；设置页用 `settings_scroll`，两页互不泄漏。
    pub scroll: usize,
    /// 设置页行列表的滚动位置。
    pub settings_scroll: usize,
    /// 面板边框 / 分隔线的字形表（尊重 `ui.border_style`），随配置重载刷新。
    pub glyphs: crate::ui::BorderGlyphs,
    pub paused: bool,
    pub process_dialog: Option<ProcessDialog>,
    pub process_filter: String,
    pub filtering_processes: bool,
    pub process_sort: ProcessSort,
    pub message: Option<String>,
    pub now_ms: u64,
    pub epoch: u64,
    /// 活动端点的 (端点, boot_id)；按引用比较，tick 内不分配（OBS-13）。
    source: Option<(ClientEndpointId, String)>,
    /// 用量轮询看到的 (活动端点, boot_id)；快照缺失时为 `None`，变化即作废
    /// 页面作用域（覆盖同 boot 断线重连的情形，`source` 不会变）。
    usage_source: Option<(ClientEndpointId, String)>,
    next_metrics: Instant,
    /// 页面作用域下一次轮询时刻；悬浮层用 `hover_scope.next_usage`。
    next_usage: Instant,
    /// 页面最近一次用量请求的发出时刻：响应说探测在途时以它为基准收紧轮询。
    usage_sent_at: Option<Instant>,
    /// 厂商列表的低频重拉时刻：页面打开时立即，之后每 5 分钟（OBS-14）。
    next_providers: Instant,
    /// 页面作用域排队中的强意图刷新：每个目标厂商的 `refresh` 都真正发出后才
    /// 清除，被在途请求挡下的厂商保留并在 200 ms 后补发；端点不支持 / 厂商已
    /// 关闭时清除（回落到普通 get）。
    refresh_usage: bool,
    /// 本轮强意图刷新（`refresh_usage`）已发出 `refresh` 的厂商：200 ms 补发只发
    /// 被挡下的厂商，不给已发出的厂商重复发（服务端 10 秒防抖会把重复发的当成
    /// 「N 秒后可刷新」）；强意图消费或页面换代时清空。
    manual_sent: Vec<Option<String>>,
    /// 厂商列表请求最近一次发出的时刻：总览只在列表刚发出（`PROVIDERS_WAIT`
    /// 内）时等它，超时即回落整体请求。
    providers_sent_at: Option<Instant>,
    /// 账号页打开时尚无厂商列表：列表到达后按聚焦 pane 的 agent / 首个已安装
    /// 厂商补选一次。
    auto_select_provider: bool,
    /// 上一 tick 账号页是否可见：由不可见变可见（含偏好恢复 / 面板随布局
    /// 恢复）且尚未选厂商时触发自动选中。
    accounts_page_seen: bool,
    /// 已在页脚说明过「所选主机不在线」的目标端点：回退目标不变时不重写页脚，
    /// 避免每 2 秒刷掉其它一次性提示。
    fallback_noted: Option<ClientEndpointId>,
    /// 在途观测请求的去重键（`Purpose::key`）。
    pending: HashSet<String>,
    alerts: HashMap<String, AlertState>,
}

impl State {
    /// 页面作用域处于「刷新中」：强意图刷新已排队或正在途中。渲染期据此把旧
    /// 数据变暗、空态显示「刷新中…」、「刷新」按钮退化为状态提示。
    pub(super) fn refreshing(&self) -> bool {
        self.refresh_usage || self.manual_in_flight
    }

    /// 滚动账号列表；`hover` 为真时按悬浮层作用域的账号数夹取并写悬浮层自己的
    /// 滚动位置，否则按页面。
    pub(super) fn scroll_accounts(&mut self, delta: isize, hover: bool) {
        let (accounts, refresh_states) = if hover {
            (&self.hover_scope.accounts, &self.hover_scope.refresh_states)
        } else {
            (&self.accounts, &self.refresh_states)
        };
        let rows = render::account_rows(self, accounts, refresh_states);
        let limit = rows.saturating_sub(1);
        let scroll = if hover {
            &mut self.hover_scope.scroll
        } else {
            &mut self.account_scroll
        };
        *scroll = scroll.saturating_add_signed(delta).min(limit);
    }

    /// 滚动系统页某张卡片的内容。上界按卡片**实际渲染**的条目数算：温度卡按
    /// 芯片汇总、网络卡折叠空闲接口、磁盘去重、进程按筛选词过滤，用快照里的
    /// 原始条数当上界会把卡片滚成空白。
    pub(super) fn scroll_card(&mut self, card: &str, delta: isize) {
        let limit = self
            .metrics
            .as_ref()
            .map_or(0, |sample| render::card_scroll_len(self, sample, card))
            .saturating_sub(1);
        let scroll = self.card_scroll.entry(card.to_owned()).or_default();
        *scroll = scroll.saturating_add_signed(delta).min(limit);
    }

    /// 滚动当前页面自己的列表：系统页滚卡片、设置页滚设置行（账号页走
    /// `scroll_accounts`）。两个滚动位置分离，切页不互相泄漏。
    pub(super) fn scroll_page(&mut self, delta: isize) {
        let scroll = match self.page {
            Some(Page::Settings) => &mut self.settings_scroll,
            _ => &mut self.scroll,
        };
        *scroll = scroll.saturating_add_signed(delta);
    }

    /// 页面当前的 (厂商, 账号) 选择是否已由订阅覆盖（已确认或等待确认）。
    fn subscribed(&self) -> bool {
        let subscription = &self.subscription;
        subscription
            .active
            .as_ref()
            .map(|active| &active.params)
            .or(subscription.requested.as_ref().map(|(params, _)| params))
            .is_some_and(|params| {
                UsageSubscription::covers(
                    params,
                    self.selected_provider.as_deref(),
                    self.selected_account.as_deref(),
                )
            })
    }

    /// 请求的代际：悬浮层请求按悬浮层代际，页面请求按页面代际，订阅生命周期不绑代际。
    fn epoch_for(&self, purpose: &Purpose) -> u64 {
        if purpose.is_hover() {
            self.hover_scope.epoch
        } else if purpose.page_scoped() {
            self.epoch
        } else {
            0
        }
    }

    /// 把一条事件里的刷新状态合并进某作用域：事件里 `binding_inferred` /
    /// `pending_binding` 按请求才有意义、保持缺省，因此沿用已有条目的值。
    fn merge_refresh_state(&mut self, hover: bool, incoming: UsageRefreshState) {
        let states = if hover {
            &mut self.hover_scope.refresh_states
        } else {
            &mut self.refresh_states
        };
        if let Some(existing) = states
            .iter_mut()
            .find(|state| state.account_id == incoming.account_id)
        {
            let binding_inferred = existing.binding_inferred;
            let pending_binding = existing.pending_binding.take();
            *existing = incoming;
            existing.binding_inferred = binding_inferred;
            existing.pending_binding = pending_binding;
        } else {
            states.push(incoming);
        }
    }

    /// 页面作用域是否接受某账号：按用户当前的厂商 / 账号选择过滤（服务端已按
    /// 订阅参数过滤，这里挡住切换作用域后旧订阅的尾巴），本机关闭的厂商一律不收
    /// （服务端只按它自己的 TOML 过滤）。
    fn page_scope_accepts(&self, account: &AccountUsageSnapshot) -> bool {
        !self.usage.disabled_providers.contains(&account.agent)
            && self
                .selected_provider
                .as_deref()
                .is_none_or(|agent| agent == account.agent)
            && self
                .selected_account
                .as_deref()
                .is_none_or(|id| id == account.account_id)
    }

    /// 按账号 id 判定页面作用域是否接受：先从页面快照、再从厂商列表解析该账号
    /// 所属厂商，解析不出即不接受（切换作用域后旧订阅的尾巴、未知账号都挡在外面）。
    fn page_scope_accepts_id(&self, account_id: &str) -> bool {
        let agent = self
            .accounts
            .iter()
            .find(|account| account.account_id == account_id)
            .map(|account| account.agent.as_str())
            .or_else(|| {
                self.providers
                    .iter()
                    .find(|provider| {
                        provider
                            .configured_accounts
                            .iter()
                            .any(|id| id == account_id)
                    })
                    .map(|provider| provider.agent.as_str())
            });
        let Some(agent) = agent else {
            return false;
        };
        !self.usage.disabled_providers.iter().any(|d| d == agent)
            && self
                .selected_provider
                .as_deref()
                .is_none_or(|selected| selected == agent)
            && self
                .selected_account
                .as_deref()
                .is_none_or(|selected| selected == account_id)
    }

    /// 某 pane 的绑定已落地（本端或别的客户端 / CLI 完成）：两个作用域里挂在该
    /// pane 上的待办绑定立即清掉，不等下一次整体替换。
    fn clear_pending_binding(&mut self, pane_id: &str) {
        for states in [
            &mut self.refresh_states,
            &mut self.hover_scope.refresh_states,
        ] {
            for state in states.iter_mut() {
                if state
                    .pending_binding
                    .as_ref()
                    .is_some_and(|pending| pending.pane_id == pane_id)
                {
                    state.pending_binding = None;
                }
            }
        }
    }

    /// `account.usage.updated`：按 `account_id` 合并进页面账号；随附的刷新状态一并
    /// 合并，缺失时表示探测已完成，清掉在途标记。
    fn merge_page_account(
        &mut self,
        account: AccountUsageSnapshot,
        refresh: Option<UsageRefreshState>,
    ) {
        let account_id = account.account_id.clone();
        record_usage_history(&mut self.usage_history, std::slice::from_ref(&account));
        if let Some(slot) = self
            .accounts
            .iter_mut()
            .find(|existing| existing.account_id == account_id)
        {
            *slot = account;
        } else {
            self.accounts.push(account);
        }
        match refresh {
            Some(refresh) => self.merge_refresh_state(false, refresh),
            None => {
                if let Some(state) = self
                    .refresh_states
                    .iter_mut()
                    .find(|state| state.account_id == account_id)
                {
                    state.in_flight = false;
                    state.queued = false;
                }
            }
        }
    }

    /// 某作用域是否还有用量请求在途（整体请求或任一厂商的逐厂商请求）。
    fn usage_in_flight(&self, hover: bool) -> bool {
        self.pending
            .iter()
            .any(|key| Purpose::is_usage_key(key, hover))
    }

    /// 响应到达：只释放该请求自己的在途键。整体请求（`agent=None`）与逐厂商请求
    /// 可能交叠（列表迟到后回落整体请求、随后列表到达又逐厂商扇出），各释放各的，
    /// 「刷新中」等全部在途响应到齐才结束；成批作废由 `bump_page_epoch` /
    /// `reset_hover_scope` 负责。
    fn settle_pending(&mut self, purpose: &Purpose) {
        self.pending.remove(purpose.key().as_ref());
    }

    /// 某作用域本轮强意图刷新已发出 `refresh` 的厂商。
    fn manual_sent(&mut self, hover: bool) -> &mut Vec<Option<String>> {
        if hover {
            &mut self.hover_scope.manual_sent
        } else {
            &mut self.manual_sent
        }
    }

    /// 用量响应落入作用域：先按本机关闭的厂商过滤（服务端只按它自己的 TOML
    /// 过滤，旧 server / 整体请求仍会带回本机关闭的厂商），再按请求带的厂商合并——
    /// `agent=Some(x)` 只替换 x 的账号与刷新状态，其它厂商保留，并按厂商列表顺序
    /// 稳定排列；`agent=None` 整体替换。
    fn merge_usage_response(
        &mut self,
        hover: bool,
        agent: Option<&str>,
        accounts: Vec<AccountUsageSnapshot>,
        refresh: Vec<UsageRefreshState>,
    ) {
        let disabled = &self.usage.disabled_providers;
        let (accounts, dropped): (Vec<_>, Vec<_>) = accounts
            .into_iter()
            .partition(|account| !disabled.contains(&account.agent));
        let refresh = refresh
            .into_iter()
            .filter(|state| {
                !dropped
                    .iter()
                    .any(|account| account.account_id == state.account_id)
            })
            .collect::<Vec<_>>();
        let providers = &self.providers;
        let provider_order = |agent: &str| {
            providers
                .iter()
                .position(|provider| provider.agent == agent)
                .unwrap_or(usize::MAX)
        };
        // 作用域已选中厂商时，该厂商的响应就是整个作用域的权威快照：切换厂商后
        // 保留到此刻的旧厂商账号（渲染期变暗）随之替换掉，不会与新厂商并存。
        let scope_provider = if hover {
            self.hover_scope.provider.as_deref()
        } else {
            self.selected_provider.as_deref()
        };
        let agent = agent.filter(|_| scope_provider.is_none());
        let (scope_accounts, scope_refresh) = if hover {
            (
                &mut self.hover_scope.accounts,
                &mut self.hover_scope.refresh_states,
            )
        } else {
            (&mut self.accounts, &mut self.refresh_states)
        };
        match agent {
            None => {
                *scope_accounts = accounts;
                *scope_refresh = refresh;
            }
            Some(agent) => {
                // 该厂商的旧刷新状态一律让位给本次权威响应。归属按厂商解析（本次
                // 响应里的账号 → 作用域快照 → 厂商列表的已配置账号），不按「作用域
                // 里已有的账号 id」：只经 refreshing 事件写入、尚未出现在快照里的
                // 状态否则永远清不掉，还会与本次响应里的同 id 条目重复。
                let owned = |account_id: &str| {
                    accounts
                        .iter()
                        .any(|account| account.account_id == account_id)
                        || refresh.iter().any(|state| state.account_id == account_id)
                        || scope_accounts.iter().any(|account| {
                            account.account_id == account_id && account.agent == agent
                        })
                        || providers.iter().any(|provider| {
                            provider.agent == agent
                                && provider
                                    .configured_accounts
                                    .iter()
                                    .any(|id| id == account_id)
                        })
                };
                let stale = scope_refresh
                    .iter()
                    .filter(|state| owned(&state.account_id))
                    .map(|state| state.account_id.clone())
                    .collect::<Vec<_>>();
                scope_refresh.retain(|state| !stale.contains(&state.account_id));
                scope_accounts.retain(|account| account.agent != agent);
                scope_accounts.extend(accounts);
                scope_refresh.extend(refresh);
                scope_accounts.sort_by_key(|account| provider_order(&account.agent));
            }
        }
        // 样本只记页面作用域：悬浮层是逐 pane 的临时视图，不进 sparkline 历史。
        if !hover {
            record_usage_history(&mut self.usage_history, &self.accounts);
        }
    }

    /// 本机关闭的厂商集合变了：两个作用域里它们的账号与刷新状态立即离开，并唤醒
    /// 两个作用域的轮询（重新启用的厂商下一次 tick 即补查）。
    fn apply_disabled_providers(&mut self) {
        let disabled = &self.usage.disabled_providers;
        for (accounts, refresh_states) in [
            (&mut self.accounts, &mut self.refresh_states),
            (
                &mut self.hover_scope.accounts,
                &mut self.hover_scope.refresh_states,
            ),
        ] {
            let stale = accounts
                .iter()
                .filter(|account| disabled.contains(&account.agent))
                .map(|account| account.account_id.clone())
                .collect::<Vec<_>>();
            if stale.is_empty() {
                continue;
            }
            accounts.retain(|account| !stale.contains(&account.account_id));
            refresh_states.retain(|state| !stale.contains(&state.account_id));
        }
        let now = Instant::now();
        self.next_usage = now;
        self.hover_scope.next_usage = now;
    }

    /// 页面作用域换代：作废所有在途页面请求（悬浮层的请求与订阅生命周期不受影响）。
    /// 旧账号快照保留到新数据到达（渲染期变暗），但旧刷新状态（防抖截止、待办绑定）
    /// 属于旧作用域，一并清掉。
    fn bump_page_epoch(&mut self) {
        self.epoch = self.epoch.saturating_add(1);
        self.pending
            .retain(|key| Purpose::is_page_independent_key(key));
        self.manual_in_flight = false;
        self.manual_sent.clear();
        self.refresh_states.clear();
    }

    /// 页面作用域的强意图刷新：下一次 tick 立即发 `account.usage.refresh`。
    fn request_refresh(&mut self) {
        self.refresh_usage = true;
        self.next_usage = Instant::now();
    }

    /// 复位悬浮层作用域并换代，作废在途悬浮层请求。
    fn reset_hover_scope(&mut self) {
        let epoch = self.hover_scope.epoch.saturating_add(1);
        self.hover_scope = HoverScope {
            epoch,
            ..HoverScope::default()
        };
        self.pending.retain(|key| !Purpose::is_hover_key(key));
    }

    /// 结束悬浮层：hover 本身与悬浮层作用域一并复位，页面作用域
    /// （`selected_*` / `accounts` / `epoch` / `account_scroll`）保持不动。
    pub(super) fn clear_hover(&mut self) {
        self.hover = None;
        self.reset_hover_scope();
    }

    /// 「切换账号」的候选：选中厂商时为其已配置账号；总览态为所有已列出
    /// 厂商的已配置账号（顺序稳定，不受当前账号快照影响）。设置里关闭的厂商
    /// 两种情况下都不参与。渲染期只取长度，因此返回迭代器而不分配。
    pub(super) fn cycle_candidates<'a>(
        &'a self,
        provider: Option<&'a str>,
    ) -> impl Iterator<Item = &'a str> + 'a {
        self.providers
            .iter()
            .filter(move |candidate| match provider {
                Some(agent) => candidate.agent == agent,
                None => provider_listed(candidate),
            })
            .filter(|candidate| !self.usage.disabled_providers.contains(&candidate.agent))
            .flat_map(|candidate| candidate.configured_accounts.iter().map(String::as_str))
    }

    /// 作用域内可操作的账号：显式选中且属于该作用域，否则该作用域唯一的账号。
    fn scoped_account(&self, hover: bool) -> Option<String> {
        let accounts = if hover {
            &self.hover_scope.accounts
        } else {
            &self.accounts
        };
        self.selected_account
            .clone()
            .filter(|selected| {
                !hover
                    || accounts
                        .iter()
                        .any(|account| &account.account_id == selected)
            })
            .or_else(|| (accounts.len() == 1).then(|| accounts[0].account_id.clone()))
    }

    pub(super) fn reload_preferences(&mut self, config: &ClientShellConfig) {
        self.monitor = config
            .preferences
            .monitor
            .clone()
            .unwrap_or_else(|| config.monitor.clone());
        self.usage = config.account_usage.clone();
        if let Some(value) = config.preferences.usage_enabled {
            self.usage.enabled = value;
        }
        if let Some(value) = config.preferences.usage_format {
            self.usage.format = value;
        }
        if let Some(value) = config.preferences.usage_position {
            self.usage.position = value;
        }
        if let Some(value) = &config.preferences.usage_disabled_providers {
            self.usage.disabled_providers.clone_from(value);
        }
        if let Some(value) = config.preferences.usage_hover_delay_ms {
            self.usage.hover_delay_ms = value;
        }
        if let Some(value) = config.preferences.monitor_tab {
            self.monitor_tab = value;
        }
        self.chart_glyphs = config.preferences.monitor_chart_glyphs.unwrap_or_default();
        self.usage_overridden = config.preferences.usage_overridden();
        self.glyphs = config.border_glyphs;
        self.next_metrics = Instant::now();
        self.apply_disabled_providers();
    }
    pub fn new(config: &ClientShellConfig) -> Self {
        let mut usage = config.account_usage.clone();
        if let Some(value) = config.preferences.usage_enabled {
            usage.enabled = value;
        }
        if let Some(value) = config.preferences.usage_format {
            usage.format = value;
        }
        if let Some(value) = config.preferences.usage_position {
            usage.position = value;
        }
        if let Some(value) = &config.preferences.usage_disabled_providers {
            usage.disabled_providers.clone_from(value);
        }
        if let Some(value) = config.preferences.usage_hover_delay_ms {
            usage.hover_delay_ms = value;
        }
        Self {
            page: None,
            monitor_tab: config.preferences.monitor_tab.unwrap_or(Page::Monitor),
            monitor: config
                .preferences
                .monitor
                .clone()
                .unwrap_or_else(|| config.monitor.clone()),
            usage,
            metrics: None,
            history: VecDeque::new(),
            usage_history: HashMap::new(),
            providers: Vec::new(),
            accounts: Vec::new(),
            selected_provider: None,
            selected_account: None,
            selected_pane: None,
            selected_pane_label: None,
            refresh_states: Vec::new(),
            subscription: UsageSubscription::default(),
            hover: None,
            hover_scope: HoverScope::default(),
            usage_overridden: config.preferences.usage_overridden(),
            queued_binding: None,
            event_repaint: false,
            page_seen: false,
            manual_in_flight: false,
            hover_rect: Rect::default(),
            hover_hits: Vec::new(),
            page_rect: Rect::default(),
            hits: Vec::new(),
            page_hits: 0,
            selected_hit: 0,
            selected_card: None,
            selected_core: None,
            layout_editing: false,
            chart_glyphs: config.preferences.monitor_chart_glyphs.unwrap_or_default(),
            net_history: HashMap::new(),
            card_scroll: HashMap::new(),
            account_scroll: 0,
            scroll: 0,
            settings_scroll: 0,
            glyphs: config.border_glyphs,
            paused: false,
            process_dialog: None,
            process_filter: String::new(),
            filtering_processes: false,
            process_sort: ProcessSort::Cpu,
            message: None,
            now_ms: 0,
            epoch: 0,
            source: None,
            usage_source: None,
            next_metrics: Instant::now(),
            next_usage: Instant::now(),
            usage_sent_at: None,
            next_providers: Instant::now(),
            refresh_usage: false,
            manual_sent: Vec::new(),
            providers_sent_at: None,
            auto_select_provider: false,
            accounts_page_seen: false,
            fallback_noted: None,
            pending: HashSet::new(),
            alerts: HashMap::new(),
        }
    }

    /// 每帧绘制前复位上一帧的命中区与矩形；之后每次 `paint` 的结果经
    /// `commit_paint` 累加写回。
    pub(super) fn begin_paint(&mut self) {
        self.hits.clear();
        self.page_hits = 0;
        self.hover_hits.clear();
        self.hover_rect = Rect::default();
        self.page_rect = Rect::default();
    }

    /// 渲染纯函数：把 `painting_page`（停靠面板传该面板的 tab，全局浮层传 `None`）、
    /// 可见的悬浮层（`draw_hover` 为真时；停靠面板逐个绘制时传 `false`，悬浮层
    /// 由随后的全局 pass 画一次）与进程对话框画进 `frame` 的 `area`，状态只读。
    ///
    /// 光标归属按覆盖区判定：只有页面矩形、悬浮层矩形或对话框矩形真正包含
    /// `frame.cursor` 坐标时才把整帧光标置空；停靠在旁边的监控面板不能抹掉
    /// 聚焦终端的插入点（经典布局下页面铺满 pane 区域，行为不变）。
    pub(super) fn paint(
        &self,
        canvas: &mut super::compose_canvas::ComposeCanvas,
        area: Rect,
        cx: &super::feedback::ChromeContext<'_>,
        painting_page: Option<Page>,
        draw_hover: bool,
    ) -> Option<Painted> {
        let palette = cx.palette;
        let visible = painting_page.is_some()
            || self.process_dialog.is_some()
            || (draw_hover && self.hover.as_ref().is_some_and(|hover| hover.visible));
        if !visible {
            return None;
        }
        let output = render::paint(canvas.buffer(), area, self, cx, painting_page, draw_hover);
        if painting_page.is_some() {
            let selected = self.selected_hit.min(output.hits.len().saturating_sub(1));
            if let Some((rect, _)) = output.hits.get(selected) {
                canvas.buffer().set_style(
                    *rect,
                    Style::default()
                        .fg(palette.accent)
                        .add_modifier(Modifier::UNDERLINED),
                );
            }
        }
        let cursor = canvas.cursor().filter(|cursor| {
            let point = (cursor.x, cursor.y);
            !(contains(output.page_rect, point)
                || contains(output.hover_rect, point)
                || contains(output.dialog_rect, point))
        });
        canvas.set_cursor(cursor);
        let dialog = !output.dialog_rect.is_empty();
        // 页面与悬浮层可同帧出现：两块各自交给 kitty 图片 occlusion，
        // 不合成外接矩形（见 `Painted::covered`）。
        let covered = if dialog {
            [output.dialog_rect, Rect::default()]
        } else {
            [output.page_rect, output.hover_rect]
        };
        Some(Painted {
            covered,
            hits: output.hits,
            hover_hits: output.hover_hits,
            page_rect: output.page_rect,
            hover_rect: output.hover_rect,
            dialog,
        })
    }

    /// 把一次绘制的命中区写回：页面命中区累加（多个停靠面板依次绘制），
    /// 悬浮层命中区单独记录以便浮层独占输入，进程对话框出现时独占全部命中区。
    pub(super) fn commit_paint(&mut self, painted: Painted) {
        if painted.dialog {
            self.hits = painted.hits;
            self.page_hits = self.hits.len();
            self.hover_rect = Rect::default();
            self.hover_hits.clear();
        } else {
            if !painted.hover_rect.is_empty() {
                self.hover_hits.clone_from(&painted.hover_hits);
                self.hover_rect = painted.hover_rect;
            }
            // 页面命中区插在总表的页面段末尾（多个停靠面板依次绘制、全局 pass 的
            // 悬浮层可能先于后画的面板提交），悬浮层命中区追加在其后：总表仍可列举
            // 浮层按钮，但键盘只在 `..page_hits` 内循环，浮层内输入只查 `hover_hits`。
            let page_end = self.page_hits;
            self.page_hits = page_end.saturating_add(painted.hits.len());
            self.hits.splice(page_end..page_end, painted.hits);
            self.hits.extend(painted.hover_hits);
        }
        if !painted.page_rect.is_empty() {
            self.page_rect = painted.page_rect;
        }
        self.selected_hit = self.selected_hit.min(self.page_hits.saturating_sub(1));
    }

    fn apply_metrics(&mut self, snapshot: Box<SystemMetricsSnapshot>) {
        if self
            .metrics
            .as_ref()
            .is_some_and(|old| old.boot_id == snapshot.boot_id && old.sequence >= snapshot.sequence)
        {
            return;
        }
        if self
            .metrics
            .as_ref()
            .is_some_and(|old| old.boot_id != snapshot.boot_id)
        {
            self.history.clear();
            self.net_history.clear();
            self.alerts.clear();
        }
        if !self.paused && snapshot.sampled_at_ms > 0 {
            // 网络接口速率：消失的接口随之丢弃；两个方向都未知（首次差分）时不记
            // 样本，空闲判定才不会把「还没算出速率」当成活动。
            self.net_history
                .retain(|id, _| snapshot.networks.iter().any(|net| net.id == *id));
            for net in &snapshot.networks {
                let (Some(rx), Some(tx)) = (
                    net.received_bytes_per_second,
                    net.transmitted_bytes_per_second,
                ) else {
                    continue;
                };
                let sample = (rx.max(0.0) as f32, tx.max(0.0) as f32);
                match self.net_history.get_mut(&net.id) {
                    Some(samples) => {
                        if samples.len() >= NET_HISTORY_SAMPLES {
                            samples.pop_front();
                        }
                        samples.push_back(sample);
                    }
                    None => {
                        self.net_history
                            .insert(net.id.clone(), VecDeque::from([sample]));
                    }
                }
            }
            let memory = (snapshot.memory.total_bytes > 0).then(|| {
                snapshot.memory.used_bytes as f32 / snapshot.memory.total_bytes as f32 * 100.0
            });
            self.history.push_back(HistoryPoint {
                at: snapshot.sampled_at_ms,
                cpu: snapshot.cpu_percent,
                memory,
                cores: snapshot
                    .cores
                    .iter()
                    .map(|core| core.usage_percent)
                    .collect(),
            });
            let cutoff = snapshot
                .sampled_at_ms
                .saturating_sub(u64::from(self.monitor.history_minutes.clamp(1, 60)) * 60_000);
            let max_points =
                (16 * 1024 * 1024 / (64 + snapshot.cores.len().saturating_mul(8))).clamp(2, 7200);
            while self.history.len() > max_points
                || self.history.front().is_some_and(|point| point.at < cutoff)
            {
                self.history.pop_front();
            }
        }
        self.metrics = Some(snapshot);
    }

    fn alert(&mut self) -> Option<String> {
        if !self.monitor.alerts_enabled {
            self.alerts.clear();
            return None;
        }
        let metrics = self.metrics.as_ref()?;
        if metrics.status != ObservationStatus::Ready
            || self.now_ms.saturating_sub(metrics.sampled_at_ms) > 15_000
        {
            return None;
        }
        for rule in &self.monitor.alerts {
            let group = if rule.metric == "disk" {
                "disks"
            } else {
                rule.metric.as_str()
            };
            if metrics
                .group_status
                .get(group)
                .is_some_and(|status| *status != ObservationStatus::Ready)
                || metrics
                    .group_sampled_at_ms
                    .get(group)
                    .is_some_and(|sampled| self.now_ms.saturating_sub(*sampled) > 15_000)
            {
                continue;
            }
            let value = match rule.metric.as_str() {
                "cpu" => metrics.cpu_percent.map(f64::from),
                "memory" => (metrics.memory.total_bytes > 0).then(|| {
                    metrics.memory.used_bytes as f64 / metrics.memory.total_bytes as f64 * 100.0
                }),
                "gpu" => metrics
                    .gpus
                    .iter()
                    .filter(|gpu| gpu.status == ObservationStatus::Ready)
                    .filter_map(|gpu| gpu.usage_percent)
                    .reduce(f32::max)
                    .map(f64::from),
                "disk" => metrics
                    .disks
                    .iter()
                    .filter(|disk| disk.total_bytes > 0)
                    .map(|disk| {
                        (disk.total_bytes.saturating_sub(disk.available_bytes)) as f64
                            / disk.total_bytes as f64
                            * 100.0
                    })
                    .reduce(f64::max),
                _ => None,
            };
            let state = self.alerts.entry(rule.metric.clone()).or_default();
            let Some(value) = value.filter(|v| v.is_finite()) else {
                state.since = None;
                continue;
            };
            if value < rule.threshold - 5.0 {
                state.since = None;
                state.armed = false;
            } else if value >= rule.threshold {
                let since = state.since.get_or_insert(self.now_ms);
                if !state.armed
                    && self.now_ms.saturating_sub(*since)
                        >= rule.duration_seconds.saturating_mul(1000)
                    && state.last_fired.is_none_or(|last| {
                        self.now_ms.saturating_sub(last)
                            >= rule.cooldown_seconds.saturating_mul(1000)
                    })
                {
                    state.armed = true;
                    state.last_fired = Some(self.now_ms);
                    return Some(format!(
                        "{} · {} {:.1}%",
                        metrics.hostname, rule.metric, value
                    ));
                }
            } else {
                state.since = None;
            }
        }
        None
    }
}

impl ClientShellState {
    /// 回到跨厂商总览（`Action::Overview`）：与选厂商同样是显式选择——页面作用域
    /// 换代、强意图刷新，旧快照保留到新数据到达；自动选中不再抢回厂商。
    fn select_usage_overview(&mut self) {
        self.observability.auto_select_provider = false;
        self.observability.account_scroll = 0;
        self.observability.bump_page_epoch();
        self.observability.selected_provider = None;
        self.observability.selected_account = None;
        self.observability.selected_pane = None;
        self.observability.selected_pane_label = None;
        self.observability.request_refresh();
    }

    /// 显式选择厂商（用户点击或账号页打开时的自动选中）：页面作用域换代、
    /// 强意图刷新；旧账号快照保留到新数据到达（渲染期变暗）。
    fn select_usage_provider(&mut self, agent: String) {
        self.observability.account_scroll = 0;
        self.observability.bump_page_epoch();
        self.observability.selected_provider = Some(agent);
        self.observability.selected_account = None;
        self.observability.selected_pane = None;
        self.observability.selected_pane_label = None;
        self.observability.request_refresh();
    }

    /// 账号页可见且尚未选厂商时：按聚焦 pane 正在运行的 agent 选，否则选第一个
    /// 已列出（已安装且未在设置里关闭）的厂商；厂商列表未到时保留标志，列表
    /// 到达后补选。
    fn auto_select_usage_provider(&mut self) {
        if !self.observability.auto_select_provider {
            return;
        }
        if self.observability.selected_provider.is_some() {
            self.observability.auto_select_provider = false;
            return;
        }
        let listed = self
            .observability
            .providers
            .iter()
            .filter(|provider| provider_listed(provider))
            .filter(|provider| {
                !self
                    .observability
                    .usage
                    .disabled_providers
                    .contains(&provider.agent)
            })
            .map(|provider| provider.agent.clone())
            .collect::<Vec<_>>();
        let Some(first) = listed.first().cloned() else {
            return;
        };
        // 停靠工作台下打开页面时焦点已在监控面板，视图焦点为空；回退到
        // 快照里服务端记录的聚焦 pane。
        let focused = self
            .focused_pane_id()
            .or_else(|| self.snapshot.as_ref()?.focused_pane_id.clone())
            .and_then(|pane_id| {
                self.snapshot
                    .as_ref()?
                    .agents
                    .iter()
                    .find(|agent| agent.pane_id == pane_id)?
                    .agent
                    .clone()
            });
        let pick = focused
            .filter(|agent| listed.iter().any(|listed| listed == agent))
            .unwrap_or(first);
        self.observability.auto_select_provider = false;
        self.select_usage_provider(pick);
    }

    pub(super) fn open_observation_page(&mut self, page: Page, outcome: &mut ClientShellInput) {
        // One dock panel hosts system, accounts, and settings pages; the
        // requested page becomes the active tab inside it and is remembered
        // across focus changes (persisted as a client preference).
        let tab_changed = self.config.preferences.monitor_tab != Some(page);
        self.observability.monitor_tab = page;
        self.workbench_open(dock::PanelId::Monitor);
        self.observability.page = Some(page);
        if tab_changed {
            self.config.preferences.monitor_tab = Some(page);
            self.persist_chrome_preferences(outcome);
        }
        self.observability.clear_hover();
        // 滚动位置按页分离：打开哪页只复位哪页，设置页的滚动不再泄漏到系统页。
        match page {
            Page::Monitor => self.observability.scroll = 0,
            Page::Settings => self.observability.settings_scroll = 0,
            Page::Accounts => {}
        }
        // 编辑布局只属于系统页。
        if page != Page::Monitor {
            self.observability.layout_editing = false;
        }
        self.observability.next_metrics = Instant::now();
        self.observability.next_usage = Instant::now();
        self.observability.next_providers = Instant::now();
        self.observability.message = None;
        if page == Page::Accounts && self.observability.selected_provider.is_none() {
            self.observability.auto_select_provider = true;
            self.auto_select_usage_provider();
        }
        self.selection = None;
        self.clear_link_hover();
        self.mode = ClientShellMode::Terminal;
        outcome.repaint = true;
    }

    /// 发出一次观测请求；结局见 `RequestOutcome`：被在途请求挡下（`Busy`）时
    /// 调用方保留强意图刷新标志稍后重试，端点离线 / 未宣告方法 / 无快照
    /// （`Unavailable`）时清除标志，只禁用该动作而不让轮询停摆。
    ///
    /// 悬浮层作用域的请求（`Purpose::is_hover`：用量、绑定、官方回调）走
    /// `hover_scope.endpoint` 与 `hover_scope.epoch`，其余走活动端点与页面
    /// `epoch`；目标端点不在线时回退到活动端点，并在回退目标变化时在页脚说明。
    fn observation_request(
        &mut self,
        method: Method,
        purpose: Purpose,
        outcome: &mut ClientShellInput,
    ) -> RequestOutcome {
        self.observation_request_at(None, method, purpose, outcome)
    }

    /// 同 `observation_request`，但 `target` 有值时严格发往该端点：不回退到活动
    /// 端点、不在页脚说明，端点不在线即 `Unavailable`。绑定（pane id 只在其所在
    /// 主机有意义）与退订（订阅 id 只在创建它的 server 上有意义）必须用严格目标。
    fn observation_request_at(
        &mut self,
        target: Option<ClientEndpointId>,
        method: Method,
        purpose: Purpose,
        outcome: &mut ClientShellInput,
    ) -> RequestOutcome {
        let key = purpose.key();
        if self.observability.pending.contains(key.as_ref()) {
            return RequestOutcome::Busy;
        }
        let strict = target.is_some();
        let preferred = target
            .or_else(|| {
                if purpose.is_hover() {
                    self.observability.hover_scope.endpoint.clone()
                } else {
                    None
                }
            })
            .unwrap_or_else(|| self.active_endpoint_id.clone());
        let online = |endpoints: &[ClientShellEndpoint], id: &ClientEndpointId| {
            endpoints.iter().any(|endpoint| {
                &endpoint.endpoint_id == id && endpoint.status == ClientEndpointStatus::Online
            })
        };
        let endpoint_id = if online(&self.endpoints, &preferred) {
            if self.observability.fallback_noted.as_ref() == Some(&preferred) {
                // 目标端点回来了：下次再离线时重新说明。
                self.observability.fallback_noted = None;
            }
            preferred
        } else if strict {
            return RequestOutcome::Unavailable;
        } else if preferred != self.active_endpoint_id
            && online(&self.endpoints, &self.active_endpoint_id)
        {
            if self.observability.fallback_noted.as_ref() != Some(&preferred) {
                self.observability.message = Some(
                    tr(
                        "The selected host is offline; querying the active host instead.",
                        "所选主机不在线，已改为查询当前主机。",
                    )
                    .into(),
                );
                self.observability.fallback_noted = Some(preferred);
            }
            self.active_endpoint_id.clone()
        } else {
            return RequestOutcome::Unavailable;
        };
        let Some(endpoint) = self
            .endpoints
            .iter()
            .find(|endpoint| endpoint.endpoint_id == endpoint_id)
        else {
            return RequestOutcome::Unavailable;
        };
        if endpoint
            .methods
            .as_ref()
            .is_none_or(|methods| !methods.contains(crate::api::api_method_name(&method)))
        {
            self.observability.message = Some(
                tr(
                    "This server does not support this feature yet.",
                    "此主机尚未支持该功能，请更新 server。",
                )
                .into(),
            );
            return RequestOutcome::Unavailable;
        }
        let Some(snapshot) = endpoint.snapshot.as_ref() else {
            return RequestOutcome::Unavailable;
        };
        let request_id = format!("client-shell:{}", self.next_request_id);
        self.next_request_id = self.next_request_id.saturating_add(1);
        let boot_id = snapshot.boot_id.clone();
        let epoch = self.observability.epoch_for(&purpose);
        self.pending_requests.insert(
            request_id.clone(),
            PendingEndpointRequest {
                boot_id: boot_id.clone(),
                method_name: crate::api::api_method_name(&method).into(),
                confirmation_workspace_id: None,
                kind: PendingEndpointKind::Observation {
                    epoch,
                    endpoint_id: endpoint_id.clone(),
                    purpose,
                },
            },
        );
        outcome.actions.push(ClientShellAction::Endpoint {
            endpoint_id,
            boot_id,
            request: Box::new(Request {
                id: request_id,
                method,
            }),
            coalesce: false,
        });
        self.observability.pending.insert(key.into_owned());
        RequestOutcome::Sent
    }

    /// 某作用域这一轮该向哪些厂商发用量请求。`disabled_providers` 是双真源：
    /// 服务端 TOML 是它自己的底线（不为任何客户端探测），本机偏好是本机覆盖——
    /// 因此总览不发 `agent=None` 让服务端探测本机关闭的厂商，而是按本机关闭
    /// 过滤后逐厂商发 `agent=Some(x)`；响应侧按厂商合并。
    ///
    /// 扇出只在服务端给出可判定信息时展开：列表里任一厂商缺 `installed`（旧
    /// server 不宣告该字段，`provider_listed` 按未知保持列出）就回落整体请求，
    /// 否则注册表的几十个厂商会被全部请求一遍；已列出厂商超过
    /// `MAX_USAGE_FAN_OUT` 同样回落。列表请求刚发出（`PROVIDERS_WAIT` 内）才
    /// 等它，丢失 / 报错后回落整体请求，轮询不停摆。
    fn usage_targets(&self, hover: bool, now: Instant) -> UsageTargets {
        let state = &self.observability;
        let provider = if hover {
            state.hover_scope.provider.as_deref()
        } else {
            state.selected_provider.as_deref()
        };
        if let Some(provider) = provider {
            return if state.usage.disabled_providers.iter().any(|d| d == provider) {
                UsageTargets::Disabled
            } else {
                UsageTargets::Agents(vec![Some(provider.to_owned())])
            };
        }
        // 总览态。悬浮层定向到别的主机时本地缓存的厂商列表不属于它，回落到整体请求。
        let remote_hover = hover
            && state
                .hover_scope
                .endpoint
                .as_ref()
                .is_some_and(|endpoint| endpoint != &self.active_endpoint_id);
        if remote_hover {
            return UsageTargets::Agents(vec![None]);
        }
        if state.providers.is_empty() {
            // 列表刚发出、仍在途：等它到达再逐厂商发（到达即唤醒轮询）。端点不支持
            // 列表方法 / 列表为空 / 列表迟迟不到时只能整体请求，响应侧再按本机关闭过滤。
            let awaiting = state.pending.contains(Purpose::Providers.key().as_ref())
                && state
                    .providers_sent_at
                    .is_some_and(|sent_at| now.saturating_duration_since(sent_at) < PROVIDERS_WAIT);
            return if awaiting {
                UsageTargets::AwaitProviders
            } else {
                UsageTargets::Agents(vec![None])
            };
        }
        if state
            .providers
            .iter()
            .any(|provider| provider.installed.is_none())
        {
            return UsageTargets::Agents(vec![None]);
        }
        let listed = state
            .providers
            .iter()
            .filter(|provider| provider_listed(provider))
            .collect::<Vec<_>>();
        if listed.is_empty() {
            return UsageTargets::NoProviders;
        }
        let agents = listed
            .iter()
            .filter(|provider| !state.usage.disabled_providers.contains(&provider.agent))
            .map(|provider| Some(provider.agent.clone()))
            .collect::<Vec<_>>();
        if agents.is_empty() {
            UsageTargets::Disabled
        } else if agents.len() > MAX_USAGE_FAN_OUT {
            UsageTargets::Agents(vec![None])
        } else {
            UsageTargets::Agents(agents)
        }
    }

    /// 向 `agents` 逐个发某作用域的用量请求；`manual` 为真时走 `account.usage.refresh`，
    /// 且跳过本轮已发出 `refresh` 的厂商（`manual_sent`）、把新发出的记进去——
    /// 被在途请求挡下的厂商由调用方保留强意图、200 ms 后再来补发。
    /// 页面轮询永不带 pane_id（pane 只属于 `Action::Bind`），悬浮层带 hover 的 pane。
    /// 端点级不可用对所有厂商一样，直接返回。
    fn scope_usage_requests(
        &mut self,
        hover: bool,
        manual: bool,
        agents: &[Option<String>],
        outcome: &mut ClientShellInput,
    ) -> FanOut {
        let mut fan_out = FanOut::default();
        for agent in agents {
            if manual && self.observability.manual_sent(hover).contains(agent) {
                continue;
            }
            let agent = agent.clone();
            let params = if hover {
                UsageParams {
                    agent: agent.clone(),
                    account_id: None,
                    pane_id: self.observability.hover_scope.pane.clone(),
                }
            } else {
                UsageParams {
                    agent: agent.clone(),
                    account_id: self.observability.selected_account.clone(),
                    pane_id: None,
                }
            };
            let method = if manual {
                Method::AccountUsageRefresh(params)
            } else {
                Method::AccountUsageGet(params)
            };
            let purpose = if hover {
                Purpose::HoverUsage {
                    agent: agent.clone(),
                }
            } else {
                Purpose::Usage {
                    agent: agent.clone(),
                }
            };
            match self.observation_request(method, purpose, outcome) {
                RequestOutcome::Sent => {
                    fan_out.sent = true;
                    if manual {
                        self.observability.manual_sent(hover).push(agent);
                    }
                }
                RequestOutcome::Busy => fan_out.busy = true,
                // 端点离线 / 未宣告方法 / 无快照对所有厂商一样：不必再试其余厂商。
                RequestOutcome::Unavailable => {
                    fan_out.unavailable = true;
                    return fan_out;
                }
            }
        }
        fan_out
    }

    pub(crate) fn is_observation_request(&self, id: &str) -> bool {
        self.pending_requests
            .get(id)
            .is_some_and(|request| matches!(request.kind, PendingEndpointKind::Observation { .. }))
    }

    pub(crate) fn tick_observability(&mut self, now: Instant, outcome: &mut ClientShellInput) {
        // 活动 source（端点 + boot_id）按引用比较，只有变化时才分配（OBS-13）。
        let source_changed = self.snapshot.as_ref().is_some_and(|snapshot| {
            !self
                .observability
                .source
                .as_ref()
                .is_some_and(|(endpoint, boot_id)| {
                    endpoint == &self.active_endpoint_id && boot_id == &snapshot.boot_id
                })
        });
        if source_changed {
            self.observability.source = self
                .snapshot
                .as_ref()
                .map(|snapshot| (self.active_endpoint_id.clone(), snapshot.boot_id.clone()));
            // 大清理：新端点 / 新 boot，此前的观测数据、两个作用域与在途请求全部作废。
            self.observability.epoch = self.observability.epoch.saturating_add(1);
            self.observability.metrics = None;
            self.observability.history.clear();
            self.observability.accounts.clear();
            self.observability.providers.clear();
            self.observability.pending.clear();
            self.observability.clear_hover();
            self.observability.refresh_usage = false;
            self.observability.manual_in_flight = false;
            self.observability.refresh_states.clear();
            // 订阅归属于旧 (端点, boot)：已确认的进入退订队列（发回旧端点），
            // 在途的订阅请求在响应到达时再排队退订。
            self.observability.subscription.detach();
            self.observability.queued_binding = None;
            self.observability.fallback_noted = None;
            self.observability.process_dialog = None;
            self.observability.next_metrics = now;
            self.observability.next_usage = now;
            self.observability.next_providers = now;
        }
        let previous_second = self.observability.now_ms / 1000;
        self.observability.now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64;
        // 悬浮层状态机：离开延时到期即结束（钉住的浮层不设离开时刻）；停留满
        // hover_delay_ms 则可见，并把目标写进悬浮层作用域（页面作用域不动）。
        enum HoverStep {
            Leave,
            Show,
        }
        let hover_delay =
            Duration::from_millis(self.observability.usage.hover_delay_ms.clamp(200, 2000));
        let step = self.observability.hover.as_ref().and_then(|hover| {
            if !hover.pinned && hover.leave_at.is_some_and(|at| now >= at) {
                Some(HoverStep::Leave)
            } else if !hover.visible && now.duration_since(hover.since) >= hover_delay {
                Some(HoverStep::Show)
            } else {
                None
            }
        });
        match step {
            Some(HoverStep::Leave) => {
                self.observability.clear_hover();
                outcome.repaint = true;
            }
            Some(HoverStep::Show) => {
                let target = self.observability.hover.as_mut().map(|hover| {
                    hover.visible = true;
                    hover.target.clone()
                });
                match target {
                    Some(HoverTarget::Agent {
                        endpoint_id,
                        pane,
                        agent,
                    }) => {
                        self.observability.reset_hover_scope();
                        self.observability.hover_scope.provider = Some(agent);
                        self.observability.hover_scope.endpoint = Some(endpoint_id);
                        self.observability.hover_scope.pane = Some(pane);
                        // 悬浮层首次可见 = 悬浮层自己的强意图刷新；页面的滚动位置与
                        // 轮询节奏都不动。
                        self.observability.hover_scope.request_refresh();
                    }
                    None => {}
                }
                outcome.repaint = true;
            }
            None => {}
        }
        let hover_visible = self
            .observability
            .hover
            .as_ref()
            .is_some_and(|hover| hover.visible);
        // 账号页（经典布局页面 / 停靠面板的账号 tab / legacy 账号面板）可见。
        let accounts_page_visible = self.observability.page == Some(Page::Accounts)
            || (self.workbench.visible(&dock::PanelId::Monitor)
                && self.observability.monitor_tab == Page::Accounts)
            || self.workbench.visible(&dock::PanelId::Accounts);
        let usage_visible = accounts_page_visible || hover_visible;
        // 账号页由不可见变可见（含偏好恢复 monitor_tab=accounts、面板随布局
        // 恢复，这些路径不经过 open_observation_page）且尚未选厂商：自动选中。
        if accounts_page_visible
            && !self.observability.accounts_page_seen
            && self.observability.selected_provider.is_none()
        {
            self.observability.auto_select_provider = true;
            self.auto_select_usage_provider();
        }
        self.observability.accounts_page_seen = accounts_page_visible;
        // 用量轮询的 source：活动端点 + 其快照的 boot_id，快照缺失时为 None；
        // 与 `source` 的差别在于同 boot 断线重连也会作废页面作用域。
        let usage_changed = {
            let boot_id = self
                .snapshot
                .as_ref()
                .map(|snapshot| snapshot.boot_id.as_str());
            let current = boot_id.map(|boot_id| (&self.active_endpoint_id, boot_id));
            let saved = self
                .observability
                .usage_source
                .as_ref()
                .map(|(endpoint, boot_id)| (endpoint, boot_id.as_str()));
            (current != saved)
                .then(|| current.map(|(endpoint, boot_id)| (endpoint.clone(), boot_id.to_owned())))
        };
        if let Some(usage_source) = usage_changed {
            self.observability.usage_source = usage_source;
            self.observability.bump_page_epoch();
            self.observability.accounts.clear();
            self.observability.providers.clear();
            // 订阅归属于旧 (端点, boot)：已确认的进入退订队列（发回旧端点），
            // 在途的订阅请求响应到达时再排队退订；订阅键立即释放，好按新作用域重订。
            self.observability.subscription.detach();
            self.observability
                .pending
                .retain(|key| !matches!(key.as_str(), "subscribe" | "unsubscribe"));
            self.observability.queued_binding = None;
            self.observability.next_usage = now;
            self.observability.next_providers = now;
        }
        // 页面作用域由不可见变可见（订阅期间兜底轮询可能还有几十秒才到期）：
        // 冷启动一次 get，先把当前快照拿到手，再靠事件保持实时。
        if accounts_page_visible && !self.observability.page_seen {
            self.observability.next_usage = now;
        }
        self.observability.page_seen = accounts_page_visible;
        if usage_visible || self.observation_surface_visible() {
            outcome.repaint |= previous_second != self.observability.now_ms / 1000;
        }
        outcome.repaint |= std::mem::take(&mut self.observability.event_repaint);
        if !self.endpoint_is_online(&self.active_endpoint_id) {
            return;
        }
        // 官方回调启用成功后排队的绑定：被在途绑定请求挡下时保留到下一次 tick。
        // pane id 只在应答回调的主机上有意义：派发时该作用域的目标主机已变
        // （悬浮层移到别的主机、活动端点切换、悬浮层结束）就放弃并说明，绝不发往
        // 新主机——PaneId 是每 server 自增的小整数，跨主机必然撞号。
        if let Some(binding) = self.observability.queued_binding.take() {
            let current = if binding.hover {
                self.observability.hover_scope.endpoint.clone()
            } else {
                Some(self.active_endpoint_id.clone())
            };
            if current.as_ref() == Some(&binding.endpoint) {
                let purpose = if binding.hover {
                    Purpose::HoverBinding
                } else {
                    Purpose::Binding
                };
                let params = AccountBindingParams {
                    pane_id: binding.pane_id.clone(),
                    account_id: binding.account_id.clone(),
                };
                match self.observation_request_at(
                    Some(binding.endpoint.clone()),
                    Method::AccountBindingSet(params),
                    purpose,
                    outcome,
                ) {
                    RequestOutcome::Busy => self.observability.queued_binding = Some(binding),
                    RequestOutcome::Sent => {}
                    RequestOutcome::Unavailable => {
                        self.observability.message = Some(
                            tr(
                                "The pane's host is offline; the pane was not bound. Bind it from the accounts page later.",
                                "pane 所在主机不在线，未绑定；稍后可在账号页手动绑定。",
                            )
                            .into(),
                        );
                    }
                }
            } else {
                self.observability.message = Some(
                    tr(
                        "The target host changed before binding; the pane was not bound. Bind it from the accounts page.",
                        "绑定目标所在主机已变化，未绑定；请在账号页手动绑定。",
                    )
                    .into(),
                );
            }
        }
        if (self.observability.page == Some(Page::Monitor)
            || self.workbench.visible(&dock::PanelId::Monitor)
            || self.observability.monitor.alerts_enabled)
            && now >= self.observability.next_metrics
        {
            let config = &self.observability.monitor;
            let interval_ms = config.interval_ms.clamp(500, 5000);
            let params = SystemMetricsParams {
                interval_ms,
                include_processes: config.visible.iter().any(|item| item == "processes"),
                groups: config.visible.clone(),
            };
            self.observability.next_metrics = now + Duration::from_millis(interval_ms);
            self.observation_request(Method::SystemMetricsGet(params), Purpose::Metrics, outcome);
        }
        let settings_open = self.observability.page == Some(Page::Settings);
        // 页面作用域的订阅：账号页可见、厂商未关闭且端点宣告了订阅
        // 方法时按当前 (厂商, 账号) 订阅；不可见即退订。订阅覆盖期间轮询降为低频兜底。
        let provider_disabled = |state: &State, agent: Option<&String>| {
            agent.is_some_and(|agent| state.usage.disabled_providers.contains(agent))
        };
        let subscription_wanted = accounts_page_visible
            && self.observability.usage.enabled
            && !provider_disabled(
                &self.observability,
                self.observability.selected_provider.as_ref(),
            )
            && self.active_endpoint_advertises("account.usage.subscribe")
            && self.active_endpoint_advertises("account.usage.unsubscribe");
        let subscribed = self.sync_usage_subscription(subscription_wanted, now, outcome);
        // 页面与悬浮层各有自己的轮询时刻：悬浮层出现 / 点「刷新」只唤醒悬浮层，
        // 页面的 2 秒节流不受影响；反之亦然。
        let page_due =
            (accounts_page_visible || settings_open) && now >= self.observability.next_usage;
        let hover_due = hover_visible && now >= self.observability.hover_scope.next_usage;
        if self.observability.usage.enabled && (page_due || hover_due) {
            // 厂商列表：页面打开时立即、之后每 5 分钟低频重拉（OBS-14）；失败按
            // `PROVIDERS_RETRY` 退避。节流不看列表是否为空——否则列表拿不到时每
            // tick 都重发一次。
            if now >= self.observability.next_providers
                && self.observation_request(
                    Method::AccountUsageProviders(EmptyParams::default()),
                    Purpose::Providers,
                    outcome,
                ) == RequestOutcome::Sent
            {
                self.observability.next_providers = now + Duration::from_secs(300);
                self.observability.providers_sent_at = Some(now);
            }
            // 轮询节奏：强意图被挡下 200 ms 重试；订阅覆盖时只留低频兜底；否则 2 秒。
            // 服务端报告探测在途时的 500 ms 收紧在响应到达处按发送时刻计算
            // （`receive_observation`），旧 server 不带刷新状态即恒为 2 秒。
            let cadence = |retry_soon: bool, subscribed: bool| {
                now + if retry_soon {
                    Duration::from_millis(200)
                } else if subscribed {
                    SUBSCRIBED_POLL
                } else {
                    IDLE_POLL
                }
            };
            if page_due {
                let mut retry_soon = false;
                match self.usage_targets(false, now) {
                    targets @ (UsageTargets::Disabled | UsageTargets::NoProviders) => {
                        // 不发请求；排队中的强意图刷新必须消费掉，否则页面永远停在
                        // 「刷新中…」且「刷新」按钮失去命中区。两种空集分开说明：本机
                        // 关闭才指向设置页，没有已列出厂商时说明此主机没装 agent CLI。
                        if std::mem::take(&mut self.observability.refresh_usage) {
                            self.observability.manual_sent.clear();
                            self.observability.message = Some(
                                if targets == UsageTargets::NoProviders {
                                    tr(
                                        "No installed agent CLI or configured account detected on this host.",
                                        "此主机未检测到已安装的 agent CLI，也没有配置账号。",
                                    )
                                } else if self.observability.selected_provider.is_some() {
                                    tr(
                                        "This provider is disabled in settings.",
                                        "此厂商已在设置中关闭，可在设置页重新启用。",
                                    )
                                } else {
                                    tr(
                                        "All providers are disabled in settings.",
                                        "所有厂商已在设置中关闭，可在设置页重新启用。",
                                    )
                                }
                                .into(),
                            );
                        }
                    }
                    // 厂商列表刚发出：保留强意图，列表到达（或失败）即唤醒；不压到
                    // 200 ms 忙等，列表丢失时下一轮按 `usage_targets` 回落整体请求。
                    UsageTargets::AwaitProviders => {}
                    UsageTargets::Agents(agents) => {
                        let manual = self.observability.refresh_usage;
                        let mut fan_out =
                            self.scope_usage_requests(false, manual, &agents, outcome);
                        if manual && fan_out.unavailable {
                            // 端点未宣告 account.usage.refresh（或此刻不可用）：只禁用
                            // 手动刷新，同一轮回落到普通 get，轮询不停摆。
                            self.observability.refresh_usage = false;
                            self.observability.manual_sent.clear();
                            fan_out = self.scope_usage_requests(false, false, &agents, outcome);
                        }
                        if fan_out.sent {
                            self.observability.usage_sent_at = Some(now);
                        }
                        if self.observability.refresh_usage {
                            if fan_out.busy {
                                // 一部分厂商被在途请求挡下：强意图不能丢（否则它们只拿到
                                // 吃 300 s 缓存的 get），保留并 200 ms 后只补发它们。
                                retry_soon = true;
                            } else {
                                // 每个目标厂商的 refresh 都已发出：消费强意图，进入在途，
                                // 全部响应到齐才结束「刷新中」（都已到齐则立即结束）。
                                self.observability.refresh_usage = false;
                                self.observability.manual_sent.clear();
                                self.observability.manual_in_flight =
                                    self.observability.usage_in_flight(false);
                            }
                        }
                    }
                }
                self.observability.next_usage = cadence(retry_soon, subscribed);
            }
            if hover_due {
                let mut retry_soon = false;
                match self.usage_targets(true, now) {
                    UsageTargets::Disabled | UsageTargets::NoProviders => {
                        self.observability.hover_scope.refresh = false;
                        self.observability.hover_scope.manual_sent.clear();
                    }
                    UsageTargets::AwaitProviders => {}
                    UsageTargets::Agents(agents) => {
                        let manual = self.observability.hover_scope.refresh;
                        let mut fan_out = self.scope_usage_requests(true, manual, &agents, outcome);
                        if manual && fan_out.unavailable {
                            self.observability.hover_scope.refresh = false;
                            self.observability.hover_scope.manual_sent.clear();
                            fan_out = self.scope_usage_requests(true, false, &agents, outcome);
                        }
                        if fan_out.sent {
                            self.observability.hover_scope.sent_at = Some(now);
                        }
                        if self.observability.hover_scope.refresh {
                            if fan_out.busy {
                                retry_soon = true;
                            } else {
                                self.observability.hover_scope.refresh = false;
                                self.observability.hover_scope.manual_sent.clear();
                                self.observability.hover_scope.manual_in_flight =
                                    self.observability.usage_in_flight(true);
                            }
                        }
                    }
                }
                self.observability.hover_scope.next_usage = cadence(retry_soon, false);
            }
        }
        if let Some(body) = self.observability.alert() {
            outcome.repaint |= self.push_endpoint_notice(
                ClientEndpointNoticeKind::Rejected,
                format!("monitor-{}", self.observability.now_ms),
                tr("Resource alert", "资源告警"),
                body,
            );
        }
    }

    /// 活动端点应答的观测响应（测试便捷入口）；真实路径见 `receive_observation_from`。
    #[cfg(test)]
    pub(super) fn receive_observation(
        &mut self,
        epoch: u64,
        purpose: Purpose,
        result: Result<ResponseResult, ClientShellEndpointError>,
    ) -> bool {
        let endpoint_id = self.active_endpoint_id.clone();
        self.receive_observation_from(&endpoint_id, epoch, purpose, result)
    }

    /// `endpoint_id` 应答的观测响应（`handle_endpoint_result` 已按 boot 校验）。
    /// 订阅生命周期与排队绑定需要知道应答的端点：订阅 id / pane id 只在它上面有意义。
    pub(super) fn receive_observation_from(
        &mut self,
        endpoint_id: &ClientEndpointId,
        epoch: u64,
        purpose: Purpose,
        result: Result<ResponseResult, ClientShellEndpointError>,
    ) -> bool {
        // 悬浮层请求按悬浮层作用域的代际判旧，页面请求按页面代际，订阅生命周期
        // 不绑代际（由 tick 按参数与 boot 对账）。
        if epoch != self.observability.epoch_for(&purpose) {
            return false;
        }
        self.observability.settle_pending(&purpose);
        // 该作用域最后一条在途用量响应（成功或失败）到达即结束在途手动刷新；逐厂商
        // 请求要等全部厂商到齐。排队中的下一次强意图（`refresh_usage` /
        // `hover_scope.refresh`）不受影响，仍显示刷新中。
        match purpose {
            Purpose::Usage { .. } if !self.observability.usage_in_flight(false) => {
                self.observability.manual_in_flight = false;
            }
            Purpose::HoverUsage { .. } if !self.observability.usage_in_flight(true) => {
                self.observability.hover_scope.manual_in_flight = false;
            }
            _ => {}
        }
        match result {
            Ok(ResponseResult::SystemMetrics { snapshot }) => {
                self.observability.apply_metrics(snapshot)
            }
            Ok(ResponseResult::AccountUsageProviders { providers }) => {
                self.observability.providers = providers;
                self.auto_select_usage_provider();
                // 总览态等着这份列表才能逐厂商发请求：立刻唤醒两个作用域的轮询。
                let now = Instant::now();
                self.observability.next_usage = self.observability.next_usage.min(now);
                self.observability.hover_scope.next_usage =
                    self.observability.hover_scope.next_usage.min(now);
            }
            Ok(ResponseResult::AccountUsage { accounts, refresh }) => {
                // 响应只写自己的作用域，且不反写用户选择的厂商；按请求带的厂商合并
                // （逐厂商请求只替换该厂商，整体请求整体替换；get / refresh 响应是
                // 权威快照，旧 server 不带刷新状态即为空）。服务端说探测在途时把该
                // 作用域的下一次轮询收紧到 500 ms（页面已由订阅覆盖时靠事件收尾，
                // 不提前轮询）。
                let refresh = refresh.unwrap_or_default();
                let in_flight = refresh.iter().any(|state| state.in_flight);
                let (hover, agent) = match &purpose {
                    Purpose::HoverUsage { agent } => (true, agent.as_deref()),
                    Purpose::Usage { agent } => (false, agent.as_deref()),
                    _ => (false, None),
                };
                self.observability
                    .merge_usage_response(hover, agent, accounts, refresh);
                if !in_flight {
                    // 无在途探测：不收紧轮询。
                } else if hover {
                    let scope = &mut self.observability.hover_scope;
                    let soon = scope.sent_at.unwrap_or_else(Instant::now) + IN_FLIGHT_POLL;
                    scope.next_usage = scope.next_usage.min(soon);
                } else if !self.observability.subscribed() {
                    let soon = self
                        .observability
                        .usage_sent_at
                        .unwrap_or_else(Instant::now)
                        + IN_FLIGHT_POLL;
                    self.observability.next_usage = self.observability.next_usage.min(soon);
                }
            }
            Ok(ResponseResult::ObservationSubscription {
                subscription_id,
                active,
            }) => match purpose {
                Purpose::Subscribe => {
                    let boot_id = self
                        .endpoint_boot_id(endpoint_id)
                        .map(str::to_owned)
                        .unwrap_or_default();
                    let subscription = &mut self.observability.subscription;
                    match subscription.requested.take() {
                        Some((params, owner)) if active && &owner == endpoint_id => {
                            subscription.active = Some(ActiveSubscription {
                                id: subscription_id,
                                params,
                                endpoint: owner,
                                boot_id,
                            });
                        }
                        // 已不再等这份订阅（页面已隐藏 / 端点或 boot 变化后 detach）：
                        // 服务端仍持有它，排队退订（发回应答的端点），免得它持续驱动
                        // 后台探测。
                        _ if active => subscription.retire.push_back(RetiringSubscription {
                            id: subscription_id,
                            endpoint: endpoint_id.clone(),
                            boot_id,
                            attempts: 0,
                        }),
                        _ => {
                            subscription.retry_at = Some(Instant::now() + SUBSCRIBE_RETRY);
                            self.observability.next_usage = Instant::now();
                        }
                    }
                }
                // 只有明确的 `active:false` 才把 id 移出退订队列（服务端对未知 id
                // 也回 active:false，语义同样是「已不存在」）。
                Purpose::Unsubscribe if !active => {
                    let subscription = &mut self.observability.subscription;
                    subscription
                        .retire
                        .retain(|entry| entry.id != subscription_id);
                    subscription.unsubscribe_retry_at = None;
                }
                _ => {}
            },
            Ok(ResponseResult::AccountBinding {
                account_id,
                pane_id,
            }) => {
                self.observability.message = Some(if zh() {
                    format!("已将 {pane_id} 绑定到 {account_id}。")
                } else {
                    format!("Bound {pane_id} to {account_id}.")
                });
                self.observability.clear_pending_binding(&pane_id);
                if matches!(purpose, Purpose::HoverBinding) {
                    // 悬浮层发起的绑定只刷新悬浮层：页面的厂商 / 账号选择不动，
                    // 否则会出现「页面厂商 claude + 账号 codex:default」这种服务端
                    // 双重过滤后必空的组合。
                    if self
                        .observability
                        .hover
                        .as_ref()
                        .is_some_and(|hover| hover.visible)
                    {
                        self.observability.hover_scope.request_refresh();
                    }
                } else {
                    // 页面发起的绑定回流 = 页面强意图刷新；被绑定账号所属厂商与
                    // 当前选中不一致时先切到该厂商，保证 agent / account 一致。
                    let owner = self
                        .observability
                        .providers
                        .iter()
                        .find(|provider| provider.configured_accounts.contains(&account_id))
                        .map(|provider| provider.agent.clone());
                    if let Some(agent) = owner.filter(|agent| {
                        Some(agent) != self.observability.selected_provider.as_ref()
                    }) {
                        self.select_usage_provider(agent);
                    }
                    self.observability.selected_account = Some(account_id);
                    self.observability.request_refresh();
                }
            }
            Ok(ResponseResult::SystemProcess { process }) => {
                self.observability.process_dialog = Some(ProcessDialog {
                    process,
                    force: false,
                    confirm: false,
                    pending: false,
                });
            }
            Ok(ResponseResult::SystemProcessTerminated { .. }) => {
                self.observability.process_dialog = None;
                self.observability.message = Some(
                    tr(
                        "Termination sent. Waiting for the next sample.",
                        "已发送结束请求，等待下一次采样。",
                    )
                    .into(),
                );
            }
            Ok(ResponseResult::Ok {})
                if matches!(
                    purpose,
                    Purpose::Integration { .. } | Purpose::HoverIntegration { .. }
                ) =>
            {
                // 启用成功后顺手把当前活动 pane 绑到该账号（零 wire 变更）：
                // 响应处理期不能发请求，排队到下一次 tick。
                let hover = matches!(purpose, Purpose::HoverIntegration { .. });
                let bind_after = match purpose {
                    Purpose::Integration { bind_after } => {
                        bind_after.map(|(pane_id, account_id)| QueuedBinding {
                            pane_id,
                            account_id,
                            hover: false,
                            endpoint: endpoint_id.clone(),
                        })
                    }
                    Purpose::HoverIntegration { bind_after } => {
                        bind_after.map(|(pane_id, account_id)| QueuedBinding {
                            pane_id,
                            account_id,
                            hover: true,
                            endpoint: endpoint_id.clone(),
                        })
                    }
                    _ => None,
                };
                self.observability.message = Some(
                    if bind_after.is_some() {
                        tr(
                            "Official statusline integration updated; binding the active pane to this account.",
                            "官方状态栏回调已更新，正在把当前 pane 绑定到该账号。",
                        )
                    } else {
                        tr(
                            "Official statusline integration updated. The CLI publishes data on its next status update.",
                            "官方状态栏回调已更新，等待 CLI 下一次状态更新。",
                        )
                    }
                    .into(),
                );
                // 开关的显示态来自作用域账号的 `UsageRefreshState.callback_enabled`：服务端已
                // 改写 settings.json，让该作用域下一个 tick 立即重拉用量（响应处理期不能发
                // 请求），否则开关最长要等到下一轮轮询才翻转、再点一次会重发同一个值。
                if hover {
                    self.observability.hover_scope.next_usage = Instant::now();
                } else {
                    self.observability.next_usage = Instant::now();
                }
                if bind_after.is_some() {
                    self.observability.queued_binding = bind_after;
                }
            }
            Ok(_) => {}
            Err(error) if matches!(purpose, Purpose::Subscribe | Purpose::Unsubscribe) => {
                // 订阅是可选增强：被拒（配额、旧 server）只回退到自适应轮询并稍后重试，
                // 不在页脚报错。
                tracing::debug!(
                    subsystem = "client_shell",
                    purpose = %purpose.key(),
                    code = error.code.as_deref().unwrap_or("unknown"),
                    "用量订阅请求失败，回退到轮询"
                );
                let subscription = &mut self.observability.subscription;
                if matches!(purpose, Purpose::Subscribe) {
                    subscription.requested = None;
                    subscription.retry_at = Some(Instant::now() + SUBSCRIBE_RETRY);
                    self.observability.next_usage = Instant::now();
                } else {
                    // 退订失败（超时 / 传输错误）：id 留在队首，退避后重试，有上界；
                    // 否则服务端订阅者继续存活并驱动后台探测。
                    let exhausted = subscription.retire.front_mut().is_some_and(|entry| {
                        entry.attempts = entry.attempts.saturating_add(1);
                        entry.attempts >= MAX_UNSUBSCRIBE_ATTEMPTS
                    });
                    if exhausted {
                        subscription.retire.pop_front();
                        subscription.unsubscribe_retry_at = None;
                    } else {
                        subscription.unsubscribe_retry_at =
                            Some(Instant::now() + UNSUBSCRIBE_RETRY);
                    }
                }
            }
            Err(error) => {
                if matches!(purpose, Purpose::Providers) {
                    // 列表拿不到（旧 server 的 server_context_required 等）：退避后再
                    // 拉，期间总览回落整体请求；立刻唤醒两个作用域，不等下一轮。
                    let now = Instant::now();
                    self.observability.next_providers = now + PROVIDERS_RETRY;
                    self.observability.next_usage = self.observability.next_usage.min(now);
                    self.observability.hover_scope.next_usage =
                        self.observability.hover_scope.next_usage.min(now);
                }
                self.observability.message = Some(error.message);
                if let Some(dialog) = &mut self.observability.process_dialog {
                    dialog.pending = false;
                    dialog.confirm = false;
                }
            }
        }
        self.observation_surface_visible()
    }

    /// 活动端点是否宣告了某个 API 方法（未知 = 未宣告：订阅是可选增强，缺失只回退轮询）。
    fn active_endpoint_advertises(&self, method: &str) -> bool {
        self.endpoints
            .iter()
            .find(|endpoint| endpoint.endpoint_id == self.active_endpoint_id)
            .and_then(|endpoint| endpoint.methods.as_ref())
            .is_some_and(|methods| methods.contains(method))
    }

    /// 对账页面作用域的订阅：`wanted` 时按当前 (厂商, 账号) 订阅，作用域变化先退旧
    /// 订阅再订新的，不 `wanted` 即退订。返回页面此刻是否已由订阅覆盖（含等待确认），
    /// 覆盖期间轮询降为低频兜底；被拒 / 不可用时退避 `SUBSCRIBE_RETRY` 后再试。
    ///
    /// 退订定向发往创建订阅的端点，收到 `active:false` 才出队；失败有界重试；
    /// 端点离线 / boot 变化 / 未宣告方法时服务端已随连接释放，直接放弃。
    fn sync_usage_subscription(
        &mut self,
        wanted: bool,
        now: Instant,
        outcome: &mut ClientShellInput,
    ) -> bool {
        // 现有订阅不再匹配（页面隐藏 / 作用域变化）：进入退订队列。每 tick 都走到
        // 这里，判定只借用、不分配。
        let stale = {
            let state = &self.observability;
            state.subscription.active.as_ref().is_some_and(|active| {
                !wanted
                    || !UsageSubscription::covers(
                        &active.params,
                        state.selected_provider.as_deref(),
                        state.selected_account.as_deref(),
                    )
            })
        };
        if stale {
            self.observability.subscription.retire_active();
        }
        let retry_due = self
            .observability
            .subscription
            .unsubscribe_retry_at
            .is_none_or(|at| now >= at);
        let front = self
            .observability
            .subscription
            .retire
            .front()
            .filter(|_| retry_due)
            .map(|entry| {
                (
                    entry.id.clone(),
                    entry.endpoint.clone(),
                    entry.boot_id.clone(),
                )
            });
        if let Some((subscription_id, endpoint, boot_id)) = front {
            if self.endpoint_boot_id(&endpoint) != Some(boot_id.as_str()) {
                // server 已重启 / 端点已无快照：订阅随旧连接消亡，无需退订。
                self.observability.subscription.retire.pop_front();
            } else {
                let params = ObservationSubscriptionParams {
                    subscription_id: Some(subscription_id),
                    interval_ms: None,
                };
                match self.observation_request_at(
                    Some(endpoint),
                    Method::AccountUsageUnsubscribe(params),
                    Purpose::Unsubscribe,
                    outcome,
                ) {
                    // 发出后留在队首等确认；在途时不重复发。
                    RequestOutcome::Sent | RequestOutcome::Busy => {}
                    // 端点离线 / 未宣告退订：服务端会在连接断开时释放，放弃。
                    RequestOutcome::Unavailable => {
                        self.observability.subscription.retire.pop_front();
                    }
                }
            }
        }
        if !wanted {
            return false;
        }
        if self.observability.subscribed() {
            return true;
        }
        let subscription = &self.observability.subscription;
        if subscription.requested.is_some() || subscription.retry_at.is_some_and(|at| now < at) {
            return false;
        }
        let params = UsageParams {
            agent: self.observability.selected_provider.clone(),
            account_id: self.observability.selected_account.clone(),
            pane_id: None,
        };
        let endpoint = self.active_endpoint_id.clone();
        match self.observation_request_at(
            Some(endpoint.clone()),
            Method::AccountUsageSubscribe(params.clone()),
            Purpose::Subscribe,
            outcome,
        ) {
            RequestOutcome::Sent => {
                self.observability.subscription.requested = Some((params, endpoint));
                true
            }
            RequestOutcome::Busy => false,
            RequestOutcome::Unavailable => {
                self.observability.subscription.retry_at = Some(now + SUBSCRIBE_RETRY);
                false
            }
        }
    }

    /// 端点推来的 `endpoint.observation.v1` 事件：只接受页面作用域所在 (端点, boot)
    /// 的推送，按 `account_id` 合并到页面账号与刷新状态；悬浮层不订阅，不受影响。
    /// 返回呈现面是否可见（可见时下一次 tick 重绘）。
    pub(crate) fn receive_endpoint_observation_event(
        &mut self,
        endpoint_id: &ClientEndpointId,
        generation: u64,
        frame: crate::protocol::endpoint::EndpointObservationEvent,
    ) -> bool {
        let boot_matches = self.endpoints.iter().any(|endpoint| {
            &endpoint.endpoint_id == endpoint_id
                && endpoint
                    .snapshot_generation
                    .is_none_or(|snapshot_generation| snapshot_generation == generation)
                && endpoint
                    .snapshot
                    .as_deref()
                    .is_some_and(|snapshot| snapshot.boot_id == frame.boot_id)
        });
        let source_matches = self
            .observability
            .usage_source
            .as_ref()
            .is_some_and(|(source, boot_id)| source == endpoint_id && *boot_id == frame.boot_id);
        if !boot_matches || !source_matches {
            return false;
        }
        match frame.event {
            ObservationEventEnvelope::AccountUsageUpdated(event) => {
                let mut refresh = event.refresh.unwrap_or_default();
                for account in event.accounts {
                    if !self.observability.page_scope_accepts(&account) {
                        continue;
                    }
                    let state = refresh
                        .iter()
                        .position(|state| state.account_id == account.account_id)
                        .map(|index| refresh.swap_remove(index));
                    self.observability.merge_page_account(account, state);
                }
            }
            ObservationEventEnvelope::AccountUsageRefreshing(event) => {
                for state in event.refresh {
                    // 与 updated 事件同一套作用域判定：只认解析得出厂商且属于当前
                    // (厂商, 账号) 选择的账号，挡住切换作用域后旧订阅的尾巴。
                    if self.observability.page_scope_accepts_id(&state.account_id) {
                        self.observability.merge_refresh_state(false, state);
                    }
                }
            }
            ObservationEventEnvelope::SystemMetricsUpdated(event) => {
                self.observability.apply_metrics(event.snapshot);
            }
        }
        let visible = self.observation_surface_visible();
        self.observability.event_repaint |= visible;
        visible
    }

    /// 活动端点快照里正在运行 agent 的 pane（可绑定候选），`provider` 有值时只要
    /// 运行该厂商 agent 的 pane。
    fn agent_panes<'a>(
        &'a self,
        provider: Option<&'a str>,
    ) -> impl Iterator<Item = &'a crate::protocol::ClientShellAgent> + 'a {
        self.snapshot
            .as_deref()
            .into_iter()
            .flat_map(|snapshot| snapshot.agents.iter())
            .filter(move |agent| {
                agent.agent.as_deref().is_some_and(|name| {
                    is_bindable_agent(name)
                        && provider.is_none_or(|provider| name.eq_ignore_ascii_case(provider))
                })
            })
    }

    /// 某端点快照里 `pane_id` 正在运行的（可绑定）agent 名；pane 不存在或没有
    /// 运行 agent 时为 `None`。
    fn endpoint_pane_agent(&self, endpoint_id: &ClientEndpointId, pane_id: &str) -> Option<String> {
        self.endpoints
            .iter()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)?
            .snapshot
            .as_deref()?
            .agents
            .iter()
            .find(|agent| agent.pane_id == pane_id)?
            .agent
            .clone()
            .filter(|name| is_bindable_agent(name))
    }

    /// 绑定行里 pane 的显示名：agent 显示名 · pane id。
    fn bind_candidate_label(agent: &crate::protocol::ClientShellAgent) -> String {
        let name = agent
            .name
            .as_deref()
            .or(agent.display_agent.as_deref())
            .or(agent.agent.as_deref())
            .unwrap_or("agent");
        format!("{name} · {}", agent.pane_id)
    }

    /// 账号所属厂商：先看厂商列表的已配置账号，再看作用域内的快照，最后看该
    /// 作用域里服务端给出的待办绑定候选；都解析不出即 `None`。
    fn account_agent(&self, account_id: &str, hover: bool) -> Option<String> {
        let (accounts, refresh_states) = if hover {
            (
                &self.observability.hover_scope.accounts,
                &self.observability.hover_scope.refresh_states,
            )
        } else {
            (
                &self.observability.accounts,
                &self.observability.refresh_states,
            )
        };
        self.observability
            .providers
            .iter()
            .find(|provider| {
                provider
                    .configured_accounts
                    .iter()
                    .any(|id| id == account_id)
            })
            .map(|provider| provider.agent.clone())
            .or_else(|| {
                accounts
                    .iter()
                    .find(|account| account.account_id == account_id)
                    .map(|account| account.agent.clone())
            })
            .or_else(|| {
                refresh_states
                    .iter()
                    .filter_map(|state| state.pending_binding.as_ref())
                    .find(|pending| pending.candidates.iter().any(|id| id == account_id))
                    .map(|pending| pending.agent.clone())
            })
    }

    /// 当前聚焦的 pane（停靠工作台下焦点在面板时回退到快照记录的聚焦 pane），且它
    /// 正在运行 `account_id` 所属厂商的 agent；厂商解析不出（端点 / boot 变化后的
    /// 空窗）视为没有候选，否则 `None`。
    fn focused_pane_for_account(
        &self,
        account_id: &str,
    ) -> Option<&crate::protocol::ClientShellAgent> {
        let focused = self
            .focused_pane_id()
            .or_else(|| self.snapshot.as_deref()?.focused_pane_id.clone())?;
        let agent = self.account_agent(account_id, false)?;
        self.agent_panes(None).find(|pane| {
            pane.pane_id == focused
                && pane
                    .agent
                    .as_deref()
                    .is_some_and(|name| name.eq_ignore_ascii_case(&agent))
        })
    }

    /// 页面作用域「启用官方回调」成功后顺手绑定的目标 pane：已选中的 pane，否则
    /// 运行该账号厂商 agent 的聚焦 pane。
    fn page_binding_target(&self, account_id: &str) -> Option<String> {
        self.observability.selected_pane.clone().or_else(|| {
            self.focused_pane_for_account(account_id)
                .map(|pane| pane.pane_id.clone())
        })
    }

    /// 某端点的连接断开（`mark_endpoint_disconnected`）：服务端随连接释放了该端点
    /// 上的订阅，本地订阅生命周期不能停在「已订阅」——否则同 boot 重连后既收不到
    /// 推送也只剩 30 s 兜底轮询。相应的在途订阅 / 退订键、退避与排队绑定一并
    /// 清掉；断开的是活动端点时把页面轮询提前到重连后的第一个 tick（冷启动 get +
    /// 重新订阅）。
    pub(crate) fn observation_endpoint_disconnected(&mut self, endpoint_id: &ClientEndpointId) {
        let (subscribe_in_flight, unsubscribe_in_flight) = self
            .observability
            .subscription
            .endpoint_disconnected(endpoint_id);
        if subscribe_in_flight {
            self.observability.pending.remove("subscribe");
        }
        if unsubscribe_in_flight {
            self.observability.pending.remove("unsubscribe");
        }
        if self
            .observability
            .queued_binding
            .as_ref()
            .is_some_and(|binding| &binding.endpoint == endpoint_id)
        {
            self.observability.queued_binding = None;
        }
        if endpoint_id == &self.active_endpoint_id {
            self.observability.subscription.retry_at = None;
            self.observability.next_usage = Instant::now();
        }
    }

    /// 监控 / 账号数据当前是否有可见的呈现面：页面、悬浮层、停靠的监控或
    /// legacy 账号面板。新响应到达时据此立即重绘，每秒 tick
    /// 据此刷新时间显示；与 `usage_visible` 的面板判据保持同一套。
    fn observation_surface_visible(&self) -> bool {
        self.observability.page.is_some()
            || self.observability.hover.is_some()
            || self.workbench.visible(&dock::PanelId::Monitor)
            || self.workbench.visible(&dock::PanelId::Accounts)
    }

    pub(super) fn observation_action(&mut self, action: Action, outcome: &mut ClientShellInput) {
        self.observation_action_in(action, false, outcome);
    }

    /// `from_hover` 标记动作来自悬浮层的命中区：读取账号列表的动作（绑定、官方
    /// 回调、官方来源、刷新、切换账号候选）按悬浮层作用域取数；选择类动作
    /// （选厂商 / 选账号 / 打开页面）本就是用户对页面的显式操作，直接落页面。
    fn observation_action_in(
        &mut self,
        action: Action,
        from_hover: bool,
        outcome: &mut ClientShellInput,
    ) {
        // 偏好按键上锁：动作只回写它自己触到的那一个键（见 `PreferenceKey`）。
        let preference = PreferenceKey::for_action(&action);
        match action {
            Action::CycleAccount => {
                // 候选来自厂商的已配置账号（总览态为全部已列出厂商）；不足两个
                // 时是禁用态：不发请求，只在页脚说明。
                let provider = if from_hover {
                    self.observability.hover_scope.provider.clone()
                } else {
                    self.observability.selected_provider.clone()
                };
                let candidates = self
                    .observability
                    .cycle_candidates(provider.as_deref())
                    .map(str::to_owned)
                    .collect::<Vec<_>>();
                if candidates.len() <= 1 {
                    self.observability.message =
                        Some(tr("No other account to switch to.", "没有其它账号可切换。").into());
                } else {
                    if from_hover && provider != self.observability.selected_provider {
                        if let Some(agent) = provider {
                            self.select_usage_provider(agent);
                        }
                    }
                    let current = candidates
                        .iter()
                        .position(|id| Some(id) == self.observability.selected_account.as_ref());
                    let next = candidates
                        [current.map_or(0, |index| (index + 1) % candidates.len())]
                    .clone();
                    self.observation_action(Action::Account(next), outcome);
                }
            }
            Action::UsageIntegration(enabled) => {
                let Some(account_id) = self.observability.scoped_account(from_hover) else {
                    // 多账号且未选：开关没有作用对象。渲染侧已是禁用态，这里兜底不静默。
                    self.observability.message =
                        Some(crate::i18n::texts().monitor.select_account_first.to_owned());
                    outcome.repaint = true;
                    return;
                };
                // 启用成功后顺手绑定的 pane：悬浮层是 hover 的 pane，页面是已选中的
                // pane 或运行该厂商 agent 的聚焦 pane；移除回调不绑定。
                let target = if !enabled {
                    None
                } else if from_hover {
                    self.observability.hover_scope.pane.clone()
                } else {
                    self.page_binding_target(&account_id)
                };
                let bind_after = target.map(|pane_id| (pane_id, account_id.clone()));
                // 悬浮层发起的请求发往 hover 的端点（可能是远端主机）。
                let purpose = if from_hover {
                    Purpose::HoverIntegration { bind_after }
                } else {
                    Purpose::Integration { bind_after }
                };
                self.observation_request(
                    Method::AccountUsageIntegration(UsageIntegrationParams {
                        account_id,
                        enabled,
                    }),
                    purpose,
                    outcome,
                );
            }
            Action::Card(card) => self.observability.selected_card = Some(card),
            Action::Core(core) => {
                self.observability.selected_core = if self.observability.selected_core == Some(core)
                {
                    None
                } else {
                    Some(core)
                }
            }
            Action::CardMove(card, delta) => {
                if let Some(index) = self
                    .observability
                    .monitor
                    .visible
                    .iter()
                    .position(|id| id == &card)
                {
                    let next = index
                        .saturating_add_signed(delta)
                        .min(self.observability.monitor.visible.len().saturating_sub(1));
                    self.observability.monitor.visible.swap(index, next);
                }
            }
            Action::CardSize(delta) => {
                let monitor = &mut self.observability.monitor;
                monitor.card_height = step_ladder(&CARD_HEIGHT_STEPS, monitor.card_height, delta);
            }
            Action::HistoryRange(delta) => {
                let monitor = &mut self.observability.monitor;
                monitor.history_minutes =
                    step_ladder(&HISTORY_STEPS, monitor.history_minutes, delta);
            }
            Action::ChartGlyphs(glyphs) => self.observability.chart_glyphs = glyphs,
            Action::Device(id) => {
                let hidden = &mut self.observability.monitor.hidden_devices;
                if hidden.contains(&id) {
                    hidden.retain(|value| value != &id);
                } else {
                    hidden.push(id);
                }
            }
            Action::ProviderEnabled(agent) => {
                let disabled = &mut self.observability.usage.disabled_providers;
                if disabled.contains(&agent) {
                    disabled.retain(|value| value != &agent);
                } else {
                    disabled.push(agent);
                }
                self.observability.apply_disabled_providers();
            }
            Action::RestoreUsagePreferences => {
                // 置 None 后按 config.toml 重载：效果与删掉偏好文件里这几个键一致。
                self.config.preferences.clear_usage_overrides();
                self.observability.reload_preferences(&self.config);
            }
            Action::AlertThreshold(index, delta) => {
                // 50–100% 每步 5 个点，越过两端回绕。
                if let Some(rule) = self.observability.monitor.alerts.get_mut(index) {
                    rule.threshold = if delta < 0 {
                        if rule.threshold <= 50.0 {
                            100.0
                        } else {
                            rule.threshold - 5.0
                        }
                    } else if rule.threshold >= 100.0 {
                        50.0
                    } else {
                        rule.threshold + 5.0
                    };
                }
            }
            Action::AlertDuration(index, delta) => {
                if let Some(rule) = self.observability.monitor.alerts.get_mut(index) {
                    rule.duration_seconds =
                        step_ladder(&ALERT_DURATION_STEPS, rule.duration_seconds, delta);
                }
            }
            Action::AlertCooldown(index, delta) => {
                if let Some(rule) = self.observability.monitor.alerts.get_mut(index) {
                    rule.cooldown_seconds =
                        step_ladder(&ALERT_COOLDOWN_STEPS, rule.cooldown_seconds, delta);
                }
            }
            Action::Page(page) => self.open_observation_page(page, outcome),
            Action::EditLayout => {
                self.observability.layout_editing = !self.observability.layout_editing;
            }
            Action::Close => {
                self.observability.clear_hover();
                self.observability.process_dialog = None;
                self.observability.layout_editing = false;
                if self.workbench.enabled {
                    // 关闭的是承载页面的面板（而非当前聚焦面板）：终端聚焦时
                    // 也能从命令面板关掉监控面板；锁定布局时给出反馈。
                    let panel = if self.workbench.dock.focused == dock::PanelId::Accounts {
                        dock::PanelId::Accounts
                    } else {
                        dock::PanelId::Monitor
                    };
                    self.close_workbench_panel(panel, outcome);
                } else {
                    self.observability.page = None;
                }
            }
            Action::Configure => self.open_observation_page(Page::Settings, outcome),
            Action::Pause => self.observability.paused = !self.observability.paused,
            Action::Interval(delta) => {
                let monitor = &mut self.observability.monitor;
                monitor.interval_ms = step_ladder(&INTERVAL_STEPS, monitor.interval_ms, delta);
                self.observability.next_metrics = Instant::now();
            }
            Action::Metric(metric) => {
                let items = &mut self.observability.monitor.visible;
                if items.contains(&metric) {
                    items.retain(|item| item != &metric);
                } else {
                    items.push(metric);
                }
            }
            Action::ToggleAlerts => {
                self.observability.monitor.alerts_enabled =
                    !self.observability.monitor.alerts_enabled
            }
            Action::UsageEnabled => {
                self.observability.usage.enabled = !self.observability.usage.enabled
            }
            Action::UsageFormat(format) => {
                self.observability.account_scroll = 0;
                self.observability.usage.format = format;
            }
            Action::UsagePosition(position) => {
                self.observability.usage.position = position;
                // 切到「页面」= agent 行悬浮层关闭：页脚说明一次，设置行也常驻提示。
                if position == UsageDisplayPosition::Page {
                    self.observability.message =
                        Some(crate::i18n::texts().monitor.hover_closed_hint.to_owned());
                }
            }
            Action::HoverDelay(delta) => {
                let usage = &mut self.observability.usage;
                usage.hover_delay_ms = step_ladder(&HOVER_DELAY_STEPS, usage.hover_delay_ms, delta);
            }
            Action::Overview => self.select_usage_overview(),
            Action::Provider(agent) => self.select_usage_provider(agent),
            Action::Account(account) => {
                // 显式选账号 = 强意图刷新；旧快照保留到新数据到达（变暗）。
                // 不清 selected_pane：它是页面绑定入口的前提。
                self.observability.account_scroll = 0;
                self.observability.bump_page_epoch();
                self.observability.selected_account = Some(account);
                self.observability.request_refresh();
            }
            Action::Bind => {
                // 页面分支的 pane 来自 pane 选择器 /「绑定到聚焦 pane」；悬浮层分支
                // 是 hover 的 pane。缺 pane / 账号时写 message 而不是静默。
                let pane_id = if from_hover {
                    self.observability.hover_scope.pane.clone()
                } else {
                    self.observability.selected_pane.clone()
                };
                let endpoint = if from_hover {
                    self.observability.hover_scope.endpoint.clone()
                } else {
                    Some(self.active_endpoint_id.clone())
                };
                match (
                    pane_id.zip(endpoint),
                    self.observability.scoped_account(from_hover),
                ) {
                    (Some((pane_id, endpoint)), Some(account_id)) => {
                        self.bind_pane_to_account(
                            endpoint, pane_id, account_id, from_hover, outcome,
                        );
                    }
                    (None, _) => {
                        self.observability.message = Some(
                            tr(
                                "Pick a pane before binding: use the pane picker or \"Bind focused pane\".",
                                "请先选择要绑定的 pane：用 pane 选择器或「绑定到聚焦 pane」。",
                            )
                            .into(),
                        );
                    }
                    (Some(_), None) => {
                        self.observability.message = Some(
                            tr(
                                "Select the account to bind first.",
                                "请先选择要绑定到的账号。",
                            )
                            .into(),
                        );
                    }
                }
            }
            Action::BindFocused => {
                // 聚焦 pane 须运行所选账号厂商的 agent，否则给出指引而不是绑错。
                match self.observability.scoped_account(false) {
                    Some(account_id) => match self
                        .focused_pane_for_account(&account_id)
                        .map(|pane| (pane.pane_id.clone(), Self::bind_candidate_label(pane)))
                    {
                        Some((pane_id, label)) => {
                            self.observability.selected_pane = Some(pane_id.clone());
                            self.observability.selected_pane_label = Some(label);
                            let endpoint = self.active_endpoint_id.clone();
                            self.bind_pane_to_account(
                                endpoint, pane_id, account_id, false, outcome,
                            );
                        }
                        None => {
                            self.observability.message = Some(
                                tr(
                                    "The focused pane is not running this provider's agent; use the pane picker.",
                                    "当前聚焦的 pane 没有运行该厂商的 agent，请用 pane 选择器。",
                                )
                                .into(),
                            );
                        }
                    },
                    None => {
                        self.observability.message = Some(
                            tr(
                                "Select the account to bind first.",
                                "请先选择要绑定到的账号。",
                            )
                            .into(),
                        );
                    }
                }
            }
            Action::CyclePane(delta) => {
                // 候选按所选厂商过滤；总览态（未选厂商）按将要绑定到的账号所属
                // 厂商过滤，选择器本身不给出跨厂商候选。
                let provider = self.observability.selected_provider.clone().or_else(|| {
                    self.observability
                        .scoped_account(false)
                        .and_then(|account_id| self.account_agent(&account_id, false))
                });
                let candidates = self
                    .agent_panes(provider.as_deref())
                    .map(|pane| (pane.pane_id.clone(), Self::bind_candidate_label(pane)))
                    .collect::<Vec<_>>();
                if candidates.is_empty() {
                    self.observability.message = Some(
                        tr(
                            "No running agent pane to bind for this provider.",
                            "没有正在运行该厂商 agent 的 pane 可供绑定。",
                        )
                        .into(),
                    );
                } else {
                    let current = candidates.iter().position(|(pane_id, _)| {
                        Some(pane_id) == self.observability.selected_pane.as_ref()
                    });
                    let len = candidates.len() as isize;
                    let next = current.map_or(if delta < 0 { len - 1 } else { 0 }, |index| {
                        (index as isize + delta).rem_euclid(len)
                    });
                    let (pane_id, label) = candidates[next as usize].clone();
                    self.observability.selected_pane = Some(pane_id);
                    self.observability.selected_pane_label = Some(label);
                }
            }
            Action::BindTo(pane_id, account_id) => {
                let label = self
                    .agent_panes(None)
                    .find(|pane| pane.pane_id == pane_id)
                    .map_or_else(|| pane_id.clone(), Self::bind_candidate_label);
                self.observability.selected_pane = Some(pane_id.clone());
                self.observability.selected_pane_label = Some(label);
                let endpoint = self.active_endpoint_id.clone();
                self.bind_pane_to_account(endpoint, pane_id, account_id, false, outcome);
            }
            Action::Refresh => {
                // 各自只唤醒自己的作用域：悬浮层的「刷新」不提前触发页面轮询。
                if from_hover {
                    self.observability.hover_scope.request_refresh();
                } else {
                    self.observability.request_refresh();
                    self.observability.next_metrics = Instant::now();
                }
            }
            Action::Source => {
                // 选中账号优先（ACC-09），否则回退作用域内第一个账号。
                let accounts = if from_hover {
                    &self.observability.hover_scope.accounts
                } else {
                    &self.observability.accounts
                };
                let selected = self.observability.scoped_account(from_hover);
                if let Some(url) = accounts
                    .iter()
                    .find(|account| Some(&account.account_id) == selected.as_ref())
                    .or_else(|| accounts.first())
                    .map(|account| account.source_url.clone())
                    .filter(|url| crate::app::actions::safe_web_url(url).is_some())
                {
                    outcome.actions.push(ClientShellAction::OpenSafeWebUrl(url));
                }
            }
            Action::Process(identity) => {
                self.observation_request(
                    Method::SystemProcessGet(ProcessGetParams { identity }),
                    Purpose::Process,
                    outcome,
                );
            }
            Action::CancelProcess => self.observability.process_dialog = None,
            Action::Terminate(force) => {
                if let Some(dialog) = &mut self.observability.process_dialog {
                    dialog.force = force;
                    dialog.confirm = true;
                }
            }
            Action::ConfirmProcess => {
                if let Some(dialog) = self.observability.process_dialog.as_mut().filter(|dialog| {
                    dialog.confirm
                        && !dialog.pending
                        && !dialog.process.protected
                        && dialog.process.action_token.is_some()
                }) {
                    dialog.pending = true;
                    let params = ProcessTerminateParams {
                        identity: dialog.process.identity.clone(),
                        action_token: dialog.process.action_token.clone(),
                        force: dialog.force,
                    };
                    self.observation_request(
                        Method::SystemProcessTerminate(params),
                        Purpose::Terminate,
                        outcome,
                    );
                }
            }
            Action::SortProcesses(sort) => self.observability.process_sort = sort,
            Action::FilterProcesses => {
                self.observability.filtering_processes = !self.observability.filtering_processes
            }
        }
        let preferences = &mut self.config.preferences;
        let usage = &self.observability.usage;
        match preference {
            Some(PreferenceKey::Monitor) => {
                preferences.monitor = Some(self.observability.monitor.clone());
            }
            Some(PreferenceKey::UsageEnabled) => preferences.usage_enabled = Some(usage.enabled),
            Some(PreferenceKey::UsageFormat) => preferences.usage_format = Some(usage.format),
            Some(PreferenceKey::UsagePosition) => {
                preferences.usage_position = Some(usage.position);
            }
            Some(PreferenceKey::DisabledProviders) => {
                preferences.usage_disabled_providers = Some(usage.disabled_providers.clone());
            }
            Some(PreferenceKey::HoverDelay) => {
                preferences.usage_hover_delay_ms = Some(usage.hover_delay_ms);
            }
            Some(PreferenceKey::ChartGlyphs) => {
                preferences.monitor_chart_glyphs = Some(self.observability.chart_glyphs);
            }
            // 恢复已在动作分支里把键置 None，这里只需落盘。
            Some(PreferenceKey::RestoreUsage) | None => {}
        }
        if preference.is_some() {
            self.persist_chrome_preferences(outcome);
            self.observability.usage_overridden = self.config.preferences.usage_overridden();
        }
        outcome.repaint = true;
    }

    /// 发一次 `account.binding.set`，严格发往 `endpoint`（pane id 只在其所在主机
    /// 有意义）：悬浮层发起的绑定发往 hover 的端点，回流只刷新悬浮层；页面发起的
    /// 回流是页面强意图刷新。所有入口共用的厂商校验：pane 必须正在运行该账号
    /// 所属厂商的 agent，解析不出或不一致时写指引、不发请求（服务端不校验 agent，
    /// 错绑会被持久化）。
    fn bind_pane_to_account(
        &mut self,
        endpoint: ClientEndpointId,
        pane_id: String,
        account_id: String,
        from_hover: bool,
        outcome: &mut ClientShellInput,
    ) {
        let pane_agent = self.endpoint_pane_agent(&endpoint, &pane_id);
        let account_agent = self.account_agent(&account_id, from_hover);
        let consistent = match (pane_agent.as_deref(), account_agent.as_deref()) {
            (Some(pane_agent), Some(account_agent)) => {
                pane_agent.eq_ignore_ascii_case(account_agent)
            }
            _ => false,
        };
        if !consistent {
            self.observability.message = Some(match account_agent {
                Some(agent) => {
                    if zh() {
                        format!("{pane_id} 没有运行 {agent} 的 agent，未绑定；请选择运行该厂商 agent 的 pane。")
                    } else {
                        format!("{pane_id} is not running a {agent} agent; pick a pane running that provider's agent.")
                    }
                }
                None => tr(
                    "Cannot tell which provider this account belongs to yet; wait for the account list and try again.",
                    "暂时无法确认该账号所属厂商，未绑定；等账号列表刷新后再试。",
                )
                .into(),
            });
            return;
        }
        let purpose = if from_hover {
            Purpose::HoverBinding
        } else {
            Purpose::Binding
        };
        if self.observation_request_at(
            Some(endpoint),
            Method::AccountBindingSet(AccountBindingParams {
                pane_id,
                account_id,
            }),
            purpose,
            outcome,
        ) == RequestOutcome::Unavailable
        {
            self.observability.message = Some(
                tr(
                    "The pane's host is offline; the pane was not bound.",
                    "pane 所在主机不在线，未绑定。",
                )
                .into(),
            );
        }
    }

    pub(super) fn observation_mouse(
        &mut self,
        mouse: MouseEvent,
        outcome: &mut ClientShellInput,
    ) -> bool {
        if self.overlay.is_some() || self.popup_terminal_id.is_some() {
            if self.observability.hover.is_some() {
                self.observability.clear_hover();
            }
            return false;
        }
        let point = (mouse.column, mouse.row);
        if self.observability.process_dialog.is_some() {
            if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
                if let Some(action) =
                    self.observability
                        .hits
                        .iter()
                        .rev()
                        .find_map(|(rect, action)| {
                            (contains(*rect, point)
                                && matches!(
                                    action,
                                    Action::CancelProcess
                                        | Action::Terminate(_)
                                        | Action::ConfirmProcess
                                ))
                            .then(|| action.clone())
                        })
                {
                    self.observation_action(action, outcome);
                }
            }
            return true;
        }
        // 浮层独占内部输入，空白和滚轮也不能穿透到底层卡片或进程行。
        if contains(self.observability.hover_rect, point) {
            if let Some(hover) = &mut self.observability.hover {
                hover.leave_at = None;
            }
            match mouse.kind {
                MouseEventKind::ScrollDown => {
                    self.observability.scroll_accounts(1, true);
                    outcome.repaint = true;
                }
                MouseEventKind::ScrollUp => {
                    self.observability.scroll_accounts(-1, true);
                    outcome.repaint = true;
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    if let Some(action) = self
                        .observability
                        .hover_hits
                        .iter()
                        .rev()
                        .find(|(rect, _)| contains(*rect, point))
                        .map(|(_, action)| action.clone())
                    {
                        self.observation_action_in(action, true, outcome);
                    }
                }
                _ => {}
            }
            return true;
        }
        if matches!(
            mouse.kind,
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp
        ) {
            if let Some(card) = self.observability.hits.iter().find_map(|(rect, action)| {
                if contains(*rect, point) {
                    if let Action::Card(id) = action {
                        Some(id.clone())
                    } else {
                        None
                    }
                } else {
                    None
                }
            }) {
                let delta = if mouse.kind == MouseEventKind::ScrollDown {
                    1
                } else {
                    -1
                };
                self.observability.scroll_card(&card, delta);
                outcome.repaint = true;
                return true;
            }
            if self.observability.page == Some(Page::Accounts) {
                self.observability.scroll_accounts(
                    if mouse.kind == MouseEventKind::ScrollDown {
                        1
                    } else {
                        -1
                    },
                    false,
                );
                outcome.repaint = true;
                return true;
            }
        }
        if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
            if let Some(action) = self
                .observability
                .hits
                .iter()
                .rev()
                .find(|(rect, _)| contains(*rect, point))
                .map(|(_, action)| action.clone())
            {
                self.observation_action(action, outcome);
                return true;
            }
        }
        if self.observability.page.is_some() && contains(self.observability.page_rect, point) {
            match mouse.kind {
                MouseEventKind::ScrollDown => self.observability.scroll_page(3),
                MouseEventKind::ScrollUp => self.observability.scroll_page(-3),
                _ => {}
            }
            outcome.repaint = true;
            return true;
        }
        // 浮层外按下即结束悬浮。
        if matches!(mouse.kind, MouseEventKind::Down(_)) && self.observability.hover.is_some() {
            self.observability.clear_hover();
            outcome.repaint = true;
        }
        let hover_moves = self.observability.usage.enabled
            && mouse.kind == MouseEventKind::Moved
            && self.chrome_drag.is_none()
            && self.pane_mouse_gesture.is_none();
        let pinned = self
            .observability
            .hover
            .as_ref()
            .is_some_and(|hover| hover.pinned);
        if hover_moves && pinned {
            // 钉住的浮层不随指针离开关闭，也不被别的 agent 行悬浮替换。
        } else if hover_moves && self.observability.usage.position != UsageDisplayPosition::Page {
            let pane = self
                .hits
                .endpoint_agents
                .iter()
                .find(|(rect, _, _)| contains(*rect, point))
                .cloned()
                .or_else(|| {
                    self.hits
                        .agents
                        .iter()
                        .find(|(rect, _)| contains(*rect, point))
                        .map(|(rect, pane)| (*rect, self.active_endpoint_id.clone(), pane.clone()))
                })
                .or_else(|| {
                    self.hits
                        .panes
                        .iter()
                        .find(|hit| mouse.row == hit.rect.y && contains(hit.rect, point))
                        .map(|hit| {
                            (
                                hit.rect,
                                self.active_endpoint_id.clone(),
                                hit.pane_id.clone(),
                            )
                        })
                });
            let target = pane.and_then(|(rect, endpoint, pane)| {
                self.endpoints
                    .iter()
                    .find(|entry| entry.endpoint_id == endpoint)?
                    .snapshot
                    .as_ref()?
                    .agents
                    .iter()
                    .find(|agent| agent.pane_id == pane)
                    .and_then(|agent| {
                        agent
                            .agent
                            .clone()
                            .filter(|agent| is_bindable_agent(agent))
                            .map(|agent| (rect, endpoint, pane, agent))
                    })
            });
            if let Some((anchor, endpoint_id, pane, agent)) = target {
                let same = self.observability.hover.as_ref().is_some_and(|hover| {
                    matches!(
                        &hover.target,
                        HoverTarget::Agent {
                            endpoint_id: current_endpoint,
                            pane: current_pane,
                            ..
                        } if *current_pane == pane && *current_endpoint == endpoint_id
                    )
                });
                if !same {
                    self.observability.hover = Some(Hover {
                        target: HoverTarget::Agent {
                            endpoint_id,
                            pane,
                            agent,
                        },
                        anchor,
                        since: Instant::now(),
                        visible: false,
                        leave_at: None,
                        pinned: false,
                    });
                    self.observability.hover_rect = Rect::default();
                    outcome.repaint = true;
                } else if let Some(hover) = &mut self.observability.hover {
                    hover.leave_at = None;
                }
            } else if let Some(hover) = &mut self.observability.hover {
                hover
                    .leave_at
                    .get_or_insert(Instant::now() + Duration::from_millis(250));
            }
        } else if hover_moves {
            // `usage.position = page` 关掉了 agent 行悬浮：设置切换前已存在的浮层仍
            // 按 250 ms 宽限关闭。
            if let Some(hover) = &mut self.observability.hover {
                hover
                    .leave_at
                    .get_or_insert(Instant::now() + Duration::from_millis(250));
            }
        }
        false
    }

    pub(super) fn observation_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) -> bool {
        // 钉住的浮层：Esc 关闭（其它按键照常落到终端 / 页面）。
        if key.code == KeyCode::Esc
            && self
                .observability
                .hover
                .as_ref()
                .is_some_and(|hover| hover.pinned)
        {
            self.observability.clear_hover();
            outcome.repaint = true;
            return true;
        }
        if self.observability.page.is_none() && self.observability.process_dialog.is_none() {
            return false;
        }
        if self.observability.process_dialog.is_some()
            && !matches!(
                key.code,
                KeyCode::Tab | KeyCode::BackTab | KeyCode::Enter | KeyCode::Esc
            )
        {
            return true;
        }
        if self.observability.filtering_processes {
            match key.code {
                KeyCode::Esc | KeyCode::Enter => self.observability.filtering_processes = false,
                KeyCode::Backspace => {
                    self.observability.process_filter.pop();
                }
                KeyCode::Char(ch)
                    if !ch.is_control() && self.observability.process_filter.len() < 256 =>
                {
                    self.observability.process_filter.push(ch)
                }
                _ => {}
            }
            outcome.repaint = true;
            return true;
        }
        if self.observability.page == Some(Page::Monitor)
            && matches!(key.code, KeyCode::Up | KeyCode::Down)
        {
            if let Some(card) = &self.observability.selected_card {
                // 编辑布局模式下 ↑↓ 移动选中的卡片，其余时候滚动卡片内容。
                if self.observability.layout_editing {
                    let card = card.clone();
                    let delta = if key.code == KeyCode::Down { 1 } else { -1 };
                    self.observation_action(Action::CardMove(card, delta), outcome);
                    return true;
                }
                let card = card.clone();
                let delta = if key.code == KeyCode::Down { 1 } else { -1 };
                self.observability.scroll_card(&card, delta);
                outcome.repaint = true;
                return true;
            }
        }
        // 键盘只在页面命中区（`hits[..page_hits]`）内循环；悬浮层与页面同帧时
        // 浮层的命中区只服务鼠标，Tab / Enter 不会跳到浮层的「打开页面」。
        let keyboard_hits = self.observability.page_hits;
        let action = match key.code {
            KeyCode::Tab | KeyCode::BackTab => {
                if keyboard_hits > 0 {
                    let current = self.observability.selected_hit.min(keyboard_hits - 1);
                    self.observability.selected_hit = if key.code == KeyCode::BackTab {
                        (current + keyboard_hits - 1) % keyboard_hits
                    } else {
                        (current + 1) % keyboard_hits
                    };
                }
                None
            }
            KeyCode::Enter => self
                .observability
                .hits
                .get(self.observability.selected_hit)
                .filter(|_| self.observability.selected_hit < keyboard_hits)
                .map(|(_, action)| action.clone()),
            KeyCode::Esc if self.observability.process_dialog.is_some() => {
                Some(Action::CancelProcess)
            }
            // 编辑布局模式下 Esc 先结束编辑，再按一次才关闭页面。
            KeyCode::Esc if self.observability.layout_editing => Some(Action::EditLayout),
            KeyCode::Esc => Some(Action::Close),
            KeyCode::Char('1') => Some(Action::Page(Page::Monitor)),
            KeyCode::Char('2') => Some(Action::Page(Page::Accounts)),
            KeyCode::Char('3') | KeyCode::Char('s') => Some(Action::Configure),
            KeyCode::Char('r') => Some(Action::Refresh),
            KeyCode::Char(' ') if self.observability.page == Some(Page::Monitor) => {
                Some(Action::Pause)
            }
            KeyCode::Char('/') if self.observability.page == Some(Page::Monitor) => {
                Some(Action::FilterProcesses)
            }
            KeyCode::Down | KeyCode::PageDown
                if self.observability.page == Some(Page::Accounts) =>
            {
                self.observability.scroll_accounts(3, false);
                None
            }
            KeyCode::Up | KeyCode::PageUp if self.observability.page == Some(Page::Accounts) => {
                self.observability.scroll_accounts(-3, false);
                None
            }
            KeyCode::Down | KeyCode::PageDown => {
                self.observability.scroll_page(3);
                None
            }
            KeyCode::Up | KeyCode::PageUp => {
                self.observability.scroll_page(-3);
                None
            }
            _ => None,
        };
        if let Some(action) = action {
            self.observation_action(action, outcome);
        }
        outcome.repaint = true;
        true
    }

    /// 经典布局：页面、悬浮层与进程对话框一次画进 pane 区域；页面打开时
    /// pane 命中区整体让位给页面。返回供 occlusion 使用的覆盖区（见
    /// `Painted::covered`）。
    pub(super) fn paint_observability(
        &mut self,
        canvas: &mut super::compose_canvas::ComposeCanvas,
        area: Rect,
    ) -> Option<[Rect; 2]> {
        let cx = super::feedback::ChromeContext {
            page_bounds: None,
            palette: &self.config.palette,
            components: &self.config.components,
            glyphs: self.config.border_glyphs,
            hover: self.hover.as_ref(),
            spinner: self.spinner_glyph(),
            now: self
                .last_composed_at
                .unwrap_or_else(std::time::Instant::now),
        };
        self.observability.begin_paint();
        let painted = self
            .observability
            .paint(canvas, area, &cx, self.observability.page, true)?;
        let covered = painted.covered;
        self.observability.commit_paint(painted);
        if self.observability.page.is_some() {
            self.hits.panes.clear();
            self.hits.pane_splits.clear();
        }
        Some(covered)
    }
}

#[cfg(test)]
mod usage_history_tests {
    use super::*;

    fn sampled(account_id: &str, at_ms: u64, percent: Option<f64>) -> AccountUsageSnapshot {
        AccountUsageSnapshot {
            account_id: account_id.into(),
            account_label: account_id.into(),
            agent: "claude".into(),
            provider: "claude".into(),
            status: ObservationStatus::Ready,
            observed_at_ms: at_ms,
            metrics: vec![UsageMetric {
                label: "session".into(),
                used_percent: percent,
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    /// 右栏 sparkline 的数据源：每次采样按 `observed_at_ms` 前进追加，重复投递
    /// 同一份快照不产生第二个点。
    #[test]
    fn samples_are_recorded_once_per_observed_at_ms() {
        let mut history: HashMap<String, VecDeque<UsageSample>> = HashMap::new();
        record_usage_history(
            &mut history,
            &[sampled("claude:default", 1_000, Some(12.0))],
        );
        record_usage_history(
            &mut history,
            &[sampled("claude:default", 1_000, Some(12.0))],
        );
        record_usage_history(
            &mut history,
            &[sampled("claude:default", 2_000, Some(30.0))],
        );
        let samples = history.get("claude:default").expect("样本");
        assert_eq!(samples.len(), 2, "同一 observed_at_ms 只记一次");
        assert_eq!(samples[0].at_ms, 1_000);
        assert_eq!(samples[1].percent, 30.0);
    }

    /// 没有额度字段的账号不产生样本；已经过期的 `observed_at_ms` 不覆盖新点。
    #[test]
    fn samples_skip_accounts_without_quota_and_ignore_stale_snapshots() {
        let mut history: HashMap<String, VecDeque<UsageSample>> = HashMap::new();
        record_usage_history(&mut history, &[sampled("claude:default", 2_000, None)]);
        assert!(history.is_empty(), "只报请求数的账号不进 sparkline");
        record_usage_history(
            &mut history,
            &[sampled("claude:default", 2_000, Some(40.0))],
        );
        record_usage_history(
            &mut history,
            &[sampled("claude:default", 1_500, Some(10.0))],
        );
        let samples = history.get("claude:default").expect("样本");
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].percent, 40.0, "过期快照不追加");
    }

    /// 压力值取各窗口里最高的一档，并夹在 0..100。
    #[test]
    fn pressure_percent_takes_the_tightest_window() {
        let mut account = sampled("claude:default", 1_000, Some(12.0));
        account.metrics.push(UsageMetric {
            label: "weekly".into(),
            used: Some(90.0),
            limit: Some(100.0),
            ..Default::default()
        });
        assert_eq!(usage_pressure_percent(&account), Some(90.0));
        let mut over = sampled("claude:default", 1_000, Some(1_000.0));
        over.metrics[0].used_percent = Some(250.0);
        assert_eq!(usage_pressure_percent(&over), Some(100.0));
    }

    /// 上限：旧点先出队。
    #[test]
    fn samples_are_bounded() {
        let mut samples: VecDeque<UsageSample> = VecDeque::new();
        for at_ms in 1..=(USAGE_HISTORY_SAMPLES as u64 + 10) {
            push_usage_sample(&mut samples, at_ms, at_ms as f32);
        }
        assert_eq!(samples.len(), USAGE_HISTORY_SAMPLES);
        assert_eq!(samples.front().map(|sample| sample.at_ms), Some(11));
    }
}
