use crate::api::schema::*;
use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc, Mutex,
};
use std::time::{Duration, Instant};

const DISK_INVENTORY_TTL: Duration = Duration::from_secs(30);
const DISK_SAMPLE_TTL: Duration = Duration::from_secs(1);
const NETWORK_SAMPLE_TTL: Duration = Duration::from_secs(1);
const SENSOR_SAMPLE_TTL: Duration = Duration::from_secs(5);
const EMPTY_RESULT_BACKOFF: Duration = Duration::from_secs(15);

#[derive(Default)]
struct SamplingGate {
    next: Option<Instant>,
}

impl SamplingGate {
    fn due(&self, now: Instant) -> bool {
        self.next.is_none_or(|next| now >= next)
    }

    fn record(&mut self, now: Instant, empty: bool, ttl: Duration) {
        self.next = Some(now + if empty { EMPTY_RESULT_BACKOFF } else { ttl });
    }
}

fn sample_with_gate<T>(
    gate: &mut SamplingGate,
    now: Instant,
    ttl: Duration,
    collect: impl FnOnce() -> T,
    is_empty: impl FnOnce(&T) -> bool,
) -> Option<T> {
    if !gate.due(now) {
        return None;
    }
    let value = collect();
    gate.record(now, is_empty(&value), ttl);
    Some(value)
}

struct Worker<T> {
    requests: mpsc::SyncSender<()>,
    cache: Arc<Mutex<(u64, T)>>,
    busy: Arc<AtomicBool>,
}

