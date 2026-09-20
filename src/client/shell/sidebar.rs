use super::*;
use ratatui::{
    text::Line,
    widgets::{Paragraph, Widget},
};

pub(in crate::client::shell) fn collapsed_sidebar_sections(
    area: Rect,
) -> (Rect, Option<u16>, Rect) {
    let content = Rect::new(area.x, area.y, area.width.saturating_sub(1), area.height);
    if content.is_empty() {
        return (Rect::default(), None, Rect::default());
    }
    if content.height < 7 {
        return (content, None, Rect::default());
    }
    let workspace_height = content.height.div_ceil(2);
    let divider_y = content.y + workspace_height;
    let detail_height = content.height.saturating_sub(workspace_height + 1);
    (
        Rect::new(content.x, content.y, content.width, workspace_height),
        Some(divider_y),
        Rect::new(content.x, divider_y + 1, content.width, detail_height),
    )
}

pub(crate) fn render_collapsed_sidebar(
    buffer: &mut Buffer,
    area: Rect,
    snapshot: &ClientShellSnapshot,
    config: &ClientShellConfig,
    state: &mut ShellRenderState<'_>,
    hits: &mut ShellHitMap,
) {
    let palette = &config.palette;
    let chrome_hover = state.chrome_hover;
    let selected_workspace_id = state
        .selected_workspace_id
        .map(|target| target.workspace_id.as_str());
    render_sidebar_background(
        buffer,
        area,
        palette,
        matches!(
            chrome_hover,
            Some(super::feedback::ChromeHover::SidebarDivider)
        ),
    );
    let (workspace_area, divider_y, detail_area) = collapsed_sidebar_sections(area);

    // 折叠侧栏此前直接 `take(height)`：工作区数超过可视高度后导航选中行会落到不可见
    // 区且永不可达，滚轮也因为 `hits.workspace_body` 恒为 default 而失效（C-26/SB-02）。
    // 这里与 `endpoint_sidebar::render_collapsed` 同构：行高恒 1 的滚动窗口 + 消费
    // reveal + 回写 hit 区域，沿用既有「compose 期回写 scroll」模式，不额外扩面。
    //
    // 矮侧栏（`content.height < 7`）没有 detail 区，workspace 区一路铺到 area 底格，
    // 而折叠开关 » 正画在该格上：与高侧栏 `detail_content` 让出底格的做法对齐，否则
    // 滚轮停在开关上会滚工作区列表，reveal 也能把目标行停到被开关压住的一行。
    let workspace_body = if detail_area.is_empty() {
        Rect::new(
            workspace_area.x,
            workspace_area.y,
            workspace_area.width,
            workspace_area.height.saturating_sub(1),
        )
    } else {
        workspace_area
    };
    // 两个 reveal 标志必须无条件消费。此前写成
    // `reveal_target.is_none() && take(reveal_focused_workspace)`：导航 reveal 命中的
    // 那一帧因 `&&` 短路不消费聚焦标志，它残留到下一次任意重绘（spinner tick、agent
    // 输出）才生效，把滚动位置拉回聚焦行，刚揭示出来的选中行再次滑出可视区——正是
    // C-26 要修的症状。
    let (reveal_navigation, reveal_focused) = if workspace_body.is_empty() {
        (false, false)
    } else {
        (
            std::mem::take(state.reveal_navigation_workspace),
            std::mem::take(state.reveal_focused_workspace),
        )
    };
    let mut reveal_target = None;
    if reveal_navigation {
        reveal_target = selected_workspace_id.and_then(|workspace_id| {
            snapshot
                .workspaces
                .iter()
                .position(|workspace| workspace.workspace_id == workspace_id)
        });
    }
    if reveal_target.is_none() && reveal_focused {
        reveal_target = snapshot
            .workspaces
            .iter()
            .position(|workspace| workspace.focused);
    }
    if let Some(target) = reveal_target {
        *state.workspace_scroll = super::scroll::uniform_scroll_start_to_reveal(
            snapshot.workspaces.len(),
            workspace_body.height,
            *state.workspace_scroll,
            target,
        );
    }
    let workspace_metrics = super::scroll::uniform_scroll_metrics(
        snapshot.workspaces.len(),
        workspace_body.height,
        *state.workspace_scroll,
    );
    hits.workspace_body = workspace_body;
    hits.workspace_max_scroll = workspace_metrics.max_offset_from_bottom;
    *state.workspace_scroll = workspace_metrics
        .max_offset_from_bottom
        .saturating_sub(workspace_metrics.offset_from_bottom);
    let workspace_scroll = *state.workspace_scroll;

    for (index, workspace) in snapshot
        .workspaces
        .iter()
        .enumerate()
        .skip(workspace_scroll)
        .take(workspace_body.height as usize)
    {
        let rect = Rect::new(
            workspace_body.x,
            workspace_body.y + (index - workspace_scroll) as u16,
            workspace_body.width,
            1,
        );
        let selected = selected_workspace_id == Some(workspace.workspace_id.as_str());
        let hovered = matches!(
            chrome_hover,
            Some(super::feedback::ChromeHover::WorkspaceRow {
                endpoint_id,
                workspace_id,
            }) if endpoint_id.is_local() && workspace_id == &workspace.workspace_id
        );
        let selection_background = palette.selection_row_bg();
        if selected {
            buffer.set_style(rect, Style::default().bg(selection_background));
        } else if workspace.focused {
            buffer.set_style(rect, Style::default().bg(palette.active_row_bg));
        } else if hovered {
            buffer.set_style(rect, Style::default().bg(palette.surface0));
        }
        let number_style = if selected {
            Style::default()
                .fg(palette.overlay1)
                .bg(selection_background)
        } else if workspace.focused {
            Style::default().fg(palette.text).bg(palette.active_row_bg)
        } else {
            Style::default().fg(palette.overlay0)
        };
        put_text(
            buffer,
            rect.x,
            rect.y,
            rect.width.min(2),
            &format!("{:<2}", index + 1),
            number_style,
        );
        let status = workspace.agent_status;
        put_text(
            buffer,
            rect.x.saturating_add(2),
            rect.y,
            rect.width.saturating_sub(2),
            status_icon(status, config.status_indicators),
            Style::default().fg(status_color(status, palette)),
        );
        hits.workspaces.push(WorkspaceHit {
            rect,
            endpoint_id: ClientEndpointId::Local,
            workspace_id: workspace.workspace_id.clone(),
            indented: false,
            group_toggle: None,
        });
    }

    if let Some(divider_y) = divider_y {
        put_text(
            buffer,
            workspace_area.x,
            divider_y,
            workspace_area.width,
            &"─".repeat(workspace_area.width as usize),
            Style::default().fg(palette.surface_dim),
        );
    }

    let detail_content = Rect::new(
        detail_area.x,
        detail_area.y,
        detail_area.width,
        detail_area.height.saturating_sub(1),
    );
    // agents 分区同样只做过 `take(height)`：这里复用同一套行高 1 的滚动窗口，让折叠态
    // 的 agent 列表也能被滚轮推动。
    let agent_pane_ids = super::ordered_agent_pane_ids(snapshot, config.agent_panel_sort);
    let agent_metrics = super::scroll::uniform_scroll_metrics(
        agent_pane_ids.len(),
        detail_content.height,
        *state.agent_scroll,
    );
    hits.agent_body = detail_content;
    hits.agent_max_scroll = agent_metrics.max_offset_from_bottom;
    *state.agent_scroll = agent_metrics
        .max_offset_from_bottom
        .saturating_sub(agent_metrics.offset_from_bottom);
    let agent_scroll = *state.agent_scroll;
    for (index, pane_id) in agent_pane_ids
        .iter()
        .enumerate()
        .skip(agent_scroll)
        .take(detail_content.height as usize)
    {
        let Some(agent) = snapshot
            .agents
            .iter()
            .find(|agent| &agent.pane_id == pane_id)
        else {
            continue;
        };
        let rect = Rect::new(
            detail_content.x,
            detail_content.y + (index - agent_scroll) as u16,
            detail_content.width,
            1,
        );
        let hovered = matches!(
            chrome_hover,
            Some(super::feedback::ChromeHover::AgentRow(id)) if id == pane_id
        );
        if agent.focused {
            buffer.set_style(rect, Style::default().bg(palette.active_row_bg));
        } else if hovered {
            buffer.set_style(rect, Style::default().bg(palette.surface0));
        }
        put_text(
            buffer,
            rect.x,
            rect.y,
            rect.width.min(2),
            &format!("{:<2}", index + 1),
            Style::default().fg(if agent.focused {
                palette.text
            } else {
                palette.overlay0
            }),
        );
        put_text(
            buffer,
            rect.x.saturating_add(2),
            rect.y,
            rect.width.saturating_sub(2),
            status_icon(agent.agent_status, config.status_indicators),
            Style::default().fg(status_color(agent.agent_status, palette)),
        );
        hits.agents.push((rect, pane_id.clone()));
    }
    hits.sidebar_toggle = if area.is_empty() || workspace_area.width == 0 {
        Rect::default()
    } else {
        Rect::new(
            workspace_area.x + workspace_area.width / 2,
            area.bottom().saturating_sub(1),
            1,
            1,
        )
    };
    put_text(
        buffer,
        hits.sidebar_toggle.x,
        hits.sidebar_toggle.y,
        hits.sidebar_toggle.width,
        "»",
        if super::super::global_menu::global_menu_attention(snapshot) {
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(
                if matches!(
                    chrome_hover,
                    Some(super::feedback::ChromeHover::SidebarToggle)
                ) {
                    palette.text
                } else {
                    palette.overlay0
                },
            )
        },
    );
}

