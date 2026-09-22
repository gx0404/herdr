//! 表格：一行表头（可排序列带方向标记与命中区）+ 数据行（选中 / 悬浮 / 右对齐
//! 列 / 单元格前景色）。列宽按 `Fixed` / `Min` / `Fill` 分配，列间 1 列间隔；
//! 放不下时先丢 `priority` 大的列（同优先级丢靠右的），至少保留一列。列布局用
//! 栈数组（最多 64 列），单元格文本由调用方的回调按需给出，原语本身只分配返回
//! 的命中表。

use std::borrow::Cow;

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
};

use super::{ellipsis_width, fill_row, put_str, put_str_ellipsis, selected_style};
use crate::app::state::Palette;
use crate::ui::display_width_u16;

/// 参与布局的最多列数；更多的列不画。
pub(crate) const MAX_COLUMNS: usize = 64;

/// 列宽规则。`Fixed(n)` 恒为 n；`Min(n)` 至少 n，没有 `Fill` 列时平分富余；
/// `Fill(w)` 至少容下表头标题，富余按权重 w 分配（w = 0 不分）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ColumnWidth {
    Fixed(u16),
    Min(u16),
    Fill(u16),
}

/// 一列。`priority` 越大越先被丢（0 = 最重要）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Column<'a> {
    pub title: &'a str,
    pub width: ColumnWidth,
    pub align_right: bool,
    pub sortable: bool,
    pub priority: u8,
}

/// 当前排序：按第 `column` 列，`descending` 为真时降序。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SortState {
    pub column: usize,
    pub descending: bool,
}

/// 表格的视图状态（都是标量，打包免得一串 `usize` 传错位置）。`scroll` 是首个
/// 可见行；越界时收回到「最后一屏」。`ascii` 把排序标记 `▲▼` 降级为 `^v`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct TableState {
    pub sort: Option<SortState>,
    pub row_count: usize,
    pub scroll: usize,
    pub selected: Option<usize>,
    pub hovered: Option<usize>,
    pub ascii: bool,
}

/// 单元格：文本与可选前景色（如按阈值着色的 CPU 占用）；选中行统一用反色，
/// 忽略 `fg`。`&str` / `String` / `Cow<str>` 都能直接转换。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TableCell<'a> {
    pub text: Cow<'a, str>,
    pub fg: Option<Color>,
}

impl<'a> From<&'a str> for TableCell<'a> {
    fn from(text: &'a str) -> Self {
        Self {
            text: Cow::Borrowed(text),
            fg: None,
        }
    }
}

impl From<String> for TableCell<'_> {
    fn from(text: String) -> Self {
        Self {
            text: Cow::Owned(text),
            fg: None,
        }
    }
}

impl<'a> From<Cow<'a, str>> for TableCell<'a> {
    fn from(text: Cow<'a, str>) -> Self {
        Self { text, fg: None }
    }
}

/// 渲染结果：可排序且画出来的表头格 `(矩形, 列下标)`、画出来的数据行
/// `(矩形, 行下标)`、表体区域（表头之下）与实际生效的 `scroll`。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct TableRender {
    pub header_cells: Vec<(Rect, usize)>,
    pub rows: Vec<(Rect, usize)>,
    pub body: Rect,
    pub scroll: usize,
}

/// 列布局：`shown` 的 bit i = 第 i 列画出来，`widths[i]` 是其宽度。
struct Layout {
    shown: u64,
    widths: [u16; MAX_COLUMNS],
}

fn base_width(column: &Column<'_>) -> u16 {
    match column.width {
        ColumnWidth::Fixed(width) | ColumnWidth::Min(width) => width,
        ColumnWidth::Fill(_) => display_width_u16(column.title).max(1),
    }
}

