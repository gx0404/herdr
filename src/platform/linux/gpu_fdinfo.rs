use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::PathBuf;
use std::time::{Duration, Instant};

const MAX_FD_ENTRIES: usize = 65_536;
const MAX_CLIENTS: usize = 4_096;
const MAX_FDINFO_BYTES: u64 = 16 * 1024;
const MAX_SAMPLE_INTERVAL: Duration = Duration::from_secs(10);

#[derive(Eq, Hash, PartialEq)]
struct ClientKey {
    device: String,
    id: u64,
}

#[derive(Default)]
struct EngineCounters {
    nanoseconds: Option<u64>,
    cycles: Option<u64>,
    total_cycles: Option<u64>,
    capacity: u32,
}

impl EngineCounters {
    fn utilization(&self, previous: &Self, elapsed: Duration) -> Option<f64> {
        let capacity = f64::from(self.capacity.max(1));
        if let (Some(current), Some(previous)) = (self.nanoseconds, previous.nanoseconds) {
            let wall_nanoseconds = elapsed.as_nanos();
            if wall_nanoseconds == 0 {
                return None;
            }
            return Some(
                current.checked_sub(previous)? as f64 / wall_nanoseconds as f64 / capacity,
            );
        }

        let cycles = self.cycles?.checked_sub(previous.cycles?)?;
        let total_cycles = self.total_cycles?.checked_sub(previous.total_cycles?)?;
        if total_cycles == 0 {
            return None;
        }
        Some(cycles as f64 / total_cycles as f64 / capacity)
    }
}

type Engines = HashMap<String, EngineCounters>;
type Clients = HashMap<ClientKey, Engines>;

struct Scan {
    at: Instant,
    clients: Clients,
}

pub(super) struct EngineSampler {
    proc_root: PathBuf,
    previous: Option<Scan>,
}

impl EngineSampler {
    pub(super) fn new(proc_root: PathBuf) -> Self {
        Self {
            proc_root,
            previous: None,
        }
    }

    /// 返回各 PCI 设备的引擎利用率；无有效前次扫描时以本次建立基线。
    pub(super) fn sample(
        &mut self,
        devices: &[String],
        now: Instant,
    ) -> HashMap<String, Option<f32>> {
        let requested: HashSet<_> = devices
            .iter()
            .map(|device| device.to_ascii_lowercase())
            .collect();
        let Ok(clients) = self.scan(&requested) else {
            self.previous = None;
            return devices
                .iter()
                .map(|device| (device.clone(), None))
                .collect();
        };
        let elapsed = self
            .previous
            .as_ref()
            .and_then(|previous| now.checked_duration_since(previous.at))
            .filter(|elapsed| *elapsed <= MAX_SAMPLE_INTERVAL);
        let utilization = self
            .previous
            .as_ref()
            .zip(elapsed)
            .map(|(previous, elapsed)| device_utilization(&clients, &previous.clients, elapsed));
        let result = devices
            .iter()
            .map(|device| {
                let percentage = utilization.as_ref().map(|values| {
                    (values
                        .get(device.to_ascii_lowercase().as_str())
                        .copied()
                        .unwrap_or(0.0)
                        * 100.0)
                        .clamp(0.0, 100.0) as f32
                });
                (device.clone(), percentage)
            })
            .collect();
        self.previous = Some(Scan { at: now, clients });
        result
    }