pub(crate) fn render_sidebar(
    buffer: &mut Buffer,
    area: Rect,
    snapshot: &ClientShellSnapshot,
    config: &ClientShellConfig,
    state: &mut ShellRenderState<'_>,
    hits: &mut ShellHitMap,
) {
    render_sidebar_regions(buffer, area, snapshot, config, state, hits, None);
}

pub(crate) fn render_sidebar_regions(
    buffer: &mut Buffer,
    area: Rect,
    snapshot: &ClientShellSnapshot,
    config: &ClientShellConfig,
    state: &mut ShellRenderState<'_>,
    hits: &mut ShellHitMap,
    regions: Option<(Rect, Rect)>,
) {
    let palette = &config.palette;
    render_sidebar_background(
        buffer,
        area,
        palette,
        matches!(
            state.chrome_hover,
            Some(super::feedback::ChromeHover::SidebarDivider)
        ),
    );
    hits.sidebar_divider = if area.is_empty() {
        Rect::default()
    } else {
        Rect::new(area.right().saturating_sub(1), area.y, 1, area.height)
    };
    let (workspace_area, detail_area) = regions
        .unwrap_or_else(|| crate::ui::expanded_sidebar_sections(area, state.sidebar_section_split));
    hits.sidebar_section_divider =
        crate::ui::sidebar_section_divider_rect(area, state.sidebar_section_split);
    put_text(
        buffer,
        workspace_area.x,
        workspace_area.y,
        workspace_area.width,
        crate::i18n::texts().sidebar.spaces,
        Style::default()
            .fg(palette.overlay0)
            .add_modifier(Modifier::BOLD),
    );

    let entries = workspace_entries(snapshot, state.collapsed_groups);
    let body = Rect::new(
        workspace_area.x,
        workspace_area.y.saturating_add(WORKSPACE_HEADER_ROWS),
        workspace_area.width,
        workspace_area
            .height
            .saturating_sub(WORKSPACE_HEADER_ROWS + 1),
    );
    hits.workspace_body = body;
    let row_heights = entries
        .iter()
        .map(|entry| {
            snapshot
                .workspaces
                .get(entry.index)
                .map(|workspace| {
                    workspace_rows(
                        workspace,
                        displayed_workspace_status(snapshot, workspace, state.collapsed_groups),
                        entry.indented,
                        &config.spaces,
                    )
                    .len()
                    .max(1)
                    .min(u16::MAX as usize) as u16
                })
                .unwrap_or(1)
        })
        .collect::<Vec<_>>();
    let gaps = entries
        .iter()
        .enumerate()
        .map(|(index, _)| {
            entries
                .get(index + 1)
                .map_or(0, |next| u16::from(!next.indented) * config.spaces.row_gap)
        })
        .collect::<Vec<_>>();
    let mut metrics = super::scroll::list_scroll_metrics(
        &row_heights,
        &gaps,
        body.height,
        *state.workspace_scroll,
    );
    if !body.is_empty() && std::mem::take(state.reveal_focused_workspace) {
        if let Some(target) = entries
            .iter()
            .position(|entry| snapshot.workspaces[entry.index].focused)
        {
            *state.workspace_scroll = super::scroll::list_scroll_start_to_reveal(
                &row_heights,
                &gaps,
                body.height,
                *state.workspace_scroll,
                target,
            );
            metrics = super::scroll::list_scroll_metrics(
                &row_heights,
                &gaps,
                body.height,
                *state.workspace_scroll,
            );
        }
    }
    hits.workspace_max_scroll = metrics.max_offset_from_bottom;
    hits.workspace_scroll_metrics = Some(metrics);
    *state.workspace_scroll = metrics
        .max_offset_from_bottom
        .saturating_sub(metrics.offset_from_bottom);
    let show_scrollbar = metrics.max_offset_from_bottom > 0 && body.width > 1;
    let content_width = body.width.saturating_sub(u16::from(show_scrollbar));
    let mut y = body.y;
    for (entry_position, entry) in entries.iter().enumerate().skip(*state.workspace_scroll) {
        let Some(workspace) = snapshot.workspaces.get(entry.index) else {
            continue;
        };
        let status = displayed_workspace_status(snapshot, workspace, state.collapsed_groups);
        let rows = workspace_rows(workspace, status, entry.indented, &config.spaces);
        let row_height = (rows.len().max(1).min(u16::MAX as usize) as u16).min(body.height);
        if y.saturating_add(row_height) > body.bottom() {
            break;
        }
        let rect = Rect::new(body.x, y, content_width, row_height);
        let selected = state.selected_workspace_id.is_some_and(|target| {
            target.matches(state.active_endpoint_id, &workspace.workspace_id)
        });
        let dragged = state.dragged_workspace_id == Some(workspace.workspace_id.as_str());
        let hovered = matches!(
            state.chrome_hover,
            Some(super::feedback::ChromeHover::WorkspaceRow {
                endpoint_id,
                workspace_id,
            }) if endpoint_id.is_local() && workspace_id == &workspace.workspace_id
        );
        if selected {
            buffer.set_style(rect, Style::default().bg(palette.selection_row_bg()));
        } else if dragged {
            buffer.set_style(rect, Style::default().bg(palette.surface1));
        } else if workspace.focused {
            buffer.set_style(rect, Style::default().bg(palette.active_row_bg));
        }
        render_workspace_rows(
            buffer,
            rect,
            workspace,
            status,
            config.status_indicators,
            entry,
            rows,
            true,
            selected,
            dragged,
            hovered,
            palette,
        );
        let group_toggle = render_parent_group_toggle(
            buffer,
            rect,
            snapshot,
            entry.index,
            state.collapsed_groups,
            palette,
        );
        hits.workspaces.push(WorkspaceHit {
            rect,
            endpoint_id: ClientEndpointId::Local,
            workspace_id: workspace.workspace_id.clone(),
            indented: entry.indented,
            group_toggle,
        });
        let gap = entries
            .get(entry_position + 1)
            .map_or(0, |next| u16::from(!next.indented) * config.spaces.row_gap);
        y = y.saturating_add(row_height + gap);
    }

    if show_scrollbar {
        let track = Rect::new(body.right().saturating_sub(1), body.y, 1, body.height);
        hits.workspace_scrollbar = track;
        let thumb_hovered = matches!(
            state.chrome_hover,
            Some(super::feedback::ChromeHover::WorkspaceScrollbarThumb)
        );
        super::scroll::render_list_scrollbar(buffer, track, metrics, palette, thumb_hovered);
    }

    if let Some(row) = state.workspace_drop_indicator_row.filter(|row| {
        *row >= workspace_area.y.saturating_add(1)
            && *row < workspace_area.bottom().saturating_sub(1)
    }) {
        put_text(
            buffer,
            body.x,
            row,
            body.width,
            &"─".repeat(body.width as usize),
            Style::default().fg(palette.accent),
        );
    }

    let footer_y = workspace_area.bottom().saturating_sub(1);
    if config.mouse_capture {
        hits.new_workspace = Rect::new(
            workspace_area.x,
            footer_y,
            5.min(workspace_area.width),
            u16::from(workspace_area.height > 0),
        );
        put_text(
            buffer,
            workspace_area.x,
            footer_y,
            workspace_area.width,
            crate::i18n::texts().sidebar.new,
            Style::default().fg(palette.overlay0),
        );
        let attention = super::super::global_menu::global_menu_attention(snapshot);
        let menu_label = crate::i18n::texts().sidebar.menu;
        let menu_width = super::render::display_width(menu_label);
        let launcher_width = if attention {
            menu_width.saturating_add(2)
        } else {
            menu_width
        }
        .min(workspace_area.width);
        hits.global_launcher = Rect::new(
            workspace_area.right().saturating_sub(launcher_width),
            footer_y,
            launcher_width,
            1,
        );
        let launcher_hovered = matches!(
            state.chrome_hover,
            Some(super::feedback::ChromeHover::GlobalLauncher)
        );
        if attention {
            let start_x = workspace_area
                .right()
                .saturating_sub(menu_width.saturating_add(2));
            put_text(
                buffer,
                start_x,
                footer_y,
                2,
                "● ",
                Style::default()
                    .fg(palette.accent)
                    .add_modifier(Modifier::BOLD),
            );
            put_text(
                buffer,
                start_x.saturating_add(2),
                footer_y,
                menu_width,
                menu_label,
                Style::default().fg(if launcher_hovered {
                    palette.text
                } else {
                    palette.overlay0
                }),
            );
        } else {
            put_right_text(
                buffer,
                workspace_area,
                footer_y,
                menu_label,
                Style::default().fg(if launcher_hovered {
                    palette.text
                } else {
                    palette.overlay0
                }),
            );
        }
    }

    super::render_agent_panel(
        buffer,
        detail_area,
        snapshot,
        config,
        state.collapsed_groups,
        state.agent_scroll,
        state.chrome_hover,
        hits,
    );

    hits.sidebar_toggle = Rect::new(
        area.right().saturating_sub(2),
        area.bottom().saturating_sub(1),
        u16::from(area.width > 1),
        u16::from(area.height > 0),
    );
    put_text(
        buffer,
        hits.sidebar_toggle.x,
        hits.sidebar_toggle.y,
        hits.sidebar_toggle.width,
        "«",
        Style::default().fg(
            if matches!(
                state.chrome_hover,
                Some(super::feedback::ChromeHover::SidebarToggle)
            ) {
                palette.text
            } else {
                palette.overlay0
            },
        ),
    );
}

