//! 转写排版：各来源样例的区块与显示行、折叠、增量同步与整体解析一致、窄宽度。

use super::*;
use crate::i18n::{lang_guard, Lang};

const CODEX: &[&str] = &[
    "[message from /root] Task name: /root/print_probe Payload: (encrypted)",
    "(thinking)",
    "[exec] echo fake-probe",
    "    Script completed",
    "    fake-probe",
    "[shell] ls -la",
    "    total 0",
    "    … +3 lines",
    "[wait_agent] → Hume",
    "fake-probe",
    "second line",
    "[plan]",
    "    1. print the probe",
    "    2. report back",
    "[mystery_event]",
    "[task complete] fake-probe",
    "[unparsable line]",
];

const CLAUDE: &[&str] = &[
    "[user] fix the build",
    "[assistant] Sure. [thinking] [tool Bash] cargo build [tool Read] src/lib.rs",
    "[user] [tool result]",
    "    Compiling herdr",
    "    … +2 lines",
    "[tool error]",
    "    no such file",
    "[assistant] Done.",
];

const OPENCODE: &[&str] = &[
    "user: Find the docs",
    "(thinking) Look under docs/ first",
    "[grep] grep docs",
    "    docs/README.md",
    "[bash] sleep (running)",
    "Done anyway",
];

const CJK: &[&str] = &[
    "user: 请把子智能体页面的转写排版做好，长句要按显示宽度折行",
    "(thinking) 先看服务端写出的行格式，再决定区块怎么划分",
    "[exec] 运行测试并检查中文折行",
    "    测试输出第一行：所有宽字符都不能被拆成两半",
    "这是一段很长的中文助手正文，用来确认折行时不会超出给定的显示宽度，也不会拆开宽字符。",
    "[task complete] 全部完成，中文折行正常",
];

fn wide(ch: char) -> bool {
    unicode_width::UnicodeWidthChar::width(ch) == Some(2)
}

fn zh() -> crate::i18n::LangGuard {
    lang_guard(Lang::ZhCn)
}

fn build(lines: &[&str], first_line: u64, mode: TranscriptMode, width: u16) -> TranscriptView {
    let mut view = TranscriptView::default();
    view.sync(lines, first_line, mode);
    view.layout(width, "Hume");
    view
}

fn transcript(lines: &[&str], width: u16) -> TranscriptView {
    build(lines, 0, TranscriptMode::Transcript, width)
}

fn texts(view: &TranscriptView) -> Vec<String> {
    view.rows()
        .iter()
        .map(|row| {
            row.segments
                .iter()
                .map(|segment| segment.text.as_str())
                .collect()
        })
        .collect()
}

fn row_width(row: &TranscriptRow) -> usize {
    row.segments
        .iter()
        .map(|segment| crate::ui::display_width(&segment.text))
        .sum()
}

fn row_with<'a>(view: &'a TranscriptView, text: &str) -> &'a TranscriptRow {
    view.rows()
        .iter()
        .find(|row| {
            row.segments
                .iter()
                .map(|segment| segment.text.as_str())
                .collect::<String>()
                == text
        })
        .unwrap_or_else(|| panic!("缺少行 {text:?}: {:#?}", texts(view)))
}

fn toggles(view: &TranscriptView) -> Vec<u64> {
    view.rows().iter().filter_map(|row| row.toggle).collect()
}

#[test]
fn codex_transcript_is_split_into_blocks() {
    let _lang = zh();
    let view = transcript(CODEX, 60);
    assert_eq!(
        texts(&view),
        [
            "│ 来自 /root",
            "│ Task name: /root/print_probe Payload: (encrypted)",
            "",
            "∴ 思考",
            "▸ exec  echo fake-probe  +2 行",
            "▸ shell  ls -la  +4 行",
            "· wait_agent  → Hume",
            "",
            "Hume",
            "  fake-probe",
            "  second line",
            "",
            "计划",
            "  1. print the probe",
            "  2. report back",
            "",
            "[mystery_event]",
            "",
            "✓ 完成 fake-probe",
            "",
            "[unparsable line]",
        ]
    );
    // 工具行可折叠，开关值是区块首行的绝对行号派生的键。
    assert_eq!(toggles(&view), [block_key(2, 0), block_key(5, 0)]);
    let quote = row_with(&view, "│ Task name: /root/print_probe Payload: (encrypted)");
    assert_eq!(
        quote
            .segments
            .last()
            .map(|segment| (segment.text.as_str(), segment.tone)),
        Some(("(encrypted)", Tone::Meta))
    );
    let exec = row_with(&view, "▸ exec  echo fake-probe  +2 行");
    assert_eq!(exec.segments[0].tone, Tone::Toggle);
    assert_eq!(exec.segments[1].tone, Tone::ToolName);
    assert_eq!(view.stats().tool_calls, 3);
    assert_eq!(view.last_output(), Some("second line"));
}

