use std::io::{self, Read, Write};
use std::mem::size_of;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows_sys::Wdk::Storage::FileSystem::{
    FilePipeLocalInformation, NtFsControlFile, NtQueryInformationFile, FILE_PIPE_CONNECTED_STATE,
    FILE_PIPE_LOCAL_INFORMATION, FSCTL_PIPE_FLUSH,
};
use windows_sys::Win32::Foundation::{
    RtlNtStatusToDosError, ERROR_IO_PENDING, HANDLE, STATUS_PENDING, WAIT_FAILED, WAIT_OBJECT_0,
    WAIT_TIMEOUT,
};
use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows_sys::Win32::System::Pipes::{DisconnectNamedPipe, SetNamedPipeHandleState, PIPE_WAIT};
use windows_sys::Win32::System::Threading::{
    CreateEventW, ResetEvent, SetEvent, WaitForMultipleObjects, WaitForSingleObject, INFINITE,
};
use windows_sys::Win32::System::IO::{
    CancelIoEx, GetOverlappedResult, IO_STATUS_BLOCK, OVERLAPPED,
};

#[derive(Debug)]
pub(crate) struct ClientStreamControl {
    handle: OwnedHandle,
    cancelled: OwnedHandle,
    stopped: AtomicBool,
    streams: AtomicUsize,
    deadline: Mutex<Option<Instant>>,
}

impl ClientStreamControl {
    fn issue(&self, operation: impl FnOnce() -> i32) -> io::Result<bool> {
        self.check()?;
        if operation() != 0 {
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_IO_PENDING as i32) {
            Ok(false)
        } else {
            Err(error)
        }
    }

    pub(crate) fn shutdown(&self) {
        if !self.stopped.swap(true, Ordering::AcqRel) {
            unsafe {
                DisconnectNamedPipe(self.handle.as_raw_handle());
                CancelIoEx(self.handle.as_raw_handle(), null());
                SetEvent(self.cancelled.as_raw_handle());
            }
        }
    }

    pub(crate) fn set_deadline(&self, deadline: Instant) {
        let mut current = self.deadline.lock().unwrap_or_else(|e| e.into_inner());
        *current = Some(current.map_or(deadline, |old| old.min(deadline)));
    }

    fn check(&self) -> io::Result<()> {
        if self.stopped.load(Ordering::Acquire) {
            return Err(io::ErrorKind::ConnectionAborted.into());
        }
        if self
            .deadline
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "client drain deadline exceeded",
            ));
        }
        Ok(())
    }

    fn pipe_info(&self) -> io::Result<FILE_PIPE_LOCAL_INFORMATION> {
        self.check()?;
        let mut info = FILE_PIPE_LOCAL_INFORMATION::default();
        let mut status = IO_STATUS_BLOCK::default();
        status.Anonymous.Status = STATUS_PENDING;
        let result = unsafe {
            NtQueryInformationFile(
                self.handle.as_raw_handle(),
                &mut status,
                (&mut info as *mut FILE_PIPE_LOCAL_INFORMATION).cast(),
                size_of::<FILE_PIPE_LOCAL_INFORMATION>() as u32,
                FilePipeLocalInformation,
            )
        };
        if result == STATUS_PENDING {
            // Cancellation is not completion: both kernel-owned buffers stay alive until the
            // status block completes. An asynchronous query is an abort, never a flush fallback.
            self.shutdown();
            while unsafe { std::ptr::read_volatile(&status.Anonymous.Status) } == STATUS_PENDING {
                std::thread::sleep(Duration::from_millis(1));
            }
            return Err(io::Error::other("asynchronous pipe quota query cancelled"));
        }
        if result < 0 {
            return Err(io::Error::other(format!(
                "pipe quota query failed: {result:#x}"
            )));
        }
        if info.NamedPipeState != FILE_PIPE_CONNECTED_STATE {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        Ok(info)
    }
}

#[derive(Debug)]
pub(crate) struct ServerClientStream {
    control: Arc<ClientStreamControl>,
    event: Mutex<OwnedHandle>,
    write_timeout: Duration,
}

fn event() -> io::Result<OwnedHandle> {
    let handle = unsafe { CreateEventW(null(), 1, 0, null()) };
    if handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}