pub(crate) fn workspace_entries(
    snapshot: &ClientShellSnapshot,
    collapsed_groups: &HashSet<String>,
) -> Vec<WorkspaceEntry> {
    let mut members = HashMap::<&str, Vec<usize>>::new();
    for (index, workspace) in snapshot.workspaces.iter().enumerate() {
        if let Some(worktree) = &workspace.worktree {
            members.entry(&worktree.key).or_default().push(index);
        }
    }
    let grouped = members
        .iter()
        .filter(|(_, indices)| {
            indices.len() >= 2
                && indices.iter().any(|index| {
                    snapshot.workspaces[*index]
                        .worktree
                        .as_ref()
                        .is_some_and(|worktree| !worktree.is_linked_worktree)
                })
        })
        .map(|(key, _)| *key)
        .collect::<HashSet<_>>();
    let mut emitted = HashSet::<&str>::new();
    let mut entries = Vec::new();
    for (index, workspace) in snapshot.workspaces.iter().enumerate() {
        let Some(worktree) = workspace
            .worktree
            .as_ref()
            .filter(|worktree| grouped.contains(worktree.key.as_str()))
        else {
            entries.push(WorkspaceEntry {
                index,
                indented: false,
                last_child: false,
            });
            continue;
        };
        if !emitted.insert(&worktree.key) {
            continue;
        }
        let Some(group_members) = members.get(worktree.key.as_str()) else {
            continue;
        };
        let parent = group_members
            .iter()
            .copied()
            .find(|member| {
                snapshot.workspaces[*member]
                    .worktree
                    .as_ref()
                    .is_some_and(|worktree| !worktree.is_linked_worktree)
            })
            .unwrap_or(index);
        entries.push(WorkspaceEntry {
            index: parent,
            indented: false,
            last_child: false,
        });
        if collapsed_groups.contains(&worktree.key) {
            if let Some(active) = group_members
                .iter()
                .copied()
                .find(|member| *member != parent && snapshot.workspaces[*member].focused)
            {
                entries.push(WorkspaceEntry {
                    index: active,
                    indented: true,
                    last_child: true,
                });
            }
            continue;
        }
        let children = group_members
            .iter()
            .copied()
            .filter(|member| *member != parent)
            .collect::<Vec<_>>();
        for (child_index, child) in children.iter().enumerate() {
            entries.push(WorkspaceEntry {
                index: *child,
                indented: true,
                last_child: child_index + 1 == children.len(),
            });
        }
    }
    entries
}

