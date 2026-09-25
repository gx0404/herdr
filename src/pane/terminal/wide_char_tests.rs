//! 宽字符在行尾、折行与 resize 前后的回归：网格、纯文本 / ANSI 快照与渲染单元格必须一致，
//! 且渲染单元格不得把 2 宽字形放进窗格最后一列（会越界盖住边框或相邻窗格）。整帧渲染与脏行
//! 补丁两条显示路径分别用独立窗格对照，补丁逐行拼回的画面必须与整帧渲染逐格相同。
//!
//! 字节流全部手写合成：自动换行时宽字符恰好落在最后一列、TUI 用 CUP 逐行写满到最后一列、
//! 覆盖已有宽字符的半边、inline 程序擦行重绘。期望按 xterm 语义给出；其中宽字符写到宽字符
//! 右半格、窄字符写到宽字符首格、以及折行位置，已用 tmux 3.0a 重放同一批字节核对一致。
//! 窄字符写到宽字符尾格时 tmux 3.0a 保留首格（「中x文」），这里按 xterm 清空首格（「 x文」）；
//! 主屏重排后的视口位置两者也不同，不作对照。「宽字符写到右半格」就是 opencode（opentui）
//! 折行缺字时实际写出的字节。
use super::*;

struct WidePane {
    /// 整帧渲染与各类读取都走这个窗格；整帧渲染会清空它的脏标记。
    pane: PaneTerminal,
    /// 只收脏行补丁、从不整帧渲染：与 `pane` 喂同样的字节与 resize，补丁路径因此不会被整帧
    /// 渲染清空脏标记（做法同 `migration_tests::incremental_rows_reconstruct_full_render`）。
    incremental: PaneTerminal,
    tx: mpsc::Sender<Bytes>,
    width: u16,
    height: u16,
    /// 由补丁逐行拼回的画面，每格一个符号。
    retained: Vec<Vec<String>>,
    /// 最近一次补丁带来的行：行号与每格符号。
    last_patch: Vec<(u16, Vec<String>)>,
}

impl WidePane {
    fn new(width: u16, height: u16) -> Self {
        let (tx, _rx) = mpsc::channel(16);
        let new_pane = || {
            let terminal = crate::ghostty::Terminal::new(width, height, 4096).unwrap();
            PaneTerminal::new(GhosttyPaneTerminal::new(terminal, tx.clone()).unwrap())
        };
        Self {
            pane: new_pane(),
            incremental: new_pane(),
            tx,
            width,
            height,
            retained: placeholder_rows(width, height),
            last_patch: Vec::new(),
        }
    }

    fn write(&mut self, bytes: &[u8]) {
        for pane in [&self.pane, &self.incremental] {
            let _ = pane.process_pty_bytes(PaneId::from_raw(1), 0, bytes, &self.tx);
        }
        self.sync_patch(false);
    }

    fn resize(&mut self, width: u16, height: u16) {
        for pane in [&self.pane, &self.incremental] {
            pane.resize(height, width, 8, 16).unwrap();
        }
        self.width = width;
        self.height = height;
        // 几何变了，补丁必须重发每一行：先填占位符，漏发的行在对照时现形。
        self.retained = placeholder_rows(width, height);
        self.sync_patch(true);
    }

