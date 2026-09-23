use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
};

use crate::app::state::Palette;

/// 叠在 `accent` 底色上的前景色（选中标签、主按钮）。按对比度挑，而不是直接拿
/// `panel_bg`：两者亮度接近的主题、或降色后量化成同色时，字会看不见。
pub(crate) fn panel_contrast_fg(palette: &Palette) -> Color {
    super::color::contrast_fg(palette, palette.accent)
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
                ModalButtonTone::Danger => {
                    (palette.red, super::color::contrast_fg(palette, palette.red))
                }
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
                ModalButtonTone::Danger => {
                    (palette.red, super::color::contrast_fg(palette, palette.red))
                }
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

/// 正文叠在输入框底色上的对比度目标（WCAG AA 正文 4.5:1）。主题常态本身就
/// 达不到时以常态为准：聚焦态只要求「不比常态更难读」。
const INPUT_TEXT_MIN_CONTRAST: f32 = 4.5;

/// 输入框占位符的候选前景，按「弱 → 强」排列：默认 `overlay0`，底色吞掉它时
/// 依次换更亮的一档。
fn placeholder_candidates(palette: &Palette) -> [Color; 3] {
    [palette.overlay0, palette.overlay1, palette.subtext0]
}

/// 叠在 `bg` 上、且不比常态输入框里的占位符（`overlay0` 叠 [`input_field_bg`]）
/// 更难读的占位符前景；没有这样的候选时为 `None`。亮度取不到（`Reset` 等）
/// 时无从比较，只保证不与底色同色。
fn legible_placeholder_fg(palette: &Palette, bg: Color) -> Option<Color> {
    let baseline = super::color::contrast_ratio(palette.overlay0, input_field_bg(palette));
    placeholder_candidates(palette)
        .into_iter()
        .find(|&candidate| {
            candidate != bg
                && match (baseline, super::color::contrast_ratio(candidate, bg)) {
                    (Some(baseline), Some(ratio)) => ratio >= baseline,
                    _ => true,
                }
        })
}

/// 输入框占位符前景：默认 `overlay0`；它与 `bg` 同色、或在 `bg` 上比常态更难读
/// 时依次换 `overlay1`、`subtext0`，都不够就取对比度最高的候选（M8 复审：聚焦
/// 换了底色后，catppuccin 的占位符对比度曾从 2.57 掉到 1.87，terminal 主题则
/// 与 Gray 底色同色、整行不可见）。
pub(crate) fn input_placeholder_fg(palette: &Palette, bg: Color) -> Color {
    legible_placeholder_fg(palette, bg).unwrap_or_else(|| {
        placeholder_candidates(palette)
            .into_iter()
            .filter(|&candidate| candidate != bg)
            .max_by(|a, b| {
                let a = super::color::contrast_ratio(*a, bg).unwrap_or(0.0);
                let b = super::color::contrast_ratio(*b, bg).unwrap_or(0.0);
                a.total_cmp(&b)
            })
            .unwrap_or(palette.text)
    })
}

/// 聚焦态输入框底色：比 [`input_field_bg`] 强一档的结构面，只有「换了确实看得
/// 出、又不伤可读性」时才用，否则为 `None`，由 [`input_field_focused_style`]
/// 改用非颜色标记（M8 复审）。候选依次是 `surface1`、`selection_bg`，要同时满足：
/// - 与常态底色、面板底色都肉眼可辨（`Palette::row_bg_is_distinct` 同一口径：
///   vesper / rose-pine 的 `surface1` 与 `surface0` 只差 1.07 / 1.09，不算）；
/// - 不与正文 `text`、占位符 `overlay0` 同色（terminal / dracula / solarized
///   的 `surface1` 就是 `overlay0`）；
/// - 正文在它上面的对比度不低于常态（上限 4.5:1）；正文是 `Reset`（跟随终端
///   前景、亮度未知）时无从保证，不换；
/// - 仍有占位符前景能保持常态的可读性。
pub(crate) fn input_field_focused_bg(palette: &Palette) -> Option<Color> {
    let normal = input_field_bg(palette);
    let text_target = super::color::contrast_ratio(palette.text, normal)
        .map_or(INPUT_TEXT_MIN_CONTRAST, |ratio| {
            ratio.min(INPUT_TEXT_MIN_CONTRAST)
        });
    [palette.surface1, palette.selection_bg]
        .into_iter()
        .find(|&candidate| {
            Palette::row_bg_is_distinct(normal, candidate)
                && Palette::row_bg_is_distinct(palette.panel_bg, candidate)
                && candidate != palette.text
                && candidate != palette.overlay0
                && super::color::contrast_ratio(palette.text, candidate)
                    .is_some_and(|ratio| ratio >= text_target)
                && legible_placeholder_fg(palette, candidate).is_some()
        })
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

/// 文本输入框聚焦态样式：在 [`input_field_style`] 上叠加聚焦标记，保证与常态
/// 肉眼可辨（M8）。底色能安全地强一档（[`input_field_focused_bg`]）就换底色；
/// 否则保持常态底色、加下划线作为非颜色标记。常态已经靠下划线划出输入区
/// （结构面全是 `Reset`）时下划线保留，再加粗与常态区分。
pub(crate) fn input_field_focused_style(palette: &Palette) -> Style {
    let normal = input_field_style(palette);
    match input_field_focused_bg(palette) {
        Some(background) => normal.bg(background),
        None if normal.add_modifier.contains(Modifier::UNDERLINED) => {
            normal.add_modifier(Modifier::BOLD)
        }
        None => normal.add_modifier(Modifier::UNDERLINED),
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

    /// M8 复审（严重）：聚焦态输入框在每个内置主题（真彩色与 256 色）下都要
    /// 与常态肉眼可辨；换底色时要过 `row_bg_is_distinct` 门槛、不与占位符 /
    /// 正文同色、正文不比常态难读；占位符在两态都不能与底色同色，聚焦态也不能
    /// 比常态更难读。
    #[test]
    fn focused_input_field_is_distinct_and_legible_for_every_built_in_theme() {
        use crate::config::ColorDepth;
        use crate::ui::color::contrast_ratio;
        for name in crate::config::THEME_NAMES {
            for depth in [ColorDepth::Truecolor, ColorDepth::Color256] {
                let palette = Palette::from_name(name)
                    .expect("built-in theme")
                    .with_color_depth(depth);
                let normal = input_field_style(&palette);
                let focused = input_field_focused_style(&palette);
                let normal_bg = normal.bg.expect("常态有底色");
                let focused_bg = focused.bg.expect("聚焦态有底色");
                if focused_bg == normal_bg {
                    assert!(
                        focused.add_modifier.contains(Modifier::UNDERLINED)
                            && focused.add_modifier != normal.add_modifier,
                        "{name}/{depth:?}：底色没换时必须有非颜色标记"
                    );
                } else {
                    assert!(
                        Palette::row_bg_is_distinct(normal_bg, focused_bg)
                            && Palette::row_bg_is_distinct(palette.panel_bg, focused_bg),
                        "{name}/{depth:?}：聚焦底色 {focused_bg:?} 与常态 / 面板不可辨"
                    );
                    assert_ne!(focused_bg, palette.overlay0, "{name}/{depth:?}");
                    assert_ne!(focused_bg, palette.text, "{name}/{depth:?}");
                    let normal_text = contrast_ratio(palette.text, normal_bg);
                    let focused_text = contrast_ratio(palette.text, focused_bg);
                    if let (Some(normal_text), Some(focused_text)) = (normal_text, focused_text) {
                        assert!(
                            focused_text >= normal_text.min(4.5),
                            "{name}/{depth:?}：聚焦态正文对比度 {focused_text:.2} 过低"
                        );
                    }
                }
                let normal_placeholder = input_placeholder_fg(&palette, normal_bg);
                let focused_placeholder = input_placeholder_fg(&palette, focused_bg);
                assert_ne!(
                    normal_placeholder, normal_bg,
                    "{name}/{depth:?}：常态占位符与底色同色"
                );
                assert_ne!(
                    focused_placeholder, focused_bg,
                    "{name}/{depth:?}：聚焦态占位符与底色同色"
                );
                if let (Some(normal_ratio), Some(focused_ratio)) = (
                    contrast_ratio(normal_placeholder, normal_bg),
                    contrast_ratio(focused_placeholder, focused_bg),
                ) {
                    assert!(
                        focused_ratio + 1e-4 >= normal_ratio,
                        "{name}/{depth:?}：聚焦态占位符对比度 {focused_ratio:.2} 低于常态 {normal_ratio:.2}"
                    );
                }
            }
        }
    }

    /// 复审点名的几个主题：terminal 的 `surface1` 与占位符同为 Gray、正文是
    /// `Reset`；vesper 的 `surface1` 与 `surface0` 只差 1.07——都改用下划线；
    /// rose-pine 的 `surface1` 不够，`selection_bg` 够；catppuccin 仍换 `surface1`，
    /// 占位符换成不比常态难读的一档。
    #[test]
    fn focused_input_field_picks_the_documented_marker_per_theme() {
        use crate::ui::color::contrast_ratio;
        let terminal = Palette::terminal();
        assert_eq!(input_field_focused_bg(&terminal), None);
        let style = input_field_focused_style(&terminal);
        assert_eq!(style.bg, Some(Color::DarkGray));
        assert!(style.add_modifier.contains(Modifier::UNDERLINED));
        assert_eq!(
            input_placeholder_fg(&terminal, Color::DarkGray),
            terminal.overlay0
        );

        assert_eq!(input_field_focused_bg(&Palette::vesper()), None);

        let rose_pine = Palette::rose_pine();
        assert_eq!(
            input_field_focused_bg(&rose_pine),
            Some(rose_pine.selection_bg)
        );

        let catppuccin = Palette::catppuccin();
        assert_eq!(
            input_field_focused_bg(&catppuccin),
            Some(catppuccin.surface1)
        );
        let placeholder = input_placeholder_fg(&catppuccin, catppuccin.surface1);
        assert_ne!(
            placeholder, catppuccin.overlay0,
            "overlay0 叠 surface1 只有 1.87"
        );
        let ratio = contrast_ratio(placeholder, catppuccin.surface1).expect("真彩色");
        assert!(ratio >= 2.57, "聚焦态占位符对比度 {ratio:.2}");
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
