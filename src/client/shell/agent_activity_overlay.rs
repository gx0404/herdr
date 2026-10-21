//! 子智能体页面（全屏）的状态、读取与输入路由：属主的活动树、所选节点的内容与三种
//! 视图（层级、状态、时间线）。版式与绘制在 `chrome::agent_page`，这里不画任何东西。
//! 数据来自 `agent.activity.read`；快照里的活动摘要只用来提示「有更新」。
//!
//! 数据通道（单飞）：页面同时至多一个在途读取。打开时先读树（`node_id` 省略），
//! 选中节点后读内容（`node_id` 给出、从头读）；内容没读完（`eof = false`）就按
//! `next_cursor` 立即续读。跟随模式下树应答落地后每隔 [`FOLLOW_INTERVAL`] 重发
//! 一次树读取（`follow = true`，让 server 在 `FOLLOW_TTL` 内保持跟随），树回来
//! 后按上次的 `next_cursor` 续读内容；`eof` 且没有游标（pi）时下次从头重读并
//! 整体替换。树读取已排队或在途时不再追加，树往返慢于间隔也饿不死内容续读。
//! 每个请求的 `epoch` 取全局递增的请求序号，响应对不上在途请求即丢弃；内容读取
//! 另带内容代际，选中节点或刷新之后到达的旧内容也丢弃。只有成功应答立即续发
//! 下一个读取；错误结局交给下一次 tick，被取消的读取（断线等）放回队列、不算
//! 读取失败。页面关闭后不再发起任何读取。外部来源属主忽略跟随（server 也
//! 忽略）。旧 server 未宣告或拒绝该方法时显示「不提供」文案并不再重试。
//!
//! 阶段划分（STATE-04）：输入阶段改选中 / 折叠 / 视图 / 筛选 / 滚动意图并排队读取；
//! 视图计算阶段（[`ClientShellState::compute_agent_activity_view`]）按页面几何建列表
//! 显示行、同步并折行转写、夹紧滚动；渲染阶段（`chrome::agent_page::paint_agent_page`）
//! 只读。页面不在 pane 规模的循环里，每帧只画可见的列表行与转写行。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use super::chrome::agent_page::{self, ListLine};
use super::chrome::agent_transcript::wrap_cjk;
use super::chrome::agent_transcript::{TranscriptMode, TranscriptView};
use super::*;
use crate::api::schema::{
    AgentActivityContent, AgentActivityContentFormat, AgentActivityKind, AgentActivityNode,
    AgentActivityReadParams, AgentActivityStatus, Method, ResponseResult,
};
use crate::harness::AgentBucket;
use crate::ui::kit::tree::{fill_last_child_masks, TreeEntry, MAX_TREE_DEPTH};
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

/// 跟随时重发树读取（并续读内容）的间隔，从上一次树应答落地时起算；与 server
/// 的 `FOLLOW_INTERVAL` 对齐，远小于 server 的 `FOLLOW_TTL`（10 s），跟随态
/// 因此持续有效。
pub(super) const FOLLOW_INTERVAL: Duration = Duration::from_secs(1);
/// 一次内容读取的字节上限（server 会再压到它自己的上限）。
const READ_MAX_BYTES: u32 = 256 * 1024;
/// 客户端保留的内容上限（显示行文本总字节）：超出时从头部整行丢弃并提示
/// 「仅显示最新部分」。
pub(super) const CONTENT_CAP_BYTES: usize = 512 * 1024;
/// 客户端接收的节点上限（适配器自身封顶 256，这里只防异常 server）。
const MAX_NODES: usize = 1024;
/// 筛选输入的字符上限（只防粘贴巨量文本）。
const FILTER_MAX_CHARS: usize = 128;

/// 活动树的属主：某个 pane 里的 agent，或不属于任何 pane 的外部来源条目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum AgentActivityOwner {
    Pane { pane_id: String },
    External { external_id: String },
}

/// 页面的三种视图。本次客户端会话内记在 `ClientShellState::agent_page_view`，重开
/// 页面沿用，不写入设置。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum AgentPageView {
    /// 层级：按父子关系的树。
    #[default]
    Tree,
    /// 状态：按状态桶分组的平铺列表。
    Status,
    /// 时间线：泳道 + 下方的转写与概要。
    Timeline,
}

impl AgentPageView {
    pub(super) const ALL: [Self; 3] = [Self::Tree, Self::Status, Self::Timeline];

    pub(super) fn label(self) -> &'static str {
        let texts = &crate::i18n::texts().agent_activity;
        match self {
            Self::Tree => texts.view_tree,
            Self::Status => texts.view_status,
            Self::Timeline => texts.view_timeline,
        }
    }
}

/// 页面上可点的控件：页头按钮、视图标签、筛选入口与「已完成 N 项」行。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AgentActivityButton {
    Close,
    ToggleFollow,
    Refresh,
    Back,
    Jump,
    View(AgentPageView),
    Filter,
    ToggleDone,
}

/// 键盘焦点所在的栏：列表（时间线视图里是泳道）或转写。窄屏只显示焦点所在的一栏。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum AgentActivityFocus {
    #[default]
    Tree,
    Content,
}

/// 在途读取（页面同时只有一个）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AgentActivityRead {
    pub(super) epoch: u64,
    /// `None` = 树读取；`Some` = 该节点的内容读取。
    pub(super) node_id: Option<String>,
    pub(super) cursor: Option<String>,
    /// 应答整体替换内容（从头读），而不是追加在后面。
    pub(super) replace: bool,
    /// 发出时的内容代际；对不上的内容应答被丢弃。
    generation: u64,
}

/// 内容显示行的色调：markdown 只给标题与列表着色，jsonl 给记录前缀着色。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LineTone {
    Plain,
    Heading,
    ListItem,
    Record,
}

/// 一条显示行（已按格式整理，折行在视图计算阶段做）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ContentLine {
    pub(super) text: String,
    pub(super) tone: LineTone,
    /// 需要着色的前缀字节数（列表记号 / jsonl 前缀）；标题整行着色。
    pub(super) marker: usize,
}

impl AsRef<str> for ContentLine {
    fn as_ref(&self) -> &str {
        &self.text
    }
}

/// 折行后的一个显示行：`lines[line]` 的 `[start, end)` 字节区间。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct WrapRow {
    pub(super) line: u32,
    pub(super) start: u32,
    pub(super) end: u32,
}

/// 已选节点的内容：按页追加，显示行与折行结果增量维护。Text 格式的折行交给
/// [`TranscriptView`]，这里只维护 Markdown / Jsonl 的按行折行。
#[derive(Debug, Default)]
pub(super) struct AgentActivityContentView {
    pub(super) node_id: String,
    pub(super) format: AgentActivityContentFormat,
    pub(super) lines: Vec<ContentLine>,
    /// 末尾还没以换行结束的原文；它的显示行是 `lines` 的最后一项，续读时先撤掉
    /// 再与新的一页拼起来重新整理。
    partial: Option<String>,
    /// `lines` 的文本总字节，超过 [`CONTENT_CAP_BYTES`] 时从头部丢行。
    bytes: usize,
    /// 客户端从头部累计丢弃的行数：`lines[0]` 在整个内容里的行号（转写的
    /// `first_line`）。从头重读时归零。
    pub(super) dropped: u64,
    /// 内容修订号：每次追加、丢行或清空都加一，视图计算阶段据此决定是否重新同步转写。
    /// 只在这份内容实例里单调；跨实例要与 [`Self::generation`] 一起比较。
    pub(super) revision: u64,
    /// 建出这份内容实例的读取代际（换选中、游标失效重读都会换新实例与新代际）。
    /// 同一节点前后两份内容的修订号可能相同，缓存键靠它区分。
    pub(super) generation: u64,
    /// server 标了截断，或客户端因上限丢过头部：顶部提示「仅显示最新部分」。
    pub(super) truncated: bool,
    pub(super) eof: bool,
    pub(super) next_cursor: Option<String>,
    pub(super) rows: Vec<WrapRow>,
    wrap_width: u16,
    /// `lines[..wrapped]` 已折进 `rows`。
    wrapped: usize,
}

impl AgentActivityContentView {
    fn new(node_id: String) -> Self {
        Self {
            node_id,
            ..Self::default()
        }
    }

    /// 全部原文（显示行按换行拼回），测试与调试用。
    #[cfg(test)]
    pub(super) fn text(&self) -> String {
        let mut text = self
            .lines
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        if self.partial.is_none() && !self.lines.is_empty() {
            text.push('\n');
        }
        text
    }

    /// 是否按转写排版（Text / 未知格式）；Markdown / Jsonl 走按行着色。
    pub(super) fn is_transcript(&self) -> bool {
        matches!(
            self.format,
            AgentActivityContentFormat::Text | AgentActivityContentFormat::Unknown
        )
    }

    /// 折行后的显示行数（视图计算阶段之后有效；只对 Markdown / Jsonl）。
    pub(super) fn row_count(&self) -> usize {
        self.rows.len()
    }

    fn clear(&mut self) {
        self.lines.clear();
        self.partial = None;
        self.bytes = 0;
        self.dropped = 0;
        self.truncated = false;
        self.revision = self.revision.wrapping_add(1);
        self.invalidate_from(0);
    }

    /// 追加一页原文：完整的行整理成显示行，末尾半行暂存并先显示出来。
    fn append(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.revision = self.revision.wrapping_add(1);
        let mut pending = match self.partial.take() {
            Some(partial) => {
                // 撤掉半行的临时显示行，与新的一页拼起来重新整理。
                if let Some(line) = self.lines.pop() {
                    self.bytes = self.bytes.saturating_sub(line.text.len());
                }
                self.invalidate_from(self.lines.len());
                partial
            }
            None => String::new(),
        };
        pending.push_str(text);
        let mut rest = pending.as_str();
        while let Some(end) = rest.find('\n') {
            self.push_line(&rest[..end]);
            rest = &rest[end + 1..];
        }
        if !rest.is_empty() {
            self.push_line(rest);
            self.partial = Some(rest.to_owned());
        }
        self.enforce_cap();
    }

