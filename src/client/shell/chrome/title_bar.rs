//! 标题栏（第 1 行，整宽，侧栏面底色）：左侧「≡ herdr」主菜单入口与聚焦终端的
//! 面包屑（`[机器 ›] 工作区 › 标签 › ⠋ agent`，按 [`BreadcrumbModel`] 去重），右侧
//! 「调整布局」标记、注意力计数、命令搜索提示与 `⋯` 布局菜单。
//!
//! 窄屏按固定顺序降级（[`layout`]）：先丢命令提示；面包屑先从左截断、再隐藏；计数
//! 收成优先级最高的一个；再丢「调整布局」标记与计数；最后才把入口收成「≡」——入口
//! 与 `⋯` 始终在。

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use super::{paint_spans, spans_width, truncate_spans_left, Span};
use crate::app::state::{ComponentStyles, Palette};
use crate::harness::{AgentBucket, BucketCounts};
use crate::ui::display_width_u16;

/// 主菜单入口：完整与极窄两档。
pub(in crate::client::shell) const LAUNCHER: &str = " ≡ herdr ";
const LAUNCHER_COMPACT: &str = " ≡ ";
const MENU: &str = " ⋯ ";
const SEARCH_GLYPH: &str = " ⌕ ";
/// 面包屑截断后至少留这么多列（含「…」），否则整段隐藏。
const BREADCRUMB_MIN: u16 = 8;
/// 入口与面包屑、面包屑与右侧簇之间的空隙；右侧簇各项自带内边距，项间再空 1 列。
const LEFT_GAP: u16 = 2;
const RIGHT_GAP: u16 = 2;
const ITEM_GAP: u16 = 1;

/// 注意力计数的展示顺序（汇总优先级）：需要输入 > 失败 > 运行中 > 待查看；空闲不计。
pub(in crate::client::shell) const CHIP_ORDER: [AgentBucket; 4] = [
    AgentBucket::NeedsInput,
    AgentBucket::Failed,
    AgentBucket::Working,
    AgentBucket::Review,
];

/// 右侧簇里的一项（从左到右）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::client::shell) enum TitleItem {
    Arrange,
    /// 第几个计数（按 [`CHIP_ORDER`] 排列、零计数已滤掉后的下标）。
    Chip(usize),
    Command,
    Menu,
}

/// 版式的输入：各项的自然宽度（`None` / 0 = 这一项不存在）。
#[derive(Debug, Clone, Default)]
pub(in crate::client::shell) struct TitleWidths {
    pub(in crate::client::shell) breadcrumb: u16,
    pub(in crate::client::shell) arrange: Option<u16>,
    pub(in crate::client::shell) chips: Vec<u16>,
    pub(in crate::client::shell) command: Option<u16>,
}

/// 版式结果：各项的 `(x, 宽)`，相对标题栏左端。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::client::shell) struct TitleLayout {
    pub(in crate::client::shell) launcher: (u16, u16),
    pub(in crate::client::shell) compact: bool,
    /// 面包屑占的区间；宽度小于自然宽度时从左截断。
    pub(in crate::client::shell) breadcrumb: Option<(u16, u16)>,
    pub(in crate::client::shell) items: Vec<(TitleItem, u16, u16)>,
}

#[derive(Clone, Copy)]
struct Stage {
    command: bool,
    chips: usize,
    arrange: bool,
    compact: bool,
    /// 这一级要求面包屑完整放下（放不下就进入下一级，先丢命令提示）。
    full_breadcrumb: bool,
}

/// 标题栏版式（纯函数）：按降级顺序找第一级放得下的排法。
pub(in crate::client::shell) fn layout(width: u16, widths: &TitleWidths) -> TitleLayout {
    let all = widths.chips.len();
    let stage = |command, chips, arrange, compact, full_breadcrumb| Stage {
        command,
        chips,
        arrange,
        compact,
        full_breadcrumb,
    };
    let stages = [
        stage(true, all, true, false, true),
        stage(false, all, true, false, false),
        stage(false, all.min(1), true, false, false),
        stage(false, all.min(1), false, false, false),
        stage(false, 0, false, false, false),
        stage(false, 0, false, true, false),
    ];
    stages
        .iter()
        .find_map(|stage| try_stage(width, widths, *stage))
        .unwrap_or_else(|| {
            // 连「≡」与 `⋯` 都放不下：只画入口，按屏宽裁。
            TitleLayout {
                launcher: (0, display_width_u16(LAUNCHER_COMPACT).min(width)),
                compact: true,
                breadcrumb: None,
                items: Vec::new(),
            }
        })
}

