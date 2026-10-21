//! 子智能体页面的转写排版（客户端）：把 server 返回的 Text 转写解析成区块（引用、
//! 助手正文、思考、工具调用及其输出、计划、完成、错误、压缩分隔、其余元信息），再按
//! 显示宽度折成带色调的显示行。
//!
//! - 只用于 `AgentActivityContentFormat::Text`；Markdown / Jsonl 仍走活动页原有的按行
//!   着色。
//! - 纯数据：不碰缓冲区、不取调色板。色调（[`Tone`]）由页面渲染映射到 Graphite 样式。
//! - 视图计算阶段调用 [`TranscriptView::sync`] / [`TranscriptView::layout`]，渲染只读
//!   [`TranscriptView::rows`]（STATE-04）。
//!
//! 输入是各来源适配器写出的行格式（`server::agent_activity::{codex, claude, opencode}`）：
//! `user: …`、`(thinking)`、`[工具] 摘要` 加 4 格缩进的输出、`[task complete] …`、
//! `[message from A → B] …`，以及 Claude 的 `[assistant] … [tool Bash] …` 与
//! `[user] [tool result]`（输出块之后的行内片段另起一行、不带角色前缀）。认不出的行
//! 当正文或元信息显示，不丢内容。
//!
//! 增量：常见变化是末尾追加行、末行（未完结的半行）被替换、头部因内容上限被丢弃。
//! 首行与模式不变时，`sync` 从受影响的区块起接着解析，`layout` 从第一个变化的区块
//! 起重排；头部丢弃、模式、宽度或小标题变化时整体重来。
//!
//! 已知局限：行格式里多行用户消息与紧随其后的助手正文无法区分，`user:` 引用之后
//! 紧跟的非标记行会并进引用；Claude 用户消息里跟在工具结果之后的正文按助手正文显示。
//!
//! 公开接口是子智能体页面与本模块之间的约定，改动要同时照顾页面一侧。

use std::collections::{HashSet, VecDeque};

use unicode_width::UnicodeWidthChar;

use super::super::observability::tr;
use super::super::render::wrap_text;

/// 原文的解析方式：转写格式明确的来源（codex / claude / opencode）按区块解析；其余
/// （kimi 任务输出、pi 的流式文本、外部来源等）每行都当正文，只折行。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::client::shell) enum TranscriptMode {
    Transcript,
    #[default]
    Plain,
}

/// 一段文字的色调；页面渲染负责映射到样式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::client::shell) enum Tone {
    /// 正文（`design::body`）。
    Body,
    /// 小标题：助手名、引用标题（`design::title`）。
    Heading,
    /// 说明（`design::muted`）：工具摘要、引用正文、思考摘要。
    Muted,
    /// 元信息（`design::meta`）：思考行、隐藏行数、未识别的方括号行。
    Meta,
    /// 结构线（`design::rule`）：引用竖线、输出边线、压缩分隔线。
    Rule,
    /// 展开 / 收起开关 `▸` `▾`（`design::meta`）。
    Toggle,
    /// 工具名（`design::tool_name`）。
    ToolName,
    /// 失败的工具名、错误标记与错误正文（失败状态色）。
    Failure,
    /// 完成标记（待查看状态色）。
    Success,
    /// 工具输出正文（`design::muted`）。
    Output,
}

/// 一段同色调的文字。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::client::shell) struct Segment {
    pub(in crate::client::shell) text: String,
    pub(in crate::client::shell) tone: Tone,
}

/// 折好的一行：从左到右的若干段，总显示宽度不超过 [`TranscriptView::layout`] 的宽度
/// （宽度为 1 时放不下的宽字符除外）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(in crate::client::shell) struct TranscriptRow {
    pub(in crate::client::shell) segments: Vec<Segment>,
    /// 可折叠区块（有输出的工具调用、带摘要的思考）的标题行：点击切换该区块的展开
    /// 状态，值原样交给 [`TranscriptView::toggle`]。值由区块首行的绝对行号派生，追加
    /// 内容与丢弃头部时保持不变。
    pub(in crate::client::shell) toggle: Option<u64>,
}

/// 概要面板用的统计。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::client::shell) struct TranscriptStats {
    /// 已加载内容里的工具调用数（头部被丢弃的不计）。
    pub(in crate::client::shell) tool_calls: usize,
}

/// 适配器给工具输出、计划条目加的缩进。
const INDENT: &str = "    ";
/// 区块键里同一行序号占的低位数：首行绝对行号放高位，同一行（Claude 一行里可能有
/// 很多并行的工具调用）的区块互不冲突。
const SUB_BITS: u32 = 32;
/// 方括号标签的长度上限：更长的不当作标签。
const MAX_TAG_CHARS: usize = 96;
/// 工具标题行里保住「+N 行」时，前面至少留给开关与名字开头的列数。
const MIN_HEAD_COLUMNS: usize = 4;

/// 一个节点的转写视图：原文行 → 区块 → 显示行。
#[derive(Debug, Default)]
pub(in crate::client::shell) struct TranscriptView {
    /// 当前保留的原文行；`lines[0]` 的绝对行号是 `first_line`。
    lines: Vec<String>,
    first_line: u64,
    mode: TranscriptMode,
    blocks: Vec<Block>,
    /// 每个工具块配对上的 Claude 工具结果块下标（与 `blocks` 等长）。
    results: Vec<Option<usize>>,
    /// 已配给某个工具块的结果块：布局时不单独出行（与 `blocks` 等长）。
    claimed: Vec<bool>,
    /// 展开的区块（[`Block::key`]）。
    expanded: HashSet<u64>,
    rows: Vec<TranscriptRow>,
    /// 每个已布局区块的第一行（含区块前的空行）在 `rows` 里的下标。
    block_rows: Vec<usize>,
    /// 最近一次布局的 (宽度, 小标题)；`None` = 需要整体重排。
    laid: Option<(u16, String)>,
    /// 从这个区块下标起需要重新布局；`None` = 布局与内容一致。
    dirty: Option<usize>,
}

