//! 子智能体页面（全屏）：页头、列表（层级 / 状态 / 时间线）、转写与概要的版式和
//! 绘制。状态、读取与输入路由在 `agent_activity_overlay.rs`，这里只放视觉：版式是
//! 纯函数（[`page_layout`] / [`list_lines`]），视图计算阶段与绘制共用同一份；绘制
//! （[`paint_agent_page`]）只读状态，命中区作为返回值交给调用方登记。
//!
//! 页面区域由 `floating_pages::overlay_bounds` 给出：工作台布局是标题栏与状态栏之间
//! 的整块内容区；经典与移动布局是整帧，最后一行由页面自己画页脚。页面不压暗、
//! 不可拖动，也不保存位置。

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

use super::super::agent_activity_overlay::{
    bucket_label, format_elapsed, kind_label, node_elapsed_ms, status_label, AgentActivityButton,
    AgentActivityContentView, AgentActivityFocus, AgentActivityOwner, AgentPageView,
    ClientAgentActivityOverlay, LineTone,
};
use super::super::feedback::{ChromeContext, ChromeHover};
use super::super::render::{display_width, put_text, wrap_text, OverlayRender};
use super::agent_transcript::Tone;
use crate::api::schema::{AgentActivityKind, AgentActivityNode, AgentActivityStatus};
use crate::harness::AgentBucket;
use crate::ui::design;
use crate::ui::kit::footer_hints::{render_footer_hints, FooterHint};
use crate::ui::kit::tree::{render_tree_prefix, tree_prefix_width, TreeEntry, MAX_TREE_DEPTH};

/// 小于这个尺寸只显示一条居中的最小提示。
pub(in crate::client::shell) const MIN_COLS: u16 = 40;
pub(in crate::client::shell) const MIN_ROWS: u16 = 8;
/// 内宽低于它单栏显示（Tab 在列表与转写之间切换）。
pub(in crate::client::shell) const SINGLE_BELOW: u16 = 100;
/// 内宽低于它列表每项只占一行（省略摘要行）。
pub(in crate::client::shell) const COMPACT_BELOW: u16 = 60;
/// 内宽低于它页头按钮只留图标。
pub(in crate::client::shell) const ICONS_BELOW: u16 = 80;
/// 高度低于它隐藏页头第 2 行。
pub(in crate::client::shell) const HEADER2_MIN_ROWS: u16 = 12;
/// 总宽低于它时间线不显示概要面板。
pub(in crate::client::shell) const SUMMARY_MIN_COLS: u16 = 120;
/// 列表宽 = 内宽的 38%，夹在 36–64 列。
const LIST_SHARE: u16 = 38;
const LIST_MIN: u16 = 36;
const LIST_MAX: u16 = 64;
/// 列表项左侧：1 列选中竖条 + 1 列留白，树前缀从这里开始。
const ITEM_PAD: u16 = 2;
/// 标签至少保留的列数（角色与耗时在这之外取舍）。
const LABEL_MIN: u16 = 10;
/// 时间线泳道右侧耗时列的宽度。
const LANE_ELAPSED: u16 = 8;

/// 页面版式：视图计算阶段与绘制共用（STATE-04）。空 `Rect` = 这一块不显示。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(in crate::client::shell) struct PageLayout {
    /// 整个页面（命中区 `agent_activity_popup`）。
    pub(in crate::client::shell) page: Rect,
    /// 小于 [`MIN_COLS`] × [`MIN_ROWS`]：只画最小提示。
    pub(in crate::client::shell) tiny: bool,
    /// 页面自己的页脚（经典 / 移动布局）。
    pub(in crate::client::shell) footer: Rect,
    /// 页头第 1 行：返回、标题、属主、位置与右侧按钮。
    pub(in crate::client::shell) header: Rect,
    /// 页头第 2 行：属主状态、时长、模型、会话标题与右侧计数。
    pub(in crate::client::shell) header2: Rect,
    /// 页头下的分隔线。
    pub(in crate::client::shell) rule: Rect,
    /// 页头以下、页脚以上的主体。
    pub(in crate::client::shell) body: Rect,
    pub(in crate::client::shell) single: bool,
    pub(in crate::client::shell) compact: bool,
    pub(in crate::client::shell) icons: bool,
    /// 视图标签行（层级 / 状态 / 时间线 + 筛选）。
    pub(in crate::client::shell) tabs: Rect,
    /// 列表显示行区域；时间线视图里是泳道区域（全宽）。
    pub(in crate::client::shell) list: Rect,
    /// 列表与转写之间的竖分隔线。
    pub(in crate::client::shell) divider: Rect,
    /// 时间线：刻度行（全宽）。
    pub(in crate::client::shell) axis: Rect,
    /// 时间线：泳道下的横分隔线。
    pub(in crate::client::shell) lane_rule: Rect,
    /// 时间线：泳道标签栏宽度。
    pub(in crate::client::shell) lane_label: u16,
    /// 时间线：时间轴的列区间（`y` 不用）。
    pub(in crate::client::shell) lane_axis: Rect,
    /// 转写栏（含头行与底部跟随行）。
    pub(in crate::client::shell) content: Rect,
    pub(in crate::client::shell) content_header: Rect,
    /// 头行之下、滚动条之左的正文区。
    pub(in crate::client::shell) content_text: Rect,
    /// 正文右侧常驻的滚动条列（折行宽度不随滚动条出没变化）。
    pub(in crate::client::shell) scrollbar: Rect,
    /// 转写底部一行：跟随状态。
    pub(in crate::client::shell) content_footer: Rect,
    /// 时间线：概要面板与它左侧的分隔线。
    pub(in crate::client::shell) summary_divider: Rect,
    pub(in crate::client::shell) summary: Rect,
}

/// 列表绘制结果：列表项命中区（节点 id）与按钮命中区。
type ListPaint = (Vec<(Rect, String)>, Vec<(Rect, AgentActivityButton)>);

/// 列表的一个显示行（视图计算阶段建好，渲染只读）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::client::shell) enum ListLine {
    /// 状态视图的组标题：`{字形} {标签} {数量}`。
    Group {
        bucket: AgentBucket,
        count: usize,
    },
    /// 一项的第 1 行（`detail = false`）或摘要行（`detail = true`）；`row` 是
    /// `overlay.rows` 的下标。
    Item {
        row: usize,
        detail: bool,
    },
    Blank,
    /// 列表末尾「已完成 N 项 · c 收起 / 展开」。
    DoneToggle,
}

/// 页面矩形：调用方给的页面边界（`overlay_bounds`）与帧取交集，没有时取整帧。
fn page_rect(area: Rect, page_bounds: Option<Rect>) -> Rect {
    page_bounds
        .map(|rect| rect.intersection(area))
        .filter(|rect| !rect.is_empty())
        .unwrap_or(area)
}

/// 页面版式（纯函数）：视图计算阶段（`compute_agent_activity_view`）与绘制都调它。
pub(in crate::client::shell) fn page_layout(
    area: Rect,
    page_bounds: Option<Rect>,
    overlay: &ClientAgentActivityOverlay,
) -> Option<PageLayout> {
    let page = page_rect(area, page_bounds);
    if page.is_empty() {
        return None;
    }
    let mut layout = PageLayout {
        page,
        ..PageLayout::default()
    };
    if page.width < MIN_COLS || page.height < MIN_ROWS {
        layout.tiny = true;
        return Some(layout);
    }
    let mut rest = page;
    if overlay.own_footer {
        layout.footer = Rect::new(page.x, page.bottom() - 1, page.width, 1);
        rest.height -= 1;
    }
    layout.icons = page.width < ICONS_BELOW;
    layout.single = page.width < SINGLE_BELOW;
    layout.compact = page.width < COMPACT_BELOW;
    layout.header = Rect::new(rest.x, rest.y, rest.width, 1);
    let mut y = rest.y + 1;
    if page.height >= HEADER2_MIN_ROWS {
        layout.header2 = Rect::new(rest.x, y, rest.width, 1);
        layout.rule = Rect::new(rest.x, y + 1, rest.width, 1);
        y += 2;
    }
    layout.body = Rect::new(rest.x, y, rest.width, rest.bottom().saturating_sub(y));
    if overlay.unsupported || layout.body.height < 2 {
        return Some(layout);
    }
    match overlay.view {
        AgentPageView::Timeline => timeline_layout(&mut layout, overlay),
        AgentPageView::Tree | AgentPageView::Status => columns_layout(&mut layout, overlay),
    }
    Some(layout)
}