fn try_stage(width: u16, widths: &TitleWidths, stage: Stage) -> Option<TitleLayout> {
    let launcher = display_width_u16(if stage.compact {
        LAUNCHER_COMPACT
    } else {
        LAUNCHER
    });
    let mut items: Vec<(TitleItem, u16)> = Vec::new();
    if stage.arrange {
        if let Some(arrange) = widths.arrange {
            items.push((TitleItem::Arrange, arrange));
        }
    }
    for (index, chip) in widths.chips.iter().take(stage.chips).enumerate() {
        items.push((TitleItem::Chip(index), *chip));
    }
    if stage.command {
        if let Some(command) = widths.command {
            items.push((TitleItem::Command, command));
        }
    }
    items.push((TitleItem::Menu, display_width_u16(MENU)));
    let right = items
        .iter()
        .fold(0u16, |total, (_, width)| total.saturating_add(*width))
        .saturating_add(ITEM_GAP.saturating_mul(items.len().saturating_sub(1) as u16));
    if launcher.saturating_add(1).saturating_add(right) > width {
        return None;
    }
    let right_x = width - right;
    let breadcrumb_x = launcher.saturating_add(LEFT_GAP);
    let room = right_x
        .saturating_sub(RIGHT_GAP)
        .saturating_sub(breadcrumb_x);
    let breadcrumb = if widths.breadcrumb == 0 {
        None
    } else if room >= widths.breadcrumb {
        Some((breadcrumb_x, widths.breadcrumb))
    } else if stage.full_breadcrumb {
        return None;
    } else if room >= BREADCRUMB_MIN {
        Some((breadcrumb_x, room))
    } else {
        None
    };
    let mut x = right_x;
    let items = items
        .into_iter()
        .map(|(item, width)| {
            let placed = (item, x, width);
            x = x.saturating_add(width).saturating_add(ITEM_GAP);
            placed
        })
        .collect();
    Some(TitleLayout {
        launcher: (0, launcher),
        compact: stage.compact,
        breadcrumb,
        items,
    })
}

/// 一个注意力计数：桶、个数与字形（运行中是当前转圈帧）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::client::shell) struct TitleChip {
    pub(in crate::client::shell) bucket: AgentBucket,
    pub(in crate::client::shell) count: usize,
    pub(in crate::client::shell) glyph: &'static str,
}

/// 计数从桶计数里按 [`CHIP_ORDER`] 取，零计数不画；空闲从不显示。
pub(in crate::client::shell) fn chips(
    counts: &BucketCounts,
    glyph: impl Fn(AgentBucket) -> &'static str,
) -> Vec<TitleChip> {
    CHIP_ORDER
        .iter()
        .filter_map(|bucket| {
            let count = counts.get(*bucket);
            (count > 0).then(|| TitleChip {
                bucket: *bucket,
                count,
                glyph: glyph(*bucket),
            })
        })
        .collect()
}

fn chip_label(chip: &TitleChip) -> String {
    format!(" {} {} ", chip.glyph, chip.count)
}

/// 标题栏上指针悬浮或「按下中」（对应的菜单开着）的一项。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::client::shell) enum TitleTarget {
    Launcher,
    Arrange,
    Chip(AgentBucket),
    Command,
    Menu,
}

/// 一帧标题栏的内容（由调用方从状态合成，绘制只读它）。
pub(in crate::client::shell) struct TitleBarView<'a> {
    pub(in crate::client::shell) breadcrumb: Vec<Span<'a>>,
    pub(in crate::client::shell) chips: Vec<TitleChip>,
    pub(in crate::client::shell) arrange: Option<&'a str>,
    /// 命令搜索的键位（键帽文字）；`None` = 没配键位，只画放大镜。
    pub(in crate::client::shell) command: Option<String>,
    /// 键帽后的说明（「命令」）；没配键位时不画。
    pub(in crate::client::shell) command_label: &'a str,
    /// 命令提示是否存在（不接鼠标、也没配键位时整项不画）。
    pub(in crate::client::shell) show_command: bool,
    pub(in crate::client::shell) hovered: Option<TitleTarget>,
    pub(in crate::client::shell) open: Option<TitleTarget>,
}

