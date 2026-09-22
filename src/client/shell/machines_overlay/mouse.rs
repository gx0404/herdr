//! 机器浮层的鼠标分派：列表悬浮 / 选中、详情与列表滚动、导入向导与转发
//! 编辑器的行与字段、表单字段与按钮、点窗外关闭。

use super::*;
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

impl ClientShellState {
    /// 机器浮层打开时的鼠标分派：不在该浮层时返回 `false`，由
    /// `mouse.rs::handle_mouse` 继续往下走。
    pub(in crate::client::shell) fn handle_machines_mouse(
        &mut self,
        mouse: MouseEvent,
        point: (u16, u16),
        outcome: &mut ClientShellInput,
    ) -> bool {
        if !matches!(self.overlay, Some(ClientShellOverlay::Machines(_))) {
            return false;
        }
        match mouse.kind {
            MouseEventKind::Moved => {
                // 指针只写 hover：`selected` 留给键盘与点击，否则划过列表
                // 就把 d / x / r / Shift+R 这些破坏性键重新指向「鼠标最后
                // 路过的机器」（MENU-01 / UX-04）。出界也要写 None。
                let hovered = self
                    .hits
                    .machines_rows
                    .iter()
                    .find(|(rect, _)| contains(*rect, point))
                    .map(|(_, profile_id)| profile_id.clone());
                outcome.repaint |= self.hover_machine_row(hovered.as_ref());
            }
            MouseEventKind::ScrollUp if contains(self.hits.machines_popup, point) => {
                if contains(self.hits.machines_detail_area, point) {
                    self.scroll_machine_details(-(self.config.mouse_scroll_lines as isize));
                } else {
                    self.scroll_machines_overlay(-(self.config.mouse_scroll_lines as isize));
                }
                outcome.repaint = true;
            }
            MouseEventKind::ScrollDown if contains(self.hits.machines_popup, point) => {
                if contains(self.hits.machines_detail_area, point) {
                    self.scroll_machine_details(self.config.mouse_scroll_lines as isize);
                } else {
                    self.scroll_machines_overlay(self.config.mouse_scroll_lines as isize);
                }
                outcome.repaint = true;
            }
            MouseEventKind::Down(MouseButton::Left) => {
                // Import wizard / forwards editor rows are index-keyed.
                let wizard_view = matches!(
                    self.overlay,
                    Some(ClientShellOverlay::Machines(ClientMachinesOverlay {
                        view: ClientMachinesView::Import(_) | ClientMachinesView::Forwards(_),
                        ..
                    }))
                );
                if wizard_view {
                    if let Some((_, button)) = self
                        .hits
                        .machines_actions
                        .iter()
                        .find(|(rect, _)| contains(*rect, point))
                        .copied()
                    {
                        self.activate_machine_button(button, outcome);
                        outcome.repaint = true;
                    } else if let Some((_, field)) = self
                        .hits
                        .machines_wizard_fields
                        .iter()
                        .find(|(rect, _)| contains(*rect, point))
                        .copied()
                    {
                        self.focus_machine_forward_field(field);
                        outcome.repaint = true;
                    } else if let Some((_, row)) = self
                        .hits
                        .machines_wizard_rows
                        .iter()
                        .find(|(rect, _)| contains(*rect, point))
                        .copied()
                    {
                        self.click_machine_wizard_row(row, outcome);
                    } else if !contains(self.hits.machines_popup, point) {
                        self.overlay = None;
                        outcome.repaint = true;
                    }
                    return true;
                }
                if contains(self.hits.machines_search, point) {
                    if let Some(ClientShellOverlay::Machines(overlay)) = self.overlay.as_mut() {
                        overlay.search_focused = true;
                        overlay.message = None;
                    }
                    outcome.repaint = true;
                } else if let Some((_, profile_id)) = self
                    .hits
                    .machines_rows
                    .iter()
                    .find(|(rect, _)| contains(*rect, point))
                    .cloned()
                {
                    // 点击是显式选择：写 `selected`（不是 hover）。
                    self.select_machine_row(&profile_id);
                    if self.hits.machines_detail_area.is_empty() {
                        self.open_machine_detail(&profile_id);
                    }
                    outcome.repaint = true;
                } else if let Some((_, button)) = self
                    .hits
                    .machines_actions
                    .iter()
                    .find(|(rect, _)| contains(*rect, point))
                    .copied()
                {
                    self.activate_machine_button(button, outcome);
                } else if let Some((rect, field)) = self
                    .hits
                    .machines_fields
                    .iter()
                    .find(|(rect, _)| contains(*rect, point))
                    .copied()
                {
                    self.click_machine_form_field(field, rect, point);
                    outcome.repaint = true;
                } else if !contains(self.hits.machines_popup, point) {
                    let running = matches!(
                        self.overlay,
                        Some(ClientShellOverlay::Machines(
                            ClientMachinesOverlay {
                                view:
                                    ClientMachinesView::Form(ref form),
                                ..
                            }
                        )) if form.bootstrap.is_some() || form.prompt.is_some()
                    );
                    if !running {
                        self.overlay = None;
                        outcome.repaint = true;
                    }
                }
            }
            _ => {}
        }
        true
    }
}
