use std::io;
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
            "任务已取消；已完成的远端操作不会回滚",
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
        .ok_or_else(|| io::Error::other("远端任务输入管道未打开"))?;
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
    copied
        .recv_timeout(remaining)
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "远端上传未在期限内完成"))??;
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
            return Err(io::Error::new(io::ErrorKind::TimedOut, "远端任务输出超时"));
        }
        thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::Read as _;
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
