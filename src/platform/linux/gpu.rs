//! Linux GPU metrics: prefer NVML, then bounded nvidia-smi or DRM counters.

use std::collections::HashMap;
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::{fs::PermissionsExt, process::CommandExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::api::schema::{GpuMetric, ObservationStatus};

#[path = "gpu_fdinfo.rs"]
mod fdinfo;

type SmiRunner = Box<dyn FnMut(&[String]) -> io::Result<String> + Send>;

pub(crate) struct NativeGpuCollector {
    drm_root: PathBuf,
    engines: fdinfo::EngineSampler,
    smi_runner: SmiRunner,
    retry_at: Option<Instant>,
    failure: Option<(io::ErrorKind, String)>,
    names: HashMap<String, String>,
}

impl Default for NativeGpuCollector {
    fn default() -> Self {
        Self {
            drm_root: "/sys/class/drm".into(),
            engines: fdinfo::EngineSampler::new("/proc".into()),
            smi_runner: Box::new(run_nvidia_smi),
            retry_at: None,
            failure: None,
            names: HashMap::new(),
        }
    }
}

impl NativeGpuCollector {
    #[cfg(test)]
    fn with_sources(drm_root: PathBuf, proc_root: PathBuf, smi_runner: SmiRunner) -> Self {
        Self {
            drm_root,
            engines: fdinfo::EngineSampler::new(proc_root),
            smi_runner,
            ..Self::default()
        }
    }

    pub(crate) fn sample(&mut self, nvidia: Vec<GpuMetric>) -> Vec<GpuMetric> {
        self.sample_at(nvidia, Instant::now())
    }

    fn sample_at(&mut self, mut nvidia: Vec<GpuMetric>, now: Instant) -> Vec<GpuMetric> {
        let mut cards: Vec<_> = std::fs::read_dir(&self.drm_root)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|card| {
                let name = card.file_name().to_string_lossy().into_owned();
                let digits = name.strip_prefix("card")?;
                if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                Some((digits.parse::<u64>().ok()?, name, card.path()))
            })
            .collect();
        cards.sort_by_key(|(index, _, _)| *index);
        // WSL may expose NVIDIA through its tools without a DRM card node.
        if cards.is_empty() && nvidia.is_empty() {
            return self.smi_metrics(&[], now).unwrap_or_default();
        }
        let mut native = Vec::new();
        let mut smi_devices = Vec::new();
        let mut engine_devices = Vec::new();
        for (_, card_name, card_path) in cards {
            let device = card_path.join("device");
            let pci = std::fs::canonicalize(&device)
                .ok()
                .and_then(|p| p.file_name().map(|s| s.to_string_lossy().to_lowercase()))
                .unwrap_or_else(|| card_name.clone());
            if nvidia.iter().any(|gpu| pci_matches(&gpu.id, &pci)) {
                continue;
            }
            // Identity attributes are safe; runtime metrics below are read only while awake.
            let vendor = std::fs::read_to_string(device.join("vendor")).unwrap_or_default();
            let vendor = match vendor.trim() {
                "0x10de" => "NVIDIA",
                "0x1002" => "AMD",
                "0x8086" => "Intel",
                _ => "GPU",
            };
            let mut gpu = GpuMetric {
                id: pci.clone(),
                name: self
                    .names
                    .iter()
                    .find_map(|(id, name)| pci_matches(id, &pci).then(|| name.clone()))
                    .unwrap_or_else(|| format!("{vendor} {card_name} ({pci})")),
                vendor: vendor.into(),
                source: "DRM/sysfs".into(),
                status: ObservationStatus::Unsupported,
                ..Default::default()
            };
            let suspended = std::fs::read_to_string(device.join("power/runtime_status"))
                .is_ok_and(|status| status.trim() == "suspended");
            if suspended {
                gpu.usage_percent = Some(0.0);
                gpu.status = ObservationStatus::Ready;
                gpu.message = Some(super::texts().gpu_runtime_suspended.into());
                native.push(gpu);
                continue;
            }
            gpu.usage_percent =
                number(&device.join("gpu_busy_percent")).map(|value| value.min(100) as f32);
            gpu.memory_used_bytes = number(&device.join("mem_info_vram_used"));
            gpu.memory_total_bytes = number(&device.join("mem_info_vram_total"));
            if let Ok(monitors) = std::fs::read_dir(device.join("hwmon")) {
                for monitor in monitors.flatten() {
                    gpu.temperature_celsius =
                        number(&monitor.path().join("temp1_input")).map(|v| v as f32 / 1000.0);
                    gpu.power_watts = number(&monitor.path().join("power1_average"))
                        .or_else(|| number(&monitor.path().join("power1_input")))
                        .map(|v| v as f32 / 1_000_000.0);
                    if gpu.temperature_celsius.is_some() || gpu.power_watts.is_some() {
                        break;
                    }
                }
            }
            if gpu.usage_percent.is_some() {
                gpu.status = ObservationStatus::Ready;
            } else {
                gpu.message = Some(super::texts().gpu_utilization_unexposed.into());
            }
            if vendor == "NVIDIA" {
                smi_devices.push(pci);
            } else if gpu.usage_percent.is_none() && is_pci_address(&pci) {
                engine_devices.push(pci);
            }
            native.push(gpu);
        }
        // Sample engine counters before a potentially slow driver command so the supplied
        // timestamp stays close to the scan and timeout/backoff cannot skew its next delta.
        let engines = if engine_devices.is_empty() {
            HashMap::new()
        } else {
            self.engines.sample(&engine_devices, now)
        };
        let smi = if smi_devices.is_empty() {
            Vec::new()
        } else {
            self.smi_metrics(&smi_devices, now).unwrap_or_default()
        };
        for mut gpu in native {
            if smi_devices.contains(&gpu.id) {
                if let Some(metric) = smi.iter().find(|metric| pci_matches(&metric.id, &gpu.id)) {
                    gpu = metric.clone();
                } else {
                    gpu.status = ObservationStatus::Unsupported;
                    gpu.message = Some(
                        if self
                            .failure
                            .as_ref()
                            .is_some_and(|(kind, _)| *kind == io::ErrorKind::NotFound)
                        {
                            super::texts().gpu_nvidia_tools_missing
                        } else {
                            super::texts().gpu_utilization_unexposed
                        }
                        .into(),
                    );
                }
            } else if let Some(usage) = engines.get(&gpu.id) {
                gpu.source = "DRM fdinfo".into();
                gpu.usage_percent = *usage;
                gpu.status = if usage.is_some() {
                    ObservationStatus::Ready
                } else {
                    ObservationStatus::Warming
                };
                gpu.message = None;
            }
            nvidia.push(gpu);
        }
        nvidia
    }

    fn smi_metrics(&mut self, devices: &[String], now: Instant) -> Option<Vec<GpuMetric>> {
        if self.retry_at.is_some_and(|retry| now < retry) {
            return None;
        }
        let result = (self.smi_runner)(devices).and_then(|output| {
            let metrics = parse_smi(&output);
            if metrics.is_empty() {
                Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "nvidia-smi returned no GPU metrics",
                ))
            } else {
                Ok(metrics)
            }
        });
        match result {
            Ok(metrics) => {
                for metric in &metrics {
                    self.names.insert(metric.id.clone(), metric.name.clone());
                }
                self.retry_at = None;
                self.failure = None;
                Some(metrics)
            }
            Err(error) => {
                let failure = (error.kind(), error.to_string());
                if self.failure.as_ref() != Some(&failure) {
                    tracing::warn!(%error, "NVIDIA metrics fallback unavailable");
                }
                let delay = if error.kind() == io::ErrorKind::NotFound {
                    600
                } else {
                    30
                };
                self.retry_at = Some(now + Duration::from_secs(delay));
                self.failure = Some(failure);
                None
            }
        }
    }
}

