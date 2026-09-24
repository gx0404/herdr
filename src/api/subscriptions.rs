use regex::Regex;

use crate::api::schema::{
    ErrorBody, ErrorResponse, EventGap, EventKind, EventStreamNotice, Method,
    PaneAgentStatusChangedEvent, PaneOutputMatchedEvent, PaneScrollChangedEvent, PaneScrollInfo,
    Request, StreamEventEnvelope, Subscription, SubscriptionEventData, SubscriptionEventEnvelope,
    SubscriptionEventKind,
};
use crate::api::server::{dispatch_to_app_with_timeout, APP_RESPONSE_TIMEOUT};
use crate::api::{ApiRequestSender, EventHub};

pub(super) fn output_match_read_source(
    source: &crate::api::schema::ReadSource,
) -> crate::api::schema::ReadSource {
    match source {
        crate::api::schema::ReadSource::Recent => crate::api::schema::ReadSource::RecentUnwrapped,
        other => *other,
    }
}

pub(super) fn match_output(
    text: &str,
    matcher: &crate::api::schema::OutputMatch,
    regex: Option<&Regex>,
) -> Option<String> {
    match matcher {
        crate::api::schema::OutputMatch::Substring { value } => text
            .lines()
            .find(|line| line.contains(value))
            .map(|line| line.to_string()),
        crate::api::schema::OutputMatch::Regex { .. } => regex.and_then(|re| {
            text.lines()
                .find(|line| re.is_match(line))
                .map(|line| line.to_string())
        }),
    }
}

pub(super) struct ActiveOutputMatchedSubscription {
    pane_id: String,
    source: crate::api::schema::ReadSource,
    lines: Option<u32>,
    matcher: crate::api::schema::OutputMatch,
    regex: Option<Regex>,
    strip_ansi: bool,
    currently_matching: bool,
    request_prefix: String,
}

pub(super) struct ActiveAgentStatusChangedSubscription {
    pane_id: String,
    status_filter: Option<crate::api::schema::AgentStatus>,
    last_status: Option<crate::api::schema::AgentStatus>,
    last_presentation: Option<PanePresentationSnapshot>,
    last_sequence: u64,
    initial_event: Option<PaneAgentStatusChangedEvent>,
    /// 本轮从 event hub 取到但调用方只要一条时的余量。游标已经推进过，
    /// 丢掉就不可恢复也不可察，所以留在这里等下次取。
    pending: std::collections::VecDeque<SubscriptionEventEnvelope>,
    request_prefix: String,
}

pub(super) struct ActiveScrollChangedSubscription {
    pane_id: String,
    last_scroll: Option<PaneScrollInfo>,
    request_prefix: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PanePresentationSnapshot {
    title: Option<String>,
    display_agent: Option<String>,
    state_labels: std::collections::HashMap<String, String>,
}

impl PanePresentationSnapshot {
    fn from(pane: &crate::api::schema::PaneInfo) -> Self {
        Self {
            title: pane.title.clone(),
            display_agent: pane.display_agent.clone(),
            state_labels: pane.state_labels.clone(),
        }
    }

