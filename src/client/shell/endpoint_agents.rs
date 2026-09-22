use super::agent_tree::{build_agent_tree, AgentTreeKind, AgentTreeRow, CollapseState};
use super::render::put_text;
use super::*;

/// 折叠侧栏的单列视图：只画统一树里的 agent 行（机器首字母 + 状态图标），行序
/// 与展开视图一致，被折叠分组藏起来的 agent 同样不出现。
pub(super) fn render_collapsed(
    buffer: &mut Buffer,
    area: Rect,
    rows: &[AgentTreeRow],
    config: &ClientShellConfig,
    chrome_hover: Option<&super::feedback::ChromeHover>,
    hits: &mut ShellHitMap,
) {
    let agents = rows.iter().filter_map(|row| match &row.kind.kind {
        AgentTreeKind::Agent {
            agent,
            machine_initial,
            ..
        } => Some((&row.kind, agent, *machine_initial)),
        _ => None,
    });
    for (index, (node, agent, initial)) in agents.take(area.height as usize).enumerate() {
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
    rows: &[AgentTreeRow],
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

/// 缓存行的类型名沿用旧名，`render.rs::ShellRenderState` 仍按它引用；实际类型是
/// 统一树的行。
pub(super) type EndpointAgentRow = AgentTreeRow;

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

    pub(super) fn rows(&self) -> &[AgentTreeRow] {
        &self.rows
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
        Self {
            rows: build_agent_tree(endpoints, active, config, collapse),
            key,
        }
    }
}

impl ClientShellState {
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
