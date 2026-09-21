use super::feedback::{relative_time_ago, ChromeContext, ChromeHover, ClientNotificationRecord};
use super::*;

mod settings_overlay;
mod worktree_overlays;
#[derive(Default)]
pub(crate) struct OverlayRender {
    pub(crate) area: Rect,
    pub(crate) menu_popup: Rect,
    pub(crate) menu_search: Rect,
    pub(crate) menu_scroll: usize,
    pub(crate) menu_rows: Vec<(Rect, usize)>,
    pub(crate) primary: Rect,
    pub(crate) clear: Rect,
    pub(crate) cancel: Rect,
    pub(crate) navigator_popup: Rect,
    pub(crate) navigator_search: Rect,
    pub(crate) navigator_rows: Vec<(Rect, ClientNavigatorTarget)>,
    pub(crate) worktree_search: Rect,
    pub(crate) worktree_rows: Vec<(Rect, usize)>,
    pub(crate) help_popup: Rect,
    pub(crate) help_scrollbar: Rect,
    pub(crate) help_scroll_metrics: Option<crate::pane::ScrollMetrics>,
    pub(crate) help_max_scroll: usize,
    pub(crate) settings_popup: Rect,
    pub(crate) settings_scroll: usize,
    pub(crate) settings_tabs: Vec<(Rect, ClientSettingsSection)>,
    pub(crate) settings_choices: Vec<(Rect, usize)>,
    pub(crate) machines_popup: Rect,
    pub(crate) machines_detail_area: Rect,
    pub(crate) machines_scroll: usize,
    /// 本帧是否真的画了某个机器面板列表（列表 / 导入向导 / 转发编辑器）。
    /// 早退分支（窗口太小、discover 步骤）不计算窗口，compose 期的 scroll
    /// 回写必须跳过，否则每帧把用户的滚动位置抹成 0。
    pub(crate) machines_scroll_valid: bool,
    pub(crate) machines_search: Rect,
    pub(crate) machines_rows: Vec<(Rect, crate::client::endpoint::ProfileId)>,
    pub(crate) machines_actions: Vec<(Rect, super::machines_overlay::MachineOverlayButton)>,
    pub(crate) machines_fields: Vec<(Rect, super::machines_overlay::MachineField)>,
    /// Index-keyed rows shared by the import wizard (candidate/toggle rows)
    /// and the forward rules editor (rule rows).
    pub(crate) machines_wizard_rows: Vec<(Rect, usize)>,
    /// Index-keyed input fields of the import wizard group editor and the
    /// forward add form.
    pub(crate) machines_wizard_fields: Vec<(Rect, usize)>,
    pub(crate) machines_max_scroll: usize,
    /// 机器面板公共 toast 的落点：List / Detail / dashboard 各自把自己的
    /// 页脚行报上来，`render_machines_overlay` 在顶层统一画一次
    /// （HERDR-MACH-006）。空 rect = 该视图不承载 toast。
    pub(crate) machines_toast: Rect,
    pub(crate) machine_auth_max_scroll: usize,
    pub(crate) machine_auth_actions: Vec<(Rect, super::machine_auth_overlay::MachineAuthButton)>,
    pub(crate) broadcast_popup: Rect,
    pub(crate) broadcast_rows: Vec<(Rect, usize)>,
    pub(crate) broadcast_actions: Vec<(Rect, super::broadcast::BroadcastButton)>,
    pub(crate) machine_files_popup: Rect,
    pub(crate) machine_files_search: Rect,
    pub(crate) machine_files_rows: Vec<(Rect, usize)>,
    pub(crate) machine_files_actions: Vec<(Rect, super::machine_files_overlay::MachineFilesButton)>,
    /// 查看器可见行数推出的滚动上界（行数 − 可见行数），渲染期回写进 view
    ///（HERDR-MACH-009）。非查看器视图时为 None。
    pub(crate) machine_files_viewer_max_scroll: Option<usize>,
    pub(crate) snippet_popup: Rect,
    pub(crate) snippet_search: Rect,
    pub(crate) snippet_rows: Vec<(Rect, usize)>,
    pub(crate) snippet_fields: Vec<(Rect, usize)>,
    pub(crate) snippet_actions: Vec<(Rect, super::snippets_overlay::SnippetOverlayButton)>,
    pub(crate) scenes_popup: Rect,
    pub(crate) scenes_rows: Vec<(Rect, usize)>,
    pub(crate) scenes_fields: Vec<(Rect, usize)>,
    pub(crate) scenes_actions: Vec<(Rect, super::scenes_overlay::SceneOverlayButton)>,
    pub(crate) notification_history_rows: Vec<(Rect, usize)>,
    pub(crate) product_announcement_scrollbar: Rect,
    pub(crate) product_announcement_scroll_metrics: Option<crate::pane::ScrollMetrics>,
    pub(crate) product_announcement_max_scroll: usize,
    pub(crate) release_notes_scrollbar: Rect,
    pub(crate) release_notes_scroll_metrics: Option<crate::pane::ScrollMetrics>,
    pub(crate) release_notes_max_scroll: usize,
    /// 浮动用量仪表盘内的账号行 / 按钮命中区（非模态，浮层内点击派发）。
    pub(in crate::client::shell) usage_dashboard_actions: Vec<(Rect, super::observability::Action)>,
    pub(crate) cursor: Option<crate::protocol::CursorState>,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn render_client_overlay(
    b: &mut Buffer,
    o: &ClientShellOverlay,
    s: Option<&ClientShellSnapshot>,
    endpoints: &[ClientShellEndpoint],
    saved_profiles: &[SavedSshEndpoint],
    broadcast: &crate::client::endpoint::BroadcastSet,
    connection_errors: &std::collections::HashMap<
        ClientEndpointId,
        crate::remote::ConnectionErrorKind,
    >,
    port_forwards: &std::collections::HashMap<
        ClientEndpointId,
        Vec<crate::remote::PortForwardStatus>,
    >,
    session_log_dropped: &std::collections::HashMap<crate::client::endpoint::ProfileId, u64>,
    active_endpoint_id: &ClientEndpointId,
    k: &LiveKeybindConfig,
    history: &std::collections::VecDeque<ClientNotificationRecord>,
    usage: &super::observability::State,
    cx: &ChromeContext<'_>,
) -> Option<OverlayRender> {
    // 非模态的浮层不压暗整屏：导航器、上下文菜单与浮动用量仪表盘（按键透传到
    // 聚焦终端，正在打字的终端内容不能是暗的）。
    if !matches!(
        o,
        ClientShellOverlay::Navigator(_)
            | ClientShellOverlay::ContextMenu(_)
            | ClientShellOverlay::UsageDashboard
    ) {
        for y in b.area.y..b.area.bottom() {
            for x in b.area.x..b.area.right() {
                let c = &mut b[(x, y)];
                c.set_style(c.style().add_modifier(Modifier::DIM));
            }
        }
    }
    match o {
        ClientShellOverlay::Onboarding => render_onboarding_overlay(b, cx),
        ClientShellOverlay::ProductAnnouncement(v) => render_product_announcement_overlay(b, v, cx),
        ClientShellOverlay::ReleaseNotes(v) => render_release_notes_overlay(
            b,
            v,
            s.map(|snapshot| snapshot.update_install_command.as_str())
                .unwrap_or_default(),
            cx,
        ),
        ClientShellOverlay::Rename(v) => render_rename_overlay(b, v, cx),
        ClientShellOverlay::ConfirmClose(v) => render_confirm_close_overlay(b, v, cx),
        ClientShellOverlay::Help(v) => render_help_overlay(b, v, k, cx),
        ClientShellOverlay::Navigator(v) => {
            render_navigator_overlay(b, v, endpoints, active_endpoint_id, cx)
        }
        ClientShellOverlay::Settings(v) => settings_overlay::render_settings_overlay(
            b,
            v,
            s.is_some_and(|snapshot| snapshot.integration_updates_available),
            cx,
        ),
        ClientShellOverlay::Machines(v) => super::machines_overlay::render_machines_overlay(
            b,
            v,
            endpoints,
            saved_profiles,
            connection_errors,
            port_forwards,
            session_log_dropped,
            cx,
        ),
        ClientShellOverlay::MachineAuth(v) => {
            super::machine_auth_overlay::render_machine_auth_overlay(b, v, cx)
        }
        ClientShellOverlay::Snippets(v) => {
            super::snippets_overlay::render_snippets_overlay(b, v, endpoints, saved_profiles, cx)
        }
        ClientShellOverlay::Scenes(v) => super::scenes_overlay::render_scenes_overlay(b, v, cx),
        ClientShellOverlay::Broadcast(v) => {
            let rows =
                super::broadcast::broadcast_target_rows(broadcast, endpoints, saved_profiles);
            let candidates = super::broadcast::broadcast_machine_candidates(
                broadcast,
                endpoints,
                saved_profiles,
            );
            let (pick_label, panes) = match &v.view {
                super::broadcast::ClientBroadcastView::PickPane { endpoint_id } => (
                    endpoints
                        .iter()
                        .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
                        .map(|endpoint| endpoint.label.clone())
                        .unwrap_or_else(|| crate::i18n::texts().sidebar.local.to_owned()),
                    super::broadcast::broadcast_pane_candidates(endpoints, endpoint_id),
                ),
                _ => (String::new(), Vec::new()),
            };
            super::broadcast::render_broadcast_overlay(
                b,
                v,
                broadcast,
                &rows,
                &candidates,
                &pick_label,
                &panes,
                cx,
            )
        }
        ClientShellOverlay::MachineFiles(v) => {
            let machine_label = saved_profiles
                .iter()
                .find(|profile| profile.id == v.profile_id)
                .map(|profile| profile.label.clone())
                .unwrap_or_else(|| v.profile_id.to_string());
            // C-16：渲染不再克隆整个目录，过滤迭代由 overlay 自己完成。
            super::machine_files_overlay::render_machine_files_overlay(b, v, &machine_label, cx)
        }
        ClientShellOverlay::WorktreeCreate(v) => {
            worktree_overlays::render_worktree_create_overlay(b, v, cx)
        }
        ClientShellOverlay::WorktreeOpen(v) => {
            worktree_overlays::render_worktree_open_overlay(b, v, cx)
        }
        ClientShellOverlay::WorktreeRemove(v) => {
            worktree_overlays::render_worktree_remove_overlay(b, v, cx)
        }
        ClientShellOverlay::NotificationHistory(v) => {
            render_notification_history_overlay(b, v, history, cx)
        }
        ClientShellOverlay::CommandPalette(v) => {
            super::command_palette::render_command_palette(b, v, cx)
        }
        ClientShellOverlay::UsageDashboard => render_usage_dashboard_overlay(b, usage, cx),
        ClientShellOverlay::ContextMenu(_) => None,
    }
}

/// Floating usage dashboard: the accounts overview inside the shared frame,
/// so it supports the same drag-to-float window behavior as settings and the
/// command palette. Non-modal: it respects `usage.format`, its account rows
/// come back as `usage_dashboard_actions` for in-overlay clicks, and data
/// refreshes through the observability polling loop while it is open.
///
/// `cursor: None` 只表示浮层不拥有光标；组合层按浮层矩形是否覆盖终端光标
/// 决定是否保留终端插入点，仪表盘打开时未被盖住的光标仍然可见。
fn render_usage_dashboard_overlay(
    b: &mut Buffer,
    usage: &super::observability::State,
    cx: &ChromeContext<'_>,
) -> Option<OverlayRender> {
    let (outer, inner) = modal_panel(b, crate::ui::ModalSize::Large, cx.palette.accent, cx)?;
    let tr = super::observability::tr;
    let mut actions = Vec::new();
    put_text(
        b,
        inner.x,
        inner.y,
        inner.width,
        tr("Usage dashboard", "用量仪表盘"),
        Style::default()
            .fg(cx.palette.text)
            .add_modifier(Modifier::BOLD),
    );
    if inner.height > 2 {
        let body = Rect::new(
            inner.x,
            inner.y + 1,
            inner.width,
            inner.height.saturating_sub(2),
        );
        if !usage.usage.enabled {
            put_text(
                b,
                body.x,
                body.y,
                body.width,
                tr(
                    "Account usage is disabled in settings.",
                    "账号用量已在设置中关闭。",
                ),
                Style::default().fg(cx.palette.overlay0),
            );
        } else if usage.accounts.is_empty() {
            put_text(
                b,
                body.x,
                body.y,
                body.width,
                if usage.refreshing() {
                    tr("Refreshing…", "刷新中…")
                } else {
                    tr("Waiting for account usage data…", "等待账号用量数据…")
                },
                Style::default().fg(cx.palette.overlay0),
            );
        } else {
            super::observability::render_usage_body(b, body, usage, cx.palette, &mut actions);
        }
    }
    if inner.height > 1 {
        put_text(
            b,
            inner.x,
            inner.y + inner.height.saturating_sub(1),
            inner.width,
            tr(
                "esc close · click a row to select · scroll · drag edges to resize",
                "esc 关闭 · 点击账号行选中 · 滚轮滚动 · 拖动边缘缩放",
            ),
            Style::default().fg(cx.palette.overlay0),
        );
    }
    Some(OverlayRender {
        area: outer,
        usage_dashboard_actions: actions,
        ..OverlayRender::default()
    })
}

pub(crate) fn render_minimum_overlay(b: &mut Buffer, cx: &ChromeContext<'_>) -> OverlayRender {
    if !b.area.is_empty() {
        b.set_style(
            b.area,
            Style::default().fg(cx.palette.text).bg(cx.palette.panel_bg),
        );
        put_text(
            b,
            b.area.x,
            b.area.y,
            b.area.width,
            crate::i18n::texts().global_menu.resize_hint,
            Style::default().fg(cx.palette.accent),
        );
    }
    OverlayRender {
        area: b.area,
        ..OverlayRender::default()
    }
}

pub(crate) fn render_context_menu(
    buffer: &mut Buffer,
    menu: &ClientContextMenuOverlay,
    cx: &ChromeContext<'_>,
) -> Option<OverlayRender> {
    let palette = cx.palette;
    let items = menu.items();
    let screen = buffer.area;
    let max_item_width = items
        .iter()
        .map(|item| display_width(item.label))
        .max()
        .unwrap_or(0);
    let width = max_item_width
        .saturating_add(4)
        .max(14)
        .min(screen.width.max(1));
    let height = (items.len() as u16)
        .saturating_add(2)
        .min(screen.height.max(1));
    let x = menu
        .x
        .min(screen.x.saturating_add(screen.width.saturating_sub(width)));
    let y = menu.y.min(
        screen
            .y
            .saturating_add(screen.height.saturating_sub(height)),
    );
    let rect = Rect::new(x, y, width, height);
    let inner = panel(buffer, rect, palette.accent, palette.panel_bg, cx.glyphs)?;
    let mut rows = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let row_y = inner.y.saturating_add(index as u16);
        if row_y >= inner.bottom() {
            break;
        }
        let row = Rect::new(inner.x, row_y, inner.width, 1);
        let style = list_row_style(
            palette,
            cx.components,
            index == menu.highlighted,
            menu.hovered == Some(index),
        );
        buffer.set_style(row, style);
        put_text(buffer, row.x, row.y, row.width, item.label, style);
        rows.push((row, index));
    }
    Some(OverlayRender {
        area: rect,
        menu_rows: rows,
        ..OverlayRender::default()
    })
}

