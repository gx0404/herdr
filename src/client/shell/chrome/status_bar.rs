//! 状态栏（最后一行，侧栏面底色）：左侧模式标签（终端模式低调，前缀 / 导航 / 复制 /
//! 调整 / 调整布局用 accent 反色），中部上下文键位提示——或紧凑 / 最大化 / 锁定等
//! 说明、复制模式的搜索栏、端点错误——右侧是 `tab_bar_right`（主机、时间等）。
//!
//! 经典布局的模式条用同一个组件：只在非终端模式（或有错误 / 广播）时画，位置与
//! 改版前一样压在终端区最后一行。

use std::borrow::Cow;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

use super::{paint_spans, spans_width, Span};
use crate::app::state::{ComponentStyles, Palette};
use crate::ui::display_width_u16;
use crate::ui::kit::footer_hints::{render_footer_hints_styled, FooterHint, FooterHintStyles};

/// 模式标签的语气。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::client::shell) enum Tone {
    /// 终端模式：低调的灰标签。
    Subtle,
    /// 前缀 / 导航 / 复制 / 调整 / 调整布局：accent 反色。
    Accent,
    /// 端点错误：红底。
    Error,
}

/// 一条键位提示（键帽 + 动作名）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::client::shell) struct Hint<'a> {
    pub(in crate::client::shell) key: Cow<'a, str>,
    pub(in crate::client::shell) label: Cow<'a, str>,
    pub(in crate::client::shell) enabled: bool,
    pub(in crate::client::shell) primary: bool,
}

impl<'a> Hint<'a> {
    pub(in crate::client::shell) fn new(
        key: impl Into<Cow<'a, str>>,
        label: impl Into<Cow<'a, str>>,
    ) -> Self {
        Self {
            key: key.into(),
            label: label.into(),
            enabled: true,
            primary: false,
        }
    }
}

