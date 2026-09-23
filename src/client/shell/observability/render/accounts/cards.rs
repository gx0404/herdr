//! 账号卡片的内容模型：每个账号按 `account.agent` 分派到厂商专属排布，产出一组
//! 与宽度无关的卡片行（行数即滚动真源）；概览（全部厂商）每厂商一张紧凑卡。
//! 只算不画，绘制在 `paint.rs`。

use std::borrow::Cow;

use super::slots::{codex_bucket, slot_of, Slot, SlotKind};
use super::*;

/// 厂商卡片的家族：决定徽标写账号状态，还是「本地统计 / 会话统计」声明。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Family {
    /// 账号额度（claude / codex / kimi / 未知厂商）：徽标是状态。
    Quota,
    /// 本机统计，不是账号额度（opencode / zcode）。
    Local,
    /// 会话统计，不是账号额度（pi）。
    Session,
}

pub(super) fn family(agent: &str) -> Family {
    match agent {
        "opencode" | "zcode" => Family::Local,
        "pi" => Family::Session,
        _ => Family::Quota,
    }
}

/// 数据的可信度：决定条形是否着语义色。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Liveness {
    /// 已更新 / 采集中：正常画。
    Live,
    /// 缓存数据：保留量值，条形转灰。
    Cached,
    /// 失效（未登录、查询失败等）：只画虚化占位，不给失效数据画确定的基线（F-2）。
    Dead,
}

pub(super) fn liveness(status: ObservationStatus) -> Liveness {
    match status {
        ObservationStatus::Ready | ObservationStatus::Warming => Liveness::Live,
        ObservationStatus::Stale => Liveness::Cached,
        _ => Liveness::Dead,
    }
}

/// 数值项的语气。标签与值按 token 整段取舍（`paint::stat_cell`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Tone {
    /// 数字（含服务端格式化好的时长 `4m19s`）：永不截断，放不下「标签 值」先整段
    /// 丢标签，连值都放不下就整项不画。
    Number,
    /// 未知 / 暂无数据：灰字，同样不截断。
    Muted,
    /// 服务端自由文本（模型名等）：标签之后仍有余量时值带省略号截断。
    Text,
}

/// 一个「标签 数值」项；一行最多三项等分列宽。
pub(super) struct Stat<'a> {
    pub label: Cow<'a, str>,
    pub value: String,
    pub tone: Tone,
}

/// 一条 meter（kit `meter_row` + `gauge`）。
pub(super) struct Meter<'a> {
    pub label: Cow<'a, str>,
    pub value: String,
    pub detail: Option<String>,
    /// 已用比例；`> 1.0` 画溢出态，`None` 画虚化占位。
    pub ratio: Option<f32>,
    /// 额度窗口已过比例（刻度）。
    pub window: Option<f32>,
    /// 灰色：缓存 / 失效数据，或未知值。
    pub stale: bool,
    /// 额度窗口已过重置时间：沿用上次值，文字随 `stale` 转灰，条形再叠 DIM
    /// （只弱化一次，数字与说明不叠 DIM）。
    pub expired: bool,
}

