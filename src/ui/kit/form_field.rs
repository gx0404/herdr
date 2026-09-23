//! 表单字段：标签行 + 输入行 + 可选的提示 / 错误行。原语只收 `value` 与
//! `cursor_col`（字符下标），不依赖客户端层的 `TextEditor`；适配函数由机器车道在
//! `client/shell/form.rs` 首用时建。输入框样式来自 `crate::ui::input_field_style`。

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
};

use super::{char_width, fill_row, put_str, put_str_ellipsis};
use crate::app::state::Palette;
use crate::ui::{display_width_u16, input_field_focused_bg, input_field_style, panel_contrast_fg};

/// 字段状态。`Invalid` 携带的错误文案占用提示行（替换 `hint`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum FieldState<'a> {
    #[default]
    Normal,
    Focused,
    Invalid(&'a str),
    Disabled,
}

/// 一个字段的全部输入。`cursor_col` 是 `value` 内的**字符下标**（0..=chars），
/// 只在 `Focused` 时画光标；值比输入框宽时水平滚动，让光标始终可见。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct FormFieldSpec<'a> {
    pub label: &'a str,
    pub value: &'a str,
    pub placeholder: &'a str,
    pub cursor_col: Option<usize>,
    pub state: FieldState<'a>,
    pub required: bool,
    pub hint: Option<&'a str>,
    /// 下拉选择字段（如「身份 agent」「仅用身份文件」）：`Focused` 时用
    /// accent 反色强调，与文本字段的聚焦态区分开（M8，恢复合并前的行为）。
    pub is_choice: bool,
}

/// 渲染结果：输入行矩形（点击聚焦用）、光标屏幕坐标（仅 `Focused`）、实际占用
/// 行数。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct FormFieldRender {
    pub input: Rect,
    pub cursor: Option<(u16, u16)>,
    pub height: u16,
}

/// 字段需要的行数：标签 + 输入，有提示或处于错误态再加一行。
pub(crate) fn form_field_height(spec: &FormFieldSpec<'_>) -> u16 {
    2 + u16::from(matches!(spec.state, FieldState::Invalid(_)) || spec.hint.is_some())
}

/// 光标所在的显示列，以及为让它可见需要跳过的前缀列数。
fn cursor_columns(value: &str, cursor: usize, width: u16) -> (u16, u16) {
    let mut column = 0usize;
    for (index, ch) in value.chars().enumerate() {
        if index >= cursor {
            break;
        }
        column += char_width(ch);
    }
    let column = u16::try_from(column).unwrap_or(u16::MAX);
    let skip = column.saturating_sub(width.saturating_sub(1));
    (column, skip)
}

/// 跳过至少 `skip` 列后剩下的文本切片，以及实际跳过的列数（按字符边界，宽字符
/// 跨过边界时整个跳过，实际列数可能比 `skip` 多 1）。
fn skip_columns(value: &str, skip: u16) -> (&str, u16) {
    let mut skipped = 0u16;
    for (byte, ch) in value.char_indices() {
        if skipped >= skip {
            return (&value[byte..], skipped);
        }
        skipped = skipped.saturating_add(u16::try_from(char_width(ch)).unwrap_or(u16::MAX));
    }
    ("", skipped)
}

