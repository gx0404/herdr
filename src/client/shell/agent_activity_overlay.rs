//! 「Agent 活动」二级窗口：左列活动树、右列所选节点的内容。只读窗口，数据来自
//! 快照里的活动树摘要与 `agent.activity.read`。
//!
//! seam-stub(activity-window)：接缝只落类型、打开入口与最小的渲染 / 键盘 / 鼠标
//! 桩（空框、Esc 返回、点外关闭、丢弃过期响应）；波 3 二级窗口车道在本文件内
//! 实现，热文件里的分派点不需要再改。

use super::render::{put_text, titled_panel, OverlayRender};
use super::*;
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

/// 活动树的属主：某个 pane 里的 agent，或不属于任何 pane 的外部来源条目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum AgentActivityOwner {
    Pane { pane_id: String },
    External { external_id: String },
}

// seam-stub(activity-window)：按钮由波 3 二级窗口车道画出并派发后删除本 allow。
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AgentActivityButton {
    Close,
    ToggleFollow,
    Refresh,
}

// seam-stub(activity-window)：多数字段要等波 3 二级窗口车道读写后才活，届时删除
// 本 allow。
#[allow(dead_code)]
#[derive(Debug)]
pub(super) struct ClientAgentActivityOverlay {
    pub(super) endpoint_id: ClientEndpointId,
    pub(super) owner: AgentActivityOwner,
    /// 键盘选中的节点：只由键盘与点击改写。
    pub(super) selected_node: Option<String>,
    /// 指针悬浮的节点：只由 `Moved` 改写（MENU-01）。
    pub(super) hovered_node: Option<String>,
    pub(super) collapsed: HashSet<String>,
    pub(super) tree_scroll: usize,
    pub(super) content_scroll: usize,
    /// 视图计算阶段写入，渲染只读（STATE-04）。
    pub(super) content_max_scroll: usize,
    pub(super) follow: bool,
    /// 在途读取的代际：响应的代际对不上即丢弃。
    pub(super) read_epoch: u64,
    pub(super) error: Option<String>,
    pub(super) content: Option<crate::api::schema::AgentActivityContent>,
    /// 从别的浮层（如右键菜单之外的入口）打开时，Esc 回到它。
    pub(super) return_to: Option<Box<ClientShellOverlay>>,
}

impl ClientAgentActivityOverlay {
    fn new(endpoint_id: ClientEndpointId, owner: AgentActivityOwner) -> Self {
        Self {
            endpoint_id,
            owner,
            selected_node: None,
            hovered_node: None,
            collapsed: HashSet::new(),
            tree_scroll: 0,
            content_scroll: 0,
            content_max_scroll: 0,
            follow: true,
            read_epoch: 0,
            error: None,
            content: None,
            return_to: None,
        }
    }
}

/// 属主的显示名：pane 型取该端点快照里 agent 的名字，外部条目取其标签；找不到
/// 时回退到 id。
fn owner_label(overlay: &ClientAgentActivityOverlay, endpoints: &[ClientShellEndpoint]) -> String {
    let snapshot = endpoints
        .iter()
        .find(|endpoint| endpoint.endpoint_id == overlay.endpoint_id)
        .and_then(|endpoint| endpoint.snapshot.as_deref());
    match &overlay.owner {
        AgentActivityOwner::Pane { pane_id } => snapshot
            .and_then(|snapshot| {
                snapshot
                    .agents
                    .iter()
                    .find(|agent| &agent.pane_id == pane_id)
            })
            .and_then(|agent| {
                agent
                    .name
                    .clone()
                    .or_else(|| agent.display_agent.clone())
                    .or_else(|| agent.agent.clone())
            })
            .unwrap_or_else(|| pane_id.clone()),
        AgentActivityOwner::External { external_id } => snapshot
            .and_then(|snapshot| {
                snapshot
                    .external_agents
                    .iter()
                    .find(|agent| &agent.external_id == external_id)
            })
            .map(|agent| agent.label.clone())
            .filter(|label| !label.is_empty())
            .unwrap_or_else(|| external_id.clone()),
    }
}

