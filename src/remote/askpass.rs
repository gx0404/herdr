//! In-memory `SSH_ASKPASS` channel for approved interactive authentication.
//!
//! Background (saved-machine) connections always run ssh with `BatchMode=yes`
//! and never prompt. When such a probe classifies as
//! `ConnectionErrorKind::AuthRequired`, a caller that obtained explicit
//! user approval may retry with this channel attached: ssh runs without
//! BatchMode and with `SSH_ASKPASS`/`SSH_ASKPASS_REQUIRE=force` pointing at a
//! herdr re-executed helper, so password and passphrase prompts travel over a
//! private local socket into the herdr process instead of a terminal.
//!
//! Security posture:
//! - Prompts and answers exist only in process memory and on a user-only
//!   (0600) local socket; nothing is written to disk, logged, or traced.
//! - Answer buffers are zeroed after use.
//! - The channel is created per approved retry and dropped with the
//!   connection it serves.

use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use interprocess::local_socket::traits::Listener as _;
use interprocess::local_socket::ListenerNonblockingMode;

/// Distinguishes multiple channels started inside one process.
// Lives behind `SshAskpassChannel::start`, part of the next stage's surface.
#[allow(dead_code)]
static NEXT_ASKPASS_CHANNEL: AtomicU64 = AtomicU64::new(1);

/// Environment variable that marks a re-executed herdr process as the
/// SSH_ASKPASS helper and tells it where the channel listens. The binary
/// entrypoint checks this variable and routes argv to
/// [`run_ssh_askpass_helper`] (wiring is a follow-up outside this module).
pub(crate) const SSH_ASKPASS_SOCKET_ENV_VAR: &str = "HERDR_SSH_ASKPASS_SOCKET";

// The channel plumbing constants below serve `SshAskpassChannel`, the next
// stage's interactive-auth surface.
#[allow(dead_code)]
const ASKPASS_PROMPT_FRAME_LIMIT: u32 = 4 * 1024;
const ASKPASS_RESPONSE_FRAME_LIMIT: u32 = 1024;
/// A prompt waits this long for an answer before ssh is told "declined".
// See ASKPASS_PROMPT_FRAME_LIMIT.
#[allow(dead_code)]
const ASKPASS_RESPONSE_TIMEOUT: Duration = Duration::from_secs(180);
/// Probe commands with the channel attached allow time for a human answer.
pub(super) const ASKPASS_SSH_COMMAND_TIMEOUT: Duration = Duration::from_secs(240);
// See ASKPASS_PROMPT_FRAME_LIMIT.
#[allow(dead_code)]
const ASKPASS_ACCEPT_POLL: Duration = Duration::from_millis(50);
// See ASKPASS_PROMPT_FRAME_LIMIT.
#[allow(dead_code)]
const ASKPASS_SOCKET_PERMISSION_MODE: u32 = 0o600;

/// Whether interactive SSH authentication was explicitly approved. Mirrors
/// the `InstallApproval` gate: the default keeps every connection fully
/// non-interactive, and only `Approved` enables the askpass channel.
// Consumed by the next stage's approved interactive retry entry points.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SshAuthApproval {
    /// Default: background connections stay fully non-interactive.
    NonInteractive,
    /// The caller obtained explicit approval to route SSH password and
    /// passphrase prompts through herdr's in-memory askpass channel.
    Approved,
}

/// Connection-time environment for the askpass channel: the helper program
/// (this herdr executable) and the channel socket.
#[derive(Clone, Debug)]
pub(crate) struct AskpassEnvironment {
    helper: PathBuf,
    socket: PathBuf,
}

impl AskpassEnvironment {
    #[cfg(test)]
    pub(crate) fn for_test(helper: PathBuf, socket: PathBuf) -> Self {
        Self { helper, socket }
    }

    /// Points ssh at the channel. `SSH_ASKPASS_REQUIRE=force` makes OpenSSH
    /// ≥ 8.4 always use the helper; detaching from the controlling terminal
    /// plus a fallback `DISPLAY` covers older releases that only consult the
    /// helper when no tty is available.
    pub(crate) fn apply(&self, command: &mut std::process::Command) {
        command
            .env("SSH_ASKPASS", &self.helper)
            .env("SSH_ASKPASS_REQUIRE", "force")
            .env(SSH_ASKPASS_SOCKET_ENV_VAR, &self.socket);
        if std::env::var_os("DISPLAY").is_none() {
            command.env("DISPLAY", "herdr-askpass:0");
        }
        crate::platform::detach_child_from_controlling_terminal(command);
    }
}