/// 层级 / 状态视图：左列表 + 竖分隔线 + 右转写；窄屏只显示焦点所在的一栏。
fn columns_layout(layout: &mut PageLayout, overlay: &ClientAgentActivityOverlay) {
    let body = layout.body;
    let (list, content) = if layout.single {
        match overlay.focus {
            AgentActivityFocus::Tree => (body, Rect::default()),
            AgentActivityFocus::Content => (Rect::default(), body),
        }
    } else {
        let width = (body.width * LIST_SHARE / 100)
            .clamp(LIST_MIN, LIST_MAX)
            .min(body.width.saturating_sub(LIST_MIN));
        layout.divider = Rect::new(body.x + width, body.y, 1, body.height);
        (
            Rect::new(body.x, body.y, width, body.height),
            Rect::new(
                body.x + width + 1,
                body.y,
                body.width - width - 1,
                body.height,
            ),
        )
    };
    if !list.is_empty() {
        layout.tabs = Rect::new(list.x, list.y, list.width, 1);
        layout.list = Rect::new(list.x, list.y + 1, list.width, list.height - 1);
    }
    content_layout(layout, content, overlay);
}

/// 时间线视图：上部泳道（全宽，约占主体 40%），下部转写 + 概要（总宽 ≥ 120 列）。
/// 窄屏只显示焦点所在的一块。
fn timeline_layout(layout: &mut PageLayout, overlay: &ClientAgentActivityOverlay) {
    let body = layout.body;
    if layout.single && overlay.focus == AgentActivityFocus::Content {
        content_layout(layout, body, overlay);
        return;
    }
    let lanes_wanted = u16::try_from(overlay.rows.len().max(1))
        .unwrap_or(u16::MAX)
        .saturating_add(2);
    let block = if layout.single {
        body.height
    } else {
        lanes_wanted
            .min((body.height * 40 / 100).max(5))
            .min(body.height.saturating_sub(5))
            .max(3)
    };
    layout.tabs = Rect::new(body.x, body.y, body.width, 1);
    layout.axis = Rect::new(body.x, body.y + 1, body.width, u16::from(block > 1));
    layout.list = Rect::new(body.x, body.y + 2, body.width, block.saturating_sub(2));
    layout.lane_label = (body.width * 24 / 100).clamp(18, 36);
    let axis_x = body.x + layout.lane_label + 1;
    let axis_right = body.right().saturating_sub(LANE_ELAPSED + 1);
    layout.lane_axis = Rect::new(axis_x, 0, axis_right.saturating_sub(axis_x), 1);
    if layout.single {
        return;
    }
    let rule_y = body.y + block;
    layout.lane_rule = Rect::new(body.x, rule_y, body.width, 1);
    let bottom = Rect::new(
        body.x,
        rule_y + 1,
        body.width,
        body.bottom().saturating_sub(rule_y + 1),
    );
    let content = if layout.page.width >= SUMMARY_MIN_COLS {
        let width = (bottom.width * 28 / 100).clamp(30, 44);
        layout.summary_divider = Rect::new(bottom.right() - width - 1, bottom.y, 1, bottom.height);
        layout.summary = Rect::new(bottom.right() - width, bottom.y, width, bottom.height);
        Rect::new(bottom.x, bottom.y, bottom.width - width - 1, bottom.height)
    } else {
        bottom
    };
    content_layout(layout, content, overlay);
}

/// 转写栏：头行（节点标题 / 摘要 / 提示）、正文 + 滚动条、底部跟随行。
fn content_layout(layout: &mut PageLayout, column: Rect, overlay: &ClientAgentActivityOverlay) {
    if column.width < 8 || column.height < 4 {
        return;
    }
    layout.content = column;
    let header = content_header_rows(overlay).min(column.height - 3);
    layout.content_header = Rect::new(column.x, column.y, column.width, header);
    layout.content_footer = Rect::new(column.x, column.bottom() - 1, column.width, 1);
    let top = column.y + header;
    let height = column.height - header - 1;
    layout.content_text = Rect::new(column.x + 1, top, column.width.saturating_sub(3), height);
    layout.scrollbar = Rect::new(column.right() - 1, top, 1, height);
}

/// 转写头行数：节点标题行 + 摘要行（有摘要时）+ 提示行（截断或读取失败时）。
fn content_header_rows(overlay: &ClientAgentActivityOverlay) -> u16 {
    let node = overlay
        .selected_node
        .as_deref()
        .and_then(|id| overlay.node(id));
    let summary = node.is_some_and(|node| node.summary.is_some());
    let notice = overlay.content.as_ref().is_some_and(|content| {
        content.truncated || (overlay.error.is_some() && !content.lines.is_empty())
    });
    1 + u16::from(summary) + u16::from(notice)
}

/// 列表显示行（纯函数）：层级视图每项两行（窄屏一行）、末尾「已完成 N 项」；
/// 状态视图按状态桶分组、组前留空行；时间线每条泳道一行。
pub(in crate::client::shell) fn list_lines(
    overlay: &ClientAgentActivityOverlay,
    layout: &PageLayout,
) -> Vec<ListLine> {
    let mut lines = Vec::with_capacity(overlay.rows.len() * 2 + 4);
    let item = |lines: &mut Vec<ListLine>, row: usize| {
        lines.push(ListLine::Item { row, detail: false });
        if !layout.compact {
            lines.push(ListLine::Item { row, detail: true });
        }
    };
    match overlay.view {
        AgentPageView::Timeline => {
            lines.extend((0..overlay.rows.len()).map(|row| ListLine::Item { row, detail: false }));
            return lines;
        }
        AgentPageView::Tree => {
            for row in 0..overlay.rows.len() {
                item(&mut lines, row);
            }
        }
        AgentPageView::Status => {
            let bucket_of = |row: &TreeEntry<usize>| {
                overlay
                    .nodes
                    .get(row.kind)
                    .map_or(AgentBucket::None, |node| {
                        design::activity_bucket(node.status)
                    })
            };
            let mut previous = None;
            for (index, row) in overlay.rows.iter().enumerate() {
                let bucket = bucket_of(row);
                if previous != Some(bucket) {
                    if previous.is_some() {
                        lines.push(ListLine::Blank);
                    }
                    let count = overlay.rows[index..]
                        .iter()
                        .take_while(|next| bucket_of(next) == bucket)
                        .count();
                    lines.push(ListLine::Group { bucket, count });
                    previous = Some(bucket);
                }
                item(&mut lines, index);
            }
        }
    }
    if overlay.done_count() > 0 {
        if !lines.is_empty() {
            lines.push(ListLine::Blank);
        }
        lines.push(ListLine::DoneToggle);
    }
    lines
}

/// 第 `row` 项占的显示行区间 `[first, last]`（reveal 用）。
pub(in crate::client::shell) fn item_span(
    lines: &[ListLine],
    row: usize,
) -> Option<(usize, usize)> {
    let is_row = |line: &ListLine| matches!(line, ListLine::Item { row: candidate, .. } if *candidate == row);
    let first = lines.iter().position(is_row)?;
    let last = lines.iter().rposition(is_row).unwrap_or(first);
    Some((first, last))
}

/// 层级视图里一项的折叠开关命中区（开关格起 2 列，与 `render_tree_prefix` 同口径）。
pub(in crate::client::shell) fn fold_toggle_rect(item: Rect, depth: u8) -> Rect {
    let x = item.x + ITEM_PAD + u16::from(depth.min(MAX_TREE_DEPTH)) * 2;
    Rect::new(x, item.y, 2, 1)
}