impl TranscriptView {
    /// 原文变化后调用：`lines` 是当前保留的全部原文行（不含换行符），`first_line` 是
    /// `lines[0]` 在整个内容里的绝对行号（客户端从头部丢弃过多少行）。首行与模式不变
    /// 时只重新解析受影响的尾部；头部丢弃或 `mode` 变化时整体重解析，并丢掉已滚出
    /// 内容的展开记录。
    pub(in crate::client::shell) fn sync<L: AsRef<str>>(
        &mut self,
        lines: &[L],
        first_line: u64,
        mode: TranscriptMode,
    ) {
        if self.unchanged(lines, first_line, mode) {
            return;
        }
        if mode != self.mode || first_line != self.first_line || self.lines.is_empty() {
            self.lines = lines.iter().map(|line| line.as_ref().to_owned()).collect();
            self.first_line = first_line;
            self.mode = mode;
            self.blocks.clear();
            self.parse_from(0);
            self.associate();
            let floor = block_key(first_line, 0);
            self.expanded.retain(|key| *key >= floor);
            self.laid = None;
            self.dirty = Some(0);
            return;
        }

        let common = self.lines.len().min(lines.len());
        let changed = (0..common)
            .find(|&index| self.lines[index] != lines[index].as_ref())
            .unwrap_or(common);
        self.lines.truncate(changed);
        self.lines
            .extend(lines[changed..].iter().map(|line| line.as_ref().to_owned()));

        // 第 changed-1 行的判定要看下一行（缩进输出里的空行、没有摘要的工具行），所以
        // 从包含它或恰在它处结束的区块起重解析；同一行拆出的几个区块一起重来。
        let mut keep = if changed == 0 {
            0
        } else {
            let probe = first_line + (changed - 1) as u64;
            self.blocks
                .iter()
                .position(|block| block.end >= probe)
                .unwrap_or(self.blocks.len())
        };
        while keep > 0
            && keep < self.blocks.len()
            && self.blocks[keep - 1].line == self.blocks[keep].line
        {
            keep -= 1;
        }
        let restart = match self.blocks.get(keep) {
            Some(block) => block.line,
            None => self.blocks.last().map_or(first_line, |block| block.end),
        };
        self.blocks.truncate(keep);
        self.parse_from(restart.saturating_sub(first_line) as usize);

        let previous = std::mem::take(&mut self.results);
        self.associate();
        // Claude 的工具结果在调用之后很多行才到：配对变化、或配对的结果块被重新解析的
        // 旧工具块（标题里的行数取自结果）也要重排。
        let paired = (0..keep.min(self.results.len()))
            .find(|&index| {
                previous.get(index) != self.results.get(index)
                    || self.results[index].is_some_and(|result| result >= keep)
            })
            .unwrap_or(keep);
        self.mark_dirty(paired);
    }

    /// 按 `width` 列折行。宽度、`speaker`、内容与展开状态都没变时直接返回。`speaker` 是
    /// 助手正文的小标题（节点昵称）；空串时不画小标题。
    pub(in crate::client::shell) fn layout(&mut self, width: u16, speaker: &str) {
        let same_frame = self
            .laid
            .as_ref()
            .is_some_and(|(laid_width, laid_speaker)| {
                *laid_width == width && laid_speaker == speaker
            });
        let from = if same_frame {
            match self.dirty {
                None => return,
                Some(from) => from.min(self.block_rows.len()).min(self.blocks.len()),
            }
        } else {
            0
        };
        let row_start = if from == 0 {
            0
        } else {
            self.block_rows
                .get(from)
                .copied()
                .unwrap_or(self.rows.len())
        };
        let mut previous = (0..from).rev().find_map(|index| {
            let start = self.block_rows[index];
            let end = self.block_rows.get(index + 1).copied().unwrap_or(row_start);
            (start < end).then(|| group(&self.blocks[index].kind, self.mode))
        });
        self.rows.truncate(row_start);
        self.block_rows.truncate(from);

        let room = usize::from(width.max(1));
        let mut content = Vec::new();
        for index in from..self.blocks.len() {
            self.block_rows.push(self.rows.len());
            content.clear();
            self.layout_block(index, room, speaker, &mut content);
            if content.is_empty() {
                continue;
            }
            let current = group(&self.blocks[index].kind, self.mode);
            if previous
                .is_some_and(|previous| previous == Group::Spaced || current == Group::Spaced)
            {
                self.rows.push(TranscriptRow::default());
            }
            self.rows.append(&mut content);
            previous = Some(current);
        }
        self.laid = Some((width, speaker.to_owned()));
        self.dirty = None;
    }

    /// 最近一次 [`Self::layout`] 的显示行。
    pub(in crate::client::shell) fn rows(&self) -> &[TranscriptRow] {
        &self.rows
    }

    /// 切换一个可折叠区块的展开状态（参数取自 [`TranscriptRow::toggle`]）。返回是否有
    /// 变化；有变化时下一次 [`Self::layout`] 从该区块起重排。
    pub(in crate::client::shell) fn toggle(&mut self, block: u64) -> bool {
        let Some(index) = self.blocks.iter().position(|item| item.key() == block) else {
            return false;
        };
        if !self.foldable(index) {
            return false;
        }
        if !self.expanded.remove(&block) {
            self.expanded.insert(block);
        }
        self.mark_dirty(index);
        true
    }

    /// Enter：有收起的可折叠区块就全部展开，否则全部收起。返回是否有变化。
    pub(in crate::client::shell) fn toggle_all(&mut self) -> bool {
        let foldable: Vec<(usize, u64)> = (0..self.blocks.len())
            .filter(|&index| self.foldable(index))
            .map(|index| (index, self.blocks[index].key()))
            .collect();
        let Some(&(first, _)) = foldable.first() else {
            return false;
        };
        let expand = foldable.iter().any(|(_, key)| !self.expanded.contains(key));
        for (_, key) in &foldable {
            if expand {
                self.expanded.insert(*key);
            } else {
                self.expanded.remove(key);
            }
        }
        self.mark_dirty(first);
        true
    }