/// 卡片里的一行。
pub(super) enum Line<'a> {
    /// 状态 / 在途 / 新鲜度 / 套餐与身份；绘制时按宿主现算。
    Meta,
    /// 服务端说明（`message`、目录信任、推断绑定、厂商退避）。
    Note(Cow<'a, str>),
    /// 小节标题（codex 多桶、token 明细）。
    Section(Cow<'a, str>),
    Meter(Meter<'a>),
    Stats(Vec<Stat<'a>>),
    /// 没有任何可显示的用量（kit `empty_state`）。
    Empty(&'static str),
}

/// 一个账号的卡片。
pub(super) struct Card<'a> {
    pub account: &'a AccountUsageSnapshot,
    pub family: Family,
    pub lines: Vec<Line<'a>>,
}

impl Card<'_> {
    /// 不受视口限制时的高度：上下边框 + 全部行。
    pub(super) fn natural_height(&self) -> usize {
        self.lines.len() + 2
    }
}

/// 百分比文案：整数不带小数，其余一位；超过 100 原样显示（溢出态）。
pub(super) fn percent_text(percent: f32) -> String {
    if (percent - percent.round()).abs() < 0.05 {
        format!("{percent:.0}%")
    } else {
        format!("{percent:.1}%")
    }
}

/// token / 计数的紧凑写法：`950` / `12.3k` / `1.5M` / `2.1B`。
pub(super) fn compact_count(value: f64) -> String {
    let value = value.max(0.0);
    let scaled = |divisor: f64, suffix: &str| {
        let text = format!("{:.1}", value / divisor);
        let text = text.strip_suffix(".0").unwrap_or(&text).to_owned();
        format!("{text}{suffix}")
    };
    if value >= 1e9 {
        scaled(1e9, "B")
    } else if value >= 1e6 {
        scaled(1e6, "M")
    } else if value >= 1e3 {
        scaled(1e3, "k")
    } else {
        format!("{value:.0}")
    }
}

/// 金额：`USD` → `$1.23`、`CNY` → `¥12.30`、`{币种} cents` 折成主单位；其它单位
/// 写在数字后。≥1 保留两位小数，更小的金额保留到四位（`$0.0042` 不写成 `$0.00`）。
pub(super) fn money_text(amount: &str, unit: &str) -> String {
    let (unit, divisor) = match unit.strip_suffix(" cents") {
        Some(currency) => (currency, 100.0),
        None => (unit, 1.0),
    };
    let rendered = match amount.trim().parse::<f64>() {
        Ok(value) if value.is_finite() => {
            let value = value / divisor;
            if value.abs() >= 1.0 || value == 0.0 {
                format!("{value:.2}")
            } else {
                let text = format!("{value:.4}");
                text.trim_end_matches('0').to_owned()
            }
        }
        _ => amount.trim().to_owned(),
    };
    match unit {
        "USD" => format!("${rendered}"),
        "CNY" => format!("¥{rendered}"),
        "" => rendered,
        other => format!("{rendered} {other}"),
    }
}

/// 额度窗口的短时长：`5h` / `7d` / `90m`（整天 / 整小时优先）。
fn short_span(seconds: u64) -> String {
    if seconds >= 86_400 && seconds.is_multiple_of(86_400) {
        format!("{}d", seconds / 86_400)
    } else if seconds >= 3600 && seconds.is_multiple_of(3600) {
        format!("{}h", seconds / 3600)
    } else if seconds >= 60 && seconds.is_multiple_of(60) {
        format!("{}m", seconds / 60)
    } else {
        span_text(seconds)
    }
}

/// 额度窗口已过比例（0..=1）：`(now - (resets_at - window)) / window`。窗口长度
/// 取服务端的 `window_seconds`，报文不带时退到槽位的名义长度 `nominal`（claude /
/// kimi 的 5 小时 / 每周 / 7 天窗口）；两者都没有或不知道 `resets_at` 时为 `None`。
pub(super) fn window_progress(
    metric: &UsageMetric,
    nominal: Option<u64>,
    now_ms: u64,
) -> Option<f32> {
    let window = metric
        .window_seconds
        .filter(|window| *window > 0)
        .or(nominal)
        .filter(|window| *window > 0)?;
    let remaining = reset_secs(metric, now_ms)?;
    Some((1.0 - remaining as f32 / window as f32).clamp(0.0, 1.0))
}

/// 额度窗口是否已过重置时间：官方 statusline 在窗口过期后把它从 JSON 里去掉，
/// 服务端沿用上次值；客户端据 `resets_at` 不晚于当前时间判定，其余厂商同理。
fn window_expired(metric: &UsageMetric, now_ms: u64) -> bool {
    metric.resets_at.is_some_and(|reset| reset <= now_ms / 1000)
}

/// 构建卡片所需的上下文。
#[derive(Clone, Copy)]
pub(super) struct Cx {
    pub now_ms: u64,
    pub live: Liveness,
    pub texts: &'static crate::i18n::MonitorTexts,
}

/// 额度窗口 meter：数字是百分比（可超过 100），说明是用量（`used/limit`）与距重置；
/// 过期窗口灰字、条形 DIM，并写「已过重置 · 沿用上次值」。`slot` 是匹配表给的
/// 槽位（通用行为 `None`），固定窗口据它补名义长度画进度刻度。
pub(super) fn quota_meter<'a>(
    label: Cow<'a, str>,
    slot: Option<Slot>,
    metric: &'a UsageMetric,
    cx: Cx,
) -> Meter<'a> {
    let percent = metric_percent(metric);
    let expired = window_expired(metric, cx.now_ms);
    let quantity = metric_quantity(metric);
    let value = match percent {
        Some(percent) => percent_text(percent),
        None => quantity.clone(),
    };
    let mut detail = Vec::with_capacity(2);
    if percent.is_some() && quantity != "—" && metric.text_value.is_none() {
        detail.push(quantity);
    }
    if expired {
        detail.push(cx.texts.window_expired.to_owned());
    } else if let Some(reset) = reset_text(metric, cx.now_ms) {
        detail.push(reset);
    }
    let ratio = percent.map(|percent| percent / 100.0);
    Meter {
        label,
        value,
        detail: (!detail.is_empty()).then(|| detail.join(" · ")),
        ratio: if cx.live == Liveness::Dead {
            None
        } else {
            ratio
        },
        window: if expired {
            None
        } else {
            window_progress(metric, slot.and_then(Slot::nominal_window_secs), cx.now_ms)
        },
        stale: cx.live != Liveness::Live || expired || percent.is_none(),
        expired,
    }
}

