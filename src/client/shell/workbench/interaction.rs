use super::*;
use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use dock::{Axis, Divider, Edge};

// 可见性放宽到 shell 层：`tests::workbench` 需要按动作类型定位命中矩形，才能
// 为「哪些点击会落盘」写回归护栏（PERF-01）。
#[derive(Clone)]
pub(in crate::client::shell) enum Action {
    Menu,
    Open(PanelId),
    Close(PanelId),
    Arrange,
    Lock,
    Reset,
    Header(PanelId),
    Maximize(PanelId),
    NewTab(u64),
    ScrollTabs(u64, isize),
    Pane(String),
    Tab { group: u64, tab: String },
}

pub(super) enum Drag {
    Divider(Divider),
    Pane {
        pane: String,
        point: (u16, u16),
    },
    Move {
        panel: PanelId,
        tab: Option<String>,
        start: (u16, u16),
        point: (u16, u16),
        moved: bool,
    },
}

fn drop_edge(area: Rect, point: (u16, u16)) -> Option<Edge> {
    let x = point.0.saturating_sub(area.x);
    let y = point.1.saturating_sub(area.y);
    if x < area.width / 4 {
        Some(Edge::Left)
    } else if x >= area.width.saturating_mul(3) / 4 {
        Some(Edge::Right)
    } else if y < area.height / 4 {
        Some(Edge::Top)
    } else if y >= area.height.saturating_mul(3) / 4 {
        Some(Edge::Bottom)
    } else {
        None
    }
}

pub(super) fn preview(area: Rect, edge: Option<Edge>) -> Rect {
    match edge {
        Some(Edge::Left) => Rect::new(area.x, area.y, area.width / 2, area.height),
        Some(Edge::Right) => Rect::new(
            area.x + area.width / 2,
            area.y,
            area.width - area.width / 2,
            area.height,
        ),
        Some(Edge::Top) => Rect::new(area.x, area.y, area.width, area.height / 2),
        Some(Edge::Bottom) => Rect::new(
            area.x,
            area.y + area.height / 2,
            area.width,
            area.height - area.height / 2,
        ),
        None => area,
    }
}

impl Drag {
    pub fn drop_target(&self, geometry: &Geometry) -> Option<(PanelId, Option<Edge>)> {
        let Self::Move {
            point, moved: true, ..
        } = self
        else {
            return None;
        };
        geometry
            .panels
            .iter()
            .find(|(_, area)| contains(*area, *point))
            .map(|(panel, area)| (panel.clone(), drop_edge(*area, *point)))
    }
}

/// `sync_observation_page_with_focus` 的字段级实现，供持有 `snapshot` 借用的
/// tick 路径调用。
pub(super) fn sync_observation_page(
    workbench: &super::State,
    observability: &mut super::super::observability::State,
) {
    if !workbench.enabled {
        return;
    }
    observability.page = match workbench.dock.focused {
        PanelId::Monitor => Some(observability.monitor_tab),
        PanelId::Accounts => Some(super::super::observability::Page::Accounts),
        _ => None,
    };
}

impl ClientShellState {
    fn publish_workbench_focus(
        &mut self,
        previous: Option<String>,
        outcome: &mut ClientShellInput,
    ) {
        let focused = self.focused_tab_id();
        if focused != previous {
            if let Some(tab_id) = focused {
                self.push_endpoint_method(
                    Method::TabFocus(crate::api::schema::TabTarget { tab_id }),
                    outcome,
                );
            }
        }
    }

    fn focus_workbench_panel(&mut self, panel: PanelId) {
        if self.workbench.dock.focused == panel {
            return;
        }
        self.workbench.dock.focused = panel;
        self.cancel_frozen_selection();
        self.selection = None;
        self.clear_link_hover();
        // 焦点离开监控面板只是键盘归终端；用户选中的 tab（monitor_tab）保留，
        // 面板继续画同一页并继续轮询。
        self.sync_observation_page_with_focus();
    }

    /// 停靠工作台下 `observability.page` 与 `dock.focused` 同步：监控 / 账号面板
    /// 聚焦时页面拥有键盘，否则键盘归终端。所有改写 `dock.focused` 的入口在
    /// 输入 / tick 阶段调用，渲染期不再改写。
    pub(in crate::client::shell) fn sync_observation_page_with_focus(&mut self) {
        sync_observation_page(&self.workbench, &mut self.observability);
    }