/// 状态栏 / 页面页脚的固定键位提示（不是可配置键位）：`(键帽, 动作)`。外部来源没有
/// 终端可跳，不列 t。
pub(in crate::client::shell) fn page_hints(external: bool) -> Vec<(&'static str, &'static str)> {
    let texts = &crate::i18n::texts().agent_activity;
    let mut hints = vec![
        ("↑↓", texts.hint_select),
        ("Tab", texts.hint_switch_focus),
        ("↵", texts.hint_expand),
        ("/", texts.hint_filter),
        ("1-3", texts.hint_views),
        ("f", texts.hint_follow),
    ];
    if !external {
        hints.push(("t", texts.hint_jump));
    }
    hints.push(("esc", texts.hint_back));
    hints
}

/// 从 `x` 写到 `right` 为止（不含），返回写完后的列。宽字符放不下时停在它前面。
fn put(b: &mut Buffer, x: u16, y: u16, right: u16, text: &str, style: Style) -> u16 {
    let width = display_width(text).min(right.saturating_sub(x));
    put_text(b, x, y, width, text, style);
    x.saturating_add(width)
}

/// 右对齐写到 `right`（不含），返回起点列；起点越过 `left` 时不写、返回 `right`。
fn put_right(b: &mut Buffer, left: u16, y: u16, right: u16, text: &str, style: Style) -> u16 {
    let width = display_width(text);
    let Some(x) = right.checked_sub(width).filter(|x| *x >= left) else {
        return right;
    };
    put_text(b, x, y, width, text, style);
    x
}

fn fill(b: &mut Buffer, rect: Rect, symbol: &str, style: Style) {
    let rect = rect.intersection(b.area);
    for y in rect.y..rect.bottom() {
        for x in rect.x..rect.right() {
            b[(x, y)].reset();
            b[(x, y)].set_symbol(symbol).set_style(style);
        }
    }
}

/// 悬浮行底色：surface0，与面板底分不开时退到主题的悬浮行底色。
fn hover_bg(cx: &ChromeContext<'_>) -> Color {
    let p = cx.palette;
    if crate::app::state::Palette::row_bg_is_distinct(p.panel_bg, p.surface0) {
        p.surface0
    } else {
        p.hover_row_bg()
    }
}

fn bucket_style(bucket: AgentBucket, cx: &ChromeContext<'_>) -> Style {
    design::status_style(bucket, cx.palette, cx.components)
}

fn failure_color(cx: &ChromeContext<'_>) -> Color {
    design::status_color(AgentBucket::Failed, cx.palette, cx.components)
}

/// 转写色调 → 样式。
fn tone_style(tone: Tone, cx: &ChromeContext<'_>) -> Style {
    let p = cx.palette;
    match tone {
        Tone::Body => design::body(p),
        Tone::Heading => design::title(p),
        Tone::Muted | Tone::Output => design::muted(p),
        Tone::Meta | Tone::Toggle => design::meta(p),
        Tone::Rule => design::rule(p),
        Tone::ToolName => design::tool_name(p),
        Tone::Failure => Style::default().fg(failure_color(cx)),
        Tone::Success => {
            Style::default().fg(design::status_color(AgentBucket::Review, p, cx.components))
        }
    }
}

/// 绘制整个页面。只读状态，不写任何滚动状态（`test_overlay_renderers_never_write_scroll_state`）。
pub(in crate::client::shell) fn paint_agent_page(
    b: &mut Buffer,
    overlay: &ClientAgentActivityOverlay,
    cx: &ChromeContext<'_>,
) -> Option<OverlayRender> {
    let layout = page_layout(b.area, cx.page_bounds, overlay)?;
    let p = cx.palette;
    fill(
        b,
        layout.page,
        " ",
        Style::default().fg(p.text).bg(p.panel_bg),
    );
    let mut render = OverlayRender {
        area: layout.page,
        agent_activity_popup: layout.page,
        ..OverlayRender::default()
    };
    if layout.tiny {
        paint_too_small(b, layout.page, cx);
        return Some(render);
    }
    render.agent_activity_actions = paint_header(b, &layout, overlay, cx);
    if !layout.header2.is_empty() {
        paint_header2(b, layout.header2, overlay, cx);
        fill(b, layout.rule, "─", design::rule(p).bg(p.panel_bg));
    }
    if overlay.unsupported {
        crate::ui::kit::empty_state::render_empty_state(
            b,
            layout.body,
            &crate::ui::kit::empty_state::EmptyState {
                glyph: Some("⊘"),
                title: crate::i18n::texts().agent_activity.unsupported,
                body: None,
                action: None,
            },
            p,
        );
    } else {
        if !layout.tabs.is_empty() {
            let tabs = paint_tabs(b, layout.tabs, overlay, cx);
            render.agent_activity_actions.extend(tabs);
        }
        match overlay.view {
            AgentPageView::Timeline => {
                render.agent_activity_tree_rows = paint_lanes(b, &layout, overlay, cx);
                if !layout.lane_rule.is_empty() {
                    fill(b, layout.lane_rule, "─", design::rule(p).bg(p.panel_bg));
                }
                if !layout.summary.is_empty() {
                    fill(
                        b,
                        layout.summary_divider,
                        "│",
                        design::rule(p).bg(p.panel_bg),
                    );
                    paint_summary(b, layout.summary, overlay, cx);
                }
            }
            AgentPageView::Tree | AgentPageView::Status => {
                let (rows, actions) = paint_list(b, &layout, overlay, cx);
                render.agent_activity_tree_rows = rows;
                render.agent_activity_actions.extend(actions);
                if !layout.divider.is_empty() {
                    fill(b, layout.divider, "│", design::rule(p).bg(p.panel_bg));
                }
            }
        }
        if !layout.content.is_empty() {
            render.agent_activity_content = layout.content;
            let painted = paint_content(b, &layout, overlay, cx);
            render.agent_activity_scrollbar = painted.scrollbar;
            render.agent_activity_scroll_metrics = painted.metrics;
            render.agent_activity_toggles = painted.toggles;
        }
    }
    if !layout.footer.is_empty() {
        paint_footer(b, layout.footer, overlay, cx);
    }
    Some(render)
}

fn paint_too_small(b: &mut Buffer, page: Rect, cx: &ChromeContext<'_>) {
    let text = crate::ui::truncate_end(
        crate::i18n::texts().agent_activity.too_small,
        usize::from(page.width.saturating_sub(2)),
    );
    let width = display_width(&text);
    let x = page.x + page.width.saturating_sub(width) / 2;
    let y = page.y + page.height / 2;
    put_text(b, x, y, width, &text, design::meta(cx.palette));
}

