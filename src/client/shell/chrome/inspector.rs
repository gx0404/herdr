//! Client-owned insight preview. Target identity fences endpoint, server boot,
//! pane and launch; read generations additionally fence late activity responses.
//! Cache mutation and follow scheduling happen before paint, never in rendering.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, MouseButton, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use unicode_segmentation::UnicodeSegmentation;

use super::super::*;
use crate::api::schema::{
    AgentActivityNode, AgentActivityReadParams, AgentActivityStatus, Method, ResponseResult,
};
use crate::harness::AgentBucket;
use crate::protocol::{ClientShellAgent, ClientShellSnapshot};
use crate::ui::design;

#[cfg(test)]
mod tests;

const TIMELINE_AGENTS: usize = 128;
const TIMELINE_ENTRIES: usize = 8;
const TREE_CACHE: usize = 8;
const TREE_NODES: usize = 512;
// Per-target retained node text, not a bound on the complete client allocation.
const TREE_BYTES: usize = 128 * 1024;
const FOLLOW: Duration = super::super::agent_activity_overlay::FOLLOW_INTERVAL;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(in crate::client::shell) struct Target {
    pub(in crate::client::shell) endpoint: ClientEndpointId,
    pub(in crate::client::shell) boot: String,
    pub(in crate::client::shell) pane: String,
    pub(in crate::client::shell) launch: u64,
}

impl Target {
    pub(in crate::client::shell) fn for_agent(
        endpoint: ClientEndpointId,
        snapshot: &ClientShellSnapshot,
        agent: &ClientShellAgent,
    ) -> Self {
        Self {
            endpoint,
            boot: snapshot.boot_id.clone(),
            pane: agent.pane_id.clone(),
            launch: agent.launch_seq,
        }
    }

    pub(in crate::client::shell) fn agent<'a>(
        &self,
        snapshot: &'a ClientShellSnapshot,
    ) -> Option<&'a ClientShellAgent> {
        (snapshot.boot_id == self.boot)
            .then(|| {
                snapshot
                    .agents
                    .iter()
                    .find(|agent| agent.pane_id == self.pane && agent.launch_seq == self.launch)
            })
            .flatten()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::client::shell) enum Action {
    Jump,
    Activity(Option<String>),
    Refresh,
    Collapse(String),
}

#[derive(Clone, Debug)]
struct Transition {
    bucket: AgentBucket,
    at: u64,
}

#[derive(Default)]
struct Tree {
    nodes: Vec<AgentActivityNode>,
    loaded: bool,
    truncated: bool,
    unsupported: bool,
    not_reported: bool,
    error: Option<String>,
    collapsed: HashSet<String>,
    next_read: Option<Instant>,
}

#[derive(Clone, Copy)]
enum Tone {
    Title,
    Body,
    Muted,
    Meta,
    Section,
    Input,
    Failed,
}

struct Row {
    text: String,
    tone: Tone,
    bucket: Option<AgentBucket>,
    action: Option<Action>,
}

type ViewKey = (Option<Target>, u64, u16, u64, u64, u64);

#[derive(Default)]
pub(in crate::client::shell) struct State {
    pub(in crate::client::shell) target: Option<Target>,
    preview: Option<Target>,
    overview_preview: bool,
    timelines: HashMap<Target, VecDeque<Transition>>,
    timeline_order: VecDeque<Target>,
    trees: HashMap<Target, Tree>,
    tree_order: VecDeque<Target>,
    pub(in crate::client::shell) in_flight: Option<u64>,
    version: u64,
    rows: Vec<Row>,
    key: Option<ViewKey>,
    pub(in crate::client::shell) area: Rect,
    scroll: usize,
    max_scroll: usize,
    selected: Option<usize>,
    reveal: bool,
    minute: u64,
    animated: bool,
}

impl State {
    pub(in crate::client::shell) fn animated(&self) -> bool {
        !self.area.is_empty() && self.animated
    }

