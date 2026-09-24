use ratatui::layout::Rect;

use crate::app;
use crate::protocol::{self, FrameData};

/// 快照里活动树的下发形态（体积护栏）。快照是逐客户端扇出路径，每次投影纪元
/// 变化都会整份重建、编码并比较。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SnapshotActivity {
    /// 每 agent 下发截断后的整棵树（至多 `MAX_AGENT_ACTIVITY_NODES` 个节点）。
    // 备用形态：生产默认是 `Summary`，本变体只由基准与测试构造；护栏阈值重新评估
    // 时切换 `SNAPSHOT_ACTIVITY` 即可启用，不必改客户端。
    #[cfg_attr(not(test), allow(dead_code))]
    Full,
    /// 每 agent 只下发计数 + 最新一个节点（`truncated` 表示还有更多）；整棵树经
    /// `agent.activity.read` 按需拉取。
    Summary,
}

/// 默认下发形态：摘要。实测（`render_scale_profile_agent_activity_snapshot`，
/// release，每 agent 32 节点；字节确定，耗时随机器浮动）：15 个 agent 时整树下发
/// 让快照 JSON 从 28.5 KB 涨到 163 KB（+472%），投影 + 编码中位耗时 +230%，远超
/// 25% 门槛；摘要形态 +14% 字节、中位耗时 +16%（1 个 agent 时 +2% / 持平）。
pub(crate) const SNAPSHOT_ACTIVITY: SnapshotActivity = SnapshotActivity::Summary;

#[cfg(test)]
pub(super) fn snapshot(
    app: &app::App,
    boot_id: &str,
    revision: u64,
    config_diagnostic: Option<&str>,
    location: Option<&crate::server::clients::ClientShellLocation>,
) -> protocol::ClientShellSnapshot {
    snapshot_with_completions(app, boot_id, revision, config_diagnostic, location).0
}

/// 生产投影入口：快照按默认下发形态 `SNAPSHOT_ACTIVITY`，同时产出完成序号投影。
pub(super) fn snapshot_with_completions(
    app: &app::App,
    boot_id: &str,
    revision: u64,
    config_diagnostic: Option<&str>,
    location: Option<&crate::server::clients::ClientShellLocation>,
) -> (
    protocol::ClientShellSnapshot,
    protocol::endpoint::EndpointAgentCompletions,
) {
    snapshot_parts(
        app,
        boot_id,
        revision,
        config_diagnostic,
        location,
        SNAPSHOT_ACTIVITY,
    )
}

/// 指定活动树下发形态的快照（基准与测试比较 `Full` / `Summary` 用）。
#[cfg(test)]
pub(super) fn snapshot_with_activity(
    app: &app::App,
    boot_id: &str,
    revision: u64,
    config_diagnostic: Option<&str>,
    location: Option<&crate::server::clients::ClientShellLocation>,
    activity: SnapshotActivity,
) -> protocol::ClientShellSnapshot {
    snapshot_parts(
        app,
        boot_id,
        revision,
        config_diagnostic,
        location,
        activity,
    )
    .0
}

