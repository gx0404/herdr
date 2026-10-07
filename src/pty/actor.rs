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
    use tokio::sync::{mpsc, oneshot};
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
            reply: oneshot::Sender<std::io::Result<()>>,
            enter_completion: Option<std_mpsc::Sender<()>>,
        },
    }

    const INPUT_SHUTDOWN_GRACE: Duration = Duration::from_millis(250);
    pub(crate) const PTY_CLOSE_TIMEOUT: Duration = Duration::from_millis(500);

    struct InputAcceptance {
        accepting: bool,
        enter_completion: Option<std_mpsc::Receiver<()>>,
        shutdown_deadline: Option<Instant>,
        closed_at: Option<Instant>,
    }

    impl InputAcceptance {
        fn close(&mut self) -> (bool, Instant) {
            self.accepting = false;
            if let Some(deadline) = self.shutdown_deadline {
                return (false, deadline);
            }
            let deadline = Instant::now() + INPUT_SHUTDOWN_GRACE;
            self.shutdown_deadline = Some(deadline);
            (true, deadline)
        }
    }

    #[derive(Clone)]
    pub(crate) struct PtyCloseCompletion {
        accepting: Arc<Mutex<InputAcceptance>>,
        deadline: Instant,
    }

    impl PtyCloseCompletion {
        pub(crate) fn deadline(&self) -> Instant {
            self.deadline
        }

        pub(crate) fn ready_at(&self, now: Instant) -> Option<Instant> {
            let closed_at = self
                .accepting
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .closed_at;
            if closed_at.is_some() {
                return closed_at;
            }
            if now >= self.deadline {
                warn!("PTY close completion timed out; starting process termination grace");
                return Some(self.deadline);
            }
            None
        }
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
        accepting: Arc<Mutex<InputAcceptance>>,
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
            if !accepting.accepting || self.data_tx.is_closed() {
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
            if !accepting.accepting || self.data_tx.is_closed() {
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

        pub(crate) fn shutdown(&self) -> PtyCloseCompletion {
            self.input_admission.close();
            let (first_shutdown, input_deadline) = self
                .accepting
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .close();
            self.pending_resize
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take();
            if first_shutdown {
                let _ = self.data_tx.try_send(PtyIoDataCommand::Shutdown);
                let _ = self.control_tx.send(PtyIoControlCommand::Shutdown);
            }
            PtyCloseCompletion {
                accepting: Arc::clone(&self.accepting),
                deadline: input_deadline + (PTY_CLOSE_TIMEOUT - INPUT_SHUTDOWN_GRACE),
            }
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
            let accepting = Arc::new(Mutex::new(InputAcceptance {
                accepting: !initially_quiesced,
                enter_completion: None,
                shutdown_deadline: None,
                closed_at: None,
            }));
            let input_admission = Admission::input();
            let response_admission = Admission::responses();
            let pending_resize = Arc::new(Mutex::new(None::<PtyResizeRequest>));

            {
                let accepting = Arc::clone(&accepting);
                let input = data_tx.downgrade();
                let control = control_tx.clone();
                let responses = Arc::clone(&response_admission);
                let input_admission = Arc::clone(&input_admission);
                crate::thread_spawn::spawn_named("herdr-pty-writer", move || {
                    run_writer(&mut writer, write_rx);
                    input_admission.close();
                    accepting
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .close();
                    responses.close();
                    if let Some(input) = input.upgrade() {
                        let _ = input.try_send(PtyIoDataCommand::Shutdown);
                    }
                    let _ = control.send(PtyIoControlCommand::Shutdown);
                    debug!(pane_id, "windows pty writer thread exiting");
                })?;
            }

            {
                let write_tx = write_tx.clone();
                let accepting = Arc::clone(&accepting);
                tokio::spawn(async move {
                    run_input_forwarder(&mut data_rx, write_tx, accepting).await;
                    debug!(pane_id, "windows pty input task exiting");
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
                crate::thread_spawn::spawn_named("herdr-pty-reader", move || {
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
                    accepting
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .close();
                    responses.close();
                    if let Some(input) = input.upgrade() {
                        let _ = input.try_send(PtyIoDataCommand::Shutdown);
                    }
                    let _ = control.send(PtyIoControlCommand::Shutdown);
                    if let Some(on_reader_exit) = on_reader_exit {
                        on_reader_exit();
                    }
                    debug!(pane_id, "windows pty reader thread exiting");
                })?;
            }

            {
                let write_tx = write_tx.clone();
                let pending_resize = Arc::clone(&pending_resize);
                let accepting = Arc::clone(&accepting);
                crate::thread_spawn::spawn_named("herdr-pty-control", move || {
                    run_control(
                        master,
                        |master, size| {
                            if let Err(err) = master.resize(PtySize {
                                rows: size.rows,
                                cols: size.cols,
                                pixel_width: size.cell_width_px.min(u16::MAX as u32) as u16,
                                pixel_height: size.cell_height_px.min(u16::MAX as u32) as u16,
                            }) {
                                warn!(pane_id, err = %err, "windows pty resize failed");
                            }
                        },
                        control_rx,
                        write_tx,
                        pending_resize,
                        accepting,
                    );
                    debug!(pane_id, "windows pty control thread exiting");
                })?;
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

    fn run_control<M>(
        master: M,
        resize: impl Fn(&M, PtyResize),
        control_rx: std_mpsc::Receiver<PtyIoControlCommand>,
        write_tx: std_mpsc::Sender<PtyIoWriteCommand>,
        pending_resize: Arc<Mutex<Option<PtyResizeRequest>>>,
        accepting: Arc<Mutex<InputAcceptance>>,
    ) {
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
                    resize(&master, request.resize);
                    if request
                        .terminal_responses
                        .into_iter()
                        .any(|response| write_tx.send(PtyIoWriteCommand::Write(response)).is_err())
                    {
                        break;
                    }
                }
                PtyIoControlCommand::Shutdown => {
                    wait_for_committed_submission(&accepting);
                    break;
                }
            }
        }
        pending_resize
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        drop(master);
        accepting
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .closed_at
            .get_or_insert_with(Instant::now);
    }

    fn run_writer(writer: &mut impl Write, write_rx: std_mpsc::Receiver<PtyIoWriteCommand>) {
        for command in write_rx {
            let result = match command {
                PtyIoWriteCommand::Write(bytes) => write_and_flush(writer, &bytes),
                PtyIoWriteCommand::SubmissionPart {
                    bytes,
                    deadline,
                    reply,
                    enter_completion,
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
                    // Release the control-thread grace even if the async acknowledgement
                    // cannot be scheduled or its receiver has been cancelled.
                    drop(enter_completion);
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

    fn wait_for_committed_submission(accepting: &Mutex<InputAcceptance>) {
        let pending = {
            let mut accepting = accepting
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            accepting
                .enter_completion
                .take()
                .zip(accepting.shutdown_deadline)
        };
        if let Some((completion, deadline)) = pending {
            let _ = completion.recv_timeout(deadline.saturating_duration_since(Instant::now()));
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

    async fn run_input_forwarder(
        data_rx: &mut mpsc::Receiver<PtyIoDataCommand>,
        write_tx: std_mpsc::Sender<PtyIoWriteCommand>,
        accepting: Arc<Mutex<InputAcceptance>>,
    ) {
        while let Some(command) = data_rx.recv().await {
            if !accepting
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .accepting
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
                    let submission = tokio::spawn(run_input_submission(
                        write_tx.clone(),
                        Arc::clone(&accepting),
                        text,
                        enter,
                        delay,
                        deadline,
                        reply,
                    ));
                    if !matches!(submission.await, Ok(false)) {
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

    async fn run_input_submission(
        write_tx: std_mpsc::Sender<PtyIoWriteCommand>,
        accepting: Arc<Mutex<InputAcceptance>>,
        text: Bytes,
        enter: Bytes,
        delay: Duration,
        deadline: Option<Instant>,
        reply: std_mpsc::Sender<std::io::Result<()>>,
    ) -> bool {
        let result = async move {
            let (completion, done) = {
                let mut accepting = accepting
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if !accepting.accepting {
                    return Err(pty_actor_closed());
                }
                if deadline.is_some_and(|deadline| {
                    deadline.saturating_duration_since(Instant::now()) <= delay
                }) {
                    return Err(input_submission_timed_out());
                }
                let text_deadline = deadline.and_then(|deadline| deadline.checked_sub(delay));
                let (done, completion) = std_mpsc::channel();
                accepting.enter_completion = Some(completion);
                (
                    enqueue_submission_part(&write_tx, text, text_deadline, None)?,
                    done,
                )
            };
            completion
                .await
                .unwrap_or_else(|_| Err(pty_actor_closed()))?;
            // A committed submission outlives its caller and forwarder task. The control
            // thread waits only until the original shutdown deadline, without holding a lock.
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            write_submission_part(&write_tx, enter, None, Some(done)).await
        }
        .await;
        let failed = result
            .as_ref()
            .is_err_and(|err| err.kind() != std::io::ErrorKind::TimedOut);
        let _ = reply.send(result);
        failed
    }

    fn enqueue_submission_part(
        write_tx: &std_mpsc::Sender<PtyIoWriteCommand>,
        bytes: Bytes,
        deadline: Option<Instant>,
        enter_completion: Option<std_mpsc::Sender<()>>,
    ) -> std::io::Result<oneshot::Receiver<std::io::Result<()>>> {
        let (reply, completion) = oneshot::channel();
        write_tx
            .send(PtyIoWriteCommand::SubmissionPart {
                bytes,
                deadline,
                reply,
                enter_completion,
            })
            .map_err(|_| pty_actor_closed())?;
        Ok(completion)
    }

    async fn write_submission_part(
        write_tx: &std_mpsc::Sender<PtyIoWriteCommand>,
        bytes: Bytes,
        deadline: Option<Instant>,
        enter_completion: Option<std_mpsc::Sender<()>>,
    ) -> std::io::Result<()> {
        enqueue_submission_part(write_tx, bytes, deadline, enter_completion)?
            .await
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
    pub(crate) mod shutdown_test_support {
        use super::*;

        struct ClosingMaster {
            closed_at: Arc<Mutex<Option<Instant>>>,
            release: std_mpsc::Sender<()>,
        }

        impl Drop for ClosingMaster {
            fn drop(&mut self) {
                *self.closed_at.lock().unwrap() = Some(Instant::now());
                let _ = self.release.send(());
            }
        }

        struct ClosingWriter {
            entered: std_mpsc::Sender<()>,
            release: Option<std_mpsc::Receiver<()>>,
        }

        impl Write for ClosingWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if bytes == b"\r" {
                    if let Some(release) = self.release.take() {
                        let _ = self.entered.send(());
                        let _ = release.recv();
                        return Err(pty_actor_closed());
                    }
                }
                Ok(bytes.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        pub(crate) struct ShutdownActor {
            pub(crate) handle: Option<PtyIoActorHandle>,
            pub(crate) closed_at: Arc<Mutex<Option<Instant>>>,
            entered: std_mpsc::Receiver<()>,
            runtime: tokio::runtime::Handle,
            input: Option<tokio::task::JoinHandle<()>>,
            input_start: Option<oneshot::Sender<()>>,
            writer: Option<std::thread::JoinHandle<()>>,
            control: Option<std::thread::JoinHandle<()>>,
            control_entered: std_mpsc::Receiver<()>,
            control_release: Option<std_mpsc::Sender<()>>,
            inputs: Arc<Admission>,
            responses: Arc<Admission>,
        }

        impl ShutdownActor {
            pub(crate) fn new(runtime: &tokio::runtime::Handle) -> Self {
                let mut actor = Self::new_paused(runtime);
                actor.start_input();
                actor
            }

            pub(crate) fn new_paused(runtime: &tokio::runtime::Handle) -> Self {
                let (data_tx, mut data_rx) = mpsc::channel(INPUT_ITEM_LIMIT);
                let (control_tx, control_rx) = std_mpsc::channel();
                let (write_tx, write_rx) = std_mpsc::channel();
                let (entered_tx, entered) = std_mpsc::channel();
                let (release_tx, release_rx) = std_mpsc::channel();
                let (input_start, input_started) = oneshot::channel();
                let (control_started, control_entered) = std_mpsc::channel();
                let (control_release, control_released) = std_mpsc::channel();
                let accepting = Arc::new(Mutex::new(InputAcceptance {
                    accepting: true,
                    enter_completion: None,
                    shutdown_deadline: None,
                    closed_at: None,
                }));
                let inputs = Admission::input();
                let responses = Admission::responses();
                let pending_resize = Arc::new(Mutex::new(None));
                let handle = PtyIoActorHandle {
                    data_tx,
                    control_tx,
                    write_tx: write_tx.clone(),
                    response_order: Arc::new(Mutex::new(())),
                    accepting: Arc::clone(&accepting),
                    input_admission: Arc::clone(&inputs),
                    response_admission: Arc::clone(&responses),
                    pending_resize: Arc::clone(&pending_resize),
                };
                let input_tx = write_tx.clone();
                let input_accepting = Arc::clone(&accepting);
                let input = runtime.spawn(async move {
                    let _ = input_started.await;
                    run_input_forwarder(&mut data_rx, input_tx, input_accepting).await;
                });
                let writer = std::thread::spawn(move || {
                    let mut writer = ClosingWriter {
                        entered: entered_tx,
                        release: Some(release_rx),
                    };
                    run_writer(&mut writer, write_rx);
                });
                let closed_at = Arc::new(Mutex::new(None));
                let master = ClosingMaster {
                    closed_at: Arc::clone(&closed_at),
                    release: release_tx,
                };
                let control = std::thread::spawn(move || {
                    let release = Mutex::new(Some(control_released));
                    run_control(
                        master,
                        |_, _| {
                            if let Some(release) = release.lock().unwrap().take() {
                                let _ = control_started.send(());
                                let _ = release.recv();
                            }
                        },
                        control_rx,
                        write_tx,
                        pending_resize,
                        accepting,
                    );
                });
                Self {
                    handle: Some(handle),
                    closed_at,
                    entered,
                    runtime: runtime.clone(),
                    input: Some(input),
                    input_start: Some(input_start),
                    writer: Some(writer),
                    control: Some(control),
                    control_entered,
                    control_release: Some(control_release),
                    inputs,
                    responses,
                }
            }

            pub(crate) fn start_input(&mut self) {
                if let Some(start) = self.input_start.take() {
                    let _ = start.send(());
                }
            }

            pub(crate) fn cancel_input(&mut self) {
                if let Some(input) = self.input.take() {
                    input.abort();
                    assert!(self.runtime.block_on(input).unwrap_err().is_cancelled());
                }
            }

            pub(crate) fn block_control(&self) {
                self.handle.as_ref().unwrap().resize(
                    24,
                    80,
                    8,
                    16,
                    vec![Bytes::from_static(b"response")],
                );
                self.control_entered
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap();
            }

            pub(crate) fn wait_for_enter(&self) {
                self.entered.recv_timeout(Duration::from_secs(2)).unwrap();
            }

            pub(crate) fn wait_for_close(&self) -> Instant {
                let deadline = Instant::now() + Duration::from_secs(2);
                loop {
                    if let Some(closed_at) = *self.closed_at.lock().unwrap() {
                        if self
                            .control
                            .as_ref()
                            .is_none_or(|control| control.is_finished())
                        {
                            return closed_at;
                        }
                    }
                    assert!(Instant::now() < deadline, "actor must close its master");
                    std::thread::sleep(Duration::from_millis(1));
                }
            }

            pub(crate) fn finish(mut self) {
                self.close_and_join();
                assert_eq!(self.inputs.in_use(), (0, 0));
                assert_eq!(self.responses.in_use(), (0, 0));
            }

            fn close_and_join(&mut self) {
                if let Some(handle) = self.handle.take() {
                    handle.shutdown();
                }
                self.start_input();
                if let Some(release) = self.control_release.take() {
                    let _ = release.send(());
                }
                if let Some(control) = self.control.take() {
                    let _ = control.join();
                }
                if let Some(input) = self.input.take() {
                    let _ = self.runtime.block_on(input);
                }
                if let Some(writer) = self.writer.take() {
                    let _ = writer.join();
                }
            }
        }

        impl Drop for ShutdownActor {
            fn drop(&mut self) {
                self.close_and_join();
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[tokio::test]
        async fn windows_pty_thread_spawn_failures_return_errors() {
            for allowed in 0..3 {
                let pair = portable_pty::native_pty_system()
                    .openpty(PtySize {
                        rows: 24,
                        cols: 80,
                        pixel_width: 0,
                        pixel_height: 0,
                    })
                    .expect("create ConPTY");
                drop(pair.slave);
                let (exited, mut reader_exit) = tokio::sync::oneshot::channel();
                crate::thread_spawn::test_hook::fail_spawns_after(allowed, 1);
                let result = PtyIoActor::spawn(PtyIoActorConfig {
                    pane_id: 1,
                    master: pair.master,
                    initially_quiesced: false,
                    on_read: Box::new(|_| PtyReadResult {
                        terminal_responses: Vec::new(),
                    }),
                    on_reader_exit: Some(Box::new(move || {
                        let _ = exited.send(());
                    })),
                });
                crate::thread_spawn::test_hook::fail_next_spawns(0);
                assert!(
                    matches!(result, Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock)
                );
                if allowed == 2 {
                    tokio::time::timeout(Duration::from_secs(30), &mut reader_exit)
                        .await
                        .expect("reader stops after failed control spawn")
                        .expect("reader exit callback");
                }
            }
        }

        fn input_acceptance(accepting: bool) -> Arc<Mutex<InputAcceptance>> {
            Arc::new(Mutex::new(InputAcceptance {
                accepting,
                enter_completion: None,
                shutdown_deadline: None,
                closed_at: None,
            }))
        }

        fn test_runtime() -> tokio::runtime::Runtime {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .unwrap()
        }

        async fn next_actor_event<T>(receiver: &std_mpsc::Receiver<T>) -> T {
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    match receiver.try_recv() {
                        Ok(command) => return command,
                        Err(std_mpsc::TryRecvError::Empty) => tokio::task::yield_now().await,
                        Err(err) => panic!("actor channel disconnected: {err}"),
                    }
                }
            })
            .await
            .expect("actor sends event")
        }

        struct QueuedInputActor {
            handle: PtyIoActorHandle,
            input_task: tokio::task::JoinHandle<()>,
            write_rx: std_mpsc::Receiver<PtyIoWriteCommand>,
            control_rx: std_mpsc::Receiver<PtyIoControlCommand>,
        }

        impl QueuedInputActor {
            fn new(runtime: &tokio::runtime::Handle, initially_accepting: bool) -> Self {
                let (data_tx, mut data_rx) = mpsc::channel(INPUT_ITEM_LIMIT);
                let (control_tx, control_rx) = std_mpsc::channel();
                let (write_tx, write_rx) = std_mpsc::channel();
                let accepting = input_acceptance(initially_accepting);
                let input_write_tx = write_tx.clone();
                let input_accepting = Arc::clone(&accepting);
                let input_task = runtime.spawn(async move {
                    run_input_forwarder(&mut data_rx, input_write_tx, input_accepting).await;
                });
                Self {
                    handle: PtyIoActorHandle {
                        data_tx,
                        control_tx,
                        write_tx,
                        response_order: Arc::new(Mutex::new(())),
                        accepting,
                        input_admission: Admission::input(),
                        response_admission: Admission::responses(),
                        pending_resize: Arc::new(Mutex::new(None)),
                    },
                    input_task,
                    write_rx,
                    control_rx,
                }
            }
        }

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
            runtime: tokio::runtime::Runtime,
            input_task: Option<tokio::task::JoinHandle<()>>,
            writer_thread: Option<std::thread::JoinHandle<RecordingWriter>>,
            callers: Vec<std::thread::JoinHandle<()>>,
        }

        impl GatedWriterActor {
            fn new(blocked_bytes: Bytes) -> Self {
                let (data_tx, mut data_rx) = mpsc::channel(INPUT_ITEM_LIMIT);
                let (control_tx, control_rx) = std_mpsc::channel();
                let (write_tx, write_rx) = std_mpsc::channel();
                let (entered_tx, entered) = std_mpsc::channel();
                let (release, release_rx) = std_mpsc::channel();
                let (flushed, _flushed_rx) = std_mpsc::channel();
                let accepting = input_acceptance(true);
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
                    runtime: test_runtime(),
                    input_task: None,
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
                actor.input_task = Some(actor.runtime.spawn(async move {
                    run_input_forwarder(&mut data_rx, input_write_tx, input_accepting).await;
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
                self.runtime
                    .block_on(self.input_task.take().expect("input task exists"))
                    .expect("input task joins");
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
                if let Some(input) = self.input_task.take() {
                    let _ = self.runtime.block_on(input);
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
            let (reply, mut completion) = oneshot::channel();
            let first = budget.try_reserve(6).unwrap();
            let second = budget.try_reserve(6).unwrap();
            write_tx
                .send(PtyIoWriteCommand::SubmissionPart {
                    bytes: first.wrap(Bytes::from_static(b"prompt")),
                    deadline: None,
                    reply,
                    enter_completion: None,
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
                completion.try_recv().unwrap().unwrap_err().kind(),
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
            during_delay: impl FnOnce(
                &std_mpsc::Sender<PtyIoWriteCommand>,
                &Arc<Mutex<InputAcceptance>>,
            ),
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
            let accepting = input_acceptance(true);
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
            let runtime = test_runtime();
            let input_task = runtime.spawn(async move {
                run_input_forwarder(&mut data_rx, input_write_tx, input_accepting).await
            });
            flushed_rx.recv().expect("prompt was flushed");
            during_delay(&write_tx, &accepting);
            let result = reply_rx.recv().expect("writer reports submission");
            drop(data_tx);
            runtime.block_on(input_task).expect("input task joins");
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
                    accepting.lock().unwrap().accepting = false;
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
            let accepting = input_acceptance(true);
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
            let runtime = test_runtime();
            let input_task = runtime.spawn(async move {
                run_input_forwarder(&mut data_rx, input_write_tx, accepting).await
            });
            first_reply_rx.recv().unwrap().unwrap();
            let err = expired_reply_rx.recv().unwrap().unwrap_err();
            assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);

            drop(data_tx);
            runtime.block_on(input_task).unwrap();
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

        #[tokio::test]
        async fn abandoned_committed_submission_finishes_without_blocking_executor() {
            let (data_tx, mut data_rx) = mpsc::channel(2);
            let (write_tx, write_rx) = std_mpsc::channel();
            let (reply_tx, reply_rx) = std_mpsc::channel();
            let deadline = Instant::now() + Duration::from_millis(50);
            data_tx
                .try_send(PtyIoDataCommand::SubmitUserInput {
                    text: Bytes::from_static(b"committed"),
                    enter: Bytes::from_static(b"\r"),
                    delay: Duration::from_millis(20),
                    deadline: Some(deadline),
                    reply: reply_tx,
                })
                .unwrap();
            data_tx
                .try_send(PtyIoDataCommand::WriteUserInput(Bytes::from_static(
                    b"after",
                )))
                .unwrap();
            drop(data_tx);
            let forwarder = tokio::spawn(async move {
                run_input_forwarder(&mut data_rx, write_tx, input_acceptance(true)).await
            });

            let PtyIoWriteCommand::SubmissionPart { bytes, reply, .. } =
                next_actor_event(&write_rx).await
            else {
                panic!("text must be first");
            };
            assert_eq!(bytes, b"committed".as_slice());
            assert!(reply_rx.try_recv().is_err(), "completion waits for flush");
            // A text write can finish after the caller's deadline. Its acknowledgement
            // must still be consumed, and Enter must follow even after the caller leaves.
            tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
            drop(reply_rx);
            reply.send(Ok(())).unwrap();
            let PtyIoWriteCommand::SubmissionPart {
                bytes,
                deadline,
                reply,
                enter_completion,
            } = next_actor_event(&write_rx).await
            else {
                panic!("queued user input cannot overtake Enter");
            };
            assert_eq!(bytes, b"\r".as_slice());
            assert!(deadline.is_none(), "committed Enter cannot expire");
            assert!(
                write_rx.try_recv().is_err(),
                "user input waits for Enter flush"
            );
            drop(enter_completion);
            reply.send(Ok(())).unwrap();
            forwarder.await.unwrap();
            let PtyIoWriteCommand::Write(bytes) = write_rx.try_recv().unwrap() else {
                panic!("ordinary user input follows Enter");
            };
            assert_eq!(bytes, b"after".as_slice());
        }

        #[test]
        fn shutdown_gives_queued_enter_a_bounded_grace() {
            for outcome in ["flushed", "failed", "disconnected", "stalled"] {
                let runtime = test_runtime();
                let QueuedInputActor {
                    handle,
                    input_task,
                    write_rx,
                    control_rx,
                } = QueuedInputActor::new(runtime.handle(), true);
                let result = handle
                    .queue_user_input_submission(
                        Bytes::from_static(b"prompt"),
                        Bytes::from_static(b"\r"),
                        Duration::ZERO,
                        None,
                    )
                    .unwrap();
                let PtyIoWriteCommand::SubmissionPart { bytes, reply, .. } =
                    write_rx.recv_timeout(Duration::from_secs(2)).unwrap()
                else {
                    panic!("text must be first")
                };
                drop(bytes);
                reply.send(Ok(())).unwrap();
                let command = write_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                let accepting = Arc::clone(&handle.accepting);
                let control_accepting = Arc::clone(&accepting);
                let (waiting_tx, waiting_rx) = std_mpsc::channel();
                let (closed_tx, closed_rx) = std_mpsc::channel();
                let control = std::thread::spawn(move || {
                    assert!(matches!(
                        control_rx.recv_timeout(Duration::from_secs(2)),
                        Ok(PtyIoControlCommand::Shutdown)
                    ));
                    waiting_tx.send(()).unwrap();
                    wait_for_committed_submission(&control_accepting);
                    closed_tx.send(Instant::now()).unwrap();
                });
                let started = Instant::now();
                handle.shutdown();
                assert!(started.elapsed() < INPUT_SHUTDOWN_GRACE);
                waiting_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                let deadline = {
                    let started = Instant::now();
                    let state = accepting.lock().unwrap();
                    assert!(
                        started.elapsed() < Duration::from_millis(100),
                        "control wait releases acceptance lock"
                    );
                    assert!(!state.accepting);
                    state.shutdown_deadline.unwrap()
                };
                handle.shutdown();
                assert_eq!(accepting.lock().unwrap().shutdown_deadline, Some(deadline));
                assert!(
                    closed_rx.try_recv().is_err(),
                    "master waits for committed Enter"
                );

                let (flushed, _flushed_rx) = std_mpsc::channel();
                let mut writer = RecordingWriter {
                    writes: vec![],
                    flushes: vec![],
                    fail_after: (outcome == "failed").then_some(0),
                    flushed,
                };
                if outcome == "stalled" {
                    let closed_at = closed_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                    assert!(closed_at >= deadline);
                    assert!(closed_at.duration_since(deadline) < Duration::from_secs(1));
                    drop(command);
                } else {
                    if outcome == "disconnected" {
                        drop(command);
                    } else {
                        let (writer_tx, writer_rx) = std_mpsc::channel();
                        writer_tx.send(command).unwrap();
                        drop(writer_tx);
                        run_writer(&mut writer, writer_rx);
                    }
                    closed_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                }
                control.join().unwrap();
                assert_eq!(
                    result
                        .recv_timeout(Duration::from_secs(2))
                        .unwrap()
                        .is_err(),
                    outcome != "flushed"
                );
                runtime.block_on(input_task).unwrap();
                assert_eq!(writer.flushes.len(), usize::from(outcome == "flushed"));
                assert_eq!(handle.input_admission.in_use(), (0, 0));
            }
        }

        #[tokio::test]
        async fn submission_rejects_quiesced_actor_and_failed_enqueue_releases_shutdown_wait() {
            for initially_accepting in [false, true] {
                let (write_tx, write_rx) = std_mpsc::channel();
                let (reply, result) = std_mpsc::channel();
                let accepting = input_acceptance(initially_accepting);
                let budget = Admission::input();
                let permit = budget.try_reserve(7).unwrap();
                let text = permit.wrap(Bytes::from_static(b"prompt"));
                let enter = permit.wrap(Bytes::from_static(b"\r"));
                drop(permit);
                drop(write_rx);
                assert!(
                    run_input_submission(
                        write_tx,
                        Arc::clone(&accepting),
                        text,
                        enter,
                        Duration::ZERO,
                        None,
                        reply,
                    )
                    .await
                );
                assert_eq!(
                    result.try_recv().unwrap().unwrap_err().kind(),
                    std::io::ErrorKind::BrokenPipe
                );
                assert_eq!(budget.in_use(), (0, 0));
                let completion = accepting.lock().unwrap().enter_completion.take();
                if initially_accepting {
                    assert!(matches!(
                        completion.unwrap().try_recv(),
                        Err(std_mpsc::TryRecvError::Disconnected)
                    ));
                } else {
                    assert!(completion.is_none());
                }
            }
        }

        #[tokio::test]
        async fn shutdown_rejects_unstarted_submission_and_releases_permits() {
            let actor = QueuedInputActor::new(&tokio::runtime::Handle::current(), true);
            let result = actor
                .handle
                .queue_user_input_submission(
                    Bytes::from_static(b"prompt"),
                    Bytes::from_static(b"\r"),
                    Duration::ZERO,
                    None,
                )
                .unwrap();
            assert_eq!(actor.handle.input_admission.in_use(), (1, 7));
            actor
                .handle
                .resize(24, 80, 8, 16, vec![Bytes::from_static(b"size")]);
            assert_eq!(actor.handle.response_admission.in_use(), (1, 4));
            actor.handle.shutdown();
            actor.input_task.await.unwrap();
            assert_eq!(
                result.try_recv().unwrap().unwrap_err().kind(),
                std::io::ErrorKind::BrokenPipe
            );
            assert!(actor.write_rx.try_recv().is_err());
            assert_eq!(actor.handle.input_admission.in_use(), (0, 0));
            assert_eq!(actor.handle.response_admission.in_use(), (0, 0));
        }

        #[tokio::test]
        async fn cancelled_forwarder_preserves_committed_submission_and_releases_permits() {
            for phase in ["text", "delay", "enter"] {
                let actor = QueuedInputActor::new(&tokio::runtime::Handle::current(), true);
                let result = actor
                    .handle
                    .queue_user_input_submission(
                        Bytes::from_static(b"prompt"),
                        Bytes::from_static(b"\r"),
                        Duration::from_millis(20),
                        None,
                    )
                    .unwrap();
                let queued = actor
                    .handle
                    .queue_user_input_submission(
                        Bytes::from_static(b"queued"),
                        Bytes::from_static(b"\r"),
                        Duration::ZERO,
                        None,
                    )
                    .unwrap();
                let text = next_actor_event(&actor.write_rx).await;
                let mut text = Some(text);
                let mut enter = None;
                if phase != "text" {
                    let PtyIoWriteCommand::SubmissionPart { bytes, reply, .. } =
                        text.take().unwrap()
                    else {
                        panic!("text must be first")
                    };
                    assert_eq!(bytes.as_ref(), b"prompt");
                    drop(bytes);
                    reply.send(Ok(())).unwrap();
                    if phase == "enter" {
                        enter = Some(next_actor_event(&actor.write_rx).await);
                    } else {
                        tokio::task::yield_now().await;
                    }
                }
                actor.input_task.abort();
                assert!(actor.input_task.await.unwrap_err().is_cancelled());
                actor.handle.shutdown();
                assert!(matches!(
                    queued.try_recv(),
                    Err(std_mpsc::TryRecvError::Disconnected)
                ));
                assert_eq!(actor.handle.input_admission.in_use(), (1, 7));
                if let Some(PtyIoWriteCommand::SubmissionPart { bytes, reply, .. }) = text {
                    drop(bytes);
                    reply.send(Ok(())).unwrap();
                }
                let enter = match enter {
                    Some(enter) => enter,
                    None => next_actor_event(&actor.write_rx).await,
                };
                let PtyIoWriteCommand::SubmissionPart {
                    bytes,
                    reply,
                    enter_completion,
                    ..
                } = enter
                else {
                    panic!("committed Enter survives forwarder cancellation")
                };
                assert_eq!(bytes.as_ref(), b"\r");
                drop((bytes, enter_completion));
                reply.send(Ok(())).unwrap();
                next_actor_event(&result).await.unwrap();
                assert!(actor.write_rx.try_recv().is_err());
                assert_eq!(actor.handle.input_admission.in_use(), (0, 0));
            }
        }

        #[tokio::test]
        async fn multi_pane_shutdown_does_not_block_executor_or_restart_grace() {
            let mut actors = Vec::new();
            let mut results = Vec::new();
            let mut enters = Vec::new();
            let mut controls = Vec::new();
            for _ in 0..15 {
                let QueuedInputActor {
                    handle,
                    input_task,
                    write_rx,
                    control_rx,
                } = QueuedInputActor::new(&tokio::runtime::Handle::current(), true);
                results.push(
                    handle
                        .queue_user_input_submission(
                            Bytes::from_static(b"prompt"),
                            Bytes::from_static(b"\r"),
                            Duration::ZERO,
                            None,
                        )
                        .unwrap(),
                );
                let PtyIoWriteCommand::SubmissionPart { bytes, reply, .. } =
                    next_actor_event(&write_rx).await
                else {
                    panic!("text must be first")
                };
                drop(bytes);
                reply.send(Ok(())).unwrap();
                enters.push(next_actor_event(&write_rx).await);
                let accepting = Arc::clone(&handle.accepting);
                controls.push(std::thread::spawn(move || {
                    assert!(matches!(
                        control_rx.recv_timeout(Duration::from_secs(2)),
                        Ok(PtyIoControlCommand::Shutdown)
                    ));
                    wait_for_committed_submission(&accepting);
                    Instant::now()
                }));
                actors.push((handle, input_task, write_rx));
            }
            let started = Instant::now();
            for (handle, _, _) in &actors {
                handle.shutdown();
            }
            assert!(
                started.elapsed() < Duration::from_secs(1),
                "15 blocked Enter writes must not serialize shutdown grace"
            );
            let deadlines: Vec<_> = actors
                .iter()
                .map(|(handle, _, _)| handle.accepting.lock().unwrap().shutdown_deadline.unwrap())
                .collect();
            tokio::time::sleep(Duration::from_millis(20)).await;
            for ((handle, _, _), deadline) in actors.iter().zip(&deadlines) {
                handle.shutdown();
                let started = Instant::now();
                let state = handle.accepting.lock().unwrap();
                assert!(
                    started.elapsed() < Duration::from_millis(100),
                    "control wait never holds shared lock"
                );
                assert_eq!(state.shutdown_deadline, Some(*deadline));
            }
            tokio::time::sleep(INPUT_SHUTDOWN_GRACE).await;
            for (control, deadline) in controls.into_iter().zip(deadlines) {
                let closed_at = control.join().unwrap();
                assert!(closed_at >= deadline);
                assert!(closed_at.duration_since(deadline) < Duration::from_secs(1));
            }
            drop(enters);
            for ((handle, input_task, _), result) in actors.into_iter().zip(results) {
                assert_eq!(
                    next_actor_event(&result).await.unwrap_err().kind(),
                    std::io::ErrorKind::BrokenPipe
                );
                input_task.await.unwrap();
                assert_eq!(handle.input_admission.in_use(), (0, 0));
            }
        }
    }
}

#[cfg(windows)]
pub(crate) use windows::*;
