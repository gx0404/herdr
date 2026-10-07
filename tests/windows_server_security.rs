#![cfg(windows)]

use std::io;
use std::mem::size_of;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::ptr::null_mut;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use interprocess::os::windows::named_pipe::{pipe_mode::Bytes, DuplexPipeStream};
use interprocess::ConnectWaitMode;
use windows_sys::Win32::Foundation::{LocalFree, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Security::{
    Authorization::ConvertStringSidToSidW, CreateRestrictedToken, GetLengthSid, GetSidSubAuthority,
    GetSidSubAuthorityCount, GetTokenInformation, ImpersonateLoggedOnUser, RevertToSelf,
    SetTokenInformation, TokenIntegrityLevel, DISABLE_MAX_PRIVILEGE, LUA_TOKEN, SID_AND_ATTRIBUTES,
    TOKEN_ADJUST_DEFAULT, TOKEN_DUPLICATE, TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicAccountingInformation,
    JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
    TerminateJobObject, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetProcessIdOfThread, OpenProcessToken, OpenThread, ResumeThread,
    CREATE_NO_WINDOW, CREATE_SUSPENDED, THREAD_QUERY_LIMITED_INFORMATION, THREAD_SUSPEND_RESUME,
};

struct TestServer {
    child: Option<Child>,
    job: Option<OwnedHandle>,
    directory: PathBuf,
}

impl TestServer {
    fn start(&mut self, command: &mut Command) -> io::Result<()> {
        self.job = Some(private_job()?);
        command.creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED);
        self.child = Some(command.spawn()?);
        let child = self.child.as_ref().unwrap();
        let job = self.job.as_ref().unwrap();
        if unsafe { AssignProcessToJobObject(job.as_raw_handle(), child.as_raw_handle()) } == 0 {
            let error = io::Error::last_os_error();
            self.stop()?;
            return Err(error);
        }
        if let Err(error) = resume_child(child) {
            self.stop()?;
            return Err(error);
        }
        Ok(())
    }

    fn stop(&mut self) -> io::Result<()> {
        if let Some(job) = &self.job {
            if unsafe { TerminateJobObject(job.as_raw_handle(), 1) } == 0 {
                return Err(io::Error::last_os_error());
            }
        }
        if let Some(child) = self.child.as_mut() {
            // Assignment failure leaves a suspended child outside the job.
            child.kill()?;
        }
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let child_exited = match self.child.as_mut() {
                Some(child) => child.try_wait()?.is_some(),
                None => true,
            };
            let job_empty = match &self.job {
                Some(job) => active_processes(job)? == 0,
                None => true,
            };
            if child_exited && job_empty {
                self.child = None;
                self.job = None;
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "test server job did not become empty",
                ));
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Err(error) = self.stop() {
            eprintln!(
                "test server cleanup failed; preserving {}: {error}",
                self.directory.display()
            );
            return;
        }
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

fn private_job() -> io::Result<OwnedHandle> {
    let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    let job = unsafe { OwnedHandle::from_raw_handle(handle) };
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    if unsafe {
        SetInformationJobObject(
            job.as_raw_handle(),
            JobObjectExtendedLimitInformation,
            std::ptr::from_ref(&limits).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(job)
}

fn active_processes(job: &OwnedHandle) -> io::Result<u32> {
    let mut accounting = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
    if unsafe {
        QueryInformationJobObject(
            job.as_raw_handle(),
            JobObjectBasicAccountingInformation,
            std::ptr::from_mut(&mut accounting).cast(),
            size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
            null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(accounting.ActiveProcesses)
}

fn resume_child(child: &Child) -> io::Result<()> {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot) };
    let mut entry = THREADENTRY32 {
        dwSize: size_of::<THREADENTRY32>() as u32,
        ..Default::default()
    };
    if unsafe { Thread32First(snapshot.as_raw_handle(), &mut entry) } == 0 {
        return Err(io::Error::last_os_error());
    }
    loop {
        if entry.th32OwnerProcessID == child.id() {
            let thread = unsafe {
                OpenThread(
                    THREAD_SUSPEND_RESUME | THREAD_QUERY_LIMITED_INFORMATION,
                    0,
                    entry.th32ThreadID,
                )
            };
            if thread.is_null() {
                return Err(io::Error::last_os_error());
            }
            let thread = unsafe { OwnedHandle::from_raw_handle(thread) };
            if unsafe { GetProcessIdOfThread(thread.as_raw_handle()) } != child.id() {
                return Err(io::Error::other("created child thread identity changed"));
            }
            if unsafe { ResumeThread(thread.as_raw_handle()) } == u32::MAX {
                return Err(io::Error::last_os_error());
            }
            return Ok(());
        }
        if unsafe { Thread32Next(snapshot.as_raw_handle(), &mut entry) } == 0 {
            return Err(io::Error::other(
                "created child primary thread was not found",
            ));
        }
    }
}

fn connect(path: &Path) -> io::Result<()> {
    let name = format!(r"\\.\pipe\{}", path.display());
    DuplexPipeStream::<Bytes>::connect_by_path_with_wait_mode(
        name.as_str(),
        ConnectWaitMode::Timeout(Duration::from_millis(500)),
    )
    .map(|_| ())
}

#[test]
fn server_integrity_requires_startup_consent_on_both_endpoints() {
    // Windows integrity SID RIDs and the SE_GROUP_INTEGRITY attribute.
    const LOW: u32 = 0x1000;
    const MEDIUM: u32 = 0x2000;
    const HIGH: u32 = 0x3000;

    let mut raw_token = null_mut();
    assert_ne!(
        unsafe {
            OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_QUERY | TOKEN_DUPLICATE | TOKEN_ADJUST_DEFAULT,
                &mut raw_token,
            )
        },
        0
    );
    let token = unsafe { OwnedHandle::from_raw_handle(raw_token) };
    let mut buffer = [0usize; 64];
    let mut needed = 0;
    assert_ne!(
        unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenIntegrityLevel,
                buffer.as_mut_ptr().cast(),
                std::mem::size_of_val(&buffer) as u32,
                &mut needed,
            )
        },
        0
    );
    let label = unsafe { &*buffer.as_ptr().cast::<TOKEN_MANDATORY_LABEL>() };
    let integrity = unsafe {
        *GetSidSubAuthority(
            label.Label.Sid,
            u32::from(*GetSidSubAuthorityCount(label.Label.Sid)) - 1,
        )
    };

    let directory = std::env::temp_dir().join(format!(
        "herdr-server-integrity-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&directory).unwrap();
    let mut server = TestServer {
        child: None,
        job: None,
        directory,
    };
    let app_dir = if cfg!(debug_assertions) {
        "herdr-dev"
    } else {
        "herdr"
    };
    let session_dir = server
        .directory
        .join(app_dir)
        .join("sessions")
        .join("integrity-test");
    let endpoints = [
        session_dir.join("herdr.sock"),
        session_dir.join("herdr-client.sock"),
    ];

    // Reuse the named session after stopping, proving consent is process-local.
    for (configured, allow_unelevated) in [
        (false, false),
        (false, true),
        (false, false),
        (true, false),
        (false, false),
    ] {
        std::fs::create_dir_all(server.directory.join(app_dir)).unwrap();
        let configuration = if configured || allow_unelevated {
            format!("[server]\nallow_unelevated_clients = {configured}\n")
        } else {
            String::new()
        };
        std::fs::write(
            server.directory.join(app_dir).join("config.toml"),
            configuration,
        )
        .unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_herdr"));
        command.args(["--session", "integrity-test", "server"]);
        if allow_unelevated {
            command.arg("--allow-unelevated-clients");
        }
        command
            .env("HOME", &server.directory)
            .env("USERPROFILE", &server.directory)
            .env("APPDATA", &server.directory)
            .env("LOCALAPPDATA", &server.directory)
            .env("XDG_CONFIG_HOME", &server.directory)
            .env("XDG_STATE_HOME", &server.directory)
            .env("XDG_DATA_HOME", &server.directory)
            .env("XDG_CACHE_HOME", &server.directory)
            .env("XDG_RUNTIME_DIR", &server.directory)
            .current_dir(&server.directory)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW);
        for name in [
            "HERDR_SOCKET_PATH",
            "HERDR_CLIENT_SOCKET_PATH",
            "HERDR_CONFIG_PATH",
            "CLAUDE_CONFIG_DIR",
            "CODEX_HOME",
            "KIMI_CODE_HOME",
            "PI_CODING_AGENT_DIR",
            "HERDR_SESSION",
            "HERDR_WORKSPACE",
            "HERDR_TAB",
            "HERDR_PANE",
            "HERDR_TERMINAL",
            "HERDR_STARTUP_CWD",
        ] {
            command.env_remove(name);
        }
        server
            .start(&mut command)
            .expect("start job-owned headless server");
        let deadline = Instant::now() + Duration::from_secs(30);
        for endpoint in &endpoints {
            loop {
                let status = server.child.as_mut().unwrap().try_wait().unwrap();
                assert!(
                    status.is_none(),
                    "server exited before readiness: {status:?}"
                );
                match connect(endpoint) {
                    Ok(()) => break,
                    Err(error) => {
                        assert!(
                            Instant::now() < deadline,
                            "{} not ready: {error}",
                            endpoint.display()
                        );
                        std::thread::sleep(Duration::from_millis(25));
                    }
                }
            }
        }

        let mut restricted = null_mut();
        assert_ne!(
            unsafe {
                CreateRestrictedToken(
                    token.as_raw_handle(),
                    DISABLE_MAX_PRIVILEGE | LUA_TOKEN,
                    0,
                    null_mut(),
                    0,
                    null_mut(),
                    0,
                    null_mut(),
                    &mut restricted,
                )
            },
            0
        );
        let restricted = unsafe { OwnedHandle::from_raw_handle(restricted) };

        // Exercise high -> medium on elevated runners and medium -> low otherwise.
        // Removing administrator groups alone does not lower a token's integrity.
        for client_integrity in [MEDIUM, LOW] {
            if client_integrity >= integrity {
                continue;
            }
            let sid_text =
                widestring::U16CString::from_str(format!("S-1-16-{client_integrity}")).unwrap();
            let mut sid = null_mut();
            assert_ne!(
                unsafe { ConvertStringSidToSidW(sid_text.as_ptr(), &mut sid) },
                0
            );
            let client_label = TOKEN_MANDATORY_LABEL {
                Label: SID_AND_ATTRIBUTES {
                    Sid: sid,
                    Attributes: 0x20,
                },
            };
            let set = unsafe {
                SetTokenInformation(
                    restricted.as_raw_handle(),
                    TokenIntegrityLevel,
                    (&client_label as *const TOKEN_MANDATORY_LABEL).cast(),
                    size_of::<TOKEN_MANDATORY_LABEL>() as u32 + GetLengthSid(sid),
                )
            };
            let error = io::Error::last_os_error();
            unsafe { LocalFree(sid.cast()) };
            assert_ne!(set, 0, "lower client integrity: {error}");

            for endpoint in &endpoints {
                assert_ne!(
                    unsafe { ImpersonateLoggedOnUser(restricted.as_raw_handle()) },
                    0
                );
                let connection = connect(endpoint);
                assert_ne!(unsafe { RevertToSelf() }, 0);
                if integrity >= HIGH
                    && (configured || allow_unelevated)
                    && client_integrity == MEDIUM
                {
                    connection.expect("explicit sharing must admit ordinary same-account clients");
                } else {
                    assert_eq!(
                        connection.unwrap_err().kind(),
                        io::ErrorKind::PermissionDenied,
                        "{}: config={configured}, flag={allow_unelevated}, client integrity={client_integrity}",
                        endpoint.display()
                    );
                }
            }
        }
        server.stop().expect("test server job is empty");
    }
}
