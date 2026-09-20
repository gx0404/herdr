use std::time::{Duration, Instant};

use super::{App, SESSION_SAVE_DEBOUNCE};

enum SessionSaveJob {
    Clear,
    Save {
        snapshot: crate::persist::SessionSnapshot,
        history: Option<crate::persist::SessionHistorySnapshot>,
    },
}

/// 判定「短窗口批量退出」的时间窗。主机重启时所有 shell 几乎同时结束，即使
/// 退出码看起来正常也要按可疑处理。
const PANE_EXIT_BURST_WINDOW: Duration = Duration::from_secs(2);
/// 窗口内达到这个条数就算批量退出。取 2 是最早能成立的「批量」：判据只用来
/// 武装下面的收缩护栏（不写盘），越早武装越安全。
const PANE_EXIT_BURST_THRESHOLD: usize = 2;
/// 疑似主机打断后「不许把盘上快照写小」的护栏窗口。
///
/// 取 90 s 对齐 systemd 默认的 `DefaultTimeoutStopSec`：忽略 SIGHUP 的登录
/// shell 与 agent CLI 要等到 SIGTERM→SIGKILL 超时才死，级联会被拉这么长。
/// 每次新的可疑退出都重新武装；窗口到期后照常写盘，所以误判只是延迟落盘，
/// 不会永久卡住（HSR-04 / 上游 #4320）。
const PANE_EXIT_CASCADE_GUARD: Duration = Duration::from_secs(90);

impl App {
    pub(super) fn schedule_session_save(&mut self) {
        if self.policy.persist_session {
            self.pane_exit_checkpoint_pending = false;
            self.session_save_deadline = Some(Instant::now() + SESSION_SAVE_DEBOUNCE);
        }
    }

    pub(crate) fn sync_session_save_schedule(&mut self) {
        if self.state.session_dirty {
            self.state.session_dirty = false;
            self.schedule_session_save();
        }
    }

    fn reap_finished_session_save(&mut self) {
        if self
            .session_save_thread
            .as_ref()
            .is_some_and(std::thread::JoinHandle::is_finished)
        {
            if let Some(thread) = self.session_save_thread.take() {
                let _ = thread.join();
            }
        }
    }

    /// 打包一次会话保存；返回 `None` 表示「这次不写」。
    ///
    /// 两条「不写」规则都是为了让主机重启后盘上还留着完整会话
    /// （HSR-04 / 上游 #4320）：
    ///
    /// 1. 空工作区集合只有在用户/API 显式关闭最后一个工作区时才写 `Clear`。
    ///    主机重启会让 pane 逐个退出、集合同样归零，那时必须保留盘上的快照；
    ///    shutdown 路径同理。
    /// 2. 疑似主机打断的护栏窗口里，不许把盘上的快照写小。级联退出会让工作区
    ///    集合一路缩水，期间的检查点、去抖自动保存与 `ensure_default_workspace`
    ///    的自动补位都会捕到中间态；直接写下去等于把「全丢」换成「丢一部分」。
    fn capture_session_save_job(&self, now: Instant) -> Option<SessionSaveJob> {
        if self.state.workspaces.is_empty() {
            if self.state.explicit_session_teardown {
                return Some(SessionSaveJob::Clear);
            }
            // 这是一条会改变持久化结果的决策，debug 级在真机排障时看不到。
            tracing::info!(
                should_quit = self.state.should_quit,
                persisted_workspaces = self.persisted_workspace_count,
                "skipping session save: workspace set emptied without an explicit teardown"
            );
            return None;
        }
        if self.pane_exit_cascade_guard_until(now).is_some()
            && self.state.workspaces.len() < self.persisted_workspace_count
        {
            tracing::info!(
                workspaces = self.state.workspaces.len(),
                persisted_workspaces = self.persisted_workspace_count,
                "skipping session save: a pane exit cascade would shrink the persisted snapshot"
            );
            return None;
        }
        let snapshot = crate::persist::capture(
            &self.state.workspaces,
            &self.state.terminals,
            &self.terminal_runtimes,
            self.state.active,
            self.state.selected,
        );
        let history = self.persist_pane_history.then(|| {
            crate::persist::capture_history(&self.state.workspaces, &self.terminal_runtimes)
        });
        Some(SessionSaveJob::Save { snapshot, history })
    }

    /// 护栏窗口还开着时返回它的截止时刻。
    ///
    /// 它同时是「这次跳过写盘」的重试点：窗口到期后再判一次，让被压住的状态
    /// 最终落盘，误判只表现为延迟而不是永久卡住。
    fn pane_exit_cascade_guard_until(&self, now: Instant) -> Option<Instant> {
        self.pane_exit_cascade_until.filter(|until| now < *until)
    }

