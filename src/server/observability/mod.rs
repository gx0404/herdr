//! server 拥有的后台观测服务。请求、采集及订阅不占用终端焦点通道。

mod accounts;
pub(crate) use accounts::run_probe_helper;
mod gpu;
mod system;

use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc,
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::api::schema::*;
use crate::server::client_transport::ServerEvent;

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

pub(crate) fn is_background_method(method: &Method) -> bool {
    let name = crate::api::api_method_name(method);
    name.starts_with("system.") || name.starts_with("account.")
}

#[derive(Clone)]
pub(crate) enum Reply {
    Api {
        sender: mpsc::Sender<String>,
        active: Option<Arc<AtomicBool>>,
        latest: Option<Arc<std::sync::Mutex<Option<String>>>>,
    },
    Endpoint {
        client_id: u64,
        boot_id: String,
        events: tokio::sync::mpsc::Sender<ServerEvent>,
        active: Arc<AtomicBool>,
    },
}

impl Reply {
    fn alive(&self) -> bool {
        match self {
            Self::Api { active, .. } => active
                .as_ref()
                .is_none_or(|flag| flag.load(Ordering::Acquire)),
            Self::Endpoint { events, active, .. } => {
                active.load(Ordering::Acquire) && !events.is_closed()
            }
        }
    }

    pub(super) fn response(&self, id: &str, result: Result<ResponseResult, (&str, String)>) {
        if !self.alive() {
            return;
        }
        let text = match result {
            Ok(result) => serde_json::to_string(&SuccessResponse {
                id: id.into(),
                result,
            }),
            Err((code, message)) => serde_json::to_string(&ErrorResponse {
                id: id.into(),
                error: ErrorBody {
                    code: code.into(),
                    message,
                },
            }),
        };
        let Ok(text) = text else {
            return;
        };
        match self {
            Self::Api { sender, .. } => {
                let _ = sender.send(text);
            }
            Self::Endpoint {
                client_id,
                boot_id,
                events,
                ..
            } => {
                let event = ServerEvent::ObservationResponse {
                    client_id: *client_id,
                    boot_id: boot_id.clone(),
                    message: crate::protocol::ServerMessage::ClientShellEndpointResponseChunk {
                        boot_id: boot_id.clone(),
                        request_id: id.into(),
                        final_chunk: true,
                        data: text.into_bytes(),
                    },
                };
                if let Err(tokio::sync::mpsc::error::TrySendError::Full(event)) =
                    events.try_send(event)
                {
                    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                        let events = events.clone();
                        runtime.spawn(async move {
                            let _ = events.send(event).await;
                        });
                    } else {
                        let _ = events.blocking_send(event);
                    }
                }
            }
        }
    }

    pub(super) fn event(&self, topic: &str, data: serde_json::Value) -> bool {
        let payload = serde_json::json!({"event": topic, "data": data});
        let Ok(text) = serde_json::to_string(&payload) else {
            return false;
        };
        match self {
            Self::Api { sender, latest, .. } => {
                if !self.alive() {
                    return false;
                }
                if let Some(latest) = latest {
                    if let Ok(mut slot) = latest.lock() {
                        *slot = Some(text);
                        true
                    } else {
                        false
                    }
                } else {
                    sender.send(text).is_ok()
                }
            }
            Self::Endpoint {
                client_id,
                boot_id,
                events,
                ..
            } => {
                let result = events.try_send(ServerEvent::ObservationResponse {
                    client_id: *client_id, boot_id: boot_id.clone(),
                    message: crate::protocol::ServerMessage::EndpointControl {
                        kind: "endpoint.observation.v1".into(),
                        data: serde_json::json!({"boot_id": boot_id, "event": topic, "data": payload["data"]}).to_string(),
                    },
                });
                !matches!(
                    result,
                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_))
                )
            }
        }
    }

    fn owner(&self) -> Option<u64> {
        match self {
            Self::Endpoint { client_id, .. } => Some(*client_id),
            _ => None,
        }
    }
}

struct RequestJob {
    request: Request,
    reply: Reply,
}

enum SystemJob {
    Request(Box<RequestJob>),
    Release(u64),
}

struct Subscription {
    owner: Option<u64>,
    reply: Reply,
    params: SystemMetricsParams,
    last_sequence: u64,
    last_sent: Option<Instant>,
}

pub(crate) struct Runtime {
    system: mpsc::SyncSender<SystemJob>,
    processes: system::ProcessWorker,
    accounts: accounts::Service,
}

