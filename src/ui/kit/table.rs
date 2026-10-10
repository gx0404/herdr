//! 表格：一行表头（可排序列带方向标记与命中区）+ 数据行（选中 / 悬浮 / 右对齐
//! 列 / 单元格前景色 / 变暗的单位）。列宽按 `Fixed` / `Min` / `Fill` 分配，列间
//! 1 列间隔；放不下时先丢 `priority` 大的列（同优先级丢靠右的），至少保留一列。
//! 列布局用栈数组（最多 64 列），单元格文本由调用方的回调按需给出，原语本身只
//! 分配返回的命中表。
//!
//! Graphite 外观：表头是说明色小字，排序列换正文色加粗并带方向标记；选中行是
//! `selection_bg` 底上的粗体正文（不反色）；数量单元格的单位（`MiB`、`GiB`）用
//! 元信息色，数字本身保持正文色。

use std::borrow::Cow;

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
};

use super::{ellipsis_width, fill_row, put_str, put_str_ellipsis, selected_row_style};
use crate::app::state::Palette;
use crate::ui::display_width_u16;

/// 参与布局的最多列数；更多的列不画。
pub(crate) const MAX_COLUMNS: usize = 64;

/// 列宽规则。`Fixed(n)` 恒为 n；`Min(n)` 至少 n，有富余时先长到容下表头与
/// 画面上的单元格（数字列不被 `Fill` 列抢光而截断），没有 `Fill` 列时再平分
/// 剩下的富余；`Fill(w)` 至少容下表头标题，富余按权重 w 分配（w = 0 不分）；
/// `FillAtLeast(n, w)` 同 `Fill(w)`，但至少 n 列——名称这类主列不会被低优先级
/// 的列挤成几个字符，放不下时先丢别的列。丢列只按声明的基础宽度（`Min` 取 n）
/// 判断：内容要保证完整时，调用方把 n 定成内容宽度。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ColumnWidth {
    Fixed(u16),
    Min(u16),
    Fill(u16),
    FillAtLeast(u16, u16),
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

/// 单元格：文本、可选前景色（如按阈值着色的 CPU 占用）与末尾变暗的字节数
/// （`dim_tail`：数量的单位，见 [`TableCell::quantity`]）。选中行忽略 `fg`，单位
/// 照样变暗。`&str` / `String` / `Cow<str>` 都能直接转换。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TableCell<'a> {
    pub text: Cow<'a, str>,
    pub fg: Option<Color>,
    pub dim_tail: usize,
}

impl<'a> TableCell<'a> {
    /// 带单位的数量（`"900.0 MiB"`）：最后一个空格之后的单位用元信息色画，数字
    /// 保持单元格本来的颜色。没有空格时整格都是数字。
    pub(crate) fn quantity(text: impl Into<Cow<'a, str>>) -> Self {
        let text = text.into();
        let dim_tail = text.rfind(' ').map_or(0, |space| text.len() - space - 1);
        Self {
            text,
            fg: None,
            dim_tail,
        }
    }
}

impl<'a> From<&'a str> for TableCell<'a> {
    fn from(text: &'a str) -> Self {
        Self {
            text: Cow::Borrowed(text),
            fg: None,
            dim_tail: 0,
        }
    }
}

impl From<String> for TableCell<'_> {
    fn from(text: String) -> Self {
        Self {
            text: Cow::Owned(text),
            fg: None,
            dim_tail: 0,
        }
    }
}

impl<'a> From<Cow<'a, str>> for TableCell<'a> {
    fn from(text: Cow<'a, str>) -> Self {
        Self {
            text,
            fg: None,
            dim_tail: 0,
        }
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
        ColumnWidth::FillAtLeast(min, _) => display_width_u16(column.title).max(min).max(1),
    }
}

/// 参与分富余的权重：`Fill` / `FillAtLeast` 的 w，其余 0。
fn fill_weight_of(width: ColumnWidth) -> Option<u32> {
    match width {
        ColumnWidth::Fill(weight) | ColumnWidth::FillAtLeast(_, weight) => Some(u32::from(weight)),
        ColumnWidth::Fixed(_) | ColumnWidth::Min(_) => None,
    }
}

