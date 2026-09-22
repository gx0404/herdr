//! 额度 / 用量条：阈值色、溢出态、额度窗口进度刻度、分段填充与 ascii 降级。
//! 只画 `area` 的第一行；数字由调用方另画（见 `meter_row`），条本身不带文字。

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
};

use super::fill_row;
use crate::app::state::Palette;

/// 阈值：`ratio >= warn` 黄，`>= crit` 红，其余青。默认 0.75 / 0.90，与账号页
/// `quota_color` 的既有口径一致。
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct GaugeThresholds {
    pub warn: f32,
    pub crit: f32,
}

impl Default for GaugeThresholds {
    fn default() -> Self {
        Self {
            warn: 0.75,
            crit: 0.90,
        }
    }
}

/// 一条 gauge 的全部输入。
///
/// - `ratio`：已用比例；`None` = 未知（与 0 区分，画全空底）；`> 1.0` = 溢出态
///   （条画满、末格画溢出标记）。
/// - `window_progress`：额度窗口已过比例，画一格刻度；同时参与配色——用量
///   跑在时间前面太多时提前转黄 / 红（同 `quota_color` 的 pace 判断）。
/// - `segments`：按顺序铺的分段 `(share, color)`（如内存 used / cache），存在时
///   填充完全由分段决定，`ratio` 只用于溢出判断。
/// - `ascii`：字形降级，宿主字体缺方块 / 线段字形时用。
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub(crate) struct GaugeSpec<'a> {
    pub ratio: Option<f32>,
    pub window_progress: Option<f32>,
    pub thresholds: GaugeThresholds,
    pub segments: Option<&'a [(f32, Color)]>,
    pub ascii: bool,
}

struct Glyphs {
    filled: &'static str,
    empty: &'static str,
    tick: &'static str,
    overflow: &'static str,
}

const UNICODE: Glyphs = Glyphs {
    filled: "━",
    empty: "░",
    tick: "┃",
    overflow: "▸",
};

const ASCII: Glyphs = Glyphs {
    filled: "#",
    empty: ".",
    tick: "|",
    overflow: ">",
};

/// 用量比例的语义色：阈值之外，窗口进度 `window_progress` 给出「按时间应到的
/// 比例」，用量超前 25 个百分点转红、超前 10 个百分点转黄。NaN 视为 0。
pub(crate) fn gauge_color(
    ratio: f32,
    window_progress: Option<f32>,
    thresholds: GaugeThresholds,
    palette: &Palette,
) -> Color {
    let ratio = if ratio.is_nan() { 0.0 } else { ratio };
    let expected = window_progress.filter(|progress| progress.is_finite());
    if ratio >= thresholds.crit || expected.is_some_and(|expected| ratio >= expected + 0.25) {
        palette.red
    } else if ratio >= thresholds.warn || expected.is_some_and(|expected| ratio > expected + 0.10) {
        palette.yellow
    } else {
        palette.teal
    }
}

/// 比例换成格数：非零比例至少占一格（有用量就看得见），上限为 `width`。
fn cells_for(share: f32, width: u16) -> u16 {
    if share.is_nan() || share <= 0.0 {
        return 0;
    }
    let cells = (share.min(1.0) * f32::from(width)).round() as u16;
    cells.clamp(1, width)
}

