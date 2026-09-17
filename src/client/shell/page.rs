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
        let footer_height = u16::from(remaining.height > 0);
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

pub(super) fn action_grid(area: Rect, labels: &[&str]) -> Vec<Rect> {
    use unicode_width::UnicodeWidthStr;
    let mut x = area.x;
    let mut y = area.y;
    let mut rects = Vec::with_capacity(labels.len());
    for label in labels {
        let width = (label.width().min(u16::MAX as usize) as u16)
            .saturating_add(4)
            .min(area.width);
        if x > area.x && x.saturating_add(width) > area.right() {
            x = area.x;
            y = y.saturating_add(1);
        }
        if y >= area.bottom() || width == 0 {
            break;
        }
        rects.push(Rect::new(x, y, width, 1));
        x = x.saturating_add(width).saturating_add(2);
    }
    rects
}

pub(super) fn action_row_count(width: u16, labels: &[&str]) -> u16 {
    action_grid(Rect::new(0, 0, width, u16::MAX), labels)
        .last()
        .map_or(0, |rect| rect.y.saturating_add(1))
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
