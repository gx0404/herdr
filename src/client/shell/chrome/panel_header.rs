//! 面板头（每个停靠面板的第一行）：画在面板自己的底色上——侧栏类面板用侧栏面，
//! 终端组、Inspector 与监控用面板底色——不再是一条 surface0 色条。
//!
//! 聚焦面板的标题加粗 `text`，其余 `subtext0`（对底色不足 4.5:1 时换更强的色）。
//! `⠿` 拖动把手只在指针悬浮在面板头上或调整布局时画（隐藏时留空位，标题与按钮不
//! 跳动）；`□` / `◫` 最大化与监控面板的 `×` 关闭平时是 `overlay0`，指针悬浮在头上、
//! 调整布局模式或面板聚焦时才亮起来。终端组把标签条并进同一行：
//! `⠿ 工作区  标签…  +  …  □`——[`PanelHeaderHits::tabs`] 是留给标签条的区域。

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

use super::{paint_spans, Span};
use crate::app::state::{ComponentStyles, Palette};
use crate::ui::display_width_u16;

/// 面板头右侧的按钮。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(in crate::client::shell) enum PanelButton {
    Maximize,
    Close,
}

/// 指针在面板头上的位置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::client::shell) enum HeaderHover {
    Header,
    Button(PanelButton),
}

/// 一帧面板头的内容。
pub(in crate::client::shell) struct PanelHeaderView<'a> {
    pub(in crate::client::shell) title: &'a str,
    pub(in crate::client::shell) focused: bool,
    /// 给 `⠿` 留不留槽位（拖得动：锁定、紧凑、最大化、不接鼠标时不留）。
    pub(in crate::client::shell) handle: bool,
    /// 槽位里这一帧画不画 `⠿`（悬浮 / 调整布局 / 关了悬浮反馈时）；不画时留空格。
    pub(in crate::client::shell) handle_visible: bool,
    /// 最大化中：按钮画 `◫`（还原），否则 `□`。
    pub(in crate::client::shell) maximized: bool,
    pub(in crate::client::shell) closable: bool,
    pub(in crate::client::shell) hovered: Option<HeaderHover>,
    pub(in crate::client::shell) arranging: bool,
    /// 终端组：标题后留出标签条区域，标题最多占可用宽度的四分之一。
    pub(in crate::client::shell) tabs: bool,
    pub(in crate::client::shell) bg: Color,
    pub(in crate::client::shell) icons: &'a [(&'a str, bool)],
}

/// 画出来的区域。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(in crate::client::shell) struct PanelHeaderHits {
    /// 整行（按下开始停靠拖动 / 切焦点），按钮与标签在它之上。
    pub(in crate::client::shell) header: Rect,
    pub(in crate::client::shell) maximize: Rect,
    pub(in crate::client::shell) close: Rect,
    /// 留给标签条的区域（非终端组为空）。
    pub(in crate::client::shell) tabs: Rect,
    pub(in crate::client::shell) icons: [Rect; 2],
}

const HANDLE: &str = "⠿";
const BUTTON_WIDTH: u16 = 3;
/// 终端组标题最窄留这么多列，再窄就把整行让给标签条。
const TITLE_MIN: u16 = 4;

/// 标题文字的颜色：聚焦面板 `text`，其余 `subtext0`（读不清时换 `text`）。
pub(in crate::client::shell) fn title_color(palette: &Palette, bg: Color, focused: bool) -> Color {
    if focused {
        palette.text
    } else {
        super::readable_on(palette, bg, &[palette.subtext0, palette.text])
    }
}

/// 把手与按钮的颜色：平时 `overlay0`；面板聚焦、调整布局或指针在头上时亮一档。
fn control_color(palette: &Palette, view: &PanelHeaderView<'_>) -> Color {
    if view.hovered.is_some() {
        palette.text
    } else if view.focused || view.arranging {
        palette.subtext0
    } else {
        palette.overlay0
    }
}