/// 会话上下文占用 meter：百分比在 `unit = "%"` 的 `used` 里；未知（首次请求前 /
/// 压缩后）写「暂无数据」灰字，不当成 0。说明是上下文 token / 窗口大小。
fn context_meter<'a>(
    metric: &'a UsageMetric,
    tokens: Option<&'a UsageMetric>,
    window: Option<&'a UsageMetric>,
    cx: Cx,
) -> Meter<'a> {
    let percent = metric
        .used
        .filter(|used| used.is_finite() && metric.unit == "%")
        .map(|used| used.max(0.0) as f32);
    let detail = match (
        tokens.and_then(|metric| metric.used),
        window.and_then(|metric| metric.used),
    ) {
        (Some(tokens), Some(window)) => Some(format!(
            "{} / {}",
            compact_count(tokens),
            compact_count(window)
        )),
        (Some(tokens), None) => Some(compact_count(tokens)),
        (None, Some(window)) => Some(format!("— / {}", compact_count(window))),
        (None, None) => None,
    };
    Meter {
        label: Cow::Borrowed(cx.texts.context),
        value: percent.map_or_else(|| cx.texts.no_data_yet.to_owned(), percent_text),
        detail,
        ratio: percent
            .filter(|_| cx.live != Liveness::Dead)
            .map(|percent| percent / 100.0),
        window: None,
        stale: cx.live != Liveness::Live || percent.is_none(),
        expired: false,
    }
}

/// 槽位指标的数值项。
fn slot_stat<'a>(slot: Slot, metric: &'a UsageMetric, cx: Cx) -> Stat<'a> {
    let label = Cow::Borrowed(slot.label(cx.texts));
    let known = |value: String| Stat {
        label: label.clone(),
        value,
        tone: Tone::Number,
    };
    // 数值缺席（例如上下文 token 在首次请求前为 null）：写「暂无数据」灰字，不当成 0。
    let unknown = || Stat {
        label: label.clone(),
        value: cx.texts.no_data_yet.to_owned(),
        tone: Tone::Muted,
    };
    match slot.kind() {
        SlotKind::Money => match &metric.amount_decimal {
            Some(amount) => known(money_text(amount, &metric.unit)),
            None => metric.used.map_or_else(unknown, |used| {
                known(money_text(&used.to_string(), &metric.unit))
            }),
        },
        SlotKind::Tokens => metric
            .used
            .map_or_else(unknown, |used| known(compact_count(used))),
        SlotKind::Count => metric
            .used
            .map_or_else(unknown, |used| known(format!("{:.0}", used.max(0.0)))),
        SlotKind::Text => match metric.text_value.as_deref().filter(|text| !text.is_empty()) {
            // 时长 / 统计窗口是服务端格式化好的数字（`4m19s`），与数字同样整段显示、
            // 永不截成半截；只有模型名是可以带省略号截断的自由文本（真机 L3）。
            Some(text) => Stat {
                label: label.clone(),
                value: text.to_owned(),
                tone: if slot == Slot::Model {
                    Tone::Text
                } else {
                    Tone::Number
                },
            },
            None => metric
                .used
                .map_or_else(unknown, |used| known(trim_number(used))),
        },
        // 额度 / 百分比槽位不走数值项（由 meter 画）；兜底写百分比。
        SlotKind::Quota | SlotKind::Percent => {
            metric_percent(metric).map_or_else(unknown, |percent| known(percent_text(percent)))
        }
    }
}

/// 未进匹配表的指标：有百分比画额度 meter，否则写一个「服务端标签 数量」项
/// （金额与厂商卡片同一种写法）。
pub(super) fn generic_line<'a>(metric: &'a UsageMetric, cx: Cx) -> Line<'a> {
    if metric_percent(metric).is_some() {
        return Line::Meter(quota_meter(Cow::Borrowed(&metric.label), None, metric, cx));
    }
    let value = match (&metric.text_value, &metric.amount_decimal) {
        (None, Some(amount)) => money_text(amount, &metric.unit),
        _ => metric_quantity(metric),
    };
    let tone = if value == "—" {
        Tone::Muted
    } else if metric.text_value.is_some() {
        Tone::Text
    } else {
        Tone::Number
    };
    Line::Stats(vec![Stat {
        label: Cow::Borrowed(&metric.label),
        value,
        tone,
    }])
}

/// 按匹配表逐条取用账号的指标；没被厂商排布取走的留给通用行兜底。
struct Picker<'a> {
    metrics: &'a [UsageMetric],
    slots: Vec<Option<Slot>>,
    taken: Vec<bool>,
}

impl<'a> Picker<'a> {
    fn new(account: &'a AccountUsageSnapshot) -> Self {
        Self {
            metrics: &account.metrics,
            slots: account
                .metrics
                .iter()
                .map(|metric| slot_of(&account.agent, metric))
                .collect(),
            taken: vec![false; account.metrics.len()],
        }
    }

    /// 第一条未取用且满足条件的指标。
    fn take_where(
        &mut self,
        wanted: impl Fn(Slot, &UsageMetric) -> bool,
    ) -> Option<&'a UsageMetric> {
        let index = (0..self.metrics.len()).find(|index| {
            !self.taken[*index]
                && self.slots[*index].is_some_and(|slot| wanted(slot, &self.metrics[*index]))
        })?;
        self.taken[index] = true;
        Some(&self.metrics[index])
    }

    fn take(&mut self, slot: Slot) -> Option<&'a UsageMetric> {
        self.take_where(|candidate, _| candidate == slot)
    }

    /// 依次取这些槽位（同一槽位可出现多次，例如两种额外用量钱包）。
    fn take_all(&mut self, slots: &[Slot]) -> Vec<(Slot, &'a UsageMetric)> {
        let mut taken = Vec::new();
        for slot in slots {
            while let Some(metric) = self.take(*slot) {
                taken.push((*slot, metric));
            }
        }
        taken
    }

    /// 剩下的指标（未进匹配表，或厂商排布没用到）。
    fn rest(&self) -> impl Iterator<Item = &'a UsageMetric> + '_ {
        let metrics = self.metrics;
        self.taken
            .iter()
            .enumerate()
            .filter(|(_, taken)| !**taken)
            .map(move |(index, _)| &metrics[index])
    }
}

