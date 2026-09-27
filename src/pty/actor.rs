use std::{
    collections::VecDeque,
    io::{Read, Write},
    os::fd::{AsRawFd, OwnedFd},
    sync::{Arc, Mutex, mpsc as std_mpsc},
    time::{Duration, Instant},
};

use crate::layout::PaneId;
use bytes::Bytes;
use tokio::sync::mpsc::{self, error::TryRecvError as DataTryRecvError};
use tracing::{debug, error, warn};

pub use crate::pty::submission::{QueuedSubmission, SubmissionCancel, SubmissionCancelOutcome};
use crate::pty::{
    fd,
    submission::{EnterStart, SharedSubmissionState, SubmissionPart, SubmissionState, lock_state},
};

// Actor handle methods must call wake_actor() after queuing work. The idle
// timeout is only a fallback for missed wakes; PTY and wake readiness drive
// normal responsiveness.
const ACTOR_IDLE_POLL_MS: i32 = 1000;
const ACTOR_COMMAND_BUFFER: usize = 1024;

pub(crate) struct PtyReadResult {
    pub terminal_responses: Vec<Bytes>,
    /// The callback could not consume the bytes and never will again: the
    /// terminal core's lock was poisoned by a panic on some other thread
    /// (render, detection, an API read). The loop ends exactly as for a
    /// panic in the callback itself, so the owner hears the pane is dead
    /// instead of the reader discarding output forever.
    pub core_broken: bool,
}

impl PtyReadResult {
    #[cfg(test)]
    pub(crate) fn empty() -> Self {
        Self {
            terminal_responses: Vec::new(),
            core_broken: false,
        }
    }
}

type ReadCallback = Box<dyn FnMut(&[u8]) -> PtyReadResult + Send + 'static>;
type ReaderExitCallback = Box<dyn FnOnce(ReaderExit) + Send + 'static>;
/// Whether the terminal core has been broken by a panic on some other thread.
/// Must be cheap (an atomic load): the actor asks on every loop iteration.
type CoreBrokenCheck = Box<dyn Fn() -> bool + Send + 'static>;

/// Why the actor's IO loop ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReaderExit {
    /// EOF, an IO error, a shutdown request or closed command queues. The
    /// child has gone or is being torn down; its own exit is reported by
    /// whoever reaps it.
    Closed,
    /// The read callback panicked, or reported the terminal core broken by a
    /// panic elsewhere (a terminal core bug either way). The loop stops and
    /// the master fd is closed, but the child may outlive the SIGHUP, so the
    /// owner must be told the pane is dead.
    Panicked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PtyResize {
    geometry: crate::geometry::PaneGeometry,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PtyResizeRequest {
    resize: PtyResize,
    terminal_responses: Vec<Bytes>,
}

#[derive(Default)]
struct SharedPtyControls {
    resize: Option<PtyResizeRequest>,
    terminal_responses: Vec<Bytes>,
}

pub(crate) struct PtyIoActorConfig {
    pub pane_id: PaneId,
    pub master_fd: OwnedFd,
    pub on_read: ReadCallback,
    pub on_reader_exit: Option<ReaderExitCallback>,
    /// Checked on every loop iteration, including the idle poll that fires
    /// at least once a second, so a core poisoned off the reader thread ends
    /// the pane even when the child prints nothing. Without it only the next
    /// read would notice (`PtyReadResult::core_broken`), and an idle pane
    /// would sit frozen, its reads quietly answering empty, indefinitely.
    pub core_broken: Option<CoreBrokenCheck>,
}

enum PtyIoDataCommand {
    WriteUserInput(Bytes),
    SubmitUserInput {
        text: Bytes,
        enter: Bytes,
        delay: Duration,
        reply: std_mpsc::Sender<std::io::Result<()>>,
        state: SharedSubmissionState,
    },
}

fn submission_withdrawn_error() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "input submission withdrawn by its caller",
    )
}

impl PtyIoDataCommand {
    /// The bytes a rejected send carried back. A failed `try_send` returns the
    /// very command that was offered, so for a write this is its payload; a
    /// submission hands back its text so no arm needs a panic.
    fn into_input_bytes(self) -> Bytes {
        match self {
            Self::WriteUserInput(bytes) => bytes,
            Self::SubmitUserInput { text, .. } => text,
        }
    }
}

enum PtyIoControlCommand {
    Shutdown,
}

#[derive(Clone)]
pub(crate) struct PtyIoActorHandle {
    data_tx: mpsc::Sender<PtyIoDataCommand>,
    control_tx: std_mpsc::Sender<PtyIoControlCommand>,
    wake: fd::WakeWriter,
    user_writes: Arc<Mutex<UserWriteGate>>,
    controls: Arc<Mutex<SharedPtyControls>>,
    response_order: Arc<Mutex<()>>,
}

#[derive(Debug)]
struct UserWriteGate {
    accepting: bool,
}

impl PtyIoActorHandle {
    pub(crate) fn try_write_user_input(
        &self,
        bytes: Bytes,
    ) -> Result<(), mpsc::error::TrySendError<Bytes>> {
        let user_writes = crate::vt::lock_auxiliary(&self.user_writes);
        if !user_writes.accepting {
            return Err(mpsc::error::TrySendError::Closed(bytes));
        }
        match self
            .data_tx
            .try_send(PtyIoDataCommand::WriteUserInput(bytes))
        {
            Ok(()) => {
                self.wake_actor();
                Ok(())
            }
            Err(mpsc::error::TrySendError::Full(command)) => {
                Err(mpsc::error::TrySendError::Full(command.into_input_bytes()))
            }
            Err(mpsc::error::TrySendError::Closed(command)) => Err(
                mpsc::error::TrySendError::Closed(command.into_input_bytes()),
            ),
        }
    }

    pub(crate) fn queue_user_input_submission(
        &self,
        text: Bytes,
        enter: Bytes,
        delay: Duration,
    ) -> std::io::Result<QueuedSubmission> {
        let user_writes = crate::vt::lock_auxiliary(&self.user_writes);
        if !user_writes.accepting {
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "pty actor closed",
            ));
        }
        let (reply_tx, reply_rx) = std_mpsc::channel();
        let state = SubmissionState::shared();
        self.data_tx
            .try_send(PtyIoDataCommand::SubmitUserInput {
                text,
                enter,
                delay,
                reply: reply_tx,
                state: Arc::clone(&state),
            })
            .map_err(|err| match err {
                mpsc::error::TrySendError::Full(_) => {
                    std::io::Error::new(std::io::ErrorKind::WouldBlock, "pty input queue is full")
                }
                mpsc::error::TrySendError::Closed(_) => {
                    std::io::Error::new(std::io::ErrorKind::BrokenPipe, "pty actor closed")
                }
            })?;
        self.wake_actor();
        Ok(QueuedSubmission {
            completion: reply_rx,
            cancel: SubmissionCancel {
                state,
                wake: Some(self.wake.clone()),
            },
        })
    }

    pub(crate) fn write_terminal_response(&self, response: impl FnOnce() -> Option<Bytes>) {
        let _order = crate::vt::lock_auxiliary(&self.response_order);
        let Some(bytes) = response() else {
            return;
        };
        if !bytes.is_empty() {
            crate::vt::lock_auxiliary(&self.controls)
                .terminal_responses
                .push(bytes);
            self.wake_actor();
        }
    }

    pub(crate) fn resize(
        &self,
        geometry: crate::geometry::PaneGeometry,
        terminal_responses: Vec<Bytes>,
    ) {
        {
            let mut controls = crate::vt::lock_auxiliary(&self.controls);
            controls.resize = Some(PtyResizeRequest {
                resize: PtyResize { geometry },
                terminal_responses,
            });
        }
        self.wake_actor();
    }

    pub(crate) fn shutdown(&self) {
        {
            let mut user_writes = crate::vt::lock_auxiliary(&self.user_writes);
            user_writes.accepting = false;
        }
        if self.control_tx.send(PtyIoControlCommand::Shutdown).is_ok() {
            self.wake_actor();
        }
    }

    fn wake_actor(&self) {
        if let Err(err) = self.wake.wake() {
            debug!(err = %err, "failed to wake PTY actor");
        }
    }
}