/// 页头第 1 行：左侧 ‹ 返回、标题、属主与位置；右侧按钮从右往左是 ×、跳到终端
/// （仅 pane 属主）、刷新、跟随（外部来源换成只读徽标）。返回按钮命中区。属主名
/// 截断，位置最先丢；放不下的按钮整个不画。
fn paint_header(
    b: &mut Buffer,
    layout: &PageLayout,
    overlay: &ClientAgentActivityOverlay,
    cx: &ChromeContext<'_>,
) -> Vec<(Rect, AgentActivityButton)> {
    let p = cx.palette;
    let texts = &crate::i18n::texts().agent_activity;
    let row = layout.header;
    let icons = layout.icons;
    let label = |icon: &str, text: &str| {
        if icons {
            format!(" {icon} ")
        } else {
            format!(" {icon} {text} ")
        }
    };
    let mut buttons = vec![(AgentActivityButton::Close, " × ".to_owned())];
    if !overlay.is_external() {
        buttons.push((AgentActivityButton::Jump, label("↵", texts.jump_button)));
    }
    buttons.push((
        AgentActivityButton::Refresh,
        label("↻", texts.refresh_button),
    ));
    if !overlay.is_external() {
        let follow = if overlay.follow_active() {
            label("↓", texts.follow_on)
        } else {
            label("‖", texts.follow_off)
        };
        buttons.push((AgentActivityButton::ToggleFollow, follow));
    }
    let back_label = label("‹", texts.back);
    let left_min = row
        .x
        .saturating_add(1)
        .saturating_add(display_width(&back_label))
        .saturating_add(1)
        .saturating_add(display_width(texts.page_title))
        .saturating_add(2);
    let button_style = |button: AgentActivityButton, base| {
        crate::ui::modal_button_style(
            p,
            crate::ui::ModalButtonTone::Secondary,
            cx.button_state(&ChromeHover::AgentActivityButton(button), base),
        )
    };
    let mut hits = Vec::new();
    let mut right = row.right().saturating_sub(1);
    for (button, text) in &buttons {
        let width = display_width(text);
        let Some(x) = right.checked_sub(width).filter(|x| *x >= left_min) else {
            break;
        };
        let base = if *button == AgentActivityButton::ToggleFollow && overlay.follow_active() {
            crate::ui::ModalButtonState::Focused
        } else {
            crate::ui::ModalButtonState::Normal
        };
        let rect = Rect::new(x, row.y, width, 1);
        let style = button_style(*button, base);
        b.set_style(rect, style);
        put_text(b, x, row.y, width, text, style);
        hits.push((rect, *button));
        right = x.saturating_sub(1);
    }
    if overlay.is_external() {
        let badge = format!(" {} ", texts.external_read_only);
        let width = display_width(&badge);
        if let Some(x) = right.checked_sub(width).filter(|x| *x >= left_min) {
            let style = Style::default().fg(p.subtext0).bg(p.surface0);
            put_text(b, x, row.y, width, &badge, style);
            right = x.saturating_sub(1);
        }
    }
    // 左侧：返回按钮与标题常驻，其余按剩余宽度取舍。
    let mut x = row.x + 1;
    let back_width = display_width(&back_label);
    if x.saturating_add(back_width) <= right {
        let rect = Rect::new(x, row.y, back_width, 1);
        let style = button_style(
            AgentActivityButton::Back,
            crate::ui::ModalButtonState::Normal,
        );
        b.set_style(rect, style);
        put_text(b, x, row.y, back_width, &back_label, style);
        hits.push((rect, AgentActivityButton::Back));
        x = x.saturating_add(back_width + 1);
    }
    x = put(b, x, row.y, right, texts.page_title, design::title(p));
    let limit = right.saturating_sub(1);
    let room = limit.saturating_sub(x.saturating_add(2));
    let owner = &overlay.owner_view.name;
    let owner_width = display_width(owner);
    let owner_shown = owner_width.min(room);
    if owner_shown >= owner_width.min(4) && owner_shown > 0 {
        x = x.saturating_add(2);
        let text = crate::ui::truncate_end(owner, usize::from(owner_shown));
        x = put(
            b,
            x,
            row.y,
            limit,
            &text,
            design::body(p).add_modifier(Modifier::BOLD),
        );
    }
    if let Some(location) = overlay.owner_view.location.as_deref() {
        let start = x.saturating_add(2);
        let room = limit.saturating_sub(start);
        if room >= 6 {
            let text = crate::ui::truncate_end(location, usize::from(room));
            put(b, start, row.y, limit, &text, design::meta(p));
        }
    }
    hits
}

/// 页头第 2 行：属主状态字形 + 状态与运行时长 · 模型 · 会话标题；右侧按需要输入、
/// 失败、运行中、已完成、等待的顺序列出非零计数。未跟随时属主有更新，最右侧先画
/// 「有更新，按 r 刷新」（计数放不下时让位给它）。
fn paint_header2(
    b: &mut Buffer,
    row: Rect,
    overlay: &ClientAgentActivityOverlay,
    cx: &ChromeContext<'_>,
) {
    let p = cx.palette;
    let order = [
        AgentBucket::NeedsInput,
        AgentBucket::Failed,
        AgentBucket::Working,
        AgentBucket::Review,
        AgentBucket::Idle,
    ];
    let mut counts = [0usize; 5];
    for node in &overlay.nodes {
        let bucket = design::activity_bucket(node.status);
        if let Some(index) = order.iter().position(|candidate| *candidate == bucket) {
            counts[index] += 1;
        }
    }
    let left = row.x + 1;
    let mut chips = Vec::new();
    for (bucket, count) in order.iter().zip(counts) {
        if count > 0 {
            chips.push((*bucket, format!(" {count} "), bucket_label(*bucket)));
        }
    }
    let chips_width = chips
        .iter()
        .map(|(bucket, number, label)| {
            display_width(overlay.glyphs.bucket(*bucket))
                + display_width(number)
                + display_width(label)
                + 2
        })
        .fold(0u16, u16::saturating_add);
    let mut right = row.right().saturating_sub(1);
    if overlay.has_updates {
        let badge = format!(" {} ", crate::i18n::texts().agent_activity.updates_badge);
        let start = put_right(
            b,
            left,
            row.y,
            right,
            &badge,
            Style::default().fg(p.yellow).add_modifier(Modifier::BOLD),
        );
        right = start.saturating_sub(1);
    }
    // 计数放不下时整组不画（左侧属主信息优先）。
    if chips_width > 0 && left + 24 + chips_width <= right {
        let mut x = right.saturating_sub(chips_width - 2);
        right = x.saturating_sub(2);
        for (bucket, number, label) in &chips {
            x = put(
                b,
                x,
                row.y,
                row.right(),
                overlay.glyphs.bucket(*bucket),
                bucket_style(*bucket, cx),
            );
            x = put(b, x, row.y, row.right(), number, design::body(p));
            x = put(b, x, row.y, row.right(), label, design::meta(p));
            x = x.saturating_add(2);
        }
    }
    let owner = &overlay.owner_view;
    let mut x = left;
    if let Some(bucket) = owner.bucket {
        x = put(
            b,
            x,
            row.y,
            right,
            overlay.glyphs.bucket(bucket),
            bucket_style(bucket, cx),
        );
        x = put(b, x, row.y, right, " ", design::body(p));
        x = put(b, x, row.y, right, bucket_label(bucket), design::body(p));
    }
    if let Some(since) = owner.since_ms {
        let duration = design::duration_label(overlay.now_ms.saturating_sub(since));
        x = put(b, x, row.y, right, " ", design::body(p));
        x = put(b, x, row.y, right, &duration, design::body(p));
    }
    for text in [owner.model.as_deref(), owner.session_title.as_deref()]
        .into_iter()
        .flatten()
    {
        if x > left {
            x = put(b, x, row.y, right, " · ", design::meta(p));
        }
        let text = crate::ui::truncate_end(text, usize::from(right.saturating_sub(x)));
        x = put(b, x, row.y, right, &text, design::muted(p));
    }
}

