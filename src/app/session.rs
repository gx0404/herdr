use std::collections::HashSet;
use std::time::{Duration, Instant};

use super::{App, SESSION_SAVE_DEBOUNCE};

#[derive(Default)]
pub(super) struct SessionLayout {
    workspaces: HashSet<String>,
    tabs: HashSet<(String, usize)>,
    panes: HashSet<(String, usize, usize)>,
    unidentified: bool,
}

impl SessionLayout {
    pub(super) fn from_snapshot(snapshot: &crate::persist::SessionSnapshot) -> Self {
        let mut layout = Self::default();
        for workspace in &snapshot.workspaces {
            let Some(id) = &workspace.id else {
                layout.unidentified = true;
                continue;
            };
            layout.workspaces.insert(id.clone());
            for (index, tab) in workspace.tabs.iter().enumerate() {
                let Some(&number) = workspace.public_tab_numbers.get(index) else {
                    layout.unidentified = true;
                    continue;
                };
                layout.tabs.insert((id.clone(), number));
                for pane in tab.panes.keys() {
                    if let Some(&pane_number) = workspace.public_pane_numbers.get(pane) {
                        layout.panes.insert((id.clone(), number, pane_number));
                    } else {
                        layout.unidentified = true;
                    }
                }
            }
        }
        layout
    }

    fn from_workspaces(workspaces: &[crate::workspace::Workspace]) -> Self {
        let mut layout = Self::default();
        for workspace in workspaces {
            layout.workspaces.insert(workspace.id.clone());
            for tab in &workspace.tabs {
                layout.tabs.insert((workspace.id.clone(), tab.number));
                for pane in tab.panes.keys() {
                    if let Some(&number) = workspace.public_pane_numbers.get(pane) {
                        layout
                            .panes
                            .insert((workspace.id.clone(), tab.number, number));
                    } else {
                        layout.unidentified = true;
                    }
                }
            }
        }
        layout
    }

    #[cfg(test)]
    pub(super) fn pane_count(&self) -> usize {
        self.panes.len()
    }

    fn is_retained_by(&self, current: &Self) -> bool {
        !self.unidentified
            && !current.unidentified
            && self.workspaces.is_subset(&current.workspaces)
            && self.tabs.is_subset(&current.tabs)
            && self.panes.is_subset(&current.panes)
    }