    pub(in crate::client::shell) fn stats(&self) -> TranscriptStats {
        TranscriptStats {
            tool_calls: self
                .blocks
                .iter()
                .filter(|block| matches!(block.kind, Kind::Tool { .. }))
                .count(),
        }
    }

    /// 最近一行非空的工具输出或助手正文（概要「最近输出」），已折成单行。
    pub(in crate::client::shell) fn last_output(&self) -> Option<&str> {
        self.blocks.iter().rev().find_map(|block| {
            let lines: &[String] = match &block.kind {
                Kind::Tool { output, .. }
                | Kind::ToolResult { output, .. }
                | Kind::Output { output, .. } => output,
                Kind::Assistant { body } => body,
                Kind::Plain { text } => std::slice::from_ref(text),
                _ => return None,
            };
            lines
                .iter()
                .rev()
                .map(|line| line.trim())
                .find(|line| !line.is_empty())
        })
    }

    /// 内容、首行与模式都没变。页面只在内容版本变化时调用 `sync`，这里逐行确认即可。
    fn unchanged<L: AsRef<str>>(&self, lines: &[L], first_line: u64, mode: TranscriptMode) -> bool {
        mode == self.mode
            && first_line == self.first_line
            && lines.len() == self.lines.len()
            && lines
                .iter()
                .rev()
                .zip(self.lines.iter().rev())
                .all(|(new, old)| new.as_ref() == old)
    }

    fn parse_from(&mut self, from: usize) {
        match self.mode {
            TranscriptMode::Plain => {
                parse_plain(&mut self.blocks, &self.lines, self.first_line, from);
            }
            TranscriptMode::Transcript => {
                parse_transcript(&mut self.blocks, &self.lines, self.first_line, from);
            }
        }
    }

    fn mark_dirty(&mut self, index: usize) {
        self.dirty = Some(self.dirty.map_or(index, |dirty| dirty.min(index)));
    }

    /// Claude 的工具结果与调用配对：先进先出（并行调用的结果按调用顺序回来），遇到新的
    /// 用户消息或回合结束时清空待配对的调用。Codex / OpenCode 的输出直接跟在调用后面。
    fn associate(&mut self) {
        self.results = vec![None; self.blocks.len()];
        self.claimed = vec![false; self.blocks.len()];
        let mut pending: VecDeque<usize> = VecDeque::new();
        for (index, block) in self.blocks.iter().enumerate() {
            match &block.kind {
                Kind::Tool { output, hidden, .. } if output.is_empty() && *hidden == 0 => {
                    pending.push_back(index);
                }
                Kind::ToolResult { .. } => {
                    if let Some(tool) = pending.pop_front() {
                        self.results[tool] = Some(index);
                        self.claimed[index] = true;
                    }
                }
                Kind::Quote { .. } | Kind::Outcome { .. } => pending.clear(),
                _ => {}
            }
        }
    }

    /// 区块是否可折叠：有输出（自身或配对的结果）的工具、带摘要的思考。
    fn foldable(&self, index: usize) -> bool {
        match &self.blocks[index].kind {
            Kind::Tool { .. } => {
                let (output, hidden, _) = self.tool_output(index);
                !output.is_empty() || hidden > 0
            }
            Kind::Thinking { notes, .. } => !notes.is_empty(),
            _ => false,
        }
    }

    /// 工具块的输出：自身的，或配对上的 Claude 结果；以及结果是否报错。
    fn tool_output(&self, index: usize) -> (&[String], u64, bool) {
        let Kind::Tool { output, hidden, .. } = &self.blocks[index].kind else {
            return (&[], 0, false);
        };
        if !output.is_empty() || *hidden > 0 {
            return (output, *hidden, false);
        }
        match self
            .results
            .get(index)
            .copied()
            .flatten()
            .map(|result| &self.blocks[result].kind)
        {
            Some(Kind::ToolResult {
                failed,
                output,
                hidden,
            }) => (output, *hidden, *failed),
            _ => (&[], 0, false),
        }
    }