    /// 关闭一个停靠面板（Esc / 命令面板的关闭动作）。锁定布局时不关闭，而是
    /// 在监控页脚给出反馈；成功后焦点回到终端、上报一次焦点并持久化布局。
    /// 面板头 × 走 `workbench_mouse`，由该命中派发块统一收尾，调用的是
    /// `close_workbench_panel_state`，避免同一次关闭重复上报焦点与写偏好。
    pub(in crate::client::shell) fn close_workbench_panel(
        &mut self,
        panel: PanelId,
        outcome: &mut ClientShellInput,
    ) -> bool {
        let previous_tab = self.focused_tab_id();
        if !self.close_workbench_panel_state(panel, outcome) {
            return false;
        }
        self.publish_workbench_focus(previous_tab, outcome);
        // 每次手势最多一次写：保持即时落盘，写失败时经 `outcome` 上报错误并
        // 重绘（去抖路径拿不到这份反馈）。
        self.persist_chrome_preferences(outcome);
        true
    }

    /// `close_workbench_panel` 的只改状态部分：不上报焦点、不写偏好，由调用方
    /// 统一收尾。
    fn close_workbench_panel_state(
        &mut self,
        panel: PanelId,
        outcome: &mut ClientShellInput,
    ) -> bool {
        if !self.workbench.enabled {
            return false;
        }
        if self.workbench.dock.locked {
            self.observability.message = Some(
                super::super::observability::tr(
                    "Layout is locked; unlock it to close this panel.",
                    "布局已锁定，解锁后才能关闭面板。",
                )
                .into(),
            );
            outcome.repaint = true;
            return false;
        }
        if !self.workbench.dock.close_panel(&panel) {
            return false;
        }
        if matches!(panel, PanelId::Monitor | PanelId::Accounts) {
            // hover 与悬浮层作用域同生共死：统一走 clear_hover，不留残留数据与
            // 在途请求。
            self.observability.clear_hover();
            self.observability.process_dialog = None;
        }
        self.cancel_frozen_selection();
        self.selection = None;
        self.clear_link_hover();
        self.sync_observation_page_with_focus();
        outcome.repaint = true;
        true
    }