/// 画字段。`area` 高度不足时从下往上省略（提示行、输入行），标签行总在。
pub(crate) fn render_form_field(
    buffer: &mut Buffer,
    area: Rect,
    spec: &FormFieldSpec<'_>,
    palette: &Palette,
) -> FormFieldRender {
    let mut render = FormFieldRender::default();
    if area.is_empty() {
        return render;
    }
    let height = form_field_height(spec).min(area.height);
    render.height = height;
    let width = area.width;

    // 标签行：状态决定颜色，必填加红星。
    let label_style = match spec.state {
        FieldState::Focused => Style::default()
            .fg(palette.accent)
            .add_modifier(Modifier::BOLD),
        FieldState::Invalid(_) => Style::default().fg(palette.red),
        FieldState::Disabled => Style::default().fg(palette.overlay0),
        FieldState::Normal => Style::default().fg(palette.subtext0),
    };
    let star_w = if spec.required { 2 } else { 0 };
    let label_w = display_width_u16(spec.label).min(width.saturating_sub(star_w));
    put_str_ellipsis(buffer, area.x, area.y, label_w, spec.label, label_style);
    if spec.required && width > label_w {
        put_str(
            buffer,
            area.x + label_w,
            area.y,
            star_w,
            " *",
            Style::default().fg(palette.red),
        );
    }

    // 输入行。
    if height >= 2 {
        let input = Rect::new(area.x, area.y + 1, width, 1);
        render.input = input;
        let style = match spec.state {
            FieldState::Disabled => Style::default()
                .fg(palette.overlay0)
                .bg(palette.surface_dim)
                .remove_modifier(Modifier::DIM),
            // choice 字段（下拉选择）聚焦时用 accent 反色强调，与合并前的
            // 专属高亮态一致，和文本字段的聚焦态区分开（M8）。
            FieldState::Focused if spec.is_choice => Style::default()
                .fg(panel_contrast_fg(palette))
                .bg(palette.accent)
                .add_modifier(Modifier::BOLD),
            // 文本字段聚焦时底色比 Normal 更亮一档，肉眼可辨（M8：合并前后
            // 两者一直同色，不是回归，但要修）。
            FieldState::Focused => Style::default()
                .fg(palette.text)
                .bg(input_field_focused_bg(palette))
                .remove_modifier(Modifier::DIM),
            FieldState::Normal | FieldState::Invalid(_) => input_field_style(palette),
        };
        fill_row(buffer, input.x, input.y, width, " ", style);
        if spec.value.is_empty() {
            put_str_ellipsis(
                buffer,
                input.x,
                input.y,
                width,
                spec.placeholder,
                style.fg(palette.overlay0),
            );
            if matches!(spec.state, FieldState::Focused) && spec.cursor_col.is_some() {
                render.cursor = Some((input.x, input.y));
            }
        } else {
            let focused_cursor = match spec.state {
                FieldState::Focused => spec.cursor_col,
                _ => None,
            };
            let (column, skip) =
                focused_cursor.map_or((0, 0), |cursor| cursor_columns(spec.value, cursor, width));
            let (visible, skipped) = skip_columns(spec.value, skip);
            put_str(buffer, input.x, input.y, width, visible, style);
            if focused_cursor.is_some() {
                let offset = column.saturating_sub(skipped).min(width - 1);
                render.cursor = Some((input.x + offset, input.y));
            }
        }
    }

    // 提示 / 错误行。
    if height >= 3 {
        let y = area.y + 2;
        match spec.state {
            FieldState::Invalid(message) => {
                put_str_ellipsis(
                    buffer,
                    area.x,
                    y,
                    width,
                    message,
                    Style::default().fg(palette.red),
                );
            }
            _ => {
                if let Some(hint) = spec.hint {
                    put_str_ellipsis(
                        buffer,
                        area.x,
                        y,
                        width,
                        hint,
                        Style::default().fg(palette.overlay0),
                    );
                }
            }
        }
    }
    render
}

#[cfg(test)]
mod tests {
    use super::super::row_text;
    use super::*;

    fn paint(area: Rect, spec: FormFieldSpec<'_>) -> (Buffer, FormFieldRender, Palette) {
        let palette = Palette::catppuccin();
        let mut buffer = Buffer::empty(area);
        let render = render_form_field(&mut buffer, area, &spec, &palette);
        (buffer, render, palette)
    }

    #[test]
    fn height_depends_on_hint_and_error_rows() {
        let base = FormFieldSpec {
            label: "Host",
            ..FormFieldSpec::default()
        };
        assert_eq!(form_field_height(&base), 2);
        assert_eq!(
            form_field_height(&FormFieldSpec {
                hint: Some("h"),
                ..base
            }),
            3
        );
        assert_eq!(
            form_field_height(&FormFieldSpec {
                state: FieldState::Invalid("bad"),
                ..base
            }),
            3
        );
    }