/// 数值项按三项一行排开。
fn push_stats<'a>(lines: &mut Vec<Line<'a>>, stats: Vec<Stat<'a>>) {
    let mut stats = stats.into_iter().peekable();
    while stats.peek().is_some() {
        lines.push(Line::Stats(stats.by_ref().take(3).collect()));
    }
}

/// 槽位 → 数值项，按三项一行排开；`section` 在两项及以上时加小节标题。
fn push_slot_stats<'a>(
    lines: &mut Vec<Line<'a>>,
    picked: Vec<(Slot, &'a UsageMetric)>,
    section: Option<&'static str>,
    cx: Cx,
) {
    if let Some(section) = section.filter(|_| picked.len() >= 2) {
        lines.push(Line::Section(Cow::Borrowed(section)));
    }
    push_stats(
        lines,
        picked
            .into_iter()
            .map(|(slot, metric)| slot_stat(slot, metric, cx))
            .collect(),
    );
}

/// 额度槽位依次画 meter。
fn push_quota_meters<'a>(
    lines: &mut Vec<Line<'a>>,
    picker: &mut Picker<'a>,
    slots: &[Slot],
    cx: Cx,
) {
    for (slot, metric) in picker.take_all(slots) {
        lines.push(Line::Meter(quota_meter(
            Cow::Borrowed(slot.label(cx.texts)),
            Some(slot),
            metric,
            cx,
        )));
    }
}

/// 会话上下文：占用 meter（说明里带 token / 窗口大小）。
fn push_context<'a>(lines: &mut Vec<Line<'a>>, picker: &mut Picker<'a>, cx: Cx) {
    if let Some(percent) = picker.take(Slot::ContextPercent) {
        let tokens = picker.take(Slot::ContextTokens);
        let window = picker.take(Slot::ContextWindow);
        lines.push(Line::Meter(context_meter(percent, tokens, window, cx)));
    }
}

/// claude：5 小时 / 每周 / 消费额度三条 meter（消费额度可超过 100%，画溢出态），
/// 之后是本会话的上下文占用与费用 / 时长。
fn claude<'a>(lines: &mut Vec<Line<'a>>, picker: &mut Picker<'a>, cx: Cx) {
    push_quota_meters(
        lines,
        picker,
        &[Slot::Quota5h, Slot::QuotaWeekly, Slot::QuotaSpend],
        cx,
    );
    push_context(lines, picker, cx);
    let session = picker.take_all(&[Slot::Cost, Slot::Duration, Slot::ApiDuration]);
    push_slot_stats(lines, session, None, cx);
}

/// codex：每个限额桶一组主 / 次窗口 meter 与额外余额；多个桶分小节。
fn codex<'a>(lines: &mut Vec<Line<'a>>, picker: &mut Picker<'a>, cx: Cx) {
    let mut buckets: Vec<&'a str> = Vec::new();
    for (metric, slot) in picker.metrics.iter().zip(&picker.slots) {
        if slot.is_some() {
            if let Some(bucket) = codex_bucket(&metric.id) {
                if !buckets.contains(&bucket) {
                    buckets.push(bucket);
                }
            }
        }
    }
    let sections = buckets.len() > 1;
    for bucket in buckets {
        let in_bucket = |slot: Slot, metric: &UsageMetric, wanted: Slot| {
            slot == wanted && codex_bucket(&metric.id) == Some(bucket)
        };
        let primary = picker.take_where(|slot, metric| in_bucket(slot, metric, Slot::QuotaPrimary));
        let secondary =
            picker.take_where(|slot, metric| in_bucket(slot, metric, Slot::QuotaSecondary));
        let credits = picker.take_where(|slot, metric| in_bucket(slot, metric, Slot::Credits));
        if sections {
            // 桶名取服务端标签 `{limitName} · 主要额度` 的前半，没有就用桶 id。
            let name = primary
                .or(secondary)
                .and_then(|metric| metric.label.split_once(" · ").map(|(name, _)| name))
                .unwrap_or(bucket);
            lines.push(Line::Section(Cow::Borrowed(name)));
        }
        for (slot, metric) in [
            (Slot::QuotaPrimary, primary),
            (Slot::QuotaSecondary, secondary),
        ] {
            if let Some(metric) = metric {
                let label = quota_label(Some(slot), metric, cx);
                lines.push(Line::Meter(quota_meter(label, Some(slot), metric, cx)));
            }
        }
        if let Some(credits) = credits {
            lines.push(Line::Stats(vec![slot_stat(Slot::Credits, credits, cx)]));
        }
    }
}