/// 状态栏中部的内容。
pub(in crate::client::shell) enum Middle<'a> {
    Empty,
    /// 键位提示，放不下从尾部整项丢弃（`primary` 最后丢）；画出来的项都回报命中区。
    Hints(Vec<Hint<'a>>),
    /// 说明文字（紧凑 / 最大化 / 锁定 / 不接鼠标），放不下时先让终端模式的低调标签
    /// 让位，仍放不下以「…」收尾。
    Text(Cow<'a, str>),
    /// 端点错误，红字。
    Error(Cow<'a, str>),
    /// 复制模式的搜索栏：方向标记、输入框与右对齐的页脚。
    CopySearch {
        marker: &'static str,
        query: &'a super::super::TextEditor,
        footer: &'a str,
    },
}

/// 一帧状态栏的内容（由调用方从状态合成，绘制只读它）。
pub(in crate::client::shell) struct StatusBarView<'a> {
    /// 模式标签（文字自带左右内边距）；`None` = 不画（经典布局的终端模式）。
    pub(in crate::client::shell) mode: Option<(&'a str, Tone)>,
    /// 广播徽标：输入正扇出到其它机器，最醒目，跟在模式标签后。
    pub(in crate::client::shell) broadcast: Option<String>,
    pub(in crate::client::shell) middle: Middle<'a>,
    /// 指针悬浮的提示下标（只会是可点项）。
    pub(in crate::client::shell) hovered: Option<usize>,
    /// 右对齐的状态段。
    pub(in crate::client::shell) right: Vec<Span<'a>>,
}

/// 画出来的提示 `(命中矩形, 下标)` 与复制搜索栏的插入点。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(in crate::client::shell) struct StatusBarHits {
    pub(in crate::client::shell) hints: Vec<(Rect, usize)>,
}

/// 复制搜索栏：模式标签占 7 列、第 8 列画方向标记，输入框从第 9 列起（与改版前的
/// 模式条同一几何）；页脚只在栏宽 ≥ 50 时画。
const COPY_PREFIX: u16 = 8;
const COPY_FOOTER_MIN_WIDTH: u16 = 50;
/// 中部内容至少保留这么多列，否则让右侧状态段让位。
const MIDDLE_MIN: u16 = 16;

fn chip_style(tone: Tone, bg: Color, palette: &Palette, components: &ComponentStyles) -> Style {
    match tone {
        Tone::Subtle => {
            let chip = if Palette::row_bg_is_distinct(bg, palette.surface0) {
                palette.surface0
            } else {
                palette.surface1
            };
            Style::default()
                .fg(super::readable_on(
                    palette,
                    chip,
                    &[palette.overlay1, palette.subtext0, palette.text],
                ))
                .bg(chip)
                .add_modifier(Modifier::BOLD)
        }
        Tone::Accent => Style::default()
            .fg(crate::ui::color::contrast_fg(
                palette,
                components.mode_bar_accent,
            ))
            .bg(components.mode_bar_accent)
            .add_modifier(Modifier::BOLD),
        Tone::Error => Style::default()
            .fg(crate::ui::color::contrast_fg(palette, palette.red))
            .bg(palette.red)
            .add_modifier(Modifier::BOLD),
    }
}

/// 中部与右侧状态段的摆放：中部从 `x`（前面画了东西就空 1 列）开始；右侧状态段
/// 右对齐、离右缘 1 列，中部放不下最小宽度时状态段让位。返回中部矩形与状态段起点。
fn middle_layout(row: Rect, x: u16, right_width: u16, has_middle: bool) -> (Rect, Option<u16>) {
    let right_edge = row.right();
    let middle_x = if x > row.x { x.saturating_add(1) } else { x };
    let right_x = if right_width == 0 {
        None
    } else {
        let start = right_edge.saturating_sub(right_width.saturating_add(1));
        let room = start.saturating_sub(2).saturating_sub(middle_x);
        (start >= middle_x && (!has_middle || room >= MIDDLE_MIN)).then_some(start)
    };
    let middle_right = right_x.map_or(right_edge.saturating_sub(1), |start| {
        start.saturating_sub(2)
    });
    (
        Rect::new(middle_x, row.y, middle_right.saturating_sub(middle_x), 1),
        right_x,
    )
}

/// 画状态栏：整行铺 `bg`，广播徽标 → 模式标签 → 中部 → 右侧状态段。
pub(in crate::client::shell) fn paint(
    buffer: &mut Buffer,
    row: Rect,
    view: &StatusBarView<'_>,
    bg: Color,
    palette: &Palette,
    components: &ComponentStyles,
) -> StatusBarHits {
    let mut hits = StatusBarHits::default();
    if row.is_empty() {
        return hits;
    }
    let row = Rect::new(row.x, row.y, row.width, 1);
    buffer.set_style(
        row,
        Style::default()
            .bg(bg)
            .fg(palette.overlay1)
            .remove_modifier(Modifier::all()),
    );
    for x in row.x..row.right() {
        buffer[(x, row.y)].set_symbol(" ");
    }
    let right_edge = row.right();
    let mut x = row.x;
    // 广播徽标最醒目，排在最前（与改版前的模式条同序）。
    if let Some(badge) = view.broadcast.as_deref() {
        let style = Style::default()
            .fg(crate::ui::color::contrast_fg(palette, palette.peach))
            .bg(palette.peach)
            .add_modifier(Modifier::BOLD);
        x = paint_spans(buffer, x, row.y, right_edge, &[Span::new(badge, style)]);
    }
    let content_x = x;
    let right_width = spans_width(&view.right);
    let has_middle = !matches!(view.middle, Middle::Empty);
    // 终端模式的低调标签只是标识：说明文字（紧凑 / 最大化 / 锁定等）连同它放不下时
    // 标签先让位，说明尽量完整。
    let mode = view.mode.filter(|(label, tone)| match &view.middle {
        Middle::Text(text) if *tone == Tone::Subtle => {
            let after = content_x.saturating_add(display_width_u16(label));
            let (middle, _) = middle_layout(row, after, right_width, has_middle);
            display_width_u16(text) <= middle.width
        }
        _ => true,
    });
    if let Some((label, tone)) = mode {
        x = paint_spans(
            buffer,
            x,
            row.y,
            right_edge,
            &[Span::new(label, chip_style(tone, bg, palette, components))],
        );
    }

    if let Middle::CopySearch {
        marker,
        query,
        footer,
    } = &view.middle
    {
        paint_copy_search(buffer, row, content_x, marker, query, footer, palette);
        return hits;
    }

    let (middle, right_x) = middle_layout(row, x, right_width, has_middle);
    match &view.middle {
        Middle::Empty | Middle::CopySearch { .. } => {}
        Middle::Hints(hints) => {
            let footer = hints
                .iter()
                .map(|hint| FooterHint {
                    key: hint.key.as_ref(),
                    label: hint.label.as_ref(),
                    enabled: hint.enabled,
                    primary: hint.primary,
                })
                .collect::<Vec<_>>();
            // 常驻的栏：与页面页脚同一套键帽与说明色标签（按栏面挑可辨的底与字色），
            // 栏够宽，项间多留一格。
            let styles = FooterHintStyles {
                key: super::bar_key_cap(palette, bg),
                label: super::bar_hint_label(palette, bg),
                hover_bg: palette.hover_row_bg(),
                gap: crate::ui::kit::key_cap::BAR_HINT_GAP,
            };
            hits.hints =
                render_footer_hints_styled(buffer, middle, &footer, view.hovered, palette, styles);
        }
        Middle::Text(text) => {
            let style = Style::default().fg(super::readable_on(
                palette,
                bg,
                &[palette.overlay1, palette.subtext0],
            ));
            let fitted = crate::ui::truncate_end(text, usize::from(middle.width));
            paint_spans(
                buffer,
                middle.x,
                row.y,
                middle.right(),
                &[Span::new(fitted, style)],
            );
        }
        Middle::Error(text) => {
            let fitted = crate::ui::truncate_end(text, usize::from(middle.width));
            paint_spans(
                buffer,
                middle.x,
                row.y,
                middle.right(),
                &[Span::new(fitted, Style::default().fg(palette.red))],
            );
        }
    }
    if let Some(start) = right_x {
        paint_spans(buffer, start, row.y, right_edge, &view.right);
    }
    hits
}

fn paint_copy_search(
    buffer: &mut Buffer,
    row: Rect,
    content_x: u16,
    marker: &str,
    query: &super::super::TextEditor,
    footer: &str,
    palette: &Palette,
) {
    // 几何以模式标签起点（广播徽标之后）为准。
    let chip_start = content_x;
    let width = row.right().saturating_sub(chip_start);
    let prefix = COPY_PREFIX.min(width);
    if prefix >= COPY_PREFIX {
        paint_spans(
            buffer,
            chip_start + COPY_PREFIX - 1,
            row.y,
            row.right(),
            &[Span::new(
                marker,
                Style::default()
                    .fg(palette.text)
                    .add_modifier(Modifier::BOLD),
            )],
        );
    }
    let footer_width = if width >= COPY_FOOTER_MIN_WIDTH {
        display_width_u16(footer)
    } else {
        0
    };
    let field = Rect::new(
        chip_start + prefix,
        row.y,
        width.saturating_sub(prefix + footer_width),
        1,
    );
    let field_style = Style::default().fg(palette.text).bg(palette.surface0);
    if let Some(cursor) = super::super::text_editor::render(buffer, field, query, field_style) {
        buffer[(cursor.x, cursor.y)]
            .set_style(Style::default().fg(palette.surface0).bg(palette.text));
    }
    if footer_width > 0 {
        paint_spans(
            buffer,
            row.right() - footer_width,
            row.y,
            row.right(),
            &[Span::new(footer, crate::ui::design::meta(palette))],
        );
    }
}

/// `tab_bar_right` 的段 → 右侧状态段：accent 段是反色小标签，其余元信息色，段间用
/// 服务端给的分隔符（弱一档）。
pub(in crate::client::shell) fn status_segments<'a>(
    segments: &'a [crate::protocol::ClientShellTabStatusSegment],
    separator: &'a str,
    palette: &Palette,
) -> Vec<Span<'a>> {
    let mut spans = Vec::with_capacity(segments.len() * 2);
    for (index, segment) in segments.iter().enumerate() {
        if index > 0 && !separator.is_empty() {
            spans.push(Span::new(separator, Style::default().fg(palette.overlay0)));
        }
        let style = if segment.accent {
            Style::default()
                .fg(crate::ui::panel_contrast_fg(palette))
                .bg(palette.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            crate::ui::design::meta(palette)
        };
        spans.push(Span::new(segment.text.as_str(), style));
    }
    spans
}

