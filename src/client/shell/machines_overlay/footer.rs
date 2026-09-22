//! 机器浮层的合并页脚：按钮与键位提示合成一条 `kit::footer_hints`——可点的
//! 提示就是按钮（命中写进 `machines_actions`，悬浮复用
//! `ChromeHover::MachineButton`），纯导航键只显示。取代此前
//! `modal_stack_areas` 并排的「按钮行 + 键位提示行」两套入口。

use super::*;
use crate::ui::kit::footer_hints::{render_footer_hints, FooterHint};

/// 页脚上的一项：键帽 + 标签；`button` 为 `Some` 时可点。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct MachineHint<'a> {
    pub(super) key: &'a str,
    pub(super) label: &'a str,
    pub(super) button: Option<MachineOverlayButton>,
    pub(super) primary: bool,
    pub(super) enabled: bool,
}

impl<'a> MachineHint<'a> {
    /// 只显示的键（导航键等），不可点。
    pub(super) fn key(key: &'a str, label: &'a str) -> Self {
        Self {
            key,
            label: label.trim(),
            button: None,
            primary: false,
            enabled: true,
        }
    }

    /// 可点的提示：点它等同于按键 / 原来的按钮。
    pub(super) fn button(key: &'a str, label: &'a str, button: MachineOverlayButton) -> Self {
        Self {
            button: Some(button),
            ..Self::key(key, label)
        }
    }

    /// 主动作：键帽 accent 反色、放不下时最后丢。
    pub(super) fn primary(self) -> Self {
        Self {
            primary: true,
            ..self
        }
    }

    /// 暂不可用：整条置灰、不可点。
    pub(super) fn disabled(self) -> Self {
        Self {
            enabled: false,
            ..self
        }
    }
}

/// 与 kit 同口径的单项宽度（` key ` 键帽 + 空格 + 标签）与项间距。
fn hint_width(hint: &MachineHint<'_>) -> u16 {
    display_width(hint.key)
        .saturating_add(3)
        .saturating_add(display_width(hint.label))
}

const GAP: u16 = 2;

/// 贪心分行：按顺序往当前行里放，放不下就换行，最多 `max_rows` 行；最后一行
/// 收下剩余全部，由 kit 从尾部丢（primary 最后丢）。返回每行的起止下标。
fn split_rows(hints: &[MachineHint<'_>], width: u16, max_rows: u16) -> Vec<(usize, usize)> {
    let mut rows = Vec::with_capacity(usize::from(max_rows.max(1)));
    let mut start = 0usize;
    while start < hints.len() {
        if rows.len() + 1 >= usize::from(max_rows.max(1)) {
            rows.push((start, hints.len()));
            break;
        }
        let mut used = 0u16;
        let mut end = start;
        while end < hints.len() {
            let needed = hint_width(&hints[end]).saturating_add(if end > start { GAP } else { 0 });
            if end > start && used.saturating_add(needed) > width {
                break;
            }
            used = used.saturating_add(needed);
            end += 1;
        }
        rows.push((start, end));
        start = end;
    }
    rows
}

/// 画页脚，返回可点项的 `(命中矩形, 按钮)`。
pub(super) fn render_machine_footer(
    b: &mut Buffer,
    area: Rect,
    hints: &[MachineHint<'_>],
    cx: &super::super::feedback::ChromeContext<'_>,
) -> Vec<(Rect, MachineOverlayButton)> {
    let mut hits = Vec::new();
    if area.is_empty() || hints.is_empty() {
        return hits;
    }
    let hovered = match cx.hover {
        Some(super::super::feedback::ChromeHover::MachineButton(button)) => Some(*button),
        _ => None,
    };
    for (row, (start, end)) in split_rows(hints, area.width, area.height)
        .into_iter()
        .enumerate()
    {
        let row_area = Rect::new(area.x, area.y + row as u16, area.width, 1);
        let slice = &hints[start..end];
        let kit_hints: Vec<FooterHint<'_>> = slice
            .iter()
            .map(|hint| FooterHint {
                key: hint.key,
                label: hint.label,
                enabled: hint.enabled,
                primary: hint.primary,
            })
            .collect();
        let hover_index = hovered.and_then(|button| {
            slice
                .iter()
                .position(|hint| hint.enabled && hint.button == Some(button))
        });
        for (rect, index) in render_footer_hints(b, row_area, &kit_hints, hover_index, cx.palette) {
            let hint = &slice[index];
            if let (Some(button), true) = (hint.button, hint.enabled) {
                hits.push((rect, button));
            }
        }
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_fill_greedily_and_the_last_row_takes_the_rest() {
        let hints = [
            MachineHint::key("a", "one"),   // 9
            MachineHint::key("b", "two"),   // 9
            MachineHint::key("c", "three"), // 11
        ];
        // 9 + 2 + 9 = 20 放得下两项，第三项换行。
        assert_eq!(split_rows(&hints, 20, 2), vec![(0, 2), (2, 3)]);
        // 只许一行：全部交给 kit 从尾部丢。
        assert_eq!(split_rows(&hints, 20, 1), vec![(0, 3)]);
        // 够宽就一行。
        assert_eq!(split_rows(&hints, 80, 2), vec![(0, 3)]);
        assert!(split_rows(&[], 80, 2).is_empty());
    }
}
