use super::super::observability::tr;
use super::super::*;
use super::inspector::Target;
use crate::api::schema::{AgentPromptParams, Method, ResponseResult};
use crate::harness::AgentBucket;
use crate::ui::design;
use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};
use unicode_segmentation::UnicodeSegmentation;

#[cfg(test)]
mod tests;

pub(in crate::client::shell) fn ready(n: &ClientNavigatorOverlay) -> bool {
    !n.overview.key.is_empty() && (n.overview.grouping == Grouping::Status || n.overview.preview)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::client::shell) enum Grouping {
    Status,
    #[default]
    Location,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::client::shell) enum Action {
    Grouping,
    Preview,
    Jump,
    Read,
    Unread,
    ReadAll,
    Reply,
    Send,
    CancelReply,
    Close,
    Group(String),
    Row(usize),
    Search,
}

#[derive(Clone, Debug)]
struct Row {
    target: Option<ClientNavigatorTarget>,
    identity: Option<Target>,
    bucket: AgentBucket,
    label: String,
    location: String,
    summary: String,
    age: String,
    stale: bool,
    group: Option<String>,
    depth: u8,
}

#[derive(Clone, Copy, Debug, Default)]
struct Layout {
    popup: Rect,
    title: Rect,
    counts: Rect,
    search: Rect,
    controls: Rect,
    list: Rect,
    preview: Rect,
    footer: Rect,
    editor: Rect,
}

#[derive(Clone, Debug, Default)]
pub(in crate::client::shell) struct State {
    pub(in crate::client::shell) grouping: Grouping,
    preview: bool,
    expanded: HashSet<String>,
    rows: Vec<Row>,
    lines: Vec<String>,
    counts: String,
    key: String,
    layout: Layout,
    selected: usize,
    reveal: bool,
    preview_scroll: usize,
    hovered: Option<Action>,
    reply: TextEditor,
    pub(in crate::client::shell) editing: bool,
    reply_target: Option<Target>,
    in_flight: Option<(u64, Target, Instant)>,
    error: Option<String>,
    version: u64,
    minute: u64,
    glyphs: Option<StatusGlyphs>,
    editor_lines: Vec<String>,
    editor_cursor: Option<(u16, u16)>,
    editor_scroll: usize,
    explicit: bool,
    can_reply: bool,
    can_read: bool,
    can_unread: bool,
}

impl State {
    pub(in crate::client::shell) fn for_endpoints(endpoints: &[ClientShellEndpoint]) -> Self {
        Self {
            grouping: if endpoints
                .iter()
                .filter_map(|endpoint| endpoint.snapshot.as_deref())
                .any(|snapshot| !snapshot.agents.is_empty())
            {
                Grouping::Status
            } else {
                Grouping::Location
            },
            ..Default::default()
        }
    }
}

fn bucket_label(bucket: AgentBucket) -> &'static str {
    match bucket {
        AgentBucket::NeedsInput => tr("Needs input", "需要输入"),
        AgentBucket::Failed => tr("Failed", "失败"),
        AgentBucket::Review => tr("To review", "待查看"),
        AgentBucket::Working => tr("Working", "工作中"),
        AgentBucket::Idle => tr("Idle", "空闲"),
        AgentBucket::None => tr("Terminals", "终端"),
    }
}

fn layout(area: Rect, preview: bool, editing: bool) -> Option<Layout> {
    let width = area.width.saturating_sub(4).min(144);
    let height = area.height.saturating_sub(2).min(42);
    if width < 12 || height < 9 {
        return None;
    }
    let popup = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    let inner = Rect::new(popup.x + 1, popup.y + 1, width - 2, height - 2);
    let mut result = Layout {
        popup,
        title: Rect::new(inner.x, inner.y, inner.width, 1),
        counts: Rect::new(inner.x, inner.y + 1, inner.width, 1),
        search: Rect::new(inner.x, inner.y + 2, inner.width, 1),
        controls: Rect::new(inner.x, inner.y + 3, inner.width, 1),
        footer: Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
        ..Default::default()
    };
    let body = Rect::new(
        inner.x,
        inner.y + 4,
        inner.width,
        inner.height.saturating_sub(5),
    );
    if preview {
        if inner.width >= 106 {
            let left = inner.width * 45 / 100;
            result.list = Rect::new(body.x, body.y, left, body.height);
            result.preview = Rect::new(
                body.x + left + 1,
                body.y,
                body.width - left - 1,
                body.height,
            );
        } else {
            result.preview = body;
        }
    } else {
        result.list = body;
    }
    if editing && !result.preview.is_empty() {
        let height = result.preview.height.min(6);
        result.editor = Rect::new(
            result.preview.x,
            result.preview.bottom() - height,
            result.preview.width,
            height,
        );
        result.preview.height = result.preview.height.saturating_sub(height);
    }
    Some(result)
}

fn wrapped(text: &str, width: u16) -> Vec<String> {
    let width = width.max(1);
    let mut rows = Vec::new();
    for line in text.split('\n') {
        let mut row = String::new();
        let mut used = 0;
        for grapheme in line.graphemes(true) {
            if grapheme.chars().any(|ch| ch.is_control()) {
                continue;
            }
            let cells = crate::ui::display_width_u16(grapheme);
            if cells > width {
                continue;
            }
            if used + cells > width {
                rows.push(std::mem::take(&mut row));
                used = 0;
            }
            row.push_str(grapheme);
            used += cells;
        }
        rows.push(row);
    }
    rows
}

fn nav_row(row: ClientNavigatorRow, endpoints: &[ClientShellEndpoint], now: u64) -> Row {
    let identity = match &row.target {
        ClientNavigatorTarget::Pane {
            endpoint_id,
            pane_id,
        } => endpoints
            .iter()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
            .and_then(|endpoint| {
                let snapshot = endpoint.snapshot.as_deref()?;
                let agent = snapshot
                    .agents
                    .iter()
                    .find(|agent| &agent.pane_id == pane_id)?;
                Some(Target::for_agent(endpoint_id.clone(), snapshot, agent))
            }),
        _ => None,
    };
    let agent = identity.as_ref().and_then(|target| {
        endpoints
            .iter()
            .find(|endpoint| endpoint.endpoint_id == target.endpoint)
            .and_then(|endpoint| endpoint.snapshot.as_deref())
            .and_then(|snapshot| target.agent(snapshot))
    });
    Row {
        bucket: agent.map_or(AgentBucket::None, client_agent_bucket),
        age: agent
            .and_then(|agent| agent.insight.updated_at_ms)
            .map(|at| design::compact_age(now.saturating_sub(at) / 60_000 * 60_000))
            .unwrap_or_default(),
        summary: agent
            .and_then(|agent| {
                agent
                    .insight
                    .question
                    .as_ref()
                    .or(agent.insight.current_tool.as_ref())
                    .or(agent.insight.last_message.as_ref())
            })
            .cloned()
            .unwrap_or_default(),
        label: row.label,
        location: identity
            .as_ref()
            .and_then(|target| {
                endpoints
                    .iter()
                    .find(|endpoint| endpoint.endpoint_id == target.endpoint)
            })
            .and_then(|endpoint| {
                let snapshot = endpoint.snapshot.as_deref()?;
                let agent = agent?;
                let workspace = snapshot
                    .workspaces
                    .iter()
                    .find(|workspace| workspace.workspace_id == agent.workspace_id)?;
                Some(format!(
                    "{} / {} / {}",
                    endpoint.label,
                    workspace.label,
                    workspace.branch.as_deref().unwrap_or_default()
                ))
            })
            .unwrap_or(if row.meta.is_empty() {
                row.detail
            } else {
                row.meta
            }),
        stale: row.stale,
        target: Some(row.target),
        identity,
        group: None,
        depth: row.depth,
    }
}

