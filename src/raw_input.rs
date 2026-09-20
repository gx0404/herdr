use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

/// Parse raw terminal input bytes into a list of `RawInputEvent`s.
///
/// This directly extracts events without going through a channel, making it
/// suitable for synchronous use.
#[cfg(any(unix, test))]
pub fn parse_raw_input_bytes_sync(data: &[u8]) -> Vec<RawInputEvent> {
    let mut framer = RawInputFramer::default();
    let mut events = framer.push(data);
    events.extend(framer.flush_timeout());
    events
}

use crate::input::{parse_terminal_key_sequence, TerminalKey, TextCommit};
use crate::terminal_theme::{
    parse_default_color_response, parse_palette_color_response, DefaultColorKind, HostAppearance,
    RgbColor,
};

const ESC: u8 = 0x1b;
/// 空闲成帧基线窗口（毫秒）：这么久没有新字节就提交或丢弃缓冲区。孤立 ESC 与
/// 不完整控制序列的更长窗口由客户端按 `[ui]` 配置在此基线之上决定。
pub(crate) const RAW_INPUT_IDLE_FLUSH_TIMEOUT_MS: i32 = 10;
pub(crate) const GHOSTTY_COLOR_SCHEME_DARK_REPORT: &[u8] = b"\x1b[?997;1n";
pub(crate) const GHOSTTY_COLOR_SCHEME_LIGHT_REPORT: &[u8] = b"\x1b[?997;2n";
const BRACKETED_PASTE_START: &[u8] = b"\x1b[200~";
const BRACKETED_PASTE_END: &[u8] = b"\x1b[201~";

/// Returns the UTF-8 payload when `data` is exactly one complete bracketed paste.
pub(crate) fn complete_text_bracketed_paste(data: &[u8]) -> Option<&str> {
    if !data.starts_with(BRACKETED_PASTE_START) {
        return None;
    }
    let end = find_subsequence(data, BRACKETED_PASTE_END)?;
    if end + BRACKETED_PASTE_END.len() != data.len() {
        return None;
    }
    std::str::from_utf8(&data[BRACKETED_PASTE_START.len()..end]).ok()
}

/// Client transport uses this to distinguish recoverable oversized interactive
/// pastes from generic oversized input, which remains a protocol violation.
pub(crate) fn is_complete_text_bracketed_paste(data: &[u8]) -> bool {
    complete_text_bracketed_paste(data).is_some()
}

#[derive(Debug)]
pub enum RawInputEvent {
    Key(TerminalKey),
    Text(TextCommit),
    Paste(String),
    Mouse(MouseEvent),
    OuterFocusGained,
    OuterFocusLost,
    HostDefaultColor {
        kind: DefaultColorKind,
        color: RgbColor,
    },
    HostPaletteColors {
        colors: Vec<(u8, RgbColor)>,
    },
    HostColorSchemeChanged(HostAppearance),
    // The dimensions are only read by the Unix client.
    #[cfg_attr(not(any(unix, test)), allow(dead_code))]
    HostCellSizeReport {
        width_px: u32,
        height_px: u32,
    },
    Unsupported,
}

#[derive(Default)]
pub(crate) struct RawInputFramer {
    byte_framer: RawInputByteFramer,
}

impl RawInputFramer {
    #[cfg(any(windows, test))]
    pub(crate) fn for_host_input() -> Self {
        Self {
            byte_framer: RawInputByteFramer::for_host_input(),
        }
    }

    pub(crate) fn push(&mut self, data: &[u8]) -> Vec<RawInputEvent> {
        Self::events_from_chunks(self.byte_framer.push(data))
    }

    #[cfg(any(windows, test))]
    pub(crate) fn has_pending_input(&self) -> bool {
        self.byte_framer.has_pending_input()
    }

    #[cfg(any(windows, test))]
    pub(crate) fn has_pending_bracketed_paste(&self) -> bool {
        self.byte_framer.has_pending_bracketed_paste()
    }

    #[cfg(any(windows, test))]
    pub(crate) fn has_pending_default_mouse_sequence(&self) -> bool {
        starts_with_incomplete_default_mouse_sequence(&self.byte_framer.buffer)
    }

    pub(crate) fn flush_timeout(&mut self) -> Vec<RawInputEvent> {
        Self::events_from_chunks(self.byte_framer.flush_timeout())
    }

    /// Semantic input ends mouse recovery, unlike another idle interval.
    #[cfg(any(windows, test))]
    pub(crate) fn flush_interrupted(&mut self) -> Vec<RawInputEvent> {
        let mut chunks = self.byte_framer.flush_timeout();
        self.byte_framer.timed_out_mouse_prefix = None;
        chunks.extend(self.byte_framer.drain_available_chunks());
        Self::events_from_chunks(chunks)
    }

    fn events_from_chunks(chunks: Vec<Vec<u8>>) -> Vec<RawInputEvent> {
        chunks
            .into_iter()
            .filter_map(|chunk| {
                if chunk.as_slice() == [ESC] {
                    return Some(RawInputEvent::Key(
                        TerminalKey::new(crossterm::event::KeyCode::Esc, KeyModifiers::empty())
                            .with_vt_bytes(chunk),
                    ));
                }
                extract_one_event(&chunk).map(|(event, _consumed)| {
                    tracing::debug!(raw_bytes = ?chunk, event = ?event, "raw input event parsed");
                    event
                })
            })
            .collect()
    }
}

#[derive(Default)]
pub(crate) struct RawInputByteFramer {
    buffer: Vec<u8>,
    discard_until: Option<ControlStringFamily>,
    discarded_tail_bytes: usize,
    // Keep the discarded prefix separate from continuation bytes awaiting validation.
    timed_out_mouse_prefix: Option<Vec<u8>>,
    // 刚因超时送出的 CSI 头部：随后到达的残余尾巴要被吃掉而不是当作文本。
    flushed_csi_head: Option<FlushedCsiHead>,
    // 裸 `ESC [` 已被多等一个空闲窗口（它是每条 CSI 的引导符，也可能是 Alt+[）。
    held_csi_intro_flush: bool,
    // 尾字节丢弃来自键盘侧的不完整 CSI（而非 herdr 自己发出的主机查询回复）：
    // 必须有时间上界，否则永远到不了的尾巴会吞掉用户随后敲的参数字节。
    input_csi_tail_discard: bool,
    // 上面两种「超时后等尾巴」的武装时刻：超过 MAX_TIMED_OUT_CSI_RECOVERY 即失效。
    // 读线程平时阻塞在 read(2) 上、不产生空闲 flush，所以上界必须是墙钟而非次数。
    timed_out_csi_at: Option<std::time::Instant>,
    host_color_replies_awaited: u16,
    host_cell_size_replies_awaited: u16,
    host_appearance_reply_awaited: bool,
    held_pending_host_reply_esc: bool,
    host_color_scheme_change_tracking: bool,
    host_appearance_query_on_focus: bool,
    split_coalesced_escape: bool,
    // 最近一次解析出鼠标报文的时刻：孤立 ESC 是否值得多等一个短窗口的证据。
    last_mouse_report_at: Option<std::time::Instant>,
}

const HOST_COLOR_QUERY_REPLIES: u16 = 258;
#[cfg(any(unix, test))]
const HOST_CELL_SIZE_QUERY_REPLIES: u16 = 1;
const MAX_ORPHANED_CSI_TAIL_BYTES: usize = 32;

/// 「超时送出头部后等尾巴」的存活上限。被 read(2) 边界切开的尾巴在几微秒到几毫秒内
/// 必到，半秒足够宽松；过了这个窗口就当作用户真正键入的字节，不再吃掉。
const MAX_TIMED_OUT_CSI_RECOVERY: std::time::Duration = std::time::Duration::from_millis(500);

/// 超时送出的 CSI 头部。CSI 在读取边界被切开时，头部已经作为按键送出（孤立 ESC
/// 送 Esc、裸 `ESC [` 送传统 Alt+[），随后到达的尾巴既不能重组也不能当作文本，
/// 只能吃掉（上游 #4356/#4365/#4184）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FlushedCsiHead {
    /// 孤立 ESC：尾巴必须以 `[` 开头，这个引导符就是「不是用户键入的文本」的判据。
    LoneEscape,
    /// 裸 `ESC [`：尾巴直接是参数字节，因此要求至少一个参数字节后才认终止字节，
    /// 否则 Alt+[ 之后用户敲的单个字符会被当成终止字节吃掉。
    CsiIntro,
}

impl FlushedCsiHead {
    /// 与尾巴拼回去用于校验的头部字节。
    fn prefix(self) -> &'static [u8] {
        match self {
            Self::LoneEscape => b"\x1b",
            Self::CsiIntro => b"\x1b[",
        }
    }

    /// 尾巴必须先出现的引导字节（孤立 ESC 的尾巴以 `[` 开头）。
    fn tail_intro(self) -> &'static [u8] {
        match self {
            Self::LoneEscape => b"[",
            Self::CsiIntro => b"",
        }
    }

    /// 认终止字节前至少需要的参数字节数。
    fn min_body_len(self) -> usize {
        match self {
            Self::LoneEscape => 0,
            Self::CsiIntro => 1,
        }
    }
}

impl RawInputByteFramer {
    pub(crate) fn for_host_input() -> Self {
        Self::with_host_input_policy(
            crate::platform::capabilities().preserve_legacy_doubled_escape_input,
        )
    }

    fn with_host_input_policy(preserve_legacy_doubled_escape_input: bool) -> Self {
        Self {
            split_coalesced_escape: !preserve_legacy_doubled_escape_input,
            ..Self::default()
        }
    }

    pub(crate) fn push(&mut self, data: &[u8]) -> Vec<Vec<u8>> {
        self.buffer.extend_from_slice(data);
        self.drain_available_chunks()
    }

    /// Hold a lone trailing ESC for one idle flush so an OSC 10/11 reply split
    /// at its ESC introducer stitches back together instead of leaking (#549).
    pub(crate) fn host_color_query_sent(&mut self) {
        self.host_color_replies_awaited = HOST_COLOR_QUERY_REPLIES;
        self.held_pending_host_reply_esc = false;
    }

    fn host_appearance_query_sent(&mut self) {
        self.host_appearance_reply_awaited = true;
        self.held_pending_host_reply_esc = false;
    }

    /// Same hold window as `host_color_query_sent`, for the XTWINOPS cell size
    /// reply. Only the Unix client sends this query.
    #[cfg(any(unix, test))]
    pub(crate) fn host_cell_size_query_sent(&mut self) {
        self.host_cell_size_replies_awaited = HOST_CELL_SIZE_QUERY_REPLIES;
        self.held_pending_host_reply_esc = false;
    }

    fn awaiting_host_reply(&self) -> bool {
        self.host_color_replies_awaited > 0
            || self.host_cell_size_replies_awaited > 0
            || self.host_appearance_reply_awaited
    }

    #[cfg(any(unix, test))]
    pub(crate) fn enable_host_color_scheme_change_tracking(&mut self) {
        self.host_color_scheme_change_tracking = true;
    }

    /// Arm the bounded host-reply window when focus gain will emit an appearance query.
    /// If the write or reply fails, a lone Escape is delayed for only one extra flush.
    #[cfg(any(not(windows), test))]
    pub(crate) fn enable_host_appearance_query_on_focus(&mut self) {
        self.host_appearance_query_on_focus = true;
    }

    pub(crate) fn has_pending_input(&self) -> bool {
        !self.buffer.is_empty()
    }

    #[cfg(unix)]
    pub(crate) fn has_pending_lone_escape(&self) -> bool {
        self.buffer.as_slice() == [ESC]
    }

    #[cfg(unix)]
    pub(crate) fn has_pending_incomplete_mouse_sequence(&self) -> bool {
        starts_with_incomplete_sgr_mouse_sequence(&self.buffer)
            || starts_with_incomplete_default_mouse_sequence(&self.buffer)
    }

    /// 已见 `ESC [` 且其后至少一个参数/中间字节（0x20..=0x3F）但尚无终止字节：
    /// 不可能是完整按键，只可能是被切断的 kitty 键序列或 SGR 鼠标报文。
    #[cfg(any(unix, test))]
    pub(crate) fn has_pending_incomplete_csi(&self) -> bool {
        starts_with_incomplete_csi(&self.buffer)
    }

    /// 缓冲区恰为裸 `ESC [`：可能是传统终端的 Alt+[，也可能是任一 CSI 的切断头。
    #[cfg(any(unix, test))]
    pub(crate) fn has_pending_csi_intro(&self) -> bool {
        self.buffer.as_slice() == b"\x1b["
    }

    /// 最近 `window` 内是否解析出过鼠标报文。
    #[cfg(any(unix, test))]
    pub(crate) fn mouse_report_seen_within(
        &self,
        now: std::time::Instant,
        window: std::time::Duration,
    ) -> bool {
        self.last_mouse_report_at
            .is_some_and(|at| now.saturating_duration_since(at) <= window)
    }

    #[cfg(any(windows, test))]
    pub(crate) fn has_pending_bracketed_paste(&self) -> bool {
        self.buffer.starts_with(BRACKETED_PASTE_START)
            && find_subsequence(&self.buffer, BRACKETED_PASTE_END).is_none()
    }