/// 画面板头，返回各区域。
pub(in crate::client::shell) fn paint(
    buffer: &mut Buffer,
    row: Rect,
    view: &PanelHeaderView<'_>,
    palette: &Palette,
    components: &ComponentStyles,
) -> PanelHeaderHits {
    let mut hits = PanelHeaderHits::default();
    if row.is_empty() {
        return hits;
    }
    let row = Rect::new(row.x, row.y, row.width, 1);
    buffer.set_style(
        row,
        Style::default()
            .bg(view.bg)
            .fg(palette.text)
            .remove_modifier(Modifier::all()),
    );
    for x in row.x..row.right() {
        buffer[(x, row.y)].set_symbol(" ");
    }
    hits.header = row;
    let control = control_color(palette, view);

    // 右侧按钮：最大化在最右，监控面板的关闭在它左边。
    let buttons = if view.closable && row.width > BUTTON_WIDTH * 2 {
        BUTTON_WIDTH * 2
    } else {
        BUTTON_WIDTH.min(row.width)
    };
    let maximize = Rect::new(
        row.right().saturating_sub(BUTTON_WIDTH).max(row.x),
        row.y,
        BUTTON_WIDTH.min(row.width),
        1,
    );
    let button_style = |button: PanelButton| {
        if view.hovered == Some(HeaderHover::Button(button)) {
            Style::default().fg(palette.text).bg(components.hover_bg)
        } else {
            Style::default().fg(control)
        }
    };
    paint_spans(
        buffer,
        maximize.x,
        row.y,
        maximize.right(),
        &[Span::new(
            if view.maximized { " ◫ " } else { " □ " },
            button_style(PanelButton::Maximize),
        )],
    );
    hits.maximize = maximize;
    if view.closable && row.width > BUTTON_WIDTH * 2 {
        let close = Rect::new(
            maximize.x.saturating_sub(BUTTON_WIDTH),
            row.y,
            BUTTON_WIDTH,
            1,
        );
        paint_spans(
            buffer,
            close.x,
            row.y,
            close.right(),
            &[Span::new(" × ", button_style(PanelButton::Close))],
        );
        hits.close = close;
    }

    let mut content_right = row.right().saturating_sub(buttons);
    for (index, (glyph, hovered)) in view.icons.iter().enumerate().take(2).rev() {
        if content_right < row.x.saturating_add(BUTTON_WIDTH * 2 + 4) {
            break;
        }
        let rect = Rect::new(content_right - BUTTON_WIDTH, row.y, BUTTON_WIDTH, 1);
        let style = if *hovered {
            Style::default().fg(palette.text).bg(components.hover_bg)
        } else {
            Style::default().fg(super::readable_on(
                palette,
                view.bg,
                &[palette.overlay1, palette.subtext0],
            ))
        };
        paint_spans(
            buffer,
            rect.x,
            row.y,
            rect.right(),
            &[Span::new(format!(" {glyph} "), style)],
        );
        hits.icons[index] = rect;
        content_right = rect.x;
    }
    let mut x = row.x;
    if view.handle && row.width > buttons + 2 {
        x = paint_spans(
            buffer,
            x,
            row.y,
            content_right,
            &[Span::new(
                if view.handle_visible { HANDLE } else { " " },
                Style::default().fg(control),
            )],
        );
        x = x.saturating_add(1);
    }
    let available = content_right.saturating_sub(1).saturating_sub(x);
    let title_width = display_width_u16(view.title);
    let budget = if view.tabs {
        let quarter = available / 4;
        if quarter < TITLE_MIN {
            0
        } else {
            title_width.min(quarter)
        }
    } else {
        available
    };
    let mut style = Style::default().fg(title_color(palette, view.bg, view.focused));
    if view.focused {
        style = style.add_modifier(Modifier::BOLD);
    }
    if budget > 0 {
        let title = crate::ui::truncate_end(view.title, usize::from(budget));
        x = paint_spans(
            buffer,
            x,
            row.y,
            x.saturating_add(budget),
            &[Span::new(title, style)],
        );
    }
    if view.tabs {
        // 标题与第一个标签之间空 2 列；没有标题时贴着把手。
        let start = if budget > 0 { x.saturating_add(2) } else { x };
        let end = content_right.saturating_sub(1);
        if end > start {
            hits.tabs = Rect::new(start, row.y, end - start, 1);
        }
    }
    hits
}

