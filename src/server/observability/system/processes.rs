//! 进程采样和实例句柄授权独立于 CPU、磁盘和显卡查询。
use super::super::Reply;
use super::{percent, process_page, protected_process};
use crate::api::schema::*;
use std::collections::HashMap;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};
use sysinfo::{CpuRefreshKind, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind, Users};

/// 进程请求的错误说明按 server 的界面语言给出（文档终审 D7）；错误码不变。
fn texts() -> &'static crate::i18n::RuntimeMessageTexts {
    &crate::i18n::texts().runtime
}

const PROCESS_ACTION_TTL: Duration = Duration::from_secs(60);
const PROCESS_ACTION_LIMIT: usize = 64;
const PROCESS_USER_REFRESH_TTL: Duration = Duration::from_secs(30);
type ProcessActions<P> = HashMap<String, (ProcessIdentity, P, Instant)>;

fn sample_if_due(
    last: &mut Option<Instant>,
    now: Instant,
    ttl: Duration,
    sample: impl FnOnce(),
) -> bool {
    if last.is_none_or(|at| now.saturating_duration_since(at) >= ttl) {
        sample();
        *last = Some(now);
        true
    } else {
        false
    }
}

fn prune_process_actions<P>(actions: &mut ProcessActions<P>, now: Instant) -> Option<Duration> {
    let mut next = None;
    actions.retain(|_, (_, _, created)| {
        let age = now.saturating_duration_since(*created);
        if age >= PROCESS_ACTION_TTL {
            return false;
        }
        let remaining = PROCESS_ACTION_TTL - age;
        next = Some(next.map_or(remaining, |at: Duration| at.min(remaining)));
        true
    });
    next
}

fn receive_process_job<P, J>(
    actions: &mut ProcessActions<P>,
    mut now: impl FnMut() -> Instant,
    mut receive: impl FnMut(Option<Duration>) -> Result<J, mpsc::RecvTimeoutError>,
) -> Option<J> {
    loop {
        let timeout = prune_process_actions(actions, now());
        match receive(timeout) {
            Ok(job) => return Some(job),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return None,
        }
    }
}

fn take_process_action<P>(
    actions: &mut ProcessActions<P>,
    params: &ProcessTerminateParams,
    now: Instant,
) -> Result<(ProcessIdentity, P), (&'static str, String)> {
    let ticket = params
        .action_token
        .as_deref()
        .ok_or_else(|| ("identity_required", texts().process_confirm_first.into()))?;
    let Some((identity, process, created)) = actions.remove(ticket) else {
        return Err(("stale_process", texts().process_confirm_expired.into()));
    };
    if identity != params.identity || now.saturating_duration_since(created) >= PROCESS_ACTION_TTL {
        return Err(("stale_process", texts().process_confirm_mismatch.into()));
    }
    Ok((identity, process))
}

enum Job {
    Sample,
    Request(Box<Request>, Reply),
}

#[derive(Clone)]
pub(in crate::server::observability) struct ProcessWorker {
    requests: mpsc::SyncSender<Job>,
    cache: Arc<Mutex<SystemMetricsSnapshot>>,
}