/// 视图标签行：`层级  状态  时间线`（当前视图选中底色加粗）+ 右侧筛选入口。
fn paint_tabs(
    b: &mut Buffer,
    row: Rect,
    overlay: &ClientAgentActivityOverlay,
    cx: &ChromeContext<'_>,
) -> Vec<(Rect, AgentActivityButton)> {
    let p = cx.palette;
    let texts = &crate::i18n::texts().agent_activity;
    let mut hits = Vec::new();
    let right = row.right().saturating_sub(1);
    let mut x = row.x + 1;
    for view in AgentPageView::ALL {
        let text = format!(" {} ", view.label());
        let width = display_width(&text);
        if x.saturating_add(width) > right {
            break;
        }
        let button = AgentActivityButton::View(view);
        let style = if overlay.view == view {
            design::selected_row(p).fg(p.text)
        } else if cx.hovered(&ChromeHover::AgentActivityButton(button)) {
            design::body(p).bg(hover_bg(cx))
        } else {
            design::meta(p)
        };
        put_text(b, x, row.y, width, &text, style);
        hits.push((Rect::new(x, row.y, width, 1), button));
        x = x.saturating_add(width + 1);
    }
    // 筛选：输入中 `/ 查询▏`，有筛选词 `/ 查询`，否则 `/ 筛选`。
    let (query, style) = if overlay.filter_editing {
        (format!("/ {}▏", overlay.filter), design::body(p))
    } else if !overlay.filter.is_empty() {
        (
            format!("/ {}", overlay.filter),
            design::body(p).add_modifier(Modifier::BOLD),
        )
    } else {
        (format!("/ {}", texts.filter), design::meta(p))
    };
    let room = right.saturating_sub(x.saturating_add(1));
    if room >= 4 {
        // 放不下时从左截断，保留正在输入的尾部。
        let spans = [super::Span::new(query.as_str(), style)];
        let text = super::truncate_spans_left(&spans, room, style)
            .into_iter()
            .map(|span| span.text.into_owned())
            .collect::<String>();
        let start = put_right(b, x, row.y, right, &text, style);
        if start < right {
            hits.push((
                Rect::new(start, row.y, right - start, 1),
                AgentActivityButton::Filter,
            ));
        }
    }
    hits
}

/// 列表（层级 / 状态视图）：组标题、两行的列表项、「已完成 N 项」行。返回列表项
/// 命中区（一项一个矩形，覆盖它的全部可见显示行）与「已完成」行的按钮命中区。
fn paint_list(
    b: &mut Buffer,
    layout: &PageLayout,
    overlay: &ClientAgentActivityOverlay,
    cx: &ChromeContext<'_>,
) -> ListPaint {
    let p = cx.palette;
    let texts = &crate::i18n::texts().agent_activity;
    let area = layout.list;
    let mut rows: Vec<(Rect, String)> = Vec::new();
    let mut actions = Vec::new();
    if area.is_empty() {
        return (rows, actions);
    }
    if overlay.rows.is_empty() {
        let (text, style) = empty_list_text(overlay, cx);
        put(
            b,
            area.x + ITEM_PAD,
            area.y,
            area.right().saturating_sub(1),
            &text,
            style,
        );
        if overlay.done_count() == 0 || !overlay.tree_loaded {
            return (rows, actions);
        }
    } else if let Some(error) = overlay.tree_error.as_deref() {
        // 刷新失败但保留旧树：错误写在标签行右侧。
        let text = crate::i18n::fill(texts.read_failed_fmt, &[("error", error)]);
        let row = layout.tabs;
        let text = crate::ui::truncate_end(&text, usize::from(row.width / 2));
        put_right(
            b,
            row.x,
            row.y,
            row.right().saturating_sub(1),
            &text,
            Style::default().fg(failure_color(cx)),
        );
    }
    let visible = usize::from(area.height);
    let start = overlay
        .list_scroll
        .min(overlay.list_lines.len().saturating_sub(visible));
    let first_y = if overlay.rows.is_empty() {
        area.y + 1
    } else {
        area.y
    };
    for (offset, line) in overlay
        .list_lines
        .iter()
        .skip(start)
        .take(visible)
        .enumerate()
    {
        let y = first_y + u16::try_from(offset).unwrap_or(u16::MAX);
        if y >= area.bottom() {
            break;
        }
        let rect = Rect::new(area.x, y, area.width, 1);
        let right = rect.right().saturating_sub(1);
        match *line {
            ListLine::Blank => {}
            ListLine::Group { bucket, count } => {
                let mut x = rect.x + ITEM_PAD;
                x = put(
                    b,
                    x,
                    y,
                    right,
                    overlay.glyphs.bucket(bucket),
                    bucket_style(bucket, cx),
                );
                x = put(b, x, y, right, " ", design::section(p));
                x = put(b, x, y, right, bucket_label(bucket), design::section(p));
                put(b, x, y, right, &format!(" {count}"), design::meta(p));
            }
            ListLine::DoneToggle => {
                let text = format!(
                    "{} · {}",
                    crate::i18n::fill(
                        texts.done_count_fmt,
                        &[("n", &overlay.done_count().to_string())]
                    ),
                    if overlay.hide_done {
                        texts.show_done
                    } else {
                        texts.hide_done
                    }
                );
                let button = AgentActivityButton::ToggleDone;
                let style = if cx.hovered(&ChromeHover::AgentActivityButton(button)) {
                    design::body(p).bg(hover_bg(cx))
                } else {
                    design::meta(p)
                };
                let x = rect.x + ITEM_PAD;
                let end = put(b, x, y, right, &text, style);
                actions.push((Rect::new(x, y, end.saturating_sub(x), 1), button));
            }
            ListLine::Item { row, detail } => {
                let Some(entry) = overlay.rows.get(row) else {
                    continue;
                };
                let Some(node) = overlay.nodes.get(entry.kind) else {
                    continue;
                };
                paint_item_line(b, rect, entry, node, detail, overlay, cx);
                match rows.last_mut() {
                    Some((last, id)) if *id == entry.key && last.bottom() == y => {
                        last.height += 1;
                    }
                    _ => rows.push((rect, entry.key.clone())),
                }
            }
        }
    }
    (rows, actions)
}

/// 列表没有可见项时的一行说明：读取失败、加载中、筛选无结果或空树。
fn empty_list_text(
    overlay: &ClientAgentActivityOverlay,
    cx: &ChromeContext<'_>,
) -> (String, Style) {
    let texts = &crate::i18n::texts().agent_activity;
    match (&overlay.tree_error, overlay.tree_loaded) {
        (Some(error), _) => (
            crate::i18n::fill(texts.read_failed_fmt, &[("error", error)]),
            Style::default().fg(failure_color(cx)),
        ),
        (None, false) => (texts.loading.to_owned(), design::meta(cx.palette)),
        (None, true) if !overlay.filter.is_empty() => {
            (texts.no_matches.to_owned(), design::meta(cx.palette))
        }
        (None, true) => (texts.empty_tree.to_owned(), design::meta(cx.palette)),
    }
}

/// 一项的一个显示行。第 1 行：选中竖条、树前缀（层级视图）、状态字形、标签（昵称
/// 加粗）、右侧角色与耗时；摘要行：树的延续线 + 摘要（说明色，失败节点用失败色；
/// 没有摘要时写状态）。选中两行都用选中底色，悬浮用 surface0 底色。
fn paint_item_line(
    b: &mut Buffer,
    rect: Rect,
    entry: &TreeEntry<usize>,
    node: &AgentActivityNode,
    detail: bool,
    overlay: &ClientAgentActivityOverlay,
    cx: &ChromeContext<'_>,
) {
    let p = cx.palette;
    let selected = overlay.selected_node.as_deref() == Some(node.id.as_str());
    let hovered = overlay.hovered_node.as_deref() == Some(node.id.as_str());
    if selected {
        b.set_style(rect, Style::default().bg(p.selection_row_bg()));
        put_text(b, rect.x, rect.y, 1, "▌", Style::default().fg(p.accent));
    } else if hovered {
        b.set_style(rect, Style::default().bg(hover_bg(cx)));
    }
    let strong = move |style: Style| {
        if selected {
            style.add_modifier(Modifier::BOLD)
        } else {
            style
        }
    };
    let tree = overlay.view != AgentPageView::Status;
    let right = rect.right().saturating_sub(1);
    let mut x = rect.x + ITEM_PAD;
    let bucket = design::activity_bucket(node.status);
    if detail {
        if tree {
            x = paint_continuation(b, x, rect.y, right, entry, design::rule(p));
        }
        x = x.saturating_add(2);
        let (text, style) = match node.summary.as_deref().filter(|text| !text.is_empty()) {
            Some(summary) if node.status == AgentActivityStatus::Failed => {
                (summary, Style::default().fg(failure_color(cx)))
            }
            Some(summary) => (summary, design::muted(p)),
            None => (status_label(node.status), design::meta(p)),
        };
        let text = crate::ui::truncate_end(text, usize::from(right.saturating_sub(x)));
        put(b, x, rect.y, right, &text, style);
        return;
    }
    if tree {
        let (used, _) = render_tree_prefix(
            b,
            x,
            rect.y,
            right.saturating_sub(x),
            entry.depth,
            entry.last_child_mask,
            entry.has_children,
            entry.collapsed,
            false,
            design::rule(p),
        );
        x = x.saturating_add(used);
    }
    x = put(
        b,
        x,
        rect.y,
        right,
        overlay.glyphs.bucket(bucket),
        bucket_style(bucket, cx),
    );
    x = put(b, x, rect.y, right, " ", design::body(p));
    let role = node
        .agent_type
        .as_deref()
        .filter(|role| !role.is_empty())
        .or_else(|| (node.kind != AgentActivityKind::Subagent).then(|| kind_label(node.kind)));
    let elapsed = node_elapsed_ms(node, overlay.now_ms).map(format_elapsed);
    paint_label_with_meta(
        b,
        (x, rect.y, right),
        &node.label,
        [role, elapsed.as_deref()],
        strong,
        cx,
    );
}

