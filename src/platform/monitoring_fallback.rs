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
pub(crate) fn usage_probe_exit(child: &mut std::process::Child) -> std::io::Result<Option<bool>> {
    child
        .try_wait()
        .map(|status| status.map(|status| status.success()))
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
pub(crate) fn usage_statusline_command(agent: &str, passthrough: bool) -> String {
    let args = if passthrough { " --passthrough" } else { "" };
    let otherwise = if passthrough { "cat" } else { ":" };
    format!("(if [ \"${{HERDR_ENV:-}}\" = 1 ] && [ -n \"${{HERDR_BIN_PATH:-}}\" ]; then \"$HERDR_BIN_PATH\" api usage-report --agent {agent}{args}; else {otherwise}; fi)")
}