    fn layout_block(
        &self,
        index: usize,
        width: usize,
        speaker: &str,
        out: &mut Vec<TranscriptRow>,
    ) {
        let block = &self.blocks[index];
        let key = block.key();
        let expanded = self.expanded.contains(&key);
        match &block.kind {
            Kind::Plain { text } => {
                let text = expand_tabs(text);
                push_wrapped(out, &[], &[], &[(text.as_str(), Tone::Body)], width);
            }
            Kind::Quote { title, body } => {
                out.push(fit_row(
                    vec![
                        ("│ ".to_owned(), Tone::Rule),
                        (title.label(), Tone::Heading),
                    ],
                    width,
                    None,
                ));
                for line in trim_blank_tail(body) {
                    let line = expand_tabs(line);
                    let spans = encrypted_spans(&line, Tone::Muted);
                    push_wrapped(
                        out,
                        &[("│ ", Tone::Rule)],
                        &[("│ ", Tone::Rule)],
                        &spans,
                        width,
                    );
                }
            }
            Kind::Assistant { body } => {
                let body = trim_blank_tail(body);
                if body.is_empty() {
                    return;
                }
                let indent: &[(&str, Tone)] = if speaker.is_empty() {
                    &[]
                } else {
                    out.push(fit_row(
                        vec![(speaker.to_owned(), Tone::Heading)],
                        width,
                        None,
                    ));
                    &[("  ", Tone::Body)]
                };
                for line in body {
                    let line = expand_tabs(line);
                    push_wrapped(out, indent, indent, &[(line.as_str(), Tone::Body)], width);
                }
            }
            Kind::Thinking { count, notes } => {
                let foldable = !notes.is_empty();
                let mut head = Vec::with_capacity(5);
                head.push(if foldable {
                    (toggle_glyph(expanded).to_owned(), Tone::Toggle)
                } else {
                    ("∴ ".to_owned(), Tone::Meta)
                });
                head.push((tr("Thinking", "思考").to_owned(), Tone::Meta));
                if *count > 1 {
                    head.push((format!(" ×{count}"), Tone::Meta));
                }
                if !expanded {
                    if let Some(note) = notes.first() {
                        head.push(("  ".to_owned(), Tone::Meta));
                        head.push((expand_tabs(note), Tone::Muted));
                    }
                }
                out.push(fit_row(head, width, foldable.then_some(key)));
                if foldable && expanded {
                    for note in notes {
                        let note = expand_tabs(note);
                        push_wrapped(
                            out,
                            &[("  │ ", Tone::Rule)],
                            &[("  │ ", Tone::Rule)],
                            &[(note.as_str(), Tone::Muted)],
                            width,
                        );
                    }
                }
            }
            Kind::Tool { name, summary, .. } => {
                let (output, hidden, failed) = self.tool_output(index);
                let foldable = !output.is_empty() || hidden > 0;
                let mut head = Vec::with_capacity(4);
                head.push(if foldable {
                    (toggle_glyph(expanded).to_owned(), Tone::Toggle)
                } else {
                    ("· ".to_owned(), Tone::Meta)
                });
                head.push((
                    name.clone(),
                    if failed {
                        Tone::Failure
                    } else {
                        Tone::ToolName
                    },
                ));
                if !summary.is_empty() {
                    head.push(("  ".to_owned(), Tone::Muted));
                    head.push((expand_tabs(summary), Tone::Muted));
                }
                let mut tail = Vec::new();
                if foldable && !expanded {
                    let count = (trim_blank_tail(output).len() as u64)
                        .saturating_add(hidden)
                        .to_string();
                    let label = crate::i18n::fill(tr("+{n} lines", "+{n} 行"), &[("n", &count)]);
                    tail.push((format!("  {label}"), Tone::Meta));
                }
                out.push(fit_row_tail(head, tail, width, foldable.then_some(key)));
                if foldable && expanded {
                    push_output(out, output, hidden, width);
                }
            }
            Kind::ToolResult {
                failed,
                output,
                hidden,
            } => {
                if self.claimed.get(index).copied().unwrap_or(false) {
                    return;
                }
                let (label, tone) = if *failed {
                    (tr("Tool error", "工具错误"), Tone::Failure)
                } else {
                    (tr("Tool result", "工具结果"), Tone::Meta)
                };
                out.push(fit_row(
                    vec![("· ".to_owned(), Tone::Meta), (label.to_owned(), tone)],
                    width,
                    None,
                ));
                push_output(out, output, *hidden, width);
            }
            Kind::Output { output, hidden } => push_output(out, output, *hidden, width),
            Kind::Plan { items, hidden } => {
                out.push(fit_row(
                    vec![(tr("Plan", "计划").to_owned(), Tone::Heading)],
                    width,
                    None,
                ));
                for item in trim_blank_tail(items) {
                    let item = expand_tabs(item);
                    push_wrapped(
                        out,
                        &[("  ", Tone::Body)],
                        &[("  ", Tone::Body)],
                        &[(item.as_str(), Tone::Body)],
                        width,
                    );
                }
                if *hidden > 0 {
                    let label = more_lines(*hidden);
                    push_wrapped(
                        out,
                        &[("  ", Tone::Body)],
                        &[("  ", Tone::Body)],
                        &[(label.as_str(), Tone::Meta)],
                        width,
                    );
                }
            }
            Kind::Outcome { outcome, message } => {
                let (mark, tone, label, body) = match outcome {
                    Outcome::Done => ("✓ ", Tone::Success, tr("Done", "完成"), Tone::Body),
                    Outcome::Failed => ("✕ ", Tone::Failure, tr("Failed", "失败"), Tone::Failure),
                    Outcome::Aborted => (
                        "✕ ",
                        Tone::Failure,
                        tr("Interrupted", "已中断"),
                        Tone::Muted,
                    ),
                    Outcome::Error => ("✕ ", Tone::Failure, tr("Error", "错误"), Tone::Failure),
                };
                let message = expand_tabs(message);
                let mut spans: Vec<(&str, Tone)> = vec![(mark, tone), (label, tone)];
                if !message.is_empty() {
                    spans.push((" ", body));
                    spans.push((message.as_str(), body));
                }
                push_wrapped(out, &[], &[("  ", Tone::Body)], &spans, width);
            }
            Kind::Compacted => {
                let label = format!("── {} ──", tr("context compacted", "上下文已压缩"));
                out.push(fit_row(vec![(label, Tone::Rule)], width, None));
            }
            Kind::Meta { text } => {
                let text = expand_tabs(text);
                push_wrapped(out, &[], &[], &[(text.as_str(), Tone::Meta)], width);
            }
        }
    }
}

/// 一个区块：来自 `[line, end)` 的原文（同一行可拆出多个区块，用 `sub` 区分）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Block {
    /// 首行的绝对行号。
    line: u64,
    /// 同一行拆出的第几个区块。
    sub: u32,
    /// 来源原文的结束行（不含，绝对行号）。
    end: u64,
    kind: Kind,
}

impl Block {
    fn key(&self) -> u64 {
        block_key(self.line, self.sub)
    }
}

