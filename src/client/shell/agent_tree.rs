//! Activity subtrees, collapse identities and keyboard navigation for Sessions.
//! Sessions owns all session rows and geometry; rendering reads precomputed
//! activity rows using the tree primitives in `crate::ui::kit::tree`.
//!
//! 活动摘要：快照默认带每个属主（agent / 外部条目）的计数（运行中 / 活跃 / 完成 /
//! 失败 / 总数）与活跃子集——等待、运行中、受阻的节点连同祖先链（`truncated`
//! 表示还有活跃节点没下发，整树经 `agent.activity.read` 取，归「Agent 活动」
//! 窗口）。只有「在运行」的属主（[`owner_running`]）才带活动子行。
//! 展开后活跃子集按 `parent_id` 前序嵌套（workflow → phase → 子 agent，深度不设
//! 上限，分组节点行首画类型字形并灰显进度），被截断时跟一行「还有 N 项」，有
//! 已结束节点时末尾再跟一行灰显的「已完成 N · 失败 M」（两行都点击打开活动
//! 窗口）。旧 server 的摘要只带最新一个节点，同一套构建照样展开。
//!
//! 折叠键命名空间（继续存 `collapsed_groups` / `remote_collapsed_groups`）：
//! - `agent-panel:<ws>`（工作区，既有）、`agent-tab:<tab>`、`agent-external:<source>`：
//!   集合里有键 = 折叠，默认展开；
//! - `agent-activity:<owner>`（agent / 外部条目下的活动摘要）与
//!   `agent-node:<owner>:<node>`（活动节点的子节点）：**有键 = 展开**，默认折叠——
//!   活动行不抢 agent 行的位置，要看再点开；
//! - 机器层不用键（命中区里写 [`MACHINE_TOGGLE_KEY`]），复用既有
//!   `collapsed_endpoints`，与工作区区的机器行同一份折叠态。
//!
//! `<owner>` 是 `pane:<pane_id>` / `ext:<external_id>`（与 `ChromeHover` /
//! `AgentActivityHit` 的 `owner_key` 同一写法）。

use std::collections::{HashMap, HashSet};

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
};

use super::agent_activity_overlay::AgentActivityOwner;
use super::aggregate_navigation::CachedEndpointSnapshot;
use super::feedback::ChromeHover;
use super::state::AgentActivityHit;
use super::*;
use crate::api::schema::{AgentActivityKind, AgentActivityStatus, AgentStatus};
use crate::protocol::{
    ClientShellActivityNode, ClientShellAgent, ClientShellAgentActivity, ClientShellExternalAgent,
};
use crate::ui::display_width;
use crate::ui::kit::tree::{fill_last_child_masks, render_tree_prefix, TreeEntry};

/// 机器层节点在 `hits.agent_tree_toggles` 里的折叠键：机器层不进
/// `collapsed_groups`，点击时改写 `collapsed_endpoints`。
pub(super) const MACHINE_TOGGLE_KEY: &str = "agent-machine";

#[derive(Debug)]
pub(super) enum AgentTreeKind {
    Activity {
        owner_key: String,
        node_id: String,
        label: String,
        kind: AgentActivityKind,
        status: AgentActivityStatus,
        group: Option<ActivityGroupMark>,
    },
    ActivityMore {
        owner_key: String,
        label: String,
    },
    ActivityDone {
        owner_key: String,
        label: String,
    },
}

#[derive(Debug)]
pub(super) struct AgentTreeNode {
    pub(super) endpoint_id: ClientEndpointId,
    pub(super) stale: bool,
    pub(super) kind: AgentTreeKind,
}

impl AgentTreeNode {
    fn new(endpoint: CachedEndpointSnapshot<'_>, kind: AgentTreeKind) -> Self {
        Self {
            endpoint_id: endpoint.endpoint_id.clone(),
            stale: endpoint.stale(),
            kind,
        }
    }
}

/// 统一树展平后的一行：`TreeEntry` 的深度 / 折叠键 / 末子掩码 + 客户端负载。
pub(super) type AgentTreeRow = TreeEntry<AgentTreeNode>;

/// Agents 面板独立的键盘焦点，用稳定身份定位，绝不从鼠标悬浮推导。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct AgentKeyboardTarget {
    endpoint: ClientEndpointId,
    key: String,
}

impl AgentKeyboardTarget {
    fn row_key(row: &AgentTreeRow) -> std::borrow::Cow<'_, str> {
        match &row.kind.kind {
            AgentTreeKind::ActivityMore { owner_key, .. } => format!("more:{owner_key}").into(),
            AgentTreeKind::ActivityDone { owner_key, .. } => format!("done:{owner_key}").into(),
            _ => row.key.as_str().into(),
        }
    }

    pub(super) fn from_row(row: &AgentTreeRow) -> Self {
        Self {
            endpoint: row.kind.endpoint_id.clone(),
            key: Self::row_key(row).into_owned(),
        }
    }

    pub(super) fn matches(&self, row: &AgentTreeRow) -> bool {
        self.endpoint == row.kind.endpoint_id
            && match &row.kind.kind {
                AgentTreeKind::ActivityMore { owner_key, .. } => {
                    self.key.strip_prefix("more:") == Some(owner_key.as_str())
                }
                AgentTreeKind::ActivityDone { owner_key, .. } => {
                    self.key.strip_prefix("done:") == Some(owner_key.as_str())
                }
                _ => self.key == row.key,
            }
    }

    fn from_session(row: &super::chrome::sessions::SessionRow) -> Option<Self> {
        use super::chrome::sessions::SessionRowKind;
        let key = match &row.kind {
            SessionRowKind::Machine { .. } => MACHINE_TOGGLE_KEY.to_owned(),
            SessionRowKind::Workspace { workspace_id, .. } => {
                format!("session-workspace:{workspace_id}")
            }
            SessionRowKind::Tab { key, .. } => key.clone(),
            SessionRowKind::Agent(agent) => format!("session-pane:{}", agent.pane_id),
            SessionRowKind::Terminal { pane_id, .. } => format!("session-terminal:{pane_id}"),
            SessionRowKind::ExternalGroup { key, .. } => format!("session-source:{key}"),
            SessionRowKind::External { external_id, .. } => {
                format!("session-external:{external_id}")
            }
            SessionRowKind::Activity(activity, _) => return Some(Self::from_row(activity)),
            _ => return None,
        };
        Some(Self {
            endpoint: row.endpoint_id.clone(),
            key,
        })
    }

    fn matches_session(&self, row: &super::chrome::sessions::SessionRow) -> bool {
        use super::chrome::sessions::SessionRowKind;
        if self.endpoint != row.endpoint_id {
            return false;
        }
        match &row.kind {
            SessionRowKind::Machine { .. } => self.key == MACHINE_TOGGLE_KEY,
            SessionRowKind::Workspace { workspace_id, .. } => {
                self.key.strip_prefix("session-workspace:") == Some(workspace_id.as_str())
            }
            SessionRowKind::Tab { key, .. } => self.key == *key,
            SessionRowKind::Agent(agent) => {
                self.key.strip_prefix("session-pane:") == Some(agent.pane_id.as_str())
            }
            SessionRowKind::Terminal { pane_id, .. } => {
                self.key.strip_prefix("session-terminal:") == Some(pane_id.as_str())
            }
            SessionRowKind::ExternalGroup { key, .. } => {
                self.key.strip_prefix("session-source:") == Some(key.as_str())
            }
            SessionRowKind::External { external_id, .. } => {
                self.key.strip_prefix("session-external:") == Some(external_id.as_str())
            }
            SessionRowKind::Activity(activity, _) => self.matches(activity),
            _ => false,
        }
    }
}