    pub(in crate::client::shell) fn observe_snapshot(
        &mut self,
        endpoint: &ClientEndpointId,
        snapshot: &ClientShellSnapshot,
    ) {
        let now = now_ms();
        self.timelines
            .retain(|target, _| &target.endpoint != endpoint || target.agent(snapshot).is_some());
        self.trees
            .retain(|target, _| &target.endpoint != endpoint || target.agent(snapshot).is_some());
        self.timeline_order
            .retain(|target| self.timelines.contains_key(target));
        self.tree_order
            .retain(|target| self.trees.contains_key(target));
        for agent in &snapshot.agents {
            let target = Target::for_agent(endpoint.clone(), snapshot, agent);
            let bucket = client_agent_bucket(agent);
            if !self.timelines.contains_key(&target) {
                while self.timelines.len() >= TIMELINE_AGENTS {
                    if let Some(old) = self.timeline_order.pop_front() {
                        self.timelines.remove(&old);
                    } else {
                        break;
                    }
                }
                self.timeline_order.push_back(target.clone());
            }
            let timeline = self.timelines.entry(target).or_default();
            if timeline.back().is_none_or(|entry| entry.bucket != bucket) {
                if timeline.len() == TIMELINE_ENTRIES {
                    timeline.pop_front();
                }
                timeline.push_back(Transition {
                    bucket,
                    at: agent.insight.state_changed_at_ms.unwrap_or(now).min(now),
                });
                self.version = self.version.saturating_add(1);
            }
        }
    }

    pub(in crate::client::shell) fn prune_endpoints(&mut self, endpoints: &[ClientShellEndpoint]) {
        let keep = |target: &Target| {
            endpoints.iter().any(|endpoint| {
                endpoint.endpoint_id == target.endpoint
                    && endpoint
                        .snapshot
                        .as_deref()
                        .is_some_and(|snapshot| target.agent(snapshot).is_some())
            })
        };
        self.timelines.retain(|target, _| keep(target));
        self.trees.retain(|target, _| keep(target));
        self.timeline_order
            .retain(|target| self.timelines.contains_key(target));
        self.tree_order
            .retain(|target| self.trees.contains_key(target));
    }

    fn switch_target(&mut self, target: Option<Target>) -> bool {
        if self.target == target {
            return false;
        }
        self.target = target.clone();
        self.in_flight = None;
        self.scroll = 0;
        self.selected = None;
        self.key = None;
        if let Some(target) = target {
            if !self.trees.contains_key(&target) {
                while self.trees.len() >= TREE_CACHE {
                    if let Some(old) = self.tree_order.pop_front() {
                        self.trees.remove(&old);
                    } else {
                        break;
                    }
                }
                self.trees.insert(target.clone(), Tree::default());
            }
            self.tree_order.retain(|old| old != &target);
            self.tree_order.push_back(target);
        }
        true
    }

    fn apply_tree(&mut self, target: &Target, nodes: Vec<AgentActivityNode>) -> bool {
        let Some(tree) = self.trees.get_mut(target) else {
            return false;
        };
        let mut bytes = 0;
        let mut kept = Vec::new();
        let mut truncated = nodes.len() > TREE_NODES;
        let mut seen = HashSet::new();
        for mut node in nodes.into_iter().take(TREE_NODES) {
            if node.id.len() > 256 || !seen.insert(node.id.clone()) {
                truncated = true;
                continue;
            }
            node.label = bounded(&node.label, 512);
            node.summary = node.summary.map(|text| bounded(&text, 1024));
            node.agent_type = node.agent_type.map(|text| bounded(&text, 64));
            node.parent_id = node.parent_id.filter(|text| text.len() <= 256);
            node.content_ref = node.content_ref.filter(|text| text.len() <= 512);
            let size = node.id.len()
                + node.label.len()
                + node.summary.as_ref().map_or(0, String::len)
                + node.agent_type.as_ref().map_or(0, String::len)
                + node.parent_id.as_ref().map_or(0, String::len)
                + node.content_ref.as_ref().map_or(0, String::len);
            if bytes + size > TREE_BYTES {
                truncated = true;
                break;
            }
            bytes += size;
            kept.push(node);
        }
        tree.collapsed
            .retain(|id| kept.iter().any(|node| &node.id == id));
        let changed = !tree.loaded
            || tree.nodes != kept
            || tree.truncated != truncated
            || tree.error.is_some();
        tree.nodes = kept;
        tree.loaded = true;
        tree.truncated = truncated;
        tree.error = None;
        tree.next_read = Some(Instant::now() + FOLLOW);
        if changed {
            self.version = self.version.saturating_add(1);
        }
        changed
    }
}

fn now_ms() -> u64 {
    crate::harness::time::system_time_ms(std::time::SystemTime::now()).unwrap_or_default()
}

fn bounded(text: &str, cap: usize) -> String {
    let mut result = String::new();
    for ch in text.chars().filter(|ch| !ch.is_control() || *ch == '\n') {
        if result.len() + ch.len_utf8() > cap {
            break;
        }
        result.push(ch);
    }
    result
}