/// kimi：5 小时 / 7 天 / 月度（及其 Code 部分）/ 套餐额度 meter，余额一行，额外
/// 用量钱包一个小节。
fn kimi<'a>(lines: &mut Vec<Line<'a>>, picker: &mut Picker<'a>, cx: Cx) {
    push_quota_meters(
        lines,
        picker,
        &[
            Slot::Quota5h,
            Slot::Quota7d,
            Slot::QuotaMonthly,
            Slot::QuotaMonthlyCode,
            Slot::QuotaPlan,
        ],
        cx,
    );
    let balances = picker.take_all(&[
        Slot::BalanceAvailable,
        Slot::BalanceVoucher,
        Slot::BalanceCash,
    ]);
    push_slot_stats(lines, balances, None, cx);
    let extra = picker.take_all(&[
        Slot::ExtraBalance,
        Slot::ExtraMonthUsed,
        Slot::ExtraMonthCap,
        Slot::ExtraTotal,
    ]);
    if !extra.is_empty() {
        lines.push(Line::Section(Cow::Borrowed(cx.texts.section_extra_usage)));
    }
    push_slot_stats(lines, extra, None, cx);
}

/// opencode：本机会话统计——会话数 / 子 agent 会话 / 累计费用，token 五项只画
/// 数值（不是账号额度，不画 gauge）。
fn opencode<'a>(lines: &mut Vec<Line<'a>>, picker: &mut Picker<'a>, cx: Cx) {
    let top = picker.take_all(&[Slot::Sessions, Slot::SubagentSessions, Slot::Cost]);
    push_slot_stats(lines, top, None, cx);
    let tokens = picker.take_all(&[
        Slot::TokensInput,
        Slot::TokensOutput,
        Slot::TokensReasoning,
        Slot::TokensCacheRead,
        Slot::TokensCacheWrite,
    ]);
    push_slot_stats(lines, tokens, Some(cx.texts.section_tokens), cx);
}

/// pi：会话卡——上下文占用 gauge、本会话费用与模型、token 明细。
fn pi<'a>(lines: &mut Vec<Line<'a>>, picker: &mut Picker<'a>, cx: Cx) {
    push_context(lines, picker, cx);
    let head = picker.take_all(&[Slot::Cost, Slot::Model]);
    push_slot_stats(lines, head, None, cx);
    let tokens = picker.take_all(&[
        Slot::TokensInput,
        Slot::TokensOutput,
        Slot::TokensTotal,
        Slot::TokensCacheRead,
        Slot::TokensCacheWrite,
    ]);
    push_slot_stats(lines, tokens, Some(cx.texts.section_tokens), cx);
}

/// zcode：本地统计卡——主任务 / 子 agent / 合计 token，工具调用、子 agent 数与统计窗口。
fn zcode<'a>(lines: &mut Vec<Line<'a>>, picker: &mut Picker<'a>, cx: Cx) {
    let tokens = picker.take_all(&[Slot::TokensMain, Slot::TokensSubagents, Slot::TokensTotal]);
    push_slot_stats(lines, tokens, Some(cx.texts.section_tokens), cx);
    let counts = picker.take_all(&[Slot::ToolUses, Slot::Subagents, Slot::WindowHours]);
    push_slot_stats(lines, counts, None, cx);
}

/// 一个账号的卡片：状态行、说明行，然后按 `account.agent` 分派的厂商排布；排布
/// 没用到的指标逐条画通用行，没有任何可显示内容时画空态。
pub(super) fn build_card<'a>(
    account: &'a AccountUsageSnapshot,
    refresh: Option<&UsageRefreshState>,
    now_ms: u64,
) -> Card<'a> {
    let cx = Cx {
        now_ms,
        live: liveness(account.status),
        texts: &crate::i18n::texts().monitor,
    };
    let family = family(&account.agent);
    let mut lines = vec![Line::Meta];
    if let Some(message) = account
        .message
        .as_deref()
        .filter(|message| !message.is_empty())
    {
        // 服务端按它自己的语言写说明；认得的换成界面语言（文档终审 D2）。
        lines.push(Line::Note(crate::i18n::localize_usage_notice(message)));
    }
    if let Some(note) = refresh_note(refresh, now_ms) {
        lines.push(Line::Note(Cow::Owned(note)));
    }
    let noted = lines.len();
    let mut picker = Picker::new(account);
    match account.agent.as_str() {
        "claude" => claude(&mut lines, &mut picker, cx),
        "codex" => codex(&mut lines, &mut picker, cx),
        "kimi" => kimi(&mut lines, &mut picker, cx),
        "opencode" => opencode(&mut lines, &mut picker, cx),
        "pi" => pi(&mut lines, &mut picker, cx),
        "zcode" => zcode(&mut lines, &mut picker, cx),
        _ => {}
    }
    lines.extend(picker.rest().map(|metric| generic_line(metric, cx)));
    if lines.len() == noted && account.message.is_none() {
        lines.push(Line::Empty(if family == Family::Session {
            cx.texts.no_session_data
        } else {
            cx.texts.no_usage_data
        }));
    }
    Card {
        account,
        family,
        lines,
    }
}