/// 画出来的可点项（空 `Rect` = 这一帧没画）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(in crate::client::shell) struct TitleBarHits {
    pub(in crate::client::shell) launcher: Rect,
    pub(in crate::client::shell) arrange: Rect,
    pub(in crate::client::shell) chips: Vec<(Rect, AgentBucket)>,
    pub(in crate::client::shell) command: Rect,
    pub(in crate::client::shell) menu: Rect,
}

fn command_width(view: &TitleBarView<'_>) -> Option<u16> {
    view.show_command.then(|| {
        display_width_u16(SEARCH_GLYPH).saturating_add(view.command.as_deref().map_or(0, |key| {
            let label = display_width_u16(view.command_label);
            crate::ui::kit::key_cap::cap_width(key)
                .saturating_add(if label > 0 {
                    label.saturating_add(1)
                } else {
                    0
                })
                .saturating_add(1)
        }))
    })
}

/// 画标题栏：整行铺侧栏面，按 [`layout`] 摆放各项，返回命中区。
pub(in crate::client::shell) fn paint(
    buffer: &mut Buffer,
    row: Rect,
    view: &TitleBarView<'_>,
    palette: &Palette,
    components: &ComponentStyles,
) -> TitleBarHits {
    let mut hits = TitleBarHits::default();
    if row.is_empty() {
        return hits;
    }
    let row = Rect::new(row.x, row.y, row.width, 1);
    let bg = super::bar_surface(palette);
    buffer.set_style(
        row,
        Style::default()
            .bg(bg)
            .fg(palette.text)
            .remove_modifier(Modifier::all()),
    );
    let chip_labels = view.chips.iter().map(chip_label).collect::<Vec<_>>();
    let widths = TitleWidths {
        breadcrumb: spans_width(&view.breadcrumb),
        arrange: view
            .arrange
            .map(|label| display_width_u16(label).saturating_add(2)),
        chips: chip_labels
            .iter()
            .map(|label| display_width_u16(label))
            .collect(),
        command: command_width(view),
    };
    let placed = layout(row.width, &widths);
    let rect = |x: u16, width: u16| Rect::new(row.x.saturating_add(x), row.y, width, 1);
    let lit = |target: TitleTarget| {
        if view.open == Some(target) {
            Some(palette.surface1)
        } else if view.hovered == Some(target) {
            Some(components.hover_bg)
        } else {
            None
        }
    };

    // 入口：「≡」弱一档，「herdr」加粗；悬浮换底，菜单开着时按下态。
    let launcher = rect(placed.launcher.0, placed.launcher.1);
    let launcher_bg = lit(TitleTarget::Launcher).unwrap_or(bg);
    let active = launcher_bg != bg;
    let mut spans = vec![
        Span::new(" ", Style::default().bg(launcher_bg)),
        Span::new(
            "≡",
            Style::default()
                .fg(if active {
                    palette.text
                } else {
                    palette.overlay1
                })
                .bg(launcher_bg),
        ),
        Span::new(" ", Style::default().bg(launcher_bg)),
    ];
    if !placed.compact {
        spans.push(Span::new(
            "herdr",
            crate::ui::design::title(palette).bg(launcher_bg),
        ));
        spans.push(Span::new(" ", Style::default().bg(launcher_bg)));
    }
    paint_spans(buffer, launcher.x, row.y, launcher.right(), &spans);
    hits.launcher = launcher;

    if let Some((x, width)) = placed.breadcrumb {
        let area = rect(x, width);
        let fitted = truncate_spans_left(
            &view.breadcrumb,
            width,
            Style::default().fg(palette.overlay0),
        );
        paint_spans(buffer, area.x, row.y, area.right(), &fitted);
    }

    for (item, x, width) in &placed.items {
        let area = rect(*x, *width);
        match item {
            TitleItem::Arrange => {
                if let Some(label) = view.arrange {
                    let style = crate::ui::design::key_cap_primary(palette);
                    paint_spans(
                        buffer,
                        area.x,
                        row.y,
                        area.right(),
                        &[Span::new(format!(" {label} "), style)],
                    );
                    hits.arrange = area;
                }
            }
            TitleItem::Chip(index) => {
                let (Some(chip), Some(label)) = (view.chips.get(*index), chip_labels.get(*index))
                else {
                    continue;
                };
                let mut style = crate::ui::design::status_style(chip.bucket, palette, components);
                if let Some(lit) = lit(TitleTarget::Chip(chip.bucket)) {
                    style = style.bg(lit);
                }
                paint_spans(
                    buffer,
                    area.x,
                    row.y,
                    area.right(),
                    &[Span::new(label.as_str(), style)],
                );
                hits.chips.push((area, chip.bucket));
            }
            TitleItem::Command => {
                let lit = lit(TitleTarget::Command);
                let glyph = Style::default()
                    .fg(if lit.is_some() {
                        palette.text
                    } else {
                        palette.overlay1
                    })
                    .bg(lit.unwrap_or(bg));
                let mut spans = vec![Span::new(SEARCH_GLYPH, glyph)];
                if let Some(key) = view.command.as_deref() {
                    // 与状态栏同一档的栏内键帽 + 说明色标签；悬浮时字提亮。
                    let mut cap = super::bar_key_cap(palette, bg);
                    if lit.is_some() {
                        cap = cap.fg(palette.text);
                    }
                    spans.push(Span::new(
                        format!(" {} ", crate::ui::kit::key_cap::cap_text(key)),
                        cap,
                    ));
                    if !view.command_label.is_empty() {
                        spans.push(Span::new(
                            format!(" {}", view.command_label),
                            super::bar_hint_label(palette, bg).bg(lit.unwrap_or(bg)),
                        ));
                    }
                    spans.push(Span::new(" ", Style::default().bg(lit.unwrap_or(bg))));
                }
                paint_spans(buffer, area.x, row.y, area.right(), &spans);
                hits.command = area;
            }
            TitleItem::Menu => {
                let lit = lit(TitleTarget::Menu);
                let style = Style::default()
                    .fg(if lit.is_some() {
                        palette.text
                    } else {
                        palette.overlay1
                    })
                    .bg(lit.unwrap_or(bg))
                    .add_modifier(Modifier::BOLD);
                paint_spans(
                    buffer,
                    area.x,
                    row.y,
                    area.right(),
                    &[Span::new(MENU, style)],
                );
                hits.menu = area;
            }
        }
    }
    hits
}