fn label(bucket: AgentBucket) -> &'static str {
    use super::super::observability::tr;
    match bucket {
        AgentBucket::NeedsInput => tr("Needs your input", "需要你的输入"),
        AgentBucket::Failed => tr("Failed", "失败"),
        AgentBucket::Working => tr("Working", "工作中"),
        AgentBucket::Review => tr("Ready to review", "待查看"),
        AgentBucket::Idle => tr("Idle", "空闲"),
        AgentBucket::None => tr("Unknown", "未知"),
    }
}

fn activity_bucket(status: AgentActivityStatus) -> AgentBucket {
    design::activity_bucket(status)
}

fn add(rows: &mut Vec<Row>, text: &str, tone: Tone, width: u16) {
    let text = bounded(text, 4096);
    for line in text.lines().take(64) {
        let mut wrapped = String::new();
        let mut used = 0u16;
        for grapheme in line.graphemes(true) {
            let cell = crate::ui::display_width_u16(grapheme);
            if cell > width {
                continue;
            }
            if used.saturating_add(cell) > width {
                rows.push(Row {
                    text: std::mem::take(&mut wrapped),
                    tone,
                    bucket: None,
                    action: None,
                });
                used = 0;
            }
            wrapped.push_str(grapheme);
            used += cell;
        }
        rows.push(Row {
            text: wrapped,
            tone,
            bucket: None,
            action: None,
        });
    }
}

fn section(rows: &mut Vec<Row>, text: &str, width: u16) {
    rows.push(Row {
        text: String::new(),
        tone: Tone::Body,
        bucket: None,
        action: None,
    });
    add(rows, text, Tone::Section, width);
}

fn button(rows: &mut Vec<Row>, text: &str, action: Action, width: u16) {
    rows.push(Row {
        text: crate::ui::truncate_end(text, usize::from(width)),
        tone: Tone::Body,
        bucket: None,
        action: Some(action),
    });
}

impl ClientShellState {
    pub(in crate::client::shell) fn toggle_inspector(&mut self, outcome: &mut ClientShellInput) {
        if !self.workbench.enabled {
            return;
        }
        let key = self.active_endpoint_id.storage_key();
        let cols = self.last_composed_size.map_or(120, |size| size.0);
        let visible = self
            .workbench
            .inspector_visibility
            .get(&key)
            .copied()
            .unwrap_or_default()
            .visible(cols)
            && self.workbench.dock.root.contains(&dock::PanelId::Agents);
        let visibility = if visible {
            dock::InspectorVisibility::Hidden
        } else {
            dock::InspectorVisibility::Shown
        };
        self.workbench.inspector_visibility.insert(key, visibility);
        if !visible && !self.workbench.dock.root.contains(&dock::PanelId::Agents) {
            let focused = self.workbench.dock.focused.clone();
            self.workbench_open(dock::PanelId::Agents);
            self.workbench.dock.focused = focused;
        }
        if visible && self.workbench.dock.focused == dock::PanelId::Agents {
            self.refocus_current_workbench_terminal();
        }
        self.tick_inspector(Instant::now(), outcome);
        self.schedule_chrome_preferences(Instant::now());
        outcome.repaint = true;
        outcome.resize = true;
    }

    pub(in crate::client::shell) fn set_inspector_preview_target(
        &mut self,
        target: Option<Target>,
    ) {
        self.inspector.overview_preview = false;
        if self.inspector.preview != target {
            self.inspector.preview = target;
            self.inspector.key = None;
        }
    }

    pub(in crate::client::shell) fn set_overview_inspector_target(
        &mut self,
        target: Option<Target>,
    ) {
        self.set_inspector_preview_target(target);
        self.inspector.overview_preview = true;
    }

    pub(in crate::client::shell) fn clear_overview_inspector_target(&mut self) {
        if self.inspector.overview_preview {
            self.set_inspector_preview_target(None);
        }
    }

    fn inspector_snapshot(&self, endpoint: &ClientEndpointId) -> Option<&ClientShellSnapshot> {
        if endpoint == &self.active_endpoint_id {
            self.snapshot.as_deref()
        } else {
            self.endpoints
                .iter()
                .find(|entry| &entry.endpoint_id == endpoint)
                .and_then(|entry| entry.snapshot.as_deref())
        }
    }

