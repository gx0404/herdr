//! pane 进程终止阶梯（HSR-01 / HSR-02）。
//!
//! 关闭 pane 时的「SIGHUP → SIGTERM → SIGKILL」阶梯以及枚举会话成员的全表扫描都跑在
//! 专用 reaper 线程上：事件循环只投递一个请求就立即返回，不再因为一个赖着不退的 pane
//! 把 PTY 输出、输入、渲染和其它 API 一起冻住。
//!
//! 两条不变量是「后台化」必须自己补上的：
//!
//! - **进程树的身份在事件循环里就固定下来**。请求携带投递时刻快照的
//!   [`ProcessSessionId`]，reaper 凭它枚举成员；否则 child 在投递与执行之间被 `wait`
//!   回收后锚点就消失了，孙进程再也找不到（漏杀），而回退到裸 `child_pid` 又可能打到
//!   被复用的无关进程（误杀）。成员带着枚举时的实例标记（[`ProcessSessionMember`]），
//!   升级信号时成员可能早已退出、pid 被别的进程拿走，平台据此核对后才发信号。
//! - **进程退出前等阶梯收尾**（[`drain_pending`]），否则 pane 进程会被留成孤儿。
//!
//! reaper 用**一个**轮询循环驱动所有在办 pane，每个 pane 有自己的阶梯进度：新投递的
//! 请求立刻登记并捕获成员，Windows 在后台 PTY 关闭后才开始首级宽限；关闭门限与阶梯预算
//! （[`LADDER_WORST_CASE`]：unix 750 ms，Windows 3 s）不随同时关闭的 pane 数增长。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use tracing::{debug, error, info, warn};

use crate::layout::PaneId;
use crate::platform::{ProcessSessionId, ProcessSessionMember, Signal};

/// 信号阶梯：每个 pane 依次收到三级信号，每级之间等一个宽限窗口。
const SIGNAL_LADDER: [(Signal, Duration); 3] = [
    (Signal::Hangup, Duration::from_millis(250)),
    (Signal::Terminate, Duration::from_millis(250)),
    (Signal::Kill, Duration::from_millis(KILL_GRACE_MS)),
];

/// 最后一级（SIGKILL；Windows 上是 TerminateProcess）之后等进程真正退出的宽限。Windows 的
/// 进程要等进程对象 signaled（句柄、cwd 都已释放）才算退出，负载下被终止后还要半秒以上；
/// 宽限太短会误报「强杀后仍存活」，`drain_pending` 也会在句柄释放前返回。轮询在进程退出时
/// 立即摘除它，正常情况下等不满。
const KILL_GRACE_MS: u64 = if cfg!(windows) { 2000 } else { 250 };

/// PTY 关闭门限与整条阶梯的宽限之和，供调用方推导 [`drain_pending`] 的超时。
pub(crate) const LADDER_WORST_CASE: Duration = {
    #[cfg(windows)]
    let close = crate::pty::actor::PTY_CLOSE_TIMEOUT;
    #[cfg(unix)]
    let close = Duration::ZERO;
    close.saturating_add(Duration::from_millis(500 + KILL_GRACE_MS))
};

/// 首级几乎总是在几毫秒内命中（shell 收到 SIGHUP 立刻退出），所以首级用 [`FAST_POLL`]
/// 轮询做到「命中即摘除」；升级到 SIGTERM/SIGKILL 的进程本来就不会很快退出，后续级别
/// 用 [`SLOW_POLL`]，不为已知的顽固进程白白多出几十次唤醒。
const FAST_POLL: Duration = Duration::from_millis(2);
const FAST_POLL_WINDOW: Duration = Duration::from_millis(60);
const SLOW_POLL: Duration = Duration::from_millis(10);
/// 轮询最短间隔：宽限窗口为 0（测试用的退化阶梯）时也不把循环变成忙等。
const MIN_POLL: Duration = Duration::from_micros(200);

/// 一个 pane 的终止请求。
///
/// `child_wait_completed` 是 spawn 路径上 wait 线程回收子进程后置位的标志；handoff
/// 导入的 pane 没有这个标志（子进程不是本进程的子进程）。`session` 是投递时刻的会话
/// 锚点，见模块文档。
pub(crate) struct PaneShutdownRequest {
    pane_id: PaneId,
    child_pid: u32,
    child_wait_completed: Option<Arc<AtomicBool>>,
    session: Option<ProcessSessionId>,
    #[cfg(windows)]
    pty_close: Option<crate::pty::actor::PtyCloseCompletion>,
}

impl PaneShutdownRequest {
    /// 在事件循环里构造：会话锚点**必须**在这里快照。这是一次廉价查询（Linux 上是单个
    /// `/proc/<pid>/stat` 读），不是计划要求移出事件循环的全表扫描；但它必须发生在
    /// child 还活着的时候，否则 reaper 拿到请求时进程树就没有可靠身份了。
    ///
    /// 快照**之后**才看 `child_wait_completed`：已经置位说明 child 已被回收、pid 可能已被
    /// 复用，读到的锚点可能属于无关进程，宁可不发信号。Windows 的 wait 线程先置位、再释放
    /// child 句柄（pid 要等句柄释放才会被复用），所以此时仍未置位就说明快照那一刻 pid 还属于
    /// child。Unix 回收时 pid 就已释放，这里挡不住置位前的那一小段，但 Unix 的 pid 递增分配，
    /// 不会在这么短的时间里被复用。
    pub(crate) fn new(
        pane_id: PaneId,
        child_pid: u32,
        child_wait_completed: Option<Arc<AtomicBool>>,
    ) -> Self {
        let session = crate::platform::process_session_id(child_pid);
        let child_reaped = child_wait_completed
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Acquire));
        let session = session.filter(|_| !child_reaped);
        Self {
            pane_id,
            child_pid,
            child_wait_completed,
            session,
            #[cfg(windows)]
            pty_close: None,
        }
    }

    #[cfg(windows)]
    pub(super) fn after_pty_close(
        mut self,
        completion: Option<crate::pty::actor::PtyCloseCompletion>,
    ) -> Self {
        self.pty_close = completion;
        self
    }

    #[cfg(test)]
    fn with_session(
        pane_id: PaneId,
        child_pid: u32,
        child_wait_completed: Option<Arc<AtomicBool>>,
        session: Option<ProcessSessionId>,
    ) -> Self {
        Self {
            pane_id,
            child_pid,
            child_wait_completed,
            session,
            #[cfg(windows)]
            pty_close: None,
        }
    }
}

#[cfg(all(test, windows))]
type TestSubmit = Arc<dyn Fn(PaneShutdownRequest)>;

#[cfg(all(test, windows))]
thread_local! {
    static TEST_SUBMIT: std::cell::RefCell<Option<TestSubmit>> = const { std::cell::RefCell::new(None) };
}

/// 把一个 pane 的终止阶梯交给 reaper 线程；调用方（事件循环）立即返回。
pub(crate) fn submit(request: PaneShutdownRequest) {
    if request.child_pid == 0 {
        return;
    }
    #[cfg(all(test, windows))]
    if let Some(submit) = TEST_SUBMIT.with(|hook| hook.borrow().clone()) {
        submit(request);
        return;
    }
    reaper().submit(request);
}

/// 等待已投递的终止阶梯收尾，最多等 `timeout`。返回是否等到全部收尾。
///
/// 进程退出前调用：阶梯移出事件循环后，只有这里能保证 pane 进程不被留成孤儿。
pub(crate) fn drain_pending(timeout: Duration) -> bool {
    let Some(reaper) = REAPER.get() else {
        // 本进程从未投递过终止请求，reaper 线程也就没起来。
        return true;
    };
    reaper.drain(timeout)
}

/// 阶梯对平台进程操作的依赖面。生产实现直接转发到 `crate::platform`；测试用假实现在
/// 毫秒级窗口里确定性地驱动阶梯，不触碰真实进程。
trait ProcessControl {
    /// 进程当前所属会话；进程已消失时返回 `None`。
    fn session_id(&self, pid: u32) -> Option<ProcessSessionId>;
    /// 一次扫描取出整批会话的成员，返回与 `sessions` 一一对应的桶。
    fn session_processes(&self, sessions: &[ProcessSessionId]) -> Vec<Vec<ProcessSessionMember>>;
    fn signal_processes(&self, members: &[ProcessSessionMember], signal: Signal);
    /// 成员是否仍需等待它退出：僵尸（已退出未被回收）按已退出处理，见 HSR-02；pid 已换了
    /// 主人也按已退出处理。
    fn process_alive(&self, member: ProcessSessionMember) -> bool;
}