fn write_frame(writer: &mut impl io::Write, bytes: &[u8]) -> io::Result<()> {
    let len = u32::try_from(bytes.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "askpass frame is too large"))?;
    writer.write_all(&len.to_le_bytes())?;
    writer.write_all(bytes)
}

fn read_frame(reader: &mut impl io::Read, limit: u32) -> io::Result<Vec<u8>> {
    let mut len_bytes = [0_u8; 4];
    reader.read_exact(&mut len_bytes)?;
    let len = u32::from_le_bytes(len_bytes);
    if len > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "askpass frame exceeds its size limit",
        ));
    }
    let mut bytes = vec![0_u8; len as usize];
    reader.read_exact(&mut bytes)?;
    Ok(bytes)
}

/// Overwrites a secret's heap buffer before it is freed.
fn zeroize(secret: String) {
    let mut bytes = secret.into_bytes();
    bytes.iter_mut().for_each(|byte| *byte = 0);
}

/// Helper side of the channel: sends the prompt, waits for the answer.
/// `Ok(None)` means the prompt was declined or timed out.
pub(crate) fn askpass_roundtrip(socket: &Path, prompt: &str) -> io::Result<Option<String>> {
    let mut stream = crate::ipc::connect_local_stream(socket)?;
    write_frame(&mut stream, prompt.as_bytes())?;
    let response = read_frame(&mut stream, ASKPASS_RESPONSE_FRAME_LIMIT)?;
    if response.is_empty() {
        return Ok(None);
    }
    String::from_utf8(response)
        .map(Some)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "askpass response is not UTF-8"))
}

/// Entry point of the re-executed helper process: prints the answer on
/// stdout for ssh and exits nonzero when the prompt was declined. The answer
/// buffer is zeroed immediately after the write.
// Entry for the main.rs askpass wiring, which is part of the next stage.
#[allow(dead_code)]
pub(crate) fn run_ssh_askpass_helper(prompt_args: &[String]) -> io::Result<()> {
    let Some(socket) = std::env::var_os(SSH_ASKPASS_SOCKET_ENV_VAR) else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("{SSH_ASKPASS_SOCKET_ENV_VAR} is not set"),
        ));
    };
    run_ssh_askpass_helper_at(Path::new(&socket), prompt_args)
}

fn run_ssh_askpass_helper_at(socket: &Path, prompt_args: &[String]) -> io::Result<()> {
    let prompt = prompt_args.join(" ");
    match askpass_roundtrip(socket, &prompt)? {
        Some(answer) => {
            let mut stdout = io::stdout().lock();
            let result = stdout
                .write_all(answer.as_bytes())
                .and_then(|()| stdout.write_all(b"\n"))
                .and_then(|()| stdout.flush());
            zeroize(answer);
            result
        }
        None => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "SSH askpass prompt was declined",
        )),
    }
}

/// One prompt delivered by ssh, awaiting a programmatic answer.
// Delivered to the next stage's prompt consumer (wizard worker, then TUI).
#[allow(dead_code)]
pub(crate) struct SshAskpassPrompt {
    prompt: String,
    responder: mpsc::SyncSender<Option<String>>,
}

impl SshAskpassPrompt {
    /// The prompt text as ssh passed it to the helper, for example
    /// `user@host's password:` or `Enter passphrase for key '/home/dev/.ssh/id_ed25519':`.
    // See the struct above.
    #[allow(dead_code)]
    pub(crate) fn prompt(&self) -> &str {
        &self.prompt
    }

    /// Answers the prompt; `None` declines it (ssh retries or fails the
    /// authentication attempt). An answer that can no longer be delivered is
    /// zeroed instead of lingering in a dropped channel buffer.
    // See the struct above.
    #[allow(dead_code)]
    pub(crate) fn respond(self, answer: Option<String>) {
        if let Err(mpsc::SendError(mut answer)) = self.responder.send(answer) {
            if let Some(secret) = answer.take() {
                zeroize(secret);
            }
        }
    }
}

/// Receiving end of the askpass channel, consumed by the caller that
/// approved the interactive retry (a wizard worker today, the TUI later).
pub(crate) struct SshAskpassPrompts {
    receiver: mpsc::Receiver<SshAskpassPrompt>,
}

impl SshAskpassPrompts {
    // Blocking receive, for the next stage's consumer loop.
    #[allow(dead_code)]
    pub(crate) fn recv(&self) -> Option<SshAskpassPrompt> {
        self.receiver.recv().ok()
    }

