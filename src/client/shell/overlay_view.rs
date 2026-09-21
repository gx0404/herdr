//! 渲染前的浮层视图计算（STATE-04 / ARCH-02 / TOOL-13）。
//!
//! 滚动窗口与一次性 `reveal` 请求只在这里更新：渲染阶段是纯函数，只读状态，
//! 不再经 `OverlayRender` 把自己的输出回写成状态。几何与行数口径由各浮层模块
//! 导出的纯函数提供，渲染阶段复用同一份实现，两边不会漂移。

use super::*;

impl ClientShellState {
    /// 每帧在绘制之前调用：按当前帧尺寸刷新列表浮层的滚动状态。
    ///
    /// `reveal` 是一次性请求（键盘把选中行滚进视野），这里消费即清；滚轮只改
    /// `scroll`，不改键盘选中（C-20 残留面）。
    pub(super) fn compute_overlay_view(&mut self, cols: u16, rows: u16) {
        let area = Rect::new(0, 0, cols, rows);
        let page_bounds = self.floating_page_rect(cols, rows);
        self.compute_scenes_scroll(area, page_bounds);
        self.compute_snippets_scroll(area, page_bounds);
    }

    fn compute_scenes_scroll(&mut self, area: Rect, page_bounds: Option<Rect>) {
        let Some(ClientShellOverlay::Scenes(overlay)) = self.overlay.as_mut() else {
            return;
        };
        if matches!(overlay.view, scenes_overlay::ClientScenesView::List) {
            if let Some((_, body)) = scenes_overlay::scene_list_geometry(area, page_bounds) {
                let selected = scenes_overlay::scene_list_selected(overlay);
                let window = page::list_window(
                    body,
                    scenes_overlay::SCENE_LIST_ROW_HEIGHT,
                    overlay.scenes.len(),
                    overlay.scroll,
                    selected,
                    overlay.reveal,
                );
                overlay.scroll = window.start;
            }
        }
        overlay.reveal = false;
    }

    fn compute_snippets_scroll(&mut self, area: Rect, page_bounds: Option<Rect>) {
        let Some(ClientShellOverlay::Snippets(overlay)) = self.overlay.as_mut() else {
            return;
        };
        match &mut overlay.view {
            snippets_overlay::ClientSnippetsView::List => {
                let rows =
                    snippets_overlay::filtered_snippets(&overlay.library, overlay.query.as_str())
                        .len();
                if let Some((_, body)) = snippets_overlay::snippet_list_geometry(area, page_bounds)
                {
                    let selected = if rows == 0 {
                        0
                    } else {
                        overlay.selected.min(rows - 1)
                    };
                    let window = page::list_window(
                        body,
                        snippets_overlay::SNIPPET_LIST_ROW_HEIGHT,
                        rows,
                        overlay.scroll,
                        selected,
                        overlay.reveal,
                    );
                    overlay.scroll = window.start;
                }
            }
            snippets_overlay::ClientSnippetsView::History { selected, scroll } => {
                let count = overlay.library.history.len();
                if let Some((_, body)) =
                    snippets_overlay::snippet_history_geometry(area, page_bounds)
                {
                    let selected = (*selected).min(count.saturating_sub(1));
                    let window =
                        page::list_window(body, 1, count, *scroll, selected, overlay.reveal);
                    *scroll = window.start;
                }
            }
            _ => {}
        }
        overlay.reveal = false;
    }
}
