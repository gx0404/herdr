use crate::api::schema::*;
use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc, Mutex,
};
use std::time::Instant;

struct Worker<T> {
    requests: mpsc::SyncSender<()>,
    cache: Arc<Mutex<(u64, T)>>,
    busy: Arc<AtomicBool>,
}

impl<T: Clone + Default + Send + 'static> Worker<T> {
    fn start(name: &str, mut collect: impl FnMut() -> T + Send + 'static) -> Option<Self> {
        let (requests, input) = mpsc::sync_channel(1);
        let cache = Arc::new(Mutex::new((0, T::default())));
        let busy = Arc::new(AtomicBool::new(false));
        let output = cache.clone();
        let pending = busy.clone();
        std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                while input.recv().is_ok() {
                    let value = collect();
                    if let Ok(mut state) = output.lock() {
                        *state = (super::super::now_ms(), value);
                    }
                    pending.store(false, Ordering::Release);
                }
            })
            .ok()?;
        Some(Self {
            requests,
            cache,
            busy,
        })
    }

    fn read(&self) -> (u64, T) {
        if !self.busy.swap(true, Ordering::AcqRel) && self.requests.try_send(()).is_err() {
            self.busy.store(false, Ordering::Release);
        }
        self.cache
            .lock()
            .map(|state| state.clone())
            .unwrap_or_default()
    }
}

pub(super) struct Workers {
    disks: Option<Worker<Vec<DiskMetric>>>,
    networks: Option<Worker<Vec<NetworkMetric>>>,
    sensors: Option<Worker<Vec<SensorMetric>>>,
}

pub(super) fn mark(snapshot: &mut SystemMetricsSnapshot, name: &str, sampled: u64) {
    snapshot.group_sampled_at_ms.insert(name.into(), sampled);
    snapshot.group_status.insert(
        name.into(),
        if sampled == 0 {
            ObservationStatus::Warming
        } else if super::super::now_ms().saturating_sub(sampled) > 15_000 {
            ObservationStatus::Stale
        } else {
            ObservationStatus::Ready
        },
    );
}

impl Workers {
    pub fn start() -> Self {
        let mut disks = sysinfo::Disks::new();
        let mut disk_last: Option<Instant> = None;
        let mut disk_totals = HashMap::<String, (u64, u64)>::new();
        let disks = Worker::start("herdr-disk-metrics", move || {
            disks.refresh(true);
            let now = Instant::now();
            let elapsed = disk_last.map(|at| now.duration_since(at).as_secs_f64());
            let mut totals = HashMap::new();
            let mut result = disks
                .iter()
                .map(|disk| {
                    let id = format!(
                        "{}:{}",
                        disk.name().to_string_lossy(),
                        disk.mount_point().display()
                    );
                    let usage = disk.usage();
                    let previous = disk_totals.get(&id);
                    totals.insert(
                        id.clone(),
                        (usage.total_read_bytes, usage.total_written_bytes),
                    );
                    DiskMetric {
                        id,
                        name: disk.name().to_string_lossy().into_owned(),
                        mount_point: disk.mount_point().to_string_lossy().into_owned(),
                        total_bytes: disk.total_space(),
                        available_bytes: disk.available_space(),
                        read_bytes_per_second: elapsed.and_then(|seconds| {
                            super::counter_rate(
                                previous.map(|v| v.0),
                                usage.total_read_bytes,
                                seconds,
                            )
                        }),
                        written_bytes_per_second: elapsed.and_then(|seconds| {
                            super::counter_rate(
                                previous.map(|v| v.1),
                                usage.total_written_bytes,
                                seconds,
                            )
                        }),
                    }
                })
                .collect::<Vec<_>>();
            // Bind mounts repeat one device under several paths with the same
            // capacities; report each device once, preferring the shortest
            // mount point as the canonical path.
            result.sort_by(|a, b| {
                a.mount_point
                    .len()
                    .cmp(&b.mount_point.len())
                    .then(a.id.cmp(&b.id))
            });
            let mut seen_devices = std::collections::HashSet::new();
            result.retain(|disk| {
                seen_devices.insert((disk.name.clone(), disk.total_bytes, disk.available_bytes))
            });
            result.sort_by(|a, b| a.id.cmp(&b.id));
            disk_totals = totals;
            disk_last = Some(now);
            result
        });
        let mut networks = sysinfo::Networks::new();
        let mut network_last: Option<Instant> = None;
        let mut network_totals = HashMap::<String, (u64, u64)>::new();
        let networks = Worker::start("herdr-network-metrics", move || {
            networks.refresh(true);
            let now = Instant::now();
            let elapsed = network_last.map(|at| now.duration_since(at).as_secs_f64());
            let mut totals = HashMap::new();
            let mut result = networks
                .iter()
                .map(|(name, value)| {
                    let previous = network_totals.get(name);
                    totals.insert(
                        name.clone(),
                        (value.total_received(), value.total_transmitted()),
                    );
                    NetworkMetric {
                        id: name.clone(),
                        total_received_bytes: value.total_received(),
                        total_transmitted_bytes: value.total_transmitted(),
                        received_bytes_per_second: elapsed.and_then(|seconds| {
                            super::counter_rate(
                                previous.map(|v| v.0),
                                value.total_received(),
                                seconds,
                            )
                        }),
                        transmitted_bytes_per_second: elapsed.and_then(|seconds| {
                            super::counter_rate(
                                previous.map(|v| v.1),
                                value.total_transmitted(),
                                seconds,
                            )
                        }),
                    }
                })
                .collect::<Vec<_>>();
            result.sort_by(|a, b| a.id.cmp(&b.id));
            network_totals = totals;
            network_last = Some(now);
            result
        });
        let mut components = sysinfo::Components::new();
        let sensors = Worker::start("herdr-temperature-metrics", move || {
            components.refresh(true);
            components
                .iter()
                .map(|sensor| SensorMetric {
                    name: sensor.label().into(),
                    temperature_celsius: sensor.temperature().filter(|v| v.is_finite()),
                    critical_celsius: sensor.critical().filter(|v| v.is_finite()),
                })
                .collect()
        });
        Self {
            disks,
            networks,
            sensors,
        }
    }

    pub fn read(&self, params: &SystemMetricsParams, snapshot: &mut SystemMetricsSnapshot) {
        let wants = |name: &str| {
            params.groups.is_empty() || params.groups.iter().any(|group| group == name)
        };
        if wants("disks") {
            if let Some(worker) = &self.disks {
                let (at, values) = worker.read();
                snapshot.disks = values;
                mark(snapshot, "disks", at);
            }
        }
        if wants("network") {
            if let Some(worker) = &self.networks {
                let (at, values) = worker.read();
                snapshot.networks = values;
                mark(snapshot, "network", at);
            }
        }
        if wants("sensors") {
            if let Some(worker) = &self.sensors {
                let (at, values) = worker.read();
                snapshot.sensors = values;
                mark(snapshot, "sensors", at);
            }
        }
    }
}