/// `content[i]` 是第 i 列（只量 `Min` 列）表头与可见单元格的最大显示宽度。
fn layout(columns: &[Column<'_>], width: u16, content: &[u16; MAX_COLUMNS]) -> Layout {
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
    // 富余：先让 Min 列长到容下自己的内容（从左到右，不超过富余），再按权重给
    // Fill 列，没有 Fill 权重时平分给 Min 列；取整余数给最后一列。
    let mut extra = u32::from(width) - used;
    for index in (0..count).filter(|&index| is_shown(shown, index)) {
        if matches!(columns[index].width, ColumnWidth::Min(_)) {
            let want = u32::from(content[index].saturating_sub(widths[index]));
            let add = want.min(extra);
            widths[index] = widths[index].saturating_add(u16::try_from(add).unwrap_or(u16::MAX));
            extra -= add;
        }
    }
    let extra = extra;
    let fill_weight: u32 = (0..count)
        .filter(|&index| is_shown(shown, index))
        .filter_map(|index| fill_weight_of(columns[index].width))
        .sum();
    let min_count = (0..count)
        .filter(|&index| is_shown(shown, index))
        .filter(|&index| matches!(columns[index].width, ColumnWidth::Min(_)))
        .count() as u32;
    let share = |index: usize| -> u32 {
        match (columns[index].width, fill_weight_of(columns[index].width)) {
            (_, Some(weight)) if fill_weight > 0 => extra * weight / fill_weight,
            (ColumnWidth::Min(_), _) if fill_weight == 0 && min_count > 0 => extra / min_count,
            _ => 0,
        }
    };
    let mut given = 0u32;
    let mut last_growing = None;
    for index in (0..count).filter(|&index| is_shown(shown, index)) {
        let add = share(index);
        let grows = match columns[index].width {
            ColumnWidth::Fill(weight) | ColumnWidth::FillAtLeast(_, weight) => {
                fill_weight > 0 && weight > 0
            }
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
    put_cell_dimmed(buffer, x, y, width, (text, 0), align_right, (style, style));
}

/// 同 [`put_cell`]，`text` 末尾 `dim_tail` 字节换成 `styles.1`——只在整格放得下时
/// 分开着色，截断时整格用 `styles.0`（半个单位没有意义）。
fn put_cell_dimmed(
    buffer: &mut Buffer,
    x: u16,
    y: u16,
    width: u16,
    (text, dim_tail): (&str, usize),
    align_right: bool,
    (style, dim): (Style, Style),
) {
    let used = ellipsis_width(text, width);
    let offset = if align_right { width - used } else { 0 };
    let split = text.len().saturating_sub(dim_tail);
    let fits = display_width_u16(text) <= width;
    if dim_tail == 0 || !fits || !text.is_char_boundary(split) {
        put_str_ellipsis(buffer, x + offset, y, width - offset, text, style);
        return;
    }
    let (head, tail) = text.split_at(split);
    let written = put_str(buffer, x + offset, y, width - offset, head, style);
    put_str(
        buffer,
        x + offset + written,
        y,
        width - offset - written,
        tail,
        dim,
    );
}

/// 画表格：第一行表头，其余是表体。`cell(row, column)` 只对画出来的格调用
/// （`Min` 列的可见格在布局前先量一次宽度，会被调用两次）。返回可排序表头格与
/// 数据行的命中矩形、表体区域与生效的 `scroll`。
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
    let count = columns.len().min(MAX_COLUMNS);
    // 可见行与布局无关（只看表体高度），先定下来，供 Min 列量内容宽度。
    let visible = usize::from(area.height - 1);
    let scroll = state.scroll.min(state.row_count.saturating_sub(visible));
    let mut content = [0u16; MAX_COLUMNS];
    for (index, column) in columns.iter().enumerate().take(count) {
        if !matches!(column.width, ColumnWidth::Min(_)) {
            continue;
        }
        // 排序列的表头多占「标记 + 空格」两列。
        let sorted = state.sort.is_some_and(|sort| sort.column == index);
        let mut widest = display_width_u16(column.title).saturating_add(if sorted { 2 } else { 0 });
        for row in (scroll..state.row_count).take(visible) {
            let value: TableCell<'c> = cell(row, index).into();
            widest = widest.max(display_width_u16(&value.text));
        }
        content[index] = widest;
    }
    let Layout { shown, widths } = layout(columns, area.width, &content);
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
        // 表头是说明层级的小字；排序列换正文色加粗，标出当前按哪一列排。
        let style = if sorted.is_some() {
            Style::default()
                .fg(palette.text)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(palette.subtext0)
        };
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
    render.scroll = scroll;
    let hover_bg = palette.hover_row_bg();
    for (offset, row) in (scroll..state.row_count).take(visible).enumerate() {
        // offset < body.height ≤ u16::MAX，转换不会失败。
        let y = body.y + u16::try_from(offset).unwrap_or(0);
        let selected = state.selected == Some(row);
        let row_style = if selected {
            selected_row_style(palette)
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
            put_cell_dimmed(
                buffer,
                x,
                y,
                width,
                (&value.text, value.dim_tail),
                column.align_right,
                (
                    style,
                    style.fg(palette.overlay1).remove_modifier(Modifier::BOLD),
                ),
            );
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
        assert_eq!(sorted.fg, Some(palette.text), "排序列表头换正文色");
        assert!(sorted.add_modifier.contains(Modifier::BOLD));
        let plain = buffer[(2, 0)].style();
        assert_eq!(plain.fg, Some(palette.subtext0), "其余表头是说明色小字");
        assert!(!plain.add_modifier.contains(Modifier::BOLD));
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

    /// 有 Fill 列时 Min 列也先长到容下表头与画面上的单元格，剩下的富余才给 Fill
    /// 列——7 位 PID 不被截断；富余不够时退回声明的基础宽度（丢列也只看它）。
    #[test]
    fn min_columns_fit_visible_content_before_fill_takes_the_rest() {
        let columns = [
            Column {
                title: "PID",
                width: ColumnWidth::Min(3),
                align_right: true,
                sortable: true,
                priority: 1,
            },
            Column {
                title: "Name",
                width: ColumnWidth::Fill(1),
                align_right: false,
                sortable: false,
                priority: 0,
            },
        ];
        let rows = [["4090512", "python3"], ["2559", "Xorg"]];
        let palette = Palette::catppuccin();
        let paint = |width: u16| {
            let area = Rect::new(0, 0, width, 3);
            let mut buffer = Buffer::empty(area);
            render_table(
                &mut buffer,
                area,
                &columns,
                &state(2),
                |row, column| rows[row][column],
                &palette,
            );
            buffer
        };
        let buffer = paint(20);
        assert_eq!(row_text(&buffer, 0), "    PID Name        ");
        assert_eq!(row_text(&buffer, 1), "4090512 python3     ");
        assert_eq!(row_text(&buffer, 2), "   2559 Xorg        ");
        let buffer = paint(8);
        assert_eq!(row_text(&buffer, 1), "40… pyt…", "没有富余时按基础宽度截断");
        // 只量可见行：滚出画面的长 PID 不撑宽列。
        let area = Rect::new(0, 0, 20, 2);
        let mut buffer = Buffer::empty(area);
        render_table(
            &mut buffer,
            area,
            &columns,
            &TableState {
                scroll: 1,
                ..state(2)
            },
            |row, column| rows[row][column],
            &palette,
        );
        assert_eq!(row_text(&buffer, 1), "2559 Xorg           ");
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
                    dim_tail: 0,
                },
            },
            &palette,
        );
        assert_eq!(row_text(&buffer, 0), "名称   占用");
        assert_eq!(row_text(&buffer, 1), "浏览…   99%", "宽字符按显示宽度截断");
        let selected = buffer[(0, 1)].style();
        assert_eq!(
            selected.bg,
            Some(palette.selection_row_bg()),
            "选中行是 selection_bg 底，不反色"
        );
        assert!(selected.add_modifier.contains(Modifier::BOLD));
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

    /// 数量单元格右对齐、单位变暗；放不下时整格截断、不再分开着色。
    #[test]
    fn quantity_cells_dim_their_unit_and_stay_right_aligned() {
        let columns = [Column {
            title: "Mem",
            width: ColumnWidth::Fixed(9),
            align_right: true,
            sortable: false,
            priority: 0,
        }];
        let palette = Palette::catppuccin();
        let area = Rect::new(0, 0, 9, 2);
        let mut buffer = Buffer::empty(area);
        render_table(
            &mut buffer,
            area,
            &columns,
            &state(1),
            |_, _| TableCell::quantity("64.0 MiB"),
            &palette,
        );
        assert_eq!(row_text(&buffer, 1), " 64.0 MiB");
        assert_eq!(
            buffer[(1, 1)].style().fg,
            Some(palette.text),
            "数字是正文色"
        );
        assert_eq!(
            buffer[(6, 1)].style().fg,
            Some(palette.overlay1),
            "单位变暗"
        );
        let cell = TableCell::quantity("12");
        assert_eq!(cell.dim_tail, 0, "没有单位就不变暗");
        let area = Rect::new(0, 0, 5, 2);
        let mut buffer = Buffer::empty(area);
        render_table(
            &mut buffer,
            area,
            &columns,
            &state(1),
            |_, _| TableCell::quantity("64.0 MiB"),
            &palette,
        );
        assert_eq!(row_text(&buffer, 1), "64.0…", "截断时整格同色");
        assert_eq!(buffer[(4, 1)].style().fg, Some(palette.text));
    }

    /// 主列带最小宽度时，放不下先丢低优先级的列，而不是把主列挤成几个字符；
    /// 放得下时它照样吃掉全部富余。
    #[test]
    fn fill_columns_with_a_minimum_make_lower_priority_columns_drop_first() {
        let columns = [
            Column {
                title: "Name",
                width: ColumnWidth::FillAtLeast(8, 1),
                align_right: false,
                sortable: false,
                priority: 0,
            },
            Column {
                title: "User",
                width: ColumnWidth::Fixed(6),
                align_right: false,
                sortable: false,
                priority: 2,
            },
            Column {
                title: "CPU",
                width: ColumnWidth::Fixed(4),
                align_right: true,
                sortable: false,
                priority: 1,
            },
        ];
        let rows = [["herdr-server", "dev", "12%"]];
        let palette = Palette::catppuccin();
        let paint = |width: u16| {
            let area = Rect::new(0, 0, width, 2);
            let mut buffer = Buffer::empty(area);
            render_table(
                &mut buffer,
                area,
                &columns,
                &state(1),
                |row, column| rows[row][column],
                &palette,
            );
            buffer
        };
        // 8 + 6 + 4 + 2 间隔 = 20：放得下就都在，富余给主列。
        assert_eq!(row_text(&paint(24), 1), "herdr-server dev     12%");
        // 19 列：先丢 User（priority 2），名称保住至少 8 列。
        assert_eq!(row_text(&paint(19), 1), "herdr-server    12%");
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
