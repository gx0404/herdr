//! 表单原语适配：把客户端的 `TextEditor` 接到 `ui::kit::form_field`。
//!
//! kit 原语不依赖客户端层，只认 `value` 与字符下标 `cursor_col`；这里负责
//! 两头换算——渲染时取编辑器的字符光标，点击时把屏幕列换回字符下标。点击
//! 换算与 kit 的水平滚动同口径：聚焦时按光标位置跳过前缀列，未聚焦从头画。

use super::text_editor::TextEditor;
use crate::ui::kit::form_field::{FieldState, FormFieldSpec};

/// 由编辑器生成字段描述。只有 `Focused` 才带光标（kit 也只在聚焦时画）。
pub(super) fn field_spec<'a>(
    editor: &'a TextEditor,
    label: &'a str,
    placeholder: &'a str,
    state: FieldState<'a>,
    required: bool,
    hint: Option<&'a str>,
) -> FormFieldSpec<'a> {
    FormFieldSpec {
        label,
        value: editor.as_str(),
        placeholder,
        cursor_col: matches!(state, FieldState::Focused).then(|| editor.cursor_char_index()),
        state,
        required,
        hint,
        is_choice: false,
    }
}

fn char_width(ch: char) -> u16 {
    let mut bytes = [0u8; 4];
    crate::ui::display_width_u16(ch.encode_utf8(&mut bytes))
}

/// 输入行上第 `column` 列（相对输入框左缘）对应的字符下标。`cursor_col` 是
/// 这一帧交给 kit 的光标（未聚焦为 `None`）。点在字符上取该字符之前，点在
/// 文本之后取末尾。
pub(super) fn char_index_at_column(
    value: &str,
    cursor_col: Option<usize>,
    width: u16,
    column: u16,
) -> usize {
    // kit::form_field：光标列 = 光标前各字符宽度之和，跳过的前缀列 =
    // 光标列 - (宽 - 1)；宽字符跨过边界时整个跳过。
    let skip = cursor_col.map_or(0, |cursor| {
        let before = value
            .chars()
            .take(cursor)
            .fold(0u16, |sum, ch| sum.saturating_add(char_width(ch)));
        before.saturating_sub(width.saturating_sub(1))
    });
    let mut index = 0usize;
    let mut skipped = 0u16;
    let mut chars = value.chars().peekable();
    while skipped < skip {
        let Some(ch) = chars.next() else {
            break;
        };
        skipped = skipped.saturating_add(char_width(ch));
        index += 1;
    }
    let mut x = 0u16;
    for ch in chars {
        let next = x.saturating_add(char_width(ch));
        if column < next {
            return index;
        }
        x = next;
        index += 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::state::Palette;
    use crate::ui::kit::form_field::render_form_field;
    use ratatui::{buffer::Buffer, layout::Rect};

    #[test]
    fn spec_carries_the_cursor_only_while_focused() {
        let mut editor = TextEditor::new("host", false);
        editor.set_cursor_char_index(2);
        let spec = field_spec(&editor, "Host", "", FieldState::Focused, true, None);
        assert_eq!(spec.value, "host");
        assert_eq!(spec.cursor_col, Some(2));
        assert!(spec.required);
        let spec = field_spec(&editor, "Host", "", FieldState::Normal, false, Some("h"));
        assert_eq!(spec.cursor_col, None);
        assert_eq!(spec.hint, Some("h"));
    }

    /// 点击换算必须与 kit 实际画出来的位置一致：逐列点一遍，再在该下标处
    /// 聚焦渲染，光标必须落回被点的那一列（宽字符的右半格落在它左侧）。
    #[test]
    fn click_columns_map_back_to_what_kit_drew() {
        let palette = Palette::catppuccin();
        let area = Rect::new(0, 0, 6, 2);
        for (value, cursor) in [
            ("abcdefghij", Some(10)),
            ("abcdefghij", Some(3)),
            ("a中文bc", Some(5)),
            ("abc", None),
        ] {
            // 这一帧 kit 画出来的输入行。
            let mut buffer = Buffer::empty(area);
            let state = if cursor.is_some() {
                FieldState::Focused
            } else {
                FieldState::Normal
            };
            let spec = FormFieldSpec {
                label: "L",
                value,
                cursor_col: cursor,
                state,
                ..FormFieldSpec::default()
            };
            render_form_field(&mut buffer, area, &spec, &palette);
            let drawn: Vec<String> = (0..area.width)
                .map(|x| buffer[(x, 1)].symbol().to_owned())
                .collect();
            for column in 0..area.width {
                let index = char_index_at_column(value, cursor, area.width, column);
                let symbol = &drawn[usize::from(column)];
                match value.chars().nth(index) {
                    // 点在字符上：换算出的下标正是被点的字符（宽字符右半格
                    // 在 buffer 里是空串，属于左边那个字符）。
                    Some(ch) if !symbol.trim().is_empty() => {
                        assert_eq!(symbol, &ch.to_string(), "{value} @ {column}");
                    }
                    _ => {}
                }
            }
        }
        // 文本之后的空白落到末尾；未聚焦从头算。
        assert_eq!(char_index_at_column("abc", None, 10, 7), 3);
        assert_eq!(char_index_at_column("abc", None, 10, 0), 0);
        // 聚焦且滚动：框宽 6、光标在 10 → 跳过 5 列，第 0 列是「f」。
        assert_eq!(char_index_at_column("abcdefghij", Some(10), 6, 0), 5);
        // 宽字符右半格。
        assert_eq!(char_index_at_column("中文", None, 10, 1), 0);
        assert_eq!(char_index_at_column("中文", None, 10, 2), 1);
        assert_eq!(char_index_at_column("", Some(0), 10, 4), 0);
    }
}