fn layout(columns: &[Column<'_>], width: u16) -> Layout {
    let count = columns.len().min(MAX_COLUMNS);
    let mut shown = if count == MAX_COLUMNS {
        u64::MAX
    } else {
        (1u64 << count) - 1
    };
    let is_shown = |shown: u64, index: usize| shown & (1u64 << index) != 0;
    let total = |shown: u64| -> u32 {
        let mut sum = 0u32;
        let mut visible = 0u32;
        for (index, column) in columns.iter().enumerate().take(count) {
            if is_shown(shown, index) {
                sum += u32::from(base_width(column));
                visible += 1;
            }
        }
        sum + visible.saturating_sub(1)
    };
    // 丢列：priority 最大的先走，同优先级丢靠右的；至少保留一列。
    while shown.count_ones() > 1 && total(shown) > u32::from(width) {
        let victim = (0..count)
            .filter(|&index| is_shown(shown, index))
            .max_by_key(|&index| (columns[index].priority, index));
        match victim {
            Some(index) => shown &= !(1u64 << index),
            None => break,
        }
    }

    let mut widths = [0u16; MAX_COLUMNS];
    for (index, column) in columns.iter().enumerate().take(count) {
        if is_shown(shown, index) {
            widths[index] = base_width(column);
        }
    }
    let used = total(shown);
    if used > u32::from(width) {
        // 只剩一列仍放不下：裁到可用宽度。
        if let Some(index) = (0..count).find(|&index| is_shown(shown, index)) {
            widths[index] = width;
        }
        return Layout { shown, widths };
    }
    // 富余：先按权重给 Fill 列，没有 Fill 权重时平分给 Min 列；取整余数给最后一列。
    let extra = u32::from(width) - used;
    let fill_weight: u32 = (0..count)
        .filter(|&index| is_shown(shown, index))
        .filter_map(|index| match columns[index].width {
            ColumnWidth::Fill(weight) => Some(u32::from(weight)),
            _ => None,
        })
        .sum();
    let min_count = (0..count)
        .filter(|&index| is_shown(shown, index))
        .filter(|&index| matches!(columns[index].width, ColumnWidth::Min(_)))
        .count() as u32;
    let share = |index: usize| -> u32 {
        match columns[index].width {
            ColumnWidth::Fill(weight) if fill_weight > 0 => extra * u32::from(weight) / fill_weight,
            ColumnWidth::Min(_) if fill_weight == 0 && min_count > 0 => extra / min_count,
            _ => 0,
        }
    };
    let mut given = 0u32;
    let mut last_growing = None;
    for index in (0..count).filter(|&index| is_shown(shown, index)) {
        let add = share(index);
        let grows = match columns[index].width {
            ColumnWidth::Fill(weight) => fill_weight > 0 && weight > 0,
            ColumnWidth::Min(_) => fill_weight == 0,
            ColumnWidth::Fixed(_) => false,
        };
        if grows {
            last_growing = Some(index);
        }
        widths[index] = widths[index].saturating_add(u16::try_from(add).unwrap_or(u16::MAX));
        given += add;
    }
    if let Some(index) = last_growing {
        let remainder = u16::try_from(extra - given).unwrap_or(u16::MAX);
        widths[index] = widths[index].saturating_add(remainder);
    }
    Layout { shown, widths }
}

/// 在 `x` 起 `width` 列内写一格文本，右对齐时按截断后的真实宽度靠右。
fn put_cell(
    buffer: &mut Buffer,
    x: u16,
    y: u16,
    width: u16,
    text: &str,
    align_right: bool,
    style: Style,
) {
    let offset = if align_right {
        width - ellipsis_width(text, width)
    } else {
        0
    };
    put_str_ellipsis(buffer, x + offset, y, width - offset, text, style);
}

/// 画表格：第一行表头，其余是表体。`cell(row, column)` 只对画出来的格调用。
/// 返回可排序表头格与数据行的命中矩形、表体区域与生效的 `scroll`。
pub(crate) fn render_table<'c, C>(
    buffer: &mut Buffer,
    area: Rect,
    columns: &[Column<'_>],
    state: &TableState,
    cell: impl Fn(usize, usize) -> C,
    palette: &Palette,
) -> TableRender
where
    C: Into<TableCell<'c>>,
{
    let mut render = TableRender::default();
    if area.is_empty() || columns.is_empty() {
        return render;
    }
    let Layout { shown, widths } = layout(columns, area.width);
    let count = columns.len().min(MAX_COLUMNS);
    let (ascending, descending) = if state.ascii {
        ("^", "v")
    } else {
        ("▲", "▼")
    };

    // 表头。
    let mut x = area.x;
    for (index, column) in columns.iter().enumerate().take(count) {
        if shown & (1u64 << index) == 0 {
            continue;
        }
        let width = widths[index].min(area.right().saturating_sub(x));
        if width == 0 {
            break;
        }
        let sorted = state.sort.filter(|sort| sort.column == index);
        let style = Style::default()
            .fg(if sorted.is_some() {
                palette.accent
            } else {
                palette.subtext0
            })
            .add_modifier(Modifier::BOLD);
        let rect = Rect::new(x, area.y, width, 1);
        match sorted {
            Some(sort) if width >= 3 => {
                let mark = if sort.descending {
                    descending
                } else {
                    ascending
                };
                // 标记放在远离对齐边的一侧：左对齐列放标题后，右对齐列放标题前。
                let title_w = width - 2;
                if column.align_right {
                    put_str(buffer, x, area.y, 1, mark, style);
                    put_cell(buffer, x + 2, area.y, title_w, column.title, true, style);
                } else {
                    let used = ellipsis_width(column.title, title_w);
                    put_str_ellipsis(buffer, x, area.y, title_w, column.title, style);
                    put_str(buffer, x + used + 1, area.y, 1, mark, style);
                }
            }
            _ => put_cell(
                buffer,
                x,
                area.y,
                width,
                column.title,
                column.align_right,
                style,
            ),
        }
        if column.sortable {
            render.header_cells.push((rect, index));
        }
        x = x.saturating_add(width).saturating_add(1);
    }

    // 表体。
    let body = Rect::new(area.x, area.y + 1, area.width, area.height - 1);
    render.body = body;
    let visible = usize::from(body.height);
    let scroll = state.scroll.min(state.row_count.saturating_sub(visible));
    render.scroll = scroll;
    let hover_bg = palette.hover_row_bg();
    for (offset, row) in (scroll..state.row_count).take(visible).enumerate() {
        // offset < body.height ≤ u16::MAX，转换不会失败。
        let y = body.y + u16::try_from(offset).unwrap_or(0);
        let selected = state.selected == Some(row);
        let row_style = if selected {
            selected_style(palette)
        } else if state.hovered == Some(row) {
            Style::default().fg(palette.text).bg(hover_bg)
        } else {
            Style::default().fg(palette.text)
        };
        if selected || state.hovered == Some(row) {
            fill_row(buffer, body.x, y, body.width, " ", row_style);
        }
        let mut x = body.x;
        for (index, column) in columns.iter().enumerate().take(count) {
            if shown & (1u64 << index) == 0 {
                continue;
            }
            let width = widths[index].min(body.right().saturating_sub(x));
            if width == 0 {
                break;
            }
            let value: TableCell<'c> = cell(row, index).into();
            let style = match value.fg {
                Some(fg) if !selected => row_style.fg(fg),
                _ => row_style,
            };
            put_cell(buffer, x, y, width, &value.text, column.align_right, style);
            x = x.saturating_add(width).saturating_add(1);
        }
        render.rows.push((Rect::new(body.x, y, body.width, 1), row));
    }
    render
}

#[cfg(test)]
mod tests {
    use super::super::row_text;
    use super::*;

    fn process_columns() -> [Column<'static>; 4] {
        [
            Column {
                title: "PID",
                width: ColumnWidth::Fixed(5),
                align_right: true,
                sortable: true,
                priority: 1,
            },
            Column {
                title: "Name",
                width: ColumnWidth::Fill(1),
                align_right: false,
                sortable: true,
                priority: 0,
            },
            Column {
                title: "CPU",
                width: ColumnWidth::Fixed(5),
                align_right: true,
                sortable: true,
                priority: 2,
            },
            Column {
                title: "Mem",
                width: ColumnWidth::Fixed(5),
                align_right: true,
                sortable: false,
                priority: 3,
            },
        ]
    }

    const ROWS: [[&str; 4]; 3] = [
        ["1", "init", "0.1", "8M"],
        ["42", "herdr server", "12.5", "96M"],
        ["777", "zsh", "0.0", "4M"],
    ];

    fn paint(
        width: u16,
        height: u16,
        columns: &[Column<'_>],
        state: TableState,
    ) -> (Buffer, TableRender, Palette) {
        let palette = Palette::catppuccin();
        let area = Rect::new(0, 0, width, height);
        let mut buffer = Buffer::empty(area);
        let render = render_table(
            &mut buffer,
            area,
            columns,
            &state,
            |row, column| ROWS[row][column],
            &palette,
        );
        (buffer, render, palette)
    }

    fn state(row_count: usize) -> TableState {
        TableState {
            row_count,
            ..TableState::default()
        }
    }

    #[test]
    fn lays_out_fixed_and_fill_columns_with_header_hits() {
        let (buffer, render, palette) = paint(
            30,
            4,
            &process_columns(),
            TableState {
                sort: Some(SortState {
                    column: 2,
                    descending: true,
                }),
                ..state(3)
            },
        );
        // 5 + 1 + Name + 1 + 5 + 1 + 5 = 30 → Name 拿 12 列。
        assert_eq!(row_text(&buffer, 0), "  PID Name         ▼ CPU   Mem");
        assert_eq!(row_text(&buffer, 1), "    1 init           0.1    8M");
        assert_eq!(row_text(&buffer, 2), "   42 herdr server  12.5   96M");
        assert_eq!(
            render.header_cells,
            vec![
                (Rect::new(0, 0, 5, 1), 0),
                (Rect::new(6, 0, 12, 1), 1),
                (Rect::new(19, 0, 5, 1), 2),
            ],
            "不可排序的 Mem 列没有表头命中区"
        );
        assert_eq!(render.body, Rect::new(0, 1, 30, 3));
        assert_eq!(
            render.rows,
            vec![
                (Rect::new(0, 1, 30, 1), 0),
                (Rect::new(0, 2, 30, 1), 1),
                (Rect::new(0, 3, 30, 1), 2),
            ]
        );
        let sorted = buffer[(21, 0)].style();
        assert_eq!(sorted.fg, Some(palette.accent), "排序列表头 accent");
        assert!(sorted.add_modifier.contains(Modifier::BOLD));
        assert_eq!(buffer[(2, 0)].style().fg, Some(palette.subtext0));
        assert_eq!(buffer[(6, 1)].style().fg, Some(palette.text));
    }

    #[test]
    fn narrow_tables_drop_high_priority_numbers_first() {
        let columns = process_columns();
        // 总基础宽：5 + 4 + 5 + 5 + 3 间隔 = 22；21 列丢 Mem（priority 3）。
        let (buffer, _, _) = paint(21, 2, &columns, state(3));
        assert_eq!(row_text(&buffer, 0), "  PID Name        CPU");
        // 15 列再丢 CPU（priority 2）。
        let (buffer, render, _) = paint(15, 2, &columns, state(3));
        assert_eq!(row_text(&buffer, 0), "  PID Name     ");
        assert_eq!(
            render
                .header_cells
                .iter()
                .map(|(_, i)| *i)
                .collect::<Vec<_>>(),
            vec![0, 1]
        );
        // 只剩最重要的 Name，且裁到可用宽度。
        let (buffer, _, _) = paint(3, 2, &columns, state(3));
        assert_eq!(row_text(&buffer, 0), "Na…");
        assert_eq!(row_text(&buffer, 1), "in…");
    }

    #[test]
    fn equal_priorities_drop_the_rightmost_and_min_columns_share_the_rest() {
        let columns = [
            Column {
                title: "A",
                width: ColumnWidth::Min(2),
                align_right: false,
                sortable: false,
                priority: 1,
            },
            Column {
                title: "B",
                width: ColumnWidth::Min(2),
                align_right: false,
                sortable: false,
                priority: 1,
            },
            Column {
                title: "C",
                width: ColumnWidth::Min(2),
                align_right: false,
                sortable: false,
                priority: 1,
            },
        ];
        let cells = |_: usize, column: usize| ["a", "b", "c"][column];
        let palette = Palette::catppuccin();
        let area = Rect::new(0, 0, 12, 2);
        let mut buffer = Buffer::empty(area);
        render_table(&mut buffer, area, &columns, &state(1), cells, &palette);
        // 2×3 + 2 间隔 = 8，富余 4 平分给三个 Min 列（1,1,1 + 余数 1 给最后一列）。
        assert_eq!(row_text(&buffer, 0), "A   B   C   ");
        assert_eq!(row_text(&buffer, 1), "a   b   c   ");
        let area = Rect::new(0, 0, 6, 2);
        let mut buffer = Buffer::empty(area);
        render_table(&mut buffer, area, &columns, &state(1), cells, &palette);
        assert_eq!(
            row_text(&buffer, 0),
            "A  B  ",
            "同优先级先丢最右的 C，余数给 B"
        );
    }

    #[test]
    fn selection_hover_cell_colors_and_cjk_truncation() {
        let columns = [
            Column {
                title: "名称",
                width: ColumnWidth::Fill(1),
                align_right: false,
                sortable: false,
                priority: 0,
            },
            Column {
                title: "占用",
                width: ColumnWidth::Fixed(4),
                align_right: true,
                sortable: false,
                priority: 0,
            },
        ];
        let palette = Palette::catppuccin();
        let area = Rect::new(0, 0, 11, 4);
        let mut buffer = Buffer::empty(area);
        let hot = palette.red;
        render_table(
            &mut buffer,
            area,
            &columns,
            &TableState {
                selected: Some(0),
                hovered: Some(1),
                ..state(3)
            },
            |row, column| match (row, column) {
                (_, 0) => TableCell::from(["浏览器进程", "终端", "编辑器"][row]),
                (_, _) => TableCell {
                    text: Cow::Borrowed("99%"),
                    fg: Some(hot),
                },
            },
            &palette,
        );
        assert_eq!(row_text(&buffer, 0), "名称   占用");
        assert_eq!(row_text(&buffer, 1), "浏览…   99%", "宽字符按显示宽度截断");
        let selected = buffer[(0, 1)].style();
        assert_eq!(selected.bg, Some(palette.accent));
        assert_ne!(buffer[(8, 1)].style().fg, Some(hot), "选中行忽略单元格色");
        assert_eq!(buffer[(0, 2)].style().bg, Some(palette.hover_row_bg()));
        assert_eq!(buffer[(8, 2)].style().fg, Some(hot), "单元格色");
        assert_eq!(buffer[(8, 3)].style().fg, Some(hot));
    }

    #[test]
    fn scroll_is_clamped_and_empty_or_tiny_areas_are_safe() {
        let columns = process_columns();
        let (buffer, render, _) = paint(
            30,
            3,
            &columns,
            TableState {
                scroll: 99,
                ..state(3)
            },
        );
        assert_eq!(render.scroll, 1, "越界收回到最后一屏");
        assert_eq!(
            render.rows.iter().map(|(_, row)| *row).collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(row_text(&buffer, 1), "   42 herdr server  12.5   96M");
        let (_, render, _) = paint(30, 3, &columns, state(0));
        assert!(render.rows.is_empty());
        assert_eq!(render.body, Rect::new(0, 1, 30, 2));
        let (buffer, render, _) = paint(30, 1, &columns, state(3));
        assert!(render.rows.is_empty(), "只有表头一行");
        assert_eq!(row_text(&buffer, 0), "  PID Name           CPU   Mem");
        let (_, render, _) = paint(30, 3, &[], state(3));
        assert_eq!(render, TableRender::default());
    }

    #[test]
    fn ascii_sort_marks_and_left_aligned_marks_follow_the_title() {
        let columns = process_columns();
        let (buffer, _, _) = paint(
            30,
            1,
            &columns,
            TableState {
                sort: Some(SortState {
                    column: 1,
                    descending: false,
                }),
                ascii: true,
                ..state(0)
            },
        );
        assert_eq!(row_text(&buffer, 0), "  PID Name ^         CPU   Mem");
    }
}