/// 区块键：首行绝对行号在高位、同一行里的序号在低 [`SUB_BITS`] 位。追加内容与丢弃
/// 头部时不变；行号大到放不下时饱和（实际转写远到不了）。
fn block_key(line: u64, sub: u32) -> u64 {
    line.saturating_mul(1 << SUB_BITS) | u64::from(sub)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Kind {
    /// 父 agent 或用户发来的消息。
    Quote {
        title: Title,
        body: Vec<String>,
    },
    /// 助手正文（连续多行合成一段）。
    Assistant {
        body: Vec<String>,
    },
    /// 连续的思考：条数与其中的摘要。
    Thinking {
        count: usize,
        notes: Vec<String>,
    },
    /// 工具调用；Codex / OpenCode 的输出紧跟在调用之后，存在这里。
    Tool {
        name: String,
        summary: String,
        output: Vec<String>,
        hidden: u64,
    },
    /// Claude 的工具结果（`[tool result]` / `[tool error]`），布局时配给调用。
    ToolResult {
        failed: bool,
        output: Vec<String>,
        hidden: u64,
    },
    /// 找不到调用可挂的缩进输出。
    Output {
        output: Vec<String>,
        hidden: u64,
    },
    Plan {
        items: Vec<String>,
        hidden: u64,
    },
    Outcome {
        outcome: Outcome,
        message: String,
    },
    Compacted,
    /// 其余方括号行（未识别的记录类型、包装标签、占位等）。
    Meta {
        text: String,
    },
    /// Plain 模式的一行。
    Plain {
        text: String,
    },
}

/// 引用的小标题；文案在布局时按当前语言取。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Title {
    User,
    Message,
    /// `[message from …]` 里的发送方（可能带 `→ 接收方`）。
    From(String),
}

