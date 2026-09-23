//! Agents 面板统一树的领域层：机器 → 工作区 → 标签页 → agent → 活动节点，外加
//! 「外部」分组。原语（`TreeEntry` 与前缀字形）在 `crate::ui::kit::tree`，这里
//! 只放认识客户端类型的部分。
//!
//! 视图计算阶段由 [`build_agent_tree`] 产出展平后的行序列（进
//! `endpoint_agents::AgentRowsCache`，键含折叠代际），渲染阶段
//! [`render_agent_tree_rows`] 只读；classic / workbench / 联邦三条路径共用同一
//! 构建与同一行渲染。
//!
//! 层级：`Spaces` 下机器（仅多端点）→ 工作区 → 标签页（仅该工作区有 >1 个标签
//! 页时）→ agent → 活动摘要；`Launch` 或状态过滤视图下 agent 平铺，但 agent 行
//! 仍可展开活动摘要。外部来源（不属于任何 pane 的条目）按 source 单列「外部」
//! 分组（状态过滤视图不列），点击其行打开「Agent 活动」窗口。
//!
//! 平铺视图：列表区不足 3 行的退化视图与联邦折叠侧栏不画分组头，改用构建期
//! 一并产出的平铺行（[`flat_view_rows`]）——忽略面板内的折叠态、保留完整
//! token、按聚合顺序列出全部 agent，被折叠分组里的 agent 仍可见可点。
//!
//! 活动摘要：快照默认只带每个 agent 的 running / total 计数与至多 1 个最新节点
//! （`truncated` 表示还有更多，整树经 `agent.activity.read` 取，归「Agent 活动」
//! 窗口）。agent 行右侧画活动徽标；展开后是最新节点一行，被截断时再跟一行
//! 「还有 N 项」（点击打开活动窗口）。server 若下发多个节点，同一套构建按
//! `parent_id` 前序展开，深度不设上限。
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
    text::Line,
    widgets::{Paragraph, Widget},
};

use super::agent_activity_overlay::AgentActivityOwner;
use super::agent_sidebar::{agent_group_key, agent_row, render_agent_list, AgentRow};
use super::aggregate_navigation::{
    aggregate_agent_rows, cached_endpoint_snapshots, CachedEndpointSnapshot,
};
use super::feedback::ChromeHover;
use super::state::AgentActivityHit;
use super::*;
use crate::api::schema::{AgentActivityKind, AgentActivityStatus, AgentStatus};
use crate::protocol::{
    ClientShellActivityNode, ClientShellAgent, ClientShellAgentActivity, ClientShellExternalAgent,
};
use crate::ui::display_width;
use crate::ui::kit::tree::{
    fill_last_child_masks, render_tree_prefix, tree_prefix_width, TreeEntry,
};

/// 机器层节点在 `hits.agent_tree_toggles` 里的折叠键：机器层不进
/// `collapsed_groups`，点击时改写 `collapsed_endpoints`。
pub(super) const MACHINE_TOGGLE_KEY: &str = "agent-machine";

/// 一行的节点种类与该种类的负载。
#[derive(Debug)]
pub(super) enum AgentTreeKind {
    /// 端点（仅多端点时出现）。
    Machine {
        label: String,
        status: ClientEndpointStatus,
    },
    Workspace {
        label: String,
        status: AgentStatus,
    },
    Tab {
        label: String,
        status: AgentStatus,
    },
    /// 某个 pane 里的 agent；`machine_initial` 供折叠侧栏的单列视图；
    /// `status_line` 是状态文案画在第几行（行配置已带状态文案时为 `None`）。
    Agent {
        agent: AgentRow,
        machine_initial: char,
        status_line: Option<usize>,
    },
    /// agent / 外部条目下的一个活动节点。
    Activity {
        owner_key: String,
        node_id: String,
        label: String,
        kind: AgentActivityKind,
        status: AgentActivityStatus,
    },
    /// 快照里的节点被上限截断：列表末尾的「还有 N 项」行（文本构建期算好），
    /// 点击打开活动窗口看全量。
    ActivityMore {
        owner_key: String,
        label: String,
    },
    /// 「外部」来源分组头（按 source 一组），`label` 是「外部 · source」。
    ExternalGroup {
        label: String,
    },
    /// 不属于任何 pane 的外部来源条目。
    ExternalAgent {
        external_id: String,
        label: String,
        agent: Option<String>,
        status: AgentStatus,
        readable: bool,
    },
}

/// 统一树一行的负载：端点身份、是否离线、分组计数、活动徽标与节点种类。
/// 行上要画的文本（计数、徽标）在构建期算好，渲染循环里不再分配。
#[derive(Debug)]
pub(super) struct AgentTreeNode {
    pub(super) endpoint_id: ClientEndpointId,
    /// 端点不在线：整行变暗。
    pub(super) stale: bool,
    /// 分组行右侧的计数文本（`· N`：工作区 / 标签页 / 机器 = agent 数，外部组 =
    /// 条目数）；非分组行为空。
    count_label: String,
    /// 有运行中的活动节点：徽标用工作色。
    running: bool,
    /// 活动徽标（见 [`ActivityBadge`]），没有活动为 `None`。
    badge: Option<ActivityBadge>,
    /// 平铺视图的行，只挂在整棵树的第 0 行上（见 [`flat_view_rows`]）；其余行与
    /// 平铺视图自己的行都是 `None`。
    flat_view: Option<Box<[AgentTreeRow]>>,
    pub(super) kind: AgentTreeKind,
}

impl AgentTreeNode {
    fn new(
        endpoint: CachedEndpointSnapshot<'_>,
        count: usize,
        activity: Option<&ClientShellAgentActivity>,
        kind: AgentTreeKind,
    ) -> Self {
        let (running, total) =
            activity.map_or((0, 0), |activity| (activity.running, activity.total));
        Self {
            endpoint_id: endpoint.endpoint_id.clone(),
            stale: endpoint.stale(),
            count_label: if count > 0 {
                format!("· {count}")
            } else {
                String::new()
            },
            running: running > 0,
            badge: activity_badge(running, total),
            flat_view: None,
            kind,
        }
    }

    /// agent 行的行数据；其它种类为 `None`。
    pub(super) fn agent(&self) -> Option<&AgentRow> {
        match &self.kind {
            AgentTreeKind::Agent { agent, .. } => Some(agent),
            _ => None,
        }
    }

    /// 分组行右侧的计数文本（`· N`）。
    pub(super) fn count_label(&self) -> &str {
        &self.count_label
    }
}