fn rows(
    endpoints: &[ClientShellEndpoint],
    active: &ClientEndpointId,
    n: &ClientNavigatorOverlay,
    now: u64,
) -> (Vec<Row>, String) {
    let mut counts = [0usize; 5];
    for agent in endpoints
        .iter()
        .filter(|endpoint| endpoint.status == ClientEndpointStatus::Online)
        .filter_map(|endpoint| endpoint.snapshot.as_deref())
        .flat_map(|snapshot| &snapshot.agents)
    {
        if let Some(index) = AgentBucket::LISTING
            .iter()
            .position(|bucket| *bucket == client_agent_bucket(agent))
        {
            counts[index] += 1;
        }
    }
    let counts = AgentBucket::LISTING
        .iter()
        .zip(counts)
        .filter(|(_, count)| *count > 0)
        .map(|(bucket, count)| format!("{} {count}", bucket_label(*bucket)))
        .collect::<Vec<_>>()
        .join("   ");
    if n.overview.grouping == Grouping::Location {
        return (
            super::super::aggregate_navigation::navigator_rows(endpoints, active, n)
                .into_iter()
                .map(|row| nav_row(row, endpoints, now))
                .collect(),
            counts,
        );
    }
    let query = n.query.to_lowercase();
    let tokens = query.split_whitespace().collect::<Vec<_>>();
    let mut items = Vec::new();
    for endpoint in endpoints {
        let Some(snapshot) = endpoint.snapshot.as_deref() else {
            continue;
        };
        for agent in &snapshot.agents {
            let bucket = client_agent_bucket(agent);
            let matches_filter = match n.filter {
                Some(ClientNavigatorFilter::Blocked) => bucket == AgentBucket::NeedsInput,
                Some(ClientNavigatorFilter::Working) => bucket == AgentBucket::Working,
                Some(ClientNavigatorFilter::Idle) => bucket == AgentBucket::Idle,
                Some(ClientNavigatorFilter::Done) => bucket == AgentBucket::Review,
                None => true,
            };
            if !matches_filter {
                continue;
            }
            let workspace = snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.workspace_id == agent.workspace_id);
            let location = format!(
                "{} / {} / {}",
                endpoint.label,
                workspace.map_or(agent.workspace_id.as_str(), |workspace| workspace
                    .label
                    .as_str()),
                workspace
                    .and_then(|workspace| workspace.branch.as_deref())
                    .unwrap_or_default()
            );
            let label = agent
                .insight
                .session_title
                .as_deref()
                .or(agent.name.as_deref())
                .or(agent.title.as_deref())
                .or(agent.display_agent.as_deref())
                .or(agent.agent.as_deref())
                .unwrap_or(&agent.pane_id)
                .to_owned();
            let summary = if bucket == AgentBucket::NeedsInput {
                agent.insight.question.as_deref().or_else(|| {
                    agent
                        .insight
                        .attention
                        .as_ref()
                        .and_then(|attention| attention.detail.as_deref())
                })
            } else if bucket == AgentBucket::Failed {
                agent.insight.error.as_deref().or_else(|| {
                    agent
                        .insight
                        .attention
                        .as_ref()
                        .and_then(|attention| attention.detail.as_deref())
                })
            } else {
                agent
                    .insight
                    .current_tool
                    .as_deref()
                    .or(agent.insight.last_message.as_deref())
            }
            .unwrap_or_default()
            .to_owned();
            let haystack = format!(
                "{label} {location} {summary} {} {}",
                agent.agent.as_deref().unwrap_or_default(),
                agent.pane_id
            )
            .to_lowercase();
            if !tokens.iter().all(|token| haystack.contains(token)) {
                continue;
            }
            items.push(Row {
                target: Some(ClientNavigatorTarget::Pane {
                    endpoint_id: endpoint.endpoint_id.clone(),
                    pane_id: agent.pane_id.clone(),
                }),
                identity: Some(Target::for_agent(
                    endpoint.endpoint_id.clone(),
                    snapshot,
                    agent,
                )),
                bucket,
                label,
                location,
                summary,
                age: agent
                    .insight
                    .updated_at_ms
                    .map(|at| design::compact_age(now.saturating_sub(at) / 60_000 * 60_000))
                    .unwrap_or_default(),
                stale: endpoint.status != ClientEndpointStatus::Online,
                group: None,
                depth: 0,
            });
        }
    }
    let mut result = Vec::new();
    for bucket in AgentBucket::LISTING {
        let group = format!("{:?}", bucket);
        let mut group_items = items
            .iter()
            .filter(|row| row.bucket == bucket)
            .cloned()
            .collect::<Vec<_>>();
        if group_items.is_empty() {
            continue;
        }
        group_items.sort_by_key(|row| row.stale);
        let len = group_items.len();
        result.push(Row {
            label: format!("{} · {len}", bucket_label(bucket)),
            bucket,
            target: None,
            identity: None,
            location: String::new(),
            summary: String::new(),
            age: String::new(),
            stale: false,
            group: Some(group.clone()),
            depth: 0,
        });
        let count = if n.overview.expanded.contains(&group) || !tokens.is_empty() {
            len
        } else {
            6
        };
        result.extend(group_items.into_iter().take(count));
        if len > count {
            result.push(Row {
                label: format!("… {} {}", len - count, tr("more (Enter)", "更多（Enter）")),
                bucket,
                target: None,
                identity: None,
                location: String::new(),
                summary: String::new(),
                age: String::new(),
                stale: false,
                group: Some(group),
                depth: 0,
            });
        }
    }
    (result, counts)
}

impl ClientShellState {
    pub(in crate::client::shell) fn saved_agent_marks(
        &self,
    ) -> HashMap<String, Vec<super::super::endpoint_agent_state::ReadMark>> {
        let mut saved = self.config.preferences.agent_marks.clone();
        let live_keys = self
            .endpoints
            .iter()
            .filter(|endpoint| endpoint.status != ClientEndpointStatus::Disabled)
            .map(|endpoint| endpoint.endpoint_id.storage_key())
            .collect::<HashSet<_>>();
        saved.retain(|key, _| live_keys.contains(key));
        for endpoint in &self.endpoints {
            if endpoint.status == ClientEndpointStatus::Disabled
                || !endpoint.agent_presentation.marks_imported()
            {
                continue;
            }
            let key = endpoint.endpoint_id.storage_key();
            saved.remove(&key);
            if !endpoint.agent_presentation.marks().is_empty() {
                saved.insert(key, endpoint.agent_presentation.marks().to_vec());
            }
        }
        let mut keys = saved.keys().cloned().collect::<Vec<_>>();
        keys.sort();
        let mut remaining = 256usize;
        for key in keys {
            if let Some(marks) = saved.get_mut(&key) {
                marks.truncate(remaining);
                remaining -= marks.len();
            }
        }
        saved.retain(|_, marks| !marks.is_empty());
        saved
    }

    fn agent_mark_target_available(&self, target: &Target) -> bool {
        self.endpoint_is_online(&target.endpoint)
            && self.overview_agent(target).is_some_and(|agent| {
                matches!(
                    client_agent_bucket(agent),
                    AgentBucket::Idle | AgentBucket::Review
                )
            })
    }

