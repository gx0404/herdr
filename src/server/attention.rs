//! agent 通知的统一决策：一次有效状态迁移（或一次失败锁存）该不该弹 toast、响铃、发
//! 语义通知，以及通知正文。AppState 的 herdr toast、server 转发给前台客户端的声音与
//! 终端 / 系统 toast、发给客户端 shell 的语义通知都查这一张表，调用点只负责收集输入
//! 与投递。纯函数，表驱动测试。

use crate::app::state::ToastKind;
use crate::detect::AgentState;
use crate::protocol::{SemanticNotificationKind, SemanticNotificationSound};
use crate::sound::Sound;

/// 通知正文（问题或最后一条消息的摘要）的字符上限。
pub(crate) const BODY_MAX_CHARS: usize = 220;

/// 一次通知的种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AttentionKind {
    /// agent 受阻，等用户处理。
    NeedsAttention,
    /// agent 做完一回合回到空闲。
    Finished,
    /// 新锁存了一次失败（集成上报、会话错误或运行中进程退出）。
    Failed,
}

impl AttentionKind {
    pub(crate) fn toast_kind(self) -> ToastKind {
        match self {
            Self::NeedsAttention => ToastKind::NeedsAttention,
            Self::Finished => ToastKind::Finished,
            Self::Failed => ToastKind::Failed,
        }
    }

    /// 语义通知的种类。失败没有专属种类：`SemanticNotification` 经 generation 1 的
    /// bincode 编码直达客户端，追加枚举变体或字段会让旧客户端解码失败（变体越界 /
    /// 拒绝尾随字节）；借 `NeedsAttention` 又会被客户端在 toast 延迟（默认 1 s）后按
    /// 「pane 仍受阻」校验丢弃，且移动横幅会改写成「等待中」。所以失败用
    /// `Custom`：客户端立即投递、不校验状态、原样显示带「失败」字样的标题，并带上
    /// 请求音。
    pub(crate) fn semantic_kind(self) -> SemanticNotificationKind {
        match self {
            Self::NeedsAttention => SemanticNotificationKind::NeedsAttention,
            Self::Finished => SemanticNotificationKind::Finished,
            Self::Failed => SemanticNotificationKind::Custom,
        }
    }

    /// 语义通知携带的声音（客户端自行判断「正看着」时是否响）。
    pub(crate) fn semantic_sound(self) -> SemanticNotificationSound {
        match self {
            Self::NeedsAttention | Self::Failed => SemanticNotificationSound::Request,
            Self::Finished => SemanticNotificationSound::Done,
        }
    }

    /// 标题里的事件文案（`{agent} {event}`）。
    pub(crate) fn event_text(self) -> &'static str {
        let notify = &crate::i18n::texts().notify;
        match self {
            Self::NeedsAttention => notify.needs_attention,
            Self::Finished => notify.finished,
            Self::Failed => notify.failed,
        }
    }
}

/// 决策输入。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AttentionInput<'a> {
    pub previous_state: AgentState,
    pub state: AgentState,
    /// 目标 pane 在用户正看着的 tab 里（见 `app::actions::active_tab_suppresses_notifications`）。
    pub suppress_active_tab: bool,
    /// 这次更新不该通知（agent 刚被接管时的空闲、刻意的退出等）。
    pub suppress_completion: bool,
    /// 完成迁移前 1.5 s 内用户单独发过 Ctrl-C / ESC。
    pub interrupted_recently: bool,
    /// 这次更新新锁存了一次失败。
    pub failed_latched: bool,
    /// 正文原文（问题、失败原因或最后一条消息）。
    pub body: Option<&'a str>,
}

/// 决策结果；`kind` 为 `None` 时什么都不发。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct AttentionDecision {
    pub kind: Option<AttentionKind>,
    /// herdr / 终端 / 系统 toast（正看着的 tab 不弹）。
    pub toast: Option<ToastKind>,
    /// 声音：只有完成音在正看着的 tab 里不响，请求音照响。
    pub sound: Option<Sound>,
    /// 发给客户端 shell 的语义通知（不做 tab 抑制，由客户端判断）。
    pub semantic: Option<SemanticNotificationKind>,
    /// 正文摘要：去 markdown、压成一行、不超过 [`BODY_MAX_CHARS`] 个字符。
    pub body: Option<String>,
}

/// 唯一的通知决策函数。
pub(crate) fn decide(input: &AttentionInput<'_>) -> AttentionDecision {
    let Some(kind) = classify(input) else {
        return AttentionDecision::default();
    };
    AttentionDecision {
        kind: Some(kind),
        toast: (!input.suppress_active_tab).then(|| kind.toast_kind()),
        sound: sound_for(kind, input.suppress_active_tab),
        semantic: Some(kind.semantic_kind()),
        body: input
            .body
            .and_then(|text| crate::harness::text::summary_text(text, BODY_MAX_CHARS)),
    }
}

/// 某种通知在投递时刻的声音（延迟投递时按投递时刻的 tab 状态重新判断）。
pub(crate) fn sound_for(kind: AttentionKind, suppress_active_tab: bool) -> Option<Sound> {
    match kind {
        AttentionKind::NeedsAttention | AttentionKind::Failed => Some(Sound::Request),
        AttentionKind::Finished => (!suppress_active_tab).then_some(Sound::Done),
    }
}

