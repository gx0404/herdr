use std::collections::HashMap;

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    text::Line,
    widgets::{Paragraph, Widget},
};

use super::*;

pub(super) struct AgentRow {
    pub(super) pane_id: String,
    pub(super) status: crate::api::schema::AgentStatus,
    pub(super) focused: bool,
    pub(super) rows: Vec<Vec<crate::ui::ResolvedToken>>,
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
    if sort == crate::config::AgentPanelSortConfig::Priority {
        agents.sort_by_key(|agent| {
            (
                std::cmp::Reverse(status_priority(agent.agent_status)),
                std::cmp::Reverse(agent.state_change_seq),
            )
        });
    }
    agents
        .into_iter()
        .map(|agent| agent.pane_id.clone())
        .collect()
}

/// One rendered line of the agents panel: a collapsible workspace header or
/// an agent row nested under its workspace.
pub(super) enum AgentPanelEntry {
    Workspace {
        key: String,
        label: String,
        agent_count: usize,
        status: crate::api::schema::AgentStatus,
        collapsed: bool,
    },
    Agent {
        row: AgentRow,
        /// `Some(last_child)` in the grouped view for tree prefixes.
        tree: Option<bool>,
    },
}

pub(super) fn agent_group_key(workspace_id: &str) -> String {
    format!("agent-panel:{workspace_id}")
}

fn workspace_agent_pane_ids(
    snapshot: &ClientShellSnapshot,
    workspace_id: &str,
    sort: crate::config::AgentPanelSortConfig,
) -> Vec<String> {
    let agents = snapshot
        .agents
        .iter()
        .filter(|agent| agent.workspace_id == workspace_id)
        .collect::<Vec<_>>();
    match sort {
        // Server already groups snapshot.agents by workspace; keep that
        // order instead of agent_order, which only drives the filtered view.
        crate::config::AgentPanelSortConfig::Spaces => agents
            .into_iter()
            .map(|agent| agent.pane_id.clone())
            .collect(),
        crate::config::AgentPanelSortConfig::Priority => {
            let mut sorted = agents;
            sorted.sort_by_key(|agent| {
                (
                    std::cmp::Reverse(status_priority(agent.agent_status)),
                    std::cmp::Reverse(agent.state_change_seq),
                )
            });
            sorted
                .into_iter()
                .map(|agent| agent.pane_id.clone())
                .collect()
        }
    }
}

/// Workspace-grouped entries for the unfiltered view; a status-filtered
/// view (`agent_view_label`) stays flat because the grouping dimension is
/// already the filter's purpose.
pub(super) fn agent_panel_entries(
    snapshot: &ClientShellSnapshot,
    config: &ClientShellConfig,
    collapsed_groups: &std::collections::HashSet<String>,
) -> Vec<AgentPanelEntry> {
    if snapshot.agent_view_label.is_some() {
        return agent_rows(snapshot, config, None)
            .into_iter()
            .map(|row| AgentPanelEntry::Agent { row, tree: None })
            .collect();
    }
    let mut entries = Vec::new();
    for workspace in &snapshot.workspaces {
        let rows: Vec<AgentRow> = workspace_agent_pane_ids(
            snapshot,
            workspace.workspace_id.as_str(),
            config.agent_panel_sort,
        )
        .into_iter()
        .filter_map(|pane_id| {
            let mut row = agent_row(snapshot, &pane_id, config, None)?;
            // The workspace header already carries the workspace identity;
            // drop the now-redundant token so children stay readable.
            for line in &mut row.rows {
                line.retain(|token| {
                    !matches!(token.kind, crate::ui::ResolvedTokenKind::Workspace(_))
                });
            }
            row.rows.retain(|line| !line.is_empty());
            Some(row)
        })
        .collect();
        if rows.is_empty() {
            continue;
        }
        let key = agent_group_key(&workspace.workspace_id);
        let collapsed = collapsed_groups.contains(&key);
        entries.push(AgentPanelEntry::Workspace {
            key,
            label: workspace.label.clone(),
            agent_count: rows.len(),
            status: rows
                .iter()
                .map(|row| row.status)
                .max_by_key(|status| status_priority(*status))
                .unwrap_or(crate::api::schema::AgentStatus::Idle),
            collapsed,
        });
        if collapsed {
            continue;
        }
        let last = rows.len().saturating_sub(1);
        for (index, row) in rows.into_iter().enumerate() {
            entries.push(AgentPanelEntry::Agent {
                row,
                tree: Some(index == last),
            });
        }
    }
    entries
}