    fn agent_marks_full_for(&self, target: &Target) -> bool {
        let total = self
            .endpoints
            .iter()
            .filter(|endpoint| endpoint.status != ClientEndpointStatus::Disabled)
            .map(|endpoint| {
                if endpoint.agent_presentation.marks_imported() {
                    endpoint.agent_presentation.marks().len()
                } else {
                    self.config
                        .preferences
                        .agent_marks
                        .get(&endpoint.endpoint_id.storage_key())
                        .map_or(0, Vec::len)
                }
            })
            .sum::<usize>();
        total >= 256
            && !self.endpoints.iter().any(|endpoint| {
                endpoint.endpoint_id == target.endpoint
                    && endpoint
                        .agent_presentation
                        .marks()
                        .iter()
                        .any(|mark| mark.pane == target.pane)
            })
    }

    fn can_mark_agent_read(&self, target: &Target, read: bool) -> bool {
        self.agent_mark_target_available(target) && (read || !self.agent_marks_full_for(target))
    }

    pub(in crate::client::shell) fn mark_agent_read(
        &mut self,
        target: &Target,
        read: bool,
    ) -> bool {
        if !self.agent_mark_target_available(target) {
            return false;
        }
        let full = self.agent_marks_full_for(target);
        if full && !read {
            return false;
        }
        let Some(endpoint) = self.endpoints.iter_mut().find(|endpoint| {
            endpoint.endpoint_id == target.endpoint
                && endpoint.status == ClientEndpointStatus::Online
        }) else {
            return false;
        };
        let Some(snapshot) = endpoint
            .snapshot
            .as_mut()
            .filter(|snapshot| target.agent(snapshot).is_some())
        else {
            return false;
        };
        let changed = if full {
            endpoint
                .agent_presentation
                .acknowledge_agent(snapshot, &target.pane)
        } else {
            endpoint
                .agent_presentation
                .mark(snapshot, &target.pane, read)
        };
        if !changed {
            return false;
        }
        if endpoint.endpoint_id == self.active_endpoint_id {
            self.snapshot = Some(snapshot.clone());
        }
        self.inspector.observe_snapshot(&target.endpoint, snapshot);
        self.agent_rows_epoch = self.agent_rows_epoch.saturating_add(1);
        self.schedule_chrome_preferences(Instant::now());
        true
    }

    fn apply_agent_read_mark(
        &mut self,
        target: Option<&Target>,
        read: bool,
        outcome: &mut ClientShellInput,
    ) -> bool {
        if target.is_some_and(|target| self.mark_agent_read(target, read)) {
            outcome.repaint = true;
            return true;
        }
        let message = if !read
            && target.is_some_and(|target| {
                self.agent_mark_target_available(target) && self.agent_marks_full_for(target)
            }) {
            tr(
                "Unread mark limit reached (256).",
                "未读标记已达上限（256）。",
            )
        } else {
            tr(
                "Read marks require an online idle or review agent.",
                "仅可标记在线的空闲或待查看智能体。",
            )
        };
        self.set_endpoint_error(message);
        outcome.repaint = true;
        false
    }

    pub(in crate::client::shell) fn mark_all_agents_read(&mut self) {
        let targets = self
            .endpoints
            .iter()
            .filter(|endpoint| endpoint.status == ClientEndpointStatus::Online)
            .filter_map(|endpoint| {
                endpoint
                    .snapshot
                    .as_deref()
                    .map(|snapshot| (endpoint.endpoint_id.clone(), snapshot))
            })
            .flat_map(|(endpoint, snapshot)| {
                snapshot
                    .agents
                    .iter()
                    .map(move |agent| Target::for_agent(endpoint.clone(), snapshot, agent))
            })
            .collect::<Vec<_>>();
        for target in targets {
            self.mark_agent_read(&target, true);
        }
    }

    fn focused_agent_mark_target(&self) -> Option<Target> {
        let snapshot = self.snapshot.as_deref()?;
        let agent = snapshot
            .agents
            .iter()
            .find(|agent| Some(agent.pane_id.as_str()) == snapshot.focused_pane_id.as_deref())?;
        Some(Target::for_agent(
            self.active_endpoint_id.clone(),
            snapshot,
            agent,
        ))
    }

    pub(in crate::client::shell) fn can_mark_focused_agent_unread(&self) -> bool {
        self.focused_agent_mark_target()
            .is_some_and(|target| self.can_mark_agent_read(&target, false))
    }

    pub(in crate::client::shell) fn mark_focused_agent_unread(
        &mut self,
        outcome: &mut ClientShellInput,
    ) -> bool {
        let target = self.focused_agent_mark_target();
        self.apply_agent_read_mark(target.as_ref(), false, outcome)
    }

    fn overview(&self) -> Option<&ClientNavigatorOverlay> {
        match self.overlay.as_ref() {
            Some(ClientShellOverlay::Navigator(n)) => Some(n),
            _ => None,
        }
    }
    fn overview_mut(&mut self) -> Option<&mut ClientNavigatorOverlay> {
        match self.overlay.as_mut() {
            Some(ClientShellOverlay::Navigator(n)) => Some(n),
            _ => None,
        }
    }
    fn overview_target(&self) -> Option<Target> {
        let n = self.overview()?;
        n.overview.rows.get(n.overview.selected)?.identity.clone()
    }
    fn overview_agent(&self, target: &Target) -> Option<&crate::protocol::ClientShellAgent> {
        self.endpoints
            .iter()
            .find(|endpoint| endpoint.endpoint_id == target.endpoint)
            .and_then(|endpoint| endpoint.snapshot.as_deref())
            .and_then(|snapshot| target.agent(snapshot))
    }
    fn reply_available(&self, target: &Target) -> bool {
        self.endpoint_is_online(&target.endpoint)
            && self.overview_agent(target).is_some_and(|agent| {
                matches!(
                    client_agent_bucket(agent),
                    AgentBucket::Idle | AgentBucket::Review
                )
            })
            && self
                .endpoints
                .iter()
                .find(|endpoint| endpoint.endpoint_id == target.endpoint)
                .and_then(|endpoint| endpoint.methods.as_ref())
                .is_some_and(|methods| methods.contains("agent.prompt"))
            && self.supports_endpoint_method_for(
                &target.endpoint,
                &Method::AgentPrompt(AgentPromptParams {
                    target: target.pane.clone(),
                    text: String::new(),
                    wait: None,
                }),
            )
    }

    pub(in crate::client::shell) fn close_overview(&mut self) {
        if self.overview().is_some() {
            self.overlay = None;
        }
        self.clear_overview_inspector_target();
        self.pending_requests.retain(|_, pending| {
            !matches!(pending.kind, PendingEndpointKind::OverviewPrompt { .. })
        });
    }

