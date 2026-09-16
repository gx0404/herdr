//! 进程采样和实例句柄授权独立于 CPU、磁盘和显卡查询。
use super::super::Reply;
use super::{percent, process_page, protected_process};
use crate::api::schema::*;
use std::collections::HashMap;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};
use sysinfo::{CpuRefreshKind, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind, Users};

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
                    boot_id,
                    snapshot: SystemMetricsSnapshot::default(),
                    process_actions: HashMap::new(),
                    next_action: 1,
                };
                let mut last_sample: Option<Instant> = None;
                while let Ok(job) = input.recv() {
                    match job {
                        Job::Sample => {
                            if last_sample.is_none_or(|at| at.elapsed() >= Duration::from_secs(2)) {
                                sampler.refresh_processes();
                                last_sample = Some(Instant::now());
                                if let Ok(mut cache) = output.lock() {
                                    *cache = sampler.snapshot.clone();
                                }
                            }
                        }
                        Job::Request(request, reply) => {
                            let id = request.id;
                            let result = match request.method {
                                Method::SystemProcessList(params) => {
                                    if last_sample
                                        .is_none_or(|at| at.elapsed() >= Duration::from_secs(2))
                                    {
                                        sampler.refresh_processes();
                                        last_sample = Some(Instant::now());
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
                                _ => Err(("unsupported_method", "不支持的进程查询".into())),
                            };
                            reply.response(&id, result);
                        }
                    }
                    sampler
                        .process_actions
                        .retain(|_, (_, _, created)| created.elapsed() < Duration::from_secs(60));
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
            reply.response(&id, Err(("server_busy", "进程查询繁忙，请稍后重试".into())));
        }
    }
}

struct ProcessSampler {
    system: System,
    users: Users,
    boot_id: String,
    snapshot: SystemMetricsSnapshot,
    process_actions: HashMap<String, (ProcessIdentity, crate::platform::MonitoredProcess, Instant)>,
    next_action: u64,
}

impl ProcessSampler {
    fn refresh_processes(&mut self) {
        let first = self.snapshot.sampled_at_ms == 0;
        if self.system.cpus().is_empty() {
            self.system.refresh_cpu_list(CpuRefreshKind::everything());
        } else {
            self.system.refresh_cpu_usage();
        }
        self.users.refresh();
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
            return Err(("stale_boot", "主机连接已更新，请重新选择进程".into()));
        }
        let before = crate::platform::process_instance_token(identity.pid)
            .map_err(|error| ("process_identity_unavailable", error.to_string()))?;
        if identity.instance_token.as_deref() != Some(before.as_str()) {
            return Err(("stale_process", "进程实例已经变化，请刷新列表".into()));
        }
        let native = crate::platform::MonitoredProcess::open(identity.pid).ok();
        if native
            .as_ref()
            .is_some_and(|process| process.instance_token != before)
        {
            return Err(("stale_process", "进程实例已经变化".into()));
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
            .ok_or_else(|| ("not_found", "进程已退出或无法读取".into()))?;
        if process.start_time() != identity.started_at {
            return Err(("stale_process", "PID 已被其他进程使用，请重新选择".into()));
        }
        let token = crate::platform::process_instance_token(identity.pid)
            .map_err(|error| ("process_identity_unavailable", error.to_string()))?;
        if token != before {
            return Err(("stale_process", "进程身份已经变化".into()));
        }
        let name = native
            .as_ref()
            .map(|process| process.name.clone())
            .unwrap_or_else(|| process.name().to_string_lossy().into_owned());
        self.process_actions
            .retain(|_, (_, _, at)| at.elapsed() < Duration::from_secs(60));
        let protected = protected_process(identity.pid, &name);
        let action_token = if !protected && self.process_actions.len() < 64 {
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
        let ticket = params
            .action_token
            .as_deref()
            .ok_or_else(|| ("identity_required", "请先打开进程详情并确认".into()))?;
        let Some((identity, process, created)) = self.process_actions.remove(ticket) else {
            return Err(("stale_process", "进程确认已过期，请重新打开详情".into()));
        };
        if identity != params.identity || created.elapsed() >= Duration::from_secs(60) {
            return Err(("stale_process", "进程确认与当前请求不匹配".into()));
        }
        if protected_process(identity.pid, &process.name) {
            return Err((
                "protected_process",
                "不能结束系统或当前 Herdr 连接进程".into(),
            ));
        }
        process
            .terminate(params.force)
            .map_err(|error| ("terminate_failed", error.to_string()))
    }
}
