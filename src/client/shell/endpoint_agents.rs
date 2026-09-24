use super::agent_sidebar::AgentRow;
use super::agent_tree::{
    build_agent_tree, AgentRowsView, AgentTreeKind, AgentTreeNode, AgentTreeRow, CollapseState,
};
use super::render::put_text;
use super::*;

/// 折叠侧栏（多机）agent 区的行：统一树平铺行里的 agent，按画面顺序（节点、
/// 行数据、机器首字母）。渲染与输入阶段的键盘揭示共用这一个取法（D10）。
fn collapsed_agents(
    rows: AgentRowsView<'_>,
) -> impl Iterator<Item = (&AgentTreeNode, &AgentRow, char)> {
    rows.flat.iter().filter_map(|row| match &row.kind.kind {
        AgentTreeKind::Agent {
            agent,
            machine_initial,
            ..
        } => Some((&row.kind, agent, *machine_initial)),
        _ => None,
    })
}

/// 折叠侧栏的单列视图：每个 agent 一行（机器首字母 + 状态图标），取统一树的
/// 平铺行（`AgentRowsView::flat`）。这里不画分组头，面板内折叠了的分组
/// 在这里展不开，所以按聚合顺序列出全部 agent；机器层折叠照旧藏起该端点的
/// agent（上方工作区区的机器行可切换）。
///
/// 行数超过 agent 区时按 `agent_scroll` 滚动（D10）：回写 `hits.agent_body` 与
/// `hits.agent_max_scroll` 供滚轮与键盘揭示使用。渲染只读，越界的滚动位置在这里
/// 按上界夹住来画、不写回——钳位写回在输入阶段。
pub(super) fn render_collapsed(
    buffer: &mut Buffer,
    area: Rect,
    rows: AgentRowsView<'_>,
    config: &ClientShellConfig,
    agent_scroll: usize,
    chrome_hover: Option<&super::feedback::ChromeHover>,
    hits: &mut ShellHitMap,
) {
    let metrics =
        super::scroll::uniform_scroll_metrics(rows.flat_agents, area.height, agent_scroll);
    let start = metrics
        .max_offset_from_bottom
        .saturating_sub(metrics.offset_from_bottom);
    hits.agent_body = area;
    hits.agent_max_scroll = metrics.max_offset_from_bottom;
    for (index, (node, agent, initial)) in collapsed_agents(rows)
        .skip(start)
        .take(area.height as usize)
        .enumerate()
    {
        let rect = Rect::new(area.x, area.y + index as u16, area.width, 1);
        let hovered = matches!(
            chrome_hover,
            Some(super::feedback::ChromeHover::EndpointAgentRow(endpoint_id, pane_id))
                if endpoint_id == &node.endpoint_id && pane_id == &agent.pane_id
        );
        if agent.focused {
            buffer.set_style(rect, Style::default().bg(config.palette.active_row_bg));
        } else if hovered {
            buffer.set_style(rect, Style::default().bg(config.palette.hover_row_bg()));
        }
        put_text(
            buffer,
            rect.x,
            rect.y,
            rect.width,
            &format!(
                "{initial}{}",
                status_icon(agent.status, config.status_indicators)
            ),
            Style::default()
                .fg(if node.stale {
                    config.palette.overlay0
                } else {
                    status_color(agent.status, &config.palette)
                })
                .add_modifier(if node.stale {
                    Modifier::DIM
                } else {
                    Modifier::empty()
                }),
        );
        hits.endpoint_agents
            .push((rect, node.endpoint_id.clone(), agent.pane_id.clone()));
    }
}

/// 联邦 / workbench 的 Agents 面板：与 classic 同一套树渲染，只是 agent 行写进
/// 端点限定的命中区。
#[allow(clippy::too_many_arguments)]
pub(super) fn render_expanded(
    buffer: &mut Buffer,
    area: Rect,
    agent_view_label: Option<&str>,
    rows: AgentRowsView<'_>,
    config: &ClientShellConfig,
    agent_scroll: &mut usize,
    chrome_hover: Option<&super::feedback::ChromeHover>,
    hits: &mut ShellHitMap,
) {
    super::agent_tree::render_agent_tree_rows(
        buffer,
        area,
        agent_view_label,
        rows,
        config,
        agent_scroll,
        chrome_hover,
        hits,
        true,
    );
}