    // Timeout-bounded receive, for the next stage's consumer loop.
    #[allow(dead_code)]
    pub(crate) fn recv_timeout(&self, timeout: Duration) -> Option<SshAskpassPrompt> {
        self.receiver.recv_timeout(timeout).ok()
    }
}

/// The channel server: a private local socket that accepts one prompt per
/// connection (ssh spawns the helper per prompt) and forwards it to the
/// [`SshAskpassPrompts`] consumer.
pub(crate) struct SshAskpassChannel {
    socket_path: PathBuf,
    socket_identity: crate::ipc::SocketFileIdentity,
    environment: AskpassEnvironment,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl SshAskpassChannel {
    // Started by the next stage's approved interactive retry (saved.rs entry
    // points are its only caller chain today).
    #[allow(dead_code)]
    pub(crate) fn start() -> io::Result<(Self, SshAskpassPrompts)> {
        let helper = std::env::current_exe()?;
        let pid = std::process::id();
        let sequence = NEXT_ASKPASS_CHANNEL.fetch_add(1, Ordering::Relaxed);
        let socket_path = crate::platform::remote_bridge_endpoint_path(
            &format!("herdr-askpass-{pid}-{sequence}.sock"),
            &format!("herdr-ap-{pid}-{sequence}.sock"),
        );
        crate::ipc::prepare_socket_path(&socket_path, |path| {
            format!(
                "SSH askpass channel is already listening at {}",
                path.display()
            )
        })?;
        let listener = crate::ipc::bind_private_local_listener(&socket_path)?;
        let socket_identity = crate::ipc::socket_file_identity(&socket_path)?;
        if let Err(error) =
            crate::ipc::restrict_socket_permissions(&socket_path, ASKPASS_SOCKET_PERMISSION_MODE)
        {
            let _ = crate::ipc::remove_socket_file_if_owned(&socket_path, &socket_identity);
            return Err(error);
        }
        if let Err(error) = listener.set_nonblocking(ListenerNonblockingMode::Accept) {
            let _ = crate::ipc::remove_socket_file_if_owned(&socket_path, &socket_identity);
            return Err(error);
        }

        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let (prompt_tx, prompt_rx) = mpsc::channel::<SshAskpassPrompt>();
        let thread = thread::spawn(move || {
            while !thread_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok(stream) => {
                        // Each prompt is served on its own thread: answering
                        // may wait for a human, and the accept loop (and the
                        // channel's Drop) must stay responsive meanwhile.
                        let connection_tx = prompt_tx.clone();
                        thread::spawn(move || {
                            if let Err(error) = serve_askpass_connection(stream, &connection_tx) {
                                tracing::debug!(%error, "SSH askpass request failed");
                            }
                        });
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(ASKPASS_ACCEPT_POLL);
                    }
                    Err(error) => {
                        tracing::debug!(%error, "SSH askpass listener failed");
                        break;
                    }
                }
            }
        });

        let environment = AskpassEnvironment {
            helper,
            socket: socket_path.clone(),
        };
        Ok((
            Self {
                socket_path,
                socket_identity,
                environment,
                stop,
                thread: Some(thread),
            },
            SshAskpassPrompts {
                receiver: prompt_rx,
            },
        ))
    }

    pub(crate) fn environment(&self) -> &AskpassEnvironment {
        &self.environment
    }
}