pub(in crate::client::shell) fn panel(
    b: &mut Buffer,
    a: Rect,
    c: ratatui::style::Color,
    bg: ratatui::style::Color,
    glyphs: crate::ui::BorderGlyphs,
) -> Option<Rect> {
    if a.width < 2 || a.height < 2 {
        return None;
    }
    let background = Style::default().bg(bg).remove_modifier(Modifier::DIM);
    let border = Style::default().fg(c).bg(bg).remove_modifier(Modifier::DIM);
    for y in a.y..a.bottom() {
        for x in a.x..a.right() {
            b[(x, y)].set_symbol(" ").set_style(background);
        }
    }
    for x in a.x..a.right() {
        b[(x, a.y)]
            .set_symbol(if x == a.x {
                glyphs.top_left
            } else if x + 1 == a.right() {
                glyphs.top_right
            } else {
                glyphs.horizontal
            })
            .set_style(border);
        let y = a.bottom() - 1;
        b[(x, y)]
            .set_symbol(if x == a.x {
                glyphs.bottom_left
            } else if x + 1 == a.right() {
                glyphs.bottom_right
            } else {
                glyphs.horizontal
            })
            .set_style(border);
    }
    for y in a.y + 1..a.bottom() - 1 {
        b[(a.x, y)].set_symbol(glyphs.vertical).set_style(border);
        b[(a.right() - 1, y)]
            .set_symbol(glyphs.vertical)
            .set_style(border);
    }
    Some(Rect::new(a.x + 1, a.y + 1, a.width - 2, a.height - 2))
}
/// 带标题的面板：`panel` 加一行画在顶边上的标题。自建面板（监控页、终端组
/// 弹窗）都从这里取边框与标题，边框字形与颜色不再各写一套（C-29 / ds-13）。
pub(in crate::client::shell) fn titled_panel(
    b: &mut Buffer,
    a: Rect,
    title: &str,
    border: ratatui::style::Color,
    bg: ratatui::style::Color,
    glyphs: crate::ui::BorderGlyphs,
) -> Option<Rect> {
    let inner = panel(b, a, border, bg, glyphs)?;
    put_text(
        b,
        a.x + 1,
        a.y,
        a.width.saturating_sub(2),
        title,
        Style::default()
            .fg(border)
            .bg(bg)
            .add_modifier(Modifier::BOLD)
            .remove_modifier(Modifier::DIM),
    );
    Some(inner)
}