    fn inspector_target(&self) -> Option<Target> {
        if let Some(target) = &self.inspector.preview {
            if self
                .inspector_snapshot(&target.endpoint)
                .and_then(|snapshot| target.agent(snapshot))
                .is_some()
            {
                return Some(target.clone());
            }
        }
        let snapshot = self.snapshot.as_deref()?;
        let focused = snapshot.focused_pane_id.as_deref()?;
        let agent = snapshot
            .agents
            .iter()
            .find(|agent| agent.pane_id == focused)?;
        Some(Target::for_agent(
            self.active_endpoint_id.clone(),
            snapshot,
            agent,
        ))
    }

    pub(in crate::client::shell) fn sync_inspector_target(&mut self, area: Rect) {
        let target = (!area.is_empty())
            .then(|| self.inspector_target())
            .flatten();
        if self.inspector.switch_target(target) {
            self.pending_requests.retain(|_, pending| {
                !matches!(pending.kind, PendingEndpointKind::InspectorRead { .. })
            });
        }
        self.inspector.area = area;
    }

    pub(crate) fn tick_inspector(&mut self, now: Instant, outcome: &mut ClientShellInput) {
        let area = if self.workbench.enabled {
            self.last_composed_size
                .and_then(|(cols, rows)| {
                    let geometry = self.workbench.compute_geometry(cols, rows);
                    geometry
                        .panels
                        .iter()
                        .find(|(panel, _)| *panel == dock::PanelId::Agents)
                        .map(|(panel, area)| workbench::body(*area, panel, geometry.compact))
                })
                .unwrap_or_default()
        } else {
            Rect::default()
        };
        self.sync_inspector_target(area);
        let minute = now_ms() / 60_000;
        if !area.is_empty() && self.inspector.minute != minute {
            self.inspector.minute = minute;
            outcome.repaint = true;
        }
        if self.overlay.is_some() || self.inspector.in_flight.is_some() {
            return;
        }
        let Some(target) = self.inspector.target.clone() else {
            return;
        };
        if !self.endpoint_is_online(&target.endpoint) {
            return;
        }
        let Some(tree) = self.inspector.trees.get(&target) else {
            return;
        };
        if tree.unsupported || tree.next_read.is_some_and(|at| now < at) {
            return;
        }
        let method = Method::AgentActivityRead(AgentActivityReadParams {
            pane_id: Some(target.pane.clone()),
            follow: true,
            ..Default::default()
        });
        if !self.supports_endpoint_method_for(&target.endpoint, &method) {
            if let Some(tree) = self.inspector.trees.get_mut(&target) {
                tree.unsupported = true;
            }
            self.inspector.version = self.inspector.version.saturating_add(1);
            outcome.repaint = true;
            return;
        }
        let epoch = self.next_request_id;
        if self.push_endpoint_method_for(
            &target.endpoint,
            method,
            PendingEndpointKind::InspectorRead {
                epoch,
                target: target.clone(),
            },
            outcome,
        ) {
            self.inspector.in_flight = Some(epoch);
        }
    }

    pub(in crate::client::shell) fn receive_inspector_read(
        &mut self,
        epoch: u64,
        target: &Target,
        boot: &str,
        result: Result<ResponseResult, ClientShellEndpointError>,
    ) -> bool {
        if self.inspector.in_flight != Some(epoch) || self.inspector.target.as_ref() != Some(target)
        {
            return false;
        }
        self.inspector.in_flight = None;
        if boot != target.boot
            || self
                .inspector_snapshot(&target.endpoint)
                .and_then(|snapshot| target.agent(snapshot))
                .is_none()
            || self.inspector.area.is_empty()
        {
            return false;
        }
        match result {
            Ok(ResponseResult::AgentActivity { nodes, .. }) => {
                return self.inspector.apply_tree(target, nodes);
            }
            Err(error) => {
                let Some(tree) = self.inspector.trees.get_mut(target) else {
                    return false;
                };
                if error.code.as_deref() == Some("endpoint_cancelled") {
                    tree.next_read = Some(Instant::now() + FOLLOW);
                    return false;
                }
                if error.code.as_deref() == Some("not_implemented") {
                    tree.not_reported = true;
                    tree.unsupported = true;
                    tree.loaded = true;
                    tree.error = None;
                    tree.next_read = None;
                } else {
                    tree.unsupported =
                        super::super::agent_activity_overlay::method_unsupported(&error);
                    tree.error = Some(bounded(&error.message, 1024));
                    tree.next_read = Some(Instant::now() + Duration::from_secs(5));
                }
                self.inspector.version = self.inspector.version.saturating_add(1);
            }
            _ => return false,
        }
        true
    }