/// 面包屑里一段的角色：决定样式与去重规则。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::client::shell) enum CrumbRole {
    /// 机器（只在连着多个端点时出现）。
    Endpoint,
    Workspace,
    Tab,
}

/// 组装面包屑的输入（按身份与角色给，不是拼好的字符串）。
#[derive(Debug, Clone, Default)]
pub(in crate::client::shell) struct BreadcrumbInput<'a> {
    /// 机器名：只在多端点时给。
    pub(in crate::client::shell) endpoint: Option<&'a str>,
    pub(in crate::client::shell) workspace: Option<&'a str>,
    pub(in crate::client::shell) tab: Option<TabCrumb<'a>>,
    /// `(字形, 字形样式, 名称)`：聚焦窗格是 agent 时。
    pub(in crate::client::shell) agent: Option<(&'static str, Style, &'a str)>,
}

/// 标签段的输入。
#[derive(Debug, Clone, Copy)]
pub(in crate::client::shell) struct TabCrumb<'a> {
    pub(in crate::client::shell) label: &'a str,
    /// 用户起过名字（否则只是自动编号）。
    pub(in crate::client::shell) custom: bool,
    /// 同一工作区里有几个标签。
    pub(in crate::client::shell) siblings: usize,
}

/// 去重后的面包屑（纯数据）：`[机器 ›] 工作区 › 标签 › ⠋ agent`。
///
/// - 单标签且标签只是自动编号、或与工作区同名时省略标签段——它不提供定位信息；
///   多个标签时保留（编号也是定位）。
/// - 相邻同名段压缩成一段；最后一段与 agent 同名时由 agent 段（带状态字形）代表。
/// - 普通终端（不是 agent）不凭空加 agent 段。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(in crate::client::shell) struct BreadcrumbModel<'a> {
    pub(in crate::client::shell) segments: Vec<(CrumbRole, &'a str)>,
    pub(in crate::client::shell) agent: Option<(&'static str, Style, &'a str)>,
}

