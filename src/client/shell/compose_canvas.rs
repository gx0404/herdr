//! compose 单 Buffer 管线（C-12 第二步后半 / 批 12b）。
//!
//! 旧管线在每个装饰段之间做整帧 `FrameData ↔ ratatui::Buffer` 往返（3~10 次/帧，
//! 其中至少一次结果被直接丢弃）。本类型让 compose 全程持有一个保留 Buffer：
//! 各 paint 段直接往 Buffer 里画，收尾只做一次 `from_ratatui_buffer_with_hyperlinks`。
//!
//! ratatui 的 `Cell` 不携带 OSC 8 超链接，所以 Buffer 旁挂一张链接侧表
//! （屏幕坐标 + 符号 + URI）：blit/冻结选区等往 Buffer 写内容时同步登记；最终转帧时
//! 按（位置 + 符号）挂接——与旧的 `preserving_effects` 语义逐格一致：某格的符号被
//! 后续段改写时链接自动失效（CFP-15 的浮层覆盖失效也由此保持），未被覆盖的链接保留。
//! 光标与 kitty graphics 作为旁路值随管线传递，与旧逐段 replace 的光标语义一致。

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use crate::protocol::{CellData, CursorState, FrameData};

/// 链接侧表条目：屏幕坐标、登记时的符号、URI。
type LinkEntry = ((u16, u16), compact_str::CompactString, String);

pub(super) struct ComposeCanvas {
    buffer: Buffer,
    cursor: Option<CursorState>,
    links: Vec<LinkEntry>,
    /// modifier 高 4 位的 underline 样式扩展（`4:2`/`4:3`…）是 herdr 的线格式扩展位，
    /// ratatui `Cell` 不携带：blit/冻结选区写格时稀疏登记（平铺索引 + 样式），
    /// 收尾转帧后写回。装饰段对 modifier 的改写语义与旧管线一致（样式位替换后
    /// 扩展位随 UNDERLINED 位一起失效或被保留）。
    underline_styles: Vec<(u32, u8)>,
}

impl ComposeCanvas {
    /// 取 compose 用的保留 Buffer：尺寸不变就复用（`reset` 清格但保留分配），
    /// 尺寸变了重建。`self.compose_buffer` 的存取由调用方（compose 入口）负责。
    pub(super) fn reuse_or_new(buffer: Option<Buffer>, cols: u16, rows: u16) -> Self {
        let mut buffer = buffer.unwrap_or_else(|| Buffer::empty(Rect::new(0, 0, cols, rows)));
        if buffer.area.width != cols || buffer.area.height != rows {
            buffer = Buffer::empty(Rect::new(0, 0, cols, rows));
        } else {
            buffer.reset();
        }
        Self {
            buffer,
            cursor: None,
            links: Vec::new(),
            underline_styles: Vec::new(),
        }
    }

    pub(super) fn buffer(&mut self) -> &mut Buffer {
        &mut self.buffer
    }

    #[cfg(test)]
    pub(super) fn buffer_ref(&self) -> &Buffer {
        &self.buffer
    }

    pub(super) fn cursor(&self) -> Option<CursorState> {
        self.cursor.clone()
    }

    pub(super) fn set_cursor(&mut self, cursor: Option<CursorState>) {
        self.cursor = cursor;
    }

    /// 登记一个带链接格（写 Buffer 的同一调用方负责同步登记）。
    pub(super) fn push_link(
        &mut self,
        x: u16,
        y: u16,
        symbol: compact_str::CompactString,
        uri: String,
    ) {
        self.links.push(((x, y), symbol, uri));
    }

    /// CFP-15：浮层覆盖区内的链接登记全部作废（即使浮层恰好写了同符号文本）。
    pub(super) fn clear_links_in(&mut self, area: Rect) {
        self.links
            .retain(|((x, y), _, _)| !crate::client::shell::contains(area, (*x, *y)));
    }

    pub(super) fn size(&self) -> (u16, u16) {
        (self.buffer.area.width, self.buffer.area.height)
    }

