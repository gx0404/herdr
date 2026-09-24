use super::super::feedback::ChromeHover;
use super::super::observability::{tr, Page};
use super::interaction::Action;
use super::*;
use crate::ui::kit::footer_hints::{render_footer_hints, FooterHint};
use ratatui::widgets::{Block, Borders, Widget};

fn put(buffer: &mut Buffer, area: Rect, text: &str, style: Style) {
    if area.is_empty() {
        return;
    }
    buffer.set_stringn(area.x, area.y, text, usize::from(area.width), style);
}

fn translated(rect: crate::protocol::SurfaceRect, area: Rect) -> Rect {
    Rect::new(
        area.x.saturating_add(rect.x),
        area.y.saturating_add(rect.y),
        rect.width,
        rect.height,
    )
    .intersection(area)
}

/// 顶栏放不下整排按钮时先丢的排在前面：监控、锁定布局、调整布局在主菜单
/// 与命令搜索里都有对应条目（调整布局模式还能按 Esc 退出）；复位只有顶栏
/// 这一个入口；主菜单是其余一切的入口，最后才丢。
fn top_bar_drop_rank(action: &Action) -> u8 {
    match action {
        Action::Open(_) => 0,
        Action::Lock => 1,
        Action::Arrange => 2,
        Action::Reset => 3,
        _ => 4,
    }
}

/// 顶栏每个按钮画不画：按钮之间留 1 列，整排超过 `cols` 时按
/// [`top_bar_drop_rank`] 整项丢弃，至少留一个。冒烟 B3：以前按剩余宽度
/// 硬截，英文调整布局模式（整排 58 列）窄时「 Reset 」被截成「 Res」。
fn fit_top_bar(buttons: &[(String, Action)], cols: u16) -> Vec<bool> {
    let mut shown = vec![true; buttons.len()];
    loop {
        let widths: Vec<usize> = buttons
            .iter()
            .zip(&shown)
            .filter(|(_, shown)| **shown)
            .map(|((label, _), _)| label.width())
            .collect();
        let width = widths.iter().sum::<usize>() + widths.len().saturating_sub(1);
        if width <= usize::from(cols) || widths.len() <= 1 {
            return shown;
        }
        let Some(drop) = buttons
            .iter()
            .enumerate()
            .filter(|(index, _)| shown[*index])
            .min_by_key(|(_, (_, action))| top_bar_drop_rank(action))
            .map(|(index, _)| index)
        else {
            return shown;
        };
        shown[drop] = false;
    }
}

/// 无边框窗格的拖动把手落点（冒烟 L2）：窗格首行就是终端内容，把手改放到正上方
/// 那一行的空位——面板顶部的窗格是标签栏行，其余是窗格间的空隙行。在窗格列宽内
/// 从左往右找 2 列把手 + 左侧 1 列间隔都满足的位置：不压标签 / 按钮 / 标题等命中
/// 区与已放的把手，不落进任何窗格矩形（无空隙堆叠时正上方是另一个窗格的内容），
/// 画面上是空白格（避开标签栏右侧的状态段落与省略号）。找不到返回 `None`。
fn borderless_pane_handle(
    composed: &Buffer,
    pane: Rect,
    taken: &[(Rect, Action)],
    panes: &[PaneHit],
) -> Option<Rect> {
    let y = pane.y.checked_sub(1)?;
    let last = pane.right().checked_sub(2)?;
    (pane.x.saturating_add(1)..=last)
        .map(|x| Rect::new(x, y, 2, 1))
        .find(|slot| {
            let padded = Rect::new(slot.x - 1, y, slot.width + 1, 1);
            padded.intersection(composed.area) == padded
                && !taken.iter().any(|(rect, _)| rect.intersects(padded))
                && !panes.iter().any(|hit| hit.rect.intersects(padded))
                && (padded.x..padded.right()).all(|x| composed[(x, y)].symbol() == " ")
        })
}

/// 面板标题文字（不含拖动把手）。终端组按它当前那一栏标签所属的工作区命名，
/// 同名工作区带上编号。
fn panel_title(dock: &DockLayout, snapshot: &ClientShellSnapshot, panel: &PanelId) -> String {
    match panel {
        PanelId::Workspaces => tr("WORKSPACES", "工作区").to_owned(),
        PanelId::Agents => "Agents".to_owned(),
        PanelId::Monitor => tr("MONITOR", "监控").to_owned(),
        PanelId::Accounts => tr("ACCOUNTS", "账号用量").to_owned(),
        PanelId::Terminal(id) => {
            // The strip holds one workspace's tabs; name the panel after that
            // workspace so each terminal page is identifiable. Same-named
            // workspaces get their number.
            let workspace = dock
                .groups
                .iter()
                .find(|group| group.id == *id)
                .and_then(|group| group.tabs.first())
                .and_then(|tab| snapshot.tabs.iter().find(|entry| &entry.tab_id == tab))
                .and_then(|tab| {
                    snapshot
                        .workspaces
                        .iter()
                        .find(|ws| ws.workspace_id == tab.workspace_id)
                });
            match workspace {
                Some(ws) => {
                    let duplicated = snapshot
                        .workspaces
                        .iter()
                        .filter(|other| other.label == ws.label)
                        .count()
                        > 1;
                    if duplicated {
                        format!("{} #{}", ws.label, ws.number)
                    } else {
                        ws.label.clone()
                    }
                }
                None => format!("{} {id}", tr("TERMINALS", "终端组")),
            }
        }
    }
}