    fn from_event(
        title: &Option<String>,
        display_agent: &Option<String>,
        state_labels: &std::collections::HashMap<String, String>,
    ) -> Self {
        Self {
            title: title.clone(),
            display_agent: display_agent.clone(),
            state_labels: state_labels.clone(),
        }
    }
}

/// 单轮投递：按订阅顺序收齐本轮全部事件，**每个订阅只 poll 一次**。
/// event-hub 支撑的订阅一次就取走本轮全部匹配事件（投递速率不再被轮询间隔
/// 钉死在 10 Hz，HSR-03）；pane 快照类订阅每次 poll 都是一次对 app/渲染线程的
/// 同步往返，重复 poll 补不回中间态，只会把请求数按轮数放大（乘法路径），
/// 所以这里不做内层 drain。
pub(super) fn poll_subscriptions_round(
    subscriptions: &mut [ActiveSubscription],
    api_tx: &ApiRequestSender,
    event_hub: &EventHub,
    notices: bool,
) -> Result<Vec<serde_json::Value>, ErrorBody> {
    let mut gaps: Vec<EventGap> = Vec::new();
    let mut events = Vec::new();
    for subscription in subscriptions.iter_mut() {
        let round = subscription.poll(api_tx, event_hub);
        if let Some(gap) = round.gap {
            // 没开 notices 的订阅方（含所有既有客户端）不能从保留窗口最旧处静默
            // 续传（上游 65927cef，#4178）：回 `events_lost` 错误并关闭本订阅，不先
            // 发本轮残缺的事件，也不再 poll 后面的订阅（快照类各是一次 app 往返）；
            // 由客户端重订阅并用 `session.snapshot` 重新同步。断层判据仍按订阅种别
            // 给（`EventHub::retained_after`），无关种别被挤掉不算丢失。
            if !notices {
                return Err(events_lost_error());
            }
            // 断层判据来自同一个 hub 游标窗口，同一连接上的多个订阅常常报出**完全
            // 相同**的区间。通知帧里没有订阅标识，重复下发客户端也无从区分，
            // 所以这里按区间去重：一轮一个连接对同一个区间只发一帧。
            if !gaps.contains(&gap) {
                gaps.push(gap);
            }
        }
        events.extend(round.events);
    }
    // 断层通知排在本轮全部事件之前：客户端先重拉快照，再应用幸存的增量。
    // `notices` 是订阅方显式开启的（`events.subscribe` 的 `notices: true`）：
    // 通知帧的 `event` 名不在 `EventKind` 里，不能无条件塞进既有的流。
    let mut round: Vec<serde_json::Value> = gaps.into_iter().filter_map(gap_notice_value).collect();
    round.extend(events);
    Ok(round)
}

fn events_lost_error() -> ErrorBody {
    ErrorBody {
        code: "events_lost".into(),
        message: "event subscription fell behind retained history; resubscribe and resync with session.snapshot".into(),
    }
}

/// 一个订阅在一轮里产出的东西：幸存事件帧，外加（可能的）断层。
/// 断层不由订阅自己拼进帧序列，而是交给 `poll_subscriptions_round` 按连接
/// 去重后统一下发。
pub(super) struct SubscriptionRound {
    pub(super) gap: Option<EventGap>,
    pub(super) events: Vec<serde_json::Value>,
}

impl SubscriptionRound {
    /// 快照类订阅：只有事件，没有 hub 游标也就没有断层。
    fn snapshot(event: Option<serde_json::Value>) -> Self {
        Self {
            gap: None,
            events: event.into_iter().collect(),
        }
    }
}

pub(super) struct ActiveEventSubscription {
    event_kind: crate::api::schema::EventKind,
    last_sequence: u64,
    /// 同 `ActiveAgentStatusChangedSubscription::pending`：单条语义的调用方
    /// 取走一条后，本轮余量留在队列里，不静默丢弃。
    pending: std::collections::VecDeque<serde_json::Value>,
}

pub(super) enum ActiveSubscription {
    Event(ActiveEventSubscription),
    OutputMatched(ActiveOutputMatchedSubscription),
    AgentStatusChanged(Box<ActiveAgentStatusChangedSubscription>),
    ScrollChanged(ActiveScrollChangedSubscription),
}

impl ActiveSubscription {
    pub(super) fn new(
        subscription: Subscription,
        request_id: &str,
        index: usize,
        api_tx: &ApiRequestSender,
        event_hub: &EventHub,
        event_start_sequence: u64,
    ) -> Result<Self, ErrorResponse> {
        let event_subscription = |event_kind| {
            Self::Event(ActiveEventSubscription {
                event_kind,
                last_sequence: event_start_sequence,
                pending: std::collections::VecDeque::new(),
            })
        };

        match subscription {
            Subscription::WorkspaceCreated {} => {
                Ok(event_subscription(EventKind::WorkspaceCreated))
            }
            Subscription::WorkspaceUpdated {} => {
                Ok(event_subscription(EventKind::WorkspaceUpdated))
            }
            Subscription::WorkspaceMetadataUpdated {} => {
                Ok(event_subscription(EventKind::WorkspaceMetadataUpdated))
            }
            Subscription::WorkspaceRenamed {} => {
                Ok(event_subscription(EventKind::WorkspaceRenamed))
            }
            Subscription::WorkspaceMoved {} => Ok(event_subscription(EventKind::WorkspaceMoved)),
            Subscription::WorkspaceReordered {} => {
                Ok(event_subscription(EventKind::WorkspaceReordered))
            }
            Subscription::WorkspaceClosed {} => Ok(event_subscription(EventKind::WorkspaceClosed)),
            Subscription::WorkspaceFocused {} => {
                Ok(event_subscription(EventKind::WorkspaceFocused))
            }
            Subscription::WorktreeCreated {} => Ok(event_subscription(EventKind::WorktreeCreated)),
            Subscription::WorktreeOpened {} => Ok(event_subscription(EventKind::WorktreeOpened)),
            Subscription::WorktreeRemoved {} => Ok(event_subscription(EventKind::WorktreeRemoved)),
            Subscription::TabCreated {} => Ok(event_subscription(EventKind::TabCreated)),
            Subscription::TabClosed {} => Ok(event_subscription(EventKind::TabClosed)),
            Subscription::TabFocused {} => Ok(event_subscription(EventKind::TabFocused)),
            Subscription::TabRenamed {} => Ok(event_subscription(EventKind::TabRenamed)),
            Subscription::TabMoved {} => Ok(event_subscription(EventKind::TabMoved)),
            Subscription::PaneCreated {} => Ok(event_subscription(EventKind::PaneCreated)),
            Subscription::PaneClosed {} => Ok(event_subscription(EventKind::PaneClosed)),
            Subscription::PaneUpdated {} => Ok(event_subscription(EventKind::PaneUpdated)),
            Subscription::PaneFocused {} => Ok(event_subscription(EventKind::PaneFocused)),
            Subscription::PaneMoved {} => Ok(event_subscription(EventKind::PaneMoved)),
            Subscription::PaneExited {} => Ok(event_subscription(EventKind::PaneExited)),
            Subscription::PaneAgentDetected {} => {
                Ok(event_subscription(EventKind::PaneAgentDetected))
            }
            Subscription::LayoutUpdated {} => Ok(event_subscription(EventKind::LayoutUpdated)),
            Subscription::PaneAgentActivityChanged {} => {
                Ok(event_subscription(EventKind::PaneAgentActivityChanged))
            }
            Subscription::PaneOutputMatched {
                pane_id,
                source,
                lines,
                r#match,
                strip_ansi,
            } => {
                let regex = match &r#match {
                    crate::api::schema::OutputMatch::Regex { value } => match Regex::new(value) {
                        Ok(regex) => Some(regex),
                        Err(err) => {
                            return Err(ErrorResponse {
                                id: request_id.to_string(),
                                error: ErrorBody {
                                    code: "invalid_regex".into(),
                                    message: err.to_string(),
                                },
                            });
                        }
                    },
                    crate::api::schema::OutputMatch::Substring { .. } => None,
                };

                let probe = pane_read(
                    format!("{request_id}:sub:{index}:probe"),
                    &pane_id,
                    source,
                    lines,
                    strip_ansi,
                    api_tx,
                );
                probe?;

                Ok(Self::OutputMatched(ActiveOutputMatchedSubscription {
                    pane_id,
                    source,
                    lines,
                    matcher: r#match,
                    regex,
                    strip_ansi,
                    currently_matching: false,
                    request_prefix: format!("{request_id}:sub:{index}"),
                }))
            }
            Subscription::PaneAgentStatusChanged {
                pane_id,
                agent_status,
            } => {
                let last_sequence = event_hub.current_sequence();
                let probe = pane_get(format!("{request_id}:sub:{index}:probe"), &pane_id, api_tx)?;
                let last_status = probe.agent_status;
                let last_presentation = PanePresentationSnapshot::from(&probe);
                let initial_event = agent_status
                    .is_some_and(|wanted| wanted == probe.agent_status)
                    .then_some(PaneAgentStatusChangedEvent {
                        pane_id: probe.pane_id.clone(),
                        workspace_id: probe.workspace_id,
                        agent_status: probe.agent_status,
                        agent: probe.agent,
                        title: probe.title,
                        display_agent: probe.display_agent,
                        state_labels: probe.state_labels,
                    });

                Ok(Self::AgentStatusChanged(Box::new(
                    ActiveAgentStatusChangedSubscription {
                        pane_id: probe.pane_id,
                        status_filter: agent_status,
                        last_status: Some(last_status),
                        last_presentation: Some(last_presentation),
                        last_sequence,
                        initial_event,
                        pending: std::collections::VecDeque::new(),
                        request_prefix: format!("{request_id}:sub:{index}"),
                    },
                )))
            }
            Subscription::PaneScrollChanged { pane_id } => {
                let probe = pane_get(format!("{request_id}:sub:{index}:probe"), &pane_id, api_tx)?;

                Ok(Self::ScrollChanged(ActiveScrollChangedSubscription {
                    pane_id: probe.pane_id,
                    last_scroll: probe.scroll,
                    request_prefix: format!("{request_id}:sub:{index}"),
                }))
            }
        }
    }

    /// 返回本轮该订阅的全部事件（可能为空）。返回 `Vec` 而不是 `Option` 是为了
    /// 去掉「每轮每订阅最多 1 条」的投递上限：调用方按 100 ms 轮询时，
    /// 上限等于 10 Hz，更高频的事件流会在环形缓冲里静默积压（HSR-03）。
    /// event hub 支撑的两类订阅（`Event` 与 `AgentStatusChanged` 的 hub 路径）
    /// 一次调用即取走本轮全部匹配事件，因此调用方不需要内层 drain 循环。
    /// 基于 pane 快照的订阅是边沿触发，一轮最多 1 条，且每轮只查一次快照。
    pub(super) fn poll(
        &mut self,
        api_tx: &ApiRequestSender,
        event_hub: &EventHub,
    ) -> SubscriptionRound {
        match self {
            Self::Event(subscription) => subscription.poll(event_hub),
            Self::OutputMatched(subscription) => SubscriptionRound::snapshot(
                subscription
                    .poll(api_tx)
                    .and_then(|event| serde_json::to_value(event).ok()),
            ),
            Self::AgentStatusChanged(subscription) => {
                let (events, gap) = subscription.poll_batch(api_tx, event_hub);
                SubscriptionRound {
                    gap,
                    events: events
                        .into_iter()
                        .filter_map(|event| serde_json::to_value(event).ok())
                        .collect(),
                }
            }
            Self::ScrollChanged(subscription) => SubscriptionRound::snapshot(
                subscription
                    .poll(api_tx)
                    .and_then(|event| serde_json::to_value(event).ok()),
            ),
        }
    }