fn classify(input: &AttentionInput<'_>) -> Option<AttentionKind> {
    if input.suppress_completion {
        return None;
    }
    if input.failed_latched {
        return Some(AttentionKind::Failed);
    }
    if input.state == input.previous_state {
        return None;
    }
    match (input.previous_state, input.state) {
        (_, AgentState::Blocked) => Some(AttentionKind::NeedsAttention),
        (AgentState::Working | AgentState::Blocked, AgentState::Idle)
            if !input.interrupted_recently =>
        {
            Some(AttentionKind::Finished)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use AgentState::{Blocked, Idle, Unknown, Working};

    fn input(previous_state: AgentState, state: AgentState) -> AttentionInput<'static> {
        AttentionInput {
            previous_state,
            state,
            suppress_active_tab: false,
            suppress_completion: false,
            interrupted_recently: false,
            failed_latched: false,
            body: None,
        }
    }

    type Row = (
        AgentState,
        AgentState,
        bool,
        bool,
        bool,
        bool,
        Option<AttentionKind>,
        Option<ToastKind>,
        Option<Sound>,
    );

    #[test]
    fn decision_table() {
        use AttentionKind::{Failed, Finished, NeedsAttention};
        // (前状态, 新状态, 正看着, 抑制完成, 刚中断, 新失败) → (种类, toast, 声音)
        #[rustfmt::skip]
        let rows: &[Row] = &[
            (Working, Blocked, false, false, false, false, Some(NeedsAttention), Some(ToastKind::NeedsAttention), Some(Sound::Request)),
            (Idle, Blocked, true, false, false, false, Some(NeedsAttention), None, Some(Sound::Request)),
            (Blocked, Blocked, false, false, false, false, None, None, None),
            (Working, Idle, false, false, false, false, Some(Finished), Some(ToastKind::Finished), Some(Sound::Done)),
            (Blocked, Idle, false, false, false, false, Some(Finished), Some(ToastKind::Finished), Some(Sound::Done)),
            (Working, Idle, true, false, false, false, Some(Finished), None, None),
            (Working, Idle, false, true, false, false, None, None, None),
            (Working, Idle, false, false, true, false, None, None, None),
            (Blocked, Idle, false, false, true, false, None, None, None),
            (Unknown, Idle, false, false, false, false, None, None, None),
            (Idle, Working, false, false, false, false, None, None, None),
            (Working, Unknown, false, false, false, false, None, None, None),
            (Working, Idle, false, false, false, true, Some(Failed), Some(ToastKind::Failed), Some(Sound::Request)),
            (Idle, Idle, false, false, false, true, Some(Failed), Some(ToastKind::Failed), Some(Sound::Request)),
            (Working, Idle, true, false, true, true, Some(Failed), None, Some(Sound::Request)),
            (Working, Idle, false, true, false, true, None, None, None),
        ];
        for &(previous, state, active, suppress, interrupted, failed, kind, toast, sound) in rows {
            let decision = decide(&AttentionInput {
                suppress_active_tab: active,
                suppress_completion: suppress,
                interrupted_recently: interrupted,
                failed_latched: failed,
                ..input(previous, state)
            });
            let row = (previous, state, active, suppress, interrupted, failed);
            assert_eq!(decision.kind, kind, "{row:?}");
            assert_eq!(decision.toast, toast, "{row:?}");
            assert_eq!(decision.sound, sound, "{row:?}");
            assert_eq!(
                decision.semantic,
                kind.map(AttentionKind::semantic_kind),
                "语义通知不受 tab 抑制：{row:?}"
            );
        }
    }

    #[test]
    fn failure_uses_a_backward_compatible_semantic_kind_and_request_sound() {
        assert_eq!(
            AttentionKind::Failed.semantic_kind(),
            SemanticNotificationKind::Custom
        );
        assert_eq!(
            AttentionKind::Failed.semantic_sound(),
            SemanticNotificationSound::Request
        );
        assert_eq!(
            AttentionKind::Finished.semantic_sound(),
            SemanticNotificationSound::Done
        );
    }

    #[test]
    fn body_is_a_single_line_summary_only_when_notifying() {
        let text = "## Question\n\nShould the API client **retry** on `429`?";
        let decision = decide(&AttentionInput {
            body: Some(text),
            ..input(Working, Blocked)
        });
        assert_eq!(
            decision.body.as_deref(),
            Some("Question Should the API client retry on 429?")
        );
        let long = "x".repeat(500);
        let decision = decide(&AttentionInput {
            body: Some(&long),
            ..input(Working, Idle)
        });
        assert_eq!(
            decision.body.map(|body| body.chars().count()),
            Some(BODY_MAX_CHARS)
        );
        assert_eq!(
            decide(&AttentionInput {
                body: Some(text),
                ..input(Idle, Working)
            }),
            AttentionDecision::default()
        );
    }

    #[test]
    fn delayed_sound_follows_the_active_tab_at_delivery() {
        assert_eq!(sound_for(AttentionKind::Finished, true), None);
        assert_eq!(sound_for(AttentionKind::Finished, false), Some(Sound::Done));
        assert_eq!(
            sound_for(AttentionKind::NeedsAttention, true),
            Some(Sound::Request)
        );
        assert_eq!(sound_for(AttentionKind::Failed, true), Some(Sound::Request));
    }
}