    fn push_line(&mut self, raw: &str) {
        let line = display_line(self.format, raw);
        self.bytes = self.bytes.saturating_add(line.text.len());
        self.lines.push(line);
    }

    /// 超出上限时从头部整行丢弃并记下累计丢弃行数；折行结果随之平移，不整体重算。
    fn enforce_cap(&mut self) {
        if self.bytes <= CONTENT_CAP_BYTES {
            return;
        }
        let mut drop = 0;
        while self.bytes > CONTENT_CAP_BYTES && drop + 1 < self.lines.len() {
            self.bytes = self.bytes.saturating_sub(self.lines[drop].text.len());
            drop += 1;
        }
        if drop == 0 {
            return;
        }
        self.lines.drain(..drop);
        self.truncated = true;
        self.dropped = self
            .dropped
            .saturating_add(u64::try_from(drop).unwrap_or(u64::MAX));
        let drop32 = u32::try_from(drop).unwrap_or(u32::MAX);
        self.rows.retain(|row| row.line >= drop32);
        for row in &mut self.rows {
            row.line -= drop32;
        }
        self.wrapped = self.wrapped.saturating_sub(drop);
    }

    /// 从第 `line` 行起的折行结果作废（行被替换或撤掉）。
    fn invalidate_from(&mut self, line: usize) {
        if line >= self.wrapped {
            return;
        }
        let line32 = u32::try_from(line).unwrap_or(u32::MAX);
        let keep = self.rows.partition_point(|row| row.line < line32);
        self.rows.truncate(keep);
        self.wrapped = line;
    }

    /// 按 `width` 列折行（按显示宽度、优先在空格处断，不拆宽字符）：宽度变了全部
    /// 重折，否则只折新增的行。
    fn ensure_wrapped(&mut self, width: u16) {
        if width == 0 {
            return;
        }
        if width != self.wrap_width {
            self.wrap_width = width;
            self.rows.clear();
            self.wrapped = 0;
        }
        let room = usize::from(width);
        let to32 = |value: usize| u32::try_from(value).unwrap_or(u32::MAX);
        for (index, line) in self.lines.iter().enumerate().skip(self.wrapped) {
            let line_index = to32(index);
            wrap_cjk(&line.text, room, room, |start, end| {
                self.rows.push(WrapRow {
                    line: line_index,
                    start: to32(start),
                    end: to32(end),
                });
            });
        }
        self.wrapped = self.lines.len();
    }
}

/// 去掉会弄乱单元格的控制字符：制表符换成空格，ESC 序列整段丢弃。
fn sanitize(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\t' => out.push_str("    "),
            '\u{1b}' => {
                // CSI（ESC [ … 终止字节 0x40–0x7e）整段丢；其余 ESC 连同下一个字符丢。
                if chars.peek() == Some(&'[') {
                    chars.next();
                    for next in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&next) {
                            break;
                        }
                    }
                } else {
                    chars.next();
                }
            }
            ch if ch.is_control() => {}
            ch => out.push(ch),
        }
    }
    out
}

/// 一行原文按格式整理成显示行：markdown 标出标题与列表记号；jsonl 取常见键做
/// 前缀，其余字段紧凑显示；不是 JSON 对象的行原样。
fn display_line(format: AgentActivityContentFormat, raw: &str) -> ContentLine {
    let text = sanitize(raw);
    match format {
        AgentActivityContentFormat::Markdown => markdown_line(text),
        AgentActivityContentFormat::Jsonl => jsonl_line(text),
        AgentActivityContentFormat::Text | AgentActivityContentFormat::Unknown => ContentLine {
            text,
            tone: LineTone::Plain,
            marker: 0,
        },
    }
}

fn markdown_line(text: String) -> ContentLine {
    let trimmed = text.trim_start();
    let indent = text.len() - trimmed.len();
    let hashes = trimmed.bytes().take_while(|byte| *byte == b'#').count();
    if (1..=6).contains(&hashes)
        && trimmed[hashes..]
            .chars()
            .next()
            .is_none_or(char::is_whitespace)
    {
        return ContentLine {
            text,
            tone: LineTone::Heading,
            marker: 0,
        };
    }
    let marker = if ["- ", "* ", "+ "]
        .iter()
        .any(|bullet| trimmed.starts_with(bullet))
    {
        Some(2)
    } else {
        let digits = trimmed.bytes().take_while(u8::is_ascii_digit).count();
        (digits > 0 && (trimmed[digits..].starts_with(". ") || trimmed[digits..].starts_with(") ")))
            .then_some(digits + 2)
    };
    match marker {
        Some(marker) => ContentLine {
            marker: indent + marker,
            text,
            tone: LineTone::ListItem,
        },
        None => ContentLine {
            text,
            tone: LineTone::Plain,
            marker: 0,
        },
    }
}

/// jsonl 记录前缀取这些键（字符串值）；取到的键不再出现在其余字段里。
const JSONL_PREFIX_KEYS: [&str; 3] = ["type", "role", "name"];

fn jsonl_line(text: String) -> ContentLine {
    let plain = |text: String| ContentLine {
        text,
        tone: LineTone::Plain,
        marker: 0,
    };
    let Ok(serde_json::Value::Object(mut map)) =
        serde_json::from_str::<serde_json::Value>(text.trim())
    else {
        return plain(text);
    };
    let mut prefix = String::new();
    for key in JSONL_PREFIX_KEYS {
        let Some(value) = map
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
        else {
            continue;
        };
        map.remove(key);
        if !prefix.is_empty() {
            prefix.push(' ');
        }
        prefix.push_str(&value);
    }
    let rest = if map.is_empty() {
        String::new()
    } else {
        serde_json::to_string(&serde_json::Value::Object(map)).unwrap_or_default()
    };
    if prefix.is_empty() {
        return plain(rest);
    }
    let prefix = sanitize(&prefix);
    let marker = prefix.len() + 2;
    let text = if rest.is_empty() {
        format!("[{prefix}]")
    } else {
        format!("[{prefix}] {}", sanitize(&rest))
    };
    ContentLine {
        text,
        tone: LineTone::Record,
        marker,
    }
}

/// 页头用的属主信息：视图计算阶段从快照取一次，渲染只读。
#[derive(Debug, Clone, Default)]
pub(super) struct AgentPageOwner {
    /// 显示名：pane 型取 agent 名，外部条目取标签；找不到时回退到 id。
    pub(super) name: String,
    /// 位置：`工作区 / 标签`（pane 型）或工作目录（外部条目）；没有就省略。
    pub(super) location: Option<String>,
    /// 属主自己的状态桶；不在快照里时为 `None`（不画状态字形）。
    pub(super) bucket: Option<AgentBucket>,
    /// 运行开始的墙钟毫秒（快照里有才有）。
    pub(super) since_ms: Option<u64>,
    pub(super) model: Option<String>,
    pub(super) session_title: Option<String>,
    /// agent 种类标签（`codex` / `claude` …），决定转写的解析方式。
    pub(super) agent: Option<String>,
}

/// 时间线「概要」面板的缓存（视图计算阶段写，渲染只读）。
#[derive(Debug, Default)]
pub(super) struct AgentPageSummary {
    /// 缓存对应的 (节点, 内容代际, 内容修订号, 解析方式, 折行宽度)。
    key: Option<(String, u64, u64, TranscriptMode, usize)>,
    /// 统计所属的节点；与所选节点不符时不显示。
    pub(super) node: String,
    pub(super) tool_calls: usize,
    /// 「最近输出」按面板宽度折好的行。
    pub(super) last_output: Vec<String>,
}

