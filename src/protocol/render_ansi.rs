//! Frame blitting — renders FrameData to the terminal using diff-based updates.
//!
//! The blitting strategy:
//! 1. On the first frame, write the entire buffer (full redraw).
//! 2. On subsequent frames, diff against the last frame and only write
//!    the cells that changed.
//! 3. Wrap each frame in synchronized output so terminals that support it do
//!    not expose intermediate cursor positions while the frame is painted.
//! 4. Before writing any cells, hide the cursor to avoid stray cursor
//!    artifacts on terminals that render the hardware cursor at intermediate
//!    `CUP` positions during the frame stream.
//! 5. After writing all changed cells, restore the final cursor visibility
//!    and position from `frame.cursor`.
//! 6. On hosts that need it, repeat the final cursor anchor after ending
//!    synchronized output so external IMEs can place candidate windows at the
//!    real input position. Windows Terminal exposes that repeat as visible
//!    cursor movement during active TUI repaints, so Windows skips it. The
//!    policy is an explicit [`BlitEncoder`] field: the client-rendered shell
//!    resolves `ui.repeat_ime_cursor_anchor` once at start via
//!    [`client_ime_anchor_repeat`] and injects it; server/headless encoders
//!    keep the legacy default. `auto` skips the repeat on terminals where the
//!    bare post-sync `CUP`+`?25h` is known to show up as cursor/tab-switch
//!    flicker (see [`ImeAnchorHostEnv::post_sync_anchor_repeat_shows_as_flicker`]).
//!
//! Escape sequences used:
//! - `CSI H` (CUP) — move cursor to (row, col)
//! - `CSI m` (SGR) — set graphic rendition (colors, bold, etc.)
//! - `CSI ? 2026 h/l` — begin/end synchronized output
//! - `CSI Ps SP q` — DECSCUSR cursor shape
//! - `ESC ] 52 ; c ; <base64> BEL` — OSC 52 clipboard write
//!
//! The goal is minimal output: skip unchanged cells, batch adjacent changes,
//! and minimize cursor movement.

use std::cmp;
use std::io::Write;

use unicode_width::UnicodeWidthStr;

use crate::protocol::{
    underline_style_from_modifier, CellData, CursorState, FrameData, PaneSurfacePatchRow,
};

const REVERSED_MODIFIER: u16 = 1 << 6;
const SYNC_OUTPUT_END: &[u8] = b"\x1b[?2026l";

pub(crate) fn final_sync_output_end(bytes: &[u8]) -> Option<usize> {
    bytes
        .windows(SYNC_OUTPUT_END.len())
        .rposition(|window| window == SYNC_OUTPUT_END)
}

/// Bytes produced by a [`BlitEncoder`] for one terminal frame.
pub(crate) struct EncodedBlit {
    /// Terminal escape bytes ready to write to the host terminal.
    pub(crate) bytes: Vec<u8>,
    /// Whether this frame was encoded as a full redraw.
    pub(crate) full: bool,
    next_last_visible_cursor: Option<(u16, u16)>,
    next_last_cursor_shape: u8,
}

/// Stateful encoder that diffs semantic frames into terminal ANSI bytes.
pub(crate) struct BlitEncoder {
    last_frame: Option<FrameData>,
    last_visible_cursor: Option<(u16, u16)>,
    last_cursor_shape: u8,
    /// 同步块结束后是否补发最终光标锚点（模块文档第 6 条）。显式字段而非进程级
    /// 全局：客户端按 `ui.repeat_ime_cursor_anchor` 注入，其它构造点用平台默认。
    repeat_ime_anchor: bool,
}

impl Default for BlitEncoder {
    fn default() -> Self {
        Self::with_ime_anchor_repeat(PLATFORM_ALLOWS_IME_ANCHOR_REPEAT)
    }
}

impl BlitEncoder {
    /// 平台默认策略（旧行为）：非 Windows 补发、Windows 不补发。server 帧流与
    /// headless 路径用它。
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// 显式注入块外锚点策略；客户端自渲染 shell 用 [`client_ime_anchor_repeat`] 的结果。
    pub(crate) fn with_ime_anchor_repeat(repeat_ime_anchor: bool) -> Self {
        Self {
            last_frame: None,
            last_visible_cursor: None,
            last_cursor_shape: 0,
            repeat_ime_anchor,
        }
    }

    /// 当前生效的块外锚点策略（测试断言注入链路用）。
    #[cfg(test)]
    pub(crate) fn repeats_ime_anchor(&self) -> bool {
        self.repeat_ime_anchor
    }

    pub(crate) fn encode(&self, frame: &FrameData, repaint: bool) -> EncodedBlit {
        self.encode_inner(frame, repaint, false)
    }

    pub(crate) fn encode_with_suppressed_visible_cursor(
        &self,
        frame: &FrameData,
        repaint: bool,
    ) -> EncodedBlit {
        self.encode_inner(frame, repaint, true)
    }

    fn encode_inner(
        &self,
        frame: &FrameData,
        repaint: bool,
        suppress_visible_cursor: bool,
    ) -> EncodedBlit {
        let previous_frame = self.last_frame.as_ref();
        let prev = if repaint { None } else { previous_frame };
        let full = repaint
            || prev.is_none()
            || prev.is_some_and(|p| p.width != frame.width || p.height != frame.height);
        let clear_before_full_redraw = previous_frame.is_none();
        let prof_stats =
            crate::render_prof::enabled().then(|| compute_prof_blit_stats(frame, prev, full));
        let prof_started = crate::render_prof::timer();
        let mut bytes = Vec::new();
        let mut next_last_visible_cursor = self.last_visible_cursor;
        let mut next_last_cursor_shape = self.last_cursor_shape;
        blit_frame_to_with_cursor_memory_and_clear_policy(
            &mut bytes,
            frame,
            prev,
            &mut next_last_visible_cursor,
            &mut next_last_cursor_shape,
            self.repeat_ime_anchor,
            clear_before_full_redraw,
            suppress_visible_cursor,
        );
        if let Some(stats) = prof_stats {
            crate::render_prof::duration_since("ansi_encode.total", prof_started);
            crate::render_prof::counter("ansi_encode.bytes", bytes.len() as u64);
            crate::render_prof::counter("ansi_encode.scanned_cells", stats.scanned_cells);
            crate::render_prof::counter("ansi_encode.changed_cells", stats.changed_cells);
            crate::render_prof::counter("ansi_encode.changed_runs", stats.changed_runs);
            if full {
                crate::render_prof::event("ansi_encode.full");
            } else {
                crate::render_prof::event("ansi_encode.partial");
            }
        }
        EncodedBlit {
            bytes,
            full,
            next_last_visible_cursor,
            next_last_cursor_shape,
        }
    }

    pub(crate) fn commit(&mut self, frame: FrameData, encoded: EncodedBlit) {
        self.last_visible_cursor = encoded.next_last_visible_cursor;
        self.last_cursor_shape = encoded.next_last_cursor_shape;
        self.last_frame = Some(frame);
    }

    pub(crate) fn is_current(&self, frame: &FrameData) -> bool {
        self.last_frame.as_ref() == Some(frame)
    }

    pub(crate) fn encode_patch(
        &self,
        rows: &[PaneSurfacePatchRow],
        cursor: Option<CursorState>,
        suppress_visible_cursor: bool,
    ) -> Option<EncodedBlit> {
        let frame = self.last_frame.as_ref()?;
        if rows.iter().any(|row| !patch_row_fits(frame, row)) || patch_rows_overlap(rows) {
            return None;
        }
        let mut bytes = Vec::new();
        let mut next_last_visible_cursor = self.last_visible_cursor;
        let mut next_last_cursor_shape = self.last_cursor_shape;
        blit_patch_to(
            &mut bytes,
            frame,
            rows,
            cursor,
            &mut next_last_visible_cursor,
            &mut next_last_cursor_shape,
            self.repeat_ime_anchor,
            suppress_visible_cursor,
        );
        Some(EncodedBlit {
            bytes,
            full: false,
            next_last_visible_cursor,
            next_last_cursor_shape,
        })
    }

    pub(crate) fn patch_rows_with_drawn_cursor(
        &self,
        rows: &[PaneSurfacePatchRow],
        cursor: Option<&CursorState>,
    ) -> Option<Vec<PaneSurfacePatchRow>> {
        let frame = self.last_frame.as_ref()?;
        let mut rows = rows.to_vec();
        let previous = frame
            .cursor
            .as_ref()
            .filter(|cursor| cursor.visible)
            .map(|cursor| clamp_cursor_position(frame, cursor.x, cursor.y));
        let next = cursor
            .filter(|cursor| cursor.visible)
            .map(|cursor| clamp_cursor_position(frame, cursor.x, cursor.y));

        if let Some((x, y)) = previous.filter(|position| Some(*position) != next) {
            if patch_cell_mut(&mut rows, x, y).is_none() {
                let mut cell = frame.cells.get(frame_cell_index(frame, x, y)?)?.clone();
                cell.modifier ^= REVERSED_MODIFIER;
                rows.push(PaneSurfacePatchRow {
                    x,
                    y,
                    cells: vec![cell],
                });
            }
        }
        if let Some((x, y)) = next {
            if let Some(cell) = patch_cell_mut(&mut rows, x, y) {
                cell.modifier ^= REVERSED_MODIFIER;
            } else if previous != next {
                let mut cell = frame.cells.get(frame_cell_index(frame, x, y)?)?.clone();
                cell.modifier ^= REVERSED_MODIFIER;
                rows.push(PaneSurfacePatchRow {
                    x,
                    y,
                    cells: vec![cell],
                });
            }
        }
        Some(rows)
    }

    pub(crate) fn commit_patch(
        &mut self,
        rows: &[PaneSurfacePatchRow],
        cursor: Option<CursorState>,
        encoded: EncodedBlit,
    ) -> bool {
        let Some(frame) = self.last_frame.as_mut() else {
            return false;
        };
        for row in rows {
            let start = usize::from(row.y) * usize::from(frame.width) + usize::from(row.x);
            let end = start + row.cells.len();
            let Some(target) = frame.cells.get_mut(start..end) else {
                return false;
            };
            target.clone_from_slice(&row.cells);
        }
        frame.cursor = cursor;
        self.last_visible_cursor = encoded.next_last_visible_cursor;
        self.last_cursor_shape = encoded.next_last_cursor_shape;
        true
    }
}

pub(crate) fn frame_with_drawn_cursor(mut frame: FrameData) -> FrameData {
    if let Some(cursor) = frame.cursor.as_ref().filter(|cursor| cursor.visible) {
        let (x, y) = clamp_cursor_position(&frame, cursor.x, cursor.y);
        let idx = (y as usize)
            .saturating_mul(frame.width as usize)
            .saturating_add(x as usize);
        if let Some(cell) = frame.cells.get_mut(idx) {
            cell.modifier ^= REVERSED_MODIFIER;
        }
    }
    frame
}

#[derive(Clone, Copy, Default)]
struct ProfBlitStats {
    scanned_cells: u64,
    changed_cells: u64,
    changed_runs: u64,
}

fn compute_prof_blit_stats(
    frame: &FrameData,
    prev: Option<&FrameData>,
    full: bool,
) -> ProfBlitStats {
    let Some(prev) = prev.filter(|_| !full) else {
        let changed_cells = frame.cells.iter().filter(|cell| !cell.skip).count() as u64;
        return ProfBlitStats {
            scanned_cells: frame.cells.len() as u64,
            changed_cells,
            changed_runs: changed_cells,
        };
    };
    if prev.width != frame.width || prev.height != frame.height {
        let changed_cells = frame.cells.iter().filter(|cell| !cell.skip).count() as u64;
        return ProfBlitStats {
            scanned_cells: frame.cells.len() as u64,
            changed_cells,
            changed_runs: changed_cells,
        };
    }

    let sanitized_hyperlinks = sanitized_frame_hyperlinks(frame);
    let prev_sanitized_hyperlinks = sanitized_frame_hyperlinks(prev);
    let mut stats = ProfBlitStats {
        scanned_cells: frame.cells.len() as u64,
        changed_cells: 0,
        changed_runs: 0,
    };
    for row in 0..frame.height {
        let mut in_run = false;
        let mut invalidated = 0usize;
        let mut to_skip = 0usize;
        for col in 0..frame.width {
            let idx = (row as usize) * (frame.width as usize) + (col as usize);
            let cell = &frame.cells[idx];
            let prev_cell = &prev.cells[idx];
            let changed = !cell.skip
                && (!cells_visually_equal(
                    &sanitized_hyperlinks,
                    cell,
                    &prev_sanitized_hyperlinks,
                    prev_cell,
                ) || invalidated > 0)
                && to_skip == 0;
            if changed {
                stats.changed_cells += 1;
                if !in_run {
                    stats.changed_runs += 1;
                    in_run = true;
                }
            } else {
                in_run = false;
            }
            to_skip = cell_width(cell).saturating_sub(1);
            let affected_width = cmp::max(cell_width(cell), cell_width(prev_cell));
            invalidated = cmp::max(affected_width, invalidated).saturating_sub(1);
        }
    }
    stats
}