    pub(in crate::client::shell) fn compute_inspector_view(&mut self, area: Rect) {
        self.sync_inspector_target(area);
        if area.is_empty() {
            self.inspector.rows.clear();
            self.inspector.key = None;
            return;
        }
        let width = area.width.saturating_sub(3).max(1);
        let target = self.inspector.target.clone();
        let snapshot = target
            .as_ref()
            .and_then(|target| self.inspector_snapshot(&target.endpoint));
        let revision = snapshot.map_or(0, |snapshot| snapshot.revision);
        let minute = now_ms() / 60_000;
        let key = (
            target.clone(),
            revision,
            width,
            minute,
            self.inspector.version,
            self.config_epoch,
        );
        if self.inspector.key.as_ref() != Some(&key) {
            let mut rows = Vec::new();
            if let Some((target, snapshot, agent)) = target.as_ref().and_then(|target| {
                let snapshot = self.inspector_snapshot(&target.endpoint)?;
                Some((target, snapshot, target.agent(snapshot)?))
            }) {
                self.build_inspector_rows(&mut rows, target, snapshot, agent, width, now_ms());
            } else {
                add(
                    &mut rows,
                    super::super::observability::tr("Terminal session", "终端会话"),
                    Tone::Title,
                    width,
                );
                section(
                    &mut rows,
                    super::super::observability::tr("No agent selected", "未选择智能体"),
                    width,
                );
                add(
                    &mut rows,
                    super::super::observability::tr(
                        "Focus an agent terminal to inspect its status and activity.",
                        "聚焦智能体终端，查看其状态和活动。",
                    ),
                    Tone::Muted,
                    width,
                );
            }
            self.inspector.animated = rows.iter().any(|row| {
                row.bucket == Some(AgentBucket::Working) && !matches!(row.tone, Tone::Meta)
            });
            self.inspector.rows = rows;
            self.inspector.key = Some(key);
            self.inspector.minute = minute;
        }
        let visible = usize::from(area.height);
        self.inspector.max_scroll = self.inspector.rows.len().saturating_sub(visible);
        self.inspector.scroll = self.inspector.scroll.min(self.inspector.max_scroll);
        if let Some(selected) = self.inspector.selected.as_mut() {
            *selected = (*selected).min(self.inspector.rows.len().saturating_sub(1));
            if self.inspector.reveal {
                self.inspector.scroll = page::list_start(
                    self.inspector.scroll,
                    *selected,
                    self.inspector.rows.len(),
                    visible,
                    true,
                );
            }
        }
        self.inspector.reveal = false;
    }