impl<T: Clone + Default + Send + 'static> Worker<T> {
    fn start(name: &str, mut collect: impl FnMut() -> Option<T> + Send + 'static) -> Option<Self> {
        let (requests, input) = mpsc::sync_channel(1);
        let cache = Arc::new(Mutex::new((0, T::default())));
        let busy = Arc::new(AtomicBool::new(false));
        let output = cache.clone();
        let pending = busy.clone();
        std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                while input.recv().is_ok() {
                    if let Some(value) = collect() {
                        if let Ok(mut state) = output.lock() {
                            *state = (super::super::now_ms(), value);
                        }
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

/// 伪文件系统与只读镜像挂载不进磁盘列表：它们要么没有容量，要么是 snap /
/// AppImage 之类的 squashfs 快照（占用率恒为 100%），只会淹没真正的数据盘。
pub(super) fn pseudo_filesystem(file_system: &str) -> bool {
    matches!(
        file_system,
        "tmpfs"
            | "devtmpfs"
            | "squashfs"
            | "overlay"
            | "overlayfs"
            | "proc"
            | "sysfs"
            | "cgroup"
            | "cgroup2"
            | "devpts"
            | "efivarfs"
            | "fusectl"
            | "debugfs"
            | "tracefs"
            | "securityfs"
            | "pstore"
            | "hugetlbfs"
            | "mqueue"
            | "bpf"
            | "autofs"
            | "binfmt_misc"
            | "configfs"
            | "ramfs"
            | "iso9660"
            | "nsfs"
            | "fuse.portal"
            | "fuse.gvfsd-fuse"
            | "fuse.snapfuse"
    )
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
        let mut disk_inventory = SamplingGate::default();
        let mut disk_sample = SamplingGate::default();
        let mut disk_last: Option<Instant> = None;
        let mut disk_totals = HashMap::<String, (u64, u64)>::new();
        let disks = Worker::start("herdr-disk-metrics", move || {
            let now = Instant::now();
            if !disk_sample.due(now) {
                return None;
            }
            if disk_inventory.due(now) {
                disks.refresh(true);
                disk_inventory.record(now, disks.is_empty(), DISK_INVENTORY_TTL);
            } else {
                for disk in disks.list_mut() {
                    let _ = disk.refresh();
                }
            }
            let elapsed = disk_last.map(|at| now.saturating_duration_since(at).as_secs_f64());
            let mut totals = HashMap::new();
            let mut result = disks
                .iter()
                .filter(|disk| {
                    disk.total_space() > 0
                        && !pseudo_filesystem(&disk.file_system().to_string_lossy())
                })
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
            disk_sample.record(now, result.is_empty(), DISK_SAMPLE_TTL);
            Some(result)
        });
        let mut networks = sysinfo::Networks::new();
        let mut network_sample = SamplingGate::default();
        let mut network_last: Option<Instant> = None;
        let mut network_totals = HashMap::<String, (u64, u64)>::new();
        let networks = Worker::start("herdr-network-metrics", move || {
            let now = Instant::now();
            sample_with_gate(
                &mut network_sample,
                now,
                NETWORK_SAMPLE_TTL,
                || {
                    networks.refresh(true);
                    let elapsed =
                        network_last.map(|at| now.saturating_duration_since(at).as_secs_f64());
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
                },
                |result| result.is_empty(),
            )
        });
        let mut components = sysinfo::Components::new();
        let mut sensor_sample = SamplingGate::default();
        let sensors = Worker::start("herdr-temperature-metrics", move || {
            let now = Instant::now();
            sample_with_gate(
                &mut sensor_sample,
                now,
                SENSOR_SAMPLE_TTL,
                || {
                    components.refresh(true);
                    components
                        .iter()
                        .map(|sensor| SensorMetric {
                            name: sensor.label().into(),
                            temperature_celsius: sensor.temperature().filter(|v| v.is_finite()),
                            critical_celsius: sensor.critical().filter(|v| v.is_finite()),
                        })
                        .collect()
                },
                |result: &Vec<SensorMetric>| result.is_empty(),
            )
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 数据盘保留，tmpfs / squashfs / overlay 等伪文件系统与镜像挂载过滤掉。
    #[test]
    fn pseudo_filesystems_are_filtered_out_of_the_disk_list() {
        for real in [
            "ext4", "xfs", "btrfs", "zfs", "ntfs", "apfs", "vfat", "nfs", "exfat",
        ] {
            assert!(!pseudo_filesystem(real), "{real} 是真实数据盘");
        }
        for pseudo in [
            "tmpfs", "squashfs", "overlay", "proc", "sysfs", "devtmpfs", "efivarfs",
        ] {
            assert!(pseudo_filesystem(pseudo), "{pseudo} 应被过滤");
        }
    }

    #[test]
    fn peripheral_sampling_gate_bounds_non_empty_calls_with_controlled_clock() {
        let start = Instant::now();
        let mut gate = SamplingGate::default();
        let mut calls = 0;
        assert_eq!(
            sample_with_gate(
                &mut gate,
                start,
                Duration::from_secs(1),
                || {
                    calls += 1;
                    vec![1_u8]
                },
                |value: &Vec<u8>| value.is_empty(),
            ),
            Some(vec![1])
        );
        assert_eq!(
            sample_with_gate(
                &mut gate,
                start + Duration::from_millis(999),
                Duration::from_secs(1),
                || {
                    calls += 1;
                    vec![2_u8]
                },
                |value: &Vec<u8>| value.is_empty(),
            ),
            None
        );
        assert_eq!(
            sample_with_gate(
                &mut gate,
                start + Duration::from_secs(1),
                Duration::from_secs(1),
                || {
                    calls += 1;
                    vec![2_u8]
                },
                |value: &Vec<u8>| value.is_empty(),
            ),
            Some(vec![2])
        );
        assert_eq!(calls, 2);
    }

    #[test]
    fn peripheral_sampling_gate_backs_off_empty_results() {
        let start = Instant::now();
        let mut gate = SamplingGate::default();
        let mut calls = 0;
        assert_eq!(
            sample_with_gate(
                &mut gate,
                start,
                SENSOR_SAMPLE_TTL,
                || {
                    calls += 1;
                    Vec::<u8>::new()
                },
                |value: &Vec<u8>| value.is_empty(),
            ),
            Some(Vec::new())
        );
        assert_eq!(
            sample_with_gate(
                &mut gate,
                start + SENSOR_SAMPLE_TTL,
                SENSOR_SAMPLE_TTL,
                || {
                    calls += 1;
                    Vec::<u8>::new()
                },
                |value: &Vec<u8>| value.is_empty(),
            ),
            None
        );
        assert_eq!(
            sample_with_gate(
                &mut gate,
                start + EMPTY_RESULT_BACKOFF,
                SENSOR_SAMPLE_TTL,
                || {
                    calls += 1;
                    Vec::<u8>::new()
                },
                |value: &Vec<u8>| value.is_empty(),
            ),
            Some(Vec::new())
        );
        assert_eq!(calls, 2);
    }

    #[test]
    fn disk_inventory_ttl_is_longer_than_numeric_sample_ttl() {
        assert!(DISK_INVENTORY_TTL > DISK_SAMPLE_TTL);
        let start = Instant::now();
        let mut inventory = SamplingGate::default();
        inventory.record(start, false, DISK_INVENTORY_TTL);
        assert!(!inventory.due(start + DISK_SAMPLE_TTL));
        assert!(inventory.due(start + DISK_INVENTORY_TTL));
    }
}