impl<'a> BreadcrumbModel<'a> {
    pub(in crate::client::shell) fn build(input: &BreadcrumbInput<'a>) -> Self {
        let mut segments: Vec<(CrumbRole, &'a str)> = Vec::with_capacity(3);
        let mut push = |role, label: &'a str| {
            let label = label.trim();
            if label.is_empty() {
                return;
            }
            match segments.last_mut() {
                // 相邻同名压缩成一段；与工作区同名时保留工作区的角色（定位锚点的样式）。
                Some((last_role, last)) if *last == label => {
                    if role == CrumbRole::Workspace {
                        *last_role = role;
                    }
                }
                _ => segments.push((role, label)),
            }
        };
        if let Some(endpoint) = input.endpoint {
            push(CrumbRole::Endpoint, endpoint);
        }
        if let Some(workspace) = input.workspace {
            push(CrumbRole::Workspace, workspace);
        }
        if let Some(tab) = input.tab {
            let redundant = tab.siblings <= 1
                && (!tab.custom
                    || input
                        .workspace
                        .is_some_and(|ws| ws.trim() == tab.label.trim()));
            if !redundant {
                push(CrumbRole::Tab, tab.label);
            }
        }
        let agent = input.agent.filter(|(_, _, name)| !name.trim().is_empty());
        if let Some((_, _, name)) = agent {
            // 只压掉与 agent 同名的标签段：工作区段是定位锚点，保留。
            if segments
                .last()
                .is_some_and(|(role, last)| *role == CrumbRole::Tab && *last == name.trim())
            {
                segments.pop();
            }
        }
        Self { segments, agent }
    }
}

/// 面包屑的 spans（目标样式：`herdr  build › ⠋ codex`）：工作区粗体正文、机器与标签
/// 说明色、分隔符 `›` 结构色；agent 段是状态字形 + 粗体名字。
pub(in crate::client::shell) fn breadcrumb_spans<'a>(
    model: &BreadcrumbModel<'a>,
    palette: &Palette,
) -> Vec<Span<'a>> {
    let separator = Style::default().fg(palette.overlay0);
    let mut spans = Vec::with_capacity(model.segments.len() * 2 + 4);
    for (index, (role, segment)) in model.segments.iter().enumerate() {
        if index > 0 {
            spans.push(Span::new(" › ", separator));
        }
        let style = match role {
            CrumbRole::Workspace => crate::ui::design::title(palette),
            CrumbRole::Endpoint | CrumbRole::Tab => crate::ui::design::muted(palette),
        };
        spans.push(Span::new(*segment, style));
    }
    if let Some((glyph, glyph_style, name)) = model.agent {
        if !spans.is_empty() {
            spans.push(Span::new(" › ", separator));
        }
        if !glyph.is_empty() {
            spans.push(Span::new(glyph, glyph_style));
            spans.push(Span::new(" ", Style::default()));
        }
        spans.push(Span::new(name, crate::ui::design::title(palette)));
    }
    spans
}