#[test]
fn claude_results_pair_with_calls_in_order() {
    let _lang = zh();
    let view = transcript(CLAUDE, 60);
    assert_eq!(
        texts(&view),
        [
            "│ 用户",
            "│ fix the build",
            "",
            "Hume",
            "  Sure.",
            "",
            "∴ 思考",
            "▸ Bash  cargo build  +3 行",
            "▸ Read  src/lib.rs  +1 行",
            "",
            "Hume",
            "  Done.",
        ]
    );
    // 同一行的四个区块各有自己的键；报错的结果把工具名染成失败色。
    assert_eq!(toggles(&view), [block_key(1, 2), block_key(1, 3)]);
    let read = row_with(&view, "▸ Read  src/lib.rs  +1 行");
    assert_eq!(read.segments[1].tone, Tone::Failure);
    assert_eq!(view.stats().tool_calls, 2);
    assert_eq!(view.last_output(), Some("Done."));

    let mut view = view;
    assert!(view.toggle(block_key(1, 2)));
    assert!(view.toggle(block_key(1, 3)));
    view.layout(60, "Hume");
    let rows = texts(&view);
    let bash = rows
        .iter()
        .position(|row| row == "▾ Bash  cargo build")
        .expect("展开的 Bash");
    assert_eq!(
        rows[bash..bash + 5],
        [
            "▾ Bash  cargo build",
            "  │ Compiling herdr",
            "  │ … 另有 2 行未显示",
            "▾ Read  src/lib.rs",
            "  │ no such file",
        ]
    );
}

#[test]
fn opencode_thinking_with_a_note_is_foldable() {
    let _lang = zh();
    let mut view = transcript(OPENCODE, 60);
    assert_eq!(
        texts(&view),
        [
            "│ 用户",
            "│ Find the docs",
            "",
            "▸ 思考  Look under docs/ first",
            "▸ grep  grep docs  +1 行",
            "· bash  sleep (running)",
            "",
            "Hume",
            "  Done anyway",
        ]
    );
    assert!(view.toggle(block_key(1, 0)));
    view.layout(60, "Hume");
    let rows = texts(&view);
    assert!(rows.contains(&"▾ 思考".to_owned()), "{rows:#?}");
    assert!(
        rows.contains(&"  │ Look under docs/ first".to_owned()),
        "{rows:#?}"
    );
}

#[test]
fn plain_mode_only_wraps_lines() {
    let _lang = zh();
    let view = build(&["a\tb", "", "c"], 0, TranscriptMode::Plain, 20);
    assert_eq!(texts(&view), ["a    b", "", "c"]);
    assert!(view.rows().iter().all(|row| row.toggle.is_none()));
    assert_eq!(view.stats().tool_calls, 0);
    assert_eq!(view.last_output(), Some("c"));
    let wrapped = build(&["one two three four"], 0, TranscriptMode::Plain, 9);
    assert!(wrapped.rows().len() > 1);
    assert!(wrapped.rows().iter().all(|row| row_width(row) <= 9));
}

#[test]
fn cjk_text_wraps_within_the_width_without_splitting_characters() {
    let _lang = zh();
    let source: usize = CJK
        .iter()
        .map(|line| line.chars().filter(|ch| wide(*ch)).count())
        .sum();
    for width in 2..=40 {
        let mut view = transcript(CJK, width);
        view.toggle_all();
        view.layout(width, "Hume");
        for row in view.rows() {
            assert!(row_width(row) <= usize::from(width), "宽 {width}: {row:?}");
        }
        if width >= 30 {
            // 宽度够放下前缀时，折行不丢字：展开后所有宽字符都还在。
            let shown: usize = texts(&view)
                .iter()
                .map(|row| row.chars().filter(|ch| wide(*ch)).count())
                .sum();
            assert!(shown >= source, "宽 {width}: {shown} < {source}");
        }
    }
}