fn parent_group_key(snapshot: &ClientShellSnapshot, index: usize) -> Option<String> {
    let workspace = snapshot.workspaces.get(index)?;
    let worktree = workspace.worktree.as_ref()?;
    if worktree.is_linked_worktree {
        return None;
    }
    (snapshot
        .workspaces
        .iter()
        .filter(|candidate| {
            candidate
                .worktree
                .as_ref()
                .is_some_and(|candidate| candidate.key == worktree.key)
        })
        .count()
        >= 2)
        .then(|| worktree.key.clone())
}

pub(in crate::client::shell) fn render_parent_group_toggle(
    buffer: &mut Buffer,
    workspace_rect: Rect,
    snapshot: &ClientShellSnapshot,
    workspace_index: usize,
    collapsed_groups: &HashSet<String>,
    palette: &Palette,
) -> Option<(Rect, String)> {
    let key = parent_group_key(snapshot, workspace_index)?;
    let toggle = Rect::new(
        workspace_rect.right().saturating_sub(1),
        workspace_rect.y,
        1,
        1,
    );
    put_text(
        buffer,
        toggle.x,
        toggle.y,
        toggle.width,
        if collapsed_groups.contains(&key) {
            "▸"
        } else {
            "▾"
        },
        Style::default().fg(palette.accent),
    );
    Some((toggle, key))
}