fn number(path: &Path) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn pci_matches(id: &str, pci: &str) -> bool {
    id.to_ascii_lowercase().ends_with(&pci.to_ascii_lowercase())
}

fn is_pci_address(value: &str) -> bool {
    let Some((domain, rest)) = value.split_once(':') else {
        return false;
    };
    let Some((bus, slot)) = rest.split_once(':') else {
        return false;
    };
    let Some((device, function)) = slot.split_once('.') else {
        return false;
    };
    [4, 8].contains(&domain.len())
        && bus.len() == 2
        && device.len() == 2
        && function.len() == 1
        && [domain, bus, device, function]
            .iter()
            .all(|part| part.bytes().all(|b| b.is_ascii_hexdigit()))
}

fn parse_smi(output: &str) -> Vec<GpuMetric> {
    fn value(text: &str) -> Option<f32> {
        text.parse::<f32>()
            .ok()
            .filter(|n| n.is_finite() && *n >= 0.0)
    }
    fn bytes(text: &str) -> Option<u64> {
        let bytes = text.parse::<f64>().ok()? * 1024.0 * 1024.0;
        (bytes.is_finite() && bytes >= 0.0 && bytes < u64::MAX as f64).then_some(bytes as u64)
    }
    output
        .lines()
        .filter_map(|line| {
            let fields: Vec<_> = line.split(',').map(str::trim).collect();
            if fields.len() < 7 || !is_pci_address(fields[0]) {
                return None;
            }
            let tail = fields.len() - 5;
            let name = fields[1..tail].join(", ");
            if name.is_empty() {
                return None;
            }
            let usage = value(fields[tail]).map(|usage| usage.min(100.0));
            Some(GpuMetric {
                id: fields[0].to_ascii_lowercase(),
                name,
                vendor: "NVIDIA".into(),
                source: "nvidia-smi".into(),
                usage_percent: usage,
                memory_used_bytes: bytes(fields[tail + 1]),
                memory_total_bytes: bytes(fields[tail + 2]),
                temperature_celsius: value(fields[tail + 3]),
                power_watts: value(fields[tail + 4]),
                status: if usage.is_some() {
                    ObservationStatus::Ready
                } else {
                    ObservationStatus::Unsupported
                },
                message: usage.is_none().then(|| {
                    crate::i18n::texts()
                        .runtime
                        .gpu_utilization_unavailable
                        .into()
                }),
                ..Default::default()
            })
        })
        .collect()
}