pub(crate) struct PtyIoActor;

impl PtyIoActor {
    pub(crate) fn spawn(config: PtyIoActorConfig) -> std::io::Result<PtyIoActorHandle> {
        Self::spawn_inner(config, None)
    }

    fn spawn_inner(
        config: PtyIoActorConfig,
        poll_observer: Option<std_mpsc::Sender<()>>,
    ) -> std::io::Result<PtyIoActorHandle> {
        fd::set_cloexec(config.master_fd.as_raw_fd())?;
        fd::set_nonblocking(config.master_fd.as_raw_fd())?;

        let (data_tx, data_rx) = mpsc::channel(ACTOR_COMMAND_BUFFER);
        let (control_tx, control_rx) = std_mpsc::channel();
        let wake_pipe = fd::create_wake_pipe()?;
        let user_writes = Arc::new(Mutex::new(UserWriteGate { accepting: true }));
        let controls = Arc::new(Mutex::new(SharedPtyControls::default()));
        let response_order = Arc::new(Mutex::new(()));
        let handle = PtyIoActorHandle {
            data_tx,
            control_tx,
            wake: wake_pipe.writer,
            user_writes,
            controls: Arc::clone(&controls),
            response_order: Arc::clone(&response_order),
        };

        let mut runner = PtyIoActorRunner {
            pane_id: config.pane_id,
            file: std::fs::File::from(config.master_fd),
            data_rx,
            control_rx,
            pending_writes: VecDeque::new(),
            current_write_offset: 0,
            active_submission: None,
            wake_read_fd: wake_pipe.read_fd,
            controls,
            response_order,
            on_read: config.on_read,
            on_reader_exit: config.on_reader_exit,
            core_broken: config.core_broken,
            read_callback_panicked: false,
            poll_observer,
        };
        std::thread::Builder::new()
            .name(format!("shepr-pty-{}", config.pane_id.raw()))
            .spawn(move || runner.run())
            .map_err(|err| std::io::Error::other(err.to_string()))?;

        Ok(handle)
    }

    #[cfg(test)]
    fn spawn_with_poll_observer(
        config: PtyIoActorConfig,
        poll_observer: std_mpsc::Sender<()>,
    ) -> std::io::Result<PtyIoActorHandle> {
        Self::spawn_inner(config, Some(poll_observer))
    }
}

struct PtyIoActorRunner {
    pane_id: PaneId,
    file: std::fs::File,
    data_rx: mpsc::Receiver<PtyIoDataCommand>,
    control_rx: std_mpsc::Receiver<PtyIoControlCommand>,
    pending_writes: VecDeque<PendingWrite>,
    current_write_offset: usize,
    active_submission: Option<ActiveSubmission>,
    wake_read_fd: OwnedFd,
    controls: Arc<Mutex<SharedPtyControls>>,
    response_order: Arc<Mutex<()>>,
    on_read: ReadCallback,
    on_reader_exit: Option<ReaderExitCallback>,
    core_broken: Option<CoreBrokenCheck>,
    read_callback_panicked: bool,
    poll_observer: Option<std_mpsc::Sender<()>>,
}

struct ActiveSubmission {
    enter: Bytes,
    reply: std_mpsc::Sender<std::io::Result<()>>,
    state: SharedSubmissionState,
}

#[derive(Debug, PartialEq, Eq)]
enum PendingWrite {
    User(Bytes),
    Submission { bytes: Bytes, part: SubmissionPart },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadOutcome {
    Data,
    WouldBlock,
    Interrupted,
    /// EOF or a hard read error: the child side is gone.
    Closed,
}

impl PtyIoActorRunner {
    fn enqueue_write(&mut self, bytes: Bytes) {
        if !bytes.is_empty() {
            self.pending_writes.push_back(PendingWrite::User(bytes));
        }
    }

    fn enqueue_submission_write(&mut self, bytes: Bytes, part: SubmissionPart) {
        if !bytes.is_empty() {
            self.pending_writes
                .push_back(PendingWrite::Submission { bytes, part });
        }
    }

    fn run(&mut self) {
        let mut should_exit = false;
        while !should_exit {
            should_exit = self.drain_commands();
            if should_exit {
                break;
            }
            if self.core_broken.as_ref().is_some_and(|broken| broken()) {
                error!(
                    pane = self.pane_id.raw(),
                    "terminal core is broken by a panic elsewhere; closing the pane"
                );
                self.read_callback_panicked = true;
                break;
            }

            self.apply_pending_controls();
            self.withdraw_cancelled_submission();

            if !self.pending_writes.is_empty() {
                match self.flush_pending_writes_once() {
                    Ok(Some(part)) => self.complete_submission_part(part),
                    Ok(None) => {}
                    Err(err) => {
                        self.handle_write_failure(err);
                        break;
                    }
                }
            }
            self.schedule_submission_enter();

            if let Some(poll_observer) = &self.poll_observer {
                let _ = poll_observer.send(());
            }

            match fd::poll_pty_and_wake(
                self.file.as_raw_fd(),
                self.wake_read_fd.as_raw_fd(),
                !self.pending_writes.is_empty(),
                self.poll_timeout_ms(),
            ) {
                Ok(readiness) => {
                    if readiness.wake_ready {
                        if let Err(err) = fd::drain_wake_fd(self.wake_read_fd.as_raw_fd()) {
                            debug!(pane = self.pane_id.raw(), err = %err, "PTY actor wake drain failed");
                            break;
                        }
                        continue;
                    }
                    if readiness.pty_read_ready && !self.read_once() {
                        break;
                    }
                    if readiness.pty_write_ready && !self.pending_writes.is_empty() {
                        match self.flush_pending_writes_once() {
                            Ok(Some(part)) => self.complete_submission_part(part),
                            Ok(None) => {}
                            Err(err) => {
                                self.handle_write_failure(err);
                                break;
                            }
                        }
                    }
                }
                Err(err) => {
                    debug!(pane = self.pane_id.raw(), err = %err, "PTY actor poll failed");
                    break;
                }
            }
        }

        self.close_input_queue();
        if let Some(on_reader_exit) = self.on_reader_exit.take() {
            on_reader_exit(if self.read_callback_panicked {
                ReaderExit::Panicked
            } else {
                ReaderExit::Closed
            });
        }
        debug!(pane = self.pane_id.raw(), "PTY actor exiting");
    }

    fn drain_commands(&mut self) -> bool {
        if self.drain_control_commands() {
            return true;
        }
        if self.active_submission.is_some() {
            return false;
        }
        self.drain_data_commands()
    }