/// 摘要行的树延续线：祖先各层有后续兄弟画 `│ `；展开的父节点在开关格垂下 `│`。
/// 宽度与第 1 行的前缀相同。
fn paint_continuation(
    b: &mut Buffer,
    x: u16,
    y: u16,
    right: u16,
    entry: &TreeEntry<usize>,
    style: Style,
) -> u16 {
    let depth = entry.depth.min(MAX_TREE_DEPTH);
    let mut cursor = x;
    for level in 1..=depth {
        let last = entry.last_child_mask & (1u64 << level) != 0;
        cursor = put(b, cursor, y, right, if last { "  " } else { "│ " }, style);
    }
    let hangs = entry.has_children && !entry.collapsed;
    put(b, cursor, y, right, if hangs { "│ " } else { "  " }, style);
    x.saturating_add(tree_prefix_width(depth)).min(right)
}

/// 标签（` · ` 前的昵称加粗、其后正文色）+ 右对齐的元信息（角色、耗时）。标签至少
/// 留 [`LABEL_MIN`] 列；放不下时先丢角色、再丢耗时，从不画半截元信息。
fn paint_label_with_meta(
    b: &mut Buffer,
    (x, y, right): (u16, u16, u16),
    label: &str,
    meta: [Option<&str>; 2],
    strong: impl Fn(Style) -> Style,
    cx: &ChromeContext<'_>,
) {
    let p = cx.palette;
    let room = right.saturating_sub(x);
    let mut keep = meta.map(|text| text.is_some_and(|text| !text.is_empty()));
    let meta_width = |keep: &[bool; 2]| {
        meta.iter()
            .zip(keep)
            .filter(|(_, kept)| **kept)
            .map(|(text, _)| display_width(text.unwrap_or_default()) + 2)
            .fold(0u16, u16::saturating_add)
    };
    let label_min = display_width(label).min(LABEL_MIN);
    for drop in [0, 1] {
        if label_min.saturating_add(meta_width(&keep)) <= room {
            break;
        }
        keep[drop] = false;
    }
    let label_room = room.saturating_sub(meta_width(&keep));
    let text = crate::ui::truncate_end(label, usize::from(label_room));
    let (nickname, rest) = match text.split_once(" · ") {
        Some((nickname, rest)) => (nickname, Some(rest)),
        None => (text.as_str(), None),
    };
    let mut cursor = put(b, x, y, right, nickname, strong(design::title(p)));
    if let Some(rest) = rest {
        cursor = put(b, cursor, y, right, " · ", design::meta(p));
        put(b, cursor, y, right, rest, strong(design::body(p)));
    }
    let mut end = right;
    for (text, kept) in meta.iter().zip(keep).rev() {
        let Some(text) = text.filter(|_| kept) else {
            continue;
        };
        end = put_right(b, x, y, end, text, design::meta(p)).saturating_sub(2);
    }
}

/// 时间线泳道：标签栏（树前缀 + 字形 + 标签）、共用时间轴上的条形与末端字形、
/// 右侧耗时。刻度行左端写最早开始时刻的相对时间，右端写「现在」。返回泳道命中区。
fn paint_lanes(
    b: &mut Buffer,
    layout: &PageLayout,
    overlay: &ClientAgentActivityOverlay,
    cx: &ChromeContext<'_>,
) -> Vec<(Rect, String)> {
    let p = cx.palette;
    let texts = &crate::i18n::texts().agent_activity;
    let mut hits = Vec::new();
    let area = layout.list;
    let axis = layout.lane_axis;
    // 时间范围：可见泳道里最早的开始时刻到现在（或更晚的结束时刻）。
    let origin = overlay
        .rows
        .iter()
        .filter_map(|row| overlay.nodes.get(row.kind)?.started_at_ms)
        .min();
    let end = overlay
        .rows
        .iter()
        .filter_map(|row| overlay.nodes.get(row.kind)?.ended_at_ms)
        .chain(std::iter::once(overlay.now_ms))
        .max()
        .unwrap_or(overlay.now_ms);
    if !layout.axis.is_empty() {
        let row = layout.axis;
        put(
            b,
            row.x + ITEM_PAD,
            row.y,
            row.x + layout.lane_label,
            texts.page_title,
            design::section(p),
        );
        if let Some(origin) = origin.filter(|_| axis.width >= 12) {
            let left = format!(
                "-{}",
                design::compact_age(overlay.now_ms.saturating_sub(origin))
            );
            put(b, axis.x, row.y, axis.right(), &left, design::meta(p));
            put_right(
                b,
                axis.x,
                row.y,
                axis.right(),
                texts.timeline_now,
                design::meta(p),
            );
        }
    }
    if area.is_empty() {
        return hits;
    }
    if overlay.rows.is_empty() {
        let (text, style) = empty_list_text(overlay, cx);
        put(b, area.x + ITEM_PAD, area.y, area.right(), &text, style);
        return hits;
    }
    let span = end.saturating_sub(origin.unwrap_or(end)).max(1_000);
    let cells = u64::from(axis.width.saturating_sub(1));
    let column = |at: u64| -> u16 {
        let offset = at.saturating_sub(origin.unwrap_or(at));
        let cell = u16::try_from(offset.saturating_mul(cells) / span).unwrap_or(u16::MAX);
        axis.x + cell.min(axis.width.saturating_sub(1))
    };
    let visible = usize::from(area.height);
    let start = overlay
        .list_scroll
        .min(overlay.rows.len().saturating_sub(visible));
    let label_right = area.x + layout.lane_label;
    for (offset, entry) in overlay.rows.iter().skip(start).take(visible).enumerate() {
        let Some(node) = overlay.nodes.get(entry.kind) else {
            continue;
        };
        let y = area.y + u16::try_from(offset).unwrap_or(u16::MAX);
        let rect = Rect::new(area.x, y, area.width, 1);
        let selected = overlay.selected_node.as_deref() == Some(node.id.as_str());
        let hovered = overlay.hovered_node.as_deref() == Some(node.id.as_str());
        let row_bg = if selected {
            Some(p.selection_row_bg())
        } else if hovered {
            Some(hover_bg(cx))
        } else {
            None
        };
        if axis.width > 0 {
            fill(b, Rect::new(axis.x, y, axis.width, 1), "┈", design::rule(p));
        }
        if let Some(bg) = row_bg {
            b.set_style(rect, Style::default().bg(bg));
        }
        if selected {
            put_text(b, rect.x, y, 1, "▌", Style::default().fg(p.accent));
        }
        let strong = move |style: Style| {
            if selected {
                style.add_modifier(Modifier::BOLD)
            } else {
                style
            }
        };
        let mut x = area.x + ITEM_PAD;
        let (used, _) = render_tree_prefix(
            b,
            x,
            y,
            label_right.saturating_sub(x),
            entry.depth,
            entry.last_child_mask,
            entry.has_children,
            entry.collapsed,
            false,
            design::rule(p),
        );
        x = x.saturating_add(used);
        let bucket = design::activity_bucket(node.status);
        let glyph = overlay.glyphs.bucket(bucket);
        x = put(b, x, y, label_right, glyph, bucket_style(bucket, cx));
        x = put(b, x, y, label_right, " ", design::body(p));
        paint_label_with_meta(
            b,
            (x, y, label_right),
            &node.label,
            [None, None],
            strong,
            cx,
        );
        if axis.width > 0 {
            match node.started_at_ms.filter(|_| origin.is_some()) {
                Some(started) => {
                    let finish = node.ended_at_ms.unwrap_or(overlay.now_ms).max(started);
                    let (from, to) = (column(started), column(finish));
                    let style = Style::default().fg(lane_color(node.status, cx));
                    for cell in from..to {
                        put_text(b, cell, y, 1, "━", style);
                    }
                    put_text(b, to, y, 1, glyph, bucket_style(bucket, cx));
                }
                None => {
                    put(
                        b,
                        axis.x,
                        y,
                        axis.right(),
                        texts.timeline_unknown,
                        design::meta(p),
                    );
                }
            }
        }
        if let Some(elapsed) = node_elapsed_ms(node, overlay.now_ms).map(format_elapsed) {
            put_right(
                b,
                axis.right().saturating_add(1),
                y,
                rect.right().saturating_sub(1),
                &elapsed,
                strong(design::meta(p)),
            );
        }
        hits.push((rect, node.id.clone()));
    }
    hits
}