    pub(crate) fn flush_timeout(&mut self) -> Vec<Vec<u8>> {
        let mut chunks = self.drain_available_chunks();

        // Idle is not evidence that a mouse report has ended. The continuation
        // stays bounded and is released if it cannot complete a valid report.
        if self.timed_out_mouse_prefix.is_some() {
            return chunks;
        }

        if let Some(family) = self.discard_until {
            if family == ControlStringFamily::HostReplyCsi {
                // 空闲不是尾巴已结束的证据；键盘侧武装的墙钟上界由
                // `expire_timed_out_csi_recovery`（上面的 drain 已执行）负责，主机
                // 回复的武装范围由 herdr 自己发出的查询限定，不设上界。
                return chunks;
            }
            let keep_split_st = self.buffer.last() == Some(&ESC);
            let keep_discarding = plausible_control_string_tail(family, &self.buffer);
            self.discarded_tail_bytes = self.discarded_tail_bytes.saturating_add(self.buffer.len());
            self.buffer.clear();
            if keep_discarding && self.discarded_tail_bytes <= MAX_DISCARDED_CONTROL_TAIL_BYTES {
                if keep_split_st {
                    self.buffer.push(ESC);
                }
            } else {
                self.discard_until = None;
                self.discarded_tail_bytes = 0;
            }
            return chunks;
        }

        if self.buffer.is_empty() {
            return chunks;
        }

        if self.flushed_csi_head == Some(FlushedCsiHead::LoneEscape)
            && self.buffer.starts_with(b"[<")
        {
            tracing::debug!(
                len = self.buffer.len(),
                "discarding incomplete orphaned SGR mouse tail after input timeout"
            );
            let mut prefix = vec![ESC];
            prefix.append(&mut self.buffer);
            self.retain_timed_out_mouse_prefix(prefix);
            self.flushed_csi_head = None;
            return chunks;
        }

        // 孤立 ESC 送出后残留的非鼠标 CSI 尾巴（`[` + 至少一个参数字节）等不到终止
        // 字节：武装带墙钟上界的尾字节丢弃，残余不得在下一次 push 里变成文本。单个
        // `[` 可能就是用户键入的字符，不走这里，交给后面的按键解析。
        if self.flushed_csi_head == Some(FlushedCsiHead::LoneEscape)
            && self.buffer.len() > 1
            && starts_with_incomplete_orphaned_csi_tail(FlushedCsiHead::LoneEscape, &self.buffer)
        {
            tracing::debug!(
                bytes = ?self.buffer,
                "arming tail discard for an orphaned CSI tail after input timeout"
            );
            self.arm_input_csi_tail_discard();
            self.buffer.clear();
            return chunks;
        }

        if starts_with_incomplete_sgr_mouse_sequence(&self.buffer) {
            tracing::debug!(
                bytes = ?self.buffer,
                "discarding incomplete SGR mouse sequence after input timeout"
            );
            let prefix = std::mem::take(&mut self.buffer);
            self.retain_timed_out_mouse_prefix(prefix);
            return chunks;
        }

        if self.buffer.starts_with(BRACKETED_PASTE_START)
            && find_subsequence(&self.buffer, BRACKETED_PASTE_END).is_none()
        {
            tracing::trace!(
                len = self.buffer.len(),
                "waiting for bracketed paste terminator"
            );
            return chunks;
        }

        if starts_with_incomplete_default_color_response(&self.buffer) {
            tracing::trace!(
                len = self.buffer.len(),
                "waiting for host color response terminator"
            );
            return chunks;
        }

        if (self.host_cell_size_replies_awaited > 0 || self.host_appearance_reply_awaited)
            && self.buffer.as_slice() == b"\x1b["
        {
            if !self.held_pending_host_reply_esc {
                self.held_pending_host_reply_esc = true;
                // 这一轮 hold 同时算作裸 `ESC [` 的引导符 hold，避免下面的分支
                // 再多等一个窗口。
                self.held_csi_intro_flush = true;
                tracing::trace!("holding incomplete host CSI reply one flush");
                return chunks;
            }
            self.host_cell_size_replies_awaited = 0;
            self.host_appearance_reply_awaited = false;
            self.held_pending_host_reply_esc = false;
        }

        if self.host_cell_size_replies_awaited > 0
            && starts_with_incomplete_host_cell_size_report(&self.buffer)
        {
            tracing::debug!(
                len = self.buffer.len(),
                "discarding incomplete host cell size report after input timeout"
            );
            self.host_cell_size_replies_awaited = 0;
            self.held_pending_host_reply_esc = false;
            self.discard_until = Some(ControlStringFamily::HostReplyCsi);
            self.discarded_tail_bytes = 0;
            self.buffer.clear();
            return chunks;
        }

        if starts_with_incomplete_host_color_scheme_report(&self.buffer) {
            if self.host_appearance_reply_awaited && !self.held_pending_host_reply_esc {
                self.held_pending_host_reply_esc = true;
                tracing::trace!(
                    len = self.buffer.len(),
                    "holding incomplete host color scheme report one flush"
                );
                return chunks;
            }
            tracing::debug!(
                len = self.buffer.len(),
                "discarding incomplete host color scheme report after input timeout"
            );
            self.host_appearance_reply_awaited = false;
            self.held_pending_host_reply_esc = false;
            self.discard_until = Some(ControlStringFamily::HostReplyCsi);
            self.discarded_tail_bytes = 0;
            self.buffer.clear();
            return chunks;
        }

        if let Some(ControlString::Incomplete { family }) = control_string(&self.buffer) {
            tracing::debug!(
                len = self.buffer.len(),
                "discarding incomplete host control string after input timeout"
            );
            // This intentionally gives host control replies precedence over legacy
            // Alt forms like Alt+] after timeout, so later reply tails cannot leak.
            self.discard_until = Some(family);
            self.discarded_tail_bytes = 0;
            self.buffer.clear();
            return chunks;
        }

        if starts_with_incomplete_csi(&self.buffer) {
            // 被 read(2) 边界切断的 kitty 键序列或 CSI-u 释放事件：前缀已无法成为
            // 完整按键，丢弃时必须连同稍后到达的尾字节（如 `;9u`）一起吃到终止字节，
            // 否则残余会作为文本进入 pane（上游 #4356/#4365/#4184）。裸 `ESC [`
            // 不走这里，保留传统终端 Alt+[ 的语义。
            tracing::debug!(
                bytes = ?self.buffer,
                "discarding incomplete CSI after input timeout; its tail is dropped through the final byte"
            );
            self.arm_input_csi_tail_discard();
            self.buffer.clear();
            return chunks;
        }

        if self.buffer.as_slice() == b"\x1b[" {
            // 裸 `ESC [` 既是传统终端的 Alt+[，也是每条 CSI 的引导符。先多等一个
            // 空闲窗口（等价于把重组窗口翻倍），到期仍无字节才按 Alt+[ 送出，并记下
            // 头部，让随后到达的参数尾巴被吃掉而不是作为文本进入 pane。
            if !self.held_csi_intro_flush {
                self.held_csi_intro_flush = true;
                tracing::trace!("holding a bare CSI intro one extra flush");
                return chunks;
            }
            self.held_csi_intro_flush = false;
            self.flushed_csi_head = Some(FlushedCsiHead::CsiIntro);
            self.timed_out_csi_at = Some(std::time::Instant::now());
            tracing::debug!("flushing a bare CSI intro as legacy alt bracket after input timeout");
            chunks.push(std::mem::take(&mut self.buffer));
            return chunks;
        }

        if self.buffer.as_slice() == [ESC] {
            if self.awaiting_host_reply() && !self.held_pending_host_reply_esc {
                self.held_pending_host_reply_esc = true;
                tracing::trace!("holding lone escape one flush while awaiting host reply");
                return chunks;
            }
            // No continuation arrived; give up the window so Escape is not delayed again.
            self.host_color_replies_awaited = 0;
            self.host_cell_size_replies_awaited = 0;
            self.host_appearance_reply_awaited = false;
            self.held_pending_host_reply_esc = false;
            tracing::warn!(
                bytes = ?self.buffer,
                "flushing lone escape after input timeout; if this follows an alt chord or focus switch it may reach the pane as plain esc"
            );
            self.flushed_csi_head = Some(FlushedCsiHead::LoneEscape);
            self.timed_out_csi_at = Some(std::time::Instant::now());
            self.held_csi_intro_flush = false;
            chunks.push(std::mem::take(&mut self.buffer));
            return chunks;
        }

        if let Ok(text) = std::str::from_utf8(&self.buffer) {
            if parse_terminal_key_sequence(text).is_some() {
                self.flushed_csi_head = None;
                self.held_csi_intro_flush = false;
                chunks.push(std::mem::take(&mut self.buffer));
                return chunks;
            }
        }

        if starts_with_incomplete_utf8_char(&self.buffer) {
            tracing::trace!(bytes = ?self.buffer, "waiting for UTF-8 continuation bytes");
            return chunks;
        }

        if self.buffer.first() == Some(&ESC) && starts_with_incomplete_utf8_char(&self.buffer[1..])
        {
            tracing::trace!(bytes = ?self.buffer, "waiting for escaped UTF-8 continuation bytes");
            return chunks;
        }

        tracing::debug!(bytes = ?self.buffer, "dropping incomplete raw input buffer after timeout");
        self.flushed_csi_head = None;
        self.held_csi_intro_flush = false;
        self.buffer.clear();
        chunks
    }

    /// 武装通用 CSI 尾字节丢弃：吃掉残余参数字节直到终止字节。与主机回复共用
    /// `HostReplyCsi` 的吃法，但额外标记来源是键盘输入，使其受墙钟上界约束。
    fn arm_input_csi_tail_discard(&mut self) {
        self.discard_until = Some(ControlStringFamily::HostReplyCsi);
        self.discarded_tail_bytes = 0;
        self.input_csi_tail_discard = true;
        self.timed_out_csi_at = Some(std::time::Instant::now());
        self.flushed_csi_head = None;
        // 不完整 CSI 不产生事件，`drain_available_chunks` 的复位不会执行；这里一并
        // 放掉 hold 标志，否则下一个真正的孤立 ESC 会跳过本该有的 hold。
        self.held_pending_host_reply_esc = false;
        self.held_csi_intro_flush = false;
    }

    fn disarm_csi_tail_discard(&mut self) {
        self.discard_until = None;
        self.discarded_tail_bytes = 0;
        self.input_csi_tail_discard = false;
        self.timed_out_csi_at = None;
    }

    /// 超时送出的 CSI 头部/尾字节丢弃过期即放手：此后到达的字节按普通输入处理。
    fn expire_timed_out_csi_recovery(&mut self) {
        let expired = self
            .timed_out_csi_at
            .is_none_or(|at| at.elapsed() > MAX_TIMED_OUT_CSI_RECOVERY);
        if !expired {
            return;
        }
        if self.flushed_csi_head.is_some() {
            tracing::debug!("giving up on an orphaned CSI tail after its recovery window");
            self.flushed_csi_head = None;
        }
        if self.input_csi_tail_discard {
            tracing::debug!("disarming the input CSI tail discard after its recovery window");
            self.disarm_csi_tail_discard();
        }
        self.timed_out_csi_at = None;
    }

    /// 把「等尾巴」的武装时刻回拨到过期，供单测验证上界而不真的睡半秒。
    #[cfg(test)]
    fn backdate_timed_out_csi_for_test(&mut self) {
        self.timed_out_csi_at = self.timed_out_csi_at.and_then(|at| {
            at.checked_sub(MAX_TIMED_OUT_CSI_RECOVERY + std::time::Duration::from_millis(1))
        });
    }

    /// 记下「刚解析出鼠标报文」的时刻：孤立 ESC 是否值得多等一个短窗口的证据。
    fn note_mouse_report(&mut self) {
        self.last_mouse_report_at = Some(std::time::Instant::now());
    }

    fn retain_timed_out_mouse_prefix(&mut self, prefix: Vec<u8>) {
        self.timed_out_mouse_prefix = (prefix.len() < MAX_DISCARDED_CONTROL_TAIL_BYTES
            && plausible_sgr_mouse_prefix(&prefix))
        .then_some(prefix);
    }

    fn drain_available_chunks(&mut self) -> Vec<Vec<u8>> {
        let mut chunks = Vec::new();

        if self.timed_out_csi_at.is_some() {
            self.expire_timed_out_csi_recovery();
        }

        loop {
            if let Some(prefix) = &self.timed_out_mouse_prefix {
                match classify_sgr_mouse_continuation(prefix, &self.buffer) {
                    SgrMouseContinuation::Incomplete => break,
                    SgrMouseContinuation::Complete(len) => {
                        self.buffer.drain(..len);
                        // 被切断后重新拼回的报文同样是「最近收到过鼠标报文」的证据。
                        self.note_mouse_report();
                    }
                    SgrMouseContinuation::Invalid => {}
                }
                self.timed_out_mouse_prefix = None;
            }

            if let Some(head) = self.flushed_csi_head {
                if starts_with_incomplete_orphaned_csi_tail(head, &self.buffer) {
                    break;
                }
                if let Some(was_mouse_report) =
                    discard_complete_orphaned_csi_tail(head, &mut self.buffer)
                {
                    if was_mouse_report {
                        self.note_mouse_report();
                    }
                    self.flushed_csi_head = None;
                    continue;
                }
                self.flushed_csi_head = None;
            }

            if let Some(family) = self.discard_until {
                if family == ControlStringFamily::HostReplyCsi {
                    if discard_host_reply_csi_tail(&mut self.buffer, &mut self.discarded_tail_bytes)
                    {
                        self.disarm_csi_tail_discard();
                        continue;
                    }
                    break;
                }
                let Some(terminator_len) =
                    control_string_terminator_for_family(&self.buffer, family)
                else {
                    break;
                };
                self.buffer.drain(..terminator_len);
                self.discard_until = None;
                self.discarded_tail_bytes = 0;
                continue;
            }

            if self.split_coalesced_escape && self.buffer.starts_with(b"\x1b\x1b") {
                chunks.push(vec![ESC]);
                self.buffer.drain(..1);
                continue;
            }

            let Some((event, consumed)) = extract_one_event(&self.buffer) else {
                break;
            };
            if matches!(event, RawInputEvent::Mouse(_)) {
                self.note_mouse_report();
            } else if matches!(
                event,
                RawInputEvent::HostDefaultColor { .. } | RawInputEvent::HostPaletteColors { .. }
            ) {
                self.host_color_replies_awaited = self.host_color_replies_awaited.saturating_sub(1);
            } else if matches!(event, RawInputEvent::HostCellSizeReport { .. }) {
                self.host_cell_size_replies_awaited =
                    self.host_cell_size_replies_awaited.saturating_sub(1);
            } else if self.host_appearance_query_on_focus
                && matches!(event, RawInputEvent::OuterFocusGained)
            {
                self.host_appearance_query_sent();
            } else if matches!(event, RawInputEvent::HostColorSchemeChanged(_)) {
                self.host_appearance_reply_awaited = false;
                if self.host_color_scheme_change_tracking {
                    self.host_color_query_sent();
                }
            }
            self.held_pending_host_reply_esc = false;
            self.held_csi_intro_flush = false;
            chunks.push(self.buffer[..consumed].to_vec());
            self.buffer.drain(..consumed);
        }

        chunks
    }
}

const MAX_DISCARDED_CONTROL_TAIL_BYTES: usize = 128;

fn plausible_control_string_tail(family: ControlStringFamily, buffer: &[u8]) -> bool {
    match family {
        ControlStringFamily::Osc => buffer.iter().all(|byte| {
            byte.is_ascii_digit()
                || matches!(
                    *byte,
                    b';' | b':'
                        | b'/'
                        | b'#'
                        | b'?'
                        | b'.'
                        | b'_'
                        | b'-'
                        | b'+'
                        | b'r'
                        | b'g'
                        | b'b'
                        | b'R'
                        | b'G'
                        | b'B'
                        | ESC
                )
        }),
        ControlStringFamily::StTerminated => buffer.last() == Some(&ESC),
        ControlStringFamily::HostReplyCsi => false,
    }
}

#[cfg(any(unix, test))]
pub(crate) fn events_require_host_surface_redraw(
    events: &[RawInputEvent],
    redraw_on_focus_gained: bool,
) -> bool {
    redraw_on_focus_gained
        && events
            .iter()
            .any(|event| matches!(event, RawInputEvent::OuterFocusGained))
}

#[cfg(any(unix, test))]
pub(crate) fn events_require_host_mode_refresh(events: &[RawInputEvent]) -> bool {
    events
        .iter()
        .any(|event| matches!(event, RawInputEvent::OuterFocusGained))
}

#[cfg(any(not(windows), test))]
pub(crate) fn events_require_host_terminal_appearance_query(events: &[RawInputEvent]) -> bool {
    events
        .iter()
        .any(|event| matches!(event, RawInputEvent::OuterFocusGained))
}

#[cfg(any(not(windows), test))]
pub(crate) fn events_require_host_terminal_theme_query(events: &[RawInputEvent]) -> bool {
    events
        .iter()
        .any(|event| matches!(event, RawInputEvent::HostColorSchemeChanged(_)))
}

