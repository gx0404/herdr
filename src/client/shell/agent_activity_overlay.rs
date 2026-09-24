//! 「Agent 活动」二级窗口：左列活动树、右列所选节点的内容。只读窗口，数据来自
//! `agent.activity.read`；快照里的活动摘要只用来提示「有更新」。
//!
//! 数据通道（单飞）：窗口同时至多一个在途读取。打开时先读树（`node_id` 省略），
//! 选中节点后读内容（`node_id` 给出、从头读）；内容没读完（`eof = false`）就按
//! `next_cursor` 立即续读。跟随模式下树应答落地后每隔 [`FOLLOW_INTERVAL`] 重发
//! 一次树读取（`follow = true`，让 server 在 `FOLLOW_TTL` 内保持跟随），树回来
//! 后按上次的 `next_cursor` 续读内容；`eof` 且没有游标（pi）时下次从头重读并
//! 整体替换。树读取已排队或在途时不再追加，树往返慢于间隔也饿不死内容续读。
//! 每个请求的 `epoch` 取全局递增的请求序号，响应对不上在途请求即丢弃；内容读取
//! 另带内容代际，选中节点或刷新之后到达的旧内容也丢弃。只有成功应答立即续发
//! 下一个读取；错误结局交给下一次 tick，被取消的读取（断线等）放回队列、不算
//! 读取失败。窗口关闭后不再发起任何读取。外部来源属主忽略跟随（server 也
//! 忽略）。旧 server 未宣告或拒绝该方法时显示「不提供」文案并不再重试。
//!
//! 阶段划分（STATE-04）：输入阶段改选中 / 折叠 / 滚动意图并排队读取；视图计算
//! 阶段（[`ClientShellState::compute_agent_activity_view`]）按当前几何夹紧滚动、
//! 折行内容、记下翻页步长；渲染阶段（[`render_agent_activity_overlay`]）只读。
//! 窗口不在 pane 规模的循环里，每帧只画可见的树行与内容行。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use super::render::{display_width, panel, panel_inner, put_text, OverlayRender};
use super::*;
use crate::api::schema::{
    AgentActivityContent, AgentActivityContentFormat, AgentActivityKind, AgentActivityNode,
    AgentActivityReadParams, AgentActivityStatus, Method, ResponseResult,
};
use crate::ui::kit::footer_hints::{render_footer_hints, FooterHint};
use crate::ui::kit::tree::{fill_last_child_masks, render_tree_prefix, TreeEntry, MAX_TREE_DEPTH};
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
/// 左列宽度占内框的比例与下限；内框窄于 [`NARROW_WIDTH`] 时只显示一列。
const TREE_SHARE_PERCENT: u16 = 35;
const TREE_MIN_WIDTH: u16 = 24;
pub(super) const NARROW_WIDTH: u16 = 60;
/// 树行左侧留白列数（开关命中区据此换算）。
const TREE_PAD: u16 = 1;

/// 活动树的属主：某个 pane 里的 agent，或不属于任何 pane 的外部来源条目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum AgentActivityOwner {
    Pane { pane_id: String },
    External { external_id: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AgentActivityButton {
    Close,
    ToggleFollow,
    Refresh,
}

/// 键盘焦点所在的列。窄窗口只显示焦点所在的一列。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum AgentActivityFocus {
    #[default]
    Tree,
    Content,
}

/// 在途读取（窗口同时只有一个）。
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
    marker: usize,
}

/// 折行后的一个显示行：`lines[line]` 的 `[start, end)` 字节区间。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WrapRow {
    line: u32,
    start: u32,
    end: u32,
}

/// 已选节点的内容：按页追加，显示行与折行结果增量维护。
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
    /// server 标了截断，或客户端因上限丢过头部：顶部提示「仅显示最新部分」。
    pub(super) truncated: bool,
    pub(super) eof: bool,
    pub(super) next_cursor: Option<String>,
    rows: Vec<WrapRow>,
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

    /// 折行后的显示行数（视图计算阶段之后有效）。
    pub(super) fn row_count(&self) -> usize {
        self.rows.len()
    }

    fn clear(&mut self) {
        self.lines.clear();
        self.partial = None;
        self.bytes = 0;
        self.truncated = false;
        self.invalidate_from(0);
    }

    /// 追加一页原文：完整的行整理成显示行，末尾半行暂存并先显示出来。
    fn append(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
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

    /// 超出上限时从头部整行丢弃；折行结果随之平移，不整体重算。
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

    /// 按 `width` 列折行：宽度变了全部重折，否则只折新增的行。
    fn ensure_wrapped(&mut self, width: u16) {
        if width == 0 {
            return;
        }
        if width != self.wrap_width {
            self.wrap_width = width;
            self.rows.clear();
            self.wrapped = 0;
        }
        for (index, line) in self.lines.iter().enumerate().skip(self.wrapped) {
            let line_index = u32::try_from(index).unwrap_or(u32::MAX);
            wrap_line(&line.text, width, |start, end| {
                self.rows.push(WrapRow {
                    line: line_index,
                    start,
                    end,
                });
            });
        }
        self.wrapped = self.lines.len();
    }
}

