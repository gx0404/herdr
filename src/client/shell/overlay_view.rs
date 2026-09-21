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
        // 联邦 agents 面板的行：按端点分代缓存，键未变则复用（PERF-02）。
        self.refresh_federated_agent_rows();
        let area = Rect::new(0, 0, cols, rows);
        let page_bounds = self.floating_page_rect(cols, rows);
        self.compute_scenes_scroll(area, page_bounds);
        self.compute_snippets_scroll(area, page_bounds);
        self.compute_palette_scroll(area, page_bounds);
        self.compute_settings_scroll(area, page_bounds);
        self.compute_scrollback_scrolls(area, page_bounds);
        self.compute_machine_files_scroll(area, page_bounds);
        self.compute_machines_view(area, page_bounds);
    }

    fn compute_palette_scroll(&mut self, area: Rect, page_bounds: Option<Rect>) {
        let Self { overlay, .. } = self;
        let Some(ClientShellOverlay::CommandPalette(palette)) = overlay.as_mut() else {
            return;
        };
        if let Some(window) = command_palette::palette_window(area, page_bounds, palette) {
            palette.scroll = window.start;
        }
        palette.reveal = false;
    }

    fn compute_settings_scroll(&mut self, area: Rect, page_bounds: Option<Rect>) {
        let Self { overlay, .. } = self;
        let Some(ClientShellOverlay::Settings(settings)) = overlay.as_mut() else {
            return;
        };
        if let Some((body, count)) =
            super::render::settings_list_window(area, page_bounds, settings)
        {
            settings.scroll = page::list_start(
                settings.scroll,
                settings.selected,
                count,
                usize::from(body.height),
                settings.reveal,
            );
        }
        settings.reveal = false;
    }

    /// 发行说明 / 产品公告：正文长度已知的浮层，只把 `scroll` 夹到当前窗口。
    fn compute_scrollback_scrolls(&mut self, area: Rect, page_bounds: Option<Rect>) {
        let Self {
            overlay,
            config,
            snapshot,
            ..
        } = self;
        let install_command = snapshot
            .as_deref()
            .map(|snapshot| snapshot.update_install_command.as_str())
            .unwrap_or_default();
        match overlay.as_mut() {
            Some(ClientShellOverlay::ReleaseNotes(notes)) => {
                let body = super::render::scrollback_overlay_body(
                    area,
                    page_bounds,
                    crate::ui::ModalSize::Content {
                        width: crate::ui::RELEASE_NOTES_MODAL_SIZE.0,
                        height: crate::ui::RELEASE_NOTES_MODAL_SIZE.1,
                    },
                );
                let Some(body) = body else {
                    return;
                };
                let lines =
                    crate::ui::release_notes_display_lines(notes, install_command, &config.palette);
                let metrics = crate::ui::display_lines_scroll_metrics(&lines, notes.scroll, body);
                notes.scroll = notes
                    .scroll
                    .min(u16::try_from(metrics.max_offset_from_bottom).unwrap_or(u16::MAX));
            }
            Some(ClientShellOverlay::ProductAnnouncement(announcement)) => {
                let body = super::render::scrollback_overlay_body(
                    area,
                    page_bounds,
                    crate::ui::ModalSize::XLarge,
                );
                let Some(body) = body else {
                    return;
                };
                let lines =
                    crate::ui::product_announcement_display_lines(announcement, &config.palette);
                let metrics =
                    crate::ui::display_lines_scroll_metrics(&lines, announcement.scroll, body);
                announcement.scroll = announcement
                    .scroll
                    .min(u16::try_from(metrics.max_offset_from_bottom).unwrap_or(u16::MAX));
            }
            Some(ClientShellOverlay::Help(help)) => {
                let Some((_, body)) = super::render::help_geometry(area, page_bounds) else {
                    return;
                };
                let max_scroll =
                    super::render::help_max_scroll(help, &config.keybinds, body, &config.palette);
                help.max_scroll = max_scroll;
                help.scroll = help.scroll.min(max_scroll);
            }
            _ => {}
        }
    }

    fn compute_machine_files_scroll(&mut self, area: Rect, page_bounds: Option<Rect>) {
        let Self { overlay, .. } = self;
        let Some(ClientShellOverlay::MachineFiles(page)) = overlay.as_mut() else {
            return;
        };
        let Some((_, body)) = machine_files_overlay::machine_files_geometry(area, page_bounds)
        else {
            return;
        };
        if let machine_files_overlay::ClientMachineFilesView::Viewer {
            line_offsets,
            max_scroll,
            scroll,
            ..
        } = &mut page.view
        {
            let visible = usize::from(body.height).max(1);
            let max = line_offsets.len().saturating_sub(visible);
            *max_scroll = max;
            *scroll = (*scroll).min(max);
        }
    }

    fn compute_scenes_scroll(&mut self, area: Rect, page_bounds: Option<Rect>) {
        let Some(ClientShellOverlay::Scenes(overlay)) = self.overlay.as_mut() else {
            return;
        };
        // 列表为空或正文高 0 时不动滚动位置：窗口为空的瞬间按 0 回写会把用户
        // 滚到的位置永久清零（STATE-03）。
        if matches!(overlay.view, scenes_overlay::ClientScenesView::List)
            && !overlay.scenes.is_empty()
        {
            if let Some((_, body)) = scenes_overlay::scene_list_geometry(area, page_bounds) {
                if body.height > 0 {
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
        }
        overlay.reveal = false;
    }

    fn compute_snippets_scroll(&mut self, area: Rect, page_bounds: Option<Rect>) {
        let Some(ClientShellOverlay::Snippets(overlay)) = self.overlay.as_mut() else {
            return;
        };
        match &mut overlay.view {
            snippets_overlay::ClientSnippetsView::List => {
                // 过滤到空结果 / 正文高 0 时不动滚动位置：窗口为空的瞬间按 0
                // 回写会把位置永久清零（STATE-03）。
                let rows =
                    snippets_overlay::filtered_snippets(&overlay.library, overlay.query.as_str())
                        .len();
                if rows > 0 {
                    if let Some((_, body)) =
                        snippets_overlay::snippet_list_geometry(area, page_bounds)
                    {
                        if body.height > 0 {
                            let selected = overlay.selected.min(rows - 1);
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
                }
            }
            snippets_overlay::ClientSnippetsView::History { selected, scroll } => {
                let count = overlay.library.history.len();
                if count > 0 {
                    if let Some((_, body)) =
                        snippets_overlay::snippet_history_geometry(area, page_bounds)
                    {
                        if body.height > 0 {
                            let selected = (*selected).min(count - 1);
                            let window = page::list_window(
                                body,
                                1,
                                count,
                                *scroll,
                                selected,
                                overlay.reveal,
                            );
                            *scroll = window.start;
                        }
                    }
                }
            }
            _ => {}
        }
        overlay.reveal = false;
    }
}
