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

/// 短命辅助子进程（如 codex `--help` 探测）的启动配置；unix 平台由调用方自建进程组。
#[cfg(not(unix))]
pub(crate) fn configure_helper_process_command(command: &mut std::process::Command) {
    crate::platform::configure_background_command(command);
}

/// 辅助子进程的监管：回退平台没有可托管的进程容器，守卫本身不持有资源。
pub(crate) struct HelperProcessGuard;
impl HelperProcessGuard {
    pub(crate) fn new(_child: &std::process::Child) -> std::io::Result<Self> {
        Ok(Self)
    }
    pub(crate) fn terminate(&mut self) {}
}

pub(crate) fn terminate_helper_process(child: &mut std::process::Child) {
    if child.try_wait().ok().flatten().is_some() {
        return;
    }
    let _ = child.kill();
    let _ = child.wait();
}

pub(crate) fn process_instance_token(_pid: u32) -> std::io::Result<String> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        crate::i18n::texts().platform.process_handle_unsupported,
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
            crate::i18n::texts().platform.process_handle_unsupported,
        ))
    }
    pub(crate) fn terminate(&self, _force: bool) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            crate::i18n::texts().platform.process_handle_unsupported,
        ))
    }
}

pub(crate) fn monitor_cpu_inventory() -> Option<String> {
    std::thread::available_parallelism()
        .ok()
        .map(|count| count.to_string())
}

#[cfg(all(test, unix))]
pub(crate) use crate::platform::unix_common::marked_legacy_statusline_wrapper;
/// unix 回退平台（macOS 等）的旧 statusline 包装与 Linux 共用 POSIX sh 剥离实现；其它平台
/// 从未生成过包装，只做保守判断：带 herdr 包装特征的命令报可识别错误，调用方原样保留。
#[cfg(unix)]
pub(crate) use crate::platform::unix_common::unwrap_legacy_statusline;

#[cfg(not(unix))]
pub(crate) fn unwrap_legacy_statusline(
    _agent: &str,
    command: &str,
) -> std::io::Result<Option<String>> {
    if crate::platform::looks_like_legacy_statusline_wrapper(command) {
        return Err(crate::platform::unrecognized_legacy_statusline_error());
    }
    Ok(None)
}