#[test]
fn toggle_and_toggle_all_switch_both_ways() {
    let _lang = zh();
    let mut view = transcript(CODEX, 60);
    let exec = block_key(2, 0);
    assert!(view.toggle(exec));
    view.layout(60, "Hume");
    assert!(texts(&view).contains(&"  │ Script completed".to_owned()));
    assert_eq!(
        row_with(&view, "▾ exec  echo fake-probe").toggle,
        Some(exec)
    );
    assert!(view.toggle(exec));
    view.layout(60, "Hume");
    assert!(!texts(&view).contains(&"  │ Script completed".to_owned()));

    // 不可折叠或不存在的区块不变。
    assert!(!view.toggle(block_key(8, 0)));
    assert!(!view.toggle(block_key(999, 0)));

    assert!(view.toggle_all());
    view.layout(60, "Hume");
    let expanded = texts(&view);
    assert!(expanded.contains(&"  │ Script completed".to_owned()));
    assert!(expanded.contains(&"  │ total 0".to_owned()));
    assert!(expanded.contains(&"  │ … 另有 3 行未显示".to_owned()));
    assert!(view.toggle_all());
    view.layout(60, "Hume");
    assert_eq!(texts(&view), texts(&transcript(CODEX, 60)));

    let mut plain = build(&["only text"], 0, TranscriptMode::Plain, 20);
    assert!(!plain.toggle_all());
}

/// 逐行追加时，增量结果与对同样内容整体解析的结果一致。
#[test]
fn appending_line_by_line_matches_a_full_parse() {
    let _lang = zh();
    for sample in [CODEX, CLAUDE, OPENCODE, CJK] {
        for width in [12, 40, 80] {
            let mut view = TranscriptView::default();
            for end in 0..=sample.len() {
                view.sync(&sample[..end], 0, TranscriptMode::Transcript);
                view.layout(width, "Hume");
                let full = transcript(&sample[..end], width);
                assert_eq!(view.rows(), full.rows(), "前 {end} 行，宽 {width}");
                assert_eq!(view.stats(), full.stats());
                assert_eq!(view.last_output(), full.last_output());
            }
        }
    }
}

/// 末行是还没写完的半行时会被替换；一次追加多行也照样一致。
#[test]
fn replacing_the_last_line_matches_a_full_parse() {
    let _lang = zh();
    let steps: &[&[&str]] = &[
        &["user: hi", "(thinking)", "[exec] cargo te"],
        &[
            "user: hi",
            "(thinking)",
            "[exec] cargo test",
            "    running 3 tests",
        ],
        &[
            "user: hi",
            "(thinking)",
            "[exec] cargo test",
            "    running 3 tests",
            "",
            "    ok",
        ],
        &[
            "user: hi",
            "(thinking)",
            "[exec] cargo test",
            "    running 3 tests",
            "",
            "    ok",
            "",
            "All green",
        ],
        &[
            "user: hi",
            "(thinking)",
            "[exec] cargo test",
            "    running 3",
        ],
    ];
    let mut view = TranscriptView::default();
    for lines in steps {
        view.sync(lines, 0, TranscriptMode::Transcript);
        view.layout(40, "Hume");
        assert_eq!(view.rows(), transcript(lines, 40).rows(), "{lines:?}");
    }
}

/// 输出里的空行要看下一行：下一行改成缩进输出时，前面的区块要重新解析。
#[test]
fn a_changed_line_after_a_blank_output_line_reparses_the_tool() {
    let _lang = zh();
    let before = ["[exec] make", "    building", "", "next prose"];
    let after = ["[exec] make", "    building", "", "    done"];
    let mut view = transcript(&before, 40);
    view.sync(&after, 0, TranscriptMode::Transcript);
    view.layout(40, "Hume");
    assert_eq!(view.rows(), transcript(&after, 40).rows());
    assert_eq!(texts(&view), ["▸ exec  make  +3 行"]);
}

#[test]
fn expanded_state_survives_appends_and_head_drops() {
    let _lang = zh();
    let mut view = transcript(CODEX, 60);
    let exec = block_key(2, 0);
    let shell = block_key(5, 0);
    assert!(view.toggle(exec));
    assert!(view.toggle(shell));
    let mut longer = CODEX.to_vec();
    longer.push("[exec] echo again");
    view.sync(&longer, 0, TranscriptMode::Transcript);
    view.layout(60, "Hume");
    assert!(texts(&view).contains(&"  │ Script completed".to_owned()));

    // 丢掉前 4 行：exec 的首行滚出内容，展开记录随之丢弃；shell 仍展开。
    view.sync(&longer[4..], 4, TranscriptMode::Transcript);
    view.layout(60, "Hume");
    assert!(!view.expanded.contains(&exec));
    assert!(view.expanded.contains(&shell));
    let mut fresh = build(&longer[4..], 4, TranscriptMode::Transcript, 60);
    assert!(fresh.toggle(shell));
    fresh.layout(60, "Hume");
    assert_eq!(view.rows(), fresh.rows());
    assert!(texts(&view).contains(&"  │ total 0".to_owned()));
}

