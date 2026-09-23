//! 停靠工作台只管理客户端几何；每个可见标签由服务端独立投影。

pub(in crate::client::shell) mod interaction;
mod render;

use super::dock::{DockLayout, Geometry, PanelId};
use super::*;
use crate::api::schema::{ClientViewSpec, ClientViewsSetParams, Method};
use std::collections::HashSet;
use std::time::{Duration, Instant};

pub(super) struct View {
    pub tab: String,
    pub surface: PaneSurfaceFrame,
    pub graphics: crate::kitty_graphics::surface::ClientState,
}

pub(super) struct State {
    pub enabled: bool,
    pub dock: DockLayout,
    pub saved: HashMap<String, DockLayout>,
    pub source: String,
    pub boot: String,
    pub revision: u64,
    pub acknowledged: u64,
    pub requested: Vec<ClientViewSpec>,
    pub views: HashMap<String, View>,
    pub geometry: Geometry,
    pub(in crate::client::shell) hits: Vec<(Rect, interaction::Action)>,
    /// 「调整布局」页脚里可点提示的 `(命中矩形, 提示下标)`，每帧重建；鼠标移动
    /// 据此算出 `footer_hover`（复审轻级 B2）。
    pub(in crate::client::shell) footer_hits: Vec<(Rect, usize)>,
    /// 指针悬浮的页脚提示下标，只会是 `footer_hits` 里的可点项。
    pub(in crate::client::shell) footer_hover: Option<usize>,
    drag: Option<interaction::Drag>,
    pub arranging: bool,
    pub tab_scroll: HashMap<u64, usize>,
    pub tab_focus: HashMap<u64, (Option<String>, u16)>,
    pub cleanup: Vec<u8>,
    pub pending: bool,
    next_request: Instant,
    last_focus: Option<String>,
}

/// 工作台的整帧内容区：顶栏（首行）与页脚 / 模式条（末行）之间。停靠几何与
/// 不属于任何面板的浮层（which-key）都以它为边界。
pub(super) fn content_area(cols: u16, rows: u16) -> Rect {
    Rect::new(0, 1.min(rows), cols, rows.saturating_sub(2))
}

pub(super) fn body(area: Rect, panel: &PanelId) -> Rect {
    let header = if matches!(panel, PanelId::Terminal(_)) {
        2
    } else {
        1
    };
    Rect::new(
        area.x,
        area.y.saturating_add(header),
        area.width,
        area.height.saturating_sub(header),
    )
}

impl State {
    pub fn new(config: &ClientShellConfig) -> Self {
        Self {
            enabled: false,
            dock: DockLayout::default(),
            saved: config
                .preferences
                .layouts
                .iter()
                .filter_map(|(key, layout)| {
                    let mut layout = layout.clone();
                    layout.discard_accounts_panel();
                    layout.valid().then(|| (key.clone(), layout))
                })
                .collect(),
            source: String::new(),
            boot: String::new(),
            revision: 0,
            acknowledged: 0,
            requested: Vec::new(),
            views: HashMap::new(),
            geometry: Geometry::default(),
            hits: Vec::new(),
            footer_hits: Vec::new(),
            footer_hover: None,
            drag: None,
            arranging: false,
            tab_scroll: HashMap::new(),
            tab_focus: HashMap::new(),
            cleanup: Vec::new(),
            pending: false,
            next_request: Instant::now(),
            last_focus: None,
        }
    }

    pub fn saved_layouts(&self) -> HashMap<String, DockLayout> {
        let mut saved = self.saved.clone();
        if !self.source.is_empty() && self.dock.valid() {
            saved.insert(self.source.clone(), self.dock.clone());
        }
        saved
    }

    pub fn disconnect(&mut self) {
        self.saved = self.saved_layouts();
        self.enabled = false;
        self.retire_views();
        self.source.clear();
        self.boot.clear();
        self.requested.clear();
        // 连接仍可能保留先前的解码器；切换主机或恢复 surface 不倒退版本。
        self.acknowledged = 0;
        self.pending = false;
        self.drag = None;
        self.last_focus = None;
    }

    fn retire_views(&mut self) {
        for (_, mut view) in self.views.drain() {
            view.graphics.set_scope("");
            self.cleanup.extend(view.graphics.take_pending_cleanup());
        }
    }

