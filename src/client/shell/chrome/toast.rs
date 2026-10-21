//! Graphite 吐司：语义通知、端点通知与复制反馈共用的卡片。
//!
//! 三段纯函数：[`measure`]（视图计算：按显示宽度折正文、算宽高）、[`place`]（给定
//! 安全区、锚点与堆叠偏移算矩形）、[`paint`]（只读入参作画，返回卡片与「按键」
//! 动作的命中区）。描边用等级色——需要输入/告警琥珀、失败红、完成绿、其余蓝；
//! 聚焦或悬浮只加粗边框并给标题行换中性底色，不把边框改成 accent。

use super::super::feedback::{ChromeContext, ClientToastLevel};
use super::super::render::put_text;
use super::settings::truncate_with_ellipsis;
use crate::ui::display_width_u16;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use std::borrow::Cow;

/// 卡片的首选宽度（窄屏收缩到安全区内）。
pub(in crate::client::shell) const WIDTH: u16 = 46;
/// 卡片与安全区左右边界的间距。
pub(in crate::client::shell) const MARGIN: u16 = 2;
/// 动作项之间的空格数。
const ACTION_GAP: u16 = 3;
/// 边框 + 左右各一格内边距。
const CHROME_WIDTH: u16 = 4;

/// 吐司锚点：herdr 通知用四角，复制反馈另有上下居中。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::client::shell) enum Anchor {
    TopLeft,
    TopCenter,
    TopRight,
    BottomLeft,
    BottomCenter,
    BottomRight,
}

impl Anchor {
    pub(in crate::client::shell) fn herdr(position: crate::config::ToastHerdrPosition) -> Self {
        use crate::config::ToastHerdrPosition as P;
        match position {
            P::TopLeft => Self::TopLeft,
            P::TopRight => Self::TopRight,
            P::BottomLeft => Self::BottomLeft,
            P::BottomRight => Self::BottomRight,
        }
    }

    pub(in crate::client::shell) fn clipboard(
        position: crate::config::ToastClipboardPosition,
    ) -> Self {
        use crate::config::ToastClipboardPosition as P;
        match position {
            P::TopLeft => Self::TopLeft,
            P::TopCenter => Self::TopCenter,
            P::TopRight => Self::TopRight,
            P::BottomLeft => Self::BottomLeft,
            P::BottomCenter => Self::BottomCenter,
            P::BottomRight => Self::BottomRight,
        }
    }

    pub(in crate::client::shell) fn is_top(self) -> bool {
        matches!(self, Self::TopLeft | Self::TopCenter | Self::TopRight)
    }
}

/// 末行动作：`Hint` 是实际处理的按键（键帽 + 说明）；`Focus` 是可点的「按键」
/// 按钮，点它把键盘焦点交给卡片（之后 enter / esc 才归卡片）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::client::shell) enum ActionKind {
    Hint,
    Focus,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::client::shell) struct Action {
    pub key: String,
    pub label: String,
    pub kind: ActionKind,
}

impl Action {
    fn width(&self) -> u16 {
        match self.kind {
            ActionKind::Hint => crate::ui::kit::key_cap::hint_width(&self.key, &self.label),
            ActionKind::Focus => {
                let cap = display_width_u16(&self.key).saturating_add(2);
                if self.label.is_empty() {
                    cap
                } else {
                    cap + 1 + display_width_u16(&self.label)
                }
            }
        }
    }
}

/// 吐司的视图模型（调用方在合成阶段组装，绘制只读）。
pub(in crate::client::shell) struct Toast<'a> {
    pub level: ClientToastLevel,
    /// 首列图标；语义通知按状态指示风格取状态字形，其余用等级图标。
    pub icon: Cow<'a, str>,
    /// agent 名（粗体）；没有时标题独占首行。
    pub agent: Option<Cow<'a, str>>,
    pub title: Cow<'a, str>,
    /// 右上角的紧凑年龄（`now` / `3m`）。
    pub age: Option<Cow<'a, str>>,
    pub location: Option<Cow<'a, str>>,
    pub body: Cow<'a, str>,
    pub actions: Vec<Action>,
    /// 聚焦或悬浮：加粗边框、标题行换中性底色。
    pub emphasized: bool,
    /// 宽度按内容收缩（复制反馈），否则取 [`WIDTH`]。
    pub fit: bool,
}

