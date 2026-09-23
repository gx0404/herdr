use std::collections::HashMap;

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
};

use super::*;
// 显式导入优先于上面的 glob：本文件按 `usize` 算宽度。
use crate::ui::display_width;

/// 一个 agent 行的行数据：按侧栏行配置解析出的 token 行，由统一树
/// （`agent_tree.rs`）在视图计算阶段构建、渲染阶段画出。
#[derive(Debug)]
pub(super) struct AgentRow {
    pub(super) pane_id: String,
    pub(super) status: crate::api::schema::AgentStatus,
    pub(super) focused: bool,
    pub(super) rows: Vec<Vec<crate::ui::ResolvedToken>>,
    /// 状态文案（服务端给的自定义状态标签优先）。行配置里没有 `state_text`
    /// token 时，统一树把它当次要信息画在名称后面，放不下先丢它。
    pub(super) state_text: String,
}

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

/// classic 单端点侧栏的 Agents 面板：行来自视图计算阶段的统一树缓存
/// （`AgentRowsCache`），与联邦 / workbench 同一套渲染，agent 行写进 `hits.agents`
/// （端点隐含为本机）。
#[allow(clippy::too_many_arguments)]
pub(super) fn render_agent_panel(
    buffer: &mut Buffer,
    area: Rect,
    agent_view_label: Option<&str>,
    rows: super::agent_tree::AgentRowsView<'_>,
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
        false,
    );
}

pub(super) fn render_agent_panel_header(
    buffer: &mut Buffer,
    area: Rect,
    agent_view_label: Option<&str>,
    config: &ClientShellConfig,
    chrome_hover: Option<&super::feedback::ChromeHover>,
    hits: &mut ShellHitMap,
) -> bool {
    if area.height == 0 {
        return false;
    }
    let section_divider_hovered = matches!(
        chrome_hover,
        Some(super::feedback::ChromeHover::SidebarSectionDivider)
    );
    put_text(
        buffer,
        area.x,
        area.y,
        area.width,
        &"─".repeat(area.width as usize),
        Style::default().fg(if section_divider_hovered {
            config.palette.overlay1
        } else {
            config.palette.surface_dim
        }),
    );
    if area.height < 2 {
        return false;
    }
    let texts = crate::i18n::texts();
    put_text(
        buffer,
        area.x,
        area.y + 1,
        area.width,
        texts.sidebar.agents,
        Style::default()
            .fg(config.palette.overlay0)
            .add_modifier(Modifier::BOLD),
    );
    let sort_label = agent_view_label.unwrap_or(match config.agent_panel_sort {
        crate::config::AgentPanelSortConfig::Spaces => texts.sidebar.sort_grouped,
        crate::config::AgentPanelSortConfig::Launch => texts.agent_panel.sort_launch,
    });
    // 宽度仲裁（冒烟 M11）：排序标签右对齐，最左只能从标题右侧留 2 列间距处
    // 开始，不再与标题粘连；放不下整个标签就截短带省略号，连「字 + …」都放不下
    // 时整个让出（命中区一并清空）。沿用拆掉「用量」按钮前 `min_usage_x` 的口径。
    let min_sort_x = area
        .x
        .saturating_add(crate::ui::display_width_u16(texts.sidebar.agents))
        .saturating_add(2);
    let room = usize::from(area.right().saturating_sub(min_sort_x));
    let fitted = if display_width(sort_label) <= room {
        std::borrow::Cow::Borrowed(sort_label)
    } else if room >= 3 {
        std::borrow::Cow::Owned(crate::ui::truncate_end(sort_label, room))
    } else {
        std::borrow::Cow::Borrowed("")
    };
    let sort_label = fitted.as_ref();
    let sort_width = display_width(sort_label).min(area.width as usize) as u16;
    let sort_rect = if sort_width == 0 {
        Rect::default()
    } else {
        Rect::new(
            area.right().saturating_sub(sort_width),
            area.y + 1,
            sort_width,
            1,
        )
    };
    hits.agent_sort_toggle = if config.mouse_capture && agent_view_label.is_none() {
        sort_rect
    } else {
        Rect::default()
    };
    let sort_hovered = matches!(
        chrome_hover,
        Some(super::feedback::ChromeHover::AgentSortToggle)
    );
    put_text(
        buffer,
        sort_rect.x,
        sort_rect.y,
        sort_rect.width,
        sort_label,
        Style::default()
            .fg(if agent_view_label.is_some() {
                config.palette.accent
            } else if sort_hovered {
                config.palette.text
            } else {
                config.palette.overlay0
            })
            .add_modifier(Modifier::BOLD),
    );
    true
}