    pub fn layout(&self, cols: u16, rows: u16) -> ClientShellLayout {
        let geometry = self.dock.geometry(content_area(cols, rows));
        let area = geometry
            .panels
            .iter()
            .find(|(id, _)| *id == self.dock.focused)
            .or_else(|| {
                geometry
                    .panels
                    .iter()
                    .find(|(id, _)| matches!(id, PanelId::Terminal(_)))
            })
            .map(|(id, area)| body(*area, id))
            .unwrap_or_default();
        ClientShellLayout {
            sidebar: Rect::default(),
            tab_bar: Rect::default(),
            mobile_header: Rect::default(),
            pane_surface: area,
        }
    }

    pub fn visible(&self, target: &PanelId) -> bool {
        self.enabled
            && self
                .geometry
                .panels
                .iter()
                .any(|(panel, _)| panel == target)
    }

    pub fn focused_view(&self) -> Option<&View> {
        let PanelId::Terminal(id) = self.dock.focused else {
            return None;
        };
        self.views.get(&id.to_string()).filter(|view| {
            self.dock
                .groups
                .iter()
                .any(|group| group.id == id && group.active.as_ref() == Some(&view.tab))
        })
    }
}

fn set_sidebar(dock: &mut DockLayout, collapsed: bool) {
    let locked = dock.locked;
    dock.locked = false;
    if collapsed {
        dock.close_panel(&PanelId::Workspaces);
        dock.close_panel(&PanelId::Agents);
    } else if let Some(group) = dock.groups.first() {
        let target = PanelId::Terminal(group.id);
        if !dock.root.contains(&PanelId::Workspaces) {
            dock.dock(PanelId::Workspaces, &target, dock::Edge::Left);
        }
        if !dock.root.contains(&PanelId::Agents) {
            dock.dock(PanelId::Agents, &PanelId::Workspaces, dock::Edge::Bottom);
        }
        dock.focused = target;
    }
    dock.locked = locked;
}

impl ClientShellState {
    pub(super) fn default_dock(&self) -> DockLayout {
        let cols = self.last_composed_size.map_or(120, |size| size.0).max(1);
        DockLayout::from_sidebar(
            f32::from(self.sidebar_width) / f32::from(cols),
            self.sidebar_section_split.max(0.6),
        )
    }

    pub(super) fn workbench_sidebar(&mut self, collapsed: bool) {
        set_sidebar(&mut self.workbench.dock, collapsed);
        self.sync_observation_page_with_focus();
    }

    pub(super) fn focused_tab_id(&self) -> Option<String> {
        if self.workbench.enabled {
            let PanelId::Terminal(id) = self.workbench.dock.focused else {
                return None;
            };
            return self
                .workbench
                .dock
                .groups
                .iter()
                .find(|group| group.id == id)?
                .active
                .clone();
        }
        self.snapshot.as_ref()?.focused_tab_id.clone()
    }

    pub(super) fn focused_workspace_id(&self) -> Option<String> {
        let snapshot = self.snapshot.as_ref()?;
        if !self.workbench.enabled {
            return snapshot.focused_workspace_id.clone();
        }
        self.focused_tab_id()
            .and_then(|id| {
                snapshot
                    .tabs
                    .iter()
                    .find(|tab| tab.tab_id == id)
                    .map(|tab| tab.workspace_id.clone())
            })
            .or_else(|| snapshot.focused_workspace_id.clone())
    }

    /// 宿主终端 resize 后是否要立即在本地组合一帧：停靠工作台的几何只取决于
    /// 终端尺寸，不等服务端（见 `ClientState::present_after_resize`）。
    pub(crate) fn composes_on_host_resize(&self) -> bool {
        self.workbench.enabled
    }

    pub(crate) fn renew_workbench_surface(&mut self) {
        self.workbench.disconnect();
    }

    /// 分组销毁后回收它的标签页视口状态：`tab_scroll` / `tab_focus` 按分组 id
    /// 索引，不过滤就会随开关面板与工作区切换无限增长（LEAK-01）。
    fn prune_tab_view_state(&mut self) {
        let live = self
            .workbench
            .dock
            .groups
            .iter()
            .map(|group| group.id)
            .collect::<HashSet<u64>>();
        self.workbench.tab_scroll.retain(|id, _| live.contains(id));
        self.workbench.tab_focus.retain(|id, _| live.contains(id));
    }