/// 构建树时的折叠态只读视图（三个集合都在 `ClientShellState` 上）。
pub(super) struct CollapseState<'a> {
    pub(super) collapsed_groups: &'a HashSet<String>,
    pub(super) remote_collapsed_groups: &'a HashMap<ClientEndpointId, HashSet<String>>,
    pub(super) collapsed_endpoints: &'a HashSet<ClientEndpointId>,
}

impl CollapseState<'_> {
    fn group_key_present(&self, endpoint_id: &ClientEndpointId, key: &str) -> bool {
        let groups = if endpoint_id.is_local() {
            Some(self.collapsed_groups)
        } else {
            self.remote_collapsed_groups.get(endpoint_id)
        };
        groups.is_some_and(|groups| groups.contains(key))
    }
}

/// `<owner>` 键：`pane:<id>` / `ext:<id>`。
fn owner_key(owner: &AgentActivityOwner) -> String {
    match owner {
        AgentActivityOwner::Pane { pane_id } => format!("pane:{pane_id}"),
        AgentActivityOwner::External { external_id } => format!("ext:{external_id}"),
    }
}

fn parse_owner_key(key: &str) -> Option<AgentActivityOwner> {
    if let Some(pane_id) = key.strip_prefix("pane:") {
        return Some(AgentActivityOwner::Pane {
            pane_id: pane_id.to_owned(),
        });
    }
    key.strip_prefix("ext:")
        .map(|external_id| AgentActivityOwner::External {
            external_id: external_id.to_owned(),
        })
}

fn activity_list_key(owner_key: &str) -> String {
    format!("agent-activity:{owner_key}")
}

fn activity_node_key(owner_key: &str, node_id: &str) -> String {
    format!("agent-node:{owner_key}:{node_id}")
}

pub(super) fn external_group_key(source: &str) -> String {
    format!("agent-external:{source}")
}

fn owner_running(status: AgentStatus, activity: &ClientShellAgentActivity) -> bool {
    matches!(status, AgentStatus::Working | AgentStatus::Blocked) || activity.running > 0
}

/// 「已完成 N · 失败 M」行在 `hits.agent_activity_rows` 里的节点 id：与「还有
/// N 项」行（空 id）分开悬浮，点击同样只打开活动窗口、不预选节点。来源的节点
/// id 不会以 NUL 开头。
pub(super) const ACTIVITY_DONE_HIT_ID: &str = "\u{0}done";

/// 分组节点（workflow / phase）行的类型字形与进度，构建期算好。
#[derive(Debug)]
pub(super) struct ActivityGroupMark {
    glyph: &'static str,
    /// 摘要首段的 `<活跃>/<总数>`；来源没给或认不出为 `None`。
    progress: Option<String>,
}

/// 分组节点的类型字形：workflow `⧉`、phase `▤`；其余节点为 `None`。
fn activity_group_mark(node: &ClientShellActivityNode) -> Option<ActivityGroupMark> {
    let glyph = match node.agent_type.as_deref()? {
        crate::api::schema::ACTIVITY_GROUP_WORKFLOW => "⧉",
        crate::api::schema::ACTIVITY_GROUP_PHASE => "▤",
        _ => return None,
    };
    Some(ActivityGroupMark {
        glyph,
        progress: node.summary.as_deref().and_then(group_progress),
    })
}

/// 分组摘要首段（`<活跃>/<总数> running …`）里的 `<活跃>/<总数>`。
fn group_progress(summary: &str) -> Option<String> {
    let head = summary.split_whitespace().next()?;
    let (active, total) = head.split_once('/')?;
    (active.parse::<u32>().is_ok() && total.parse::<u32>().is_ok()).then(|| head.to_owned())
}

/// 活动子行末尾「已完成 N · 失败 M」的文案，为 0 的段不画；两段都为 0 时 `None`。
fn finished_label(done: u32, failed: u32) -> Option<String> {
    let texts = &crate::i18n::texts().agent_panel;
    let parts = [
        (done, texts.activity_done_fmt),
        (failed, texts.activity_failed_fmt),
    ]
    .into_iter()
    .filter(|(count, _)| *count > 0)
    .map(|(count, template)| crate::i18n::fill(template, &[("n", &count.to_string())]))
    .collect::<Vec<_>>();
    (!parts.is_empty()).then(|| parts.join(" · "))
}

struct TreeBuilder<'a> {
    rows: &'a mut Vec<AgentTreeRow>,
    collapse: &'a CollapseState<'a>,
}