    /// 单条语义（`*.wait`）：命中第一条即返回。这里**不得**写成
    /// 「`poll()` 取第一条、丢其余」——event hub 路径会把游标推到本轮最后一条，
    /// 被丢弃的事件不可恢复也不可察。两类 hub 订阅把余量留在自己的 `pending`
    /// 队列里，快照类订阅本来每轮最多 1 条。分支写穷尽，新增变体必须显式决定
    /// 是排队还是丢弃，不被通配符静默吞掉。
    pub(super) fn poll_for_wait(
        &mut self,
        api_tx: &ApiRequestSender,
        event_hub: &EventHub,
    ) -> Result<Option<serde_json::Value>, ErrorResponse> {
        match self {
            Self::Event(subscription) => Ok(subscription.poll_next(event_hub)),
            Self::AgentStatusChanged(subscription) => Ok(subscription
                .poll_result(api_tx, event_hub)?
                .and_then(|event| serde_json::to_value(event).ok())),
            Self::OutputMatched(subscription) => Ok(subscription
                .poll(api_tx)
                .and_then(|event| serde_json::to_value(event).ok())),
            Self::ScrollChanged(subscription) => Ok(subscription
                .poll(api_tx)
                .and_then(|event| serde_json::to_value(event).ok())),
        }
    }
}

impl ActiveEventSubscription {
    /// 一轮取走全部匹配事件。此前命中第一条就 return，投递速率被钉死在
    /// 1 条/订阅/轮（HSR-03）。断层交给调用方按连接去重后排在事件之前。
    fn poll(&mut self, event_hub: &EventHub) -> SubscriptionRound {
        let gap = self.refill(event_hub);
        SubscriptionRound {
            gap,
            events: self.pending.drain(..).collect(),
        }
    }

    /// 单条语义：只取一条，余量留在 `pending` 里等下次取。
    /// `*.wait` 路径**不保留也不下发**断层：调用方在等某一条具体事件，通知帧
    /// 混进返回值会被当成命中结果，而 `*.wait` 的响应形状里没有地方表达
    /// 「流断过」。断层在这里只记日志后丢弃——下发断层的唯一出口是订阅流。
    fn poll_next(&mut self, event_hub: &EventHub) -> Option<serde_json::Value> {
        if self.pending.is_empty() {
            if let Some(gap) = self.refill(event_hub) {
                tracing::warn!(
                    event_kind = self.event_kind.dot_name(),
                    gap_from = gap.from,
                    gap_to = gap.to,
                    "*.wait 路径出现事件断层：单条语义无处下发通知帧，已丢弃"
                );
            }
        }
        self.pending.pop_front()
    }

    /// 取本轮事件并返回（可能的）断层。断层判据按本订阅的 `event_kind` 给：
    /// 被挤掉的全是别的种别时不算断层，否则每次事件突发都会让低频订阅方做一次
    /// 无谓的全量快照重拉。
    fn refill(&mut self, event_hub: &EventHub) -> Option<EventGap> {
        let retained = event_hub.retained_after(self.last_sequence, Some(self.event_kind));
        if let Some(gap) = retained.gap {
            tracing::debug!(
                event_kind = self.event_kind.dot_name(),
                gap_from = gap.from,
                gap_to = gap.to,
                "事件订阅游标落在保留窗口之外，下发断层通知"
            );
        }
        for (sequence, event) in retained.events {
            self.last_sequence = sequence;
            if event.event != self.event_kind {
                continue;
            }
            if let Ok(value) = serde_json::to_value(StreamEventEnvelope::new(sequence, event)) {
                self.pending.push_back(value);
            }
        }
        retained.gap
    }
}

fn gap_notice_value(gap: EventGap) -> Option<serde_json::Value> {
    serde_json::to_value(EventStreamNotice::EventsLost(gap)).ok()
}

impl ActiveOutputMatchedSubscription {
    fn poll(&mut self, api_tx: &ApiRequestSender) -> Option<SubscriptionEventEnvelope> {
        let read = pane_read(
            format!("{}:read", self.request_prefix),
            &self.pane_id,
            output_match_read_source(&self.source),
            self.lines,
            self.strip_ansi,
            api_tx,
        )
        .ok()?;

        let matched_line = match_output(&read.text, &self.matcher, self.regex.as_ref());
        match matched_line {
            Some(matched_line) => {
                if self.currently_matching {
                    return None;
                }
                self.currently_matching = true;
                Some(SubscriptionEventEnvelope {
                    sequence: None,
                    event: SubscriptionEventKind::PaneOutputMatched,
                    data: SubscriptionEventData::PaneOutputMatched(PaneOutputMatchedEvent {
                        pane_id: read.pane_id.clone(),
                        matched_line,
                        read,
                    }),
                })
            }
            None => {
                self.currently_matching = false;
                None
            }
        }
    }
}

/// `refill_from_event_hub` 的结果：本轮是否见过本 pane 的状态事件，以及
/// 游标与保留窗口之间（针对本订阅种别）的断层。
struct HubRefill {
    saw_status_event: bool,
    gap: Option<EventGap>,
}

impl ActiveAgentStatusChangedSubscription {
    /// 本轮全部可投递事件。先批量取走 event hub 里命中的事件（廉价的内存读），
    /// hub 没有可投递事件时才回落到**一次** `pane_get`（一次 app/渲染线程同步
    /// 往返）。调用方每个轮询间隔只调一次：快照判定是边沿触发，重复调用补不回
    /// 中间态，只会把 app 往返次数按轮数放大（乘法路径，HSR-03 的代价面）。
    fn poll_batch(
        &mut self,
        api_tx: &ApiRequestSender,
        event_hub: &EventHub,
    ) -> (Vec<SubscriptionEventEnvelope>, Option<EventGap>) {
        let refill = self.refill_from_event_hub(event_hub);
        if !self.pending.is_empty() {
            return (self.pending.drain(..).collect(), refill.gap);
        }
        let events = self
            .poll_snapshot_fallback(api_tx, event_hub, refill.saw_status_event)
            .unwrap_or_default()
            .into_iter()
            .collect();
        (events, refill.gap)
    }

    /// 单条语义（`*.wait` 路径）：本轮余量留在 `pending` 里，下次调用继续取。
    fn poll_result(
        &mut self,
        api_tx: &ApiRequestSender,
        event_hub: &EventHub,
    ) -> Result<Option<SubscriptionEventEnvelope>, ErrorResponse> {
        if let Some(event) = self.pending.pop_front() {
            return Ok(Some(event));
        }
        let refill = self.refill_from_event_hub(event_hub);
        if let Some(gap) = refill.gap {
            // 同 `ActiveEventSubscription::poll_next`：单条语义无处下发通知帧。
            // 这条路径靠 `poll_snapshot_fallback` 的 `pane_get` 从快照补回终态，
            // 所以丢弃断层不会让等待方停在陈旧状态上。
            tracing::warn!(
                pane_id = %self.pane_id,
                gap_from = gap.from,
                gap_to = gap.to,
                "*.wait 路径出现事件断层：单条语义无处下发通知帧，改由快照兜底"
            );
        }
        if let Some(event) = self.pending.pop_front() {
            return Ok(Some(event));
        }
        self.poll_snapshot_fallback(api_tx, event_hub, refill.saw_status_event)
    }