    fn build_inspector_rows(
        &self,
        rows: &mut Vec<Row>,
        target: &Target,
        snapshot: &ClientShellSnapshot,
        agent: &ClientShellAgent,
        width: u16,
        now: u64,
    ) {
        use super::super::observability::tr;
        let insight = &agent.insight;
        let title = insight
            .session_title
            .as_deref()
            .or(agent.title.as_deref())
            .or(agent.name.as_deref())
            .or(agent.display_agent.as_deref())
            .or(agent.agent.as_deref())
            .unwrap_or(&agent.pane_id);
        add(rows, title, Tone::Title, width);
        let bucket = client_agent_bucket(agent);
        let elapsed = if bucket == AgentBucket::Working {
            insight
                .working_since_ms
                .map(|start| now.saturating_sub(start))
        } else {
            insight.last_turn_ms
        };
        let status = elapsed.map_or_else(
            || label(bucket).to_owned(),
            |elapsed| {
                format!(
                    "{} · {}",
                    label(bucket),
                    if bucket == AgentBucket::Working {
                        if elapsed < 60_000 {
                            "<1m".to_owned()
                        } else {
                            design::compact_age(elapsed / 60_000 * 60_000)
                        }
                    } else {
                        design::duration_label(elapsed)
                    }
                )
            },
        );
        rows.push(Row {
            text: crate::ui::truncate_end(&status, usize::from(width.saturating_sub(2))),
            tone: Tone::Body,
            bucket: Some(bucket),
            action: None,
        });
        let workspace = snapshot
            .workspaces
            .iter()
            .find(|ws| ws.workspace_id == agent.workspace_id);
        let tab = snapshot.tabs.iter().find(|tab| tab.tab_id == agent.tab_id);
        section(rows, tr("Location", "位置"), width);
        add(
            rows,
            &format!(
                "{} / {}",
                self.endpoint_label(&target.endpoint),
                workspace.map_or(agent.workspace_id.as_str(), |ws| ws.label.as_str())
            ),
            Tone::Muted,
            width,
        );
        if let Some(branch) = workspace.and_then(|ws| ws.branch.as_deref()) {
            add(rows, branch, Tone::Meta, width);
        }
        add(
            rows,
            tab.map_or(agent.tab_id.as_str(), |tab| tab.label.as_str()),
            Tone::Meta,
            width,
        );
        if let Some(model) = insight.model.as_deref() {
            add(rows, model, Tone::Meta, width);
        }
        let attention = insight.attention.as_ref();
        if bucket == AgentBucket::NeedsInput {
            section(rows, tr("Waiting for your answer", "等待回答"), width);
            add(
                rows,
                insight
                    .question
                    .as_deref()
                    .or_else(|| {
                        attention.and_then(|attention| {
                            attention.detail.as_deref().or(attention.title.as_deref())
                        })
                    })
                    .unwrap_or(tr(
                        "Answer in the agent terminal.",
                        "请在智能体终端中回答。",
                    )),
                Tone::Input,
                width,
            );
            let options = if insight.options.is_empty() {
                attention
                    .map(|attention| attention.options.as_slice())
                    .unwrap_or_default()
            } else {
                &insight.options
            };
            for option in options.iter().take(16) {
                add(rows, &format!("· {option}"), Tone::Muted, width);
            }
            button(
                rows,
                tr("↵ Jump and answer", "↵ 跳转并回答"),
                Action::Jump,
                width,
            );
        } else if bucket == AgentBucket::Failed {
            section(rows, tr("Failure", "失败原因"), width);
            add(
                rows,
                insight
                    .error
                    .as_deref()
                    .or_else(|| {
                        attention.and_then(|attention| {
                            attention.detail.as_deref().or(attention.title.as_deref())
                        })
                    })
                    .unwrap_or(tr("No failure details available.", "没有可用的失败详情。")),
                Tone::Failed,
                width,
            );
        }
        if let Some(tool) = insight.current_tool.as_deref() {
            section(rows, tr("Recent activity", "最近活动"), width);
            add(rows, tool, Tone::Muted, width);
        }
        if let Some(message) = insight.last_message.as_deref() {
            section(rows, tr("Last message", "最近消息"), width);
            add(rows, message, Tone::Body, width);
        }
        if let Some(prompt) = insight.last_prompt.as_deref() {
            section(rows, tr("Last prompt", "最近提示"), width);
            add(rows, prompt, Tone::Muted, width);
        }
        section(rows, tr("Activity tree", "活动树"), width);
        if let Some(tree) = self.inspector.trees.get(target) {
            if tree.not_reported {
                add(
                    rows,
                    crate::i18n::texts().agent_activity.not_reported,
                    Tone::Meta,
                    width,
                );
            } else if tree.unsupported {
                add(
                    rows,
                    crate::i18n::texts().agent_activity.unsupported,
                    Tone::Meta,
                    width,
                );
            } else {
                if let Some(error) = tree.error.as_deref() {
                    add(rows, error, Tone::Failed, width);
                }
                if !tree.loaded {
                    add(
                        rows,
                        crate::i18n::texts().agent_activity.loading,
                        Tone::Meta,
                        width,
                    );
                } else if tree.nodes.is_empty() {
                    add(
                        rows,
                        crate::i18n::texts().agent_activity.empty_tree,
                        Tone::Meta,
                        width,
                    );
                }
                for entry in
                    super::super::agent_activity_overlay::build_rows(&tree.nodes, &tree.collapsed)
                {
                    let node = &tree.nodes[entry.kind];
                    let indent = "  ".repeat(usize::from(entry.depth).min(8));
                    let toggle = if entry.has_children {
                        if entry.collapsed {
                            "▸"
                        } else {
                            "▾"
                        }
                    } else {
                        " "
                    };
                    let text = format!("{indent}{toggle} {}", node.label);
                    rows.push(Row {
                        text: crate::ui::truncate_end(&text, usize::from(width.saturating_sub(2))),
                        tone: Tone::Body,
                        bucket: Some(activity_bucket(node.status)),
                        action: Some(if entry.has_children {
                            Action::Collapse(node.id.clone())
                        } else {
                            Action::Activity(Some(node.id.clone()))
                        }),
                    });
                }
                if tree.truncated {
                    add(
                        rows,
                        crate::i18n::texts().agent_activity.truncated,
                        Tone::Meta,
                        width,
                    );
                }
            }
        }
        section(rows, tr("State timeline", "状态时间线"), width);
        if let Some(timeline) = self.inspector.timelines.get(target) {
            for entry in timeline.iter().rev() {
                let age = design::compact_age(now.saturating_sub(entry.at) / 60_000 * 60_000);
                rows.push(Row {
                    text: crate::ui::truncate_end(
                        &format!("{}  {age}", label(entry.bucket)),
                        usize::from(width.saturating_sub(2)),
                    ),
                    tone: Tone::Meta,
                    bucket: Some(entry.bucket),
                    action: None,
                });
            }
        }
        section(rows, tr("Actions", "快捷操作"), width);
        button(
            rows,
            tr("↵ Jump to terminal", "↵ 跳转到终端"),
            Action::Jump,
            width,
        );
        button(
            rows,
            tr("d  Detailed activity", "d  详细活动"),
            Action::Activity(None),
            width,
        );
        button(
            rows,
            tr("r  Refresh activity", "r  刷新活动"),
            Action::Refresh,
            width,
        );
    }