/// 统一树展平后的一行：`TreeEntry` 的深度 / 折叠键 / 末子掩码 + 客户端负载。
pub(super) type AgentTreeRow = TreeEntry<AgentTreeNode>;

/// 退化视图（列表区不足 3 行）与联邦折叠侧栏用的平铺行。这两种视图不画分组头，
/// 面板内折叠了的分组在这里展不开，所以平铺行忽略面板内的折叠态、保留完整 token
/// （工作区名等不再由分组头承载），按 `aggregate_agent_rows` 的顺序列出全部
/// agent，再跟外部条目（状态过滤视图不列）；机器层折叠（`collapsed_endpoints`，
/// 工作区区的机器行照样能切换）在树里生效时这里同样生效。
///
/// 平铺行在视图计算阶段与树一起构建，挂在树的第 0 行上随行切片传给渲染：
/// `ShellRenderState` 只带一个行切片（共享热文件，不为它加字段）。有 agent 或外部
/// 条目时树至少有一个分组头或 agent 行，所以树为空时平铺视图也为空。
pub(super) fn flat_view_rows(rows: &[AgentTreeRow]) -> &[AgentTreeRow] {
    rows.first()
        .and_then(|row| row.kind.flat_view.as_deref())
        .unwrap_or(&[])
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

    fn endpoint_collapsed(&self, endpoint_id: &ClientEndpointId) -> bool {
        self.collapsed_endpoints.contains(endpoint_id)
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

fn tab_key(tab_id: &str) -> String {
    format!("agent-tab:{tab_id}")
}

fn external_group_key(source: &str) -> String {
    format!("agent-external:{source}")
}

/// 状态图标（字形 + 间隔共 2 列）与名称至少 6 列：行首这 8 列先于徽标分配，
/// 徽标只拿剩下的宽度，窄侧栏里深层 agent 行不会只剩一截徽标。
const PRIMARY_MIN_WIDTH: u16 = 8;

/// agent / 外部条目行右侧的活动徽标：构建期算好完整文案与只留数字的紧凑形态
/// （`2/5`；没有运行中时是总数 `5`）及各自宽度，渲染按可用宽度三档退化——
/// 完整文案 → 只留数字 → 不画，循环里不再分配。
#[derive(Debug)]
struct ActivityBadge {
    full: String,
    full_width: u16,
    compact: String,
    compact_width: u16,
}

impl ActivityBadge {
    /// 首行宽 `width` 列时要画的档位（文本, 宽度）：先扣 [`PRIMARY_MIN_WIDTH`]
    /// 与徽标前的 1 列间隔，剩下的放得下哪档画哪档；都放不下为 `None`。
    fn fit(&self, width: u16) -> Option<(&str, u16)> {
        self.fit_room(width.saturating_sub(PRIMARY_MIN_WIDTH + 1))
    }

    /// 徽标自己能占 `room` 列时要画的档位：完整文案 → 只留数字 → `None`。
    fn fit_room(&self, room: u16) -> Option<(&str, u16)> {
        if self.full_width <= room {
            Some((self.full.as_str(), self.full_width))
        } else if self.compact_width <= room {
            Some((self.compact.as_str(), self.compact_width))
        } else {
            None
        }
    }
}

/// 活动徽标：完整文案见 [`badge_text`]，紧凑形态只留数字；没有活动为 `None`。
fn activity_badge(running: u32, total: u32) -> Option<ActivityBadge> {
    let full = badge_text(running, total)?;
    let total = total.max(running);
    let compact = if running > 0 {
        format!("{running}/{total}")
    } else {
        total.to_string()
    };
    Some(ActivityBadge {
        full_width: text_width(&full),
        full,
        compact_width: text_width(&compact),
        compact,
    })
}

fn text_width(text: &str) -> u16 {
    u16::try_from(display_width(text)).unwrap_or(u16::MAX)
}

/// 活动徽标文案，三种情况：有运行中的节点写「运行中 / 总数」形态（`2/5
/// running`，运行中等于总数时同样如此）；没有运行中的只写总数（`5 activities`，
/// 总数为 1 用单数键）；没有活动为 `None`。运行中与否另由徽标颜色区分。
fn badge_text(running: u32, total: u32) -> Option<String> {
    let texts = &crate::i18n::texts().agent_panel;
    let total = total.max(running);
    if total == 0 {
        return None;
    }
    Some(if running > 0 {
        crate::i18n::fill(
            texts.activity_badge_running_fmt,
            &[
                ("running", &running.to_string()),
                ("total", &total.to_string()),
            ],
        )
    } else if total == 1 {
        texts.activity_badge_one.to_owned()
    } else {
        crate::i18n::fill(texts.activity_badge_total_fmt, &[("n", &total.to_string())])
    })
}

/// mobile 切换器 agent 行的活动徽标，与桌面树同一口径、同样三档退化：`room`
/// 是详情行排完其余字段与徽标前间隔后剩下的列数，放得下完整文案给完整文案，
/// 否则只留数字（`2/5`；没有运行中时是总数 `5`），再放不下为 `None`；没有活动
/// 也为 `None`。`mobile.rs` 只调这一处，徽标逻辑留在本文件。
pub(super) fn mobile_activity_badge(agent: &ClientShellAgent, room: u16) -> Option<String> {
    let badge = activity_badge(agent.activity.running, agent.activity.total)?;
    badge.fit_room(room).map(|(text, _)| text.to_owned())
}

/// 视图计算阶段：按当前排序、折叠态与各端点快照产出展平的行序列，末子掩码已
/// 填好。渲染阶段只读它。
pub(super) fn build_agent_tree(
    endpoints: &[ClientShellEndpoint],
    active_endpoint_id: &ClientEndpointId,
    config: &ClientShellConfig,
    collapse: &CollapseState<'_>,
) -> Vec<AgentTreeRow> {
    let ordered = aggregate_agent_rows(endpoints, active_endpoint_id, config.agent_panel_sort);
    let view_label = endpoints
        .iter()
        .find(|endpoint| &endpoint.endpoint_id == active_endpoint_id)
        .and_then(|endpoint| endpoint.snapshot.as_deref())
        .and_then(|snapshot| snapshot.agent_view_label.as_deref());
    // 状态过滤视图的分组维度就是过滤本身，与「按启动顺序」一样平铺。
    let flat = config.agent_panel_sort == crate::config::AgentPanelSortConfig::Launch
        || view_label.is_some();
    let federated = endpoints.len() > 1;
    let mut rows = Vec::new();
    let mut builder = TreeBuilder {
        rows: &mut rows,
        config,
        collapse,
        active_endpoint_id,
        owners_only: false,
    };

    if flat {
        for row in &ordered {
            builder.push_agent(
                0,
                row.endpoint,
                row.agent,
                federated.then_some(row.endpoint.label),
                TokenDrop::default(),
            );
        }
        // 状态过滤只作用于 pane 里的 agent，外部条目不参与过滤，也就不列出。
        if view_label.is_none() {
            for endpoint in cached_endpoint_snapshots(endpoints) {
                builder.push_external_groups(0, endpoint);
            }
        }
    } else {
        for endpoint in cached_endpoint_snapshots(endpoints) {
            let snapshot = endpoint.snapshot;
            let mut agents = ordered
                .iter()
                .filter(|row| row.endpoint.endpoint_index == endpoint.endpoint_index)
                .map(|row| row.agent)
                .peekable();
            if agents.peek().is_none() && snapshot.external_agents.is_empty() {
                continue;
            }
            let agents = agents.collect::<Vec<_>>();
            let mut depth = 0;
            if federated {
                let collapsed = collapse.endpoint_collapsed(endpoint.endpoint_id);
                builder.push_group(
                    0,
                    MACHINE_TOGGLE_KEY.to_owned(),
                    endpoint,
                    agents.len() + snapshot.external_agents.len(),
                    collapsed,
                    AgentTreeKind::Machine {
                        label: endpoint.label.to_owned(),
                        status: endpoint.status,
                    },
                );
                if collapsed {
                    continue;
                }
                depth = 1;
            }
            for workspace in &snapshot.workspaces {
                let in_workspace =
                    |agent: &&ClientShellAgent| agent.workspace_id == workspace.workspace_id;
                let (count, status) = summarize(agents.iter().copied().filter(in_workspace));
                if count == 0 {
                    continue;
                }
                let key = agent_group_key(&workspace.workspace_id);
                let collapsed = collapse.group_key_present(endpoint.endpoint_id, &key);
                builder.push_group(
                    depth,
                    key,
                    endpoint,
                    count,
                    collapsed,
                    AgentTreeKind::Workspace {
                        label: workspace.label.clone(),
                        status,
                    },
                );
                if collapsed {
                    continue;
                }
                let tab_level = snapshot
                    .tabs
                    .iter()
                    .filter(|tab| tab.workspace_id == workspace.workspace_id)
                    .count()
                    > 1;
                if !tab_level {
                    for agent in agents.iter().copied().filter(in_workspace) {
                        builder.push_agent(
                            depth + 1,
                            endpoint,
                            agent,
                            None,
                            TokenDrop {
                                workspace: true,
                                tab: false,
                            },
                        );
                    }
                    continue;
                }
                for tab in snapshot
                    .tabs
                    .iter()
                    .filter(|tab| tab.workspace_id == workspace.workspace_id)
                {
                    let in_tab = |agent: &&ClientShellAgent| {
                        agent.workspace_id == workspace.workspace_id && agent.tab_id == tab.tab_id
                    };
                    let (count, status) = summarize(agents.iter().copied().filter(in_tab));
                    if count == 0 {
                        continue;
                    }
                    let key = tab_key(&tab.tab_id);
                    let collapsed = collapse.group_key_present(endpoint.endpoint_id, &key);
                    builder.push_group(
                        depth + 1,
                        key,
                        endpoint,
                        count,
                        collapsed,
                        AgentTreeKind::Tab {
                            label: tab.label.clone(),
                            status,
                        },
                    );
                    if collapsed {
                        continue;
                    }
                    for agent in agents.iter().copied().filter(in_tab) {
                        builder.push_agent(
                            depth + 2,
                            endpoint,
                            agent,
                            None,
                            TokenDrop {
                                workspace: true,
                                tab: true,
                            },
                        );
                    }
                }
            }
            builder.push_external_groups(depth, endpoint);
        }
    }
    fill_last_child_masks(&mut rows);

    // 平铺视图（见 `flat_view_rows`）：机器层折叠只在树按机器分组时生效，与树
    // 一致（`Launch` / 状态过滤的平铺树本来就不看机器折叠）。
    let honor_machine_collapse = federated && !flat;
    let visible = |endpoint_id: &ClientEndpointId| {
        !(honor_machine_collapse && collapse.endpoint_collapsed(endpoint_id))
    };
    let mut flat_rows = Vec::new();
    let mut flat_builder = TreeBuilder {
        rows: &mut flat_rows,
        config,
        collapse,
        active_endpoint_id,
        owners_only: true,
    };
    for row in ordered
        .iter()
        .filter(|row| visible(row.endpoint.endpoint_id))
    {
        flat_builder.push_agent(
            0,
            row.endpoint,
            row.agent,
            federated.then_some(row.endpoint.label),
            TokenDrop::default(),
        );
    }
    if view_label.is_none() {
        for endpoint in
            cached_endpoint_snapshots(endpoints).filter(|endpoint| visible(endpoint.endpoint_id))
        {
            flat_builder.push_external_entries(0, endpoint);
        }
    }
    if let Some(first) = rows.first_mut() {
        if !flat_rows.is_empty() {
            first.kind.flat_view = Some(flat_rows.into_boxed_slice());
        }
    }
    rows
}

/// 分组头的汇总：agent 数与最高优先级状态。
fn summarize<'a>(agents: impl Iterator<Item = &'a ClientShellAgent>) -> (usize, AgentStatus) {
    agents.fold((0, AgentStatus::Idle), |(count, status), agent| {
        let next = if status_priority(agent.agent_status) > status_priority(status) {
            agent.agent_status
        } else {
            status
        };
        (count + 1, next)
    })
}

/// agent 行要丢掉的 token：分组头已承载工作区 / 标签页身份时，子行不再重复。
#[derive(Default, Clone, Copy)]
struct TokenDrop {
    workspace: bool,
    tab: bool,
}

/// 按 [`TokenDrop`] 丢 token。某行因此只剩状态图标时并进下一行（默认行配置
/// `[状态图标 机器 工作区 标签页] / [agent]` 在树里就成了 `○ one` 一行），不留
/// 只有图标的半行；因此变空的行直接去掉。
fn drop_group_tokens(lines: &mut Vec<Vec<crate::ui::ResolvedToken>>, drop: TokenDrop) {
    use crate::ui::ResolvedTokenKind as Kind;
    let mut index = 0;
    while index < lines.len() {
        let before = lines[index].len();
        lines[index].retain(|token| match token.kind {
            Kind::Workspace(_) => !drop.workspace,
            Kind::Tab(_) => !drop.tab,
            _ => true,
        });
        let dropped = lines[index].len() != before;
        if lines[index].is_empty() {
            lines.remove(index);
            continue;
        }
        let icon_only = lines[index]
            .iter()
            .all(|token| matches!(token.kind, Kind::StateIcon));
        if dropped && icon_only && index + 1 < lines.len() {
            // 并进下一行的行首；下一行还没过滤，留在原下标继续处理。
            let icons = lines.remove(index);
            lines[index].splice(0..0, icons);
            continue;
        }
        index += 1;
    }
}

/// 状态文案（次要信息）画在哪一行：行配置里已有 `state_text` token 时不另画
/// （`None`）；否则画在带 agent 名的那一行，没有名字 token 时画在首行。
fn status_text_line(lines: &[Vec<crate::ui::ResolvedToken>]) -> Option<usize> {
    use crate::ui::ResolvedTokenKind as Kind;
    if lines
        .iter()
        .flatten()
        .any(|token| matches!(token.kind, Kind::StateText(_)))
    {
        return None;
    }
    Some(
        lines
            .iter()
            .position(|line| {
                line.iter()
                    .any(|token| matches!(token.kind, Kind::Agent(_)))
            })
            .unwrap_or(0),
    )
}

struct TreeBuilder<'a> {
    rows: &'a mut Vec<AgentTreeRow>,
    config: &'a ClientShellConfig,
    collapse: &'a CollapseState<'a>,
    active_endpoint_id: &'a ClientEndpointId,
    /// 平铺视图只要属主行（agent / 外部条目）：不挂活动子行，也没有折叠开关。
    owners_only: bool,
}

