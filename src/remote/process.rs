use std::io::{self, Read as _, Write as _};
use std::process::Output;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;
use std::time::{Duration, Instant};

/// 仅属于一次客户端后台任务，不进入连接配置或持久化状态。
#[derive(Clone, Debug, Default)]
pub(crate) struct TaskCancellation(Arc<AtomicBool>, Arc<AtomicBool>);

thread_local! {
    static TASK: std::cell::RefCell<Option<TaskCancellation>> = const { std::cell::RefCell::new(None) };
}

impl TaskCancellation {
    pub(crate) fn flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.0)
    }

    pub(crate) fn cancel(&self) {
        if !self.1.load(Ordering::Acquire) {
            self.0.store(true, Ordering::Release);
        }
    }
    pub(crate) fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
    pub(crate) fn complete(&self) {
        self.1.store(true, Ordering::Release);
    }

    pub(crate) fn run<T>(&self, run: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
        struct Restore(Option<TaskCancellation>);
        impl Drop for Restore {
            fn drop(&mut self) {
                TASK.with(|task| {
                    task.replace(self.0.take());
                });
            }
        }
        let previous = TASK.with(|task| task.replace(Some(self.clone())));
        let _restore = Restore(previous);
        check_cancelled()?;
        run()
    }
}

pub(super) fn current_task() -> Option<TaskCancellation> {
    TASK.with(|task| task.borrow().clone())
}

pub(super) fn check_cancelled() -> io::Result<()> {
    if TASK.with(|task| {
        task.borrow()
            .as_ref()
            .is_some_and(TaskCancellation::is_cancelled)
    }) {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            crate::i18n::texts().remote.task_cancelled,
        ))
    } else {
        Ok(())
    }
}

pub(super) struct TaskChild {
    child: std::process::Child,
    guard: Option<crate::platform::StatusCommandGuard>,
}

impl std::ops::Deref for TaskChild {
    type Target = std::process::Child;
    fn deref(&self) -> &Self::Target {
        &self.child
    }
}
impl std::ops::DerefMut for TaskChild {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.child
    }
}
impl From<std::process::Child> for TaskChild {
    fn from(child: std::process::Child) -> Self {
        Self { child, guard: None }
    }
}
impl TaskChild {
    pub(super) fn terminate(&mut self) {
        if let Some(guard) = self.guard.as_mut() {
            guard.terminate();
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Drop for TaskChild {
    fn drop(&mut self) {
        // 结束已拥有的进程组，关闭孙进程继承的输出管道。
        if self.guard.is_some() || self.child.try_wait().ok().flatten().is_none() {
            self.terminate();
        }
    }
}

pub(super) fn spawn(command: &mut std::process::Command) -> io::Result<TaskChild> {
    check_cancelled()?;
    let scoped =
        TASK.with(|task| task.borrow().is_some()) && crate::platform::status_commands_supported();
    if scoped {
        crate::platform::configure_status_command(command);
    }
    let mut child = command.spawn()?;
    let guard = if scoped {
        match crate::platform::StatusCommandGuard::from_std_child(&child) {
            Ok(guard) => Some(guard),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        }
    } else {
        None
    };
    Ok(TaskChild { child, guard })
}

pub(super) fn spawn_owned(command: &mut std::process::Command) -> io::Result<TaskChild> {
    current_task().unwrap_or_default().run(|| spawn(command))
}

pub(super) fn copy_input(
    mut child: TaskChild,
    mut input: impl io::Read + Send + 'static,
    timeout: Duration,
) -> io::Result<Output> {
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other(crate::i18n::texts().remote.task_input_closed))?;
    let (done, copied) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let _ = done.send(io::copy(&mut input, &mut stdin));
    });
    let started = Instant::now();
    let output = wait_with_output_timeout(child, timeout)?;
    if !output.status.success() {
        return Ok(output);
    }
    let remaining = timeout
        .saturating_sub(started.elapsed())
        .max(Duration::from_millis(1));
    copied.recv_timeout(remaining).map_err(|_| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            crate::i18n::texts().remote.upload_timed_out,
        )
    })??;
    Ok(output)
}