    pub(crate) fn tick_workbench(&mut self, now: Instant, outcome: &mut ClientShellInput) {
        if !self.endpoint_is_online(&self.active_endpoint_id) {
            return;
        }
        let supported = self
            .endpoints
            .iter()
            .find(|endpoint| endpoint.endpoint_id == self.active_endpoint_id)
            .and_then(|endpoint| endpoint.methods.as_ref())
            .is_some_and(|methods| methods.contains("client.views.set"));
        if !supported {
            return;
        }
        let Some(snapshot) = self.snapshot.as_ref() else {
            return;
        };
        let source = self.active_endpoint_id.storage_key();
        let restored = self.workbench.source != source || self.workbench.boot != snapshot.boot_id;
        if restored {
            // `disconnect` 会清掉 enabled；布局恢复完成后在下方统一置位。
            self.workbench.disconnect();
            self.workbench.dock = self
                .workbench
                .saved
                .get(&source)
                .cloned()
                .unwrap_or_else(|| self.default_dock());
            self.workbench.source = source;
            self.workbench.boot.clone_from(&snapshot.boot_id);
            self.workbench.next_request = now;
            if self.sidebar_collapsed {
                set_sidebar(&mut self.workbench.dock, true);
            }
        }
        self.workbench.enabled = true;
        if restored {
            // 恢复的布局可能以监控面板为焦点；页面归属随之同步（必须在置位
            // enabled 之后，`sync_observation_page` 在工作台未启用时早退）。
            interaction::sync_observation_page(&self.workbench, &mut self.observability);
        }
        let tabs = snapshot
            .tabs
            .iter()
            .map(|tab| tab.tab_id.clone())
            .collect::<Vec<_>>();
        // The primary terminal strip mirrors the focused workspace only, so
        // each workspace keeps an independent terminal page and clicking a
        // workspace swaps both the strip and its terminal content.
        let focused_workspace_tabs = match snapshot.focused_workspace_id.as_deref() {
            Some(workspace) => snapshot
                .tabs
                .iter()
                .filter(|tab| tab.workspace_id == workspace)
                .map(|tab| tab.tab_id.clone())
                .collect::<Vec<_>>(),
            None => tabs.clone(),
        };
        let changed_focus = self.workbench.last_focus != snapshot.focused_tab_id;
        outcome.repaint |= self.workbench.dock.reconcile_workspace_tabs(
            &focused_workspace_tabs,
            &tabs,
            // Re-anchor the active tab only when the server focus actually
            // moved, so a local tab click stays ahead of server confirmation.
            changed_focus
                .then_some(snapshot.focused_tab_id.as_deref())
                .flatten(),
        );
        if changed_focus {
            if let Some(group) = self
                .workbench
                .dock
                .groups
                .iter()
                .find(|group| group.active == snapshot.focused_tab_id && group.active.is_some())
            {
                if matches!(
                    self.workbench.dock.focused,
                    PanelId::Terminal(_) | PanelId::Workspaces | PanelId::Agents
                ) {
                    self.workbench.dock.focused = PanelId::Terminal(group.id);
                }
            }
            self.workbench
                .last_focus
                .clone_from(&snapshot.focused_tab_id);
        }
        self.prune_tab_view_state();
        let Some((cols, rows)) = self.last_composed_size else {
            return;
        };
        let geometry = self.workbench.dock.geometry(content_area(cols, rows));
        let mut views = geometry
            .panels
            .iter()
            .filter_map(|(panel, area)| {
                let PanelId::Terminal(id) = panel else {
                    return None;
                };
                let tab = self
                    .workbench
                    .dock
                    .groups
                    .iter()
                    .find(|group| group.id == *id)?
                    .active
                    .as_ref()?;
                let area = body(*area, panel);
                (!area.is_empty()).then(|| ClientViewSpec {
                    view_id: id.to_string(),
                    tab_id: tab.clone(),
                    cols: area.width,
                    rows: area.height,
                    focused: panel == &self.workbench.dock.focused,
                })
            })
            .collect::<Vec<_>>();
        if !views.iter().any(|view| view.focused) {
            if let Some(view) = views.first_mut() {
                view.focused = true;
            }
        }
        self.workbench.geometry = geometry;
        if self.workbench.pending
            || now < self.workbench.next_request
            || (views == self.workbench.requested
                && self.workbench.acknowledged == self.workbench.revision
                && self.workbench.revision != 0)
        {
            return;
        }
        let revision = self.workbench.revision.saturating_add(1);
        if self.push_endpoint_method_with_kind(
            Method::ClientViewsSet(ClientViewsSetParams {
                revision,
                views: views.clone(),
            }),
            PendingEndpointKind::Views { revision },
            outcome,
        ) {
            self.workbench.retire_views();
            self.workbench.requested = views;
            self.workbench.revision = revision;
            self.workbench.pending = true;
            self.workbench.next_request = now + Duration::from_millis(75);
            outcome.repaint = true;
        }
    }