pub(in crate::client::shell) fn displayed_workspace_status(
    snapshot: &ClientShellSnapshot,
    workspace: &ClientShellWorkspace,
    collapsed_groups: &HashSet<String>,
) -> crate::api::schema::AgentStatus {
    let Some(worktree) = workspace
        .worktree
        .as_ref()
        .filter(|worktree| !worktree.is_linked_worktree)
    else {
        return workspace.agent_status;
    };
    if !collapsed_groups.contains(&worktree.key) {
        return workspace.agent_status;
    }
    snapshot
        .workspaces
        .iter()
        .filter(|candidate| {
            candidate
                .worktree
                .as_ref()
                .is_some_and(|candidate| candidate.key == worktree.key)
        })
        .map(|candidate| candidate.agent_status)
        .max_by_key(|status| status_priority(*status))
        .unwrap_or(workspace.agent_status)
}

pub(in crate::client::shell) fn workspace_rows(
    workspace: &ClientShellWorkspace,
    status: crate::api::schema::AgentStatus,
    indented: bool,
    config: &SpacesSidebarConfig,
) -> Vec<Vec<crate::ui::ResolvedToken>> {
    let label = if indented && !workspace.custom_label {
        workspace
            .branch
            .as_deref()
            .and_then(|branch| branch.strip_prefix("worktree/").or(Some(branch)))
            .unwrap_or(&workspace.label)
    } else {
        &workspace.label
    };
    let token_values = workspace.tokens.iter().cloned().collect::<HashMap<_, _>>();
    crate::ui::sidebar_space_rows(
        config,
        crate::ui::SpaceTokenContext {
            workspace: label,
            branch: workspace.branch.as_deref(),
            state_text: status_text(status),
            ahead_behind: workspace.git_ahead_behind,
            tokens: &token_values,
            suppress_git_details: indented,
        },
    )
}