#[test]
fn switching_modes_reparses_everything() {
    let _lang = zh();
    let mut view = transcript(CODEX, 60);
    view.sync(CODEX, 0, TranscriptMode::Plain);
    view.layout(60, "Hume");
    assert_eq!(
        view.rows(),
        build(CODEX, 0, TranscriptMode::Plain, 60).rows()
    );
    view.sync(CODEX, 0, TranscriptMode::Transcript);
    view.layout(60, "Hume");
    assert_eq!(view.rows(), transcript(CODEX, 60).rows());
}

#[test]
fn narrow_widths_never_panic_or_overflow() {
    let _lang = zh();
    for sample in [CODEX, CLAUDE, OPENCODE, CJK] {
        for mode in [TranscriptMode::Transcript, TranscriptMode::Plain] {
            for width in 0..=12 {
                let mut view = build(sample, 0, mode, width);
                view.toggle_all();
                view.layout(width, "Hume");
                if width >= 2 {
                    for row in view.rows() {
                        assert!(row_width(row) <= usize::from(width), "宽 {width}: {row:?}");
                    }
                }
            }
        }
    }
}

#[test]
fn layout_is_cached_until_something_changes() {
    let _lang = zh();
    let mut view = transcript(OPENCODE, 0);
    assert_eq!(view.laid, Some((0, "Hume".to_owned())));
    assert_eq!(view.dirty, None);
    let before = view.rows().to_vec();
    view.layout(0, "Hume");
    assert_eq!(view.rows(), before.as_slice());

    // 小标题变化要重排。
    view.layout(60, "Noor");
    assert!(texts(&view).contains(&"Noor".to_owned()));
    // 空小标题时助手正文不缩进。
    view.layout(60, "");
    assert!(texts(&view).contains(&"Done anyway".to_owned()));
}

#[test]
fn tool_header_keeps_the_line_count_when_the_summary_is_long() {
    let _lang = zh();
    let view = transcript(
        &[
            "[exec] cargo test --workspace --all-features --locked -- --nocapture",
            "    ok",
        ],
        30,
    );
    let rows = texts(&view);
    assert_eq!(rows.len(), 1);
    assert!(rows[0].ends_with("…  +1 行"), "{rows:?}");
    assert!(row_width(&view.rows()[0]) <= 30);
}

#[test]
fn inter_agent_messages_and_outcomes_have_their_own_blocks() {
    let _lang = zh();
    let view = transcript(
        &[
            "[message from Hume → Noor] check the docs",
            "[message] (encrypted)",
            "[context compacted]",
            "[turn aborted] interrupted",
            "[task failed] boom",
        ],
        60,
    );
    assert_eq!(
        texts(&view),
        [
            "│ 来自 Hume → Noor",
            "│ check the docs",
            "",
            "│ 消息",
            "│ (encrypted)",
            "",
            "── 上下文已压缩 ──",
            "",
            "✕ 已中断 interrupted",
            "",
            "✕ 失败 boom",
        ]
    );
}

#[test]
fn english_labels_follow_the_language() {
    let _lang = lang_guard(Lang::En);
    let view = transcript(OPENCODE, 60);
    let rows = texts(&view);
    assert!(rows.contains(&"│ User".to_owned()), "{rows:#?}");
    assert!(
        rows.contains(&"▸ grep  grep docs  +1 lines".to_owned()),
        "{rows:#?}"
    );
}