impl TreeBuilder<'_> {
    fn push_group(
        &mut self,
        depth: u8,
        key: String,
        endpoint: CachedEndpointSnapshot<'_>,
        count: usize,
        collapsed: bool,
        kind: AgentTreeKind,
    ) {
        self.rows.push(TreeEntry {
            depth,
            key,
            kind: AgentTreeNode::new(endpoint, count, None, kind),
            last_child_mask: 0,
            has_children: true,
            collapsed,
        });
    }

    fn push_agent(
        &mut self,
        depth: u8,
        endpoint: CachedEndpointSnapshot<'_>,
        agent: &ClientShellAgent,
        machine: Option<&str>,
        drop: TokenDrop,
    ) {
        let Some(mut row) = agent_row(endpoint.snapshot, &agent.pane_id, self.config, machine)
        else {
            return;
        };
        row.focused &= endpoint.endpoint_id == self.active_endpoint_id;
        if drop.workspace || drop.tab {
            drop_group_tokens(&mut row.rows, drop);
        }
        let status_line = status_text_line(&row.rows);
        let owner = owner_key(&AgentActivityOwner::Pane {
            pane_id: agent.pane_id.clone(),
        });
        let machine_initial = endpoint.label.chars().next().unwrap_or('?');
        self.push_owner(
            depth,
            endpoint,
            owner,
            &agent.activity,
            AgentTreeKind::Agent {
                agent: row,
                machine_initial,
                status_line,
            },
        );
    }

    fn push_external_groups(&mut self, depth: u8, endpoint: CachedEndpointSnapshot<'_>) {
        let externals = &endpoint.snapshot.external_agents;
        if externals.is_empty() {
            return;
        }
        for source in external_sources(externals) {
            let key = external_group_key(source);
            let collapsed = self.collapse.group_key_present(endpoint.endpoint_id, &key);
            let count = externals
                .iter()
                .filter(|external| external.source == source)
                .count();
            self.push_group(
                depth,
                key,
                endpoint,
                count,
                collapsed,
                AgentTreeKind::ExternalGroup {
                    label: format!(
                        "{} · {source}",
                        crate::i18n::texts().agent_panel.external_group
                    ),
                },
            );
            if collapsed {
                continue;
            }
            for external in externals
                .iter()
                .filter(|external| external.source == source)
            {
                self.push_external_entry(depth + 1, endpoint, external);
            }
        }
    }

    /// 平铺视图的外部条目：不画来源分组头、不看其折叠态，顺序与树里一致（按
    /// source 首次出现的顺序归组）。
    fn push_external_entries(&mut self, depth: u8, endpoint: CachedEndpointSnapshot<'_>) {
        let externals = &endpoint.snapshot.external_agents;
        if externals.is_empty() {
            return;
        }
        for source in external_sources(externals) {
            for external in externals
                .iter()
                .filter(|external| external.source == source)
            {
                self.push_external_entry(depth, endpoint, external);
            }
        }
    }

    fn push_external_entry(
        &mut self,
        depth: u8,
        endpoint: CachedEndpointSnapshot<'_>,
        external: &ClientShellExternalAgent,
    ) {
        let owner = owner_key(&AgentActivityOwner::External {
            external_id: external.external_id.clone(),
        });
        self.push_owner(
            depth,
            endpoint,
            owner,
            &external.activity,
            AgentTreeKind::ExternalAgent {
                external_id: external.external_id.clone(),
                label: if external.label.is_empty() {
                    external.external_id.clone()
                } else {
                    external.label.clone()
                },
                agent: external.agent.clone(),
                status: external.agent_status,
                readable: external.readable,
            },
        );
    }

    /// 活动树的属主行（agent 或外部条目）及其展开的活动节点。
    fn push_owner(
        &mut self,
        depth: u8,
        endpoint: CachedEndpointSnapshot<'_>,
        owner: String,
        activity: &ClientShellAgentActivity,
        kind: AgentTreeKind,
    ) {
        let hidden = if activity.truncated {
            activity
                .total
                .saturating_sub(activity.nodes.len().min(u32::MAX as usize) as u32)
        } else {
            0
        };
        let has_children = !self.owners_only && (!activity.nodes.is_empty() || hidden > 0);
        let key = activity_list_key(&owner);
        // 活动摘要默认折叠：键在集合里才展开。
        let expanded = has_children && self.collapse.group_key_present(endpoint.endpoint_id, &key);
        self.rows.push(TreeEntry {
            depth,
            key,
            kind: AgentTreeNode::new(endpoint, 0, Some(activity), kind),
            last_child_mask: 0,
            has_children,
            collapsed: has_children && !expanded,
        });
        if expanded {
            self.push_activity(depth + 1, endpoint, &owner, &activity.nodes, hidden);
        }
    }

    /// 前序展开活动节点：根 = 没有父节点或父节点不在列表里；子节点默认折叠
    /// （键在集合里才展开）。用显式栈而不递归，`visited` 兜住重复 id 造成的环。
    fn push_activity(
        &mut self,
        depth: u8,
        endpoint: CachedEndpointSnapshot<'_>,
        owner: &str,
        nodes: &[ClientShellActivityNode],
        hidden: u32,
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
                    0,
                    None,
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
            self.rows.push(TreeEntry {
                depth,
                key: String::new(),
                kind: AgentTreeNode::new(
                    endpoint,
                    0,
                    None,
                    AgentTreeKind::ActivityMore {
                        owner_key: owner.to_owned(),
                        label: crate::i18n::fill(
                            crate::i18n::texts().agent_panel.activity_more_fmt,
                            &[("n", &hidden.to_string())],
                        ),
                    },
                ),
                last_child_mask: 0,
                has_children: false,
                collapsed: false,
            });
        }
    }
}