pub(super) fn render_agent_panel(
    buffer: &mut Buffer,
    area: Rect,
    snapshot: &ClientShellSnapshot,
    config: &ClientShellConfig,
    collapsed_groups: &std::collections::HashSet<String>,
    agent_scroll: &mut usize,
    chrome_hover: Option<&super::feedback::ChromeHover>,
    hits: &mut ShellHitMap,
) {
    if !render_agent_panel_header(
        buffer,
        area,
        snapshot.agent_view_label.as_deref(),
        config,
        chrome_hover,
        hits,
    ) {
        return;
    }

    let mut entries = agent_panel_entries(snapshot, config, collapsed_groups);
    // Degraded mode for very short panels: without room for a workspace
    // header plus at least one agent row, fall back to the flat list so
    // agents stay visible and clickable.
    if area.height.saturating_sub(3) < 3 {
        entries = agent_rows(snapshot, config, None)
            .into_iter()
            .map(|row| AgentPanelEntry::Agent { row, tree: None })
            .collect();
    }
    render_agent_list(
        buffer,
        area,
        &entries,
        snapshot
            .agent_view_label
            .as_ref()
            .map(|_| crate::i18n::texts().sidebar.no_matching_agents),
        config,
        agent_scroll,
        matches!(
            chrome_hover,
            Some(super::feedback::ChromeHover::AgentScrollbarThumb)
        ),
        hits,
        |entry| match entry {
            AgentPanelEntry::Workspace { .. } => 1,
            AgentPanelEntry::Agent { row, .. } => row.rows.len(),
        },
        |buffer, rect, entry, hits| match entry {
            AgentPanelEntry::Workspace {
                key,
                label,
                agent_count,
                status,
                collapsed,
            } => {
                let hovered = matches!(
                    chrome_hover,
                    Some(super::feedback::ChromeHover::AgentGroupRow(id)) if id == key
                );
                render_agent_group_header(
                    buffer,
                    rect,
                    label,
                    *agent_count,
                    *status,
                    *collapsed,
                    config,
                    hovered,
                );
                hits.agent_group_toggles.push((rect, key.clone()));
            }
            AgentPanelEntry::Agent { row, tree } => {
                hits.agents.push((rect, row.pane_id.clone()));
                let hovered = matches!(
                    chrome_hover,
                    Some(super::feedback::ChromeHover::AgentRow(id)) if id == &row.pane_id
                );
                render_agent_row(buffer, rect, row, config, hovered, *tree);
            }
        },
    );
}

fn render_agent_group_header(
    buffer: &mut Buffer,
    rect: Rect,
    label: &str,
    agent_count: usize,
    status: crate::api::schema::AgentStatus,
    collapsed: bool,
    config: &ClientShellConfig,
    hovered: bool,
) {
    let palette = &config.palette;
    if hovered {
        buffer.set_style(rect, Style::default().bg(palette.surface0));
    }
    put_text(
        buffer,
        rect.x + 1,
        rect.y,
        1,
        status_icon(status, config.status_indicators),
        Style::default().fg(status_color(status, palette)),
    );
    let count = format!("· {agent_count}");
    let chevron_x = rect.right().saturating_sub(1);
    let count_width = display_width(&count).min(rect.width as usize) as u16;
    let count_x = chevron_x.saturating_sub(count_width + 1);
    let label_width = count_x.saturating_sub(rect.x + 3);
    put_text(
        buffer,
        rect.x + 3,
        rect.y,
        label_width,
        label,
        Style::default()
            .fg(palette.text)
            .add_modifier(Modifier::BOLD),
    );
    put_text(
        buffer,
        count_x,
        rect.y,
        count_width,
        &count,
        Style::default().fg(palette.overlay0),
    );
    put_text(
        buffer,
        chevron_x,
        rect.y,
        1,
        if collapsed { "▸" } else { "▾" },
        Style::default().fg(palette.accent),
    );
}