/// Claude 一行里的并行工具调用再多，各自的折叠键也互不冲突。
#[test]
fn many_blocks_on_one_line_toggle_independently() {
    let _lang = zh();
    let calls: String = (0..20)
        .map(|index| format!(" [tool T{index}] arg{index}"))
        .collect();
    let mut lines = vec![format!("[assistant]{calls}")];
    for index in 0..20 {
        lines.push("[user] [tool result]".to_owned());
        lines.push(format!("    out{index}"));
    }
    let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
    let mut view = transcript(&lines, 60);
    let keys = toggles(&view);
    assert_eq!(keys.len(), 20);
    let unique: HashSet<u64> = keys.iter().copied().collect();
    assert_eq!(unique.len(), 20, "键互不相同");
    assert_eq!(keys[17], block_key(0, 17));

    assert!(view.toggle(keys[17]));
    view.layout(60, "Hume");
    let rows = texts(&view);
    assert!(rows.contains(&"▾ T17  arg17".to_owned()), "{rows:#?}");
    assert!(rows.contains(&"  │ out17".to_owned()), "{rows:#?}");
    assert!(
        rows.contains(&"▸ T16  arg16  +1 行".to_owned()),
        "{rows:#?}"
    );
    assert!(
        rows.contains(&"▸ T18  arg18  +1 行".to_owned()),
        "{rows:#?}"
    );
    assert!(!rows.contains(&"  │ out16".to_owned()), "{rows:#?}");
}

fn wrapped(text: &str, width: usize) -> Vec<String> {
    let mut rows = Vec::new();
    wrap_cjk(text, width, width, |start, end| {
        rows.push(text[start..end].to_owned())
    });
    rows
}

/// 中文没有空格：折行在字与字之间断开，不把混排句子提前断在行首附近的空格处。
#[test]
fn chinese_text_breaks_between_characters_instead_of_at_early_spaces() {
    let rows = wrapped(
        "审计 herdr 全部 TUI 界面，按严重度输出问题清单和可直接实施的修改建议",
        24,
    );
    // 「界面，」放不下：逗号不放行首，连同前一个字移到下一行。
    assert_eq!(rows[0], "审计 herdr 全部 TUI 界");
    for row in &rows {
        assert!(crate::ui::display_width(row) <= 24, "{row:?}");
    }
    assert_eq!(
        rows.concat(),
        "审计 herdr 全部 TUI 界面，按严重度输出问题清单和可直接实施的修改建议"
    );
}

#[test]
fn closing_punctuation_never_starts_a_line_and_opening_never_ends_one() {
    // 宽 10 列：第五个字后正好满行，逗号不能落到下一行行首。
    let rows = wrapped("一二三四五，六七八九十", 10);
    assert!(rows.iter().all(|row| !row.starts_with('，')), "{rows:?}");
    let rows = wrapped("一二三四（五六七八九", 10);
    assert!(rows.iter().all(|row| !row.ends_with('（')), "{rows:?}");
}

#[test]
fn latin_words_still_wrap_at_spaces_and_long_words_hard_break() {
    assert_eq!(
        wrapped("one two three four", 9),
        ["one two ", "three ", "four"]
    );
    assert_eq!(wrapped("abcdefghijkl", 5), ["abcde", "fghij", "kl"]);
    assert_eq!(wrapped("", 5), [""]);
    assert_eq!(wrapped("a\nb", 5), ["a", "b"]);
}

/// 折行以字素为单位：emoji 连接序列与组合字符不会被拆到两行。
#[test]
fn wrapping_and_truncation_keep_graphemes_whole() {
    let family = "👩\u{200d}💻";
    for width in 2..=6 {
        let rows = wrapped(&format!("a {family} b"), width);
        assert!(
            rows.iter().any(|row| row.contains(family)),
            "宽 {width}: {rows:?}"
        );
        assert!(
            rows.iter().all(|row| !row.ends_with('\u{200d}')),
            "宽 {width}: {rows:?}"
        );
    }
    let accented = "e\u{301}";
    let rows = wrapped(&format!("abcd{accented}fg"), 5);
    assert!(rows.iter().any(|row| row.contains(accented)), "{rows:?}");
    // 截断同样不拆字素。
    let row = fit_row(
        vec![(format!("ab{family}{family}{family}"), Tone::Body)],
        5,
        None,
    );
    let text: String = row
        .segments
        .iter()
        .map(|segment| segment.text.as_str())
        .collect();
    assert!(
        !text.contains('\u{200d}') || text.contains(family),
        "{text:?}"
    );
    assert!(text.ends_with('…'), "{text:?}");
}

/// 只有空格的行首不是断点：缩进不会自成一行。
#[test]
fn leading_indentation_is_not_a_break_point() {
    assert_eq!(wrapped(" abcdef", 6), [" abcde", "f"]);
    assert_eq!(
        wrapped("    缩进的中文正文需要折行", 12),
        ["    缩进的中", "文正文需要折", "行"]
    );
}
