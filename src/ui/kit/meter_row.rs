//! 单行「标签 + 条 + 数字 + 说明」：监控卡片与账号页的基本行。宽度预算按
//! value（数字永不裁）> label > gauge > detail 分配——窄时先砍条形、保数字。

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
};

use super::gauge::{render_gauge, GaugeSpec};
use super::{put_str, put_str_ellipsis};
use crate::app::state::Palette;
use crate::ui::display_width_u16;

/// 一行的全部输入。`value` 是已经格式化好的数字文本（调用方负责单位与精度）；
/// `stale` = 数据过期：文字转灰、条形改灰色填充，但仍显示上次的值。
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct MeterRow<'a> {
    pub label: &'a str,
    pub value: &'a str,
    pub detail: Option<&'a str>,
    pub gauge: GaugeSpec<'a>,
    pub stale: bool,
}

/// 同一张卡里多条 meter row 共用的列宽（显示列）：标签左对齐补到 `label`、数字
/// 右对齐补到 `value`、说明补到 `detail`，各行条形因此起止对齐——逐行各自预算时
/// 「0」与「10」、「57%」与「100%」会让条形长短不一。0 = 该列按本行自身宽度
/// （`Default` 即逐行预算）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct MeterColumns {
    pub label: u16,
    pub value: u16,
    pub detail: u16,
}

impl MeterColumns {
    /// 把一行的各段宽度并进列宽（逐列取最大）。
    pub(crate) fn fit(&mut self, label: &str, value: &str, detail: Option<&str>) {
        self.label = self.label.max(display_width_u16(label));
        self.value = self.value.max(display_width_u16(value));
        self.detail = self.detail.max(detail.map_or(0, display_width_u16));
    }
}

/// 条形最窄画几格；再窄就整条砍掉，把列让给数字与标签。
const GAUGE_MIN: u16 = 4;

/// 各段占多少列。分配顺序即优先级：value 先拿（最多整行），label 次之（放不下
/// 时截断、不足 3 列则整段丢），detail 只在条形仍能保住最小宽度时保留，条形拿
/// 剩下的全部。返回 `(label, gauge, value, detail)` 的列数（0 = 不画）。
fn budget(width: u16, label: u16, value: u16, detail: u16) -> (u16, u16, u16, u16) {
    let value_w = value.min(width);
    let mut avail = width - value_w;
    let label_w = if label == 0 {
        0
    } else if avail > label {
        label
    } else if avail >= 3 {
        avail - 1
    } else {
        0
    };
    if label_w > 0 {
        avail -= label_w + 1;
    }
    // 说明需要「条形最小宽 + 间隔 + 说明 + 间隔」都还放得下。
    let detail_w = if detail > 0 && avail > GAUGE_MIN + 1 + detail {
        detail
    } else {
        0
    };
    if detail_w > 0 {
        avail -= detail_w + 1;
    }
    let gauge_w = if avail > GAUGE_MIN { avail - 1 } else { 0 };
    (label_w, gauge_w, value_w, detail_w)
}

/// 在 `area` 的第一行画一条 meter row。条形吸收全部富余宽度；没有条形时富余
/// 留在标签与数字之间，数字（连同其后的说明）靠右。逐行预算，同卡多行要对齐时
/// 用 `render_meter_row_aligned`。
pub(crate) fn render_meter_row(
    buffer: &mut Buffer,
    area: Rect,
    row: &MeterRow<'_>,
    palette: &Palette,
) {
    render_meter_row_aligned(buffer, area, row, MeterColumns::default(), palette);
}