    /// 把 pane surface 帧 blit 进 Buffer（写符号/样式/登记链接/传递光标），
    /// 语义与旧 `blit_pane_surface`（FrameData 级）逐格一致。
    pub(super) fn blit_frame(&mut self, source: &FrameData, area: Rect) {
        let copy_width = source.width.min(area.width);
        let copy_height = source.height.min(area.height);
        for row in 0..copy_height {
            for col in 0..copy_width {
                let source_index = row as usize * source.width as usize + col as usize;
                let Some(source_cell) = source.cells.get(source_index) else {
                    continue;
                };
                let uri = source_cell
                    .hyperlink
                    .and_then(|index| source.hyperlinks.get(index as usize));
                self.put_cell_data(
                    area.x + col,
                    area.y + row,
                    source_cell,
                    uri.map(String::as_str),
                );
            }
        }
        // 尺寸不匹配（resize 首帧仍在等新尺寸的 surface）时真实光标在可见区外：
        // 把它夹回目标区域边缘但标记不可见；区域内的光标原样透传。
        self.cursor = source.cursor.as_ref().and_then(|cursor| {
            (copy_width > 0 && copy_height > 0).then(|| {
                let in_range = cursor.x < copy_width && cursor.y < copy_height;
                crate::protocol::CursorState {
                    x: area.x + cursor.x.min(copy_width - 1),
                    y: area.y + cursor.y.min(copy_height - 1),
                    visible: cursor.visible && in_range,
                    shape: cursor.shape,
                }
            })
        });
    }

    /// 保存一行区域的格内容（mode bar 保存/恢复用）。
    pub(super) fn save_cells(&self, area: Rect) -> Vec<ratatui::buffer::Cell> {
        let start = usize::from(area.y) * usize::from(self.buffer.area.width) + usize::from(area.x);
        self.buffer.content()[start..start + usize::from(area.width)].to_vec()
    }

    /// 恢复一行区域的格内容；光标落在区域内时清掉（与旧 restore_mode_bar 一致）。
    pub(super) fn restore_cells(&mut self, area: Rect, cells: &[ratatui::buffer::Cell]) {
        for (offset, cell) in cells.iter().enumerate() {
            let x = area.x + offset as u16;
            if x >= area.x + area.width
                || x >= self.buffer.area.width
                || area.y >= self.buffer.area.height
            {
                break;
            }
            self.buffer[(x, area.y)] = cell.clone();
        }
        if self
            .cursor
            .as_ref()
            .is_some_and(|cursor| crate::client::shell::contains(area, (cursor.x, cursor.y)))
        {
            self.cursor = None;
        }
    }

    /// 把一格 CellData 写进 Buffer（符号 + 样式 + skip），带链接时同步登记侧表，
    /// underline 样式扩展位同步进稀疏侧表（ratatui Cell 不携带该 nibble）。
    /// 写消隐（skip）格与旧 `blit_pane_surface` 的整格拷贝语义一致。
    pub(super) fn put_cell_data(&mut self, x: u16, y: u16, cell: &CellData, uri: Option<&str>) {
        if x >= self.buffer.area.width || y >= self.buffer.area.height {
            return;
        }
        let target = &mut self.buffer[(x, y)];
        target.set_symbol(cell.symbol.as_str());
        target.fg = crate::protocol::u32_to_color(cell.fg);
        target.bg = crate::protocol::u32_to_color(cell.bg);
        target.modifier = crate::protocol::u16_to_modifier(cell.modifier);
        target.skip = cell.skip;
        let underline = crate::protocol::underline_style_from_modifier(cell.modifier);
        if underline != 0 {
            let index = u32::from(y) * u32::from(self.buffer.area.width) + u32::from(x);
            self.underline_styles.push((index, underline));
        }
        if let Some(uri) = uri {
            self.push_link(x, y, cell.symbol.clone(), uri.to_owned());
        }
    }

