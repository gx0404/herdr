//! 迷你图：sparkline 与堆叠面积图。盲文字形一格两列四级，方块字形一格一列
//! 八级，ascii 一格一列八级；宿主字体缺盲文时切 `Blocks`，连方块都缺时切
//! `Ascii`。样本靠右对齐（最新在右），超出列数的旧样本丢弃；不分配。

#![allow(dead_code)] // seam-stub(monitor)：波 2 监控车道接入系统页后删除

use ratatui::{buffer::Buffer, layout::Rect, style::Color, style::Style};

/// 字形集：决定每格容纳几个样本、每行分几级。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChartGlyphs {
    /// U+2800 盲文：每格 2 列 × 4 级，分辨率最高。
    Braille,
    /// `▁▂▃▄▅▆▇█`：每格 1 列 × 8 级。
    Blocks,
    /// `_.-:=+*#`：每格 1 列 × 8 级，纯 ASCII。
    Ascii,
}

impl ChartGlyphs {
    fn samples_per_cell(self) -> usize {
        match self {
            Self::Braille => 2,
            Self::Blocks | Self::Ascii => 1,
        }
    }

    fn levels_per_row(self) -> u16 {
        match self {
            Self::Braille => 4,
            Self::Blocks | Self::Ascii => 8,
        }
    }
}

const BLOCKS: [&str; 8] = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
const ASCII: [&str; 8] = ["_", ".", "-", ":", "=", "+", "*", "#"];
/// 盲文左列 / 右列自底向上填 0..=4 点的位图。
const BRAILLE_LEFT: [u32; 5] = [0x00, 0x40, 0x44, 0x46, 0x47];
const BRAILLE_RIGHT: [u32; 5] = [0x00, 0x80, 0xA0, 0xB0, 0xB8];

/// 数值换成总级数：非零值至少一级，上限 `total`；NaN / 负数 / 无效上限为 0。
fn level_of(value: f32, max: f32, total: u16) -> u16 {
    if value.is_nan() || value <= 0.0 || max.is_nan() || max <= 0.0 || total == 0 {
        return 0;
    }
    let level = (value / max * f32::from(total)).ceil() as u16;
    level.clamp(1, total)
}

/// 右对齐取样：把最后 `columns` 个样本铺到 0..columns 的槽位上，槽位落在数据
/// 之前的返回 `None`。
fn sample_at(samples: &[f32], slot: usize, columns: usize) -> Option<f32> {
    let visible = samples.len().min(columns);
    let start = columns - visible;
    let index = slot.checked_sub(start)?;
    samples.get(samples.len() - visible + index).copied()
}

fn braille_char(left: u16, right: u16) -> char {
    let bits = BRAILLE_LEFT[usize::from(left.min(4))] | BRAILLE_RIGHT[usize::from(right.min(4))];
    char::from_u32(0x2800 + bits).unwrap_or(' ')
}

/// 把一格写进缓冲区：`heights` 是该格内各子列的行内高度（0 = 空）。
fn put_cell(
    buffer: &mut Buffer,
    x: u16,
    y: u16,
    glyphs: ChartGlyphs,
    heights: [u16; 2],
    style: Style,
) {
    let Some(cell) = buffer.cell_mut((x, y)) else {
        return;
    };
    match glyphs {
        ChartGlyphs::Braille => {
            if heights == [0, 0] {
                cell.set_char(' ').set_style(style);
            } else {
                cell.set_char(braille_char(heights[0], heights[1]))
                    .set_style(style);
            }
        }
        ChartGlyphs::Blocks | ChartGlyphs::Ascii => {
            let height = heights[0].min(8);
            if height == 0 {
                cell.set_symbol(" ").set_style(style);
            } else {
                let table = if glyphs == ChartGlyphs::Blocks {
                    BLOCKS
                } else {
                    ASCII
                };
                cell.set_symbol(table[usize::from(height - 1)])
                    .set_style(style);
            }
        }
    }
}

fn auto_max(samples: &[f32], columns: usize) -> f32 {
    let visible = &samples[samples.len().saturating_sub(columns)..];
    visible
        .iter()
        .copied()
        .filter(|value| value.is_finite())
        .fold(0.0, f32::max)
}