fn extract_one_event(buffer: &[u8]) -> Option<(RawInputEvent, usize)> {
    if buffer.is_empty() {
        return None;
    }

    if buffer.starts_with(BRACKETED_PASTE_START) {
        let end = find_subsequence(buffer, BRACKETED_PASTE_END)?;
        let content = std::str::from_utf8(&buffer[BRACKETED_PASTE_START.len()..end]).ok()?;
        return Some((
            RawInputEvent::Paste(content.to_string()),
            end + BRACKETED_PASTE_END.len(),
        ));
    }

    if buffer[0] == ESC {
        let seq_len = complete_escape_sequence_len(buffer)?;
        if buffer[..seq_len].starts_with(b"\x1b[M") {
            let event = parse_default_mouse(&buffer[..seq_len])
                .map(RawInputEvent::Mouse)
                .unwrap_or(RawInputEvent::Unsupported);
            return Some((event, seq_len));
        }
        let seq = std::str::from_utf8(&buffer[..seq_len]).ok()?;

        if let Some((kind, color)) = parse_default_color_response(seq) {
            return Some((RawInputEvent::HostDefaultColor { kind, color }, seq_len));
        }
        if let Some((index, color)) = parse_palette_color_response(seq) {
            return Some((
                RawInputEvent::HostPaletteColors {
                    colors: vec![(index, color)],
                },
                seq_len,
            ));
        }

        match seq {
            "\x1b[I" => return Some((RawInputEvent::OuterFocusGained, seq_len)),
            "\x1b[O" => return Some((RawInputEvent::OuterFocusLost, seq_len)),
            _ => {}
        }

        if let Some(appearance) = parse_host_color_scheme_report(&buffer[..seq_len]) {
            return Some((RawInputEvent::HostColorSchemeChanged(appearance), seq_len));
        }

        if let Some((width_px, height_px)) = parse_host_cell_size_report(&buffer[..seq_len]) {
            return Some((
                RawInputEvent::HostCellSizeReport {
                    width_px,
                    height_px,
                },
                seq_len,
            ));
        }

        if let Some(mouse) = parse_sgr_mouse(seq) {
            return Some((RawInputEvent::Mouse(mouse), seq_len));
        }

        if let Some(key) = parse_terminal_key_sequence(seq) {
            return Some((
                RawInputEvent::Key(key.with_vt_bytes(buffer[..seq_len].to_vec())),
                seq_len,
            ));
        }

        tracing::debug!(sequence = ?seq, "dropping unsupported escape sequence");
        return Some((RawInputEvent::Unsupported, seq_len));
    }

    let consumed = first_complete_utf8_char_len(buffer)?;
    let text = std::str::from_utf8(&buffer[..consumed]).ok()?;
    let key = parse_terminal_key_sequence(text)?
        .with_text_commit()
        .with_vt_bytes(buffer[..consumed].to_vec());
    Some((RawInputEvent::Key(key), consumed))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ControlStringFamily {
    Osc,
    StTerminated,
    HostReplyCsi,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ControlString {
    Complete {
        len: usize,
        family: ControlStringFamily,
    },
    Incomplete {
        family: ControlStringFamily,
    },
}

fn parse_host_color_scheme_report(buffer: &[u8]) -> Option<HostAppearance> {
    match buffer {
        GHOSTTY_COLOR_SCHEME_DARK_REPORT => Some(HostAppearance::Dark),
        GHOSTTY_COLOR_SCHEME_LIGHT_REPORT => Some(HostAppearance::Light),
        _ => None,
    }
}

/// Parses an XTWINOPS cell size report (`CSI 6 ; height ; width t`) into
/// `(width_px, height_px)`; note the reply orders height first.
fn parse_host_cell_size_report(buffer: &[u8]) -> Option<(u32, u32)> {
    let body = buffer.strip_prefix(b"\x1b[")?.strip_suffix(b"t")?;
    let text = std::str::from_utf8(body).ok()?;
    let mut params = text.split(';');
    if params.next()? != "6" {
        return None;
    }
    let height_px = params.next()?.parse::<u32>().ok()?;
    let width_px = params.next()?.parse::<u32>().ok()?;
    if params.next().is_some() || width_px == 0 || height_px == 0 {
        return None;
    }
    Some((width_px, height_px))
}

fn starts_with_incomplete_default_color_response(buffer: &[u8]) -> bool {
    matches!(
        control_string(buffer),
        Some(ControlString::Incomplete {
            family: ControlStringFamily::Osc
        })
    ) && matches!(buffer.get(..5), Some(b"\x1b]10;" | b"\x1b]11;"))
}

fn starts_with_incomplete_host_color_scheme_report(buffer: &[u8]) -> bool {
    buffer.starts_with(b"\x1b[?")
        && (GHOSTTY_COLOR_SCHEME_DARK_REPORT.starts_with(buffer)
            || GHOSTTY_COLOR_SCHEME_LIGHT_REPORT.starts_with(buffer))
        && buffer.len() < GHOSTTY_COLOR_SCHEME_DARK_REPORT.len()
}

fn starts_with_incomplete_host_cell_size_report(buffer: &[u8]) -> bool {
    let Some(body) = buffer.strip_prefix(b"\x1b[") else {
        return false;
    };
    if body.is_empty() || body.last() == Some(&b't') {
        return false;
    }

    let mut params = body.split(|byte| *byte == b';');
    if params.next() != Some(b"6".as_slice()) {
        return false;
    }
    let height = params.next();
    let width = params.next();
    params.next().is_none()
        && height.is_none_or(|value| value.iter().all(u8::is_ascii_digit))
        && width.is_none_or(|value| value.iter().all(u8::is_ascii_digit))
        && !(height.is_some_and(<[u8]>::is_empty) && width.is_some())
}

fn control_string(buffer: &[u8]) -> Option<ControlString> {
    let family = match buffer.get(..2)? {
        b"\x1b]" => ControlStringFamily::Osc,
        b"\x1bP" | b"\x1b_" | b"\x1b^" | b"\x1bX" => ControlStringFamily::StTerminated,
        _ => return None,
    };

    Some(match control_string_terminator_for_family(buffer, family) {
        Some(len) => ControlString::Complete { len, family },
        None => ControlString::Incomplete { family },
    })
}

fn first_complete_utf8_char_len(buffer: &[u8]) -> Option<usize> {
    let width = utf8_char_width(*buffer.first()?)?;

    if buffer.len() < width {
        return None;
    }

    std::str::from_utf8(&buffer[..width]).ok()?;
    Some(width)
}

fn starts_with_incomplete_utf8_char(buffer: &[u8]) -> bool {
    match std::str::from_utf8(buffer) {
        Ok(_) => false,
        Err(err) => err.valid_up_to() == 0 && err.error_len().is_none(),
    }
}

fn utf8_char_width(first: u8) -> Option<usize> {
    if first < 0x80 {
        Some(1)
    } else if first & 0b1110_0000 == 0b1100_0000 {
        Some(2)
    } else if first & 0b1111_0000 == 0b1110_0000 {
        Some(3)
    } else if first & 0b1111_1000 == 0b1111_0000 {
        Some(4)
    } else {
        None
    }
}

fn complete_escape_sequence_len(buffer: &[u8]) -> Option<usize> {
    if buffer.len() == 1 {
        return None;
    }

    if buffer.starts_with(b"\x1b\x1b[<") {
        if let Some(mouse_len) = find_csi_final(&buffer[1..], b"Mm") {
            let mouse_sequence = std::str::from_utf8(&buffer[1..1 + mouse_len]).ok()?;
            if parse_sgr_mouse(mouse_sequence).is_some() {
                return Some(1);
            }
        }
    }

    if buffer.len() >= 7
        && buffer.starts_with(b"\x1b\x1b[M")
        && parse_default_mouse(&buffer[1..7]).is_some()
    {
        return Some(1);
    }

    if buffer.starts_with(b"\x1b\x1b") {
        return complete_escape_sequence_len(&buffer[1..]).map(|len| len + 1);
    }

    if buffer.starts_with(b"\x1b[") {
        if buffer.starts_with(b"\x1b[<") {
            return find_csi_final(buffer, b"Mm");
        }
        if buffer.starts_with(b"\x1b[M") {
            return (buffer.len() >= 6).then_some(6);
        }
        return find_csi_final(
            buffer,
            b"@ABCDEFGHIJKLMNOPQRSTUVWXYZ[\\]^_`abcdefghijklmnopqrstuvwxyz{|}~",
        );
    }

    if let Some(control) = control_string(buffer) {
        return match control {
            ControlString::Complete { len, .. } => Some(len),
            ControlString::Incomplete { .. } => None,
        };
    }

    if buffer.starts_with(b"\x1bO") {
        return (buffer.len() >= 3).then_some(3);
    }

    let escaped_char_width = utf8_char_width(buffer[1])?;
    if buffer.len() < 1 + escaped_char_width {
        return None;
    }
    std::str::from_utf8(&buffer[1..1 + escaped_char_width]).ok()?;
    Some(1 + escaped_char_width)
}

fn starts_with_incomplete_sgr_mouse_sequence(buffer: &[u8]) -> bool {
    buffer.starts_with(b"\x1b[<")
        && buffer[3..]
            .iter()
            .all(|byte| byte.is_ascii_digit() || *byte == b';')
}

/// `ESC [` 之后至少一个字节且全部落在 ECMA-48 参数/中间字节区（0x20..=0x3F），
/// 尚未出现终止字节（0x40..=0x7E）。裸 `ESC [` 不算，它可能是传统 Alt+[。
fn starts_with_incomplete_csi(buffer: &[u8]) -> bool {
    match buffer.strip_prefix(b"\x1b[") {
        Some(body) => !body.is_empty() && body.iter().all(|byte| (0x20..=0x3f).contains(byte)),
        None => false,
    }
}

#[cfg(any(unix, windows, test))]
fn starts_with_incomplete_default_mouse_sequence(buffer: &[u8]) -> bool {
    buffer.starts_with(b"\x1b[M") && buffer.len() < 6
}

/// 缓冲区仍可能长成 `head` 被切断的那条 CSI 的尾巴（尚无终止字节），值得再等。
fn starts_with_incomplete_orphaned_csi_tail(head: FlushedCsiHead, buffer: &[u8]) -> bool {
    if buffer.len() > MAX_ORPHANED_CSI_TAIL_BYTES {
        return false;
    }
    let intro = head.tail_intro();
    if buffer.len() < intro.len() {
        // 孤立 ESC 之后连 `[` 都还没到。
        return intro.starts_with(buffer);
    }
    buffer.starts_with(intro)
        && buffer[intro.len()..]
            .iter()
            .all(|byte| (0x20..=0x3f).contains(byte))
}

/// 若缓冲区开头正是 `head` 被切断的那条 CSI 的完整尾巴（含终止字节），就吃掉它；
/// 返回 `Some(true)` 表示吃掉的是鼠标报文。只认「与头部拼回去确实解析成一个已知
/// 事件」的尾巴，其它一律释放，避免把用户键入的文本吃掉。
fn discard_complete_orphaned_csi_tail(head: FlushedCsiHead, buffer: &mut Vec<u8>) -> Option<bool> {
    let body_start = head.tail_intro().len();
    if !buffer.starts_with(head.tail_intro()) {
        return None;
    }
    let mut index = body_start;
    let terminator_len = loop {
        if index >= MAX_ORPHANED_CSI_TAIL_BYTES {
            return None;
        }
        match *buffer.get(index)? {
            0x20..=0x3f => index += 1,
            0x40..=0x7e if index >= body_start + head.min_body_len() => break index + 1,
            _ => return None,
        }
    };

    let mut sequence = head.prefix().to_vec();
    sequence.extend_from_slice(&buffer[..terminator_len]);
    let (event, consumed) = extract_one_event(&sequence)?;
    if consumed != sequence.len() || matches!(event, RawInputEvent::Unsupported) {
        return None;
    }
    buffer.drain(..terminator_len);
    Some(matches!(event, RawInputEvent::Mouse(_)))
}

/// 吃掉一条已被丢弃前缀的 CSI 尾巴直到终止字节（含），遇到非 CSI 字节（如新的 ESC）
/// 立即释放；既用于超时的主机 CSI 回复，也用于超时的不完整键盘/鼠标 CSI。
fn discard_host_reply_csi_tail(buffer: &mut Vec<u8>, discarded_tail_bytes: &mut usize) -> bool {
    let remaining = MAX_DISCARDED_CONTROL_TAIL_BYTES.saturating_sub(*discarded_tail_bytes);
    let inspected = buffer.len().min(remaining);

    for index in 0..inspected {
        match buffer[index] {
            0x20..=0x3f => {}
            0x40..=0x7e => {
                buffer.drain(..=index);
                return true;
            }
            _ => {
                buffer.drain(..index);
                return true;
            }
        }
    }

    buffer.drain(..inspected);
    *discarded_tail_bytes = discarded_tail_bytes.saturating_add(inspected);
    *discarded_tail_bytes >= MAX_DISCARDED_CONTROL_TAIL_BYTES
}

enum SgrMouseContinuation {
    Incomplete,
    Complete(usize),
    Invalid,
}

fn classify_sgr_mouse_continuation(prefix: &[u8], tail: &[u8]) -> SgrMouseContinuation {
    let remaining = MAX_DISCARDED_CONTROL_TAIL_BYTES.saturating_sub(prefix.len());
    let tail = &tail[..tail.len().min(remaining)];
    let final_index = tail
        .iter()
        .position(|byte| !byte.is_ascii_digit() && *byte != b';');
    let payload_len = final_index.unwrap_or(tail.len());
    let mut report = prefix.to_vec();
    report.extend_from_slice(&tail[..payload_len]);
    if !plausible_sgr_mouse_prefix(&report) {
        return SgrMouseContinuation::Invalid;
    }
    if let Some(index) = final_index {
        report.push(tail[index]);
        let valid = std::str::from_utf8(&report)
            .ok()
            .and_then(parse_sgr_mouse)
            .is_some();
        return if valid {
            SgrMouseContinuation::Complete(index + 1)
        } else {
            SgrMouseContinuation::Invalid
        };
    }
    if report.len() >= MAX_DISCARDED_CONTROL_TAIL_BYTES {
        SgrMouseContinuation::Invalid
    } else {
        SgrMouseContinuation::Incomplete
    }
}

// Reject impossible continuations early, without changing the general mouse
// parser. A partial last field (including zero) can still become valid.
fn plausible_sgr_mouse_prefix(report: &[u8]) -> bool {
    let Some(body) = report.strip_prefix(b"\x1b[<") else {
        return false;
    };
    let mut fields = body.split(|byte| *byte == b';').enumerate().peekable();
    while let Some((field, digits)) = fields.next() {
        if field > 2 {
            return false;
        }
        if digits.is_empty() {
            return fields.peek().is_none();
        }
        if !digits.iter().all(u8::is_ascii_digit) {
            return false;
        }
        let Some(value) = std::str::from_utf8(digits)
            .ok()
            .and_then(|digits| digits.parse::<u16>().ok())
        else {
            return false;
        };
        if field == 0 && value > u16::from(u8::MAX) {
            return false;
        }
        if fields.peek().is_some()
            && ((field == 0 && parse_mouse_cb(value as u8).is_none()) || (field == 1 && value == 0))
        {
            return false;
        }
    }
    true
}

fn osc_string_terminator(buffer: &[u8]) -> Option<usize> {
    let st = find_subsequence(buffer, b"\x1b\\").map(|idx| idx + 2);
    let bel = buffer
        .iter()
        .position(|byte| *byte == b'\x07')
        .map(|idx| idx + 1);

    match (st, bel) {
        (Some(st), Some(bel)) => Some(st.min(bel)),
        (Some(st), None) => Some(st),
        (None, Some(bel)) => Some(bel),
        (None, None) => None,
    }
}

fn st_string_terminator(buffer: &[u8]) -> Option<usize> {
    find_subsequence(buffer, b"\x1b\\").map(|idx| idx + 2)
}

fn control_string_terminator_for_family(
    buffer: &[u8],
    family: ControlStringFamily,
) -> Option<usize> {
    match family {
        ControlStringFamily::Osc => osc_string_terminator(buffer),
        ControlStringFamily::StTerminated => st_string_terminator(buffer),
        ControlStringFamily::HostReplyCsi => None,
    }
}

fn find_csi_final(buffer: &[u8], finals: &[u8]) -> Option<usize> {
    for (idx, byte) in buffer.iter().enumerate().skip(2) {
        if finals.contains(byte) {
            return Some(idx + 1);
        }
    }
    None
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn parse_default_mouse(sequence: &[u8]) -> Option<MouseEvent> {
    let &[ESC, b'[', b'M', encoded_cb, encoded_column, encoded_row] = sequence else {
        return None;
    };
    let cb = encoded_cb.checked_sub(32)?;
    let column = u16::from(encoded_column).checked_sub(33)?;
    let row = u16::from(encoded_row).checked_sub(33)?;
    let (kind, modifiers) = parse_mouse_cb(cb)?;

    Some(MouseEvent {
        kind,
        column,
        row,
        modifiers,
    })
}

fn parse_sgr_mouse(sequence: &str) -> Option<MouseEvent> {
    let body = sequence.strip_prefix("\x1b[<")?;
    let final_char = body.chars().last()?;
    if final_char != 'M' && final_char != 'm' {
        return None;
    }

    let payload = &body[..body.len() - 1];
    let mut parts = payload.split(';');
    let cb = parts.next()?.parse::<u8>().ok()?;
    let column = parts.next()?.parse::<u16>().ok()?.checked_sub(1)?;
    let row = parts.next()?.parse::<u16>().ok()?.checked_sub(1)?;
    let (kind, modifiers) = parse_mouse_cb(cb)?;

    let kind = if final_char == 'm' {
        match kind {
            MouseEventKind::Down(button) => MouseEventKind::Up(button),
            other => other,
        }
    } else {
        kind
    };

    Some(MouseEvent {
        kind,
        column,
        row,
        modifiers,
    })
}

fn parse_mouse_cb(cb: u8) -> Option<(MouseEventKind, KeyModifiers)> {
    let button_number = (cb & 0b0000_0011) | ((cb & 0b1100_0000) >> 4);
    let dragging = cb & 0b0010_0000 == 0b0010_0000;

    let kind = match (button_number, dragging) {
        (0, false) => MouseEventKind::Down(MouseButton::Left),
        (1, false) => MouseEventKind::Down(MouseButton::Middle),
        (2, false) => MouseEventKind::Down(MouseButton::Right),
        (0, true) => MouseEventKind::Drag(MouseButton::Left),
        (1, true) => MouseEventKind::Drag(MouseButton::Middle),
        (2, true) => MouseEventKind::Drag(MouseButton::Right),
        (3, false) => MouseEventKind::Up(MouseButton::Left),
        // Crossterm cannot represent extended-button drags. Preserve their
        // position as motion so a stuck host button cannot suppress hover.
        (3, true) | (4, true) | (5, true) | (8, true) | (9, true) => MouseEventKind::Moved,
        (4, false) => MouseEventKind::ScrollUp,
        (5, false) => MouseEventKind::ScrollDown,
        (6, false) => MouseEventKind::ScrollLeft,
        (7, false) => MouseEventKind::ScrollRight,
        _ => return None,
    };

    let mut modifiers = KeyModifiers::empty();
    if cb & 0b0000_0100 != 0 {
        modifiers |= KeyModifiers::SHIFT;
    }
    if cb & 0b0000_1000 != 0 {
        modifiers |= KeyModifiers::ALT;
    }
    if cb & 0b0001_0000 != 0 {
        modifiers |= KeyModifiers::CONTROL;
    }

    Some((kind, modifiers))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEventKind};

    fn assert_raw_key(event: RawInputEvent, code: KeyCode, modifiers: KeyModifiers) {
        let RawInputEvent::Key(key) = event else {
            panic!("expected key");
        };
        assert_eq!(key.code, code);
        assert_eq!(key.modifiers, modifiers);
    }

    fn decode_hex(hex: &str) -> Vec<u8> {
        let hex = hex.trim();
        assert_eq!(hex.len() % 2, 0, "hex string must have even length");
        (0..hex.len())
            .step_by(2)
            .map(|idx| u8::from_str_radix(&hex[idx..idx + 2], 16).unwrap())
            .collect()
    }

    fn parse_fixture_key_code(value: &str) -> KeyCode {
        match value {
            "enter" => KeyCode::Enter,
            "tab" => KeyCode::Tab,
            "backspace" => KeyCode::Backspace,
            "esc" => KeyCode::Esc,
            "up" => KeyCode::Up,
            "down" => KeyCode::Down,
            "left" => KeyCode::Left,
            "right" => KeyCode::Right,
            "home" => KeyCode::Home,
            "end" => KeyCode::End,
            "pageup" => KeyCode::PageUp,
            "pagedown" => KeyCode::PageDown,
            "insert" => KeyCode::Insert,
            "delete" => KeyCode::Delete,
            value if value.starts_with("char:") => {
                KeyCode::Char(value.trim_start_matches("char:").chars().next().unwrap())
            }
            other => panic!("unsupported fixture key code: {other}"),
        }
    }

    fn parse_fixture_modifiers(value: &str) -> KeyModifiers {
        if value == "-" || value.is_empty() {
            return KeyModifiers::empty();
        }

        let mut modifiers = KeyModifiers::empty();
        for part in value.split('+') {
            match part {
                "shift" => modifiers |= KeyModifiers::SHIFT,
                "alt" => modifiers |= KeyModifiers::ALT,
                "control" => modifiers |= KeyModifiers::CONTROL,
                "super" => modifiers |= KeyModifiers::SUPER,
                "hyper" => modifiers |= KeyModifiers::HYPER,
                "meta" => modifiers |= KeyModifiers::META,
                other => panic!("unsupported fixture modifier: {other}"),
            }
        }
        modifiers
    }

    #[test]
    fn parses_kitty_shift_letter_release() {
        let (RawInputEvent::Key(key), consumed) = extract_one_event(b"\x1b[108:76;2:3u").unwrap()
        else {
            panic!("expected key");
        };
        assert_eq!(consumed, 13);
        assert_eq!(key.code, KeyCode::Char('l'));
        assert_eq!(key.modifiers, KeyModifiers::SHIFT);
        assert_eq!(key.kind, KeyEventKind::Release);
        assert_eq!(key.shifted_codepoint, Some('L' as u32));
    }

    #[test]
    fn parses_bracketed_paste() {
        let (RawInputEvent::Paste(text), consumed) =
            extract_one_event(b"\x1b[200~hello\x1b[201~rest").unwrap()
        else {
            panic!("expected paste");
        };
        assert_eq!(text, "hello");
        assert_eq!(consumed, 17);
    }

    #[test]
    fn complete_text_bracketed_paste_requires_one_exact_utf8_sequence() {
        assert_eq!(
            complete_text_bracketed_paste(b"\x1b[200~hello\x1b[201~"),
            Some("hello")
        );
        assert!(!is_complete_text_bracketed_paste(b"\x1b[200~hello"));
        assert!(!is_complete_text_bracketed_paste(
            b"\x1b[200~hello\x1b[201~rest"
        ));
        assert!(!is_complete_text_bracketed_paste(
            b"\x1b[200~one\x1b[201~\x1b[200~two\x1b[201~"
        ));
        assert!(!is_complete_text_bracketed_paste(b"\x1b[200~\xff\x1b[201~"));
    }

    #[test]
    fn parses_sgr_mouse() {
        let (RawInputEvent::Mouse(mouse), consumed) = extract_one_event(b"\x1b[<0;20;10M").unwrap()
        else {
            panic!("expected mouse");
        };
        assert_eq!(consumed, 11);
        assert_eq!(mouse.kind, MouseEventKind::Down(MouseButton::Left));
        assert_eq!(mouse.column, 19);
        assert_eq!(mouse.row, 9);
        assert_eq!(mouse.modifiers, KeyModifiers::empty());
    }

    #[test]
    fn parses_default_mouse_encoding() {
        let mut framer = RawInputFramer::default();
        let events = framer.push(b"\x1b[MCN1");
        let [RawInputEvent::Mouse(mouse)] = events.as_slice() else {
            panic!("expected one mouse event");
        };
        assert_eq!(mouse.kind, MouseEventKind::Moved);
        assert_eq!((mouse.column, mouse.row), (45, 16));
        assert_eq!(mouse.modifiers, KeyModifiers::empty());
    }

    #[test]
    fn rejected_default_mouse_frame_preserves_trailing_input() {
        let mut framer = RawInputFramer::default();
        let events = framer.push(b"\x1b[M\x82AAx");

        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], RawInputEvent::Unsupported));
        assert_raw_key(
            events.into_iter().nth(1).unwrap(),
            KeyCode::Char('x'),
            KeyModifiers::empty(),
        );
    }

    #[test]
    fn parses_extended_button_drag_as_mouse_motion() {
        for input in [
            b"\x1b[<160;20;10M".as_slice(),
            b"\x1b[<161;20;10M".as_slice(),
        ] {
            let (RawInputEvent::Mouse(mouse), _) = extract_one_event(input).unwrap() else {
                panic!("expected mouse");
            };
            assert_eq!(mouse.kind, MouseEventKind::Moved);
            assert_eq!((mouse.column, mouse.row), (19, 9));
        }
    }

    #[test]
    fn parses_sgr_mouse_observable_modifiers() {
        let cases = [
            (b"\x1b[<8;20;10M".as_slice(), KeyModifiers::ALT),
            (b"\x1b[<16;20;10M".as_slice(), KeyModifiers::CONTROL),
            (
                b"\x1b[<24;20;10M".as_slice(),
                KeyModifiers::ALT | KeyModifiers::CONTROL,
            ),
        ];

        for (input, expected) in cases {
            let (RawInputEvent::Mouse(mouse), _) = extract_one_event(input).unwrap() else {
                panic!("expected mouse");
            };
            assert_eq!(mouse.modifiers, expected);
            assert!(!mouse.modifiers.contains(KeyModifiers::SUPER));
        }
    }

    #[test]
    fn parses_host_default_color_response_with_st() {
        let (RawInputEvent::HostDefaultColor { kind, color }, consumed) =
            extract_one_event(b"\x1b]10;rgb:cccc/dddd/eeee\x1b\\").unwrap()
        else {
            panic!("expected host color response");
        };
        assert_eq!(consumed, 25);
        assert_eq!(kind, DefaultColorKind::Foreground);
        assert_eq!(
            color,
            RgbColor {
                r: 0xcc,
                g: 0xdd,
                b: 0xee
            }
        );
    }

    #[test]
    fn parses_host_default_color_response_with_bel() {
        let (RawInputEvent::HostDefaultColor { kind, color }, consumed) =
            extract_one_event(b"\x1b]11;#112233\x07").unwrap()
        else {
            panic!("expected host color response");
        };
        assert_eq!(consumed, 13);
        assert_eq!(kind, DefaultColorKind::Background);
        assert_eq!(
            color,
            RgbColor {
                r: 0x11,
                g: 0x22,
                b: 0x33
            }
        );
    }

    #[test]
    fn parses_host_palette_color_response() {
        let (RawInputEvent::HostPaletteColors { colors }, consumed) =
            extract_one_event(b"\x1b]4;7;rgb:1111/2222/3333\x1b\\").unwrap()
        else {
            panic!("expected host palette response");
        };
        assert_eq!(consumed, 26);
        assert_eq!(
            colors,
            vec![(
                7,
                RgbColor {
                    r: 0x11,
                    g: 0x22,
                    b: 0x33,
                }
            )]
        );
    }

    #[test]
    fn parses_legacy_up_arrow() {
        let (RawInputEvent::Key(key), consumed) = extract_one_event(b"\x1b[A").unwrap() else {
            panic!("expected key");
        };
        assert_eq!(consumed, 3);
        assert_eq!(key.code, KeyCode::Up);
    }

    #[test]
    fn parses_outer_focus_events() {
        let (event, consumed) = extract_one_event(b"\x1b[I").unwrap();
        assert_eq!(consumed, 3);
        assert!(matches!(event, RawInputEvent::OuterFocusGained));

        let (event, consumed) = extract_one_event(b"\x1b[O").unwrap();
        assert_eq!(consumed, 3);
        assert!(matches!(event, RawInputEvent::OuterFocusLost));
    }

    #[test]
    fn outer_focus_gained_requests_host_surface_redraw() {
        let events = parse_raw_input_bytes_sync(b"\x1b[I");
        assert!(events_require_host_surface_redraw(&events, true));
        assert!(!events_require_host_surface_redraw(&events, false));

        let events = parse_raw_input_bytes_sync(b"\x1b[O");
        assert!(!events_require_host_surface_redraw(&events, true));
    }

    #[test]
    fn outer_focus_gained_requests_host_mode_refresh() {
        assert!(events_require_host_mode_refresh(
            &parse_raw_input_bytes_sync(b"\x1b[I")
        ));
        assert!(!events_require_host_mode_refresh(
            &parse_raw_input_bytes_sync(b"\x1b[O")
        ));
    }

    #[test]
    fn outer_focus_gained_requests_host_appearance_query() {
        let gained = parse_raw_input_bytes_sync(b"\x1b[I");
        let lost = parse_raw_input_bytes_sync(b"\x1b[O");
        let scheme_report = parse_raw_input_bytes_sync(b"\x1b[?997;1n");

        assert!(events_require_host_terminal_appearance_query(&gained));
        assert!(!events_require_host_terminal_appearance_query(&lost));
        assert!(!events_require_host_terminal_appearance_query(
            &scheme_report
        ));
        assert!(events_require_host_terminal_theme_query(&scheme_report));
    }

    #[test]
    fn parses_ghostty_color_scheme_reports() {
        for bytes in [
            GHOSTTY_COLOR_SCHEME_DARK_REPORT,
            GHOSTTY_COLOR_SCHEME_LIGHT_REPORT,
        ] {
            let events = parse_raw_input_bytes_sync(bytes);
            assert_eq!(events.len(), 1, "bytes: {bytes:?}");
            assert!(matches!(
                events[0],
                RawInputEvent::HostColorSchemeChanged(HostAppearance::Dark | HostAppearance::Light)
            ));
            assert!(events_require_host_terminal_theme_query(&events));
        }
    }

    #[test]
    fn ghostty_color_scheme_report_parser_is_exact() {
        for bytes in [
            b"\x1b[?997;0n".as_slice(),
            b"\x1b[?997;3n".as_slice(),
            b"\x1b[?998;1n".as_slice(),
        ] {
            let events = parse_raw_input_bytes_sync(bytes);
            assert_eq!(events.len(), 1, "bytes: {bytes:?}");
            assert!(matches!(events[0], RawInputEvent::Unsupported));
            assert!(!events_require_host_terminal_theme_query(&events));
        }
    }

    #[test]
    fn raw_input_framer_reassembles_split_color_scheme_report() {
        let mut framer = RawInputFramer::default();

        assert!(framer.push(b"\x1b[?997;").is_empty());
        let events = framer.push(b"1n");

        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            RawInputEvent::HostColorSchemeChanged(HostAppearance::Dark)
        ));
    }

    #[test]
    fn parses_host_cell_size_report() {
        let events = parse_raw_input_bytes_sync(b"\x1b[6;21;10t");

        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            RawInputEvent::HostCellSizeReport {
                width_px: 10,
                height_px: 21,
            }
        ));
    }

    #[test]
    fn host_cell_size_report_parser_is_exact() {
        for bytes in [
            // Zero dimensions carry no usable cell size.
            b"\x1b[6;0;10t".as_slice(),
            b"\x1b[6;21;0t".as_slice(),
            // Missing or extra parameters.
            b"\x1b[6;21t".as_slice(),
            b"\x1b[6;21;10;3t".as_slice(),
            // Other XTWINOPS reports must not be mistaken for a cell size.
            b"\x1b[4;1610;777t".as_slice(),
            b"\x1b[8;37;161t".as_slice(),
            // Non-numeric parameters.
            b"\x1b[6;21;1-t".as_slice(),
        ] {
            assert!(
                parse_host_cell_size_report(bytes).is_none(),
                "bytes: {bytes:?}"
            );
        }
    }

    #[test]
    fn split_color_scheme_timeout_does_not_swallow_legacy_alt_bracket() {
        let mut framer = RawInputByteFramer::default();

        // 裸 `ESC [` 是每条 CSI 的引导符：先多等一个空闲窗口，到期才按 Alt+[ 送出。
        assert!(framer.push(b"\x1b[").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert_eq!(framer.flush_timeout(), vec![b"\x1b[".to_vec()]);
    }

    #[test]
    fn raw_input_byte_framer_discards_timed_out_split_color_scheme_report_tail() {
        let mut framer = RawInputByteFramer::default();

        assert!(framer.push(b"\x1b[?997;").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert!(framer.push(b"1n").is_empty());
        assert_eq!(framer.push(b"a"), vec![b"a".to_vec()]);
        assert!(framer.flush_timeout().is_empty());
    }

    #[test]
    fn parses_xterm_alt_up_arrow() {
        let (RawInputEvent::Key(key), consumed) = extract_one_event(b"\x1b[1;3A").unwrap() else {
            panic!("expected key");
        };
        assert_eq!(consumed, 6);
        assert_eq!(key.code, KeyCode::Up);
        assert_eq!(key.modifiers, KeyModifiers::ALT);
    }

    #[test]
    fn parses_legacy_alt_backspace() {
        let (RawInputEvent::Key(key), consumed) = extract_one_event(b"\x1b\x7f").unwrap() else {
            panic!("expected key");
        };
        assert_eq!(consumed, 2);
        assert_eq!(key.code, KeyCode::Backspace);
        assert_eq!(key.modifiers, KeyModifiers::ALT);
    }

    #[test]
    fn parses_kitty_alt_backspace() {
        let (RawInputEvent::Key(key), consumed) = extract_one_event(b"\x1b[127;3u").unwrap() else {
            panic!("expected key");
        };
        assert_eq!(consumed, 8);
        assert_eq!(key.code, KeyCode::Backspace);
        assert_eq!(key.modifiers, KeyModifiers::ALT);
    }

    #[test]
    fn parses_enhanced_pageup_press() {
        let (RawInputEvent::Key(key), consumed) = extract_one_event(b"\x1b[5;1:1~").unwrap() else {
            panic!("expected key");
        };
        assert_eq!(consumed, 8);
        assert_eq!(key.code, KeyCode::PageUp);
        assert_eq!(key.modifiers, KeyModifiers::empty());
        assert_eq!(key.kind, KeyEventKind::Press);
    }

    #[test]
    fn parses_enhanced_pagedown_release() {
        let (RawInputEvent::Key(key), consumed) = extract_one_event(b"\x1b[6;1:3~").unwrap() else {
            panic!("expected key");
        };
        assert_eq!(consumed, 8);
        assert_eq!(key.code, KeyCode::PageDown);
        assert_eq!(key.modifiers, KeyModifiers::empty());
        assert_eq!(key.kind, KeyEventKind::Release);
    }

    #[test]
    fn raw_input_family_matrix_is_covered() {
        let cases: &[(&[u8], KeyCode, KeyModifiers)] = &[
            (b"\x02", KeyCode::Char('b'), KeyModifiers::CONTROL),
            (b"\r", KeyCode::Enter, KeyModifiers::empty()),
            (b"\t", KeyCode::Tab, KeyModifiers::empty()),
            (b"\x7f", KeyCode::Backspace, KeyModifiers::empty()),
            (b"\x1b[A", KeyCode::Up, KeyModifiers::empty()),
            (b"\x1b[1;3A", KeyCode::Up, KeyModifiers::ALT),
            (b"\x1b\x7f", KeyCode::Backspace, KeyModifiers::ALT),
            (b"\x1b[127;3u", KeyCode::Backspace, KeyModifiers::ALT),
            (b"\x1b[57420;1u", KeyCode::Down, KeyModifiers::empty()),
            (b"\x1b[57423;1u", KeyCode::Home, KeyModifiers::empty()),
            (b"\x1bOq", KeyCode::Char('1'), KeyModifiers::empty()),
            (b"\x1b[14~", KeyCode::F(4), KeyModifiers::empty()),
            (b"\x1b[11;2~", KeyCode::F(1), KeyModifiers::SHIFT),
            (b"\x1b[13;1:1~", KeyCode::F(3), KeyModifiers::empty()),
            (b"\x1b[14;3~", KeyCode::F(4), KeyModifiers::ALT),
            (b"\x1b[57364;1u", KeyCode::F(1), KeyModifiers::empty()),
            (b"\x1b[57366;1u", KeyCode::F(3), KeyModifiers::empty()),
            (b"\x1b[57366;2u", KeyCode::F(3), KeyModifiers::SHIFT),
            (b"\x1b[57375;1u", KeyCode::F(12), KeyModifiers::empty()),
            (b"\x1b[57376;1u", KeyCode::F(13), KeyModifiers::empty()),
            (b"\x1b[49:33;2:1u", KeyCode::Char('1'), KeyModifiers::SHIFT),
        ];

        for (bytes, code, modifiers) in cases {
            let (event, consumed) = extract_one_event(bytes).unwrap();
            assert_eq!(consumed, bytes.len());
            assert_raw_key(event, *code, *modifiers);
        }
    }

    #[test]
    fn raw_framer_waits_for_application_keypad_sequence_final_byte() {
        let mut framer = RawInputFramer::default();

        assert!(framer.push(b"\x1bO").is_empty());
        let events = framer.push(b"q");

        assert_eq!(events.len(), 1);
        assert_raw_key(
            events.into_iter().next().unwrap(),
            KeyCode::Char('1'),
            KeyModifiers::empty(),
        );
    }

    #[test]
    fn unsupported_ss3_sequence_stays_unsupported() {
        let (event, consumed) = extract_one_event(b"\x1bOz").unwrap();

        assert_eq!(consumed, 3);
        assert!(matches!(event, RawInputEvent::Unsupported));
    }

    #[test]
    fn parses_modified_rxvt_f_key_alias() {
        let (event, consumed) = extract_one_event(b"\x1b[14;3~").unwrap();

        assert_eq!(consumed, 7);
        assert_raw_key(event, KeyCode::F(4), KeyModifiers::ALT);
    }

    #[test]
    fn flushes_lone_escape_after_timeout() {
        let mut framer = RawInputFramer::default();
        assert!(framer.push(&[ESC]).is_empty());

        let events = framer.flush_timeout();
        assert_eq!(events.len(), 1);
        assert_raw_key(
            events.into_iter().next().unwrap(),
            KeyCode::Esc,
            KeyModifiers::empty(),
        );
    }

    #[test]
    fn parses_raw_ctrl_b() {
        let (RawInputEvent::Key(key), consumed) = extract_one_event(b"\x02").unwrap() else {
            panic!("expected key");
        };
        assert_eq!(consumed, 1);
        assert_eq!(key.code, KeyCode::Char('b'));
        assert_eq!(key.modifiers, KeyModifiers::CONTROL);
    }

    #[test]
    fn parses_raw_lf_as_ctrl_j() {
        let (RawInputEvent::Key(key), consumed) = extract_one_event(b"\n").unwrap() else {
            panic!("expected key");
        };
        assert_eq!(consumed, 1);
        assert_eq!(key.code, KeyCode::Char('j'));
        assert_eq!(key.modifiers, KeyModifiers::CONTROL);
    }

    fn assert_fixture_extracts_whole_events(corpus: &str, macos_layout: bool) {
        for line in corpus.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            let mut columns: Vec<_> = line.split('\t').collect();
            if columns.len() == 5 {
                columns.push("");
            }

            if macos_layout {
                if columns.len() == 6 {
                    columns.push("");
                }
                assert_eq!(
                    columns.len(),
                    7,
                    "macOS fixture row must have 7 columns: {line}"
                );
                if columns[2].is_empty() {
                    continue;
                }
                let bytes = decode_hex(columns[2]);
                let (event, consumed) = extract_one_event(&bytes).unwrap();
                assert_eq!(
                    consumed,
                    bytes.len(),
                    "fixture should extract a whole event: {line}"
                );
                assert_raw_key(
                    event,
                    parse_fixture_key_code(columns[3]),
                    parse_fixture_modifiers(columns[4]),
                );
            } else {
                if columns.len() == 5 {
                    columns.push("");
                }
                let (bytes_hex, code, modifiers) = match columns.len() {
                    6 => {
                        if columns[1].chars().all(|ch| ch.is_ascii_hexdigit()) {
                            (columns[1], columns[2], columns[3])
                        } else {
                            (columns[2], columns[3], columns[4])
                        }
                    }
                    7 => (columns[2], columns[3], columns[4]),
                    _ => panic!("fixture row must have 6 or 7 columns: {line}"),
                };
                assert!(
                    bytes_hex.chars().all(|ch| ch.is_ascii_hexdigit()),
                    "non-hex fixture bytes: {bytes_hex} in {line}"
                );
                let bytes = decode_hex(bytes_hex);
                let (event, consumed) = extract_one_event(&bytes).unwrap();
                assert_eq!(
                    consumed,
                    bytes.len(),
                    "fixture should extract a whole event: {line}"
                );
                assert_raw_key(
                    event,
                    parse_fixture_key_code(code),
                    parse_fixture_modifiers(modifiers),
                );
            }
        }
    }

    #[test]
    fn raw_input_corpus_fixture_extracts_whole_events() {
        let corpus = include_str!("../tests/fixtures/keyboard_protocol_corpus.tsv");
        assert_fixture_extracts_whole_events(corpus, false);
    }

    #[test]
    fn raw_input_macos_terminal_variants_fixture_extracts_whole_events() {
        let corpus = include_str!("../tests/fixtures/macos_terminal_variants.tsv");
        assert_fixture_extracts_whole_events(corpus, true);
    }

    #[test]
    fn raw_input_linux_terminal_variants_fixture_extracts_whole_events() {
        let corpus = include_str!("../tests/fixtures/linux_terminal_variants.tsv");
        assert_fixture_extracts_whole_events(corpus, false);
    }

    #[test]
    fn chunked_legacy_arrow_waits_for_completion() {
        let mut framer = RawInputFramer::default();

        assert!(framer.push(b"\x1b").is_empty());
        let events = framer.push(b"[A");
        assert_eq!(events.len(), 1);
        assert_raw_key(
            events.into_iter().next().unwrap(),
            KeyCode::Up,
            KeyModifiers::empty(),
        );
    }

    #[test]
    fn lone_escape_is_buffered_until_timeout_flush() {
        let mut framer = RawInputFramer::default();

        assert!(framer.push(b"\x1b").is_empty());
        let events = framer.flush_timeout();
        assert_eq!(events.len(), 1);
        assert_raw_key(
            events.into_iter().next().unwrap(),
            KeyCode::Esc,
            KeyModifiers::empty(),
        );
    }

    #[test]
    fn escape_followed_by_arrow_before_flush_does_not_emit_escape() {
        let mut framer = RawInputFramer::default();

        assert!(framer.push(b"\x1b").is_empty());
        let events = framer.push(b"[B");
        assert_eq!(events.len(), 1);
        assert_raw_key(
            events.into_iter().next().unwrap(),
            KeyCode::Down,
            KeyModifiers::empty(),
        );
    }

    #[test]
    fn escape_followed_by_sgr_mouse_before_flush_does_not_emit_text() {
        let mut framer = RawInputFramer::default();

        assert!(framer.push(b"\x1b").is_empty());
        let events = framer.push(b"[<65;43;26M");

        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            RawInputEvent::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 42,
                row: 25,
                ..
            })
        ));
    }

    #[test]
    fn lone_escape_then_complete_sgr_mouse_report_emits_both_events() {
        for report in [b"\x1b[<35;10;20M".as_slice(), b"\x1b[<35;10;20m".as_slice()] {
            let mut framer = RawInputFramer::default();

            assert!(framer.push(b"\x1b").is_empty());
            let events = framer.push(report);

            assert_eq!(events.len(), 2);
            let mut events = events.into_iter();
            assert_raw_key(events.next().unwrap(), KeyCode::Esc, KeyModifiers::empty());
            assert!(matches!(
                events.next().unwrap(),
                RawInputEvent::Mouse(MouseEvent {
                    kind: MouseEventKind::Moved,
                    column: 9,
                    row: 19,
                    ..
                })
            ));
            assert!(framer.flush_timeout().is_empty());
        }
    }

    #[test]
    fn lone_escape_then_default_mouse_report_emits_both_events() {
        let mut framer = RawInputFramer::default();

        assert!(framer.push(b"\x1b").is_empty());
        let events = framer.push(b"\x1b[MCN1");

        assert_eq!(events.len(), 2);
        let mut events = events.into_iter();
        assert_raw_key(events.next().unwrap(), KeyCode::Esc, KeyModifiers::empty());
        assert!(matches!(
            events.next().unwrap(),
            RawInputEvent::Mouse(MouseEvent {
                kind: MouseEventKind::Moved,
                column: 45,
                row: 16,
                ..
            })
        ));
        assert!(framer.flush_timeout().is_empty());
    }

    #[test]
    fn legacy_doubled_escape_alt_arrow_remains_one_event() {
        let mut framer = RawInputFramer::default();

        let events = framer.push(b"\x1b\x1b[A");

        assert_eq!(events.len(), 1);
        assert_raw_key(
            events.into_iter().next().unwrap(),
            KeyCode::Up,
            KeyModifiers::ALT,
        );
        assert!(framer.flush_timeout().is_empty());
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn non_macos_host_input_splits_lone_escape_from_arrow() {
        let mut framer = RawInputByteFramer::for_host_input();

        assert_eq!(
            framer.push(b"\x1b\x1b[D"),
            vec![b"\x1b".to_vec(), b"\x1b[D".to_vec()]
        );
    }

    #[test]
    fn macos_host_input_policy_preserves_legacy_doubled_escape_alt_arrow() {
        let mut framer = RawInputByteFramer::with_host_input_policy(true);

        assert_eq!(framer.push(b"\x1b\x1b[D"), vec![b"\x1b\x1b[D".to_vec()]);
    }

    #[test]
    fn sgr_mouse_sequence_split_after_button_prefix_is_reassembled_before_timeout() {
        let mut framer = RawInputFramer::default();

        assert!(framer.push(b"\x1b[<3").is_empty());
        let events = framer.push(b"5;58;30M");

        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            RawInputEvent::Mouse(MouseEvent {
                kind: MouseEventKind::Moved,
                column: 57,
                row: 29,
                ..
            })
        ));
    }

    #[test]
    fn timed_out_split_sgr_mouse_tail_is_discarded_and_following_input_is_preserved() {
        let mut framer = RawInputFramer::default();

        assert!(framer.push(b"\x1b[<3").is_empty());
        assert!(framer.flush_timeout().is_empty());
        let events = framer.push(b"5;58;30Mx");

        assert_eq!(events.len(), 1);
        assert_raw_key(
            events.into_iter().next().unwrap(),
            KeyCode::Char('x'),
            KeyModifiers::empty(),
        );
    }

    #[test]
    fn captured_sgr_mouse_tail_after_second_idle_flush_is_discarded() {
        let mut framer = RawInputByteFramer::for_host_input();

        // Issue #3911, 2026-09-13 07:02:14 UTC: this prefix timed out,
        // then its tail arrived 33 ms later. The Unix reader flushes again
        // after 10 ms of continued idle following the first discard.
        assert!(framer.push(b"\x1b[<3").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert_eq!(framer.push(b"5;28;31M"), Vec::<Vec<u8>>::new());
    }

    #[test]
    fn timed_out_sgr_mouse_invalid_completion_is_preserved_after_idle() {
        let mut framer = RawInputByteFramer::default();

        assert!(framer.push(b"\x1b[<3").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert_eq!(framer.push(b"M"), vec![b"M".to_vec()]);
    }

    #[test]
    fn timed_out_sgr_mouse_completion_survives_read_splits_and_idle() {
        let tail = b"5;28;31M";
        for split in 0..=tail.len() {
            let mut framer = RawInputByteFramer::for_host_input();
            assert!(framer.push(b"\x1b[<3").is_empty());
            assert!(framer.flush_timeout().is_empty());
            assert!(framer.flush_timeout().is_empty());
            assert!(framer.push(&tail[..split]).is_empty());
            assert!(framer.flush_timeout().is_empty());
            assert!(framer.flush_timeout().is_empty());
            let mut rest = tail[split..].to_vec();
            rest.extend_from_slice(b"x\x1b[A");
            assert_eq!(framer.push(&rest), vec![b"x".to_vec(), b"\x1b[A".to_vec()]);
            assert!(framer.timed_out_mouse_prefix.is_none());
            assert!(!framer.has_pending_input());
        }
    }

    #[test]
    fn timed_out_sgr_mouse_invalid_syntax_releases_continuation() {
        for tail in [
            b"5;0;31M".as_slice(), // zero coordinate
            b"5;;31M",             // empty field
            b"5;28;31;1M",         // extra field (the general parser is permissive)
            b"999;28;31M",         // button overflow
            b"5;65536;31M",        // coordinate overflow
        ] {
            let mut framer = RawInputByteFramer::default();
            assert!(framer.push(b"\x1b[<3").is_empty());
            assert!(framer.flush_timeout().is_empty());
            assert_eq!(framer.push(tail).concat(), tail);
            assert!(framer.timed_out_mouse_prefix.is_none());
        }
    }

    #[test]
    fn timed_out_sgr_mouse_interruption_preserves_text_and_new_events() {
        for suffix in [
            b"x".as_slice(),
            b"\x1b[A",                  // new key sequence
            b"\x1b[200~paste\x1b[201~", // bracketed paste
            "\u{4f60}".as_bytes(),      // UTF-8 split across reads
        ] {
            let mut framer = RawInputByteFramer::default();
            assert!(framer.push(b"\x1b[<3").is_empty());
            assert!(framer.flush_timeout().is_empty());
            assert!(framer.push(b"5;28;").is_empty());
            assert!(framer.flush_timeout().is_empty());
            let mut chunks = Vec::new();
            for byte in suffix {
                chunks.extend(framer.push(&[*byte]));
            }
            let mut expected = b"5;28;".to_vec();
            expected.extend_from_slice(suffix);
            assert_eq!(chunks.concat(), expected);
            assert!(framer.timed_out_mouse_prefix.is_none());
            assert_eq!(framer.push(b"123M").concat(), b"123M");
        }
    }

    #[test]
    fn timed_out_sgr_mouse_budget_includes_prefix_and_preserves_overflow() {
        let prefix = b"\x1b[<35;1;";
        let remaining = MAX_DISCARDED_CONTROL_TAIL_BYTES - prefix.len();
        let mut framer = RawInputByteFramer::default();
        assert!(framer.push(prefix).is_empty());
        assert!(framer.flush_timeout().is_empty());
        let mut valid_tail = vec![b'0'; remaining - 2];
        valid_tail.extend_from_slice(b"1M");
        assert!(framer.push(&valid_tail).is_empty()); // complete exactly at limit
        assert!(framer.timed_out_mouse_prefix.is_none());

        for length in [remaining, remaining + 1024] {
            let mut framer = RawInputByteFramer::default();
            assert!(framer.push(prefix).is_empty());
            assert!(framer.flush_timeout().is_empty());
            let tail = vec![b'0'; length];
            assert_eq!(framer.push(&tail).concat(), tail);
            assert!(framer.timed_out_mouse_prefix.is_none());
            assert_eq!(framer.push(b"1Mtext").concat(), b"1Mtext");
        }

        let mut framer = RawInputByteFramer::default();
        assert!(framer.push(prefix).is_empty());
        assert!(framer.flush_timeout().is_empty());
        let mut tail = vec![b'0'; remaining - 1];
        assert!(framer.push(&tail).is_empty());
        assert!(framer.flush_timeout().is_empty());
        tail.extend_from_slice(b"01M");
        assert_eq!(framer.push(b"01M").concat(), tail);
        assert!(framer.timed_out_mouse_prefix.is_none());
    }

    #[test]
    fn sgr_mouse_tail_after_lone_escape_timeout_is_discarded() {
        let mut framer = RawInputFramer::default();

        assert!(framer.push(b"\x1b").is_empty());
        let timeout_events = framer.flush_timeout();
        assert_eq!(timeout_events.len(), 1);
        assert_raw_key(
            timeout_events.into_iter().next().unwrap(),
            KeyCode::Esc,
            KeyModifiers::empty(),
        );

        assert!(framer.push(b"[<65;43;26M").is_empty());
    }

    #[test]
    fn input_after_discarded_complete_sgr_mouse_tail_is_preserved() {
        let mut framer = RawInputFramer::default();

        assert!(framer.push(b"\x1b").is_empty());
        assert_eq!(framer.flush_timeout().len(), 1);
        let events = framer.push(b"[<65;43;26Mx");
        assert_eq!(events.len(), 1);
        assert_raw_key(
            events.into_iter().next().unwrap(),
            KeyCode::Char('x'),
            KeyModifiers::empty(),
        );
    }

    #[test]
    fn invalid_orphaned_sgr_mouse_tail_after_escape_timeout_is_preserved() {
        let mut framer = RawInputFramer::default();

        assert!(framer.push(b"\x1b").is_empty());
        assert_eq!(framer.flush_timeout().len(), 1);

        let events = framer.push(b"[<x");

        assert_eq!(events.len(), 3);
        assert_raw_key(
            events.into_iter().last().unwrap(),
            KeyCode::Char('x'),
            KeyModifiers::empty(),
        );
    }

    #[test]
    fn double_split_sgr_mouse_tail_after_lone_escape_timeout_is_discarded() {
        let mut framer = RawInputFramer::default();

        assert!(framer.push(b"\x1b").is_empty());
        assert_eq!(framer.flush_timeout().len(), 1);

        assert!(framer.push(b"[<65;4").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert!(framer.flush_timeout().is_empty());
        let events = framer.push(b"3;26Mx");
        assert_eq!(events.len(), 1);
        assert_raw_key(
            events.into_iter().next().unwrap(),
            KeyCode::Char('x'),
            KeyModifiers::empty(),
        );
    }

    #[test]
    fn escape_followed_by_alt_char_before_flush_becomes_alt_key() {
        let mut framer = RawInputFramer::default();

        assert!(framer.push(b"\x1b").is_empty());
        let events = framer.push(b"b");
        assert_eq!(events.len(), 1);
        assert_raw_key(
            events.into_iter().next().unwrap(),
            KeyCode::Char('b'),
            KeyModifiers::ALT,
        );
    }

    #[test]
    fn chunked_kitty_sequence_waits_for_completion() {
        let mut framer = RawInputFramer::default();

        assert!(framer.push(b"\x1b[49:33;2:").is_empty());
        let events = framer.push(b"1u");
        assert_eq!(events.len(), 1);
        assert_raw_key(
            events.into_iter().next().unwrap(),
            KeyCode::Char('1'),
            KeyModifiers::SHIFT,
        );
    }

    #[test]
    fn timed_out_incomplete_csi_discards_its_tail_instead_of_leaking_text() {
        // kitty CSI-u（Super+Space、释放事件）与 ghostty 增强释放序列在参数中间被
        // read(2) 切断：前缀超时后必须武装尾字节丢弃，`;9u` 一类残余不得变成文本。
        for (prefix, tail) in [
            (b"\x1b[32".as_slice(), b";9u".as_slice()),
            (b"\x1b[32;".as_slice(), b"9u".as_slice()),
            (b"\x1b[32;9".as_slice(), b"u".as_slice()),
            (b"\x1b[108:76;2:3".as_slice(), b"u".as_slice()),
            (b"\x1b[1;1:".as_slice(), b"3A".as_slice()),
        ] {
            let mut framer = RawInputByteFramer::default();
            assert!(framer.push(prefix).is_empty(), "prefix: {prefix:?}");
            assert!(framer.flush_timeout().is_empty(), "prefix: {prefix:?}");
            assert!(framer.push(tail).is_empty(), "tail: {tail:?}");
            assert_eq!(framer.push(b"a"), vec![b"a".to_vec()], "tail: {tail:?}");
            assert!(framer.flush_timeout().is_empty(), "tail: {tail:?}");
        }
    }

    #[test]
    fn incomplete_csi_tail_discard_stops_at_the_final_byte() {
        let mut framer = RawInputByteFramer::default();

        assert!(framer.push(b"\x1b[32").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert_eq!(
            framer.push(b";9uabc"),
            vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]
        );
    }

    #[test]
    fn incomplete_csi_tail_discard_releases_on_a_new_escape() {
        let mut framer = RawInputByteFramer::default();

        assert!(framer.push(b"\x1b[32").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert_eq!(framer.push(b"\x1b[A"), vec![b"\x1b[A".to_vec()]);
        assert!(!framer.has_pending_input());
    }

    #[test]
    fn incomplete_csi_tail_discard_is_bounded_by_time_and_by_byte_budget() {
        // 键盘侧的武装必须有墙钟上界：一条永远到不了的尾巴（应用中途退出、SSH 丢
        // 字节、粘贴被截断）不得把用户随后敲的数字/`;`/`:` 吞掉。读线程平时阻塞在
        // read(2) 上、不产生空闲 flush，所以上界不能靠空闲次数。
        let mut framer = RawInputByteFramer::default();
        assert!(framer.push(b"\x1b[32").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert!(framer.push(b";").is_empty()); // 窗口内到达的尾字节仍被吃掉
        framer.backdate_timed_out_csi_for_test();
        assert_eq!(framer.push(b"9"), vec![b"9".to_vec()]);
        assert_eq!(framer.push(b";"), vec![b";".to_vec()]);
        assert!(framer.flush_timeout().is_empty());

        // 空闲 flush 也不会让过期的武装继续吞字节。
        let mut framer = RawInputByteFramer::default();
        assert!(framer.push(b"\x1b[32").is_empty());
        assert!(framer.flush_timeout().is_empty());
        framer.backdate_timed_out_csi_for_test();
        assert!(framer.flush_timeout().is_empty());
        assert_eq!(
            framer.push(b";9u"),
            vec![b";".to_vec(), b"9".to_vec(), b"u".to_vec()]
        );

        // 单次 push 内的字节预算仍然有界，不依赖时间。
        let mut framer = RawInputByteFramer::default();
        assert!(framer.push(b"\x1b[32").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert!(framer.push(&[b'1'; 64]).is_empty());
        assert_eq!(
            framer.push(&[b'2'; 67]),
            vec![b"2".to_vec(), b"2".to_vec(), b"2".to_vec()]
        );
    }

    #[test]
    fn orphaned_csi_tail_recovery_expires_so_later_typing_is_not_eaten() {
        // 孤立 ESC 送出后的「等尾巴」窗口同样有墙钟上界：过期后用户键入的 `[A`
        // 不再被当作被切断的 Up 键尾巴吃掉。
        let mut framer = RawInputByteFramer::default();
        assert!(framer.push(b"\x1b").is_empty());
        assert_eq!(framer.flush_timeout(), vec![b"\x1b".to_vec()]);
        framer.backdate_timed_out_csi_for_test();
        assert_eq!(framer.push(b"[A"), vec![b"[".to_vec(), b"A".to_vec()]);

        // 裸 `ESC [` 之后同理。
        let mut framer = RawInputByteFramer::default();
        assert!(framer.push(b"\x1b[").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert_eq!(framer.flush_timeout(), vec![b"\x1b[".to_vec()]);
        framer.backdate_timed_out_csi_for_test();
        assert_eq!(framer.push(b"32;9u").concat(), b"32;9u");
    }

    #[test]
    fn incomplete_csi_timeout_does_not_change_legacy_alt_bracket_or_lone_escape() {
        // 裸 `\x1b[` 仍作为传统终端的 Alt+[ 送出，只是先被多等一个空闲窗口（它是
        // 每条 CSI 的引导符）；孤立 ESC 仍按原节奏一个窗口后送出。
        let mut alt_bracket = RawInputByteFramer::default();
        assert!(alt_bracket.push(b"\x1b[").is_empty());
        assert!(alt_bracket.flush_timeout().is_empty());
        assert_eq!(alt_bracket.flush_timeout(), vec![b"\x1b[".to_vec()]);
        assert_eq!(alt_bracket.push(b"a"), vec![b"a".to_vec()]);

        let mut escape = RawInputByteFramer::default();
        assert!(escape.push(b"\x1b").is_empty());
        assert_eq!(escape.flush_timeout(), vec![b"\x1b".to_vec()]);
        assert_eq!(escape.push(b"a"), vec![b"a".to_vec()]);
    }

    #[test]
    fn bare_csi_intro_reassembles_when_its_parameters_arrive_one_window_late() {
        // `\x1b[` | `32;9u`（Super+Space 恰在引导符后被切断）：引导符被多等一个
        // 窗口，尾巴到达后整条序列仍然重组，按键不丢也不泄漏为文本。
        let mut framer = RawInputFramer::default();
        assert!(framer.push(b"\x1b[").is_empty());
        assert!(framer.flush_timeout().is_empty());
        let events = framer.push(b"32;9u");
        assert_eq!(events.len(), 1, "{events:?}");
        assert_raw_key(
            events.into_iter().next().unwrap(),
            KeyCode::Char(' '),
            KeyModifiers::SUPER,
        );
    }

    #[test]
    fn alt_bracket_then_orphaned_parameter_tail_is_discarded_not_typed() {
        // 两个空闲窗口都没等到字节 → 按 Alt+[ 送出；此后到达的参数尾巴仍是被切断
        // 的序列残余，必须吃掉而不是变成 `32;9u` 文本。
        let mut framer = RawInputByteFramer::default();
        assert!(framer.push(b"\x1b[").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert_eq!(framer.flush_timeout(), vec![b"\x1b[".to_vec()]);
        assert!(framer.push(b"32;9u").is_empty());
        assert_eq!(framer.push(b"a"), vec![b"a".to_vec()]);

        // 但只剩一个终止字节的尾巴与用户键入的字符同形，只能作为文本送出。
        let mut framer = RawInputByteFramer::default();
        assert!(framer.push(b"\x1b[").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert_eq!(framer.flush_timeout(), vec![b"\x1b[".to_vec()]);
        assert_eq!(framer.push(b"A"), vec![b"A".to_vec()]);
    }

    #[test]
    fn orphaned_csi_tail_after_lone_escape_timeout_is_discarded_not_typed() {
        // #4356 的原始症状：kitty 序列恰在 ESC 后 0 字节处被切断，孤立 ESC 送出后
        // `[32;9u` 一类尾巴不得逐字节变成文本。
        for tail in [
            b"[32;9u".as_slice(),
            b"[108:76;2:1u".as_slice(),
            b"[1;1:3A".as_slice(),
            b"[27;6;108~".as_slice(),
            b"[A".as_slice(),
        ] {
            let mut framer = RawInputFramer::default();
            assert!(framer.push(b"\x1b").is_empty());
            let timed_out = framer.flush_timeout();
            assert_eq!(timed_out.len(), 1, "tail: {tail:?}");
            assert_raw_key(
                timed_out.into_iter().next().unwrap(),
                KeyCode::Esc,
                KeyModifiers::empty(),
            );
            assert!(framer.push(tail).is_empty(), "tail: {tail:?}");
            let mut events = framer.push(b"x");
            assert_eq!(events.len(), 1, "tail: {tail:?}");
            assert_raw_key(events.remove(0), KeyCode::Char('x'), KeyModifiers::empty());
        }
    }

    #[test]
    fn orphaned_csi_tail_that_is_not_a_known_sequence_stays_text() {
        // 拼回头部也解析不出已知事件的字节不是被切断的序列尾巴，必须原样交给 pane。
        let mut framer = RawInputFramer::default();
        assert!(framer.push(b"\x1b").is_empty());
        assert_eq!(framer.flush_timeout().len(), 1);
        let events = framer.push(b"[<x");
        assert_eq!(events.len(), 3, "{events:?}");

        // 用户在 Esc 之后键入 `[`：单个 `[` 先被等一个窗口，随后作为文本送出，
        // 尾巴武装也随之解除，下一个字符不受影响。
        let mut framer = RawInputByteFramer::default();
        assert!(framer.push(b"\x1b").is_empty());
        assert_eq!(framer.flush_timeout(), vec![b"\x1b".to_vec()]);
        assert!(framer.push(b"[").is_empty());
        assert_eq!(framer.flush_timeout(), vec![b"[".to_vec()]);
        assert_eq!(framer.push(b"3"), vec![b"3".to_vec()]);
    }

    #[test]
    fn timed_out_incomplete_csi_releases_a_held_host_reply_escape() {
        // 不完整 CSI 不产生事件，`drain_available_chunks` 的复位不会执行：新分支
        // 必须自己放掉 hold 标志，否则下一个孤立 ESC 会跳过本该有的 hold 并清空
        // 等待计数，一次本该被 hold 的主机回复 ESC 变成打进 pane 的 Esc。
        // 对照：hold 一轮后到期，孤立 ESC 照常送出。
        let mut framer = RawInputByteFramer::default();
        framer.host_color_query_sent();
        assert!(framer.push(b"\x1b").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert_eq!(framer.flush_timeout(), vec![b"\x1b".to_vec()]);

        let mut framer = RawInputByteFramer::default();
        framer.host_color_query_sent();
        assert!(framer.push(b"\x1b").is_empty());
        assert!(framer.flush_timeout().is_empty()); // hold 一轮
        assert!(framer.push(b"[32;9").is_empty());
        assert!(framer.flush_timeout().is_empty()); // 不完整 CSI 武装尾字节丢弃
        assert!(framer.push(b"\x1b").is_empty());
        assert!(
            framer.flush_timeout().is_empty(),
            "held flag must be released so the next lone escape is held again"
        );
        assert_eq!(framer.flush_timeout(), vec![b"\x1b".to_vec()]);
    }

    #[test]
    fn recovered_split_mouse_report_counts_as_mouse_report_evidence() {
        let window = std::time::Duration::from_millis(500);

        // 被切断后按前缀重组的报文。
        let mut framer = RawInputByteFramer::default();
        assert!(framer.push(b"\x1b[<35;2").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert!(framer.push(b";3M").is_empty());
        assert!(
            framer.mouse_report_seen_within(std::time::Instant::now(), window),
            "recovered report must refresh the evidence timestamp"
        );

        // 孤立 ESC 送出后被吃掉的鼠标尾巴。
        let mut framer = RawInputByteFramer::default();
        assert!(framer.push(b"\x1b").is_empty());
        assert_eq!(framer.flush_timeout(), vec![b"\x1b".to_vec()]);
        assert!(framer.push(b"[<35;2;3M").is_empty());
        assert!(
            framer.mouse_report_seen_within(std::time::Instant::now(), window),
            "discarded orphaned report must refresh the evidence timestamp"
        );
    }

    #[test]
    fn pending_incomplete_csi_accessors_distinguish_the_bare_intro() {
        let mut bare = RawInputByteFramer::default();
        assert!(bare.push(b"\x1b[").is_empty());
        assert!(bare.has_pending_csi_intro());
        assert!(!bare.has_pending_incomplete_csi());

        let mut key = RawInputByteFramer::default();
        assert!(key.push(b"\x1b[49:33;2:").is_empty());
        assert!(key.has_pending_incomplete_csi());
        assert!(!key.has_pending_csi_intro());

        let mut mouse = RawInputByteFramer::default();
        assert!(mouse.push(b"\x1b[<35;2").is_empty());
        assert!(mouse.has_pending_incomplete_csi());

        let mut escape = RawInputByteFramer::default();
        assert!(escape.push(b"\x1b").is_empty());
        assert!(!escape.has_pending_incomplete_csi());
        assert!(!escape.has_pending_csi_intro());

        let mut osc = RawInputByteFramer::default();
        assert!(osc.push(b"\x1b]11;").is_empty());
        assert!(!osc.has_pending_incomplete_csi());
        assert!(!osc.has_pending_csi_intro());

        let mut paste = RawInputByteFramer::default();
        assert!(paste.push(b"\x1b[200~partial").is_empty());
        assert!(!paste.has_pending_incomplete_csi());
    }

    #[test]
    fn mouse_report_evidence_expires_and_ignores_keys() {
        let window = std::time::Duration::from_millis(500);
        let mut framer = RawInputByteFramer::default();
        assert!(!framer.mouse_report_seen_within(std::time::Instant::now(), window));

        assert_eq!(framer.push(b"\x1b[A").len(), 1);
        assert!(!framer.mouse_report_seen_within(std::time::Instant::now(), window));

        assert_eq!(framer.push(b"\x1b[<35;2;3M").len(), 1);
        let now = std::time::Instant::now();
        assert!(framer.mouse_report_seen_within(now, window));
        assert!(!framer.mouse_report_seen_within(now + std::time::Duration::from_secs(1), window));

        let mut default_mouse = RawInputByteFramer::default();
        assert_eq!(default_mouse.push(b"\x1b[M !!").len(), 1);
        assert!(default_mouse.mouse_report_seen_within(std::time::Instant::now(), window));
    }

    #[test]
    fn keyboard_corpus_split_at_every_byte_boundary_never_leaks_text() {
        // 语料中每条多字节序列在每个字节边界切成两次 push，全部切分点都要满足
        // 「产生 KeyCode::Char 的切分数为 0」。三种超时时序的语义各不相同：
        // (a) 无空闲：原样重组为语料期望的按键；
        // (b) 切分点 1（缓冲区只有孤立 ESC）：ESC 作为 Esc 键送出（无法追回），
        //     随后到达的尾巴被吃掉，既不泄漏为文本也不吃掉后续按键；
        // (c) 切分点 2（裸 `ESC [`）：引导符被多等一个空闲窗口，尾巴到达后整条
        //     序列仍然重组为期望按键；
        // (d) 切分点 ≥3（前缀已含参数字节）：整条序列静默丢弃。
        // 切分点 2 连续两个空窗口后按 Alt+[ 送出的分支见
        // `alt_bracket_then_orphaned_parameter_tail_is_discarded_not_typed`。
        let corpus = include_str!("../tests/fixtures/keyboard_protocol_corpus.tsv");
        let mut covered = [0usize; 4];
        for line in corpus.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let columns: Vec<_> = line.split('\t').collect();
            let bytes = decode_hex(columns[1]);
            if bytes.len() < 2 {
                continue;
            }
            let expected_code = parse_fixture_key_code(columns[2]);
            let expected_modifiers = parse_fixture_modifiers(columns[3]);

            for split in 1..bytes.len() {
                let (head, tail) = bytes.split_at(split);

                let mut framer = RawInputFramer::default();
                assert!(framer.push(head).is_empty(), "{line} split {split}");
                let mut events = framer.push(tail);
                events.extend(framer.flush_timeout());
                assert_eq!(events.len(), 1, "{line} split {split}: {events:?}");
                assert_raw_key(events.remove(0), expected_code, expected_modifiers);
                covered[0] += 1;

                let mut framer = RawInputFramer::default();
                assert!(framer.push(head).is_empty(), "{line} split {split}");
                let timed_out = framer.flush_timeout();
                match split {
                    1 => {
                        assert_eq!(timed_out.len(), 1, "{line} split {split}: {timed_out:?}");
                        assert_raw_key(
                            timed_out.into_iter().next().unwrap(),
                            KeyCode::Esc,
                            KeyModifiers::empty(),
                        );
                        for event in framer.push(tail) {
                            assert!(
                                !matches!(
                                    event,
                                    RawInputEvent::Key(ref key)
                                        if matches!(key.code, KeyCode::Char(_))
                                ),
                                "{line} split {split}: tail leaked as text {event:?}"
                            );
                        }
                        covered[1] += 1;
                    }
                    2 => {
                        assert!(
                            timed_out.is_empty(),
                            "{line} split {split}: bare CSI intro must be held one extra window, got {timed_out:?}"
                        );
                        let mut events = framer.push(tail);
                        events.extend(framer.flush_timeout());
                        assert_eq!(events.len(), 1, "{line} split {split}: {events:?}");
                        assert_raw_key(events.remove(0), expected_code, expected_modifiers);
                        covered[2] += 1;
                    }
                    _ => {
                        assert!(
                            timed_out.is_empty(),
                            "{line} split {split}: timed-out prefix produced {timed_out:?}"
                        );
                        let leaked = framer.push(tail);
                        assert!(
                            leaked.is_empty(),
                            "{line} split {split}: tail leaked as {leaked:?}"
                        );
                        covered[3] += 1;
                    }
                }

                // 无论走哪一支，紧随其后的正常按键都必须完整到达。
                let mut events = framer.push(b"x");
                assert_eq!(events.len(), 1, "{line} split {split}: {events:?}");
                assert_raw_key(events.remove(0), KeyCode::Char('x'), KeyModifiers::empty());
                assert!(framer.flush_timeout().is_empty(), "{line} split {split}");
            }
        }
        assert!(
            covered.iter().all(|count| *count > 0),
            "every timing branch must be exercised: {covered:?}"
        );
    }

    #[test]
    fn chunked_bracketed_paste_waits_for_terminator() {
        let mut framer = RawInputFramer::default();

        assert!(framer.push(b"\x1b[200~hello").is_empty());
        let events = framer.push(b"\x1b[201~");
        assert_eq!(events.len(), 1);
        let RawInputEvent::Paste(text) = &events[0] else {
            panic!("expected paste");
        };
        assert_eq!(text, "hello");
    }

    #[test]
    fn incomplete_bracketed_paste_is_not_flushed_on_timeout() {
        let mut framer = RawInputFramer::default();

        assert!(framer.push(b"\x1b[200~hello\nworld").is_empty());
        assert!(framer.flush_timeout().is_empty());
        let events = framer.push(b"\x1b[201~");
        assert_eq!(events.len(), 1);
        let RawInputEvent::Paste(text) = &events[0] else {
            panic!("expected paste");
        };
        assert_eq!(text, "hello\nworld");
    }

    #[test]
    fn complete_utf8_char_before_incomplete_char_is_drained() {
        let mut framer = RawInputByteFramer::default();
        let mut input = "你".as_bytes().to_vec();
        input.push("好".as_bytes()[0]);

        assert_eq!(framer.push(&input), vec!["你".as_bytes().to_vec()]);
        assert_eq!(framer.push(&[]), Vec::<Vec<u8>>::new());
    }

    #[test]
    fn incomplete_utf8_prefix_is_not_flushed_on_timeout() {
        let mut framer = RawInputByteFramer::default();
        let prefix = &"好".as_bytes()[..1];

        assert!(framer.push(prefix).is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert_eq!(
            framer.push(&"好".as_bytes()[1..]),
            vec!["好".as_bytes().to_vec()]
        );
    }

    #[test]
    fn invalid_utf8_lead_byte_is_flushed_instead_of_buffered_forever() {
        let mut framer = RawInputByteFramer::default();

        assert!(framer.push(&[0xC0]).is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert!(!framer.has_pending_input());
    }

    #[test]
    fn complete_utf8_char_before_incomplete_char_survives_timeout_and_next_chunk() {
        let mut framer = RawInputByteFramer::default();
        let mut input = "你".as_bytes().to_vec();
        input.push("好".as_bytes()[0]);

        assert_eq!(framer.push(&input), vec!["你".as_bytes().to_vec()]);
        assert!(framer.flush_timeout().is_empty());
        assert_eq!(
            framer.push(&"好".as_bytes()[1..]),
            vec!["好".as_bytes().to_vec()]
        );
    }

    #[test]
    fn alt_utf8_char_drains_as_one_event_before_following_input() {
        let events = parse_raw_input_bytes_sync("\x1béx".as_bytes());
        assert_eq!(events.len(), 2);
        assert_raw_key(
            events.into_iter().next().unwrap(),
            KeyCode::Char('é'),
            KeyModifiers::ALT,
        );
    }

    #[test]
    fn chunked_alt_utf8_waits_for_continuation_byte_after_escape() {
        let mut framer = RawInputFramer::default();
        let bytes = "\x1bé".as_bytes();

        assert!(framer.push(&bytes[..2]).is_empty());
        assert!(framer.flush_timeout().is_empty());
        let events = framer.push(&bytes[2..]);
        assert_eq!(events.len(), 1);
        assert_raw_key(
            events.into_iter().next().unwrap(),
            KeyCode::Char('é'),
            KeyModifiers::ALT,
        );
    }

    #[test]
    fn chunked_utf8_waits_for_continuation_byte() {
        let mut framer = RawInputFramer::default();

        assert!(framer.push(&"é".as_bytes()[..1]).is_empty());
        let events = framer.push(&"é".as_bytes()[1..]);
        assert_eq!(events.len(), 1);
        assert_raw_key(
            events.into_iter().next().unwrap(),
            KeyCode::Char('é'),
            KeyModifiers::empty(),
        );
    }

    #[test]
    fn chunked_cjk_utf8_waits_for_all_continuation_bytes() {
        let mut framer = RawInputFramer::default();
        let bytes = "好".as_bytes();

        assert!(framer.push(&bytes[..1]).is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert!(framer.push(&bytes[1..2]).is_empty());
        assert!(framer.flush_timeout().is_empty());
        let events = framer.push(&bytes[2..]);
        assert_eq!(events.len(), 1);
        assert_raw_key(
            events.into_iter().next().unwrap(),
            KeyCode::Char('好'),
            KeyModifiers::empty(),
        );
    }

    #[test]
    fn chunked_four_byte_utf8_waits_for_all_continuation_bytes() {
        let mut framer = RawInputFramer::default();
        let bytes = "🙂".as_bytes();

        for split in 1..bytes.len() {
            assert!(framer.push(&bytes[split - 1..split]).is_empty());
            assert!(framer.flush_timeout().is_empty());
        }

        let events = framer.push(&bytes[bytes.len() - 1..]);
        assert_eq!(events.len(), 1);
        assert_raw_key(
            events.into_iter().next().unwrap(),
            KeyCode::Char('🙂'),
            KeyModifiers::empty(),
        );
    }

    #[test]
    fn long_multilingual_voice_like_burst_drains_without_truncation() {
        let text = "你好，今天我们测试一段比较长的语音输入。こんにちは。안녕하세요.🙂".repeat(128);
        assert!(
            text.len() > 4096,
            "test input should exceed the client read buffer"
        );
        let mut framer = RawInputByteFramer::default();

        let chunks = framer.push(text.as_bytes());
        let rebuilt: Vec<u8> = chunks.into_iter().flatten().collect();

        assert!(!framer.has_pending_input());
        assert_eq!(rebuilt, text.as_bytes());
    }

    #[test]
    fn long_multilingual_burst_survives_one_byte_chunks_and_timeouts() {
        let text = "中文かなカナ한글🙂，。".repeat(64);
        let mut framer = RawInputByteFramer::default();
        let mut rebuilt = Vec::new();

        for byte in text.as_bytes() {
            rebuilt.extend(
                framer
                    .push(std::slice::from_ref(byte))
                    .into_iter()
                    .flatten(),
            );
            if framer.has_pending_input() {
                assert!(framer.flush_timeout().is_empty());
            }
        }

        rebuilt.extend(framer.flush_timeout().into_iter().flatten());
        assert!(!framer.has_pending_input());
        assert_eq!(rebuilt, text.as_bytes());
    }

    #[test]
    fn parses_ghostty_default_background_response() {
        let events = parse_raw_input_bytes_sync(b"\x1b]11;rgb:2828/2a2a/3636\x07");

        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            RawInputEvent::HostDefaultColor {
                kind: DefaultColorKind::Background,
                color: RgbColor {
                    r: 0x28,
                    g: 0x2a,
                    b: 0x36
                }
            }
        ));
    }

    #[test]
    fn raw_input_framer_reassembles_split_default_background_response() {
        let mut framer = RawInputFramer::default();

        assert!(framer.push(b"\x1b]").is_empty());
        let events = framer.push(b"11;#123456\x07");

        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            RawInputEvent::HostDefaultColor {
                kind: DefaultColorKind::Background,
                color: RgbColor {
                    r: 0x12,
                    g: 0x34,
                    b: 0x56,
                }
            }
        ));
    }

    #[test]
    fn raw_input_byte_framer_discards_split_control_string_after_timeout() {
        let mut framer = RawInputByteFramer::default();

        assert!(framer.push(b"\x1b]").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert!(framer.push(b"11;#123456\x07").is_empty());
        assert_eq!(framer.push(b"a"), vec![b"a".to_vec()]);
    }

    #[test]
    fn raw_input_byte_framer_keeps_discarding_tail_across_timeout() {
        let mut framer = RawInputByteFramer::default();

        assert!(framer.push(b"\x1b]").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert!(framer.push(b"1").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert!(framer.push(b"1;#123456\x07").is_empty());
        assert_eq!(framer.push(b"a"), vec![b"a".to_vec()]);
    }

    #[test]
    fn raw_input_byte_framer_releases_discard_on_implausible_tail() {
        let mut framer = RawInputByteFramer::default();

        assert!(framer.push(b"\x1b]").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert!(framer.push(b"a").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert_eq!(framer.push(b"b"), vec![b"b".to_vec()]);
    }

    #[test]
    fn parse_raw_input_bytes_sync_does_not_parse_incomplete_strings_as_alt_keys() {
        for bytes in [
            b"\x1b]".as_slice(),
            b"\x1bP".as_slice(),
            b"\x1b_".as_slice(),
            b"\x1b^".as_slice(),
            b"\x1bX".as_slice(),
        ] {
            let events = parse_raw_input_bytes_sync(bytes);

            assert!(events.is_empty(), "parsed {bytes:?} as {events:?}");
        }
    }

    #[test]
    fn non_osc_control_strings_ignore_bel_and_complete_at_st() {
        let bytes = b"\x1bPabc\x07def\x1b\\x";

        let (event, consumed) = extract_one_event(bytes).unwrap();

        assert!(matches!(event, RawInputEvent::Unsupported));
        assert_eq!(consumed, b"\x1bPabc\x07def\x1b\\".len());
    }

    #[test]
    fn non_osc_default_color_text_remains_key_input() {
        let events = parse_raw_input_bytes_sync(b"11;rgb:2828/2a2a/3636\x07");

        assert_eq!(events.len(), 22);
        assert_raw_key(
            events.into_iter().next().unwrap(),
            KeyCode::Char('1'),
            KeyModifiers::empty(),
        );
    }

    #[test]
    fn byte_framer_does_not_hold_non_osc_default_color_text() {
        let mut framer = RawInputByteFramer::default();

        let chunks = framer.push(b"11;rgb:2828");

        assert_eq!(chunks.len(), 11);
        assert!(framer.flush_timeout().is_empty());
    }

    #[test]
    fn holds_lone_escape_and_stitches_split_host_color_reply() {
        let mut framer = RawInputByteFramer::default();
        framer.host_color_query_sent();

        // The reply is split right at its ESC introducer.
        assert!(framer.push(b"\x1b").is_empty());
        // The idle flush must not release the ESC as an Escape key while a host
        // color reply is still outstanding.
        assert!(framer.flush_timeout().is_empty());

        // The rest of the OSC 11 reply arrives and stitches back together
        // instead of leaking its payload into the focused pane.
        let chunks = framer.push(b"]11;rgb:2424/2727/3a3a\x1b\\");
        assert_eq!(chunks.len(), 1);
        let (event, _) = extract_one_event(&chunks[0]).unwrap();
        assert!(matches!(
            event,
            RawInputEvent::HostDefaultColor {
                kind: DefaultColorKind::Background,
                ..
            }
        ));
    }

    #[test]
    fn holds_lone_escape_and_stitches_split_host_cell_size_reply() {
        let mut framer = RawInputByteFramer::default();
        framer.host_cell_size_query_sent();

        // The XTWINOPS reply is split right at its ESC introducer.
        assert!(framer.push(b"\x1b").is_empty());
        assert!(framer.flush_timeout().is_empty());

        let chunks = framer.push(b"[6;21;10t");
        assert_eq!(chunks, vec![b"\x1b[6;21;10t".to_vec()]);
        let (event, _) = extract_one_event(&chunks[0]).unwrap();
        assert!(matches!(
            event,
            RawInputEvent::HostCellSizeReport {
                width_px: 10,
                height_px: 21,
            }
        ));
    }

    #[test]
    fn timed_out_host_cell_size_reply_fragments_do_not_leak() {
        for (prefix, tail) in [
            (b"\x1b[6".as_slice(), b";21;10t".as_slice()),
            (b"\x1b[6;".as_slice(), b"21;10t".as_slice()),
            (b"\x1b[6;21;".as_slice(), b"10t".as_slice()),
        ] {
            let mut framer = RawInputByteFramer::default();
            framer.host_cell_size_query_sent();

            assert!(framer.push(prefix).is_empty(), "prefix: {prefix:?}");
            assert!(framer.flush_timeout().is_empty(), "prefix: {prefix:?}");
            assert!(framer.push(tail).is_empty(), "tail: {tail:?}");
            assert_eq!(framer.push(b"a"), vec![b"a".to_vec()]);
        }
    }

    #[test]
    fn split_host_cell_size_reply_after_csi_intro_gets_one_more_flush() {
        let mut framer = RawInputByteFramer::default();
        framer.host_cell_size_query_sent();

        assert!(framer.push(b"\x1b[").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert_eq!(framer.push(b"6;21;10t"), vec![b"\x1b[6;21;10t".to_vec()]);

        let mut alt_bracket = RawInputByteFramer::default();
        alt_bracket.host_cell_size_query_sent();
        assert!(alt_bracket.push(b"\x1b[").is_empty());
        assert!(alt_bracket.flush_timeout().is_empty());
        assert_eq!(alt_bracket.flush_timeout(), vec![b"\x1b[".to_vec()]);
    }

    #[test]
    fn malformed_host_reply_tail_preserves_following_input() {
        let mut framer = RawInputByteFramer::default();
        framer.host_cell_size_query_sent();

        assert!(framer.push(b"\x1b[6;21").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert_eq!(
            framer.push(b";10xabc"),
            vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]
        );
    }

    #[test]
    fn host_reply_tail_discard_is_bounded_across_pushes() {
        let mut framer = RawInputByteFramer::default();
        framer.host_cell_size_query_sent();

        assert!(framer.push(b"\x1b[6;21").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert!(framer.push(&[b'1'; 64]).is_empty());
        assert_eq!(
            framer.push(&[b'2'; 67]),
            vec![b"2".to_vec(), b"2".to_vec(), b"2".to_vec()]
        );
    }

    #[test]
    fn stops_holding_lone_escape_after_host_cell_size_reply_completes() {
        let mut framer = RawInputByteFramer::default();
        framer.host_cell_size_query_sent();

        assert_eq!(
            framer.push(b"\x1b[6;21;10t"),
            vec![b"\x1b[6;21;10t".to_vec()]
        );

        // Window closed: a later lone Escape flushes immediately.
        assert!(framer.push(b"\x1b").is_empty());
        assert_eq!(framer.flush_timeout(), vec![b"\x1b".to_vec()]);
    }

    #[test]
    fn default_byte_framer_does_not_rearm_after_color_scheme_report() {
        let mut framer = RawInputByteFramer::default();

        assert_eq!(
            framer.push(GHOSTTY_COLOR_SCHEME_DARK_REPORT),
            vec![GHOSTTY_COLOR_SCHEME_DARK_REPORT.to_vec()]
        );
        assert!(framer.push(b"\x1b").is_empty());
        assert_eq!(framer.flush_timeout(), vec![b"\x1b".to_vec()]);
    }

    #[test]
    fn opt_in_does_not_delay_plain_escape_without_color_scheme_report() {
        let mut framer = RawInputByteFramer::default();
        framer.enable_host_color_scheme_change_tracking();

        assert!(framer.push(b"\x1b").is_empty());
        assert_eq!(framer.flush_timeout(), vec![b"\x1b".to_vec()]);
    }

    #[test]
    fn opted_in_byte_framer_rearms_after_outer_focus_gained() {
        let mut framer = RawInputByteFramer::default();
        framer.enable_host_color_scheme_change_tracking();
        framer.enable_host_appearance_query_on_focus();

        assert_eq!(framer.push(b"\x1b[I"), vec![b"\x1b[I".to_vec()]);
        assert!(framer.push(b"\x1b").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert_eq!(
            framer.push(b"[?997;2n"),
            vec![GHOSTTY_COLOR_SCHEME_LIGHT_REPORT.to_vec()]
        );
    }

    #[test]
    fn opted_in_byte_framer_reassembles_appearance_reply_split_after_csi() {
        let mut framer = RawInputByteFramer::default();
        framer.enable_host_color_scheme_change_tracking();
        framer.enable_host_appearance_query_on_focus();

        assert_eq!(framer.push(b"\x1b[I"), vec![b"\x1b[I".to_vec()]);
        assert!(framer.push(b"\x1b[").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert_eq!(
            framer.push(b"?997;2n"),
            vec![GHOSTTY_COLOR_SCHEME_LIGHT_REPORT.to_vec()]
        );
    }

    #[test]
    fn opted_in_byte_framer_reassembles_delayed_appearance_reply() {
        let mut framer = RawInputByteFramer::default();
        framer.enable_host_color_scheme_change_tracking();
        framer.enable_host_appearance_query_on_focus();

        assert_eq!(framer.push(b"\x1b[I"), vec![b"\x1b[I".to_vec()]);
        assert!(framer.push(b"\x1b[?997;").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert_eq!(
            framer.push(b"2n"),
            vec![GHOSTTY_COLOR_SCHEME_LIGHT_REPORT.to_vec()]
        );
    }

    #[test]
    fn timed_out_appearance_reply_preserves_pending_color_reply_window() {
        let mut framer = RawInputByteFramer::default();
        framer.host_color_query_sent();
        framer.enable_host_appearance_query_on_focus();

        assert_eq!(framer.push(b"\x1b[I"), vec![b"\x1b[I".to_vec()]);
        assert!(framer.push(b"\x1b[?997;").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert!(framer.push(b"2n").is_empty());

        assert!(framer.push(b"\x1b").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert_eq!(
            framer.push(b"]10;rgb:aaaa/bbbb/cccc\x1b\\"),
            vec![b"\x1b]10;rgb:aaaa/bbbb/cccc\x1b\\".to_vec()]
        );
    }

    #[test]
    fn disabled_focus_query_does_not_rearm_byte_framer() {
        let mut framer = RawInputByteFramer::default();
        framer.enable_host_color_scheme_change_tracking();

        assert_eq!(framer.push(b"\x1b[I"), vec![b"\x1b[I".to_vec()]);
        assert!(framer.push(b"\x1b").is_empty());
        assert_eq!(framer.flush_timeout(), vec![b"\x1b".to_vec()]);
    }

    #[test]
    fn focus_query_policy_does_not_delay_plain_escape_without_focus() {
        let mut framer = RawInputByteFramer::default();
        framer.enable_host_appearance_query_on_focus();

        assert!(framer.push(b"\x1b").is_empty());
        assert_eq!(framer.flush_timeout(), vec![b"\x1b".to_vec()]);
    }

    #[test]
    fn focus_query_without_reply_holds_escape_for_only_one_flush() {
        let mut framer = RawInputByteFramer::default();
        framer.enable_host_appearance_query_on_focus();

        assert_eq!(framer.push(b"\x1b[I"), vec![b"\x1b[I".to_vec()]);
        assert!(framer.push(b"\x1b").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert_eq!(framer.flush_timeout(), vec![b"\x1b".to_vec()]);
    }

    #[test]
    fn opted_in_byte_framer_rearms_after_color_scheme_report() {
        let mut framer = RawInputByteFramer::default();
        framer.enable_host_color_scheme_change_tracking();

        assert_eq!(
            framer.push(GHOSTTY_COLOR_SCHEME_DARK_REPORT),
            vec![GHOSTTY_COLOR_SCHEME_DARK_REPORT.to_vec()]
        );

        assert!(framer.push(b"\x1b").is_empty());
        assert!(framer.flush_timeout().is_empty());
        let chunks = framer.push(b"]10;#abcdef\x07");
        assert_eq!(chunks.len(), 1);
        let (event, _) = extract_one_event(&chunks[0]).unwrap();
        assert!(matches!(
            event,
            RawInputEvent::HostDefaultColor {
                kind: DefaultColorKind::Foreground,
                color: RgbColor {
                    r: 0xab,
                    g: 0xcd,
                    b: 0xef
                }
            }
        ));

        assert!(framer.push(b"\x1b").is_empty());
        assert!(framer.flush_timeout().is_empty());
        let chunks = framer.push(b"]11;#123456\x07");
        assert_eq!(chunks.len(), 1);
        let (event, _) = extract_one_event(&chunks[0]).unwrap();
        assert!(matches!(
            event,
            RawInputEvent::HostDefaultColor {
                kind: DefaultColorKind::Background,
                color: RgbColor {
                    r: 0x12,
                    g: 0x34,
                    b: 0x56
                }
            }
        ));

        assert!(framer.push(b"\x1b").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert_eq!(framer.flush_timeout(), vec![b"\x1b".to_vec()]);
    }

    #[test]
    fn flushes_lone_escape_when_not_awaiting_host_color_reply() {
        let mut framer = RawInputByteFramer::default();

        assert!(framer.push(b"\x1b").is_empty());
        assert_eq!(framer.flush_timeout(), vec![b"\x1b".to_vec()]);
    }

    #[test]
    fn gives_up_holding_lone_escape_after_one_idle_flush() {
        let mut framer = RawInputByteFramer::default();
        framer.host_color_query_sent();

        assert!(framer.push(b"\x1b").is_empty());
        // First idle flush holds the escape.
        assert!(framer.flush_timeout().is_empty());
        // No continuation arrived; the second idle flush releases it as Escape.
        assert_eq!(framer.flush_timeout(), vec![b"\x1b".to_vec()]);
    }

    #[test]
    fn stops_holding_lone_escape_after_host_color_reply_completes() {
        use std::fmt::Write as _;

        let mut framer = RawInputByteFramer::default();
        framer.host_color_query_sent();
        let mut replies =
            String::from("\x1b]10;rgb:6565/7b7b/8383\x1b\\\x1b]11;rgb:2424/2727/3a3a\x1b\\");
        for index in 0..=u8::MAX {
            let _ = write!(replies, "\x1b]4;{index};rgb:1111/2222/3333\x1b\\");
        }

        let chunks = framer.push(replies.as_bytes());
        assert_eq!(chunks.len(), 258);

        // Window closed: a later lone Escape flushes immediately.
        assert!(framer.push(b"\x1b").is_empty());
        assert_eq!(framer.flush_timeout(), vec![b"\x1b".to_vec()]);
    }
}