    /// 收尾：唯一一次 Buffer → FrameData 转换。链接按（位置 + 符号）挂接——
    /// 符号被后续段改写的格自动掉链，与旧逐段 preserving_effects 语义一致；
    /// underline 样式扩展位按稀疏侧表写回（仅对仍有 UNDERLINED 位的格有意义）。
    /// 返回帧与 Buffer：Buffer 由调用方存回 `compose_buffer` 跨帧复用分配。
    pub(super) fn finish(self, graphics: Vec<u8>) -> (FrameData, Buffer) {
        let ComposeCanvas {
            buffer,
            cursor,
            links,
            underline_styles,
        } = self;
        let mut frame = FrameData::from_ratatui_buffer_with_hyperlinks(
            &buffer,
            cursor,
            &links
                .iter()
                .map(|(pos, symbol, uri)| (*pos, symbol.to_string(), uri.clone()))
                .collect::<Vec<_>>(),
        );
        for (index, style) in &underline_styles {
            if let Some(cell) = frame.cells.get_mut(*index as usize) {
                cell.modifier =
                    crate::protocol::modifier_u16_with_underline_style(cell.modifier, *style);
            }
        }
        frame.graphics = graphics;
        (frame, buffer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canvas_links_attach_only_when_symbol_survives() {
        let mut canvas = ComposeCanvas::reuse_or_new(None, 4, 2);
        let cell = CellData {
            symbol: "b".into(),
            fg: 0,
            bg: 0,
            modifier: 0,
            skip: false,
            hyperlink: None,
        };
        canvas.put_cell_data(1, 0, &cell, Some("https://example.com"));
        let (frame, _) = canvas.finish(Vec::new());
        assert_eq!(frame.cells[1].hyperlink, Some(0));
        assert_eq!(frame.hyperlinks, vec!["https://example.com".to_owned()]);

        // 符号被后续段改写：链接掉。
        let mut canvas = ComposeCanvas::reuse_or_new(None, 4, 2);
        canvas.put_cell_data(1, 0, &cell, Some("https://example.com"));
        canvas.buffer()[(1, 0)].set_symbol("x");
        let (frame, _) = canvas.finish(Vec::new());
        assert_eq!(frame.cells[1].hyperlink, None);
        assert!(frame.hyperlinks.is_empty());
    }

    #[test]
    fn canvas_clear_links_in_drops_covered_entries() {
        let mut canvas = ComposeCanvas::reuse_or_new(None, 4, 2);
        let cell = CellData {
            symbol: "b".into(),
            fg: 0,
            bg: 0,
            modifier: 0,
            skip: false,
            hyperlink: None,
        };
        canvas.put_cell_data(1, 0, &cell, Some("https://covered"));
        canvas.put_cell_data(2, 1, &cell, Some("https://visible"));
        canvas.clear_links_in(Rect::new(0, 0, 4, 1));
        let (frame, _) = canvas.finish(Vec::new());
        assert_eq!(frame.cells[1].hyperlink, None);
        assert_eq!(frame.cells[6].hyperlink, Some(0));
        assert_eq!(frame.hyperlinks, vec!["https://visible".to_owned()]);
    }

    #[test]
    fn canvas_preserves_underline_style_extension_bits() {
        // underline 样式扩展位（4:2/4:3…）是线格式 nibble，ratatui Cell 不携带：
        // blit 时稀疏登记、收尾写回，零装饰帧也不能丢。
        let mut canvas = ComposeCanvas::reuse_or_new(None, 4, 1);
        let curly = crate::protocol::modifier_u16_with_underline_style(1 << 4, 3);
        let cell = CellData {
            symbol: "u".into(),
            fg: 0,
            bg: 0,
            modifier: curly,
            skip: false,
            hyperlink: None,
        };
        canvas.put_cell_data(2, 0, &cell, None);
        let (frame, _) = canvas.finish(Vec::new());
        assert_eq!(
            frame.cells[2].modifier, curly,
            "underline style nibble survives the canvas pipeline"
        );
        assert_eq!(
            crate::protocol::underline_style_from_modifier(frame.cells[2].modifier),
            3
        );
    }

    #[test]
    fn canvas_reuse_keeps_allocation_across_frames() {
        let canvas = ComposeCanvas::reuse_or_new(None, 8, 4);
        let storage = canvas.buffer_ref().content().as_ptr();
        let canvas = ComposeCanvas::reuse_or_new(Some(canvas.buffer), 8, 4);
        assert_eq!(canvas.buffer_ref().content().as_ptr(), storage);
        let canvas = ComposeCanvas::reuse_or_new(Some(canvas.buffer), 4, 4);
        assert_ne!(
            canvas.buffer_ref().content().as_ptr(),
            storage,
            "resize rebuilds"
        );
    }
}