/// 按显示宽度折一行，逐段回调 `[start, end)` 字节区间；空行也占一行。优先在
/// 本段最后一个空格之后断行（空格留在上一段末尾），没有空格时按列硬断。
fn wrap_line(text: &str, width: u16, mut emit: impl FnMut(u32, u32)) {
    let width = usize::from(width.max(1));
    let to32 = |value: usize| u32::try_from(value).unwrap_or(u32::MAX);
    let mut start = 0usize;
    let mut used = 0usize;
    // 本段里最近一个空格之后的位置，及到它为止占用的列数。
    let mut soft_break: Option<(usize, usize)> = None;
    for (offset, ch) in text.char_indices() {
        let cell = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        while used + cell > width && offset > start {
            match soft_break
                .take()
                .filter(|(at, _)| *at > start && *at <= offset)
            {
                Some((at, used_at)) => {
                    emit(to32(start), to32(at));
                    start = at;
                    used -= used_at;
                }
                None => {
                    emit(to32(start), to32(offset));
                    start = offset;
                    used = 0;
                }
            }
        }
        used += cell;
        if ch == ' ' {
            soft_break = Some((offset + ch.len_utf8(), used));
        }
    }
    emit(to32(start), to32(text.len()));
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
    pub(super) tree_scroll: usize,
    /// 键盘移动选中后要求把它滚进视野；视图计算阶段消费。
    tree_reveal: bool,
    pub(super) content_scroll: usize,
    /// 视图计算阶段写入，渲染只读（STATE-04）。
    pub(super) content_max_scroll: usize,
    /// 内容钉在底部：新内容到达后视图计算阶段把滚动跟到最新（跟随时的默认）。
    pub(super) content_tail: bool,
    /// 视图计算阶段记下的一页行数（PgUp / PgDn 步长）。
    tree_page: usize,
    content_page: usize,
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
    /// 按父子前序展开、跳过折叠子树的可见行（`kind` = `nodes` 下标）。
    rows: Vec<TreeEntry<usize>>,
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
    /// server 未宣告或拒绝 `agent.activity.read`：显示「不提供」并不再重试。
    pub(super) unsupported: bool,
    /// 视图计算阶段记下的墙钟毫秒，渲染据此算运行中节点的耗时。
    now_ms: u64,
    /// 从别的浮层（如右键菜单之外的入口）打开时，Esc 回到它。
    pub(super) return_to: Option<Box<ClientShellOverlay>>,
}