struct PlatformProcessControl;

impl ProcessControl for PlatformProcessControl {
    fn session_id(&self, pid: u32) -> Option<ProcessSessionId> {
        crate::platform::process_session_id(pid)
    }

    fn session_processes(&self, sessions: &[ProcessSessionId]) -> Vec<Vec<ProcessSessionMember>> {
        crate::platform::session_members_batch(sessions)
    }

    fn signal_processes(&self, members: &[ProcessSessionMember], signal: Signal) {
        crate::platform::signal_session_members(members, signal);
    }

    fn process_alive(&self, member: ProcessSessionMember) -> bool {
        crate::platform::session_member_alive(member)
    }
}

/// 阶梯执行期间的一个 pane：进程集合在进入阶梯前扫描一次，之后每轮只摘除已退出的成员。
struct ReapTarget {
    pane_id: PaneId,
    child_pid: u32,
    child_wait_completed: Option<Arc<AtomicBool>>,
    members: Vec<ProcessSessionMember>,
    /// 已经发出的信号级数，即下一次要发 `ladder[stage]`。
    stage: usize,
    last_signal: Option<Signal>,
    stage_started: Instant,
    next_signal_at: Instant,
    #[cfg(windows)]
    pty_close: Option<crate::pty::actor::PtyCloseCompletion>,
}

impl ReapTarget {
    fn new(request: PaneShutdownRequest, members: Vec<ProcessSessionMember>, now: Instant) -> Self {
        Self {
            pane_id: request.pane_id,
            child_pid: request.child_pid,
            child_wait_completed: request.child_wait_completed,
            members,
            stage: 0,
            last_signal: None,
            stage_started: now,
            next_signal_at: now,
            #[cfg(windows)]
            pty_close: request.pty_close,
        }
    }

    fn next_wakeup(&self) -> Instant {
        #[cfg(windows)]
        if let Some(completion) = &self.pty_close {
            return completion.deadline();
        }
        self.next_signal_at
    }

    /// 摘掉已经退出的成员；全部退出时返回 true。
    ///
    /// 每级发信号前都先摘一遍：已被 `wait` 回收（pid 可能已被复用）或已经退出的进程
    /// 不该再收到信号。
    fn prune_exited(&mut self, control: &impl ProcessControl) -> bool {
        let child_wait_completed = self
            .child_wait_completed
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Acquire));
        let child_pid = self.child_pid;
        self.members.retain(|member| {
            process_alive_for_shutdown(member.pid, child_pid, child_wait_completed, |_| {
                control.process_alive(*member)
            })
        });
        self.members.is_empty()
    }
}

/// 某个 pid 在终止阶梯里是否还算活着。自己 spawn 的直接子进程一旦被 wait 回收就按已退出
/// 处理（此时 pid 可能已被复用）；其余进程交给平台存活判定。
fn process_alive_for_shutdown(
    pid: u32,
    child_pid: u32,
    child_wait_completed: bool,
    process_alive: impl FnOnce(u32) -> bool,
) -> bool {
    if pid == child_pid && child_wait_completed {
        return false;
    }
    process_alive(pid)
}

/// 本轮轮询间隔：只有还在首级宽限窗口里的 pane 值得快轮询。
fn poll_interval(targets: &[ReapTarget], now: Instant) -> Duration {
    let in_first_stage = targets.iter().any(|target| {
        target.stage <= 1 && now.saturating_duration_since(target.stage_started) < FAST_POLL_WINDOW
    });
    if in_first_stage {
        FAST_POLL
    } else {
        SLOW_POLL
    }
}

/// 下一次唤醒前可以睡多久：不超过轮询间隔，也不越过最近一次信号升级的时刻。
fn sleep_budget(targets: &[ReapTarget], now: Instant) -> Duration {
    let poll = poll_interval(targets, now);
    let until_next_signal = targets
        .iter()
        .map(|target| target.next_wakeup().saturating_duration_since(now))
        .min()
        .unwrap_or(poll);
    poll.min(until_next_signal).max(MIN_POLL)
}

/// 把一批请求变成阶梯目标：整批只做**一次**会话扫描。
///
/// 每个请求都按投递时刻的会话锚点解析成员，因此 child 已被回收也不影响找到孙进程；
/// 找不到任何成员时只有在 child 仍属于同一会话时才回退到它，避免对被复用的 pid 发信号。
fn prepare_targets(
    requests: Vec<PaneShutdownRequest>,
    control: &impl ProcessControl,
) -> Vec<ReapTarget> {
    let mut targets = Vec::with_capacity(requests.len());
    let mut anchored: Vec<(PaneShutdownRequest, ProcessSessionId)> =
        Vec::with_capacity(requests.len());
    for request in requests {
        let Some(session) = request.session else {
            // 投递时就读不到会话锚点：child 在离开事件循环前已经消失，没有任何可靠身份
            // 可以据此发信号——对可能已被复用的 pid 发 SIGKILL 比漏发更糟。
            debug!(
                pane = request.pane_id.raw(),
                pid = request.child_pid,
                "pane session anchor was already gone when shutdown was queued"
            );
            #[cfg(windows)]
            if request.pty_close.is_some() {
                targets.push(ReapTarget::new(request, Vec::new(), Instant::now()));
            }
            continue;
        };
        anchored.push((request, session));
    }
    if anchored.is_empty() {
        return targets;
    }

    let sessions: Vec<ProcessSessionId> = anchored.iter().map(|(_, session)| *session).collect();
    let buckets = control.session_processes(&sessions);
    let now = Instant::now();

    // 平台实现返回的桶数与 sessions 等长；万一更短也不能 panic，缺的按「扫不到成员」处理。
    let buckets = buckets.into_iter().chain(std::iter::repeat_with(Vec::new));
    for ((request, session), mut members) in anchored.into_iter().zip(buckets) {
        if members.is_empty() {
            match control.session_id(request.child_pid) {
                Some(current) if current.same_root(session) => {
                    members.push(ProcessSessionMember {
                        pid: request.child_pid,
                        instance: current.instance,
                    });
                }
                _ => {
                    debug!(
                        pane = request.pane_id.raw(),
                        pid = request.child_pid,
                        "pane session had no live processes left"
                    );
                    #[cfg(windows)]
                    if request.pty_close.is_some() {
                        targets.push(ReapTarget::new(request, Vec::new(), now));
                    }
                    continue;
                }
            }
        }
        members.sort_unstable();
        members.dedup();
        targets.push(ReapTarget::new(request, members, now));
    }
    targets
}

/// 推进一轮：先摘除已退出的 pane，再给宽限窗口用尽的 pane 发下一级信号。
fn advance_targets(
    targets: &mut Vec<ReapTarget>,
    ladder: &[(Signal, Duration)],
    control: &impl ProcessControl,
    now: Instant,
) {
    targets.retain_mut(|target| {
        #[cfg(windows)]
        if let Some(completion) = &target.pty_close {
            let Some(ready_at) = completion.ready_at(now) else {
                return true;
            };
            target.pty_close = None;
            target.stage_started = ready_at;
            target.next_signal_at = ready_at;
        }
        if target.prune_exited(control) {
            info!(
                pane = target.pane_id.raw(),
                pid = target.child_pid,
                signal = ?target.last_signal,
                "pane session terminated"
            );
            return false;
        }
        if now < target.next_signal_at {
            return true;
        }
        let Some((signal, grace)) = ladder.get(target.stage) else {
            let pids: Vec<u32> = target.members.iter().map(|member| member.pid).collect();
            warn!(
                pane = target.pane_id.raw(),
                pid = target.child_pid,
                pids = ?pids,
                "pane session still alive after forced shutdown"
            );
            return false;
        };
        control.signal_processes(&target.members, *signal);
        target.stage += 1;
        target.last_signal = Some(*signal);
        target.stage_started = now;
        target.next_signal_at = now + *grace;
        true
    });
}

/// 降级路径：reaper 线程起不来时在调用方线程上把阶梯跑完。宁可阻塞也不漏杀进程。
fn run_ladder_blocking(
    requests: Vec<PaneShutdownRequest>,
    ladder: &[(Signal, Duration)],
    control: &impl ProcessControl,
) {
    let mut targets = prepare_targets(requests, control);
    while !targets.is_empty() {
        advance_targets(&mut targets, ladder, control, Instant::now());
        if targets.is_empty() {
            break;
        }
        std::thread::sleep(sleep_budget(&targets, Instant::now()));
    }
}