// Row chrome flags stay positional like the other sidebar row renderers;
// bundling them would touch every call site for no readability gain.
#[allow(clippy::too_many_arguments)]
pub(in crate::client::shell) fn render_workspace_rows(
    buffer: &mut Buffer,
    area: Rect,
    workspace: &ClientShellWorkspace,
    status: crate::api::schema::AgentStatus,
    indicators: crate::config::StatusIndicatorStyle,
    entry: &WorkspaceEntry,
    rows: Vec<Vec<crate::ui::ResolvedToken>>,
    endpoint_active: bool,
    selected: bool,
    dragged: bool,
    hovered: bool,
    palette: &Palette,
) {
    for (row_index, row) in rows.iter().enumerate() {
        let y = area.y + row_index as u16;
        if y >= area.bottom() {
            break;
        }
        let mut x = area.x;
        if entry.indented {
            let prefix = if row_index == 0 {
                if entry.last_child {
                    "   └─ "
                } else {
                    "   ├─ "
                }
            } else if entry.last_child {
                "        "
            } else {
                "   │    "
            };
            x = put_segment(
                buffer,
                x,
                y,
                area.right(),
                prefix,
                Style::default().fg(palette.overlay0),
            );
        } else if row_index == 0 {
            x = x.saturating_add(1);
        } else {
            x = x.saturating_add(3);
        }
        let highlighted = endpoint_active && workspace.focused || dragged;
        let workspace_style = Style::default()
            .fg(if highlighted {
                palette.text
            } else {
                palette.subtext0
            })
            .add_modifier(if highlighted {
                Modifier::BOLD
            } else {
                Modifier::empty()
            });
        let secondary_style = Style::default().fg(if endpoint_active && workspace.focused {
            palette.mauve
        } else {
            palette.overlay0
        });
        let spans = crate::ui::resolved_token_spans(
            row,
            (
                status_icon(status, indicators),
                Style::default().fg(status_color(status, palette)),
            ),
            Style::default().fg(status_color(status, palette)),
            workspace_style,
            secondary_style,
            Style::default().fg(palette.overlay1),
            palette,
            area.right().saturating_sub(2).saturating_sub(x) as usize,
        );
        Paragraph::new(Line::from(spans)).render(
            Rect::new(x, y, area.right().saturating_sub(2).saturating_sub(x), 1),
            buffer,
        );
    }

    let background = if selected {
        Some(palette.selection_row_bg())
    } else if dragged {
        Some(palette.surface1)
    } else if endpoint_active && workspace.focused {
        Some(palette.active_row_bg)
    } else if hovered {
        Some(palette.surface0)
    } else {
        None
    };
    if let Some(background) = background {
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                buffer[(x, y)].set_bg(background);
            }
        }
    }
}