impl ClientAgentActivityOverlay {
    fn new(endpoint_id: ClientEndpointId, owner: AgentActivityOwner) -> Self {
        let follow = matches!(owner, AgentActivityOwner::Pane { .. });
        Self {
            endpoint_id,
            owner,
            selected_node: None,
            hovered_node: None,
            collapsed: HashSet::new(),
            tree_scroll: 0,
            tree_reveal: false,
            content_scroll: 0,
            content_max_scroll: 0,
            content_tail: follow,
            tree_page: 1,
            content_page: 1,
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
            unsupported: false,
            now_ms: 0,
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

    /// 可见树行的节点 id（按显示顺序）。
    #[cfg(test)]
    pub(super) fn visible_node_ids(&self) -> impl Iterator<Item = &str> {
        self.rows.iter().map(|row| row.key.as_str())
    }

    fn selected_row(&self) -> Option<usize> {
        let selected = self.selected_node.as_deref()?;
        self.rows.iter().position(|row| row.key == selected)
    }

    fn node(&self, id: &str) -> Option<&AgentActivityNode> {
        self.nodes.iter().find(|node| node.id == id)
    }

    fn mark_unsupported(&mut self) {
        self.unsupported = true;
        self.want_tree = false;
        self.want_content = false;
        self.in_flight = None;
    }

    /// 改选中节点：内容换新代际、清空，从头读。已是它时只要求滚进视野。
    fn select(&mut self, node_id: String) -> bool {
        self.tree_reveal = true;
        if self.selected_node.as_deref() == Some(node_id.as_str()) {
            return false;
        }
        self.selected_node = Some(node_id);
        self.restart_selected_content();
        self.content = None;
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

    fn rebuild_rows(&mut self) {
        self.rows = build_rows(&self.nodes, &self.collapsed);
    }

    fn toggle_collapsed(&mut self, node_id: &str) -> bool {
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
            self.content = Some(AgentActivityContentView::new(node.to_owned()));
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
fn method_unsupported(error: &ClientShellEndpointError) -> bool {
    match error.code.as_deref() {
        Some("unsupported_method" | "method_not_found") => true,
        Some("invalid_request") => error.message.contains("unknown variant"),
        _ => false,
    }
}

/// 节点按 `parent_id` 前序展开：找不到父节点的是根；成环的链在首个节点处断开；
/// 折叠节点的子树不展开。
fn build_rows(nodes: &[AgentActivityNode], collapsed: &HashSet<String>) -> Vec<TreeEntry<usize>> {
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
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
    let mut roots = Vec::new();
    for (position, parent) in parent_of.iter().enumerate() {
        match parent {
            Some(parent) => children[*parent].push(position),
            None => roots.push(position),
        }
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

/// 属主的显示名：pane 型取该端点快照里 agent 的名字，外部条目取其标签；找不到
/// 时回退到 id。
fn owner_label(overlay: &ClientAgentActivityOverlay, endpoints: &[ClientShellEndpoint]) -> String {
    let snapshot = endpoint_snapshot(endpoints, &overlay.endpoint_id);
    match &overlay.owner {
        AgentActivityOwner::Pane { pane_id } => snapshot
            .and_then(|snapshot| {
                snapshot
                    .agents
                    .iter()
                    .find(|agent| &agent.pane_id == pane_id)
            })
            .and_then(|agent| {
                agent
                    .name
                    .clone()
                    .or_else(|| agent.display_agent.clone())
                    .or_else(|| agent.agent.clone())
            })
            .unwrap_or_else(|| pane_id.clone()),
        AgentActivityOwner::External { external_id } => snapshot
            .and_then(|snapshot| {
                snapshot
                    .external_agents
                    .iter()
                    .find(|agent| &agent.external_id == external_id)
            })
            .map(|agent| agent.label.clone())
            .filter(|label| !label.is_empty())
            .unwrap_or_else(|| external_id.clone()),
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

fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

/// 节点耗时：结束时刻（未结束的运行态取现在）减开始时刻；缺时间戳时为 `None`。
fn node_elapsed_ms(node: &AgentActivityNode, now_ms: u64) -> Option<u64> {
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

fn kind_label(kind: AgentActivityKind) -> &'static str {
    let texts = &crate::i18n::texts().agent_activity;
    match kind {
        AgentActivityKind::Subagent => texts.kind_subagent,
        AgentActivityKind::Task => texts.kind_task,
        AgentActivityKind::Todo => texts.kind_todo,
        AgentActivityKind::Background => texts.kind_background,
        AgentActivityKind::Unknown => texts.kind_unknown,
    }
}

fn status_label(status: AgentActivityStatus) -> &'static str {
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

fn status_glyph(status: AgentActivityStatus) -> &'static str {
    match status {
        AgentActivityStatus::Running => "●",
        AgentActivityStatus::Pending => "○",
        AgentActivityStatus::Blocked => "◐",
        AgentActivityStatus::Done => "✓",
        AgentActivityStatus::Failed => "✗",
        AgentActivityStatus::Unknown => "·",
    }
}

/// 状态色：运行中强调、失败红、完成灰、受阻黄，其余常规文字色。
fn status_color(status: AgentActivityStatus, palette: &Palette) -> ratatui::style::Color {
    match status {
        AgentActivityStatus::Running => palette.accent,
        AgentActivityStatus::Failed => palette.red,
        AgentActivityStatus::Done => palette.overlay0,
        AgentActivityStatus::Blocked => palette.yellow,
        AgentActivityStatus::Pending | AgentActivityStatus::Unknown => palette.text,
    }
}

/// 窗口版式：视图计算与渲染共用同一份（STATE-04）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct AgentActivityLayout {
    pub(super) popup: Rect,
    /// 内框第一行：标题、徽标与按钮。
    pub(super) title: Rect,
    pub(super) footer: Rect,
    pub(super) narrow: bool,
    /// 标题栏与页脚之间的整块区域（两列与分隔线）。
    pub(super) body: Rect,
    /// 左列（含表头行）；窄窗口焦点在内容时为空。
    pub(super) tree: Rect,
    pub(super) tree_rows: Rect,
    pub(super) divider: Rect,
    /// 右列（含头行）；窄窗口焦点在树时为空。
    pub(super) content: Rect,
    pub(super) content_header: Rect,
    /// 头行之下、滚动条之左的正文区。
    pub(super) content_text: Rect,
    /// 正文右侧预留的滚动条列（常驻，折行宽度不随滚动条出没变化）。
    pub(super) scrollbar: Rect,
}

/// 右列头行数：节点标题行 + 摘要行（有 summary / agent_type 时）+ 提示行（截断
/// 或读取失败时）。
fn content_header_rows(overlay: &ClientAgentActivityOverlay) -> u16 {
    let node = overlay
        .selected_node
        .as_deref()
        .and_then(|id| overlay.node(id));
    let detail = node.is_some_and(|node| node.summary.is_some() || node.agent_type.is_some());
    let notice = overlay.content.as_ref().is_some_and(|content| {
        content.truncated || (overlay.error.is_some() && !content.lines.is_empty())
    });
    1 + u16::from(detail) + u16::from(notice)
}

pub(super) fn agent_activity_layout(
    area: Rect,
    page_bounds: Option<Rect>,
    overlay: &ClientAgentActivityOverlay,
) -> Option<AgentActivityLayout> {
    let popup = page_bounds
        .map(|rect| rect.intersection(area))
        .filter(|rect| !rect.is_empty())
        .or_else(|| crate::ui::modal_rect(area, crate::ui::ModalSize::Large))?;
    let inner = panel_inner(popup)?;
    let mut layout = AgentActivityLayout {
        popup,
        title: Rect::new(inner.x, inner.y, inner.width, 1),
        ..AgentActivityLayout::default()
    };
    if inner.width < 20 || inner.height < 4 {
        return Some(layout);
    }
    let footer_height = u16::from(inner.height >= 6);
    if footer_height > 0 {
        layout.footer = Rect::new(inner.x, inner.bottom() - 1, inner.width, 1);
    }
    let body = Rect::new(
        inner.x,
        inner.y + 1,
        inner.width,
        inner.height - 1 - footer_height,
    );
    layout.body = body;
    layout.narrow = inner.width < NARROW_WIDTH;
    let (tree, content) = if layout.narrow {
        match overlay.focus {
            AgentActivityFocus::Tree => (body, Rect::default()),
            AgentActivityFocus::Content => (Rect::default(), body),
        }
    } else {
        let tree_width = (inner.width * TREE_SHARE_PERCENT / 100)
            .max(TREE_MIN_WIDTH)
            .min(inner.width.saturating_sub(21));
        layout.divider = Rect::new(body.x + tree_width, body.y, 1, body.height);
        (
            Rect::new(body.x, body.y, tree_width, body.height),
            Rect::new(
                body.x + tree_width + 1,
                body.y,
                body.width - tree_width - 1,
                body.height,
            ),
        )
    };
    layout.tree = tree;
    if !tree.is_empty() {
        layout.tree_rows = Rect::new(tree.x, tree.y + 1, tree.width, tree.height - 1);
    }
    layout.content = content;
    if !content.is_empty() {
        let header = content_header_rows(overlay).min(content.height.saturating_sub(1));
        layout.content_header = Rect::new(content.x, content.y, content.width, header);
        let text_height = content.height - header;
        layout.content_text = Rect::new(
            content.x + 1,
            content.y + header,
            content.width.saturating_sub(2),
            text_height,
        );
        layout.scrollbar = Rect::new(content.right() - 1, content.y + header, 1, text_height);
    }
    Some(layout)
}

/// 渲染：标题栏、左列活动树、右列内容、页脚提示。只读状态，不写任何滚动状态
/// （`test_overlay_renderers_never_write_scroll_state`）。
pub(super) fn render_agent_activity_overlay(
    b: &mut Buffer,
    overlay: &ClientAgentActivityOverlay,
    endpoints: &[ClientShellEndpoint],
    cx: &super::feedback::ChromeContext<'_>,
) -> Option<OverlayRender> {
    let layout = agent_activity_layout(b.area, cx.page_bounds, overlay)?;
    panel(
        b,
        layout.popup,
        cx.palette.accent,
        cx.palette.panel_bg,
        cx.glyphs,
    )?;
    let actions = render_title_bar(b, layout.title, overlay, endpoints, cx);
    let mut render = OverlayRender {
        area: layout.popup,
        agent_activity_popup: layout.popup,
        agent_activity_actions: actions,
        ..OverlayRender::default()
    };
    if layout.tree.is_empty() && layout.content.is_empty() {
        return Some(render);
    }
    if overlay.unsupported {
        crate::ui::kit::empty_state::render_empty_state(
            b,
            layout.body,
            &crate::ui::kit::empty_state::EmptyState {
                glyph: Some("⊘"),
                title: crate::i18n::texts().agent_activity.unsupported,
                body: None,
                action: None,
            },
            cx.palette,
        );
    } else {
        if !layout.tree.is_empty() {
            render.agent_activity_tree_rows = render_tree_column(b, &layout, overlay, cx);
        }
        if !layout.divider.is_empty() {
            let style = Style::default().fg(cx.palette.surface1);
            for y in layout.divider.y..layout.divider.bottom() {
                put_text(b, layout.divider.x, y, 1, cx.glyphs.vertical, style);
            }
        }
        if !layout.content.is_empty() {
            render.agent_activity_content = layout.content;
            let (scrollbar, metrics) = render_content_column(b, &layout, overlay, cx);
            render.agent_activity_scrollbar = scrollbar;
            render.agent_activity_scroll_metrics = metrics;
        }
    }
    if !layout.footer.is_empty() {
        render_footer(b, layout.footer, overlay, layout.narrow, cx);
    }
    Some(render)
}

/// 标题栏里标题至少保留的列数（含前导空格与省略号）。
const TITLE_MIN_WIDTH: u16 = 12;

/// 标题栏：左侧标题与徽标，右侧 [跟随 ●/○] [刷新] [×]。按钮从右往左放，放不下
/// 的（先丢跟随、再丢刷新）不画也不登记命中区。标题按徽标留出的宽度截断，徽标
/// 从不被长标题挤掉。
fn render_title_bar(
    b: &mut Buffer,
    title_row: Rect,
    overlay: &ClientAgentActivityOverlay,
    endpoints: &[ClientShellEndpoint],
    cx: &super::feedback::ChromeContext<'_>,
) -> Vec<(Rect, AgentActivityButton)> {
    let p = cx.palette;
    let texts = &crate::i18n::texts().agent_activity;
    let follow_label = format!(
        " {} {} ",
        texts.follow_button,
        if overlay.follow_active() {
            "●"
        } else {
            "○"
        }
    );
    let refresh_label = format!(" {} ", texts.refresh_button);
    let buttons = [
        (AgentActivityButton::Close, " × "),
        (AgentActivityButton::Refresh, refresh_label.as_str()),
        (AgentActivityButton::ToggleFollow, follow_label.as_str()),
    ];
    let mut hits = Vec::new();
    let mut right = title_row.right();
    // 标题至少留 TITLE_MIN_WIDTH 列，放不下的按钮整个不画。
    let min_left = title_row.x.saturating_add(TITLE_MIN_WIDTH);
    for (button, label) in buttons {
        let width = display_width(label);
        let Some(x) = right.checked_sub(width) else {
            break;
        };
        if x < min_left {
            break;
        }
        let enabled = button != AgentActivityButton::ToggleFollow || !overlay.is_external();
        let base = match button {
            AgentActivityButton::ToggleFollow if overlay.follow_active() => {
                crate::ui::ModalButtonState::Focused
            }
            _ => crate::ui::ModalButtonState::Normal,
        };
        let state = if enabled {
            cx.button_state(
                &super::feedback::ChromeHover::AgentActivityButton(button),
                base,
            )
        } else {
            crate::ui::ModalButtonState::Disabled
        };
        let rect = Rect::new(x, title_row.y, width, 1);
        let style = crate::ui::modal_button_style(p, crate::ui::ModalButtonTone::Secondary, state);
        b.set_style(rect, style);
        put_text(b, rect.x, rect.y, rect.width, label, style);
        if enabled {
            hits.push((rect, button));
        }
        right = x.saturating_sub(1);
    }
    let base = Style::default().bg(p.panel_bg);
    let title = format!(
        " {}",
        crate::i18n::fill(
            texts.title_fmt,
            &[("name", &owner_label(overlay, endpoints))]
        )
    );
    let limit = right.saturating_sub(1);
    let mut x = title_row.x;
    let room = limit.saturating_sub(x);
    // 徽标紧跟标题，按显示顺序：只读（说明窗口性质，最后丢）、有更新（先丢）。
    let mut badges = [
        overlay
            .is_external()
            .then(|| format!(" {} ", texts.external_read_only)),
        overlay
            .has_updates
            .then(|| format!(" {} ", texts.updates_badge)),
    ];
    let styles = [
        base.fg(p.subtext0).bg(p.surface0),
        base.fg(p.yellow).add_modifier(Modifier::BOLD),
    ];
    // 每个徽标前留一列间隔。
    let badges_width = |badges: &[Option<String>; 2]| {
        badges
            .iter()
            .flatten()
            .map(|label| display_width(label).saturating_add(1))
            .fold(0u16, u16::saturating_add)
    };
    // 标题先截到给徽标留出的宽度（至少 TITLE_MIN_WIDTH 列，带省略号）；实在放不下
    // 时从后往前整个丢徽标。
    let title_min = display_width(&title).min(TITLE_MIN_WIDTH);
    for index in (0..badges.len()).rev() {
        if title_min.saturating_add(badges_width(&badges)) <= room {
            break;
        }
        badges[index] = None;
    }
    let title_room = room.saturating_sub(badges_width(&badges));
    let title = crate::ui::truncate_end(&title, usize::from(title_room));
    let width = display_width(&title).min(title_room);
    put_text(
        b,
        x,
        title_row.y,
        width,
        &title,
        base.fg(p.text).add_modifier(Modifier::BOLD),
    );
    x = x.saturating_add(width);
    for (badge, style) in badges.iter().zip(styles) {
        let Some(label) = badge else {
            continue;
        };
        let width = display_width(label);
        put_text(b, x + 1, title_row.y, width, label, style);
        x = x + 1 + width;
    }
    hits
}

/// 左列：表头行 + 活动树（`kit::tree` 前缀 + 状态字形 + 标签 + 种类 + 耗时）。
fn render_tree_column(
    b: &mut Buffer,
    layout: &AgentActivityLayout,
    overlay: &ClientAgentActivityOverlay,
    cx: &super::feedback::ChromeContext<'_>,
) -> Vec<(Rect, String)> {
    let p = cx.palette;
    let texts = &crate::i18n::texts().agent_activity;
    let tree = layout.tree;
    let focused = overlay.focus == AgentActivityFocus::Tree;
    let header_style = Style::default()
        .fg(if focused { p.accent } else { p.subtext0 })
        .add_modifier(Modifier::BOLD);
    match overlay.tree_error.as_deref() {
        Some(error) if overlay.tree_loaded => {
            let text = crate::i18n::fill(texts.read_failed_fmt, &[("error", error)]);
            put_text(
                b,
                tree.x + TREE_PAD,
                tree.y,
                tree.width.saturating_sub(TREE_PAD),
                &text,
                Style::default().fg(p.red),
            );
        }
        _ => put_text(
            b,
            tree.x + TREE_PAD,
            tree.y,
            tree.width.saturating_sub(TREE_PAD),
            texts.tree_title,
            header_style,
        ),
    }
    let rows_area = layout.tree_rows;
    let mut hits = Vec::new();
    if rows_area.is_empty() {
        return hits;
    }
    if overlay.rows.is_empty() {
        let (line, style) = match (&overlay.tree_error, overlay.tree_loaded) {
            (Some(error), _) => (
                crate::i18n::fill(texts.read_failed_fmt, &[("error", error)]),
                Style::default().fg(p.red),
            ),
            (None, false) => (texts.loading.to_owned(), Style::default().fg(p.overlay1)),
            (None, true) => (texts.empty_tree.to_owned(), Style::default().fg(p.overlay1)),
        };
        put_text(
            b,
            rows_area.x + TREE_PAD,
            rows_area.y,
            rows_area.width.saturating_sub(TREE_PAD),
            &line,
            style,
        );
        return hits;
    }
    let visible = usize::from(rows_area.height);
    let start = overlay
        .tree_scroll
        .min(overlay.rows.len().saturating_sub(visible));
    for (offset, entry) in overlay.rows.iter().skip(start).take(visible).enumerate() {
        let y = rows_area.y + u16::try_from(offset).unwrap_or(u16::MAX);
        let row = Rect::new(rows_area.x, y, rows_area.width, 1);
        let node = &overlay.nodes[entry.kind];
        let selected = overlay.selected_node.as_deref() == Some(node.id.as_str());
        let hovered = overlay.hovered_node.as_deref() == Some(node.id.as_str());
        render_tree_row(b, row, entry, node, selected, hovered, overlay, cx);
        hits.push((row, node.id.clone()));
    }
    hits
}

#[allow(clippy::too_many_arguments)] // 一行的全部呈现输入；拆结构体只为过 lint 反而难读
fn render_tree_row(
    b: &mut Buffer,
    row: Rect,
    entry: &TreeEntry<usize>,
    node: &AgentActivityNode,
    selected: bool,
    hovered: bool,
    overlay: &ClientAgentActivityOverlay,
    cx: &super::feedback::ChromeContext<'_>,
) {
    let p = cx.palette;
    let focused = overlay.focus == AgentActivityFocus::Tree;
    // 选中且树有焦点：accent 反色；选中但焦点在内容：浅底；悬浮：hover 底。
    let (fill, strong) = if selected && focused {
        (Some(p.accent), Some(panel_contrast_fg(p)))
    } else if selected {
        (Some(p.surface0), None)
    } else if hovered {
        (Some(p.hover_row_bg()), None)
    } else {
        (None, None)
    };
    if let Some(fill) = fill {
        b.set_style(row, Style::default().bg(fill));
    }
    let fg = |color: ratatui::style::Color| Style::default().fg(strong.unwrap_or(color));
    let x = row.x + TREE_PAD;
    let right = row.right().saturating_sub(1);
    let (used, toggle) = render_tree_prefix(
        b,
        x,
        row.y,
        right.saturating_sub(x),
        entry.depth,
        entry.last_child_mask,
        entry.has_children,
        entry.collapsed,
        false,
        fg(p.overlay0),
    );
    if !toggle.is_empty() && !selected && hovered {
        b.set_style(toggle, Style::default().fg(p.accent));
    }
    let mut x = x.saturating_add(used);
    let glyph = status_glyph(node.status);
    let color = status_color(node.status, p);
    x = put_segment_until(b, x, row.y, right, glyph, fg(color));
    x = put_segment_until(b, x, row.y, right, " ", fg(p.text));
    let elapsed = node_elapsed_ms(node, overlay.now_ms).map(format_elapsed);
    let kind = kind_label(node.kind);
    let elapsed_width = elapsed.as_deref().map_or(0, |text| display_width(text) + 1);
    let label_width = display_width(&node.label);
    let kind_width = display_width(kind) + 1;
    let room = right.saturating_sub(x);
    // 宽度预算：标签优先（至少留 8 列），其次右对齐的耗时，种类最先让出。
    let show_elapsed = label_width.min(8) + kind_width + elapsed_width <= room
        || label_width.min(8) + elapsed_width <= room;
    let show_kind = label_width + kind_width + if show_elapsed { elapsed_width } else { 0 } <= room;
    let label_limit = room
        .saturating_sub(if show_kind { kind_width } else { 0 })
        .saturating_sub(if show_elapsed { elapsed_width } else { 0 });
    let label_style = match node.status {
        AgentActivityStatus::Running => fg(color).add_modifier(Modifier::BOLD),
        AgentActivityStatus::Failed | AgentActivityStatus::Done => fg(color),
        _ => fg(p.text),
    };
    let label = crate::ui::truncate_end(&node.label, usize::from(label_limit));
    x = put_segment_until(
        b,
        x,
        row.y,
        x.saturating_add(label_limit),
        &label,
        label_style,
    );
    if show_kind {
        x = put_segment_until(b, x, row.y, right, " ", fg(p.text));
        put_segment_until(b, x, row.y, right, kind, fg(p.overlay1));
    }
    if show_elapsed {
        if let Some(elapsed) = elapsed.as_deref() {
            let width = display_width(elapsed);
            put_text(
                b,
                right.saturating_sub(width),
                row.y,
                width,
                elapsed,
                fg(p.overlay0),
            );
        }
    }
}

/// 从 `x` 写到 `right` 为止，返回写完后的列。
fn put_segment_until(b: &mut Buffer, x: u16, y: u16, right: u16, text: &str, style: Style) -> u16 {
    let width = display_width(text).min(right.saturating_sub(x));
    put_text(b, x, y, width, text, style);
    x.saturating_add(width)
}

/// 右列头行里标签至少保留的列数（含省略号）；元信息只在这之外的宽度里取舍。
const HEADER_LABEL_MIN_WIDTH: u16 = 8;

/// 右列头行的宽度预算：`tokens` 是按显示顺序的 [种类, 状态, 耗时]，返回标签可用
/// 的列数与要画的元信息（`  种类 · 状态 · 耗时` 的子集）。元信息按整段取舍、从不
/// 画半截；优先级从高到低：标签的前 [`HEADER_LABEL_MIN_WIDTH`] 列、耗时、种类、
/// 标签其余部分、状态（状态字形已经表达了它）。
fn header_meta_budget(label: &str, room: u16, tokens: [Option<&str>; 3]) -> (u16, String) {
    const KIND: usize = 0;
    const STATUS: usize = 1;
    const ELAPSED: usize = 2;
    let mut keep = tokens.map(|token| token.is_some_and(|token| !token.is_empty()));
    let meta_width = |keep: &[bool; 3]| {
        let (count, width) = tokens.iter().zip(keep).filter(|(_, kept)| **kept).fold(
            (0u16, 0u16),
            |(count, width), (token, _)| {
                (
                    count + 1,
                    width.saturating_add(display_width(token.unwrap_or_default())),
                )
            },
        );
        if count == 0 {
            0
        } else {
            // 前导两空格 + 各段 + 段间「 · 」。
            width.saturating_add(2 + 3 * (count - 1))
        }
    };
    let label_width = display_width(label);
    // 状态排在标签其余部分之后：整段标签连同全部元信息放不下时先让出状态。
    if label_width.saturating_add(meta_width(&keep)) > room {
        keep[STATUS] = false;
    }
    // 再保标签前几列：放不下时依次整段丢种类、耗时。
    let label_min = label_width.min(HEADER_LABEL_MIN_WIDTH);
    for drop in [KIND, ELAPSED] {
        if label_min.saturating_add(meta_width(&keep)) <= room {
            break;
        }
        keep[drop] = false;
    }
    let label_limit = room.saturating_sub(meta_width(&keep));
    let mut meta = String::new();
    for token in tokens
        .iter()
        .zip(keep)
        .filter_map(|(token, kept)| kept.then_some(*token).flatten())
    {
        meta.push_str(if meta.is_empty() { "  " } else { " · " });
        meta.push_str(token);
    }
    (label_limit, meta)
}

/// 右列：头行（节点标签 · 种类 · 状态 · 耗时 / 摘要 / 提示）+ 正文 + 滚动条。
fn render_content_column(
    b: &mut Buffer,
    layout: &AgentActivityLayout,
    overlay: &ClientAgentActivityOverlay,
    cx: &super::feedback::ChromeContext<'_>,
) -> (Rect, Option<crate::pane::ScrollMetrics>) {
    let p = cx.palette;
    let texts = &crate::i18n::texts().agent_activity;
    let header = layout.content_header;
    let focused = overlay.focus == AgentActivityFocus::Content;
    let node = overlay
        .selected_node
        .as_deref()
        .and_then(|id| overlay.node(id));
    let x = header.x + 1;
    let right = header.right().saturating_sub(1);
    let mut y = header.y;
    if header.height > 0 {
        // 右侧：跟随状态（只对 pane 属主）。
        let mut limit = right;
        if !overlay.is_external() && overlay.selected_node.is_some() {
            let (status, style) = if overlay.follow_active() {
                (
                    format!("● {}", texts.follow_on),
                    Style::default().fg(p.accent),
                )
            } else {
                (
                    format!("○ {}", texts.follow_off),
                    Style::default().fg(p.overlay0),
                )
            };
            let width = display_width(&status);
            if width + 12 < right.saturating_sub(x) {
                put_text(b, right - width, y, width, &status, style);
                limit = right - width - 1;
            }
        }
        let title_style = Style::default()
            .fg(if focused { p.accent } else { p.subtext0 })
            .add_modifier(Modifier::BOLD);
        match (node, overlay.selected_node.as_deref()) {
            (Some(node), _) => {
                let mut cursor = put_segment_until(
                    b,
                    x,
                    y,
                    limit,
                    status_glyph(node.status),
                    Style::default().fg(status_color(node.status, p)),
                );
                cursor = put_segment_until(b, cursor, y, limit, " ", title_style);
                let elapsed = node_elapsed_ms(node, overlay.now_ms).map(format_elapsed);
                let (label_limit, meta) = header_meta_budget(
                    &node.label,
                    limit.saturating_sub(cursor),
                    [
                        Some(kind_label(node.kind)),
                        Some(status_label(node.status)),
                        elapsed.as_deref(),
                    ],
                );
                let label = crate::ui::truncate_end(&node.label, usize::from(label_limit));
                cursor = put_segment_until(
                    b,
                    cursor,
                    y,
                    cursor.saturating_add(label_limit),
                    &label,
                    title_style,
                );
                if !meta.is_empty() {
                    put_segment_until(b, cursor, y, limit, &meta, Style::default().fg(p.overlay1));
                }
            }
            (None, Some(id)) => {
                let id = crate::ui::truncate_end(id, usize::from(limit.saturating_sub(x)));
                put_segment_until(b, x, y, limit, &id, title_style);
            }
            (None, None) => {
                put_segment_until(b, x, y, limit, texts.content_title, title_style);
            }
        }
        y += 1;
    }
    if y < header.bottom() {
        if let Some(node) = node.filter(|node| node.summary.is_some() || node.agent_type.is_some())
        {
            let detail = match (node.agent_type.as_deref(), node.summary.as_deref()) {
                (Some(agent_type), Some(summary)) => format!("{agent_type} — {summary}"),
                (Some(agent_type), None) => agent_type.to_owned(),
                (None, Some(summary)) => summary.to_owned(),
                (None, None) => String::new(),
            };
            let detail = sanitize(&detail);
            let detail = crate::ui::truncate_end(&detail, usize::from(right.saturating_sub(x)));
            put_segment_until(b, x, y, right, &detail, Style::default().fg(p.overlay1));
            y += 1;
        }
    }
    if y < header.bottom() {
        let notice = match (&overlay.error, overlay.content.as_ref()) {
            (Some(error), Some(content)) if !content.lines.is_empty() => Some((
                crate::i18n::fill(texts.read_failed_fmt, &[("error", error)]),
                Style::default().fg(p.red),
            )),
            (_, Some(content)) if content.truncated => Some((
                format!("↑ {}", texts.truncated),
                Style::default().fg(p.yellow),
            )),
            _ => None,
        };
        if let Some((notice, style)) = notice {
            let notice = crate::ui::truncate_end(&notice, usize::from(right.saturating_sub(x)));
            put_segment_until(b, x, y, right, &notice, style);
        }
    }

    let text = layout.content_text;
    if text.is_empty() {
        return (Rect::default(), None);
    }
    let placeholder = |b: &mut Buffer, line: &str, style: Style| {
        put_text(b, text.x, text.y, text.width, line, style);
    };
    let Some(selected) = overlay.selected_node.as_deref() else {
        crate::ui::kit::empty_state::render_empty_state(
            b,
            text,
            &crate::ui::kit::empty_state::EmptyState {
                glyph: None,
                title: texts.empty_content,
                body: None,
                action: None,
            },
            p,
        );
        return (Rect::default(), None);
    };
    let Some(content) = overlay
        .content
        .as_ref()
        .filter(|content| content.node_id == selected)
    else {
        match overlay.error.as_deref() {
            Some(error) => placeholder(
                b,
                &crate::i18n::fill(texts.read_failed_fmt, &[("error", error)]),
                Style::default().fg(p.red),
            ),
            None => placeholder(b, texts.loading, Style::default().fg(p.overlay1)),
        }
        return (Rect::default(), None);
    };
    if content.lines.is_empty() {
        let line = if content.eof {
            texts.no_output
        } else {
            texts.loading
        };
        placeholder(b, line, Style::default().fg(p.overlay1));
        return (Rect::default(), None);
    }
    let visible = usize::from(text.height);
    let max_scroll = content.rows.len().saturating_sub(visible);
    let scroll = overlay.content_scroll.min(max_scroll);
    for (offset, row) in content.rows.iter().skip(scroll).take(visible).enumerate() {
        let y = text.y + u16::try_from(offset).unwrap_or(u16::MAX);
        let Some(line) = content.lines.get(row.line as usize) else {
            continue;
        };
        let (start, end) = (row.start as usize, row.end as usize);
        let Some(slice) = line.text.get(start..end) else {
            continue;
        };
        let base = match line.tone {
            LineTone::Heading => Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
            _ => Style::default().fg(p.text),
        };
        put_text(b, text.x, y, text.width, slice, base);
        if matches!(line.tone, LineTone::ListItem | LineTone::Record) && start < line.marker {
            let marker_end = line.marker.min(end);
            if let Some(marker) = line.text.get(start..marker_end) {
                let style = match line.tone {
                    LineTone::Record => Style::default().fg(p.blue).add_modifier(Modifier::BOLD),
                    _ => Style::default().fg(p.accent),
                };
                put_text(b, text.x, y, text.width, marker, style);
            }
        }
    }
    let metrics = crate::pane::ScrollMetrics {
        offset_from_bottom: max_scroll.saturating_sub(scroll),
        max_offset_from_bottom: max_scroll,
        viewport_rows: visible,
    };
    let track = layout.scrollbar;
    if max_scroll > 0 && !track.is_empty() {
        crate::ui::render_scrollbar_buffer(b, metrics, track, p.surface1, p.overlay1, "▐");
        (track, Some(metrics))
    } else {
        (Rect::default(), Some(metrics))
    }
}

fn render_footer(
    b: &mut Buffer,
    footer: Rect,
    overlay: &ClientAgentActivityOverlay,
    narrow: bool,
    cx: &super::feedback::ChromeContext<'_>,
) {
    let texts = &crate::i18n::texts().agent_activity;
    let hints = [
        FooterHint {
            key: "↑↓",
            label: texts.hint_select,
            enabled: !overlay.unsupported,
            primary: true,
        },
        FooterHint {
            key: "Tab",
            label: texts.hint_switch_column,
            enabled: !overlay.unsupported,
            primary: narrow,
        },
        FooterHint {
            key: "f",
            label: texts.hint_follow,
            enabled: !overlay.is_external() && !overlay.unsupported,
            primary: false,
        },
        FooterHint {
            key: "r",
            label: texts.hint_refresh,
            enabled: !overlay.unsupported,
            primary: false,
        },
        FooterHint {
            key: "←→",
            label: texts.hint_collapse,
            enabled: !overlay.unsupported,
            primary: false,
        },
        FooterHint {
            key: "esc",
            label: texts.hint_close,
            enabled: true,
            primary: true,
        },
    ];
    let area = Rect::new(
        footer.x + 1,
        footer.y,
        footer.width.saturating_sub(2),
        footer.height,
    );
    render_footer_hints(b, area, &hints, None, cx.palette);
}

impl ClientShellState {
    fn agent_activity(&self) -> Option<&ClientAgentActivityOverlay> {
        match self.overlay.as_ref() {
            Some(ClientShellOverlay::AgentActivity(overlay)) => Some(overlay),
            _ => None,
        }
    }

    fn agent_activity_mut(&mut self) -> Option<&mut ClientAgentActivityOverlay> {
        match self.overlay.as_mut() {
            Some(ClientShellOverlay::AgentActivity(overlay)) => Some(overlay),
            _ => None,
        }
    }

    /// 打开某个属主的「Agent 活动」窗口并立即读树；已有浮层时记为返回目标
    /// （右键菜单不记；从另一个活动窗口打开时沿用它的返回目标，不叠层）。
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
        let mut overlay = ClientAgentActivityOverlay::new(endpoint_id, owner);
        overlay.next_follow_at = Some(Instant::now() + FOLLOW_INTERVAL);
        if !supported {
            overlay.mark_unsupported();
        }
        overlay.return_to = match self.overlay.take() {
            None | Some(ClientShellOverlay::ContextMenu(_)) => None,
            Some(ClientShellOverlay::AgentActivity(mut previous)) => previous.return_to.take(),
            Some(previous) => Some(Box::new(previous)),
        };
        self.overlay = Some(ClientShellOverlay::AgentActivity(overlay));
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
    /// 轮不到。窗口没开时什么也不做。
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

    /// 视图计算阶段：墙钟、「有更新」、树滚动（含一次性 reveal）、内容折行与
    /// 滚动上界。渲染只读这里写下的结果（STATE-04）。
    pub(super) fn compute_agent_activity_view(&mut self, area: Rect, page_bounds: Option<Rect>) {
        let Some(summary) = self
            .agent_activity()
            .map(|overlay| owner_summary(&self.endpoints, overlay))
        else {
            return;
        };
        let Some(overlay) = self.agent_activity_mut() else {
            return;
        };
        overlay.now_ms = unix_now_ms();
        overlay.observe_summary(summary);
        let Some(layout) = agent_activity_layout(area, page_bounds, overlay) else {
            return;
        };
        let visible = usize::from(layout.tree_rows.height);
        if visible > 0 {
            let selected = overlay.selected_row();
            overlay.tree_scroll = page::list_start(
                overlay.tree_scroll,
                selected.unwrap_or(0),
                overlay.rows.len(),
                visible,
                overlay.tree_reveal && selected.is_some(),
            );
            overlay.tree_reveal = false;
            overlay.tree_page = visible;
        }
        let text = layout.content_text;
        let height = usize::from(text.height);
        if text.width > 0 && height > 0 {
            let rows = overlay.content.as_mut().map_or(0, |content| {
                content.ensure_wrapped(text.width);
                content.row_count()
            });
            let max = rows.saturating_sub(height);
            overlay.content_max_scroll = max;
            overlay.content_scroll = if overlay.content_tail {
                max
            } else {
                overlay.content_scroll.min(max)
            };
            overlay.content_page = height;
        }
    }

    /// 键盘：↑↓ 选节点（焦点在内容时滚内容）、←→ 折叠 / 展开、Enter 选中并拉内容、
    /// Tab 切列、PgUp / PgDn / Home / End 按焦点滚动、f 跟随、r 刷新、Esc 关闭并
    /// 回到 `return_to`。其余按键吞掉。
    pub(super) fn route_agent_activity_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) -> bool {
        if self.agent_activity().is_none() {
            return false;
        }
        let (code, modifiers) = crate::config::normalize_key_combo((key.code, key.modifiers));
        let plain = modifiers.is_empty();
        match code {
            KeyCode::Esc => {
                self.close_agent_activity();
                outcome.repaint = true;
            }
            KeyCode::Tab | KeyCode::BackTab => self.toggle_agent_activity_focus(outcome),
            KeyCode::Char('f') if plain => {
                self.activate_agent_activity_button(AgentActivityButton::ToggleFollow, outcome);
            }
            KeyCode::Char('r') if plain => {
                self.activate_agent_activity_button(AgentActivityButton::Refresh, outcome);
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

    fn toggle_agent_activity_focus(&mut self, outcome: &mut ClientShellInput) {
        if let Some(overlay) = self.agent_activity_mut() {
            overlay.focus = match overlay.focus {
                AgentActivityFocus::Tree => AgentActivityFocus::Content,
                AgentActivityFocus::Content => AgentActivityFocus::Tree,
            };
            overlay.tree_reveal = true;
            outcome.repaint = true;
        }
    }

    /// 标题栏按钮与对应快捷键的共同入口。
    pub(super) fn activate_agent_activity_button(
        &mut self,
        button: AgentActivityButton,
        outcome: &mut ClientShellInput,
    ) {
        match button {
            AgentActivityButton::Close => {
                self.close_agent_activity();
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

    /// ←：展开的节点先折叠，否则跳到父节点；焦点在内容时回到树。
    fn collapse_agent_activity(&mut self, outcome: &mut ClientShellInput) {
        let Some(overlay) = self.agent_activity_mut() else {
            return;
        };
        if overlay.focus == AgentActivityFocus::Content {
            overlay.focus = AgentActivityFocus::Tree;
            overlay.tree_reveal = true;
            outcome.repaint = true;
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

    /// Enter：树里没有选中时选第一行；有选中时从头重读它的内容并把焦点交给内容。
    fn enter_agent_activity(&mut self, outcome: &mut ClientShellInput) {
        let Some(overlay) = self.agent_activity_mut() else {
            return;
        };
        if overlay.focus != AgentActivityFocus::Tree {
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
                let page = overlay.tree_page.max(1);
                overlay.tree_scroll = if down {
                    overlay.tree_scroll.saturating_add(page)
                } else {
                    overlay.tree_scroll.saturating_sub(page)
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

    /// 鼠标：点树行选中、点开关折叠、点按钮、点内容区把焦点交给内容、点滚动条
    /// 跳转、窗外点击关闭；滚轮按指针所在的列滚动；悬浮节点只由 `Moved` 改写
    /// （MENU-01）。窗口开着时吞掉全部鼠标事件。
    pub(super) fn handle_agent_activity_mouse(
        &mut self,
        mouse: MouseEvent,
        point: (u16, u16),
        outcome: &mut ClientShellInput,
    ) -> bool {
        if self.agent_activity().is_none() {
            return false;
        }
        let popup = self.hits.agent_activity_popup;
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
                if !super::contains(popup, point) {
                    self.close_agent_activity();
                    outcome.repaint = true;
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
                        let span = usize::from(track.height.saturating_sub(1)).max(1);
                        let offset = usize::from(point.1.saturating_sub(track.y));
                        overlay.content_scroll =
                            overlay.content_max_scroll * offset.min(span) / span;
                        overlay.content_tail = overlay.follow_active()
                            && overlay.content_scroll >= overlay.content_max_scroll;
                        overlay.focus = AgentActivityFocus::Content;
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
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                if super::contains(popup, point) =>
            {
                let lines = isize::try_from(self.config.mouse_scroll_lines).unwrap_or(3);
                let delta = if mouse.kind == MouseEventKind::ScrollUp {
                    -lines
                } else {
                    lines
                };
                let over_content = !content.is_empty() && point.0 >= content.x;
                if let Some(overlay) = self.agent_activity_mut() {
                    if over_content {
                        overlay.scroll_content(delta);
                    } else {
                        overlay.tree_scroll = overlay.tree_scroll.saturating_add_signed(delta);
                    }
                    outcome.repaint = true;
                }
            }
            _ => {}
        }
        true
    }

    /// 点树行：落在折叠开关（前缀里本节点那一格起 2 列）上切换折叠，否则选中。
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
        let toggle = overlay
            .rows
            .iter()
            .find(|row| row.key == node_id && row.has_children)
            .map(|row| {
                let x = rect.x + TREE_PAD + u16::from(row.depth.min(MAX_TREE_DEPTH)) * 2;
                (x..x.saturating_add(2)).contains(&point.0)
            })
            .unwrap_or(false);
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
}