    fn drain_control_commands(&mut self) -> bool {
        let mut should_exit = false;
        loop {
            match self.control_rx.try_recv() {
                Ok(command) => {
                    if self.handle_control_command(&command) {
                        should_exit = true;
                        break;
                    }
                }
                Err(std_mpsc::TryRecvError::Empty) => break,
                Err(std_mpsc::TryRecvError::Disconnected) => {
                    should_exit = true;
                    break;
                }
            }
        }
        should_exit
    }

    fn drain_data_commands(&mut self) -> bool {
        let mut should_exit = false;
        loop {
            match self.data_rx.try_recv() {
                Ok(command) => {
                    self.handle_data_command(command);
                    if self.active_submission.is_some() {
                        break;
                    }
                }
                Err(DataTryRecvError::Empty) => break,
                Err(DataTryRecvError::Disconnected) => {
                    should_exit = true;
                    break;
                }
            }
        }
        should_exit
    }

    fn handle_data_command(&mut self, command: PtyIoDataCommand) {
        match command {
            PtyIoDataCommand::WriteUserInput(bytes) => self.enqueue_write(bytes),
            PtyIoDataCommand::SubmitUserInput {
                text,
                enter,
                delay,
                reply,
                state,
            } => {
                let started = lock_state(&state).start(text.is_empty(), delay);
                if !started {
                    lock_state(&state).finish();
                    let _ = reply.send(Err(submission_withdrawn_error()));
                    return;
                }
                if !text.is_empty() {
                    self.enqueue_submission_write(text, SubmissionPart::Text);
                }
                self.active_submission = Some(ActiveSubmission {
                    enter,
                    reply,
                    state,
                });
            }
        }
    }

    fn handle_control_command(&mut self, command: &PtyIoControlCommand) -> bool {
        match command {
            PtyIoControlCommand::Shutdown => true,
        }
    }

    fn apply_pending_controls(&mut self) {
        let (resize, terminal_responses) = {
            let mut controls = crate::vt::lock_auxiliary(&self.controls);
            (
                controls.resize.take(),
                std::mem::take(&mut controls.terminal_responses),
            )
        };
        if let Some(request) = resize {
            self.resize(request.resize);
            self.enqueue_terminal_responses(request.terminal_responses);
        }
        self.enqueue_terminal_responses(terminal_responses);
    }

    fn read_once(&mut self) -> bool {
        self.read_chunk() != ReadOutcome::Closed
    }

    /// A write failure usually means the child has gone (the master reports EIO
    /// once the slave side is closed), but whatever it printed before exiting is
    /// still buffered on the master. Read that out before the loop ends so the
    /// child's last output reaches the terminal. Bounded so a peer that keeps
    /// producing output cannot hold the actor here.
    fn handle_write_failure(&mut self, err: std::io::Error) {
        self.fail_active_submission(err);
        const MAX_DRAIN_CHUNKS: usize = 1024;
        for _ in 0..MAX_DRAIN_CHUNKS {
            match self.read_chunk() {
                ReadOutcome::Data | ReadOutcome::Interrupted => {}
                ReadOutcome::WouldBlock | ReadOutcome::Closed => break,
            }
        }
        // Replies generated while draining have nowhere to go.
        self.pending_writes.clear();
        self.current_write_offset = 0;
    }