/// 外部条目的来源，按首次出现的顺序去重；来源种类极少，线性查重即可。
fn external_sources(externals: &[ClientShellExternalAgent]) -> Vec<&str> {
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

/// 渲染阶段：表头 + 统一树的行列表。`endpoint_qualified` 决定 agent 行写进
/// `hits.agents`（classic 单端点，端点隐含为本机）还是 `hits.endpoint_agents`
/// （联邦 / workbench）。列表区不足 3 行（放不下「分组头 + 一个 agent 行」）时退化
/// 为 [`flat_view_rows`] 的平铺视图：没有分组头，被折叠分组里的 agent 也列出，
/// 仍可见可点。
#[allow(clippy::too_many_arguments)]
pub(super) fn render_agent_tree_rows(
    buffer: &mut Buffer,
    area: Rect,
    agent_view_label: Option<&str>,
    rows: &[AgentTreeRow],
    config: &ClientShellConfig,
    agent_scroll: &mut usize,
    chrome_hover: Option<&ChromeHover>,
    hits: &mut ShellHitMap,
    endpoint_qualified: bool,
) {
    if !super::agent_sidebar::render_agent_panel_header(
        buffer,
        area,
        agent_view_label,
        config,
        chrome_hover,
        hits,
    ) {
        return;
    }
    let empty_message = agent_view_label.map(|_| crate::i18n::texts().sidebar.no_matching_agents);
    let thumb_hovered = matches!(chrome_hover, Some(ChromeHover::AgentScrollbarThumb));
    let cx = RowContext {
        config,
        chrome_hover,
        endpoint_qualified,
    };
    if area.height.saturating_sub(3) < 3 {
        render_agent_list(
            buffer,
            area,
            flat_view_rows(rows),
            empty_message,
            config,
            agent_scroll,
            thumb_hovered,
            hits,
            row_lines,
            |buffer, rect, row, hits| render_tree_row(buffer, rect, row, &cx, hits, true),
        );
        return;
    }
    render_agent_list(
        buffer,
        area,
        rows,
        empty_message,
        config,
        agent_scroll,
        thumb_hovered,
        hits,
        row_lines,
        |buffer, rect, row, hits| render_tree_row(buffer, rect, row, &cx, hits, false),
    );
}

fn row_lines(row: &AgentTreeRow) -> usize {
    row.kind.agent().map_or(1, |agent| agent.rows.len().max(1))
}

struct RowContext<'a> {
    config: &'a ClientShellConfig,
    chrome_hover: Option<&'a ChromeHover>,
    endpoint_qualified: bool,
}