/// 概览里的一个厂商：按账号在作用域里首次出现的顺序排。
pub(super) struct VendorGroup<'a> {
    pub agent: &'a str,
    pub accounts: Vec<&'a AccountUsageSnapshot>,
}

impl VendorGroup<'_> {
    /// 该厂商已加载账号里最需要注意的状态（与厂商 chip 的圆点同一口径）。
    pub(super) fn worst_status(&self) -> ObservationStatus {
        self.accounts
            .iter()
            .map(|account| account.status)
            .max_by_key(|status| status_severity(*status))
            .unwrap_or_default()
    }

    /// 最近一次观测时刻（0 = 从未更新）。
    pub(super) fn latest_ms(&self) -> u64 {
        self.accounts
            .iter()
            .map(|account| account.observed_at_ms)
            .max()
            .unwrap_or(0)
    }
}

pub(super) fn vendor_groups(accounts: &[AccountUsageSnapshot]) -> Vec<VendorGroup<'_>> {
    let mut groups: Vec<VendorGroup<'_>> = Vec::new();
    for account in accounts {
        match groups.iter_mut().find(|group| group.agent == account.agent) {
            Some(group) => group.accounts.push(account),
            None => groups.push(VendorGroup {
                agent: &account.agent,
                accounts: vec![account],
            }),
        }
    }
    groups
}

/// 额度槽位的显示标签：codex 主 / 次窗口知道长度时写「5h 窗口」，未进匹配表的
/// 指标用服务端标签。
fn quota_label<'a>(slot: Option<Slot>, metric: &'a UsageMetric, cx: Cx) -> Cow<'a, str> {
    slot_label(slot, metric, cx.texts)
}

/// 表格格式等只要一个指标名的地方（文档终审 D7）：认得的指标写界面语言的槽位名，
/// 认不出的沿用服务端标签——服务端标签随快照类型进了冻结摘要的线协议，旧客户端照原样
/// 显示，所以不在服务端改写，映射表就是卡片共用的这张匹配表。codex 的多个限额桶在
/// 表格里各占一行，桶名（服务端标签 `{limitName} · …` 的前半，没有就用桶 id）要保留，
/// 否则各桶的行分不开。
pub(super) fn metric_label<'a>(agent: &str, metric: &'a UsageMetric) -> Cow<'a, str> {
    let slot = slot_of(agent, metric);
    let label = slot_label(slot, metric, &crate::i18n::texts().monitor);
    if agent == "codex" && slot.is_some() {
        let bucket = metric
            .label
            .split_once(" · ")
            .map(|(name, _)| name)
            .or_else(|| codex_bucket(&metric.id));
        if let Some(bucket) = bucket {
            return Cow::Owned(format!("{bucket} · {label}"));
        }
    }
    label
}

fn slot_label<'a>(
    slot: Option<Slot>,
    metric: &'a UsageMetric,
    texts: &'static crate::i18n::MonitorTexts,
) -> Cow<'a, str> {
    match slot {
        Some(slot @ (Slot::QuotaPrimary | Slot::QuotaSecondary)) => metric
            .window_seconds
            .filter(|secs| *secs > 0)
            .map_or(Cow::Borrowed(slot.label(texts)), |secs| {
                Cow::Owned(crate::i18n::fill(
                    texts.window_fmt,
                    &[("span", &short_span(secs))],
                ))
            }),
        Some(slot) => Cow::Borrowed(slot.label(texts)),
        None => Cow::Borrowed(&metric.label),
    }
}