/// [`measure`] 的结果：卡片尺寸与折好的正文行。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::client::shell) struct Measured {
    pub width: u16,
    pub height: u16,
    pub body: Vec<String>,
}

fn title_width(toast: &Toast<'_>) -> u16 {
    let mut width = display_width_u16(&toast.icon).saturating_add(1);
    if let Some(agent) = toast.agent.as_deref() {
        width = width.saturating_add(display_width_u16(agent));
        if !toast.title.is_empty() {
            width = width.saturating_add(1);
        }
    }
    width.saturating_add(display_width_u16(&toast.title))
}

/// 视图计算：在安全区宽度内定卡片宽度、按显示宽度折正文（最多两行）、算高度。
pub(in crate::client::shell) fn measure(toast: &Toast<'_>, safe: Rect) -> Measured {
    let title = title_width(toast);
    let age = toast
        .age
        .as_deref()
        .map_or(0, |age| display_width_u16(age).saturating_add(2));
    let preferred = if toast.fit {
        let actions = toast
            .actions
            .iter()
            .map(Action::width)
            .fold(0u16, |sum, width| sum.saturating_add(width + ACTION_GAP));
        title
            .saturating_add(age)
            .max(display_width_u16(&toast.body).min(WIDTH))
            .max(actions.saturating_sub(ACTION_GAP))
            .saturating_add(CHROME_WIDTH)
            .min(WIDTH)
    } else {
        WIDTH
    };
    // 窄屏：先收掉两侧间距，仍不够时至少放下标题（不超过安全区）。
    let room = safe.width.saturating_sub(MARGIN * 2);
    let minimum = title.saturating_add(CHROME_WIDTH).min(preferred);
    let width = preferred.min(room).max(minimum).min(safe.width);
    let content = width.saturating_sub(CHROME_WIDTH);
    let body = super::notification::body_lines(&toast.body, content);
    let rows =
        1 + u16::from(
            toast
                .location
                .as_deref()
                .is_some_and(|location| !location.is_empty()),
        ) + body.len() as u16
            + u16::from(!toast.actions.is_empty());
    Measured {
        width,
        height: rows.saturating_add(2).min(safe.height),
        body,
    }
}

/// 安全区内的卡片矩形。右侧 `x = 右边界 − 宽 − 2`，左侧 `x = 左边界 + 2`；底部贴安全区
/// 底边，顶部贴安全区顶边。`offset` 是向安全区内部的堆叠偏移（行）。
pub(in crate::client::shell) fn place(
    safe: Rect,
    anchor: Anchor,
    width: u16,
    height: u16,
    offset: u16,
) -> Rect {
    if safe.is_empty() || width == 0 || height == 0 {
        return Rect::default();
    }
    let width = width.min(safe.width);
    let height = height.min(safe.height);
    let max_x = safe.right() - width;
    let x = match anchor {
        Anchor::TopLeft | Anchor::BottomLeft => safe.x.saturating_add(MARGIN),
        Anchor::TopCenter | Anchor::BottomCenter => safe.x + (safe.width - width) / 2,
        Anchor::TopRight | Anchor::BottomRight => max_x.saturating_sub(MARGIN),
    }
    .clamp(safe.x, max_x);
    let max_y = safe.bottom() - height;
    let y = if anchor.is_top() {
        safe.y.saturating_add(offset)
    } else {
        max_y.saturating_sub(offset)
    }
    .clamp(safe.y, max_y);
    Rect::new(x, y, width, height)
}

/// 不同锚点也可能横向相交；沿锚点向内寻找空位，放不下则本帧不画，保留通知状态。
pub(in crate::client::shell) fn place_avoiding(
    safe: Rect,
    anchor: Anchor,
    width: u16,
    height: u16,
    offset: u16,
    occupied: &[Rect],
) -> Rect {
    for offset in offset..=safe.height.saturating_sub(height) {
        let rect = place(safe, anchor, width, height, offset);
        if !occupied.iter().any(|other| other.intersects(rect)) {
            return rect;
        }
    }
    Rect::default()
}

/// 绘制结果：卡片矩形与「按键」按钮（键盘焦点入口，没有时为空）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::client::shell) struct Painted {
    pub card: Rect,
    pub focus: Rect,
}