    /// 收一次补丁并逐行拼进 `retained`，再与整帧渲染逐格对照。每次 write / resize 都至少
    /// 要比对到一行，补丁空转或回退都算失败。
    fn sync_patch(&mut self, expect_every_row: bool) {
        let patch = match self
            .incremental
            .collect_dirty_patch(self.width, self.height)
        {
            TerminalDirtyPatchOutcome::Patch(patch) => patch,
            outcome => panic!("每次 write / resize 都应产出脏行补丁，得到 {outcome:?}"),
        };
        self.last_patch = patch_symbols(patch);
        assert!(!self.last_patch.is_empty(), "补丁至少应带一行");
        if expect_every_row {
            let rows: Vec<u16> = self.last_patch.iter().map(|(row, _)| *row).collect();
            assert_eq!(
                rows,
                (0..self.height).collect::<Vec<_>>(),
                "resize 后补丁应重发每一行"
            );
        }
        for (row, symbols) in &self.last_patch {
            assert_eq!(
                symbols.len(),
                usize::from(self.width),
                "补丁第 {row} 行宽度"
            );
            self.retained[usize::from(*row)] = symbols.clone();
        }
        assert_eq!(
            self.retained,
            self.render(),
            "补丁拼回的画面与整帧渲染不一致"
        );
    }

    /// 最近一次补丁里指定行的每格符号。
    fn patch_row(&self, y: u16) -> &[String] {
        self.last_patch
            .iter()
            .find(|(row, _)| *row == y)
            .map(|(_, symbols)| symbols.as_slice())
            .expect("最近一次补丁应带这一行")
    }

    /// 整帧渲染：每行每格的符号（宽字符尾格为空串）。
    fn render(&self) -> Vec<Vec<String>> {
        self.render_area(self.width)
    }