    #[test]
    fn normal_field_shows_label_star_value_and_hint() {
        let (buffer, render, palette) = paint(
            Rect::new(0, 0, 12, 3),
            FormFieldSpec {
                label: "Host",
                value: "example",
                placeholder: "host",
                required: true,
                hint: Some("ssh host"),
                ..FormFieldSpec::default()
            },
        );
        assert_eq!(row_text(&buffer, 0), "Host *      ");
        assert_eq!(row_text(&buffer, 1), "example     ");
        assert_eq!(row_text(&buffer, 2), "ssh host    ");
        assert_eq!(buffer[(5, 0)].style().fg, Some(palette.red));
        assert_eq!(buffer[(0, 0)].style().fg, Some(palette.subtext0));
        assert_eq!(buffer[(0, 1)].style().bg, input_field_style(&palette).bg);
        assert_eq!(buffer[(0, 2)].style().fg, Some(palette.overlay0));
        assert_eq!(render.input, Rect::new(0, 1, 12, 1));
        assert_eq!(render.cursor, None, "未聚焦不画光标");
        assert_eq!(render.height, 3);
    }

    #[test]
    fn focused_field_places_the_cursor_by_display_width_and_scrolls() {
        let (buffer, render, palette) = paint(
            Rect::new(0, 0, 8, 2),
            FormFieldSpec {
                label: "名称",
                value: "中文ab",
                cursor_col: Some(2),
                state: FieldState::Focused,
                ..FormFieldSpec::default()
            },
        );
        assert_eq!(row_text(&buffer, 0), "名称    ");
        assert_eq!(buffer[(0, 0)].style().fg, Some(palette.accent));
        assert_eq!(row_text(&buffer, 1), "中文ab  ");
        assert_eq!(render.cursor, Some((4, 1)), "两个宽字符后光标在第 4 列");

        // 值比框宽：滚动到光标可见，光标钉在最后一列。
        let (buffer, render, _) = paint(
            Rect::new(0, 0, 6, 2),
            FormFieldSpec {
                label: "L",
                value: "abcdefghij",
                cursor_col: Some(10),
                state: FieldState::Focused,
                ..FormFieldSpec::default()
            },
        );
        assert_eq!(row_text(&buffer, 1), "fghij ");
        assert_eq!(render.cursor, Some((5, 1)));
        // 光标在中间时不滚动。
        let (buffer, render, _) = paint(
            Rect::new(0, 0, 6, 2),
            FormFieldSpec {
                label: "L",
                value: "abcdefghij",
                cursor_col: Some(3),
                state: FieldState::Focused,
                ..FormFieldSpec::default()
            },
        );
        assert_eq!(row_text(&buffer, 1), "abcdef");
        assert_eq!(render.cursor, Some((3, 1)));
        // 滚动边界落在宽字符中间：整个宽字符跳过，光标按实际跳过的列数回算。
        let (buffer, render, _) = paint(
            Rect::new(0, 0, 4, 2),
            FormFieldSpec {
                label: "L",
                value: "a中文b",
                cursor_col: Some(3),
                state: FieldState::Focused,
                ..FormFieldSpec::default()
            },
        );
        // 光标在第 5 列（a=1、中=2、文=2），框宽 4 → 需跳 2 列，「中」横跨第 1–2
        // 列，实际跳 3 列，剩「文b」。
        assert_eq!(row_text(&buffer, 1), "文b ");
        assert_eq!(render.cursor, Some((2, 1)), "光标紧跟「文」之后");
    }

