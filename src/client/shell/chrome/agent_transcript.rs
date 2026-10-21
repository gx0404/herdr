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
//! 公开接口是子智能体页面与本模块之间的约定，改动要同时照顾页面一侧。

// 骨架阶段页面还没接入，接口暂时无人调用；页面落地后删掉这条 allow。
#![allow(dead_code)]

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
    /// 说明（`design::muted`）：工具摘要、引用正文。
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

/// 折好的一行：从左到右的若干段，总显示宽度不超过 [`TranscriptView::layout`] 的宽度。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(in crate::client::shell) struct TranscriptRow {
    pub(in crate::client::shell) segments: Vec<Segment>,
    /// 工具调用的标题行：点击切换该工具输出的展开状态。值是工具块首行的绝对行号，与
    /// [`TranscriptView::toggle`] 的参数同口径。
    pub(in crate::client::shell) toggle: Option<u64>,
}

/// 概要面板用的统计。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::client::shell) struct TranscriptStats {
    /// 已加载内容里的工具调用数（头部被丢弃的不计）。
    pub(in crate::client::shell) tool_calls: usize,
}

/// 一个节点的转写视图：原文行 → 区块 → 显示行。
///
/// 骨架实现：每行原文都当正文、按宽度折行；区块解析、展开状态与统计由转写解析子任务
/// 补齐，公开接口保持不变。
#[derive(Debug, Default)]
pub(in crate::client::shell) struct TranscriptView {
    lines: Vec<String>,
    first_line: u64,
    mode: TranscriptMode,
    rows: Vec<TranscriptRow>,
    /// 最近一次布局的 (宽度, 小标题)；`None` = 需要重新布局。
    laid_out: Option<(u16, String)>,
}

impl TranscriptView {
    /// 原文变化后调用：`lines` 是当前保留的全部原文行（不含换行符），`first_line` 是
    /// `lines[0]` 在整个内容里的绝对行号（客户端从头部丢弃过多少行）。只重新解析受影响
    /// 的尾部；头部丢弃时一并丢弃对应区块与展开记录；`mode` 变化时整体重解析。
    pub(in crate::client::shell) fn sync<L: AsRef<str>>(
        &mut self,
        lines: &[L],
        first_line: u64,
        mode: TranscriptMode,
    ) {
        let same = self.mode == mode
            && self.first_line == first_line
            && self.lines.len() == lines.len()
            && self
                .lines
                .iter()
                .zip(lines)
                .all(|(old, new)| old == new.as_ref());
        if same {
            return;
        }
        self.mode = mode;
        self.first_line = first_line;
        self.lines = lines.iter().map(|line| line.as_ref().to_owned()).collect();
        self.laid_out = None;
    }

    /// 按 `width` 列折行。宽度、`speaker`、内容与展开状态都没变时直接返回。`speaker` 是
    /// 助手正文的小标题（节点昵称）；空串时不画小标题。
    pub(in crate::client::shell) fn layout(&mut self, width: u16, speaker: &str) {
        if self
            .laid_out
            .as_ref()
            .is_some_and(|(laid_width, laid_speaker)| {
                *laid_width == width && laid_speaker == speaker
            })
        {
            return;
        }
        let room = usize::from(width.max(1));
        let mut rows = Vec::new();
        for line in &self.lines {
            wrap_text(line, room, room, |start, end| {
                rows.push(TranscriptRow {
                    segments: vec![Segment {
                        text: line[start..end].to_owned(),
                        tone: Tone::Body,
                    }],
                    toggle: None,
                });
            });
        }
        self.rows = rows;
        self.laid_out = Some((width, speaker.to_owned()));
    }

    /// 最近一次 [`Self::layout`] 的显示行。
    pub(in crate::client::shell) fn rows(&self) -> &[TranscriptRow] {
        &self.rows
    }

    /// 切换一个工具块输出的展开状态（参数取自 [`TranscriptRow::toggle`]）。返回是否有
    /// 变化；有变化时下一次 [`Self::layout`] 重新折行。
    pub(in crate::client::shell) fn toggle(&mut self, _tool: u64) -> bool {
        false
    }

    /// Enter：有收起的工具输出就全部展开，否则全部收起。返回是否有变化。
    pub(in crate::client::shell) fn toggle_all(&mut self) -> bool {
        false
    }

    pub(in crate::client::shell) fn stats(&self) -> TranscriptStats {
        TranscriptStats::default()
    }

    /// 最近一行非空的工具输出或助手正文（概要「最近输出」），已折成单行。
    pub(in crate::client::shell) fn last_output(&self) -> Option<&str> {
        self.lines
            .iter()
            .rev()
            .map(|line| line.trim())
            .find(|line| !line.is_empty())
    }
}