const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Reads a child stream to EOF, keeping at most `limit` bytes. Bytes past the
/// limit are discarded so a flooding child cannot grow memory without bound.
pub(super) fn read_to_end_bounded(mut reader: impl io::Read, limit: usize) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            return Ok(bytes);
        }
        let remaining = limit.saturating_sub(bytes.len());
        bytes.extend_from_slice(&buffer[..read.min(remaining)]);
    }
}

pub(super) fn wait_with_output_timeout(
    child: impl Into<TaskChild>,
    timeout: Duration,
) -> io::Result<Output> {
    wait_with_output_timeout_bounded(child, timeout, 16 * 1024 * 1024, 512 * 1024)
}

pub(super) fn wait_with_output_timeout_bounded(
    child: impl Into<TaskChild>,
    timeout: Duration,
    stdout_limit: usize,
    stderr_limit: usize,
) -> io::Result<Output> {
    let mut child = child.into();
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let (out_tx, out_rx) = std::sync::mpsc::sync_channel(1);
    let (err_tx, err_rx) = std::sync::mpsc::sync_channel(1);
    let mut stdout = if let Some(stdout) = stdout_pipe {
        thread::spawn(move || {
            let _ = out_tx.send(read_to_end_bounded(stdout, stdout_limit));
        });
        None
    } else {
        Some(Vec::new())
    };
    let mut stderr = if let Some(stderr) = stderr_pipe {
        thread::spawn(move || {
            let _ = err_tx.send(read_to_end_bounded(stderr, stderr_limit));
        });
        None
    } else {
        Some(Vec::new())
    };
    let started = Instant::now();
    let mut status = None;
    loop {
        if let Err(error) = check_cancelled() {
            child.terminate();
            return Err(error);
        }
        if status.is_none() {
            status = child.try_wait()?;
        }
        if stdout.is_none() {
            stdout = out_rx.try_recv().ok().transpose()?;
        }
        if stderr.is_none() {
            stderr = err_rx.try_recv().ok().transpose()?;
        }
        if status.is_some() && stdout.is_some() && stderr.is_some() {
            if let (Some(status), Some(stdout), Some(stderr)) =
                (status, stdout.take(), stderr.take())
            {
                return Ok(Output {
                    status,
                    stdout,
                    stderr,
                });
            }
        }
        if started.elapsed() >= timeout {
            child.terminate();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                crate::i18n::texts().remote.task_output_timed_out,
            ));
        }
        thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    #[test]
    fn timeout_closes_pipes_inherited_by_grandchildren() {
        let start = Instant::now();
        let error = TaskCancellation::default()
            .run(|| {
                let mut command = Command::new("sh");
                command
                    .args(["-c", "sleep 5 & exit 0"])
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                wait_with_output_timeout(spawn(&mut command)?, Duration::from_millis(100))
            })
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn stalled_upload_is_cancelled_without_starting_the_next_command() {
        let cancel = TaskCancellation::default();
        let worker_cancel = cancel.clone();
        let (ready, started) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            worker_cancel.run::<()>(|| {
                let mut command = Command::new("sh");
                command
                    .args(["-c", "exec sleep 5"])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                let child = spawn(&mut command)?;
                ready.send(()).unwrap();
                copy_input(
                    child,
                    io::repeat(b'x').take(2 * 1024 * 1024),
                    Duration::from_secs(10),
                )?;
                panic!("取消后不能进入提交阶段");
            })
        });
        started.recv_timeout(Duration::from_secs(2)).unwrap();
        let start = Instant::now();
        cancel.cancel();
        assert_eq!(
            worker.join().unwrap().unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn cancellation_stops_only_its_child_and_clears_thread_context() {
        let cancel = TaskCancellation::default();
        let worker_cancel = cancel.clone();
        let (ready, started) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let result = worker_cancel.run(|| {
                let child = Command::new("sh")
                    .args(["-c", "exec sleep 10"])
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()?;
                ready.send(()).unwrap();
                wait_with_output_timeout(child, Duration::from_secs(20))
            });
            assert!(check_cancelled().is_ok(), "取消上下文不能泄漏到后续任务");
            result
        });
        started.recv_timeout(Duration::from_secs(2)).unwrap();
        let start = Instant::now();
        cancel.cancel();
        assert_eq!(
            worker.join().unwrap().unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn timeout_kills_the_child() {
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg("exec sleep 10")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let started = Instant::now();
        let error = wait_with_output_timeout(command.spawn().unwrap(), Duration::from_millis(25))
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn bounded_output_truncates_without_blocking_the_child() {
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg("head -c 65536 /dev/zero | tr '\\0' 'x'")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let output = wait_with_output_timeout_bounded(
            command.spawn().unwrap(),
            Duration::from_secs(5),
            128,
            128,
        )
        .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout.len(), 128);
        assert!(output.stdout.iter().all(|byte| *byte == b'x'));
    }
}