    #[test]
    fn placeholder_error_and_disabled_states() {
        let (buffer, render, palette) = paint(
            Rect::new(0, 0, 10, 3),
            FormFieldSpec {
                label: "Port",
                placeholder: "22",
                cursor_col: Some(0),
                state: FieldState::Focused,
                hint: Some("hidden"),
                ..FormFieldSpec::default()
            },
        );
        assert_eq!(row_text(&buffer, 1), "22        ");
        assert_eq!(
            buffer[(0, 1)].style().fg,
            Some(palette.overlay0),
            "占位文字灰"
        );
        assert_eq!(render.cursor, Some((0, 1)), "空值时光标在开头");

        let (buffer, _, palette) = paint(
            Rect::new(0, 0, 10, 3),
            FormFieldSpec {
                label: "Port",
                value: "99999",
                state: FieldState::Invalid("1–65535"),
                hint: Some("hidden"),
                ..FormFieldSpec::default()
            },
        );
        assert_eq!(row_text(&buffer, 2), "1–65535   ", "错误文案替换提示");
        assert_eq!(buffer[(0, 2)].style().fg, Some(palette.red));
        assert_eq!(buffer[(0, 0)].style().fg, Some(palette.red));

        let (buffer, render, palette) = paint(
            Rect::new(0, 0, 10, 2),
            FormFieldSpec {
                label: "Port",
                value: "22",
                cursor_col: Some(1),
                state: FieldState::Disabled,
                ..FormFieldSpec::default()
            },
        );
        assert_eq!(buffer[(0, 1)].style().fg, Some(palette.overlay0));
        assert_eq!(buffer[(0, 1)].style().bg, Some(palette.surface_dim));
        assert_eq!(render.cursor, None);
    }

    /// M8：合并进 `render_form_field` 之前，choice 字段（下拉选择）聚焦时
    /// 用 accent 反色强调；合并后与文本字段共用 `input_field_style`，丢了
    /// 这个专属高亮态。`is_choice` 恢复它。
    #[test]
    fn focused_choice_field_keeps_the_accent_highlight() {
        let (buffer, _, palette) = paint(
            Rect::new(0, 0, 10, 2),
            FormFieldSpec {
                label: "身份 agent",
                value: "‹ 默认 ›",
                state: FieldState::Focused,
                is_choice: true,
                ..FormFieldSpec::default()
            },
        );
        assert_eq!(
            buffer[(0, 1)].style().bg,
            Some(palette.accent),
            "choice 聚焦态应该用 accent 反色，不是普通输入框底色"
        );
        assert!(
            buffer[(0, 1)].style().add_modifier.contains(Modifier::BOLD),
            "choice 聚焦态应该加粗"
        );
    }

    /// M8：文本字段的 `Focused` 与 `Normal` 底色完全相同，肉眼看不出聚焦
    /// 在哪个字段上；`Focused` 要比 `Normal` 更亮一档。
    #[test]
    fn focused_text_field_uses_a_brighter_background_than_normal() {
        let spec = |state| FormFieldSpec {
            label: "用户",
            value: "root",
            cursor_col: Some(4),
            state,
            ..FormFieldSpec::default()
        };
        let (normal_buffer, _, _) = paint(Rect::new(0, 0, 10, 2), spec(FieldState::Normal));
        let (focused_buffer, _, _) = paint(Rect::new(0, 0, 10, 2), spec(FieldState::Focused));
        assert_ne!(
            normal_buffer[(0, 1)].style().bg,
            focused_buffer[(0, 1)].style().bg,
            "Focused 应该比 Normal 更亮，不能是同一个底色"
        );
    }

    #[test]
    fn short_areas_drop_rows_from_the_bottom_and_clip_labels() {
        let (buffer, render, _) = paint(
            Rect::new(0, 0, 6, 1),
            FormFieldSpec {
                label: "Hostname",
                value: "v",
                required: true,
                hint: Some("h"),
                ..FormFieldSpec::default()
            },
        );
        assert_eq!(row_text(&buffer, 0), "Hos… *");
        assert_eq!(render.height, 1);
        assert_eq!(render.input, Rect::default());
        let (_, render, _) = paint(Rect::default(), FormFieldSpec::default());
        assert_eq!(render, FormFieldRender::default());
    }
}