/// 渲染桩：带标题的空框。渲染期不写任何滚动状态
/// （`test_overlay_renderers_never_write_scroll_state`）。
pub(super) fn render_agent_activity_overlay(
    b: &mut Buffer,
    overlay: &ClientAgentActivityOverlay,
    endpoints: &[ClientShellEndpoint],
    cx: &super::feedback::ChromeContext<'_>,
) -> Option<OverlayRender> {
    let texts = &crate::i18n::texts().agent_activity;
    let outer = cx
        .page_bounds
        .map(|rect| rect.intersection(b.area))
        .or_else(|| crate::ui::modal_rect(b.area, crate::ui::ModalSize::Large))?;
    let title = format!(
        " {} ",
        crate::i18n::fill(
            texts.title_fmt,
            &[("name", &owner_label(overlay, endpoints))]
        )
    );
    let inner = titled_panel(
        b,
        outer,
        &title,
        cx.palette.accent,
        cx.palette.panel_bg,
        cx.glyphs,
    )?;
    put_text(
        b,
        inner.x,
        inner.y,
        inner.width,
        overlay.error.as_deref().unwrap_or(texts.empty_tree),
        Style::default().fg(cx.palette.overlay0),
    );
    Some(OverlayRender {
        area: outer,
        agent_activity_popup: outer,
        ..OverlayRender::default()
    })
}

impl ClientShellState {
    /// 打开某个属主的「Agent 活动」窗口；已有浮层时记为返回目标。
    pub(super) fn open_agent_activity(
        &mut self,
        endpoint_id: ClientEndpointId,
        owner: AgentActivityOwner,
    ) {
        let mut overlay = ClientAgentActivityOverlay::new(endpoint_id, owner);
        overlay.return_to = self
            .overlay
            .take()
            .filter(|previous| !matches!(previous, ClientShellOverlay::ContextMenu(_)))
            .map(Box::new);
        self.overlay = Some(ClientShellOverlay::AgentActivity(overlay));
    }

    /// 键盘桩：只认 Esc（回到 `return_to`，没有就关闭）；其余按键吞掉。
    pub(super) fn route_agent_activity_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) -> bool {
        if !matches!(self.overlay, Some(ClientShellOverlay::AgentActivity(_))) {
            return false;
        }
        if key.code == KeyCode::Esc {
            self.close_agent_activity();
            outcome.repaint = true;
        }
        true
    }

    /// 鼠标桩：左键点在窗口外关闭；窗口内的事件先吞掉。
    pub(super) fn handle_agent_activity_mouse(
        &mut self,
        mouse: MouseEvent,
        point: (u16, u16),
        outcome: &mut ClientShellInput,
    ) -> bool {
        if !matches!(self.overlay, Some(ClientShellOverlay::AgentActivity(_))) {
            return false;
        }
        if mouse.kind == MouseEventKind::Down(MouseButton::Left)
            && !super::contains(self.hits.agent_activity_popup, point)
        {
            self.close_agent_activity();
            outcome.repaint = true;
        }
        true
    }

    fn close_agent_activity(&mut self) {
        self.overlay = match self.overlay.take() {
            Some(ClientShellOverlay::AgentActivity(mut overlay)) => {
                overlay.return_to.take().map(|previous| *previous)
            }
            other => other,
        };
    }

    /// `agent.activity.read` 的响应：窗口已关、代际或选中节点对不上的一律丢弃。
    pub(super) fn receive_agent_activity_read(
        &mut self,
        epoch: u64,
        node_id: Option<String>,
        result: Result<crate::api::schema::ResponseResult, ClientShellEndpointError>,
    ) -> (bool, Vec<ClientShellAction>) {
        let Some(ClientShellOverlay::AgentActivity(overlay)) = self.overlay.as_mut() else {
            return (false, Vec::new());
        };
        if overlay.read_epoch != epoch || overlay.selected_node != node_id {
            return (false, Vec::new());
        }
        match result {
            Ok(crate::api::schema::ResponseResult::AgentActivity { content, .. }) => {
                overlay.error = None;
                overlay.content = content;
            }
            Ok(_) => return (false, Vec::new()),
            Err(error) => overlay.error = Some(error.message),
        }
        (true, Vec::new())
    }
}