    fn is_single_pane(&self) -> bool {
        !self.unidentified
            && self.workspaces.len() == 1
            && self.tabs.len() == 1
            && self.panes.len() == 1
    }
}

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

    /// 仅在用户/API 已完成显式布局变更后调用；普通置脏不授予收缩许可。
    pub(crate) fn authorize_session_layout_change(&mut self) {
        self.authorized_session_layout =
            Some(SessionLayout::from_workspaces(&self.state.workspaces));
    }

    pub(super) fn authorize_default_workspace_replacement(&mut self) {
        let current = SessionLayout::from_workspaces(&self.state.workspaces);
        if self.persisted_session_layout.is_single_pane() && current.is_single_pane() {
            self.authorize_session_layout_change();
        }
    }

    /// 隐式归零不清盘；护栏内保留基线的 workspace/tab/pane 身份。
    /// 新建同等数量的替代项不能掩盖丢失；旧快照缺身份时保守等到护栏到期。
    fn capture_session_save_job(&self, now: Instant) -> Option<SessionSaveJob> {
        if self.state.workspaces.is_empty() {
            if self.state.explicit_session_teardown {
                return Some(SessionSaveJob::Clear);
            }
            tracing::info!(
                should_quit = self.state.should_quit,
                persisted_workspaces = self.persisted_session_layout.workspaces.len(),
                "skipping session save: workspace set emptied without an explicit teardown"
            );
            return None;
        }
        if self.pane_exit_cascade_guard_until(now).is_some() {
            let current = SessionLayout::from_workspaces(&self.state.workspaces);
            let protected = self
                .authorized_session_layout
                .as_ref()
                .unwrap_or(&self.persisted_session_layout);
            if !protected.is_retained_by(&current) {
                tracing::info!(
                    workspaces = current.workspaces.len(),
                    tabs = current.tabs.len(),
                    panes = current.panes.len(),
                    persisted_workspaces = protected.workspaces.len(),
                    persisted_tabs = protected.tabs.len(),
                    persisted_panes = protected.panes.len(),
                    "skipping session save: a pane exit cascade would discard persisted layout identities"
                );
                return None;
            }
        }
        let snapshot = crate::persist::capture(
            &self.state.workspaces,
            &self.state.terminals,
            &self.terminal_runtimes,
            self.state.active,
            self.state.selected,
        );
        let history = self.persist_pane_history.then(|| {
            crate::persist::capture_history(
                &snapshot,
                &self.state.workspaces,
                &self.terminal_runtimes,
            )
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

    /// 按派发的实际快照记账，不按可继续变化的 live state，也不声称写盘已成功。
    /// 未显式授权的护栏内保存只能增加身份：写失败仍保留较大的账本，最多多拦；
    /// 只有显式布局授权或护栏到期才接受身份丢失，写失败仍由 SessionWriter 报告。
    fn note_session_save_dispatched(&mut self, job: &SessionSaveJob) {
        self.persisted_session_layout = match job {
            SessionSaveJob::Clear => SessionLayout::default(),
            SessionSaveJob::Save { snapshot, .. } => SessionLayout::from_snapshot(snapshot),
        };
        self.authorized_session_layout = None;
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
        let writer = self.session_writer.clone();
        match std::thread::Builder::new()
            .name("herdr-session-save".into())
            .spawn(move || run_session_save_job(job, &writer))
        {
            Ok(thread) => self.session_save_thread = Some(thread),
            Err(err) => {
                tracing::warn!(err = %err, "failed to spawn session save thread; saving inline");
                if let Some(job) = self.capture_session_save_job(now) {
                    run_session_save_job(job, &self.session_writer);
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
            run_session_save_job(job, &self.session_writer);
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

fn run_session_save_job(
    job: SessionSaveJob,
    writer: &std::sync::Mutex<crate::persist::SessionWriter>,
) {
    let mut writer = match writer.lock() {
        Ok(writer) => writer,
        Err(err) => {
            tracing::warn!(err = %err, "session writer is poisoned; refusing to modify session");
            return;
        }
    };
    match job {
        SessionSaveJob::Clear => writer.clear(),
        SessionSaveJob::Save { snapshot, history } => writer.save(&snapshot, history.as_ref()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{AppPolicy, AppState};
    use crate::events::AppEvent;
    use crate::platform::ChildExitReason;
    use crate::workspace::Workspace;
    use ratatui::layout::Direction;

    fn test_workspace() -> Workspace {
        let mut workspace = Workspace::test_new("identity-guard");
        workspace.test_split(Direction::Horizontal);
        workspace.test_add_tab(Some("logs"));
        workspace.switch_tab(0);
        workspace
    }

    fn test_app(workspace: Workspace) -> App {
        let mut app = App::new(
            &crate::config::Config::default(),
            AppPolicy::TEST,
            None,
            tokio::sync::mpsc::unbounded_channel().1,
            crate::api::EventHub::default(),
        );
        app.state = AppState::test_new();
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.ensure_test_terminals();
        app
    }

    #[test]
    fn session_shrink_guard_rejects_equal_count_replacements_after_growth() {
        let _dirs = crate::config::test_dirs::isolate_dirs("layout-identity-replacements");
        for replaced in ["workspace", "tab", "pane"] {
            let mut app = test_app(test_workspace());
            let now = Instant::now();
            let baseline = app.capture_session_save_job(now).expect("baseline job");
            app.note_session_save_dispatched(&baseline);
            assert!(app.note_pane_exit_requires_checkpoint(ChildExitReason::Interrupted, now));
            app.state.workspaces[0].test_split(Direction::Vertical);
            app.state.ensure_test_terminals();
            let grown = app
                .capture_session_save_job(now + Duration::from_secs(1))
                .expect("adding a pane retains every protected identity");
            let before = SessionLayout::from_workspaces(&app.state.workspaces);

            match replaced {
                "workspace" => {
                    let mut replacement = test_workspace();
                    replacement.test_split(Direction::Vertical);
                    app.state.workspaces = vec![replacement];
                }
                "tab" => {
                    assert!(app.state.workspaces[0].close_tab(1));
                    app.state.workspaces[0].test_add_tab(Some("replacement"));
                }
                "pane" => {
                    let pane_id = app.state.workspaces[0].tabs[0].root_pane;
                    app.state.handle_app_event(AppEvent::PaneDied {
                        pane_id,
                        exit_reason: ChildExitReason::Exited,
                    });
                    app.state.workspaces[0].test_split(Direction::Horizontal);
                }
                _ => unreachable!(),
            }
            app.state.mark_session_dirty();
            app.state.ensure_test_terminals();
            app.note_session_save_dispatched(&grown);
            let after = SessionLayout::from_workspaces(&app.state.workspaces);
            assert_eq!(
                (
                    before.workspaces.len(),
                    before.tabs.len(),
                    before.panes.len()
                ),
                (after.workspaces.len(), after.tabs.len(), after.panes.len()),
                "{replaced} replacement must preserve all three counts"
            );
            assert!(
                app.capture_session_save_job(now + Duration::from_secs(2)).is_none(),
                "the dispatched job, not the later live {replaced}, defines the protected identities"
            );
        }
    }

    #[test]
    fn session_shrink_guard_restored_snapshot_uses_public_ids_and_keeps_missing_tabs() {
        let _dirs = crate::config::test_dirs::isolate_dirs("layout-identity-restore");
        let mut app = test_app(test_workspace());
        let now = Instant::now();
        let Some(SessionSaveJob::Save { mut snapshot, .. }) = app.capture_session_save_job(now)
        else {
            panic!("expected baseline snapshot");
        };
        let old_root = app.state.workspaces[0].tabs[0].root_pane;
        let mut restored = test_workspace();
        restored.id = snapshot.workspaces[0].id.clone().unwrap();
        assert_ne!(restored.tabs[0].root_pane, old_root);
        assert!(restored.move_tab(0, 2));
        app.state.workspaces = vec![restored];
        app.state.ensure_test_terminals();
        app.persisted_session_layout = SessionLayout::from_snapshot(&snapshot);
        assert!(app.note_pane_exit_requires_checkpoint(ChildExitReason::Interrupted, now));
        assert!(app.capture_session_save_job(now).is_some());

        snapshot.workspaces[0].public_pane_numbers.clear();
        assert!(!SessionLayout::from_snapshot(&snapshot)
            .is_retained_by(&SessionLayout::from_workspaces(&app.state.workspaces)));
        assert!(app.state.workspaces[0].close_tab(0));
        assert!(app.capture_session_save_job(now).is_none());
    }

    #[test]
    fn session_shrink_guard_limits_replacement_authorization_and_expires() {
        let _dirs = crate::config::test_dirs::isolate_dirs("layout-identity-authorization");
        for panes in [1, 3] {
            let mut workspace = Workspace::test_new("original");
            for _ in 1..panes {
                workspace.test_split(Direction::Horizontal);
            }
            let mut app = test_app(workspace);
            let now = Instant::now();
            let baseline = app.capture_session_save_job(now).expect("baseline job");
            app.note_session_save_dispatched(&baseline);
            assert!(app.note_pane_exit_requires_checkpoint(ChildExitReason::Interrupted, now));
            app.state.workspaces = vec![Workspace::test_new("default")];
            app.state.ensure_test_terminals();
            app.state.mark_session_dirty();
            assert!(app.capture_session_save_job(now).is_none());
            app.authorize_default_workspace_replacement();
            assert_eq!(app.capture_session_save_job(now).is_some(), panes == 1);
            if panes > 1 {
                let until = now + PANE_EXIT_CASCADE_GUARD;
                assert!(app
                    .capture_session_save_job(until - Duration::from_nanos(1))
                    .is_none());
                assert!(app.capture_session_save_job(until).is_some());
            }

            app.authorize_session_layout_change();
            let authorized = app
                .capture_session_save_job(now)
                .expect("explicit replacement");
            app.note_session_save_dispatched(&authorized);
            app.state.workspaces = vec![Workspace::test_new("unapproved replacement")];
            app.state.ensure_test_terminals();
            assert!(app.capture_session_save_job(now).is_none());
            app.state.close_selected_workspace();
            let clear = app
                .capture_session_save_job(now)
                .expect("explicit final close");
            assert!(matches!(clear, SessionSaveJob::Clear));
            app.note_session_save_dispatched(&clear);
            assert!(app.persisted_session_layout.workspaces.is_empty());
            assert!(app.authorized_session_layout.is_none());
        }
    }
}