impl Runtime {
    pub(crate) fn start(boot_id: String) -> std::io::Result<Self> {
        let processes = system::ProcessWorker::start(boot_id.clone())?;
        let process_reader = processes.clone();
        let (sender, jobs) = mpsc::sync_channel(64);
        std::thread::Builder::new()
            .name("herdr-system-metrics".into())
            .spawn(move || {
                let mut sampler = system::Sampler::new(boot_id, process_reader);
                let gpu = gpu::GpuWorker::start().ok();
                let mut subscribers = HashMap::<String, Subscription>::new();
                let mut recent = HashMap::<String, (SystemMetricsParams, Instant)>::new();
                let mut subscription_sequence = 0_u64;
                loop {
                    let job = match jobs.recv_timeout(Duration::from_millis(100)) {
                        Ok(job) => Some(job),
                        Err(mpsc::RecvTimeoutError::Timeout) => None,
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    };
                    if let Some(SystemJob::Release(owner)) = job {
                        subscribers.retain(|_, sub| sub.owner != Some(owner));
                        recent.remove(&format!("client-{owner}"));
                    } else if let Some(SystemJob::Request(job)) = job {
                        let id = job.request.id;
                        let result = match job.request.method {
                            Method::SystemMetricsGet(params) => {
                                let key = job
                                    .reply
                                    .owner()
                                    .map(|owner| format!("client-{owner}"))
                                    .unwrap_or_else(|| {
                                        format!(
                                            "api:{:?}:{}:{}",
                                            params.groups,
                                            params.include_processes,
                                            params.interval_ms
                                        )
                                    });
                                remember_demand(&mut recent, key, params.clone(), Instant::now());
                                let mut snapshot = sampler.snapshot.clone();
                                if snapshot.sampled_at_ms > 0
                                    && now_ms().saturating_sub(snapshot.sampled_at_ms) > 15_000
                                {
                                    snapshot.status = ObservationStatus::Stale;
                                }
                                if !params.include_processes {
                                    snapshot.processes.clear();
                                }
                                Ok(ResponseResult::SystemMetrics {
                                    snapshot: Box::new(snapshot),
                                })
                            }
                            Method::SystemMetricsSubscribe(params) => {
                                subscription_sequence = subscription_sequence.saturating_add(1);
                                let subscription_id = format!("system-{subscription_sequence}");
                                if subscribers.len() >= 256 {
                                    Err(("subscription_limit", "监控订阅数量超过限制".into()))
                                } else {
                                    subscribers.insert(
                                        subscription_id.clone(),
                                        Subscription {
                                            owner: job.reply.owner(),
                                            reply: job.reply.clone(),
                                            params,
                                            last_sequence: 0,
                                            last_sent: None,
                                        },
                                    );
                                    Ok(ResponseResult::ObservationSubscription {
                                        subscription_id,
                                        active: true,
                                    })
                                }
                            }
                            Method::SystemMetricsUnsubscribe(params) => {
                                let id = params.subscription_id.unwrap_or_default();
                                if subscribers
                                    .get(&id)
                                    .is_some_and(|sub| sub.owner == job.reply.owner())
                                {
                                    subscribers.remove(&id);
                                }
                                Ok(ResponseResult::ObservationSubscription {
                                    subscription_id: id,
                                    active: false,
                                })
                            }
                            _ => Err(("unsupported_method", "不支持的监控请求".into())),
                        };
                        job.reply.response(&id, result);
                    }
                    subscribers.retain(|_, sub| sub.reply.alive());
                    recent.retain(|_, (_, deadline)| *deadline > Instant::now());
                    if !recent.is_empty() || !subscribers.is_empty() {
                        let params = aggregate_demand(
                            recent
                                .values()
                                .map(|(params, _)| params)
                                .chain(subscribers.values().map(|sub| &sub.params)),
                        );
                        let sequence = sampler.snapshot.sequence;
                        sampler.refresh(&params, Instant::now());
                        if sequence != sampler.snapshot.sequence {
                            if params.groups.is_empty()
                                || params.groups.iter().any(|group| group == "gpu")
                            {
                                if let Some(gpu) = &gpu {
                                    let (sampled, gpus) = gpu.request_and_read();
                                    let status = if sampled == 0 {
                                        ObservationStatus::Warming
                                    } else if now_ms().saturating_sub(sampled) > 15_000 {
                                        ObservationStatus::Stale
                                    } else if gpus.is_empty() {
                                        ObservationStatus::Unsupported
                                    } else {
                                        ObservationStatus::Ready
                                    };
                                    sampler.snapshot.group_status.insert("gpu".into(), status);
                                    sampler
                                        .snapshot
                                        .group_sampled_at_ms
                                        .insert("gpu".into(), sampled);
                                    sampler.snapshot.gpus = gpus;
                                }
                            }
                            subscribers.retain(|_, sub| {
                                if sub.last_sequence == sampler.snapshot.sequence {
                                    return true;
                                }
                                if sub.last_sent.is_some_and(|at| {
                                    at.elapsed()
                                        < Duration::from_millis(
                                            sub.params.interval_ms.clamp(500, 5000),
                                        )
                                }) {
                                    return true;
                                }
                                sub.last_sequence = sampler.snapshot.sequence;
                                sub.last_sent = Some(Instant::now());
                                let mut snapshot = sampler.snapshot.clone();
                                if !sub.params.include_processes {
                                    snapshot.processes.clear();
                                }
                                sub.reply.event(
                                    "system.metrics.updated",
                                    serde_json::json!({"snapshot": snapshot}),
                                )
                            });
                        }
                    }
                }
            })?;
        Ok(Self {
            system: sender,
            processes,
            accounts: accounts::Service::start()?,
        })
    }

