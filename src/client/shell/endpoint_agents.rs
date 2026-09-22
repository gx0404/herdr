use super::render::put_text;
use super::*;

pub(super) fn render_collapsed(
    buffer: &mut Buffer,
    area: Rect,
    rows: &[EndpointAgentRow],
    config: &ClientShellConfig,
    chrome_hover: Option<&super::feedback::ChromeHover>,
    hits: &mut ShellHitMap,
) {
    for (index, row) in rows.iter().take(area.height as usize).enumerate() {
        let rect = Rect::new(area.x, area.y + index as u16, area.width, 1);
        let hovered = matches!(
            chrome_hover,
            Some(super::feedback::ChromeHover::EndpointAgentRow(endpoint_id, pane_id))
                if endpoint_id == &row.endpoint_id && pane_id == &row.agent.pane_id
        );
        if row.agent.focused {
            buffer.set_style(rect, Style::default().bg(config.palette.active_row_bg));
        } else if hovered {
            buffer.set_style(rect, Style::default().bg(config.palette.surface0));
        }
        let initial = row.machine_label.chars().next().unwrap_or('?');
        put_text(
            buffer,
            rect.x,
            rect.y,
            rect.width,
            &format!(
                "{initial}{}",
                status_icon(row.agent.status, config.status_indicators)
            ),
            Style::default()
                .fg(if row.stale {
                    config.palette.overlay0
                } else {
                    status_color(row.agent.status, &config.palette)
                })
                .add_modifier(if row.stale {
                    Modifier::DIM
                } else {
                    Modifier::empty()
                }),
        );
        hits.endpoint_agents
            .push((rect, row.endpoint_id.clone(), row.agent.pane_id.clone()));
    }
}

pub(super) fn render_expanded(
    buffer: &mut Buffer,
    area: Rect,
    agent_view_label: Option<&str>,
    rows: &[EndpointAgentRow],
    config: &ClientShellConfig,
    agent_scroll: &mut usize,
    chrome_hover: Option<&super::feedback::ChromeHover>,
    hits: &mut ShellHitMap,
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
    super::agent_sidebar::render_agent_list(
        buffer,
        area,
        rows,
        agent_view_label.map(|_| crate::i18n::texts().sidebar.no_matching_agents),
        config,
        agent_scroll,
        matches!(
            chrome_hover,
            Some(super::feedback::ChromeHover::AgentScrollbarThumb)
        ),
        hits,
        |row| row.agent.rows.len(),
        |buffer, rect, row, hits| {
            let hovered = matches!(
                chrome_hover,
                Some(super::feedback::ChromeHover::EndpointAgentRow(endpoint_id, pane_id))
                    if endpoint_id == &row.endpoint_id && pane_id == &row.agent.pane_id
            );
            super::agent_sidebar::render_agent_row(buffer, rect, &row.agent, config, hovered, None);
            if row.stale {
                buffer.set_style(
                    rect,
                    Style::default()
                        .fg(config.palette.overlay0)
                        .add_modifier(Modifier::DIM),
                );
            }
            hits.endpoint_agents
                .push((rect, row.endpoint_id.clone(), row.agent.pane_id.clone()));
        },
    );
}

#[derive(Debug)]
pub(super) struct EndpointAgentRow {
    pub(super) endpoint_id: ClientEndpointId,
    pub(super) machine_label: String,
    pub(super) stale: bool,
    pub(super) agent: super::agent_sidebar::AgentRow,
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

/// 联邦 agents 面板的行：视图计算阶段按 `AgentRowsKey` 维护，渲染只读。
pub(super) struct AgentRowsCache {
    key: AgentRowsKey,
    rows: Vec<EndpointAgentRow>,
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

    pub(super) fn rows(&self) -> &[EndpointAgentRow] {
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
    ) -> Self {
        Self {
            rows: agent_rows(endpoints, active, config),
            key,
        }
    }
}

impl ClientShellState {
    /// 视图计算阶段刷新联邦 agents 行缓存（PERF-02）：键未变则复用上一帧的行。
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
            ));
        }
    }
}

fn agent_rows(
    endpoints: &[ClientShellEndpoint],
    active_endpoint_id: &ClientEndpointId,
    config: &ClientShellConfig,
) -> Vec<EndpointAgentRow> {
    let mut rendered_rows = endpoints
        .iter()
        .filter_map(|endpoint| {
            endpoint.snapshot.as_deref().map(|snapshot| {
                snapshot
                    .agents
                    .iter()
                    .filter_map(|agent| {
                        super::agent_sidebar::agent_row(
                            snapshot,
                            &agent.pane_id,
                            config,
                            Some(&endpoint.label),
                        )
                    })
                    .map(|agent| ((endpoint.endpoint_id.clone(), agent.pane_id.clone()), agent))
                    .collect::<Vec<_>>()
            })
        })
        .flatten()
        .collect::<HashMap<_, _>>();

    super::aggregate_navigation::aggregate_agent_rows(
        endpoints,
        active_endpoint_id,
        config.agent_panel_sort,
    )
    .into_iter()
    .filter_map(|row| {
        let key = (row.endpoint.endpoint_id.clone(), row.agent.pane_id.clone());
        let mut agent = rendered_rows.remove(&key)?;
        agent.focused &= row.endpoint.endpoint_id == active_endpoint_id;
        Some(EndpointAgentRow {
            endpoint_id: row.endpoint.endpoint_id.clone(),
            machine_label: row.endpoint.label.to_owned(),
            stale: row.endpoint.stale(),
            agent,
        })
    })
    .collect()
}