impl Title {
    fn label(&self) -> String {
        match self {
            Self::User => tr("User", "用户").to_owned(),
            Self::Message => tr("Message", "消息").to_owned(),
            Self::From(from) => format!("{} {from}", tr("From", "来自")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Done,
    Failed,
    Aborted,
    Error,
}

/// 区块之间要不要空一行：工具、思考与元信息紧挨着排，正文类区块前后各空一行。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Group {
    Tight,
    Spaced,
}

fn group(kind: &Kind, mode: TranscriptMode) -> Group {
    if mode == TranscriptMode::Plain {
        return Group::Tight;
    }
    match kind {
        Kind::Quote { .. }
        | Kind::Assistant { .. }
        | Kind::Plan { .. }
        | Kind::Outcome { .. }
        | Kind::Compacted => Group::Spaced,
        Kind::Thinking { .. }
        | Kind::Tool { .. }
        | Kind::ToolResult { .. }
        | Kind::Output { .. }
        | Kind::Meta { .. }
        | Kind::Plain { .. } => Group::Tight,
    }
}

/// 当前区块还在接收哪类续行。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Open {
    None,
    /// 缩进行是输出 / 计划条目。
    Output,
    /// 非标记行都是正文续行（多行的 user 消息、助手正文）。
    Prose,
}

fn parse_plain(blocks: &mut Vec<Block>, lines: &[String], base: u64, from: usize) {
    for (index, text) in lines.iter().enumerate().skip(from) {
        let line = base + index as u64;
        blocks.push(Block {
            line,
            sub: 0,
            end: line + 1,
            kind: Kind::Plain { text: text.clone() },
        });
    }
}

/// 从 `lines[from]` 起解析，接在 `blocks` 已有的区块之后（合并思考、给同一行的区块
/// 编号都要看前一个区块）。`from` 必须是某个区块的首行或上一区块之后，此时解析状态
/// 与从头解析一致。
fn parse_transcript(blocks: &mut Vec<Block>, lines: &[String], base: u64, from: usize) {
    let mut open = Open::None;
    let mut index = from.min(lines.len());
    while index < lines.len() {
        let raw = lines[index].as_str();
        let line = base + index as u64;
        let next = lines.get(index + 1).map(String::as_str);
        if open == Open::Output {
            if let Some(text) = raw.strip_prefix(INDENT) {
                if let Some(last) = blocks.last_mut() {
                    push_output_line(&mut last.kind, text);
                    last.end = line + 1;
                }
                index += 1;
                continue;
            }
            if raw.trim().is_empty() {
                // 输出中间的空行：后面还有缩进行（或已到末尾、还看不出来）时算输出。
                if next.is_none_or(|next| next.starts_with(INDENT)) {
                    if let Some(last) = blocks.last_mut() {
                        push_output_line(&mut last.kind, "");
                        last.end = line + 1;
                    }
                } else {
                    open = Open::None;
                }
                index += 1;
                continue;
            }
            open = Open::None;
        }
        if open == Open::Prose && !is_marker(raw) {
            if let Some(last) = blocks.last_mut() {
                if let Kind::Quote { body, .. } | Kind::Assistant { body } = &mut last.kind {
                    body.push(raw.to_owned());
                }
                last.end = line + 1;
            }
            index += 1;
            continue;
        }
        open = start_line(blocks, raw, line, next);
        index += 1;
    }
}

/// 把一行缩进输出记到区块上；服务端注明的 `… +N lines` 只计数。
fn push_output_line(kind: &mut Kind, text: &str) {
    let (output, hidden) = match kind {
        Kind::Tool { output, hidden, .. }
        | Kind::ToolResult { output, hidden, .. }
        | Kind::Output { output, hidden }
        | Kind::Plan {
            items: output,
            hidden,
        } => (output, hidden),
        _ => return,
    };
    match hidden_count(text) {
        Some(count) => *hidden = hidden.saturating_add(count),
        None => output.push(text.to_owned()),
    }
}

fn hidden_count(text: &str) -> Option<u64> {
    let rest = text.strip_prefix("… +")?;
    rest.strip_suffix(" lines")
        .or_else(|| rest.strip_suffix(" line"))?
        .parse()
        .ok()
}

/// 这一行是否开启新的区块（结束正文续行）。
fn is_marker(raw: &str) -> bool {
    claude_role(raw).is_some()
        || claude_continuation(raw)
        || raw.starts_with("user:")
        || raw.starts_with("(thinking)")
        || bracket(raw).is_some_and(|(tag, _)| tag_kind(tag).is_some() || tool_like(tag))
}

/// 一行开启的区块（可能不止一个），返回之后接收的续行类型。
fn start_line(blocks: &mut Vec<Block>, raw: &str, line: u64, next: Option<&str>) -> Open {
    if let Some((role, rest)) = claude_role(raw) {
        return start_claude(blocks, role, rest, line);
    }
    if claude_continuation(raw) {
        return start_claude(blocks, Role::Assistant, raw, line);
    }
    if let Some(rest) = raw.strip_prefix("user:") {
        push(
            blocks,
            line,
            Kind::Quote {
                title: Title::User,
                body: vec![rest.trim_start().to_owned()],
            },
        );
        return Open::Prose;
    }
    if let Some(rest) = raw.strip_prefix("(thinking)") {
        push_thinking(blocks, line, rest.trim());
        return Open::None;
    }
    if raw.trim().is_empty() {
        return Open::None;
    }
    if let Some(text) = raw.strip_prefix(INDENT) {
        push(
            blocks,
            line,
            Kind::Output {
                output: Vec::new(),
                hidden: 0,
            },
        );
        if let Some(last) = blocks.last_mut() {
            push_output_line(&mut last.kind, text);
        }
        return Open::Output;
    }
    if let Some((tag, rest)) = bracket(raw) {
        if let Some(kind) = tag_kind(tag) {
            let (kind, open) = match kind {
                TagKind::Message(title) => (
                    Kind::Quote {
                        title,
                        body: if rest.is_empty() {
                            Vec::new()
                        } else {
                            vec![rest.to_owned()]
                        },
                    },
                    Open::None,
                ),
                TagKind::Plan => (
                    Kind::Plan {
                        items: Vec::new(),
                        hidden: 0,
                    },
                    Open::Output,
                ),
                TagKind::Outcome(outcome) => (
                    Kind::Outcome {
                        outcome,
                        message: rest.to_owned(),
                    },
                    Open::None,
                ),
                TagKind::Compacted => (Kind::Compacted, Open::None),
                TagKind::Meta => (
                    Kind::Meta {
                        text: raw.to_owned(),
                    },
                    Open::None,
                ),
            };
            push(blocks, line, kind);
            return open;
        }
        let looks_called = !rest.is_empty()
            || next.is_some_and(|next| next.starts_with(INDENT))
            || known_tool(tag);
        if tool_like(tag) && looks_called {
            push(
                blocks,
                line,
                Kind::Tool {
                    name: tag.to_owned(),
                    summary: rest.to_owned(),
                    output: Vec::new(),
                    hidden: 0,
                },
            );
            return Open::Output;
        }
        push(
            blocks,
            line,
            Kind::Meta {
                text: raw.to_owned(),
            },
        );
        return Open::None;
    }
    push(
        blocks,
        line,
        Kind::Assistant {
            body: vec![raw.to_owned()],
        },
    );
    Open::Prose
}

/// Claude 的一行：`[user]` / `[assistant]` 之后（或输出块之后另起的一行里）的行内片段。
fn start_claude(blocks: &mut Vec<Block>, role: Role, rest: &str, line: u64) -> Open {
    let mut open = Open::None;
    for part in claude_parts(rest) {
        open = Open::None;
        match part {
            ClaudePart::Text(text) => match role {
                Role::User => push(
                    blocks,
                    line,
                    Kind::Quote {
                        title: Title::User,
                        body: vec![text.to_owned()],
                    },
                ),
                Role::Assistant => {
                    // 相邻两条助手消息合成一段，只画一次小标题。
                    if let Some(Block {
                        kind: Kind::Assistant { body },
                        end,
                        ..
                    }) = blocks
                        .last_mut()
                        .filter(|last| last.end == line || last.line == line)
                    {
                        body.push(text.to_owned());
                        *end = line + 1;
                    } else {
                        push(
                            blocks,
                            line,
                            Kind::Assistant {
                                body: vec![text.to_owned()],
                            },
                        );
                    }
                }
            },
            ClaudePart::Thinking => push_thinking(blocks, line, ""),
            ClaudePart::Tool { name, summary } => {
                push(
                    blocks,
                    line,
                    Kind::Tool {
                        name: name.to_owned(),
                        summary: summary.to_owned(),
                        output: Vec::new(),
                        hidden: 0,
                    },
                );
                open = Open::Output;
            }
            ClaudePart::Result { failed } => {
                push(
                    blocks,
                    line,
                    Kind::ToolResult {
                        failed,
                        output: Vec::new(),
                        hidden: 0,
                    },
                );
                open = Open::Output;
            }
            ClaudePart::Meta(text) => push(
                blocks,
                line,
                Kind::Meta {
                    text: text.to_owned(),
                },
            ),
        }
    }
    open
}

/// 追加一个区块；同一行已有区块时编号递增。
fn push(blocks: &mut Vec<Block>, line: u64, kind: Kind) {
    let sub = blocks
        .last()
        .filter(|last| last.line == line)
        .map_or(0, |last| last.sub.saturating_add(1));
    blocks.push(Block {
        line,
        sub,
        end: line + 1,
        kind,
    });
}

/// 思考行并进紧挨着的上一段思考。
fn push_thinking(blocks: &mut Vec<Block>, line: u64, note: &str) {
    if let Some(Block {
        kind: Kind::Thinking { count, notes },
        end,
        ..
    }) = blocks
        .last_mut()
        .filter(|last| last.end == line || last.line == line)
    {
        *count += 1;
        if !note.is_empty() {
            notes.push(note.to_owned());
        }
        *end = line + 1;
        return;
    }
    push(
        blocks,
        line,
        Kind::Thinking {
            count: 1,
            notes: if note.is_empty() {
                Vec::new()
            } else {
                vec![note.to_owned()]
            },
        },
    );
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    User,
    Assistant,
}

fn claude_role(raw: &str) -> Option<(Role, &str)> {
    [("[user]", Role::User), ("[assistant]", Role::Assistant)]
        .into_iter()
        .find_map(|(tag, role)| {
            let rest = raw.strip_prefix(tag)?;
            (rest.is_empty() || rest.starts_with(' ')).then(|| (role, rest.trim_start()))
        })
}

/// Claude 记录在输出块之后另起一行的行内片段：没有角色前缀，以 `[tool …]` 或
/// `[thinking]` 开头。Codex / OpenCode 的工具名不含空格，不会撞上 `[tool …]`。
fn claude_continuation(raw: &str) -> bool {
    (raw.starts_with("[tool ") && raw.contains(']'))
        || raw
            .strip_prefix("[thinking]")
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(' '))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClaudePart<'a> {
    Text(&'a str),
    Thinking,
    Tool { name: &'a str, summary: &'a str },
    Result { failed: bool },
    Meta(&'a str),
}

/// 把 Claude 一行的行内片段拆开：片段之间以空格相接，标签以 `[` 开头。
fn claude_parts(rest: &str) -> Vec<ClaudePart<'_>> {
    // 标签起点：行首或空格之后的 `[thinking]`、`[tool …]`、`[image]`。
    let starts: Vec<usize> = rest
        .match_indices('[')
        .map(|(offset, _)| offset)
        .filter(|&offset| offset == 0 || rest[..offset].ends_with(' '))
        .filter(|&offset| {
            let tail = &rest[offset..];
            tail.starts_with("[thinking]")
                || tail.starts_with("[image]")
                || (tail.starts_with("[tool ") && tail.contains(']'))
        })
        .collect();
    let mut parts = Vec::new();
    push_text(
        &mut parts,
        starts.first().map_or(rest, |&first| &rest[..first]),
    );
    for (position, &start) in starts.iter().enumerate() {
        let stop = starts.get(position + 1).copied().unwrap_or(rest.len());
        let piece = rest[start..stop].trim_end();
        if let Some(after) = piece.strip_prefix("[thinking]") {
            parts.push(ClaudePart::Thinking);
            push_text(&mut parts, after);
        } else if let Some(after) = piece.strip_prefix("[image]") {
            parts.push(ClaudePart::Meta("[image]"));
            push_text(&mut parts, after);
        } else if let Some((name, summary)) = piece
            .strip_prefix("[tool ")
            .and_then(|body| body.split_once(']'))
        {
            parts.push(match name.trim() {
                "result" => ClaudePart::Result { failed: false },
                "error" => ClaudePart::Result { failed: true },
                name => ClaudePart::Tool {
                    name,
                    summary: summary.trim(),
                },
            });
        }
    }
    parts
}

fn push_text<'a>(parts: &mut Vec<ClaudePart<'a>>, text: &'a str) {
    let text = text.trim();
    if !text.is_empty() {
        parts.push(ClaudePart::Text(text));
    }
}