    pub(in crate::client::shell) fn compute_overview_view(&mut self, area: Rect) {
        if self.overview().is_none() {
            self.clear_overview_inspector_target();
            return;
        }
        let now =
            crate::harness::time::system_time_ms(std::time::SystemTime::now()).unwrap_or_default();
        let glyphs = self.status_glyphs();
        let Some(n) = self.overview() else {
            return;
        };
        let Some(geometry) = layout(area, n.overview.preview, n.overview.editing) else {
            if let Some(n) = self.overview_mut() {
                n.overview.layout = Layout::default();
                n.overview.key.clear();
            }
            return;
        };
        let key = format!(
            "{}:{}:{}:{}:{:?}:{:?}:{}:{}:{:?}",
            self.agent_rows_epoch,
            now / 60_000,
            area.width,
            area.height,
            n.filter,
            n.overview.grouping,
            n.query,
            n.overview.version,
            n.selected,
        );
        let rebuild = key != n.overview.key;
        let next_rows = rebuild.then(|| rows(&self.endpoints, &self.active_endpoint_id, n, now));
        let Some(n) = self.overview_mut() else {
            return;
        };
        if let Some((rows, counts)) = next_rows {
            n.overview.rows = rows;
            n.overview.counts = counts;
            n.overview.key = key;
            n.overview.minute = now / 60_000;
        }
        n.overview.layout = geometry;
        n.overview.glyphs = Some(glyphs);
        if let Some(target) = &n.selected {
            if let Some(index) = n
                .overview
                .rows
                .iter()
                .position(|row| row.target.as_ref() == Some(target))
            {
                n.overview.selected = index;
            } else {
                n.selected = None;
            }
        }
        if n.selected.is_none() && !n.overview.explicit {
            n.overview.selected = n
                .overview
                .rows
                .iter()
                .position(|row| row.target.is_some())
                .unwrap_or(0);
            n.selected = n
                .overview
                .rows
                .get(n.overview.selected)
                .and_then(|row| row.target.clone());
        }
        let count = n.overview.rows.len();
        n.overview.selected = n.overview.selected.min(count.saturating_sub(1));
        let visible = usize::from(geometry.list.height);
        n.scroll = page::list_start(
            n.scroll,
            n.overview.selected,
            count,
            visible,
            n.overview.reveal,
        );
        n.overview.reveal = false;
        let target = n
            .overview
            .rows
            .get(n.overview.selected)
            .and_then(|row| row.identity.clone());
        let preview = n.overview.preview;
        let stale = n
            .overview
            .rows
            .get(n.overview.selected)
            .is_some_and(|row| row.stale);
        let reply_available = target
            .as_ref()
            .is_some_and(|target| self.reply_available(target));
        let read_available = target
            .as_ref()
            .is_some_and(|target| self.can_mark_agent_read(target, true));
        let unread_available = target
            .as_ref()
            .is_some_and(|target| self.can_mark_agent_read(target, false));
        let mut lines = Vec::new();
        if preview && rebuild {
            if let Some(target) = &target {
                if let Some(agent) = self.overview_agent(target) {
                    let endpoint = self
                        .endpoints
                        .iter()
                        .find(|endpoint| endpoint.endpoint_id == target.endpoint);
                    let snapshot = endpoint.and_then(|endpoint| endpoint.snapshot.as_deref());
                    let workspace = snapshot.and_then(|snapshot| {
                        snapshot
                            .workspaces
                            .iter()
                            .find(|workspace| workspace.workspace_id == agent.workspace_id)
                    });
                    let tab = snapshot.and_then(|snapshot| {
                        snapshot.tabs.iter().find(|tab| tab.tab_id == agent.tab_id)
                    });
                    let title = agent
                        .insight
                        .session_title
                        .as_deref()
                        .or(agent.title.as_deref())
                        .or(agent.name.as_deref())
                        .unwrap_or(&agent.pane_id);
                    let raw = format!(
                        "{}\n{} / {} / {} / {}\n{}\n{}\n{}\n\n{}\n{}\n{}\n\n{}\n{}\n\n{}\n{}\n\n{}\n{}\n\n{}",
                        title,
                        endpoint.map_or("", |endpoint| endpoint.label.as_str()),
                        workspace.map_or(agent.workspace_id.as_str(), |workspace| workspace
                            .label
                            .as_str()),
                        workspace
                            .and_then(|workspace| workspace.branch.as_deref())
                            .unwrap_or_default(),
                        tab.map_or(agent.tab_id.as_str(), |tab| tab.label.as_str()),
                        agent.insight.model.as_deref().unwrap_or_default(),
                        bucket_label(client_agent_bucket(agent)),
                        agent
                            .insight
                            .last_turn_ms
                            .map(design::duration_label)
                            .unwrap_or_default(),
                        tr("Question / failure", "问题 / 失败详情"),
                        if client_agent_bucket(agent) == AgentBucket::Failed { agent.insight.error.as_deref().or(agent.insight.question.as_deref()) } else { agent.insight.question.as_deref().or(agent.insight.error.as_deref()) }.unwrap_or_default(),
                        agent
                            .insight
                            .attention
                            .as_ref()
                            .and_then(|attention| attention.detail.as_deref())
                            .unwrap_or_default(),
                        tr("Options", "选项"),
                        agent
                            .insight
                            .options
                            .iter()
                            .chain(
                                agent
                                    .insight
                                    .attention
                                    .iter()
                                    .flat_map(|attention| &attention.options)
                            )
                            .cloned()
                            .collect::<Vec<_>>()
                            .join("\n"),
                        tr("Last message", "最近消息"),
                        agent.insight.last_message.as_deref().unwrap_or_default(),
                        tr("Last prompt", "最近提示"),
                        agent.insight.last_prompt.as_deref().unwrap_or_default(),
                        tr(
                            "Received insight only; the server may have shortened it.",
                            "仅显示收到的洞察；服务端可能已经截短。"
                        )
                    );
                    let turn = agent
                        .insight
                        .working_since_ms
                        .map(|start| {
                            format!(
                                "{} {}",
                                tr("Working started", "工作开始距今"),
                                design::compact_age(now.saturating_sub(start) / 60_000 * 60_000)
                            )
                        })
                        .unwrap_or_default();
                    let secondary =
                        if agent.insight.question.is_some() && agent.insight.error.is_some() {
                            if client_agent_bucket(agent) == AgentBucket::Failed {
                                agent.insight.question.as_deref().unwrap_or_default()
                            } else {
                                agent.insight.error.as_deref().unwrap_or_default()
                            }
                        } else {
                            ""
                        };
                    let extra = format!(
                        "\n{turn}\n{secondary}\n{}\n{}",
                        agent.insight.current_tool.as_deref().unwrap_or_default(),
                        agent
                            .insight
                            .attention
                            .as_ref()
                            .and_then(|attention| attention.title.as_deref())
                            .unwrap_or_default()
                    );
                    lines = wrapped(&(raw + &extra), geometry.preview.width.saturating_sub(2));
                }
            } else {
                lines=wrapped(tr("Select an agent to preview. Location mode also includes machines and workspaces.","选择智能体预览。位置模式也保留机器与工作区导航。"),geometry.preview.width.saturating_sub(2));
            }
        }
        self.set_overview_inspector_target(if preview && !stale {
            target.clone()
        } else {
            None
        });
        let invalid_reply = self.overview().is_some_and(|n| {
            n.overview.reply_target.is_some() && n.overview.reply_target != target
        });
        if invalid_reply {
            self.pending_requests.retain(|_, pending| {
                !matches!(pending.kind, PendingEndpointKind::OverviewPrompt { .. })
            });
        }
        if let Some(n) = self.overview_mut() {
            n.overview.can_reply = reply_available;
            n.overview.can_read = read_available;
            n.overview.can_unread = unread_available;
            if rebuild {
                n.overview.lines = lines;
            }
            n.overview.preview_scroll = n.overview.preview_scroll.min(
                n.overview
                    .lines
                    .len()
                    .saturating_sub(usize::from(geometry.preview.height)),
            );
            if invalid_reply {
                n.overview.reply = TextEditor::multiline();
                n.overview.editing = false;
                n.overview.in_flight = None;
                n.overview.reply_target = None;
                n.overview.error = None;
                n.overview.version += 1;
            }
            if n.overview.editing && rebuild {
                n.overview.editor_lines = wrapped(
                    n.overview.reply.as_str(),
                    geometry.editor.width.saturating_sub(2),
                );
                let prefix = n
                    .overview
                    .reply
                    .chars()
                    .take(n.overview.reply.cursor_char_index())
                    .collect::<String>();
                let cursor = wrapped(&prefix, geometry.editor.width.saturating_sub(2));
                let y = cursor.len().saturating_sub(1);
                let x = cursor
                    .last()
                    .map_or(0, |line| crate::ui::display_width_u16(line));
                let height = usize::from(geometry.editor.height).max(1);
                n.overview.editor_scroll = n
                    .overview
                    .editor_scroll
                    .min(y)
                    .max(y.saturating_sub(height - 1));
                n.overview.editor_cursor =
                    Some((x, y.saturating_sub(n.overview.editor_scroll) as u16));
            }
        }
    }