fn snapshot_parts(
    app: &app::App,
    boot_id: &str,
    revision: u64,
    config_diagnostic: Option<&str>,
    location: Option<&crate::server::clients::ClientShellLocation>,
    activity: SnapshotActivity,
) -> (
    protocol::ClientShellSnapshot,
    protocol::endpoint::EndpointAgentCompletions,
) {
    let snapshot = app.session_snapshot_for_projection();
    let completions = protocol::endpoint::EndpointAgentCompletions {
        boot_id: boot_id.to_owned(),
        revision,
        completions: snapshot
            .agents
            .iter()
            .filter_map(|agent| agent.completion_seq.map(|seq| (agent.pane_id.clone(), seq)))
            .collect(),
    };
    let focused_workspace_id = location
        .and_then(|location| location.focused_workspace_id.clone())
        .or_else(|| snapshot.focused_workspace_id.clone());
    let focused_tab_id = location
        .and_then(|location| location.focused_tab_id().map(str::to_owned))
        .or_else(|| snapshot.focused_tab_id.clone());
    let focused_pane_id = focused_tab_id
        .as_deref()
        .and_then(|tab_id| app.parse_tab_id(tab_id))
        .and_then(|(workspace_index, tab_index)| {
            let pane_id = app
                .state
                .workspaces
                .get(workspace_index)?
                .tabs
                .get(tab_index)?
                .layout
                .focused();
            app.public_pane_id(workspace_index, pane_id)
        })
        .or_else(|| snapshot.focused_pane_id.clone());
    let workspaces = snapshot
        .workspaces
        .into_iter()
        .zip(&app.state.workspaces)
        .enumerate()
        .map(|(workspace_index, (workspace, state))| {
            let mut tokens = workspace.tokens.into_iter().collect::<Vec<_>>();
            tokens.sort_by(|left, right| left.0.cmp(&right.0));
            let workspace_id = workspace.workspace_id;
            let active_tab_id = location
                .and_then(|location| location.active_tab_ids.get(&workspace_id))
                .cloned()
                .unwrap_or(workspace.active_tab_id);
            let active_tab_index =
                app.parse_tab_id(&active_tab_id)
                    .and_then(|(tab_workspace_index, tab_index)| {
                        (tab_workspace_index == workspace_index).then_some(tab_index)
                    });
            protocol::ClientShellWorkspace {
                focused: focused_workspace_id.as_deref() == Some(workspace_id.as_str()),
                workspace_id,
                active_tab_id,
                new_workspace_cwd: app
                    .resolved_new_workspace_cwd_from_tab(workspace_index, active_tab_index)
                    .display()
                    .to_string(),
                number: workspace.number,
                label: workspace.label,
                custom_label: state.custom_name.is_some(),
                branch: state.branch(),
                git_ahead_behind: state.git_ahead_behind(),
                tokens,
                worktree: workspace
                    .worktree
                    .map(|worktree| protocol::ClientShellWorktree {
                        key: worktree.repo_key,
                        label: worktree.repo_name,
                        is_linked_worktree: worktree.is_linked_worktree,
                    }),
                agent_status: workspace.agent_status,
            }
        })
        .collect();
    let tabs = snapshot
        .tabs
        .into_iter()
        .map(|tab| {
            // APP-013：按 tab_id 解析对应状态，不按位置 zip——`session_snapshot`
            // 里任何被静默跳过的 tab 都会让后续 tab 的标签/状态错位。
            let state = app
                .parse_tab_id(&tab.tab_id)
                .and_then(|(ws_idx, tab_idx)| app.state.workspaces.get(ws_idx)?.tabs.get(tab_idx));
            let tab_id = tab.tab_id;
            protocol::ClientShellTab {
                focused: focused_tab_id.as_deref() == Some(tab_id.as_str()),
                tab_id,
                workspace_id: tab.workspace_id,
                number: tab.number,
                label: tab.label,
                custom_label: state.is_some_and(|state| !state.is_auto_named()),
                zoomed: state.is_some_and(|state| state.zoomed),
                agent_status: tab.agent_status,
            }
        })
        .collect();
    let panes = snapshot
        .panes
        .into_iter()
        .map(|pane| {
            let pane_id = pane.pane_id;
            let focused = focused_pane_id.as_deref() == Some(pane_id.as_str());
            let right_click_passthrough = app
                .parse_pane_id(&pane_id)
                .and_then(|(workspace_index, pane_id)| {
                    app.state
                        .workspaces
                        .get(workspace_index)?
                        .pane_state(pane_id)
                })
                .is_some_and(|pane| pane.right_click_passthrough);
            protocol::ClientShellPane {
                pane_id,
                workspace_id: pane.workspace_id,
                tab_id: pane.tab_id,
                label: pane.label,
                cwd: pane.cwd,
                foreground_cwd: pane.foreground_cwd,
                focused,
                right_click_passthrough,
            }
        })
        .collect();
    let agents = snapshot
        .agents
        .into_iter()
        .map(|agent| {
            let pane_id = agent.pane_id;
            let focused = focused_pane_id.as_deref() == Some(pane_id.as_str());
            let mut state_labels = agent.state_labels.into_iter().collect::<Vec<_>>();
            state_labels.sort_by(|left, right| left.0.cmp(&right.0));
            let mut tokens = agent.tokens.into_iter().collect::<Vec<_>>();
            tokens.sort_by(|left, right| left.0.cmp(&right.0));
            protocol::ClientShellAgent {
                workspace_id: agent.workspace_id,
                tab_id: agent.tab_id,
                name: agent.name,
                display_agent: agent.display_agent,
                agent: agent.agent,
                title: agent.title,
                terminal_title: agent.terminal_title,
                terminal_title_stripped: agent.terminal_title_stripped,
                agent_status: agent.agent_status,
                state_change_seq: agent.state_change_seq,
                state_labels,
                tokens,
                focused,
                launch_seq: agent.launch_seq,
                activity: agent_activity_projection(app, &pane_id, activity),
                pane_id,
            }
        })
        .collect();

    let agent_view_label = app
        .state
        .agent_view_override
        .as_ref()
        .map(|view| view.label.clone().unwrap_or_else(|| "filtered".to_owned()));
    let agent_order = crate::ui::agent_panel_entries_from(&app.state, &app.terminal_runtimes)
        .into_iter()
        .filter_map(|entry| app.public_pane_id(entry.ws_idx, entry.pane_id))
        .collect();

    let zoomed = focused_tab_id
        .as_deref()
        .and_then(|tab_id| app.parse_tab_id(tab_id))
        .and_then(|(workspace_index, tab_index)| {
            app.state
                .workspaces
                .get(workspace_index)?
                .tabs
                .get(tab_index)
        })
        .is_some_and(|tab| tab.zoomed);
    let tab_bar_right = app
        .state
        .tab_bar_right
        .iter()
        .filter_map(|segment| match segment {
            crate::app::state::TabBarStatusSegment::Zoom if zoomed => {
                Some(protocol::ClientShellTabStatusSegment {
                    text: "ZOOM".to_owned(),
                    accent: true,
                })
            }
            crate::app::state::TabBarStatusSegment::Text(Some(text)) if !text.is_empty() => {
                Some(protocol::ClientShellTabStatusSegment {
                    text: text.clone(),
                    accent: false,
                })
            }
            crate::app::state::TabBarStatusSegment::Zoom
            | crate::app::state::TabBarStatusSegment::Text(_) => None,
        })
        .collect();

    let product_announcement = app.state.product_announcement.as_ref().map(|announcement| {
        protocol::ClientShellProductAnnouncement {
            version: announcement.version.clone(),
            id: announcement.id.clone(),
            title: announcement.title.clone(),
            body: announcement.body.clone(),
            preview: announcement.preview,
        }
    });
    let release_notes =
        app.state
            .latest_release_notes
            .as_ref()
            .map(|notes| protocol::ClientShellReleaseNotes {
                version: notes.version.clone(),
                body: notes.body.clone(),
                preview: notes.preview,
            });

    let shell = protocol::ClientShellSnapshot {
        boot_id: boot_id.to_owned(),
        revision,
        config_diagnostic: config_diagnostic.map(str::to_owned),
        product_announcement,
        update_available: app.state.update_available.clone(),
        update_install_command: app.state.update_install_command.clone(),
        server_keybindings_toml: app.client_shell_keybindings_profile().map(str::to_owned),
        latest_release_notes_available: app.state.latest_release_notes_available,
        integration_updates_available: app.state.integration_updates_available(),
        worktree_directory: app.state.worktree_directory.to_string_lossy().into_owned(),
        release_notes,
        focused_workspace_id,
        focused_tab_id,
        focused_pane_id,
        tab_bar_right,
        tab_bar_right_separator: app.state.tab_bar_right_separator.clone(),
        agent_view_label,
        agent_order,
        workspaces,
        tabs,
        panes,
        agents,
        commands: app.client_shell_command_manifest(),
        external_agents: external_agents_projection(app, activity),
    };
    (shell, completions)
}

/// 一个 agent 的活动树投影，直接读存储（投影路径的 `AgentInfo` 不复制活动树）。
/// 存储里没有任何活动树时不解析 pane id。
fn agent_activity_projection(
    app: &app::App,
    public_pane_id: &str,
    mode: SnapshotActivity,
) -> protocol::ClientShellAgentActivity {
    if !app.state.agent_activity.has_activity() {
        return protocol::ClientShellAgentActivity::default();
    }
    let Some(stored) = app
        .parse_pane_id(public_pane_id)
        .and_then(|(_, pane_id)| app.state.agent_activity.activity(pane_id))
    else {
        return protocol::ClientShellAgentActivity::default();
    };
    activity_projection(
        stored.counts.running,
        stored.counts.total,
        stored.truncated,
        &stored.nodes,
        mode,
    )
}

