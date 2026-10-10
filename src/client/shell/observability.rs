//! 监控面板的客户端状态；网络和设备访问均通过公开 API。

mod render;
use super::*;
use crate::api::schema::*;
use crate::config::MonitorConfig;
use crossterm::event::{KeyCode, MouseButton, MouseEvent, MouseEventKind};
use std::time::{Duration, Instant};

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

/// 告警规则指标的显示名：偏好页告警行与资源告警通知共用一份。配置里写了不认识
/// 的 id 时原样显示（`State::alert` 也不会为它取值），不猜测含义。
pub(super) fn alert_metric_label(metric: &str) -> &str {
    let texts = &crate::i18n::texts().monitor;
    match metric {
        "cpu" => texts.alert_metric_cpu,
        "memory" => texts.alert_metric_memory,
        "gpu" => texts.alert_metric_gpu,
        "disk" => texts.alert_metric_disk,
        other => other,
    }
}

/// 监控面板内的页面（tab）；可持久化为客户端偏好，因此 wire 名固定为 snake_case。
/// 已移除的账号页曾以 `accounts` 持久化：偏好读取遇到不认识的值宽松回落到系统页。
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Page {
    Monitor,
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
    Process,
    Terminate,
}

impl Purpose {
    /// 在途去重键：同键请求在途时不重复发。
    fn key(&self) -> &'static str {
        match self {
            Self::Metrics => "metrics",
            Self::Process => "process",
            Self::Terminate => "terminate",
        }
    }
}

/// 设置页动作触到的偏好键：只回写用户改动的那一个键，其余键保持原值。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PreferenceKey {
    Monitor,
    /// 系统页迷你图 / 条形的字形档位（`monitor_chart_glyphs`）。
    ChartGlyphs,
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
            Action::ChartGlyphs(_) => Self::ChartGlyphs,
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
/// 告警阈值：50–100%，每档 5 个点（文档写的范围）。
const ALERT_THRESHOLD_STEPS: [f64; 11] = [
    50.0, 55.0, 60.0, 65.0, 70.0, 75.0, 80.0, 85.0, 90.0, 95.0, 100.0,
];

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
    /// 系统页迷你图 / 条形的字形档位。
    ChartGlyphs(ChartGlyphsPreference),
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
    Device(String),
    /// 第 n 条告警规则的阈值 / 持续 / 冷却步进（±1 档）。
    AlertThreshold(usize, i8),
    AlertDuration(usize, i8),
    AlertCooldown(usize, i8),
}

/// 每个网络接口保留的速率样本数（网络卡堆叠迷你图的窗口；按默认 1 s 采样约
/// 两分钟），也是渲染期栈上拷贝缓冲的上限。
pub(super) const NET_HISTORY_SAMPLES: usize = 128;

#[derive(Clone)]
pub(super) struct HistoryPoint {
    pub at: u64,
    pub cpu: Option<f32>,
    pub memory: Option<f32>,
    pub cores: Vec<Option<f32>>,
    /// 采样时未隐藏的传感器里最高的温度（°C）：温度卡的历史迷你图。
    pub temperature: Option<f32>,
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
    /// 供 kitty 图片 occlusion 使用的覆盖区：进程对话框存在时是对话框，否则是页面
    /// （空块由 `Occlusion::cover` 忽略）。
    pub covered: Rect,
    /// 页面（或进程对话框）的命中区。
    pub hits: Vec<(Rect, Action)>,
    pub page_rect: Rect,
    pub dialog: bool,
    /// 系统页各可滚动卡片的滚动上界，见 `render::CardScrollLimits`。
    pub card_scroll_limits: render::CardScrollLimits,
    /// 本次画出的页面的页面级滚动度量，见 `render::PageScrollLimits`。
    pub page_scroll_limits: render::PageScrollLimits,
}

/// `State::scroll_page` 一步的单位。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PageStep {
    /// 滚轮一格 / 方向键：系统页一行卡片，偏好页 `SETTINGS_NOTCH_LINES` 行。
    Notch,
    /// PageUp / PageDown：一屏（`render::PageScroll::screen`）。
    Screen,
}

/// 偏好页滚轮一格 / 方向键滚动的行数。
const SETTINGS_NOTCH_LINES: usize = 3;