// ---------------------------------------------------------------------------
// Color → escape sequence
// ---------------------------------------------------------------------------

/// Converts a packed u32 color to an SGR escape sequence fragment.
///
/// Returns a string like `38;5;123` (indexed) or `38;2;255;128;64` (RGB)
/// or `39` (reset), without the leading `\x1b[` or trailing `m`.
fn color_to_sgr_fg(val: u32) -> String {
    match val >> 24 {
        0x00 => match val & 0xFF {
            0x00 => "39".to_owned(), // Reset
            0x01 => "30".to_owned(), // Black
            0x02 => "31".to_owned(), // Red
            0x03 => "32".to_owned(), // Green
            0x04 => "33".to_owned(), // Yellow
            0x05 => "34".to_owned(), // Blue
            0x06 => "35".to_owned(), // Magenta
            0x07 => "36".to_owned(), // Cyan
            0x08 => "37".to_owned(), // Gray (light gray)
            0x09 => "90".to_owned(), // DarkGray
            0x0A => "91".to_owned(), // LightRed
            0x0B => "92".to_owned(), // LightGreen
            0x0C => "93".to_owned(), // LightYellow
            0x0D => "94".to_owned(), // LightBlue
            0x0E => "95".to_owned(), // LightMagenta
            0x0F => "96".to_owned(), // LightCyan
            0x10 => "97".to_owned(), // White
            _ => "39".to_owned(),    // Unknown → Reset
        },
        0x01 => format!("38;5;{}", val & 0xFF), // Indexed
        0x02 => {
            // RGB
            let r = (val >> 16) & 0xFF;
            let g = (val >> 8) & 0xFF;
            let b = val & 0xFF;
            format!("38;2;{r};{g};{b}")
        }
        _ => "39".to_owned(), // Unknown → Reset
    }
}

/// Converts a packed u32 color to a background SGR fragment.
fn color_to_sgr_bg(val: u32) -> String {
    match val >> 24 {
        0x00 => match val & 0xFF {
            0x00 => "49".to_owned(),  // Reset
            0x01 => "40".to_owned(),  // Black
            0x02 => "41".to_owned(),  // Red
            0x03 => "42".to_owned(),  // Green
            0x04 => "43".to_owned(),  // Yellow
            0x05 => "44".to_owned(),  // Blue
            0x06 => "45".to_owned(),  // Magenta
            0x07 => "46".to_owned(),  // Cyan
            0x08 => "47".to_owned(),  // Gray (light gray)
            0x09 => "100".to_owned(), // DarkGray
            0x0A => "101".to_owned(), // LightRed
            0x0B => "102".to_owned(), // LightGreen
            0x0C => "103".to_owned(), // LightYellow
            0x0D => "104".to_owned(), // LightBlue
            0x0E => "105".to_owned(), // LightMagenta
            0x0F => "106".to_owned(), // LightCyan
            0x10 => "107".to_owned(), // White
            _ => "49".to_owned(),     // Unknown → Reset
        },
        0x01 => format!("48;5;{}", val & 0xFF), // Indexed
        0x02 => {
            let r = (val >> 16) & 0xFF;
            let g = (val >> 8) & 0xFF;
            let b = val & 0xFF;
            format!("48;2;{r};{g};{b}")
        }
        _ => "49".to_owned(),
    }
}

// ---------------------------------------------------------------------------
// Modifier → SGR
// ---------------------------------------------------------------------------

/// Converts a u16 modifier bitmask to SGR escape sequence fragments.
///
/// Returns a Vec of SGR parameter strings (e.g., "1" for bold, "3" for italic).
fn modifier_to_sgr_parts(val: u16) -> Vec<&'static str> {
    let mut parts = Vec::new();

    // ratatui::Modifier bits (from bitflags)
    const BOLD: u16 = 1 << 0; // 0x01
    const DIM: u16 = 1 << 1; // 0x02
    const ITALIC: u16 = 1 << 2; // 0x04
    const UNDERLINED: u16 = 1 << 3; // 0x08
    const SLOW_BLINK: u16 = 1 << 4; // 0x10
    const RAPID_BLINK: u16 = 1 << 5; // 0x20
    const HIDDEN: u16 = 1 << 7; // 0x80
    const CROSSED_OUT: u16 = 1 << 8; // 0x100

    if val & BOLD != 0 {
        parts.push("1");
    }
    if val & DIM != 0 {
        parts.push("2");
    }
    if val & ITALIC != 0 {
        parts.push("3");
    }
    if val & UNDERLINED != 0 {
        parts.push(match underline_style_from_modifier(val) {
            2 => "4:2",
            3 => "4:3",
            4 => "4:4",
            5 => "4:5",
            _ => "4",
        });
    }
    if val & SLOW_BLINK != 0 {
        parts.push("5");
    }
    if val & RAPID_BLINK != 0 {
        parts.push("6");
    }
    if val & REVERSED_MODIFIER != 0 {
        parts.push("7");
    }
    if val & HIDDEN != 0 {
        parts.push("8");
    }
    if val & CROSSED_OUT != 0 {
        parts.push("9");
    }

    parts
}

/// Builds a complete SGR escape sequence for a cell's style.
fn build_sgr(fg: u32, bg: u32, modifier: u16) -> String {
    let mut parts = vec!["0".to_owned()];
    parts.extend(
        modifier_to_sgr_parts(modifier)
            .into_iter()
            .map(str::to_owned),
    );
    parts.push(color_to_sgr_fg(fg));
    parts.push(color_to_sgr_bg(bg));
    format!("\x1b[{}m", parts.join(";"))
}

// ---------------------------------------------------------------------------
// Cell comparison
// ---------------------------------------------------------------------------

/// Checks if two cells are visually identical.
fn cells_equal(a: &CellData, b: &CellData) -> bool {
    a.symbol == b.symbol
        && a.fg == b.fg
        && a.bg == b.bg
        && a.modifier == b.modifier
        && a.hyperlink == b.hyperlink
    // Skip flag is only for ratatui internal use, not visual.
}

// ---------------------------------------------------------------------------
// Blitting
// ---------------------------------------------------------------------------

/// Blits a frame to a writer, diffing against the previous frame.
#[cfg(test)]
fn blit_frame_to(writer: impl Write, frame: &FrameData, prev: Option<&FrameData>) {
    let mut last_visible_cursor = None;
    let mut last_cursor_shape = 0;
    blit_frame_to_with_cursor_memory(
        writer,
        frame,
        prev,
        &mut last_visible_cursor,
        &mut last_cursor_shape,
        false,
    );
}

#[cfg(test)]
fn blit_frame_to_with_cursor_memory(
    writer: impl Write,
    frame: &FrameData,
    prev: Option<&FrameData>,
    last_visible_cursor: &mut Option<(u16, u16)>,
    last_cursor_shape: &mut u8,
    suppress_visible_cursor: bool,
) {
    blit_frame_to_with_cursor_memory_and_policy(
        writer,
        frame,
        prev,
        last_visible_cursor,
        last_cursor_shape,
        PLATFORM_ALLOWS_IME_ANCHOR_REPEAT,
        suppress_visible_cursor,
    );
}

#[cfg(test)]
fn blit_frame_to_with_cursor_memory_and_policy(
    writer: impl Write,
    frame: &FrameData,
    prev: Option<&FrameData>,
    last_visible_cursor: &mut Option<(u16, u16)>,
    last_cursor_shape: &mut u8,
    repeat_ime_anchor: bool,
    suppress_visible_cursor: bool,
) {
    blit_frame_to_with_cursor_memory_and_clear_policy(
        writer,
        frame,
        prev,
        last_visible_cursor,
        last_cursor_shape,
        repeat_ime_anchor,
        true,
        suppress_visible_cursor,
    );
}

fn frame_cell_index(frame: &FrameData, x: u16, y: u16) -> Option<usize> {
    (x < frame.width && y < frame.height)
        .then(|| usize::from(y) * usize::from(frame.width) + usize::from(x))
}

fn patch_cell_mut(rows: &mut [PaneSurfacePatchRow], x: u16, y: u16) -> Option<&mut CellData> {
    rows.iter_mut().rev().find_map(|row| {
        if row.y != y || x < row.x {
            return None;
        }
        row.cells.get_mut(usize::from(x - row.x))
    })
}

fn patch_rows_overlap(rows: &[PaneSurfacePatchRow]) -> bool {
    rows.iter().enumerate().any(|(index, left)| {
        let left_end = left.x.saturating_add(left.cells.len() as u16);
        rows[index + 1..].iter().any(|right| {
            if left.y != right.y {
                return false;
            }
            let right_end = right.x.saturating_add(right.cells.len() as u16);
            left.x < right_end && right.x < left_end
        })
    })
}

fn patch_row_fits(frame: &FrameData, row: &PaneSurfacePatchRow) -> bool {
    let Ok(len) = u16::try_from(row.cells.len()) else {
        return false;
    };
    if row.y >= frame.height
        || row.x.saturating_add(len) > frame.width
        || row.cells.iter().any(|cell| cell.hyperlink.is_some())
    {
        return false;
    }
    let start = usize::from(row.y) * usize::from(frame.width) + usize::from(row.x);
    let end = start + row.cells.len();
    frame
        .cells
        .get(start..end)
        .is_some_and(|cells| cells.iter().all(|cell| cell.hyperlink.is_none()))
}

fn blit_patch_to(
    mut writer: impl Write,
    frame: &FrameData,
    rows: &[PaneSurfacePatchRow],
    cursor: Option<CursorState>,
    last_visible_cursor: &mut Option<(u16, u16)>,
    last_cursor_shape: &mut u8,
    repeat_ime_anchor: bool,
    suppress_visible_cursor: bool,
) {
    let _ = writer.write_all(b"\x1b[?2026h\x1b[?25l\x1b]8;;\x1b\\");
    let mut last_sgr = String::new();
    let mut active_hyperlink = None;
    for row in rows {
        let mut invalidated = 0usize;
        let mut to_skip = 0usize;
        let mut next_inline_col = None;
        for (offset, cell) in row.cells.iter().enumerate() {
            let col = row.x + offset as u16;
            let idx = usize::from(row.y) * usize::from(frame.width) + usize::from(col);
            let prev_cell = &frame.cells[idx];
            if !cell.skip && (!cells_equal(cell, prev_cell) || invalidated > 0) && to_skip == 0 {
                let cursor_position =
                    (next_inline_col != Some(col) || invalidated > 0).then_some((col, row.y));
                write_cell(
                    &mut writer,
                    cursor_position,
                    cell,
                    &mut last_sgr,
                    &mut active_hyperlink,
                    frame,
                );
                next_inline_col = (cell.symbol.is_ascii() && cell_width(cell) == 1)
                    .then_some(col.saturating_add(1));
            }
            to_skip = cell_width(cell).saturating_sub(1);
            let affected_width = cmp::max(cell_width(cell), cell_width(prev_cell));
            invalidated = cmp::max(affected_width, invalidated).saturating_sub(1);
        }
    }
    close_hyperlink(&mut writer, &mut active_hyperlink);
    if !last_sgr.is_empty() {
        let _ = writer.write_all(b"\x1b[0m");
    }

    let cursor_frame = FrameData {
        cells: Vec::new(),
        width: frame.width,
        height: frame.height,
        cursor,
        hyperlinks: Vec::new(),
        graphics: Vec::new(),
    };
    let mut host_cursor = resolve_host_cursor_state(&cursor_frame, last_visible_cursor);
    if suppress_visible_cursor && host_cursor.visible {
        host_cursor.visible = false;
    }
    write_host_cursor_state(&mut writer, host_cursor, last_cursor_shape);
    let _ = writer.write_all(b"\x1b[?2026l");
    if repeat_ime_anchor {
        write_ime_anchor_cursor_state(&mut writer, host_cursor);
    }
    let _ = writer.flush();
}