/// 与平台无关的文案用例（上面的 `tests` 只在 unix 上跑真实子进程）。
#[cfg(test)]
mod message_tests {
    use super::*;

    /// 文档终审 D7：取消远端任务的说明按客户端的界面语言给出——英文界面不含 CJK，中文
    /// 界面是中文。
    #[test]
    fn cancelled_task_note_follows_the_interface_language() {
        use crate::i18n::{has_cjk, lang_guard, Lang};
        let task = TaskCancellation::default();
        task.cancel();
        for (lang, chinese) in [(Lang::En, false), (Lang::ZhCn, true)] {
            let _guard = lang_guard(lang);
            let error = task.run(|| Ok(())).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::Interrupted);
            let message = error.to_string();
            assert_eq!(has_cjk(&message), chinese, "{lang:?}: {message}");
        }
    }
}

const SSH_PIPE_POLL_INTERVAL: Duration = Duration::from_millis(10);

pub(super) fn cancelled() -> io::Error {
    io::Error::new(
        io::ErrorKind::ConnectionAborted,
        "SSH connection attempt cancelled",
    )
}

pub(super) struct OwnedSshChild(pub(super) TaskChild);

impl Drop for OwnedSshChild {
    fn drop(&mut self) {
        // Only this SSH invocation is ours; a multiplex master may serve other sessions.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub(super) struct PipeControl {
    cancelled: Arc<AtomicBool>,
    stopped: AtomicBool,
    finished: AtomicBool,
}

impl PipeControl {
    pub(super) fn new(cancelled: Arc<AtomicBool>) -> Arc<Self> {
        Arc::new(Self {
            cancelled,
            stopped: AtomicBool::new(false),
            finished: AtomicBool::new(false),
        })
    }

    pub(super) fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
    }
    pub(super) fn finish_reads(&self) {
        self.finished.store(true, Ordering::Release);
    }

    fn check(&self) -> io::Result<()> {
        if self.stopped.load(Ordering::Acquire) || self.cancelled.load(Ordering::Acquire) {
            Err(cancelled())
        } else {
            Ok(())
        }
    }
}

pub(super) struct CancellablePipe {
    pipe: crate::platform::ChildPipe,
    control: Arc<PipeControl>,
}

impl CancellablePipe {
    pub(super) fn new(pipe: crate::platform::ChildPipe, control: &Arc<PipeControl>) -> Self {
        Self {
            pipe,
            control: Arc::clone(control),
        }
    }
}

impl io::Read for CancellablePipe {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.control.check().is_err() {
                return Ok(0);
            }
            match self.pipe.read(buffer) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    // Drain buffered output, then stop even if a descendant kept the pipe.
                    if self.control.finished.load(Ordering::Acquire) {
                        return Ok(0);
                    }
                    self.pipe.wait(false)?;
                }
                result => return result,
            }
        }
    }
}

