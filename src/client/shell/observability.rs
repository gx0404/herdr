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

/// 浮动用量仪表盘的正文：按 `usage.format` 画仪表盘（进度条）或表格，作用域
/// 是页面作用域（与账号页共用 `accounts` / `selected_*` / `account_scroll`），
/// 账号行命中区写进 `hits` 供浮层内点击派发。
pub(super) fn render_usage_body(
    buffer: &mut Buffer,
    area: Rect,
    state: &State,
    palette: &Palette,
    hits: &mut Vec<(Rect, Action)>,
) {
    let scope = render::page_scope(state);
    if state.usage.format == UsageDisplayFormat::Table {
        render::usage_table(buffer, area, state, &scope, palette, hits);
    } else {
        render::usage_dashboard(buffer, area, state, &scope, palette, hits);
    }
    if scope.refreshing {
        // 与账号页一致：强意图刷新在途时旧快照保留但变暗。
        buffer.set_style(area, Style::default().add_modifier(Modifier::DIM));
    }
}

/// 监控面板内的页面（tab）；可持久化为客户端偏好，因此 wire 名固定为 snake_case。
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Page {
    Monitor,
    Accounts,
    Settings,
}

#[derive(Clone, Debug)]
pub(super) enum Purpose {
    Metrics,
    Providers,
    Usage,
    /// 悬浮层自己的用量请求：带 hover 的 pane，落到 `hover_scope`，永不写页面。
    HoverUsage,
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
    fn key(&self) -> &'static str {
        match self {
            Self::Metrics => "metrics",
            Self::Providers => "providers",
            Self::Usage => "usage",
            Self::HoverUsage => "hover_usage",
            Self::Binding => "binding",
            Self::HoverBinding => "hover_binding",
            Self::Integration { .. } => "integration",
            Self::HoverIntegration { .. } => "hover_integration",
            Self::Subscribe => "subscribe",
            Self::Unsubscribe => "unsubscribe",
            Self::Process => "process",
            Self::Terminate => "terminate",
        }
    }

    /// 悬浮层作用域的请求：目标端点与代际都取自 `hover_scope`。
    fn is_hover(&self) -> bool {
        matches!(
            self,
            Self::HoverUsage | Self::HoverBinding | Self::HoverIntegration { .. }
        )
    }

    /// 随页面代际作废的请求：既不是悬浮层作用域，也不是订阅生命周期。
    fn page_scoped(&self) -> bool {
        !self.is_hover() && !matches!(self, Self::Subscribe | Self::Unsubscribe)
    }

    const HOVER_KEYS: [&'static str; 3] = ["hover_usage", "hover_binding", "hover_integration"];
    /// 不随页面代际作废的请求键：悬浮层作用域 + 订阅生命周期。
    const PAGE_INDEPENDENT_KEYS: [&'static str; 5] = [
        "hover_usage",
        "hover_binding",
        "hover_integration",
        "subscribe",
        "unsubscribe",
    ];
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

#[derive(Clone, Debug)]
pub(super) enum Action {
    Page(Page),
    Close,
    Pause,
    Configure,
    Refresh,
    ToggleAlerts,
    Interval,
    Metric(String),
    UsageEnabled,
    UsageFormat,
    UsagePosition,
    /// 悬浮延时档位循环（200 / 400 / 800 / 1200 / 2000 ms），持久化为客户端偏好。
    HoverDelay,
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
    SortProcesses,
    FilterProcesses,
    Card(String),
    Core(usize),
    CardMove(String, isize),
    CardSize,
    HistoryRange,
    ProviderEnabled(String),
    Device(String),
    AlertThreshold(usize),
    AlertDuration(usize),
    AlertCooldown(usize),
}

#[derive(Clone)]
pub(super) struct HistoryPoint {
    pub at: u64,
    pub cpu: Option<f32>,
    pub memory: Option<f32>,
    pub cores: Vec<Option<f32>>,
}

/// 悬浮层的目标：Agents 面板里某个 pane 的 agent（按 pane 查该厂商用量），或
/// 面板头部「用量」按钮的跨厂商总览（不带厂商 / pane，不受 `usage.position` 门禁）。
#[derive(Clone, Debug, PartialEq)]
pub(super) enum HoverTarget {
    Agent {
        endpoint_id: ClientEndpointId,
        pane: String,
        agent: String,
    },
    UsageOverview,
}

pub(super) struct Hover {
    pub target: HoverTarget,
    pub anchor: Rect,
    pub since: Instant,
    pub visible: bool,
    pub leave_at: Option<Instant>,
    /// 点击「用量」按钮钉住的总览：指针离开不再关闭，Esc / 再次点击 / 浮层外
    /// 点击才关闭。只有 `UsageOverview` 会被钉住。
    pub pinned: bool,
}

impl Hover {
    pub(super) fn is_overview(&self) -> bool {
        self.target == HoverTarget::UsageOverview
    }
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
    fn request_refresh(&mut self) {
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
    /// 不合成外接矩形：经典布局下页面铺在 pane 区、总览浮层锚在侧栏按钮下方，
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
    pub providers: Vec<UsageProviderInfo>,
    /// 账号页 / 浮动仪表盘的账号快照（页面作用域）。
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
    /// 客户端偏好 `usage_hover_dashboard`：扫过「用量」按钮是否弹出跨厂商总览
    /// （默认开）。关掉后点击仍可钉住总览。
    pub usage_hover_dashboard: bool,
    /// 官方回调启用成功后排队的绑定，下一次 tick 发出。
    queued_binding: Option<QueuedBinding>,
    /// 订阅推送在呈现面可见时到达：下一次 tick 重绘一次（事件不逐帧 compose）。
    event_repaint: bool,
    /// 上一 tick 页面作用域（账号页 / 浮动仪表盘）是否可见：由不可见变可见时
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
    /// 循环，浮层命中区只服务鼠标——否则页面打开且总览钉住时 Tab 到末尾会把
    /// 高亮夹在最后一个页面控件上、Enter 却触发浮层的「打开账号页」。
    pub page_hits: usize,
    pub selected_hit: usize,
    pub selected_card: Option<String>,
    pub selected_core: Option<usize>,
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
    /// 页面作用域排队中的强意图刷新：请求真正发出后才清除，被在途请求挡下时
    /// 保留并在 200 ms 后重试；端点不支持 / 厂商已关闭时清除（回落到普通 get）。
    refresh_usage: bool,
    /// 账号页打开时尚无厂商列表：列表到达后按聚焦 pane 的 agent / 首个已安装
    /// 厂商补选一次。
    auto_select_provider: bool,
    /// 上一 tick 账号页是否可见：由不可见变可见（含偏好恢复 / 面板随布局
    /// 恢复）且尚未选厂商时触发自动选中。
    accounts_page_seen: bool,
    /// 已在页脚说明过「所选主机不在线」的目标端点：回退目标不变时不重写页脚，
    /// 避免每 2 秒刷掉其它一次性提示。
    fallback_noted: Option<ClientEndpointId>,
    pending: HashSet<&'static str>,
    alerts: HashMap<String, AlertState>,
}

impl State {
    /// 页面作用域处于「刷新中」：强意图刷新已排队或正在途中。渲染期据此把旧
    /// 数据变暗、空态显示「刷新中…」、「刷新」按钮退化为状态提示。
    pub(super) fn refreshing(&self) -> bool {
        self.refresh_usage || self.manual_in_flight
    }

    /// 滚动账号列表；`hover` 为真时按悬浮层作用域的账号数夹取并写悬浮层自己的
    /// 滚动位置，否则按页面。总览浮层用紧凑版的行数口径。
    pub(super) fn scroll_accounts(&mut self, delta: isize, hover: bool) {
        let (accounts, refresh_states) = if hover {
            (&self.hover_scope.accounts, &self.hover_scope.refresh_states)
        } else {
            (&self.accounts, &self.refresh_states)
        };
        let overview = hover && self.hover.as_ref().is_some_and(Hover::is_overview);
        let rows = if overview {
            render::usage_hover_rows(accounts)
        } else {
            render::account_rows(self, accounts, refresh_states)
        };
        let limit = rows.saturating_sub(1);
        let scroll = if hover {
            &mut self.hover_scope.scroll
        } else {
            &mut self.account_scroll
        };
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

    /// 页面作用域是否接受某账号：只按用户当前的厂商 / 账号选择过滤（服务端已按
    /// 订阅参数过滤，这里挡住切换作用域后旧订阅的尾巴）。
    fn page_scope_accepts(&self, account: &AccountUsageSnapshot) -> bool {
        self.selected_provider
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
        self.selected_provider
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

    /// 页面作用域换代：作废所有在途页面请求（悬浮层的请求与订阅生命周期不受影响）。
    /// 旧账号快照保留到新数据到达（渲染期变暗），但旧刷新状态（防抖截止、待办绑定）
    /// 属于旧作用域，一并清掉。
    fn bump_page_epoch(&mut self) {
        self.epoch = self.epoch.saturating_add(1);
        self.pending
            .retain(|key| Purpose::PAGE_INDEPENDENT_KEYS.contains(key));
        self.manual_in_flight = false;
        self.refresh_states.clear();
    }

    /// 页面作用域的强意图刷新：下一次 tick 立即发 `account.usage.refresh`。
    fn request_refresh(&mut self) {
        self.refresh_usage = true;
        self.next_usage = Instant::now();
    }

    /// 打开跨厂商总览的悬浮层作用域：复位并换代，不选厂商 / pane（请求带
    /// `agent=None, pane_id=None`），定向到 `endpoint`，出现本身视为强意图刷新。
    /// 页面作用域（`selected_*` / `accounts` / `epoch` / `account_scroll`）不动。
    fn open_overview_scope(&mut self, endpoint: ClientEndpointId) {
        self.reset_hover_scope();
        self.hover_scope.endpoint = Some(endpoint);
        self.hover_scope.request_refresh();
    }

    /// 复位悬浮层作用域并换代，作废在途悬浮层请求。
    fn reset_hover_scope(&mut self) {
        let epoch = self.hover_scope.epoch.saturating_add(1);
        self.hover_scope = HoverScope {
            epoch,
            ..HoverScope::default()
        };
        self.pending
            .retain(|key| !Purpose::HOVER_KEYS.contains(key));
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
        self.usage_hover_dashboard = config.preferences.usage_hover_dashboard.unwrap_or(true);
        self.glyphs = config.border_glyphs;
        self.next_metrics = Instant::now();
        self.next_usage = Instant::now();
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
            usage_hover_dashboard: config.preferences.usage_hover_dashboard.unwrap_or(true),
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
        frame: &mut FrameData,
        area: Rect,
        palette: &Palette,
        painting_page: Option<Page>,
        draw_hover: bool,
    ) -> Option<Painted> {
        let visible = painting_page.is_some()
            || self.process_dialog.is_some()
            || (draw_hover && self.hover.as_ref().is_some_and(|hover| hover.visible));
        if !visible {
            return None;
        }
        let mut buffer = frame.to_ratatui_buffer()?;
        let output = render::paint(&mut buffer, area, self, palette, painting_page, draw_hover);
        if painting_page.is_some() {
            let selected = self.selected_hit.min(output.hits.len().saturating_sub(1));
            if let Some((rect, _)) = output.hits.get(selected) {
                buffer.set_style(
                    *rect,
                    Style::default()
                        .fg(palette.accent)
                        .add_modifier(Modifier::UNDERLINED),
                );
            }
        }
        let cursor = frame.cursor.clone().filter(|cursor| {
            let point = (cursor.x, cursor.y);
            !(contains(output.page_rect, point)
                || contains(output.hover_rect, point)
                || contains(output.dialog_rect, point))
        });
        frame.replace_from_ratatui_buffer_preserving_effects(&buffer, cursor);
        let dialog = !output.dialog_rect.is_empty();
        // 经典布局下页面与总览浮层可同帧出现：两块各自交给 kitty 图片 occlusion，
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
            self.alerts.clear();
        }
        if !self.paused && snapshot.sampled_at_ms > 0 {
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
    pub(super) fn toggle_usage_dashboard(&mut self, outcome: &mut ClientShellInput) {
        if matches!(self.overlay, Some(ClientShellOverlay::UsageDashboard)) {
            self.overlay = None;
        } else {
            self.overlay = Some(ClientShellOverlay::UsageDashboard);
            // 仪表盘是跨厂商总览：悬浮层作用域整体复位，页面选择回到总览态并
            // 换代，打开本身视为强意图刷新；厂商列表也顺手重拉一次。
            self.observability.clear_hover();
            self.observability.auto_select_provider = false;
            self.observability.account_scroll = 0;
            self.observability.bump_page_epoch();
            self.observability.selected_provider = None;
            self.observability.selected_account = None;
            self.observability.selected_pane = None;
            self.observability.selected_pane_label = None;
            self.observability.request_refresh();
            self.observability.next_providers = Instant::now();
        }
        outcome.repaint = true;
    }

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
        if self.observability.pending.contains(key) {
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
        });
        self.observability.pending.insert(key);
        RequestOutcome::Sent
    }

    /// 发一次页面作用域的用量请求：`manual` 为真时走 `account.usage.refresh`。
    fn page_usage_request(
        &mut self,
        manual: bool,
        outcome: &mut ClientShellInput,
    ) -> RequestOutcome {
        // 页面轮询永不带 pane_id：pane 只属于 `Action::Bind`。
        let params = UsageParams {
            agent: self.observability.selected_provider.clone(),
            account_id: self.observability.selected_account.clone(),
            pane_id: None,
        };
        let method = if manual {
            Method::AccountUsageRefresh(params)
        } else {
            Method::AccountUsageGet(params)
        };
        self.observation_request(method, Purpose::Usage, outcome)
    }

    /// 发一次悬浮层作用域的用量请求（带 hover 的 pane）。
    fn hover_usage_request(
        &mut self,
        manual: bool,
        outcome: &mut ClientShellInput,
    ) -> RequestOutcome {
        let params = UsageParams {
            agent: self.observability.hover_scope.provider.clone(),
            account_id: None,
            pane_id: self.observability.hover_scope.pane.clone(),
        };
        let method = if manual {
            Method::AccountUsageRefresh(params)
        } else {
            Method::AccountUsageGet(params)
        };
        self.observation_request(method, Purpose::HoverUsage, outcome)
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
        // 孤儿防护：总览浮层锚定「用量」按钮，按钮命中区消失（面板变窄 / 关闭
        // 鼠标捕获）或移位（重新布局）即结束浮层，钉住的也不例外。只看完整
        // compose 产生的命中区：终端 resize / 配置重载 / 未配对 surface 会把
        // `hits` 整体重置成空表且不立即重绘，空表不代表锚点丢失。
        let orphaned = self.hits.composed
            && self.observability.hover.as_ref().is_some_and(|hover| {
                hover.is_overview()
                    && (self.hits.agent_usage_toggle.is_empty()
                        || hover.anchor != self.hits.agent_usage_toggle)
            });
        if orphaned {
            self.observability.clear_hover();
            outcome.repaint = true;
        }
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
                    Some(HoverTarget::UsageOverview) => {
                        let endpoint = self.active_endpoint_id.clone();
                        self.observability.open_overview_scope(endpoint);
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
        // 账号页（经典布局页面 / 停靠面板的账号 tab / legacy 账号面板）可见；
        // 浮动仪表盘是跨厂商总览，单列出来以免触发自动选厂商。
        let accounts_page_visible = self.observability.page == Some(Page::Accounts)
            || (self.workbench.visible(&dock::PanelId::Monitor)
                && self.observability.monitor_tab == Page::Accounts)
            || self.workbench.visible(&dock::PanelId::Accounts);
        let page_visible = accounts_page_visible
            || matches!(self.overlay, Some(ClientShellOverlay::UsageDashboard));
        let usage_visible = page_visible || hover_visible;
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
                .retain(|key| !matches!(*key, "subscribe" | "unsubscribe"));
            self.observability.queued_binding = None;
            self.observability.next_usage = now;
            self.observability.next_providers = now;
        }
        // 页面作用域由不可见变可见（订阅期间兜底轮询可能还有几十秒才到期）：
        // 冷启动一次 get，先把当前快照拿到手，再靠事件保持实时。
        if page_visible && !self.observability.page_seen {
            self.observability.next_usage = now;
        }
        self.observability.page_seen = page_visible;
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
        // 页面作用域的订阅：账号页 / 浮动仪表盘可见、厂商未关闭且端点宣告了订阅
        // 方法时按当前 (厂商, 账号) 订阅；不可见即退订。订阅覆盖期间轮询降为低频兜底。
        let provider_disabled = |state: &State, agent: Option<&String>| {
            agent.is_some_and(|agent| state.usage.disabled_providers.contains(agent))
        };
        let subscription_wanted = page_visible
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
        let page_due = (page_visible || settings_open) && now >= self.observability.next_usage;
        let hover_due = hover_visible && now >= self.observability.hover_scope.next_usage;
        if self.observability.usage.enabled && (page_due || hover_due) {
            // 厂商列表：首次、页面打开时与每 5 分钟低频重拉（OBS-14）。
            if (self.observability.providers.is_empty() || now >= self.observability.next_providers)
                && self.observation_request(
                    Method::AccountUsageProviders(EmptyParams::default()),
                    Purpose::Providers,
                    outcome,
                ) == RequestOutcome::Sent
            {
                self.observability.next_providers = now + Duration::from_secs(300);
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
                let disabled = self
                    .observability
                    .selected_provider
                    .as_ref()
                    .is_some_and(|agent| {
                        self.observability.usage.disabled_providers.contains(agent)
                    });
                let mut retry_soon = false;
                if disabled {
                    // 设置里关掉的厂商不发请求；排队中的强意图刷新必须消费掉，
                    // 否则页面永远停在「刷新中…」且「刷新」按钮失去命中区。
                    if std::mem::take(&mut self.observability.refresh_usage) {
                        self.observability.message = Some(
                            tr(
                                "This provider is disabled in settings.",
                                "此厂商已在设置中关闭，可在设置页重新启用。",
                            )
                            .into(),
                        );
                    }
                } else {
                    let manual = self.observability.refresh_usage;
                    let mut sent = self.page_usage_request(manual, outcome);
                    if manual && sent == RequestOutcome::Unavailable {
                        // 端点未宣告 account.usage.refresh（或此刻不可用）：只禁用
                        // 手动刷新，同一轮回落到普通 get，轮询不停摆。
                        self.observability.refresh_usage = false;
                        sent = self.page_usage_request(false, outcome);
                    }
                    match sent {
                        RequestOutcome::Sent => {
                            // 只在请求真正发出后清除强意图；发出的是 refresh 时进入在途。
                            self.observability.usage_sent_at = Some(now);
                            if std::mem::take(&mut self.observability.refresh_usage) {
                                self.observability.manual_in_flight = true;
                            }
                        }
                        // 被在途请求挡下的强意图刷新不能丢：保留标志并 200 ms 后重试。
                        RequestOutcome::Busy => retry_soon = self.observability.refresh_usage,
                        RequestOutcome::Unavailable => {}
                    }
                }
                self.observability.next_usage = cadence(retry_soon, subscribed);
            }
            if hover_due {
                let disabled = self
                    .observability
                    .hover_scope
                    .provider
                    .as_ref()
                    .is_some_and(|agent| {
                        self.observability.usage.disabled_providers.contains(agent)
                    });
                let mut retry_soon = false;
                if disabled {
                    self.observability.hover_scope.refresh = false;
                } else {
                    let manual = self.observability.hover_scope.refresh;
                    let mut sent = self.hover_usage_request(manual, outcome);
                    if manual && sent == RequestOutcome::Unavailable {
                        self.observability.hover_scope.refresh = false;
                        sent = self.hover_usage_request(false, outcome);
                    }
                    match sent {
                        RequestOutcome::Sent => {
                            self.observability.hover_scope.sent_at = Some(now);
                            if std::mem::take(&mut self.observability.hover_scope.refresh) {
                                self.observability.hover_scope.manual_in_flight = true;
                            }
                        }
                        RequestOutcome::Busy => retry_soon = self.observability.hover_scope.refresh,
                        RequestOutcome::Unavailable => {}
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
        self.observability.pending.remove(purpose.key());
        // 用量响应（成功或失败）到达即结束该作用域的在途手动刷新；排队中的下一次
        // 强意图（`refresh_usage` / `hover_scope.refresh`）不受影响，仍显示刷新中。
        match purpose {
            Purpose::Usage => self.observability.manual_in_flight = false,
            Purpose::HoverUsage => self.observability.hover_scope.manual_in_flight = false,
            _ => {}
        }
        match result {
            Ok(ResponseResult::SystemMetrics { snapshot }) => {
                self.observability.apply_metrics(snapshot)
            }
            Ok(ResponseResult::AccountUsageProviders { providers }) => {
                self.observability.providers = providers;
                self.auto_select_usage_provider();
            }
            Ok(ResponseResult::AccountUsage { accounts, refresh }) => {
                // 响应只写自己的作用域，且不反写用户选择的厂商；刷新状态整体替换
                // （get / refresh 响应是权威快照，旧 server 不带即为空）。服务端说探测
                // 在途时把该作用域的下一次轮询收紧到 500 ms（页面已由订阅覆盖时靠事件
                // 收尾，不提前轮询）。
                let refresh = refresh.unwrap_or_default();
                let in_flight = refresh.iter().any(|state| state.in_flight);
                if matches!(purpose, Purpose::HoverUsage) {
                    let scope = &mut self.observability.hover_scope;
                    scope.accounts = accounts;
                    scope.refresh_states = refresh;
                    if in_flight {
                        let soon = scope.sent_at.unwrap_or_else(Instant::now) + IN_FLIGHT_POLL;
                        scope.next_usage = scope.next_usage.min(soon);
                    }
                } else {
                    self.observability.accounts = accounts;
                    self.observability.refresh_states = refresh;
                    if in_flight && !self.observability.subscribed() {
                        let soon = self
                            .observability
                            .usage_sent_at
                            .unwrap_or_else(Instant::now)
                            + IN_FLIGHT_POLL;
                        self.observability.next_usage = self.observability.next_usage.min(soon);
                    }
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
                    purpose = purpose.key(),
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
    /// legacy 账号面板、浮动用量仪表盘。新响应到达时据此立即重绘，每秒 tick
    /// 据此刷新时间显示；与 `usage_visible` 的面板判据保持同一套。
    fn observation_surface_visible(&self) -> bool {
        self.observability.page.is_some()
            || self.observability.hover.is_some()
            || self.workbench.visible(&dock::PanelId::Monitor)
            || self.workbench.visible(&dock::PanelId::Accounts)
            || matches!(self.overlay, Some(ClientShellOverlay::UsageDashboard))
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
        let monitor_changed = matches!(
            &action,
            Action::Interval
                | Action::Metric(_)
                | Action::CardMove(..)
                | Action::CardSize
                | Action::HistoryRange
                | Action::Device(_)
                | Action::ToggleAlerts
                | Action::AlertThreshold(_)
                | Action::AlertDuration(_)
                | Action::AlertCooldown(_)
        );
        // 偏好按键上锁：`usage_changed` 只覆盖四个 usage_* 键，悬浮延时单独判定，
        // 否则只点「悬浮延时」也会把从未改过的 usage_* 键写成偏好影子值。
        let usage_changed = matches!(
            &action,
            Action::UsageEnabled
                | Action::UsageFormat
                | Action::UsagePosition
                | Action::ProviderEnabled(_)
        );
        let hover_delay_changed = matches!(&action, Action::HoverDelay);
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
            Action::CardSize => {
                self.observability.monitor.card_height =
                    match self.observability.monitor.card_height {
                        7 => 10,
                        10 => 16,
                        16 => 24,
                        _ => 7,
                    }
            }
            Action::HistoryRange => {
                self.observability.monitor.history_minutes =
                    match self.observability.monitor.history_minutes {
                        1 => 5,
                        5 => 15,
                        15 => 30,
                        30 => 60,
                        _ => 1,
                    }
            }
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
            }
            Action::AlertThreshold(index) => {
                if let Some(rule) = self.observability.monitor.alerts.get_mut(index) {
                    rule.threshold = if rule.threshold >= 100.0 {
                        50.0
                    } else {
                        rule.threshold + 5.0
                    };
                }
            }
            Action::AlertDuration(index) => {
                if let Some(rule) = self.observability.monitor.alerts.get_mut(index) {
                    rule.duration_seconds = match rule.duration_seconds {
                        10 => 30,
                        30 => 60,
                        60 => 300,
                        _ => 10,
                    };
                }
            }
            Action::AlertCooldown(index) => {
                if let Some(rule) = self.observability.monitor.alerts.get_mut(index) {
                    rule.cooldown_seconds = match rule.cooldown_seconds {
                        60 => 300,
                        300 => 900,
                        _ => 60,
                    };
                }
            }
            Action::Page(page) => self.open_observation_page(page, outcome),
            Action::Close => {
                self.observability.clear_hover();
                self.observability.process_dialog = None;
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
            Action::Interval => {
                self.observability.monitor.interval_ms =
                    match self.observability.monitor.interval_ms {
                        500 => 1000,
                        1000 => 2000,
                        2000 => 5000,
                        _ => 500,
                    };
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
            Action::UsageFormat => {
                self.observability.account_scroll = 0;
                self.observability.usage.format = match self.observability.usage.format {
                    UsageDisplayFormat::Dashboard => UsageDisplayFormat::Table,
                    UsageDisplayFormat::Table => UsageDisplayFormat::Dashboard,
                }
            }
            Action::UsagePosition => {
                self.observability.usage.position = match self.observability.usage.position {
                    UsageDisplayPosition::Hover => UsageDisplayPosition::Page,
                    UsageDisplayPosition::Page => UsageDisplayPosition::Both,
                    UsageDisplayPosition::Both => UsageDisplayPosition::Hover,
                };
                // 切到「页面」= agent 行悬浮层关闭：页脚说明一次，设置行也常驻提示。
                if self.observability.usage.position == UsageDisplayPosition::Page {
                    self.observability.message =
                        Some(crate::i18n::texts().monitor.hover_closed_hint.to_owned());
                }
            }
            Action::HoverDelay => {
                // 在固定档位表里取「大于当前值的下一档」，末档回到首档：配置文件里的
                // 非档位值（如 300）第一次点击落到 400，不跳档。
                let current = self.observability.usage.hover_delay_ms;
                self.observability.usage.hover_delay_ms = HOVER_DELAY_STEPS
                    .iter()
                    .copied()
                    .find(|step| *step > current)
                    .unwrap_or(HOVER_DELAY_STEPS[0]);
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
            Action::SortProcesses => {
                self.observability.process_sort = match self.observability.process_sort {
                    ProcessSort::Cpu => ProcessSort::Memory,
                    ProcessSort::Memory => ProcessSort::Name,
                    _ => ProcessSort::Cpu,
                }
            }
            Action::FilterProcesses => {
                self.observability.filtering_processes = !self.observability.filtering_processes
            }
        }
        if monitor_changed {
            self.config.preferences.monitor = Some(self.observability.monitor.clone());
        }
        if usage_changed {
            self.config.preferences.usage_enabled = Some(self.observability.usage.enabled);
            self.config.preferences.usage_format = Some(self.observability.usage.format);
            self.config.preferences.usage_position = Some(self.observability.usage.position);
            self.config.preferences.usage_disabled_providers =
                Some(self.observability.usage.disabled_providers.clone());
        }
        if hover_delay_changed {
            self.config.preferences.usage_hover_delay_ms =
                Some(self.observability.usage.hover_delay_ms);
        }
        if monitor_changed || usage_changed || hover_delay_changed {
            self.persist_chrome_preferences(outcome);
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
                let count =
                    self.observability
                        .metrics
                        .as_ref()
                        .map_or(0, |sample| match card.as_str() {
                            "cores" => sample.cores.len(),
                            "gpu" => sample.gpus.len(),
                            "disks" => sample.disks.len(),
                            "network" => sample.networks.len(),
                            "sensors" => sample.sensors.len(),
                            "processes" => sample.processes.len(),
                            _ => 0,
                        });
                let scroll = self.observability.card_scroll.entry(card).or_default();
                *scroll = if mouse.kind == MouseEventKind::ScrollDown {
                    scroll.saturating_add(1).min(count.saturating_sub(1))
                } else {
                    scroll.saturating_sub(1)
                };
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
        // 浮层外按下即结束悬浮；「用量」按钮上的按下留给 `pin_usage_overview`
        // 处理（钉住 / 取消钉住），不能先把 hover 清掉。
        let on_usage_toggle = !self.hits.agent_usage_toggle.is_empty()
            && contains(self.hits.agent_usage_toggle, point);
        if matches!(mouse.kind, MouseEventKind::Down(_))
            && self.observability.hover.is_some()
            && !on_usage_toggle
        {
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
        if hover_moves && on_usage_toggle {
            // 跨厂商总览：不受 `usage.position` 门禁影响（F01）；已是总览就只撤销
            // 离开时刻，否则替换掉 agent 悬浮进入延时状态。偏好
            // `usage_hover_dashboard` 关闭时按钮不弹总览，但指针已经离开了 agent
            // 行，现有 agent 悬浮仍要按 250 ms 宽限关闭。
            match self.observability.hover.as_mut() {
                Some(hover) if hover.is_overview() => hover.leave_at = None,
                _ if self.observability.usage_hover_dashboard => {
                    self.observability.clear_hover();
                    self.observability.hover = Some(Hover {
                        target: HoverTarget::UsageOverview,
                        anchor: self.hits.agent_usage_toggle,
                        since: Instant::now(),
                        visible: false,
                        leave_at: None,
                        pinned: false,
                    });
                    self.observability.hover_rect = Rect::default();
                    outcome.repaint = true;
                }
                Some(hover) => {
                    hover
                        .leave_at
                        .get_or_insert(Instant::now() + Duration::from_millis(250));
                }
                None => {}
            }
        } else if hover_moves && pinned {
            // 钉住的总览不随指针离开关闭，也不被 agent 行悬浮替换。
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
            // `usage.position = page` 关掉了 agent 行悬浮，但总览浮层仍要在指针
            // 离开按钮后按 250 ms 宽限关闭。
            if let Some(hover) = &mut self.observability.hover {
                hover
                    .leave_at
                    .get_or_insert(Instant::now() + Duration::from_millis(250));
            }
        }
        false
    }

    /// 点击 Agents 面板头部的「用量」按钮：钉住 / 取消钉住跨厂商总览浮层。
    /// 幂等 show/hide，不复用 `toggle_usage_dashboard`：未显示 → 立即显示并钉住；
    /// 悬浮中（未钉住）→ 钉住；已钉住 → 关闭。钉住不走模态 overlay，页面
    /// （`observability.page`）与停靠焦点都不动。
    ///
    /// 入口门禁有意不对称：`usage.enabled` 关闭时扫过按钮不弹任何东西（被动
    /// 动作不该冒出浮层），但显式点击仍打开浮层，由 `render::usage_hover` 的
    /// disabled 分支说明「已在设置中关闭」——点击必须有反馈而不是静默失败。
    pub(super) fn pin_usage_overview(&mut self, outcome: &mut ClientShellInput) {
        let anchor = self.hits.agent_usage_toggle;
        match self.observability.hover.as_mut() {
            Some(hover) if hover.is_overview() && hover.pinned => {
                self.observability.clear_hover();
            }
            Some(hover) if hover.is_overview() => {
                hover.pinned = true;
                hover.leave_at = None;
                if !hover.visible {
                    hover.visible = true;
                    let endpoint = self.active_endpoint_id.clone();
                    self.observability.open_overview_scope(endpoint);
                }
            }
            _ => {
                self.observability.clear_hover();
                self.observability.hover = Some(Hover {
                    target: HoverTarget::UsageOverview,
                    anchor,
                    since: Instant::now(),
                    visible: true,
                    leave_at: None,
                    pinned: true,
                });
                self.observability.hover_rect = Rect::default();
                let endpoint = self.active_endpoint_id.clone();
                self.observability.open_overview_scope(endpoint);
            }
        }
        outcome.repaint = true;
    }

    pub(super) fn observation_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) -> bool {
        // 钉住的总览浮层：Esc 关闭（其它按键照常落到终端 / 页面）。
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
                let count =
                    self.observability
                        .metrics
                        .as_ref()
                        .map_or(0, |sample| match card.as_str() {
                            "cores" => sample.cores.len(),
                            "gpu" => sample.gpus.len(),
                            "disks" => sample.disks.len(),
                            "network" => sample.networks.len(),
                            "sensors" => sample.sensors.len(),
                            "processes" => sample.processes.len(),
                            _ => 0,
                        });
                let scroll = self
                    .observability
                    .card_scroll
                    .entry(card.clone())
                    .or_default();
                *scroll = if key.code == KeyCode::Down {
                    scroll.saturating_add(1).min(count.saturating_sub(1))
                } else {
                    scroll.saturating_sub(1)
                };
                outcome.repaint = true;
                return true;
            }
        }
        // 键盘只在页面命中区（`hits[..page_hits]`）内循环；总览浮层与页面同帧时
        // 浮层的命中区只服务鼠标，Tab / Enter 不会跳到浮层的「打开账号页」。
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
        frame: &mut FrameData,
        area: Rect,
    ) -> Option<[Rect; 2]> {
        self.observability.begin_paint();
        let painted = self.observability.paint(
            frame,
            area,
            &self.config.palette,
            self.observability.page,
            true,
        )?;
        let covered = painted.covered;
        self.observability.commit_paint(painted);
        if self.observability.page.is_some() {
            self.hits.panes.clear();
            self.hits.pane_splits.clear();
        }
        Some(covered)
    }
}