/// 紧凑视图的面板切换条：每个面板一段 ` 名称 `，段间 1 列；当前面板 `surface1` 底
/// 加粗，其余 `subtext0`。返回每段的 `(矩形, 下标)`；整排放不下返回空（调用方退回
/// 只画聚焦面板的标题）。
pub(in crate::client::shell) fn paint_switcher(
    buffer: &mut Buffer,
    row: Rect,
    names: &[&str],
    current: usize,
    bg: Color,
    palette: &Palette,
) -> Vec<(Rect, usize)> {
    let segments = names
        .iter()
        .map(|name| display_width_u16(name).saturating_add(2))
        .collect::<Vec<_>>();
    let total = segments
        .iter()
        .fold(0u16, |total, width| total.saturating_add(*width))
        .saturating_add(segments.len().saturating_sub(1) as u16);
    if names.len() < 2 || total > row.width {
        return Vec::new();
    }
    let mut x = row.x;
    let mut hits = Vec::with_capacity(names.len());
    for (index, (name, width)) in names.iter().zip(&segments).enumerate() {
        let rect = Rect::new(x, row.y, *width, 1);
        let style = if index == current {
            Style::default()
                .fg(palette.text)
                .bg(palette.surface1)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(title_color(palette, bg, false))
        };
        paint_spans(
            buffer,
            rect.x,
            row.y,
            rect.right(),
            &[Span::new(format!(" {name} "), style)],
        );
        hits.push((rect, index));
        x = x.saturating_add(*width).saturating_add(1);
    }
    hits
}

use super::super::dock::{DockLayout, Geometry, PanelId};
use super::super::feedback::ChromeHover;
use super::super::workbench::interaction::Action;
use super::super::{ClientShellConfig, ShellHitMap, StatusGlyphs};
use crate::protocol::ClientShellSnapshot;
use std::collections::HashMap;