pub(crate) fn prepare_server_client_stream(
    stream: crate::ipc::LocalStream,
    write_timeout: Duration,
) -> io::Result<ServerClientStream> {
    let crate::ipc::LocalStream::NamedPipe(pipe) = stream;
    // Consume before cloning or doing I/O. Dropping interprocess's cloned stream can enter its
    // unbounded FlushFileBuffers linger pool, even after assume_flushed().
    let handle: OwnedHandle = pipe.into();
    let mode = PIPE_WAIT;
    if unsafe { SetNamedPipeHandleState(handle.as_raw_handle(), &mode, null(), null()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(ServerClientStream {
        control: Arc::new(ClientStreamControl {
            handle,
            cancelled: event()?,
            stopped: AtomicBool::new(false),
            streams: AtomicUsize::new(1),
            deadline: Mutex::new(None),
        }),
        event: Mutex::new(event()?),
        write_timeout,
    })
}

pub(crate) fn client_stream_control(
    stream: &ServerClientStream,
) -> io::Result<Arc<ClientStreamControl>> {
    Ok(stream.control.clone())
}

impl ServerClientStream {
    pub(crate) fn try_clone(&self) -> io::Result<Self> {
        let event = event()?;
        self.control.streams.fetch_add(1, Ordering::Relaxed);
        Ok(Self {
            control: self.control.clone(),
            event: Mutex::new(event),
            write_timeout: self.write_timeout,
        })
    }

    fn complete_io(
        &self,
        started: bool,
        overlapped: &mut OVERLAPPED,
        deadline: Option<Instant>,
    ) -> io::Result<usize> {
        let handle = self.control.handle.as_raw_handle();
        if !started {
            let handles: [HANDLE; 2] = [overlapped.hEvent, self.control.cancelled.as_raw_handle()];
            let wait = deadline.map_or(INFINITE, |deadline| {
                deadline
                    .saturating_duration_since(Instant::now())
                    .as_millis()
                    .min((INFINITE - 1) as u128) as u32
            });
            let result = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, wait) };
            if result != WAIT_OBJECT_0 {
                let error = if result == WAIT_TIMEOUT {
                    io::Error::new(io::ErrorKind::TimedOut, "client I/O deadline exceeded")
                } else if result == WAIT_FAILED {
                    io::Error::last_os_error()
                } else {
                    io::ErrorKind::ConnectionAborted.into()
                };
                // The OVERLAPPED and borrowed data cannot be freed until cancellation completes.
                unsafe {
                    CancelIoEx(handle, overlapped);
                    let mut count = 0;
                    GetOverlappedResult(handle, overlapped, &mut count, 1);
                }
                return Err(error);
            }
        }
        let mut count = 0;
        if unsafe { GetOverlappedResult(handle, overlapped, &mut count, 1) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(count as usize)
    }

    fn read_until(&self, data: &mut [u8], deadline: Option<Instant>) -> io::Result<usize> {
        if data.is_empty() {
            return Ok(0);
        }
        self.control.check()?;
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(io::ErrorKind::TimedOut.into());
        }
        let event = self.event.lock().unwrap_or_else(|e| e.into_inner());
        unsafe { ResetEvent(event.as_raw_handle()) };
        let mut overlapped = OVERLAPPED {
            hEvent: event.as_raw_handle(),
            ..OVERLAPPED::default()
        };
        let started = self.control.issue(|| unsafe {
            ReadFile(
                self.control.handle.as_raw_handle(),
                data.as_mut_ptr(),
                data.len().min(u32::MAX as usize) as u32,
                null_mut(),
                &mut overlapped,
            )
        })?;
        self.complete_io(started, &mut overlapped, deadline)
    }
}

impl Drop for ServerClientStream {
    fn drop(&mut self) {
        if self.control.streams.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.control.shutdown();
        }
    }
}

impl Read for ServerClientStream {
    fn read(&mut self, data: &mut [u8]) -> io::Result<usize> {
        self.read_until(data, None)
    }
}