/// 概览紧凑卡的首行。额度厂商取所有账号里最紧张（未过期优先、已用比例最高）的
/// 一条额度 meter；pi 是上下文占用；本地统计是关键数值。什么都没有时是空态。
pub(super) fn headline<'a>(group: &VendorGroup<'a>, now_ms: u64) -> Line<'a> {
    let texts = &crate::i18n::texts().monitor;
    let cx_of = |account: &AccountUsageSnapshot| Cx {
        now_ms,
        live: liveness(account.status),
        texts,
    };
    let family = family(group.agent);
    if family == Family::Quota {
        let tightest = group
            .accounts
            .iter()
            .flat_map(|account| {
                account.metrics.iter().filter_map(move |metric| {
                    let slot = slot_of(&account.agent, metric);
                    if slot.is_some_and(|slot| slot.kind() != SlotKind::Quota) {
                        return None;
                    }
                    let percent = metric_percent(metric)?;
                    Some((*account, slot, metric, percent))
                })
            })
            .max_by(|a, b| {
                let key = |(_, _, metric, percent): &(_, _, &UsageMetric, f32)| {
                    (!window_expired(metric, now_ms), *percent)
                };
                let (a, b) = (key(a), key(b));
                a.0.cmp(&b.0).then(a.1.total_cmp(&b.1))
            });
        if let Some((account, slot, metric, _)) = tightest {
            let cx = cx_of(account);
            let label = quota_label(slot, metric, cx);
            let label = if group.accounts.len() > 1 {
                Cow::Owned(format!("{} · {label}", account.account_label))
            } else {
                label
            };
            return Line::Meter(quota_meter(label, slot, metric, cx));
        }
        // 没有额度窗口：退到第一条金额（credits / 余额）。
        let money = group.accounts.iter().find_map(|account| {
            account.metrics.iter().find_map(|metric| {
                slot_of(&account.agent, metric)
                    .filter(|slot| slot.kind() == SlotKind::Money)
                    .map(|slot| slot_stat(slot, metric, cx_of(account)))
            })
        });
        return money.map_or(Line::Empty(texts.no_usage_data), |stat| {
            Line::Stats(vec![stat])
        });
    }
    let wanted: &[Slot] = match group.agent {
        "zcode" => &[Slot::TokensTotal, Slot::Subagents],
        "pi" => &[Slot::Cost, Slot::TokensTotal],
        _ => &[Slot::Cost, Slot::Sessions],
    };
    for account in &group.accounts {
        let cx = cx_of(account);
        let mut picker = Picker::new(account);
        if family == Family::Session {
            let mut lines = Vec::new();
            push_context(&mut lines, &mut picker, cx);
            if let Some(line) = lines.pop() {
                return line;
            }
        }
        let stats = picker
            .take_all(wanted)
            .into_iter()
            .map(|(slot, metric)| slot_stat(slot, metric, cx))
            .collect::<Vec<_>>();
        if !stats.is_empty() {
            return Line::Stats(stats);
        }
    }
    Line::Empty(if family == Family::Session {
        texts.no_session_data
    } else {
        texts.no_usage_data
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW_MS: u64 = 1_800_000_000_000;
    const NOW_S: u64 = NOW_MS / 1000;

    fn metric(id: &str, scope: &str) -> UsageMetric {
        UsageMetric {
            id: id.into(),
            label: id.into(),
            unit: "%".into(),
            scope: scope.into(),
            ..Default::default()
        }
    }

    fn claude_account() -> AccountUsageSnapshot {
        AccountUsageSnapshot {
            agent: "claude".into(),
            status: ObservationStatus::Ready,
            metrics: vec![
                UsageMetric {
                    used_percent: Some(162.8),
                    resets_at: Some(NOW_S + 3600),
                    ..metric("spend_limit", "account")
                },
                UsageMetric {
                    used_percent: Some(91.0),
                    resets_at: Some(NOW_S - 60),
                    ..metric("seven_day", "account")
                },
                // 与 `parse::claude` 的真实输出一致：statusline 只有 used_percentage
                // 与 resets_at，没有窗口长度。
                UsageMetric {
                    used_percent: Some(42.0),
                    resets_at: Some(NOW_S + 3 * 3600),
                    ..metric("five_hour", "account")
                },
                metric("context_window/used_percentage", "session"),
                metric("cli-0", "account"),
            ],
            ..Default::default()
        }
    }

    #[test]
    fn number_formats_are_compact_and_never_hide_small_amounts() {
        assert_eq!(percent_text(42.0), "42%");
        assert_eq!(percent_text(162.8), "162.8%");
        assert_eq!(compact_count(950.0), "950");
        assert_eq!(compact_count(12_345.0), "12.3k");
        assert_eq!(compact_count(1_000_000.0), "1M");
        assert_eq!(compact_count(1_500_000.0), "1.5M");
        assert_eq!(money_text("1.234567", "USD"), "$1.23");
        assert_eq!(money_text("0.0042", "USD"), "$0.0042");
        assert_eq!(money_text("12345", "CNY cents"), "¥123.45");
        assert_eq!(money_text("12.5", "credits"), "12.50 credits");
        assert_eq!(money_text("n/a", "USD"), "$n/a");
        assert_eq!(short_span(5 * 3600), "5h");
        assert_eq!(short_span(7 * 86_400), "7d");
        assert_eq!(short_span(90 * 60), "90m");
    }

    /// claude 卡片：额度窗口按 5 小时 / 每周 / 消费额度的固定顺序（与报文顺序无关），
    /// 消费额度保留溢出比例，过期窗口灰 + DIM 并换成说明，上下文未知是「暂无数据」，
    /// 未进匹配表的指标落到末尾的通用行。
    #[test]
    fn claude_card_orders_windows_and_keeps_overflow_and_expiry() {
        let _guard = crate::i18n::lang_guard(crate::i18n::Lang::ZhCn);
        let account = claude_account();
        let card = build_card(&account, None, NOW_MS);
        let meters = card
            .lines
            .iter()
            .filter_map(|line| match line {
                Line::Meter(meter) => Some(meter),
                _ => None,
            })
            .collect::<Vec<_>>();
        let labels = meters
            .iter()
            .map(|meter| meter.label.as_ref())
            .collect::<Vec<_>>();
        assert_eq!(labels, ["5 小时", "每周", "消费额度", "上下文"]);
        let (five, weekly, spend, context) = (meters[0], meters[1], meters[2], meters[3]);
        assert!(!five.stale && !five.expired);
        assert!(
            five.window
                .is_some_and(|progress| (progress - 0.4).abs() < 0.01),
            "报文不带窗口长度时按名义 5 小时算：过了 2/5：{:?}",
            five.window
        );
        assert!(spend.window.is_none(), "消费额度周期不固定，不画刻度");
        assert!(
            spend.ratio.is_some_and(|ratio| ratio > 1.6),
            "溢出比例不截断"
        );
        assert_eq!(spend.value, "162.8%");
        assert!(weekly.expired && weekly.stale && weekly.window.is_none());
        assert!(weekly
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("已过重置")));
        assert_eq!(context.value, "暂无数据");
        assert!(context.stale && context.ratio.is_none());
        assert!(
            matches!(card.lines.last(), Some(Line::Stats(stats)) if stats[0].label == "cli-0"),
            "未进匹配表的指标落到通用行"
        );
    }

    /// 窗口长度：服务端给了 `window_seconds` 就用它，没给时退到槽位的名义长度；
    /// 两者都没有（或没有 `resets_at`）不画刻度。kimi 的 7 天窗口剩 3 天：过了 4/7。
    #[test]
    fn window_progress_prefers_the_reported_length_then_the_nominal_one() {
        let week = Some(7 * 86_400);
        let reported = UsageMetric {
            resets_at: Some(NOW_S + 3600),
            window_seconds: Some(2 * 3600),
            ..metric("codex/primary", "account")
        };
        let progress = window_progress(&reported, week, NOW_MS);
        assert!(progress.is_some_and(|progress| (progress - 0.5).abs() < 0.01));
        let bare = UsageMetric {
            resets_at: Some(NOW_S + 3 * 86_400),
            window_seconds: Some(0),
            ..metric("limit7d", "account")
        };
        let progress = window_progress(&bare, week, NOW_MS);
        assert!(
            progress.is_some_and(|progress| (progress - 4.0 / 7.0).abs() < 0.01),
            "{progress:?}"
        );
        assert_eq!(window_progress(&bare, None, NOW_MS), None);
        let no_reset = UsageMetric {
            resets_at: None,
            ..bare.clone()
        };
        assert_eq!(window_progress(&no_reset, week, NOW_MS), None);
        // kimi 卡片里 5 小时 / 7 天窗口据名义长度带刻度。
        let kimi = AccountUsageSnapshot {
            agent: "kimi".into(),
            status: ObservationStatus::Ready,
            metrics: vec![
                UsageMetric {
                    used_percent: Some(25.0),
                    resets_at: Some(NOW_S + 3600),
                    ..metric("limit5h", "account")
                },
                UsageMetric {
                    used_percent: Some(60.0),
                    ..bare
                },
            ],
            ..Default::default()
        };
        let card = build_card(&kimi, None, NOW_MS);
        let windows = card
            .lines
            .iter()
            .filter_map(|line| match line {
                Line::Meter(meter) => meter.window,
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(windows.len(), 2, "两条窗口都有刻度");
        assert!((windows[0] - 0.8).abs() < 0.01, "{windows:?}");
    }

    /// 失效账号（未登录等）的额度只画虚化占位（不给失效数据画确定的基线），
    /// 缓存数据保留量值但转灰。
    #[test]
    fn dead_and_cached_accounts_do_not_paint_live_bars() {
        let mut account = claude_account();
        account.status = ObservationStatus::NotAuthenticated;
        let card = build_card(&account, None, NOW_MS);
        let Some(Line::Meter(dead)) = card.lines.get(1) else {
            panic!("首条 meter");
        };
        assert!(dead.ratio.is_none() && dead.stale);
        account.status = ObservationStatus::Stale;
        let card = build_card(&account, None, NOW_MS);
        let Some(Line::Meter(cached)) = card.lines.get(1) else {
            panic!("首条 meter");
        };
        assert!(cached.ratio.is_some() && cached.stale);
    }

    /// 概览首行：取未过期窗口里最紧张的一条（过期窗口的旧值不抢首行）。
    #[test]
    fn overview_headline_prefers_the_tightest_live_window() {
        let _guard = crate::i18n::lang_guard(crate::i18n::Lang::ZhCn);
        let mut account = claude_account();
        account.metrics[0].used_percent = Some(50.0);
        let accounts = [account];
        let groups = vendor_groups(&accounts);
        let Line::Meter(meter) = headline(&groups[0], NOW_MS) else {
            panic!("额度厂商的首行是 meter");
        };
        assert_eq!(meter.label, "消费额度");
        assert_eq!(meter.value, "50%");
        // 本地统计厂商没有额度：首行是关键数值。
        let local = [AccountUsageSnapshot {
            agent: "opencode".into(),
            metrics: vec![UsageMetric {
                amount_decimal: Some("4.5".into()),
                unit: "USD".into(),
                ..metric("total_cost", "local")
            }],
            ..Default::default()
        }];
        let groups = vendor_groups(&local);
        let Line::Stats(stats) = headline(&groups[0], NOW_MS) else {
            panic!("本地统计的首行是数值项");
        };
        assert_eq!(stats[0].value, "$4.50");
    }
}
