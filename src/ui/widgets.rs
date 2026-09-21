use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
};

use crate::app::state::Palette;

pub(super) fn panel_contrast_fg(palette: &Palette) -> Color {
    match palette.panel_bg {
        Color::Reset => palette.surface_dim,
        color => color,
    }
}

/// Named modal size tiers. Overlays pick a tier instead of hardcoding cell
/// sizes; `Content` carries genuinely content-derived dimensions (dynamic
/// heights, exported constants mirrored by the input side).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModalSize {
    /// Compact prompt dialogs (rename).
    Small,
    /// Mid-size info dialogs (onboarding).
    Medium,
    /// Large browsing dialogs (help, settings).
    Large,
    /// Widest reading dialogs (product announcement).
    XLarge,
    /// Explicit cell size for content-shaped dialogs.
    Content { width: u16, height: u16 },
}

impl ModalSize {
    pub(crate) const fn cells(self) -> (u16, u16) {
        match self {
            Self::Small => (56, 7),
            Self::Medium => (64, 16),
            Self::Large => (76, 22),
            Self::XLarge => (88, 24),
            Self::Content { width, height } => (width, height),
        }
    }

    /// Same tier width with a content-derived height.
    pub(crate) const fn with_height(self, height: u16) -> Self {
        let (width, _) = self.cells();
        Self::Content { width, height }
    }
}

/// The one modal geometry entry point: center `size` inside `area` with the
/// shared modal margin/minimum rule (`None` below 4x4).
pub(crate) fn modal_rect(area: Rect, size: ModalSize) -> Option<Rect> {
    let (width, height) = size.cells();
    centered_popup_rect(area, width, height)
}

pub(crate) fn centered_popup_rect(area: Rect, popup_width: u16, popup_height: u16) -> Option<Rect> {
    crate::popup_size::centered_rect(
        area,
        popup_width,
        popup_height,
        crate::popup_size::MODAL_CENTER_RULE,
    )
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ModalStackAreas {
    pub header: Rect,
    pub content: Rect,
    pub footer: Option<Rect>,
    pub actions: Option<Rect>,
}

pub(crate) fn modal_stack_areas(
    inner: Rect,
    header_height: u16,
    footer_height: u16,
    actions_height: u16,
    gap: u16,
) -> ModalStackAreas {
    #[derive(Clone, Copy)]
    enum Slot {
        Header,
        Content,
        Footer,
        Actions,
    }

    let mut constraints = Vec::new();
    let mut slots = Vec::new();
    let mut push = |slot: Slot, constraint: Constraint| {
        if !slots.is_empty() {
            constraints.push(Constraint::Length(gap));
        }
        constraints.push(constraint);
        slots.push(slot);
    };

    push(Slot::Header, Constraint::Length(header_height));
    push(Slot::Content, Constraint::Min(0));
    if footer_height > 0 {
        push(Slot::Footer, Constraint::Length(footer_height));
    }
    if actions_height > 0 {
        push(Slot::Actions, Constraint::Length(actions_height));
    }

    let areas = Layout::vertical(constraints).split(inner);
    let mut result = ModalStackAreas {
        header: Rect::default(),
        content: Rect::default(),
        footer: None,
        actions: None,
    };
    for (slot, area) in slots.into_iter().zip(areas.iter().step_by(2).copied()) {
        match slot {
            Slot::Header => result.header = area,
            Slot::Content => result.content = area,
            Slot::Footer => result.footer = Some(area),
            Slot::Actions => result.actions = Some(area),
        }
    }
    result
}

/// Button color role: accent primary, destructive primary, or neutral
/// secondary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModalButtonTone {
    Primary,
    Danger,
    Secondary,
}

/// Interaction state of a modal button. Renderers that cannot observe hover
/// pass `Focused` for the primary action and `Normal` otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModalButtonState {
    Focused,
    Hovered,
    Normal,
    Disabled,
}

