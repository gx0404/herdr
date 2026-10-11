use std::collections::HashMap;

use super::*;

pub(super) fn ordered_agent_pane_ids(
    snapshot: &ClientShellSnapshot,
    sort: crate::config::AgentPanelSortConfig,
) -> Vec<String> {
    if snapshot.agent_view_label.is_some() {
        return snapshot
            .agent_order
            .iter()
            .filter(|pane_id| {
                snapshot
                    .agents
                    .iter()
                    .any(|agent| agent.pane_id == pane_id.as_str())
            })
            .cloned()
            .collect();
    }
    let mut agents = snapshot.agents.iter().collect::<Vec<_>>();
    if sort == crate::config::AgentPanelSortConfig::Launch {
        agents.sort_by_key(|agent| launch_order_key(agent));
    }
    agents
        .into_iter()
        .map(|agent| agent.pane_id.clone())
        .collect()
}

/// 「按启动顺序」的稳定排序键：`launch_seq` 升序，未知（0，旧 server 不下发）
/// 排在最后；全为 0 时稳定排序保持快照顺序。
pub(super) fn launch_order_key(agent: &crate::protocol::ClientShellAgent) -> (bool, u64) {
    (agent.launch_seq == 0, agent.launch_seq)
}

/// 工作区分组头的折叠键（`agent-panel:` 命名空间，存 `collapsed_groups`）。
pub(super) fn agent_group_key(workspace_id: &str) -> String {
    format!("{AGENT_GROUP_PREFIX}{workspace_id}")
}

const AGENT_GROUP_PREFIX: &str = "agent-panel:";

/// [`agent_group_key`] 的反解：工作区分组头的折叠键 → 工作区 id；其它键为 `None`。
pub(super) fn group_key_workspace(key: &str) -> Option<&str> {
    key.strip_prefix(AGENT_GROUP_PREFIX)
}

/// 按侧栏行配置解析一个 agent 的 token 行。`machine` 有值时行里带机器 token
/// （联邦平铺视图）；树视图里机器 / 工作区 / 标签页由分组头承载，调用方按需
/// 丢掉对应 token。
pub(super) fn agent_row(
    snapshot: &ClientShellSnapshot,
    pane_id: &str,
    config: &ClientShellConfig,
    machine: Option<&str>,
) -> Option<Vec<Vec<crate::ui::ResolvedToken>>> {
    let agent = snapshot
        .agents
        .iter()
        .find(|agent| agent.pane_id == pane_id)?;
    let workspace = snapshot
        .workspaces
        .iter()
        .find(|workspace| workspace.workspace_id == agent.workspace_id)?;
    let tab = snapshot.tabs.iter().find(|tab| tab.tab_id == agent.tab_id);
    let pane = snapshot
        .panes
        .iter()
        .find(|pane| pane.pane_id == agent.pane_id);
    let tab_count = snapshot
        .tabs
        .iter()
        .filter(|candidate| candidate.workspace_id == agent.workspace_id)
        .count();
    let tab_label = tab
        .filter(|tab| tab_count > 1 || tab.custom_label)
        .map(|tab| tab.label.as_str());
    let agent_label = agent
        .display_agent
        .as_deref()
        .or(agent.name.as_deref())
        .or(agent.agent.as_deref())
        .or(agent.title.as_deref());
    let labels = agent
        .state_labels
        .iter()
        .cloned()
        .collect::<HashMap<_, _>>();
    let tokens = agent.tokens.iter().cloned().collect::<HashMap<_, _>>();
    let state_text = labels
        .get(status_text(agent.agent_status))
        .map(String::as_str)
        .unwrap_or_else(|| sidebar_status_text(agent.agent_status));
    let canonical_agent = agent
        .agent
        .as_deref()
        .and_then(crate::detect::parse_agent_label);
    let rows = crate::ui::sidebar_agent_rows(
        &config.agents,
        crate::ui::AgentTokenContext {
            machine,
            workspace: &workspace.label,
            tab: tab_label,
            pane: agent
                .title
                .as_deref()
                .or_else(|| pane.and_then(|pane| pane.label.as_deref())),
            agent_label,
            terminal_title: agent.terminal_title.as_deref(),
            terminal_title_stripped: agent.terminal_title_stripped.as_deref(),
            canonical_agent,
            tokens: &tokens,
        },
        state_text,
    );
    Some(rows)
}

fn sidebar_status_text(status: crate::api::schema::AgentStatus) -> &'static str {
    use crate::api::schema::AgentStatus;
    let texts = &crate::i18n::texts().status;
    match status {
        AgentStatus::Blocked => texts.blocked,
        AgentStatus::Done => texts.done,
        AgentStatus::Working => texts.working,
        AgentStatus::Idle | AgentStatus::Unknown => texts.idle,
    }
}