/// 非终端模式（前缀 / 导航 / 调整 / 复制）与端点错误的标签与中部内容：经典布局的
/// 模式条与工作台状态栏共用同一份。终端模式且没有错误时是 `None`，由调用方决定。
pub(in crate::client::shell) fn mode_content<'a>(
    mode: super::super::ClientShellMode,
    copy_mode: Option<&'a super::super::ClientCopyModeState>,
    endpoint_error: Option<&'a str>,
    keybinds: &crate::config::LiveKeybindConfig,
) -> Option<((&'static str, Tone), Middle<'a>)> {
    use super::super::ClientShellMode;
    let t = &crate::i18n::texts().mode_bar;
    if let Some(error) = endpoint_error {
        return Some(((t.error, Tone::Error), Middle::Error(Cow::Borrowed(error))));
    }
    let unset = || crate::i18n::texts().keybinds.unset.to_owned();
    let prefix_rhs = |bindings: &crate::config::ActionKeybinds| {
        bindings.prefix_rhs_label().unwrap_or_else(unset)
    };
    let label = |bindings: &crate::config::ActionKeybinds| bindings.label().unwrap_or_else(unset);
    let (chip, hints) = match mode {
        ClientShellMode::Terminal => return None,
        ClientShellMode::Prefix => (
            t.prefix,
            vec![
                Hint::new("esc", t.prefix_cancel),
                Hint::new(keybinds.primary_prefix_label(), t.prefix_send),
                Hint::new(
                    prefix_rhs(&keybinds.keybinds.workspace_picker),
                    t.prefix_nav,
                ),
                Hint::new(prefix_rhs(&keybinds.keybinds.help), t.prefix_keybinds),
            ],
        ),
        ClientShellMode::Navigate => (
            t.navigate,
            vec![
                Hint::new("esc", t.nav_back),
                Hint::new(
                    format!(
                        "{} / {}",
                        label(&keybinds.keybinds.navigate.workspace_up),
                        label(&keybinds.keybinds.navigate.workspace_down)
                    ),
                    t.nav_workspace,
                ),
                Hint::new("tab", t.nav_pane),
                Hint::new(prefix_rhs(&keybinds.keybinds.help), t.prefix_keybinds),
            ],
        ),
        ClientShellMode::Resize => (
            t.resize,
            vec![
                Hint::new("h/l", t.resize_width),
                Hint::new("j/k", t.resize_height),
                Hint::new("esc", t.resize_done),
            ],
        ),
        ClientShellMode::Copy => {
            let Some(copy_mode) = copy_mode else {
                return Some(((t.copy, Tone::Accent), Middle::Empty));
            };
            if let Some(prompt) = copy_mode.search_prompt.as_ref() {
                let marker = match prompt.direction {
                    crate::api::schema::PaneCopySearchDirection::Forward => "/",
                    crate::api::schema::PaneCopySearchDirection::Backward => "?",
                };
                return Some((
                    (t.copy, Tone::Accent),
                    Middle::CopySearch {
                        marker,
                        query: &prompt.query,
                        footer: t.copy_footer,
                    },
                ));
            }
            let select = if copy_mode.selection.is_some() {
                "selecting"
            } else {
                "select"
            };
            let match_status = copy_mode
                .search_current_global
                .map(|current| format!(" {}/{}", current + 1, copy_mode.search_total))
                .or_else(|| (!copy_mode.search_query.is_empty()).then(|| " 0/0".to_owned()))
                .unwrap_or_default();
            let (exit_keys, exit_label) =
                if copy_mode.search_query.is_empty() && copy_mode.selection.is_none() {
                    ("q/esc", "exit")
                } else {
                    ("esc", "clear · q exit")
                };
            (
                t.copy,
                vec![
                    Hint::new("h/j/k/l w/b/e { }", "move"),
                    Hint::new("/ ?", "search"),
                    Hint::new("n/N", format!("repeat{match_status}")),
                    Hint::new("v/space", select),
                    Hint::new("y/enter", "copy"),
                    Hint::new(exit_keys, exit_label),
                ],
            )
        }
    };
    Some(((chip, Tone::Accent), Middle::Hints(hints)))
}

/// 广播徽标（输入正扇出到其它机器时）。
pub(in crate::client::shell) fn broadcast_badge(count: usize) -> String {
    crate::i18n::fill(
        crate::i18n::texts().mode_bar.broadcast_fmt,
        &[("count", &count.to_string())],
    )
}

/// 状态栏中部的提示与每条提示点下去做什么（`None` = 只是提示，不可点）。
type StatusHints = (
    Middle<'static>,
    Vec<Option<super::super::workbench::interaction::Action>>,
);

impl super::super::ClientShellState {
    /// 工作台状态栏：画在最后一行（侧栏面底色），返回可点提示的 `(矩形, 下标, 动作)`
    /// ——调用方登记进工作台命中表与页脚悬浮命中表（`footer_hits`），这里只读状态。
    /// `compact_switcher_shown`：这一帧面板头画了紧凑视图的切换条（说明改成「点上方
    /// 的面板名」）。
    pub(in crate::client::shell) fn paint_workbench_status_bar(
        &self,
        buffer: &mut Buffer,
        footer: Rect,
        compact_switcher_shown: bool,
    ) -> Vec<(Rect, usize, super::super::workbench::interaction::Action)> {
        let t = &crate::i18n::texts().mode_bar;
        let palette = &self.config.palette;
        let mut actions = Vec::new();
        // 子智能体页面打开时：模式块写「子智能体」，中部是页面的固定键位提示；端点
        // 错误仍优先。
        let page = self
            .endpoint_error
            .is_none()
            .then(|| self.agent_page_status())
            .flatten();
        let (mode, middle) = match page.or_else(|| {
            mode_content(
                self.mode,
                self.copy_mode.as_ref(),
                self.endpoint_error.as_deref(),
                &self.config.keybinds,
            )
        }) {
            Some(content) => content,
            None if self.workbench.arranging => {
                let (middle, clickable) = self.arrange_hints();
                actions = clickable;
                ((t.arrange, Tone::Accent), middle)
            }
            None => {
                let (middle, clickable) = self.terminal_status(compact_switcher_shown);
                actions = clickable;
                ((t.terminal, Tone::Subtle), middle)
            }
        };
        let right = self
            .snapshot
            .as_deref()
            .map(|snapshot| {
                status_segments(
                    &snapshot.tab_bar_right,
                    &snapshot.tab_bar_right_separator,
                    palette,
                )
            })
            .unwrap_or_default();
        let view = StatusBarView {
            mode: Some(mode),
            broadcast: self.broadcast_indicator_count().map(broadcast_badge),
            middle,
            hovered: self.workbench.footer_hover,
            right,
        };
        let hits = paint(
            buffer,
            footer,
            &view,
            super::bar_surface(palette),
            palette,
            &self.config.components,
        );
        hits.hints
            .into_iter()
            .filter_map(|(rect, index)| {
                actions
                    .get(index)
                    .cloned()
                    .flatten()
                    .map(|action| (rect, index, action))
            })
            .collect()
    }

    /// 子智能体页面开着时状态栏的模式块与中部提示（页面固定键，不是可配置键位；外部
    /// 来源不列 t）。
    fn agent_page_status(&self) -> Option<((&'static str, Tone), Middle<'static>)> {
        let Some(super::super::ClientShellOverlay::AgentActivity(overlay)) = self.overlay.as_ref()
        else {
            return None;
        };
        let hints = super::agent_page::page_hints(overlay.is_external())
            .into_iter()
            .map(|(key, label)| Hint::new(key, label))
            .collect();
        Some((
            (crate::i18n::texts().agent_activity.mode_chip, Tone::Accent),
            Middle::Hints(hints),
        ))
    }

    /// 调整布局模式的键位提示；「Esc 完成」可点，退出模式。锁定布局时调尺寸 / 移动
    /// 都会被拒绝，这两条置灰（冒烟 L1）；切换面板、最大化不改布局，照常可用。
    fn arrange_hints(&self) -> StatusHints {
        use super::super::workbench::interaction::Action;
        let menu = &crate::i18n::texts().menu;
        let unlocked = !self.workbench.dock.locked;
        let hint = |key: &'static str, label: &'static str, enabled: bool| Hint {
            enabled,
            ..Hint::new(key, label)
        };
        let hints = vec![
            hint("Tab", menu.arrange_focus, true),
            hint("←↑↓→", menu.arrange_resize, unlocked),
            hint("Shift+←↑↓→", menu.arrange_move, unlocked),
            hint("Enter", menu.arrange_maximize, true),
            Hint {
                primary: true,
                ..Hint::new("Esc", menu.arrange_done)
            },
        ];
        let mut clickable = vec![None; hints.len() - 1];
        clickable.push(Some(Action::Arrange));
        (Middle::Hints(hints), clickable)
    }

    /// 终端模式的中部：紧凑视图 / 最大化 / 锁定 / 不接鼠标时是说明文字（教用户
    /// 怎么切换、还原或排布），其余时候是前缀键、会话导航、命令搜索与快捷键表的
    /// 键位提示（后三条接鼠标时可点）。
    fn terminal_status(&self, compact_switcher_shown: bool) -> StatusHints {
        use super::super::observability::tr;
        let mouse = self.config.mouse_capture;
        // 点名字切换只在接鼠标时成立；不接鼠标时保留键盘唯一的切换路径「调整布局
        // + Tab」（W1）。
        let text = if compact_switcher_shown && mouse {
            Some(tr(
                "Compact view · click a name above to switch panels",
                "紧凑视图 · 点上方的面板名切换面板",
            ))
        } else if self.workbench.geometry.compact {
            Some(tr(
                "Compact view · use Arrange layout / Tab to switch panels",
                "紧凑视图 · 在「调整布局」模式用 Tab 切换面板",
            ))
        } else if self.workbench.dock.maximized.is_some() {
            // 最大化只投影一个面板：没有别的面板可停靠、也没有分隔线可拖，改说怎么
            // 还原（N20）；不接鼠标时 `◫` 点不到，说键盘路径。锁定不拦还原（点 `◫`
            // 与调整布局的 Enter 都不查锁定），所以锁定时照样先说怎么还原、再注明
            // 布局已锁定（复审轻级 1）。
            Some(match (mouse, self.workbench.dock.locked) {
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
            })
        } else if self.workbench.dock.locked {
            // 锁定后拖动停靠 / 调尺寸都被拒绝：说明怎么解锁（冒烟 L1）；不接鼠标时
            // 标题栏点不到，指向主菜单里的同名开关（N20）。
            Some(if mouse {
                tr(
                    "Layout locked · turn off Lock layout in the ⋯ menu to rearrange",
                    "布局已锁定 · 在标题栏 ⋯ 菜单取消「锁定布局」后可重新排布",
                )
            } else {
                tr(
                    "Layout locked · turn off Lock layout in the main menu to rearrange",
                    "布局已锁定 · 在主菜单取消「锁定布局」后可重新排布",
                )
            })
        } else if !mouse {
            // 不接鼠标时拖不动（W1 同类，N20）：说键盘怎么排布——从主菜单进「调整
            // 布局」，键位与该模式的提示同名。
            Some(tr(
                "Arrange layout (main menu): Tab focus panel · ←↑↓→ resize · Shift+←↑↓→ move",
                "从主菜单进入「调整布局」：Tab 切换面板 · ←↑↓→ 调尺寸 · Shift+←↑↓→ 移动",
            ))
        } else {
            None
        };
        if let Some(text) = text {
            return (Middle::Text(Cow::Borrowed(text)), Vec::new());
        }
        use super::super::workbench::interaction::Action;
        use crate::input::KeybindAction;
        let t = &crate::i18n::texts().mode_bar;
        let keybinds = &self.config.keybinds;
        let mut hints = vec![Hint::new(keybinds.primary_prefix_label(), t.hint_prefix)];
        let mut clickable = vec![None];
        for (binding, label, action) in [
            (
                &keybinds.keybinds.goto,
                t.hint_navigator,
                KeybindAction::OpenNavigator,
            ),
            (
                &keybinds.keybinds.command_search,
                t.hint_commands,
                KeybindAction::CommandSearch,
            ),
            (&keybinds.keybinds.help, t.hint_keys, KeybindAction::Help),
        ] {
            if let Some(key) = super::key_sequence(keybinds, binding) {
                hints.push(Hint::new(key, label));
                clickable.push(Some(Action::Binding(action)));
            }
        }
        (Middle::Hints(hints), clickable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row_text(buffer: &Buffer, y: u16) -> String {
        (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol().to_owned())
            .collect()
    }

    fn view<'a>(
        mode: Option<(&'a str, Tone)>,
        middle: Middle<'a>,
        right: Vec<Span<'a>>,
    ) -> StatusBarView<'a> {
        StatusBarView {
            mode,
            broadcast: None,
            middle,
            hovered: None,
            right,
        }
    }

    #[test]
    fn mode_chip_tones_separate_terminal_from_the_loud_modes() {
        let palette = Palette::graphite_dark();
        let components = ComponentStyles::from_palette(&palette);
        let bg = super::super::bar_surface(&palette);
        let area = Rect::new(0, 0, 60, 1);
        for (tone, loud) in [(Tone::Subtle, false), (Tone::Accent, true)] {
            let mut buffer = Buffer::empty(area);
            paint(
                &mut buffer,
                area,
                &view(Some((" 终端 ", tone)), Middle::Empty, Vec::new()),
                bg,
                &palette,
                &components,
            );
            let cell = &buffer[(1, 0)];
            assert_eq!(cell.symbol(), "终");
            assert_eq!(cell.bg == components.mode_bar_accent, loud, "{tone:?}");
            assert_ne!(cell.bg, bg, "{tone:?}：标签有自己的底色");
        }
    }

    #[test]
    fn right_segments_are_right_aligned_and_yield_to_the_middle() {
        let palette = Palette::graphite_dark();
        let components = ComponentStyles::from_palette(&palette);
        let bg = super::super::bar_surface(&palette);
        let right = vec![Span::new(
            "devbox 14:02",
            Style::default().fg(palette.overlay1),
        )];
        let area = Rect::new(0, 0, 60, 1);
        let mut buffer = Buffer::empty(area);
        paint(
            &mut buffer,
            area,
            &view(
                Some((" 终端 ", Tone::Subtle)),
                Middle::Text("说明".into()),
                right.clone(),
            ),
            bg,
            &palette,
            &components,
        );
        let text = row_text(&buffer, 0);
        assert!(text.trim_end().ends_with("devbox 14:02"), "{text:?}");
        assert_eq!(buffer[(59, 0)].symbol(), " ", "离右缘 1 列");
        // 窄到中部不够最小宽度：状态段让位，说明照画。
        let area = Rect::new(0, 0, 30, 1);
        let mut buffer = Buffer::empty(area);
        paint(
            &mut buffer,
            area,
            &view(
                Some((" 终端 ", Tone::Subtle)),
                Middle::Text("已最大化 · 点 ◫ 还原布局".into()),
                right,
            ),
            bg,
            &palette,
            &components,
        );
        let text = row_text(&buffer, 0);
        assert!(!text.contains("devbox"), "{text:?}");
        assert!(text.contains('已'), "{text:?}");
    }

    /// 终端模式的四条键位提示与右侧状态段在任何宽度都不相撞：提示放不下就整条丢，
    /// 与状态段至少隔 2 列；状态段放不下就让位。键帽是低一档的栏内键帽，标签元信息色。
    #[test]
    fn terminal_hints_and_right_segments_never_collide() {
        let palette = Palette::graphite_dark();
        let components = ComponentStyles::from_palette(&palette);
        let bg = super::super::bar_surface(&palette);
        let hints = || {
            Middle::Hints(vec![
                Hint::new("ctrl+b", "前缀"),
                Hint::new("ctrl+b g", "会话导航"),
                Hint::new("ctrl+b /", "命令"),
                Hint::new("ctrl+b ?", "快捷键"),
            ])
        };
        let status = "devbox 14:02";
        let right = vec![Span::new(status, Style::default().fg(palette.overlay1))];
        for width in [40u16, 60, 72, 80, 100, 160] {
            let area = Rect::new(0, 0, width, 1);
            let mut buffer = Buffer::empty(area);
            let hits = paint(
                &mut buffer,
                area,
                &view(Some((" 终端 ", Tone::Subtle)), hints(), right.clone()),
                bg,
                &palette,
                &components,
            );
            let text = row_text(&buffer, 0);
            let hints_end = hits
                .hints
                .iter()
                .map(|(rect, _)| rect.right())
                .max()
                .unwrap_or(0);
            assert!(!hits.hints.is_empty(), "{width} 列：至少画出一条提示");
            if text.contains("devbox") {
                let start = width - 1 - display_width_u16(status);
                assert!(
                    hints_end + 2 <= start,
                    "{width} 列：提示与右侧状态段至少隔 2 列：{text:?}"
                );
                assert!(text.trim_end().ends_with(status), "{width} 列：{text:?}");
            } else {
                assert!(hints_end < width, "{width} 列：{text:?}");
            }
        }

        let area = Rect::new(0, 0, 80, 1);
        let mut buffer = Buffer::empty(area);
        let hits = paint(
            &mut buffer,
            area,
            &view(Some((" 终端 ", Tone::Subtle)), hints(), right),
            bg,
            &palette,
            &components,
        );
        let (first, _) = hits.hints[0];
        let cap = super::super::bar_key_cap(&palette, bg);
        let key = &buffer[(first.x + 1, 0)];
        assert_eq!(key.symbol(), "c");
        assert_eq!(Some(key.bg), cap.bg, "栏内键帽的底色");
        assert_eq!(Some(key.fg), cap.fg);
        let page = crate::ui::design::key_cap(&palette);
        assert_eq!(Some(key.bg), page.bg, "与页面页脚同一套键帽");
        assert_eq!(Some(key.fg), page.fg);
        let label = &buffer[(first.x + 9, 0)];
        assert_eq!(label.symbol(), "前");
        assert_eq!(
            Some(label.fg),
            super::super::bar_hint_label(&palette, bg).fg,
            "标签说明色"
        );
        assert_eq!(Some(label.fg), crate::ui::design::key_label(&palette).fg);
    }

    #[test]
    fn subtle_chip_yields_when_the_explanation_would_be_cut() {
        let palette = Palette::graphite_dark();
        let components = ComponentStyles::from_palette(&palette);
        let bg = super::super::bar_surface(&palette);
        let area = Rect::new(0, 0, 30, 1);
        let paint_text = |chip: (&str, Tone), text: &str| {
            let mut buffer = Buffer::empty(area);
            paint(
                &mut buffer,
                area,
                &view(Some(chip), Middle::Text(text.to_owned().into()), Vec::new()),
                bg,
                &palette,
                &components,
            );
            buffer
        };
        // 带 6 列的标签，中部从第 7 列起、到右缘前 1 列：22 列。
        let fits = "x".repeat(22);
        let buffer = paint_text((" 终端 ", Tone::Subtle), &fits);
        assert_eq!(buffer[(1, 0)].symbol(), "终", "放得下：标签照画");
        assert_eq!(buffer[(7, 0)].symbol(), "x");
        // 多 1 列就放不下：低调标签让位，说明从第 0 列完整画出。
        let long = "x".repeat(23);
        let buffer = paint_text((" 终端 ", Tone::Subtle), &long);
        assert_eq!(row_text(&buffer, 0).trim_end(), long);
        // accent 标签（前缀等模式）从不让位，说明截断。
        let buffer = paint_text((" 前缀 ", Tone::Accent), &long);
        assert_eq!(buffer[(1, 0)].symbol(), "前");
        assert!(row_text(&buffer, 0).trim_end().ends_with('…'));
    }

    #[test]
    fn hints_report_hit_rects_and_errors_paint_red() {
        let palette = Palette::graphite_dark();
        let components = ComponentStyles::from_palette(&palette);
        let bg = super::super::bar_surface(&palette);
        let area = Rect::new(0, 0, 80, 1);
        let mut buffer = Buffer::empty(area);
        let hits = paint(
            &mut buffer,
            area,
            &view(
                Some((" 终端 ", Tone::Subtle)),
                Middle::Hints(vec![
                    Hint::new("ctrl+b", "前缀"),
                    Hint::new("ctrl+b /", "命令"),
                ]),
                Vec::new(),
            ),
            bg,
            &palette,
            &components,
        );
        assert_eq!(hits.hints.len(), 2);
        assert_eq!(hits.hints[0].0.x, 7, "标签后空 1 列");
        let mut buffer = Buffer::empty(area);
        paint(
            &mut buffer,
            area,
            &view(
                Some((" 错误 ", Tone::Error)),
                Middle::Error("连接断开".into()),
                Vec::new(),
            ),
            bg,
            &palette,
            &components,
        );
        assert_eq!(buffer[(1, 0)].bg, palette.red);
        assert_eq!(buffer[(7, 0)].fg, palette.red);
    }
}