#[derive(Debug)]
pub(super) struct ClientAgentActivityOverlay {
    pub(super) endpoint_id: ClientEndpointId,
    pub(super) owner: AgentActivityOwner,
    /// 键盘选中的节点：只由键盘与点击改写。
    pub(super) selected_node: Option<String>,
    /// 指针悬浮的节点：只由 `Moved` 改写（MENU-01）。
    pub(super) hovered_node: Option<String>,
    /// 折叠的节点（默认全部展开）。
    pub(super) collapsed: HashSet<String>,
    /// 当前视图（层级 / 状态 / 时间线）。
    pub(super) view: AgentPageView,
    /// 筛选词（按标签、摘要、角色不分大小写匹配，保留匹配项的祖先）。
    pub(super) filter: String,
    /// 正在输入筛选词：可打印字符进筛选框，不当快捷键。
    pub(super) filter_editing: bool,
    /// 收起已完成：隐藏整棵子树都已完成的节点（失败节点永远可见）。
    pub(super) hide_done: bool,
    /// 列表滚动（显示行；时间线视图里是泳道行）。视图计算阶段夹紧。
    pub(super) list_scroll: usize,
    /// 键盘移动选中后要求把它滚进视野；视图计算阶段消费。
    list_reveal: bool,
    /// 视图计算阶段建的列表显示行（渲染只读）。
    pub(super) list_lines: Vec<ListLine>,
    pub(super) content_scroll: usize,
    /// 视图计算阶段写入，渲染只读（STATE-04）。
    pub(super) content_max_scroll: usize,
    /// 内容钉在底部：新内容到达后视图计算阶段把滚动跟到最新（跟随时的默认）。
    pub(super) content_tail: bool,
    /// 视图计算阶段记下的一页行数（PgUp / PgDn 步长）。
    list_page: usize,
    content_page: usize,
    /// 正在拖转写的滚动条。
    scroll_drag: bool,
    pub(super) follow: bool,
    pub(super) focus: AgentActivityFocus,
    /// 最近一次发出的读取的代际（全局请求序号）。
    pub(super) read_epoch: u64,
    pub(super) in_flight: Option<AgentActivityRead>,
    want_tree: bool,
    want_content: bool,
    /// 下一次内容读取从头读并整体替换（新选中 / 刷新 / 上次 `eof` 且无游标）。
    restart_content: bool,
    /// 内容目标（选中节点 / 刷新）的代际。
    content_generation: u64,
    /// 跟随模式下一次重发树读取的时刻（树应答落地时重置）；`None` = 立即。
    next_follow_at: Option<Instant>,
    pub(super) nodes: Vec<AgentActivityNode>,
    /// 当前视图的可选条目（`kind` = `nodes` 下标）：层级 / 时间线按父子前序展开、
    /// 跳过折叠子树；状态视图按状态桶排序、不分层。筛选与「收起已完成」已应用。
    pub(super) rows: Vec<TreeEntry<usize>>,
    pub(super) tree_loaded: bool,
    pub(super) tree_error: Option<String>,
    /// 最近一次读到的树的 (running, total)，与快照摘要比对得出「有更新」。
    tree_summary: (u32, u32),
    /// 上一帧看到的快照摘要；变化且与树不符时提示「有更新」。
    last_snapshot_summary: Option<(u32, u32)>,
    /// 读到这棵树之后，快照摘要是否与它对上过；只有外部条目看它。外部条目的快照
    /// 摘要来自来源的列表查询，列表让各条目分摊行数上限，被挤掉的条目在快照里只剩
    /// 一截残树，而读树是只针对该条目的单独查询（整树，见
    /// `server::agent_activity::Worker::read_external_tree`），两者对不上不说明有
    /// 变化。pane 属主的快照与读树同一口径，不看它。
    snapshot_matched_tree: bool,
    pub(super) has_updates: bool,
    /// 内容读取的错误。
    pub(super) error: Option<String>,
    pub(super) content: Option<AgentActivityContentView>,
    /// 所选节点 Text 内容的转写排版（视图计算阶段同步与折行，渲染只读）。
    pub(super) transcript: TranscriptView,
    /// 转写同步到的 (节点, 内容修订号, 解析方式)；对不上时视图计算阶段重新同步。
    transcript_synced: Option<(String, u64, u64, TranscriptMode)>,
    /// 时间线概要用的统计与「最近输出」折行：视图计算阶段按（节点、内容修订号、
    /// 解析方式、宽度）缓存，渲染只读。
    pub(super) summary: AgentPageSummary,
    /// server 未宣告或拒绝 `agent.activity.read`：显示「不提供」并不再重试。
    pub(super) unsupported: bool,
    /// 视图计算阶段记下的墙钟毫秒，渲染据此算运行中节点的耗时。
    pub(super) now_ms: u64,
    /// 视图计算阶段从快照取的属主信息。
    pub(super) owner_view: AgentPageOwner,
    /// 视图计算阶段取的状态字形（指示风格 + 全局转圈帧）。
    pub(super) glyphs: StatusGlyphs,
    /// 页面自己画页脚（经典 / 移动布局没有工作台状态栏）。视图计算阶段写入。
    pub(super) own_footer: bool,
    /// 从别的浮层（如右键菜单之外的入口）打开时，Esc 回到它。
    pub(super) return_to: Option<Box<ClientShellOverlay>>,
}

impl ClientAgentActivityOverlay {
    /// 最近读到的树里有运行中的节点：页面上它们画转圈，转圈时钟要继续走。
    pub(super) fn has_running(&self) -> bool {
        self.tree_summary.0 > 0
    }

    fn new(endpoint_id: ClientEndpointId, owner: AgentActivityOwner, view: AgentPageView) -> Self {
        let follow = matches!(owner, AgentActivityOwner::Pane { .. });
        Self {
            endpoint_id,
            owner,
            selected_node: None,
            hovered_node: None,
            collapsed: HashSet::new(),
            view,
            filter: String::new(),
            filter_editing: false,
            hide_done: false,
            list_scroll: 0,
            list_reveal: false,
            list_lines: Vec::new(),
            content_scroll: 0,
            content_max_scroll: 0,
            content_tail: follow,
            list_page: 1,
            content_page: 1,
            scroll_drag: false,
            follow,
            focus: AgentActivityFocus::Tree,
            read_epoch: 0,
            in_flight: None,
            want_tree: true,
            want_content: false,
            restart_content: true,
            content_generation: 0,
            next_follow_at: None,
            nodes: Vec::new(),
            rows: Vec::new(),
            tree_loaded: false,
            tree_error: None,
            tree_summary: (0, 0),
            last_snapshot_summary: None,
            snapshot_matched_tree: false,
            has_updates: false,
            error: None,
            content: None,
            transcript: TranscriptView::default(),
            transcript_synced: None,
            summary: AgentPageSummary::default(),
            unsupported: false,
            now_ms: 0,
            owner_view: AgentPageOwner::default(),
            glyphs: StatusGlyphs::fixed(crate::config::StatusIndicatorStyle::Graphite),
            own_footer: false,
            return_to: None,
        }
    }

    pub(super) fn is_external(&self) -> bool {
        matches!(self.owner, AgentActivityOwner::External { .. })
    }

    /// 跟随只对 pane 属主有效；外部来源忽略它（server 也忽略）。
    pub(super) fn follow_active(&self) -> bool {
        self.follow && !self.is_external()
    }

    /// 可见条目的节点 id（按显示顺序）。
    #[cfg(test)]
    pub(super) fn visible_node_ids(&self) -> impl Iterator<Item = &str> {
        self.rows.iter().map(|row| row.key.as_str())
    }

    pub(super) fn selected_row(&self) -> Option<usize> {
        let selected = self.selected_node.as_deref()?;
        self.rows.iter().position(|row| row.key == selected)
    }

    pub(super) fn node(&self, id: &str) -> Option<&AgentActivityNode> {
        self.nodes.iter().find(|node| node.id == id)
    }

    /// 已完成（`Done`）的节点数：列表末尾「已完成 N 项」。
    pub(super) fn done_count(&self) -> usize {
        self.nodes
            .iter()
            .filter(|node| node.status == AgentActivityStatus::Done)
            .count()
    }

    fn mark_unsupported(&mut self) {
        self.unsupported = true;
        self.want_tree = false;
        self.want_content = false;
        self.in_flight = None;
    }

    /// 改选中节点：内容换新代际、清空，从头读。已是它时只要求滚进视野。
    fn select(&mut self, node_id: String) -> bool {
        self.list_reveal = true;
        if self.selected_node.as_deref() == Some(node_id.as_str()) {
            return false;
        }
        self.selected_node = Some(node_id);
        self.restart_selected_content();
        self.content = None;
        self.transcript = TranscriptView::default();
        self.transcript_synced = None;
        self.error = None;
        self.content_scroll = 0;
        self.content_tail = self.follow_active();
        true
    }

    /// 选中节点的内容从头重读（旧内容在新应答到达前保留）。
    fn restart_selected_content(&mut self) {
        self.content_generation = self.content_generation.wrapping_add(1);
        self.restart_content = true;
        self.want_content = self.selected_node.is_some();
    }

    /// 按视图、折叠、筛选与「收起已完成」重建可选条目。
    fn rebuild_rows(&mut self) {
        let keep = visible_mask(&self.nodes, &self.filter, self.hide_done);
        self.rows = match self.view {
            AgentPageView::Tree | AgentPageView::Timeline => {
                build_rows_where(&self.nodes, &self.collapsed, Some(&keep))
            }
            AgentPageView::Status => status_rows(&self.nodes, &keep),
        };
    }

    fn toggle_collapsed(&mut self, node_id: &str) -> bool {
        if self.view == AgentPageView::Status {
            return false;
        }
        let has_children = self
            .rows
            .iter()
            .any(|row| row.key == node_id && row.has_children);
        if !has_children {
            return false;
        }
        if !self.collapsed.remove(node_id) {
            self.collapsed.insert(node_id.to_owned());
        }
        self.rebuild_rows();
        true
    }

    /// 下一个该发的读取：`(node_id, cursor, replace)`。树优先（打开、刷新与跟随
    /// 都先让左列就位），再读选中节点的内容。
    fn next_read(&self) -> Option<(Option<String>, Option<String>, bool)> {
        if self.unsupported || self.in_flight.is_some() {
            return None;
        }
        if self.want_tree {
            return Some((None, None, false));
        }
        if !self.want_content {
            return None;
        }
        let node = self.selected_node.as_ref()?;
        let cursor = self
            .content
            .as_ref()
            .filter(|content| !self.restart_content && &content.node_id == node)
            .and_then(|content| content.next_cursor.clone());
        let replace = cursor.is_none();
        Some((Some(node.clone()), cursor, replace))
    }

    /// 应用一个与在途读取配对的应答；返回是否需要重绘。
    fn apply_read(
        &mut self,
        read: AgentActivityRead,
        result: Result<ResponseResult, ClientShellEndpointError>,
    ) -> bool {
        let current = |overlay: &Self, node: &str| {
            read.generation == overlay.content_generation
                && overlay.selected_node.as_deref() == Some(node)
        };
        match result {
            Err(error) => {
                if method_unsupported(&error) {
                    self.mark_unsupported();
                    return true;
                }
                match read.node_id.as_deref() {
                    None => self.tree_error = Some(error.message),
                    Some(node)
                        if current(self, node)
                            && error.code.as_deref() == Some("activity_cursor_stale") =>
                    {
                        self.restart_selected_content();
                        self.content = None;
                        self.error = None;
                        self.content_scroll = 0;
                        self.content_tail = self.follow_active();
                    }
                    Some(node) if current(self, node) => self.error = Some(error.message),
                    Some(_) => return false,
                }
                true
            }
            Ok(ResponseResult::AgentActivity { nodes, content }) => match read.node_id.as_deref() {
                None => {
                    self.apply_tree(nodes);
                    true
                }
                Some(node) if current(self, node) => {
                    let chunk = content.unwrap_or_else(|| AgentActivityContent {
                        node_id: node.to_owned(),
                        eof: true,
                        ..AgentActivityContent::default()
                    });
                    self.apply_content(&read, node, chunk);
                    true
                }
                Some(_) => false,
            },
            Ok(_) => false,
        }
    }

