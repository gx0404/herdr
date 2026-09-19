//! Windows 原生 GPU 计数器和持有进程句柄的安全终止。

use std::collections::HashMap;
use std::io;
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};
use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, HANDLE};
use windows_sys::Win32::System::Performance::*;
use windows_sys::Win32::System::Threading::QueryFullProcessImageNameW;
use windows_sys::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, TerminateProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_TERMINATE,
};

use crate::api::schema::{GpuMetric, ObservationStatus};

pub(crate) struct NativeGpuCollector {
    query: PDH_HQUERY,
    engines: PDH_HCOUNTER,
    dedicated: PDH_HCOUNTER,
    shared: PDH_HCOUNTER,
    sampled: bool,
}

impl Default for NativeGpuCollector {
    fn default() -> Self {
        let mut value = Self {
            query: std::ptr::null_mut(),
            engines: std::ptr::null_mut(),
            dedicated: std::ptr::null_mut(),
            shared: std::ptr::null_mut(),
            sampled: false,
        };
        if unsafe { PdhOpenQueryW(std::ptr::null(), 0, &mut value.query) } == 0 {
            value.engines = add_counter(value.query, "\\GPU Engine(*)\\Utilization Percentage");
            value.dedicated = add_counter(value.query, "\\GPU Adapter Memory(*)\\Dedicated Usage");
            value.shared = add_counter(value.query, "\\GPU Adapter Memory(*)\\Shared Usage");
        }
        value
    }
}

impl Drop for NativeGpuCollector {
    fn drop(&mut self) {
        if !self.query.is_null() {
            unsafe {
                PdhCloseQuery(self.query);
            }
        }
    }
}

fn add_counter(query: PDH_HQUERY, path: &str) -> PDH_HCOUNTER {
    let wide = path.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
    let mut counter = std::ptr::null_mut();
    // 使用英语路径 API，避免中文和其他语言 Windows 的计数器名称差异。
    if unsafe { PdhAddEnglishCounterW(query, wide.as_ptr(), 0, &mut counter) } == 0 {
        counter
    } else {
        std::ptr::null_mut()
    }
}

fn counters(counter: PDH_HCOUNTER) -> Vec<(String, f64)> {
    if counter.is_null() {
        return Vec::new();
    }
    let mut bytes = 0;
    let mut count = 0;
    let status = unsafe {
        PdhGetFormattedCounterArrayW(
            counter,
            PDH_FMT_DOUBLE,
            &mut bytes,
            &mut count,
            std::ptr::null_mut(),
        )
    };
    if status != PDH_MORE_DATA || bytes == 0 || bytes > 4 * 1024 * 1024 {
        return Vec::new();
    }
    // usize 提供 PDH 结构体需要的对齐，不能将未对齐的 Vec<u8> 强制转换。
    let mut storage = vec![0_usize; (bytes as usize).div_ceil(std::mem::size_of::<usize>())];
    let ptr = storage.as_mut_ptr().cast::<PDH_FMT_COUNTERVALUE_ITEM_W>();
    if unsafe { PdhGetFormattedCounterArrayW(counter, PDH_FMT_DOUBLE, &mut bytes, &mut count, ptr) }
        != 0
    {
        return Vec::new();
    }
    if count as usize > bytes as usize / std::mem::size_of::<PDH_FMT_COUNTERVALUE_ITEM_W>() {
        return Vec::new();
    }
    let base = storage.as_ptr() as usize;
    let end = base + storage.len() * std::mem::size_of::<usize>();
    let mut result = Vec::new();
    for item in unsafe { std::slice::from_raw_parts(ptr, count as usize) } {
        let status = item.FmtValue.CStatus;
        if status != PDH_CSTATUS_VALID_DATA && status != PDH_CSTATUS_NEW_DATA {
            continue;
        }
        let address = item.szName as usize;
        if address < base || address >= end || !address.is_multiple_of(2) {
            continue;
        }
        let name =
            unsafe { std::slice::from_raw_parts(item.szName, ((end - address) / 2).min(2048)) };
        let Some(length) = name.iter().position(|c| *c == 0) else {
            continue;
        };
        let value = unsafe { item.FmtValue.Anonymous.doubleValue };
        if value.is_finite() && value >= 0.0 {
            result.push((String::from_utf16_lossy(&name[..length]), value));
        }
    }
    result
}

fn adapter_usage(values: &[(String, f64)], luid: &str) -> Option<f32> {
    let mut engines: HashMap<&str, f64> = HashMap::new();
    for (name, value) in values {
        if !name.contains(luid) {
            continue;
        }
        let Some((_, engine)) = name.split_once("_eng_") else {
            continue;
        };
        *engines.entry(engine).or_default() += value;
    }
    engines
        .values()
        .copied()
        .reduce(f64::max)
        .map(|v| v.clamp(0.0, 100.0) as f32)
}