    fn scan(&self, devices: &HashSet<String>) -> io::Result<Clients> {
        let processes = fs::read_dir(&self.proc_root)?;
        let mut clients = HashMap::new();
        let mut checked_fds = 0;
        let mut contents = String::with_capacity(1024);
        'processes: for process in processes.flatten() {
            let name = process.file_name();
            if name.as_encoded_bytes().is_empty()
                || !name.as_encoded_bytes().iter().all(u8::is_ascii_digit)
            {
                continue;
            }
            let process_path = process.path();
            let Ok(descriptors) = fs::read_dir(process_path.join("fd")) else {
                continue;
            };
            let fdinfo_path = process_path.join("fdinfo");
            for descriptor in descriptors {
                if checked_fds >= MAX_FD_ENTRIES {
                    break 'processes;
                }
                checked_fds += 1;
                let Ok(descriptor) = descriptor else {
                    continue;
                };
                let Ok(target) = fs::read_link(descriptor.path()) else {
                    continue;
                };
                if !target
                    .as_os_str()
                    .as_encoded_bytes()
                    .starts_with(b"/dev/dri/")
                {
                    continue;
                }
                let Ok(file) = File::open(fdinfo_path.join(descriptor.file_name())) else {
                    continue;
                };
                contents.clear();
                if file
                    .take(MAX_FDINFO_BYTES)
                    .read_to_string(&mut contents)
                    .is_err()
                {
                    continue;
                }
                if let Some((key, engines)) = parse_client(&contents, devices) {
                    clients.entry(key).or_insert(engines);
                    if clients.len() >= MAX_CLIENTS {
                        break 'processes;
                    }
                }
            }
        }
        Ok(clients)
    }
}

fn parse_client(contents: &str, devices: &HashSet<String>) -> Option<(ClientKey, Engines)> {
    let mut device = None;
    let mut client_id = None;
    for line in contents.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        match key {
            "drm-pdev" => device = Some(value.trim()),
            "drm-client-id" => client_id = value.trim().parse::<u64>().ok(),
            _ => {}
        }
    }
    let device = device?.to_ascii_lowercase();
    if !devices.contains(&device) {
        return None;
    }
    let key = ClientKey {
        device,
        id: client_id?,
    };
    let mut engines = HashMap::<String, EngineCounters>::new();
    for line in contents.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if let Some(name) = key
            .strip_prefix("drm-engine-capacity-")
            .filter(|name| !name.is_empty())
        {
            if let Ok(capacity) = value.parse::<u32>() {
                engines.entry(name.to_owned()).or_default().capacity = capacity.max(1);
            }
        } else if let Some(name) = key
            .strip_prefix("drm-engine-")
            .filter(|name| !name.is_empty())
        {
            let mut fields = value.split_whitespace();
            if let (Some(nanoseconds), Some("ns"), None) =
                (fields.next(), fields.next(), fields.next())
            {
                if let Ok(nanoseconds) = nanoseconds.parse::<u64>() {
                    engines.entry(name.to_owned()).or_default().nanoseconds = Some(nanoseconds);
                }
            }
        } else if let Some(name) = key
            .strip_prefix("drm-cycles-")
            .filter(|name| !name.is_empty())
        {
            if let Ok(cycles) = value.parse::<u64>() {
                engines.entry(name.to_owned()).or_default().cycles = Some(cycles);
            }
        } else if let Some(name) = key
            .strip_prefix("drm-total-cycles-")
            .filter(|name| !name.is_empty())
        {
            if let Ok(total_cycles) = value.parse::<u64>() {
                engines.entry(name.to_owned()).or_default().total_cycles = Some(total_cycles);
            }
        }
    }
    Some((key, engines))
}

