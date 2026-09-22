use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::common::{AgentStatus, ReadSource};
use super::panes::{PaneInfo, PaneReadResult, PaneScrollInfo};
use super::tabs::TabInfo;
use super::workspaces::WorkspaceInfo;
use super::worktrees::WorktreeInfo;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct EventsSubscribeParams {
    pub subscriptions: Vec<Subscription>,
    /// 是否在这条流上下发**通知帧**（目前只有 `events.lost`）。通知帧的
    /// `event` 名不在 `EventKind` 里，严格按 `event` 条目解码每一行的客户端会
    /// 解不开，所以默认关闭、由调用方显式开启（HSR-03/APP-006）。
    /// 省略即 `false`，线上形状与旧请求逐字节一致。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub notices: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type")]
pub enum Subscription {
    #[serde(rename = "workspace.created")]
    WorkspaceCreated {},
    #[serde(rename = "workspace.updated")]
    WorkspaceUpdated {},
    #[serde(rename = "workspace.metadata_updated")]
    WorkspaceMetadataUpdated {},
    #[serde(rename = "workspace.renamed")]
    WorkspaceRenamed {},
    #[serde(rename = "workspace.moved")]
    WorkspaceMoved {},
    #[serde(rename = "workspace.reordered")]
    WorkspaceReordered {},
    #[serde(rename = "workspace.closed")]
    WorkspaceClosed {},
    #[serde(rename = "workspace.focused")]
    WorkspaceFocused {},
    #[serde(rename = "worktree.created")]
    WorktreeCreated {},
    #[serde(rename = "worktree.opened")]
    WorktreeOpened {},
    #[serde(rename = "worktree.removed")]
    WorktreeRemoved {},
    #[serde(rename = "tab.created")]
    TabCreated {},
    #[serde(rename = "tab.closed")]
    TabClosed {},
    #[serde(rename = "tab.focused")]
    TabFocused {},
    #[serde(rename = "tab.renamed")]
    TabRenamed {},
    #[serde(rename = "tab.moved")]
    TabMoved {},
    #[serde(rename = "pane.created")]
    PaneCreated {},
    #[serde(rename = "pane.closed")]
    PaneClosed {},
    #[serde(rename = "pane.updated")]
    PaneUpdated {},
    #[serde(rename = "pane.focused")]
    PaneFocused {},
    #[serde(rename = "pane.moved")]
    PaneMoved {},
    #[serde(rename = "pane.exited")]
    PaneExited {},
    #[serde(rename = "pane.agent_detected")]
    PaneAgentDetected {},
    #[serde(rename = "pane.output_matched")]
    PaneOutputMatched {
        pane_id: String,
        source: ReadSource,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lines: Option<u32>,
        r#match: OutputMatch,
        #[serde(default = "super::common::default_true")]
        strip_ansi: bool,
    },
    #[serde(rename = "pane.agent_status_changed")]
    PaneAgentStatusChanged {
        pane_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_status: Option<AgentStatus>,
    },
    #[serde(rename = "pane.scroll_changed")]
    PaneScrollChanged { pane_id: String },
    #[serde(rename = "layout.updated")]
    LayoutUpdated {},
    /// 无参：订阅者按事件里的 `pane_id` 自行过滤（投递侧不按 pane 过滤，宣告了
    /// 承重参数却不生效才是问题）。
    #[serde(rename = "pane.agent_activity_changed")]
    PaneAgentActivityChanged {},
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct EventsWaitParams {
    pub match_event: EventMatch,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PaneWaitForOutputParams {
    pub pane_id: String,
    pub source: ReadSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<u32>,
    pub r#match: OutputMatch,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default = "super::common::default_true")]
    pub strip_ansi: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutputMatch {
    Substring { value: String },
    Regex { value: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum EventMatch {
    WorkspaceCreated {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace_id: Option<String>,
    },
    WorkspaceUpdated {
        workspace_id: String,
    },
    WorkspaceClosed {
        workspace_id: String,
    },
    WorkspaceRenamed {
        workspace_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    WorkspaceMoved {
        workspace_id: String,
    },
    WorkspaceFocused {
        workspace_id: String,
    },
    TabCreated {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tab_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace_id: Option<String>,
    },
    TabClosed {
        tab_id: String,
    },
    TabRenamed {
        tab_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    TabMoved {
        tab_id: String,
    },
    TabFocused {
        tab_id: String,
    },
    PaneCreated {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pane_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace_id: Option<String>,
    },
    PaneClosed {
        pane_id: String,
    },
    PaneFocused {
        pane_id: String,
    },
    PaneMoved {
        pane_id: String,
    },
    PaneOutputChanged {
        pane_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min_revision: Option<u64>,
    },
    PaneExited {
        pane_id: String,
    },
    PaneAgentDetected {
        pane_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<String>,
    },
    PaneAgentStatusChanged {
        pane_id: String,
        agent_status: AgentStatus,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    WorkspaceCreated,
    WorkspaceUpdated,
    WorkspaceMetadataUpdated,
    WorkspaceClosed,
    WorkspaceRenamed,
    WorkspaceMoved,
    WorkspaceReordered,
    WorkspaceFocused,
    WorktreeCreated,
    WorktreeOpened,
    WorktreeRemoved,
    TabCreated,
    TabClosed,
    TabRenamed,
    TabMoved,
    TabFocused,
    PaneCreated,
    PaneClosed,
    PaneUpdated,
    PaneFocused,
    PaneMoved,
    PaneOutputChanged,
    PaneExited,
    PaneAgentDetected,
    PaneAgentStatusChanged,
    LayoutUpdated,
    PaneAgentActivityChanged,
}

impl EventKind {
    pub fn dot_name(self) -> &'static str {
        match self {
            EventKind::WorkspaceCreated => "workspace.created",
            EventKind::WorkspaceUpdated => "workspace.updated",
            EventKind::WorkspaceMetadataUpdated => "workspace.metadata_updated",
            EventKind::WorkspaceClosed => "workspace.closed",
            EventKind::WorkspaceRenamed => "workspace.renamed",
            EventKind::WorkspaceMoved => "workspace.moved",
            EventKind::WorkspaceReordered => "workspace.reordered",
            EventKind::WorkspaceFocused => "workspace.focused",
            EventKind::WorktreeCreated => "worktree.created",
            EventKind::WorktreeOpened => "worktree.opened",
            EventKind::WorktreeRemoved => "worktree.removed",
            EventKind::TabCreated => "tab.created",
            EventKind::TabClosed => "tab.closed",
            EventKind::TabRenamed => "tab.renamed",
            EventKind::TabMoved => "tab.moved",
            EventKind::TabFocused => "tab.focused",
            EventKind::PaneCreated => "pane.created",
            EventKind::PaneClosed => "pane.closed",
            EventKind::PaneUpdated => "pane.updated",
            EventKind::PaneFocused => "pane.focused",
            EventKind::PaneMoved => "pane.moved",
            EventKind::PaneOutputChanged => "pane.output_changed",
            EventKind::PaneExited => "pane.exited",
            EventKind::PaneAgentDetected => "pane.agent_detected",
            EventKind::PaneAgentStatusChanged => "pane.agent_status_changed",
            EventKind::LayoutUpdated => "layout.updated",
            EventKind::PaneAgentActivityChanged => "pane.agent_activity_changed",
        }
    }
}

impl EventKind {
    /// `EventKind` 变体总数。`EventHub` 用它作为「按种别记录挤出高水位」的
    /// 定长数组长度。这是进程内实现细节，不进 wire 形状。
    pub(crate) const COUNT: usize = 27;

    /// 稳定的进程内下标。**不是** wire 值，不得序列化；只用来索引定长数组。
    /// 写成穷尽 match，新增变体必须显式分配下标（并同步 `COUNT`）。
    pub(crate) fn index(self) -> usize {
        match self {
            EventKind::WorkspaceCreated => 0,
            EventKind::WorkspaceUpdated => 1,
            EventKind::WorkspaceMetadataUpdated => 2,
            EventKind::WorkspaceClosed => 3,
            EventKind::WorkspaceRenamed => 4,
            EventKind::WorkspaceMoved => 5,
            EventKind::WorkspaceReordered => 6,
            EventKind::WorkspaceFocused => 7,
            EventKind::WorktreeCreated => 8,
            EventKind::WorktreeOpened => 9,
            EventKind::WorktreeRemoved => 10,
            EventKind::TabCreated => 11,
            EventKind::TabClosed => 12,
            EventKind::TabRenamed => 13,
            EventKind::TabMoved => 14,
            EventKind::TabFocused => 15,
            EventKind::PaneCreated => 16,
            EventKind::PaneClosed => 17,
            EventKind::PaneUpdated => 18,
            EventKind::PaneFocused => 19,
            EventKind::PaneMoved => 20,
            EventKind::PaneOutputChanged => 21,
            EventKind::PaneExited => 22,
            EventKind::PaneAgentDetected => 23,
            EventKind::PaneAgentStatusChanged => 24,
            EventKind::LayoutUpdated => 25,
            EventKind::PaneAgentActivityChanged => 26,
        }
    }
}

#[cfg(test)]
pub const KNOWN_EVENT_KINDS: &[EventKind] = &[
    EventKind::WorkspaceCreated,
    EventKind::WorkspaceUpdated,
    EventKind::WorkspaceMetadataUpdated,
    EventKind::WorkspaceClosed,
    EventKind::WorkspaceRenamed,
    EventKind::WorkspaceMoved,
    EventKind::WorkspaceReordered,
    EventKind::WorkspaceFocused,
    EventKind::WorktreeCreated,
    EventKind::WorktreeOpened,
    EventKind::WorktreeRemoved,
    EventKind::TabCreated,
    EventKind::TabClosed,
    EventKind::TabRenamed,
    EventKind::TabMoved,
    EventKind::TabFocused,
    EventKind::PaneCreated,
    EventKind::PaneClosed,
    EventKind::PaneUpdated,
    EventKind::PaneFocused,
    EventKind::PaneMoved,
    EventKind::PaneOutputChanged,
    EventKind::PaneExited,
    EventKind::PaneAgentDetected,
    EventKind::PaneAgentStatusChanged,
    EventKind::LayoutUpdated,
    EventKind::PaneAgentActivityChanged,
];

pub const PLUGIN_HOOK_EVENT_KINDS: &[EventKind] = &[
    EventKind::WorkspaceCreated,
    EventKind::WorkspaceUpdated,
    EventKind::WorkspaceClosed,
    EventKind::WorkspaceRenamed,
    EventKind::WorkspaceMoved,
    EventKind::WorkspaceReordered,
    EventKind::WorkspaceFocused,
    EventKind::WorktreeCreated,
    EventKind::WorktreeOpened,
    EventKind::WorktreeRemoved,
    EventKind::TabCreated,
    EventKind::TabClosed,
    EventKind::TabRenamed,
    EventKind::TabMoved,
    EventKind::TabFocused,
    EventKind::PaneCreated,
    EventKind::PaneClosed,
    EventKind::PaneFocused,
    EventKind::PaneMoved,
    EventKind::PaneExited,
    EventKind::PaneAgentDetected,
    EventKind::PaneAgentStatusChanged,
];

#[cfg(test)]
pub fn known_event_names() -> Vec<&'static str> {
    KNOWN_EVENT_KINDS
        .iter()
        .copied()
        .map(EventKind::dot_name)
        .collect()
}

/// Event names that manifest `[[events]] on` hooks can reference. This is
/// intentionally narrower than `EventKind` until high-volume output-change hook
/// semantics are implemented.
pub fn plugin_hook_event_names() -> Vec<&'static str> {
    PLUGIN_HOOK_EVENT_KINDS
        .iter()
        .copied()
        .map(EventKind::dot_name)
        .collect()
}

#[cfg(test)]
mod known_event_name_tests {
    use super::*;

    #[test]
    fn known_event_names_stay_in_sync_with_event_kind() {
        let mut from_kind = KNOWN_EVENT_KINDS
            .iter()
            .map(|kind| kind.dot_name())
            .collect::<Vec<_>>();
        from_kind.sort_unstable();
        let mut known = known_event_names();
        known.sort_unstable();
        assert_eq!(
            from_kind, known,
            "known_event_names() out of sync with EventKind"
        );
    }

    #[test]
    fn plugin_hook_event_names_exclude_high_volume_events() {
        let names = plugin_hook_event_names();
        assert!(!names.contains(&"pane.output_changed"));
        assert!(!names.contains(&"layout.updated"));
        assert!(!names.contains(&"workspace.metadata_updated"));
        assert!(!names.contains(&"pane.updated"));
        assert!(names.contains(&"pane.moved"));
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct EventEnvelope {
    pub event: EventKind,
    pub data: EventData,
}

/// 事件订阅流上的一帧。形状与 `EventEnvelope` 完全一致（`event` + `data`），
/// 只**纯追加**一个可选 `sequence`（该事件在 server 事件序列里的序号）。
/// 旧客户端按 `EventEnvelope` 解码时会忽略这个未知字段，语义不变
/// （HSR-03/APP-006）。`sequence` 可用作重连游标，并与 `events.lost` 通知帧
/// 一起让断层可感知。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct StreamEventEnvelope {
    pub event: EventKind,
    pub data: EventData,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequence: Option<u64>,
}

impl StreamEventEnvelope {
    pub fn new(sequence: u64, envelope: EventEnvelope) -> Self {
        Self {
            event: envelope.event,
            data: envelope.data,
            sequence: Some(sequence),
        }
    }
}

/// 被 server 事件环形缓冲挤掉的序号闭区间：`from..=to` 这些事件已经不可恢复。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct EventGap {
    pub from: u64,
    pub to: u64,
}

/// 事件订阅流上的**通知帧**：不是某个 pane/workspace 的生命周期事件，而是关于
/// 这条流本身的元信息。线上形状与事件帧同形（`{"event": ..., "data": {...}}`），
/// 只是 `event` 取通知名——`events.lost` 不在 `EventKind` 里，按 `event` 条目
/// 严格解码每一行的客户端会解不开，所以这类帧**只对显式开启
/// `events.subscribe` 的 `notices: true` 的订阅方下发**；socket-api 同时把
/// 「必须忽略无法识别的 `event` 名」写成了这条流的显式契约。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "event", content = "data")]
pub enum EventStreamNotice {
    /// 订阅方的游标落在保留窗口之外：`from..=to` 的事件已被挤掉，客户端需要
    /// 重新拉取 `session.snapshot` 之类的全量快照再续流。
    #[serde(rename = "events.lost")]
    EventsLost(EventGap),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub enum SubscriptionEventKind {
    #[serde(rename = "pane.output_matched")]
    PaneOutputMatched,
    #[serde(rename = "pane.agent_status_changed")]
    PaneAgentStatusChanged,
    #[serde(rename = "pane.scroll_changed")]
    ScrollChanged,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SubscriptionEventEnvelope {
    pub event: SubscriptionEventKind,
    pub data: SubscriptionEventData,
    /// 事件来自 server 事件序列时的序号；由 pane 快照兜底产生的帧没有序号。
    /// 纯追加的可选字段，旧客户端忽略（HSR-03/APP-006）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequence: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum SubscriptionEventData {
    PaneOutputMatched(PaneOutputMatchedEvent),
    PaneAgentStatusChanged(PaneAgentStatusChangedEvent),
    ScrollChanged(PaneScrollChangedEvent),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PaneOutputMatchedEvent {
    pub pane_id: String,
    pub matched_line: String,
    pub read: PaneReadResult,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PaneAgentStatusChangedEvent {
    pub pane_id: String,
    pub workspace_id: String,
    pub agent_status: AgentStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_agent: Option<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub state_labels: HashMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PaneScrollChangedEvent {
    pub pane_id: String,
    pub workspace_id: String,
    pub scroll: PaneScrollInfo,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventData {
    WorkspaceCreated {
        workspace: WorkspaceInfo,
    },
    WorkspaceUpdated {
        workspace: WorkspaceInfo,
    },
    WorkspaceMetadataUpdated {
        workspace: WorkspaceInfo,
    },
    WorkspaceClosed {
        workspace_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace: Option<WorkspaceInfo>,
    },
    WorkspaceRenamed {
        workspace_id: String,
        label: String,
    },
    WorkspaceMoved {
        workspace_id: String,
        insert_index: usize,
        workspaces: Vec<WorkspaceInfo>,
    },
    WorkspaceReordered {
        workspace_ids: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        before_workspace_id: Option<String>,
        workspaces: Vec<WorkspaceInfo>,
    },
    WorkspaceFocused {
        workspace_id: String,
    },
    WorktreeCreated {
        workspace: WorkspaceInfo,
        worktree: WorktreeInfo,
    },
    WorktreeOpened {
        workspace: WorkspaceInfo,
        worktree: WorktreeInfo,
        already_open: bool,
    },
    WorktreeRemoved {
        workspace_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace: Option<WorkspaceInfo>,
        worktree: WorktreeInfo,
        forced: bool,
    },
    TabCreated {
        tab: TabInfo,
    },
    TabClosed {
        tab_id: String,
        workspace_id: String,
    },
    TabRenamed {
        tab_id: String,
        workspace_id: String,
        label: String,
    },
    TabMoved {
        tab_id: String,
        workspace_id: String,
        insert_index: usize,
        tabs: Vec<TabInfo>,
    },
    TabFocused {
        tab_id: String,
        workspace_id: String,
    },
    PaneCreated {
        pane: PaneInfo,
    },
    PaneClosed {
        pane_id: String,
        workspace_id: String,
    },
    PaneUpdated {
        pane: PaneInfo,
    },
    PaneFocused {
        pane_id: String,
        workspace_id: String,
    },
    PaneMoved {
        previous_pane_id: String,
        previous_workspace_id: String,
        previous_tab_id: String,
        pane: Box<PaneInfo>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        created_workspace: Option<WorkspaceInfo>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        created_tab: Option<TabInfo>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        closed_workspace_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        closed_tab_id: Option<String>,
    },
    PaneOutputChanged {
        pane_id: String,
        workspace_id: String,
        revision: u64,
    },
    PaneExited {
        pane_id: String,
        workspace_id: String,
    },
    PaneAgentDetected {
        pane_id: String,
        workspace_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        released: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        final_status: Option<AgentStatus>,
    },
    PaneAgentStatusChanged {
        pane_id: String,
        workspace_id: String,
        agent_status: AgentStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display_agent: Option<String>,
        #[serde(default, skip_serializing_if = "HashMap::is_empty")]
        state_labels: HashMap<String, String>,
    },
    LayoutUpdated {
        layout: super::panes::PaneLayoutSnapshot,
    },
    /// 某 pane 的 agent 活动树变了。只带计数；节点经 `agent.get` /
    /// `agent.activity.read` 取。
    PaneAgentActivityChanged {
        pane_id: String,
        running: u32,
        total: u32,
    },
}