/// 树前缀里折叠开关的命中区（无子节点或被裁掉时为空），续行据此决定要不要在
/// 开关列画接到子节点的引导线。
struct Prefix {
    toggle: Rect,
}

fn render_tree_row(
    buffer: &mut Buffer,
    rect: Rect,
    row: &AgentTreeRow,
    cx: &RowContext<'_>,
    hits: &mut ShellHitMap,
    flat: bool,
) {
    let palette = &cx.config.palette;
    let node = &row.kind;
    let (depth, mask, has_children, collapsed) = if flat {
        (0, 0, false, false)
    } else {
        (
            row.depth,
            row.last_child_mask,
            row.has_children,
            row.collapsed,
        )
    };
    let toggle_hovered = matches!(
        cx.chrome_hover,
        Some(ChromeHover::AgentTreeToggle(endpoint_id, key))
            if endpoint_id == &node.endpoint_id && key == &row.key
    );
    let row_hovered = match &node.kind {
        AgentTreeKind::Agent { agent, .. } => match cx.chrome_hover {
            Some(ChromeHover::AgentRow(pane_id)) => {
                !cx.endpoint_qualified && pane_id == &agent.pane_id
            }
            Some(ChromeHover::EndpointAgentRow(endpoint_id, pane_id)) => {
                cx.endpoint_qualified
                    && endpoint_id == &node.endpoint_id
                    && pane_id == &agent.pane_id
            }
            _ => false,
        },
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
        AgentTreeKind::ExternalAgent { external_id, .. } => matches!(
            cx.chrome_hover,
            Some(ChromeHover::ExternalAgentRow(endpoint_id, id))
                if endpoint_id == &node.endpoint_id && id == external_id
        ),
        // 分组头整行都是开关：悬浮即开关悬浮。
        _ => toggle_hovered,
    };
    let focused = node.agent().is_some_and(|agent| agent.focused);
    if focused {
        buffer.set_style(rect, Style::default().bg(palette.active_row_bg));
    } else if row_hovered {
        buffer.set_style(rect, Style::default().bg(palette.hover_row_bg()));
    }

    let prefix_style = Style::default().fg(palette.overlay0);
    let (used, toggle) = render_tree_prefix(
        buffer,
        rect.x,
        rect.y,
        rect.width,
        depth,
        mask,
        has_children,
        collapsed,
        false,
        prefix_style,
    );
    if has_children && !toggle.is_empty() {
        buffer.set_style(
            toggle,
            Style::default().fg(if toggle_hovered {
                palette.text
            } else {
                palette.accent
            }),
        );
    }
    let prefix = Prefix { toggle };
    let content = Rect::new(
        rect.x.saturating_add(used),
        rect.y,
        rect.width.saturating_sub(used),
        rect.height,
    );

    match &node.kind {
        AgentTreeKind::Agent {
            agent, status_line, ..
        } => {
            render_agent_lines(
                buffer,
                rect,
                content,
                row,
                agent,
                *status_line,
                cx,
                depth,
                mask,
                &prefix,
            );
            if has_children && !toggle.is_empty() {
                hits.agent_tree_toggles
                    .push((toggle, node.endpoint_id.clone(), row.key.clone()));
            }
            if cx.endpoint_qualified {
                hits.endpoint_agents
                    .push((rect, node.endpoint_id.clone(), agent.pane_id.clone()));
            } else {
                hits.agents.push((rect, agent.pane_id.clone()));
            }
        }
        AgentTreeKind::Activity {
            owner_key,
            node_id,
            label,
            kind,
            status,
        } => {
            render_activity_line(buffer, content, label, *kind, *status, cx);
            if has_children && !toggle.is_empty() {
                hits.agent_tree_toggles
                    .push((toggle, node.endpoint_id.clone(), row.key.clone()));
            }
            hits.agent_activity_rows.push(AgentActivityHit {
                rect,
                endpoint_id: node.endpoint_id.clone(),
                owner_key: owner_key.clone(),
                node_id: node_id.clone(),
            });
        }
        AgentTreeKind::ActivityMore { owner_key, label } => {
            put_text(
                buffer,
                content.x,
                content.y,
                content.width,
                label,
                Style::default()
                    .fg(palette.overlay0)
                    .add_modifier(Modifier::DIM),
            );
            hits.agent_activity_rows.push(AgentActivityHit {
                rect,
                endpoint_id: node.endpoint_id.clone(),
                owner_key: owner_key.clone(),
                node_id: String::new(),
            });
        }
        AgentTreeKind::ExternalAgent {
            external_id,
            label,
            agent,
            status,
            readable,
        } => {
            render_external_line(
                buffer,
                content,
                node,
                label,
                agent.as_deref(),
                *status,
                *readable,
                cx,
            );
            if has_children && !toggle.is_empty() {
                hits.agent_tree_toggles
                    .push((toggle, node.endpoint_id.clone(), row.key.clone()));
            }
            hits.external_agents
                .push((rect, node.endpoint_id.clone(), external_id.clone()));
        }
        AgentTreeKind::Machine { .. }
        | AgentTreeKind::Workspace { .. }
        | AgentTreeKind::Tab { .. }
        | AgentTreeKind::ExternalGroup { .. } => {
            render_group_line(buffer, content, node, cx);
            if !flat {
                // 分组头整行都是折叠开关（桌面树的惯例）。
                hits.agent_tree_toggles
                    .push((rect, node.endpoint_id.clone(), row.key.clone()));
            }
        }
    }
    if node.stale {
        buffer.set_style(
            rect,
            Style::default()
                .fg(palette.overlay0)
                .add_modifier(Modifier::DIM),
        );
    }
}