/// Three-state modal button style: focused/hovered actions carry the tone
/// color, normal secondaries sit on `surface0`, disabled buttons dim down.
/// The DIM removal keeps buttons readable over the dimmed backdrop that
/// modal overlays paint first.
pub(crate) fn modal_button_style(
    palette: &Palette,
    tone: ModalButtonTone,
    state: ModalButtonState,
) -> Style {
    let base = Style::default().remove_modifier(Modifier::DIM);
    match state {
        ModalButtonState::Focused | ModalButtonState::Hovered => {
            let (bg, fg) = match tone {
                ModalButtonTone::Primary => (palette.accent, panel_contrast_fg(palette)),
                ModalButtonTone::Danger => (palette.red, panel_contrast_fg(palette)),
                // 次级按钮的焦点/悬浮同样取 accent 底：这是有意的「安全默认项
                // 也要显眼」语义（强制删除确认页把取消按钮画成强调项）。
                ModalButtonTone::Secondary => (palette.accent, panel_contrast_fg(palette)),
            };
            base.fg(fg).bg(bg).add_modifier(Modifier::BOLD)
        }
        ModalButtonState::Normal => {
            let (bg, fg) = match tone {
                ModalButtonTone::Secondary => (palette.surface0, palette.text),
                ModalButtonTone::Primary => (palette.accent, panel_contrast_fg(palette)),
                ModalButtonTone::Danger => (palette.red, panel_contrast_fg(palette)),
            };
            // 常态不加粗：Primary/Danger 的悬浮与焦点因此有可见反馈
            // （HERDR-UX-02 之前两态逐字段相同）。
            base.fg(fg).bg(bg)
        }
        ModalButtonState::Disabled => base.fg(palette.overlay0).bg(palette.surface0),
    }
}

/// 输入框底色：结构性的「弱面板」token。主题可以把它留成 `Color::Reset`
/// （= 终端默认背景，16 色与手写调色板都可能这样），那时输入框与四周面板
/// 同像素，既没有边界也没有焦点提示（C-28 / ds-14）。回退按「弱 → 强」试
/// 其余结构面 token。
pub(crate) fn input_field_bg(palette: &Palette) -> Color {
    for candidate in [palette.surface0, palette.surface_dim, palette.surface1] {
        if candidate != Color::Reset {
            return candidate;
        }
    }
    Color::Reset
}

/// 唯一的文本输入框样式：浮层输入框、过滤框与表单行都从这里取样式，字段边界
/// 与光标底色因此保持一致。连回退结构面都未定义时用下划线划出输入区——没有
/// 颜色可用时，文字属性是最后的边界。
pub(crate) fn input_field_style(palette: &Palette) -> Style {
    let background = input_field_bg(palette);
    let base = Style::default()
        .fg(palette.text)
        .bg(background)
        .remove_modifier(Modifier::DIM);
    if background == Color::Reset {
        base.add_modifier(Modifier::UNDERLINED)
    } else {
        base
    }
}

/// Button width follows the i18n label display width (CJK safe); labels
/// carry their own padding so the rect hugs the text exactly.
pub(crate) fn modal_button_width(label: &str) -> u16 {
    u16::try_from(unicode_width::UnicodeWidthStr::width(label)).unwrap_or(u16::MAX)
}

/// Modal action buttons are rendered from these exact strings; the matching
/// `*_button_rect` hit-test rects must stay width-twins with them. Both sides
/// read the same i18n entry so translated labels keep the rects in sync.
pub(crate) fn modal_close_button_text() -> &'static str {
    crate::i18n::texts().chrome.close_button
}

pub(crate) fn modal_continue_button_text() -> &'static str {
    crate::i18n::texts().chrome.continue_button
}

pub(crate) fn close_button_rect(area: Rect) -> Rect {
    let width = modal_button_width(modal_close_button_text());
    Rect::new(area.x + area.width.saturating_sub(width), area.y, width, 1)
}

pub(crate) fn continue_button_rect(area: Rect) -> Rect {
    let width = modal_button_width(modal_continue_button_text());
    Rect::new(area.x, area.y, width, 1)
}

#[cfg(test)]
mod button_state_tests {
    use super::*;