/// 泳道条形色：运行中 / 需要输入 / 失败用状态色，已完成用中性灰，等待用空闲色。
fn lane_color(status: AgentActivityStatus, cx: &ChromeContext<'_>) -> Color {
    match status {
        AgentActivityStatus::Done => cx.palette.overlay1,
        AgentActivityStatus::Unknown => cx.palette.overlay0,
        status => design::status_color(design::activity_bucket(status), cx.palette, cx.components),
    }
}

/// 时间线的「概要」面板：状态、角色、摘要、开始于、子级数、工具调用数、最近输出。
fn paint_summary(
    b: &mut Buffer,
    area: Rect,
    overlay: &ClientAgentActivityOverlay,
    cx: &ChromeContext<'_>,
) {
    let p = cx.palette;
    let texts = &crate::i18n::texts().agent_activity;
    let x = area.x + 2;
    let right = area.right().saturating_sub(1);
    let bottom = area.bottom();
    let mut y = area.y;
    if y >= bottom {
        return;
    }
    put(b, x, y, right, texts.summary_title, design::section(p));
    y += 1;
    let Some(node) = overlay
        .selected_node
        .as_deref()
        .and_then(|id| overlay.node(id))
    else {
        if y < bottom {
            put(b, x, y, right, texts.empty_content, design::meta(p));
        }
        return;
    };
    let labels = [
        texts.summary_status,
        texts.summary_role,
        texts.summary_detail,
        texts.summary_started,
        texts.summary_children,
        texts.summary_tools,
    ];
    let label_width = labels
        .iter()
        .map(|label| display_width(label))
        .max()
        .unwrap_or(0)
        + 2;
    let value_x = x + label_width;
    let bucket = design::activity_bucket(node.status);
    let mut status = status_label(node.status).to_owned();
    if let Some(elapsed) = node_elapsed_ms(node, overlay.now_ms) {
        status.push_str(" · ");
        status.push_str(&format_elapsed(elapsed));
    }
    let children = overlay
        .nodes
        .iter()
        .filter(|candidate| candidate.parent_id.as_deref() == Some(node.id.as_str()))
        .count();
    let transcript = overlay
        .content
        .as_ref()
        .filter(|content| content.is_transcript() && content.node_id == node.id);
    let tools = transcript.map(|content| {
        let count = overlay.transcript.stats().tool_calls;
        if content.truncated {
            format!("≥{count}")
        } else {
            count.to_string()
        }
    });
    let role = node
        .agent_type
        .clone()
        .unwrap_or_else(|| kind_label(node.kind).to_owned());
    let started = node.started_at_ms.map(|started| {
        crate::i18n::fill(
            texts.ago_fmt,
            &[(
                "age",
                &design::compact_age(overlay.now_ms.saturating_sub(started)),
            )],
        )
    });
    let values = [
        Some(status),
        Some(role),
        node.summary.clone(),
        started,
        Some(children.to_string()),
        tools,
    ];
    for (label, value) in labels.iter().zip(values) {
        let Some(value) = value.filter(|value| !value.is_empty()) else {
            continue;
        };
        if y >= bottom {
            return;
        }
        put(b, x, y, value_x, label, design::meta(p));
        let mut cursor = value_x;
        if *label == texts.summary_status {
            cursor = put(
                b,
                cursor,
                y,
                right,
                overlay.glyphs.bucket(bucket),
                bucket_style(bucket, cx),
            );
            cursor = put(b, cursor, y, right, " ", design::body(p));
        }
        let value = crate::ui::truncate_end(&value, usize::from(right.saturating_sub(cursor)));
        put(b, cursor, y, right, &value, design::body(p));
        y += 1;
    }
    let Some(last) = transcript.and_then(|_| overlay.transcript.last_output()) else {
        return;
    };
    y += 1;
    if y >= bottom {
        return;
    }
    put(
        b,
        x,
        y,
        right,
        texts.summary_last_output,
        design::section(p),
    );
    y += 1;
    let width = usize::from(right.saturating_sub(x).max(1));
    let mut lines = Vec::new();
    wrap_text(last, width, width, |start, end| lines.push((start, end)));
    for (start, end) in lines {
        if y >= bottom {
            break;
        }
        put(b, x, y, right, &last[start..end], design::muted(p));
        y += 1;
    }
}

/// 转写栏绘制结果。
struct ContentPaint {
    scrollbar: Rect,
    metrics: Option<crate::pane::ScrollMetrics>,
    toggles: Vec<(Rect, u64)>,
}

/// 转写栏：头行（字形 + 标签，右侧角色 · 耗时；摘要；截断 / 失败提示）、正文、
/// 滚动条与底部跟随行。
fn paint_content(
    b: &mut Buffer,
    layout: &PageLayout,
    overlay: &ClientAgentActivityOverlay,
    cx: &ChromeContext<'_>,
) -> ContentPaint {
    let p = cx.palette;
    let texts = &crate::i18n::texts().agent_activity;
    let mut painted = ContentPaint {
        scrollbar: Rect::default(),
        metrics: None,
        toggles: Vec::new(),
    };
    paint_content_header(b, layout.content_header, overlay, cx);
    paint_follow_line(b, layout.content_footer, overlay, cx);
    let text = layout.content_text;
    if text.is_empty() {
        return painted;
    }
    let placeholder = |b: &mut Buffer, line: &str, style: Style| {
        put_text(b, text.x, text.y, text.width, line, style);
    };
    let Some(selected) = overlay.selected_node.as_deref() else {
        crate::ui::kit::empty_state::render_empty_state(
            b,
            text,
            &crate::ui::kit::empty_state::EmptyState {
                glyph: None,
                title: texts.empty_content,
                body: None,
                action: None,
            },
            p,
        );
        return painted;
    };
    let Some(content) = overlay
        .content
        .as_ref()
        .filter(|content| content.node_id == selected)
    else {
        match overlay.error.as_deref() {
            Some(error) => placeholder(
                b,
                &crate::i18n::fill(texts.read_failed_fmt, &[("error", error)]),
                Style::default().fg(failure_color(cx)),
            ),
            None => placeholder(b, texts.loading, design::meta(p)),
        }
        return painted;
    };
    if content.lines.is_empty() {
        let line = if content.eof {
            texts.no_output
        } else {
            texts.loading
        };
        placeholder(b, line, design::meta(p));
        return painted;
    }
    let visible = usize::from(text.height);
    let total = if content.is_transcript() {
        overlay.transcript.rows().len()
    } else {
        content.rows.len()
    };
    let max_scroll = total.saturating_sub(visible);
    let scroll = overlay.content_scroll.min(max_scroll);
    if content.is_transcript() {
        for (offset, row) in overlay
            .transcript
            .rows()
            .iter()
            .skip(scroll)
            .take(visible)
            .enumerate()
        {
            let y = text.y + u16::try_from(offset).unwrap_or(u16::MAX);
            let mut cursor = text.x;
            for segment in &row.segments {
                let style = tone_style(segment.tone, cx);
                cursor = put(b, cursor, y, text.right(), &segment.text, style);
            }
            if let Some(tool) = row.toggle {
                painted
                    .toggles
                    .push((Rect::new(text.x, y, text.width, 1), tool));
            }
        }
    } else {
        paint_wrapped_lines(b, text, content, scroll, cx);
    }
    let metrics = crate::pane::ScrollMetrics {
        offset_from_bottom: max_scroll.saturating_sub(scroll),
        max_offset_from_bottom: max_scroll,
        viewport_rows: visible,
    };
    painted.metrics = Some(metrics);
    let track = layout.scrollbar;
    if max_scroll > 0 && !track.is_empty() {
        crate::ui::render_scrollbar_buffer(b, metrics, track, p.surface1, p.overlay1, "▐");
        painted.scrollbar = track;
    }
    painted
}