/// 分组头：`[状态图标] 标签 …… · N`。宽度预算：计数（值）> 标签 > 次要信息
/// （离线机器的状态文案）。
fn render_group_line(
    buffer: &mut Buffer,
    content: Rect,
    node: &AgentTreeNode,
    cx: &RowContext<'_>,
) {
    let palette = &cx.config.palette;
    let (icon, icon_color, label, secondary): (&str, _, &str, Option<&str>) = match &node.kind {
        AgentTreeKind::Machine { label, status } => {
            let (glyph, status_label, color) =
                super::endpoints::endpoint_status_presentation(*status, palette, "…");
            (
                glyph,
                color,
                label.as_str(),
                (*status != ClientEndpointStatus::Online).then_some(status_label),
            )
        }
        AgentTreeKind::Workspace { label, status } | AgentTreeKind::Tab { label, status } => (
            status_icon(*status, cx.config.status_indicators),
            status_color(*status, palette),
            label.as_str(),
            None,
        ),
        AgentTreeKind::ExternalGroup { label } => ("", palette.overlay0, label.as_str(), None),
        _ => return,
    };
    let count = node.count_label();
    let count_width = display_width(count).min(usize::from(content.width)) as u16;
    let count_x = content.right().saturating_sub(count_width);
    put_text(
        buffer,
        count_x,
        content.y,
        count_width,
        count,
        Style::default().fg(palette.overlay0),
    );
    let mut x = content.x;
    let mut remaining = count_x.saturating_sub(content.x).saturating_sub(1);
    if !icon.is_empty() {
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
    }
    let label_width = (display_width(label) as u16).min(remaining);
    put_text(
        buffer,
        x,
        content.y,
        label_width,
        label,
        Style::default()
            .fg(palette.text)
            .add_modifier(Modifier::BOLD),
    );
    if let Some(secondary) = secondary {
        let x = x.saturating_add(label_width + 1);
        let width = display_width(secondary) as u16;
        if x.saturating_add(width) <= count_x.saturating_sub(1) {
            put_text(
                buffer,
                x,
                content.y,
                width,
                secondary,
                Style::default().fg(palette.overlay0),
            );
        }
    }
}

