use std::io;
use std::process::Output;
use std::thread;
use std::time::{Duration, Instant};

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
    child: std::process::Child,
    timeout: Duration,
) -> io::Result<Output> {
    wait_with_output_timeout_bounded(child, timeout, usize::MAX, usize::MAX)
}

pub(super) fn wait_with_output_timeout_bounded(
    mut child: std::process::Child,
    timeout: Duration,
    stdout_limit: usize,
    stderr_limit: usize,
) -> io::Result<Output> {
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("SSH command stdout was not captured"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("SSH command stderr was not captured"))?;
    let stdout = thread::spawn(move || read_to_end_bounded(stdout, stdout_limit));
    let stderr = thread::spawn(move || read_to_end_bounded(stderr, stderr_limit));
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout.join();
                let _ = stderr.join();
                return Err(error);
            }
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout.join();
            let _ = stderr.join();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "noninteractive SSH command timed out",
            ));
        }
        thread::sleep(POLL_INTERVAL);
    };
    let stdout = stdout
        .join()
        .map_err(|_| io::Error::other("SSH stdout reader panicked"))??;
    let stderr = stderr
        .join()
        .map_err(|_| io::Error::other("SSH stderr reader panicked"))??;
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

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