/// 转写头行：字形 + 标签（右侧角色 · 耗时）、摘要（失败节点用失败色）、截断或读取
/// 失败提示。
fn paint_content_header(
    b: &mut Buffer,
    header: Rect,
    overlay: &ClientAgentActivityOverlay,
    cx: &ChromeContext<'_>,
) {
    let p = cx.palette;
    let texts = &crate::i18n::texts().agent_activity;
    if header.is_empty() {
        return;
    }
    let node = overlay
        .selected_node
        .as_deref()
        .and_then(|id| overlay.node(id));
    let x = header.x + 1;
    let right = header.right().saturating_sub(1);
    let mut y = header.y;
    if let Some(node) = node {
        let bucket = design::activity_bucket(node.status);
        let mut cursor = put(
            b,
            x,
            y,
            right,
            overlay.glyphs.bucket(bucket),
            bucket_style(bucket, cx),
        );
        cursor = put(b, cursor, y, right, " ", design::body(p));
        let role = node
            .agent_type
            .as_deref()
            .filter(|role| !role.is_empty())
            .unwrap_or_else(|| kind_label(node.kind));
        let elapsed = node_elapsed_ms(node, overlay.now_ms).map(format_elapsed);
        paint_label_with_meta(
            b,
            (cursor, y, right),
            &node.label,
            [Some(role), elapsed.as_deref()],
            |style| style,
            cx,
        );
    }
    y += 1;
    if let Some(node) = node.filter(|node| node.summary.is_some()) {
        if y < header.bottom() {
            let style = if node.status == AgentActivityStatus::Failed {
                Style::default().fg(failure_color(cx))
            } else {
                design::muted(p)
            };
            let summary = node.summary.as_deref().unwrap_or_default();
            let text = crate::ui::truncate_end(summary, usize::from(right.saturating_sub(x)));
            put(b, x, y, right, &text, style);
            y += 1;
        }
    }
    if y >= header.bottom() {
        return;
    }
    let notice = match (&overlay.error, overlay.content.as_ref()) {
        (Some(error), Some(content)) if !content.lines.is_empty() => Some((
            crate::i18n::fill(texts.read_failed_fmt, &[("error", error)]),
            Style::default().fg(failure_color(cx)),
        )),
        (_, Some(content)) if content.truncated => Some((
            format!("↑ {}", texts.truncated),
            Style::default().fg(p.yellow),
        )),
        _ => None,
    };
    if let Some((notice, style)) = notice {
        let notice = crate::ui::truncate_end(&notice, usize::from(right.saturating_sub(x)));
        put(b, x, y, right, &notice, style);
    }
}

/// Markdown / Jsonl：按行着色（标题强调色加粗，列表记号与 jsonl 前缀着色）。
fn paint_wrapped_lines(
    b: &mut Buffer,
    text: Rect,
    content: &AgentActivityContentView,
    scroll: usize,
    cx: &ChromeContext<'_>,
) {
    let p = cx.palette;
    for (offset, row) in content
        .rows
        .iter()
        .skip(scroll)
        .take(usize::from(text.height))
        .enumerate()
    {
        let y = text.y + u16::try_from(offset).unwrap_or(u16::MAX);
        let Some(line) = content.lines.get(row.line as usize) else {
            continue;
        };
        let (start, end) = (row.start as usize, row.end as usize);
        let Some(slice) = line.text.get(start..end) else {
            continue;
        };
        let base = match line.tone {
            LineTone::Heading => Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
            _ => design::body(p),
        };
        put_text(b, text.x, y, text.width, slice, base);
        if matches!(line.tone, LineTone::ListItem | LineTone::Record) && start < line.marker {
            let marker_end = line.marker.min(end);
            if let Some(marker) = line.text.get(start..marker_end) {
                let style = match line.tone {
                    LineTone::Record => Style::default().fg(p.blue).add_modifier(Modifier::BOLD),
                    _ => Style::default().fg(p.accent),
                };
                put_text(b, text.x, y, text.width, marker, style);
            }
        }
    }
}

/// 转写底部一行：`↓ 跟随中 · 新输出自动滚到底` / `‖ 已暂停 · f 继续跟随`；外部来源写
/// 「外部来源，只读」。右对齐、元信息色。未跟随时属主有更新，前面再加「有更新，按 r
/// 刷新」（页头放不下徽标时这里仍看得到）。
fn paint_follow_line(
    b: &mut Buffer,
    row: Rect,
    overlay: &ClientAgentActivityOverlay,
    cx: &ChromeContext<'_>,
) {
    if row.is_empty() {
        return;
    }
    let texts = &crate::i18n::texts().agent_activity;
    let text = match &overlay.owner {
        AgentActivityOwner::External { .. } => texts.external_read_only.to_owned(),
        AgentActivityOwner::Pane { .. } if overlay.follow_active() => {
            format!("↓ {} · {}", texts.follow_on, texts.follow_tail)
        }
        AgentActivityOwner::Pane { .. } => {
            format!("‖ {} · {}", texts.follow_off, texts.follow_resume)
        }
    };
    let right = row.right().saturating_sub(1);
    let text = crate::ui::truncate_end(&text, usize::from(right.saturating_sub(row.x + 1)));
    let start = put_right(b, row.x + 1, row.y, right, &text, design::meta(cx.palette));
    if overlay.has_updates {
        let badge = format!("{} · ", texts.updates_badge);
        put_right(
            b,
            row.x + 1,
            row.y,
            start,
            &badge,
            Style::default()
                .fg(cx.palette.yellow)
                .add_modifier(Modifier::BOLD),
        );
    }
}

/// 经典 / 移动布局的页面页脚：与工作台状态栏同一组固定键位提示。
fn paint_footer(
    b: &mut Buffer,
    footer: Rect,
    overlay: &ClientAgentActivityOverlay,
    cx: &ChromeContext<'_>,
) {
    fill(
        b,
        footer,
        " ",
        Style::default().bg(super::bar_surface(cx.palette)),
    );
    let hints = page_hints(overlay.is_external());
    let hints = hints
        .iter()
        .map(|(key, label)| FooterHint {
            key,
            label,
            enabled: !overlay.unsupported || *key == "esc",
            primary: *key == "esc",
        })
        .collect::<Vec<_>>();
    let area = Rect::new(
        footer.x + 1,
        footer.y,
        footer.width.saturating_sub(2),
        footer.height,
    );
    render_footer_hints(b, area, &hints, None, cx.palette);
}