impl TreeBuilder<'_> {
    /// 活动树的属主行（agent 或外部条目）及其展开的活动节点。`running` 是
    /// [`owner_running`]：不在运行的属主不画徽标、没有活动子行与折叠开关。
    fn owner_activity(
        &self,
        endpoint: CachedEndpointSnapshot<'_>,
        owner: &str,
        activity: &ClientShellAgentActivity,
        running: bool,
    ) -> OwnerActivity {
        let clamp = |count: usize| u32::try_from(count).unwrap_or(u32::MAX);
        let hidden = if !activity.truncated {
            0
        } else if activity.active > 0 {
            let shown = activity
                .nodes
                .iter()
                .filter(|node| node.status.is_active())
                .count();
            activity.active.saturating_sub(clamp(shown))
        } else {
            activity.total.saturating_sub(clamp(activity.nodes.len()))
        };
        let finished = finished_label(activity.done, activity.failed);
        let has_children =
            running && (!activity.nodes.is_empty() || hidden > 0 || finished.is_some());
        let key = activity_list_key(owner);
        let expanded = has_children && self.collapse.group_key_present(endpoint.endpoint_id, &key);
        OwnerActivity {
            key,
            has_children,
            expanded,
            hidden,
            finished,
        }
    }

    /// 前序展开活动节点：根 = 没有父节点或父节点不在列表里；子节点默认折叠
    /// （键在集合里才展开）。用显式栈而不递归，`visited` 兜住重复 id 造成的环。
    /// 节点之后依次是「还有 N 项」（`hidden > 0`）与「已完成 N · 失败 M」
    /// （`finished`）两行。
    fn push_activity(
        &mut self,
        depth: u8,
        endpoint: CachedEndpointSnapshot<'_>,
        owner: &str,
        nodes: &[ClientShellActivityNode],
        hidden: u32,
        finished: Option<String>,
    ) {
        let has_node = |id: &str| nodes.iter().any(|node| node.id == id);
        let is_root = |node: &ClientShellActivityNode| {
            node.parent_id
                .as_deref()
                .is_none_or(|parent| !has_node(parent))
        };
        let mut visited = vec![false; nodes.len()];
        let mut stack: Vec<(usize, u8)> = nodes
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, node)| is_root(node))
            .map(|(index, _)| (index, depth))
            .collect();
        while let Some((index, depth)) = stack.pop() {
            if visited[index] {
                continue;
            }
            visited[index] = true;
            let node = &nodes[index];
            let has_children = child_indices(nodes, node).any(|child| !visited[child]);
            let key = activity_node_key(owner, &node.id);
            let expanded =
                has_children && self.collapse.group_key_present(endpoint.endpoint_id, &key);
            self.rows.push(TreeEntry {
                depth,
                key,
                kind: AgentTreeNode::new(
                    endpoint,
                    AgentTreeKind::Activity {
                        owner_key: owner.to_owned(),
                        node_id: node.id.clone(),
                        label: if node.label.is_empty() {
                            node.id.clone()
                        } else {
                            node.label.clone()
                        },
                        kind: node.kind,
                        status: node.status,
                        group: activity_group_mark(node),
                    },
                ),
                last_child_mask: 0,
                has_children,
                collapsed: has_children && !expanded,
            });
            if expanded {
                let next_depth = depth.saturating_add(1);
                let children = child_indices(nodes, node)
                    .filter(|child| !visited[*child])
                    .collect::<Vec<_>>();
                stack.extend(children.into_iter().rev().map(|child| (child, next_depth)));
            }
        }
        if hidden > 0 {
            self.push_leaf(
                depth,
                endpoint,
                AgentTreeKind::ActivityMore {
                    owner_key: owner.to_owned(),
                    label: crate::i18n::fill(
                        crate::i18n::texts().agent_panel.activity_more_fmt,
                        &[("n", &hidden.to_string())],
                    ),
                },
            );
        }
        if let Some(label) = finished {
            self.push_leaf(
                depth,
                endpoint,
                AgentTreeKind::ActivityDone {
                    owner_key: owner.to_owned(),
                    label,
                },
            );
        }
    }

    /// 活动子行末尾不可展开、没有折叠键的一行（「还有 N 项」「已完成 N · 失败 M」）。
    fn push_leaf(&mut self, depth: u8, endpoint: CachedEndpointSnapshot<'_>, kind: AgentTreeKind) {
        self.rows.push(TreeEntry {
            depth,
            key: String::new(),
            kind: AgentTreeNode::new(endpoint, kind),
            last_child_mask: 0,
            has_children: false,
            collapsed: false,
        });
    }
}

struct OwnerActivity {
    key: String,
    has_children: bool,
    expanded: bool,
    hidden: u32,
    finished: Option<String>,
}

pub(super) struct SessionActivity {
    pub(super) key: String,
    pub(super) has_children: bool,
    pub(super) expanded: bool,
    pub(super) count: u32,
    pub(super) rows: Vec<AgentTreeRow>,
}

pub(super) fn session_activity(
    endpoint: CachedEndpointSnapshot<'_>,
    owner: &AgentActivityOwner,
    status: AgentStatus,
    activity: &ClientShellAgentActivity,
    _config: &ClientShellConfig,
    collapse: &CollapseState<'_>,
) -> SessionActivity {
    let mut rows = Vec::new();
    let mut builder = TreeBuilder {
        rows: &mut rows,
        collapse,
    };
    let owner = owner_key(owner);
    let running = owner_running(status, activity);
    let info = builder.owner_activity(endpoint, &owner, activity, running);
    if info.expanded {
        builder.push_activity(
            1,
            endpoint,
            &owner,
            &activity.nodes,
            info.hidden,
            info.finished,
        );
        fill_last_child_masks(&mut rows);
    }
    let finished = activity.done.saturating_add(activity.failed);
    SessionActivity {
        key: info.key,
        has_children: info.has_children,
        expanded: info.expanded,
        count: if !running {
            0
        } else if activity.running > 0 {
            activity.running
        } else if finished > 0 {
            finished
        } else {
            u32::try_from(activity.nodes.len()).unwrap_or(u32::MAX)
        },
        rows,
    }
}

pub(super) fn render_session_activity_row(
    buffer: &mut Buffer,
    rect: Rect,
    row: &AgentTreeRow,
    config: &ClientShellConfig,
    glyphs: StatusGlyphs,
    chrome_hover: Option<&ChromeHover>,
    hits: &mut ShellHitMap,
) {
    let cx = RowContext {
        config,
        glyphs,
        chrome_hover,
    };
    render_tree_row(buffer, rect, row, &cx, hits);
}

