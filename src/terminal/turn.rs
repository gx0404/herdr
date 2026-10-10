//! 回合计时、待处理事项（失败锁存与 needs-input 载荷）和用户中断标记。状态仲裁仍只在
//! `TerminalState::recompute_effective_state`：有效状态变化时它调
//! [`TurnTracker::on_state_change`] 记账，这里不决定状态。

use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::api::schema::{AgentAttention, AgentAttentionKind};
use crate::detect::AgentState;

/// 用户单独发 Ctrl-C / ESC 后，多久之内的完成迁移算被用户中断（不发「已完成」通知）。
pub(crate) const INTERRUPT_COMPLETION_WINDOW: Duration = Duration::from_millis(1500);

/// 钩子随受阻状态带来的说明在 needs-input 详情里的字符上限。
const HOOK_MESSAGE_MAX_CHARS: usize = 2000;

/// 墙钟毫秒时间戳（对外的 `*_ms` 字段用；时钟早于纪元时取 0）。
pub(crate) fn wall_clock_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

/// 失败锁存：上一回合失败，直到用户看过、重新开始工作、agent 被释放或取代、或集成清除。
#[derive(Debug, Clone, PartialEq, Eq)]
struct FailureLatch {
    attention: AgentAttention,
    /// 锁存时持有 pane 的 agent；换成别的 agent 时作废。
    agent_label: Option<String>,
    /// 还没发过通知（Working 期间锁存的失败等这一回合结束再通知）。
    notice_pending: bool,
}

/// 一个终端的回合记账。纯数据，无 PTY 可测。
#[derive(Debug, Default)]
pub(crate) struct TurnTracker {
    working_since_ms: Option<u64>,
    last_turn_ms: Option<u64>,
    state_changed_at_ms: Option<u64>,
    /// 最近一次进入 Working 的时刻；离开 Working 后保留，判定会话错误是否属于本回合。
    last_working_start_ms: Option<u64>,
    blocked_since_ms: Option<u64>,
    failed_attention: Option<FailureLatch>,
    /// 已发出的失败通知次数（API 路径按前后差判断本次请求是否锁存了失败）。
    failure_notices: u64,
    /// 最近一次只带载荷的 needs_input 上报；状态离开 Blocked 时清掉。
    needs_input: Option<AgentAttention>,
    /// `pane.report_agent_attention` 的逐来源序号。
    report_sequences: HashMap<String, u64>,
    /// 本回合已处理过的会话错误（同一错误被看过后不再重复锁存）。
    observed_error: Option<String>,
    last_interrupt_input_at: Option<Instant>,
    last_completion_interrupted: bool,
    /// 待处理事项的修订号：变化即客户端投影要重建。
    revision: u64,
}

impl TurnTracker {
    /// 有效状态从 `previous` 变到 `state`（调用方保证两者不同）。
    pub(crate) fn on_state_change(
        &mut self,
        previous: AgentState,
        state: AgentState,
        now: Instant,
        now_ms: u64,
    ) {
        self.state_changed_at_ms = Some(now_ms);
        let completion = state == AgentState::Idle
            && matches!(previous, AgentState::Working | AgentState::Blocked);
        self.last_completion_interrupted = completion && self.interrupted_within(now);
        if state == AgentState::Working {
            self.working_since_ms = Some(now_ms);
            self.last_working_start_ms = Some(now_ms);
            // 中断与会话错误都只对当前回合有意义；重新开始工作也就不再「失败」。
            self.last_interrupt_input_at = None;
            self.observed_error = None;
            self.clear_failure();
        } else if previous == AgentState::Working {
            self.last_turn_ms = self
                .working_since_ms
                .take()
                .map(|since| now_ms.saturating_sub(since));
        }
        if state == AgentState::Blocked {
            self.blocked_since_ms = Some(now_ms);
        } else {
            self.blocked_since_ms = None;
            if self.needs_input.take().is_some() {
                self.bump();
            }
        }
    }

    pub(crate) fn working_since_ms(&self) -> Option<u64> {
        self.working_since_ms
    }

    pub(crate) fn last_turn_ms(&self) -> Option<u64> {
        self.last_turn_ms
    }