/// 面板标题文字（不含拖动把手）。终端组按它当前（活动）标签所属的工作区命名——与
/// 标题栏面包屑同一口径；一个组里可以混着别的工作区的标签，所以不取第一个标签。
/// 另一个工作区与它同名（按身份比，不按字符串）时带上编号。
pub(in crate::client::shell) fn panel_title(
    dock: &DockLayout,
    snapshot: &ClientShellSnapshot,
    panel: &PanelId,
) -> String {
    use super::super::observability::tr;
    match panel {
        PanelId::Workspaces => tr("Sessions", "会话").to_owned(),
        PanelId::Agents => "Inspector".to_owned(),
        // 已退役的账号面板只为解码旧布局保留（加载时即被丢弃），不会画出来。
        PanelId::Monitor | PanelId::Accounts => tr("MONITOR", "监控").to_owned(),
        PanelId::Terminal(id) => {
            let workspace = dock
                .groups
                .iter()
                .find(|group| group.id == *id)
                .and_then(|group| group.active.as_ref().or(group.tabs.first()))
                .and_then(|tab| snapshot.tabs.iter().find(|entry| &entry.tab_id == tab))
                .and_then(|tab| {
                    snapshot
                        .workspaces
                        .iter()
                        .find(|ws| ws.workspace_id == tab.workspace_id)
                });
            match workspace {
                Some(ws) => {
                    let duplicated = snapshot.workspaces.iter().any(|other| {
                        other.workspace_id != ws.workspace_id && other.label == ws.label
                    });
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

/// 工作台面板头这一帧要读的状态（只读借用，与要改写的命中表 / 标签视口分开传）。
pub(in crate::client::shell) struct HeaderFrame<'a> {
    pub(in crate::client::shell) dock: &'a DockLayout,
    pub(in crate::client::shell) geometry: &'a Geometry,
    pub(in crate::client::shell) controls: &'a Geometry,
    pub(in crate::client::shell) snapshot: &'a ClientShellSnapshot,
    pub(in crate::client::shell) config: &'a ClientShellConfig,
    pub(in crate::client::shell) hover: Option<&'a ChromeHover>,
    pub(in crate::client::shell) arranging: bool,
    pub(in crate::client::shell) visual_bell: bool,
    pub(in crate::client::shell) glyphs: StatusGlyphs,
    /// 快照来自不在线的端点：标签里的状态字形静态、置灰。
    pub(in crate::client::shell) stale: bool,
}

/// 标签视口状态与命中表（按分组 id 索引的滚动 / 揭示戳记，工作台与 chrome 命中表）。
pub(in crate::client::shell) struct HeaderOutputs<'a> {
    pub(in crate::client::shell) tab_scroll: &'a mut HashMap<u64, usize>,
    pub(in crate::client::shell) tab_focus: &'a mut HashMap<u64, (Option<String>, u16)>,
    pub(in crate::client::shell) actions: &'a mut Vec<(Rect, Action)>,
    pub(in crate::client::shell) hits: &'a mut ShellHitMap,
}

/// 指针此刻在哪个面板头上（标签与按钮都算在它们所在的面板头里）。
fn header_hover(frame: &HeaderFrame<'_>, panel: &PanelId) -> Option<HeaderHover> {
    match frame.hover? {
        ChromeHover::PanelHeader(id) if id == panel => Some(HeaderHover::Header),
        ChromeHover::PanelButton(id, button) if id == panel => Some(HeaderHover::Button(*button)),
        ChromeHover::Tab(tab) | ChromeHover::TabClose(tab) => {
            let PanelId::Terminal(group) = panel else {
                return None;
            };
            frame
                .dock
                .groups
                .iter()
                .find(|entry| entry.id == *group)
                .filter(|entry| entry.tabs.iter().any(|id| id == tab))
                .map(|_| HeaderHover::Header)
        }
        _ => None,
    }
}

/// 画工作台每个停靠面板的面板头（终端组把标签条并进同一行；紧凑视图把面板头换成
/// 切换条，终端组的标签条退到第二行），按「整行 → 按钮 → 切换条 / 标签」的顺序登记
/// 命中区（后登记的在上层）。返回这一帧有没有画紧凑视图的切换条。
pub(in crate::client::shell) fn paint_workbench_headers(
    buffer: &mut Buffer,
    frame: &HeaderFrame<'_>,
    out: HeaderOutputs<'_>,
) -> bool {
    let HeaderOutputs {
        tab_scroll,
        tab_focus,
        actions,
        hits,
    } = out;
    let config = frame.config;
    let palette = &config.palette;
    let dock = frame.dock;
    // 标签条里「+」与滚动按钮的悬浮只解析聚焦分组的那一份（悬浮身份不带分组）。
    let mut focused_strip: Option<ShellHitMap> = None;
    let mut compact_switcher_shown = false;
    for (panel, area) in &frame.geometry.panels {
        if area.is_empty() || (frame.geometry.rail && *panel == PanelId::Workspaces) {
            continue;
        }
        let focused = panel == dock.projected_focus(frame.geometry);
        let terminal = matches!(panel, PanelId::Terminal(_));
        let bg = if terminal || matches!(panel, PanelId::Monitor | PanelId::Agents) {
            palette.panel_bg
        } else {
            palette.sidebar_bg
        };
        let row = Rect::new(area.x, area.y, area.width, 1);
        let title = panel_title(dock, frame.snapshot, panel);
        // 紧凑视图只投影聚焦面板：面板头改成切换条，按布局顺序（与调整布局模式里 Tab
        // 轮转同序）列出全部面板；整排放不下才退回只画聚焦面板的标题。
        let switcher = if frame.geometry.compact {
            let panels = frame.controls.panels.clone();
            let titles = panels
                .iter()
                .map(|(id, _)| panel_title(dock, frame.snapshot, id))
                .collect::<Vec<_>>();
            let current = panels.iter().position(|(id, _)| id == panel).unwrap_or(0);
            Some((panels, titles, current))
        } else {
            None
        };
        let closable = *panel == PanelId::Monitor;
        let buttons = if closable && area.width > BUTTON_WIDTH * 2 {
            BUTTON_WIDTH * 2
        } else {
            BUTTON_WIDTH
        };
        let switcher_fits = switcher.as_ref().is_some_and(|(panels, titles, _)| {
            panels.len() > 1
                && titles
                    .iter()
                    .map(|title| display_width_u16(title).saturating_add(3))
                    .fold(0u16, u16::saturating_add)
                    .saturating_sub(1)
                    <= area.width.saturating_sub(buttons)
        });
        use super::sessions::SessionHit;
        let session_icons = *panel == PanelId::Workspaces && !switcher_fits;
        let icon_hover = |hit: &SessionHit| matches!(frame.hover, Some(ChromeHover::Session(current)) if current == hit);
        let icons = [
            ("✚", icon_hover(&SessionHit::NewWorkspace)),
            ("⌕", icon_hover(&SessionHit::Search)),
        ];
        let view = PanelHeaderView {
            icons: if session_icons && config.mouse_capture {
                &icons
            } else {
                &[]
            },
            title: if switcher_fits { "" } else { &title },
            focused,
            // `⠿` 是拖动停靠的把手：锁定布局后拖动被拒绝（冒烟 L1）、紧凑视图与最大化
            // 只投影一个面板没有停靠目标（W2、N20）、不接鼠标时工作台不收鼠标事件
            // （N20），都不画它，标题左移占位。
            handle: !(dock.locked
                || frame.geometry.compact
                || dock.maximized.is_some()
                || !config.mouse_capture),
            // 把手平时隐藏：指针在这个面板头上（含其中的按钮与标签）、调整布局时才画；
            // 关了悬浮反馈就没有悬浮可言，退回常驻。
            handle_visible: header_hover(frame, panel).is_some()
                || frame.arranging
                || !config.feedback.hover_effects,
            maximized: dock.maximized.is_some(),
            closable,
            hovered: header_hover(frame, panel),
            arranging: frame.arranging,
            tabs: terminal && !frame.geometry.compact,
            bg,
        };
        let header = paint(buffer, row, &view, palette, &config.components);
        actions.push((header.header, Action::Header(panel.clone())));
        hits.panel_headers.push((header.header, panel.clone()));
        actions.push((header.maximize, Action::Maximize(panel.clone())));
        hits.panel_buttons
            .push((header.maximize, panel.clone(), PanelButton::Maximize));
        if !header.close.is_empty() {
            actions.push((header.close, Action::Close(panel.clone())));
            hits.panel_buttons
                .push((header.close, panel.clone(), PanelButton::Close));
        }
        for (rect, (binding, hit)) in header.icons.into_iter().zip([
            (
                crate::input::KeybindAction::NewWorkspace,
                super::sessions::SessionHit::NewWorkspace,
            ),
            (
                crate::input::KeybindAction::OpenNavigator,
                super::sessions::SessionHit::Search,
            ),
        ]) {
            if !rect.is_empty() {
                actions.push((rect, Action::Binding(binding)));
                hits.sessions.push((rect, hit));
            }
        }
        if let Some((panels, titles, current)) = switcher.filter(|_| switcher_fits) {
            let names = titles.iter().map(String::as_str).collect::<Vec<_>>();
            let segments = paint_switcher(buffer, row, &names, current, bg, palette);
            compact_switcher_shown |= !segments.is_empty();
            // 不接鼠标（`mouse_capture = false`）时工作台整体不收鼠标事件，点不到的
            // 切换区不登记；切换条照画，只标出当前在哪个面板（W1）。
            if config.mouse_capture {
                for (rect, index) in segments {
                    if let Some((candidate, _)) = panels.get(index) {
                        actions.push((rect, Action::Switch(candidate.clone())));
                    }
                }
            }
        }
        let PanelId::Terminal(id) = panel else {
            continue;
        };
        let Some(group) = dock.groups.iter().find(|group| group.id == *id) else {
            continue;
        };
        let strip_area = if view.tabs {
            header.tabs
        } else {
            Rect::new(area.x, area.y + 1, area.width, u16::from(area.height > 1))
        };
        let tabs = group
            .tabs
            .iter()
            .filter_map(|id| frame.snapshot.tabs.iter().find(|tab| &tab.tab_id == id))
            .collect::<Vec<_>>();
        let scroll = tab_scroll.entry(*id).or_default();
        let stamp = (group.active.clone(), area.width);
        let mut reveal = tab_focus.get(id) != Some(&stamp);
        tab_focus.insert(*id, stamp);
        let hover = frame.hover.filter(|hover| {
            focused
                || !matches!(
                    hover,
                    ChromeHover::NewTab | ChromeHover::TabScrollLeft | ChromeHover::TabScrollRight
                )
        });
        let mut strip = ShellHitMap::default();
        super::super::render::render_tab_strip(
            buffer,
            strip_area,
            config,
            scroll,
            &mut reveal,
            super::super::render::TabStripContext {
                tabs: &tabs,
                focused: group.active.as_deref(),
                status: None,
                agents: &frame.snapshot.agents,
                glyphs: frame.glyphs,
                stale: frame.stale,
                bg: palette.panel_bg,
                hover,
                visual_bell: frame.visual_bell && focused,
                insert_index: None,
            },
            &mut strip,
        );
        for (rect, tab) in &strip.tabs {
            actions.push((
                *rect,
                Action::Tab {
                    group: *id,
                    tab: tab.clone(),
                },
            ));
        }
        for (rect, tab) in &strip.tab_close {
            actions.push((*rect, Action::CloseTab(tab.clone())));
        }
        for (rect, action) in [
            (strip.tab_scroll_left, Action::ScrollTabs(*id, -1)),
            (strip.tab_scroll_right, Action::ScrollTabs(*id, 1)),
            (strip.new_tab, Action::NewTab(*id)),
        ] {
            if !rect.is_empty() {
                actions.push((rect, action));
            }
        }
        hits.tabs.append(&mut strip.tabs);
        hits.tab_close.append(&mut strip.tab_close);
        if focused || focused_strip.is_none() {
            focused_strip = Some(strip);
        }
    }
    if let Some(strip) = focused_strip {
        hits.new_tab = strip.new_tab;
        hits.tab_scroll_left = strip.tab_scroll_left;
        hits.tab_scroll_right = strip.tab_scroll_right;
    }
    compact_switcher_shown
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(title: &str) -> PanelHeaderView<'_> {
        PanelHeaderView {
            title,
            focused: false,
            handle: true,
            handle_visible: true,
            maximized: false,
            closable: false,
            hovered: None,
            arranging: false,
            tabs: false,
            bg: Palette::graphite_dark().panel_bg,
            icons: &[],
        }
    }

    fn row_text(buffer: &Buffer) -> String {
        (0..buffer.area.width)
            .map(|x| buffer[(x, 0)].symbol().to_owned())
            .collect()
    }

    #[test]
    fn headers_sit_on_the_panel_background_with_dim_controls() {
        let palette = Palette::graphite_dark();
        let components = ComponentStyles::from_palette(&palette);
        let area = Rect::new(0, 0, 30, 1);
        let mut buffer = Buffer::empty(area);
        let hits = paint(&mut buffer, area, &view("MONITOR"), &palette, &components);
        assert_eq!(row_text(&buffer), format!("⠿ MONITOR{}□ ", " ".repeat(19)));
        assert_eq!(buffer[(0, 0)].fg, palette.overlay0, "把手平时变暗");
        assert_eq!(buffer[(28, 0)].fg, palette.overlay0, "按钮平时变暗");
        assert_eq!(buffer[(2, 0)].bg, palette.panel_bg, "不再是 surface0 色条");
        assert_eq!(hits.maximize, Rect::new(27, 0, 3, 1));
        assert!(hits.tabs.is_empty());

        let mut lit = view("MONITOR");
        lit.focused = true;
        lit.closable = true;
        let mut buffer = Buffer::empty(area);
        let hits = paint(&mut buffer, area, &lit, &palette, &components);
        assert_eq!(buffer[(0, 0)].fg, palette.subtext0, "聚焦时控件亮一档");
        assert_eq!(buffer[(2, 0)].fg, palette.text);
        assert!(
            buffer[(2, 0)].modifier.contains(Modifier::BOLD),
            "聚焦标题加粗"
        );
        assert_eq!(hits.close, Rect::new(24, 0, 3, 1));
        assert_eq!(buffer[(25, 0)].symbol(), "×");

        let mut hovered = view("MONITOR");
        hovered.closable = true;
        hovered.hovered = Some(HeaderHover::Button(PanelButton::Close));
        let mut buffer = Buffer::empty(area);
        paint(&mut buffer, area, &hovered, &palette, &components);
        assert_eq!(buffer[(25, 0)].bg, components.hover_bg, "悬浮的按钮换底");
        assert_eq!(buffer[(28, 0)].fg, palette.text, "悬浮时其它控件也亮");
    }

    #[test]
    fn terminal_headers_leave_the_rest_of_the_row_to_the_tab_strip() {
        let palette = Palette::graphite_dark();
        let components = ComponentStyles::from_palette(&palette);
        let area = Rect::new(10, 0, 60, 1);
        let mut terminal = view("client-shell");
        terminal.tabs = true;
        let mut buffer = Buffer::empty(Rect::new(0, 0, 70, 1));
        let hits = paint(&mut buffer, area, &terminal, &palette, &components);
        // 把手 2 列 + 标题 12 列 + 空 2 列 → 标签区从第 26 列起，到按钮前 1 列。
        assert_eq!(hits.tabs, Rect::new(26, 0, 40, 1));
        // 很窄时标题让位给标签条。
        let narrow = Rect::new(0, 0, 18, 1);
        let mut buffer = Buffer::empty(narrow);
        let hits = paint(&mut buffer, narrow, &terminal, &palette, &components);
        assert_eq!(hits.tabs, Rect::new(2, 0, 12, 1));
        assert!(!row_text(&buffer).contains('c'), "标题让位");
    }

    #[test]
    fn switcher_marks_the_current_panel_and_falls_back_when_too_wide() {
        let palette = Palette::graphite_dark();
        let area = Rect::new(0, 0, 30, 1);
        let mut buffer = Buffer::empty(area);
        let hits = paint_switcher(
            &mut buffer,
            area,
            &["工作区", "Agents", "监控"],
            1,
            palette.sidebar_bg,
            &palette,
        );
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[1].0, Rect::new(9, 0, 8, 1));
        assert_eq!(buffer[(10, 0)].bg, palette.surface1, "当前面板换底");
        assert_ne!(buffer[(1, 0)].bg, palette.surface1);
        let narrow = Rect::new(0, 0, 10, 1);
        let mut buffer = Buffer::empty(narrow);
        assert!(paint_switcher(
            &mut buffer,
            narrow,
            &["工作区", "Agents"],
            0,
            palette.sidebar_bg,
            &palette
        )
        .is_empty());
    }
}