impl NativeGpuCollector {
    pub(crate) fn sample(&mut self, nvidia: Vec<GpuMetric>) -> Vec<GpuMetric> {
        let sampled = !self.query.is_null() && unsafe { PdhCollectQueryData(self.query) } == 0;
        let engines = if sampled && self.sampled {
            counters(self.engines)
        } else {
            Vec::new()
        };
        let dedicated = counters(self.dedicated);
        let shared = counters(self.shared);
        self.sampled = sampled;
        let Ok(factory): Result<IDXGIFactory1, _> = (unsafe { CreateDXGIFactory1() }) else {
            return nvidia;
        };
        let mut result = Vec::new();
        for index in 0..64 {
            let Ok(adapter) = (unsafe { factory.EnumAdapters1(index) }) else {
                break;
            };
            let Ok(desc) = (unsafe { adapter.GetDesc1() }) else {
                continue;
            };
            if desc.Flags & 2 != 0 {
                continue;
            }
            let length = desc
                .Description
                .iter()
                .position(|c| *c == 0)
                .unwrap_or(desc.Description.len());
            let name = String::from_utf16_lossy(&desc.Description[..length]);
            let luid = format!(
                "luid_0x{:08x}_0x{:08x}",
                desc.AdapterLuid.HighPart as u32, desc.AdapterLuid.LowPart
            );
            let usage = adapter_usage(&engines, &luid);
            let memory = |values: &[(String, f64)]| {
                values
                    .iter()
                    .find(|(id, _)| id.contains(&luid))
                    .map(|(_, value)| *value as u64)
            };
            result.push(GpuMetric {
                id: luid.clone(),
                name,
                vendor: match desc.VendorId {
                    0x10de => "NVIDIA",
                    0x1002 => "AMD",
                    0x8086 => "Intel",
                    _ => "GPU",
                }
                .into(),
                status: if usage.is_some() {
                    ObservationStatus::Ready
                } else {
                    ObservationStatus::Unsupported
                },
                source: "Windows PDH/DXGI".into(),
                usage_percent: usage,
                memory_used_bytes: memory(&dedicated),
                memory_total_bytes: (desc.DedicatedVideoMemory > 0)
                    .then_some(desc.DedicatedVideoMemory as u64),
                shared_memory_used_bytes: memory(&shared),
                message: usage
                    .is_none()
                    .then(|| "正在等待有效 WDDM 计数器；旧驱动可能不支持".into()),
                ..Default::default()
            });
        }
        // 仅在名称可唯一关联时补充 NVML 传感器，避免两张同名显卡串号。
        let names = result.iter().map(|g| g.name.clone()).collect::<Vec<_>>();
        for gpu in &mut result {
            if names.iter().filter(|name| **name == gpu.name).count() != 1 {
                continue;
            }
            let candidates = nvidia
                .iter()
                .filter(|g| g.name == gpu.name)
                .collect::<Vec<_>>();
            if let [device] = candidates.as_slice() {
                gpu.temperature_celsius = device.temperature_celsius;
                gpu.power_watts = device.power_watts;
            }
        }
        if result.is_empty() {
            nvidia
        } else {
            result
        }
    }
}

pub(crate) fn monitor_environment() -> String {
    "Windows 主机".into()
}

pub(crate) fn configure_usage_probe_command(command: &mut std::process::Command) {
    crate::platform::configure_status_command(command);
}

pub(crate) struct UsageProbeGuard(super::StatusCommandGuard);
impl UsageProbeGuard {
    pub(crate) fn new(child: &std::process::Child) -> io::Result<Self> {
        super::StatusCommandGuard::from_std_child(child).map(Self)
    }
    pub(crate) fn terminate(&mut self) {
        self.0.terminate();
    }
}
/// Windows 没有信号语义：只有退出码（`from_status` 在非 Unix 上不会填 `signal`）。
pub(crate) fn usage_probe_exit(
    child: &mut std::process::Child,
) -> io::Result<Option<crate::platform::UsageProbeExit>> {
    child
        .try_wait()
        .map(|status| status.map(crate::platform::UsageProbeExit::from_status))
}

