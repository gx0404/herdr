use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;

use crate::api::schema::{GpuMetric, ObservationStatus};

#[derive(Default)]
pub(crate) struct NativeGpuCollector;

fn number(path: &Path) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

impl NativeGpuCollector {
    pub(crate) fn sample(&mut self, nvidia: Vec<GpuMetric>) -> Vec<GpuMetric> {
        let mut result = nvidia;
        let Ok(cards) = std::fs::read_dir("/sys/class/drm") else {
            return result;
        };
        for card in cards.flatten() {
            let name = card.file_name().to_string_lossy().into_owned();
            if !name
                .strip_prefix("card")
                .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
            {
                continue;
            }
            let device = card.path().join("device");
            let vendor = std::fs::read_to_string(device.join("vendor")).unwrap_or_default();
            let vendor = match vendor.trim() {
                "0x10de" => "NVIDIA",
                "0x1002" => "AMD",
                "0x8086" => "Intel",
                _ => "GPU",
            };
            let pci = std::fs::canonicalize(&device)
                .ok()
                .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()))
                .unwrap_or_else(|| name.clone());
            if result.iter().any(|gpu| gpu.id.ends_with(&pci)) {
                continue;
            }
            let usage = number(&device.join("gpu_busy_percent")).map(|value| value.min(100) as f32);
            let mut gpu = GpuMetric {
                id: pci.clone(),
                name: format!("{vendor} {name} ({pci})"),
                vendor: vendor.into(),
                source: "DRM/sysfs".into(),
                usage_percent: usage,
                memory_used_bytes: number(&device.join("mem_info_vram_used")),
                memory_total_bytes: number(&device.join("mem_info_vram_total")),
                status: if usage.is_some() {
                    ObservationStatus::Ready
                } else {
                    ObservationStatus::Unsupported
                },
                ..Default::default()
            };
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
            if gpu.usage_percent.is_none() {
                gpu.message = Some("驱动未公开可读取的全卡利用率；可用传感器仍正常显示".into());
            }
            result.push(gpu);
        }
        result
    }
}

pub(crate) fn monitor_environment() -> String {
    let release = std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .unwrap_or_default()
        .to_lowercase();
    if release.contains("microsoft") {
        "WSL（当前 Linux 环境）".into()
    } else if Path::new("/.dockerenv").exists() || Path::new("/run/.containerenv").exists() {
        "容器（当前可见资源）".into()
    } else {
        "Linux 主机".into()
    }
}