#[allow(clippy::too_many_arguments)]
pub(super) fn render_agent_list<T>(
    buffer: &mut Buffer,
    area: Rect,
    rows: &[T],
    empty_message: Option<&str>,
    config: &ClientShellConfig,
    agent_scroll: &mut usize,
    thumb_hovered: bool,
    hits: &mut ShellHitMap,
    row_lines: impl Fn(&T) -> usize,
    mut render_row: impl FnMut(&mut Buffer, Rect, &T, &mut ShellHitMap),
) {
    let body = Rect::new(
        area.x,
        area.y.saturating_add(3),
        area.width,
        area.height.saturating_sub(3),
    );
    hits.agent_body = body;
    if body.is_empty() || rows.is_empty() {
        *agent_scroll = 0;
        if let Some(message) = empty_message.filter(|_| !body.is_empty()) {
            put_text(
                buffer,
                body.x,
                body.y,
                body.width,
                message,
                Style::default()
                    .fg(config.palette.overlay0)
                    .add_modifier(Modifier::DIM),
            );
        }
        return;
    }

    let row_heights = rows
        .iter()
        .map(|row| row_lines(row).max(1).min(u16::MAX as usize) as u16)
        .collect::<Vec<_>>();
    let gaps = rows
        .iter()
        .enumerate()
        .map(|(index, _)| {
            if index + 1 < rows.len() {
                config.agents.row_gap
            } else {
                0
            }
        })
        .collect::<Vec<_>>();
    let metrics =
        super::scroll::list_scroll_metrics(&row_heights, &gaps, body.height, *agent_scroll);
    hits.agent_max_scroll = metrics.max_offset_from_bottom;
    hits.agent_scroll_metrics = Some(metrics);
    *agent_scroll = metrics
        .max_offset_from_bottom
        .saturating_sub(metrics.offset_from_bottom);
    let show_scrollbar = metrics.max_offset_from_bottom > 0 && body.width > 1;
    let content_width = body.width.saturating_sub(u16::from(show_scrollbar));
    let mut y = body.y;
    for (index, row) in rows.iter().enumerate().skip(*agent_scroll) {
        let height = row_heights[index].min(body.height);
        if y.saturating_add(height) > body.bottom() {
            break;
        }
        let rect = Rect::new(body.x, y, content_width, height);
        render_row(buffer, rect, row, hits);
        y = y
            .saturating_add(height)
            .saturating_add(if index + 1 < rows.len() {
                config.agents.row_gap
            } else {
                0
            });
    }

    if show_scrollbar {
        // 轨道让出底格：侧栏折叠开关 « 画在同一列的最后一行，几何重叠时
        // `agent_scrollbar` 在鼠标分派里排在 `sidebar_toggle` 之前且无条件
        // return，点 « 会变成「列表跳到底」，绘制上也会盖掉 «。
        let track = Rect::new(
            body.right().saturating_sub(1),
            body.y,
            1,
            body.height.saturating_sub(1),
        );
        hits.agent_scrollbar = track;
        super::scroll::render_list_scrollbar(
            buffer,
            track,
            metrics,
            &config.palette,
            thumb_hovered,
        );
    }
}

/// 按侧栏行配置解析一个 agent 的 token 行。`machine` 有值时行里带机器 token
/// （联邦平铺视图）；树视图里机器 / 工作区 / 标签页由分组头承载，调用方按需
/// 丢掉对应 token。
pub(super) fn agent_row(
    snapshot: &ClientShellSnapshot,
    pane_id: &str,
    config: &ClientShellConfig,
    machine: Option<&str>,
) -> Option<AgentRow> {
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
    Some(AgentRow {
        pane_id: agent.pane_id.clone(),
        status: agent.agent_status,
        focused: agent.focused,
        rows,
        state_text: state_text.to_owned(),
    })
}

fn put_text(buffer: &mut Buffer, x: u16, y: u16, width: u16, text: &str, style: Style) {
    // set_stringn truncates by display width and writes spacer cells after
    // double-width graphemes; a per-cell char loop would corrupt CJK text.
    buffer.set_stringn(x, y, text, width as usize, style);
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