/// 头部放不下「用量」整词时的 1 列入口。必须是 East Asian Width 为 Narrow 的
/// 字形：Ambiguous 字符（如 `◱`）在 CJK 终端里常按 2 列渲染，会压掉与排序标签
/// 之间的间距、命中区也与视觉宽度不符。
pub(super) const USAGE_ICON: &str = "%";

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
    put_text(
        buffer,
        area.x,
        area.y + 1,
        area.width,
        crate::i18n::texts().sidebar.agents,
        Style::default()
            .fg(config.palette.overlay0)
            .add_modifier(Modifier::BOLD),
    );
    let texts = crate::i18n::texts();
    let sort_label = agent_view_label.unwrap_or(match config.agent_panel_sort {
        crate::config::AgentPanelSortConfig::Spaces => texts.sidebar.sort_grouped,
        crate::config::AgentPanelSortConfig::Priority => texts.sidebar.sort_priority,
    });
    // 宽度预算先保证用量入口：「用量」按钮放在排序标签左侧；放不下整词就缩成
    // 1 列图标，仍放不下则截断排序标签。这样中文默认宽度（内容 25 列）与
    // 两种排序标签下按钮都在，不会随排序模式闪烁（UD-07）。
    let agents_width = display_width(texts.sidebar.agents) as u16;
    // 按钮最左可起点：Agents 标题右侧留 2 列间距。
    let min_usage_x = area.x.saturating_add(agents_width).saturating_add(3);
    let sort_full = display_width(sort_label).min(area.width as usize) as u16;
    let usage_full = display_width(texts.sidebar.agent_usage) as u16;
    // 图标宽度与整词同一口径（`display_width`），预算与实际绘制不会静默错位。
    let icon_width = display_width(USAGE_ICON) as u16;
    // 排序标签右对齐，按钮在其左侧留 1 列间距；`fits` 判断按钮起点是否不早于
    // `min_usage_x`。
    let fits = |usage_width: u16, sort_width: u16| {
        area.right()
            .checked_sub(sort_width.saturating_add(usage_width).saturating_add(1))
            .is_some_and(|usage_x| usage_x >= min_usage_x)
    };
    let (usage_label, usage_width, sort_width) = if fits(usage_full, sort_full) {
        (texts.sidebar.agent_usage, usage_full, sort_full)
    } else if fits(icon_width, sort_full) {
        (USAGE_ICON, icon_width, sort_full)
    } else {
        // 截断排序标签：给图标留 `icon_width` 列 + 1 列间距；排序标签被挤到 0 列
        // 时整个放弃它，图标直接靠右对齐，命中区仍保住。因此只要内容宽
        // ≥ `agents_width + 3 + icon_width` 按钮就存在。
        let sort_width = area
            .right()
            .saturating_sub(min_usage_x.saturating_add(icon_width).saturating_add(1))
            .min(sort_full);
        (USAGE_ICON, icon_width, sort_width)
    };
    let sort_rect = Rect::new(
        area.right().saturating_sub(sort_width),
        area.y + 1,
        sort_width,
        1,
    );
    // 排序标签存在时按钮与其留 1 列间距；标签被放弃时按钮直接靠右。
    let usage_gap = u16::from(sort_width > 0);
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
    let usage_rect = Rect::new(
        sort_rect
            .x
            .saturating_sub(usage_width.saturating_add(usage_gap)),
        area.y + 1,
        usage_width,
        1,
    );
    let usage_fits = usage_rect.x >= min_usage_x;
    // `!mouse_capture` 下按钮只绘制不产生命中区；键位 / which-key 兜底（U-6）
    // 尚未落地，届时总览由 `toggle_usage_dashboard` 的默认键位承接。
    hits.agent_usage_toggle = if config.mouse_capture && usage_fits {
        usage_rect
    } else {
        Rect::default()
    };
    if usage_fits {
        let usage_hovered = matches!(
            chrome_hover,
            Some(super::feedback::ChromeHover::AgentUsageToggle)
        );
        put_text(
            buffer,
            usage_rect.x,
            usage_rect.y,
            usage_rect.width,
            usage_label,
            Style::default()
                .fg(if usage_hovered {
                    config.palette.text
                } else {
                    config.palette.overlay0
                })
                .add_modifier(Modifier::BOLD),
        );
    }
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
        let track = Rect::new(body.right().saturating_sub(1), body.y, 1, body.height);
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

