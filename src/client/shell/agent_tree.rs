//! Agents 面板统一树的领域层：机器 → 工作区 → tab → agent → 活动节点，外加
//! 「外部」分组。原语（`TreeEntry` 与前缀字形）在 `crate::ui::kit::tree`，这里
//! 只放认识客户端类型的部分。
//!
//! seam-stub(agent-panel)：接缝只落状态持有者与热文件分派点调用的入口（树节点
//! 点击、agent 行右键菜单及其动作）；`AgentTreeKind` / `AgentTreeRow` /
//! `build_agent_tree`（视图计算阶段调用，结果进缓存）由波 2 面板车道在本文件内
//! 声明。折叠键命名空间：既有 `agent-panel:<ws>`，新增 `agent-tab:<tab>`、
//! `agent-activity:<pane>`、`agent-node:<pane>:<node>`、`agent-external:<source>`，
//! 继续存 `collapsed_groups` / `remote_collapsed_groups`。

use super::agent_activity_overlay::AgentActivityOwner;
use super::*;

/// 面板车道的私有状态持有者：`ClientShellState` 不再为它加字段，新状态放这里。
#[derive(Default)]
pub(super) struct AgentTreeState {}

impl ClientShellState {
    /// 左键落在树节点的折叠开关上：切换该折叠键。开关矩形落在行矩形之内，
    /// 调用方必须先于行点击调用本函数。
    pub(super) fn handle_agent_tree_click(
        &mut self,
        point: (u16, u16),
        outcome: &mut ClientShellInput,
    ) -> bool {
        let Some((endpoint_id, key)) = self
            .hits
            .agent_tree_toggles
            .iter()
            .find(|(rect, _, _)| super::contains(*rect, point))
            .map(|(_, endpoint_id, key)| (endpoint_id.clone(), key.clone()))
        else {
            return false;
        };
        self.toggle_collapsed_group(&endpoint_id, key);
        self.agent_scroll = 0;
        outcome.repaint = true;
        self.persist_chrome_preferences(outcome);
        true
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
        let Some(agent) = self
            .endpoints
            .iter()
            .find(|endpoint| endpoint.endpoint_id == endpoint_id)
            .and_then(|endpoint| endpoint.snapshot.as_deref())
            .and_then(|snapshot| {
                snapshot
                    .agents
                    .iter()
                    .find(|agent| agent.pane_id == pane_id)
            })
        else {
            return false;
        };
        let target = ClientContextMenuTarget::Agent {
            endpoint_id,
            pane_id,
            workspace_id: agent.workspace_id.clone(),
            agent: agent.agent.clone(),
            has_manual_name: agent.name.is_some(),
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

    /// agent 行 / 外部条目右键菜单的动作。条目由接缝在 `context_menu.rs` 定稿；
    /// 重命名、用量、绑定账号、关闭由波 2 面板车道在这里接上，接上时同步把
    /// `context_menu.rs` 里对应条目的 `enabled` 从 false 翻回 true。
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
            (Action::ViewAgentActivity, owner) => self.open_agent_activity(endpoint_id, owner),
            // seam-stub(agent-panel)：其余动作的条目在菜单里已灰显、不可激活，
            // 这里兜住键盘/程序化路径。
            _ => {}
        }
    }
}
