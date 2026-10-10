//! 有界子进程：起一个短命的只读命令，限时、限输出，超时即杀。`opencode db` 与
//! `sqlite3` 两个调用方共用，退出码与输出的含义由调用方判定。
//!
//! 输出由各自的读线程收集：保留上限内的字节，超出的部分照读照丢（子进程不会卡在写满
//! 的管道上），并记下被截断。等子进程与等读线程共用同一个截止时刻：子进程的孙进程
//! 攥着管道不放时，到点照样返回，读线程留到管道关闭时自行结束，不拖住调用方。

use std::io::{self, Read};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// 一次运行的上限。
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// 从启动到收齐输出的总时限。
    pub timeout: Duration,
    /// 轮询子进程是否退出的间隔。
    pub poll: Duration,
    /// stdout 最多保留的字节数。
    pub stdout_bytes: u64,
    /// stderr 最多保留的字节数；`None` 不接 stderr（交给空设备）。
    pub stderr_bytes: Option<u64>,
}

/// 子进程按时退出后的结果。
#[derive(Debug)]
pub struct Output {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    /// stdout 超过了 [`Limits::stdout_bytes`]，`stdout` 只是开头。
    pub stdout_truncated: bool,
    pub stderr: Vec<u8>,
}

#[derive(Debug)]
pub enum RunError {
    /// 起不来：找不到程序、没有权限。
    Spawn(io::Error),
    /// 读线程起不来，或等待子进程失败；子进程已被杀掉。
    Io(io::Error),
    /// 超时；子进程已被杀掉并回收。
    TimedOut,
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(error) => write!(f, "cannot start: {error}"),
            Self::Io(error) => write!(f, "io: {error}"),
            Self::TimedOut => f.write_str("timed out"),
        }
    }
}

/// 运行 `command`（参数与环境由调用方设好；stdin 接空设备，按后台命令配置）。
pub fn run(mut command: Command, limits: Limits) -> Result<Output, RunError> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(if limits.stderr_bytes.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
    crate::platform::configure_background_command(&mut command);
    let deadline = Instant::now() + limits.timeout;
    let mut child = command.spawn().map_err(RunError::Spawn)?;
    let Some(stdout) = child.stdout.take() else {
        terminate(&mut child);
        return Err(RunError::Io(io::Error::other("stdout pipe is missing")));
    };
    let stdout = match spawn_reader(stdout, limits.stdout_bytes) {
        Ok(reader) => reader,
        Err(error) => {
            terminate(&mut child);
            return Err(RunError::Io(error));
        }
    };
    let stderr = match (limits.stderr_bytes, child.stderr.take()) {
        (Some(cap), Some(pipe)) => match spawn_reader(pipe, cap) {
            Ok(reader) => Some(reader),
            Err(error) => {
                terminate(&mut child);
                return Err(RunError::Io(error));
            }
        },
        _ => None,
    };
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(limits.poll),
            Ok(None) => {
                terminate(&mut child);
                return Err(RunError::TimedOut);
            }
            Err(error) => {
                terminate(&mut child);
                return Err(RunError::Io(error));
            }
        }
    };
    let (stdout, stdout_truncated) = collect(&stdout, deadline)?;
    let stderr = match &stderr {
        Some(reader) => collect(reader, deadline)?.0,
        None => Vec::new(),
    };
    Ok(Output {
        status,
        stdout,
        stdout_truncated,
        stderr,
    })
}

type Collected = (Vec<u8>, bool);

/// 起一个读线程：保留前 `cap` 字节，其余读掉丢弃，结果经通道交回。
fn spawn_reader<R>(reader: R, cap: u64) -> io::Result<mpsc::Receiver<Collected>>
where
    R: Read + Send + 'static,
{
    let (sender, receiver) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("herdr-subprocess-output".into())
        .spawn(move || {
            let mut reader = reader;
            let mut kept = Vec::new();
            let _ = (&mut reader).take(cap).read_to_end(&mut kept);
            let dropped = io::copy(&mut reader, &mut io::sink()).unwrap_or(0);
            let _ = sender.send((kept, dropped > 0));
        })?;
    Ok(receiver)
}

fn collect(reader: &mpsc::Receiver<Collected>, deadline: Instant) -> Result<Collected, RunError> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    match reader.recv_timeout(remaining) {
        Ok(collected) => Ok(collected),
        Err(mpsc::RecvTimeoutError::Timeout) => Err(RunError::TimedOut),
        // 读线程没交结果就结束了（按构造不会发生）：当作没有输出。
        Err(mpsc::RecvTimeoutError::Disconnected) => Ok((Vec::new(), false)),
    }
}

fn terminate(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(timeout: Duration, stdout_bytes: u64) -> Limits {
        Limits {
            timeout,
            poll: Duration::from_millis(5),
            stdout_bytes,
            stderr_bytes: Some(1024),
        }
    }

    #[cfg(windows)]
    fn script(text: &str) -> Command {
        use std::os::windows::process::CommandExt;
        let mut command = Command::new("cmd");
        command.args(["/D", "/C"]).raw_arg(text);
        command
    }

    #[cfg(unix)]
    fn script(text: &str) -> Command {
        let mut command = Command::new("sh");
        command.args(["-c", text]);
        command
    }

    /// 一个只睡不写的进程（不经 shell，杀掉它就没有孙进程留着管道）。
    #[cfg(windows)]
    fn sleeper() -> Command {
        let mut command = Command::new("powershell");
        command.args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "Start-Sleep -Seconds 30",
        ]);
        command
    }

    #[cfg(unix)]
    fn sleeper() -> Command {
        let mut command = Command::new("sleep");
        command.arg("30");
        command
    }

    #[test]
    fn output_and_exit_status_come_back() {
        let output = run(script("echo hello"), limits(Duration::from_secs(30), 1024))
            .expect("echo 可以运行");
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "hello");
        assert!(!output.stdout_truncated);
        let output =
            run(script("exit 3"), limits(Duration::from_secs(30), 1024)).expect("非零退出照样交回");
        assert_eq!(output.status.code(), Some(3));
    }

    #[test]
    fn stdout_beyond_the_cap_is_drained_and_flagged() {
        #[cfg(windows)]
        let text = "for /L %i in (1,1,400) do @echo 0123456789";
        #[cfg(unix)]
        let text = "i=0; while [ $i -lt 400 ]; do echo 0123456789; i=$((i+1)); done";
        let output = run(script(text), limits(Duration::from_secs(30), 64)).expect("可以运行");
        assert!(
            output.status.success(),
            "超出的输出照读照丢，子进程正常退出"
        );
        assert_eq!(output.stdout.len(), 64);
        assert!(output.stdout_truncated);
    }

    #[test]
    fn a_hung_process_is_killed_at_the_deadline() {
        let started = Instant::now();
        let result = run(sleeper(), limits(Duration::from_millis(300), 1024));
        assert!(matches!(result, Err(RunError::TimedOut)), "{result:?}");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "到点即收尾：{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_missing_program_reports_a_spawn_error() {
        let command = Command::new("herdr-no-such-program-for-subprocess-test");
        assert!(matches!(
            run(command, limits(Duration::from_secs(5), 1024)),
            Err(RunError::Spawn(_))
        ));
    }
}