    pub(crate) fn submit(&self, request: Request, reply: Reply) {
        if matches!(
            &request.method,
            Method::SystemProcessList(_)
                | Method::SystemProcessGet(_)
                | Method::SystemProcessTerminate(_)
        ) {
            self.processes.submit(request, reply);
            return;
        }
        if crate::api::api_method_name(&request.method).starts_with("account.") {
            self.accounts.submit(request, reply);
        } else {
            let id = request.id.clone();
            if let Err(error) = self
                .system
                .try_send(SystemJob::Request(Box::new(RequestJob {
                    request,
                    reply: reply.clone(),
                })))
            {
                let message = match error {
                    mpsc::TrySendError::Full(_) => "监控查询繁忙，请稍后重试",
                    mpsc::TrySendError::Disconnected(_) => "监控服务不可用",
                };
                reply.response(&id, Err(("server_busy", message.into())));
            }
        }
    }

    pub(crate) fn release(&self, client_id: u64) {
        let _ = self.system.try_send(SystemJob::Release(client_id));
        self.accounts.release(client_id);
    }
}

fn remember_demand(
    recent: &mut HashMap<String, (SystemMetricsParams, Instant)>,
    key: String,
    params: SystemMetricsParams,
    now: Instant,
) {
    recent.retain(|_, (_, deadline)| *deadline > now);
    if recent.len() >= 64 && !recent.contains_key(&key) {
        if let Some(oldest) = recent
            .iter()
            .min_by_key(|(_, (_, deadline))| *deadline)
            .map(|(key, _)| key.clone())
        {
            recent.remove(&oldest);
        }
    }
    recent.insert(key, (params, now + Duration::from_secs(10)));
}

fn aggregate_demand<'a>(
    consumers: impl Iterator<Item = &'a SystemMetricsParams>,
) -> SystemMetricsParams {
    let mut params = SystemMetricsParams {
        interval_ms: 5000,
        include_processes: false,
        groups: Vec::new(),
    };
    let mut all_groups = false;
    for consumer in consumers {
        params.interval_ms = params
            .interval_ms
            .min(consumer.interval_ms.clamp(500, 5000));
        params.include_processes |= consumer.include_processes;
        all_groups |= consumer.groups.is_empty();
        for group in &consumer.groups {
            if !params.groups.contains(group) {
                params.groups.push(group.clone());
            }
        }
    }
    if all_groups {
        params.groups.clear();
    }
    params
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn polling_clients_keep_independent_groups_until_their_demand_expires() {
        let mut recent = HashMap::new();
        let now = Instant::now();
        remember_demand(
            &mut recent,
            "cpu-client".into(),
            SystemMetricsParams {
                groups: vec!["cpu".into()],
                interval_ms: 1000,
                include_processes: false,
            },
            now,
        );
        remember_demand(
            &mut recent,
            "network-client".into(),
            SystemMetricsParams {
                groups: vec!["network".into()],
                interval_ms: 5000,
                include_processes: false,
            },
            now,
        );
        let params = aggregate_demand(recent.values().map(|(params, _)| params));
        assert!(params.groups.contains(&"cpu".into()) && params.groups.contains(&"network".into()));
        assert_eq!(params.interval_ms, 1000);
        remember_demand(
            &mut recent,
            "later".into(),
            SystemMetricsParams::default(),
            now + Duration::from_secs(11),
        );
        assert_eq!(recent.len(), 1);
    }

    #[test]
    fn only_live_consumers_contribute_to_sampling_demand() {
        let cpu = SystemMetricsParams {
            interval_ms: 5000,
            groups: vec!["cpu".into()],
            ..Default::default()
        };
        let network = SystemMetricsParams {
            interval_ms: 500,
            groups: vec!["network".into()],
            ..Default::default()
        };
        let demand = aggregate_demand([&cpu, &network].into_iter());
        assert_eq!(demand.interval_ms, 500);
        assert!(!demand.include_processes);
        assert_eq!(demand.groups, ["cpu", "network"]);
        assert_eq!(aggregate_demand([&cpu].into_iter()), cpu);
    }

    #[tokio::test]
    async fn endpoint_queue_pressure_never_panics_in_async_server() {
        let (events, _receiver) = tokio::sync::mpsc::channel(1);
        let reply = Reply::Endpoint {
            client_id: 1,
            boot_id: "boot".into(),
            events,
            active: Arc::new(AtomicBool::new(true)),
        };
        reply.response("one", Ok(ResponseResult::Ok {}));
        reply.response("two", Err(("server_busy", "busy".into())));
    }
}