/// 在 `rect`（[`place`] 的结果）画一张吐司；只读入参。
pub(in crate::client::shell) fn paint(
    buffer: &mut Buffer,
    rect: Rect,
    toast: &Toast<'_>,
    measured: &Measured,
    cx: &ChromeContext<'_>,
) -> Painted {
    use ratatui::widgets::{Block, Borders, Clear, Widget};
    if rect.is_empty() {
        return Painted::default();
    }
    let p = cx.palette;
    let color = toast.level.color(cx.components);
    let bg = p.panel_bg;
    let mut border = Style::default().fg(color);
    if toast.emphasized {
        border = border.add_modifier(Modifier::BOLD);
    }
    Clear.render(rect, buffer);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(crate::ui::BorderGlyphs::ROUNDED.border_set())
        .border_style(border)
        .style(Style::default().bg(bg).remove_modifier(Modifier::all()));
    let inner = block.inner(rect);
    block.render(rect, buffer);
    let mut painted = Painted {
        card: rect,
        focus: Rect::default(),
    };
    if inner.is_empty() {
        return painted;
    }
    let content = Rect::new(
        inner.x.saturating_add(1),
        inner.y,
        inner.width.saturating_sub(2),
        inner.height,
    );
    if content.width == 0 {
        return painted;
    }
    // 首行：图标 + agent（粗体）+ 标题，右端年龄。
    let row_bg = if toast.emphasized { p.surface0 } else { bg };
    if toast.emphasized {
        buffer.set_style(
            Rect::new(inner.x, inner.y, inner.width, 1),
            Style::default().bg(row_bg),
        );
    }
    let age_width = toast
        .age
        .as_deref()
        .map_or(0, display_width_u16)
        .min(content.width / 3);
    let title_right = if age_width > 0 {
        content.right().saturating_sub(age_width + 1)
    } else {
        content.right()
    };
    let mut x = content.x;
    let y = content.y;
    let icon_style = Style::default()
        .fg(color)
        .bg(row_bg)
        .add_modifier(Modifier::BOLD);
    let segments = [
        (toast.icon.as_ref(), icon_style),
        (" ", Style::default().bg(row_bg)),
    ];
    for (text, style) in segments {
        x = put_segment(buffer, x, y, title_right, text, style);
    }
    if let Some(agent) = toast.agent.as_deref() {
        x = put_segment(
            buffer,
            x,
            y,
            title_right,
            &truncate_with_ellipsis(agent, title_right.saturating_sub(x)),
            crate::ui::design::title(p).bg(row_bg),
        );
        if !toast.title.is_empty() {
            x = put_segment(buffer, x, y, title_right, " ", Style::default().bg(row_bg));
        }
    }
    let title_style = if toast.agent.is_some() {
        crate::ui::design::body(p)
    } else {
        crate::ui::design::title(p)
    };
    put_segment(
        buffer,
        x,
        y,
        title_right,
        &truncate_with_ellipsis(&toast.title, title_right.saturating_sub(x)),
        title_style.bg(row_bg),
    );
    if let Some(age) = toast.age.as_deref().filter(|_| age_width > 0) {
        put_text(
            buffer,
            content.right() - age_width,
            y,
            age_width,
            &truncate_with_ellipsis(age, age_width),
            crate::ui::design::meta(p).bg(row_bg),
        );
    }
    let footer = if toast.actions.is_empty() {
        inner.bottom()
    } else {
        inner.bottom() - 1
    };
    let mut y = content.y + 1;
    if let Some(location) = toast
        .location
        .as_deref()
        .filter(|location| !location.is_empty())
    {
        if y < footer {
            put_text(
                buffer,
                content.x,
                y,
                content.width,
                &truncate_with_ellipsis(location, content.width),
                crate::ui::design::meta(p).bg(bg),
            );
            y += 1;
        }
    }
    for line in &measured.body {
        if y >= footer {
            break;
        }
        put_text(
            buffer,
            content.x,
            y,
            content.width,
            line,
            crate::ui::design::muted(p).bg(bg),
        );
        y += 1;
    }
    if toast.actions.is_empty() || inner.height < 2 {
        return painted;
    }
    // 末行动作：放不下的整项不画（第一项除外，按右界裁）。
    let y = inner.bottom() - 1;
    let right = content.right();
    let mut x = content.x;
    let cap = crate::ui::design::key_cap(p);
    let label = crate::ui::design::key_label(p).bg(bg);
    for (index, action) in toast.actions.iter().enumerate() {
        if index > 0 {
            if x.saturating_add(ACTION_GAP).saturating_add(action.width()) > right {
                break;
            }
            x += ACTION_GAP;
        }
        let start = x;
        x = match action.kind {
            ActionKind::Hint => crate::ui::kit::key_cap::paint_hint(
                buffer,
                x,
                y,
                right,
                &action.key,
                &action.label,
                cap,
                label,
            ),
            ActionKind::Focus => {
                let text = format!(" {} ", action.key);
                let end = put_segment(buffer, x, y, right, &text, cap);
                let end = if action.label.is_empty() || end >= right {
                    end
                } else {
                    let end = put_segment(buffer, end, y, right, " ", label);
                    put_segment(buffer, end, y, right, &action.label, label)
                };
                painted.focus = Rect::new(start, y, end - start, 1);
                end
            }
        };
        if x >= right {
            break;
        }
    }
    painted
}