/// 联邦 agents 面板行的缓存键：端点集合与各自快照分代、排序、过滤标签、配置
/// 代际。任何一项变化才重算（PERF-02）。
/// 键里的每个端点：身份 + 快照分代（revision, boot）+ 显示名 + 是否 stale。
type EndpointRowsKey = (ClientEndpointId, Option<(u64, String)>, String, bool);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AgentRowsKey {
    config_epoch: u64,
    data_epoch: u64,
    /// 折叠 / 展开集合的代际（`ClientShellState::tree_collapse_epoch`）。
    collapse_epoch: u64,
    /// 不随快照 revision 变化的活动 / 外部来源数据的代际
    /// （`ClientShellState::agent_activity_epoch`）。
    activity_epoch: u64,
    active_endpoint: ClientEndpointId,
    sort: crate::config::AgentPanelSortConfig,
    view_label: Option<String>,
    endpoints: Vec<EndpointRowsKey>,
}

/// Agents 面板统一树的行：视图计算阶段按 `AgentRowsKey` 维护，渲染只读。
/// classic / 联邦 / workbench 三条路径都从这里取行。
pub(super) struct AgentRowsCache {
    key: AgentRowsKey,
    rows: Vec<AgentTreeRow>,
    /// 平铺视图的行（见 `agent_tree::AgentTree::flat`）。
    flat_rows: Vec<AgentTreeRow>,
    /// 平铺行里 agent 行的个数（见 `AgentRowsView::flat_agents`）。
    flat_agents: usize,
}

impl AgentRowsCache {
    pub(super) fn key_for(
        endpoints: &[ClientShellEndpoint],
        config: &ClientShellConfig,
        config_epoch: u64,
        data_epoch: u64,
        collapse_epoch: u64,
        activity_epoch: u64,
        active_endpoint_id: &ClientEndpointId,
        view_label: Option<&str>,
    ) -> AgentRowsKey {
        AgentRowsKey {
            config_epoch,
            data_epoch,
            collapse_epoch,
            activity_epoch,
            active_endpoint: active_endpoint_id.clone(),
            sort: config.agent_panel_sort,
            view_label: view_label.map(str::to_owned),
            endpoints: endpoints
                .iter()
                .map(|endpoint| {
                    (
                        endpoint.endpoint_id.clone(),
                        endpoint
                            .snapshot
                            .as_deref()
                            .map(|snapshot| (snapshot.revision, snapshot.boot_id.clone())),
                        endpoint.label.clone(),
                        endpoint.status != ClientEndpointStatus::Online,
                    )
                })
                .collect(),
        }
    }

    /// 树行（测试按行序核对缓存内容用；渲染一律经 [`Self::view`]）。
    #[cfg(test)]
    pub(super) fn rows(&self) -> &[AgentTreeRow] {
        &self.rows
    }

    /// 平铺行（同上，只供测试）。
    #[cfg(test)]
    pub(super) fn flat_rows(&self) -> &[AgentTreeRow] {
        &self.flat_rows
    }

    /// 渲染用的两套行：树行与平铺行各自成片，经 `ShellRenderState` 并列传递。
    pub(super) fn view(&self) -> AgentRowsView<'_> {
        AgentRowsView {
            tree: &self.rows,
            flat: &self.flat_rows,
            flat_agents: self.flat_agents,
        }
    }

    pub(super) fn key_matches(&self, key: &AgentRowsKey) -> bool {
        self.key == *key
    }

    fn build(
        key: AgentRowsKey,
        endpoints: &[ClientShellEndpoint],
        active: &ClientEndpointId,
        config: &ClientShellConfig,
        collapse: &CollapseState<'_>,
    ) -> Self {
        let tree = build_agent_tree(endpoints, active, config, collapse);
        let flat_agents = collapsed_agents(AgentRowsView {
            flat: &tree.flat,
            ..AgentRowsView::default()
        })
        .count();
        Self {
            rows: tree.rows,
            flat_rows: tree.flat,
            flat_agents,
            key,
        }
    }
}