    /// 把 event hub 中本轮命中本 pane 的事件全部收进 `pending`，返回是否见到过
    /// 本 pane 的状态事件（含被 `status_filter` 过滤掉的）以及（可能的）断层。
    /// 只读 event hub，不做任何 app 往返。断层判据按 `pane.agent_status_changed`
    /// 这一个种别给：别的种别被挤掉不影响本订阅。
    fn refill_from_event_hub(&mut self, event_hub: &EventHub) -> HubRefill {
        let mut saw_status_event = false;
        let retained = event_hub.retained_after(
            self.last_sequence,
            Some(crate::api::schema::EventKind::PaneAgentStatusChanged),
        );
        if let Some(gap) = retained.gap {
            tracing::debug!(
                pane_id = %self.pane_id,
                gap_from = gap.from,
                gap_to = gap.to,
                "pane 状态订阅游标落在保留窗口之外，下发断层通知"
            );
        }
        for (sequence, event) in retained.events {
            self.last_sequence = sequence;
            let crate::api::schema::EventData::PaneAgentStatusChanged {
                pane_id,
                workspace_id,
                agent_status,
                agent,
                title,
                display_agent,
                state_labels,
            } = event.data
            else {
                continue;
            };
            if event.event != crate::api::schema::EventKind::PaneAgentStatusChanged {
                continue;
            }
            if pane_id != self.pane_id {
                continue;
            }
            saw_status_event = true;

            let current_presentation =
                PanePresentationSnapshot::from_event(&title, &display_agent, &state_labels);
            self.last_status = Some(agent_status);
            self.last_presentation = Some(current_presentation);
            if self
                .status_filter
                .is_some_and(|wanted| wanted != agent_status)
            {
                continue;
            }

            self.pending.push_back(SubscriptionEventEnvelope {
                event: SubscriptionEventKind::PaneAgentStatusChanged,
                data: SubscriptionEventData::PaneAgentStatusChanged(PaneAgentStatusChangedEvent {
                    pane_id,
                    workspace_id,
                    agent_status,
                    agent,
                    title,
                    display_agent,
                    state_labels,
                }),
                sequence: Some(sequence),
            });
        }
        if saw_status_event {
            self.initial_event = None;
        }
        HubRefill {
            saw_status_event,
            gap: retained.gap,
        }
    }

    /// event hub 本轮没有可投递事件时的快照兜底：一次 `pane_get`。
    fn poll_snapshot_fallback(
        &mut self,
        api_tx: &ApiRequestSender,
        event_hub: &EventHub,
        saw_status_event: bool,
    ) -> Result<Option<SubscriptionEventEnvelope>, ErrorResponse> {
        if !saw_status_event {
            if event_hub.current_sequence() != self.last_sequence {
                return Ok(None);
            }
            if let Some(event) = self.initial_event.take() {
                return Ok(Some(SubscriptionEventEnvelope {
                    event: SubscriptionEventKind::PaneAgentStatusChanged,
                    data: SubscriptionEventData::PaneAgentStatusChanged(event),
                    sequence: None,
                }));
            }
        }

        let before_snapshot_sequence = self.last_sequence;
        let pane = pane_get(
            format!("{}:pane", self.request_prefix),
            &self.pane_id,
            api_tx,
        );
        let after_snapshot_sequence = event_hub.current_sequence();
        if after_snapshot_sequence != before_snapshot_sequence {
            return Ok(None);
        }
        let pane = pane?;

        let event = self.event_from_snapshot(pane);
        if event.is_some() {
            self.last_sequence = after_snapshot_sequence;
        }
        Ok(event)
    }

    fn event_from_snapshot(
        &mut self,
        pane: crate::api::schema::PaneInfo,
    ) -> Option<SubscriptionEventEnvelope> {
        let current_status = pane.agent_status;
        let current_presentation = PanePresentationSnapshot::from(&pane);
        let previous_status = self.last_status.replace(current_status);
        let previous_presentation = self.last_presentation.replace(current_presentation.clone());
        let presentation_changed = previous_presentation
            .as_ref()
            .is_some_and(|previous| previous != &current_presentation);
        let status_changed = previous_status.is_some_and(|previous| previous != current_status);
        if !(status_changed || presentation_changed) {
            return None;
        }
        if self
            .status_filter
            .is_some_and(|wanted| wanted != current_status)
        {
            return None;
        }

        Some(SubscriptionEventEnvelope {
            sequence: None,
            event: SubscriptionEventKind::PaneAgentStatusChanged,
            data: SubscriptionEventData::PaneAgentStatusChanged(PaneAgentStatusChangedEvent {
                pane_id: pane.pane_id,
                workspace_id: pane.workspace_id,
                agent_status: current_status,
                agent: pane.agent,
                title: pane.title,
                display_agent: pane.display_agent,
                state_labels: pane.state_labels,
            }),
        })
    }
}

impl ActiveScrollChangedSubscription {
    fn poll(&mut self, api_tx: &ApiRequestSender) -> Option<SubscriptionEventEnvelope> {
        let pane = pane_get(
            format!("{}:pane", self.request_prefix),
            &self.pane_id,
            api_tx,
        )
        .ok()?;
        self.event_from_snapshot(pane)
    }

    fn event_from_snapshot(
        &mut self,
        pane: crate::api::schema::PaneInfo,
    ) -> Option<SubscriptionEventEnvelope> {
        let scroll = pane.scroll;
        if self.last_scroll == scroll {
            return None;
        }
        self.last_scroll = scroll;
        let scroll = scroll?;

        Some(SubscriptionEventEnvelope {
            sequence: None,
            event: SubscriptionEventKind::ScrollChanged,
            data: SubscriptionEventData::ScrollChanged(PaneScrollChangedEvent {
                pane_id: pane.pane_id,
                workspace_id: pane.workspace_id,
                scroll,
            }),
        })
    }
}

fn pane_read(
    request_id: String,
    pane_id: &str,
    source: crate::api::schema::ReadSource,
    lines: Option<u32>,
    strip_ansi: bool,
    api_tx: &ApiRequestSender,
) -> Result<crate::api::schema::PaneReadResult, ErrorResponse> {
    let response = dispatch_to_app_with_timeout(
        Request {
            id: request_id.clone(),
            method: Method::PaneRead(crate::api::schema::PaneReadParams {
                pane_id: pane_id.to_string(),
                source,
                lines,
                format: crate::api::schema::ReadFormat::Text,
                strip_ansi,
                intent: crate::api::schema::ReadIntent::Passive,
            }),
        },
        api_tx,
        Some(APP_RESPONSE_TIMEOUT),
    );
    let value: serde_json::Value = serde_json::from_str(&response).map_err(|_| ErrorResponse {
        id: request_id.clone(),
        error: ErrorBody {
            code: "internal_error".into(),
            message: "failed to decode pane read response".into(),
        },
    })?;
    if value.get("error").is_some() {
        return serde_json::from_value(value).map_err(|_| ErrorResponse {
            id: request_id,
            error: ErrorBody {
                code: "internal_error".into(),
                message: "failed to decode pane read error".into(),
            },
        });
    }
    serde_json::from_value(value["result"]["read"].clone()).map_err(|_| ErrorResponse {
        id: request_id,
        error: ErrorBody {
            code: "internal_error".into(),
            message: "failed to decode pane read result".into(),
        },
    })
}