/// `[标签] 余下`：标签不含 `]`、不超过 [`MAX_TAG_CHARS`]，后面是行尾或空格。
fn bracket(raw: &str) -> Option<(&str, &str)> {
    let (tag, rest) = raw.strip_prefix('[')?.split_once(']')?;
    if tag.is_empty() || tag.chars().count() > MAX_TAG_CHARS {
        return None;
    }
    (rest.is_empty() || rest.starts_with(' ')).then(|| (tag, rest.trim()))
}

enum TagKind {
    Message(Title),
    Plan,
    Outcome(Outcome),
    Compacted,
    Meta,
}

/// 适配器写出的固定标签。
fn tag_kind(tag: &str) -> Option<TagKind> {
    Some(match tag {
        "plan" => TagKind::Plan,
        "task complete" => TagKind::Outcome(Outcome::Done),
        "task failed" => TagKind::Outcome(Outcome::Failed),
        "turn aborted" => TagKind::Outcome(Outcome::Aborted),
        "error" => TagKind::Outcome(Outcome::Error),
        "compacted" | "context compacted" => TagKind::Compacted,
        "message" => TagKind::Message(Title::Message),
        "file" | "image" | "oversized record" | "unparsable line" => TagKind::Meta,
        _ => {
            let from = tag.strip_prefix("message from ")?.trim();
            if from.is_empty() {
                return None;
            }
            TagKind::Message(Title::From(from.to_owned()))
        }
    })
}

/// 工具名的样子：字母或下划线开头，只含字母数字与 `_ . : / -`（含命名空间形式）。
fn tool_like(tag: &str) -> bool {
    let mut chars = tag.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | ':' | '/' | '-'))
}

/// 没有摘要也没有输出时，仍能认作工具调用的名字：分类认得的、命名空间形式的，以及
/// 只等待或管理其他 agent 的控制类工具（它们常常没有参数摘要）。
fn known_tool(tag: &str) -> bool {
    use crate::harness::tool_display::{tool_kind, ToolKind};
    tool_kind(tag) != ToolKind::Other
        || tag.contains("__")
        || tag.contains('.')
        || matches!(
            tag,
            "wait"
                | "wait_agent"
                | "list_agents"
                | "close_agent"
                | "resume_agent"
                | "interrupt_agent"
        )
}

fn toggle_glyph(expanded: bool) -> &'static str {
    if expanded {
        "▾ "
    } else {
        "▸ "
    }
}

/// 工具输出与计划条目共用的输出行。
fn push_output(out: &mut Vec<TranscriptRow>, output: &[String], hidden: u64, width: usize) {
    const RULE: &[(&str, Tone)] = &[("  │ ", Tone::Rule)];
    for line in trim_blank_tail(output) {
        let line = expand_tabs(line);
        push_wrapped(out, RULE, RULE, &[(line.as_str(), Tone::Output)], width);
    }
    if hidden > 0 {
        let label = more_lines(hidden);
        push_wrapped(out, RULE, RULE, &[(label.as_str(), Tone::Meta)], width);
    }
}

fn more_lines(hidden: u64) -> String {
    let count = hidden.to_string();
    format!(
        "… {}",
        crate::i18n::fill(
            tr("{n} more lines not shown", "另有 {n} 行未显示"),
            &[("n", &count)]
        )
    )
}