/// agent 行：首行「前缀 + token 行 + 右侧活动徽标」，续行画祖先引导线并缩进两列
/// 对齐名称；`status_line` 那一行在 token 之后补状态文案。宽度预算：状态图标与
/// 名称先保 [`PRIMARY_MIN_WIDTH`] 列 > 徽标（完整 → 只留数字 → 不画）> token 行
/// 其余部分（`resolved_token_spans` 自行按固定 / 弹性宽度裁剪）> 状态文案（次要
/// 信息，整段放不下就不画）。
#[allow(clippy::too_many_arguments)]
fn render_agent_lines(
    buffer: &mut Buffer,
    rect: Rect,
    content: Rect,
    row: &AgentTreeRow,
    agent: &AgentRow,
    status_line: Option<usize>,
    cx: &RowContext<'_>,
    depth: u8,
    mask: u64,
    prefix: &Prefix,
) {
    let palette = &cx.config.palette;
    let name_style = Style::default()
        .fg(if agent.focused {
            palette.text
        } else {
            palette.subtext0
        })
        .add_modifier(Modifier::BOLD);
    let status_style = Style::default().fg(status_color(agent.status, palette));
    let secondary = Style::default().fg(palette.overlay0);
    let icon = (
        status_icon(agent.status, cx.config.status_indicators),
        status_style,
    );
    // 行配置解析后一个 token 都没有：至少画状态图标，行仍可点。
    let fallback = [crate::ui::ResolvedToken {
        kind: crate::ui::ResolvedTokenKind::StateIcon,
        style: Default::default(),
    }];
    let lines: &[Vec<crate::ui::ResolvedToken>] = &agent.rows;
    // 徽标只在首行右侧，按宽度档位画，剩下的给 token 行。
    let first_width = render_badge(buffer, content, &row.kind, cx);
    let continuation_indent = tree_prefix_width(depth).saturating_add(2);
    for index in 0..usize::from(rect.height) {
        let y = rect.y + index as u16;
        let tokens: &[crate::ui::ResolvedToken] = match lines.get(index) {
            Some(tokens) => tokens.as_slice(),
            None if index == 0 => &fallback,
            None => break,
        };
        let (x, width) = if index == 0 {
            (content.x, first_width)
        } else {
            render_continuation_guides(
                buffer,
                rect.x,
                y,
                rect.width,
                depth,
                mask,
                row.has_children && !row.collapsed,
                Style::default().fg(palette.overlay0),
                prefix,
            );
            (
                rect.x.saturating_add(continuation_indent),
                rect.width.saturating_sub(continuation_indent),
            )
        };
        if width == 0 {
            continue;
        }
        let spans = crate::ui::resolved_token_spans(
            tokens,
            icon,
            status_style,
            name_style,
            secondary,
            secondary,
            palette,
            usize::from(width),
        );
        let line = Line::from(spans);
        let used = u16::try_from(line.width()).unwrap_or(u16::MAX).min(width);
        Paragraph::new(line).render(Rect::new(x, y, width, 1), buffer);
        if status_line == Some(index) {
            let text = agent.state_text.as_str();
            let text_width = display_width(text) as u16;
            if !text.is_empty() && used.saturating_add(1).saturating_add(text_width) <= width {
                put_text(buffer, x + used + 1, y, text_width, text, secondary);
            }
        }
    }
}

/// 在 `content` 首行右侧画活动徽标（档位见 [`ActivityBadge::fit`]），返回首行
/// 留给左侧内容（状态图标、名称等）的宽度：已扣掉徽标与它前面的 1 列间隔，
/// 没画徽标时是整行宽。
fn render_badge(
    buffer: &mut Buffer,
    content: Rect,
    node: &AgentTreeNode,
    cx: &RowContext<'_>,
) -> u16 {
    let Some((text, width)) = node
        .badge
        .as_ref()
        .and_then(|badge| badge.fit(content.width))
    else {
        return content.width;
    };
    let palette = &cx.config.palette;
    put_text(
        buffer,
        content.right().saturating_sub(width),
        content.y,
        width,
        text,
        Style::default().fg(if node.running {
            palette.yellow
        } else {
            palette.overlay0
        }),
    );
    content.width.saturating_sub(width + 1)
}

/// agent 行续行的引导线：祖先各层沿用末子掩码（`│ ` / 空白），本节点展开时在
/// 开关列画 `│` 接到下面的活动节点。
#[allow(clippy::too_many_arguments)]
fn render_continuation_guides(
    buffer: &mut Buffer,
    x: u16,
    y: u16,
    max_width: u16,
    depth: u8,
    mask: u64,
    connect_children: bool,
    style: Style,
    prefix: &Prefix,
) {
    let mut cursor = x;
    let end = x.saturating_add(max_width);
    for level in 1..=u32::from(depth.min(crate::ui::kit::tree::MAX_TREE_DEPTH)) {
        if cursor.saturating_add(2) > end {
            return;
        }
        let text = if mask & (1u64 << level) != 0 {
            "  "
        } else {
            "│ "
        };
        put_text(buffer, cursor, y, 2, text, style);
        cursor = cursor.saturating_add(2);
    }
    if connect_children && !prefix.toggle.is_empty() && cursor < end {
        put_text(buffer, cursor, y, 1, "│", style);
    }
}

/// 活动节点行：`状态图标 标签 [种类]`。标签优先于种类（次要信息）。
fn render_activity_line(
    buffer: &mut Buffer,
    content: Rect,
    label: &str,
    kind: AgentActivityKind,
    status: AgentActivityStatus,
    cx: &RowContext<'_>,
) {
    let palette = &cx.config.palette;
    let mapped = activity_status_as_agent(status);
    let icon = status_icon(mapped, cx.config.status_indicators);
    let icon_color = status_color(mapped, palette);
    let texts = &crate::i18n::texts().agent_activity;
    let kind_label = match kind {
        AgentActivityKind::Subagent => texts.kind_subagent,
        AgentActivityKind::Task => texts.kind_task,
        AgentActivityKind::Todo => texts.kind_todo,
        AgentActivityKind::Background => texts.kind_background,
        AgentActivityKind::Unknown => texts.kind_unknown,
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
    let label_width = (display_width(label) as u16).min(remaining);
    put_text(
        buffer,
        x,
        content.y,
        label_width,
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
            Style::default().fg(palette.overlay0),
        );
    }
}