/// 同 `render_meter_row`，但标签 / 数字 / 说明各占 `columns` 给出的列宽（不足按
/// 本行自身宽度）：同一张卡的各行传同一份 `columns`，条形起止就对齐。标签左对齐、
/// 数字右对齐，说明左对齐；列宽参与同一套宽度预算，窄时照样先砍条形、保数字。
pub(crate) fn render_meter_row_aligned(
    buffer: &mut Buffer,
    area: Rect,
    row: &MeterRow<'_>,
    columns: MeterColumns,
    palette: &Palette,
) {
    if area.is_empty() {
        return;
    }
    let y = area.y;
    let detail = row.detail.unwrap_or("");
    let value_own = display_width_u16(row.value);
    let (label_w, gauge_w, value_w, detail_w) = budget(
        area.width,
        display_width_u16(row.label).max(columns.label),
        value_own.max(columns.value),
        display_width_u16(detail).max(columns.detail),
    );

    let (label_style, value_style, detail_style) = if row.stale {
        let muted = Style::default().fg(palette.overlay0);
        (muted, muted, muted)
    } else {
        (
            Style::default().fg(palette.subtext0),
            Style::default()
                .fg(palette.text)
                .add_modifier(Modifier::BOLD),
            Style::default().fg(palette.overlay0),
        )
    };

    if label_w > 0 {
        put_str_ellipsis(buffer, area.x, y, label_w, row.label, label_style);
    }

    if gauge_w > 0 {
        let gauge_x = area.x + label_w + u16::from(label_w > 0);
        let gauge_area = Rect::new(gauge_x, y, gauge_w, 1);
        if row.stale {
            // 过期数据：保留量值，但用灰色单段代替阈值色，不给旧数据画确定的语义色。
            let share = row.gauge.ratio.map_or_else(
                || {
                    row.gauge.segments.map_or(0.0, |segments| {
                        segments.iter().map(|(share, _)| share).sum()
                    })
                },
                |ratio| ratio.clamp(0.0, 1.0),
            );
            let muted = [(share, palette.overlay0)];
            let spec = GaugeSpec {
                segments: Some(&muted),
                ..row.gauge
            };
            render_gauge(buffer, gauge_area, &spec, palette);
        } else {
            render_gauge(buffer, gauge_area, &row.gauge, palette);
        }
    }

    let detail_span = if detail_w > 0 { detail_w + 1 } else { 0 };
    let value_x = area.right().saturating_sub(detail_span + value_w);
    // 数字在自己的列里右对齐：列比数字宽时左侧留空，比数字窄（整行都放不下）时
    // 从列首开始画、尾部被裁，与逐行预算一致。
    let shown = value_own.min(value_w);
    put_str(
        buffer,
        value_x + (value_w - shown),
        y,
        shown,
        row.value,
        value_style,
    );
    if detail_w > 0 {
        put_str(
            buffer,
            value_x + value_w + 1,
            y,
            detail_w,
            detail,
            detail_style,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::super::row_text;
    use super::*;

    fn sample<'a>(detail: Option<&'a str>) -> MeterRow<'a> {
        MeterRow {
            label: "CPU",
            value: "42%",
            detail,
            gauge: GaugeSpec {
                ratio: Some(0.5),
                ..GaugeSpec::default()
            },
            stale: false,
        }
    }

    fn render(width: u16, row: &MeterRow<'_>) -> (Buffer, Palette) {
        let palette = Palette::catppuccin();
        let area = Rect::new(0, 0, width, 1);
        let mut buffer = Buffer::empty(area);
        render_meter_row(&mut buffer, area, row, &palette);
        (buffer, palette)
    }

    #[test]
    fn wide_row_shows_every_segment_in_order() {
        let (buffer, palette) = render(24, &sample(Some("4 cores")));
        assert_eq!(row_text(&buffer, 0), "CPU ━━━━░░░░ 42% 4 cores");
        assert_eq!(buffer[(0, 0)].style().fg, Some(palette.subtext0));
        assert_eq!(buffer[(4, 0)].style().fg, Some(palette.teal));
        let value = buffer[(13, 0)].style();
        assert_eq!(value.fg, Some(palette.text));
        assert!(value.add_modifier.contains(Modifier::BOLD));
        assert_eq!(buffer[(17, 0)].style().fg, Some(palette.overlay0));
    }

    #[test]
    fn narrowing_drops_detail_then_gauge_then_label_but_never_the_value() {
        let (buffer, _) = render(12, &sample(Some("4 cores")));
        assert_eq!(
            row_text(&buffer, 0),
            "CPU ━━░░ 42%",
            "说明先丢，条形缩到最窄"
        );
        let (buffer, _) = render(8, &sample(Some("4 cores")));
        assert_eq!(row_text(&buffer, 0), "CPU  42%", "条形丢掉，数字靠右");
        let (buffer, _) = render(6, &sample(None));
        assert_eq!(row_text(&buffer, 0), "C… 42%", "标签截断保数字");
        let (buffer, _) = render(5, &sample(None));
        assert_eq!(row_text(&buffer, 0), "  42%", "标签放不下整段丢");
        let (buffer, _) = render(2, &sample(None));
        assert_eq!(row_text(&buffer, 0), "42", "比数字还窄时才裁数字");
    }

    #[test]
    fn cjk_label_and_value_budget_by_display_width() {
        let row = MeterRow {
            label: "内存",
            value: "6.2G",
            ..sample(None)
        };
        let (buffer, _) = render(18, &row);
        assert_eq!(row_text(&buffer, 0), "内存 ━━━━░░░░ 6.2G");
        let (buffer, _) = render(8, &row);
        assert_eq!(row_text(&buffer, 0), "内… 6.2G", "宽字符按 2 列截断");
    }

    #[test]
    fn stale_rows_keep_the_value_but_drop_the_semantic_color() {
        let row = MeterRow {
            stale: true,
            ..sample(Some("old"))
        };
        let (buffer, palette) = render(20, &row);
        assert_eq!(row_text(&buffer, 0), "CPU ━━━━░░░░ 42% old");
        assert_eq!(buffer[(4, 0)].style().fg, Some(palette.overlay0));
        let value = buffer[(13, 0)].style();
        assert_eq!(value.fg, Some(palette.overlay0));
        assert!(!value.add_modifier.contains(Modifier::BOLD));
    }

    /// 同卡多行共用列宽：标签「0」「10」、数字「57%」「100%」宽度不同，条形起点
    /// 与长度仍一致；数字右对齐，没有说明的行也占住说明列。
    #[test]
    fn aligned_rows_share_the_gauge_column() {
        let palette = Palette::catppuccin();
        let rows = [("0", "57%", None), ("10", "100%", Some("hot"))];
        let mut columns = MeterColumns::default();
        for (label, value, detail) in rows {
            columns.fit(label, value, detail);
        }
        assert_eq!(
            columns,
            MeterColumns {
                label: 2,
                value: 4,
                detail: 3
            }
        );
        let area = Rect::new(0, 0, 20, 2);
        let mut buffer = Buffer::empty(area);
        for (y, (label, value, detail)) in rows.into_iter().enumerate() {
            let row = MeterRow {
                label,
                value,
                detail,
                gauge: GaugeSpec {
                    ratio: Some(0.5),
                    ..GaugeSpec::default()
                },
                stale: false,
            };
            render_meter_row_aligned(
                &mut buffer,
                Rect::new(0, y as u16, 20, 1),
                &row,
                columns,
                &palette,
            );
        }
        assert_eq!(row_text(&buffer, 0), "0  ━━━━░░░░  57%    ");
        assert_eq!(row_text(&buffer, 1), "10 ━━━━░░░░ 100% hot");
        // 窄到放不下条形时仍先砍条形、保数字，与逐行预算同口径。
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 1));
        render_meter_row_aligned(
            &mut buffer,
            Rect::new(0, 0, 8, 1),
            &MeterRow {
                label: "0",
                value: "57%",
                ..sample(None)
            },
            columns,
            &palette,
        );
        assert_eq!(row_text(&buffer, 0), "0    57%");
    }

    #[test]
    fn ascii_and_empty_area_are_handled() {
        let mut row = sample(None);
        row.gauge.ascii = true;
        let (buffer, _) = render(16, &row);
        assert_eq!(row_text(&buffer, 0), "CPU ####.... 42%");
        let palette = Palette::catppuccin();
        let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 1));
        render_meter_row(&mut buffer, Rect::new(0, 0, 0, 0), &row, &palette);
        assert_eq!(row_text(&buffer, 0), "    ");
    }
}