/// 紧凑视图标题栏的面板切换条：按布局顺序（与调整布局模式里 Tab 轮转同序）每个
/// 面板一段 ` 名称 `，段间留 1 列。整排超过 `width` 返回空，调用方退回只画聚焦
/// 面板的标题。
fn compact_switcher(
    dock: &DockLayout,
    snapshot: &ClientShellSnapshot,
    width: u16,
) -> Vec<(PanelId, String)> {
    // 与 Tab 轮转同一取法：去最大化投影，足够大的区域不会触发紧凑投影。
    let panels = dock.layout_geometry(Rect::new(0, 0, 4096, 4096)).panels;
    if panels.len() < 2 {
        return Vec::new();
    }
    let segments = panels
        .into_iter()
        .map(|(panel, _)| {
            let segment = format!(" {} ", panel_title(dock, snapshot, &panel));
            (panel, segment)
        })
        .collect::<Vec<_>>();
    let total = segments
        .iter()
        .map(|(_, segment)| segment.width())
        .sum::<usize>()
        + segments.len().saturating_sub(1);
    if total > usize::from(width) {
        return Vec::new();
    }
    segments
}

pub(super) fn pane_hit(pane: &crate::protocol::PaneSurfacePane, area: Rect) -> PaneHit {
    PaneHit {
        rect: translated(pane.rect, area),
        inner_rect: translated(pane.inner_rect, area),
        scrollbar_rect: pane.scrollbar_rect.map(|rect| translated(rect, area)),
        scroll: pane.scroll.map(|value| crate::pane::ScrollMetrics {
            offset_from_bottom: value.offset_from_bottom as usize,
            max_offset_from_bottom: value.max_offset_from_bottom as usize,
            viewport_rows: value.viewport_rows as usize,
        }),
        pane_id: pane.pane_id.clone(),
        popup: false,
        mouse_reporting: pane.mouse_reporting,
        sgr_pixel_mouse: pane.sgr_pixel_mouse,
        pixel_width: pane.pixel_width,
        pixel_height: pane.pixel_height,
    }
}