impl super::super::ClientShellState {
    /// 工作台标题栏（第 0 行）：按当前状态合成内容并画进 `buffer`，返回命中区（由
    /// 调用方登记——这里只读状态）。
    pub(in crate::client::shell) fn paint_workbench_title_bar(
        &self,
        buffer: &mut Buffer,
        row: Rect,
        snapshot: &crate::protocol::ClientShellSnapshot,
    ) -> TitleBarHits {
        use super::super::command_palette::BrowserView;
        use super::super::dock::PanelId;
        use super::super::feedback::ChromeHover;
        use super::super::ClientShellOverlay;
        let palette = &self.config.palette;
        let glyphs = self.status_glyphs();

        // 面包屑跟着聚焦的终端：终端组聚焦时取它这一栏的活动标签，侧栏 / 监控面板
        // 聚焦时取服务端的聚焦标签；agent 段只在这个标签的聚焦窗格是 agent 时出现。
        let tab_id = match &self.workbench.dock.focused {
            PanelId::Terminal(id) => self
                .workbench
                .dock
                .groups
                .iter()
                .find(|group| group.id == *id)
                .and_then(|group| group.active.as_deref()),
            _ => None,
        }
        .or(snapshot.focused_tab_id.as_deref());
        let tab = tab_id.and_then(|id| snapshot.tabs.iter().find(|tab| tab.tab_id == id));
        let workspace = tab.and_then(|tab| {
            snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.workspace_id == tab.workspace_id)
        });
        let agent = snapshot
            .focused_pane_id
            .as_deref()
            .filter(|pane_id| {
                snapshot
                    .panes
                    .iter()
                    .any(|pane| pane.pane_id == *pane_id && Some(pane.tab_id.as_str()) == tab_id)
            })
            .and_then(|pane_id| {
                snapshot
                    .agents
                    .iter()
                    .find(|agent| agent.pane_id == pane_id)
            });
        let crumbs = BreadcrumbModel::build(&BreadcrumbInput {
            endpoint: (self.endpoints.len() > 1).then(|| self.active_endpoint_label()),
            workspace: workspace.map(|workspace| workspace.label.as_str()),
            tab: tab.map(|tab| TabCrumb {
                label: tab.label.as_str(),
                custom: tab.custom_label,
                siblings: snapshot
                    .tabs
                    .iter()
                    .filter(|other| other.workspace_id == tab.workspace_id)
                    .count(),
            }),
            agent: agent.and_then(|agent| {
                let name = agent
                    .display_agent
                    .as_deref()
                    .or(agent.name.as_deref())
                    .or(agent.agent.as_deref())
                    .or(agent.title.as_deref())?;
                let bucket = super::super::client_agent_bucket(agent);
                // 端点不在线时快照是缓存：与侧栏 stale 行同口径，静态字形、置灰。
                let stale = self.active_endpoint_stale();
                let glyph = if bucket == AgentBucket::None {
                    ""
                } else if stale {
                    super::super::StatusGlyphs::fixed(self.config.status_indicators).bucket(bucket)
                } else {
                    glyphs.bucket(bucket)
                };
                let style = if stale {
                    Style::default().fg(palette.overlay0)
                } else {
                    crate::ui::design::status_style(bucket, palette, &self.config.components)
                };
                Some((glyph, style, name))
            }),
        });

        let counts = self
            .agent_attention
            .as_ref()
            .map(|rows| rows.counts())
            .unwrap_or_default();
        let command = super::key_sequence(
            &self.config.keybinds,
            &self.config.keybinds.keybinds.command_search,
        );
        let hovered = match self.hover.as_ref() {
            Some(ChromeHover::GlobalLauncher) => Some(TitleTarget::Launcher),
            Some(ChromeHover::TitleChip(bucket)) => Some(TitleTarget::Chip(*bucket)),
            Some(ChromeHover::TitleCommand) => Some(TitleTarget::Command),
            Some(ChromeHover::TitleMenu) => Some(TitleTarget::Menu),
            Some(ChromeHover::TitleArrange) => Some(TitleTarget::Arrange),
            _ => None,
        };
        let open = match self.overlay.as_ref() {
            Some(ClientShellOverlay::CommandPalette(palette)) => match palette.view {
                BrowserView::Layout => Some(TitleTarget::Menu),
                BrowserView::Menu(_) => Some(TitleTarget::Launcher),
                BrowserView::Search => None,
            },
            _ => None,
        };
        let view = TitleBarView {
            breadcrumb: breadcrumb_spans(&crumbs, palette),
            chips: chips(&counts, |bucket| glyphs.bucket(bucket)),
            arrange: self
                .workbench
                .arranging
                .then_some(crate::i18n::texts().menu.arrange_chip),
            // 不接鼠标时提示点不了，没配键位也就没什么可提示的。
            show_command: self.config.mouse_capture || command.is_some(),
            command,
            command_label: super::super::observability::tr("Commands", "命令"),
            hovered,
            open,
        };
        paint(buffer, row, &view, palette, &self.config.components)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn widths(breadcrumb: u16, arrange: bool, chips: usize, command: bool) -> TitleWidths {
        TitleWidths {
            breadcrumb,
            arrange: arrange.then_some(10),
            chips: vec![5; chips],
            command: command.then_some(13),
        }
    }

    fn items(layout: &TitleLayout) -> Vec<TitleItem> {
        layout.items.iter().map(|(item, _, _)| *item).collect()
    }