impl Drop for SshAskpassChannel {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = crate::ipc::remove_socket_file_if_owned(&self.socket_path, &self.socket_identity);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve_askpass_connection(
    mut stream: crate::ipc::LocalStream,
    prompt_tx: &mpsc::Sender<SshAskpassPrompt>,
) -> io::Result<()> {
    let prompt_bytes = read_frame(&mut stream, ASKPASS_PROMPT_FRAME_LIMIT)?;
    let prompt = String::from_utf8(prompt_bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "askpass prompt is not UTF-8"))?;
    let (responder_tx, responder_rx) = mpsc::sync_channel::<Option<String>>(1);
    if prompt_tx
        .send(SshAskpassPrompt {
            prompt,
            responder: responder_tx,
        })
        .is_err()
    {
        // Nobody consumes prompts: decline immediately so ssh fails fast.
        return write_frame(&mut stream, &[]);
    }
    match responder_rx.recv_timeout(ASKPASS_RESPONSE_TIMEOUT) {
        Ok(Some(secret)) => {
            let result = write_frame(&mut stream, secret.as_bytes());
            zeroize(secret);
            result
        }
        Ok(None) | Err(_) => write_frame(&mut stream, &[]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_and_enforce_limits() {
        let mut buffer = Vec::new();
        write_frame(&mut buffer, b"prompt").expect("write frame");
        write_frame(&mut buffer, &[]).expect("write empty frame");
        let mut cursor = io::Cursor::new(buffer);
        assert_eq!(
            read_frame(&mut cursor, 16).expect("read frame"),
            b"prompt".to_vec()
        );
        assert_eq!(
            read_frame(&mut cursor, 16).expect("read empty frame"),
            Vec::<u8>::new()
        );

        let mut oversized = Vec::new();
        oversized.extend_from_slice(&17_u32.to_le_bytes());
        oversized.extend_from_slice(&[0_u8; 17]);
        let mut cursor = io::Cursor::new(oversized);
        let error = read_frame(&mut cursor, 16).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn askpass_environment_points_ssh_at_the_channel() {
        let environment = AskpassEnvironment {
            helper: PathBuf::from("/usr/local/bin/herdr"),
            socket: PathBuf::from("/tmp/herdr-askpass-1.sock"),
        };
        let mut command = std::process::Command::new("ssh");
        environment.apply(&mut command);
        let envs = command
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(
            envs.get("SSH_ASKPASS").and_then(|value| value.as_deref()),
            Some("/usr/local/bin/herdr")
        );
        assert_eq!(
            envs.get("SSH_ASKPASS_REQUIRE")
                .and_then(|value| value.as_deref()),
            Some("force")
        );
        assert_eq!(
            envs.get(SSH_ASKPASS_SOCKET_ENV_VAR)
                .and_then(|value| value.as_deref()),
            Some("/tmp/herdr-askpass-1.sock")
        );
    }

    #[test]
    fn channel_delivers_prompts_and_returns_answers() {
        let (channel, prompts) = SshAskpassChannel::start().expect("start channel");
        let socket = channel.environment().socket.clone();

        let helper = thread::spawn(move || askpass_roundtrip(&socket, "user@host's password:"));
        let prompt = prompts
            .recv_timeout(Duration::from_secs(5))
            .expect("prompt arrives");
        assert_eq!(prompt.prompt(), "user@host's password:");
        prompt.respond(Some("s3cret".to_string()));
        let answer = helper.join().expect("helper thread").expect("roundtrip");
        assert_eq!(answer.as_deref(), Some("s3cret"));
    }

    #[test]
    fn helper_entry_prints_the_answer_for_ssh() {
        let (channel, prompts) = SshAskpassChannel::start().expect("start channel");
        let socket = channel.environment().socket.clone();

        let helper = thread::spawn(move || {
            run_ssh_askpass_helper_at(&socket, &["Enter passphrase for key".to_string()])
        });
        let prompt = prompts
            .recv_timeout(Duration::from_secs(5))
            .expect("prompt arrives");
        assert_eq!(prompt.prompt(), "Enter passphrase for key");
        prompt.respond(Some("hunter2".to_string()));
        helper.join().expect("helper thread").expect("helper entry");
    }

    #[test]
    fn dropping_the_channel_with_an_unanswered_prompt_is_bounded() {
        let (channel, prompts) = SshAskpassChannel::start().expect("start channel");
        let socket = channel.environment().socket.clone();
        let helper = thread::spawn(move || askpass_roundtrip(&socket, "waiting:"));
        let prompt = prompts
            .recv_timeout(Duration::from_secs(5))
            .expect("prompt arrives");
        let started = std::time::Instant::now();
        drop(channel);
        assert!(started.elapsed() < Duration::from_secs(2));
        // The outstanding prompt is still answerable; the detached serve
        // thread delivers it and exits.
        prompt.respond(Some("late".to_string()));
        let answer = helper.join().expect("helper thread").expect("roundtrip");
        assert_eq!(answer.as_deref(), Some("late"));
    }

    #[test]
    fn declined_prompts_and_missing_consumers_fail_the_helper() {
        let (channel, prompts) = SshAskpassChannel::start().expect("start channel");
        let socket = channel.environment().socket.clone();

        let helper = thread::spawn(move || askpass_roundtrip(&socket, "passphrase:"));
        let prompt = prompts
            .recv_timeout(Duration::from_secs(5))
            .expect("prompt arrives");
        prompt.respond(None);
        let answer = helper.join().expect("helper thread").expect("roundtrip");
        assert!(answer.is_none());

        drop(prompts);
        let answer = askpass_roundtrip(channel.environment().socket.as_path(), "again:")
            .expect("roundtrip without consumer");
        assert!(answer.is_none());
    }
}