pub(super) struct State {
    /// 当前拥有键盘输入的页面：经典布局下即打开的页面；停靠工作台下仅当
    /// 监控面板聚焦时为 `Some`，与 `dock.focused` 同步（不在渲染期改写）。
    pub page: Option<Page>,
    /// 用户在监控面板里选中的 tab；焦点回到终端后面板仍画这个 tab，并持久化。
    pub monitor_tab: Page,
    pub monitor: MonitorConfig,
    pub metrics: Option<Box<SystemMetricsSnapshot>>,
    pub history: VecDeque<HistoryPoint>,
    /// 观测事件推送在呈现面可见时到达：下一次 tick 重绘一次（事件不逐帧 compose）。
    event_repaint: bool,
    pub page_rect: Rect,
    /// 本帧全部命中区（页面或进程对话框的）：键盘 Tab / Enter 在其中循环。
    pub hits: Vec<(Rect, Action)>,
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
    /// 上一帧各可滚动卡片的滚动上界（`commit_paint` 写回、`begin_paint` 复位）：
    /// `scroll_card` 按它钳位，与卡片渲染时的钳位是同一个值。
    pub card_scroll_limits: render::CardScrollLimits,
    /// 上一帧画出的系统页 / 偏好页的页面级滚动度量（`commit_paint` 写回、
    /// `begin_paint` 复位）：`scroll_page` 按它钳位，与页面渲染时的钳位是同一个值。
    pub page_scroll_limits: render::PageScrollLimits,
    /// 系统页卡片网格的滚动位置：首个可见的卡片行（单列一行一张卡、双列一行
    /// 两张）；设置页用 `settings_scroll`，两页互不泄漏。
    pub scroll: usize,
    /// 设置页的滚动位置（文本行）。
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
    next_metrics: Instant,
    /// 在途观测请求的去重键（`Purpose::key`）。
    pending: HashSet<&'static str>,
    alerts: HashMap<String, AlertState>,
}

impl State {
    /// 滚动系统页某张卡片的内容，按上一帧画出的该卡滚动上界
    /// （`card_scroll_limits`：条目数减去卡片内高放得下的条目数）钳位——内容
    /// 放得下时不动，滚到底时最后一条正好完整露出。上一帧没画出这张卡片时
    /// 不知道上界，只许往回滚。
    pub(super) fn scroll_card(&mut self, card: &str, delta: isize) {
        let current = self.card_scroll.get(card).copied().unwrap_or(0);
        let limit = self.card_scroll_limits.get(card).unwrap_or(current);
        // 先把存量值钳回上界再走一步：快照变动 / 卡片变高会让它越界，不钳的话
        // 反向的前几格都耗在越界部分上，画面不动。
        let next = current.min(limit).saturating_add_signed(delta).min(limit);
        self.card_scroll.insert(card.to_owned(), next);
    }

    /// 滚动 `page` 自己的列表：系统页按卡片行、设置页按文本行。两个滚动位置
    /// 分离，切页不互相泄漏。
    ///
    /// 与 `scroll_card` 同口径写回钳位：按上一帧画出的该页上界
    /// （`page_scroll_limits`）先把存量值钳回上界再走一步——多滚的格数不会
    /// 存进状态，反向第一格画面就动；上一帧没画出这一页时不知道上界，只许
    /// 往回滚。
    pub(super) fn scroll_page(&mut self, page: Page, step: PageStep, delta: isize) {
        let limits = self.page_scroll_limits.get(page);
        let scroll = match page {
            Page::Monitor => &mut self.scroll,
            Page::Settings => &mut self.settings_scroll,
        };
        let unit = match step {
            PageStep::Screen => limits.map_or(1, |limits| limits.screen.max(1)),
            PageStep::Notch if page == Page::Settings => SETTINGS_NOTCH_LINES,
            PageStep::Notch => 1,
        };
        let limit = limits.map_or(*scroll, |limits| limits.max);
        let delta = delta.saturating_mul(isize::try_from(unit).unwrap_or(isize::MAX));
        *scroll = (*scroll).min(limit).saturating_add_signed(delta).min(limit);
    }

    pub(super) fn reload_preferences(&mut self, config: &ClientShellConfig) {
        self.monitor = config
            .preferences
            .monitor
            .clone()
            .unwrap_or_else(|| config.monitor.clone());
        if let Some(value) = config.preferences.monitor_tab {
            self.monitor_tab = value;
        }
        self.chart_glyphs = config.preferences.monitor_chart_glyphs.unwrap_or_default();
        self.glyphs = config.border_glyphs;
        self.next_metrics = Instant::now();
    }