impl Write for ServerClientStream {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        write_client_stream(self, data)?;
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(crate) fn read_client_handshake(
    stream: &mut ServerClientStream,
    data: &mut [u8],
    deadline: Instant,
) -> io::Result<usize> {
    stream.read_until(data, Some(deadline))
}

pub(crate) fn write_client_stream(stream: &ServerClientStream, mut data: &[u8]) -> io::Result<()> {
    let mut progress = Instant::now();
    while !data.is_empty() {
        let info = stream.control.pipe_info()?;
        // Quota is a size hint, not a readiness gate: a pending peer read may reserve all of it.
        // At zero quota one cancellable byte waits for real progress without idle polling or
        // hiding partial progress inside a large, still-incomplete WriteFile request.
        let count = data
            .len()
            .min((info.WriteQuotaAvailable as usize).max(1))
            .min(64 * 1024);
        let event = stream.event.lock().unwrap_or_else(|e| e.into_inner());
        unsafe { ResetEvent(event.as_raw_handle()) };
        let mut overlapped = OVERLAPPED {
            hEvent: event.as_raw_handle(),
            ..OVERLAPPED::default()
        };
        let started = stream.control.issue(|| unsafe {
            WriteFile(
                stream.control.handle.as_raw_handle(),
                data.as_ptr(),
                count as u32,
                null_mut(),
                &mut overlapped,
            )
        })?;
        let written = stream.complete_io(
            started,
            &mut overlapped,
            Some(progress + stream.write_timeout),
        )?;
        if written == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        data = &data[written..];
        progress = Instant::now();
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn client_transport_process_sample() -> (u32, u32, Duration) {
    use windows_sys::Win32::Foundation::{FILETIME, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
    };
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, GetProcessHandleCount, GetProcessTimes,
    };

    let mut handles = 0;
    assert_ne!(
        unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut handles) },
        0
    );
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    assert_ne!(snapshot, INVALID_HANDLE_VALUE);
    let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot) };
    let mut entry = THREADENTRY32 {
        dwSize: size_of::<THREADENTRY32>() as u32,
        ..THREADENTRY32::default()
    };
    let mut threads = 0;
    let mut next = unsafe { Thread32First(snapshot.as_raw_handle(), &mut entry) };
    while next != 0 {
        if entry.th32OwnerProcessID == std::process::id() {
            threads += 1;
        }
        next = unsafe { Thread32Next(snapshot.as_raw_handle(), &mut entry) };
    }
    let mut created = FILETIME::default();
    let mut exited = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    assert_ne!(
        unsafe {
            GetProcessTimes(
                GetCurrentProcess(),
                &mut created,
                &mut exited,
                &mut kernel,
                &mut user,
            )
        },
        0
    );
    let ticks =
        |time: FILETIME| (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime);
    (
        handles,
        threads,
        Duration::from_nanos((ticks(kernel) + ticks(user)) * 100),
    )
}

pub(crate) fn finish_client_stream(stream: &mut ServerClientStream) -> io::Result<()> {
    stream.control.check()?;
    let event = stream.event.lock().unwrap_or_else(|e| e.into_inner());
    if unsafe { ResetEvent(event.as_raw_handle()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut status = IO_STATUS_BLOCK::default();
    status.Anonymous.Status = STATUS_PENDING;
    let submitted = unsafe {
        NtFsControlFile(
            stream.control.handle.as_raw_handle(),
            event.as_raw_handle(),
            None,
            null(),
            &mut status,
            FSCTL_PIPE_FLUSH,
            null(),
            0,
            null_mut(),
            0,
        )
    };
    let native_result = |status| {
        if status < 0 {
            Err(io::Error::from_raw_os_error(unsafe {
                RtlNtStatusToDosError(status) as i32
            }))
        } else {
            Ok(())
        }
    };
    if submitted != STATUS_PENDING {
        native_result(submitted)?;
        stream.control.shutdown();
        return Ok(());
    }
    let result = (|| {
        let mut progress = Instant::now();
        let mut previous_quota = None;
        let handles = [
            event.as_raw_handle(),
            stream.control.cancelled.as_raw_handle(),
        ];
        loop {
            match unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, 16) } {
                WAIT_OBJECT_0 => return native_result(unsafe { status.Anonymous.Status }),
                WAIT_TIMEOUT => {}
                WAIT_FAILED => return Err(io::Error::last_os_error()),
                _ => return Err(io::ErrorKind::ConnectionAborted.into()),
            }
            let info = stream.control.pipe_info()?;
            if previous_quota.is_some_and(|quota| info.WriteQuotaAvailable > quota) {
                progress = Instant::now();
            }
            previous_quota = Some(info.WriteQuotaAvailable);
            if progress.elapsed() >= stream.write_timeout {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "client stopped draining output",
                ));
            }
        }
    })();
    stream.control.shutdown();
    if result.is_err() {
        // CancelIoEx requests cancellation; the status block must outlive actual completion.
        while unsafe { WaitForSingleObject(event.as_raw_handle(), INFINITE) } != WAIT_OBJECT_0 {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    result
}