    pub(crate) fn tick_overview(&mut self, now: Instant, outcome: &mut ClientShellInput) {
        let minute = crate::harness::time::system_time_ms(std::time::SystemTime::now())
            .unwrap_or_default()
            / 60_000;
        let Some(n) = self.overview_mut() else {
            return;
        };
        if minute != n.overview.minute {
            outcome.repaint = true;
        }
        if n.overview
            .in_flight
            .as_ref()
            .is_some_and(|(_, _, deadline)| now >= *deadline)
        {
            n.overview.in_flight = None;
            n.overview.error = Some(
                tr(
                    "Timed out; may be sent. Check terminal before retry.",
                    "发送超时，可能已提交；请先检查终端，草稿保留。",
                )
                .into(),
            );
            n.overview.version += 1;
            outcome.repaint = true;
        }
    }

    pub(in crate::client::shell) fn insert_overview_text(&mut self, text: &str) -> bool {
        let Some(n) = self.overview_mut().filter(|n| n.overview.editing) else {
            return false;
        };
        if n.overview.in_flight.is_none()
            && n.overview.reply.len().saturating_add(text.len()) <= 16384
        {
            n.overview.reply.insert(text);
            n.overview.version += 1;
        }
        true
    }

    pub(in crate::client::shell) fn send_overview_reply(&mut self, outcome: &mut ClientShellInput) {
        let Some(target) = self.overview_target() else {
            return;
        };
        if !self.reply_available(&target) {
            return;
        }
        let Some(n) = self.overview() else {
            return;
        };
        if n.overview.in_flight.is_some()
            || n.overview.reply_target.as_ref() != Some(&target)
            || n.overview.reply.trim().is_empty()
            || n.overview.reply.len() > 16384
        {
            return;
        }
        let text = n.overview.reply.as_str().to_owned();
        let epoch = self.next_request_id;
        if self.push_endpoint_method_for(
            &target.endpoint,
            Method::AgentPrompt(AgentPromptParams {
                target: target.pane.clone(),
                text,
                wait: None,
            }),
            PendingEndpointKind::OverviewPrompt {
                epoch,
                target: target.clone(),
            },
            outcome,
        ) {
            if let Some(n) = self.overview_mut() {
                n.overview.in_flight =
                    Some((epoch, target, Instant::now() + Duration::from_secs(10)));
                n.overview.error = None;
                n.overview.version += 1;
                outcome.repaint = true;
            }
        }
    }

    pub(in crate::client::shell) fn receive_overview_prompt(
        &mut self,
        epoch: u64,
        target: &Target,
        boot: &str,
        result: Result<ResponseResult, ClientShellEndpointError>,
    ) -> bool {
        if boot != target.boot || self.overview_agent(target).is_none() {
            return false;
        }
        let Some(n) = self.overview_mut() else {
            return false;
        };
        if n.overview
            .in_flight
            .as_ref()
            .is_none_or(|(current, identity, _)| *current != epoch || identity != target)
            || n.overview.reply_target.as_ref() != Some(target)
        {
            return false;
        }
        n.overview.in_flight = None;
        match result {
            Ok(ResponseResult::AgentPrompted { .. }) => {
                n.overview.reply.clear();
                n.overview.editing = false;
                n.overview.error = Some(tr("Reply sent", "回复已发送").into());
            }
            Err(error) => n.overview.error = Some(error.message),
            Ok(_) => {
                n.overview.error = Some(
                    tr(
                        "Unexpected reply result; draft kept.",
                        "回复结果不符合预期，草稿已保留。",
                    )
                    .into(),
                )
            }
        }
        n.overview.version += 1;
        true
    }

    pub(in crate::client::shell) fn activate_overview(
        &mut self,
        action: Action,
        outcome: &mut ClientShellInput,
    ) {
        match action {
            Action::Close => self.close_overview(),
            Action::ReadAll => self.mark_all_agents_read(),
            Action::Read | Action::Unread => {
                let target = self.overview_target();
                self.apply_agent_read_mark(target.as_ref(), action == Action::Read, outcome);
            }
            Action::Send => self.send_overview_reply(outcome),
            Action::Jump => {
                let row = self
                    .overview()
                    .and_then(|n| n.overview.rows.get(n.overview.selected))
                    .cloned();
                if let Some(row) = row.filter(|row| {
                    !row.stale
                        && row.target.as_ref().is_none_or(|target| {
                            let endpoint = match target {
                                ClientNavigatorTarget::Machine { endpoint_id }
                                | ClientNavigatorTarget::Workspace { endpoint_id, .. }
                                | ClientNavigatorTarget::Pane { endpoint_id, .. } => endpoint_id,
                            };
                            self.endpoint_is_online(endpoint)
                        })
                }) {
                    if let Some(target) = row.target {
                        if row
                            .identity
                            .as_ref()
                            .is_some_and(|target| self.overview_agent(target).is_none())
                        {
                            return;
                        }
                        let activated = match target {
                            ClientNavigatorTarget::Machine { endpoint_id } => {
                                self.activate_endpoint(endpoint_id, outcome)
                            }
                            ClientNavigatorTarget::Workspace {
                                endpoint_id,
                                workspace_id,
                            } => self.focus_or_activate(
                                endpoint_id,
                                ClientEndpointFocusTarget::Workspace(workspace_id),
                                outcome,
                            ),
                            ClientNavigatorTarget::Pane {
                                endpoint_id,
                                pane_id,
                            } => self.focus_or_activate(
                                endpoint_id,
                                ClientEndpointFocusTarget::Pane(pane_id),
                                outcome,
                            ),
                        };
                        if activated {
                            self.close_overview();
                            self.refocus_current_workbench_terminal();
                        }
                    } else if let Some(group) = row.group {
                        self.activate_overview(Action::Group(group), outcome);
                    }
                }
            }
            Action::Reply => {
                if let Some(target) = self
                    .overview_target()
                    .filter(|target| self.reply_available(target))
                {
                    if let Some(n) = self.overview_mut() {
                        n.overview.preview = true;
                        n.overview.editing = true;
                        if n.overview.reply_target.as_ref() != Some(&target) {
                            n.overview.reply = TextEditor::multiline();
                            n.overview.editor_scroll = 0;
                        }
                        n.overview.reply_target = Some(target);
                        n.overview.error = None;
                        n.overview.version += 1;
                    }
                }
            }
            Action::CancelReply => {
                if let Some(n) = self.overview_mut() {
                    n.overview.editing = false;
                    n.overview.in_flight = None;
                    n.overview.version += 1;
                }
                self.pending_requests.retain(|_, pending| {
                    !matches!(pending.kind, PendingEndpointKind::OverviewPrompt { .. })
                });
            }
            other => {
                if let Some(n) = self.overview_mut() {
                    match other {
                        Action::Grouping => {
                            n.overview.grouping = if n.overview.grouping == Grouping::Status {
                                Grouping::Location
                            } else {
                                Grouping::Status
                            };
                            n.selected = None;
                            n.scroll = 0;
                        }
                        Action::Preview => {
                            n.overview.preview = !n.overview.preview;
                            n.overview.preview_scroll = 0;
                        }
                        Action::Group(group) => {
                            if !n.overview.expanded.remove(&group) {
                                n.overview.expanded.insert(group);
                            }
                            n.overview.reveal = true;
                        }
                        Action::Row(index) => {
                            n.overview.explicit = true;
                            n.overview.selected = index;
                            n.selected = n
                                .overview
                                .rows
                                .get(index)
                                .and_then(|row| row.target.clone());
                            n.overview.reveal = true;
                        }
                        Action::Search => n.search_focused = true,
                        _ => {}
                    }
                    n.overview.version += 1;
                }
            }
        }
        outcome.repaint = true;
    }