impl ClientShellState {
    pub(in crate::client::shell) fn compose_workbench(
        &mut self,
        cols: u16,
        rows: u16,
    ) -> Option<crate::client::frame_output::ComposedFrame> {
        let snapshot = self.snapshot.as_deref()?;
        let now = self
            .last_composed_at
            .unwrap_or_else(std::time::Instant::now);
        // 视图计算：记下第一次看到这个快照修订号的时刻，算出沿用旧帧的视图里最早
        // 到期的宽限（到期由 `tick_workbench` 请求重绘）。渲染只读这些结果。
        if self
            .workbench
            .snapshot_seen
            .is_none_or(|(revision, _)| revision != snapshot.revision)
        {
            self.workbench.snapshot_seen = Some((snapshot.revision, now));
        }
        self.workbench.stale_until = self
            .workbench
            .views
            .iter()
            .filter_map(|(id, view)| {
                match super::presentation(
                    view,
                    self.workbench.stamps.get(id).copied(),
                    self.workbench.snapshot_seen,
                    snapshot,
                    now,
                ) {
                    super::Presentation::Stale { until } => Some(until),
                    _ => None,
                }
            })
            .min();
        let spinner = self.spinner_glyph();
        let visual_bell = self.visual_bell_active();
        let broadcast_count = self.broadcast_indicator_count();
        let palette = self.config.palette.clone();
        let palette = &palette;
        let full = Rect::new(0, 0, cols, rows);
        // 单 Buffer 管线（批 12b）：与经典 compose 共用同一个保留 Buffer。
        let mut canvas = super::super::compose_canvas::ComposeCanvas::reuse_or_new(
            self.compose_buffer.take(),
            cols,
            rows,
        );
        canvas
            .buffer()
            .set_style(full, Style::default().fg(palette.text).bg(palette.panel_bg));
        self.hits = ShellHitMap::default();
        self.workbench.hits.clear();
        self.workbench.footer_hits.clear();
        if !self.workbench.arranging {
            // 页脚提示只在调整布局模式出现：退出后不留悬浮态，下次进入不闪旧高亮。
            self.workbench.footer_hover = None;
        }
        self.workbench.geometry = self.workbench.dock.geometry(content_area(cols, rows));
        let menu_texts = &crate::i18n::texts().menu;
        // 顶栏：主菜单入口、监控、调整布局（模式开关，进入时反色）、锁定布局
        // （二态开关用勾选态，不再在「锁定 / 解锁」两套文案间切换）、复位（只在
        // 调整布局时出现）。
        let launcher_hovered = matches!(self.hover, Some(ChromeHover::GlobalLauncher));
        let lock_label = if self.workbench.dock.locked {
            format!(" ✓ {} ", menu_texts.lock_layout)
        } else {
            format!(" {} ", menu_texts.lock_layout)
        };
        let buttons: Vec<(String, Action)> = [
            (" herdr ≡ ".to_owned(), Action::Menu),
            (
                format!(" {} ", menu_texts.monitor_short),
                Action::Open(PanelId::Monitor),
            ),
            (format!(" {} ", menu_texts.arrange_layout), Action::Arrange),
            (lock_label, Action::Lock),
            (tr(" Reset ", " 复位 ").to_owned(), Action::Reset),
        ]
        .into_iter()
        .filter(|(_, action)| self.workbench.arranging || !matches!(action, Action::Reset))
        .collect();
        let shown = fit_top_bar(&buttons, cols);
        let mut x = 0;
        for ((label, action), shown) in buttons.into_iter().zip(shown) {
            if !shown {
                continue;
            }
            // 只剩主菜单仍放不下（不到 9 列）时照旧按屏宽裁。
            let width = (label.width() as u16).min(cols.saturating_sub(x));
            let rect = Rect::new(x, 0, width, u16::from(rows > 0));
            let base = Style::default()
                .fg(palette.accent)
                .bg(palette.surface0)
                .add_modifier(Modifier::BOLD);
            let style = match action {
                Action::Arrange if self.workbench.arranging => Style::default()
                    .fg(crate::ui::color::contrast_fg(palette, palette.accent))
                    .bg(palette.accent)
                    .add_modifier(Modifier::BOLD),
                Action::Menu if launcher_hovered => {
                    base.fg(palette.text).bg(self.config.components.hover_bg)
                }
                // 锁定布局时复位点了也无效（文档终审 D1），与页脚里锁定后置灰的
                // 提示用同一种灰、不加粗。
                Action::Reset if self.workbench.dock.locked => {
                    Style::default().fg(palette.overlay0).bg(palette.surface0)
                }
                _ => base,
            };
            put(canvas.buffer(), rect, &label, style);
            if matches!(action, Action::Menu) {
                self.hits.global_launcher = rect;
            }
            self.workbench.hits.push((rect, action));
            x = x.saturating_add(width).saturating_add(1);
        }
        let workspace = self
            .workbench
            .geometry
            .panels
            .iter()
            .find(|(id, _)| *id == PanelId::Workspaces)
            .map(|(id, area)| body(*area, id))
            .unwrap_or_default();
        let agents = self
            .workbench
            .geometry
            .panels
            .iter()
            .find(|(id, _)| *id == PanelId::Agents)
            .map(|(id, area)| body(*area, id))
            .unwrap_or_default();
        let federated_agent_rows = self
            .federated_agent_rows
            .as_ref()
            .map(|cache| cache.view())
            .unwrap_or_default();
        // 导航确认后等待权威焦点期间，工作区面板同样保留目标行高亮（上游 #4408），
        // 取法与经典布局的 `compose` 一致。
        let pending_workspace_highlight =
            self.pending_workspace_highlight.as_ref().filter(|pending| {
                self.mode != ClientShellMode::Navigate
                    && pending.target.endpoint_id == self.active_endpoint_id
                    && self.navigation_target_valid(&pending.target)
            });
        let mut sidebar_state = super::super::render::ShellRenderState {
            federated_agent_rows,
            endpoints: &self.endpoints,
            machine_chrome: &self.machine_chrome,
            active_endpoint_id: &self.active_endpoint_id,
            collapsed_endpoints: &self.collapsed_endpoints,
            collapsed_groups: &self.collapsed_groups,
            remote_collapsed_groups: &self.remote_collapsed_groups,
            workspace_scroll: &mut self.workspace_scroll,
            agent_scroll: &mut self.agent_scroll,
            tab_scroll: &mut self.tab_scroll,
            reveal_focused_workspace: &mut self.reveal_focused_workspace,
            reveal_focused_tab: &mut self.reveal_focused_tab,
            sidebar_collapsed: false,
            sidebar_section_split: 0.5,
            tab_drag_insert_index: None,
            selected_workspace_id: self
                .navigate_workspace_id
                .as_ref()
                .or_else(|| pending_workspace_highlight.map(|pending| &pending.target)),
            reveal_navigation_workspace: &mut self.reveal_navigation_workspace,
            dragged_workspace_id: None,
            workspace_drop_indicator_row: None,
            chrome_hover: self.hover.as_ref(),
            spinner,
        };
        if !workspace.is_empty() || !agents.is_empty() {
            super::super::endpoint_sidebar::render_expanded_regions(
                canvas.buffer(),
                workspace,
                Some(snapshot),
                &self.config,
                &mut sidebar_state,
                &mut self.hits,
                Some((workspace, agents)),
            );
        }
        self.hits.sidebar_divider = Rect::default();
        self.hits.sidebar_section_divider = Rect::default();
        self.hits.sidebar_toggle = Rect::default();
        // 非聚焦面板的标题与按钮按对标题栏底色的对比度选色（冒烟 L3：overlay0 叠
        // surface0 只有 2.57:1），聚焦面板仍用 accent 区分。
        let unfocused_title = super::super::render::readable_muted_fg(palette, palette.surface0);
        let mut compact_switcher_shown = false;
        for (panel, area) in &self.workbench.geometry.panels {
            if area.is_empty() {
                continue;
            }
            let focused = panel == &self.workbench.dock.focused;
            let color = if focused {
                palette.accent
            } else {
                unfocused_title
            };
            // 标题前的 `⠿` 是拖动停靠的把手：锁定布局后拖动被拒绝（冒烟 L1）、紧凑
            // 视图与最大化只投影一个面板没有停靠目标（W2、N20）、不接鼠标时工作台
            // 不收鼠标事件（N20），都不画它，标题文字左移占位。
            let handle = if self.workbench.dock.locked
                || self.workbench.geometry.compact
                || self.workbench.dock.maximized.is_some()
                || !self.config.mouse_capture
            {
                ""
            } else {
                "⠿ "
            };
            let header = Rect::new(area.x, area.y, area.width, 1);
            canvas
                .buffer()
                .set_style(header, Style::default().bg(palette.surface0));
            // 监控 / 账号面板头部带显式关闭按钮：终端聚焦时 Esc 进终端，
            // 用户仍能一键关掉停靠面板。
            let closable = matches!(panel, PanelId::Monitor | PanelId::Accounts);
            let controls = if closable { 6 } else { 3 };
            self.workbench
                .hits
                .push((header, Action::Header(panel.clone())));
            let title_area = Rect::new(area.x, area.y, area.width.saturating_sub(controls), 1);
            // 紧凑视图只投影聚焦面板：标题栏改成面板切换条，按布局顺序列出全部面板，
            // 点名字就切过去（L18）；整排放不下才退回只画聚焦面板的标题。
            let switcher = if self.workbench.geometry.compact {
                compact_switcher(&self.workbench.dock, snapshot, title_area.width)
            } else {
                Vec::new()
            };
            compact_switcher_shown |= !switcher.is_empty();
            if switcher.is_empty() {
                put(
                    canvas.buffer(),
                    title_area,
                    &format!(
                        "{handle}{}",
                        panel_title(&self.workbench.dock, snapshot, panel)
                    ),
                    Style::default()
                        .fg(color)
                        .bg(palette.surface0)
                        .add_modifier(Modifier::BOLD),
                );
            }
            let mut x = title_area.x;
            for (candidate, segment) in &switcher {
                let width = segment.width() as u16;
                let rect = Rect::new(x, area.y, width, 1);
                let style = if candidate == panel {
                    Style::default()
                        .fg(crate::ui::color::contrast_fg(palette, palette.accent))
                        .bg(palette.accent)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(unfocused_title).bg(palette.surface0)
                };
                put(canvas.buffer(), rect, segment, style);
                // 不接鼠标（`mouse_capture = false`）时工作台整体不收鼠标事件，点不到
                // 的切换区不登记；切换条照画，只标出当前在哪个面板（W1）。
                if self.config.mouse_capture {
                    self.workbench
                        .hits
                        .push((rect, Action::Switch(candidate.clone())));
                }
                x = x.saturating_add(width).saturating_add(1);
            }
            let toggle = Rect::new(
                area.right().saturating_sub(3).max(area.x),
                area.y,
                3.min(area.width),
                1,
            );
            put(
                canvas.buffer(),
                toggle,
                if self.workbench.dock.maximized.is_some() {
                    " ◫ "
                } else {
                    " □ "
                },
                Style::default().fg(color),
            );
            self.workbench
                .hits
                .push((toggle, Action::Maximize(panel.clone())));
            if closable && area.width > 6 {
                let close = Rect::new(toggle.x.saturating_sub(3).max(area.x), area.y, 3, 1);
                put(canvas.buffer(), close, " × ", Style::default().fg(color));
                self.workbench
                    .hits
                    .push((close, Action::Close(panel.clone())));
            }
            if let PanelId::Terminal(id) = panel {
                let Some(group) = self
                    .workbench
                    .dock
                    .groups
                    .iter()
                    .find(|group| group.id == *id)
                else {
                    continue;
                };
                let tabs = group
                    .tabs
                    .iter()
                    .filter_map(|id| snapshot.tabs.iter().find(|tab| &tab.tab_id == id))
                    .collect::<Vec<_>>();
                let scroll = self.workbench.tab_scroll.entry(*id).or_default();
                let stamp = (group.active.clone(), area.width);
                let mut reveal = self.workbench.tab_focus.get(id) != Some(&stamp);
                self.workbench.tab_focus.insert(*id, stamp);
                let mut strip_hits = ShellHitMap::default();
                super::super::render::render_tab_strip(
                    canvas.buffer(),
                    Rect::new(area.x, area.y + 1, area.width, u16::from(area.height > 1)),
                    &self.config,
                    scroll,
                    &mut reveal,
                    super::super::render::TabStripContext {
                        tabs: &tabs,
                        focused: group.active.as_deref(),
                        status: focused.then_some(snapshot),
                        hover: self.hover.as_ref(),
                        visual_bell: visual_bell && focused,
                        insert_index: None,
                    },
                    &mut strip_hits,
                );
                for (rect, tab) in &strip_hits.tabs {
                    self.workbench.hits.push((
                        *rect,
                        Action::Tab {
                            group: *id,
                            tab: tab.clone(),
                        },
                    ));
                }
                for (rect, action) in [
                    (strip_hits.tab_scroll_left, Action::ScrollTabs(*id, -1)),
                    (strip_hits.tab_scroll_right, Action::ScrollTabs(*id, 1)),
                    (strip_hits.new_tab, Action::NewTab(*id)),
                ] {
                    if !rect.is_empty() {
                        self.workbench.hits.push((rect, action));
                    }
                }
                self.hits.tabs.extend(strip_hits.tabs);
            }
        }
        for divider in &self.workbench.geometry.dividers {
            for y in divider.handle.y..divider.handle.bottom() {
                for x in divider.handle.x..divider.handle.right() {
                    canvas.buffer()[(x, y)]
                        .set_symbol(if divider.axis == dock::Axis::Horizontal {
                            "│"
                        } else {
                            "─"
                        })
                        .set_style(Style::default().fg(palette.surface1));
                }
            }
        }
        let footer = Rect::new(0, rows.saturating_sub(1), cols, u16::from(rows > 0));
        if self.workbench.arranging {
            // 「调整布局」模式：状态标签 + 键位提示条；「Esc 完成」可点，退出模式。
            let status = format!(" {} ", menu_texts.arrange_hint);
            let status_width = (status.width() as u16).min(footer.width);
            put(
                canvas.buffer(),
                Rect::new(footer.x, footer.y, status_width, footer.height),
                &status,
                Style::default()
                    .fg(palette.accent)
                    .add_modifier(Modifier::BOLD),
            );
            // 锁定布局时调尺寸 / 移动都会被拒绝，这两条提示置灰（冒烟 L1）；
            // 切换面板、最大化不改布局，照常可用。
            let unlocked = !self.workbench.dock.locked;
            let hints = [
                FooterHint {
                    key: "Tab",
                    label: menu_texts.arrange_focus,
                    enabled: true,
                    primary: false,
                },
                FooterHint {
                    key: "←↑↓→",
                    label: menu_texts.arrange_resize,
                    enabled: unlocked,
                    primary: false,
                },
                FooterHint {
                    key: "Shift+←↑↓→",
                    label: menu_texts.arrange_move,
                    enabled: unlocked,
                    primary: false,
                },
                FooterHint {
                    key: "Enter",
                    label: menu_texts.arrange_maximize,
                    enabled: true,
                    primary: false,
                },
                FooterHint {
                    key: "Esc",
                    label: menu_texts.arrange_done,
                    enabled: true,
                    primary: true,
                },
            ];
            let hint_area = Rect::new(
                footer.x.saturating_add(status_width).saturating_add(1),
                footer.y,
                footer.width.saturating_sub(status_width.saturating_add(1)),
                footer.height,
            );
            // 悬浮下标由鼠标移动按上一帧的 `footer_hits` 算出，只落在可点项上
            // （复审轻级 B2），与机器页等其它可点页脚同一套悬浮反馈。
            for (rect, index) in render_footer_hints(
                canvas.buffer(),
                hint_area,
                &hints,
                self.workbench.footer_hover,
                palette,
            ) {
                if index == hints.len() - 1 {
                    self.workbench.hits.push((rect, Action::Arrange));
                    self.workbench.footer_hits.push((rect, index));
                }
            }
        } else {
            // 点名字切换只在接鼠标时成立；不接鼠标时保留键盘唯一的切换路径「调整布局
            // + Tab」（W1）。
            let hint = if compact_switcher_shown && self.config.mouse_capture {
                // 紧凑视图本来就窄，提示写短。
                tr(
                    "Compact view · click a name above to switch panels",
                    "紧凑视图 · 点上方的面板名切换面板",
                )
            } else if self.workbench.geometry.compact {
                tr(
                    "Compact view · use Arrange layout / Tab to switch panels",
                    "紧凑视图 · 在「调整布局」模式用 Tab 切换面板",
                )
            } else if self.workbench.dock.maximized.is_some() {
                // 最大化只投影一个面板：没有别的面板可停靠、也没有分隔线可拖，页脚
                // 改说怎么还原（N20）；不接鼠标时 `◫` 点不到，说键盘路径。锁定不拦
                // 还原（点 `◫` 与调整布局的 Enter 都不查锁定），所以锁定时照样先说
                // 怎么还原、再注明布局已锁定（复审轻级 1）。
                match (self.config.mouse_capture, self.workbench.dock.locked) {
                    (true, false) => tr(
                        "Maximized · click ◫ to restore the layout",
                        "已最大化 · 点 ◫ 还原布局",
                    ),
                    (true, true) => tr(
                        "Maximized · click ◫ to restore the layout · layout locked",
                        "已最大化 · 点 ◫ 还原布局 · 布局已锁定",
                    ),
                    (false, false) => tr(
                        "Maximized · press Enter in Arrange layout to restore the layout",
                        "已最大化 · 在「调整布局」模式按 Enter 还原布局",
                    ),
                    (false, true) => tr(
                        "Maximized · press Enter in Arrange layout to restore the layout · layout locked",
                        "已最大化 · 在「调整布局」模式按 Enter 还原布局 · 布局已锁定",
                    ),
                }
            } else if self.workbench.dock.locked {
                // 锁定后拖动停靠 / 调尺寸都被拒绝：不再提示拖动，改为说明怎么解锁
                // （冒烟 L1）；不接鼠标时顶栏点不到，指向主菜单里的同名开关（N20）。
                if self.config.mouse_capture {
                    tr(
                        "Layout locked · turn off Lock layout in the top bar to rearrange",
                        "布局已锁定 · 在顶栏取消「锁定布局」后可重新排布",
                    )
                } else {
                    tr(
                        "Layout locked · turn off Lock layout in the main menu to rearrange",
                        "布局已锁定 · 在主菜单取消「锁定布局」后可重新排布",
                    )
                }
            } else if !self.config.mouse_capture {
                // 不接鼠标时拖不动（W1 同类，N20）：说键盘怎么排布——从主菜单进「调整
                // 布局」，键位与该模式页脚同名。
                tr(
                    "Arrange layout (main menu): Tab focus panel · ←↑↓→ resize · Shift+←↑↓→ move",
                    "从主菜单进入「调整布局」：Tab 切换面板 · ←↑↓→ 调尺寸 · Shift+←↑↓→ 移动",
                )
            } else {
                tr(
                    "Drag ⠿ to dock · drag borders to resize · drag tabs to split or regroup",
                    "拖动 ⠿ 停靠 · 拖动分隔线调尺寸 · 拖动标签拆分或归组",
                )
            };
            // 放不下时截短并以省略号收尾，看得出后面还有字。
            put(
                canvas.buffer(),
                footer,
                &crate::ui::truncate_end(hint, usize::from(footer.width)),
                Style::default().fg(palette.overlay0),
            );
        }
        let mut occlusion = crate::kitty_graphics::surface::Occlusion::default();
        self.observability.begin_paint();
        let mut stale_panel_areas = Vec::new();
        for (panel, rect) in &self.workbench.geometry.panels {
            let area = body(*rect, panel);
            if let PanelId::Terminal(id) = panel {
                let view_id = id.to_string();
                let Some(view) = self.workbench.views.get(&view_id).filter(|view| {
                    super::presentation(
                        view,
                        self.workbench.stamps.get(&view_id).copied(),
                        self.workbench.snapshot_seen,
                        snapshot,
                        now,
                    ) != super::Presentation::Unavailable
                }) else {
                    // C-12 (a)：占位不再逐面板整帧往返，先收集区域，与把手/预览合并成一次。
                    stale_panel_areas.push(area);
                    continue;
                };
                let cursor = canvas.cursor();
                canvas.blit_frame(&view.surface.frame, area);
                if panel != &self.workbench.dock.focused {
                    canvas.set_cursor(cursor);
                }
                self.hits
                    .panes
                    .extend(view.surface.panes.iter().map(|pane| pane_hit(pane, area)));
                let signature = pane_surface_topology_signature(&view.surface);
                self.hits
                    .pane_splits
                    .extend(view.surface.splits.iter().map(|split| PaneSplitHit {
                        tab_id: Some(view.tab.clone()),
                        direction: split.direction,
                        pos: split.pos.saturating_add(
                            if split.direction
                                == crate::protocol::PaneSurfaceSplitDirection::Horizontal
                            {
                                area.x
                            } else {
                                area.y
                            },
                        ),
                        area: translated(split.area, area),
                        hit_rect: translated(split.hit_rect, area),
                        path: split.path.clone(),
                        topology_signature: signature,
                    }));
            } else if matches!(panel, PanelId::Monitor | PanelId::Accounts) {
                // The monitor panel hosts all observation pages and always
                // paints the tab the user last selected, whether or not the
                // panel is focused; rendering never rewrites `page`. Legacy
                // accounts panels keep rendering the accounts page.
                let tab = if *panel == PanelId::Accounts {
                    Page::Accounts
                } else {
                    self.observability.monitor_tab
                };
                // 面板 pass 不画悬浮层：悬浮层由下方的全局 pass 画一次。
                let cx = super::super::feedback::ChromeContext {
                    page_bounds: None,
                    palette,
                    components: &self.config.components,
                    glyphs: self.config.border_glyphs,
                    hover: None,
                    spinner,
                    now: self
                        .last_composed_at
                        .unwrap_or_else(std::time::Instant::now),
                };
                if let Some(painted) =
                    self.observability
                        .paint(&mut canvas, area, &cx, Some(tab), false)
                {
                    for rect in painted.covered {
                        occlusion.cover(rect);
                    }
                    self.observability.commit_paint(painted);
                }
            }
        }
        // 窗格的 `⠿` 把手只在拖得动时画：锁定布局拒绝拖动（冒烟 L1），不接鼠标时
        // 工作台不收鼠标事件（N20），都不画、也不登记点不到的命中区。
        let drag_handles = !self.workbench.dock.locked && self.config.mouse_capture;
        // C-12 (b)：把手/预览/错误/占位的合并往返只在确有内容可画时才做；
        // 守卫谓词与下方各绘制分支完全一致，守卫为假时今天也是空跑往返。
        let has_handles = drag_handles
            && self.hits.panes.iter().any(|hit| {
                hit.rect.width > 4 && (hit.inner_rect.y > hit.rect.y || self.workbench.arranging)
            });
        let drop_preview = self
            .workbench
            .drag
            .as_ref()
            .and_then(|drag| drag.drop_target(&self.workbench.geometry));
        if !stale_panel_areas.is_empty()
            || has_handles
            || drop_preview.is_some()
            || self.endpoint_error.is_some()
        {
            {
                let composed = canvas.buffer();
                for area in &stale_panel_areas {
                    put(
                        composed,
                        *area,
                        tr("Waiting for terminal…", "正在同步终端…"),
                        Style::default().fg(palette.overlay0),
                    );
                }
                for hit in &self.hits.panes {
                    if hit.rect.width > 4
                        && drag_handles
                        && (hit.inner_rect.y > hit.rect.y || self.workbench.arranging)
                    {
                        // 有上边框：把手压在边框行上。无边框（只在调整布局模式画）：
                        // 首行是终端内容，把手挪到正上方那一行的空位（冒烟 L2）；
                        // 上方没有 chrome 行可用（无空隙堆叠在另一个窗格下面、或
                        // 标签占满窗格列宽）才退回原位，保住拖动能力。
                        let on_border = Rect::new(hit.rect.x + 1, hit.rect.y, 2, 1);
                        let handle = if hit.inner_rect.y > hit.rect.y {
                            on_border
                        } else {
                            borderless_pane_handle(
                                composed,
                                hit.rect,
                                &self.workbench.hits,
                                &self.hits.panes,
                            )
                            .unwrap_or(on_border)
                        };
                        put(composed, handle, "⠿", Style::default().fg(palette.accent));
                        self.workbench
                            .hits
                            .push((handle, Action::Pane(hit.pane_id.clone())));
                        occlusion.cover(handle);
                    }
                }
                if let Some((target, edge)) = drop_preview {
                    if let Some((_, area)) = self
                        .workbench
                        .geometry
                        .panels
                        .iter()
                        .find(|(panel, _)| panel == &target)
                    {
                        let area = super::interaction::preview(*area, edge);
                        // 拖放预览只画轮廓、不填底：盖住底下的终端内容就没法判断
                        // 放到哪一侧。边框字形与颜色跟面板边框同源（THEME-01/02）。
                        Block::default()
                            .borders(Borders::ALL)
                            .border_set(self.config.border_glyphs.border_set())
                            .border_style(
                                Style::default().fg(self.config.components.pane_border_focused),
                            )
                            .title(tr(" Drop here ", " 放到这里 "))
                            .title_style(Style::default().fg(palette.accent))
                            .render(area, composed);
                        occlusion.cover(area);
                    }
                }
                if let Some(error) = self.endpoint_error.as_deref() {
                    put(composed, footer, error, Style::default().fg(palette.red));
                }
            }
        }
        if visual_bell {
            if let Some(hit) = snapshot
                .focused_pane_id
                .as_ref()
                .and_then(|id| self.hits.panes.iter().find(|hit| &hit.pane_id == id))
            {
                // 聚焦边框强调只改样式不改符号，链接登记不受影响。
                super::super::composition::emphasize_pane_border(
                    canvas.buffer(),
                    hit,
                    palette.yellow,
                );
            }
        }
        self.paint_frozen_selection(&mut canvas, &mut occlusion);
        self.paint_shell_copy(&mut canvas, &mut occlusion)?;
        // C-12 (c)：overlay 打开时这段不画，提前到任何写入之前判断。
        if self.overlay.is_none() {
            {
                if let Some(bar) = super::super::render::render_mode_bar(
                    canvas.buffer(),
                    full,
                    self.mode,
                    self.copy_mode.as_ref(),
                    self.endpoint_error.as_deref(),
                    false,
                    broadcast_count,
                    &self.config.keybinds,
                    palette,
                    &self.config.components,
                ) {
                    occlusion.cover(bar);
                }
            }
        }
        self.hits.popup = None;
        if let Some((area, popup)) = self
            .workbench
            .geometry
            .panels
            .iter()
            .find(|(panel, _)| panel == &self.workbench.dock.focused)
            .and_then(|(panel, area)| {
                self.workbench
                    .focused_view()?
                    .surface
                    .popup
                    .as_deref()
                    .map(|popup| (body(*area, panel), popup))
            })
        {
            if let Some(geometry) = crate::popup_size::resolve_popup_geometry(
                popup
                    .width
                    .map(super::super::composition::client_popup_size),
                popup
                    .height
                    .map(super::super::composition::client_popup_size),
                area,
            ) {
                occlusion.start_popup(geometry.outer);
                ratatui::widgets::Clear.render(geometry.outer, canvas.buffer());
                // 弹窗边框走共享的带标题面板（字形跟随 ui.border_style，颜色取
                // 组件 token），随后 blit 的弹窗帧盖住面板底（THEME-01/02）。
                super::super::render::titled_panel(
                    canvas.buffer(),
                    geometry.outer,
                    &popup.title,
                    self.config.components.pane_border_focused,
                    palette.panel_bg,
                    self.config.border_glyphs,
                );
                canvas.set_cursor(None);
                canvas.blit_frame(&popup.frame, geometry.inner);
                self.hits.popup = Some(PaneHit {
                    rect: geometry.outer,
                    inner_rect: geometry.inner,
                    scrollbar_rect: None,
                    scroll: None,
                    pane_id: popup.terminal_id.clone(),
                    popup: true,
                    mouse_reporting: popup.mouse_reporting,
                    sgr_pixel_mouse: popup.sgr_pixel_mouse,
                    pixel_width: popup.pixel_width,
                    pixel_height: popup.pixel_height,
                });
            }
        }
        if self.observability.process_dialog.is_some()
            || self
                .observability
                .hover
                .as_ref()
                .is_some_and(|hover| hover.visible)
        {
            // 全局浮层在终端内容、把手与选择高亮之后绘制，保持视觉与输入层级一致。
            let cx = super::super::feedback::ChromeContext {
                page_bounds: None,
                palette,
                components: &self.config.components,
                glyphs: self.config.border_glyphs,
                hover: None,
                spinner,
                now: self
                    .last_composed_at
                    .unwrap_or_else(std::time::Instant::now),
            };
            if let Some(painted) = self.observability.paint(&mut canvas, full, &cx, None, true) {
                for rect in painted.covered {
                    occlusion.cover(rect);
                }
                self.observability.commit_paint(painted);
            }
        }
        self.paint_shell_feedback(
            &mut canvas,
            ClientShellLayout {
                sidebar: Rect::default(),
                tab_bar: Rect::default(),
                mobile_header: Rect::default(),
                pane_surface: full,
            },
            &mut occlusion,
        )?;
        self.paint_shell_overlays(&mut canvas, &mut occlusion)?;
        // 图形与帧分开交出（上游 #4561）：由 `frame_output` 按输出通道写出。
        let mut graphics = crate::kitty_graphics::GraphicsOutput::from_bytes(std::mem::take(
            &mut self.workbench.cleanup,
        ));
        // 组合期间快照不会变；上面的 `&mut self` 绘制段之后重新借用。
        let snapshot = self.snapshot.as_deref();
        let stamps = &self.workbench.stamps;
        let snapshot_seen = self.workbench.snapshot_seen;
        for (id, view) in &mut self.workbench.views {
            let area = self.workbench.geometry.panels.iter().find(|(panel, _)| matches!(panel, PanelId::Terminal(group) if group.to_string() == *id)).map(|(panel, area)| body(*area, panel));
            // 图形与画面同一口径：沿用旧帧期间图片照常显示，不跟着闪。
            let visible = area.is_some()
                && snapshot.is_some_and(|snapshot| {
                    super::presentation(view, stamps.get(id).copied(), snapshot_seen, snapshot, now)
                        != super::Presentation::Unavailable
                })
                && self.endpoint_error.is_none();
            let area = area.unwrap_or_default();
            let popup = self
                .hits
                .popup
                .as_ref()
                .filter(|_| {
                    self.workbench.dock.focused == PanelId::Terminal(id.parse().unwrap_or(0))
                })
                .map(|hit| (hit.inner_rect.x, hit.inner_rect.y));
            let visibility = if !visible {
                crate::kitty_graphics::surface::Visibility::Hidden
            } else if popup.is_some() {
                crate::kitty_graphics::surface::Visibility::Popup
            } else {
                crate::kitty_graphics::surface::Visibility::Main
            };
            graphics.extend(view.graphics.encode_output(
                visibility,
                (area.x, area.y),
                popup,
                self.graphics_cell_size,
                &occlusion,
            ));
        }
        let (frame, buffer) = canvas.finish(Vec::new());
        self.compose_buffer = Some(buffer);
        self.hits.rebuild_chrome_bounds();
        Some(crate::client::frame_output::ComposedFrame { frame, graphics })
    }
}