/// 在 `area` 的第一行画 gauge。`area` 为空直接返回；多余的行不动。
pub(crate) fn render_gauge(
    buffer: &mut Buffer,
    area: Rect,
    spec: &GaugeSpec<'_>,
    palette: &Palette,
) {
    if area.is_empty() {
        return;
    }
    let glyphs = if spec.ascii { ASCII } else { UNICODE };
    let width = area.width;
    let y = area.y;
    fill_row(
        buffer,
        area.x,
        y,
        width,
        glyphs.empty,
        Style::default().fg(palette.surface_dim),
    );

    match spec.segments {
        Some(segments) => {
            let mut cursor = 0u16;
            for (share, color) in segments {
                let cells = cells_for(*share, width).min(width - cursor);
                fill_row(
                    buffer,
                    area.x + cursor,
                    y,
                    cells,
                    glyphs.filled,
                    Style::default().fg(*color),
                );
                cursor += cells;
                if cursor >= width {
                    break;
                }
            }
        }
        None => {
            if let Some(ratio) = spec.ratio {
                let color = gauge_color(ratio, spec.window_progress, spec.thresholds, palette);
                fill_row(
                    buffer,
                    area.x,
                    y,
                    cells_for(ratio, width),
                    glyphs.filled,
                    Style::default().fg(color),
                );
            }
        }
    }

    if let Some(progress) = spec
        .window_progress
        .filter(|progress| (0.0..=1.0).contains(progress))
    {
        let column = ((progress * f32::from(width)).round() as u16).min(width - 1);
        if let Some(cell) = buffer.cell_mut((area.x + column, y)) {
            cell.set_symbol(glyphs.tick)
                .set_style(Style::default().fg(palette.overlay1));
        }
    }

    if spec.ratio.is_some_and(|ratio| ratio > 1.0) {
        if let Some(cell) = buffer.cell_mut((area.x + width - 1, y)) {
            cell.set_symbol(glyphs.overflow)
                .set_style(Style::default().fg(palette.red));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::row_text;
    use super::*;

    fn render(width: u16, spec: GaugeSpec<'_>) -> (Buffer, Palette) {
        let palette = Palette::catppuccin();
        let area = Rect::new(0, 0, width, 1);
        let mut buffer = Buffer::empty(area);
        render_gauge(&mut buffer, area, &spec, &palette);
        (buffer, palette)
    }

    #[test]
    fn fills_by_ratio_with_threshold_colors() {
        let (buffer, palette) = render(
            10,
            GaugeSpec {
                ratio: Some(0.5),
                ..GaugeSpec::default()
            },
        );
        assert_eq!(row_text(&buffer, 0), "━━━━━░░░░░");
        assert_eq!(buffer[(0, 0)].style().fg, Some(palette.teal));
        assert_eq!(buffer[(9, 0)].style().fg, Some(palette.surface_dim));

        let (buffer, palette) = render(
            10,
            GaugeSpec {
                ratio: Some(0.8),
                ..GaugeSpec::default()
            },
        );
        assert_eq!(row_text(&buffer, 0), "━━━━━━━━░░");
        assert_eq!(buffer[(0, 0)].style().fg, Some(palette.yellow));

        let (buffer, palette) = render(
            10,
            GaugeSpec {
                ratio: Some(0.95),
                ..GaugeSpec::default()
            },
        );
        assert_eq!(buffer[(0, 0)].style().fg, Some(palette.red));
    }

    #[test]
    fn overflow_fills_the_bar_and_marks_the_last_cell() {
        let (buffer, palette) = render(
            8,
            GaugeSpec {
                ratio: Some(1.3),
                ..GaugeSpec::default()
            },
        );
        assert_eq!(row_text(&buffer, 0), "━━━━━━━▸");
        assert_eq!(buffer[(0, 0)].style().fg, Some(palette.red));
        assert_eq!(buffer[(7, 0)].style().fg, Some(palette.red));
    }

    #[test]
    fn unknown_ratio_draws_only_the_empty_track() {
        let (buffer, palette) = render(6, GaugeSpec::default());
        assert_eq!(row_text(&buffer, 0), "░░░░░░");
        assert_eq!(buffer[(0, 0)].style().fg, Some(palette.surface_dim));
        let (buffer, _) = render(
            6,
            GaugeSpec {
                ratio: Some(0.0),
                ..GaugeSpec::default()
            },
        );
        assert_eq!(row_text(&buffer, 0), "░░░░░░", "0 与未知都不填充");
        let (buffer, _) = render(
            6,
            GaugeSpec {
                ratio: Some(0.01),
                ..GaugeSpec::default()
            },
        );
        assert_eq!(row_text(&buffer, 0), "━░░░░░", "非零比例至少占一格");
    }

    #[test]
    fn ascii_glyphs_degrade_every_symbol() {
        let (buffer, _) = render(
            10,
            GaugeSpec {
                ratio: Some(1.5),
                window_progress: Some(0.5),
                ascii: true,
                ..GaugeSpec::default()
            },
        );
        assert_eq!(row_text(&buffer, 0), "#####|###>");
        let (buffer, _) = render(
            4,
            GaugeSpec {
                ascii: true,
                ..GaugeSpec::default()
            },
        );
        assert_eq!(row_text(&buffer, 0), "....");
    }

    #[test]
    fn window_progress_draws_a_tick_and_shifts_the_color_by_pace() {
        let (buffer, palette) = render(
            10,
            GaugeSpec {
                ratio: Some(0.3),
                window_progress: Some(0.5),
                ..GaugeSpec::default()
            },
        );
        assert_eq!(row_text(&buffer, 0), "━━━░░┃░░░░");
        assert_eq!(buffer[(5, 0)].style().fg, Some(palette.overlay1));
        assert_eq!(buffer[(0, 0)].style().fg, Some(palette.teal));

        // 用量跑在时间前面：0.5 已用 vs 窗口才过 0.2 → 超前 30 个百分点 → 红。
        let thresholds = GaugeThresholds::default();
        assert_eq!(
            gauge_color(0.5, Some(0.2), thresholds, &palette),
            palette.red
        );
        assert_eq!(
            gauge_color(0.35, Some(0.2), thresholds, &palette),
            palette.yellow
        );
        assert_eq!(
            gauge_color(0.25, Some(0.2), thresholds, &palette),
            palette.teal
        );
        assert_eq!(
            gauge_color(f32::NAN, None, thresholds, &palette),
            palette.teal
        );
        // 进度 1.0 的刻度钉在末格，不越界。
        let (buffer, _) = render(
            4,
            GaugeSpec {
                window_progress: Some(1.0),
                ..GaugeSpec::default()
            },
        );
        assert_eq!(row_text(&buffer, 0), "░░░┃");
    }

    #[test]
    fn segments_lay_out_in_order_with_their_own_colors() {
        let palette = Palette::catppuccin();
        let segments = [
            (0.3, palette.blue),
            (0.2, palette.mauve),
            (0.9, palette.red),
        ];
        let (buffer, _) = render(
            10,
            GaugeSpec {
                ratio: Some(0.5),
                segments: Some(&segments),
                ..GaugeSpec::default()
            },
        );
        assert_eq!(
            row_text(&buffer, 0),
            "━━━━━━━━━━",
            "分段总和超过 1 时截到条宽"
        );
        assert_eq!(buffer[(0, 0)].style().fg, Some(palette.blue));
        assert_eq!(buffer[(3, 0)].style().fg, Some(palette.mauve));
        assert_eq!(buffer[(5, 0)].style().fg, Some(palette.red));
        let segments = [(0.3, palette.blue), (0.2, palette.mauve)];
        let (buffer, _) = render(
            10,
            GaugeSpec {
                segments: Some(&segments),
                ..GaugeSpec::default()
            },
        );
        assert_eq!(row_text(&buffer, 0), "━━━━━░░░░░");
    }

    #[test]
    fn empty_and_narrow_areas_never_panic() {
        let palette = Palette::catppuccin();
        let mut buffer = Buffer::empty(Rect::new(0, 0, 3, 1));
        render_gauge(
            &mut buffer,
            Rect::new(0, 0, 0, 1),
            &GaugeSpec {
                ratio: Some(2.0),
                window_progress: Some(0.5),
                ..GaugeSpec::default()
            },
            &palette,
        );
        assert_eq!(row_text(&buffer, 0), "   ");
        let (buffer, _) = render(
            1,
            GaugeSpec {
                ratio: Some(2.0),
                window_progress: Some(0.5),
                ..GaugeSpec::default()
            },
        );
        assert_eq!(row_text(&buffer, 0), "▸", "单格条只剩溢出标记");
    }
}