/// 单序列迷你图。`max` 为 `None` 时按可见样本自动定标；`area` 可多行，级数
/// 随行数线性叠加。空样本或空区域不画。
pub(crate) fn render_sparkline(
    buffer: &mut Buffer,
    area: Rect,
    samples: &[f32],
    max: Option<f32>,
    glyphs: ChartGlyphs,
    style: Style,
) {
    if area.is_empty() || samples.is_empty() {
        return;
    }
    let per_cell = glyphs.samples_per_cell();
    let columns = usize::from(area.width) * per_cell;
    let max = max
        .filter(|max| *max > 0.0)
        .unwrap_or_else(|| auto_max(samples, columns));
    let levels_per_row = glyphs.levels_per_row();
    let total = area.height.saturating_mul(levels_per_row);
    for column in 0..area.width {
        let mut levels = [0u16; 2];
        for (sub, level) in levels.iter_mut().enumerate().take(per_cell) {
            let slot = usize::from(column) * per_cell + sub;
            *level =
                sample_at(samples, slot, columns).map_or(0, |value| level_of(value, max, total));
        }
        for row in 0..area.height {
            let base = row * levels_per_row;
            let heights = [
                levels[0].saturating_sub(base).min(levels_per_row),
                levels[1].saturating_sub(base).min(levels_per_row),
            ];
            put_cell(
                buffer,
                area.x + column,
                area.bottom() - 1 - row,
                glyphs,
                heights,
                style,
            );
        }
    }
}