    pub(crate) fn state_changed_at_ms(&self) -> Option<u64> {
        self.state_changed_at_ms
    }

    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }

    pub(crate) fn note_interrupt_input(&mut self, now: Instant) {
        self.last_interrupt_input_at = Some(now);
    }

    #[cfg(test)]
    pub(crate) fn last_interrupt_input_at(&self) -> Option<Instant> {
        self.last_interrupt_input_at
    }

    /// `now` 之前 [`INTERRUPT_COMPLETION_WINDOW`] 内用户单独发过 Ctrl-C / ESC。观测时刻
    /// 早于中断时按 0 间隔算（迟到的屏幕观测不把中断挤出窗口）。
    pub(crate) fn interrupted_within(&self, now: Instant) -> bool {
        self.last_interrupt_input_at
            .is_some_and(|at| now.saturating_duration_since(at) <= INTERRUPT_COMPLETION_WINDOW)
    }

    /// 最近一次有效状态变化是否是紧跟用户中断的完成迁移。
    pub(crate) fn last_completion_interrupted(&self) -> bool {
        self.last_completion_interrupted
    }

    /// 锁存一次失败。同一失败重复上报不算变化（也不再通知）。
    pub(crate) fn latch_failure(
        &mut self,
        title: Option<String>,
        detail: Option<String>,
        agent_label: Option<&str>,
        now_ms: u64,
    ) -> bool {
        if self
            .failed_attention
            .as_ref()
            .is_some_and(|latch| latch.attention.title == title && latch.attention.detail == detail)
        {
            return false;
        }
        self.failed_attention = Some(FailureLatch {
            attention: AgentAttention {
                kind: AgentAttentionKind::Failed,
                title,
                detail,
                options: Vec::new(),
                since_ms: Some(now_ms),
            },
            agent_label: agent_label.map(str::to_owned),
            notice_pending: true,
        });
        self.bump();
        true
    }

    pub(crate) fn clear_failure(&mut self) -> bool {
        if self.failed_attention.take().is_none() {
            return false;
        }
        self.bump();
        true
    }

    /// 用户看过 pane：只确认已经通知过的失败——还没露面的失败谈不上被看过。
    pub(crate) fn acknowledge(&mut self) -> bool {
        if self
            .failed_attention
            .as_ref()
            .is_some_and(|latch| !latch.notice_pending)
        {
            return self.clear_failure();
        }
        false
    }

    /// 取走一次待发的失败通知：有尚未通知的失败、且不在 Working 时返回 true（Working
    /// 期间锁存的失败等这一回合结束再通知）。
    pub(crate) fn take_failure_notice(&mut self, state: AgentState) -> bool {
        if state == AgentState::Working {
            return false;
        }
        let Some(latch) = self
            .failed_attention
            .as_mut()
            .filter(|latch| latch.notice_pending)
        else {
            return false;
        };
        latch.notice_pending = false;
        self.failure_notices = self.failure_notices.wrapping_add(1);
        true
    }

    pub(crate) fn failure_notices(&self) -> u64 {
        self.failure_notices
    }

    /// agent 被释放：与它绑定的待处理事项一并作废。
    pub(crate) fn forget_agent(&mut self) {
        let failure = self.failed_attention.take().is_some();
        let needs_input = self.needs_input.take().is_some();
        self.observed_error = None;
        if failure || needs_input {
            self.bump();
        }
    }

    /// 有效 agent 换成了另一个：失败锁存属于原来的 agent 时作废。
    pub(crate) fn reconcile_agent(&mut self, agent_label: Option<&str>) {
        let replaced = match (
            self.failed_attention
                .as_ref()
                .and_then(|latch| latch.agent_label.as_deref()),
            agent_label,
        ) {
            (Some(latched), Some(current)) => latched != current,
            _ => false,
        };
        if replaced {
            self.clear_failure();
        }
    }

    /// 按来源取舍 `pane.report_agent_attention`，与生命周期上报同口径：带序号的只收比
    /// 已记下的更大的，不带序号的只在该来源还没收过带序号的上报时收。
    pub(crate) fn accept_report(&mut self, source: &str, seq: Option<u64>) -> bool {
        let last = self.report_sequences.get(source).copied();
        match seq {
            Some(seq) if last.is_none_or(|last| seq > last) => {
                self.report_sequences.insert(source.to_owned(), seq);
                true
            }
            Some(_) => false,
            None => last.is_none(),
        }
    }

    /// 只带载荷的 needs_input 上报（不改状态，只在 Blocked 时露面）。
    pub(crate) fn set_needs_input(
        &mut self,
        title: Option<String>,
        detail: Option<String>,
        options: Vec<String>,
        now_ms: u64,
    ) -> bool {
        if self.needs_input.as_ref().is_some_and(|current| {
            current.title == title && current.detail == detail && current.options == options
        }) {
            return false;
        }
        self.needs_input = Some(AgentAttention {
            kind: AgentAttentionKind::NeedsInput,
            title,
            detail,
            options,
            since_ms: Some(now_ms),
        });
        self.bump();
        true
    }

    /// 集成清除：失败锁存与 needs-input 载荷都清掉。
    pub(crate) fn clear_reported(&mut self) -> bool {
        let failure = self.failed_attention.take().is_some();
        let needs_input = self.needs_input.take().is_some();
        if failure || needs_input {
            self.bump();
        }
        failure || needs_input
    }

    /// 会话洞察带来的错误：不在 Working、不早于本回合开始、本回合还没处理过同一错误时
    /// 锁存失败。没有错误时刻的按现在算；本 server 从没见过这个 agent 工作时不锁存（启动
    /// 时读到的旧错误不算失败）。
    pub(crate) fn observe_error(
        &mut self,
        error: &str,
        at_ms: Option<u64>,
        state: AgentState,
        agent_label: Option<&str>,
        now_ms: u64,
    ) -> bool {
        if state == AgentState::Working {
            return false;
        }
        let Some(turn_start) = self.last_working_start_ms else {
            return false;
        };
        // 毫秒粒度：与回合开始同一毫秒的错误算本回合的。
        if at_ms.unwrap_or(now_ms) < turn_start || self.observed_error.as_deref() == Some(error) {
            return false;
        }
        self.observed_error = Some(error.to_owned());
        self.latch_failure(None, Some(error.to_owned()), agent_label, now_ms)
    }

    /// 对外的待处理事项：受阻时是 needs-input（集成上报的载荷优先，否则用钩子随受阻
    /// 状态带来的说明）；Working 时没有；其余时候是失败锁存。
    pub(crate) fn attention(
        &self,
        state: AgentState,
        hook_message: Option<&str>,
    ) -> Option<AgentAttention> {
        match state {
            AgentState::Blocked => {
                Some(self.needs_input.clone().unwrap_or_else(|| AgentAttention {
                    kind: AgentAttentionKind::NeedsInput,
                    title: None,
                    detail: hook_message.and_then(|message| {
                        crate::harness::text::clean_line(message, HOOK_MESSAGE_MAX_CHARS)
                    }),
                    options: Vec::new(),
                    since_ms: self.blocked_since_ms,
                }))
            }
            AgentState::Working => None,
            AgentState::Idle | AgentState::Unknown => self
                .failed_attention
                .as_ref()
                .map(|latch| latch.attention.clone()),
        }
    }

    fn bump(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracker_after_turn(start_ms: u64) -> TurnTracker {
        let mut tracker = TurnTracker::default();
        let now = Instant::now();
        tracker.on_state_change(AgentState::Idle, AgentState::Working, now, start_ms);
        tracker.on_state_change(AgentState::Working, AgentState::Idle, now, start_ms + 50);
        tracker
    }

    #[test]
    fn report_sequences_are_monotonic_per_source() {
        let mut tracker = TurnTracker::default();
        assert!(
            tracker.accept_report("herdr:kimi", None),
            "首个无序号上报可收"
        );
        assert!(tracker.accept_report("herdr:kimi", Some(5)));
        assert!(!tracker.accept_report("herdr:kimi", Some(5)), "重复序号");
        assert!(!tracker.accept_report("herdr:kimi", Some(4)), "旧序号");
        assert!(
            !tracker.accept_report("herdr:kimi", None),
            "已有序号后不收无序号"
        );
        assert!(tracker.accept_report("custom:bot", Some(1)), "来源各自计数");
        assert!(tracker.accept_report("herdr:kimi", Some(6)));
    }

    #[test]
    fn insight_error_latches_once_per_turn_and_only_after_the_turn_started() {
        let mut tracker = TurnTracker::default();
        assert!(
            !tracker.observe_error("boom", Some(10), AgentState::Idle, Some("codex"), 20),
            "没见过工作的 agent 不锁存旧错误"
        );
        let mut tracker = tracker_after_turn(100);
        assert!(!tracker.observe_error("old", Some(90), AgentState::Idle, None, 200));
        assert!(!tracker.observe_error("boom", Some(120), AgentState::Working, None, 200));
        assert!(tracker.observe_error("boom", Some(120), AgentState::Idle, Some("codex"), 200));
        assert!(tracker.take_failure_notice(AgentState::Idle));
        assert!(tracker.acknowledge(), "通知过的失败可被确认");
        assert!(
            !tracker.observe_error("boom", Some(120), AgentState::Idle, None, 300),
            "同一错误确认后不再锁存"
        );
        assert!(tracker.observe_error("other", None, AgentState::Idle, None, 300));
    }

    #[test]
    fn pending_failure_survives_acknowledgement_until_announced() {
        let mut tracker = TurnTracker::default();
        assert!(tracker.latch_failure(None, Some("x".into()), Some("pi"), 1));
        assert!(!tracker.acknowledge(), "还没通知的失败不能被看过");
        assert!(
            !tracker.take_failure_notice(AgentState::Working),
            "Working 时推迟"
        );
        assert!(tracker.take_failure_notice(AgentState::Idle));
        assert!(!tracker.take_failure_notice(AgentState::Idle), "只通知一次");
        assert!(
            !tracker.latch_failure(None, Some("x".into()), Some("pi"), 2),
            "同一失败重复上报不算变化"
        );
        assert!(tracker.acknowledge());
        assert_eq!(tracker.attention(AgentState::Idle, None), None);
    }

    #[test]
    fn replacement_agent_drops_the_previous_agents_failure() {
        let mut tracker = TurnTracker::default();
        tracker.latch_failure(None, None, Some("claude"), 1);
        tracker.reconcile_agent(None);
        tracker.reconcile_agent(Some("claude"));
        assert!(tracker.attention(AgentState::Idle, None).is_some());
        tracker.reconcile_agent(Some("codex"));
        assert!(tracker.attention(AgentState::Idle, None).is_none());
    }
}