    fn apply_tree(&mut self, mut nodes: Vec<AgentActivityNode>) {
        nodes.truncate(MAX_NODES);
        let running = nodes
            .iter()
            .filter(|node| node.status == AgentActivityStatus::Running)
            .count();
        self.tree_summary = (
            u32::try_from(running).unwrap_or(u32::MAX),
            u32::try_from(nodes.len()).unwrap_or(u32::MAX),
        );
        self.snapshot_matched_tree = self.last_snapshot_summary == Some(self.tree_summary);
        self.nodes = nodes;
        self.tree_loaded = true;
        self.tree_error = None;
        self.has_updates = false;
        self.rebuild_rows();
    }

    fn apply_content(&mut self, read: &AgentActivityRead, node: &str, chunk: AgentActivityContent) {
        if self
            .content
            .as_ref()
            .is_none_or(|content| content.node_id != node)
        {
            self.content = Some(AgentActivityContentView {
                generation: read.generation,
                ..AgentActivityContentView::new(node.to_owned())
            });
        }
        let Some(content) = self.content.as_mut() else {
            return;
        };
        if read.replace {
            content.clear();
        }
        content.format = chunk.format;
        content.append(&chunk.text);
        content.truncated |= chunk.truncated;
        content.eof = chunk.eof;
        content.next_cursor = chunk.next_cursor;
        self.restart_content = false;
        self.error = None;
        // 没读完且游标有推进：立即续读。没有推进的空页当作读完，免得原地打转。
        let advanced = !chunk.text.is_empty() || content.next_cursor != read.cursor;
        if !content.eof && content.next_cursor.is_some() && advanced {
            self.want_content = true;
        }
        // pi：读完后不给游标，下一次（跟随）从头重读并整体替换。
        if content.eof && content.next_cursor.is_none() {
            self.restart_content = true;
        }
    }

    /// 被取消的读取放回队列：树读取重新排队；内容读取仍对得上当前目标时重新排队。
    /// 续读的游标与追加语义由 `next_read` 按未变的内容状态重新算出，与被取消的
    /// 那次一致；对不上的（已换选中 / 已刷新）由新目标自己的排队接手。
    fn requeue(&mut self, read: &AgentActivityRead) {
        match read.node_id.as_deref() {
            None => self.want_tree = true,
            Some(node) => {
                if read.generation == self.content_generation
                    && self.selected_node.as_deref() == Some(node)
                {
                    self.want_content = true;
                }
            }
        }
    }

    /// 快照摘要变化且与已读到的树不符：未跟随时提示「有更新」。外部条目另要求快照
    /// 此前与这棵树对上过（`snapshot_matched_tree`）：被列表挤掉行数的条目，快照里
    /// 的残树与整树恒不相等，不这样限定就会随列表的每次变化误报。
    fn observe_summary(&mut self, summary: Option<(u32, u32)>) {
        if summary == self.last_snapshot_summary {
            return;
        }
        let first = self.last_snapshot_summary.is_none();
        self.last_snapshot_summary = summary;
        // 外部列表仅保留运行根。退出列表是一次性终态信号：通过历史 ID 补取
        // 最终树及所选内容，复用单飞队列，不把只读窗口变成周期轮询。
        if self.is_external() && !first && summary.is_none() {
            self.want_tree = true;
            self.want_content = self.selected_node.is_some();
            self.has_updates = true;
        }
        if !self.tree_loaded {
            return;
        }
        let matches_tree = summary == Some(self.tree_summary);
        let comparable = !self.is_external() || self.snapshot_matched_tree;
        self.snapshot_matched_tree |= matches_tree;
        if first || self.follow_active() {
            return;
        }
        if summary.is_some() && !matches_tree && comparable {
            self.has_updates = true;
        }
    }
}

/// 应答是否表示 server 根本不认识该方法（旧 server）。来源未实现
/// （`not_implemented`，如 pane 暂无会话引用）按普通读取失败处理，跟随时照常重试。
pub(in crate::client::shell) fn method_unsupported(error: &ClientShellEndpointError) -> bool {
    match error.code.as_deref() {
        Some("unsupported_method" | "method_not_found" | "unknown_method") => true,
        Some("invalid_request") => error.message.contains("unknown variant"),
        _ => false,
    }
}

/// 每个节点的父节点下标：找不到父节点的是根；成环的链在首个节点处断开。
fn parent_indices(nodes: &[AgentActivityNode]) -> Vec<Option<usize>> {
    let index: HashMap<&str, usize> = nodes
        .iter()
        .enumerate()
        .map(|(position, node)| (node.id.as_str(), position))
        .collect();
    let mut parent_of: Vec<Option<usize>> = nodes
        .iter()
        .enumerate()
        .map(|(position, node)| {
            node.parent_id
                .as_deref()
                .and_then(|parent| index.get(parent).copied())
                .filter(|parent| *parent != position)
        })
        .collect();
    for start in 0..nodes.len() {
        let mut cursor = parent_of[start];
        let mut steps = 0;
        while let Some(parent) = cursor {
            if parent == start {
                parent_of[start] = None;
                break;
            }
            steps += 1;
            if steps > nodes.len() {
                break;
            }
            cursor = parent_of[parent];
        }
    }
    parent_of
}

/// 节点按 `parent_id` 前序展开：找不到父节点的是根；成环的链在首个节点处断开；
/// 折叠节点的子树不展开。只展开 `keep` 为真的节点（`None` = 全部）。`keep` 须对
/// 祖先封闭（保留的节点其祖先也保留，[`visible_mask`] 保证），否则被丢掉的父节点下
/// 的子树整个看不见。
fn build_rows_where(
    nodes: &[AgentActivityNode],
    collapsed: &HashSet<String>,
    keep: Option<&[bool]>,
) -> Vec<TreeEntry<usize>> {
    let kept =
        |position: usize| keep.is_none_or(|keep| keep.get(position).copied().unwrap_or(false));
    let parent_of = parent_indices(nodes);
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
    let mut roots = Vec::new();
    for (position, parent) in parent_of.iter().enumerate() {
        if !kept(position) {
            continue;
        }
        match parent {
            Some(parent) => children[*parent].push(position),
            None => roots.push(position),
        }
    }
    // 活跃节点的整条祖先链置前，其余兄弟按结束时间倒序；相同键保留来源顺序。
    let mut active = vec![false; nodes.len()];
    for (position, node) in nodes.iter().enumerate() {
        if matches!(
            node.status,
            AgentActivityStatus::Running | AgentActivityStatus::Blocked
        ) {
            let mut cursor = Some(position);
            while let Some(index) = cursor {
                if active[index] {
                    break;
                }
                active[index] = true;
                cursor = parent_of[index];
            }
        }
    }
    let compare = |left: &usize, right: &usize| {
        active[*right].cmp(&active[*left]).then_with(|| {
            if active[*left] {
                std::cmp::Ordering::Equal
            } else {
                nodes[*right].ended_at_ms.cmp(&nodes[*left].ended_at_ms)
            }
        })
    };
    roots.sort_by(compare);
    for siblings in &mut children {
        siblings.sort_by(compare);
    }
    let mut rows = Vec::new();
    let mut stack: Vec<(usize, u8)> = roots.iter().rev().map(|root| (*root, 0)).collect();
    while let Some((position, depth)) = stack.pop() {
        let node = &nodes[position];
        let has_children = !children[position].is_empty();
        let is_collapsed = has_children && collapsed.contains(&node.id);
        rows.push(TreeEntry {
            depth,
            key: node.id.clone(),
            kind: position,
            last_child_mask: 0,
            has_children,
            collapsed: is_collapsed,
        });
        if !is_collapsed {
            let child_depth = depth.saturating_add(1).min(MAX_TREE_DEPTH);
            stack.extend(
                children[position]
                    .iter()
                    .rev()
                    .map(|child| (*child, child_depth)),
            );
        }
    }
    fill_last_child_masks(&mut rows);
    rows
}

/// 哪些节点可见：筛选词（不分大小写匹配标签、摘要、角色）保留匹配项及其祖先；
/// 「收起已完成」隐藏整棵子树都已完成（`Done`）的节点——失败、运行中与等待的
/// 节点及其祖先永远可见。结果对祖先封闭。
pub(super) fn visible_mask(
    nodes: &[AgentActivityNode],
    filter: &str,
    hide_done: bool,
) -> Vec<bool> {
    let parent_of = parent_indices(nodes);
    // 沿祖先链逐个置位；步数封顶，防异常数据里没断开的长链。
    let mark_ancestors = |mask: &mut Vec<bool>, start: usize| {
        let mut cursor = Some(start);
        let mut steps = 0;
        while let Some(index) = cursor {
            if steps > nodes.len() || mask[index] {
                break;
            }
            mask[index] = true;
            steps += 1;
            cursor = parent_of[index];
        }
    };
    let query = filter.trim().to_lowercase();
    let mut keep = if query.is_empty() {
        vec![true; nodes.len()]
    } else {
        let mut keep = vec![false; nodes.len()];
        for (position, node) in nodes.iter().enumerate() {
            let matches = [
                Some(node.label.as_str()),
                node.summary.as_deref(),
                node.agent_type.as_deref(),
            ]
            .into_iter()
            .flatten()
            .any(|text| text.to_lowercase().contains(&query));
            if matches {
                mark_ancestors(&mut keep, position);
            }
        }
        keep
    };
    if hide_done {
        let mut unfinished = vec![false; nodes.len()];
        for (position, node) in nodes.iter().enumerate() {
            if node.status != AgentActivityStatus::Done {
                mark_ancestors(&mut unfinished, position);
            }
        }
        for (kept, unfinished) in keep.iter_mut().zip(unfinished) {
            *kept &= unfinished;
        }
    }
    keep
}

