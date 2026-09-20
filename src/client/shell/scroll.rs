use ratatui::{buffer::Buffer, layout::Rect, style::Style};

use super::Palette;

pub(super) fn list_scroll_metrics(
    row_heights: &[u16],
    gaps_after: &[u16],
    body_height: u16,
    requested_start: usize,
) -> crate::pane::ScrollMetrics {
    if row_heights.is_empty() || body_height == 0 {
        return crate::pane::ScrollMetrics {
            offset_from_bottom: 0,
            max_offset_from_bottom: 0,
            viewport_rows: 0,
        };
    }

    let mut used = 0u16;
    let mut max_start = row_heights.len();
    for index in (0..row_heights.len()).rev() {
        let height = row_heights[index].max(1).min(body_height);
        let gap = gaps_after.get(index).copied().unwrap_or(0);
        if used.saturating_add(height).saturating_add(gap) > body_height {
            break;
        }
        used = used.saturating_add(height).saturating_add(gap);
        max_start = index;
    }
    max_start = max_start.min(row_heights.len().saturating_sub(1));
    let start = requested_start.min(max_start);

    let mut viewport_rows = 0usize;
    let mut used = 0u16;
    for (index, row_height) in row_heights.iter().enumerate().skip(start) {
        let height = (*row_height).max(1).min(body_height);
        if used.saturating_add(height) > body_height {
            break;
        }
        used = used.saturating_add(height);
        viewport_rows += 1;
        let gap = gaps_after.get(index).copied().unwrap_or(0);
        if used.saturating_add(gap) > body_height {
            break;
        }
        used = used.saturating_add(gap);
    }

    crate::pane::ScrollMetrics {
        offset_from_bottom: max_start.saturating_sub(start),
        max_offset_from_bottom: max_start,
        viewport_rows,
    }
}

pub(super) fn list_scroll_start_to_reveal(
    row_heights: &[u16],
    gaps_after: &[u16],
    body_height: u16,
    requested_start: usize,
    target: usize,
) -> usize {
    let mut metrics = list_scroll_metrics(row_heights, gaps_after, body_height, requested_start);
    let mut start = metrics
        .max_offset_from_bottom
        .saturating_sub(metrics.offset_from_bottom);
    if target < start {
        return target;
    }
    while target >= start.saturating_add(metrics.viewport_rows)
        && start < metrics.max_offset_from_bottom
    {
        start = start.saturating_add(1);
        metrics = list_scroll_metrics(row_heights, gaps_after, body_height, start);
    }
    start
}

/// 行高恒 1、行间无间隙的列表的纯算术版本。
///
/// 语义与 `list_scroll_metrics(&vec![1; len], &[], body_height, requested_start)`
/// 逐位一致（`uniform_helpers_match_the_general_scroll_arithmetic` 固化），但不
/// 分配：折叠侧栏每帧要为工作区与 agents 两段各算一次，落在渲染热路径上，而
/// app-render 要求「一个标量事实足够时禁止分配」。
pub(super) fn uniform_scroll_metrics(
    len: usize,
    body_height: u16,
    requested_start: usize,
) -> crate::pane::ScrollMetrics {
    if len == 0 || body_height == 0 {
        return crate::pane::ScrollMetrics {
            offset_from_bottom: 0,
            max_offset_from_bottom: 0,
            viewport_rows: 0,
        };
    }
    let height = usize::from(body_height);
    let max_start = len.saturating_sub(height);
    let start = requested_start.min(max_start);
    crate::pane::ScrollMetrics {
        offset_from_bottom: max_start.saturating_sub(start),
        max_offset_from_bottom: max_start,
        viewport_rows: len.saturating_sub(start).min(height),
    }
}

/// `list_scroll_start_to_reveal` 的等高行版本，同样不分配。
pub(super) fn uniform_scroll_start_to_reveal(
    len: usize,
    body_height: u16,
    requested_start: usize,
    target: usize,
) -> usize {
    let metrics = uniform_scroll_metrics(len, body_height, requested_start);
    let start = metrics
        .max_offset_from_bottom
        .saturating_sub(metrics.offset_from_bottom);
    if target < start {
        return target;
    }
    // 目标行要落在 `[start, start + viewport_rows)` 内；等高行下只要没到底
    // viewport_rows 恒为 body_height，所需起点即 `target + 1 - body_height`。
    let needed = target
        .saturating_add(1)
        .saturating_sub(usize::from(body_height));
    start.max(needed).min(metrics.max_offset_from_bottom)
}

pub(super) fn render_list_scrollbar(
    buffer: &mut Buffer,
    track: Rect,
    metrics: crate::pane::ScrollMetrics,
    palette: &Palette,
    thumb_hovered: bool,
) {
    let Some(thumb) = crate::ui::scrollbar_thumb(metrics, track) else {
        return;
    };
    for row in track.y..track.bottom() {
        if let Some(cell) = buffer.cell_mut((track.x, row)) {
            cell.set_symbol("▕")
                .set_style(Style::default().fg(palette.surface_dim));
        }
    }
    let thumb_color = if thumb_hovered {
        palette.subtext0
    } else {
        palette.overlay0
    };
    for row in thumb.top..thumb.top.saturating_add(thumb.len) {
        if let Some(cell) = buffer.cell_mut((track.x, row)) {
            cell.set_symbol("▕")
                .set_style(Style::default().fg(thumb_color));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uniform_helpers_match_the_general_scroll_arithmetic() {
        // 折叠侧栏改用无分配版本后，这条穷举等价性用例是它唯一的语义真源：
        // 任何一侧改动导致两者分叉都会在这里变红。
        for len in 0..12usize {
            let row_heights = vec![1u16; len];
            for body_height in 0..8u16 {
                for requested_start in 0..14usize {
                    assert_eq!(
                        uniform_scroll_metrics(len, body_height, requested_start),
                        list_scroll_metrics(&row_heights, &[], body_height, requested_start),
                        "len={len} body_height={body_height} start={requested_start}"
                    );
                    for target in 0..len.saturating_add(2) {
                        assert_eq!(
                            uniform_scroll_start_to_reveal(
                                len,
                                body_height,
                                requested_start,
                                target
                            ),
                            list_scroll_start_to_reveal(
                                &row_heights,
                                &[],
                                body_height,
                                requested_start,
                                target
                            ),
                            "len={len} body_height={body_height} start={requested_start} target={target}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn list_metrics_preserve_variable_rows_and_caller_owned_gap_policy() {
        let top = list_scroll_metrics(&[1, 3, 2], &[1, 1, 0], 5, 0);
        assert_eq!(top.max_offset_from_bottom, 2);
        assert_eq!(top.offset_from_bottom, 2);
        assert_eq!(top.viewport_rows, 2);

        let bottom = list_scroll_metrics(&[1, 3, 2], &[1, 1, 0], 5, usize::MAX);
        assert_eq!(bottom.offset_from_bottom, 0);
        assert_eq!(bottom.viewport_rows, 1);

        let parent_child = list_scroll_metrics(&[2, 2, 2], &[0, 1, 0], 5, 0);
        assert_eq!(parent_child.max_offset_from_bottom, 1);
        assert_eq!(parent_child.viewport_rows, 2);
    }
}