    /// 登记一次**真实 pane**（仍在 `workspaces` 里、非 overlay）的退出，武装
    /// 收缩护栏，并回答「是否需要在移除 pane 前落会话检查点」。
    ///
    /// 除了平台判定出的（疑似）打断，短窗口内的批量退出也算可疑：主机重启时
    /// 部分 shell 会以 `0` 收尾，单看退出码分不出来。
    ///
    /// 调用方必须先过滤显式关闭路径：`close_pane` / `close_selected_workspace`
    /// 会同步摘除 pane，随后到达的 `PaneDied` 不是「主机重启」的证据，不能进
    /// 窗口——否则关掉一个 3 pane 的 tab 就足以武装 burst。
    pub(crate) fn note_pane_exit_requires_checkpoint(
        &mut self,
        exit_reason: crate::platform::ChildExitReason,
        now: Instant,
    ) -> bool {
        self.recent_pane_exits
            .retain(|at| now.saturating_duration_since(*at) < PANE_EXIT_BURST_WINDOW);
        // 只保留判定所需的条数，15 pane 批量退出时也不会增长。
        if self.recent_pane_exits.len() >= PANE_EXIT_BURST_THRESHOLD {
            self.recent_pane_exits.remove(0);
        }
        self.recent_pane_exits.push(now);
        let burst = self.recent_pane_exits.len() >= PANE_EXIT_BURST_THRESHOLD;
        let suspicious = exit_reason.requires_session_checkpoint() || burst;
        if suspicious {
            // 护栏只挡「写小」，不写盘，所以宁可早武装。
            self.pane_exit_cascade_until = Some(now + PANE_EXIT_CASCADE_GUARD);
            tracing::debug!(
                exits = self.recent_pane_exits.len(),
                ?exit_reason,
                burst,
                "suspected host interruption; arming the session shrink guard"
            );
        }
        suspicious
    }

    /// 记录一次真正派发出去的写盘，维护「盘上有多少 workspace」的账本。
    ///
    /// 乐观记账（后台线程写失败只打日志）：记多了只会让护栏更保守地保留旧快照，
    /// 方向是安全的。
    fn note_session_save_dispatched(&mut self, job: &SessionSaveJob) {
        self.persisted_workspace_count = match job {
            SessionSaveJob::Clear => 0,
            SessionSaveJob::Save { snapshot, .. } => snapshot.workspaces.len(),
        };
        #[cfg(test)]
        {
            self.session_save_writes += 1;
        }
    }

    pub(crate) fn start_background_session_save(&mut self, now: Instant) {
        if !self.policy.persist_session {
            self.session_save_deadline = None;
            return;
        }

        self.reap_finished_session_save();
        if self.session_save_thread.is_some() {
            self.session_save_deadline = Some(now + Duration::from_millis(250));
            return;
        }

        let Some(job) = self.capture_session_save_job(now) else {
            // 保留重试点而不是清空：护栏到期或集合恢复非空后要能自动落盘。
            self.session_save_deadline = self.pane_exit_cascade_guard_until(now);
            return;
        };
        self.note_session_save_dispatched(&job);
        self.pane_exit_checkpoint_pending = false;
        self.session_save_deadline = None;
        match std::thread::Builder::new()
            .name("herdr-session-save".into())
            .spawn(move || run_session_save_job(job))
        {
            Ok(thread) => self.session_save_thread = Some(thread),
            Err(err) => {
                tracing::warn!(err = %err, "failed to spawn session save thread; saving inline");
                if let Some(job) = self.capture_session_save_job(now) {
                    run_session_save_job(job);
                }
            }
        }
    }

    pub(crate) fn save_session_now(&mut self) {
        if let Some(thread) = self.session_save_thread.take() {
            let _ = thread.join();
        }

        if !self.policy.persist_session {
            self.session_save_deadline = None;
            return;
        }

        let now = Instant::now();
        if let Some(job) = self.capture_session_save_job(now) {
            self.note_session_save_dispatched(&job);
            run_session_save_job(job);
            self.pane_exit_checkpoint_pending = false;
            self.session_save_deadline = None;
        } else {
            self.session_save_deadline = self.pane_exit_cascade_guard_until(now);
        }
    }

    /// 在移除 pane 之前落一份检查点。
    ///
    /// 必须同步写：捕获的必须是「这个 pane 还在」的状态，交给后台线程就可能
    /// 排到移除之后。频率由两道门收敛——`pane_exit_checkpoint_pending &&
    /// !session_dirty` 让一次级联只写一次，收缩护栏让后续捕到中间态的尝试直接
    /// 返回 `None`（连 capture 之后的写盘都不做）。
    pub(crate) fn checkpoint_session_before_pane_exit(&mut self) {
        if !self.policy.persist_session
            || (self.pane_exit_checkpoint_pending && !self.state.session_dirty)
        {
            return;
        }
        self.save_session_now();
        self.pane_exit_checkpoint_pending = true;
        self.state.session_dirty = false;
    }

    pub(crate) fn finish_checkpointed_pane_exit(&mut self) {
        if self.pane_exit_checkpoint_pending {
            self.state.session_dirty = false;
            self.session_save_deadline = Some(Instant::now() + SESSION_SAVE_DEBOUNCE);
        }
    }

    pub(crate) fn save_session_on_shutdown(&mut self) {
        if self.pane_exit_checkpoint_pending && !self.state.session_dirty {
            self.session_save_deadline = None;
            return;
        }
        self.save_session_now();
    }
}

fn run_session_save_job(job: SessionSaveJob) {
    match job {
        SessionSaveJob::Clear => crate::persist::clear(),
        SessionSaveJob::Save { snapshot, history } => {
            crate::persist::save(&snapshot, history.as_ref());
        }
    }
}
