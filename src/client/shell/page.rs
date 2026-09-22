//! 页面共用的布局与列表投影。几何同时提供给绘制、键盘焦点和鼠标命中。

use ratatui::layout::Rect;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PageFocus {
    Navigation,
    Search,
    Content,
    Actions,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct PageLayout {
    pub header: Rect,
    pub navigation: Rect,
    pub search: Rect,
    pub content: Rect,
    pub actions: Rect,
    pub footer: Rect,
}

impl PageLayout {
    pub fn new(area: Rect, navigation_rows: u16, search: bool, actions: bool) -> Self {
        Self::with_action_rows(area, navigation_rows, search, u16::from(actions))
    }

    pub fn with_action_rows(
        area: Rect,
        navigation_rows: u16,
        search: bool,
        action_rows: u16,
    ) -> Self {
        Self::with_footer_rows(area, navigation_rows, search, action_rows, 1)
    }

    /// 页脚要多于一行时用这个入口：键表较长的页面（机器列表 / 工作台）给
    /// `render_key_hints` 两行，避免页面级键被单行尾部截断吃掉。
    pub fn with_footer_rows(
        area: Rect,
        navigation_rows: u16,
        search: bool,
        action_rows: u16,
        footer_rows: u16,
    ) -> Self {
        let mut remaining = area;
        let mut take = |height: u16| {
            let result = Rect::new(
                remaining.x,
                remaining.y,
                remaining.width,
                height.min(remaining.height),
            );
            remaining.y = result.bottom();
            remaining.height = remaining.height.saturating_sub(result.height);
            result
        };
        let header = take(1);
        let navigation = take(navigation_rows);
        let search = take(u16::from(search));
        take(u16::from(area.height >= 10));
        // `footer_rows == 1` 与旧行为逐格一致：剩余高度为 0 时没有页脚。
        let footer_height = footer_rows.min(remaining.height);
        let action_height = action_rows.min(remaining.height.saturating_sub(footer_height));
        let content = Rect::new(
            remaining.x,
            remaining.y,
            remaining.width,
            remaining
                .height
                .saturating_sub(footer_height + action_height),
        );
        let actions = Rect::new(
            remaining.x,
            content.bottom(),
            remaining.width,
            action_height,
        );
        let footer = Rect::new(
            remaining.x,
            actions.bottom(),
            remaining.width,
            footer_height,
        );
        Self {
            header,
            navigation,
            search,
            content,
            actions,
            footer,
        }
    }
}

pub(super) fn list_start(
    requested: usize,
    selected: usize,
    count: usize,
    height: usize,
    reveal: bool,
) -> usize {
    let height = height.max(1);
    let start = requested.min(count.saturating_sub(height));
    if reveal {
        start
            .min(selected)
            .max(selected.saturating_add(1).saturating_sub(height))
    } else {
        start
    }
}

/// 渲染前的列表滚动窗口：几何 + 行高 + 选中行 + 一次性 `reveal` 折算成实际
/// 起点与上界。视图计算阶段（`ClientShellState::compute_overlay_view`）用它
/// 更新 `scroll`，渲染阶段只读结果——渲染是纯函数，不再回写滚动状态
/// （STATE-04 / ARCH-02 / TOOL-13）。
#[derive(Debug, Clone, Copy)]
pub(super) struct ListWindow {
    pub body: Rect,
    pub visible: usize,
    pub start: usize,
}

pub(super) fn list_window(
    body: Rect,
    row_height: usize,
    count: usize,
    scroll: usize,
    selected: usize,
    reveal: bool,
) -> ListWindow {
    let row_height = row_height.max(1);
    let visible = (usize::from(body.height) / row_height).max(1);
    let start =
        list_start(scroll, selected, count, visible, reveal).min(count.saturating_sub(visible));
    ListWindow {
        body,
        visible,
        start,
    }
}

/// 分类窄屏换行，所有入口都保留；内容区域随实际导航高度调整。
pub(super) fn navigation_rows(width: u16, labels: &[&str]) -> u16 {
    use unicode_width::UnicodeWidthStr;
    let mut rows = 1u16;
    let mut used = 0u16;
    for label in labels {
        let size = (label.width().min(u16::MAX as usize) as u16)
            .saturating_add(3)
            .min(width);
        if used > 0 && used.saturating_add(size) > width {
            rows += 1;
            used = 0;
        }
        used = used.saturating_add(size);
    }
    rows
}

impl super::ClientShellState {
    pub(super) fn content_page(&self) -> Option<&super::ClientShellOverlay> {
        if matches!(
            self.overlay,
            Some(super::ClientShellOverlay::CommandPalette(_))
        ) {
            self.browser_return.as_deref()
        } else {
            self.overlay.as_ref()
        }
    }

    pub(super) fn content_page_mut(&mut self) -> Option<&mut super::ClientShellOverlay> {
        if matches!(
            self.overlay,
            Some(super::ClientShellOverlay::CommandPalette(_))
        ) {
            self.browser_return.as_deref_mut()
        } else {
            self.overlay.as_mut()
        }
    }
}