/// Centered modal of a size tier plus its painted panel: the shared frame
/// every overlay starts from.
pub(in crate::client::shell) fn modal_panel(
    b: &mut Buffer,
    size: crate::ui::ModalSize,
    border: ratatui::style::Color,
    cx: &ChromeContext<'_>,
) -> Option<(Rect, Rect)> {
    let outer = cx
        .page_bounds
        .map(|rect| rect.intersection(b.area))
        .or_else(|| crate::ui::modal_rect(b.area, size))?;
    let inner = panel(b, outer, border, cx.palette.panel_bg, cx.glyphs)?;
    Some((outer, inner))
}

/// Modal action button: rect width always matches the i18n label display
/// width (CJK safe), style comes from the shared tone/state table.
pub(in crate::client::shell) fn modal_button(
    b: &mut Buffer,
    r: Rect,
    label: &str,
    tone: crate::ui::ModalButtonTone,
    state: crate::ui::ModalButtonState,
    p: &Palette,
) {
    let style = crate::ui::modal_button_style(p, tone, state);
    b.set_style(r, style);
    let w = display_width(label).min(r.width);
    put_text(b, r.x + (r.width - w) / 2, r.y, w, label, style)
}

/// Centered row of action buttons on the given row rect; widths derive from
/// the labels so translations never overflow a hardcoded cell count.
pub(in crate::client::shell) fn modal_button_row(
    area: Rect,
    labels: &[&str],
    gap: u16,
) -> Vec<Rect> {
    let widths = labels
        .iter()
        .map(|label| crate::ui::modal_button_width(label))
        .collect::<Vec<_>>();
    let total = widths.iter().sum::<u16>() + gap * (widths.len().saturating_sub(1) as u16);
    let mut x = area.x + area.width.saturating_sub(total) / 2;
    widths
        .iter()
        .map(|w| {
            let r = Rect::new(
                x,
                area.y,
                (*w).min(area.width.saturating_sub(x - area.x)),
                1,
            );
            x += *w + gap;
            r
        })
        .collect()
}

/// Shared modal search bar: ` / ` prompt while focused, hint or query echo
/// otherwise, optional status override (navigator filter) and right-aligned
/// counter. Returns the text cursor while focused.
pub(in crate::client::shell) struct SearchBar<'a> {
    pub focused: bool,
    pub query: &'a TextEditor,
    pub hint: &'a str,
    pub status: Option<String>,
    pub echo_query: bool,
    pub count: Option<String>,
}

pub(in crate::client::shell) fn render_search_bar(
    b: &mut Buffer,
    area: Rect,
    bar: &SearchBar<'_>,
    p: &Palette,
) -> Option<crate::protocol::CursorState> {
    let text = if bar.focused {
        " / ".to_owned()
    } else if let Some(status) = bar.status.as_deref() {
        format!(" / {status}")
    } else if bar.echo_query && !bar.query.as_str().is_empty() {
        format!(" / {}", bar.query.as_str())
    } else {
        bar.hint.to_owned()
    };
    put_text(
        b,
        area.x,
        area.y,
        area.width,
        &text,
        Style::default()
            .fg(if bar.focused { p.text } else { p.overlay0 })
            .bg(p.panel_bg),
    );
    let count_width = bar.count.as_deref().map(display_width).unwrap_or(0);
    let cursor = if bar.focused {
        text_editor::render(
            b,
            Rect::new(
                area.x + 3,
                area.y,
                area.width
                    .saturating_sub(3 + count_width + u16::from(bar.count.is_some())),
                1,
            ),
            bar.query,
            Style::default().fg(p.text).bg(p.panel_bg),
        )
    } else {
        None
    };
    if let Some(count) = bar.count.as_deref() {
        put_right_text(
            b,
            area,
            area.y,
            count,
            Style::default().fg(p.overlay0).bg(p.panel_bg),
        );
    }
    cursor
}

/// Shared scrollable modal body with a scrollbar track on the right edge:
/// display lines, wrap-aware metrics, clamped scroll, optional track.
struct ScrollbackOverlayRender {
    area: Rect,
    close: Rect,
    track: Option<Rect>,
    metrics: Option<crate::pane::ScrollMetrics>,
    max_scroll: usize,
}

/// `render_scrollback_overlay` 的版式推导结果：header / content / footer 三段与
/// 关闭按钮。
pub(in crate::client::shell) struct ScrollbackOverlayLayout {
    pub stack: crate::ui::ModalStackAreas,
    pub close: Rect,
}