impl ProcessWorker {
    pub fn start(boot_id: String) -> std::io::Result<Self> {
        let (requests, input) = mpsc::sync_channel(16);
        let cache = Arc::new(Mutex::new(SystemMetricsSnapshot::default()));
        let output = cache.clone();
        std::thread::Builder::new()
            .name("herdr-process-metrics".into())
            .spawn(move || {
                let mut sampler = ProcessSampler {
                    system: System::new(),
                    users: Users::new(),
                    last_user_refresh: None,
                    boot_id,
                    snapshot: SystemMetricsSnapshot::default(),
                    process_actions: HashMap::new(),
                    next_action: 1,
                };
                let mut last_sample: Option<Instant> = None;
                while let Some(job) =
                    receive_process_job(&mut sampler.process_actions, Instant::now, |timeout| {
                        match timeout {
                            Some(timeout) => input.recv_timeout(timeout),
                            None => input
                                .recv()
                                .map_err(|_| mpsc::RecvTimeoutError::Disconnected),
                        }
                    })
                {
                    match job {
                        Job::Sample => {
                            let now = Instant::now();
                            if last_sample.is_none_or(|at| {
                                now.saturating_duration_since(at) >= Duration::from_secs(2)
                            }) {
                                sampler.refresh_processes(now);
                                last_sample = Some(now);
                                if let Ok(mut cache) = output.lock() {
                                    *cache = sampler.snapshot.clone();
                                }
                            }
                        }
                        Job::Request(request, reply) => {
                            let id = request.id;
                            let result = match request.method {
                                Method::SystemProcessList(params) => {
                                    let now = Instant::now();
                                    if last_sample.is_none_or(|at| {
                                        now.saturating_duration_since(at) >= Duration::from_secs(2)
                                    }) {
                                        sampler.refresh_processes(now);
                                        last_sample = Some(now);
                                        if let Ok(mut cache) = output.lock() {
                                            *cache = sampler.snapshot.clone();
                                        }
                                    }
                                    let (total, processes) =
                                        process_page(&sampler.snapshot, &params);
                                    Ok(ResponseResult::SystemProcesses {
                                        sampled_at_ms: sampler.snapshot.sampled_at_ms,
                                        total,
                                        processes,
                                    })
                                }
                                Method::SystemProcessGet(params) => sampler
                                    .process(&params.identity)
                                    .map(|process| ResponseResult::SystemProcess { process }),
                                Method::SystemProcessTerminate(params) => sampler
                                    .terminate(&params)
                                    .map(|()| ResponseResult::SystemProcessTerminated {
                                        identity: params.identity,
                                        force: params.force,
                                    }),
                                _ => Err((
                                    "unsupported_method",
                                    texts().process_request_unsupported.into(),
                                )),
                            };
                            reply.response(&id, result);
                        }
                    }
                }
            })?;
        Ok(Self { requests, cache })
    }

    pub fn request_and_read(&self) -> (u64, Vec<ProcessMetric>) {
        let _ = self.requests.try_send(Job::Sample);
        self.cache
            .lock()
            .map(|value| (value.sampled_at_ms, value.processes.clone()))
            .unwrap_or_default()
    }

    pub fn submit(&self, request: Request, reply: Reply) {
        let id = request.id.clone();
        if self
            .requests
            .try_send(Job::Request(Box::new(request), reply.clone()))
            .is_err()
        {
            reply.response(&id, Err(("server_busy", texts().process_busy.into())));
        }
    }
}

struct ProcessSampler {
    system: System,
    users: Users,
    last_user_refresh: Option<Instant>,
    boot_id: String,
    snapshot: SystemMetricsSnapshot,
    process_actions: ProcessActions<crate::platform::MonitoredProcess>,
    next_action: u64,
}

