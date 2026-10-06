use std::collections::HashSet;
use std::time::{Duration, Instant};

use super::{App, SESSION_SAVE_DEBOUNCE};

#[derive(Clone, Default)]
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

    pub(super) fn capture_session_layout(&self) -> SessionLayout {
        SessionLayout::from_workspaces(&self.state.workspaces)
    }

    /// before 必须紧邻本次同步结构修改捕获；只授权本次差量，不追认此前丢失的身份。
    pub(super) fn authorize_session_layout_change(&mut self, before: SessionLayout) {
        let after = self.capture_session_layout();
        if before.workspaces == after.workspaces
            && before.tabs == after.tabs
            && before.panes == after.panes
        {
            return;
        }
        let protected = self
            .authorized_session_layout
            .get_or_insert_with(|| self.persisted_session_layout.clone());
        if !before.unidentified && !after.unidentified {
            for removed in before.workspaces.difference(&after.workspaces) {
                protected.workspaces.remove(removed);
            }
            for removed in before.tabs.difference(&after.tabs) {
                protected.tabs.remove(removed);
            }
            for removed in before.panes.difference(&after.panes) {
                protected.panes.remove(removed);
            }
        }
        protected
            .workspaces
            .extend(after.workspaces.difference(&before.workspaces).cloned());
        protected
            .tabs
            .extend(after.tabs.difference(&before.tabs).cloned());
        protected
            .panes
            .extend(after.panes.difference(&before.panes).cloned());
        protected.unidentified |= before.unidentified || after.unidentified;
    }

    pub(super) fn authorize_default_workspace_replacement(&mut self) {
        let current = self.capture_session_layout();
        let protected = self
            .authorized_session_layout
            .as_ref()
            .unwrap_or(&self.persisted_session_layout);
        if protected.is_single_pane() && current.is_single_pane() {
            self.authorized_session_layout = Some(current);
        }
    }

    /// 隐式归零不清盘；护栏内保留基线的 workspace/tab/pane 身份。
    /// 新建同等数量的替代项不能掩盖丢失；旧快照缺身份时保守等到护栏到期。
    fn capture_session_save_job(&self, now: Instant) -> Option<SessionSaveJob> {
        if self.state.workspaces.is_empty() && !self.state.explicit_session_teardown {
            tracing::info!(
                should_quit = self.state.should_quit,
                persisted_workspaces = self.persisted_session_layout.workspaces.len(),
                "skipping session save: workspace set emptied without an explicit teardown"
            );
            return None;
        }
        if self.pane_exit_cascade_guard_until(now).is_some() {
            let current = self.capture_session_layout();
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
        if self.state.workspaces.is_empty() {
            return Some(SessionSaveJob::Clear);
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
        let now = Instant::now();
        for panes in [1, 3] {
            let mut workspace = Workspace::test_new("original");
            for _ in 1..panes {
                workspace.test_split(Direction::Horizontal);
            }
            let exiting = workspace.tabs[0].layout.pane_ids();
            let mut app = audit_session_app(workspace, now);
            for pane_id in exiting {
                audit_interrupt_pane(&mut app, pane_id, now);
            }
            assert!(app.state.workspaces.is_empty());
            assert!(app.capture_session_save_job(now).is_none());
            app.state.workspaces = vec![Workspace::test_new("default")];
            app.state.active = Some(0);
            app.state.ensure_test_terminals();
            app.state.mark_session_dirty();
            app.state.assert_invariants_for_test();
            assert!(app.capture_session_save_job(now).is_none());
            app.authorize_default_workspace_replacement();
            assert_eq!(app.capture_session_save_job(now).is_some(), panes == 1);
            if panes > 1 {
                let until = now + PANE_EXIT_CASCADE_GUARD;
                assert!(app
                    .capture_session_save_job(until - Duration::from_nanos(1))
                    .is_none());
                let Some(SessionSaveJob::Save { snapshot, .. }) =
                    app.capture_session_save_job(until)
                else {
                    panic!("default replacement must become saveable after expiry");
                };
                assert_eq!(
                    snapshot.workspaces[0].custom_name.as_deref(),
                    Some("default")
                );
                assert_eq!(SessionLayout::from_snapshot(&snapshot).pane_count(), 1);
            }
        }
    }

    #[test]
    fn audit_p0_session_explicit_replacement_and_clear_are_authorized() {
        let _dirs = crate::config::test_dirs::isolate_dirs("audit-p0-session-explicit-replacement");
        let now = Instant::now();
        let mut app = audit_session_app(test_workspace(), now);
        assert!(app.note_pane_exit_requires_checkpoint(ChildExitReason::Interrupted, now));
        let before = app.capture_session_layout();
        app.state.workspaces = vec![Workspace::test_new("explicit replacement")];
        app.state.ensure_test_terminals();
        app.authorize_session_layout_change(before);
        app.state.assert_invariants_for_test();
        let authorized = app
            .capture_session_save_job(now)
            .expect("replacing identities present before the operation is authorized");
        let SessionSaveJob::Save { snapshot, .. } = &authorized else {
            panic!("expected explicit replacement snapshot");
        };
        assert_eq!(
            snapshot.workspaces[0].custom_name.as_deref(),
            Some("explicit replacement")
        );
        app.note_session_save_dispatched(&authorized);

        let before = app.capture_session_layout();
        app.state.close_selected_workspace();
        app.authorize_session_layout_change(before);
        app.state.assert_invariants_for_test();
        let clear = app
            .capture_session_save_job(now)
            .expect("explicit final close");
        assert!(matches!(clear, SessionSaveJob::Clear));
        app.note_session_save_dispatched(&clear);
        assert!(app.persisted_session_layout.workspaces.is_empty());
        assert!(app.authorized_session_layout.is_none());
    }

    fn audit_session_app(workspace: Workspace, now: Instant) -> App {
        let mut app = test_app(workspace);
        app.state.assert_invariants_for_test();
        let baseline = app.capture_session_save_job(now).expect("baseline job");
        app.note_session_save_dispatched(&baseline);
        app
    }

    fn audit_interrupt_pane(app: &mut App, pane_id: crate::layout::PaneId, now: Instant) {
        assert!(app.note_pane_exit_requires_checkpoint(ChildExitReason::Interrupted, now));
        app.state.handle_app_event(AppEvent::PaneDied {
            pane_id,
            exit_reason: ChildExitReason::Interrupted,
        });
        app.state.assert_invariants_for_test();
    }

    fn audit_add_authorized_test_tab(app: &mut App, name: &str) -> crate::layout::PaneId {
        let before = app.capture_session_layout();
        let tab = app.state.workspaces[0].test_add_tab(Some(name));
        app.state.ensure_test_terminals();
        app.authorize_session_layout_change(before);
        app.state.assert_invariants_for_test();
        app.state.workspaces[0].tabs[tab].root_pane
    }

    fn audit_close_pane(app: &mut App, pane_id: crate::layout::PaneId) {
        use crate::api::schema::{Method, PaneTarget, Request, ResponseResult, SuccessResponse};

        let pane_id = app.public_pane_id(0, pane_id).expect("live pane id");
        let response = app.handle_api_request(Request {
            id: "audit-p0-close-pane".into(),
            method: Method::PaneClose(PaneTarget { pane_id }),
        });
        let response: SuccessResponse =
            serde_json::from_str(&response).expect("pane.close response");
        assert_eq!(response.result, ResponseResult::Ok {});
        app.state.assert_invariants_for_test();
    }

    #[test]
    fn audit_p0_session_reorder_does_not_authorize_prior_loss() {
        use crate::api::schema::{Method, Request, ResponseResult, SuccessResponse, TabMoveParams};

        let _dirs = crate::config::test_dirs::isolate_dirs("audit-p0-session-reorder");
        let now = Instant::now();
        let mut workspace = Workspace::test_new("reorder");
        let lost = workspace.tabs[0].root_pane;
        workspace.test_add_tab(Some("moved"));
        workspace.test_add_tab(Some("survivor"));
        let mut app = audit_session_app(workspace, now);

        audit_interrupt_pane(&mut app, lost, now);
        assert!(app.capture_session_save_job(now).is_none());
        let moved_id = app.public_tab_id(0, 0).expect("surviving tab id");
        let response = app.handle_api_request(Request {
            id: "audit-p0-reorder".into(),
            method: Method::TabMove(TabMoveParams {
                tab_id: moved_id.clone(),
                insert_index: 2,
            }),
        });
        let response: SuccessResponse = serde_json::from_str(&response).expect("tab.move response");
        assert!(matches!(response.result, ResponseResult::TabList { .. }));
        assert_eq!(app.public_tab_id(0, 1), Some(moved_id));
        app.state.assert_invariants_for_test();

        assert!(
            app.capture_session_save_job(now).is_none(),
            "reordering surviving tabs must not authorize an earlier interrupted tab loss"
        );
    }

    #[test]
    fn audit_p0_session_create_does_not_authorize_prior_loss() {
        let _dirs = crate::config::test_dirs::isolate_dirs("audit-p0-session-create");
        let now = Instant::now();
        let mut workspace = Workspace::test_new("create");
        let lost = workspace.tabs[0].root_pane;
        workspace.test_add_tab(Some("survivor"));
        let mut app = audit_session_app(workspace, now);

        audit_interrupt_pane(&mut app, lost, now);
        assert!(app.capture_session_save_job(now).is_none());
        audit_add_authorized_test_tab(&mut app, "created");
        assert_eq!(app.state.workspaces[0].tabs.len(), 2);
        assert_eq!(
            SessionLayout::from_workspaces(&app.state.workspaces).pane_count(),
            app.persisted_session_layout.pane_count()
        );

        assert!(
            app.capture_session_save_job(now).is_none(),
            "creating an unrelated tab must not authorize the missing tab even when counts recover"
        );
        assert!(app
            .capture_session_save_job(now + PANE_EXIT_CASCADE_GUARD)
            .is_some());
    }

    #[test]
    fn audit_p0_session_last_pane_close_does_not_clear_prior_loss() {
        let _dirs = crate::config::test_dirs::isolate_dirs("audit-p0-session-final-pane-close");
        let now = Instant::now();
        for prior_loss in [false, true] {
            let mut workspace = Workspace::test_new("final-close");
            let first = workspace.tabs[0].root_pane;
            let last = workspace.test_split(Direction::Horizontal);
            let mut app = audit_session_app(workspace, now);

            if prior_loss {
                audit_interrupt_pane(&mut app, first, now);
                assert!(app.capture_session_save_job(now).is_none());
            } else {
                assert!(app.note_pane_exit_requires_checkpoint(ChildExitReason::Interrupted, now));
                audit_close_pane(&mut app, first);
                let Some(SessionSaveJob::Save { snapshot, .. }) = app.capture_session_save_job(now)
                else {
                    panic!("an explicit close without prior loss must remain saveable");
                };
                assert_eq!(snapshot.workspaces[0].tabs[0].panes.len(), 1);
                assert!(snapshot.workspaces[0].tabs[0]
                    .panes
                    .contains_key(&last.raw()));
            }

            audit_close_pane(&mut app, last);
            assert!(app.state.workspaces.is_empty());
            let job = app.capture_session_save_job(now);
            if prior_loss {
                assert!(
                    job.is_none(),
                    "closing the last live pane must not clear an earlier unapproved pane loss"
                );
                assert!(matches!(
                    app.capture_session_save_job(now + PANE_EXIT_CASCADE_GUARD),
                    Some(SessionSaveJob::Clear)
                ));
            } else {
                assert!(
                    matches!(job, Some(SessionSaveJob::Clear)),
                    "explicitly closing every protected pane must still clear the session"
                );
            }
        }
    }

    #[test]
    fn audit_p0_session_multiple_authorizations_retain_unsaved_loss() {
        let _dirs = crate::config::test_dirs::isolate_dirs("audit-p0-session-compose");
        let now = Instant::now();
        let mut workspace = Workspace::test_new("compose");
        let explicitly_closed = workspace.test_split(Direction::Horizontal);
        let mut app = audit_session_app(workspace, now);
        let lost = audit_add_authorized_test_tab(&mut app, "not-yet-saved");
        let Some(SessionSaveJob::Save { snapshot, .. }) = app.capture_session_save_job(now) else {
            panic!("authorized growth should be saveable");
        };
        assert_eq!(SessionLayout::from_snapshot(&snapshot).pane_count(), 3);
        assert_eq!(app.persisted_session_layout.pane_count(), 2);

        audit_interrupt_pane(&mut app, lost, now);
        assert!(app.capture_session_save_job(now).is_none());
        audit_close_pane(&mut app, explicitly_closed);
        audit_add_authorized_test_tab(&mut app, "later-create");
        assert_eq!(app.state.workspaces[0].tabs.len(), 2);

        assert!(
            app.capture_session_save_job(now).is_none(),
            "successive close/create authorizations must retain an unsaved authorized pane that died"
        );
    }

    #[test]
    fn audit_p0_session_default_replacement_uses_effective_baseline() {
        let _dirs = crate::config::test_dirs::isolate_dirs("audit-p0-session-default-baseline");
        let now = Instant::now();
        for authorized_growth in [false, true] {
            let workspace = Workspace::test_new("default-baseline");
            let mut exiting = vec![workspace.tabs[0].root_pane];
            let mut app = audit_session_app(workspace, now);
            if authorized_growth {
                let before = app.capture_session_layout();
                exiting.push(app.state.workspaces[0].test_split(Direction::Horizontal));
                app.state.ensure_test_terminals();
                app.authorize_session_layout_change(before);
                app.state.assert_invariants_for_test();
            }
            assert_eq!(app.persisted_session_layout.pane_count(), 1);

            for pane_id in exiting {
                audit_interrupt_pane(&mut app, pane_id, now);
            }
            assert!(app.state.workspaces.is_empty());
            assert!(app.capture_session_save_job(now).is_none());
            app.state.workspaces = vec![Workspace::test_new("default")];
            app.state.active = Some(0);
            app.state.ensure_test_terminals();
            app.state.assert_invariants_for_test();
            assert!(app.capture_session_save_job(now).is_none());

            app.authorize_default_workspace_replacement();
            if authorized_growth {
                assert!(
                    app.capture_session_save_job(now).is_none(),
                    "a stale single-pane persisted baseline must not replace a two-pane effective baseline"
                );
            } else {
                let Some(SessionSaveJob::Save { snapshot, .. }) = app.capture_session_save_job(now)
                else {
                    panic!("a genuinely single-pane default replacement must remain saveable");
                };
                assert_eq!(snapshot.workspaces.len(), 1);
                assert_eq!(
                    snapshot.workspaces[0].id.as_deref(),
                    Some(app.state.workspaces[0].id.as_str())
                );
            }
        }
    }

    fn audit_layout_pane(command: &str) -> crate::api::schema::LayoutNode {
        crate::api::schema::LayoutNode::Pane {
            pane: crate::api::schema::LayoutPane {
                cwd: Some(std::env::temp_dir().display().to_string()),
                command: Some(vec![command.into()]),
                ..Default::default()
            },
        }
    }

    #[tokio::test]
    async fn audit_p0_session_layout_apply_authorizes_only_committed_replacement() {
        use crate::api::schema::{
            LayoutApplyParams, LayoutNode, Method, Request, ResponseResult, SplitDirection,
            SuccessResponse,
        };
        use crate::app::api::test_support::{exiting_test_command, shutdown_test_runtimes};

        let _dirs = crate::config::test_dirs::isolate_dirs("audit-p0-session-layout-apply");
        let now = Instant::now();
        for prior_loss in [false, true] {
            let mut workspace = Workspace::test_new("layout-apply");
            let lost = prior_loss.then(|| {
                let tab = workspace.test_add_tab(Some("unrelated"));
                workspace.tabs[tab].root_pane
            });
            let mut app = audit_session_app(workspace, now);
            let old_tab_id = app.public_tab_id(0, 0).unwrap();
            if let Some(lost) = lost {
                audit_interrupt_pane(&mut app, lost, now);
            } else {
                assert!(app.note_pane_exit_requires_checkpoint(ChildExitReason::Interrupted, now));
            }

            let response = app.handle_api_request(Request {
                id: "audit-p0-layout-apply".into(),
                method: Method::LayoutApply(LayoutApplyParams {
                    workspace_id: None,
                    tab_id: Some(old_tab_id.clone()),
                    tab_label: Some("replacement".into()),
                    focus: false,
                    root: LayoutNode::Split {
                        direction: SplitDirection::Right,
                        ratio: 0.5,
                        first: Box::new(audit_layout_pane(exiting_test_command())),
                        second: Box::new(audit_layout_pane(exiting_test_command())),
                    },
                }),
            });
            let job = app.capture_session_save_job(now);
            shutdown_test_runtimes(&mut app);

            let response: SuccessResponse = serde_json::from_str(&response).unwrap();
            assert!(matches!(
                response.result,
                ResponseResult::LayoutApply { .. }
            ));
            assert_eq!(app.state.workspaces[0].tabs.len(), 1);
            assert_eq!(app.state.workspaces[0].tabs[0].panes.len(), 2);
            assert_ne!(app.public_tab_id(0, 0).unwrap(), old_tab_id);
            app.state.assert_invariants_for_test();
            assert_eq!(
                job.is_some(),
                !prior_loss,
                "a committed replacement may remove its target, never an earlier unrelated loss"
            );
            let Some(SessionSaveJob::Save { snapshot, .. }) =
                app.capture_session_save_job(now + PANE_EXIT_CASCADE_GUARD)
            else {
                panic!("the replacement must become saveable after expiry");
            };
            assert_eq!(SessionLayout::from_snapshot(&snapshot).pane_count(), 2);
        }
    }

    #[tokio::test]
    async fn audit_p0_session_layout_apply_rollback_does_not_authorize() {
        use crate::api::schema::{
            ErrorResponse, LayoutApplyParams, LayoutNode, Method, Request, SplitDirection,
        };
        use crate::app::api::test_support::{exiting_test_command, shutdown_test_runtimes};

        let _dirs = crate::config::test_dirs::isolate_dirs("audit-p0-session-layout-rollback");
        let now = Instant::now();
        let missing_command = std::env::temp_dir()
            .join(format!(
                "herdr-missing-command-{}",
                crate::config::test_dirs::unique_id()
            ))
            .display()
            .to_string();
        for prior_loss in [false, true] {
            let mut workspace = Workspace::test_new("layout-rollback");
            let lost = prior_loss.then(|| {
                let tab = workspace.test_add_tab(Some("unrelated"));
                workspace.tabs[tab].root_pane
            });
            let mut app = audit_session_app(workspace, now);
            if let Some(lost) = lost {
                audit_interrupt_pane(&mut app, lost, now);
            } else {
                assert!(app.note_pane_exit_requires_checkpoint(ChildExitReason::Interrupted, now));
            }
            let before = app.capture_session_layout();
            let original_tab = app.public_tab_id(0, 0).unwrap();
            let next_tab_number = app.state.workspaces[0].next_public_tab_number;
            let response = app.handle_api_request(Request {
                id: "audit-p0-layout-rollback".into(),
                method: Method::LayoutApply(LayoutApplyParams {
                    workspace_id: None,
                    tab_id: Some(original_tab.clone()),
                    tab_label: None,
                    focus: false,
                    root: LayoutNode::Split {
                        direction: SplitDirection::Right,
                        ratio: 0.5,
                        first: Box::new(audit_layout_pane(exiting_test_command())),
                        second: Box::new(audit_layout_pane(&missing_command)),
                    },
                }),
            });
            shutdown_test_runtimes(&mut app);

            let response: ErrorResponse = serde_json::from_str(&response).unwrap();
            assert_eq!(response.error.code, "layout_apply_failed");
            assert!(app.state.workspaces[0].next_public_tab_number > next_tab_number);
            assert_eq!(app.public_tab_id(0, 0), Some(original_tab));
            assert!(before.is_retained_by(&app.capture_session_layout()));
            assert!(app.capture_session_layout().is_retained_by(&before));
            assert!(app.authorized_session_layout.is_none());
            assert_eq!(app.capture_session_save_job(now).is_some(), !prior_loss);
            app.state.assert_invariants_for_test();
        }
    }

    #[tokio::test]
    async fn audit_p0_session_api_tab_create_retains_guarded_identities() {
        use crate::api::schema::{
            Method, Request, ResponseResult, SuccessResponse, TabCreateParams,
        };
        use crate::app::api::test_support::{exiting_test_command, shutdown_test_runtimes};

        let _dirs = crate::config::test_dirs::isolate_dirs("audit-p0-session-api-create");
        let now = Instant::now();
        for prior_loss in [false, true] {
            let mut workspace = Workspace::test_new("api-create");
            let lost = workspace.tabs[0].root_pane;
            workspace.test_add_tab(Some("survivor"));
            let mut app = audit_session_app(workspace, now);
            app.state.default_shell = exiting_test_command().into();
            app.state.shell_mode = crate::config::ShellModeConfig::NonLogin;
            if prior_loss {
                audit_interrupt_pane(&mut app, lost, now);
            } else {
                assert!(app.note_pane_exit_requires_checkpoint(ChildExitReason::Interrupted, now));
            }
            let response = app.handle_api_request(Request {
                id: "audit-p0-api-create".into(),
                method: Method::TabCreate(TabCreateParams {
                    workspace_id: None,
                    cwd: Some(std::env::temp_dir().display().to_string()),
                    focus: false,
                    label: Some("created".into()),
                    env: Default::default(),
                }),
            });
            let job = app.capture_session_save_job(now);
            shutdown_test_runtimes(&mut app);

            let response: SuccessResponse = serde_json::from_str(&response).unwrap();
            assert!(matches!(response.result, ResponseResult::TabCreated { .. }));
            app.state.assert_invariants_for_test();
            assert_eq!(job.is_some(), !prior_loss);
            assert_eq!(
                app.authorized_session_layout.as_ref().unwrap().pane_count(),
                3
            );
            let created = app.state.workspaces[0].tabs.last().unwrap().root_pane;
            audit_interrupt_pane(&mut app, created, now);
            assert!(
                app.capture_session_save_job(now).is_none(),
                "an API-created pane is protected before its first save is dispatched"
            );
        }
    }

    #[test]
    fn audit_p0_session_pane_move_authorizes_only_transferred_identities() {
        use crate::api::schema::{
            Method, PaneMoveDestination, PaneMoveParams, Request, ResponseResult, SplitDirection,
            SuccessResponse,
        };

        let _dirs = crate::config::test_dirs::isolate_dirs("audit-p0-session-pane-move");
        let now = Instant::now();
        for cross_workspace in [false, true] {
            for prior_loss in [false, true] {
                let mut source = Workspace::test_new("source");
                let moved = source.tabs[0].root_pane;
                let lost = prior_loss.then(|| source.test_split(Direction::Horizontal));
                if !cross_workspace {
                    source.test_add_tab(Some("target"));
                }
                let mut app = test_app(source);
                let target_tab = if cross_workspace {
                    app.state.workspaces.push(Workspace::test_new("target"));
                    app.public_tab_id(1, 0).unwrap()
                } else {
                    app.public_tab_id(0, 1).unwrap()
                };
                app.state.ensure_test_terminals();
                app.state.assert_invariants_for_test();
                let baseline = app.capture_session_save_job(now).unwrap();
                app.note_session_save_dispatched(&baseline);
                let previous_pane_id = app.public_pane_id(0, moved).unwrap();
                if let Some(lost) = lost {
                    audit_interrupt_pane(&mut app, lost, now);
                } else {
                    assert!(
                        app.note_pane_exit_requires_checkpoint(ChildExitReason::Interrupted, now)
                    );
                }
                let response = app.handle_api_request(Request {
                    id: "audit-p0-pane-move".into(),
                    method: Method::PaneMove(PaneMoveParams {
                        pane_id: previous_pane_id.clone(),
                        destination: PaneMoveDestination::Tab {
                            tab_id: target_tab.clone(),
                            target_pane_id: None,
                            split: SplitDirection::Right,
                            ratio: None,
                        },
                        focus: false,
                    }),
                });
                let response: SuccessResponse = serde_json::from_str(&response).unwrap();
                let ResponseResult::PaneMove { move_result } = response.result else {
                    panic!("expected pane move response");
                };
                assert!(move_result.changed);
                assert_eq!(move_result.pane.tab_id, target_tab);
                assert_eq!(
                    move_result.pane.pane_id != previous_pane_id,
                    cross_workspace
                );
                assert_eq!(app.parse_pane_id(&previous_pane_id), Some((0, moved)));
                assert_eq!(app.state.workspaces.len(), 1);
                assert_eq!(app.state.workspaces[0].tabs.len(), 1);
                app.state.assert_invariants_for_test();
                assert_eq!(
                    app.capture_session_save_job(now).is_some(),
                    !prior_loss,
                    "removing an empty source must not discard a pane lost before the move"
                );
                let Some(SessionSaveJob::Save { snapshot, .. }) =
                    app.capture_session_save_job(now + PANE_EXIT_CASCADE_GUARD)
                else {
                    panic!("the moved layout must become saveable after expiry");
                };
                assert_eq!(SessionLayout::from_snapshot(&snapshot).pane_count(), 2);
            }
        }
    }

    #[test]
    fn audit_p0_session_noop_and_rejected_move_do_not_authorize() {
        use crate::api::schema::{
            ErrorResponse, Method, PaneMoveDestination, PaneMoveParams, Request, ResponseResult,
            SplitDirection, SuccessResponse,
        };

        let _dirs = crate::config::test_dirs::isolate_dirs("audit-p0-session-noop");
        let now = Instant::now();
        for reject in [false, true] {
            let mut app = audit_session_app(Workspace::test_new("unchanged"), now);
            let before = app.capture_session_layout();
            app.authorize_session_layout_change(before);
            assert!(app.authorized_session_layout.is_none());
            let root = app.state.workspaces[0].tabs[0].root_pane;
            let tab_id = if reject {
                "missing-workspace:t1".into()
            } else {
                app.public_tab_id(0, 0).unwrap()
            };
            let response = app.handle_api_request(Request {
                id: "audit-p0-noop".into(),
                method: Method::PaneMove(PaneMoveParams {
                    pane_id: app.public_pane_id(0, root).unwrap(),
                    destination: PaneMoveDestination::Tab {
                        tab_id,
                        target_pane_id: None,
                        split: SplitDirection::Right,
                        ratio: None,
                    },
                    focus: false,
                }),
            });
            if reject {
                let response: ErrorResponse = serde_json::from_str(&response).unwrap();
                assert_eq!(response.error.code, "tab_not_found");
            } else {
                let response: SuccessResponse = serde_json::from_str(&response).unwrap();
                let ResponseResult::PaneMove { move_result } = response.result else {
                    panic!("expected unchanged pane move response");
                };
                assert!(!move_result.changed);
            }
            assert!(app.authorized_session_layout.is_none());
            app.state.assert_invariants_for_test();
        }
    }

    #[test]
    fn audit_p0_session_authorization_keeps_unidentified_layouts_protected() {
        let _dirs = crate::config::test_dirs::isolate_dirs("audit-p0-session-unidentified");
        let now = Instant::now();
        for unidentified in ["protected", "before", "after"] {
            let mut workspace = Workspace::test_new("unidentified");
            let root = workspace.tabs[0].root_pane;
            let tab = workspace.test_add_tab(Some("closed"));
            let closed = workspace.tabs[tab].root_pane;
            let closed_identity = (
                workspace.id.clone(),
                workspace.tabs[tab].number,
                workspace.public_pane_numbers[&closed],
            );
            let mut app = audit_session_app(workspace, now);
            assert!(app.note_pane_exit_requires_checkpoint(ChildExitReason::Interrupted, now));
            if unidentified == "protected" {
                app.persisted_session_layout.unidentified = true;
            }
            let mut missing_number = None;
            if unidentified == "before" {
                missing_number = app.state.workspaces[0].public_pane_numbers.remove(&root);
            }
            let before = app.capture_session_layout();
            assert!(app.state.workspaces[0].close_tab(tab));
            if unidentified == "after" {
                missing_number = app.state.workspaces[0].public_pane_numbers.remove(&root);
            }
            app.authorize_session_layout_change(before);
            if let Some(number) = missing_number {
                app.state.workspaces[0]
                    .public_pane_numbers
                    .insert(root, number);
            }
            let protected = app.authorized_session_layout.as_ref().unwrap();
            assert!(protected.unidentified);
            if unidentified != "protected" {
                assert!(protected.panes.contains(&closed_identity));
            }
            app.state.assert_invariants_for_test();
            assert!(app.capture_session_save_job(now).is_none());
            audit_close_pane(&mut app, root);
            assert!(app.capture_session_save_job(now).is_none());
            assert!(matches!(
                app.capture_session_save_job(now + PANE_EXIT_CASCADE_GUARD),
                Some(SessionSaveJob::Clear)
            ));
        }
    }

    #[test]
    fn audit_p0_session_default_replacement_accepts_effective_single_pane() {
        let _dirs = crate::config::test_dirs::isolate_dirs("audit-p0-session-default-after-close");
        let now = Instant::now();
        let mut workspace = Workspace::test_new("shrinking");
        let root = workspace.tabs[0].root_pane;
        let closed = workspace.test_split(Direction::Horizontal);
        let mut app = audit_session_app(workspace, now);
        assert!(app.note_pane_exit_requires_checkpoint(ChildExitReason::Interrupted, now));
        audit_close_pane(&mut app, closed);
        assert_eq!(app.persisted_session_layout.pane_count(), 2);
        assert_eq!(
            app.authorized_session_layout.as_ref().unwrap().pane_count(),
            1
        );
        audit_interrupt_pane(&mut app, root, now);
        assert!(app.state.workspaces.is_empty());
        app.state.workspaces = vec![Workspace::test_new("default")];
        app.state.active = Some(0);
        app.state.ensure_test_terminals();
        app.authorize_default_workspace_replacement();
        app.state.assert_invariants_for_test();
        assert!(app.capture_session_save_job(now).is_some());
    }
}
