//! CPU、内存与窄缓存投影；可能阻塞的后端在各自有界线程内采集。
mod peripherals;
mod processes;
pub(super) use processes::ProcessWorker;

use crate::api::schema::*;
use std::time::{Duration, Instant};
use sysinfo::{CpuRefreshKind, System};

pub(super) struct Sampler {
    system: System,
    last_cpu: Option<Instant>,
    last_inventory: Option<Instant>,
    inventory: Option<String>,
    peripherals: peripherals::Workers,
    processes: ProcessWorker,
    pub snapshot: SystemMetricsSnapshot,
}

pub(super) fn counter_rate(previous: Option<u64>, current: u64, elapsed: f64) -> Option<f64> {
    if !elapsed.is_finite() || elapsed <= 0.0 {
        return None;
    }
    current
        .checked_sub(previous?)
        .map(|delta| delta as f64 / elapsed)
}

fn percent(value: f32) -> Option<f32> {
    value.is_finite().then(|| value.clamp(0.0, 100.0))
}

impl Sampler {
    pub fn new(boot_id: String, processes: ProcessWorker) -> Self {
        sysinfo::set_open_files_limit(0);
        Self {
            system: System::new(),
            last_cpu: None,
            last_inventory: None,
            inventory: None,
            peripherals: peripherals::Workers::start(),
            processes,
            snapshot: SystemMetricsSnapshot {
                boot_id,
                hostname: System::host_name().unwrap_or_else(|| "本机".into()),
                operating_system: System::long_os_version()
                    .unwrap_or_else(|| std::env::consts::OS.into()),
                environment: crate::platform::monitor_environment(),
                physical_core_count: System::physical_core_count(),
                ..Default::default()
            },
        }
    }

    pub fn refresh(&mut self, params: &SystemMetricsParams, now: Instant) {
        let interval_ms = params.interval_ms.clamp(500, 5000);
        if self
            .last_cpu
            .is_some_and(|last| now.duration_since(last) < Duration::from_millis(interval_ms))
        {
            return;
        }
        let inventory_due = self
            .last_inventory
            .is_none_or(|at| now.duration_since(at) >= Duration::from_secs(30));
        let inventory = if inventory_due {
            crate::platform::monitor_cpu_inventory()
        } else {
            self.inventory.clone()
        };
        let first = self.system.cpus().is_empty()
            || inventory
                .as_ref()
                .zip(self.inventory.as_ref())
                .is_some_and(|(new, old)| new != old);
        if first {
            self.system.refresh_cpu_list(CpuRefreshKind::everything());
        } else {
            self.system.refresh_cpu_usage();
        }
        if inventory_due {
            self.inventory = inventory;
            self.system.refresh_cpu_frequency();
            self.last_inventory = Some(now);
        }
        self.system.refresh_memory();
        self.snapshot.cpu_brand = self
            .system
            .cpus()
            .first()
            .map(|cpu| cpu.brand().into())
            .unwrap_or_default();
        self.snapshot.cpu_percent = (!first)
            .then(|| percent(self.system.global_cpu_usage()))
            .flatten();
        self.snapshot.cores = self
            .system
            .cpus()
            .iter()
            .enumerate()
            .map(|(id, cpu)| CpuCoreMetric {
                id,
                name: cpu.name().into(),
                usage_percent: (!first).then(|| percent(cpu.cpu_usage())).flatten(),
                frequency_mhz: (cpu.frequency() > 0).then_some(cpu.frequency()),
            })
            .collect();
        self.snapshot.memory = MemoryMetric {
            total_bytes: self.system.total_memory(),
            used_bytes: self.system.used_memory(),
            available_bytes: self.system.available_memory(),
            swap_total_bytes: self.system.total_swap(),
            swap_used_bytes: self.system.used_swap(),
        };
        self.peripherals.read(params, &mut self.snapshot);
        if params.include_processes {
            let (at, processes) = self.processes.request_and_read();
            self.snapshot.processes = processes;
            peripherals::mark(&mut self.snapshot, "processes", at);
        } else {
            self.snapshot.processes.clear();
        }
        self.snapshot.sequence = self.snapshot.sequence.saturating_add(1);
        self.snapshot.sampled_at_ms = super::now_ms();
        self.snapshot.interval_ms = interval_ms;
        self.snapshot.uptime_seconds = System::uptime();
        self.snapshot.status = if !sysinfo::IS_SUPPORTED_SYSTEM {
            ObservationStatus::Unsupported
        } else if first {
            ObservationStatus::Warming
        } else {
            ObservationStatus::Ready
        };
        self.last_cpu = Some(now);
    }
}

pub(super) fn protected_process(pid: u32, name: &str) -> bool {
    pid <= 1
        || pid == std::process::id()
        || matches!(
            name.to_ascii_lowercase().as_str(),
            "herdr"
                | "herdr.exe"
                | "herdr-dev"
                | "herdr-dev.exe"
                | "system"
                | "registry"
                | "smss.exe"
                | "csrss.exe"
                | "wininit.exe"
                | "lsass.exe"
        )
}

pub(super) fn process_page(
    snapshot: &SystemMetricsSnapshot,
    params: &ProcessListParams,
) -> (usize, Vec<ProcessMetric>) {
    let filter = params.filter.to_lowercase();
    let mut processes = snapshot
        .processes
        .iter()
        .filter(|p| {
            filter.is_empty()
                || p.name.to_lowercase().contains(&filter)
                || p.identity.pid.to_string().contains(&filter)
        })
        .cloned()
        .collect::<Vec<_>>();
    processes.sort_by(|a, b| {
        let order = match params.sort {
            ProcessSort::Cpu => a
                .cpu_percent
                .unwrap_or(-1.0)
                .total_cmp(&b.cpu_percent.unwrap_or(-1.0)),
            ProcessSort::Memory => a.memory_bytes.cmp(&b.memory_bytes),
            ProcessSort::Name => a.name.cmp(&b.name),
            ProcessSort::Pid => a.identity.pid.cmp(&b.identity.pid),
        };
        (if params.descending {
            order.reverse()
        } else {
            order
        })
        .then(a.identity.pid.cmp(&b.identity.pid))
    });
    let total = processes.len();
    (
        total,
        processes
            .into_iter()
            .skip(params.offset)
            .take(params.limit.clamp(1, 500))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn differential_counters_do_not_invent_initial_or_reset_rates() {
        assert_eq!(counter_rate(None, 100, 1.0), None);
        assert_eq!(counter_rate(Some(100), 90, 1.0), None);
        assert_eq!(counter_rate(Some(100), 140, 2.0), Some(20.0));
        assert_eq!(counter_rate(Some(100), 140, 0.0), None);
    }

    #[test]
    fn process_filter_and_sort_are_stable_and_bounded() {
        let mut snapshot = SystemMetricsSnapshot::default();
        for (pid, cpu, name) in [(7, 30.0, "worker"), (9, 50.0, "worker"), (4, 60.0, "shell")] {
            snapshot.processes.push(ProcessMetric {
                identity: ProcessIdentity {
                    pid,
                    ..Default::default()
                },
                name: name.into(),
                cpu_percent: Some(cpu),
                ..Default::default()
            });
        }
        let (total, page) = process_page(
            &snapshot,
            &ProcessListParams {
                filter: "worker".into(),
                limit: 1,
                ..Default::default()
            },
        );
        assert_eq!(total, 2);
        assert_eq!(page[0].identity.pid, 9);
        assert!(protected_process(std::process::id(), "test"));
        assert!(!protected_process(99999, "worker"));
    }
}