fn blit_frame_to_with_cursor_memory_and_clear_policy(
    mut writer: impl Write,
    frame: &FrameData,
    prev: Option<&FrameData>,
    last_visible_cursor: &mut Option<(u16, u16)>,
    last_cursor_shape: &mut u8,
    repeat_ime_anchor: bool,
    clear_before_full_redraw: bool,
    suppress_visible_cursor: bool,
) {
    // On first frame or size change, do a full redraw.
    let full_redraw =
        prev.is_none() || prev.is_some_and(|p| p.width != frame.width || p.height != frame.height);

    // Ask terminals that support synchronized output to apply the whole frame
    // atomically. This keeps IMEs and cursor trackers from observing the
    // intermediate CUP positions used while painting changed cells.
    let _ = writer.write_all(b"\x1b[?2026h");

    // Hide cursor before any cell writes to avoid stray cursor artifacts
    // on terminals that render the hardware cursor at intermediate CUP positions.
    let _ = writer.write_all(b"\x1b[?25l");

    // Start each frame from a known OSC 8 state. If a previous write was
    // interrupted or the outer terminal had an active hyperlink, unlinked cells
    // must not inherit it.
    let _ = writer.write_all(b"\x1b]8;;\x1b\\");

    if full_redraw {
        if clear_before_full_redraw {
            let _ = writer.write_all(b"\x1b[2J");
        }
        write_all_cells(&mut writer, frame);
    } else {
        // Diff-based update: only write changed cells.
        let prev = prev.unwrap();
        write_changed_cells(&mut writer, frame, prev);
    }

    // Position the cursor while it is still hidden, then restore visibility.
    // Showing before moving makes slow terminals and IMEs briefly observe the
    // cursor at the last painted cell, which can be an animated sidebar/status
    // cell rather than the focused pane's input position. When the focused pane
    // hides its cursor, still park the host cursor intentionally so IMEs do not
    // anchor to whichever cell happened to be painted last.
    let mut host_cursor = resolve_host_cursor_state(frame, last_visible_cursor);
    if suppress_visible_cursor && host_cursor.visible {
        host_cursor.visible = false;
    }
    write_host_cursor_state(&mut writer, host_cursor, last_cursor_shape);

    // End the synchronized output block immediately after the final cursor
    // state is emitted so supporting terminals can present the frame atomically.
    let _ = writer.write_all(b"\x1b[?2026l");

    // Some native IMEs track candidate-window placement from normal terminal
    // cursor updates and may not observe cursor moves emitted inside synchronized
    // output. Re-emit only the resolved final cursor anchor after the sync block
    // on targets that need it; Windows Terminal exposes that repeat as cursor
    // movement during active TUI repaints.
    if repeat_ime_anchor {
        write_ime_anchor_cursor_state(&mut writer, host_cursor);
    }
    let _ = writer.flush();
}

/// `ui.repeat_ime_cursor_anchor` 的运行时形态：协议层不依赖 config 模型的 serde 细节。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum ImeAnchorRepeatMode {
    /// 宿主已知支持 DEC 2026 同步输出时关闭块外重复，未知宿主保持旧行为。
    #[default]
    Auto,
    /// 无条件重复（Windows 除外）。
    Always,
    /// 从不重复。
    Never,
}

impl From<crate::config::RepeatImeCursorAnchorConfig> for ImeAnchorRepeatMode {
    fn from(config: crate::config::RepeatImeCursorAnchorConfig) -> Self {
        use crate::config::RepeatImeCursorAnchorConfig as Config;
        match config {
            Config::Auto => Self::Auto,
            Config::Always => Self::Always,
            Config::Never => Self::Never,
        }
    }
}

/// 启动期采集一次的宿主终端身份证据；仓库里没有对外层终端的 DECRQM 2026 探测，
/// 与 `terminal_notify` / `handshake::direct_graphics_profile_values` 一样按环境变量启发。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct ImeAnchorHostEnv {
    pub(crate) term_program: Option<String>,
    pub(crate) term: Option<String>,
    /// `KITTY_WINDOW_ID` 存在（kitty 不设置 `TERM_PROGRAM`）。
    pub(crate) kitty_window: bool,
    /// 运行在 tmux/screen 里（`TMUX`/`STY` 存在，或 `TERM` 以 `screen`/`tmux` 开头）。
    /// 多路复用器会把外层终端的 `TERM_PROGRAM`/`KITTY_WINDOW_ID` 原样传给 pane，
    /// 但 2026 passthrough 行为随版本变化，因此必须先于宿主白名单判定（与
    /// `input::model::host_modify_other_keys_mode_for_env` 先查 `TMUX` 同一约定）。
    pub(crate) in_multiplexer: bool,
}

impl ImeAnchorHostEnv {
    pub(crate) fn from_process_env() -> Self {
        Self::from_values(
            std::env::var("TERM_PROGRAM").ok(),
            std::env::var("TERM").ok(),
            std::env::var_os("KITTY_WINDOW_ID").is_some(),
            std::env::var_os("TMUX").is_some() || std::env::var_os("STY").is_some(),
        )
    }

    /// 由环境变量值构造（可测）：`multiplexer_var` 是 `TMUX`/`STY` 任一存在。
    fn from_values(
        term_program: Option<String>,
        term: Option<String>,
        kitty_window: bool,
        multiplexer_var: bool,
    ) -> Self {
        let in_multiplexer = multiplexer_var
            || term
                .as_deref()
                .is_some_and(|term| term.starts_with("screen") || term.starts_with("tmux"));
        Self {
            term_program,
            term,
            kitty_window,
            in_multiplexer,
        }
    }

    /// 在这些宿主上，同步块外的裸 `CUP`+`?25h` 会被看见成光标/切换标签闪烁，所以
    /// `auto` 不补发。白名单表示「补发会被看见」，**不等价于**「宿主原子呈现 DEC 2026」：
    /// 例如 wezterm 在 `?2026h` 处也会 flush，一帧被拆成多次渲染，块外锚点恰恰因此
    /// 可见。多路复用器（tmux/screen）先于白名单判定：外层终端身份会被继承，但
    /// passthrough 行为随版本变化，保守按未知宿主处理并保留补发。VS Code/Apple Terminal
    /// 等同样不在列表内。按终端名判定不看版本。
    pub(crate) fn post_sync_anchor_repeat_shows_as_flicker(&self) -> bool {
        if self.in_multiplexer {
            return false;
        }
        if self.kitty_window {
            return true;
        }
        let program = self
            .term_program
            .as_deref()
            .map(|value| value.trim().to_ascii_lowercase());
        if matches!(
            program.as_deref(),
            Some(
                "wezterm"
                    | "kitty"
                    | "ghostty"
                    | "contour"
                    | "foot"
                    | "iterm.app"
                    | "alacritty"
                    | "rio"
            )
        ) {
            return true;
        }
        let term = self
            .term
            .as_deref()
            .map(|value| value.trim().to_ascii_lowercase())
            .unwrap_or_default();
        matches!(
            term.as_str(),
            "xterm-kitty" | "xterm-ghostty" | "alacritty" | "contour" | "rio"
        ) || term.contains("wezterm")
            || term.starts_with("foot")
    }
}

/// 纯判定（与平台无关）：给定配置模式与宿主证据，是否在同步块外重复最终光标锚点。
pub(crate) fn resolve_ime_anchor_repeat(
    mode: ImeAnchorRepeatMode,
    host: &ImeAnchorHostEnv,
) -> bool {
    match mode {
        ImeAnchorRepeatMode::Always => true,
        ImeAnchorRepeatMode::Never => false,
        ImeAnchorRepeatMode::Auto => !host.post_sync_anchor_repeat_shows_as_flicker(),
    }
}

/// 平台默认（旧行为）：Windows Terminal 把块外补发显示为 TUI 重绘期间的光标移动，
/// 因此 Windows 恒不补发；其它平台默认补发。
#[cfg(windows)]
const PLATFORM_ALLOWS_IME_ANCHOR_REPEAT: bool = false;
#[cfg(not(windows))]
const PLATFORM_ALLOWS_IME_ANCHOR_REPEAT: bool = true;

/// 客户端自渲染 shell 启动时解析一次的生效策略：平台闸门叠加 [`resolve_ime_anchor_repeat`]，
/// 结果由调用方注入 [`BlitEncoder::with_ime_anchor_repeat`]。生效值以 info 级别记录，
/// 便于排查「为什么我的 auto 没生效」。
#[must_use = "生效值必须注入 BlitEncoder，否则配置不起作用"]
pub(crate) fn client_ime_anchor_repeat(mode: ImeAnchorRepeatMode, host: &ImeAnchorHostEnv) -> bool {
    let effective = PLATFORM_ALLOWS_IME_ANCHOR_REPEAT && resolve_ime_anchor_repeat(mode, host);
    tracing::info!(
        ?mode,
        term_program = host.term_program.as_deref().unwrap_or(""),
        term = host.term.as_deref().unwrap_or(""),
        kitty_window = host.kitty_window,
        in_multiplexer = host.in_multiplexer,
        repeat = effective,
        "resolved post-sync IME cursor anchor repeat"
    );
    effective
}

/// Writes all cells in the frame (full redraw).
fn cell_width(cell: &CellData) -> usize {
    if is_halfwidth_katakana_voiced_grapheme(&cell.symbol) {
        return 2;
    }
    cell.symbol.width()
}

fn is_halfwidth_katakana_voiced_grapheme(symbol: &str) -> bool {
    let mut chars = symbol.chars();
    let Some(base) = chars.next() else {
        return false;
    };
    let Some(mark) = chars.next() else {
        return false;
    };
    chars.next().is_none()
        && ('\u{ff66}'..='\u{ff9d}').contains(&base)
        && matches!(mark, '\u{ff9e}' | '\u{ff9f}')
}

#[derive(Clone, Copy)]
struct HostCursorState {
    position: (u16, u16),
    visible: bool,
    /// DECSCUSR parameter (0–6). 0 means terminal default.
    shape: u8,
}

fn resolve_host_cursor_state(
    frame: &FrameData,
    last_visible_cursor: &mut Option<(u16, u16)>,
) -> HostCursorState {
    if let Some(cursor) = &frame.cursor {
        if cursor.visible {
            let position = clamp_cursor_position(frame, cursor.x, cursor.y);
            *last_visible_cursor = Some(position);
            return HostCursorState {
                position,
                visible: true,
                shape: normalize_cursor_shape(cursor.shape),
            };
        }

        let position = clamp_cursor_position(frame, cursor.x, cursor.y);
        return HostCursorState {
            position,
            visible: false,
            shape: normalize_cursor_shape(cursor.shape),
        };
    }

    let position = (*last_visible_cursor)
        .map(|(x, y)| clamp_cursor_position(frame, x, y))
        .unwrap_or_else(|| default_hidden_cursor_position(frame));
    HostCursorState {
        position,
        visible: false,
        shape: 0,
    }
}

fn normalize_cursor_shape(shape: u8) -> u8 {
    if shape <= 6 {
        shape
    } else {
        0
    }
}

fn default_hidden_cursor_position(frame: &FrameData) -> (u16, u16) {
    (
        frame.width.saturating_sub(1),
        frame.height.saturating_sub(1),
    )
}

fn clamp_cursor_position(frame: &FrameData, x: u16, y: u16) -> (u16, u16) {
    (
        x.min(frame.width.saturating_sub(1)),
        y.min(frame.height.saturating_sub(1)),
    )
}

fn write_cursor_position(writer: &mut impl Write, (x, y): (u16, u16)) {
    // CUP: move cursor to (row+1, col+1) — 1-based.
    let _ = write!(writer, "\x1b[{};{}H", y + 1, x + 1);
}

fn write_host_cursor_state(writer: &mut impl Write, cursor: HostCursorState, last_shape: &mut u8) {
    write_cursor_position(writer, cursor.position);
    if cursor.shape != *last_shape {
        let _ = write!(writer, "\x1b[{} q", cursor.shape);
        *last_shape = cursor.shape;
    }
    if cursor.visible {
        // Show cursor only after it is already at the final position.
        let _ = writer.write_all(b"\x1b[?25h");
    } else {
        let _ = writer.write_all(b"\x1b[?25l");
    }
}