    pub(in crate::client::shell) fn workbench_mouse(
        &mut self,
        mouse: MouseEvent,
        outcome: &mut ClientShellInput,
    ) -> bool {
        if !self.workbench.enabled
            || !self.config.mouse_capture
            || self.overlay.is_some()
            || self.observability.process_dialog.is_some()
        {
            self.workbench.drag = None;
            return false;
        }
        let point = (mouse.column, mouse.row);
        // 已捕获的拖动保持归属；新点击优先交给最上层的提示操作。
        if self.workbench.drag.is_none()
            && [
                self.hits.notification_toast,
                self.hits.lifecycle_banner_retry,
                self.hits.lifecycle_banner_give_up,
            ]
            .iter()
            .any(|rect| contains(*rect, point))
        {
            return false;
        }
        let previous_tab = self.focused_tab_id();
        if let Some(mut drag) = self.workbench.drag.take() {
            match mouse.kind {
                MouseEventKind::Drag(MouseButton::Left) => {
                    match &mut drag {
                        Drag::Pane { point: last, .. } => *last = point,
                        Drag::Divider(divider) => {
                            let (offset, total) = if divider.axis == Axis::Horizontal {
                                (point.0.saturating_sub(divider.area.x), divider.area.width)
                            } else {
                                (point.1.saturating_sub(divider.area.y), divider.area.height)
                            };
                            self.workbench.dock.resize(
                                &divider.path,
                                f32::from(offset) / f32::from(total.saturating_sub(1).max(1)),
                            );
                        }
                        Drag::Move {
                            start,
                            point: last,
                            moved,
                            ..
                        } => {
                            *last = point;
                            *moved |=
                                start.0.abs_diff(point.0) > 1 || start.1.abs_diff(point.1) > 1;
                        }
                    }
                    self.workbench.drag = Some(drag);
                }
                MouseEventKind::Up(MouseButton::Left) => {
                    if let Drag::Pane { pane, .. } = &drag {
                        self.drop_workbench_pane(pane.clone(), point, outcome);
                    }
                    let target = drag.drop_target(&self.workbench.geometry);
                    if let (Drag::Move { panel, tab, .. }, Some((target, edge))) = (drag, target) {
                        if let Some(tab) = tab {
                            if let PanelId::Terminal(group) = target {
                                let index = self
                                    .workbench
                                    .hits
                                    .iter()
                                    .find_map(|(rect, action)| {
                                        if !contains(*rect, point) {
                                            return None;
                                        }
                                        let Action::Tab {
                                            group: hit_group,
                                            tab: target,
                                        } = action
                                        else {
                                            return None;
                                        };
                                        (*hit_group == group)
                                            .then(|| {
                                                self.workbench
                                                    .dock
                                                    .groups
                                                    .iter()
                                                    .find(|entry| entry.id == group)
                                                    .and_then(|entry| {
                                                        entry
                                                            .tabs
                                                            .iter()
                                                            .position(|id| id == target)
                                                    })
                                            })
                                            .flatten()
                                    })
                                    .unwrap_or(usize::MAX);
                                // 标签栏内拖动是归组/重排，内容边缘拖动才创建新组。
                                let in_tabs = self.workbench.hits.iter().any(|(rect, action)| {
                                    matches!(action, Action::Tab { .. }) && contains(*rect, point)
                                });
                                self.workbench.dock.move_tab(
                                    &tab,
                                    group,
                                    index,
                                    if in_tabs { None } else { edge },
                                );
                            }
                        } else {
                            self.workbench
                                .dock
                                .dock(panel, &target, edge.unwrap_or(Edge::Right));
                        }
                    }
                    // 拖拽释放每次手势只写一次，保持即时语义与错误反馈。
                    self.persist_chrome_preferences(outcome);
                }
                _ => {
                    self.workbench.drag = Some(drag);
                }
            }
            self.sync_observation_page_with_focus();
            self.publish_workbench_focus(previous_tab, outcome);
            outcome.repaint = true;
            return true;
        }
        // 悬浮层内部与「用量」按钮上的事件透传给 observability：按钮上的按下是
        // 钉住 / 取消钉住总览，不能在这里清 hover，也不能落到下方的面板聚焦兜底
        // （那会顺带把监控页 `page` 清空）。
        if contains(self.observability.hover_rect, point)
            || contains(self.hits.agent_usage_toggle, point)
        {
            return false;
        }
        if matches!(mouse.kind, MouseEventKind::Down(_)) {
            self.observability.clear_hover();
            self.observability.hover_rect = Rect::default();
            self.observability.hover_hits.clear();
        }
        if matches!(
            mouse.kind,
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp
        ) {
            if let Some((_, Action::Tab { group, .. })) =
                self.workbench.hits.iter().find(|(rect, action)| {
                    matches!(action, Action::Tab { .. }) && contains(*rect, point)
                })
            {
                let scroll = self.workbench.tab_scroll.entry(*group).or_default();
                *scroll = if mouse.kind == MouseEventKind::ScrollDown {
                    scroll.saturating_add(1)
                } else {
                    scroll.saturating_sub(1)
                };
                outcome.repaint = true;
                return true;
            }
        }
        if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
            return false;
        }
        if !self.workbench.dock.locked {
            if let Some(divider) = self
                .workbench
                .geometry
                .dividers
                .iter()
                .find(|divider| contains(divider.handle, point))
                .cloned()
            {
                self.workbench.drag = Some(Drag::Divider(divider));
                return true;
            }
        }
        let action = self
            .workbench
            .hits
            .iter()
            .rev()
            .find(|(rect, _)| contains(*rect, point))
            .map(|(_, action)| action.clone());
        if let Some(action) = action {
            let creates_tab = matches!(&action, Action::NewTab(_));
            // 判据是「是否会写入 `ClientChromePreferences`」而不是「是否呈现
            // 状态」：标签滚动、全局菜单、`arranging` 开关与开始拖 pane 都不进
            // `DockLayout` / `saved_layouts()`，标脏只会换来一次无效全量写
            // （PERF-01）。
            let dirty = !matches!(
                action,
                Action::ScrollTabs(..) | Action::Menu | Action::Arrange | Action::Pane(_)
            );
            match action {
                Action::ScrollTabs(group, delta) => {
                    let scroll = self.workbench.tab_scroll.entry(group).or_default();
                    *scroll = scroll.saturating_add_signed(delta);
                }
                Action::Menu => self.toggle_global_menu(),
                Action::Close(panel) => {
                    // 焦点上报与偏好持久化由本派发块末尾统一完成。
                    self.close_workbench_panel_state(panel, outcome);
                }
                Action::Open(panel) => self.open_observation_page(
                    if panel == PanelId::Monitor {
                        super::super::observability::Page::Monitor
                    } else {
                        super::super::observability::Page::Accounts
                    },
                    outcome,
                ),
                Action::Arrange => self.workbench.arranging = !self.workbench.arranging,
                Action::Lock => self.workbench.dock.locked = !self.workbench.dock.locked,
                Action::Reset => {
                    self.workbench.dock = self.default_dock();
                    self.workbench.last_focus = None;
                }
                Action::Maximize(panel) => {
                    self.focus_workbench_panel(panel.clone());
                    self.workbench.dock.maximized = if self.workbench.dock.maximized.is_some() {
                        None
                    } else {
                        Some(panel)
                    };
                }
                Action::Header(panel) => {
                    self.focus_workbench_panel(panel.clone());
                    if !self.workbench.dock.locked {
                        self.workbench.drag = Some(Drag::Move {
                            panel,
                            tab: None,
                            start: point,
                            point,
                            moved: false,
                        });
                    }
                }
                Action::Tab { group, tab } => {
                    if let Some(entry) = self
                        .workbench
                        .dock
                        .groups
                        .iter_mut()
                        .find(|entry| entry.id == group)
                    {
                        entry.active = Some(tab.clone());
                    }
                    self.focus_workbench_panel(PanelId::Terminal(group));
                    if !self.workbench.dock.locked {
                        self.workbench.drag = Some(Drag::Move {
                            panel: PanelId::Terminal(group),
                            tab: Some(tab),
                            start: point,
                            point,
                            moved: false,
                        });
                    }
                }
                Action::Pane(pane) => {
                    if !self.workbench.dock.locked {
                        self.workbench.drag = Some(Drag::Pane { pane, point });
                    }
                }
                Action::NewTab(group) => {
                    self.focus_workbench_panel(PanelId::Terminal(group));
                    let workspace_id = self
                        .workbench
                        .dock
                        .groups
                        .iter()
                        .find(|entry| entry.id == group)
                        .and_then(|entry| entry.active.as_ref())
                        .and_then(|tab| {
                            self.snapshot
                                .as_ref()?
                                .tabs
                                .iter()
                                .find(|entry| &entry.tab_id == tab)
                        })
                        .map(|entry| entry.workspace_id.clone());
                    self.push_endpoint_method(
                        Method::TabCreate(crate::api::schema::TabCreateParams {
                            workspace_id,
                            cwd: None,
                            focus: true,
                            label: None,
                            env: HashMap::new(),
                        }),
                        outcome,
                    );
                }
            }
            self.sync_observation_page_with_focus();
            if !creates_tab {
                self.publish_workbench_focus(previous_tab, outcome);
            }
            if dirty {
                self.schedule_chrome_preferences(std::time::Instant::now());
            }
            outcome.repaint = true;
            return true;
        }
        if let Some((panel, _)) = self
            .workbench
            .geometry
            .panels
            .iter()
            .find(|(_, area)| contains(*area, point))
            .cloned()
        {
            self.focus_workbench_panel(panel);
            self.publish_workbench_focus(previous_tab, outcome);
            outcome.repaint = true;
        }
        false
    }

    fn drop_workbench_pane(
        &mut self,
        pane: String,
        point: (u16, u16),
        outcome: &mut ClientShellInput,
    ) {
        use crate::api::schema::{
            PaneMoveDestination, PaneMoveParams, PaneSwapParams, SplitDirection,
        };
        let Some(snapshot) = self.snapshot.as_ref() else {
            return;
        };
        let Some(source) = snapshot.panes.iter().find(|item| item.pane_id == pane) else {
            return;
        };
        let target = self
            .hits
            .panes
            .iter()
            .find(|hit| hit.pane_id != pane && contains(hit.rect, point));
        let target_tab = target
            .and_then(|hit| {
                snapshot
                    .panes
                    .iter()
                    .find(|item| item.pane_id == hit.pane_id)
            })
            .map(|item| item.tab_id.clone())
            .or_else(|| {
                self.workbench
                    .hits
                    .iter()
                    .find_map(|(rect, action)| match action {
                        Action::Tab { tab, .. } if contains(*rect, point) => Some(tab.clone()),
                        _ => None,
                    })
            });
        let Some(tab) = target_tab else {
            return;
        };
        let method = if tab == source.tab_id {
            let Some(target) = target else {
                return;
            };
            Method::PaneSwap(PaneSwapParams {
                source_pane_id: Some(pane),
                target_pane_id: Some(target.pane_id.clone()),
                ..Default::default()
            })
        } else {
            let vertical = target.is_some_and(|target| {
                matches!(
                    drop_edge(target.rect, point),
                    Some(Edge::Top | Edge::Bottom)
                )
            });
            Method::PaneMove(PaneMoveParams {
                pane_id: pane,
                destination: PaneMoveDestination::Tab {
                    tab_id: tab,
                    target_pane_id: target.map(|target| target.pane_id.clone()),
                    split: if vertical {
                        SplitDirection::Down
                    } else {
                        SplitDirection::Right
                    },
                    ratio: Some(0.5),
                },
                focus: true,
            })
        };
        self.push_endpoint_method(method, outcome);
    }

    pub(in crate::client::shell) fn workbench_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) -> bool {
        if !self.workbench.enabled || !self.workbench.arranging || self.overlay.is_some() {
            return false;
        }
        let previous_tab = self.focused_tab_id();
        // 去最大化投影：最大化时 `geometry` 只有一个面板、零分隔线，Tab /
        // 方向键会静默失效（WB-01）。
        let geometry = self
            .workbench
            .dock
            .layout_geometry(Rect::new(0, 0, 4096, 4096));
        // 只有真正改写了布局的按键才标脏落盘：未处理按键与 Esc（`arranging`
        // 不入偏好）保持零磁盘写（PERF-01）。
        let mut dirty = false;
        match key.code {
            KeyCode::Esc => self.workbench.arranging = false,
            KeyCode::Tab | KeyCode::BackTab => {
                let panels = geometry
                    .panels
                    .iter()
                    .map(|(panel, _)| panel.clone())
                    .collect::<Vec<_>>();
                if let Some(index) = panels
                    .iter()
                    .position(|panel| panel == &self.workbench.dock.focused)
                {
                    let reverse =
                        key.code == KeyCode::BackTab || key.modifiers.contains(KeyModifiers::SHIFT);
                    let next = if reverse {
                        (index + panels.len() - 1) % panels.len()
                    } else {
                        (index + 1) % panels.len()
                    };
                    // 只有一个面板时 `next == index`，轮转是彻底的空操作：
                    // 不改焦点也不改最大化目标，就不能标脏（PERF-01）。
                    if next != index {
                        self.focus_workbench_panel(panels[next].clone());
                        if self.workbench.dock.maximized.is_some() {
                            // 最大化跟随焦点，否则轮转后看到的还是旧面板。
                            self.workbench.dock.maximized = Some(panels[next].clone());
                        }
                        dirty = true;
                    }
                }
            }
            KeyCode::Enter => {
                self.workbench.dock.maximized = if self.workbench.dock.maximized.is_some() {
                    None
                } else {
                    Some(self.workbench.dock.focused.clone())
                };
                dirty = true;
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Up | KeyCode::Down => {
                let edge = match key.code {
                    KeyCode::Left => Edge::Left,
                    KeyCode::Right => Edge::Right,
                    KeyCode::Up => Edge::Top,
                    _ => Edge::Bottom,
                };
                if key.modifiers.contains(KeyModifiers::SHIFT) {
                    if let Some((target, _)) = geometry
                        .panels
                        .iter()
                        .find(|(panel, _)| panel != &self.workbench.dock.focused)
                    {
                        dirty |= self.workbench.dock.dock(
                            self.workbench.dock.focused.clone(),
                            target,
                            edge,
                        );
                    }
                } else {
                    // 最大化时屏幕上只有一个铺满的面板，分隔线全被挡住：先退出
                    // 最大化（与 `DockLayout::dock` 内 `maximized = None` 的既有
                    // 语义一致），resize 才是用户看得见的改动，而不是「无声不可
                    // 见改动 + 落盘」（WB-01）。
                    if !self.workbench.dock.locked && self.workbench.dock.maximized.take().is_some()
                    {
                        dirty = true;
                    }
                    let horizontal = matches!(edge, Edge::Left | Edge::Right);
                    if let Some((_, focused)) = geometry
                        .panels
                        .iter()
                        .find(|(panel, _)| panel == &self.workbench.dock.focused)
                    {
                        if let Some(divider) = geometry.dividers.iter().rev().find(|divider| {
                            (divider.axis == Axis::Horizontal) == horizontal
                                && divider.area.contains((focused.x, focused.y).into())
                        }) {
                            let ratio = if horizontal {
                                f32::from(divider.handle.x - divider.area.x)
                                    / f32::from(divider.area.width.max(1))
                            } else {
                                f32::from(divider.handle.y - divider.area.y)
                                    / f32::from(divider.area.height.max(1))
                            };
                            dirty |= self.workbench.dock.resize(
                                &divider.path,
                                ratio
                                    + if matches!(edge, Edge::Left | Edge::Top) {
                                        -0.03
                                    } else {
                                        0.03
                                    },
                            );
                        }
                    }
                }
            }
            _ => {}
        }
        self.sync_observation_page_with_focus();
        self.publish_workbench_focus(previous_tab, outcome);
        if dirty {
            self.schedule_chrome_preferences(std::time::Instant::now());
        }
        outcome.repaint = true;
        true
    }
}