    pub(in crate::client::shell) fn route_overview_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) -> bool {
        let Some(current) = self.overview() else {
            return false;
        };
        let (code, modifiers) = crate::config::normalize_key_combo((key.code, key.modifiers));
        if current.overview.grouping == Grouping::Location
            && !current.overview.preview
            && !current.overview.editing
        {
            let reserved = current.search_focused
                || modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
                || code == KeyCode::Char('g') && modifiers.contains(KeyModifiers::SHIFT);
            let special =
                !reserved && matches!(code, KeyCode::Char(' ' | 'g' | 'r' | 'u' | 'R' | 'e'));
            if !special {
                return false;
            }
        }
        let (cols, rows) = self.last_composed_size.unwrap_or((120, 40));
        self.compute_overview_view(Rect::new(0, 0, cols, rows));
        let Some(n) = self.overview() else {
            return false;
        };
        let (code, modifiers) = crate::config::normalize_key_combo((key.code, key.modifiers));
        if n.overview.editing {
            if code == KeyCode::Esc {
                self.activate_overview(Action::CancelReply, outcome);
            } else if code == KeyCode::Enter && modifiers == KeyModifiers::CONTROL {
                self.send_overview_reply(outcome);
            } else if let Some(n) = self.overview_mut() {
                if n.overview.in_flight.is_none() {
                    let old = n.overview.reply.clone();
                    if code == KeyCode::Enter {
                        n.overview.reply.insert("\n");
                    } else {
                        n.overview.reply.handle_key(key);
                    }
                    if n.overview.reply.len() > 16384 {
                        n.overview.reply = old;
                    }
                    n.overview.version += 1;
                    outcome.repaint = true;
                }
            }
            return true;
        }
        if n.search_focused {
            if code == KeyCode::Esc {
                if let Some(n) = self.overview_mut() {
                    n.search_focused = false;
                }
                outcome.repaint = true;
            } else if code == KeyCode::Enter {
                self.activate_overview(Action::Jump, outcome);
            } else if matches!(code, KeyCode::Up | KeyCode::Down) {
                self.step_overview(if code == KeyCode::Down { 1 } else { -1 });
                outcome.repaint = true;
            } else if let Some(n) = self.overview_mut() {
                if n.query.handle_key(key).is_some() {
                    n.filter = None;
                    n.selected = None;
                    n.overview.explicit = false;
                    n.overview.version += 1;
                    outcome.repaint = true;
                }
            }
            return true;
        }
        if crate::config::terminal_key_matches_combo(key, (KeyCode::Char('g'), KeyModifiers::SHIFT))
        {
            self.step_overview(isize::MAX);
            outcome.repaint = true;
            return true;
        }
        let action = match code {
            KeyCode::Esc => Some(Action::Close),
            KeyCode::Enter => Some(Action::Jump),
            KeyCode::Char(' ') => Some(Action::Preview),
            KeyCode::Char('g') => Some(Action::Grouping),
            KeyCode::Char('r') if modifiers.is_empty() => Some(Action::Read),
            KeyCode::Char('u') if modifiers.is_empty() => Some(Action::Unread),
            KeyCode::Char('R') => Some(Action::ReadAll),
            KeyCode::Char('r') if modifiers == KeyModifiers::SHIFT => Some(Action::ReadAll),
            KeyCode::Char('e') => Some(Action::Reply),
            KeyCode::Char('/') => Some(Action::Search),
            _ => None,
        };
        if let Some(action) = action {
            self.activate_overview(action, outcome);
            return true;
        }
        match code {
            KeyCode::Up | KeyCode::Char('k') => self.step_overview(-1),
            KeyCode::Down | KeyCode::Char('j') => self.step_overview(1),
            KeyCode::PageUp | KeyCode::PageDown => {
                if let Some(n) = self.overview_mut() {
                    if n.overview.preview {
                        n.overview.preview_scroll = n
                            .overview
                            .preview_scroll
                            .saturating_add_signed(if code == KeyCode::PageDown { 8 } else { -8 });
                    } else {
                        self.step_overview(if code == KeyCode::PageDown { 8 } else { -8 });
                    }
                }
            }
            KeyCode::Home => self.step_overview(isize::MIN),
            KeyCode::End | KeyCode::Char('G') => self.step_overview(isize::MAX),
            KeyCode::Left | KeyCode::Right => {
                if let Some(n) = self.overview_mut() {
                    if n.overview.preview {
                        n.overview.preview_scroll = n
                            .overview
                            .preview_scroll
                            .saturating_add_signed(if code == KeyCode::Right { 3 } else { -3 });
                    } else {
                        self.move_navigator_workspace(code == KeyCode::Right);
                    }
                }
            }
            KeyCode::Char(ch @ ('b' | 'w' | 'i' | 'd' | 'a')) => {
                if let Some(n) = self.overview_mut() {
                    n.filter = match ch {
                        'b' => Some(ClientNavigatorFilter::Blocked),
                        'w' => Some(ClientNavigatorFilter::Working),
                        'i' => Some(ClientNavigatorFilter::Idle),
                        'd' => Some(ClientNavigatorFilter::Done),
                        _ => None,
                    };
                    n.selected = None;
                    n.overview.version += 1;
                }
            }
            _ => {}
        }
        outcome.repaint = true;
        true
    }

    pub(in crate::client::shell) fn scroll_overview_to(
        &mut self,
        scroll: usize,
        viewport_rows: usize,
    ) -> bool {
        let Some(n) = self.overview_mut().filter(|n| ready(n)) else {
            return false;
        };
        n.scroll = scroll.min(n.overview.rows.len().saturating_sub(viewport_rows.max(1)));
        n.overview.reveal = false;
        true
    }

    fn step_overview(&mut self, delta: isize) {
        if let Some(n) = self.overview_mut() {
            if n.overview.rows.is_empty() {
                return;
            }
            let index = n
                .overview
                .selected
                .saturating_add_signed(delta)
                .min(n.overview.rows.len() - 1);
            n.overview.explicit = true;
            n.overview.selected = index;
            n.selected = n.overview.rows[index].target.clone();
            n.overview.reveal = true;
            n.overview.version += 1;
        }
    }

    pub(in crate::client::shell) fn overview_mouse(
        &mut self,
        mouse: MouseEvent,
        outcome: &mut ClientShellInput,
    ) -> bool {
        if self.overview().is_none_or(|n| !ready(n)) {
            return false;
        }
        let point = (mouse.column, mouse.row);
        let action = self
            .hits
            .overview_actions
            .iter()
            .find(|(rect, _)| contains(*rect, point))
            .map(|(_, action)| action.clone());
        match mouse.kind {
            MouseEventKind::Moved => {
                if let Some(n) = self.overview_mut() {
                    if n.overview.hovered != action {
                        n.overview.hovered = action;
                        outcome.repaint = true;
                    }
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if contains(self.hits.navigator_scrollbar, point) {
                    if let Some(metrics) = self.hits.navigator_scroll_metrics {
                        if let Some(grab_row_offset) = crate::ui::scrollbar_thumb_grab_offset(
                            metrics,
                            self.hits.navigator_scrollbar,
                            mouse.row,
                        ) {
                            self.chrome_drag =
                                Some(ClientChromeDrag::NavigatorScrollbar { grab_row_offset });
                        } else {
                            let offset = crate::ui::scrollbar_offset_from_row(
                                metrics,
                                self.hits.navigator_scrollbar,
                                mouse.row,
                            );
                            self.scroll_overview_to(
                                metrics.max_offset_from_bottom.saturating_sub(offset),
                                metrics.viewport_rows,
                            );
                            outcome.repaint = true;
                        }
                    }
                } else if let Some(action) = action {
                    self.activate_overview(action, outcome);
                } else if !contains(self.hits.navigator_popup, point) {
                    self.close_overview();
                    outcome.repaint = true;
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let delta = if mouse.kind == MouseEventKind::ScrollUp {
                    -(self.config.mouse_scroll_lines as isize)
                } else {
                    self.config.mouse_scroll_lines as isize
                };
                if let Some(n) = self.overview_mut() {
                    if contains(n.overview.layout.preview, point) {
                        n.overview.preview_scroll =
                            n.overview.preview_scroll.saturating_add_signed(delta);
                    } else {
                        n.scroll = n.scroll.saturating_add_signed(delta);
                        n.overview.reveal = false;
                    }
                    outcome.repaint = true;
                }
            }
            _ => {}
        }
        true
    }
}

/// 位置详情跟随已经画出的完整名称，不能猜测树连接线、当前项和状态字形的宽度。
/// 名称被裁切时不追加详情；因此不同导航器标签前缀也共用实际单元格几何。
fn location_detail_area(buffer: &Buffer, rect: Rect, label: &str, reserve: u16) -> Option<Rect> {
    if label.is_empty() || rect.is_empty() {
        return None;
    }
    let right = rect.right().saturating_sub(reserve + 1);
    for start in rect.x..right {
        let mut end = start;
        let matches = label.graphemes(true).all(|glyph| {
            let width = crate::ui::display_width_u16(glyph);
            if end.saturating_add(width) > right || buffer[(end, rect.y)].symbol() != glyph {
                return false;
            }
            end += width;
            true
        });
        // 名称与位置之间至少两列，保持两段文字分离。
        let x = end.saturating_add(2);
        if matches && x < right {
            return Some(Rect::new(x, rect.y, right - x, 1));
        }
    }
    None
}

pub(in crate::client::shell) fn paint_location_details(
    buffer: &mut Buffer,
    n: &ClientNavigatorOverlay,
    hits: &super::super::render::OverlayRender,
    cx: &super::super::feedback::ChromeContext<'_>,
) {
    for (rect, target) in &hits.navigator_rows {
        let Some(row) = n
            .overview
            .rows
            .iter()
            .find(|row| row.target.as_ref() == Some(target) && row.identity.is_some())
        else {
            continue;
        };
        let reserve = if rect.width >= 64 {
            24
        } else if rect.width >= 36 {
            12
        } else {
            0
        };
        let Some(detail) = location_detail_area(buffer, *rect, &row.label, reserve) else {
            continue;
        };
        let cue = format!("{}  {}  {}", row.location, row.summary, row.age);
        let style = design::meta(cx.palette).bg(buffer[(detail.x, detail.y)].bg);
        let cue = super::settings::truncate_with_ellipsis(&cue, detail.width);
        super::super::render::put_text(buffer, detail.x, detail.y, detail.width, &cue, style);
    }
}

fn reply_notice(state: &State) -> Option<&str> {
    state.error.as_deref().or_else(|| {
        (state.preview && !state.can_reply).then_some(tr(
            "Reply unavailable. Jump to the terminal to respond.",
            "不可在此回复，请跳转到终端。",
        ))
    })
}

pub(in crate::client::shell) fn paint(
    buffer: &mut Buffer,
    n: &ClientNavigatorOverlay,
    cx: &super::super::feedback::ChromeContext<'_>,
) -> Option<super::super::render::OverlayRender> {
    let state = &n.overview;
    let geometry = state.layout;
    if geometry.popup.is_empty() {
        return None;
    }
    let palette = cx.palette;
    super::super::render::panel(
        buffer,
        geometry.popup,
        crate::ui::design::frame(palette),
        palette.panel_bg,
        cx.glyphs,
    )?;
    let put = |buffer: &mut Buffer, rect: Rect, text: &str, style: Style| {
        super::super::render::put_text(buffer, rect.x, rect.y, rect.width, text, style)
    };
    put(
        buffer,
        geometry.title,
        tr("Agent overview", "智能体总览"),
        design::title(palette),
    );
    put(
        buffer,
        geometry.counts,
        &state.counts,
        design::meta(palette),
    );
    let cursor = super::super::render::render_search_bar(
        buffer,
        geometry.search,
        &super::super::render::SearchBar {
            focused: n.search_focused,
            query: &n.query,
            hint: tr(
                "/ Search agents, workspaces, machines…",
                "/ 搜索智能体、工作区、机器…",
            ),
            status: None,
            echo_query: true,
            count: None,
        },
        palette,
    );
    let mut result = super::super::render::OverlayRender {
        area: geometry.popup,
        navigator_popup: geometry.popup,
        navigator_search: geometry.search,
        cursor: cursor.or(Some(crate::protocol::CursorState {
            x: geometry.popup.x,
            y: geometry.popup.y,
            visible: false,
            shape: 1,
        })),
        ..Default::default()
    };
    let mut x = geometry.controls.x;
    for (action, text) in [
        (
            Action::Grouping,
            if state.grouping == Grouping::Status {
                tr("g Status", "g 按状态")
            } else {
                tr("g Location", "g 按位置")
            },
        ),
        (Action::Preview, tr("Space Preview", "Space 预览")),
        (Action::Read, "r"),
        (Action::Unread, "u"),
        (Action::ReadAll, "R"),
        (
            Action::Jump,
            if state
                .rows
                .get(state.selected)
                .is_some_and(|row| row.bucket == AgentBucket::NeedsInput)
            {
                tr("↵ Jump and answer", "↵ 跳转并回答")
            } else {
                tr("↵ Jump", "↵ 跳转")
            },
        ),
        (Action::Reply, tr("e Reply", "e 回复")),
        (Action::Close, "Esc"),
    ] {
        let (key, label) = text.split_once(' ').unwrap_or((text, ""));
        let width = crate::ui::kit::key_cap::hint_width(key, label);
        if x + width > geometry.controls.right() {
            break;
        }
        let rect = Rect::new(x, geometry.controls.y, width, 1);
        let enabled = match action {
            Action::Reply => state.can_reply,
            Action::Jump => state.rows.get(state.selected).is_some_and(|row| !row.stale),
            Action::Read => state.can_read,
            Action::Unread => state.can_unread,
            _ => true,
        };
        let style = if !enabled {
            design::meta(palette).add_modifier(Modifier::DIM)
        } else if state.hovered.as_ref() == Some(&action) {
            design::body(palette).bg(palette.hover_row_bg())
        } else {
            design::muted(palette).bg(palette.surface0)
        };
        crate::ui::kit::key_cap::paint_hint(
            buffer,
            rect.x,
            rect.y,
            rect.right(),
            key,
            label,
            if enabled {
                design::key_cap(palette)
            } else {
                style
            },
            style,
        );
        if enabled {
            result.overview_actions.push((rect, action));
        }
        x += width + 1;
    }
    let glyphs = state
        .glyphs
        .unwrap_or_else(|| StatusGlyphs::fixed(crate::config::StatusIndicatorStyle::Graphite));
    for (index, row) in state
        .rows
        .iter()
        .enumerate()
        .skip(n.scroll)
        .take(usize::from(geometry.list.height))
    {
        let rect = Rect::new(
            geometry.list.x,
            geometry.list.y + (index - n.scroll) as u16,
            geometry.list.width.saturating_sub(1),
            1,
        );
        let style = if row.stale {
            design::meta(palette).add_modifier(Modifier::DIM)
        } else if index == state.selected {
            design::selected_row_text(palette)
        } else if state.hovered == Some(Action::Row(index)) {
            design::body(palette).bg(palette.hover_row_bg())
        } else {
            design::body(palette)
        };
        buffer.set_style(rect, style);
        let indent = usize::from(row.depth).min(3) * 2 + 1;
        let budget = rect.width.saturating_sub(2 + indent as u16);
        let meta = if budget >= 90 {
            format!("{}  {}  {}", row.location, row.summary, row.age)
        } else if budget >= 48 {
            format!("{}  {}", row.summary, row.age)
        } else {
            row.age.clone()
        };
        let name_budget = if meta.is_empty() {
            budget
        } else {
            budget.min(32).max(budget / 2)
        };
        let label = crate::ui::truncate_end(&row.label, usize::from(name_budget));
        put(
            buffer,
            rect,
            &format!(
                "{}{} {label}",
                " ".repeat(indent),
                glyphs.bucket(row.bucket)
            ),
            style,
        );
        if row.group.is_none() {
            super::super::render::put_text(
                buffer,
                rect.x + indent as u16,
                rect.y,
                1,
                glyphs.bucket(row.bucket),
                if row.stale {
                    style
                } else {
                    design::status_style(row.bucket, palette, cx.components)
                },
            );
        }
        if budget > name_budget + 2 {
            let meta_rect = Rect::new(
                rect.x + indent as u16 + 2 + name_budget + 1,
                rect.y,
                budget - name_budget - 1,
                1,
            );
            put(
                buffer,
                meta_rect,
                &crate::ui::truncate_end(&meta, usize::from(meta_rect.width)),
                design::meta(palette),
            );
        }
        if index == state.selected && row.group.is_none() {
            crate::ui::kit::paint_selection_mark(buffer, rect.x, rect.y, palette);
        }
        result.overview_actions.push((rect, Action::Row(index)));
        if let Some(target) = &row.target {
            result.navigator_rows.push((rect, target.clone()));
        }
    }
    if !geometry.list.is_empty() {
        let max = state
            .rows
            .len()
            .saturating_sub(usize::from(geometry.list.height));
        let metrics = crate::pane::ScrollMetrics {
            offset_from_bottom: max.saturating_sub(n.scroll),
            max_offset_from_bottom: max,
            viewport_rows: usize::from(geometry.list.height),
        };
        if max > 0 {
            let track = Rect::new(
                geometry.list.right() - 1,
                geometry.list.y,
                1,
                geometry.list.height,
            );
            crate::ui::render_scrollbar_buffer(
                buffer,
                metrics,
                track,
                palette.surface1,
                palette.overlay1,
                "▐",
            );
            result.navigator_scrollbar = track;
        }
        result.navigator_scroll_metrics = Some(metrics);
    }
    for (offset, line) in state
        .lines
        .iter()
        .skip(state.preview_scroll)
        .take(usize::from(geometry.preview.height))
        .enumerate()
    {
        put(
            buffer,
            Rect::new(
                geometry.preview.x + 1,
                geometry.preview.y + offset as u16,
                geometry.preview.width.saturating_sub(2),
                1,
            ),
            line,
            if offset == 0 && state.preview_scroll == 0 {
                design::title(palette)
            } else {
                design::body(palette)
            },
        );
    }
    if state.editing && !geometry.editor.is_empty() {
        buffer.set_style(geometry.editor, design::body(palette).bg(palette.surface0));
        for (offset, line) in state
            .editor_lines
            .iter()
            .skip(state.editor_scroll)
            .take(usize::from(geometry.editor.height))
            .enumerate()
        {
            put(
                buffer,
                Rect::new(
                    geometry.editor.x + 1,
                    geometry.editor.y + offset as u16,
                    geometry.editor.width.saturating_sub(2),
                    1,
                ),
                line,
                design::body(palette).bg(palette.surface0),
            );
        }
        if let Some((x, y)) = state.editor_cursor {
            result.cursor = Some(crate::protocol::CursorState {
                x: geometry.editor.x + 1 + x.min(geometry.editor.width.saturating_sub(3)),
                y: geometry.editor.y + y,
                visible: state.in_flight.is_none(),
                shape: 1,
            });
        }
        let label = if state.in_flight.is_some() {
            tr("Sending…", "发送中…")
        } else {
            tr("Send", "发送")
        };
        let width =
            crate::ui::kit::key_cap::hint_width("ctrl+enter", label).min(geometry.footer.width);
        let send = Rect::new(
            geometry.footer.right().saturating_sub(width),
            geometry.footer.y,
            width,
            1,
        );
        if state.in_flight.is_none() && state.can_reply {
            result.overview_actions.push((send, Action::Send));
        }
        crate::ui::kit::key_cap::paint_hint(
            buffer,
            send.x,
            send.y,
            send.right(),
            "ctrl+enter",
            label,
            design::key_cap(palette),
            design::title(palette),
        );
        let notice = Rect::new(
            geometry.footer.x,
            geometry.footer.y,
            geometry.footer.width.saturating_sub(width + 1),
            1,
        );
        if let Some(message) = reply_notice(state) {
            put(buffer, notice, message, design::meta(palette));
        } else {
            use crate::ui::kit::footer_hints::{render_footer_hints, FooterHint};
            render_footer_hints(
                buffer,
                notice,
                &[
                    FooterHint::new("enter", tr("Newline", "换行")),
                    FooterHint::new("esc", tr("Back", "返回")),
                ],
                None,
                palette,
            );
        }
    } else {
        if let Some(message) = reply_notice(state) {
            put(buffer, geometry.footer, message, design::meta(palette));
        } else {
            use crate::ui::kit::footer_hints::{render_footer_hints, FooterHint};
            render_footer_hints(
                buffer,
                geometry.footer,
                &[
                    FooterHint::new("↑↓", tr("Select", "选择")),
                    FooterHint::new("space", tr("Preview", "预览")),
                    FooterHint {
                        primary: true,
                        ..FooterHint::new("enter", tr("Jump", "跳转"))
                    },
                    FooterHint::new("r / u / R", tr("Read", "已读")),
                    FooterHint {
                        enabled: state.can_reply,
                        ..FooterHint::new("e", tr("Reply", "回复"))
                    },
                ],
                None,
                palette,
            );
        }
    }
    Some(result)
}