/// 滚动式浮窗（发行说明 / 产品公告）的版式唯一真源：外框 → 内框 → 三段栈 →
/// 关闭按钮。渲染与 `ClientShellState::projected_release_notes_geometry` 的首帧
/// 回退几何都调用它，两边不再各留一份副本（OV-01 的漂移机制）。
pub(in crate::client::shell) fn scrollback_overlay_layout(
    outer: Rect,
) -> Option<ScrollbackOverlayLayout> {
    if outer.width < 2 || outer.height < 2 {
        return None;
    }
    // 与 `panel()` 返回的内框同构：四边各让出一格边框。
    let inner = Rect::new(
        outer.x.saturating_add(1),
        outer.y.saturating_add(1),
        outer.width.saturating_sub(2),
        outer.height.saturating_sub(2),
    );
    if inner.height < 8 || inner.width < 20 {
        return None;
    }
    let stack = crate::ui::modal_stack_areas(inner, 2, 1, 0, 1);
    let close = crate::ui::release_notes_close_button_rect(Rect::new(
        stack.header.x,
        stack.header.y,
        stack.header.width,
        1,
    ));
    Some(ScrollbackOverlayLayout { stack, close })
}

/// Title/subtitle/close header plus scrollable body and scroll-hint footer;
/// release notes and product announcement share this frame.
#[allow(clippy::too_many_arguments)]
fn render_scrollback_overlay(
    b: &mut Buffer,
    size: crate::ui::ModalSize,
    title: &str,
    subtitle: &str,
    lines: Vec<(usize, ratatui::text::Line<'_>)>,
    scroll: u16,
    thumb_hover: &ChromeHover,
    cx: &ChromeContext<'_>,
) -> Option<ScrollbackOverlayRender> {
    let p = cx.palette;
    let (outer, _inner) = modal_panel(b, size, p.accent, cx)?;
    let Some(layout) = scrollback_overlay_layout(outer) else {
        return Some(ScrollbackOverlayRender {
            area: outer,
            close: Rect::default(),
            track: None,
            metrics: None,
            max_scroll: 0,
        });
    };

    let stack = layout.stack;
    let base = Style::default()
        .bg(p.panel_bg)
        .remove_modifier(Modifier::DIM);
    let title_area = Rect::new(
        stack.header.x.saturating_add(1),
        stack.header.y,
        stack.header.width.saturating_sub(2),
        1,
    );
    let subtitle_area = Rect::new(
        stack.header.x.saturating_add(1),
        stack.header.y.saturating_add(1),
        stack.header.width.saturating_sub(2),
        1,
    );
    put_text(
        b,
        title_area.x,
        title_area.y,
        title_area.width,
        title,
        base.fg(p.text).add_modifier(Modifier::BOLD),
    );
    put_text(
        b,
        subtitle_area.x,
        subtitle_area.y,
        subtitle_area.width,
        subtitle,
        base.fg(p.overlay1),
    );
    let close = layout.close;
    modal_button(
        b,
        close,
        crate::ui::modal_close_button_text(),
        crate::ui::ModalButtonTone::Primary,
        cx.button_state(
            &ChromeHover::OverlayPrimary,
            crate::ui::ModalButtonState::Focused,
        ),
        p,
    );

    let body = stack.content;
    let metrics = crate::ui::display_lines_scroll_metrics(&lines, scroll, body);
    let max_scroll = metrics.max_offset_from_bottom;
    let scroll = usize::from(scroll).min(max_scroll);
    let track = crate::ui::release_notes_scrollbar_rect(body, metrics);
    let text_area = track
        .map(|_| Rect::new(body.x, body.y, body.width.saturating_sub(1), body.height))
        .unwrap_or(body);
    let paragraph = ratatui::widgets::Paragraph::new(
        lines.into_iter().map(|(_, line)| line).collect::<Vec<_>>(),
    )
    .wrap(ratatui::widgets::Wrap { trim: false })
    .scroll((u16::try_from(scroll).unwrap_or(u16::MAX), 0));
    ratatui::widgets::Widget::render(paragraph, text_area, b);
    if let Some(track) = track {
        let thumb = cx.thumb_color(thumb_hover, p.overlay1);
        crate::ui::render_scrollbar_buffer(b, metrics, track, p.overlay0, thumb, "▐");
    }

    if let Some(footer_area) = stack.footer {
        let t = &crate::i18n::texts().overlays;
        super::render_key_hints(
            b,
            footer_area,
            &[
                ("wheel ↑↓".to_owned(), t.hint_scroll.to_owned()),
                ("esc / enter".to_owned(), t.footer_close.to_owned()),
            ],
            p,
            cx.components,
        );
    }

    Some(ScrollbackOverlayRender {
        area: outer,
        close,
        track,
        metrics: Some(metrics),
        max_scroll,
    })
}
fn render_release_notes_overlay(
    b: &mut Buffer,
    notes: &crate::app::state::ReleaseNotesState,
    install_command: &str,
    cx: &ChromeContext<'_>,
) -> Option<OverlayRender> {
    let subtitle = if notes.preview {
        crate::i18n::texts().overlays.update_ready
    } else {
        crate::i18n::texts().overlays.whats_new_in_release
    };
    let lines = crate::ui::release_notes_display_lines(notes, install_command, cx.palette);
    let rendered = render_scrollback_overlay(
        b,
        crate::ui::ModalSize::Content {
            width: crate::ui::RELEASE_NOTES_MODAL_SIZE.0,
            height: crate::ui::RELEASE_NOTES_MODAL_SIZE.1,
        },
        &format!("v{}", notes.version),
        subtitle,
        lines,
        notes.scroll,
        &ChromeHover::ReleaseNotesScrollbarThumb,
        cx,
    )?;
    Some(OverlayRender {
        area: rendered.area,
        primary: rendered.close,
        release_notes_scrollbar: rendered.track.unwrap_or_default(),
        release_notes_scroll_metrics: rendered.metrics,
        release_notes_max_scroll: rendered.max_scroll,
        ..OverlayRender::default()
    })
}

fn render_product_announcement_overlay(
    b: &mut Buffer,
    announcement: &crate::app::state::ProductAnnouncementState,
    cx: &ChromeContext<'_>,
) -> Option<OverlayRender> {
    let subtitle = if announcement.preview {
        crate::i18n::texts().overlays.product_announcement_preview
    } else {
        crate::i18n::texts().overlays.product_announcement
    };
    let subtitle = format!("{subtitle} · v{}", announcement.version);
    let lines = crate::ui::product_announcement_display_lines(announcement, cx.palette);
    let rendered = render_scrollback_overlay(
        b,
        crate::ui::ModalSize::XLarge,
        &announcement.title,
        &subtitle,
        lines,
        announcement.scroll,
        &ChromeHover::ProductAnnouncementScrollbarThumb,
        cx,
    )?;
    Some(OverlayRender {
        area: rendered.area,
        primary: rendered.close,
        product_announcement_scrollbar: rendered.track.unwrap_or_default(),
        product_announcement_scroll_metrics: rendered.metrics,
        product_announcement_max_scroll: rendered.max_scroll,
        ..OverlayRender::default()
    })
}

fn render_onboarding_overlay(b: &mut Buffer, cx: &ChromeContext<'_>) -> Option<OverlayRender> {
    let p = cx.palette;
    let (outer, inner) = modal_panel(b, crate::ui::ModalSize::Medium, p.accent, cx)?;
    if inner.height < 11 {
        return Some(OverlayRender {
            area: outer,
            ..OverlayRender::default()
        });
    }
    let stack = crate::ui::modal_stack_areas(inner, 2, 0, 1, 1);
    let base = Style::default()
        .bg(p.panel_bg)
        .remove_modifier(Modifier::DIM);
    let title = base.fg(p.text).add_modifier(Modifier::BOLD);
    let muted = base.fg(p.overlay0);
    let text = base.fg(p.overlay1);
    let accent = base.fg(p.accent).add_modifier(Modifier::BOLD);

    put_text(
        b,
        stack.header.x,
        stack.header.y,
        stack.header.width,
        crate::ui::ONBOARDING_TITLE,
        title,
    );
    put_text(
        b,
        stack.header.x,
        stack.header.y.saturating_add(1),
        stack.header.width,
        crate::i18n::texts().onboarding.subtitle,
        muted,
    );

    let content = stack.content;
    for (offset, line) in crate::i18n::texts()
        .onboarding
        .description
        .iter()
        .enumerate()
    {
        put_text(
            b,
            content.x,
            content.y.saturating_add(offset as u16),
            content.width,
            line,
            text,
        );
    }

    let key_y = content.y.saturating_add(4);
    let mut key_x = content.x;
    for (value, style) in [
        ("  ", base),
        (crate::ui::ONBOARDING_PREFIX_LABEL, accent),
        (crate::i18n::texts().onboarding.prefix_suffix, text),
        (crate::ui::ONBOARDING_HELP_LABEL, accent),
        (crate::i18n::texts().onboarding.help_suffix, text),
    ] {
        let width = display_width(value);
        put_text(
            b,
            key_x,
            key_y,
            content.right().saturating_sub(key_x),
            value,
            style,
        );
        key_x = key_x.saturating_add(width);
    }
    put_text(
        b,
        content.x,
        content.y.saturating_add(5),
        content.width,
        crate::i18n::texts().onboarding.next,
        text,
    );

    let primary = crate::ui::onboarding_welcome_continue_rect(stack.actions.unwrap_or_default());
    modal_button(
        b,
        primary,
        crate::ui::modal_continue_button_text(),
        crate::ui::ModalButtonTone::Primary,
        cx.button_state(
            &ChromeHover::OverlayPrimary,
            crate::ui::ModalButtonState::Focused,
        ),
        p,
    );
    Some(OverlayRender {
        area: outer,
        primary,
        ..OverlayRender::default()
    })
}

fn render_rename_overlay(
    b: &mut Buffer,
    v: &ClientRenameOverlay,
    cx: &ChromeContext<'_>,
) -> Option<OverlayRender> {
    let p = cx.palette;
    let (q, i) = modal_panel(b, crate::ui::ModalSize::Small, p.accent, cx)?;
    let stack = crate::ui::modal_stack_areas(i, 1, 0, 1, 1);
    put_text(
        b,
        stack.header.x,
        stack.header.y,
        stack.header.width,
        v.title,
        Style::default()
            .fg(p.text)
            .bg(p.panel_bg)
            .add_modifier(Modifier::BOLD),
    );
    let input = Rect::new(stack.content.x, stack.content.y, stack.content.width, 1);
    let field = crate::ui::input_field_style(p);
    b.set_style(input, field);
    let cursor = text_editor::render(
        b,
        Rect::new(input.x + 1, input.y, input.width.saturating_sub(1), 1),
        &v.input,
        field,
    );
    let rs = modal_button_row(
        stack.actions.unwrap_or_default(),
        &[
            crate::i18n::texts().overlays.save_button,
            crate::i18n::texts().overlays.clear_button,
            crate::i18n::texts().overlays.cancel_button,
        ],
        2,
    );
    let [save, clear, cancel] = rs.as_slice() else {
        return None;
    };
    modal_button(
        b,
        *save,
        crate::i18n::texts().overlays.save_button,
        crate::ui::ModalButtonTone::Primary,
        cx.button_state(
            &ChromeHover::OverlayPrimary,
            crate::ui::ModalButtonState::Focused,
        ),
        p,
    );
    modal_button(
        b,
        *clear,
        crate::i18n::texts().overlays.clear_button,
        crate::ui::ModalButtonTone::Secondary,
        cx.button_state(
            &ChromeHover::OverlayClear,
            crate::ui::ModalButtonState::Normal,
        ),
        p,
    );
    modal_button(
        b,
        *cancel,
        crate::i18n::texts().overlays.cancel_button,
        crate::ui::ModalButtonTone::Secondary,
        cx.button_state(
            &ChromeHover::OverlayCancel,
            crate::ui::ModalButtonState::Normal,
        ),
        p,
    );
    Some(OverlayRender {
        area: q,
        primary: *save,
        clear: *clear,
        cancel: *cancel,
        navigator_popup: Rect::default(),
        navigator_search: Rect::default(),
        navigator_rows: Vec::new(),
        worktree_search: Rect::default(),
        worktree_rows: Vec::new(),
        cursor,
        ..OverlayRender::default()
    })
}

/// Mark following siblings in preorder without rescanning descendants for each row.
/// The reverse stack contains at most one entry per depth; every entry is pushed
/// and popped at most once, so this pass is linear in the number of rows.
fn navigator_following_siblings(rows: &[ClientNavigatorRow]) -> Vec<bool> {
    let mut following = vec![false; rows.len()];
    let mut depths = Vec::new();
    for (index, row) in rows.iter().enumerate().rev() {
        while depths.last().is_some_and(|depth| *depth > row.depth) {
            depths.pop();
        }
        following[index] = depths.last() == Some(&row.depth);
        if !following[index] {
            depths.push(row.depth);
        }
    }
    following
}

fn render_navigator_overlay(
    b: &mut Buffer,
    n: &ClientNavigatorOverlay,
    endpoints: &[ClientShellEndpoint],
    active_endpoint_id: &ClientEndpointId,
    cx: &ChromeContext<'_>,
) -> Option<OverlayRender> {
    let p = cx.palette;
    let a = b.area;
    let mx = (a.width / 16).max(2);
    let my = (a.height / 10).max(1);
    let q = Rect::new(
        a.x + mx,
        a.y + my,
        a.width.saturating_sub(mx * 2).max(4),
        a.height.saturating_sub(my * 2).max(4),
    )
    .intersection(a);
    let i = panel(b, q, p.accent, p.panel_bg, cx.glyphs)?;
    let rows = super::aggregate_navigation::navigator_rows(endpoints, active_endpoint_id, n);
    let count = format!(
        "{} panes",
        rows.iter()
            .filter(|row| matches!(row.target, ClientNavigatorTarget::Pane { .. }))
            .count()
    );
    let filter_label = n.filter.map(|f| {
        match f {
            ClientNavigatorFilter::Blocked => crate::i18n::texts().overlays.filter_blocked,
            ClientNavigatorFilter::Working => crate::i18n::texts().overlays.filter_working,
            ClientNavigatorFilter::Idle => crate::i18n::texts().overlays.filter_idle,
            ClientNavigatorFilter::Done => crate::i18n::texts().overlays.filter_done,
        }
        .to_owned()
    });
    let cursor = render_search_bar(
        b,
        Rect::new(i.x, i.y, i.width, 1),
        &SearchBar {
            focused: n.search_focused,
            query: &n.query,
            hint: crate::i18n::texts().overlays.search_panes_hint,
            status: filter_label,
            echo_query: true,
            count: Some(count),
        },
        p,
    );
    put_text(
        b,
        i.x,
        i.y + 1,
        i.width,
        &"─".repeat(i.width as usize),
        Style::default().fg(p.surface1).bg(p.panel_bg),
    );
    let body = Rect::new(i.x, i.y + 2, i.width, i.height.saturating_sub(4));
    let selected = super::aggregate_navigation::navigator_selected_index(&rows, n).unwrap_or(0);
    let max = rows.len().saturating_sub(body.height as usize);
    let scroll = n
        .scroll
        .max(selected.saturating_sub(body.height.saturating_sub(1) as usize))
        .min(selected)
        .min(max);
    let hovered = n
        .hovered
        .as_ref()
        .and_then(|target| rows.iter().position(|row| row.target == *target));
    let following_siblings = navigator_following_siblings(&rows);
    let mut ancestor_siblings = Vec::new();
    let federated = endpoints.len() > 1;
    let mut row_hits = Vec::new();
    for (ix, r) in rows.iter().enumerate().take(scroll + body.height as usize) {
        ancestor_siblings.truncate(usize::from(r.depth));
        ancestor_siblings.push(following_siblings[ix]);
        if ix < scroll {
            continue;
        }
        let rect = Rect::new(body.x, body.y + (ix - scroll) as u16, body.width, 1);
        row_hits.push((rect, r.target.clone()));
        // 三态与其它浮层同一口径：选中 accent 反色 > 悬浮弱底色 > 常态。
        // stale 行保留它自己的 surface0 选中底色（它不参与反色）。
        let is_hovered = ix != selected && hovered == Some(ix);
        let row_bg = super::list_row_bg(p, cx.components, false, is_hovered);
        let st = if r.stale {
            Style::default()
                .fg(p.overlay0)
                .bg(if ix == selected { p.surface0 } else { row_bg })
                .add_modifier(Modifier::DIM)
        } else if ix == selected {
            Style::default()
                .fg(panel_contrast_fg(p))
                .bg(p.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(if r.current { p.text } else { p.subtext0 })
                .bg(row_bg)
        };
        b.set_style(rect, st);
        let tree = match &r.target {
            ClientNavigatorTarget::Machine { .. } => "▾".to_owned(),
            ClientNavigatorTarget::Workspace {
                endpoint_id,
                workspace_id,
            } if n
                .expanded_workspaces
                .contains(&(endpoint_id.clone(), workspace_id.clone())) =>
            {
                if r.depth == 0 { "▾" } else { "  ▾" }.to_owned()
            }
            ClientNavigatorTarget::Workspace { .. } => {
                if r.depth == 0 { "▸" } else { "  ▸" }.to_owned()
            }
            ClientNavigatorTarget::Tab { .. } | ClientNavigatorTarget::Pane { .. } => {
                // Machines and workspaces keep their existing caret decoration.
                // Connected branches begin below each workspace.
                let mut prefix = if federated { "    " } else { "" }.to_owned();
                for &following in ancestor_siblings
                    .iter()
                    .take(usize::from(r.depth))
                    .skip(if federated { 2 } else { 1 })
                {
                    prefix.push_str(if following { "│  " } else { "   " });
                }
                prefix.push_str(if following_siblings[ix] {
                    "├──"
                } else {
                    "└──"
                });
                prefix
            }
        };
        let current = if r.current { "◆ " } else { "" };
        let status = r.status.map(status_dot).unwrap_or_default();
        let status_separator = if status.is_empty() { "" } else { " " };
        let label = format!(" {tree} {current}{status}{status_separator}{}", r.label);
        put_text(b, rect.x, rect.y, rect.width, &label, st);
        if let Some(status) = r.status {
            let prefix = format!(" {tree} {current}");
            let status_style = if r.stale || ix == selected {
                st
            } else {
                Style::default().fg(status_color(status, p)).bg(row_bg)
            };
            put_text(
                b,
                rect.x.saturating_add(display_width(&prefix)),
                rect.y,
                display_width(status_dot(status)),
                status_dot(status),
                status_style,
            );
        }
        let machine_status = match &r.target {
            ClientNavigatorTarget::Machine { endpoint_id } if !endpoint_id.is_local() => endpoints
                .iter()
                .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
                .map(|endpoint| endpoint.status),
            _ => None,
        };
        if let Some(status) = machine_status {
            let (glyph, state, color) = endpoint_status_presentation(status, p, cx.spinner);
            let signal = if status == ClientEndpointStatus::Online {
                glyph.to_owned()
            } else {
                format!("{glyph} {state}")
            };
            let signal_style = if ix == selected {
                st
            } else {
                Style::default()
                    .fg(color)
                    .bg(row_bg)
                    .add_modifier(if r.stale {
                        Modifier::DIM
                    } else {
                        Modifier::empty()
                    })
            };
            put_right_text(b, rect, rect.y, &signal, signal_style);
        } else if !r.meta.is_empty() {
            let label_width = display_width(&label).min(rect.width);
            let meta = Rect::new(
                rect.x.saturating_add(label_width).saturating_add(1),
                rect.y,
                rect.width.saturating_sub(label_width.saturating_add(1)),
                1,
            );
            put_right_text(b, meta, rect.y, &r.meta, st)
        }
    }
    let dy = i.bottom() - 2;
    if let Some(r) = rows.get(selected) {
        put_text(
            b,
            i.x,
            dy,
            i.width,
            &format!(" {} · {}", r.label, r.meta),
            Style::default().fg(p.overlay0).bg(p.panel_bg),
        )
    }
    put_text(
        b,
        i.x,
        i.bottom() - 1,
        i.width,
        if n.search_focused {
            crate::i18n::texts().overlays.navigator_search_footer
        } else {
            crate::i18n::texts().overlays.navigator_footer
        },
        Style::default().fg(p.overlay0).bg(p.panel_bg),
    );
    Some(OverlayRender {
        area: q,
        primary: Rect::default(),
        clear: Rect::default(),
        cancel: Rect::default(),
        navigator_popup: q,
        navigator_search: Rect::new(i.x, i.y, i.width, 1),
        navigator_rows: row_hits,
        worktree_search: Rect::default(),
        worktree_rows: Vec::new(),
        cursor,
        ..OverlayRender::default()
    })
}

fn help_lines(
    keybinds: &LiveKeybindConfig,
    query: &str,
    palette: &Palette,
) -> Vec<(usize, ratatui::text::Line<'static>)> {
    use ratatui::text::{Line, Span};

    let groups = crate::input::filter_keybind_help_groups(
        crate::input::keybind_help_groups(&keybinds.keybinds, keybinds.prefix),
        query,
    );
    let key_width = groups
        .iter()
        .flat_map(|(_, entries)| {
            entries
                .iter()
                .map(|(key, _)| usize::from(display_width(key)))
        })
        .max()
        .unwrap_or(8);
    if groups.is_empty() {
        let message = crate::i18n::texts().overlays.no_matching_keybinds;
        return vec![(
            usize::from(display_width(message)),
            Line::from(Span::styled(
                message,
                Style::default().fg(palette.overlay1).bg(palette.panel_bg),
            )),
        )];
    }

    let mut lines = Vec::new();
    for (group, entries) in groups {
        lines.push((
            usize::from(display_width(group)) + 1,
            Line::from(Span::styled(
                format!(" {group}"),
                Style::default()
                    .fg(palette.accent)
                    .bg(palette.panel_bg)
                    .add_modifier(Modifier::BOLD),
            )),
        ));
        for (key, label) in entries {
            let padded_key = format!(" {key:<key_width$} ");
            let width =
                usize::from(display_width(&padded_key)) + usize::from(display_width(&label));
            lines.push((
                width,
                Line::from(vec![
                    Span::styled(
                        padded_key,
                        Style::default()
                            .fg(palette.mauve)
                            .bg(palette.panel_bg)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        label.into_owned(),
                        Style::default().fg(palette.text).bg(palette.panel_bg),
                    ),
                ]),
            ));
        }
        lines.push((0, Line::raw("")));
    }
    lines
}

fn render_help_overlay(
    b: &mut Buffer,
    h: &ClientHelpOverlay,
    k: &LiveKeybindConfig,
    cx: &ChromeContext<'_>,
) -> Option<OverlayRender> {
    use ratatui::widgets::{Paragraph, Widget, Wrap};

    let p = cx.palette;
    let (q, i) = modal_panel(b, crate::ui::ModalSize::Large, p.accent, cx)?;
    if i.width < 20 || i.height < 6 {
        return None;
    }
    let stack = crate::ui::modal_stack_areas(i, 2, 1, 0, 1);
    put_text(
        b,
        stack.header.x,
        stack.header.y,
        stack.header.width,
        crate::i18n::texts().overlays.keybinds_title,
        Style::default()
            .fg(p.text)
            .bg(p.panel_bg)
            .add_modifier(Modifier::BOLD),
    );
    let close_label = if h.search_focused {
        crate::i18n::texts().overlays.back_button
    } else {
        crate::ui::modal_close_button_text()
    };
    let close_width = crate::ui::modal_button_width(close_label);
    let close = Rect::new(
        stack.header.right().saturating_sub(close_width),
        stack.header.y,
        close_width,
        1,
    );
    modal_button(
        b,
        close,
        close_label,
        crate::ui::ModalButtonTone::Primary,
        cx.button_state(
            &ChromeHover::OverlayCancel,
            crate::ui::ModalButtonState::Focused,
        ),
        p,
    );
    let cursor = render_search_bar(
        b,
        Rect::new(i.x, stack.header.y + 1, i.width, 1),
        &SearchBar {
            focused: h.search_focused,
            query: &h.query,
            hint: crate::i18n::texts().overlays.help_filter_hint,
            status: None,
            echo_query: false,
            count: None,
        },
        p,
    );

    let body = stack.content;
    let lines = help_lines(k, &h.query, p);
    let viewport_rows = usize::from(body.height.max(1));
    let wrapped_rows = |width: u16| {
        let width = usize::from(width.max(1));
        lines
            .iter()
            .map(|(line_width, _)| line_width.max(&1).div_ceil(width))
            .sum::<usize>()
    };
    let needs_scrollbar = wrapped_rows(body.width) > viewport_rows;
    let text_area = if needs_scrollbar {
        Rect::new(body.x, body.y, body.width.saturating_sub(1), body.height)
    } else {
        body
    };
    let total_rows = wrapped_rows(text_area.width);
    let max_scroll = total_rows.saturating_sub(viewport_rows);
    let scroll = h.scroll.min(max_scroll);
    let metrics = crate::pane::ScrollMetrics {
        offset_from_bottom: max_scroll.saturating_sub(scroll),
        max_offset_from_bottom: max_scroll,
        viewport_rows,
    };
    let scrollbar = crate::ui::release_notes_scrollbar_rect(body, metrics);
    Widget::render(
        Paragraph::new(lines.into_iter().map(|(_, line)| line).collect::<Vec<_>>())
            .wrap(Wrap { trim: false })
            .scroll((u16::try_from(scroll).unwrap_or(u16::MAX), 0)),
        text_area,
        b,
    );
    if let Some(track) = scrollbar {
        let thumb = cx.thumb_color(&ChromeHover::HelpScrollbarThumb, p.overlay1);
        crate::ui::render_scrollbar_buffer(b, metrics, track, p.overlay0, thumb, "▐");
    }

    if let Some(footer) = stack.footer {
        put_text(
            b,
            footer.x,
            footer.y,
            footer.width,
            if h.search_focused {
                crate::i18n::texts().overlays.edit_footer
            } else {
                crate::i18n::texts().overlays.search_footer
            },
            Style::default().fg(p.overlay0).bg(p.panel_bg),
        );
    }
    Some(OverlayRender {
        area: q,
        cancel: close,
        help_popup: q,
        help_scrollbar: scrollbar.unwrap_or_default(),
        help_scroll_metrics: Some(metrics),
        help_max_scroll: max_scroll,
        cursor,
        ..OverlayRender::default()
    })
}
fn render_confirm_close_overlay(
    b: &mut Buffer,
    c: &ClientConfirmCloseOverlay,
    cx: &ChromeContext<'_>,
) -> Option<OverlayRender> {
    let p = cx.palette;
    let (q, i) = modal_panel(b, crate::ui::ModalSize::Medium.with_height(6), p.red, cx)?;
    let stack = crate::ui::modal_stack_areas(i, 2, 0, 1, 0);
    put_text(
        b,
        stack.header.x,
        stack.header.y,
        stack.header.width,
        &format!(" {}", c.title),
        Style::default()
            .fg(p.red)
            .bg(p.panel_bg)
            .add_modifier(Modifier::BOLD),
    );
    put_text(
        b,
        stack.header.x,
        stack.header.y + 1,
        stack.header.width,
        &format!(" {}", c.detail),
        Style::default().fg(p.text).bg(p.panel_bg),
    );
    let rs = modal_button_row(
        stack.actions.unwrap_or_default(),
        &[
            crate::i18n::texts().overlays.confirm_button,
            crate::i18n::texts().overlays.cancel_button,
        ],
        2,
    );
    let [ok, cancel] = rs.as_slice() else {
        return None;
    };
    modal_button(
        b,
        *ok,
        crate::i18n::texts().overlays.confirm_button,
        crate::ui::ModalButtonTone::Danger,
        cx.button_state(
            &ChromeHover::OverlayPrimary,
            crate::ui::ModalButtonState::Focused,
        ),
        p,
    );
    modal_button(
        b,
        *cancel,
        crate::i18n::texts().overlays.cancel_button,
        crate::ui::ModalButtonTone::Secondary,
        cx.button_state(
            &ChromeHover::OverlayCancel,
            crate::ui::ModalButtonState::Normal,
        ),
        p,
    );
    Some(OverlayRender {
        area: q,
        primary: *ok,
        clear: Rect::default(),
        cancel: *cancel,
        navigator_popup: Rect::default(),
        navigator_search: Rect::default(),
        navigator_rows: Vec::new(),
        worktree_search: Rect::default(),
        worktree_rows: Vec::new(),
        cursor: None,
        ..OverlayRender::default()
    })
}

/// Notification history modal: one row per ring entry (oldest on top, the
/// selection auto-followed into view), level icon + title + relative time.
fn render_notification_history_overlay(
    b: &mut Buffer,
    o: &super::feedback::ClientNotificationHistoryOverlay,
    history: &std::collections::VecDeque<ClientNotificationRecord>,
    cx: &ChromeContext<'_>,
) -> Option<OverlayRender> {
    let p = cx.palette;
    let t = &crate::i18n::texts().history;
    let (q, i) = modal_panel(b, crate::ui::ModalSize::Large, p.accent, cx)?;
    let stack = crate::ui::modal_stack_areas(i, 1, 1, 0, 1);
    let base = Style::default()
        .bg(p.panel_bg)
        .remove_modifier(Modifier::DIM);
    put_text(
        b,
        stack.header.x,
        stack.header.y,
        stack.header.width,
        t.title,
        base.fg(p.text).add_modifier(Modifier::BOLD),
    );
    let body = stack.content;
    let mut rows = Vec::new();
    let count = history.len();
    if count == 0 {
        if !body.is_empty() {
            put_text(b, body.x, body.y, body.width, t.empty, base.fg(p.overlay0));
        }
    } else {
        let viewport = usize::from(body.height.max(1));
        let selected = o.selected.min(count.saturating_sub(1));
        let max_scroll = count.saturating_sub(viewport);
        // Auto-follow the selection: reveal it at the bottom edge at most.
        let scroll = selected
            .saturating_add(1)
            .saturating_sub(viewport)
            .min(selected)
            .min(max_scroll);
        for (row_offset, ix) in (scroll..count).take(viewport).enumerate() {
            let Some(record) = history.get(ix) else {
                break;
            };
            let rect = Rect::new(
                body.x,
                body.y.saturating_add(row_offset as u16),
                body.width,
                1,
            );
            let is_selected = ix == selected;
            // 三态与其它浮层同一口径：选中 accent 反色 > 悬浮弱底色 > 常态。
            let is_hovered = !is_selected && o.hovered == Some(ix);
            let row_bg = super::list_row_bg(p, cx.components, is_selected, is_hovered);
            let base = base.bg(row_bg);
            let row_style = if is_selected {
                base.fg(panel_contrast_fg(p)).add_modifier(Modifier::BOLD)
            } else {
                base.fg(p.text)
            };
            b.set_style(rect, row_style);
            let level_color = if is_selected {
                row_style.fg.unwrap_or(p.text)
            } else {
                record.level.color(cx.components)
            };
            put_text(
                b,
                rect.x,
                rect.y,
                2,
                &format!(" {}", record.level.icon()),
                if is_selected {
                    row_style
                } else {
                    base.fg(level_color)
                },
            );
            let time = relative_time_ago(record.received_at, cx.now);
            let time_width = display_width(&time).saturating_add(1);
            let text_width = rect.width.saturating_sub(2 + time_width);
            let title_style = if is_selected {
                row_style
            } else {
                base.fg(p.subtext0)
            };
            match record.body.as_deref().filter(|body| !body.is_empty()) {
                Some(body) => {
                    let combined = format!("{} · {}", record.title, body);
                    put_text(
                        b,
                        rect.x.saturating_add(2),
                        rect.y,
                        text_width,
                        &combined,
                        title_style,
                    );
                }
                None => {
                    put_text(
                        b,
                        rect.x.saturating_add(2),
                        rect.y,
                        text_width,
                        &record.title,
                        title_style,
                    );
                }
            }
            put_right_text(
                b,
                rect,
                rect.y,
                &time,
                if is_selected {
                    row_style
                } else {
                    base.fg(p.overlay0)
                },
            );
            rows.push((rect, ix));
        }
    }
    if let Some(footer) = stack.footer {
        put_text(
            b,
            footer.x,
            footer.y,
            footer.width,
            t.footer,
            base.fg(p.overlay0),
        );
    }
    Some(OverlayRender {
        area: q,
        notification_history_rows: rows,
        ..OverlayRender::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffer_text(buffer: &Buffer, row: u16) -> String {
        (buffer.area.x..buffer.area.right())
            .map(|x| buffer[(x, row)].symbol().to_string())
            .collect()
    }

    fn test_cx<'a>(
        palette: &'a Palette,
        components: &'a crate::app::state::ComponentStyles,
    ) -> ChromeContext<'a> {
        ChromeContext {
            page_bounds: None,
            palette,
            components,
            glyphs: crate::ui::BorderGlyphs::SINGLE,
            hover: None,
            spinner: "◐",
            now: std::time::Instant::now(),
        }
    }

    #[test]
    fn modal_button_row_centers_buttons_sized_by_label_display_width() {
        // CJK labels: " 确定 " is 6 display cells, " esc 取消 " is 10.
        let rects = modal_button_row(Rect::new(0, 5, 40, 1), &[" 确定 ", " esc 取消 "], 2);
        assert_eq!(rects.len(), 2);
        assert_eq!(rects[0].width, 6);
        assert_eq!(rects[1].width, 10);
        let total = 6 + 2 + 10;
        assert_eq!(rects[0].x, (40 - total) / 2);
        assert_eq!(rects[1].x, rects[0].x + 6 + 2);
        assert_eq!(rects[0].y, 5);
    }

    #[test]
    fn modal_panel_centers_tier_and_hides_below_minimum() {
        let palette = Palette::catppuccin();
        let components = crate::app::state::ComponentStyles::from_palette(&palette);
        let cx = test_cx(&palette, &components);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 106, 30));
        let (outer, inner) = modal_panel(
            &mut buffer,
            crate::ui::ModalSize::Medium,
            palette.accent,
            &cx,
        )
        .expect("medium modal");
        assert_eq!(outer, Rect::new(21, 7, 64, 16));
        assert_eq!(inner, Rect::new(22, 8, 62, 14));
        assert_eq!(buffer[(21, 7)].symbol(), "┌");

        let mut tiny = Buffer::empty(Rect::new(0, 0, 8, 5));
        assert!(modal_panel(&mut tiny, crate::ui::ModalSize::Small, palette.accent, &cx).is_none());
    }

    #[test]
    fn search_bar_shows_hint_echo_prompt_and_count() {
        let palette = Palette::catppuccin();
        let area = Rect::new(0, 0, 40, 1);

        let mut buffer = Buffer::empty(area);
        let query = TextEditor::new("", false);
        let cursor = render_search_bar(
            &mut buffer,
            area,
            &SearchBar {
                focused: false,
                query: &query,
                hint: "filter panes",
                status: None,
                echo_query: true,
                count: Some("3 panes".to_owned()),
            },
            &palette,
        );
        assert!(cursor.is_none());
        let text = buffer_text(&buffer, 0);
        assert!(text.starts_with("filter panes"));
        assert!(text.trim_end().ends_with("3 panes"));

        let mut buffer = Buffer::empty(area);
        let query = TextEditor::new("web", false);
        render_search_bar(
            &mut buffer,
            area,
            &SearchBar {
                focused: false,
                query: &query,
                hint: "filter panes",
                status: None,
                echo_query: true,
                count: None,
            },
            &palette,
        );
        assert!(buffer_text(&buffer, 0).starts_with(" / web"));

        let mut buffer = Buffer::empty(area);
        let query = TextEditor::new("web", false);
        let cursor = render_search_bar(
            &mut buffer,
            area,
            &SearchBar {
                focused: true,
                query: &query,
                hint: "filter panes",
                status: Some("working".to_owned()),
                echo_query: true,
                count: Some("3 panes".to_owned()),
            },
            &palette,
        );
        let text = buffer_text(&buffer, 0);
        assert!(text.starts_with(" / web"));
        assert!(text.trim_end().ends_with("3 panes"));
        let cursor = cursor.expect("focused search cursor");
        assert!(cursor.visible);
        assert_eq!(cursor.y, 0);

        // Status wins over the query echo when not focused.
        let mut buffer = Buffer::empty(area);
        render_search_bar(
            &mut buffer,
            area,
            &SearchBar {
                focused: false,
                query: &query,
                hint: "filter panes",
                status: Some("working".to_owned()),
                echo_query: true,
                count: None,
            },
            &palette,
        );
        assert!(buffer_text(&buffer, 0).starts_with(" / working"));
    }
}
