//! 页面窗口只改变客户端呈现，不重排或暂停后台终端。

use super::*;
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct Window {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

impl Window {
    pub(super) fn valid(&self) -> bool {
        [self.x, self.y, self.width, self.height]
            .iter()
            .all(|value| value.is_finite() && (0.0..=1.0).contains(value))
            && self.width > 0.0
            && self.height > 0.0
    }

    fn from_rect(rect: Rect, cols: u16, rows: u16) -> Self {
        Self {
            x: f32::from(rect.x) / f32::from(cols.max(1)),
            y: f32::from(rect.y) / f32::from(rows.max(1)),
            width: f32::from(rect.width) / f32::from(cols.max(1)),
            height: f32::from(rect.height) / f32::from(rows.max(1)),
        }
    }

    fn rect(&self, cols: u16, rows: u16) -> Rect {
        let width = ((self.width * f32::from(cols)).round() as u16)
            .max(20.min(cols))
            .min(cols);
        let height = ((self.height * f32::from(rows)).round() as u16)
            .max(6.min(rows))
            .min(rows);
        Rect::new(
            ((self.x * f32::from(cols)).round() as u16).min(cols.saturating_sub(width)),
            ((self.y * f32::from(rows)).round() as u16).min(rows.saturating_sub(height)),
            width,
            height,
        )
    }
}

pub(super) struct Drag {
    kind: ClientShellOverlayKind,
    key: String,
    start: (u16, u16),
    rect: Rect,
    resize_x: bool,
    resize_y: bool,
    left: bool,
    changed: bool,
}

fn window_key(overlay: &ClientShellOverlay) -> Option<String> {
    (!matches!(overlay, ClientShellOverlay::ContextMenu(_)))
        .then(|| format!("{:?}", overlay.kind()))
}

impl ClientShellState {
    pub(super) fn floating_page_rect(&self, cols: u16, rows: u16) -> Option<Rect> {
        let key = window_key(self.overlay.as_ref()?)?;
        self.page_windows
            .get(&key)
            .filter(|window| window.valid())
            .map(|window| window.rect(cols, rows))
    }

    pub(super) fn floating_page_mouse(
        &mut self,
        mouse: MouseEvent,
        outcome: &mut ClientShellInput,
    ) -> bool {
        let Some(overlay) = self.overlay.as_ref() else {
            self.page_drag = None;
            return false;
        };
        let Some(key) = window_key(overlay) else {
            self.page_drag = None;
            return false;
        };
        if !self.config.mouse_capture {
            self.page_drag = None;
            return false;
        }
        let kind = overlay.kind();
        let point = (mouse.column, mouse.row);
        let (cols, rows) = self.last_composed_size.unwrap_or_default();
        if let Some(mut drag) = self.page_drag.take() {
            if kind != drag.kind {
                return false;
            }
            match mouse.kind {
                MouseEventKind::Drag(MouseButton::Left) => {
                    let dx = i32::from(point.0) - i32::from(drag.start.0);
                    let dy = i32::from(point.1) - i32::from(drag.start.1);
                    let mut rect = drag.rect;
                    if drag.resize_x || drag.resize_y {
                        if drag.resize_x {
                            if drag.left {
                                let right = rect.right();
                                rect.x = (i32::from(rect.x) + dx)
                                    .clamp(0, i32::from(right.saturating_sub(20.min(cols))))
                                    as u16;
                                rect.width = right - rect.x;
                            } else {
                                rect.width = (i32::from(rect.width) + dx).clamp(
                                    i32::from(20.min(cols.saturating_sub(rect.x))),
                                    i32::from(cols.saturating_sub(rect.x)),
                                ) as u16;
                            }
                        }
                        if drag.resize_y {
                            rect.height = (i32::from(rect.height) + dy).clamp(
                                i32::from(6.min(rows.saturating_sub(rect.y))),
                                i32::from(rows.saturating_sub(rect.y)),
                            ) as u16;
                        }
                    } else {
                        rect.x = (i32::from(rect.x) + dx)
                            .clamp(0, i32::from(cols.saturating_sub(rect.width)))
                            as u16;
                        rect.y = (i32::from(rect.y) + dy)
                            .clamp(0, i32::from(rows.saturating_sub(rect.height)))
                            as u16;
                    }
                    drag.changed |= rect != drag.rect;
                    self.page_windows
                        .insert(drag.key.clone(), Window::from_rect(rect, cols, rows));
                    if drag.resize_x || drag.resize_y {
                        match self.overlay.as_mut() {
                            Some(ClientShellOverlay::CommandPalette(page)) => page.reveal = true,
                            Some(ClientShellOverlay::Settings(page)) => page.reveal = true,
                            Some(ClientShellOverlay::Machines(page)) => page.reveal = true,
                            _ => {}
                        }
                    }
                    self.page_drag = Some(drag);
                    outcome.repaint = true;
                }
                MouseEventKind::Up(MouseButton::Left) => {
                    if drag.changed {
                        self.persist_chrome_preferences(outcome);
                    }
                    outcome.repaint = true;
                }
                _ => self.page_drag = Some(drag),
            }
            return true;
        }
        if mouse.kind != MouseEventKind::Down(MouseButton::Left)
            || !mouse.modifiers.is_empty()
            || (self.workbench.enabled && self.workbench.dock.locked)
        {
            return false;
        }
        let rect = self.hits.overlay_bounds;
        if rect.width < 4 || rect.height < 4 || !contains(rect, point) {
            return false;
        }
        let left = point.0 == rect.x;
        let right = point.0 == rect.right().saturating_sub(1);
        let bottom = point.1 == rect.bottom().saturating_sub(1);
        if point.1 == rect.y || left || right || bottom {
            self.page_drag = Some(Drag {
                kind,
                key,
                start: point,
                rect,
                resize_x: left || right,
                resize_y: bottom,
                left,
                changed: false,
            });
            return true;
        }
        false
    }
}