/// 从 `x` 起写一段文字，不越过 `right`；返回写到的列（宽字符不劈开）。
fn put_segment(buffer: &mut Buffer, x: u16, y: u16, right: u16, text: &str, style: Style) -> u16 {
    super::super::render::put_segment(buffer, x, y, right, text, style)
}

/// 吐司右上角的年龄：一分钟内写 `now`，之后按分钟取整（不逐秒重绘）。
pub(in crate::client::shell) fn age_label(elapsed: std::time::Duration) -> String {
    let seconds = elapsed.as_secs();
    if seconds < 60 {
        "now".to_owned()
    } else {
        crate::ui::design::compact_age(seconds.saturating_mul(1_000))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::state::Palette;

    fn cx<'a>(
        palette: &'a Palette,
        components: &'a crate::app::state::ComponentStyles,
    ) -> ChromeContext<'a> {
        ChromeContext {
            page_bounds: None,
            palette,
            components,
            glyphs: crate::ui::BorderGlyphs::ROUNDED,
            hover: None,
            spinner: "◌",
            now: std::time::Instant::now(),
        }
    }

    fn toast(level: ClientToastLevel, focused: bool) -> Toast<'static> {
        Toast {
            level,
            icon: level.icon().into(),
            agent: Some("codex".into()),
            title: "需要你的输入".into(),
            age: Some("now".into()),
            location: None,
            body: "请确认是否修改 src/client/shell/state.rs，当前终端等待权限输入。".into(),
            actions: if focused {
                vec![
                    Action {
                        key: "enter".into(),
                        label: "跳转并回答".into(),
                        kind: ActionKind::Hint,
                    },
                    Action {
                        key: "esc".into(),
                        label: "忽略".into(),
                        kind: ActionKind::Hint,
                    },
                ]
            } else {
                vec![Action {
                    key: "按键".into(),
                    label: "启用 enter/esc".into(),
                    kind: ActionKind::Focus,
                }]
            },
            emphasized: focused,
            fit: false,
        }
    }

    #[test]
    fn placement_respects_the_safe_area_margins_and_corners() {
        let safe = Rect::new(0, 1, 160, 43);
        let right = place(safe, Anchor::BottomRight, WIDTH, 6, 0);
        assert_eq!(right, Rect::new(160 - WIDTH - 2, 44 - 6, WIDTH, 6));
        let left = place(safe, Anchor::TopLeft, WIDTH, 6, 0);
        assert_eq!((left.x, left.y), (2, 1));
        let stacked = place(safe, Anchor::BottomRight, WIDTH, 5, 6);
        assert_eq!(stacked.bottom(), right.y);
        let top = place(safe, Anchor::TopRight, WIDTH, 5, 3);
        assert_eq!(top.y, 4);
        // 偏移超出安全区时夹回安全区内。
        let clamped = place(safe, Anchor::BottomLeft, WIDTH, 5, 100);
        assert_eq!(clamped.y, safe.y);
    }

    #[test]
    fn narrow_safe_areas_shrink_the_card_but_keep_the_title() {
        let level = ClientToastLevel::Warning;
        let measured = measure(&toast(level, true), Rect::new(0, 1, 60, 18));
        assert_eq!(measured.width, WIDTH);
        assert_eq!(measured.body.len(), 2);
        assert_eq!(measured.height, 2 + 1 + 2 + 1);
        let narrow = measure(&toast(level, true), Rect::new(0, 1, 30, 18));
        assert_eq!(narrow.width, 26);
        assert!(narrow.body.iter().all(|line| display_width_u16(line) <= 22));
        // 安全区比标题还窄：贴满安全区，不越界。
        let tiny = measure(&toast(level, true), Rect::new(0, 0, 12, 4));
        assert_eq!(tiny.width, 12);
        assert_eq!(tiny.height, 4);
    }

    #[test]
    fn level_colors_the_border_and_icon_and_focus_hit_matches_the_paint() {
        let palette = Palette::graphite_dark();
        let components = crate::app::state::ComponentStyles::from_palette(&palette);
        let cx = cx(&palette, &components);
        for (level, color) in [
            (ClientToastLevel::Warning, components.status_input),
            (ClientToastLevel::Error, components.toast_border_error),
            (ClientToastLevel::Success, components.toast_border_success),
            (ClientToastLevel::Info, components.toast_border_info),
        ] {
            let safe = Rect::new(0, 1, 80, 22);
            let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 24));
            let card = toast(level, false);
            let measured = measure(&card, safe);
            let rect = place(
                safe,
                Anchor::BottomRight,
                measured.width,
                measured.height,
                0,
            );
            let painted = paint(&mut buffer, rect, &card, &measured, &cx);
            assert_eq!(painted.card, rect);
            assert_eq!(buffer[(rect.x, rect.y)].symbol(), "╭");
            assert_eq!(buffer[(rect.x, rect.y)].fg, color);
            assert_eq!(buffer[(rect.x + 2, rect.y + 1)].symbol(), level.icon());
            assert_eq!(buffer[(rect.x + 2, rect.y + 1)].fg, color);
            // 年龄靠右、说明色。
            let age_x = rect.right() - 2 - 3;
            assert_eq!(buffer[(age_x, rect.y + 1)].symbol(), "n");
            assert_eq!(buffer[(age_x, rect.y + 1)].fg, palette.overlay1);
            // 正文说明色。
            assert_eq!(buffer[(rect.x + 2, rect.y + 2)].fg, palette.subtext0);
            // 「按键」按钮：命中区就是画出的键帽 + 标签。
            let focus = painted.focus;
            assert_eq!(focus.y, rect.bottom() - 2);
            assert_eq!(focus.x, rect.x + 2);
            let text: String = (focus.x..focus.right())
                .map(|x| buffer[(x, focus.y)].symbol())
                .collect();
            assert!(text.starts_with(" 按"), "{text}");
            assert_eq!(buffer[(focus.x, focus.y)].bg, palette.surface1);
        }
    }

    #[test]
    fn focus_emphasis_keeps_the_level_border() {
        let palette = Palette::graphite_light();
        let components = crate::app::state::ComponentStyles::from_palette(&palette);
        let cx = cx(&palette, &components);
        let safe = Rect::new(0, 1, 80, 22);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 24));
        let card = toast(ClientToastLevel::Error, true);
        let measured = measure(&card, safe);
        let rect = place(safe, Anchor::TopLeft, measured.width, measured.height, 0);
        let painted = paint(&mut buffer, rect, &card, &measured, &cx);
        assert_eq!(buffer[(rect.x, rect.y)].fg, components.toast_border_error);
        assert!(buffer[(rect.x, rect.y)].modifier.contains(Modifier::BOLD));
        assert_eq!(buffer[(rect.x + 1, rect.y + 1)].bg, palette.surface0);
        assert!(painted.focus.is_empty());
        let footer: String = (rect.x..rect.right())
            .map(|x| buffer[(x, rect.bottom() - 2)].symbol())
            .collect();
        assert!(footer.contains("enter"), "{footer}");
        assert!(footer.contains("esc"), "{footer}");
    }

    #[test]
    fn ages_round_to_minutes() {
        use std::time::Duration;
        assert_eq!(age_label(Duration::from_secs(12)), "now");
        assert_eq!(age_label(Duration::from_secs(185)), "3m");
    }
}