/// 外部来源条目投影（不属于任何 pane）。
fn external_agents_projection(
    app: &app::App,
    mode: SnapshotActivity,
) -> Vec<protocol::ClientShellExternalAgent> {
    app.state
        .agent_activity
        .external()
        .iter()
        .map(|record| {
            let info = &record.info;
            protocol::ClientShellExternalAgent {
                external_id: info.external_id.clone(),
                source: info.source.clone(),
                agent_status: info.agent_status,
                label: info.label.clone(),
                readable: info.readable,
                agent: info.agent.clone(),
                cwd: info.cwd.clone(),
                updated_at_ms: info.updated_at_ms,
                activity: activity_projection(
                    record.counts.running,
                    record.counts.total,
                    record.truncated,
                    &info.activity,
                    mode,
                ),
            }
        })
        .collect()
}

fn activity_projection(
    running: u32,
    total: u32,
    truncated: bool,
    nodes: &[crate::api::schema::AgentActivityNode],
    mode: SnapshotActivity,
) -> protocol::ClientShellAgentActivity {
    match mode {
        SnapshotActivity::Full => protocol::ClientShellAgentActivity {
            running,
            total,
            truncated,
            nodes: nodes
                .iter()
                .cloned()
                .map(activity_node_projection)
                .collect(),
        },
        // 摘要的选点与截断口径是 `AppState::apply_agent_activity` 判定「投影是否
        // 变化」的同一套规则，真源在 `app::state`，两边不得各写一份。
        SnapshotActivity::Summary => {
            let latest = app::state::latest_activity_node(nodes);
            protocol::ClientShellAgentActivity {
                running,
                total,
                truncated: app::state::activity_summary_truncated(truncated, nodes),
                nodes: latest
                    .cloned()
                    .map(activity_node_projection)
                    .into_iter()
                    .collect(),
            }
        }
    }
}

/// `AgentActivityNode` → wire 镜像（字段逐一移入）。
fn activity_node_projection(
    node: crate::api::schema::AgentActivityNode,
) -> protocol::ClientShellActivityNode {
    protocol::ClientShellActivityNode {
        id: node.id,
        kind: node.kind,
        label: node.label,
        status: node.status,
        parent_id: node.parent_id,
        agent_type: node.agent_type,
        content_ref: node.content_ref,
        summary: node.summary,
        started_at_ms: node.started_at_ms,
        ended_at_ms: node.ended_at_ms,
    }
}

/// RS-06：一次完整渲染的结果。可被同 tick 内同 tab 同几何的客户端复用。
#[derive(Clone)]
pub(super) struct RenderedPaneSurface {
    pub(super) frame: FrameData,
    pub(super) panes: Vec<protocol::PaneSurfacePane>,
    pub(super) splits: Vec<protocol::PaneSurfaceSplit>,
    pub(super) popup: Option<Box<protocol::ClientShellPopupSurface>>,
    pub(super) graphics: protocol::SurfaceGraphicsScene,
    pub(super) graphics_delivery: crate::kitty_graphics::surface::DeliveryCache,
    pub(super) graphics_sources: crate::kitty_graphics::surface::SourceFiles,
}

#[derive(Debug)]
pub(super) enum SurfaceRenderDeferred {
    Synchronized,
    Changed,
}

/// RS-06：单次 render_and_stream 内按（tab、面积、cell 尺寸、popup）复用
/// 同 tab 多客户端的渲染结果。键不覆盖的 per-client 部分（graphics_delivery、
/// 投影修订、序列化）由调用方重算。仅在 resize_panes=false 的调用使用
/// （渲染不改状态，复用安全）；含 kitty graphics 资产的场景不入缓存。
#[derive(Default)]
pub(super) struct SurfaceMemo {
    entries: std::collections::HashMap<SurfaceMemoKey, RenderedPaneSurface>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct SurfaceMemoKey {
    target: Option<(usize, usize)>,
    width: u16,
    height: u16,
    cell_width_px: u32,
    cell_height_px: u32,
    shows_popup: bool,
}

impl SurfaceMemoKey {
    pub(super) fn new(
        target: Option<crate::ui::TabSurfaceTarget>,
        area: Rect,
        cell_size: crate::kitty_graphics::HostCellSize,
        shows_popup: bool,
    ) -> Self {
        Self {
            target: target.map(|target| (target.workspace_index, target.tab_index)),
            width: area.width,
            height: area.height,
            cell_width_px: cell_size.width_px,
            cell_height_px: cell_size.height_px,
            shows_popup,
        }
    }
}

impl SurfaceMemo {
    /// 命中返回克隆；调用方负责把 `graphics_delivery` 换成命中客户端自己的。
    /// 只允许 delivery 为空（无已投递/待决资产）的客户端命中。
    pub(super) fn reuse(&self, key: SurfaceMemoKey) -> Option<RenderedPaneSurface> {
        self.entries.get(&key).cloned()
    }