    pub fn new(config: &ClientShellConfig) -> Self {
        Self {
            page: None,
            monitor_tab: config.preferences.monitor_tab.unwrap_or(Page::Monitor),
            monitor: config
                .preferences
                .monitor
                .clone()
                .unwrap_or_else(|| config.monitor.clone()),
            metrics: None,
            history: VecDeque::new(),
            event_repaint: false,
            page_rect: Rect::default(),
            hits: Vec::new(),
            selected_hit: 0,
            selected_card: None,
            selected_core: None,
            layout_editing: false,
            chart_glyphs: config.preferences.monitor_chart_glyphs.unwrap_or_default(),
            net_history: HashMap::new(),
            card_scroll: HashMap::new(),
            card_scroll_limits: render::CardScrollLimits::default(),
            page_scroll_limits: render::PageScrollLimits::default(),
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
            next_metrics: Instant::now(),
            pending: HashSet::new(),
            alerts: HashMap::new(),
        }
    }

    /// 每帧绘制前复位上一帧的命中区与矩形；之后每次 `paint` 的结果经
    /// `commit_paint` 累加写回。
    pub(super) fn begin_paint(&mut self) {
        self.hits.clear();
        self.page_rect = Rect::default();
        self.card_scroll_limits = render::CardScrollLimits::default();
        self.page_scroll_limits = render::PageScrollLimits::default();
    }

