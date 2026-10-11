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
    /// 本回合已经发过失败通知。一个失败的回合只通知一次：同一回合里之后再报的失败
    /// （钩子报过之后会话洞察又读到同一个错误等）不再锁存，之后进入 Idle / Blocked 也不
    /// 再发「已完成」「需要处理」。进入 Working 开始新回合时清掉，agent 被释放或被别的
    /// agent 取代时也清掉。
    failure_announced: bool,
    /// 那次失败通知所属的 agent（锁存被用户看过、清掉之后仍要知道它属于谁）。
    announced_agent: Option<String>,
    /// 本回合已经发过「已完成」。一个回合只通知一次：之后才报来的失败（洞察读取超过
    /// 等待期限才读到会话里的错误、迟到的钩子上报）照常锁存、在待处理事项里露面，但
    /// 不再通知。清除时机同 `failure_announced`。
    completion_announced: bool,
    /// 那次「已完成」所属的 agent。
    completion_agent: Option<String>,
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
            self.forget_announced_failure();
            self.forget_announced_completion();
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

    /// 锁存一次失败。同一失败重复上报不算变化（也不再通知）；本回合已经通知过失败时
    /// 不再锁存；本回合已经发过「已完成」时只锁存、不再通知（用户看过即确认）。
    pub(crate) fn latch_failure(
        &mut self,
        title: Option<String>,
        detail: Option<String>,
        agent_label: Option<&str>,
        now_ms: u64,
    ) -> bool {
        if self.failure_announced
            || self.failed_attention.as_ref().is_some_and(|latch| {
                latch.attention.title == title && latch.attention.detail == detail
            })
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
            notice_pending: !self.completion_announced && !self.last_completion_interrupted,
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
        self.failure_announced = true;
        self.announced_agent = latch.agent_label.clone();
        true
    }

    pub(crate) fn failure_notices(&self) -> u64 {
        self.failure_notices
    }

    /// 本回合已经发过失败通知（规则见同名字段）。
    pub(crate) fn failure_announced(&self) -> bool {
        self.failure_announced
    }

    fn forget_announced_failure(&mut self) {
        self.failure_announced = false;
        self.announced_agent = None;
    }

    /// 有锁存的失败（不论是否已通知）。字段读取、不分配。
    pub(crate) fn has_failure(&self) -> bool {
        self.failed_attention.is_some()
    }

    /// 记下本回合发过「已完成」（规则见 `completion_announced` 字段）。
    pub(crate) fn note_completion_announced(&mut self, agent_label: Option<&str>) {
        self.completion_announced = true;
        self.completion_agent = agent_label.map(str::to_owned);
    }

    /// 本回合已经发过「已完成」。
    pub(crate) fn completion_announced(&self) -> bool {
        self.completion_announced
    }

    fn forget_announced_completion(&mut self) {
        self.completion_announced = false;
        self.completion_agent = None;
    }

    /// agent 被释放：与它绑定的待处理事项一并作废。
    pub(crate) fn forget_agent(&mut self) {
        let failure = self.failed_attention.take().is_some();
        let needs_input = self.needs_input.take().is_some();
        self.observed_error = None;
        self.forget_announced_failure();
        self.forget_announced_completion();
        if failure || needs_input {
            self.bump();
        }
    }

    /// 有效 agent 换成了另一个：失败锁存与「本回合已通知过失败」属于原来的 agent 时作废。
    pub(crate) fn reconcile_agent(&mut self, agent_label: Option<&str>) {
        let Some(current) = agent_label else {
            return;
        };
        if self
            .failed_attention
            .as_ref()
            .and_then(|latch| latch.agent_label.as_deref())
            .is_some_and(|latched| latched != current)
        {
            self.clear_failure();
        }
        if self
            .announced_agent
            .as_deref()
            .is_some_and(|announced| announced != current)
        {
            self.forget_announced_failure();
        }
        if self
            .completion_agent
            .as_deref()
            .is_some_and(|announced| announced != current)
        {
            self.forget_announced_completion();
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

    /// 会话洞察带来的错误：不在 Working、错误记录自己的时刻不早于本回合开始、本回合还
    /// 没处理过同一错误时锁存失败。没有错误时刻的不锁存——证明不了它属于本回合，更早
    /// 回合留下的错误绝不能算成这一回合的失败；本 server 从没见过这个 agent 工作时也不
    /// 锁存（启动时读到的旧错误不算失败）。集成已经为本回合报过并通知过失败时（钩子
    /// 先报，会话文件稍后才读到同一个错误）也不再锁存。
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
        let (Some(turn_start), Some(at_ms)) = (self.last_working_start_ms, at_ms) else {
            return false;
        };
        // 毫秒粒度：与回合开始同一毫秒的错误算本回合的。
        if at_ms < turn_start || self.observed_error.as_deref() == Some(error) {
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
        assert!(
            !tracker.observe_error("other", Some(130), AgentState::Idle, None, 300),
            "本回合已经通知过失败"
        );
        let now = Instant::now();
        tracker.on_state_change(AgentState::Idle, AgentState::Working, now, 400);
        tracker.on_state_change(AgentState::Working, AgentState::Idle, now, 450);
        assert!(tracker.observe_error("other", Some(420), AgentState::Idle, None, 500));
    }

    /// 一个失败的回合只通知一次：钩子报的失败通知过之后，会话文件稍后读到的同一个错误、
    /// 用户看过之后迟到的上报都不再锁存；新的 Working 开始新回合。
    #[test]
    fn a_failed_turn_is_announced_once_until_the_next_turn() {
        let mut tracker = TurnTracker::default();
        let now = Instant::now();
        tracker.on_state_change(AgentState::Idle, AgentState::Working, now, 100);
        assert!(tracker.latch_failure(
            Some("provider_error".into()),
            Some("hook".into()),
            Some("kimi"),
            150
        ));
        assert!(!tracker.take_failure_notice(AgentState::Working));
        assert!(!tracker.failure_announced(), "还没通知");
        tracker.on_state_change(AgentState::Working, AgentState::Idle, now, 200);
        assert!(tracker.take_failure_notice(AgentState::Idle));
        assert!(tracker.failure_announced());

        assert!(
            !tracker.observe_error("insight", Some(180), AgentState::Idle, Some("kimi"), 300),
            "钩子之后会话洞察读到的同一失败"
        );
        assert!(!tracker.take_failure_notice(AgentState::Idle));
        assert_eq!(
            tracker
                .attention(AgentState::Idle, None)
                .and_then(|attention| attention.detail)
                .as_deref(),
            Some("hook"),
            "保留先报的详情"
        );
        assert!(tracker.acknowledge());
        assert!(
            !tracker.latch_failure(None, Some("late".into()), Some("kimi"), 400),
            "看过之后迟到的上报不再亮起"
        );
        assert_eq!(tracker.attention(AgentState::Idle, None), None);
        tracker.on_state_change(AgentState::Idle, AgentState::Blocked, now, 450);
        assert!(tracker.failure_announced(), "进入 Blocked 仍是同一回合");

        tracker.on_state_change(AgentState::Blocked, AgentState::Working, now, 500);
        assert!(!tracker.failure_announced(), "新回合");
        assert!(tracker.latch_failure(None, Some("again".into()), Some("kimi"), 550));
    }

    /// 回合已经发过「已完成」：之后读到的同一回合的错误只锁存（待处理事项里露面、看过
    /// 即确认），不再产生通知；新回合、释放与换 agent 恢复通知。
    #[test]
    fn a_failure_after_the_completion_notice_latches_silently() {
        let mut tracker = tracker_after_turn(100);
        tracker.note_completion_announced(Some("codex"));
        assert!(tracker.completion_announced());
        assert!(tracker.observe_error("late", Some(120), AgentState::Idle, Some("codex"), 300));
        assert!(
            !tracker.take_failure_notice(AgentState::Idle),
            "同一回合不再通知"
        );
        assert!(!tracker.failure_announced());
        assert_eq!(
            tracker
                .attention(AgentState::Idle, None)
                .and_then(|attention| attention.detail)
                .as_deref(),
            Some("late"),
            "失败照常露面"
        );
        assert!(tracker.acknowledge(), "没有待发通知的失败可被看过");

        let now = Instant::now();
        tracker.on_state_change(AgentState::Idle, AgentState::Working, now, 400);
        assert!(!tracker.completion_announced(), "新回合");
        tracker.on_state_change(AgentState::Working, AgentState::Idle, now, 450);
        assert!(tracker.observe_error("boom", Some(420), AgentState::Idle, Some("codex"), 500));
        assert!(tracker.take_failure_notice(AgentState::Idle));

        let mut tracker = tracker_after_turn(100);
        tracker.note_completion_announced(Some("claude"));
        tracker.reconcile_agent(Some("claude"));
        assert!(tracker.completion_announced(), "同一个 agent");
        tracker.reconcile_agent(Some("codex"));
        assert!(!tracker.completion_announced(), "换成别的 agent");
        tracker.note_completion_announced(Some("codex"));
        tracker.forget_agent();
        assert!(!tracker.completion_announced(), "释放");
    }

    #[test]
    fn a_released_or_replaced_agent_does_not_inherit_the_announced_failure() {
        let mut tracker = TurnTracker::default();
        tracker.latch_failure(None, Some("x".into()), Some("claude"), 1);
        assert!(tracker.take_failure_notice(AgentState::Idle));
        tracker.forget_agent();
        assert!(!tracker.failure_announced(), "释放");

        tracker.latch_failure(None, Some("y".into()), Some("claude"), 2);
        assert!(tracker.take_failure_notice(AgentState::Idle));
        assert!(tracker.acknowledge());
        tracker.reconcile_agent(None);
        tracker.reconcile_agent(Some("claude"));
        assert!(tracker.failure_announced(), "还是同一个 agent");
        tracker.reconcile_agent(Some("codex"));
        assert!(!tracker.failure_announced(), "换成别的 agent");
    }

    /// 更早回合留下的错误永远不能锁存成当前回合的失败：没有时刻的错误不锁存，早于本
    /// 回合开始的错误在之后的任何回合里都不锁存，哪怕用户看过、错误文本没变。
    #[test]
    fn a_stale_error_from_an_earlier_turn_never_latches() {
        let mut tracker = tracker_after_turn(100);
        assert!(
            !tracker.observe_error("boom", None, AgentState::Idle, Some("claude"), 10_000),
            "没有时刻的错误证明不了属于本回合"
        );
        assert!(tracker.observe_error("boom", Some(120), AgentState::Idle, Some("claude"), 200));
        assert!(tracker.take_failure_notice(AgentState::Idle));
        assert!(tracker.acknowledge());

        // 新的回合开始又结束：来源还在报上一回合的那条错误（时刻 120 早于新回合开始）。
        let now = Instant::now();
        tracker.on_state_change(AgentState::Idle, AgentState::Working, now, 1_000);
        tracker.on_state_change(AgentState::Working, AgentState::Idle, now, 1_500);
        assert!(!tracker.observe_error("boom", Some(120), AgentState::Idle, Some("claude"), 1_600));
        assert!(!tracker.observe_error("boom", None, AgentState::Idle, Some("claude"), 1_600));
        assert_eq!(tracker.attention(AgentState::Idle, None), None);
        // 本回合真的又失败了（时刻晚于回合开始）才锁存。
        assert!(tracker.observe_error(
            "boom",
            Some(1_200),
            AgentState::Idle,
            Some("claude"),
            1_600
        ));
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