    fn read_chunk(&mut self) -> ReadOutcome {
        let mut buf = [0u8; 8192];
        match self.file.read(&mut buf) {
            Ok(0) => ReadOutcome::Closed,
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => ReadOutcome::WouldBlock,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => ReadOutcome::Interrupted,
            Err(err) => {
                debug!(pane = self.pane_id.raw(), err = %err, "PTY actor read failed");
                ReadOutcome::Closed
            }
            Ok(n) => {
                let response_order = Arc::clone(&self.response_order);
                let _order = crate::vt::lock_auxiliary(&response_order);
                // A panic in the terminal core must not unwind out of the
                // actor thread: that would skip the reader-exit report and
                // leave the pane dead with nobody told. Catching it costs
                // nothing on the non-panicking path.
                let on_read = &mut self.on_read;
                let bytes = &buf[..n];
                let result =
                    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| on_read(bytes)))
                    {
                        Ok(result) => result,
                        Err(payload) => {
                            error!(
                                pane = self.pane_id.raw(),
                                panic = panic_payload_message(&*payload),
                                "PTY read callback panicked; closing the pane"
                            );
                            self.read_callback_panicked = true;
                            return ReadOutcome::Closed;
                        }
                    };
                if result.core_broken {
                    error!(
                        pane = self.pane_id.raw(),
                        "terminal core is broken by an earlier panic; closing the pane"
                    );
                    self.read_callback_panicked = true;
                    return ReadOutcome::Closed;
                }
                crate::vt::lock_auxiliary(&self.controls)
                    .terminal_responses
                    .extend(result.terminal_responses);
                drop(_order);
                let terminal_responses = std::mem::take(
                    &mut crate::vt::lock_auxiliary(&self.controls).terminal_responses,
                );
                self.enqueue_terminal_responses(terminal_responses);
                ReadOutcome::Data
            }
        }
    }

    fn enqueue_terminal_responses(&mut self, terminal_responses: Vec<Bytes>) {
        for bytes in terminal_responses {
            self.enqueue_write(bytes);
        }
    }

    /// Called when a submission part finished writing, or was skipped because
    /// the submission was cancelled before it started.
    fn complete_submission_part(&mut self, part: SubmissionPart) {
        match part {
            SubmissionPart::Text => {
                let Some(submission) = self.active_submission.as_ref() else {
                    return;
                };
                let completed = lock_state(&submission.state).text_finished(Instant::now());
                if !completed {
                    self.finish_active_submission(Err(std::io::Error::other(
                        "PTY actor completed text outside the submission state machine",
                    )));
                } else if self.active_submission_cancelled() {
                    self.finish_active_submission(Err(submission_withdrawn_error()));
                }
            }
            SubmissionPart::Enter => {
                let Some(submission) = self.active_submission.as_ref() else {
                    return;
                };
                let finished = lock_state(&submission.state).enter_finished();
                if finished {
                    self.finish_active_submission(Ok(()));
                } else if self.active_submission_cancelled() {
                    self.finish_active_submission(Err(submission_withdrawn_error()));
                } else {
                    self.finish_active_submission(Err(std::io::Error::other(
                        "PTY actor completed Enter outside the submission state machine",
                    )));
                }
            }
        }
    }

    fn active_submission_cancelled(&self) -> bool {
        self.active_submission
            .as_ref()
            .is_some_and(|submission| lock_state(&submission.state).cancelled())
    }

    fn finish_active_submission(&mut self, result: std::io::Result<()>) {
        if let Some(submission) = self.active_submission.take() {
            lock_state(&submission.state).finish();
            let _ = submission.reply.send(result);
        }
    }

    /// Drop a cancelled submission as soon as the actor sees it, rather than
    /// when its delay runs out or the PTY next becomes writable, so the input
    /// queued behind it is not held up. A text write already under way is
    /// left to finish (see `SubmissionCancel`); its completion ends the
    /// submission.
    fn withdraw_cancelled_submission(&mut self) {
        let Some(submission) = self.active_submission.as_ref() else {
            return;
        };
        let should_withdraw = lock_state(&submission.state).should_withdraw();
        if !should_withdraw {
            return;
        }
        self.pending_writes
            .retain(|write| matches!(write, PendingWrite::User(_)));
        self.finish_active_submission(Err(submission_withdrawn_error()));
    }

    fn schedule_submission_enter(&mut self) {
        let Some(submission) = self.active_submission.as_ref() else {
            return;
        };
        let enter = submission.enter.clone();
        let state = Arc::clone(&submission.state);
        let start = lock_state(&state).start_enter(Instant::now(), enter.is_empty());
        match start {
            EnterStart::Cancelled => {
                self.finish_active_submission(Err(submission_withdrawn_error()));
            }
            EnterStart::Empty => {
                self.finish_active_submission(Ok(()));
            }
            EnterStart::Started => self.enqueue_submission_write(enter, SubmissionPart::Enter),
            EnterStart::NotReady => {}
        }
    }

    fn poll_timeout_ms(&self) -> i32 {
        let Some(submission) = self.active_submission.as_ref() else {
            return ACTOR_IDLE_POLL_MS;
        };
        let Some(deadline) = lock_state(&submission.state).deadline() else {
            return ACTOR_IDLE_POLL_MS;
        };
        i32::try_from(
            deadline
                .saturating_duration_since(Instant::now())
                .as_millis()
                .max(1)
                .min(ACTOR_IDLE_POLL_MS as u128),
        )
        .unwrap_or(ACTOR_IDLE_POLL_MS)
    }

    fn fail_active_submission(&mut self, err: std::io::Error) {
        self.finish_active_submission(Err(err));
    }

    fn close_input_queue(&mut self) {
        self.data_rx.close();
        self.fail_active_submission(input_submission_closed_error());
        while let Some(command) = self.data_rx.blocking_recv() {
            if let PtyIoDataCommand::SubmitUserInput { reply, state, .. } = command {
                lock_state(&state).finish();
                let _ = reply.send(Err(input_submission_closed_error()));
            }
        }
    }

    fn flush_pending_writes_once(&mut self) -> std::io::Result<Option<SubmissionPart>> {
        while let Some(write) = self.pending_writes.front() {
            let (bytes, part) = match write {
                PendingWrite::User(bytes) => (bytes, None),
                PendingWrite::Submission { bytes, part } => (bytes, Some(*part)),
            };
            // The first byte of a submission part is written under the state
            // lock, so cancellation either skips the whole part or observes
            // that writing has started. Later text chunks can finish without
            // the lock because cancellation never truncates an in-flight part.
            let state = match (part, self.current_write_offset) {
                (Some(_), 0) => self
                    .active_submission
                    .as_ref()
                    .map(|submission| Arc::clone(&submission.state)),
                _ => None,
            };
            if part.is_some() && self.current_write_offset == 0 && state.is_none() {
                self.pending_writes.pop_front();
                return Ok(part);
            }
            let mut state_guard = state.as_deref().map(lock_state);
            if state_guard
                .as_ref()
                .is_some_and(|guard| part.is_some_and(|part| !guard.can_write_first_byte(part)))
            {
                drop(state_guard);
                self.pending_writes.pop_front();
                return Ok(part);
            }
            let chunk = &bytes[self.current_write_offset..];
            match self.file.write(chunk) {
                Ok(0) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "PTY actor write returned zero bytes",
                    ));
                }
                Ok(written) => {
                    if let (Some(guard), Some(part)) = (state_guard.as_mut(), part) {
                        guard.first_byte_written(part);
                    }
                    drop(state_guard);
                    self.current_write_offset += written;
                    if self.current_write_offset >= bytes.len() {
                        let Some(completed) = self.pending_writes.pop_front() else {
                            return Ok(None);
                        };
                        self.current_write_offset = 0;
                        if let PendingWrite::Submission { part, .. } = completed {
                            self.file.flush()?;
                            return Ok(Some(part));
                        }
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => return Ok(None),
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => return Ok(None),
                Err(err) => {
                    warn!(pane = self.pane_id.raw(), err = %err, "PTY actor write failed");
                    self.pending_writes.clear();
                    self.current_write_offset = 0;
                    return Err(err);
                }
            }
        }
        self.file.flush()?;
        Ok(None)
    }

    fn resize(&self, resize: PtyResize) {
        if let Err(err) = fd::resize_pty_fd(
            self.file.as_raw_fd(),
            resize.geometry.rows(),
            resize.geometry.cols(),
            resize.geometry.cell_width(),
            resize.geometry.cell_height(),
        ) {
            debug!(pane = self.pane_id.raw(), err = %err, "PTY resize failed");
        }
    }
}

fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> &str {
    if let Some(message) = payload.downcast_ref::<&'static str>() {
        message
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.as_str()
    } else {
        "non-string panic payload"
    }
}