fn write_ime_anchor_cursor_state(writer: &mut impl Write, cursor: HostCursorState) {
    write_cursor_position(writer, cursor.position);
    if cursor.visible {
        let _ = writer.write_all(b"\x1b[?25h");
    } else {
        let _ = writer.write_all(b"\x1b[?25l");
    }
}

fn write_all_cells(writer: &mut impl Write, frame: &FrameData) {
    let mut last_sgr = String::new();
    let mut active_hyperlink = None;
    for row in 0..frame.height {
        let mut to_skip = 0usize;
        let mut next_inline_col = None;
        for col in 0..frame.width {
            if to_skip > 0 {
                to_skip -= 1;
                continue;
            }

            let idx = (row as usize) * (frame.width as usize) + (col as usize);
            let cell = &frame.cells[idx];

            if cell.skip {
                next_inline_col = None;
                continue;
            }

            let cursor_position = (next_inline_col != Some(col)).then_some((col, row));
            write_cell(
                writer,
                cursor_position,
                cell,
                &mut last_sgr,
                &mut active_hyperlink,
                frame,
            );
            let width = cell_width(cell);
            next_inline_col =
                (cell.symbol.is_ascii() && width == 1).then_some(col.saturating_add(1));
            to_skip = width.saturating_sub(1);
        }
    }

    close_hyperlink(writer, &mut active_hyperlink);

    // Reset style at the end.
    let _ = writer.write_all(b"\x1b[0m");
}

fn cell_hyperlink_uri<'a>(frame: &'a FrameData, cell: &CellData) -> Option<&'a str> {
    let index = cell.hyperlink? as usize;
    frame.hyperlinks.get(index).map(String::as_str)
}

fn sanitized_hyperlink_uri(uri: &str) -> Option<String> {
    let sanitized: String = uri
        .chars()
        .filter(|ch| *ch != '\x1b' && *ch != '\x07' && !ch.is_control())
        .collect();
    (!sanitized.is_empty()).then_some(sanitized)
}

fn sanitized_frame_hyperlinks(frame: &FrameData) -> Vec<Option<String>> {
    frame
        .hyperlinks
        .iter()
        .map(|uri| sanitized_hyperlink_uri(uri))
        .collect()
}

fn sanitized_cell_hyperlink_uri<'a>(
    sanitized_hyperlinks: &'a [Option<String>],
    cell: &CellData,
) -> Option<&'a str> {
    let index = cell.hyperlink? as usize;
    sanitized_hyperlinks.get(index)?.as_deref()
}

fn write_hyperlink_if_changed(
    writer: &mut impl Write,
    active: &mut Option<String>,
    requested: Option<&str>,
) {
    let requested = requested.and_then(sanitized_hyperlink_uri);
    if active.as_deref() == requested.as_deref() {
        return;
    }

    if active.is_some() {
        let _ = writer.write_all(b"\x1b]8;;\x1b\\");
    }
    *active = requested;
    if let Some(uri) = active.as_deref() {
        let _ = write!(writer, "\x1b]8;;{uri}\x1b\\");
    }
}

fn close_hyperlink(writer: &mut impl Write, active: &mut Option<String>) {
    if active.take().is_some() {
        let _ = writer.write_all(b"\x1b]8;;\x1b\\");
    }
}

fn write_cell(
    writer: &mut impl Write,
    cursor_position: Option<(u16, u16)>,
    cell: &CellData,
    last_sgr: &mut String,
    active_hyperlink: &mut Option<String>,
    frame: &FrameData,
) {
    if cell.skip {
        return;
    }

    if let Some(position) = cursor_position {
        write_cursor_position(writer, position);
    }

    let sgr = build_sgr(cell.fg, cell.bg, cell.modifier);
    if sgr != *last_sgr {
        let _ = writer.write_all(sgr.as_bytes());
        *last_sgr = sgr;
    }

    write_hyperlink_if_changed(writer, active_hyperlink, cell_hyperlink_uri(frame, cell));
    let _ = writer.write_all(cell.symbol.as_bytes());
}

/// Writes only the cells that changed between the previous and current frame.
fn cells_visually_equal(
    sanitized_hyperlinks: &[Option<String>],
    cell: &CellData,
    prev_sanitized_hyperlinks: &[Option<String>],
    prev_cell: &CellData,
) -> bool {
    cell.symbol == prev_cell.symbol
        && cell.fg == prev_cell.fg
        && cell.bg == prev_cell.bg
        && cell.modifier == prev_cell.modifier
        && sanitized_cell_hyperlink_uri(sanitized_hyperlinks, cell)
            == sanitized_cell_hyperlink_uri(prev_sanitized_hyperlinks, prev_cell)
    // Skip flag is only for ratatui internal use, not visual.
}