/// 外部条目的来源，按首次出现的顺序去重；来源种类极少，线性查重即可。
pub(super) fn external_sources(externals: &[ClientShellExternalAgent]) -> Vec<&str> {
    let mut sources: Vec<&str> = Vec::new();
    for external in externals {
        if !sources.contains(&external.source.as_str()) {
            sources.push(&external.source);
        }
    }
    sources
}

/// `parent` 的直接子节点下标（按列表顺序）；自指的节点不算自己的子节点。
fn child_indices<'n>(
    nodes: &'n [ClientShellActivityNode],
    parent: &'n ClientShellActivityNode,
) -> impl Iterator<Item = usize> + 'n {
    nodes
        .iter()
        .enumerate()
        .filter(move |(_, node)| {
            !std::ptr::eq(*node, parent) && node.parent_id.as_deref() == Some(parent.id.as_str())
        })
        .map(|(index, _)| index)
}

#[derive(Clone, Copy)]
struct RowContext<'a> {
    config: &'a ClientShellConfig,
    glyphs: StatusGlyphs,
    chrome_hover: Option<&'a ChromeHover>,
}

fn render_tree_row(
    buffer: &mut Buffer,
    rect: Rect,
    row: &AgentTreeRow,
    cx: &RowContext<'_>,
    hits: &mut ShellHitMap,
) {
    let node = &row.kind;
    let stale_cx;
    let cx = if node.stale {
        stale_cx = RowContext {
            glyphs: StatusGlyphs::fixed(cx.config.status_indicators),
            ..*cx
        };
        &stale_cx
    } else {
        cx
    };
    let palette = &cx.config.palette;
    let toggle_hovered = matches!(
        cx.chrome_hover,
        Some(ChromeHover::AgentTreeToggle(endpoint_id, key))
            if endpoint_id == &node.endpoint_id && key == &row.key
    );
    let row_hovered = match &node.kind {
        AgentTreeKind::Activity {
            owner_key, node_id, ..
        } => matches!(
            cx.chrome_hover,
            Some(ChromeHover::AgentActivityRow(endpoint_id, owner, id))
                if endpoint_id == &node.endpoint_id && owner == owner_key && id == node_id
        ),
        AgentTreeKind::ActivityMore { owner_key, .. } => matches!(
            cx.chrome_hover,
            Some(ChromeHover::AgentActivityRow(endpoint_id, owner, id))
                if endpoint_id == &node.endpoint_id && owner == owner_key && id.is_empty()
        ),
        AgentTreeKind::ActivityDone { owner_key, .. } => matches!(
            cx.chrome_hover,
            Some(ChromeHover::AgentActivityRow(endpoint_id, owner, id))
                if endpoint_id == &node.endpoint_id && owner == owner_key && id == ACTIVITY_DONE_HIT_ID
        ),
    };
    if row_hovered {
        buffer.set_style(rect, Style::default().bg(palette.hover_row_bg()));
    }
    let muted = super::render::readable_muted_fg(
        palette,
        buffer
            .cell((rect.x, rect.y))
            .map_or(palette.panel_bg, |cell| cell.bg),
    );
    let (used, toggle) = render_tree_prefix(
        buffer,
        rect.x,
        rect.y,
        rect.width,
        row.depth,
        row.last_child_mask,
        row.has_children,
        row.collapsed,
        false,
        Style::default().fg(palette.overlay0),
    );
    if row.has_children && !toggle.is_empty() {
        buffer.set_style(
            toggle,
            Style::default().fg(if toggle_hovered {
                palette.text
            } else {
                palette.accent
            }),
        );
        hits.agent_tree_toggles
            .push((toggle, node.endpoint_id.clone(), row.key.clone()));
    }
    let content = Rect::new(
        rect.x.saturating_add(used),
        rect.y,
        rect.width.saturating_sub(used),
        rect.height,
    );
    let (owner_key, node_id) = match &node.kind {
        AgentTreeKind::Activity {
            owner_key,
            node_id,
            label,
            kind,
            status,
            group,
        } => {
            render_activity_line(
                buffer,
                content,
                label,
                *kind,
                *status,
                group.as_ref(),
                cx,
                muted,
            );
            (owner_key, node_id.clone())
        }
        AgentTreeKind::ActivityMore { owner_key, label }
        | AgentTreeKind::ActivityDone { owner_key, label } => {
            put_label(
                buffer,
                content.x,
                content.y,
                content.width,
                label,
                Style::default()
                    .fg(palette.overlay0)
                    .add_modifier(Modifier::DIM),
            );
            let node_id = if matches!(node.kind, AgentTreeKind::ActivityDone { .. }) {
                ACTIVITY_DONE_HIT_ID.to_owned()
            } else {
                String::new()
            };
            (owner_key, node_id)
        }
    };
    hits.agent_activity_rows.push(AgentActivityHit {
        rect,
        endpoint_id: node.endpoint_id.clone(),
        owner_key: owner_key.clone(),
        node_id,
    });
    if node.stale {
        buffer.set_style(
            rect,
            Style::default()
                .fg(palette.overlay0)
                .add_modifier(Modifier::DIM),
        );
    }
}