fn input_submission_closed_error() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::BrokenPipe,
        "PTY actor closed during input submission",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        os::fd::{FromRawFd, IntoRawFd},
        os::unix::net::UnixStream,
        sync::atomic::{AtomicBool, Ordering},
    };

    fn test_wake_pair() -> (fd::WakeWriter, OwnedFd) {
        let pipe = fd::create_wake_pipe().expect("wake pipe");
        (pipe.writer, pipe.read_fd)
    }

    fn actor_with_socket_pair() -> (PtyIoActorHandle, UnixStream, std_mpsc::Receiver<Bytes>) {
        actor_with_socket_pair_and_poll_observer(None)
    }

    fn actor_with_socket_pair_and_poll_observer(
        poll_observer: Option<std_mpsc::Sender<()>>,
    ) -> (PtyIoActorHandle, UnixStream, std_mpsc::Receiver<Bytes>) {
        let (actor_socket, peer) = UnixStream::pair().expect("socket pair");
        actor_socket
            .set_nonblocking(true)
            .expect("actor socket nonblocking");
        peer.set_read_timeout(Some(Duration::from_secs(1)))
            .expect("peer timeout");
        // SAFETY: into_raw_fd transfers this socket's sole fd ownership to OwnedFd.
        let owned = unsafe { OwnedFd::from_raw_fd(actor_socket.into_raw_fd()) };
        let (read_tx, read_rx) = std_mpsc::channel();
        let config = PtyIoActorConfig {
            pane_id: PaneId::from_raw(1),
            master_fd: owned,
            on_read: Box::new(move |bytes| {
                read_tx
                    .send(Bytes::copy_from_slice(bytes))
                    .expect("read callback receiver alive");
                PtyReadResult::empty()
            }),
            on_reader_exit: None,
            core_broken: None,
        };
        let handle = if let Some(poll_observer) = poll_observer {
            PtyIoActor::spawn_with_poll_observer(config, poll_observer)
        } else {
            PtyIoActor::spawn(config)
        }
        .expect("actor spawn");
        (handle, peer, read_rx)
    }

    fn actor_runner_for_unit_test() -> (PtyIoActorRunner, UnixStream) {
        let (actor_socket, peer) = UnixStream::pair().expect("socket pair");
        actor_socket
            .set_nonblocking(true)
            .expect("actor socket nonblocking");
        // SAFETY: into_raw_fd transfers this socket's sole fd ownership to OwnedFd.
        let owned = unsafe { OwnedFd::from_raw_fd(actor_socket.into_raw_fd()) };
        let (_data_tx, data_rx) = mpsc::channel(ACTOR_COMMAND_BUFFER);
        let (_control_tx, control_rx) = std_mpsc::channel();
        let wake_pipe = fd::create_wake_pipe().expect("wake pipe");
        let runner = PtyIoActorRunner {
            pane_id: PaneId::from_raw(1),
            file: std::fs::File::from(owned),
            data_rx,
            control_rx,
            pending_writes: VecDeque::new(),
            current_write_offset: 0,
            active_submission: None,
            wake_read_fd: wake_pipe.read_fd,
            controls: Arc::new(Mutex::new(SharedPtyControls::default())),
            response_order: Arc::new(Mutex::new(())),
            on_read: Box::new(|_| PtyReadResult::empty()),
            on_reader_exit: None,
            core_broken: None,
            read_callback_panicked: false,
            poll_observer: None,
        };
        (runner, peer)
    }

    #[test]
    fn write_failure_still_delivers_the_childs_last_output() {
        let (actor_socket, mut peer) = UnixStream::pair().expect("socket pair");
        actor_socket
            .set_nonblocking(true)
            .expect("actor socket nonblocking");
        // SAFETY: into_raw_fd transfers this socket's sole fd ownership to OwnedFd.
        let owned = unsafe { OwnedFd::from_raw_fd(actor_socket.into_raw_fd()) };
        // Keep the senders alive so the loop does not exit on a closed queue
        // before it reaches the pending write.
        let (_data_tx, data_rx) = mpsc::channel(ACTOR_COMMAND_BUFFER);
        let (_control_tx, control_rx) = std_mpsc::channel();
        let wake_pipe = fd::create_wake_pipe().expect("wake pipe");
        let (read_tx, read_rx) = std_mpsc::channel();
        let mut runner = PtyIoActorRunner {
            pane_id: PaneId::from_raw(1),
            file: std::fs::File::from(owned),
            data_rx,
            control_rx,
            pending_writes: VecDeque::new(),
            current_write_offset: 0,
            active_submission: None,
            wake_read_fd: wake_pipe.read_fd,
            controls: Arc::new(Mutex::new(SharedPtyControls::default())),
            response_order: Arc::new(Mutex::new(())),
            on_read: Box::new(move |bytes| {
                let _ = read_tx.send(Bytes::copy_from_slice(bytes));
                PtyReadResult::empty()
            }),
            on_reader_exit: None,
            core_broken: None,
            read_callback_panicked: false,
            poll_observer: None,
        };
        // The child prints its last words and exits with a reply still queued.
        peer.write_all(b"last-output").expect("peer write");
        drop(peer);
        runner.enqueue_write(Bytes::from_static(b"queued-reply"));

        runner.run();

        let read = read_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("buffered output is read after the write fails");
        assert_eq!(read, Bytes::from_static(b"last-output"));
    }

    #[test]
    fn rejected_user_input_hands_its_bytes_back() {
        let (data_tx, data_rx) = mpsc::channel(1);
        let (control_tx, _control_rx) = std_mpsc::channel();
        let (wake, _wake_read_fd) = test_wake_pair();
        let handle = PtyIoActorHandle {
            data_tx,
            control_tx,
            wake,
            user_writes: Arc::new(Mutex::new(UserWriteGate { accepting: true })),
            controls: Arc::new(Mutex::new(SharedPtyControls::default())),
            response_order: Arc::new(Mutex::new(())),
        };
        handle
            .try_write_user_input(Bytes::from_static(b"fill"))
            .expect("first write fits the queue");
        match handle.try_write_user_input(Bytes::from_static(b"full")) {
            Err(mpsc::error::TrySendError::Full(bytes)) => assert_eq!(bytes, "full"),
            other => panic!("expected a full queue, got {other:?}"),
        }
        drop(data_rx);
        match handle.try_write_user_input(Bytes::from_static(b"closed")) {
            Err(mpsc::error::TrySendError::Closed(bytes)) => assert_eq!(bytes, "closed"),
            other => panic!("expected a closed queue, got {other:?}"),
        }
    }

    #[test]
    fn actor_ignores_empty_user_input_write() {
        let (mut runner, _peer) = actor_runner_for_unit_test();

        runner.handle_data_command(PtyIoDataCommand::WriteUserInput(Bytes::new()));

        assert!(runner.pending_writes.is_empty());
    }

    #[test]
    fn submission_part_does_not_wait_for_following_protocol_write() {
        let (mut runner, _peer) = actor_runner_for_unit_test();
        let state = SubmissionState::shared();
        assert!(lock_state(&state).start(false, Duration::ZERO));
        let (reply, _completion) = std_mpsc::channel();
        runner.active_submission = Some(ActiveSubmission {
            enter: Bytes::new(),
            reply,
            state,
        });
        runner.enqueue_submission_write(Bytes::from_static(b"prompt"), SubmissionPart::Text);
        runner.enqueue_write(Bytes::from_static(b"response"));

        assert_eq!(
            runner
                .flush_pending_writes_once()
                .expect("test precondition"),
            Some(SubmissionPart::Text)
        );
        assert_eq!(
            runner.pending_writes[0],
            PendingWrite::User(Bytes::from_static(b"response"))
        );
    }

    #[test]
    fn actor_writes_user_input_to_owned_fd() {
        let (handle, mut peer, _read_rx) = actor_with_socket_pair();

        handle
            .try_write_user_input(Bytes::from_static(b"hello"))
            .expect("write command accepted");

        let mut buf = [0u8; 5];
        peer.read_exact(&mut buf).expect("peer receives write");
        assert_eq!(&buf, b"hello");
        handle.shutdown();
    }

    #[test]
    fn actor_delays_enter_from_completed_prompt_write() {
        let (handle, mut peer, _read_rx) = actor_with_socket_pair();
        let text = Bytes::from(vec![b'x'; 4 * 1024 * 1024]);
        let text_len = text.len();
        let delay = Duration::from_millis(200);
        let reader = std::thread::spawn(move || {
            std::thread::sleep(delay);
            let mut received = vec![0; text_len];
            peer.read_exact(&mut received)
                .expect("peer receives prompt");
            let prompt_completed = Instant::now();
            let mut enter = [0; 1];
            peer.read_exact(&mut enter).expect("peer receives enter");
            let enter_received = Instant::now();
            let mut user = [0; 4];
            peer.read_exact(&mut user)
                .expect("peer receives queued input");
            (prompt_completed, enter_received, enter, user)
        });

        let completion = handle
            .queue_user_input_submission(text, Bytes::from_static(b"\r"), delay)
            .expect("submission queues")
            .completion;
        handle
            .try_write_user_input(Bytes::from_static(b"user"))
            .expect("ordinary input queues behind submission");
        completion
            .recv()
            .expect("actor reports submission")
            .expect("submission completes");
        let (prompt_completed, enter_received, enter, user) = reader.join().expect("reader joins");

        assert_eq!(enter, *b"\r");
        assert_eq!(user, *b"user");
        assert!(enter_received.duration_since(prompt_completed) >= delay / 2);

        let err = match handle.queue_user_input_submission(
            Bytes::from_static(b"prompt"),
            Bytes::from_static(b"\r"),
            Duration::ZERO,
        ) {
            Ok(queued) => queued
                .completion
                .recv()
                .expect("actor reports submission")
                .expect_err("closed PTY rejects submission"),
            Err(err) => err,
        };

        assert!(matches!(
            err.kind(),
            std::io::ErrorKind::BrokenPipe
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::WriteZero
        ));
    }

    /// Fill the actor side's send buffer so the actor cannot write anything
    /// until the peer reads. Returns how many filler bytes were queued.
    fn fill_send_buffer(socket: &mut UnixStream) -> usize {
        let mut prefilled = 0;
        for chunk in [8192usize, 1] {
            let fill = vec![0xAA; chunk];
            loop {
                match socket.write(&fill) {
                    Ok(written) => prefilled += written,
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(err) => panic!("failed to fill actor write buffer: {err}"),
                }
            }
        }
        prefilled
    }

    #[test]
    fn cancelled_submission_is_never_typed_and_input_behind_it_flows() {
        let (mut actor_socket, mut peer) = UnixStream::pair().expect("socket pair");
        actor_socket
            .set_nonblocking(true)
            .expect("actor socket nonblocking");
        let prefilled = fill_send_buffer(&mut actor_socket);
        // SAFETY: into_raw_fd transfers this socket's sole fd ownership to OwnedFd.
        let owned = unsafe { OwnedFd::from_raw_fd(actor_socket.into_raw_fd()) };
        let handle = PtyIoActor::spawn(PtyIoActorConfig {
            pane_id: PaneId::from_raw(1),
            master_fd: owned,
            on_read: Box::new(|_| PtyReadResult::empty()),
            on_reader_exit: None,
            core_broken: None,
        })
        .expect("actor spawn");

        let queued = handle
            .queue_user_input_submission(
                Bytes::from_static(b"prompt"),
                Bytes::from_static(b"\r"),
                Duration::ZERO,
            )
            .expect("submission queues");
        handle
            .try_write_user_input(Bytes::from_static(b"after"))
            .expect("input queues behind the submission");

        // The PTY is not writable, so not a byte of the prompt went out.
        assert_eq!(queued.cancel.cancel(), SubmissionCancelOutcome::Withdrawn);
        let err = queued
            .completion
            .recv_timeout(Duration::from_secs(1))
            .expect("actor reports the withdrawn submission")
            .expect_err("a withdrawn submission does not complete");
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);

        peer.set_read_timeout(Some(Duration::from_secs(1)))
            .expect("peer timeout");
        let mut received = vec![0; prefilled + b"after".len()];
        peer.read_exact(&mut received)
            .expect("peer receives the filler and the later input");
        assert!(received[..prefilled].iter().all(|byte| *byte == 0xAA));
        assert_eq!(&received[prefilled..], b"after");
        peer.set_read_timeout(Some(Duration::from_millis(100)))
            .expect("peer timeout");
        let mut extra = [0u8; 1];
        assert!(
            peer.read(&mut extra).is_err(),
            "the withdrawn prompt must never reach the pane"
        );
        handle.shutdown();
    }

    #[test]
    fn cancelling_during_the_enter_delay_drops_the_enter() {
        let (handle, mut peer, _read_rx) = actor_with_socket_pair();
        let queued = handle
            .queue_user_input_submission(
                Bytes::from_static(b"prompt"),
                Bytes::from_static(b"\r"),
                Duration::from_secs(5),
            )
            .expect("submission queues");
        let mut prompt = [0; 6];
        peer.read_exact(&mut prompt).expect("peer receives prompt");
        assert_eq!(&prompt, b"prompt");

        let cancelled_at = Instant::now();
        assert_eq!(
            queued.cancel.cancel(),
            SubmissionCancelOutcome::TextUnsubmitted
        );
        queued
            .completion
            .recv_timeout(Duration::from_secs(1))
            .expect("actor ends the submission without waiting out the delay")
            .expect_err("a cancelled submission does not complete");
        assert!(cancelled_at.elapsed() < Duration::from_secs(1));

        handle
            .try_write_user_input(Bytes::from_static(b"x"))
            .expect("input flows once the submission is gone");
        let mut next = [0u8; 1];
        peer.read_exact(&mut next)
            .expect("peer receives later input");
        assert_eq!(&next, b"x", "the Enter must not be sent after a cancel");
        handle.shutdown();
    }

    #[test]
    fn cancelling_a_finished_submission_leaves_its_result() {
        let (handle, mut peer, _read_rx) = actor_with_socket_pair();
        let queued = handle
            .queue_user_input_submission(
                Bytes::from_static(b"p"),
                Bytes::from_static(b"\r"),
                Duration::ZERO,
            )
            .expect("submission queues");
        let mut written = [0; 2];
        peer.read_exact(&mut written).expect("peer receives prompt");
        let result = queued
            .completion
            .recv_timeout(Duration::from_secs(1))
            .expect("actor reports submission");
        assert!(result.is_ok());
        assert_eq!(queued.cancel.cancel(), SubmissionCancelOutcome::Finished);
        handle.shutdown();
    }

    #[test]
    fn actor_completes_empty_submission_parts() {
        let (handle, mut peer, _read_rx) = actor_with_socket_pair();
        peer.set_read_timeout(Some(Duration::from_secs(1)))
            .expect("peer timeout");

        let completion = handle
            .queue_user_input_submission(Bytes::new(), Bytes::from_static(b"\r"), Duration::ZERO)
            .expect("empty prompt submission queues")
            .completion;
        let mut enter = [0; 1];
        peer.read_exact(&mut enter)
            .expect("peer receives enter for empty prompt");
        assert_eq!(enter, *b"\r");
        completion
            .recv_timeout(Duration::from_secs(1))
            .expect("actor reports empty prompt submission")
            .expect("empty prompt submission completes");

        let completion = handle
            .queue_user_input_submission(
                Bytes::from_static(b"prompt"),
                Bytes::new(),
                Duration::from_millis(40),
            )
            .expect("empty enter submission queues")
            .completion;
        let mut prompt = [0; 6];
        peer.read_exact(&mut prompt)
            .expect("peer receives prompt before empty enter");
        assert_eq!(&prompt, b"prompt");
        completion
            .recv_timeout(Duration::from_secs(1))
            .expect("actor reports empty enter submission")
            .expect("empty enter submission completes");
        handle.shutdown();
    }

    #[test]
    fn actor_reports_peer_closure_during_submission_delay() {
        let (handle, mut peer, _read_rx) = actor_with_socket_pair();
        let completion = handle
            .queue_user_input_submission(
                Bytes::from_static(b"prompt"),
                Bytes::from_static(b"\r"),
                Duration::from_secs(1),
            )
            .expect("submission queues")
            .completion;
        let mut prompt = [0; 6];
        peer.read_exact(&mut prompt).expect("peer receives prompt");
        drop(peer);

        let err = completion
            .recv_timeout(Duration::from_secs(1))
            .expect("actor reports peer closure")
            .expect_err("peer closure fails the active submission");
        assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn actor_fails_buffered_submissions_on_exit() {
        let (handle, mut peer, _read_rx) = actor_with_socket_pair();
        let active = handle
            .queue_user_input_submission(
                Bytes::from_static(b"first"),
                Bytes::from_static(b"\r"),
                Duration::from_secs(1),
            )
            .expect("first submission queues")
            .completion;
        let mut prompt = [0; 5];
        peer.read_exact(&mut prompt).expect("peer receives prompt");
        let buffered = handle
            .queue_user_input_submission(
                Bytes::from_static(b"second"),
                Bytes::from_static(b"\r"),
                Duration::ZERO,
            )
            .expect("second submission queues")
            .completion;

        drop(peer);
        let active_err = active
            .recv_timeout(Duration::from_secs(1))
            .expect("actor reports active submission")
            .expect_err("peer closure fails active submission");
        let buffered_err = buffered
            .recv_timeout(Duration::from_secs(1))
            .expect("actor reports buffered submission")
            .expect_err("peer closure fails buffered submission");

        assert_eq!(active_err.kind(), std::io::ErrorKind::BrokenPipe);
        assert_eq!(buffered_err.kind(), std::io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn actor_rejects_submission_after_io_loop_exits() {
        let (actor_socket, peer) = UnixStream::pair().expect("socket pair");
        actor_socket
            .set_nonblocking(true)
            .expect("actor socket nonblocking");
        // SAFETY: into_raw_fd transfers this socket's sole fd ownership to OwnedFd.
        let owned = unsafe { OwnedFd::from_raw_fd(actor_socket.into_raw_fd()) };
        let handle_slot = Arc::new(Mutex::new(None::<PtyIoActorHandle>));
        let (attempt_tx, attempt_rx) = std_mpsc::channel();
        let config = PtyIoActorConfig {
            pane_id: PaneId::from_raw(1),
            master_fd: owned,
            on_read: Box::new(|_| PtyReadResult::empty()),
            on_reader_exit: Some(Box::new({
                let handle_slot = Arc::clone(&handle_slot);
                move |_| {
                    let handle = crate::vt::lock_auxiliary(&handle_slot)
                        .as_ref()
                        .expect("actor handle installed")
                        .clone();
                    let attempt = handle.queue_user_input_submission(
                        Bytes::from_static(b"prompt"),
                        Bytes::from_static(b"\r"),
                        Duration::ZERO,
                    );
                    attempt_tx.send(attempt).expect("attempt receiver alive");
                }
            })),
            core_broken: None,
        };
        let handle = PtyIoActor::spawn(config).expect("actor spawn");
        *crate::vt::lock_auxiliary(&handle_slot) = Some(handle);

        drop(peer);
        let err = match attempt_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("reader exit callback attempts submission")
        {
            Ok(queued) => queued
                .completion
                .recv_timeout(Duration::from_secs(1))
                .expect("actor reports submission")
                .expect_err("closed actor rejects submission"),
            Err(err) => err,
        };

        assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
    }

    fn actor_reporting_exit(
        on_read: ReadCallback,
    ) -> (PtyIoActorHandle, UnixStream, std_mpsc::Receiver<ReaderExit>) {
        actor_reporting_exit_with_core_check(on_read, None)
    }

    fn actor_reporting_exit_with_core_check(
        on_read: ReadCallback,
        core_broken: Option<CoreBrokenCheck>,
    ) -> (PtyIoActorHandle, UnixStream, std_mpsc::Receiver<ReaderExit>) {
        let (actor_socket, peer) = UnixStream::pair().expect("socket pair");
        actor_socket
            .set_nonblocking(true)
            .expect("actor socket nonblocking");
        // SAFETY: into_raw_fd transfers this socket's sole fd ownership to OwnedFd.
        let owned = unsafe { OwnedFd::from_raw_fd(actor_socket.into_raw_fd()) };
        let (exit_tx, exit_rx) = std_mpsc::channel();
        let handle = PtyIoActor::spawn(PtyIoActorConfig {
            pane_id: PaneId::from_raw(1),
            master_fd: owned,
            on_read,
            on_reader_exit: Some(Box::new(move |exit| {
                let _ = exit_tx.send(exit);
            })),
            core_broken,
        })
        .expect("actor spawn");
        (handle, peer, exit_rx)
    }

    #[test]
    fn a_core_broken_elsewhere_ends_an_idle_pane() {
        let broken = Arc::new(AtomicBool::new(false));
        let check = Arc::clone(&broken);
        let (_handle, _peer, exit_rx) = actor_reporting_exit_with_core_check(
            Box::new(|_| PtyReadResult::empty()),
            Some(Box::new(move || check.load(Ordering::Acquire))),
        );
        assert!(
            exit_rx.recv_timeout(Duration::from_millis(100)).is_err(),
            "a healthy idle pane keeps running"
        );

        // The child prints nothing; the idle poll alone must notice.
        broken.store(true, Ordering::Release);
        assert_eq!(
            exit_rx
                .recv_timeout(Duration::from_secs(3))
                .expect("reader exit is reported without any output"),
            ReaderExit::Panicked
        );
    }

    #[test]
    fn read_callback_panic_ends_the_loop_and_reports_it() {
        let (_handle, mut peer, exit_rx) =
            actor_reporting_exit(Box::new(|_| panic!("terminal core bug")));

        peer.write_all(b"output").expect("peer write");

        assert_eq!(
            exit_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("reader exit is reported after the panic"),
            ReaderExit::Panicked
        );
        // The master side is closed, as it would be for any other exit.
        let mut buf = [0u8; 1];
        peer.set_read_timeout(Some(Duration::from_secs(1)))
            .expect("peer timeout");
        assert_eq!(peer.read(&mut buf).expect("peer read after close"), 0);
    }

    #[test]
    fn broken_core_ends_the_loop_like_a_panic() {
        let (_handle, mut peer, exit_rx) = actor_reporting_exit(Box::new(|_| PtyReadResult {
            terminal_responses: Vec::new(),
            core_broken: true,
        }));

        peer.write_all(b"output").expect("peer write");

        assert_eq!(
            exit_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("reader exit is reported for a broken core"),
            ReaderExit::Panicked
        );
    }

    #[test]
    fn peer_closure_reports_a_plain_reader_exit() {
        let (_handle, peer, exit_rx) = actor_reporting_exit(Box::new(|_| PtyReadResult::empty()));

        drop(peer);

        assert_eq!(
            exit_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("reader exit is reported after peer closure"),
            ReaderExit::Closed
        );
    }

    #[test]
    fn actor_wakes_idle_poll_for_user_input() {
        let (poll_tx, poll_rx) = std_mpsc::channel();
        let (handle, mut peer, _read_rx) = actor_with_socket_pair_and_poll_observer(Some(poll_tx));
        peer.set_read_timeout(Some(Duration::from_millis(500)))
            .expect("peer timeout");
        poll_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("actor entered idle poll");

        let start = Instant::now();
        handle
            .try_write_user_input(Bytes::from_static(b"x"))
            .expect("write command accepted");

        let mut buf = [0u8; 1];
        peer.read_exact(&mut buf)
            .expect("peer receives write without waiting for actor poll timeout");
        assert_eq!(&buf, b"x");
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "actor write should be driven by wake fd, not the idle poll timeout"
        );
        handle.shutdown();
    }

    #[test]
    fn actor_reads_output_while_input_is_backpressured() {
        let (mut actor_socket, mut peer) = UnixStream::pair().expect("socket pair");
        actor_socket
            .set_nonblocking(true)
            .expect("actor socket nonblocking");
        peer.set_read_timeout(Some(Duration::from_secs(1)))
            .expect("peer timeout");

        let fill = [0xAA; 8192];
        let mut prefilled = 0;
        loop {
            match actor_socket.write(&fill) {
                Ok(written) => prefilled += written,
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(err) => panic!("failed to fill actor write buffer: {err}"),
            }
        }
        assert!(prefilled > 0, "actor write buffer should accept some bytes");

        // SAFETY: into_raw_fd transfers this socket's sole fd ownership to OwnedFd.
        let owned = unsafe { OwnedFd::from_raw_fd(actor_socket.into_raw_fd()) };
        let (read_tx, read_rx) = std_mpsc::channel();
        let handle = PtyIoActor::spawn(PtyIoActorConfig {
            pane_id: PaneId::from_raw(1),
            master_fd: owned,
            on_read: Box::new(move |bytes| {
                read_tx
                    .send(Bytes::copy_from_slice(bytes))
                    .expect("read callback receiver alive");
                PtyReadResult::empty()
            }),
            on_reader_exit: None,
            core_broken: None,
        })
        .expect("actor spawn");

        let marker = Bytes::from_static(b"queued-input");
        let completion = handle
            .queue_user_input_submission(marker.clone(), Bytes::from_static(b"\r"), Duration::ZERO)
            .expect("submission accepted")
            .completion;

        const OUTPUT_LEN: usize = 128 * 1024;
        let mut peer_writer = peer.try_clone().expect("clone peer writer");
        let output_writer = std::thread::spawn(move || {
            peer_writer
                .write_all(&vec![0xBB; OUTPUT_LEN])
                .expect("peer writes sustained output");
        });
        let deadline = Instant::now() + Duration::from_millis(500);
        let mut output_len = 0;
        while output_len < OUTPUT_LEN {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "actor did not keep reading blocked peer output"
            );
            let output = read_rx
                .recv_timeout(remaining)
                .expect("actor keeps reading while input remains blocked");
            assert!(output.iter().all(|byte| *byte == 0xBB));
            output_len += output.len();
        }
        assert_eq!(output_len, OUTPUT_LEN);
        output_writer.join().expect("output writer joins");

        let mut received_input = vec![0; prefilled + marker.len() + 1];
        peer.read_exact(&mut received_input)
            .expect("peer receives prefill and queued input");
        assert!(received_input[..prefilled].iter().all(|byte| *byte == 0xAA));
        assert_eq!(
            &received_input[prefilled..prefilled + marker.len()],
            marker.as_ref()
        );
        assert_eq!(received_input.last(), Some(&b'\r'));
        completion
            .recv_timeout(Duration::from_secs(1))
            .expect("actor reports submission")
            .expect("submission completes");
        handle.shutdown();
    }

    #[test]
    fn actor_delivers_fd_reads_to_callback() {
        let (handle, mut peer, read_rx) = actor_with_socket_pair();

        peer.write_all(b"from-peer").expect("peer write");

        let read = read_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("actor read callback");
        assert_eq!(read, Bytes::from_static(b"from-peer"));
        handle.shutdown();
    }

    #[test]
    fn resize_keeps_latest_request_when_command_queue_is_full() {
        let (data_tx, _data_rx) = mpsc::channel(1);
        let (control_tx, _control_rx) = std_mpsc::channel();
        data_tx
            .try_send(PtyIoDataCommand::WriteUserInput(Bytes::from_static(
                b"fill",
            )))
            .expect("fill command queue");
        let controls = Arc::new(Mutex::new(SharedPtyControls::default()));
        let (wake, _wake_read_fd) = test_wake_pair();
        let handle = PtyIoActorHandle {
            data_tx,
            control_tx,
            wake,
            user_writes: Arc::new(Mutex::new(UserWriteGate { accepting: true })),
            controls: Arc::clone(&controls),
            response_order: Arc::new(Mutex::new(())),
        };

        handle.resize(
            crate::geometry::PaneGeometry::new(80, 20, 8, 16),
            vec![Bytes::from_static(b"old")],
        );
        handle.resize(
            crate::geometry::PaneGeometry::new(120, 40, 9, 18),
            vec![Bytes::from_static(b"new")],
        );
        handle.write_terminal_response(|| Some(Bytes::from_static(b"response")));

        let controls = crate::vt::lock_auxiliary(&controls);
        assert_eq!(
            controls.resize,
            Some(PtyResizeRequest {
                resize: PtyResize {
                    geometry: crate::geometry::PaneGeometry::new(120, 40, 9, 18),
                },
                terminal_responses: vec![Bytes::from_static(b"new")],
            })
        );
        assert_eq!(
            controls.terminal_responses,
            vec![Bytes::from_static(b"response")]
        );
    }

    #[test]
    fn appearance_transition_report_precedes_query_of_new_scheme() {
        let (actor_socket, mut peer) = UnixStream::pair().expect("socket pair");
        actor_socket
            .set_nonblocking(true)
            .expect("actor socket nonblocking");
        // SAFETY: into_raw_fd transfers this socket's sole fd ownership to OwnedFd.
        let owned = unsafe { OwnedFd::from_raw_fd(actor_socket.into_raw_fd()) };
        let (data_tx, data_rx) = mpsc::channel(ACTOR_COMMAND_BUFFER);
        let (control_tx, control_rx) = std_mpsc::channel();
        let wake_pipe = fd::create_wake_pipe().expect("wake pipe");
        let controls = Arc::new(Mutex::new(SharedPtyControls::default()));
        let response_order = Arc::new(Mutex::new(()));
        let light = Arc::new(AtomicBool::new(false));
        let query_light = Arc::clone(&light);
        let runner = PtyIoActorRunner {
            pane_id: PaneId::from_raw(1),
            file: std::fs::File::from(owned),
            data_rx,
            control_rx,
            pending_writes: VecDeque::new(),
            current_write_offset: 0,
            active_submission: None,
            wake_read_fd: wake_pipe.read_fd,
            controls: Arc::clone(&controls),
            response_order: Arc::clone(&response_order),
            on_read: Box::new(move |_| PtyReadResult {
                terminal_responses: vec![if query_light.load(Ordering::Acquire) {
                    Bytes::from_static(b"query-light")
                } else {
                    Bytes::from_static(b"query-dark")
                }],
                core_broken: false,
            }),
            on_reader_exit: None,
            core_broken: None,
            read_callback_panicked: false,
            poll_observer: None,
        };
        let handle = PtyIoActorHandle {
            data_tx,
            control_tx,
            wake: wake_pipe.writer,
            user_writes: Arc::new(Mutex::new(UserWriteGate { accepting: true })),
            controls,
            response_order,
        };
        let (changed_tx, changed_rx) = std_mpsc::channel();
        let (continue_tx, continue_rx) = std_mpsc::channel();

        let appearance = std::thread::spawn(move || {
            handle.write_terminal_response(|| {
                light.store(true, Ordering::Release);
                changed_tx.send(()).expect("notify appearance change");
                continue_rx.recv().expect("continue appearance report");
                Some(Bytes::from_static(b"live-light"))
            });
        });
        changed_rx.recv().expect("appearance changed");
        peer.write_all(b"query").expect("write query");
        let reader = std::thread::spawn(move || {
            let mut runner = runner;
            assert!(runner.read_once());
            runner
        });
        continue_tx.send(()).expect("release appearance report");
        appearance.join().expect("appearance thread joins");
        let runner = reader.join().expect("reader thread joins");

        assert_eq!(
            runner.pending_writes,
            VecDeque::from([
                PendingWrite::User(Bytes::from_static(b"live-light")),
                PendingWrite::User(Bytes::from_static(b"query-light")),
            ])
        );
    }

    #[test]
    fn resize_writes_terminal_responses_after_applying_resize() {
        let (handle, mut peer, _read_rx) = actor_with_socket_pair();
        let response = Bytes::from_static(b"\x1B[48;40;100;720;900t");

        handle.resize(
            crate::geometry::PaneGeometry::new(100, 40, 9, 18),
            vec![response.clone()],
        );

        let mut buf = vec![0; response.len()];
        peer.read_exact(&mut buf)
            .expect("peer receives resize response");
        assert_eq!(Bytes::from(buf), response);
        handle.shutdown();
    }
}
