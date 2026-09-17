//! 选字专用的不可变文本；与 PTY、实时 viewport 和渲染 dirty 状态无关。

use serde::{Deserialize, Serialize};

pub(crate) const MAX_SNAPSHOT_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const MAX_SNAPSHOT_CELLS: usize = 65_536;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FrozenCell {
    pub text: String,
    pub fg: u32,
    pub bg: u32,
    pub modifier: u16,
    /// 0：宽字符尾格，1：普通字符，2：宽字符首格，3：软换行前的空白占位。
    pub width: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hyperlink: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FrozenRow {
    pub cells: Vec<FrozenCell>,
    pub soft_wrapped: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FrozenText {
    pub cols: u16,
    pub viewport_rows: u16,
    pub viewport_start: u32,
    pub row_origin: u32,
    pub range_start: u32,
    pub range_end: u32,
    pub total_rows: u32,
    pub alternate_screen: bool,
    pub content_revision: u64,
    pub truncated: bool,
    pub rows: Vec<FrozenRow>,
}

impl FrozenText {
    pub(crate) fn valid_window(&self) -> bool {
        let Some(end) = self
            .row_origin
            .checked_add(self.rows.len().try_into().unwrap_or(u32::MAX))
        else {
            return false;
        };
        let Some(viewport_end) = self
            .viewport_start
            .checked_add(u32::from(self.viewport_rows))
        else {
            return false;
        };
        self.cols > 0
            && self.viewport_rows > 0
            && !self.rows.is_empty()
            && self.rows.len() <= 2048
            && self.rows.len().saturating_mul(usize::from(self.cols)) <= MAX_SNAPSHOT_CELLS
            && self.range_start <= self.row_origin
            && end <= self.range_end
            && self.range_end <= self.total_rows
            && self.viewport_start >= self.range_start
            && viewport_end <= self.range_end
            && self.rows.iter().all(|row| {
                row.cells.len() == usize::from(self.cols)
                    && row
                        .cells
                        .iter()
                        .all(|cell| cell.width <= 3 && !cell.text.is_empty())
            })
            && self.bytes() <= MAX_SNAPSHOT_BYTES
    }

    pub(crate) fn valid_capture(&self) -> bool {
        self.valid_window()
            && self.row(self.viewport_start).is_some()
            && self
                .viewport_start
                .checked_add(u32::from(self.viewport_rows.saturating_sub(1)))
                .is_some_and(|row| self.row(row).is_some())
    }

    pub(crate) fn window(&self, start: u32, count: u16) -> Option<Self> {
        let index = start.checked_sub(self.row_origin)? as usize;
        let end = index
            .saturating_add(usize::from(count.max(1)))
            .min(self.rows.len());
        let rows = self.rows.get(index..end)?.to_vec();
        Some(Self {
            cols: self.cols,
            viewport_rows: self.viewport_rows,
            viewport_start: self.viewport_start,
            row_origin: start,
            range_start: self.range_start,
            range_end: self.range_end,
            total_rows: self.total_rows,
            alternate_screen: self.alternate_screen,
            content_revision: self.content_revision,
            truncated: self.truncated,
            rows,
        })
    }

    pub(crate) fn bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self
                .rows
                .iter()
                .map(|row| {
                    std::mem::size_of::<FrozenRow>()
                        + row
                            .cells
                            .iter()
                            .map(|cell| {
                                std::mem::size_of::<FrozenCell>()
                                    + cell.text.len()
                                    + cell.hyperlink.as_ref().map_or(0, String::len)
                            })
                            .sum::<usize>()
                })
                .sum::<usize>()
    }

    pub(crate) fn row(&self, absolute: u32) -> Option<&FrozenRow> {
        self.rows
            .get(absolute.checked_sub(self.row_origin)? as usize)
    }

    pub(crate) fn selection(&self, anchor: (u32, u16), cursor: (u32, u16)) -> Option<String> {
        let (start, end) = if anchor <= cursor {
            (anchor, cursor)
        } else {
            (cursor, anchor)
        };
        if start.1 >= self.cols || end.1 >= self.cols {
            return None;
        }
        self.row(start.0)?;
        self.row(end.0)?;
        let mut text = String::new();
        for y in start.0..=end.0 {
            let row = self.row(y)?;
            let mut left = if y == start.0 {
                usize::from(start.1)
            } else {
                0
            };
            let right = if y == end.0 {
                usize::from(end.1) + 1
            } else {
                row.cells.len()
            };
            // 从宽字符尾格开始时仍复制完整 grapheme。
            if left > 0
                && row.cells.get(left).is_some_and(|cell| cell.width == 0)
                && row.cells[left - 1].width == 2
            {
                left -= 1;
            }
            let mut line = String::new();
            for cell in row.cells.get(left..right)? {
                if cell.width != 0 && cell.width != 3 {
                    line.push_str(&cell.text);
                }
            }
            if row.soft_wrapped && y < end.0 {
                text.push_str(&line);
            } else {
                text.push_str(line.trim_end_matches(' '));
                if y < end.0 {
                    text.push('\n');
                }
            }
        }
        text.truncate(text.trim_end_matches([' ', '\n']).len());
        Some(text)
    }
}