/// 活动节点行：`状态图标 标签 [种类]`。标签优先于种类（次要信息）。分组节点
/// （workflow / phase）行首换成类型字形（颜色仍随状态），标签后灰显的是进度
/// `<活跃>/<总数>` 而不是种类。
// 一行的只读呈现参数保持集中，避免为传参构造临时拥有型对象。
#[allow(clippy::too_many_arguments)]
fn render_activity_line(
    buffer: &mut Buffer,
    content: Rect,
    label: &str,
    kind: AgentActivityKind,
    status: AgentActivityStatus,
    group: Option<&ActivityGroupMark>,
    cx: &RowContext<'_>,
    muted: ratatui::style::Color,
) {
    let palette = &cx.config.palette;
    let mapped = activity_status_as_agent(status);
    let icon = group.map_or_else(
        || cx.glyphs.status(mapped),
        |group| {
            if content.width < 12 {
                match group.glyph {
                    "⧉" => "W",
                    "▤" => "P",
                    other => other,
                }
            } else {
                group.glyph
            }
        },
    );
    let icon_color = status_color(mapped, palette, &cx.config.components);
    let texts = &crate::i18n::texts().agent_activity;
    let kind_label = match (group, kind) {
        (Some(group), _) => group.progress.as_deref().unwrap_or_default(),
        (None, AgentActivityKind::Subagent) => texts.kind_subagent,
        (None, AgentActivityKind::Task) => texts.kind_task,
        (None, AgentActivityKind::Todo) => texts.kind_todo,
        (None, AgentActivityKind::Background) => texts.kind_background,
        (None, AgentActivityKind::Unknown) => texts.kind_unknown,
    };
    let mut x = content.x;
    let mut remaining = content.width;
    let icon_width = display_width(icon) as u16;
    if remaining < icon_width + 1 {
        return;
    }
    put_text(
        buffer,
        x,
        content.y,
        icon_width,
        icon,
        Style::default().fg(icon_color),
    );
    x = x.saturating_add(icon_width + 1);
    remaining = remaining.saturating_sub(icon_width + 1);
    let label_width = put_label(
        buffer,
        x,
        content.y,
        remaining,
        label,
        Style::default().fg(palette.subtext0),
    );
    let kind_width = display_width(kind_label) as u16;
    if label_width + 1 + kind_width <= remaining {
        put_text(
            buffer,
            x.saturating_add(label_width + 1),
            content.y,
            kind_width,
            kind_label,
            Style::default().fg(muted),
        );
    }
}

/// 活动节点状态到面板状态图标 / 颜色的映射。
fn activity_status_as_agent(status: AgentActivityStatus) -> AgentStatus {
    match status {
        AgentActivityStatus::Running => AgentStatus::Working,
        AgentActivityStatus::Done => AgentStatus::Done,
        AgentActivityStatus::Blocked | AgentActivityStatus::Failed => AgentStatus::Blocked,
        AgentActivityStatus::Pending => AgentStatus::Idle,
        AgentActivityStatus::Unknown => AgentStatus::Unknown,
    }
}

fn put_text(buffer: &mut Buffer, x: u16, y: u16, width: u16, text: &str, style: Style) {
    // set_stringn 按显示宽度截断并给宽字符补占位格，逐字符写会弄坏 CJK。
    buffer.set_stringn(x, y, text, usize::from(width), style);
}

/// 同 [`put_text`]，放不下时截短并以「…」收尾（与 agent 名的 `truncate_end` 同
/// 口径；真机 L1：活动子行硬截成「执行 echo 命令并返」，看不出被截断）。宽字符
/// 放不下时停在它前面。直接写缓冲区、不分配，返回实际占用的列数。
fn put_label(buffer: &mut Buffer, x: u16, y: u16, width: u16, text: &str, style: Style) -> u16 {
    if width == 0 || !buffer.area.contains(ratatui::layout::Position::new(x, y)) {
        return 0;
    }
    if display_width(text) <= usize::from(width) {
        let (end, _) = buffer.set_stringn(x, y, text, usize::from(width), style);
        return end.saturating_sub(x);
    }
    let (end, _) = buffer.set_stringn(x, y, text, usize::from(width - 1), style);
    let (end, _) = buffer.set_stringn(end, y, "…", 1, style);
    end.saturating_sub(x)
}

