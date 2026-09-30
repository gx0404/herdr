//! 驱动查询独立于 CPU 采样；最多保留一个待处理请求。

use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use crate::api::schema::{GpuMetric, ObservationStatus};

/// RS-20：最后一次指标请求之后多久释放 NVML（见 worker 循环）。
const GPU_NVML_IDLE_TIMEOUT: Duration = Duration::from_secs(5);
/// GPU 利用率的最小采样间隔：驱动查询（Windows 的 `GPU Engine` 计数器、NVML）不便宜，
/// 监控页最快 500 ms 一拍的请求在间隔内直接复用上一份结果。
const GPU_MIN_SAMPLE_INTERVAL: Duration = Duration::from_secs(2);

/// 距上次采样不足最小间隔时跳过这次请求。
fn gpu_sample_due(last_sampled: Option<Instant>, now: Instant) -> bool {
    last_sampled.is_none_or(|at| now.saturating_duration_since(at) >= GPU_MIN_SAMPLE_INTERVAL)
}

pub(super) struct GpuWorker {
    request: mpsc::SyncSender<()>,
    latest: Arc<Mutex<(Option<Instant>, Vec<GpuMetric>)>>,
}

impl GpuWorker {
    pub fn start() -> std::io::Result<Self> {
        let (request, requests) = mpsc::sync_channel(1);
        let latest = Arc::new(Mutex::new((None, Vec::new())));
        let output = latest.clone();
        std::thread::Builder::new()
            .name("herdr-gpu-metrics".into())
            .spawn(move || {
                let mut nvml = None;
                let mut retry_at = Instant::now();
                let mut native: Option<crate::platform::NativeGpuCollector> = None;
                let mut last_sampled = None;
                loop {
                    // RS-20：空闲就释放 NVML——初始化后它常驻 7 个设备 fd 与上百 MB
                    // 驱动映射，而指标只有打开监控页时才会被请求。下一次请求重新 init。
                    // NVML 已释放时没有要计时的事，直接阻塞等下一次请求。
                    let request = if nvml.is_some() {
                        requests.recv_timeout(GPU_NVML_IDLE_TIMEOUT)
                    } else {
                        requests
                            .recv()
                            .map_err(|_| mpsc::RecvTimeoutError::Disconnected)
                    };
                    match request {
                        Ok(()) => {}
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            let released = nvml.take().is_some();
                            if released {
                                // 复审轻级 3：释放的同时复位 30 s 失败节流——否则
                                // 「初始化失败 → 释放 → 用户重开监控页」会落在节流
                                // 窗口里拿到空表。空闲已经过去了，重试一次是便宜的。
                                retry_at = Instant::now();
                            }
                            continue;
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                    if !gpu_sample_due(last_sampled, Instant::now()) {
                        continue;
                    }
                    if nvml.is_none() && Instant::now() >= retry_at {
                        nvml = nvml_wrapper::Nvml::init().ok();
                        retry_at = Instant::now() + Duration::from_secs(30);
                    }
                    let devices = native
                        .get_or_insert_with(Default::default)
                        .sample(nvml.as_ref().map(nvidia_metrics).unwrap_or_default());
                    let sampled_at = Instant::now();
                    last_sampled = Some(sampled_at);
                    if let Ok(mut state) = output.lock() {
                        *state = (Some(sampled_at), devices);
                    }
                }
            })?;
        Ok(Self { request, latest })
    }

    pub fn request_and_read(&self) -> (u64, Vec<GpuMetric>) {
        let _ = self.request.try_send(());
        let Ok(state) = self.latest.lock() else {
            return (0, Vec::new());
        };
        let mut values = state.1.clone();
        if state
            .0
            .is_some_and(|at| at.elapsed() > Duration::from_secs(15))
        {
            for gpu in &mut values {
                gpu.status = ObservationStatus::Stale;
                gpu.message = Some(crate::i18n::texts().runtime.gpu_driver_timeout.into());
            }
        }
        (
            state.0.map_or(0, |at| {
                super::now_ms()
                    .saturating_sub(at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64)
            }),
            values,
        )
    }
}

fn nvidia_metrics(nvml: &nvml_wrapper::Nvml) -> Vec<GpuMetric> {
    let mut metrics = Vec::new();
    let Ok(count) = nvml.device_count() else {
        return metrics;
    };
    for index in 0..count.min(128) {
        let Ok(device) = nvml.device_by_index(index) else {
            continue;
        };
        let memory = device.memory_info().ok();
        let utilization = device.utilization_rates().ok();
        let id = device
            .pci_info()
            .map(|pci| pci.bus_id.to_lowercase())
            .or_else(|_| device.uuid())
            .unwrap_or_else(|_| format!("nvidia-{index}"));
        metrics.push(GpuMetric {
            id,
            name: device
                .name()
                .unwrap_or_else(|_| format!("NVIDIA GPU {index}")),
            vendor: "NVIDIA".into(),
            source: "NVML".into(),
            status: if utilization.is_some() {
                ObservationStatus::Ready
            } else {
                ObservationStatus::Unsupported
            },
            usage_percent: utilization.as_ref().map(|value| value.gpu.min(100) as f32),
            memory_used_bytes: memory.as_ref().map(|value| value.used),
            memory_total_bytes: memory.as_ref().map(|value| value.total),
            temperature_celsius: device
                .temperature(nvml_wrapper::enum_wrappers::device::TemperatureSensor::Gpu)
                .ok()
                .map(|value| value as f32),
            power_watts: device.power_usage().ok().map(|value| value as f32 / 1000.0),
            message: utilization.is_none().then(|| {
                crate::i18n::texts()
                    .runtime
                    .gpu_utilization_unavailable
                    .into()
            }),
            ..Default::default()
        });
    }
    metrics
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 监控页最快 500 ms 一拍：GPU 驱动查询最多每 2 秒一次，其间复用上一份结果。
    #[test]
    fn gpu_sampling_is_limited_to_the_minimum_interval() {
        let now = Instant::now();
        assert!(gpu_sample_due(None, now));
        assert!(!gpu_sample_due(Some(now), now + Duration::from_millis(500)));
        assert!(!gpu_sample_due(
            Some(now),
            now + GPU_MIN_SAMPLE_INTERVAL - Duration::from_millis(1)
        ));
        assert!(gpu_sample_due(Some(now), now + GPU_MIN_SAMPLE_INTERVAL));
    }

    /// 文档终审 D7：驱动超过 15 秒没有应答时的说明按界面语言给出：英文界面不含 CJK，
    /// 中文界面是中文。
    #[test]
    fn stale_gpu_note_follows_the_interface_language() {
        use crate::i18n::{has_cjk, lang_guard, texts, Lang};
        let (request, _requests) = mpsc::sync_channel(1);
        let stale_at = Instant::now().checked_sub(Duration::from_secs(20));
        assert!(stale_at.is_some(), "用例前提：单调时钟已走过 20 秒");
        let worker = GpuWorker {
            request,
            latest: Arc::new(Mutex::new((stale_at, vec![GpuMetric::default()]))),
        };
        for (lang, chinese) in [(Lang::En, false), (Lang::ZhCn, true)] {
            let _guard = lang_guard(lang);
            let (_, values) = worker.request_and_read();
            assert_eq!(values[0].status, ObservationStatus::Stale);
            let message = values[0].message.clone().unwrap_or_default();
            assert_eq!(message, texts().runtime.gpu_driver_timeout);
            assert_eq!(has_cjk(&message), chinese, "{lang:?}: {message}");
        }
    }
}