/// 堆叠面积图：`series[i]` 画在前 i 条之上，各用 `colors[i]`（不够时沿用最后一
/// 个）。一格只能有一种前景色，取该格内最高的那条序列的颜色。`max` 为 `None`
/// 时按各列堆叠和的最大值定标。
pub(crate) fn render_area_chart(
    buffer: &mut Buffer,
    area: Rect,
    series: &[&[f32]],
    max: Option<f32>,
    glyphs: ChartGlyphs,
    colors: &[Color],
) {
    if area.is_empty() || series.is_empty() {
        return;
    }
    let per_cell = glyphs.samples_per_cell();
    let columns = usize::from(area.width) * per_cell;
    let stacked_at = |slot: usize, upto: usize| -> f32 {
        series[..upto]
            .iter()
            .map(|samples| sample_at(samples, slot, columns).unwrap_or(0.0).max(0.0))
            .filter(|value| value.is_finite())
            .sum()
    };
    let max = max.filter(|max| *max > 0.0).unwrap_or_else(|| {
        (0..columns)
            .map(|slot| stacked_at(slot, series.len()))
            .fold(0.0, f32::max)
    });
    let levels_per_row = glyphs.levels_per_row();
    let total = area.height.saturating_mul(levels_per_row);
    let fallback = colors.last().copied().unwrap_or(Color::Reset);

    for column in 0..area.width {
        for row in 0..area.height {
            let base = row * levels_per_row;
            let mut heights = [0u16; 2];
            let mut top_series: Option<usize> = None;
            for (sub, height) in heights.iter_mut().enumerate().take(per_cell) {
                let slot = usize::from(column) * per_cell + sub;
                let mut cumulative = 0.0f32;
                let mut in_row = 0u16;
                for (index, samples) in series.iter().enumerate() {
                    cumulative += sample_at(samples, slot, columns)
                        .filter(|value| value.is_finite())
                        .unwrap_or(0.0)
                        .max(0.0);
                    let level = level_of(cumulative, max, total)
                        .saturating_sub(base)
                        .min(levels_per_row);
                    if level > in_row {
                        in_row = level;
                        // 这条序列在本格内确实抬高了轮廓，它就是本格最上面的一条。
                        if top_series.is_none_or(|top| index >= top) {
                            top_series = Some(index);
                        }
                    }
                }
                *height = in_row;
            }
            let color = top_series
                .and_then(|index| colors.get(index).copied())
                .unwrap_or(fallback);
            put_cell(
                buffer,
                area.x + column,
                area.bottom() - 1 - row,
                glyphs,
                heights,
                Style::default().fg(color),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::row_text;
    use super::*;

    fn sparkline(
        width: u16,
        height: u16,
        samples: &[f32],
        max: Option<f32>,
        glyphs: ChartGlyphs,
    ) -> Buffer {
        let area = Rect::new(0, 0, width, height);
        let mut buffer = Buffer::empty(area);
        render_sparkline(&mut buffer, area, samples, max, glyphs, Style::default());
        buffer
    }

    #[test]
    fn blocks_scale_to_eight_levels_per_row() {
        let buffer = sparkline(3, 1, &[0.0, 4.0, 8.0], Some(8.0), ChartGlyphs::Blocks);
        assert_eq!(row_text(&buffer, 0), " ▄█");
        let buffer = sparkline(3, 1, &[0.0, 4.0, 8.0], Some(8.0), ChartGlyphs::Ascii);
        assert_eq!(row_text(&buffer, 0), " :#");
    }

    #[test]
    fn braille_packs_two_samples_per_cell() {
        let buffer = sparkline(2, 1, &[0.0, 4.0, 8.0, 8.0], Some(8.0), ChartGlyphs::Braille);
        // 左列 0 点 + 右列 2 点 = 0xA0；两列全满 = 0xFF。
        assert_eq!(row_text(&buffer, 0), "⢠⣿");
        assert_eq!(braille_char(1, 0), '⡀');
        assert_eq!(braille_char(4, 4), '⣿');
    }

    #[test]
    fn samples_align_to_the_right_and_old_ones_fall_off() {
        let buffer = sparkline(4, 1, &[8.0], Some(8.0), ChartGlyphs::Blocks);
        assert_eq!(row_text(&buffer, 0), "   █");
        let buffer = sparkline(2, 1, &[8.0, 1.0, 8.0], Some(8.0), ChartGlyphs::Blocks);
        assert_eq!(row_text(&buffer, 0), "▁█", "只保留最后两个样本");
    }

    #[test]
    fn multi_row_charts_stack_levels_bottom_up() {
        let buffer = sparkline(2, 2, &[4.0, 8.0], Some(8.0), ChartGlyphs::Blocks);
        assert_eq!(row_text(&buffer, 1), "██", "底行");
        assert_eq!(row_text(&buffer, 0), " █", "顶行");
        let buffer = sparkline(1, 2, &[1.0], Some(8.0), ChartGlyphs::Braille);
        // 单样本靠右落在右列：总 8 级里 1/8 → 底行右列 1 点。
        assert_eq!(row_text(&buffer, 1), "⢀");
        assert_eq!(row_text(&buffer, 0), " ");
    }

    #[test]
    fn auto_scale_uses_the_visible_maximum_and_empty_input_draws_nothing() {
        let buffer = sparkline(2, 1, &[1.0, 2.0], None, ChartGlyphs::Blocks);
        assert_eq!(row_text(&buffer, 0), "▄█");
        let buffer = sparkline(2, 1, &[], None, ChartGlyphs::Blocks);
        assert_eq!(row_text(&buffer, 0), "  ");
        let buffer = sparkline(2, 1, &[0.0, 0.0], None, ChartGlyphs::Blocks);
        assert_eq!(row_text(&buffer, 0), "  ", "全零不除零、不画");
        let buffer = sparkline(2, 1, &[f32::NAN, -3.0], Some(8.0), ChartGlyphs::Blocks);
        assert_eq!(row_text(&buffer, 0), "  ");
    }

    #[test]
    fn area_chart_colors_each_cell_by_its_top_series() {
        let palette = crate::app::state::Palette::catppuccin();
        let colors = [palette.blue, palette.red];
        let area = Rect::new(0, 0, 2, 2);
        let mut buffer = Buffer::empty(area);
        render_area_chart(
            &mut buffer,
            area,
            &[&[8.0, 4.0], &[8.0, 0.0]],
            Some(16.0),
            ChartGlyphs::Blocks,
            &colors,
        );
        // 列 0：底行全是第一条（蓝），顶行全是第二条（红）。
        assert_eq!(row_text(&buffer, 1), "█▄");
        assert_eq!(buffer[(0, 1)].style().fg, Some(palette.blue));
        assert_eq!(row_text(&buffer, 0), "█ ");
        assert_eq!(buffer[(0, 0)].style().fg, Some(palette.red));
        // 列 1：第二条为 0，颜色仍是第一条。
        assert_eq!(buffer[(1, 1)].style().fg, Some(palette.blue));
    }

    #[test]
    fn area_chart_handles_missing_colors_and_auto_scale() {
        let area = Rect::new(0, 0, 1, 1);
        let mut buffer = Buffer::empty(area);
        render_area_chart(
            &mut buffer,
            area,
            &[&[1.0], &[1.0]],
            None,
            ChartGlyphs::Braille,
            &[],
        );
        // 单样本靠右：盲文格的左列空、右列按堆叠和 2.0 自动定标为满 4 点。
        assert_eq!(row_text(&buffer, 0), "⢸", "堆叠和自动定标为满格");
        assert_eq!(buffer[(0, 0)].style().fg, Some(Color::Reset));
        let mut buffer = Buffer::empty(area);
        render_area_chart(&mut buffer, area, &[], None, ChartGlyphs::Blocks, &[]);
        assert_eq!(row_text(&buffer, 0), " ");
    }
}