impl ClientShellState {
    pub(super) fn agent_tree_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) -> bool {
        use super::chrome::sessions::SessionRowKind;
        use crossterm::event::KeyCode;
        if !self.workbench.enabled
            || self.workbench.arranging
            || self.workbench.dock.focused != super::dock::PanelId::Workspaces
            || self.mode != ClientShellMode::Terminal
            || self.overlay.is_some()
            || self.session_view.rail
            || self.session_view.body.is_empty()
            || !key.modifiers.is_empty()
            || !matches!(
                key.code,
                KeyCode::Up | KeyCode::Down | KeyCode::Enter | KeyCode::Left | KeyCode::Right
            )
        {
            return false;
        }
        self.refresh_session_rows();
        let Some(cache) = &self.session_rows else {
            return true;
        };
        let selectable = cache
            .rows
            .iter()
            .enumerate()
            .filter_map(|(index, row)| {
                (!matches!(
                    &row.kind,
                    SessionRowKind::MachineGroup { .. }
                        | SessionRowKind::Section { .. }
                        | SessionRowKind::MobileAction { .. }
                        | SessionRowKind::Empty(_)
                ))
                .then_some(index)
            })
            .collect::<Vec<_>>();
        let current = self
            .workbench
            .agent_keyboard_target
            .as_ref()
            .and_then(|target| {
                selectable
                    .iter()
                    .position(|index| target.matches_session(&cache.rows[*index]))
            });
        if selectable.is_empty()
            || (key.code == KeyCode::Enter
                && self.workbench.agent_keyboard_target.is_some()
                && current.is_none())
        {
            self.workbench.agent_keyboard_target = None;
            outcome.repaint = true;
            return true;
        }
        let selected = match (key.code, current) {
            (KeyCode::Down, Some(index)) => (index + 1).min(selectable.len() - 1),
            (KeyCode::Up, Some(index)) => index.saturating_sub(1),
            (_, Some(index)) => index,
            _ => 0,
        };
        let index = selectable[selected];
        let row = &cache.rows[index];
        let target = AgentKeyboardTarget::from_session(row);
        let scroll = super::scroll::list_scroll_start_to_reveal(
            &cache.heights,
            &cache.gaps,
            self.session_view.body.height,
            self.workspace_scroll,
            index,
        );
        let toggle = self.session_keyboard_toggle(row);
        let row_rect = self.session_keyboard_rect(row);
        let group = matches!(
            &row.kind,
            SessionRowKind::Machine { .. }
                | SessionRowKind::Workspace { .. }
                | SessionRowKind::ExternalGroup { .. }
        );
        self.workbench.agent_keyboard_target = target;
        self.workspace_scroll = scroll;
        if let Some((rect, _)) = toggle.filter(|(_, collapsed)| match key.code {
            KeyCode::Left => !collapsed,
            KeyCode::Right => *collapsed,
            KeyCode::Enter => group,
            _ => false,
        }) {
            let point = (rect.x, rect.y);
            if !self.handle_session_click(point, outcome) {
                self.handle_agent_tree_click(point, outcome);
            }
        } else if key.code == KeyCode::Enter {
            if let Some(rect) = row_rect {
                let point = (rect.right().saturating_sub(1), rect.y);
                if !self.handle_session_click(point, outcome)
                    && !self.handle_agent_tree_click(point, outcome)
                {
                    self.handle_endpoint_agent_click(point, outcome);
                }
            }
        }
        outcome.repaint = true;
        true
    }

    fn session_keyboard_toggle(
        &self,
        row: &super::chrome::sessions::SessionRow,
    ) -> Option<(Rect, bool)> {
        use super::chrome::sessions::{SessionHit, SessionRowKind};
        let rect_for = |key: &str| {
            self.hits.sessions.iter().find_map(|(rect, hit)| match hit {
                SessionHit::Toggle(endpoint, candidate)
                    if endpoint == &row.endpoint_id && candidate == key =>
                {
                    Some(*rect)
                }
                _ => None,
            })
        };
        match &row.kind {
            SessionRowKind::Machine { collapsed, .. } => self
                .hits
                .machines
                .iter()
                .find(|hit| hit.endpoint_id == row.endpoint_id)
                .map(|hit| (hit.collapse_toggle, *collapsed)),
            SessionRowKind::Workspace { toggle, .. } => toggle
                .as_ref()
                .and_then(|(key, collapsed)| rect_for(key).map(|rect| (rect, *collapsed))),
            SessionRowKind::Tab { key, collapsed, .. }
            | SessionRowKind::ExternalGroup { key, collapsed, .. } => {
                rect_for(key).map(|rect| (rect, *collapsed))
            }
            SessionRowKind::Agent(super::chrome::sessions::SessionAgent { activity, .. })
            | SessionRowKind::External { activity, .. } => activity
                .as_ref()
                .and_then(|(key, expanded, _)| rect_for(key).map(|rect| (rect, !expanded))),
            SessionRowKind::Activity(activity, _) if activity.has_children => self
                .hits
                .session_tree_toggles
                .iter()
                .find(|(_, endpoint, key)| endpoint == &row.endpoint_id && key == &activity.key)
                .map(|(rect, _, _)| (*rect, activity.collapsed)),
            _ => None,
        }
    }

    fn session_keyboard_rect(&self, row: &super::chrome::sessions::SessionRow) -> Option<Rect> {
        use super::chrome::sessions::{SessionHit, SessionRowKind};
        match &row.kind {
            SessionRowKind::Machine { .. } => self.hits.machines.iter().find(|hit| hit.endpoint_id == row.endpoint_id).map(|hit| hit.rect),
            SessionRowKind::Workspace { workspace_id, .. } => self.hits.workspaces.iter().find(|hit| hit.endpoint_id == row.endpoint_id && &hit.workspace_id == workspace_id).map(|hit| hit.rect),
            SessionRowKind::Tab { tab_id, .. } => self.hits.sessions.iter().find_map(|(rect, hit)| matches!(hit, SessionHit::Tab(endpoint, id) if endpoint == &row.endpoint_id && id == tab_id).then_some(*rect)),
            SessionRowKind::Agent(agent) => self.hits.session_agents.iter().find(|(_, endpoint, pane)| endpoint == &row.endpoint_id && pane == &agent.pane_id).map(|(rect, _, _)| *rect),
            SessionRowKind::Terminal { pane_id, .. } => self.hits.sessions.iter().find_map(|(rect, hit)| matches!(hit, SessionHit::Terminal(endpoint, pane) if endpoint == &row.endpoint_id && pane == pane_id).then_some(*rect)),
            SessionRowKind::ExternalGroup { key, .. } => self.hits.sessions.iter().find_map(|(rect, hit)| matches!(hit, SessionHit::Toggle(endpoint, candidate) if endpoint == &row.endpoint_id && candidate == key).then_some(*rect)),
            SessionRowKind::External { external_id, .. } => self.hits.session_external_agents.iter().find(|(_, endpoint, id)| endpoint == &row.endpoint_id && id == external_id).map(|(rect, _, _)| *rect),
            SessionRowKind::Activity(activity, _) => {
                let (owner, node) = match &activity.kind.kind {
                    AgentTreeKind::Activity { owner_key, node_id, .. } => (owner_key.as_str(), node_id.as_str()),
                    AgentTreeKind::ActivityMore { owner_key, .. } => (owner_key.as_str(), ""),
                    AgentTreeKind::ActivityDone { owner_key, .. } => (owner_key.as_str(), ACTIVITY_DONE_HIT_ID),
                };
                self.hits.session_activity_rows.iter().find(|hit| hit.endpoint_id == row.endpoint_id && hit.owner_key == owner && hit.node_id == node).map(|hit| hit.rect)
            }
            _ => None,
        }
    }

    pub(super) fn paint_agent_keyboard_focus(&self, buffer: &mut Buffer) {
        if !self.workbench.enabled
            || self.workbench.arranging
            || self.workbench.dock.focused != super::dock::PanelId::Workspaces
            || self.mode != ClientShellMode::Terminal
            || self.session_view.rail
        {
            return;
        }
        let Some(target) = &self.workbench.agent_keyboard_target else {
            return;
        };
        let Some(cache) = &self.session_rows else {
            return;
        };
        if let Some(rect) = cache
            .rows
            .iter()
            .find(|row| target.matches_session(row))
            .and_then(|row| self.session_keyboard_rect(row))
        {
            buffer.set_style(rect, Style::default().add_modifier(Modifier::UNDERLINED));
        }
    }

    /// 左键落在统一树的非 agent 行上：分组头 / 折叠开关切换折叠态，活动节点行与
    /// 外部条目行打开「Agent 活动」窗口。开关矩形落在行矩形之内，调用方必须先于
    /// agent 行点击调用本函数。
    pub(super) fn handle_agent_tree_click(
        &mut self,
        point: (u16, u16),
        outcome: &mut ClientShellInput,
    ) -> bool {
        if let Some((endpoint_id, key)) = self
            .hits
            .agent_tree_toggles
            .iter()
            .chain(self.hits.session_tree_toggles.iter())
            .find(|(rect, _, _)| super::contains(*rect, point))
            .map(|(_, endpoint_id, key)| (endpoint_id.clone(), key.clone()))
        {
            if key == MACHINE_TOGGLE_KEY {
                self.toggle_collapsed_endpoint(&endpoint_id);
            } else {
                self.toggle_collapsed_group(&endpoint_id, key);
            }
            outcome.repaint = true;
            self.persist_chrome_preferences(outcome);
            return true;
        }
        if let Some((endpoint_id, owner_key, node_id)) = self
            .hits
            .agent_activity_rows
            .iter()
            .chain(self.hits.session_activity_rows.iter())
            .find(|hit| super::contains(hit.rect, point))
            .map(|hit| {
                (
                    hit.endpoint_id.clone(),
                    hit.owner_key.clone(),
                    hit.node_id.clone(),
                )
            })
        {
            if let Some(owner) = parse_owner_key(&owner_key) {
                self.open_agent_activity(endpoint_id, owner, outcome);
                // 「还有 N 项」（空 id）与「已完成 N · 失败 M」只打开窗口，不预选节点。
                if !node_id.is_empty() && node_id != ACTIVITY_DONE_HIT_ID {
                    self.select_agent_activity_node(node_id, outcome);
                }
                outcome.repaint = true;
            }
            return true;
        }
        if let Some((endpoint_id, external_id)) = self
            .hits
            .external_agents
            .iter()
            .chain(self.hits.session_external_agents.iter())
            .find(|(rect, _, _)| super::contains(*rect, point))
            .map(|(_, endpoint_id, external_id)| (endpoint_id.clone(), external_id.clone()))
        {
            self.open_agent_activity(
                endpoint_id,
                AgentActivityOwner::External { external_id },
                outcome,
            );
            outcome.repaint = true;
            return true;
        }
        false
    }

    /// 机器层节点的折叠：与工作区区的机器行共用 `collapsed_endpoints`。
    fn toggle_collapsed_endpoint(&mut self, endpoint_id: &ClientEndpointId) {
        if !self.collapsed_endpoints.remove(endpoint_id) {
            self.collapsed_endpoints.insert(endpoint_id.clone());
        }
        self.bump_tree_collapse_epoch();
    }

    /// 聚焦 agent pane，并将本机回选交给 runtime 取消尚未完成的远端切换。
    pub(super) fn focus_agent_pane(
        &mut self,
        endpoint_id: ClientEndpointId,
        pane_id: String,
        outcome: &mut ClientShellInput,
    ) {
        self.focus_or_activate(
            endpoint_id,
            ClientEndpointFocusTarget::Pane(pane_id),
            outcome,
        );
    }

    /// 右键落在 agent 行（本机面板、联邦面板）或外部条目上：打开对应菜单。
    pub(super) fn open_agent_context_menu_at(&mut self, point: (u16, u16)) -> bool {
        let agent = self
            .hits
            .agents
            .iter()
            .find(|(rect, _)| super::contains(*rect, point))
            .map(|(_, pane_id)| (self.active_endpoint_id.clone(), pane_id.clone()))
            .or_else(|| {
                self.hits
                    .endpoint_agents
                    .iter()
                    .chain(self.hits.session_agents.iter())
                    .find(|(rect, _, _)| super::contains(*rect, point))
                    .map(|(_, endpoint_id, pane_id)| (endpoint_id.clone(), pane_id.clone()))
            });
        if let Some((endpoint_id, pane_id)) = agent {
            return self.open_agent_context_menu(endpoint_id, pane_id, point.0, point.1);
        }
        let external = self
            .hits
            .external_agents
            .iter()
            .chain(self.hits.session_external_agents.iter())
            .find(|(rect, _, _)| super::contains(*rect, point))
            .map(|(_, endpoint_id, external_id)| (endpoint_id.clone(), external_id.clone()));
        if let Some((endpoint_id, external_id)) = external {
            self.open_external_agent_context_menu(endpoint_id, external_id, point.0, point.1);
            return true;
        }
        // 工作区分组头（整行都是折叠开关）右键打开与工作区列表同一份工作区菜单
        // （冒烟 L8）。与工作区列表同口径，只对当前端点的工作区。
        let workspace = self
            .hits
            .agent_tree_toggles
            .iter()
            .chain(self.hits.session_tree_toggles.iter())
            .find(|(rect, endpoint_id, _)| {
                endpoint_id == &self.active_endpoint_id && super::contains(*rect, point)
            })
            .and_then(|(_, _, key)| super::agent_sidebar::group_key_workspace(key))
            .map(str::to_owned)
            .or_else(|| {
                self.hits
                    .workspaces
                    .iter()
                    .find(|hit| {
                        hit.endpoint_id == self.active_endpoint_id
                            && super::contains(hit.rect, point)
                    })
                    .map(|hit| hit.workspace_id.clone())
            });
        if let Some(workspace_id) = workspace {
            self.open_workspace_context_menu(workspace_id, point.0, point.1);
            return matches!(self.overlay, Some(ClientShellOverlay::ContextMenu(_)));
        }
        false
    }

    fn open_agent_context_menu(
        &mut self,
        endpoint_id: ClientEndpointId,
        pane_id: String,
        x: u16,
        y: u16,
    ) -> bool {
        let Some(agent) = self.endpoint_agent(&endpoint_id, &pane_id) else {
            return false;
        };
        let has_activity = agent.activity.total > 0 || !agent.activity.nodes.is_empty();
        let renamable = self.agent_pane_renamable(&endpoint_id, &pane_id);
        let target = ClientContextMenuTarget::Agent {
            endpoint_id,
            pane_id,
            has_activity,
            renamable,
        };
        self.overlay = Some(ClientShellOverlay::ContextMenu(ClientContextMenuOverlay {
            target,
            x,
            y,
            highlighted: 0,
            hovered: None,
            submenu: None,
        }));
        true
    }

    pub(super) fn open_external_agent_context_menu(
        &mut self,
        endpoint_id: ClientEndpointId,
        external_id: String,
        x: u16,
        y: u16,
    ) {
        self.overlay = Some(ClientShellOverlay::ContextMenu(ClientContextMenuOverlay {
            target: ClientContextMenuTarget::ExternalAgent {
                endpoint_id,
                external_id,
            },
            x,
            y,
            highlighted: 0,
            hovered: None,
            submenu: None,
        }));
    }

    /// 某端点快照里 `pane_id` 的 agent 条目。
    fn endpoint_agent(
        &self,
        endpoint_id: &ClientEndpointId,
        pane_id: &str,
    ) -> Option<&ClientShellAgent> {
        self.endpoints
            .iter()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
            .and_then(|endpoint| endpoint.snapshot.as_deref())
            .and_then(|snapshot| {
                snapshot
                    .agents
                    .iter()
                    .find(|agent| agent.pane_id == pane_id)
            })
    }

    /// agent 行 / 外部条目右键菜单的动作。条目由接缝在 `context_menu.rs` 定稿
    /// （是否可点也在那里决定）；这里只接动作。
    pub(super) fn activate_agent_context_action(
        &mut self,
        endpoint_id: ClientEndpointId,
        owner: AgentActivityOwner,
        action: ClientContextMenuAction,
        outcome: &mut ClientShellInput,
    ) {
        use ClientContextMenuAction as Action;
        match (action, owner) {
            (Action::FocusAgent, AgentActivityOwner::Pane { pane_id }) => {
                self.focus_agent_pane(endpoint_id, pane_id, outcome);
            }
            (Action::ViewAgentActivity, owner) => {
                self.open_agent_activity(endpoint_id, owner, outcome);
            }
            (Action::RenameAgent, AgentActivityOwner::Pane { pane_id }) => {
                self.rename_agent_pane(endpoint_id, pane_id, outcome);
            }
            (Action::CloseAgentPane, AgentActivityOwner::Pane { pane_id }) => {
                self.close_agent_pane(endpoint_id, pane_id, outcome);
            }
            _ => {}
        }
    }

    /// 这个 agent 的窗格能否从这里重命名：当前端点照常；其它端点要在线且宣告了
    /// `pane.rename`，提交才能经 `push_endpoint_method_for` 直接发过去。菜单据此
    /// 置灰「重命名窗格」（文档终审 D9）。
    fn agent_pane_renamable(&self, endpoint_id: &ClientEndpointId, pane_id: &str) -> bool {
        *endpoint_id == self.active_endpoint_id
            || (self.endpoint_is_online(endpoint_id)
                && self.supports_endpoint_method_for(
                    endpoint_id,
                    &crate::api::schema::Method::PaneRename(crate::api::schema::PaneRenameParams {
                        pane_id: pane_id.to_owned(),
                        label: None,
                    }),
                ))
    }

    /// 「重命名窗格」：沿用 pane 重命名浮层（标题同键盘重命名路径
    /// `open_rename_pane_overlay`，即 `dialogs.rename_pane`）与 `pane.rename`，
    /// 改的是 pane 标签，菜单文案照实写「重命名窗格」。
    /// 当前端点的 pane 走 `ClientRenameTarget::Pane`；其它端点的 pane 按现有 API
    /// 能力直接重命名（文档终审 D9）：浮层目标带上端点，提交发往该端点、不切换
    /// 当前端点，与「关闭窗格」同口径——以前这里只会切过去并聚焦。该端点此刻
    /// 不可达（菜单里已置灰，这里兜底）时照「关闭窗格」的做法提示未就绪。
    fn rename_agent_pane(
        &mut self,
        endpoint_id: ClientEndpointId,
        pane_id: String,
        outcome: &mut ClientShellInput,
    ) {
        if !self.agent_pane_renamable(&endpoint_id, &pane_id) {
            let label = self.endpoint_label(&endpoint_id).to_owned();
            self.receive_endpoint_unavailable(crate::i18n::fill(
                crate::i18n::texts().mobile.not_ready_fmt,
                &[("label", &label)],
            ));
            outcome.repaint = true;
            return;
        }
        let snapshot = if endpoint_id == self.active_endpoint_id {
            self.snapshot.as_deref()
        } else {
            self.endpoints
                .iter()
                .find(|endpoint| endpoint.endpoint_id == endpoint_id)
                .and_then(|endpoint| endpoint.snapshot.as_deref())
        };
        let label = snapshot.and_then(|snapshot| {
            snapshot
                .panes
                .iter()
                .find(|pane| pane.pane_id == pane_id)
                .and_then(|pane| pane.label.clone())
        });
        let target = if endpoint_id == self.active_endpoint_id {
            ClientRenameTarget::Pane { pane_id }
        } else {
            ClientRenameTarget::EndpointPane {
                endpoint_id,
                pane_id,
            }
        };
        self.overlay = Some(ClientShellOverlay::Rename(ClientRenameOverlay {
            title: crate::i18n::texts().dialogs.rename_pane,
            input: TextEditor::new(label.as_deref().unwrap_or_default(), label.is_none()),
            target,
        }));
        outcome.repaint = true;
    }

    /// 「关闭窗格」：发 `pane.close`，与 pane 右键菜单的「关闭窗格」同一请求；
    /// 其它在线端点直接发往该端点（不切换当前端点），应答在该端点不是当前端点时
    /// 也放行、失败照常提示（T1 审查轻 5）；不在线则提示。
    fn close_agent_pane(
        &mut self,
        endpoint_id: ClientEndpointId,
        pane_id: String,
        outcome: &mut ClientShellInput,
    ) {
        let method =
            crate::api::schema::Method::PaneClose(crate::api::schema::PaneTarget { pane_id });
        if endpoint_id == self.active_endpoint_id {
            self.push_endpoint_method(method, outcome);
        } else if !self.push_endpoint_method_for(
            &endpoint_id,
            method,
            PendingEndpointKind::CrossEndpointAction {
                endpoint_id: endpoint_id.clone(),
            },
            outcome,
        ) {
            let label = self.endpoint_label(&endpoint_id).to_owned();
            self.receive_endpoint_unavailable(crate::i18n::fill(
                crate::i18n::texts().mobile.not_ready_fmt,
                &[("label", &label)],
            ));
            outcome.repaint = true;
        }
    }
}