/// 状态视图的条目：按状态桶的列表顺序（需要输入、失败、已完成、运行中、等待、未知）
/// 稳定排序，组内保持层级视图的前序；不分层、不折叠。
fn status_rows(nodes: &[AgentActivityNode], keep: &[bool]) -> Vec<TreeEntry<usize>> {
    let mut rows = build_rows_where(nodes, &HashSet::new(), Some(keep));
    rows.sort_by_key(|row| {
        crate::ui::design::activity_bucket(nodes[row.kind].status).listing_rank()
    });
    for row in &mut rows {
        row.depth = 0;
        row.last_child_mask = 0;
        row.has_children = false;
        row.collapsed = false;
    }
    rows
}

/// 属主的页头信息：pane 型取该端点快照里的 agent（名字、位置、状态、模型、会话
/// 标题），外部条目取其标签与工作目录；找不到时名字回退到 id。
fn owner_view(
    endpoints: &[ClientShellEndpoint],
    endpoint_id: &ClientEndpointId,
    owner: &AgentActivityOwner,
) -> AgentPageOwner {
    let snapshot = endpoint_snapshot(endpoints, endpoint_id);
    match owner {
        AgentActivityOwner::Pane { pane_id } => {
            let agent = snapshot.and_then(|snapshot| {
                snapshot
                    .agents
                    .iter()
                    .find(|agent| &agent.pane_id == pane_id)
            });
            let Some(agent) = agent else {
                return AgentPageOwner {
                    name: pane_id.clone(),
                    ..AgentPageOwner::default()
                };
            };
            let location = snapshot.and_then(|snapshot| {
                let workspace = snapshot
                    .workspaces
                    .iter()
                    .find(|workspace| workspace.workspace_id == agent.workspace_id)?;
                let tab = snapshot
                    .tabs
                    .iter()
                    .find(|tab| tab.tab_id == agent.tab_id)
                    .map(|tab| tab.label.as_str())
                    .filter(|label| !label.is_empty());
                Some(match tab {
                    Some(tab) => format!("{} / {tab}", workspace.label),
                    None => workspace.label.clone(),
                })
            });
            let bucket = client_agent_bucket(agent);
            AgentPageOwner {
                name: agent
                    .name
                    .clone()
                    .or_else(|| agent.display_agent.clone())
                    .or_else(|| agent.agent.clone())
                    .unwrap_or_else(|| pane_id.clone()),
                location,
                bucket: Some(bucket),
                since_ms: (bucket == AgentBucket::Working)
                    .then_some(agent.insight.working_since_ms)
                    .flatten(),
                model: agent
                    .insight
                    .model
                    .clone()
                    .filter(|model| !model.is_empty()),
                session_title: agent
                    .insight
                    .session_title
                    .clone()
                    .filter(|title| !title.is_empty()),
                agent: agent.agent.clone(),
            }
        }
        AgentActivityOwner::External { external_id } => {
            let agent = snapshot.and_then(|snapshot| {
                snapshot
                    .external_agents
                    .iter()
                    .find(|agent| &agent.external_id == external_id)
            });
            AgentPageOwner {
                name: agent
                    .map(|agent| agent.label.clone())
                    .filter(|label| !label.is_empty())
                    .unwrap_or_else(|| external_id.clone()),
                location: agent.and_then(|agent| agent.cwd.clone()),
                bucket: agent.map(|agent| agent_bucket(agent.agent_status)),
                since_ms: None,
                model: None,
                session_title: None,
                agent: agent.and_then(|agent| agent.agent.clone()),
            }
        }
    }
}

fn endpoint_snapshot<'a>(
    endpoints: &'a [ClientShellEndpoint],
    endpoint_id: &ClientEndpointId,
) -> Option<&'a ClientShellSnapshot> {
    endpoints
        .iter()
        .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
        .and_then(|endpoint| endpoint.snapshot.as_deref())
}

/// 快照里属主的活动摘要 (running, total)；属主不在快照里时为 `None`。
fn owner_summary(
    endpoints: &[ClientShellEndpoint],
    overlay: &ClientAgentActivityOverlay,
) -> Option<(u32, u32)> {
    let snapshot = endpoint_snapshot(endpoints, &overlay.endpoint_id)?;
    let activity = match &overlay.owner {
        AgentActivityOwner::Pane { pane_id } => {
            &snapshot
                .agents
                .iter()
                .find(|agent| &agent.pane_id == pane_id)?
                .activity
        }
        AgentActivityOwner::External { external_id } => {
            &snapshot
                .external_agents
                .iter()
                .find(|agent| &agent.external_id == external_id)?
                .activity
        }
    };
    Some((activity.running, activity.total))
}

/// 转写的解析方式：转写格式明确的属主（codex / claude / opencode）按区块解析，其余
/// （kimi、pi、外部来源等）每行都当正文。
fn transcript_mode(owner: &AgentPageOwner, external: bool) -> TranscriptMode {
    let agent = owner.agent.as_deref().map(str::to_ascii_lowercase);
    match agent.as_deref() {
        Some("codex" | "claude" | "opencode") if !external => TranscriptMode::Transcript,
        _ => TranscriptMode::Plain,
    }
}

/// 节点标签里 ` · ` 之前的昵称（Codex 子智能体的 `Hume · ui_audit` 取 `Hume`）。
pub(super) fn node_nickname(label: &str) -> &str {
    label
        .split_once(" · ")
        .map_or(label, |(nickname, _)| nickname)
}

fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

/// 节点耗时：结束时刻（未结束的运行态取现在）减开始时刻；缺时间戳时为 `None`。
pub(super) fn node_elapsed_ms(node: &AgentActivityNode, now_ms: u64) -> Option<u64> {
    let started = node.started_at_ms?;
    let ended = match node.ended_at_ms {
        Some(ended) => ended,
        None if matches!(
            node.status,
            AgentActivityStatus::Running | AgentActivityStatus::Blocked
        ) =>
        {
            now_ms
        }
        None => return None,
    };
    ended.checked_sub(started)
}

/// 紧凑耗时：`45s`、`12m05s`、`1h05m`。
pub(super) fn format_elapsed(ms: u64) -> String {
    let seconds = ms / 1000;
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m{:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h{:02}m", seconds / 3600, (seconds % 3600) / 60)
    }
}

pub(super) fn kind_label(kind: AgentActivityKind) -> &'static str {
    let texts = &crate::i18n::texts().agent_activity;
    match kind {
        AgentActivityKind::Subagent => texts.kind_subagent,
        AgentActivityKind::Task => texts.kind_task,
        AgentActivityKind::Todo => texts.kind_todo,
        AgentActivityKind::Background => texts.kind_background,
        AgentActivityKind::Unknown => texts.kind_unknown,
    }
}

pub(super) fn status_label(status: AgentActivityStatus) -> &'static str {
    let texts = &crate::i18n::texts().agent_activity;
    match status {
        AgentActivityStatus::Pending => texts.status_pending,
        AgentActivityStatus::Running => texts.status_running,
        AgentActivityStatus::Blocked => texts.status_blocked,
        AgentActivityStatus::Done => texts.status_done,
        AgentActivityStatus::Failed => texts.status_failed,
        AgentActivityStatus::Unknown => texts.status_unknown,
    }
}

/// 状态桶的短标签（状态视图的组标题、页头计数）。
pub(super) fn bucket_label(bucket: AgentBucket) -> &'static str {
    let texts = &crate::i18n::texts().agent_activity;
    match bucket {
        AgentBucket::NeedsInput => texts.status_blocked,
        AgentBucket::Failed => texts.status_failed,
        AgentBucket::Working => texts.status_running,
        AgentBucket::Review => texts.status_done,
        AgentBucket::Idle => texts.status_pending,
        AgentBucket::None => texts.group_unknown,
    }
}

impl ClientShellState {
    fn agent_activity(&self) -> Option<&ClientAgentActivityOverlay> {
        match self.overlay.as_ref() {
            Some(ClientShellOverlay::AgentActivity(overlay)) => Some(&**overlay),
            _ => None,
        }
    }

    fn agent_activity_mut(&mut self) -> Option<&mut ClientAgentActivityOverlay> {
        match self.overlay.as_mut() {
            Some(ClientShellOverlay::AgentActivity(overlay)) => Some(&mut **overlay),
            _ => None,
        }
    }

    /// 打开某个属主的子智能体页面并立即读树；已有浮层时记为返回目标（右键菜单不记；
    /// 从另一个活动页面打开时沿用它的返回目标，不叠层）。视图沿用本次会话上次所选。
    pub(super) fn open_agent_activity(
        &mut self,
        endpoint_id: ClientEndpointId,
        owner: AgentActivityOwner,
        outcome: &mut ClientShellInput,
    ) {
        let supported = self.supports_endpoint_method_for(
            &endpoint_id,
            &Method::AgentActivityRead(AgentActivityReadParams::default()),
        );
        let mut overlay = ClientAgentActivityOverlay::new(endpoint_id, owner, self.agent_page_view);
        overlay.next_follow_at = Some(Instant::now() + FOLLOW_INTERVAL);
        if !supported {
            overlay.mark_unsupported();
        }
        overlay.return_to = match self.overlay.take() {
            None | Some(ClientShellOverlay::ContextMenu(_)) => None,
            Some(ClientShellOverlay::AgentActivity(mut previous)) => previous.return_to.take(),
            Some(previous) => Some(Box::new(previous)),
        };
        self.overlay = Some(ClientShellOverlay::AgentActivity(Box::new(overlay)));
        outcome.repaint = true;
        self.pump_agent_activity(outcome);
    }

    /// 选中活动树的一个节点并读它的内容（键盘、点击与外部入口的唯一写入点）。
    pub(super) fn select_agent_activity_node(
        &mut self,
        node_id: String,
        outcome: &mut ClientShellInput,
    ) {
        let Some(overlay) = self.agent_activity_mut() else {
            return;
        };
        overlay.select(node_id);
        outcome.repaint = true;
        self.pump_agent_activity(outcome);
    }