    /// HERDR-UX-02：Primary/Danger 的常态与悬浮/焦点必须看得出差别，次级按钮
    /// 也不能长得跟 Primary 一样。
    #[test]
    fn modal_button_states_are_visually_distinct() {
        let palette = Palette::catppuccin();
        for tone in [
            ModalButtonTone::Primary,
            ModalButtonTone::Danger,
            ModalButtonTone::Secondary,
        ] {
            let normal = modal_button_style(&palette, tone, ModalButtonState::Normal);
            let focused = modal_button_style(&palette, tone, ModalButtonState::Focused);
            let hovered = modal_button_style(&palette, tone, ModalButtonState::Hovered);
            assert_ne!(normal, focused, "{tone:?} 常态与焦点同形");
            assert_eq!(focused, hovered, "{tone:?} 焦点与悬浮同一口径");
        }
        // 常态之间的区别也要在：次级是弱表面，主/危险是实心色块。
        let primary_normal =
            modal_button_style(&palette, ModalButtonTone::Primary, ModalButtonState::Normal);
        let secondary_normal = modal_button_style(
            &palette,
            ModalButtonTone::Secondary,
            ModalButtonState::Normal,
        );
        assert_ne!(
            primary_normal, secondary_normal,
            "常态下三种 tone 必须可区分"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modal_size_tiers_resolve_to_cells() {
        assert_eq!(ModalSize::Small.cells(), (56, 7));
        assert_eq!(ModalSize::Medium.cells(), (64, 16));
        assert_eq!(ModalSize::Large.cells(), (76, 22));
        assert_eq!(ModalSize::XLarge.cells(), (88, 24));
        assert_eq!(
            ModalSize::Content {
                width: 80,
                height: 24
            }
            .cells(),
            (80, 24)
        );
        assert_eq!(ModalSize::Large.with_height(30).cells(), (76, 30));
    }

    #[test]
    fn modal_rect_centers_with_modal_rule() {
        let rect = modal_rect(Rect::new(0, 0, 106, 30), ModalSize::Medium).unwrap();
        assert_eq!(rect, Rect::new(21, 7, 64, 16));
        assert!(modal_rect(Rect::new(0, 0, 8, 5), ModalSize::Small).is_none());
    }

    #[test]
    fn modal_button_width_uses_display_width_for_cjk() {
        assert_eq!(modal_button_width(" esc close "), 11);
        // " ↵ 确认 ": 1 + 1 + 1 + 4 + 1 display cells.
        assert_eq!(modal_button_width(" ↵ 确认 "), 8);
    }

    #[test]
    fn modal_button_style_maps_tone_and_state() {
        let palette = Palette::catppuccin();
        let focused = modal_button_style(
            &palette,
            ModalButtonTone::Primary,
            ModalButtonState::Focused,
        );
        assert_eq!(focused.bg, Some(palette.accent));
        assert!(focused.add_modifier.contains(Modifier::BOLD));
        let danger =
            modal_button_style(&palette, ModalButtonTone::Danger, ModalButtonState::Hovered);
        assert_eq!(danger.bg, Some(palette.red));
        let normal = modal_button_style(
            &palette,
            ModalButtonTone::Secondary,
            ModalButtonState::Normal,
        );
        assert_eq!(normal.fg, Some(palette.text));
        assert_eq!(normal.bg, Some(palette.surface0));
        let disabled = modal_button_style(
            &palette,
            ModalButtonTone::Primary,
            ModalButtonState::Disabled,
        );
        assert_eq!(disabled.fg, Some(palette.overlay0));
        assert!(!disabled.add_modifier.contains(Modifier::BOLD));
    }

    /// C-28 (ds-14)：输入框必须有自己的组件，且在每个内置主题下都与常态面板
    /// 可区分；`surface0` 塌缩成 Reset 的主题不能把输入框画成普通文本。
    #[test]
    fn input_field_style_stays_visible_for_every_built_in_theme() {
        for name in crate::config::THEME_NAMES {
            let palette = Palette::from_name(name).expect("built-in theme");
            let style = input_field_style(&palette);
            assert_eq!(style.fg, Some(palette.text), "{name} 输入文字应与正文同色");
            assert_ne!(
                input_field_bg(&palette),
                palette.panel_bg,
                "{name} 输入框底色与面板同色，字段没有边界"
            );
            if input_field_bg(&palette) == Color::Reset {
                assert!(
                    style.add_modifier.contains(Modifier::UNDERLINED),
                    "{name} 无可用底色时必须用下划线划出输入区"
                );
            }
        }
    }

    #[test]
    fn input_field_style_falls_back_when_every_surface_is_unset() {
        let mut palette = Palette::terminal();
        palette.surface0 = Color::Reset;
        palette.surface_dim = Color::Reset;
        palette.surface1 = Color::Reset;
        let style = input_field_style(&palette);
        assert_eq!(style.bg, Some(Color::Reset));
        assert!(style.add_modifier.contains(Modifier::UNDERLINED));
        // 弱 → 强依次回退：只把 surface0 留空时用 surface_dim。
        palette.surface_dim = Color::DarkGray;
        assert_eq!(input_field_bg(&palette), Color::DarkGray);
        palette.surface_dim = Color::Reset;
        palette.surface1 = Color::Gray;
        assert_eq!(input_field_bg(&palette), Color::Gray);
    }

    #[test]
    fn input_field_style_drops_the_dimmed_backdrop_modifier() {
        let style = input_field_style(&Palette::catppuccin());
        assert!(style.sub_modifier.contains(Modifier::DIM));
        assert_eq!(style.bg, Some(Palette::catppuccin().surface0));
    }

    #[test]
    fn close_and_continue_rects_track_label_width() {
        let area = Rect::new(10, 5, 40, 1);
        let close = close_button_rect(area);
        assert_eq!(close.width, modal_button_width(modal_close_button_text()));
        assert_eq!(close.right(), area.right());
        let cont = continue_button_rect(area);
        assert_eq!(cont.x, area.x);
        assert_eq!(cont.width, modal_button_width(modal_continue_button_text()));
    }
}