impl ProcessSampler {
    fn refresh_processes(&mut self, now: Instant) {
        let first = self.snapshot.sampled_at_ms == 0;
        if self.system.cpus().is_empty() {
            self.system.refresh_cpu_list(CpuRefreshKind::everything());
        } else {
            self.system.refresh_cpu_usage();
        }
        sample_if_due(
            &mut self.last_user_refresh,
            now,
            PROCESS_USER_REFRESH_TTL,
            || self.users.refresh(),
        );
        self.snapshot.sampled_at_ms = super::super::now_ms();
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing()
                .with_cpu()
                .with_memory()
                .with_user(UpdateKind::OnlyIfNotSet),
        );
        let cpu_count = self.system.cpus().len().max(1) as f32;
        self.snapshot.processes = self
            .system
            .processes()
            .iter()
            .map(|(pid, process)| {
                let name = process.name().to_string_lossy().into_owned();
                ProcessMetric {
                    identity: ProcessIdentity {
                        pid: pid.as_u32(),
                        started_at: process.start_time(),
                        boot_id: self.boot_id.clone(),
                        instance_token: crate::platform::process_instance_token(pid.as_u32()).ok(),
                    },
                    parent_pid: process.parent().map(|id| id.as_u32()),
                    protected: protected_process(pid.as_u32(), &name),
                    action_token: None,
                    name,
                    cpu_percent: (!first)
                        .then(|| percent(process.cpu_usage() / cpu_count))
                        .flatten(),
                    memory_bytes: process.memory(),
                    status: process.status().to_string(),
                    user: process
                        .user_id()
                        .and_then(|id| self.users.get_user_by_id(id))
                        .map(|user| user.name().to_owned()),
                    executable: None,
                }
            })
            .collect();
        self.snapshot.processes.sort_by(|a, b| {
            b.cpu_percent
                .unwrap_or(-1.0)
                .total_cmp(&a.cpu_percent.unwrap_or(-1.0))
                .then(a.identity.pid.cmp(&b.identity.pid))
        });
    }

    pub fn process(
        &mut self,
        identity: &ProcessIdentity,
    ) -> Result<ProcessMetric, (&'static str, String)> {
        if identity.boot_id != self.boot_id {
            return Err(("stale_boot", texts().process_host_changed.into()));
        }
        let before = crate::platform::process_instance_token(identity.pid)
            .map_err(|error| ("process_identity_unavailable", error.to_string()))?;
        if identity.instance_token.as_deref() != Some(before.as_str()) {
            return Err(("stale_process", texts().process_changed_refresh.into()));
        }
        let native = crate::platform::MonitoredProcess::open(identity.pid).ok();
        if native
            .as_ref()
            .is_some_and(|process| process.instance_token != before)
        {
            return Err(("stale_process", texts().process_changed.into()));
        }
        let pid = sysinfo::Pid::from_u32(identity.pid);
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[pid]),
            true,
            ProcessRefreshKind::nothing()
                .with_memory()
                .with_exe(UpdateKind::Always),
        );
        let process = self
            .system
            .process(pid)
            .ok_or_else(|| ("not_found", texts().process_gone.into()))?;
        if process.start_time() != identity.started_at {
            return Err(("stale_process", texts().process_pid_reused.into()));
        }
        let token = crate::platform::process_instance_token(identity.pid)
            .map_err(|error| ("process_identity_unavailable", error.to_string()))?;
        if token != before {
            return Err(("stale_process", texts().process_identity_changed.into()));
        }
        let name = native
            .as_ref()
            .map(|process| process.name.clone())
            .unwrap_or_else(|| process.name().to_string_lossy().into_owned());
        let _ = prune_process_actions(&mut self.process_actions, Instant::now());
        let protected = protected_process(identity.pid, &name);
        let action_token = if !protected && self.process_actions.len() < PROCESS_ACTION_LIMIT {
            native.map(|native| {
                let ticket = format!("{}-{}", self.boot_id, self.next_action);
                self.next_action = self.next_action.saturating_add(1);
                self.process_actions
                    .insert(ticket.clone(), (identity.clone(), native, Instant::now()));
                ticket
            })
        } else {
            None
        };
        Ok(ProcessMetric {
            identity: ProcessIdentity {
                instance_token: Some(token),
                ..identity.clone()
            },
            parent_pid: process.parent().map(|id| id.as_u32()),
            protected,
            action_token,
            name,
            cpu_percent: self
                .snapshot
                .processes
                .iter()
                .find(|entry| entry.identity == *identity)
                .and_then(|entry| entry.cpu_percent),
            memory_bytes: process.memory(),
            status: process.status().to_string(),
            user: process
                .user_id()
                .and_then(|id| self.users.get_user_by_id(id))
                .map(|user| user.name().to_owned()),
            executable: process
                .exe()
                .map(|path| path.to_string_lossy().into_owned()),
        })
    }

    pub fn terminate(
        &mut self,
        params: &ProcessTerminateParams,
    ) -> Result<(), (&'static str, String)> {
        let (identity, process) =
            take_process_action(&mut self.process_actions, params, Instant::now())?;
        if protected_process(identity.pid, &process.name) {
            return Err(("protected_process", texts().process_protected.into()));
        }
        process
            .terminate(params.force)
            .map_err(|error| ("terminate_failed", error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    use super::*;
    use crate::i18n::{has_cjk, lang_guard, Lang};

    struct DropProbe(Arc<AtomicUsize>);

    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn process_action_idle_wait_releases_all_expired_leases_without_requests() {
        let issued = Instant::now();
        let clock = std::cell::Cell::new(issued + PROCESS_ACTION_TTL - Duration::from_secs(1));
        let drops = Arc::new(AtomicUsize::new(0));
        let mut actions = ProcessActions::new();
        for index in 0..PROCESS_ACTION_LIMIT {
            let created = issued
                + Duration::from_secs(if index < PROCESS_ACTION_LIMIT / 2 {
                    0
                } else {
                    5
                });
            actions.insert(
                index.to_string(),
                (
                    ProcessIdentity::default(),
                    DropProbe(Arc::clone(&drops)),
                    created,
                ),
            );
        }
        let mut waits = Vec::new();
        let result: Option<()> = receive_process_job(
            &mut actions,
            || clock.get(),
            |timeout| {
                waits.push(timeout);
                match waits.len() {
                    1 => {
                        assert_eq!(timeout, Some(Duration::from_secs(1)));
                        assert_eq!(drops.load(Ordering::SeqCst), 0);
                        clock.set(issued + PROCESS_ACTION_TTL);
                        Err(mpsc::RecvTimeoutError::Timeout)
                    }
                    2 => {
                        assert_eq!(timeout, Some(Duration::from_secs(5)));
                        assert_eq!(drops.load(Ordering::SeqCst), PROCESS_ACTION_LIMIT / 2);
                        clock.set(issued + PROCESS_ACTION_TTL + Duration::from_secs(5));
                        Err(mpsc::RecvTimeoutError::Timeout)
                    }
                    3 => {
                        assert_eq!(timeout, None, "no leases means a blocking receive");
                        assert_eq!(drops.load(Ordering::SeqCst), PROCESS_ACTION_LIMIT);
                        Err(mpsc::RecvTimeoutError::Disconnected)
                    }
                    _ => panic!("idle cleanup must stop when the receiver disconnects"),
                }
            },
        );
        assert_eq!(result, None);
        assert_eq!(waits.len(), 3);
        assert!(actions.is_empty());
        drop(actions);
        assert_eq!(drops.load(Ordering::SeqCst), PROCESS_ACTION_LIMIT);
    }

    #[test]
    fn process_action_expiry_and_identity_mismatch_drop_without_authorizing() {
        let issued = Instant::now();
        let deadline = issued + PROCESS_ACTION_TTL;
        let identity = ProcessIdentity {
            pid: 42,
            started_at: 100,
            boot_id: "boot-1".into(),
            instance_token: Some("instance-1".into()),
        };
        for (now, requested_identity, accepted) in [
            (deadline - Duration::from_nanos(1), identity.clone(), true),
            (deadline, identity.clone(), false),
            (deadline + Duration::from_nanos(1), identity.clone(), false),
            (
                issued,
                ProcessIdentity {
                    pid: 43,
                    ..identity.clone()
                },
                false,
            ),
            (
                issued,
                ProcessIdentity {
                    started_at: 101,
                    ..identity.clone()
                },
                false,
            ),
            (
                issued,
                ProcessIdentity {
                    boot_id: "boot-2".into(),
                    ..identity.clone()
                },
                false,
            ),
            (
                issued,
                ProcessIdentity {
                    instance_token: None,
                    ..identity.clone()
                },
                false,
            ),
            (
                issued,
                ProcessIdentity {
                    instance_token: Some("instance-2".into()),
                    ..identity.clone()
                },
                false,
            ),
        ] {
            let drops = Arc::new(AtomicUsize::new(0));
            let mut actions = ProcessActions::from([(
                "ticket".into(),
                (identity.clone(), DropProbe(Arc::clone(&drops)), issued),
            )]);
            let params = ProcessTerminateParams {
                identity: requested_identity,
                action_token: Some("ticket".into()),
                force: true,
            };
            let result = take_process_action(&mut actions, &params, now);
            assert!(actions.is_empty());
            if accepted {
                let (confirmed, lease) = result.expect("matching unexpired ticket");
                assert_eq!(confirmed, identity);
                assert_eq!(drops.load(Ordering::SeqCst), 0);
                drop(lease);
            } else {
                assert_eq!(result.err().map(|(code, _)| code), Some("stale_process"));
            }
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            assert_eq!(
                take_process_action(&mut actions, &params, now)
                    .err()
                    .map(|(code, _)| code),
                Some("stale_process"),
                "a ticket is consumed even when confirmation is rejected"
            );
            assert_eq!(drops.load(Ordering::SeqCst), 1);
        }
    }

    #[test]
    fn process_user_directory_refresh_uses_ttl_and_controlled_clock() {
        let start = Instant::now();
        let mut last = None;
        let mut calls = 0;
        assert!(sample_if_due(
            &mut last,
            start,
            PROCESS_USER_REFRESH_TTL,
            || calls += 1
        ));
        assert!(!sample_if_due(
            &mut last,
            start + PROCESS_USER_REFRESH_TTL - Duration::from_nanos(1),
            PROCESS_USER_REFRESH_TTL,
            || calls += 1
        ));
        assert!(sample_if_due(
            &mut last,
            start + PROCESS_USER_REFRESH_TTL,
            PROCESS_USER_REFRESH_TTL,
            || calls += 1
        ));
        assert_eq!(calls, 2);
    }

    fn sampler() -> ProcessSampler {
        ProcessSampler {
            system: System::new(),
            users: Users::new(),
            last_user_refresh: None,
            boot_id: "boot-1".into(),
            snapshot: SystemMetricsSnapshot::default(),
            process_actions: HashMap::new(),
            next_action: 0,
        }
    }

    /// 文档终审 D7：进程详情与结束进程的错误说明按界面语言给出——英文界面不含 CJK，
    /// 中文界面是中文；错误码不随语言变化。
    #[test]
    fn process_errors_follow_the_interface_language() {
        let mut sampler = sampler();
        let stale_boot = ProcessIdentity {
            boot_id: "other-boot".into(),
            ..Default::default()
        };
        // 本进程还活着，但请求里没有实例标识：服务端按「实例已变化」拒绝（平台拿不到标识
        // 时按「标识不可用」拒绝），都不会去碰真实进程。
        let unconfirmed_instance = ProcessIdentity {
            pid: std::process::id(),
            boot_id: "boot-1".into(),
            ..Default::default()
        };
        let unconfirmed = ProcessTerminateParams {
            identity: ProcessIdentity::default(),
            action_token: None,
            force: false,
        };
        let expired = ProcessTerminateParams {
            action_token: Some("gone".into()),
            ..unconfirmed.clone()
        };
        for (lang, chinese) in [(Lang::En, false), (Lang::ZhCn, true)] {
            let _guard = lang_guard(lang);
            let cases = [
                (sampler.process(&stale_boot).err(), Some("stale_boot")),
                (sampler.process(&unconfirmed_instance).err(), None),
                (
                    sampler.terminate(&unconfirmed).err(),
                    Some("identity_required"),
                ),
                (sampler.terminate(&expired).err(), Some("stale_process")),
            ];
            for (error, expected_code) in cases {
                let (code, message) = error.expect("这些请求都应被拒绝");
                if let Some(expected) = expected_code {
                    assert_eq!(code, expected);
                }
                assert_eq!(has_cjk(&message), chinese, "{lang:?} {code}: {message}");
            }
        }
    }
}