pub(super) fn agent_rows(
    snapshot: &ClientShellSnapshot,
    config: &ClientShellConfig,
    machine: Option<&str>,
) -> Vec<AgentRow> {
    ordered_agent_pane_ids(snapshot, config.agent_panel_sort)
        .into_iter()
        .filter_map(|pane_id| agent_row(snapshot, &pane_id, config, machine))
        .collect()
}

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
    })
}

pub(super) fn render_agent_row(
    buffer: &mut Buffer,
    rect: Rect,
    row: &AgentRow,
    config: &ClientShellConfig,
    hovered: bool,
    tree_last_child: Option<bool>,
) {
    let palette = &config.palette;
    let row_style = if row.focused {
        Style::default().bg(palette.active_row_bg)
    } else if hovered {
        Style::default().bg(palette.surface0)
    } else {
        Style::default()
    };
    let name_style = if row.focused {
        Style::default()
            .fg(palette.text)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(palette.subtext0)
            .add_modifier(Modifier::BOLD)
    };
    let status_style = Style::default().fg(status_color(row.status, palette));
    let secondary = Style::default().fg(palette.overlay0);
    let icon = (
        status_icon(row.status, config.status_indicators),
        Style::default().fg(status_color(row.status, palette)),
    );
    let rows = if row.rows.is_empty() {
        vec![vec![crate::ui::ResolvedToken {
            kind: crate::ui::ResolvedTokenKind::StateIcon,
            style: Default::default(),
        }]]
    } else {
        row.rows.clone()
    };
    for (index, tokens) in rows.iter().take(rect.height as usize).enumerate() {
        let (indent, prefix) = match tree_last_child {
            Some(last_child) => (
                0,
                if index == 0 {
                    if last_child {
                        "   └─ "
                    } else {
                        "   ├─ "
                    }
                } else if last_child {
                    "        "
                } else {
                    "   │    "
                },
            ),
            None => (if index == 0 { 1 } else { 3 }, ""),
        };
        let mut spans = vec![ratatui::text::Span::raw(" ".repeat(indent))];
        if !prefix.is_empty() {
            spans.push(ratatui::text::Span::styled(
                prefix,
                Style::default().fg(palette.overlay0),
            ));
        }
        spans.extend(crate::ui::resolved_token_spans(
            tokens,
            icon,
            status_style,
            name_style,
            secondary,
            secondary,
            palette,
            rect.width
                .saturating_sub((indent + display_width(prefix)) as u16) as usize,
        ));
        Paragraph::new(Line::from(spans)).style(row_style).render(
            Rect::new(rect.x, rect.y + index as u16, rect.width, 1),
            buffer,
        );
    }
}

fn put_text(buffer: &mut Buffer, x: u16, y: u16, width: u16, text: &str, style: Style) {
    // set_stringn truncates by display width and writes spacer cells after
    // double-width graphemes; a per-cell char loop would corrupt CJK text.
    buffer.set_stringn(x, y, text, width as usize, style);
}

fn display_width(text: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(text)
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