    pub(in crate::client::shell) fn activate_inspector(
        &mut self,
        action: Action,
        outcome: &mut ClientShellInput,
    ) {
        let Some(target) = self.inspector.target.clone() else {
            return;
        };
        if self
            .inspector_snapshot(&target.endpoint)
            .and_then(|snapshot| target.agent(snapshot))
            .is_none()
        {
            return;
        }
        match action {
            Action::Jump => {
                self.set_inspector_preview_target(None);
                self.refocus_current_workbench_terminal();
                self.focus_or_activate(
                    target.endpoint,
                    ClientEndpointFocusTarget::Pane(target.pane),
                    outcome,
                );
            }
            Action::Activity(node) => {
                self.open_agent_activity(
                    target.endpoint,
                    super::super::agent_activity_overlay::AgentActivityOwner::Pane {
                        pane_id: target.pane,
                    },
                    outcome,
                );
                if let Some(node) = node {
                    self.select_agent_activity_node(node, outcome);
                }
            }
            Action::Refresh => {
                if let Some(tree) = self.inspector.trees.get_mut(&target) {
                    if !tree.unsupported {
                        tree.next_read = None;
                    }
                }
                self.tick_inspector(Instant::now(), outcome);
            }
            Action::Collapse(id) => {
                if let Some(tree) = self.inspector.trees.get_mut(&target) {
                    if !tree.collapsed.remove(&id) {
                        tree.collapsed.insert(id);
                    }
                    self.inspector.version = self.inspector.version.saturating_add(1);
                }
            }
        }
        outcome.repaint = true;
    }

    pub(in crate::client::shell) fn inspector_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) -> bool {
        if !self.workbench.enabled
            || self.workbench.dock.focused != dock::PanelId::Agents
            || self.workbench.arranging
            || self.inspector.area.is_empty()
        {
            return false;
        }
        match key.code {
            KeyCode::Esc => self.refocus_current_workbench_terminal(),
            KeyCode::Enter => {
                let action = self
                    .inspector
                    .selected
                    .and_then(|index| self.inspector.rows.get(index))
                    .and_then(|row| row.action.clone())
                    .unwrap_or(Action::Jump);
                self.activate_inspector(action, outcome);
            }
            KeyCode::Char('d') => self.activate_inspector(Action::Activity(None), outcome),
            KeyCode::Char('r') => self.activate_inspector(Action::Refresh, outcome),
            KeyCode::Up | KeyCode::Char('k') | KeyCode::Down | KeyCode::Char('j') => {
                let down = matches!(key.code, KeyCode::Down | KeyCode::Char('j'));
                let selected = self.inspector.selected.unwrap_or(self.inspector.scroll);
                self.inspector.selected = Some(
                    selected
                        .saturating_add_signed(if down { 1 } else { -1 })
                        .min(self.inspector.rows.len().saturating_sub(1)),
                );
                self.inspector.reveal = true;
            }
            KeyCode::PageUp => {
                self.inspector.scroll = self
                    .inspector
                    .scroll
                    .saturating_sub(usize::from(self.inspector.area.height))
            }
            KeyCode::PageDown => {
                self.inspector.scroll = self
                    .inspector
                    .scroll
                    .saturating_add(usize::from(self.inspector.area.height))
                    .min(self.inspector.max_scroll)
            }
            KeyCode::Home => self.inspector.scroll = 0,
            KeyCode::End => self.inspector.scroll = self.inspector.max_scroll,
            _ => {}
        }
        outcome.repaint = true;
        true
    }

    pub(in crate::client::shell) fn inspector_mouse(
        &mut self,
        mouse: MouseEvent,
        outcome: &mut ClientShellInput,
    ) -> bool {
        let point = (mouse.column, mouse.row);
        if !contains(self.hits.inspector_body, point) {
            return false;
        }
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.workbench.dock.focused = dock::PanelId::Agents;
                self.cancel_frozen_selection();
                self.selection = None;
                self.clear_link_hover();
                self.sync_observation_page_with_focus();
                if contains(self.hits.inspector_scrollbar, point) {
                    let span =
                        usize::from(self.hits.inspector_scrollbar.height.saturating_sub(1)).max(1);
                    self.inspector.scroll = self.inspector.max_scroll
                        * usize::from(point.1.saturating_sub(self.hits.inspector_scrollbar.y))
                            .min(span)
                        / span;
                } else {
                    self.inspector.selected = Some(
                        self.inspector.scroll
                            + usize::from(point.1.saturating_sub(self.hits.inspector_body.y)),
                    );
                    if let Some(action) = self
                        .hits
                        .inspector_actions
                        .iter()
                        .find(|(rect, _)| contains(*rect, point))
                        .map(|(_, action)| action.clone())
                    {
                        self.activate_inspector(action, outcome);
                    }
                }
                outcome.repaint = true;
                true
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let lines = self.config.mouse_scroll_lines;
                self.inspector.scroll = if mouse.kind == MouseEventKind::ScrollUp {
                    self.inspector.scroll.saturating_sub(lines)
                } else {
                    self.inspector
                        .scroll
                        .saturating_add(lines)
                        .min(self.inspector.max_scroll)
                };
                outcome.repaint = true;
                true
            }
            _ => false,
        }
    }
}

