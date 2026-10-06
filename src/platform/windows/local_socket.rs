use std::{
    io,
    path::Path,
    time::{Duration, Instant},
};

use interprocess::os::windows::named_pipe::{pipe_mode::Bytes, DuplexPipeStream};
use interprocess::ConnectWaitMode;
use widestring::U16CString;
use windows_sys::Win32::{
    Storage::FileSystem::GetFullPathNameW,
    System::Pipes::{WaitNamedPipeW, NMPWAIT_WAIT_FOREVER},
};

pub(crate) struct WindowsPipeNames {
    pub(crate) open: U16CString,
    pub(crate) wait: U16CString,
}

pub(crate) fn windows_pipe_names(path: &Path) -> io::Result<WindowsPipeNames> {
    let name = U16CString::from_str(format!(r"\\.\pipe\{}", path.to_string_lossy()))
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let mut capacity = name.len() + 1;
    let wait = loop {
        let mut buffer = vec![0_u16; capacity];
        let size = u32::try_from(buffer.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "pipe name is too long"))?;
        let written = unsafe {
            GetFullPathNameW(
                name.as_ptr(),
                size,
                buffer.as_mut_ptr(),
                std::ptr::null_mut(),
            )
        } as usize;
        if written == 0 {
            return Err(io::Error::last_os_error());
        }
        if written < buffer.len() {
            buffer.truncate(written);
            break U16CString::from_vec(buffer)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        }
        capacity = written;
    };
    let prefix = r"\\.\pipe\".encode_utf16().collect::<Vec<_>>();
    if !wait.as_slice().starts_with(&prefix) || wait.len() == prefix.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "pipe name must remain in the local named-pipe namespace",
        ));
    }
    let mut open = wait.as_slice().to_vec();
    open[2] = u16::from(b'?');
    let open = U16CString::from_vec(open)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    Ok(WindowsPipeNames { open, wait })
}

pub(crate) fn connect_local_pipe(
    path: &Path,
    deadline: Option<Instant>,
) -> io::Result<crate::ipc::LocalStream> {
    let names = windows_pipe_names(path)?;
    loop {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(io::ErrorKind::TimedOut.into());
        }
        match DuplexPipeStream::<Bytes>::connect_by_path_with_wait_mode(
            names.open.as_ucstr(),
            ConnectWaitMode::Timeout(Duration::ZERO),
        ) {
            Ok(stream) => return Ok(crate::ipc::LocalStream::NamedPipe(stream.into())),
            Err(error)
                if error.kind() == io::ErrorKind::TimedOut && error.raw_os_error().is_none() => {}
            Err(error) => return Err(error),
        }
        let millis = if let Some(deadline) = deadline {
            let remaining = deadline
                .saturating_duration_since(Instant::now())
                .as_millis();
            if remaining == 0 {
                return Err(io::ErrorKind::TimedOut.into());
            }
            remaining.min(u128::from(u32::MAX - 1)) as u32
        } else {
            NMPWAIT_WAIT_FOREVER
        };
        // WaitNamedPipeW requires the legacy name; only opening uses the extended name.
        if unsafe { WaitNamedPipeW(names.wait.as_ptr(), millis) } == 0 {
            return Err(io::Error::last_os_error());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt as _;
    use std::path::PathBuf;

    #[test]
    fn names_preserve_existing_lossy_path_identity() {
        let mut units = "p9-surrogate-".encode_utf16().collect::<Vec<_>>();
        units.push(0xd800);
        units.extend(".sock".encode_utf16());
        let path = PathBuf::from(OsString::from_wide(&units));
        let names = windows_pipe_names(&path).unwrap();
        let legacy = format!(r"\\.\pipe\{}", path.to_string_lossy());
        let extended = format!(r"\\?\pipe\{}", path.to_string_lossy());
        assert_eq!(
            names.wait.as_slice(),
            legacy.encode_utf16().collect::<Vec<_>>()
        );
        assert_eq!(
            names.open.as_slice(),
            extended.encode_utf16().collect::<Vec<_>>()
        );
    }

    #[test]
    fn canonical_names_never_leave_the_local_pipe_namespace() {
        for path in [
            r"..\..",
            r"..\..\other-device",
            r"\??\C:\file",
            r"\\remote\share\name",
        ] {
            if let Ok(names) = windows_pipe_names(Path::new(path)) {
                assert!(names.wait.to_string_lossy().starts_with(r"\\.\pipe\"));
                assert!(names.open.to_string_lossy().starts_with(r"\\?\pipe\"));
            }
        }
        assert!(
            matches!(windows_pipe_names(Path::new("nul\0name")), Err(error) if error.kind() == io::ErrorKind::InvalidInput)
        );
    }
}