/// 外部条目行：`状态图标 标签 [agent] [暂不可读]`，右侧活动徽标；宽度预算同
/// agent 行（状态图标与标签先保 [`PRIMARY_MIN_WIDTH`] 列，徽标三档退化）。
#[allow(clippy::too_many_arguments)]
fn render_external_line(
    buffer: &mut Buffer,
    content: Rect,
    node: &AgentTreeNode,
    label: &str,
    agent: Option<&str>,
    status: AgentStatus,
    readable: bool,
    cx: &RowContext<'_>,
) {
    let palette = &cx.config.palette;
    let mut remaining = render_badge(buffer, content, node, cx);
    let mut x = content.x;
    let icon = status_icon(status, cx.config.status_indicators);
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
        Style::default().fg(status_color(status, palette)),
    );
    x = x.saturating_add(icon_width + 1);
    remaining = remaining.saturating_sub(icon_width + 1);
    let label_width = (display_width(label) as u16).min(remaining);
    put_text(
        buffer,
        x,
        content.y,
        label_width,
        label,
        Style::default()
            .fg(palette.subtext0)
            .add_modifier(Modifier::BOLD),
    );
    x = x.saturating_add(label_width + 1);
    remaining = remaining.saturating_sub(label_width + 1);
    let secondary = Style::default().fg(palette.overlay0);
    for extra in [
        agent,
        (!readable).then_some(crate::i18n::texts().agent_panel.external_unreadable),
    ]
    .into_iter()
    .flatten()
    {
        let width = display_width(extra) as u16;
        if remaining < width + 1 {
            break;
        }
        put_text(buffer, x, content.y, width, extra, secondary);
        x = x.saturating_add(width + 1);
        remaining = remaining.saturating_sub(width + 1);
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

impl ClientShellState {
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
                if !node_id.is_empty() {
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

    /// 聚焦某端点上的 agent pane：当前端点直接发 `pane.focus`，其它在线端点先
    /// 切过去；不在线则提示。
    pub(super) fn focus_agent_pane(
        &mut self,
        endpoint_id: ClientEndpointId,
        pane_id: String,
        outcome: &mut ClientShellInput,
    ) {
        if !self.endpoint_is_online(&endpoint_id) {
            let label = self.endpoint_label(&endpoint_id).to_owned();
            self.receive_endpoint_unavailable(crate::i18n::fill(
                crate::i18n::texts().mobile.reconnecting_fmt,
                &[("label", &label)],
            ));
            outcome.repaint = true;
        } else if endpoint_id == self.active_endpoint_id {
            self.push_endpoint_method(
                crate::api::schema::Method::PaneFocus(crate::api::schema::PaneTarget { pane_id }),
                outcome,
            );
        } else {
            outcome.actions.push(ClientShellAction::ActivateEndpoint {
                endpoint_id,
                target: Some(ClientEndpointFocusTarget::Pane(pane_id)),
            });
        }
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
            .find(|(rect, _, _)| super::contains(*rect, point))
            .map(|(_, endpoint_id, external_id)| (endpoint_id.clone(), external_id.clone()));
        if let Some((endpoint_id, external_id)) = external {
            self.open_external_agent_context_menu(endpoint_id, external_id, point.0, point.1);
            return true;
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
        let target = ClientContextMenuTarget::Agent {
            endpoint_id,
            pane_id,
            agent: agent.agent.clone(),
            has_activity: agent.activity.total > 0 || !agent.activity.nodes.is_empty(),
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
            (Action::BindAgentAccount, AgentActivityOwner::Pane { pane_id }) => {
                self.bind_agent_account(endpoint_id, pane_id, outcome);
            }
            (Action::CloseAgentPane, AgentActivityOwner::Pane { pane_id }) => {
                self.close_agent_pane(endpoint_id, pane_id, outcome);
            }
            (Action::ShowAgentUsage, AgentActivityOwner::Pane { pane_id }) => {
                // 打开并钉住该 agent 的用量卡；厂商取快照里的 agent 名，未识别
                // （`None`）或不可绑定（muse）时没有用量可看，无动作。
                let agent = self
                    .endpoint_agent(&endpoint_id, &pane_id)
                    .and_then(|agent| agent.agent.clone())
                    .filter(|name| super::observability::is_bindable_agent(name));
                if let Some(agent) = agent {
                    self.pin_agent_usage_card(endpoint_id, pane_id, agent, outcome);
                }
            }
            _ => {}
        }
    }

    /// 「重命名窗格」：沿用 pane 重命名浮层（标题同键盘重命名路径
    /// `open_rename_pane_overlay`，即 `dialogs.rename_pane`）与 `pane.rename`，
    /// 改的是 pane 标签，菜单文案照实写「重命名窗格」。
    /// 浮层提交发往当前端点（`ClientRenameTarget::Pane` 不带端点），所以只对
    /// 当前端点直接打开；其它端点的 agent 在菜单里该项应灰显，这里的兜底只是切
    /// 过去并聚焦该 pane（与「聚焦」相同），不在端点切换完成前打开浮层，免得
    /// 重命名落到同 id 的别处 pane 上。
    fn rename_agent_pane(
        &mut self,
        endpoint_id: ClientEndpointId,
        pane_id: String,
        outcome: &mut ClientShellInput,
    ) {
        if endpoint_id != self.active_endpoint_id {
            self.focus_agent_pane(endpoint_id, pane_id, outcome);
            return;
        }
        let label = self.snapshot.as_deref().and_then(|snapshot| {
            snapshot
                .panes
                .iter()
                .find(|pane| pane.pane_id == pane_id)
                .and_then(|pane| pane.label.clone())
        });
        self.overlay = Some(ClientShellOverlay::Rename(ClientRenameOverlay {
            title: crate::i18n::texts().dialogs.rename_pane,
            input: TextEditor::new(label.as_deref().unwrap_or_default(), label.is_none()),
            target: ClientRenameTarget::Pane { pane_id },
        }));
        outcome.repaint = true;
    }

    /// 「关闭窗格」：发 `pane.close`，与 pane 右键菜单的「关闭窗格」同一请求；
    /// 其它在线端点直接发往该端点（不切换当前端点），不在线则提示。
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
            PendingEndpointKind::Generic,
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

    /// 「绑定账号」：打开监控 → 账号页，选中该 agent 的厂商，并把页面的待绑定
    /// pane 设为这个 agent（账号页的「绑定」随即作用于它）。账号页的作用域是
    /// 当前端点：其它端点的 agent 先切过去并聚焦该 pane，页面只选厂商，pane 由
    /// 「绑定到聚焦 pane」接手。
    fn bind_agent_account(
        &mut self,
        endpoint_id: ClientEndpointId,
        pane_id: String,
        outcome: &mut ClientShellInput,
    ) {
        let Some(agent) = self.endpoint_agent(&endpoint_id, &pane_id) else {
            return;
        };
        let Some(provider) = agent
            .agent
            .clone()
            .filter(|name| super::observability::is_bindable_agent(name))
        else {
            return;
        };
        let label = bind_candidate_label(agent);
        let active = endpoint_id == self.active_endpoint_id;
        if !active {
            if !self.endpoint_is_online(&endpoint_id) {
                self.focus_agent_pane(endpoint_id, pane_id, outcome);
                return;
            }
            self.focus_agent_pane(endpoint_id, pane_id.clone(), outcome);
        }
        self.open_observation_page(super::observability::Page::Accounts, outcome);
        self.observation_action(super::observability::Action::Provider(provider), outcome);
        if active {
            self.observability.selected_pane = Some(pane_id);
            self.observability.selected_pane_label = Some(label);
        }
    }
}

/// 账号页绑定行里 pane 的显示名：agent 显示名 · pane id。与
/// `observability.rs` 的私有同名函数同一格式（那边归监控车道，这里不改它）。
fn bind_candidate_label(agent: &ClientShellAgent) -> String {
    let name = agent
        .name
        .as_deref()
        .or(agent.display_agent.as_deref())
        .or(agent.agent.as_deref())
        .unwrap_or("agent");
    format!("{name} · {}", agent.pane_id)
}