    /// 渲染纯函数：把 `painting_page`（停靠面板传该面板的 tab，全局浮层传 `None`）
    /// 与进程对话框画进 `frame` 的 `area`，状态只读。`framed` 为真是经典布局 pass：
    /// 页面铺满 pane 区、没有别的 chrome，自带外框并在边框上写标题。
    ///
    /// 光标归属按覆盖区判定：只有页面矩形或对话框矩形真正包含 `frame.cursor`
    /// 坐标时才把整帧光标置空；停靠在旁边的监控面板不能抹掉聚焦终端的插入点
    /// （经典布局下页面铺满 pane 区域，行为不变）。
    pub(super) fn paint(
        &self,
        canvas: &mut super::compose_canvas::ComposeCanvas,
        area: Rect,
        cx: &super::feedback::ChromeContext<'_>,
        painting_page: Option<Page>,
        framed: bool,
    ) -> Option<Painted> {
        let palette = cx.palette;
        if painting_page.is_none() && self.process_dialog.is_none() {
            return None;
        }
        let output = render::paint(canvas.buffer(), area, self, cx, painting_page, framed);
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
            !(contains(output.page_rect, point) || contains(output.dialog_rect, point))
        });
        canvas.set_cursor(cursor);
        let dialog = !output.dialog_rect.is_empty();
        Some(Painted {
            covered: if dialog {
                output.dialog_rect
            } else {
                output.page_rect
            },
            hits: output.hits,
            page_rect: output.page_rect,
            dialog,
            card_scroll_limits: output.card_scroll_limits,
            page_scroll_limits: output.page_scroll_limits,
        })
    }

    /// 把一次绘制的命中区写回：页面命中区累加（多个停靠面板依次绘制），进程
    /// 对话框出现时独占全部命中区。
    pub(super) fn commit_paint(&mut self, painted: Painted) {
        if painted.dialog {
            self.hits = painted.hits;
        } else {
            self.hits.extend(painted.hits);
        }
        if !painted.page_rect.is_empty() {
            self.page_rect = painted.page_rect;
        }
        self.card_scroll_limits.merge(&painted.card_scroll_limits);
        self.page_scroll_limits.merge(&painted.page_scroll_limits);
        self.selected_hit = self.selected_hit.min(self.hits.len().saturating_sub(1));
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
            // 与温度卡同口径：隐藏的传感器不参与。
            let hidden = &self.monitor.hidden_devices;
            let temperature = snapshot
                .sensors
                .iter()
                .filter(|sensor| {
                    !hidden
                        .iter()
                        .any(|id| id.strip_prefix("sensor:") == Some(sensor.name.as_str()))
                })
                .filter_map(|sensor| sensor.temperature_celsius.filter(|value| value.is_finite()))
                .reduce(f32::max);
            self.history.push_back(HistoryPoint {
                at: snapshot.sampled_at_ms,
                cpu: snapshot.cpu_percent,
                memory,
                cores: snapshot
                    .cores
                    .iter()
                    .map(|core| core.usage_percent)
                    .collect(),
                temperature,
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
                        metrics.hostname,
                        alert_metric_label(&rule.metric),
                        value
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
    pub(super) fn open_observation_page(&mut self, page: Page, outcome: &mut ClientShellInput) {
        // One dock panel hosts the system and preferences pages; the requested
        // page becomes the active tab inside it and is remembered across focus
        // changes (persisted as a client preference).
        let tab_changed = self.config.preferences.monitor_tab != Some(page);
        self.observability.monitor_tab = page;
        self.workbench_open(dock::PanelId::Monitor);
        self.observability.page = Some(page);
        if tab_changed {
            self.config.preferences.monitor_tab = Some(page);
            self.persist_chrome_preferences(outcome);
        }
        // 滚动位置按页分离：打开哪页只复位哪页，设置页的滚动不再泄漏到系统页。
        match page {
            Page::Monitor => self.observability.scroll = 0,
            Page::Settings => self.observability.settings_scroll = 0,
        }
        // 编辑布局只属于系统页。
        if page != Page::Monitor {
            self.observability.layout_editing = false;
        }
        self.observability.next_metrics = Instant::now();
        self.observability.message = None;
        self.selection = None;
        self.clear_link_hover();
        self.mode = ClientShellMode::Terminal;
        outcome.repaint = true;
    }

    /// 向活动端点发出一次观测请求。同类请求在途、端点离线、没有快照或未宣告该
    /// 方法时不发（未宣告时在页脚说明）：只禁用该动作，不让轮询停摆。
    fn observation_request(
        &mut self,
        method: Method,
        purpose: Purpose,
        outcome: &mut ClientShellInput,
    ) {
        let key = purpose.key();
        if self.observability.pending.contains(key) {
            return;
        }
        let endpoint_id = self.active_endpoint_id.clone();
        let Some(endpoint) = self.endpoints.iter().find(|endpoint| {
            endpoint.endpoint_id == endpoint_id && endpoint.status == ClientEndpointStatus::Online
        }) else {
            return;
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
            return;
        }
        let Some(snapshot) = endpoint.snapshot.as_ref() else {
            return;
        };
        let request_id = format!("client-shell:{}", self.next_request_id);
        self.next_request_id = self.next_request_id.saturating_add(1);
        let boot_id = snapshot.boot_id.clone();
        self.pending_requests.insert(
            request_id.clone(),
            PendingEndpointRequest {
                endpoint_id: endpoint_id.clone(),
                boot_id: boot_id.clone(),
                method_name: crate::api::api_method_name(&method).into(),
                confirmation_workspace_id: None,
                kind: PendingEndpointKind::Observation {
                    epoch: self.observability.epoch,
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
        self.observability.pending.insert(key);
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
            // 新端点 / 新 boot：此前的观测数据与在途请求全部作废。
            self.observability.epoch = self.observability.epoch.saturating_add(1);
            self.observability.metrics = None;
            self.observability.history.clear();
            self.observability.net_history.clear();
            self.observability.alerts.clear();
            self.observability.pending.clear();
            self.observability.process_dialog = None;
            self.observability.next_metrics = now;
        }
        let previous_second = self.observability.now_ms / 1000;
        self.observability.now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64;
        if self.observation_surface_visible() {
            outcome.repaint |= previous_second != self.observability.now_ms / 1000;
        }
        outcome.repaint |= std::mem::take(&mut self.observability.event_repaint);
        if !self.endpoint_is_online(&self.active_endpoint_id) {
            return;
        }
        let system_page_visible = self.observability.page == Some(Page::Monitor)
            || (self.workbench.visible(&dock::PanelId::Monitor)
                && self.observability.monitor_tab == Page::Monitor);
        if (system_page_visible || self.observability.monitor.alerts_enabled)
            && now >= self.observability.next_metrics
        {
            let config = &self.observability.monitor;
            let mut groups = if system_page_visible {
                config.visible.clone()
            } else {
                Vec::new()
            };
            if config.alerts_enabled {
                for rule in &config.alerts {
                    let group = match rule.metric.as_str() {
                        "cpu" => "cpu",
                        "memory" => "memory",
                        "gpu" => "gpu",
                        "disk" => "disks",
                        _ => continue,
                    };
                    if !groups.iter().any(|item| item == group) {
                        groups.push(group.to_string());
                    }
                }
            }
            let interval_ms = config.interval_ms.clamp(500, 5000);
            self.observability.next_metrics = now + Duration::from_millis(interval_ms);
            if system_page_visible || !groups.is_empty() {
                let params = SystemMetricsParams {
                    interval_ms,
                    include_processes: groups.iter().any(|item| item == "processes"),
                    groups,
                };
                self.observation_request(
                    Method::SystemMetricsGet(params),
                    Purpose::Metrics,
                    outcome,
                );
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

    /// 观测响应（`handle_endpoint_result` 已按 boot 校验）：旧代际的响应直接丢弃。
    /// 返回呈现面是否可见（可见时立即重绘）。
    pub(super) fn receive_observation(
        &mut self,
        epoch: u64,
        purpose: Purpose,
        result: Result<ResponseResult, ClientShellEndpointError>,
    ) -> bool {
        if epoch != self.observability.epoch {
            return false;
        }
        self.observability.pending.remove(purpose.key());
        match result {
            Ok(ResponseResult::SystemMetrics { snapshot }) => {
                self.observability.apply_metrics(snapshot)
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
            Ok(_) => {}
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

    /// 端点推来的 `endpoint.observation.v1` 事件：只接受活动端点当前 boot 的推送。
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
            .source
            .as_ref()
            .is_some_and(|(source, boot_id)| source == endpoint_id && *boot_id == frame.boot_id);
        if !boot_matches || !source_matches {
            return false;
        }
        let ObservationEventEnvelope::SystemMetricsUpdated(event) = frame.event;
        self.observability.apply_metrics(event.snapshot);
        let visible = self.observation_surface_visible();
        self.observability.event_repaint |= visible;
        visible
    }

    /// 监控数据当前是否有可见的呈现面：经典布局的页面或停靠的监控面板。新响应
    /// 到达时据此立即重绘，每秒 tick 据此刷新时间显示。
    fn observation_surface_visible(&self) -> bool {
        self.observability.page.is_some() || self.workbench.visible(&dock::PanelId::Monitor)
    }

    pub(super) fn observation_action(&mut self, action: Action, outcome: &mut ClientShellInput) {
        // 偏好按键上锁：动作只回写它自己触到的那一个键（见 `PreferenceKey`）。
        let preference = PreferenceKey::for_action(&action);
        match action {
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
            Action::AlertThreshold(index, delta) => {
                // 与其它步进器一样走档位表（文档终审 D4）：档间值先走到相邻档，
                // 越过两端回绕，结果永远落在 50–100%。以前固定 ±5，配置里的 30
                // 会被加到 35。
                if let Some(rule) = self.observability.monitor.alerts.get_mut(index) {
                    rule.threshold = step_ladder(&ALERT_THRESHOLD_STEPS, rule.threshold, delta);
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
                self.observability.process_dialog = None;
                self.observability.layout_editing = false;
                if self.workbench.enabled {
                    // 关闭的是承载页面的监控面板（而非当前聚焦面板）：终端聚焦时
                    // 也能从命令面板关掉监控面板；锁定布局时给出反馈。
                    self.close_workbench_panel(dock::PanelId::Monitor, outcome);
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
            Action::Refresh => self.observability.next_metrics = Instant::now(),
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
        match preference {
            Some(PreferenceKey::Monitor) => {
                preferences.monitor = Some(self.observability.monitor.clone());
            }
            Some(PreferenceKey::ChartGlyphs) => {
                preferences.monitor_chart_glyphs = Some(self.observability.chart_glyphs);
            }
            None => {}
        }
        if preference.is_some() {
            self.persist_chrome_preferences(outcome);
        }
        outcome.repaint = true;
    }

    /// 指针下画着的监控页（D5）：经典布局是打开的页面（铺在 pane 区）；停靠工作台按
    /// 指针所在的监控面板取它画的 `monitor_tab`（没聚焦也照画，渲染不改写 `page`）。
    /// 输入阶段按上一帧的几何命中。
    fn observation_page_at(&self, point: (u16, u16)) -> Option<Page> {
        if !self.workbench.enabled {
            return self
                .observability
                .page
                .filter(|_| contains(self.observability.page_rect, point));
        }
        self.workbench
            .geometry
            .panels
            .iter()
            .find_map(|(panel, area)| {
                (*panel == dock::PanelId::Monitor
                    && contains(super::workbench::body(*area, panel), point))
                .then_some(self.observability.monitor_tab)
            })
    }

    pub(super) fn observation_mouse(
        &mut self,
        mouse: MouseEvent,
        outcome: &mut ClientShellInput,
    ) -> bool {
        if self.overlay.is_some() || self.popup_terminal_id.is_some() {
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
        if matches!(
            mouse.kind,
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp
        ) {
            let delta = if mouse.kind == MouseEventKind::ScrollDown {
                1
            } else {
                -1
            };
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
                // 卡片内容放得下（上界 0）或根本不可滚（CPU / 内存卡、上一帧没画出
                // 内容）时滚轮交给页面，否则落在卡片上的滚轮会吃掉整页滚动。卡片
                // 命中区只在系统页上登记，面板未聚焦时同样成立。
                if self
                    .observability
                    .card_scroll_limits
                    .get(&card)
                    .is_some_and(|limit| limit > 0)
                {
                    self.observability.scroll_card(&card, delta);
                } else {
                    self.observability
                        .scroll_page(Page::Monitor, PageStep::Notch, delta);
                }
                outcome.repaint = true;
                return true;
            }
            // 其余落点按指针下画着的那一页滚（D5）：停靠的监控面板没聚焦也照画
            // `monitor_tab`，卡片之间的空隙与偏好页上的滚轮都不要求先聚焦，也不改
            // 焦点；指针不在页面上（例如指在终端上）就不归页面。
            if let Some(page) = self.observation_page_at(point) {
                self.observability.scroll_page(page, PageStep::Notch, delta);
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
        // 拥有键盘的页面独占落在页面矩形里的其余指针事件（滚轮已在上面按落点分派）。
        if self.observability.page.is_some() && contains(self.observability.page_rect, point) {
            outcome.repaint = true;
            return true;
        }
        false
    }

    pub(super) fn observation_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) -> bool {
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
                // 与滚轮同口径：卡片内容放得下（上界 0）或根本不可滚（CPU / 内存卡、
                // 上一帧没画出内容）时落到下面的页面滚动，否则选中这类卡片后 ↑↓
                // 空转、页面不动。
                if self
                    .observability
                    .card_scroll_limits
                    .get(card)
                    .is_some_and(|limit| limit > 0)
                {
                    let card = card.clone();
                    let delta = if key.code == KeyCode::Down { 1 } else { -1 };
                    self.observability.scroll_card(&card, delta);
                    outcome.repaint = true;
                    return true;
                }
            }
        }
        // 键盘在页面（或进程对话框）的命中区内循环。
        let keyboard_hits = self.observability.hits.len();
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
            KeyCode::Char('2') | KeyCode::Char('s') => Some(Action::Configure),
            KeyCode::Char('r') => Some(Action::Refresh),
            KeyCode::Char(' ') if self.observability.page == Some(Page::Monitor) => {
                Some(Action::Pause)
            }
            KeyCode::Char('/') if self.observability.page == Some(Page::Monitor) => {
                Some(Action::FilterProcesses)
            }
            KeyCode::Down | KeyCode::Up | KeyCode::PageDown | KeyCode::PageUp => {
                if let Some(page) = self.observability.page {
                    let step = if matches!(key.code, KeyCode::PageDown | KeyCode::PageUp) {
                        PageStep::Screen
                    } else {
                        PageStep::Notch
                    };
                    let delta = if matches!(key.code, KeyCode::Down | KeyCode::PageDown) {
                        1
                    } else {
                        -1
                    };
                    self.observability.scroll_page(page, step, delta);
                }
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

    /// 经典布局：页面与进程对话框一次画进 pane 区域；页面打开时 pane 命中区
    /// 整体让位给页面。返回供 occlusion 使用的覆盖区（见 `Painted::covered`）。
    pub(super) fn paint_observability(
        &mut self,
        canvas: &mut super::compose_canvas::ComposeCanvas,
        area: Rect,
    ) -> Option<Rect> {
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
mod alert_tests {
    use super::*;
    use crate::i18n::{lang_guard, Lang};

    /// 内存规则立即触发（持续 0 s）、内存占用 95% 的状态。
    fn memory_alert_state() -> State {
        let mut state = State::new(&ClientShellConfig::from_config(
            &crate::config::Config::default(),
        ));
        state.monitor.alerts_enabled = true;
        let mut rule = crate::config::MonitorConfig::default()
            .alerts
            .into_iter()
            .find(|rule| rule.metric == "memory")
            .expect("默认规则里有内存");
        rule.duration_seconds = 0;
        state.monitor.alerts = vec![rule];
        state.now_ms = 100_000;
        state.metrics = Some(Box::new(SystemMetricsSnapshot {
            status: ObservationStatus::Ready,
            sampled_at_ms: 100_000,
            hostname: "host".into(),
            memory: MemoryMetric {
                total_bytes: 100,
                used_bytes: 95,
                ..Default::default()
            },
            ..Default::default()
        }));
        state
    }

    fn observation_client_with_history() -> ClientShellState {
        let mut state = ClientShellState::new(ClientShellConfig::from_config(
            &crate::config::Config::default(),
        ));
        state.set_snapshot(Box::new(crate::client::shell::tests::snapshot()));
        state.tick_observability(Instant::now(), &mut ClientShellInput::default());
        let mut fixture = memory_alert_state();
        let mut metrics = fixture.metrics.take().unwrap();
        metrics.boot_id = state.snapshot.as_ref().unwrap().boot_id.clone();
        state.observability.monitor = fixture.monitor;
        state.observability.now_ms = fixture.now_ms;
        state.observability.apply_metrics(metrics);
        state
            .observability
            .net_history
            .insert("shared-nic".into(), VecDeque::from([(10.0, 20.0)]));
        assert!(state.observability.alert().is_some());
        state
    }

    #[test]
    fn audit_p1_observability_new_source_clears_histories_and_rearms_alerts() {
        for (new_endpoint, new_boot) in [(false, true), (true, false), (true, true)] {
            let mut state = observation_client_with_history();
            let mut projection = crate::client::shell::tests::snapshot();
            projection.revision += 1;
            if new_boot {
                projection.boot_id = "next-boot".into();
            }
            let boot_id = projection.boot_id.clone();
            if new_endpoint {
                let profile = crate::client::endpoint::SavedSshEndpoint::new(
                    "other",
                    "user@other.invalid",
                    "default",
                )
                .unwrap();
                let endpoint = ClientEndpointId::Ssh(profile.id.clone());
                state.set_endpoint_catalog(&[profile]);
                state.set_endpoint_status(&endpoint, ClientEndpointStatus::Online);
                state.set_endpoint_snapshot(&endpoint, Box::new(projection));
                assert!(state.activate_endpoint_projection(&endpoint));
            } else {
                state.set_snapshot(Box::new(projection));
            }
            state.tick_observability(Instant::now(), &mut ClientShellInput::default());

            assert!(
                state.observability.history.is_empty(),
                "new endpoint={new_endpoint}, new boot={new_boot}"
            );
            assert!(state.observability.net_history.is_empty());
            assert!(state.observability.alerts.is_empty());
            let mut metrics = memory_alert_state().metrics.take().unwrap();
            metrics.boot_id = boot_id;
            metrics.hostname = "new-host".into();
            metrics.sampled_at_ms = state.observability.now_ms;
            state.observability.apply_metrics(metrics);
            let alert = state
                .observability
                .alert()
                .expect("new host alert must not inherit the previous host latch");
            assert!(alert.starts_with("new-host"));
        }
    }

    #[test]
    fn audit_p1_observability_same_source_retains_histories_and_alert_latch() {
        let mut state = observation_client_with_history();
        let mut projection = crate::client::shell::tests::snapshot();
        projection.revision += 1;
        state.set_snapshot(Box::new(projection));
        state.tick_observability(Instant::now(), &mut ClientShellInput::default());

        assert_eq!(state.observability.history.len(), 1);
        assert_eq!(state.observability.net_history["shared-nic"].len(), 1);
        assert!(state.observability.alerts["memory"].armed);
    }

    /// 冒烟 L11：资源告警通知与偏好页告警行同一份指标显示名，不把配置里的原始
    /// id（memory / disk）拼进正文。
    #[test]
    fn alert_body_names_the_metric_in_the_ui_language() {
        {
            let _guard = lang_guard(Lang::ZhCn);
            let body = memory_alert_state().alert().expect("超过阈值即告警");
            assert!(body.contains("内存"), "{body}");
            assert!(!body.contains("memory"), "{body}");
        }
        let _guard = lang_guard(Lang::En);
        let body = memory_alert_state().alert().expect("alert fires");
        assert!(body.contains("Memory"), "{body}");
    }
}