pub(in crate::client::shell) fn paint(
    buffer: &mut Buffer,
    state: &State,
    config: &ClientShellConfig,
    glyphs: StatusGlyphs,
    hover: Option<&super::super::feedback::ChromeHover>,
    hits: &mut ShellHitMap,
) {
    let area = state.area;
    if area.is_empty() {
        return;
    }
    let palette = &config.palette;
    buffer.set_style(area, Style::default().bg(palette.panel_bg));
    hits.inspector_body = area;
    for (offset, row) in state
        .rows
        .iter()
        .skip(state.scroll)
        .take(usize::from(area.height))
        .enumerate()
    {
        let y = area.y.saturating_add(offset as u16);
        let rect = Rect::new(area.x + 1, y, area.width.saturating_sub(3), 1);
        let mut style = match row.tone {
            Tone::Title => design::title(palette),
            Tone::Body => design::body(palette),
            Tone::Muted => design::muted(palette),
            Tone::Meta => design::meta(palette),
            Tone::Section => design::section(palette),
            Tone::Input => {
                design::status_style(AgentBucket::NeedsInput, palette, &config.components)
                    .bg(palette.surface0)
            }
            Tone::Failed => design::status_style(AgentBucket::Failed, palette, &config.components)
                .bg(palette.surface0),
        };
        let selected = state.selected == Some(state.scroll + offset);
        let hovered = row.action.is_some()
            && matches!(hover, Some(super::super::feedback::ChromeHover::Inspector(candidate)) if *candidate == rect);
        if selected {
            style = style.patch(design::selected_row(palette));
        } else if hovered {
            style = style.bg(palette.hover_row_bg());
        }
        buffer.set_style(rect, style);
        let mut x = rect.x;
        if let Some(bucket) = row.bucket {
            super::super::render::put_text(
                buffer,
                x,
                y,
                rect.width.min(1),
                if matches!(row.tone, Tone::Meta) {
                    StatusGlyphs::fixed(config.status_indicators).bucket(bucket)
                } else {
                    glyphs.bucket(bucket)
                },
                design::status_style(bucket, palette, &config.components),
            );
            x = x.saturating_add(2);
        }
        super::super::render::put_text(
            buffer,
            x,
            y,
            rect.right().saturating_sub(x),
            &row.text,
            style,
        );
        if let Some(action) = &row.action {
            hits.inspector_actions.push((rect, action.clone()));
        }
    }
    if state.max_scroll > 0 && area.width > 1 {
        let track = Rect::new(area.right() - 1, area.y, 1, area.height);
        let metrics = crate::pane::ScrollMetrics {
            offset_from_bottom: state.max_scroll.saturating_sub(state.scroll),
            max_offset_from_bottom: state.max_scroll,
            viewport_rows: usize::from(area.height),
        };
        crate::ui::render_scrollbar_buffer(
            buffer,
            metrics,
            track,
            palette.surface1,
            palette.overlay1,
            "▐",
        );
        hits.inspector_scrollbar = track;
    }
}