impl io::Write for CancellablePipe {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        loop {
            self.control.check()?;
            if self.control.finished.load(Ordering::Acquire) {
                return Err(io::ErrorKind::BrokenPipe.into());
            }
            match self.pipe.write(buffer) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => self.pipe.wait(true)?,
                result => return result,
            }
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) fn wait_cancellable_with_output_timeout(
    child: impl Into<TaskChild>,
    input: Option<Vec<u8>>,
    timeout: Duration,
    cancelled: Arc<AtomicBool>,
) -> io::Result<Output> {
    wait_with_output(child, input, timeout, cancelled, Some(16 * 1024 * 1024))
}

#[cfg(any(unix, test))]
pub(super) fn wait_with_output_limit(
    child: impl Into<TaskChild>,
    timeout: Duration,
    cancelled: Arc<AtomicBool>,
    limit: usize,
) -> io::Result<Output> {
    wait_with_output(child, None, timeout, cancelled, Some(limit))
}

fn read_output(mut pipe: CancellablePipe, limit: Option<usize>) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    if let Some(limit) = limit {
        io::Read::by_ref(&mut pipe)
            .take((limit + 1) as u64)
            .read_to_end(&mut bytes)?;
        // Drain excess so a full pipe cannot manufacture a command timeout.
        io::copy(&mut pipe, &mut io::sink())?;
    } else {
        pipe.read_to_end(&mut bytes)?;
    }
    Ok(bytes)
}

fn wait_with_output(
    child: impl Into<TaskChild>,
    input: Option<Vec<u8>>,
    timeout: Duration,
    cancelled: Arc<AtomicBool>,
    limit: Option<usize>,
) -> io::Result<Output> {
    let mut child = OwnedSshChild(child.into());
    let control = PipeControl::new(cancelled);
    let stdout = crate::platform::ChildPipe::new_reader(
        child
            .0
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("SSH command stdout was not captured"))?,
    )?;
    let stderr = crate::platform::ChildPipe::new_reader(
        child
            .0
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("SSH command stderr was not captured"))?,
    )?;
    let stdin = child
        .0
        .stdin
        .take()
        .map(crate::platform::ChildPipe::new_writer)
        .transpose()?;
    let stdout = CancellablePipe::new(stdout, &control);
    let stderr = CancellablePipe::new(stderr, &control);
    let stdin = stdin.map(|pipe| CancellablePipe::new(pipe, &control));
    thread::scope(|scope| {
        let stdout = scope.spawn(move || read_output(stdout, limit));
        let stderr = scope
            .spawn(move || read_output(stderr, Some(limit.unwrap_or(512 * 1024).min(512 * 1024))));
        let stdin = scope.spawn(move || match (stdin, input) {
            (Some(mut stdin), Some(input)) => stdin.write_all(&input),
            (None, Some(_)) => Err(io::Error::other("ssh bootstrap stdin missing")),
            _ => Ok(()),
        });
        let deadline = Instant::now() + timeout;
        let mut status = None;
        let result = loop {
            if let Err(error) = control.check() {
                break Err(error);
            }
            if status.is_none() {
                match child.0.try_wait() {
                    Ok(Some(exited)) => {
                        status = Some(exited);
                        control.finish_reads();
                    }
                    Ok(None) => {}
                    Err(error) => break Err(error),
                }
            }
            if let Some(status) = status {
                if stdout.is_finished() && stderr.is_finished() && stdin.is_finished() {
                    break Ok(status);
                }
            }
            if Instant::now() >= deadline {
                break Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "noninteractive SSH command timed out",
                ));
            }
            thread::sleep(SSH_PIPE_POLL_INTERVAL);
        };
        if result.is_err() {
            control.stop();
        }
        drop(child);
        let stdout = stdout
            .join()
            .map_err(|_| io::Error::other("SSH stdout reader panicked"));
        let stderr = stderr
            .join()
            .map_err(|_| io::Error::other("SSH stderr reader panicked"));
        let stdin = stdin
            .join()
            .map_err(|_| io::Error::other("SSH stdin writer panicked"));
        let status = result?;
        let stdout = stdout??;
        let stderr = stderr??;
        // A rejected command can close stdin early; its SSH error is more useful.
        if status.success() {
            stdin??;
        }
        Ok(Output {
            status,
            stdout,
            stderr,
        })
    })
}