    /// 把窗格画进宽 `area_width` 的区域；比网格窄时只画左边这几列。
    fn render_area(&self, area_width: u16) -> Vec<Vec<String>> {
        let backend = ratatui::backend::TestBackend::new(area_width, self.height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut rows = Vec::new();
        terminal
            .draw(|frame| {
                self.pane
                    .render(frame, Rect::new(0, 0, area_width, self.height), false);
                let buffer = frame.buffer_mut();
                for y in 0..self.height {
                    rows.push(
                        (0..area_width)
                            .map(|x| buffer[(x, y)].symbol().to_string())
                            .collect(),
                    );
                }
            })
            .unwrap();
        rows
    }

    fn render_row(&self, y: usize) -> Vec<String> {
        self.render().swap_remove(y)
    }

    /// 网格：宽字符首格给出字符、尾格跳过，软换行占位格记作 ⏎，空格子为空格；去掉行尾空格。
    fn grid(&self) -> Vec<String> {
        let (_, _, rows) = self.pane.screen_text_snapshot().unwrap();
        let start = rows.len().saturating_sub(usize::from(self.height));
        rows[start..]
            .iter()
            .map(|row| {
                let mut line = String::new();
                for cell in &row.cells {
                    match cell.wide {
                        crate::ghostty::CellWide::SpacerTail => {}
                        crate::ghostty::CellWide::SpacerHead => line.push('⏎'),
                        _ if cell.graphemes.is_empty() => line.push(' '),
                        _ => {
                            line.extend(cell.graphemes.iter().filter_map(|cp| char::from_u32(*cp)))
                        }
                    }
                }
                line.trim_end_matches(' ').to_string()
            })
            .collect()
    }

    fn soft_wraps(&self) -> Vec<bool> {
        let (_, _, rows) = self.pane.screen_text_snapshot().unwrap();
        let start = rows.len().saturating_sub(usize::from(self.height));
        rows[start..].iter().map(|row| row.soft_wrapped).collect()
    }

    /// ANSI 快照去掉 SGR 后按行拆开；只保留到最后一个非空行。
    fn ansi(&self) -> Vec<String> {
        let ansi = self.pane.visible_ansi();
        let plain = regex::Regex::new("\x1b\\[[0-9;:]*m")
            .unwrap()
            .replace_all(&ansi, "")
            .into_owned();
        let mut lines: Vec<String> = plain
            .split("\r\n")
            .map(|line| line.trim_end_matches(' ').to_string())
            .collect();
        while lines.last().is_some_and(|line| line.is_empty()) {
            lines.pop();
        }
        lines
    }

    /// 去掉软换行后的最近文本：折行、重排都不得丢字或重复。
    fn unwrapped(&self) -> String {
        self.pane
            .recent_unwrapped_text_snapshot(usize::from(self.height))
            .text
            .trim_end_matches('\n')
            .to_string()
    }

    /// 渲染单元格不得让 2 宽字形越过右边界。
    fn assert_no_wide_glyph_past_right_edge(&self) {
        assert_no_wide_glyph_past_right_edge(&self.render());
    }
}

/// 每个 2 宽字形都必须还有一列给右半边。
fn assert_no_wide_glyph_past_right_edge(rows: &[Vec<String>]) {
    for (y, row) in rows.iter().enumerate() {
        for (x, symbol) in row.iter().enumerate() {
            if symbol.width() > 1 {
                assert!(
                    x + 1 < row.len(),
                    "第 {y} 行第 {x} 列的 {symbol:?} 越过右边界：{row:?}"
                );
            }
        }
    }
}

fn placeholder_rows(width: u16, height: u16) -> Vec<Vec<String>> {
    vec![vec!["?".to_string(); usize::from(width)]; usize::from(height)]
}

fn patch_symbols(patch: TerminalDirtyPatch) -> Vec<(u16, Vec<String>)> {
    patch
        .rows
        .into_iter()
        .map(|(row, cells)| {
            (
                row,
                cells.iter().map(|cell| cell.symbol.to_string()).collect(),
            )
        })
        .collect()
}

/// 在从未收过补丁的新窗格上，按给定可绘宽度收第一次补丁（第一次收集带全部行）。
fn first_patch_in_area(width: u16, height: u16, bytes: &[u8], area_width: u16) -> Vec<Vec<String>> {
    let (tx, _rx) = mpsc::channel(16);
    let terminal = crate::ghostty::Terminal::new(width, height, 4096).unwrap();
    let pane = PaneTerminal::new(GhosttyPaneTerminal::new(terminal, tx.clone()).unwrap());
    let _ = pane.process_pty_bytes(PaneId::from_raw(1), 0, bytes, &tx);
    let patch = match pane.collect_dirty_patch(area_width, height) {
        TerminalDirtyPatchOutcome::Patch(patch) => patch_symbols(patch),
        outcome => panic!("第一次收集应产出补丁，得到 {outcome:?}"),
    };
    assert_eq!(
        patch.iter().map(|(row, _)| *row).collect::<Vec<_>>(),
        (0..height).collect::<Vec<_>>(),
        "第一次收集应带全部行"
    );
    patch.into_iter().map(|(_, symbols)| symbols).collect()
}

fn cells(symbols: &[&str]) -> Vec<String> {
    symbols.iter().map(|symbol| symbol.to_string()).collect()
}

#[test]
fn autowrap_moves_a_wide_char_that_misses_the_last_column_whole_to_the_next_row() {
    // 7 列：「ab中文」占 6 列后只剩奇数的 1 列，「字」放不下，整字折到下一行首。
    let mut pane = WidePane::new(7, 4);
    pane.write("ab中文字".as_bytes());

    assert_eq!(pane.grid(), ["ab中文⏎", "字", "", ""]);
    assert_eq!(pane.soft_wraps(), [true, false, false, false]);
    assert_eq!(pane.pane.visible_text(), "ab中文\n字\n");
    assert_eq!(pane.ansi(), ["ab中文", "字"]);
    assert_eq!(pane.unwrapped(), "ab中文字");
    assert_eq!(
        pane.render_row(0),
        cells(&["a", "b", "中", "", "文", "", " "])
    );
    assert_eq!(
        pane.render_row(1),
        cells(&["字", "", " ", " ", " ", " ", " "])
    );
    pane.assert_no_wide_glyph_past_right_edge();

    // resize 前后：变宽合并、变窄重新折行，整段文字不丢不重。
    for (width, first, second) in [
        (9, "ab中文字", ""),
        (5, "ab中⏎", "文字"),
        (7, "ab中文⏎", "字"),
    ] {
        pane.resize(width, 4);
        let grid = pane.grid();
        assert_eq!(
            (grid[0].as_str(), grid[1].as_str()),
            (first, second),
            "{width} 列"
        );
        assert_eq!(pane.unwrapped(), "ab中文字", "{width} 列");
        pane.assert_no_wide_glyph_past_right_edge();
    }
}

#[test]
fn tui_rows_written_to_the_last_column_keep_the_next_row_intact() {
    // 备用屏 8 列：第 1 行 CUP 后写满 8 列，「字」正好占最后两列（挂起换行）；
    // 下一行重新 CUP，不得把上一行的字带过来。第 3 行只剩 1 列时写宽字符，按 xterm 语义折到下一行。
    let mut pane = WidePane::new(8, 4);
    pane.write("\x1b[?1049h\x1b[1;1Hab中文字\x1b[2;1H下一行\x1b[3;8H宽".as_bytes());

    assert_eq!(pane.grid(), ["ab中文字", "下一行", "       ⏎", "宽"]);
    assert_eq!(pane.soft_wraps(), [false, false, true, false]);
    assert_eq!(pane.pane.visible_text(), "ab中文字\n下一行\n\n宽\n");
    assert_eq!(pane.ansi(), ["ab中文字", "下一行", "", "宽"]);
    assert_eq!(
        pane.render_row(0),
        cells(&["a", "b", "中", "", "文", "", "字", ""])
    );
    assert_eq!(
        pane.render_row(1),
        cells(&["下", "", "一", "", "行", "", " ", " "])
    );
    pane.assert_no_wide_glyph_past_right_edge();

    // 变宽再变回：备用屏不重排，文字原样保留；软换行占位格离开最后一列后按内核规则清空。
    for width in [10, 8] {
        pane.resize(width, 4);
        assert_eq!(pane.grid(), ["ab中文字", "下一行", "", "宽"], "{width} 列");
        pane.assert_no_wide_glyph_past_right_edge();
    }
}

#[test]
fn overwriting_half_of_a_wide_char_blanks_its_other_half() {
    let mut pane = WidePane::new(8, 3);
    // opencode（opentui）折行缺字时的实际写法：它把被劈开的「何」当 1 格宽重画在行首，
    // 紧接着写「命」，再 CUP 到第 4 列（「命」的右半格）写「令」。任何终端都会因此擦掉「命」。
    pane.write("\x1b[?1049h\x1b[1;1H何命\x1b[1;4H令".as_bytes());
    // 窄字符写到宽字符的尾格：首格清空。
    pane.write("\x1b[2;1H中文\x1b[2;2Hx".as_bytes());
    // 窄字符写到宽字符的首格：尾格清空。
    pane.write("\x1b[3;1H中文\x1b[3;3Hy".as_bytes());

    assert_eq!(pane.grid(), ["何 令", " x文", "中y"]);
    assert_eq!(pane.pane.visible_text(), "何 令\n x文\n中y\n");
    assert_eq!(pane.ansi(), ["何 令", " x文", "中y"]);
    assert_eq!(
        pane.render_row(0),
        cells(&["何", "", " ", "令", "", " ", " ", " "])
    );
    assert_eq!(
        pane.render_row(1),
        cells(&[" ", "x", "文", "", " ", " ", " ", " "])
    );
    assert_eq!(
        pane.render_row(2),
        cells(&["中", "", "y", " ", " ", " ", " ", " "])
    );
    pane.assert_no_wide_glyph_past_right_edge();

    for width in [10, 8] {
        pane.resize(width, 3);
        assert_eq!(pane.grid(), ["何 令", " x文", "中y"], "{width} 列");
        pane.assert_no_wide_glyph_past_right_edge();
    }
}

#[test]
fn alt_screen_shrink_that_cuts_a_wide_char_draws_a_blank_at_the_edge() {
    // 备用屏不重排：8 列缩到 7 列时，最后两列上的「字」只剩首格留在第 6 列（0 基）。
    let mut pane = WidePane::new(8, 3);
    pane.write("\x1b[?1049h\x1b[1;1Hab中文字\x1b[2;1H下一行\x1b[3;1Habcdefgh".as_bytes());
    pane.resize(7, 3);

    // 网格与文本读取保留这个字（tmux 3.0a 的 capture-pane 同样保留），变回原宽时还能显示。
    assert_eq!(pane.grid(), ["ab中文字", "下一行", "abcdefg"]);
    assert_eq!(pane.pane.visible_text(), "ab中文字\n下一行\nabcdefg\n");
    // 两条显示路径都把放不下的首格画成空白，不让 2 宽字形越出窗格；末格的普通窄字符照画。
    // tmux 3.0a 客户端画这一格时同样输出空格（已录客户端输出核对）。
    assert_eq!(
        pane.render_row(0),
        cells(&["a", "b", "中", "", "文", "", " "])
    );
    assert_eq!(pane.patch_row(0).last().map(String::as_str), Some(" "));
    assert_eq!(pane.render_row(2).last().map(String::as_str), Some("g"));
    assert_eq!(pane.patch_row(2).last().map(String::as_str), Some("g"));
    pane.assert_no_wide_glyph_past_right_edge();

    // vendored libghostty-vt 的既有行为（0 基列）：缩窄截掉第 7 列的尾格后，变宽时不补回尾格，
    // 第 6 列仍是没有尾格的宽字符首格、第 7 列是普通空格，所以这里钉住第 7 列为空格。另据
    // `Terminal.zig::printCell` 推断：之后若只往第 7 列写窄字符，它不回看左侧首格，该字会被
    // 第 6 列的 2 宽字形盖住，直到应用重写第 6 列。
    pane.resize(8, 3);
    assert_eq!(pane.grid()[0], "ab中文字");
    assert_eq!(
        pane.render_row(0),
        cells(&["a", "b", "中", "", "文", "", "字", " "])
    );
    assert_eq!(pane.patch_row(0), pane.render_row(0).as_slice());
    pane.assert_no_wide_glyph_past_right_edge();
}

#[test]
fn drawable_area_narrower_than_the_grid_blanks_only_a_cut_wide_char() {
    // 可绘区域比网格窄（虚拟渲染，或窗格已缩小而终端 resize 尚未生效）：按可绘列数判断右边缘，
    // 只把右半格落在区域外的宽字符首格画成空白，末格的普通窄字符照画。
    let bytes = "\x1b[?1049h\x1b[1;1Hab中文字\x1b[2;1Habcdefgh".as_bytes();
    let mut pane = WidePane::new(8, 2);
    pane.write(bytes);
    for (area_width, expected) in [
        (
            7,
            [
                cells(&["a", "b", "中", "", "文", "", " "]),
                cells(&["a", "b", "c", "d", "e", "f", "g"]),
            ],
        ),
        (
            5,
            [
                cells(&["a", "b", "中", "", " "]),
                cells(&["a", "b", "c", "d", "e"]),
            ],
        ),
    ] {
        let rendered = pane.render_area(area_width);
        assert_eq!(rendered, expected, "整帧渲染，可绘 {area_width} 列");
        assert_no_wide_glyph_past_right_edge(&rendered);
        let patched = first_patch_in_area(8, 2, bytes, area_width);
        assert_eq!(patched, expected, "脏行补丁，可绘 {area_width} 列");
    }
}

#[test]
fn narrow_cell_holding_a_two_wide_grapheme_is_blanked_only_at_the_last_column() {
    // 应用关闭 2027（字素簇）后，VS16 只挂在原来的窄格上：「⚠️」「⌨️」是 2 宽字素却只占 1 格。
    // 落在最后一列时同样越过右边界，两条显示路径都画空白；行中间的维持原样（既有行为）。
    let mut pane = WidePane::new(5, 2);
    pane.write("\x1b[?2027l\x1b[1;1Habcd⚠\u{fe0f}\x1b[2;1Hab⌨\u{fe0f}cd".as_bytes());

    // 网格与文本读取保留原字素。
    assert_eq!(pane.grid(), ["abcd⚠\u{fe0f}", "ab⌨\u{fe0f}cd"]);
    assert_eq!(pane.render_row(0), cells(&["a", "b", "c", "d", " "]));
    assert_eq!(pane.patch_row(0).last().map(String::as_str), Some(" "));
    assert_eq!(
        pane.render_row(1),
        cells(&["a", "b", "⌨\u{fe0f}", "c", "d"])
    );
    pane.assert_no_wide_glyph_past_right_edge();
}

#[test]
fn inline_redraw_of_cjk_lines_that_fill_the_width_leaves_no_residue() {
    // Claude Code 这类 inline 程序（Ink）：自己按显示宽度折行，行与行之间写换行，
    // 重绘时逐行 EL + CUU 擦掉上一帧再整帧重写，从不依赖终端自动换行。
    let mut pane = WidePane::new(10, 8);
    pane.write("● 回答\r\n一二三四五\r\n六七八九x\r\n十".as_bytes());
    assert_eq!(
        pane.grid(),
        ["● 回答", "一二三四五", "六七八九x", "十", "", "", "", ""]
    );
    assert_eq!(pane.soft_wraps(), [false; 8]);

    let erase_four_lines = "\x1b[2K\x1b[1A\x1b[2K\x1b[1A\x1b[2K\x1b[1A\x1b[2K\x1b[G";
    pane.write(format!("{erase_four_lines}● 回答完毕\r\n甲乙丙丁戊\r\n己庚辛壬癸").as_bytes());
    assert_eq!(
        pane.grid(),
        ["● 回答完毕", "甲乙丙丁戊", "己庚辛壬癸", "", "", "", "", ""]
    );
    assert_eq!(pane.soft_wraps(), [false; 8]);
    assert_eq!(pane.ansi(), ["● 回答完毕", "甲乙丙丁戊", "己庚辛壬癸"]);
    assert_eq!(
        pane.render_row(1),
        cells(&["甲", "", "乙", "", "丙", "", "丁", "", "戊", ""])
    );
    pane.assert_no_wide_glyph_past_right_edge();

    // 主屏 resize 会重排：写满整行的中文在奇数宽度下留出 1 列占位格、整字折到下一行，
    // 变回原宽后合并，文字不丢不重。
    for (width, rows) in [
        (
            7,
            [
                "● 回答⏎",
                "完毕",
                "甲乙丙⏎",
                "丁戊",
                "己庚辛⏎",
                "壬癸",
                "",
                "",
            ],
        ),
        (
            10,
            ["● 回答完毕", "甲乙丙丁戊", "己庚辛壬癸", "", "", "", "", ""],
        ),
    ] {
        pane.resize(width, 8);
        assert_eq!(pane.grid(), rows, "{width} 列");
        assert_eq!(
            pane.unwrapped(),
            "● 回答完毕\n甲乙丙丁戊\n己庚辛壬癸",
            "{width} 列"
        );
        pane.assert_no_wide_glyph_past_right_edge();
    }
}

#[test]
fn halfwidth_katakana_voiced_grapheme_is_blank_only_when_cut() {
    let bytes = "ｶﾞZ".as_bytes();
    let mut pane = WidePane::new(4, 1);
    pane.write(bytes);
    for (width, expected) in [(1, cells(&[" "])), (2, cells(&["ｶﾞ", ""]))] {
        assert_eq!(pane.render_area(width)[0], expected);
        assert_eq!(first_patch_in_area(4, 1, bytes, width)[0], expected);
    }
    assert_eq!(pane.grid(), ["ｶﾞZ"]);
}
