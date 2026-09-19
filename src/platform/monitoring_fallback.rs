use crate::api::schema::GpuMetric;

#[derive(Default)]
pub(crate) struct NativeGpuCollector;

impl NativeGpuCollector {
    pub(crate) fn sample(&mut self, nvidia: Vec<GpuMetric>) -> Vec<GpuMetric> {
        nvidia
    }
}

pub(crate) fn monitor_environment() -> String {
    std::env::consts::OS.into()
}

pub(crate) fn configure_usage_probe_command(command: &mut std::process::Command) {
    crate::platform::configure_background_command(command);
}

pub(crate) struct UsageProbeGuard;
impl UsageProbeGuard {
    pub(crate) fn new(_child: &std::process::Child) -> std::io::Result<Self> {
        Ok(Self)
    }
    pub(crate) fn terminate(&mut self) {}
}
pub(crate) fn usage_probe_exit(
    child: &mut std::process::Child,
) -> std::io::Result<Option<crate::platform::UsageProbeExit>> {
    child
        .try_wait()
        .map(|status| status.map(crate::platform::UsageProbeExit::from_status))
}

pub(crate) fn terminate_usage_probe(child: &mut std::process::Child) {
    if child.try_wait().ok().flatten().is_some() {
        return;
    }
    let _ = child.kill();
    let _ = child.wait();
}

pub(crate) fn terminate_usage_pty(child: &mut dyn portable_pty::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

pub(crate) fn process_instance_token(_pid: u32) -> std::io::Result<String> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "本平台尚未提供安全进程实例句柄",
    ))
}

pub(crate) struct MonitoredProcess {
    pub(crate) instance_token: String,
    pub(crate) name: String,
}
impl MonitoredProcess {
    pub(crate) fn open(_pid: u32) -> std::io::Result<Self> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "此平台尚无稳定进程句柄",
        ))
    }
    pub(crate) fn terminate(&self, _force: bool) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "此平台尚无稳定进程句柄",
        ))
    }
}

pub(crate) fn terminate_monitored_process(
    _pid: u32,
    _expected: &str,
    _force: bool,
) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "本平台尚未提供安全进程终止接口",
    ))
}

pub(crate) fn usage_probe_needs_job_helper() -> bool {
    false
}

pub(crate) fn monitor_cpu_inventory() -> Option<String> {
    std::thread::available_parallelism()
        .ok()
        .map(|count| count.to_string())
}

/// unix 回退平台（macOS 等）的 statusline 包装与 Linux 共用 POSIX sh 实现；其它平台尚无
/// 可靠的 shell 形态，拼装返回 `Unsupported`，剥离只做保守判断。
#[cfg(unix)]
pub(crate) use crate::platform::unix_common::{strip_usage_statusline, usage_statusline_pipeline};

#[cfg(not(unix))]
pub(crate) fn usage_statusline_pipeline(_agent: &str, _original: &str) -> std::io::Result<String> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "本平台尚未提供 statusline 用量回调的包装形态",
    ))
}

#[cfg(not(unix))]
pub(crate) fn strip_usage_statusline(
    _agent: &str,
    command: &str,
) -> std::io::Result<Option<String>> {
    if crate::platform::looks_like_usage_wrapper(command) {
        return Err(crate::platform::unrecognized_usage_statusline_error());
    }
    Ok(None)
}