#[cfg(test)]
pub(in crate::remote) mod pipe_tests {
    use super::*;
    use std::process::{Command, Stdio};

    pub(in crate::remote) struct PipeFixture {
        root: std::path::PathBuf,
        mode: &'static str,
    }

    impl PipeFixture {
        pub(in crate::remote) fn new(mode: &'static str) -> Self {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let root = std::env::temp_dir().join(format!(
                "herdr-pipes-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&root).unwrap();
            Self { root, mode }
        }

        pub(in crate::remote) fn command(&self) -> Command {
            let mut command = fixture_command(self.mode);
            command.env("HERDR_PIPE_FIXTURE_ROOT", &self.root);
            command
        }

        pub(in crate::remote) fn assert_owned_descendant_reaped(&self, pid: u32) {
            // Job/process-group termination is asynchronous. Keep the existing
            // five-second cleanup bound while requiring actual process exit.
            let deadline = Instant::now() + Duration::from_secs(5);
            while crate::platform::process_exists(pid) {
                assert!(
                    Instant::now() < deadline,
                    "owned pipe holder was not reaped"
                );
                thread::sleep(SSH_PIPE_POLL_INTERVAL);
            }
        }

        pub(in crate::remote) fn ready(&self) -> u32 {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if let Some(pid) = std::fs::read_to_string(self.root.join("pid"))
                    .ok()
                    .and_then(|pid| pid.parse().ok())
                {
                    return pid;
                }
                assert!(Instant::now() < deadline, "pipe fixture did not start");
                thread::sleep(SSH_PIPE_POLL_INTERVAL);
            }
        }
    }

    impl Drop for PipeFixture {
        fn drop(&mut self) {
            let _ = std::fs::write(self.root.join("release"), b"");
            if let Some(pid) = std::fs::read_to_string(self.root.join("pid"))
                .ok()
                .and_then(|pid| pid.parse().ok())
            {
                let deadline = Instant::now() + Duration::from_secs(5);
                while crate::platform::process_exists(pid) && Instant::now() < deadline {
                    thread::sleep(SSH_PIPE_POLL_INTERVAL);
                }
            }
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    pub(in crate::remote) fn fixture_command(mode: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        // libtest names omit the crate prefix. Derive the fixture name from its
        // module so moving this test helper cannot silently select zero tests.
        let test_name = concat!(module_path!(), "::pipe_fixture")
            .split_once("::")
            .expect("fixture module includes its crate name")
            .1;
        command
            .args(["--exact", test_name, "--nocapture"])
            .env("HERDR_PIPE_FIXTURE", mode);
        crate::platform::configure_background_command(&mut command);
        command
    }

    #[test]
    // This bounded fixture deliberately leaves inherited pipes open after its parent exits.
    #[allow(clippy::zombie_processes)]
    fn pipe_fixture() {
        let Ok(mode) = std::env::var("HERDR_PIPE_FIXTURE") else {
            return;
        };
        let root = std::path::PathBuf::from(std::env::var_os("HERDR_PIPE_FIXTURE_ROOT").unwrap());
        let release = root.join("release");
        thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(30);
            while !release.exists() && Instant::now() < deadline {
                thread::sleep(SSH_PIPE_POLL_INTERVAL);
            }
            std::process::exit(0);
        });
        if mode == "holder" {
            // Acknowledge only after libtest's startup output, before the parent can close
            // the captured pipes. Otherwise the harness itself can exit on a broken pipe.
            std::fs::write(root.join("pid"), std::process::id().to_string()).unwrap();
        } else {
            fixture_command("holder").spawn().unwrap();
            // Keep the parent alive and the pipe empty until the holder is actually ready.
            while !root.join("pid").exists() {
                thread::sleep(SSH_PIPE_POLL_INTERVAL);
            }
            if mode.starts_with("bridge-") && mode != "bridge-preamble" {
                println!("herdr-remote-output-ready:1");
            }
            if mode == "bridge-output" {
                io::stdout()
                    .write_all(&vec![b'x'; 2 * 1024 * 1024])
                    .unwrap();
            }
            if mode == "output-limit" {
                eprintln!("Permission denied (publickey)");
                io::stdout().write_all(&vec![b'x'; 1024 * 1024]).unwrap();
                io::stderr().write_all(&vec![b'y'; 1024 * 1024]).unwrap();
            }
            println!("final child output");
            eprintln!("Permission denied (publickey)");
        }
        if mode != "exit" && mode != "bridge-final" && mode != "output-limit" {
            thread::sleep(Duration::from_secs(30));
        }
    }

    #[test]
    fn diagnostic_capture_drains_flooded_pipes_with_bounded_memory() {
        let fixture = PipeFixture::new("output-limit");
        let child = fixture
            .command()
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let descendant = fixture.ready();
        let limit = 16 * 1024;
        let output = wait_with_output_limit(
            child,
            Duration::from_secs(5),
            Arc::new(AtomicBool::new(false)),
            limit,
        )
        .unwrap();
        assert!(
            output.status.success(),
            "capture must drain instead of blocking the child"
        );
        assert_eq!(output.stdout.len(), limit + 1);
        assert_eq!(output.stderr.len(), limit + 1);
        assert!(String::from_utf8_lossy(&output.stderr).contains("Permission denied (publickey)"));
        assert!(crate::platform::process_exists(descendant));
    }

    #[test]
    fn child_exit_drains_output_without_waiting_for_inherited_pipes() {
        let fixture = PipeFixture::new("exit");
        let child = fixture
            .command()
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let descendant = fixture.ready();
        let output = wait_cancellable_with_output_timeout(
            child,
            None,
            Duration::from_secs(5),
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        assert!(output.status.success());
        assert!(
            crate::platform::process_exists(descendant),
            "cleanup must not kill a shared descendant"
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("final child output"));
        assert!(String::from_utf8_lossy(&output.stderr).contains("Permission denied (publickey)"));
    }

    #[test]
    fn cancellation_and_timeout_reap_child_with_blocked_pipes() {
        for cancel in [false, true] {
            let fixture = PipeFixture::new("wait");
            let child = fixture
                .command()
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let descendant = fixture.ready();
            let stopped = Arc::new(AtomicBool::new(false));
            let pid = child.id();
            let cancel_stop = Arc::clone(&stopped);
            let cancel_thread = thread::spawn(move || {
                if cancel {
                    thread::sleep(Duration::from_millis(100));
                    cancel_stop.store(true, Ordering::Release);
                }
            });
            let error = wait_cancellable_with_output_timeout(
                child,
                Some(vec![b'x'; 1024 * 1024]),
                // Test cancellation independently of the scheduler's ability
                // to run its thread within the timeout branch's 100 ms margin.
                if cancel {
                    Duration::from_secs(5)
                } else {
                    Duration::from_millis(200)
                },
                stopped,
            )
            .unwrap_err();
            cancel_thread.join().unwrap();
            assert!(
                !crate::platform::process_exists(pid),
                "owned child was not reaped"
            );
            assert_eq!(
                error.kind(),
                if cancel {
                    io::ErrorKind::ConnectionAborted
                } else {
                    io::ErrorKind::TimedOut
                }
            );
            assert!(
                crate::platform::process_exists(descendant),
                "cleanup waited for or killed the independent holder"
            );
        }
    }
}