#[derive(Default)]
struct ReaperState {
    queue: VecDeque<PaneShutdownRequest>,
    /// 已从队列取走、阶梯尚未走完的 pane 数量。`drain` 用它判断是否收尾。
    in_flight: usize,
    stopped: bool,
}

/// 专用 reaper：事件循环投递，后台线程执行阶梯。
struct Reaper {
    state: Mutex<ReaperState>,
    has_work: Condvar,
    idle: Condvar,
    worker_started: AtomicBool,
    worker: OnceLock<std::thread::JoinHandle<()>>,
}

static REAPER: OnceLock<&'static Reaper> = OnceLock::new();

fn reaper() -> &'static Reaper {
    REAPER.get_or_init(|| {
        // 进程生命周期内只有一个 reaper；泄漏这一个分配换取后台线程可以持有 'static 引用。
        let reaper: &'static Reaper = Box::leak(Box::new(Reaper::new()));
        match std::thread::Builder::new()
            .name("herdr-pane-reaper".to_owned())
            .spawn(|| reaper.run())
        {
            Ok(handle) => {
                // 保留 handle 只为探活：线程若意外结束，`drain` 立刻报告而不是空等满超时。
                let _ = reaper.worker.set(handle);
                reaper.worker_started.store(true, Ordering::Release);
            }
            Err(err) => warn!(
                err = %err,
                "failed to start pane reaper thread; terminating pane processes inline"
            ),
        }
        reaper
    })
}

