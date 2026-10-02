#[cfg(unix)]
mod unix;

mod admission;

#[cfg(unix)]
pub(crate) use unix::*;

#[cfg(windows)]
mod windows {
    use std::io::{Read, Write};
    use std::sync::{mpsc as std_mpsc, Arc, Mutex};
    use std::time::{Duration, Instant};

    use bytes::Bytes;
    use portable_pty::{MasterPty, PtySize};
    use tokio::sync::mpsc;
    use tracing::{debug, warn};

    use super::admission::{Admission, INPUT_ITEM_LIMIT};

    pub(crate) struct PtyReadResult {
        pub terminal_responses: Vec<Bytes>,
    }

    type ReadCallback = Box<dyn FnMut(&[u8]) -> PtyReadResult + Send + 'static>;
    type ReaderExitCallback = Box<dyn FnOnce() + Send + 'static>;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct PtyResize {
        rows: u16,
        cols: u16,
        cell_width_px: u32,
        cell_height_px: u32,
    }

    struct PtyResizeRequest {
        resize: PtyResize,
        terminal_responses: Vec<Bytes>,
    }

    pub(crate) struct PtyIoActorConfig {
        pub pane_id: u32,
        pub master: Box<dyn MasterPty + Send>,
        pub initially_quiesced: bool,
        pub on_read: ReadCallback,
        pub on_reader_exit: Option<ReaderExitCallback>,
    }

    enum PtyIoDataCommand {
        WriteUserInput(Bytes),
        SubmitUserInput {
            text: Bytes,
            enter: Bytes,
            delay: Duration,
            deadline: Option<Instant>,
            reply: std_mpsc::Sender<std::io::Result<()>>,
        },
        Shutdown,
    }

    enum PtyIoWriteCommand {
        Write(Bytes),
        SubmissionPart {
            bytes: Bytes,
            deadline: Option<Instant>,
            reply: std_mpsc::Sender<std::io::Result<()>>,
        },
    }

    enum PtyIoControlCommand {
        Resize,
        Shutdown,
    }

    #[derive(Clone)]
    pub(crate) struct PtyIoActorHandle {
        data_tx: mpsc::Sender<PtyIoDataCommand>,
        control_tx: std_mpsc::Sender<PtyIoControlCommand>,
        write_tx: std_mpsc::Sender<PtyIoWriteCommand>,
        response_order: Arc<Mutex<()>>,
        accepting: Arc<Mutex<bool>>,
        input_admission: Arc<Admission>,
        response_admission: Arc<Admission>,
        pending_resize: Arc<Mutex<Option<PtyResizeRequest>>>,
    }

    impl PtyIoActorHandle {
        pub(crate) fn try_write_user_input(
            &self,
            bytes: Bytes,
        ) -> Result<(), mpsc::error::TrySendError<Bytes>> {
            let accepting = self
                .accepting
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if !*accepting || self.data_tx.is_closed() {
                return Err(mpsc::error::TrySendError::Closed(bytes));
            }
            let Some(permit) = self.input_admission.try_reserve(bytes.len()) else {
                return Err(mpsc::error::TrySendError::Full(bytes));
            };
            self.data_tx
                .try_send(PtyIoDataCommand::WriteUserInput(permit.wrap(bytes.clone())))
                .map_err(|err| match err {
                    mpsc::error::TrySendError::Full(_) => mpsc::error::TrySendError::Full(bytes),
                    mpsc::error::TrySendError::Closed(_) => {
                        mpsc::error::TrySendError::Closed(bytes)
                    }
                })
        }

        pub(crate) fn queue_user_input_submission(
            &self,
            text: Bytes,
            enter: Bytes,
            delay: Duration,
            deadline: Option<Instant>,
        ) -> std::io::Result<std_mpsc::Receiver<std::io::Result<()>>> {
            let accepting = self
                .accepting
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if !*accepting || self.data_tx.is_closed() {
                return Err(pty_actor_closed());
            }
            let permit = text
                .len()
                .checked_add(enter.len())
                .and_then(|bytes| self.input_admission.try_reserve(bytes))
                .ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::WouldBlock,
                        "pty input item or byte budget is full",
                    )
                })?;
            let (reply_tx, reply_rx) = std_mpsc::channel();
            self.data_tx
                .try_send(PtyIoDataCommand::SubmitUserInput {
                    text: permit.wrap(text),
                    enter: permit.wrap(enter),
                    delay,
                    deadline,
                    reply: reply_tx,
                })
                .map_err(|err| match err {
                    mpsc::error::TrySendError::Full(_) => std::io::Error::new(
                        std::io::ErrorKind::WouldBlock,
                        "pty input queue is full",
                    ),
                    mpsc::error::TrySendError::Closed(_) => pty_actor_closed(),
                })?;
            Ok(reply_rx)
        }

        pub(crate) fn write_terminal_response(&self, response: impl FnOnce() -> Option<Bytes>) {
            let _order = self
                .response_order
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(bytes) =
                response().and_then(|bytes| self.response_admission.admit_response(bytes))
            {
                let _ = self.write_tx.send(PtyIoWriteCommand::Write(bytes));
            }
        }

        pub(crate) fn resize(
            &self,
            rows: u16,
            cols: u16,
            cell_width_px: u32,
            cell_height_px: u32,
            terminal_responses: Vec<Bytes>,
        ) {
            let mut pending = self
                .pending_resize
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let notify = pending.is_none();
            pending.take();
            *pending = Some(PtyResizeRequest {
                resize: PtyResize {
                    rows,
                    cols,
                    cell_width_px,
                    cell_height_px,
                },
                terminal_responses: terminal_responses
                    .into_iter()
                    .filter_map(|bytes| self.response_admission.admit_response(bytes))
                    .collect(),
            });
            if notify && self.control_tx.send(PtyIoControlCommand::Resize).is_err() {
                pending.take();
            }
        }

        pub(crate) fn shutdown(&self) {
            self.input_admission.close();
            *self
                .accepting
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = false;
            let _ = self.data_tx.try_send(PtyIoDataCommand::Shutdown);
            self.pending_resize
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take();
            let _ = self.control_tx.send(PtyIoControlCommand::Shutdown);
        }
    }

    pub(crate) struct PtyIoActor;

    impl PtyIoActor {
        pub(crate) fn spawn(config: PtyIoActorConfig) -> std::io::Result<PtyIoActorHandle> {
            let PtyIoActorConfig {
                pane_id,
                master,
                initially_quiesced,
                mut on_read,
                on_reader_exit,
            } = config;

            let mut reader = master
                .try_clone_reader()
                .map_err(|err| std::io::Error::other(err.to_string()))?;
            let mut writer = master
                .take_writer()
                .map_err(|err| std::io::Error::other(err.to_string()))?;
            let (data_tx, mut data_rx) = mpsc::channel::<PtyIoDataCommand>(INPUT_ITEM_LIMIT);
            let (control_tx, control_rx) = std_mpsc::channel::<PtyIoControlCommand>();
            let (write_tx, write_rx) = std_mpsc::channel::<PtyIoWriteCommand>();
            let response_order = Arc::new(Mutex::new(()));
            let accepting = Arc::new(Mutex::new(!initially_quiesced));
            let input_admission = Admission::input();
            let response_admission = Admission::responses();
            let pending_resize = Arc::new(Mutex::new(None::<PtyResizeRequest>));

            {
                let accepting = Arc::clone(&accepting);
                let input = data_tx.downgrade();
                let control = control_tx.clone();
                let responses = Arc::clone(&response_admission);
                let input_admission = Arc::clone(&input_admission);
                std::thread::spawn(move || {
                    run_writer(&mut writer, write_rx);
                    input_admission.close();
                    *accepting
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) = false;
                    responses.close();
                    if let Some(input) = input.upgrade() {
                        let _ = input.try_send(PtyIoDataCommand::Shutdown);
                    }
                    let _ = control.send(PtyIoControlCommand::Shutdown);
                    debug!(pane_id, "windows pty writer thread exiting");
                });
            }

            {
                let write_tx = write_tx.clone();
                let accepting = Arc::clone(&accepting);
                std::thread::spawn(move || {
                    run_input_forwarder(&mut data_rx, write_tx, accepting);
                    debug!(pane_id, "windows pty input thread exiting");
                });
            }

            {
                let write_tx = write_tx.clone();
                let response_order = Arc::clone(&response_order);
                let responses = Arc::clone(&response_admission);
                let input_admission = Arc::clone(&input_admission);
                let accepting = Arc::clone(&accepting);
                let input = data_tx.downgrade();
                let control = control_tx.clone();
                std::thread::spawn(move || {
                    let mut buf = [0u8; 8192];
                    loop {
                        match reader.read(&mut buf) {
                            Ok(0) => break,
                            Ok(n) => {
                                let _order = response_order
                                    .lock()
                                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                                let result = on_read(&buf[..n]);
                                if result
                                    .terminal_responses
                                    .into_iter()
                                    .filter_map(|bytes| responses.admit_response(bytes))
                                    .any(|response| {
                                        write_tx.send(PtyIoWriteCommand::Write(response)).is_err()
                                    })
                                {
                                    break;
                                }
                            }
                            Err(err) => {
                                debug!(pane_id, err = %err, "windows pty reader failed");
                                break;
                            }
                        }
                    }
                    input_admission.close();
                    *accepting
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) = false;
                    responses.close();
                    if let Some(input) = input.upgrade() {
                        let _ = input.try_send(PtyIoDataCommand::Shutdown);
                    }
                    let _ = control.send(PtyIoControlCommand::Shutdown);
                    if let Some(on_reader_exit) = on_reader_exit {
                        on_reader_exit();
                    }
                    debug!(pane_id, "windows pty reader thread exiting");
                });
            }

            {
                let write_tx = write_tx.clone();
                let pending_resize = Arc::clone(&pending_resize);
                std::thread::spawn(move || {
                    for command in control_rx {
                        match command {
                            PtyIoControlCommand::Resize => {
                                let request = pending_resize
                                    .lock()
                                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                                    .take();
                                let Some(request) = request else {
                                    continue;
                                };
                                let size = request.resize;
                                if let Err(err) = master.resize(PtySize {
                                    rows: size.rows,
                                    cols: size.cols,
                                    pixel_width: size.cell_width_px.min(u16::MAX as u32) as u16,
                                    pixel_height: size.cell_height_px.min(u16::MAX as u32) as u16,
                                }) {
                                    warn!(pane_id, err = %err, "windows pty resize failed");
                                }
                                if request.terminal_responses.into_iter().any(|response| {
                                    write_tx.send(PtyIoWriteCommand::Write(response)).is_err()
                                }) {
                                    break;
                                }
                            }
                            PtyIoControlCommand::Shutdown => break,
                        }
                    }
                    pending_resize
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .take();
                    debug!(pane_id, "windows pty control thread exiting");
                });
            }

            Ok(PtyIoActorHandle {
                data_tx,
                control_tx,
                write_tx,
                response_order,
                accepting,
                input_admission,
                response_admission,
                pending_resize,
            })
        }
    }

    fn run_writer(writer: &mut impl Write, write_rx: std_mpsc::Receiver<PtyIoWriteCommand>) {
        for command in write_rx {
            let result = match command {
                PtyIoWriteCommand::Write(bytes) => write_and_flush(writer, &bytes),
                PtyIoWriteCommand::SubmissionPart {
                    bytes,
                    deadline,
                    reply,
                } => {
                    let result = if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                        Err(input_submission_timed_out())
                    } else {
                        write_and_flush(writer, &bytes)
                    };
                    drop(bytes);
                    let failed = result
                        .as_ref()
                        .is_err_and(|err| err.kind() != std::io::ErrorKind::TimedOut);
                    let _ = reply.send(result);
                    if failed {
                        break;
                    }
                    continue;
                }
            };
            if result.is_err() {
                break;
            }
        }
    }

    fn reject_queued_input(command: PtyIoDataCommand) {
        if let PtyIoDataCommand::SubmitUserInput {
            text, enter, reply, ..
        } = command
        {
            drop((text, enter));
            let _ = reply.send(Err(pty_actor_closed()));
        }
    }

    fn run_input_forwarder(
        data_rx: &mut mpsc::Receiver<PtyIoDataCommand>,
        write_tx: std_mpsc::Sender<PtyIoWriteCommand>,
        accepting: Arc<Mutex<bool>>,
    ) {
        while let Some(command) = data_rx.blocking_recv() {
            if !*accepting
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
            {
                reject_queued_input(command);
                break;
            }
            match command {
                PtyIoDataCommand::WriteUserInput(bytes) => {
                    if write_tx.send(PtyIoWriteCommand::Write(bytes)).is_err() {
                        break;
                    }
                }
                PtyIoDataCommand::SubmitUserInput {
                    text,
                    enter,
                    delay,
                    deadline,
                    reply,
                } => {
                    let result = if deadline.is_some_and(|deadline| {
                        deadline.saturating_duration_since(Instant::now()) <= delay
                    }) {
                        drop((text, enter));
                        Err(input_submission_timed_out())
                    } else {
                        let text_deadline =
                            deadline.and_then(|deadline| deadline.checked_sub(delay));
                        write_submission_part(&write_tx, text, text_deadline).and_then(|()| {
                            // A started text write is committed even if shutdown races the delay.
                            std::thread::sleep(delay);
                            let completion = enqueue_submission_part(&write_tx, enter, None)?;
                            completion
                                .recv()
                                .unwrap_or_else(|_| Err(pty_actor_closed()))
                        })
                    };
                    let failed = result
                        .as_ref()
                        .is_err_and(|err| err.kind() != std::io::ErrorKind::TimedOut);
                    let _ = reply.send(result);
                    if failed {
                        break;
                    }
                }
                PtyIoDataCommand::Shutdown => break,
            }
        }
        data_rx.close();
        while let Ok(command) = data_rx.try_recv() {
            reject_queued_input(command);
        }
    }

    fn enqueue_submission_part(
        write_tx: &std_mpsc::Sender<PtyIoWriteCommand>,
        bytes: Bytes,
        deadline: Option<Instant>,
    ) -> std::io::Result<std_mpsc::Receiver<std::io::Result<()>>> {
        let (reply, completion) = std_mpsc::channel();
        write_tx
            .send(PtyIoWriteCommand::SubmissionPart {
                bytes,
                deadline,
                reply,
            })
            .map_err(|_| pty_actor_closed())?;
        Ok(completion)
    }

    fn write_submission_part(
        write_tx: &std_mpsc::Sender<PtyIoWriteCommand>,
        bytes: Bytes,
        deadline: Option<Instant>,
    ) -> std::io::Result<()> {
        enqueue_submission_part(write_tx, bytes, deadline)?
            .recv()
            .unwrap_or_else(|_| Err(pty_actor_closed()))
    }

    fn pty_actor_closed() -> std::io::Error {
        std::io::Error::new(std::io::ErrorKind::BrokenPipe, "pty actor closed")
    }

    fn input_submission_timed_out() -> std::io::Error {
        std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "agent prompt timed out before input submission",
        )
    }

    fn write_and_flush(writer: &mut impl Write, bytes: &[u8]) -> std::io::Result<()> {
        writer.write_all(bytes)?;
        writer.flush()
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        struct RecordingWriter {
            writes: Vec<(Vec<u8>, Instant)>,
            flushes: Vec<Instant>,
            fail_after: Option<usize>,
            flushed: std_mpsc::Sender<()>,
        }

        impl Write for RecordingWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if self.fail_after == Some(self.writes.len()) {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "writer closed",
                    ));
                }
                self.writes.push((bytes.to_vec(), Instant::now()));
                Ok(bytes.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                self.flushes.push(Instant::now());
                let _ = self.flushed.send(());
                Ok(())
            }
        }

        struct GatedWriter {
            recorded: RecordingWriter,
            blocked_bytes: Bytes,
            entered: std_mpsc::Sender<()>,
            release: Option<std_mpsc::Receiver<()>>,
        }

        impl Write for GatedWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if bytes == self.blocked_bytes.as_ref() {
                    if let Some(release) = self.release.take() {
                        let _ = self.entered.send(());
                        let _ = release.recv();
                    }
                }
                self.recorded.write(bytes)
            }

            fn flush(&mut self) -> std::io::Result<()> {
                self.recorded.flush()
            }
        }

        struct GatedWriterActor {
            handle: Option<PtyIoActorHandle>,
            entered: std_mpsc::Receiver<()>,
            control_rx: std_mpsc::Receiver<PtyIoControlCommand>,
            release: Option<std_mpsc::Sender<()>>,
            input_thread: Option<std::thread::JoinHandle<()>>,
            writer_thread: Option<std::thread::JoinHandle<RecordingWriter>>,
            callers: Vec<std::thread::JoinHandle<()>>,
        }

        impl GatedWriterActor {
            fn new(blocked_bytes: Bytes) -> Self {
                let (data_tx, mut data_rx) = mpsc::channel(1024);
                let (control_tx, control_rx) = std_mpsc::channel();
                let (write_tx, write_rx) = std_mpsc::channel();
                let (entered_tx, entered) = std_mpsc::channel();
                let (release, release_rx) = std_mpsc::channel();
                let (flushed, _flushed_rx) = std_mpsc::channel();
                let accepting = Arc::new(Mutex::new(true));
                let input_write_tx = write_tx.clone();
                let input_accepting = Arc::clone(&accepting);
                let mut actor = Self {
                    handle: Some(PtyIoActorHandle {
                        data_tx,
                        control_tx,
                        write_tx,
                        response_order: Arc::new(Mutex::new(())),
                        accepting,
                        input_admission: Admission::input(),
                        response_admission: Admission::responses(),
                        pending_resize: Arc::new(Mutex::new(None)),
                    }),
                    entered,
                    control_rx,
                    release: Some(release),
                    input_thread: None,
                    writer_thread: None,
                    callers: Vec::new(),
                };
                let mut writer = GatedWriter {
                    recorded: RecordingWriter {
                        writes: Vec::new(),
                        flushes: Vec::new(),
                        fail_after: None,
                        flushed,
                    },
                    blocked_bytes,
                    entered: entered_tx,
                    release: Some(release_rx),
                };
                actor.writer_thread = Some(std::thread::spawn(move || {
                    run_writer(&mut writer, write_rx);
                    writer.recorded
                }));
                actor.input_thread = Some(std::thread::spawn(move || {
                    run_input_forwarder(&mut data_rx, input_write_tx, input_accepting);
                }));
                actor
            }

            fn unblock(&mut self) {
                if let Some(release) = self.release.take() {
                    let _ = release.send(());
                }
            }

            fn finish(mut self) -> RecordingWriter {
                self.unblock();
                while let Some(caller) = self.callers.pop() {
                    caller.join().expect("handle caller joins");
                }
                drop(self.handle.take());
                self.input_thread
                    .take()
                    .expect("input thread exists")
                    .join()
                    .expect("input thread joins");
                self.writer_thread
                    .take()
                    .expect("writer thread exists")
                    .join()
                    .expect("writer thread joins")
            }
        }

        impl Drop for GatedWriterActor {
            fn drop(&mut self) {
                self.unblock();
                while let Some(caller) = self.callers.pop() {
                    let _ = caller.join();
                }
                drop(self.handle.take());
                if let Some(input) = self.input_thread.take() {
                    let _ = input.join();
                }
                if let Some(writer) = self.writer_thread.take() {
                    let _ = writer.join();
                }
            }
        }

        #[test]
        fn enter_backpressure_does_not_block_input_acceptance_or_shutdown() {
            let mut actor = GatedWriterActor::new(Bytes::from_static(b"\r"));
            let completion = actor
                .handle
                .as_ref()
                .unwrap()
                .queue_user_input_submission(
                    Bytes::from_static(b"prompt"),
                    Bytes::from_static(b"\r"),
                    Duration::ZERO,
                    None,
                )
                .expect("submission queues");
            actor
                .entered
                .recv_timeout(Duration::from_secs(2))
                .expect("writer is blocked on Enter");

            let input_handle = actor.handle.as_ref().unwrap().clone();
            let (input_tx, input_rx) = std_mpsc::channel();
            actor.callers.push(std::thread::spawn(move || {
                let _ =
                    input_tx.send(input_handle.try_write_user_input(Bytes::from_static(b"user")));
            }));
            let input = input_rx.recv_timeout(Duration::from_secs(2));

            let shutdown_handle = actor.handle.as_ref().unwrap().clone();
            let (shutdown_tx, shutdown_rx) = std_mpsc::channel();
            actor.callers.push(std::thread::spawn(move || {
                shutdown_handle.shutdown();
                let _ = shutdown_tx
                    .send(shutdown_handle.try_write_user_input(Bytes::from_static(b"late")));
            }));
            let shutdown = shutdown_rx.recv_timeout(Duration::from_secs(2));
            let control = actor.control_rx.recv_timeout(Duration::from_secs(2));
            let writer = actor.finish();
            let _ = completion
                .recv_timeout(Duration::from_secs(2))
                .expect("submission is resolved after releasing Enter");
            assert_eq!(writer.writes[0].0, b"prompt");
            assert_eq!(writer.writes[1].0, b"\r");
            assert!(
                matches!(input, Ok(Ok(()))),
                "input acceptance must not wait for the blocked Enter write: {input:?}"
            );
            assert!(
                matches!(
                    shutdown,
                    Ok(Err(mpsc::error::TrySendError::Closed(ref bytes))) if bytes.as_ref() == b"late"
                ),
                "shutdown must close admission without waiting for Enter: {shutdown:?}"
            );
            assert!(matches!(control, Ok(PtyIoControlCommand::Shutdown)));
        }

        #[test]
        fn input_capacity_is_not_released_by_forwarding_to_a_blocked_writer() {
            let actor = GatedWriterActor::new(Bytes::from_static(b"input"));
            let handle = actor.handle.as_ref().unwrap().clone();
            let capacity = handle.data_tx.max_capacity();
            handle
                .try_write_user_input(Bytes::from_static(b"input"))
                .expect("first input queues");
            actor
                .entered
                .recv_timeout(Duration::from_secs(2))
                .expect("writer is blocked before completing any input");
            for _ in 1..capacity {
                handle
                    .try_write_user_input(Bytes::from_static(b"input"))
                    .expect("input within the capacity queues");
            }
            let expired = handle.queue_user_input_submission(
                Bytes::from_static(b"expired"),
                Bytes::from_static(b"\r"),
                Duration::ZERO,
                Some(Instant::now()),
            );
            assert_eq!(
                expired
                    .expect_err("all input permits remain occupied while writer is blocked")
                    .kind(),
                std::io::ErrorKind::WouldBlock
            );
            let attempts = [
                handle.try_write_user_input(Bytes::from_static(b"extra")),
                handle.try_write_user_input(Bytes::from_static(b"extra")),
            ];
            let writer = {
                drop(handle);
                actor.finish()
            };
            assert!(attempts
                .iter()
                .all(|attempt| matches!(attempt, Err(mpsc::error::TrySendError::Full(_)))));
            assert_eq!(writer.writes.len(), capacity);
        }

        #[test]
        fn input_byte_limit_rejects_without_losing_payload_or_leaking_permits() {
            let mut actor = GatedWriterActor::new(Bytes::from_static(b"held"));
            let budget = Admission::new(INPUT_ITEM_LIMIT, 6);
            actor.handle.as_mut().unwrap().input_admission = Arc::clone(&budget);
            let handle = actor.handle.as_ref().unwrap().clone();
            handle
                .try_write_user_input(Bytes::from_static(b"held"))
                .unwrap();
            actor.entered.recv_timeout(Duration::from_secs(2)).unwrap();
            let rejected = handle.try_write_user_input(Bytes::from_static(b"new"));
            assert!(
                matches!(rejected, Err(mpsc::error::TrySendError::Full(ref bytes)) if bytes.as_ref() == b"new")
            );
            let submission = handle
                .queue_user_input_submission(
                    Bytes::from_static(b"xy"),
                    Bytes::from_static(b"\r"),
                    Duration::ZERO,
                    None,
                )
                .unwrap_err();
            assert_eq!(submission.kind(), std::io::ErrorKind::WouldBlock);
            assert_eq!(budget.in_use(), (1, 4));
            handle
                .try_write_user_input(Bytes::from_static(b"ok"))
                .unwrap();
            assert_eq!(budget.in_use(), (2, 6));
            let writer = {
                drop(handle);
                actor.finish()
            };
            assert_eq!(writer.writes.len(), 2);
            assert_eq!(budget.in_use(), (0, 0));
        }

        #[test]
        fn abandoned_submission_still_commits_enter_before_following_input() {
            let actor = GatedWriterActor::new(Bytes::from_static(b"prompt"));
            let handle = actor.handle.as_ref().unwrap().clone();
            let budget = Arc::clone(&handle.input_admission);
            let completion = handle
                .queue_user_input_submission(
                    Bytes::from_static(b"prompt"),
                    Bytes::from_static(b"\r"),
                    Duration::ZERO,
                    None,
                )
                .unwrap();
            actor.entered.recv_timeout(Duration::from_secs(2)).unwrap();
            drop(completion);
            handle
                .try_write_user_input(Bytes::from_static(b"user"))
                .unwrap();
            assert_eq!(budget.in_use(), (2, 11));
            let writer = {
                drop(handle);
                actor.finish()
            };
            assert_eq!(
                writer
                    .writes
                    .iter()
                    .map(|write| write.0.as_slice())
                    .collect::<Vec<_>>(),
                vec![b"prompt".as_slice(), b"\r".as_slice(), b"user".as_slice()]
            );
            assert_eq!(budget.in_use(), (0, 0));
        }

        #[test]
        fn partial_write_failure_releases_current_and_buffered_permits() {
            struct PartialFailureWriter {
                budget: Arc<Admission>,
                calls: usize,
            }
            impl Write for PartialFailureWriter {
                fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                    assert_eq!(self.budget.in_use(), (2, 12));
                    self.calls += 1;
                    if self.calls == 1 {
                        assert_eq!(bytes, b"prompt");
                        Ok(2)
                    } else {
                        assert_eq!(bytes, b"ompt");
                        Err(std::io::Error::new(
                            std::io::ErrorKind::BrokenPipe,
                            "partial write failed",
                        ))
                    }
                }
                fn flush(&mut self) -> std::io::Result<()> {
                    Ok(())
                }
            }
            let budget = Admission::new(2, 12);
            let (write_tx, write_rx) = std_mpsc::channel();
            let (reply, completion) = std_mpsc::channel();
            let first = budget.try_reserve(6).unwrap();
            let second = budget.try_reserve(6).unwrap();
            write_tx
                .send(PtyIoWriteCommand::SubmissionPart {
                    bytes: first.wrap(Bytes::from_static(b"prompt")),
                    deadline: None,
                    reply,
                })
                .unwrap();
            write_tx
                .send(PtyIoWriteCommand::Write(
                    second.wrap(Bytes::from_static(b"queued")),
                ))
                .unwrap();
            drop((first, second, write_tx));
            let mut writer = PartialFailureWriter {
                budget: Arc::clone(&budget),
                calls: 0,
            };
            run_writer(&mut writer, write_rx);
            assert_eq!(writer.calls, 2);
            assert_eq!(
                completion
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap()
                    .unwrap_err()
                    .kind(),
                std::io::ErrorKind::BrokenPipe
            );
            assert_eq!(budget.in_use(), (0, 0));
        }

        #[test]
        fn response_backpressure_is_bounded_without_consuming_input_capacity() {
            let mut actor = GatedWriterActor::new(Bytes::from_static(b"held"));
            let responses = Admission::new(2, 6);
            actor.handle.as_mut().unwrap().response_admission = Arc::clone(&responses);
            let handle = actor.handle.as_ref().unwrap().clone();
            handle.write_terminal_response(|| Some(Bytes::from_static(b"held")));
            actor.entered.recv_timeout(Duration::from_secs(2)).unwrap();
            handle.write_terminal_response(|| Some(Bytes::from_static(b"ok")));
            handle.write_terminal_response(|| Some(Bytes::from_static(b"dropped")));
            assert_eq!(responses.in_use(), (2, 6));
            handle
                .try_write_user_input(Bytes::from_static(b"user"))
                .unwrap();
            let inputs = Arc::clone(&handle.input_admission);
            assert_eq!(inputs.in_use(), (1, 4));
            let writer = {
                drop(handle);
                actor.finish()
            };
            assert_eq!(
                writer
                    .writes
                    .iter()
                    .map(|write| write.0.as_slice())
                    .collect::<Vec<_>>(),
                vec![b"held".as_slice(), b"ok".as_slice(), b"user".as_slice()]
            );
            assert_eq!(responses.in_use(), (0, 0));
            assert_eq!(inputs.in_use(), (0, 0));
        }

        #[test]
        fn resize_backlog_coalesces_and_releases_replaced_response_permits() {
            let actor = GatedWriterActor::new(Bytes::from_static(b"unused"));
            let handle = actor.handle.as_ref().unwrap().clone();
            let responses = Arc::clone(&handle.response_admission);
            for rows in 1..=1024 {
                handle.resize(rows, 80, 8, 16, vec![Bytes::from_static(b"size")]);
            }
            assert!(matches!(
                actor.control_rx.try_recv(),
                Ok(PtyIoControlCommand::Resize)
            ));
            assert!(matches!(
                actor.control_rx.try_recv(),
                Err(std_mpsc::TryRecvError::Empty)
            ));
            assert_eq!(
                handle
                    .pending_resize
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .resize
                    .rows,
                1024
            );
            assert_eq!(responses.in_use(), (1, 4));
            drop(handle);
            actor.finish();
            assert_eq!(responses.in_use(), (0, 0));
        }

        fn run_recorded_submission(
            fail_after: Option<usize>,
            delay: Duration,
            deadline: Option<Instant>,
            during_delay: impl FnOnce(&std_mpsc::Sender<PtyIoWriteCommand>, &Arc<Mutex<bool>>),
        ) -> (RecordingWriter, std::io::Result<()>) {
            let (flushed_tx, flushed_rx) = std_mpsc::channel();
            let mut writer = RecordingWriter {
                writes: Vec::new(),
                flushes: Vec::new(),
                fail_after,
                flushed: flushed_tx,
            };
            let (data_tx, mut data_rx) = mpsc::channel(2);
            let (write_tx, write_rx) = std_mpsc::channel();
            let (reply_tx, reply_rx) = std_mpsc::channel();
            let accepting = Arc::new(Mutex::new(true));
            data_tx
                .try_send(PtyIoDataCommand::SubmitUserInput {
                    text: Bytes::from_static(b"prompt"),
                    enter: Bytes::from_static(b"\r"),
                    delay,
                    deadline,
                    reply: reply_tx,
                })
                .unwrap();
            data_tx
                .try_send(PtyIoDataCommand::WriteUserInput(Bytes::from_static(
                    b"user",
                )))
                .unwrap();
            let writer_thread = std::thread::spawn(move || {
                run_writer(&mut writer, write_rx);
                writer
            });
            let input_write_tx = write_tx.clone();
            let input_accepting = Arc::clone(&accepting);
            let input_thread = std::thread::spawn(move || {
                run_input_forwarder(&mut data_rx, input_write_tx, input_accepting)
            });
            flushed_rx.recv().expect("prompt was flushed");
            during_delay(&write_tx, &accepting);
            let result = reply_rx.recv().expect("writer reports submission");
            drop(data_tx);
            input_thread.join().expect("input thread joins");
            drop(write_tx);
            (writer_thread.join().expect("writer thread joins"), result)
        }

        #[test]
        fn submission_sequences_user_input_but_allows_terminal_responses() {
            let delay = Duration::from_millis(30);
            let (writer, result) = run_recorded_submission(None, delay, None, |write_tx, _| {
                write_tx
                    .send(PtyIoWriteCommand::Write(Bytes::from_static(b"response")))
                    .unwrap();
            });
            result.expect("submission succeeds");

            assert_eq!(writer.writes[0].0, b"prompt");
            assert_eq!(writer.writes[1].0, b"response");
            assert_eq!(writer.writes[2].0, b"\r");
            assert_eq!(writer.writes[3].0, b"user");
            assert!(writer.writes[2].1.duration_since(writer.flushes[0]) >= delay);
        }

        #[test]
        fn submission_returns_enter_write_failure() {
            let (_writer, result) =
                run_recorded_submission(Some(1), Duration::ZERO, None, |_, _| {});
            let err = result.expect_err("enter failure reaches caller");

            assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
        }

        #[test]
        fn shutdown_during_submission_delay_still_finishes_enter() {
            let (writer, result) =
                run_recorded_submission(None, Duration::from_millis(30), None, |_, accepting| {
                    *accepting.lock().unwrap() = false;
                });
            result.expect("started submission keeps its Enter commitment");
            assert_eq!(
                writer
                    .writes
                    .iter()
                    .map(|write| write.0.as_slice())
                    .collect::<Vec<_>>(),
                vec![b"prompt".as_slice(), b"\r".as_slice()]
            );
        }

        #[test]
        fn expired_queued_submission_is_not_written() {
            let (flushed_tx, _flushed_rx) = std_mpsc::channel();
            let mut writer = RecordingWriter {
                writes: Vec::new(),
                flushes: Vec::new(),
                fail_after: None,
                flushed: flushed_tx,
            };
            let (data_tx, mut data_rx) = mpsc::channel(2);
            let (write_tx, write_rx) = std_mpsc::channel();
            let (first_reply_tx, first_reply_rx) = std_mpsc::channel();
            let (expired_reply_tx, expired_reply_rx) = std_mpsc::channel();
            let accepting = Arc::new(Mutex::new(true));
            data_tx
                .try_send(PtyIoDataCommand::SubmitUserInput {
                    text: Bytes::from_static(b"first"),
                    enter: Bytes::from_static(b"\r"),
                    delay: Duration::from_millis(30),
                    deadline: None,
                    reply: first_reply_tx,
                })
                .unwrap();
            data_tx
                .try_send(PtyIoDataCommand::SubmitUserInput {
                    text: Bytes::from_static(b"expired"),
                    enter: Bytes::from_static(b"\r"),
                    delay: Duration::ZERO,
                    deadline: Some(Instant::now() + Duration::from_millis(10)),
                    reply: expired_reply_tx,
                })
                .unwrap();

            let writer_thread = std::thread::spawn(move || {
                run_writer(&mut writer, write_rx);
                writer
            });
            let input_write_tx = write_tx.clone();
            let input_thread = std::thread::spawn(move || {
                run_input_forwarder(&mut data_rx, input_write_tx, accepting)
            });
            first_reply_rx.recv().unwrap().unwrap();
            let err = expired_reply_rx.recv().unwrap().unwrap_err();
            assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);

            drop(data_tx);
            input_thread.join().unwrap();
            drop(write_tx);
            let writer = writer_thread.join().unwrap();
            assert_eq!(
                writer
                    .writes
                    .iter()
                    .map(|write| write.0.as_slice())
                    .collect::<Vec<_>>(),
                vec![b"first".as_slice(), b"\r".as_slice()]
            );
        }
    }
}

#[cfg(windows)]
pub(crate) use windows::*;