    pub(crate) fn receive_view(
        &mut self,
        generation: u64,
        view: crate::protocol::views::DecodedView,
    ) -> bool {
        if !self.workbench.enabled
            || self.active_snapshot_generation != Some(generation)
            || view.boot_id != self.workbench.boot
            || view.views_revision != self.workbench.revision
            || !self
                .workbench
                .requested
                .iter()
                .any(|spec| spec.view_id == view.view_id && spec.tab_id == view.tab_id)
        {
            return false;
        }
        match view.message {
            crate::protocol::ServerMessage::PaneSurface(surface) => {
                if surface.frame.width == 0 || surface.frame.height == 0 {
                    return false;
                }
                let mut graphics = self
                    .workbench
                    .views
                    .remove(&view.view_id)
                    .map(|view| view.graphics)
                    .unwrap_or_default();
                graphics.set_scope(&format!(
                    "{}:{}:{}",
                    self.workbench.source, self.workbench.boot, view.view_id
                ));
                graphics.set_scene(surface.graphics.clone());
                self.workbench.views.insert(
                    view.view_id,
                    View {
                        tab: view.tab_id,
                        surface,
                        graphics,
                    },
                );
            }
            crate::protocol::ServerMessage::PaneSurfacePatch(patch) => {
                let Some(current) = self.workbench.views.get_mut(&view.view_id) else {
                    return false;
                };
                if current.surface.surface_revision != patch.base_surface_revision
                    || current.surface.projection_revision != patch.projection_revision
                    || patch.surface_revision != patch.base_surface_revision.saturating_add(1)
                {
                    return false;
                }
                // 先验证整包，避免无效行留下部分写入的画面。
                if patch.rows.iter().any(|row| {
                    row.y >= current.surface.frame.height
                        || usize::from(row.x) + row.cells.len()
                            > usize::from(current.surface.frame.width)
                }) || patch.panes.iter().any(|pane| {
                    !current.surface.panes.iter().any(|old| {
                        old.pane_id == pane.pane_id
                            && old.rect == pane.rect
                            && old.inner_rect == pane.inner_rect
                    })
                }) {
                    return false;
                }
                if !super::surface_patch::apply_patch_to_surface(&mut current.surface, &patch) {
                    return false;
                }
            }
            _ => return false,
        }
        true
    }

    pub(crate) fn view_request(&self, request: ClientMessage) -> Option<ClientMessage> {
        if !self.workbench.enabled {
            return Some(request);
        }
        let ClientMessage::ClientShellPaneInput { pane_id, events } = request else {
            return Some(request);
        };
        let tab = self
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.panes.iter().find(|pane| pane.pane_id == pane_id))
            .map(|pane| pane.tab_id.as_str());
        let spec = self
            .workbench
            .requested
            .iter()
            .find(|spec| Some(spec.tab_id.as_str()) == tab);
        let spec = spec?;
        let data = serde_json::to_string(&crate::protocol::views::ViewInput {
            boot_id: self.workbench.boot.clone(),
            views_revision: self.workbench.revision,
            view_id: spec.view_id.clone(),
            tab_id: spec.tab_id.clone(),
            pane_id,
            events,
        })
        .ok()?;
        Some(ClientMessage::EndpointControl {
            kind: crate::protocol::views::INPUT_KIND.into(),
            data,
        })
    }

    pub(super) fn workbench_open(&mut self, panel: PanelId) {
        if !self.workbench.enabled {
            return;
        }
        if !self.workbench.dock.root.contains(&panel) {
            let target = self.workbench.dock.focused.clone();
            let locked = self.workbench.dock.locked;
            self.workbench.dock.locked = false;
            self.workbench
                .dock
                .dock(panel.clone(), &target, dock::Edge::Right);
            self.workbench.dock.locked = locked;
        }
        self.workbench.dock.focused = panel;
        self.workbench.dock.maximized = None;
        self.sync_observation_page_with_focus();
    }
}