fn lock_state(reaper: &Reaper) -> MutexGuard<'_, ReaperState> {
    reaper
        .state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Reaper {
    fn new() -> Self {
        Self {
            state: Mutex::new(ReaperState::default()),
            has_work: Condvar::new(),
            idle: Condvar::new(),
            worker_started: AtomicBool::new(false),
            worker: OnceLock::new(),
        }
    }

    fn submit(&self, request: PaneShutdownRequest) {
        if !self.worker_started.load(Ordering::Acquire) {
            // reaper 线程起不来时宁可阻塞调用方，也不能把 pane 进程留下不管。
            run_ladder_blocking(vec![request], &SIGNAL_LADDER, &PlatformProcessControl);
            return;
        }
        {
            let mut state = lock_state(self);
            state.queue.push_back(request);
        }
        self.has_work.notify_one();
    }

    fn run(&self) {
        self.run_with(&PlatformProcessControl, &SIGNAL_LADDER);
    }

    /// reaper 主循环：一个轮询循环驱动所有在办 pane，新请求随时并入。
    ///
    /// 单批次内的 panic 不会让终止机制静默失效：捕获后丢掉本轮目标、复位计数并继续服务，
    /// 否则 `worker_started` 仍为 true、请求照常入队却再没有消费者。
    fn run_with(&self, control: &impl ProcessControl, ladder: &[(Signal, Duration)]) {
        let mut targets: Vec<ReapTarget> = Vec::new();
        loop {
            let pending = {
                let mut state = lock_state(self);
                while state.queue.is_empty() {
                    if !targets.is_empty() {
                        // 有在办 pane：最多睡到下一次轮询/升级时刻，新投递会提前唤醒。
                        let budget = sleep_budget(&targets, Instant::now());
                        let (next, _timeout) = self
                            .has_work
                            .wait_timeout(state, budget)
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        state = next;
                        break;
                    }
                    if state.stopped {
                        return;
                    }
                    state = self
                        .has_work
                        .wait(state)
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                }
                let pending: Vec<PaneShutdownRequest> = state.queue.drain(..).collect();
                state.in_flight = targets.len() + pending.len();
                pending
            };

            if !pending.is_empty() {
                debug!(panes = pending.len(), "reaping pane sessions");
            }
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if !pending.is_empty() {
                    targets.extend(prepare_targets(pending, control));
                }
                advance_targets(&mut targets, ladder, control, Instant::now());
            }));
            if outcome.is_err() {
                error!("pane reaper batch panicked; dropping its targets and continuing");
                targets.clear();
            }

            let idle = {
                let mut state = lock_state(self);
                state.in_flight = targets.len();
                state.in_flight == 0 && state.queue.is_empty()
            };
            if idle {
                self.idle.notify_all();
            }
        }
    }

    /// 只给测试用：让 `run_with` 在队列排空后退出循环。
    #[cfg(test)]
    fn stop(&self) {
        lock_state(self).stopped = true;
        self.has_work.notify_all();
    }

    /// worker 线程是否已经结束（正常只会在进程退出时发生）。结束后队列再无消费者，
    /// `drain` 必须立刻报告而不是空等满超时。
    fn worker_finished(&self) -> bool {
        self.worker.get().is_some_and(|handle| handle.is_finished())
    }

    fn drain(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut state = lock_state(self);
        while !state.queue.is_empty() || state.in_flight > 0 {
            if self.worker_finished() {
                warn!("pane reaper thread is gone; pending pane sessions were not reaped");
                return false;
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                warn!("timed out waiting for pane sessions to terminate");
                return false;
            };
            if remaining.is_zero() {
                warn!("timed out waiting for pane sessions to terminate");
                return false;
            }
            let (next, result) = self
                .idle
                .wait_timeout(state, remaining)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state = next;
            if result.timed_out() && (!state.queue.is_empty() || state.in_flight > 0) {
                warn!("timed out waiting for pane sessions to terminate");
                return false;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU64, AtomicUsize};

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum FakeState {
        Running,
        /// 已退出但仍在进程表里（HSR-02）：能被会话扫描列出，但阶梯按已退出处理。
        Zombie,
        Gone,
    }

    #[derive(Clone, Copy)]
    struct FakeProcess {
        session: i64,
        /// 进程实例标记（对应 Windows 的创建时间）：pid 被复用后换成另一个值。
        instance: u64,
        state: FakeState,
        dies_on: Option<Signal>,
        zombies_on: Option<Signal>,
    }

    /// 假进程表：记录收到的信号，并在指定信号到达后改变进程状态。
    struct FakeProcesses {
        procs: Mutex<HashMap<u32, FakeProcess>>,
        signals: Mutex<Vec<(Vec<u32>, Signal)>>,
        alive_probes: AtomicUsize,
        panic_on_next_signal: AtomicBool,
        /// 每次读锚点都给出新的快照时刻（与 Windows 一样），同一个根只能靠 `same_root` 相认。
        captures: AtomicU64,
    }

    impl FakeProcesses {
        fn new() -> Self {
            Self {
                procs: Mutex::new(HashMap::new()),
                signals: Mutex::new(Vec::new()),
                alive_probes: AtomicUsize::new(0),
                panic_on_next_signal: AtomicBool::new(false),
                captures: AtomicU64::new(0),
            }
        }

        /// 一个会话：`pids` 全部存活，收到 `dies_on` 后消失。
        fn session(session: i64, pids: &[u32], dies_on: Option<Signal>) -> Self {
            let fake = Self::new();
            for pid in pids {
                fake.insert(
                    *pid,
                    FakeProcess {
                        session,
                        instance: 0,
                        state: FakeState::Running,
                        dies_on,
                        zombies_on: None,
                    },
                );
            }
            fake
        }

        fn insert(&self, pid: u32, process: FakeProcess) {
            if let Ok(mut procs) = self.procs.lock() {
                procs.insert(pid, process);
            }
        }

        fn set_state(&self, pid: u32, state: FakeState) {
            if let Ok(mut procs) = self.procs.lock() {
                if let Some(process) = procs.get_mut(&pid) {
                    process.state = state;
                }
            }
        }

        fn signal_log(&self) -> Vec<Signal> {
            self.signals
                .lock()
                .map(|log| log.iter().map(|(_, signal)| *signal).collect())
                .unwrap_or_default()
        }

        fn signals_for(&self, pid: u32) -> Vec<Signal> {
            self.signals
                .lock()
                .map(|log| {
                    log.iter()
                        .filter(|(pids, _)| pids.contains(&pid))
                        .map(|(_, signal)| *signal)
                        .collect()
                })
                .unwrap_or_default()
        }

        fn signalled_pid_sets(&self) -> Vec<Vec<u32>> {
            self.signals
                .lock()
                .map(|log| log.iter().map(|(pids, _)| pids.clone()).collect())
                .unwrap_or_default()
        }
    }

    impl ProcessControl for FakeProcesses {
        fn session_id(&self, pid: u32) -> Option<ProcessSessionId> {
            let procs = self.procs.lock().ok()?;
            let process = procs.get(&pid)?;
            (process.state != FakeState::Gone).then(|| ProcessSessionId {
                id: process.session,
                instance: process.instance,
                captured: self.captures.fetch_add(1, Ordering::Relaxed) + 1,
            })
        }

        fn session_processes(
            &self,
            sessions: &[ProcessSessionId],
        ) -> Vec<Vec<ProcessSessionMember>> {
            let Ok(procs) = self.procs.lock() else {
                return vec![Vec::new(); sessions.len()];
            };
            sessions
                .iter()
                .map(|session| {
                    let mut members: Vec<ProcessSessionMember> = procs
                        .iter()
                        .filter(|(_, process)| {
                            process.session == session.id && process.state != FakeState::Gone
                        })
                        .map(|(pid, process)| ProcessSessionMember {
                            pid: *pid,
                            instance: process.instance,
                        })
                        .collect();
                    members.sort_unstable();
                    members
                })
                .collect()
        }

        fn signal_processes(&self, members: &[ProcessSessionMember], signal: Signal) {
            if self.panic_on_next_signal.swap(false, Ordering::SeqCst) {
                panic!("injected reaper panic");
            }
            if let Ok(mut log) = self.signals.lock() {
                log.push((members.iter().map(|member| member.pid).collect(), signal));
            }
            let updates: Vec<(u32, FakeState)> = {
                let Ok(procs) = self.procs.lock() else {
                    return;
                };
                members
                    .iter()
                    .filter_map(|member| {
                        // 平台按实例核对后才发信号：pid 换了主人的进程收不到。
                        let process = procs
                            .get(&member.pid)
                            .filter(|process| process.instance == member.instance)?;
                        if process.dies_on == Some(signal) {
                            Some((member.pid, FakeState::Gone))
                        } else if process.zombies_on == Some(signal) {
                            Some((member.pid, FakeState::Zombie))
                        } else {
                            None
                        }
                    })
                    .collect()
            };
            for (pid, state) in updates {
                self.set_state(pid, state);
            }
        }

        fn process_alive(&self, member: ProcessSessionMember) -> bool {
            self.alive_probes.fetch_add(1, Ordering::Relaxed);
            self.procs
                .lock()
                .map(|procs| {
                    procs.get(&member.pid).is_some_and(|process| {
                        process.instance == member.instance && process.state == FakeState::Running
                    })
                })
                .unwrap_or(false)
        }
    }

    fn test_ladder() -> Vec<(Signal, Duration)> {
        vec![
            (Signal::Hangup, Duration::from_millis(20)),
            (Signal::Terminate, Duration::from_millis(20)),
            (Signal::Kill, Duration::from_millis(20)),
        ]
    }

    /// 不带实例标记的锚点（Unix 会话 id 的形态）。
    fn anchor(session: i64) -> ProcessSessionId {
        ProcessSessionId {
            id: session,
            instance: 0,
            captured: 0,
        }
    }

    fn request(pane: u32, pid: u32, session: i64) -> PaneShutdownRequest {
        PaneShutdownRequest::with_session(PaneId::from_raw(pane), pid, None, Some(anchor(session)))
    }

    fn target_in_stage(stage: usize, stage_started: Instant) -> ReapTarget {
        ReapTarget {
            pane_id: PaneId::from_raw(1),
            child_pid: 7,
            child_wait_completed: None,
            members: vec![ProcessSessionMember {
                pid: 7,
                instance: 0,
            }],
            stage,
            last_signal: None,
            stage_started,
            next_signal_at: stage_started,
            #[cfg(windows)]
            pty_close: None,
        }
    }

    #[test]
    fn shutdown_liveness_treats_reaped_direct_child_as_gone() {
        assert!(!process_alive_for_shutdown(42, 42, true, |_| true));
    }

    #[test]
    fn shutdown_liveness_keeps_unreaped_direct_child_alive() {
        assert!(process_alive_for_shutdown(42, 42, false, |_| true));
    }

    #[test]
    fn shutdown_liveness_keeps_other_session_processes_alive() {
        assert!(process_alive_for_shutdown(43, 42, true, |_| true));
    }

    #[test]
    fn shutdown_liveness_treats_missing_process_as_gone() {
        assert!(!process_alive_for_shutdown(43, 42, false, |_| false));
    }

    #[test]
    fn shutdown_polls_fast_only_in_the_first_grace_window() {
        let now = Instant::now();
        assert_eq!(poll_interval(&[target_in_stage(1, now)], now), FAST_POLL);
        assert_eq!(
            poll_interval(&[target_in_stage(1, now - FAST_POLL_WINDOW)], now),
            SLOW_POLL,
            "首级窗口用尽后不再快轮询"
        );
        assert_eq!(
            poll_interval(&[target_in_stage(2, now)], now),
            SLOW_POLL,
            "升到 SIGTERM 的进程不值得再快轮询"
        );
        assert!(FAST_POLL <= Duration::from_millis(5));
    }

    #[test]
    fn shutdown_sleep_budget_never_overshoots_the_next_escalation() {
        let now = Instant::now();
        let mut target = target_in_stage(2, now);
        target.next_signal_at = now + Duration::from_millis(3);
        assert_eq!(
            sleep_budget(&[target], now),
            Duration::from_millis(3),
            "睡到升级时刻为止，不越过它"
        );
    }

    #[test]
    fn shutdown_ladder_stops_at_hangup_when_session_exits() {
        let control = FakeProcesses::session(5, &[7, 9], Some(Signal::Hangup));
        run_ladder_blocking(vec![request(1, 7, 5)], &test_ladder(), &control);
        assert_eq!(control.signal_log(), vec![Signal::Hangup]);
    }

    #[test]
    fn shutdown_ladder_escalates_to_kill_for_stubborn_session() {
        let control = FakeProcesses::session(5, &[7], Some(Signal::Kill));
        run_ladder_blocking(vec![request(1, 7, 5)], &test_ladder(), &control);
        assert_eq!(
            control.signal_log(),
            vec![Signal::Hangup, Signal::Terminate, Signal::Kill]
        );
    }

    #[test]
    fn shutdown_ladder_stops_at_hangup_when_the_session_leaves_a_zombie() {
        // HSR-02：handoff 导入的 pane 没有 child_wait_completed，其子进程退出后留成僵尸
        // （`kill(pid, 0)` 仍成功）。僵尸按已退出处理，阶梯必须在首级就摘除这个 pane，
        // 而不是走满 750 ms 才 SIGKILL。
        let control = FakeProcesses::new();
        control.insert(
            7,
            FakeProcess {
                session: 5,
                instance: 0,
                state: FakeState::Running,
                dies_on: None,
                zombies_on: Some(Signal::Hangup),
            },
        );
        run_ladder_blocking(vec![request(1, 7, 5)], &test_ladder(), &control);
        assert_eq!(
            control.signal_log(),
            vec![Signal::Hangup],
            "僵尸不应把阶梯拖到 SIGTERM/SIGKILL"
        );
    }

    #[test]
    fn shutdown_ladder_prunes_a_member_whose_pid_now_names_another_instance() {
        // 成员在两级信号之间退出、pid 被无关进程拿走：按实例核对它已退出，不能把下一级
        // 信号发给新主人。
        let control = FakeProcesses::new();
        control.insert(
            7,
            FakeProcess {
                session: 5,
                instance: 2,
                state: FakeState::Running,
                dies_on: Some(Signal::Terminate),
                zombies_on: None,
            },
        );
        let now = Instant::now();
        let mut targets = vec![ReapTarget {
            members: vec![ProcessSessionMember {
                pid: 7,
                instance: 1,
            }],
            ..target_in_stage(1, now)
        }];
        advance_targets(&mut targets, &test_ladder(), &control, now);
        assert!(targets.is_empty(), "原来的成员已经退出，pane 应当收尾");
        assert!(control.signal_log().is_empty(), "新主人不得收到信号");
    }

    #[test]
    fn shutdown_request_drops_the_anchor_once_the_child_was_reaped() {
        // 锚点快照之后 child 已被回收：pid 可能已经属于无关进程，读到的锚点不可信。
        let pid = std::process::id();
        let pane = PaneId::from_raw(1);
        let unreaped = Arc::new(AtomicBool::new(false));
        assert!(
            PaneShutdownRequest::new(pane, pid, Some(unreaped))
                .session
                .is_some(),
            "child 未被回收时应快照到锚点"
        );
        let reaped = Arc::new(AtomicBool::new(true));
        assert_eq!(
            PaneShutdownRequest::new(pane, pid, Some(reaped)).session,
            None
        );
        assert!(
            PaneShutdownRequest::new(pane, pid, None).session.is_some(),
            "没有回收标志的 pane（handoff 导入）照旧快照"
        );
    }

    #[test]
    fn shutdown_ladder_skips_sessions_without_a_session_anchor() {
        let control = FakeProcesses::session(5, &[7], Some(Signal::Hangup));
        run_ladder_blocking(
            vec![PaneShutdownRequest::with_session(
                PaneId::from_raw(1),
                7,
                None,
                None,
            )],
            &test_ladder(),
            &control,
        );
        assert!(control.signal_log().is_empty());
    }

    /// 把会话扫描强制变成「扫不到任何成员」，其余行为透传：用来覆盖平台扫描落空
    /// （Windows 快照拿不到进程树、/proc 竞态）时的 child_pid 回退。
    struct EmptyScan<'a>(&'a FakeProcesses);

    impl ProcessControl for EmptyScan<'_> {
        fn session_id(&self, pid: u32) -> Option<ProcessSessionId> {
            self.0.session_id(pid)
        }

        fn session_processes(
            &self,
            sessions: &[ProcessSessionId],
        ) -> Vec<Vec<ProcessSessionMember>> {
            vec![Vec::new(); sessions.len()]
        }

        fn signal_processes(&self, members: &[ProcessSessionMember], signal: Signal) {
            self.0.signal_processes(members, signal);
        }

        fn process_alive(&self, member: ProcessSessionMember) -> bool {
            self.0.process_alive(member)
        }
    }

    #[test]
    fn shutdown_ladder_falls_back_to_the_child_pid_when_the_scan_finds_nothing() {
        // 扫不到会话成员，但 child 仍属于请求携带的会话：回退到 child_pid 是安全的。假进程表
        // 每次给出新的快照时刻，回退也要认得出同一个根。
        let control = FakeProcesses::session(5, &[4242], Some(Signal::Hangup));
        run_ladder_blocking(
            vec![request(1, 4242, 5)],
            &test_ladder(),
            &EmptyScan(&control),
        );
        assert_eq!(control.signalled_pid_sets(), vec![vec![4242]]);
        assert_eq!(control.signal_log(), vec![Signal::Hangup]);
    }

    #[test]
    fn shutdown_ladder_does_not_fall_back_to_another_instance_of_the_child_pid() {
        // Windows 的锚点 id 就是 child pid：child 被回收后 pid 落到新进程手里，id 对得上、
        // 实例对不上，回退必须放弃。
        let control = FakeProcesses::new();
        control.insert(
            4242,
            FakeProcess {
                session: 4242,
                instance: 2,
                state: FakeState::Running,
                dies_on: Some(Signal::Hangup),
                zombies_on: None,
            },
        );
        let anchored = ProcessSessionId {
            id: 4242,
            instance: 1,
            captured: 3,
        };
        run_ladder_blocking(
            vec![PaneShutdownRequest::with_session(
                PaneId::from_raw(1),
                4242,
                None,
                Some(anchored),
            )],
            &test_ladder(),
            &EmptyScan(&control),
        );
        assert!(control.signal_log().is_empty());
    }

    #[test]
    fn shutdown_ladder_drops_a_request_whose_child_pid_was_recycled() {
        // child 在投递与执行之间被回收、pid 被另一个会话复用：既扫不到原会话成员，
        // session_id(child) 也对不上，必须整条丢弃而不是对无辜进程发信号。
        let control = FakeProcesses::new();
        control.insert(
            7,
            FakeProcess {
                session: 99,
                instance: 0,
                state: FakeState::Running,
                dies_on: None,
                zombies_on: None,
            },
        );
        run_ladder_blocking(vec![request(1, 7, 5)], &test_ladder(), &control);
        assert!(
            control.signal_log().is_empty(),
            "不得对复用了 pid 的无关进程发信号"
        );
    }

    #[test]
    fn shutdown_ladder_signals_grandchildren_after_the_child_was_reaped() {
        // HSR-01 的核心竞态：shell（child）在请求排队期间被 wait 回收，`/proc/<child>`
        // 消失。凭投递时刻的会话锚点仍然能找到孙进程，阶梯照常发信号。
        let control = FakeProcesses::new();
        control.insert(
            7,
            FakeProcess {
                session: 5,
                instance: 0,
                state: FakeState::Gone,
                dies_on: None,
                zombies_on: None,
            },
        );
        control.insert(
            9,
            FakeProcess {
                session: 5,
                instance: 0,
                state: FakeState::Running,
                dies_on: Some(Signal::Terminate),
                zombies_on: None,
            },
        );
        let child_wait_completed = Arc::new(AtomicBool::new(true));
        run_ladder_blocking(
            vec![PaneShutdownRequest::with_session(
                PaneId::from_raw(1),
                7,
                Some(child_wait_completed),
                Some(anchor(5)),
            )],
            &test_ladder(),
            &control,
        );
        assert_eq!(
            control.signals_for(9),
            vec![Signal::Hangup, Signal::Terminate],
            "孙进程必须仍然走阶梯"
        );
        assert!(
            control.signals_for(7).is_empty(),
            "已被回收的 child pid 不该再收到信号"
        );
    }

    #[test]
    fn shutdown_ladder_does_not_signal_a_reaped_direct_child() {
        // 会话里只剩已被 wait 回收的 child：pid 可能已被复用，一个信号都不该发。
        let child_wait_completed = Arc::new(AtomicBool::new(true));
        let control = FakeProcesses::session(5, &[7], None);
        run_ladder_blocking(
            vec![PaneShutdownRequest::with_session(
                PaneId::from_raw(1),
                7,
                Some(child_wait_completed),
                Some(anchor(5)),
            )],
            &test_ladder(),
            &control,
        );
        assert!(control.signal_log().is_empty());
    }

    #[test]
    fn shutdown_ladder_backs_off_instead_of_busy_looping() {
        let control = FakeProcesses::session(5, &[7], None);
        run_ladder_blocking(
            vec![request(1, 7, 5)],
            &[(Signal::Hangup, Duration::from_millis(30))],
            &control,
        );
        // 30 ms 窗口按 2 ms 轮询最多 ~16 轮；忙等会是数万次。
        assert!(control.alive_probes.load(Ordering::Relaxed) < 100);
    }

    #[test]
    fn reaper_runs_the_ladder_off_the_caller_thread_and_drain_waits_for_it() {
        let control = FakeProcesses::session(5, &[7], Some(Signal::Terminate));
        let reaper = Reaper::new();
        reaper.worker_started.store(true, Ordering::Release);
        let ladder = test_ladder();
        std::thread::scope(|scope| {
            scope.spawn(|| reaper.run_with(&control, &ladder));
            let submitted = Instant::now();
            reaper.submit(request(1, 7, 5));
            reaper.submit(request(2, 7, 5));
            // 投递不承担阶梯耗时：事件循环立刻回到自己的工作。
            assert!(submitted.elapsed() < Duration::from_millis(10));
            assert!(reaper.drain(Duration::from_secs(5)), "阶梯应在超时前收尾");
            assert!(control.signal_log().contains(&Signal::Terminate));
            reaper.stop();
        });
    }

    #[test]
    fn reaper_reaps_a_batch_of_panes_without_serializing_their_ladders() {
        // 15 个 pane 各自有独立会话，都扛到 SIGTERM。串行跑阶梯是 15×80 ms；并入同一个
        // 轮询循环后总耗时仍是一轮阶梯量级。
        let control = FakeProcesses::new();
        for pane in 1..=15u32 {
            control.insert(
                pane,
                FakeProcess {
                    session: i64::from(pane),
                    instance: 0,
                    state: FakeState::Running,
                    dies_on: Some(Signal::Terminate),
                    zombies_on: None,
                },
            );
        }
        let reaper = Reaper::new();
        reaper.worker_started.store(true, Ordering::Release);
        let ladder = vec![
            (Signal::Hangup, Duration::from_millis(40)),
            (Signal::Terminate, Duration::from_millis(40)),
            (Signal::Kill, Duration::from_millis(40)),
        ];
        std::thread::scope(|scope| {
            scope.spawn(|| reaper.run_with(&control, &ladder));
            let started = Instant::now();
            for pane in 1..=15u32 {
                reaper.submit(request(pane, pane, i64::from(pane)));
            }
            assert!(reaper.drain(Duration::from_secs(10)), "阶梯应在超时前收尾");
            let elapsed = started.elapsed();
            reaper.stop();

            for pane in 1..=15u32 {
                assert_eq!(
                    control.signals_for(pane),
                    vec![Signal::Hangup, Signal::Terminate],
                    "pane {pane} 应各自走完两级"
                );
            }
            // 串行阶梯是 15×80 ms = 1.2 s；这里给一轮阶梯留 7 倍余量仍远低于串行成本。
            assert!(
                elapsed < Duration::from_millis(600),
                "整批耗时 {elapsed:?} 不应随 pane 数线性增长"
            );
        });
    }

    #[test]
    fn reaper_gives_a_pane_submitted_mid_batch_its_own_full_ladder() {
        // 队列放大回归：reaper 正在为一个顽固 pane 跑阶梯时投递第二个 pane，后者必须
        // 立刻并入当前循环并从首级开始，而不是等上一批走完、也不是继承在办的级别。
        let control = FakeProcesses::new();
        control.insert(
            7,
            FakeProcess {
                session: 5,
                instance: 0,
                state: FakeState::Running,
                dies_on: None,
                zombies_on: None,
            },
        );
        control.insert(
            8,
            FakeProcess {
                session: 6,
                instance: 0,
                state: FakeState::Running,
                dies_on: Some(Signal::Hangup),
                zombies_on: None,
            },
        );
        let reaper = Reaper::new();
        reaper.worker_started.store(true, Ordering::Release);
        let ladder = vec![
            (Signal::Hangup, Duration::from_millis(60)),
            (Signal::Terminate, Duration::from_millis(60)),
            (Signal::Kill, Duration::from_millis(60)),
        ];
        std::thread::scope(|scope| {
            scope.spawn(|| reaper.run_with(&control, &ladder));
            reaper.submit(request(1, 7, 5));
            // 等第一级信号落地，确保第二条请求确实落在「批次在办」的窗口里。
            let deadline = Instant::now() + Duration::from_secs(5);
            while control.signals_for(7).is_empty() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
            let submitted = Instant::now();
            reaper.submit(request(2, 8, 6));
            let deadline = Instant::now() + Duration::from_secs(5);
            while control.signals_for(8).is_empty() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
            let first_signal_latency = submitted.elapsed();
            assert!(reaper.drain(Duration::from_secs(10)));
            reaper.stop();

            assert_eq!(
                control.signals_for(8),
                vec![Signal::Hangup],
                "后投递的 pane 必须从首级开始，且首级命中即摘除"
            );
            assert!(
                first_signal_latency < Duration::from_millis(60),
                "后投递的 pane 等了 {first_signal_latency:?} 才拿到首级信号，说明被排到了下一轮"
            );
        });
    }

    #[test]
    fn reaper_keeps_serving_after_a_batch_panics() {
        // 单批次 panic 不得让终止机制静默失效：`in_flight` 必须复位、后续请求仍被消费。
        // 这里故意制造一次 panic，测试输出里的 backtrace 是预期的。
        let control = FakeProcesses::session(5, &[7], Some(Signal::Hangup));
        control.panic_on_next_signal.store(true, Ordering::SeqCst);
        let reaper = Reaper::new();
        reaper.worker_started.store(true, Ordering::Release);
        let ladder = test_ladder();
        std::thread::scope(|scope| {
            scope.spawn(|| reaper.run_with(&control, &ladder));
            reaper.submit(request(1, 7, 5));
            assert!(
                reaper.drain(Duration::from_secs(5)),
                "panic 后 in_flight 必须复位，drain 不能空等"
            );
            reaper.submit(request(2, 7, 5));
            assert!(reaper.drain(Duration::from_secs(5)), "reaper 应继续服务");
            assert_eq!(control.signal_log(), vec![Signal::Hangup]);
            reaper.stop();
        });
    }

    #[test]
    fn reaper_drain_returns_immediately_when_nothing_was_submitted() {
        // 实例级断言，不依赖进程级单例的状态（因此也不依赖「每个测试一个进程」的运行器）。
        let reaper = Reaper::new();
        assert!(reaper.drain(Duration::ZERO));
    }

    #[test]
    fn reaper_drain_reports_timeout_while_a_batch_is_still_running() {
        let control = FakeProcesses::session(5, &[7], None);
        let reaper = Reaper::new();
        reaper.worker_started.store(true, Ordering::Release);
        let ladder = vec![(Signal::Hangup, Duration::from_millis(200))];
        std::thread::scope(|scope| {
            scope.spawn(|| reaper.run_with(&control, &ladder));
            reaper.submit(request(1, 7, 5));
            assert!(!reaper.drain(Duration::from_millis(20)));
            assert!(reaper.drain(Duration::from_secs(5)));
            reaper.stop();
        });
    }

    #[cfg(windows)]
    mod pty_close_tests {
        use super::*;
        use crate::pane::{PaneRuntime, PaneRuntimeIo};
        use crate::pty::actor::shutdown_test_support::ShutdownActor;
        use bytes::Bytes;

        struct SubmitGuard;

        impl SubmitGuard {
            fn install(reaper: Arc<Reaper>) -> Self {
                TEST_SUBMIT.with(|hook| {
                    *hook.borrow_mut() = Some(Arc::new(move |mut request| {
                        assert!(
                            request.session.is_some(),
                            "runtime captured the process anchor before closing I/O"
                        );
                        request.child_pid = request.pane_id.raw();
                        request.session = Some(anchor(i64::from(request.child_pid)));
                        reaper.submit(request);
                    }));
                });
                Self
            }
        }

        impl Drop for SubmitGuard {
            fn drop(&mut self) {
                TEST_SUBMIT.with(|hook| hook.borrow_mut().take());
            }
        }

        struct SavingProcesses {
            closed: Vec<Arc<Mutex<Option<Instant>>>>,
            now: Mutex<Option<Instant>>,
            killed: Mutex<Vec<u32>>,
            scanned: AtomicUsize,
        }

        impl ProcessControl for SavingProcesses {
            fn session_id(&self, pid: u32) -> Option<ProcessSessionId> {
                Some(anchor(i64::from(pid)))
            }

            fn session_processes(
                &self,
                sessions: &[ProcessSessionId],
            ) -> Vec<Vec<ProcessSessionMember>> {
                self.scanned.fetch_add(sessions.len(), Ordering::Relaxed);
                sessions
                    .iter()
                    .map(|session| {
                        vec![ProcessSessionMember {
                            pid: session.id as u32,
                            instance: 0,
                        }]
                    })
                    .collect()
            }

            fn signal_processes(&self, members: &[ProcessSessionMember], signal: Signal) {
                if signal == Signal::Kill {
                    self.killed
                        .lock()
                        .unwrap()
                        .extend(members.iter().map(|member| member.pid));
                }
            }

            fn process_alive(&self, member: ProcessSessionMember) -> bool {
                if self.killed.lock().unwrap().contains(&member.pid) {
                    return false;
                }
                let now = self.now.lock().unwrap().unwrap_or_else(Instant::now);
                self.closed[member.pid as usize - 1]
                    .lock()
                    .unwrap()
                    .is_none_or(|closed| now < closed + Duration::from_millis(350))
            }
        }

        fn take_targets(reaper: &Reaper, control: &impl ProcessControl) -> Vec<ReapTarget> {
            let pending = {
                let mut state = lock_state(reaper);
                let pending: Vec<_> = state.queue.drain(..).collect();
                state.in_flight = pending.len();
                pending
            };
            prepare_targets(pending, control)
        }

        fn advance(
            reaper: &Reaper,
            targets: &mut Vec<ReapTarget>,
            control: &SavingProcesses,
            now: Instant,
        ) {
            *control.now.lock().unwrap() = Some(now);
            advance_targets(targets, &SIGNAL_LADDER, control, now);
            lock_state(reaper).in_flight = targets.len();
        }

        fn test_runtime() -> tokio::runtime::Runtime {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .unwrap()
        }

        fn runtime_with_actor(
            runtime: &tokio::runtime::Runtime,
            actor: &ShutdownActor,
            id: u32,
        ) -> PaneRuntime {
            let _entered = runtime.enter();
            let (mut pane, _) = PaneRuntime::test_with_channel(80, 24);
            pane.pane_id = PaneId::from_raw(id);
            pane.io = PaneRuntimeIo::Actor(actor.handle.as_ref().unwrap().clone());
            pane.child_pid.store(std::process::id(), Ordering::Release);
            pane.preserve_processes_on_drop = false;
            pane
        }

        #[test]
        fn pty_close_idle_and_cancelled_input_release_without_fixed_grace() {
            for mode in ["idle", "queued", "cancelled"] {
                let runtime = test_runtime();
                let reaper = Arc::new(Reaper::new());
                reaper.worker_started.store(true, Ordering::Release);
                let _hook = SubmitGuard::install(Arc::clone(&reaper));
                let mut actor = ShutdownActor::new_paused(runtime.handle());
                let pane = runtime_with_actor(&runtime, &actor, 1);
                let result = (mode != "idle").then(|| {
                    pane.queue_user_input_submission(
                        Bytes::from_static(b"save"),
                        Bytes::from_static(b"\r"),
                        Duration::ZERO,
                        None,
                    )
                    .unwrap()
                });
                if mode == "cancelled" {
                    actor.cancel_input();
                }
                let control = SavingProcesses {
                    closed: vec![Arc::clone(&actor.closed_at)],
                    now: Mutex::new(Some(Instant::now())),
                    killed: Mutex::new(Vec::new()),
                    scanned: AtomicUsize::new(0),
                };
                let started = Instant::now();
                pane.shutdown();
                assert!(!reaper.drain(Duration::ZERO));
                let mut targets = take_targets(&reaper, &control);
                let deadline = targets[0].pty_close.as_ref().unwrap().deadline();
                let closed = actor.wait_for_close();
                assert!(
                    closed.duration_since(started) < Duration::from_millis(250),
                    "idle close must not spend the bounded Enter grace"
                );
                assert_eq!(
                    actor.handle.as_ref().unwrap().shutdown().deadline(),
                    deadline
                );
                actor.start_input();
                if let Some(result) = result {
                    let result = result.recv_timeout(Duration::from_secs(2));
                    if mode == "cancelled" {
                        assert!(matches!(
                            result,
                            Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
                        ));
                    } else {
                        assert_eq!(
                            result.unwrap().unwrap_err().kind(),
                            std::io::ErrorKind::BrokenPipe
                        );
                    }
                }
                advance(
                    &reaper,
                    &mut targets,
                    &control,
                    closed + Duration::from_millis(1),
                );
                assert_eq!(targets[0].stage, 1);
                advance(
                    &reaper,
                    &mut targets,
                    &control,
                    closed + Duration::from_millis(350),
                );
                assert!(reaper.drain(Duration::ZERO));
                assert!(control.killed.lock().unwrap().is_empty());
                actor.finish();
            }
        }

        #[test]
        fn pty_close_stalled_control_has_an_absolute_bounded_reaper_deadline() {
            let runtime = test_runtime();
            let reaper = Arc::new(Reaper::new());
            reaper.worker_started.store(true, Ordering::Release);
            let _hook = SubmitGuard::install(Arc::clone(&reaper));
            let actor = ShutdownActor::new(runtime.handle());
            actor.block_control();
            let pane = runtime_with_actor(&runtime, &actor, 1);
            let started = Instant::now();
            pane.shutdown();
            let control = FakeProcesses::session(1, &[1], None);
            assert!(!reaper.drain(Duration::ZERO));
            let mut targets = take_targets(&reaper, &control);
            let deadline = targets[0].pty_close.as_ref().unwrap().deadline();
            assert_eq!(
                actor.handle.as_ref().unwrap().shutdown().deadline(),
                deadline
            );
            assert!(sleep_budget(&targets, Instant::now()) >= FAST_POLL);
            advance_targets(
                &mut targets,
                &SIGNAL_LADDER,
                &control,
                deadline - Duration::from_millis(1),
            );
            assert_eq!(targets[0].stage, 0);
            assert!(!reaper.drain(Duration::ZERO));
            for offset in [0, 250, 500] {
                advance_targets(
                    &mut targets,
                    &SIGNAL_LADDER,
                    &control,
                    deadline + Duration::from_millis(offset),
                );
            }
            assert_eq!(
                control.signal_log(),
                vec![Signal::Hangup, Signal::Terminate, Signal::Kill]
            );
            advance_targets(
                &mut targets,
                &SIGNAL_LADDER,
                &control,
                deadline + Duration::from_millis(500 + KILL_GRACE_MS),
            );
            assert!(targets.is_empty());
            lock_state(&reaper).in_flight = 0;
            assert!(reaper.drain(Duration::ZERO));
            assert!(
                deadline + Duration::from_millis(500 + KILL_GRACE_MS)
                    <= started + LADDER_WORST_CASE + Duration::from_millis(50)
            );
            actor.finish();
        }

        #[test]
        fn pty_close_without_a_process_anchor_still_stays_registered() {
            let runtime = test_runtime();
            let reaper = Arc::new(Reaper::new());
            reaper.worker_started.store(true, Ordering::Release);
            let _hook = SubmitGuard::install(Arc::clone(&reaper));
            let actor = ShutdownActor::new(runtime.handle());
            actor.block_control();
            runtime_with_actor(&runtime, &actor, 1).shutdown();
            lock_state(&reaper).queue.front_mut().unwrap().session = None;
            let control = FakeProcesses::new();
            let mut targets = take_targets(&reaper, &control);
            assert_eq!(targets.len(), 1);
            let deadline = targets[0].pty_close.as_ref().unwrap().deadline();
            advance_targets(
                &mut targets,
                &SIGNAL_LADDER,
                &control,
                deadline - Duration::from_millis(1),
            );
            assert!(!reaper.drain(Duration::ZERO));
            advance_targets(&mut targets, &SIGNAL_LADDER, &control, deadline);
            assert!(targets.is_empty());
            assert!(control.signal_log().is_empty());
            lock_state(&reaper).in_flight = 0;
            assert!(reaper.drain(Duration::ZERO));
            actor.finish();
        }

        #[test]
        fn pty_close_reaper_drains_fifteen_saving_panes_without_early_kill() {
            let runtime = test_runtime();
            let reaper = Arc::new(Reaper::new());
            reaper.worker_started.store(true, Ordering::Release);
            let _hook = SubmitGuard::install(Arc::clone(&reaper));
            let mut actors = Vec::new();
            let mut panes = Vec::new();
            for id in 1..=15 {
                let mut actor = ShutdownActor::new(runtime.handle());
                let pane = runtime_with_actor(&runtime, &actor, id);
                let result = pane
                    .queue_user_input_submission(
                        Bytes::from_static(b"save"),
                        Bytes::from_static(b"\r"),
                        Duration::ZERO,
                        None,
                    )
                    .unwrap();
                actor.wait_for_enter();
                if id % 2 == 0 {
                    actor.cancel_input();
                }
                drop(result);
                actors.push(actor);
                panes.push(pane);
            }
            let control = SavingProcesses {
                closed: actors
                    .iter()
                    .map(|actor| Arc::clone(&actor.closed_at))
                    .collect(),
                now: Mutex::new(None),
                killed: Mutex::new(Vec::new()),
                scanned: AtomicUsize::new(0),
            };
            let started = Instant::now();
            let drained = std::thread::scope(|scope| {
                scope.spawn(|| reaper.run_with(&control, &SIGNAL_LADDER));
                for pane in panes {
                    pane.shutdown();
                }
                let already_drained = reaper.drain(Duration::ZERO);
                let drained = reaper.drain(LADDER_WORST_CASE + Duration::from_secs(1));
                reaper.stop();
                assert!(
                    !already_drained,
                    "pending close gates must be visible to drain"
                );
                drained
            });
            assert!(drained);
            assert_eq!(control.scanned.load(Ordering::Relaxed), 15);
            assert!(control.killed.lock().unwrap().is_empty());
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "15 pane shutdowns must advance independently"
            );
            for actor in actors {
                actor.finish();
            }
        }

        #[test]
        fn pty_close_preserves_save_window_for_runtime_shutdown_and_drop() {
            for panes in [1, 15] {
                for implicit_drop in [false, true] {
                    let runtime = tokio::runtime::Builder::new_multi_thread()
                        .worker_threads(1)
                        .enable_all()
                        .build()
                        .unwrap();
                    let reaper = Arc::new(Reaper::new());
                    reaper.worker_started.store(true, Ordering::Release);
                    let _hook = SubmitGuard::install(Arc::clone(&reaper));
                    let mut actors = Vec::new();
                    let mut runtimes = Vec::new();
                    let mut results = Vec::new();
                    for pane_id in 1..=panes {
                        let actor = ShutdownActor::new(runtime.handle());
                        let (mut pane, _) = {
                            let _entered = runtime.enter();
                            PaneRuntime::test_with_channel(80, 24)
                        };
                        pane.pane_id = PaneId::from_raw(pane_id);
                        pane.io = PaneRuntimeIo::Actor(actor.handle.as_ref().unwrap().clone());
                        pane.child_pid.store(std::process::id(), Ordering::Release);
                        pane.preserve_processes_on_drop = false;
                        results.push(
                            pane.queue_user_input_submission(
                                Bytes::from_static(b"save"),
                                Bytes::from_static(b"\r"),
                                Duration::ZERO,
                                None,
                            )
                            .unwrap(),
                        );
                        actor.wait_for_enter();
                        actors.push(actor);
                        runtimes.push(pane);
                    }
                    let control = SavingProcesses {
                        closed: actors
                            .iter()
                            .map(|actor| Arc::clone(&actor.closed_at))
                            .collect(),
                        now: Mutex::new(Some(Instant::now())),
                        killed: Mutex::new(Vec::new()),
                        scanned: AtomicUsize::new(0),
                    };
                    let started = Instant::now();
                    for pane in runtimes {
                        if implicit_drop {
                            drop(pane);
                        } else {
                            pane.shutdown();
                        }
                    }
                    assert!(
                        !reaper.drain(Duration::ZERO),
                        "drain must see registered requests before PTY close"
                    );
                    let mut targets = take_targets(&reaper, &control);
                    assert_eq!(control.scanned.load(Ordering::Relaxed), panes as usize);
                    let first = Instant::now();
                    advance(&reaper, &mut targets, &control, first);
                    assert!(targets.iter().all(|target| target.stage == 0), "PTY close must precede the first ladder timer; blocked Enter cannot consume the process save window");
                    assert!(!reaper.drain(Duration::ZERO));
                    let closed = actors
                        .iter()
                        .map(ShutdownActor::wait_for_close)
                        .max()
                        .unwrap();
                    assert!(
                        started.elapsed() < Duration::from_secs(2),
                        "pane close grace must run concurrently"
                    );
                    let after_close = (closed + Duration::from_millis(10))
                        .max(first + Duration::from_millis(250));
                    advance(&reaper, &mut targets, &control, after_close);
                    advance(
                        &reaper,
                        &mut targets,
                        &control,
                        after_close + Duration::from_millis(250),
                    );
                    advance(
                        &reaper,
                        &mut targets,
                        &control,
                        after_close + Duration::from_millis(350),
                    );
                    assert!(
                        control.killed.lock().unwrap().is_empty(),
                        "saving processes must get 350ms after CLOSE without Kill"
                    );
                    assert!(reaper.drain(Duration::ZERO));
                    for (actor, result) in actors.into_iter().zip(results) {
                        assert!(result
                            .recv_timeout(Duration::from_secs(2))
                            .unwrap()
                            .is_err());
                        actor.finish();
                    }
                }
            }
        }
    }

    #[cfg(windows)]
    const LADDER_TREE_ENV: &str = "HERDR_TEST_LADDER_TREE";
    #[cfg(windows)]
    const LADDER_TREE_READY: &str = "herdr-ladder-tree-ready";

    /// 重新拉起本测试二进制，只跑真实进程树的阶梯测试，`mode` 决定它在树里扮演哪一层。
    #[cfg(windows)]
    fn relaunch_ladder_tree_test(mode: &str) -> std::process::Command {
        let mut command =
            std::process::Command::new(std::env::current_exe().expect("test executable"));
        command
            .args([
                "--exact",
                "pane::shutdown::tests::platform_ladder_terminates_a_real_windows_process_tree",
                "--nocapture",
            ])
            .env(LADDER_TREE_ENV, mode)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        command
    }

    /// 真实的 Windows 进程树（测试二进制 → 测试二进制 → ping）走一遍平台阶梯：前两级在 Windows
    /// 上什么都不发，末级 `TerminateProcess` 把三层都终止干净、不留孤儿。中间一层拉起 ping
    /// 之后才报就绪，阶梯枚举成员时整棵树都在。
    #[cfg(windows)]
    #[test]
    fn platform_ladder_terminates_a_real_windows_process_tree() {
        use std::process::Stdio;

        match std::env::var(LADDER_TREE_ENV).as_deref() {
            Ok("root") => {
                // 中间一层继承 stdout，就绪行直接交给测试。
                let middle = relaunch_ladder_tree_test("middle")
                    .stdout(Stdio::inherit())
                    .spawn();
                if let Ok(mut middle) = middle {
                    let _ = middle.wait();
                }
                return;
            }
            Ok("middle") => {
                let mut leaf = std::process::Command::new("ping")
                    .args(["-n", "60", "127.0.0.1"])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .expect("spawn ping");
                println!("{LADDER_TREE_READY}");
                std::thread::sleep(Duration::from_secs(60));
                // 正常情况下阶梯早已终止这一层；走到这里说明测试没有收尾，自己收拾叶子。
                let _ = leaf.kill();
                let _ = leaf.wait();
                return;
            }
            _ => {}
        }

        let mut root = relaunch_ladder_tree_test("root")
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn root");
        let stdout = root.stdout.take().expect("root stdout");
        let ready = std::io::BufRead::lines(std::io::BufReader::new(stdout))
            .map_while(Result::ok)
            .any(|line| line.contains(LADDER_TREE_READY));
        let request = PaneShutdownRequest::new(
            PaneId::from_raw(1),
            root.id(),
            Some(Arc::new(AtomicBool::new(false))),
        );
        let members = request
            .session
            .and_then(|session| crate::platform::session_members_batch(&[session]).pop())
            .unwrap_or_default();
        if !ready || members.len() < 3 {
            crate::platform::signal_session_members(&members, Signal::Kill);
            let _ = root.kill();
            let _ = root.wait();
            panic!("process tree did not come up: ready={ready} members={members:?}");
        }

        // 只走挂断与终止两级时整棵树都还在：Windows 的硬杀只落在末级，与 Unix 的 SIGKILL
        // 同一级。
        let soft_ladder = [
            (Signal::Hangup, Duration::from_millis(20)),
            (Signal::Terminate, Duration::from_millis(20)),
        ];
        let soft_request = PaneShutdownRequest::new(
            PaneId::from_raw(1),
            root.id(),
            Some(Arc::new(AtomicBool::new(false))),
        );
        run_ladder_blocking(vec![soft_request], &soft_ladder, &PlatformProcessControl);
        let ended_early: Vec<ProcessSessionMember> = members
            .iter()
            .copied()
            .filter(|member| !crate::platform::session_member_alive(*member))
            .collect();

        let ladder_started = Instant::now();
        run_ladder_blocking(vec![request], &SIGNAL_LADDER, &PlatformProcessControl);
        // 阶梯在末级之后只再等一个宽限窗口；负载重时被终止的进程可能还没拆完（进程对象尚未
        // signaled）。用宽上限等它们退出：没收到信号的成员要活满 60 s，照样等不到而失败。
        let deadline = Instant::now() + Duration::from_secs(30);
        let survivors = loop {
            let survivors: Vec<ProcessSessionMember> = members
                .iter()
                .copied()
                .filter(|member| crate::platform::session_member_alive(*member))
                .collect();
            if survivors.is_empty() || Instant::now() >= deadline {
                break survivors;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let elapsed = ladder_started.elapsed();
        crate::platform::signal_session_members(&survivors, Signal::Kill);
        let _ = root.wait();
        assert!(
            ended_early.is_empty(),
            "Hangup and Terminate must not end Windows pane processes: {ended_early:?}"
        );
        assert!(
            survivors.is_empty(),
            "pane process tree must not outlive the ladder ({elapsed:?} after it started): {survivors:?}"
        );
    }
}