    /// 场景不含任何图形资产时才入缓存（kitty graphics pane 排除）。
    pub(super) fn store(&mut self, key: SurfaceMemoKey, rendered: &RenderedPaneSurface) {
        if rendered.graphics.assets.is_empty()
            && rendered.graphics.placements.is_empty()
            && rendered.graphics.retained_assets.is_empty()
        {
            self.entries.insert(key, rendered.clone());
        }
    }
}

pub(super) fn render_pane_surface(
    app: &mut app::App,
    target: Option<crate::ui::TabSurfaceTarget>,
    area: Rect,
    resize_panes: bool,
    show_popup: bool,
    cell_size: crate::kitty_graphics::HostCellSize,
    graphics_delivery: &crate::kitty_graphics::surface::DeliveryCache,
    client_id: u64,
) -> Result<RenderedPaneSurface, SurfaceRenderDeferred> {
    let layout = crate::ui::compute_tab_surface_for(
        &app.state,
        &app.terminal_runtimes,
        target,
        area,
        resize_panes,
        cell_size,
    );
    let mut content_revisions_before = std::collections::HashMap::new();
    if let Some(target) = target {
        for pane in &layout.pane_infos {
            if let Some(runtime) = app.state.runtime_for_pane_in_workspace(
                &app.terminal_runtimes,
                target.workspace_index,
                pane.id,
            ) {
                let (synchronized, epoch) = runtime.synchronized_output_state();
                if synchronized {
                    return Err(SurfaceRenderDeferred::Synchronized);
                }
                let revision = runtime.content_seq();
                content_revisions_before.insert(pane.id, (epoch, revision));
            }
        }
    }
    let popup_revision_before = if show_popup {
        app.state
            .popup_pane
            .as_ref()
            .and_then(|popup| app.terminal_runtimes.get(&popup.terminal_id))
            .map(|runtime| {
                let (synchronized, epoch) = runtime.synchronized_output_state();
                if synchronized {
                    return Err(SurfaceRenderDeferred::Synchronized);
                }
                Ok(epoch)
            })
            .transpose()?
    } else {
        None
    };
    let (buffer, cursor, hyperlinks, layout) =
        crate::server::render_stream::render_tab_surface_virtual(
            &app.state,
            &app.terminal_runtimes,
            layout,
            area,
        );
    let panes = target
        .map(|target| {
            let workspace_index = target.workspace_index;
            layout
                .pane_infos
                .iter()
                .filter_map(|pane| {
                    app.public_pane_id(workspace_index, pane.id).map(|pane_id| {
                        let runtime = app.state.runtime_for_pane_in_workspace(
                            &app.terminal_runtimes,
                            workspace_index,
                            pane.id,
                        );
                        let metadata = runtime.map_or(
                            crate::pane::PanePresentationMetadata::default(),
                            crate::terminal::TerminalRuntime::presentation_metadata,
                        );
                        let (pixel_width, pixel_height) = if cell_size.is_known() {
                            (
                                u32::from(pane.inner_rect.width) * cell_size.width_px,
                                u32::from(pane.inner_rect.height) * cell_size.height_px,
                            )
                        } else {
                            (0, 0)
                        };
                        let content_revision = runtime.map_or(0, |runtime| {
                            let after = runtime.content_seq();
                            if content_revisions_before
                                .get(&pane.id)
                                .is_some_and(|&(_, before)| before == after)
                                && after.is_multiple_of(2)
                            {
                                after
                            } else {
                                after | 1
                            }
                        });
                        protocol::PaneSurfacePane {
                            pane_id,
                            content_revision,
                            rect: pane.rect.into(),
                            inner_rect: pane.inner_rect.into(),
                            scrollbar_rect: pane.scrollbar_rect.map(Into::into),
                            scroll: metadata.scroll_metrics.map(|metrics| {
                                protocol::PaneSurfaceScrollMetrics {
                                    offset_from_bottom: metrics.offset_from_bottom as u64,
                                    max_offset_from_bottom: metrics.max_offset_from_bottom as u64,
                                    viewport_rows: metrics.viewport_rows as u64,
                                }
                            }),
                            focused: pane.is_focused,
                            mouse_reporting: metadata.mouse_reporting,
                            sgr_pixel_mouse: metadata.sgr_pixel_mouse,
                            alternate_screen_active: metadata.alternate_screen_active,
                            pixel_width,
                            pixel_height,
                        }
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let pane_frames = layout
        .pane_infos
        .iter()
        .map(|pane| pane.rect)
        .collect::<Vec<_>>();
    let splits = layout
        .split_borders
        .iter()
        .filter_map(|split| {
            let hit_rect = split_hit_rect(
                split,
                app.state.pane_borders.draws_borders(),
                app.state.pane_gaps,
                &pane_frames,
            )?;
            let direction = match split.direction {
                ratatui::layout::Direction::Horizontal => {
                    protocol::PaneSurfaceSplitDirection::Horizontal
                }
                ratatui::layout::Direction::Vertical => {
                    protocol::PaneSurfaceSplitDirection::Vertical
                }
            };
            Some(protocol::PaneSurfaceSplit {
                direction,
                pos: split.pos,
                area: split.area.into(),
                hit_rect: hit_rect.into(),
                path: split.path.clone(),
            })
        })
        .collect();
    let popup = show_popup
        .then(|| render_popup_surface(app, area, resize_panes, cell_size))
        .flatten();
    let (graphics, next_graphics_delivery, graphics_sources) =
        crate::server::client_shell_graphics::collect(
            app,
            &layout.pane_infos,
            &layout.split_borders,
            popup.as_deref(),
            target,
            cell_size,
            graphics_delivery,
            client_id,
        );
    if let Some(target) = target {
        for (&pane_id, &(epoch, _)) in &content_revisions_before {
            if let Some(runtime) = app.state.runtime_for_pane_in_workspace(
                &app.terminal_runtimes,
                target.workspace_index,
                pane_id,
            ) {
                let (synchronized, after_epoch) = runtime.synchronized_output_state();
                if synchronized {
                    return Err(SurfaceRenderDeferred::Synchronized);
                }
                if after_epoch != epoch {
                    return Err(SurfaceRenderDeferred::Changed);
                }
            }
        }
    }
    if let Some(before) = popup_revision_before {
        if let Some(runtime) = app
            .state
            .popup_pane
            .as_ref()
            .and_then(|popup| app.terminal_runtimes.get(&popup.terminal_id))
        {
            let (synchronized, after_epoch) = runtime.synchronized_output_state();
            if synchronized {
                return Err(SurfaceRenderDeferred::Synchronized);
            }
            if after_epoch != before {
                return Err(SurfaceRenderDeferred::Changed);
            }
        }
    }
    Ok(RenderedPaneSurface {
        frame: FrameData::from_ratatui_buffer_with_hyperlinks(&buffer, cursor, &hyperlinks),
        panes,
        splits,
        popup,
        graphics,
        graphics_delivery: next_graphics_delivery,
        graphics_sources,
    })
}

fn render_popup_surface(
    app: &app::App,
    area: Rect,
    resize_runtime: bool,
    cell_size: crate::kitty_graphics::HostCellSize,
) -> Option<Box<protocol::ClientShellPopupSurface>> {
    let popup = app.state.popup_pane.as_ref()?;
    let geometry = if resize_runtime {
        resize_popup_runtime(app, area, cell_size)?
    } else {
        crate::popup_size::resolve_popup_geometry(popup.width, popup.height, area)?
    };
    let runtime = app.terminal_runtimes.get(&popup.terminal_id)?;
    let content_area = Rect::new(0, 0, geometry.inner.width, geometry.inner.height);
    let (buffer, cursor) =
        crate::server::render_stream::render_terminal_virtual(runtime, content_area);
    let hyperlinks = runtime.visible_hyperlinks(content_area);
    let metadata = runtime.presentation_metadata();
    let title = app
        .state
        .terminals
        .get(&popup.terminal_id)
        .and_then(|terminal| terminal.manual_label.clone())
        .unwrap_or_else(|| crate::i18n::texts().chrome.popup_title.to_owned());
    let (pixel_width, pixel_height) = if cell_size.is_known() {
        (
            u32::from(content_area.width) * cell_size.width_px,
            u32::from(content_area.height) * cell_size.height_px,
        )
    } else {
        (0, 0)
    };
    Some(Box::new(protocol::ClientShellPopupSurface {
        terminal_id: popup.terminal_id.to_string(),
        title,
        width: popup.width.map(client_popup_size),
        height: popup.height.map(client_popup_size),
        frame: FrameData::from_ratatui_buffer_with_hyperlinks(&buffer, cursor, &hyperlinks),
        mouse_reporting: metadata.mouse_reporting,
        sgr_pixel_mouse: metadata.sgr_pixel_mouse,
        pixel_width,
        pixel_height,
    }))
}

pub(super) fn resize_popup_runtime(
    app: &app::App,
    area: Rect,
    cell_size: crate::kitty_graphics::HostCellSize,
) -> Option<crate::popup_size::PopupResolvedGeometry> {
    let popup = app.state.popup_pane.as_ref()?;
    let geometry = crate::popup_size::resolve_popup_geometry(popup.width, popup.height, area)?;
    let runtime = app.terminal_runtimes.get(&popup.terminal_id)?;
    if !app
        .state
        .direct_attach_resize_locks
        .contains(&popup.terminal_id)
    {
        runtime.resize(
            geometry.inner.height,
            geometry.inner.width,
            cell_size.width_px,
            cell_size.height_px,
        );
    }
    Some(geometry)
}

fn client_popup_size(size: crate::popup_size::PopupSize) -> protocol::ClientShellPopupSize {
    match size {
        crate::popup_size::PopupSize::Cells(cells) => protocol::ClientShellPopupSize::Cells(cells),
        crate::popup_size::PopupSize::Percent(percent) => {
            protocol::ClientShellPopupSize::Percent(percent)
        }
    }
}

fn split_hit_rect(
    split: &crate::layout::SplitBorder,
    pane_borders: bool,
    pane_gaps: bool,
    pane_frames: &[Rect],
) -> Option<Rect> {
    let hit = match (split.direction, pane_borders, pane_gaps) {
        (ratatui::layout::Direction::Horizontal, true, false) => {
            Rect::new(split.pos, split.area.y, 1, split.area.height)
        }
        (ratatui::layout::Direction::Horizontal, true, true) => {
            let start = split.pos.saturating_sub(1);
            Rect::new(
                start,
                split.area.y,
                split.pos.saturating_sub(start).saturating_add(1),
                split.area.height,
            )
        }
        (ratatui::layout::Direction::Horizontal, false, true) => Rect::new(
            split.pos.checked_sub(1)?,
            split.area.y,
            1,
            split.area.height,
        ),
        (ratatui::layout::Direction::Vertical, true, false) => {
            Rect::new(split.area.x, split.pos, split.area.width, 1)
        }
        (ratatui::layout::Direction::Vertical, true, true) => {
            let start = split.pos.saturating_sub(1);
            Rect::new(
                split.area.x,
                start,
                split.area.width,
                split.pos.saturating_sub(start).saturating_add(1),
            )
        }
        (ratatui::layout::Direction::Vertical, false, true) => {
            Rect::new(split.area.x, split.pos.checked_sub(1)?, split.area.width, 1)
        }
        (_, false, false) => return None,
    };
    if !pane_borders
        && pane_frames.iter().any(|pane| {
            hit.x < pane.right()
                && hit.right() > pane.x
                && hit.y < pane.bottom()
                && hit.bottom() > pane.y
        })
    {
        return None;
    }
    Some(hit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_projects_cached_release_and_update_facts() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = crate::app::App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.integration_recommendations.clear();
        app.state.update_available = Some("0.8.3".into());
        app.state.update_install_command = "herdr update".into();
        app.state.latest_release_notes_available = true;
        app.state.latest_release_notes = Some(crate::release_notes::ReleaseNotes {
            version: "0.8.3".into(),
            body: "### Changed\n- Client shell".into(),
            preview: true,
        });

        let snapshot = snapshot(&app, "boot", 7, None, None);

        assert_eq!(snapshot.update_available.as_deref(), Some("0.8.3"));
        assert_eq!(snapshot.update_install_command, "herdr update");
        assert!(snapshot.latest_release_notes_available);
        assert!(!snapshot.integration_updates_available);
        assert_eq!(
            snapshot.release_notes.as_ref().map(|notes| (
                notes.version.as_str(),
                notes.body.as_str(),
                notes.preview
            )),
            Some(("0.8.3", "### Changed\n- Client shell", true))
        );
    }

    #[test]
    fn snapshot_badges_only_outdated_integrations() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = crate::app::App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.integration_recommendations =
            vec![crate::integration::IntegrationRecommendation {
                target: crate::api::schema::IntegrationTarget::Claude,
                label: "claude",
                command: "claude",
                available: true,
                path: std::path::PathBuf::from("claude-hook"),
                state: crate::integration::IntegrationStatusKind::NotInstalled,
            }];

        assert!(!snapshot(&app, "boot", 1, None, None).integration_updates_available);

        app.state.integration_recommendations[0].state =
            crate::integration::IntegrationStatusKind::Outdated;
        assert!(snapshot(&app, "boot", 2, None, None).integration_updates_available);
    }

    #[test]
    fn split_hits_follow_released_border_and_gap_geometry() {
        let horizontal = crate::layout::SplitBorder {
            pos: 20,
            direction: ratatui::layout::Direction::Horizontal,
            ratio: 0.5,
            area: Rect::new(2, 3, 40, 12),
            path: vec![false],
        };
        assert_eq!(
            split_hit_rect(&horizontal, true, false, &[]),
            Some(Rect::new(20, 3, 1, 12))
        );
        assert_eq!(
            split_hit_rect(&horizontal, true, true, &[]),
            Some(Rect::new(19, 3, 2, 12))
        );
        assert_eq!(
            split_hit_rect(&horizontal, false, true, &[]),
            Some(Rect::new(19, 3, 1, 12))
        );
        assert_eq!(split_hit_rect(&horizontal, false, false, &[]), None);

        let vertical = crate::layout::SplitBorder {
            pos: 9,
            direction: ratatui::layout::Direction::Vertical,
            ratio: 0.5,
            area: Rect::new(2, 3, 40, 12),
            path: vec![true],
        };
        assert_eq!(
            split_hit_rect(&vertical, true, true, &[]),
            Some(Rect::new(2, 8, 40, 2))
        );

        let edge = crate::layout::SplitBorder {
            pos: 0,
            direction: ratatui::layout::Direction::Horizontal,
            ratio: 0.5,
            area: Rect::new(0, 0, 1, 4),
            path: Vec::new(),
        };
        assert_eq!(
            split_hit_rect(&edge, true, true, &[]),
            Some(Rect::new(0, 0, 1, 4))
        );
        assert_eq!(split_hit_rect(&edge, false, true, &[]), None);
        assert_eq!(
            split_hit_rect(&horizontal, false, true, &[Rect::new(19, 3, 1, 12)]),
            None
        );
    }
}

#[cfg(test)]
mod agent_activity_tests {
    use super::snapshot;
    use crate::detect::{Agent, AgentState};
    use crate::events::AppEvent;

    fn app_with_panes(names: &[&str]) -> crate::app::App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = crate::app::App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        for name in names {
            app.state
                .workspaces
                .push(crate::workspace::Workspace::test_new(name));
        }
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        app
    }

    fn detect(app: &mut crate::app::App, pane_id: crate::layout::PaneId, agent: Agent) {
        app.handle_internal_event(AppEvent::StateChanged {
            pane_id,
            agent: Some(agent),
            state: AgentState::Working,
            visible_blocker: false,
            visible_working: false,
            process_exited: false,
            observed_at: std::time::Instant::now(),
        });
    }

    /// headless 投影带上 server 分配的启动序号：按识别顺序 1、2，未识别的 pane 不进
    /// agents。
    #[test]
    fn snapshot_carries_server_assigned_launch_seq() {
        let mut app = app_with_panes(&["first", "second"]);
        let first = app.state.workspaces[0].tabs[0].root_pane;
        let second = app.state.workspaces[1].tabs[0].root_pane;
        // 反序识别：序号跟识别顺序走，不跟工作区顺序走。
        detect(&mut app, second, Agent::Claude);
        detect(&mut app, first, Agent::Pi);

        let snapshot = snapshot(&app, "boot", 1, None, None);
        let mut seqs = snapshot
            .agents
            .iter()
            .map(|agent| (agent.agent.clone(), agent.launch_seq))
            .collect::<Vec<_>>();
        seqs.sort();
        assert_eq!(
            seqs,
            vec![(Some("claude".into()), 1), (Some("pi".into()), 2)]
        );
    }

    fn snapshot_with_activity() -> crate::protocol::ClientShellSnapshot {
        use crate::api::schema::{AgentActivityNode, AgentActivityStatus};
        let mut app = app_with_panes(&["first"]);
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        detect(&mut app, pane_id, Agent::Claude);
        app.handle_internal_event(AppEvent::AgentActivityRefreshed {
            pane_id,
            ticket: 1,
            result: Ok(vec![AgentActivityNode {
                id: "a".into(),
                label: "task a".into(),
                status: AgentActivityStatus::Running,
                started_at_ms: Some(10),
                ..AgentActivityNode::default()
            }]),
        });
        app.handle_internal_event(AppEvent::ExternalAgentsRefreshed {
            source: "zcode".into(),
            ticket: 1,
            result: Ok(vec![crate::api::schema::ExternalAgentInfo {
                external_id: "zcode:s-1".into(),
                source: "zcode".into(),
                agent_status: crate::api::schema::AgentStatus::Idle,
                label: "desktop".into(),
                readable: false,
                agent: Some("zcode".into()),
                cwd: None,
                updated_at_ms: Some(5),
                activity: Vec::new(),
            }]),
        });
        snapshot(&app, "boot", 1, None, None)
    }

    /// 一个接近真实上限的活动树：4 个子 agent（2 运行中、2 已完成），各带 7 个任务，
    /// 共 32 个节点；字段按 Claude 子 agent 的量级填（id、标题、类型、内容引用、摘要、
    /// 起止时间）。
    fn bench_activity_tree() -> Vec<crate::api::schema::AgentActivityNode> {
        use crate::api::schema::{AgentActivityKind, AgentActivityNode, AgentActivityStatus};
        let mut nodes = Vec::with_capacity(32);
        for agent in 0..4u64 {
            let agent_id = format!("agent-a3f9c2e1b7d04e{agent:02}");
            let running = agent < 2;
            nodes.push(AgentActivityNode {
                id: agent_id.clone(),
                kind: AgentActivityKind::Subagent,
                label: format!("Explore the render pipeline and summarize hot paths #{agent}"),
                status: if running {
                    AgentActivityStatus::Running
                } else {
                    AgentActivityStatus::Done
                },
                parent_id: None,
                agent_type: Some("general-purpose".into()),
                content_ref: Some(format!("subagents/workflows/wf_0001/{agent_id}.jsonl")),
                summary: (!running).then(|| {
                    "Found three hot paths in compute_view; retained render early-outs hold.".into()
                }),
                started_at_ms: Some(1_758_000_000_000 + agent * 1_000),
                ended_at_ms: (!running).then_some(1_758_000_060_000 + agent * 1_000),
            });
            for task in 0..7u64 {
                let done = task < 5;
                nodes.push(AgentActivityNode {
                    id: format!("{agent_id}:task-{task}"),
                    kind: AgentActivityKind::Task,
                    label: format!("Read src/server/headless/render.rs section {task}"),
                    status: if done {
                        AgentActivityStatus::Done
                    } else {
                        AgentActivityStatus::Running
                    },
                    parent_id: Some(agent_id.clone()),
                    agent_type: None,
                    content_ref: None,
                    summary: None,
                    started_at_ms: Some(1_758_000_001_000 + agent * 1_000 + task * 100),
                    ended_at_ms: done.then_some(1_758_000_002_000 + agent * 1_000 + task * 100),
                });
            }
        }
        nodes
    }

    struct SnapshotCost {
        json_bytes: usize,
        framed_bytes: usize,
        median_us: u128,
        p95_us: u128,
    }

    /// 一次快照投影 + JSON 编码 + bincode 帧（单客户端），与 `render_scale_benchmark`
    /// 的 snapshot encoding 口径一致。
    fn measure_snapshot(app: &crate::app::App, activity: super::SnapshotActivity) -> SnapshotCost {
        const WARMUP: usize = 20;
        const SAMPLES: usize = 200;
        let run = || {
            let started = std::time::Instant::now();
            let snapshot =
                super::snapshot_with_activity(app, "bench-boot", 1, None, None, activity);
            let message = crate::protocol::endpoint::snapshot_message(&snapshot)
                .expect("benchmark snapshot should serialize");
            let json_bytes = match &message {
                crate::protocol::ServerMessage::EndpointControl { data, .. } => data.len(),
                _ => 0,
            };
            let framed = bincode::serde::encode_to_vec(&message, bincode::config::standard())
                .expect("benchmark snapshot should frame");
            (started.elapsed(), json_bytes, framed.len())
        };
        for _ in 0..WARMUP {
            std::hint::black_box(run());
        }
        let mut samples = Vec::with_capacity(SAMPLES);
        let (mut json_bytes, mut framed_bytes) = (0, 0);
        for _ in 0..SAMPLES {
            let (elapsed, json, framed) = run();
            samples.push(elapsed.as_micros());
            (json_bytes, framed_bytes) = (json, framed);
        }
        samples.sort_unstable();
        SnapshotCost {
            json_bytes,
            framed_bytes,
            median_us: samples[SAMPLES / 2],
            p95_us: samples[(SAMPLES - 1) * 95 / 100],
        }
    }

    fn percent_over(value: u128, base: u128) -> f64 {
        if base == 0 {
            return 0.0;
        }
        (value as f64 - base as f64) * 100.0 / base as f64
    }

    /// 体积护栏的实测依据（`just bench-render-scale` 会跑到它）：每个 pane 一个
    /// agent，比较「无活动」（与接入活动树之前的快照同形）、每 agent 32 节点整树
    /// 下发、每 agent 摘要（计数 + 最新节点）三种形态的快照字节与编码耗时。
    #[test]
    #[ignore = "manual snapshot size / encoding profile for agent activity"]
    fn render_scale_profile_agent_activity_snapshot() {
        println!("agent activity snapshot projection + JSON framing (32 nodes per agent)");
        println!(
            "  agents  variant  json_bytes  framed_bytes  median_us  p95_us  bytes_vs_base  median_vs_base"
        );
        for count in [1usize, 15, 50] {
            let names = (0..count)
                .map(|index| format!("bench-{index}"))
                .collect::<Vec<_>>();
            let names = names.iter().map(String::as_str).collect::<Vec<_>>();
            let mut app = app_with_panes(&names);
            let panes = app
                .state
                .workspaces
                .iter()
                .map(|workspace| workspace.tabs[0].root_pane)
                .collect::<Vec<_>>();
            for pane_id in &panes {
                detect(&mut app, *pane_id, Agent::Claude);
            }
            let base = measure_snapshot(&app, super::SnapshotActivity::Full);
            for pane_id in &panes {
                app.state.apply_agent_activity(
                    *pane_id,
                    bench_activity_tree(),
                    std::time::Instant::now(),
                );
            }
            let full = measure_snapshot(&app, super::SnapshotActivity::Full);
            let summary = measure_snapshot(&app, super::SnapshotActivity::Summary);
            for (variant, cost) in [("base", &base), ("full", &full), ("summary", &summary)] {
                println!(
                    "  {count:>6}  {variant:<7}  {:>10}  {:>12}  {:>9}  {:>6}  {:>12.1}%  {:>13.1}%",
                    cost.json_bytes,
                    cost.framed_bytes,
                    cost.median_us,
                    cost.p95_us,
                    percent_over(cost.json_bytes as u128, base.json_bytes as u128),
                    percent_over(cost.median_us, base.median_us),
                );
            }
        }
    }

    fn timed_node(
        id: &str,
        status: crate::api::schema::AgentActivityStatus,
        started: Option<u64>,
        ended: Option<u64>,
    ) -> crate::api::schema::AgentActivityNode {
        crate::api::schema::AgentActivityNode {
            id: id.into(),
            status,
            started_at_ms: started,
            ended_at_ms: ended,
            ..crate::api::schema::AgentActivityNode::default()
        }
    }

    /// 摘要选点的真源在 `app::state`（投影与「投影是否变化」的判定共用它）；
    /// 这里从投影侧钉住它的行为。
    #[test]
    fn latest_node_prefers_running_then_the_most_recent_time() {
        use crate::api::schema::AgentActivityStatus::{Done, Pending, Running};
        let nodes = vec![
            timed_node("old-running", Running, Some(10), None),
            timed_node("new-running", Running, Some(30), None),
            timed_node("done-late", Done, Some(5), Some(99)),
        ];
        assert_eq!(
            crate::app::state::latest_activity_node(&nodes).map(|node| node.id.as_str()),
            Some("new-running")
        );
        let nodes = vec![
            timed_node("done-early", Done, Some(1), Some(20)),
            timed_node("pending", Pending, Some(25), None),
            timed_node("no-time", Done, None, None),
        ];
        assert_eq!(
            crate::app::state::latest_activity_node(&nodes).map(|node| node.id.as_str()),
            Some("pending"),
            "结束时间缺失时按开始时间比"
        );
        let nodes = vec![
            timed_node("first", Done, None, None),
            timed_node("second", Done, None, None),
        ];
        assert_eq!(
            crate::app::state::latest_activity_node(&nodes).map(|node| node.id.as_str()),
            Some("second"),
            "同分取来源顺序靠后者"
        );
        assert!(crate::app::state::latest_activity_node(&[]).is_none());
    }

    /// 生产默认下发摘要，`AppState::apply_agent_activity` 的投影纪元规则（只在
    /// 摘要变化时递增）以此为前提。改回 `Full` 必须同时把那条规则改成「任意节点
    /// 变化即递增」，否则深层节点的变化不会同步给客户端。
    #[test]
    fn snapshot_activity_defaults_to_summary_per_agent() {
        assert_eq!(super::SNAPSHOT_ACTIVITY, super::SnapshotActivity::Summary);
    }

    /// 两种下发形态都只用既有 wire 字段（客户端不改就能消费）：整树带全部节点；
    /// 摘要只带计数 + 最新节点，`truncated` 告诉客户端去 `agent.activity.read` 取全量。
    #[test]
    fn snapshot_activity_shapes_share_the_wire_fields() {
        use crate::api::schema::AgentActivityStatus::{Done, Running};
        let mut app = app_with_panes(&["first"]);
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        detect(&mut app, pane_id, Agent::Claude);
        app.handle_internal_event(AppEvent::AgentActivityRefreshed {
            pane_id,
            ticket: 1,
            result: Ok(vec![
                timed_node("a", Running, Some(10), None),
                timed_node("b", Done, Some(1), Some(2)),
                timed_node("c", Running, Some(20), None),
            ]),
        });

        let full = super::snapshot_with_activity(
            &app,
            "boot",
            1,
            None,
            None,
            super::SnapshotActivity::Full,
        );
        let activity = &full.agents[0].activity;
        assert_eq!(
            (activity.running, activity.total, activity.truncated),
            (2, 3, false)
        );
        assert_eq!(
            activity
                .nodes
                .iter()
                .map(|node| node.id.as_str())
                .collect::<Vec<_>>(),
            ["a", "b", "c"]
        );

        let summary = super::snapshot_with_activity(
            &app,
            "boot",
            1,
            None,
            None,
            super::SnapshotActivity::Summary,
        );
        let activity = &summary.agents[0].activity;
        assert_eq!(
            (activity.running, activity.total, activity.truncated),
            (2, 3, true)
        );
        assert_eq!(
            activity
                .nodes
                .iter()
                .map(|node| node.id.as_str())
                .collect::<Vec<_>>(),
            ["c"]
        );
        assert_eq!(super::SNAPSHOT_ACTIVITY, super::SnapshotActivity::Summary);
        assert_eq!(snapshot(&app, "boot", 1, None, None), summary, "默认走摘要");

        // 单节点树的摘要不算截断。
        app.handle_internal_event(AppEvent::AgentActivityRefreshed {
            pane_id,
            ticket: 1,
            result: Ok(vec![timed_node("only", Running, Some(1), None)]),
        });
        let single = snapshot(&app, "boot", 1, None, None);
        assert!(!single.agents[0].activity.truncated);

        // 投影路径不把活动树复制进 AgentInfo；API 快照照常带全量。
        assert!(app.session_snapshot_for_projection().agents[0]
            .activity
            .is_empty());
        assert_eq!(app.session_snapshot().agents[0].activity.len(), 1);
    }

    /// 混版本：新 server 的快照对不认新字段的旧客户端仍可解码；旧 server 的快照
    /// （缺新字段）对新客户端一律取默认，且新结构里每个子字段都单独有默认值。
    #[test]
    fn snapshot_activity_fields_survive_mixed_version_round_trips() {
        let snapshot = snapshot_with_activity();
        assert_eq!(snapshot.agents[0].launch_seq, 1);
        assert_eq!(snapshot.agents[0].activity.nodes[0].id, "a");
        assert_eq!(snapshot.external_agents[0].external_id, "zcode:s-1");
        let json = serde_json::to_value(&snapshot).expect("编码快照");

        // 旧客户端：只认旧字段的结构（serde 默认忽略未知字段）。
        #[derive(serde::Deserialize)]
        #[allow(dead_code)] // 字段只为解码形状存在
        struct LegacyAgent {
            pane_id: String,
            agent_status: serde_json::Value,
            state_change_seq: u64,
            focused: bool,
        }
        #[derive(serde::Deserialize)]
        #[allow(dead_code)] // 字段只为解码形状存在
        struct LegacySnapshot {
            boot_id: String,
            revision: u64,
            agents: Vec<LegacyAgent>,
            commands: Vec<serde_json::Value>,
        }
        let legacy: LegacySnapshot =
            serde_json::from_value(json.clone()).expect("旧客户端解码新快照");
        assert_eq!(legacy.agents.len(), 1);

        // 旧 server：去掉全部新键后，新客户端解码取默认。
        let mut stripped = json.clone();
        stripped
            .as_object_mut()
            .expect("对象")
            .remove("external_agents");
        for agent in stripped["agents"].as_array_mut().expect("数组") {
            let agent = agent.as_object_mut().expect("对象");
            agent.remove("launch_seq");
            agent.remove("activity");
        }
        let decoded: crate::protocol::ClientShellSnapshot =
            serde_json::from_value(stripped).expect("新客户端解码旧快照");
        assert!(decoded.external_agents.is_empty());
        assert_eq!(decoded.agents[0].launch_seq, 0);
        assert_eq!(
            decoded.agents[0].activity,
            crate::protocol::ClientShellAgentActivity::default()
        );

        // 子字段各自带默认：只剩必填键也能解码。
        let mut sparse = json;
        sparse["agents"][0]["activity"] = serde_json::json!({ "nodes": [{ "id": "x" }] });
        sparse["external_agents"] = serde_json::json!([{
            "external_id": "zcode:s-2",
            "source": "zcode",
            "agent_status": "idle"
        }]);
        let decoded: crate::protocol::ClientShellSnapshot =
            serde_json::from_value(sparse).expect("稀疏字段可解码");
        let activity = &decoded.agents[0].activity;
        assert_eq!(
            (activity.running, activity.total, activity.truncated),
            (0, 0, false)
        );
        assert_eq!(
            activity.nodes[0].kind,
            crate::api::schema::AgentActivityKind::Unknown
        );
        let external = &decoded.external_agents[0];
        assert!(external.readable, "readable 缺省为可读");
        assert_eq!(external.label, "");
        assert_eq!(
            external.activity,
            crate::protocol::ClientShellAgentActivity::default()
        );
    }
}