fn pane_get(
    request_id: String,
    pane_id: &str,
    api_tx: &ApiRequestSender,
) -> Result<crate::api::schema::PaneInfo, ErrorResponse> {
    let response = dispatch_to_app_with_timeout(
        Request {
            id: request_id.clone(),
            method: Method::PaneGet(crate::api::schema::PaneTarget {
                pane_id: pane_id.to_string(),
            }),
        },
        api_tx,
        Some(APP_RESPONSE_TIMEOUT),
    );
    let value: serde_json::Value = serde_json::from_str(&response).map_err(|_| ErrorResponse {
        id: request_id.clone(),
        error: ErrorBody {
            code: "internal_error".into(),
            message: "failed to decode pane get response".into(),
        },
    })?;
    if value.get("error").is_some() {
        let response =
            serde_json::from_value::<ErrorResponse>(value).map_err(|_| ErrorResponse {
                id: request_id,
                error: ErrorBody {
                    code: "internal_error".into(),
                    message: "failed to decode pane get error".into(),
                },
            })?;
        return Err(response);
    }
    serde_json::from_value(value["result"]["pane"].clone()).map_err(|_| ErrorResponse {
        id: request_id,
        error: ErrorBody {
            code: "internal_error".into(),
            message: "failed to decode pane get result".into(),
        },
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::api::schema::{AgentStatus, EventData, EventEnvelope, EventKind, PaneInfo};

    fn presentation_event(title: Option<&str>) -> EventEnvelope {
        EventEnvelope {
            event: EventKind::PaneAgentStatusChanged,
            data: EventData::PaneAgentStatusChanged {
                pane_id: "pane_1".into(),
                workspace_id: "workspace_1".into(),
                agent_status: AgentStatus::Working,
                agent: Some("pi".into()),
                title: title.map(str::to_string),
                display_agent: None,
                state_labels: HashMap::new(),
            },
        }
    }

    fn workspace_focused_event(workspace_id: &str) -> EventEnvelope {
        EventEnvelope {
            event: EventKind::WorkspaceFocused,
            data: EventData::WorkspaceFocused {
                workspace_id: workspace_id.into(),
            },
        }
    }

    fn pane_info_with_scroll(scroll: Option<PaneScrollInfo>) -> PaneInfo {
        PaneInfo {
            pane_id: "pane_1".into(),
            terminal_id: "terminal_1".into(),
            workspace_id: "workspace_1".into(),
            tab_id: "tab_1".into(),
            focused: true,
            cwd: None,
            foreground_cwd: None,
            restore_error: None,
            label: None,
            agent: None,
            title: None,
            terminal_title: None,
            terminal_title_stripped: None,
            display_agent: None,
            agent_status: AgentStatus::Unknown,
            state_labels: HashMap::new(),
            tokens: HashMap::new(),
            agent_session: None,
            scroll,
            revision: 0,
        }
    }

    #[test]
    fn lifecycle_subscription_skips_history_but_keeps_setup_window_events() {
        let event_hub = EventHub::default();
        event_hub.push(workspace_focused_event("before_subscription"));
        let event_start_sequence = event_hub.current_sequence();
        event_hub.push(workspace_focused_event("during_setup"));

        let (api_tx, _api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut subscription = ActiveSubscription::new(
            Subscription::WorkspaceFocused {},
            "test",
            0,
            &api_tx,
            &event_hub,
            event_start_sequence,
        )
        .expect("workspace focus subscription");

        let setup_event = subscription
            .poll(&api_tx, &event_hub)
            .events
            .into_iter()
            .next()
            .expect("setup-window event");
        assert_eq!(setup_event["data"]["workspace_id"], "during_setup");
        assert!(subscription
            .poll(&api_tx, &event_hub)
            .events
            .into_iter()
            .next()
            .is_none());

        event_hub.push(workspace_focused_event("after_setup"));
        let live_event = subscription
            .poll(&api_tx, &event_hub)
            .events
            .into_iter()
            .next()
            .expect("live event");
        assert_eq!(live_event["data"]["workspace_id"], "after_setup");
    }

    /// HSR-03 回归：一轮 poll 必须投递全部匹配事件。此前每轮只返回第一条，
    /// 100 ms 轮询把投递速率钉死在 1 条/订阅/100 ms（10 Hz），更高频的事件流
    /// 会在 512 条环形缓冲里静默积压并被挤掉。
    #[test]
    fn event_subscription_delivers_every_matching_event_in_one_poll() {
        let event_hub = EventHub::default();
        let (api_tx, _api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut subscription = ActiveSubscription::new(
            Subscription::WorkspaceFocused {},
            "test",
            0,
            &api_tx,
            &event_hub,
            event_hub.current_sequence(),
        )
        .expect("workspace focus subscription");

        for index in 0..50 {
            // 交错推入其它类型的事件，确认过滤与推进游标都不受影响。
            event_hub.push(presentation_event(Some("noise")));
            event_hub.push(workspace_focused_event(&format!("workspace_{index}")));
        }

        let delivered: Vec<_> = subscription.poll(&api_tx, &event_hub).events;
        assert_eq!(delivered.len(), 50, "一轮 poll 必须投递全部匹配事件");
        for (index, event) in delivered.iter().enumerate() {
            assert_eq!(event["data"]["workspace_id"], format!("workspace_{index}"));
        }
        assert!(
            subscription
                .poll(&api_tx, &event_hub)
                .events
                .into_iter()
                .next()
                .is_none(),
            "drain 干净后本轮不应再有事件"
        );
    }

    fn focus_subscription(
        api_tx: &ApiRequestSender,
        event_hub: &EventHub,
        start_sequence: u64,
    ) -> ActiveSubscription {
        ActiveSubscription::new(
            Subscription::WorkspaceFocused {},
            "test",
            0,
            api_tx,
            event_hub,
            start_sequence,
        )
        .expect("workspace focus subscription")
    }

    /// HSR-03/APP-006 回归：订阅游标被环形缓冲甩掉时，必须先下发
    /// `events.lost` 通知帧再补发保留窗口里的事件，客户端据此重拉快照。
    /// 此前被挤掉的事件无声无息，外部 supervisor 会把死掉的 agent 长期报健康。
    #[test]
    fn event_subscription_reports_the_gap_before_the_events_that_survived() {
        let event_hub = EventHub::default();
        let (api_tx, _api_rx) = tokio::sync::mpsc::unbounded_channel();
        // 游标停在 0：随后推入的事件多到把保留窗口整体推走。
        let mut subscriptions = vec![focus_subscription(&api_tx, &event_hub, 0)];

        let pushed = 600;
        for index in 0..pushed {
            event_hub.push(workspace_focused_event(&format!("workspace_{index}")));
        }

        let delivered =
            poll_subscriptions_round(&mut subscriptions, &api_tx, &event_hub, true).unwrap();
        let notice = delivered.first().expect("断层通知帧");
        assert_eq!(notice["event"], "events.lost");
        assert_eq!(notice["data"]["from"], 1);
        assert_eq!(notice["data"]["to"], (pushed - 512) as u64);
        assert_eq!(
            delivered.len(),
            513,
            "通知帧之后必须补发保留窗口里的全部事件"
        );
        assert_eq!(delivered[1]["data"]["workspace_id"], "workspace_88");

        // 断层只报一次：游标已经跟上，下一轮不再重复通知。
        event_hub.push(workspace_focused_event("fresh"));
        let next = poll_subscriptions_round(&mut subscriptions, &api_tx, &event_hub, true).unwrap();
        assert_eq!(next.len(), 1);
        assert_eq!(next[0]["data"]["workspace_id"], "fresh");
    }

    /// 通知帧是显式可选面：没开 `notices` 的订阅方（含所有既有客户端）不会在流上
    /// 见到 `events.lost` 这个 `EventKind` 之外的 `event` 名；遇到断层时改回上游的
    /// `events_lost` 错误（65927cef），且不先发本轮残缺的事件。
    #[test]
    fn gap_notices_stay_off_unless_the_subscriber_opts_in() {
        let event_hub = EventHub::default();
        let (api_tx, _api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut subscriptions = vec![focus_subscription(&api_tx, &event_hub, 0)];

        for index in 0..600 {
            event_hub.push(workspace_focused_event(&format!("workspace_{index}")));
        }

        let error = poll_subscriptions_round(&mut subscriptions, &api_tx, &event_hub, false)
            .expect_err("未开启 notices 的订阅方遇到断层收到 events_lost");
        assert_eq!(error.code, "events_lost");

        // 无关种别被挤掉不算丢失：订阅照常投递幸存事件，没有错误也没有通知帧。
        let event_hub = EventHub::with_test_capacity(4);
        let mut subscriptions = vec![focus_subscription(&api_tx, &event_hub, 0)];
        for index in 0..20 {
            event_hub.push(presentation_event(Some(&format!("noise_{index}"))));
        }
        event_hub.push(workspace_focused_event("kept"));
        let delivered =
            poll_subscriptions_round(&mut subscriptions, &api_tx, &event_hub, false).unwrap();
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0]["data"]["workspace_id"], "kept");
    }

    /// 同一连接上的多个订阅共用一个 hub 游标窗口，断层区间往往完全相同。
    /// 通知帧里没有订阅标识，重复下发客户端无从区分，所以一轮只发一帧。
    #[test]
    fn gap_notice_is_emitted_once_per_connection_round() {
        let event_hub = EventHub::default();
        let (api_tx, _api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut subscriptions = vec![
            focus_subscription(&api_tx, &event_hub, 0),
            focus_subscription(&api_tx, &event_hub, 0),
        ];

        for index in 0..600 {
            event_hub.push(workspace_focused_event(&format!("workspace_{index}")));
        }

        let delivered =
            poll_subscriptions_round(&mut subscriptions, &api_tx, &event_hub, true).unwrap();
        let notices = delivered
            .iter()
            .filter(|frame| frame["event"] == "events.lost")
            .count();
        assert_eq!(notices, 1, "区间相同的断层每轮每连接只发一帧");
        assert_eq!(delivered.len(), 1 + 512 * 2);
    }

    /// 断层判据按订阅种别给：被挤掉的事件与订阅无关时不得报断层——否则每次
    /// 事件突发都会让所有低频订阅方做一次无谓的 `session.snapshot` 全量重拉，
    /// 代价按「连接 × 订阅」放大，恰好落在 15 pane 并发翻转这种目标工况里。
    #[test]
    fn unrelated_event_evictions_do_not_report_a_gap() {
        let event_hub = EventHub::with_test_capacity(4);
        let (api_tx, _api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut subscriptions = vec![focus_subscription(&api_tx, &event_hub, 0)];

        // 只有 `pane.agent_status_changed` 噪声滚过窗口，订阅方关心的
        // `workspace.focused` 一条都没丢。
        for index in 0..20 {
            event_hub.push(presentation_event(Some(&format!("noise_{index}"))));
        }

        let delivered =
            poll_subscriptions_round(&mut subscriptions, &api_tx, &event_hub, true).unwrap();
        assert!(
            delivered.is_empty(),
            "与订阅无关的事件被挤掉不构成这条订阅的断层：{delivered:?}"
        );

        // 轮到本种别真被挤掉时仍然要报。
        for index in 0..20 {
            event_hub.push(workspace_focused_event(&format!("workspace_{index}")));
        }
        let delivered =
            poll_subscriptions_round(&mut subscriptions, &api_tx, &event_hub, true).unwrap();
        assert_eq!(delivered[0]["event"], "events.lost");
    }

    /// 事件帧携带序号：客户端可以据此对账并在重连后续流。
    #[test]
    fn event_subscription_frames_carry_the_server_sequence() {
        let event_hub = EventHub::default();
        let (api_tx, _api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut subscription = ActiveSubscription::new(
            Subscription::WorkspaceFocused {},
            "test",
            0,
            &api_tx,
            &event_hub,
            event_hub.current_sequence(),
        )
        .expect("workspace focus subscription");

        event_hub.push(presentation_event(Some("noise")));
        event_hub.push(workspace_focused_event("first"));
        event_hub.push(workspace_focused_event("second"));

        let delivered = subscription.poll(&api_tx, &event_hub).events;
        assert_eq!(delivered.len(), 2);
        assert_eq!(delivered[0]["sequence"], 2);
        assert_eq!(delivered[1]["sequence"], 3);
        // 形状纯追加：旧客户端按 `EventEnvelope` 解码仍然成立。
        let legacy: crate::api::schema::EventEnvelope =
            serde_json::from_value(delivered[0].clone()).expect("旧形状解码必须宽松");
        assert_eq!(legacy.event, EventKind::WorkspaceFocused);
    }

    /// `*.wait` 是单条语义：通知帧不得混进结果里被当成命中的事件。
    #[test]
    fn wait_path_never_returns_the_gap_notice_as_a_match() {
        let event_hub = EventHub::default();
        let (api_tx, _api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut subscription = ActiveSubscription::new(
            Subscription::WorkspaceFocused {},
            "test",
            0,
            &api_tx,
            &event_hub,
            0,
        )
        .expect("workspace focus subscription");

        for index in 0..600 {
            event_hub.push(workspace_focused_event(&format!("workspace_{index}")));
        }

        let first = subscription
            .poll_for_wait(&api_tx, &event_hub)
            .expect("poll for wait")
            .expect("第一条命中事件");
        assert_eq!(first["event"], "workspace_focused");
        assert_eq!(first["data"]["workspace_id"], "workspace_88");
    }

    /// pane 状态订阅同样要能感知断层，并给 hub 派生的帧带上序号；
    /// APP-006 的观测面之一就是 `pane.agent_status_changed` 被静默丢弃。
    #[test]
    fn agent_status_subscription_reports_the_gap_and_tags_hub_events() {
        let event_hub = EventHub::default();
        let (api_tx, _api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut subscriptions = vec![ActiveSubscription::AgentStatusChanged(Box::new(
            ActiveAgentStatusChangedSubscription {
                pane_id: "pane_1".into(),
                status_filter: None,
                last_status: Some(AgentStatus::Working),
                last_presentation: Some(PanePresentationSnapshot {
                    title: None,
                    display_agent: None,
                    state_labels: HashMap::new(),
                }),
                last_sequence: 0,
                initial_event: None,
                pending: std::collections::VecDeque::new(),
                request_prefix: "test".into(),
            },
        ))];

        for index in 0..600 {
            event_hub.push(presentation_event(Some(&format!("title_{index}"))));
        }

        let delivered =
            poll_subscriptions_round(&mut subscriptions, &api_tx, &event_hub, true).unwrap();
        let notice = delivered.first().expect("断层通知帧");
        assert_eq!(notice["event"], "events.lost");
        assert_eq!(notice["data"]["from"], 1);
        assert_eq!(notice["data"]["to"], 88);
        assert_eq!(delivered[1]["sequence"], 89);
        assert_eq!(delivered[1]["data"]["title"], "title_88");
    }

    /// 单轮按订阅顺序收齐所有订阅的事件；每个订阅只 poll 一次，
    /// `stream_subscriptions` 把这一轮的结果写完就 sleep。
    #[test]
    fn subscription_round_collects_events_from_every_subscription() {
        let event_hub = EventHub::default();
        let (api_tx, _api_rx) = tokio::sync::mpsc::unbounded_channel();
        let new_subscription = || {
            ActiveSubscription::new(
                Subscription::WorkspaceFocused {},
                "test",
                0,
                &api_tx,
                &event_hub,
                event_hub.current_sequence(),
            )
            .expect("workspace focus subscription")
        };
        let mut subscriptions = vec![new_subscription(), new_subscription()];

        event_hub.push(workspace_focused_event("first"));
        event_hub.push(workspace_focused_event("second"));

        let round =
            poll_subscriptions_round(&mut subscriptions, &api_tx, &event_hub, true).unwrap();
        assert_eq!(round.len(), 4, "两个订阅各投递两条事件");
        assert_eq!(round[0]["data"]["workspace_id"], "first");
        assert_eq!(round[1]["data"]["workspace_id"], "second");
        assert_eq!(round[2]["data"]["workspace_id"], "first");
        assert_eq!(round[3]["data"]["workspace_id"], "second");

        assert!(
            poll_subscriptions_round(&mut subscriptions, &api_tx, &event_hub, true)
                .unwrap()
                .is_empty(),
            "无新事件时本轮为空"
        );
    }

    /// `pane.agent_activity_changed` 是无参订阅：走通用事件投递，订阅者按事件里的
    /// `pane_id` 自行过滤。
    #[test]
    fn agent_activity_subscription_registers_as_a_plain_event_subscription() {
        let event_hub = EventHub::default();
        let (api_tx, _api_rx) = tokio::sync::mpsc::unbounded_channel();
        let subscription = ActiveSubscription::new(
            Subscription::PaneAgentActivityChanged {},
            "test",
            0,
            &api_tx,
            &event_hub,
            event_hub.current_sequence(),
        )
        .expect("agent activity subscription");

        assert!(matches!(
            subscription,
            ActiveSubscription::Event(ActiveEventSubscription {
                event_kind: EventKind::PaneAgentActivityChanged,
                ..
            })
        ));
        assert_eq!(
            serde_json::from_str::<Subscription>(r#"{"type":"pane.agent_activity_changed"}"#).ok(),
            Some(Subscription::PaneAgentActivityChanged {}),
        );
    }

    #[test]
    fn workspace_metadata_subscription_uses_dedicated_event_kind() {
        let event_hub = EventHub::default();
        let (api_tx, _api_rx) = tokio::sync::mpsc::unbounded_channel();
        let subscription = ActiveSubscription::new(
            Subscription::WorkspaceMetadataUpdated {},
            "test",
            0,
            &api_tx,
            &event_hub,
            event_hub.current_sequence(),
        )
        .expect("workspace metadata subscription");

        assert!(matches!(
            subscription,
            ActiveSubscription::Event(ActiveEventSubscription {
                event_kind: EventKind::WorkspaceMetadataUpdated,
                ..
            })
        ));
    }

    #[test]
    fn scroll_subscription_emits_when_scroll_snapshot_changes() {
        let at_bottom = PaneScrollInfo {
            offset_from_bottom: 0,
            max_offset_from_bottom: 40,
            viewport_rows: 20,
        };
        let scrolled_back = PaneScrollInfo {
            offset_from_bottom: 8,
            max_offset_from_bottom: 40,
            viewport_rows: 20,
        };
        let mut subscription = ActiveScrollChangedSubscription {
            pane_id: "pane_1".into(),
            last_scroll: Some(at_bottom),
            request_prefix: "test".into(),
        };

        assert!(subscription
            .event_from_snapshot(pane_info_with_scroll(Some(at_bottom)))
            .is_none());

        let event = subscription
            .event_from_snapshot(pane_info_with_scroll(Some(scrolled_back)))
            .expect("scroll event");
        assert_eq!(event.event, SubscriptionEventKind::ScrollChanged);
        let SubscriptionEventData::ScrollChanged(data) = event.data else {
            panic!("wrong event data");
        };
        assert_eq!(data.pane_id, "pane_1");
        assert_eq!(data.workspace_id, "workspace_1");
        assert_eq!(data.scroll, scrolled_back);
    }

    #[test]
    fn agent_status_subscription_replays_queued_metadata_set_and_expiry_events() {
        let event_hub = EventHub::default();
        let mut subscription = ActiveAgentStatusChangedSubscription {
            pane_id: "pane_1".into(),
            status_filter: None,
            last_status: Some(AgentStatus::Working),
            last_presentation: Some(PanePresentationSnapshot {
                title: None,
                display_agent: None,
                state_labels: HashMap::new(),
            }),
            last_sequence: event_hub.current_sequence(),
            initial_event: None,
            pending: std::collections::VecDeque::new(),
            request_prefix: "test".into(),
        };

        event_hub.push(presentation_event(Some("short lived")));
        event_hub.push(presentation_event(None));

        let set_event = subscription
            .poll_result(&tokio::sync::mpsc::unbounded_channel().0, &event_hub)
            .expect("poll set event")
            .expect("set event");
        let SubscriptionEventData::PaneAgentStatusChanged(set_data) = set_event.data else {
            panic!("wrong event data");
        };
        assert_eq!(set_data.title.as_deref(), Some("short lived"));

        let expiry_event = subscription
            .poll_result(&tokio::sync::mpsc::unbounded_channel().0, &event_hub)
            .expect("poll expiry event")
            .expect("expiry event");
        let SubscriptionEventData::PaneAgentStatusChanged(expiry_data) = expiry_event.data else {
            panic!("wrong event data");
        };
        assert_eq!(expiry_data.title, None);
    }

    /// HSR-03 回归（偏差 2 的直接证据）：`pane.agent_status_changed` 订阅
    /// 走的是 `ActiveAgentStatusChangedSubscription`，此前它命中第一条就
    /// `return`，一轮只产 1 条，只能靠调用方的内层 drain 循环补齐——而那个
    /// drain 循环会连带重跑 pane 快照类订阅，把 app 往返按轮数放大。
    /// 现在 hub 路径一次取走本轮全部命中事件，单轮即可脱离 10 Hz 上限，
    /// 调用方不再需要内层 drain。
    #[test]
    fn agent_status_subscription_delivers_every_hub_event_in_one_poll() {
        let event_hub = EventHub::default();
        // 这个 sender 没有对端：一旦回落到 `pane_get` 快照兜底就会超时，
        // 所以本测试同时钉住「hub 有事件时不做任何 app 往返」。
        let (api_tx, _api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut subscription = ActiveSubscription::AgentStatusChanged(Box::new(
            ActiveAgentStatusChangedSubscription {
                pane_id: "pane_1".into(),
                status_filter: None,
                last_status: Some(AgentStatus::Working),
                last_presentation: Some(PanePresentationSnapshot {
                    title: None,
                    display_agent: None,
                    state_labels: HashMap::new(),
                }),
                last_sequence: event_hub.current_sequence(),
                initial_event: None,
                pending: std::collections::VecDeque::new(),
                request_prefix: "test".into(),
            },
        ));

        for index in 0..3 {
            event_hub.push(presentation_event(Some(&format!("title_{index}"))));
        }

        let started = std::time::Instant::now();
        let delivered = subscription.poll(&api_tx, &event_hub).events;
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "hub 有事件时不得回落到 pane_get 快照兜底"
        );
        assert_eq!(
            delivered.len(),
            3,
            "一轮 poll 必须投递 hub 里的全部命中事件"
        );
        for (index, event) in delivered.iter().enumerate() {
            assert_eq!(event["data"]["title"], format!("title_{index}"));
        }
    }

    /// 等待语义（`*.wait`）每次只取一条，但不得把同轮余量连同游标一起丢掉：
    /// `events_after` 的游标已经推进，被丢弃的事件不可恢复也不可察。
    #[test]
    fn wait_path_keeps_the_rest_of_the_round_instead_of_dropping_it() {
        let event_hub = EventHub::default();
        let (api_tx, _api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut subscription = ActiveSubscription::new(
            Subscription::WorkspaceFocused {},
            "test",
            0,
            &api_tx,
            &event_hub,
            event_hub.current_sequence(),
        )
        .expect("workspace focus subscription");

        for index in 0..3 {
            event_hub.push(workspace_focused_event(&format!("workspace_{index}")));
        }

        for index in 0..3 {
            let event = subscription
                .poll_for_wait(&api_tx, &event_hub)
                .expect("poll for wait")
                .unwrap_or_else(|| panic!("第 {index} 条事件被静默丢弃"));
            assert_eq!(event["data"]["workspace_id"], format!("workspace_{index}"));
        }

        assert!(subscription
            .poll_for_wait(&api_tx, &event_hub)
            .expect("poll for wait")
            .is_none());
    }

    #[test]
    fn agent_status_subscription_prefers_setup_window_events_over_initial_snapshot() {
        let event_hub = EventHub::default();
        let mut subscription = ActiveAgentStatusChangedSubscription {
            pane_id: "pane_1".into(),
            status_filter: Some(AgentStatus::Working),
            last_status: Some(AgentStatus::Working),
            last_presentation: Some(PanePresentationSnapshot {
                title: None,
                display_agent: None,
                state_labels: HashMap::new(),
            }),
            last_sequence: event_hub.current_sequence(),
            initial_event: Some(PaneAgentStatusChangedEvent {
                pane_id: "pane_1".into(),
                workspace_id: "workspace_1".into(),
                agent_status: AgentStatus::Working,
                agent: Some("pi".into()),
                title: None,
                display_agent: None,
                state_labels: HashMap::new(),
            }),
            pending: std::collections::VecDeque::new(),
            request_prefix: "test".into(),
        };

        event_hub.push(presentation_event(Some("short lived")));
        event_hub.push(presentation_event(None));

        let set_event = subscription
            .poll_result(&tokio::sync::mpsc::unbounded_channel().0, &event_hub)
            .expect("poll set event")
            .expect("set event");
        let SubscriptionEventData::PaneAgentStatusChanged(set_data) = set_event.data else {
            panic!("wrong event data");
        };
        assert_eq!(set_data.title.as_deref(), Some("short lived"));

        let expiry_event = subscription
            .poll_result(&tokio::sync::mpsc::unbounded_channel().0, &event_hub)
            .expect("poll expiry event")
            .expect("expiry event");
        let SubscriptionEventData::PaneAgentStatusChanged(expiry_data) = expiry_event.data else {
            panic!("wrong event data");
        };
        assert_eq!(expiry_data.title, None);
    }

    #[test]
    fn agent_status_subscription_emits_setup_window_event_already_reflected_by_probe() {
        let event_hub = EventHub::default();
        let mut subscription = ActiveAgentStatusChangedSubscription {
            pane_id: "pane_1".into(),
            status_filter: Some(AgentStatus::Working),
            last_status: Some(AgentStatus::Working),
            last_presentation: Some(PanePresentationSnapshot {
                title: Some("short lived".into()),
                display_agent: None,
                state_labels: HashMap::new(),
            }),
            last_sequence: event_hub.current_sequence(),
            initial_event: Some(PaneAgentStatusChangedEvent {
                pane_id: "pane_1".into(),
                workspace_id: "workspace_1".into(),
                agent_status: AgentStatus::Working,
                agent: Some("pi".into()),
                title: Some("short lived".into()),
                display_agent: None,
                state_labels: HashMap::new(),
            }),
            pending: std::collections::VecDeque::new(),
            request_prefix: "test".into(),
        };

        event_hub.push(presentation_event(Some("short lived")));

        let event = subscription
            .poll_result(&tokio::sync::mpsc::unbounded_channel().0, &event_hub)
            .expect("poll setup-window event")
            .expect("setup-window event");
        let SubscriptionEventData::PaneAgentStatusChanged(data) = event.data else {
            panic!("wrong event data");
        };
        assert_eq!(data.title.as_deref(), Some("short lived"));
        assert!(subscription.initial_event.is_none());
    }

    /// 上游 65927cef：生命周期订阅一轮取完保留批次，按序投递并越过不匹配的事件。
    #[test]
    fn lifecycle_batch_drains_in_order_and_advances_past_unmatched_events() {
        let event_hub = EventHub::default();
        event_hub.push(workspace_focused_event("old"));
        let start = event_hub.current_sequence();
        event_hub.push(workspace_focused_event("setup"));
        let (api_tx, _api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut subscription = ActiveSubscription::new(
            Subscription::WorkspaceFocused {},
            "batch",
            0,
            &api_tx,
            &event_hub,
            start,
        )
        .unwrap();
        event_hub.push(presentation_event(None));
        event_hub.push(workspace_focused_event("live"));
        let events = subscription.poll(&api_tx, &event_hub).events;
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["data"]["workspace_id"], "setup");
        assert_eq!(events[1]["data"]["workspace_id"], "live");
        assert!(subscription.poll(&api_tx, &event_hub).events.is_empty());
        let ActiveSubscription::Event(subscription) = subscription else {
            panic!("expected lifecycle subscription");
        };
        assert_eq!(subscription.last_sequence, event_hub.current_sequence());
    }

    /// 上游 65927cef：状态订阅一轮保留全部转换、过滤与初始快照的先后。
    #[test]
    fn agent_status_batch_preserves_transitions_filters_and_initial_state_ordering() {
        for filtered in [false, true] {
            let event_hub = EventHub::default();
            let mut subscription = ActiveSubscription::AgentStatusChanged(Box::new(
                ActiveAgentStatusChangedSubscription {
                    pane_id: "pane_1".into(),
                    status_filter: filtered.then_some(AgentStatus::Working),
                    last_status: Some(AgentStatus::Working),
                    last_presentation: None,
                    last_sequence: event_hub.current_sequence(),
                    initial_event: Some(PaneAgentStatusChangedEvent {
                        pane_id: "pane_1".into(),
                        workspace_id: "workspace_1".into(),
                        agent_status: AgentStatus::Working,
                        agent: Some("pi".into()),
                        title: Some("stale initial snapshot".into()),
                        display_agent: None,
                        state_labels: HashMap::new(),
                    }),
                    pending: std::collections::VecDeque::new(),
                    request_prefix: "batch".into(),
                },
            ));
            for (status, title) in [
                (AgentStatus::Working, "started"),
                (AgentStatus::Blocked, "approval"),
                (AgentStatus::Idle, "finished"),
                (AgentStatus::Working, "restarted"),
            ] {
                let mut event = presentation_event(Some(title));
                let EventData::PaneAgentStatusChanged { agent_status, .. } = &mut event.data else {
                    panic!("expected status data");
                };
                *agent_status = status;
                event_hub.push(event);
            }
            let (api_tx, _api_rx) = tokio::sync::mpsc::unbounded_channel();
            let events = subscription.poll(&api_tx, &event_hub).events;
            let titles = events
                .iter()
                .map(|event| event["data"]["title"].as_str().unwrap())
                .collect::<Vec<_>>();
            assert_eq!(
                titles,
                if filtered {
                    vec!["started", "restarted"]
                } else {
                    vec!["started", "approval", "finished", "restarted"]
                }
            );
            let ActiveSubscription::AgentStatusChanged(subscription) = subscription else {
                panic!("expected agent subscription");
            };
            assert_eq!(subscription.last_sequence, event_hub.current_sequence());
            assert!(subscription.initial_event.is_none());
        }
    }
}