    /// 发出下一个该发的读取（单飞：已有在途读取时什么也不做）。
    fn pump_agent_activity(&mut self, outcome: &mut ClientShellInput) {
        let Some(overlay) = self.agent_activity() else {
            return;
        };
        let Some((node_id, cursor, replace)) = overlay.next_read() else {
            return;
        };
        let (pane_id, external_id) = match &overlay.owner {
            AgentActivityOwner::Pane { pane_id } => (Some(pane_id.clone()), None),
            AgentActivityOwner::External { external_id } => (None, Some(external_id.clone())),
        };
        let endpoint_id = overlay.endpoint_id.clone();
        let generation = overlay.content_generation;
        let method = Method::AgentActivityRead(AgentActivityReadParams {
            pane_id,
            external_id,
            node_id: node_id.clone(),
            cursor: cursor.clone(),
            max_bytes: node_id.is_some().then_some(READ_MAX_BYTES),
            follow: overlay.follow_active(),
        });
        if !self.supports_endpoint_method_for(&endpoint_id, &method) {
            if let Some(overlay) = self.agent_activity_mut() {
                overlay.mark_unsupported();
            }
            outcome.repaint = true;
            return;
        }
        // 代际取全局请求序号：跨窗口、跨重开都不会撞上旧应答。
        let epoch = self.next_request_id;
        let kind = PendingEndpointKind::AgentActivityRead {
            epoch,
            node_id: node_id.clone(),
        };
        if !self.push_endpoint_method_for(&endpoint_id, method, kind, outcome) {
            // 端点暂不在线：保留意图，下一次 tick 再发。
            return;
        }
        if let Some(overlay) = self.agent_activity_mut() {
            overlay.read_epoch = epoch;
            if node_id.is_some() {
                overlay.want_content = false;
            } else {
                overlay.want_tree = false;
            }
            overlay.in_flight = Some(AgentActivityRead {
                epoch,
                node_id,
                cursor,
                replace,
                generation,
            });
        }
    }

    /// 客户端定时器（约 100 ms 一次）：跟随模式下树应答落地满 [`FOLLOW_INTERVAL`]
    /// 排一次树读取与内容续读，并把排队的读取发出去。树读取已排队或在途时不再
    /// 追加：否则树往返慢于间隔时每次树应答回来都又排上了树读取，内容续读永远
    /// 轮不到。页面没开时什么也不做。
    pub(crate) fn tick_agent_activity(&mut self, now: Instant, outcome: &mut ClientShellInput) {
        let Some(overlay) = self.agent_activity_mut() else {
            return;
        };
        if overlay.unsupported {
            return;
        }
        let tree_pending = overlay.want_tree
            || overlay
                .in_flight
                .as_ref()
                .is_some_and(|read| read.node_id.is_none());
        if overlay.follow_active()
            && !tree_pending
            && overlay.next_follow_at.is_none_or(|at| now >= at)
        {
            overlay.next_follow_at = Some(now + FOLLOW_INTERVAL);
            overlay.want_tree = true;
            if overlay.selected_node.is_some() {
                overlay.want_content = true;
            }
        }
        self.pump_agent_activity(outcome);
    }

    /// 视图计算阶段：墙钟、属主信息与状态字形、「有更新」、列表显示行与滚动（含
    /// 一次性 reveal）、转写同步与折行、内容滚动上界。页面矩形与绘制共用
    /// `floating_pages::overlay_bounds` 一个入口；渲染只读这里写下的结果（STATE-04）。
    pub(super) fn compute_agent_activity_view(&mut self, area: Rect, page_bounds: Option<Rect>) {
        let Some((summary, owner)) = self.agent_activity().map(|overlay| {
            (
                owner_summary(&self.endpoints, overlay),
                owner_view(&self.endpoints, &overlay.endpoint_id, &overlay.owner),
            )
        }) else {
            return;
        };
        // 端点离线 / 陈旧时页面是缓存：与会话树、面包屑同口径画静态字形，不驱动转圈。
        let glyphs = match self.agent_activity() {
            Some(overlay) if self.endpoint_stale(&overlay.endpoint_id) => {
                StatusGlyphs::fixed(self.config.status_indicators)
            }
            _ => self.status_glyphs(),
        };
        let own_footer = !self.workbench.enabled;
        let Some(overlay) = self.agent_activity_mut() else {
            return;
        };
        overlay.now_ms = unix_now_ms();
        overlay.observe_summary(summary);
        overlay.owner_view = owner;
        overlay.glyphs = glyphs;
        overlay.own_footer = own_footer;
        let Some(layout) = agent_page::page_layout(area, page_bounds, overlay) else {
            return;
        };
        overlay.list_lines = agent_page::list_lines(overlay, &layout);
        let visible = usize::from(layout.list.height);
        if visible > 0 {
            let total = overlay.list_lines.len();
            let mut start = overlay.list_scroll.min(total.saturating_sub(visible));
            if overlay.list_reveal {
                if let Some((first, last)) = overlay
                    .selected_row()
                    .and_then(|row| agent_page::item_span(&overlay.list_lines, row))
                {
                    start = start.min(first).max((last + 1).saturating_sub(visible));
                }
                overlay.list_reveal = false;
            }
            overlay.list_scroll = start;
            overlay.list_page = visible;
        }
        // 转写同步与宽度无关：窄屏只显示列表时也同步，切到转写后的按键（Enter 全部
        // 展开、点工具标题）立即有区块可切。
        let mode = transcript_mode(&overlay.owner_view, overlay.is_external());
        overlay.sync_transcript(mode);
        if !layout.summary.is_empty() {
            overlay.refresh_summary(mode, agent_page::summary_text_width(layout.summary));
        }
        let text = layout.content_text;
        let height = usize::from(text.height);
        if text.width == 0 || height == 0 {
            return;
        }
        let speaker = overlay
            .selected_node
            .as_deref()
            .and_then(|id| overlay.node(id))
            .map(|node| node_nickname(&node.label).to_owned())
            .unwrap_or_default();
        let ClientAgentActivityOverlay {
            content,
            transcript,
            ..
        } = overlay;
        let rows = match content.as_mut() {
            Some(content) if content.is_transcript() => {
                transcript.layout(text.width, &speaker);
                transcript.rows().len()
            }
            Some(content) => {
                content.ensure_wrapped(text.width);
                content.row_count()
            }
            None => 0,
        };
        let max = rows.saturating_sub(height);
        overlay.content_max_scroll = max;
        overlay.content_scroll = if overlay.content_tail {
            max
        } else {
            overlay.content_scroll.min(max)
        };
        overlay.content_page = height;
    }