fn write_changed_cells(writer: &mut impl Write, frame: &FrameData, prev: &FrameData) {
    let mut last_sgr = String::new(); // Track last SGR to avoid redundant style changes.
    let mut active_hyperlink = None;
    let sanitized_hyperlinks = sanitized_frame_hyperlinks(frame);
    let prev_sanitized_hyperlinks = sanitized_frame_hyperlinks(prev);

    for row in 0..frame.height {
        let mut invalidated = 0usize;
        let mut to_skip = 0usize;
        // Herdr clients disable host autowrap, so safe cells can advance inline
        // without spilling into adjacent rows during a resize race.
        let mut next_inline_col = None;

        for col in 0..frame.width {
            let idx = (row as usize) * (frame.width as usize) + (col as usize);
            let cell = &frame.cells[idx];
            let prev_cell = &prev.cells[idx];

            if !cell.skip
                && (!cells_visually_equal(
                    &sanitized_hyperlinks,
                    cell,
                    &prev_sanitized_hyperlinks,
                    prev_cell,
                ) || invalidated > 0)
                && to_skip == 0
            {
                let cursor_position =
                    (next_inline_col != Some(col) || invalidated > 0).then_some((col, row));
                write_cell(
                    writer,
                    cursor_position,
                    cell,
                    &mut last_sgr,
                    &mut active_hyperlink,
                    frame,
                );
                next_inline_col = (cell.symbol.is_ascii() && cell_width(cell) == 1)
                    .then_some(col.saturating_add(1));
            }

            to_skip = cell_width(cell).saturating_sub(1);
            let affected_width = cmp::max(cell_width(cell), cell_width(prev_cell));
            invalidated = cmp::max(affected_width, invalidated).saturating_sub(1);
        }
    }

    close_hyperlink(writer, &mut active_hyperlink);

    // Reset style if we wrote anything.
    if !last_sgr.is_empty() {
        let _ = writer.write_all(b"\x1b[0m");
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{CellData, CursorState};

    const WIDE_GRAPHEME: &str = "💡";
    const HALFWIDTH_VOICED_KANA: &str = "ｶ\u{ff9e}";

    fn make_cell(symbol: &str, fg: u32, bg: u32, modifier: u16) -> CellData {
        CellData {
            symbol: symbol.into(),
            fg,
            bg,
            modifier,
            skip: false,
            hyperlink: None,
        }
    }

    fn make_skip_cell(symbol: &str, fg: u32, bg: u32, modifier: u16) -> CellData {
        let mut cell = make_cell(symbol, fg, bg, modifier);
        cell.skip = true;
        cell
    }

    fn make_frame(width: u16, height: u16, cells: Vec<CellData>) -> FrameData {
        FrameData {
            cells,
            width,
            height,
            cursor: None,
            hyperlinks: Vec::new(),
            graphics: Vec::new(),
        }
    }

    fn linked_cell(symbol: &str, index: u32) -> CellData {
        let mut cell = make_cell(symbol, 0, 0, 0);
        cell.hyperlink = Some(index);
        cell
    }

    #[test]
    fn color_to_sgr_fg_named_colors() {
        assert_eq!(color_to_sgr_fg(0x00_00_00_00), "39"); // Reset
        assert_eq!(color_to_sgr_fg(0x00_00_00_01), "30"); // Black
        assert_eq!(color_to_sgr_fg(0x00_00_00_02), "31"); // Red
        assert_eq!(color_to_sgr_fg(0x00_00_00_10), "97"); // White
    }

    #[test]
    fn color_to_sgr_fg_indexed() {
        assert_eq!(color_to_sgr_fg(0x01_00_00_AB), "38;5;171");
    }

    #[test]
    fn color_to_sgr_fg_rgb() {
        assert_eq!(color_to_sgr_fg(0x02_FF_80_40), "38;2;255;128;64");
    }

    #[test]
    fn color_to_sgr_bg_named_colors() {
        assert_eq!(color_to_sgr_bg(0x00_00_00_00), "49"); // Reset
        assert_eq!(color_to_sgr_bg(0x00_00_00_01), "40"); // Black
        assert_eq!(color_to_sgr_bg(0x00_00_00_10), "107"); // White
    }

    #[test]
    fn color_to_sgr_bg_rgb() {
        assert_eq!(color_to_sgr_bg(0x02_FF_80_40), "48;2;255;128;64");
    }

    #[test]
    fn modifier_to_sgr_parts_bold() {
        let parts = modifier_to_sgr_parts(1); // BOLD
        assert!(parts.contains(&"1"));
    }

    #[test]
    fn modifier_to_sgr_parts_italic() {
        let parts = modifier_to_sgr_parts(4); // ITALIC
        assert!(parts.contains(&"3"));
    }

    #[test]
    fn modifier_to_sgr_parts_empty() {
        let parts = modifier_to_sgr_parts(0);
        assert!(parts.is_empty());
    }

    #[test]
    fn build_sgr_produces_valid_sequence() {
        let sgr = build_sgr(0x00_00_00_02, 0x00_00_00_01, 1); // fg=Red, bg=Black, bold
        assert!(sgr.starts_with("\x1b["));
        assert!(sgr.ends_with("m"));
        assert!(sgr.contains("0")); // reset existing style first
        assert!(sgr.contains("1")); // bold
        assert!(sgr.contains("31")); // fg red
        assert!(sgr.contains("40")); // bg black
    }

    #[test]
    fn build_sgr_resets_previous_modifiers_when_cell_is_plain() {
        assert_eq!(build_sgr(0x00_00_00_00, 0x00_00_00_00, 0), "\x1b[0;39;49m");
    }

    #[test]
    fn build_sgr_preserves_curly_underline_style() {
        let modifier = crate::protocol::modifier_to_u16(
            crate::protocol::modifier_with_underline_style(ratatui::style::Modifier::UNDERLINED, 3),
        );

        assert_eq!(
            build_sgr(0x00_00_00_00, 0x00_00_00_00, modifier),
            "\x1b[0;4:3;39;49m"
        );
    }

    #[test]
    fn cells_equal_identical() {
        let a = make_cell("A", 2, 1, 0);
        let b = make_cell("A", 2, 1, 0);
        assert!(cells_equal(&a, &b));
    }

    #[test]
    fn cells_equal_different_symbol() {
        let a = make_cell("A", 2, 1, 0);
        let b = make_cell("B", 2, 1, 0);
        assert!(!cells_equal(&a, &b));
    }

    #[test]
    fn cells_equal_different_color() {
        let a = make_cell("A", 2, 1, 0);
        let b = make_cell("A", 3, 1, 0);
        assert!(!cells_equal(&a, &b));
    }

    /// 契约：帧没有光标时宿主光标隐藏；位置沿用上一次可见位置（夹到帧内），
    /// 没有历史时落在右下角；形状回到终端默认。
    #[test]
    fn resolve_host_cursor_state_without_cursor_hides_at_last_visible_position() {
        let frame = make_frame(10, 4, vec![make_cell(" ", 0, 0, 0); 40]);
        let mut last_visible_cursor = None;
        let fresh = resolve_host_cursor_state(&frame, &mut last_visible_cursor);
        assert!(!fresh.visible);
        assert_eq!(fresh.position, (9, 3));
        assert_eq!(fresh.shape, 0);
        assert_eq!(last_visible_cursor, None);

        let mut last_visible_cursor = Some((30, 30));
        let remembered = resolve_host_cursor_state(&frame, &mut last_visible_cursor);
        assert!(!remembered.visible);
        assert_eq!(remembered.position, (9, 3), "历史位置夹到帧内");
        assert_eq!(remembered.shape, 0);
        assert_eq!(
            last_visible_cursor,
            Some((30, 30)),
            "隐藏帧不改写上一次可见位置"
        );

        let mut last_visible_cursor = Some((2, 1));
        let inside = resolve_host_cursor_state(&frame, &mut last_visible_cursor);
        assert!(!inside.visible);
        assert_eq!(inside.position, (2, 1));
    }

    #[test]
    fn blit_frame_hides_cursor_before_full_redraw_writes() {
        let frame = make_frame(
            2,
            2,
            vec![
                make_cell("H", 0, 0, 0),
                make_cell("i", 0, 0, 0),
                make_cell("!", 0, 0, 0),
                make_cell(" ", 0, 0, 0),
            ],
        );

        let mut output = Vec::new();
        blit_frame_to(&mut output, &frame, None);

        let output_str = String::from_utf8(output).unwrap();
        assert!(
            output_str.starts_with("\x1b[?2026h\x1b[?25l"),
            "should hide cursor inside synchronized frame painting during full redraw"
        );
    }

    #[test]
    fn blit_frame_hides_cursor_before_diff_writes() {
        let prev = make_frame(
            2,
            2,
            vec![
                make_cell("H", 0, 0, 0),
                make_cell("i", 0, 0, 0),
                make_cell("!", 0, 0, 0),
                make_cell(" ", 0, 0, 0),
            ],
        );

        let curr = make_frame(
            2,
            2,
            vec![
                make_cell("X", 0, 0, 0), // Changed
                make_cell("i", 0, 0, 0), // Same
                make_cell("!", 0, 0, 0), // Same
                make_cell(" ", 0, 0, 0), // Same
            ],
        );

        let mut output = Vec::new();
        blit_frame_to(&mut output, &curr, Some(&prev));

        let output_str = String::from_utf8(output).unwrap();
        assert!(
            output_str.starts_with("\x1b[?2026h\x1b[?25l"),
            "should hide cursor inside synchronized frame painting during diff"
        );
    }

    #[test]
    fn blit_frame_wraps_frame_in_synchronized_output() {
        let frame = make_frame(1, 1, vec![make_cell("A", 0, 0, 0)]);

        let mut output = Vec::new();
        blit_frame_to(&mut output, &frame, None);

        let output_str = String::from_utf8(output).unwrap();
        assert!(
            output_str.starts_with("\x1b[?2026h\x1b[?25l"),
            "should begin synchronized output before frame writes"
        );
        let sync_end = output_str
            .find("\x1b[?2026l")
            .expect("should end synchronized output after frame writes");
        assert!(
            sync_end > 0,
            "should end synchronized output after frame writes"
        );
    }

    #[test]
    fn blit_frame_begins_sync_before_hiding_cursor_after_visible_cursor_repeat() {
        let visible = FrameData {
            cells: vec![make_cell("A", 0, 0, 0); 9],
            width: 3,
            height: 3,
            cursor: Some(CursorState {
                x: 2,
                y: 1,
                visible: true,
                shape: 0,
            }),
            hyperlinks: Vec::new(),
            graphics: Vec::new(),
        };
        let mut changed = visible.clone();
        changed.cells[0] = make_cell("B", 0, 0, 0);

        let mut last_visible_cursor = None;
        let mut last_cursor_shape = 0;
        let mut first_output = Vec::new();
        blit_frame_to_with_cursor_memory_and_policy(
            &mut first_output,
            &visible,
            None,
            &mut last_visible_cursor,
            &mut last_cursor_shape,
            true,
            false,
        );

        let mut second_output = Vec::new();
        blit_frame_to_with_cursor_memory_and_policy(
            &mut second_output,
            &changed,
            Some(&visible),
            &mut last_visible_cursor,
            &mut last_cursor_shape,
            true,
            false,
        );

        let second_output_str = std::str::from_utf8(&second_output).unwrap();
        assert!(
            second_output_str.starts_with("\x1b[?2026h\x1b[?25l"),
            "next frame should enter synchronized output before hiding the cursor"
        );

        let hide = second_output_str
            .find("\x1b[?25l")
            .expect("second frame should hide cursor before painting");
        let first_paint = second_output_str
            .find("\x1b[1;1H")
            .expect("second frame should paint changed cell");
        assert!(
            hide < first_paint,
            "cursor should still hide before painting"
        );

        first_output.extend_from_slice(&second_output);
        let combined = String::from_utf8(first_output).unwrap();
        assert!(
            combined.contains("\x1b[?2026l\x1b[2;3H\x1b[?25h\x1b[?2026h\x1b[?25l"),
            "post-sync cursor repeat should be followed by a synchronized cursor hide"
        );
    }

    #[test]
    fn blit_frame_can_repeat_final_cursor_state_after_synchronized_output() {
        let frame = FrameData {
            cells: vec![make_cell("A", 0, 0, 0); 9],
            width: 3,
            height: 3,
            cursor: Some(CursorState {
                x: 2,
                y: 1,
                visible: true,
                shape: 0,
            }),
            hyperlinks: Vec::new(),
            graphics: Vec::new(),
        };

        let mut last_visible_cursor = None;
        let mut last_cursor_shape = 0;
        let mut output = Vec::new();
        blit_frame_to_with_cursor_memory_and_policy(
            &mut output,
            &frame,
            None,
            &mut last_visible_cursor,
            &mut last_cursor_shape,
            true,
            false,
        );

        let output_str = String::from_utf8(output).unwrap();
        let sync_end = output_str
            .find("\x1b[?2026l")
            .expect("should end synchronized output");
        let trailing_cursor = &output_str[sync_end + "\x1b[?2026l".len()..];
        assert_eq!(
            trailing_cursor, "\x1b[2;3H\x1b[?25h",
            "should expose only the final cursor state after synchronized output"
        );
    }

    #[test]
    fn blit_frame_can_skip_final_cursor_state_after_synchronized_output() {
        let frame = FrameData {
            cells: vec![make_cell("A", 0, 0, 0); 9],
            width: 3,
            height: 3,
            cursor: Some(CursorState {
                x: 2,
                y: 1,
                visible: true,
                shape: 0,
            }),
            hyperlinks: Vec::new(),
            graphics: Vec::new(),
        };

        let mut last_visible_cursor = None;
        let mut last_cursor_shape = 0;
        let mut output = Vec::new();
        blit_frame_to_with_cursor_memory_and_policy(
            &mut output,
            &frame,
            None,
            &mut last_visible_cursor,
            &mut last_cursor_shape,
            false,
            false,
        );

        let output_str = String::from_utf8(output).unwrap();
        let sync_end = output_str
            .find("\x1b[?2026l")
            .expect("should end synchronized output");
        let trailing_cursor = &output_str[sync_end + "\x1b[?2026l".len()..];
        assert_eq!(
            trailing_cursor, "",
            "should not expose a post-sync cursor repeat when the target terminal flickers on it"
        );
    }

    #[test]
    fn drawn_cursor_reverses_visible_cursor_cell() {
        let frame = FrameData {
            cells: vec![make_cell("A", 0, 0, 0); 9],
            width: 3,
            height: 3,
            cursor: Some(CursorState {
                x: 2,
                y: 1,
                visible: true,
                shape: 6,
            }),
            hyperlinks: Vec::new(),
            graphics: Vec::new(),
        };
        let drawn = frame_with_drawn_cursor(frame.clone());

        assert_eq!(drawn.cells[5].modifier, REVERSED_MODIFIER);
        assert_eq!(frame.cells[5].modifier, 0);

        let encoded = BlitEncoder::new().encode_with_suppressed_visible_cursor(&drawn, false);
        let output_str = String::from_utf8(encoded.bytes).unwrap();

        assert!(
            output_str.contains("\x1b[2;3H\x1b[6 q\x1b[?25l"),
            "drawn cursor mode should park the host cursor hidden at the focused cursor position"
        );
        assert!(
            !output_str.contains("\x1b[?25h"),
            "drawn cursor mode should not show the host cursor"
        );
        assert!(
            output_str.contains("\x1b[0;7;39;49mA"),
            "drawn cursor should be emitted as reverse-video cell content"
        );
    }

    #[test]
    fn drawn_cursor_ignores_hidden_cursor() {
        let frame = FrameData {
            cells: vec![make_cell("A", 0, 0, 0)],
            width: 1,
            height: 1,
            cursor: Some(CursorState {
                x: 0,
                y: 0,
                visible: false,
                shape: 0,
            }),
            hyperlinks: Vec::new(),
            graphics: Vec::new(),
        };

        assert_eq!(frame_with_drawn_cursor(frame.clone()), frame);
    }

    #[test]
    fn blit_frame_emits_cursor_shape_before_visibility_without_touching_ime_anchor() {
        let frame = FrameData {
            cells: vec![make_cell("A", 0, 0, 0)],
            width: 1,
            height: 1,
            cursor: Some(CursorState {
                x: 0,
                y: 0,
                visible: true,
                shape: 6,
            }),
            hyperlinks: Vec::new(),
            graphics: Vec::new(),
        };

        let mut last_visible_cursor = None;
        let mut last_cursor_shape = 0;
        let mut output = Vec::new();
        blit_frame_to_with_cursor_memory_and_policy(
            &mut output,
            &frame,
            None,
            &mut last_visible_cursor,
            &mut last_cursor_shape,
            true,
            false,
        );

        let output_str = String::from_utf8(output).unwrap();
        let final_cursor = output_str
            .find("\x1b[1;1H\x1b[6 q\x1b[?25h")
            .expect("should set cursor shape before showing cursor");
        let sync_end = output_str
            .find("\x1b[?2026l")
            .expect("should end synchronized output");
        assert!(
            final_cursor < sync_end,
            "shape should be part of the synchronized final cursor state"
        );
        let trailing_cursor = &output_str[sync_end + "\x1b[?2026l".len()..];
        assert_eq!(
            trailing_cursor, "\x1b[1;1H\x1b[?25h",
            "IME anchor update should preserve the existing position/visibility-only contract"
        );
    }

    #[test]
    fn blit_frame_repeats_explicit_hidden_cursor_anchor_after_synchronized_output() {
        let visible = FrameData {
            cells: vec![make_cell("A", 0, 0, 0); 9],
            width: 3,
            height: 3,
            cursor: Some(CursorState {
                x: 0,
                y: 0,
                visible: true,
                shape: 0,
            }),
            hyperlinks: Vec::new(),
            graphics: Vec::new(),
        };
        let hidden = FrameData {
            cells: vec![make_cell("B", 0, 0, 0); 9],
            width: 3,
            height: 3,
            cursor: Some(CursorState {
                x: 2,
                y: 1,
                visible: false,
                shape: 0,
            }),
            hyperlinks: Vec::new(),
            graphics: Vec::new(),
        };
        let mut last_visible_cursor = None;
        let mut last_cursor_shape = 0;
        let mut output = Vec::new();

        blit_frame_to_with_cursor_memory_and_policy(
            &mut output,
            &visible,
            None,
            &mut last_visible_cursor,
            &mut last_cursor_shape,
            true,
            false,
        );
        output.clear();
        blit_frame_to_with_cursor_memory_and_policy(
            &mut output,
            &hidden,
            Some(&visible),
            &mut last_visible_cursor,
            &mut last_cursor_shape,
            true,
            false,
        );

        let output_str = String::from_utf8(output).unwrap();
        let sync_end = output_str
            .find("\x1b[?2026l")
            .expect("should end synchronized output");
        let trailing_cursor = &output_str[sync_end + "\x1b[?2026l".len()..];
        assert_eq!(
            trailing_cursor, "\x1b[2;3H\x1b[?25l",
            "should repeat the explicit hidden cursor position while preserving visibility"
        );
    }

    #[test]
    fn blit_frame_emits_osc8_for_linked_cells() {
        let mut frame = make_frame(
            3,
            1,
            vec![
                linked_cell("L", 0),
                linked_cell("i", 0),
                make_cell("!", 0, 0, 0),
            ],
        );
        frame.hyperlinks.push("https://example.com".to_owned());

        let mut output = Vec::new();
        blit_frame_to(&mut output, &frame, None);

        let output_str = String::from_utf8(output).unwrap();
        assert!(output_str.contains("\x1b]8;;https://example.com\x1b\\L"));
        assert!(output_str.contains('i'));
        assert!(output_str.contains("\x1b]8;;\x1b\\"));
    }

    #[test]
    fn blit_frame_sanitizes_hyperlink_uris() {
        let mut frame = make_frame(1, 1, vec![linked_cell("L", 0)]);
        frame
            .hyperlinks
            .push("https://exa\x1b\x07mple.com".to_owned());

        let mut output = Vec::new();
        blit_frame_to(&mut output, &frame, None);

        let output_str = String::from_utf8(output).unwrap();
        assert!(output_str.contains("\x1b]8;;https://example.com\x1b\\L"));
    }

    #[test]
    fn blit_frame_first_frame_produces_output() {
        let frame = make_frame(
            2,
            2,
            vec![
                make_cell("H", 0, 0, 0),
                make_cell("i", 0, 0, 0),
                make_cell("!", 0, 0, 0),
                make_cell(" ", 0, 0, 0),
            ],
        );

        let mut output = Vec::new();
        blit_frame_to(&mut output, &frame, None);

        let output_str = String::from_utf8(output).unwrap();
        // Full redraw should start with clear screen.
        assert!(
            output_str.contains("\x1b[2J"),
            "full redraw should clear screen"
        );
        assert!(
            output_str.contains('H') || output_str.contains('i'),
            "should contain cell content"
        );
    }

    #[test]
    fn blit_frame_diff_only_writes_changed_cells() {
        let prev = make_frame(
            2,
            2,
            vec![
                make_cell("H", 0, 0, 0),
                make_cell("i", 0, 0, 0),
                make_cell("!", 0, 0, 0),
                make_cell(" ", 0, 0, 0),
            ],
        );

        // Only the first cell changed.
        let curr = make_frame(
            2,
            2,
            vec![
                make_cell("X", 0, 0, 0), // Changed
                make_cell("i", 0, 0, 0), // Same
                make_cell("!", 0, 0, 0), // Same
                make_cell(" ", 0, 0, 0), // Same
            ],
        );

        let mut output = Vec::new();
        blit_frame_to(&mut output, &curr, Some(&prev));

        let output_str = String::from_utf8(output).unwrap();
        // Diff should NOT clear the screen.
        assert!(
            !output_str.contains("\x1b[2J"),
            "diff should not clear screen"
        );
        // Should contain the changed cell content.
        assert!(output_str.contains('X'), "should contain changed cell 'X'");
    }

    #[test]
    fn scroll_sized_ascii_shift_batches_changed_cells_by_row() {
        const WIDTH: u16 = 140;
        const HEIGHT: u16 = 50;
        let prev = make_frame(
            WIDTH,
            HEIGHT,
            vec![make_cell("A", 0, 0, 0); usize::from(WIDTH) * usize::from(HEIGHT)],
        );
        let curr = make_frame(
            WIDTH,
            HEIGHT,
            vec![make_cell("B", 0, 0, 0); usize::from(WIDTH) * usize::from(HEIGHT)],
        );

        let mut output = Vec::new();
        blit_frame_to(&mut output, &curr, Some(&prev));

        let cup_count = output.iter().filter(|&&byte| byte == b'H').count();
        assert!(
            cup_count <= usize::from(HEIGHT) + 2,
            "one dense scroll frame should need at most one CUP per row plus cursor anchors, got {cup_count}"
        );
        assert!(
            output.len() <= 16_290,
            "one dense scroll frame should stay below 25% of the 65,161-byte live baseline, got {} bytes",
            output.len()
        );
    }

    #[test]
    fn batched_ascii_diff_replays_to_current_frame() {
        let prev = make_frame(4, 3, vec![make_cell("A", 0, 0, 0); 12]);
        let curr = make_frame(4, 3, vec![make_cell("B", 0, 0, 0); 12]);
        let mut terminal = crate::ghostty::Terminal::new(4, 3, 0).unwrap();

        let mut initial = Vec::new();
        blit_frame_to(&mut initial, &prev, None);
        terminal.write(&initial);

        let mut diff = Vec::new();
        blit_frame_to(&mut diff, &curr, Some(&prev));
        terminal.write(&diff);

        for row in 0..3 {
            for col in 0..4 {
                let (_, graphemes) = terminal.screen_cell(col, row).unwrap();
                assert_eq!(graphemes, vec![u32::from('B')]);
            }
        }
    }

    #[test]
    fn encoder_size_change_repaints_without_clearing() {
        let prev = make_frame(2, 2, vec![make_cell("A", 0, 0, 0); 4]);
        let curr = make_frame(3, 2, vec![make_cell("B", 0, 0, 0); 6]);
        let mut encoder = BlitEncoder::new();
        let initial = encoder.encode(&prev, false);
        encoder.commit(prev, initial);

        let encoded = encoder.encode(&curr, false);
        assert!(encoded.full);
        let output = String::from_utf8(encoded.bytes).unwrap();

        assert!(!output.contains("\x1b[2J"));
        assert!(output.bytes().filter(|byte| *byte == b'B').count() >= 6);
    }

    #[test]
    fn encoder_forced_repaint_writes_all_cells_without_clearing() {
        let frame = make_frame(3, 2, vec![make_cell("A", 0, 0, 0); 6]);
        let mut encoder = BlitEncoder::new();
        let initial = encoder.encode(&frame, false);
        encoder.commit(frame.clone(), initial);

        let encoded = encoder.encode(&frame, true);
        assert!(encoded.full);
        let output = String::from_utf8(encoded.bytes).unwrap();

        assert!(!output.contains("\x1b[2J"));
        assert!(output.bytes().filter(|byte| *byte == b'A').count() >= 6);
    }

    #[test]
    fn retained_patch_matches_full_diff_and_updates_the_encoder_baseline() {
        let previous = make_frame(
            4,
            2,
            vec![
                make_cell("a", 0, 0, 0),
                make_cell("b", 0, 0, 0),
                make_cell("c", 0, 0, 0),
                make_cell("d", 0, 0, 0),
                make_cell("e", 0, 0, 0),
                make_cell("f", 0, 0, 0),
                make_cell("g", 0, 0, 0),
                make_cell("h", 0, 0, 0),
            ],
        );
        let mut encoder = BlitEncoder::new();
        let initial = encoder.encode(&previous, false);
        encoder.commit(previous.clone(), initial);

        let rows = vec![PaneSurfacePatchRow {
            x: 0,
            y: 1,
            cells: vec![
                make_cell("E", 0, 0, 0),
                make_cell("f", 0, 0, 0),
                make_cell("G", 0, 0, 0),
                make_cell("h", 0, 0, 0),
            ],
        }];
        let cursor = Some(CursorState {
            x: 3,
            y: 1,
            visible: true,
            shape: 2,
        });
        let mut expected = previous;
        expected.cells[4..8].clone_from_slice(&rows[0].cells);
        expected.cursor = cursor.clone();

        let full_diff = encoder.encode(&expected, false);
        let patch = encoder
            .encode_patch(&rows, cursor.clone(), false)
            .expect("valid retained patch");
        assert_eq!(patch.bytes, full_diff.bytes);
        assert!(encoder.commit_patch(&rows, cursor, patch));
        assert!(encoder.is_current(&expected));
    }

    #[test]
    fn retained_patch_width_transition_matches_full_diff_with_following_cell() {
        let previous = make_frame(
            3,
            1,
            vec![
                make_cell("界", 0, 0, 0),
                make_cell("z", 0, 0, 0),
                make_cell("q", 0, 0, 0),
            ],
        );
        let mut encoder = BlitEncoder::new();
        let initial = encoder.encode(&previous, false);
        encoder.commit(previous.clone(), initial);

        let rows = vec![PaneSurfacePatchRow {
            x: 0,
            y: 0,
            cells: vec![make_cell("x", 0, 0, 0), make_cell("z", 0, 0, 0)],
        }];
        let mut expected = previous;
        expected.cells[0..2].clone_from_slice(&rows[0].cells);

        let full_diff = encoder.encode(&expected, false);
        let patch = encoder
            .encode_patch(&rows, None, false)
            .expect("valid retained patch");
        assert_eq!(patch.bytes, full_diff.bytes);
    }

    #[test]
    fn retained_patch_rejects_overlapping_rows() {
        let frame = make_frame(
            3,
            1,
            vec![
                make_cell("a", 0, 0, 0),
                make_cell("b", 0, 0, 0),
                make_cell("c", 0, 0, 0),
            ],
        );
        let mut encoder = BlitEncoder::new();
        let initial = encoder.encode(&frame, false);
        encoder.commit(frame, initial);
        let rows = vec![
            PaneSurfacePatchRow {
                x: 0,
                y: 0,
                cells: vec![make_cell("A", 0, 0, 0), make_cell("B", 0, 0, 0)],
            },
            PaneSurfacePatchRow {
                x: 1,
                y: 0,
                cells: vec![make_cell("C", 0, 0, 0)],
            },
        ];

        assert!(encoder.encode_patch(&rows, None, false).is_none());
    }

    #[test]
    fn retained_patch_preserves_the_client_drawn_cursor_overlay() {
        let mut previous = make_frame(
            3,
            1,
            vec![
                make_cell("a", 0, 0, 0),
                make_cell("b", 0, 0, 0),
                make_cell("c", 0, 0, 0),
            ],
        );
        previous.cursor = Some(CursorState {
            x: 0,
            y: 0,
            visible: true,
            shape: 0,
        });
        let previous_drawn = frame_with_drawn_cursor(previous.clone());
        let mut encoder = BlitEncoder::new();
        let initial = encoder.encode_with_suppressed_visible_cursor(&previous_drawn, false);
        encoder.commit(previous_drawn, initial);

        let rows = vec![PaneSurfacePatchRow {
            x: 0,
            y: 0,
            cells: vec![
                make_cell("A", 0, 0, 0),
                make_cell("b", 0, 0, 0),
                make_cell("c", 0, 0, 0),
            ],
        }];
        let cursor = Some(CursorState {
            x: 1,
            y: 0,
            visible: true,
            shape: 0,
        });
        let drawn_rows = encoder
            .patch_rows_with_drawn_cursor(&rows, cursor.as_ref())
            .expect("drawn cursor patch rows");
        let mut expected = previous;
        expected.cells[0..3].clone_from_slice(&rows[0].cells);
        expected.cursor = cursor.clone();
        let expected = frame_with_drawn_cursor(expected);

        let full_diff = encoder.encode_with_suppressed_visible_cursor(&expected, false);
        let patch = encoder
            .encode_patch(&drawn_rows, cursor.clone(), true)
            .expect("valid drawn cursor patch");
        assert_eq!(patch.bytes, full_diff.bytes);
        assert!(encoder.commit_patch(&drawn_rows, cursor, patch));
        assert!(encoder.is_current(&expected));
    }

    #[test]
    fn blit_frame_positions_cursor() {
        let frame = FrameData {
            cells: vec![make_cell("A", 0, 0, 0)],
            width: 1,
            height: 1,
            cursor: Some(CursorState {
                x: 0,
                y: 0,
                visible: true,
                shape: 0,
            }),
            hyperlinks: Vec::new(),
            graphics: Vec::new(),
        };

        let mut output = Vec::new();
        blit_frame_to(&mut output, &frame, None);

        let output_str = String::from_utf8(output).unwrap();
        assert!(
            output_str.contains("\x1b[1;1H"),
            "should position cursor at (1,1)"
        );
    }

    #[test]
    fn blit_frame_hides_cursor_when_invisible() {
        let frame = FrameData {
            cells: vec![make_cell("A", 0, 0, 0)],
            width: 1,
            height: 1,
            cursor: Some(CursorState {
                x: 0,
                y: 0,
                visible: false,
                shape: 0,
            }),
            hyperlinks: Vec::new(),
            graphics: Vec::new(),
        };

        let mut output = Vec::new();
        blit_frame_to(&mut output, &frame, None);

        let output_str = String::from_utf8(output).unwrap();
        assert!(
            output_str.contains("\x1b[?25l"),
            "should hide cursor when invisible"
        );
    }

    #[test]
    fn blit_frame_no_cursor_hides_cursor() {
        let frame = FrameData {
            cells: vec![make_cell("A", 0, 0, 0)],
            width: 1,
            height: 1,
            cursor: None,
            hyperlinks: Vec::new(),
            graphics: Vec::new(),
        };

        let mut output = Vec::new();
        blit_frame_to(&mut output, &frame, None);

        let output_str = String::from_utf8(output).unwrap();
        assert!(
            output_str.contains("\x1b[?25l"),
            "should hide cursor when no cursor state"
        );
    }

    #[test]
    fn blit_frame_restores_cursor_visibility() {
        // First frame: cursor hidden.
        let prev = FrameData {
            cells: vec![make_cell("A", 0, 0, 0)],
            width: 1,
            height: 1,
            cursor: Some(CursorState {
                x: 0,
                y: 0,
                visible: false,
                shape: 0,
            }),
            hyperlinks: Vec::new(),
            graphics: Vec::new(),
        };

        let mut output = Vec::new();
        blit_frame_to(&mut output, &prev, None);
        assert!(
            String::from_utf8(output).unwrap().contains("\x1b[?25l"),
            "first frame should hide cursor"
        );

        // Second frame: cursor visible — should restore visibility.
        let curr = FrameData {
            cells: vec![make_cell("B", 0, 0, 0)],
            width: 1,
            height: 1,
            cursor: Some(CursorState {
                x: 0,
                y: 0,
                visible: true,
                shape: 0,
            }),
            hyperlinks: Vec::new(),
            graphics: Vec::new(),
        };

        let mut output = Vec::new();
        blit_frame_to(&mut output, &curr, Some(&prev));
        let output_str = String::from_utf8(output).unwrap();
        assert!(
            output_str.contains("\x1b[?25h"),
            "second frame should restore cursor visibility with ?25h"
        );
        assert!(
            output_str.contains("\x1b[1;1H"),
            "should position cursor before showing it"
        );
    }

    #[test]
    fn blit_frame_positions_cursor_before_showing_it() {
        let prev = FrameData {
            cells: vec![make_cell("A", 0, 0, 0); 9],
            width: 3,
            height: 3,
            cursor: Some(CursorState {
                x: 0,
                y: 0,
                visible: true,
                shape: 0,
            }),
            hyperlinks: Vec::new(),
            graphics: Vec::new(),
        };
        let mut curr = prev.clone();
        curr.cells[0] = make_cell("B", 0, 0, 0);
        curr.cursor = Some(CursorState {
            x: 2,
            y: 2,
            visible: true,
            shape: 0,
        });

        let mut output = Vec::new();
        blit_frame_to(&mut output, &curr, Some(&prev));
        let output_str = String::from_utf8(output).unwrap();
        let final_move = output_str
            .rfind("\x1b[3;3H")
            .expect("should move cursor to final position");
        let show = output_str
            .rfind("\x1b[?25h")
            .expect("should show cursor after positioning it");

        assert!(
            final_move < show,
            "should move cursor to final position before showing it"
        );
    }

    #[test]
    fn blit_frame_parks_hidden_cursor_at_last_visible_position() {
        let visible = FrameData {
            cells: vec![make_cell("A", 0, 0, 0); 9],
            width: 3,
            height: 3,
            cursor: Some(CursorState {
                x: 1,
                y: 1,
                visible: true,
                shape: 0,
            }),
            hyperlinks: Vec::new(),
            graphics: Vec::new(),
        };
        let hidden = FrameData {
            cells: vec![make_cell("B", 0, 0, 0); 9],
            width: 3,
            height: 3,
            cursor: None,
            hyperlinks: Vec::new(),
            graphics: Vec::new(),
        };
        let mut last_visible_cursor = None;
        let mut last_cursor_shape = 0;
        let mut output = Vec::new();

        blit_frame_to_with_cursor_memory(
            &mut output,
            &visible,
            None,
            &mut last_visible_cursor,
            &mut last_cursor_shape,
            false,
        );
        output.clear();
        blit_frame_to_with_cursor_memory(
            &mut output,
            &hidden,
            Some(&visible),
            &mut last_visible_cursor,
            &mut last_cursor_shape,
            false,
        );

        let output_str = String::from_utf8(output).unwrap();
        let park = output_str
            .rfind("\x1b[2;2H")
            .expect("should park hidden cursor at last visible position");
        let hide = output_str
            .rfind("\x1b[?25l")
            .expect("should keep hidden cursor hidden");
        assert!(park < hide, "should park cursor before hiding it");
    }

    #[test]
    fn blit_frame_parks_hidden_cursor_at_bottom_right_without_history() {
        let frame = FrameData {
            cells: vec![make_cell("A", 0, 0, 0); 6],
            width: 3,
            height: 2,
            cursor: None,
            hyperlinks: Vec::new(),
            graphics: Vec::new(),
        };
        let mut last_visible_cursor = None;
        let mut last_cursor_shape = 0;
        let mut output = Vec::new();

        blit_frame_to_with_cursor_memory(
            &mut output,
            &frame,
            None,
            &mut last_visible_cursor,
            &mut last_cursor_shape,
            false,
        );

        let output_str = String::from_utf8(output).unwrap();
        assert!(
            output_str.contains("\x1b[2;3H\x1b[?25l"),
            "should park hidden cursor at bottom-right before ending the frame"
        );
    }

    #[test]
    fn blit_frame_hides_previous_visible_cursor_when_next_frame_has_none() {
        let prev = FrameData {
            cells: vec![make_cell("A", 0, 0, 0)],
            width: 1,
            height: 1,
            cursor: Some(CursorState {
                x: 0,
                y: 0,
                visible: true,
                shape: 0,
            }),
            hyperlinks: Vec::new(),
            graphics: Vec::new(),
        };
        let curr = FrameData {
            cells: vec![make_cell("B", 0, 0, 0)],
            width: 1,
            height: 1,
            cursor: None,
            hyperlinks: Vec::new(),
            graphics: Vec::new(),
        };

        let mut output = Vec::new();
        blit_frame_to(&mut output, &curr, Some(&prev));

        assert!(
            String::from_utf8(output).unwrap().contains("\x1b[?25l"),
            "diff redraw should hide a previously visible cursor when the next frame has none"
        );
    }

    #[test]
    fn full_redraw_skips_trailing_cells_covered_by_wide_graphemes() {
        let frame = FrameData {
            cells: vec![
                make_cell(WIDE_GRAPHEME, 0, 0, 0),
                make_cell(" ", 0, 0, 0),
                make_cell("Z", 0, 0, 0),
            ],
            width: 3,
            height: 1,
            cursor: None,
            hyperlinks: Vec::new(),
            graphics: Vec::new(),
        };

        let mut output = Vec::new();
        blit_frame_to(&mut output, &frame, None);
        let output_str = String::from_utf8(output).unwrap();

        assert!(output_str.contains("\x1b[1;1H"));
        assert!(!output_str.contains("\x1b[1;2H"));
        assert!(output_str.contains("\x1b[1;3H"));
    }

    #[test]
    fn full_redraw_skips_trailing_cells_covered_by_halfwidth_voiced_kana() {
        let frame = FrameData {
            cells: vec![
                make_cell(HALFWIDTH_VOICED_KANA, 0, 0, 0),
                make_skip_cell(" ", 0, 0, 0),
                make_cell("Z", 0, 0, 0),
            ],
            width: 3,
            height: 1,
            cursor: None,
            hyperlinks: Vec::new(),
            graphics: Vec::new(),
        };

        let mut output = Vec::new();
        blit_frame_to(&mut output, &frame, None);
        let output_str = String::from_utf8(output).unwrap();

        assert!(output_str.contains("\x1b[1;1H"));
        assert!(!output_str.contains("\x1b[1;2H"));
        assert!(output_str.contains("\x1b[1;3H"));
    }

    #[test]
    fn diff_redraw_reveals_cells_hidden_by_previous_wide_graphemes() {
        let prev = FrameData {
            cells: vec![
                make_cell(WIDE_GRAPHEME, 0, 0, 0),
                make_cell(" ", 0, 0, 0),
                make_cell("Z", 0, 0, 0),
            ],
            width: 3,
            height: 1,
            cursor: None,
            hyperlinks: Vec::new(),
            graphics: Vec::new(),
        };
        let curr = FrameData {
            cells: vec![
                make_cell("A", 0, 0, 0),
                make_cell(" ", 0, 0, 0),
                make_cell("Z", 0, 0, 0),
            ],
            width: 3,
            height: 1,
            cursor: None,
            hyperlinks: Vec::new(),
            graphics: Vec::new(),
        };

        let mut output = Vec::new();
        blit_frame_to(&mut output, &curr, Some(&prev));
        let output_str = String::from_utf8(output).unwrap();

        assert!(output_str.contains("\x1b[1;1H"));
        assert!(
            output_str.contains("\x1b[1;2H"),
            "cells hidden by a previous wide grapheme must be redrawn when they become visible"
        );
    }

    #[test]
    fn diff_redraw_skips_new_trailing_cells_covered_by_wide_graphemes() {
        let prev = FrameData {
            cells: vec![
                make_cell("A", 0, 0, 0),
                make_cell("B", 0, 0, 0),
                make_cell("Z", 0, 0, 0),
            ],
            width: 3,
            height: 1,
            cursor: None,
            hyperlinks: Vec::new(),
            graphics: Vec::new(),
        };
        let curr = FrameData {
            cells: vec![
                make_cell(WIDE_GRAPHEME, 0, 0, 0),
                make_cell(" ", 0, 0, 0),
                make_cell("Z", 0, 0, 0),
            ],
            width: 3,
            height: 1,
            cursor: None,
            hyperlinks: Vec::new(),
            graphics: Vec::new(),
        };

        let mut output = Vec::new();
        blit_frame_to(&mut output, &curr, Some(&prev));
        let output_str = String::from_utf8(output).unwrap();

        assert!(output_str.contains("\x1b[1;1H"));
        assert!(!output_str.contains("\x1b[1;2H"));
    }

    #[test]
    fn diff_redraw_reveals_cells_hidden_by_previous_halfwidth_voiced_kana() {
        let prev = FrameData {
            cells: vec![
                make_cell(HALFWIDTH_VOICED_KANA, 0, 0, 0),
                make_skip_cell(" ", 0, 0, 0),
                make_cell("Z", 0, 0, 0),
            ],
            width: 3,
            height: 1,
            cursor: None,
            hyperlinks: Vec::new(),
            graphics: Vec::new(),
        };
        let curr = FrameData {
            cells: vec![
                make_cell("A", 0, 0, 0),
                make_cell(" ", 0, 0, 0),
                make_cell("Z", 0, 0, 0),
            ],
            width: 3,
            height: 1,
            cursor: None,
            hyperlinks: Vec::new(),
            graphics: Vec::new(),
        };

        let mut output = Vec::new();
        blit_frame_to(&mut output, &curr, Some(&prev));
        let output_str = String::from_utf8(output).unwrap();

        assert!(output_str.contains("\x1b[1;1H"));
        assert!(
            output_str.contains("\x1b[1;2H"),
            "cells hidden by a previous halfwidth voiced kana must be redrawn when visible"
        );
    }

    // -----------------------------------------------------------------------
    // 切换标签闪烁（计划 2.1）：块外 IME 锚点重复按宿主判定并显式注入编码器
    // -----------------------------------------------------------------------

    fn host_env(
        term_program: Option<&str>,
        term: Option<&str>,
        kitty_window: bool,
    ) -> ImeAnchorHostEnv {
        ImeAnchorHostEnv {
            term_program: term_program.map(str::to_owned),
            term: term.map(str::to_owned),
            kitty_window,
            in_multiplexer: false,
        }
    }

    fn multiplexed(mut host: ImeAnchorHostEnv) -> ImeAnchorHostEnv {
        host.in_multiplexer = true;
        host
    }

    /// 切换标签量级的两帧：同尺寸、整屏内容全变、光标可见且位置不变。
    fn tab_switch_frames() -> (FrameData, FrameData) {
        let cursor = Some(CursorState {
            x: 3,
            y: 2,
            visible: true,
            shape: 0,
        });
        let mut before = make_frame(24, 8, vec![make_cell("A", 0, 0, 0); 24 * 8]);
        before.cursor = cursor.clone();
        let mut after = make_frame(24, 8, vec![make_cell("B", 0, 0, 0); 24 * 8]);
        after.cursor = cursor;
        (before, after)
    }

    /// 块外锚点的确切字节：最终光标 (3,2) 的 CUP + 显示光标。
    const TAB_SWITCH_POST_SYNC_ANCHOR: &str = "\x1b[3;4H\x1b[?25h";

    /// 经生产入口 `BlitEncoder::encode`：先提交首帧，再返回切换帧的字节。
    fn encode_tab_switch_with_encoder(encoder: &mut BlitEncoder) -> String {
        let (before, after) = tab_switch_frames();
        let first = encoder.encode(&before, false);
        encoder.commit(before, first);
        let second = encoder.encode(&after, false);
        String::from_utf8(second.bytes).unwrap()
    }

    fn assert_single_sync_block_without_trailing_bytes(output: &str) {
        assert_eq!(
            output.matches("\x1b[?2026h").count(),
            1,
            "恰一个同步块开头: {output:?}"
        );
        assert_eq!(
            output.matches("\x1b[?2026l").count(),
            1,
            "恰一个同步块结尾: {output:?}"
        );
        assert!(output.starts_with("\x1b[?2026h"), "块前 0 字节: {output:?}");
        assert!(
            output.ends_with("\x1b[?25h\x1b[?2026l"),
            "块内以 ?25h 结尾且块外 0 字节: {output:?}"
        );
        assert!(!output.contains("\x1b[2J"), "切换标签不清屏: {output:?}");
    }

    fn assert_only_post_sync_anchor_outside_block(output: &str) {
        assert_eq!(output.matches("\x1b[?2026l").count(), 1);
        let sync_end = output.find("\x1b[?2026l").expect("同步块结尾");
        let trailing = &output[sync_end + "\x1b[?2026l".len()..];
        assert_eq!(
            trailing, TAB_SWITCH_POST_SYNC_ANCHOR,
            "块外只剩最终光标锚点: {output:?}"
        );
        assert!(!output.contains("\x1b[2J"));
    }

    #[test]
    fn tab_switch_frames_stay_inside_one_sync_block_when_auto_resolves_to_no_repeat() {
        // auto + WezTerm（块外补发会被看见成闪烁）→ 经 BlitEncoder::encode 的切换帧块外 0 字节。
        let policy = resolve_ime_anchor_repeat(
            ImeAnchorRepeatMode::Auto,
            &host_env(Some("WezTerm"), Some("xterm-256color"), false),
        );
        assert!(!policy, "auto 且宿主会把补发看见成闪烁 → 不重复块外锚点");
        let mut encoder = BlitEncoder::with_ime_anchor_repeat(policy);
        assert_single_sync_block_without_trailing_bytes(&encode_tab_switch_with_encoder(
            &mut encoder,
        ));
    }

    #[test]
    fn tab_switch_frames_stay_inside_one_sync_block_when_repeat_is_never() {
        let policy = resolve_ime_anchor_repeat(
            ImeAnchorRepeatMode::Never,
            &host_env(None, Some("xterm-256color"), false),
        );
        assert!(!policy, "never 在未知宿主上也不重复");
        let mut encoder = BlitEncoder::with_ime_anchor_repeat(policy);
        assert_single_sync_block_without_trailing_bytes(&encode_tab_switch_with_encoder(
            &mut encoder,
        ));
    }

    #[test]
    fn tab_switch_frames_keep_the_post_sync_anchor_for_always_and_unknown_hosts() {
        let policies = [
            resolve_ime_anchor_repeat(
                ImeAnchorRepeatMode::Always,
                &host_env(Some("WezTerm"), Some("xterm-256color"), false),
            ),
            resolve_ime_anchor_repeat(
                ImeAnchorRepeatMode::Auto,
                &host_env(None, Some("xterm-256color"), false),
            ),
        ];
        for policy in policies {
            assert!(policy, "always / 未知宿主保留旧行为");
            let mut encoder = BlitEncoder::with_ime_anchor_repeat(policy);
            assert_only_post_sync_anchor_outside_block(&encode_tab_switch_with_encoder(
                &mut encoder,
            ));
        }
    }

    #[test]
    fn injected_anchor_policy_changes_exactly_the_post_sync_anchor_bytes() {
        // 扩展证据（确定性替代 HERDR_RENDER_PROF 前后对比）：两种策略编码同一对切换帧，
        // 字节差恰好是块外锚点，块内内容逐字节一致；策略不会影响 diff 本身。
        let mut with_repeat = BlitEncoder::with_ime_anchor_repeat(true);
        let mut without_repeat = BlitEncoder::with_ime_anchor_repeat(false);
        let repeated = encode_tab_switch_with_encoder(&mut with_repeat);
        let silent = encode_tab_switch_with_encoder(&mut without_repeat);
        assert_eq!(
            repeated,
            format!("{silent}{TAB_SWITCH_POST_SYNC_ANCHOR}"),
            "策略只增减块外锚点"
        );
        assert_eq!(
            repeated.len() - silent.len(),
            TAB_SWITCH_POST_SYNC_ANCHOR.len()
        );
    }

    #[test]
    fn encode_patch_honours_the_injected_anchor_policy() {
        let (before, _) = tab_switch_frames();
        let cursor = before.cursor.clone();
        let row = PaneSurfacePatchRow {
            x: 0,
            y: 0,
            cells: vec![make_cell("Z", 0, 0, 0); 4],
        };
        for repeat in [false, true] {
            let mut encoder = BlitEncoder::with_ime_anchor_repeat(repeat);
            let first = encoder.encode(&before, false);
            encoder.commit(before.clone(), first);
            let patch = encoder
                .encode_patch(std::slice::from_ref(&row), cursor.clone(), false)
                .expect("patch fits the committed frame");
            let output = String::from_utf8(patch.bytes).unwrap();
            assert_eq!(output.matches("\x1b[?2026l").count(), 1, "{output:?}");
            let sync_end = output.find("\x1b[?2026l").expect("同步块结尾");
            let trailing = &output[sync_end + "\x1b[?2026l".len()..];
            let expected = if repeat {
                TAB_SWITCH_POST_SYNC_ANCHOR
            } else {
                ""
            };
            assert_eq!(trailing, expected, "repeat={repeat}: {output:?}");
        }
    }

    #[test]
    fn config_auto_on_wezterm_reaches_the_encoder_without_post_sync_bytes() {
        // 端到端接线：config 枚举 → ImeAnchorRepeatMode → client_ime_anchor_repeat →
        // BlitEncoder::with_ime_anchor_repeat → encode 字节。auto+WezTerm 在任何平台上
        // 都不补发。
        let mode: ImeAnchorRepeatMode = crate::config::RepeatImeCursorAnchorConfig::Auto.into();
        let host = host_env(Some("WezTerm"), Some("xterm-256color"), false);
        let policy = client_ime_anchor_repeat(mode, &host);
        assert!(!policy);
        let mut encoder = BlitEncoder::with_ime_anchor_repeat(policy);
        assert!(!encoder.repeats_ime_anchor());
        assert_single_sync_block_without_trailing_bytes(&encode_tab_switch_with_encoder(
            &mut encoder,
        ));

        // always 受平台闸门：非 Windows 补发，Windows 恒不补发。
        let mode: ImeAnchorRepeatMode = crate::config::RepeatImeCursorAnchorConfig::Always.into();
        let policy = client_ime_anchor_repeat(mode, &host);
        assert_eq!(policy, PLATFORM_ALLOWS_IME_ANCHOR_REPEAT);
        let mut encoder = BlitEncoder::with_ime_anchor_repeat(policy);
        let output = encode_tab_switch_with_encoder(&mut encoder);
        if PLATFORM_ALLOWS_IME_ANCHOR_REPEAT {
            assert_only_post_sync_anchor_outside_block(&output);
        } else {
            assert_single_sync_block_without_trailing_bytes(&output);
        }
    }

    #[test]
    fn blit_encoder_default_keeps_the_platform_legacy_anchor_policy() {
        // server 帧流 / headless 路径不经过 client 配置：`new()` 保持旧行为
        // （非 Windows 补发、Windows 不补发），与显式注入互不影响。
        assert_eq!(
            BlitEncoder::new().repeats_ime_anchor(),
            PLATFORM_ALLOWS_IME_ANCHOR_REPEAT
        );
        assert_eq!(
            BlitEncoder::default().repeats_ime_anchor(),
            PLATFORM_ALLOWS_IME_ANCHOR_REPEAT
        );
        assert!(!BlitEncoder::with_ime_anchor_repeat(false).repeats_ime_anchor());
        assert!(BlitEncoder::with_ime_anchor_repeat(true).repeats_ime_anchor());
    }

    #[test]
    fn ime_anchor_repeat_auto_follows_host_flicker_evidence() {
        let flickering = [
            (Some("WezTerm"), None, false),
            (Some("wezterm"), Some("xterm-256color"), false),
            (Some("kitty"), None, false),
            (Some("ghostty"), None, false),
            (Some("contour"), None, false),
            (Some("foot"), None, false),
            (Some("iTerm.app"), Some("xterm-256color"), false),
            (Some("Alacritty"), None, false),
            (Some("rio"), None, false),
            (None, Some("xterm-kitty"), false),
            (None, Some("xterm-ghostty"), false),
            (None, Some("wezterm"), false),
            (None, Some("xterm-wezterm"), false),
            (None, Some("foot-extra"), false),
            (None, Some("alacritty"), false),
            (None, Some("contour"), false),
            (None, Some("rio"), false),
            (None, Some("xterm-256color"), true),
        ];
        for (program, term, kitty_window) in flickering {
            assert!(
                !resolve_ime_anchor_repeat(
                    ImeAnchorRepeatMode::Auto,
                    &host_env(program, term, kitty_window)
                ),
                "{program:?}/{term:?}/kitty={kitty_window} 块外补发会被看见成闪烁 → 不补发"
            );
        }
        let unknown = [
            (None, None),
            (None, Some("xterm-256color")),
            (Some("Apple_Terminal"), Some("xterm-256color")),
            (Some("vscode"), Some("xterm-256color")),
        ];
        for (program, term) in unknown {
            assert!(
                resolve_ime_anchor_repeat(
                    ImeAnchorRepeatMode::Auto,
                    &host_env(program, term, false)
                ),
                "{program:?}/{term:?} 未知宿主保持旧行为"
            );
        }
        assert!(resolve_ime_anchor_repeat(
            ImeAnchorRepeatMode::Always,
            &host_env(Some("WezTerm"), None, false)
        ));
        assert!(!resolve_ime_anchor_repeat(
            ImeAnchorRepeatMode::Never,
            &host_env(None, None, false)
        ));
    }

    #[test]
    fn ime_anchor_repeat_auto_keeps_the_repeat_inside_a_multiplexer() {
        // tmux 继承外层终端的 TERM_PROGRAM / KITTY_WINDOW_ID，但 2026 passthrough 随版本
        // 变化：多路复用器闸门先于白名单，auto 保留补发（旧行为）。
        let inherited = [
            multiplexed(host_env(Some("WezTerm"), Some("tmux-256color"), false)),
            multiplexed(host_env(Some("kitty"), Some("screen-256color"), false)),
            multiplexed(host_env(None, Some("xterm-kitty"), true)),
            multiplexed(host_env(None, Some("xterm-256color"), false)),
        ];
        for host in inherited {
            assert!(
                resolve_ime_anchor_repeat(ImeAnchorRepeatMode::Auto, &host),
                "{host:?} 多路复用器内保留补发"
            );
        }
        // 显式 never 仍然关闭。
        assert!(!resolve_ime_anchor_repeat(
            ImeAnchorRepeatMode::Never,
            &multiplexed(host_env(Some("WezTerm"), Some("tmux-256color"), false))
        ));
    }

    #[test]
    fn ime_anchor_host_env_derives_the_multiplexer_flag_from_tmux_sty_or_term() {
        let wezterm_in_tmux = ImeAnchorHostEnv::from_values(
            Some("WezTerm".into()),
            Some("tmux-256color".into()),
            false,
            true,
        );
        assert!(wezterm_in_tmux.in_multiplexer);
        assert!(resolve_ime_anchor_repeat(
            ImeAnchorRepeatMode::Auto,
            &wezterm_in_tmux
        ));

        // 只有 TERM 前缀（例如 tmux 里 TMUX 被清掉）也算多路复用器。
        for term in ["screen", "screen-256color", "tmux", "tmux-256color"] {
            let host = ImeAnchorHostEnv::from_values(
                Some("WezTerm".into()),
                Some(term.into()),
                false,
                false,
            );
            assert!(host.in_multiplexer, "TERM={term}");
        }
        // 直接跑在 wezterm 里：无 TMUX/STY、TERM 不以 screen/tmux 开头。
        let bare = ImeAnchorHostEnv::from_values(
            Some("WezTerm".into()),
            Some("xterm-256color".into()),
            false,
            false,
        );
        assert!(!bare.in_multiplexer);
        assert!(!resolve_ime_anchor_repeat(ImeAnchorRepeatMode::Auto, &bare));
    }
}