    #[test]
    fn wide_bars_show_everything_with_the_menu_pinned_right() {
        let placed = layout(160, &widths(30, true, 3, true));
        assert_eq!(placed.launcher, (0, 9));
        assert!(!placed.compact);
        assert_eq!(placed.breadcrumb, Some((11, 30)));
        assert_eq!(
            items(&placed),
            vec![
                TitleItem::Arrange,
                TitleItem::Chip(0),
                TitleItem::Chip(1),
                TitleItem::Chip(2),
                TitleItem::Command,
                TitleItem::Menu,
            ]
        );
        let (_, x, width) = placed.items.last().copied().expect("⋯");
        assert_eq!(x + width, 160, "⋯ 贴右缘");
        for pair in placed.items.windows(2) {
            assert_eq!(pair[0].1 + pair[0].2 + ITEM_GAP, pair[1].1, "项间 1 列");
        }
    }

    #[test]
    fn narrowing_drops_the_command_hint_before_touching_the_breadcrumb() {
        // 全部放下需要：入口 9 + 2 + 面包屑 30 + 2 + 右侧 (10+5+13+3+3) = 77。
        let full = widths(30, true, 1, true);
        assert!(items(&layout(77, &full)).contains(&TitleItem::Command));
        let placed = layout(76, &full);
        assert!(
            !items(&placed).contains(&TitleItem::Command),
            "先丢命令提示"
        );
        assert_eq!(placed.breadcrumb, Some((11, 30)), "面包屑仍完整");
    }

    #[test]
    fn the_breadcrumb_truncates_then_hides_before_chips_collapse() {
        let full = widths(30, false, 3, true);
        // 去掉命令提示后右侧 = 5+5+5+3 + 3 = 21：留给面包屑 width - 9 - 2 - 2 - 21。
        let truncated = layout(50, &full);
        assert_eq!(truncated.breadcrumb, Some((11, 16)), "从左截断");
        assert_eq!(items(&truncated).len(), 4, "三个计数都在");
        let hidden = layout(40, &full);
        assert_eq!(hidden.breadcrumb, None, "放不下最小宽度就隐藏");
        assert_eq!(items(&hidden).len(), 4, "隐藏面包屑之后计数仍完整");
    }

    #[test]
    fn extreme_widths_collapse_chips_then_drop_marks_then_shrink_the_launcher() {
        let full = widths(30, true, 3, true);
        // 入口 9 + 1 + 右侧 (10 + 5*3 + 3 + 4 个间隔) = 42 → 41 列时计数收成一个。
        assert_eq!(items(&layout(42, &full)).len(), 5);
        assert_eq!(
            items(&layout(41, &full)),
            vec![TitleItem::Arrange, TitleItem::Chip(0), TitleItem::Menu]
        );
        // 9 + 1 + (10 + 5 + 3 + 2) = 30；29 列丢「调整布局」标记。
        assert_eq!(
            items(&layout(29, &full)),
            vec![TitleItem::Chip(0), TitleItem::Menu]
        );
        // 9 + 1 + (5 + 3 + 1) = 19；18 列只剩 ⋯。
        assert_eq!(items(&layout(18, &full)), vec![TitleItem::Menu]);
        assert!(!layout(13, &full).compact, "9 + 1 + 3 = 13 列时入口完整");
        let tiny = layout(12, &full);
        assert!(tiny.compact, "入口收成「≡」");
        assert_eq!(tiny.launcher, (0, 3));
        assert_eq!(items(&tiny), vec![TitleItem::Menu], "⋯ 始终在");
        assert_eq!(
            layout(3, &full).items,
            Vec::new(),
            "连 ⋯ 都放不下时只剩入口"
        );
    }

    #[test]
    fn chips_follow_rollup_priority_and_skip_zero_and_idle() {
        let counts = BucketCounts {
            needs_input: 1,
            failed: 0,
            working: 2,
            review: 3,
            idle: 9,
        };
        let chips = chips(&counts, |bucket| match bucket {
            AgentBucket::Working => "⠹",
            _ => "◆",
        });
        assert_eq!(
            chips
                .iter()
                .map(|chip| (chip.bucket, chip.count))
                .collect::<Vec<_>>(),
            vec![
                (AgentBucket::NeedsInput, 1),
                (AgentBucket::Working, 2),
                (AgentBucket::Review, 3),
            ]
        );
        assert_eq!(chip_label(&chips[1]), " ⠹ 2 ");
    }