    /// 键盘：↑↓ / jk 在列表里选节点、在转写里滚动；←→ / hl 折叠 / 展开；Tab 切换
    /// 焦点；Enter 在列表里打开转写、在转写里展开 / 收起全部工具输出；1-3 切换视图；
    /// `/` 筛选；c 收起已完成；f 跟随；r 刷新；t 跳到终端；PgUp / PgDn / Home / End
    /// 翻页与首尾；Esc 先退出筛选、再关闭页面并回到 `return_to`。其余按键吞掉。
    pub(super) fn route_agent_activity_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) -> bool {
        let Some(overlay) = self.agent_activity() else {
            return false;
        };
        let (code, modifiers) = crate::config::normalize_key_combo((key.code, key.modifiers));
        if overlay.filter_editing {
            self.route_agent_activity_filter_key(code, modifiers, outcome);
            return true;
        }
        let plain = modifiers.is_empty();
        match code {
            KeyCode::Esc => self.escape_agent_activity(outcome),
            KeyCode::Tab | KeyCode::BackTab => self.toggle_agent_activity_focus(outcome),
            KeyCode::Char('f') if plain => {
                self.activate_agent_activity_button(AgentActivityButton::ToggleFollow, outcome);
            }
            KeyCode::Char('r') if plain => {
                self.activate_agent_activity_button(AgentActivityButton::Refresh, outcome);
            }
            KeyCode::Char('t') if plain => {
                self.activate_agent_activity_button(AgentActivityButton::Jump, outcome);
            }
            KeyCode::Char('c') if plain => {
                self.activate_agent_activity_button(AgentActivityButton::ToggleDone, outcome);
            }
            KeyCode::Char('/') if plain => {
                self.activate_agent_activity_button(AgentActivityButton::Filter, outcome);
            }
            KeyCode::Char(digit @ '1'..='3') if plain => {
                let index = usize::from(digit as u8 - b'1');
                if let Some(view) = AgentPageView::ALL.get(index) {
                    self.activate_agent_activity_button(AgentActivityButton::View(*view), outcome);
                }
            }
            KeyCode::Up | KeyCode::Char('k') if plain => self.step_agent_activity(-1, outcome),
            KeyCode::Down | KeyCode::Char('j') if plain => self.step_agent_activity(1, outcome),
            KeyCode::Left | KeyCode::Char('h') if plain => self.collapse_agent_activity(outcome),
            KeyCode::Right | KeyCode::Char('l') if plain => self.expand_agent_activity(outcome),
            KeyCode::Enter => self.enter_agent_activity(outcome),
            KeyCode::PageUp => self.page_agent_activity(false, outcome),
            KeyCode::PageDown => self.page_agent_activity(true, outcome),
            KeyCode::Home => self.edge_agent_activity(false, outcome),
            KeyCode::End => self.edge_agent_activity(true, outcome),
            _ => {}
        }
        true
    }

    /// 筛选输入中：可打印字符追加、Backspace 删除、Enter 确认（退出输入、保留筛选）、
    /// Esc 清空并退出输入；↑↓ 照常在列表里选择。
    fn route_agent_activity_filter_key(
        &mut self,
        code: KeyCode,
        modifiers: crossterm::event::KeyModifiers,
        outcome: &mut ClientShellInput,
    ) {
        use crossterm::event::KeyModifiers;
        let typing = (modifiers - KeyModifiers::SHIFT).is_empty();
        match code {
            KeyCode::Up => return self.step_agent_activity(-1, outcome),
            KeyCode::Down => return self.step_agent_activity(1, outcome),
            _ => {}
        }
        let Some(overlay) = self.agent_activity_mut() else {
            return;
        };
        match code {
            KeyCode::Esc => {
                overlay.filter_editing = false;
                overlay.filter.clear();
            }
            KeyCode::Enter => overlay.filter_editing = false,
            KeyCode::Backspace => {
                overlay.filter.pop();
            }
            KeyCode::Char(ch) if typing && !ch.is_control() => {
                if overlay.filter.chars().count() < FILTER_MAX_CHARS {
                    overlay.filter.push(ch);
                }
            }
            _ => return,
        }
        overlay.rebuild_rows();
        overlay.list_scroll = 0;
        overlay.list_reveal = true;
        outcome.repaint = true;
    }

    /// Esc：有筛选词先清空筛选，否则关闭页面（回到 `return_to`）。
    fn escape_agent_activity(&mut self, outcome: &mut ClientShellInput) {
        let Some(overlay) = self.agent_activity_mut() else {
            return;
        };
        if !overlay.filter.is_empty() {
            overlay.filter.clear();
            overlay.rebuild_rows();
            overlay.list_reveal = true;
        } else {
            self.close_agent_activity();
        }
        outcome.repaint = true;
    }

    fn toggle_agent_activity_focus(&mut self, outcome: &mut ClientShellInput) {
        if let Some(overlay) = self.agent_activity_mut() {
            overlay.focus = match overlay.focus {
                AgentActivityFocus::Tree => AgentActivityFocus::Content,
                AgentActivityFocus::Content => AgentActivityFocus::Tree,
            };
            overlay.list_reveal = true;
            outcome.repaint = true;
        }
    }

    /// 切换视图并记到本次会话（重开页面沿用）。
    fn set_agent_page_view(&mut self, view: AgentPageView, outcome: &mut ClientShellInput) {
        self.agent_page_view = view;
        let Some(overlay) = self.agent_activity_mut() else {
            return;
        };
        if overlay.view == view {
            return;
        }
        overlay.view = view;
        overlay.rebuild_rows();
        overlay.list_scroll = 0;
        overlay.list_reveal = true;
        outcome.repaint = true;
    }

    /// 页头按钮、视图标签、筛选入口、「已完成 N 项」行与对应快捷键的共同入口。
    pub(super) fn activate_agent_activity_button(
        &mut self,
        button: AgentActivityButton,
        outcome: &mut ClientShellInput,
    ) {
        match button {
            AgentActivityButton::Close | AgentActivityButton::Back => {
                self.close_agent_activity();
                outcome.repaint = true;
            }
            AgentActivityButton::Jump => self.jump_from_agent_activity(outcome),
            AgentActivityButton::View(view) => self.set_agent_page_view(view, outcome),
            AgentActivityButton::Filter => {
                let Some(overlay) = self.agent_activity_mut() else {
                    return;
                };
                overlay.filter_editing = true;
                overlay.focus = AgentActivityFocus::Tree;
                outcome.repaint = true;
            }
            AgentActivityButton::ToggleDone => {
                let Some(overlay) = self.agent_activity_mut() else {
                    return;
                };
                overlay.hide_done = !overlay.hide_done;
                overlay.rebuild_rows();
                overlay.list_reveal = true;
                outcome.repaint = true;
            }
            AgentActivityButton::ToggleFollow => {
                let Some(overlay) = self.agent_activity_mut() else {
                    return;
                };
                if overlay.is_external() || overlay.unsupported {
                    return;
                }
                overlay.follow = !overlay.follow;
                overlay.content_tail = overlay.follow;
                overlay.has_updates = false;
                // 打开跟随：下一次 tick 立即刷新一轮。
                overlay.next_follow_at = None;
                outcome.repaint = true;
            }
            AgentActivityButton::Refresh => {
                let Some(overlay) = self.agent_activity_mut() else {
                    return;
                };
                if overlay.unsupported {
                    return;
                }
                overlay.want_tree = true;
                overlay.has_updates = false;
                if overlay.selected_node.is_some() {
                    overlay.restart_selected_content();
                }
                outcome.repaint = true;
                self.pump_agent_activity(outcome);
            }
        }
    }

    /// t / 「跳到终端」：聚焦属主所在的 pane 并关闭页面（与 Inspector 的跳转同一
    /// 做法）。外部来源没有终端，什么也不做。
    fn jump_from_agent_activity(&mut self, outcome: &mut ClientShellInput) {
        let Some((endpoint_id, pane_id)) = self.agent_activity().and_then(|overlay| match &overlay
            .owner
        {
            AgentActivityOwner::Pane { pane_id } => {
                Some((overlay.endpoint_id.clone(), pane_id.clone()))
            }
            AgentActivityOwner::External { .. } => None,
        }) else {
            return;
        };
        self.overlay = None;
        self.refocus_current_workbench_terminal();
        self.focus_or_activate(
            endpoint_id,
            ClientEndpointFocusTarget::Pane(pane_id),
            outcome,
        );
        outcome.repaint = true;
    }

    fn step_agent_activity(&mut self, delta: isize, outcome: &mut ClientShellInput) {
        let Some(overlay) = self.agent_activity_mut() else {
            return;
        };
        match overlay.focus {
            AgentActivityFocus::Content => {
                overlay.scroll_content(delta);
                outcome.repaint = true;
            }
            AgentActivityFocus::Tree => {
                if overlay.rows.is_empty() {
                    return;
                }
                let last = overlay.rows.len() - 1;
                let next = match overlay.selected_row() {
                    Some(index) => index.saturating_add_signed(delta).min(last),
                    None if delta < 0 => last,
                    None => 0,
                };
                let node_id = overlay.rows[next].key.clone();
                self.select_agent_activity_node(node_id, outcome);
            }
        }
    }

    /// ←：展开的节点先折叠，否则跳到父节点（状态视图不分层，不跳）；焦点在转写时
    /// 回到列表。
    fn collapse_agent_activity(&mut self, outcome: &mut ClientShellInput) {
        let Some(overlay) = self.agent_activity_mut() else {
            return;
        };
        if overlay.focus == AgentActivityFocus::Content {
            overlay.focus = AgentActivityFocus::Tree;
            overlay.list_reveal = true;
            outcome.repaint = true;
            return;
        }
        if overlay.view == AgentPageView::Status {
            return;
        }
        let Some(index) = overlay.selected_row() else {
            return;
        };
        let entry = &overlay.rows[index];
        if entry.has_children && !entry.collapsed {
            let key = entry.key.clone();
            overlay.toggle_collapsed(&key);
            outcome.repaint = true;
            return;
        }
        let parent = overlay
            .node(&entry.key)
            .and_then(|node| node.parent_id.clone())
            .filter(|parent| overlay.rows.iter().any(|row| &row.key == parent));
        if let Some(parent) = parent {
            self.select_agent_activity_node(parent, outcome);
        }
    }

    /// →：折叠的节点展开，已展开的跳到第一个子节点。
    fn expand_agent_activity(&mut self, outcome: &mut ClientShellInput) {
        let Some(overlay) = self.agent_activity_mut() else {
            return;
        };
        if overlay.focus != AgentActivityFocus::Tree {
            return;
        }
        let Some(index) = overlay.selected_row() else {
            return;
        };
        let entry = &overlay.rows[index];
        if !entry.has_children {
            return;
        }
        if entry.collapsed {
            let key = entry.key.clone();
            overlay.toggle_collapsed(&key);
            outcome.repaint = true;
            return;
        }
        if let Some(child) = overlay.rows.get(index + 1).map(|row| row.key.clone()) {
            self.select_agent_activity_node(child, outcome);
        }
    }

    /// Enter：列表里没有选中时选第一项；有选中时从头重读它的内容并把焦点交给转写。
    /// 焦点在转写时展开 / 收起全部工具输出。
    fn enter_agent_activity(&mut self, outcome: &mut ClientShellInput) {
        let Some(overlay) = self.agent_activity_mut() else {
            return;
        };
        if overlay.focus == AgentActivityFocus::Content {
            if overlay
                .content
                .as_ref()
                .is_some_and(AgentActivityContentView::is_transcript)
                && overlay.transcript.toggle_all()
            {
                outcome.repaint = true;
            }
            return;
        }
        match overlay.selected_row() {
            None => {
                if let Some(first) = overlay.rows.first().map(|row| row.key.clone()) {
                    self.select_agent_activity_node(first, outcome);
                }
            }
            Some(_) => {
                overlay.restart_selected_content();
                overlay.focus = AgentActivityFocus::Content;
                outcome.repaint = true;
                self.pump_agent_activity(outcome);
            }
        }
    }

    fn page_agent_activity(&mut self, down: bool, outcome: &mut ClientShellInput) {
        let Some(overlay) = self.agent_activity_mut() else {
            return;
        };
        match overlay.focus {
            AgentActivityFocus::Tree => {
                let page = overlay.list_page.max(1);
                overlay.list_scroll = if down {
                    overlay.list_scroll.saturating_add(page)
                } else {
                    overlay.list_scroll.saturating_sub(page)
                };
            }
            AgentActivityFocus::Content => {
                let page = isize::try_from(overlay.content_page.max(1)).unwrap_or(isize::MAX);
                overlay.scroll_content(if down { page } else { -page });
            }
        }
        outcome.repaint = true;
    }

    fn edge_agent_activity(&mut self, end: bool, outcome: &mut ClientShellInput) {
        let Some(overlay) = self.agent_activity_mut() else {
            return;
        };
        match overlay.focus {
            AgentActivityFocus::Tree => {
                let target = if end {
                    overlay.rows.last()
                } else {
                    overlay.rows.first()
                };
                if let Some(node_id) = target.map(|row| row.key.clone()) {
                    self.select_agent_activity_node(node_id, outcome);
                }
            }
            AgentActivityFocus::Content => {
                overlay.content_scroll = if end { overlay.content_max_scroll } else { 0 };
                overlay.content_tail = end && overlay.follow_active();
                outcome.repaint = true;
            }
        }
    }

    /// 鼠标：点列表项选中、点折叠开关折叠、点视图标签 / 页头按钮 / 「已完成 N 项」行、
    /// 点工具标题行切换该工具输出、点转写把焦点交给转写、点或拖滚动条跳转；滚轮按
    /// 指针所在的栏滚动；页面外（工作台标题栏、状态栏）的点击吞掉、不关闭页面。
    /// 悬浮节点只由 `Moved` 改写（MENU-01）。页面开着时吞掉全部鼠标事件。
    pub(super) fn handle_agent_activity_mouse(
        &mut self,
        mouse: MouseEvent,
        point: (u16, u16),
        outcome: &mut ClientShellInput,
    ) -> bool {
        if self.agent_activity().is_none() {
            return false;
        }
        let page = self.hits.agent_activity_popup;
        let content = self.hits.agent_activity_content;
        let row_hit = self
            .hits
            .agent_activity_tree_rows
            .iter()
            .find(|(rect, _)| super::contains(*rect, point))
            .map(|(rect, id)| (*rect, id.clone()));
        match mouse.kind {
            MouseEventKind::Moved => {
                let hovered = row_hit.map(|(_, id)| id);
                if let Some(overlay) = self.agent_activity_mut() {
                    if overlay.hovered_node != hovered {
                        overlay.hovered_node = hovered;
                        outcome.repaint = true;
                    }
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if !super::contains(page, point) {
                    // 工作台标题栏 / 状态栏：页面是整块内容区，不当作「点外面关闭」。
                } else if let Some(button) = self
                    .hits
                    .agent_activity_actions
                    .iter()
                    .find(|(rect, _)| super::contains(*rect, point))
                    .map(|(_, button)| *button)
                {
                    self.activate_agent_activity_button(button, outcome);
                } else if let Some((rect, node_id)) = row_hit {
                    self.click_agent_activity_row(rect, node_id, point, outcome);
                } else if super::contains(self.hits.agent_activity_scrollbar, point) {
                    let track = self.hits.agent_activity_scrollbar;
                    if let Some(overlay) = self.agent_activity_mut() {
                        overlay.scroll_drag = true;
                        overlay.scroll_to_track(track, point.1);
                        overlay.focus = AgentActivityFocus::Content;
                        outcome.repaint = true;
                    }
                } else if let Some(tool) = self
                    .hits
                    .agent_activity_toggles
                    .iter()
                    .find(|(rect, _)| super::contains(*rect, point))
                    .map(|(_, tool)| *tool)
                {
                    if let Some(overlay) = self.agent_activity_mut() {
                        overlay.focus = AgentActivityFocus::Content;
                        overlay.transcript.toggle(tool);
                        outcome.repaint = true;
                    }
                } else if super::contains(content, point) {
                    if let Some(overlay) = self.agent_activity_mut() {
                        if overlay.focus != AgentActivityFocus::Content {
                            overlay.focus = AgentActivityFocus::Content;
                            outcome.repaint = true;
                        }
                    }
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let track = self.hits.agent_activity_scrollbar;
                if let Some(overlay) = self.agent_activity_mut() {
                    if overlay.scroll_drag && !track.is_empty() {
                        overlay.scroll_to_track(track, point.1);
                        outcome.repaint = true;
                    }
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if let Some(overlay) = self.agent_activity_mut() {
                    overlay.scroll_drag = false;
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                if super::contains(page, point) =>
            {
                let lines = isize::try_from(self.config.mouse_scroll_lines).unwrap_or(3);
                let delta = if mouse.kind == MouseEventKind::ScrollUp {
                    -lines
                } else {
                    lines
                };
                let over_content = super::contains(content, point);
                if let Some(overlay) = self.agent_activity_mut() {
                    if over_content {
                        overlay.scroll_content(delta);
                    } else {
                        overlay.list_scroll = overlay.list_scroll.saturating_add_signed(delta);
                    }
                    outcome.repaint = true;
                }
            }
            _ => {}
        }
        true
    }

    /// 点列表项：层级视图里落在本节点的折叠开关上就切换折叠，否则选中。
    fn click_agent_activity_row(
        &mut self,
        rect: Rect,
        node_id: String,
        point: (u16, u16),
        outcome: &mut ClientShellInput,
    ) {
        let Some(overlay) = self.agent_activity_mut() else {
            return;
        };
        overlay.focus = AgentActivityFocus::Tree;
        let toggle = overlay.view == AgentPageView::Tree
            && overlay
                .rows
                .iter()
                .find(|row| row.key == node_id && row.has_children)
                .is_some_and(|row| {
                    super::contains(agent_page::fold_toggle_rect(rect, row.depth), point)
                });
        if toggle {
            overlay.toggle_collapsed(&node_id);
            outcome.repaint = true;
            return;
        }
        self.select_agent_activity_node(node_id, outcome);
    }

    fn close_agent_activity(&mut self) {
        self.overlay = match self.overlay.take() {
            Some(ClientShellOverlay::AgentActivity(mut overlay)) => {
                overlay.return_to.take().map(|previous| *previous)
            }
            other => other,
        };
    }

    /// `agent.activity.read` 的响应：窗口已关、对不上在途读取的一律丢弃。只有成功
    /// 应答立即发出下一个排队的读取（续读 / 先树后内容）；错误结局不在这里续发，
    /// 排队的读取交给下一次 tick（≤ 100 ms）。取消路径（断线、代际不符、泳道退役）
    /// 经 `cancel_endpoint_request` 进来，那里不允许再产生动作；被取消的读取也不是
    /// 读取失败，意图放回队列，端点恢复后由 tick 重发。
    pub(super) fn receive_agent_activity_read(
        &mut self,
        epoch: u64,
        node_id: Option<String>,
        result: Result<ResponseResult, ClientShellEndpointError>,
    ) -> (bool, Vec<ClientShellAction>) {
        let Some(overlay) = self.agent_activity_mut() else {
            return (false, Vec::new());
        };
        let Some(read) = overlay
            .in_flight
            .take_if(|read| read.epoch == epoch && read.node_id == node_id)
        else {
            return (false, Vec::new());
        };
        let succeeded = match &result {
            Ok(_) => true,
            Err(error) if error.code.as_deref() == Some("endpoint_cancelled") => {
                overlay.requeue(&read);
                return (false, Vec::new());
            }
            Err(_) => false,
        };
        let tree_read = read.node_id.is_none();
        let repaint = overlay.apply_read(read, result);
        if tree_read {
            // 跟随间隔从树应答落地（成功或失败）时起算。
            overlay.next_follow_at = Some(Instant::now() + FOLLOW_INTERVAL);
        }
        if !succeeded {
            return (repaint, Vec::new());
        }
        let mut outcome = ClientShellInput::default();
        self.pump_agent_activity(&mut outcome);
        (repaint || outcome.repaint, outcome.actions)
    }
}

impl ClientAgentActivityOverlay {
    /// 视图计算阶段：所选节点的 Text 内容有变化（节点、内容代际、修订号或解析方式）
    /// 时重新同步转写；换了节点或内容实例（重新选中、游标失效重读）先丢掉旧的展开
    /// 状态。
    fn sync_transcript(&mut self, mode: TranscriptMode) {
        let Some(content) = self
            .content
            .as_ref()
            .filter(|content| content.is_transcript())
        else {
            return;
        };
        let current =
            self.transcript_synced
                .as_ref()
                .is_some_and(|(node, generation, revision, synced)| {
                    node == &content.node_id
                        && *generation == content.generation
                        && *revision == content.revision
                        && *synced == mode
                });
        if current {
            return;
        }
        if self
            .transcript_synced
            .as_ref()
            .is_none_or(|(node, generation, _, _)| {
                node != &content.node_id || *generation != content.generation
            })
        {
            self.transcript = TranscriptView::default();
        }
        self.transcript.sync(&content.lines, content.dropped, mode);
        self.transcript_synced = Some((
            content.node_id.clone(),
            content.generation,
            content.revision,
            mode,
        ));
    }

    /// 视图计算阶段：概要的工具调用数与「最近输出」折行随转写或宽度变化重算。
    fn refresh_summary(&mut self, mode: TranscriptMode, width: usize) {
        let Some(content) = self
            .content
            .as_ref()
            .filter(|content| content.is_transcript())
        else {
            self.summary = AgentPageSummary::default();
            return;
        };
        let current = self.summary.key.as_ref().is_some_and(
            |(node, generation, revision, synced, cached)| {
                node == &content.node_id
                    && *generation == content.generation
                    && *revision == content.revision
                    && *synced == mode
                    && *cached == width
            },
        );
        if current {
            return;
        }
        let mut last_output = Vec::new();
        if let Some(last) = self.transcript.last_output() {
            wrap_cjk(last, width, width, |start, end| {
                last_output.push(last[start..end].to_owned());
            });
        }
        self.summary = AgentPageSummary {
            key: Some((
                content.node_id.clone(),
                content.generation,
                content.revision,
                mode,
                width,
            )),
            node: content.node_id.clone(),
            tool_calls: self.transcript.stats().tool_calls,
            last_output,
        };
    }

    /// 按行滚内容；滚到底且在跟随时重新钉住底部。上界由视图计算阶段夹紧。
    fn scroll_content(&mut self, delta: isize) {
        self.content_scroll = self.content_scroll.saturating_add_signed(delta);
        if delta < 0 {
            self.content_tail = false;
        } else if self.content_scroll >= self.content_max_scroll {
            self.content_scroll = self.content_max_scroll;
            self.content_tail = self.follow_active();
        }
    }

    /// 滚动条点击 / 拖动：按指针在轨道上的位置跳转。
    fn scroll_to_track(&mut self, track: Rect, row: u16) {
        let span = usize::from(track.height.saturating_sub(1)).max(1);
        let offset = usize::from(row.saturating_sub(track.y));
        self.content_scroll = self.content_max_scroll * offset.min(span) / span;
        self.content_tail = self.follow_active() && self.content_scroll >= self.content_max_scroll;
    }
}