fn run_nvidia_smi(devices: &[String]) -> io::Result<String> {
    let executable = crate::platform::command_search_dirs("nvidia-smi")
        .into_iter()
        .map(|dir| dir.join("nvidia-smi"))
        .chain([
            PathBuf::from("/usr/bin/nvidia-smi"),
            PathBuf::from("/usr/lib/wsl/lib/nvidia-smi"),
        ])
        .find(|path| {
            std::fs::metadata(path).is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
        })
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "nvidia-smi executable not found")
        })?;
    let mut command = Command::new(executable);
    command.args([
        "--query-gpu=pci.bus_id,name,utilization.gpu,memory.used,memory.total,temperature.gpu,power.draw",
        "--format=csv,noheader,nounits",
    ]);
    if !devices.is_empty() {
        command.arg(format!("--id={}", devices.join(",")));
    }
    command.env("LC_ALL", "C");
    bounded_output(&mut command, Duration::from_secs(3))
}

/// Poll nonblocking stdout in the owning thread: even inherited pipe writers cannot delay cleanup.
fn bounded_output(command: &mut Command, timeout: Duration) -> io::Result<String> {
    const LIMIT: usize = 64 * 1024;
    command
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(Stdio::piped())
        .process_group(0);
    let start = Instant::now();
    let mut child = command.spawn()?;
    let result = (|| {
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("missing nvidia-smi stdout"))?;
        let fd = stdout.as_raw_fd();
        // The child owns the write end; this changes only our read end's status flags.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut output = Vec::new();
        let mut buffer = [0; 4096];
        let mut closed = false;
        loop {
            if start.elapsed() >= timeout {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "nvidia-smi timed out",
                ));
            }
            if !closed {
                match stdout.read(&mut buffer) {
                    Ok(0) => closed = true,
                    Ok(count) => {
                        if output.len() + count > LIMIT {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "nvidia-smi output exceeds 64 KiB",
                            ));
                        }
                        output.extend_from_slice(&buffer[..count]);
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                        ) => {}
                    Err(error) => return Err(error),
                }
            }
            if let Some(status) = child.try_wait()? {
                if !status.success() {
                    return Err(io::Error::other(format!("nvidia-smi exited with {status}")));
                }
                if closed {
                    return String::from_utf8(output)
                        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error));
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    })();
    crate::platform::terminate_helper_process(&mut child);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_dirs::TempDir;
    use std::sync::{Arc, Mutex};

    const NVIDIA_PCI: &str = "0000:01:00.0";
    const INTEL_PCI: &str = "0000:00:02.0";
    const SMI_ROW: &str = "00000000:01:00.0, NVIDIA RTX 4060, 32, 2844, 8188, 73, 36.74\n";

    fn card(root: &Path, index: u32, pci: &str, vendor: &str, state: &str) -> PathBuf {
        let device = root.join("pci").join(pci);
        std::fs::create_dir_all(device.join("power")).unwrap();
        std::fs::write(device.join("vendor"), vendor).unwrap();
        std::fs::write(device.join("power/runtime_status"), state).unwrap();
        let card = root.join("drm").join(format!("card{index}"));
        std::fs::create_dir_all(&card).unwrap();
        std::os::unix::fs::symlink(&device, card.join("device")).unwrap();
        device
    }

    fn collector(root: &Path, runner: SmiRunner) -> NativeGpuCollector {
        NativeGpuCollector::with_sources(root.join("drm"), root.join("proc"), runner)
    }

    fn no_smi(_: &[String]) -> io::Result<String> {
        panic!("nvidia-smi must not run")
    }

    #[test]
    fn sleeping_cards_do_not_query_smi_or_metrics_and_sort_numerically() {
        let root = TempDir::new("gpu-sleep");
        let intel = card(&root, 10, INTEL_PCI, "0x8086", "suspended\n");
        let nvidia = card(&root, 2, NVIDIA_PCI, "0x10de", "suspended");
        for device in [intel, nvidia] {
            std::fs::write(device.join("gpu_busy_percent"), "77").unwrap();
            std::fs::write(device.join("mem_info_vram_used"), "123").unwrap();
            std::fs::create_dir_all(device.join("hwmon/hwmon0")).unwrap();
            std::fs::write(device.join("hwmon/hwmon0/temp1_input"), "73000").unwrap();
        }
        let metrics = collector(&root, Box::new(no_smi)).sample(Vec::new());
        assert_eq!(metrics.len(), 2);
        assert_eq!(metrics[0].id, NVIDIA_PCI);
        assert_eq!(metrics[1].id, INTEL_PCI);
        for metric in metrics {
            assert_eq!(metric.usage_percent, Some(0.0));
            assert_eq!(metric.status, ObservationStatus::Ready);
            assert_eq!(metric.source, "DRM/sysfs");
            assert_eq!(
                metric.message.as_deref(),
                Some(super::super::texts().gpu_runtime_suspended)
            );
            assert_eq!(metric.memory_used_bytes, None);
            assert_eq!(metric.temperature_celsius, None);
        }
    }

    #[test]
    fn only_active_uncovered_nvidia_cards_are_queried() {
        let root = TempDir::new("gpu-ids");
        card(&root, 1, INTEL_PCI, "0x8086", "suspended");
        card(&root, 2, NVIDIA_PCI, "0x10de", "active");
        card(&root, 3, "0000:02:00.0", "0x10de", "suspended");
        card(&root, 4, "0000:0a:00.0", "0x10de", "active");
        let mut gpu = collector(
            &root,
            Box::new(|devices| {
                assert_eq!(devices, &[NVIDIA_PCI.to_owned()]);
                Ok(SMI_ROW.into())
            }),
        );
        let nvml = GpuMetric {
            id: "00000000:0A:00.0".into(),
            source: "NVML".into(),
            ..Default::default()
        };
        let metrics = gpu.sample(vec![nvml]);
        assert_eq!(metrics.len(), 4);
        assert_eq!(metrics[0].source, "NVML");
        assert_eq!(metrics[2].source, "nvidia-smi");
        assert_eq!(metrics[2].name, "NVIDIA RTX 4060");
    }

    #[test]
    fn parses_smi_units_names_unavailable_values_and_pci_case() {
        let rows = format!("{SMI_ROW}00000000:0A:00.0, NVIDIA, Special Edition, [N/A], [Not Supported], N/A, , [Unknown Error]\n");
        let metrics = parse_smi(&rows);
        assert_eq!(metrics.len(), 2);
        let first = &metrics[0];
        assert_eq!(first.usage_percent, Some(32.0));
        assert_eq!(first.memory_used_bytes, Some(2844 * 1024 * 1024));
        assert_eq!(first.memory_total_bytes, Some(8188 * 1024 * 1024));
        assert_eq!(first.temperature_celsius, Some(73.0));
        assert_eq!(first.power_watts, Some(36.74));
        assert_eq!(first.status, ObservationStatus::Ready);
        let second = &metrics[1];
        assert_eq!(second.id, "00000000:0a:00.0");
        assert!(pci_matches(&second.id, "0000:0A:00.0"));
        assert_eq!(second.name, "NVIDIA, Special Edition");
        assert_eq!(second.usage_percent, None);
        assert_eq!(second.memory_used_bytes, None);
        assert_eq!(second.memory_total_bytes, None);
        assert_eq!(second.temperature_celsius, None);
        assert_eq!(second.power_watts, None);
        assert_eq!(second.status, ObservationStatus::Unsupported);
        assert_eq!(
            second.message.as_deref(),
            Some(crate::i18n::texts().runtime.gpu_utilization_unavailable)
        );
        assert!(parse_smi("").is_empty());
        assert!(parse_smi("garbage\nNo devices were found\na,b,1,2,3,4,5").is_empty());
        let invalid = parse_smi("0000:01:00.0, GPU, NaN, inf, -1, inf, -1");
        assert_eq!(invalid[0].usage_percent, None);
        assert_eq!(invalid[0].memory_used_bytes, None);
        assert_eq!(invalid[0].memory_total_bytes, None);
    }

    #[test]
    fn failures_back_off_and_keep_sysfs_cards() {
        for (kind, delay, missing) in [
            (io::ErrorKind::NotFound, 600, true),
            (io::ErrorKind::TimedOut, 30, false),
        ] {
            let root = TempDir::new("gpu-backoff");
            card(&root, 2, NVIDIA_PCI, "0x10de", "active");
            let calls = Arc::new(Mutex::new(0));
            let count = Arc::clone(&calls);
            let mut gpu = collector(
                &root,
                Box::new(move |_| {
                    *count.lock().unwrap() += 1;
                    Err(io::Error::new(kind, "test error"))
                }),
            );
            let now = Instant::now();
            for elapsed in [0, delay - 1, delay] {
                let metrics = gpu.sample_at(Vec::new(), now + Duration::from_secs(elapsed));
                assert_eq!(metrics.len(), 1);
                assert_eq!(metrics[0].source, "DRM/sysfs");
                assert_eq!(metrics[0].status, ObservationStatus::Unsupported);
                assert_eq!(
                    metrics[0].message.as_deref(),
                    Some(if missing {
                        super::super::texts().gpu_nvidia_tools_missing
                    } else {
                        super::super::texts().gpu_utilization_unexposed
                    })
                );
                assert_eq!(*calls.lock().unwrap(), if elapsed == delay { 2 } else { 1 });
            }
        }
    }

    #[test]
    fn empty_output_backs_off_then_success_clears_failure() {
        let root = TempDir::new("gpu-empty");
        card(&root, 2, NVIDIA_PCI, "0x10de", "active");
        let calls = Arc::new(Mutex::new(0));
        let count = Arc::clone(&calls);
        let mut gpu = collector(
            &root,
            Box::new(move |_| {
                let mut count = count.lock().unwrap();
                *count += 1;
                Ok(if *count == 1 {
                    String::new()
                } else {
                    SMI_ROW.into()
                })
            }),
        );
        let now = Instant::now();
        assert_eq!(
            gpu.sample_at(vec![], now)[0].status,
            ObservationStatus::Unsupported
        );
        gpu.sample_at(vec![], now + Duration::from_secs(29));
        assert_eq!(*calls.lock().unwrap(), 1);
        assert_eq!(
            gpu.sample_at(vec![], now + Duration::from_secs(30))[0].source,
            "nvidia-smi"
        );
        assert!(gpu.failure.is_none());
        gpu.sample_at(vec![], now + Duration::from_secs(31));
        assert_eq!(*calls.lock().unwrap(), 3);
    }

    #[test]
    fn cached_model_survives_runtime_suspend() {
        let root = TempDir::new("gpu-name");
        let device = card(&root, 2, NVIDIA_PCI, "0x10de", "active");
        let calls = Arc::new(Mutex::new(0));
        let count = Arc::clone(&calls);
        let mut gpu = collector(
            &root,
            Box::new(move |_| {
                *count.lock().unwrap() += 1;
                Ok(SMI_ROW.into())
            }),
        );
        assert_eq!(gpu.sample(vec![])[0].name, "NVIDIA RTX 4060");
        std::fs::write(device.join("power/runtime_status"), "suspended").unwrap();
        let metrics = gpu.sample(vec![]);
        assert_eq!(metrics[0].name, "NVIDIA RTX 4060");
        assert_eq!(metrics[0].usage_percent, Some(0.0));
        assert_eq!(*calls.lock().unwrap(), 1);
    }

    #[test]
    fn no_drm_devices_queries_all_gpus_but_nvml_prevents_fallback() {
        let root = TempDir::new("gpu-wsl");
        let mut gpu = collector(
            &root,
            Box::new(|devices| {
                assert!(devices.is_empty());
                Ok(format!(
                    "{SMI_ROW}00000000:02:00.0, Another GPU, 3, 1, 2, 50, 10\n"
                ))
            }),
        );
        assert_eq!(gpu.sample(vec![]).len(), 2);
        let mut gpu = collector(&root, Box::new(no_smi));
        assert_eq!(gpu.sample(vec![GpuMetric::default()]).len(), 1);
    }

    #[test]
    fn active_sysfs_metrics_keep_units_and_missing_usage_warms_fdinfo() {
        let root = TempDir::new("gpu-active");
        let device = card(&root, 1, INTEL_PCI, "0x8086", "active");
        std::fs::create_dir_all(device.join("hwmon/hwmon0")).unwrap();
        std::fs::write(device.join("hwmon/hwmon0/temp1_input"), "42000").unwrap();
        std::fs::write(device.join("hwmon/hwmon0/power1_input"), "12500000").unwrap();
        std::fs::write(device.join("mem_info_vram_used"), "1024").unwrap();
        std::fs::write(device.join("mem_info_vram_total"), "8192").unwrap();
        let mut gpu = collector(&root, Box::new(no_smi));
        let metrics = gpu.sample(vec![]);
        assert_eq!(metrics[0].source, "DRM fdinfo");
        assert_eq!(metrics[0].status, ObservationStatus::Warming);
        assert_eq!(metrics[0].message, None);
        assert_eq!(metrics[0].temperature_celsius, Some(42.0));
        assert_eq!(metrics[0].power_watts, Some(12.5));
        assert_eq!(metrics[0].memory_used_bytes, Some(1024));
        assert_eq!(metrics[0].memory_total_bytes, Some(8192));
        std::fs::write(device.join("gpu_busy_percent"), "47").unwrap();
        let metrics = gpu.sample(vec![]);
        assert_eq!(metrics[0].source, "DRM/sysfs");
        assert_eq!(metrics[0].usage_percent, Some(47.0));
        assert_eq!(metrics[0].status, ObservationStatus::Ready);
    }

    #[test]
    fn default_runner_uses_search_path_locale_and_optional_pci_filter() {
        let root = TempDir::new("gpu-smi-command");
        let executable = root.join("nvidia-smi");
        std::fs::write(
            &executable,
            "#!/bin/sh\nprintf '%s\\n' \"$LC_ALL\" \"$@\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let _search = crate::config::test_dirs::override_search_path(root.as_os_str());
        let output = run_nvidia_smi(&[NVIDIA_PCI.into(), "0000:02:00.0".into()]).unwrap();
        let args: Vec<_> = output.lines().collect();
        assert_eq!(args, [
            "C",
            "--query-gpu=pci.bus_id,name,utilization.gpu,memory.used,memory.total,temperature.gpu,power.draw",
            "--format=csv,noheader,nounits",
            "--id=0000:01:00.0,0000:02:00.0",
        ]);
        let output = run_nvidia_smi(&[]).unwrap();
        assert_eq!(output.lines().count(), 3);
        assert!(!output.contains("--id="));
    }

    #[test]
    fn subprocess_output_is_bounded_and_exit_errors_propagate() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "printf 'gpu output'"]);
        assert_eq!(
            bounded_output(&mut command, Duration::from_secs(3)).unwrap(),
            "gpu output"
        );
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "exit 6"]);
        assert!(bounded_output(&mut command, Duration::from_secs(3)).is_err());
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "head -c 65537 /dev/zero"]);
        assert_eq!(
            bounded_output(&mut command, Duration::from_secs(3))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "sleep 30"]);
        let start = Instant::now();
        assert_eq!(
            bounded_output(&mut command, Duration::from_millis(50))
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
        assert!(start.elapsed() < Duration::from_secs(3));
    }

    #[test]
    #[ignore = "queries this machine's GPU drivers"]
    fn live_native_collector_reads_this_machine() {
        let mut collector = NativeGpuCollector::default();
        for index in 0..2 {
            if index > 0 {
                std::thread::sleep(Duration::from_millis(1100));
            }
            let metrics = collector.sample(Vec::new());
            println!("sample {index}: {metrics:#?}");
            assert!(metrics.iter().any(|gpu| gpu.source == "nvidia-smi"
                || gpu.message.as_deref() == Some(super::super::texts().gpu_runtime_suspended)));
        }
    }
}