fn device_utilization<'a>(
    clients: &'a Clients,
    previous: &Clients,
    elapsed: Duration,
) -> HashMap<&'a str, f64> {
    let mut engines = HashMap::<(&str, &str), f64>::new();
    for (key, current) in clients {
        // 新建或已消失的客户端没有完整时间窗，不能用累计计数估算本次占用。
        let Some(previous) = previous.get(key) else {
            continue;
        };
        for (name, counters) in current {
            if let Some(utilization) = previous
                .get(name)
                .and_then(|previous| counters.utilization(previous, elapsed))
            {
                *engines
                    .entry((key.device.as_str(), name.as_str()))
                    .or_default() += utilization;
            }
        }
    }
    let mut devices = HashMap::<&str, f64>::new();
    for ((device, _), utilization) in engines {
        let maximum = devices.entry(device).or_default();
        *maximum = maximum.max(utilization);
    }
    devices
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_dirs::{self, TempDir};
    use std::os::unix::fs::symlink;

    const DEVICE: &str = "0000:00:02.0";

    struct ProcFixture {
        _directory: TempDir,
        root: PathBuf,
    }

    impl ProcFixture {
        fn new() -> Self {
            let directory = TempDir::new("gpu-fdinfo");
            let root = directory.join(format!("proc-{}", test_dirs::unique_id()));
            fs::create_dir(&root).unwrap();
            Self {
                _directory: directory,
                root,
            }
        }

        fn write(&self, pid: u32, fd: u32, device: &str, client: u64, engines: &str) {
            self.write_target(pid, fd, "/dev/dri/renderD128", &format!(
                "pos:\t0\nflags:\t02104002\ndrm-driver:\ti915\ndrm-pdev:\t{device}\ndrm-client-id:\t{client}\n{engines}"
            ));
        }

        fn write_target(&self, pid: u32, fd: u32, target: &str, contents: &str) {
            let process = self.root.join(pid.to_string());
            fs::create_dir_all(process.join("fd")).unwrap();
            fs::create_dir_all(process.join("fdinfo")).unwrap();
            let link = process.join("fd").join(fd.to_string());
            if link.symlink_metadata().is_err() {
                symlink(target, link).unwrap();
            }
            fs::write(process.join("fdinfo").join(fd.to_string()), contents).unwrap();
        }

        fn remove(&self, pid: u32, fd: u32) {
            let process = self.root.join(pid.to_string());
            fs::remove_file(process.join("fd").join(fd.to_string())).unwrap();
            fs::remove_file(process.join("fdinfo").join(fd.to_string())).unwrap();
        }

        fn sampler(&self) -> EngineSampler {
            EngineSampler::new(self.root.clone())
        }
    }

    fn devices() -> Vec<String> {
        vec![DEVICE.to_owned()]
    }

    fn assert_percentage(result: &HashMap<String, Option<f32>>, device: &str, expected: f32) {
        let actual = result[device].expect("有效相邻扫描应返回利用率");
        assert!((actual - expected).abs() < 0.001, "{actual} != {expected}");
    }

    #[test]
    fn sampler_can_move_to_a_worker_thread() {
        fn assert_send<T: Send>() {}
        assert_send::<EngineSampler>();
    }

    #[test]
    fn first_scan_is_baseline_then_nanoseconds_measure_elapsed_time() {
        let fixture = ProcFixture::new();
        fixture.write(1, 3, DEVICE, 1, "drm-engine-render:\t100 ns\n");
        let mut sampler = fixture.sampler();
        let now = Instant::now();
        assert_eq!(sampler.sample(&devices(), now)[DEVICE], None);
        fixture.write(1, 3, DEVICE, 1, "drm-engine-render:\t250000100 ns\n");
        assert_percentage(
            &sampler.sample(&devices(), now + Duration::from_secs(1)),
            DEVICE,
            25.0,
        );
    }

    #[test]
    fn shared_clients_are_counted_once_across_processes_and_descriptors() {
        let fixture = ProcFixture::new();
        for (pid, fd) in [(1, 3), (1, 4), (2, 5)] {
            fixture.write(pid, fd, DEVICE, 48, "drm-engine-render:\t0 ns\n");
        }
        let mut sampler = fixture.sampler();
        let now = Instant::now();
        sampler.sample(&devices(), now);
        for (pid, fd) in [(1, 3), (1, 4), (2, 5)] {
            fixture.write(pid, fd, DEVICE, 48, "drm-engine-render:\t250000000 ns\n");
        }
        assert_percentage(
            &sampler.sample(&devices(), now + Duration::from_secs(1)),
            DEVICE,
            25.0,
        );
    }

    #[test]
    fn clients_sum_per_engine_then_device_uses_maximum_with_capacity() {
        let fixture = ProcFixture::new();
        let baseline = "drm-engine-render:\t0 ns\ndrm-engine-copy:\t0 ns\ndrm-engine-video:\t0 ns\ndrm-engine-capacity-video:\t2\n";
        fixture.write(1, 3, DEVICE, 1, baseline);
        fixture.write(2, 4, DEVICE, 2, baseline);
        let mut sampler = fixture.sampler();
        let now = Instant::now();
        sampler.sample(&devices(), now);
        fixture.write(1, 3, DEVICE, 1, "drm-engine-render:\t250000000 ns\ndrm-engine-copy:\t100000000 ns\ndrm-engine-video:\t900000000 ns\ndrm-engine-capacity-video:\t2\n");
        fixture.write(2, 4, DEVICE, 2, "drm-engine-render:\t350000000 ns\ndrm-engine-copy:\t100000000 ns\ndrm-engine-video:\t100000000 ns\ndrm-engine-capacity-video:\t2\n");
        assert_percentage(
            &sampler.sample(&devices(), now + Duration::from_secs(1)),
            DEVICE,
            60.0,
        );
    }

    #[test]
    fn xe_cycles_use_total_cycle_delta_and_engine_capacity() {
        let fixture = ProcFixture::new();
        fixture.write(1, 3, DEVICE, 1, "drm-cycles-render:\t100\ndrm-total-cycles-render:\t1000\ndrm-cycles-video:\t200\ndrm-total-cycles-video:\t2000\ndrm-engine-capacity-video:\t2\n");
        let mut sampler = fixture.sampler();
        let now = Instant::now();
        sampler.sample(&devices(), now);
        fixture.write(1, 3, DEVICE, 1, "drm-cycles-render:\t400\ndrm-total-cycles-render:\t2000\ndrm-cycles-video:\t1000\ndrm-total-cycles-video:\t3000\ndrm-engine-capacity-video:\t2\n");
        assert_percentage(
            &sampler.sample(&devices(), now + Duration::from_secs(1)),
            DEVICE,
            40.0,
        );
    }

    #[test]
    fn arriving_departing_and_reset_clients_do_not_add_utilization() {
        let fixture = ProcFixture::new();
        fixture.write(1, 3, DEVICE, 1, "drm-engine-render:\t100000000 ns\n");
        fixture.write(2, 4, DEVICE, 2, "drm-engine-render:\t100000000 ns\n");
        let mut sampler = fixture.sampler();
        let now = Instant::now();
        sampler.sample(&devices(), now);
        fixture.write(1, 3, DEVICE, 1, "drm-engine-render:\t350000000 ns\n");
        fixture.remove(2, 4);
        fixture.write(3, 5, DEVICE, 3, "drm-engine-render:\t1000000000 ns\n");
        assert_percentage(
            &sampler.sample(&devices(), now + Duration::from_secs(1)),
            DEVICE,
            25.0,
        );
        fixture.write(1, 3, DEVICE, 1, "drm-engine-render:\t1 ns\n");
        fixture.remove(3, 5);
        assert_percentage(
            &sampler.sample(&devices(), now + Duration::from_secs(2)),
            DEVICE,
            0.0,
        );
    }

    #[test]
    fn stale_or_reversed_time_establishes_a_new_baseline() {
        let fixture = ProcFixture::new();
        fixture.write(1, 3, DEVICE, 1, "drm-engine-render:\t0 ns\n");
        let mut sampler = fixture.sampler();
        let now = Instant::now();
        sampler.sample(&devices(), now);
        assert_eq!(
            sampler.sample(&devices(), now + Duration::from_secs(11))[DEVICE],
            None
        );
        fixture.write(1, 3, DEVICE, 1, "drm-engine-render:\t250000000 ns\n");
        assert_percentage(
            &sampler.sample(&devices(), now + Duration::from_secs(12)),
            DEVICE,
            25.0,
        );
        assert_eq!(
            sampler.sample(&devices(), now + Duration::from_secs(10))[DEVICE],
            None
        );
        fixture.write(1, 3, DEVICE, 1, "drm-engine-render:\t500000000 ns\n");
        assert_percentage(
            &sampler.sample(&devices(), now + Duration::from_secs(11)),
            DEVICE,
            25.0,
        );
    }

    #[test]
    fn percentages_are_clamped_and_zero_capacity_defaults_to_one() {
        let fixture = ProcFixture::new();
        fixture.write(
            1,
            3,
            DEVICE,
            1,
            "drm-engine-render:\t0 ns\ndrm-engine-capacity-render:\t0\n",
        );
        let mut sampler = fixture.sampler();
        let now = Instant::now();
        sampler.sample(&devices(), now);
        fixture.write(
            1,
            3,
            DEVICE,
            1,
            "drm-engine-render:\t250000000 ns\ndrm-engine-capacity-render:\t0\n",
        );
        assert_percentage(
            &sampler.sample(&devices(), now + Duration::from_secs(1)),
            DEVICE,
            25.0,
        );
        fixture.write(1, 3, DEVICE, 1, "drm-engine-render:\t2250000000 ns\n");
        assert_percentage(
            &sampler.sample(&devices(), now + Duration::from_secs(2)),
            DEVICE,
            100.0,
        );
    }

    #[test]
    fn empty_devices_and_devices_without_clients_have_zero_utilization() {
        let fixture = ProcFixture::new();
        let mut sampler = fixture.sampler();
        let now = Instant::now();
        assert!(sampler.sample(&[], now).is_empty());
        assert_percentage(
            &sampler.sample(&devices(), now + Duration::from_secs(1)),
            DEVICE,
            0.0,
        );
    }

    #[test]
    fn pci_addresses_are_case_insensitive_and_other_devices_are_ignored() {
        let fixture = ProcFixture::new();
        let requested = vec!["0000:0A:0B.C".to_owned(), DEVICE.to_owned()];
        fixture.write(1, 3, "0000:0a:0b.c", 1, "drm-engine-render:\t0 ns\n");
        fixture.write(1, 4, DEVICE, 1, "drm-engine-render:\t0 ns\n");
        fixture.write(2, 4, "0000:01:00.0", 2, "drm-engine-render:\t0 ns\n");
        let mut sampler = fixture.sampler();
        let now = Instant::now();
        sampler.sample(&requested, now);
        fixture.write(
            1,
            3,
            "0000:0a:0b.c",
            1,
            "drm-engine-render:\t250000000 ns\n",
        );
        fixture.write(1, 4, DEVICE, 1, "drm-engine-render:\t125000000 ns\n");
        fixture.write(
            2,
            4,
            "0000:01:00.0",
            2,
            "drm-engine-render:\t1000000000 ns\n",
        );
        let result = sampler.sample(&requested, now + Duration::from_secs(1));
        assert_percentage(&result, "0000:0A:0B.C", 25.0);
        assert_percentage(&result, DEVICE, 12.5);
        let clients = &sampler.previous.as_ref().unwrap().clients;
        assert_eq!(clients.len(), 2);
        assert!(clients
            .keys()
            .all(|key| key.device == "0000:0a:0b.c" || key.device == DEVICE));
    }

    #[test]
    fn non_drm_links_and_unreadable_fdinfo_are_ignored() {
        let fixture = ProcFixture::new();
        let contents =
            format!("drm-pdev:\t{DEVICE}\ndrm-client-id:\t1\ndrm-engine-render:\t0 ns\n");
        fixture.write_target(1, 3, "/dev/null", &contents);
        fixture.write_target(1, 4, "/dev/dri-other/renderD128", &contents);
        fixture.write_target(1, 5, "/dev/dri/renderD128", &contents);
        let unreadable = fixture.root.join("1/fdinfo/5");
        fs::remove_file(&unreadable).unwrap();
        // 目录内容无法作为 fdinfo 读取，且不依赖运行测试的用户是否为 root。
        fs::create_dir(unreadable).unwrap();
        let mut sampler = fixture.sampler();
        let now = Instant::now();
        sampler.sample(&devices(), now);
        fixture.write_target(
            1,
            3,
            "/dev/null",
            &contents.replace("0 ns", "1000000000 ns"),
        );
        fixture.write_target(
            1,
            4,
            "/dev/dri-other/renderD128",
            &contents.replace("0 ns", "1000000000 ns"),
        );
        assert_percentage(
            &sampler.sample(&devices(), now + Duration::from_secs(1)),
            DEVICE,
            0.0,
        );
    }

    #[test]
    fn missing_proc_root_returns_none_and_clears_previous_scan() {
        let fixture = ProcFixture::new();
        let mut sampler = fixture.sampler();
        let now = Instant::now();
        sampler.sample(&devices(), now);
        fs::remove_dir(&fixture.root).unwrap();
        assert_eq!(
            sampler.sample(&devices(), now + Duration::from_secs(1))[DEVICE],
            None
        );
        assert!(sampler.previous.is_none());
        fs::create_dir(&fixture.root).unwrap();
        assert_eq!(
            sampler.sample(&devices(), now + Duration::from_secs(2))[DEVICE],
            None
        );
    }

    #[test]
    fn unavailable_or_decreasing_cycle_counters_are_skipped() {
        let fixture = ProcFixture::new();
        fixture.write(1, 3, DEVICE, 1, "drm-cycles-render:\t100\ndrm-total-cycles-render:\t1000\ndrm-cycles-copy:\t100\ndrm-total-cycles-copy:\t1000\ndrm-cycles-video:\t100\ndrm-total-cycles-video:\t1000\n");
        let mut sampler = fixture.sampler();
        let now = Instant::now();
        sampler.sample(&devices(), now);
        fixture.write(1, 3, DEVICE, 1, "drm-cycles-render:\t200\ndrm-total-cycles-render:\t1000\ndrm-cycles-copy:\t1\ndrm-total-cycles-copy:\t2000\ndrm-cycles-video:\t200\ndrm-total-cycles-video:\t1\n");
        assert_percentage(
            &sampler.sample(&devices(), now + Duration::from_secs(1)),
            DEVICE,
            0.0,
        );
    }

    #[test]
    fn malformed_counter_lines_are_ignored_and_fdinfo_reads_are_bounded() {
        let fixture = ProcFixture::new();
        let mut contents = format!("drm-pdev:\t{DEVICE}\ndrm-client-id:\t1\ndrm-engine-render:\t0 seconds\ndrm-engine-copy:\tinvalid ns\ndrm-cycles-video:\tinvalid\n");
        contents.push_str(&" ".repeat(MAX_FDINFO_BYTES as usize));
        contents.push_str("\ndrm-engine-render:\t1000000000 ns\n");
        fixture.write_target(1, 3, "/dev/dri/renderD128", &contents);
        let mut sampler = fixture.sampler();
        let now = Instant::now();
        sampler.sample(&devices(), now);
        assert_percentage(
            &sampler.sample(&devices(), now + Duration::from_secs(1)),
            DEVICE,
            0.0,
        );
        assert!(sampler
            .previous
            .as_ref()
            .unwrap()
            .clients
            .values()
            .all(HashMap::is_empty));
    }

    #[test]
    #[ignore = "读取本机 /proc，需手动运行"]
    fn live_fdinfo_sampler_reads_this_machine() {
        let mut sampler = EngineSampler::new("/proc".into());
        let requested = devices();
        let first_at = Instant::now();
        let first = sampler.sample(&requested, first_at);
        let first_elapsed = first_at.elapsed();
        std::thread::sleep(Duration::from_secs(1));
        let second_at = Instant::now();
        let second = sampler.sample(&requested, second_at);
        let second_elapsed = second_at.elapsed();
        println!("fdinfo first={first:?}, elapsed={first_elapsed:?}; second={second:?}, elapsed={second_elapsed:?}");
        let percentage = second[DEVICE].expect("第二次扫描应有利用率");
        assert!((0.0..=100.0).contains(&percentage));
    }
}