pub(crate) fn terminate_usage_probe(child: &mut std::process::Child) {
    if child.try_wait().ok().flatten().is_some() {
        return;
    }
    let pids = super::session_processes(child.id());
    for pid in pids.into_iter().rev() {
        if let Ok(token) = process_instance_token(pid) {
            let _ = terminate_monitored_process(pid, &token, true);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

pub(crate) fn terminate_usage_pty(child: &mut dyn portable_pty::Child) {
    if let Some(root) = child.process_id() {
        for pid in super::session_processes(root).into_iter().rev() {
            if let Ok(token) = process_instance_token(pid) {
                let _ = terminate_monitored_process(pid, &token, true);
            }
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

struct ProcessHandle(HANDLE);
impl Drop for ProcessHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

fn process_handle(pid: u32, terminate: bool) -> io::Result<ProcessHandle> {
    let access = PROCESS_QUERY_LIMITED_INFORMATION | if terminate { PROCESS_TERMINATE } else { 0 };
    let handle = unsafe { OpenProcess(access, 0, pid) };
    if handle.is_null() {
        Err(io::Error::last_os_error())
    } else {
        Ok(ProcessHandle(handle))
    }
}

fn creation_time(handle: &ProcessHandle) -> io::Result<String> {
    let mut creation: FILETIME = unsafe { std::mem::zeroed() };
    let mut exit: FILETIME = unsafe { std::mem::zeroed() };
    let mut kernel: FILETIME = unsafe { std::mem::zeroed() };
    let mut user: FILETIME = unsafe { std::mem::zeroed() };
    if unsafe { GetProcessTimes(handle.0, &mut creation, &mut exit, &mut kernel, &mut user) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(
        ((u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime))
            .to_string(),
    )
}

pub(crate) fn process_instance_token(pid: u32) -> io::Result<String> {
    creation_time(&process_handle(pid, false)?)
}

pub(crate) struct MonitoredProcess {
    handle: ProcessHandle,
    pub(crate) instance_token: String,
    pub(crate) name: String,
}

impl MonitoredProcess {
    pub(crate) fn open(pid: u32) -> io::Result<Self> {
        let handle = process_handle(pid, true)?;
        let instance_token = creation_time(&handle)?;
        let mut name = vec![0_u16; 32768];
        let mut length = name.len() as u32;
        if unsafe { QueryFullProcessImageNameW(handle.0, 0, name.as_mut_ptr(), &mut length) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let image = String::from_utf16_lossy(&name[..length as usize]);
        let name = image
            .rsplit(['\\', '/'])
            .next()
            .unwrap_or(&image)
            .to_owned();
        Ok(Self {
            handle,
            instance_token,
            name,
        })
    }

    pub(crate) fn terminate(&self, force: bool) -> io::Result<()> {
        if !force {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Windows 需要单独确认强制结束",
            ));
        }
        if unsafe { TerminateProcess(self.handle.0, 1) } == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

pub(crate) fn terminate_monitored_process(pid: u32, expected: &str, force: bool) -> io::Result<()> {
    if !force {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Windows 需要单独确认强制结束",
        ));
    }
    if pid <= 1 || pid == std::process::id() {
        return Err(io::Error::other("受保护的进程"));
    }
    let handle = process_handle(pid, true)?;
    if creation_time(&handle)? != expected {
        return Err(io::Error::other("进程身份已经变化"));
    }
    if unsafe { TerminateProcess(handle.0, 1) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

pub(crate) fn usage_probe_needs_job_helper() -> bool {
    true
}

pub(crate) fn monitor_cpu_inventory() -> Option<String> {
    let count = unsafe { windows_sys::Win32::System::Threading::GetActiveProcessorCount(0xffff) };
    (count > 0).then(|| count.to_string())
}

pub(crate) fn usage_statusline_command(agent: &str, passthrough: bool) -> String {
    use base64::Engine;
    let args = if passthrough { " --passthrough" } else { "" };
    let otherwise = if passthrough {
        "[Console]::Out.Write([Console]::In.ReadToEnd())"
    } else {
        ""
    };
    let script = format!("$enc=[System.Text.UTF8Encoding]::new($false);[Console]::InputEncoding=$enc;[Console]::OutputEncoding=$enc;$OutputEncoding=$enc;if($env:HERDR_ENV -eq '1' -and $env:HERDR_BIN_PATH){{& $env:HERDR_BIN_PATH api usage-report --agent {agent}{args}}}else{{{otherwise}}}");
    let bytes = script
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    format!(
        "powershell.exe -NoLogo -NoProfile -NonInteractive -EncodedCommand {}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpu_uses_busiest_engine_without_double_counting_parallel_engines() {
        let data = vec![
            ("pid_1_luid_0x1_0x2_phys_0_eng_0_engtype_3D".into(), 30.0),
            ("pid_2_luid_0x1_0x2_phys_0_eng_0_engtype_3D".into(), 40.0),
            ("pid_3_luid_0x1_0x2_phys_0_eng_1_engtype_Copy".into(), 50.0),
        ];
        assert_eq!(adapter_usage(&data, "luid_0x1_0x2"), Some(70.0));
        assert_eq!(adapter_usage(&data, "luid_0x9_0x9"), None);
    }
}