/// 正文末尾的 `(encrypted)` 用元信息色。
fn encrypted_spans(line: &str, tone: Tone) -> Vec<(&str, Tone)> {
    const MARK: &str = "(encrypted)";
    match line.strip_suffix(MARK) {
        Some(head) if head.is_empty() || head.ends_with(' ') => {
            let mut spans = Vec::with_capacity(2);
            if !head.is_empty() {
                spans.push((head, tone));
            }
            spans.push((MARK, Tone::Meta));
            spans
        }
        _ => vec![(line, tone)],
    }
}

fn trim_blank_tail(lines: &[String]) -> &[String] {
    let end = lines
        .iter()
        .rposition(|line| !line.trim().is_empty())
        .map_or(0, |last| last + 1);
    &lines[..end]
}

fn expand_tabs(text: &str) -> String {
    if text.contains('\t') {
        text.replace('\t', "    ")
    } else {
        text.to_owned()
    }
}

fn char_width(ch: char) -> usize {
    UnicodeWidthChar::width(ch).unwrap_or(0)
}

fn spans_width<T: AsRef<str>>(spans: &[(T, Tone)]) -> usize {
    spans
        .iter()
        .map(|(text, _)| crate::ui::display_width(text.as_ref()))
        .sum()
}

/// 不折行的一行：超宽时在末尾截断并补 `…`（按显示宽度，不拆宽字符）。
fn fit_row(spans: Vec<(String, Tone)>, width: usize, toggle: Option<u64>) -> TranscriptRow {
    fit_row_tail(spans, Vec::new(), width, toggle)
}

/// 同 [`fit_row`]，但尽量保住末尾的 `tail`（如「+N 行」）：先截断前面的内容；宽度连
/// `tail` 与 [`MIN_HEAD_COLUMNS`] 列开头都放不下时才一起截断。
fn fit_row_tail(
    head: Vec<(String, Tone)>,
    tail: Vec<(String, Tone)>,
    width: usize,
    toggle: Option<u64>,
) -> TranscriptRow {
    let head_width = spans_width(&head);
    let tail_width = spans_width(&tail);
    let segments = if head_width + tail_width <= width {
        to_segments(head.into_iter().chain(tail))
    } else if !tail.is_empty() && width >= tail_width + MIN_HEAD_COLUMNS {
        let mut segments = truncate_spans(head, width - tail_width);
        segments.extend(to_segments(tail));
        segments
    } else {
        truncate_spans(head.into_iter().chain(tail).collect(), width)
    };
    TranscriptRow { segments, toggle }
}

fn to_segments(spans: impl IntoIterator<Item = (String, Tone)>) -> Vec<Segment> {
    spans
        .into_iter()
        .filter(|(text, _)| !text.is_empty())
        .map(|(text, tone)| Segment { text, tone })
        .collect()
}

/// 截到 `width` 列并在末尾补 `…`；调用方保证原文超宽。
fn truncate_spans(spans: Vec<(String, Tone)>, width: usize) -> Vec<Segment> {
    let budget = width.saturating_sub(1);
    let mut used = 0usize;
    let mut segments = Vec::with_capacity(spans.len() + 1);
    let mut cut_tone = spans.first().map_or(Tone::Body, |(_, tone)| *tone);
    'spans: for (text, tone) in spans {
        cut_tone = tone;
        let mut kept = String::new();
        for ch in text.chars() {
            let cell = char_width(ch);
            if used + cell > budget {
                if !kept.is_empty() {
                    segments.push(Segment { text: kept, tone });
                }
                break 'spans;
            }
            used += cell;
            kept.push(ch);
        }
        if !kept.is_empty() {
            segments.push(Segment { text: kept, tone });
        }
    }
    if width > 0 {
        segments.push(Segment {
            text: "…".to_owned(),
            tone: cut_tone,
        });
    }
    segments
}

/// 按显示宽度折行：首行接 `first_prefix`，续行接 `rest_prefix`；宽度放不下前缀时省略
/// 前缀。空文字也占一行（只有前缀，去掉末尾空格）。
fn push_wrapped(
    out: &mut Vec<TranscriptRow>,
    first_prefix: &[(&str, Tone)],
    rest_prefix: &[(&str, Tone)],
    spans: &[(&str, Tone)],
    width: usize,
) {
    let first_width = spans_width(first_prefix);
    let rest_width = spans_width(rest_prefix);
    let (first_prefix, rest_prefix, first_room, rest_room) =
        if width > first_width.max(rest_width) + 1 {
            (
                first_prefix,
                rest_prefix,
                width - first_width,
                width - rest_width,
            )
        } else {
            (&[][..], &[][..], width, width)
        };
    let mut text = String::new();
    let mut bounds: Vec<(usize, Tone)> = Vec::with_capacity(spans.len());
    for (piece, tone) in spans {
        text.push_str(piece);
        bounds.push((text.len(), *tone));
    }
    if text.is_empty() {
        let mut segments = prefix_segments(first_prefix);
        if let Some(last) = segments.last_mut() {
            let trimmed = last.text.trim_end().to_owned();
            last.text = trimmed;
        }
        segments.retain(|segment| !segment.text.is_empty());
        out.push(TranscriptRow {
            segments,
            toggle: None,
        });
        return;
    }
    let mut first = true;
    wrap_text(&text, first_room.max(1), rest_room.max(1), |start, end| {
        let mut segments = prefix_segments(if first { first_prefix } else { rest_prefix });
        first = false;
        let mut position = start;
        for &(bound, tone) in &bounds {
            if bound <= position {
                continue;
            }
            let stop = bound.min(end);
            if stop > position {
                segments.push(Segment {
                    text: text[position..stop].to_owned(),
                    tone,
                });
                position = stop;
            }
            if position >= end {
                break;
            }
        }
        out.push(TranscriptRow {
            segments,
            toggle: None,
        });
    });
}

fn prefix_segments(prefix: &[(&str, Tone)]) -> Vec<Segment> {
    prefix
        .iter()
        .filter(|(text, _)| !text.is_empty())
        .map(|(text, tone)| Segment {
            text: (*text).to_owned(),
            tone: *tone,
        })
        .collect()
}

#[cfg(test)]
mod tests;