impl ClientShellState {
    /// 输入阶段：键盘在多机之间切到的 agent 在 Agents 面板可见窗口外时，把
    /// `agent_scroll` 改到恰好露出它的位置（上游 #4355）。行序与行高取法与
    /// `agent_tree::render_agent_tree_rows` 一致：列表区不足 3 行时画平铺行，否则画
    /// 树行；目标藏在折叠分组里（不在所画的行里）时不动。多机折叠侧栏的 agent 区
    /// 按平铺行等高计（D10），交给 [`Self::reveal_collapsed_endpoint_agent`]。
    ///
    /// 行缓存先按当前状态刷新：切换端点后行序跟着活动端点变（`AgentRowsKey` 含活动
    /// 端点），上游在激活后才揭示正是为了用目的端点的排序。
    pub(super) fn reveal_endpoint_agent(
        &mut self,
        endpoint_id: &ClientEndpointId,
        pane_id: &str,
        body_height: u16,
    ) {
        if body_height == 0 {
            return;
        }
        self.refresh_federated_agent_rows();
        if self.sidebar_collapsed && !self.workbench.enabled {
            self.reveal_collapsed_endpoint_agent(endpoint_id, pane_id);
            return;
        }
        let Some(rows) = self.federated_agent_rows.as_ref().map(AgentRowsCache::view) else {
            return;
        };
        let listed = if body_height < 3 {
            rows.flat
        } else {
            rows.tree
        };
        let Some(target) = listed.iter().position(|row| {
            &row.kind.endpoint_id == endpoint_id
                && row
                    .kind
                    .agent()
                    .is_some_and(|agent| agent.pane_id == pane_id)
        }) else {
            return;
        };
        let heights = listed
            .iter()
            .map(|row| {
                row.kind
                    .agent()
                    .map_or(1, |agent| agent.rows.len().max(1))
                    .min(u16::MAX as usize) as u16
            })
            .collect::<Vec<_>>();
        let mut gaps = vec![self.config.agents.row_gap; listed.len()];
        if let Some(last) = gaps.last_mut() {
            *last = 0;
        }
        self.agent_scroll = super::scroll::list_scroll_start_to_reveal(
            &heights,
            &gaps,
            body_height,
            self.agent_scroll,
            target,
        );
    }

    /// 输入阶段（D10）：键盘切到的 agent 在多机折叠侧栏的 agent 区外时，把
    /// `agent_scroll` 改到恰好露出它的位置，并按当前行数钳位写回。视口高度取上一帧
    /// 的 `hits.agent_body`，行序取视图计算阶段的行缓存（与 [`render_collapsed`]
    /// 同一取法）；侧栏没折叠、工作台布局或上一帧没有 agent 区时不动——那些视图的
    /// `agent_scroll` 按树行计，口径不同。
    pub(super) fn reveal_collapsed_endpoint_agent(
        &mut self,
        endpoint_id: &ClientEndpointId,
        pane_id: &str,
    ) {
        let body = self.hits.agent_body;
        if !self.sidebar_collapsed || self.workbench.enabled || body.is_empty() {
            return;
        }
        let Some(rows) = self.federated_agent_rows.as_ref().map(AgentRowsCache::view) else {
            return;
        };
        let Some(target) = collapsed_agents(rows).position(|(node, agent, _)| {
            &node.endpoint_id == endpoint_id && agent.pane_id == pane_id
        }) else {
            return;
        };
        self.agent_scroll = super::scroll::uniform_scroll_start_to_reveal(
            rows.flat_agents,
            body.height,
            self.agent_scroll,
            target,
        );
    }

    /// 视图计算阶段刷新 agents 面板的行缓存（PERF-02）：键未变则复用上一帧的行。
    pub(super) fn refresh_federated_agent_rows(&mut self) {
        let key = AgentRowsCache::key_for(
            &self.endpoints,
            &self.config,
            self.config_epoch,
            self.agent_rows_epoch,
            self.tree_collapse_epoch,
            self.agent_activity_epoch,
            &self.active_endpoint_id,
            self.snapshot
                .as_deref()
                .and_then(|snapshot| snapshot.agent_view_label.as_deref()),
        );
        if self
            .federated_agent_rows
            .as_ref()
            .is_none_or(|cache| !cache.key_matches(&key))
        {
            self.federated_agent_rows = Some(AgentRowsCache::build(
                key,
                &self.endpoints,
                &self.active_endpoint_id,
                &self.config,
                &CollapseState {
                    collapsed_groups: &self.collapsed_groups,
                    remote_collapsed_groups: &self.remote_collapsed_groups,
                    collapsed_endpoints: &self.collapsed_endpoints,
                },
            ));
        }
    }
}