    fn crumb_text(input: &BreadcrumbInput<'_>) -> String {
        let palette = Palette::graphite_dark();
        breadcrumb_spans(&BreadcrumbModel::build(input), &palette)
            .iter()
            .map(|span| span.text.as_ref())
            .collect()
    }

    fn tab(label: &str, custom: bool, siblings: usize) -> Option<TabCrumb<'_>> {
        Some(TabCrumb {
            label,
            custom,
            siblings,
        })
    }

    #[test]
    fn breadcrumb_styles_follow_the_refined_reference() {
        let palette = Palette::graphite_dark();
        let glyph = Style::default().fg(palette.blue);
        let model = BreadcrumbModel::build(&BreadcrumbInput {
            endpoint: None,
            workspace: Some("herdr"),
            tab: tab("build", true, 2),
            agent: Some(("⠋", glyph, "codex")),
        });
        let spans = breadcrumb_spans(&model, &palette);
        let text: String = spans.iter().map(|span| span.text.as_ref()).collect();
        assert_eq!(text, "herdr › build › ⠋ codex");
        assert_eq!(
            spans[0].style,
            crate::ui::design::title(&palette),
            "工作区粗体正文"
        );
        assert_eq!(spans[1].style.fg, Some(palette.overlay0), "分隔符结构色");
        assert_eq!(
            spans[2].style,
            crate::ui::design::muted(&palette),
            "标签说明色"
        );
        assert_eq!(spans[4].style, glyph, "agent 带状态字形");
        assert_eq!(
            spans.last().map(|span| span.style),
            Some(crate::ui::design::title(&palette)),
            "agent 名字粗体"
        );
    }

    #[test]
    fn breadcrumb_model_drops_redundant_segments() {
        let glyph = Style::default();
        // 截屏里的「herdr / 1 · claude」：单标签的自动编号不提供定位。
        assert_eq!(
            crumb_text(&BreadcrumbInput {
                workspace: Some("herdr"),
                tab: tab("1", false, 1),
                agent: Some(("◆", glyph, "claude")),
                ..Default::default()
            }),
            "herdr › ◆ claude"
        );
        // 多标签时编号就是定位，保留。
        assert_eq!(
            crumb_text(&BreadcrumbInput {
                workspace: Some("herdr"),
                tab: tab("2", false, 3),
                ..Default::default()
            }),
            "herdr › 2"
        );
        // 单标签与工作区同名：省略；多标签同名：相邻同名压缩。
        for siblings in [1, 2] {
            assert_eq!(
                crumb_text(&BreadcrumbInput {
                    workspace: Some("herdr"),
                    tab: tab("herdr", true, siblings),
                    ..Default::default()
                }),
                "herdr",
                "{siblings} 个标签"
            );
        }
        // 单标签但起过名字：保留。
        assert_eq!(
            crumb_text(&BreadcrumbInput {
                workspace: Some("herdr"),
                tab: tab("build", true, 1),
                ..Default::default()
            }),
            "herdr › build"
        );
        // 标签与 agent 同名：agent 段代表它；工作区与 agent 同名则保留工作区锚点。
        assert_eq!(
            crumb_text(&BreadcrumbInput {
                workspace: Some("web"),
                tab: tab("claude", true, 2),
                agent: Some(("●", glyph, "claude")),
                ..Default::default()
            }),
            "web › ● claude"
        );
        assert_eq!(
            crumb_text(&BreadcrumbInput {
                workspace: Some("claude"),
                tab: tab("1", false, 1),
                agent: Some(("●", glyph, "claude")),
                ..Default::default()
            }),
            "claude › ● claude"
        );
        // 多端点：机器段在最前；与工作区同名时压缩。
        assert_eq!(
            crumb_text(&BreadcrumbInput {
                endpoint: Some("devbox"),
                workspace: Some("herdr"),
                tab: tab("build", true, 2),
                agent: None,
            }),
            "devbox › herdr › build"
        );
        assert_eq!(
            crumb_text(&BreadcrumbInput {
                endpoint: Some("herdr"),
                workspace: Some("herdr"),
                ..Default::default()
            }),
            "herdr"
        );
        // 普通终端不加 agent 段；什么都没有就是空。
        assert_eq!(
            BreadcrumbModel::build(&BreadcrumbInput::default()),
            BreadcrumbModel::default()
        );
    }
}