pub(crate) fn configure_usage_probe_command(command: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

pub(crate) fn terminate_usage_probe(child: &mut std::process::Child) {
    if child.id() > 1 {
        unsafe {
            libc::kill(-(child.id() as i32), libc::SIGKILL);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

pub(crate) struct UsageProbeGuard;
impl UsageProbeGuard {
    pub(crate) fn new(_child: &std::process::Child) -> io::Result<Self> {
        Ok(Self)
    }
    pub(crate) fn terminate(&mut self) {}
}

/// 非阻塞地观察探测子进程是否结束；结束时给出退出码或终止信号（`CLD_KILLED` /
/// `CLD_DUMPED` 的 `si_status` 是信号编号）。
pub(crate) fn usage_probe_exit(
    child: &mut std::process::Child,
) -> io::Result<Option<crate::platform::UsageProbeExit>> {
    // 保留组长的僵尸记录，清理进程组前不能回收并复用其 PID。
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    if unsafe {
        libc::waitid(
            libc::P_PID,
            child.id(),
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    if unsafe { info.si_pid() } == 0 {
        return Ok(None);
    }
    let status = unsafe { info.si_status() };
    Ok(Some(if info.si_code == libc::CLD_EXITED {
        crate::platform::UsageProbeExit {
            code: Some(status),
            signal: None,
        }
    } else {
        crate::platform::UsageProbeExit {
            code: None,
            signal: Some(status),
        }
    }))
}

pub(crate) fn terminate_usage_pty(child: &mut dyn portable_pty::Child) {
    if let Some(pid) = child.process_id().filter(|pid| *pid > 1) {
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

pub(crate) fn process_instance_token(pid: u32) -> io::Result<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let tail = stat
        .rsplit_once(')')
        .ok_or_else(|| io::Error::other("进程状态格式无效"))?
        .1;
    let started = tail
        .split_whitespace()
        .nth(19)
        .ok_or_else(|| io::Error::other("进程启动标识缺失"))?;
    if !started.bytes().all(|b| b.is_ascii_digit()) {
        return Err(io::Error::other("进程启动标识无效"));
    }
    Ok(started.into())
}

pub(crate) struct MonitoredProcess {
    handle: OwnedFd,
    pub(crate) instance_token: String,
    pub(crate) name: String,
}

impl MonitoredProcess {
    pub(crate) fn open(pid: u32) -> io::Result<Self> {
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0_u32) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let handle = unsafe { OwnedFd::from_raw_fd(fd as i32) };
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
        let (head, tail) = stat
            .rsplit_once(')')
            .ok_or_else(|| io::Error::other("进程状态无效"))?;
        let name = head
            .split_once('(')
            .ok_or_else(|| io::Error::other("进程名称缺失"))?
            .1
            .to_owned();
        let instance_token = tail
            .split_whitespace()
            .nth(19)
            .ok_or_else(|| io::Error::other("进程实例缺失"))?
            .to_owned();
        let mut descriptor = libc::pollfd {
            fd: handle.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        if unsafe { libc::poll(&mut descriptor, 1, 0) } != 0 {
            return Err(io::Error::other("进程已退出"));
        }
        Ok(Self {
            handle,
            instance_token,
            name,
        })
    }

    pub(crate) fn terminate(&self, force: bool) -> io::Result<()> {
        let signal = if force { libc::SIGKILL } else { libc::SIGTERM };
        let result = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.handle.as_raw_fd(),
                signal,
                std::ptr::null::<libc::siginfo_t>(),
                0_u32,
            )
        };
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
pub(crate) fn terminate_monitored_process(pid: u32, expected: &str, force: bool) -> io::Result<()> {
    if pid <= 1 || pid == std::process::id() {
        return Err(io::Error::other("受保护的进程"));
    }
    // pidfd 固定进程实例，避免校验与发送信号之间 PID 被复用。
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0_u32) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let handle = unsafe { OwnedFd::from_raw_fd(fd as i32) };
    if process_instance_token(pid)? != expected {
        return Err(io::Error::other("进程身份已经变化"));
    }
    let signal = if force { libc::SIGKILL } else { libc::SIGTERM };
    let result = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            handle.as_raw_fd(),
            signal,
            std::ptr::null::<libc::siginfo_t>(),
            0_u32,
        )
    };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

pub(crate) fn usage_probe_needs_job_helper() -> bool {
    false
}

pub(crate) fn monitor_cpu_inventory() -> Option<String> {
    std::fs::read_to_string("/sys/devices/system/cpu/online").ok()
}

pub(crate) fn usage_statusline_command(agent: &str, passthrough: bool) -> String {
    let args = if passthrough { " --passthrough" } else { "" };
    let otherwise = if passthrough { "cat" } else { ":" };
    format!("(if [ \"${{HERDR_ENV:-}}\" = 1 ] && [ -n \"${{HERDR_BIN_PATH:-}}\" ]; then \"$HERDR_BIN_PATH\" api usage-report --agent {agent}{args}; else {otherwise}; fi)")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait_probe_exit(child: &mut std::process::Child) -> crate::platform::UsageProbeExit {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if let Some(exit) = usage_probe_exit(child).unwrap() {
                let _ = child.wait();
                return exit;
            }
            assert!(std::time::Instant::now() < deadline, "子进程未在期限内退出");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn usage_probe_exit_distinguishes_exit_codes_from_signals() {
        let mut child = std::process::Command::new("sh")
            .args(["-c", "exit 3"])
            .spawn()
            .unwrap();
        assert_eq!(
            wait_probe_exit(&mut child),
            crate::platform::UsageProbeExit {
                code: Some(3),
                signal: None
            }
        );
        let mut child = std::process::Command::new("sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let exit = wait_probe_exit(&mut child);
        assert!(exit.success());
        let mut child = std::process::Command::new("sh")
            .args(["-c", "kill -9 $$"])
            .spawn()
            .unwrap();
        let exit = wait_probe_exit(&mut child);
        assert_eq!(
            exit,
            crate::platform::UsageProbeExit {
                code: None,
                signal: Some(libc::SIGKILL)
            }
        );
        assert!(!exit.success(), "被信号终止不算成功");
        // 未退出的子进程观察为 None，且不回收其记录。
        let mut child = std::process::Command::new("sh")
            .args(["-c", "sleep 5"])
            .spawn()
            .unwrap();
        assert_eq!(usage_probe_exit(&mut child).unwrap(), None);
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn current_process_identity_is_stable_and_cannot_be_terminated() {
        let pid = std::process::id();
        let token = process_instance_token(pid).unwrap();
        assert_eq!(process_instance_token(pid).unwrap(), token);
        assert!(terminate_monitored_process(pid, &token, true).is_err());
    }
}
